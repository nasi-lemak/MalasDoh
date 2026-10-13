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
- [ ] 4.4 Double-click a text file, change a line: the gutter shows a blue
  mark; hover it to see the old line. **Save**: the diff appears, then it
  saves. A Windows-style (CRLF) file stays CRLF.
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
- [ ] 6.3 Wait for the next code, then **Find servers** → tick some → **Add**.
  They appear under "Teleport · <cluster>" in the list; double-click one:
  terminal and files work, with no fingerprint question.
- [ ] 6.4 **Log out**, then connect again: the login form opens; after
  logging in, the connection carries on by itself.

## 7. Safety nets

- [ ] 7.1 Edit a file without saving, then end MalasDoh in **Task Manager**.
  Start it, connect again: it offers the unsaved edits back.
- [ ] 7.2 **Port forward**: add a local forward (e.g. 8080 → localhost:80 on
  the server) and open `http://localhost:8080` in your browser. Windows
  Firewall should not ask (it only listens on 127.0.0.1).
- [ ] 7.3 Quit with a session open; start again: "Reopen last session" works.
- [ ] 7.4 **Connection → Export connections…** to a file with a passphrase,
  then **Import connections…** it back: nothing is duplicated.

## Reporting

For each failed box: the number, what you expected, what happened, a
screenshot. If MalasDoh closed by itself, attach `crash.log` from the data
folder above.
