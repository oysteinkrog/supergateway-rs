# Spin-Poll CPU Bug Postmortem

This document captures the `/sysperf` session that diagnosed and root-caused
the spin-poll CPU regression in supergateway-rs stateful HTTP sessions (2026-05-28).
The fix landed in commit c8837d4 (`fix(stateful_http,stateless_http): eliminate relay + handler spin-polls`);
see the **Resolution** section below for details.

## Resolution

The fix was committed as **c8837d4** (`fix(stateful_http,stateless_http): eliminate relay + handler spin-polls`).

`stateful_http.rs:919-920` (and the equivalent in `stateless_http.rs`) were rewritten
to replace the blocking spin-poll relay thread with an async task on the asupersync
runtime, matching the approach already used in `sse.rs` and `ws.rs`.
Idle sessions now cost ~0 CPU instead of ~1000 wakeups/sec per session.

---

## Original /sysperf Session

❯ /sysperf

● Bash(echo "=== Load ===" && uptime && echo "=== Cores ===" && nproc && echo
      "=== Memory ===" && free -h && echo "=== Swap ===" && (swapon --show
      2>/dev/null || echo…)
  ⎿  === Load ===
      10:55:07 up 53 min,  0 user,  load average: 0.52, 0.58, 0.59
     === Cores ===
     … +9 lines (ctrl+o to expand)

● Bash(echo "=== TMUX SESSIONS ===" && tmux list-sessions 2>/dev/null || echo
      "No tmux sessions"
      echo ""…)
  ⎿  === TMUX SESSIONS ===
     No tmux sessions

● Bash(echo "=== TOP CPU ===" && ps aux --sort=-%cpu | head -40)
  ⎿  === TOP CPU ===
     USER       PID %CPU %MEM    VSZ   RSS TTY      STAT START   TIME COMMA
     ND
     … +426 lines (ctrl+o to expand)

● Bash(echo "=== LONG-RUNNING PROCESSES (by elapsed time) ===" && ps -eo
      pid,ppid,user,%cpu,%mem,etime,args --sort=-etime | head -60)
  ⎿  === LONG-RUNNING PROCESSES (by elapsed time) ===
       PID  PPID USER     %CPU %MEM     ELAPSED COMMAND
     29597 29573 oystein   0.0  0.0 441077234-00:18:40 ps -eo pid,ppid,user
     … +59 lines (ctrl+o to expand)

● Bash(echo "=== ZOMBIES ===" && ps aux | awk '$8 ~ /Z/ {print}' | head -40 ||
      echo "(no zombies)")
  ⎿  === ZOMBIES ===

● Bash(echo "=== TOP MEM ===" && ps aux --sort=-%mem | head -30)
  ⎿  === TOP MEM ===
     USER       PID %CPU %MEM    VSZ   RSS TTY      STAT START   TIME COMMA
     ND
     … +426 lines (ctrl+o to expand)

● Bash(echo "=== fff-mcp instances ===" && ps -eo
      pid,ppid,%cpu,%mem,rss,etime,tty,args | grep -E '(fff-mcp|claude)' | grep
      -v grep | head -40
      echo ""…)
  ⎿  Error: Search tool steering:
       - `grep` is slower and worse-defaulted than `rg`.
       - `grep` is slower and worse-defaulted than `rg`.
       - `grep` is slower and worse-defaulted than `rg`.
       - `grep` is slower and worse-defaulted than `rg`.
       - `grep` is slower and worse-defaulted than `rg`.
       - `grep` is slower and worse-defaulted than `rg`.

     Prefer the fff MCP tools (frecency-ranked, git-aware) — `mcp__fff__ffgrep`
     for content search, `mcp__fff__fffind` for paths. Fall back to `rg
     <pattern> [path]` / `fd <pattern>` when fff doesn't fit.

     … +1 line (ctrl+o to expand)

● Bash(echo "=== fff-mcp instances ===" && ps -eo
      pid,ppid,%cpu,%mem,rss,etime,tty,args | rg '(fff-mcp|claude
      --dangerously)' | rg -v 'rg ' | head -50
      echo ""…)
  ⎿  === fff-mcp instances ===
      2072  1998 23.8  0.5 368288      52:43 tty4     /c/users/oystein/.loc
     al/bin/fff-mcp
     … +34 lines (ctrl+o to expand)

● Bash(echo "=== TOTAL RSS by command ===" && ps -eo rss,comm --no-headers | awk
       '{a[$2]+=$1} END {for (k in a) print a[k], k}' | sort -rn | head -20
      echo ""…)
  ⎿  === TOTAL RSS by command ===
     5391236 ld-linux-x86-64
     3723124 fff-mcp
     … +31 lines (ctrl+o to expand)

● Bash(echo "=== Stuck bash shell snapshots ===" && ps -eo pid,ppid,etime,args |
       rg 'shell-snapshot' | rg -v 'rg ' | head -20
      echo ""…)
  ⎿  === Stuck bash shell snapshots ===
     16274  8653       09:24 /bin/bash -c source /c/users/oystein/.claude/s
     hell-snapshots/snapshot-bash-1779956298701-99bf2j.sh 2>/dev/null || tr
     … +426 lines (ctrl+o to expand)

● Bash(echo "=== ALL long shell-snapshots ===" && ps -eo pid,ppid,etime,args |
      rg 'shell-snapshot' | rg -v 'rg shell-snapshot' | awk '{print}' | head
      -30)
  ⎿  === ALL long shell-snapshots ===
     21135  7133       38:43 /bin/bash -c source /c/users/oystein/.claude/s
     hell-snapshots/snapshot-bash-1779956269704-gk9zvt.sh 2>/dev/null || tr
     … +65 lines (ctrl+o to expand)

● Diagnosis Summary

  Load: 0.52 / 32 cores (1.6% — not overloaded)
  Memory: 61 GiB / 63 GiB used, 2.8 GiB free, 3.7 GiB swap (tight)
  Uptime: 53 min (nothing has had time to go truly stale)
  Zombies: 0
  Tmux sessions: 0

  What I found

  This isn't a stuck-process problem. The machine is busy with legitimate
  concurrent agent work and there's nothing safe to harvest. Inventory:

  ┌───────────────────────────────────────┬───────┬────────┬────────────────┐
  │             Process group             │ Count │ Memory │     Status     │
  ├───────────────────────────────────────┼───────┼────────┼────────────────┤
  │                                       │       │        │ PROTECTED —    │
  │ claude agents (across ttys)           │ 18    │ 5.6 GB │ your active    │
  │                                       │       │        │ coding agents  │
  ├───────────────────────────────────────┼───────┼────────┼────────────────┤
  │                                       │       │        │ PROTECTED —    │
  │ fff-mcp (one per claude session)      │ 16    │ 3.6 GB │ child of each  │
  │                                       │       │        │ agent          │
  ├───────────────────────────────────────┼───────┼────────┼────────────────┤
  │ supergateway-rs MCP proxies           │       │        │ PROTECTED —    │
  │ (slack/sentry/hubspot/zendesk/apify,  │ 5     │ 34 MB  │ under PM2,     │
  │ ports 4803-4807)                      │       │        │ used by        │
  │                                       │       │        │ sessions       │
  ├───────────────────────────────────────┼───────┼────────┼────────────────┤
  │                                       │       │        │ PROTECTED —    │
  │ pm2 daemons                           │ 4     │ 727 MB │ managing the   │
  │                                       │       │        │ supergateways  │
  ├───────────────────────────────────────┼───────┼────────┼────────────────┤
  │ node/npm (MCP servers under           │       │        │ PROTECTED —    │
  │ supergateway)                         │ ~10   │ 5.9 GB │ wrapped MCP    │
  │                                       │       │        │ servers        │
  └───────────────────────────────────────┴───────┴────────┴────────────────┘

  Long-running bash shell-snapshots (the things that look stuck at first glance)
   all trace back to a live claude parent doing real work:

  - tail -F on test/translate logs (background monitors paired with active runs)
  - gh pr checks polling loop (agent waiting on CI)
  - python3 translate.py bulk (active translation job)
  - cargo build -p atlas-server (active compile)
  - measurex_demo.exe on Windows (active app run)
  - cmd.exe /c "build.cmd test" (active .NET test run)

  None of these are orphans. No zombies. No kill-able barnacles.

  The actual situation

  - CPU is fine. Load 0.52 on 32 cores.
  - Memory is tight but not from waste. Sum of process RSS is only ~17 GB, but
  WSL2 reports 61 GB used — that's the WSL2 VM holding allocated pages it hasn't
   returned. 3.7 GB of swap is in use as a result.

  Options if memory pressure is biting you

  1. Close idle claude tabs — each idle session still costs ~300-400 MB (claude
  + fff-mcp pair). I can list which ttys are idlest if you want.
  2. Shrink the WSL VM — edit ~/.wslconfig memory= or run wsl --shutdown from
  Windows to fully reclaim.
  3. Leave it alone — load is fine, swap is doing its job, the agents are
  working.

  No kill recommendation. Everything visible is doing legitimate work, and the
  safety rules forbid killing active coding agents. Tell me which path you want
  and I'll act.

✻ Cooked for 3m 15s

❯ is fff "thread-safe"? can we have a single instance user supergateway? check
  source code (clone into /c/work/)

● Bash(file ~/.local/bin/fff-mcp 2>/dev/null && ~/.local/bin/fff-mcp --version
      2>&1 | head -5; echo "---"; ~/.local/bin/fff-mcp --help 2>&1 | head -40)
  ⎿  /c/users/oystein/.local/bin/fff-mcp: ELF 64-bit LSB pie executable, x8
     6-64, version 1 (SYSV), static-pie linked, stripped
     fff-mcp 0.8.4 (030a583b37802416ce9108295b209d2e6b9d0079)
     … +33 lines (ctrl+o to expand)

● Bash(curl -sL https://dmtrkovalenko.dev/install-fff-mcp.sh | head -60)
  ⎿  #!/usr/bin/env bash
     set -eo pipefail
     … +57 lines (ctrl+o to expand)
● Bash(ls /c/work/ | head -20; echo "---"; [ -d /c/work/fff.nvim ] && echo
      "Already cloned" || (cd /c/work && git clone --depth 1
      https://github.com/dmtrKovalenko/fff.…)
  ⎿  AI
     ANALYSIS_MANIFEST.txt
     ArchUnitNET
     … +39 lines (ctrl+o to expand)

● Bash(cd /c/work/fff.nvim && ls && echo "---MCP DIR---" && find . -type d -name
       '*mcp*' -o -name 'crates' | head)
  ⎿  Error: Search tool steering:
       - `find -name/-path/-regex` is slower than `fd`.

     Prefer the fff MCP tools (frecency-ranked, git-aware) — `mcp__fff__ffgrep`
     for content search, `mcp__fff__fffind` for paths. Fall back to `rg
     <pattern> [path]` / `fd <pattern>` when fff doesn't fit.

     Re-run with the suggested tool. (See ~/.claude/hooks/rewrite-search.py for
     the exact rules.)

● Bash(cd /c/work/fff.nvim && ls && echo "---" && fd -t d mcp | head)
  ⎿  AGENTS.md
     CLAUDE.md
     Cargo.lock
     … +26 lines (ctrl+o to expand)
  ⎿  Shell cwd was reset to /c/users/oystein

  Searched for 2 patterns, read 3 files, listed 1 directory (ctrl+o to expand)

● Answer

  Thread-safe: yes. Single shared instance across all sessions: no, not without
  a code change.

  Thread safety — confirmed by source

  crates/fff-core/src/lib.rs:23 documents it directly:

  ▎ SharedFilePicker, SharedFrecency, and SharedQueryTracker are newtype
  ▎ wrappers around Arc<RwLock<Option<T>>> for thread-safe shared access.

  In fff-mcp/src/server.rs:
  - FffServer { picker: SharedFilePicker, frecency: SharedFrecency,
  cursor_store: Arc<Mutex<CursorStore>>, … } (server.rs:181)
  - Every tool acquires self.picker.read() (read-only lock) before searching
  (server.rs:253)
  - Runtime is #[tokio::main] with tokio = full — multi-threaded
  - grep/find_files/multi_grep all take the shared read lock — concurrent calls
  run in parallel since the picker is never write-locked during a query (writes
  only happen from the background scanner/watcher)

  Multiple concurrent MCP calls hitting one fff-mcp process are safe.

  Why one shared instance across all sessions won't work today

  The base path is fixed at process startup and there's no per-call override:

  1. main.rs:208-233 — base_path = args.base_path.unwrap_or(cwd), then
  Repository::discover() rebases it to the single git root. That root is baked
  into FilePicker::new_with_shared_state(...) (main.rs:260-275).
  2. The watcher (background_watcher), scanner (scan), mmap cache, and content
  index are all rooted at that one base path.
  3. The three MCP tools FindFilesParams / GrepParams / MultiGrepParams
  (server.rs:85, 100, 164) have only query, maxResults, cursor, output_mode,
  constraints, context — no cwd / root / project field. There is no way to tell
  the server "search a different repo this call."

  So if you ran one fff-mcp behind supergateway-rs and pointed all sessions at
  it, every session would see the same single repo's index regardless of which
  directory the session is in.

  What is actually feasible

  Option: One fff-mcp per repo (not per session), via supergateway-rs --stateful
  Effort: ~zero — config change
  What you'd gain: Sessions on the same repo (multiple agents in ifkb) share the

    index. Different repos still need separate daemons.
  ────────────────────────────────────────
  Option: True multi-repo daemon
  Effort: Patch upstream: add project_path to each tool's params, hold
    Arc<RwLock<HashMap<PathBuf, SharedFilePicker>>> keyed by repo root,
  lazy-init
     on first call. The watcher is per-FilePicker so each entry stays
    self-contained.
  What you'd gain: One process serves N repos. Each (repo, query) still runs
    without contention because each FilePicker has its own RwLock.
  ────────────────────────────────────────
  Option: Leave it alone
  Effort: —
  What you'd gain: Current memory cost is ~3.6 GB across 16 processes; most of
    that is per-repo content index that can't be deduped anyway. Sharing only
    helps when the same repo is open multiple times.

  Recommendation

  The cheapest real win is option 1: run one fff-mcp per repo through
  supergateway-rs (port per repo), and have your wrapper script map cwd →
  repo-root → port. Same-repo agents (your common case — multiple agents in ifkb
   / measurex-re / Desktop) share the index, which is where memory currently
  duplicates.

  Option 2 is a clean upstream contribution and the code structure is friendly
  to it (the Shared* wrappers are already designed for sharing) — maybe a day's
  work plus tests. Worth opening an issue first; the author may have a reason
  it's per-process by design (e.g. LMDB exclusivity for frecency).

  Source cloned to /c/work/fff.nvim.

✻ Sautéed for 3m 17s

※ recap: You asked whether fff-mcp is thread-safe and whether a single shared instance could replace the 16 per-session processes. Answer: thread-safe yes, single shared instance no without code changes; next action is deciding between one-per-repo via supergateway or patching upstream. (disable recaps in /config)

❯ I see supergateway-rs using quite a lot of cpu... investigate why

● Bash(echo "=== Current supergateway-rs state ===" && ps -eo
      pid,ppid,%cpu,%mem,rss,vsz,nlwp,etime,stat,args | rg 'supergateway' | rg
      -v 'rg '
      echo ""…)
  ⎿  === Current supergateway-rs state ===
      1280  1039  119  0.0  5184 10564600 141   02:20:50 Ssl  /c/work/super
     gateway-rs/target/release/supergateway-rs --stdio /c/users/oystein/bin
     … +90 lines (ctrl+o to expand)

● Bash([ -d /c/work/supergateway-rs ] && echo "exists" || echo "missing"
      ls /c/work/supergateway-rs 2>/dev/null…)
  ⎿  exists
     BENCHMARKS.md
     CLAUDE.md
     … +28 lines (ctrl+o to expand)

● Bash(echo "=== Take an instant sample (10s) ==="
      # Sample to see if CPU is real ongoing work or idle…)
  ⎿  === Take an instant sample (10s) ===
     top - 12:23:34 up  2:21,  0 user,  load average: 0.52, 0.58, 0.59
     Threads: 341 total,  46 running, 295 sleeping,   0 stopped,   0 zombie
     … +22 lines (ctrl+o to expand)

● Bash(echo "=== Cargo.toml: runtime deps ===" && rg -n
      '^(name|asupersync|tokio|hyper|axum|reqwest)'
      /c/work/supergateway-rs/Cargo.toml 2>/dev/null | head -30
      echo ""…)
  ⎿  === Cargo.toml: runtime deps ===
     2:name = "supergateway-rs"
     11:asupersync = { version = "=0.2.7", features = ["tls-webpki-roots"]
     … +20 lines (ctrl+o to expand)

  Read 1 file (ctrl+o to expand)

● Bash(echo "=== /proc/1405/wchan ==="
      cat /proc/1405/wchan 2>/dev/null; echo ""…)
  ⎿  === /proc/1405/wchan ===

     === /proc/1405/stack ===
     … +125 lines (ctrl+o to expand)

● Bash(echo "=== child count per gateway ==="
      for pid in 1280 1281 1282 1357 1417; do…)
  ⎿  === child count per gateway ===
     PID 1280 (/slack-mcp): 27 children
     PID 1281 (/sentry-mcp): 0 children
     … +12 lines (ctrl+o to expand)

  Read 1 file (ctrl+o to expand)

● Bash(ls /c/work/supergateway-rs/src/gateway && echo "---"
      echo "=== child shells with no descendants (dead sessions) for zendesk
      ==="…)
  ⎿  http_to_stdio.rs
     mod.rs
     sse.rs
     … +10 lines (ctrl+o to expand)

  Searched for 2 patterns, read 2 files (ctrl+o to expand)
● Bash(echo "=== relay loop ==="
      rg -n 'fn spawn_relay|relay_thread|relay_task|while|loop
      \{|read_line|read_until'
      /c/work/supergateway-rs/src/gateway/stateful_http.rs…)
  ⎿  === relay loop ===
     261:fn spawn_relay_thread(
     281:    loop {
     … +91 lines (ctrl+o to expand)

  Read 2 files (ctrl+o to expand)

● Root cause: per-session 1ms spin-poll in the relay thread

  Symptoms:
  - zendesk gateway (PID 1357): 341 threads, 270% CPU, 77 live child sessions,
  298 fds
  - slack gateway (PID 1280): 141 threads, 119% CPU, 27 child sessions
  - sentry/hubspot/apify: tiny (~7% CPU, 33 threads) — no sessions held
  - top: 74.6% sys / 22.2% us / 0% idle, individual worker threads pegged at
  99.9%
  - Zero TCP clients connected to the gateways right now — they're spinning idle

  The bug

  crates fff-mcp/src/child.rs:217-231:

  pub fn recv_message(&self) -> Result<Parsed, ChildError> {
      loop {
          let guard = self.stdout_rx.lock().unwrap();
          let rx = guard.as_ref().ok_or(ChildError::StdoutClosed)?;
          match rx.try_recv() {
              Ok(Ok(parsed)) => return Ok(parsed),
              Err(async_mpsc::RecvError::Empty) => {
                  drop(guard);
                  std::thread::sleep(Duration::from_millis(1));   // ← spin-poll
              }
              ...
          }
      }
  }

  And src/gateway/stateful_http.rs:276-307:

  fn relay_loop(...) {
      loop {
          let parsed = match data.child.recv_message() { ... };   // ← uses
  spin-poll variant
          ...
      }
  }

  Every stateful session spawns a dedicated OS thread running relay_loop. When
  the child MCP server is idle (the usual state), the loop does:

  1. mutex.lock()
  2. rx.try_recv() → Empty
  3. drop lock
  4. nanosleep(1ms)
  5. repeat

  That's ~1000 wakeups/sec per idle session. With 77 zendesk sessions = ~77,000
  wakeups/sec, mostly kernel time (futex + nanosleep). Matches the 74.6% sys CPU
   and 99.9%-per-thread profile exactly.

  child.rs:200-202 already admits the right path:

  ▎ "Relay loops (sse.rs, ws.rs) should call this once and use rx.recv(cx).await
  ▎  directly instead of the spin-polling methods below."

  The sse.rs and ws.rs relays apparently do this correctly.
  stateful_http.rs::relay_loop does not — it calls the spin-poll variant, so
  every stateful HTTP session pays this tax.

  Secondary issue: sessions are accumulating, not timing out

  --sessionTimeout 300000 is 5 minutes. Gateways have been up 2h21m. Zendesk has
   77 live sessions, slack 27. The idle-timer logic at stateful_http.rs:1173 is
  gated on access_count == 0 — if any guard isn't dropped (e.g. a long-running
  SSE GET stream holding a guard), the session never becomes "idle" and the
  timer is never armed. With 18 concurrent claude agents reconnecting over time,
   they pile up.

  This amplifies the spin-poll cost linearly, but fix #1 is the actual fix —
  even if sessions accumulate, idle ones should cost ~0 CPU.

  Fix

  In src/gateway/stateful_http.rs::relay_loop, switch to the async path the doc
  already recommends:

  let mut rx = data.child.take_stdout_rx().expect("relay owns stdout rx");
  loop {
      let parsed = match rx.recv(cx).await {
          Some(Ok(p))  => p,
          Some(Err(e)) => { /* codec error → fail */ break; }
          None         => { /* EOF → child dead */ break; }
      };
      // route as before
  }

  This requires relay_loop to be an async task on the asupersync runtime (it
  currently is spawned as a std::thread::spawn). Mirror what sse.rs/ws.rs
  already do.

  Immediate mitigation

  Restart the four high-CPU gateways via PM2 — no fix required, just kills the
  accumulated relay threads. The bug returns as soon as ~10 sessions accumulate
  again, so expect this to recur until the source fix lands.

  pm2 restart slack-mcp zendesk-mcp     # only the two bad ones, others are fine

  (Source already cloned at /c/work/supergateway-rs; happy to draft the patch if
   you want.)

✻ Brewed for 3m 21s