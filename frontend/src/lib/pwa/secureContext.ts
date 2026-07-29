/**
 * Whether the page can use the capabilities USB-09 and USB-10 depend on.
 *
 * Chrome gates `navigator.wakeLock`, service-worker registration and `beforeinstallprompt` behind
 * a **secure context**: HTTPS, or the `localhost` special case. A VPN does not qualify — the
 * browser judges the origin, and `http://` over a tunnel is still an insecure origin (SEC-05).
 *
 * This exists because the symptom lies. Every one of those capabilities reports itself as absent,
 * which reads as "this browser is too old" — and the operator goes to check their phone when the
 * answer is in the address bar. Worse, it all works on `http://localhost`, so a developer can
 * build and demo the whole app without ever seeing it.
 */
export function isSecure(): boolean {
  return window.isSecureContext;
}

/** The reason to show instead of a capability state, or `null` when the origin is fine. */
export function insecureOriginNote(): string | null {
  return isSecure() ? null : 'unavailable — this origin is not HTTPS';
}
