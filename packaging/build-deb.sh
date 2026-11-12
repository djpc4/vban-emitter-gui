#!/bin/sh
# Builds the vban-emitter-gui .deb from the release program. Run from the project folder after cargo build --release.
# It also builds vban_emitter from a pinned vban release (github.com/quiniouben/vban, GPL-3.0) and ships it, unchanged,
# in /usr/lib/vban-emitter-gui, with vban's license and the exact source it was built from.
# Needs: git, cmake, a C compiler, libasound2-dev, libpulse-dev.
set -eu
target=${CARGO_TARGET_DIR:-target}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
version="$version+$(date -u +%Y%m%d.%H%M)"
out=${1:-$target/vban-emitter-gui_${version}_amd64.deb}

VBAN_TAG=v2.2.0
VBAN_COMMIT=9c3d558bbdb9d971cdddf95dff1abdcb95e94466
vsrc="$target/vban-src"
vbuild="$target/vban-build"
if [ ! -x "$vbuild/src/vban_emitter" ] || [ "$(git -C "$vsrc" rev-parse HEAD 2>/dev/null)" != "$VBAN_COMMIT" ]; then
    rm -rf "$vsrc" "$vbuild"
    git -c advice.detachedHead=false clone -q --depth 1 --branch "$VBAN_TAG" https://github.com/quiniouben/vban.git "$vsrc"
    if [ "$(git -C "$vsrc" rev-parse HEAD)" != "$VBAN_COMMIT" ]; then
        echo "vban $VBAN_TAG is not commit $VBAN_COMMIT any more; check it before updating the pin" >&2
        exit 1
    fi
    # The emitter refuses JACK, so it isn't built in.
    # vban's own compiler warnings are left out of the output; a failed build still stops here, with its log.
    cmake -S "$vsrc" -B "$vbuild" -DCMAKE_BUILD_TYPE=Release -DWITH_JACK=OFF >/dev/null
    cmake --build "$vbuild" -j "$(nproc)" > "$target/vban-build.log" 2>&1 || { tail -n 30 "$target/vban-build.log" >&2; exit 1; }
fi

root=$(mktemp -d); trap 'rm -rf "$root"' EXIT
doc="$root/usr/share/doc/vban-emitter-gui"
install -Dm755 "$target/release/vban-emitter-gui" "$root/usr/bin/vban-emitter-gui"
install -Dm755 "$vbuild/src/vban_emitter" "$root/usr/lib/vban-emitter-gui/vban_emitter"
strip "$root/usr/lib/vban-emitter-gui/vban_emitter" 2>/dev/null || true
install -Dm644 packaging/vban-emitter-gui.desktop "$root/usr/share/applications/vban-emitter-gui.desktop"
install -Dm644 packaging/vban-emitter-gui.svg "$root/usr/share/icons/hicolor/scalable/apps/vban-emitter-gui.svg"
install -Dm644 README.md "$doc/README.md"
install -Dm644 LICENSE "$doc/copyright"
install -Dm644 "$vsrc/COPYING" "$doc/vban/COPYING"
git -C "$vsrc" archive --format=tar.gz --prefix="vban-$VBAN_TAG/" HEAD > "$doc/vban/vban-$VBAN_TAG-source.tar.gz"
cat > "$doc/vban/README" <<EOF
/usr/lib/vban-emitter-gui/vban_emitter is vban_emitter from the vban project by Benoit Quiniou and contributors,
https://github.com/quiniouben/vban, release $VBAN_TAG (commit $VBAN_COMMIT), built unchanged with -DWITH_JACK=OFF.
It is free software under the GNU General Public License version 3 (COPYING, here). Its complete source code is
vban-$VBAN_TAG-source.tar.gz, here.
EOF

mkdir -p "$root/DEBIAN"
# So the menu and taskbar pick up a changed icon at once.
cat > "$root/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q -f /usr/share/icons/hicolor || true
exit 0
EOF
chmod 755 "$root/DEBIAN/postinst"
cat > "$root/DEBIAN/control" <<EOF
Package: vban-emitter-gui
Version: $version
Section: sound
Priority: optional
Architecture: amd64
Maintainer: VBAN Emitter GUI contributors <nobody@invalid>
Depends: libc6, libgcc-s1, libgtk-3-0t64 | libgtk-3-0, libxdo3, libasound2t64 | libasound2, libpulse0, pulseaudio-utils, systemd
Recommends: libayatana-appindicator3-1, libxkbcommon-x11-0, libgl1
Description: Send this computer's sound as VBAN streams
 VBAN Emitter GUI sends any number of VBAN audio streams to Voicemeeter or other
 VBAN receivers: pick each stream's sound device with live level meters, check
 the receiver answers, and run it all as a user service that starts at login.
 Includes vban_emitter from the vban project (GPL-3.0), with its source.
EOF
dpkg-deb --root-owner-group --build "$root" "$out" >/dev/null
echo "$out"
