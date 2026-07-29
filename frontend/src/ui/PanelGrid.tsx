import type { ReactNode } from 'react';

/**
 * USB-08's responsive rule, in one place: single column on a phone, multi-panel from `md` up.
 *
 * It is a component rather than a class string copied into each screen so that "what counts as a
 * tablet" is one decision. SDD §5.9's tablet arrangement puts a narrow target/statistics column
 * beside a wide image surface, which is a `md:grid-cols-[minmax(16rem,1fr)_2fr]` shape M1-T04
 * will add here as a variant — not a second grid invented in a panel.
 */
export function PanelGrid({ children }: { children: ReactNode }): ReactNode {
  return <div className="grid grid-cols-1 gap-3 md:grid-cols-2">{children}</div>;
}
