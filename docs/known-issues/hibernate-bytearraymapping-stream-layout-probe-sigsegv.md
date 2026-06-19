# `ByteArrayMappingTests` SIGSEGV — native-stack overflow from CV-specific deep recursion in `Byte[]`→H2-array binding

**Severity:** High (hard, uncatchable crash, `EXCEPTION_ACCESS_VIOLATION` / SIGSEGV, rc=139).
**Status:** 🔴 OPEN — root cause CONFIRMED (see "CONFIRMED mechanism" below); deterministic, reproduces solo
on latest dev. Two non-trivial root causes, neither a safe quick fix — handoff/decision needed.
**Mode:** Interpreter (JIT-off; `CRATONVM_DISABLE_JIT=1`).
**HotSpot (JDK 25):** PASS (persists the entity with shallow depth).

## Symptom

`org.hibernate.orm.test.mapping.basic.ByteArrayMappingTests` crashes the VM:

```
#  EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005) at pc=0x00007FF720686994
```

The crash is preceded by a **flood** (hundreds) of identical gen_heap guard WARNs:

```
WARN cratonvm::gc::guard: gen_heap::get_field / set_field: out-of-bounds field read/write dropped
  (caller used slot index past receiver's layout)
  obj=0x… index=1 num_slots=1 class_id=ClassId(275) class_name=java/util/stream/Stream real_field_count=Some(0)
```

i.e. something repeatedly reads/writes **slot index 1 on a `java/util/stream/Stream` object that has 0/1
slots** ("speculative collection-layout probe dispatched on a non-matching receiver type"). The
`gen_heap::guard` *drops* each individual OOB access, but the underlying mis-dispatch eventually performs a
raw access that the guard does **not** intercept → hard SIGSEGV. The hs_err frame shows a ShadowStack /
native-frame context (VM internals), not a Java NPE.

## Repro

```
CRATONVM_DISABLE_JIT=1 cratonvm --java-home <jdk25> @common.args -Dcraton.batch=1 \
  CratonRunner <list-with-only ByteArrayMappingTests> 0
```
Reproduces **strictly alone** (census fully drained, 0 other CV procs) → rc=139. Not contamination.
DDL + first inserts run (`create table EntityOfByteArrays`, `EntityOfByteArrays` insert) before the fault,
so the crash is during entity/Stream processing, not bootstrap.

## CONFIRMED mechanism (2026-06-19, release-with-debug symbolized)

The crash is a **native (Rust) call-stack overflow**, not the operand stack and not (directly) the Stream
slot read. Symbolized native frames show a tight recursive cycle repeating to the guard page, faulting in
`ValueStack::push` (which itself has a correct `max_size` check — it just happened to run when the native
guard page was hit):

```
ValueStack::push  (value_stack.rs:328)          <- faults (guard page)
  execute_instruction (interpreter.rs:8359)
  execute_frame      (interpreter.rs:6115)
  execute            (interpreter.rs:3730)  ┐
  invoke_on_class_shared_inner (vm_exec.rs) │ recursive cycle,
  invoke_or_native   (vm_exec.rs:6301)      │ repeats to overflow
  try_lambda_dispatch (interpreter.rs:13106)│  (~6 large Rust frames
  execute_invoke_kind (interpreter.rs:11574)│   per Java call level)
  execute_frame      (interpreter.rs:6043)  ┘
```

Triggered while **binding the INSERT parameters** — specifically the `Byte[] boxed` column bound as an H2
`tinyint array` (last SQL before the fault is the `insert into EntityOfByteArrays (boxed, …)`). A flood of
`gen_heap get_field OOB index=1 on a 1-slot ClassId(275) java/util/stream/Stream` WARNs (each a *different*
object) accompanies the recursion — a CV-specific Stream mis-handling (synthetic streams are 2-slot;
something produces/processes a 1-slot real-layout Stream) drives a deep lambda-dispatch recursion that
**HotSpot does not have** (HotSpot persists this entity with shallow depth and PASSES).

### Why the StackOverflowError guard (H6) fails to catch it

`derive_exec_depth_ceiling` (interpreter.rs:2079) assumes `NATIVE_STACK_BYTES_PER_EXEC_LEVEL = 8 KiB`. The
**lambda-dispatch path here burns ~100–200 KiB of native stack per Java level** (6 large nested Rust frames),
so on the (worker) thread it runs on, the native stack overflows at **~40 levels**:

- `CRATONVM_EXEC_DEPTH_CEILING=40/100/400` → **SIGSEGV** (native stack dies before the counter trips).
- `CRATONVM_EXEC_DEPTH_CEILING=5` → catchable `StackOverflowError` (but that's below *normal* Hibernate
  nesting, so it false-trips during SessionFactory build).

The native-overflow depth (~40) is **lower than normal operation depth** (~200), so a *uniform per-level
counter* fundamentally cannot distinguish this heavy path from legitimate deep recursion (binaryTrees etc.).
The guard is structurally unable to protect this path → uncatchable SIGSEGV.

## Two root causes, neither a quick localized fix

1. **CV-specific deep recursion** during `Byte[]`→H2-array INSERT binding, induced by the 1-slot class-275
   Stream mis-handling (HotSpot doesn't recurse here). FIX = find the Java recursion cycle (the harness
   swallows the forced-SOE stack; the hard fault yields no Java frames — needs gen_heap-guard backtrace
   instrumentation on the ClassId(275) OOB, or a path that prints the uncaught-SOE Java stack) and stop the
   mis-sized Stream from being produced/streamed. This is the one that would make the test PASS like HotSpot.
2. **Structural SOE-guard gap**: the per-level native-stack estimate (8 KiB) is 12–25× too low for the
   lambda-dispatch path, and a uniform counter can't catch a heavy path that overflows below normal depth.
   FIX = a real native-stack-pointer probe (stack banging) in `execute`, throwing a catchable SOE within a
   guard-page safety margin — robust regardless of per-level cost — instead of (or in addition to) the
   counter. Risky on the interpreter hot path; needs care not to regress throughput.

Raising `NATIVE_STACK_BYTES_PER_EXEC_LEVEL` globally is **not** safe — it lowers the ceiling for all
interpreter recursion and would regress legitimate deep-recursion workloads (binaryTrees) that the comment at
interpreter.rs:2065 explicitly calls out.

### Diagnostic recipe (for the next session)
- Build `--profile release-with-debug`; reproduce solo; symbolize the VEH frames with
  `CRATONVM_SYMBOLIZE=0x<rva>,… cratonvm` (plain `--release` has no symbols).
- `CRATONVM_EXEC_DEPTH_CEILING=5` proves the cycle goes through guarded `execute` (throws catchable SOE).
- Repro is the single class `mapping.basic.ByteArrayMappingTests` (JIT-off), reproduces strictly solo.

## Related

Distinct from the JSON-function `al_state` foreign-receiver SIGSEGV (fixed this session) — that was a native
reading ArrayList slots off a non-ArrayList; this is a Stream layout-probe. Same *family* (native slot
computation on a non-matching receiver), different native. The sibling crash
`onetoone.nopojo.DynamicMapOneToOneTest` exits **rc=127** (abnormal exit, not SIGSEGV) and also reproduces
solo — likely a separate dynamic-map (`Map`-backed entity, no POJO) issue; not yet triaged.
