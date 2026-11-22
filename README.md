# wargames-tui

A terminal UI for playing several SSH-based wargames from one binary. It shows
the current level's instructions next to a real, embedded SSH terminal,
verifies next-level secrets in the background, and moves on when a secret is
accepted.

The default catalog includes every SSH-based OverTheWire game and the five
UnderTheWire games:

| Id | Game | SSH endpoint | Levels |
| --- | --- | --- | --- |
| `bandit` | [OverTheWire Bandit](https://overthewire.org/wargames/bandit/) | `bandit.labs.overthewire.org:2220` | 0–33 |
| `leviathan` | [OverTheWire Leviathan](https://overthewire.org/wargames/leviathan/) | `leviathan.labs.overthewire.org:2223` | 0–7 |
| `krypton` | [OverTheWire Krypton](https://overthewire.org/wargames/krypton/) | `krypton.labs.overthewire.org:2231` | 1–7 |
| `narnia` | [OverTheWire Narnia](https://overthewire.org/wargames/narnia/) | `narnia.labs.overthewire.org:2226` | 0–9 |
| `behemoth` | [OverTheWire Behemoth](https://overthewire.org/wargames/behemoth/) | `behemoth.labs.overthewire.org:2221` | 0–8 |
| `utumno` | [OverTheWire Utumno](https://overthewire.org/wargames/utumno/) | `utumno.labs.overthewire.org:2227` | 0–9 |
| `maze` | [OverTheWire Maze](https://overthewire.org/wargames/maze/) | `maze.labs.overthewire.org:2225` | 0–8 |
| `vortex` | [OverTheWire Vortex](https://overthewire.org/wargames/vortex/) | `vortex.labs.overthewire.org:2228` | 1–26 |
| `manpage` | [OverTheWire Manpage](https://overthewire.org/wargames/manpage/) | `manpage.labs.overthewire.org:2224` | 0–6 |
| `drifter` | [OverTheWire Drifter](https://overthewire.org/wargames/drifter/) | `drifter.labs.overthewire.org:2230` | 0–15 |
| `formulaone` | [OverTheWire FormulaOne](https://overthewire.org/wargames/formulaone/) | `formulaone.labs.overthewire.org:2232` | 0–5 |
| `century` | [UnderTheWire Century](https://underthewire.tech/wargames/century) | `century.underthewire.tech:22` | 1–15 |
| `cyborg` | [UnderTheWire Cyborg](https://underthewire.tech/wargames/cyborg) | `cyborg.underthewire.tech:22` | 1–15 |
| `groot` | [UnderTheWire Groot](https://underthewire.tech/wargames/groot) | `groot.underthewire.tech:22` | 1–15 |
| `oracle` | [UnderTheWire Oracle](https://underthewire.tech/wargames/oracle) | `oracle.underthewire.tech:22` | 1–15 |
| `trebek` | [UnderTheWire Trebek](https://underthewire.tech/wargames/trebek) | `trebek.underthewire.tech:22` | 1–15 |

Natas is intentionally not included: it is web-only and has no SSH login.
Vortex, Drifter, and FormulaOne begin with a non-SSH challenge; solve that
challenge outside the app, then enter the resulting password at the SSH prompt.

Connections are made directly from your machine; there is no shared proxy.

## Requirements

- Rust 1.88+ (edition 2024), `cargo`
- An `ssh` client in `PATH`
- For password authentication, either OpenSSH **8.4+** (the app uses
  `SSH_ASKPASS_REQUIRE=force` with a generated helper script — no extra
  packages) **or** `sshpass` in `PATH` if you already have it. `sshpass` is
  optional: when present it is used; otherwise the app falls back to OpenSSH's
  `SSH_ASKPASS`. Private-key secrets need neither.
- Network access to the game's instruction site for uncached pages

Password verification is fully non-interactive and time-bounded in both modes;
neither path requires you to watch a prompt.

## Install and run

After the crate is published:

```sh
cargo install wargames-tui
wargames-tui
```

From a checkout:

```sh
cargo install --path .
# or
cargo build --release
./target/release/wargames-tui
# during development
cargo run
```

The source repository is
`https://github.com/world-class-engineers/wargames-tui` (SSH remote:
`git@github.com:world-class-engineers/wargames-tui.git`). Publishing to
crates.io is a manual maintainer step (`cargo publish` from a clean checkout);
there is no automatic release pipeline in this repository.

## Wargame picker

The picker appears on **every startup** with the last played game preselected.
Use `Up`/`Down` (or click a row) to choose, then `Enter` (or click) to start.
`Esc` at startup starts the preselected game. Press `F2` any time to reopen the
picker; when opened with `F2`, `Esc` cancels without changing games. Each row
shows that game's own cleared progress (`Cleared 2/14`, or `Done` when
complete). Switching games saves the choice, starts a fresh session, and
filters out any fetch or verification result that belongs to the previous game.

No PTY is spawned and no level page is fetched until a game is confirmed.

The catalog drives progress labels, endpoint, username pattern, page mapping,
initial public secret, allowed secret kinds, verification command, and
completion message. For Bandit, the first run logs in as `bandit0` with password
`bandit0`; OverTheWire's "Level X" page describes getting **into** banditX, so
while playing `banditL` the app fetches `bandit{L+1}.html`. Bandit 33 is the
final playable level and the placeholder page 34 is never fetched. The other
OverTheWire games use page `L` directly; Krypton and Vortex start at SSH level 1
because their level 0 challenges are solved outside SSH. UnderTheWire games
start at level 1, use page `L`, and treat level 15 as the final congratulations
screen (14 challenges cleared). A game's
`initial_secret` is optional: with no public starting password, the SSH prompt
asks you for it.

## Layout and keybindings

Left pane: level goal, commands, helpful reading, notes, and the next-level
secret box. Right pane: the embedded SSH session (colors, attributes, cursor,
and scrolling are rendered by `vt100`). Bottom line: context-sensitive hints.

Clicking a pane focuses it: the *Next Level Secret* box switches to secret
entry, and the SSH box switches back to the terminal. Clicks are UI actions and
are never sent to the remote shell.

### Wargame picker

| Key | Action |
| --- | --- |
| `Up` / `Down` | Move selection |
| `Enter` | Start the selected game |
| `Esc` | At startup: start the preselected game; after `F2`: cancel |
| `F2` | Cancel a reopened picker / close |
| Left-click a row | Select and start that game |

### In the SSH terminal (default focus)

| Key | Action |
| --- | --- |
| type / paste | Sent to the SSH PTY |
| `PageUp` / `PageDown` | Scroll SSH scrollback one screenful (not sent to the remote shell) |
| Mouse wheel over the SSH pane | Scroll a few lines per tick |
| Drag in the SSH pane | Select terminal text |
| `Ctrl+P` | With a selection: insert it as the password secret and copy it; without one: enter secret mode |
| `Alt+I` | Enter secret-entry mode |
| `Alt+M` | Toggle native terminal selection |
| `F2` | Reopen the wargame picker |
| `Ctrl+C` | Sent to the remote shell (interrupt), *not* a quit key |
| `Ctrl+Q` | Quit and terminate the SSH session |
| `F5` or `Alt+R` | Retry the level-page fetch; if the SSH session has ended, reconnect instead |
| `r` | Retry while the red fetch-error banner is shown |
| `F1` or `Ctrl+?` | Open the keybinding overlay |

The SSH pane keeps up to 2000 lines of scrollback. While viewing history, the
title shows `[scrollback: N lines up]` and its border turns bright yellow.
Typing, pasting, or any non-scroll key returns to live output before the input
reaches the remote shell. Key-release events are ignored.

Mouse capture is enabled on startup and disabled on exit (including on panic).
It is toggled only when `Alt+M` changes the mode.

### Text selection and clipboard

Dragging inside the SSH pane selects terminal cells. The selected text is
copied with a best-effort OSC 52 escape sequence; support depends on your
terminal. `Ctrl+P` then inserts that text into the secret box as a
`password`, focuses the box, clears the selection highlight, and copies the
text again. Any other click, scroll, paste, or resize clears the selection.

`Alt+M` disables mouse capture so your terminal can perform its own native
selection. While native mode is on, the app ignores all mouse events; only
`Alt+M` turns app mouse handling back on. The status line announces each
change.

### In secret-entry mode

| Key | Action |
| --- | --- |
| type / paste | Insert into the secret field |
| `Tab` | Toggle secret type: `password` ↔ `private key path` (when the game allows both) |
| `Backspace` / `Delete` / arrows / `Home` / `End` | Edit the field |
| `Enter` | Verify asynchronously against the next level of the active game |
| `Esc` (or `Ctrl+G`) | Return to the SSH terminal, keeping the entered text |
| `Ctrl+C` or `Ctrl+Q` | Quit |
| `F5` / `Alt+R` | Retry the level-page fetch; reconnect if the SSH session has ended |
| `F2` | Reopen the wargame picker |
| `F1` or `Ctrl+?` | Open the keybinding overlay |

A failed verification keeps the current SSH session alive, shows the error in
the status box, and preserves the input. Only one verifier runs at a time. A
successful verification saves the secret, terminates the SSH process group, and
starts a fresh PTY for the next level with focus back in the terminal.

### Help overlay

| Key | Action |
| --- | --- |
| `F1` or `Ctrl+?` | Open the keybinding overlay (from either mode) |
| Any key, including `Esc` | Close it |

While the overlay is open, all keys and pastes are consumed — the closing
keypress is not typed into the SSH session or the secret field — and mouse
events do nothing. `Ctrl+?` is accepted with or without SHIFT, depending on how
your terminal reports it; some emulators reserve `F1`.

## Configuration

### Wargame catalog (TOML)

On first run, the embedded default catalog is written to:

| Content | Location |
| --- | --- |
| Wargame catalog (TOML) | `~/.config/wargames-tui/wargames.toml` |
| Verified secrets + progress | `~/.config/wargames-tui/credentials.json` |
| Remembered SSH host keys | `~/.config/wargames-tui/known_hosts` |
| OpenSSH askpass helper (fallback only) | `~/.config/wargames-tui/askpass.sh` |
| Level instruction cache | `~/.cache/wargames-tui/levels/<game_id>/<page>.json` |

`XDG_CONFIG_HOME` and `XDG_CACHE_HOME` override those roots. The catalog file
is **yours once seeded**: the app never overwrites or merges it. An invalid or
malformed TOML file prints a clear, path-prefixed error and exits instead of
falling back to the built-in defaults. Edits take effect on the next launch.

The embedded copy is `wargames.toml` in the source checkout. Each `[[wargames]]`
entry defines `id`, `name`, `homepage`, `ssh_host`, `ssh_port`,
`username_template`, `level_label`, `first_level`, `final_level`, `page_offset`,
`allowed_secret_kinds`, `initial_secret`, `verification_remote_command`, and
`completion_message`. The `[wargames.source]` table chooses either `html`
parsing (with CSS selectors and a page validation marker) or `js_json`
discovery (fetching a module script, decoding a `JSON.parse('…')` literal, and
mapping JSON fields). Both source types record a `cache_version`; changing it
in your config invalidates old cached pages for that game.

### Credentials file

The JSON shape is versioned and namespaced by game id:

```json
{
  "version": 2,
  "last_wargame": "bandit",
  "games": {
    "bandit": {
      "last_level": 14,
      "secrets": {
        "14": { "Password": "..." },
        "15": { "PrivateKeyPath": "/home/you/bandit/sshkey.private" }
      }
    }
  }
}
```

Keys in `secrets` are the levels the secret unlocks. A malformed credentials
file is reported as a startup warning and the app starts with empty progress;
it is not silently overwritten. On Unix the config directory is `0700`;
`askpass.sh` is `0700`; `credentials.json`, `known_hosts`, and the helper's
temporary files are `0600`. Credentials are written via a temporary file and
rename. The askpass helper only prints `$WARGAMES_TUI_ASKPASS_SECRET`; it never
contains a password.

### Clean rename from `bandit-tui`

This is a new application identity, not an in-place upgrade. The old
`~/.config/bandit-tui` and `~/.cache/bandit-tui` directories are deliberately
**never read, copied, migrated, or deleted**. **Bandit progress does not carry
over** — start Bandit again in the picker and re-enter/verify any secrets you
need. You may remove the old directories yourself if you no longer want them.
There is no `bandit-tui` compatibility binary.

### Level cache

Pages are loaded from cache first; a miss triggers an HTTP fetch with a loading
indicator. Cache entries carry the game id, source cache version, level, and
page, and are rejected if any of those do not match the current request. A
missing, stale, or incomplete entry is refetched. `F5`/`Alt+R` forces a
refetch while the SSH session is alive, and reconnects when it has ended.
Fetch/parse failures are non-fatal: the SSH session stays usable and an error
banner with a retry hint is shown.

### Host-key trust trade-off

SSH uses `StrictHostKeyChecking=accept-new` with the dedicated `known_hosts`
file above: the first time a host is seen its key is trusted and remembered;
later connections reject a changed key. This avoids the MITM risk of using
`/dev/null`, but it still means **the first connection is trusted on first use
(TOFU)** — verify the fingerprint out of band if that matters to you. Delete
the file to reset trusted keys.

To start a game over, remove its entry from `credentials.json` (and optionally
its cache directory under `levels/`) yourself.

## Private key workflow

Bandit can hand out a private SSH key instead of a password. The app verifies
and reuses a **local copy** of the key:

1. In the embedded SSH session, locate the key on the remote host (for example
   `~/sshkey.private`) and read the level page for the exact path.
2. In another terminal, copy it to your machine and restrict permissions:

   ```sh
   scp -P 2220 bandit14@bandit.labs.overthewire.org:~/sshkey.private ~/bandit/sshkey.private
   chmod 600 ~/bandit/sshkey.private
   ```

   If the key lives on a second host reached by an SSH hop, `scp` from that
   host instead. The app only needs a readable local file.
3. Press `Alt+I` (or `Ctrl+P`), then `Tab` until the secret type says
   `private key path`.
4. Type the path (`~/…` is expanded) and press `Enter`.

The app checks that the local file exists, then verifies with
`ssh -i <path> -o BatchMode=yes -o IdentitiesOnly=yes … <remote command>`.
UnderTheWire games are password-only in the default catalog, so `Tab` reports
that instead of switching kinds. Encrypted keys may prompt in the embedded
terminal; the non-interactive verifier expects an unencrypted key.

## Implementation notes / limitations

- HTTP fetching and SSH verification run on dedicated threads with `mpsc`
  channels. Request/generation IDs discard stale responses, including results
  from a previously selected wargame. Verifiers are killed and reaped after a
  20-second timeout.
- Password verification prefers `sshpass` (`sshpass -e`) and falls back to a
  generated `0700` askpass helper with `SSH_ASKPASS_REQUIRE=force`. Error text
  is redacted so an echoed password cannot reach the status box.
- Auto-advance targets the next direct account defined by the active catalog
  entry. Levels that require an SSH hop from the remote host or a non-standard
  target are not auto-advanced: play them manually, then enter the secret found
  there for the next direct level. Final levels show their configured
  completion message and do not verify further.
- The instructions pane clips long pages rather than scrolling. The secret
  field uses character indices, so multibyte UTF-8 editing is safe and capped
  at 4096 bytes.
- Terminal raw mode, bracketed paste, the alternate screen, and mouse capture
  are restored on normal exit, on `SIGINT`/`SIGTERM`, and on panic.
- Tests are offline unit tests covering catalog validation and seeding,
  credential/cache serde and permissions, HTML and JS-JSON fixtures, command
  construction with a forced no-`sshpass` path, prompt detection and one-shot
  PTY injection, layout math, selection hit testing, picker/mode/help key
  handling, stale messages, and bounded PTY/verifier teardown with local stub
  `sh` commands. Live HTTP and real SSH authentication are **not**
  integration-tested here.

Built with [ratatui](https://ratatui.rs) and ideas from the wider
[awesome-ratatui][^awesome-ratatui] ecosystem.

[^awesome-ratatui]: https://github.com/ratatui/awesome-ratatui
