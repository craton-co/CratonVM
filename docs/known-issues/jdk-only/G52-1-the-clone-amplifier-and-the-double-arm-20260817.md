# G52-1 — the clone "amplifier" that normalises, and the `Double` arm that was right for a reason nobody had written down

**Status:** MEASURED / DOCUMENTED-SOURCE / TWO-ASSIGNMENTS-DECLINED-WITH-REASONS
/ ONE-SPECIES-CLOSED.
**Provenance:** every runtime number below is MEASURED on
`C:/craton/target-rel3/release/cratonvm.exe` (built from `9ae371468`, the first
binary carrying the G30 coercion instrument), `--jdk-only`, classpath
`C:/craton/CratonVM1/regression-suite/build`. `C:/craton/target-rel4` did not
exist when this lane ran, so every event line below still reports
`class_id=-1 index=-1`; where a class or slot is named it was recovered from
`CRATONVM_DBG_CLONE=1` / `CRATONVM_DBG_OVERLAY=1` on the same run and
cross-checked against `javap -p` on HotSpot 25.0.3+9-LTS. Layout claims are
MEASURED against that `javap`. This lane may not build, so nothing here claims
an "after" for a source change.

**Files this lane owns and touched:** `gc/src/heap.rs`,
`native-builtins/src/lib.rs`. Nothing else.

Continues `G30-1` (§4.4 / N6), `G43-1` (N5) and `G45-1` (§3 / N2).

---

## 0. The headline

| assignment | the standing reading | what this lane MEASURED |
|---|---|---|
| **A** `native_object_clone` — "clone re-runs the coercion per field, so it amplifies whatever the original got wrong" | 54 events in 19 vectors, 270 corpus-wide; ranked 4th in `G45-1` §3 | **It amplifies nothing.** 100% of its events are READS of a never-descriptor-initialised reference slot answering `null` — the benign `native_rq_poll` species. The clone gets a *correctly tagged* null where the original keeps a mis-tagged raw zero. It NORMALISES. (§1) |
| **A**, second half | "ask why it goes through the descriptor-coercing setter rather than a raw slot copy" | Because **a raw slot copy is not expressible from a native.** `NativeContext` has no slot-INDEXED raw accessor; its only raw pair is name-keyed. Not a decision this site made. NOMINATION 1. (§1.3) |
| **B** the `b'I'` `Double` arm, `d.to_bits() as i32` | `G43-1` N5: should be `d as i32`, a real narrowing | **KEPT, and it is right for three reasons, one of which is that the proposed repair makes `G43-1`'s own case WORSE.** (§2) |
| **C** the `Float`/`ReturnAddress`-at-a-reference-slot species | `G45-1` N2: 0 / 1,339, "either genuinely empty or outside these 19 vectors" | **CLOSED AS EMPTY.** 0 / 1,977 runtime events across 26 vector-runs, 0 rows in an independent instrument, and a **static census with no producer**. `ReturnAddress` is not merely empty, it is unreachable. (§3) |

Two behaviour changes were available and both were declined, with reasons, in
§2.4 and §3.3. What changed is the record at each site and five tests that
make the reasoning falsifiable.

---

## 1. Assignment A — `native_object_clone` (`native-builtins/src/lib.rs`)

### 1.1 What it is doing, mechanically. SOURCE-VERIFIED.

```rust
let num_fields = ctx.object_num_fields(this);
let clone_ref = ctx.alloc_object(class_id, num_fields);
for i in 0..num_fields {
    let val = ctx.get_field(this, i);
    ctx.set_field(clone_ref, i, val);
}
```

`NativeContextImpl::get_field` and `::set_field` (`vm/src/vm/vm_exec.rs:11400`
and `:11437`) BOTH call `resolve_field_descriptor_byte_cached` and route
through `get_field_as` / `set_field_as`, i.e. through
`gc/src/heap.rs::coerce_field_value_for_slot`. So **every cloned field is
coerced twice**, once out of the original and once into the copy, and each
cloned field costs **two** descriptor resolutions.

That the answer is nevertheless stable rests on a property nothing in the tree
asserted before this lane: **the coercion is idempotent.** Now pinned by
`the_descriptor_coercion_is_idempotent` (`gc/src/heap.rs`), over all nine
`Value` variants × eleven descriptor bytes. The clone therefore holds exactly
the original's *coerced* contents; the second pass cannot compound the first.

### 1.2 What it does at runtime — and it is not what the brief predicted

MEASURED, `CRATONVM_DBG_COERCION=1` + `CRATONVM_DBG_CLONE=1`, `--jdk-only`.

| vector | clones performed | of those, ARRAYS | object clones | coercion events |
|---|---:|---:|---:|---:|
| `RJdkSecurity` | 4,534 | **4,533** | 1 (`java/security/Security$1`) | 3 |
| `RSerial` | 29 | 17 | 12 (all `java/lang/invoke/MemberName`) | 21 |

**The first number is the one that reframes the site.** 4,533 of
`RJdkSecurity`'s 4,534 clones take the `ObjectKind::Array` arm, which uses
`get_array_element` / `set_array_element` — **not** descriptor-aware, never
reaching the coercion helper at all. The coercion question does not touch
99.98% of what `Object.clone()` actually does in that vector.

Every one of the 24 events is the same event:

```
species="primitive-into-reference" access="unattributed" descriptor=L value=Int(0) class_id=-1 index=-1
```

Uniform `value=Int(0)`, uniform `descriptor=L`. `CRATONVM_DBG_OVERLAY=1` on the
same run names the slots:

```
[OVERLAY] suspect native get_field [cross-type]: class=java/lang/invoke/MemberName slot=4 value=Int(0) real_field_desc='L'   (×12)
[OVERLAY] suspect native get_field [cross-type]: class=java/lang/invoke/MemberName slot=5 value=Int(0) real_field_desc='L'   (× 9)
```

`javap -p java.lang.invoke.MemberName` on 25.0.3+9 gives the instance layout
`0 clazz:L, 1 name:L, 2 type:L, 3 flags:I, 4 method:LResolvedMethodName, 5
resolution:LObject`. **Slots 4 and 5 are `method` and `resolution`, both
legitimately null on an unresolved `MemberName`.** 12 + 9 = 21 = the RSerial
count exactly.

### 1.3 Read or store? The direction is provable without a rel4 binary

Both halves of the loop report `unattributed` on this binary, so the log cannot
say. The source can, and the argument is closed:

> A store event with `value=Int(0)` at `descriptor=L` would require the READ to
> have returned `Int(0)`. But `Int(0)` at `descriptor=L` is exactly the input
> that makes the read fire and answer `Object(None)`. So if a store fired, the
> read fired first and handed the store `Object(None)`, which cannot fire.
> Contradiction. **Every clone event is a read.** (Both halves use the same
> descriptor: the clone is allocated with the receiver's `class_id`.)

So the loop reads `Int(0)` out of a slot the class declares `L`, and writes
`Object(None)` into the copy. **The original keeps a mis-tagged raw zero; the
clone gets a properly tagged null.** On 100% of the measured population the
double coercion is a repair, not an amplification.

The `Int(0)` itself is the R-niche decode rule (`gc/src/heap.rs`): a slot that
was never written through a descriptor-aware or `alloc_object_with_descriptors`
path reads back `Int(0)`, bit-identical to an explicit `Value::Int(0)`. It is
the same population as `G45-1` §3's 670 `native_rq_poll` reads and the 336
`ReferenceQueue.head` reads of `G30-1` §2.1.

### 1.4 Why it is not a raw copy — and it is not because anyone decided that

`Object.clone()` is specified as a verbatim field copy, so the brief's question
is the right one. The answer is availability, not deliberation:

| `NativeContext` accessor | slot-indexed? | coerces? |
|---|---|---|
| `get_field` / `set_field` | yes | **yes** — resolves the descriptor in `NativeContextImpl` |
| `get_field_volatile` / `set_field_volatile` | yes | **yes** (`vm_exec.rs:12753`/`:12782`) |
| `compare_and_swap_field` | yes | **yes** |
| `get_field_typed` | yes | yes, with a caller-supplied descriptor |
| `get_field_by_name` / `set_field_by_name` | **no** — name-keyed | **no** — raw `heap.get_field`/`set_field` |

There is **no slot-indexed raw accessor on the trait.** The only raw pair is
name-keyed, cannot be driven from a slot index, and resolves a shadowed field
name to the wrong slot. The descriptor-aware pair was made descriptor-aware in
T10.9.E, *after* this loop was written; the loop did not opt in, the API moved
underneath it. That is NOMINATION 1, and `native-api/src/registry.rs` is not
this lane's file.

### 1.5 The half-fix that was available, evaluated and declined

`get_field_typed(this, i, d)` with a `d` the coercion table does not know falls
through `_ => value` and returns the slot **verbatim** — the descriptor is used
*only* inside the coercion (`collector.rs:450`, `gen_heap.rs:4178`: `let raw =
self.get_field(obj, index)` then coerce), so no compact-layout decode depends
on it. Replacing the read half with it would:

* save one descriptor resolution per cloned field (the production comment at
  `vm_exec.rs:4088` puts `resolve_field_descriptor_byte_cached` at 15.3% of a
  profile it was measured in), and
* leave the stored value **bit-identical**, by the idempotence theorem of §1.1.

It is declined because of what it does to the instrument. The loss would move
from the `read` column to the `store` column — and `store` is the column
`gc/src/collector.rs`'s own doc reserves for the real defect ("a read that
coerces is usually the slot repairing a never-initialised tag, a store that
coerces has destroyed something a writer meant"). It would put clone's 24
benign events in the same bucket as `provider_chain`'s 217 real ones, which is
precisely the separation `G45-1` was written to create. **A measurable
performance win that costs signal-to-noise in the only instrument pointed at
this defect is not worth taking without a profile, and this lane cannot
build one.** It is NOMINATION 2, with the trade stated so the next lane does
not have to rediscover it.

### 1.6 The hazard that WOULD be amplification, and why it has never fired

If a slot declared primitive holds a live `Object(Some(_))` — the
`pointer-into-primitive` species, 207 events in `RJdkSecurity` from
`jca/provider_chain.rs:317` — then cloning that object writes
`Int(ptr as i32)` into the copy: a truncated, stale address that no collector
will remap, and a reference silently dropped. That is real amplification and it
is the one shape worth watching for.

It has never fired here because `java.security.Provider.clone()` dispatches to
the `java/util/Hashtable.clone` bridge (`native-builtins/src/deprecated_util.rs:2178`,
`owns_slot: true`) rather than to `native_object_clone`. Recorded, not fixed —
there is nothing to fix until something measures it.

### 1.7 Reachability, first, as instructed

`--dump-native-registry`, `RJdkHello`, `--jdk-only`:

```json
{ "class": "java/lang/Object", "name": "clone",
  "descriptor": "()Ljava/lang/Object;", "kind": "bridge",
  "registered_by": "native-builtins/src/lib.rs:10363",
  "owns_slot": true,
  "real_declaring_method": {"loaded": true, "declared": true, "acc_native": true} }
```

`owns_slot: true` and the real method is loaded, declared and `ACC_NATIVE`.
Reachable. (`invocations: 0` in `RJdkHello`, which proves nothing — the same
site performs 4,534 calls in `RJdkSecurity`.)

---

## 2. Assignment B — the `b'I'` `Double` arm. KEPT, and here is the reason it deserved

`gc/src/heap.rs`, the `b'I' | b'B' | b'C' | b'S' | b'Z'` arm:

```rust
Value::Double(d) => Value::Int(d.to_bits() as i32),
```

`G43-1` N5 asked for `d as i32`. **Declined.** Three arguments, in increasing
order of decisiveness.

### 2.1 It is the untagged-compact-slot decode, not a double conversion

`coerce_field_value_by_descriptor`'s own doc declares it "a superset of
`CompactValue::decode_by_descriptor`". This is the arm that makes the claim
true. `types/src/compact_value.rs:1675` answers an integral descriptor on an
**untagged** slot with `Value::Int(self.0 as u32 as i32)` — the low 32 raw
bits. An untagged slot decoded through `to_value()` surfaces as
`Value::Double(f64::from_bits(raw))` (that is the whole reason the `b'J'` arm's
Session-93 repair, `Double(d) => Long(d.to_bits() as i64)`, exists and is
documented). So **a `Value::Double` arriving at an integral descriptor is, on
the shipped compact-layout path, a long/int BIT PATTERN and not a number.**
Taking its low half is the correct `l2i`.

### 2.2 It is forced by the `b'J'` arm — changing it resurrects Session 93 at 32 bits

`Value::Long(l)` and `Value::Double(f64::from_bits(l as u64))` are the same
untagged slot read two ways. They must agree about the slot's low 32 bits, or a
long read at `I` and the same long read at `J` contradict each other.

| `l` | `Long(l)` at `I` | `Double(from_bits(l))` at `I`, **current** | same arm under `d as i32` |
|---:|---:|---:|---:|
| 5 | `Int(5)` | `Int(5)` ✓ | `Int(0)` ✗ |
| −1 | `Int(-1)` | `Int(-1)` ✓ | `Int(0)` ✗ (NaN → 0) |
| `0x0123_4567_89AB_CDEF` | `Int(-1985229329)` | `Int(-1985229329)` ✓ | `Int(0)` ✗ |

`CompactValue::long(5)` read back at an `int` field would answer `0`. That is
`types/src/compact_value.rs:2986`'s `decode_by_descriptor_j_roundtrips_small_long`
failure mode, one width down. Pinned by
`a_double_at_an_integral_slot_decodes_like_the_untagged_long_it_usually_is`.

### 2.3 On `G43-1`'s own case the proposed repair is strictly worse

`G43-1` §5.2's argument is that `25.0f64.to_bits() as i32 == 0` kept
`java.util.Hashtable.count` at zero, `getEnumeration` early-returned, and the
corruption stayed latent — and that a fractional version would have exposed it.
Both halves are true. But run the proposed rule over the same case:

| version | `to_bits() as i32` (current) | `d as i32` (proposed) |
|---|---:|---:|
| `25.0` | `0` → `count == 0`, early return. **LATENT** | `25` → walks 25 buckets of a `table` holding a `String`. **LIVE** |
| `1.8` | `0xCCCCCCCD` = −858993459. LIVE | `1` → walks 1 bucket. LIVE |

**The numeric rule converts that record's own latent corruption into a live
one.** `G43-1`'s underlying complaint — that the blast radius depends on the
double's VALUE — is correct and is now written at the arm; but the cause is
that a `Value::Double` at an integral descriptor is *ambiguous by
construction*, and choosing the other answer does not remove the ambiguity, it
just picks the reading that does not occur on a shipped path.

(The producer is gated off at HEAD anyway: `provider_has_named_layout`,
`native-builtins/src/jca/provider_chain.rs:285`, used at `:408`, commit
`36c7d47bd`. So the `25.0 → 0` luck is no longer load-bearing there either.)

### 2.4 What DID change: the reason is now written, and one real asymmetry is pinned

Kept byte-for-byte; the arm gained the argument above, in the imperative,
naming its test. And the sweep of the whole table for mutually-inverse pairs
turned up **exactly one cell that disagrees with itself**, and it is not the
one `G43-1` pointed at:

| pair | forward | reverse | agree? |
|---|---|---|---|
| Int ↔ Long | `Int(i)=>Long(i as i64)` | `Long(l)=>Int(l as i32)` | ✓ |
| Int ↔ Float | `Int(i)=>Float(from_bits(i as u32))` | `Float(f)=>Int(f.to_bits() as i32)` | ✓ |
| Long ↔ Double | `Long(l)=>Double(from_bits(l as u64))` | `Double(d)=>Long(d.to_bits() as i64)` | ✓ |
| Long ↔ Float | `Long(l)=>Float(from_bits(l as u32))` | `Float(f)=>Long(f.to_bits() as i64)` | ✓ |
| Int ↔ Double | `Int(i)=>Double(i as f64)` | `Double(d)=>Int(d.to_bits() as i32)` | ✓ *via §2.2's slot identity* |
| **Double at `b'F'`** | `Long(l)=>Float(from_bits(l as u32))` — **bit** | `Double(d)=>Float(d as f32)` — **numeric** | **✗** |

`Long(5)` at `F` is `Float(7e-45)`; `Double(f64::from_bits(5))` at `F` is
`Float(0.0)`. Same slot, two answers.

**Not changed.** Either answer is defensible in isolation (`d as f32` is the
correct JVMS `d2f` for a genuine double; `from_bits` is the correct decode for
an untagged slot), and **MEASURED: zero `Value::Double` reached any `b'F'` slot
in this lane's 26 vector-runs**, so there is no live population to decide it
against and no run that could falsify a change. It is pinned instead, by
`the_double_and_long_arms_disagree_only_at_a_float_slot`, so it cannot drift in
silence and so whoever decides it changes a test rather than meets a surprise.

### 2.5 Why the `HashMap.table` pin is untouched

`the_hashmap_table_degrade_to_null_is_pinned` writes `Int(16)`, `Int(1)` and
`Long(64)` through the descriptor-aware setter at descriptor `[`. Those inputs
reach the `b'L' | b'['` arm, which this lane did not modify. The `b'I'` and
`b'F'` arms cannot be reached by any of them. No behaviour change anywhere in
`gc/src/heap.rs` this session: the only edits to executable code are the three
new tests and the shared `same()` helper inside `#[cfg(test)]`.

---

## 3. Assignment C — the `Float` / `ReturnAddress` species. CLOSED AS EMPTY.

`G30-1` §4.4 / N6 disclosed it; `G45-1` N2 gave it a denominator of 1,339 and
declined to close it. Three independent measurements now close it.

### 3.1 Runtime, the species counter: 0 / 1,977

| sweep | vectors | total coercion events | `primitive-into-reference-uncoerced` |
|---|---:|---:|---:|
| `G45-1` §3 | 19 | 1,339 | **0** |
| this lane | 10 | 638 | **0** |
| | 29 vector-runs | 1,977 | **0** |

This lane's 638, per vector: `RJdkSecurity` 254, `RSerial` 217, `RStrings` 69,
`RJdkCollections` 39, `RCrypto` 33, `RJdkHello` 22, `RJdkReflect` 4,
`RCollections` / `RMapGcStress` / `RMapResizeGc` 0 each. By (species,
descriptor, variant) the entire 638 is three rows: 420
`primitive-into-reference L Int`, 209 `pointer-into-primitive I Object`, 9
`primitive-into-reference [ Int`. **No `Float`, no `ReturnAddress`, and no
`Double` at any primitive descriptor.**

### 3.2 Runtime, a second and independent instrument: 0 rows

`CRATONVM_DBG_OVERLAY=1` over `RJdkSecurity` / `RSerial` / `RJdkCollections` /
`RCrypto` classifies cross-type accesses from the *layout* side rather than the
*species* side, so it is not the same measurement. 211 `[cross-type]` rows, and
**every one of them is `Int` at `L` or `[`** — 168 `ReferenceQueue#0 head`, 21
`MemberName#4/#5`, 6 `java/net/URL`, 4 `RSerial$Node`, plus singletons on
`SSLContext#1`, `X509TrustManagerImpl#0`, `PKCS12KeyStore#4`,
`X509CertImpl#3`, `java/lang/Class#0`.

### 3.3 Static: no producer exists

A statement-joining census over `native-builtins`, `native-collections`,
`native-io`, `native-api`, `native-builtins-crypto`, `native-builtins-security`,
`vm` and `jit` for any `set_field` / `set_field_as` / `set_field_by_name` /
`set_field_volatile` / `compare_and_swap_field` call whose argument list
contains `Value::Float` or `Value::ReturnAddress` finds **25 sites, of which 21
are production and 4 are tests. Every one of the 21 targets a slot whose real
JDK-25 descriptor is `F`**, resolved against `javap -p` on 25.0.3+9:

| n | target | real field | ✓ |
|---:|---|---|---|
| 6 | `HashMap`/`Hashtable`/`Properties` `loadFactor` (`apps_h2.rs:442`, `lang_system.rs:4141`, `lib.rs:32132`, `native-collections` ×3, incl. `set_field(props, 3, ..)`) | `Hashtable` slot 3 = `loadFactor:F` | ✓ |
| 5 | `java/lang/Float` boxes (`lang_math.rs:6478`/`:6998`, `phases_early.rs:24633`, `lib.rs:42588`, `xml_json.rs:3193`) | slot 0 = `value:F` | ✓ |
| 4 | `CharsetEncoder`/`CharsetDecoder` slots 1,2 (`charset_buffers.rs:531/532/551/552`) | slots 1,2 = `averageBytesPerChar:F` / `maxBytesPerChar:F` (and the `Decoder` twin) | ✓ |
| 2 | `lucene_es.rs:1258/1299` — app-class `score` / `thresholdScore`, and `set_field_by_name` is raw anyway | | ✓ |
| 4 | `jni.rs:3396` `SetFloatField`, `vm_exec.rs:22400`, and the JIT/interp float boxes | | ✓ |

**The species has no producer in this tree.** Not "none observed" — none
written.

`ReturnAddress` is stronger than empty, it is **unreachable**: the only
constructor outside tests is `vm/src/runtime/interpreter/opcodes.rs:4009`/`:4016`
(`jsr` / `jsr_w`), those opcodes are illegal in class files of version ≥ 51,
`--jdk-only` runs JDK 25 (v69) class files, and even a hypothetical one could
reach a slot only through the interpreter's `putfield`, which does not coerce.

### 3.4 So why was the hole not closed?

Because closing it (nulling `Float` like `Double`) is now provably **safe and
provably unverifiable at the same time.** An empty population means no run in
this corpus can distinguish the changed VM from the unchanged one. `G30-1` §4.2
and `G45-1` §4 both earned their "no behaviour change" by exhibiting a
measurement; a change that no measurement can see does not get that, and this
lane cannot build one either.

The condition that would justify taking it is now exact and cheap to test: **a
non-zero `primitive-into-reference-uncoerced` count from any vector.** That
condition is written at the arm, next to the three measurements above, so the
next lane inherits the denominator instead of the question.

---

## 4. Files this lane touched

### `gc/src/heap.rs` — no behaviour change

* The `b'I' | b'B' | b'C' | b'S' | b'Z'` arm's `Double` case: unchanged value,
  §2.1–§2.3's argument written at the arm and naming its test.
* The `b'F'` arm's `Double` case: unchanged value, §2.4's asymmetry written at
  the arm with its measurement and its pin.
* The `b'L' | b'['` arm's `Float`/`ReturnAddress` case: unchanged value, §3's
  three measurements and the reopen condition written at the arm.
* Three tests, plus a shared bit-exact `same()` comparator (`PartialEq` on
  floats says a NaN is not itself, and several arms legitimately produce one):
  * `a_double_at_an_integral_slot_decodes_like_the_untagged_long_it_usually_is`
  * `the_double_and_long_arms_disagree_only_at_a_float_slot`
  * `the_descriptor_coercion_is_idempotent`

  All three take `g30_lock()`. They fire losses, so without it they would make
  the existing exact-delta tests flaky; none of them asserts a counter value.

### `native-builtins/src/lib.rs` — no behaviour change

* `native_object_clone`'s `ObjectKind::Object` arm: §1's account written at the
  loop — the double coercion, why a raw copy is not expressible, the
  idempotence theorem it rests on, the per-vector measurements, and §1.6's
  hazard.
* `mod g52_object_clone_tests`, five tests. `MockNativeContext`'s field
  accessors are RAW (the descriptor resolution lives in `NativeContextImpl`,
  and this crate cannot start a VM), so what is pinned here is the half the
  mock can see and the half a refactor is most likely to break: totality,
  fidelity, independence, and that the array arm — 4,533 of `RJdkSecurity`'s
  4,534 clones — stays on the array accessors and never enters the field path.
  * `object_clone_copies_every_slot_verbatim_through_a_raw_accessor_pair`
  * `writing_the_clone_does_not_move_the_receiver`
  * `the_array_arm_copies_elements_and_keeps_the_element_type`
  * `a_reference_array_clones_as_a_reference_array`
  * `clone_on_null_is_a_null_pointer_exception`

`rustfmt --edition 2021 --check` introduces no diff in any region this lane
edited. (Both files carry pre-existing drift elsewhere: `heap.rs:294` and
`:636`, and a large amount in `lib.rs` — none of it in a hunk touched here.)

---

## 5. NOMINATIONS

**N1 — `native-api/src/registry.rs`: give `NativeContext` a slot-INDEXED raw
accessor pair.** The single highest-value item in this record. Today the only
raw pair is name-keyed, which is why `Object.clone()` — a specified verbatim
field copy — cannot be one, and why the coercion at `lib.rs`'s clone loop is
availability rather than intent (§1.4). A `get_field_raw(obj, index)` /
`set_field_raw(obj, index, value)` on `NativeHeapAccess`, with
`NativeContextImpl` implementing them as the existing `heap.get_field` /
`heap.set_field` it already uses for the name-keyed pair, would make a verbatim
clone a two-line change and would give every other native that means "copy this
slot" a way to say so. Cost to this lane: not its file.

**N2 — the half-fix at `native_object_clone`, with its trade stated.** Replacing
the READ half with `get_field_typed(this, i, <descriptor the table does not
know>)` saves one descriptor resolution per cloned field and leaves the stored
value bit-identical (§1.5). It is declined here because it migrates 24 benign
events from the instrument's `read` column to its `store` column, undoing part
of what `G45-1` built. Worth taking **only** together with N1 (which makes the
descriptor trick unnecessary) or with a profile that shows the resolution
matters on a real workload. Do not take it for tidiness.

**N3 — `gc/src/gen_heap.rs`, `gc/src/collector.rs`: the `access` column is
already right, and it is now load-bearing for a second reason.** §1.3 had to
prove read-vs-store by contradiction because this lane's binary predates
`cb2ade4fd`. Once a rel4-or-later binary exists, `access="read"` on all 24
clone events is a one-command confirmation:
`CRATONVM_DBG_COERCION=1 cratonvm --jdk-only -cp . RSerial 2>&1 | grep native_object_clone -B1`.
Someone should run it and mark §1.3 MEASURED.

**N4 — `java/util/Hashtable.clone` (`native-builtins/src/deprecated_util.rs:2178`)
is unexamined and is the one clone bridge that CAN reach §1.6's hazard.**
`Provider.clone()` dispatches there, and `Provider` is the class carrying 207
`pointer-into-primitive` events. Whatever that bridge does with slots 0–4 of a
`Hashtable` is the question §1.6 could not ask of `native_object_clone`. Not
this lane's file.

**N5 — `native-builtins/src/properties_sidetable.rs` (~`:1680`), still the
second-largest cluster and still uninvestigated.** 245 events in `G45-1` §3; 30
per vector in three of this lane's ten. Carried forward from `G45-1` N4
unchanged, and reinforced: it is the largest cluster whose benignity nobody has
argued. `props_defaults` reading `Properties.defaults` should find a
`Properties` or `null`, not an `Int`.

**Carried forward unchanged:** `G45-1` N3 (resolve `class_id` to a name inside
the `#[cold]` reporter — a `gc/src/heap.rs` change this lane deliberately did
not take, because it is orthogonal to both assignments and would have put an
unrelated edit in the same diff as a declined behaviour change). `G30-1` N5
(`ArrayList#1 elementData`, 62 sites), N7 (`Thread#4 contextClassLoader`), N8
(the `null-into-primitive` direction, 26 sites).

---

## 6. Verification

Re-run on `C:/craton/target-rel3/release/cratonvm.exe`, `--jdk-only`, classpath
`regression-suite/build`, no per-class flags required by
`harness-guard.sh::class_cv_args` for any of the ten. All ten are **PASS**
(`rc=0` and a `PASS <Class>` line), which establishes that the before-state
this lane measured against is green:

| vector | rc | `PASS` line | coercion events under `CRATONVM_DBG_COERCION=1` |
|---|---:|---|---:|
| `RJdkHello` | 0 | ✓ | 22 |
| `RCollections` | 0 | ✓ | 0 |
| `RStrings` | 0 | ✓ | 69 |
| `RJdkCollections` | 0 | ✓ | 39 |
| `RMapGcStress` | 0 | ✓ | 0 |
| `RMapResizeGc` | 0 | ✓ | 0 |
| `RJdkSecurity` | 0 | ✓ | 254 |
| `RCrypto` | 0 | ✓ | 33 |
| `RSerial` | 0 | ✓ | 217 |
| `RJdkReflect` | 0 | ✓ | 4 |

The three zeroes corroborate `G45-1` §3's observation that the population is
concentrated in JDK-boot-heavy and security/net vectors and is absent from the
GC-stress family — the vectors that exercise the field-write path hardest are
the ones that produce no events at all.

**No "after" line was executed.** This lane may not build, so no binary contains
these edits. The claim being made is narrower than usual and correspondingly
easier to check: **no executable code outside `#[cfg(test)]` changed in either
file.** Every edit is a comment or a test. A reviewer can confirm that from the
diff alone, without running anything.

## 7. What this lane could not settle

* **Whether the `b'F'` `Double` arm should be numeric or a bit decode** (§2.4).
  It needs a live `Value::Double` at a float slot to decide against, and there
  are none in 26 vector-runs. Pinned, not resolved.
* **Whether the clone half-fix is worth its cost to the instrument** (§1.5,
  N2). That is a profiling question and this lane cannot profile.
* **`java/security/Security$1`'s three events.** The class is an anonymous
  inner class with no `javap` entry available to this lane, so its slots were
  not named the way `MemberName`'s were. The species and value (`Int(0)` at
  `L`) are identical to the 21 that were named, so it is almost certainly the
  same never-initialised-slot population — but "almost certainly" is not
  MEASURED and is not written as such.
* **The corpus-wide figures were not reproduced.** `G45-1`'s 5,431-across-63
  and the 270 corpus-wide clone count come from earlier lanes' 100-vector
  sweeps. This lane swept 10 vectors and 638 events. Where the two overlap they
  agree (`provider_chain` 207 in `RJdkSecurity`, exactly as `G45-1` §3
  reports); the clone number here is a floor for the corpus, not a correction
  of it.
