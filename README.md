# MalasDoh

A native SSH client for people who log in to servers to patch files and fix
configs. It combines a terminal, an SFTP file browser and an editor in one
window, and it is careful about the one thing that matters most when you edit
a file on a live server: **not breaking it**.

Written in Rust. One binary for Windows, Linux and macOS, with no Electron, no
browser engine and no PuTTY/OpenSSH runtime dependency.

![Terminal and file browser](docs/screenshots/terminal-and-browser.png)

## Why another SSH client

electerm, WinSCP and Termius each do part of this well. MalasDoh focuses on the
remote-editing loop, where they are weakest:

| When you save a remote file… | MalasDoh |
| --- | --- |
| Do you see what you changed? | A diff is shown before writing (can be turned off). |
| Did someone else change it meanwhile (a colleague, Ansible, a deploy)? | The file is re-read first. If it changed, nothing is written; you get their version vs yours, and **if the edits touch different lines you can keep both** (three-way merge). |
| Can you undo it? | The previous contents are always backed up on your computer (optionally also as `file.bak-<time>` on the server). Any backup can be restored, through the same diff review. |
| Does it keep owner, group, mode, hard links, SELinux labels? | Yes. The file is rewritten in place, not replaced. |
| Does it keep CRLF line endings, a BOM, a missing final newline? | Yes, byte for byte on the lines you did not touch. |
| Did the write actually land? | The file is read back after saving and must match. |
| The file is owned by root? | "Edit as root (sudo)", or "Save as root" when a save hits *Permission denied*. |
| Is the config still valid? | Matching files are checked as part of saving: `visudo -c`, `sshd -t`, `nginx -t`, `apachectl configtest`, HAProxy, BIND, systemd units, fstab, Caddy, PostgreSQL, and JSON/TOML/YAML syntax. Checks that must not see a broken file (sudoers) run on a copy first; others run in place and **the original is put back automatically** if the check fails. An optional reload command runs only after a pass. |

![Someone else changed the file: keep both changes](docs/screenshots/conflict-merge.png)

## Features

- **Connections**: saved profiles with groups and search; jump hosts
  (ProxyJump) with chains; import from `~/.ssh/config`; duplicate; copy as an
  `ssh` command.
- **Proxies**: reach the first server through SOCKS5, an HTTP proxy
  (`CONNECT`), or any command (`ProxyCommand`, e.g. `nc -X connect -x
  proxy:3128 %h %p`, imported from `~/.ssh/config`). Proxy passwords are
  asked for and kept only in the OS keychain.
- **Keepalive** per connection (default every 30 s), so idle sessions survive
  NAT and firewalls and a dead link is noticed.
- **Export / import** your connections, snippets and accepted host keys as
  one file encrypted with a passphrase (PBKDF2-SHA256 + ChaCha20-Poly1305),
  to back up or move to another computer. Importing only adds what is new.
- **Port forwarding**: local (`-L`), remote (`-R`) and SOCKS5 (`-D`), saved per
  connection or added for one session, started with the connection if you
  like, and restarted after a reconnect, with live connection and byte counts.
- **Authentication**: SSH agent (OpenSSH agent and Pageant on Windows,
  `SSH_AUTH_SOCK` elsewhere), key files (OpenSSH, PEM, **PuTTY `.ppk`**,
  encrypted or not), password and keyboard-interactive (including OTP prompts).
  Passwords, key passphrases and sudo passwords are only stored if you tick
  "remember", and then in the OS keychain (Windows Credential Manager, macOS
  Keychain, Secret Service), never in MalasDoh's files. A saved sudo password
  that stops working is forgotten and you are asked again (not retried, since
  failed sudo attempts are logged and can lock the account). Per connection
  you can also let sudo try the saved login password first.
- **Host keys**: checked against your existing `~/.ssh/known_hosts` (so hosts
  you trust in `ssh` are trusted here) and MalasDoh's own file. A new host shows
  its SHA256 fingerprint for you to confirm. A *changed* or `@revoked` key is
  refused outright and never offered as "accept anyway". Host certificates
  signed by a `@cert-authority` you trust are accepted without a prompt.
- **SSH keys** (Tools → SSH keys): make an Ed25519 key (with a passphrase if
  you like), put any key on a server like `ssh-copy-id` does (owner-only
  permissions, never added twice), then switch the connection to log in with
  it.
- **Import** connections from `~/.ssh/config`, PuTTY (registry on Windows,
  `~/.putty/sessions` elsewhere) and WinSCP (registry or `WinSCP.ini`, with
  its folders). Saved passwords are not imported.
- **Certificates for you**: `id_ed25519-cert.pub` next to your key, or a
  certificate in your agent, is used automatically.
- **Terminal**: xterm-256color and truecolor, full-screen programs (vim, nano,
  htop, less), scrollback, selection and copy, bracketed paste, several
  terminals per connection. Ctrl+C copies when text is selected and interrupts
  otherwise. Search the scrollback (Ctrl+Shift+F), and log a terminal to a
  plain-text file. Colour themes (Tomorrow Night, Solarized dark and light,
  Dracula, Gruvbox, Light) and your own monospace font file. Right click
  copies a selection or pastes, middle click pastes, copy-on-select if you
  like; pasting several lines into a shell asks first. Ctrl+click opens
  links. Programs that use the mouse (htop, mc, vim) get it; Shift+drag still
  selects.
- **Split**: two panes side by side, any mix of terminals, files and logs.
- **Type into several terminals** (Tools): what you type goes to the servers
  you tick, under a red bar so it is never on by accident.
- **Agent forwarding** per connection (`ssh -A`), off by default, never to
  jump hosts; agent requests from servers without it are refused.
- **File browser**: navigate, filter, hidden files, upload (button or drag and
  drop from the desktop) and download (files or whole folders, with progress and
  cancel), new file/folder, rename, delete, chmod, copy path, "cd here in
  terminal". Nothing is overwritten without asking first. Sort by name, size
  or date; bookmark folders per connection; change owner (`chown`, with sudo)
  and create symlinks; pack a selection into `.tar.gz`/`.tar.xz`/`.zip` and
  unpack archives into a new folder (never over existing files).
- **Browse as root**: the "root" button in the file browser switches it to
  SFTP through sudo (the server's own `sftp-server`), so folders like
  `/etc/ssl/private` or `/root` can be listed, transferred and edited. A red
  badge shows it is on; files opened then are edited as root, with checks
  run as root too.
- **Apply a patch** (`+ New → Apply a patch…`): paste or open a `diff -u` /
  `git diff`, choose the folder it is relative to (and `-p`-style strip), and
  Check: every file is read and every change must match exactly (it may have
  moved, but no fuzz). You see each file's diff; nothing is written unless all
  match, and then each file is saved like an edit (conflict check, backup,
  config check, journal), stopping at the first that fails. New files are
  created; deleting files is left to you. Applying twice does nothing.
- **Find files**: by name pattern and/or text inside, from any folder, as
  root if needed; click a result to show it, double-click to edit.
- **Follow a log** (`tail -F`, also as root): new lines stream in, with a
  filter, pause, errors and warnings coloured, and it keeps going across log
  rotation. Closing the tab stops `tail` on the server.
- **Two panes**: "Local files" puts this computer's folders next to the
  server's, WinSCP style; drag either way, or use the Upload button.
- **Copy between servers**: send files or folders from one connected server to
  another (they pass through a private temporary folder on your computer, so the
  servers do not need to reach each other).
- **Compare files**: a file against the same (or any) path on another server, a
  file on your computer, or one of its backups; or two selected files. Context
  of 3 or 10 lines or the whole file, swap sides, copy as a patch.
- **Change journal** (Tools → Change journal): every file you save, when, on
  which server, what changed (as a patch), and how the config check went,
  including saves that were rolled back. Search it by server, path or changed
  text, and compare any save's backup with the server's file now. "Change
  history…" on a file shows just that file.
- **Folder compare and sync**: local to server, server to local, or both ways
  (newer wins), by size and time and optionally contents. You see every copy
  and deletion first and can untick any of them; deletes are opt-in.
- **Servers without SFTP** (busybox, dropbear, routers, appliances): files go
  through plain shell commands instead, and everything above still works.
- **Editor**: syntax highlighting for nginx/Apache/sshd-style configs,
  INI/systemd/TOML, YAML, JSON, shell, `.env`, XML and Dockerfiles; line
  numbers with a gutter marking lines added, changed or removed since the file
  was opened (hover a mark to see what was there), auto-indent, Tab/Shift+Tab on selections, find and replace (Ctrl+F,
  Ctrl+H), go to line (Ctrl+G), Ctrl+S, and the save safety above. Or open the file in your
  own editor (VS Code, Notepad++, vim…): each save there is uploaded with the
  same checks.
- **Snippets**: saved commands in folders with `{{placeholders}}` asked for on
  use; insert into the terminal, run, or send to many hosts.
- **Run on many hosts**: pick hosts or whole groups, run a command in parallel,
  and compare exit codes and output side by side; retry the failures.
- **Edit a file on many servers** (Tools menu, or "Edit on many servers…" on a
  file): load the file from one server, change it, and Preview shows what that
  does to each server's own copy. "Apply my change" carries only the lines you
  changed, so each server keeps its own differences (names, sizes); a copy
  that does not match where the change goes is left alone, and one placed by
  the changed lines alone (its neighbours differ) is marked for a closer look.
  "Same content everywhere" sends the whole text. Saving goes server by server
  with the usual conflict check, backup, config check and journal entry,
  stopping at the first failure unless told to keep going. As root, each
  server's saved sudo password is used; servers without one ask once (tick
  "remember" and they will not ask again).
- **PostgreSQL**: saving `postgresql.conf`, a `conf.d` file or
  `postgresql.auto.conf` runs that cluster's own `postgres -C` over the whole
  configuration (as the `postgres` user, running or not), so a typo, a bad
  value or a setting the version dropped is refused and the original put
  back. `pg_hba.conf` and `pg_ident.conf` are checked by the running server
  (`pg_hba_file_rules`), line by line; a stopped server cannot, which is
  said. A passing save says which changes need a restart (`shared_buffers`)
  rather than a reload. Debian/Ubuntu (`/etc/postgresql/<version>/<name>`)
  and data-folder layouts (Red Hat, source builds) are both found. The editor
  also flags settings the cluster's version no longer accepts, such as
  `wal_keep_segments` in 13+ or `promote_trigger_file` in 16+, with what to
  use instead; click one to go to it. For a check that also reloads, set the
  rule's reload to `builtin:postgresql-reload` in Settings → Checks.
- **Lasting terminals** (the ⏷ next to + Terminal): the shell runs in tmux
  (or screen) on the server, so `pg_upgrade`, a dump or a release upgrade
  keeps going when the connection drops. The tab goes back into it on
  reconnect, and after restarting MalasDoh; closed tabs can be reattached
  from the same menu. Type `exit` in it to end it.
- **Resilience**: a dropped connection is detected; open files keep your
  unsaved edits, and Reconnect picks up where you were. Unsaved edits are
  also written to disk every few seconds, so after a crash, a power cut or
  quitting with changes, the next connect offers them back; if the server's
  copy changed meanwhile, saving goes through the usual conflict and merge.
- **Pick up where you left off**: the start screen offers "Reopen last
  session" (or turn on reopening automatically in Settings): connections come
  back with their folders, open files, terminals and local pane. Only paths
  are kept, never file contents.
- **Per connection**: a startup command typed into each new terminal
  (`sudo -i`, `cd /var/www`), and "File browser follows the terminal's
  folder" (bash and zsh, or any shell that reports its folder with OSC 7).
- **Portable mode**: put a file named `malasdoh.portable` next to the
  executable and everything (profiles, known hosts, backups) is kept in a
  `data` folder beside it.

## Download

Tagged releases (from `.github/workflows/release.yml`) publish:

- **Windows**: `malasdoh-<version>-windows-x86_64.zip`, and a `-portable.zip`
  that keeps everything in a folder next to the exe.
- **Linux**: a `.deb` (desktop entry and icon included) and a `.tar.gz`.
- **macOS**: a universal (Apple Silicon and Intel) `.dmg`.

Builds are not code-signed yet: Windows SmartScreen and macOS Gatekeeper will
ask before the first run (macOS: right-click → Open).

If MalasDoh closes unexpectedly, `crash.log` in its data folder says why.
[docs/windows-testing.md](docs/windows-testing.md) is a 15-minute checklist
for trying a Windows build.

## Building

You need a recent stable Rust toolchain (1.89 or newer).

```sh
cargo build --release
# the app:
./target/release/malasdoh        # malasdoh.exe on Windows
```

Linux needs the usual desktop libraries at run time (X11 or Wayland, OpenGL,
`libxkbcommon`); every desktop distribution has them.

## Where things are kept

| What | Where |
| --- | --- |
| Profiles | `profiles.toml` in the config directory (`%APPDATA%\MalasDoh\config`, `~/.config/malasdoh`, `~/Library/Application Support/MalasDoh`) |
| Settings | `settings.toml` beside it |
| Host keys you accepted | `known_hosts` in the data directory (`~/.ssh/known_hosts` is read but never written) |
| Backups of edited files | `backups/<host>/<remote path>/<file>.<UTC time>` in the data directory, owner-only permissions |
| Change journal | `journal.jsonl` in the data directory (one JSON line per save, owner-only permissions; the patches can contain secrets from config files) |
| Snippets | `snippets.toml` in the config directory |
| What was open last time | `session.toml` in the config directory (paths only) |
| Unsaved edits | `drafts/` in the data directory, owner-only, removed when saved or discarded |
| Terminal logs | `logs/<host>/` in the data directory, owner-only permissions |

Settings → shows the exact paths. `MALASDOH_HOME=/some/dir` overrides all of
them.

## Project layout

- `crates/malasdoh-core`: everything that touches the network or disk.
  Profiles, `known_hosts`, SSH connect/auth (on [`russh`](https://github.com/warp-tech/russh),
  pure Rust), SFTP and the shell fallback, port forwarding, folder sync, config
  checks, the safe-edit logic, diff and three-way merge. No GUI code.
- `crates/malasdoh`: the [egui](https://github.com/emilk/egui) desktop app.

## Tests

```sh
cargo test --workspace
```

`crates/malasdoh-core/tests/sshd.rs` starts a real OpenSSH `sshd` on a free
local port (throwaway keys, nothing on the machine is touched) and checks the
whole path end to end: host-key trust and refusal, passphrase retry and
remember, jump hosts, host and user certificates, shell, exec, SFTP transfers,
the no-SFTP shell fallback, safe save, conflict detection, CRLF and permission
preservation, sudo editing, config checks with rollback (including real
`visudo`), port forwarding in all three directions, and folder sync. If `sshd` is not
installed those tests are skipped; CI installs it.

See [`docs/roadmap.md`](docs/roadmap.md) for what is planned next.
