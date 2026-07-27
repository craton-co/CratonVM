# SIGSEGV during WildFly server boot/shutdown — originally attributed to `infinispan_local::CacheInner::remove_listener`, ACTUALLY the A4 register-only-oop gap (`xnio_async::native_builder_set`)

Status: **FIXED** — landed on `dev` as `60079fc4` (`fix(jit): validate heap membership for L/[ arg slots in
jit_invoke_virtual_mic`). Moved here from `../../../known-issues` once fixed and independently re-verified.
Severity: was **High** (real native SIGSEGV, not a misclassification) — root cause turned out to be the
same defect already tracked centrally as the "A4" family in
[`../fork6-fjp-multithread-jit-root-reclamation-FIXED.md`](../fork6-fjp-multithread-jit-root-reclamation-FIXED.md).
First confirmed: 2026-07-07, Azure worktree `test/wildfly-full-suite-20260707`, dev@37efdc4a
Root-caused + fixed: 2026-07-07, two independent sessions converged on the same diagnosis —
`wt-infinispan-remlist-0707` (branch `fix/infinispan-remove-listener-segfault-0707`, landed the fix as
`60079fc4`) and `wt-infinispan-listener-segfault-20260707-154032` (branch
`fix/infinispan-remove-listener-segfault-20260707-154032`, independent live-gdb confirmation below).

## ⛔ CORRECTION — the original hypothesis was WRONG

The original version of this doc (kept below, unedited, for the record) blamed
`../../../../native-builtins/src/infinispan_local.rs`'s `CacheInner::remove_listener`:

```rust
pub fn remove_listener(&self, listener: ObjectRef) -> bool {
    let mut listeners = self.listeners.lock();
    let ptr = listener.as_ptr() as usize;             // <-- doc's claimed crash site
    let before = listeners.len();
    listeners.retain(|h| h.listener_ptr != ptr);
    listeners.len() != before
}
```

**This cannot be the fault site.** `ObjectRef::as_ptr()` (`../../../../types/src/value.rs`) is a trivial
`self.ptr.as_ptr()` getter over a `NonNull<u8>` — it copies a pointer *value*, it never dereferences
memory. `ListenerHandle::dispatch` (the only other place `listener_ptr` is read) is explicitly documented
in-source as never dereferencing it either ("We intentionally do NOT call back into the VM here — the
listener store only records the pointer bits"). The only real memory dereference anywhere in
`remove_listener` is `self.listeners.lock()` — a `parking_lot::Mutex::lock()` on the `&self` receiver,
which is a `Arc<CacheInner>` reconstructed via a verified-balanced `Arc::into_raw`/`Arc::from_raw`
pair (`native_dcm_get_cache` / `cache_from_field`). Static analysis (done before any live debugging)
already flagged this contradiction and correctly refused to accept the doc's own literal claim.

**The real explanation: `addr2line` symbol merging on a stripped/LTO release binary.** The original
doc's evidence was `addr2line -e <release binary> -f -C <fault-rva>` on a `strip = "debuginfo"` release
build, giving `cratonvm_native_builtins::infinispan_local::CacheInner::remove_listener`. A live gdb
repro (below, full DWARF line-table symbols, `release-with-debug` profile) reproduced a SIGSEGV at
**the exact same kernel fault address the doc reported (`segfault at 1d4c0`)** — but resolves to a
completely different function: `NativeContextImpl::read_string` (`vm/src/vm/vm_exec.rs:3376`), called
from `xnio_async::native_builder_set` (`native-builtins/src/xnio_async.rs:887`), reached through a
JIT-compiled call site (`jit_invoke_virtual_mic`, `../../../../vm/src/jit/helpers.rs`). This is **the same crash
already root-caused and documented** in
[`wildfly-elytron-remoting-segfault-post-keyfactory-fix.md`](wildfly-elytron-remoting-segfault-post-keyfactory-fix.md)
and [`wildfly-xnio-mockselector-mutex-segfault.md`](wildfly-xnio-mockselector-mutex-segfault.md) (which
itself was originally misattributed to an unrelated `std::sync::Mutex`/native-handle-UAF theory in
`xnio_io_thread.rs`, and independently corrected the same day). LTO + fat codegen-units=1 causes LLVM to
merge/dedupe machine code across unrelated generic instantiations and small functions; whichever
symbol table entry happens to survive linking gets reported for every address inside the merged region,
so `addr2line` on a release build can point at a plausible-sounding but wrong function. This is now the
**third** WildFly SIGSEGV doc on this repo that independently hit this same addr2line-misattribution trap
for the same underlying crash — a good argument for always getting a live, symbol-accurate backtrace
(`release-with-debug` profile, `debug = "line-tables-only"`, `strip = "none"`) before trusting an
`addr2line`-derived hypothesis on a stripped/LTO'd binary.

## True root cause (CONFIRMED via live gdb, 2026-07-07)

**Repro** (`org.jboss.as.test.integration.deployment.classloading.ear.EarClassLoadingTestCase`, the
doc's own original repro target — no infinispan-specific trigger needed, since the crash isn't in
infinispan at all): a `gdb -batch -ex run -ex 'thread apply all bt full' -ex 'info registers' -ex 'x/8i
$pc'` wrapper substituted as Surefire's forked `java` (technique reused from
`wildfly-elytron-remoting-segfault-post-keyfactory-fix.md`; `set logging redirect on` keeps gdb's own
output off stdout so Surefire's IPC handshake survives; additionally `handle SIGUSR2 nostop noprint pass`
was needed — CratonVM's own cross-thread JIT-takeover protocol
(`vm/src/jit/xt_root_scan.rs::TAKEOVER_SIGNAL`) uses `SIGUSR2` internally, and gdb's default
stop-on-SIGUSR2 handling stalls that protocol and corrupts the Surefire goodbye handshake, producing
spurious non-crash failures that look like "the repro didn't reproduce"):

```text
Thread 2 "main-vm" received signal SIGSEGV, Segmentation fault.
[Switching to Thread 0x7ffff71bf6c0 (LWP 308932)]
read_string () at vm/src/vm/vm_exec.rs:3376
3376            let class_id = self.shared.heap.class_id_of(obj);

Thread 2 (Thread 0x7ffff71bf6c0 (LWP 308932) "main-vm"):
#0  read_string () at vm/src/vm/vm_exec.rs:3376
#1  0x0000555555d45027 in native_builder_set () at native-builtins/src/xnio_async.rs:887
#2  0x00005555563ea955 in {closure#4} () at vm/src/vm/vm_exec.rs:605
...
#8  safe_native_call () at vm/src/vm/vm_exec.rs:604
#9  0x00005555564c840a in invoke_or_native () at vm/src/vm/vm_exec.rs:7745
#10 0x0000555556536e1e in jit_invoke_virtual_mic () at vm/src/jit/helpers.rs:4920
#11 0x00007ffff7dbb219 in ?? ()   <- JIT-generated code, no debug info

rdx            0xea60              60000          <- fault register: NOT a pointer, a millisecond int!
rip            0x5555563ad507      <read_string+23>
=> 0x5555563ad507 <read_string+23>:  mov    (%rdx),%r13d      <- deref of 0xea60 -> SIGSEGV
```

Reproduced twice in a row with an identical stack (both attempts). A third, non-gdb raw-exec run against
the same repro produced a kernel-logged segfault matching the doc's exact original fault address:

```text
kernel: main-vm[315580]: segfault at 1d4c0 ip 00005cbcc915f507 sp 000071afc23a9d30 error 4 \
  in frozen-infinispan-cratonvm[e58507,5cbcc877a000+11d2000]
```

**Both fault addresses are not "small near-null garbage pointers" as the original doc guessed — they are
literal XNIO worker/option millisecond timeout values**: `0xea60` = 60000, `0x1d4c0` = 120000. The
mechanism: `jit_invoke_virtual_mic`'s deferred argument decoder (`../../../../vm/src/jit/helpers.rs`, the
`decode_values` closure used on a MIC cache-miss/lambda/register-overflow bailout) decodes a JIT call's
raw register/stack argument words into typed `Value`s using the callee's method descriptor. For an `L`/`[`
(reference-typed) descriptor slot, the old code treated *any* 8-byte-aligned value under the 48-bit
canonical address ceiling as a "plausible" heap pointer and built an `ObjectRef` from it unconditionally.
But a **primitive `long` argument that should have gone through the `J` arm** — such as
`org.xnio.OptionMap$Builder.set(Option, Object)` being fed a boxed/unboxed timeout constant — can itself
be 8-byte-aligned and well under 2^48 (any round millisecond value like 60000 or 120000 trivially is).
The alignment+ceiling heuristic alone cannot distinguish "real heap pointer" from "small aligned
integer", so it fabricated a bogus `ObjectRef` pointing at unmapped low memory, which was then handed to
`native_builder_set` (the native override for `OptionMap.Builder.set`), which called `ctx.read_string()`
on it — the first line of `read_string` dereferences the object header, SIGSEGV.

This is the exact same "A4 register-only-oop" crash chain independently found and root-caused the same
day in [`wildfly-elytron-remoting-segfault-post-keyfactory-fix.md`](wildfly-elytron-remoting-segfault-post-keyfactory-fix.md)
(`ElytronRemoteOutboundConnectionTestCase`) and
[`wildfly-xnio-mockselector-mutex-segfault.md`](wildfly-xnio-mockselector-mutex-segfault.md) (which
found it dominating 64% of `testsuite/integration/basic` CRASH classifications) — see
[`../fork6-fjp-multithread-jit-root-reclamation-FIXED.md`](../fork6-fjp-multithread-jit-root-reclamation-FIXED.md) for the
central tracker. All three docs' original hypotheses were wrong in different ways (this doc: wrong
function entirely via addr2line merge; the mockselector doc: wrong subsystem, `std::sync::Mutex`
native-handle-UAF theory refuted the same day); all three converged on the identical gdb-confirmed stack.
**Unlike the Elytron doc's framing** (which described this as a deep, un-fixable-blind "JIT needs a
register-oop bitmap on `OopMapEntry`" gap), the specific manifestation reached through
`native_builder_set`/`OptionMap.Builder.set` turned out to be independently fixable at the JIT argument
*decode* boundary — see "Fix" below. It does **not** close the more general A4 register-invisibility gap
(see `fork6-fjp-multithread-jit-root-reclamation.md`'s own caveats about `Fork6Hard`'s GC_STRESS repro
still being open) — it closes this one concrete, high-frequency instance of it.

## Fix

`../../../../vm/src/jit/helpers.rs`, inside `jit_invoke_virtual_mic`'s `decode_values` closure (~line 4512), the
`L`/`[` descriptor-slot decode arm. Before:

```rust
Some(b'L') | Some(b'[') => {
    if raw == 0 {
        Value::Object(None)
    } else {
        // Same defensive guard as the receiver decode above.
        // Tagged-long bits in an L/[ slot are downgraded to
        // null instead of panicking in ObjectRef::from_raw.
        let bits = raw as u64;
        if (bits & 0x7) == 0 && bits < (1u64 << 48) {
            // SAFETY: bits is non-zero, 8-byte aligned, and
            // within the 48-bit canonical address space.
            Value::Object(Some(ObjectRef::from_raw(raw as usize as *mut u8)))
        } else {
            Value::Object(None)
        }
    }
}
```

After (landed as `60079fc4`):

```rust
Some(b'L') | Some(b'[') => {
    if raw == 0 {
        Value::Object(None)
    } else {
        // Tagged-long bits leaked into an L/[ slot ... must be
        // downgraded to null instead of being treated as a heap
        // pointer. The alignment + 48-bit-ceiling check ALONE is
        // not sufficient: a round-number primitive long is also
        // 8-byte-aligned and well under 2^48 ...
        let bits = raw as u64;
        let validated = if (bits & 0x7) == 0 && bits < (1u64 << 48) {
            vm.heap.is_object_address(bits as usize)
        } else {
            None
        };
        match validated {
            Some(obj) => Value::Object(Some(obj)),
            None => Value::Object(None),
        }
    }
}
```

`vm.heap.is_object_address(addr)` (`../../../../gc/src/vm_heap.rs`, `../../../../gc/src/gen_heap.rs`, `../../../../gc/src/g1.rs`) checks
**actual heap membership** — the address must land on a live object header in the real heap — not just
bit-pattern plausibility. This exactly matches an already-correct sibling decode path a few hundred
lines earlier in the same file (the MIC-miss/register-overflow-bailout arg decode at
`vm/src/jit/helpers.rs:~1451`), which already used `is_object_address` and was never vulnerable to this.

## Verification

Two independent sessions verified the fix on the same day, against two independently-frozen builds of
the same commit:

- **Session A** (`wt-infinispan-remlist-0707`): 8 consecutive `run-suite-linux.sh` attempts against
  `EarClassLoadingTestCase` post-fix, all classified `FAIL` (ordinary functional test error), zero
  `CRASH`. Pre-fix baseline: 2/2 attempts `CRASH`.
- **Session B** (this doc's author, `wt-infinispan-listener-segfault-20260707-154032`, independent gdb
  build + independent verification harness):
  - Pre-fix: 2 gdb-wrapped repro attempts, **both SIGSEGV** with the identical stack above.
  - Post-fix: **10/10 consecutive gdb-wrapped repro attempts, zero SIGSEGV** (`signal=[none]` on every
    attempt; full gdb backtrace logs captured for each).
  - Broader regression slice: 30 classes from `testsuite/integration/basic`'s
    `deployment`/`classloading` packages (real managed-container boot/deploy/shutdown cycles), fixed
    binary, `--jit on --jdk real`: **0 CRASH, 0 ABEND, 30 FAIL** (`classes: FAIL=30`, empty
    `crashes.log`) — the FAIL classification is the pre-existing, separately-tracked
    `container.java.home`-unset harness gap (server boots under real JDK 17 as CratonVM's *client*, not
    as the hosted server — see this doc's original "Compounding harness discovery" section below), not
    a regression from this fix.
  - `journalctl -k` showed **zero** kernel segfaults for CratonVM's own fixed binary across the entire
    verification window. Three unrelated kernel segfaults observed during the same window belonged to a
    different, pre-fix frozen binary (`frozen-precheck-cratonvm`) owned by the concurrent Elytron
    investigation, confirmed by binary path/hash — not a residual of this fix.

Combined: **18 consecutive clean targeted repro attempts + 30/30 clean broader-slice classes**, versus a
100% pre-fix crash rate (2/2 and 2/2 across both sessions' baselines) on the identical repro.

## Related

- [`wildfly-elytron-remoting-segfault-post-keyfactory-fix.md`](wildfly-elytron-remoting-segfault-post-keyfactory-fix.md) —
  same crash chain, first root-caused here (as a general, harder-to-fix JIT register-oop-bitmap gap).
- [`wildfly-xnio-mockselector-mutex-segfault.md`](wildfly-xnio-mockselector-mutex-segfault.md) — same
  crash chain, confirmed the same day as the dominant blocker (64% CRASH rate) for
  `testsuite/integration/basic`.
- [`../fork6-fjp-multithread-jit-root-reclamation-FIXED.md`](../fork6-fjp-multithread-jit-root-reclamation-FIXED.md) — the
  central "A4" tracker; this fix closes one concrete, high-frequency manifestation (the `L`/`[`
  JIT-arg-decode plausibility gap) but does **not** close the more general register-invisible-oop gap
  described there (that doc's own `Fork6Hard`/`GC_STRESS` repro is a different code path and remains
  open — see its own caveats about the "register-oop-bitmap" theory being refuted for that specific
  repro).
- The harness-only findings in this doc's original version (orphaned server processes not being
  cleaned up on crash/timeout, and `container.java.home` being unset so the managed server runs under
  real JDK 17 instead of CratonVM) are **still valid and still open** — they are orthogonal harness bugs
  in `apps/wildfly-suite-runner`, not touched by this fix. See the "Compounding harness discovery"
  section preserved below.

---

## Original doc (2026-07-07, preserved for the record — root-cause section above supersedes the hypothesis below)

Status: OPEN — new, found during 2026-07-07 full WildFly suite bug-bash follow-up (harness debugging session)
Severity: **High** — real native SIGSEGV crash, not a misclassification. Likely masked hundreds of the
"no managed container" classifications from the original full-suite run (see below).
First confirmed: 2026-07-07, Azure worktree `test/wildfly-full-suite-20260707`, dev@37efdc4a

### Symptom

When Maven Surefire forks CratonVM to run a `testsuite/integration/basic` class that actually reaches
the point of starting a real Arquillian-managed WildFly standalone server, the forked CratonVM process
can segfault outright (`Process Exit Code: 139`), rather than failing/passing cleanly:

```text
[ERROR] Error occurred in starting fork, check output in log
[ERROR] Process Exit Code: 139
```

Kernel confirms the crash and its exact location:

```text
kernel: main-vm[<pid>]: segfault at 1d4c0 ip 00005bd2212c2df7 sp 00007a224cda9e50 error 4 in java.exe[...]
```

`error 4` = user-mode read of a non-present page. `ip - image base` symbolizes cleanly (binary was built
`not stripped`) to:

```text
cratonvm_native_builtins::infinispan_local::CacheInner::remove_listener
```

**(Superseded — see correction above: this symbolization was an addr2line drop-glue/LTO merge artifact.
The true crash is `NativeContextImpl::read_string` via `xnio_async::native_builder_set`.)**

### Compounding harness discovery: crashed/timed-out runs orphan the spawned WildFly server process

Separately from the crash itself: when the forked CratonVM test-runner JVM dies (segfault, or gets
`timeout`-killed for exceeding `--class-to`), the **real WildFly server process it spawned as a child
survives** (it's not in the same process group / doesn't get cleaned up), and stays bound to the
module's default ports (8080/8180/8443/8543/9990/10090) indefinitely. **This finding is still valid and
unaddressed** — a harness bug in `apps/wildfly-suite-runner`, independent of the CratonVM crash that
originally surfaced it. Fixing `container.java.home` (also still unaddressed) so the spawned server
actually runs under CratonVM (rather than falling back to real JDK 17) remains necessary to exercise
this module family's stated purpose.
