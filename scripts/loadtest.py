#!/usr/bin/env python3
"""Small dependency-free load generator for oxide-api (stdlib only).

  scripts/loadtest.py --url http://localhost:8080 --key $KEY \
      --files tests/fixtures/native.pdf tests/fixtures/sample.docx \
      --concurrency 8 --requests 400

  # 80 concurrent uploads of a 32 MiB blob (admission / memory bound):
  scripts/loadtest.py --url http://localhost:8080 --key $KEY --blob-mib 31.9 \
      --concurrency 80 --requests 80

Optionally samples a container's memory with `docker stats` (--container).
"""

import argparse
import concurrent.futures as cf
import collections
import os
import re
import subprocess
import threading
import time
import urllib.error
import urllib.request


def one(url, key, body, accept="application/json"):
    req = urllib.request.Request(url, data=body, method="POST", headers={
        "Authorization": f"Bearer {key}",
        "Content-Type": "application/octet-stream",
        "Accept": accept,
    })
    t = time.perf_counter()
    try:
        with urllib.request.urlopen(req, timeout=300) as r:
            r.read()
            status, timing = r.status, r.headers.get("Server-Timing", "")
    except urllib.error.HTTPError as e:
        e.read()
        status, timing = e.code, ""
    except Exception as e:  # connection reset etc.
        status, timing = f"error:{type(e).__name__}:{getattr(e, "reason", e)}", ""
    return status, time.perf_counter() - t, timing


def mem_sampler(container, stop, peak):
    unit = {"B": 1, "KiB": 1024, "MiB": 1024**2, "GiB": 1024**3}
    proc = subprocess.Popen(["docker", "stats", "--format", "{{.MemUsage}}", container],
                            stdout=subprocess.PIPE, text=True)
    while not stop.is_set():
        line = proc.stdout.readline()
        m = re.search(r"([\d.]+)\s*(B|KiB|MiB|GiB)\s*/", line)
        if m:
            peak[0] = max(peak[0], float(m.group(1)) * unit[m.group(2)])
    proc.terminate()


def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(round(p / 100 * (len(xs) - 1))))]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", default="http://localhost:8080")
    ap.add_argument("--endpoint", default="/v1/extract/text")
    ap.add_argument("--key", default=os.environ.get("OXIDE_API_API_KEY", ""))
    ap.add_argument("--files", nargs="*", default=[])
    ap.add_argument("--blob-mib", type=float, default=0)
    ap.add_argument("--concurrency", type=int, default=8)
    ap.add_argument("--requests", type=int, default=100)
    ap.add_argument("--container")
    a = ap.parse_args()

    bodies = [open(f, "rb").read() for f in a.files]
    if a.blob_mib:
        bodies.append(os.urandom(int(a.blob_mib * 1024 * 1024)))
    assert bodies, "need --files or --blob-mib"

    stop, peak = threading.Event(), [0.0]
    sampler = None
    if a.container:
        sampler = threading.Thread(target=mem_sampler, args=(a.container, stop, peak), daemon=True)
        sampler.start()
        time.sleep(1.5)

    url = a.url + a.endpoint
    t0 = time.perf_counter()
    with cf.ThreadPoolExecutor(a.concurrency) as ex:
        results = list(ex.map(lambda i: one(url, a.key, bodies[i % len(bodies)]), range(a.requests)))
    wall = time.perf_counter() - t0
    time.sleep(1.5)
    stop.set()

    statuses = collections.Counter(r[0] for r in results)
    ok = [r[1] for r in results if r[0] == 200]
    print(f"requests={a.requests} concurrency={a.concurrency} wall={wall:.2f}s throughput={a.requests / wall:.1f}/s")
    print(f"statuses={dict(statuses)}")
    if ok:
        print(f"latency ok: p50={pct(ok, 50) * 1000:.1f}ms p90={pct(ok, 90) * 1000:.1f}ms "
              f"p99={pct(ok, 99) * 1000:.1f}ms max={max(ok) * 1000:.1f}ms")
    if a.container:
        print(f"peak container memory ≈ {peak[0] / 1024**2:.1f} MiB (docker stats sampling)")


if __name__ == "__main__":
    main()
