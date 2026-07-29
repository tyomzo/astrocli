# TLS on the field node

**Governs:** PRD SEC-05..SEC-08, USB-09, USB-10 · ADD §4 · Implemented by M0-T09
**Applies to:** `astroctl-field` only. The field↔stack hop stays plain HTTP (SEC-09).

This is the procedure for giving a field node a certificate the operator's phone already trusts.
It was worked through end to end on 2026-07-29 against a real deployment; the traps recorded here
are the ones that actually cost time, not ones anticipated in the abstract.

---

## 1. Why this is not optional

Not confidentiality — the VPN already provides that.

Chrome gates `navigator.wakeLock`, service-worker registration and `beforeinstallprompt` behind a
**secure context**. Over plain HTTP at any address other than `localhost` all three are withheld,
so **USB-09 (installable PWA) and USB-10 (offline shell) are unreachable**. A VPN does not
substitute: the browser judges the *origin*, and a tunnelled `http://` origin is still insecure.

The trap is that every one of those APIs works on `http://localhost`, which browsers treat as
secure. The entire PWA can be built, tested and demoed without ever meeting the problem. It shows
up the first time someone opens the app on a phone, as three unrelated-looking symptoms at once —
`display mode: browser tab`, `screen wake lock: not supported by this browser`, `install prompt:
not offered by this browser`.

**Self-signed certificates are a dead end and should not be offered as a fallback.** A warning
interstitial and a secure context are mutually exclusive, and Android makes user-installed CAs
awkward on purpose. Either the device trusts the certificate or the exercise has achieved nothing.

## 2. What the node does with it

TLS terminates **inside `astroctl-field`** — no reverse proxy, no sidecar (ADD §4). The field node
is the only process that must be up for the system to work; a Pi already running the mount and the
camera does not need a second process to supervise, and a proxy that fails to start is a rig with
no UI at all.

Three consequences worth knowing before you configure it:

- **No `tls` block means plain HTTP**, and that is a supported mode, not a legacy branch.
  `localhost` development and the two-container harness (M0-T08) run on it by design.
- **A `tls` block that will not load stops the node**, naming the file. Quietly serving plaintext
  when the operator asked for TLS is the wrong direction to fail in: the symptom would be a PWA
  that silently refuses to install, with nothing in the log pointing at the cause.
- **There is no reload.** A renewal needs a restart — see §7, including why that is cheap here.

## 3. Prerequisites

- A DNS name you control, resolving to the node's VPN address (see §6 — SEC-08).
- Outbound internet on whatever host runs `acme.sh`. **No inbound exposure of the field node**
  (SEC-06): DNS-01 only, never HTTP-01 and never a port forward, or SEC-01 stops holding.
- An API token for your DNS provider, exported into the environment — never written into a config
  file (SEC-04).

## 4. Issuing the certificate

Settled choice: **Let's Encrypt via `acme.sh --dns dns_hostinger`**. DNS for `diirc.online` is at
Hostinger (`athena`/`apollo.dns-parking.com`) and `acme.sh` ships a native Hostinger plugin, so
DNS-01 automates against the zone as it stands — no migration to another DNS provider, no
`_acme-challenge` CNAME delegation, no `acme-dns` instance.

### 4.1 Install `acme.sh` from upstream, not from the distribution

```sh
curl https://get.acme.sh | sh -s email=you@example.com
```

**The packaged build is too old.** Debian/Ubuntu ship v3.1.1, which has 157 DNS plugins and
`dns_hostinger` is not among them — it landed upstream later. `--issue --dns dns_hostinger`
against it fails with an unknown-DNS-API error that says nothing about the version being the
cause. The packaged build is also broken in its own right: `--log` dies with
`shift: can't shift that many` under `dash`.

Do **not** work around this by dropping the single plugin file into `/usr/share/acme.sh/dnsapi/`.
A package upgrade would silently remove it, and a current plugin does not necessarily match a
v3.1.1 core.

**After installing, invoke it by path.** `/usr/bin/acme.sh` stays on `PATH` and shadows
`~/.acme.sh/acme.sh` in any non-interactive shell — which is precisely where the renewal cron
runs. Every command below is written with the full path for that reason.

### 4.2 Set the CA explicitly, once

```sh
~/.acme.sh/acme.sh --set-default-ca --server letsencrypt
```

**`acme.sh` defaults to ZeroSSL, not Let's Encrypt.** A bare `--issue` picks
`https://acme.zerossl.com/v2/DV90`, which sets `externalAccountRequired`, so it stops at *"Please
update your account with an email address first"* — before it ever looks at DNS, and with an error
that never mentions the CA being the surprise. Let's Encrypt needs no EAB and is what every other
certificate in this deployment already uses.

### 4.3 Issue

```sh
export HOSTINGER_Token='...'        # SEC-04: environment, never a config file
~/.acme.sh/acme.sh --issue --dns dns_hostinger -d astrocli-dev.diirc.online --keylength ec-256
```

`--keylength ec-256` asks for ECDSA. It is the default and it is what you want on a Pi, but it is
worth stating because it determines the key encoding — see §5.

**Expect the first attempt to fail if the zone is new.** Delegation has to propagate before the CA
can validate anything. `diirc.online` was registered the same day this was first attempted and was
not yet resolving publicly; that is a wait, not a bug to debug.

The result lands in `~/.acme.sh/<name>_ecc/`:

| File | Use |
|------|-----|
| `fullchain.cer` | `server.tls.cert_path` — leaf **and** intermediate |
| `<name>.key` | `server.tls.key_path` |
| `<name>.cer` | leaf only — **do not use this one** |

Use `fullchain.cer`. A leaf on its own validates on a desktop that has already cached the
intermediate and fails on the phone that has not, which is the harder of the two failures to
reproduce and the only one that matters here.

### 4.4 Where renewal runs

On the field node itself, which needs only *outbound* internet and has it whenever the rig is
home. Fewest moving parts, and the default.

The risk is a rig that sits in storage past the renewal window — which is exactly what SEC-07's
expiry reporting is for (§8). If the 2027 move to 100-day certificates makes that uncomfortable,
issue centrally on the always-on host and push to the field node over the VPN; that is a change of
one script, not of this design.

**Choosing a different CA buys nothing.** Every publicly trusted CA is bound by CA/Browser Forum
ballot SC-081v3: maximum validity fell to **200 days on 15 March 2026**, drops to **100 days in
March 2027** and **47 days in March 2029**. Nobody sells a set-and-forget certificate any more, so
the question is not "which CA" but "is renewal automated".

## 5. Configuring the node

```yaml
server:
  host: 0.0.0.0
  port: 8470
  # …
  tls:
    cert_path: /home/astro/.acme.sh/astrocli-dev.diirc.online_ecc/fullchain.cer
    key_path: /home/astro/.acme.sh/astrocli-dev.diirc.online_ecc/astrocli-dev.diirc.online.key
    warn_days_before_expiry: 14
```

- Both paths must be **absolute**. A relative path resolves against whatever directory systemd
  started the unit in, which is the class of bug that only appears in production. The config
  validator rejects them before any file is opened. `~` is expanded, as everywhere else.
- `warn_days_before_expiry` is 1..60. The upper bound is deliberate: `acme.sh` renews at 60 days
  remaining by default, so a threshold at or above that would be latched on from the moment the
  renewal window opens, and a warning that is always lit carries no information.
- The key may be PKCS#8, PKCS#1 or **SEC1**. This matters more than it sounds: an ECDSA key from
  `acme.sh` is `-----BEGIN EC PRIVATE KEY-----`, which is SEC1. A loader written and tested
  against a generated RSA key passes its own tests and then fails on the only certificate the
  deployment actually has.
- The process must be able to *read* both files. `acme.sh` writes the key `0600`; if the node runs
  as a different user, use `--reloadcmd` to install copies with the right ownership rather than
  loosening the mode on `~/.acme.sh`.

Start it and check the log:

```
INFO astroctl_field: API listening addr=0.0.0.0:8470 auth_enforced=true scheme="https"
                     cert_expires_at=2026-10-27T16:28:54.000Z cert_days_remaining=89
```

`scheme="https"` is the line to look for. If it says `http`, the block is not being read.

Verify from another machine, **without `-k`** — the point is that the system trust store accepts
it, and `-k` is exactly the flag that hides the failure this whole document exists to prevent:

```sh
curl -sS -o /dev/null -w '%{http_code} verify=%{ssl_verify_result}\n' \
  https://astrocli-dev.diirc.online:8470/
# 200 verify=0
```

## 6. Resolution without internet (SEC-08)

**The name must resolve through the VPN's own DNS, not only the public zone.** If resolution
depends on reaching public DNS, the UI becomes unreachable precisely when the field node is
operating standalone with no uplink — which is ARC-06's whole premise, and the criterion most
likely to be skipped because it passes at home.

Verify deliberately: with the uplink down, the phone must still resolve the hostname and load the
app.

A private address in a public A record is acceptable and is **not** exposure — `192.168.1.109` in
public DNS tells an attacker nothing they could act on and nothing they did not already assume.
Noted here so it does not later read as a leak.

## 7. Renewal, and why there is no hot reload

`acme.sh` installs a cron at install time and renews at 60 days remaining. The node reads its
certificate **once, at startup**, so a renewal takes effect on the next restart:

```sh
~/.acme.sh/acme.sh --install-cert -d astrocli-dev.diirc.online --ecc \
  --reloadcmd 'systemctl restart astroctl-field'
```

Hot reload was considered and deliberately left out. The argument for it is that a running session
must not be interrupted; the argument against is that **on this node it isn't**. Shutdown is
specified to leave tracking alone (SDD §7) — restarting the service does not stop the mount, which
is the asymmetry the shutdown path exists to encode. A restart therefore costs a browser reconnect,
not a session. A documented restart is honest; a half-working reload is not.

**Known gap, stated rather than hidden:** the cron fires at a time `acme.sh` picks, so a renewal
restart could land in the middle of an imaging night. In M0 there is nothing to interrupt. When
M1 lands the session orchestrator, `--reloadcmd` should become a script that defers the restart
while a session is active. Until then, the honest mitigation is that renewal happens roughly four
times a year and the window is minutes.

## 8. Expiry reporting (SEC-07)

`/api/system/health` carries the certificate's expiry, because an expired certificate revokes the
secure context exactly as a missing one does — it disables the wake lock and the installed app,
and the operator must not discover that in a field.

```jsonc
{
  "status": "ok",                                 // "warn" inside the threshold
  "cert_expires_at": "2026-10-27T16:28:54.000Z",  // null on a plain-HTTP node
  "cert_days_remaining": 89                       // negative once it has passed
}
```

- `status` degrades to `warn` when `cert_days_remaining < warn_days_before_expiry`. It stays
  `warn` after expiry: an expired certificate is the same warning, still on, not a new state.
- `null` on a plain-HTTP node is a supported deployment, not a fault. A dashboard must not read a
  missing certificate as "0 days remaining".
- The same warning is logged once at startup, because a certificate already inside the window at
  boot is the case where nobody is watching the health endpoint yet.

## 9. When something goes wrong

| Symptom | Cause |
|---------|-------|
| `holds no certificate; expected a PEM file with a -----BEGIN block` | The path points at a real, readable file that is not a certificate — a README, a CSR, a truncated copy |
| `is not a valid PEM certificate: section end "CERTIFICATE" missing` | Truncated file; the copy or the download did not finish |
| `cannot be served together: keys may not be consistent: KeyMismatch` | Certificate and key are from different issuances. Most often `<name>.cer` paired with a regenerated key, or a stale copy |
| `cannot read certificate file …: Permission denied` | The node's user cannot read `~/.acme.sh`. Install copies via `--reloadcmd`; do not chmod the acme.sh store |
| `server.tls.cert_path: … is not an absolute path` | The config validator, before any file is opened |
| Browser: `ERR_SSL_PROTOCOL_ERROR` on `http://…:8470` | Correct behaviour. The port speaks TLS; a plain HTTP request gets a TLS alert, not a page |
| Node starts with `scheme="http"` when TLS was configured | The `tls:` block is not where you think — check its indentation under `server:` |
| Phone shows a certificate warning; desktop does not | `<name>.cer` is configured instead of `fullchain.cer`. The desktop has the intermediate cached; the phone does not |
| PWA still will not install over a valid HTTPS origin | Not a TLS problem any more. Check the manifest and service worker (M0-T06) |

## 10. What this deployment actually has

Checked rather than assumed, 2026-07-29:

| Name | Certificate | Notes |
|------|-------------|-------|
| `astrocli-dev.diirc.online` | Let's Encrypt, ECDSA P-256, to 2026-10-27 | The dev field node. Issued by the procedure above; resolves to `192.168.1.109` |
| `diirc.online` | none | Registered 2026-07-29 at Hostinger; NS `athena`/`apollo.dns-parking.com` |
| `diirc.lt` | Let's Encrypt, SAN `diirc.lt`, `www.diirc.lt` | Hostinger shared hosting; key not exportable |
| `through.diirc.lt` | Let's Encrypt, SAN `through.diirc.lt` only | Azure host, own ACME client — the VPN endpoint |

**Nothing existing could be reused.** The two `diirc.lt` certificates are single-name and one of
them is on shared hosting where the private key is not exportable. There is no wildcard anywhere.
Each field node needs its own certificate; the "copy an existing wildcard" shortcut does not exist
in this deployment.

Note the name in the table is `astrocli-dev.diirc.online`, while M0-T09 was written naming the
production node `field.diirc.online`. Both are correct: the dev node is what exists today, and the
procedure is identical for either name.
