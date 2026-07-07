# SIGSEGV in Mutex&lt;bool&gt;/Condvar::wait_timeout drop path — dominant crash once WildFly actually runs under CratonVM

Status: OPEN — new, found 2026-07-07 during round-2 rerun of the full WildFly suite (after fixing the
test harness so the managed WildFly server actually runs under CratonVM instead of falling back to real JDK)
Severity: **Critical for this suite's signal** — the single dominant failure mode once the harness bug
(container.java.home falling back to real JDK) was fixed; hit ~64% of all classes attempted in a ~200-class
sample of round 2 (128/200 classes CRASH, all exit code 139, all at the identical code offset).
First confirmed: 2026-07-07, Azure worktree `test/wildfly-full-suite-20260707`, dev@37efdc4a (round-2 binary)

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
safepoint, in `vm/src/jit/helpers.rs` / `vm/src/vm/vm_exec.rs`), not a native
`std::sync::Mutex`/handle UAF in `xnio_io_thread.rs`. So these two do NOT share this
native-handle-UAF root cause -- they are separate bugs that happen to both be
WildFly-under-CratonVM SIGSEGVs found the same day. Still worth checking
[[wildfly-infinispan-remove-listener-segfault]] against whichever mechanism gets
confirmed here.

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

`native-builtins/src/xnio_io_thread.rs`, `MockSelector`:

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
`native-builtins/src` and `vm/src` for `Mutex<bool>` combined with `wait_timeout`; other `wait_timeout`
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

Distinct from [[wildfly-infinispan-remove-listener-segfault]] (different symbol, different subsystem,
much lower frequency/more intermittent) and [[wildfly-elytron-remoting-segfault-post-keyfactory-fix]]
(different symptom shape — that one has no symbolization attempt yet). All three are native SIGSEGVs
found in the same 2026-07-07 WildFly bug-bash session's harness-debugging phase, once the harness
was fixed to actually exercise CratonVM as the WildFly host rather than falling back to real JDK.
