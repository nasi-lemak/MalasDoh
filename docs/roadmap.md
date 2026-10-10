# Roadmap

## Done (0.1)

- Profiles, groups, jump-host chains, `~/.ssh/config` import
- Agent / key (incl. `.ppk`) / password / keyboard-interactive auth, OS keychain
- Strict host-key checking with `~/.ssh/known_hosts` interop
- Terminal (vt100, 256 colours, full-screen apps, scrollback, several per host)
- SFTP browser with recursive transfers, drag and drop, chmod/rename/delete
- Safe remote editing: review diff, conflict detection, three-way merge,
  local + optional remote backups, in-place writes, line ending/BOM
  preservation, read-back verification, sudo editing, restore from backup
- External editor round trip with the same checks
- Disconnect detection and reconnect without losing unsaved edits

## Next

Ordered by how much they help the patch-and-fix-configs workflow.

1. **Validate before save**: run a check command after writing and roll back
   automatically if it fails. Presets for `nginx -t`, `sshd -t`, `visudo -c`,
   `apachectl configtest`, `named-checkconf`, JSON/YAML/TOML syntax; custom per
   profile. Optionally run a reload command on success.
2. **Syntax highlighting** for common config formats and shell scripts.
3. **Port forwarding**: local (`-L`), remote (`-R`) and dynamic SOCKS (`-D`),
   saved per profile and started with the connection.
4. **Folder sync / compare**: WinSCP-style "synchronise" between a local and a
   remote folder, with a preview of what would be copied or deleted.
5. **Run on many hosts**: send a command (or apply the same patch) to a group
   of profiles and collect the results side by side.
6. **Snippets**: saved commands per profile or group, one click to paste into
   the terminal.
7. **Session logging** to a local file, and terminal search.
8. **Host certificates** (`@cert-authority`) and OpenSSH user certificates.
9. **SCP fallback** for servers without the SFTP subsystem (busybox, some
   network gear).
10. Packaging: signed Windows installer and portable zip, `.deb`/`.rpm`/AppImage,
    macOS `.dmg`; release builds from CI on tags.

## Known limitations

- `sudo` editing needs `sudo` to work without a TTY (`Defaults requiretty`
  breaks it, with a clear error).
- The terminal does not answer cursor-position queries (`ESC [6n`); a few
  programs that rely on them may draw slightly wrong.
- Files over 20 MiB are not opened in the built-in editor.
- Copies made for the external editor are not yet cleaned up automatically.
