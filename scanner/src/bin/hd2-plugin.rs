//! Stream Deck plugin process: launched by Stream Deck, scans the game on a worker thread and
//! keeps the keys in sync with the stratagem loadout. Writes hd2-plugin.log next to the exe.
//! Its data (the stratagem list, the icons and the status pictures) is built into the exe: under the
//! Elgato Marketplace's DRM a plugin cannot read its own files.

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::Write as _;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use streamdeck_rs::{ImagePayload, LogMessagePayload, Message, MessageOut, StreamDeckSocket, Target, TitlePayload};
use tokio::sync::mpsc;

use hd_scanner::capture::{Source, set_dpi_aware};
use hd_scanner::data::Data;
use hd_scanner::engine::{self, Config, Engine, GameState, SlotView, Status};
use hd_scanner::img::Img;
use hd_scanner::matcher::{ICON_SIZE, Kind, Matcher};

const SLOT: &str = "com.xiaodoudou.helldivers2-stratagems.slot";
const SCAN: &str = "com.xiaodoudou.helldivers2-stratagems.rescan";
const CLEAR: &str = "com.xiaodoudou.helldivers2-stratagems.clear";
const SETUP: &str = "com.xiaodoudou.helldivers2-stratagems.setup";
// set at runtime too: Stream Deck only reloads manifest images when it restarts
const CLEAR_SVG: &str = include_str!("../../../tools/icon_src/clear.svg");
const STEAM_LAUNCH: &str = "steam://rungameid/553850"; // Helldivers 2's Steam app id

type Out = MessageOut<Value, Value, Value>;
type Loadout = Vec<Option<SlotView>>;

struct Key {
    action: String,
    slot: usize,
    show_name: bool,
    loadout: Vec<usize>, // a Stratagem Setup key's loadout stratagems (0: the first one chosen), in calling order
}

// A Stratagem Setup key calls SETUP_MIN to LOADOUT_SIZE of the player's loadout stratagems, picked by their place in the
// loadout rather than by menu slot (see Plugin::loadout_slot); a new one takes SETUP_DEFAULT (1-based: the first two).
// Between two of its stratagems it waits SETUP_GAP after the throw, so that the throw has ended when the menu opens again.
const SETUP_MIN: usize = 2;
const LOADOUT_SIZE: usize = 4;
const SETUP_DEFAULT: [usize; 2] = [1, 2];
const SETUP_GAP: Duration = Duration::from_millis(1000);
const SLOT_MAX: usize = 12; // the slots the settings panels offer

/// Where one of the player's loadout stratagems is in the menu, as far as it is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Place {
    At(usize), // this menu slot
    Waiting,   // not known yet: the menu has not been read up to it
    Missing,   // the loadout has fewer stratagems
}

/// What a key shows for one slot, as the picture cache knows it: stratagem, status, fill step, badge.
type IconKey = (usize, Status, u8, u64);

enum Event {
    Loadout(Loadout, Vec<usize>), // the menu slots, and the player's chosen loadout (see Engine::chosen)
    State(GameState), // whether the game can be read, and whether a mission is on
    Standby(bool),    // the game is not running: nothing is scanned until it starts

    Done { context: String, result: Result<(), String> },
}

#[derive(Clone)]
struct Stratagem {
    name: String,
    code: String,
    png: Vec<u8>,  // the picture its key shows: an 8-bit RGBA PNG, ICON_SIZE square
    mission: bool, // given by the game (gold: Reinforce, a mission's own...), never one a player chooses
}

/// The matcher's catalog with the picture each stratagem's key shows: its icon as the game draws it, made from the
/// game's own texture: the very icon the matcher compares the screen with.
fn catalog(m: &Matcher) -> Vec<Stratagem> {
    m.catalog
        .iter()
        .enumerate()
        .map(|(i, s)| Stratagem { name: s.name.clone(), code: s.code.clone(), png: png_rgba(m.icon(i)), mission: s.kind == Kind::Mission })
        .collect()
}

/// An opaque 8-bit RGBA PNG of `img`.
fn png_rgba(img: &Img) -> Vec<u8> {
    let px: Vec<u8> =
        img.px.as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255.0].map(|v| v.round().clamp(0.0, 255.0) as u8)).collect();
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, img.w as u32, img.h as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().and_then(|mut w| w.write_image_data(&px)).expect("encoding into memory");
    out
}

static LOG: OnceLock<(Mutex<std::fs::File>, Instant)> = OnceLock::new();

fn log(msg: impl AsRef<str>) {
    if let Some((file, start)) = LOG.get() {
        let mut f = file.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(f, "[{:>9.3}] {}", start.elapsed().as_secs_f64(), msg.as_ref());
    }
}

/// End any other copy still running (a previous launch Stream Deck lost track of, or one under the
/// plugin's earlier name hd-plugin.exe): the newest wins.
fn end_older_instances() {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::{GetCurrentProcessId, OpenProcess, PROCESS_TERMINATE, TerminateProcess};
    unsafe {
        let me = GetCurrentProcessId();
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return };
        let mut e = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut more = Process32FirstW(snap, &mut e).is_ok();
        while more {
            let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
            let name = String::from_utf16_lossy(&e.szExeFile[..len]);
            if e.th32ProcessID != me
                && (name.eq_ignore_ascii_case("hd2-plugin.exe") || name.eq_ignore_ascii_case("hd-plugin.exe"))
                && let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, e.th32ProcessID)
            {
                let _ = TerminateProcess(h, 0);
                let _ = CloseHandle(h);
                log(format!("ended older instance pid {}", e.th32ProcessID));
            }
            more = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
}

/// Log whether this process runs inside a Windows job and which UI restrictions that job imposes:
/// JOB_OBJECT_UILIMIT_HANDLES would hide other programs' windows (and the foreground window) from us.
fn log_job() {
    use windows::Win32::System::JobObjects::{
        IsProcessInJob, JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicUIRestrictions,
        JobObjectExtendedLimitInformation, QueryInformationJobObject,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;
    unsafe {
        let mut in_job = windows::core::BOOL(0);
        let _ = IsProcessInJob(GetCurrentProcess(), None, &mut in_job);
        if !in_job.as_bool() {
            log("not in a job");
            return;
        }
        let mut ui = JOBOBJECT_BASIC_UI_RESTRICTIONS::default();
        let ui_ok = QueryInformationJobObject(
            None,
            JobObjectBasicUIRestrictions,
            &mut ui as *mut _ as *mut _,
            size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
            None,
        );
        let mut ext = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        let ext_ok = QueryInformationJobObject(
            None,
            JobObjectExtendedLimitInformation,
            &mut ext as *mut _ as *mut _,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            None,
        );
        log(format!(
            "in a job: ui restrictions {:#06x} ({}), limit flags {:#010x} ({})",
            ui.UIRestrictionsClass.0,
            if ui_ok.is_ok() { "read" } else { "unreadable" },
            ext.BasicLimitInformation.LimitFlags.0,
            if ext_ok.is_ok() { "read" } else { "unreadable" }
        ));
    }
}

/// Stream Deck launches plugins with `-port N -pluginUUID X -registerEvent Y -info {...}`.
/// Parsed by hand: a strict parse of -info could reject fields from a newer Stream Deck.
fn launch_args() -> Option<(u16, String, String)> {
    let a: Vec<String> = std::env::args().collect();
    let get = |k: &str| a.iter().position(|x| x == k).and_then(|i| a.get(i + 1)).cloned();
    Some((get("-port")?.parse().ok()?, get("-pluginUUID")?, get("-registerEvent")?))
}

fn data_url(mime: &str, bytes: &[u8]) -> String {
    format!("data:{mime};base64,{}", B64.encode(bytes))
}

// standby (game not running): keys turn to these greys and stop animating
const IDLE_FRAME: &str = "#5c5c5c";
const IDLE_INNER: &str = "#1c1c1c";
const IDLE_INK: &str = "#9a9a9a";

/// Signal-acquiring tile: the fan of arcs fills in over `step` 0..3; all grey and lit in standby.
fn acquiring(step: usize, idle: bool) -> String {
    let (frame, frame_opacity, inner, ink) =
        if idle { (IDLE_FRAME, 1.0, IDLE_INNER, IDLE_INK) } else { ("#c9b269", 0.45, "#26241a", "#ffffee") };
    let step = if idle { 3 } else { step };
    let arc = |r: f64| {
        let d = r * std::f64::consts::FRAC_1_SQRT_2;
        format!("M{:.1} {:.1} A{r} {r} 0 0 1 {:.1} {:.1}", 72.0 - d, 96.0 - d, 72.0 + d, 96.0 - d)
    };
    let arcs: String = [20.0, 36.0, 52.0]
        .iter()
        .enumerate()
        .map(|(i, &r)| {
            format!(
                r##"<path d="{}" fill="none" stroke="{ink}" stroke-width="7" stroke-linecap="round" opacity="{}"/>"##,
                arc(r),
                if i < step { 1.0 } else { 0.2 }
            )
        })
        .collect();
    let svg = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 144 144"><rect width="144" height="144" fill="{frame}" fill-opacity="{frame_opacity}"/><rect x="11" y="11" width="122" height="122" fill="{inner}"/>{arcs}<circle cx="72" cy="96" r="7" fill="{ink}"/></svg>"##
    );
    data_url("image/svg+xml", svg.as_bytes())
}

/// Clear key, greyed out in standby.
fn clear_icon(idle: bool) -> String {
    let svg = if idle {
        CLEAR_SVG.replace("#dc6455", IDLE_FRAME).replace("#190301", IDLE_INNER).replace("#ffffee", IDLE_INK)
    } else {
        CLEAR_SVG.to_string()
    };
    data_url("image/svg+xml", svg.as_bytes())
}

/// Scan key in standby: a start button that launches the game.
fn start_icon() -> String {
    let ink = "#ffd23f";
    let svg = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 144 144"><rect width="144" height="144" fill="{ink}"/><rect x="11" y="11" width="122" height="122" fill="#363426"/><g transform="translate(72 72)" fill="{ink}" stroke="{ink}" stroke-width="6" stroke-linejoin="round"><circle r="38" fill="none"/><path d="M-11 -19 L22 0 L-11 19 Z"/></g></svg>"##
    );
    data_url("image/svg+xml", svg.as_bytes())
}

/// Scan key: the radar from imgs/scan with its sweep turned by `angle` degrees.
fn radar(angle: u32, ink: &str) -> String {
    let svg = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 144 144"><rect width="144" height="144" fill="{ink}"/><rect x="11" y="11" width="122" height="122" fill="#363426"/><g transform="translate(72 72)" fill="none" stroke="{ink}" stroke-width="6" stroke-linecap="round"><circle r="38"/><circle r="20" stroke-opacity=".5"/><g transform="rotate({angle})"><path d="M0 0 L38 0 A38 38 0 0 0 26.9 -26.9 Z" fill="{ink}" fill-opacity=".22" stroke="none"/><path d="M0 0 L26.9 -26.9"/></g><circle cx="-17" cy="15" r="5.5" fill="{ink}" stroke="none"/></g></svg>"##
    );
    data_url("image/svg+xml", svg.as_bytes())
}

fn blank() -> String {
    data_url(
        "image/svg+xml",
        br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 144 144"><rect width="144" height="144" fill="#000"/></svg>"##,
    )
}

const FILL: [f32; 3] = [139.0, 128.0, 112.0]; // the game's beige progress fill
const CREAM: [f32; 3] = [240.0, 232.0, 214.0]; // glyph and edge line inside the fill

/// The stratagem icon (`png`, as stored) with the game's progress overlay across the whole icon,
/// frame included: a beige fill with a cream glyph, rising from the bottom while cooling down, coming
/// down from the top while inbound, covering it all when unavailable. With nothing to draw, the icon
/// itself, as it is. None when it needs drawing and cannot be decoded.
fn icon_png<'a>(png: &'a [u8], view: &SlotView) -> Option<Cow<'a, [u8]>> {
    let covered = match view.status {
        Status::Unavailable => true,
        Status::Cooldown | Status::Active => view.fill > 0.0,
        Status::Ready | Status::Unknown => false,
    };
    if !covered && view.badge.is_none() {
        return Some(Cow::Borrowed(png));
    }
    let decoder = png::Decoder::new(std::io::Cursor::new(png));
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return Some(Cow::Borrowed(png));
    }
    let (w, h) = (info.width as usize, info.height as usize);
    let px = &mut buf[..info.buffer_size()];
    let n = ((view.fill * h as f32).round() as usize).min(h);
    let (rows, line) = match view.status {
        _ if !covered => (0..0, None),
        Status::Cooldown => (h - n..h, Some(h - n)),
        Status::Active if n < h => (0..n, Some(n)),
        _ => (0..h, None),
    };
    for y in rows {
        for x in 0..w {
            let p = &mut px[(y * w + x) * 4..(y * w + x) * 4 + 3];
            let lum = 0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32;
            let c = if lum > 90.0 { CREAM } else { FILL };
            (p[0], p[1], p[2]) = (c[0] as u8, c[1] as u8, c[2] as u8);
        }
    }
    if let Some(ly) = line {
        for y in ly.saturating_sub(1)..(ly + 1).min(h) {
            for x in 0..w {
                let p = &mut px[(y * w + x) * 4..(y * w + x) * 4 + 3];
                (p[0], p[1], p[2]) = (CREAM[0] as u8, CREAM[1] as u8, CREAM[2] as u8);
            }
        }
    }
    if let Some(b) = &view.badge {
        draw_badge(px, w, h, b);
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, info.width, info.height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().ok()?.write_image_data(px).ok()?;
    }
    Some(Cow::Owned(out))
}

/// What one place of a Stratagem Setup key shows: what its slot's own key would show.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Cell {
    Icon(IconKey),        // the stratagem, as its slot key draws it
    Waiting(usize, bool), // the slot not read yet: the signal of `acquiring`, at this step, greyed in standby
    Empty,                // nothing in that slot this mission: black
}

/// "#rrggbb" as RGB values.
fn rgb(hex: &str) -> [f32; 3] {
    std::array::from_fn(|c| u8::from_str_radix(&hex[1 + 2 * c..3 + 2 * c], 16).map_or(0.0, f32::from))
}

/// The picture of `acquiring` drawn `size` pixels square (from its 144-unit design, sampling each pixel 3x3 times),
/// for a place of a Stratagem Setup key.
fn acquiring_img(step: usize, idle: bool, size: usize) -> Img {
    let (frame, frame_opacity, inner, ink) =
        if idle { (rgb(IDLE_FRAME), 1.0, rgb(IDLE_INNER), rgb(IDLE_INK)) } else { (rgb("#c9b269"), 0.45, rgb("#26241a"), rgb("#ffffee")) };
    let step = if idle { 3 } else { step };
    let mix = |under: [f32; 3], over: [f32; 3], a: f32| -> [f32; 3] { std::array::from_fn(|c| under[c] * (1.0 - a) + over[c] * a) };
    let at = |x: f64, y: f64| -> [f32; 3] {
        let mut p = if (11.0..133.0).contains(&x) && (11.0..133.0).contains(&y) { inner } else { mix([0.0; 3], frame, frame_opacity) };
        let (dx, dy) = (x - 72.0, y - 96.0);
        let dist = dx.hypot(dy);
        for (i, r) in [20.0, 36.0, 52.0].into_iter().enumerate() {
            // a quarter circle over the dot, from 45 degrees left of the top to 45 degrees right, with round ends
            let d = r * std::f64::consts::FRAC_1_SQRT_2;
            let on_arc = -dy >= dx.abs() && (dist - r).abs() <= 3.5;
            let on_end = [(-d, -d), (d, -d)].iter().any(|&(ex, ey)| (dx - ex).hypot(dy - ey) <= 3.5);
            if on_arc || on_end {
                p = mix(p, ink, if i < step { 1.0 } else { 0.2 });
            }
        }
        if dist <= 7.0 { ink } else { p }
    };
    let mut img = Img::new(size, size);
    let unit = 144.0 / size as f64;
    for y in 0..size {
        for x in 0..size {
            let mut sum = [0.0f32; 3];
            for (i, j) in (0..3).flat_map(|i| (0..3).map(move |j| (i, j))) {
                let p = at((x as f64 + (i as f64 + 0.5) / 3.0) * unit, (y as f64 + (j as f64 + 0.5) / 3.0) * unit);
                (0..3).for_each(|c| sum[c] += p[c] / 9.0);
            }
            img.px[(y * size + x) * 3..(y * size + x) * 3 + 3].copy_from_slice(&sum);
        }
    }
    img
}

/// A Stratagem Setup key's picture: its slots as their own keys show them, side by side for two, in a square of four
/// places for three or four (the third centred under the first two). `cells` pairs each place with its stratagem's
/// picture as its slot key draws it, when it shows one.
fn setup_png(cells: &[(Cell, Option<Vec<u8>>)]) -> Vec<u8> {
    const CELL: usize = 68;
    let places: &[(usize, usize)] = match cells.len() {
        0..=2 => &[(2, 38), (74, 38)],
        3 => &[(2, 2), (74, 2), (38, 74)],
        _ => &[(2, 2), (74, 2), (2, 74), (74, 74)],
    };
    let mut key = Img::new(ICON_SIZE, ICON_SIZE);
    for (&(x, y), (cell, png)) in places.iter().zip(cells) {
        let cell = match cell {
            Cell::Icon(_) => png
                .as_deref()
                .and_then(|b| Img::decode_png(b).ok())
                .map(|i| i.resize_area(CELL, CELL))
                .unwrap_or_else(|| Img::new(CELL, CELL)),
            &Cell::Waiting(step, idle) => acquiring_img(step, idle, CELL),
            Cell::Empty => Img::new(CELL, CELL),
        };
        for row in 0..CELL {
            let to = ((y + row) * ICON_SIZE + x) * 3;
            key.px[to..to + CELL * 3].copy_from_slice(&cell.px[row * CELL * 3..(row + 1) * CELL * 3]);
        }
    }
    png_rgba(&key)
}

/// Whether a press on a slot showing `v` is let through: not while cooling down (unless close enough to its end,
/// see near_ready), unavailable, or already on its way.
fn callable(v: &SlotView, now: Instant) -> bool {
    !matches!(v.status, Status::Cooldown | Status::Unavailable | Status::Active) || near_ready(v, now)
}

const BADGE_BG: [f32; 3] = [232.0, 229.0, 206.0];
const BADGE_INK: [f32; 3] = [61.0, 60.0, 39.0];

const BADGE_PAD: (f32, f32) = (7.0, 5.0); // cream margin around the digits in the game, at 1440p

/// Redraw the game's count badge in the icon's bottom-right corner, at the in-game size relative to the icon (57 px
/// icon at 1440p). A number read from it is drawn with the glyphs at the key's size, cleanly; otherwise the captured
/// digits are blown up, smoothed by sampling each key pixel 3x3 times.
fn draw_badge(px: &mut [u8], w: usize, h: usize, b: &hd_scanner::engine::Badge) {
    let s = w as f32 / 57.0;
    let (sw, sh) = (b.w as f32 + 2.0 * BADGE_PAD.0, b.h as f32 + 2.0 * BADGE_PAD.1); // badge size in game pixels
    let (bw, bh) = (((sw * s).round() as usize).min(w), ((sh * s).round() as usize).min(h));
    let (ox, oy) = (w - bw, h - bh);
    // the digits' place inside the badge, in key pixels
    let (dx, dy) = ((BADGE_PAD.0 * s).round() as usize, (BADGE_PAD.1 * s).round() as usize);
    let (dw, dh) = (bw.saturating_sub(2 * dx), bh.saturating_sub(2 * dy));
    let clean = b.number.as_deref().map(|n| hd_scanner::glyphs::render(n, dw, dh));
    for ty in 0..bh {
        for tx in 0..bw {
            let t = match &clean {
                Some(ink) => {
                    let inside = (dx..dx + dw).contains(&tx) && (dy..dy + dh).contains(&ty);
                    if inside { ink[(ty - dy) * dw + tx - dx] } else { 0.0 }
                }
                None => {
                    let mut ink = 0.0f32;
                    for i in 0..3 {
                        for j in 0..3 {
                            let gx = (tx as f32 + (i as f32 + 0.5) / 3.0) / s - BADGE_PAD.0;
                            let gy = (ty as f32 + (j as f32 + 0.5) / 3.0) / s - BADGE_PAD.1;
                            if gx >= 0.0 && gy >= 0.0 && (gx as usize) < b.w && (gy as usize) < b.h {
                                ink += b.bits[gy as usize * b.w + gx as usize] as u8 as f32;
                            }
                        }
                    }
                    ink / 9.0
                }
            };
            let p = &mut px[((oy + ty) * w + ox + tx) * 4..((oy + ty) * w + ox + tx) * 4 + 4];
            for c in 0..3 {
                p[c] = (BADGE_BG[c] * (1.0 - t) + BADGE_INK[c] * t) as u8;
            }
            p[3] = 255;
        }
    }
}

/// The game size the plugin found, and how, for the Scan key's panel.
fn found_size((w, h): (usize, usize), source: Source) -> String {
    let how = match source {
        Source::Window | Source::Foreground => "from its window",
        Source::LastKnown => "where its window was last seen",
        Source::MainScreen => "the main screen's size: the window itself can't be seen",
    };
    format!("{w}x{h} ({how})")
}

fn set_image(context: &str, image: String) -> Out {
    MessageOut::SetImage { context: context.into(), payload: ImagePayload { image: Some(image), target: Target::Both, state: None } }
}

fn set_title(context: &str, title: &str) -> Out {
    MessageOut::SetTitle { context: context.into(), payload: TitlePayload { title: Some(title.into()), target: Target::Both, state: None } }
}

/// Whether a press on a cooling slot goes through at `now`: close enough to its end.
fn near_ready(v: &SlotView, now: Instant) -> bool {
    v.status == Status::Cooldown
        && v.ready.is_some_and(|r| r.at.saturating_duration_since(now) <= if r.timed { TIMER_SLACK } else { NEAR_READY })
}

fn describe(l: &Loadout, names: &[String]) -> String {
    l.iter()
        .enumerate()
        .map(|(k, v)| match v {
            None => format!("{}: ?", k + 1),
            Some(v) => format!("{}: {} {:?} {:.0}%", k + 1, names[v.index], v.status, v.fill * 100.0),
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

struct Plugin {
    keys: HashMap<String, Key>,
    loadout: Loadout,
    chosen: Vec<usize>, // the player's loadout as read from the card: the stratagem each slot holds, in the order chosen
    catalog: Vec<Stratagem>,
    icons: HashMap<IconKey, String>,
    setups: HashMap<Vec<Cell>, String>, // Stratagem Setup pictures, by what their places show
    frame: usize,                       // animation tick, every ANIM
    state: GameState,
    standby: bool, // the game is not running
}

const ANIM: Duration = Duration::from_millis(200);
const BURST: Duration = Duration::from_secs(4); // after a call-in, scan fast to catch the game's state change
const BURST_PERIOD: Duration = Duration::from_millis(50);
const MENU_SETTLE: Duration = Duration::from_secs(2); // after the HUD appears, before opening the menu
// A cooldown this close to its estimated end lets a press through anyway: the last sliver of the fill is
// too thin to measure (if it is not quite ready, the game ignores the code). With a timer read on screen
// the end is known, and only TIMER_SLACK of it is given away.
const NEAR_READY: Duration = Duration::from_millis(1500);
const TIMER_SLACK: Duration = Duration::from_millis(300);
// at mission start, the menu is tried every MENU_RETRY until it opens, MENU_TRIES times at most (about 20 s)
const MENU_RETRY: Duration = Duration::from_secs(2);
const MENU_TRIES: u32 = 10;
// A mission expected from the loadout screen (Engine::mission_expected) is tried at once and more often: the HUD
// showing Reinforce is the cue, and a press the game ignores during the landing costs nothing.
const MENU_SETTLE_EXPECTED: Duration = Duration::from_millis(300);
const MENU_RETRY_EXPECTED: Duration = Duration::from_secs(1);
const MENU_TRIES_EXPECTED: u32 = 20;
const MENU_OPEN_ROWS: usize = 4; // the open menu lists Reinforce, SoS Beacon and Resupply, plus the loadout
const GAME_POLL: Duration = Duration::from_secs(2); // how often to check that the game is running
const CARD_PERIOD: Duration = Duration::from_secs(1); // how often to look for the loadout screen before a mission

/// Lets a key press wake the scan thread and switch it to fast scans for a while.
#[derive(Default)]
struct Wake {
    until: Mutex<Option<Instant>>,
    cv: std::sync::Condvar,
    recheck: std::sync::atomic::AtomicBool, // look for the game process now, not at the next GAME_POLL
}

impl Wake {
    fn recheck(&self) {
        self.recheck.store(true, std::sync::atomic::Ordering::Relaxed);
        self.cv.notify_one();
    }

    fn take_recheck(&self) -> bool {
        self.recheck.swap(false, std::sync::atomic::Ordering::Relaxed)
    }

    fn burst(&self) {
        *self.until.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now() + BURST);
        self.cv.notify_one();
    }

    /// Sleep for `normal`, or BURST_PERIOD while a burst lasts; returns early when woken.
    fn sleep(&self, normal: Duration, spent: Duration) {
        let guard = self.until.lock().unwrap_or_else(|e| e.into_inner());
        let period = if guard.is_some_and(|u| Instant::now() < u) { BURST_PERIOD } else { normal };
        let wait = period.saturating_sub(spent).max(Duration::from_millis(10));
        let _ = self.cv.wait_timeout(guard, wait);
    }
}

impl Plugin {
    /// An unread slot inside the menu is still acquiring, and so is every slot until the open menu has
    /// been read once (it always lists Reinforce, SoS Beacon and Resupply before the loadout);
    /// past the last slot of a read menu it is blank.
    fn is_acquiring(&self, slot: usize) -> bool {
        self.loadout.get(slot).is_none_or(|v| v.is_none()) && (self.loadout.len() < 4 || slot < self.loadout.len())
    }

    fn paint(&mut self, context: &str) -> Vec<Out> {
        let Some(key) = self.keys.get(context) else { return vec![] };
        if key.action == CLEAR {
            return vec![set_image(context, clear_icon(self.standby))];
        }
        if key.action == SCAN {
            if self.standby {
                return vec![set_image(context, start_icon())];
            }
            // the sweep turns while the game is being read, and rests when it cannot be seen
            let angle = if self.state != GameState::Missing { (self.frame as u32 * 30) % 360 } else { 0 };
            let ink = match self.state {
                GameState::Mission => "#7be07b",
                GameState::Lobby => "#ffd23f",
                GameState::Missing => "#8a8a8a",
            };
            return vec![set_image(context, radar(angle, ink))];
        }
        if key.action == SETUP {
            let places: Vec<Place> = key.loadout.iter().map(|&k| self.loadout_slot(k)).collect();
            let views: Vec<Option<SlotView>> =
                places.iter().map(|p| if let Place::At(s) = *p { self.loadout.get(s).cloned().flatten() } else { None }).collect();
            let id: Vec<Cell> = places
                .iter()
                .zip(&views)
                .map(|(p, v)| match (p, v) {
                    (_, Some(v)) => Cell::Icon(self.icon_key(v)),
                    (Place::Missing, None) => Cell::Empty,
                    _ => Cell::Waiting(if self.standby { 3 } else { self.frame / 3 % 4 }, self.standby),
                })
                .collect();
            if let Some(image) = self.setups.get(&id) {
                return vec![set_image(context, image.clone())];
            }
            let cells: Vec<(Cell, Option<Vec<u8>>)> = id
                .iter()
                .zip(&views)
                .map(|(cell, v)| (cell.clone(), v.as_ref().and_then(|v| icon_png(&self.catalog[v.index].png, v).map(Cow::into_owned))))
                .collect();
            let image = data_url("image/png", &setup_png(&cells));
            self.setups.insert(id, image.clone());
            return vec![set_image(context, image)];
        }
        if key.action != SLOT {
            return vec![];
        }
        let (slot, show_name) = (key.slot, key.show_name);
        match self.loadout.get(slot).cloned().flatten() {
            Some(v) => {
                let id = self.icon_key(&v);
                let png = &self.catalog[v.index].png;
                let icon = self
                    .icons
                    .entry(id)
                    .or_insert_with(|| icon_png(png, &v).map(|b| data_url("image/png", &b)).unwrap_or_else(blank))
                    .clone();
                let title = if show_name { self.catalog[v.index].name.as_str() } else { "" };
                vec![set_image(context, icon), set_title(context, title)]
            }
            None => {
                let image = if self.is_acquiring(slot) { acquiring(self.frame / 3 % 4, self.standby) } else { blank() };
                vec![set_image(context, image), set_title(context, "")]
            }
        }
    }

    /// Where the player's loadout stratagem `k` (0: the first one chosen) is in the menu. The loadout stratagems sit
    /// together, in the order chosen, after any a mission gives, so once the first is found by what it is (the card
    /// says which stratagem that is), the k-th follows it. This holds even when a mission puts a stratagem among them
    /// that the reader can't place, or one a player could have chosen, which counting the menu would stumble on.
    /// Without a card (the plugin started inside a mission) it falls back to counting the stratagems a player chooses,
    /// in menu order, skipping the ones every mission gives. Waiting while the loadout has not been read yet.
    fn loadout_slot(&self, k: usize) -> Place {
        if let Some(&first) = self.chosen.first() {
            if k >= self.chosen.len() {
                return Place::Missing; // the loadout has fewer stratagems
            }
            return match self.loadout.iter().position(|v| v.as_ref().is_some_and(|s| s.index == first)) {
                Some(s0) => Place::At(s0 + k),
                None => Place::Waiting, // the first loadout stratagem is not on screen yet, so none is placed
            };
        }
        let mut n = 0;
        for (s, v) in self.loadout.iter().enumerate() {
            match v {
                None => return Place::Waiting,
                Some(v) if self.catalog[v.index].mission => {}
                Some(_) if n == k => return Place::At(s),
                Some(_) => n += 1,
            }
        }
        // the open menu lists every slot; before it has been read (the closed HUD shows Reinforce only), nothing is known
        if self.loadout.len() < MENU_OPEN_ROWS { Place::Waiting } else { Place::Missing }
    }

    fn icon_key(&self, v: &SlotView) -> IconKey {
        (v.index, v.status, (v.fill * 72.0).round() as u8, v.badge.as_ref().map_or(0, |b| b.hash))
    }

    fn paint_where(&mut self, keep: impl Fn(&Key) -> bool) -> Vec<Out> {
        let contexts: Vec<String> = self.keys.iter().filter(|(_, k)| keep(k)).map(|(c, _)| c.clone()).collect();
        contexts.iter().flat_map(|c| self.paint(c)).collect()
    }

    fn paint_slots(&mut self) -> Vec<Out> {
        self.paint_where(|k| k.action == SLOT || k.action == SETUP)
    }

    /// Settings of a key; a new slot key takes the first free slot and saves it.
    fn register(&mut self, action: &str, context: &str, settings: &Value) -> Vec<Out> {
        let mut out = Vec::new();
        let given = settings.get("slot").and_then(Value::as_u64).filter(|&n| n > 0).map(|n| n as usize - 1);
        let slot = match given {
            Some(s) => s,
            None if action == SLOT => {
                let used: Vec<usize> =
                    self.keys.iter().filter(|(c, k)| c.as_str() != context && k.action == SLOT).map(|(_, k)| k.slot).collect();
                let s = (0..).find(|s| !used.contains(s)).unwrap_or(0);
                let mut saved = settings.as_object().cloned().unwrap_or_default();
                saved.insert("slot".into(), json!(s + 1));
                out.push(MessageOut::SetSettings { context: context.into(), payload: Value::Object(saved) });
                s
            }
            None => 0,
        };
        let show_name = settings.get("show_name").and_then(Value::as_bool).unwrap_or(false);
        let mut loadout = Vec::new();
        if action == SETUP {
            // 1-based in the settings, each once, SETUP_MIN to LOADOUT_SIZE of them; anything else gets the default
            let given = settings.get("loadout").and_then(Value::as_array).cloned().unwrap_or_default();
            for n in given.iter().filter_map(Value::as_u64).map(|n| n as usize).filter(|n| (1..=LOADOUT_SIZE).contains(n)) {
                if !loadout.contains(&(n - 1)) {
                    loadout.push(n - 1);
                }
            }
            if loadout.len() < SETUP_MIN {
                loadout = SETUP_DEFAULT.iter().map(|n| n - 1).collect();
                let mut saved = settings.as_object().cloned().unwrap_or_default();
                saved.remove("slots"); // menu slots, as a build before 0.5.2 saved them
                saved.insert("loadout".into(), json!(SETUP_DEFAULT));
                out.push(MessageOut::SetSettings { context: context.into(), payload: Value::Object(saved) });
            }
        }
        self.keys.insert(context.into(), Key { action: action.into(), slot, show_name, loadout });
        out.extend(self.paint(context));
        out
    }
}

/// Scans on its own thread. Sends the loadout whenever what a key would show changes (including
/// cooldown progress drained by the clock while the menu is closed). Survives a scan that panics.
fn scan_thread(engine: Arc<Mutex<Engine>>, cfg: Arc<Mutex<Config>>, tx: mpsc::UnboundedSender<Event>, names: Vec<String>, wake: Arc<Wake>) {
    let mut last: Loadout = Vec::new();
    let (mut last_err, mut was_state): (Option<String>, Option<GameState>) = (None, None);
    let mut last_source = None;
    let (mut mission_since, mut menu_tries, mut menu_tried): (Option<Instant>, u32, Option<Instant>) = (None, 0, None);
    let mut expected = false; // the mission under way was expected from the loadout screen
    let mut menu_skip: Option<String> = None; // why the last mission-start menu read was refused, logged once
    let (mut beat, mut scans, mut focused) = (Instant::now(), 0u32, 0u32);
    let (mut running, mut looked): (Option<bool>, Option<Instant>) = (None, None);
    let mut card_read: Option<Instant> = None;
    loop {
        let c = cfg.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let t = Instant::now();
        // standby while the game is not running: no capture at all, only a process check now and then
        if wake.take_recheck() || looked.is_none_or(|l| l.elapsed() >= GAME_POLL) {
            looked = Some(t);
            let up = hd_scanner::capture::running(&c.process_name);
            if running != Some(up) {
                log(if up { "game running: scanning" } else { "game not running: standby" });
                if !up && running.is_some() {
                    // the loadout belonged to the game that just closed
                    let mut e = engine.lock().unwrap_or_else(|p| p.into_inner());
                    e.forget();
                    last = e.loadout();
                    let chosen = e.chosen();
                    drop(e);
                    if tx.send(Event::Loadout(last.clone(), chosen)).is_err() {
                        return;
                    }
                }
                running = Some(up);
                if tx.send(Event::Standby(!up)).is_err() {
                    return;
                }
            }
        }
        if running == Some(false) {
            wake.sleep(GAME_POLL, t.elapsed());
            continue;
        }
        let (l, chosen, ms, err, state, mission, coming) = {
            let mut e = engine.lock().unwrap_or_else(|p| p.into_inner());
            if c.auto_scan {
                if std::panic::catch_unwind(AssertUnwindSafe(|| e.scan(&c))).is_err() {
                    log("scan panicked; carrying on");
                }
                scans += 1;
                focused += e.focused as u32;
            }
            // before a mission, the player's card on the loadout screen tells the loadout: look for it now and then
            if c.auto_scan && e.game_state() == GameState::Lobby && card_read.is_none_or(|t: Instant| t.elapsed() >= CARD_PERIOD) {
                card_read = Some(Instant::now());
                match std::panic::catch_unwind(AssertUnwindSafe(|| e.read_card(&c))) {
                    Ok(Ok(true)) => log("read the loadout from the loadout screen"),
                    Ok(_) => {}
                    Err(_) => log("loadout screen read panicked; carrying on"),
                }
            }
            if e.source != last_source {
                log(format!("game position from {:?}", e.source));
                last_source = e.source;
            }
            (e.loadout(), e.chosen(), e.scan_ms, e.last_error.clone(), e.game_state(), e.mission_slots(), e.mission_expected())
        };
        if err != last_err {
            match &err {
                Some(msg) => log(format!("cannot read the game: {msg}")),
                None => log("reading the game"),
            }
            last_err = err;
        }
        if was_state != Some(state) {
            if state == GameState::Mission {
                (mission_since, menu_tries, menu_tried, menu_skip, expected) = (Some(Instant::now()), 0, None, None, coming);
            }
            log(format!(
                "game state {state:?}{}",
                if state == GameState::Mission && expected { " (expected from the loadout screen)" } else { "" }
            ));
            was_state = Some(state);
            if tx.send(Event::State(state)).is_err() {
                return;
            }
        }
        let (settle, retry, tries) =
            if expected { (MENU_SETTLE_EXPECTED, MENU_RETRY_EXPECTED, MENU_TRIES_EXPECTED) } else { (MENU_SETTLE, MENU_RETRY, MENU_TRIES) };
        // a mission has started and the loadout has not been read in it yet (one known from the loadout screen is
        // checked too): open the menu and read it. The HUD shows before the player can open the menu (hellpod,
        // landing), so a try that finds no open menu is retried soon, until the loadout is known, whoever opened it
        if c.auto_menu_scan
            && state == GameState::Mission
            && mission < MENU_OPEN_ROWS
            && menu_tries < tries
            && mission_since.is_some_and(|t| t.elapsed() >= settle)
            && menu_tried.is_none_or(|t: Instant| t.elapsed() >= retry)
        {
            menu_tried = Some(Instant::now());
            let mut e = engine.lock().unwrap_or_else(|p| p.into_inner());
            match e.scan_menu(&c) {
                Ok(_) => {
                    // only presses count as tries: a refused one (game not in front) sent nothing
                    menu_tries += 1;
                    // rows on screen before the menu key and after it: whether the menu really opened
                    let open = e.menu_open_ms.map_or(String::new(), |ms| format!(", open for {ms} ms"));
                    let rows = format!("rows before and after the menu key: {:?}{open}", e.menu_rows);
                    if e.mission_slots() >= MENU_OPEN_ROWS {
                        log(format!("mission start: read the loadout from the menu (try {menu_tries}; {rows})"));
                    } else if menu_tries < tries {
                        log(format!("mission start: the menu did not open (try {menu_tries}; {rows}), trying again"));
                    } else {
                        log(format!("mission start: the menu did not open after {menu_tries} tries ({rows}); open it once, or press Scan"));
                    }
                }
                Err(err) if menu_skip.as_ref() != Some(&err) => {
                    log(format!("mission start: menu read skipped: {err}"));
                    menu_skip = Some(err);
                }
                Err(_) => {}
            }
            let fresh = e.loadout();
            let chosen = e.chosen();
            drop(e);
            if tx.send(Event::Loadout(fresh, chosen)).is_err() {
                return;
            }
            continue; // `l` above predates the menu read
        }
        if l != last {
            log(format!("loadout {}", describe(&l, &names)));
            if tx.send(Event::Loadout(l.clone(), chosen.clone())).is_err() {
                log("plugin loop gone; scan thread stops");
                return;
            }
            last = l;
        }
        if beat.elapsed() >= Duration::from_secs(10) {
            log(format!("heartbeat: {scans} scans, {focused} with the game window visible, last scan {ms:.1} ms"));
            (beat, scans, focused) = (Instant::now(), 0, 0);
        }
        wake.sleep(Duration::from_secs_f64(c.scan_period), t.elapsed());
    }
}

/// A development aid for demo videos: with a file named `record-keys` beside the exe, every image and title sent to a
/// key, and every key press, goes to `keys-record.jsonl` there, one JSON object a line with the time (Unix
/// milliseconds) and the key's place, so that a screen recording of the game can be shown with the keys as they were.
struct KeyRecord {
    file: Option<std::fs::File>,
    places: HashMap<String, (u8, u8)>, // context -> (column, row)
}

impl KeyRecord {
    fn open(dir: &std::path::Path) -> KeyRecord {
        let file = dir.join("record-keys").exists().then(|| std::fs::File::create(dir.join("keys-record.jsonl")).ok()).flatten();
        if file.is_some() {
            log("recording the keys to keys-record.jsonl");
        }
        KeyRecord { file, places: HashMap::new() }
    }

    fn place(&mut self, context: &str, at: Option<&streamdeck_rs::Coordinates>) {
        if let Some(c) = at {
            self.places.insert(context.into(), (c.column, c.row));
        }
    }

    fn write(&mut self, what: &str, context: &str, value: &str) {
        let Some(f) = self.file.as_mut() else { return };
        let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64() * 1000.0);
        let at = self.places.get(context).copied();
        let _ = writeln!(f, "{}", json!({ "t": ms, "what": what, "key": at, "value": value }));
    }

    fn sent(&mut self, m: &Out) {
        match m {
            MessageOut::SetImage { context, payload } => self.write("image", context, payload.image.as_deref().unwrap_or("")),
            MessageOut::SetTitle { context, payload } => self.write("title", context, payload.title.as_deref().unwrap_or("")),
            _ => {}
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    set_dpi_aware();
    let dir = std::env::current_exe().ok().and_then(|p| p.parent().map(PathBuf::from)).unwrap_or_default();
    let exe_path = dir.join("hd2-plugin.exe");
    let exe_built = std::fs::metadata(&exe_path).and_then(|m| m.modified()).ok();
    // a redeploy or crash restarts the plugin: keep the previous run's log for diagnosis
    let _ = std::fs::rename(dir.join("hd2-plugin.log"), dir.join("hd2-plugin.prev.log"));
    if let Ok(f) = std::fs::File::create(dir.join("hd2-plugin.log")) {
        let _ = LOG.set((Mutex::new(f), Instant::now()));
    }
    std::panic::set_hook(Box::new(|info| log(format!("panic: {info}"))));
    let Some((port, uuid, event)) = launch_args() else {
        eprintln!("hd2-plugin is started by Stream Deck (expects -port, -pluginUUID, -registerEvent)");
        std::process::exit(2);
    };
    log(format!("start pid {} port {port}", std::process::id()));
    log_job();
    end_older_instances();
    // built in: under the Marketplace's DRM the plugin cannot read its own files
    let data = Data::Embedded;

    let socket = match StreamDeckSocket::<Value, Value, Value, Value>::connect(port, event, uuid.clone()).await {
        Ok(s) => s,
        Err(e) => {
            log(format!("cannot connect to Stream Deck: {e}"));
            std::process::exit(1);
        }
    };
    let (mut sink, mut stream) = socket.split();

    let engine = match Engine::new(&data) {
        Ok(e) => Arc::new(Mutex::new(e)),
        Err(e) => {
            log(format!("cannot load data: {e}"));
            let _ = sink.send(MessageOut::LogMessage { payload: LogMessagePayload { message: format!("hd2-plugin: {e}") } }).await;
            std::process::exit(1);
        }
    };
    let catalog = catalog(&engine.lock().unwrap().matcher);
    let names: Vec<String> = catalog.iter().map(|s| s.name.clone()).collect();
    log(format!("loaded {} stratagems", catalog.len()));
    let cfg = Arc::new(Mutex::new(Config::default()));
    let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
    let wake = Arc::new(Wake::default());
    {
        let (engine, cfg, tx, wake) = (engine.clone(), cfg.clone(), tx.clone(), wake.clone());
        std::thread::spawn(move || scan_thread(engine, cfg, tx, names, wake));
    }

    let _ = sink.send(MessageOut::GetGlobalSettings { context: uuid.clone() }).await;
    let mut record = KeyRecord::open(&dir);
    let mut p = Plugin {
        keys: HashMap::new(),
        loadout: Vec::new(),
        chosen: Vec::new(),
        catalog,
        icons: HashMap::new(),
        setups: HashMap::new(),
        frame: 0,
        state: GameState::Missing,
        standby: false,
    };
    let mut anim = tokio::time::interval(ANIM);

    loop {
        let out: Vec<Out> = tokio::select! {
            msg = stream.next() => match msg {
                None => {
                    log("Stream Deck closed the connection; exiting");
                    break;
                }
                Some(Err(e)) => {
                    log(format!("bad message: {e}"));
                    continue;
                }
                Some(Ok(m)) => match m {
                    Message::WillAppear { action, context, payload, .. } => {
                        record.place(&context, payload.coordinates.as_ref());
                        p.register(&action, &context, &payload.settings)
                    }
                    Message::DidReceiveSettings { action, context, payload, .. } => p.register(&action, &context, &payload.settings),
                    Message::WillDisappear { context, .. } => {
                        p.keys.remove(&context);
                        vec![]
                    }
                    Message::DidReceiveGlobalSettings { payload } => {
                        *cfg.lock().unwrap() = Config::from_json(&payload.settings);
                        log(format!("settings: {:?}", cfg.lock().unwrap()));
                        vec![]
                    }
                    // the Scan key's panel shows the game size the plugin found, next to the size setting
                    Message::PropertyInspectorDidAppear { action, context, .. } if action == SCAN => {
                        let e = engine.lock().unwrap_or_else(|p| p.into_inner());
                        let detected = if p.standby { None } else { e.measured.zip(e.source).map(|(size, source)| found_size(size, source)) };
                        drop(e);
                        vec![MessageOut::SendToPropertyInspector { action, context, payload: json!({ "detected": detected }) }]
                    }
                    Message::KeyDown { action, context, .. } => {
                        record.write("press", &context, &action);
                        let c = cfg.lock().unwrap().clone();
                        let tx = tx.clone();
                        match action.as_str() {
                            SLOT => {
                                let slot = p.keys.get(&context).map_or(usize::MAX, |k| k.slot);
                                match p.loadout.get(slot).cloned().flatten() {
                                    // nothing there, or the game would refuse it right now
                                    None => vec![MessageOut::ShowAlert { context }],
                                    // a cooldown about to end lets the press through (see NEAR_READY)
                                    Some(v) if !callable(&v, Instant::now()) => {
                                        let left = v.ready.map_or(String::new(), |r| {
                                            let secs = r.at.saturating_duration_since(Instant::now()).as_secs_f32();
                                            format!(", {secs:.1} s left by {}", if r.timed { "the timer" } else { "the fill" })
                                        });
                                        log(format!("slot {} pressed while {:?}{left}", slot + 1, v.status));
                                        vec![MessageOut::ShowAlert { context }]
                                    }
                                    Some(v) => {
                                        let code = p.catalog[v.index].code.clone();
                                        log(format!("calling {} ({code})", p.catalog[v.index].name));
                                        let (engine, wake) = (engine.clone(), wake.clone());
                                        tokio::task::spawn_blocking(move || {
                                            let result = engine::fire(&code, &c, c.auto_throw);
                                            if result.is_ok() {
                                                let mut e = engine.lock().unwrap_or_else(|p| p.into_inner());
                                                if e.mark_called(v.index) {
                                                    let _ = tx.send(Event::Loadout(e.loadout(), e.chosen()));
                                                }
                                                drop(e);
                                                wake.burst();
                                            }
                                            let _ = tx.send(Event::Done { context, result });
                                        });
                                        vec![]
                                    }
                                }
                            }
                            // call every one of its loadout stratagems that is ready, in order, throwing each before the
                            // next; the others are skipped
                            SETUP => {
                                let picked = p.keys.get(&context).map(|k| k.loadout.clone()).unwrap_or_default();
                                let now = Instant::now();
                                let calls: Vec<(usize, String, String)> = picked
                                    .iter()
                                    .filter_map(|&k| if let Place::At(s) = p.loadout_slot(k) { p.loadout.get(s).cloned().flatten() } else { None })
                                    .filter(|v| callable(v, now))
                                    .map(|v| (v.index, p.catalog[v.index].code.clone(), p.catalog[v.index].name.clone()))
                                    .collect();
                                if c.throw_key.is_empty() {
                                    log("stratagem setup refused: it throws each stratagem, so it needs the throw key (set it on the Scan key)");
                                    vec![MessageOut::ShowAlert { context }]
                                } else if calls.is_empty() {
                                    log("stratagem setup refused: none of its stratagems is ready");
                                    vec![MessageOut::ShowAlert { context }]
                                } else {
                                    log(format!(
                                        "stratagem setup: calling {}",
                                        calls.iter().map(|(_, code, name)| format!("{name} ({code})")).collect::<Vec<_>>().join(", then ")
                                    ));
                                    let (engine, wake) = (engine.clone(), wake.clone());
                                    tokio::task::spawn_blocking(move || {
                                        let mut result = Ok(());
                                        for (k, (index, code, _)) in calls.iter().enumerate() {
                                            if k > 0 {
                                                std::thread::sleep(SETUP_GAP);
                                            }
                                            result = engine::fire(code, &c, true);
                                            if result.is_err() {
                                                break;
                                            }
                                            let mut e = engine.lock().unwrap_or_else(|p| p.into_inner());
                                            if e.mark_called(*index) {
                                                let _ = tx.send(Event::Loadout(e.loadout(), e.chosen()));
                                            }
                                            drop(e);
                                            wake.burst();
                                        }
                                        let _ = tx.send(Event::Done { context, result });
                                    });
                                    vec![]
                                }
                            }
                            // the game is not running: start it through Steam; scanning resumes once it runs.
                            // Started some other way since the last check: no second launch, just scan now.
                            SCAN if p.standby && hd_scanner::capture::running(&c.process_name) => {
                                log("start pressed while the game runs: scanning now");
                                wake.recheck();
                                vec![MessageOut::ShowOk { context }]
                            }
                            SCAN if p.standby => {
                                log(format!("starting the game: {STEAM_LAUNCH}"));
                                match std::process::Command::new("explorer.exe").arg(STEAM_LAUNCH).spawn() {
                                    Ok(_) => vec![MessageOut::ShowOk { context }],
                                    Err(e) => {
                                        log(format!("cannot start the game: {e}"));
                                        vec![MessageOut::ShowAlert { context }]
                                    }
                                }
                            }
                            SCAN => {
                                let engine = engine.clone();
                                tokio::task::spawn_blocking(move || {
                                    let mut e = engine.lock().unwrap_or_else(|p| p.into_inner());
                                    let result = e.scan_menu(&c).map(|_| ());
                                    let _ = tx.send(Event::Loadout(e.loadout(), e.chosen()));
                                    let _ = tx.send(Event::Done { context, result });
                                });
                                vec![]
                            }
                            CLEAR => {
                                let mut e = engine.lock().unwrap_or_else(|p| p.into_inner());
                                e.clear();
                                p.loadout = e.loadout();
                                p.chosen = e.chosen();
                                log("loadout cleared");
                                let mut out = p.paint_slots();
                                out.push(MessageOut::ShowOk { context });
                                out
                            }
                            _ => vec![],
                        }
                    }
                    _ => vec![],
                },
            },
            Some(ev) = rx.recv() => match ev {
                Event::Loadout(l, chosen) => {
                    if l != p.loadout || chosen != p.chosen {
                        p.loadout = l;
                        p.chosen = chosen;
                        p.paint_slots()
                    } else {
                        vec![]
                    }
                }
                Event::State(s) => {
                    p.state = s;
                    p.paint_where(|k| k.action == SCAN)
                }
                Event::Standby(on) => {
                    p.standby = on;
                    p.paint_where(|_| true)
                }
                Event::Done { context, result: Ok(()) } => vec![MessageOut::ShowOk { context }],
                Event::Done { context, result: Err(e) } => {
                    log(format!("key action failed: {e}"));
                    vec![
                        MessageOut::ShowAlert { context },
                        MessageOut::LogMessage { payload: LogMessagePayload { message: format!("hd2-plugin: {e}") } },
                    ]
                }
            },
            _ = anim.tick() => {
                p.frame += 1;
                // build.ps1 renames the running exe aside and puts a new build in its place: hand over to it
                // (a running process keeps reporting its original path, so the file's timestamp is compared).
                // Only a timestamp read and different counts: an exe that can't be read (missing for a moment
                // during a copy, or shielded, as a DRM-protected install may be) is no new build.
                let modified = || std::fs::metadata(&exe_path).and_then(|m| m.modified()).ok();
                if p.frame.is_multiple_of(10) && exe_built.zip(modified()).is_some_and(|(built, now)| built != now) {
                    log("a new build replaced this exe; exiting so Stream Deck starts it");
                    std::process::exit(0);
                }
                // acquiring tiles step every 600 ms, the radar every 200 ms; nothing moves in standby
                let step_acquiring = p.frame.is_multiple_of(3) && !p.standby;
                let (state, standby) = (p.state, p.standby);
                let acquiring: Vec<usize> = (0..SLOT_MAX).filter(|&s| p.is_acquiring(s)).collect();
                let waiting: Vec<usize> = (0..LOADOUT_SIZE).filter(|&k| p.loadout_slot(k) == Place::Waiting).collect();
                p.paint_where(|k| {
                    (k.action == SCAN && !standby && state != GameState::Missing)
                        || (step_acquiring && k.action == SLOT && acquiring.contains(&k.slot))
                        || (step_acquiring && k.action == SETUP && k.loadout.iter().any(|n| waiting.contains(n)))
                })
            }
        };
        for m in out {
            record.sent(&m);
            if sink.send(m).await.is_err() {
                log("send to Stream Deck failed; exiting");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use super::*;
    use hd_scanner::engine::Badge;
    use hd_scanner::img::Img;

    /// Reinforce's key picture, as the plugin makes it.
    fn icon() -> Vec<u8> {
        static ICON: OnceLock<Vec<u8>> = OnceLock::new();
        ICON.get_or_init(|| {
            let m = Matcher::load(&Data::Embedded).expect("built in");
            catalog(&m).into_iter().find(|s| s.name == "Reinforce").expect("Reinforce").png
        })
        .clone()
    }

    fn sv(index: usize, status: Status, fill: f32, badge: Option<Badge>) -> SlotView {
        SlotView { index, status, fill, badge, ready: None }
    }

    fn plugin(loadout: Loadout) -> Plugin {
        let catalog = vec![Stratagem { name: "Reinforce".into(), code: "UDRLU".into(), png: icon(), mission: true }];
        Plugin {
            keys: HashMap::new(),
            loadout,
            chosen: Vec::new(),
            catalog,
            icons: HashMap::new(),
            setups: HashMap::new(),
            frame: 0,
            state: GameState::Missing,
            standby: false,
        }
    }

    /// The SVG text inside a data URL.
    fn svg(url: &str) -> String {
        let b64 = url.strip_prefix("data:image/svg+xml;base64,").unwrap_or_else(|| panic!("not an SVG data URL: {url:.40}"));
        String::from_utf8(B64.decode(b64).expect("base64")).expect("utf-8")
    }

    /// Balanced tags, quoted attributes, and no leftovers of a format! template.
    fn assert_well_formed(svg: &str) {
        assert!(svg.starts_with("<svg ") && svg.trim_end().ends_with("</svg>"), "{svg}");
        assert!(svg.contains(r#"xmlns="http://www.w3.org/2000/svg""#) && svg.contains(r#"viewBox="0 0 144 144""#), "{svg}");
        assert!(!svg.contains(['{', '}']) && !svg.contains("NaN") && !svg.contains("inf"), "{svg}");
        let mut open = Vec::new();
        let mut rest = svg;
        while let Some(start) = rest.find('<') {
            assert!(rest[..start].trim().is_empty(), "text outside tags: {:?}", &rest[..start]);
            let end = start + rest[start..].find('>').expect("unclosed '<'");
            let tag = &rest[start + 1..end];
            assert!(!tag.contains('<') && tag.matches('"').count().is_multiple_of(2), "bad tag <{tag}>");
            if let Some(name) = tag.strip_prefix('/') {
                assert_eq!(open.pop(), Some(name.trim()), "closing </{name}>");
            } else if !tag.ends_with('/') {
                open.push(tag.split_whitespace().next().expect("tag name"));
            }
            rest = &rest[end + 1..];
        }
        assert!(rest.trim().is_empty() && open.is_empty(), "unclosed {open:?}");
    }

    fn decode(png_bytes: &[u8]) -> (usize, usize, Vec<u8>) {
        let mut reader = png::Decoder::new(std::io::Cursor::new(png_bytes)).read_info().expect("png");
        let mut buf = vec![0u8; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).expect("frame");
        assert_eq!((info.color_type, info.bit_depth), (png::ColorType::Rgba, png::BitDepth::Eight));
        buf.truncate(info.buffer_size());
        (info.width as usize, info.height as usize, buf)
    }

    #[test]
    fn a_press_goes_through_only_near_the_end_of_a_cooldown() {
        use hd_scanner::engine::ReadyAt;
        let now = Instant::now();
        let cooling = |secs: f32, timed: bool| SlotView {
            ready: Some(ReadyAt { at: now + Duration::from_secs_f32(secs), timed }),
            ..sv(0, Status::Cooldown, 0.1, None)
        };
        // estimated from the fill: in seconds left, whatever the cooldown's length (it used to be 5% of the fill,
        // 15 s of a 300 s cooldown)
        assert!(near_ready(&cooling(1.4, false), now));
        assert!(!near_ready(&cooling(1.6, false), now));
        assert!(!near_ready(&cooling(9.0, false), now));
        // a timer read on screen: only a few hundred milliseconds early
        assert!(near_ready(&cooling(0.25, true), now));
        assert!(!near_ready(&cooling(0.5, true), now));
        assert!(!near_ready(&cooling(1.4, true), now));
        assert!(near_ready(&cooling(0.0, true), now + Duration::from_secs(1)), "past the end");
        // nothing else counts as near ready
        assert!(!near_ready(&sv(0, Status::Cooldown, 0.0, None), now), "no estimate");
        let active = SlotView { status: Status::Active, ..cooling(0.1, true) };
        assert!(!near_ready(&active, now));
    }

    #[test]
    fn describe_lists_each_slot() {
        let names = vec!["Reinforce".to_string(), "Orbital Laser".to_string()];
        let l: Loadout = vec![Some(sv(0, Status::Ready, 0.0, None)), None, Some(sv(1, Status::Cooldown, 0.5, None))];
        assert_eq!(describe(&l, &names), "1: Reinforce Ready 0% | 2: ? | 3: Orbital Laser Cooldown 50%");
        assert_eq!(describe(&vec![Some(sv(1, Status::Unavailable, 1.0, None))], &names), "1: Orbital Laser Unavailable 100%");
        assert_eq!(describe(&vec![], &names), "");
    }

    #[test]
    fn data_url_is_base64() {
        assert_eq!(data_url("image/png", b"abc"), "data:image/png;base64,YWJj");
        assert_eq!(data_url("image/svg+xml", b""), "data:image/svg+xml;base64,");
    }

    #[test]
    fn acquiring_lights_one_more_arc_per_step() {
        let mut seen = Vec::new();
        for step in 0..=3 {
            let s = svg(&acquiring(step, false));
            assert_well_formed(&s);
            assert_eq!(s.matches(r#" opacity="1""#).count(), step, "step {step}");
            assert_eq!(s.matches(r#" opacity="0.2""#).count(), 3 - step, "step {step}");
            assert!(s.contains("#c9b269") && !s.contains(IDLE_FRAME));
            seen.push(s);
        }
        seen.dedup();
        assert_eq!(seen.len(), 4, "every step draws differently");
    }

    #[test]
    fn acquiring_in_standby_is_grey_and_still() {
        let idle: Vec<String> = (0..4).map(|step| svg(&acquiring(step, true))).collect();
        assert!(idle.iter().all(|s| s == &idle[0]), "standby does not animate");
        assert_well_formed(&idle[0]);
        assert_eq!(idle[0].matches(r#" opacity="1""#).count(), 3, "all arcs lit");
        assert!([IDLE_FRAME, IDLE_INNER, IDLE_INK].iter().all(|c| idle[0].contains(c)));
        assert!(!idle[0].contains("#c9b269") && !idle[0].contains("#ffffee"));
    }

    #[test]
    fn radar_turns_its_sweep_in_the_given_colour() {
        for (angle, ink) in [(0, "#8a8a8a"), (90, "#7be07b"), (330, "#ffd23f")] {
            let s = svg(&radar(angle, ink));
            assert_well_formed(&s);
            assert!(s.contains(&format!("rotate({angle})")) && s.contains(&format!(r#"fill="{ink}""#)), "{s}");
        }
        assert_ne!(radar(0, "#7be07b"), radar(30, "#7be07b"));
    }

    #[test]
    fn start_icon_and_blank_are_valid_svg() {
        let start = svg(&start_icon());
        assert_well_formed(&start);
        assert!(start.contains("#ffd23f"));
        assert_ne!(start_icon(), radar(0, "#ffd23f"));
        let b = svg(&blank());
        assert_well_formed(&b);
        assert!(b.contains(r##"fill="#000""##));
    }

    #[test]
    fn clear_icon_greys_out_in_standby() {
        assert_eq!(svg(&clear_icon(false)), CLEAR_SVG);
        assert_well_formed(CLEAR_SVG);
        let idle = svg(&clear_icon(true));
        assert_well_formed(&idle);
        for (live, grey) in [("#dc6455", IDLE_FRAME), ("#190301", IDLE_INNER), ("#ffffee", IDLE_INK)] {
            assert!(CLEAR_SVG.contains(live), "clear.svg no longer uses {live}: update clear_icon");
            assert!(!idle.contains(live) && idle.contains(grey), "{live} -> {grey}");
        }
    }

    #[test]
    fn icon_png_passes_the_icon_through_when_nothing_is_drawn() {
        let file = icon();
        for v in [
            sv(0, Status::Ready, 0.0, None),
            sv(0, Status::Unknown, 0.7, None),
            sv(0, Status::Cooldown, 0.0, None),
            sv(0, Status::Active, 0.0, None),
        ] {
            let out = icon_png(&file, &v).expect("the icon");
            assert!(matches!(out, Cow::Borrowed(b) if std::ptr::eq(b, &file[..])), "{v:?}: not the very same bytes");
        }
        // a broken picture goes through when nothing is drawn on it, and gives none to draw on
        assert_eq!(icon_png(b"not a png", &sv(0, Status::Ready, 0.0, None)).as_deref(), Some(&b"not a png"[..]));
        assert_eq!(icon_png(b"not a png", &sv(0, Status::Unavailable, 1.0, None)), None);
        // not 8-bit RGBA: passed through untouched rather than drawn on
        let grey = Data::Embedded.read("status/arrow_up.png").expect("built in");
        assert_eq!(icon_png(&grey, &sv(0, Status::Cooldown, 0.5, None)).as_deref(), Some(&grey[..]));
    }

    /// Each key shows its stratagem's icon as the matcher draws it from the game's texture, 144x144, as
    /// an 8-bit RGBA picture that icon_png can draw on.
    #[test]
    fn every_key_shows_its_stratagems_game_icon() {
        let m = Matcher::load(&Data::Embedded).expect("the catalog from the built-in data");
        let c = catalog(&m);
        assert_eq!(c.len(), m.catalog.len());
        for (i, (s, from)) in c.iter().zip(&m.catalog).enumerate() {
            assert_eq!((s.name.as_str(), s.code.as_str()), (from.name.as_str(), from.code.as_str()));
            let (w, h, px) = decode(&s.png);
            assert_eq!((w, h), (ICON_SIZE, ICON_SIZE), "{}", s.name);
            let want = m.icon(i);
            let rgb = px.chunks(4).flat_map(|p| [p[0], p[1], p[2]]);
            assert!(rgb.zip(&want.px).all(|(a, b)| (a as f32 - b).abs() <= 0.5), "{}: not the game's icon", s.name);
            assert!(px.chunks(4).all(|p| p[3] == 255), "{}: not opaque", s.name);
        }
    }

    #[test]
    fn icon_png_draws_the_fill_from_the_right_end() {
        let (w, h, src) = decode(&icon());
        let fill = FILL.map(|c| c as u8);
        let cream = CREAM.map(|c| c as u8);
        // (status, fill, rows covered, rows that must stay untouched; the edge line takes a row more)
        let cases = [
            (Status::Cooldown, 0.5, 72..h, 0..71),
            (Status::Active, 0.25, 0..36, 37..h),
            (Status::Unavailable, 0.0, 0..h, 0..0),
            (Status::Unavailable, 0.4, 0..h, 0..0),
        ];
        for (status, f, covered, clean) in cases {
            let (ow, oh, px) = decode(&icon_png(&icon(), &sv(0, status, f, None)).expect("drawn"));
            assert_eq!((ow, oh), (w, h), "{status:?}: same size as the icon");
            for y in 0..h {
                for x in 0..w {
                    let p = (y * w + x) * 4;
                    let (got, was) = (&px[p..p + 4], &src[p..p + 4]);
                    assert_eq!(got[3], was[3], "{status:?} ({x},{y}): alpha kept");
                    if clean.contains(&y) {
                        assert_eq!(got, was, "{status:?} ({x},{y}) changed");
                    } else if covered.contains(&y) {
                        assert!(got[..3] == fill || got[..3] == cream, "{status:?} ({x},{y}) = {got:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn icon_png_redraws_the_badge_in_the_corner() {
        let (w, h, src) = decode(&icon());
        // a 3x5 block of ink stands in for a digit
        let badge = Badge { hash: 7, w: 3, h: 5, bits: Arc::new(vec![true; 15]), number: None };
        let (_, _, px) = decode(&icon_png(&icon(), &sv(0, Status::Ready, 0.0, Some(badge))).expect("drawn"));
        // (3 + 2 * 7) x (5 + 2 * 5) game pixels at 144 / 57 key pixels each
        let (bw, bh) = (43, 38);
        let at = |x: usize, y: usize| &px[(y * w + x) * 4..(y * w + x) * 4 + 4];
        for y in 0..h {
            for x in 0..w {
                if x < w - bw || y < h - bh {
                    assert_eq!(at(x, y), &src[(y * w + x) * 4..(y * w + x) * 4 + 4], "({x},{y}) outside the badge changed");
                } else {
                    assert_eq!(at(x, y)[3], 255, "badge is opaque");
                }
            }
        }
        assert_eq!(at(w - bw, h - bh)[..3], BADGE_BG.map(|c| c as u8), "margin is the badge colour");
        assert_eq!(at(w - 1, h - 1)[..3], BADGE_BG.map(|c| c as u8));
        assert_eq!(at(w - bw + 21, h - bh + 19)[..3], BADGE_INK.map(|c| c as u8), "digit ink in the middle");
    }

    #[test]
    fn a_badge_read_as_a_number_is_drawn_with_clean_digits() {
        let (w, h, src) = decode(&icon());
        // a "1" of the game's size (8x14 at 1440p): drawn as the glyph, not as the blocks of the capture
        let badge = Badge { hash: 1, w: 8, h: 14, bits: Arc::new(vec![false; 112]), number: Some("1".into()) };
        let (_, _, px) = decode(&icon_png(&icon(), &sv(0, Status::Ready, 0.0, Some(badge))).expect("drawn"));
        let at = |x: usize, y: usize| -> [u8; 3] { std::array::from_fn(|c| px[(y * w + x) * 4 + c]) };
        let (bw, bh) = (((8.0 + 14.0) * 144.0 / 57.0_f32).round() as usize, ((14.0 + 10.0) * 144.0 / 57.0_f32).round() as usize);
        let (ox, oy) = (w - bw, h - bh);
        assert_eq!(at(ox + 1, oy + 1), BADGE_BG.map(|c| c as u8), "the badge's margin");
        // the 1's stem, down the middle of the digit's place, is ink; beside it, cream
        let (dx, dy) = ((7.0 * 144.0 / 57.0_f32).round() as usize, (5.0 * 144.0 / 57.0_f32).round() as usize);
        let stem_x = (ox + dx..w - dx).find(|&x| at(x, oy + dy + 18) == BADGE_INK.map(|c| c as u8)).expect("ink across the stem");
        assert!((stem_x as f32) > (ox + dx) as f32 + 4.0, "the stem, not the box's edge: {stem_x}");
        // partly inked pixels at the stem's edges: smooth, not blocky
        let row: Vec<[u8; 3]> = (ox + dx..w - dx).map(|x| at(x, oy + dy + 18)).collect();
        assert!(row.iter().any(|p| p[0] > BADGE_INK[0] as u8 + 10 && p[0] < BADGE_BG[0] as u8 - 10), "antialiased edges");
        let _ = src;
    }

    #[test]
    fn acquiring_until_the_open_menu_is_read() {
        let v = |i| Some(sv(i, Status::Ready, 0.0, None));
        // nothing read yet: every key waits
        let p = plugin(vec![]);
        assert!((0..12).all(|s| p.is_acquiring(s)));
        // only the closed HUD's few rows so far: unread keys keep waiting, past the end too
        let p = plugin(vec![v(0), None, v(0)]);
        assert_eq!((0..6).map(|s| p.is_acquiring(s)).collect::<Vec<_>>(), [false, true, false, true, true, true]);
        // the open menu was read (4 or more slots): gaps inside it wait, keys past it are blank
        let p = plugin(vec![v(0), v(0), v(0), None, v(0)]);
        assert_eq!((0..7).map(|s| p.is_acquiring(s)).collect::<Vec<_>>(), [false, false, false, true, false, false, false]);
    }

    fn image_of(out: &Out) -> &str {
        match out {
            MessageOut::SetImage { payload, .. } => payload.image.as_deref().expect("image"),
            other => panic!("expected SetImage, got {other:?}"),
        }
    }

    fn title_of(out: &Out) -> &str {
        match out {
            MessageOut::SetTitle { payload, .. } => payload.title.as_deref().expect("title"),
            other => panic!("expected SetTitle, got {other:?}"),
        }
    }

    /// A plugin with the whole catalog and the named stratagems in the menu, all ready; "" is a slot not read yet.
    fn plugin_with(menu: &[&str]) -> Plugin {
        static CATALOG: OnceLock<Vec<Stratagem>> = OnceLock::new();
        let all = CATALOG.get_or_init(|| catalog(&Matcher::load(&Data::Embedded).expect("built in"))).clone();
        let index = |name: &str| all.iter().position(|s| s.name == name).unwrap_or_else(|| panic!("no {name}"));
        let loadout = menu
            .iter()
            .map(|&n| (!n.is_empty()).then(|| SlotView { index: index(n), status: Status::Ready, fill: 0.0, badge: None, ready: None }))
            .collect();
        let mut p = plugin(loadout);
        p.catalog = all;
        p
    }

    const MENU: [&str; 7] =
        ["Reinforce", "SoS Beacon", "Resupply", "Orbital Laser", "LAS-98 Laser Cannon", "LIFT-182 Warp Pack", "M-102 Gunner FRV"];

    #[test]
    fn a_stratagem_setup_key_keeps_two_to_four_loadout_stratagems_and_saves_a_default() {
        let mut p = plugin(vec![]);
        let out = p.register(SETUP, "s", &json!({ "slots": [4, 5] }));
        match &out[0] {
            MessageOut::SetSettings { context, payload } => {
                assert_eq!((context.as_str(), payload), ("s", &json!({ "loadout": [1, 2] })), "menu slots of an earlier build go")
            }
            other => panic!("expected SetSettings, got {other:?}"),
        }
        assert_eq!(p.keys["s"].loadout, [0, 1]);
        // 1-based, each once, 1 to 4 only; a saved choice is not saved again
        let out = p.register(SETUP, "t", &json!({ "loadout": [3, 1, 3, 0, 5, 4, 2] }));
        assert!(out.iter().all(|m| !matches!(m, MessageOut::SetSettings { .. })));
        assert_eq!(p.keys["t"].loadout, [2, 0, 3, 1]);
        // fewer than two: the default
        p.register(SETUP, "u", &json!({ "loadout": [4] }));
        assert_eq!(p.keys["u"].loadout, [0, 1]);
    }

    #[test]
    fn the_loadout_is_found_past_the_stratagems_the_game_gives() {
        let p = plugin_with(&MENU);
        assert_eq!((0..4).map(|k| p.loadout_slot(k)).collect::<Vec<_>>(), [Place::At(3), Place::At(4), Place::At(5), Place::At(6)]);
        // a mission starting with a stratagem of its own moves the loadout along by one
        let shifted = ["Reinforce", "SoS Beacon", "Resupply", "Call In Super Destroyer", "Orbital Laser", "LAS-98 Laser Cannon"];
        let p = plugin_with(&shifted);
        assert_eq!((p.loadout_slot(0), p.loadout_slot(1)), (Place::At(4), Place::At(5)));
        assert_eq!(p.loadout_slot(2), Place::Missing, "a loadout of two");
        // two of the mission's own before the loadout, and one handed out later after it: the loadout is found all the same
        let two = [
            "Reinforce",
            "SoS Beacon",
            "Resupply",
            "Call In Super Destroyer",
            "Orbital Illumination Flare",
            "Orbital Laser",
            "LAS-98 Laser Cannon",
            "LIFT-182 Warp Pack",
            "M-102 Gunner FRV",
            "NUX-223 Hellbomb",
        ];
        let p = plugin_with(&two);
        assert_eq!(
            (0..5).map(|k| p.loadout_slot(k)).collect::<Vec<_>>(),
            [Place::At(5), Place::At(6), Place::At(7), Place::At(8), Place::Missing]
        );
        // not read yet: before the menu, or a slot before it unread
        assert_eq!(plugin_with(&["Reinforce"]).loadout_slot(0), Place::Waiting);
        assert_eq!(plugin_with(&["Reinforce", "SoS Beacon", "", "Orbital Laser"]).loadout_slot(0), Place::Waiting);
        assert_eq!(plugin_with(&["Reinforce", "SoS Beacon", "Resupply", "Orbital Laser", ""]).loadout_slot(0), Place::At(3));
    }

    #[test]
    fn the_setup_key_binds_to_the_card_past_a_mission_stratagem_it_cannot_read() {
        // the live menu from issue #9: a mission put HMG Emplacement (one a player can also choose, so not flagged a
        // mission stratagem) and an Eagle the reader can't place ("") before the loadout, moving it to slots 6..10.
        // Counting the menu would take HMG for the first loadout stratagem and stall at the unreadable one; binding to
        // the card, which says what the player chose, finds the loadout all the same.
        let menu = [
            "Reinforce", "SoS Beacon", "Resupply", "E/MG-101 HMG Emplacement", "", "Eagle Rearm",
            "Orbital Laser", "LAS-98 Laser Cannon", "LIFT-182 Warp Pack", "EXO-45 Patriot Exosuit",
        ];
        let mut p = plugin_with(&menu);
        p.chosen = ["Orbital Laser", "LAS-98 Laser Cannon", "LIFT-182 Warp Pack", "EXO-45 Patriot Exosuit"]
            .iter()
            .map(|name| p.catalog.iter().position(|s| s.name == *name).unwrap_or_else(|| panic!("no {name}")))
            .collect();
        assert_eq!(
            (0..4).map(|k| p.loadout_slot(k)).collect::<Vec<_>>(),
            [Place::At(6), Place::At(7), Place::At(8), Place::At(9)],
        );
        assert_eq!(p.loadout_slot(4), Place::Missing, "the loadout has four");
        // the first loadout stratagem not on screen yet: nothing is placed, even with the card known
        let mut early = plugin_with(&["Reinforce", "SoS Beacon", "Resupply"]);
        early.chosen = p.chosen.clone();
        assert_eq!(early.loadout_slot(0), Place::Waiting);
    }

    #[test]
    fn a_stratagem_setup_key_shows_its_stratagems_side_by_side_or_in_a_square() {
        let shifted = ["Reinforce", "SoS Beacon", "Resupply", "Call In Super Destroyer", "Orbital Laser", "LAS-98 Laser Cannon"];
        let mut p = plugin_with(&shifted);
        p.register(SETUP, "two", &json!({ "loadout": [1, 2] }));
        p.register(SETUP, "three", &json!({ "loadout": [2, 1, 3] }));
        let picture = |p: &mut Plugin, key: &str| {
            let out = p.paint(key);
            let png = B64.decode(image_of(&out[0]).strip_prefix("data:image/png;base64,").expect("a PNG")).unwrap();
            let (w, h, px) = decode(&png);
            assert_eq!((w, h), (144, 144), "{key}");
            move |x: usize, y: usize| -> [u8; 3] { std::array::from_fn(|c| px[(y * w + x) * 4 + c]) }
        };
        // two side by side across the middle: the Orbital Laser (red frame), then the Laser Cannon (blue frame)
        let at = picture(&mut p, "two");
        assert!(at(4, 72)[0] > 150 && at(4, 72)[2] < 120, "red: {:?}", at(4, 72));
        assert!(at(76, 72)[2] > 150 && at(76, 72)[0] < 120, "blue: {:?}", at(76, 72));
        // three in a square, two on top: the Laser Cannon, the Orbital Laser, and a third the loadout doesn't have
        let at = picture(&mut p, "three");
        assert!(at(4, 36)[2] > 150 && at(76, 36)[0] > 150, "{:?} {:?}", at(4, 36), at(76, 36));
        assert_eq!(at(72, 108), [0, 0, 0], "no third stratagem: black");
        // nothing read yet: the waiting signal in each place, as on the slot keys
        let mut p = plugin_with(&[]);
        p.register(SETUP, "two", &json!({ "loadout": [1, 2] }));
        let at = picture(&mut p, "two");
        assert_eq!((at(3, 39), at(75, 39)), ([90, 80, 47], [90, 80, 47]), "the signal's frame, gold at 45%");
        assert_eq!(at(74 + 34, 38 + 45), [255, 255, 238], "its dot");
        assert_eq!(p.setups.len(), 1, "cached");
        p.paint("two");
        assert_eq!(p.setups.len(), 1);
    }

    #[test]
    fn the_waiting_signal_is_drawn_like_the_slot_keys_one() {
        let img = acquiring_img(1, false, 144);
        let at = |x: usize, y: usize| -> [u8; 3] { std::array::from_fn(|c| img.px[(y * 144 + x) * 3 + c].round() as u8) };
        assert_eq!(at(2, 2), [90, 80, 47], "frame: #c9b269 at 45% on black");
        assert_eq!(at(30, 30), [38, 36, 26], "inside: #26241a");
        assert_eq!(at(72, 96), [255, 255, 238], "the dot");
        assert_eq!(at(72, 76), [255, 255, 238], "the first arc, lit at step 1");
        assert!(at(72, 60)[0] < 100, "the second arc, dim at step 1");
        assert_eq!(acquiring_img(1, true, 144).px[(96 * 144 + 72) * 3].round(), 154.0, "grey in standby: #9a9a9a");
    }

    #[test]
    fn a_press_goes_through_unless_the_stratagem_is_busy() {
        let now = Instant::now();
        let v = |status: Status| SlotView { index: 0, status, fill: 0.5, badge: None, ready: None };
        assert!(callable(&v(Status::Ready), now) && callable(&v(Status::Unknown), now));
        for status in [Status::Cooldown, Status::Unavailable, Status::Active] {
            assert!(!callable(&v(status), now), "{status:?}");
        }
        let ending = SlotView { ready: Some(engine::ReadyAt { at: now + Duration::from_millis(200), timed: true }), ..v(Status::Cooldown) };
        assert!(callable(&ending, now), "the last moment of a cooldown read on screen");
    }

    #[test]
    fn a_new_slot_key_takes_the_first_free_slot_and_saves_it() {
        let mut p = plugin(vec![]);
        let out = p.register(SLOT, "a", &json!({}));
        match &out[0] {
            MessageOut::SetSettings { context, payload } => assert_eq!((context.as_str(), &payload["slot"]), ("a", &json!(1))),
            other => panic!("expected SetSettings, got {other:?}"),
        }
        assert_eq!(out.len(), 3, "settings, image, title");
        let out = p.register(SLOT, "b", &json!({ "show_name": true }));
        match &out[0] {
            MessageOut::SetSettings { payload, .. } => assert_eq!(payload, &json!({ "show_name": true, "slot": 2 })),
            other => panic!("expected SetSettings, got {other:?}"),
        }
        let out = p.register(SLOT, "c", &json!({ "slot": 7 }));
        assert_eq!(out.len(), 2, "a saved slot is not saved again");
        assert_eq!((p.keys["a"].slot, p.keys["b"].slot, p.keys["c"].slot), (0, 1, 6));
        assert!(p.keys["b"].show_name && !p.keys["a"].show_name);
        // slot 0 in the settings means none; a key re-registering skips its own slot when looking
        p.register(SLOT, "d", &json!({ "slot": 0 }));
        assert_eq!(p.keys["d"].slot, 2);
        p.register(SLOT, "a", &json!({}));
        assert_eq!(p.keys["a"].slot, 0);
        // other actions do not take slots
        assert!(p.register(SCAN, "s", &json!({})).iter().all(|m| !matches!(m, MessageOut::SetSettings { .. })));
    }

    #[test]
    fn slot_keys_paint_the_icon_or_a_placeholder() {
        let mut p = plugin(vec![Some(sv(0, Status::Ready, 0.0, None)), None, None, None, Some(sv(0, Status::Cooldown, 0.5, None))]);
        p.register(SLOT, "named", &json!({ "slot": 1, "show_name": true }));
        p.register(SLOT, "plain", &json!({ "slot": 5 }));
        p.register(SLOT, "gap", &json!({ "slot": 2 }));
        p.register(SLOT, "past", &json!({ "slot": 9 }));
        let out = p.paint("named");
        assert!(image_of(&out[0]).starts_with("data:image/png;base64,"));
        assert_eq!(title_of(&out[1]), "Reinforce");
        let out = p.paint("plain");
        assert!(image_of(&out[0]).starts_with("data:image/png;base64,"));
        assert_eq!(title_of(&out[1]), "");
        assert_eq!(image_of(&p.paint("gap")[0]), acquiring(0, false));
        assert_eq!(image_of(&p.paint("past")[0]), blank());
        assert_eq!(p.icons.len(), 2, "one cached icon per (stratagem, status, fill step, badge)");
        assert!(p.paint("unknown context").is_empty());
    }

    #[test]
    fn scan_and_clear_keys_follow_the_game_state() {
        let mut p = plugin(vec![]);
        p.register(SCAN, "scan", &json!({}));
        p.register(CLEAR, "clear", &json!({}));
        p.frame = 5;
        assert_eq!(image_of(&p.paint("scan")[0]), radar(0, "#8a8a8a"), "game missing: grey, at rest");
        p.state = GameState::Lobby;
        assert_eq!(image_of(&p.paint("scan")[0]), radar(150, "#ffd23f"));
        p.state = GameState::Mission;
        assert_eq!(image_of(&p.paint("scan")[0]), radar(150, "#7be07b"));
        assert_eq!(image_of(&p.paint("clear")[0]), clear_icon(false));
        p.standby = true;
        assert_eq!(image_of(&p.paint("scan")[0]), start_icon());
        assert_eq!(image_of(&p.paint("clear")[0]), clear_icon(true));
    }

    /// Not a check: plays a scripted mission through the plugin's own painting code and writes every frame's
    /// key images to `target/demo/frames.json`, which `tools/make_demo_gif.py` turns into `docs/img/demo.gif`.
    /// cargo test --bin hd2-plugin -- --ignored --nocapture demo_frames
    #[test]
    #[ignore = "generator for docs/img/demo.gif"]
    fn demo_frames() {
        use hd_scanner::matcher::column_box;

        let data = Data::Embedded;
        let mut m = Matcher::load(&data).expect("catalog");
        let catalog = catalog(&m);
        let idx = |name: &str| catalog.iter().position(|s| s.name == name).unwrap_or_else(|| panic!("no {name}"));
        let (reinforce, sos, resupply) = (idx("Reinforce"), idx("SoS Beacon"), idx("Resupply"));
        let (laser, quasar, warp, frv) =
            (idx("Orbital Laser"), idx("LAS-99 Quasar Cannon"), idx("LIFT-182 Warp Pack"), idx("M-102 Gunner FRV"));

        // the game's own count badges, as read from the screenshot fixtures
        let mut badges = |fixture: &str| -> HashMap<usize, Badge> {
            let col = Img::load_png(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(fixture)).expect("fixture");
            let (x0, y0, _, _) = column_box(2560, 1440, 1.0);
            m.reset();
            m.read_rows(&col, 1440, (x0, y0), 1.0).into_iter().filter_map(|r| r.badge.map(|b| (r.index, b))).collect()
        };
        let (open, activating) = (badges("menu_open.png"), badges("menu_activating.png"));
        let laser_uses = |left: u8| if left == 3 { activating.get(&laser).cloned() } else { open.get(&laser).cloned() };

        let keys = ["scan", "clear", "s1", "s2", "s3", "s4", "s5", "s6", "s7", "setup"];
        let mut p = Plugin {
            keys: HashMap::new(),
            loadout: vec![],
            chosen: Vec::new(),
            catalog,
            icons: HashMap::new(),
            setups: HashMap::new(),
            frame: 0,
            state: GameState::Missing,
            standby: true,
        };
        p.register(SCAN, "scan", &json!({}));
        p.register(CLEAR, "clear", &json!({}));
        for k in 1..=7 {
            p.register(SLOT, &format!("s{k}"), &json!({ "slot": k }));
        }
        // the loadout's second and third stratagems: the Quasar Cannon and the Warp Pack
        p.register(SETUP, "setup", &json!({ "loadout": [2, 3] }));
        let mut frames: Vec<Value> = Vec::new();
        let mut shoot = |p: &mut Plugin, pressed: Option<usize>| {
            p.frame += 1;
            let images: Vec<String> = keys.iter().map(|k| image_of(&p.paint(k)[0]).to_string()).collect();
            frames.push(json!({ "keys": images, "pressed": pressed }));
        };

        // the game is closed: a start button, grey keys; then the start button is pressed
        for i in 0..8 {
            shoot(&mut p, (i == 7).then_some(0));
        }
        // the game runs, on the ship: yellow radar, the slots wait for a loadout
        (p.standby, p.state) = (false, GameState::Lobby);
        for _ in 0..12 {
            shoot(&mut p, None);
        }
        // a mission starts: green radar; the menu is read once and the keys fill
        p.state = GameState::Mission;
        for _ in 0..4 {
            shoot(&mut p, None);
        }
        let ready = |index: usize, badge: Option<Badge>| Some(sv(index, Status::Ready, 0.0, badge));
        let mut loadout = vec![
            ready(reinforce, open.get(&reinforce).cloned()),
            ready(sos, open.get(&sos).cloned()),
            ready(resupply, None),
            ready(laser, laser_uses(3)),
            ready(quasar, None),
            ready(warp, None),
            ready(frv, None),
        ];
        p.loadout = loadout.clone();
        for _ in 0..8 {
            shoot(&mut p, None);
        }
        // the Orbital Laser is called in from its key, then the Setup key calls the Quasar Cannon and, a throw later,
        // the Warp Pack: each on its way (fill from the top), then cooling down (fill from the bottom, sped up here)
        let flight = |t: i32, start: i32, flying: i32, cooling: i32| -> Option<(Status, f32)> {
            let t = t - start;
            if t < 0 {
                None
            } else if t < flying {
                Some((Status::Active, (t + 1) as f32 / (flying + 1) as f32))
            } else if t < flying + cooling {
                Some((Status::Cooldown, 1.0 - (t - flying) as f32 / cooling as f32))
            } else {
                None
            }
        };
        for t in 0..56 {
            let (l, q, w) = (flight(t, 0, 8, 24), flight(t, 10, 6, 28), flight(t, 15, 6, 30));
            let badge = laser_uses(if t >= 8 { 2 } else { 3 });
            loadout[3] = Some(l.map_or(sv(laser, Status::Ready, 0.0, badge.clone()), |(s, f)| sv(laser, s, f, badge.clone())));
            loadout[4] = Some(q.map_or(sv(quasar, Status::Ready, 0.0, None), |(s, f)| sv(quasar, s, f, None)));
            loadout[5] = Some(w.map_or(sv(warp, Status::Ready, 0.0, None), |(s, f)| sv(warp, s, f, None)));
            p.loadout = loadout.clone();
            shoot(
                &mut p,
                match t {
                    0 => Some(5),
                    10 => Some(9),
                    _ => None,
                },
            );
        }
        for _ in 0..8 {
            shoot(&mut p, None);
        }

        let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/demo");
        std::fs::create_dir_all(&out).expect("target/demo");
        let file = out.join("frames.json");
        std::fs::write(&file, serde_json::to_vec(&json!({ "columns": 5, "frame_ms": ANIM.as_millis() as u64, "frames": frames })).unwrap())
            .expect("write frames");
        println!("{} frames written to {}", frames.len(), file.display());
    }

    /// Not a check: the keys the plugin draws for two real screenshots of the menu (read through the reader,
    /// each row shown as a key shows it on first sight), the keys in standby, and the Scan key's four looks.
    /// `tools/make_store_media.py` lays them out next to the screenshots for the Marketplace page.
    /// cargo test --bin hd2-plugin -- --ignored --nocapture store_scenes
    #[test]
    #[ignore = "generator for the Marketplace media"]
    fn store_scenes() {
        use hd_scanner::matcher::column_box;

        let data = Data::Embedded;
        let mut m = Matcher::load(&data).expect("catalog");
        let catalog = catalog(&m);
        let keys = ["scan", "clear", "s1", "s2", "s3", "s4", "s5", "s6", "s7", "setup"];
        let mut p = Plugin {
            keys: HashMap::new(),
            loadout: vec![],
            chosen: Vec::new(),
            catalog,
            icons: HashMap::new(),
            setups: HashMap::new(),
            frame: 4,
            state: GameState::Mission,
            standby: false,
        };
        p.register(SCAN, "scan", &json!({}));
        p.register(CLEAR, "clear", &json!({}));
        for k in 1..=7 {
            p.register(SLOT, &format!("s{k}"), &json!({ "slot": k }));
        }
        // the loadout's second and third stratagems: the Quasar Cannon and the Warp Pack
        p.register(SETUP, "setup", &json!({ "loadout": [2, 3] }));
        let shoot = |p: &mut Plugin| -> Vec<String> { keys.iter().map(|k| image_of(&p.paint(k)[0]).to_string()).collect() };

        let mut scenes = serde_json::Map::new();
        for fixture in ["menu_open", "menu_unavailable"] {
            let col = Img::load_png(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(format!("{fixture}.png")))
                .expect("fixture");
            let (x0, y0, _, _) = column_box(2560, 1440, 1.0);
            m.reset();
            let rows = m.read_rows(&col, 1440, (x0, y0), 1.0);
            let mut loadout: Loadout = vec![None; rows.iter().map(|r| r.slot + 1).max().unwrap_or(0)];
            for r in rows {
                let (status, fill) = match r.status {
                    Status::Cooldown => (Status::Cooldown, r.fill_bottom),
                    Status::Active => (Status::Active, r.fill_top),
                    Status::Unavailable => (Status::Unavailable, 1.0),
                    _ => (Status::Ready, 0.0),
                };
                let badge = if status == Status::Unavailable { None } else { r.badge };
                loadout[r.slot] = Some(SlotView { index: r.index, status, fill, badge, ready: None });
            }
            p.loadout = loadout;
            scenes.insert(fixture.into(), json!(shoot(&mut p)));
        }
        (p.loadout, p.standby, p.state) = (vec![], true, GameState::Missing);
        scenes.insert("standby".into(), json!(shoot(&mut p)));

        // a press of the Setup key: both ready, the Quasar Cannon on its way, then the Warp Pack on its way too
        let idx = |name: &str| p.catalog.iter().position(|s| s.name == name).unwrap_or_else(|| panic!("no {name}"));
        let (quasar, warp) = (idx("LAS-99 Quasar Cannon"), idx("LIFT-182 Warp Pack"));
        let menu: Vec<usize> =
            ["Reinforce", "SoS Beacon", "Resupply", "Orbital Laser", "LAS-99 Quasar Cannon", "LIFT-182 Warp Pack", "M-102 Gunner FRV"]
                .iter()
                .map(|n| idx(n))
                .collect();
        (p.standby, p.state) = (false, GameState::Mission);
        let mut setup = serde_json::Map::new();
        for (name, q, w) in [
            ("ready", (Status::Ready, 0.0), (Status::Ready, 0.0)),
            ("first", (Status::Active, 0.5), (Status::Ready, 0.0)),
            ("both", (Status::Active, 0.9), (Status::Active, 0.4)),
        ] {
            p.loadout = menu
                .iter()
                .map(|&i| {
                    let (status, fill) = if i == quasar {
                        q
                    } else if i == warp {
                        w
                    } else {
                        (Status::Ready, 0.0)
                    };
                    Some(sv(i, status, fill, None))
                })
                .collect();
            setup.insert(name.into(), json!(image_of(&p.paint("setup")[0])));
        }
        p.loadout = vec![];

        let mut scan = serde_json::Map::new();
        for (name, standby, state) in [
            ("start", true, GameState::Missing),
            ("missing", false, GameState::Missing),
            ("lobby", false, GameState::Lobby),
            ("mission", false, GameState::Mission),
        ] {
            (p.standby, p.state) = (standby, state);
            scan.insert(name.into(), json!(image_of(&p.paint("scan")[0])));
        }

        let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/demo");
        std::fs::create_dir_all(&out).expect("target/demo");
        let file = out.join("scenes.json");
        std::fs::write(&file, serde_json::to_vec(&json!({ "columns": 5, "scenes": scenes, "scan": scan, "setup": setup })).unwrap())
            .expect("write scenes");
        println!("scenes written to {}", file.display());
    }
}
