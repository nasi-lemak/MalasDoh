# Testing on Windows

About 15 minutes. You need one Linux server you can SSH into (a VM or a
cheap VPS is fine), ideally with a sudo user. Tick each box; for anything
that fails, note the step number, what you saw, and attach a screenshot
and `crash.log` if there is one.

**Where things are**

| | Normal (`malasdoh.exe` from the plain zip) | Portable zip |
|---|---|---|
| Settings, connections | `%APPDATA%\MalasDoh\config` | `data\` next to the exe |
| Backups, journal, drafts, `crash.log` | `%APPDATA%\MalasDoh\data` | `data\` next to the exe |
| Saved passwords | Credential Manager → Windows Credentials, names ending in `.MalasDoh` | same |

Paste `%APPDATA%\MalasDoh` into the Explorer address bar to open it.

## 1. Start

- [ ] 1.1 Unzip and double-click `malasdoh.exe`. The build is not signed, so
  SmartScreen says "Windows protected your PC": click **More info → Run
  anyway**. (Defender may also scan it the first time.)
- [ ] 1.2 The window opens with the MalasDoh icon in the title bar and
  taskbar. Text is sharp; at 125%/150% display scaling nothing is cut off.
- [ ] 1.3 If it does not start, a message box should say why (most often
  OpenGL: update the graphics driver). Note what it says.

## 2. Connections

- [ ] 2.1 If you use PuTTY or WinSCP: **Connection → Import from PuTTY**
  (or **Import from WinSCP**). Your saved sessions appear, with the right host, port and user.
- [ ] 2.2 **+** (new connection) to your server with a **password**; tick
  "Remember in the system keychain". It connects; the terminal shows a prompt.
- [ ] 2.3 Open Credential Manager: there is an entry ending in `.MalasDoh`.
  Quit MalasDoh, start it again, connect: no password is asked.
- [ ] 2.4 Key login, whichever you use: a key file in `C:\Users\you\.ssh`
  (OpenSSH or `.ppk`), **Pageant** running with your key, or the Windows
  OpenSSH agent service. Connects without a password.
- [ ] 2.4b A key copied from a password manager (e.g. Proton Pass): **Tools →
  SSH keys → Paste a key…**, give it a name, paste the private key → **Keep
  it** (if it has a passphrase, it asks for it once). It says the key is in
  the system keychain; Credential Manager has an `ssh-key:` entry. **Copy
  public key** and put it on the server (or **Put on a server**). Edit the
  connection: **Authentication → Stored key** → your key. It connects with
  no question, and no key file exists in `C:\Users\you\.ssh` for it.
- [ ] 2.5 A host you already trust in `C:\Users\you\.ssh\known_hosts` (from
  Windows `ssh`) connects without the "New server" fingerprint question.

## 3. Terminal

- [ ] 3.1 Typing, Backspace, arrows, Tab completion, Ctrl+C work. `htop` or
  `vim` draws correctly; resizing the window resizes them.
- [ ] 3.2 Select text and right-click: it is copied (paste into Notepad).
  Right-click with nothing selected pastes. Pasting several lines asks first.
- [ ] 3.3 Non-English text: `echo héllo 日本` shows correctly.

## 4. Files

- [ ] 4.1 Browse to a folder; **Download** a file: the save dialog opens and
  the file lands where you chose (try the Desktop).
- [ ] 4.2 Drag a file from Explorer onto the server's file list: it uploads.
  Try a name with spaces and one with non-English letters.
- [ ] 4.3 **Local files**: your computer's folders appear next to the
  server's; its **Drives** menu lists `C:\`, `D:\`…; drag files both ways.
- [ ] 4.3b Click a file in the server's list, then **Ctrl+A**: everything is
  selected (the terminal gets nothing). **Shift+click** another file: the
  range between. **↓**/**Shift+↓** move and extend; **F2** renames;
  **Delete** asks first; **Enter** on a folder opens it; **Backspace** goes
  back up; **Esc** clears. The same keys work in **Local files**.
- [ ] 4.3c In **Local files**, right-click a file: **Rename…** and
  **Delete…** are there. **F2**, type a new name, **Enter**: renamed. Rename
  it to the name of another file in the folder: refused, nothing is
  replaced. Select two files, **Delete**, **Enter**: both go to the
  **Recycle Bin** (restore them from there). Delete a folder with files in
  it and choose **Delete for good**: gone, not in the Recycle Bin.
- [ ] 4.3d Dialogs answer the keyboard: **Enter** for the main button,
  **Esc** to cancel. Try the password prompt (Enter logs in), Rename and New
  folder on the server (Enter), the delete question (Enter deletes, Esc
  keeps), and in **Port forwards** fill in a new forward and press Enter.
  With the forwards window open, typing a command and Enter in the
  terminal behind it runs the command and adds no forward.
- [ ] 4.3e In a file list, type the first letters of a file's name: it
  jumps there. **Ctrl+C** copies the selected paths (paste them in
  Notepad). **Ctrl+Shift+N**: new folder. **F5** refreshes. Open a folder,
  then **Alt+←** goes back and **Alt+→** forward. **Ctrl+L**: the path box,
  selected, ready to type. **Shift+F10**: the right-click menu, at the row;
  **Esc** closes it.
- [ ] 4.4 Double-click a text file, change a line: the gutter shows a blue
  mark; hover it to see the old line. **Save**: the diff appears, then it
  saves. A Windows-style (CRLF) file stays CRLF.
- [ ] 4.4b In an empty `.json` file type `{`, Enter, `"a": 1`, Enter, `}`: the
  `}` goes back to the left edge, under the `{`. Type `x = {}`, put the
  cursor between the braces and press Enter: the braces open onto three
  lines with the cursor indented in the middle.
- [ ] 4.5 **Open in external editor** (right-click a file): it opens in Notepad
  (or the editor set in Settings, e.g. VS Code); saving there uploads after
  the review.
- [ ] 4.6 Drag a folder with many files (and one big file, say 200 MB) to
  the server. The **Transfers** row shows files done, speed and time left.
  Press ⏸ while the big file is copying, then ▶: it continues rather than
  starting over. At the end it says the checksums match.
- [ ] 4.7 Start a big download, then pull the network cable (or turn off
  Wi-Fi) for a moment. The row says it is waiting; **Reconnect** and it
  carries on. A cancelled download leaves no `.malasdoh-part` file behind.
- [ ] 4.8 **Settings → Transfers**: put `node_modules, *.tmp` in the skip
  list and upload a folder containing those: they are left out, and the
  row's **Details** lists them.
- [ ] 4.9 Upload a folder of config files over one already on the server, with
  one file changed and one identical. **Replace files?** lists both: the
  identical one says "same content", the changed one has **Show changes**
  (lines removed in red, added in green). Untick another file: it stays as
  it was on the server.
- [ ] 4.10 After 4.9, right-click the replaced file → **Backups…**: the version
  from before the upload is there. **Tools → Change journal** has the upload
  with what changed.
- [ ] 4.11 Edit a file in Notepad so it has Windows line endings, then upload
  it over the server's copy: **Show changes** says the line endings change
  (the text itself looks the same).

## 5. Root and many servers

- [ ] 5.1 Right-click a root-owned file (e.g. `/etc/hosts`) → **Edit as root
  (sudo)**. Enter the sudo password with "Remember" ticked; it opens.
  Credential Manager now also has a `sudo-password:` entry.
- [ ] 5.2 Disconnect, reconnect, click **root** in the file browser: no
  password asked, the red badge shows.
- [ ] 5.3 If you have two servers: **Tools → Edit a file on many servers**,
  load a harmless file, change a line, **Preview**, **Save**.

## 6. Teleport (if you use it)

- [ ] 6.1 **Connection → Teleport…** → **+ Add a cluster**: the web address
  you use in the browser, and your user name. **Check** shows the cluster's
  name and version.
- [ ] 6.2 Password and authenticator code → **Log in**: it says how long the
  login lasts and which server logins you have.
- [ ] 6.3 The servers you can use are listed straight after logging in (no
  second code) → tick some → **Add**.
  They appear under "Teleport · <cluster>" in the list; double-click one:
  terminal and files work, with no fingerprint question.
- [ ] 6.4 **Log out**, then connect again: the login form opens; after
  logging in, the connection carries on by itself.
- [ ] 6.5 If the cluster has Windows desktops: they are listed under
  **Windows desktops** after logging in. Pick the Windows user → **Add**.
  At least one Linux server of the cluster that can reach the desktop must
  be in your list too (6.3).
- [ ] 6.6 Double-click the desktop (marked "· RDP"): the **Remote Desktop**
  window says which server it goes through, and Windows' own Remote Desktop
  opens and asks for your Windows password. Moving the mouse and scrolling
  should feel like normal Remote Desktop, not like the browser.
- [ ] 6.7 Close the Remote Desktop window: MalasDoh's window says it is
  closed. Double-click again: it goes straight through the same server.

## 7. WireGuard (if you use it)

- [ ] 7.1 **Connection → WireGuard…** → **Open a .conf file…** (the one you
  would give the WireGuard app) → **Import**. It should say the private key
  is in the system keychain (Credential Manager), not in MalasDoh's files.
- [ ] 7.2 Edit a connection to a server behind WireGuard: **Proxy →
  WireGuard**, pick the configuration, save, connect. No admin prompt, and
  the WireGuard app does not need to be running (or installed).
- [ ] 7.3 Back in the WireGuard window it says **Running**, with bytes sent
  and received. Close the connection: after a moment it says not running.
- [ ] 7.4 If your configuration has `DNS =`, a connection to an internal
  name (not an IP) works.
- [ ] 7.5 Right after the import in 7.1, the window tests the configuration
  by itself: **OK** for the server address, the handshake and (with `DNS =`)
  traffic through the tunnel. Type a server behind it, e.g. `10.0.0.5:22`,
  next to **Test** and press it: that line says reachable too.
- [ ] 7.6 Paste your configuration again with the `Endpoint` port changed to
  a wrong one and import it as "broken": its test says **Failed** at
  Handshake, with a reason. Remove it afterwards.
- [ ] 7.7 Right-click a group in the sidebar → **Use WireGuard for all** →
  your configuration: the message says how many connections changed; edit
  one to see its Proxy. **Stop using WireGuard for all** puts them back to
  direct.
- [ ] 7.8 **Edit…** on your configuration: the PrivateKey line says
  "(saved, not shown)", never the key. Add a typo to an AllowedIPs line and
  press **Save**: it says "Not saved" with the line number. Fix it, Save: it
  tests again, and connections using it still connect.

## 8. Keyboard

- [ ] 8.0a **F1** shows the list of keys; **Esc** closes it.
- [ ] 8.0b **Ctrl+K**, type part of a connection's name, **Enter**: it
  connects (once). **Ctrl+K**, type, **↓**: the list has the keyboard;
  **↓**/**↑**, **F2** (edit, **Esc** closes), **Delete** (asks; **Esc**
  keeps it), **Enter** connects.
- [ ] 8.0c With two connections open, **Ctrl+Tab** switches between them,
  and you can type in the terminal straight away. **Ctrl+Shift+T** opens a
  terminal, **Ctrl+PgUp**/**Ctrl+PgDn** move between tabs, **Ctrl+Shift+W**
  closes one. In the terminal, **Ctrl+K** and **Ctrl+N** still do what bash
  does with them.
- [ ] 8.0d Upload a big file; click its row in Transfers: **Space** pauses,
  **Space** again continues, **Delete** cancels.
- [ ] 8.0e **Tools → Run on many hosts**: click a host, **↓**, **Space**
  ticks the next one, **Ctrl+A** ticks all (again: none).

## 9. Safety nets

- [ ] 9.1 Edit a file without saving, then end MalasDoh in **Task Manager**.
  Start it, connect again: it offers the unsaved edits back.
- [ ] 9.2 **Port forward**: add a local forward (e.g. 8080 → localhost:80 on
  the server) and open `http://localhost:8080` in your browser. Windows
  Firewall should not ask (it only listens on 127.0.0.1).
- [ ] 9.3 Quit with a session open; start again: "Reopen last session" works.
- [ ] 9.4 **Connection → Export connections…** to a file with a passphrase,
  then **Import connections…** it back: nothing is duplicated.

## Reporting

For each failed box: the number, what you expected, what happened, a
screenshot. If MalasDoh closed by itself, attach `crash.log` from the data
folder above.
