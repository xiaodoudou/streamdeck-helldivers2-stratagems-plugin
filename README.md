<div align="center">

# Helldivers 2 Stratagems for Stream Deck

> Your stratagems on Stream Deck keys, with their cooldowns, called in with one press

[![Build](https://img.shields.io/github/actions/workflow/status/xiaodoudou/streamdeck-helldivers2-stratagems-plugin/test.yml?branch=dev&style=flat-square&label=build)](https://github.com/xiaodoudou/streamdeck-helldivers2-stratagems-plugin/actions/workflows/test.yml) [![GitHub Release](https://img.shields.io/github/v/release/xiaodoudou/streamdeck-helldivers2-stratagems-plugin?style=flat-square&color=e94560)](https://github.com/xiaodoudou/streamdeck-helldivers2-stratagems-plugin/releases/latest) [![License](https://img.shields.io/badge/License-Apache--2.0-blue?style=flat-square)](LICENSE) [![Stars](https://img.shields.io/github/stars/xiaodoudou/streamdeck-helldivers2-stratagems-plugin?style=flat-square&color=yellow)](https://github.com/xiaodoudou/streamdeck-helldivers2-stratagems-plugin/stargazers) [![Windows](https://img.shields.io/badge/Windows-10%20%7C%2011-0078d6?style=flat-square)](docs/INSTALLATION.md#what-you-need) [![Stream Deck](https://img.shields.io/badge/Stream%20Deck-%3E%3D7.1-111111?style=flat-square)](docs/INSTALLATION.md) [![Built with Rust](https://img.shields.io/badge/Built%20with-Rust-dea584?style=flat-square)](docs/HOW_IT_WORKS.md)

A Stream Deck plugin for Helldivers 2. It reads your loadout from the game's screen, shows each stratagem on a key with its cooldown, and calls it in when you press the key, by opening the stratagem menu and typing the arrow code for you.

![The keys during a mission: the game starts, the loadout fills the keys, then the Orbital Laser is called in from its key and a Stratagem Setup key calls in the Quasar Cannon and the Warp Pack, each cooling down after](docs/img/demo.gif)

</div>

---

## What it does

- **Finds your loadout by itself.** On the loadout screen, the keys already fill with the stratagems you pick. When the mission starts, the plugin opens the stratagem menu once and reads it. Each Stratagem Slot key then shows the stratagem at its position in the menu: Reinforce, SOS Beacon, Resupply, your four stratagems, and the ones a mission hands out.
- **Shows what the game shows.** A key fills up from the bottom while its stratagem cools down, fills from the top while a call-in is on its way, and is covered when the stratagem is unavailable. The game's counters come along too, like the reinforcements left or the uses of the Orbital Laser.
- **Calls stratagems in.** Press a key and the plugin enters the code with your own key bindings, in hold or toggle mode. It refuses a stratagem that isn't ready, and never types into another window. A Stratagem Setup key calls two to four of them in with one press, throwing each in turn.
- **Stays out of the way.** While the game is closed it captures nothing and only checks every two seconds whether the game has started. The Scan key turns into a start button that launches the game through Steam.

The plugin only looks at the screen and presses keys, the way you do. It doesn't open the game's process, read its memory or touch its files.

## Getting started

You need Windows 10 or later, the Stream Deck app 7.1 or later, and Helldivers 2 on Steam.

1. Download `com.xiaodoudou.helldivers2-stratagems.streamDeckPlugin` from the [latest release](https://github.com/xiaodoudou/streamdeck-helldivers2-stratagems-plugin/releases/latest) and open it. Stream Deck installs the plugin.
2. In the Stream Deck app, drag *Scan Now*, *Clear Stratagem Slots* and a few *Stratagem Slot* keys from the **Helldivers 2 Stratagems** category onto a profile.
3. Select the Scan key and set the stratagem menu key, the menu mode and the arrow keys to match your bindings in the game.
4. Start a mission.

[docs/USAGE.md](docs/USAGE.md) explains the keys and every setting. To build it yourself, see [docs/INSTALLATION.md](docs/INSTALLATION.md).

## Limits

- The game has to be in English for the keys to show cooldowns: the plugin reads the status words on screen, and only knows the English ones so far. [Other languages](docs/TRANSLATIONS.md) need screenshots, and you can send some.
- It reads the stratagem panel in the top-left corner, so nothing may cover it.
- It has been tested at 2560x1440, with the game's default HUD. Other resolutions are scaled from the window's height.

## Performance

Measured on the development PC, with the game at 2560x1440:

| | |
|---|---|
| One scan | About 13 ms: finding the game under 0.1 ms, capturing 9 ms, reading the rows 4 ms |
| How often | Every 150 ms, and every 50 ms for four seconds after a call-in |
| CPU while the game runs | About 7% of one core (734 ms of CPU time over 10 s on the ship) |
| CPU while the game is closed | Under 1% of one core (94 ms of CPU time over 10 s): nothing is captured, the plugin only checks every two seconds whether the game has started |
| Memory | About 64 MB |
| Download | 1.3 MB |

A scan captures only the column of stratagem icons and the start of their status lines, 332x993 pixels at 2560x1440, about 9% of the screen. The plugin keeps the game's window and process between scans instead of looking them up each time, and redraws a key only when what it shows changes. The scan period is a setting ([USAGE.md](docs/USAGE.md#all-settings)): a longer one costs less and reacts later.

## Documentation

| Page | What it covers |
|---|---|
| [Installation](docs/INSTALLATION.md) | Installing from a release or from the source, updating, and what to do when something goes wrong |
| [How to use](docs/USAGE.md) | Setting up the keys, matching the game's controls, what the keys show, every setting |
| [Game languages](docs/TRANSLATIONS.md) | Why the plugin reads English only, and the screenshots that add a language |
| [How it works](docs/HOW_IT_WORKS.md) | Finding the game, reading the menu, following it over time, the files of the repository |
| [Contributing](CONTRIBUTING.md) | Building, tests, commit messages, branches and releases |
| [Changelog](CHANGELOG.md) | What changed in each version |

## About

A fan project, not made, endorsed or supported by Arrowhead Game Studios, Sony Interactive Entertainment or Elgato.

Helldivers and Helldivers 2, and everything from the game this plugin shows or uses (the stratagem names, icons, codes and status words), belong to Sony Interactive Entertainment and Arrowhead Game Studios; their copyrights and trademarks stay theirs. They are used here only to show which stratagem is which. Stream Deck and Elgato are trademarks of Corsair. The stratagem list, codes and cooldowns were collected from the [Helldivers Wiki](https://helldivers.wiki.gg/wiki/Stratagems). The stratagem icons, which the plugin recognises on screen and shows on the keys, were taken from the game's own files.

The plugin's own code and documentation are under the [Apache License 2.0](LICENSE). That license doesn't cover the game's material.
