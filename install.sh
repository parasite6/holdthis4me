#!/usr/bin/env bash
# Build holdthis4me and install it to ~/.local/share/holdthis4me, then restart the
# background process. Safe to re-run: files are replaced atomically and installed
# read-only. The shelf itself (~/.local/share/holdthis4me/shelf) is never touched.
set -euo pipefail

src="$(cd "$(dirname "$0")" && pwd)"
dest="$HOME/.local/share/holdthis4me"

# linuxbrew's pkg-config shadows Fedora's and can't see the system GTK .pc files.
PKG_CONFIG=/usr/bin/pkg-config cargo build --release --manifest-path "$src/Cargo.toml" --target-dir "$src/target"

mkdir -p "$dest/bin" "$dest/hypr" "$dest/shelf"

put() { # put SRC DEST MODE
    install -m "$3" "$1" "$2.new"
    mv -f "$2.new" "$2"
}
put "$src/target/release/holdthis4me"  "$dest/bin/holdthis4me"       0555
put "$src/scripts/holdthis4me-start"   "$dest/bin/holdthis4me-start" 0555
put "$src/hypr/holdthis4me.lua"        "$dest/hypr/holdthis4me.lua"  0444

# Restart. A deliberate install isn't a crash loop, so it doesn't count toward
# ensure-running's "3 starts in 5 minutes" guard.
rm -f "${XDG_RUNTIME_DIR:-/tmp}/ensure-running/holdthis4me.starts"
pkill -x holdthis4me || true
for _ in $(seq 20); do pgrep -x holdthis4me >/dev/null || break; sleep 0.1; done
# Started by Hyprland, so it gets the desktop's environment rather than this shell's.
if ! hyprctl dispatch "hl.dsp.exec_cmd(\"$dest/bin/holdthis4me-start\")" >/dev/null 2>&1; then
    "$dest/bin/holdthis4me-start"
fi

for _ in $(seq 30); do
    if timeout 2 gdbus call --session --dest local.holdthis4me --object-path /local/holdthis4me \
        --method org.gtk.Actions.List >/dev/null 2>&1; then
        echo "holdthis4me installed to $dest and running"
        exit 0
    fi
    sleep 0.2
done
echo "holdthis4me installed to $dest but is not answering; see: journalctl --user -t ensure-running" >&2
exit 1
