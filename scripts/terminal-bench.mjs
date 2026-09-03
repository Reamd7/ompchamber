#!/usr/bin/env node
// Terminal performance benchmark. Measures the terminal this script runs in.
//
//   bench:terminal  (bun run bench:terminal / node scripts/terminal-bench.mjs)
//
// Two tests, two different render paths:
//
//   cat test   Stream a plain-text fixture (default 500 MiB, 80 bytes/line)
//              to stdout and time it. Pty flow control blocks the writer once
//              the terminal falls behind, so at 500 MiB the wall time is the
//              terminal's scroll path end to end (parse, grid damage,
//              scrollback store, paint) plus at most a few hundred KiB of
//              pipe slack. Score: MiB/s and Mlines/s, best of N reps.
//
//   fire test  Build const-void/DOOM-fire-zig with -Doptimize=ReleaseFast and
//              run it full screen. It repaints the entire grid in 256-color
//              ANSI every frame and prints a cumulative "[ N.NN fps ]" counter
//              on its bottom line. The operator watches until the number
//              settles, hits Ctrl+C, and types the value back. Score: avg fps,
//              graded very poor <=5 / poor <=24 / ok / great >=100 (upstream
//              definitions). A Debug build would benchmark the compiler's
//              codegen, not the terminal; ReleaseFast is not optional.
//
// One JSON record per run appends to artifacts/term-bench/results.jsonl.
// Compare records only across the same machine, OS, window size and font.
//
// Runs on Windows (cmd, PowerShell, Windows Terminal, Git Bash), macOS and
// Linux. When the local zig does not match the DOOM-fire ref's .zigversion
// pin, the pinned zig is fetched into the cache so every machine builds the
// same tested combination.

import fs from "node:fs"
import path from "node:path"
import os from "node:os"
import { spawnSync, spawn } from "node:child_process"
import { fileURLToPath } from "node:url"
import { parseArgs } from "node:util"
import readline from "node:readline/promises"

const IS_WIN = process.platform === "win32"
const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..")
const OUT_DIR = path.join(REPO_ROOT, "artifacts", "term-bench")
const CACHE_DIR = path.join(OUT_DIR, "cache")
const RESULTS = path.join(OUT_DIR, "results.jsonl")
const FIRE_URL = "https://github.com/const-void/DOOM-fire-zig"
const FIRE_DIR = path.join(CACHE_DIR, "doom-fire-zig")

function usage() {
  console.log(`usage: node scripts/terminal-bench.mjs [options]

Runs INSIDE the terminal being measured.

  --label NAME      name of the terminal under test, recorded with the results
  --reps N          cat-test repetitions (default 3; best rep is the score)
  --mib N           fixture size in MiB (default 500)
  --min-watch SECS  advised fire watch time before Ctrl+C (default 45)
  --fire-ref REF    DOOM-fire-zig git ref (default: branch matching the zig
                    used, else master)
  --results FILE    JSONL output (default artifacts/term-bench/results.jsonl)
  --skip-cat        skip the throughput test
  --skip-fire       skip the DOOM-fire test (also skips clone/build)
  --rebuild-fire    force a rebuild of DOOM-fire
  --force           allow running without a tty on stdout (mechanics check
                    only; the numbers then measure a pipe, not a terminal)
  -h, --help        this help`)
  process.exit(0)
}

const args = parseArgs({
  options: {
    label: { type: "string" },
    reps: { type: "string" },
    mib: { type: "string" },
    "min-watch": { type: "string" },
    "fire-ref": { type: "string" },
    results: { type: "string" },
    "skip-cat": { type: "boolean" },
    "skip-fire": { type: "boolean" },
    "rebuild-fire": { type: "boolean" },
    force: { type: "boolean" },
    help: { type: "boolean", short: "h" },
  },
}).values

const LABEL = args.label ?? ""
const MIB = Number(args.mib ?? 500)
const REPS = Number(args.reps ?? 3)
const MIN_WATCH = Number(args["min-watch"] ?? 45)
const FIRE_REF = args["fire-ref"] ?? "auto"
const RESULTS_PATH = args.results ?? RESULTS
const SKIP_CAT = args["skip-cat"] === true
const SKIP_FIRE = args["skip-fire"] === true
const REBUILD = args["rebuild-fire"] === true
const FORCE = args.force === true
if (args.help) usage()

const notes = []
function die(msg) {
  console.error(`terminal-bench: error: ${msg}`)
  process.exit(1)
}
function note(msg) {
  notes.push(msg)
  console.error(`terminal-bench: note: ${msg}`)
}
function isPositiveInt(v, name) {
  return Number.isInteger(v) && v >= 1 ? null : `--${name} must be a positive integer (got '${v}')`
}
for (const [v, n] of [[REPS, "reps"], [MIB, "mib"]]) {
  const err = isPositiveInt(v, n)
  if (err) die(err)
}
if (!Number.isFinite(MIN_WATCH) || MIN_WATCH <= 0) die(`--min-watch must be a positive number (got '${MIN_WATCH}')`)

// git here targets the DOOM-fire clone, never the caller's repo; inherited
// GIT_* discovery variables (e.g. from a worktree) would break it.
const GIT_ENV = Object.fromEntries(Object.entries(process.env).filter(([k]) => !k.startsWith("GIT_")))

function run(cmd, cmdArgs, opts = {}) {
  return spawnSync(cmd, cmdArgs, { encoding: "utf8", ...opts })
}

function guessTerminal() {
  if (process.env.WT_SESSION) return "Windows Terminal"
  if (process.env.TERM_PROGRAM) return process.env.TERM_PROGRAM
  return process.env.TERM || "unknown"
}

function termSize() {
  return { cols: process.stdout.columns ?? 0, rows: process.stdout.rows ?? 0 }
}

function gradeFor(fps) {
  if (fps <= 5) return "very poor"
  if (fps <= 24) return "poor"
  if (fps < 100) return "ok"
  return "great"
}

// --- cat test -----------------------------------------------------------------

async function runCatTest() {
  // 80 bytes per line; round the target down to whole lines (1 MiB is not a
  // multiple of 80; 500 MiB is, so the default fixture is exactly 500 MiB).
  const lines = Math.floor((MIB * 1048576) / 80)
  const bytes = lines * 80
  const file = path.join(CACHE_DIR, `cat-${MIB}.txt`)

  let needGen = true
  try {
    needGen = fs.statSync(file).size !== bytes
  } catch {
    // missing -> generate
  }
  if (needGen) {
    console.error(`terminal-bench: generating ${MIB} MiB fixture once (cached at ${file})`)
    fs.mkdirSync(CACHE_DIR, { recursive: true })
    const tmp = `${file}.tmp`
    const fd = fs.openSync(tmp, "w")
    const CHUNK = 100_000
    let batch = []
    for (let i = 1; i <= lines; i++) {
      batch.push(String(i).padStart(79, "0"))
      if (batch.length === CHUNK) {
        fs.writeSync(fd, batch.join("\n") + "\n")
        batch = []
      }
    }
    if (batch.length) fs.writeSync(fd, batch.join("\n") + "\n")
    fs.closeSync(fd)
    if (fs.statSync(tmp).size !== bytes) die("fixture generation produced the wrong size")
    fs.renameSync(tmp, file)
  }

  const size = termSize()
  console.log(`\n[cat test] ${MIB} MiB / ${lines} lines / ${REPS} reps at ${size.cols}x${size.rows} -- leave the window alone\n`)

  const timesS = []
  const buf = Buffer.alloc(1024 * 1024)
  for (let rep = 1; rep <= REPS; rep++) {
    if (process.stdout.isTTY) process.stdout.write("\x1b[3J\x1b[H\x1b[2J") // wipe screen + scrollback
    await new Promise((r) => setTimeout(r, 400)) // let the previous frame drain
    const fd = fs.openSync(file, "r")
    const t0 = performance.now()
    for (;;) {
      const n = fs.readSync(fd, buf, 0, buf.length, null)
      if (n === 0) break
      let off = 0
      while (off < n) off += fs.writeSync(1, buf, off, n - off) // blocks on pty backpressure
    }
    const t1 = performance.now()
    fs.closeSync(fd)
    const s = (t1 - t0) / 1000
    timesS.push(s)
    console.log(
      `  rep ${rep}/${REPS}  ${s.toFixed(2).padStart(8)} s  ${(MIB / s).toFixed(1).padStart(9)} MiB/s  ${(lines / 1e6 / s).toFixed(2).padStart(8)} Mlines/s`,
    )
  }

  const sorted = [...timesS].sort((a, b) => a - b)
  const best = sorted[0]
  const median = sorted[Math.floor(sorted.length / 2)]
  console.log(
    `\n  cat score: best ${(MIB / best).toFixed(1)} MiB/s (${(lines / 1e6 / best).toFixed(2)} Mlines/s), median rep ${(MIB / median).toFixed(1)} MiB/s`,
  )
  return { mib: MIB, lines, reps: REPS, times_s: timesS.map((t) => Number(t.toFixed(3))), best_s: Number(best.toFixed(3)), best_mib_s: Number((MIB / best).toFixed(1)), best_mlines_s: Number((lines / 1e6 / best).toFixed(2)), cols: size.cols, rows: size.rows }
}

// --- fire test ------------------------------------------------------------------

function resolveLocalZig() {
  for (const name of ["zig", "zig.exe"]) {
    const r = run(name, ["version"])
    if (r.status === 0 && r.stdout.trim()) return { bin: name, version: r.stdout.trim() }
  }
  return null
}

function pickFireRef(zigVersion) {
  const mm = zigVersion.includes(".") ? zigVersion.split(".").slice(0, 2).join(".") : zigVersion
  const r = run("git", ["ls-remote", "--heads", FIRE_URL, `refs/heads/zig-${mm}`], { env: GIT_ENV })
  return r.status === 0 && r.stdout.trim() ? `zig-${mm}` : "master"
}

function cloneFire(ref) {
  const marker = path.join(FIRE_DIR, ".term-bench-ref")
  let current = ""
  try {
    current = fs.readFileSync(marker, "utf8").trim()
  } catch {
    // absent -> clone
  }
  if (!fs.existsSync(path.join(FIRE_DIR, ".git")) || current !== ref) {
    fs.rmSync(FIRE_DIR, { recursive: true, force: true })
    console.error(`terminal-bench: cloning ${FIRE_URL} (ref ${ref})`)
    fs.mkdirSync(CACHE_DIR, { recursive: true })
    const r = run("git", ["clone", "--quiet", "--depth", "1", "--branch", ref, FIRE_URL, FIRE_DIR], { env: GIT_ENV })
    if (r.status !== 0) die(`clone failed: ${(r.stderr || "").trim()}`)
    fs.writeFileSync(marker, `${ref}\n`)
  }
  const rev = run("git", ["-C", FIRE_DIR, "rev-parse", "--short", "HEAD"], { env: GIT_ENV })
  return rev.status === 0 ? rev.stdout.trim() : "unknown"
}

function platTag() {
  if (IS_WIN) return "windows"
  if (process.platform === "darwin") return "macos"
  return "linux"
}
function archTag() {
  return process.arch === "arm64" ? "aarch64" : "x86_64"
}

async function download(url, dest) {
  // curl first: it streams to disk and honors flaky links better than an
  // in-memory fetch. --speed-* aborts transfers that stall mid-stream;
  const curlArgs = [
    "-fsSL",
    "--connect-timeout", "15",
    "--speed-time", "30",
    "--speed-limit", "10240",
    "--max-time", "240",
    "-o", dest,
    url,
  ]
  for (let attempt = 0; attempt < 3; attempt++) {
    const curl = run("curl", curlArgs, { stdio: "ignore" })
    if (curl.status === 0 && fs.existsSync(dest) && fs.statSync(dest).size > 1024) return true
    const wget = run("wget", ["-q", "--timeout=60", "--tries=2", "-O", dest, url], { stdio: "ignore" })
    if (wget.status === 0 && fs.existsSync(dest) && fs.statSync(dest).size > 1024) return true
    try {
      fs.rmSync(dest, { force: true })
    } catch {
      // best effort
    }
  }
  try {
    const res = await fetch(url, { signal: AbortSignal.timeout(300000) })
    if (!res.ok) return false
    const buf = Buffer.from(await res.arrayBuffer())
    if (buf.length < 1024) return false
    fs.writeFileSync(dest, buf)
    return true
  } catch {
    return false
  }
}

// Echo the path to a cached zig matching the DOOM-fire pin ("0.14"), fetching
// it from ziglang.org on first use. Tries newer patch releases of the pin
// first; both historical archive name orders are attempted. The cache is
// keyed by version+platform+arch so Windows and WSL runs do not clobber each
// other.
async function pinnedZig(pin) {
  const plat = platTag()
  const arch = archTag()
  const ext = IS_WIN ? "zip" : "tar.xz"
  const candidates = pin.split(".").length === 2 ? [`${pin}.1`, `${pin}.0`, pin] : [pin]
  for (const ver of candidates) {
    const dir = path.join(CACHE_DIR, "toolchains", `zig-${ver}-${plat}-${arch}`)
    const okBin = path.join(dir, IS_WIN ? "zig.exe" : "zig")
    const usable = () => fs.existsSync(okBin) && run(okBin, ["version"]).status === 0
    if (usable()) return okBin

    console.error(`terminal-bench: fetching pinned zig ${ver} into the cache (one-time)`)
    const tmpDir = path.join(CACHE_DIR, "tmp")
    fs.mkdirSync(path.join(CACHE_DIR, "toolchains"), { recursive: true })
    fs.mkdirSync(tmpDir, { recursive: true })
    const archive = path.join(tmpDir, `zig-dl-${ver}.${ext}`)
    const names = [`zig-${arch}-${plat}-${ver}.${ext}`, `zig-${plat}-${arch}-${ver}.${ext}`]
    let downloaded = false
    for (const name of names) {
      if (await download(`https://ziglang.org/download/${ver}/${name}`, archive)) {
        downloaded = true
        break
      }
    }
    if (!downloaded) continue

    fs.rmSync(dir, { recursive: true, force: true })
    const extractDir = path.join(tmpDir, `extract-${ver}`)
    fs.rmSync(extractDir, { recursive: true, force: true })
    fs.mkdirSync(extractDir, { recursive: true })
    let extracted = false
    if (IS_WIN) {
      const r = run("powershell.exe", [
        "-NoProfile",
        "-Command",
        `Expand-Archive -LiteralPath '${archive}' -DestinationPath '${extractDir}' -Force`,
      ])
      extracted = r.status === 0
    } else {
      extracted = run("tar", ["-xf", archive, "-C", extractDir]).status === 0
    }
    fs.rmSync(archive, { force: true })
    if (extracted) {
      const inner = fs.readdirSync(extractDir).find((e) => e.startsWith("zig-") && e.endsWith(ver))
      if (inner) {
        try {
          fs.renameSync(path.join(extractDir, inner), dir)
        } catch {
          extracted = false
        }
      } else {
        extracted = false
      }
    }
    fs.rmSync(extractDir, { recursive: true, force: true })
    if (extracted && usable()) return okBin
    fs.rmSync(dir, { recursive: true, force: true })
  }
  return null
}

async function ensureFire() {
  if (run("git", ["--version"]).status !== 0) die("fire test needs git")
  const local = resolveLocalZig()

  // Pass 1: pick a ref with the local zig (or the explicit --fire-ref).
  const ref1 = FIRE_REF === "auto" ? pickFireRef(local ? local.version : "none") : FIRE_REF
  let refUsed = ref1
  let rev = cloneFire(refUsed)

  // Pass 2: if the tree pins a different zig, switch to a cached pinned
  // toolchain and re-pick the ref, so the build matches what upstream tested.
  let pin = ""
  try {
    pin = fs.readFileSync(path.join(FIRE_DIR, ".zigversion"), "utf8").trim()
  } catch {
    // absent -> no pin
  }
  let zigBin = local ? local.bin : ""
  const localMm = local ? local.version.split(".").slice(0, 2).join(".") : null
  if (pin && pin !== localMm) {
    const okBin = await pinnedZig(pin)
    if (okBin) {
      zigBin = okBin
      note(`using pinned zig ${pin} for DOOM-fire (local zig: ${local ? local.version : "none"})`)
      if (FIRE_REF === "auto") {
        const zv = run(zigBin, ["version"]).stdout.trim()
        const ref2 = pickFireRef(zv)
        if (ref2 !== refUsed) {
          refUsed = ref2
          rev = cloneFire(ref2)
        }
      }
    } else {
      note("could not fetch the pinned zig (curl/wget honor https_proxy; set it if your link to ziglang.org is slow or blocked); building with the local zig, which will likely fail")
    }
  }
  if (!zigBin) die("fire test needs zig (or zig.exe) on PATH, or network to fetch the pinned zig (https://ziglang.org/download/)")

  const zigVersion = run(zigBin, ["version"]).stdout.trim()
  const fireExe = zigBin.endsWith(".exe") || IS_WIN ? ".exe" : ""
  const fireBin = path.join(FIRE_DIR, "zig-out", "bin", `DOOM-fire${fireExe}`)
  const tag = `ReleaseFast ${zigVersion} ref ${refUsed} rev ${rev}`
  const builtMarker = path.join(FIRE_DIR, ".term-bench-built")
  let builtTag = ""
  try {
    builtTag = fs.readFileSync(builtMarker, "utf8").trim()
  } catch {
    // absent -> build
  }
  if (REBUILD || !fs.existsSync(fireBin) || builtTag !== tag) {
    console.error(`terminal-bench: building DOOM-fire (${tag})`)
    fs.mkdirSync(CACHE_DIR, { recursive: true })
    const r = run(zigBin, ["build", "-Doptimize=ReleaseFast"], { cwd: FIRE_DIR })
    const log = path.join(CACHE_DIR, "fire-build.log")
    fs.writeFileSync(log, `${r.stdout ?? ""}\n${r.stderr ?? ""}`)
    if (r.status !== 0) {
      console.error((r.stderr || r.stdout || "").split("\n").slice(0, 40).join("\n"))
      die(`zig build failed (full log: ${log}); tried zig ${zigVersion} on ref ${refUsed} (pins ${pin || "?"})`)
    }
    fs.writeFileSync(builtMarker, `${tag}\n`)
  }
  if (!fs.existsSync(fireBin)) die(`expected DOOM-fire binary at ${fireBin}`)
  return { zigBin, zigVersion, refUsed, rev, fireBin }
}

async function runFireTest(prep) {
  if (!process.stdin.isTTY || !process.stdout.isTTY) die("the fire test is interactive; run it inside the terminal being measured")

  const rl = readline.createInterface({ input: process.stdin, output: process.stdout })
  console.log(`\n[fire test] DOOM-fire-zig ${prep.refUsed} @ ${prep.rev}, zig ${prep.zigVersion}, ReleaseFast\n`)
  console.log("  1. MAXIMIZE this window now -- fps scales with cell count.")
  console.log("  2. A color test and a marquee play first (~10 s), then the fire starts.")
  console.log('  3. The bottom line shows a cumulative "[ N.NN fps ]" counter.')
  console.log(`  4. Watch at least ${MIN_WATCH} s, until the number stops moving much.`)
  console.log("  5. Stop the fire (Ctrl+C in a desktop terminal; in OpenChamber's")
  console.log("     embedded terminal Ctrl+C arrives as a raw byte and will not stop")
  console.log("     it — close/restart the terminal tab instead), then type the")
  console.log("     number you last saw.\n")
  await rl.question("Press Enter when maximized... ")
  rl.close()

  const size = termSize()
  if (size.cols < 120 || size.rows < 22) {
    die(`fire test needs a window of at least 120x22; have ${size.cols}x${size.rows}. Maximize and rerun.`)
  }
  if (process.stdout.isTTY) process.stdout.write("\x1b[3J")

  // Ctrl+C must kill DOOM-fire, not this script; the pre-fed newlines answer
  // its "press return" pauses. A raw 0x03 byte in stdin (how Ctrl+C arrives
  // through a terminal websocket) makes readline close itself, so the fps
  // question below gets a freshly created reader.
  const ignoreInt = () => {}
  process.on("SIGINT", ignoreInt)
  await new Promise((resolve) => {
    const child = spawn(prep.fireBin, [], { stdio: ["pipe", "inherit", "inherit"] })
    child.stdin.end("\n\n")
    child.on("close", resolve)
  })
  process.removeListener("SIGINT", ignoreInt)

  process.stdout.write("\r\n") // keep the fps line visible above the prompt
  process.stdout.write("fps from the line above: ")
  // Read the answer with a raw stdin reader: a stray 0x03 byte (how Ctrl+C
  // arrives through a terminal websocket) closes any readline instance with
  // ERR_USE_AFTER_CLOSE before question() can run; in raw mode it is data.
  const readAnswer = () => new Promise((resolve) => {
    if (!process.stdin.isTTY) { resolve(""); return; }
    process.stdin.setRawMode(true);
    process.stdin.resume();
    let acc = "";
    const cleanup = () => { process.stdin.removeListener("data", onData); process.stdin.pause(); process.stdin.setRawMode(false); };
    const onData = (chunk) => {
      for (const byte of chunk) {
        if (byte === 0x0d || byte === 0x0a || byte === 0x03) { cleanup(); process.stdout.write("\r\n"); resolve(acc.trim()); return; }
        if (byte === 0x08) { acc = acc.slice(0, -1); process.stdout.write("\b \b"); continue; }
        const ch = String.fromCharCode(byte);
        if (ch >= " ") { acc += ch; process.stdout.write(ch); }
      }
    };
    process.stdin.on("data", onData);
  });
  const raw = await readAnswer()

  process.stdout.write("\x1b[?1049l\x1b[?25h\x1b[0m\r\n") // fire dies on Ctrl+C before restoring these
  clearWindowsConsoleMode()

  const fps = Number(raw)
  if (raw !== "" && Number.isFinite(fps) && fps >= 0) {
    return { fps, grade: gradeFor(fps), cols: size.cols, rows: size.rows }
  }
  note(`fire fps not recorded (operator entered: '${raw || "nothing"}')`)
  return { fps: null, grade: null, cols: size.cols, rows: size.rows }
}

// DOOM-fire sets DISABLE_NEWLINE_AUTO_RETURN on the console and dies on
// Ctrl+C before restoring it, leaving the shell prompt staircased. Clear that
// bit (keep VT processing). Best effort, Windows only.
function clearWindowsConsoleMode() {
  if (!IS_WIN) return
  const ps =
    "Add-Type -MemberDefinition " +
    "'[DllImport(\"kernel32.dll\")] public static extern bool SetConsoleMode(IntPtr h, uint m); " +
    '[DllImport("kernel32.dll")] public static extern bool GetConsoleMode(IntPtr h, out uint m);\' ' +
    "-Name K -Namespace W; " +
    "$m = 0; [W.K]::GetConsoleMode([W.K]::GetStdHandle(-11), [ref]$m) | Out-Null; " +
    "[W.K]::SetConsoleMode([W.K]::GetStdHandle(-11), ($m -band (-bnot 8))) | Out-Null"
  run("powershell.exe", ["-NoProfile", "-Command", ps], { stdio: "ignore" })
}

// --- record ------------------------------------------------------------------------

function writeRecord(cat, fire, envSize, label) {
  fs.mkdirSync(path.dirname(RESULTS_PATH), { recursive: true })
  fs.mkdirSync(CACHE_DIR, { recursive: true })
  const record = {
    ts: new Date().toISOString(),
    label,
    terminal_guess: guessTerminal(),
    term: process.env.TERM || "",
    term_program: process.env.TERM_PROGRAM || "",
    term_program_version: process.env.TERM_PROGRAM_VERSION || "",
    colorterm: process.env.COLORTERM || "",
    wt_session: Boolean(process.env.WT_SESSION),
    cols: cat ? cat.cols : envSize.cols,
    rows: cat ? cat.rows : envSize.rows,
    os: `${os.type()} ${os.release()} ${os.arch()}`,
    shell: `node ${process.version}`,
    cat,
    fire,
    notes,
  }
  fs.appendFileSync(RESULTS_PATH, `${JSON.stringify(record)}\n`)

  console.log("\n=== summary ===")
  console.log(`terminal   ${record.label}`)
  if (cat) console.log(`cat        ${cat.best_mib_s.toFixed(1)} MiB/s, ${cat.best_mlines_s.toFixed(2)} Mlines/s (at ${cat.cols}x${cat.rows})`)
  else console.log("cat        skipped")
  if (fire && fire.fps != null) console.log(`fire       ${fire.fps} fps (${fire.grade}, at ${fire.cols}x${fire.rows})`)
  else if (fire) console.log("fire       not recorded")
  else console.log("fire       skipped")
  console.log(`recorded   ${RESULTS_PATH}`)
  console.log("compare only runs with the same machine, OS, window size and font;")
  console.log("scrollback settings are not recorded -- keep them identical too.")
}

// --- main ---------------------------------------------------------------------------

async function main() {
  if (!SKIP_CAT && !process.stdout.isTTY && !FORCE) {
    die("stdout is not a terminal; the cat test would measure a pipe, not your terminal. Run this inside the terminal under test (or --force for a mechanics check).")
  }
  if (!process.stdout.isTTY) note("running without a tty on stdout; cat-test numbers do not describe a terminal")

  let label = LABEL
  if (!label && process.stdin.isTTY) {
    const rl = readline.createInterface({ input: process.stdin, output: process.stdout })
    label = (await rl.question(`Label for the terminal under test [${guessTerminal()}]: `)).trim()
    rl.close()
  }

  const cat = SKIP_CAT ? null : await runCatTest()
  const fire = SKIP_FIRE ? null : await runFireTest(await ensureFire())
  writeRecord(cat, fire, termSize(), label || guessTerminal())
}

await main()
