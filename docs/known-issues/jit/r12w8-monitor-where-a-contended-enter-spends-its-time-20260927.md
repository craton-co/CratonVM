# Where a contended monitorenter spends its time today (by reading)

Status: OPEN
Area: `jit/src/runtime_lowering.rs` (`emit_inline_thin_lock`, `emit_inline_inflated_arm`, `emit_inline_inflated_spin_acquire`), `vm/src/threading/monitor.rs`, `vm/src/vm/vm_exec.rs` (`monitor_enter_blocking`), `vm/src/jit/helpers.rs` (`jit_monitor_enter` / `jit_monitor_exit`), `vm/src/threading/gc_barrier.rs`
Severity: high (performance; not a wrong answer)
Found by: round 12 wave 8 lane monitor

Read on `6176aa79f` (round 12 wave 7). Nothing here was measured by the lane (no
builds, no runs). It is the walk behind the three changes this wave made or
filed, and the input for the next census run. The workloads are the ones on
`r11w15-orch-contended-monitor-throughput-80x-hotspot-CLOSED-20260929.md`:
`ThreadChurn 192 20000 1` (48 live threads, `synchronized (mine) { synchronized
(shared) { s += i & 7; } }`, 3.84M contended enters of `shared`; 1.32-1.36 s
against HotSpot's 0.49 s at w6) and `R11X64genSyncCost` `sync-block-4t` (4
threads, one lock, 4M enters; 395-572 ms against 266). The host has 32
logical CPUs, so the 48-thread runs are 1.5x oversubscribed.

## 1. The compiled path (almost every enter on both workloads)

Both tiers emit the same sequence (`emit_inline_thin_lock`). For `shared`
(INFLATED, owned by another thread):

| step | instructions | memory touched |
|---|---|---|
| thin screen: receiver, thread (TLS), armed, lock stack, `spilled`, mark | ~14 | thread-private lines; the object's header line (read) |
| inflated arm: state bits, stack room, `LeaseBlock` key, `index_epoch` compare, owner word, monitor | ~16 | the lease block (private); `IndexEpochCell` (read-shared, written only by an inflation) |
| spin (`emit_inline_inflated_spin_acquire`): `CMP [entry_waiters],0`, `PAUSE`, `CMP [owner],0`, per round | 5 per round, up to `spin_limit` rounds | owner line 0 and hand-off line 1, both READ |
| the win: `LOCK CMPXCHG [owner]` | 2 | owner line: read-for-ownership |
| `entry_count = 1` | 1 | owner line: a STORE |
| lock-stack push (seqlock: `seq` odd, slot, `top`, `seq` even) | 8 | thread-private |

and the exit: the same screen, `owner == me`, `entry_count == 1`, `jfr == 0`,
`thin_seed == 0` (owner-line reads), the wave-1 pre-check (line-1 reads),
`entry_count = 0` (an owner-line STORE), `XCHG [owner]` (owner line), the
post-check (line-1 reads), the pop.

No call, no mutex, no allocation and no clock read anywhere on this path. The
instruction count (~45 each way against HotSpot C2's ~15) is not where the
time goes: at 48 contenders the cost of a hand-over is coherence traffic.

### 1a. The owner line is written four times per critical section

Every inline spinner polls `owner` (line 0) with a plain load between
`PAUSE`s, so line 0 sits Shared in every contender's cache for the whole
critical section. The owner then writes that line up to four times:

1. the acquiring `LOCK CMPXCHG` (read-for-ownership, invalidating every copy);
2. `entry_count = 1` right after it: a spinner's poll that lands between the
   two downgrades the line, and the store needs a second RFO;
3. `entry_count = 0` at exit: by then every poller holds the line again, so
   this is an RFO that invalidates N-1 copies, inside the critical section;
4. the releasing `XCHG` (a hit if nothing polled since 3).

HotSpot's `ObjectMonitor` pads `_owner` alone and keeps `_recursions` (entries
minus one) elsewhere, so C2's fast path writes the polled line twice: the CAS
and the release store. Writes 2 and 3 are what separates the two designs on
this path. **Fix filed as an exact patch**:
`r12w8-monitor-entry-count-off-the-owner-line-patch-FIXED-20260927.md` (a layout:
`entry_count` moves to a third, owner-private line; the emitter's two
`entry_count` accesses become disp32).

### 1b. The herd at every release

The inline spin is test-and-test-and-set with no back-off: every spinner whose
poll reads the `0` the `XCHG` published issues a `LOCK CMPXCHG`. One wins; each
of the others takes line 0 exclusive in turn and fails, and those RFOs queue in
front of the winner's own accesses (its `entry_count` store, then its exit).
With 47 pollers and a critical section of a few dozen instructions, a release
can set off many failing CASes, serialised on one line, all on the critical
path. **Fix filed as an exact patch**:
`r12w8-monitor-inline-spin-lost-cas-backoff-patch-FIXED-20260927.md` (after a lost
CAS, 16 `PAUSE`s without polling, charged to the budget). The Rust spins got
the same treatment this wave (below).

### 1c. Every contender spins, and never stops because someone else won

A compiled contender spins inline for the monitor's whole `spin_limit` (up to
`SPIN_MAX` = 1024 `PAUSE`s, ~10-45 us depending on the core's `PAUSE`
latency) however many hand-overs it watches go to others; only a parked
entrant (`entry_waiters != 0`) or the budget sends it to the helper. HotSpot's
`TrySpin` aborts on a failed CAS and when it sees the owner change to another
thread, and parks, so most of a crowd sleeps and the few that spin do not
compete for the owner's line. At 48 threads on 32 CPUs every spinning
contender also takes a CPU a preempted owner needs; when that happens the
whole crowd burns its budget and falls into the helper. Not changed (a parked
contender costs far more here than in HotSpot, see 3); proposal W8-3 in
`jit-r12-lock-proposals.md`.

## 2. The helper path (budget out, a parked entrant, re-entry)

`jit_monitor_enter` (panic guard, SATB flush door, `JIT_THREAD`) ->
`monitor_enter_blocking`:

1. `arm_jit_monitor_block` (one compare), census flag;
2. `enter_or_contend`: lease slot (thread-local cache), thin CAS refused,
   `INFLATED_MONITOR_CACHE` hit (no shard lock, no `Arc`), `try_enter`, then
   `spin_try_enter_adaptive` -- a SECOND spin of the same budget, the one that
   adapts it -- then `share_inflated_with_compiled_code`;
3. `spin_try_enter`: a THIRD, fixed spin, 16 rounds of 1..256 hints (~2 300
   `PAUSE`s) then 8 `yield_now`s, re-reading `stw_requested` every round;
4. only then the JMX contention publish and the park (section 3).

So a contender that loses spins three times before it parks: inline
(`spin_limit`), adaptive (`spin_limit`), fixed (~2 300 plus yields). Each
bounded, each a counted mutator, so each delays a stop-the-world pause by at
most its length. The adaptive spin's failure halved `spin_limit`, which the
inline spin reads too; until this wave a spin that failed because it lost
races to a crowd (not because the lock was held long) halved it as well, so a
crowd shrank the inline budget and sent more of itself into the helper. Fixed
this wave (`CRATONVM_MONITOR_SPIN_BACKOFF`, below).

Where the helper releases (`monitor_exit_and_retract_jmx` ->
`Monitor::exit_reporting_release`), it wrote FOUR owner-line words where the
inline exit writes two: `entry_count`, `jfr_enter_recorded` (already false in
every release without JFR), `thin_seed` (already 0 after the first release of
a seeded monitor), `owner`. The interpreter's `monitorexit` and every
helper-path exit paid the two redundant RFOs. Fixed this wave
(`CRATONVM_MONITOR_QUIET_RELEASE`).

## 3. The park (spins all failed)

`monitor_enter_blocking` after `spin_try_enter`: `set_jmx_contended_monitor`
(two mutexes, `Instant::now`), the opt-in running park
(`CRATONVM_MONITOR_LAZY_PARK_US`, off), then the GC-blocked park: TLAB retire
(a filler into the young arena), `deposit_root_snapshot` (a FRESH
conservative scan of the thread's compiled frames, slot origins, frame trace,
the SATB flush), `GcBarrier::enter_blocked`, `Monitor::block_enter` (state
lock, register, CAS, `entry_condvar.wait`), and on the way out
`BlockedGuard::drop`, `check_post_block_gc` (`leave_blocked_region_flagged`,
the fixup, a second deposit), `complete_jmx_monitor_enter` (two more mutexes).

The GC barrier's `inner` mutex is VM-global and every park takes it THREE
times (`enter_blocked`, `BlockedGuard::drop`, `leave_blocked_region_flagged`),
as does every blocking native: a crowd that parks convoys on it. And
`check_post_block_gc_refs` evaluates `format!("fixup={}", fixup.len())` for
`remap_trace_push` on every wake although the trace is off by default (the
callee tests `remap_trace_on()` only after the `String` was built): one
allocation per park. Both outside what this lane owns (`gc_barrier.rs`, the
generic post-block helper); the second is a one-line fix for the owner of
`vm_exec.rs` (`if remap_trace_on() { .. }` around the call, `remap_trace_on`
is in `runtime/interpreter/gc_and_alloc.rs`).

The w1 census of `ThreadChurn` counted 189 parks for 3.84M enters, so on that
workload the park is not the cost; on a workload whose critical sections
outlast the three spins it is all of the cost, which is what W17-1 (the
running park, opt-in since round 11 wave 19, its pause wake wired since round
12 wave 1) is for.

## 4. The wake

`Monitor::wake_successor`: nothing unless `entry_waiters != 0 &&
!succ_pending && spinners == 0` (HotSpot's `_succ` rule), then the state lock
and one `notify_one` (parking_lot requeues the waiter onto the state mutex, so
no thundering herd there), and the woken entrant spins (`spin_after_wake`)
before it re-parks. `notify_all` on `wait_condvar` wakes every waiter, each of
which must then re-acquire through the entry protocol; HotSpot moves waiters
to its entry list without waking them. Not a cost on the probes of this page
(`ping-pong` has one waiter).

## 5. The uncontended synchronized method (`sync-method-1t`)

The monitor part of `sync-method-1t` is the thin path above minus the inflated
arm: one `LOCK CMPXCHG` each way plus ~30 instructions each way (the thin
screen, the `held` count, the seqlock push/pop). HotSpot's lightweight locking
also pays one CAS each way. The ~2x on that row is mostly not the lock:
`plain-method-1t` (the same loop without `synchronized`) is ~25 ms against
HotSpot's 3 (HotSpot inlines `incPlain`, the call is lane callcost's page),
and the `static synchronized` callee's monitor is an `Op::ConstClass` per call
(an ldc slot probe, and the `jit_ldc_class_cp` helper when the site's slot is
not recorded: `jit-r11-sync-proposals.md` W10-1). A virtual (not statically
bound) synchronized call still goes through the dispatch helper and the Rust
monitor guard (W9-3, a self-locking compiled body). No change this wave; the
probe's `sync-method-1t` rows (five timed rounds) separate warm-up from the
steady state.

## Landed this wave (lane monitor, `vm/src/threading/monitor.rs`)

* `CRATONVM_MONITOR_QUIET_RELEASE` (default on): the Rust release writes
  `thin_seed` / `jfr_enter_recorded` only when they are not already clear
  (2 owner-line RFOs fewer per helper / interpreter release).
* `CRATONVM_MONITOR_SPIN_BACKOFF` (default on): the Rust spins
  (`spin_try_enter_adaptive`, `spin_after_wake`) back off exponentially (8..128
  hints, charged to the budget) after a lost race, and a spin that ran out
  having lost a race keeps `spin_limit` instead of halving it.
* Census: `lost_cas` in `CRATONVM_DBG_MONITOR_CONTENTION` (the herd size in the
  Rust spins).
* Both switches are per VM (`MonitorTable::tuning`, copied into each
  `Monitor`), not process statics.

## What is left

1. Apply `r12w8-monitor-entry-count-off-the-owner-line-patch-FIXED-20260927.md` and
   `r12w8-monitor-inline-spin-lost-cas-backoff-patch-FIXED-20260927.md`, then measure
   (below).
2. The spinner crowd under oversubscription (1c): proposal W8-3.
3. The census does not see the inline spin (lock proposal W2L2-3); until it
   does, `adaptive_spin_*` / `lost_cas` count only the helper's share.
4. The park path's global barrier lock and the per-wake `format!` (section 3).

## How to confirm

* `cargo test -p cratonvm-vm --lib threading::monitor` (new:
  `lost_cas_backoff_windows_grow_cap_and_fit_the_room`,
  `a_quiet_release_ends_in_the_state_a_plain_one_does`,
  `backing_off_spinners_lose_no_update_and_no_wakeup`,
  `an_inflated_monitor_carries_its_tables_switches`), `vm/tests/monitor_stress.rs`.
* `C:\craton\jitr12-probes\src\R12MonitorContended.java`: every line as in its
  header, in the arms it lists.
* Timings, interleaved against the w7 binary: `ThreadChurn 192 20000 1`,
  `R11X64genSyncCost`, `R11W15LockLeaseChurn`, `R12MonitorContended`'s
  `t-contended-*` / `t-nested-48t` / `t-sync-method-*` lines; each also with
  `CRATONVM_MONITOR_SPIN_BACKOFF=0` and `CRATONVM_MONITOR_QUIET_RELEASE=0`.
* One census run (`CRATONVM_DBG_MONITOR_CONTENTION=1`) of `ThreadChurn`:
  `lost_cas` against `adaptive_spin_wins` sizes the herd the helper spins see.

## Round 13 wave 1 (lane sync)

Status stays OPEN (nothing measured by the lane). Item by item against "What is left":

1. Both wave-8 patches are applied (`entry_count` at disp32 128,
   `INFLATED_MONITOR_ENTRY_COUNT_OFFSET`; `CRATONVM_JIT_INLINE_SPIN_BACKOFF`) and their
   pages retired to `docs/internal/fixed-bugs/`. Still unmeasured: the census and
   timing runs under "How to confirm" above are the orchestrator's.
2. The spinner crowd under oversubscription (1c): not changed. The inline spin
   (`jit/src/runtime_lowering.rs` `emit_inline_inflated_spin_acquire`) has no free
   register for HotSpot's "owner changed hands, abort" test (it may clobber only RAX,
   RCX, RDX, R10, R11, all in use), so it needs a stack slot or a restructured arm;
   filed with a concrete shape as proposal S13-5 in `jit-r13-sync-proposals-RETIRED-20260929.md`
   rather than hand-encoded unmeasured.
3. The census is still blind to the inline spin: proposal S13-6 (counters must have a
   production reader, and the arm's registers are exhausted; the census belongs on
   the slow label, where the thread is re-read anyway).
4. The per-wake `format!` in `check_post_block_gc_refs`: exact patch
   `r13w1-sync-vm-exec-per-wake-remap-trace-format-patch-FIXED-20260928.md` (outside this
   lane's `vm_exec.rs` region). The VM-global `GcBarrier` lock taken three times per
   park: read, not changed. Every one of the three sites (`enter_blocked`,
   `BlockedGuard::drop`, `leave_blocked_region_flagged`) orders the blocked flag
   against a pause census under that lock (finding 1(a/c) in `check_post_block_gc_refs`);
   a lock-free version is a GC-protocol change, not a monitor one. Proposal S13-7.

Also landed this wave for the uncontended half (section 5, "A virtual (not statically
bound) synchronized call still goes through the dispatch helper"): the single-pass
instance caller-held direct CALL (`CRATONVM_JIT_SP_SYNC_DIRECT_INSTANCE`), see
`r12w8-orch-synchronized-instance-calls-and-bigdecimal-are-slow-20260927.md`, "Round
13 wave 1 (lane sync)". It takes the receiver's monitor with the inline sequence
section 1 describes, so a contended callee's enter (for a callee the route admits:
a closed body) now spins inline in the caller
instead of failing the door's uncontended acquire and falling to the by-name
`invoke_or_native` route. Probe `R13SyncInstanceThreads` (4 threads on one receiver,
mixed with `synchronized (c)` blocks, throws and allocation while held).

## Round 13 wave 6 (lane sync3)

Status stays OPEN; no change to the items above (all wait on the census and
timing runs under "How to confirm"). One addition by reading, for section 2
("re-entry"): a RECURSIVE enter never takes the inline path
(`emit_inline_thin_lock` requires a NEUTRAL mark on enter and a recursion-0 own
thin lock on exit), so nested synchronized calls on one receiver -- now common
with self-locking bodies (a self-locking callee under a caller that already
holds the receiver, the interpreter door's guard around a self-locking body) --
pay the helper both ways at every nesting level. Proposal S3-1 in
`jit-r13-sync3-proposals-RETIRED-20260929.md` (lock-stack-top recursion, HotSpot's lightweight
locking rule); the door's redundant hold is the exact patch
`r13w6-sync3-env-cache-door-guard-skip-patch-FIXED-20260928.md`.

## Round 13 wave 8 (lane sync5)

Status stays OPEN (nothing measured by the lane; items 2-4 of "What is left"
unchanged). The re-entry gap the wave-6 section named is closed for THIN
locks: `emit_inline_thin_lock` (both tiers) now sends a word thin-locked by
this lease to a recursion arm (`runtime_lowering.rs`
`emit_inline_thin_recursion_arm`, `CRATONVM_JIT_INLINE_THIN_LOCK_RECURSION`,
default on) before the inflated arm: one `LOCK CMPXCHG` of the mark's 3-bit
recursion field each way, exactly the helper's `try_thin_recursive_lock` /
`try_thin_unlock`, no lease-count or lock-stack write (the lock stack is a set
of held objects, not a recursion record). HotSpot's lightweight locking pushes
the object again instead; the VM's thin word already carries the count, so the
existing lock-stack and JMX readers need no change. A re-entry at
`MAX_THIN_LOCK_RECURSION` (7) still takes the helper, which inflates, and a
recursive exit whose lock-stack top names another object still takes the
helper. An INFLATED re-entry (`owner == me` in the inflated arm) is inline
too, under its own switch (`CRATONVM_JIT_INLINE_INFLATED_RECURSION`, default
on): `ADD DWORD [monitor + entry_count], 1`, and a non-final exit (count > 1)
`SUB`, exactly `Monitor::try_enter`'s owner arm and `exit_reporting_release`'s
`count > 1` arm -- no owner-word write, no wake decision, JFR flag and thin
seed ignored as the helper ignores them there; count 0 stays the helper's
IMSE. The count sits on the owner-private third line
(`INFLATED_MONITOR_ENTRY_COUNT_OFFSET`), so no spinner's line moves.
Tests: `runtime_lowering::tests::inline_thin_lock_recursion_arm_counts_in_the_mark_word_only`,
`inline_inflated_recursion_moves_only_the_entry_count`.
Probe: `C:\craton\jitr13-probes\src\R13Sync5Reentrant.java` (nesting on a
private object, a non-top recursive exit, a shared object recursing past the
thin limit under 4-way contention, a static self-recursion on the mirror).

## Round 13 wave 10 (lane monitor2)

Status stays OPEN (nothing built or run by the lane). Item by item against
"What is left", then what this wave added outside those items.

2. **The spinner crowd (1c), S13-5: not implemented, and not as written.** An
   inline spinner that aborts on an ownership change goes to the helper,
   where `spin_try_enter_adaptive` (same budget, no abort) and
   `spin_try_enter` (~2 300 hints, 8 yields) spin again before any park, so the
   crowd would move its spinning into Rust (plus a helper call each) rather
   than park. HotSpot's crowd parks because its park costs a futex; this VM's
   costs the GC-blocked protocol (section 3). The order that can work is a
   cheaper park first -- `CRATONVM_MONITOR_LAZY_PARK_US` by default, which is
   now sound to consider since both production stop-the-world initiators wake
   running parkers (`gc_and_alloc.rs`: `request_non_collection_pause` and the
   collection initiator call `MonitorTable::wake_lazy_parkers`; no third
   production caller of `request_stw*` exists) -- and then an abort in the
   RUST spins. Proposal `jit-r13-monitor2-proposals-RETIRED-20260929.md` M2-2, gated on M2-1.
3. **The census is still blind to the inline spin.** It needs two
   `JitMonitorBlock` words (`jit-api`, not this lane's crate); the exact shape
   is M2-1. Until then `adaptive_spin_*` and `lost_cas` count the helper's
   spins only.
4. Unchanged (GC protocol; S13-7).
5. New by reading, section 1a: the `entry_count` line still moves to every
   new owner (written `= 1` after the acquiring CAS and `= 0` before the
   releasing `XCHG`; the `XCHG` drains that store). HotSpot's `_recursions`
   (entries minus one) is not written by a hand-over at all. A representation
   change both crates must agree on, so it is proposal M2-5 with a
   per-monitor representation byte that makes a compile/runtime mismatch
   impossible, not a blind edit.

### Landed (each with a kill switch)

* `CRATONVM_MONITOR_WAIT_REACQUIRE_SPIN` (default on, per VM through
  `MonitorTuning`): `Object.wait()`'s re-acquisition. Section 4's walk assumed
  one waiter per ping-pong and missed the re-entry: a notified waiter whose
  notifier had not left the `synchronized` block yet (the normal case: `notify`
  is almost always followed by the block's `monitorexit`) found the monitor
  owned, registered in `entry_waiters` and parked on `entry_condvar` -- a
  second OS wake-up for the same hand-over, and the notifier's compiled exit
  saw the registered entrant (the wave-1 pre-check) and went to the helper to
  issue it. The waiter now spins first (`Monitor::spin_for_free`: unregistered,
  state lock released, the adaptive budget, the lost-race back-off), and a
  woken re-acquirer spins before it re-parks in `enter_labeled`'s order (spin
  with `succ_pending` set, clear it, CAS under the lock). An unregistered
  spinner owes and suppresses no wake-up, so `wake_successor`'s argument is
  untouched. The waiter is GC-blocked throughout; the spin touches only the
  monitor, which its `Arc` keeps alive, and never the heap. `Thread.getState()`
  reads BLOCKED from the first failed CAS, spin included (HotSpot's
  `wait_reenter_begin`). Census `wait_reacquire_spin_wins`.
* `CRATONVM_MONITOR_CACHED_NOTIFY` (default on, per VM): `holds` (so
  `Thread.holdsLock`, and the IMSE re-check of every failed exit), `notify` and
  `notifyAll` of an INFLATED object read the monitor from the thread's
  `INFLATED_MONITOR_CACHE` (`MonitorTable::cached_monitor_of_inflated`), which
  `exit_reporting_release` has used since round 11 wave 16 under the same
  conditions. Before, each call took the object's index shard lock and cloned
  and dropped the monitor's `Arc` (two RMWs on the line in front of the
  monitor). A miss (or the switch off) takes the index as before.
* `CRATONVM_JIT_INLINE_INFLATED_TWO_WAY` (default on, compile time, per site;
  lock proposal W17-3): the lease cache compiled code reads has a second way
  (`LeaseBlock::inflated_key2` / `inflated_monitor2`, offsets 48 / 56, pinned
  both sides). The VM keeps the entry a fill displaces there only when it was
  valid at the SAME epoch as the new one (the block keeps one epoch for both
  ways) and names another object; an entry keeps exactly the `(key, monitor,
  epoch)` it was recorded with, so `cached_inflated_monitor`'s liveness
  argument covers both ways. The arm probes way 1, then way 2 (three
  instructions more on a way-1 miss only), then checks the one epoch as
  before. Switch off: the arm's bytes are the wave-9 bytes.

### How to confirm

* Unit tests: `threading::monitor` --
  `a_wait_notify_ping_pong_hands_over_exactly_in_both_reacquire_arms`,
  `a_reacquire_spin_that_runs_out_still_parks_and_is_woken`,
  `holds_and_notify_answer_alike_through_the_cache_and_the_index`,
  `the_lease_cache_keeps_the_displaced_entry_as_its_second_way_at_one_epoch`;
  `runtime_lowering` --
  `inline_inflated_arm_finds_its_monitor_in_either_way_of_the_lease_cache`.
* `R13Monitor2WaitNotify` (`t-ping-pong`, `t-ping-pong-nested`, `t-ring-4`,
  `t-prodcons`) default vs `CRATONVM_MONITOR_WAIT_REACQUIRE_SPIN=0`, and
  `R13Monitor2Contended` `t-two-lock-*` vs `CRATONVM_JIT_INLINE_INFLATED_TWO_WAY=0`,
  interleaved (`bench13.sh`-style, 3+ reps). One
  `CRATONVM_DBG_MONITOR_CONTENTION=1` run of `R13Monitor2WaitNotify`:
  `wait_reacquire_spin_wins` should be a large share of the ~120 000 turns
  (ping-pong, nested, ring), and `condvar_waits` / `successor_wakes` should
  fall against the switch-off run.

## Round 13 wave 12 (lane monitor3)

Status stays OPEN (nothing built or run by the lane). Item 5 of the wave-10
list above -- the entry count's line moving to every new owner -- landed, in a
form that needs no representation byte; the two lock-stack/self-lock cuts
lane sync7 proposed (S7-4, S7-5) landed with it. Each behind its own switch,
read per compile site (compiled code) and needing no VM-side switch.

* **The compiled hand-over writes the owner word only**
  (`CRATONVM_JIT_INFLATED_STICKY_COUNT`, default on;
  `jit/src/runtime_lowering.rs` `emit_inline_inflated_arm`,
  `vm/src/threading/monitor.rs`). The inflated arm's final release no longer
  stores `entry_count = 0`, and its acquisition stores `1` only when it does
  not read `1` already, so between two compiled critical sections the count's
  line (disp 128) stays Shared in every contender's cache instead of being
  taken for ownership by each new owner -- a store the releasing `XCHG` had to
  drain, inside the critical section (1a's writes 2 and 3; HotSpot's
  `_recursions` is not written by a hand-over either). The count is exact
  while the monitor is owned (every acquisition makes it 1: the compiled arm,
  or `Monitor::try_acquire_free`, which now also skips a store of the 1
  already there) and only the owner writes it; the two readers that look at a
  monitor they do not own now ask the owner word first: `Monitor::is_idle`
  (the prune predicate) is `owner == 0` -- on every Rust path that clears the
  owner the count was already 0, so the old conjunction answered the same --
  and `MonitorTable::entry_count` reads unowned as 0
  (`Monitor::held_entry_count`). No fence moved. Monitor2's M2-5 argued for a
  per-monitor representation byte so compiled code and a monitor could not
  disagree; here they cannot disagree, because both representations of an
  unowned count (0 from a Rust release, `wait` or a forced release; 1 from a
  compiled release) mean "unowned" to every reader, and sites compiled with
  and without the switch interoperate. Tests:
  `runtime_lowering::tests::the_sticky_count_is_exact_while_owned_and_left_at_one_by_a_release`,
  `monitor::tests::a_count_left_at_one_by_a_compiled_release_reads_as_unowned`.
* **The lock-stack push / pop is two stores and one `seq += 2`** (proposal
  S7-4, `CRATONVM_JIT_LOCK_STACK_SINGLE_BUMP`, default on; both tiers, every
  compiled `monitorenter` / `monitorexit`, thin and inflated): one load, one
  store and one ALU op fewer each, `seq` at the same value after every write.
  The cross-thread reader (`JmxLockStack::snapshot`) is unchanged except that
  its `top` load is `Acquire` (a plain `MOV` on x86 either way); the argument
  that it still sees only held sets -- each compiled write is consistent at
  every prefix, and the trailing bump makes it retry across a second write --
  is on `lock_stack_single_bump_enabled`. Tests:
  `runtime_lowering::tests::a_single_bump_lock_stack_write_ends_where_the_bracket_did`,
  `thread_registry::jit_inline_lock_stack_tests::a_single_bump_writer_never_shows_a_reader_a_torn_set`
  (a racing reader against the modelled writer), and the helper/compiled
  interleaving test now alternates both shapes.
* **A self-locking body's own enter and release have no null test**
  (proposal S7-5 (a), `CRATONVM_JIT_SELF_LOCK_TRIM`, default on;
  `runtime_lowering::emit_inline_self_lock`, used by `x64/frames.rs`
  `emit_self_lock_inline` and, for the release at a `*return`, by
  `x64/op_object.rs` `emit_monitor_on_stack_top_with`). S7-5 (b), one thread
  load instead of two, was not done: with no register free across the CAS the
  only reshuffle (the lease pointer loaded early, `top` re-read after the CAS)
  trades the re-load of the thread for a re-load of `top` -- the same number
  of loads. Proposal M3-3 names what would make it worth doing. Test:
  `runtime_lowering::tests::a_non_null_receiver_sequence_drops_only_the_null_test`.

What is left, unchanged: items 2-4 (the crowd under oversubscription, the
census's blindness to the inline spin, the park path's barrier lock), all
waiting on the census run (`jit-r13-monitor2-proposals-RETIRED-20260929.md` M2-1).

### How to confirm (orchestrator)

* `cargo test -p cratonvm-jit --lib runtime_lowering` (the three new tests;
  the older executed thin/inflated tests pin the sticky count OFF in their
  probe builder and pass under either lock-stack shape),
  `cargo test -p cratonvm-vm --lib threading::monitor`,
  `cargo test -p cratonvm-vm --lib threading::thread_registry`,
  `vm/tests/monitor_stress.rs`.
* Probes (`C:\craton\jitr13-probes\src`, expected output in each header):
  `R13Monitor3ContendedRecursion` (contended inflated re-entry, `wait` at
  depth, the plain compiled hand-over; arms: default,
  `CRATONVM_JIT_INFLATED_STICKY_COUNT=0`, `CRATONVM_JIT_INLINE_INFLATED_RECURSION=0`,
  `CRATONVM_JIT_INLINE_INFLATED_LOCK=0`, `CRATONVM_MONITOR_FASTPATH=0`,
  `--nojit`, `-XX:+UseG1GC`), `R13Monitor3LockStackRace` (JMX snapshots
  racing a compiled owner; default and `CRATONVM_JIT_LOCK_STACK_SINGLE_BUMP=0`),
  `R13Monitor3Uncontended` (per-shape timings in five rounds).
* Timings, interleaved: `R12MonitorContended` `t-sync-method-4t` /
  `t-contended-4t` / `t-contended-16t` and `R13Monitor3ContendedRecursion`
  `t-handoff-4t` against `CRATONVM_JIT_INFLATED_STICKY_COUNT=0` (the
  hand-over); `bench13.sh` `SyncM ns` and `R13Monitor3Uncontended` against
  `CRATONVM_JIT_LOCK_STACK_SINGLE_BUMP=0` and `CRATONVM_JIT_SELF_LOCK_TRIM=0`
  (a few ns per monitor op at most; expect noise-level on SyncM).

## Round 14 wave 1 (lane sync)

Status stays OPEN (nothing built or run by the lane).

**Item 3 ("the census does not see the inline spin"): landed** (proposal M2-1 of
`jit-r13-monitor2-proposals-RETIRED-20260929.md`, reshaped so it needs no `jit-api` or `jvm_thread.rs` change).
The three outcomes of compiled code's inline spin are counted ON THE MONITOR, in three `u64` words
on the entry count's line (`Monitor::census_inline_spin_wins` / `_budget_outs` /
`_waiter_exits`, offsets 136/144/152, pinned both sides as
`INFLATED_MONITOR_CENSUS_SPIN_*_OFFSET`). `emit_inline_inflated_spin_acquire` bumps them with one
`LOCK ADD QWORD [RCX + disp32], 1` per outcome (RCX is the monitor on all three edges) only on
sites compiled with the census flag (`CRATONVM_DBG_JITC`, the flag the thin/helper census already
uses); without it not a byte changes (test
`runtime_lowering::tests::the_inline_spin_census_bumps_each_outcome_word_once`). The helper's own
spin (`Monitor::spin_try_enter_adaptive`, where every compiled budget-out and waiter exit lands)
drains them into the process census (`Monitor::drain_inline_spin_census`), and the
`[MONITOR-CONTENTION] EXIT` line now ends with `inline_spin_wins=`, `inline_spin_budget_outs=`,
`inline_spin_waiter_exits=` (test
`monitor::tests::the_inline_spin_census_words_drain_only_under_the_census`). Wins of a monitor no
thread later takes to the helper are not drained: an undercount at the uncontended end only. The
census run perturbs what it counts (contenders share the words), as the helper census already
does; read counts from it, never timings.

**The census run all of items 2 and 4 wait for** (M2-1's measurement), with a binary of this wave:

    CRATONVM_DBG_JITC=1 CRATONVM_DBG_MONITOR_CONTENTION=1 cratonvm ThreadChurn 192 20000 1
    CRATONVM_DBG_JITC=1 CRATONVM_DBG_MONITOR_CONTENTION=1 cratonvm R11W15LockLeaseChurn
    CRATONVM_DBG_JITC=1 CRATONVM_DBG_MONITOR_CONTENTION=1 cratonvm R12MonitorContended

(`CRATONVM_DBG_JITC` is noisy on stderr; grep `MONITOR-CONTENTION`.) What decides: `inline_spin_wins`
against `inline_spin_budget_outs + inline_spin_waiter_exits` sizes how much of the crowd the inline
spin serves; `inline_spin_budget_outs` against `adaptive_spin_wins` / `blocked_parks` says whether
the helper's second and third spins earn anything (M2-2); `lost_cas` against
`inline_spin_budget_outs` sizes the herd.

Items 2 (the crowd, M2-2) and 4 (the barrier lock, S13-7): unchanged, gated on that run.

## Round 14 wave 3 (lane monitor)

Status stays OPEN (nothing built or run by the lane). Items 2 (the crowd under
oversubscription, M2-2) and 4 (the park path's VM-global barrier lock, S13-7) are unchanged and
still gated on the census run of the wave-1 section above; the contended enter/exit code was not
changed this wave. Re-read on `20a1dbb4f` for what the round-13/14 synchronized-method work
changed here: a self-locking body, a deopt hand-over and a spliced synchronized callee all take
and release the monitor through the same inline thin/inflated sequence
(`emit_inline_thin_lock` / `emit_inline_inflated_arm`) this walk describes, so sections 1-4 still
describe a contended synchronized METHOD as well as a block; section 5 (the uncontended
synchronized method) is the one those rounds moved, and it is tracked on
`r12w8-orch-synchronized-instance-calls-and-bigdecimal-are-slow-20260927.md`.

Changed this wave next to section 4 (the wake), on the `Object.wait()` side:
`notify()` now wakes one waiter instead of every waiter on the monitor (M2-3,
`CRATONVM_MONITOR_NOTIFY_ONE_WAITER`), and an idle waiter polls 10 times a second instead of 200
(M2-4, `CRATONVM_MONITOR_WAIT_POLL_BACKOFF`). Neither touches `entry_condvar`, the successor
protocol or the park. The census run should add `R14MonitorContended`
(`C:\craton\jitr14-probes\src`), whose `waitnotify-4t` phase is the only wait/notify shape in
the census set.

## Round 14 wave 4 (lane monitor2)

Status stays OPEN (nothing built or run by the lane).

**This page now carries `r11w15-orch-contended-monitor-throughput-80x-hotspot-CLOSED-20260929.md`**
(set `CLOSED-PENDING` this wave with a pointer here; nothing moved). Its measured gap -- ~2.6x
HotSpot on `ThreadChurn 192 20000 1`, ~1.8x on `R11W15LockLeaseChurn`, `R11X64genSyncCost`
`sync-block-4t` -- is the one this page's items are for; record the next measurement of those
three here.

Items 2 and 4 of "What is left", re-read on `20a668e12`:

* **Item 2 (the crowd under oversubscription, M2-2): not doable by reading.** Unchanged since
  round 13 wave 10's analysis: aborting the inline spin on an ownership change only moves the
  crowd into the helper's two Rust spins unless the park gets cheaper first (the lazy park by
  default), and whether the helper's spins earn anything is exactly what the round-14-wave-1 census
  run measures (`inline_spin_budget_outs` against `adaptive_spin_wins` / `blocked_parks`). Still
  gated on that run.
* **Item 4 (the park path): half done, half GC-owned.** The per-wake `format!("fixup={}", ..)`
  that section 3 found in `check_post_block_gc` is gone: the call is now wrapped in
  `if remap_trace_on() { .. }` (`vm/src/vm/vm_exec.rs` ~9685), so a park allocates nothing for the
  trace when it is off. What remains is the VM-global `GcBarrier::inner` mutex taken three times
  per park (`enter_blocked`, `BlockedGuard::drop`, `leave_blocked_region_flagged`): `gc_barrier.rs`
  is GC-owned, and whether it convoys is again the census's question (`blocked_parks` per enter on
  `ThreadChurn` was 189 in 3.84M at round 12 wave 1, so not on that workload). S13-7 unchanged.

Landed this wave on the `Object.wait()` side (section 4, the wake; no effect on the contended
enter/exit workloads, which never wait): `jit-r14-monitor-proposals.md` MON14-1 (an interrupt wakes
only its target's wait-set entry, `CRATONVM_MONITOR_INTERRUPT_WAKES_TARGET`; callers switch over
with `r14w4-monitor2-interrupt-wakes-only-its-target-callers-patch-FIXED-20260929.md`), MON14-2 (the
interrupt flag re-read under the state lock right after enrolment,
`CRATONVM_MONITOR_WAIT_ENROL_INTERRUPT_CHECK`) and MON14-4 (a waiter parks once per remaining
timeout, capped at a 1 s safety slice, `CRATONVM_MONITOR_WAIT_SINGLE_PARK`). An idle untimed
waiter now wakes once a second (was ten times, and 200 before round 14 wave 3); a `wait(ms)` of
up to a second wakes once, at its deadline. Probe `C:\craton\jitr14-probes\src\R14Monitor2InterruptOne.java`.

Round 14 wave 5 (lane monitor2) correction to the paragraph above: the safety slice is now 250 ms
(an idle untimed waiter wakes 4 times a second; a `wait(ms)` of up to 250 ms wakes once), and the
interrupt handshake is SeqCst (RV5-1).

## Round 14 wave 6 (lane monitor3)

Status stays OPEN (nothing built or run by the lane). Item 2 (the crowd, M2-2) is now fully
measurable and its second half is landed as an opt-in arm; item 4 is unchanged (GC-owned,
S13-7). All in `vm/src/threading/monitor.rs`.

**Census hooks for M2-2** (`CRATONVM_DBG_MONITOR_CONTENTION`, appended to the `EXIT` line). The
existing counts said how often each spin won or failed, not whether an abort-on-hand-over (HotSpot's
`TrySpin` rule, S13-5/M2-2) would cost wins, nor how long a park lasts:

* `spin_wins_after_handover` / `spin_fails_after_handover`: a Rust contended spin
  (`spin_try_enter_adaptive`, `spin_try_enter`) that won / failed AFTER watching the monitor change
  hands (a lost CAS, or the owner word naming a different thread on two consecutive held polls,
  `HandoverWatch`). The first is exactly the set of wins an abort would turn into parks; if it is
  small against the second, the abort is cheap.
* `spin_handover_aborts`: spins the opt-in arm below ended.
* `entrant_wait_us` plus `entrant_waits_under_100us` / `_under_1ms` / `_under_16ms` / `_longer`: how
  long a registered entrant waits in `Monitor::enter_labeled` (the GC-blocked park's condvar loop, and
  JNI's `block_enter`), register to acquire; one clock read at each end, census only. This is what a
  default `CRATONVM_MONITOR_LAZY_PARK_US` must cover (the `_under_16ms` bucket is one Windows clock
  tick, the shortest the lazy park's timed wait gets there).

With the census off the spins do no extra work (the watch is fed only under the census or the
arm; the default spin loads the owner word exactly as often as before).

**M2-2's second half, opt-in: `CRATONVM_MONITOR_SPIN_HANDOVER_ABORT=1`** (per VM,
`MonitorTuning::spin_handover_abort`, default OFF). Both Rust spins give up at the first hand-over
they watch, and an adaptive spin that aborted keeps its budget (it saw a release). **Inert unless
`CRATONVM_MONITOR_LAZY_PARK_US` is also set**: the proposal's order is "a cheaper park first", and an
abort into the GC-blocked park would re-open the TLAB-retire thrash the fixed spin exists for
(`GenR4W4HeapFullThrashProbe`). With the switch off, or the lazy park off, every spin decision is
byte-for-byte the wave-5 one (the budget-halving rule included). `MonitorTuning` is now 10 bytes (the
layout assertion moved 9 -> 10; still inside the owner line's pad). Not done: the inline (compiled)
spin still runs its whole budget -- it has no free register for the owner-change test (S13-5) -- and
after an aborted adaptive spin the helper still runs `spin_try_enter` (which, under the arm, aborts at
its own first observed hand-over); telling it the first spin aborted needs a `vm_exec.rs` change,
proposal MC3-1 in `jit-r14-monitor3-proposals.md`.

Tests (`cargo test -p cratonvm-vm --lib threading::monitor::tests::monitor3`):
`monitor3_the_handover_watch_sees_only_an_owner_changing_threads`,
`monitor3_an_aborting_spin_leaves_at_an_owner_change_and_keeps_its_budget` (a flipper thread moves
the owner word between two other threads; abort off halves the budget exactly as before, abort on
leaves and keeps it), `monitor3_entrant_waits_fall_in_their_named_buckets`.

**The run that decides items 2 and M3-5** (add to the wave-1 census set above):

    CRATONVM_DBG_JITC=1 CRATONVM_DBG_MONITOR_CONTENTION=1 cratonvm R14Monitor3Crowd
    ... the same with CRATONVM_MONITOR_LAZY_PARK_US=100000
    ... the same with CRATONVM_MONITOR_LAZY_PARK_US=100000 CRATONVM_MONITOR_SPIN_HANDOVER_ABORT=1
    ... and ThreadChurn 192 20000 1, R11W15LockLeaseChurn in the same three arms

then the `t-*` timing lines of `C:\craton\jitr14-probes\src\R14Monitor3Crowd.java` interleaved in
those arms against HotSpot. Flip the lazy park (and then the abort) only if `short-48t` /
`ThreadChurn` improve and `short-16t` / `long-8t` do not regress; `entrant_waits_*` picks the budget.
