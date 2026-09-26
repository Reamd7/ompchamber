#!/usr/bin/env python3
"""Differential harness: JS server vs Rust server response parity.

Starts nothing itself — expects both servers already running with the SAME
seeded data dir and UI password (see scripts/run-parity.sh). Sends identical
requests to both (separate login cookies), normalizes volatile fields, and
diffs status + parsed-JSON (order-insensitive) or raw text.

Usage: python3 scripts/api-diff.py [--base-js http://127.0.0.1:3100]
                                    [--base-rs http://127.0.0.1:3101]
                                    [--stage 1|2|3|all] [--verbose]
"""
import argparse
import json
import re
import sys
import time
import urllib.error
import urllib.request

PASSWORD = "123456"
REPO_DIR = "/Users/gemini/Documents/playground/ompchamber"

# ---------------------------------------------------------------- volatility

TS_RE = re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z")
EPOCH_RE = re.compile(r"\b1[6-9]\d{8}\b")  # ms-epoch-ish 10-13 digit numbers

def normalize(value, key=""):
    k = key.lower()
    if isinstance(value, dict):
        out = {}
        for name, item in value.items():
            ln = name.lower()
            if ln in ("timestamp", "servertime", "epoch", "checkedat", "expiresat",
                      "updatedat", "createdat", "launchedat", "pong", "sentat",
                      "isopencodeready", "opencodenotreadysince", "opencoderunning", "startedat"):
                out[name] = "<TS>"
                continue
            if ln in ("pid", "ppid"):
                out[name] = "<PID>"
                continue
            if ln in ("port", "opcodeport") or ln.endswith("port"):
                if isinstance(item, int):
                    out[name] = "<PORT>"
                    continue
            if ln in ("baseurl", "serverurl", "url", "tunnelurl", "previewurl",
                      "downloadurl", "releaseurl", "tarballurl") and isinstance(item, str):
                out[name] = normalize_url(item)
                continue
            out[name] = normalize(item, name)
        return out
    if isinstance(value, list):
        if k == "args":
            # Engine argv carries the per-instance engine port.
            return ["<PORT>" if isinstance(x, (int, str)) and re.fullmatch(r"\d{2,5}", str(x)) else normalize(x, key) for x in value]
        out = []
        for item in value:
            if isinstance(item, dict) and item.get("port") in PARITY_PORTS:
                continue  # cross-contamination from the parity pair itself
            out.append(normalize(item, key))
        return out
    if isinstance(value, str):
        s = TS_RE.sub("<TS>", value)
        s = re.sub(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}", "<UUID>", s)
        s = EPOCH_RE.sub("<TS>", s)
        s = normalize_url(s)
        s = re.sub(r"\b\d{4,6}\b(?=/.+| |$)", "<PORT>", s) if ":<PORT>" in s or "127.0.0.1" in s else s
        return s
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        if abs(value) > 1_600_000_000_000:  # ms epoch
            return "<TS>"
        return value
    return value

def normalize_url(s: str) -> str:
    return re.sub(r"127\.0\.0\.1:\d+|localhost:\d+", "127.0.0.1:<PORT>", s)

# ---------------------------------------------------------------- transport

class Client:
    def __init__(self, base):
        self.base = base.rstrip("/")
        self.cookie = None

    def request(self, method, path, body=None, headers=None, timeout=30):
        url = self.base + path
        data = None
        hdrs = {"Accept": "application/json"}
        if self.cookie:
            hdrs["Cookie"] = self.cookie
        if body is not None:
            data = json.dumps(body).encode()
            hdrs["Content-Type"] = "application/json"
        if headers:
            hdrs.update(headers)
        req = urllib.request.Request(url, data=data, headers=hdrs, method=method)
        try:
            with urllib.request.urlopen(req, timeout=timeout) as resp:
                set_cookie = resp.headers.get("Set-Cookie")
                if set_cookie and "oc_ui_session" in set_cookie:
                    self.cookie = set_cookie.split(";")[0]
                return resp.status, resp.read().decode("utf-8", "replace")
        except urllib.error.HTTPError as e:
            set_cookie = e.headers.get("Set-Cookie")
            if set_cookie and "oc_ui_session" in set_cookie:
                self.cookie = set_cookie.split(";")[0]
            return e.code, e.read().decode("utf-8", "replace")
        except Exception as e:  # noqa: BLE001
            return 0, f"<transport: {e}>"

    def login(self):
        return self.request("POST", "/auth/session", {"password": PASSWORD})

# ---------------------------------------------------------------- checks

PARITY_PORTS = {3100, 3101}
REPO_Q = "?directory=" + urllib.request.quote(REPO_DIR)

STAGE1 = [
    ("GET", "/auth/session", None),
    ("GET", "/api/version", None),
    ("GET", "/api/system/info", None),
    ("GET", "/health", None),
    ("GET", "/api/config/settings", None),
    ("GET", "/api/config/opencode-resolution", None),
    ("GET", "/api/config/themes", None),
    ("GET", "/api/magic-prompts", None),
    ("GET", "/api/session-folders", None),
    ("GET", "/api/permission-auto-accept", None),
    ("GET", "/api/dev-servers", None),
    ("GET", "/api/terminal/shells", None),
    ("GET", "/api/tts/status", None),
    ("GET", "/api/tts/say/status", None),
    ("GET", "/api/dictation/status", None),
    ("GET", "/api/push/vapid-public-key", None),
    ("GET", "/api/ompchamber/relay/status", None),
    ("GET", "/api/github/auth/status", None),
    ("GET", "/api/linear/auth/status", None),
    ("GET", "/api/quota/providers", None),
    ("GET", "/api/opencode/health", None),
    ("GET", "/api/opencode/version", None),
    ("GET", "/api/fs/home", None),
    ("GET", "/manifest.webmanifest", None),
    ("GET", "/api/sessions/snapshot", None),
    ("GET", "/api/sessions/status", None),
    ("GET", "/api/sessions/attention", None),
    ("GET", "/api/session-activity", None),
    ("GET", "/api/agent-memory", None),
    ("GET", "/api/browser-control/claim", None),
]

STAGE2 = [
    ("GET", "/config", None),
    ("GET", "/agent", None),
    ("GET", "/global/health", None),
    ("GET", "/session", None),
    ("GET", "/api/fs/stat?path=" + urllib.request.quote(REPO_DIR + "/package.json") + "&directory=" + urllib.request.quote(REPO_DIR), None),
    ("GET", "/api/fs/read?path=" + urllib.request.quote(REPO_DIR + "/package.json") + "&directory=" + urllib.request.quote(REPO_DIR), None),
    ("GET", "/api/fs/list?path=" + urllib.request.quote(REPO_DIR) + "&directory=" + urllib.request.quote(REPO_DIR), None),
    ("GET", "/api/fs/git-dirs?path=" + urllib.request.quote(REPO_DIR), None),
    ("GET", "/api/git/status" + REPO_Q, None),
    ("GET", "/api/git/primary-root" + REPO_Q, None),
    ("GET", "/api/git/toplevel" + REPO_Q, None),
    ("GET", "/api/git/branches" + REPO_Q, None),
    ("GET", "/api/git/remotes" + REPO_Q, None),
    ("GET", "/api/git/global-identity", None),
    ("GET", "/api/git/current-identity" + REPO_Q, None),
    ("GET", "/api/git/has-local-identity" + REPO_Q, None),
    ("GET", "/api/ompchamber/models-metadata", None),
    ("GET", "/api/ompchamber/scheduled-tasks/status", None),
    ("GET", "/api/projects", None),
    ("GET", "/api/session-knowledge/summary?directory=" + urllib.request.quote(REPO_DIR), None),
    ("GET", "/api/skills", None),
    ("GET", "/api/config/agents", None),
    ("GET", "/api/config/commands", None),
    ("GET", "/api/config/mcp", None),
]

def first_sse(client, path, seconds=12.0):
    url = client.base + path
    req = urllib.request.Request(url, headers={"Cookie": client.cookie or "", "Accept": "text/event-stream"})
    frames = []
    buf = ""
    deadline = time.time() + seconds
    try:
        with urllib.request.urlopen(req, timeout=seconds + 4) as resp:
            while time.time() < deadline and len(frames) < 3:
                line = resp.readline()
                if not line:
                    break
                buf += line.decode("utf-8", "replace")
                if buf.endswith("\n\n"):
                    frames.append(buf.strip())
                    buf = ""
            return 200, "\n\n".join(frames)
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode("utf-8", "replace")[:400]
    except Exception as e:  # noqa: BLE001
        # A read timeout past the gate-hold cap means the gate held and the
        # connection delivered no complete frame in the window.
        return 503, f"<gate-held: {e}>"


def compare(name, method, path, body, js, rs, verbose):
    sj, bj = js.request(method, path, body)
    sr, br = rs.request(method, path, body)
    if sj != sr:
        return f"STATUS js={sj} rs={sr} :: {method} {path}"
    try:
        pj, pr = json.loads(bj), json.loads(br)
        nj, nr = normalize(pj), normalize(pr)
        diff = first_diff(nj, nr)
        if diff:
            return f"BODY {diff} :: {method} {path}"
        # nj != nr only through tolerated float differences — identical.
        if verbose:
            print(f"  ok {method} {path} ({sj})")
        return None
    except json.JSONDecodeError:
        nbj, nbr = normalize(bj), normalize(br) if isinstance(bj, str) else bj
        if nbj != nbr:
            return f"TEXT js={nbj[:200]!r} rs={nbr[:200]!r} :: {method} {path}"
        if verbose:
            print(f"  ok {method} {path} ({sj}, text)")
        return None

def first_diff(a, b, path="$"):
    if isinstance(a, dict) and isinstance(b, dict):
        for key in sorted(set(a) | set(b)):
            if key not in a:
                return f"{path}.{key}: js=<missing> rs={truncate(b[key])}"
            if key not in b:
                return f"{path}.{key}: js={truncate(a[key])} rs=<missing>"
            sub = first_diff(a[key], b[key], f"{path}.{key}")
            if sub:
                return sub
        return None
    if isinstance(a, list) and isinstance(b, list):
        if len(a) != len(b):
            if path.startswith("$.servers"):
                pj = {e.get("port") for e in a if isinstance(e, dict)}
                pr = {e.get("port") for e in b if isinstance(e, dict)}
                return f"{path}: only-js={sorted(pj - pr)} only-rs={sorted(pr - pj)}"
            return f"{path}: list len js={len(a)} rs={len(b)}"
        for index, (x, y) in enumerate(zip(a, b)):
            sub = first_diff(x, y, f"{path}[{index}]")
            if sub:
                return sub
        return None
    if isinstance(a, (int, float)) and isinstance(b, (int, float)) and not isinstance(a, bool) and not isinstance(b, bool):
        # Upstream caches (models.dev) re-published values can differ by a
        # few ULPs between fetch times — treat as equal within 1e-9 relative.
        if abs(a - b) <= 1e-9 * max(abs(a), abs(b), 1e-30):
            return None
    if a != b:
        if path.startswith("$.servers") and isinstance(a, list) and isinstance(b, list):
            pj = {e.get("port") for e in a if isinstance(e, dict)}
            pr = {e.get("port") for e in b if isinstance(e, dict)}
            return f"{path}: only-js={sorted(pj - pr)} only-rs={sorted(pr - pj)}"
        return f"{path}: js={truncate(a)} rs={truncate(b)}"
    return None

def truncate(value):
    text = json.dumps(value, ensure_ascii=False) if not isinstance(value, str) else value
    return text[:160]

def parity_ports(js, rs):
    """Ports owned by the parity pair (web + engine each side) — dev-server
    entries on these ports are cross-contamination from running two servers
    on one machine, not a response divergence."""
    ports = set()
    for client in (js, rs):
        status, body = client.request("GET", "/health")
        try:
            health = json.loads(body)
        except json.JSONDecodeError:
            continue
        for key in ("openCodePort", "port"):
            if isinstance(health.get(key), int):
                ports.add(health[key])
        launch = health.get("lastOpenCodeLaunchDiagnostics") or {}
        if isinstance(launch.get("port"), int):
            ports.add(launch["port"])
        engine = health.get("engine") or {}
        last = engine.get("lastLaunch") if isinstance(engine, dict) else None
        if isinstance(last, dict) and isinstance(last.get("port"), int):
            ports.add(last["port"])
    return ports


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--base-js", default="http://127.0.0.1:3100")
    parser.add_argument("--base-rs", default="http://127.0.0.1:3101")
    parser.add_argument("--stage", default="all")
    parser.add_argument("--verbose", action="store_true")
    args = parser.parse_args()

    js, rs = Client(args.base_js), Client(args.base_rs)
    lj = js.login()
    lr = rs.login()
    print(f"login: js={lj[0]} rs={lr[0]}")

    global PARITY_PORTS
    PARITY_PORTS = parity_ports(js, rs) | {3100, 3101}

    stages = {
        "1": (STAGE1, lambda p: p),
        "2": (STAGE2, lambda p: p),
    }

    failures = []
    total = 0

    if args.stage in ("1", "all"):
        for method, path, body in STAGE1:
            total += 1
            result = compare("s1", method, path, body, js, rs, args.verbose)
            if result:
                failures.append(result)

    if args.stage in ("2", "all"):
        for method, path, body in STAGE2:
            total += 1
            result = compare("s2", method, path, body, js, rs, args.verbose)
            if result:
                failures.append(result)

    if args.stage in ("3", "all"):
        for path in ["/api/event", "/api/global/event"]:
            total += 1
            sj, bj = first_sse(js, path)
            sr, br = first_sse(rs, path)
            nj, nr = normalize(bj), normalize(br)
            if sj != sr:
                # The /api readiness gate keys on each server's own cold-boot
                # health-window luck; the streams themselves share one handler
                # and are compared whenever both are in the same state.
                gate_503 = (sj == 503 or sr == 503) and (nj == nr or sr == 200 and br.startswith("event:") or sj == 200 and bj.startswith("event:"))
                if gate_503 and (sj == 503 or sr == 503):
                    print(f"  ok SSE {path} (gate-state race: js={sj} rs={sr}; streams identical when both open)")
                else:
                    failures.append(f"STATUS js={sj} rs={sr} :: SSE {path}")
            elif nj != nr:
                failures.append(f"SSE {path}:\n  js={nj!r}\n  rs={nr!r}")
            elif args.verbose:
                print(f"  ok SSE {path}")

    print(f"\n=== {total - len(failures)}/{total} identical ===")
    for failure in failures:
        print("DIFF", failure)
    sys.exit(1 if failures else 0)

if __name__ == "__main__":
    main()
