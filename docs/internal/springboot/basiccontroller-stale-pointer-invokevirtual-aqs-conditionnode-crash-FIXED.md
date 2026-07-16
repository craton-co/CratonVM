# BasicErrorControllerIntegrationTests CRASH — young-GC object-start walk truncation leaves live objects unforwarded → mass stale-pointer/all-zero-header corruption → bogus ClassCastException

**Status: FIXED 2026-07-16.** The fix this doc's own "Related" section named
as unmerged (`fix/wildfly-cce0079-close-20260716`) landed in `dev` and was
merged into this worktree the same day. Confirmed via `grep -n
skip_free_blocks gc/src/gen_heap.rs` post-merge: the `young_object_starts`
walk (line 3692) now calls `skip_free_blocks`, matching the sibling
`exact_cursor` walk. Re-ran `BasicErrorControllerIntegrationTests` against a
fresh post-merge build: the fatal `ClassCastException`/process-CRASH is
gone (now `HANG` at the 300s timeout — a different, unrelated outcome not
investigated further here). The GC forwarding-walk truncation mechanism
this doc describes is closed; the new HANG is a separate matter.

**Severity (at time of discovery): CRITICAL** (GC memory-safety — live heap objects silently dropped from the forwarding set during a moving young collection; manifests here as a fatal, unhandled `ClassCastException` inside `SbRunner.main`, but the same mechanism produces hard SIGSEGVs elsewhere in this family).

| | |
|---|---|
| **Module** | `module/spring-boot-webmvc` |
| **Class** | `org.springframework.boot.webmvc.autoconfigure.error.BasicErrorControllerIntegrationTests` |
| **Result** | `CRASH`, exit code `1`, wall time 254.121s (`rerun-20260716` / shard6) |
| **Worktree** | `C:\craton\CratonVM-spring-boot-crashfail-20260714` (branch `feat/spring-boot-crashfail-20260714`) |
| **Log** | `apps\spring-boot-suite-runner\.suite\results\rerun-20260716\shard6\logs\module_spring-boot-webmvc.org.springframework.boot.webmvc.autoconfigure.error.BasicErrorControlle-957a1d0f4289.{out,err}.log` |

## Symptom

The `.err.log` for this class is dominated by two families of GC-guard warnings
from the very first line, well before the fatal exception (line numbers from
the log):

```
WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped
    (caller used slot index past receiver's layout — class layout is correct;
    the bug is in the caller's slot computation, typically a speculative
    collection-layout probe dispatched on a non-matching receiver type)
    obj=0x1ca96e00 index=0 num_slots=0 class_id=ClassId(705)
    class_name=org/junit/jupiter/engine/execution/InterceptingExecutableInvoker
    real_field_count=Some(0)
```

(also seen against `org/springframework/core/$Proxy17`, `jdk/proxy1/$Proxy26`,
`InvocationInterceptorChain`, etc. — all objects whose header has decayed to
`num_slots=0 / real_field_count=Some(0)`, i.e. **the class layout is fine but
the object itself has been zeroed**.)

At the very end of the run (20:15:23.186569–.187415Z — **under 1 millisecond
of wall time** for ~1200 repeats, i.e. a tight loop, not a hang), the same
mechanism produces a storm against two specific stuck addresses:

```
WARN cratonvm_vm::runtime::interpreter: Stale pointer detected in invokevirtual receiver
    (ptr=0x2ef01a88, all-zero header) — falling back to CP class
    java/util/concurrent/locks/AbstractQueuedSynchronizer$ConditionNode
WARN cratonvm_vm::runtime::interpreter: Stale pointer detected in invokevirtual receiver
    (ptr=0x1ca87738, all-zero header) — falling back to CP class jdk/internal/misc/Unsafe
WARN cratonvm::gc::guard: gen_heap::set_field: out-of-bounds field write dropped
    (caller used slot index past receiver's layout — class layout is correct;
    the bug is in the caller's slot computation) obj=0x2ef01a88 index=3 num_slots=0
    class_id=ClassId(0) class_name=java/lang/Object real_field_count=Some(0) value=Int(0)
```

**The process does not hang and is not killed by the runner's timeout.** It
exits on its own, quickly, with:

```
[cratonvm] main-vm run() returned Err: Exception in thread "main"
java/lang/ClassCastException: java.lang.Object cannot be cast to
org.junit.platform.engine.TestExecutionResult$Status
    at SbRunner.main(SbRunner.java:36)
    at org/junit/platform/launcher/core/SessionPerRequestLauncher.execute(...)
    ...
    at org/junit/platform/engine/TestExecutionResult.<init>(TestExecutionResult.java:102)
```

`results.tsv` classifies this as `CRASH` purely because `SbRunner`'s exit code
is `1` (an uncaught top-level exception) — there is no SIGSEGV in this
particular run, though this bug family is documented elsewhere in this
codebase (`BUG-DF02`) to also produce hard native crashes when the same
zeroed receiver reaches JIT-compiled code instead of the interpreter.

The `.out.log` shows only Spring Boot banner reprints (one embedded-context
boot per test method) and no JUnit-level failure output — the JUnit engine
itself never got to report a result; it died constructing the
`TestExecutionResult` for the engine-failure report.

## Root cause

This is the **same bug family already root-caused and partially fixed this
session** under `docs/known-issues/wildfly-standalone-boot-attributeaccess-cce-register-invisible-root.md`
and its fix doc (`fix/wildfly-cce0079-close-20260716`, **not yet merged into
this worktree** — see below), plus the historical
`docs/internal/spring-boot-probe-sweep/spring-bug-10-junit-platform-execution-loaderr.md`
which documented this *exact* symptom pair (`Stale pointer detected …
all-zero header` immediately followed by `ClassCastException … cannot be cast
to org.junit.platform.engine.TestExecutionResult$Status`) as a JUnit-platform
GC root-undercount race, and marked it RESOLVED on 2026-06-21 via precise
JIT-oop-maps-default-on. **That earlier fix addressed a different mechanism**
(register-resident/above-band missed roots on JIT frames); today's recurrence
is a *new, distinct* producer of the identical symptom, introduced same-day
by the just-merged GC work, so `spring-bug-10` is not actually re-broken —
this is a second, independent way to reach the same failure signature.

### The concrete defect: `young_object_starts` walk in `gc/src/gen_heap.rs` doesn't skip free-list/TLAB-tail ranges

Same-day commit `1c4aaa069` ("close stream ArrayList pressure corruption")
added an exact pre-forwarding object-start walk to the **moving (Cheney)**
young-GC path, to build a precise `young_object_starts: FxHashSet<usize>` so
that only real, allocator-written object headers can ever be forwarded
(conservative/interior root hits that don't land on a real header are
ignored rather than corrupting the copy). The walk assumes the young
from-space is a contiguous run of real objects (`gc/src/gen_heap.rs:3611-3642`,
current worktree HEAD `da808e3ec`):

```rust
let mut young_object_starts: FxHashSet<usize> = FxHashSet::default();
...
while young_cursor < young_used {
    let obj_ptr = (young_base + young_cursor) as *mut u8;
    let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
    if header.class_id.as_u32() == crate::tlab::GAP_FILLER_CLASS_ID.as_u32() {
        // handles Bug-D GAP-filler sentinels only
        ...
        continue;
    }
    let size = gen_object_total_size(header);
    if size < HEADER_SIZE || young_cursor.checked_add(size).is_none_or(|end| end > young_used) {
        tracing::warn!(young_cursor, young_used, "GC: young object-start walk stopped at an implausible extent");
        break;                       // <-- silent truncation, no recovery
    }
    young_object_starts.insert(obj_ptr as usize);
    young_cursor += size;
}
```

A same-day follow-up, `fb15be63f` ("GAP_FILLER_CLASS_ID not special-cased in
new young-GC exact-walk loops", **already merged into this worktree**),
special-cased the Bug-D GAP-filler sentinel (a sub-`HEADER_SIZE` TLAB gap
marker whose offset-4 field holds a raw byte length, not header fields) in
both this loop and its non-moving-sweep sibling (`young_object_ranges`,
`gc/src/gen_heap.rs:4628-4670`). That fix is real and closed the majority of
the corruption this session's WildFly investigation measured.

**However, this loop still does not skip the other two gap classes every
other young-heap walk in this file already handles**: merged free-list blocks
(from a prior non-moving sweep) and reserved TLAB tails
(`jit_tlab_skip_offsets`, `gc/src/gen_heap.rs:1653`). Contrast with the
non-moving sweep's sibling walk at `young_object_ranges`
(`gc/src/gen_heap.rs:4628-4670`), which *does* call `skip_free_blocks`
(`gc/src/gen_heap.rs:4633`) before attempting to parse a header — every other
exact/mark walk in this file (lines ~5243, 5590, 5736, 5882, 6071, 7225,
7398, 8454, 9482) also calls `skip_free_blocks` first. The moving-path
`young_object_starts` walk at line 3611 is the one outlier that only checks
for the GAP-filler sentinel and otherwise assumes every byte is a real
header — so when it lands on a free-list block or a reserved TLAB-tail
range under heavy allocation churn (many short-lived JUnit/Spring/AOP
objects, exactly this test's shape), it still hits the `break` at line
3637-3638 and **silently truncates the start set**, exactly as
`fb15be63f`'s own commit message describes for the GAP-filler case it fixed.

### Why truncation corrupts the heap

`forward_object_impl`'s membership check (`gc/src/gen_heap.rs:7737`) rejects
anything not in the (now-incomplete) `young_object_starts` set:

```rust
if !young_object_starts.contains(&(old_ptr as usize)) {
    return old_ptr;   // NOT copied, NOT forwarded
}
```

This check exists to reject conservative *interior* root hits — but it can't
distinguish "this is an interior word, ignore it" from "this is a genuine
object header past the walk's truncation point, but the walk never
recorded it." Every object allocated after the truncation point is
therefore **never copied, never forwarded, for every reference to it** —
precise interpreter-frame roots and `native_pin_roots` pins included. The
semispace flip then recycles that memory. A subsequent reader that follows
any pre-existing reference to one of those addresses gets whatever fresh
allocation has since landed there (or nothing, if it's now free) — a valid
header for the wrong object, or (if genuinely reclaimed and zeroed) the
`all-zero header` symptom this log shows for `0x2ef01a88` /
`0x1ca87738` against `AbstractQueuedSynchronizer$ConditionNode` /
`jdk/internal/misc/Unsafe` receivers, and separately against
`InterceptingExecutableInvoker` / `$Proxy17` / `$Proxy26` field slots earlier
in the same log. This is a heap-wide GC-batch-scale corruption, not a
per-site missed pin — which is exactly why it surfaces as a grab-bag of
unrelated-looking symptoms (stale AQS/Unsafe receivers here, `ClassCastException
Object→TestExecutionResult$Status` at the JUnit-platform level, arbitrary
CCEs in the WildFly investigation, out-of-bounds field read/write "dropped"
guard warnings throughout).

The `AbstractQueuedSynchronizer$ConditionNode` / `Unsafe` receivers are not
themselves special — `BasicErrorControllerIntegrationTests` boots a full
embedded Tomcat servlet container per test method (see repeated
`Http11NioProtocol`/`StandardEngine` banners in the `.err.log`), which
allocates AQS condition nodes and parks/unparks worker threads constantly;
under this walk's truncation, any of those short-lived objects allocated
late in a young cycle can be the one that gets dropped. The final storm is a
tight (sub-millisecond) loop, not a real hang — consistent with the VM
repeatedly re-dispatching against the same two now-permanently-stale
addresses while unwinding/reporting the resulting exception, not with any
`-Parallel` cross-process contention (`-Parallel 2` for this shard governs
concurrent CratonVM *processes*; this corruption is intra-process, entirely
explained by this one process's own young-GC behavior).

### Is this a regression from the just-merged GC work, or a residual of an in-flight fix?

**Residual of an in-flight fix, not (only) a fresh regression.** The
`young_object_starts` truncation bug was independently discovered today in
the WildFly suite and root-caused in
`docs/known-issues/wildfly-standalone-boot-attributeaccess-cce-register-invisible-root.md`
(commit `92ebad642`'s doc update, branch `fix/wildfly-cce0079-close-20260716`,
forked from `origin/dev @ dcb24161`). Two fixes for it exist:

1. `fb15be63f` — **merged into this worktree** — special-cases only the
   Bug-D GAP-filler sentinel in both exact-walk loops.
2. The `fix/wildfly-cce0079-close-20260716` branch — **NOT merged into this
   worktree** (`git merge-base --is-ancestor f0ea83214 HEAD` fails) — is the
   fuller fix: it "merges the free-list/TLAB skip set, tracks walk
   completeness, and adds the sound skip-cycle fail-safe (without the flag,
   a corrupt-filler `break` still silently truncates the forwardable set —
   the original hazard in a rarer edge)," per that branch's own commit
   message (`92ebad642`). Confirmed against this worktree's current
   `gc/src/gen_heap.rs:3611-3642`: the moving-path walk still has no
   `skip_free_blocks` call and still `break`s (rather than failing the whole
   cycle safely) on an unrecognized extent.

So: this worktree has the *partial* fix (`fb15be63f`) but not the
*complete* one. This crash is evidence that the GAP-filler case alone was
not sufficient — free-list blocks and/or reserved TLAB tails are still
capable of triggering the same silent truncation under this test's
allocation shape, and this worktree needs the `fix/wildfly-cce0079-close-20260716`
branch (or an equivalent walk-completeness fix) merged in before this class
of corruption is closed.

## Repro

```powershell
cd C:\craton\CratonVM-spring-boot-crashfail-20260714
# single-class.tsv needs the same header row as all-tests.tsv (module<TAB>class):
@"
module`tclass
module/spring-boot-webmvc`torg.springframework.boot.webmvc.autoconfigure.error.BasicErrorControllerIntegrationTests
"@ | Set-Content -Encoding utf8 .\single-class.tsv

powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -Vm craton -ClassList .\single-class.tsv `
  -Exe C:\craton\CratonVM-spring-boot-crashfail-20260714\target\release\cratonvm-spring-boot-rerun-20260716.exe `
  -RunName basiccontroller-stale-pointer-repro -Parallel 1 -TimeoutSec 300
```

Diagnostics available in this worktree to pin the exact corrupted extent:

- `CRATONVM_DBG_STALE_OBJREF_CYCLES=N` (`gc/src/stale_objref_debug.rs`) —
  generalizes the stale-`ObjectRef` quarantine to an N-cycle ring so a stale
  read arriving ≥2 minor GCs late panics loudly with the offending address
  instead of silently resolving to recycled memory.
- `CRATONVM_DBG_CCE_BT` — prints receiver identity (class + address,
  `via_pin`) at `ClassCastException`/`checkcast` construction sites.
- `CRATONVM_DBG_BLOCKED_ACCESS=warn` (`gc/src/blocked_access_debug.rs`, new
  this merge) — reports heap access from a thread whose
  `in_blocked_region` flag is raised; worth checking here since the
  embedded Tomcat's AQS park/unpark traffic is exactly the shape this
  detector targets, though this doc's root cause (the walk truncation) is
  independent of blocked-thread accounting.
- The walk's own warning (`"GC: young object-start walk stopped at an
  implausible extent"` / `"...found an implausible GAP-filler sentinel"`,
  `gc/src/gen_heap.rs:3632`/`3637`) is the direct signal; grep for it in a
  full-verbosity run to confirm this mechanism fires during this specific
  test's young collections.

## Related

- `docs/known-issues/wildfly-standalone-boot-attributeaccess-cce-register-invisible-root.md`
  — the original discovery of this walk-truncation family (WildFly
  `parallel-extension-add` CCE storm), same root cause.
- `docs/internal/fixed-suite-bugs/wildfly-cce0079-young-start-set-truncation-FIXED.md`
  (only present on branch `fix/wildfly-cce0079-close-20260716`, NOT this
  worktree) — the fuller fix (free-list/TLAB skip + walk-completeness flag +
  sound skip-cycle fail-safe) that this worktree is missing.
- `docs/internal/spring-boot-probe-sweep/spring-bug-10-junit-platform-execution-loaderr.md`
  — documents the identical symptom signature (`Stale pointer detected …
  all-zero header` → `ClassCastException … TestExecutionResult$Status`) as a
  *different*, already-RESOLVED (2026-06-21) mechanism (register-resident
  JIT-frame root undercount, fixed via precise-JIT-oop-maps-default-on).
  Today's recurrence is NOT a regression of that fix — it is a new,
  independent producer of the same surface symptom (see "Is this a
  regression" above).
- `docs/internal/CRATONVM_BUGS/BUG-DF02-stale-zeroed-oop-receiver-dispatch-segv.md`
  — the general "stale/zeroed OOP as invokevirtual receiver" family
  (dispatch-time linkage-error in the interpreter vs. hard SIGSEGV in JIT
  code for the identical corrupted-receiver shape); this doc's crash is the
  interpreter-side manifestation of that same family, from a newly
  identified producer (GC walk truncation) rather than DF02's originally
  investigated one (register-resident missed JIT root).
- `gc/src/gen_heap.rs:1653` (`jit_tlab_skip_offsets`), `:3611-3642`
  (`young_object_starts`, the defective walk), `:4628-4670`
  (`young_object_ranges`, the sibling walk that already calls
  `skip_free_blocks`), `:7692-7745` (`forward_object_impl` and its
  membership check), `:9370` (`skip_free_blocks`).
- `gc/src/blocked_access_debug.rs` — new this merge; a related but distinct
  diagnostic (excluded-while-running blocked-thread heap access), not itself
  the cause of this crash but worth ruling out given the AQS-heavy
  workload.
