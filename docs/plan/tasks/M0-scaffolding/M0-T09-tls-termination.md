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

Domain `diirc.lt`; the VPN is reachable as `through.diirc.lt`.

- **Name.** `field.diirc.lt`, resolving to the field node's VPN address.
- **Issuance.** DNS-01 against `diirc.lt` (SEC-06) — it proves control with a TXT record and needs
  no inbound path to the Pi, so SEC-01's "no public exposure or port forwarding" holds. HTTP-01 is
  ruled out for exactly that reason. An existing wildcard for `*.diirc.lt` copied to the node is
  equally valid and renews annually rather than every 90 days.
- **Resolution (SEC-08).** The name must resolve **through the VPN's DNS, not only the public
  zone**. If resolution depends on reaching public DNS, the UI becomes unreachable precisely when
  the field node is operating standalone with no internet — which is ARC-06's whole premise. Verify
  this deliberately: with the uplink down, the phone must still resolve `field.diirc.lt`.
- A private address in a public A record is acceptable and does not constitute exposure. Note it in
  the setup document so it does not later read as a leak.

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
