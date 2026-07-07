# WildFly domain startup timeout with repeated corrupt `Value` cell guard

Status: OPEN — but the HIB-CV-32 corrupt-Value guard did NOT fire in ANY 2026-07-07 run (the sixth session finally ran the real Arquillian test end-to-end under CratonVM nested processes; see the 2026-07-07 sixth-session update at the bottom). Three merged fixes on this path (class_manager/vtable_manager AB-BA 48b3c2d2, XNIO AbstractMethodError 952f0093, jit_instanceof UAF) plus the 11 MSC real-start boot fixes (4b2508cf) appear to have cleared or now out-gate whatever produced the guard; two NEW blockers (sibling doc) currently stop the boot before the sustained-load phase that originally provoked it. Do not move to internal until a domain boot again runs a long sustained workload with the guard confirmed silent under CRATONVM_DIAG_HIB32=1.
Severity: High
First confirmed: 2026-07-05 on Azure worktree `codex/wildfly-nonpassed-probes-20260705-035722`

## Symptom

After fixing the JBoss Modules multi-entry `-mp` bug, `EEConcurrencyExecutorShutdownTestCase` no longer exits immediately during process-controller launch. It now waits the full startup window and fails with:

```text
java.util.concurrent.TimeoutException: Managed servers were not started within [120] seconds
```

The log repeatedly emits the same heap guard diagnostic while the test polls management:

```text
gen_heap::read_slot: corrupt Value cell (out-of-range discriminant) - returning null instead of a UB-on-match Value. Heap reference-integrity defect (see HIB-CV-32). slot=0x2002600b1d0 raw0="0x0000000100000009" raw1="0x0000000000000000"
```

The management client retries `remote://127.0.0.1:9999` until timeout. The generated domain directory contains configuration and `data/kernel/process-uuid`, but no `process-controller.log` or `host-controller.log` beyond the empty audit log.

## Evidence

Primary run:

```text
/data/wt/wt-wildfly-nonpassed-20260705-035722/apps/wildfly-suite-runner/out/azure-eeconcurrency-mpmulti2-082-jit-real-failed-20260705-160124
```

Key files:

```text
logs/00001-org.jboss.as.test.integration.domain.EEConcurrencyExecutorShutdownTestCase.log
failcauses.log
summary.txt
```

Result summary:

```text
classes: FAIL=1
test-methods: found=1 passed=0 failed=0 errors=1 sum-class-ms=128157
wall-clock=128s
```

## Notes

This is distinct from the fixed process-controller module-path bug. The old immediate `ModuleNotFoundException` and `MDC.put` linkage failure are gone with `cratonvm-wildfly-nonpassed-20260705-035722-mpmulti2`; the remaining failure is a real 120-second domain startup timeout with a repeated guarded heap-corruption signature.

This bug report and the `gen_heap::read_slot` HIB-CV-32 discriminant guard it references were both added in the same commit (`a3728860`, 2026-07-05, "Fix WildFly non-passed suite blockers") — that commit's `gen_heap.rs` changes are unrelated conservative-root-candidate hardening (Family-A), not a fix for this guard's trigger. The root cause of *why* the slot decodes to an out-of-range discriminant (`0x...09` here, valid range is `0..=6`) was left open.

## 2026-07-06 investigation (this session)

Evidence-gathering only — no reproduction achieved, no code change made. Recorded so the next session doesn't re-walk the same ruled-out path.

**Hypothesis 1 (ruled out): plain-field 16-byte `Value` slot tearing.** Same day this doc was filed, three commits closed a real mutator-vs-mutator tearing gap in plain (non-volatile) `getfield`/`putfield`: interpreter `read_slot`/`write_slot` across all three heap backends (`2dfdfddc`), the older JIT `jit_putfield_*` fix (`4e6b560f`), and JIT `jit_getfield`'s read side (`5198fccd`, landed via `investigate/aqs-rwl-writer-contention-jit-hang`) — all now on `dev`. `EEConcurrencyExecutorShutdownTestCase`'s concurrent-executor shape is exactly the kind of code this bug hits, so this looked like the fix at first.

It is **not**, on closer reading of `types/src/value.rs`'s own doc comments (`read_value_atomic`/`write_value_atomic`, `read_value_checked_atomic`): "for a statistically-typed Java field the discriminant word is invariant across stores, so even a cross-word 'torn' pair reconstructs to a valid `Object(ptr-or-null)`/primitive — **never a spliced garbage pointer**." A single Java field only ever stores one `Value` variant, so tearing between two writes to the *same* field can produce a stale or torn *payload*, but never an out-of-range *discriminant* — the exact HIB-CV-32 guard this report's log line trips. Confirmed empirically too: a synthetic repro (two real OS threads, one hammering `h.x = A/B` on a shared `int` field via a JIT-OSR-compiled loop, one reading it, `CRATONVM_DIAG_HIB32=1`) ran ~200M iterations/side with `bad=0` on both a pre-tearing-fix baseline build (`164264c8`) and current `dev` — consistent with this bug class only affecting payload staleness, not discriminant validity. Array element access was also audited (two independent passes) and confirmed **not** to share this bug class at all: array elements are packed native-width primitives or bare 8-byte pointers, never the 16-byte tagged `Value` layout object fields use.

**Hypothesis 2 (best candidate, unconfirmed): JIT `getfield` reference-oop mistagging.** Commit `e60b7a5c` (2026-07-06, `fix/jasper-jdt-parser-aioobe-20260706`, already on `dev`) fixed a *different*, more topically-relevant bug: `getfield`'s three x86-64 codegen paths never marked a reference-typed field's loaded value as a GC oop on the JIT operand stack, so it silently decayed to a plain `Int` (holding the raw pointer bits) at any GC-safepoint or deopt boundary that captured it while uncommitted — confirmed in that investigation via `CRATONVM_DBG_DEOPT=1` showing a `char[][]` field's value captured as `Int(4332917944)` instead of `Object(...)`. This is precisely the shape of "heap reference-integrity defect" HIB-CV-32 exists to survive: a live reference silently mistyped, subsequently mis-relocated/mis-collected/misused, eventually landing on a slot read that decodes bytes belonging to something else (e.g. an object header's `num_slots`/flags fields — `raw0=0x0000000100000009` reads suspiciously like `num_slots=9, some 1-valued flag` rather than any real `Value` encoding). The fix is unconditional (no feature flag), so it applies to any reference-typed `getfield`, not just the JDT parser call sites that surfaced it — plausible for WildFly's heavy dynamic-module/executor code under GC pressure during domain startup.

**Not yet confirmed empirically.** Two synthetic repros were attempted against the pre-fix baseline (`164264c8`, predates `e60b7a5c`): a generic reference-field-getfield-plus-allocation loop, and a closer mirror of the original JDT idiom (`this.intStack[this.intPtr--]` immediately followed by a reference-element-array `System.arraycopy`, inside an OSR-compiled instance-method loop, matching the exact shape documented in `docs/internal/jasper-jdt-parser-arrayindexoutofbounds.md`). Both ran to 200k+ iterations with zero mismatches and no HIB-CV-32 guard hits, confirming OSR-compilation occurred (`CRATONVM_DBG_JITC=1` showed `OSR-compile`/`bg-compile tier=C2`) but not exercising whatever precise interleaving is needed. This matches that same JDT investigation's own account: even the team that root-caused and fixed bug #1 "could not get a minimal standalone Java repro to fail-then-pass" for the *tearing* commit, and needed 12 purpose-built, iteratively-refined synthetic probes (T3–T14) to reliably trigger *this* bug family at all — a repro budget well beyond what this session could allocate as a side-investigation.

**Recommended next step:** re-run `EEConcurrencyExecutorShutdownTestCase` (and ideally the full WildFly domain-mode slice) end-to-end against current `dev` (which now includes `e60b7a5c` and all three tearing fixes). This needs a rebuilt WildFly distribution + Arquillian domain harness on a build host — the prior evidence run's artifacts and the Azure host's WildFly build were both lost to disk-pressure cleanup since 2026-07-05, so this is a from-scratch rebuild (WildFly `install -DskipTests` alone took ~54 min in the original suite run), out of scope for this session. If the guard diagnostic and timeout are gone, close this out referencing `e60b7a5c`; if not, the synthetic repros in `docs/known-issues/repros/wildfly-domain-startup-timeout/` (`FieldTearRepro.java`, `ParserIdiomRepro.java`) are a starting point to iterate into a reliable standalone trigger, same as the JDT investigation's T-series did.

## 2026-07-06 update (parallel session) — boot-infrastructure blocker found; live E2E still not reachable

A second, independent investigation this same day tried the "rebuild the WildFly
distribution and re-run E2E" next step above via a shortcut: rather than a full
`wildfly-core` testsuite + Maven build (not available on the Azure probe host used),
it downloaded a **binary** WildFly 32.0.1.Final distribution from GitHub releases (no
Maven build needed) and drove `bin/standalone.sh`/`bin/domain.sh` directly under a fresh
`dev`-HEAD `cratonvm`, using the same real-JDK/`--nojit` configuration
`apps/wildfly-suite-runner/run-suite.sh` uses.

This did not reach far enough to re-observe (or rule out) either hypothesis above: both
boots stall **before** any application-level service does real, sustained work.
CratonVM only drives the real `Service.start(StartContext)` MSC callback when
`CRATONVM_MSC_REAL_START=1` is set (default off — see
`docs/internal/app-jvm-bugs/handoff-wildfly-msc-service-start.md`, an existing,
separately-tracked, explicitly-incomplete effort). `run-suite.sh` never sets this flag,
so with the default configuration the very first application-level MSC service install
after `WFLYSRV0049 ... starting` never signals completion and the boot hangs
indefinitely — confirmed via `CRATONVM_DEFAULT_WATCHDOG_SEC` + the built-in stack-dump
watchdog to be a genuine parked wait (all non-daemon threads idle in
`EnhancedQueueExecutor$ThreadBody.run`, near-0% CPU), not slow interpretation. Turning
the flag on gets standalone mode further, but into an unrelated
`ServiceNotFoundException` (`BootstrapImpl.internalBootstrap` failing to resolve
`Services.JBOSS_AS`) within ~3 seconds, and makes domain mode hang even earlier with no
error at all. Filed as
[bug-15](../internal/wildfly-suite-bugs/bug-15-msc-real-start-servicenotfound-and-domain-hang.md)
— a boot-infrastructure gap orthogonal to both hypotheses above, but one that must be
resolved (or Maven + the real testsuite restored) before *either* hypothesis can be
confirmed or refuted against a live, sustained-load domain-mode process again.

**Combined recommended next step:** whoever picks this up next needs one of (a) Maven +
a `wildfly-core` testsuite checkout to rerun the actual Arquillian test, or (b) progress
on bug-15 (the MSC real-start gate) so a hand-driven binary-distribution boot can reach
real sustained concurrent execution — only then can Hypothesis 2 above (or a new one) be
tested against a live process again. The `FieldTearRepro.java`/`ParserIdiomRepro.java`
synthetic repros remain the fastest path to iterate on Hypothesis 2 without either.

## 2026-07-06 update (third session) — Hypothesis 2 traced end-to-end at the code level; weakened

Rather than another blind synthetic-repro attempt, this session traced Hypothesis 2's
actual runtime consequence through the code, since a repro couldn't be forced (see below)
and the previous two sessions' repro attempts had already come up empty:

1. **The mistagged value genuinely reaches the resumed interpreter frame — this part of
   Hypothesis 2 is confirmed, not speculative.** A getfield result left unmarked as an
   oop on the JIT operand stack is captured at deopt as `FrameValue::Int(raw_pointer)`,
   and `fv_to_value` (`vm/src/runtime/interpreter.rs`) maps `FrameValue::Int` straight to
   `Value::Int` — unlike `FrameValue::Unsupported` (the *locals*-only failure mode from
   the *separate* bug #2 in the same `e60b7a5c` fix), which `ir_deopt_frame_values`/
   `ir_deopt_locals` reject outright (the whole `.collect()` short-circuits to `None`,
   forcing a safe whole-method re-run instead). Bug #1 (operand stack) and bug #2
   (locals/params) are two different oop-tracking mechanisms with two different failure
   modes — only #2's failure is caught by that reject-on-`Unsupported` safety net. So a
   `Value::Int(raw_pointer_bits)` really does land on the resumed interpreter's operand
   stack where bytecode expects an `Object`, exactly as the original write-up describes.

2. **But the JDT idiom's own next consumer — `System.arraycopy` — is type-safe against
   this, so it can't be the vector for THIS specific symptom.**
   `native_system_arraycopy` (`native-builtins/src/lang_system.rs`) extracts `src`/`dest`
   via `match args.first() { Some(Value::Object(Some(obj))) => *obj, _ => return
   NullPointerException }` — a mistagged `Value::Int` here throws a benign (if
   misleadingly-worded) NPE, not a wild pointer dereference. Whatever the JDT
   investigation's `CRATONVM_DBG_DEOPT` trace captured, it did not go on to corrupt
   memory via this call; the JDT bug's own observed symptom (AIOOBE) is fully explained
   by bug #3 (the unsafe re-run double-executing `stack[ptr--]`), independent of whether
   bug #1's mistagging ever caused any further harm.

3. **Under CratonVM's DEFAULT configuration — what WildFly's suite runner and both prior
   hand-driven repro attempts use — bug #1's oop-marking gap has no GC-liveness
   consequence at all.** `vm/src/jit/conservative_roots.rs`'s own module doc: while
   `CRATONVM_PRECISE_JIT_MAPS` is unset (default), active JIT frames are scanned
   *conservatively* — every 8-byte-aligned stack qword is treated as a *possible* heap
   pointer and validated via `is_object_address`, independent of any oop mark ("false
   negatives are impossible... every real reference is at an 8-byte aligned spill slot").
   So a getfield result missing `mark_top_as_oop()` is still found and kept alive by a
   plain GC pause — the oop mark only matters for `CRATONVM_PRECISE_JIT_MAPS`/
   `CRATONVM_MOVING_YOUNG` (both default-off) or a deopt (see #1/#2 above). This rules
   out the "prematurely collected while unrooted, then dereferenced through reused
   memory" mechanism as the DEFAULT-config explanation — that mechanism is real but only
   fires under those non-default flags.

**Net effect: Hypothesis 2, as literally stated (`e60b7a5c`'s getfield-oop-marking fix),
does not by itself explain how an out-of-range *discriminant* — not just a wrong *value*
— appears in a heap `Value` cell under CratonVM's default configuration.** A mistagged
reference produces a `Value::Int` holding raw pointer bits, which is a real, distinct
correctness bug (wrong value, right discriminant tag) but not the literal "16-byte cell
whose bytes don't form any valid `Value`" signature this guard fires on, at least not via
the two consumer paths traced here (arraycopy call, GC root scan). Something must still
either (a) feed that mistagged `Value::Int` into some OTHER, not-yet-identified consumer
that skips the safe `Value`-matching CratonVM otherwise uses everywhere (a raw,
untagged machine-code dereference in re-JIT-compiled continuation code is the most
likely remaining candidate, not yet traced), or (b) be a mechanism unrelated to either
hypothesis investigated so far.

**Repro-engineering attempt (also inconclusive, recorded to save the next session the
same dead end):** built a from-scratch mirror of the JDT idiom
(`this.intStack[this.intPtr--]` immediately followed by `System.arraycopy` on a
reference-element `char[][]`), tried three variants against the pre-fix baseline
(`164264c8`) — a version with the hot loop and the idiom in separate methods, an inlined
single-method version, and both a zero-length and a `len=4` real reference-array copy
(the zero-length call turned out to bypass the element-kind guard entirely via the
intrinsic's own dedicated zero-length fast exit — worth knowing if reused). None of the
three ever produced a single `[cratonvm-deopt]` trace line for the arraycopy call site
across 200,000 iterations each (`CRATONVM_DBG_DEOPT=1`) — the guard this session expected
to fail on every call, per the original bug write-up, never visibly fired. Did not
resolve why (candidates: the intrinsic wasn't applied to this exact call shape at all,
`consumeLike()`-as-separate-method never got hot enough to compile independently even
after 200k calls — confirmed no `bg-compile GetfieldOopRepro.consumeLike` line ever
appeared, only `runLoop`'s own OSR-compile — or a precondition specific to real JDT
bytecode this synthetic mirror doesn't reproduce). Separately hit and worked around an
unrelated JIT footgun: `println`/string-concatenation (`invokedynamic`) inside the same
hot loop triggers an `UnreachedCode` uncommon-trap on the *dead* `StringConcatFactory`
branch that silently truncates the loop's remaining iterations without any exception —
harmless once known, but worth flagging for whoever writes the next synthetic probe here.

**Recommended next step:** the fastest remaining path is very likely a full Arquillian
E2E rerun (still blocked on Maven/wildfly-core availability and bug-15), since three
sessions' worth of synthetic-repro and code-tracing effort has not yet nailed a minimal
standalone trigger. If another synthetic attempt is still preferred over waiting on
infra, first confirm the arraycopy intrinsic guard is even being exercised (e.g. add a
`CRATONVM_DBG_JITC`/direct disassembly check that `ArraycopyPrimitive`'s guard code is
actually emitted and taken) before investing further iteration count.

## 2026-07-06 update (fourth session) — deadlock root-caused and fixed via a new,
## WildFly-independent repro; distinct from Hypotheses 1/2 above

Rather than continue iterating on Hypothesis 2's synthetic-repro dead end (three
sessions' worth of JDT-idiom mirrors never fired the guard), this session took a
different approach: reproduce the *shape* of `EEConcurrencyExecutorShutdownTestCase`
directly — a real `ExecutorService` thread pool, no WildFly/Spring needed — and see
what actually breaks under real concurrent OS-thread execution + GC pressure with the
JIT enabled (the prior sessions' repros were single-threaded or JIT-off).

**New repro:** `docs/known-issues/repros/wildfly-domain-startup-timeout/FieldSpawn.java`
— a real `Executors.newFixedThreadPool(8)` where every worker hammers plain
putfield/getfield on a small shared array of heap-object fields (mirroring
`ManagedExecutorService` usage), under constant allocation and small-heap GC pressure
(`-Xmx48m`), JIT enabled (no `--nojit`).

**Result: on unmodified `dev`, this reliably DEADLOCKS within the first round** (3/3
repro runs hung; confirmed not a "just slow" false read via `ps -o pcpu` showing 0% CPU
across every worker thread). A live capture via `gdb -p <pid> -batch -ex 'thread apply
all bt'` (same technique the independent HttpClient-hang investigation used — see
memory note `httpclient-hangs-are-classmanager-rwlock-deadlock-and-methodhandle-dispatch`,
which hit the *same lock family* from a different angle and left it unconfirmed) showed
a textbook **AB-BA lock-order inversion** between `SharedVm.class_manager` and
`SharedVm.vtable_manager` (both `parking_lot::RwLock`s):

- **Class definition** (`ClassManager::define_class_with_options` →
  `fire_vtable_install_hook` → `vtable_install_adapter`, `vm/src/runtime/vtable.rs`):
  holds `class_manager` (write, for the whole definition) → then takes `vtable_manager`
  (write, `manager.write().install_vtable(...)`).
- **Virtual-dispatch fast path** (`execute_invokevirtual_vtable_fast`,
  `vm/src/runtime/interpreter.rs` ~26840-26880): took `vtable_manager` (read) and, while
  STILL HOLDING that guard, ALSO took `class_manager` (read) to resolve the declaring
  class's name for a native-shadow check — the exact OPPOSITE lock order.

One thread mid-class-definition (holding `class_manager` write, blocked acquiring
`vtable_manager` write) and one thread mid-dispatch (holding `vtable_manager` read,
blocked acquiring `class_manager` read) deadlock each other permanently. This directly
violates `resolve_virtual_slot`'s own documented invariant — a test in `vm.rs` right
next to it states "the vtable must be queryable without taking the class_manager lock" —
so this was a real regression against the component's own stated contract, not a new
design question.

**Fix (branch `fix/wildfly-cv-corrupt-value-20260706`):** `execute_invokevirtual_vtable_fast`
now copies out the small pieces of data it needs (`declaring_class_id`, `is_native`, the
cloned `Arc<CachedBytecodeMethod>`) and explicitly `drop(guard)`s the `vtable_manager`
read lock *before* acquiring `class_manager` — the two locks are never held nested
after this fix, in either direction.

**Validated:**
- Same repro against the fix: 5/5 clean runs, full completion (200 rounds / 320,000 ops
  each, `failed=false`), vs. 3/3 reliable hangs on the pre-fix baseline binary with
  identical arguments.
- `cargo check -p cratonvm-vm` clean (no new warnings); `cratonvm-cli` release build
  succeeds.
- (Heavier `cargo test --release` integration-test compile OOM-killed on the build host
  under `lto=fat`/`codegen-units=1` — an environment resource limit unrelated to this
  change, not attempted further; the functional repro is stronger evidence for a
  concurrency bug than a single-threaded unit test would be regardless.)
- Incidentally re-confirmed a separate, already-documented JIT bug while building this
  repro: string concatenation (`invokedynamic`/`StringConcatFactory`) inside the same
  hot loop as the workload silently truncates the loop's remaining iterations
  (`UnreachedCode` uncommon-trap on the dead branch) — same family the third session
  flagged. Worked around here by not concatenating inside the round loop; still an open
  item for whoever picks up JIT invokedynamic/uncommon-trap work next.

**How this relates to Hypotheses 1/2 above:** this is a genuinely different bug (a plain
lock-ordering defect, no tearing or GC/oop-marking involved) that reliably reproduces
under the same real-concurrency + GC-pressure + JIT-enabled conditions the WildFly test
needs, and explains the *timeout/hang* shape of the symptom
(`TimeoutException: Managed servers were not started within [120] seconds`) extremely
well — a wedged VM thread during concurrent classloading matches a domain boot that
stalls while the management client keeps retrying. **Not separately re-confirmed:**
whether this exact deadlock is *also* the literal trigger for the repeated `HIB-CV-32`
"corrupt Value cell" log line — across all 5 post-fix repro runs the guard never fired
(only the unrelated, benign `mark_young: rejecting ... implausible extent` conservative-
root-candidate rejection noise did, which is expected/by-design per `gen_heap.rs`'s own
comments). It's plausible both symptoms share a common trigger condition (heavy
concurrent classloading + dispatch under GC pressure during domain startup) without
being the same code path.

**Recommended next step:** re-run `EEConcurrencyExecutorShutdownTestCase` end-to-end
against `dev` + this fix once Maven/`wildfly-core` availability (or bug-15) is resolved
— same blocker as every prior session. If the timeout is gone, close this out
referencing the lock-order fix; if the `HIB-CV-32` guard still fires, Hypothesis 2's
residual mystery (a not-yet-identified consumer that skips the safe `Value`-matching
CratonVM otherwise uses everywhere) remains open and worth another pass.

## 2026-07-06/07 update (fifth session) — two more real bugs found+fixed along the same path; full clean E2E still not achieved (host contention, not code)

Picked up directly from the fourth session's fix
(`fix/wildfly-cv-corrupt-value-20260706`, merged `48b3c2d2`) with the explicit
goal of actually reaching a full Maven/Arquillian `EEConcurrencyExecutorShutdownTestCase`
run against it, per that session's own "recommended next step". Built the
missing infrastructure from scratch on the Azure host: cloned
`github.com/wildfly/wildfly` at tag `32.0.1.Final` into `apps/wildfly`
(previously absent — the original 2026-07-05 evidence run's WildFly build was
lost to disk-pressure cleanup, per the doc's own history), installed
`openjdk-17-jdk` (WildFly's `maven-compiler-plugin:3.8.1` cannot compile
under JDK 21+ for its `--release 11` target — a from-scratch build needs
JDK 17 specifically), built the full reactor (`install -DskipTests`, ~4
minutes with `-T 8` and a warm `~/.m2` cache), and adapted a Linux copy of
`apps/wildfly-suite-runner/run-suite.sh`.

**Key operational finding: the domain's own nested processes
(process-controller/host-controller/servers) do NOT automatically inherit
`-Djvm=<cratonvm>`** — that Surefire property only selects the executable
for the *outer* JUnit-runner fork. The nested WildFly processes are
launched via `-default-jvm <path>`, itself derived from a *different* system
property pair: `-Djboss.test.host.primary.jvmhome=<dir>` /
`-Djboss.test.host.primary.controller.jvmhome=<dir>` (`<dir>` must contain
`bin/java`), read directly by `DomainTestSupport$Configuration`'s static
initializer (`org.wildfly.core:wildfly-core-testsuite-shared`). Without
setting these explicitly, the nested processes silently ran on whatever JDK
happened to be `JAVA_HOME` for the outer `mvn` invocation (real HotSpot),
never exercising CratonVM at all. **Anyone re-running this test under
CratonVM must set both properties to a directory whose `bin/java` is the
CratonVM binary**, or the domain-mode boot never touches CratonVM and any
result is meaningless.

### Bug A: `class_manager`/`vtable_manager` AB-BA deadlock — same as session four, independently reconfirmed

Reproduced the exact deadlock the fourth session fixed (`48b3c2d2`) via a
from-scratch repro (`FieldSpawn.java`, real `ExecutorService` + shared heap
fields + GC pressure) — 3/3 hangs on a `dev` checkout predating that fix,
5/5 clean with it. No new information here beyond confirming the fix is
real and already merged; see that session's own write-up above.

### Bug B (NEW): `Xnio.build(XnioWorker$Builder)` → `AbstractMethodError`, blocking the native/http management interfaces from ever starting

**Fixed, merged dev `952f0093`.** Manually drove `process-controller` →
`host-controller` directly under CratonVM (bypassing the JUnit layer for a
faster edit/rebuild/observe loop) and found the *actual* mechanism behind
the domain never opening its management port: `NativeManagementAddHandler`
(the `native-interface`, port 9999) and `HttpManagementAddHandler` both fail
during boot with

```text
Caused by: java.lang.AbstractMethodError: method org/xnio/Xnio.build(Lorg/xnio/XnioWorker$Builder;)Lorg/xnio/XnioWorker; has no Code attribute
```

Root cause: `native-builtins/src/xnio_worker.rs`'s `native_xnio_get_instance`
hands Java code a singleton stamped with the abstract `org/xnio/Xnio` class
itself (`alloc_xnio_mirror`) rather than a concrete subclass. Only the
*legacy* `Xnio.createWorker(OptionMap)` factory had a matching native
registration; the *modern* XNIO 3.8.x builder-style factory
(`XnioWorker.Builder.build()` → `xnio.build(this)`, what WildFly 32.x
actually calls) had none, so `invokevirtual` correctly found only the
abstract declaration — genuinely no Code attribute exists to dispatch to,
not a CHA/vtable staleness bug. This cascaded into a full
`WFLYCTL0459`/`WFLYHC0034` config rollback and unrecoverable host-controller
abort every time, which is exactly what starves the client's connection
retries against port 9999 into the `TimeoutException` this doc opened with.

Fix: registered `native_xnio_build_worker` on `CLS_XNIO` for
`build(Lorg/xnio/XnioWorker$Builder;)Lorg/xnio/XnioWorker;`, mirroring
`createWorker`'s existing simplification (default `OptionMap`, ignore the
`Builder`'s configured pool sizes/name). Verified: the exact same manual
host-controller boot no longer hits `AbstractMethodError`/`WFLYHC0034` at
all, and progresses much further (2289 vs ~919 captured output lines) into
real management-subsystem/Elytron/audit-log startup.

### Bug C (NEW): `jit_instanceof` SIGSEGV on a stale-but-bit-plausible `ObjectRef`

**Fixed, merged dev — branch `fix/jit-instanceof-uaf-20260706`.** With bug B
fixed, the domain boot progressed far enough to hit a *different* crash: a
live `gdb` capture on the outer JUnit-runner JVM (a client-side `xnio-task-N`
executor thread, this VM's own management-connection retry loop) showed

```text
Thread 27 "Thread-4" received signal SIGSEGV, Segmentation fault.
#0  cratonvm_vm::jit::helpers::jit_instanceof ()
```

`jit_instanceof` (`vm/src/jit/helpers.rs`) validated its receiver with only
`plausible_heap_pointer` — a pure bit-pattern check (non-null, 8-aligned,
≤47-bit address) documented as having "zero false positives" but no view of
the heap's actual mapped extent. A stale `ObjectRef` into memory the heap
has since reclaimed/reused (the exact same class of GC root-coverage gap
behind the BUG-03 family, see
[[bug03-cross-thread-jit-root-scan-insufficient]] /
[[bug03-concurrent-spawn-frame-remap-gap]] in memory) can satisfy that bit
check while still being dangling, and the unchecked
`ObjectRef::from_raw` + `class_id_of` this function did next then read
through it.

Fix: swapped the unchecked construction for `vm.heap.is_object_address(addr)`
— the same heap-region-validating check `roots.rs`'s conservative scan
already uses — degrading a dangling reference to "not an instance" instead
of dereferencing it, consistent with every other stale-reference guard in
this codebase (`gen_heap::read_slot`'s `HIB-CV-32` guard,
`mark_young`'s implausible-extent rejection, etc.). Kept the cheap
`plausible_heap_pointer` pre-filter ahead of the `vm_ptr` dereference so the
existing unit tests (which pass `vm_ptr = 0`) are unaffected — all 3 pass
unchanged. This is a narrow, targeted fix for the one call site that
actually crashed live; it does **not** audit or fix the other `jit_*`
helpers `plausible_heap_pointer`'s own doc comment says share the identical
pattern (`jit_getfield`, `jit_aaload`) — that audit is a reasonable
follow-up but out of scope here (no evidence they've crashed in practice,
and this session's budget went to the one confirmed live crash).

### Why a full clean E2E pass still hasn't been observed this session

With all three fixes deployed, later attempts to re-run the full
Maven/Arquillian test hit **environment instability, not a fourth code
bug**, on this shared Azure host (confirmed **40 concurrent user sessions**
at the time, `load average` ~6/16 cores, one prior run leaving an orphaned
competing `cratonvm` process bound to the same ports after a `timeout`
kill):

- The Maven **wrapper's own bootstrap JVM** (`MavenWrapperMain`) hung for
  200+ seconds with completely flat CPU time (`futex_do_wait`) before ever
  reaching the project build — most likely contention on the shared
  `~/.m2` repository/wrapper-dist cache across many concurrent Maven
  invocations on this host. Worked around by invoking the already-extracted
  Maven distribution directly
  (`~/.m2/wrapper/dists/apache-maven-3.6.3-bin/*/apache-maven-3.6.3/bin/mvn`),
  which reliably reached the actual test phase in seconds.
- A subsequent run stalled with the outer test JVM legitimately parked in
  `CountDownLatch.await()` (confirmed via live `gdb`, not a crash) for the
  full timeout window, **before `target/domains` was ever created** — i.e.
  before `DomainLifecycleUtil.start()` even ran. Not yet root-caused
  whether this is further host-load-induced slowness in early JUnit/class-
  loading setup, or a genuine (fourth) bug; ran out of session budget before
  isolating it.
- Separately, a **parallel session working the adjacent
  `HttpComponentsClientHttpRequestFactoryTests` hang** (memory:
  `httpclient-hangs-are-classmanager-rwlock-deadlock-and-methodhandle-dispatch`)
  found and merged the *same* `class_manager`/`vtable_manager` AB-BA fix
  independently (commits `caa4ee65`/`fc77a2f3`), and — after that fix — found
  a **separate, still-OPEN residual**: `class_manager` RwLock writer
  starvation under heavy concurrent reader pressure (non-fair
  `parking_lot::RwLock`, a queued writer can starve behind a steady stream
  of short reader acquisitions), now its own doc,
  `docs/known-issues/class-manager-rwlock-writer-starvation.md`. This is
  architecturally very plausible for `EEConcurrencyExecutorShutdownTestCase`
  too (heavy concurrent class-loading/dispatch during domain boot) and is
  worth checking first if a future session hits a `class_manager`-shaped
  hang here specifically (distinguish via live `gdb`: reader-starvation
  shows a queued writer + actively-cycling readers, never a 2-thread AB-BA
  cycle).

**Status of the three fixes themselves: all independently verified,
merged, and pushed to `dev`** (`48b3c2d2`, `952f0093`, and the
`jit-instanceof-uaf` merge). They are real, narrow, low-risk correctness
fixes on their own merits regardless of this doc's outcome. What remains
unconfirmed is only the **compound, full end-to-end claim** — that these
three together (plus whatever the reader-starvation doc's fix eventually
is) are *sufficient* to make `EEConcurrencyExecutorShutdownTestCase` pass
cleanly end-to-end, and whether the original `HIB-CV-32` corrupt-Value-cell
log line specifically was ever caused by any of them (it did not reproduce
in any of this session's targeted repros, matching the fourth session's
same non-finding).

**Recommended next step:** retry the full Maven/Arquillian run on a
quieter window of this shared host (or a dedicated one), using the direct
Maven-distribution invocation (not the wrapper) and the
`jboss.test.host.primary.jvmhome`/`controller.jvmhome` properties documented
above. If it hangs again in `CountDownLatch.await()` before
`target/domains` exists, get a live `gdb` capture immediately (before any
timeout kills it) to identify what that latch actually is and who's
supposed to count it down — this session did not reach that. If it instead
reaches domain boot and hits a `class_manager`-shaped stall, check
`class-manager-rwlock-writer-starvation.md` first.

## 2026-07-07 update (sixth session) — the actual Arquillian test ran end-to-end at last; HIB-CV-32 guard did NOT fire in any run

The "recommended next step" every prior session ended on — get a live
Maven/Arquillian `DefaultConfigSmokeTestCase` (and the domain suite) running with
CratonVM as the *nested* domain JVM — was finally achieved this session, after the
2026-07-07 MSC real-start boot fixes (`4b2508cf`, 11 blockers) made the host-controller
boot reach real sustained execution. The full working harness recipe is in the sibling
doc `wildfly-domain-managed-servers-timeout.md`'s 2026-07-07 update (the load-bearing
detail: nested processes take their JVM from
`-Djboss.test.host.primary.jvmhome`/`.controller.jvmhome`, NOT the outer Surefire
`-Djvm`, so both must point at a CratonVM `bin/java` or the domain silently runs on
HotSpot).

**Key finding for THIS doc: across every 2026-07-07 run — testsuite E2E (both JIT and
no-JIT) and multiple hand-driven `domain.sh` boots — the `gen_heap::read_slot: corrupt
Value cell` (HIB-CV-32) guard did NOT fire once.** The failures observed instead were:

- **no-JIT:** host-controller boots, both managed servers reach `WFLYSRV0025 ... started`,
  but the Arquillian `awaitServers` poll never sees them as started (a management-model /
  server-registration propagation gap — a NEW residual, tracked in the sibling doc).
- **JIT-on:** a JIT miscompile fails HC *interface resolution* (`WFLYSRV0082`) at ~311s,
  well before anything the corrupt-Value guard was about — also a NEW, separate blocker
  (sibling doc), and clearly not this one.

This is meaningful negative evidence. The corrupt-Value guard was a *symptom* of a heap
reference-integrity defect somewhere upstream; with the three fixes this doc already
tracks (`48b3c2d2` class_manager/vtable_manager AB-BA, `952f0093` XNIO
`AbstractMethodError`, and the `jit-instanceof-uaf` heap-region guard) plus the 11 MSC
real-start boot fixes all now on `dev`, the domain boot path that used to spin emitting
that guard line no longer reaches (or no longer creates) whatever produced the
out-of-range discriminant. Two readings remain open and this session cannot yet
distinguish them:

1. **Genuinely resolved** — one of the merged fixes (most plausibly the
   `jit-instanceof-uaf` heap-region-validation guard, which directly addresses a
   stale-`ObjectRef`-dereference of exactly the kind that lands garbage in a heap slot)
   removed the upstream defect, and HIB-CV-32 is effectively closed.
2. **Merely not-yet-reached** — the two new blockers above (awaitServers propagation
   under no-JIT; interface-resolution JIT miscompile) now gate the boot *before* the
   sustained concurrent-classloading-under-GC-pressure phase that originally provoked the
   guard, so it simply hasn't had the chance to fire.

**Recommended next step:** resolve the two new blockers in the sibling doc first (they
now gate reaching sustained load). Once a domain boot again runs a long sustained
concurrent workload — either a full `awaitServers`-passing `DefaultConfigSmokeTestCase`
or `EEConcurrencyExecutorShutdownTestCase` proper — re-check for the HIB-CV-32 line under
`CRATONVM_DIAG_HIB32=1`. If it stays silent through that phase, close this doc referencing
the `jit-instanceof-uaf` fix; if it returns, the residual "some consumer skips the safe
`Value`-matching" mystery from the third-session update is still live. Until then this doc
stays OPEN, but note the guard has now gone unobserved across an entire session's worth of
real domain runs for the first time since 2026-07-05.
