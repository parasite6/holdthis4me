# holdthis4me

A shelf for files and folders that lives in the top-left corner of the screen. Put things
on it, go to any other app, and drag them off it again: no copying paths, no digging
through Nautilus in every upload dialog.

## Using it

| Do this | What happens |
| --- | --- |
| Touch the top screen border where the shelf lives (the top-left corner, as wide as the shelf) | The shelf drops down instantly |
| Drag files or folders up to that same stretch of the top border | The shelf opens at once, and you drop them on it |
| Drop selected text, or a link from a browser | It's held as a note: the text itself, or the link's address |
| Drag a note out | A text box or chat gets the text or address; an app that only takes files gets a small `.txt` / `.url` file |
| Double-click a link note | Opens it in your browser |
| `+` / folder button | Opens the file chooser, so you can pick files or folders by browsing |
| Drag an item off the shelf into any app | The app gets the real file; the item stays on the shelf |
| **Drag all** (2+ items) | Drags everything at once |
| Double-click an item | Opens it in its default app |
| Hover an item → location (crosshair) icon | Shows the file selected in Nautilus |
| Hover an item → × | Lets go of it (the file itself is untouched) |
| Clear button, twice | Lets go of everything (the first click arms it; the second, within 2.5 s, clears) |
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
- Every change goes through a handle on the shelf folder itself. The folder is opened without following symlinks, and only if it's a real directory you own that nobody else can write to. If the shelf path were ever swapped for a symlink to another folder, nothing there could be listed, added or removed; "Clear" could never sweep your real links.
- Removing moves the entry aside under a private name first, then checks what it actually moved. Only a symlink is deleted; anything else is put back. Nothing can be swapped in between the check and the delete. If a removal is ever interrupted, startup finishes it, again for symlinks only.
- Nothing ever points back into the shelf. The shelf itself can't be held. A file reached through a held folder's link is held at its real location.
- The shelf folders are private (0700).
- If a held file is moved or deleted, its item shows as *missing*.

Nothing reads the held files on the main thread. Whether an item still exists, and its icon, are checked in the background, so a held file on a dead network mount can't freeze the shelf.

### Text and links

Text and links aren't files, so each one is saved as a small file in
`~/.local/share/holdthis4me/notes` (private, 0600 files) and held like anything else:
- **Text:** a `.txt` named after its first line.
- **Links:** a `.url` Internet shortcut named after the address.

A single web address on its own counts as a link; anything else is text, up to 1 MiB.

An image dragged out of a web page arrives as its address, so it becomes a link note, not
the image.

These notes are the only files holdthis4me ever deletes, and only its own:
- **Letting go:** when you let go of a note, its file goes too.
- **Startup:** notes no item points to (say, after you deleted links by hand) are cleared out. This never runs if the shelf folder is refused, because a refused shelf lists as empty and every note would look orphaned.
- **What can be deleted:** only a regular file directly inside the notes folder. Deletes go through the same pinned, moved-aside-then-checked path as shelf links.

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

Each screen gets two layer surfaces:

Both sit on the Top layer while the shelf is hidden: above normal windows, under fullscreen ones. A fullscreen game or video never gets the shelf popping over it, and nothing is composited on top of it. The shelf moves to the Overlay layer only while it's out.

- **Top:** an invisible 2 px strip along the top border, exactly as long as the shelf is wide. Hover or drag opens the shelf.

Only the outermost 2 px react. Apps can reach right up to the screen edge, so nothing sits further in, where it could cover their buttons.
- **Shelf:** always mapped, so it comes out without waiting for a new window. While hidden, it's slid away, transparent and click-through: its input region is empty and re-applied after every layout. It sizes to its items (260 px minimum), so a drag that opened it lands on it.

The triggers aren't fully transparent; they're 1% black. GTK never commits a frame for a surface
with nothing to draw, so Hyprland would never map it.

Source: `src/main.rs` (UI, triggers, drag and drop, theme), `src/shelf.rs` (the link folder,
with tests), `src/style.css`.

Build and test: `PKG_CONFIG=/usr/bin/pkg-config cargo test --release`.

## Audit (2026-10-11)

**Fixed:**

- **File safety:**
  - A shelf path swapped for a symlink could have let "Clear" delete another folder's links. The folder is now pinned and checked.
  - There was a race between checking that an entry is a symlink and deleting it. Removal now moves it aside, then checks it.
  - Links could point back into the shelf.
  - Clear was a single click.
- **Hangs and crashes:**
  - Checking held files on the main thread could freeze the shelf when a mount died. Double-click open did the same.
  - Reopening on an unplugged screen could get the app disconnected.
  - After a hotplug, the trigger strips stacked above the shelf.
  - The fix itself first introduced a RefCell double borrow, through a borrow held in an `if let` scrutinee. That aborted at startup. It was caught at install and fixed.
- **Performance:**
  - Hidden surfaces sat over fullscreen apps.
  - Big drops were quadratic: each file re-read the shelf.
  - Names near the 255-byte limit couldn't be numbered.

**Checked and fine:**

- **Input:** theme values are hex only; labels and tooltips are plain text; URIs to Nautilus are escaped.
- **Drags:** copy only, both ways.
- **GTK behaviour:** a row removing itself mid-click is safe, and no timer is removed twice.
- **Idle cost:** zero CPU.

**Known limits:**

- **Name squatting:** any program running as you could claim the D-Bus name first, or call `--toggle`. That's the same trust level as you, so it isn't guarded.
- **Hung mounts:** a background check stuck on a hung mount keeps one GIO worker thread busy until the mount answers or times out. A file whose check is still running is never checked again, so reopening the shelf doesn't pile up more stuck checks.
- **Refused folder:** if the shelf folder is refused, the shelf says why in place of "Drop files or folders here".
