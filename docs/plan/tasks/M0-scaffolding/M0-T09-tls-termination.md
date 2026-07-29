# M0-T09 — TLS termination on the field node

**Milestone:** M0 · **Depends on:** M0-T05, M0-T06 · **Crates:** astroctl-field
**Size:** M · **Status:** done
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

**The procedure now lives in [`docs/ops/tls-setup.md`](../../../ops/tls-setup.md)** — issuance,
configuration, renewal, resolution and a symptom table, with the traps that actually cost time.
It is written to be repeatable from it alone, which is what this section was drafted towards and
what an operator needs at 2am. Kept here is only what a *task* record should keep: the decisions
and why they were taken, so a later reader does not re-litigate them.

- **Issuance: Let's Encrypt via `acme.sh --dns dns_hostinger`.** DNS for the zone is at Hostinger
  and `acme.sh` ships a native Hostinger plugin, so DNS-01 automates against the zone as it
  stands. No migration to another DNS provider, no `_acme-challenge` CNAME delegation, no
  `acme-dns` instance — all of which earlier drafts of this task proposed before the plugin was
  found. Credential is `HOSTINGER_Token`, in the environment, never in a config file (SEC-04).
- **Traps, all three found by running it rather than reading about it**, and all three now in the
  ops document: `acme.sh` defaults to **ZeroSSL**, which requires external account binding and
  fails at account registration with an error that never mentions the CA; the **distribution
  package is v3.1.1** and has no `dns_hostinger`; and after installing upstream,
  `/usr/bin/acme.sh` still **shadows** `~/.acme.sh/acme.sh` on `PATH` in non-interactive shells —
  which is exactly where the renewal cron runs.
- **Choosing a different CA buys nothing.** CA/Browser Forum ballot SC-081v3 took maximum validity
  to 200 days on 15 March 2026, 100 days in March 2027 and 47 days in March 2029. Nobody sells a
  set-and-forget certificate any more, so the choice is not "which CA" but "is renewal automated".
- **Renewal runs on the field node**, which needs only outbound internet and has it whenever the
  rig is home. The risk is a rig that sits in storage past the window — which is what SEC-07's
  expiry reporting is for.
- **A private address in a public A record is acceptable** and is not exposure. Recorded in the
  ops document so it does not later read as a leak.

### What actually exists, and the name

The certificate issued is for **`astrocli-dev.diirc.online`** (Let's Encrypt, ECDSA P-256, to
2026-10-27), not the `field.diirc.online` this task was written naming. That is the dev node,
which is what exists; the procedure is identical for either name and nothing in the design depends
on which. `diirc.online` itself was registered the day this task was written and had to propagate
before ACME could validate anything — a first-attempt failure worth expecting rather than
debugging.

**Nothing existing could be reused.** The two `diirc.lt` certificates are single-name, and the
Hostinger one is on shared hosting where the private key is not exportable. There is no wildcard
anywhere, so the "copy an existing wildcard" shortcut an earlier draft offered does not exist.

## Acceptance criteria

- [x] `https://astrocli-dev.diirc.online:8470/` serves the PWA and the API against the **system
      trust store** — `curl` without `-k` returns 200 with `ssl_verify_result=0`, TLS 1.3, ECDSA,
      ALPN `http/1.1`. (Name is the dev node's; see above.)
- [ ] `window.isSecureContext` is `true`; `navigator.wakeLock` is defined — **needs a browser**
- [x] With TLS configured, plain HTTP on the same port is refused rather than served: the port
      answers a plaintext request with a TLS alert, which curl reports as `Received HTTP/0.9 when
      not allowed`. No page is served
- [x] Config without a `tls` block still serves HTTP — verified on a copy of the dev config with
      the block removed: `scheme="http"`, 200, and `cert_*` reported as `null` rather than 0
- [x] A `tls` block naming an unreadable or malformed certificate **fails at startup** with a
      diagnostic naming the path, and leaves nothing listening. Verified for six cases: missing
      file, a readable file that is not a certificate, a readable file that is not a key, a
      truncated PEM, a certificate/key pair from different issuances, and a relative path (caught
      by the config validator before any file is opened)
- [x] `/api/system/health` reports `cert_expires_at` and `cert_days_remaining`; a short-dated
      certificate degrades `status` to `warn`. Verified live with a 5-day certificate
      (`status: "warn"`, `cert_days_remaining: 4`) and in tests with a certificate that expired in
      2020 — an expiry in the past is permanently inside any threshold, so the test cannot rot
- [ ] With the internet uplink down, the operator's phone still resolves the hostname and loads
      the app (SEC-08) — **needs a phone and the uplink down**. The name currently resolves via
      public DNS to `192.168.1.109`; the VPN-DNS half is unverified
- [x] `docs/ops/tls-setup.md` is complete enough for the procedure to be repeated from it alone

**Then close out M0-T06's four device gates**, which have been unrunnable until now: Lighthouse
installability, add-to-home-screen on Android, Screen Wake Lock held/released, and offline shell
start. Record the device model and Chrome version in the M0-T06 task file. **Still outstanding** —
the blocker is removed, the gates themselves need the device.

## What was built

- `crates/astroctl-field/src/tls.rs` — loading, expiry, and a `TlsListener` implementing
  `axum::serve::Listener`. That trait is the whole reason `main` still has **one** `axum::serve`
  call and one graceful-shutdown path: HTTP and HTTPS differ by the listener and by nothing else,
  so SDD §7's shutdown ordering — including the deliberate omission of stopping tracking — exists
  once rather than twice.
- The handshake runs in a spawned task, not inline in `accept`. Inline is the obvious
  implementation and it is wrong: a peer that opens a socket and sends nothing would hold the
  accept loop for as long as it liked. Bounded at 64 in flight so the fix is not itself an
  unbounded queue.
- `server.tls` in PRD §8.1 / the shipped example / `FieldServerConfig`, in one change set, with
  the block **commented out** so absence keeps meaning plain HTTP.
- SEC-07 in `/api/system/health`, with `warn` as a *third health value* derived at response time
  rather than a third lifecycle state — see the SDD 1.13.0 change note for why that distinction
  is load-bearing.

## Reload: deliberately not implemented

The scope note offered "reload without a restart if it is cheap". It is not, and more to the
point it buys less here than it looks. Shutdown is specified to leave tracking alone (SDD §7), so
restarting the service does not stop the mount — a renewal restart costs a browser reconnect, not
a session. `--reloadcmd 'systemctl restart astroctl-field'` is the documented path.

The one honest gap: `acme.sh`'s cron fires at a time it picks, so a renewal restart could land
mid-night. Nothing to interrupt in M0; when M1 lands the session orchestrator, `--reloadcmd`
should defer while a session is active. Recorded in the ops document rather than left implicit.

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
