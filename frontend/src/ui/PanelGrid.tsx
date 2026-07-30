import type { ReactNode } from 'react';

/**
 * USB-08's responsive rule, in one place: single column on a phone, multi-panel from `md` up.
 *
 * It is a component rather than a class string copied into each screen so that "what counts as a
 * tablet" is one decision.
 *
 * Two layouts, both from SDD §5.9:
 *
 *  * `even` — equal columns, for the diagnostic cards that have no hierarchy between them.
 *  * `sidebar` — the operating arrangement: a narrow target/statistics column beside a wide image
 *    surface. The column carries the target *and* the stack statistics deliberately, because both
 *    answer "what is this session doing" and keeping them together leaves the image surface
 *    uninterrupted — the same reason the D-pad overlays the image rather than sitting beside it.
 */
type Layout = 'even' | 'sidebar';

const LAYOUT: Record<Layout, string> = {
  even: 'md:grid-cols-2',
  sidebar: 'md:grid-cols-[minmax(16rem,1fr)_2fr] md:items-start',
};

export function PanelGrid({
  children,
  layout = 'even',
}: {
  children: ReactNode;
  layout?: Layout;
}): ReactNode {
  return <div className={`grid grid-cols-1 gap-3 ${LAYOUT[layout]}`}>{children}</div>;
}
