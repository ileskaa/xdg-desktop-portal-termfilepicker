#!/bin/sh
# Builds and installs the Yazi file-chooser portal for the current user (no root needed).
set -eu

NAME=xdg-desktop-portal-termfilepicker
PORTAL_ID=termfilepicker
DBUS_NAME=org.freedesktop.impl.portal.desktop.$PORTAL_ID
CHOOSER_KEY=org.freedesktop.impl.portal.FileChooser
MARK="# added by $NAME"

DATA_HOME=${XDG_DATA_HOME:-$HOME/.local/share}
CONFIG_HOME=${XDG_CONFIG_HOME:-$HOME/.config}
LIBEXEC_DIR=$HOME/.local/libexec
STATE_DIR=$DATA_HOME/$NAME

cd "$(dirname "$0")"

say() { printf '\033[1m==>\033[0m %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "$1 not found. $2"; }

need cargo "Install Rust from https://rustup.rs or your package manager."
need yazi "Install Yazi, e.g. 'sudo pacman -S yazi'."
need systemctl "A systemd user session is required."
[ -n "${XDG_CURRENT_DESKTOP:-}" ] || die "XDG_CURRENT_DESKTOP is unset; run this from your graphical session."

say "Building"
cargo build --release --locked --quiet

say "Installing files"
install -Dm755 "target/release/$NAME" "$LIBEXEC_DIR/$NAME"
libexec_sed=$(printf %s "$LIBEXEC_DIR" | sed 's/[\\|&]/\\&/g')
render() { mkdir -p "$(dirname "$2")"; sed "s|@LIBEXECDIR@|$libexec_sed|g" "data/$1" > "$2"; }
render "$PORTAL_ID.portal" "$DATA_HOME/xdg-desktop-portal/portals/$PORTAL_ID.portal"
render "$DBUS_NAME.service" "$DATA_HOME/dbus-1/services/$DBUS_NAME.service"
render "$NAME.service" "$CONFIG_HOME/systemd/user/$NAME.service"

say "Selecting $PORTAL_ID as the file chooser"
desktop=$(printf %s "$XDG_CURRENT_DESKTOP" | cut -d: -f1 | tr '[:upper:]' '[:lower:]')
conf=$CONFIG_HOME/xdg-desktop-portal/$desktop-portals.conf
mkdir -p "$(dirname "$conf")" "$STATE_DIR"
created_record=$STATE_DIR/created-portals-conf
if [ ! -e "$conf" ]; then
    # A desktop-specific user file shadows every other portals.conf, so start from
    # the one the frontend would otherwise have used.
    base=""
    for candidate in "$CONFIG_HOME/xdg-desktop-portal/portals.conf" \
        /usr/local/share/xdg-desktop-portal/"$desktop"-portals.conf \
        /usr/share/xdg-desktop-portal/"$desktop"-portals.conf \
        /usr/local/share/xdg-desktop-portal/portals.conf \
        /usr/share/xdg-desktop-portal/portals.conf; do
        [ -f "$candidate" ] && { base=$candidate; break; }
    done
    if [ -n "$base" ]; then cp "$base" "$conf"; else printf '[preferred]\n' > "$conf"; fi
    printf '%s\n' "$conf" > "$created_record"
fi
# Comment out other FileChooser choices (restored by uninstall.sh), then add ours under [preferred].
tmp=$(mktemp "$conf.XXXXXX")
awk -v ours="$CHOOSER_KEY=$PORTAL_ID" -v mark="$MARK" \
    -v chooser='^[ \t]*org[.]freedesktop[.]impl[.]portal[.]FileChooser[ \t]*=' '
    $0 == mark { after_mark = 1; next }
    after_mark && $0 == ours { after_mark = 0; next }
    { after_mark = 0 }
    $0 ~ chooser { print "#" mark " disabled# " $0; next }
    { print }
    /^[ \t]*\[preferred\][ \t]*$/ && !done { print mark; print ours; done = 1 }
    END { if (!done) { print "[preferred]"; print mark; print ours } }
' "$conf" > "$tmp"
# Write in place so a symlinked conf (dotfiles repo) stays a symlink.
cat "$tmp" > "$conf"
rm -f "$tmp"
# Remember the content so uninstall.sh only deletes a file it created if it is unchanged.
[ "$(cat "$created_record" 2>/dev/null)" = "$conf" ] && cksum < "$conf" > "$created_record.cksum"

config_file=$CONFIG_HOME/$NAME/config.toml
# shellcheck disable=SC2016 # matches a literal "set $term <cmd>" line
sway_terminal() { sed -n 's/^[[:space:]]*set[[:space:]]\{1,\}\$term[[:space:]]\{1,\}\([^[:space:]]*\).*/\1/p' "$CONFIG_HOME/sway/config" 2>/dev/null | head -n1; }
if [ ! -e "$config_file" ]; then
    term=""
    for candidate in "${TERMINAL:-}" "$(sway_terminal)" \
        alacritty foot kitty wezterm ghostty; do
        candidate=$(basename -- "${candidate:-none}")
        case "$candidate" in alacritty|foot|kitty|wezterm|ghostty) ;; *) continue ;; esac
        command -v "$candidate" >/dev/null 2>&1 && { term=$candidate; break; }
    done
    [ -n "$term" ] || die "no supported terminal found (alacritty, foot, kitty, wezterm, ghostty); see README for custom terminals."
    mkdir -p "$(dirname "$config_file")"
    cat > "$config_file" <<CONF
# See README.md for all options.
terminal = "$term"
# default_dir = "~/Downloads"
CONF
    say "Using terminal: $term (edit $config_file to change)"
fi

say "Restarting portal services"
systemctl --user daemon-reload
# The session bus only notices new/removed activation files after a reload.
busctl --user call org.freedesktop.DBus /org/freedesktop/DBus org.freedesktop.DBus ReloadConfig >/dev/null
systemctl --user try-restart "$NAME.service"
if ! systemctl --user show-environment | grep -q '^WAYLAND_DISPLAY=\|^DISPLAY='; then
    printf 'warning: systemd user session has no WAYLAND_DISPLAY/DISPLAY; terminals cannot open.\n' >&2
    printf '         Add to your compositor config: exec systemctl --user import-environment WAYLAND_DISPLAY DISPLAY XDG_CURRENT_DESKTOP\n' >&2
fi
systemctl --user restart xdg-desktop-portal.service

say "Done. Open any file-upload button in your browser to get Yazi."
cat <<'TIP'
Firefox: set about:config -> widget.use-xdg-desktop-portal.file-picker = 1
Chrome/Chromium: uses the portal automatically.
Logs: journalctl --user -u xdg-desktop-portal-termfilepicker
TIP
