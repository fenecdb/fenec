// A worker of the `jobs` collection, docs/functions: it claims the oldest
// ready jobs, each its own for a lease; the claim is held at the server until
// there is one (`wait`), so an idle worker costs nothing and a new job reaches
// it in under a millisecond. A job done is deleted; a job that fails is put
// back after a back-off; a worker that dies holds its jobs only until the
// lease lapses. Each step names the owner it was claimed by and requires its
// row, so a worker whose lease lapsed finds out (412) instead of acking
// another worker's job. (The queue itself is docs/queues.)
import { connect, expr, inc, raw } from '@fenecdb/web/client';

export async function work({ fenec, token, handlers, name = 'worker', lease = 30_000, wait = 30_000, signal, log = console.log }) {
  const jobs = connect(fenec, { token }).from('jobs');
  const me = `${name}-${Math.random().toString(36).slice(2, 6)}`;
  while (!signal?.aborted) {
    const owner = `${me}:${Date.now()}`; // a claim, not the worker: two claims of one job tell apart
    let claimed;
    try {
      claimed = await jobs.where(raw('run_at <= now()')).order('run_at').limit(10)
        .update({ owner, run_at: expr('now() + ?', lease), attempts: inc(1) }, { returning: true, wait, signal });
    } catch (e) {
      if (signal?.aborted) return;
      throw e;
    }
    for (const job of claimed) {
      log(`${me}: job ${job.id} ${job.kind}, attempt ${job.attempts}`);
      try {
        await handlers[job.kind](job.payload);
        await jobs.where('id', job.id).where('owner', owner).delete({ require: 1 });
      } catch (e) {
        log(`${me}: job ${job.id} failed (${e.message}); back in ${2 ** job.attempts} s`);
        await jobs.where('id', job.id).where('owner', owner)
          .update({ owner: null, run_at: expr('now() + ?', 1000 * 2 ** job.attempts) }, { require: 1 }).catch(() => {});
      }
    }
  }
}
