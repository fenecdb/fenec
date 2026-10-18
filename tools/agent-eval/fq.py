#!/usr/bin/env python3
"""The agent's one tool: `fq '<statement>' ['<params JSON array>']`.

POSTs the statement to the server's /query and prints the status and the
answer, cut at OUTPUT characters (the log keeps it whole for the scoring).
A statement may come on stdin instead, for one holding single quotes.
Each call that reaches the server is a line of the run's log; past the
task's allowance it is refused without being sent. The harness writes a
`fq` script a run that calls this with the server's URL, the log and the
allowance before the agent's own arguments."""

import json
import sys
import time
import urllib.error
import urllib.request

OUTPUT = 4000


def main():
    url, log, allowance = sys.argv[1], sys.argv[2], int(sys.argv[3])
    args = sys.argv[4:]
    if len(args) > 2:
        sys.exit("usage: fq '<statement>' ['<params as a JSON array>'] -- quote the statement as one argument")
    if not args or args[0] == "-":
        statement = sys.stdin.read()
    else:
        statement = args[0]
    params = []
    if len(args) == 2:
        try:
            params = json.loads(args[1])
        except ValueError as e:
            sys.exit(f"params are not JSON: {e}")
        if not isinstance(params, list):
            sys.exit("params must be a JSON array: '[\"DE\", 10]'")
    if not statement.strip():
        sys.exit("no statement given")
    try:
        with open(log) as f:
            used = sum(1 for _ in f)
    except FileNotFoundError:
        used = 0
    if used >= allowance:
        sys.exit(f"refused: the {allowance} statements this task allows are used up. "
                 "Reply with your answer.")

    body = json.dumps({"query": statement, "params": params}).encode()
    req = urllib.request.Request(url + "/query", data=body,
                                 headers={"content-type": "application/json"})
    start = time.monotonic()
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            status, text = r.status, r.read().decode()
    except urllib.error.HTTPError as e:
        status, text = e.code, e.read().decode()
    except OSError as e:
        status, text = 0, f"no answer from the server: {e}"
    ms = round((time.monotonic() - start) * 1000, 1)
    with open(log, "a") as f:
        f.write(json.dumps({"n": used + 1, "statement": statement, "params": params,
                            "status": status, "body": text, "ms": ms}) + "\n")
    shown = text if len(text) <= OUTPUT else text[:OUTPUT] + f"... ({len(text) - OUTPUT} more characters)"
    print(f"HTTP {status}\n{shown}")
    print(f"({allowance - used - 1} of {allowance} statements left)")
    return 0 if 200 <= status < 300 else 1


if __name__ == "__main__":
    sys.exit(main())
