import os, sys, time, fcntl, statistics
path, mode, n = sys.argv[1], sys.argv[2], int(sys.argv[3])
fd = os.open(path, os.O_RDWR | os.O_CREAT | os.O_TRUNC, 0o644)
buf = b"x" * 1024
ts = []
for i in range(n):
    os.write(fd, buf)
    t = time.perf_counter()
    if mode == "full":
        fcntl.fcntl(fd, fcntl.F_FULLFSYNC)
    elif mode == "fsync":
        os.fsync(fd)
    else:
        os.fdatasync(fd)
    ts.append((time.perf_counter() - t) * 1e6)
os.close(fd); os.unlink(path)
ts.sort()
print(f"{mode}: {n} syncs of a 1 KB append, p50 {ts[n//2]:.0f} us, mean {statistics.mean(ts):.0f} us, p99 {ts[int(n*0.99)]:.0f} us, {1e6/statistics.mean(ts):.0f}/s")
