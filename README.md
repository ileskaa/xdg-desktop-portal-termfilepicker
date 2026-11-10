# xdg-desktop-portal-termfilepicker

Pick files with **[Yazi](https://yazi-rs.github.io)** whenever an app (browser upload button, "Save as…", etc.)
asks for a file chooser through xdg-desktop-portal. Inspired by
[xdg-desktop-portal-termfilechooser](https://github.com/hunkyburrito/xdg-desktop-portal-termfilechooser),
with a focus on a small, hardened design and a one-command install.

## Install

Requirements: Rust (`cargo`), `yazi`, a systemd user session, and one of
alacritty, foot, kitty, wezterm or ghostty.

```sh
./install.sh
```

That's it. The script builds the binary, installs it for your user only (no root),
sets it as the FileChooser for your desktop, picks your terminal and restarts the portal.
It is idempotent, so run it again after `git pull` to update.

Then click any file-upload button:

- **Chrome / Chromium**: works out of the box.
- **Firefox**: set `widget.use-xdg-desktop-portal.file-picker` to `1` in `about:config`.

Uninstall with `./uninstall.sh` (add `--purge` to also delete your config).

## Using it

What to do in Yazi for each kind of dialog:

- **Open file(s):** move to a file and press `Enter`. For several files, mark them with `Space`
  first.
- **Open folder:** quit with `q` inside the folder and confirm, or mark folders with `Space` and
  press `Enter`.
- **Save:** quit with `q` in the target folder, then edit the suggested name at the `Save as:`
  prompt. Or press `Enter` on an existing file to replace it (you are asked first).
- **Save several files:** quit with `q` in the target folder and confirm.

To cancel, quit with `Q`. At the `Save as:` prompt, `Ctrl-C` goes back to Yazi and `Ctrl-D`
cancels.

The terminal window uses the app id/class `termfilepicker`, so you can float it in sway:

```
for_window [app_id="termfilepicker"] floating enable, resize set 1200 800
for_window [class="termfilepicker"] floating enable, resize set 1200 800
```

## Configuration

`~/.config/xdg-desktop-portal-termfilepicker/config.toml` (created by the installer; re-read on every
request, no restart needed):

```toml
# A preset: "alacritty", "foot", "kitty", "wezterm", "ghostty"
terminal = "alacritty"
# …or any command that runs the trailing arguments and waits for them to exit:
# terminal = ["footclient", "--app-id", "termfilepicker", "--"]

# yazi = "/usr/bin/yazi"        # default: yazi from PATH
# default_dir = "~/Downloads"   # used when the app suggests no folder
```

Unknown keys are rejected. The terminal command is executed directly, never through a shell.

## Troubleshooting

- Logs: `journalctl --user -u xdg-desktop-portal-termfilepicker` (and `-u xdg-desktop-portal`).
- Nothing opens: check `systemctl --user show-environment` contains `WAYLAND_DISPLAY` (or `DISPLAY`).
  Most sway setups import it via `/etc/sway/config.d/50-systemd-user.conf`.
- The terminal flashes and closes: your terminal may hand the command to an already-running
  instance and exit immediately. Use a preset, or a command that blocks until the program exits.
- The installer edits `~/.config/xdg-desktop-portal/<desktop>-portals.conf`. Your previous
  `FileChooser=` line is commented out, not deleted, and `uninstall.sh` restores it.

## Security design

Temporary files:

- Each request gets a fresh random directory (mode 0700) under
  `$XDG_RUNTIME_DIR/xdg-desktop-portal-termfilepicker/`. That location must be owned by you and
  not group/world accessible; otherwise the request fails (there is no `/tmp` fallback).
- The portal's own files are created 0600 with `O_EXCL`. Reads use `O_NOFOLLOW`, check owner and
  file type, and cap the size. Yazi writes its two output files there with your umask, protected
  by the 0700 directory.
- The directory is deleted when the request ends (also on cancel), and leftovers are removed at
  start-up.

Saving:

- The portal never creates, writes or deletes anything outside its private directory; no
  placeholder files are put in the target folder.
- The name is asked for in the terminal, and replacing an existing file needs an explicit `y`.
- As with every file chooser, the app writes the file after the dialog closes, so the target can
  still change in between.

Paths stay private:

- Folders and names travel only inside the private directory, never in command-line arguments
  (which other users can read in `/proc`). Yazi gets the start folder as its working directory.
- Logs contain request handles, the app id, the mode, a count and error messages, never folders
  or file names, at every log level.

Other hardening:

- No shell anywhere: no wrapper scripts, the terminal command is executed directly.
- Names suggested by apps are reduced to a single path component (`../../.bashrc` becomes `.bashrc`).
  Names with control characters (terminal escape sequences), bidi overrides or zero-width
  characters are rejected (`SaveFile` falls back to `untitled`, and `SaveFiles` fails).
  Every path, app id and title is escaped before it is printed.
- Every prompt names the requesting app and its dialog title, so one app's request cannot pass
  for another's.
- Results from the picker are re-validated by the daemon (absolute, exist / have a real parent
  folder, file vs. folder matches the request, count matches).
- At most 4 pickers are open at a time, so an app cannot flood the screen with terminals.
  Cancelling a request stops the terminal (SIGTERM, then SIGKILL after 2 s).
- Only the current owner of `org.freedesktop.portal.Desktop` (xdg-desktop-portal) may call the
  backend or close requests. This is defence in depth: it stops other session-bus clients from
  bypassing the frontend, but the frontend itself lets any app open a picker.

Limits to be aware of:

- The app receives the paths you pick (that is the purpose).
- The app chooses the start folder, so its name shows in Yazi and in the terminal title.
- Yazi previews files with its normal previewers, outside any sandbox. A sandboxed app could
  suggest a start folder that contains crafted files, which are then previewed on the host.
  This is the same risk as browsing that folder in Yazi yourself.
- For unsandboxed apps the app id is empty or self-declared. The prompt then says
  "An unsandboxed app".
- Anything running as your own user can already read your files, so it is out of scope.

## Development

```sh
cargo test          # unit tests + end-to-end tests on a private D-Bus bus (needs dbus-daemon)
cargo clippy --all-targets -- -D warnings
```

Layout: `src/portal.rs` (D-Bus interface, caller check, result validation), `src/pick.rs` (runs in the
terminal, drives Yazi and prompts), `src/workspace.rs` (private temp directories), `src/protocol.rs`
(daemon ↔ picker file format), `src/validate.rs` (name/path checks), `src/terminal.rs`, `src/config.rs`.
