#!/usr/bin/env python3
"""Differential harness: node bin/cli.js vs ompchamber-server binary.

Boots one instance PER CLI (same seeded data dir per side, unique ports),
then runs the same command against both and diffs stdout/stderr/exit code.

Usage: python3 scripts/cli-diff.py [--verbose]
"""
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import urllib.request

WEB = "/Users/gemini/Documents/playground/ompchamber/packages/web"
NODE_CLI = ["node", "bin/cli.js"]
RS_CLI = ["./server-rs/target/debug/ompchamber-server"]
PW = "123456"
JS_PORT, RS_PORT = 3210, 3211

VOLATILE = [
    (re.compile(r"\b\d{4,7}\b"), "<PORT>"),           # dynamic ports in output
    (re.compile(r"pid \d+|PID: \d+|\"pid\": ?\d+"), "<PID>"),
    (re.compile(r"/tmp/[^\s\"]+"), "<TMP>"),
    (re.compile(r"\d{4}-\d{2}-\d{2}T[0-9:.]+Z?"), "<TS>"),
    (re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}"), "<UUID>"),
]

def normalize(text, own_port):
    text = text.replace(str(own_port), "<OWN-PORT>")
    for pattern, repl in VOLATILE:
        text = pattern.sub(repl, text)
    return text

def run_cli(base, argv, env, timeout=60):
    cmd = base + argv
    proc = subprocess.run(cmd, cwd=WEB, env=env, capture_output=True, text=True, timeout=timeout)
    return proc.returncode, proc.stdout, proc.stderr

def wait_health(port, timeout=90):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=2)
            return True
        except Exception:
            time.sleep(0.5)
    return False

def stop_all(cli, env):
    run_cli(cli, ["stop", "--all", "--quiet"], env, timeout=30)

def make_env(data_dir, port):
    env = dict(os.environ)
    env.update({
        "OMPCHAMBER_DATA_DIR": data_dir,
        "OMPCHAMBER_UI_PASSWORD": PW,
        "OMPCHAMBER_HOST": "127.0.0.1",
        "OMPCHAMBER_RUNTIME": "web",
    })
    env.pop("OMPCHAMBER_PORT", None)
    return env

CASES = [
    # (label, argv, needs_instance)
    ("help", ["--help"], False),
    ("version", ["--version"], False),
    ("control-help", ["control"], False),
    ("schedule-help", ["schedule", "--help"], False),
    ("session-help", ["session", "--help"], False),
    ("models-help", ["models", "--help"], False),
    ("projects-help", ["projects", "--help"], False),
    ("tunnel-help", ["tunnel", "help"], False),
    ("startup-help", ["startup", "--help"], False),
    ("connect-url-help", ["connect-url", "--help"], False),
    ("bad-port", ["--port", "99999"], False),
    ("bad-command", ["definitely-not-a-command"], False),
    ("removed-flag", ["--try-cf-tunnel"], False),
    ("status-none", ["status", "--quiet"], False),
    ("status-none-json", ["status", "--json"], False),
    ("status-none-human", ["status"], False),
    ("tunnel-providers", ["tunnel", "providers"], False),
    ("tunnel-profile-list", ["tunnel", "profile", "list"], False),
    ("tunnel-profile-list-json", ["tunnel", "profile", "list", "--json"], False),
    ("startup-status", ["startup", "status"], False),
    ("startup-status-json", ["startup", "status", "--json"], False),
    ("models", ["models"], True),
    ("models-json", ["models", "--json"], True),
    ("projects", ["projects"], True),
    ("projects-json", ["projects", "--json"], True),
    ("schedule-status", ["schedule", "status"], True),
    ("schedule-list", ["schedule", "list"], True),
    ("schedule-list-json", ["schedule", "list", "--json"], True),
    ("session-list", ["session", "list"], True),
    ("session-list-json", ["session", "list", "--json"], True),
    ("stop-missing-port", ["stop", "--port", "39999", "--quiet"], False),
    ("stop-missing-port-json", ["stop", "--port", "39999", "--json"], False),
    ("logs-missing", ["logs", "--port", "39999", "--no-follow"], False),
]

def main():
    verbose = "--verbose" in sys.argv
    js_data = tempfile.mkdtemp(prefix="oc-clidiff-js.")
    rs_data = tempfile.mkdtemp(prefix="oc-clidiff-rs.")
    js_env, rs_env = make_env(js_data, JS_PORT), make_env(rs_data, RS_PORT)

    failures, total = [], 0
    procs = {}
    try:
        # Boot one instance per CLI for the needs_instance cases.
        for label, cli, env, port in [("js", NODE_CLI, js_env, JS_PORT), ("rs", RS_CLI, rs_env, RS_PORT)]:
            argv = cli + ["serve", "--port", str(port), "--quiet"]
            env2 = dict(env)
            env2["OMPCHAMBER_PORT"] = str(port)
            procs[label] = subprocess.Popen(argv, cwd=WEB, env=env2,
                                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        print(f"boot: js={wait_health(JS_PORT)} rs={wait_health(RS_PORT)}")
        time.sleep(2)

        for label, argv, needs_instance in CASES:
            total += 1
            js_argv = [a.replace("{PORT}", str(JS_PORT)) for a in argv]
            rs_argv = [a.replace("{PORT}", str(RS_PORT)) for a in argv]
            try:
                jc, jo, je = run_cli(NODE_CLI, js_argv, js_env)
            except subprocess.TimeoutExpired:
                failures.append(f"{label}: JS timed out")
                continue
            try:
                rc, ro, re_ = run_cli(RS_CLI, rs_argv, rs_env)
            except subprocess.TimeoutExpired:
                failures.append(f"{label}: RS timed out")
                continue
            jo, je = normalize(jo, JS_PORT), normalize(je, JS_PORT)
            ro, re_ = normalize(ro, RS_PORT), normalize(re_, RS_PORT)
            if jc != rc or jo != ro or je != re_:
                detail = []
                if jc != rc: detail.append(f"exit js={jc} rs={rc}")
                if jo != ro:
                    detail.append(f"stdout:\n  js={jo[:300]!r}\n  rs={ro[:300]!r}")
                if je != re_:
                    detail.append(f"stderr:\n  js={je[:300]!r}\n  rs={re_[:300]!r}")
                failures.append(f"{label}: " + "; ".join(detail))
            elif verbose:
                print(f"  ok {label}")
    finally:
        run_cli(NODE_CLI, ["stop", "--all", "--quiet"], js_env, timeout=30)
        run_cli(RS_CLI, ["stop", "--all", "--quiet"], rs_env, timeout=30)
        for proc in procs.values():
            proc.send_signal(signal.SIGTERM)
        time.sleep(2)
        for proc in procs.values():
            proc.kill()
        shutil.rmtree(js_data, ignore_errors=True)
        shutil.rmtree(rs_data, ignore_errors=True)

    print(f"\n=== {total - len(failures)}/{total} identical ===")
    for failure in failures:
        print("DIFF", failure)
    sys.exit(1 if failures else 0)

if __name__ == "__main__":
    main()
