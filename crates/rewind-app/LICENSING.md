# Licensing the desktop app

The app works fully without a license. An unregistered copy says so in the
header and, after every 20 engine actions or 45 minutes of use, shows a
notice with Buy, Enter license and Not now. Nothing is ever blocked or
turned off. A license stops the reminders and shows the licensee's name.

Licenses are checked offline against an Ed25519 public key compiled into
the app (`PUBLIC_KEY` in `src/license.rs`). There is no activation server
and nothing phones home.

## The license block

The buyer receives a text block by email and pastes it into the app
(the pill at the right of the header opens the dialog), where a block
that checks out registers at once. `rewind-app --license <file>`, or
`--license -` for standard input, registers without opening a window,
for setting up machines from a script:

```
----- BEGIN REWIND VM LICENSE -----
Name: Ada Lovelace
Email: ada@example.com
Edition: Commercial
Seats: 3
Id: 0123456789abcdef
Issued: 2026-09-30
Updates-Until: 2027-09-30
Signature: <base64 Ed25519 signature>
----- END REWIND VM LICENSE -----
```

The signature covers the lines above it exactly as `Key: value\n` in the
order shown, with the values trimmed. Pasting tolerates CRLF line ends,
indentation, text around the block, the `> ` a reply quotes lines with,
no-break spaces, and lines a mail client wrapped: a line with no key goes
on the end of the value before it. The app keeps the block alone,
as it was issued, in `$XDG_CONFIG_HOME/rewind/license.txt` (or
`~/.config/rewind/license.txt`), readable by its owner only.

- Personal and Commercial licenses both include 3 years of updates
  (`UPDATE_YEARS` in `src/license.rs`). `Updates-Until` is what counts.
- A license registers every version released on or before its
  `Updates-Until`, forever. A version released after it
  (`RELEASE_DATE` in `src/license.rs`) runs as an evaluation, fully and
  with the reminders, and the header says "Updates ended · Renew"; a new
  license covers it. Raise `RELEASE_DATE` with every release.
- Ids in `REVOKED` (refunds, leaked keys) are refused. Revocation takes
  effect in the next release.

## Keys

The signing key lives in 1Password, never in this repository or on disk.
To make a new pair (only before the first sale, or after a leak):

```
nix run .#license -- keygen
```

prints the signing key once on standard error, as 64 hex digits, and the
public key on standard output as a Rust array. Put the 64 digits in a
1Password item's password field, clear the terminal, and paste the array
over `PUBLIC_KEY` in `src/license.rs`. `keygen --out <dir>` writes the key to
`<dir>/signing.key` instead (mode 0600, never overwriting one).

- Anyone with the signing key can issue licenses. If it leaks, make a new
  pair, ship a release with the new public key, and reissue licenses to
  existing customers.
- Changing the public key invalidates every license issued with the old
  one, so rotate only when you must.

`rewind-license` is built only with the `issuer` feature, as its own
package (`nix/license.nix`), so it never ships in the app's package.

## Issuing

```
nix run .#license -- issue --key - \
  --name "Ada Lovelace" --email ada@example.com \
  --edition commercial --seats 3
```

asks for the signing key without echoing it (paste it from 1Password) and
prints the block. With the 1Password CLI, pipe it instead:
`op read "op://<vault>/<item>/password" | nix run .#license -- issue --key - ...`.
`--key <file>` reads a `signing.key`. `--issued YYYY-MM-DD` backdates the
license; without it the issue date is today (UTC). An unknown flag,
`--seats 0` or a blank name or email is refused before the key is asked
for, and the name and email are trimmed. The block is read back against
`PUBLIC_KEY` before it is printed, so a signing key that does not match
the app's fails instead of printing a block every copy refuses. The id is 16 random hex
digits; record it with the order so the license can be revoked on a
refund.
