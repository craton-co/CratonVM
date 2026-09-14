# G49-1 — the two largest coercion clusters, re-derived from the instrument

**Status:** BOTH CLUSTERS ADJUDICATED BENIGN (MEASURED) / ONE UNRELATED DEFECT
FOUND AT THE SAME SITE AND FIXED-SOURCE / AFTER-LINE PREDICTED.

**Provenance.** Every count, species, descriptor, read/store split and
differential in §1–§5 is **MEASURED** on
`C:/craton/target-rel3/release/cratonvm.exe` (built from `9ae371468` out of
`C:\craton\cvm-mergecheck\`, i.e. a **BEFORE** binary that provably cannot
contain this lane's edits), against **HotSpot 25.0.3+9-LTS**
(`Temurin-25.0.3+9`). `C:/craton/target-rel4/` **does not exist**, so the
`access` / `class_id` / `index` fields G45-1 predicted are still
`"unattributed"` / `-1` / `-1` in every event this lane could observe; the
read/store split below is recovered from the **backtraces** instead (frame 6 is
`VmHeap::get_field_as` or `VmHeap::set_field_as`), which is stronger evidence
than the field would have been because it names the accessor, not a flag.
This lane may not build, so §6's after-line is **PREDICTED**.

**Files owned and changed:** `native-builtins/src/reference.rs`,
`native-builtins/src/properties_sidetable.rs`. Nothing else was touched.

**Scripts / logs:** `scratchpad/g49/` (parser `parse.py`, 19 instrumented
vector logs, four Java differentials under `j/`).

---

## 0. The headline

| the brief asked | answer |
|---|---|
| Is the 670 at `reference::native_rq_poll` genuinely benign? | **Yes.** MEASURED 796 events over 19 vectors, **100% reads**, **100% `value=Int(0)`**, and `CRATONVM_DBG_OVERLAY=1` names the slot outright: `class=java/lang/ref/ReferenceQueue slot=0 … real=head:Ljava/lang/ref/Reference;`. Nothing writes an `Int` there. A 13-check differential against HotSpot is identical. |
| What is the 245 at `properties_sidetable::props_defaults`? | **Also benign, and G45-1's suspicion was wrong.** MEASURED 337 events, **100% reads**, **100% `value=Int(0)`** — `java.util.Properties.defaults` on a `Properties` that has no parent. Two differentials totalling 22 checks, including every hard `defaults` case in the JDK contract, are identical to HotSpot. The fall-through chain is **not** broken. |
| Was a fix warranted for either cluster? | **No.** Both are the never-initialised-slot shape, and the coercion answers `null`, which is what both fields mean. Changing either reader would be a widened refusal for zero gain. |
| Did the instrument find anything at these sites? | **Yes, one real defect, and it is the same mechanism read from the other end.** `Reference.isEnqueued()` answered **`false` after a successful `enqueue()`** for weak, soft and phantom references alike. Fixed in `reference.rs`. |

The defect is worth stating plainly because it is exactly the shape G30-1
describes, inverted:

```
MEASURED, cratonvm --jdk-only, before:        MEASURED, java (HotSpot 25.0.3+9):
  CK enqueue=true                               CK enqueue=true
  CK afterEnq_isEnqueued=false      <-- WRONG   CK afterEnq_isEnqueued=true
  CK soft_isEnqueued=false          <-- WRONG   CK soft_isEnqueued=true
  CK phantom_isEnqueued=false       <-- WRONG   CK phantom_isEnqueued=true
```

`native_ref_is_enqueued` tested the queue slot against the synthetic sentinel
`Value::Int(1)` **and nothing else**. On the real JDK layout that slot is
`java.lang.ref.Reference.queue`, declared `Ljava/lang/ref/ReferenceQueue;` — so
the sentinel **cannot survive in it**: any primitive aimed at an `L` slot takes
`heap::coerce_field_value_for_slot`'s `b'L'` arm and is nulled, which is G30-1
itself. Meanwhile the writer that actually runs under `--jdk-only` is
`native_ref_enqueue`'s real-layout arm, which delegates to the JDK's own
`ReferenceQueue.enqueue` bytecode, and `enqueue0` publishes
`r.queue = ENQUEUED` — a live object. **The reader was waiting for an `Int` the
writer never writes, and could not have seen it if it did.**

---

## 1. Reachability — checked first, and both sites are live

Two consecutive lanes were handed census sites that sat behind
`use_synthetic_jdk` and could not fire at all. These do not. MEASURED,
`--dump-native-registry <FILE> --jdk-only -cp . RJdkHello` (10,691 entries):

| class | method | kind | `owns_slot` | `invocations` | real declaring method |
|---|---|---|---|---:|---|
| `java/lang/ref/ReferenceQueue` | `poll ()Ljava/lang/ref/Reference;` | bridge | **true** | **5** | loaded, declared, has_code |
| `java/util/Properties` | `getProperty (Ljava/lang/String;)Ljava/lang/String;` | bridge | **true** | **37** | loaded, declared, has_code |
| `java/lang/ref/Reference` | `isEnqueued ()Z` | bridge | **true** | 0 | loaded, declared, has_code |

Neither registrar is gated on `use_synthetic_jdk` — grep is empty in both
files, and `vm_init.rs`'s two `register_properties_sidetable` call sites and
`lib.rs`'s two `register_reference_natives` call sites are on the ordinary
path. `isEnqueued`'s `invocations = 0` in that particular run proves nothing
(the brief says so, and four bypass families are known); it is reached, and §4
reaches it.

## 2. The sweep, and the read/store split the backtraces gave up

MEASURED: **19 vectors**, `--jdk-only`, `CRATONVM_DBG_COERCION=1`,
**1,648 events** — 1,362 reads, 235 stores, 51 unattributable (backtrace
truncated). Attributed by nearest `native_builtins` frame; direction taken from
the `VmHeap::get_field_as` / `set_field_as` frame directly above it.

| n | site | direction | species | desc | value |
|---:|---|---|---|---|---|
| **793** | `reference::native_rq_poll::closure$0` @ `reference.rs:758` | **read** | primitive-into-reference | `L` | `Int(0)` |
| **337** | `properties_sidetable::props_defaults` @ `properties_sidetable.rs:1680` | **read** | primitive-into-reference | `L` | `Int(0)` |
| 205 | `jca::provider_chain::make_provider` @ `provider_chain.rs:317` | store | pointer-into-primitive | `I` | `Object(Some(..))` |
| 56 | `lang_invoke::varhandle_compare_and_set` @ `lang_invoke.rs:4536` | read | primitive-into-reference | `L` | `Int(0)` |
| 54 | `native_object_clone` @ `lib.rs:26586` | read | primitive-into-reference | `L` | `Int(0)` |
| 22 | `unsafe_natives_ext::native_unsafe_cas_object` @ `:2426` | read | primitive-into-reference | `L` | `Int(0)` |
| 21 | `unsafe_natives_ext::native_unsafe_get_object` @ `:2677` | read | primitive-into-reference | `L`/`[` | `Int(0)` |
| **3** | `reference::native_rq_poll::closure$0` @ `reference.rs:758` | **read** | **pointer-into-primitive** | **`J`** | `Object(Some(..))` |
| … | 27 further sites, each ≤ 12 | | | | |

My two sites are **1,133 of 1,648 — 68.8% of the whole population, and every
one of them is a read.** G45-1's 670/245 reproduce as 793/337 because this
sweep includes `RJdkNet`, `RJdkReflect` and `RSerial`, which G45-1's did not.

Per-vector, my two sites:

| vector | `native_rq_poll` | `props_defaults` | vector result |
|---|---:|---:|---|
| RJdkNet | 179 | 30 | PASS (81 checks) |
| RJdkReflect | 151 | 30 | PASS (67) |
| RSerial | 150 | 30 | PASS (21) |
| RJdkLogging | 111 | 30 | PASS (79) |
| RJdkProcess | 54 | 30 | PASS (55) |
| RJdkFormatLocale | 53 | 16 | PASS (20) |
| RStrings | 53 | 16 | PASS (46) |
| RJdkX509Intercept | 10 | 16 | PASS (26) |
| RCrypto | 8 | 16 | PASS (57) |
| RJdkExecutors | 7 | 14 | PASS (69) |
| RJdkCollections | 5 | 16 | PASS (69) |
| RJdkHello | 5 | 17 | PASS (41) |
| RJdkSecurity | 5 | 30 | PASS (153) |
| RSslNullSession | 5 | 30 | PASS (89) |
| RJdkServices | 0 | 16 | PASS (19) |
| RCollections, RMapGcStress, RJdkEnvMap, RClassUnloadSweep | 0 | 0 | PASS |

`RCollections` and `RMapGcStress` produce **zero** coercion events, confirming
G45-1 §3's observation that the GC-stress family is not where this lives.

### 2.1 Why every one of them is `Int(0)` — the root cause is not in these files

`gen_heap.rs::read_slot`'s R-niche rule: after `Value::Object` gained a
`NonNull` niche, **the all-zero bit pattern decodes as `Value::Int(0)`, not
`Value::Object(None)`.** `alloc_object_with_descriptors` therefore writes an
explicit `Object(None)` into reference slots — but the interpreter's `new` does
not use it. `vm/src/runtime/interpreter/gc_and_alloc.rs::init_primitive_fields`
walks the hierarchy and writes typed zeroes for `I/B/C/S/Z/J/F/D`, and for
reference descriptors does:

```rust
_ => None, // Reference types: already Object(None) from zero memory
```

That comment is **stale** — it predates the niche. Every reference field of
every `new`-allocated object is left as raw zero, which reads back as
`Int(0)`, and the first descriptor-aware read of any such field that was never
assigned fires `primitive-into-reference`. That single line is the origin of
**at least 1,290 of this sweep's 1,362 reads**, including all 1,133 of mine. It
is not in either of my files; see NOMINATION N1.

## 3. Site table

| # | class | slot | real descriptor | what is written / read | what happens now | verdict |
|---|---|---:|---|---|---|---|
| 1 | `java.lang.ref.ReferenceQueue` | 0 = `head` | `Ljava/lang/ref/Reference;` | **read** by `native_rq_poll`; never written until an enqueue publishes a real `Reference` | slot holds raw `Int(0)`; coercion answers `null`; poll returns "queue empty" | **BENIGN — no change** |
| 2 | `java.util.Properties` | 8 = `defaults` | `Ljava/util/Properties;` | **read** by `props_defaults` on every `getProperty` miss; written only by the JDK's own `Properties(Properties)` `putfield`, which does not coerce | slot holds raw `Int(0)` when there is no parent; coercion answers `null`; lookup correctly stops | **BENIGN — no change** |
| 3 | `java.lang.ref.Reference` | 1 = `queue` | `Ljava/lang/ref/ReferenceQueue;` | **read** by `native_ref_is_enqueued`, which compared it to `Int(1)` | the JDK's `enqueue0` stored `ENQUEUED` here; the comparison never matched, so `isEnqueued()` was **always false** | **DEFECT — FIXED**, §4 |
| 4 | `java.lang.ref.ReferenceQueue` | 1 = `queueLength` | `J` | **read** by `native_rq_poll`/`native_ref_enqueue` for the width-preserving count; 3 MEASURED reads found an **object** there | coerced to `Long(addr)`; the count is then written back over it | **OPEN — not changed**, §5 |

Slots 0/8 confirmed against `javap -p java.util.Hashtable`,
`javap -p java.util.Properties`, `javap -p java.lang.ref.ReferenceQueue`,
`javap -p java.lang.ref.Reference` on HotSpot 25.0.3+9-LTS, and cross-checked
against `native-collections`' own `define_field(PROPERTIES_CID, "defaults", 8)`
and `CRATONVM_DBG_LAYOUT=1` (`java/lang/ref/ReferenceQueue cid=46 body=24
refs=2 fields=3`, i.e. `head`/`queueLength`/`lock`).

### 3.1 Instrument evidence, before

Row 1 — `CRATONVM_DBG_OVERLAY=1`, `RJdkProcess`, verbatim, 58 occurrences:

```
[OVERLAY] suspect native get_field [cross-type]: class=java/lang/ref/ReferenceQueue
  slot=0 value=Int(0) real_field_desc='L' model=_f0:Ljava/lang/Object;
  real=head:Ljava/lang/ref/Reference; verdict=ok (overlay layout bound to a real JDK class)
```

This is the whole of row 1: the overlay guard, which knows the class and the
slot, has already adjudicated it `ok`.

Row 2 — `CRATONVM_DBG_COERCION=1`, `RJdkHello`, the backtrace that names the
site and the direction in one place:

```
species="primitive-into-reference" access="unattributed" descriptor=L value=Int(0) class_id=-1 index=-1
   6: cratonvm_gc::vm_heap::VmHeap::get_field_as            <-- READ, not a store
   7: cratonvm_vm::vm::vm_exec::impl$14::get_field
   8: cratonvm_native_builtins::properties_sidetable::props_defaults
             at ...\native-builtins\src\properties_sidetable.rs:1680
   9: cratonvm_native_builtins::properties_sidetable::native_properties_get_property_1
```

**A caution for the next lane: `reference.rs:758` in the census is the
*closure*, not that statement.** In release builds the whole of
`native_rq_poll::closure$0` collapses onto its first field access, which is why
the `descriptor=J` reads of row 4 also report line 758 even though line 758
reads `RQ_FIELD_HEAD`. Do not read a census line number as a statement.

### 3.2 Differentials — the load-bearing evidence

Rows 1 and 2 are settled by behaviour, not by counting events. Each probe was
compiled once and run under both `java` and `cratonvm --jdk-only`.

`TProps` + `TProps2` — **22 checks, identical**, including every part of the
`defaults` contract that a broken fall-through would break: single-level
fall-through, own-entry precedence, a three-level chain, `getProperty(k, def)`
preferring `defaults` over the supplied default, a `Properties` **subclass**
constructed with `super(defaults)`, `propertyNames()` and
`stringPropertyNames()` unioning the parent, `size()`/`containsKey()`/`get()`
**not** seeing the parent, and the JDK's own oddity that a **non-String own
value falls through to `defaults`** (`p.put("k", Integer)` +
`d.setProperty("k", "…")` must answer the *defaults* string). All 22 agree.

`TRefQ` — **13 checks, identical**: empty poll, explicit enqueue, single
delivery, re-enqueue refused, two references both delivered exactly once,
`remove(timeout)` returning null, `refersTo(null)`, and enqueue-after-clear.

`TCleaner` — **4 checks, identical**: 200 `Cleaner.register` cleanups, 50
explicit enqueues polled back as exactly 50.

**If either slot were losing a value, these would not agree.** They do.

## 4. The fix — `native_ref_is_enqueued`

`reference.rs` gains one helper and one rewritten body.

```rust
fn reference_queue_enqueued_sentinel(ctx: &dyn NativeContext) -> Option<ObjectRef> {
    let class_id = ctx.class_id_by_name("java/lang/ref/ReferenceQueue")?;
    let index = ctx.static_field_index_by_name(class_id, "ENQUEUED")?;
    match ctx.get_static_field(class_id, index) { Value::Object(Some(s)) => Some(s), _ => None }
}
```

and the reader now accepts **both** shapes:

```rust
let enqueued = match ctx.get_field(this, REF_FIELD_QUEUE) {
    Value::Int(1) => true,                                          // synthetic two-slot shape
    Value::Object(Some(q)) => reference_queue_enqueued_sentinel(ctx) == Some(q), // real JDK
    _ => false,                                                     // no queue, or already polled
};
```

Four properties of this change, each deliberate:

1. **It is a correct value, not a refusal.** The new arm can only turn a
   `false` into a `true`, and only when the queue slot holds the exact object
   the JDK's own `isEnqueued` compares against. `Object(None)` and every other
   value still answer `false`.
2. **The synthetic path is untouched.** `vm/src/vm/tests.rs`'s
   `s28_is_enqueued_lifecycle` builds `ClassId(0)` objects with no `L`
   descriptor on slot 1, drives `native_ref_enqueue`'s non-real-layout arm, and
   asserts `isEnqueued == true` off the `Int(1)` sentinel. That arm is kept
   first and unchanged, so that test stays green. The two arms cannot collide:
   `Int(1)` is unreachable on a real layout precisely *because* of the G30-1
   coercion.
3. **No GC-capable call is added.** The sentinel is resolved with
   `class_id_by_name`, **not** `ensure_class_initialized` — the only caller
   arrives holding a live `ReferenceQueue` out of the slot, which already
   proves the class is initialised. So nothing between reading the slot and
   comparing it can relocate the object, and no pin is needed. The helper's doc
   comment says this, because an `ensure_class_initialized` added later would
   silently invalidate it.
4. **`ReferenceQueue` absent ⇒ `false`, never a throw.** No class means no
   queue object means nothing enqueued.

`native_rq_poll` was **deliberately not changed** even though it writes
`queue = Object(None)` where the JDK's `poll0` writes `NULL_QUEUE`, and
`next = Object(None)` where `poll0` self-links `next = r`. Every reachable
reader was checked and the answers are identical: `isEnqueued` is `false` for
both `null` and `NULL_QUEUE` (neither is `ENQUEUED`); `native_ref_enqueue` owns
the `enqueue` slot and returns `false` for a null queue, which is exactly what
`Null.enqueue`/`enqueue0` return for `NULL_QUEUE`; and
`ReferenceQueue.forEach` only *compares* `r.queue`, never dereferences it.
Writing the real sentinels would be more faithful, but it is a change to the
single hottest reference native in the VM with no observable gain, and this
lane cannot build to prove it harmless. See NOMINATION N3.

## 5. What this lane could not settle

**Three `pointer-into-primitive` reads at descriptor `J` inside
`native_rq_poll`** (`RJdkProcess` only, 3 of that vector's 109 events;
`value=Object(Some(ptr))`). The only `J` slot any of that closure's reads can
land on is `RQ_FIELD_SIZE = 1`, which
`CRATONVM_DBG_LAYOUT=1` and `javap` agree is
`java.lang.ref.ReferenceQueue.queueLength : J`. So an **object** is sitting in
`queueLength`, the coercion turns it into `Long(addr)`, and
`native_rq_poll`'s width-preserving `new_size` is then written back **over
it**. No writer of an object into that slot was found: the only writers are
`native_rq_init` (`Int(0)`), `native_rq_poll`/`native_ref_enqueue`
(`Int`/`Long` arithmetic) and `gc_and_alloc.rs:2524` (likewise), and
`init_primitive_fields` correctly seeds `J` with `Long(0)`.

`CRATONVM_DBG_OVERLAY=1` on the same vector reports **no** slot-1 event, but it
does report the thing most likely to be behind this:

```
[OVERLAY-LAYOUT] java/lang/ref/ReferenceQueue — model has 2 slot(s), 1 disagree with the loaded image
[OVERLAY-LAYOUT]   slot  1 TYPE model=_f1:Ljava/lang/Object; real=queueLength:J
```

The synthetic model calls slot 1 a **reference**; the loaded image calls it a
**long**. A writer driven by the model would legitimately put an object there.
That is a `classloading` disagreement, not a `reference.rs` one — NOMINATION
N4. It is left alone because `RJdkProcess` passes, the `TCleaner` and `TRefQ`
differentials are identical to HotSpot, and 3 events in 1 of 19 vectors is not
a mandate to edit the VM's hottest reference native blind.

**No "after" binary exists.** The rule against `cargo build` means §6's
after-line is composed from the unchanged code path plus MEASURED values. The
`isEnqueued` differential in §0 is the falsifiable claim: re-running `TEnq`
(`scratchpad/g49/j/TEnq.java`) on a binary containing this change must turn
three `false`s into `true`s and change nothing else.

**Whether `props_defaults` should stop firing the guard at all.** It cannot
today: `NativeContext` exposes no non-coercing field read, so the 337 events
are unavoidable from inside `properties_sidetable.rs`. Fixing N1 removes them
at the source, along with ~1,290 others.

## 6. After-line (PREDICTED)

Rows 1 and 2 are expected to be **unchanged** — they are correct, and nothing
in this lane's diff touches either reader. The measurable delta is behavioural,
not instrumental:

```
$ cratonvm --jdk-only -cp scratchpad/g49/j TEnq
CK afterEnq_isEnqueued=true      (was false)
CK soft_isEnqueued=true          (was false)
CK phantom_isEnqueued=true       (was false)
```

Whoever runs it first should mark §0 MEASURED.

## 7. Tests

`native-builtins/src/reference.rs`, existing `mod tests` (6 new):

- `the_real_jdk_enqueued_sentinel_reads_as_enqueued` — **the pin.** A
  `Reference` whose `queue` slot holds `ReferenceQueue.ENQUEUED` is enqueued.
  Reverting to the `Int(1)`-only test makes this red.
- `a_reference_still_holding_its_own_queue_is_not_enqueued` — the other half,
  so the pin cannot be satisfied by a function that always answers `true`.
  This is the state every `new WeakReference(o, q)` starts in.
- `the_synthetic_int_sentinel_still_reads_as_enqueued` — guards
  `s28_is_enqueued_lifecycle`'s shape from another crate.
- `queueless_detached_and_never_written_all_read_as_not_enqueued` — covers
  `Object(None)` (polled), never-had-a-queue, and the raw `Int(0)` a
  never-written slot decodes to under the R-niche rule.
- `an_unresolvable_sentinel_answers_not_enqueued_rather_than_failing` — pins
  the licence for skipping `ensure_class_initialized`.
- `a_missing_receiver_answers_not_enqueued` — `isEnqueued` is registered for
  four classes and the arg shape is not guaranteed.

`native-builtins/src/properties_sidetable.rs`, existing `mod tests` (4 new) —
these encode the §3 adjudication so the 337-event cluster cannot be "fixed"
into a regression later:

- `a_never_written_defaults_slot_means_no_defaults_not_a_lost_parent` — **the
  337-event shape**, asserted to still read `Int(0)` in the fixture before the
  subject runs, so the test cannot pass against a null.
- `a_real_parent_is_found_so_the_fall_through_chain_survives` — the falsifier;
  this is precisely the failure G45-1 suspected and it is red the moment
  `props_defaults` starts refusing.
- `an_explicitly_null_defaults_slot_is_also_no_defaults`.
- `an_unresolvable_defaults_field_answers_none_rather_than_slot_zero` — slot 0
  on the flat layout is `Hashtable.table`, an `Entry[]`; answering with it
  would hand `getProperty` an array to recurse into.

Both files use the existing `crate::test_utils::mock_ctx` harness and
`set_declared_fields`/`set_static_field`, the same way `lang_class.rs`'s
`mod tests` does. `MockNativeContext::set_field` is raw, so each fixture plants
the exact `Value` the production heap would hold.

**Not run:** this lane may not invoke `cargo build`/`check`/`test`.

## 8. Formatting and hygiene

`rustfmt --edition 2021 --check` was run against `git show HEAD:<file>` to
establish the pre-existing drift, then against the working tree:

| file | hunks at HEAD | hunks now |
|---|---:|---:|
| `native-builtins/src/reference.rs` | 5 | **5** |
| `native-builtins/src/properties_sidetable.rs` | 9 | **9** |

**No new formatting hunk.** (Two of this lane's edits were reflowed by hand to
keep that true: the test-module `use` order, and one `set_field` call.) Both
files are **0 CR bytes** — byte-counted, and `git ls-files --eol` reports
`i/lf w/lf` for both.

---

## NOMINATIONS

**N1 — `init_primitive_fields` leaves reference slots as raw zero, and raw zero
is no longer `null`.** `vm/src/runtime/interpreter/gc_and_alloc.rs`, the
`_ => None, // Reference types: already Object(None) from zero memory` arm.
Its premise was true before `Value::Object` gained a `NonNull` niche and is
false now — `gen_heap.rs::read_slot`'s own doc says the all-zero pattern
decodes as `Value::Int(0)` and that "there is no *zeroed slot reads as null*
shortcut". **This is the single largest source of G30 events in the corpus:
MEASURED, at least 1,290 of this sweep's 1,362 reads, including 100% of the two
largest clusters.** The fix is one line — extend the `match` so reference and
array descriptors write an explicit `Value::Object(None)` — and it is the same
thing `alloc_object_with_descriptors` already does. Expected effect: the 793,
the 337, the 56 at `varhandle_compare_and_set`, the 54 at `native_object_clone`
and the 21 at `native_unsafe_get_object` all go to **zero**, leaving the store
population (the actual defects) alone in the census. Two cautions for whoever
takes it: (a) it costs one extra slot write per reference field at every
allocation, on the hot path — measure; (b) `vm_exec.rs::values_equal_for_cas`
carries a "belt-and-braces" fallback written against the *old* premise
(`Object(None)` vs typed primitive zero) whose comment should be re-read at the
same time.

**N2 — the GC's own auto-enqueue writes the synthetic `Int(1)` sentinel into a
real `L` slot.** `vm/src/runtime/interpreter/gc_and_alloc.rs:2526`,
`shared.mem.heap.set_field(ref_obj, 1, Value::Int(1)); // REF_FIELD_QUEUE`.
Slot 1 of a real `java.lang.ref.Reference` is `queue :
Ljava/lang/ref/ReferenceQueue;`. That write goes through the **raw** setter so
it fires no G30 event, but every descriptor-aware *reader* of the slot nulls it
on the way out. Consequence: §4's fix repairs `isEnqueued()` for an **explicit**
`enqueue()` but **not** for a GC auto-enqueue, where the reader sees `null`
rather than the sentinel. The correct value is
`java.lang.ref.ReferenceQueue.ENQUEUED`, resolvable exactly as
`reference_queue_enqueued_sentinel` does it; the same site should probably also
stop writing raw `Int` into a slot the class declares `L`, since that is a
type-punned heap word the collector must later scan. Not this lane's file. The
identical sentinel write is duplicated at
`scratch/SpringTestCompilerAnnotation-interpreter.rs:1650`.

**N3 — `native_rq_poll` writes `null` where `poll0` writes `NULL_QUEUE`, and
`null` where `poll0` self-links `next`.** This IS my file, and I deliberately
left it: §4 records that every reachable reader gives the identical answer
either way, so the change is unobservable today and touches the VM's hottest
reference native. It stops being unobservable the moment any real
`ReferenceQueue` bytecode is allowed to run against a natively-polled
reference — `Reference.enqueue()`'s `this.queue.enqueue(this)` would NPE on the
null, and `forEach`'s self-loop terminator would misread the list. Worth doing
by whichever lane next has a binary and can run `TRefQ`/`TCleaner` on it.

**N4 — the synthetic model and the loaded image disagree about
`ReferenceQueue` slot 1.** MEASURED, `CRATONVM_DBG_OVERLAY=1`:
`model=_f1:Ljava/lang/Object; real=queueLength:J`. The model is
`classloading/src/class_manager.rs`'s `"java/util/…"`-style
`synthetic_stub_field_model`, which gives `java/lang/ref/ReferenceQueue` a run
of unnamed `Ljava/lang/Object;` slots. This is the most plausible origin of §5's
three unexplained `descriptor=J` reads holding an object. A `classloading`
change, not a `native-builtins` one.

**N5 — `provider_chain.rs:317` reproduces exactly.** 205 events here against
G45-1's 217, same species, same descriptor, same store direction, 173 of them
one repeated pointer. Carried forward unchanged; still not this lane's file.
