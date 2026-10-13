# Roadmap

## Done

- Profiles, groups, jump-host chains, `~/.ssh/config` import
- Agent / key (incl. `.ppk`) / password / keyboard-interactive auth, OS keychain
- Strict host-key checking with `~/.ssh/known_hosts` interop; host certificates
  from trusted CAs; user certificates from key files and the agent
- Terminal: vt100, 256/true colour, full-screen apps, scrollback search, logs,
  answers to cursor/device queries, several terminals per host
- SFTP browser with recursive transfers, drag and drop, chmod/rename/delete
- Shell-command fallback for servers without SFTP
- Safe remote editing: review diff, conflict detection, three-way merge,
  local + optional remote backups, in-place writes, line ending/BOM
  preservation, read-back verification, sudo editing, restore from backup
- Config checks on save with automatic rollback, and reload after a pass
- Syntax highlighting for common server config formats
- Port forwarding: local, remote, SOCKS5
- Folder compare and sync with a reviewable plan
- Two-pane local/remote browser, drag both ways, copy between servers,
  confirmation before anything is overwritten
- Compare files across servers, with local files and with backups
- Change journal of every save, with patches, check results and rollbacks
- Editor: auto-indent, Tab/Shift+Tab, find and replace, go to line, changed-lines gutter
- PostgreSQL: config check with rollback (`postgres -C`, `pg_hba_file_rules`), restart-needed notes, warnings for removed settings
- Lasting terminals (tmux/screen) that survive disconnects and app restarts
- Transfer queue: parallel files, pause/resume, continue interrupted files, temp-name writes, kept times, SHA-256 checks on the server, skip patterns; compression per connection
- Teleport: password + authenticator login without tsh, server list, proxy jump (separate SSH port or TLS routing), CA trust, login-expired prompt
- Terminal: themes, font file, copy-on-select, right/middle-click paste,
  multi-line paste check, links, mouse reporting, split panes, typing into
  several servers, agent forwarding
- Browser: sort, bookmarks, chown, symlinks, archives, find files, follow logs
- Connections: SOCKS5 / HTTP / ProxyCommand proxies, keepalive setting,
  encrypted export and import
- SSH key generation and install (ssh-copy-id); import from PuTTY and WinSCP
- Apply patch files (all-or-nothing check, saved like edits)
- Browse as root through `sudo sftp-server`
- Crash recovery: drafts of unsaved edits, restored through conflict checks
- Reopen last session; startup command per connection; browser follows the
  terminal's folder
- Snippets, and running commands on many hosts
- Editing one file on many servers (per-server preview, safe saves)
- Saved sudo passwords in the OS keychain (forgotten when rejected), optional sudo with the saved login password
- External editor round trip with the same checks
- Disconnect detection and reconnect without losing unsaved edits
- Release builds: Windows zip and portable zip, Linux .deb and tarball,
  macOS universal .dmg

## Next

1. Code signing for the Windows and macOS builds (needs certificates).
2. Run through docs/windows-testing.md on real Windows and fix what turns up.
3. Teleport: per-session MFA (`require_session_mfa`), SSO and security-key logins.
4. rsync as an optional speed-up when the server has it (large files that changed a little).
5. From the WinSCP list: keep a server folder up to date from a local one, synchronized browsing, delete to a trash folder, folder sizes.

## Known limitations

- `sudo` editing needs `sudo` to work without a TTY (`Defaults requiretty`
  breaks it, with a clear error).
- Files over 20 MiB are not opened in the built-in editor.
- The shell fallback needs `stat -c` (GNU coreutils or busybox); BSD-only
  userlands without SFTP are not supported.
