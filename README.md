# holdthis4me

A shelf for files and folders that lives in the top-left corner of the screen. Put things
on it, go to any other app, and drag them off it again: no copying paths, no digging
through Nautilus in every upload dialog.

## Using it

| Do this | What happens |
| --- | --- |
| Push the pointer against the top or left screen border, within 200 px of the corner | The shelf drops down (after a 150 ms pause, so passing by doesn't trigger it) |
| Drag files or folders onto the top part of the left screen edge | The shelf opens at once, and you drop them on it |
| `+` / folder button | Opens the file chooser, so you can pick files or folders by browsing |
| Drag an item off the shelf into any app | The app gets the real file; the item stays on the shelf |
| **Drag all** (2+ items) | Drags everything at once |
| Double-click an item | Opens it in its default app |
| Hover an item → location (crosshair) icon | Shows the file selected in Nautilus |
| Hover an item → × | Lets go of it (the file itself is untouched) |
| Clear button | Lets go of everything |
| Open-folder button | Opens the shelf folder in Nautilus |

The shelf rises back out a moment after the pointer leaves it. While you drag an item out, it
drops behind your windows so they can take the drop.

`holdthis4me --toggle` opens or closes it from a keybind, if you want one.

## How it holds things

The shelf is a folder of symlinks: `~/.local/share/holdthis4me/shelf`. Nothing is copied,
nothing is kept in memory, and the items survive logouts and reboots. You can open that
folder in Nautilus and add or delete links there; the shelf follows.

Safety rules:

- Drags out offer **copy** only. Dropping into Nautilus copies the file; it never moves your original.
- Drops in accept **copy** only. Accepting "move" would tell Nautilus to delete the originals.
- Letting go deletes only the link, never the file it points to. It refuses anything in the folder that isn't a symlink (there's a test for this).
- If a held file is moved or deleted, its item shows as *missing*.

Only files and folders are accepted. Things that aren't local files are ignored: a link or an
image dragged straight out of a browser page, for example.

## Look

It is built like the quickshell bar and dock:

- a 2 px border fading from `primary` to `on_primary`
- the `background` colour inside, translucent, with radius 16 and a soft shadow
- JetBrains Mono, and peach pill buttons

The colours come from `~/.config/ml4w/colors/colors.json`, the file the bar reads. The shelf
reloads when that file changes, so a new wallpaper re-themes it too. The fill is slightly
more opaque than the bar (0.88 against 0.8), so text stays readable over white windows.

## Install

```bash
./install.sh
```

This builds and installs to `~/.local/share/holdthis4me/{bin,hypr}` and restarts it. The
shelf folder is never touched by installs.

Two lines in the Hyprland config hook it up (already added):

- `~/.config/hypr/conf/autostart.lua` starts `holdthis4me-start`. That runs the binary through `ensure-running`: one instance, health-checked over D-Bus, no restart loops.
- `~/.config/hypr/custom.lua` loads `hypr/holdthis4me.lua`. It turns off Hyprland's own layer animations for holdthis4me. The shelf animates itself with a CSS transition: it drops in on the "overshot" curve your window animations use, and rises out faster. That way it doesn't depend on the animation preset.

## How it works

Each screen gets three layer surfaces:

- **Top:** an invisible 2 px strip along the top border, 200 px from the corner. Hover or drag opens the shelf.
- **Left:** an invisible 2 px strip down the left border, 45% of the screen tall. Hover opens the shelf in the top 200 px; a drag opens it anywhere along the strip.

Only the outermost 2 px react. Apps can reach right up to the screen edge, so nothing sits further in, where it could cover their buttons.
- **Shelf:** mapped only while it's out. It covers the screen edge and is at least as tall as the strip, so a drag that opened it lands on it.

The triggers aren't fully transparent; they're 1% black. GTK never commits a frame for a surface
with nothing to draw, so Hyprland would never map it.

Source: `src/main.rs` (UI, triggers, drag and drop, theme), `src/shelf.rs` (the link folder,
with tests), `src/style.css`.

Build and test: `PKG_CONFIG=/usr/bin/pkg-config cargo test --release`.
