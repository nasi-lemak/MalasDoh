# Security

MalasDoh holds SSH keys, passwords and logins, so security problems in it
matter. Thank you for reporting them.

## Reporting a problem

Please do **not** open a public issue for a security problem. Report it
privately instead, through GitHub:
[Report a vulnerability](https://github.com/nasi-lemak/MalasDoh/security/advisories/new)
(the repository's **Security** tab, then **Report a vulnerability**).

Useful to include:

- the MalasDoh version and your system (Windows, Linux or macOS);
- what an attacker can do, and what they need first (a malicious server, a
  network position, access to your computer…);
- steps or a proof of concept, if you have them.

You should hear back within a week. Once a fix is released, the report is
published with credit to you, unless you would rather not be named.

## Supported versions

Fixes go into the latest release only; please update before reporting.

## What counts

Examples of what is in scope:

- secrets (passwords, keys, passphrases) written to disk in the clear, put
  in logs or crash reports, or sent anywhere they should not go;
- host keys not checked, or a changed host key accepted without asking;
- a server being able to read or write files on your computer outside what
  you asked for (for example through file names in a transfer), or to run
  commands on it;
- anything in the Teleport, WireGuard, proxy or Remote Desktop code that
  weakens the protection those normally give.

Out of scope: problems that need someone who already controls your user
account on your computer, and weaknesses of the servers you connect to.

## How MalasDoh keeps secrets

Saved passwords, passphrases and pasted keys go into the system keychain
(Windows Credential Manager, the Secret Service on Linux, the macOS
Keychain), not into MalasDoh's settings files. The one exception: on a
computer without a keychain, a pasted key is kept in an owner-only file in
MalasDoh's data folder, and the SSH keys window says so. See "Where things are kept" in
the [README](README.md) for what is stored where.
