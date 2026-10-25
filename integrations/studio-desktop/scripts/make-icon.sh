#!/bin/sh
# icon/icon.svg -> icon.iconset/, which Electrobun turns into the .icns
# (build.mac.icons), with the tools macOS has: sips draws the SVG and
# resizes, nothing is downloaded.
set -eu
cd "$(dirname "$0")/.."

svg=icon/icon.svg
set_dir=icon.iconset
big=$(mktemp -d "${TMPDIR:-/tmp}/fenec-icon.XXXXXX")
trap 'rm -rf "$big"' EXIT

# One 1024-point render, every other size is cut from it. sips keeps the alpha
# channel, which the squircle's corners need. An older macOS's sips does not
# read SVG; QuickLook's thumbnailer, which is part of the system too, does.
if ! sips -s format png "$svg" --out "$big/1024.png" >/dev/null 2>&1; then
  echo "make-icon: sips cannot draw $svg here, using qlmanage" >&2
  qlmanage -t -s 1024 -o "$big" "$svg" >/dev/null 2>&1 || true
  [ -f "$big/$(basename "$svg").png" ] || {
    echo "make-icon: neither sips nor qlmanage could draw $svg" >&2
    exit 1
  }
  mv "$big/$(basename "$svg").png" "$big/1024.png"
fi

rm -rf "$set_dir"
mkdir -p "$set_dir"
# A point size and its @2x, as iconutil names them.
for size in 16 32 128 256 512; do
  double=$((size * 2))
  sips -z "$size" "$size" "$big/1024.png" --out "$set_dir/icon_${size}x${size}.png" >/dev/null
  sips -z "$double" "$double" "$big/1024.png" --out "$set_dir/icon_${size}x${size}@2x.png" >/dev/null
done
# iconutil would refuse a set it cannot read; this is the cheapest way to say
# so here and not in the middle of a build.
iconutil -c icns "$set_dir" -o "$big/check.icns"
echo "icon.iconset: $(ls "$set_dir" | wc -l | tr -d ' ') images from $svg"
