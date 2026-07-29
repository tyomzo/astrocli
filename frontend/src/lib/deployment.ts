import { useEffect, useState } from 'react';

/**
 * Which deployment this app is talking to — production, or a labelled non-production node.
 *
 * # Why this reads the manifest rather than an API
 *
 * The marker's whole job is to stop a dev app being mistaken for the production one while it drives
 * a real mount, so it has to render before a token is entered and whether or not either node is
 * reachable. That rules out every authenticated endpoint.
 *
 * The manifest is the right source rather than merely an available one: it is what names the app on
 * the home screen, so sourcing the in-app marker from the same document makes the two incapable of
 * disagreeing. A separate field could drift from the installed name and would be believed anyway.
 *
 * `null` means production — no marker, nothing to say.
 */
export function useDeploymentLabel(): string | null {
  const [label, setLabel] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    // Same origin and unauthenticated by construction: the manifest has to be readable by the
    // browser's install machinery, which sends no credentials.
    fetch('/manifest.webmanifest', { cache: 'no-store' })
      .then((response) => (response.ok ? response.json() : null))
      .then((manifest: { name?: unknown } | null) => {
        if (!live) return;
        setLabel(labelFrom(manifest?.name));
      })
      .catch(() => {
        // A failure here means production styling on a dev node, which is the wrong direction to
        // fail in — but the alternative, marking production as dev, would teach the operator to
        // ignore the marker. Silence is the safer error, and the home-screen name still differs.
      });
    return () => {
      live = false;
    };
  }, []);

  return label;
}

/** `"AstroCtl Dev"` → `"Dev"`; `"AstroCtl"` and anything unexpected → `null`. */
export function labelFrom(name: unknown): string | null {
  if (typeof name !== 'string') return null;
  const suffix = name.replace(/^AstroCtl\s*/, '').trim();
  return suffix === '' ? null : suffix;
}
