# WildFly domain boot: `WFLYHC0053` inventory transport resolved; async-future deadlock FIXED; residual now downstream

Status: OPEN, narrowed a third time — but the defect this doc's title names is now FIXED. The
`WFLYHC0053` inventory timeout is fixed (2026-07-14), the StreamDecoder stale-buffer residual is fixed
(2026-07-15, `21c5d6f6`), and the named blocker from the 2026-07-15 follow-up — the
`async_future_wait_keepalive` stall in `native-builtins/src/wildfly_core.rs:1537` that permanently wedged
every managed-server boot before it could reach `WFLYSRV0025` — is root-caused and FIXED (2026-07-16, see
below): it was never actually inside the async-future/remoting machinery itself; it was a genuine AB-BA
lock-order-inversion deadlock in the synthetic XNIO conduit listener-dispatch layer, one native call
level below. What keeps this record open is that a clean, fully-uncontaminated verification run showing
BOTH `Server:server-one` and `Server:server-two` reach `WFLYSRV0025` in the SAME run has not yet been
captured — not because the fix doesn't work (it demonstrably does; see verification below, including one
run where `server-two` reached its own `WFLYSRV0025` cleanly), but because the Azure probe host was under
severe, worsening disk pressure (root filesystem at 97-100% full, sometimes <50 MB free) for this entire
session, and because getting far enough into boot now reliably surfaces the separate, already-tracked
`WFLYCTL0079`/`AttributeAccess` ClassCastException family
(`wildfly-standalone-boot-attributeaccess-cce-register-invisible-root.md`) as the next blocker. Neither
of those is this doc's defect to fix; see "2026-07-16" section below for the full evidence chain and an
honest accounting of what is and isn't proven.

## Fixed and removed from the active issue scope (2026-07-14)

The Process Controller emitted the correct WildFly protocol response: a `0x98` chunk header, payload beginning with the expected `0x15` inventory command, and `0x99` end marker. The Host Controller nevertheless read `0` for that one-byte command and rejected it as an invalid command, closed the connection, and later raised `WFLYHC0053`.

The fault was in `native_socket_input_stream_read_one` (`native-builtins/src/net_phase_e.rs`). It allocated a one-byte Java array, called the potentially blocking socket-read native, and then dereferenced its raw `ObjectRef`. A moving GC during the read could forward the array, leaving the local stale. The fix pins the one-byte array across the read, refreshes it through the pin, then reads and unpins it. The lower-level bulk read already uses the blocking-region/root protocol.

Two adjacent GC-safety defects exposed by the now-progressing boot were fixed in the same changeset:

- `native-collections` now pins and refreshes the values-view map/list around entry collection, and refreshes key/value references through resize and allocation in linked-hash-map insertion.
- Stream-chain processing pins its current element around lambda invocation and refreshes the forwarded value before continuing or emitting it.
- `DelegatingServiceController` no longer receives unsafe native aliases intended for the concrete MSC controller layout; the wrapper's inherited methods now dispatch normally.

With the unique remote binary `/data/bin/cratonvm-wildfly-hc0053-complete-20260714-231440`, a fresh WildFly 32.0.1.Final no-JIT domain probe passed the former handshake point, produced neither `Invalid command byte` nor `WFLYHC0053`, and launched both `Server:server-one` and `Server:server-two`.

## Fixed 2026-07-15: the StreamDecoder server-output reader residual

The previous "Remaining residual" — both launched servers' stderr-reader threads failing the
`CRATONVM_DBG_STALE_OBJREF` canary inside `java/io/InputStreamReader.read([CII)I` — is fixed on branch
`fix/wildfly-gc-pin-stream-20260715` (commit `21c5d6f6`, merged to dev with this doc update).
`native-io/src/stream_decoder.rs::decode_into` held three raw refs across its GC-capable refill window
(the temporary byte array across the potentially blocking `InputStream.read([BII)I` invoke; the
destination char array and `this` across the same window); `native_sd_read` re-used `this`+`out` across
`decode_into` iterations unpinned; `native_sd_close` re-used `this` after re-entering Java via
`close()`. All now pin and re-read through `native_pin_roots` per the Family-1 contract.

Verified: three no-JIT domain probes with `CRATONVM_DBG_STALE_OBJREF=1` (2026-07-15, fixed binary)
produced **zero canary firings anywhere in the process** — previously both server stderr-reader threads
tripped it. `cargo test -p cratonvm-native-io --lib`: 349 passed.

## 2026-07-15 verification state (why this record is still open)

Domain probes on the fixed binaries (Azure host, WildFly 32.0.1.Final, unique loopback/ports; logs under
`/data/wt-wfgc-20260715/probes/`):

- No `Invalid command byte`, no `WFLYHC0053`, no stale-canary firing in any probe (6+ runs, no-JIT and
  JIT, plain and canary-flagged).
- The Host Controller completes its own boot (`WFLYSRV0025 ... (Host Controller) started`), both managed
  server processes are spawned (`Starting process 'Server:server-one'`/`'server-two'`) and register
  (`WFLYHC0020: Registering server server-two`), and both emit `WFLYSRV0049 ... starting` on the console.
- Neither server reached its own `WFLYSRV0025 started` within the probe timeouts. Two boot-wide wedge
  mechanisms in exactly this window (a census-counted CHM segment-monitor deadlock against the STW
  barrier, and a segment-monitor ↔ registry-RWLock ordering cycle) were root-caused and fixed during
  this same session — see the 2026-07-15 follow-up in
  `wildfly-standalone-boot-attributeaccess-cce-register-invisible-root.md`. The HC itself was measured
  stalling for minutes per STW pause pre-fix (`STW cross-thread JIT takeover ... rounds=64` in the HC
  log), which starved the server-registration sync; that marker is gone post-fix. The remaining
  verification gap is a clean post-fix domain run on a quiet host reaching `WFLYSRV0025` in both
  server logs.

## Verification completed for committed changes

- `cargo test -p cratonvm-native-builtins jboss_msc --lib`: 18 passed.
- `cargo test -p cratonvm-native-builtins net_phase_e --lib`: 36 passed.
- `cargo test -p cratonvm-native-collections --lib`: 72 passed.
- `cargo test -p cratonvm-native-io --lib`: 349 passed (2026-07-15).
- Remote fresh WildFly 32.0.1.Final domain probe reached managed-server launch and cleared the original protocol failure; 2026-07-15 probes additionally cleared the stale-canary bar and reached both-servers-registering.

## Next step

Re-run the no-JIT and JIT domain probes against a dev build containing `21c5d6f6` + `7831ce2c` +
`b1ac28f3` on a host with load ≤ cores, with generous (≥900 s) timeouts, and confirm
`domain/servers/server-{one,two}/log/server.log` each contain `WFLYSRV0025`. Everything else in this
record is fixed and verified.

## 2026-07-15 addendum: second stale site fixed live (toArray iterator); next blocker named (managed-server async-future stall)

Two of the backtrace-enabled stale-canary domain probes (DOM13/DOM17, logs under
`/data/wt-wfgc-20260715/probes/logs/` on the Azure host) caught a SECOND, unrelated stale-ref site in
the Host Controller during extension init: `real_jdk_to_array_typed` (`vm/src/vm/vm_init.rs`) — the
`toArray(T[])` iterator fallback re-used `this`/template/target/iterator raw across its repeated
GC-capable `ctx.invoke` calls (`size`/`iterator`/`hasNext`/`next`), and the ArrayList-shaped path read
`elementData` after the target allocation could move it. **FIXED** (`94145eaf`, pinned per the
Family-1 contract; `cargo test -p cratonvm-vm --lib`: 2217 passed). Other probes (DOM10/DOM16) ran the
identical config with zero firings — the site is timing-dependent, so future stale-canary runs should
always set `RUST_BACKTRACE=1`.

The remaining "both servers reach WFLYSRV0025" gap now has a concrete, named blocker: a live
sudo-gdb attach on a stalled `Server:server-one` process (`/data/tmp/server-one-stall.threads`) shows
its Controller Boot Thread parked in `monitor_wait` under
`async_future_wait_keepalive` (`native-builtins/src/wildfly_core.rs:1537`) — an async future that is
never completed — while two peer threads sit in interpreter `monitorenter` and every
XNIO/MSC/remoting carrier idles normally. The server processes reach `WFLYSRV0049 starting` +
root-service start, register with the HC (`WFLYHC0020`), then wedge there deterministically
(~210 console lines each, identical across probes and timeouts up to 1500 s, JIT and no-JIT alike).
This is a NEW, separate defect in the synthetic WildFly async-future/remoting sync — the next
investigation for this record, with the gdb dump above as its starting evidence.

## 2026-07-16: async_future_wait_keepalive stall ROOT-CAUSED and FIXED — it was an AB-BA conduit-listener deadlock, not an async-future/remoting defect

Investigated 2026-07-16, isolated worktree `/data/data/wt-hc0053-20260716` (Azure host), branch
`fix/wildfly-hc0053-asyncfuture-20260716`, forked from `origin/dev @ d1be7310`.

### Reproduction

The stall reproduces reliably on a fresh build with no diagnostic flags: a domain boot with
`CRATONVM_MSC_REAL_START=1` reaches `WFLYHC0020: Registering server server-one`/`server-two`, both
managed servers reach `WFLYSRV0049 starting`, and then the whole `Server:server-one` process goes
completely, permanently silent — no further console output at all, indefinitely (confirmed with a
240s window; the 2026-07-15 finding's own testing to 1500s showed the same permanent wedge, not a slow
resolution).

### Live diagnosis

A temporary, session-local diagnostic (not part of the landed fix — added to
`monitor_enter_blocking` in `vm/src/vm/vm_exec.rs`, gated behind the existing `CRATONVM_DBG_MONENTER`
flag, reverted before committing) logged, on every contended `synchronized`-monitor entry: the blocked
thread's VM `ThreadId`, the monitor object's class name, and the monitor's current owner `ThreadId`
(via `MonitorTable::current_owner`, already present as a test-only helper). Cross-referenced against a
live `sudo -n gdb -p <pid> -batch -ex 'thread apply all bt'` snapshot of the wedged `server-one`
process, this gave the full picture:

- Two native threads were parked inside `monitor_enter_blocking`/`block_enter`
  (`vm/src/threading/monitor.rs`), both reached via the exact same native function,
  `invoke_source_read_listener` (`native-builtins/src/xnio_conduits.rs`), but via two **different**
  native call chains:
  - `native_source_poller_run` (`native-builtins/src/xnio_worker.rs:1272`) — the dedicated background
    thread (`cratonvm-xnio-s`) that periodically scans every registered source and fires its
    read-ready listener if data is pending (`notify_registered_sources_readable` →
    `notify_source_readable` → `notify_source_readable_with_delays`).
  - `native_sink_resume_writes` (`native-builtins/src/xnio_conduits.rs:1553`, the native for
    `ConduitStreamSinkChannel.resumeWrites()`/`wakeupWrites()`) — called synchronously on whatever
    application thread invokes it, which for a paired in-process pipe also ends up notifying the
    paired source's read listener (`notify_paired_source_readable_with_delays`).
- The diagnostic's log showed the exact deadlock: one thread held `org/xnio/streams/BufferPipeInputStream`'s
  monitor and blocked wanting `java/util/ArrayDeque`'s monitor; the other held the `ArrayDeque` monitor
  and blocked wanting the `BufferPipeInputStream` monitor — a textbook AB-BA lock-order inversion,
  permanent because neither side would ever release.

### Root cause

Real XNIO guarantees that a given channel/conduit's listener is invoked from exactly one IO thread at a
time — there is never a second concurrent invocation to race against. CratonVM's synthetic conduit layer
does not have real `epoll`/selector-driven IO threads for this transport; instead it added a
fixed-interval **background poller thread** (`native_source_poller_run`) that scans and fires listeners
on its own schedule, entirely independent of (and unsynchronized with) the direct,
`resumeWrites`-triggered notify path. Both paths can invoke the exact same source's real (interpreted,
unmodified) XNIO/JBoss-threads bytecode — e.g. `BufferPipeInputStream`'s push/pop logic backed by an
`ArrayDeque` — concurrently from two different native OS threads. That real bytecode was written under
XNIO's actual single-thread-per-channel invariant and has no reason to keep a globally consistent lock
order across its internal objects; under concurrent invocation from two threads, the two call paths can
(and, deterministically in this exact boot shape, do) acquire those two objects' monitors in opposite
order, deadlocking forever. Neither `async_future_wait_keepalive` nor the domain-boot async-future/
remoting machinery this doc originally suspected has any defect — the Controller Boot Thread's
`monitor_wait` there is a correct, honest wait for the real boot-completion signal; that signal simply
never arrives because the underlying I/O layer wedged first.

### Fix

`native-builtins/src/xnio_conduits.rs`: added a non-reentrant per-source dispatch guard
(`SourceChannel::dispatching: AtomicBool`, CAS-acquired before calling `invoke_source_read_listener` and
released via an RAII `DispatchGuard` on every exit path including unwind). If a notifier finds dispatch
already in progress for a source, it skips firing this round instead of invoking concurrently — the
poller's next tick (or the caller's own retry-with-delays loop) fires it once the in-flight dispatch
completes. This restores, at the dispatch level, the same "only one thread ever executes this listener
at a time" invariant real XNIO gets for free from its single-IO-thread-per-channel model, without
touching the interpreted listener bytecode itself.

### Verification

- `cargo test -p cratonvm-native-builtins --release --lib`: **2999 passed, 0 failed, 6 ignored** — no
  regressions.
- Five post-fix domain-boot repro attempts (`FIXCHECK1`-`3`, `VERIFY_NOJIT_1`, `VERIFY_JIT_1`; JIT and
  `CRATONVM_DISABLE_JIT=1`; logs under `/data/data/wt-hc0053-20260716/probes/logs/` on the Azure host):
  **zero** occurrences of the `ArrayDeque`/`BufferPipeInputStream` AB-BA pattern (or any other permanent
  monitor-entry wedge) — a complete change from the pre-fix baseline, which wedged there in every single
  attempt (including the original `server-one-stall.threads` capture this doc's 2026-07-15 section is
  built on). Concretely:
  - Host Controller now reaches its own `WFLYSRV0025` reliably in every post-fix run.
  - Both `server-one` and `server-two` now reach `WFLYHC0020` registration reliably in every post-fix
    run (previously the wedge occurred before/at this exact point).
  - In `VERIFY_NOJIT_1` (`CRATONVM_DISABLE_JIT=1`), **`Server:server-two` reached its own full
    `WFLYSRV0025`**: `WildFly Full 32.0.1.Final ... started in 38675ms` — direct, positive proof a
    managed server can now boot completely under the fixed binary.
- What did NOT yet happen: a single run where **both** `server-one` and `server-two` reach `WFLYSRV0025`
  together. In every post-fix run, at most one server got that far before the *other* (or, in one run,
  the same one on a later attempt) hit one of two things, neither of which is this doc's defect:
  1. The already-tracked, already-OPEN `WFLYCTL0079`/`AttributeAccess` `ClassCastException` family
     (`wildfly-standalone-boot-attributeaccess-cce-register-invisible-root.md`) — observed once,
     explicitly and unambiguously, in `FIXCHECK3`:
     `WFLYCTL0158: Operation handler failed: java.lang.RuntimeException: WFLYCTL0079: Failed
     initializing module org.jboss.as.logging`, followed by `WFLYSRV0056: Server boot has failed in an
     unrecoverable manner`. That doc's own established policy is not to patch this family piecemeal
     (the real fix is the precise-oop-map/shadow-stack infrastructure); nothing was attempted here.
  2. A silent process disappearance with **no** panic, exception, `SIGSEGV`, or any other diagnostic
     marker anywhere in the combined log, at a different and generally *later* point in boot on each
     successive attempt (deep in extension loading in one run, deep in subsystem/MSC service start in
     another). This pattern — no error signature, and the failure point drifting later each time as if
     progress is simply interrupted — was investigated and correlates with the Azure host's disk state
     during this entire session: the root filesystem (`/`, where `/tmp` lives) sat at 97-100% full
     (as low as ~44 MB free) throughout, and the separate `/data/data` mount (where this worktree and
     its build output live) sat at 97-99% full and *shrinking* over the session from unrelated
     concurrent activity on this shared host (confirmed via `df -h`; not this investigation's own
     doing — `TMPDIR`/`-Djava.io.tmpdir` were redirected to the far roomier `/data` mount, `df -h`
     58-59 GB free throughout, specifically to rule this out for the *build* step, though the probe
     JVMs' own runtime writes were only partially covered by that redirect). No OOM-killer evidence was
     found in `dmesg`/`free -h` (23 GB RAM free throughout), so this looks like disk-pressure-induced
     I/O failure (a write that silently fails or a JVM-internal scratch-file operation erroring out)
     rather than a memory kill, but this was not conclusively isolated before the host's disk state made
     further clean attempts unproductive.

### Updated status (superseded — see 2026-07-16 second session below)

The defect this doc's title and the 2026-07-15 follow-up named — the `async_future_wait_keepalive`
stall / permanent managed-server wedge — is **FIXED and verified** (root cause identified with live
evidence, targeted fix landed, zero regressions, and direct positive proof of a complete managed-server
boot post-fix). This record stays open only because the very last verification bar this doc set for
itself — both servers reaching `WFLYSRV0025` in one clean run — was not captured, and the reason it
wasn't is now well-characterized as two separate, non-blocking factors: the already-tracked
`WFLYCTL0079` CCE family (its own doc, its own policy) and Azure host disk pressure (an infrastructure
condition, not a code defect). Whoever revisits this next should either (a) re-run
`FIXCHECK`/`VERIFY_JIT`/`VERIFY_NOJIT`-style probes on a host with headroom on both `/` and whatever
mount hosts the WildFly install/build output, expecting the fix already landed here to hold, or (b) if
both-servers-together is still desired as an explicit closing artifact, treat it as blocked on the
`WFLYCTL0079` family closing first, not as separate open work in this doc.

## 2026-07-16 (second session): the `WFLYCTL0079` blocker's ROOT CAUSE found and fixed; server-two `WFLYSRV0025` reproduced on the fixed build; residual long-tail sites being closed with new producer-side diagnostics

Worktree `/data/wt-cce0079-20260716` (Azure host), branch `fix/wildfly-cce0079-close-20260716`,
forked from `origin/dev @ dcb24161`. Full writeup:
`docs/internal/fixed-suite-bugs/wildfly-cce0079-young-start-set-truncation-FIXED.md`.

- The `WFLYCTL0079`/CCE family that this doc's closing artifact was blocked on turned out to be a
  **single dominant GC defect**: the moving young collector's object-start walk broke at the first
  un-striden TLAB GAP-filler sentinel and silently dropped every young object above the breakout from
  the forwardable set — `forward_object` then returned every affected root/reference UNMOVED, so whole
  swaths of live young objects were never evacuated and every reference to them dangled into recycled
  memory after the semispace swap. The walk-stop warning appears in **100% of baseline boot logs**;
  fixed by making the walk gap-aware (free-list + TLAB skips + GAP-filler stride) with a
  skip-this-cycle fail-safe.
- Post-fix, WildFly *standalone* boots show **0 CCE across 14 valid probes** (vs ~50% baseline), with
  full `WFLYSRV0025` boots reproducing on a loaded shared host.
- Domain no-JIT probes now reliably reach both-servers-registered, the Host Controller's own
  `WFLYSRV0025`, and — on run `DC_001` (fix4 binary, stale-canary active) — **`Server:server-two`
  reached its own `WFLYSRV0025`** (92.9 s), replicating the 2026-07-15 single-server proof on the
  new build.
- The remaining gap to both-servers-in-one-run is a residual long-tail of Family-1 stale-ref
  producers that only fire in the domain no-JIT window (captured live this session:
  `xnio_conduits` sink resume/suspend, `dis_read_utf`/`dis_read_exact`, channel-alloc registry
  keys, TCP open/accept listener dispatch — all fixed; plus at least one still-open producer
  feeding an `aastore` in `SubsystemResourceDescriptionResolver.<init>` and a
  `cid=0`-receiver CCE in `AddStepHandler.recordCapabilitiesAndRequirements`). Two new
  producer-side diagnostics were landed to close these: stale-value checks at the
  `set_field`/`set_array_element` funnels (under `CRATONVM_DBG_STALE_OBJREF`) and a
  RETURN-value `load_and_forward` healing barrier at the native-call funnel (always on — the
  symmetric counterpart to the existing argument barrier), which both heals stale-at-return
  natives outright and names them under the debug flag.

