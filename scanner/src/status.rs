//! Reads a menu row's status line: the arrow code when it can be called, otherwise a state word,
//! and after "Cooldown" or "Inbound" the timer (T-MM:SS).
//! Templates are greyscale cuts from 2560x1440 frames (tools/make_status_templates.py).
//! The words are the English HUD text; other game languages need their own cuts.

use crate::data::Data;
use crate::img::Img;

const BAND_Y: f64 = 34.0; // status line top, below the row's icon top
const WORD_X: f64 = 154.0;
const ARROW_X: f64 = 155.0;
const SLACK: isize = 3; // position tolerance in pixels at 1440p
const STATUS_MIN: f32 = 0.55;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Status {
    Ready,
    Cooldown,
    Unavailable,
    Active, // "Activating" / "Inbound" / "Impact": already called in and on its way
    Unknown,
}

const ARROW_STEP: f64 = 29.0; // distance between arrows of a code at 1440p
const ARROW_W: f64 = 26.0;
// arrows read when lookalike icons need the code to tell them apart: the Rover and K-9 backpacks
// only differ at the 6th, and the longest code (Aquifer Drill) has 8
pub const CODE_READ: usize = 8;

// The timer's digits sit in fixed cells after the word: the first one TIMER_AFTER_* px right of where
// the word starts, the others DIGIT_CELLS further (the colon between minutes and seconds), at 1440p.
const TIMER_AFTER_COOLDOWN: f64 = 133.0; // "Cooldown T-"
const TIMER_AFTER_INBOUND: f64 = 117.0; // "Inbound T-"
const DIGIT_CELLS: [f64; 4] = [0.0, 13.0, 31.0, 44.0];
const DIGIT_MIN: f32 = 0.7; // a digit's correlation, and its lead over the next best digit
const DIGIT_MARGIN: f32 = 0.03;

struct Template {
    status: Status,
    dir: Option<char>,  // arrow templates: the direction letter
    timer: Option<f64>, // word templates followed by a timer: where its first digit starts, past the word
    digit: Option<u32>, // digit templates: the digit
    x: f64,
    w: usize,
    h: usize,
    v: Vec<f32>, // zero-mean, unit norm
}

/// A template as cut, before scaling to the game's size.
struct Raw {
    status: Status,
    dir: Option<char>,
    timer: Option<f64>,
    digit: Option<u32>,
    x: f64,
    img: Img,
}

pub struct StatusReader {
    raw: Vec<Raw>,
    scaled: Option<(usize, Vec<Template>)>, // keyed by icon size
}

/// What a row's status line says.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Line {
    pub status: Status,
    pub score: f32,
    /// The timer after "Cooldown" or "Inbound", in seconds, when all four digits read clearly.
    pub timer: Option<u32>,
    /// Ready because the line shows the code's arrows (the menu is open), not the word "Ready": worth reading them.
    pub arrows: bool,
}

/// Right edge (at 1440p) the capture must reach for the status words and the first CODE_READ arrows.
pub const NEEDS_X: f64 = ARROW_X + ARROW_STEP * (CODE_READ - 1) as f64 + ARROW_W + SLACK as f64 + 1.0;

fn gray(img: &Img) -> Vec<f32> {
    img.px.as_chunks::<3>().0.iter().map(|p| 0.299 * p[0] + 0.587 * p[1] + 0.114 * p[2]).collect()
}

impl StatusReader {
    /// The templates in the data's status/ folder; any missing is left out.
    pub fn load(data: &Data) -> StatusReader {
        let table = [
            ("word_ready", Status::Ready, None, None, WORD_X),
            ("word_cooldown", Status::Cooldown, None, Some(TIMER_AFTER_COOLDOWN), WORD_X),
            ("word_unavailable", Status::Unavailable, None, None, WORD_X),
            // "Super Destroyer Unavailable": the Super Destroyer is away, so its stratagems can't be called
            ("word_super_destroyer", Status::Unavailable, None, None, WORD_X),
            ("word_impact", Status::Active, None, None, WORD_X),
            ("word_inbound", Status::Active, None, Some(TIMER_AFTER_INBOUND), WORD_X),
            ("word_activating", Status::Active, None, None, WORD_X),
            ("arrow_up", Status::Ready, Some('U'), None, ARROW_X),
            ("arrow_down", Status::Ready, Some('D'), None, ARROW_X),
            ("arrow_left", Status::Ready, Some('L'), None, ARROW_X),
            ("arrow_right", Status::Ready, Some('R'), None, ARROW_X),
        ];
        let load = |name: &str| data.png(&format!("status/{name}.png")).ok();
        let mut raw: Vec<Raw> = table
            .iter()
            .filter_map(|&(name, status, dir, timer, x)| load(name).map(|img| Raw { status, dir, timer, digit: None, x, img }))
            .collect();
        raw.extend((0..10).filter_map(|d| {
            load(&format!("digit_{d}")).map(|img| Raw { status: Status::Unknown, dir: None, timer: None, digit: Some(d), x: 0.0, img })
        }));
        StatusReader { raw, scaled: None }
    }

    fn prepare(&mut self, f: f64, size: usize) {
        if self.scaled.as_ref().is_some_and(|s| s.0 == size) {
            return;
        }
        let tpls = self
            .raw
            .iter()
            .map(|r| {
                let (w, h) = (((r.img.w as f64) * f).round().max(4.0) as usize, ((r.img.h as f64) * f).round().max(4.0) as usize);
                let mut v = gray(&r.img.resize_area(w, h));
                let mean = v.iter().sum::<f32>() / v.len() as f32;
                v.iter_mut().for_each(|p| *p -= mean);
                let n = v.iter().map(|p| p * p).sum::<f32>().sqrt().max(1e-3);
                v.iter_mut().for_each(|p| *p /= n);
                Template { status: r.status, dir: r.dir, timer: r.timer, digit: r.digit, x: r.x, w, h, v }
            })
            .collect();
        self.scaled = Some((size, tpls));
    }

    /// The status line of the row whose icon top is at `top` (1440p units), in a capture starting at `origin`.
    pub fn read(&mut self, col: &Img, f: f64, size: usize, top: f64, origin: (usize, usize)) -> Line {
        self.prepare(f, size);
        let Some((_, tpls)) = &self.scaled else { return Line { status: Status::Unknown, score: 0.0, timer: None, arrows: false } };
        let mut best: Option<(&Template, f32, isize, isize)> = None;
        for t in tpls.iter().filter(|t| t.digit.is_none()) {
            let (c, x, y) = near(col, t, at(f, t.x, top, origin), slack(f));
            if c > best.map_or(STATUS_MIN, |b| b.1) {
                best = Some((t, c, x, y));
            }
        }
        let Some((t, score, x, y)) = best else { return Line { status: Status::Unknown, score: STATUS_MIN, timer: None, arrows: false } };
        let timer = t.timer.and_then(|after| read_timer(col, f, tpls, (x + (after * f).round() as isize, y)));
        Line { status: t.status, score, timer, arrows: t.dir.is_some() }
    }

    /// The arrow code shown on the status line, as "UDLR" letters, up to `max` arrows; stops at the
    /// first position without a clear arrow (none at all when the row shows a word instead).
    pub fn read_arrows(&mut self, col: &Img, f: f64, size: usize, top: f64, origin: (usize, usize), max: usize) -> String {
        self.prepare(f, size);
        let Some((_, tpls)) = &self.scaled else { return String::new() };
        let mut code = String::new();
        for i in 0..max {
            let x = ARROW_X + ARROW_STEP * i as f64;
            let best =
                tpls.iter().filter_map(|t| t.dir.map(|d| (d, best_at(col, f, t, x, top, origin)))).max_by(|a, b| a.1.total_cmp(&b.1));
            match best {
                Some((d, c)) if c >= STATUS_MIN => code.push(d),
                _ => break,
            }
        }
        code
    }
}

/// The timer's seconds, its first digit's cell starting at `first` (capture pixels, placed after where
/// the word matched): each of the four cells must hold one digit clearly ahead of the others, and the
/// tens of seconds be 0-5.
fn read_timer(col: &Img, f: f64, tpls: &[Template], first: (isize, isize)) -> Option<u32> {
    let mut d = [0u32; 4];
    for (cell, digit) in DIGIT_CELLS.iter().zip(d.iter_mut()) {
        let x = first.0 + (cell * f).round() as isize;
        let mut scores: Vec<(f32, u32)> = tpls.iter().filter_map(|t| t.digit.map(|n| (near(col, t, (x, first.1), 1).0, n))).collect();
        scores.sort_by(|a, b| b.0.total_cmp(&a.0));
        let (best, second) = (scores.first()?, scores.get(1).map_or(-1.0, |s| s.0));
        if best.0 < DIGIT_MIN || best.0 - second < DIGIT_MARGIN {
            return None;
        }
        *digit = best.1;
    }
    (d[2] <= 5).then_some((d[0] * 10 + d[1]) * 60 + d[2] * 10 + d[3])
}

fn slack(f: f64) -> isize {
    ((SLACK as f64) * f).round().max(1.0) as isize
}

/// Capture pixel where a template cut at `x` (1440p) on the status line of the row at `top` starts.
fn at(f: f64, x: f64, top: f64, origin: (usize, usize)) -> (isize, isize) {
    ((x * f).round() as isize - origin.0 as isize, ((top + BAND_Y) * f).round() as isize - origin.1 as isize)
}

/// Best normalised correlation of template `t` placed at `x` (1440p) on the status line, within the slack.
fn best_at(col: &Img, f: f64, t: &Template, x: f64, top: f64, origin: (usize, usize)) -> f32 {
    near(col, t, at(f, x, top, origin), slack(f)).0
}

/// Best normalised correlation of template `t` with its top-left within `slack` pixels of `pos`
/// (capture pixels), and where that was.
fn near(col: &Img, t: &Template, pos: (isize, isize), slack: isize) -> (f32, isize, isize) {
    let (x, y) = (pos.0 - slack, pos.1 - slack);
    let win = col.crop(x, y, t.w + 2 * slack as usize, t.h + 2 * slack as usize);
    if win.w < t.w || win.h < t.h {
        return (-1.0, pos.0, pos.1);
    }
    let (x, y) = (x.max(0), y.max(0)); // where the window starts once clamped to the capture
    let g = gray(&win);
    let mut best = (-1.0f32, pos.0, pos.1);
    for dy in 0..=win.h - t.h {
        for dx in 0..=win.w - t.w {
            let (mut s, mut ss, mut d) = (0.0f32, 0.0f32, 0.0f32);
            for r in 0..t.h {
                let row = &g[(dy + r) * win.w + dx..(dy + r) * win.w + dx + t.w];
                let tr = &t.v[r * t.w..(r + 1) * t.w];
                for (p, q) in row.iter().zip(tr) {
                    s += p;
                    ss += p * p;
                    d += p * q;
                }
            }
            let n = (t.w * t.h) as f32;
            let var = ss - s * s / n;
            if var > 1e-3 && d / var.sqrt() > best.0 {
                best = (d / var.sqrt(), x + dx as isize, y + dy as isize);
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    //! Status lines drawn from the templates themselves onto a flat background.

    use std::path::{Path, PathBuf};

    use super::*;

    const TOP: f64 = 200.0; // icon top of the test row, 1440p units

    fn dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../data/status")
    }

    fn reader() -> StatusReader {
        StatusReader::load(&Data::Dir(dir().join("..")))
    }

    fn template(name: &str) -> Img {
        Img::load_png(&dir().join(format!("{name}.png"))).unwrap()
    }

    /// A flat capture covering the status line of the row at TOP, for a game frame `f` times 1440p.
    fn line(f: f64) -> Img {
        let (w, h) = ((420.0 * f) as usize, ((TOP + 80.0) * f) as usize);
        let mut img = Img::new(w, h);
        img.px.iter_mut().for_each(|v| *v = 35.0);
        img
    }

    /// Draw a template scaled by `f` with its top-left at (x, TOP + BAND_Y + dy) in 1440p units.
    fn draw(img: &mut Img, name: &str, x: f64, dy: isize, f: f64) {
        let t = template(name);
        let t = t.resize_area(((t.w as f64) * f).round() as usize, ((t.h as f64) * f).round() as usize);
        let (x0, y0) = ((x * f).round() as usize, (((TOP + BAND_Y) * f).round() as isize + dy) as usize);
        for y in 0..t.h {
            for x in 0..t.w {
                let (s, d) = ((y * t.w + x) * 3, ((y0 + y) * img.w + x0 + x) * 3);
                img.px[d..d + 3].copy_from_slice(&t.px[s..s + 3]);
            }
        }
    }

    fn draw_code(img: &mut Img, code: &str, f: f64) {
        for (i, c) in code.chars().enumerate() {
            let name = match c {
                'U' => "arrow_up",
                'D' => "arrow_down",
                'L' => "arrow_left",
                _ => "arrow_right",
            };
            draw(img, name, ARROW_X + ARROW_STEP * i as f64, 0, f);
        }
    }

    fn size(f: f64) -> usize {
        (57.0 * f).round() as usize
    }

    #[test]
    fn capture_width_reaches_the_last_code_arrow() {
        assert_eq!(NEEDS_X, 388.0);
        assert!(NEEDS_X >= WORD_X + template("word_unavailable").w as f64 + SLACK as f64);
    }

    #[test]
    fn loads_every_template() {
        let r = reader();
        assert_eq!(r.raw.len(), 21);
        assert_eq!(r.raw.iter().filter(|t| t.dir.is_some()).count(), 4, "four arrows");
        assert_eq!(r.raw.iter().filter(|t| t.status == Status::Unavailable).count(), 2, "unavailable, super destroyer unavailable");
        assert_eq!(r.raw.iter().filter(|t| t.status == Status::Active).count(), 3, "impact, inbound and activating");
        assert_eq!(r.raw.iter().filter_map(|t| t.digit).collect::<Vec<_>>(), (0..10).collect::<Vec<_>>(), "ten digits");
        assert_eq!(r.raw.iter().filter(|t| t.timer.is_some()).count(), 2, "cooldown and inbound have a timer");
    }

    #[test]
    fn missing_templates_read_unknown() {
        let mut r = StatusReader::load(&Data::Dir("no/such/dir".into()));
        let mut img = line(1.0);
        draw(&mut img, "word_ready", WORD_X, 0, 1.0);
        assert_eq!(r.read(&img, 1.0, size(1.0), TOP, (0, 0)).status, Status::Unknown);
        assert_eq!(r.read_arrows(&img, 1.0, size(1.0), TOP, (0, 0), CODE_READ), "");
    }

    #[test]
    fn an_empty_line_reads_unknown() {
        let mut r = reader();
        let img = line(1.0);
        assert_eq!(r.read(&img, 1.0, size(1.0), TOP, (0, 0)).status, Status::Unknown);
        assert_eq!(r.read_arrows(&img, 1.0, size(1.0), TOP, (0, 0), CODE_READ), "");
    }

    #[test]
    fn each_word_reads_as_its_status() {
        for (word, status) in [
            ("word_ready", Status::Ready),
            ("word_cooldown", Status::Cooldown),
            ("word_unavailable", Status::Unavailable),
            ("word_impact", Status::Active),
            ("word_inbound", Status::Active),
            ("word_activating", Status::Active),
        ] {
            for f in [1.0, 0.75, 1.5] {
                let mut r = reader();
                let mut img = line(f);
                draw(&mut img, word, WORD_X, 0, f);
                let Line { status: got, score, .. } = r.read(&img, f, size(f), TOP, (0, 0));
                assert_eq!(got, status, "{word} at scale {f} (score {score})");
            }
        }
    }

    #[test]
    fn a_word_off_by_the_slack_still_reads() {
        for dy in [-SLACK, SLACK] {
            let mut r = reader();
            let mut img = line(1.0);
            draw(&mut img, "word_cooldown", WORD_X + SLACK as f64, dy, 1.0);
            assert_eq!(r.read(&img, 1.0, size(1.0), TOP, (0, 0)).status, Status::Cooldown, "dy {dy}");
        }
    }

    #[test]
    fn arrows_read_as_the_code_and_count_as_ready() {
        for code in ["UDRU", "DDUR", "LDRDRDU", "LLLUDRDD", "RRU"] {
            for f in [1.0, 0.75] {
                let mut r = reader();
                let mut img = line(f);
                draw_code(&mut img, code, f);
                assert_eq!(r.read_arrows(&img, f, size(f), TOP, (0, 0), CODE_READ), code, "scale {f}");
                assert_eq!(r.read(&img, f, size(f), TOP, (0, 0)).status, Status::Ready, "{code} at scale {f}");
            }
        }
    }

    #[test]
    fn arrow_reading_stops_at_max_and_at_the_first_gap() {
        let mut r = reader();
        let mut img = line(1.0);
        draw_code(&mut img, "LDRDRDU", 1.0);
        assert_eq!(r.read_arrows(&img, 1.0, size(1.0), TOP, (0, 0), 3), "LDR");
        assert_eq!(r.read_arrows(&img, 1.0, size(1.0), TOP, (0, 0), 0), "");
        let mut img = line(1.0);
        draw_code(&mut img, "UD", 1.0);
        draw(&mut img, "arrow_right", ARROW_X + ARROW_STEP * 3.0, 0, 1.0); // after an empty position
        assert_eq!(r.read_arrows(&img, 1.0, size(1.0), TOP, (0, 0), CODE_READ), "UD");
    }

    #[test]
    fn the_capture_origin_is_honoured() {
        let mut r = reader();
        let mut full = line(1.0);
        draw_code(&mut full, "DLRD", 1.0);
        let origin = (56, 43);
        let col = full.crop(origin.0 as isize, origin.1 as isize, full.w - origin.0, full.h - origin.1);
        assert_eq!(r.read_arrows(&col, 1.0, size(1.0), TOP, origin, CODE_READ), "DLRD");
    }

    /// A status line showing `word` then the timer `mm:ss` (digits only; a blank keeps its cell empty),
    /// each piece where the game draws it, `dx` px (1440p) off for the whole line.
    fn draw_timer_line(img: &mut Img, word: &str, after: f64, mm_ss: &str, dx: f64, f: f64) {
        draw(img, word, WORD_X + dx, 0, f);
        for (cell, c) in DIGIT_CELLS.iter().zip(mm_ss.chars().filter(|c| *c != ':')) {
            if c != ' ' {
                draw(img, &format!("digit_{c}"), WORD_X + dx + after + cell, 0, f);
            }
        }
    }

    #[test]
    fn the_timer_after_cooldown_and_inbound_reads_in_seconds() {
        for (word, after, status) in
            [("word_cooldown", TIMER_AFTER_COOLDOWN, Status::Cooldown), ("word_inbound", TIMER_AFTER_INBOUND, Status::Active)]
        {
            for (shown, secs) in [("04:50", 290), ("00:07", 7), ("12:34", 754), ("09:59", 599), ("00:00", 0), ("38:16", 2296)] {
                for f in [1.0, 0.75, 1.5] {
                    for dx in [0.0, 2.0] {
                        let mut r = reader();
                        let mut img = line(f);
                        draw_timer_line(&mut img, word, after, shown, dx, f);
                        let got = r.read(&img, f, size(f), TOP, (0, 0));
                        assert_eq!((got.status, got.timer), (status, Some(secs)), "{word} T-{shown} at scale {f}, {dx} px off");
                    }
                }
            }
        }
    }

    #[test]
    fn no_timer_without_one_on_screen_or_after_other_words() {
        let read = |word: &str, after: f64, shown: &str| {
            let mut r = reader();
            let mut img = line(1.0);
            draw_timer_line(&mut img, word, after, shown, 0.0, 1.0);
            r.read(&img, 1.0, size(1.0), TOP, (0, 0))
        };
        assert_eq!(read("word_cooldown", TIMER_AFTER_COOLDOWN, "    ").timer, None, "the word alone");
        assert_eq!(read("word_cooldown", TIMER_AFTER_COOLDOWN, "04: 0").timer, None, "a digit missing");
        assert_eq!(read("word_cooldown", TIMER_AFTER_COOLDOWN, "04:70").timer, None, "70 seconds is no time");
        for word in ["word_ready", "word_impact", "word_activating", "word_unavailable"] {
            let got = read(word, TIMER_AFTER_COOLDOWN, "04:50");
            assert!(got.status != Status::Unknown && got.timer.is_none(), "{word}: {got:?}");
        }
    }
}
