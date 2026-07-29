import type { ReactNode } from 'react';

import { HeaderBar } from './HeaderBar';

/**
 * The frame every screen sits in — SDD §5.9.
 *
 * What is fixed here is fixed for the life of the app: the header with its permanent e-stop slot,
 * a scrolling content region, and the safe-area padding a phone with a gesture bar needs. The
 * three-destination bottom navigation (◎target ▣frame ⛁stack) belongs to M1-T04, when there are
 * three destinations; a navigation bar with one live tab is a control that teaches the operator
 * to ignore it.
 */
export function AppShell({ children }: { children: ReactNode }): ReactNode {
  return (
    <div className="flex min-h-dvh flex-col bg-surface text-fg">
      <HeaderBar />
      <main className="mx-auto w-full max-w-5xl flex-1 px-3 pt-3 pb-[max(1rem,env(safe-area-inset-bottom))]">
        {children}
      </main>
    </div>
  );
}
