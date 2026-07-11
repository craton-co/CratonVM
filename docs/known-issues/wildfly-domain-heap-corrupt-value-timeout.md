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

## 2026-07-07 update (seventh session) — the standalone-boot blocker is pinned to a specific STW accounting stall (BUG-03 family); precise census captured

The 2026-07-07 sixth-session update above noted a "GC/STW cooperative-mutator
stall ~75s into standalone boot". This session reproduced it under a clean
JIT-on standalone boot (WildFly 32.0.1.Final binary dist, `bin/standalone.sh
-b=127.0.0.2 -bmanagement=127.0.0.2`, real JDK 25, `CRATONVM_MSC_REAL_START`
NOT needed — this is the plain standalone path) and pinned the exact
accounting via `CRATONVM_DBG_STW_CENSUS=1` + two independent live `gdb`
captures.

**Precise signature (reproducible):**

```text
[stw-request] initiator=11 alive=20 blocked=14 expected=5
[stw-census]  rounds=64 pending=1 taken=0 blocked=14 alive=20
```

- `expected = alive - 1(initiator) - blocked = 20 - 1 - 14 = 5`.
- `arrived = expected - pending = 4`. So of the 5 counted (non-blocked,
  non-initiator) mutators, 4 reach the JIT-takeover safepoint and **one never
  does** — the STW `wait_for_all_timeout` loop in
  `stw_take_over_and_wait` (`vm/src/runtime/interpreter.rs`) then spins
  forever (rounds keep climbing past 64; the process sits at ~0-2% CPU for the
  whole timeout window — confirmed genuinely stalled, not slow).
- `taken=0`: the cross-thread takeover froze **zero** in-JIT peers, i.e.
  `conservative_roots::any_thread_in_jit()` reported no thread currently
  executing JIT machine code, so `take_over_pass` never excused anyone. The
  pending mutator is therefore NOT caught by the in-JIT takeover path.

**Every thread is parked at the stall.** Two full `gdb -p <pid> -batch -ex
'thread apply all bt'` captures (one 22-thread early-boot, one 96-thread
deep-boot) show **no thread spinning in JIT or interpreter code** — the top
frame of every thread is either `__futex_abstimed_wait_common64` (parking_lot
park, ~58/96 in the deep capture) or `epoll_wait` (XNIO NIO I/O threads,
~38/96). So the one `pending` mutator is *parked in a native* (a
`LockSupport.park`/`parkNanos`, a `Selector`/`epoll_wait`, or an executor idle
park) yet is still counted in the barrier's `expected` set (`blocked=false` in
`debug_thread_census`), i.e. it entered that native block WITHOUT going through
the `gc_barrier` blocked-region protocol (`enter_blocked` /
`mark_blocked_region_enter`). The census breadcrumbs for the non-blocked
mutators point at `org/jboss/threads/EnhancedQueueExecutor$ThreadBody.run@442`
(the JBoss Threads worker idle-park) and one `java/io/FileInputStream.read`
(these frame_traces are last-seen breadcrumbs and can be stale, so treat as a
lead, not proof).

**This is the BUG-03 family** ("cross-thread STW JIT root scan INSUFFICIENT",
see `docs/internal` / memory `bug03-cross-thread-jit-root-scan-insufficient`):
a mutator that neither cooperatively reaches an interpreter safepoint nor is
detected as in-JIT stalls the STW barrier. What is new and useful here is a
**clean, deterministic repro** (WildFly standalone boot under JIT reliably
wedges; far simpler than the app/ForkJoin workloads BUG-03 was originally
chased with) plus the exact census numbers isolating it to **one** non-blocked
mutator parked in a native, with `taken=0` proving the in-JIT takeover is not
the mechanism that would rescue it.

**Live `vm_state` narrows the mechanism (the harder half of BUG-03).** A second
run with `CRATONVM_DBG_STW_CENSUS=1 CRATONVM_DBG_VM_STATE=1` printed the pending
population (`expected=7 blocked=11 alive=19`, one pending) with live states: the
non-blocked mutators are the JBoss-Threads `EnhancedQueueExecutor` workers, all
showing `state="native:return"` (`vm/src/vm/vm_exec.rs:615` — the breadcrumb set
immediately after a native callback returns, before the next native call). Yet
in `gdb` these very threads are parked in `__futex_abstimed_wait_common64` /
`epoll_wait`. The reconciliation: they parked *after* their last real native
returned, via a path that updated neither the `gc_barrier` blocked accounting
(`blocked=false` — so still counted in `expected`) nor the interpreter
`vm_state` (stale `native:return`). That is a **park that skips
`enter_blocked`** — i.e. NOT `NativeContextImpl::park`/`monitor_wait` (both of
which set `blocked=true`), but a JIT-compiled executor idle-park (or an internal
`parking_lot` wait) whose `Rip` lands in libc/futex, not in a registered JIT
code range.

This is precisely the scenario `vm/src/jit/xt_root_scan.rs` (lines ~105-118)
calls out: *"A peer that is blocked (or parked) with its `Rip` in Rust/native
code can still have live JIT frames on its native stack."* The helper-window
scan there recovers such a thread's **roots** after the barrier, but nothing
lets the **barrier itself complete**: the thread is counted in `expected`,
`take_over_pass` cannot freeze it (its `Rip` is not inside a JIT range, so the
forcible-takeover pass skips it → `taken=0`), and it never cooperatively
arrives → infinite spin in `stw_take_over_and_wait`.

**Recommended next step (deliberately NOT attempted this session — this is the
harder, still-open half of BUG-03: deep GC-barrier/takeover work with high
regression risk, and the host was too SSH-unstable to iterate a GC-internals
change safely):** the fix must make a JIT-thread parked-in-native either (a)
register as `gc_barrier`-blocked at the JIT→park boundary (so it is excluded
from `expected`, the same way `NativeContextImpl::park` excludes an
interpreter park), or (b) be *excused* from the barrier by the takeover the way
a frozen in-JIT peer is (identity-matched against `counted_os_tids`), since it
holds live JIT frames but cannot be frozen at its libc `Rip`. Option (a) is
cleaner but requires the JIT's park/blocking-call lowering to go through the
blocked-region hook; option (b) is a `stw_take_over_and_wait` change to treat
"counted mutator, parked with JIT frames, un-freezable" as excused rather than
awaited. First confirm which park the executor worker actually uses
(`Unsafe.park` is intrinsified in the JIT vs. calling `native_unsafe_park` →
`ctx.park` → `enter_blocked`; the observed `blocked=false` proves the executor
is NOT reaching `ctx.park`, so the JIT is either intrinsifying the park or the
wait is on an internal `parking_lot` primitive).

This is the current gating blocker for a JIT-on WildFly standalone (and, by
extension, the domain servers/host-controller, which are standalone-shaped
JIT-on boots). It is distinct from — and now more clearly separated than — the
no-JIT `awaitServers` propagation gap and the JIT-on domain interface-
resolution miscompile documented above.

## 2026-07-07 update (eighth session) — a THIRD, distinct WFLYSRV0082 defect found in `bin/domain.sh`'s stock config; corrects scope of the earlier JIT-miscompile note; JIT bisect attempted, did not converge

Follow-up on the sixth-session finding ("JIT-on: HC interface resolution
fails with WFLYSRV0082 ... does NOT reproduce under no-JIT"). That
observation was made entirely through the **Arquillian testsuite harness**,
whose `DomainTestSupport`-generated `host.xml`/`domain.xml` use **literal**
`<inet-address value="127.0.0.2"/>` addresses (no `${...}` expression
syntax). This session tested the **stock** `bin/domain.sh`/`bin/standalone.sh`
config instead (`<inet-address value="${jboss.bind.address.management:127.0.0.1}"/>`)
and found a **third, separate** interface-resolution defect that reproduces
identically regardless of JIT state or bind address:

```text
[Host Controller] DEBUG [org.jboss.as.server.net] Starting NetworkInterfaceService
[Host Controller] ERROR [org.jboss.as.controller.management-operation] WFLYCTL0013: Operation ("add") failed - address: (["host"=>"primary","core-service"=>"management","management-interface"=>"http-interface"])
  - failure description: {"WFLYCTL0412: Required services that are not installed:" => [""], "WFLYCTL0180: ... => ["service  is missing []"]}
[Host Controller] ERROR ... address: (["host"=>"primary","interface"=>"management"])
  - failure description: {"WFLYCTL0080: Failed services" => {"" => "WFLYSRV0082: failed to resolve interface management"}}
```

Reproduced with `bin/domain.sh` (both default `127.0.0.1` binding and
explicit `-bmanagement=127.0.0.2`) under **both** JIT-on and
`CRATONVM_DISABLE_JIT=1` — same failure every time, ruling out JIT and the
specific bind address as factors for THIS variant.

**What was ruled out this session, with direct probes:**
- `ModelNode` expression resolution (`${jboss.bind.address.management:127.0.0.1}`)
  resolves correctly to `"127.0.0.1"` on both HotSpot and CratonVM — confirmed
  via a standalone `org.jboss.dmr.ModelNode.resolve()` probe.
- `ServiceName.toString()`/`.equals()`/`.hashCode()` all work correctly
  against CratonVM's synthetic `ServiceName` mirror (which unconditionally
  intercepts `of`/`append`/`getCanonicalName`/`getParent` — NOT gated behind
  `CRATONVM_MSC_REAL_START`, so this runs on every boot). A direct probe
  (`ServiceName.of("jboss","network","interface","management")`) round-trips
  `toString()`/`equals()` identically on both VMs.
- `NetworkInterfaceService.resolveInterface(OverallInterfaceCriteria)`
  invoked directly via reflection with the exact same JVM flags as the
  Host Controller resolves fine on both VMs (confirmed in the sixth-session
  update above).

**Not yet found:** the `"service  is missing []"` (two spaces = an empty
`ServiceName`, `[]` = an empty dependency list) means some REAL MSC service
registration during boot is keyed by a genuinely empty-segment `ServiceName`
— i.e. a real value that should have carried a service reference (most
likely the `http-interface` management-interface's dependency on the
"management" `NetworkInterfaceService`, wired via the model's
`<socket interface="management" .../>` attribute) collapsed to zero segments
somewhere in the real `org.jboss.as.controller`/model-processing bytecode
between reading that attribute and constructing the dependency's
`ServiceName`. This is NOT a `ServiceName`-machinery bug (ruled out above);
it must be upstream, in whatever code builds a composite `ServiceName` from
a model attribute value during capability/socket-binding resolution. Not
pinned to a specific class/method this session.

**JIT bisect on the testsuite's own WFLYSRV0082 (literal-address config,
the ORIGINAL sixth-session finding) — attempted, did not converge.** Ran
`CRATONVM_JIT_DENY=org/jboss/as/controller/interfaces/,org/jboss/dmr/,java/net/`
against `DefaultConfigSmokeTestCase#testStandardHost` under the full E2E
harness. The run silently died with zero output past the JUnit test-class
header — no `BUILD SUCCESS`/`FAILURE`, no crash dump, no nested JVM left
alive — most likely because denying JIT wholesale for `org/jboss/dmr/`
(an extremely hot-path package touched by nearly every model operation) is
not a safe bisection axis by itself, or coincided with host instability
(this session's Azure host had frequent SSH connection resets and at least
one instance of a detached background launch dying without `disown -a`).
Not re-attempted after two consecutive silent deaths — this needs a
narrower, single-class `CRATONVM_JIT_BISECT_SKIP=Class.method` bisect
(rather than whole-package `CRATONVM_JIT_DENY`) on a quieter host, or a
live `gdb`/deopt-trace capture of the actual HC process at the moment of
its `WFLYSRV0082` failure under the testsuite's literal-address config,
which was never attempted directly (only the domain.sh variant was
deep-probed this session).

**Status:** three distinct, real, reproducible `WFLYSRV0082`-shaped
defects are now known across this doc pair:
1. Testsuite literal-address config, JIT-on only (sixth session) — cause
   still unknown, JIT-implicated but not yet isolated to a method.
2. `bin/domain.sh`/`bin/standalone.sh` stock expression-config, BOTH JIT
   states, BOTH default and explicit bind addresses (this session) — cause
   narrowed to an empty-`ServiceName` dependency, not yet pinned to the
   exact model-attribute-read/ServiceName-construction site.
3. (Unconfirmed whether #1 and #2 share a root cause — the testsuite's
   no-JIT PASS on its own literal-address config is the strongest evidence
   they're different, since #2 fails under no-JIT too.)

Both docs remain OPEN. No fix landed this session; this is a documentation
and scoping pass only, to prevent a future session from re-treading the
same ground or conflating these three distinct failure modes.

## 2026-07-08 update (ninth session) - two standalone residuals fixed; HIB-CV-32 still silent; boot remains gated by STW/rollback blockers

This session picked up the open residuals from the standalone WildFly 32.0.1.Final
binary-distribution probes and fixed two concrete linkage/layout failures that were
masking the already-known STW blocker:

1. `ContextNames.bindInfoFor("java:jboss/datasources/KeycloakDS")` now returns a
   `ContextNames$BindInfo` mirror with the real WildFly field layout:
   parent `ServiceName`, binder `ServiceName`, stripped bind name, absolute JNDI
   name. The previous two-slot/string layout wrote the JNDI string where WildFly
   expected a `ServiceName`, causing `AbstractDataSourceService.getServiceName` to
   dispatch `ServiceName.getCanonicalName()` on a `String` and fail with
   `NoSuchMethodError: java/lang/String.getCanonicalName()Ljava/lang/String;`.
2. Phase-56 `Collectors.toUnmodifiableList()` / `toUnmodifiableSet()` now use the
   same synthetic collector shape as `native-collections`' stream collector engine:
   tags `1`/`2` and a four-field `java/util/stream/Collector` object. The previous
   phase-local tags/three-slot allocation could replace the core collector shape
   and later fail as `NoSuchMethodError: java/lang/Object.supplier()...` after the
   synthetic helper resolved through an `Object` identity.

Verification used the unique probe binary
`/data/data/probes/wildfly-hib-residuals-20260708-164823/bin/cratonvm-wildfly-hib-residuals-20260708-164823`
and the matching `JAVA_HOME` shim at
`/data/data/probes/wildfly-hib-residuals-20260708-164823/javahome/bin/java`.

Focused checks:

```text
cargo test -p cratonvm-native-builtins t19_2_b_context_names_bind_info_parses_absolute_name -- --nocapture
=> pass

cargo test -p cratonvm-vm --features synthetic-jdk collectors_to_unmodifiable_list_p56 -- --nocapture
=> not usable: pre-existing synthetic-jdk inline test compile drift (Arc<str>/LazyAttribute errors) prevents this target from building
```

Standalone probes after the fix:

```text
no-JIT: /data/data/probes/wildfly-hib-residuals-20260708-164823/runs/standalone-nojit-after-collector-20260708-180904.log
rc=124 (timeout)
getCanonicalName=0 Object.supplier=0 HIB-CV-32=0 NoClassDefFoundError=0 NoSuchMethodError=0 STW cross-thread=1

JIT-on: /data/data/probes/wildfly-hib-residuals-20260708-164823/runs/standalone-jit-after-collector-20260708-181811.log
rc=124 (timeout)
getCanonicalName=0 Object.supplier=0 HIB-CV-32=0 NoClassDefFoundError=0 NoSuchMethodError=0 STW cross-thread=1
```

So the two method-linkage residuals are fixed, and the original HIB-CV-32 corrupt
`Value` guard remains silent under `CRATONVM_DIAG_HIB32=1`. This document stays OPEN:
the same probes now roll into subsystem boot failures/rollback and the existing
BUG-03-family STW accounting stall. The no-JIT run reported early datasource and
Infinispan boot errors (`ModuleLoader.loadModule(ModuleIdentifier)` null receiver;
`ServiceBuilderImpl.assertNotNull` "Method parameter cannot be null") before rollback,
then wedged at `pending=1 taken=0`. The JIT-on comparison wedged at
`pending=2 taken=0`. The STW fix should be handled as a separate barrier/blocked-region
change with an identity-aware reproducer; do not conflate it with the fixed BindInfo or
collector layout issues.

## 2026-07-09 update — process-controller/STW watchdog blocker cleared; later HC boot residuals remain

Branch `codex/fix-wildfly-pc-respawn-20260709-103040` moved the hand-driven WildFly
32.0.1.Final `domain.sh` probe past the process-controller VM native/STW watchdog path.
The key fixed run used:

```text
/data/data/probes/wildfly-pc-respawn-20260709-103040/bin/java-wildfly-pc-enumset-addall-20260709-210535
/data/data/probes/wildfly-pc-respawn-20260709-103040/runs/domain-enumset-addall-20260709-210549.log
```

After rebasing this branch on current `dev`, the final verification used the same
domain probe path with `CRATONVM_MSC_REAL_START=1` set explicitly:

```text
/data/data/probes/wildfly-pc-respawn-20260709-103040/bin/java-wildfly-pc-postrebase-20260709-212221
/data/data/probes/wildfly-pc-respawn-20260709-103040/runs/domain-postrebase-mscreal-20260709-214000.log
```

Result:

```text
rc=0
[Host Controller] [msc] <- start id=3 OK
[Host Controller] TRACE ... Connected to 127.0.0.1:40695
[Host Controller] TRACE ... Sent initial greeting message
INFO [org.jboss.as.process.Host Controller.status] WFLYPC0011: Process 'Host Controller' finished with an exit status of %d
INFO [org.jboss.as.process] WFLYPC0017: Shutting down process controller
INFO [org.jboss.as.process] WFLYPC0016: All processes finished; exiting
```

The rebased verification also exits `rc=0`, reaches the Host Controller
process-controller connection (`Connected to 127.0.0.1:42563`, `Sent initial greeting
message`), and then shuts the process controller down normally after the later Host
Controller residuals trigger `System.exit(99)`.

Important intermediate residuals were also cleared in this session:

```text
/data/data/probes/wildfly-pc-respawn-20260709-103040/runs/domain-qname-essential-20260709-202257.log
  NoSuchMethodError Executors.newScheduledThreadPool(int, ThreadFactory)

/data/data/probes/wildfly-pc-respawn-20260709-103040/runs/domain-verify-debug-20260709-204532.log
  [cratonvm-verify] org/jboss/as/controller/ModelController.<clinit>: expected java/security/Permission, found ControllerPermission

/data/data/probes/wildfly-pc-respawn-20260709-103040/runs/domain-npe-stack-20260709-205240.log
  NPE in ConcreteResourceRegistration.registerSubModel at PathElement.getValue()

/data/data/probes/wildfly-pc-respawn-20260709-103040/runs/domain-path-address-20260709-210020.log
  NoSuchMethodError java/util/EnumSet.addAll(Collection)
```

The fixes in this branch cover the synthetic socket read path that had made the Host
Controller see EOF before the process-controller greeting, the no-arg `Object.wait()`
bridge needed by the process protocol pipe, `QName` construction, scheduled executor
factory overloads, WildFly controller permission verifier edges, a `PathAddress`
varargs bridge, and `EnumSet.addAll(Collection)`.

The remaining front-line blockers are now ordinary Host Controller boot residuals, not
the original STW watchdog/respawn lifecycle failure:

```text
NoSuchMethodError javax/management/AttributeChangeNotification.<init>(Object,long,long,String,String,String,Object,Object)
ClassCastException in org.jboss.as.server.deployment.ContentCleanerService.start(ContentCleanerService.java:101)
NoSuchMethodError java/io/FileInputStream.<init>(java.io.File)
WFLYHC0034: Host Controller boot has failed in an unrecoverable manner; exiting
```

Keep this document under `docs/known-issues` for now. The specific process-controller
watchdog blocker is cleared, but the older HIB-CV-32 heap-corrupt/sustained-load
question still has not been revalidated because WildFly domain boot now stops at later
Host Controller configuration-loading/JMX/content-cleaner gaps before reaching a long
managed-server workload.

## 2026-07-10 update — one real blocker fixed (`Level.parse`), a second NEW regression
## found and NOT fixed; the front-line residuals below could not be re-observed live

Picked this doc up specifically to fix the four front-line residuals from the
2026-07-09 update above (`AttributeChangeNotification` NoSuchMethodError,
`ContentCleanerService` ClassCastException, `FileInputStream(File)`
NoSuchMethodError, cascading `WFLYHC0034`). Working on branch
`fix/wildfly-hib32-residuals-20260710` (Azure host, separate worktree from
`/data/data/cratonvm`), against a **freshly-downloaded, pristine** WildFly
32.0.1.Final distribution (`/data/data/probes/wildfly-hib32-20260710/dist/`) —
the shared `/data/data/wildfly-dist` copy several prior sessions reused has
accumulated `.bak`/regenerated config files from those sessions' own runs and
is no longer a clean baseline; a fresh download from
`github.com/wildfly/wildfly/releases` avoids that ambiguity for future
sessions too.

### Fixed: `java.util.logging.Level.parse(String)` threw for every name, including standard JDK constants

Reproducing this doc's exact harness recipe against a pristine distribution
(not the shared, already-mutated one) hits a **different, earlier** blocker
than the four residuals above: Host Controller's own `host.xml`/`domain.xml`
parsing fails immediately with

```text
ERROR [org.jboss.as.host.controller] WFLYCTL0085: Failed to parse configuration
ParseError at [row,col]:[69,21]
Message: "WFLYLOG0026: Log level WARN is invalid."
```

Standalone repro (no WildFly involved) confirmed this is a genuine, universal
CratonVM bug, not a config or WildFly issue: `Level.parse("WARNING")` — a
**standard** `java.util.logging.Level` constant, not even a JBoss LogManager
extension — throws `IllegalArgumentException: Bad level "WARNING"` under
real-JDK mode. Real JDK 25's `Level.parse` resolves names through
`KnownLevel.findByName`, which throws an internal `NullPointerException`
("Cannot invoke isNamed on null" on a `Module` reference — the same family of
gap already tracked in `docs/internal/gaps/kc16-blocker-map.md`'s KC16
investigation, `Class.getModule()` synthesis being incomplete) before it can
match anything by name; `Level.parse`'s own catch-all then reports the
generic `IllegalArgumentException` regardless of whether the name was a
genuine standard constant or a JBoss extension (`WARN`/`ERROR`/`FATAL`/etc).

Fixed with a native override for `Level.parse(String)`
(`native-builtins/src/logmanager.rs::native_level_parse`, forced to win over
real bytecode via `force_native_over_real_jdk_bytecode` in
`vm/src/runtime/interpreter.rs`) that resolves both the 9 standard
`java.util.logging.Level` constants and `org.jboss.logmanager.Level`'s 5
extensions directly from their static fields — the same technique already
used for the adjacent `LogContext.getLevelForName` workaround
(`native_jboss_log_context_get_level_for_name`, same file). Verified
standalone (`Level.parse("WARNING")`/`Level.parse("WARN")` both now resolve
correctly) and confirmed the `WFLYLOG0026` parse failure no longer occurs
against the pristine distribution.

### NEW regression found (NOT fixed): `Executors.newSingleThreadExecutor()`/
### `newFixedThreadPool()`/`newCachedThreadPool()` — `.execute()` NPEs on `ctl`

With the logging fix in place, `domain.sh` boot reaches a **different,
still-earlier** blocker than either the front-line residuals or the
known BUG-03-family STW/JIT-takeover stall: the outer process-controller VM's
own "Read thread" (`org.jboss.as.process.protocol.ConnectionImpl$2.run`,
spawned via a plain `Executors`-backed pool to read the Host Controller
child's initial greeting) dies with an uncaught
`NullPointerException: Cannot invoke "AtomicInteger.get()" because "this.ctl"
is null` (decoded via a new `CRATONVM_DBG_UNCAUGHT` `toString()` print added
this session, `vm/src/vm/vm_exec.rs`) — a **completely standalone-reproducible
regression**, unrelated to WildFly, bisected (via fresh from-scratch rebuilds,
not cached binaries) to somewhere in `be605560..f28d6ae6`
(2026-07-09). Full writeup, evidence, and the three reverted (ineffective)
fix attempts:
[`threadpoolexecutor-execute-npe-on-ctl-regression.md`](threadpoolexecutor-execute-npe-on-ctl-regression.md).

This is now the **actual gating blocker** for re-observing this doc's own
front-line residuals live: it kills the process-controller before Host
Controller's greeting is processed, which is exactly what produces the
"T19.H1 watchdog: main thread is in native (Rust) code" hang this doc's
2026-07-09 update already described as a known, still-present shape
(`process-controller/STW watchdog blocker cleared; later HC boot residuals
remain` was evidently describing a *different* trigger of the same-shaped
hang — this session's fresh pristine-distribution runs hit it deterministically,
100% of attempts, whereas the shared/mutated distribution the 2026-07-09
session used apparently avoided it, most likely by luck of timing/config
state rather than by being fixed).

### Status of the four front-line residuals from 2026-07-09

**Not re-verified either way this session** — boot never got far enough,
blocked by the two issues above. They remain the best-known next blocker
once the `ThreadPoolExecutor` regression is fixed; nothing in this session's
findings contradicts the 2026-07-09 analysis of them (the log lines quoted
there are still the most recent direct evidence for all four).

### Recommended next steps, in order

1. Fix the `ThreadPoolExecutor.execute()` regression
   (`threadpoolexecutor-execute-npe-on-ctl-regression.md`) — ideally with a
   live debugger this time, since three separate print-tracing attempts in
   this session failed to even locate which dispatch function handles the
   call.
2. Re-run this doc's harness recipe (a **pristine** WildFly 32.0.1.Final
   distribution — do not reuse a previously-booted copy, see above) with
   `CRATONVM_MSC_REAL_START=1`, and confirm `host.xml`/`domain.xml` parsing
   and the process-controller/Host-Controller handshake both complete
   cleanly.
3. Only then will boot reach the point where the four front-line residuals
   (`AttributeChangeNotification`, `ContentCleanerService`, `FileInputStream(File)`,
   `WFLYHC0034`) can be re-observed and actually fixed — investigate each via
   the same technique that worked this session (`CRATONVM_DBG_UNCAUGHT=1`
   plus, where useful, `CRATONVM_DBG_MCL=1` for the two classloading-shaped
   NoSuchMethodErrors — `javax/management/AttributeChangeNotification` and
   `java/io/FileInputStream(File)` both looked, in the 2026-07-09 log, like
   the JBoss-Modules `ModuleClassLoader` resolving them to synthetic stubs
   rather than real JDK bytecode; not re-confirmed this session, still the
   best lead for whoever picks this up next).

## 2026-07-10 update (this session) — the gating `ThreadPoolExecutor` regression is FIXED; the four residuals still need a live domain boot to re-observe

Picked this up specifically to clear step 1 of the 2026-07-10 recommended
steps above. Root-caused and fixed on branch `fix/wildfly-hib32-gate-20260710`
(Windows worktree; no Azure host access this session) — full writeup in
`docs/internal/threadpoolexecutor-execute-npe-on-ctl-regression-FIXED.md`
(a narrower, `execute()`-only fix for the same bug landed on `dev`
independently while this session was in progress, branch
`fix/tpe-npe-dispatch-20260710`; this generalizes it to `submit()`/
`shutdown()` too — see that doc's own "later same day" section for the
reconciliation). Short version: the earlier bisection to `f28d6ae6` was a red herring — the
actual cause is a `native-api/src/registry.rs` registration-time gate
(`f157de8a`, 2026-06-17) that unconditionally dropped every native
registered on class name `java/util/concurrent/ThreadPoolExecutor` in
real-JDK mode, including `execute`/`submit`/`shutdown`, which starved
CratonVM's own synthetic `Executors.*` placeholder objects (they share that
exact class name) of their native overrides. An independent, parallel
Elasticsearch-suite investigation hit and documented the identical bug the
same day — see
`docs/internal/elasticsearch-suite/ES-FAIL-20260710-executors-factory-synthetic-mainlock-npe-FIXED.md`
(a THIRD independent fix, landed while this session was mid-verification,
took a different tack for that specific doc — constructing genuinely real
`ThreadPoolExecutor`/`Thread` objects via their real constructors — which is
also a legitimate resolution and doesn't conflict with this fix). A related,
separately-filed dispatch bug
(`docs/internal/threadpoolexecutor-execute-dispatch-degrades-to-synchronous-FIXED.md`
— any real `ThreadPoolExecutor.execute()` losing async semantics, found by
yet another session verifying the above) turned out to share this exact same
root cause and is fixed by the same change.
Fix moves the real-vs-synthetic distinction from registration time to
dispatch time via a new `NativeContext::invoke_virtual_bytecode_only`
escape hatch. Verified with three standalone probes (no WildFly): the
original `ExecProbe.java` repro, the ES doc's `submit()`/`shutdown()` repro,
and a genuinely-real `new ThreadPoolExecutor(...)` (confirms the original
`f157de8a` intent — real executors still get real bytecode — still holds).
`cargo test -p cratonvm-native-api --lib` (179 tests, including the
pre-existing STPE/EnumSet-drop coverage this change didn't touch) and
`cargo test -p cratonvm-native-builtins --lib` both pass. Merged to `dev`.

**The four front-line residuals themselves were NOT re-observed this
session** — this Windows box has no WildFly Maven/domain-boot harness set
up and no access to the Azure Linux host used by every prior session on
this doc (its IP is ephemeral; needs to be re-obtained from whoever's
running that host). Two narrower, WildFly-independent checks were done
instead, since both looked like plain-JDK classloading gaps per the
2026-07-09 update's own hypothesis:

- **`javax.management.AttributeChangeNotification` and
  `java.io.FileInputStream(File)`: standalone probes (JDK-only, no
  WildFly/JBoss-Modules involved) both construct and use these classes
  correctly** on the fixed binary — `new AttributeChangeNotification(source,
  1L, 2L, "msg", "attrName", "attrType", "oldVal", "newVal")` and `new
  FileInputStream(File)` (opening a real temp file) both succeed with no
  NoSuchMethodError. This rules out a *plain* JDK-bytecode/native-registry
  bug for either constructor and reinforces the 2026-07-09 hypothesis that
  the NoSuchMethodError is specific to how WildFly's JBoss Modules
  `ModuleClassLoader` resolves these classes (a per-module class definition
  that isn't the same one these standalone probes exercise) — a live
  domain boot (or at minimum a JBoss-Modules-driven classloading harness,
  which this session did not have time to stand up) is still needed to
  reproduce and fix this pair; the probes at least save the next session
  from re-checking "is this a generic bug" first.
- **`ContentCleanerService.start(StartContext)` (WildFly's own
  `org.jboss.as.server.deployment.ContentCleanerService`,
  `wildfly-server-24.0.1.Final.jar` inside the 32.0.1.Final distribution):
  disassembled with `javap -c -l`** to at least scope the crash without a
  live boot. Line 101 is entirely the sequence `aload_0; getfield
  clientFactorySupplier:Ljava/util/function/Supplier;; invokeinterface
  Supplier.get:()Ljava/lang/Object;; checkcast
  org/jboss/as/controller/ModelControllerClientFactory` — i.e. the
  `ClassCastException` is on the value an MSC-injected
  `Supplier<ModelControllerClientFactory>` capability field hands back from
  `.get()`. This points at CratonVM's MSC capability-injection machinery
  (`native-builtins/src/jboss_msc.rs`) constructing or wiring that
  particular `Supplier` with the wrong value type, but confirming that (and
  ruling out it being isolated to just this one capability) needs a live
  `CRATONVM_MSC_REAL_START=1` boot trace, not static bytecode reading —
  not attempted further this session.

**Recommended next step:** get access to a Linux host with the WildFly
Maven/domain-boot harness (or rebuild one — a pristine WildFly 32.0.1.Final
distribution download is enough per the 2026-07-10-earlier-session recipe
above; no Maven needed for the `bin/domain.sh` hand-driven path) and re-run
this doc's harness recipe now that the gating regression is cleared. If
`host.xml`/`domain.xml` parsing and the process-controller/Host-Controller
handshake complete, the four front-line residuals should become observable
again — use `CRATONVM_DBG_UNCAUGHT=1`/`CRATONVM_DBG_MCL=1` as the 2026-07-09
update recommended, and for `ContentCleanerService` specifically, trace
which capability's `Supplier` resolves to the wrong type first (add tracing
in `jboss_msc.rs`'s capability-injection path rather than guessing further
from bytecode alone).


## 2026-07-10 update (Azure host session, later same day) — one real blocker found+fixed (Cleaner), a second found but not yet fixed (Host Controller SIGSEGV); the four residuals still not reached

Continued directly from the "gate is fixed" update above, now with Azure host access
(`victor@20.83.144.174`, branch `fix/wildfly-residuals-20260710`, worktree per the standing
isolated-worktree workflow). Re-ran this doc's own harness recipe (pristine WildFly 32.0.1.Final,
`bin/domain.sh`, `CRATONVM_MSC_REAL_START=1`).

**Confirmed the gate fix works**: process-controller/Host-Controller handshake now completes
cleanly (no more `ThreadPoolExecutor` NPE killing the process-controller's read thread).

**New blocker #1, found and FIXED**: Host Controller crashed immediately after with a
`NullPointerException` in `java.lang.ref.Cleaner.register()` → `CleanerImpl.getCleanerImpl()`
returning null, while `org.jboss.msc.service.ServiceContainer$Factory.create()` was setting up its
shutdown hook. Same bug class as the `ThreadPoolExecutor` regression: a synthetic `Cleaner.create()`
native (meant only as a fallback stub) was winning over real bytecode for this **static** factory
method, returning a Cleaner with its real `impl` field left null, which real `register()` bytecode
then NPE'd on. Fixed by extending the `drop_real_layout_synthetic` registry gate to
`java/lang/ref/Cleaner`/`Cleaner$Cleanable`, same pattern already used for `ThreadPoolExecutor`.
See `docs/internal/java-lang-ref-cleaner-static-native-half-initialized-object-FIXED.md`.

**New blocker #2, found but NOT fixed**: with the Cleaner fix in place, boot progresses much
further — extensions parse, Elytron initializes, `host=foo:add()` op runs — then Host Controller
**segfaults** (confirmed via `strace -f -e trace=exit_group`: genuine `SIGSEGV`/`SEGV_MAPERR`,
`si_addr=NULL`, not a Java exception or clean exit) and respawns forever until the boot times out.
Confirmed independent of the JIT (`CRATONVM_DISABLE_JIT=1` reproduces identically). Live-gdb
(temporarily relaxing `ptrace_scope`, restored afterward) pinned the crash to a specific,
reproducible instruction sequence — a monomorphic inline-cache dispatch stub dereferencing a NULL
receiver — but the exact Rust source line was not identified before this session's budget ran out.
Filed as `docs/known-issues/wildfly-domain-hostcontroller-sigsegv-inline-cache-null-receiver.md`,
with the live-gdb recipe and disassembly included for whoever continues.

**The four original front-line residuals (`AttributeChangeNotification`, `ContentCleanerService`,
`FileInputStream(File)`, `WFLYHC0034`) still have not been re-observed** — this SIGSEGV is now the
gating blocker, one step later in the boot sequence than the Cleaner bug, itself one step later
than the original `ThreadPoolExecutor` regression. Recommended next step: fix the SIGSEGV per that
doc's own recommended next steps (get the exact crash-site source line via `addr2line` against the
captured instruction offset, since it's ASLR-base-independent and was confirmed constant across
multiple captures), then re-run this doc's harness recipe again.

## 2026-07-10 update (later same day) — SIGSEGV investigation continued, still not fixed; no new information for this doc

Picked up the gating SIGSEGV doc (`wildfly-domain-hostcontroller-sigsegv-inline-cache-null-receiver.md`)
per its own recommended next step. Made substantial progress there (crash site conclusively
identified as a JIT-compiled `ReentrantLock.lock()`, a specific named root-cause candidate found
in classloading field-layout padding, and a separate confirmed finding that env vars do not reach
the Host Controller child process) but did **not** land a fix — see that doc's own 2026-07-10
"new session" entry for full detail. The four front-line residuals this doc tracks
(`AttributeChangeNotification`, `ContentCleanerService`, `FileInputStream(File)`, `WFLYHC0034`)
remain unreachable and unobserved; nothing changed for this doc specifically. This doc stays OPEN,
still gated by the sibling SIGSEGV doc.


## 2026-07-10 update (third session, same day) — the gating Host Controller SIGSEGV is FIXED; residual hunt is unblocked

The Host Controller SIGSEGV that gated this doc's four front-line residuals is fixed — root
cause: fabricated `(0, false)` compact-field slots poisoning the JIT's inline getfield
(reference field read as a 32-bit sign-extended slice of a `Value` cell → bogus non-null
receiver into the invoke inline cache; crashing method `ReentrantLock.lock()`). Full write-up:
`docs/internal/wildfly-domain-hostcontroller-sigsegv-inline-cache-null-receiver-FIXED.md`
(also fixes a `BufferedReader.readLine` global-mutex-across-blocking-read starvation that made
the post-fix Host Controller look silent).

With both fixes the domain boot invokes `host=foo:add()` once, never respawns, and proceeds
into management-model territory. The four front-line residuals
(`AttributeChangeNotification`, `ContentCleanerService`, `FileInputStream(File)`,
`WFLYHC0034`) did **not** re-appear verbatim in a 150 s bounded run; what surfaces instead:
`WFLYCTL0013 Operation("add") failed` on
`host=primary/core-service=management/management-interface=http-interface`
(`IllegalStateException: Container is down`) and a `StackOverflowError` from
`ScheduledThreadPoolExecutor.shutdown` recursing into itself (dispatch-bug shaped, sibling of
the prior TPE fixes). Those are the new front line for this doc's hunt.


## 2026-07-10/11 update (fourth session) — two MSC bugs fixed; both "third session" residuals cleared; new front line is the pre-existing, separately-tracked BUG-03 STW/JIT-takeover stall

Picked up where the third session left off: `WFLYCTL0013 Container is down`
(`IllegalStateException` on the `http-interface` `add` op) and a
`StackOverflowError` from `ScheduledThreadPoolExecutor.shutdown` self-recursion.
Root-caused via `CRATONVM_DBG_MSC=1` trace diffing (extracted `-> start id=N` /
`<- start id=N OK` lines, diffed started-vs-completed ids to confirm the missing
services never even *started*, not started-and-failed, then cross-referenced
`[msc] install` lines for the relevant service names). Found two independent,
real bugs in the from-scratch MSC (`native-builtins/src/jboss_msc.rs`), both
fixed and pushed to `dev`:

1. **`ServiceController.provides()` had zero native backing.** It's a real MSC
   1.5.x interface method with no default implementation; any caller through
   the synthetic mirror died with `AbstractMethodError:
   org/jboss/msc/service/ServiceController.provides()Ljava/util/Set;`. The
   `jboss.remoting.endpoint.management.management.operation.handler` service's
   own start() path calls it — the resulting `AbstractMethodError` marked that
   service FAILED, cascading into `WFLYCTL0459: Triggering roll back due to
   missing management services`, which is why the *later*, unrelated
   `http-interface` `add` then saw `IllegalStateException: Container is down`
   (the management transaction was already rolled back). Fixed by implementing
   `native_service_controller_provides()`: returns the controller's primary
   `ServiceName` plus every alias reverse-scanned from `ContainerState.aliases`.
   Commit `bd5626a9`.

2. **OnDemand/Lazy dependencies of Active services were never demanded.**
   `take_ready_start()`'s `can_start()` check requires every dependency to be
   `Up`, but an `OnDemand`/`Lazy` service only becomes start-eligible once
   `demanded == true`. The only production call site for `demand()` was
   `ServiceController.setMode(Mode.ACTIVE)` — so an OnDemand service reachable
   *only* via a dependency edge (never given an explicit `setMode(ACTIVE)` by
   any Java code) never got demanded: a permanent deadlock. Concretely,
   `org.wildfly.management.http.extensible` (OnDemand) is only reachable via
   its Active dependent `...extensible.shutdown`'s dependency edge, so it never
   started, and the whole http-management service chain never came up. Real
   MSC treats an Active/Passive (or demanded Lazy/OnDemand) service's mere
   dependency edge to an OnDemand/Lazy service as an implicit demand — added a
   cheap pre-pass at the top of `take_ready_start()` that propagates demand
   along dependency edges before computing start-eligibility (converges within
   a few polling calls even for multi-level OnDemand chains). Commit
   `f3d69e2b`.

Both merged into `dev` at `13011cff`, verified against `origin/dev` with a
`git log -S<symbol>` shadowing check (clean — neither `jboss_msc.rs` nor these
symbols were touched by any concurrent commit since the branch point) and
`cargo check -p cratonvm-native-builtins` + `cargo test -p
cratonvm-native-builtins --lib` (2960 passed, same 5 pre-existing unrelated
failures as the `origin/dev` baseline, plus one **confirmed-flaky**
parallel-test-isolation failure —
`lang_system::checkexec_security_tests::denying_sm_blocks_processbuilder_start_stub`
races on the process-wide `SECURITY_MANAGER` `Mutex` static against other
`checkexec_security_tests` running concurrently in sibling threads; passes
deterministically under `--test-threads=1`; pre-existing test-infra gap,
unrelated to this session's changes, not fixed here).

**Live verification (Azure host, pristine WildFly 32.0.1.Final,
`CRATONVM_MSC_REAL_START=1`, binary built from `dev` @ these two commits):**
`WFLYCTL0459`, `Container is down`, the `AbstractMethodError`, **and** the
`StackOverflowError` from `ScheduledThreadPoolExecutor.shutdown` are **all
gone (0 hits)** — the boot log grew from a ~105-line stall to 595+ lines,
reaching `Invoking domain.xml ops` and activating multiple subsystem
extensions (JAX-RS, Transactions, Weld, JSF(Mojarra), Datasources,
ResourceAdapters) before stalling. A 30 s re-check confirmed the log genuinely
stops growing there, not just slow progress.

**New front line: the boot now stalls at the pre-existing, separately-tracked
BUG-03 STW/JIT-takeover family** (`STW cross-thread JIT takeover is still
waiting for cooperative mutators rounds=64 pending=7 taken=0`, from
`vm/src/runtime/interpreter.rs`). Per this investigation's standing scope
guardrail, BUG-03 itself is **not** chased here — but I did check whether it
can be routed *around* (not fixed) via `CRATONVM_DISABLE_JIT=1`, since the
stall is explicitly JIT-related: it cannot. With JIT disabled the boot fails
**much earlier** and differently — `org/jboss/modules/Main.<clinit>` throws
`ExceptionInInitializerError` wrapping an `UnsatisfiedLinkError` for `Missing
native method in real-JDK mode method=java/io/InputStreamReader.<init>
(Ljava/io/InputStream;Ljava/nio/charset/Charset;)V`, before JBoss Modules even
finishes bootstrapping. This looks like a real, separate, interpreter-only
native-registration gap (the 2-arg `InputStreamReader(InputStream, Charset)`
constructor apparently isn't registered/reachable in real-JDK mode when the
JIT never compiles the caller) — **not investigated further, not fixed, not
filed as its own doc** (out of scope for this session); noted here only to
save a future session from re-trying the same "disable JIT to route around
BUG-03" idea and hitting the same dead end.

**The four original front-line residuals (`AttributeChangeNotification`,
`ContentCleanerService`, `FileInputStream(File)`, `WFLYHC0034`) and the
`HIB-CV-32` sustained-load / `CRATONVM_DIAG_HIB32=1` corrupt-Value-cell check
still have not been reached** — boot has now moved several phases past where
they were originally observed (bootstrap → extensions → Elytron →
`host=foo:add()` → Cleaner → SIGSEGV → `provides()`/demand-propagation →
domain.xml subsystem activation), each phase revealing the next blocker
in sequence. It remains unknown whether those four residuals are still
reachable in their original form or have been superseded, because boot has
not yet gotten past BUG-03 to find out. This doc stays OPEN, now gated by
BUG-03 (tracked in its own existing doc/section, not duplicated here).

No lingering `domain.sh`/Host Controller/Process Controller processes were
left running on the probe host after this session (verified via `ps aux`
post-run).
