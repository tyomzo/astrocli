import type { ReactNode } from 'react';

import type { Destination } from '../store/ui';
import { useUiStore } from '../store/ui';

/**
 * The three phone destinations — SDD §5.9's `◎target ▣frame ⛁stack`.
 *
 * They are session concerns, not subsystems: what to point at, acquiring, and the result. **On a
 * tablet this bar disappears entirely** (`md:hidden`) because all three regions are on screen at
 * once — a navigation control whose destinations are already visible teaches the operator that
 * navigation controls do nothing.
 *
 * The glyphs are from the sketch and are not decorative: they differ in fill and outline, so the
 * selected destination is legible without relying on the accent colour, which night mode collapses
 * toward the same red as everything else.
 */
const DESTINATIONS: readonly { id: Destination; glyph: string; label: string }[] = [
  { id: 'target', glyph: '◎', label: 'target' },
  { id: 'frame', glyph: '▣', label: 'frame' },
  { id: 'stack', glyph: '⛁', label: 'stack' },
];

export function BottomNav(): ReactNode {
  const destination = useUiStore((state) => state.destination);
  const systemOpen = useUiStore((state) => state.systemOpen);
  const show = useUiStore((state) => state.show);

  return (
    <nav
      aria-label="Session"
      className="sticky bottom-0 z-20 border-t border-edge bg-surface/95 pb-[env(safe-area-inset-bottom)] backdrop-blur md:hidden"
    >
      <div className="mx-auto flex w-full max-w-5xl">
        {DESTINATIONS.map((entry) => {
          const current = !systemOpen && destination === entry.id;
          return (
            <button
              key={entry.id}
              type="button"
              aria-current={current ? 'page' : undefined}
              onClick={() => show(entry.id)}
              className={`flex min-h-touch flex-1 flex-col items-center justify-center gap-0.5 py-2 ${
                current ? 'text-accent' : 'text-muted'
              }`}
            >
              <span aria-hidden="true" className="text-xl leading-none">
                {entry.glyph}
              </span>
              <span className="text-xs tracking-wide uppercase">{entry.label}</span>
            </button>
          );
        })}
      </div>
    </nav>
  );
}
