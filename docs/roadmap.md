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
- Editor: auto-indent, Tab/Shift+Tab, find and replace, go to line
- SSH key generation and install (ssh-copy-id); import from PuTTY and WinSCP
- Reopen last session; startup command per connection; browser follows the
  terminal's folder
- Snippets, and running commands on many hosts
- External editor round trip with the same checks
- Disconnect detection and reconnect without losing unsaved edits
- Release builds: Windows zip and portable zip, Linux .deb and tarball,
  macOS universal .dmg

## Next

1. Code signing for the Windows and macOS builds (needs certificates).
2. Split panes: two terminals, or a terminal and an editor, side by side.
3. Editor: a gutter marking lines changed since opening.
4. Optional SFTP "edit as root" through `sudo sftp-server` where allowed,
   which is faster than the current sudo + cat path for large trees.
5. Mouse reporting in the terminal (for htop/mc mouse use).

## Known limitations

- `sudo` editing needs `sudo` to work without a TTY (`Defaults requiretty`
  breaks it, with a clear error).
- Files over 20 MiB are not opened in the built-in editor.
- The shell fallback needs `stat -c` (GNU coreutils or busybox); BSD-only
  userlands without SFTP are not supported.
