"""Update the names, codes and cooldowns of data/stratagems.json from helldivers.wiki.gg.

Each stratagem keeps its "type" and "icon", which name the game's own texture in data/icons/: those come from the
game's files, not from the wiki. It keeps its "also" codes too, the other codes the game gives it on some missions,
which the wiki's list doesn't show (SSSD Delivery's article has both of its codes; the list, one). When the list shows
one of those, the main code stays: which code is the default (typed before the menu has shown which) is chosen here. A stratagem the wiki lists and the file lacks is reported and left out until its
texture is added.
"""
import html
import json
import re
import sys
import urllib.request
from pathlib import Path

BASE = "https://helldivers.wiki.gg"
ROOT = Path(__file__).resolve().parent.parent
CATALOG = ROOT / "data" / "stratagems.json"
ARROW = {"Up": "U", "Down": "D", "Left": "L", "Right": "R"}


def get(url: str) -> bytes:
    req = urllib.request.Request(url, headers={"User-Agent": "streamdeck-helldivers/0.1"})
    with urllib.request.urlopen(req, timeout=30) as r:
        return r.read()


def main() -> int:
    known = {s["name"]: s for s in json.loads(CATALOG.read_text(encoding="utf-8"))}
    page = json.loads(get(f"{BASE}/api.php?action=parse&page=Stratagems&prop=text&format=json"))
    text = page["parse"]["text"]["*"]

    out, new = [], []
    seen = set()
    for row in re.findall(r"<tr>(.*?)</tr>", text, flags=re.S):
        cells = re.findall(r"<td[^>]*>(.*?)</td>", row, flags=re.S)
        if len(cells) < 4:
            continue
        name = html.unescape(re.sub(r"<[^>]+>", "", cells[1]).strip())
        arrows = re.findall(r'alt="Stratagem Arrow (\w+)\.svg"', cells[2])
        if not name or not arrows or name in seen:
            continue
        seen.add(name)
        if name not in known:
            new.append(name)
            continue
        code = "".join(ARROW[a] for a in arrows)
        if code in known[name].get("also", []):  # the list shows one of its other codes: keep the main one chosen
            code = known[name]["code"]
        entry = {
            "name": name,
            "code": code,
            "cooldown": re.sub(r"<[^>]+>", "", cells[3]).strip(),
            "type": known[name]["type"],
            "icon": known[name]["icon"],
        }
        if "also" in known[name]:  # other codes the game gives it on some missions, which the list doesn't show
            entry["also"] = known[name]["also"]
        out.append(entry)

    CATALOG.write_text(json.dumps(out, indent=2), encoding="utf-8", newline="\n")
    print(f"{len(out)} stratagems")
    if gone := sorted(set(known) - seen):
        print("no longer on the wiki, dropped:", ", ".join(gone))
    if new:
        print("new on the wiki, left out until their icon is in data/icons:", ", ".join(new))
    return 0


if __name__ == "__main__":
    sys.exit(main())
