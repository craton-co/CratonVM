# HIB-CV-32 — SIGSEGV "binding a byte[] BLOB parameter" is actually a corrupt `Value` in `getfield java.lang.Byte.value`

> **✅ FIXED + VERIFIED on dev (merge `c9258e17`, branch `fix/gc-young-sweep-corruptor`).** Confirmed = the **same GC corruptor as HIB-CV-22/33** (victim here: a boxed `java.lang.Byte` whose reclaimed-then-reused storage made `getfield Byte.value` read a malformed `Value` `{heap-ptr, 6}`). The "distinct from HIB-CV-32" note in HIB-CV-33 is superseded — one fix resolves all three. Two parts landed: (1) **root** — `gen_heap.rs` `promotion_oom_risk` no longer diverts `--nojit` young collections into the corrupting non-moving sweep when no conservative JIT roots exist (precise moving collector runs instead; opt-out `CRATONVM_PROMOTION_OOM_GUARD_BROAD=1`); (2) **defense-in-depth** — `types/src/value.rs::read_value_checked` validates the discriminant before constructing the enum (the guard this report recommended), so a corrupt cell degrades to null+diagnostic instead of a wild SIGSEGV.
>
> **Independent verification (this report, 2026-06-24, dev `95a3a3ba` incl. the fix):** rebuilt release + re-ran the exact repro → **no SIGSEGV / 0 access violations, clean `System.exit(0)`**; the corruptor path is gone (the run no longer reaches `binding parameter (3:BLOB)`). `ByteArrayMappingTests` now fails **earlier and unrelatedly** with `ExceptionInInitializerError` in `org/hibernate/type/descriptor/JdbcTypeNameMapper.<clinit>` ← NPE `ModuleDescriptor.isOpen()` "this.descriptor is null" — a **separate JPMS module-descriptor bug** (being worked separately), not this crash. The analysis below stands as the root-cause record.

**Status:** **FIXED** (was: deterministic SIGSEGV in the generational collector). Residual: e2e blocked on a separate JPMS `ModuleDescriptor` NPE before the BLOB path.
**Severity:** High — deterministic SIGSEGV, killed the JVM, reproduced under `--nojit`.
**First filed:** run-20260622. **Confirmed failing:** 2026-06-23 on dev (`f8cdd52b`…`fbcf8f61`). **Fixed+verified:** 2026-06-24 on dev (`c9258e17`/`95a3a3ba`).
**Collector:** generational (default; no `-XX:+UseG1GC`).

---

## TL;DR

The crash is **mis-attributed** by the title. The byte[]→BLOB *bind* path is **not** the bug, and there is **no native blob-write out-of-bounds read / stale array pointer**. The SIGSEGV fires while Hibernate **logs** the just-bound parameter (`TRACE … binding parameter (3:BLOB) <- [[97, 98, 99]]`): it iterates the `byte[]`, boxes each element to `java.lang.Byte`, and calls `Byte.toString()`. The `getfield java/lang/Byte.value` (a **primitive `byte` field**) reads a **malformed `Value`** out of the heap — its discriminant slot contains a heap pointer instead of a small enum tag — and the interpreter's operand-stack push then indexes a `match` jump table with that pointer, faulting far outside the module.

It is **deterministic, pure-interpreter** (reproduces with `--nojit` *and* `CRATONVM_BG_COMPILE=0`), so it is **not** a JIT or background-compile bug. GC forwarding was observed active at the crash, and the corrupt value's pointer varies per run while the rest is constant — pointing at a **GC reference-integrity defect around boxed `java.lang.Byte` objects**, not at JDBC/BLOB code.

---

## Reproduction

Build release `cratonvm` from dev, then from `C:\craton\CratonVM\apps\hibernate-orm\.cratonvm-suite`:

```
listfile = one line: org.hibernate.orm.test.mapping.basic.ByteArrayMappingTests

CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cratonvm> \
  --java-home "C:/Program Files/Java/jdk-25" --nojit @common.args \
  CratonRunner <listfile> 0
```

Crash fires immediately after:
```
TRACE [org.hibernate.orm.jdbc.bind] binding parameter (3:BLOB) <- [[97, 98, 99]]
#  EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005) at pc=…  read at <wild addr>
```
`EXIT=139`. Reproduces identically with `CRATONVM_BG_COMPILE=0` added (rules out the background JIT worker).

> Operational note: kill stray `cratonvm.exe` between runs (`taskkill /F /IM cratonvm.exe` or `Get-Process cratonvm | Stop-Process -Force`). Crashed/watchdog-disabled runs leave processes that hold the binary and the H2 port. A *concurrent* process on this machine was also running the same suite from `target/release` during the investigation.

---

## Crash site (definitive — cdb `.pdata` unwind + PDB symbols on a `profsym` build)

```
ValueStack::push                          value_stack.rs:328
  └ CompactValue::from_value (inlined)    compact_value.rs:825
execute_instruction  (Getfield handler)   interpreter.rs:~11231  (push(value)?)
execute_frame / execute
vm_exec::invoke_on_class_shared_inner
… (reflective Method.invoke, see Java stack below) …
```

Faulting instruction (in `ValueStack::push`):
```asm
lea    r10, [jump_table]            ; CompactValue::from_value match table
movsxd r9,  dword ptr [r10+r9*4]    ; <-- AV: r9 = 0x02e91188 (corrupt Value discriminant)
add    r9,  r10
jmp    r9
```
`r9` is the `Value` discriminant, loaded as the low 32 bits of the value's first word. Valid discriminants are `0..=6` (`Int,Long,Float,Double,Object,ReturnAddress,Uninitialized`); here it is a heap-pointer low-32 (`0x02e91188`), so `[r10 + r9*4]` reads ~hundreds of MB past the module → SIGSEGV. The read address varies per run (it tracks the heap pointer); the pointer's high half is constant `0x00000001`.

`ValueStack::push` / `CompactValue::from_value` trust the discriminant and index the table **without bounds-checking it**, so any upstream-corrupt `Value` becomes a wild jump rather than a controlled error.

---

## The corrupt value (cdb `dq` at the AV; `r8 = &value`)

```
value (16 bytes):  [ 0x00000001_02e91188 ][ 0x00000000_00000006 ]
                     ^disc word (= heap ptr)  ^payload = 6
```
Layout of `Value` in this build: **discriminant `u32` @ byte 0**, payload @ byte 8 (16-byte enum; bytes 4–7 are padding). So the value decodes as `{disc = 0x02e91188, payload = 6}` — not any valid variant. Across runs the disc word is always a fresh heap pointer (`~0x1_02exxxxx`) and the payload is always `6`.

---

## What it is (opcode + field + Java context)

Diagnostic added at the Getfield push site (gated `CRATONVM_DIAG_HIB32`, flags `value` whose `disc as u32 > 6`) fired **exactly once**, right after the BLOB-bind trace:

```
[HIB32] CORRUPT getfield value raw=[0x0000000102e91188,0x0000000000000006]
        field="value" is_ref=false cp#100 in java/lang/Byte.toString pc=4
```

- Opcode: **`getfield java/lang/Byte.value`** — a **primitive `byte`** field (`is_ref=false`).
- `get_field` → `gen_heap::get_field` → `read_slot(base)` = **`std::ptr::read(base as *const Value)`** (a *raw 16-byte copy* of the heap field cell). Therefore the **heap cell physically contains** `{0x1_02e91188, 6}` — this is real heap content, not a decode artifact.

Java stack at the crash (innermost first):
```
java/lang/Byte.toString()Ljava/lang/String;                                         pc=4
org/hibernate/type/descriptor/java/AbstractClassJavaType.extractLoggableRepresentation(Object) pc=13
org/hibernate/type/descriptor/jdbc/BasicBinder.bind(PreparedStatement,Object,int,WrapperOptions) pc=66
org/hibernate/action/queue/spi/bind/JdbcValueBindings.beforeStatement(...)          pc=91
org/hibernate/engine/jdbc/batch/internal/SingleStatementBatchImpl.addToBatch(...)   pc=130
… BatchingPlanStepExecutor / FlushCoordinator / AbstractFlushingEventListener …
org/hibernate/internal/SessionImpl.fireFlush() → beforeTransactionCompletion()
org/hibernate/engine/transaction/internal/TransactionImpl.commit()                  pc=34
org/hibernate/testing/orm/transaction/TransactionUtil.wrapInTransaction(...)
org/hibernate/orm/test/mapping/basic/ByteArrayMappingTests.verifyMappings(...)      pc=503
org/junit/platform/commons/util/ReflectionUtils.invokeMethod(Method,Object,Object[]) pc=45
… JUnit reflective invocation …
```

So the bind itself succeeded; the crash is in Hibernate's **`extractLoggableRepresentation`** building the `[[97, 98, 99]]` log string, calling `Byte.toString()` on a boxed `byte[]` element. The `byte[]` is `{97,98,99}` ("abc"); the boxed elements are `Byte.valueOf(...)` cache instances.

---

## Why this is a heap / GC reference-integrity bug (not JDBC, not JIT)

1. **Pure interpreter.** `--nojit` sets `CRATONVM_DISABLE_JIT`; the crash also reproduces with `CRATONVM_BG_COMPILE=0`. No JIT frame is in the real `.pdata` unwind. Deterministic.
2. **The heap cell is genuinely corrupt.** `read_slot` is a raw `ptr::read::<Value>`; the 16 bytes `{heap-ptr, 6}` are what the `Byte.value` cell holds. A real `java.lang.Byte.value` cell should hold `Value::Int(byteValue)` = `{disc=0, payload=97|98|99}`.
3. **No `Value`-aware writer produces `{ptr@0, 6@8}`.** The interpreter's `putfield` and native `set_field` both write a `Value` (small discriminant at offset 0) via `write_slot` = `ptr::write::<Value>`. The corrupt shape has a *pointer* at offset 0 — i.e. it was written by something that does **not** treat the cell as a `Value`, or the cell belongs to a **different object** than the receiver claims.
4. **GC forwarding was active.** An earlier cdb dump showed the Getfield read-barrier `VmHeap::load_and_forward` returning a *forwarded* address (`load_and_forward(inner) != inner`), i.e. a relocation cycle had run. The corrupt disc pointer changes every run (heap addresses); the payload is invariantly `6`.

Two candidate mechanisms (both GC reference-integrity defects around the cached boxed `Byte` objects):

- **(A) Stale / reused receiver.** The `this` passed to `Byte.toString()` is a stale reference (a root not updated across a relocation) whose backing memory was reused by another object/array, so the "value field" read returns foreign 16 bytes `{ref, 6}`. (`Byte.toString` receiver was optimized out at the crash — `dv obj_ref` = `<value unavailable>` — so this was not directly confirmed; it is the leading hypothesis.)
- **(B) In-place primitive-cell corruption.** The real cached `Byte`'s primitive `value` cell was overwritten during a GC relocation (e.g. mis-scanned as a reference, or clobbered by an evacuation), leaving pointer-shaped bytes.

The cell content's first word (`0x1_02e91188`) is itself a readable heap address with object-like data (`dq` → `0x12, 0x15a1a, 0x1, …`), consistent with the cell now holding a *reference to another object* rather than a `byte`.

This is **not** the same family as the HIB-CV-20/21 OSR back-edge JIT hangs (those are JIT/OSR; this is interpreter-only).

---

## Relationship to HIB-CV-22 / HIB-CV-33 (the GC-corruptor family)

This is almost certainly the **same GC corruptor** as:
- **HIB-CV-33** (`docs/known-issues/HIB-CV-33-…joined-inheritance-sf-build.md`) — root-caused to the generational **non-moving young-gen sweep** (`gen_heap::sweep_young_non_moving`) reclaiming/relocating a *still-live young object* under the `promotion_oom_risk` branch (both gens transiently ≥90% full) with no conservative roots to protect, leaving a dangling pointer.
- **HIB-CV-22** ("InvocationInterceptors called invocation multiple times") — explicitly "the **same GC corruptor as HIB-CV-33, with a different victim object**" (a freshly-allocated `AtomicBoolean`).

HIB-CV-32 fits as **a third victim**: a live boxed `java.lang.Byte` (a `Byte.valueOf` cache instance) whose memory is reclaimed/reused, so its primitive `value` cell reads back as foreign data `{heap-ptr, 6}`. Shared signals: same suite, same JUnit reflective-invoke context (`InvocationInterceptorChain$ValidatingInvocation.proceed` appears in both the HIB-CV-22 and HIB-CV-32 stacks), GC forwarding active, `--nojit`, heap-pointer-shaped corruption of a live object.

> **Two notes to reconcile for the maintainer:**
> 1. HIB-CV-33 currently states "**Distinct from HIB-CV-32 (verified)**" — that note predates this deep analysis (it was written against the original "blob-write OOB" triage). The evidence here (GC corruption of a live boxed `Byte`) argues HIB-CV-32 is the *same* family, not distinct.
> 2. **Determinism caveat:** HIB-CV-33/22 are load-sensitive heisenbugs; HIB-CV-32 reproduces **deterministically** at the same `getfield` every run. Either `ByteArrayMappingTests` deterministically drives the heap into the `promotion_oom_risk` state at the same allocation point (plausible — small fixed workload), or HIB-CV-32 is a deterministic sibling defect in the same `gen_heap` sweep/promotion path. The receiver-identity dump (below) plus a `gen_heap` sweep trace at the crash will settle it.

If the non-moving-young-sweep fix for HIB-CV-33/22 lands, **re-test `ByteArrayMappingTests`** before treating HIB-CV-32 as independent — it may resolve for free.

---

## Fixes

**True fix (upstream):** restore reference integrity for boxed `java.lang.Byte` (and by extension the other cached boxes — `Short`/`Integer`/`Character`/`Long`) across GC relocation. Next diagnostic step to disambiguate (A) vs (B):

- Re-run with the receiver dump (a built variant of the gated `CRATONVM_DIAG_HIB32` block, extended to print `obj_ref` ptr, `class_id_of(obj_ref)`, resolved class name, `kind_of`, `is_object_address(ptr).is_some()`, and the raw 32-byte header). If `class != java/lang/Byte` or `live == false` → stale/reused receiver (A); if it *is* a live `java.lang.Byte` → in-place cell corruption (B).
  - This build was attempted but repeatedly failed to **link** during the session because a concurrent agent was committing to `dev` and running `cratonvm` from `target/release` (churning sources mid-build, contending the cargo target lock). Retry on a quiescent tree. The diagnostic edit is in `vm/src/runtime/interpreter.rs` at the Getfield push site, gated by `crate::runtime::env_cache::diag_hib32()` (`CRATONVM_DIAG_HIB32`).
- Then bisect on the cached-box lifecycle: check whether `Byte.ByteCache` instances are correctly enrolled as GC roots / updated, and whether `gen_heap` evacuation ever treats a 16-byte primitive `Value::Int` cell as a reference.

**Defensive guard (cheap, high-value, does not fix root cause):** make `CompactValue::from_value` / `ValueStack::push` reject an out-of-range `Value` discriminant instead of indexing the `match` jump table blind. That converts this (and any future upstream `Value` corruption) from a wild SIGSEGV into a diagnosable, catchable internal error with the offending class/method/pc — turning a fatal crash into a localizable bug report.

---

## Investigation artifacts / method

- Crash is stripped in `[profile.release]` (`strip="debuginfo"`); built `--profile profsym` (full debug, same `opt-level=3`/LTO) for symbols.
- Accurate stack via **cdb** `sxe av; g; kb` (`.pdata` unwind) on the `profsym` binary; the in-VM crash dump's frame walk above the faulting PC is a heuristic stack-scan and is unreliable.
- Source lines via `apps/tomcat/.tooling/pdbresolve` (extended to emit `file:line`) against `target/profsym/cratonvm.pdb` (RVA = VA − module base; module base from cdb `lm m cratonvm`).
- Instruction-level proof via `llvm-objdump -d` on `target/release/cratonvm.exe` (image base `0x140000000`); the panic-landing `core::panic::Location` structs in `.rdata` localized the crashing function to `vm/src/runtime/value_stack.rs` before symbols were available.
- `CRATONVM_SYMBOLIZE=<exe+RVAs>` is the in-VM offline symbolizer mode (resolves to nearest *export* only — too coarse; use pdbresolve/cdb instead).

## Key source references
- [vm/src/runtime/value_stack.rs:314](../../../../../vm/src/runtime/value_stack.rs) — `ValueStack::push`
- [types/src/compact_value.rs:825](../../../../../types/src/compact_value.rs) — `CompactValue::from_value` (the jump table)
- [vm/src/runtime/interpreter.rs:~11196](../../../../../vm/src/runtime/interpreter.rs) — Getfield handler (reference + primitive coercion, push)
- [gc/src/gen_heap.rs:1382](../../../../../gc/src/gen_heap.rs) `get_field` → [gc/src/gen_heap.rs:6530](../../../../../gc/src/gen_heap.rs) `read_slot` (raw `ptr::read::<Value>`)
- [gc/src/vm_heap.rs:462](../../../../../gc/src/vm_heap.rs) — `load_and_forward` (Brooks read barrier; observed returning a forwarded addr)
- [types/src/value.rs:27](../../../../../types/src/value.rs) — `Value` enum (Int=0,Long=1,Float=2,Double=3,Object=4,ReturnAddress=5,Uninitialized=6)
