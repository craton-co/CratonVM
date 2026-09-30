# Proposal: the special door answers a trivial default constructor without a frame

**Status: open, narrowed — filed 2026-09-27 by interpreter round i1 wave 24,
lane L4 (proposal). Wave 25 landed stages 1-2 (bench half), which wave 28
(lane L7) found had never engaged on the real build and fixed; wave 28 also
landed field-store constructors. Remaining: the JVMTI probe row, the memo
on `CachedBytecodeMethod`, the super-chain rule (below).**

## Progress (wave 28) — lane L7

**The wave-25 elision never engaged.** `body_is_trivial_ctor_shape`
required `cached.code.len() == 5`, the JIT twin's number
(`jit_bridge::is_elidable_construction` reads the class file's
`init.code()`), but every invoke-cache entry carries its body through
`frame::padded_bytecode_for_method` (`populate_invoke_cache` in
`dispatch_static.rs`, `populate_virtual_invoke_cache` in
`dispatch_virtual.rs`): two speculative-read zero bytes longer, so javac's
default constructor is 7 bytes there. `trivial_object_ctor_uncached`
compared the same padded bytes with the class file's. The unit tests built
UNPADDED entries (their helper's doc said "in the padded shape the caches
hold"), so they passed; the census line `trivial constructors: elided=N`
could never print. The wave-25 row of the round's measurement table
(`new-default` 264 → 226) is therefore the door-slot change alone. (The
virtual door's trivial-getter test is written against the padded length,
`code.len() == 7` for a five-byte getter, which is how the two disagree.)

Changes (`vm/src/runtime/interpreter/invoke_fast.rs` unless noted):

* **Shape and verdict on the real length** (`jit_bridge::cached_method_code_len`).
  New test `the_trivial_shape_is_read_on_the_padded_body_the_caches_hold`
  builds the entry through `padded_bytecode_for_method`; `ctor_entry` pads.
* **The memo is per method**: `TRIVIAL_CTOR_MEMO` is keyed by the padded
  body `Arc` (one per method) plus the declaring class, not by the
  inline-cache entry (one per call site, so a loop with more constructor
  sites than slots re-ran the `class_manager` verdict per call once the
  elision was live), and is two-way set-associative, 16 sets.
* **Stack depth**: the framed call pushes two frames (the constructor, then
  `Object.<init>`, which the door declines at a full stack so the general
  path throws `StackOverflowError`), so the frameless answer also needs
  `frames.len() + 1 < max_stack_depth`; the depth at which a constructor
  overflows is unchanged.
* **Field-store constructors** (`FIELD_CTOR_ELISION`, a const kill switch):
  a direct `Object` subclass's `<init>` whose body is the `Object.<init>`
  call, 1..4 stores `aload_0; <value>; putfield` (a parameter via
  `iload`/`fload`/`aload` of local >= 1, or `iconst_*`, `bipush`, `sipush`,
  `fconst_*`, `aconst_null`), then `return`, is answered in
  `execute_nonvirtual_fast_door` after the prelude: `field_ctor_prefilter`
  (four byte compares) → `field_ctor_frameless` (the memoized verdict, now
  for any descriptor, then `field_ctor_plan`, a pure byte parse) →
  `field_ctor_stores_answered` (stack room for two frames, the VM-wide
  `jvmti_requires_interpreter`, category-1 parameters only) →
  `field_fast::ctor_field_stores_fast`: per store the thread's
  `fast_field_sites` entry of `(declaring class, cp index)` -- present only
  once the framed body's slow `putfield` has resolved and filled it -- the
  `<init>` final-field admission, `field_ptr_for` on the receiver, the value
  validated; all stores validated before any is made. The stores are
  `putfield_fast`'s own code: its arms became `field_fast::put_field_value::<STORE>`
  (`STORE = false` validates only), so there is one copy of the barriers.
  Not counted for tier-up, like the empty body and the trivial constructor.
* **Census**: `CRATONVM_DBG_FIELD_SITE=1` prints `[invoke-door] field-store
  constructors: elided=N framed=M` beside the trivial line
  (`FRAMELESS_CENSUS` grew to six entries; no new static).
* **Probe** `tools/probes/interp/L7/L7W28FieldCtorProbe.java` (HotSpot 25's
  output in its header; every field width, constants, finals, subclass
  receivers, a `long` parameter, reference fields across `System.gc()`,
  constructors at the stack limit). **Bench**
  `tools/probes/interp/L7/L7W28FramelessCtorBench.java`: `new-default`,
  `new-field-init`, `new-point` expected well down, `new-sub-field` and
  `new-long-param` controls flat; also `InvokeDoorCostBench` `ctor` and this
  page's `L4W25FramelessDoorBench` rows (whose header still calls
  `new-field-init` a control).

**What remains:** stage 2's JVMTI half (a MethodEntry agent row: both
frames must come back); stage 3 (the verdict on `CachedBytecodeMethod`); the
recursive rule over a super chain of trivial or field-store constructors
(`new-sub-field`'s shape); `long` / `double` parameters and constants
(local numbering and the kind mark); with the JIT on, whether never counting
these constructors in the interpreter changes a compiled caller's inlining
(the JIT-on `ctor` row should be read).

## Problem

Since wave 20 the non-virtual door answers a body that returns at pc 0
without a frame (`invoke_fast::body_returns_at_entry`, `EMPTY_BODY_ELISION`),
and since wave 24 without touching the entry's refcount. That covers
`new Object()` and nothing else in a constructor chain: every class that
declares no constructor gets javac's default one,

```text
aload_0; invokespecial java/lang/Object.<init>()V; return      (2a b7 hi lo b1)
```

and each interpreted `new C()` of such a class still pushes and pops a full
frame (`push_frame_verbatim`: pool refill, locals copy, bookkeeping, the
`Arc` clone moved into the frame and dropped at return) for a body whose
only effect is a call the door already answers without one. The JIT has had
the equivalent for years: `jit_bridge::is_elidable_construction` recognises
exactly this body (and refuses a class with a finalizer, an uninitialized
class, and a registered native that would run instead), and the codegen
drops the call (`jit/src/lib.rs`, `elide_trivial_ctor_enabled`).

Evidence of the cost: `tools/probes/interp/L4/EmptyBodyBorrowBench.java`,
`--nojit`, row `new-default` (`new Plain()`, a default constructor) against
row `new-object` (`new Object()`, the already-frameless call): the
difference is one frame push and pop plus one door pass. HotSpot 25 `-Xint`
on the reference laptop: 64 vs 45 ns/call; the interpreter's own gap should
be read off the same bench.

## Design

In `nonvirtual_door_prelude`, beside the `body_returns_at_entry` test, a
second predicate `body_is_trivial_object_ctor(cached)`:

1. `cached.code` is exactly `2a b7 hi lo b1` and `cached.method_name ==
   "<init>"`, `method_descriptor == "()V"` (so `total_args == 1`);
2. constant-pool entry `hi lo` of the DECLARING class is a Methodref to
   `java/lang/Object.<init>()V` — the one fact the bytes cannot show. It
   needs the class's constant pool, i.e. a `class_manager` read, so it must
   be MEMOISED per method, never asked per call;
3. the nested `Object.<init>` is itself served as plain bytecode (no
   registered native that would win: the same resolver
   `jit_bridge::elidable_ctor_native_would_run` uses, asked of
   `java/lang/Object`), and `empty_body_elidable` holds for BOTH methods
   (JVMTI MethodEntry/Exit or a JDWP request on either keeps the frame).

When all hold, pop the receiver and answer `Handled`, exactly as the empty
body does. Nothing observable is lost: the body cannot throw, allocate or
reach a safepoint, and finalizable instances are registered at allocation.

Where the memo lives: a `OnceLock<bool>` on `CachedBytecodeMethod` is the
natural home but adding a field is blocked on the 64 struct literals in
`jit/`, `jit/tests/` and `jit-api/` (see
`interpreter-L3-proposal-cheap-interpreted-call-RETIRED-20261003.md`, stage 1). Until a
JIT-owned round converts them to `CachedBytecodeMethod::from_parts`, a
per-thread memo keyed by `(declaring_class_id, Arc::as_ptr(cached))` in
`invoke_fast.rs` (a 4-entry array, not a hash map; cleared on a
redefinition epoch move like the other door memos) is enough, since the
hot constructors of a loop are few.

## Expected win and how to measure

One frame push/pop and one door pass per interpreted `new` of a class with
a default constructor directly under `Object` — plain data holders, most
exception-free POJOs, generated classes. Measure with `EmptyBodyBorrowBench`
`new-default` (`--nojit`, fat-LTO build, interleaved A/B, medians of 5): it
should approach `new-object`. Census: count elided trivial constructors
beside `[invoke-door] empty-body callees: elided=N framed=M`
(`CRATONVM_DBG_FIELD_SITE=1`) so a zero is visible.

## Cost / risk

Low if the three conditions are exact. The risks are the ones
`is_elidable_construction` already names: a registered native constructor
shadowing the bytes (the `HashMap.<init>` story in its comment), and
redefinition (the memo must be dropped with the entry's gate generation, and
the door already declines a redefined target under the kill switch). A
subclass whose default constructor calls a NON-Object superclass
constructor is out of scope (its `invokespecial` names the superclass); a
recursive rule over the super chain is a later stage, as it is for the JIT.

## Staged plan

1. The predicate and its per-thread memo, behind a `const` kill switch
   (`TRIVIAL_CTOR_ELISION`), with a unit test on a hand-built entry (bytes
   and constant pool) and the census line.
2. `EmptyBodyBorrowBench` `new-default` A/B; `EmptyBodyElisionProbe`
   extended with a default-constructor row under a JVMTI MethodEntry agent
   (the frame must come back).
3. When `CachedBytecodeMethod` gains memo fields, move the memo onto it and
   delete the per-thread table.

## Progress (wave 25) — lane L4

Stages 1 and 2 (bench half) landed; stage 3 waits on the same
`CachedBytecodeMethod` prerequisite as before.

* `vm/src/runtime/interpreter/invoke_fast.rs`: `TRIVIAL_CTOR_ELISION`
  (`const` kill switch), `body_is_trivial_ctor_shape` (the JIT's five-byte
  test plus `<init>` / `()V`), `trivial_object_ctor` (the verdict, memoized
  per thread in `TRIVIAL_CTOR_MEMO`: 4 direct-mapped slots keyed by
  `vm_identity`, an `Arc` clone of the entry -- so `Arc::ptr_eq` means
  identity -- and `class_redefinition_count`, which expires every verdict on
  any redefinition), `trivial_object_ctor_uncached` (the entry's code is the
  class's CURRENT `<init>()V`; constant-pool `hi lo` is a `Methodref` to
  `java/lang/Object.<init>()V`; the superclass is `java/lang/Object`, whose
  `<init>()V` is unsynchronized and returns at pc 0; and
  `jit_bridge::elidable_ctor_native_would_run(shared, "java/lang/Object")`,
  the JIT's own resolver, made `pub(super)`, is false), and
  `trivial_ctor_elidable` (the VM-wide `jvmti_requires_interpreter`, since
  the call would have pushed two frames). `nonvirtual_door_prelude` answers
  an `invokespecial` of such a constructor like an empty body (pop the
  receiver, `Handled`, uncounted for tier-up). The prelude now takes
  `&Arc<CachedBytecodeMethod>` (both callers had one).
* Census: `CRATONVM_DBG_FIELD_SITE=1` prints `[invoke-door] trivial
  constructors: elided=N framed=M` beside the empty-body line (the two
  tallies share one `FRAMELESS_CENSUS` array, so the per-VM statics ratchet
  count is unchanged: two statics became one, the memo is the other).
* Tests (`invoke_fast.rs`): `only_the_default_constructor_bytes_have_the_trivial_shape`,
  `the_special_door_keeps_the_frame_of_a_constructor_it_cannot_prove_trivial`,
  `a_proven_trivial_constructor_is_called_without_a_frame` (defines
  `w25/Trivial`, `w25/Base`, `w25/Sub` from hand-built class files and reads
  the verdict off the real constant pool; skips, saying so, when the
  unit-test VM cannot define them or its `Object.<init>` is not plain).
* Bench: `tools/probes/interp/L4/L4W25FramelessDoorBench.java`, `--nojit`,
  row `new-default` (expected to drop towards `new-object`),
  `new-sub-default` (its nested `Plain.<init>` is elided: a smaller drop),
  `new-field-init` (control, flat).
* Not done: stage 2's JVMTI half (`EmptyBodyElisionProbe` with a
  MethodEntry agent row for a default constructor); the recursive rule over
  a super chain of trivial constructors (a subclass's default constructor
  calling a trivial one is itself trivial) -- a later stage, as for the JIT.
