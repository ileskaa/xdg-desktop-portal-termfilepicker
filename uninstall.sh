#!/bin/sh
# Reverses install.sh. Keeps your config.toml unless --purge is given.
set -eu

NAME=xdg-desktop-portal-termfilepicker
PORTAL_ID=termfilepicker
DBUS_NAME=org.freedesktop.impl.portal.desktop.$PORTAL_ID
CHOOSER_KEY=org.freedesktop.impl.portal.FileChooser
MARK="# added by $NAME"

DATA_HOME=${XDG_DATA_HOME:-$HOME/.local/share}
CONFIG_HOME=${XDG_CONFIG_HOME:-$HOME/.config}
STATE_DIR=$DATA_HOME/$NAME

systemctl --user stop "$NAME.service" 2>/dev/null || true
rm -f "$HOME/.local/libexec/$NAME" \
    "$DATA_HOME/xdg-desktop-portal/portals/$PORTAL_ID.portal" \
    "$DATA_HOME/dbus-1/services/$DBUS_NAME.service" \
    "$CONFIG_HOME/systemd/user/$NAME.service"

created=$(cat "$STATE_DIR/created-portals-conf" 2>/dev/null || true)
created_sum=$(cat "$STATE_DIR/created-portals-conf.cksum" 2>/dev/null || true)
for conf in "$CONFIG_HOME"/xdg-desktop-portal/*portals.conf; do
    [ -f "$conf" ] || continue
    if [ "$conf" = "$created" ] && [ "$(cksum < "$conf")" = "$created_sum" ]; then
        rm -f "$conf"
        continue
    fi
    tmp=$(mktemp "$conf.XXXXXX")
    awk -v ours="$CHOOSER_KEY=$PORTAL_ID" -v mark="$MARK" '
        $0 == mark { after_mark = 1; next }
        after_mark && $0 == ours { after_mark = 0; next }
        { after_mark = 0 }
        index($0, "#" mark " disabled# ") == 1 { print substr($0, length(mark) + 13); next }
        { print }
    ' "$conf" > "$tmp"
    cat "$tmp" > "$conf"
    rm -f "$tmp"
done
rm -rf "$STATE_DIR"
[ "${1:-}" = "--purge" ] && rm -rf "${CONFIG_HOME:?}/$NAME"

systemctl --user daemon-reload
# The session bus only notices new/removed activation files after a reload.
busctl --user call org.freedesktop.DBus /org/freedesktop/DBus org.freedesktop.DBus ReloadConfig >/dev/null
systemctl --user restart xdg-desktop-portal.service
echo "Uninstalled $NAME."
