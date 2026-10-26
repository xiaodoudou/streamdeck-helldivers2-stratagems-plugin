//! Integrity of data/stratagems.json and the icons it points to, and of the copy built into the binaries.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use hd_scanner::data::Data;
use hd_scanner::img::Img;
use hd_scanner::matcher::{Matcher, Stratagem};
use hd_scanner::status::CODE_READ;

fn data() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../data")
}

fn entries() -> Vec<Stratagem> {
    let text = std::fs::read_to_string(data().join("stratagems.json")).expect("stratagems.json");
    serde_json::from_str(&text).expect("stratagems.json parses")
}

#[test]
fn every_entry_has_every_field() {
    let all = entries();
    assert!(all.len() > 100, "only {} entries", all.len());
    for s in &all {
        assert!(!s.name.is_empty() && s.name == s.name.trim(), "name {:?}", s.name);
        assert!(!s.code.is_empty() && !s.cooldown.is_empty(), "{}", s.name);
        assert!(s.icon.ends_with(".svg"), "{}: icon {}", s.name, s.icon);
    }
}

#[test]
fn names_are_unique() {
    let mut seen = HashSet::new();
    for s in entries() {
        assert!(seen.insert(s.name.clone()), "{} listed twice", s.name);
    }
}

#[test]
fn codes_use_only_arrows() {
    for s in entries() {
        for code in s.codes() {
            assert!(!code.is_empty() && code.chars().all(|c| "UDLR".contains(c)), "{}: {:?}", s.name, code);
            assert!(code.len() <= 10, "{}: code of {} arrows", s.name, code.len());
        }
    }
}

/// The game gives every stratagem its own code; the one exception shares its icon too, so the
/// scanner treats the two as interchangeable. A stratagem's other codes ("also") count as its own.
#[test]
fn codes_are_unique_except_upload_data_and_reinforcement_pods() {
    let mut by_code: HashMap<String, Vec<String>> = HashMap::new();
    for s in entries() {
        for code in s.codes() {
            by_code.entry(code.to_string()).or_default().push(s.name.clone());
        }
    }
    let mut shared: Vec<Vec<String>> = by_code.into_values().filter(|v| v.len() > 1).collect();
    for v in &mut shared {
        v.sort();
    }
    assert_eq!(shared, [["Reinforcement Pods", "Upload Data"]]);
}

#[test]
fn cooldowns_are_seconds_or_not_applicable() {
    for s in entries() {
        let ok = s.cooldown == "N/A" || s.cooldown.strip_suffix('s').is_some_and(|n| n.parse::<u32>().is_ok());
        assert!(ok, "{}: cooldown {:?}", s.name, s.cooldown);
    }
}

/// Each icon is the game's own texture for it, as taken from the game's files: 256x256, its red channel marking the
/// parts drawn in the type's colour and its green the cream parts. Blue is unused, but for a trace of compression.
#[test]
fn every_icon_is_a_game_texture_with_both_parts() {
    let dir = data().join("icons");
    for s in entries() {
        let img = Img::load_png(&dir.join(s.png())).unwrap_or_else(|e| panic!("{}: {e}", s.name));
        assert_eq!((img.w, img.h), (256, 256), "{}", s.name);
        let channel = |c: usize| img.px.iter().skip(c).step_by(3);
        // Matcher::load silently drops entries whose texture is all black
        assert!(channel(0).any(|&v| v > 128.0) && channel(1).any(|&v| v > 128.0), "{}: no coloured or no cream part", s.name);
        assert!(channel(2).all(|&v| v <= 40.0), "{}: blue in use, not a stratagem texture", s.name);
    }
}

#[test]
fn every_icon_file_belongs_to_a_stratagem() {
    let used: HashSet<String> = entries().iter().map(|s| s.png()).collect();
    for entry in std::fs::read_dir(data().join("icons")).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        assert!(used.contains(&name), "icons/{name}: no stratagem uses it");
    }
}

#[test]
fn the_matcher_keeps_every_entry_in_order() {
    let m = Matcher::load(&Data::Dir(data())).expect("catalog");
    let names: Vec<String> = entries().into_iter().map(|s| s.name).collect();
    let loaded: Vec<String> = m.catalog.iter().map(|s| s.name.clone()).collect();
    assert_eq!(loaded, names);
}

/// Lookalike groups by name, from Matcher::lookalike_groups ("Name (CODE)" strings).
fn lookalike_groups(m: &Matcher) -> Vec<Vec<String>> {
    m.lookalike_groups()
        .into_iter()
        .map(|g| g.into_iter().map(|e| e.rsplit_once(" (").expect("Name (CODE)").0.to_string()).collect())
        .collect()
}

#[test]
fn entries_sharing_an_icon_are_lookalikes() {
    let m = Matcher::load(&Data::Dir(data())).expect("catalog");
    let groups = lookalike_groups(&m);
    let group_of = |name: &str| groups.iter().position(|g| g.iter().any(|n| n == name));
    let mut by_icon: HashMap<String, Vec<String>> = HashMap::new();
    for s in entries() {
        by_icon.entry(s.icon).or_default().push(s.name);
    }
    for (icon, names) in by_icon.iter().filter(|(_, v)| v.len() > 1) {
        let first = group_of(&names[0]);
        assert!(first.is_some(), "{icon}: {names:?} not in any lookalike group");
        for n in names {
            assert_eq!(group_of(n), first, "{icon}: {names:?} split across groups");
        }
    }
    // the groups known when this test was written, by name
    let known: [&[&str]; 3] = [
        &["SSSD Delivery", "Upload Data", "Reinforcement Pods"],
        &["Hive Breaker Drill", "Activate E-711 Extraction Drill", "Prospecting Drill", "Aquifer Drill"],
        &["Portable Comms Relay", "Cargo Container"],
    ];
    for k in known {
        let g = group_of(k[0]).map(|i| &groups[i]).expect(k[0]);
        assert!(k.iter().all(|n| g.iter().any(|x| x == n)), "{k:?} vs {g:?}");
    }
}

/// resolve_lookalike reads at most CODE_READ arrows and accepts a read that is a prefix of a code
/// (or the other way round): lookalikes must have codes within that length that never fit each other,
/// whichever of its codes each shows.
#[test]
fn lookalike_codes_can_be_told_apart_by_the_code_reader() {
    let m = Matcher::load(&Data::Dir(data())).expect("catalog");
    let codes: HashMap<String, Vec<String>> = m.catalog.iter().map(|s| (s.name.clone(), s.codes().map(str::to_string).collect())).collect();
    for g in lookalike_groups(&m) {
        for a in &g {
            for ca in &codes[a] {
                assert!(ca.len() <= CODE_READ, "{a}: {} arrows, the reader stops at {CODE_READ}", ca.len());
                for b in g.iter().filter(|b| *b != a && codes[*b] != codes[a]) {
                    for cb in &codes[b] {
                        let fits = cb.starts_with(ca.as_str()) || ca.starts_with(cb.as_str());
                        assert!(!fits, "{a} ({ca}) and {b} ({cb}) cannot be told apart");
                    }
                }
            }
        }
    }
}

/// What the plugin uses from data/: the stratagem list and every file in icons/ and status/.
fn data_files() -> Vec<(String, Vec<u8>)> {
    let mut files = vec![("stratagems.json".to_string(), std::fs::read(data().join("stratagems.json")).unwrap())];
    for dir in ["icons", "status"] {
        for entry in std::fs::read_dir(data().join(dir)).unwrap() {
            let path = entry.unwrap().path();
            files.push((format!("{dir}/{}", path.file_name().unwrap().to_str().unwrap()), std::fs::read(&path).unwrap()));
        }
    }
    files.sort();
    files
}

#[test]
fn the_built_in_data_is_exactly_the_data_folder() {
    let built_in: Vec<(String, Vec<u8>)> = Data::embedded_files().iter().map(|&(name, bytes)| (name.to_string(), bytes.to_vec())).collect();
    let on_disk = data_files();
    let names = |files: &[(String, Vec<u8>)]| files.iter().map(|f| f.0.clone()).collect::<Vec<_>>();
    assert_eq!(names(&built_in), names(&on_disk), "built in and in data/ (a new picture builds in on the next build)");
    for ((name, a), (_, b)) in built_in.iter().zip(&on_disk) {
        assert!(a == b, "{name}: built in differs from data/");
    }
    assert!(built_in.len() > 100, "{} files", built_in.len());
}

#[test]
fn the_matcher_reads_the_same_catalog_built_in() {
    let (built_in, on_disk) = (Matcher::load(&Data::Embedded).expect("built in"), Matcher::load(&Data::Dir(data())).expect("data/"));
    let fields = |m: &Matcher| {
        m.catalog.iter().map(|s| (s.name.clone(), s.code.clone(), s.cooldown.clone(), s.kind, s.icon.clone())).collect::<Vec<_>>()
    };
    assert_eq!(fields(&built_in), fields(&on_disk));
    assert_eq!(built_in.lookalike_groups(), on_disk.lookalike_groups());
}
