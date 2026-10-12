// An order's state as people read it: `assigned` or `picked` whose lease
// ran out is lapsed -- the courier went quiet, and the next claim takes it.
export interface Stated {
  status: string;
  run_at: string | null;
}

export function shownState(o: Stated, now: number): string {
  if ((o.status === 'assigned' || o.status === 'picked') && o.run_at && Date.parse(o.run_at) < now) return 'lapsed';
  return o.status;
}

/** The time a page is rendered at, on the server. */
export const serverNow = () => Date.now();
