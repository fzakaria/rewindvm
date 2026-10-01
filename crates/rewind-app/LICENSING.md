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
(the "Unregistered" pill in the header opens the dialog):

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
indentation and text around the block. The app keeps the block in
`$XDG_CONFIG_HOME/rewind/license.txt` (or `~/.config/rewind/license.txt`).

- Personal licenses include 3 years of updates, Commercial 1 year.
  `Updates-Until` is what counts.
- A license whose `Updates-Until` is before the app's release date
  (`RELEASE_DATE` in `src/license.rs`) still registers that version; the
  header says "License covers versions until <date>". Raise
  `RELEASE_DATE` with every release.
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
license; without it the issue date is today (UTC). The id is 16 random hex
digits; record it with the order so the license can be revoked on a
refund.

## Selling with Stripe

A small server holds the signing key, as a secret it reads at start, and
turns paid checkouts into licenses:

1. The site's Buy button creates a Stripe Checkout session with the
   edition and seat count as the price and quantity.
2. Stripe calls the server's webhook with `checkout.session.completed`.
   The server checks the webhook signature with the endpoint's signing
   secret, reads the customer's name and email and the line items, and
   ignores events it has already handled (Stripe retries).
3. The server runs `rewind-license issue --key - --name ... --email ...
--edition ... --seats ...` with the key on standard input, and stores
   the id with the Stripe session id.
4. It emails the block to the customer and answers the webhook with 200.
5. On `charge.refunded`, it looks up the id and adds it to a revocation
   list that goes into `REVOKED` in the next release.

The server is the one place the signing key lives online, so it should do
nothing else, accept only Stripe's webhook, and keep the key readable by
its own user only.
