#!/usr/bin/env bash
# Build fw-helper_<version>_<arch>.deb.
#
# dpkg-deb from a staging tree rather than cargo-deb: no extra tooling to install, and
# the layout is visible in one place. Packaging proper is M7; this is the whole of it.
set -euo pipefail
REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$REPO"

VERSION=$(awk -F'"' '/^version/ {print $2; exit}' Cargo.toml)
ARCH=$(dpkg --print-architecture)
STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT

echo "building release binaries..."
cargo build --release --all

install -d "$STAGE/DEBIAN" \
    "$STAGE/usr/bin" "$STAGE/usr/libexec" \
    "$STAGE/usr/share/applications" "$STAGE/usr/share/fw-helper" \
    "$STAGE/usr/share/doc/fw-helper" "$STAGE/usr/share/fw-helper/mangohud" \
    "$STAGE/usr/share/polkit-1/actions" \
    "$STAGE/etc/dbus-1/system.d" "$STAGE/etc/fw-helper/profiles.d" \
    "$STAGE/lib/systemd/system"

# The daemon and the crash-path fan restore are not for users to run; the CLI and the
# window are.
install -m 755 target/release/fw-helperd            "$STAGE/usr/libexec/"
install -m 755 target/release/fw-helper-restore-fan "$STAGE/usr/libexec/"
install -m 755 target/release/fw-helperctl          "$STAGE/usr/bin/"
install -m 755 target/release/fw-helper             "$STAGE/usr/bin/"

install -m 644 data/org.fwhelper.Daemon1.conf "$STAGE/etc/dbus-1/system.d/"
install -m 644 data/org.fwhelper.policy       "$STAGE/usr/share/polkit-1/actions/"
install -m 644 data/fw-helperd.service        "$STAGE/lib/systemd/system/"
install -m 644 data/fw-helper.desktop         "$STAGE/usr/share/applications/"
install -m 644 data/example-profile.conf      "$STAGE/usr/share/fw-helper/"
# For in-game numbers. No ordinary window can be drawn over a fullscreen game on
# Wayland, so this hands MangoHud - which is loaded into the game itself - the state
# only fw-helper knows.
install -m 644 data/mangohud/fw-helper.conf   "$STAGE/usr/share/fw-helper/mangohud/"
install -m 644 README.md LICENSE CHANGELOG.md "$STAGE/usr/share/doc/fw-helper/"

# Debian expects a machine-readable copyright file; shipping LICENSE alone is not it.
cat > "$STAGE/usr/share/doc/fw-helper/copyright" <<'COPYRIGHT'
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: fw-helper
Source: https://github.com/Wooloomooloo2/fw-helper

Files: *
Copyright: 2026 Mark Hall
License: GPL-3.0-only

License: GPL-3.0-only
 This program is free software: you can redistribute it and/or modify it under
 the terms of the GNU General Public License version 3, as published by the
 Free Software Foundation.
 .
 This program is distributed in the hope that it will be useful, but WITHOUT ANY
 WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A
 PARTICULAR PURPOSE.  See the GNU General Public License for more details.
 .
 On Debian systems, the complete text of the GNU General Public License version 3
 can be found in "/usr/share/common-licenses/GPL-3".
COPYRIGHT
chmod 644 "$STAGE/usr/share/doc/fw-helper/copyright"

# Work out library dependencies rather than guessing: the GUI links GTK4 and
# libadwaita and their sonames differ across releases.
#
# dpkg-shlibdeps insists on a debian/control existing relative to its working
# directory, even with -O, so it gets a throwaway one. It is deleted before the
# package is built - shipping a debian/ directory inside the .deb would be absurd.
install -d "$STAGE/debian"
cat > "$STAGE/debian/control" <<EOF
Source: fw-helper
Package: fw-helper
Architecture: $ARCH
EOF
DEPS=$(cd "$STAGE" && dpkg-shlibdeps -O --ignore-missing-info \
        usr/bin/fw-helper usr/bin/fw-helperctl usr/libexec/fw-helperd 2>/dev/null \
        | sed 's/^shlibs:Depends=//') || DEPS=""
rm -rf "$STAGE/debian"

if [ -z "$DEPS" ]; then
    # Not fatal, but the package would then declare no library dependencies at all and
    # would install onto a machine that cannot run it. Say so rather than shipping it
    # quietly.
    echo "WARNING: could not compute library dependencies; falling back to a minimum" >&2
    DEPS="libc6, libgtk-4-1, libadwaita-1-0"
fi

cat > "$STAGE/DEBIAN/control" <<EOF
Package: fw-helper
Version: $VERSION
Section: utils
Priority: optional
Architecture: $ARCH
Depends: ${DEPS}, dbus, systemd
Recommends: power-profiles-daemon
Maintainer: Mark Hall <mark.a.hall@gmail.com>
Homepage: https://github.com/Wooloomooloo2/fw-helper
Description: Firmware control for Framework laptops
 Fan curves, sustained power limits, battery charge limit and performance
 profiles for the Framework Laptop 13 on Ubuntu, in one application.
 .
 Profiles are layered over power-profiles-daemon rather than replacing it, so
 the GNOME power slider keeps working and stays in sync.
 .
 Manual fan control is never taken without being asked, and is bounded by the
 firmware's own behaviour: the fan is never run slower than the EC would run it,
 and control is handed back on exit, on crash, on suspend, and if the daemon
 stops responding.
EOF

cat > "$STAGE/DEBIAN/conffiles" <<'EOF'
/etc/dbus-1/system.d/org.fwhelper.Daemon1.conf
EOF

cat > "$STAGE/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = configure ]; then
    systemctl daemon-reload || true
    # enable AND restart: `enable --now` leaves an already-running old binary in
    # place on upgrade, which is indistinguishable from the new one not working.
    systemctl enable fw-helperd.service || true
    systemctl restart fw-helperd.service || true

    # Retire the ADR 0008 workaround. Earlier versions only advised removing it, and
    # advice does not remove anything: the drop-in is not merely useless, it forces
    # cros_charge-control to bind and produces a working-LOOKING
    # charge_control_end_threshold wired to the mechanism this board ignores. That
    # appearance is what made ADR 0008 look correct for weeks. Leaving it in place
    # leaves the trap armed.
    #
    # Only ever our own file: matched on content, not just the path, so an unrelated
    # drop-in that happens to share the name is never touched. It is not a dpkg
    # conffile - the package never shipped it to /etc - so removing it here is cleaning
    # up after ourselves rather than deleting a user's configuration.
    DROPIN=/etc/modprobe.d/fw-helper.conf
    if [ -e "$DROPIN" ] && grep -q probe_with_fwk_charge_control "$DROPIN" 2>/dev/null; then
        rm -f "$DROPIN"
        echo ""
        echo "fw-helper: removed $DROPIN (superseded by ADR 0012)."
        echo "  It forced cros_charge-control to bind, which produced a charge limit"
        echo "  attribute that reads back correctly and does not govern charging."
        echo "  The limit now goes through the EC command that does."
        echo ""
        echo "  The module parameter itself survives until reboot, so"
        echo "  charge_control_end_threshold may still exist this session. It is inert."
        echo ""
    fi
fi
EOF

cat > "$STAGE/DEBIAN/prerm" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = remove ] || [ "$1" = deconfigure ]; then
    # Stopping runs the unit's ExecStopPost, which returns the fan to the EC. Do it
    # while the binary still exists: removing the package must never leave the fan
    # held at a fixed duty with nothing managing it.
    systemctl stop fw-helperd.service || true
    systemctl disable fw-helperd.service || true
    [ -x /usr/libexec/fw-helper-restore-fan ] && /usr/libexec/fw-helper-restore-fan || true
fi
EOF

cat > "$STAGE/DEBIAN/postrm" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = purge ]; then
    rm -f /etc/modprobe.d/fw-helper.conf
    rm -rf /var/lib/fw-helper
    # Profiles the user wrote are theirs; remove the directory only if it is empty.
    rmdir --ignore-fail-on-non-empty /etc/fw-helper/profiles.d /etc/fw-helper 2>/dev/null || true
fi
systemctl daemon-reload || true
EOF

chmod 755 "$STAGE/DEBIAN/postinst" "$STAGE/DEBIAN/prerm" "$STAGE/DEBIAN/postrm"

OUT="$REPO/fw-helper_${VERSION}_${ARCH}.deb"
dpkg-deb --root-owner-group --build "$STAGE" "$OUT" >/dev/null
echo "built $OUT"
dpkg-deb --info "$OUT" | sed -n '1,12p'
echo
echo "contents:"
dpkg-deb --contents "$OUT" | awk '{print $6, $7, $8}' | grep -vE '/$' | sed 's/^/  /'
