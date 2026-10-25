// A worker of the `jobs` collection, docs/functions: it claims one ready job
// at a time, its own for a lease; the claim is held at the server until there
// is one (`wait`), so an idle worker costs nothing and a new job reaches it in
// under a millisecond. A job done is deleted. A job that fails is put back
// after a back-off, and after `maxAttempts` tries is kept with its error and no
// time to run (a dead letter: no claim takes a job without one) for a person
// to look at. A worker that dies holds its job only until the lease lapses.
// Each step names the owner it was claimed by and requires its row, so a
// worker whose lease lapsed finds out (412) instead of acking another worker's
// job. (The queue itself is docs/queues.)
//
// One job to a claim, because a claim's lease is the job's: ten claimed
// together under one 30 s lease, and run one after the other, would lapse on
// the ninth while the first was still being done. A handler is also given the
// time the lease has left, and a signal that fires then.
import { connect, expr, inc, raw } from '@fenecdb/web/client';

/**
 * `token` is `() => Promise<token>`: the worker's own (service-token.mjs),
 * asked for again on each claim since a claim can wait half a minute.
 */
export async function work({
  fenec, token, handlers, id = Math.random().toString(36).slice(2, 6), lease = 30_000, wait = 30_000,
  maxAttempts = 3, backoff = 1000, signal, log = console.log,
}) {
  while (!signal?.aborted) {
    const jobs = connect(fenec, { token: await token() }).from('jobs');
    const owner = `${id}:${Date.now()}`; // a claim, not the worker: two claims of one job tell apart
    let claimed;
    try {
      claimed = await jobs.where(raw('run_at <= now()')).order('run_at').limit(1)
        .update({ owner, run_at: expr('now() + ?', lease), attempts: inc(1) }, { returning: true, wait, signal });
    } catch (e) {
      if (signal?.aborted) return;
      throw e;
    }
    for (const job of claimed) {
      log(`${id}: job ${job.id} ${job.kind}, attempt ${job.attempts}`);
      const mine = () => jobs.where('id', job.id).where('owner', owner);
      try {
        const left = AbortSignal.timeout(lease - 5000); // stop short of the lease
        await Promise.race([
          handlers[job.kind](job.payload, { signal: left }),
          new Promise((_, no) => left.addEventListener('abort', () => no(new Error('out of time')))),
        ]);
        await mine().delete({ require: 1 });
      } catch (e) {
        if (job.attempts >= maxAttempts) {
          log(`${id}: job ${job.id} failed (${e.message}); ${job.attempts} attempts, kept as dead`);
          await mine().update({ owner: null, run_at: null, error: e.message }, { require: 1 }).catch(() => {});
        } else {
          const ms = backoff * 2 ** job.attempts;
          log(`${id}: job ${job.id} failed (${e.message}); again in ${ms / 1000} s`);
          await mine().update({ owner: null, run_at: expr('now() + ?', ms), error: e.message }, { require: 1 }).catch(() => {});
        }
      }
    }
  }
}
