#!/usr/bin/env python3
"""The time zones `bucket(at, 1d, 'Europe/Berlin')` knows: IANA's rules
since 2000, out of the system's compiled tzdata (TZif files, as Python's
zoneinfo reads them), into crates/fenec-core/src/tz/zones.bin -- and the
references crates/fenec-core/tests/zones.txt holds the engine to.

    python3 tools/tz/gen.py [ZONEINFO_DIR]     # /usr/share/zoneinfo by default

A zone is the offset it had at the start of 1999, the transitions since,
and the rule its TZif footer -- a POSIX TZ string -- gives every year after
the last transition the rule does not make: a few bytes for a zone that
has kept its rule since before 1999, a few hundred for Gaza's. Every
offset the table gives is checked here against zoneinfo's, at every
transition and every six hours from 2000 to 2050, before it is written.

The bucket references are worked out here from zoneinfo's offsets alone,
by the definition and not the engine's walk over a zone's pieces: a day,
a week, a month starts at the first instant whose wall clock reads its
first moment or later; a shorter interval at the last instant, at or
before the time, at which the clock read the interval's start or jumped
past it.
"""

import calendar
import os
import random
import struct
import sys
import zoneinfo
from datetime import datetime, timezone

ROOT = sys.argv[1] if len(sys.argv) > 1 else '/usr/share/zoneinfo'
HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
BIN = os.path.join(REPO, 'crates/fenec-core/src/tz/zones.bin')
REF = os.path.join(REPO, 'crates/fenec-core/tests/zones.txt')

# The table answers from 2000 on; a year before it is kept so that a day
# that starts before 2000 is looked back on with the offset it had.
KEPT = calendar.timegm((1999, 1, 1, 0, 0, 0))
FROM = calendar.timegm((2000, 1, 1, 0, 0, 0))
CHECK_TO = calendar.timegm((2050, 1, 1, 0, 0, 0))
MAGIC = 0x5A54_4E46  # "FNTZ" read little-endian


def read_tzif(path):
    """The 64-bit data of a TZif file: transitions, each one's offset, the
    first type's offset, and the footer."""
    b = open(path, 'rb').read()
    if b[:4] != b'TZif':
        raise ValueError(f'{path} is not a TZif file')

    def counts(off):
        return struct.unpack('>6l', b[off + 20:off + 44])

    isut, isstd, leap, timecnt, typecnt, charcnt = counts(0)
    off = 44 + timecnt * 5 + typecnt * 6 + charcnt + leap * 8 + isstd + isut
    isut, isstd, leap, timecnt, typecnt, charcnt = counts(off)
    p = off + 44
    times = struct.unpack('>%dq' % timecnt, b[p:p + 8 * timecnt])
    p += 8 * timecnt
    idx = b[p:p + timecnt]
    p += timecnt
    types = []
    for _ in range(typecnt):
        ut, _dst, _ai = struct.unpack('>lBB', b[p:p + 6])
        p += 6
        types.append(ut)
    p += charcnt + leap * 12 + isstd + isut
    footer = b[p:].strip(b'\n').decode()
    return list(times), [types[i] for i in idx], types[0], footer


def parse_posix(s):
    """A POSIX TZ string: `(std, None)` or `(std, (dst, start, end))`, the
    offsets east of UTC in seconds, a rule `(month, week, weekday, secs)`."""
    i = 0

    def name():
        nonlocal i
        if s[i] == '<':
            i = s.index('>', i) + 1
        else:
            while i < len(s) and s[i].isalpha():
                i += 1

    def time():
        nonlocal i
        sign = 1
        if s[i] in '+-':
            sign = -1 if s[i] == '-' else 1
            i += 1
        parts = [0, 0, 0]
        for k in range(3):
            j = i
            while i < len(s) and s[i].isdigit():
                i += 1
            parts[k] = int(s[j:i])
            if k < 2 and i < len(s) and s[i] == ':':
                i += 1
            else:
                break
        return sign * (parts[0] * 3600 + parts[1] * 60 + parts[2])

    def rule():
        nonlocal i
        if s[i] != 'M':
            raise ValueError(f'{s}: only the M form of a rule is read')
        i += 1
        j = s.index('.', i)
        month = int(s[i:j])
        k = s.index('.', j + 1)
        week = int(s[j + 1:k])
        i = k + 1
        wday = int(s[i])
        i += 1
        at = 7200
        if i < len(s) and s[i] == '/':
            i += 1
            at = time()
        return (month, week, wday, at)

    name()
    std = -time()
    if i >= len(s):
        return std, None
    name()
    dst = std + 3600
    if s[i] != ',':
        dst = -time()
    if s[i] != ',':
        raise ValueError(s)
    i += 1
    start = rule()
    if s[i] != ',':
        raise ValueError(s)
    i += 1
    end = rule()
    return std, (dst, start, end)


def rule_day(year, r):
    """The UTC midnight of the day a rule names in `year`."""
    month, week, wday, _ = r
    first = (calendar.weekday(year, month, 1) + 1) % 7  # 0 = Sunday
    day = 1 + (wday - first) % 7 + (week - 1) * 7
    last = calendar.monthrange(year, month)[1]
    while day > last:
        day -= 7
    return calendar.timegm((year, month, day, 0, 0, 0))


def rule_changes(rule, year):
    """The rule's two changes in `year`, in UTC seconds."""
    std, (doff, start, end) = rule
    return [rule_day(year, start) + start[3] - std, rule_day(year, end) + end[3] - doff]


def rule_offset(rule, t):
    """The offset a POSIX rule gives at `t`, in seconds."""
    std, dst = rule
    if dst is None:
        return std
    doff, start, end = dst
    year = time_year(t)
    # Each year's start and end in UTC: a start is in standard time, an end
    # in daylight time, as POSIX reads them.
    on = rule_day(year, start) + start[3] - std
    off = rule_day(year, end) + end[3] - doff
    if on < off:
        return doff if on <= t < off else std
    return std if off <= t < on else doff


def time_year(t):
    return datetime.fromtimestamp(t, timezone.utc).year


def model(name):
    """A zone as the table holds it: the offset at the start of 1999, the
    transitions since that its rule does not make, the rule, and the year
    from which the rule answers."""
    times, offs, first, footer = read_tzif(os.path.join(ROOT, name))
    rule = parse_posix(footer) if footer else None
    init = first
    for t, o in zip(times, offs):
        if t <= KEPT:
            init = o
    trans = [(t, o) for t, o in zip(times, offs) if t > KEPT]
    # Drop the transitions the rule makes: those of every year from the
    # first whose transitions, and each later year's up to the file's last
    # (a TZif file holds them to 2037), are the rule's own -- not merely
    # after the last one it does not make: Egypt kept no daylight time
    # from 2015 to 2022, and its rule of 2023 would have given it some.
    if rule is not None and rule[1] is not None:
        last_year = time_year(times[-1]) if times else time_year(KEPT)
        y0 = last_year + 1
        for y in range(last_year, time_year(KEPT) - 1, -1):
            here = sorted(t for t, _ in trans if time_year(t) == y)
            made = sorted(rule_changes(rule, y))
            if here != made or any(dict(trans)[t] != rule_offset(rule, t) for t in here):
                break
            y0 = y
        trans = [(t, o) for t, o in trans if time_year(t) < y0]
        # From that year's first change on, not from its start: Turks and
        # Caicos kept -4 from 2015 into 2018, when its rule of -5 and -4
        # took over in March.
        rule_from = min(rule_changes(rule, y0))
    elif rule is not None:
        rule_from = None
        if rule[1] is None:
            # A rule with no daylight time is the last offset kept.
            last = trans[-1][1] if trans else init
            if last != rule[0]:
                raise SystemExit(f'{name}: the footer {footer} is not the last offset, {last}')
            rule = None
    else:
        rule_from = None
    return init, trans, rule, rule_from


def model_offset(m, t):
    init, trans, rule, rule_from = m
    o = init
    for tt, oo in trans:
        if tt <= t:
            o = oo
        else:
            break
    if rule is not None and t >= rule_from and (not trans or t >= trans[-1][0]):
        return rule_offset(rule, t)
    return o


def check(name, m):
    """The table's offsets against zoneinfo's, at every transition there is
    from 2000 to 2050 and every six hours."""
    z = zoneinfo.ZoneInfo(name)
    times, _, _, _ = read_tzif(os.path.join(ROOT, name))
    points = list(range(FROM, CHECK_TO, 6 * 3600))
    for t in times:
        if FROM <= t < CHECK_TO:
            points += [t - 1, t, t + 1]
    for t in points:
        want = int(datetime.fromtimestamp(t, z).utcoffset().total_seconds())
        got = model_offset(m, t)
        if want != got:
            raise SystemExit(f'{name} at {t}: the table gives {got}, zoneinfo {want}')


def varint(v):
    out = bytearray()
    while True:
        b = v & 0x7F
        v >>= 7
        if v:
            out.append(b | 0x80)
        else:
            out.append(b)
            return bytes(out)


def write_table(names, version):
    models = {n: model(n) for n in names}
    for n in names:
        check(n, models[n])
    # Zones whose tables are alike share one entry.
    datas, data_of = [], {}
    for n in names:
        init, trans, rule, rule_from = models[n]
        key = (init, tuple(trans), rule, rule_from)
        if key not in data_of:
            data_of[key] = len(datas)
            datas.append(key)
    offsets = sorted({k[0] for k in datas} | {o for k in datas for _, o in k[1]}
                     | {o for k in datas if k[2] for o in (k[2][0], k[2][1][0])})
    rules = sorted({k[2] for k in datas if k[2]})
    if len(offsets) > 255 or len(rules) > 254:
        raise SystemExit('the table counts offsets and rules in a byte')
    for o in offsets:
        if o % 60:
            raise SystemExit(f'an offset of {o} s is no whole minute')
    # Laid out to be read where it lies, with no pass over it first: the
    # header says where the names start, offsets and rules are records of a
    # fixed size, and each zone's entry is found by its place in an index.
    # Parsed into lists and the names front-coded, the reader was 1.6 KB
    # more of the browser module, 0.7 KB brotli of every page, for a table
    # 5.4 KB smaller, 0.8 KB brotli, fetched by the pages that name a zone.
    head = bytearray()
    head += struct.pack('<I', MAGIC)
    v = version.encode()
    head += bytes([len(v)]) + v
    head += struct.pack('<q', FROM * 1000)
    head += struct.pack('<q', KEPT * 1000)
    names_at_pos = len(head)
    head += struct.pack('<I', 0)
    head += bytes([len(offsets)])
    for o in offsets:
        head += struct.pack('<h', o // 60)
    head += bytes([len(rules)])
    for std, (dst, start, end) in rules:
        head += bytes([offsets.index(std), offsets.index(dst)])
        for (month, week, wday, at) in (start, end):
            head += bytes([month, week, wday]) + struct.pack('<i', at)
    head += struct.pack('<H', len(datas))
    index_at = len(head)
    head += bytes(4 * len(datas))
    entries = bytearray()
    places = []
    for init, trans, rule, rule_from in datas:
        places.append(len(head) + len(entries))
        entries += bytes([offsets.index(init), 255 if rule is None else rules.index(rule)])
        if rule is not None:
            entries += varint(rule_from - KEPT)
        entries += varint(len(trans))
        prev = KEPT
        for t, o in trans:
            entries += varint(t - prev) + bytes([offsets.index(o)])
            prev = t
    for k, at in enumerate(places):
        head[index_at + 4 * k:index_at + 4 * k + 4] = struct.pack('<I', at)
    out = head + entries
    out[names_at_pos:names_at_pos + 4] = struct.pack('<I', len(out))
    # Names in order, each its length, its bytes and its entry.
    out += struct.pack('<H', len(names))
    for n in sorted(names):
        b = n.encode()
        out += bytes([len(b)]) + b + struct.pack('<H', data_of[datas_key(models[n])])
    os.makedirs(os.path.dirname(BIN), exist_ok=True)
    open(BIN, 'wb').write(out)
    return models, len(datas), len(rules), len(offsets), len(out)


def datas_key(m):
    init, trans, rule, rule_from = m
    return (init, tuple(trans), rule, rule_from)


# ------------------------------------------------------------- references

DAY_MS = 86_400_000


def offset_ms(z, ms):
    """zoneinfo's offset at `ms`, in milliseconds."""
    t = datetime.fromtimestamp(ms // 1000, z)
    return int(t.utcoffset().total_seconds()) * 1000


def wall(z, ms):
    return ms + offset_ms(z, ms)


def truncate(w, interval):
    """`w`, wall milliseconds, truncated as `bucket` truncates UTC."""
    kind, n, origin = interval
    if kind == 'ms':
        return (w - origin) // n * n + origin
    days = w // DAY_MS
    d = datetime.fromtimestamp(days * 86400, timezone.utc)
    months = d.year * 12 + d.month - 1
    b = months - months % n
    return calendar.timegm((b // 12, b % 12 + 1, 1, 0, 0, 0)) * 1000


def transitions(z, a, b):
    """The instants in [a, b] at which the offset changes, found by
    zoneinfo alone: sampled a quarter of an hour apart, then halved."""
    out = []
    step = 15 * 60_000
    prev_t, prev_o = a, offset_ms(z, a)
    t = a + step
    while t <= b + step:
        o = offset_ms(z, t)
        if o != prev_o:
            lo, hi = prev_t, t
            while hi - lo > 1:
                mid = (lo + hi) // 2
                if offset_ms(z, mid) == prev_o:
                    lo = mid
                else:
                    hi = mid
            out.append(hi)
        prev_t, prev_o = t, o
        t += step
    return out


AROUND = {}


def around(z, t):
    """The transitions within two days of `t`, and the offsets there."""
    key = (str(z), t)
    if key not in AROUND:
        window = transitions(z, t - 2 * DAY_MS, t + 2 * DAY_MS)
        seen = {offset_ms(z, x) for x in range(t - 2 * DAY_MS, t + 2 * DAY_MS, 3_600_000)}
        seen |= {offset_ms(z, u) for u in window} | {offset_ms(z, u - 1) for u in window}
        AROUND.clear()
        AROUND[key] = (window, seen)
    return AROUND[key]


def bucket(z, t, interval):
    """`bucket(t, interval, zone)` by its definition."""
    kind, n, _ = interval
    w = truncate(wall(z, t), interval)
    calendar_like = kind == 'mo' or n % DAY_MS == 0
    # Around where the bucket starts: `w` for a day or longer, which may be
    # a month before `t`; `t` for an hour or less.
    window, seen = around(z, w if calendar_like else t)
    if calendar_like:
        # The first instant whose wall clock reads `w` or later.
        cands = [w - o for o in seen] + window
        ok = [u for u in cands if wall(z, u) >= w]
        return min(ok)
    # The last instant, at or before `t`, whose clock reads `w` -- or at
    # which it jumps from before `w` to past it.
    cands = [w - o for o in seen if w - o <= t]
    shown = [u for u in cands if wall(z, u) == w]
    jumped = [u for u in window if u <= t and wall(z, u - 1) < w < wall(z, u)]
    return max(shown + jumped)


# 40m: boundaries a jump of an hour skips without landing on one.
INTERVALS = [('15m', ('ms', 15 * 60_000, 0)), ('40m', ('ms', 40 * 60_000, 0)),
             ('1h', ('ms', 3_600_000, 0)),
             ('3h', ('ms', 3 * 3_600_000, 0)), ('1d', ('ms', DAY_MS, 0)),
             ('1w', ('ms', 7 * DAY_MS, 4 * DAY_MS)), ('1mo', ('mo', 1, 0))]

REF_ZONES = [
    'Europe/Berlin', 'America/New_York', 'Europe/Istanbul', 'Australia/Lord_Howe',
    'Asia/Kolkata', 'America/Sao_Paulo', 'Pacific/Apia', 'America/Havana',
    'Asia/Beirut', 'America/Santiago', 'Europe/Dublin', 'Asia/Gaza',
]


def write_refs(names, models):
    lines = [
        '# Written by tools/tz/gen.py from zoneinfo: do not edit.',
        '# offsets <name> <fnv1a-64 of the offsets, in minutes, at 2000-01-01 + k * 11h 13m 7s up to 2045>',
        '# bucket <zone> <t> then, for ' + ' '.join(i for i, _ in INTERVALS) +
        ', t minus the start',
    ]
    for n in sorted(names):
        z = zoneinfo.ZoneInfo(n)
        h = 0xcbf29ce484222325
        step = (11 * 3600 + 13 * 60 + 7)
        t = FROM
        end = calendar.timegm((2045, 1, 1, 0, 0, 0))
        while t < end:
            o = int(datetime.fromtimestamp(t, z).utcoffset().total_seconds()) // 60
            for b in struct.pack('<h', o):
                h ^= b
                h = (h * 0x100000001b3) & 0xFFFFFFFFFFFFFFFF
            t += step
        lines.append(f'offsets {n} {h:016x}')
    rnd = random.Random(20261012)
    for n in REF_ZONES:
        z = zoneinfo.ZoneInfo(n)
        times, _, _, _ = read_tzif(os.path.join(ROOT, n))
        pick = [t for t in times if FROM + 3 * 86400 <= t < CHECK_TO]
        # Each transition near which a bucket is asked: every one up to
        # 2027, a sample after.
        early = [t for t in pick if t < calendar.timegm((2027, 1, 1, 0, 0, 0))]
        late = [t for t in pick if t not in early]
        chosen = early if len(early) <= 8 else rnd.sample(early, 8)
        chosen += rnd.sample(late, min(2, len(late)))
        points = set()
        for t in sorted(chosen):
            ms = t * 1000
            for d in (-3_600_001, -1, 0, 1_800_000, 7_200_000):
                points.add(ms + d)
        for _ in range(6):
            points.add(rnd.randrange(FROM * 1000, CHECK_TO * 1000))
        for t in sorted(points):
            row = [str(t - bucket(z, t, iv)) for _, iv in INTERVALS]
            lines.append(f'bucket {n} {t} ' + ' '.join(row))
    open(REF, 'w').write('\n'.join(lines) + '\n')
    return len(lines)


def main():
    version = open(os.path.join(ROOT, '+VERSION')).read().strip() if os.path.exists(
        os.path.join(ROOT, '+VERSION')) else 'unknown'
    names = sorted(n for n in zoneinfo.available_timezones() if n not in ('Factory', 'localtime', 'posixrules'))
    models, ndata, nrules, noffsets, size = write_table(names, version)
    print(f'tzdata {version}: {len(names)} names, {ndata} tables, {nrules} rules, '
          f'{noffsets} offsets -> {os.path.relpath(BIN, REPO)} {size} bytes')
    n = write_refs(names, models)
    print(f'{n} lines -> {os.path.relpath(REF, REPO)}')


if __name__ == '__main__':
    main()
