# W7-89 — the FFM liveness gate validated nothing, and the arity bug was the smaller half

Status: the defect W7-86-static-native-arity.md §6(1) left is **real, measured,
and fixed** — and measuring it found that fixing it alone would have changed
nothing, because the model it routes into had been inert in Compatible mode
since the day the carrier class became the real one. Four fail-open paths and a
cross-layout write are closed here.

Branch `fix/memorysession-checkvalidstate-20260812`, worktree
`C:/craton/CratonVM-memsess-20260812`.

**Nothing here was built.** This lane may not invoke `cargo`; the orchestrator
builds. Every CratonVM behavioural claim is a transcript of the prebuilt
current-dev binary `target/release/cratonvm.exe` of 2026-08-12 07:33 — the same
minute as `HEAD` (`2a69c76a3`, 07:33:58), so it is the current-dev arm and not a
day-old one. Every HotSpot claim is a transcript of Eclipse Adoptium
jdk-25.0.3.9 on this Windows host. Claims about the Rust source say "source-level".
`rustfmt --edition 2021 --check` was run on both edited files as a **parser**;
it reports no `error:` lines. That proves they parse and nothing else.

---

## 1. What CratonVM actually does on a closed-arena access

`probes/MemorySessionValidStateProbe.java`, both arms, sections A–D and F–G
(section E is skipped on CratonVM because it takes a fatal
`EXCEPTION_ACCESS_VIOLATION`; see §7.1). Rows that agree are omitted.

| row | HotSpot 25.0.3.9 | CratonVM, before |
|---|---|---|
| `C.closed.scope.isAlive` | `false` | **`true`** |
| `C.closed.get` | `IllegalStateException: Already closed` | **`NO-THROW:1432778632`** |
| `C.closed.set` | `IllegalStateException: Already closed` | **`NO-THROW:void`** |
| `C.closed.getAtIndex` | `IllegalStateException: Already closed` | `NO-THROW:0` |
| `C.closed.slice.get` | `IllegalStateException: Already closed` | `NO-THROW:1` |
| `C.closed.staticCheck` | `IllegalStateException: Already closed` | **`NO-THROW:void`** |
| `C.closed.staticCheck.slice` | `IllegalStateException: Already closed` | `NO-THROW:void` |
| `C.closed.reclose` | `IllegalStateException: Already closed` | `NO-THROW:void` |
| `C.closed.allocate` | `IllegalStateException: Already closed` | **`NO-THROW:8`** |
| `D.shared.closed.*` | same three, on `Arena.ofShared()` | same three misses |
| `F.confined.offThread.get` | `WrongThreadException: Attempted access outside owning thread` | **`NO-THROW:0`** |
| `F.confined.offThread.staticCheck` | `WrongThreadException` | `NO-THROW:void` |

`1432778632` is `0x55667788` — the value the probe wrote *before* `close()`. The
read after close returned it, so the block was still mapped: **a use-after-close
that reads live memory**, not a crash. §5.3 explains why it never became a
use-after-free.

The gate is therefore not merely mis-indexed. **Nothing in the FFM lifetime
model was working**: `close()` recorded nothing, `isAlive()` answered true
forever, thread confinement never fired, a closed arena still allocated, and a
second `close()` was silently accepted.

## 2. The arity defect is real, and it is not sufficient

W7-86 §4.1 row 6 reproduces exactly. `probes/MemorySessionValidStateProbe.java`
section A, measured identically on both VMs:

```
A.method.static     = true
A.method.paramCount = 1
A.method.param0     = java.lang.foreign.MemorySegment
```

`public static void checkValidState(MemorySegment)` — so by W7-86 §1's measured
calling convention `args[0]` is the **segment**. The body was

```rust
let this = obj_arg(args, 0)?;
p67_session_check_valid(ctx, this)?;
```

and `p67_session_check_valid` opens `if !p67_session_modelled(session) { return
Ok(()) }`. A segment is not a modelled session, so it returned `Ok(())` for every
input. The comment above it — "validity is a property of the session receiver" —
was describing the *zero-argument instance* overload registered eight lines up.

The JDK's own body (`lib/src.zip`, `jdk/internal/foreign/MemorySessionImpl.java:221`)
is one line:

```java
public static void checkValidState(MemorySegment segment) {
    ((AbstractMemorySegmentImpl)segment).sessionImpl().checkValidState();
}
```

so the repair is `p67_segment_check_scope`, which is precisely
`sessionImpl().checkValidState()` — and which **already existed**. Routing to it
is a four-line change. It would have moved nothing, for the reason in §3.

## 3. The root cause: the model's state word was written into a declared-REFERENCE slot

### 3.1 The instrument that separated "stable but wrong" from "minted fresh"

A `scope()` that mints a fresh always-open session answers `isAlive() == true`
forever; so does a stable-but-wrong one. Only identity separates them, so
`probes/MemorySessionIdentityProbe.java` asks nothing else:

| row | HotSpot | CratonVM |
|---|---|---|
| `confined.arena.scope.stable` (`arena.scope() == arena.scope()`) | `true` | **`false`** |
| `confined.seg.scope==arena.scope` | `true` | `false` |
| `confined.segA.scope==segB.scope` | `true` | `false` |
| `confined.slice.scope==seg.scope` | `true` | `false` |
| `heap.scope.stable` (`MemorySegment.ofArray`) | `true` | `true` |

`Arena.scope()` is `p67_receiver_session`, which mints a fresh session **only**
when `p67_arena_session` rejects the one the arena is holding. Every other
predicate in that chain is separately measured true:

* the arena's runtime class is `java.lang.foreign.Arena` (`MemorySessionShapeProbe`);
* it is exactly 2 slots wide (`CRATONVM_DBG_LAYOUT_ALIAS=1`: `class="java/lang/foreign/Arena" requested_fields=2 real_fields=0 direction="undeclared"`, whose text is "this object is exactly requested_fields wide"), and `p67_arena_session` needs `> 1`;
* slot 1 holds the session, written by `p67_new_arena` (source-level);
* the session's runtime class is `jdk.internal.foreign.MemorySessionImpl` with four declared instance fields (`MemorySessionShapeProbe` dumps `resourceList`, `owner`, `state`, `acquireCount`), and the layout instrument prints **no** row for that class, which for `classify(4, real)` means `real == 4` — so `object_num_fields >= 4` holds.

That leaves exactly one predicate: `p67_session_modelled`'s
`matches!(ctx.get_field(session, 0), Value::Int(_))`.

### 3.2 Dead on arrival, not killed by the first allocation

Two causes produce that same symptom, and the identity probe cannot separate
them because it allocates before it asks. `probes/MemorySessionPreAllocProbe.java`
asks first:

| row | HotSpot | CratonVM |
|---|---|---|
| `beforeAllocate.scope.stable` | `true` | **`false`** |
| `afterAllocate.scope.stable` | `true` | `false` |
| `neverAllocated.scope.stable` | `true` | `false` |
| `afterClose.scope.isAlive` | `false` | `true` |

Unstable **before any `allocate()`**, and unstable for an arena that never
allocates at all. So the rival allocator's cross-layout write (§4.3) — which
would also have clobbered the state word — is not the cause of this. It is a
separate defect, fixed anyway.

### 3.3 The failing predicate, read directly

§3.1 reaches `p67_session_modelled` by eliminating four other predicates. That
is sound but it is still elimination, so `probes/MemorySessionModelledProbe.java`
asks the last one on its own, with no arena, no segment and no resolution chain
in the way: take a session object, call `MemorySessionImpl.close()` **directly**
on it by reflection, then ask `isAlive()`.

| row | HotSpot | CratonVM |
|---|---|---|
| `beforeClose.isAlive` | `true` | `true` |
| `close` | `ok` | `ok` |
| `afterClose.isAlive` | **`false`** | **`true`** |
| `arenaScope.close` | `IllegalStateException: Already closed` | `ok` |
| `arenaScope.afterClose.isAlive` | `false` | `true` |

CratonVM's `close` native is `p67_session_just_close`, which opens
`if !p67_session_modelled(session) { return Ok(()) }` and otherwise writes the
state word; `isAlive` is `!p67_session_modelled(session) || state == 1`. A
modelled session therefore *must* answer `false` on the row after its own
`close()`. It answers `true`. **`p67_session_modelled` is false, measured on the
one object, with nothing else in the chain.**

Combined with the layout instrument's silence on this class — which for
`classify(4, real)` means `real == 4`, so `object_num_fields` is 4 and the width
half of the predicate holds — the failing half is
`matches!(ctx.get_field(session, 0), Value::Int(_))`: **the state word does not
read back as an int.**

(HotSpot's `arenaScope.beforeClose.isAlive` is already `false` because the row
above closed the same session object — `seg.scope()` and `arena.scope()` are one
object there. That is the contrast the probe is for.)

### 3.4 Why the write does not survive — inferred, not measured

Source-level, and the one step of the chain that is not measured here.

In Compatible mode the carrier is the **real, loaded**
`jdk/internal/foreign/MemorySessionImpl`, which declares `resourceList` and
`owner` (references) and `state` and `acquireCount` (ints). The model wrote its
`Int` state word into **slot 0**, and on that class slot 0 is a declared
reference. That is the primitive-in-a-reference-slot family,
W7-84-primitive-in-reference-store.md.

**This much is honest inference and not measurement, and one fact cuts against
the simple version of it.** W7-84 converged all four heaps on auto-boxing with
an un-boxing read, and HANDOFF-20260812.md records that
`cargo test -p cratonvm-gc --test primitive_in_reference_slot` passes 10/10
including `every_collector_agrees_on_a_primitive_in_a_reference_slot` — on a
tree this binary was built from. So "the primitive is dropped" is *not* a
sufficient account. **Corrected 2026-08-12: there are THREE candidates, not
two, and the second one now looks false.**

  (a) This object is not on the arm that test covers.
  (b) `NativeContext::get_field` does not reach the un-boxing read. **This now
      looks FALSE by reading**: all four collectors un-box inside their own
      `get_field` — `gc/src/heap.rs:680`, `gc/src/gen_heap.rs:3639`,
      `gc/src/g1.rs:9078`, `gc/src/zgc.rs:5211`, each calling
      `crate::autobox::unbox_reference_slot(…)` — and `NativeContext::get_field`
      routes to the collector. Anything going through `get_field` un-boxes.
  (c) **The un-boxing is not uniform across execution tiers.** The JIT's
      compact-reference field read does NOT un-box: neither the helper
      (`vm/src/jit/helpers.rs:5642-5660`, `jit_getfield`, which returns
      `jit_decode_ref_word` on the raw word — a heap-plausibility filter, not a
      class-id test) nor the default inline path (`jit/src/ir_lower.rs:2419-2421`
      and `jit/src/x64/bytecode_walk.rs:4034-4036`, a bare
      `MOV RAX, [RAX + disp32]`). `AUTOBOX_CLASS_ID` appears nowhere in `jit/`,
      `jit-api/`, or `vm/src/jit/`.

The transcripts in §1 and §3 were taken through natives, i.e. through
`NativeContext::get_field`, so (c) is **not** what produced these rows — (a)
remains the live explanation for the measurement. What (c) does establish is
that "the four GC tests pass" cannot be read as "every reader un-boxes": the
gc ratchet (`gc/tests/primitive_in_reference_slot.rs`) only scans the four
`gc/src` heaps, and the JIT is a fifth, independent field reader outside its
reach. Any lane instrumenting this must say **which tier it measured**.

Which of (a)/(c) applies here is still a question for a lane that can build and
instrument the heap, and is left open rather than guessed at (§6.7).

What is measured, and what the repair rests on, is narrower and enough: slot 0
of this object does not read back as a `Value::Int`, and slot 0 is the one slot
in the four that the real class types as a reference and the model used for an
int. Putting the int in the class's own int field removes the question rather
than answering it.

This is why the model looks correct in every unit test and in `--synthetic-jdk`:
there the carrier is a fabricated stub with **no** declared fields, the slots are
untyped, and any `Value` round-trips. The defect exists only where the class is
real, which is the mode that ships.

**The corollary is the interesting one.** `p67_session_modelled`'s stated purpose
is "is this a session *we* built?", and it was answering that question with a
test that had silently become "is the carrier class field-less?".

## 4. The repair

### 4.1 The four words are located by NAME

`native-builtins/src/phases_late/foreign_ffm.rs` gains `P67SessionSlots` and
`p67_session_slots`. The map is resolved per session object:

| model word | JDK field | kind |
|---|---|---|
| `state` | `state` | int |
| `acquires` | `acquireCount` | int |
| `owner` | `owner` | reference (`Thread`) |
| `actions` | `resourceList` | reference (the close-action list — the mapping is semantic, not merely kind-compatible) |

All four names must resolve or none is used, so the two maps can never be mixed;
the fallback is the old `0..3`, which is correct for the field-less carrier.
Resolving by name rather than permuting the constants means the repair does not
depend on the real class's field **order**, which this lane cannot measure
directly — and it makes every write type-correct, which is the actual
requirement. The old constants survive only as the fallback map's values; there
is no remaining absolute index in the file.

`panama.rs` calls the same resolver. It had a **second copy** of the index
(`PE_SESSION_STATE_FIELD = 0`, `PE_SESSION_SLOTS = 4`); both are deleted. That
second copy is the reason `pe_segment_check_scope` — correctly wired to
`get`/`set`/`getAtIndex`/`setAtIndex` and carrying a comment calling itself "the
single choke point" — had never once fired.

### 4.2 Three more fail-open paths, found while wiring the first

1. **`p67_session_modelled` now excludes a real session EXPLICITLY.** It used to
   fall out of the slot-0 read; with name resolution a real `ConfinedSession`
   resolves `state` too, and its encoding is the JDK's — `OPEN = 0`,
   `CLOSED = -1`, `NONCLOSEABLE = 1` (`MemorySessionImpl.java:66–68`) — the
   inverse of this model's `1 = open`. Interpreting one with the other's
   encoding would report **every live real session as closed**. That is exactly
   the over-correction §6 guards against, and it would have been introduced by
   the fix rather than found by it. `p67_session_is_real` is the same test the
   delegation path already uses, so the two agree by construction.
2. **`p67_segment_check_scope` accepted a non-session as a scope.** It returned
   `Ok(())` unconditionally once the segment's named `scope` field held
   anything — including one of OUR sessions, which is the *only* shape that
   resolves. Measured: `MemorySessionShapeProbe` reports
   `heap.field.scope = jdk.internal.foreign.MemorySessionImpl` on CratonVM
   against `GlobalSession$HeapSession` on HotSpot, because `createHeap` is
   force-dispatched into this file. It now checks a modelled session and falls
   **through** to the arena for anything that is neither real nor modelled —
   `get_field_by_name` resolves an index in the loaded class's hierarchy, and a
   synthetic segment is stamped with the `MemorySegment` INTERFACE, so the name
   can land on a slot the model owns.
3. **`asSlice` wrote the "no arena" marker into every slice's scope slot**, so no
   slice ever had a resolvable session (`C.closed.slice.get`). It now propagates
   the parent's session — and only a session `pe_segment_session` resolved, which
   is what keeps the slot's other tenant safe: on an `ofArray` segment slot 2
   holds the Java backing array (`SEG_BACKING_ARRAY_FIELD`), an array resolves to
   no session, and such a slice keeps its historical `Object(None)`. So
   `isNative()` and `sync_heap_backed_segment` see exactly what they saw before.

### 4.3 Two arena layouts, one allocator

Found by following the census rather than the source. `--dump-native-registry`
on the probe run:

| triple | `owns_slot` | `invocations` | registrar |
|---|---|---|---|
| `Arena.ofConfined ()Ljava/lang/foreign/Arena;` | true | 4 | `foreign_ffm.rs:1645` (`p67_new_arena`, **2 slots**) |
| `Arena.allocate (J)…` | **true** | 7 | `foreign_ffm.rs:2729` → `crate::panama::pe_arena_allocate` |
| `Arena.allocate (J)…` | false | 0 | `foreign_ffm.rs:1669` (dead) |
| `Arena.close ()V` | true | 5 | `foreign_ffm.rs:1712` |
| `MemorySegment.get (…OfInt;J)I` | **true** | 7 | `panama.rs:788` (`pe_segment_get`) |
| `MemorySegment.get (…OfInt;J)I` | false | 0 | `foreign_ffm.rs:1999` (dead) |
| `MemorySegment.set (…OfInt;JI)V` | **true** | 4 | `panama.rs:801` |
| `MemorySegment.scope ()…` | true | 3 | `foreign_ffm.rs:2119` |
| `MemorySessionImpl.checkValidState (Ljava/lang/foreign/MemorySegment;)V` | true | **0** | `foreign_ffm.rs:1941` |

Two things fall out of that table.

**The arena is built by one file and allocated from by another.**
`pe_arena_allocate_impl` was written for `register_pe_arena`'s FOUR-slot arena
(`[0]` global flag, `[1]` alloc-id array, `[2]` closed flag, `[3]` count) but in
Compatible mode receives `p67_new_arena`'s TWO-slot one (`[0]` open,
`[1]` session). So on every Compatible-mode allocation it read slots 2 and 3
**past the end of the object** and called `set_array_element` on slot 1 — which
holds the **session**, not an array. Both reads are now gated on the object's
width, and the id write additionally carries W7-83's kind screen
(`ctx.object_is_array`). `pe_arena_allocate_impl` also now consults the resolved
session, which is what makes `C.closed.allocate` raise.

**`checkValidState(MemorySegment)` has `invocations: 0`.** The static overload is
never called in Compatible mode: our segments are synthetic and our `get`/`set`
are panama's, so the JDK bytecode that would call it never runs. Its arity bug
was therefore *live-registered but never dispatched* on this workload. Repairing
it is still right — it is reachable from any Java caller, and the probe calls it
directly by reflection — but the row that mattered for real programs was the
model behind it. **A registration that owns its slot is not the same as a
registration that runs**, and only the `invocations` column distinguishes them.

### 4.4 What this does NOT change

* **No registration was added, moved or removed.** Every edit is a body. No
  `set_category`/`with_category` scope moved, so no `NativeKind` block boundary
  moved and last-write-wins is unaffected.
* **W7-83's two kind screens are verified untouched** —
  `native-io/src/lib.rs:7900` and `native-builtins/src/servlet.rs:3418` both
  still read `ctx.heap_kind_of(a) == ObjectKind::Array`, checked by grep after
  the last edit. Nothing in this lane touches `bb_resolve_heap_array` or
  `s2_bb_arr`.
* **No `CRATONVM_*` variable was added**, so the four-file flag surface
  (`types/src/flag_groups.rs`, `types/tests/flag-surface.txt`,
  `docs/flag-tokens.md`, `docs/config/flag-inventory.md`) is untouched and
  `cargo test -p cratonvm-types` sees no new token.
* **No side table was needed.** The brief anticipated one, with W7-72's
  identity-hash keying and GC-remap discriminator. It is not needed and would
  have been the wrong instrument: the segment→session mapping already exists in
  the object graph (`AbstractMemorySegmentImpl.scope` by name for a real
  segment, slot 2 for a synthetic one, and both were already read). The defect
  was in the session's own state word, not in finding the session. An
  address-keyed table would have added a recycling hazard to fix a problem that
  had none.
* **No test was weakened.** Nothing existing was edited. The four `panama.rs`
  arena unit tests build a genuine 4-slot arena with a real
  `new_array(Long, 256)` at slot 1, so `four_slot_layout` is true and
  `object_is_array` answers true (that mock is honest — W7-83 §2.2); the new
  `pe_arena_session` call resolves to `None` there (a `long[]` fails the session
  class-name memo) and is a no-op.

## 5. Blast radius

Compatible mode is contractually frozen except genuine parity fixes. **This is a
widening into a throw**, and the carve-out it claims is that HotSpot raises
`IllegalStateException: Already closed` on all of these and CratonVM returned.
Stated bluntly so it is expected rather than discovered:

### 5.1 What starts throwing

* Any code that touches a segment after its `Arena` closed — `get`, `set`,
  `getAtIndex`, `setAtIndex`, on the segment or on a slice of it.
* `arena.allocate(...)` after `arena.close()`.
* A second `arena.close()`.
* **Thread confinement**: a segment from `Arena.ofConfined()` accessed off the
  creating thread now raises `WrongThreadException`. This is the widest of the
  four, because it fires on *live* segments and on code that never closes
  anything. A library that creates a confined arena on one thread and hands the
  segment to another was silently tolerated and is not any more. HotSpot has
  always refused it (`F.confined.offThread.get`), so the direction is right —
  but a workload that reddens here is reddening on a rule it was already
  breaking, and that is worth saying up front rather than triaging as a
  regression.

### 5.2 What must NOT start throwing — the over-correction guard

A gate that refuses everything passes every row in §1 and fails these. They are
in the same probe run, deliberately, so a one-sided fix cannot look green:

* `B.live.*` — a live confined arena: `get`/`set` round-trip, `asSlice(...).get`,
  `scope().isAlive() == true`, and `staticCheck` returning normally.
* `E.global.*`, `E.auto.*`, `E.heap.*`, `E.null.*` — `Arena.global()`,
  `Arena.ofAuto()`, `MemorySegment.ofArray(byte[])` and `MemorySegment.NULL` are
  sessions that can never close; every one must still read, write and answer
  `isAlive() == true`.
* `D.shared.live.*` — the same for `Arena.ofShared()` before its close.
* `F.confined.onThread.get` — the owning thread must still be let through, which
  is the row that fails if confinement is enforced against the wrong thread
  object.
* `heap.scope.stable` / `heap.slice.scope==heap.scope` in the identity probe —
  the two rows CratonVM already passes, and which a change to the slot map could
  break.

`MemorySegment.NULL` is worth one line of its own: its `scope` field resolves to
a `java.lang.String` on CratonVM (measured). Under the repair that is neither a
real nor a modelled session, so `p67_segment_check_scope` falls through and the
access proceeds — `E.null.staticCheck` stays `NO-THROW`. Before §4.2(2) it would
have been *accepted as a scope*.

### 5.3 What is NOT in the radius

**No use-after-free is introduced.** `pe_arena_allocate_impl` records its
`alloc_id` into the arena's id array, and `pe_arena_close` frees from that array
— but `Arena.close()` in Compatible mode is `foreign_ffm.rs:1712`, which closes
the session and never calls `pe_arena_close`. Combined with §4.3 (the id was
being written into the session, not an array), **native memory allocated by an
FFM arena is never freed in Compatible mode today**. That is a leak, it is
pre-existing, this lane does not fix it, and it is the reason `C.closed.get`
returned the old value instead of faulting. The order matters for the next lane:
**the liveness gate must land before anyone makes `close()` free**, or a
use-after-close becomes a use-after-free. It now has.

## 6. What still passes unchecked

Named exactly, because "the gate works now" would be the wrong summary.

1. **The `checkValidState(MemorySegment)` static is still never dispatched on a
   normal Compatible-mode workload** (`invocations: 0`, §4.3). It is correct
   now; it is not load-bearing there.
2. **`MemorySegment.copy`, `fill`, `mismatch`, `asByteBuffer`** do not route
   through `pe_segment_access_addr` and were not audited. `B.live.copyFrom`
   raises `AbstractMethodError: … copyFrom … has no Code attribute` on CratonVM
   — an unregistered descriptor, a different defect, untouched.
3. **A `ByteBuffer` view of a closed segment** (`G.closed.buffer.getInt`) is
   unmeasured on CratonVM: section G produced **no output at all**, so it dies
   before its first row. HotSpot raises `IllegalStateException` there. This is
   the same object graph W7-83 screened and is the natural next lane.
4. **A real JDK session** — `ConfinedSession`, `SharedSession`, `GlobalSession`
   — is deliberately left strictly alone in both files. We cannot read its state
   word without owning its encoding, so it is delegated (`p67_session_delegate`)
   or skipped, never interpreted. If a path ever hands one to
   `p67_session_check_valid`, it is a no-op by construction.
5. **`pe_arena_close` still reads the four-slot layout unguarded.** It is not the
   Compatible-mode winner, so it cannot be reached with a two-slot arena there.
   Whether `--synthetic-jdk` pairs it with `p67_new_arena`'s two-slot arena is
   **not measured** — this lane has no synthetic-jdk binary — and is left as a
   stated unknown rather than a guessed fix.
6. **`Arena.global()` mints a fresh arena on every call** rather than returning a
   singleton (`NULL.scope==global.scope` is `true` on HotSpot, `false` here).
   Harmless for liveness because nothing closes the global arena, so it is
   recorded rather than fixed.
7. **Why the slot-0 write does not survive is not fully explained** (§3.4).
   That slot 0 does not read back as an `Int` is measured; whether the value is
   dropped, boxed-without-an-unboxing-read, or something else needs a lane that
   can build and instrument the heap. The repair does not depend on the answer —
   it stops writing an int into a reference slot — but the next reader should
   not take §3.4 for a closed question, because W7-84's own gc-crate test passes
   on this tree.
8. **The `state` encoding collision.** The model writes `1 = open` / `0 = closed`
   into the real class's `state` field, whose JDK meaning is `0 = OPEN`,
   `-1 = CLOSED`, `1 = NONCLOSEABLE`. Every method that reads `state` is on
   `force_native_over_real_jdk_bytecode`'s list for `MemorySessionImpl`, and no
   real subclass is ever instantiated, so no JDK bytecode reads our word today.
   That is an argument from the current force-list, not a guarantee: a lane that
   *removes* a name from that list must re-check this. Normalising the model onto
   the JDK's own encoding would remove the hazard and is the honest follow-up.

## 7. Defects found in passing, not fixed

### 7.1 A fatal SIGSEGV on a heap segment

`MemorySegment.ofArray(new byte[16]).set(JAVA_INT_UNALIGNED, 0, 7)` takes
`EXCEPTION_ACCESS_VIOLATION (0xC0000005) … read at address 0x10` and kills the
process. `addr=0x10` is the signature the memory index already carries for a
`Buffer.address` read as a pointer. It is unrelated to liveness — the segment is
alive — and it is why `MemorySessionValidStateProbe` takes a section selector
(`… MemorySessionValidStateProbe ABCDFG`) rather than losing every later
section to it. The probe was also changed to print and **flush per row** rather
than buffer to the end, for the same reason: a buffered probe on a VM that dies
mid-run reports nothing at all.

### 7.2 `MemorySegment.set` is refused without `--enable-native-access`

CratonVM raises `IllegalCallerException: Native access is not enabled for this
module (MemorySegment.set denied)` where HotSpot 25 permits it — `set` is not a
restricted method. Every command in this record therefore passes
`--enable-native-access=ALL-UNNAMED` to **both** VMs, so the flag is a constant
of the comparison and not a variable of it. Recorded, not fixed.

### 7.3 `copyFrom` has no registered descriptor

`B.live.copyFrom` → `AbstractMethodError: method
java/lang/foreign/MemorySegment.copyFrom(…)… has no Code attribute`. Per
panama.rs's own comment on the `get`/`set` descriptor lists, an unregistered
descriptor on this interface is not a slow path but a hard throw. Untouched.

## 8. Reproducing

```
javac -d out probes/MemorySessionValidStateProbe.java \
             probes/MemorySessionShapeProbe.java \
             probes/MemorySessionIdentityProbe.java \
             probes/MemorySessionPreAllocProbe.java

# the RED and its over-correction guard, side by side
java     --enable-native-access=ALL-UNNAMED \
         --add-exports java.base/jdk.internal.foreign=ALL-UNNAMED \
         -cp out MemorySessionValidStateProbe            # == the .expected.txt
cratonvm --enable-native-access=ALL-UNNAMED \
         --add-exports java.base/jdk.internal.foreign=ALL-UNNAMED \
         -cp out MemorySessionValidStateProbe ABCDFG     # E crashes, see 7.1

# is scope() stable, and is it dead before the first allocate?
java     --enable-native-access=ALL-UNNAMED -cp out MemorySessionIdentityProbe
cratonvm --enable-native-access=ALL-UNNAMED -cp out MemorySessionIdentityProbe
java     --enable-native-access=ALL-UNNAMED -cp out MemorySessionPreAllocProbe
cratonvm --enable-native-access=ALL-UNNAMED -cp out MemorySessionPreAllocProbe

# the object graph the repair keys on (needs --add-opens to read the fields)
cratonvm --enable-native-access=ALL-UNNAMED \
         --add-exports java.base/jdk.internal.foreign=ALL-UNNAMED \
         --add-opens   java.base/jdk.internal.foreign=ALL-UNNAMED \
         -cp out MemorySessionShapeProbe

# which registration wins, and which ones actually RUN
cratonvm --dump-native-registry census.json --enable-native-access=ALL-UNNAMED \
         -cp out MemorySessionValidStateProbe ABCDFG

# the arena's real width
CRATONVM_DBG_LAYOUT_ALIAS=1 cratonvm --enable-native-access=ALL-UNNAMED \
         -cp out MemorySessionIdentityProbe
```

Three `.expected.txt` files carry the HotSpot column. `MemorySessionShapeProbe`
has none: its output is runtime class names, which differ between the VMs by
design, and an expected file would assert HotSpot's implementation classes as
though they were a contract.

## 9. This record is NOT discharged by the 70/0 suite green

HANDOFF-20260812.md established that `run.sh` schedules `CORE_CLASSES` and
`JDKONLY_CLASSES` only, and that **`probes/` is never run at any `SUITE=`
value**. Every piece of evidence here is a probe. So a green regression run is
non-regression evidence for this change and nothing more: it says the widening
in §5.1 broke none of the scheduled fixtures, which is worth knowing and is not
the same as showing the gate now fires.

Checked rather than assumed. Grepping all 70 scheduled classes (the
`CORE_CLASSES`/`JDKONLY_CLASSES` lists read out of `regression-suite/run.sh`)
for `java.lang.foreign` or `Arena.` returns exactly one file,
`regression-suite/src/RForeignLayoutJdkInterfaces.java` — and its only hit is a
**javadoc sentence** explaining that `MemorySegment` is sealed and so cannot
receive a foreign implementor. It makes no FFM call. **No scheduled fixture
opens an `Arena`**, so there is no vector on the defect path today, and none of
the §5.1 widenings can be reached by the suite either.

Writing one is the honest way to make this record dischargeable, and it is a
smaller job than the probes: close an arena and assert the throw, plus the two
over-correction arms from §5.2 (a live arena still reads and writes; a global or
heap segment still does). Both assertions in one fixture, because the arm that
catches an over-correction is the arm a "fix" is most likely to have broken.

**DONE 2026-08-12 — see §11.2.** It is
`RForeignLayoutJdkInterfaces.foreignArenaLifetime()`, 22 checks, in
`CORE_CLASSES` already so no `run.sh` change was needed. The "reads and writes"
half of the over-correction arm had to be replaced by `allocate`/`byteSize`,
because `get`/`set` are gated on `--enable-native-access` and this suite passes
no such flag (§7.2); §11.2 lists what that costs.

## 10. The single next step

**Run the two behavioural probes on a binary built from this branch.** Every
CratonVM column here is the *before*; the *after* column has never been
produced, for this lane or for W7-83, W7-76, W7-69 or W7-58. `C` and `D` must
flip to HotSpot's answers, `F.confined.offThread.*` must flip, and **`B` and `E`
must not move** — that last clause is the whole guard, and a run that reports
only the first two has not tested the fix.

---

## 11. The fix is on the live path, and §9's fixture now exists (2026-08-12)

Two things this section settles, neither of which needed a build.

### 11.1 The witness was re-tested before the fix was trusted

W7-89's own §2 is the cautionary tale — the arity bug was real and fixing it
alone would have moved nothing, because the predicate behind it was false. So
the repair's *own* predicate was re-read end to end rather than assumed, in the
merged tree:

| link | verified | where |
|---|---|---|
| the model no longer holds an absolute index | `p67_session_slots` resolves `state`/`acquireCount`/`owner`/`resourceList` by name and falls back to `P67SessionSlots::SYNTHETIC` only when **all four** miss | `native-builtins/src/phases_late/foreign_ffm.rs`, `p67_session_slots` |
| our own session is still MODELLED | `p67_session_is_real` requires the class name to differ from `jdk/internal/foreign/MemorySessionImpl`, and `p67_memory_session` allocates with exactly that name — so the new real-session exclusion cannot swallow the model's own carrier | `p67_session_is_real`, `p67_memory_session` |
| the width half of the predicate holds | `required_width()` is `1 + max(index)`; on the real four-field carrier that is 4, and `object_num_fields` is 4 | `P67SessionSlots::required_width` |
| a REAL session is still left alone | `p67_session_modelled` returns false for it *first*, before any slot read, so the JDK's inverse `OPEN = 0` encoding is never interpreted with this model's `1 = open` | `p67_session_modelled` |
| the second copy of the index is gone | `panama.rs` has no `PE_SESSION_STATE_FIELD` / `PE_SESSION_SLOTS` and both `pe_session_modelled` and `pe_session_check_open` call the shared resolver | `native-builtins/src/panama.rs` |
| the arena width gate landed | `let four_slot_layout = ctx.object_num_fields(arena_obj) > 3;` gates both the slot-2 read and the slot-3 read, and the id write additionally carries `ctx.object_is_array` | `pe_arena_allocate_impl` |
| W7-83's two screens are still there | `native-io/src/lib.rs` and `native-builtins/src/servlet.rs` both still read `ctx.heap_kind_of(a) == ObjectKind::Array` | grep, both files |

**Verdict: the landed fix is on the live path.** Nothing here is a measurement
of behaviour; it is the confirmation that the four repairs are the code that
runs, which is what §2 shows cannot be taken for granted.

### 11.2 The scheduled fixture — `RForeignLayoutJdkInterfaces.foreignArenaLifetime()`

§9 asked for it in as many words ("close an arena and assert the throw, plus the
two over-correction arms from §5.2 … Both assertions in one fixture") and named
`regression-suite/src/RForeignLayoutJdkInterfaces.java` as the only scheduled
class that so much as mentions `java.lang.foreign`. That is where it went, so it
is scheduled by the existing `CORE_CLASSES` word list with **no `run.sh` edit**
— a new `src/*.java` in no list is the `RJdkPhaser` failure mode and would have
looked like coverage while being none.

22 checks. Each RED row's expected value is copied from §1's measured
transcript, not reasoned about:

| rows | discharges | before (measured, §1/§3) |
|---|---|---|
| `a closed arena's scope is NOT alive`, ×2 factories | `C.closed.scope.isAlive`, `D.shared.closed.*` | `true` |
| `allocate() on a closed arena` → `IllegalStateException`, ×2 | `C.closed.allocate` | `NO-THROW:8` |
| `close() on an already-closed arena` → `IllegalStateException`, ×2 | `C.closed.reclose` | `NO-THROW:void` |
| `arena.scope() is stable across calls`, `a segment's scope is its arena's scope`, `a second segment shares the same scope` | `confined.arena.scope.stable`, `confined.seg.scope==arena.scope`, `confined.segA.scope==segB.scope` (§3.1) | `false` on all three |
| the LIVE arm — a live arena's `scope().isAlive()`, `allocate`, `byteSize` | §5.2's `B.live.*` / `D.shared.live.*` | green, and must stay green |
| `Arena.global()`, `Arena.ofAuto()`, `MemorySegment.ofArray` — alive, allocating, and (heap only) a stable scope | §5.2's `E.global.*` / `E.auto.*` / `E.heap.*` and `heap.scope.stable` | green, and must stay green |

**What it deliberately cannot cover, and why that is not a choice.** The suite
passes no `--enable-native-access`, and `MemorySegment.get`/`set`/`getAtIndex`/
`setAtIndex`/`copy`/`fill` are all gated on it (`require_native_access`,
`native-builtins/src/panama.rs`) — they would raise `IllegalCallerException`
here for a reason that has nothing to do with liveness (§7.2). So:

* **`C.closed.get` / `C.closed.set` / `C.closed.slice.get` are not in the
  fixture.** The use-after-close *read* — the row that returned `0x55667788`
  after `close()` — stays probe-only.
* **Thread confinement is not in the fixture either**, and it is the widest of
  §5.1's four widenings. `p67_session_check_valid` is where the owner check
  lives, and in Compatible mode the only ungated paths into it are `close()` and
  the arena allocator; `pe_session_check_open`, which is what
  `pe_arena_allocate_impl` calls, checks the state word **only** and has no
  owner test at all (read, not inferred). So an off-thread `allocate` on a
  confined arena does *not* raise, and asserting that it does would have pinned
  a behaviour this repair does not implement.
* Making either coverable needs one line in `run.sh`'s `class_cv_args` giving
  this class `--enable-native-access=ALL-UNNAMED`. That is a runner change, it
  is outside the lane that wrote this section, and it is the honest next step for
  §6.3 as well.

`Arena.global().scope() == Arena.global().scope()` is also absent, deliberately:
§6.6 records that `Arena.global()` mints a fresh arena per call, so that row is
red for a reason this record does not fix, and putting it in a scheduled fixture
would freeze a known divergence into the green baseline.

---

## 12. Triage re-read against source, 2026-08-12 (lane A24, doc-only)

This lane may not build, check, test or run anything, so nothing here is a
measurement. §11.1 already re-read the repair's own predicate; this section
re-reads it independently, checks whether the evidence is scheduled, and adds one
finding about this family that the record did not have.

### 12.1 §11.1's chain re-verified, independently

Every link §11.1 claims is present in the tree today:

* `P67SessionSlots` (`native-builtins/src/phases_late/foreign_ffm.rs:588`) and
  `p67_session_slots` (`:623`), which resolve all four names or fall back to
  `P67SessionSlots::SYNTHETIC` — the all-or-nothing rule §4.1 says the two maps
  can never be mixed by.
* **The second copy really is gone.** `native-builtins/src/panama.rs` has no
  `PE_SESSION_STATE_FIELD` and no `PE_SESSION_SLOTS`; the only surviving mention
  is a historical comment at `:1465` (*"This file used to hard-code `state` at
  slot 0"*), and both live sites — `:1553`, `:1646` — call
  `crate::phases_late::foreign_ffm::p67_session_slots`. That matters more than
  it reads: §4.1 identifies that duplicate index as the reason
  `pe_segment_check_scope`, a function whose own comment calls itself "the single
  choke point", had never once fired.
* Fourteen further `p67_session_slots` call sites across `foreign_ffm.rs`
  (`:647, 721, 882, 888, 896, 933, 959, 973, 986, 1026, 1074, 1891, 2017,
  2039`), i.e. the resolver is the file's only route to those words rather than
  one of two. (Corrected 2026-08-12: this said "nineteen". The file has 15
  occurrences of the symbol, one of which is the definition at `:623`, so 14
  are call sites. The point is unaffected; the count was not checked.)

**This is reading, not verification of behaviour.** §10 is still the whole
question and is still untaken: no *after* column exists for
`MemorySessionValidStateProbe`, and §10's guard clause — that `B` and `E` must
**not** move — is what a run that reports only `C`/`D`/`F` fails to test.

### 12.2 The evidence, and whether it is scheduled

| evidence | scheduled? |
|---|---|
| §1/§3's four probes (`MemorySessionValidStateProbe`, `…Identity…`, `…PreAlloc…`, `…Shape…`) — every measured row in this record | **NO.** §9 says so and it re-checks true: the string `probes` occurs **zero** times in `regression-suite/run.sh` at any `SUITE=` value. |
| §11.2's `RForeignLayoutJdkInterfaces.foreignArenaLifetime()` | **YES, and it is wired.** Defined `regression-suite/src/RForeignLayoutJdkInterfaces.java:538` and **called** at `:612` — the half that turns a method into coverage. `RForeignLayoutJdkInterfaces` is in `run.sh`'s `CORE_CLASSES` list, so no `run.sh` edit was needed and none was made. |

So §9's verdict stands unchanged: a green suite is non-regression evidence for
this change, and the 22 checks of §11.2 are the only part of it a suite run can
see. §11.2's three exclusions — the use-after-close *read*, thread confinement,
and `Arena.global()` identity — remain outside any scheduled vector.

### 12.3 The 33-probe reachability screen's FFM row is not what it looks like

A campaign-level screen (HotSpot 25 oracle 33/33; `--jdk-only` 28/33) reports the
FFM downcall arm failing with
`NoClassDefFoundError: java/lang/foreign/DowncallHandle`. Read as "strict mode is
missing an FFM class", that would be a large new row on this family. It is not.

**`java.lang.foreign.DowncallHandle` is a class no real JDK declares.** It is
CratonVM's own fabricated carrier. The tree states this in its own words at
`regression-suite/src/RJdkForeign.java:24-28`, and the class name is
special-cased in dispatch at `vm/src/vm/vm_exec.rs:1907`. Three frozen artefacts
carry it as a known synthetic:

* `scripts/baselines/jdk-only-kind-map-25-linux.tsv:3642-3645` — four triples,
  `synthetic-stub`;
* `scripts/baselines/jdk-only-gated-never-delete.tsv:28-31` — the same four,
  pointing at `native-builtins/src/phases_late/foreign_ffm.rs:2883-2901`;
* `scripts/baselines/jdk-only-dead-everywhere-GATED.tsv:12-15` — the `bridge`
  copies, marked `class-absent`.

So that screen row is **strict mode refusing a fabricated carrier, which is what
`--jdk-only` is for** — the same species as this record's §5.2 `MemorySegment.NULL`
row, where the `scope` field resolves to a `java.lang.String` and the repair
correctly falls through rather than accepting it. It is a control, not a defect,
and it does not add a row to §6. What it *does* say is that FFM downcalls have no
real-JDK carrier in this VM at all, which bounds how much of §6.2's unaudited
surface (`copy`, `fill`, `mismatch`, `asByteBuffer`) can be reached from a real
downcall in the first place.

Symmetrically: the same screen's `ByteBuffer.allocateDirect` + `putInt`/`getInt`
row **passing** is also a control, not a refutation of anything here. A default
`--jdk-only` build does not compile `register_nio_natives` at all
(`native-io/src/lib.rs:6875`, `#[cfg(feature = "synthetic-jdk")]` **and**
`!registry.drops_real_layout_synthetic()`), so that probe was answered by real
JDK bytecode plus the s2 family — neither of which is the code this record
repairs.

### 12.4 The tagged-handle hazard belongs to this family, and this record does not name it

§7.1 records a fatal `EXCEPTION_ACCESS_VIOLATION … read at address 0x10` on
`MemorySegment.ofArray(...).set(...)` and correctly identifies `0x10` as a
`Buffer.address` read as a pointer. There is a **second, larger** value of the
same shape that this record never mentions and that sits directly on the FFM
path:

`Unsafe.allocateMemory` returns a **tagged handle, not an address**.
`native-builtins/src/unsafe_natives_ext.rs:4151` declares
`const ARENA_TAG: i64 = 1 << 62` with `ARENA_BASE = ARENA_TAG | 0x10_0000_0000`,
and the doc is explicit that *"The tag is part of the address value end-to-end —
it is NEVER stripped"*, flowing opaquely through Java as a `long` including
`DirectByteBuffer.address()`. Consequently **every direct `ByteBuffer`'s
`address` in this VM is undereferenceable**, and a `0x4000_0010_…` value in a
register at a SIGSEGV inside a third-party `.so` **is** that diagnosis rather
than a corruption. Only `GetDirectBufferAddress` is audited to convert one.

Relevance to this record, stated narrowly so it is not over-claimed:

* §6.3's next lane — "a `ByteBuffer` view of a closed segment"
  (`G.closed.buffer.getInt`), which produced **no output at all** on CratonVM —
  is walking straight into this. A section that dies before its first row is
  consistent with a fault, and this is the fault that family takes.
* §5.3's ordering rule gains a second reason. It already says the liveness gate
  must land before anyone makes `close()` free. It must also land before anyone
  makes a segment's address dereferenceable, or the same edit turns a
  use-after-close into a use-after-free *through a handle the pointer screens do
  not recognise*.
* The exact classifier exists — `unsafe_arena_addr_is_tagged`
  (`unsafe_natives_ext.rs:4447`) — and the ByteBuffer-side screen
  `is_plausible_native_addr` (`v >= 0x1_0000`, two copies) does not consult it,
  so a tagged handle passes it trivially. That is
  W7-76-bytebuffer-alias-residuals.md §11.3 and NOMINATION 1 there; it is
  recorded here only because §6.3 routes the next lane into it.

**Not a vacuous test, and not a new defect in this record** — no claim in W7-89
rests on a pointer-plausibility screen. It is a hazard on the path §6.3 names as
"the natural next lane", written down so that lane does not rediscover it from a
crash dump.

### 12.5 Nothing is nominated here

Every repair this record describes is already landed and re-read. §12.3 and
§12.4 are corrections to the record's *reach*, not to its code, and the one code
change they imply belongs to W7-76's file and is nominated there.
