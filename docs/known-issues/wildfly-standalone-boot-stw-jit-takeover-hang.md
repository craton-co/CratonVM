# WildFly standalone boot hangs forever in CratonVM's STW cross-thread JIT-takeover during parallel-extension-add

Status: OPEN — new, found 2026-07-13 while root-causing
[[wildfly-standalone-managed-server-boot-fails-under-surefire-fork]] after the
`ProcessBuilder.environment()` fix (`d22a5e73`) landed and changed but did not resolve that bug.
Severity: **Critical** — this is now the *dominant* cause (~91% of observed failures, see Scale) of
WildFly managed-server boot failure under CratonVM, larger than the companion
[[wildfly-remoting-classcastexception-parallel-extension-add]] race.
First confirmed: 2026-07-13, Azure worktree `test/wildfly-full-suite-20260707`, dev@e8f36c78 (round-6e
binary, `frozen-cratonvm-wildfly-bugbash-v6-20260713`, built after `d22a5e73`)

## Symptom

WildFly's standalone boot process ("`org.jboss.as.standalone`" launched exactly as Arquillian's
`CommonManagedDeployableContainer` launches it) reaches the `parallel-extension-add` boot step — the
point where every WildFly extension (remoting, undertow, elytron, jaxrs, ejb3, weld, connector,
clustering.*, microprofile.*, ...) is initialized **concurrently**, spinning up ~30-40 threads at once —
and then simply stops producing any further output, permanently, for as long as the process is left
running (tested up to 120s in isolation; Arquillian's own harness gives up well before that and reports
`LifecycleException: Could not start container`).

The last line ever printed before the hang:

```text
WARN cratonvm_vm::runtime::interpreter: STW cross-thread JIT takeover is still waiting for cooperative
mutators rounds=64 pending=6 taken=0
```

This is a CratonVM-internal diagnostic, not WildFly's own logging — it comes from CratonVM's JIT-tiering
safepoint/stop-the-world machinery. `pending=6` and `taken=0` mean six mutator threads were asked to
cooperate with a JIT-takeover safepoint and, after 64 polling rounds, **not one of them ever did** — the
classic signature of a stuck/deadlocked safepoint rather than an ordinary slow boot.

## Confirmed as a genuine permanent hang, not slowness

Ran the exact captured Arquillian launch command in isolation (no Surefire, no Maven, no other concurrent
shards) five times:

- 1/5 attempts crashed quickly via a different, independent bug
  ([[wildfly-remoting-classcastexception-parallel-extension-add]])
- 4/5 attempts hit this hang: `timeout 20` killed all four after 20s with zero further output past the
  STW warning; a fifth run given a full 120s budget also produced nothing more after the same STW warning
  line — 72 total log lines, then silence for the remaining ~100+ seconds. `server.log` (WildFly's own
  post-bootstrap log file) never gets created in any of these runs, because the logging extension is
  itself one of the ~30 extensions stuck in the same parallel-add operation that never completes.

## Scale (round 6e, this session, post `d22a5e73`)

Of 478 classes completed before the run was stopped for this investigation: 463 `FAIL`. Of those,
401 (87%) show `LifecycleException: Could not start container` — Arquillian's own wait-timeout path,
consistent with the managed server silently hanging rather than exiting — versus only 39 (8%) showing
`LifecycleException: ... exited unexpectedly with code [1]` (the crash path, matching
[[wildfly-remoting-classcastexception-parallel-extension-add]]'s rarer race). This is a **reversal** from
before the `ProcessBuilder.environment()` fix landed, where the two signatures were roughly balanced
(round 6b/6c/6d: ~50/50) — that fix changed the dominant failure mode from "fast crash" to "silent hang,"
without net progress in OK count (still 0 across every round observed so far).

## Why this matters beyond WildFly — related to, but apparently not fixed by, `bee86ff0`

A symptom with the identical diagnostic ("`STW cross-thread JIT takeover is still waiting for cooperative
mutators ... rounds=64 ... taken=0`") was previously root-caused and marked FIXED in
`docs/internal/fixed-suite-bugs/wildfly-gc-barrier-boot-hang-and-harness-fixes.md` (dev `bee86ff0`,
2026-07-07): `main-vm`'s post-`main()` cleanup blocked in a raw `pthread_join` loop
(`ThreadRegistry::wait_for_non_daemon_threads`) without registering itself as "blocked" with the GC
barrier, so it stayed counted as an "alive, uncooperative" safepoint participant forever once any
worker thread requested a GC/STW pause. That fix long predates this session's binary
(`dev@e8f36c78`, 2026-07-13, includes `bee86ff0`) — yet the hang still reproduces. Two details argue this
is a related-but-distinct trigger, not a straightforward regression of that exact fix:

- The earlier bug reproduced with `pending=1` (only `main-vm` itself uncooperative, stuck post-`main()`).
  This one reproduces with `pending=6` — multiple worker threads, hit *during* active, mid-boot,
  concurrent extension loading (`parallel-extension-add`'s ~30-40 threads), not after `main()` has
  returned and handed off to a background thread pool.
- The fixed scenario was specifically about a *single, identifiable* uncooperative participant
  (`main-vm` parked in `pthread_join`). This one has *several* pending participants simultaneously,
  which is more consistent with a genuinely different code path failing to reach (or poll for) a
  safepoint under heavy concurrent thread creation/classloading, rather than the same one-thread-stuck
  mechanism recurring.

Flagging both possibilities for whoever picks this up: either the `bee86ff0` fix has a gap that a
different uncooperative-thread scenario still falls through, or this is a second, independent way to
starve the same cross-thread JIT-takeover safepoint (also plausibly connected to the separately-noted
`wip/gc-stw-quota-race-20260710` branch, parked because an attempted fix there itself "HANGS — do not
merge"). Either way, WildFly's `parallel-extension-add` step is a clean, concrete, reliably-reproducible
trigger (~30-40 threads all doing real work — classloading, static init, service registration —
concurrently) that's considerably easier to reproduce in isolation than chasing this inside the full
suite, and should be a useful addition to whoever's test matrix for this general class of STW work.

## Repro

```bash
cd apps/wildfly/testsuite/integration/basic
rm -f target/wildfly/standalone/log/server.log
timeout 60 <cratonvm-javahome>/bin/java \
  -Xmx512m -XX:MetaspaceSize=128m \
  -Djboss.home.dir=target/wildfly -Djboss.server.base.dir=target/wildfly/standalone \
  -Djboss.server.log.dir=target/wildfly/standalone/log -Djboss.server.config.dir=target/wildfly/standalone/configuration \
  -Dorg.jboss.boot.log.file=target/wildfly/standalone/log/server.log \
  -Dlogging.configuration=file:target/wildfly/standalone/configuration/logging.properties \
  -jar target/wildfly/jboss-modules.jar \
  -mp <shared-dist>/modules:testsuite/integration/basic/target/modules \
  org.jboss.as.standalone -Dts.wildfly.version=32.0.1.Final -c=standalone.xml
# -> boots normally through ~30-40 thread startups, then (most runs) prints exactly one
#    "STW cross-thread JIT takeover is still waiting for cooperative mutators ... taken=0"
#    line and produces no further output for the rest of the timeout window.
```

Reproduces in ~4/5 attempts on an otherwise-idle host (no other concurrent shards); likelihood may be
load-dependent given it's a scheduling-sensitive race, so worth retrying a few times if it doesn't
reproduce immediately.

## Evidence

```text
/tmp/repro_iter1.log .. /tmp/repro_iter4.log, /tmp/repro_long.log on the Azure host — 5 isolated repro
  attempts, 4/5 hung silently after the STW warning line (72 lines total, then nothing)
/data/data/wt-wildfly-bugbash-20260707-runner/out/round6e-s*of6-jit-real-others-20260713-024309/
  logs/*.log — 401/463 FAIL classes this round show "Could not start container" (consistent with this
  hang; Arquillian's own timeout path never observes the CratonVM-side diagnostic directly since child
  stdout isn't forwarded into these logs)
```

## Related

[[wildfly-standalone-managed-server-boot-fails-under-surefire-fork]] — the original, less-precise
documentation of this whole failure family, written before this specific root cause was isolated.
[[wildfly-remoting-classcastexception-parallel-extension-add]] — the rarer, independent crash-flavored
sibling hit during the same boot step; possibly related at a deeper level (both fire specifically during
concurrent, multi-threaded extension/module loading) but not confirmed to share a root cause.
`docs/internal/fixed-suite-bugs/wildfly-gc-barrier-boot-hang-and-harness-fixes.md` — the earlier,
already-FIXED (`bee86ff0`, 2026-07-07) bug with the *identical* diagnostic message but a different
specific trigger (`main-vm` parked in a post-`main()` `pthread_join`, `pending=1`); see "Why this matters"
above for why this looks related but not simply a reopened regression of that exact fix.

## Investigation update 2026-07-13 (second session): root cause found and partially fixed, NOT merged to dev

**Status stays OPEN.** A candidate fix was developed, tested, and found to close the originally-diagnosed
deadlock but introduce a severe, only-partially-understood residual (silent early-boot stalls, and at
least one unconfirmed-attribution segfault). Given this bug class's history in this codebase (deep
GC-barrier work, high regression risk — see the two "candidate fixes, NOT attempted" this doc originally
listed), the fix was deliberately **not merged**. It is pushed to `origin/fix/wildfly-stw-jit-takeover-20260713`
(3 commits on top of `dev@d706ac4d`: `87fbb718`, `99ee0bb7`, `6d332a1e`) for whoever picks this up next.

### Context: this bug is no longer dominant

Before assuming this doc's original "~91% of failures" framing still holds: a clean-host baseline run
against current `origin/dev` (which now includes today's separately-landed
[[wildfly-remoting-classcastexception-parallel-extension-add]] fix and the stale-ObjectRef fix — see
`docs/internal/wildfly-parallel-boot-stale-objectref-residual.md`) already boots the isolated repro **OK
7/15 times (47%)**, with `STW_HANG: 3/15, CCE_CRASH: 4/15 (a separate, still-open AttributeAccess-cast
variant — see `docs/known-issues/wildfly-standalone-boot-attributeaccess-cce-register-invisible-root.md`
if present), TIMEOUT_NO_WARN: 1/15`. The STW hang is real but re-verify the current dominance/scale
numbers before prioritizing — they may have shifted again since.

### Root cause (confirmed via live gdb, poll-on-log-line technique)

Two distinct native blocking primitives never registered with the GC barrier's blocked-region
bookkeeping, unlike `NativeContextImpl::park` (`LockSupport.park`/`Object.wait`):

1. `native-builtins/src/stamped_lock.rs`: `rw_write_lock`/`rw_read_lock` (`ReentrantReadWriteLock`) and
   `stamped_write_lock`/`stamped_read_lock` (`StampedLock`) blocked on a raw `parking_lot::Condvar` with
   zero GC-barrier interaction — a genuinely contended wait here is indistinguishable from
   `LockSupport.park` to the GC barrier, but stayed counted in `expected` the whole time.
2. `vm/src/vm/vm_exec.rs`: `NativeContextImpl::monitor_enter` called `self.shared.monitors.enter()`
   directly instead of the established `monitor_enter_blocking()` contended-path helper — proven live via
   gdb to deadlock `CountDownLatch.await()`'s handshake-latch polling loop
   (`native_cdl_await`/`native_cdl_await_timeout`/`native_cdl_count_down`) under WildFly's
   `parallel-extension-add` (~30-40 `EnhancedQueueExecutor` workers).

First fix attempt (commit `87fbb718`) applied `monitor_enter_blocking` **broadly** to all ~81
`ctx.monitor_enter(...)` call sites in `native-builtins/src/{lang_invoke,lib,phases_early,wildfly_core,
jboss_msc,phases_late,letsgo_compat}.rs`. **This is unsafe and was reverted**: an audit found the
overwhelming majority of those call sites keep using the same `ObjectRef`/`Value` afterward without
pinning — since `monitor_enter_blocking`'s contended wait can now genuinely span a completing (possibly
moving) GC pause, this would trade one hang for a batch of new stale-`ObjectRef`-across-GC bugs (this
codebase's single most recurring defect class). Commit `6d332a1e` narrows this to a new opt-in
`NativeContext::monitor_enter_gc_safe` trait method, used ONLY by the 3 live-gdb-confirmed CDL call sites
(each already updated to pin+refresh via the existing `monitor_enter_keepalive`/`monitor_wait_keepalive`
idiom); `monitor_enter` itself stays on its original, non-GC-blocked path for every other caller.

### Second bug found via live gdb: lock held across `begin_blocking_region`/`end_blocking_region`

The first `stamped_lock.rs` fix (in `87fbb718`) called `ctx.begin_blocking_region()`/`end_blocking_region()`
**while still holding the lock's own raw `parking_lot::MutexGuard`** (the `state` binding). Both calls can
synchronously block for an entire in-flight STW pause (`GcBarrier::arrive_and_wait_auto`/
`mark_blocked_region_leave`). Holding `state` across that means: the pause waits for the actual lock
holder to arrive; the holder needs `state` to release/re-check; a WAITER (not the holder) is holding
`state` blocked inside the pause-wait itself — a real 3-way deadlock, live-gdb-captured as 18 threads
piled up in the `cv.wait` loop with zero progress and, critically, **no STW warning ever printed** (the
arrived waiter's own wait for pause-completion isn't reflected in the initiator's `rounds`/`pending`
diagnostic — this is why it manifests as a SILENT hang, not the expected warning). Fix (also in `6d332a1e`):
drop the guard before each `begin_blocking_region`/`end_blocking_region` call, reacquire after; only the
actual `cv.wait` loop needs the guard (and manages its own atomic release/reacquire internally).

### Verification: encouraging on the specific deadlock, but NOT clean overall

A 20-attempt clean-host repro loop (host idle, confirmed via `uptime`) against the fully-fixed binary
(`87fbb718`+`99ee0bb7`+`6d332a1e`) showed **zero STW_HANG** — the two originally-diagnosed deadlocks
appear genuinely closed. However the SAME loop (and a repeat with a 180s timeout, and a single-run CPU-
activity probe) showed **`TIMEOUT_NO_WARN` in the 8-15/20 range** — boots silently stalling somewhere
EARLIER than `parallel-extension-add` (log tail consistently stops mid-sequential-extension-init, e.g.
right after "Initializing Connector Extension" / "Initializing Datasources Extension", sometimes preceded
by a `gen_heap::guard out-of-bounds field read dropped` warning) — a category that **did not exist at all**
in this doc's original characterization (baseline only ever showed STW_HANG or CCE_CRASH, never a silent
stall with no diagnostic). Bisection (4 separate binaries: clean baseline, `87fbb718` alone, `6d332a1e`
narrow-scoped, and `87fbb718` with `stamped_lock.rs` reverted) could not cleanly attribute this to either
individual change — EVERY variant that exercises the shared `begin_blocking_region`/
`monitor_enter_blocking`/`GcBarrier::enter_blocked` machinery at all shows this stall at similar severity,
while the untouched baseline shows it in only 1/15 runs. This points at a bug in the shared blocked-region
primitive itself under this specific ~30-40-thread concurrent workload, not in either specific caller's
own logic — **not yet root-caused**.

Separately, `dmesg` showed two SIGSEGV crashes of a `main-vm` process at the identical faulting address
during this session's testing window — but the host was, by that point, extremely oversubscribed by many
OTHER concurrent sessions' own `cratonvm`-based processes (load average >4 on an 8-core box, 15+ users),
and "main-vm" is a generic process name shared by every `cratonvm` binary — **attribution to this fix
specifically is NOT confirmed**, just flagged as a loose thread worth checking first before assuming it's
unrelated.

`cargo test -p cratonvm-vm --lib` (debug mode, NOT `--release` — release mode disables the
`debug_assert!`-gated `runtime::lock_order` enforcement tests and produces 9 additional false-positive
failures, a trap worth remembering for future verification of anything in this area) showed **2198
passed / 12 failed**, identical to the known pre-existing baseline — no unit-test-level regression from
the diff itself.

### How to apply / next steps for whoever picks this up

1. **Do this on an idle host.** This session's later verification rounds were badly contaminated by
   15+ concurrent unrelated sessions sharing the Azure box — `TIMEOUT_NO_WARN` counts are NOT trustworthy
   under that load (boot may simply need more than the timeout budget, not be truly stuck) and could not
   be reliably disambiguated from a genuine new bug even with a 180s timeout and a CPU-activity probe.
   Check `uptime`/`ps aux --sort=-%cpu` before trusting any repro-loop numbers.
2. Start from `origin/fix/wildfly-stw-jit-takeover-20260713` (`6d332a1e`) rather than redoing the above —
   the two root-caused-and-fixed deadlocks are solid, keep them. Rebase onto current `origin/dev` (expect
   a real merge — dev has moved substantially since the `d706ac4d` fork point, including a conflict in
   `native-builtins/src/lib.rs` last time this was attempted, not yet resolved).
3. Live-gdb the `TIMEOUT_NO_WARN` stall specifically (NOT the original STW-warning hang, which is
   understood): poll the boot log for it going quiet for several seconds after an "Initializing X
   Extension" line (rather than polling for the STW warning, which never appears in these runs), then
   `thread apply all bt` every thread. Figure out whether it's inside the blocked-region enter/exit
   protocol itself (lock ordering, a missed wakeup, a counter never decremented) or something else
   entirely that merely correlates with these commits.
4. Only merge once a CLEAN-host repro loop shows a clear, reproducible improvement over the 7/15 baseline
   OK rate — not just STW_HANG=0, since TIMEOUT_NO_WARN is arguably just as bad a boot outcome.
