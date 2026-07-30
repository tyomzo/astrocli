import type { ReactNode } from 'react';

import { BottomNav } from './BottomNav';
import { HeaderBar } from './HeaderBar';

/**
 * The frame every screen sits in — SDD §5.9.
 *
 * What is fixed here is fixed for the life of the app: the header with its permanent e-stop slot,
 * a scrolling content region, the three-destination navigation on a phone, and the safe-area
 * padding a phone with a gesture bar needs.
 *
 * The header is `sticky top-0` and the navigation `sticky bottom-0` so that the e-stop and the
 * destinations are reachable at any scroll position (USB-03). Only the middle scrolls, which is
 * also what keeps the e-stop out from under Chrome's collapsing URL bar.
 */
export function AppShell({ children }: { children: ReactNode }): ReactNode {
  return (
    <div className="flex min-h-dvh flex-col bg-surface text-fg">
      <HeaderBar />
      <main className="mx-auto w-full max-w-5xl flex-1 px-3 pt-3 pb-3">{children}</main>
      <BottomNav />
    </div>
  );
}
