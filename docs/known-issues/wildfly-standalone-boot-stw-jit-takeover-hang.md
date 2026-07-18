# WildFly standalone boot hangs forever in CratonVM's STW cross-thread JIT-takeover during parallel-extension-add

Status: **REOPENED 2026-07-18** — see "Recurrence 2026-07-18" at the bottom. Symptom reproduced again
("LifecycleException: Could not start container", empty server.log, dominant failure mode across a
fresh 6-shard full-suite rerun) on a binary built from current dev, which includes every fix commit
this doc's history references as an ancestor. This is the FOURTH time this exact symptom has recurred
via a different specific mechanism (see the 2026-07-14 "Final resolution", the 2026-07-14 addendum, and
the 2026-07-15 follow-up below, each of which closed the doc after fixing a distinct root cause under
the same diagnostic signature) -- moved back to `docs/known-issues/` accordingly. The historical
resolution sections below are preserved as-is; none of their conclusions are disputed for the window
they cover, they just did not hold going forward.

Prior status (preserved for history): **RESOLVED** (2026-07-14) — see "Final resolution" at the bottom.
The specific bug this doc documents (the `STW cross-thread JIT takeover ... rounds=64 pending=N
taken=0` warning followed by a permanent hang) is fixed: 0 occurrences across 50+ verification attempts
post-fix, versus ~80-90% before. Moved to `docs/internal/fixed-suite-bugs/` accordingly; see the bottom
section for what remains genuinely open (separate, already-tracked bugs, not sub-parts of this one).

Original filing (2026-07-13), preserved for history:

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

## Investigation update 2026-07-14: CDL deadlock fix merged; doc STAYS OPEN (TIMEOUT_NO_WARN residual)

The CountDownLatch `monitor_enter` deadlock described above (root-caused via live gdb, previously pushed
to `origin/fix/wildfly-stw-jit-takeover-20260713` but held back pending an `~80`-call-site audit) is now
merged: `fix(gc): close CountDownLatch monitor_enter STW-barrier deadlock` (`native-api/src/registry.rs`,
`native-builtins/src/lib.rs`, `vm/Cargo.toml`, `vm/src/vm/vm_exec.rs` — new opt-in
`NativeContext::monitor_enter_gc_safe`, used only by the 3 live-gdb-confirmed CDL call sites; the other
~80 `monitor_enter` callers are untouched, deliberately, per the audit in the prior addendum). The earlier
broader `stamped_lock.rs` fix from that same branch was **dropped as redundant** — `dev` had already
independently gained an equivalent (and structurally simpler — call-site-level blocking-region wrap, no
lock-ordering hazard to manage) fix for `ReentrantReadWriteLock`/`StampedLock` via a different session's
"5-class cluster" fix (`945e4492`, already on `dev` before this branch was rebased). Verified: isolated
`cargo test -p cratonvm-vm --lib host_native_excludes_idle_thread_from_stw` passes; the full suite's other
handful of failures (native-count-threshold drift, unrelated flaky tests) reproduce identically on an
unmodified checkout — not a regression from this change. Merged to `dev` (`41b06719`), build-verified
clean post-push.

**This does NOT close the doc.** Two independent deadlock mechanisms behind the original "STW cross-thread
JIT takeover ... taken=0" warning are now fixed (rwlock/StampedLock via `945e4492`, CountDownLatch via
this fix) — `STW_HANG` (the case where that exact warning fires and then nothing) is meaningfully
reduced. But a **third, larger-magnitude, and still-unexplained** failure mode dominates: `TIMEOUT_NO_WARN`
— the boot process silently stalls earlier (mid sequential-extension-init, no STW warning ever printed) at
a rate that stayed high (roughly 40-70% of attempts across several sampling rounds) even during a
genuinely idle host window (freshly rebooted, `uptime` load 0.00/0.00/0.00, single user) — ruling out
"it's just host contention" as a full explanation. This is NOT yet root-caused. See the prior addendum's
"How to apply / next steps" section (poll the boot log for it going quiet, not for the STW warning, since
this stall never prints one) — that guidance stands unchanged and is the correct starting point for
whoever picks this up next. Given the volume of independent verification already invested here across
two sessions (isolated-repro bisection across 4+ binaries, live-gdb captures, a genuinely idle-host
sample) without pinning it down, this likely needs either a dedicated live-gdb session specifically
targeting the TIMEOUT_NO_WARN stall (not the now-fixed STW-warning cases), or a completely different
diagnostic approach (e.g. periodic `/proc/<pid>/stack` or `perf record` sampling across the whole stall
window rather than a single point-in-time `bt`, since the earlier live-attach attempts for the
STW-warning cases don't apply — there's no log line to poll for here).

## Final resolution 2026-07-14 (third session): TIMEOUT_NO_WARN was a measurement artifact — CLOSING

The prior addendum's `TIMEOUT_NO_WARN` residual — the thing keeping this doc open after both real
deadlocks were fixed — turned out to be a **false alarm caused by an under-informative repro-loop
methodology**, not a third bug. A CPU-activity-aware repro loop was built specifically to settle this:
after an initial 45s grace budget, it samples `/proc/<pid>/stat` `utime+stime` every 2s and only
classifies a slow run as `GENUINE_STALL` if CPU ticks stay **completely flat across 5 consecutive
samples** (10s of zero progress) — gdb-attaching immediately when that happens, poll-and-pounce style.
Anything still burning CPU when the hard 150s budget is hit is `SLOW_ACTIVE` instead: still working,
just slower than the old blind timeout allowed for.

15-run result on an idle host (post the CDL fix + `945e4302`/`945e4492`'s rwlock fix, both already on
`dev`):

```text
OK: 1   STW_HANG: 0   CCE_CRASH: 6   GENUINE_STALL: 0   SLOW_ACTIVE: 7   SEGV: 1
```

**Zero genuine stalls.** Every run the earlier `TIMEOUT_NO_WARN` bucket would have caught was actively
consuming CPU the whole time (`SLOW_ACTIVE`) — WildFly's later-stage *sequential* per-subsystem
extension init (after `parallel-extension-add` itself completes) is just legitimately slow under
CratonVM's interpreter on a host shared by 15+ other concurrent sessions, not stuck. The prior sessions'
`TIMEOUT_NO_WARN` numbers were an artifact of fixed, too-short timeouts (20-45s) with no CPU-activity
check to distinguish "still working" from "wedged" — exactly the trap the very first version of this
doc's own "Confirmed as a genuine permanent hang, not slowness" section warned about, re-encountered by
later sessions using a less careful method.

The other two buckets are separate, independently-tracked bugs, not part of this one:

- `CCE_CRASH` (6/15, 40%) — `ClassCastException: Object cannot be cast to
  org.jboss.as.controller.AttributeDefinition`, this run's instance triggered by
  `org.wildfly.extension.io.IOExtension` rather than the originally-filed `org.jboss.as.remoting`. The
  *original* site of this exact signature was root-caused and fixed
  (`fix(jit): pin checkcast/instanceof receiver across GC-triggering class load`, `70154861`,
  [[wildfly-remoting-classcastexception-parallel-extension-add]], moved to
  `docs/internal/fixed-suite-bugs/`) — that fix IS present in the binary used for this batch
  (confirmed via `git merge-base --is-ancestor`), so this is a **recurrence of the same bug class at a
  different, not-yet-covered call site**, exactly the "long-tail" pattern
  `docs/known-issues/wildfly-parallel-boot-stale-objectref-residual.md` already documents. Tracked
  there / in `wildfly-standalone-boot-attributeaccess-cce-register-invisible-root.md` if that doc
  exists — not re-diagnosed here.
- `SEGV` (1/15) — consistent with the same stale-`ObjectRef`-across-GC defect family; not attributed to
  a specific site in this session, flagged for whoever next works the stale-ObjectRef residual doc.

### Fix summary (three independent mechanisms, all now on `dev`)

The one diagnostic signature this doc tracks (`STW cross-thread JIT takeover ... rounds=64 pending=N
taken=0` then permanent silence) had **two distinct root causes**, both native blocking primitives that
never registered with the GC barrier's blocked-region bookkeeping (unlike `NativeContextImpl::park`,
used for `LockSupport.park`/`Object.wait`) — so a thread genuinely waiting on one stayed counted in the
STW barrier's `expected` set forever:

1. **`ReentrantReadWriteLock`/`StampedLock`** (`native-builtins/src/stamped_lock.rs` — `rw_write_lock`/
   `rw_read_lock`/`stamped_write_lock`/`stamped_read_lock`) — blocked on a raw `parking_lot::Condvar`
   with zero GC-barrier bracket. Root-caused independently by two sessions the same day: this doc's own
   investigation (live-gdb caught two threads permanently parked here during WildFly's
   `parallel-extension-add`, the lock held by `ConcreteResourceRegistration`'s shared registry lock)
   and, separately, a Tomcat/`TestOrderInterceptor` investigation
   (`docs/known-issues/tomcat-08-07/stw-crossthread-jit-takeover-hang-cluster.md`) that found the exact
   same missing bracket via a Windows `cdb` stack (Tribes' internal executor). The Tomcat-side fix
   landed first (`fix(gc): bracket 5 missing GC-blocking-region locks — the real STW takeover root
   cause`, `945e4492`, merged `dev`) — call-site-level `ctx.begin_blocking_region()`/
   `end_blocking_region()` wraps around each `lock()` registration in `native-builtins/src/lib.rs`, no
   function-signature change to `stamped_lock.rs` itself. It ALSO covers two more blocking primitives
   this doc's own investigation never got to: `native-builtins/src/xnio_async.rs`'s `native_iof_await`
   family (XNIO `IoFuture`, heavily used by Undertow/WildFly) and
   `native-builtins/src/concurrent_extras.rs`'s `SynchronousQueue` put/take/poll. This doc's own
   first-attempt fix (commit `87fbb718`, function-signature change + internal restructuring) was
   **dropped as redundant** once `945e4492` was confirmed already on `dev` and structurally simpler
   (no lock-ordering hazard to manage — see next point).
2. **`CountDownLatch`** (`native-builtins/src/lib.rs` — `native_cdl_await`/`native_cdl_await_timeout`/
   `native_cdl_count_down`) — `NativeContextImpl::monitor_enter` called `self.shared.monitors.enter()`
   directly instead of the established `monitor_enter_blocking()` contended-path helper. Live-gdb
   caught the lone holdout parked in `Monitor::block_enter`'s condvar wait while every other worker was
   correctly (GC-blocked) parked in `Object.wait()` inside the same handshake-latch polling loop. Fixed
   narrowly: a first attempt (`87fbb718`) applied `monitor_enter_blocking` to ALL ~80 `monitor_enter`
   call sites and was reverted after an audit found most of them keep using the same `ObjectRef`
   afterward with no pin-and-refresh (would have traded this hang for a batch of new
   stale-`ObjectRef`-across-GC bugs — this codebase's most recurring defect class). The final fix adds
   a narrow, opt-in `NativeContext::monitor_enter_gc_safe` used ONLY by the 3 live-gdb-confirmed CDL
   call sites (`native-api/src/registry.rs`, `vm/src/vm/vm_exec.rs`); `monitor_enter` itself is
   untouched for every other caller. Merged `dev` as `fix(gc): close CountDownLatch monitor_enter
   STW-barrier deadlock` (`41b06719`).

A **third, self-inflicted bug was also found and fixed during this doc's own investigation**, worth
flagging since it's an easy mistake to repeat: an early revision of the `stamped_lock.rs` fix called
`ctx.begin_blocking_region()`/`end_blocking_region()` **while still holding the lock's own raw
`parking_lot::MutexGuard`**. Since both calls can block synchronously for an entire in-flight STW pause,
holding that guard across them let a WAITER (not even the lock holder) block the actual holder from
ever reacquiring the state mutex to release/re-check — a new 3-way deadlock, live-gdb-captured as 18
threads piled up with zero progress and **no STW warning ever printed** (an already-arrived waiter's own
wait for pause-completion isn't reflected in the initiator's `rounds`/`pending` diagnostic — this is
exactly what a naive `begin_blocking_region`/`end_blocking_region` bracket placed carelessly around
existing lock code can produce, a hazard worth checking for in any future fix of this shape). Superseded
by `945e4492`'s call-site-wrap approach, which structurally avoids the issue (both calls happen entirely
outside any lock acquisition).

### Verification tally (all post-fix, no genuine hangs in any batch)

- This session (own): 4 isolated runs (livecap1-4) — 0 STW_HANG, then a 5-run CPU-census batch — 0
  STW_HANG, then the 15-run CPU-aware batch above — 0 STW_HANG, 0 GENUINE_STALL.
- An independent concurrent session's own 13-run verification loop (same combined-fix binary): 0
  STW_HANG.
- Unit tests: `cargo test -p cratonvm-native-builtins -p cratonvm-vm stamped` — 46/46 passing (43 +
  3), including the multi-threaded contention tests (`rw_concurrent_readers_no_serialization`,
  `stamped_no_lost_wakeup_under_contention`, `rw_writer_blocks_on_readers_and_releases`) — no
  regression from any of the fixes above.
- 0 STW hangs across 50+ total isolated attempts post-fix, versus ~80-90% before (the original doc's
  "Scale" section).

### Related

[[wildfly-remoting-classcastexception-parallel-extension-add]] — FIXED (`70154861`), moved to
`docs/internal/fixed-suite-bugs/`; the `CCE_CRASH` bucket above is a recurrence of the same bug class at
a new site, not this exact bug reopened.
`docs/known-issues/tomcat-08-07/stw-crossthread-jit-takeover-hang-cluster.md` — the sibling
investigation that found and fixed the `ReentrantReadWriteLock`/`StampedLock`/XNIO/`SynchronousQueue`
cluster (`945e4492`) via a completely different repro (Tomcat `TestOrderInterceptor`), independently
confirming the same root-cause class this doc found for `ReentrantReadWriteLock`/`StampedLock`.
`docs/known-issues/wildfly-parallel-boot-stale-objectref-residual.md` — tracks the `CCE_CRASH`/`SEGV`
long-tail family still causing WildFly boot failures; genuinely open, not closed by anything in this
doc.

## Addendum 2026-07-14 (fourth session): TIMEOUT_NO_WARN re-verified after a same-day regression; two more real bugs found and fixed

Picked this up right after the "Final resolution" section above closed the doc. Independently re-verified
that conclusion and found it still holds -- but only after fixing a fresh, unrelated, same-day regression
that would otherwise have made re-verification impossible (100% of boots crashed before ever reaching the
part of boot this doc is about).

### The doc's own "Final resolution" pre-dates a same-day regression that briefly broke ALL boots

Building a baseline release binary from this session's assigned fork point (`origin/dev @ 523ca9ba`,
2026-07-14 ~19:51 UTC) and running it against the isolated repro crashed **100% deterministically**
(20/20, and every manual retry) with:

```
[cratonvm] main-vm run() returned Err: Exception in thread "main" java/lang/IllegalStateException: Can't register delegate.
  ... at java/lang/management/ManagementFactory.getPlatformMBeanServer(...)
Caused by: java/lang/NullPointerException: Cannot read the array length because "this._ca_array" is null
  ... at javax/management/ObjectName.getCanonicalKeyPropertyListString(...)
```

`git bisect` (automated, build+repro at each step) isolated this to commit `d8092acb`
("fix-tests-real-jdk-contracts", 2026-07-14 17:09:46 UTC) -- landed **48 minutes before** this doc's own
"Final resolution" closing commit (`2c18948f`, 17:57:48 UTC). The closing session's own 15-run CPU-aware
sample shows a mixed `CCE_CRASH`/`SLOW_ACTIVE`/`SEGV`/`OK` distribution with no mention of this crash,
which is inconsistent with their binary actually including `d8092acb` (a deterministic, unconditional
crash would show as 100% of that specific type, not mixed) -- their binary almost certainly was built from
a fork point that predates it. **Their "Final resolution" conclusion (TIMEOUT_NO_WARN was a measurement
artifact) is not contradicted by this** -- it just did not have the chance to re-break on this specific
regression, which is independent of anything either of them investigated.

Root cause: `vm/src/vm/vm_init.rs`'s real-JDK-mode registration arms started unconditionally forcing
`native_methods.set_drop_synthetic_stubs(true)` (previously an opt-in-only mechanism via
`CRATONVM_NO_STUBS`, per the field's own doc comment in `native-api/src/registry.rs`). This silently
dropped several `register_*` clusters that are tagged `SyntheticStub` but are actually permanent bridges
needed in BOTH modes (no working real-bytecode fallback exists for them):

- `native-builtins/src/jmx.rs`: the entire `java.lang.management`/JMX native surface. Dropping
  `register_management_factory_platform_server_stub` let real bytecode run for
  `ManagementFactory.getPlatformMBeanServer()` for the first time ever in this VM -- exposing a genuine,
  previously-unexercised interpreter gap deep in `com.sun.jmx.mbeanserver.*` (real `ObjectName`
  construction NPEs). Not investigated further (out of scope -- see "Not investigated" below); fixed by
  retagging the whole cluster (13 of 14 `register_*` functions in the file) back to `Bridge`, so the
  untested real-bytecode path is never taken. `register_mbean_server_factory_synthetic` correctly stays
  `SyntheticStub` (real synthetic-JDK-only, never called in real mode, per its own pre-existing doc
  comment).
- `native-builtins/src/lib.rs`: `register_function_identity_natives`
  (`java.util.function.Function$Identity`) -- a purely VM-internal synthetic stand-in for the real
  lambda-based `Function.identity()`, with zero real bytecode to fall back to at all.
  `UnsatisfiedLinkError: Function$Identity.andThen` the instant the stub was dropped.

Fixed on `dev` (merged same-day as multiple *other* concurrent sessions independently retagging different
mis-categorized clusters the same way -- `d9e693d7` String.getBytes/Charset, `f62d2073` Properties,
`776b8cb0` a second, independent fix to the exact same `register_vm_management_impl` this session also
retagged, merged as a same-comment collision). `set_drop_synthetic_stubs(true)` itself stays enabled by
default for real-JDK mode, matching that established, now-clear convention, rather than reverting the
whole mechanism.

### Family-1 stale-ObjectRef-across-GC: root-caused via CRATONVM_DBG_STALE_OBJREF + a Linux backtrace, and a poisoning-cascade amplifier found

This session's real assignment: chase the [[wildfly-standalone-boot-attributeaccess-cce-register-invisible-root]]
doc's "Separate finding" lead (a Windows session's `CRATONVM_DBG_STALE_OBJREF` capture during
`parallel-extension-add`, 9/20 firing rate, unattributed backtrace due to Windows symbol resolution
failing). On Linux, `RUST_BACKTRACE=1` resolves cleanly. A 20-attempt diagnostic loop (post-JMX-fix
binary) hit this assertion **20/20** -- every single boot -- confirming it is real, common, and load-bearing
for the TIMEOUT_NO_WARN shape: **every one of the 20 panics left the process hung (`rc=124`, timeout, not
crashed)**, because the panic fires on a background `parallel-extension-add` worker thread and Rust's
default panic behavior (unwind, not abort) only kills that one thread -- the VM's own per-native-call
`catch_unwind` wrapper (`safe_native_call_impl`) converts it to a caught error rather than propagating a
process-level crash.

Deduped by innermost native-crate backtrace frame (`native-collections`/`native-builtins`), 20 panics
mapped to ~10 distinct call sites, dominated by `native-collections/src/lib.rs`'s `LinkedHashMap`
machinery (`lhm_find_node`, `native_stream_for_each`, `native_stream_all_match`,
`invoke_deferred_stream_lambda`, `native_path_address_from_elements`, and others). A **genuinely
concurrent same-day session** (dev `2d45ef40`, "8 more stale-ObjectRef-across-GC sites found via
live-log mining") landed a comprehensive fix for most of these independently, via the exact same mining
technique this doc's "How to apply" guidance already suggested (mining the WFLYCTL0153 investigation's
preserved `HIT_staleobjref_*.log` backtraces) -- this session's own overlapping `lhm_find_node` fix was
dropped in favor of that one during the `dev` merge (functionally identical pin/re-read pattern, no real
conflict).

**Not covered by that session, and fixed here**: `native-collections/src/lib.rs`'s overlay-table global
`Mutex` accessors (`lhm_overlay()`/`lhm_ptr_cache()`/`ll_overlay()`) still used naive `.lock().unwrap()`
at 6 call sites reachable from ordinary `get`/`put`/`remove` operations -- as opposed to the GC's own
root-scan pass, which `dev eb13200b` already hardened back in June. A `std::sync::Mutex` poisons
*permanently* once any thread panics while holding it. Live-captured directly in one diagnostic run: a
single `CRATONVM_DBG_STALE_OBJREF` panic inside `lhm_find_node`'s callers, immediately followed by
**5 more panics on completely unrelated worker threads**, all `PoisonError` on
`lhm_overlay()`/`lhm_ptr_cache()`, while the boot log sat silent exactly at
`DEBUG [org.jboss.as.connector] Initializing Connector Extension` -- the precise symptom this doc's own
prior addendum described ("the log tail consistently stops mid-sequential-extension-init ... right after
'Initializing Connector Extension'"). This is a real, additional, well-evidenced mechanism by which a
*single* Family-1 stale-ObjectRef bug (this codebase's most recurring defect class, not fully closed even
after 50+ prior fixes) can cascade into dozens of unrelated worker threads failing, matching the
TIMEOUT_NO_WARN shape exactly. Applied this file's own already-established poison-recovery idiom
(`.lock().unwrap_or_else(|e| e.into_inner())`, already used at 8 other call sites in this same file since
June) to the remaining 6 naive sites, so a future/still-open Family-1 panic degrades to one bad lookup
instead of cascading VM-wide.

### Verification

CPU-activity-aware repro loop (this doc's own "Final resolution" methodology), 20-run batches, Azure Linux
host, moderate-to-idle load (`uptime` load average 1.7-2.3 on 16 cores):

| Binary | OK | STW_HANG | CCE_CRASH | GENUINE_STALL | SLOW_ACTIVE | SEGV |
|---|---|---|---|---|---|---|
| Post-JMX-fix, pre-poison-fix | 1 | 0 | 6 | 0 | 4 | 5 |
| Final (JMX + Function$Identity + poison-lock fixes) | 1 | 0 | 8 | 0 | 10 | 1 |

**`GENUINE_STALL: 0/20` in both batches** -- consistent with the "Final resolution" section's own
conclusion, now re-confirmed on current `dev` after fixing the same-day regression that would otherwise
have made this unverifiable. `CCE_CRASH` and `SEGV` are the already-separately-tracked
register-invisible-JIT-root / stale-ObjectRef long-tail family
(`docs/known-issues/wildfly-parallel-boot-stale-objectref-residual.md`,
`wildfly-standalone-boot-attributeaccess-cce-register-invisible-root.md`) -- not this doc's concern, not
reopened here. `SLOW_ACTIVE` (still consuming CPU, not stuck) is expected interpreter overhead under host
contention, matching this doc's existing conclusion.

`cargo test -p cratonvm-native-builtins --lib` (2994/0/6 ignored), `-p cratonvm-native-collections --lib`
(72/0), `-p cratonvm-vm --lib` (2217/0/111 ignored) all pass, identical to pre-fix baselines.

### Not investigated (honest residuals, not chased further here)

- **The real-bytecode JMX gap** (`ObjectName`/`Repository`/`DefaultMBeanServerInterceptor` construction
  failing under real interpretation) is now avoided (native intercepts the call again) but not
  root-caused. If a future session wants CratonVM to actually run real JMX bytecode in real-JDK mode
  (rather than the synthetic `alloc_mbean_server` shim), that gap needs a dedicated investigation.
- **`lambda_arg_provably_not_instance`** (`vm/src/runtime/interpreter.rs:19024`, inside
  `checkcast_lambda_instantiated_args`/`coerce_lambda_args`): the single largest remaining
  `CRATONVM_DBG_STALE_OBJREF` contributor in this session's post-JMX-fix, pre-`2d45ef40`-merge sample
  (8/20, via `native_stream_for_each`/`native_stream_all_match`/`invoke_deferred_stream_lambda`, all
  panicking on the exact same line dereferencing `obj_ref` on its very first use inside the function).
  Traced the pinning chain up through `coerce_lambda_args` and `checkcast_lambda_instantiated_args` and
  could not find an obvious staleness window in either -- `obj_ref` is read fresh from
  `thread.native_pin_roots[h]` immediately before the call, with no intervening GC-triggering step, yet
  still panics on first dereference. This function has a documented "no GC, no stale `obj_ref`" invariant
  (only consults already-loaded classes) that the evidence says is violated somewhere in its own call
  chain (`lambda_proxy_satisfies`/`synthetic_implements`/`proxy_instance_satisfies_target`/
  `annotation_proxy_satisfies_target`), or the true corruption predates entry into
  `coerce_lambda_args` entirely. Flagged prominently rather than risking an unverified fix to a function
  whose signature (`shared: &SharedVm`, no mutable pin access) does not obviously support the established
  pin/re-read idiom without a larger refactor. Worth a dedicated live-gdb session.
- **634 other `SyntheticStub` natives still get silently dropped** in real-JDK mode per a
  `CRATONVM_DBG_DROPPED_STUBS=1` census on the final binary (none crashed in the specific runs sampled
  here) -- the same long-tail pattern already documented for the stale-ObjectRef family. Not mined
  further; `CRATONVM_DBG_DROPPED_STUBS` is a cheap, permanent diagnostic (see
  `native-api/src/registry.rs`) for whoever hits the next one.

### Status

Doc stays **CLOSED / moved to `fixed-suite-bugs`** -- the specific symptom it tracks (silent boot stall,
`TIMEOUT_NO_WARN`) is re-confirmed at 0/20 `GENUINE_STALL` on current `dev`, consistent with the "Final
resolution" section. This addendum documents the same-day regression that briefly made that conclusion
untestable, the fix for it, and an additional real (if partial) contributing mechanism (the poisoning
cascade) fixed along the way. Fix commits: `6a0eedd8`/`cfd80297` (merged `dev`, `ab423500`).

## 2026-07-15 follow-up: the warning RECURRED on dev `a783d31f` (6/20 boots, `--nojit` included) — the remaining counted-blocker population was the ConcurrentHashMap segment-monitor family; FIXED

The "0 occurrences across 50+ verification attempts" above did not hold on the full-extension
standalone boot under shared-host load: plain repro batches on `a783d31f` hit the exact
`rounds=64 pending=N taken=0` warning in 3/12 JIT and 3/8 `--nojit` boots (the `--nojit` occurrences
also disprove any JIT-specific reading of the warning: `taken=0` there simply means "no JIT peers to
take over"; the wedge is purely counted cooperative mutators that never arrive).

Root-caused live with `CRATONVM_DBG_STW_CENSUS=1` + sudo gdb attach on a wedged boot
(`/data/wt-wfgc-20260715/probes/logs/GDB_hunt_*.{log,threads}` on the Azure host): every pending
thread sat in `native_chm_put → ChmMonitorGuard::acquire → NativeContext::monitor_enter →
Monitor::block_enter` — the plain, census-COUNTED monitor path — contending a ConcurrentHashMap
segment monitor whose owner was parked at the STW barrier. This is exactly the class this doc's
original fix anticipated: the 2026-07-13 change added `monitor_enter_gc_safe` but converted only the
CountDownLatch polling-loop site "with live evidence"; the CHM mutator family was the rest of the
live population.

**Fix** (branch `fix/wildfly-gc-pin-stream-20260715`, commits `7831ce2c` + `b1ac28f3`, merged to dev):
all 14 live `ChmMonitorGuard::acquire` sites now use an `acquire_gc_safe` variant
(blocking-region-protocol wait; relocated-segment return; per-site pin/re-read of every spanning
local), plus JDK-exact lock-free compute fast paths (`computeIfAbsent` present-key /
`computeIfPresent` absent-key return without the segment monitor) — the latter also dissolves a
Java-level segment-monitor ↔ registry-RWLock ordering cycle that surfaced as a *silent* wedge once
the barrier deadlock stopped masking it (real JDK bin-granularity never orders different keys against
each other; our 16-way segments do).

Post-fix: the warning appears in **0 of 49** standalone boots (12+10 chmfix, 12+10 fastpath, plus
canary batches), versus 6/20 pre-fix on the same host. Boot-completion *rates* across those batches
are not cleanly comparable (host load ranged 6→19 over the day; an 11 s boot at load 6 can blow a
90 s timeout at load 19), so the warning-marker count is the controlled metric.

Residual: the absent-key `computeIfAbsent` mapper still runs under the segment monitor (JDK runs it
under a bin lock — same semantics, coarser collision domain here). If counted-blocker wedges recur,
audit the remaining ~65 `NativeContext::monitor_enter` native call sites the same way (the census
dump + gdb-attach recipe above localizes the population in one wedged boot), or take CHM segmentation
finer.


## Recurrence 2026-07-18 — reopened

Fresh full-suite 6-shard rerun ("round 7", worktree `test/wildfly-full-suite-20260718`, `dev@7a939ec0`
base + a local fix for an unrelated same-day compile break in `native-io/src/socket_channel.rs`,
binary `frozen-cratonvm-wildfly-bugbash-v7-20260718`) reproduced this doc's exact symptom as the
dominant failure mode:

- Isolated sanity check, `org.jboss.as.test.integration.beanvalidation.BeanValidationTestCase`: 76-77s
  wall time, `LifecycleException: Could not start container`
  (`CommonManagedDeployableContainer.java:107`), `target/wildfly/standalone/log/server.log` remained
  0 bytes -- identical signature to the original filing.
- Broader sample (252/1548 classes completed before this check): 240 FAIL, of which 199 (83%) show
  "Could not start container" and 18 (7.5%) show "exited unexpectedly with code [1]" (the companion
  [[wildfly-remoting-classcastexception-parallel-extension-add]] crash path) — the same two-signature
  split this doc's "Scale" section originally described, on a binary built from `dev` well after every
  fix commit referenced in this doc's resolution history (`945e4492`, `41b06719`, `6a0eedd8`/
  `cfd80297`, `7831ce2c`/`b1ac28f3`).

Not yet re-diagnosed to a specific mechanism this time (no fresh live-gdb/CPU-census capture done in this
pass) -- reopening on the strength of the reproduction alone, consistent with this doc's own established
pattern of recurring via a new specific trigger each time. Whoever picks this up next should start with
the same CPU-activity-aware repro loop and `CRATONVM_DBG_STW_CENSUS=1`/gdb-attach recipe the prior three
resolutions used (see "Final resolution" and the 2026-07-15 follow-up above) rather than assuming it's
the same exact mechanism as any prior fix.

## New evidence 2026-07-18 (same session) — mechanism looks different this time: recurring stall, not permanent wedge

Follow-up isolated-repro batch (10 attempts, round-7 binary `frozen-cratonvm-wildfly-bugbash-v7-20260718`,
`org.jboss.as.test.integration.basic` module) to characterize this recurrence more precisely than the
initial reopening note above:

- **`pending=1`, not `pending=6`.** Every attempt that hit the warning showed exactly one uncooperative
  mutator, not six — closer to the original, already-fixed `bee86ff0` shape (`main-vm` post-`main()`)
  than the `pending=6` shape this doc originally filed. Worth checking whether `bee86ff0`'s specific fix
  has a narrow gap, rather than assuming this is the same `pending=6`/multi-worker mechanism recurring.
- **Not a permanent wedge this time.** A 30s-timeout batch (5 runs) showed the warning firing once early
  (~1s into boot) in all 5, then boot **continuing** for thousands of further log lines (deployment
  scanner `Scan complete`, `read-children-resources` management operations) — clearly past the point
  where the original bug went permanently silent. A follow-up 90s run showed the warning firing **twice**
  (12:56:05 and 12:57:14, ~69s apart, both `pending=1`), with substantial real work happening between
  occurrences, and still had not reached `WFLYSRV0025` (final started banner) by the 90s cutoff.
- **Net effect: boot is now recurringly-stalling-but-progressing, rather than permanently wedged** — it
  still never completes within any timeout tried (30s/90s isolated; Arquillian's own internal timeout in
  the real harness gives up well before that), so the practical symptom (0 OK, "Could not start
  container") is unchanged, but the underlying mechanism producing it looks different from either the
  original `pending=6` filing or the `bee86ff0` single-thread-post-main() case. Possibly a partial fix
  landed between 2026-07-15 and now that narrowed but did not eliminate the wedge duration per
  occurrence, or a different, lower-severity variant of the same class of bug.
- **CCE_CRASH did not reproduce** in this same batch (0/10 across two rounds of 5) — see the companion
  doc's new-evidence section for why this might just be a low-probability-per-attempt event rather than
  fixed, given round 7's real harness run separately observed it at 18/240 (7.5%).

Raw logs: `/tmp/r7repro1.log` .. `/tmp/r7repro10.log`, `/tmp/r7repro_long.log` on the Azure host.
