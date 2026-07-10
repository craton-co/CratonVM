# ES FAIL family - vector codec exceptions with corrupted Throwable cause output

Status: OPEN (narrowed — 2 of 3 rows now PASS; 1 residual root-caused, not yet fixed)

## Update 2026-07-10 (follow-up session)

Reproducing this family first required fixing an unrelated, more severe
regression: `RandomizedContext.current()` started returning `null` instead
of throwing `IllegalStateException` (introduced by dev commit `4978c5d5c`
"Speed up Elasticsearch sliced IVF native paths", which rewrote
`RandomizedContext` as a native for performance). That broke
`AssertingCodec.<init>`'s `catch (IllegalStateException e) { targetClass =
null; }` fallback for code running outside a randomized-test thread (e.g. a
test class's own `<clinit>`), turning a tolerated case into an uncaught NPE
-> `ExceptionInInitializerError` that prevented every class in this family
(and likely much of the broader ES suite) from even loading. See
`docs/internal/fixed-suite-bugs/randomizedcontext-current-null-instead-of-throw-FIXED.md`
for that fix (merged separately; required before this doc's rows could be
re-tested at all).

With that blocker fixed and reverified against the same seed
(`B17AC9D3E1F2A0C4`):
- `ES815BitFlatVectorFormatTests` — **PASS**, 5/5 repeated runs, 6/6 tests each (was the AIOOBE row).
- `ES93HnswBFloat16VectorsFormatTests` — **PASS**, 17/17 tests (was the AIOOBE row).
- `ES93FlatBFloat16VectorFormatTests` — **STILL FAILS**, deterministically, same seed, both JIT-on and `--nojit`: `testMultiClose` throws `BufferUnderflowException` with the same `Caused by: java.lang.Object` corruption.

It's unclear whether the first two rows were fixed by a side effect of the
IVF-speedup commit's native vector math rewrite, or whether they were always
seed/timing-dependent and simply didn't trigger this time — the
RandomizedContext fix is what made them *testable* again, not necessarily
what fixed them. Re-verify with a spread of seeds before fully retiring
those two rows from this family.

### Root cause of the `Caused by: java.lang.Object` corruption (testMultiClose)

Added temporary env-gated instrumentation (`CRATONVM_DBG_CAUSE=1`, left in
tree in `native-builtins/src/lang_misc.rs`'s `write_throwable_cause`/
`throwable_cause`, following the codebase's existing `CRATONVM_DBG_*`
diagnostic pattern) that logs every write and read of a `Throwable.cause`
field with the object's class and identity hash. Reproducing
`ES93FlatBFloat16VectorFormatTests.testMultiClose` with it enabled shows:

```text
CAUSE_DBG_WRITE this=java/nio/BufferUnderflowException hash=493728 cause=SELF
CAUSE_DBG_WRITE this=java/lang/reflect/InvocationTargetException hash=493747 cause=java/nio/BufferUnderflowException hash=493728
...
CAUSE_DBG_READ  this=java/nio/BufferUnderflowException hash=493728 cause=java/lang/Object cause_hash=493742
```

The `BufferUnderflowException` (identity hash `493728`) is constructed
correctly — `cause` is written as the self-referential JDK sentinel
(`cause == this`, meaning "no cause set"). No code anywhere in the run ever
calls `write_throwable_cause` with hash `493742` as a value for *any*
object's cause field — that identity never appears on a WRITE line at all.
Yet the final read of the *same* object's `cause` field (during
`printStackTrace` -> `throwable_cause`, driven by JUnit's
`Throwables.getFullStackTrace`) returns a live, valid `java.lang.Object`
instance with a *different* identity hash (`493742`), not the self-sentinel
it was constructed with.

This is not a native-logic bug (the write path is correct) and not
"corrupted/garbage bytes" either — `493742` is a real, addressable object
that decodes as a valid `Value::Object`, which is why `throwable_cause`'s
read succeeds and returns `Some(...)` instead of tripping the
`read_value_checked_atomic` corrupt-cell guard for *this* slot. (A companion
`gen_heap::read_slot: corrupt Value cell (out-of-range discriminant)`
HIB-CV-32 diagnostic fires on a *different*, nearby slot in the same run,
suggesting broader heap disturbance around the same GC cycle rather than an
isolated one-field bug.)

**Self-forwarding-GC theory — tried, REFUTED.** The initial hypothesis (a
self-referential pointer surviving a GC move stale, aliasing whatever gets
reused at the object's old address once it relocates) was directly tested
and disproven:

- A new Rust unit test, `self_referential_field_survives_promotion` in
  `gc/src/gen_heap.rs` (kept in tree), allocates an object with a
  self-referential field, forces it through `PROMOTION_AGE` young->old
  promotions plus 5 more post-promotion GC cycles, and asserts the field
  still points at the (relocated) object each time. **Passes cleanly** —
  ordinary promotion correctly updates self-references.
- Two concurrent Java repros (`ConcurrentSelfRefRepro`/`ConcurrentSelfRefRepro2`,
  3 allocator/churner threads + a polling/throw-catch thread hammering a
  small heap for 30s each, one holding plain `Exception` self-references,
  the other actually throwing/catching real `BufferUnderflowException`
  instances) found **no mismatch** across ~4.9M and ~3.6M checks.
- Most decisively: enhancing `CRATONVM_DBG_CAUSE` to also log the raw heap
  pointer (`ObjectRef::as_ptr()`), not just identity hash, on the REAL
  failing `ES93FlatBFloat16VectorFormatTests.testMultiClose` repro shows
  the victim `BufferUnderflowException`'s address is **IDENTICAL** at
  write time and read time (e.g. `ptr=0x220f7720` both times) — **the
  object never moves.** There is no relocation for a stale self-pointer to
  survive.

**What's actually happening: a live, valid `Object` reference gets written
into the cause slot's memory, in place.** Since the object's address is
constant, something performs a genuine (mistargeted) write to that exact
byte range sometime after construction. To catch it red-handed, two new
pieces of temporary diagnostic tooling were added (kept in tree, gated,
following the codebase's existing `CRATONVM_DBG_*` pattern):

- `cratonvm_gc::heap::{set_dynamic_watch, dynamic_watch_addr}` (`gc/src/heap.rs`)
  — a runtime-settable companion to the existing `CRATONVM_DBG_WATCH_CELL`
  env-var watchpoint, since the address to watch (the victim's cause slot)
  isn't known until the object is allocated mid-run. `cell_watch_check` now
  checks both.
- `NativeContext::dbg_set_watch_cell` (`native-api/src/registry.rs`, default
  no-op; overridden in `vm/src/vm/vm_exec.rs`) lets native code arm it.
- `write_throwable_cause` arms the watch (`CRATONVM_DBG_WATCH_CAUSE_SELF=<class>`)
  on the cause slot right after writing the self-sentinel for a matching
  class.

Running with `CRATONVM_DBG_CAUSE=1 CRATONVM_DBG_WATCH_CAUSE_SELF=java/nio/BufferUnderflowException`
catches the write **immediately** (within 1-2 log lines of construction, not
some arbitrary time later) at `gc::heap::write_slot`:

```text
CAUSE_DBG_WRITE this=java/nio/BufferUnderflowException hash=493794 ptr=0x221c56f0 cause=SELF
CAUSE_DBG_ARM watch=0x221c5738 for java/nio/BufferUnderflowException hash=493794
CAUSE_DBG_WRITE this=org/apache/lucene/index/CorruptIndexException hash=493823 ptr=0x221a8d58 cause=java/io/EOFException hash=493820
[CELLWATCH] write_slot: write [0x221c5738 +16) covers watch 0x221c5738 value=Object(Some(ObjectRef { ptr: 0x221c6048 }))
```

Reproduced across 4 independent runs (2 different builds, JIT-on and
`--nojit`): the corrupted cause value's address is **consistently exactly
`0x958` (2392) bytes past the victim's own base address** — not random
garbage, and not a stale/reused address (the offset is *positive* and
*relative to the still-live victim*, ruling out "old address got reused").
The corruption reliably happens right around construction of an
`org.apache.lucene.index.CorruptIndexException` wrapping an `EOFException`
(`native_exc_init_cause`, which calls `ctx.invoke_virtual(cause, "toString", …)`
mid-construction — a virtual dispatch, and thus a re-entrant window).

A Rust `Backtrace::force_capture()` at the `[CELLWATCH]` site is
**unreliable here** — it comes back as ~17 repeats of
`core::fmt::num::impl$35::fmt` plus a `jit_self_call_stack_guard` frame,
identically shaped in both JIT-on and `--nojit` runs, indicating the
Windows SEH unwinder loses the stack (likely once it crosses a JIT-compiled
frame lacking `.pdata`/`.xdata` unwind info) rather than reporting anything
real. A companion Java-level stack dump was added at the interpreter's
`Instruction::Putfield` site (`vm/src/runtime/interpreter.rs`, prints
`[WATCHFIELD]` + the full `thread.frames` when a putfield's target matches
the dynamic watch) — **it never fires**, so the corrupting write is not a
plain interpreted `putfield`.

The strongest lead so far comes from the codebase's own **pre-existing**
`CRATONVM_DBG_CELLCORRUPT`/`CRATONVM_DBG_BADREF` diagnostics
(`gc/src/gen_heap.rs::set_array_element`, already documented in-tree as
catching "an array-element write through a STALE array reference … writing
a raw 8-byte pointer into the middle of that object's 16-byte Value cells").
Enabling them on the same repro fires right as the corrupted cause gets
read:

```text
[CELLCORRUPT] holder=0xc6722398 (young_from=true old=false) class_id=0 class=java/lang/Object kind=0x01 num_slots=1 array_len=1 gc_flags=0x0 index=0 raw0=0x00000000c675dc80 raw1=0x0000000100000028
[CELLCORRUPT]   shift-test over 1 cells: valid@aligned=0 valid@+8=0 valid@-8=1
[CELLCORRUPT]   target-header: class_id=3040 class=org/apache/lucene/index/CorruptIndexException kind=0x00 num_slots=12 array_len=0 gc_flags=0x0
```

`holder=0xc6722398` is exactly the address the corrupted `cause` field
points to. It's a 1-element array (`kind=0x01`, `num_slots=1`,
`array_len=1`) whose declared component type is `class_id=0`
(`java/lang/Object`) — **which may or may not itself be legitimate**: a
genuine `new Object[1]` (e.g. varargs boxing) is correctly `class_id=0`
too, so this is not by itself proof of the separate, already-partially-fixed
"`ClassId(0)` fallback anti-pattern" family (`686de27c1`,
`cdbbc4152` — transient `ensure_class_initialized` failures minting a
zero-field `ClassId(0)` stub instead of retrying via
`ctx.ensure_synthetic_class`). What *is* conclusive: the **shift-test**
— reading this array's single element at its nominal (aligned) offset
produces garbage (fails the corrupt-cell check), but reading it **shifted
back by exactly 8 bytes** decodes as a valid pointer to a live
`org.apache.lucene.index.CorruptIndexException` (`num_slots=12`, matching
Throwable's 5 inherited slots + Lucene's own extra fields). That's an
**8-byte misalignment** in how this specific 1-element `Object[]`'s data
region is addressed — consistent with (though not yet proven to be) a
compact/legacy layout-size disagreement for arrays, in the same family as
the already-documented compact-ref-fields legacy-instance bug (see
`reference_compact_ref_fields_jit_inline_per_object_flag` — though that one
is `CRATONVM_COMPACT_REF_FIELDS`-gated and default-OFF, so if this is
related it's a different code path with the same *shape* of bug: an object
addressed at the wrong element/field stride).

**Not yet fixed.** What's confirmed:
- NOT a GC-relocation/self-forwarding bug (object never moves).
- NOT triggered by generic concurrency or by throwing/catching the specific
  exception type concurrently (two dedicated repros, both clean).
- NOT a plain interpreted `putfield` (the `[WATCHFIELD]` hook never fires).
- IS a genuine out-of-place write of a valid `Object` reference, landing
  consistently 2392 bytes past the victim, coinciding with
  `CorruptIndexException(String, DataInput, Throwable)` construction.
- The object the corrupted `cause` ends up pointing at is a 1-element
  `Object[]` array with an apparent 8-byte element-addressing bug, whose
  (correctly-offset) element points at that same `CorruptIndexException`.

**Recommended next steps for a follow-up session:**
1. Find what allocates a 1-element `Object[]` right around
   `CorruptIndexException`/`EOFException` construction in this test's call
   path (`CodecUtil.checkFooter` → `Lucene104PostingsReader.<init>` →
   `AssertingPostingsFormat.fieldsProducer` → …) — likely varargs boxing for
   a message-formatting call, or `Throwable.getStackTrace()`/suppressed-list
   machinery. `CRATONVM_DBG_CELLCORRUPT=1` is the fastest way back into this
   evidence.
2. Extend the `[WATCHFIELD]`-style java-stack dump (already added at
   `Instruction::Putfield`) to `aastore` (`0x53` in
   `vm/src/runtime/interpreter.rs`, around the `set_array_element` call) and
   to `gc::gen_heap::write_prim_element`/`set_field_as` call sites more
   broadly, to catch the actual write's Java call site directly instead of
   inferring it from timing.
3. Investigate whether the array's own allocation size/stride computation
   (wherever it's built — likely a `new_ref_array`/`alloc_array` call) is
   off by one `Value` cell (16 bytes) vs. 8 bytes somewhere in its header or
   component-size accounting, matching the shift-test's `-8` finding.

All temporary diagnostic tooling from this investigation (`CRATONVM_DBG_CAUSE`,
the dynamic watch mechanism, `[WATCHFIELD]`) is committed but **not merged
into dev** — it's exploratory and should be reviewed before landing
permanently. See branch `fix/es-vector-codec-exception-cause-object-20260710`.

## Original entry (2026-07-10, before this update)

Observed in:
- Run: `esfull-20260710-083851`
- Host: local Windows box
- CratonVM binary: `C:\craton\cratonvm-targets\es-full-local-20260710-083851\release\cratonvm-es-full-local-20260710-083851.exe`
- Suite mode: `craton` / JIT on
- Stopped partial run totals: 367 recorded classes, 69 PASS, 283 FAIL, 15 HANG, 0 CRASH

Family count:
- 3 FAIL rows:
  - `server org.elasticsearch.index.codec.vectors.ES815BitFlatVectorFormatTests` -> `ArrayIndexOutOfBoundsException`
  - `server org.elasticsearch.index.codec.vectors.es93.ES93HnswBFloat16VectorsFormatTests` -> `ArrayIndexOutOfBoundsException`
  - `server org.elasticsearch.index.codec.vectors.es93.ES93FlatBFloat16VectorFormatTests` -> `BufferUnderflowException`

User-visible signals:
```text
java.lang.ArrayIndexOutOfBoundsException
Caused by: java.lang.Object
```

```text
java.nio.BufferUnderflowException
Caused by: java.lang.Object
```

HotSpot controls:
- `ES815BitFlatVectorFormatTests`: PASS, 3.6s, run `esprobe-hotspot-vector-815-20260710`.
- `ES93FlatBFloat16VectorFormatTests`: PASS, 4.4s, run `esprobe-hotspot-vector-bfloat-20260710`.
- `ES93HnswBFloat16VectorsFormatTests`: PASS, 4.9s, run `esprobe-hotspot-vector-hnsw-bfloat-20260710`.

Focused CratonVM throw-debug evidence:
- Run: `esprobe-throw-aioobe-20260710`
- Class: `ES815BitFlatVectorFormatTests`
- Result: FAIL, 68.438s.
- Throw site:
```text
ATHROW class=java/lang/ArrayIndexOutOfBoundsException msg="<no msg>"
  ATHROW-STK[33] org/elasticsearch/index/codec/vectors/BaseKnnBitVectorsFormatTestCase.testRandom pc=580
```

- Run: `esprobe-throw-bufunder-20260710`
- Class: `ES93FlatBFloat16VectorFormatTests`
- Result: FAIL, 16.208s.
- Throw site:
```text
ATHROW class=java/nio/BufferUnderflowException msg="<no msg>"
  ATHROW-STK[39] org/apache/lucene/codecs/CodecUtil.checkFooter pc=122
  ATHROW-STK[38] org/apache/lucene/codecs/lucene104/Lucene104PostingsReader.<init> pc=183
  ATHROW-STK[33] org/apache/lucene/tests/index/BaseIndexFileFormatTestCase.testMultiClose pc=471
```

Evidence:
- AIOOBE stdout: `C:\craton\esfull-20260710-083851\results\esprobe-throw-aioobe-20260710\jit-throw-aioobe\logs\server.org.elasticsearch.index.codec.vectors.ES815BitFlatVectorFormatTests.out.log`
- AIOOBE stderr: `C:\craton\esfull-20260710-083851\results\esprobe-throw-aioobe-20260710\jit-throw-aioobe\logs\server.org.elasticsearch.index.codec.vectors.ES815BitFlatVectorFormatTests.err.log`
- BufferUnderflow stdout: `C:\craton\esfull-20260710-083851\results\esprobe-throw-bufunder-20260710\jit-throw-bufunder\logs\server.org.elasticsearch.index.codec.vectors.es93.ES93FlatBFloat.3125d9cdc58a.out.log`
- BufferUnderflow stderr: `C:\craton\esfull-20260710-083851\results\esprobe-throw-bufunder-20260710\jit-throw-bufunder\logs\server.org.elasticsearch.index.codec.vectors.es93.ES93FlatBFloat.3125d9cdc58a.err.log`

Interpretation:
- These are CratonVM-only vector codec correctness failures, but the exact lower-level corruption is not yet isolated.
- The bizarre `Caused by: java.lang.Object` output is itself a VM divergence and may be obscuring the real stack/cause.
- Keep this as one residual family until the common lower-level cause is split or proven separate.

Not duplicates:
- These rows are not the `FloatBuffer.order()` no-Code family; the throw-debug rows point to Lucene vector/random codec work and footer reading rather than no-Code dispatch.
- These rows are also not the older fixed vector score/value/footer families unless a later focused probe proves the same root.
