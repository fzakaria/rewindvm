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

The public key in the repository is a DEVELOPMENT key. Its signing half
was generated outside the repository and signs test licenses only.

Before the first release, generate the production pair on a machine you
trust:

```
cargo run --features issuer --bin rewind-license -- keygen --out <offline dir>
```

This writes `signing.key` (the 32 byte seed in hex, mode 0600; it refuses
to overwrite an existing key) and prints the public key as a Rust array.
Paste the array over `PUBLIC_KEY` in `src/license.rs`, then:

- Keep `signing.key` offline: a password manager entry, or an encrypted
  volume. It never goes into this repository, a CI secret store you do not
  control, or a laptop backup in the clear.
- Anyone with `signing.key` can issue licenses. If it leaks, generate a new
  pair, ship a release with the new public key, and reissue licenses to
  existing customers.
- Changing the public key invalidates every license issued with the old
  one, so rotate only when you must.

`rewind-license` is built only with `--features issuer`, so it never ships
in the app's package.

## Issuing

```
rewind-license issue --key signing.key \
  --name "Ada Lovelace" --email ada@example.com \
  --edition commercial --seats 3
```

prints the block. `--issued YYYY-MM-DD` backdates it; without it the
issue date is today (UTC). The id is 16 random hex digits; record it with
the order so the license can be revoked on a refund.

## Selling with Stripe

A small server holds `signing.key` and turns paid checkouts into
licenses:

1. The site's Buy button creates a Stripe Checkout session with the
   edition and seat count as the price and quantity.
2. Stripe calls the server's webhook with `checkout.session.completed`.
   The server checks the webhook signature with the endpoint's signing
   secret, reads the customer's name and email and the line items, and
   ignores events it has already handled (Stripe retries).
3. The server runs `rewind-license issue --key signing.key --name ...
--email ... --edition ... --seats ...` and stores the id with the
   Stripe session id.
4. It emails the block to the customer and answers the webhook with 200.
5. On `charge.refunded`, it looks up the id and adds it to a revocation
   list that goes into `REVOKED` in the next release.

The server is the one place the signing key lives online, so it should do
nothing else, accept only Stripe's webhook, and keep the key readable by
its own user only.
