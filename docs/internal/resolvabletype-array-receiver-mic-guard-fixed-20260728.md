# `ResolvableType[]` array-cast `ClassCastException` — FIXED: a virtual call site cached by class id alone accepted an ARRAY receiver (2026-07-28)

Status: ✅ **RESOLVED**. Root cause found, fixed at four codegen/dispatch
sites, and the `SPRING-RT-EQUALS.1` JIT ban that had been masking it since
2026-07-27 has been REMOVED. Retires
`docs/known-issues/resolvabletype-equals-jit-narrowed-20260727.md` (and the
earlier `resolvabletype-array-cast-aggressive-jit-20260727.md` it superseded).

## The bug in one paragraph

`ObjectHeader.class_id` lives at byte offset 0 for objects **and** for arrays,
and a reference array stores its **component** class id in that word. Every
inline-cache guard in the VM — the single-pass MIC probe and 4-way PIC cascade
(`jit/src/x64.rs`), the IR lowerer's cascade (`jit/src/ir_lower.rs`), the shared
hashed megamorphic stub (`jit/src/runtime_lowering.rs`), and the Rust-side
`jit_invoke_virtual_mic` MIC/megamorphic fast paths (`vm/src/jit/helpers.rs`) —
selected a cached compiled callee by comparing **only those four bytes**. So a
`Foo[]` receiver and a `Foo` receiver were indistinguishable: once a virtual
call site had been warmed on a `Foo` receiver, a later `Foo[]` receiver passed
the guard and was called straight into `Foo`'s own method body with an array as
`this`. Per JVMS §4.4.1 an array class inherits `java/lang/Object`'s method
table, not its component's.

`ObjectHeader.kind` (offset 4, `ObjectKind::Object` = 0, `Array` = 1) is what
separates them. All five sites now check it.

## Minimal reproducer (no Spring, 40 lines)

`docs/internal/repros/resolvabletype-array-receiver-mic-20260728/ArrayReceiverMicProbe.java`

```java
static boolean cmp(Object a, Object b) { return a.equals(b); }   // one call site
...
for (int i = 0; i < 50_000; i++) { cmp(f1, f2); cmp(f1, f3); }   // warm on Foo
Foo[] a1 = { f1 }, a2 = { f1 };
for (int i = 0; i < 5_000; i++) { cmp(a1, a2); }                 // now hand it Foo[]
```

Arrays do not override `equals`, so `a1.equals(a2)` must be identity-false.
Before the fix, on real-JDK mode at the DEFAULT JIT threshold:

```
FIRST_THROW=java.lang.ClassCastException: class [LArrayReceiverMicProbe$Foo;
            cannot be cast to class ArrayReceiverMicProbe$Foo
MISMATCH_COUNT=5000        # HotSpot: 0
```

The `checkcast Foo` that throws is the one inside `Foo.equals` itself — which
is why the message names an array being cast to its own component type and
reads like a duplicate-class / classloader-identity split. It is not one.

## How that became the Spring Boot symptom

`ResolvableType.equals(Object)` bci 119/128 calls
`VariableResolver.getSource()` and feeds both results to
`ObjectUtils.nullSafeEquals(Object, Object)`.
`ResolvableType$TypeVariablesVariableResolver.getSource()` returns its
`ResolvableType[] generics` field — a **reference array**. So `nullSafeEquals`
does `o1.equals(o2)` on two `ResolvableType[]` receivers at a call site that
Spring's `ConcurrentReferenceHashMap` lookup chain has already warmed to
`ResolvableType.equals`. The array receiver passed the class-id guard, entered
`ResolvableType.equals`, and its bci-25 `checkcast ResolvableType` threw

```
ClassCastException: class [Lorg.springframework.core.ResolvableType;
  cannot be cast to class org.springframework.core.ResolvableType
```

which propagated out of `ResolvableType.forType` and killed
`org/springframework/boot/context/config/Profiles.<clinit>`.

That also explains every earlier bisection result without any of them being the
real culprit: denying `ResolvableType.equals`, `ObjectUtils.nullSafeEquals`,
`ConcurrentReferenceHashMap$Segment.findInChain`, or the whole
`ConcurrentReferenceHashMap` each removes one *link* of the compiled chain, and
`CRATONVM_JIT_BISECT_ONLY=<core>` / `<util>` alone never gets both the warming
call site and the array-passing caller compiled together.

## Two corrections to the retired doc

1. **It is NOT `CRATONVM_JIT_THRESHOLD=1`-only.** The retired doc's
   "Reachability caveat" says a real workload would never hit this. The
   standalone `RtEqualsProbe` (below) reproduces 2881/3000 at the **default**
   threshold on `c7c0ac86e`; `S01_Context` only looked threshold-only because
   its `<clinit>` runs once. This was a default-path correctness bug.
2. **The disassembly analysis was a dead end.** The retired doc's central
   hypothesis — that the 4-way PIC codegen template in `jit/src/x64.rs`
   mis-selects a slot, or that PIC slots leak across call sites — is wrong.
   MIC/PIC slots are allocated per (bytecode-pc, compiled-method) and the
   helper never *installs* an entry for an array receiver (`cacheable_receiver`
   is false for `ObjectKind::Array`). Only the **consumption guard** was ever
   wrong. The retired `equals_disasm.txt` has been dropped.

## The fix

Five guards, all "reject a receiver whose `ObjectHeader.kind` is not
`ObjectKind::Object` and take the resolving slow path, which dispatches an
array on `java/lang/Object`":

| Site | What it guards |
|---|---|
| `jit/src/x64.rs` (MIC arm) | `CMP BYTE [RAX+4], 0` before `MOV EAX,[RAX]` |
| `jit/src/x64.rs` (4-way PIC arm) | same, on `ARG_REGS[1]` (REX-aware) |
| `jit/src/ir_lower.rs` (IR cascade) | same, before the single shared class-id load |
| `jit/src/runtime_lowering.rs` (hashed megamorphic stub) | same, before `MOV EDX,[RAX]` |
| `vm/src/jit/helpers.rs` (`jit_invoke_virtual_mic`) | `receiver_is_plain_object` on the MIC-hit and megamorphic-lookup fast paths |

The machine-code guards and the Rust guard are BOTH load-bearing: with only the
machine guards the array falls through to `jit_invoke_virtual_mic`, whose
`cached_cid == receiver_cid` fast path made the identical mistake (verified —
`cratonvm-rtq-fix2` still failed 5000/5000).

Regression tests: `ic_cascade_rejects_array_receivers_before_the_class_id_guard`
(`jit/src/ir_lower.rs`) and `hashed_vtable_stub_rejects_array_receivers`
(`jit/src/runtime_lowering.rs`) assert the guard is emitted *and* precedes the
class-id load it protects.

## Verification

Witness class named by the ban comment,
`org.springframework.boot.autoconfigure.condition.ConditionalOnPropertyTests`
(38 tests), real-JDK mode, default JIT:

| binary | result |
|---|---|
| dev + `SPRING-RT-EQUALS.1` ban (baseline) | 38/38 pass |
| dev, ban REMOVED, no fix | **0/38 pass — 38 failed** |
| dev, ban REMOVED, fix applied | 38/38 pass |
| HotSpot control | 38/38 pass |

Probes (all `MISMATCH_COUNT=0` after the fix, all in the repro dir):

* `ArrayReceiverMicProbe` — 5000 → 0 thrown.
* `RtEqualsProbe` — 2884 → 0 `ClassCastException`.
* `CrhmProbe` — `ConcurrentReferenceHashMap<Object,Object>` keyed by
  `ResolvableType`; aborted before the fix, clean after.
* `RtEqualsUnit` — `equals`/`nullSafeEquals` in isolation; clean before AND
  after (they were never the defective bodies — useful negative control).

Cross-module Spring suite `S01..S10` (`/data/data/spring-boot-tomcat-crossmodule-20260717`),
both at `CRATONVM_JIT_THRESHOLD=1` and at the default threshold: 95/95 pass,
0 fail, no crashes. `S01_Context` specifically ran 10/10 clean.

`cargo test -p cratonvm-vm --lib`: 2435 passed, 3 failed — the same 3
(`classify_complex_ctor_with_putfield`, `complex_ctor_keeps_constructor_ban`,
`generated_proxy_class_is_jit_eligible_after_proxy_jitcall_1_removal`) fail
identically on pristine `d8e407f84`, and `cargo test -p cratonvm-jit --lib`
aborts at `live_monitor_ops_execute_direct_runtime_stubs` (SIGSEGV) on pristine
dev too. Both are pre-existing and unrelated; targeted `ir_lower` /
`runtime_lowering` / `x64::flag_and_header` filters are green.

> **Resolved 2026-07-30.** Those three were stale *tests*, not defects: each
> asserted behaviour a later deliberate change had replaced —
> `allow_putfield_init`'s 07-28 default flip for the two constructor tests,
> SPR-PROXY.1's 07-28 re-ban for the proxy one. Retargeted; see
> [`dev-stale-jit-tests-fixed-20260730.md`](dev-stale-jit-tests-fixed-20260730.md).

## Second, independent bug fixed in the same session

Reproducing `S01_Context` under `CRATONVM_JIT_THRESHOLD=1` on current dev no
longer produced the `ClassCastException` at all — it **SIGSEGV'd** first, in
`conservative_roots::published_shadow_values`, before ever reaching the Spring
code. That is a different defect and is fixed here too:

`moving_young_unpublished_frame_oop_present` walked the JIT entry chain using
`info.exact_rbp` together with `info.compiled_method` **without** the
`chain_entry_rbp_is_foreign` guard that its sibling
`refresh_moving_young_coverage_for_current_thread` (and
`scan_one_frame_precise`, and `remap_active_jit_frames`) all apply. When the
innermost RBP belongs to a compiled callee reached through the inline MIC/PIC
cascade — which `CRATONVM_MOVING_YOUNG_COVERAGE_DBG=1` shows happening on
**every** collection in this workload (573 `FOREIGN_INNERMOST_RBP` reports in
one run) — the boundary method's `shadow_thread_slot_off` reads an arbitrary
aligned stack word out of an unrelated frame, `shadow_window_from_frame`
dereferenced it as a `*mut JvmThread`, and the resulting `base` (observed:
`0x5555_0000_0004`) was walked as a 2 MiB shadow window.

Fixed by (a) applying the same foreign-RBP guard in the band verifier, and
(b) hardening `shadow_window_from_frame`: it now rejects a cached thread
pointer that is not this thread's installed `JvmThread`
(`helpers::current_jit_thread_ptr`, a new side-effect-free TLS read), and
rejects an unaligned or over-wide `[base, top)` instead of clamping the walk.
Regression tests: `shadow_window_rejects_an_unaligned_base` and
`shadow_window_rejects_a_window_wider_than_the_backing_buffer`.

## Diagnostic added

A silent `if compiler.buf.overflowed() { return None; }` in `x64.rs` bailed
whole methods out of JIT compilation with no record of which method or by how
much — the only visible symptom was a flood of anonymous
`try_patch_*: offset out of bounds` warnings. `ExecutableBuffer` now tracks
`wanted()` (bytes codegen asked to emit, including writes dropped after
overflow) and the bail logs `method`, `code_len`, `capacity`, `wanted`. On the
`S01_Context` run that surfaces 7 methods whose buffers are under-provisioned,
e.g. `ConcurrentReferenceHashMap$Segment.restructure` at capacity 32256 /
wanted 36327. Those are a separate (throughput, not correctness) matter and are
NOT addressed here.

## Repro commands

```bash
CP=/data/tmp/rtq/probe
cratonvm --java-home <jdk25> -cp "$CP" ArrayReceiverMicProbe     # MISMATCH_COUNT=0

ACP=$(cat .../core/spring-boot-autoconfigure/build/cratonvm-test-cp.txt)
cratonvm --java-home <jdk25> -cp "$CP:$ACP" Junit5One \
    org.springframework.boot.autoconfigure.condition.ConditionalOnPropertyTests
```
