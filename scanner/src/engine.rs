//! Loadout tracking on top of the row reader: confirmation across scans, slot ownership,
//! and the key sequences that call stratagems in.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::capture::{GameWindow, Grabber, Source, game_in_focus, locate};
use crate::data::Data;
use crate::img::Img;
use crate::input;
pub use crate::matcher::Badge;
use crate::matcher::{Matcher, Row, card_box, column_box};
pub use crate::status::Status;

const UNSURE_FACTOR: u32 = 3; // a lookalike-close reading must top its slot this many times longer
const MENU_TAP_MS: u64 = 60; // shorter taps of the menu key have been missed by the game
// Reading the menu the plugin opens: the screen is read every MENU_POLL until the menu has settled (see
// menu_settled), a menu that has not opened is looked at once more MENU_LATE later, and closing it is checked every
// MENU_POLL for MENU_CLOSE_CHECK before the menu key is pressed again.
const MENU_POLL: Duration = Duration::from_millis(25);
const MENU_LATE: Duration = Duration::from_millis(250);
const MENU_CLOSE_CHECK: Duration = Duration::from_millis(400);
const MENU_OPEN_ROWS: usize = 4; // the open menu lists Reinforce, SoS Beacon and Resupply plus the loadout

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub auto_scan: bool,
    pub auto_menu_scan: bool, // open the stratagem menu once when a mission starts, to read the loadout
    pub scan_period: f64,
    pub confirm_scans: u32,
    pub icon_scale: f64,
    pub resolution: String, // "auto" (the game window's size) or "WIDTHxHEIGHT", for when the window can't be measured
    pub process_name: String,
    pub menu_key: String,
    pub menu_mode: String,
    pub up: String,
    pub down: String,
    pub left: String,
    pub right: String,
    pub throw_key: String,
    pub auto_throw: bool, // a slot key throws its stratagem after the code (a Stratagem Setup key always does)
    pub menu_open_delay_ms: u64,
    pub scan_open_wait_ms: u64,
    pub key_hold_ms: u64,
    pub key_delay_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            auto_scan: true,
            auto_menu_scan: true,
            scan_period: 0.15,
            confirm_scans: 2,
            icon_scale: 1.0,
            resolution: "auto".into(),
            process_name: "helldivers2.exe".into(),
            menu_key: "lctrl".into(),
            menu_mode: "hold".into(),
            up: "w".into(),
            down: "s".into(),
            left: "a".into(),
            right: "d".into(),
            throw_key: String::new(),
            auto_throw: false,
            menu_open_delay_ms: 120,
            scan_open_wait_ms: 350,
            key_hold_ms: 35,
            key_delay_ms: 40,
        }
    }
}

impl Config {
    /// Parse leniently: anything missing or invalid falls back to its default.
    pub fn from_json(v: &serde_json::Value) -> Config {
        let d = Config::default();
        let mut c: Config = serde_json::from_value(v.clone()).unwrap_or_default();
        c.scan_period = if c.scan_period.is_finite() { c.scan_period.clamp(0.1, 60.0) } else { d.scan_period };
        c.confirm_scans = c.confirm_scans.clamp(1, 10);
        c.icon_scale = if c.icon_scale.is_finite() { c.icon_scale.clamp(0.5, 2.0) } else { 1.0 };
        for (field, fallback) in
            [(&mut c.menu_key, &d.menu_key), (&mut c.up, &d.up), (&mut c.down, &d.down), (&mut c.left, &d.left), (&mut c.right, &d.right)]
        {
            if !input::valid(field) {
                *field = fallback.clone();
            }
        }
        if !c.throw_key.is_empty() && !input::valid(&c.throw_key) {
            c.throw_key.clear();
        }
        // before 0.5.2, setting a throw key was what made the keys throw: a configuration of then keeps doing so
        if v.get("auto_throw").and_then(serde_json::Value::as_bool).is_none() {
            c.auto_throw = !c.throw_key.is_empty();
        }
        if c.menu_mode != "toggle" {
            c.menu_mode = "hold".into();
        }
        c.resolution = c.resolution.trim().to_lowercase().replace(' ', "");
        if c.game_size().is_none() {
            c.resolution = "auto".into();
        }
        c.process_name = c.process_name.to_lowercase();
        c
    }

    /// The game's size set by hand, if any: "2560x1440", within 640x360 and 7680x4320.
    pub fn game_size(&self) -> Option<(usize, usize)> {
        let (w, h) = self.resolution.split_once('x')?;
        let (w, h): (usize, usize) = (w.parse().ok()?, h.parse().ok()?);
        ((640..=7680).contains(&w) && (360..=4320).contains(&h)).then_some((w, h))
    }
}

struct Slot {
    index: usize,
    score: f32,
    sure: bool,
    status: Status,
    next: Option<(Status, u32)>, // a status change waiting to be seen again
    fill: f32,                   // game progress fill when last seen (cooldown: from the bottom; active: from the top)
    fill_at: Instant,
    rate: Option<f32>,        // measured cooldown fill drain per second
    cooldown_s: f32,          // wiki cooldown, the drain rate until one is measured
    assumed: Option<Instant>, // shown active after a call-in, not yet confirmed on screen
    badge: Option<Badge>,
    challenger: Option<(usize, u32)>, // a different stratagem read here, and for how many scans in a row
    cooldown_from: Option<Instant>,   // when a cooldown was seen starting full, to learn its real length
    seen_at: Instant,                 // last time its row was on screen
    ends: Option<(Instant, Instant)>, // the cooldown's end as bounded by the timers read on screen
    prefilled: bool,                  // read from the loadout screen, not yet seen in a mission (see Engine::prefill)
    code_read: Option<(String, u32)>, // the code last read on the open menu, and how many times in a row
    code: Option<String>,             // the code the key types, once a read was confirmed (see note_code)
}

const TAKEOVER_SCANS: u32 = 4; // sure readings of another stratagem in a row before it takes a slot
const CODE_CONFIRM: u32 = 3; // readings of the same code in a row before the key types it
const MIN_CODE: usize = 3; // arrows of the shortest code (Orbital Precision Strike): fewer is a read cut short

/// The code a key types for a stratagem whose arrows read `read` on the open menu, `codes` being the catalog's (its
/// main code first); None: the catalog's main code. The reader may stop an arrow early or catch a stray one past the
/// code's end, so a read fitting one catalog code (either starts with the other) gets that code: which of SSSD
/// Delivery's two codes the mission gives, say. A read fitting none is a code the catalog doesn't know, taken as read,
/// unless it reached the capture's edge (CODE_READ arrows), where it may go on, or is too short to be a code.
pub fn code_from_read(read: &str, codes: &[&str]) -> Option<String> {
    if codes.contains(&read) {
        return Some(read.to_string());
    }
    let fitting: Vec<&str> = codes.iter().copied().filter(|c| c.starts_with(read) || read.starts_with(c)).collect();
    match fitting[..] {
        [one] => Some(one.to_string()),
        [] if (MIN_CODE..crate::status::CODE_READ).contains(&read.len()) => Some(read.to_string()),
        _ => None, // fits several (cut short before they part), or can't be trusted
    }
}

const ASSUMED_FOR: f32 = 6.0; // seconds an unconfirmed call-in keeps the key active
const UNSEEN_GRACE: f32 = 20.0; // seconds past the estimated end before an unseen cooldown is taken as over
// The closed HUD drops a row once its call-in has landed and only brings it back when ready again,
// so an in-flight row gone this long (HUD still up) means its cooldown has started.
const ACTIVE_GONE: Duration = Duration::from_millis(1500);
// The cooldown timer shows whole seconds, and whether it rounds the time left down or up is not known:
// "T-00:09" means 9 to 9 + TIMER_ROUNDING seconds left. The key counts down to the latest, never early.
const TIMER_ROUNDING: f32 = 1.0;
// A narrower bound on the end moving it less than this is not taken: every change of a view is a repaint.
const TIMER_STEP: Duration = Duration::from_millis(100);

/// What a key shows for one menu slot.
#[derive(Clone, Debug, PartialEq)]
pub struct SlotView {
    pub index: usize,
    pub status: Status,
    /// Progress fill to draw, rounded to 1/72 so repaints only happen when a key pixel row changes.
    pub fill: f32,
    /// The game's count badge, last time it was seen.
    pub badge: Option<Badge>,
    /// While cooling down: when it is expected to be ready.
    pub ready: Option<ReadyAt>,
    /// The code the key types, as the open menu showed it (see Slot::note_code); None: the catalog's.
    pub code: Option<String>,
}

/// When a cooling slot is expected to be ready: as a timer read on screen says (`timed`, exact to the
/// second), or where the fill's drain reaches zero (an estimate). A moment rather than the time left,
/// so a view kept by the plugin stays right, and only changes when the estimate does.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReadyAt {
    pub at: Instant,
    pub timed: bool,
}

impl Slot {
    fn new(r: &Row, cooldown_s: f32) -> Slot {
        let mut s = Slot {
            index: r.index,
            score: r.score,
            sure: r.sure,
            status: r.status,
            next: None,
            fill: 0.0,
            fill_at: Instant::now(),
            rate: None,
            cooldown_s,
            assumed: None,
            badge: r.badge.clone(),
            challenger: None,
            cooldown_from: None,
            seen_at: Instant::now(),
            ends: None,
            prefilled: false,
            code_read: None,
            code: None,
        };
        s.observe_fill(r);
        s
    }

    /// A code read on the open menu. Read the same CODE_CONFIRM times in a row, it decides what the key types (see
    /// code_from_read), `codes` being the catalog's. Returns whether that changed.
    fn note_code(&mut self, read: &str, codes: &[&str]) -> bool {
        let n = match &self.code_read {
            Some((c, n)) if c == read => n + 1,
            _ => 1,
        };
        self.code_read = Some((read.to_string(), n));
        if n < CODE_CONFIRM {
            return false;
        }
        let code = code_from_read(read, codes).filter(|c| c != codes[0]); // the catalog's main code needs no telling
        let changed = code != self.code;
        self.code = code;
        changed
    }

    /// Record the fill just read; two cooldown readings a few seconds apart give the real drain rate
    /// (ship upgrades shorten cooldowns, so the wiki figure is only a fallback).
    fn observe_fill(&mut self, r: &Row) {
        let now = Instant::now();
        match self.status {
            Status::Cooldown if self.ends.is_some() => {} // the timer read on screen says more
            Status::Cooldown => {
                let dt = now.duration_since(self.fill_at).as_secs_f32();
                let drop = self.fill - r.fill_bottom;
                if dt >= 3.0 && drop > 0.0 && drop < 0.5 {
                    // ship upgrades shorten cooldowns, but not beyond these bounds of the wiki figure
                    let wiki = 1.0 / self.cooldown_s.max(1.0);
                    self.rate = Some((drop / dt).clamp(wiki * 0.5, wiki * 2.0));
                }
                if dt >= 3.0 || self.fill == 0.0 || r.fill_bottom > self.fill {
                    (self.fill, self.fill_at) = (r.fill_bottom, now);
                }
            }
            Status::Active => (self.fill, self.fill_at) = (r.fill_top, now),
            _ => (self.fill, self.fill_at) = (0.0, now),
        }
    }

    /// A cooldown timer read on screen, `secs` left. Each reading bounds the end (see TIMER_ROUNDING) and
    /// narrows the bounds kept so far: the shown second changing pins the end to within a scan. A reading
    /// that does not fit them starts afresh. The key counts down to the latest end, with the fill drawn
    /// reaching zero right then. Returns the cooldown's length when the reading tells it, which is only
    /// when the cooldown was seen starting. Time left / fill is no measure of it: an icon with large white
    /// parts looks the same under the game's cream fill, so its fill reads far too low (a Gunner FRV with
    /// 468 s of 480 left read as 63% covered, which made a 740 s cooldown of it); the length then stays the
    /// one learned earlier or the wiki's, and never below the time left.
    fn observe_timer(&mut self, secs: u32, now: Instant) -> Option<f32> {
        let lo = now + Duration::from_secs(secs as u64);
        let hi = lo + Duration::from_secs_f32(TIMER_ROUNDING);
        let (lo, hi) = match self.ends {
            Some((l, h)) if lo < h && l < hi => (l.max(lo), if h.duration_since(hi) >= TIMER_STEP { hi } else { h }),
            _ => (lo, hi),
        };
        self.ends = Some((lo, hi));
        let left = hi.duration_since(now).as_secs_f32();
        let length = self.cooldown_from.map(|start| (now.duration_since(start).as_secs_f32() + left).max(1.0));
        if let Some(s) = length {
            self.cooldown_s = s;
        }
        let total = self.cooldown_s.max(left).max(1.0);
        (self.fill, self.fill_at, self.rate) = (left / total, now, Some(1.0 / total));
        length
    }

    fn view(&self) -> SlotView {
        let mut v = self.view_state();
        // no uses left: the game drops the badge, and so do we
        v.badge = if v.status == Status::Unavailable { None } else { self.badge.clone() };
        v
    }

    fn view_state(&self) -> SlotView {
        let q = |f: f32| (f.clamp(0.0, 1.0) * 72.0).round() / 72.0;
        let v = |status, fill| SlotView { index: self.index, status, fill, badge: None, ready: None, code: self.code.clone() };
        match self.status {
            Status::Cooldown => {
                // the end of a cooldown is a sliver too thin to measure, so without a timer the estimate
                // only draws the fill; the key unlocks when the game shows it ready, or long after the
                // estimated end if that moment went unseen. A timer read on screen gives the end itself.
                let rate = self.rate.unwrap_or(1.0 / self.cooldown_s.max(1.0));
                let since = self.fill_at.elapsed().as_secs_f32();
                let (end, timed, grace) = match self.ends {
                    Some((_, hi)) => (hi, true, 0.0),
                    None => (self.fill_at + Duration::from_secs_f32((self.fill / rate.max(1e-4)).max(0.0)), false, UNSEEN_GRACE),
                };
                if Instant::now() > end + Duration::from_secs_f32(grace) {
                    v(Status::Ready, 0.0)
                } else {
                    SlotView { ready: Some(ReadyAt { at: end, timed }), ..v(self.status, q(self.fill - rate * since).max(1.0 / 72.0)) }
                }
            }
            // a call-in the game never showed (refused, or typed into a vehicle) stops blocking the key
            Status::Active if self.assumed.is_some_and(|t| t.elapsed().as_secs_f32() > ASSUMED_FOR) => v(Status::Ready, 0.0),
            Status::Active => v(self.status, q(self.fill)),
            Status::Unavailable => v(self.status, 1.0),
            _ => v(self.status, 0.0),
        }
    }

    /// A status sticks once read on `need` scans in a row; Unknown (row without a status line) never overrides.
    fn note(&mut self, st: Status, need: u32) -> bool {
        if st != Status::Unknown {
            self.assumed = None;
        }
        if st == Status::Unknown || st == self.status {
            self.next = None;
            return false;
        }
        let n = match self.next {
            Some((s, n)) if s == st => n + 1,
            _ => 1,
        };
        if n >= need {
            self.status = st;
            self.next = None;
            (self.fill, self.rate, self.ends) = (0.0, None, None);
            true
        } else {
            self.next = Some((st, n));
            false
        }
    }
}

pub struct Engine {
    pub matcher: Matcher,
    grabber: Grabber,
    slots: BTreeMap<usize, Slot>, // menu slot -> what it holds
    pending: HashMap<(usize, usize), u32>,
    layout: Option<(Vec<(usize, usize)>, u32)>, // the open menu last seen as (slot, stratagem), and scans in a row
    learned: HashMap<usize, f32>,               // stratagem -> cooldown seconds measured this session
    pub focused: bool,
    pub scan_ms: f64,
    pub last_error: Option<String>,       // why the last scan could not read the game
    pub menu_rows: Vec<usize>,            // rows seen by the last menu read: before the menu key, then after it
    pub menu_open_ms: Option<u64>,        // how long the last menu read kept the menu open, when it opened it
    pub source: Option<Source>,           // how the game's position was found in the last scan
    pub measured: Option<(usize, usize)>, // the game's size as found in the last scan, before any size set by hand
    last_hud: Option<Instant>,            // last time any stratagem row was on screen
    card: Option<Vec<(usize, bool)>>,     // the loadout card as last read, to be read the same once more
    card_seen: Option<Instant>,           // last time the loadout card was on screen
    chosen: Vec<usize>,                   // the stratagems the player chose, from the last card confirmed (see chosen)
}

/// What the game is doing, as far as the screen tells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GameState {
    Missing, // not running, or its window cannot be read
    Lobby,   // on screen, but no stratagem HUD: ship, menus, loading
    Mission, // the stratagem HUD is showing (Reinforce is always listed in a mission)
}

const HUD_GRACE: Duration = Duration::from_secs(3); // loading flickers and menus hide the HUD briefly
// Without the HUD this long, the mission is over: forget the loadout. Long enough to survive a map
// check or a death camera, which also hide it.
const LOBBY_RESET: Duration = Duration::from_secs(10);
// After the loadout screen, a mission is expected this long: the next stratagem HUD is that mission starting.
const MISSION_EXPECTED: Duration = Duration::from_secs(300);

static FIRING: Mutex<()> = Mutex::new(());

impl Engine {
    pub fn new(data: &Data) -> Result<Engine, String> {
        Ok(Engine {
            matcher: Matcher::load(data)?,
            grabber: Grabber::new(),
            slots: BTreeMap::new(),
            pending: HashMap::new(),
            layout: None,
            learned: HashMap::new(),
            focused: false,
            scan_ms: 0.0,
            last_error: None,
            menu_rows: Vec::new(),
            menu_open_ms: None,
            source: None,
            measured: None,
            last_hud: None,
            card: None,
            card_seen: None,
            chosen: Vec::new(),
        })
    }

    /// What each menu slot shows, up to the last known slot; None = slot not read yet.
    pub fn loadout(&self) -> Vec<Option<SlotView>> {
        let len = self.slots.keys().next_back().map_or(0, |k| k + 1);
        (0..len).map(|k| self.slots.get(&k).map(Slot::view)).collect()
    }

    /// The stratagems the player chose on the loadout screen, in the order chosen (0: the first), from the last card
    /// read the same twice; empty when none has been. They are kept once the card leaves the screen, for the whole
    /// mission (see forget). The Stratagem Setup key binds to these, so a stratagem a mission adds among them, even
    /// one the reader cannot place, does not move the loadout positions along.
    pub fn chosen(&self) -> Vec<usize> {
        self.chosen.clone()
    }

    /// Cooldown length: as last measured this session, otherwise the wiki figure.
    fn cooldown_s(&self, index: usize) -> f32 {
        if let Some(&s) = self.learned.get(&index) {
            return s;
        }
        let c = &self.matcher.catalog[index].cooldown;
        c.trim_end_matches('s').trim().parse().unwrap_or(180.0)
    }

    pub fn game_state(&self) -> GameState {
        if !self.focused {
            GameState::Missing
        } else if self.last_hud.is_some_and(|t| t.elapsed() < HUD_GRACE) {
            GameState::Mission
        } else {
            GameState::Lobby
        }
    }

    /// Right after calling a stratagem in: show it as active until the screen says otherwise.
    pub fn mark_called(&mut self, index: usize) -> bool {
        match self.slots.values_mut().find(|s| s.index == index) {
            Some(s) if s.status != Status::Active => {
                (s.status, s.next, s.fill, s.fill_at, s.ends) = (Status::Active, None, 0.0, Instant::now(), None);
                s.assumed = Some(Instant::now());
                true
            }
            _ => false,
        }
    }

    pub fn clear(&mut self) {
        self.slots.clear();
        self.pending.clear();
        self.matcher.reset();
    }

    /// The mission is over, or the game closed: clear the slots and forget what the player chose, which only the next
    /// loadout screen tells. The Clear key only clears: the loadout chosen still holds for the rest of the mission.
    pub fn forget(&mut self) {
        self.clear();
        self.chosen.clear();
    }

    /// Slots read in the mission, as opposed to those known only from the loadout screen.
    pub fn mission_slots(&self) -> usize {
        self.slots.values().filter(|s| !s.prefilled).count()
    }

    /// Read the player's card on the loadout screen and, once the same card has been read twice in a row, fill the
    /// slots with it (see `prefill`). Returns whether the loadout changed.
    pub fn read_card(&mut self, cfg: &Config) -> Result<bool, String> {
        let mut fg = locate(&cfg.process_name).ok_or("game not running")?;
        if let Some((w, h)) = cfg.game_size() {
            (fg.w, fg.h) = (w, h);
        }
        let (x0, y0, x1, y1) = card_box(fg.w, fg.h);
        let img: Img = self.grabber.grab(fg.x + x0 as i32, fg.y + y0 as i32, (x1 - x0) as i32, (y1 - y0) as i32)?;
        Ok(self.take_card(self.matcher.read_card(&img, fg.h, (x0, y0))))
    }

    /// A reading of the loadout card, None when it is not on screen. The card on screen means a mission is coming
    /// (see mission_expected); read the same twice in a row, it fills the slots. Returns whether the loadout changed.
    fn take_card(&mut self, card: Option<Vec<(usize, bool)>>) -> bool {
        if card.is_some() {
            self.card_seen = Some(Instant::now());
        }
        let again = card.is_some() && card == self.card;
        self.card = card;
        if !again {
            return false;
        }
        let card = self.card.clone().unwrap_or_default();
        self.chosen = card.iter().skip(crate::matcher::CARD_COMMON).map(|&(i, _)| i).collect();
        self.prefill(card)
    }

    /// The loadout screen was on screen within MISSION_EXPECTED: the next stratagem HUD (Reinforce, which the
    /// closed HUD always shows) is a mission starting, and the menu is worth opening as soon as the game lets it.
    pub fn mission_expected(&self) -> bool {
        self.card_seen.is_some_and(|t| t.elapsed() < MISSION_EXPECTED)
    }

    /// Before a mission, fill the slots with the stratagems of the loadout screen's card, in menu order, shown
    /// unavailable: nothing can be called in yet. The mission's first reading of a slot takes it over (see set_slot),
    /// and the open menu drops those it does not list (see scan_menu). Slots read in a mission are never touched.
    fn prefill(&mut self, card: Vec<(usize, bool)>) -> bool {
        if card.is_empty() || self.mission_slots() > 0 {
            return false;
        }
        let have: Vec<(usize, usize)> = self.slots.iter().map(|(&k, s)| (k, s.index)).collect();
        let want: Vec<(usize, usize)> = card.iter().enumerate().map(|(k, &(index, _))| (k, index)).collect();
        if have == want {
            return false;
        }
        self.slots.clear();
        self.pending.clear();
        for (k, (index, _)) in card.into_iter().enumerate() {
            let row = Row {
                index,
                score: 0.0,
                sure: false,
                slot: k,
                status: Status::Unavailable,
                fill_bottom: 0.0,
                fill_top: 0.0,
                badge: None,
                timer: None,
                code: None,
            };
            let mut s = Slot::new(&row, self.cooldown_s(index));
            s.prefilled = true;
            self.slots.insert(k, s);
        }
        true
    }

    /// The game window, only if it is the one receiving keyboard input.
    fn game_focused(cfg: &Config) -> Option<GameWindow> {
        locate(&cfg.process_name).filter(|_| game_in_focus(&cfg.process_name))
    }

    /// Reads wherever the game window is on screen, focused or not; it only has to be visible.
    fn read(&mut self, cfg: &Config) -> Result<Vec<Row>, String> {
        let mut fg =
            locate(&cfg.process_name).ok_or_else(|| format!("game not running: {}", crate::capture::diagnose(&cfg.process_name)))?;
        self.source = Some(fg.source);
        self.measured = Some((fg.w, fg.h));
        // a size set by hand wins: when the window is hidden from us, the fallbacks can only guess it
        if let Some((w, h)) = cfg.game_size() {
            (fg.w, fg.h) = (w, h);
        }
        let (x0, y0, x1, y1) = column_box(fg.w, fg.h, cfg.icon_scale);
        let col = self.grabber.grab(fg.x + x0 as i32, fg.y + y0 as i32, (x1 - x0) as i32, (y1 - y0) as i32)?;
        Ok(self.matcher.read_rows(&col, fg.h, (x0, y0), cfg.icon_scale))
    }

    /// Fill a slot. A different stratagem replaces the current one with a better reading, or when it
    /// keeps reading surely for TAKEOVER_SCANS in a row: objective stratagems change in place during a
    /// mission (SSSD Delivery becomes Upload Data). An unsure reading never replaces a sure one.
    /// Returns whether the loadout changed.
    fn set_slot(&mut self, r: &Row) -> bool {
        if let Some(cur) = self.slots.get_mut(&r.slot) {
            if cur.index == r.index {
                cur.score = cur.score.max(r.score);
                cur.sure |= r.sure;
                cur.challenger = None;
                cur.prefilled = false;
                return false;
            }
            let streak = match cur.challenger {
                Some((i, n)) if i == r.index => n + 1,
                _ => 1,
            };
            cur.challenger = Some((r.index, streak));
            let takes_over = r.sure && streak >= TAKEOVER_SCANS;
            if (cur.sure, cur.score) >= (r.sure, r.score) && !takes_over {
                return false;
            }
        }
        let cooldown_s = self.cooldown_s(r.index);
        self.slots.retain(|_, s| s.index != r.index);
        self.slots.insert(r.slot, Slot::new(r, cooldown_s));
        true
    }

    /// The rows' layout as (slot, stratagem) when they are the open menu: every slot from the first on, at least
    /// MENU_OPEN_ROWS of them, each read surely, starting with Reinforce, which every mission lists first.
    fn open_menu(&self, rows: &[Row]) -> Option<Vec<(usize, usize)>> {
        let whole = rows.len() >= MENU_OPEN_ROWS && rows.iter().enumerate().all(|(k, r)| r.slot == k && r.sure);
        let reinforce = rows.first().is_some_and(|r| self.matcher.catalog[r.index].name == "Reinforce");
        (whole && reinforce).then(|| rows.iter().map(|r| (r.slot, r.index)).collect())
    }

    /// Lay the slots out as the open menu shows them (`rows`, see open_menu): each stratagem goes to its slot with
    /// what it has been showing (its cooldown, timer and badge), one the menu has just added comes in, and one it no
    /// longer lists goes. The game moves the menu along when a mission hands out a stratagem (SEAF Artillery comes in
    /// after Resupply) or takes one back (a used SOS Beacon), which slot-by-slot readings would follow only stratagem
    /// by stratagem, mixing old and new places meanwhile. Returns whether the layout changed.
    fn relayout(&mut self, rows: &[Row]) -> bool {
        let now: Vec<(usize, usize)> = self.slots.iter().map(|(&k, s)| (k, s.index)).collect();
        let shown: Vec<(usize, usize)> = rows.iter().map(|r| (r.slot, r.index)).collect();
        if now == shown {
            return false;
        }
        let mut by_stratagem: HashMap<usize, Slot> = std::mem::take(&mut self.slots).into_values().map(|s| (s.index, s)).collect();
        for r in rows {
            let mut slot = match by_stratagem.remove(&r.index) {
                Some(s) => s,
                None => Slot::new(r, self.cooldown_s(r.index)),
            };
            (slot.prefilled, slot.challenger, slot.sure) = (false, None, true);
            slot.score = slot.score.max(r.score);
            self.slots.insert(r.slot, slot);
        }
        self.pending.clear();
        self.matcher.reset(); // its per-slot memory of where each icon sat no longer holds
        true
    }

    /// Update status and progress of slots whose row was just seen holding the stratagem we already know.
    fn note_statuses(&mut self, rows: &[Row], need: u32) -> bool {
        let mut changed = false;
        for r in rows {
            if let Some(s) = self.slots.get_mut(&r.slot).filter(|s| s.index == r.index) {
                s.seen_at = Instant::now();
                let before = s.status;
                changed |= s.note(r.status, need);
                if r.status == s.status {
                    s.observe_fill(r);
                }
                // learn the real cooldown from one seen whole: ship upgrades make it shorter than the wiki's
                match (before, s.status) {
                    // a call-in seen on its way, then cooling, has just started its cooldown; otherwise only a
                    // full fill says so (and a fill can read low, see observe_timer)
                    (b, Status::Cooldown) if b != Status::Cooldown => {
                        s.cooldown_from = (b == Status::Active || r.fill_bottom >= 0.9).then(Instant::now);
                    }
                    (Status::Cooldown, Status::Ready) => {
                        if let Some(t) = s.cooldown_from.take() {
                            let secs = t.elapsed().as_secs_f32();
                            if secs > 5.0 {
                                self.learned.insert(s.index, secs);
                                s.cooldown_s = secs;
                                s.rate = None;
                            }
                        }
                    }
                    _ => {}
                }
                if let Some(secs) = r.timer.filter(|_| (r.status, s.status) == (Status::Cooldown, Status::Cooldown))
                    && let Some(length) = s.observe_timer(secs, Instant::now())
                {
                    self.learned.insert(s.index, length);
                }
                if r.badge.is_some() && r.badge != s.badge {
                    s.badge = r.badge.clone();
                    changed = true;
                }
                if let Some(read) = &r.code {
                    let codes: Vec<&str> = self.matcher.catalog[s.index].codes().collect();
                    changed |= s.note_code(read, &codes);
                }
            }
        }
        changed
    }

    /// One auto-scan step. Every reading must repeat on consecutive scans before it counts:
    /// rows sliding in or out pass through neighbouring slot positions for a frame or two.
    pub fn scan(&mut self, cfg: &Config) -> bool {
        let t = Instant::now();
        let rows = match self.read(cfg) {
            Ok(rows) => rows,
            Err(e) => {
                self.focused = false;
                self.last_error = Some(e);
                return false;
            }
        };
        self.update(&rows, cfg, t)
    }

    /// The scan step once the rows are read (`t`: when the scan started); apart so tests can feed rows.
    fn update(&mut self, rows: &[Row], cfg: &Config, t: Instant) -> bool {
        self.focused = true;
        self.last_error = None;
        if !rows.is_empty() {
            self.last_hud = Some(Instant::now());
        } else if self.last_hud.is_some_and(|t| t.elapsed() > LOBBY_RESET) {
            // the mission is over: forget it, once, so that a loadout read on the loadout screen afterwards stays
            self.last_hud = None;
            if !self.slots.is_empty() {
                self.forget();
                return true;
            }
        }
        self.scan_ms = t.elapsed().as_secs_f64() * 1000.0;
        // the open menu, read the same on confirm_scans scans in a row, lays the slots out as it shows them
        let mut changed = false;
        match self.open_menu(rows) {
            Some(layout) => {
                let n = match &self.layout {
                    Some((seen, n)) if *seen == layout => n + 1,
                    _ => 1,
                };
                self.layout = Some((layout, n));
                if n >= cfg.confirm_scans {
                    changed |= self.relayout(rows);
                }
            }
            None => self.layout = None,
        }
        let seen: HashMap<(usize, usize), &Row> = rows.iter().map(|r| ((r.slot, r.index), r)).collect();
        self.pending.retain(|k, _| seen.contains_key(k));
        for (key, r) in &seen {
            let n = self.pending.entry(*key).or_default();
            *n += 1;
            let need = cfg.confirm_scans * if r.sure { 1 } else { UNSURE_FACTOR };
            if *n >= need {
                changed |= self.set_slot(r);
            }
        }
        changed |= self.note_statuses(rows, cfg.confirm_scans);
        if !rows.is_empty() {
            for s in self.slots.values_mut() {
                if s.status == Status::Active && s.assumed.is_none() && s.seen_at.elapsed() > ACTIVE_GONE {
                    (s.status, s.next, s.fill, s.fill_at, s.rate, s.ends) = (Status::Cooldown, None, 1.0, s.seen_at, None, None);
                    s.cooldown_from = Some(s.seen_at);
                    changed = true;
                }
            }
        }
        changed
    }

    /// Open the menu if it is closed, read it, and put the menu back as it was. The menu stays open only until it
    /// has settled on screen (see read_open_menu), and the plugin presses the menu key again only to close a menu
    /// it opened, or to reopen one its press closed.
    pub fn scan_menu(&mut self, cfg: &Config) -> Result<bool, String> {
        if Engine::game_focused(cfg).is_none() {
            return Err("game not in focus; the menu key would go to another window".into());
        }
        let _busy = FIRING.try_lock().map_err(|_| "busy entering a stratagem")?;
        let wait = Duration::from_millis(cfg.menu_open_delay_ms + cfg.scan_open_wait_ms);
        let before = self.read(cfg)?;
        self.menu_open_ms = None;
        let reads = if cfg.menu_mode == "hold" {
            input::down(&cfg.menu_key)?;
            let pressed = Instant::now();
            let after = self.read_open_menu(cfg, wait);
            input::up(&cfg.menu_key)?;
            self.menu_open_ms = Some(pressed.elapsed().as_millis() as u64);
            let (after, agreed) = after?;
            [Some(before), agreed, Some(after)].into_iter().flatten().collect()
        } else {
            let hold = cfg.key_hold_ms.max(MENU_TAP_MS);
            input::tap(&cfg.menu_key, hold)?;
            let pressed = Instant::now();
            let (mut after, agreed) = self.read_open_menu(cfg, wait)?;
            let was_open = before.len() >= MENU_OPEN_ROWS;
            if !was_open && after.len() < MENU_OPEN_ROWS {
                // not open (yet): the game may still be ignoring the key, or be slow to show the menu
                sleep(MENU_LATE);
                after = self.read(cfg)?;
            }
            if was_open && after.len() < before.len() {
                // it was already open and the tap closed it: reopen, keep the earlier read
                sleep(Duration::from_millis(100));
                input::tap(&cfg.menu_key, hold)?;
                vec![before]
            } else if !was_open && after.len() >= MENU_OPEN_ROWS {
                // opened by the tap: close it, and check, as the game has been seen to ignore a closing tap
                input::tap(&cfg.menu_key, hold)?;
                self.menu_open_ms = Some(pressed.elapsed().as_millis() as u64);
                for _ in 0..2 {
                    if self.menu_closes(cfg)? {
                        break;
                    }
                    input::tap(&cfg.menu_key, hold)?;
                }
                [Some(before), agreed, Some(after)].into_iter().flatten().collect()
            } else {
                // the game ignored the tap: nothing to put back
                vec![before, after]
            }
        };
        self.menu_rows = reads.iter().map(Vec::len).collect();
        Ok(self.menu_read(&reads))
    }

    /// Right after the menu key: read the screen every MENU_POLL until the menu has settled (menu_settled), or for
    /// `max` at most; the last reading, and the one before it when the two agree (another reading of the codes).
    fn read_open_menu(&mut self, cfg: &Config, max: Duration) -> Result<(Vec<Row>, Option<Vec<Row>>), String> {
        let start = Instant::now();
        let mut last: Option<Vec<Row>> = None;
        loop {
            sleep(MENU_POLL);
            let rows = self.read(cfg)?;
            if menu_settled(last.as_deref(), &rows) {
                return Ok((rows, last));
            }
            if start.elapsed() >= max {
                return Ok((rows, None));
            }
            last = Some(rows);
        }
    }

    /// Whether the menu closes within MENU_CLOSE_CHECK after the closing tap.
    fn menu_closes(&mut self, cfg: &Config) -> Result<bool, String> {
        let start = Instant::now();
        while start.elapsed() < MENU_CLOSE_CHECK {
            sleep(MENU_POLL);
            if self.read(cfg)?.len() < MENU_OPEN_ROWS {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Take in the readings around opening the menu: every sure row fills its slot at once and statuses count from
    /// one reading. The open menu lists every slot, so a slot still only known from the loadout screen is not part
    /// of this mission and goes. Returns whether the loadout changed.
    fn menu_read(&mut self, reads: &[Vec<Row>]) -> bool {
        let mut changed = false;
        if let Some(open) = reads.iter().rev().find(|rows| self.open_menu(rows).is_some()) {
            changed |= self.relayout(open);
        }
        for r in reads.iter().flatten().filter(|r| r.sure) {
            changed |= self.set_slot(r);
        }
        for rows in reads {
            changed |= self.note_statuses(rows, 1);
        }
        if reads.iter().any(|rows| rows.len() >= MENU_OPEN_ROWS) && self.slots.values().any(|s| s.prefilled) {
            self.slots.retain(|_, s| !s.prefilled);
            changed = true;
        }
        changed
    }
}

/// Whether the open menu has settled: at least MENU_OPEN_ROWS rows, each with its status line read, the same
/// stratagems at the same places with the same statuses as the reading before. The rows slide in when the menu opens,
/// and one reading of them may still catch a row on its way.
fn menu_settled(before: Option<&[Row]>, rows: &[Row]) -> bool {
    let key = |rows: &[Row]| rows.iter().map(|r| (r.slot, r.index, r.status)).collect::<Vec<_>>();
    rows.len() >= MENU_OPEN_ROWS && rows.iter().all(|r| r.status != Status::Unknown) && before.is_some_and(|b| key(b) == key(rows))
}

/// The bound key for each arrow of a code ("UDRL..."); any other character is skipped.
fn code_keys<'a>(code: &'a str, cfg: &'a Config) -> impl Iterator<Item = &'a str> {
    code.chars().filter_map(move |c| match c {
        'U' => Some(cfg.up.as_str()),
        'D' => Some(cfg.down.as_str()),
        'L' => Some(cfg.left.as_str()),
        'R' => Some(cfg.right.as_str()),
        _ => None,
    })
}

/// Enter a stratagem's code ("UDRL..."), then throw it when `throw` is set and a throw key is bound. Does not need
/// the engine, so scanning carries on meanwhile.
pub fn fire(code: &str, cfg: &Config, throw: bool) -> Result<(), String> {
    if Engine::game_focused(cfg).is_none() {
        let (pid, name) = crate::capture::foreground_owner();
        let game = locate(&cfg.process_name).map_or("not found".to_string(), |g| format!("pid {} via {:?}", g.pid, g.source));
        return Err(format!("game not in focus: foreground is {name} (pid {pid}), game {game}"));
    }
    let _busy = FIRING.try_lock().map_err(|_| "busy entering a stratagem")?;
    let gap = Duration::from_millis(cfg.key_delay_ms);
    if cfg.menu_mode == "hold" {
        input::down(&cfg.menu_key)?;
    } else {
        input::tap(&cfg.menu_key, cfg.key_hold_ms)?;
    }
    sleep(Duration::from_millis(cfg.menu_open_delay_ms));
    let mut result = Ok(());
    for key in code_keys(code, cfg) {
        if let Err(e) = input::tap(key, cfg.key_hold_ms) {
            result = Err(e);
            break;
        }
        sleep(gap);
    }
    if cfg.menu_mode == "hold" {
        input::up(&cfg.menu_key)?;
    }
    result?;
    if throw && !cfg.throw_key.is_empty() {
        sleep(gap * 2);
        input::tap(&cfg.throw_key, cfg.key_hold_ms)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Slot logic driven through `update` with hand-made rows. Waits are simulated by moving the
    //! slot's timestamps into the past, so nothing here sleeps.

    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::img::Img;

    fn data() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../data")
    }

    fn engine() -> Engine {
        Engine::new(&Data::Dir(data())).expect("engine loads the catalog")
    }

    fn idx(e: &Engine, name: &str) -> usize {
        e.matcher.catalog.iter().position(|s| s.name == name).unwrap_or_else(|| panic!("{name} not in the catalog"))
    }

    fn row(index: usize, slot: usize, status: Status) -> Row {
        Row { index, score: 0.95, sure: true, slot, status, fill_bottom: 0.0, fill_top: 0.0, badge: None, timer: None, code: None }
    }

    fn badge(hash: u64) -> Badge {
        Badge { hash, w: 1, h: 1, bits: Arc::new(vec![true]), number: None }
    }

    /// An instant `secs` seconds ago.
    fn ago(secs: f32) -> Instant {
        Instant::now().checked_sub(Duration::from_secs_f32(secs)).expect("system uptime shorter than the simulated wait")
    }

    fn feed(e: &mut Engine, rows: &[Row]) -> bool {
        e.update(rows, &Config::default(), Instant::now())
    }

    /// Feed the same rows `n` times; whether the last scan changed the loadout.
    fn feed_n(e: &mut Engine, rows: &[Row], n: usize) -> bool {
        (0..n).map(|_| feed(e, rows)).last().unwrap_or(false)
    }

    fn view(e: &Engine, slot: usize) -> SlotView {
        e.loadout().get(slot).cloned().flatten().unwrap_or_else(|| panic!("slot {slot} empty"))
    }

    fn names(e: &Engine) -> Vec<String> {
        e.loadout().iter().map(|v| v.as_ref().map_or("-".to_string(), |v| e.matcher.catalog[v.index].name.clone())).collect()
    }

    fn cfg_debug(v: serde_json::Value) -> String {
        format!("{:?}", Config::from_json(&v))
    }

    // ---- Config::from_json ----

    #[test]
    fn throwing_after_the_code_is_a_setting_of_its_own() {
        assert!(!Config::default().auto_throw);
        let c = |v: serde_json::Value| Config::from_json(&v);
        assert!(!c(json!({ "throw_key": "mouse1", "auto_throw": false })).auto_throw);
        assert!(c(json!({ "throw_key": "mouse1", "auto_throw": true })).auto_throw);
        // a configuration from before the setting: a throw key set meant throwing
        assert!(c(json!({ "throw_key": "mouse1" })).auto_throw);
        assert!(!c(json!({ "throw_key": "" })).auto_throw && !c(json!({})).auto_throw);
    }

    #[test]
    fn config_defaults() {
        let d = Config::default();
        assert!(d.auto_scan && d.auto_menu_scan);
        assert_eq!((d.scan_period, d.confirm_scans, d.icon_scale), (0.15, 2, 1.0));
        assert_eq!(d.process_name, "helldivers2.exe");
        assert_eq!((d.menu_key.as_str(), d.menu_mode.as_str()), ("lctrl", "hold"));
        assert_eq!([d.up.as_str(), d.down.as_str(), d.left.as_str(), d.right.as_str()], ["w", "s", "a", "d"]);
        assert_eq!(d.throw_key, "");
        assert_eq!((d.menu_open_delay_ms, d.scan_open_wait_ms, d.key_hold_ms, d.key_delay_ms), (120, 350, 35, 40));
        for key in [&d.menu_key, &d.up, &d.down, &d.left, &d.right] {
            assert!(input::valid(key), "default key {key} is not a valid key name");
        }
    }

    #[test]
    fn config_empty_or_not_an_object_is_default() {
        let d = format!("{:?}", Config::default());
        for v in [json!({}), json!(null), json!([1, 2]), json!("lctrl"), json!(3)] {
            assert_eq!(cfg_debug(v.clone()), d, "{v}");
        }
    }

    #[test]
    fn config_reads_every_field() {
        let c = Config::from_json(&json!({
            "auto_scan": false,
            "auto_menu_scan": false,
            "scan_period": 0.5,
            "confirm_scans": 3,
            "icon_scale": 1.25,
            "resolution": "1920x1080",
            "process_name": "HellDivers2.EXE",
            "menu_key": "mouse4",
            "menu_mode": "toggle",
            "up": "up",
            "down": "down",
            "left": "left",
            "right": "right",
            "throw_key": "mouse1",
            "menu_open_delay_ms": 200,
            "scan_open_wait_ms": 500,
            "key_hold_ms": 50,
            "key_delay_ms": 60
        }));
        assert!(!c.auto_scan && !c.auto_menu_scan);
        assert_eq!((c.scan_period, c.confirm_scans, c.icon_scale), (0.5, 3, 1.25));
        assert_eq!(c.game_size(), Some((1920, 1080)));
        assert_eq!(c.process_name, "helldivers2.exe", "process name is lowercased");
        assert_eq!((c.menu_key.as_str(), c.menu_mode.as_str(), c.throw_key.as_str()), ("mouse4", "toggle", "mouse1"));
        assert_eq!([c.up.as_str(), c.down.as_str(), c.left.as_str(), c.right.as_str()], ["up", "down", "left", "right"]);
        assert_eq!((c.menu_open_delay_ms, c.scan_open_wait_ms, c.key_hold_ms, c.key_delay_ms), (200, 500, 50, 60));
    }

    #[test]
    fn config_missing_fields_keep_their_default() {
        let c = Config::from_json(&json!({ "menu_key": "f1", "unknown_field": 7 }));
        let d = Config::default();
        assert_eq!(c.menu_key, "f1");
        assert_eq!(format!("{:?}", Config { menu_key: d.menu_key.clone(), ..c }), format!("{d:?}"));
    }

    #[test]
    fn config_clamps_numbers() {
        let c = |v| Config::from_json(&v);
        assert_eq!(c(json!({ "scan_period": 0.01 })).scan_period, 0.1);
        assert_eq!(c(json!({ "scan_period": -3.0 })).scan_period, 0.1);
        assert_eq!(c(json!({ "scan_period": 600.0 })).scan_period, 60.0);
        assert_eq!(c(json!({ "confirm_scans": 0 })).confirm_scans, 1);
        assert_eq!(c(json!({ "confirm_scans": 50 })).confirm_scans, 10);
        assert_eq!(c(json!({ "icon_scale": 0.1 })).icon_scale, 0.5);
        assert_eq!(c(json!({ "icon_scale": 9.0 })).icon_scale, 2.0);
    }

    #[test]
    fn config_bad_key_names_fall_back_one_by_one() {
        let c = Config::from_json(&json!({
            "menu_key": "hyper",
            "up": "W",
            "down": "",
            "left": "mouse9",
            "right": "ctrl+d",
            "throw_key": "nope",
            "key_hold_ms": 77
        }));
        assert_eq!([c.menu_key.as_str(), c.up.as_str(), c.down.as_str(), c.left.as_str(), c.right.as_str()], ["lctrl", "w", "s", "a", "d"]);
        assert_eq!(c.throw_key, "", "an invalid throw key is dropped");
        assert_eq!(c.key_hold_ms, 77, "valid fields next to invalid key names are kept");
    }

    #[test]
    fn config_resolution_is_auto_or_a_sane_size() {
        let c = |v: &str| Config::from_json(&json!({ "resolution": v }));
        assert_eq!(Config::default().resolution, "auto");
        assert_eq!(Config::default().game_size(), None);
        for (given, want, size) in [
            ("2560x1440", "2560x1440", Some((2560, 1440))),
            (" 1920 X 1080 ", "1920x1080", Some((1920, 1080))),
            ("3840x2160", "3840x2160", Some((3840, 2160))),
            ("auto", "auto", None),
            ("", "auto", None),
            ("1080p", "auto", None),
            ("100x100", "auto", None),
            ("99999x1440", "auto", None),
            ("2560x", "auto", None),
            ("x1440", "auto", None),
            ("-2560x1440", "auto", None),
        ] {
            let cfg = c(given);
            assert_eq!((cfg.resolution.as_str(), cfg.game_size()), (want, size), "{given:?}");
        }
    }

    #[test]
    fn config_menu_mode_is_hold_unless_toggle() {
        for (given, want) in [("toggle", "toggle"), ("hold", "hold"), ("press", "hold"), ("Toggle", "hold"), ("", "hold")] {
            assert_eq!(Config::from_json(&json!({ "menu_mode": given })).menu_mode, want, "{given}");
        }
    }

    /// A field of the wrong JSON type fails the whole parse, so every field, valid ones included,
    /// falls back to its default (current behaviour of `unwrap_or_default` on the whole struct).
    #[test]
    fn config_a_wrongly_typed_field_resets_every_field() {
        let d = format!("{:?}", Config::default());
        assert_eq!(cfg_debug(json!({ "confirm_scans": "3", "menu_key": "f1" })), d);
        assert_eq!(cfg_debug(json!({ "key_hold_ms": -5, "menu_key": "f1" })), d);
        assert_eq!(cfg_debug(json!({ "auto_scan": "yes", "menu_key": "f1" })), d);
        assert_eq!(cfg_debug(json!({ "scan_period": null, "menu_key": "f1" })), d);
    }

    // ---- code_keys ----

    #[test]
    fn code_keys_follow_the_bindings() {
        let mut cfg = Config::default();
        assert_eq!(code_keys("UDLR", &cfg).collect::<Vec<_>>(), ["w", "s", "a", "d"]);
        assert_eq!(code_keys("RRU", &cfg).collect::<Vec<_>>(), ["d", "d", "w"]);
        assert_eq!(code_keys("", &cfg).count(), 0);
        assert_eq!(code_keys("U x-D\n", &cfg).collect::<Vec<_>>(), ["w", "s"], "other characters are skipped");
        assert_eq!(code_keys("udlr", &cfg).count(), 0, "lowercase letters are not arrows");
        (cfg.up, cfg.down, cfg.left, cfg.right) = ("up".into(), "down".into(), "left".into(), "right".into());
        assert_eq!(code_keys("LDRDRDU", &cfg).collect::<Vec<_>>(), ["left", "down", "right", "down", "right", "down", "up"]);
    }

    #[test]
    fn every_catalog_code_maps_to_one_key_per_arrow() {
        let e = engine();
        let cfg = Config::default();
        for s in &e.matcher.catalog {
            let keys: Vec<&str> = code_keys(&s.code, &cfg).collect();
            assert_eq!(keys.len(), s.code.len(), "{}: {}", s.name, s.code);
            assert!(keys.iter().all(|k| input::valid(k)));
        }
    }

    // ---- cooldown lengths ----

    #[test]
    fn cooldown_from_the_catalog_with_fallback() {
        let mut e = engine();
        for (name, secs) in
            [("Orbital Laser", 300.0), ("Eagle Airstrike", 15.0), ("Resupply", 180.0), ("Reinforce", 0.0), ("Upload Data", 180.0)]
        {
            assert_eq!(e.cooldown_s(idx(&e, name)), secs, "{name}");
        }
        for i in 0..e.matcher.catalog.len() {
            let s = e.cooldown_s(i);
            assert!(s.is_finite() && s >= 0.0, "{}: {s}", e.matcher.catalog[i].name);
        }
        let laser = idx(&e, "Orbital Laser");
        e.learned.insert(laser, 42.0);
        assert_eq!(e.cooldown_s(laser), 42.0, "a measured cooldown wins over the wiki figure");
    }

    // ---- confirmation across scans ----

    #[test]
    fn a_sure_row_needs_confirm_scans_in_a_row() {
        let mut e = engine();
        let r = [row(idx(&e, "Reinforce"), 0, Status::Ready)];
        assert!(!feed(&mut e, &r));
        assert!(e.loadout().is_empty(), "one scan is not enough with confirm_scans = 2");
        assert!(feed(&mut e, &r));
        assert_eq!(names(&e), ["Reinforce"]);
        assert!(!feed(&mut e, &r), "seeing the same loadout again changes nothing");
    }

    #[test]
    fn confirm_scans_of_one_takes_the_first_reading() {
        let mut e = engine();
        let cfg = Config { confirm_scans: 1, ..Config::default() };
        assert!(e.update(&[row(idx(&e, "Reinforce"), 0, Status::Ready)], &cfg, Instant::now()));
        assert_eq!(names(&e), ["Reinforce"]);
    }

    #[test]
    fn an_unsure_row_needs_three_times_as_many_scans() {
        let mut e = engine();
        let r = [Row { sure: false, ..row(idx(&e, "Reinforce"), 0, Status::Ready) }];
        feed_n(&mut e, &r, 5);
        assert!(e.loadout().is_empty());
        assert!(feed(&mut e, &r));
        assert_eq!(names(&e), ["Reinforce"]);
    }

    #[test]
    fn a_missed_scan_restarts_the_count() {
        let mut e = engine();
        let r = [row(idx(&e, "Reinforce"), 0, Status::Ready)];
        feed(&mut e, &r);
        feed(&mut e, &[]);
        feed(&mut e, &r);
        assert!(e.loadout().is_empty(), "the row was not seen on consecutive scans");
        feed(&mut e, &r);
        assert_eq!(names(&e), ["Reinforce"]);
    }

    #[test]
    fn a_status_change_needs_confirming_and_unknown_never_overrides() {
        let mut e = engine();
        let laser = idx(&e, "Orbital Laser");
        feed_n(&mut e, &[row(laser, 3, Status::Ready)], 2);
        let cooling = [Row { fill_bottom: 0.5, ..row(laser, 3, Status::Cooldown) }];
        assert!(!feed(&mut e, &cooling));
        assert_eq!(view(&e, 3).status, Status::Ready);
        assert!(feed(&mut e, &cooling));
        assert_eq!(view(&e, 3).status, Status::Cooldown);
        for _ in 0..5 {
            assert!(!feed(&mut e, &[row(laser, 3, Status::Unknown)]));
        }
        assert_eq!(view(&e, 3).status, Status::Cooldown);
        // a change seen once, then the old status again, does not stick
        feed(&mut e, &[row(laser, 3, Status::Unavailable)]);
        feed(&mut e, &cooling);
        feed(&mut e, &[row(laser, 3, Status::Unavailable)]);
        assert_eq!(view(&e, 3).status, Status::Cooldown);
    }

    // ---- slot ownership ----

    #[test]
    fn loadout_lists_slots_up_to_the_last_known_one() {
        let mut e = engine();
        assert!(e.loadout().is_empty());
        let (reinforce, laser) = (idx(&e, "Reinforce"), idx(&e, "Orbital Laser"));
        feed_n(&mut e, &[row(reinforce, 0, Status::Ready), row(laser, 3, Status::Ready)], 2);
        let l = e.loadout();
        assert_eq!(l.len(), 4);
        assert_eq!(l.iter().map(Option::is_some).collect::<Vec<_>>(), [true, false, false, true]);
        assert_eq!((view(&e, 0).index, view(&e, 3).index), (reinforce, laser));
        assert_eq!((view(&e, 0).fill, view(&e, 3).fill), (0.0, 0.0));
    }

    #[test]
    fn a_stratagem_seen_in_another_slot_moves_there() {
        let mut e = engine();
        let laser = idx(&e, "Orbital Laser");
        feed_n(&mut e, &[row(laser, 3, Status::Ready)], 2);
        feed_n(&mut e, &[row(laser, 4, Status::Ready)], 2);
        assert_eq!(names(&e), ["-", "-", "-", "-", "Orbital Laser"]);
    }

    #[test]
    fn another_sure_stratagem_takes_a_slot_after_four_scans() {
        let mut e = engine();
        let (sssd, upload) = (idx(&e, "SSSD Delivery"), idx(&e, "Upload Data"));
        feed_n(&mut e, &[row(sssd, 5, Status::Ready)], 2);
        let challenger = [Row { score: 0.90, ..row(upload, 5, Status::Ready) }];
        // pending reaches confirm_scans on the 2nd scan; the challenger streak then needs 4 sure readings
        for scan in 1..=4 {
            assert!(!feed(&mut e, &challenger), "scan {scan}");
            assert_eq!(view(&e, 5).index, sssd, "scan {scan}");
        }
        assert!(feed(&mut e, &challenger));
        assert_eq!(view(&e, 5).index, upload);
    }

    #[test]
    fn a_better_reading_replaces_an_unsure_one_at_once() {
        let mut e = engine();
        let (sssd, upload) = (idx(&e, "SSSD Delivery"), idx(&e, "Upload Data"));
        feed_n(&mut e, &[Row { sure: false, score: 0.7, ..row(sssd, 5, Status::Ready) }], 6);
        assert_eq!(view(&e, 5).index, sssd);
        feed_n(&mut e, &[row(upload, 5, Status::Ready)], 2);
        assert_eq!(view(&e, 5).index, upload);
    }

    #[test]
    fn an_unsure_reading_never_replaces_a_sure_one() {
        let mut e = engine();
        let (sssd, upload) = (idx(&e, "SSSD Delivery"), idx(&e, "Upload Data"));
        feed_n(&mut e, &[Row { score: 0.8, ..row(sssd, 5, Status::Ready) }], 2);
        feed_n(&mut e, &[Row { sure: false, score: 0.99, ..row(upload, 5, Status::Ready) }], 20);
        assert_eq!(view(&e, 5).index, sssd);
    }

    // ---- badges ----

    #[test]
    fn badges_follow_the_screen_and_unavailable_hides_them() {
        let mut e = engine();
        let laser = idx(&e, "Orbital Laser");
        feed_n(&mut e, &[Row { badge: Some(badge(2)), ..row(laser, 3, Status::Ready) }], 2);
        assert_eq!(view(&e, 3).badge.map(|b| b.hash), Some(2));
        assert!(feed(&mut e, &[Row { badge: Some(badge(1)), ..row(laser, 3, Status::Ready) }]), "a new count is a change");
        assert_eq!(view(&e, 3).badge.map(|b| b.hash), Some(1));
        feed(&mut e, &[row(laser, 3, Status::Ready)]);
        assert_eq!(view(&e, 3).badge.map(|b| b.hash), Some(1), "a row read without its badge keeps the last one");
        feed_n(&mut e, &[row(laser, 3, Status::Unavailable)], 2);
        let v = view(&e, 3);
        assert_eq!((v.status, v.fill, v.badge), (Status::Unavailable, 1.0, None), "no uses left: no badge, full cover");
    }

    // ---- call-ins ----

    #[test]
    fn mark_called_shows_the_slot_active() {
        let mut e = engine();
        let (reinforce, laser) = (idx(&e, "Reinforce"), idx(&e, "Orbital Laser"));
        feed_n(&mut e, &[row(reinforce, 0, Status::Ready), row(laser, 3, Status::Ready)], 2);
        assert!(e.mark_called(laser));
        assert_eq!((view(&e, 3).status, view(&e, 3).fill), (Status::Active, 0.0));
        assert_eq!(view(&e, 0).status, Status::Ready, "other slots are untouched");
        assert!(!e.mark_called(laser), "already active");
        assert!(!e.mark_called(idx(&e, "Resupply")), "not in the loadout");
    }

    #[test]
    fn a_call_in_the_game_never_shows_expires() {
        let mut e = engine();
        let laser = idx(&e, "Orbital Laser");
        feed_n(&mut e, &[row(laser, 3, Status::Ready)], 2);
        e.mark_called(laser);
        e.slots.get_mut(&3).unwrap().assumed = Some(ago(ASSUMED_FOR - 1.0));
        assert_eq!(view(&e, 3).status, Status::Active);
        e.slots.get_mut(&3).unwrap().assumed = Some(ago(ASSUMED_FOR + 1.0));
        assert_eq!((view(&e, 3).status, view(&e, 3).fill), (Status::Ready, 0.0));
    }

    #[test]
    fn an_assumed_call_in_is_not_taken_for_a_finished_one() {
        let mut e = engine();
        let (reinforce, laser) = (idx(&e, "Reinforce"), idx(&e, "Orbital Laser"));
        feed_n(&mut e, &[row(reinforce, 0, Status::Ready), row(laser, 3, Status::Ready)], 2);
        e.mark_called(laser);
        e.slots.get_mut(&3).unwrap().seen_at = ago(5.0);
        feed(&mut e, &[row(reinforce, 0, Status::Ready)]);
        assert_eq!(view(&e, 3).status, Status::Active, "still waiting for the game to show the call-in");
        // a row without a status line does not confirm it either
        feed(&mut e, &[row(laser, 3, Status::Unknown)]);
        assert!(e.slots[&3].assumed.is_some());
        // the game showing any status does
        feed(&mut e, &[row(laser, 3, Status::Active)]);
        assert!(e.slots[&3].assumed.is_none());
    }

    #[test]
    fn an_active_row_gone_from_the_hud_starts_its_cooldown() {
        let mut e = engine();
        let (reinforce, laser) = (idx(&e, "Reinforce"), idx(&e, "Orbital Laser"));
        let inbound = Row { fill_top: 0.6, ..row(laser, 3, Status::Active) };
        feed_n(&mut e, &[row(reinforce, 0, Status::Ready), inbound], 2);
        assert_eq!(view(&e, 3).status, Status::Active);
        // gone for less than ACTIVE_GONE: still in flight
        e.slots.get_mut(&3).unwrap().seen_at = ago(ACTIVE_GONE.as_secs_f32() - 1.0);
        assert!(!feed(&mut e, &[row(reinforce, 0, Status::Ready)]));
        assert_eq!(view(&e, 3).status, Status::Active);
        // gone longer, while the HUD still shows other rows: landed, cooling down from full
        let seen = ago(ACTIVE_GONE.as_secs_f32() + 0.5);
        e.slots.get_mut(&3).unwrap().seen_at = seen;
        assert!(feed(&mut e, &[row(reinforce, 0, Status::Ready)]));
        let v = view(&e, 3);
        assert_eq!(v.status, Status::Cooldown);
        assert!(v.fill > 0.95, "cooldown starts full, fill {}", v.fill);
        let s = &e.slots[&3];
        assert_eq!((s.cooldown_from, s.fill_at, s.rate), (Some(seen), seen, None));
    }

    #[test]
    fn an_active_row_is_not_inferred_finished_without_the_hud() {
        let mut e = engine();
        let laser = idx(&e, "Orbital Laser");
        feed_n(&mut e, &[row(laser, 3, Status::Active)], 2);
        e.slots.get_mut(&3).unwrap().seen_at = ago(ACTIVE_GONE.as_secs_f32() + 1.0);
        assert!(!feed(&mut e, &[]), "no rows at all: map, death camera or loading, not a landing");
        assert_eq!(view(&e, 3).status, Status::Active);
    }

    // ---- cooldown progress ----

    #[test]
    fn a_cooldown_drains_with_time_and_floors_at_one_step() {
        let mut e = engine();
        let laser = idx(&e, "Orbital Laser"); // 300 s on the wiki
        feed_n(&mut e, &[Row { fill_bottom: 0.5, ..row(laser, 3, Status::Cooldown) }], 2);
        assert!((view(&e, 3).fill - 0.5).abs() <= 1.0 / 72.0);
        e.slots.get_mut(&3).unwrap().fill_at = ago(30.0);
        let f = view(&e, 3).fill;
        assert!((f - 0.4).abs() <= 1.0 / 72.0, "0.5 - 30 s / 300 s, got {f}");
        assert_eq!((f * 72.0).fract(), 0.0, "fill is quantised to 1/72");
        // past the estimated end (5 s at 10 s for the whole), within the grace: the thinnest sliver
        e.slots.get_mut(&3).unwrap().cooldown_s = 10.0;
        e.slots.get_mut(&3).unwrap().fill_at = ago(10.0);
        assert_eq!((view(&e, 3).status, view(&e, 3).fill), (Status::Cooldown, 1.0 / 72.0));
    }

    #[test]
    fn an_unseen_cooldown_ends_after_the_grace_period() {
        let mut e = engine();
        let laser = idx(&e, "Orbital Laser");
        feed_n(&mut e, &[Row { fill_bottom: 0.5, ..row(laser, 3, Status::Cooldown) }], 2);
        e.slots.get_mut(&3).unwrap().cooldown_s = 10.0; // ends 5 s after the reading, then UNSEEN_GRACE
        e.slots.get_mut(&3).unwrap().fill_at = ago(5.0 + UNSEEN_GRACE - 1.0);
        assert_eq!(view(&e, 3).status, Status::Cooldown);
        e.slots.get_mut(&3).unwrap().fill_at = ago(5.0 + UNSEEN_GRACE + 1.0);
        assert_eq!((view(&e, 3).status, view(&e, 3).fill), (Status::Ready, 0.0));
    }

    #[test]
    fn the_drain_rate_is_measured_and_bounded_by_the_wiki_figure() {
        let mut e = engine();
        let laser = idx(&e, "Orbital Laser");
        let at = |fill| [Row { fill_bottom: fill, ..row(laser, 3, Status::Cooldown) }];
        feed_n(&mut e, &at(0.6), 2);
        assert_eq!(e.slots[&3].rate, None);
        // readings less than 3 s apart are not used
        e.slots.get_mut(&3).unwrap().fill_at = ago(1.0);
        feed(&mut e, &at(0.59));
        assert_eq!(e.slots[&3].rate, None);
        // 0.02 in 4 s: 1/200 per second, inside [wiki / 2, wiki * 2] for 300 s
        e.slots.get_mut(&3).unwrap().fill_at = ago(4.0);
        feed(&mut e, &at(0.58));
        let rate = e.slots[&3].rate.expect("rate measured");
        assert!((rate - 0.005).abs() < 1e-4, "{rate}");
        assert!((e.slots[&3].fill - 0.58).abs() < 1e-6, "the newer reading becomes the reference");
        // a drop far too fast for the wiki figure is clamped to twice the wiki rate
        e.slots.get_mut(&3).unwrap().fill_at = ago(4.0);
        feed(&mut e, &at(0.18));
        let rate = e.slots[&3].rate.unwrap();
        assert!((rate - 2.0 / 300.0).abs() < 1e-6, "{rate}");
    }

    #[test]
    fn a_cooldown_seen_whole_teaches_its_real_length() {
        let mut e = engine();
        let laser = idx(&e, "Orbital Laser");
        feed_n(&mut e, &[row(laser, 3, Status::Ready)], 2);
        feed_n(&mut e, &[Row { fill_bottom: 0.95, ..row(laser, 3, Status::Cooldown) }], 2);
        assert!(e.slots[&3].cooldown_from.is_some(), "a cooldown seen starting full is timed");
        e.slots.get_mut(&3).unwrap().cooldown_from = Some(ago(30.0));
        assert!(feed_n(&mut e, &[row(laser, 3, Status::Ready)], 2));
        let learned = e.cooldown_s(laser);
        assert!((learned - 30.0).abs() < 1.0, "{learned}");
        assert!((e.slots[&3].cooldown_s - learned).abs() < 1e-6);
    }

    #[test]
    fn a_cooldown_first_seen_half_way_is_not_timed() {
        let mut e = engine();
        let laser = idx(&e, "Orbital Laser");
        feed_n(&mut e, &[row(laser, 3, Status::Ready)], 2);
        feed_n(&mut e, &[Row { fill_bottom: 0.5, ..row(laser, 3, Status::Cooldown) }], 2);
        assert!(e.slots[&3].cooldown_from.is_none());
        feed_n(&mut e, &[row(laser, 3, Status::Ready)], 2);
        assert_eq!(e.cooldown_s(laser), 300.0);
    }

    // ---- HUD presence ----

    #[test]
    fn game_state_follows_focus_and_the_hud() {
        let mut e = engine();
        assert_eq!(e.game_state(), GameState::Missing);
        let reinforce = idx(&e, "Reinforce");
        feed(&mut e, &[row(reinforce, 0, Status::Ready)]);
        assert!(e.focused && e.last_error.is_none());
        assert_eq!(e.game_state(), GameState::Mission);
        e.last_hud = Some(ago(HUD_GRACE.as_secs_f32() + 1.0));
        assert_eq!(e.game_state(), GameState::Lobby);
        e.focused = false;
        assert_eq!(e.game_state(), GameState::Missing);
    }

    #[test]
    fn a_long_absence_of_the_hud_forgets_the_loadout() {
        let mut e = engine();
        let reinforce = idx(&e, "Reinforce");
        feed_n(&mut e, &[row(reinforce, 0, Status::Ready)], 2);
        e.last_hud = Some(ago(LOBBY_RESET.as_secs_f32() - 2.0));
        assert!(!feed(&mut e, &[]));
        assert_eq!(names(&e), ["Reinforce"], "a short gap (map, death camera) keeps the loadout");
        e.last_hud = Some(ago(LOBBY_RESET.as_secs_f32() + 1.0));
        assert!(feed(&mut e, &[]));
        assert!(e.loadout().is_empty());
        assert!(!feed(&mut e, &[]), "nothing left to forget");
    }

    // ---- the loadout screen ----

    fn card(e: &Engine, names: &[&str]) -> Vec<(usize, bool)> {
        names.iter().map(|n| (idx(e, n), true)).collect()
    }

    fn menu(e: &Engine, names: &[&str]) -> Vec<Row> {
        names.iter().enumerate().map(|(k, n)| row(idx(e, n), k, Status::Ready)).collect()
    }

    #[test]
    fn the_loadout_card_fills_the_slots_unavailable_in_menu_order() {
        let mut e = engine();
        let c = card(&e, &["Reinforce", "SoS Beacon", "Resupply", "Orbital Laser"]);
        assert!(e.prefill(c.clone()));
        assert_eq!(names(&e), ["Reinforce", "SoS Beacon", "Resupply", "Orbital Laser"]);
        assert!((0..4).all(|k| view(&e, k).status == Status::Unavailable), "nothing can be called in before the mission");
        assert_eq!(e.mission_slots(), 0);
        assert!(!e.prefill(c), "the same card again changes nothing");
        assert!(e.prefill(card(&e, &["Reinforce", "SoS Beacon", "Resupply", "Eagle Airstrike"])), "the player picks another");
        assert_eq!(names(&e)[3], "Eagle Airstrike");
        assert!(!e.prefill(vec![]));
    }

    #[test]
    fn the_mission_takes_over_the_prefilled_slots() {
        let mut e = engine();
        e.prefill(card(&e, &["Reinforce", "SoS Beacon", "Resupply", "Orbital Laser"]));
        let (reinforce, railcannon) = (idx(&e, "Reinforce"), idx(&e, "Orbital Railcannon Strike"));
        // the closed HUD shows Reinforce: confirmed, with its status as read
        feed_n(&mut e, &[row(reinforce, 0, Status::Ready)], 2);
        assert_eq!((e.mission_slots(), view(&e, 0).status), (1, Status::Ready));
        assert_eq!(view(&e, 3).status, Status::Unavailable, "not seen in the mission yet");
        // another stratagem read where the card said otherwise replaces it
        feed_n(&mut e, &[row(railcannon, 3, Status::Ready)], 2);
        assert_eq!(names(&e)[3], "Orbital Railcannon Strike");
        assert!(!e.prefill(card(&e, &["Reinforce"])), "slots read in a mission are never prefilled over");
    }

    #[test]
    fn the_open_menu_drops_prefilled_slots_it_does_not_list() {
        let mut e = engine();
        e.prefill(card(&e, &["Reinforce", "SoS Beacon", "Resupply", "Call In Super Destroyer", "Orbital Laser"]));
        assert!(e.menu_read(&[menu(&e, &["Reinforce", "SoS Beacon", "Resupply", "Orbital Laser"])]));
        assert_eq!(names(&e), ["Reinforce", "SoS Beacon", "Resupply", "Orbital Laser"]);
        assert_eq!(e.mission_slots(), 4);
        assert!((0..4).all(|k| view(&e, k).status == Status::Ready));
        // a closed menu (a row or two) says nothing about the slots it does not show
        let mut e = engine();
        e.prefill(card(&e, &["Reinforce", "SoS Beacon", "Resupply", "Orbital Laser"]));
        e.menu_read(&[menu(&e, &["Reinforce"])]);
        assert_eq!(names(&e), ["Reinforce", "SoS Beacon", "Resupply", "Orbital Laser"]);
    }

    #[test]
    fn the_menu_has_settled_once_two_readings_agree_on_its_rows() {
        let e = engine();
        let open = menu(&e, &["Reinforce", "SoS Beacon", "Resupply", "Orbital Laser"]);
        assert!(!menu_settled(None, &open), "one reading is not enough");
        assert!(menu_settled(Some(&open), &open));
        assert!(!menu_settled(Some(&open[..3]), &open), "a row still sliding in");
        assert!(!menu_settled(Some(&open[..3]), &open[..3]), "too few rows: the closed HUD");
        let mut unread = open.clone();
        unread[3].status = Status::Unknown;
        assert!(!menu_settled(Some(&unread), &unread), "a status line not read yet");
        let mut moved = open.clone();
        moved[3].slot = 4;
        assert!(!menu_settled(Some(&moved), &open));
    }

    #[test]
    fn the_card_read_twice_fills_the_slots_and_announces_a_mission() {
        let mut e = engine();
        assert!(!e.mission_expected());
        let c = card(&e, &["Reinforce", "SoS Beacon", "Resupply"]);
        assert!(!e.take_card(Some(c.clone())), "one reading is not enough");
        assert!(e.mission_expected(), "the loadout screen is on: a mission is coming");
        assert!(e.take_card(Some(c.clone())));
        assert_eq!(names(&e), ["Reinforce", "SoS Beacon", "Resupply"]);
        assert!(!e.take_card(None) && !e.take_card(Some(c.clone())), "a gap starts the count again");
        assert!(e.mission_expected());
        e.card_seen = Some(ago(MISSION_EXPECTED.as_secs_f32() + 1.0));
        assert!(!e.mission_expected(), "too long ago");
    }

    #[test]
    fn the_open_menu_moves_the_slots_along_when_a_mission_hands_out_a_stratagem() {
        let mut e = engine();
        let before = ["Reinforce", "Resupply", "Orbital Laser", "LAS-99 Quasar Cannon", "AX/LAS-5 Rover", "EXO-49 Emancipator Exosuit"];
        let rows = menu(&e, &before);
        feed_n(&mut e, &rows, 2);
        assert_eq!(names(&e), before);
        // the Quasar Cannon cools down, its timer read
        let quasar = idx(&e, "LAS-99 Quasar Cannon");
        let reinforce = idx(&e, "Reinforce");
        feed_n(&mut e, &[row(reinforce, 0, Status::Ready), cooling(quasar, 3, 0.5, Some(120))], 2);
        assert_eq!(view(&e, 3).status, Status::Cooldown);
        // SEAF Artillery comes in after Resupply: everything after it moves along at once, keeping its state
        let after = [
            "Reinforce",
            "Resupply",
            "SEAF Artillery",
            "Orbital Laser",
            "LAS-99 Quasar Cannon",
            "AX/LAS-5 Rover",
            "EXO-49 Emancipator Exosuit",
        ];
        let mut rows = menu(&e, &after);
        rows[4] = cooling(quasar, 4, 0.5, Some(118));
        assert!(!feed(&mut e, &rows) || names(&e) == before, "one reading of the new layout is not enough");
        assert!(feed(&mut e, &rows));
        assert_eq!(names(&e), after);
        let (left, timed) = left(&e, 4);
        assert!(timed && left > 100.0, "the Quasar Cannon kept its cooldown: {left} s");
        // a used SOS Beacon leaves the menu: the slots close up
        let mut e = engine();
        let with_sos = ["Reinforce", "SoS Beacon", "Resupply", "Orbital Laser", "LAS-99 Quasar Cannon"];
        let rows = menu(&e, &with_sos);
        feed_n(&mut e, &rows, 2);
        let without = ["Reinforce", "Resupply", "Orbital Laser", "LAS-99 Quasar Cannon"];
        let rows = menu(&e, &without);
        feed_n(&mut e, &rows, 2);
        assert_eq!(names(&e), without);
    }

    #[test]
    fn notices_of_the_closed_hud_lay_nothing_out() {
        let mut e = engine();
        let open = ["Reinforce", "SoS Beacon", "Resupply", "Orbital Laser", "LAS-99 Quasar Cannon"];
        let rows = menu(&e, &open);
        feed_n(&mut e, &rows, 2);
        // the closed HUD: Reinforce, and a notice of the Quasar Cannon at its own place, with a gap before it
        let closed = [row(idx(&e, "Reinforce"), 0, Status::Unknown), row(idx(&e, "LAS-99 Quasar Cannon"), 4, Status::Ready)];
        feed_n(&mut e, &closed, 3);
        assert_eq!(names(&e), open, "a closed HUD is no layout");
        // rows not read surely are no layout either
        let mut unsure = menu(&e, &["Reinforce", "Resupply", "Orbital Laser", "LAS-99 Quasar Cannon"]);
        unsure.iter_mut().for_each(|r| r.sure = false);
        feed_n(&mut e, &unsure, 2);
        assert_eq!(names(&e)[1], "SoS Beacon");
    }

    #[test]
    fn what_the_player_chose_outlasts_the_card_until_the_mission_is_over() {
        let mut e = engine();
        let c = card(&e, &["Reinforce", "SoS Beacon", "Resupply", "Orbital Laser", "LIFT-182 Warp Pack"]);
        e.take_card(Some(c.clone()));
        assert!(e.chosen().is_empty(), "one reading is not enough");
        e.take_card(Some(c));
        let want = vec![idx(&e, "Orbital Laser"), idx(&e, "LIFT-182 Warp Pack")];
        assert_eq!(e.chosen(), want);
        // the loadout screen goes (hellpod, loading): the card reads as nothing, the choice stays
        e.take_card(None);
        assert_eq!(e.chosen(), want);
        e.clear();
        assert_eq!(e.chosen(), want, "the Clear key leaves the loadout chosen");
        // a mission, then no HUD long enough: the mission is over and the choice goes with it
        let reinforce = idx(&e, "Reinforce");
        feed_n(&mut e, &[row(reinforce, 0, Status::Ready)], 2);
        e.last_hud = Some(ago(LOBBY_RESET.as_secs_f32() + 1.0));
        assert!(feed(&mut e, &[]), "the mission is over");
        assert!(e.chosen().is_empty());
    }

    #[test]
    fn a_code_read_gets_the_catalog_code_it_fits_or_stands_as_read() {
        let sssd = ["DDDUU", "DDDDDUU"];
        assert_eq!(code_from_read("DDDUU", &sssd).as_deref(), Some("DDDUU"));
        assert_eq!(code_from_read("DDDDDUU", &sssd).as_deref(), Some("DDDDDUU"), "its other code");
        assert_eq!(code_from_read("DDDDD", &sssd).as_deref(), Some("DDDDDUU"), "cut short, fitting one only");
        assert_eq!(code_from_read("DDD", &sssd), None, "cut short before the two part");
        assert_eq!(code_from_read("RDURDL", &["RDURD"]).as_deref(), Some("RDURD"), "a stray arrow past the end");
        assert_eq!(code_from_read("UDDUUDDU", &["UDDUUDDUU"]).as_deref(), Some("UDDUUDDUU"), "9 arrows read to the capture's edge");
        assert_eq!(code_from_read("LLRRU", &["RDURD"]).as_deref(), Some("LLRRU"), "a code the catalog doesn't know");
        assert_eq!(code_from_read("LLRRUUDD", &["RDURD"]), None, "unknown and at the capture's edge: it may go on");
        assert_eq!(code_from_read("LR", &["RDURD"]), None, "too short to be a code");
    }

    #[test]
    fn a_code_read_on_the_menu_is_typed_once_confirmed() {
        let mut e = engine();
        let sssd = idx(&e, "SSSD Delivery");
        let shown = |code: &str| Row { code: Some(code.into()), ..row(sssd, 0, Status::Ready) };
        feed_n(&mut e, &[row(sssd, 0, Status::Ready)], 3);
        assert_eq!(view(&e, 0).code, None, "no code read: the key types the catalog's");
        feed_n(&mut e, &[shown("DDDDDUU")], CODE_CONFIRM as usize - 1);
        assert_eq!(view(&e, 0).code, None, "not read often enough yet");
        assert!(feed(&mut e, &[shown("DDDDDUU")]), "the code confirmed changes the key");
        assert_eq!(view(&e, 0).code.as_deref(), Some("DDDDDUU"), "SSSD's other code, as the menu shows it");
        feed(&mut e, &[shown("DDDDDU")]);
        assert_eq!(view(&e, 0).code.as_deref(), Some("DDDDDUU"), "one stray reading changes nothing");
        feed_n(&mut e, &[shown("DDDUU")], CODE_CONFIRM as usize);
        assert_eq!(view(&e, 0).code, None, "the main code confirmed: the catalog's again");
    }

    #[test]
    fn a_loadout_read_after_a_mission_stays() {
        let mut e = engine();
        let reinforce = idx(&e, "Reinforce");
        feed_n(&mut e, &[row(reinforce, 0, Status::Ready)], 2);
        e.last_hud = Some(ago(LOBBY_RESET.as_secs_f32() + 1.0));
        assert!(feed(&mut e, &[]), "the mission is over");
        assert!(e.prefill(card(&e, &["Reinforce", "SoS Beacon"])));
        assert!(!feed(&mut e, &[]));
        assert_eq!(names(&e), ["Reinforce", "SoS Beacon"]);
    }

    #[test]
    fn clear_forgets_slots_and_pending_readings() {
        let mut e = engine();
        let (reinforce, laser) = (idx(&e, "Reinforce"), idx(&e, "Orbital Laser"));
        feed_n(&mut e, &[row(reinforce, 0, Status::Ready)], 2);
        feed(&mut e, &[row(laser, 3, Status::Ready)]);
        e.clear();
        assert!(e.loadout().is_empty() && e.pending.is_empty());
        feed(&mut e, &[row(laser, 3, Status::Ready)]);
        assert!(e.loadout().is_empty(), "the reading before clear() does not count");
        feed(&mut e, &[row(laser, 3, Status::Ready)]);
        assert_eq!(names(&e), ["-", "-", "-", "Orbital Laser"]);
    }

    // ---- real frames through the engine ----

    #[test]
    fn loadout_from_real_frames() {
        let mut e = engine();
        let (x0, y0, x1, y1) = column_box(2560, 1440, 1.0);
        let read = |e: &mut Engine, name: &str| {
            let col = Img::load_png(&Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/{name}.png"))).expect(name);
            assert_eq!((col.w, col.h), (x1 - x0, y1 - y0));
            e.matcher.read_rows(&col, 1440, (x0, y0), 1.0)
        };
        let open = read(&mut e, "menu_open");
        let fed = Instant::now(); // the timers are read from here on
        feed_n(&mut e, &open, 2);
        assert_eq!(
            names(&e),
            ["Reinforce", "SoS Beacon", "Resupply", "Orbital Laser", "LAS-99 Quasar Cannon", "LIFT-182 Warp Pack", "M-102 Gunner FRV"]
        );
        let statuses: Vec<Status> = e.loadout().iter().flatten().map(|v| v.status).collect();
        use Status::*;
        assert_eq!(statuses, [Ready, Ready, Ready, Active, Cooldown, Cooldown, Ready]);
        // the menu closes: the closed HUD only lists Reinforce, and the Orbital Laser (impact) drops out
        let closed = read(&mut e, "menu_closed");
        feed(&mut e, &closed);
        assert_eq!(view(&e, 3).status, Active);
        assert_eq!(view(&e, 0).status, Ready, "a row without a status line keeps its status");
        e.slots.get_mut(&3).unwrap().seen_at = ago(ACTIVE_GONE.as_secs_f32() + 0.5);
        assert!(feed(&mut e, &closed));
        assert_eq!(view(&e, 3).status, Cooldown);
        assert_eq!(e.loadout().len(), 7, "slots off screen are remembered");
        // the timers read on menu_open drive the countdowns: Quasar Cannon T-04:50, Warp Pack T-04:48, at most a
        // rounding more, less the time gone since they were read (the reads above take seconds in a slow test build)
        for (k, shown) in [(4, 290.0), (5, 288.0)] {
            let (secs, timed) = left(&e, k);
            let (most, least) = (shown + TIMER_ROUNDING, shown + TIMER_ROUNDING - fed.elapsed().as_secs_f32());
            assert!(timed && secs <= most + 0.05 && secs >= least - 0.05, "slot {k}: {secs} s left, {least}..{most} expected");
        }
    }

    // ---- cooldown timers ----

    fn cooling(index: usize, slot: usize, fill: f32, timer: Option<u32>) -> Row {
        Row { fill_bottom: fill, timer, ..row(index, slot, Status::Cooldown) }
    }

    /// Seconds until slot `k` is expected to be ready, and whether a timer says so.
    fn left(e: &Engine, k: usize) -> (f32, bool) {
        let r = view(e, k).ready.unwrap_or_else(|| panic!("slot {k} has no end: {:?}", view(e, k)));
        (r.at.saturating_duration_since(Instant::now()).as_secs_f32(), r.timed)
    }

    /// Let `secs` seconds pass for slot `k`: every moment it remembers moves that far into the past.
    fn pass(e: &mut Engine, k: usize, secs: f32) {
        let back = |t: Instant| t.checked_sub(Duration::from_secs_f32(secs)).expect("system uptime shorter than the simulated wait");
        let s = e.slots.get_mut(&k).unwrap();
        (s.fill_at, s.seen_at) = (back(s.fill_at), back(s.seen_at));
        s.ends = s.ends.map(|(lo, hi)| (back(lo), back(hi)));
        s.cooldown_from = s.cooldown_from.map(back);
        s.assumed = s.assumed.map(back);
    }

    /// Seen live: "Cooldown T-00:09" over a 1 px fill read as about 1% of a 300 s cooldown. The fill
    /// alone said 3 s left; the timer keeps the key cooling until its end, and the fill drawn does not
    /// reach zero before.
    #[test]
    fn a_timer_read_near_the_end_keeps_the_slot_cooling_until_it_runs_out() {
        let mut e = engine();
        let laser = idx(&e, "Orbital Laser");
        feed_n(&mut e, &[cooling(laser, 3, 0.01, Some(9))], 2);
        let (secs, timed) = left(&e, 3);
        assert!(timed && (secs - (9.0 + TIMER_ROUNDING)).abs() < 0.1, "{secs} s left");
        for t in [3.0, 6.0, 9.5, 9.9] {
            let elapsed = 9.0 + TIMER_ROUNDING - left(&e, 3).0;
            pass(&mut e, 3, t - elapsed);
            let v = view(&e, 3);
            assert_eq!(v.status, Status::Cooldown, "{t} s after the reading");
            assert!(v.fill > 0.0, "the fill is still drawn {t} s after the reading");
        }
        pass(&mut e, 3, 0.2);
        let v = view(&e, 3);
        assert_eq!((v.status, v.ready), (Status::Ready, None), "the timer has run out");
    }

    #[test]
    fn without_a_timer_the_end_is_estimated_from_the_fill() {
        let mut e = engine();
        let laser = idx(&e, "Orbital Laser"); // 300 s on the wiki
        feed_n(&mut e, &[cooling(laser, 3, 0.5, None)], 2);
        let (secs, timed) = left(&e, 3);
        assert!(!timed && (secs - 150.0).abs() < 0.5, "{secs} s left");
        assert_eq!(e.slots[&3].ends, None);
    }

    #[test]
    fn a_cooldown_seen_starting_learns_its_length_from_the_timer() {
        let mut e = engine();
        let laser = idx(&e, "Orbital Laser");
        feed_n(&mut e, &[row(laser, 3, Status::Ready)], 2);
        feed_n(&mut e, &[cooling(laser, 3, 0.95, Some(290))], 2);
        let length = 290.0 + TIMER_ROUNDING;
        assert!((e.cooldown_s(laser) - length).abs() < 0.1, "{}", e.cooldown_s(laser));
        assert!((view(&e, 3).fill - 1.0).abs() <= 1.0 / 72.0, "a cooldown just started is full");
        // 100 s on, the timer agrees with the length learned
        pass(&mut e, 3, 100.0);
        feed(&mut e, &[cooling(laser, 3, 0.6, Some(190))]);
        assert!((e.cooldown_s(laser) - length).abs() < 0.1, "{}", e.cooldown_s(laser));
        let v = view(&e, 3);
        assert!((v.fill - (190.0 + TIMER_ROUNDING) / length).abs() <= 1.0 / 72.0, "{}", v.fill);
    }

    #[test]
    fn a_fill_never_sets_the_length() {
        let mut e = engine();
        let laser = idx(&e, "Orbital Laser");
        // first seen half way through, whatever its fill reads: no start seen, so the wiki's length stays
        for fill in [0.3, 0.6, 1.0] {
            let mut e = engine();
            feed_n(&mut e, &[cooling(laser, 3, fill, Some(120))], 2);
            assert_eq!(e.cooldown_s(laser), 300.0, "fill {fill}");
            let (secs, timed) = left(&e, 3);
            assert!(timed && (secs - (120.0 + TIMER_ROUNDING)).abs() < 0.1, "the end follows the timer");
            assert!((view(&e, 3).fill - (120.0 + TIMER_ROUNDING) / 300.0).abs() <= 1.0 / 72.0, "fill {fill}");
        }
        // a timer longer than the length known: the key is full, never over
        feed_n(&mut e, &[cooling(laser, 3, 0.6, Some(400))], 2);
        assert!((view(&e, 3).fill - 1.0).abs() <= 1.0 / 72.0);
    }

    /// Seen live: a Gunner FRV with 468 s of its 480 s left read as 63% covered (its white car and bullets look
    /// the same under the cream fill); the length was taken as 468 / 0.63 = 743 s and the key showed 62%.
    #[test]
    fn a_low_fill_reading_does_not_shrink_the_key() {
        let mut e = engine();
        let frv = idx(&e, "M-102 Gunner FRV");
        assert_eq!(e.cooldown_s(frv), 480.0);
        feed_n(&mut e, &[cooling(frv, 6, 0.63, Some(468))], 2);
        assert_eq!(e.cooldown_s(frv), 480.0);
        let f = view(&e, 6).fill;
        assert!((f - (468.0 + TIMER_ROUNDING) / 480.0).abs() <= 1.0 / 72.0, "{f}");
    }

    #[test]
    fn a_call_in_seen_on_its_way_then_cooling_starts_its_cooldown() {
        let mut e = engine();
        let frv = idx(&e, "M-102 Gunner FRV");
        feed_n(&mut e, &[row(frv, 6, Status::Active)], 2);
        // its fill reads low, but the cooldown has just started: the timer gives its length
        feed_n(&mut e, &[cooling(frv, 6, 0.63, Some(431))], 2);
        let length = 431.0 + TIMER_ROUNDING;
        assert!((e.cooldown_s(frv) - length).abs() < 0.5, "{}", e.cooldown_s(frv));
        assert!((view(&e, 6).fill - 1.0).abs() <= 1.0 / 72.0, "a cooldown just started is full");
    }

    #[test]
    fn readings_narrow_the_end_and_one_that_does_not_fit_starts_afresh() {
        let mut e = engine();
        let laser = idx(&e, "Orbital Laser");
        feed_n(&mut e, &[cooling(laser, 3, 0.2, Some(20))], 2);
        let first = view(&e, 3).ready.unwrap().at;
        // the same second, read again: nothing new, so the view, and the key, stay as they are
        pass(&mut e, 3, 0.4);
        let before = e.loadout();
        assert!(!feed(&mut e, &[cooling(laser, 3, 0.2, Some(20))]));
        assert_eq!(e.loadout(), before, "no repaint");
        // the shown second changes 0.7 s after the first reading: the end is 20 s after that change
        pass(&mut e, 3, 0.3);
        feed(&mut e, &[cooling(laser, 3, 0.2, Some(19))]);
        let (secs, _) = left(&e, 3);
        assert!((secs - 20.0).abs() < 0.05, "{secs} s left");
        let moved = first.checked_sub(Duration::from_secs_f32(0.7)).unwrap().saturating_duration_since(view(&e, 3).ready.unwrap().at);
        assert!((moved.as_secs_f32() - 0.3).abs() < 0.05, "the end came 0.3 s closer, {moved:?}");
        // a reading that fits none of that: a new countdown
        feed(&mut e, &[cooling(laser, 3, 0.9, Some(50))]);
        let (secs, timed) = left(&e, 3);
        assert!(timed && (secs - (50.0 + TIMER_ROUNDING)).abs() < 0.05, "{secs} s left");
    }

    #[test]
    fn a_new_status_forgets_the_timer_and_inbound_timers_are_not_cooldowns() {
        let mut e = engine();
        let laser = idx(&e, "Orbital Laser");
        feed_n(&mut e, &[cooling(laser, 3, 0.2, Some(20))], 2);
        feed_n(&mut e, &[row(laser, 3, Status::Ready)], 2);
        assert_eq!((view(&e, 3).status, view(&e, 3).ready, e.slots[&3].ends), (Status::Ready, None, None));
        feed_n(&mut e, &[Row { timer: Some(7), fill_top: 0.2, ..row(laser, 3, Status::Active) }], 2);
        assert_eq!((view(&e, 3).status, view(&e, 3).ready, e.slots[&3].ends), (Status::Active, None, None));
        feed_n(&mut e, &[cooling(laser, 3, 0.95, None)], 2);
        assert!(!left(&e, 3).1, "a cooldown without a timer on screen is an estimate");
    }
}
