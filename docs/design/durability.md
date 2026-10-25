# Durability

The long form of the invariants [CLAUDE.md](../../CLAUDE.md) lists under *Durability*: a storage error stopping writes, buffered writes written behind the lock, the sync log, and the one shutdown (added after the move). Each section below is a paragraph moved word for word from CLAUDE.md, which keeps its rule in a few sentences and links here; "above" and "below" in them refer to the order the invariants had there, which [README.md](README.md#the-original-order) lists.

## Storage errors

**A storage error stops writes.** Once the sink refuses an append, a sync or a
rewrite, every later write and sync returns `Error::Io` until the file is
reopened (`Database::failure`); reads go on, from memory and from the
mapped file's pages. A page that cannot be read in is the process's end
(`SIGBUS`), not an error -- which is what `--no-mmap` is for on a disk that
fails reads or a network file system -- and a mapped file is only ever
replaced by rename: a copy over it in place took a server down. A failed `fsync` is
never retried -- the kernel may already have dropped the pages -- and
`fenec-server` under `--sync always` reports it (500) instead of the success it
had not yet sent. That fsync runs *outside* the exclusive lock: under it a
write only calls `Database::flush`, which hands back a `Durability` to run once
the lock is released, and `FileSink` writes the bytes there as well (a `write`
under the lock waited out concurrent fsyncs on macOS). The writes the lock
still makes -- the 1 MB buffer's once it is full, a handover's -- take the
sink's disk, which a durability holds through its fsync, so both are put
off while one does (`Sink::syncing`, a `try_lock`): the buffer up to
`WRITE_HELD` (8 MB), a handover to twice its bounds. A ledger's transfer
landing a handover waited out the syncer's `F_FULLFSYNC` of a quarter
second's writes, 52 ms, every request behind it. A durability whose bytes
an earlier fsync already covered runs none, which is the group commit: 268 ->
1 156 durable writes/s over eight clients. A failed one is reported back with
`Database::fail` so the engine stops taking writes. The syncer of `--sync
<ms>` flushes the same way, a database's and each of a node's tenants'
(`Tenants::sync_dirty`): the tenants' held their lock through the fsync, and
every read and write of one waited up to 27 ms a pass on macOS.

## Writes behind the lock

**Buffered writes are written behind the lock** (`Database::write_behind`,
`Sink::write_behind`). On macOS a `write` that takes a file's dirty pages
past each 8 MB waits for the disk: 1 MB writes appended one after another
took 1.5-2.9 ms every eighth, 2.0-11 ms with the file mapped, and
preallocating 128 MB (`F_PREALLOCATE`) changed nothing. The buffer written
as it outgrew 1 MB, under the write lock, held four readers beside blocks of
1 000 writes 4-6 ms every 8 MB. `write_behind` hands back what writes the
buffer, with no fsync, for the caller to run once it has let the lock go,
as it runs a durability: a server asks after every write that `--sync
always` does not fsync (`flush_for`), the buffer once it holds 64 KB
(`BEHIND_AT`). It passes over a disk a durability holds (`try_lock`), whose
fsync writes the bytes anyway: waiting, a buffered write waited out the
syncer's `F_FULLFSYNC`, a ledger's transfer once 126 ms, where it had
touched no disk before. A sink
asked once leaves its buffer for it from then on, an append writing it
only past `WRITE_HELD`, should the writes behind fall back. A handover writes what is pending, so for a database written behind
it is put off to the next block's start (`hand_over_before`), where the
write behind -- every byte, once a handover is due -- left nothing pending,
and runs at the end of a block only past twice its bounds: written there,
its bytes met the same stall under the lock. A replica and the shell write
as before; the native library writes behind as a server does (below).

## The sync log

**A durable write syncs a log beside the file, not the file**
(`synclog.rs`, Linux and Android). The file only grows, and on ext4 an
fsync of a file whose length changed commits the journal too: in Docker's
VM a 300-byte append and its `fdatasync` took 356 us, the same bytes
written into room the file already had and synced 65 -- why PostgreSQL
fills its WAL segments first. So `FileSink`'s sync writes what the file
took since the last one into `<file>.sync` (the whole name and `.sync`:
with the extension replaced, `x.db` and `x.fenec` shared one, as they
shared `.compacting` and `.beside` -- `fs::beside` names all three), 4 KB
of header and 256 KB of entries written once at full size and never
grown, and syncs that alone (`Tail`). The file is fsynced when an entry
would not fit, a sync holds more than 64 KB, or the sink is new to the
file -- whose bytes an earlier process may have left unsynced -- and the
log then begins a generation from where the file stands. An entry is the
file's bytes at a place, with its generation and a hash, so a cut one
ends the log; the header names the generation, where the file stood
synced, the file's device and inode, and a hash of the 4 KB before that
point, so a log is applied to its own file alone -- not to one renamed
into the place, even one holding the same bytes. `create` applies it
before the file is read or mapped (`synclog::recover`), fsyncs the file
and wipes the header, so a file cut below the log's end after that open
is not written over at the next. Everything that renames a file into a
database's place removes the log first and makes the removal durable --
`swap_in` after syncing the file it replaces, a restore, a backup
unsealed, a tenant's import (`fs::forget_sync_log`); a clean close syncs
the file and removes it, so a node keeps none beside a closed tenant, and
`Tenants::stats` counts it; a log that cannot be made is not tried again
until the next open, each try being 260 KB of writes. `open_read_only`
applies none: after a power loss, `fenec types` or a raw copy taken
before the next open may lack the last durable writes. On macOS
`F_FULLFSYNC` flushes the drive either way, 3.9 ms appended or not, so
there is no log (`ENABLED`; `fs::keep_sync_log` has a test thread keep
one). In Docker a durable update went 624 -> 325 us p50 (`make
roundtrip-bench`), against PostgreSQL's 378; `tests/synclog.rs` crashes
the machine (the process forgotten, the file cut back to the log's
start) through a full log, a sync past an entry, a torn header, a file
renamed in, a second open and two files of one stem, and fenec-core's
and fenec-http's tests pass on Linux, where every file keeps one.

## The one shutdown

**Every way a server stops is the one shutdown** (`fenec-server`'s
`durability`). `SIGINT`, `SIGTERM` and `SIGHUP` set an atomic in their
handler, which is all a handler may safely do, and the thread that syncs
-- `run_syncer` over a file, `serve_dir`'s loop over a node's tenants --
sees it at its next tick: the held writes answered (`waits::end_all`),
what was registered to run first (`before_shutdown`: `--follow`'s
follower stopped with every change it applied on disk, the traces sent),
then the final sync and the checkpoint, the write lock held to
`process::exit`, so no write is answered between the sync and the exit.
`--exit-on-stdin-close` sets the same flag (`request_shutdown`) from a
thread that reads stdin to its end. An app
that starts a server for a file it opened holds a pipe on the server's
stdin, and when the app is killed (`kill -9`, a crash) the system closes
the pipe, where no signal would have come: the desktop studio's spike
(#381) found the orphaned server, its parent now pid 1, holding
`<file>.lock`, so the file was refused to the app until its next launch
reaped the server from a registry of pids. Watching the parent's pid was
the alternative: a poll, late by its interval and fooled by a pid used
again, and macOS has no `PR_SET_PDEATHSIG`. What arrives on stdin is read
into a 4 KB buffer and dropped, so a parent's writes never block on a
full pipe and nothing grows. `/dev/null`, or a stdin closed before the
start, ends at once and so does the server, exit 0, which a parent that
forgot the pipe learns at its first start. A terminal is refused at the
start (exit 2): a server reading one from the background is stopped whole
by `SIGTTIN`, its lock held -- the stuck server the flag is for -- and in
a terminal Ctrl-C makes the same shutdown. A way down of its own, an
exit from the stdin thread, would have been a second place to hold the
lock to the exit, and one the `--dir` loop and the follower were never
taught. `tests/stdin.rs` holds a file's and a node's shutdown to it, a
server without the flag to never reading stdin, `/dev/null` and a
terminal.
