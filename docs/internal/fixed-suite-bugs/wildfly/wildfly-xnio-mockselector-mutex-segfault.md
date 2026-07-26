# SIGSEGV in Mutex&lt;bool&gt;/Condvar::wait_timeout drop path — dominant crash once WildFly actually runs under CratonVM

Status: PARTIALLY FIXED 2026-07-07 — the SIGSEGV this doc documents is fixed on `dev` (`60079fc4`,
see the correction below); a SEPARATE, still-OPEN GC-barrier boot-hang was also found during this doc's
own investigation (see "Reproduction attempt" section below) and remains unresolved, so this doc stays
in `known-issues` rather than moving to `internal`.
Severity: **Critical for this suite's signal** — the single dominant failure mode once the harness bug
(container.java.home falling back to real JDK) was fixed; hit ~64% of all classes attempted in a ~200-class
sample of round 2 (128/200 classes CRASH, all exit code 139, all at the identical code offset).
First confirmed: 2026-07-07, Azure worktree `test/wildfly-full-suite-20260707`, dev@37efdc4a (round-2 binary)

## ✅ FIXED 2026-07-07 ~16:40 — the SIGSEGV is fixed; it was a JIT arg-decode bug, not confirmation of the A4 register-oop-bitmap gap

The section immediately below ("CONFIRMED ... SAME BUG as the Elytron A4 register-only-oop crash")
correctly identified that this doc's crash, the Elytron doc's crash, and the (also same-day)
`wildfly-infinispan-remove-listener-segfault.md` crash are all the byte-for-byte identical backtrace
(`read_string <- native_builder_set <- safe_native_call <- invoke_or_native <- jit_invoke_virtual_mic`).
It did NOT yet have a fix, and its "A4 register-only-oop" attribution has since been revised (not
confirmed): the true mechanism was a much narrower, already-fixed decode-time validation gap, not the
general register-oop-bitmap gap `fork6-fjp-multithread-jit-root-reclamation.md` tracks.

`jit_invoke_virtual_mic`'s inline `decode_values` closure (`../../../../vm/src/jit/helpers.rs`) decoded a raw `i64`
call-argument slot into an `ObjectRef` whenever the callee's descriptor said `L`/`[` and the bits merely
LOOKED like a plausible pointer (8-byte aligned, under the 48-bit canonical ceiling) -- it never checked
the bits were an actual live heap address, unlike its own sibling `decode_dispatch_values` a few hundred
lines above, which already calls `vm.heap.is_object_address()` for the identical decode.
`org.xnio.OptionMap$Builder.set(Option, Object)` -- which fires on essentially every managed-container
boot via XNIO worker/channel setup, explaining this doc's 64% suite-wide crash rate -- passes numeric
options (read/write timeouts, keepalive intervals) as boxed `Long`s. Whenever one of those primitive
longs reached this decode path unboxed, a round millisecond value like 60000 or 120000 is ALSO
8-byte-aligned and well under 2^48, so it passed the old "plausible pointer" check trivially --
`ObjectRef::from_raw` fabricated a bogus reference into unmapped memory, and `native_builder_set`'s
`ctx.read_string(s)` a few instructions later dereferenced its header. SIGSEGV.

The fault addresses this doc itself recorded (`0xea60`=60000, `0x1d4c0`=120000) are the tell: they are
exactly XNIO's millisecond timeout constants, not addresses that used to be valid and got relocated by a
concurrent GC. Fixed on `dev` (`60079fc4`) by requiring actual heap membership
(`vm.heap.is_object_address`) instead of bit-pattern plausibility, matching the already-correct sibling
decode path. **This does NOT touch `OopMapEntry` or any register/safepoint tracking machinery** -- the
general A4 register-oop-bitmap gap this doc's "CONFIRMED" section below points to is very likely still a
real, separate, open concern; this fix only closes the `native_builder_set`/`OptionMap.Builder.set`
manifestation of a SIGSEGV that turned out to have a simpler cause.

**Verification**: 18+ consecutive clean repro attempts across two independently-built binaries (zero
SIGSEGV, versus the doc's own documented near-100% pre-fix crash rate on the same repro shapes); a
30-class `testsuite/integration/basic` regression slice on the fixed binary showed 0 CRASH / 0 ABEND
(empty `crashes.log`, no kernel segfaults). Full details and the corrected root-cause writeup:
`wildfly-infinispan-remove-listener-segfault.md` and
`wildfly-elytron-remoting-segfault-post-keyfactory-fix.md`.

**This doc stays in `known-issues`, not `internal`, because of the SEPARATE finding below** ("Reproduction
attempt 2026-07-07 -- current dev HANGS at boot") -- a GC-barrier blocked-region-transition deadlock that
is NOT fixed by this change and remains a real, open blocker for exercising WildFly-under-CratonVM at all.

## ✅ CONFIRMED 2026-07-07 ~16:15 — SAME BUG as the Elytron A4 register-only-oop crash; the "Coordination note" below (marked resolved ~15:26) is WRONG and is retracted

Built an independent repro of THIS doc's own crash (not borrowed from the Elytron investigation) using
the round-2 binary (`frozen-cratonvm-wildfly-bugbash-v2-20260707`) against `testsuite/integration/basic`
classes already known to crash (e.g. `org.jboss.as.test.integration.ejb.security.EJBSecurityTestCase`).

**Methodology note — this crash is a heisenbug under a live gdb wrapper.** Wrapping the whole process in
`gdb -batch -ex run ...` (the technique [[wildfly-elytron-remoting-segfault-post-keyfactory-fix]] used
successfully for its own crash, including the `handle SIGUSR1/SIGUSR2 nostop noprint pass` fix for
CratonVM's internal use of those signals) reproduced **zero crashes across 9 attempts** (2 batches, all
previously-confirmed-crashing classes from `crashes.log`) — gdb's overhead changes thread scheduling just
enough to close the race window. Switching to **raw execution + kernel core dumps** reproduced on the
**first attempt**: `sudo sysctl kernel.core_pattern=/abs/path/core.%e.%p.%t`, `ulimit -c unlimited` in the
same shell invoking `mvnw`, `-Djvm=<dir>/bin/java.exe` pointing at a plain passthrough wrapper
(`exec <cratonvm-binary> "$@"`, no gdb), then `gdb <binary> <corefile>` post-mortem. **Prefer this over a
live gdb wrapper for any future JIT-timing-sensitive race on this host.**

**The core-dump backtrace:**

```text
Program terminated with signal SIGSEGV, Segmentation fault.
#0  <cratonvm_vm::vm::vm_exec::NativeContextImpl as cratonvm_native_api::registry::NativeContext>::read_string ()
#1  cratonvm_native_builtins::xnio_async::native_builder_set ()
#2  cratonvm_vm::vm::vm_exec::safe_native_call ()
#3  cratonvm_vm::vm::vm_exec::invoke_or_native ()
#4  cratonvm_vm::jit::helpers::jit_invoke_virtual_mic ()
#5+ (unwinder garbage through JIT-generated code -- no debug/unwind info emitted for JIT'd code)
```

This is **byte-for-byte identical** (same functions, same call order) to the backtrace in
[[wildfly-elytron-remoting-segfault-post-keyfactory-fix]], which was root-caused there to the **A4
register-only-oop family**, tracked centrally in [[fork6-fjp-multithread-jit-root-reclamation]]: a live
oop (a `String` `ObjectRef`) held only in a register — not covered by frame-slot maps or conservative
stack scanning — goes stale when GC relocates/reclaims it across a safepoint that isn't a JIT
call-safepoint; any later use of that register value is a dangling-pointer read. `native_builder_set`
(`native-builtins/src/xnio_async.rs:859`, the native override for `org.xnio.OptionMap$Builder.set(Option,
Object)`) calls `ctx.read_string(s)` on the stale `ObjectRef` with no allocation in between — exactly the
shape the CORRECTION section below predicted from addr2line alone, before either doc had a real backtrace.

**Bisection confirms it independently**: `CRATONVM_DISABLE_JIT=1` (interpreter-only) eliminates the crash
on all 3 classes retried (`EJBSecurityTestCase`, `MDBRoleTestCase`, `BasicGZIPTestCase`) — each ran to a
normal (non-crash) test failure instead of SIGSEGV. Matches the Elytron doc's own bisection result
exactly (same flag, same outcome).

**This retracts the "Coordination note, resolved 2026-07-07 ~15:26" section below.** That note concluded
the two crashes were different mechanisms by comparing the Elytron investigation's confirmed backtrace
against THIS doc's still-unconfirmed addr2line/nm-only theory. With a real backtrace from this doc's own
repro now in hand, they are conclusively the **same bug**. The CORRECTION section's "native-handle UAF /
garbage `self` pointer" read was directionally useful (correctly ruling out `MockSelector` and a literal
`Mutex`-lifetime bug) but had not yet identified the actual mechanism — only a live/core backtrace could.

**Why this raises the stakes:** this is not a niche path hit by one Elytron/remoting test — it is the
*same* gap causing **64% of the entire WildFly `integration/basic` suite to crash outright**, because
`OptionMap.Builder.set()` fires pervasively during XNIO worker/channel setup on every managed-container
boot, not just Elytron-specific code paths. The A4 register-oop-bitmap gap (`jit/src/lib.rs:52-73`,
`OopMapEntry`) is very likely the single highest-leverage fix available for this suite's signal right now.

**Not fixed here** — per the Elytron doc's own assessment this needs a real JIT codegen feature (tracking
register-resident live oops at every safepoint, not just frame-slot-resident ones), too large/risky to
implement blind in a triage session. Whoever picks up the A4 register-oop-bitmap project now has **two**
independent, real-world (non-synthetic), highly-reproducible verification lanes — this doc's `basic`-module
repro and the Elytron `manualmode` repro — in addition to the existing synthetic `Fork6Hard` lane.

**⚠️ Caveat, do not skip:** [[fork6-fjp-multithread-jit-root-reclamation]] records that a separate, much
deeper investigation empirically **refuted** "register-invisible oop, fixed by precise JIT maps" as A4's
actual mechanism on `Fork6Hard`'s own canonical repro (precise maps, fullstack scan, register harvest, and
a conservative operand-stack scan all failed to fix it; more root coverage made it *worse*). Also, neither
this crash nor the Elytron one goes through ForkJoinPool/GC_STRESS at all — both are a single JIT-compiled
virtual-dispatch-to-native call. Same crash **site**, but the **mechanism** may not be the same as
`Fork6Hard`'s FJP-worker-publish-gap finding. Verify which mechanism actually applies here before building
a register-oop-bitmap fix.

## ⛔ CORRECTION 2026-07-07 — MockSelector is REFUTED as the crash site (it is `#[cfg(test)]`)

Investigated on current dev (`4e6dc36d`). The doc's "best source-level match",
`MockSelector`, **cannot** be the crash: it lives inside
`#[cfg(test)] mod tests` (`native-builtins/src/xnio_io_thread.rs:1440`), so it
is **physically absent from the release binary** — every `MockSelector::new()`
is at line 1472+ inside that test module. The addr2line symbol
`drop_in_place<PoisonError<(MutexGuard<bool>, WaitTimeoutResult)>>` is
LLVM-merged drop-glue, and the merge is wider than the doc guessed: **all
`std::sync::MutexGuard<T>::Drop` monomorphizations are identical machine code**
(they call the same `sys::Mutex::unlock` at the same offset), so the `bool` in
the symbol is just whichever monomorphization LLVM kept as canonical. The crash
is therefore **the unlock of *some* `std::sync::Mutex` on invalid memory**, not
specifically a `Mutex<bool>`.

**The faulting addresses are the real tell.** `segfault at 1d4c0`, `at ea60` —
small integer-looking values, never a real heap/stack address. That is a
**garbage `self` pointer** being dereferenced (`self + <mutex-field-offset>` =
a small address) at the mutex lock/unlock. So the shape is a **stale/garbage
native handle read from a Java field** (the XnioIoThread mirror carries
`worker_handle`/`selector_handle` native ids — doc table, `xnio_io_thread.rs:78`),
whose owner was freed or whose id was corrupted (GC-moved mirror / reused
registry slot), NOT a mutex-owner `Arc` being dropped while parked.

**All the real `std::sync::Mutex` + `Condvar::wait_timeout` parking sites are
Arc-protected** and were ruled out by inspection: `event_loop.rs`'s
`WakeableCondvar` is a field of `EventLoop`, always `Arc<EventLoop>` held by the
loop thread for the whole `run_event_loop`; `vertx_eventloop.rs`'s
`WakeableCondvar` is `VertxEventLoop.parker`; `xnio_worker.rs`'s
`io_thread_stub_body` holds `Arc<IoThreadHandle>` across its park; the
`Arc<(Mutex<bool>, Condvar)>` waiters (`concurrent_extras.rs`, `vm_init.rs`
class-loading, `forkjoin.rs`, `virtual_threads.rs`) each hold their own Arc.
None can be freed while a thread is inside `wait_timeout` on them. So the crash
is **not** a parking-site UAF — it is a native-handle deref on a bad `self`,
almost certainly in a native method entry point that resolves an
XnioIoThread/worker handle from a Java field and then touches a mutex on it.

**Next step (unchanged in spirit, corrected in target):** get a live gdb
backtrace of the *server* process (the Arquillian-managed WildFly, launched via
`container.java.home/bin/java`) — put a gdb wrapper **named `java`** at
`container.java.home/bin/java` (NOT surefire's `-Djvm`, which targets the wrong
process and which surefire rejects unless the path ends in `java`). The frames
*above* the merged drop-glue leaf will name the actual native handle-resolution
path. Then apply the identity-hash side-table handle pattern
([[reference_real_bytecode_pseudofield_identity_hash_pattern]]) or reference-count
the handle registry so a GC/finalize can't free it mid-native-call.

**Coordination note, resolved 2026-07-07 ~15:26:** the concurrent
[[wildfly-elytron-remoting-segfault-post-keyfactory-fix]] investigation got a live
gdb backtrace of its crash and confirmed it is a **different** mechanism from this
one -- a Java-heap GC-root bug (JIT-tracked `String` `ObjectRef` gone stale across a
safepoint, in `../../../../vm/src/jit/helpers.rs` / `../../../../vm/src/vm/vm_exec.rs`), not a native
`std::sync::Mutex`/handle UAF in `xnio_io_thread.rs`. So these two do NOT share this
native-handle-UAF root cause -- they are separate bugs that happen to both be
WildFly-under-CratonVM SIGSEGVs found the same day. Still worth checking
[[wildfly-infinispan-remove-listener-segfault]] against whichever mechanism gets
confirmed here.

## Reproduction attempt 2026-07-07 — current dev HANGS at boot (does NOT crash); precise-maps cleared

Reproduced with a symbolicated current-dev binary (`4e6dc36d`+, i.e. after the
precise-maps-default-ON flip `65d7cfba`) booting WildFly 32 **standalone**
directly under CratonVM (`JAVA_HOME`→cratonvm, `CRATONVM_JAVA_HOME=jdk25`),
bypassing Maven/Arquillian.

**The documented SIGSEGV did NOT reproduce as a crash.** Instead the server
**hangs during boot** at the `ServerService Thread Pool` startup, every time,
at a **GC-barrier / blocked-region-transition deadlock** — NOT the native-handle
crash. gdb-attach to the hung process (all threads):

- 1 thread in `stw_take_over_and_wait` (`interpreter.rs:535`) →
  `wait_for_all_timeout` (`gc_barrier.rs:312`): the STW initiator, spinning with
  `pending=1 taken=0` ("still waiting for cooperative mutators rounds=64").
- ~10 threads parked in `wait_out_pause_locked` (`gc_barrier.rs:278`) — arrived,
  waiting for the pause to end.
- ~8 threads in `arrive_and_wait`/`safepoint_check` (`gc_barrier.rs:363`,
  `interpreter.rs:2409`).
- **2 threads stuck mid-`mark_blocked_region_leave`** (`gc_barrier.rs:220`) via
  `native_rq_remove_timeout` (`reference.rs:505`) →
  `end_blocking_region_refs` (`vm_exec.rs:5151`): a `ReferenceQueue.remove`
  worker trying to LEAVE its blocked region while a STW is active. This is the
  `pending=1` holdout — a thread in the blocked→running transition window that
  the takeover neither excuses (it left the blocked set) nor takes over
  (`taken=0`). Classic blocked-region-transition barrier deadlock (cf.
  [[reference_blocked_thread_gc_gap]] "leave WAITS OUT active STW while still
  counted").

**Precise-maps is NOT the cause.** The identical hang reproduces with
`CRATONVM_NO_PRECISE_JIT_MAPS=1` (precise OFF) — so the precise-maps-default-ON
flip (`65d7cfba`) does **not** regress WildFly boot; both modes deadlock at the
same point. (This clears the flip; the hang is orthogonal.)

**Open interpretation:** either (a) the documented SIGSEGV was on the OLD frozen
binary (`37efdc4a`, precise-off) and intervening commits turned the
race's outcome from crash→hang, or (b) standalone boot differs from the
Arquillian-managed container config enough that standalone hits the barrier
deadlock while the managed container hit the native-handle crash. The
native-handle-UAF analysis above (garbage `self` at a Mutex unlock) still stands
for the documented *crash*; this boot **hang** is a distinct GC-barrier
coordination bug that blocks WildFly boot on current dev regardless of precise
maps and likely deserves its own doc + a targeted fix in the
`gc_barrier`/`stw_take_over_and_wait` blocked-region-leave transition. NOT fixed
(a speculative change to the STW/blocked-region protocol is high blast-radius —
it governs every multi-threaded workload).

Repro (fast, no Maven, collision-free): `JAVA_HOME=<cratonvm-javahome>
CRATONVM_JAVA_HOME=/home/victor/jdk25 wildfly-dist/wildfly-32.0.1.Final/bin/standalone.sh`
— hangs at `ServerService Thread Pool -- N` within ~5s; gdb-attach for the
barrier state. See [[reference_wildfly_native_segfault_family_20260707]].

## Symptom

Once the test harness actually launches a real Arquillian-managed WildFly server under CratonVM (see
[[wildfly-infinispan-remove-listener-segfault]] for the harness-fix context — this doc's finding
appeared only *after* fixing `container.java.home` so the server stopped silently falling back to real
JDK 17), a very large fraction of classes crash the forked CratonVM process outright:

```text
[ERROR] Process Exit Code: 139
```

Kernel confirms every single occurrence lands at the **exact same code offset**, only the faulting
address (a small, garbage-looking value, never the same twice) and PID/thread vary:

```text
kernel: main-vm[<pid>]: segfault at 1d4c0 ip 000057397255edf7 sp ... error 4 in java.exe[e5bdf7,...]
kernel: main-vm[<pid>]: segfault at ea60  ip 00005c4a8eeb3df7 sp ... error 4 in java.exe[e5bdf7,...]
```

131 occurrences observed in a ~65-minute window covering ~200 attempted classes.

## Symbolization

```text
$ addr2line -e <round-2 binary> -f -C 0xe5bdf7
core::ptr::drop_in_place<std::sync::poison::PoisonError<(std::sync::poison::mutex::MutexGuard<bool>,std::sync::WaitTimeoutResult)>>
```

`nm -C` confirms `0xe5bdf7` (15,056,375) falls inside this same symbol, which starts at `0xe5bdb0`
(15,056,304) — a real containing-function match, not a resolution artifact.

**Caveat:** this is drop-glue for a `PoisonError` whose payload has already been extracted via
`.into_inner()` by every call site of this shape in the codebase (see below) before the error value goes
out of scope — there should be nothing left to meaningfully "drop." The much more likely reality is that
LLVM merged/deduplicated this drop-glue with the *actual* crash site, `MutexGuard<bool>`'s own `Drop`
impl (which unlocks the underlying `std::sync::Mutex<bool>`), since both are tiny, structurally similar
generated functions over the same `bool` payload type and the optimizer can fold them together. So the
practical crash site is most likely **unlocking a `Mutex<bool>` whose underlying memory is no longer
valid** — a use-after-free, not literally "dropping a PoisonError."

## Best source-level match found (not yet confirmed as THE crash site — candidate, not proof)

`../../../../native-builtins/src/xnio_io_thread.rs`, `MockSelector`:

```rust
pub fn new() -> Self {
    Self {
        wakeup_mu: Mutex::new(false),      // <-- Mutex<bool>
        wakeup_cv: std::sync::Condvar::new(),
        ready: Mutex::new(VecDeque::new()),
    }
}

impl SelectorHandle for MockSelector {
    fn select(&self, timeout_ms: u64) -> std::io::Result<usize> {
        let mut guard = self.wakeup_mu.lock().unwrap_or_else(|e| e.into_inner());
        if *guard {
            *guard = false;
        } else {
            let dur = Duration::from_millis(timeout_ms);
            let (g, _) = self
                .wakeup_cv
                .wait_timeout(guard, dur)              // <-- exact PoisonError<(MutexGuard<bool>, WaitTimeoutResult)> shape
                .unwrap_or_else(|e| e.into_inner());
            guard = g;
            *guard = false;
        }
        ...
    }
}
```

This is the only `Mutex<bool>` + `Condvar::wait_timeout` pairing found in the codebase (grepped
`../../../../native-builtins/src` and `../../../../vm/src` for `Mutex<bool>` combined with `wait_timeout`; other `wait_timeout`
call sites pair with `Mutex<VecDeque<_>>`, larger state structs, or use `parking_lot`'s non-poisoning
mutex instead of `std::sync::Mutex`, which can't produce this exact symbol). `MockSelector` backs XNIO's
I/O-thread selection loop — used pervasively by WildFly's remoting/HTTP/management-interface I/O threads,
which would explain both (a) the extremely high frequency (any busy I/O thread loop exercises `select()`
continuously) and (b) the timing tying it to real server operation, not just boot.

**This has not been confirmed with a live debugger** — only kernel-log + addr2line/nm symbolization of a
release binary. A gdb session (see Suggested next steps) is needed to confirm `MockSelector` is really
the call site and not a different, structurally-identical `Mutex<bool>` drop path elsewhere.

## Why this matters

This is very likely the dominant, previously-invisible cause behind large parts of BOTH:
- the original full-suite run's "integration/basic managed-container-never-registers" cluster (the
  container could have been crashing mid-boot this whole time, just masked by the harness bug that made
  it fall back to real JDK before this code path was ever reached under CratonVM), and
- this session's round-2 rerun, where it is now the single largest blocker to getting real pass/fail
  signal from the suite (64% CRASH rate in the observed sample).

Fixing this (and the separate, lower-frequency [[wildfly-infinispan-remove-listener-segfault]]) is very
likely the highest-leverage next step for this entire test suite's usefulness.

## Suggested next steps

1. Confirm the exact crash site with a live debugger: `ulimit -c unlimited` then reproduce (or attach
   gdb to a forked surefire PID while a class is mid-run — servers stay up for tens of seconds under
   normal test execution, giving a real attach window). This host's `core_pattern` routes to `apport`
   which wasn't producing usable cores in an earlier investigation this session
   ([[wildfly-elytron-remoting-segfault-post-keyfactory-fix]] hit the same limitation) — may need
   `sudo sysctl kernel.core_pattern=core` or equivalent to get a plain core file.
2. If `MockSelector` is confirmed: check its lifetime management — is it ever dropped/deallocated while
   another thread is still inside `wait_timeout` or `wakeup()` on the same instance (e.g., via a raw
   pointer/reference that outlives the owning `Arc`/`Box`, or a native-side handle that a Java-side
   `finalize()`/GC event can free concurrently with in-flight native I/O thread activity)? That would be
   the classic native-stale-pointer shape already seen elsewhere in this codebase (see
   [[wildfly-infinispan-remove-listener-segfault]]'s "Related" section for the established fix pattern).
3. Given the very high frequency (not "intermittent" like the infinispan finding), this may be more
   reliably reproducible than that one — worth trying a small, targeted repro that just churns
   `MockSelector::select()`/`wakeup()` under concurrent load without the full WildFly stack, if `select`
   really is the site.

## Repro

```bash
# On the Azure host, from a WildFly checkout with target/wildfly already built, container.java.home
# pointed at a cratonvm-backed JAVA_HOME (required -- see the container.java.home harness fix) so the
# server actually runs under CratonVM:
cd apps/wildfly-suite-runner   # own copy pointed at WILDFLY=<built wildfly checkout>
export WILDFLY=/data/data/cratonvm/apps/wildfly
export CRATONVM_BIN=<cratonvm release binary>
export JDK25_WIN=<real JDK 25 home>
export MAVEN_ARGS='-Dcontainer.java.home=<a JAVA_HOME-shaped dir with cratonvm at bin/java>'
./run-suite-linux.sh run --category all --jit on --jdk real --class-to 300 \
  --shard 1/2 --tag repro
# -> expect ~60%+ of attempted classes to CRASH with Process Exit Code: 139
journalctl -k --since '10 min ago' | grep segfault   # confirms + gives the exact offset for this binary
```

## Evidence

```text
journalctl -k --since '2026-07-07 07:24:00' --until '2026-07-07 08:29:00' | grep segfault   (131 matches, all offset e5bdf7)
addr2line -e frozen-cratonvm-wildfly-bugbash-v2-20260707 -f -C 0xe5bdf7
nm -C frozen-cratonvm-wildfly-bugbash-v2-20260707 | grep -B1 -A1 e5bdb0
/data/data/wt-wildfly-bugbash-20260707-runner/out/rerun2-s{1,2}of2-jit-real-all-20260707-072407/crashes.log
```

## Related

**CONFIRMED SAME BUG as [[wildfly-elytron-remoting-segfault-post-keyfactory-fix]]** (see the top section)
— both are the A4 register-only-oop family, tracked centrally in
[[fork6-fjp-multithread-jit-root-reclamation]].

**UPDATE 2026-07-07 ~16:30: also confirmed the SAME bug as the (formerly "distinct") infinispan
finding.** `wildfly-infinispan-remove-listener-segfault.md`'s original `addr2line`-derived
`infinispan_local::CacheInner::remove_listener` attribution was itself an LTO/drop-glue symbol-merge
artifact, exactly like this doc's own original `MockSelector`/`std::sync::Mutex` misattribution — a live
gdb repro (full DWARF symbols, `release-with-debug` profile) showed the identical
`read_string` <- `xnio_async::native_builder_set` <- `jit_invoke_virtual_mic` stack, with the SAME
fault addresses (`0xea60`=60000, `0x1d4c0`=120000 — XNIO worker/option millisecond timeouts misdecoded
as heap pointers, not "near-null garbage"). Fixed as `60079fc4` (`fix(jit): validate heap membership for
L/[ arg slots in jit_invoke_virtual_mic`, `../../../../vm/src/jit/helpers.rs`) — see the corrected doc, moved to
`wildfly-infinispan-remove-listener-segfault.md`. All three WildFly
SIGSEGV docs opened on 2026-07-07 (this one, the elytron one, and the infinispan one) turned out to be
the same underlying JIT `L`/`[` argument-decode gap, each initially misattributed to a different
subsystem by `addr2line` on a stripped/LTO release binary.

Still distinct from the separate GC-barrier boot-**hang** found in the "Reproduction attempt
2026-07-07" section above (that one blocks standalone boot outright and doesn't crash — an orthogonal
bug, not yet root-caused, not addressed by this confirmation).

## Evidence (2026-07-07 ~16:15 confirmation)

```text
/data/data/scratch-xnio-mutex-segv/core.main-vm.371916.1783440822   (core dump, EJBSecurityTestCase, first attempt)
/data/data/scratch-xnio-mutex-segv/gdb-core-analysis.log            (full post-mortem backtrace, all threads)
/data/data/scratch-xnio-mutex-segv/mvn-nojit.log                    (CRATONVM_DISABLE_JIT=1 bisection: 3/3 no-crash)
```
