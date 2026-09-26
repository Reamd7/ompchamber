#!/usr/bin/env python3
"""Performance comparison: JS (node) vs Rust (release) server.

Boots both with identical seeded state + UI password, warms up, then measures:
  - boot time (spawn -> /health 200)
  - sequential latency (N=150) per endpoint: p50/p95/p99
  - concurrent throughput (16 workers x N)
  - steady-state RSS (server process + engine child)
Proxied endpoints are engine-dominated; they measure the server overhead delta.
"""
import concurrent.futures
import json
import os
import signal
import statistics
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

PASSWORD = "123456"
WEB = "/Users/gemini/Documents/playground/ompchamber/packages/web"
RS_BIN = WEB + "/server-rs/target/release/ompchamber-server"
REPO = "/Users/gemini/Documents/playground/ompchamber"
N_SEQ = 150
N_CONC = 400
WORKERS = 16

ENDPOINTS = [
    ("health", "/health"),
    ("version", "/api/version"),
    ("settings", "/api/config/settings"),
    ("magic-prompts", "/api/magic-prompts"),
    ("session-folders", "/api/session-folders"),
    ("dev-servers", "/api/dev-servers"),
    ("proxy-config", "/config"),
    ("proxy-agent", "/agent"),
]

def bench_req(port, path, cookie):
    url = f"http://127.0.0.1:{port}{path}"
    req = urllib.request.Request(url, headers={"Cookie": cookie, "Accept": "application/json"})
    t0 = time.perf_counter()
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            resp.read()
            return (time.perf_counter() - t0) * 1000, resp.status
    except urllib.error.HTTPError as e:
        e.read()
        return (time.perf_counter() - t0) * 1000, e.code

def login(port):
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}/auth/session",
        data=json.dumps({"password": PASSWORD}).encode(),
        headers={"Content-Type": "application/json"}, method="POST")
    with urllib.request.urlopen(req, timeout=10) as resp:
        return resp.headers.get("Set-Cookie", "").split(";")[0]

def pct(samples, p):
    ordered = sorted(samples)
    return ordered[min(len(ordered) - 1, int(len(ordered) * p))]

def boot_one(label, cmd, env_extra):
    data = tempfile.mkdtemp(prefix=f"oc-bench-{label}.")
    env = dict(os.environ)
    env.update({"OMPCHAMBER_DATA_DIR": data, "OMPCHAMBER_UI_PASSWORD": PASSWORD,
                "OMPCHAMBER_DIST_DIR": WEB + "/dist"})
    for key in ("OMPCHAMBER_RUNTIME", "OMPCHAMBER_HOST"):
        env.pop(key, None)
    env.update(env_extra)
    t0 = time.perf_counter()
    proc = subprocess.Popen(cmd, cwd=WEB, env=env,
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    port = env["OMPCHAMBER_PORT"]
    boot_ms = None
    for _ in range(240):
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=2):
                boot_ms = (time.perf_counter() - t0) * 1000
                break
        except Exception:
            time.sleep(0.05)
    return proc, port, boot_ms, data

def rss_mb(pid):
    try:
        out = subprocess.check_output(["ps", "-o", "rss=", "-p", str(pid)], text=True).strip()
        return int(out) / 1024
    except Exception:
        return 0.0

def children_rss_mb(pid):
    total = 0.0
    try:
        out = subprocess.check_output(["pgrep", "-P", str(pid)], text=True).split()
    except Exception:
        return 0.0
    for child in out:
        total += rss_mb(child)
    return total

def main():
    results = {}
    procs = []
    try:
        # Boot-time measurement: isolated, sequential.
        for label, cmd, port in [
            ("js", ["node", "server/index.js"], "3160"),
            ("rs", [RS_BIN, "--port", "3161", "--ui-password", PASSWORD], "3161"),
        ]:
            proc, p, boot_ms, data = boot_one(label, cmd, {"OMPCHAMBER_PORT": port})
            procs.append((label, proc))
            results.setdefault(label, {})["boot_ms"] = boot_ms
            results[label]["_data"] = data
            results[label]["_port"] = p
            proc.send_signal(signal.SIGTERM); proc.wait(timeout=10)
        procs = []

        # Latency/throughput: both up simultaneously (parity-style).
        data = tempfile.mkdtemp(prefix="oc-bench-both.")
        env = dict(os.environ)
        env.update({"OMPCHAMBER_DATA_DIR": data, "OMPCHAMBER_UI_PASSWORD": PASSWORD})
        for key in ("OMPCHAMBER_RUNTIME", "OMPCHAMBER_HOST"):
            env.pop(key, None)
        ej, er = dict(env), dict(env)
        ej["OMPCHAMBER_PORT"] = "3162"
        er["OMPCHAMBER_PORT"] = "3163"
        js = subprocess.Popen(["node", "server/index.js"], cwd=WEB, env=ej,
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        rs = subprocess.Popen([RS_BIN, "--port", "3163", "--ui-password", PASSWORD], cwd=WEB, env=er,
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        procs = [("js", js), ("rs", rs)]

        for label, port in [("js", 3162), ("rs", 3163)]:
            for _ in range(240):
                try:
                    urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=2)
                    break
                except Exception:
                    time.sleep(0.25)

        # Warm engines so proxied endpoints measure steady state.
        for port in (3162, 3163):
            for _ in range(120):
                try:
                    h = json.load(urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=2))
                    ep = h.get("openCodePort")
                    if ep:
                        import base64
                        auth = base64.b64encode(f"opencode:{os.environ.get('OPENCODE_SERVER_PASSWORD','')}".encode()).decode()
                        rq = urllib.request.Request(f"http://127.0.0.1:{ep}/global/health",
                                                    headers={"Authorization": f"Basic {auth}"})
                        body = urllib.request.urlopen(rq, timeout=2).read()
                        if b'"healthy":true' in body:
                            break
                except Exception:
                    pass
                time.sleep(0.5)

        cookies = {"js": login(3162), "rs": login(3163)}

        # Warmup.
        for label, port in [("js", 3162), ("rs", 3163)]:
            for _, path in ENDPOINTS:
                bench_req(port, path, cookies[label])

        print(f"{'endpoint':<16} {'':>7}JS p50/p95/p99 ms        {'':>1}Rust p50/p95/p99 ms       ratio(p50)")
        for name, path in ENDPOINTS:
            row = {}
            for label, port in [("js", 3162), ("rs", 3163)]:
                samples = []
                statuses = {}
                for _ in range(N_SEQ):
                    ms, status = bench_req(port, path, cookies[label])
                    statuses[status] = statuses.get(status, 0) + 1
                    if status == 200:
                        samples.append(ms)
                if len(samples) < N_SEQ * 0.5:
                    row[label] = None
                    row.setdefault("statuses", {}).update(statuses)
                    continue
                row[label] = (
                    pct(samples, 0.5), pct(samples, 0.95), pct(samples, 0.99),
                    statistics.mean(samples))
            if row["js"] is None or row["rs"] is None:
                print(f"{name:<16} n/a (statuses js={row.get('js-statuses') or row.get('statuses')} )")
                continue
            js_p50, rs_p50 = row["js"][0], row["rs"][0]
            ratio = f"{js_p50 / rs_p50:5.2f}x" if rs_p50 > 0 else "  inf"
            print(f"{name:<16} "
                  f"{row['js'][0]:8.1f} {row['js'][1]:7.1f} {row['js'][2]:7.1f}   "
                  f"{row['rs'][0]:8.1f} {row['rs'][1]:7.1f} {row['rs'][2]:7.1f}   {ratio}")
            results.setdefault("seq", {})[name] = {"js": row["js"], "rs": row["rs"]}

        print(f"\nConcurrent: {WORKERS} workers x {N_CONC} total reqs on /health + /api/version mix")
        for name, path in [("health", "/health"), ("version", "/api/version")]:
            row = {}
            for label, port in [("js", 3162), ("rs", 3163)]:
                paths = [path] * N_CONC
                t0 = time.perf_counter()
                with concurrent.futures.ThreadPoolExecutor(max_workers=WORKERS) as pool:
                    list(pool.map(lambda i: bench_req(port, paths[i], cookies[label]), range(N_CONC)))
                wall = time.perf_counter() - t0
                row[label] = N_CONC / wall
            print(f"  {name:<10} js {row['js']:8.0f} req/s   rust {row['rs']:8.0f} req/s   "
                  f"ratio {row['rs'] / row['js']:.2f}x")
            results.setdefault("conc", {})[name] = row

        time.sleep(2)
        for label, proc in procs:
            results.setdefault("rss", {})[label] = {
                "server": rss_mb(proc.pid),
                "engine_children": children_rss_mb(proc.pid),
            }
        print(f"\nRSS: js server {results['rss']['js']['server']:.0f} MB (+engine {results['rss']['js']['engine_children']:.0f} MB)"
              f"   rust server {results['rss']['rs']['server']:.0f} MB (+engine {results['rss']['rs']['engine_children']:.0f} MB)")
        print(f"Boot to /health: js {results['js']['boot_ms']:.0f} ms   rust {results['rs']['boot_ms']:.0f} ms")
    finally:
        for _, proc in procs:
            proc.send_signal(signal.SIGTERM)
        time.sleep(2)
        for _, proc in procs:
            proc.kill()

if __name__ == "__main__":
    main()
