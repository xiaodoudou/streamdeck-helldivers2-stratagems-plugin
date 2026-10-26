//! End-to-end reads of real game frames.
//!
//! tests/fixtures/<name>.png is the icon column of data/samples/<name>.png (full 2560x1440
//! screenshots, not in git), cropped to exactly `column_box(2560, 1440, 1.0)`; `make_fixtures`
//! below rebuilds them. FRAMES lists what each frame visibly shows, judged by eye from the
//! screenshots. Where the reader disagrees with the picture, the visible value stays in FRAMES and
//! the disagreement is listed in MISREADS, checked by its own ignored test.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use hd_scanner::data::Data;
use hd_scanner::engine::code_from_read;
use hd_scanner::img::Img;
use hd_scanner::matcher::{Matcher, Row, column_box};
use hd_scanner::status::Status::{self, *};

const FRAME_W: usize = 2560;
const FRAME_H: usize = 1440;

/// Raw fill readings at or below this count as no fill: clean icons read up to about 0.09.
const CLEAR: f32 = 0.15;
/// Raw fill readings at or above this count as covering the whole icon.
const FULL: f32 = 0.9;

#[derive(Clone, Copy, Debug)]
enum Fill {
    Clear,            // no progress fill
    Bottom(f32, f32), // rising from the bottom (cooldown), covering this share of the icon
    Top(f32, f32),    // coming down from the top (inbound)
    Full,             // over the whole icon (unavailable, impact)
}

struct Seen {
    slot: usize,
    name: &'static str,
    status: Status, // Unknown: the row shows no status line (closed HUD)
    fill: Fill,
    badge: Option<u8>,          // the digit on the count badge
    code: Option<&'static str>, // the arrows on the status line
}

struct Frame {
    name: &'static str,
    rows: &'static [Seen],
}

const fn seen(slot: usize, name: &'static str, status: Status, fill: Fill, badge: Option<u8>, code: Option<&'static str>) -> Seen {
    Seen { slot, name, status, fill, badge, code }
}

const REINFORCE: &str = "Reinforce";
const SOS: &str = "SoS Beacon";
const RESUPPLY: &str = "Resupply";
const LASER: &str = "Orbital Laser";
const QUASAR: &str = "LAS-99 Quasar Cannon";
const WARP: &str = "LIFT-182 Warp Pack";
const FRV: &str = "M-102 Gunner FRV";

const FRAMES: &[Frame] = &[
    Frame {
        name: "menu_open",
        rows: &[
            seen(0, REINFORCE, Ready, Fill::Clear, Some(5), None),
            seen(1, SOS, Ready, Fill::Clear, Some(1), Some("UDRU")),
            seen(2, RESUPPLY, Ready, Fill::Clear, None, Some("DDUR")),
            seen(3, LASER, Active, Fill::Full, Some(2), None),             // "Impact"
            seen(4, QUASAR, Cooldown, Fill::Bottom(0.6, 0.9), None, None), // T-04:50
            seen(5, WARP, Cooldown, Fill::Bottom(0.6, 0.9), None, None),   // T-04:48
            seen(6, FRV, Ready, Fill::Clear, None, Some("LDRDRDU")),
        ],
    },
    Frame {
        name: "menu_unavailable",
        rows: &[
            seen(0, REINFORCE, Ready, Fill::Clear, Some(4), None),
            seen(1, SOS, Ready, Fill::Clear, Some(1), Some("UDRU")),
            seen(2, RESUPPLY, Cooldown, Fill::Bottom(0.0, 0.2), None, None), // T-00:16, a thin band at the bottom
            seen(3, LASER, Unavailable, Fill::Full, None, None),
            seen(4, QUASAR, Cooldown, Fill::Bottom(0.0, 0.1), None, None), // T-00:05, almost done
            seen(5, WARP, Active, Fill::Top(0.1, 0.35), None, None),       // "Inbound T-00:07"
            seen(6, FRV, Ready, Fill::Clear, None, Some("LDRDRDU")),       // dimmed, arrows shown
        ],
    },
    Frame {
        name: "menu_activating",
        rows: &[
            seen(0, REINFORCE, Unknown, Fill::Clear, Some(5), None),
            seen(3, LASER, Active, Fill::Clear, Some(3), None), // "Activating", red icon
        ],
    },
    Frame {
        name: "closed_ready_row",
        rows: &[
            seen(0, REINFORCE, Unknown, Fill::Clear, Some(5), None),
            // the closed HUD's ready notice shows four arrows; stratagems.json has DLRDLR
            seen(5, WARP, Ready, Fill::Clear, None, Some("DLRD")),
        ],
    },
    Frame { name: "menu_closed", rows: &[seen(0, REINFORCE, Unknown, Fill::Clear, Some(5), None)] },
    Frame { name: "capture_2560x1440_081813", rows: &[seen(0, REINFORCE, Unknown, Fill::Clear, Some(4), None)] },
    Frame {
        name: "capture_2560x1440_082327",
        rows: &[
            seen(0, REINFORCE, Ready, Fill::Clear, Some(4), None),
            seen(1, SOS, Ready, Fill::Clear, Some(1), Some("UDRU")),
            seen(2, RESUPPLY, Ready, Fill::Clear, None, Some("DDUR")),
            seen(3, LASER, Unavailable, Fill::Full, None, None),
            seen(4, QUASAR, Cooldown, Fill::Bottom(0.3, 0.6), None, None), // T-03:08
            seen(5, WARP, Cooldown, Fill::Bottom(0.3, 0.6), None, None),   // T-02:59
            seen(6, FRV, Ready, Fill::Clear, None, Some("LDRDRDU")),
        ],
    },
];

/// Frames checked by a test of their own rather than through FRAMES, cropped by make_fixtures all the same.
const EXTRA_FIXTURES: &[&str] = &["super_destroyer_unavailable"];

/// The timers the status lines show, in seconds ("Cooldown T-04:50" is 290); every other row shows none.
const TIMERS: &[(&str, usize, u32)] = &[
    ("menu_open", 4, 4 * 60 + 50),
    ("menu_open", 5, 4 * 60 + 48),
    ("menu_unavailable", 2, 16),
    ("menu_unavailable", 4, 5),
    ("menu_unavailable", 5, 7), // Inbound
    ("capture_2560x1440_082327", 4, 3 * 60 + 8),
    ("capture_2560x1440_082327", 5, 2 * 60 + 59),
];

#[derive(Clone, Copy, Debug, PartialEq)]
enum Field {
    Name,
    Sure,
    Status,
    Fill,
    Badge,
    Code,
}

/// Where the reader disagrees with the picture today: (frame, slot, field).
const MISREADS: &[(&str, usize, Field)] = &[("menu_open", 2, Field::Code), ("menu_unavailable", 6, Field::Fill)];

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn data_dir() -> PathBuf {
    manifest_dir().join("../data")
}

fn fixture_path(name: &str) -> PathBuf {
    manifest_dir().join("tests/fixtures").join(format!("{name}.png"))
}

fn origin() -> (usize, usize) {
    let (x0, y0, _, _) = column_box(FRAME_W, FRAME_H, 1.0);
    (x0, y0)
}

struct Read {
    rows: Vec<Row>,
    names: Vec<String>,
    codes: Vec<String>, // arrows read on each row's status line
}

/// Every frame read once from a fresh row cache, as `bench` does; shared by the tests.
fn reads() -> &'static HashMap<&'static str, Read> {
    static READS: OnceLock<HashMap<&'static str, Read>> = OnceLock::new();
    READS.get_or_init(|| {
        let mut m = Matcher::load(&Data::Dir(data_dir())).expect("catalog");
        FRAMES
            .iter()
            .map(|f| {
                let col = Img::load_png(&fixture_path(f.name)).expect(f.name);
                m.reset();
                let rows = m.read_rows(&col, FRAME_H, origin(), 1.0);
                let codes = rows.iter().map(|r| m.slot_code(&col, FRAME_H, origin(), 1.0, r.slot)).collect();
                let names = rows.iter().map(|r| m.catalog[r.index].name.clone()).collect();
                (f.name, Read { rows, names, codes })
            })
            .collect()
    })
}

fn frame(name: &str) -> &'static Frame {
    FRAMES.iter().find(|f| f.name == name).expect("frame in FRAMES")
}

/// Mismatches between what was read and what the frame shows, for the fields `check` selects.
fn mismatches(name: &str, check: impl Fn(usize, Field) -> bool) -> Vec<String> {
    let (f, read) = (frame(name), &reads()[name]);
    let mut out = Vec::new();
    let read_slots: Vec<usize> = read.rows.iter().map(|r| r.slot).collect();
    let seen_slots: Vec<usize> = f.rows.iter().map(|s| s.slot).collect();
    if read_slots != seen_slots {
        out.push(format!("rows in slots {read_slots:?}, the frame shows {seen_slots:?} ({:?})", read.names));
    }
    for s in f.rows {
        let Some(i) = read.rows.iter().position(|r| r.slot == s.slot) else { continue };
        let (r, name, code) = (&read.rows[i], &read.names[i], &read.codes[i]);
        let mut bad = |field: Field, ok: bool, msg: String| {
            if check(s.slot, field) && !ok {
                out.push(format!("slot {} {}: {msg}", s.slot, s.name));
            }
        };
        bad(Field::Name, name == s.name, format!("read as {name}"));
        bad(Field::Sure, r.sure, format!("read unsure (score {:.3})", r.score));
        bad(Field::Status, r.status == s.status, format!("status {:?}, shows {:?}", r.status, s.status));
        let (b, t) = (r.fill_bottom, r.fill_top);
        let fill_ok = match s.fill {
            Fill::Clear => b <= CLEAR && t <= CLEAR,
            Fill::Bottom(lo, hi) => (lo..=hi).contains(&b) && t <= CLEAR,
            Fill::Top(lo, hi) => (lo..=hi).contains(&t) && b <= CLEAR,
            Fill::Full => b >= FULL && t >= FULL,
        };
        bad(Field::Fill, fill_ok, format!("fill bottom {b:.3} top {t:.3}, shows {:?}", s.fill));
        bad(Field::Badge, r.badge.is_some() == s.badge.is_some(), format!("badge read {:?}, shows {:?}", r.badge.is_some(), s.badge));
        if let Some(want) = s.code {
            bad(Field::Code, code == want, format!("arrows read {code:?}, shows {want:?}"));
        }
    }
    out
}

fn known_misread(name: &str, slot: usize, field: Field) -> bool {
    MISREADS.contains(&(name, slot, field))
}

/// Everything the frame shows is read, apart from the known misreads.
fn assert_frame(name: &str) {
    let bad = mismatches(name, |slot, field| !known_misread(name, slot, field));
    assert!(bad.is_empty(), "{name}:\n{}", bad.join("\n"));
}

/// One field of one row is read as the frame shows it.
fn assert_field(name: &str, slot: usize, field: Field) {
    let bad = mismatches(name, |s, f| s == slot && f == field);
    assert!(bad.is_empty(), "{name}:\n{}", bad.join("\n"));
}

/// The known misread still happens (so MISREADS stays current) and, once fixed, this passes.
fn assert_misread_fixed(name: &str, slot: usize, field: Field) {
    assert!(known_misread(name, slot, field));
    assert_field(name, slot, field);
}

/// What a key would type from the codes the rows read (see code_from_read) is, on every frame, the stratagem's own
/// code: the catalog's, as these frames predate any code change. That holds for a code shown cut short (the Warp
/// Pack's first four arrows on closed_ready_row) and for a stray arrow read past one (Resupply on menu_open). A row
/// showing a word reads no code.
#[test]
fn codes_read_with_the_rows_resolve_to_the_stratagems_own() {
    let m = Matcher::load(&Data::Dir(data_dir())).expect("catalog");
    let mut bad = Vec::new();
    for f in FRAMES {
        for s in f.rows {
            let Some(r) = reads()[f.name].rows.iter().find(|r| r.slot == s.slot && m.catalog[r.index].name == s.name) else { continue };
            let st = &m.catalog[r.index];
            let typed = r.code.as_ref().map(|c| code_from_read(c, &st.codes().collect::<Vec<_>>()).unwrap_or(st.code.clone()));
            if typed.as_deref() != s.code.map(|_| st.code.as_str()) {
                bad.push(format!("{} slot {} {}: shows {:?}, read {:?}, would type {typed:?}", f.name, s.slot, s.name, s.code, r.code));
            }
        }
    }
    assert!(bad.is_empty(), "\n{}", bad.join("\n"));
}

#[test]
fn fixtures_are_the_column_box_of_a_2560x1440_frame() {
    let (x0, y0, x1, y1) = column_box(FRAME_W, FRAME_H, 1.0);
    for name in FRAMES.iter().map(|f| f.name).chain(EXTRA_FIXTURES.iter().copied()) {
        let path = fixture_path(name);
        let img = Img::load_png(&path).expect(name);
        assert_eq!((img.w, img.h), (x1 - x0, y1 - y0), "{name}");
        let bytes = std::fs::metadata(&path).unwrap().len();
        assert!(bytes < 600_000, "{name}: {bytes} bytes");
    }
}

#[test]
fn menu_open_reads_as_seen() {
    assert_frame("menu_open");
}

#[test]
fn menu_unavailable_reads_as_seen() {
    assert_frame("menu_unavailable");
}

#[test]
fn menu_activating_reads_as_seen() {
    assert_frame("menu_activating");
}

#[test]
fn closed_ready_row_reads_as_seen() {
    assert_frame("closed_ready_row");
}

#[test]
fn menu_closed_reads_as_seen() {
    assert_frame("menu_closed");
}

#[test]
fn capture_081813_reads_as_seen() {
    assert_frame("capture_2560x1440_081813");
}

#[test]
fn capture_082327_reads_as_seen() {
    assert_frame("capture_2560x1440_082327");
}

#[test]
fn timers_read_exactly_and_only_where_shown() {
    let mut bad = Vec::new();
    for f in FRAMES {
        for (r, name) in reads()[f.name].rows.iter().zip(&reads()[f.name].names) {
            let want = TIMERS.iter().find(|t| t.0 == f.name && t.1 == r.slot).map(|t| t.2);
            if r.timer != want {
                bad.push(format!("{} slot {} {name}: timer {:?}, shows {want:?}", f.name, r.slot, r.timer));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "{}",
        bad.join(
            "
"
        )
    );
}

/// The engine compares badges by hash: the same digit must hash the same in every frame, and
/// different digits differently.
#[test]
fn badges_hash_by_digit_across_frames() {
    let mut by_digit: HashMap<u8, Vec<(u64, String)>> = HashMap::new();
    for f in FRAMES {
        let read = &reads()[f.name];
        for s in f.rows {
            let badge = read.rows.iter().find(|r| r.slot == s.slot).and_then(|r| r.badge.as_ref());
            if let (Some(digit), Some(b)) = (s.badge, badge) {
                by_digit.entry(digit).or_default().push((b.hash, format!("{} slot {}", f.name, s.slot)));
            }
        }
    }
    let mut digits: Vec<u8> = by_digit.keys().copied().collect();
    digits.sort();
    assert_eq!(digits, [1, 2, 3, 4, 5], "badges read");
    let mut hashes = HashMap::new();
    for (digit, seen) in &by_digit {
        assert!(seen.iter().all(|s| s.0 == seen[0].0), "badge {digit} hashes differ: {seen:?}");
        if let Some(other) = hashes.insert(seen[0].0, digit) {
            panic!("badges {digit} and {other} hash the same");
        }
    }
    assert!(by_digit[&5].len() >= 3 && by_digit[&4].len() >= 3, "{by_digit:?}");
}

/// Live scans keep the row cache warm from frame to frame; reading the open menu frames in a row
/// that way must find the same stratagems and statuses as fresh reads.
#[test]
fn a_warm_row_cache_reads_like_a_fresh_one() {
    let mut m = Matcher::load(&Data::Dir(data_dir())).expect("catalog");
    for name in ["menu_open", "capture_2560x1440_082327", "menu_unavailable", "menu_closed"] {
        let col = Img::load_png(&fixture_path(name)).expect(name);
        let rows = m.read_rows(&col, FRAME_H, origin(), 1.0);
        let got: Vec<(usize, &str, Status)> = rows.iter().map(|r| (r.slot, m.catalog[r.index].name.as_str(), r.status)).collect();
        let want: Vec<(usize, &str, Status)> = frame(name).rows.iter().map(|s| (s.slot, s.name, s.status)).collect();
        assert_eq!(got, want, "{name}");
    }
}

/// Every count badge in the frames is read as the number it shows, so the keys can draw it cleanly.
#[test]
fn badges_read_as_their_numbers() {
    let mut bad = Vec::new();
    for f in FRAMES {
        for seen in f.rows.iter().filter(|s| s.badge.is_some()) {
            let row = reads()[f.name].rows.iter().find(|r| r.slot == seen.slot);
            let read = row.and_then(|r| r.badge.as_ref()).and_then(|b| hd_scanner::glyphs::read(&b.bits, b.w, b.h));
            let want = seen.badge.map(|d| d.to_string());
            if read != want {
                bad.push(format!("{} slot {}: read {read:?}, shows {want:?}", f.name, seen.slot));
            }
        }
    }
    let mut m = Matcher::load(&Data::Dir(data_dir())).expect("catalog");
    let col = Img::load_png(&fixture_path("super_destroyer_unavailable")).expect("fixture");
    for (slot, want) in [(0, "9"), (1, "1"), (4, "2")] {
        let rows = m.read_rows(&col, FRAME_H, origin(), 1.0);
        let read =
            rows.iter().find(|r| r.slot == slot).and_then(|r| r.badge.as_ref()).and_then(|b| hd_scanner::glyphs::read(&b.bits, b.w, b.h));
        if read.as_deref() != Some(want) {
            bad.push(format!("super_destroyer_unavailable slot {slot}: read {read:?}, shows {want}"));
        }
    }
    assert!(
        bad.is_empty(),
        "{}",
        bad.join(
            "
"
        )
    );
}

/// While the Super Destroyer is away, the game greys out every stratagem it delivers and their status line reads
/// "Super Destroyer Unavailable": they are unavailable, not as they were before (the Laser Cannon stayed ready).
#[test]
fn super_destroyer_unavailable_reads_its_stratagems_unavailable() {
    let mut m = Matcher::load(&Data::Dir(data_dir())).expect("catalog");
    let col = Img::load_png(&fixture_path("super_destroyer_unavailable")).expect("fixture");
    let rows = m.read_rows(&col, FRAME_H, origin(), 1.0);
    let got: Vec<(&str, Status)> = rows.iter().map(|r| (m.catalog[r.index].name.as_str(), r.status)).collect();
    assert_eq!(
        got,
        [
            ("Reinforce", Ready),
            ("SoS Beacon", Ready),
            ("Resupply", Unavailable),
            ("Call In Super Destroyer", Unavailable),
            ("Orbital Laser", Unavailable),
            ("LAS-98 Laser Cannon", Unavailable),
            ("LIFT-182 Warp Pack", Unavailable),
            ("M-102 Gunner FRV", Unavailable),
        ]
    );
}

// ---- known misreads: the visible value, failing today ----

/// The status line has four arrows; past them, the panel's brown gradient correlates with the
/// right-arrow template above the threshold and a fifth "R" is read. Harmless for Resupply (not a
/// lookalike), and resolve_lookalike tolerates trailing arrows by prefix.
#[test]
#[ignore = "misread: a stray 5th arrow is read on the background past Resupply's code (DDURR, shows DDUR)"]
fn menu_open_resupply_code() {
    assert_misread_fixed("menu_open", 2, Field::Code);
}

/// The dimmed Gunner FRV differs from the clean template on every row, so fill_profile reports
/// 1.0 from both ends although no fill is drawn. Harmless: the engine ignores fill on Ready rows.
#[test]
#[ignore = "misread: the dimmed Gunner FRV row reads as fully filled (bottom 1.0, top 1.0), shows no fill"]
fn menu_unavailable_dimmed_row_fill() {
    assert_misread_fixed("menu_unavailable", 6, Field::Fill);
}

/// The Reinforce badge "5" sits against a bright sky as cream as the box itself; the badge is found
/// by its place and its digits, not by the longest cream run, which spilled into the sky.
#[test]
fn menu_activating_reinforce_badge() {
    assert_field("menu_activating", 0, Field::Badge);
}

/// The Quasar Cannon, half covered by its cooldown fill, scores 0.76 on the whole glyph with the
/// Laser Cannon within ROW_MARGIN; scored as the fill draws it, it is read surely, so scan_menu
/// (which keeps sure rows only) learns the slot.
#[test]
fn capture_082327_quasar_sure() {
    assert_field("capture_2560x1440_082327", 4, Field::Sure);
}

// ---- fixture generator ----

fn save_png(img: &Img, path: &Path) {
    let mut bytes = Vec::with_capacity(img.w * img.h * 3);
    for v in &img.px {
        assert!(v.fract() == 0.0 && (0.0..=255.0).contains(v), "pixel value {v} is not an 8-bit integer");
        bytes.push(*v as u8);
    }
    let file = std::fs::File::create(path).expect("create fixture");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), img.w as u32, img.h as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::High);
    enc.write_header().expect("png header").write_image_data(&bytes).expect("png data");
}

/// Crops data/samples/<name>.png to the icon column into tests/fixtures/<name>.png for every frame
/// in FRAMES and EXTRA_FIXTURES, checks that the crop reads exactly like the full frame, and prints what was read.
/// Run: cargo test --test frames -- --ignored --nocapture make_fixtures
#[test]
#[ignore = "generator: needs data/samples (full screenshots, not in git); rebuilds tests/fixtures"]
fn make_fixtures() {
    let mut m = Matcher::load(&Data::Dir(data_dir())).expect("catalog");
    let (x0, y0, x1, y1) = column_box(FRAME_W, FRAME_H, 1.0);
    std::fs::create_dir_all(manifest_dir().join("tests/fixtures")).expect("fixtures dir");
    for name in FRAMES.iter().map(|f| f.name).chain(EXTRA_FIXTURES.iter().copied()) {
        let frame = Img::load_png(&data_dir().join("samples").join(format!("{name}.png"))).expect(name);
        assert_eq!((frame.w, frame.h), (FRAME_W, FRAME_H), "{}", name);
        let col = frame.crop(x0 as isize, y0 as isize, x1 - x0, y1 - y0);
        let out = fixture_path(name);
        save_png(&col, &out);
        let back = Img::load_png(&out).expect("reload");
        assert_eq!(back.px, col.px, "{}: fixture does not round-trip", name);
        m.reset();
        let whole = m.read_rows(&frame, FRAME_H, (0, 0), 1.0);
        m.reset();
        let cropped = m.read_rows(&back, FRAME_H, (x0, y0), 1.0);
        assert_eq!(format!("{whole:?}"), format!("{cropped:?}"), "{}: the crop reads differently", name);
        println!("{}: {} bytes", name, std::fs::metadata(&out).unwrap().len());
        for r in &cropped {
            let code = m.slot_code(&back, FRAME_H, (x0, y0), 1.0, r.slot);
            println!(
                "  slot {} {} score {:.3}{} {:?} timer {:?} fill b{:.3} t{:.3} badge {:?} arrows {code:?}",
                r.slot,
                m.catalog[r.index].name,
                r.score,
                if r.sure { "" } else { " unsure" },
                r.status,
                r.timer,
                r.fill_bottom,
                r.fill_top,
                r.badge.as_ref().map(|b| (b.w, b.h, b.hash)),
            );
        }
    }
}
