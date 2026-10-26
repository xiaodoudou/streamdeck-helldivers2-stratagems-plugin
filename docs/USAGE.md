← [Documentation](README.md)

# How to use

This page assumes the plugin is installed ([INSTALLATION.md](INSTALLATION.md)). It covers setting up the keys, the settings, and what the keys show while you play.

## Setting up the keys

In the Stream Deck app, the plugin adds a category named **Helldivers 2 Stratagems** with four actions. Drag them onto a profile:

| Action | What it is for |
|---|---|
| Scan Now | The Scan key. Its animated radar shows whether the game is being read, and pressing it reads the stratagem menu right away. Its settings panel holds the settings of the whole plugin. |
| Stratagem Slot | One stratagem. Add one per stratagem you want on the deck. |
| Clear Stratagem Slots | Empties every slot. They fill again on the next scan. |
| Stratagem Setup | Two to four of your loadout stratagems on one key: a press calls them in one after the other and throws each. See [Stratagem Setup](#stratagem-setup). |

A slot shows the stratagem at the same position in the game's stratagem menu. The menu always starts with Reinforce, SOS Beacon and Resupply, so:

| Slot | Stratagem |
|---|---|
| 1 | Reinforce |
| 2 | SOS Beacon |
| 3 | Resupply |
| 4 to 7 | Your four loadout stratagems, in menu order. A mission that starts with stratagems of its own, like Call In Super Destroyer, lists those first, which moves yours along |
| After them | Stratagems a mission hands out, like the Hellbomb or the SSSD Delivery |

Each new Stratagem Slot key takes the first free slot. To change it, select the key and pick another number in its settings. The same panel has *Show name*, which writes the stratagem's name under its icon (off by default).

## Matching the game's controls

The plugin calls a stratagem in the way you would: it opens the stratagem menu and presses the arrow keys of the code. So its inputs have to match your key bindings in the game. Select the Scan key and set them in its settings panel; each change is saved as you make it, and a value out of range is outlined in red with the reason:

- *Stratagem menu* is the key that opens the menu (Left Ctrl by default).
- *Menu mode* must match the game's own option for the stratagem menu: *Hold while entering* if you hold the key, *Tap to toggle* if a tap opens the menu and the next one closes it.
- *Up*, *Down*, *Left* and *Right* are the keys of the codes (W, S, A and D by default; the arrow keys work as well if you bound them).
- *Throw* is the key or mouse button you throw a stratagem with (the left mouse button by default in the game). Stratagem Setup keys need it, since they throw each stratagem before calling the next.
- *Throw after calling in* makes the slot keys throw too, right after the code (off by default). A configuration from before 0.5.2 with a throw key set has it on, since setting the key was what made the keys throw then.

To set an input, click its box, then press the key or mouse button, anywhere in the panel. The boxes take the letters and digits, the arrow keys, Home, End, Insert, Delete, Page Up and Page Down, the function keys and the five mouse buttons, so you can match whatever you have bound in the game. Backspace clears the throw key.

## Playing

Start the game, or press the Scan key while the game is closed: it shows a play button then, and starts the game through Steam.

Before a mission, the plugin watches for your card on the loadout screen, the one with your helldiver and the *Mission Stratagems* rows. As soon as it's on screen, the slot keys fill with what the stratagem menu will start the mission with, in its order: Reinforce, SOS Beacon, Resupply, then the four you chose. They show as unavailable until the mission starts, since nothing can be called in before, and they change if you pick other stratagems. The mission's own stratagems, which the card lists after Resupply, are left out: most of them, like the Hellbomb or the SSSD Delivery, only join the menu later in the mission. When a mission does start with one, such as Call In Super Destroyer, the menu read at landing adds it and moves your four along by one key.

When a mission starts, the plugin opens the stratagem menu to read the loadout, even when the loadout screen already gave it: the menu shows what each stratagem's state is, and settles any slot the loadout screen got wrong. The HUD shows up before you can open the menu (in the hellpod, while landing), so the plugin checks whether the menu really opened, and tries again until it does, for about twenty seconds. After the loadout screen, it presses the menu key as soon as the HUD shows Reinforce and tries every second; otherwise, for instance when the plugin starts during a mission, it waits two seconds first and tries every two seconds. If you open the menu yourself in the meantime, that counts too. After that, it follows what the HUD shows with the menu closed. Opening the menu yourself, or pressing the Scan key, reads everything again.

Press a slot key to call its stratagem in. The game has to be the window in front; if it isn't, the key shows a warning triangle and nothing is typed into the other window. The plugin also refuses a stratagem the game would refuse (cooling down, unavailable or already on its way), except in the very last moment of a cooldown: the last 0.3 s when it read the game's timer, the last 1.5 s when it had to estimate the end.

### Stratagem Setup

A Stratagem Setup key calls several stratagems in with one press, such as a sentry and the supply pack that goes with it. In its settings, pick two to four stratagems of your loadout, numbered in the order you chose them: *Loadout 1* is the first. A new key takes the first two. The plugin finds them in the stratagem menu wherever they are. It knows your loadout from your card on the loadout screen, looks for the first of your stratagems in the menu, and takes the others in order after it, so the stratagems a mission puts before yours don't throw it off, even one the plugin can't read. If the plugin only started during the mission, it hasn't seen the loadout screen: it then counts your stratagems in the menu, skipping the ones every mission gives.

Pressing it calls each slot's stratagem in, in the order chosen, and throws it before the next, since you can only hold one at a time. So it needs the *Throw* input, set on the Scan key; without it, the key shows a warning triangle and nothing is typed. A stratagem that isn't ready (cooling down, unavailable, or already on its way) is skipped, and the others are called in. Between two stratagems, the plugin waits a second after the throw, so that the throw has ended when it opens the menu again.

The key shows its stratagems' icons side by side for two, and in a square for three or four, each with its own cooldown, like the slot keys. Until the plugin has read your loadout, each place shows the signal icon of a slot not read yet.

### What a slot key shows

| The key shows | Meaning |
|---|---|
| A signal icon, filling in | The slot hasn't been read yet |
| The stratagem's icon | Ready |
| A beige fill rising from the bottom, going down as time passes | Cooling down. The fill shows how much of the cooldown is left |
| A fill coming down from the top | On its way (activating, inbound or impact) |
| The icon covered completely | Unavailable, or read from the loadout screen and waiting for the mission |
| A number in the bottom-right corner | The count the game shows, like the reinforcements left or the uses of a stratagem |
| Black | Nothing in that slot this mission |

With the menu open, the plugin reads the game's own timer (`Cooldown T-01:23`), and the key counts down to it exactly: it turns ready when the timer reaches zero, not before. With the menu closed, the game shows no timer. The plugin then counts the cooldown down itself, from the timer it read last, from the length it learned earlier in the session, or from the [wiki](https://helldivers.wiki.gg/wiki/Stratagems) figure, and corrects it as soon as the game shows the row again.

### What the Scan key shows

| The key shows | Meaning |
|---|---|
| A yellow play button | The game isn't running. Press it to start the game, or to start reading straight away if the game was just started |
| A grey radar, not moving | The game is running, but its window can't be read (minimised, or still starting) |
| A yellow radar | The game is open with no mission HUD: the ship, a menu or a loading screen |
| A green radar | In a mission |

After about ten seconds on yellow, the slots go back to the signal icon, ready for the next loadout.

While the game isn't running, the plugin doesn't capture the screen at all. It only checks every two seconds whether the game has started, and the Clear key and the empty slots turn grey and stop moving.

## All settings

They are on the Scan key's settings panel and apply to every key.

| Setting | Default | What it does |
|---|---|---|
| Auto scan | On (checked) | Reads the screen every *Scan period*. Unchecked: only the Scan key reads it |
| Open the menu at mission start | On (checked) | Reads the loadout when a mission starts, as described above |
| Scan period | 0.15 s | Time between two reads (0.1 to 60 s). For a few seconds after you call a stratagem in, the plugin reads every 50 ms to catch the change |
| Confirm after | 2 | How many reads in a row must see a stratagem before its key shows it (1 to 10) |
| Icon scale | 1 | Only for a HUD scale other than the game's default (0.5 to 2) |
| Game resolution | auto | The game's size in pixels. *auto* measures the game's window, and the line under the box shows what was found. Type a size such as `2560x1440` only when that is wrong: when Windows hides the game's window from the plugin, the plugin falls back to the main screen's size, which differs from the game's if the game doesn't run at the screen's resolution |
| Stratagem menu, Menu mode, Up, Down, Left, Right, Throw, Throw after calling in | Left Ctrl, Hold, W, S, A, D, none, off | See [Matching the game's controls](#matching-the-games-controls) |
| Menu open delay | 120 ms | Wait after opening the menu, before the first arrow |
| Scan menu wait | 350 ms | Added to *Menu open delay*: the longest the plugin waits for a menu it opened to show. It reads the menu as soon as it has settled, and closes it right away |
| Key hold | 35 ms | How long each key is held down |
| Delay between keys | 40 ms | Pause between two arrows |

If the game misses arrows, raise *Key hold* and *Delay between keys* a little. If the first arrow is lost, raise *Menu open delay*.

## Limits

- The plugin reads English status words only, so with the game in another language the keys can't show cooldowns. [TRANSLATIONS.md](TRANSLATIONS.md) explains how to add a language.
- It reads the game's pixels, so the stratagem panel in the top-left corner must be visible: a window over it hides it from the plugin too.
- It has been tested at 2560x1440, with the game's default HUD.
