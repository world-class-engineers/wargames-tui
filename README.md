# bandit-tui

A terminal UI for playing the [OverTheWire Bandit](https://overthewire.org/wargames/bandit/)
wargame. It shows the current level's instructions next to a real, embedded SSH
terminal, verifies next-level secrets in the background, and automatically
moves on to the next level when a secret is accepted.

Connections are made directly from your machine to
`bandit.labs.overthewire.org:2220`; there is no shared proxy.

## Requirements

- Rust 1.88+ (edition 2024), `cargo`
- An `ssh` client in `PATH`
- For password authentication, either OpenSSH **8.4+** (the app uses
  `SSH_ASKPASS_REQUIRE=force` with a generated helper script — no extra
  packages) **or** `sshpass` in `PATH` if you already have it. `sshpass` is
  optional: when present it is used; otherwise the app falls back to OpenSSH's
  `SSH_ASKPASS`. Private-key secrets need neither.
- Network access to `overthewire.org` for level pages (cached pages work offline)

Password verification is fully non-interactive and time-bounded in both modes;
neither path requires you to watch a prompt.

## Build and run

```sh
cargo build --release
./target/release/bandit-tui
# or, during development:
cargo run
```

The app starts at level 0 (password `bandit0`). If you have verified a secret
before, `last_level` from `credentials.json` is used instead and the saved
secret is used for automatic login. The level pane's first line shows a compact
progress indicator (for example, `Cleared: 1  |  Now: bandit1 / 33`, where 33 is
the final level number). The game has 34 levels, 0 through 33. The indicator is
derived from the saved `last_level`/secrets and resumes on restart.

OverTheWire's "Level X" page describes how to get **into** banditX from
bandit(X-1): e.g. `bandit1.html` is the task for a player logged into bandit0.
So while logged into `banditL`, this app displays `bandit{L+1}.html` — the
instructions for the level you are actually playing. `bandit34.html` is only a
placeholder and is never fetched; level 33 is the end of the game.

## Layout and keybindings

Left pane: level goal, commands, helpful reading, notes, and the next-level
secret box. Right pane: the embedded SSH session (colors, attributes, cursor,
and scrolling are rendered by `vt100`). Bottom line: context-sensitive hints,
including the help keys.

You can also **click a pane to focus it**: clicking the *Next Level Secret* box
switches to secret-input mode (then just type or paste), and clicking the SSH
box switches back to the terminal. Clicks are only used for focus — they are
never sent to the remote shell. Resizing the terminal keeps the click targets
aligned with the rendered boxes.

### In the SSH terminal (default focus)

| Key | Action |
| --- | --- |
| type / paste | Sent to the SSH PTY |
| `PageUp` / `PageDown` | Scroll the SSH scrollback up/down one screenful (not sent to the remote shell) |
| Mouse wheel over the SSH pane | Scroll the SSH scrollback a few lines per tick |
| Click the *Next Level Secret* box | Enter secret-entry mode |
| `Alt+I` (or `Ctrl+P`) | Enter secret-entry mode |
| `Ctrl+C` | Sent to the remote shell (interrupt), *not* a quit key |
| `Ctrl+Q` | Quit and terminate the SSH session |
| `F5` or `Alt+R` | Retry the level-page fetch |
| `r` | Retry the fetch while the red fetch-error banner is shown |

The SSH pane keeps up to 2000 lines of scrollback. While you are viewing
history, the SSH block title shows `[scrollback: N lines up]` and its border
turns bright yellow. **Typing, pasting, or using any other key immediately
returns the view to live output** before the input reaches the remote shell;
`PageUp`/`PageDown` and the mouse wheel are the only inputs that stay in
history. Key-release events are ignored. Mouse capture is enabled for the
lifetime of the UI and disabled again on exit (including on panic).

### In secret-entry mode

| Key | Action |
| --- | --- |
| type / paste | Insert into the secret field |
| `Tab` | Toggle secret type: `password` ↔ `private key path` |
| `Backspace` / `Delete` / arrows / `Home` / `End` | Edit the field |
| `Enter` | Verify asynchronously against `bandit{next}` |
| `Esc` (or `Ctrl+G`) | Return to the SSH terminal, keeping the entered text |
| `Ctrl+C` or `Ctrl+Q` | Quit |
| `F5` / `Alt+R` | Retry the level-page fetch |

A failed verification keeps the current SSH session alive, shows the SSH error
in the status box, and preserves the input so you can fix it. Pressing `Enter`
again while a verification is running is ignored, so only one verifier runs at
a time. A successful verification saves the secret, signals the entire SSH
process group (so `ssh`/`sshpass` grandchildren cannot keep the PTY open), and
starts a fresh PTY for the next level with focus back in the terminal. When
`sshpass` is unavailable, that new session is a plain `ssh` with the saved
password injected once at the remote `password:` prompt; otherwise `sshpass`
feeds it and no injection happens. PTY teardown has a bounded reader-thread
join: if a child still holds the PTY, the UI detaches the reader instead of
hanging.

### Help overlay

| Key | Action |
| --- | --- |
| `F1` or `Ctrl+?` | Open the keybinding overlay (from either mode) |
| Any key, including `Esc` (or `F1` again) | Close it |

The overlay is a centered panel listing every keybinding grouped by context.
While it is open, **all keys and pastes are consumed** — the closing keypress is
not typed into the SSH session or the secret field — and mouse clicks/wheel
events do nothing. Closing it leaves the current focus and SSH scrollback
unchanged. `Ctrl+?` is accepted with or without SHIFT, depending on how your
terminal reports the keystroke; some emulators reserve `F1`, in which case use
`Ctrl+?`.

## Credentials and cache

| Content | Location |
| --- | --- |
| Verified secrets + last level | `~/.config/bandit-tui/credentials.json` |
| Remembered SSH host keys | `~/.config/bandit-tui/known_hosts` |
| OpenSSH askpass helper (fallback only) | `~/.config/bandit-tui/askpass.sh` |
| Level instruction cache (keyed by source page) | `~/.cache/bandit-tui/levels/bandit{L+1}.json` |

`XDG_CONFIG_HOME` and `XDG_CACHE_HOME` override those roots. On Unix the
config directory is `0700`; `askpass.sh` is `0700`; `credentials.json` and
`known_hosts` are `0600`. The helper script is recreated if missing or changed
and only prints `$BANDIT_TUI_ASKPASS_SECRET`; it never contains a password.
Credentials are written via a temporary file and rename. The JSON shape is:

```json
{
  "last_level": 14,
  "secrets": {
    "14": { "Password": "..." },
    "15": { "PrivateKeyPath": "/home/you/bandit/sshkey.private" }
  }
}
```

Levels are loaded from cache first; a miss triggers an HTTP fetch (with a
loading indicator). Cache filenames use the source page number: the
instructions shown while playing bandit1 are stored in `bandit2.json`. A miss,
an entry without the `page` field (old cache format), or an entry whose
session/page identity does not match is ignored and refetched. `F5`/`Alt+R`
forces a refetch. Fetch/parse failures are non-fatal: the SSH session stays
usable and an error banner with a retry hint is shown.

### Host-key trust trade-off

SSH uses `StrictHostKeyChecking=accept-new` with the dedicated `known_hosts`
file above: the first time a host is seen its key is trusted and remembered;
later connections reject a changed key. This avoids the MITM risk of the old
`/dev/null` approach, but it still means **the first connection is trusted on
first use (TOFU)** — verify the fingerprint out of band if that matters to you.
Delete the file if you need to reset trusted keys.

To start over, remove `credentials.json` (and optionally the level cache
directory) yourself.

## Private key workflow

Some levels hand out a private SSH key instead of a password (for example
bandit 14 → 15). The app verifies and reuses a **local copy** of the key:

1. In the embedded SSH session, locate the key on the remote host (for example
   `~/sshkey.private`) and read the level page for the exact path.
2. In another terminal, copy it to your machine and restrict permissions:

   ```sh
   scp -P 2220 bandit14@bandit.labs.overthewire.org:~/sshkey.private ~/bandit/sshkey.private
   chmod 600 ~/bandit/sshkey.private
   ```

   If the key lives on a second host reached by an SSH hop, `scp` from that
   host instead. You can also paste the key through any other method you
   prefer — the app only needs a readable local file.
3. Press `Alt+I` (or `Ctrl+P`), then `Tab` until the secret type says `private key path`.
4. Type the path (`~/bandit/sshkey.private` is expanded) and press `Enter`.

The app checks that the local file exists, then verifies with
`ssh -i <path> -o BatchMode=yes -o IdentitiesOnly=yes ... true`. On success the
path is saved and used for the next session's automatic login. For encrypted
keys, the `ssh` passphrase prompt appears in the embedded terminal; the
non-interactive verifier itself expects an unencrypted key.

## Implementation notes / limitations

- HTTP fetching and SSH verification run on dedicated threads with `mpsc`
  channels. Request/generation IDs discard stale responses. Verifiers are
  killed and reaped after a 20-second timeout.
- Password verification prefers `sshpass` and supplies the password through
  the `SSHPASS` environment variable (`sshpass -e`). Without `sshpass`, the app
  writes a `0700` helper script in the config dir and runs
  `ssh -o PreferredAuthentications=password -o PubkeyAuthentication=no
  -o NumberOfPasswordPrompts=1 -o BatchMode=no ... true` with
  `DISPLAY=:0`, `SSH_ASKPASS`, `SSH_ASKPASS_REQUIRE=force` and
  `BANDIT_TUI_ASKPASS_SECRET`. OpenSSH 8.4+ is needed for the forced askpass
  mode. Both paths keep stdin closed, drain stderr with a bounded read, and
  kill the verifier's process group at the 20-second timeout. Error text is
  redacted so an echoed password cannot reach the status box.
- Automatic verification/advance only targets the direct
  `bandit{current+1}@bandit.labs.overthewire.org:2220` accounts. **Levels that
  require an SSH hop from the remote host or a non-standard target are not
  auto-advanced**: play those levels manually in the embedded terminal, then
  enter the secret found there for the next direct level. Private-key levels
  still auto-advance, but the key must be copied to a local file first (see
  above). Level 33 is the final level; no further verification is attempted.
- Verifiers get their own process group and are killed as a group after the
  20-second timeout; stderr is drained on a helper thread with a bounded read
  so a stalled child cannot hang verification.
- The instructions pane clips long pages rather than scrolling; the SSH pane
  renders `vt100` scrollback by changing the screen's scrollback offset, so no
  copy of the widget is needed. The secret field uses character indices,
  so multibyte UTF-8 editing is safe and capped at 4096 bytes.
- Terminal raw mode/alternate screen are restored on normal exit, on `SIGINT`
  or `SIGTERM` (via a flag consumed by the event loop), and on panic.
- Tests are offline unit tests (HTML fixture parsing, cache/secret serde and
  permissions, path/host-key and askpass-script helpers, command construction
  with a forced no-`sshpass` path, prompt detection and one-shot PTY injection,
  layout math, key translation, UTF-8 input editing, stale messages, mode
  switching, click hit testing, help-overlay key handling, bounded PTY/verifier
  teardown with local stub `sh` commands). Live
  HTTP and real SSH authentication are **not** integration-tested here.
