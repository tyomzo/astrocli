# M0-T09 — TLS termination on the field node

**Milestone:** M0 · **Depends on:** M0-T05, M0-T06 · **Crates:** astroctl-field
**Size:** M · **Status:** not started
**Spec:** PRD SEC-05/06/07/08, USB-09, USB-10; ADD §4 (context diagram note); SDD §5.9 (target platform)

## Objective

Serve the operator-facing API and PWA over HTTPS, with a certificate the operator's device already
trusts, so the browser grants a **secure context**.

This is not hardening. Chrome gates `navigator.wakeLock`, service-worker registration and
`beforeinstallprompt` behind a secure context, so **USB-09 and USB-10 cannot be satisfied without
it** — and neither can the four device gates M0-T06 left outstanding. A VPN does not substitute:
the browser judges the *origin*, and `http://` over a tunnel is an insecure origin.

## How this was found, and why it is worth stating

M0-T06 was reported complete with the shell serving correctly. Opening it on a real Android phone
over the LAN produced three symptoms at once — `display mode: browser tab`, `screen wake lock: not
supported by this browser`, `install prompt: not offered by this browser` — which read as three
separate defects and were one cause.

The trap is that **every one of these works on `http://localhost`**, which browsers treat as
secure. A developer can build, test and demo the entire PWA without ever encountering the problem.
That is what makes it worth a task rather than a footnote.

## Scope

- **`rustls` termination in `astroctl-field`.** `axum` with `tokio-rustls`/`axum-server`; no
  reverse proxy, no sidecar. ADD §4 records why: the field node is the only process that must be up
  for the system to work, so it is where the certificate belongs. A Pi already running the mount
  and camera does not need a second process to supervise.
- **Config** — a `server.tls` block with `cert_path`, `key_path` and `warn_days_before_expiry`.
  **Absent means plain HTTP**, which keeps `localhost` development and the M0-T08 container harness
  working unchanged. Present and unreadable is a startup failure, not a silent downgrade to HTTP:
  quietly serving plaintext when the operator asked for TLS is the wrong direction to fail in.

  **Three artefacts move together, in one commit**: PRD §8.1's schema block, `config/field-node.example.yaml`,
  and the `FieldConfig` struct. `astroctl-core` sets `deny_unknown_fields` at every level, and
  `config::tests::shipped_field_example_is_the_prd_block_verbatim` asserts the first two are
  identical line for line. Adding the schema ahead of the struct ships an example the binary refuses
  to parse — this was tried while writing this task and the drift test caught it immediately, which
  is the intended behaviour, not an obstacle. Its failure message is worth reading before you start.
- **Certificate expiry in `/api/system/health`** (SEC-07) — parse `notAfter` at load, report
  `cert_expires_at` and `cert_days_remaining`, and degrade `status` to `warn` under a configurable
  threshold (default 14 days). An expired certificate revokes the secure context exactly as a
  missing one does; the operator must not discover that in a field.
- **Reload without a restart** if it is cheap — a running session must not be interrupted by a
  renewal. If it is not cheap, say so and leave it out; a documented restart is honest, a
  half-working reload is not.
- **`docs/ops/tls-setup.md`** — the operator-facing procedure, worked through for this deployment.

## The concrete deployment

Domain `diirc.lt`; the VPN is reachable as `through.diirc.lt`. What is actually deployed today,
checked rather than assumed (2026-07-29):

| Name | Cert | Issuer | Where it lives |
|------|------|--------|----------------|
| `diirc.lt` | SAN `diirc.lt`, `www.diirc.lt` | Let's Encrypt | Hostinger shared hosting (`2.57.91.91`, `server: hcdn`), auto-provisioned and auto-renewed |
| `through.diirc.lt` | SAN `through.diirc.lt` only | Let's Encrypt | Azure host (`4.223.171.38`), own ACME client |

**Neither certificate is reusable and there is no wildcard.** Both are single-name Let's Encrypt
certificates covering the names they serve; neither covers `field.diirc.lt`. The Hostinger one is
additionally out of reach — on shared hosting the private key is not normally exportable. So
`field.diirc.lt` needs its own certificate; the "copy an existing wildcard" shortcut does not exist
here. (An earlier draft of this task offered it, before the certificates were checked.)

- **Name.** `field.diirc.lt`, resolving to the field node's VPN address. Does not resolve today.
- **Issuance — settled: Let's Encrypt via `acme.sh --dns dns_hostinger`.** DNS for the zone is at
  Hostinger (`ns1/ns2.dns-parking.com`), and `acme.sh` ships a **native Hostinger DNS plugin**, so
  DNS-01 automates against the zone as it stands. No migration to another DNS provider, no
  `_acme-challenge` CNAME delegation, no `acme-dns` instance — all of which an earlier draft of this
  task proposed before the plugin was found.

  Verify the credential variable against the installed version before scripting it: the project
  wiki documents `HOSTINGER_COM_Username` / `HOSTINGER_COM_Password`, while more recent material
  describes a `HOSTINGER_Token` against the official API at `developers.hostinger.com`. Both refer
  to the same `dns_hostinger` plugin; the plugin appears to have moved from credentials to an API
  token. Prefer the token if the installed version supports it.

  **Choosing a different CA buys nothing.** Every publicly trusted CA is bound by CA/Browser Forum
  ballot SC-081v3: maximum validity fell to **200 days on 15 March 2026**, drops to **100 days in
  March 2027** and **47 days in March 2029**. Nobody sells a set-and-forget certificate any more, so
  the choice is not "which CA" but "is renewal automated" — and once it is automated, Let's Encrypt
  is free and already understood by every client.

- **Where renewal runs.** The field node needs only *outbound* internet to renew, which it has
  whenever the rig is home. That is the fewest moving parts and the default for this task. The risk
  is a rig that sits in storage past the renewal window — which is exactly what SEC-07's expiry
  warning is for. If the 2027 move to 100-day certificates makes that uncomfortable, issue centrally
  on the always-on Azure host (it already runs ACME) and push to the field node over the VPN; that
  is a change of one script, not of this design.

- **Resolution (SEC-08).** The name must resolve **through the VPN's DNS, not only the public
  zone**. If resolution depends on reaching public DNS, the UI becomes unreachable precisely when
  the field node is operating standalone with no internet — which is ARC-06's whole premise. Verify
  deliberately: with the uplink down, the phone must still resolve `field.diirc.lt`.
- A private address in a public A record is acceptable and is not exposure. Note it in the setup
  document so it does not later read as a leak.
- **Renewal is every 90 days**, and the industry ceiling is falling (200 days today, 100 in March
  2027, 47 in March 2029). SEC-07's expiry reporting is doing real work rather than guarding a
  hypothetical, and it will matter more each year.

## Acceptance criteria

- [ ] `https://field.diirc.lt:8470/` serves the PWA with no certificate warning on Android Chrome
- [ ] `window.isSecureContext` is `true`; `navigator.wakeLock` is defined
- [ ] With TLS configured, plain HTTP on the same port is refused rather than served
- [ ] Config without a `tls` block still serves HTTP — `localhost` development and M0-T08 unchanged
- [ ] A `tls` block naming an unreadable or malformed certificate **fails at startup** with a
      diagnostic naming the path; it does not fall back to HTTP
- [ ] `/api/system/health` reports `cert_expires_at` and `cert_days_remaining`; a certificate
      inside the warning threshold degrades `status` to `warn` (test with a short-dated cert)
- [ ] With the internet uplink down, the operator's phone still resolves the hostname and loads
      the app (SEC-08 — the criterion most likely to be skipped, and the one that fails in a field)
- [ ] `docs/ops/tls-setup.md` is complete enough for the procedure to be repeated from it alone

**Then close out M0-T06's four device gates**, which have been unrunnable until now: Lighthouse
installability, add-to-home-screen on Android, Screen Wake Lock held/released, and offline shell
start. Record the device model and Chrome version in the M0-T06 task file.

## Notes for whoever picks this up

`rustls` needs a crypto provider installed explicitly in recent versions — install it once at
startup rather than relying on a default feature, or the first handshake panics at runtime instead
of failing at build time.

Keep the HTTP path a real supported mode, not a legacy branch. The container harness (M0-T08) runs
plain HTTP by design and there is no reason to give two containers on a bridge network a
certificate.

Self-signed certificates are a dead end here and should not be offered as a fallback: Android makes
user-installed CAs awkward, and a warning interstitial and a secure context are mutually exclusive.
Either the device trusts the certificate or the task has not achieved its objective.
