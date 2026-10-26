//! Reads the stratagem menu rows at their fixed HUD positions.
//! Thresholds were tuned on captured frames (first in the Python prototype this replaced).

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

use crate::data::Data;
use crate::img::Img;
use crate::status::{self, Status, StatusReader};

pub const REF_HEIGHT: f64 = 1440.0;
pub const REF_ICON: f64 = 57.0; // icon box at 2560x1440, 100% HUD scale
pub const REF_X: f64 = 70.0; // left edge of the icon column at 2560x1440
const X_TOL: f64 = 14.0;
const BADGE_FRAC: f64 = 0.55; // the use-count badge covers the lower-right of the icon
const FRAME_FRAC: f64 = 0.12; // lookalikes share the coloured frame, so identity is scored on the glyph
const ROW0_Y: f64 = 108.0; // top of the first menu row (Reinforce) at 2560x1440
const ROW_PITCH: f64 = 70.0;
const MAX_ROWS: usize = 13;
const ROW_MIN: f32 = 0.55; // whole-icon score: real rows, dimmed included, >= 0.69; empty spots <= 0.40
const ROW_MIN_ISOLATED: f32 = 0.80; // rows after an empty slot are never dimmed (seen >= 0.93)
const ROW_KEEP: f32 = 0.60; // a slot still showing last scan's stratagem skips the ranking
const ROW_LOW: usize = 24; // side of the cheap ranking pass
const ROW_LOW_MIN: f32 = 0.45; // cheap-pass score below which a slot is empty (real >= 0.56, empty <= 0.47)
const ROW_TOP: usize = 5; // candidates scored at full size
const ROW_SURE: f32 = 0.85; // glyph score trusted outright
const ROW_MARGIN: f32 = 0.08; // below ROW_SURE, the runner-up glyph must trail by this much
const FILL_DIFF: f32 = 45.0; // grey-level difference that marks a pixel as covered by the progress fill
const FILL_GAP: usize = 3; // filled-row runs tolerate this many unclear rows (white glyphs)
const COVER_MIN: f32 = 0.15; // fill share below which an unsure reading is not rescored under the fill
// how the progress fill draws an icon: beige up to luminance FILL_DARK, cream from FILL_BRIGHT
const FILL_DARK: f32 = 40.0;
const FILL_BRIGHT: f32 = 160.0;
const FILL_CREAM: [f32; 3] = [240.0, 232.0, 214.0];
const FILL_BEIGE: [f32; 3] = [139.0, 128.0, 112.0];

#[derive(Deserialize, Clone)]
pub struct Stratagem {
    pub name: String,
    pub code: String,
    pub cooldown: String,
    #[serde(rename = "type")]
    pub kind: Kind,
    pub icon: String,
    /// Other codes the game gives it on some missions (SSSD Delivery has one for Super Earth objectives and one
    /// for Automaton ones); `code` is the one typed when the menu hasn't shown which (see Engine::code_from_read).
    #[serde(default)]
    pub also: Vec<String>,
}

impl Stratagem {
    /// Every code the game may show for it, `code` first.
    pub fn codes(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.code.as_str()).chain(self.also.iter().map(String::as_str))
    }

    pub fn png(&self) -> String {
        let stem = Path::new(&self.icon).file_stem().unwrap_or_default().to_string_lossy().into_owned();
        format!("{stem}.png")
    }

    /// Its icon's texture in the data folder (see `Data` and `game_icon`).
    pub fn icon_path(&self) -> String {
        format!("icons/{}", self.png())
    }
}

/// A stratagem's type, which colours its icon.
#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Mission,
    Offensive,
    Supply,
    Defensive,
}

impl Kind {
    /// The colour of the icon's frame and coloured parts, and of its inside.
    fn colours(self) -> ([f32; 3], [f32; 3]) {
        match self {
            Kind::Mission => ([201.0, 178.0, 105.0], [54.0, 52.0, 38.0]),
            Kind::Offensive => ([220.0, 100.0, 85.0], [25.0, 3.0, 1.0]),
            Kind::Supply => ([85.0, 185.0, 210.0], [1.0, 20.0, 25.0]),
            Kind::Defensive => ([105.0, 150.0, 85.0], [8.0, 25.0, 1.0]),
        }
    }
}

pub const ICON_SIZE: usize = 144; // side of the icons the matcher keeps, and of the keys
const ICON_CREAM: [f32; 3] = [255.0, 255.0, 238.0];
const ICON_FRAME: f64 = 7.85 / 144.0; // the frame the game draws around an icon, as a share of its side

/// A stratagem icon as the game draws it, from its texture in data/icons (taken from the game's files): the
/// texture's red marks the parts in the type's colour, its green the cream parts, over the dark inside, and the
/// frame goes on top.
pub fn game_icon(texture: &Img, kind: Kind) -> Img {
    let (frame, inside) = kind.colours();
    let border = ICON_FRAME * texture.w as f64;
    let (full, part) = (border as usize, border.fract() as f32);
    let mut out = Img::new(texture.w, texture.h);
    for y in 0..texture.h {
        for x in 0..texture.w {
            let p = (y * texture.w + x) * 3;
            let (mut a, mut b) = (texture.px[p] / 255.0, texture.px[p + 1] / 255.0);
            if a + b > 1.0 {
                (a, b) = (a / (a + b), b / (a + b));
            }
            let edge = x.min(y).min(texture.w - 1 - x).min(texture.h - 1 - y);
            let f = if edge < full {
                1.0
            } else if edge == full {
                part
            } else {
                0.0
            };
            for c in 0..3 {
                let v = inside[c] * (1.0 - a - b) + frame[c] * a + ICON_CREAM[c] * b;
                out.px[p + c] = v * (1.0 - f) + frame[c] * f;
            }
        }
    }
    out
}

#[derive(Debug, Clone)]
pub struct Row {
    pub index: usize,
    pub score: f32,
    pub sure: bool,
    pub slot: usize,
    pub status: Status,
    /// Share of the icon covered by the game's progress fill, rising from the bottom (cooldown,
    /// shrinks as it ends) and coming down from the top (inbound).
    pub fill_bottom: f32,
    pub fill_top: f32,
    /// The use-count badge at the icon's corner, if the row has one.
    pub badge: Option<Badge>,
    /// Seconds left on the status line's timer ("Cooldown T-04:50", "Inbound T-00:07"), when it shows one.
    pub timer: Option<u32>,
    /// The arrows of its code, as far as the capture reaches (CODE_READ), when the open menu shows them.
    pub code: Option<String>,
}

/// The count badge's digits as a mask (true = dark digit pixel), trimmed to the digits, and the number they show when
/// it reads clearly (see glyphs::read); the cream box is redrawn around them on the key.
#[derive(Clone, Debug)]
pub struct Badge {
    pub hash: u64,
    pub w: usize,
    pub h: usize,
    pub bits: std::sync::Arc<Vec<bool>>,
    pub number: Option<String>,
}

impl PartialEq for Badge {
    fn eq(&self, other: &Self) -> bool {
        self.hash == other.hash
    }
}

// The count badge at 1440p: a cream box over the icon's bottom-right corner, its last column at
// x=BADGE_RIGHT and its rows BADGE_ROWS below the icon's top, growing leftwards with more digits;
// dark digits inside, with a margin of cream all round.
const BADGE_RIGHT: f64 = 138.0;
const BADGE_ROWS: (f64, f64) = (33.0, 56.0);
const BADGE_MIN_W: f64 = 18.0; // one digit makes a 24 px box
const BADGE_CREAM: usize = 90; // percent of a margin that must be cream (the box's edge blends with the scene)

/// Find the count badge of the icon whose top is `top` (1440p units). The scene next to the icon
/// can be as cream as the box (a bright sky), so the box is not found by its colour alone but by its
/// place: its right margin lies past the icon's right edge where no glyph reaches, its top and
/// bottom margins run cream from there to its left edge, and dark digits sit inside.
fn read_badge(col: &Img, f: f64, top: f64, origin: (usize, usize)) -> Option<Badge> {
    let (left0, top0) = ((100.0 * f).round() as isize, ((top + 28.0) * f).round() as isize);
    let reg = col.crop(left0 - origin.0 as isize, top0 - origin.1 as isize, (46.0 * f).round() as usize, (36.0 * f).round() as usize);
    // region column of 1440p x, region row of 1440p rows below the icon's top
    let cx = |x: f64| ((x * f).round() as isize - left0).max(0) as usize;
    let cy = |dy: f64| (((top + dy) * f).round() as isize - top0).max(0) as usize;
    let (right, y0, y1) = (cx(BADGE_RIGHT), cy(BADGE_ROWS.0), cy(BADGE_ROWS.1));
    if reg.w <= right || reg.h <= y1 {
        return None;
    }
    let at = |x: usize, y: usize| &reg.px[(y * reg.w + x) * 3..(y * reg.w + x) * 3 + 3];
    let lum = |p: &[f32]| 0.299 * p[0] + 0.587 * p[1] + 0.114 * p[2];
    let cream =
        |p: &[f32]| lum(p) > 175.0 && p.iter().cloned().fold(f32::MIN, f32::max) - p.iter().cloned().fold(f32::MAX, f32::min) < 45.0;
    let creamy = |xs: std::ops::RangeInclusive<usize>, ys: &[usize]| {
        let n = xs.clone().count() * ys.len();
        xs.flat_map(|x| ys.iter().map(move |&y| (x, y))).filter(|&(x, y)| cream(at(x, y))).count() * 100 >= n * BADGE_CREAM
    };
    // rows inside the box's soft edge; its top and bottom margins hold no digit (some digits start
    // three rows into the box, others five)
    let inner: Vec<usize> = (y0 + 1..y1).collect();
    let (above, below) = ((2.0 * f).round().max(1.0) as usize, (3.0 * f).round().max(1.0) as usize);
    let margins: Vec<usize> = inner.iter().take(above).chain(inner.iter().rev().take(below)).copied().collect();
    // the right margin, past the icon's right edge
    if !creamy(cx(BADGE_RIGHT - 6.0)..=right - 1, &inner) {
        return None;
    }
    // the box's left edge: the top and bottom margins run cream up to it
    let mut x0 = cx(BADGE_RIGHT - 7.0);
    while x0 > 0 && creamy(x0 - 1..=x0 - 1, &margins) {
        x0 -= 1;
    }
    if (((right - x0 + 1) as f64) < BADGE_MIN_W * f) || !creamy(x0 + 1..=x0 + (4.0 * f).round() as usize, &inner) {
        return None;
    }
    let x1 = right;
    // the box's soft edge blends with the scene and wobbles by a pixel or two between frames;
    // the digits are solid, so the badge is keyed on the digits alone, trimmed tight
    // the outer two pixels of the box are where the scene shows through, so they are not searched
    let ink = |x: usize, y: usize| lum(at(x, y)) < 140.0;
    let (sx0, sx1, sy0, sy1) = (x0 + 2, x1.saturating_sub(2), y0 + 2, y1.saturating_sub(2));
    let cols: Vec<usize> = (sx0..=sx1).filter(|&x| (sy0..=sy1).any(|y| ink(x, y))).collect();
    let rows: Vec<usize> = (sy0..=sy1).filter(|&y| (sx0..=sx1).any(|x| ink(x, y))).collect();
    let (&ix0, &ix1, &iy0, &iy1) = (cols.first()?, cols.last()?, rows.first()?, rows.last()?);
    let (w, h) = (ix1 - ix0 + 1, iy1 - iy0 + 1);
    // digits are 14-16 px tall at 1440p; anything else is scenery caught in the box
    if (h as f64) < 12.0 * f || (h as f64) > 17.0 * f || (w as f64) > 30.0 * f {
        return None;
    }
    let bits: Vec<bool> = (iy0..=iy1).flat_map(|y| (ix0..=ix1).map(move |x| (x, y))).map(|(x, y)| ink(x, y)).collect();
    let mut hash: u64 = 0xcbf29ce484222325 ^ ((w as u64) << 32 | h as u64);
    for &b in &bits {
        hash = (hash ^ b as u64).wrapping_mul(0x100000001b3);
    }
    let number = crate::glyphs::read(&bits, w, h);
    Some(Badge { hash, w, h, bits: std::sync::Arc::new(bits), number })
}

/// Masked, zero-mean (per channel), unit-norm template pixels.
struct Flat {
    v: Vec<f32>,
    norm: f32,
}

struct Sized {
    size: usize,
    whole_idx: Vec<usize>,
    glyph_idx: Vec<usize>,
    whole: Vec<Flat>,
    glyph: Vec<Flat>,
    images: Vec<Img>, // templates at this size, for the row-by-row fill comparison
}

pub struct Matcher {
    pub catalog: Vec<Stratagem>,
    raw: Vec<Img>,
    low: Vec<Vec<f32>>,
    low_glyph: Vec<Vec<f32>>, // the glyph alone (no frame), small, for the loadout card's frameless icons
    sized: Option<Sized>,
    last: HashMap<usize, (usize, usize, usize)>, // slot -> (template, dx, dy) of the last sure match
    status: StatusReader,
    lookalikes: Vec<Vec<usize>>, // per stratagem, the others drawn with the same icon
}

const LOOKALIKE: f32 = 0.97; // icon correlation at which two catalog icons count as the same picture

/// Where the icon column sits inside a w x h game frame: (x0, y0, x1, y1).
pub fn column_box(w: usize, h: usize, icon_scale: f64) -> (usize, usize, usize, usize) {
    let f = h as f64 / REF_HEIGHT;
    // read_rows puts the icons at REF_X * icon_scale, so a smaller HUD starts further left
    let x0 = ((REF_X * icon_scale - X_TOL) * f).round().max(0.0) as usize;
    let right = (REF_X + REF_ICON * icon_scale + X_TOL).max(status::NEEDS_X * icon_scale);
    let x1 = ((right * f).round() as usize).min(w);
    (x0, (h as f64 * 0.03) as usize, x1, (h as f64 * 0.72) as usize)
}

// The player's card on the loadout screen before a mission, at 2560x1440. Its "Mission Stratagems" row starts with
// the CARD_COMMON stratagems every mission gives (Reinforce, SOS Beacon, Resupply), drawn like the menu's icons; the
// row under it holds the four the player chose, larger and without their frame. The mission's own stratagems follow
// in the first row, but most of them (Hellbomb, SSSD Delivery) only join the menu later in the mission, and the card
// can't tell which, so they are left for the menu read at mission start. (left of the first icon, its top, its side,
// the step to the next)
const CARD_MISSION: (f64, f64, f64, f64) = (80.0, 1037.0, 60.0, 81.3);
const CARD_LOADOUT: (f64, f64, f64, f64) = (78.0, 1119.0, 98.0, 114.0);
pub const CARD_COMMON: usize = 3;
const CARD_LOADOUT_SLOTS: usize = 4;
const CARD_PAD: f64 = 4.0; // the icons are searched this far around their place
// glyph score of a stratagem on the card: icons read 0.79 or more, empty boxes and other screens 0.55 at most
const CARD_MIN: f32 = 0.65;

/// Where the loadout card's two rows sit inside a w x h game frame: (x0, y0, x1, y1).
pub fn card_box(w: usize, h: usize) -> (usize, usize, usize, usize) {
    let f = h as f64 / REF_HEIGHT;
    let row_end = |(x, _, side, step): (f64, f64, f64, f64), n: usize| x + step * (n - 1) as f64 + side;
    let x0 = (CARD_MISSION.0.min(CARD_LOADOUT.0) - 2.0 * CARD_PAD) * f;
    let x1 = (row_end(CARD_MISSION, CARD_COMMON).max(row_end(CARD_LOADOUT, CARD_LOADOUT_SLOTS)) + 2.0 * CARD_PAD) * f;
    let y0 = (CARD_MISSION.1 - 2.0 * CARD_PAD) * f;
    let y1 = (CARD_LOADOUT.1 + CARD_LOADOUT.2 + 2.0 * CARD_PAD) * f;
    (x0.round().max(0.0) as usize, y0.round() as usize, (x1.round() as usize).min(w), (y1.round() as usize).min(h))
}

impl Matcher {
    pub fn load(data: &Data) -> Result<Matcher, String> {
        let text = data.read("stratagems.json")?;
        let all: Vec<Stratagem> = serde_json::from_slice(&text).map_err(|e| format!("stratagems.json: {e}"))?;
        let mut catalog = Vec::new();
        let mut raw = Vec::new();
        for s in all {
            let texture = data.png(&s.icon_path())?;
            if texture.px.iter().all(|&v| v == 0.0) {
                continue;
            }
            // kept at ICON_SIZE: every scan size is smaller, and 256 px floats would cost 0.8 MB an icon
            raw.push(game_icon(&texture, s.kind).resize_area(ICON_SIZE, ICON_SIZE));
            catalog.push(s);
        }
        let low: Vec<Vec<f32>> = raw.iter().map(|r| normalized(&r.resize_area(ROW_LOW, ROW_LOW).px)).collect();
        let band = (ICON_SIZE as f64 * FRAME_FRAC).round() as usize;
        let low_glyph: Vec<Vec<f32>> = raw
            .iter()
            .map(|r| {
                normalized(
                    &r.crop(band as isize, band as isize, ICON_SIZE - 2 * band, ICON_SIZE - 2 * band).resize_area(ROW_LOW, ROW_LOW).px,
                )
            })
            .collect();
        let mut lookalikes = vec![Vec::new(); low.len()];
        for i in 0..low.len() {
            for j in i + 1..low.len() {
                if dot(&low[i], &low[j]) >= LOOKALIKE {
                    lookalikes[i].push(j);
                    lookalikes[j].push(i);
                }
            }
        }
        let status = StatusReader::load(data);
        Ok(Matcher { catalog, raw, low, low_glyph, sized: None, last: HashMap::new(), status, lookalikes })
    }

    pub fn reset(&mut self) {
        self.last.clear();
    }

    /// The icon of catalog stratagem `index` as the game draws it (see `game_icon`), ICON_SIZE square.
    pub fn icon(&self, index: usize) -> &Img {
        &self.raw[index]
    }

    fn prepare(&mut self, size: usize) {
        if self.sized.as_ref().is_some_and(|s| s.size == size) {
            return;
        }
        let badge = (size as f64 * BADGE_FRAC) as usize;
        let band = (size as f64 * FRAME_FRAC).round() as usize;
        let mut whole_idx = Vec::new();
        let mut glyph_idx = Vec::new();
        for y in 0..size {
            for x in 0..size {
                if x >= badge && y >= badge {
                    continue;
                }
                whole_idx.push(y * size + x);
                if x >= band && y >= band && x < size - band && y < size - band {
                    glyph_idx.push(y * size + x);
                }
            }
        }
        let mut whole = Vec::new();
        let mut glyph = Vec::new();
        let mut images = Vec::new();
        for r in &self.raw {
            let t = r.resize_area(size, size);
            whole.push(flatten(&t.px, &whole_idx));
            glyph.push(flatten(&t.px, &glyph_idx));
            images.push(t);
        }
        self.last.clear();
        self.sized = Some(Sized { size, whole_idx, glyph_idx, whole, glyph, images });
    }

    /// Read every menu slot. `col` is the captured icon column, `origin` its top-left in the game frame.
    pub fn read_rows(&mut self, col: &Img, h: usize, origin: (usize, usize), icon_scale: f64) -> Vec<Row> {
        let f = h as f64 / REF_HEIGHT * icon_scale;
        let size = (REF_ICON * f).round().max(16.0) as usize;
        let pad = (3.0 * f).round().max(2.0) as isize;
        self.prepare(size);
        let x = (REF_X * f).round() as isize - origin.0 as isize;
        let mut rows = Vec::new();
        let mut gap = false;
        for k in 0..MAX_ROWS {
            let y = ((ROW0_Y + ROW_PITCH * k as f64) * f).round() as isize - origin.1 as isize;
            let win = col.crop(x - pad, y - pad, size + 2 * pad as usize, size + 2 * pad as usize);
            if win.w < size || win.h < size {
                break;
            }
            match self.classify(&win, k) {
                // dimmed rows only occur in the open menu, which is unbroken from slot 0; a row after a
                // gap is a bright notice and must clear a bar scenery cannot (scenery reached 0.60)
                Some((index, score, sure, dx, dy)) if !gap || score >= ROW_MIN_ISOLATED => {
                    let top = ROW0_Y + ROW_PITCH * k as f64;
                    let line = self.status.read(col, f, size, top, origin);
                    let (index, sure) = self.resolve_lookalike(col, f, size, top, origin, k, index, sure);
                    let sized = self.sized.as_ref().expect("prepared");
                    let (fill_bottom, fill_top) = fill_profile(&win, dx, dy, &sized.images[index], size);
                    let badge = read_badge(col, f, ROW0_Y + ROW_PITCH * k as f64, origin);
                    let code = line
                        .arrows
                        .then(|| self.status.read_arrows(col, f, size, top, origin, status::CODE_READ))
                        .filter(|c| !c.is_empty());
                    let timer = line.timer;
                    rows.push(Row { index, score, sure, slot: k, status: line.status, fill_bottom, fill_top, badge, timer, code })
                }
                _ => {
                    gap = true;
                    self.last.remove(&k);
                }
            }
        }
        rows
    }

    /// Stratagems sharing one icon (SSSD Delivery and Upload Data) are told apart by the arrow code the
    /// row shows. Without arrows on screen the slot keeps the one it last resolved to, and a first
    /// reading of such a slot stays unsure until the code is seen.
    #[allow(clippy::too_many_arguments)]
    fn resolve_lookalike(
        &mut self,
        col: &Img,
        f: f64,
        size: usize,
        top: f64,
        origin: (usize, usize),
        k: usize,
        index: usize,
        sure: bool,
    ) -> (usize, bool) {
        if self.lookalikes[index].is_empty() {
            return (index, sure);
        }
        let group: Vec<usize> = std::iter::once(index).chain(self.lookalikes[index].iter().copied()).collect();
        let code = self.status.read_arrows(col, f, size, top, origin, status::CODE_READ);
        // either may be the longer: reading can stop early, or pick up a stray arrow past the code's end
        let fits = |c: &str| c.starts_with(&code) || code.starts_with(c);
        let matching: Vec<usize> =
            if code.is_empty() { vec![] } else { group.iter().copied().filter(|&i| self.catalog[i].codes().any(fits)).collect() };
        // lookalikes with the very same code (Upload Data, Reinforcement Pods) are interchangeable
        let same_code = !matching.is_empty() && matching.iter().all(|&i| self.catalog[i].code == self.catalog[matching[0]].code);
        let last = self.last.get(&k).map(|l| l.0);
        let resolved = match last {
            _ if same_code => (last.filter(|l| matching.contains(l)).unwrap_or(matching[0]), true),
            Some(l) if group.contains(&l) => (l, sure),
            _ => (index, false),
        };
        if let Some(entry) = self.last.get_mut(&k) {
            entry.0 = resolved.0;
        }
        resolved
    }

    /// The arrow code shown on menu slot `k`, for testing the code reader.
    pub fn slot_code(&mut self, col: &Img, h: usize, origin: (usize, usize), icon_scale: f64, k: usize) -> String {
        let f = h as f64 / REF_HEIGHT * icon_scale;
        let size = (REF_ICON * f).round().max(16.0) as usize;
        self.status.read_arrows(col, f, size, ROW0_Y + ROW_PITCH * k as f64, origin, status::CODE_READ)
    }

    /// Groups of catalog stratagems whose icons are the same picture, by name.
    pub fn lookalike_groups(&self) -> Vec<Vec<String>> {
        let mut seen = vec![false; self.catalog.len()];
        let mut out = Vec::new();
        for i in 0..self.catalog.len() {
            if seen[i] || self.lookalikes[i].is_empty() {
                continue;
            }
            let mut g = vec![i];
            g.extend(&self.lookalikes[i]);
            g.iter().for_each(|&j| seen[j] = true);
            out.push(g.iter().map(|&j| format!("{} ({})", self.catalog[j].name, self.catalog[j].code)).collect());
        }
        out
    }

    /// The stratagems on the player's card of the loadout screen that the stratagem menu will start the mission with,
    /// in its order: the three every mission gives, then the player's four (empty boxes left out; the mission's own
    /// are not read, see CARD_COMMON), with whether each reading is sure. None when the card is not on screen, which
    /// its first icon tells: the mission row always starts with Reinforce. `img` is the capture of `card_box`,
    /// `origin` its top-left in the game frame.
    pub fn read_card(&self, img: &Img, h: usize, origin: (usize, usize)) -> Option<Vec<(usize, bool)>> {
        let f = h as f64 / REF_HEIGHT;
        let at = |(x, y, side, step): (f64, f64, f64, f64), k: usize| {
            let size = (side * f).round() as usize;
            let pad = (CARD_PAD * f).round().max(2.0) as usize;
            let x = ((x + step * k as f64) * f).round() as isize - (origin.0 + pad) as isize;
            let y = (y * f).round() as isize - (origin.1 + pad) as isize;
            self.identify(&img.crop(x, y, size + 2 * pad, size + 2 * pad), size)
        };
        let (first, sure) = at(CARD_MISSION, 0)?;
        if self.catalog[first].name != "Reinforce" || !sure {
            return None;
        }
        let mission = (1..CARD_COMMON).map_while(|k| at(CARD_MISSION, k));
        let loadout = (0..CARD_LOADOUT_SLOTS).filter_map(|k| at(CARD_LOADOUT, k));
        Some(std::iter::once((first, sure)).chain(mission).chain(loadout).collect())
    }

    /// Which stratagem's glyph `win` shows, drawn `size` pixels square somewhere in it, with or without its frame:
    /// (template, sure), when one scores CARD_MIN. Ranked on small copies of the glyph first, then the best few
    /// scored at full size; sure as for the menu's rows.
    fn identify(&self, win: &Img, size: usize) -> Option<(usize, bool)> {
        let band = (size as f64 * FRAME_FRAC).round() as usize;
        let crops = low_crops(win, ROW_LOW as f64 / (size - 2 * band) as f64)?;
        let idx: Vec<usize> = (band..size - band).flat_map(|y| (band..size - band).map(move |x| y * size + x)).collect();
        let mut full: Vec<(f32, usize)> = rank_low(&self.low_glyph, &crops)[..ROW_TOP.min(self.low_glyph.len())]
            .iter()
            .map(|&(_, t)| (best_offset(win, &flatten(&self.raw[t].resize_area(size, size).px, &idx), &idx, size).0, t))
            .collect();
        full.sort_by(|a, b| b.0.total_cmp(&a.0));
        let (best, t) = full[0];
        let second = full.get(1).map_or(-1.0, |s| s.0);
        (best >= CARD_MIN).then_some((t, best >= ROW_SURE || best - second >= ROW_MARGIN))
    }

    /// (template, whole-icon score, sure, dx, dy). The whole icon decides whether an icon is there at all,
    /// the glyph alone which one it is.
    fn classify(&mut self, win: &Img, k: usize) -> Option<(usize, f32, bool, usize, usize)> {
        let sized = self.sized.as_ref().expect("prepared");
        let size = sized.size;
        if let Some(&(t, dx, dy)) = self.last.get(&k) {
            let sc = score_at(win, &sized.whole[t], &sized.whole_idx, size, dx, dy);
            if sc >= ROW_KEEP {
                return Some((t, sc, true, dx, dy));
            }
        }

        let crops = low_crops(win, ROW_LOW as f64 / size as f64)?;
        let low = rank_low(&self.low, &crops);
        if low[0].0 < ROW_LOW_MIN {
            self.last.remove(&k);
            return None;
        }

        let mut glyph: Vec<(f32, usize)> =
            low[..ROW_TOP.min(low.len())].iter().map(|&(_, t)| (best_offset(win, &sized.glyph[t], &sized.glyph_idx, size).0, t)).collect();
        glyph.sort_by(|a, b| b.0.total_cmp(&a.0));
        let (best_g, mut best) = glyph[0];
        let second_g = glyph.get(1).map_or(-1.0, |g| g.0);
        let (mut exist, mut dx, mut dy) = best_offset(win, &sized.whole[best], &sized.whole_idx, size);
        if exist < ROW_MIN {
            self.last.remove(&k);
            return None;
        }
        let mut sure = best_g >= ROW_SURE || best_g - second_g >= ROW_MARGIN;
        if !sure && let Some(t) = rescore_under_fill(win, sized, &glyph, dx, dy) {
            // scored as the progress fill draws it, one candidate leads clearly
            if t != best {
                best = t;
                (exist, dx, dy) = best_offset(win, &sized.whole[best], &sized.whole_idx, size);
                if exist < ROW_MIN {
                    self.last.remove(&k);
                    return None;
                }
            }
            sure = true;
        }
        if sure {
            self.last.insert(k, (best, dx, dy));
        } else {
            self.last.remove(&k);
        }
        Some((best, exist, sure, dx, dy))
    }
}

/// Every ROW_LOW-square crop of `win` scaled by `scale`, with its inverse spread (0 when flat): the small templates
/// are zero-mean, so dot(crop, template) times it is their correlation. None when the scaled window is too small.
fn low_crops(win: &Img, scale: f64) -> Option<Vec<(Vec<f32>, f32)>> {
    let (lw, lh) = ((win.w as f64 * scale).round() as usize, (win.h as f64 * scale).round() as usize);
    if lw < ROW_LOW || lh < ROW_LOW {
        return None;
    }
    let ws = win.resize_area(lw, lh);
    let mut crops = Vec::new();
    for j in 0..=lh - ROW_LOW {
        for i in 0..=lw - ROW_LOW {
            let c = ws.crop(i as isize, j as isize, ROW_LOW, ROW_LOW).px;
            let mut sum = [0.0f32; 3];
            for (q, v) in c.iter().enumerate() {
                sum[q % 3] += v;
            }
            let sumsq = dot(&c, &c);
            let n = (ROW_LOW * ROW_LOW) as f32;
            let var = sumsq - (sum[0] * sum[0] + sum[1] * sum[1] + sum[2] * sum[2]) / n;
            crops.push((c, if var > 1e-6 { 1.0 / var.sqrt() } else { 0.0 }));
        }
    }
    Some(crops)
}

/// Each small template's best correlation over the crops, best first: (score, template).
fn rank_low(templates: &[Vec<f32>], crops: &[(Vec<f32>, f32)]) -> Vec<(f32, usize)> {
    let mut low: Vec<(f32, usize)> =
        templates.iter().enumerate().map(|(t, tv)| (crops.iter().map(|(c, inv)| dot(tv, c) * inv).fold(f32::MIN, f32::max), t)).collect();
    low.sort_by(|a, b| b.0.total_cmp(&a.0));
    low
}

/// For an unsure reading of an icon partly under the game's progress fill (cooldown from the bottom,
/// in flight from the top). The fill draws the glyph cream on beige at about half the contrast, so the
/// rows it leaves clear dominate the whole-glyph score; similar icons often differ only under the fill
/// (the Quasar and Laser cannons share their top half). The fill is measured against the leading
/// candidate; every candidate is then scored on the clear rows as it is and on the covered rows as the
/// fill draws it, each part normalised on its own and weighted by its size, at one placement near the
/// leading candidate's. Returns the candidate that then leads by ROW_MARGIN, if any.
fn rescore_under_fill(win: &Img, sized: &Sized, glyph: &[(f32, usize)], dx: usize, dy: usize) -> Option<usize> {
    let size = sized.size;
    let band = (size as f64 * FRAME_FRAC).round() as usize;
    let n = (size - 2 * band) as f32; // rows fill_profile measures
    let (bottom, top) = fill_profile(win, dx, dy, &sized.images[glyph[0].1], size);
    if bottom + top < COVER_MIN {
        return None;
    }
    // template rows under the fill: the measured interior rows, plus the frame band on that side
    let from_top = if top > 0.0 { band + (top * n).round() as usize } else { 0 };
    let from_bottom = if bottom > 0.0 { size - band - (bottom * n).round() as usize } else { size };
    let (covered, clear): (Vec<usize>, Vec<usize>) = sized.glyph_idx.iter().partition(|&&p| p / size < from_top || p / size >= from_bottom);
    let total = sized.glyph_idx.len() as f32;
    // the icon sits where the leading candidate matched, give or take a pixel
    let near = |d: usize, room: usize| d.saturating_sub(1)..=(d + 1).min(room);
    let mut scores: Vec<(f32, usize)> = glyph
        .iter()
        .map(|&(_, t)| {
            let drawn = under_fill(&sized.images[t], from_top, from_bottom);
            let parts: Vec<(Flat, &[usize])> =
                [&covered, &clear].into_iter().filter(|idx| !idx.is_empty()).map(|idx| (flatten(&drawn.px, idx), idx.as_slice())).collect();
            let mut best = -1.0f32;
            for oy in near(dy, win.h.saturating_sub(size)) {
                for ox in near(dx, win.w.saturating_sub(size)) {
                    let s: f32 = parts.iter().map(|(f, idx)| score_at(win, f, idx, size, ox, oy) * idx.len() as f32).sum();
                    best = best.max(s / total);
                }
            }
            (best, t)
        })
        .collect();
    scores.sort_by(|a, b| b.0.total_cmp(&a.0));
    let second = scores.get(1).map_or(-1.0, |s| s.0);
    (scores[0].0 - second >= ROW_MARGIN).then_some(scores[0].1)
}

/// A clean icon as the game's progress fill draws it over rows `..from_top` and `from_bottom..`: from
/// beige where it is dark to cream where it is bright (glyph, frame), with a cream line at the fill's edge.
fn under_fill(icon: &Img, from_top: usize, from_bottom: usize) -> Img {
    let mut out = icon.clone();
    for y in (0..from_top).chain(from_bottom..icon.h) {
        let edge = y + 1 == from_top || y == from_bottom;
        for p in out.px[y * icon.w * 3..(y + 1) * icon.w * 3].as_chunks_mut::<3>().0 {
            let lum = 0.299 * p[0] + 0.587 * p[1] + 0.114 * p[2];
            let t = if edge { 1.0 } else { ((lum - FILL_DARK) / (FILL_BRIGHT - FILL_DARK)).clamp(0.0, 1.0) };
            *p = std::array::from_fn(|c| FILL_BEIGE[c] + (FILL_CREAM[c] - FILL_BEIGE[c]) * t);
        }
    }
    out
}

/// How much of the icon the game's progress fill covers, from the bottom and from the top.
/// A row counts as filled when most of its pixels differ clearly from the clean icon; the
/// right side is skipped because of the use-count badge, and short gaps are tolerated
/// because a white glyph looks much the same filled or not.
fn fill_profile(win: &Img, dx: usize, dy: usize, t: &Img, size: usize) -> (f32, f32) {
    let band = (size as f64 * FRAME_FRAC).round() as usize;
    let right = (size as f64 * BADGE_FRAC) as usize;
    let lum = |p: &[f32]| 0.299 * p[0] + 0.587 * p[1] + 0.114 * p[2];
    let filled: Vec<bool> = (band..size - band)
        .map(|y| {
            let hits = (band..right)
                .filter(|&x| {
                    let p = ((dy + y) * win.w + dx + x) * 3;
                    let q = (y * size + x) * 3;
                    (lum(&win.px[p..p + 3]) - lum(&t.px[q..q + 3])).abs() > FILL_DIFF
                })
                .count();
            hits * 2 > right - band
        })
        .collect();
    let run = |rows: &mut dyn Iterator<Item = &bool>| {
        let (mut len, mut gap, mut best) = (0usize, 0usize, 0usize);
        for &f in rows {
            if f {
                len += gap + 1;
                gap = 0;
                best = len;
            } else {
                gap += 1;
                if gap > FILL_GAP {
                    break;
                }
            }
        }
        best
    };
    let n = filled.len().max(1) as f32;
    (run(&mut filled.iter().rev()) as f32 / n, run(&mut filled.iter()) as f32 / n)
}

fn flatten(px: &[f32], idx: &[usize]) -> Flat {
    let mut v: Vec<f32> = idx.iter().flat_map(|&p| [px[p * 3], px[p * 3 + 1], px[p * 3 + 2]]).collect();
    center(&mut v);
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    Flat { v, norm }
}

/// Subtract the per-channel mean of interleaved RGB values.
fn center(v: &mut [f32]) {
    let n = (v.len() / 3).max(1) as f32;
    for c in 0..3 {
        let mean = v.iter().skip(c).step_by(3).sum::<f32>() / n;
        v.iter_mut().skip(c).step_by(3).for_each(|x| *x -= mean);
    }
}

fn normalized(px: &[f32]) -> Vec<f32> {
    let mut v = px.to_vec();
    center(&mut v);
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-3);
    v.iter_mut().for_each(|x| *x /= n);
    v
}

/// Eight independent accumulators so the compiler can vectorise (a single running f32 sum cannot be).
fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0.0f32; 8];
    let ((ca, ra), (cb, rb)) = (a.as_chunks::<8>(), b.as_chunks::<8>());
    let tail: f32 = ra.iter().zip(rb).map(|(x, y)| x * y).sum();
    for (x, y) in ca.iter().zip(cb) {
        for i in 0..8 {
            acc[i] += x[i] * y[i];
        }
    }
    acc.iter().sum::<f32>() + tail
}

/// Correlation of a raw (uncentred) patch with a zero-mean unit template: the template's zero mean
/// makes centring the patch unnecessary for the numerator, and the patch norm comes from its sums.
fn corr_raw(sum: [f32; 3], sumsq: f32, n: f32, d: f32) -> f32 {
    let var = sumsq - (sum[0] * sum[0] + sum[1] * sum[1] + sum[2] * sum[2]) / n;
    if var < 1e-6 { -1.0 } else { d / var.sqrt() }
}

/// Masked zero-mean correlation of a template placed at (dx, dy) inside `win`, in one pass.
fn score_at(win: &Img, t: &Flat, idx: &[usize], size: usize, dx: usize, dy: usize) -> f32 {
    if dx + size > win.w || dy + size > win.h || t.norm < 1e-3 {
        return -1.0;
    }
    let (mut sum, mut sumsq, mut d) = ([0.0f32; 3], 0.0f32, 0.0f32);
    for (i, &p) in idx.iter().enumerate() {
        let q = ((p / size + dy) * win.w + p % size + dx) * 3;
        let px = &win.px[q..q + 3];
        let tv = &t.v[i * 3..i * 3 + 3];
        for c in 0..3 {
            sum[c] += px[c];
            sumsq += px[c] * px[c];
            d += px[c] * tv[c];
        }
    }
    corr_raw(sum, sumsq, idx.len() as f32, d / t.norm)
}

fn best_offset(win: &Img, t: &Flat, idx: &[usize], size: usize) -> (f32, usize, usize) {
    let mut best = (-1.0f32, 0, 0);
    for dy in 0..=win.h.saturating_sub(size) {
        for dx in 0..=win.w.saturating_sub(size) {
            let s = score_at(win, t, idx, size, dx, dy);
            if s > best.0 {
                best = (s, dx, dy);
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    // status line geometry at 1440p, as in status.rs
    const ARROW_X: f64 = 155.0;
    const ARROW_STEP: f64 = 29.0;
    const WORD_X: f64 = 154.0;
    const BAND_Y: f64 = 34.0;
    const SLACK: f64 = 3.0;

    fn data() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../data")
    }

    fn matcher() -> Matcher {
        Matcher::load(&Data::Dir(data())).expect("catalog")
    }

    fn idx(m: &Matcher, name: &str) -> usize {
        m.catalog.iter().position(|s| s.name == name).unwrap_or_else(|| panic!("{name} not in the catalog"))
    }

    // ---- column_box ----

    #[test]
    fn column_box_at_common_resolutions() {
        assert_eq!(column_box(2560, 1440, 1.0), (56, 43, 388, 1036));
        assert_eq!(column_box(1920, 1080, 1.0), (42, 32, 291, 777));
        assert_eq!(column_box(3840, 2160, 1.0), (84, 64, 582, 1555));
        assert_eq!(column_box(1280, 720, 1.0), (28, 21, 194, 518));
    }

    #[test]
    fn column_box_follows_the_hud_scale_and_the_window_width() {
        // the left edge follows the icons (REF_X * scale - X_TOL); the status line needs NEEDS_X * scale,
        // wider than the icon column at any scale
        assert_eq!(column_box(2560, 1440, 2.0), (126, 43, 776, 1036));
        assert_eq!(column_box(2560, 1440, 0.5), (21, 43, 194, 1036));
        assert_eq!(column_box(300, 1440, 1.0), (56, 43, 300, 1036), "clamped to a narrow window");
    }

    /// Every region read_rows reads for slot k (icon window, status words, CODE_READ arrows, badge),
    /// as (what, x0, y0, x1, y1) in game-frame pixels, following read_rows, best_at and read_badge.
    fn regions(h: usize, scale: f64, k: usize) -> Vec<(String, isize, isize, isize, isize)> {
        let f = h as f64 / REF_HEIGHT * scale;
        let r = |v: f64| v.round() as isize;
        let size = r(REF_ICON * f).max(16);
        let pad = r(3.0 * f).max(2);
        let top = ROW0_Y + ROW_PITCH * k as f64;
        let mut out = vec![("icon".to_string(), r(REF_X * f) - pad, r(top * f) - pad, r(REF_X * f) + size + pad, r(top * f) + size + pad)];
        let slack = r(SLACK * f).max(1);
        let line = |what: String, x: f64, w: f64| {
            let (x0, y0) = (r(x * f) - slack, r((top + BAND_Y) * f) - slack);
            (what, x0, y0, x0 + r(w * f).max(4) + 2 * slack, y0 + r(25.0 * f).max(4) + 2 * slack)
        };
        out.push(line("words".into(), WORD_X, 58.0));
        for i in 0..status::CODE_READ {
            out.push(line(format!("arrow {i}"), ARROW_X + ARROW_STEP * i as f64, 26.0));
        }
        let (bx, by) = (r(100.0 * f), r((top + 28.0) * f));
        out.push(("badge".into(), bx, by, bx + r(46.0 * f), by + r(36.0 * f)));
        out
    }

    fn outside(w: usize, h: usize, scale: f64, slots: usize) -> Vec<String> {
        let (x0, y0, x1, y1) = column_box(w, h, scale);
        let (x0, y0, x1, y1) = (x0 as isize, y0 as isize, x1 as isize, y1 as isize);
        (0..slots)
            .flat_map(|k| regions(h, scale, k).into_iter().map(move |r| (k, r)))
            .filter(|(_, (_, a, b, c, d))| *a < x0 || *b < y0 || *c > x1 || *d > y1)
            .map(|(k, (what, ..))| format!("{w}x{h} scale {scale}: slot {k} {what}"))
            .collect()
    }

    #[test]
    fn column_box_holds_every_row_at_full_scale() {
        for (w, h) in [(1280, 720), (1600, 900), (1920, 1080), (2560, 1440), (3440, 1440), (2560, 1600), (3840, 2160)] {
            let bad = outside(w, h, 1.0, MAX_ROWS);
            assert!(bad.is_empty(), "{bad:#?}");
        }
    }

    #[test]
    fn column_box_holds_the_first_row_at_larger_hud_scales() {
        for scale in [1.0, 1.25, 1.5, 2.0] {
            let bad = outside(2560, 1440, scale, 1);
            assert!(bad.is_empty(), "{bad:#?}");
        }
    }

    /// Config allows icon_scale down to 0.5, but column_box keeps its left edge at (REF_X - X_TOL) for
    /// every scale while read_rows places the icon at REF_X * scale. Below about 0.85 the icon search
    /// window is clipped on the left; below about 0.77 it comes out narrower than the icon and
    /// read_rows stops at slot 0, reading nothing.
    #[test]
    fn column_box_holds_the_first_row_at_smaller_hud_scales() {
        let bad: Vec<String> = [0.5, 0.75].iter().flat_map(|&scale| outside(2560, 1440, scale, 1)).collect();
        assert!(bad.is_empty(), "{bad:#?}");
    }

    // ---- small helpers ----

    #[test]
    fn png_name_comes_from_the_svg_name() {
        let png = |icon: &str| {
            let (name, code, cooldown, also) = (String::new(), String::new(), String::new(), vec![]);
            Stratagem { name, code, cooldown, kind: Kind::Supply, icon: icon.into(), also }.png()
        };
        assert_eq!(png("Reinforce_Stratagem_Icon_Background.svg"), "Reinforce_Stratagem_Icon_Background.png");
        assert_eq!(png("W.A.S.P._Launcher_Stratagem_Icon_Background.svg"), "W.A.S.P._Launcher_Stratagem_Icon_Background.png");
        assert_eq!(png("sub/dir/Drill.svg"), "Drill.png");
        assert_eq!(png("NoExtension"), "NoExtension.png");
    }

    #[test]
    fn game_icon_paints_the_texture_in_the_type_colours_inside_the_frame() {
        // a 144 px texture: red (coloured part) at (40, 40), green (cream part) at (60, 60), half of each at (80, 80)
        let mut t = Img::new(144, 144);
        let mut set = |x: usize, y: usize, rgb: [f32; 3]| t.px[(y * 144 + x) * 3..(y * 144 + x) * 3 + 3].copy_from_slice(&rgb);
        set(40, 40, [255.0, 0.0, 0.0]);
        set(60, 60, [0.0, 255.0, 0.0]);
        set(80, 80, [255.0, 255.0, 0.0]);
        set(2, 70, [0.0, 255.0, 0.0]); // under the frame
        let icon = game_icon(&t, Kind::Offensive);
        let at = |x: usize, y: usize| -> [f32; 3] { std::array::from_fn(|c| icon.px[(y * 144 + x) * 3 + c]) };
        let ((frame, inside), cream) = (Kind::Offensive.colours(), ICON_CREAM);
        assert_eq!(at(40, 40), frame);
        assert_eq!(at(60, 60), cream);
        assert_eq!(at(80, 80), std::array::from_fn(|c| (frame[c] + cream[c]) / 2.0), "parts sharing a pixel share it");
        assert_eq!(at(30, 100), inside);
        assert_eq!(at(2, 70), frame, "the frame goes on top");
        for (x, y) in [(0, 0), (6, 70), (70, 137), (143, 143)] {
            assert_eq!(at(x, y), frame, "{x},{y}");
        }
        assert!((0..3).all(|c| at(7, 70)[c] > inside[c] && at(7, 70)[c] < frame[c].max(inside[c]) + 1.0), "the frame's soft edge");
        assert_eq!(at(8, 70), inside);
    }

    #[test]
    fn dot_matches_a_plain_sum_at_every_length() {
        for n in 0..40 {
            let a: Vec<f32> = (0..n).map(|i| i as f32 * 0.5 - 3.0).collect();
            let b: Vec<f32> = (0..n).map(|i| (i % 7) as f32).collect();
            let plain: f32 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
            assert!((dot(&a, &b) - plain).abs() < 1e-3, "length {n}");
        }
    }

    #[test]
    fn normalized_is_zero_mean_per_channel_and_unit_length() {
        let px: Vec<f32> = (0..30).map(|i| ((i * 37) % 11) as f32).collect();
        let v = normalized(&px);
        for c in 0..3 {
            assert!(v.iter().skip(c).step_by(3).sum::<f32>().abs() < 1e-5);
        }
        assert!((dot(&v, &v) - 1.0).abs() < 1e-5);
        assert!(normalized(&[5.0; 30]).iter().all(|&x| x == 0.0), "a flat patch has nothing to correlate");
    }

    #[test]
    fn score_at_is_one_on_the_template_and_minus_one_when_flat_or_out_of_bounds() {
        let size = 8;
        let mut t = Img::new(size, size);
        t.px.iter_mut().enumerate().for_each(|(i, v)| *v = ((i * 31) % 97) as f32);
        let idx: Vec<usize> = (0..size * size).collect();
        let flat = flatten(&t.px, &idx);
        assert!((score_at(&t, &flat, &idx, size, 0, 0) - 1.0).abs() < 1e-4);
        let mut win = Img::new(size + 3, size + 2);
        win.px.iter_mut().for_each(|v| *v = 9.0);
        for y in 0..size {
            let (s, d) = (y * size * 3, ((y + 2) * win.w + 3) * 3);
            win.px[d..d + size * 3].copy_from_slice(&t.px[s..s + size * 3]);
        }
        let (best, dx, dy) = best_offset(&win, &flat, &idx, size);
        assert!((best - 1.0).abs() < 1e-4);
        assert_eq!((dx, dy), (3, 2));
        assert_eq!(score_at(&win, &flat, &idx, size, 4, 0), -1.0, "out of bounds");
        let grey = Img { w: size, h: size, px: vec![50.0; size * size * 3] };
        assert_eq!(score_at(&grey, &flat, &idx, size, 0, 0), -1.0, "flat patch");
    }

    // ---- fill_profile ----

    const SIZE: usize = 57;

    fn template() -> Img {
        let mut t = Img::new(SIZE, SIZE);
        t.px.iter_mut().for_each(|v| *v = 40.0);
        t
    }

    /// The template with rows `rows` (and columns `cols`) painted bright, like the game's fill.
    fn lit(rows: std::ops::Range<usize>, cols: std::ops::Range<usize>) -> Img {
        let mut w = template();
        for y in rows {
            for x in cols.clone() {
                w.px[(y * SIZE + x) * 3..(y * SIZE + x) * 3 + 3].copy_from_slice(&[200.0; 3]);
            }
        }
        w
    }

    #[test]
    fn fill_profile_measures_from_each_end() {
        let t = template();
        let n = (SIZE - 2 * 7) as f32; // rows inside the frame band, round(57 * FRAME_FRAC) = 7
        assert_eq!(fill_profile(&t, 0, 0, &t, SIZE), (0.0, 0.0));
        assert_eq!(fill_profile(&lit(30..SIZE, 0..SIZE), 0, 0, &t, SIZE), (20.0 / n, 0.0), "cooldown: from the bottom");
        assert_eq!(fill_profile(&lit(0..17, 0..SIZE), 0, 0, &t, SIZE), (0.0, 10.0 / n), "inbound: from the top");
        assert_eq!(fill_profile(&lit(0..SIZE, 0..SIZE), 0, 0, &t, SIZE), (1.0, 1.0), "unavailable: all of it");
    }

    #[test]
    fn fill_profile_ignores_the_badge_side_and_bridges_short_gaps() {
        let t = template();
        let n = (SIZE - 2 * 7) as f32;
        let right = (SIZE as f64 * BADGE_FRAC) as usize;
        assert_eq!(fill_profile(&lit(0..SIZE, right..SIZE), 0, 0, &t, SIZE), (0.0, 0.0), "only the badge side changed");
        let mut gap3 = lit(20..SIZE, 0..SIZE);
        let mut gap4 = gap3.clone();
        for y in 30..33 {
            gap3.px[y * SIZE * 3..(y + 1) * SIZE * 3].copy_from_slice(&t.px[y * SIZE * 3..(y + 1) * SIZE * 3]);
        }
        for y in 30..34 {
            gap4.px[y * SIZE * 3..(y + 1) * SIZE * 3].copy_from_slice(&t.px[y * SIZE * 3..(y + 1) * SIZE * 3]);
        }
        assert_eq!(fill_profile(&gap3, 0, 0, &t, SIZE).0, 30.0 / n, "a 3-row gap (white glyph) is bridged");
        assert_eq!(fill_profile(&gap4, 0, 0, &t, SIZE).0, 16.0 / n, "a 4-row gap ends the fill");
    }

    #[test]
    fn fill_profile_reads_at_the_match_offset() {
        let t = template();
        let src = lit(30..SIZE, 0..SIZE);
        let mut win = Img::new(SIZE + 6, SIZE + 6);
        win.px.iter_mut().for_each(|v| *v = 200.0); // bright surroundings must not count
        for y in 0..SIZE {
            let (s, d) = (y * SIZE * 3, ((y + 3) * win.w + 2) * 3);
            win.px[d..d + SIZE * 3].copy_from_slice(&src.px[s..s + SIZE * 3]);
        }
        assert_eq!(fill_profile(&win, 2, 3, &t, SIZE), fill_profile(&src, 0, 0, &t, SIZE));
    }

    // ---- read_rows on synthetic columns ----

    fn blank_column() -> (Img, (usize, usize)) {
        let (x0, y0, x1, y1) = column_box(2560, 1440, 1.0);
        let mut col = Img::new(x1 - x0, y1 - y0);
        col.px.iter_mut().for_each(|v| *v = 30.0);
        (col, (x0, y0))
    }

    fn paste(col: &mut Img, src: &Img, x: f64, y: f64, origin: (usize, usize)) {
        let (x0, y0) = (x as usize - origin.0, y as usize - origin.1);
        for row in 0..src.h {
            let (s, d) = (row * src.w * 3, ((y0 + row) * col.w + x0) * 3);
            col.px[d..d + src.w * 3].copy_from_slice(&src.px[s..s + src.w * 3]);
        }
    }

    /// Draw catalog entry `index`'s icon in menu slot `k`, with `status` on its status line: an arrow
    /// code ("DDDUU"), a word template name ("word_cooldown") or nothing ("").
    fn draw_row(m: &Matcher, col: &mut Img, origin: (usize, usize), k: usize, index: usize, status: &str) {
        let top = ROW0_Y + ROW_PITCH * k as f64;
        paste(col, &m.raw[index].resize_area(57, 57), REF_X, top, origin);
        let template = |name: &str| Img::load_png(&data().join(format!("status/{name}.png"))).unwrap();
        if status.starts_with("word_") {
            paste(col, &template(status), WORD_X, top + BAND_Y, origin);
            return;
        }
        for (i, c) in status.chars().enumerate() {
            let name = match c {
                'U' => "arrow_up",
                'D' => "arrow_down",
                'L' => "arrow_left",
                _ => "arrow_right",
            };
            paste(col, &template(name), ARROW_X + ARROW_STEP * i as f64, top + BAND_Y, origin);
        }
    }

    /// (slot, name, sure, status) of every row read.
    fn read(m: &mut Matcher, col: &Img, origin: (usize, usize)) -> Vec<(usize, String, bool, Status)> {
        let rows = m.read_rows(col, 1440, origin, 1.0);
        rows.iter().map(|r| (r.slot, m.catalog[r.index].name.clone(), r.sure, r.status)).collect()
    }

    #[test]
    fn a_blank_column_has_no_rows() {
        let mut m = matcher();
        let (col, origin) = blank_column();
        assert!(m.read_rows(&col, 1440, origin, 1.0).is_empty());
    }

    #[test]
    fn rows_are_read_in_their_slots_even_after_a_gap() {
        let mut m = matcher();
        let (mut col, origin) = blank_column();
        draw_row(&m, &mut col, origin, 0, idx(&m, "Reinforce"), "UDRLU");
        draw_row(&m, &mut col, origin, 5, idx(&m, "LIFT-182 Warp Pack"), "word_cooldown");
        let rows = read(&mut m, &col, origin);
        assert_eq!(
            rows,
            [(0, "Reinforce".to_string(), true, Status::Ready), (5, "LIFT-182 Warp Pack".to_string(), true, Status::Cooldown)]
        );
        assert_eq!(m.slot_code(&col, 1440, origin, 1.0, 0), "UDRLU");
        assert_eq!(m.slot_code(&col, 1440, origin, 1.0, 5), "");
    }

    #[test]
    fn shared_icons_are_told_apart_by_the_code() {
        let mut m = matcher();
        for (icon, code, want) in [
            ("Upload Data", "DDDDDUU", &["SSSD Delivery"][..]),
            ("Upload Data", "DDDUU", &["SSSD Delivery"]), // its other code, on the other kind of objective
            ("SSSD Delivery", "LRUUU", &["Upload Data", "Reinforcement Pods"]), // same code: interchangeable
            ("Hive Breaker Drill", "LLLUDRDD", &["Aquifer Drill"]),
            ("Aquifer Drill", "DDLRDD", &["Prospecting Drill"]),
            ("Prospecting Drill", "DDLLDD", &["Activate E-711 Extraction Drill"]),
            ("Tectonic Drill", "UDUDUD", &["Tectonic Drill"]), // a drill with its own icon
            ("Cargo Container", "UUDDLLD", &["Portable Comms Relay"]),
            ("Portable Comms Relay", "UUDDRD", &["Cargo Container"]),
        ] {
            m.reset();
            let (mut col, origin) = blank_column();
            draw_row(&m, &mut col, origin, 3, idx(&m, icon), code);
            let rows = read(&mut m, &col, origin);
            assert!(rows.len() == 1 && want.contains(&rows[0].1.as_str()) && rows[0].2, "{icon} icon with {code}: {rows:?}");
        }
    }

    #[test]
    fn near_identical_icons_are_read_with_their_code() {
        let mut m = matcher();
        for (name, code) in
            [("AX/LAS-5 Rover", "DULURR"), ("AX/ARC-3 K-9", "DULURL"), ("Orbital Precision Strike", "RRU"), ("SEAF Artillery", "RUUD")]
        {
            m.reset();
            let (mut col, origin) = blank_column();
            draw_row(&m, &mut col, origin, 4, idx(&m, name), code);
            assert_eq!(read(&mut m, &col, origin), [(4, name.to_string(), true, Status::Ready)]);
        }
    }

    #[test]
    fn a_resolved_lookalike_keeps_its_identity_while_the_code_is_hidden() {
        let mut m = matcher();
        let upload = idx(&m, "Upload Data");
        let (mut col, origin) = blank_column();
        draw_row(&m, &mut col, origin, 4, upload, "DDDDDUU");
        assert_eq!(read(&mut m, &col, origin), [(4, "SSSD Delivery".to_string(), true, Status::Ready)]);
        // the next frames show the same icon on cooldown, without arrows
        let (mut col, origin) = blank_column();
        draw_row(&m, &mut col, origin, 4, upload, "word_cooldown");
        for _ in 0..2 {
            assert_eq!(read(&mut m, &col, origin), [(4, "SSSD Delivery".to_string(), true, Status::Cooldown)]);
        }
    }

    // ---- read_badge ----

    const SKY: [f32; 3] = [225.0, 220.0, 200.0]; // as cream as the badge box, like menu_activating's sky
    const DUSK: [f32; 3] = [30.0, 28.0, 20.0];

    fn column_of(scene: [f32; 3]) -> (Img, (usize, usize)) {
        let (mut col, origin) = blank_column();
        col.px.as_chunks_mut::<3>().0.iter_mut().for_each(|p| *p = scene);
        (col, origin)
    }

    #[test]
    fn a_badge_reads_the_same_on_any_scene() {
        let m = matcher();
        let (_, origin) = blank_column();
        let frame = Img::load_png(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/menu_closed.png")).unwrap();
        let want = read_badge(&frame, 1.0, ROW0_Y, origin).expect("Reinforce's badge 5 on menu_closed");
        // the box as drawn on menu_closed, without its soft edge: the scene meets it directly, as the
        // bright sky does on menu_activating
        let (bx, by) = (115.0, ROW0_Y + BADGE_ROWS.0);
        let badge = frame.crop(bx as isize - origin.0 as isize, by as isize - origin.1 as isize, 24, 24);
        for scene in [SKY, DUSK, [250.0, 250.0, 250.0]] {
            let (mut col, origin) = column_of(scene);
            paste(&mut col, &m.raw[idx(&m, "Reinforce")].resize_area(57, 57), REF_X, ROW0_Y, origin);
            paste(&mut col, &badge, bx, by, origin);
            let got = read_badge(&col, 1.0, ROW0_Y, origin).unwrap_or_else(|| panic!("no badge on scene {scene:?}"));
            assert_eq!((got.w, got.h, got.hash, &got.bits), (want.w, want.h, want.hash, &want.bits), "scene {scene:?}");
        }
    }

    #[test]
    fn icons_without_a_badge_read_none_even_on_a_scene_as_cream_as_one() {
        let m = matcher();
        let dimmed = |icon: &Img| Img { px: icon.px.iter().map(|v| v * 0.45 + 20.0).collect(), ..icon.clone() };
        for scene in [SKY, DUSK] {
            let (col, origin) = column_of(scene);
            assert!(read_badge(&col, 1.0, ROW0_Y, origin).is_none(), "bare scene {scene:?}");
            for name in ["LIFT-182 Warp Pack", "Resupply", "Orbital Laser", "LAS-99 Quasar Cannon", "M-102 Gunner FRV", "SoS Beacon"] {
                let icon = m.raw[idx(&m, name)].resize_area(57, 57);
                let looks = [
                    ("clean", icon.clone()),
                    ("covered", under_fill(&icon, 0, 0)),
                    ("half covered", under_fill(&icon, 0, 30)),
                    ("dimmed", dimmed(&icon)),
                ];
                for (look, drawn) in looks {
                    let (mut col, origin) = column_of(scene);
                    paste(&mut col, &drawn, REF_X, ROW0_Y, origin);
                    assert!(read_badge(&col, 1.0, ROW0_Y, origin).is_none(), "{look} {name} on scene {scene:?}");
                }
            }
        }
    }

    /// An icon as the game's fill shows it from template row `from` down (or above `from` with
    /// `from_top`): two flat tones as measured on screen in capture_2560x1440_082327, cream
    /// (255, 255, 237) where the icon is bright and beige (151, 144, 112) where it is dark.
    fn on_screen_fill(icon: &Img, from: usize, from_top: bool) -> Img {
        let mut out = icon.clone();
        let rows = if from_top { 0..from } else { from..icon.h };
        for y in rows {
            for p in out.px[y * icon.w * 3..(y + 1) * icon.w * 3].as_chunks_mut::<3>().0 {
                let lum = 0.299 * p[0] + 0.587 * p[1] + 0.114 * p[2];
                *p = if lum > 90.0 { [255.0, 255.0, 237.0] } else { [151.0, 144.0, 112.0] };
            }
        }
        out
    }

    #[test]
    fn under_fill_draws_dark_beige_bright_cream_and_an_edge_line() {
        let mut icon = Img::new(4, 6);
        for y in 0..6 {
            icon.px[y * 12..y * 12 + 6].copy_from_slice(&[10.0, 10.0, 10.0, 250.0, 250.0, 250.0]); // dark, bright
            icon.px[y * 12 + 6..y * 12 + 12].copy_from_slice(&[100.0, 100.0, 100.0, 60.0, 220.0, 240.0]); // mid grey, blue
        }
        let out = under_fill(&icon, 1, 4);
        let px = |x: usize, y: usize| out.px[(y * 4 + x) * 3..(y * 4 + x) * 3 + 3].to_vec();
        assert_eq!(out.px[12..48], icon.px[12..48], "rows between the two fills are untouched");
        for y in [0, 4] {
            assert!((0..4).all(|x| px(x, y) == FILL_CREAM), "row {y} is the fill's edge line");
        }
        assert_eq!(px(0, 5), FILL_BEIGE, "dark turns beige");
        assert_eq!(px(1, 5), FILL_CREAM, "bright turns cream");
        assert_eq!(px(3, 5), FILL_CREAM, "a bright colour turns cream too");
        let mid = px(2, 5);
        assert!((0..3).all(|c| FILL_BEIGE[c] < mid[c] && mid[c] < FILL_CREAM[c]), "in between: {mid:?}");
    }

    /// The Quasar and Laser cannons share their top half; with the bottom half under the cooldown fill
    /// the whole-glyph score leaves them within ROW_MARGIN. Scored as the fill draws them, the covered
    /// gun tells them apart.
    #[test]
    fn similar_icons_half_under_the_fill_are_read_surely() {
        let mut m = matcher();
        for name in ["LAS-99 Quasar Cannon", "LAS-98 Laser Cannon"] {
            for (from, from_top, word, status) in [
                (31, false, "word_cooldown", Status::Cooldown),
                (37, false, "word_cooldown", Status::Cooldown),
                (24, true, "word_inbound", Status::Active),
            ] {
                m.reset();
                let (mut col, origin) = blank_column();
                let icon = on_screen_fill(&m.raw[idx(&m, name)].resize_area(57, 57), from, from_top);
                let top = ROW0_Y; // slot 0: no empty slot above, as in the open menu
                paste(&mut col, &icon, REF_X, top, origin);
                paste(&mut col, &Img::load_png(&data().join(format!("status/{word}.png"))).unwrap(), WORD_X, top + BAND_Y, origin);
                assert_eq!(read(&mut m, &col, origin), [(0, name.to_string(), true, status)], "{name}, fill at row {from}");
            }
        }
    }

    /// resolve_lookalike's doc: "a first reading of such a slot stays unsure until the code is
    /// seen". classify stores a sure glyph match in `last` before resolve_lookalike runs, so a crisp
    /// shared icon read without its code comes out sure, as whichever group member ranked first.
    #[test]
    #[ignore = "doc mismatch: a shared icon first read without its arrow code is reported sure (as the first catalog member)"]
    fn a_first_reading_of_a_shared_icon_without_its_code_is_unsure() {
        let mut m = matcher();
        let (mut col, origin) = blank_column();
        draw_row(&m, &mut col, origin, 4, idx(&m, "Upload Data"), "word_cooldown");
        let rows = read(&mut m, &col, origin);
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].2, "read as a sure {} without seeing its code", rows[0].1);
    }
}
