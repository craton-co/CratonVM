# `get_field_by_name` is not descriptor-aware

**Status:** Partial — the dangerous call sites are mitigated; the accessor
itself is unchanged.

## What is true today

`NativeContextImpl::get_field_by_name` (`vm/src/vm/vm_exec.rs`) still performs
the raw read — it resolves a field index in the hierarchy and calls
`heap.get_field(obj, index)` with **no descriptor decode**. The one-line fix
(routing through the descriptor-aware `self.get_field(obj, index)`) is not
applied, and there is no `get_field_by_name_desc` on `NativeContext`.

What *has* landed is a descriptor-safe reader module,
`native-builtins/src/field_read.rs`, used at roughly 22 call sites — the ones
where a wrong answer had security or correctness consequences. There remain
2,000+ `get_field_by_name` uses across the tree.

**Get the failure direction right — it is the opposite of the folklore.**

| Situation | by-name reader answers | by-index reader answers |
|---|---|---|
| field is **absent** | `Value::Object(None)` | — |
| field is **present but unwritten**, reference slot, tagged layout | `Value::Int(0)` | `Value::Object(None)` |

So "answers `Int(0)` for an absent field" is wrong twice over: an absent field
gives `Object(None)`, and the `Int(0)` case is a *present* one. Records
elsewhere in the tree took the loose phrasing literally and built layout
discriminators on it. The only absent-vs-null oracle available is
`resolve_field_index_by_class_id(..).is_some()`.

This is the defect class behind two fixed bugs: a BouncyCastle stream-cipher
round count read out of a reference slot (recovering a key), and
`Enum.toString` returning a primitive from a `()Ljava/lang/String;` method.

**A by-name resolution returns the most-derived declaration.** Fixing a
`java.lang.Enum` read by resolving `"name"` on the *receiver's* class is
therefore not a fix: an enum may declare its own field called `name`, and then
`Enum.name()` answers the field's value instead of the constant's name, which
makes `Enum.valueOf` throw `No enum constant`. That attempted fix was reverted.

## 1. The two readers disagree

`NativeContext` has two ways to read an instance field, and they do not answer
the same question about the same slot of the same object.

**By index** — `vm/src/vm/vm_exec.rs:8605`:

```rust
fn get_field(&self, obj: ObjectRef, index: usize) -> Value {
    let class_id = self.shared.mem.heap.class_id_of(obj);
    match resolve_field_descriptor_byte_cached(self.shared, class_id, index) {
        Some(desc) => self.shared.mem.heap.get_field_as(obj, index, desc),
        None => self.shared.mem.heap.get_field(obj, index),
    }
}
```

`get_field_as` (`gc/src/heap.rs:691`) is `coerce_field_value_by_descriptor`
(`gc/src/heap.rs:1444`) applied to the raw slot. So a `L…;`/`[…`-descriptor
field **always** surfaces as `Value::Object(..)`, an `I` field always as
`Value::Int`, and so on.

**By name** — `vm/src/vm/vm_exec.rs:8735`:

```rust
fn get_field_by_name(&self, obj: ObjectRef, field_name: &str) -> Value {
    let class_id = self.shared.mem.heap.class_id_of(obj);
    let cm = self.shared.classes.class_manager.read();
    if let Some(index) = resolve_field_index_in_hierarchy(class_id, field_name, &cm.class_store) {
        self.shared.mem.heap.get_field(obj, index)   // RAW — no descriptor decode
    } else {
        Value::Object(None)
    }
}
```

Both resolve the *same* slot: `resolve_field_index_in_hierarchy`
(vm_exec.rs:2527) is `resolve_field_index_in_hierarchy_desc(.., None, ..)`
(vm_exec.rs:2551), which is exactly the name-only lookup. **The only difference
is the descriptor decode** — plus the fact that the by-name reader has a third
outcome (the field does not exist) that it folds into a value.

### Why the raw read of an unwritten reference slot is `Int(0)`

`gc/src/heap.rs:398-411` states it directly: after `Value::Object` became
`Option<ObjectRef>` with a `NonNull` niche, the all-zero 16-byte slot that
`alloc_zeroed` leaves decodes as `Value::Int(0)` — discriminant 0 — **not**
`Value::Object(None)` (discriminant 4). A zeroed slot and a slot explicitly
written `Int(0)` are bit-indistinguishable, so `read_slot` cannot recover the
distinction; the allocator has to write the typed default explicitly.

`alloc_object_with_descriptors` does exactly that (heap.rs:391, gen_heap.rs:1354,
g1.rs:6818, zgc.rs:1636) — every reference slot gets an explicit
`Value::Object(None)`. But the path natives actually take does not:
`NativeContext::new_object` / `new_object_initialized` (vm_exec.rs:7932, :7956)
allocate raw and then call `init_primitive_fields`
(`vm/src/runtime/interpreter.rs:2798`), which writes a typed zero for every
*primitive* field and deliberately skips reference fields on this comment
(interpreter.rs:2815):

```rust
_ => None, // Reference types: already Object(None) from zero memory
```

That comment is **stale** — the niche change made it false, and heap.rs:398-411
is the note explaining why. This is the root of the whole family.

### Layout caveat

The divergence applies to the **tagged 16-byte slot** layout. For a *compact*
object layout, `read_compact_field` (`types/src/field_layout.rs:772`) is
storage-kind-aware and already answers `Value::Object(None)` for a zeroed
`FieldStorageKind::Reference` cell. So the bug is **conditional on layout**:
testing one class does not generalize to another. Treat "it worked when I tried
it" as no evidence.

---

## 2. Semantics table

For an instance field of `this`, comparing `ctx.get_field_by_name(this, name)`
against `ctx.get_field(this, resolved_index)`:

| declared descriptor | slot state | by-name returns | by-index returns | agree? |
|---|---|---|---|---|
| `I` `B` `C` `S` `Z` | unwritten (tagged layout) | `Int(0)` | `Int(0)` | ✅ |
| `J` | unwritten | `Long(0)` | `Long(0)` | ✅ |
| `F` | unwritten | `Float(0.0)` | `Float(0.0)` | ✅ |
| `D` | unwritten | `Double(0.0)` | `Double(0.0)` | ✅ |
| `L…;` / `[…` | unwritten (tagged layout) | **`Int(0)`** | **`Object(None)`** | ❌ |
| `L…;` / `[…` | unwritten (compact layout) | `Object(None)` | `Object(None)` | ✅ |
| `L…;` / `[…` | written null | `Object(None)` | `Object(None)` | ✅ |
| `L…;` / `[…` | written non-null | `Object(Some(o))` | `Object(Some(o))` | ✅ |
| any | tag drift (a native wrote the wrong `Value` variant) | the drifted tag, verbatim | normalized to the declared type | ❌ |
| **field not declared** | — | `Object(None)` | *(index does not resolve; caller sees `None`)* | ❌ |

Three consequences, each of which has cost a real bug:

1. `matches!(get_field_by_name(..), Value::Object(None))` **is not a null
   test.** It is *false* for an unwritten reference field and *true* for a field
   the class does not declare at all.
2. `match get_field_by_name(..) { Value::Int(v) => v, _ => DEFAULT }` **does not
   default when the field is missing-or-unset.** The `Int(0)` from an unwritten
   reference slot — and the genuine `Int(0)` from an unwritten primitive slot —
   is taken by the *first* arm. The `_ => DEFAULT` fallback is only reachable
   when the field does not resolve at all. If `DEFAULT` is the safe value, it is
   unreachable for exactly the input that needs it.
3. Returning a by-name read straight out of a native whose registered descriptor
   is a reference type can hand `Value::Int(0)` to bytecode about to
   `areturn` / `checkcast` / `arraylength` it.

`Object(None)` from the by-name reader means "null **OR** absent". To tell them
apart, ask `resolve_field_index_by_class_id(class_id, name).is_some()`. That is
the only oracle available to a native.

---

## 3. Risk census — `native-builtins/`

Baseline (`HEAD` before this change): **1576** `.get_field_by_name(` call sites
across **83** files. Counting them is not useful; a by-name read whose result is
only *used as a value* behaves correctly. Triaged by risk:

| # | risk shape | count (baseline) | verdict |
|---|---|---|---|
| R1 | result returned directly out of a native (`Ok(Some(ctx.get_field_by_name(..)))`, `=> ctx.get_field_by_name(..)`) | **78** | **dangerous when the registered return descriptor is a reference type** — this is the `Enum.toString` family. 18 fixed, 60 remain (mostly `()I` returns, generic getters whose descriptor is not statically known, and third-party shims). |
| R2 | tested against `Value::Object(None)` as a null/presence check | **10** | mostly **sound by construction** — see below. 1 genuinely wrong, fixed. |
| R3 | `match … { Value::Int(v) => v, _ => default }` | ~745 syntactic `match` sites | dangerous only where `0` is unsafe or the default is more permissive than the real value. 2 security-relevant instances found and fixed; the rest read counters/offsets/flags where `0` is the correct answer for an unset field. |
| R4 | `if let Value::Object(Some(o)) = …` / `let … else` | ~317 | **benign.** An unwritten reference slot yields `Int(0)`, misses the arm, and is treated as null — which is what it is. |

**R2 is smaller than it looks, and that is a real finding, not a shortcut.** Of
the ten sites, seven are documentation comments and one is a test assertion. Of
the two live predicates:

* `native-builtins/src/servlet.rs:2913` (`mark`) and `:3032` (`bigEndian`) are
  **correct**. Both fields are primitives in the real JDK layout (`int` /
  `boolean`), so a by-name read of them can *never* answer `Object(None)` unless
  resolution itself failed — which is precisely the discriminator those sites
  want. The DirectByteBuffer lane already reasoned this through in the comments
  there; the reasoning holds, and the comments are worth keeping.
* `native-builtins/src/lib.rs:6542-6543` (Thread `target`/`holder`) is a *reference*
  test, but it fails **closed**: an unwritten slot makes the condition false and
  skips a synthetic indexed-write fallback. Left as-is, recorded here.
* `native-builtins/src/spring_startup_bootstrap.rs:1026` (`data`) was genuinely
  wrong — fixed.

The general lesson: **a by-name `Object(None)` test on a field whose declared
descriptor is a primitive is sound; on a reference field it is not.** That is
the cheapest triage question to ask at a call site.

---

## 4. Fixed here

New module `native-builtins/src/field_read.rs` (7 unit tests) provides:

| helper | use |
|---|---|
| `ref_field(ctx, this, name) -> Value` | reference-typed read; guaranteed `Value::Object(..)`, never a primitive tag |
| `ref_field_obj(ctx, this, name) -> Option<ObjectRef>` | the same as an `Option` |
| `ref_field_is_null(ctx, this, name) -> bool` | the null test `matches!(.., Object(None))` is not |
| `declares_field(ctx, this, name) -> bool` | separates "absent" from "null" |
| `int_field_strict(ctx, this, name) -> Option<i32>` | **fail-closed** primitive read for security/crypto parameters; `None` for absent, no by-name fallback |

All of them resolve the slot with `resolve_field_index_by_class_id` on the
**receiver's own** `ClassId` and read by index, so the descriptor decode
applies. (That also fixes a second, independent problem at some of these sites:
the name-keyed `resolve_field_index("java/lang/Enum", ..)` re-resolves the class
*globally* by name and returns `None` under a loader split — see
`resolve_field_index_by_class_id`'s doc in `native-api/src/registry.rs:2186`.
Note `native-api/` is being edited concurrently by another lane on this branch;
if that line has moved, `rg -n 'fn resolve_field_index_by_class_id' native-api`
finds it.)

### The two confirmed instances

**(a) BouncyCastle stream-cipher round count** —
`native-builtins/src/phases_late/bouncycastle.rs:7013` (the comment block above
it, :6994-7012, records the reasoning). Was:

```rust
let rounds = match ctx.get_field_by_name(this, "rounds") {
    Value::Int(v) => v,
    _ => 20,
};
```

`rounds` is `protected int rounds`, so this is consequence (2) above: the
`_ => 20` safe default was **unreachable for the dangerous input**, because an
unwritten slot answers `Int(0)` and the *first* arm accepts it.
`chacha_core(0, ..)` / `salsa_core(0, ..)` is the identity permutation, so the
"keystream" is the engine state and the key is recoverable from the ciphertext.
`native-builtins-crypto/src/bc_chacha.rs` (`check_rounds`) names this exact call
site as one of the two arms that validate nothing, and its only remaining guard
is a `panic!` in `demand_rounds`. Now reads via `int_field_strict`, requires a
positive even count, and raises `IllegalStateException` otherwise — **no
default at all**. Do not restore one.

**(b) `Enum.name` / `Enum.toString`** — both registered
`()Ljava/lang/String;`, both were tailed by a raw by-name read. Two competing
registrations exist (memory: *duplicate native registrations — verify which
wins*), so both were fixed:
`native-builtins/src/lib.rs:15554` and `:15567` (registrations), and
`native-builtins/src/lang_misc.rs:1678` (`native_enum_name`, which
`lang_misc.rs:1633` registers for `toString` as well). The `lang_misc` variant
read hardcoded slot 0 by index, which is descriptor-decoded *only* when the
receiver's class metadata resolves — a synthetic stand-in with no resolvable
layout degrades to the raw read and leaks `Int(0)`. It now prefers the resolved
`name` field and coerces the slot-0 fallback.

### The rest

| file:line | what | why |
|---|---|---|
| `phases_late/bouncycastle.rs:10414` | `bc_pkcs12_read_state` `iterationCount` | **security.** Its sibling `bc_pkcs5s2_read_state` (PBKDF2) already refuses a non-positive count; this one accepted `Int(0)`. A PKCS#12 KDF at zero iterations degenerates the derived key. Now `int_field_strict` + `> 0` + `IllegalArgumentException`. |
| `lang_class.rs` ×8 | `Field.getName/getType/getDeclaringClass/getGenericType`, `Method.getName`, `Constructor.getParameterTypes/getDeclaringClass`, `RecordComponent.getGenericType` | all reference/array return descriptors. `getModifiers` (`()I`) deliberately left on the by-name read. |
| `lang_reflect.rs` ×5 | `ParameterizedTypeImpl.getRawType` (both descriptors), `.getOwnerType`, `TypeVariableImpl.getName`, `GenericArrayTypeImpl.getGenericComponentType` | `Type`/`Class`/`String` returns; this is the same shape as the already-recorded Spring `ClassCastException to Type[]` bug in that file. |
| `lib.rs:16984`, `:16994` | `LogRecord.getLevel`, `.getMessage` | `()Ljava/util/logging/Level;`, `()Ljava/lang/String;` |
| `lib.rs:2796` | `Randomness.getRandom` | `()Ljava/util/Random;` |
| `spring_startup_bootstrap.rs:1032` | `data` null test | R2 — reference field tested with `matches!(.., Object(None))` |

22 call sites in 6 files.

---

## 5. Grep recipe for the rest

Highest signal first. Run from the repo root.

```sh
# R1 — a by-name read returned straight out of a native. DANGEROUS whenever the
# registered descriptor is a reference type; check the `registry.register(..)`
# for the enclosing fn before deciding.
rg -n 'Ok\(Some\(ctx\.get_field_by_name|return ctx\.get_field_by_name|=> ctx\.get_field_by_name' native-builtins/src

# R2 — Object(None) used as a null/presence test. SOUND if the field's declared
# descriptor is a primitive, WRONG if it is a reference.
rg -n 'get_field_by_name' native-builtins/src | rg 'Object\(None\)'

# R3 — a match with a default arm. Dangerous only where the default is safer
# than the value, or where 0 is not a legal value. Read the default, not the
# match.
rg -n -A4 'match .*\.get_field_by_name' native-builtins/src | rg -B4 '_ =>'

# Security/crypto parameters specifically — the highest-consequence subset.
rg -n 'get_field_by_name' \
   native-builtins/src/phases_late/bouncycastle.rs \
   native-builtins/src/jca native-builtins/src/keystore.rs \
   native-builtins/src/x509_manager.rs native-builtins/src/securerandom.rs \
   native-builtins/src/t27_tls.rs native-builtins/src/tls.rs \
   native-builtins/src/phases_late/ssl_security.rs

# The stale comment that is the root of the family.
rg -n 'already Object\(None\) from zero memory' vm/src/runtime/interpreter.rs
```

Triage question at each hit, in order:

1. What is the field's **declared descriptor**? Reference or primitive?
2. Does any arm mean "absent" / "not set" / "default"? If so, `Object(None)` and
   `Int(0)` are both reachable for reasons the code did not intend.
3. Is the default **more permissive** than the real value? (A round count, an
   iteration count, a key length, a permission mask, a `verify` flag.) If so,
   refuse instead of defaulting.
4. Is the value returned to bytecode? Then the tag must match the registered
   descriptor.

If the answer to 1 is "reference" or to 3 is "yes", use `field_read::` rather
than patching the match.

---

## 6. What a proper fix looks like — `native-api/` + `vm/` (NOT done)

The call-site fixes above are real but they are containment. The accessor
itself is wrong and every future native will hit it again. Two changes, neither
in `native-builtins/`:

**6.1 — make `get_field_by_name` descriptor-aware (`vm/src/vm/vm_exec.rs:8735`).**
One line: route the found index through `self.get_field(obj, index)` instead of
`self.shared.mem.heap.get_field(obj, index)`, so the same
`resolve_field_descriptor_byte_cached` + `get_field_as` decode applies. This
makes the two readers agree for every descriptor kind and closes the whole
family.

*This is a behaviour change for every native in the tree* and must not be
folded into a call-site fix. Expect fallout in natives that today rely on the
`Int(0)`-means-absent behaviour
(**terminology** read that as *unwritten*, not *absent*. This
document's own §1 table is the authority — an **absent** field answers
`Object(None)` from production `get_field_by_name`; `Int(0)` is what a
present-but-**unwritten** slot decodes to. Several records elsewhere in `docs/`
took the loose phrasing literally and built layout discriminators on it; see
[§4 of *Natives over real JDK
classes*](../architecture/natives-over-real-jdk-classes.md));
`lang_misc::init_suppressed_sentinel` documents
relying on it explicitly, and `read_throwable_field`
(`native-builtins/src/lang_misc.rs:128`) was broken once by not accounting for
it. Land it on its own, with the synthetic-jdk VM gate green on both platforms.

**6.2 — expose a descriptor-qualified resolver on the trait (`native-api/src/registry.rs`).**
`resolve_field_index_in_hierarchy_desc` (`vm/src/vm/vm_exec.rs:2551`) already
exists and is what the DirectByteBuffer lane used, but it is `pub(crate)` to the
`vm` crate — a native cannot reach it. Natives therefore cannot address a
**shadowed** super-class field at all: the name-only resolve always returns the
most-derived declaration. Proposed addition, mirroring the existing
`resolve_field_index_by_class_id`:

```rust
/// Resolve a field name to its slot index, disambiguated by its JVM type
/// descriptor (`"I"`, `"Ljava/lang/String;"`, `"[J"`, …). `descriptor: None`
/// is exactly `resolve_field_index_by_class_id`. Passing a super-class
/// field's descriptor walks past a subclass shadow to the intended slot.
fn resolve_field_index_by_class_id_desc(
    &self,
    class_id: ClassId,
    field_name: &str,
    descriptor: Option<&str>,
) -> Option<usize> {
    // default: ignore the descriptor, so mocks compile unchanged
    self.resolve_field_index_by_class_id(class_id, field_name)
}
```

with the `vm` impl delegating to `resolve_field_index_in_hierarchy_desc`. Once
that lands, `field_read::` should take an optional descriptor and pass it
through, and `int_field_strict` should assert `"I"` rather than inferring the
type from the returned tag.

**6.3 — fix the stale comment (`vm/src/runtime/interpreter.rs:2815`).** It
asserts reference slots are "already `Object(None)` from zero memory". They are
not. Either correct the comment or — better — have `init_primitive_fields` write
`Value::Object(None)` for reference descriptors too, which would make the whole
family unreachable for objects allocated through it. That is the smallest change
with the largest blast radius and deserves its own measurement: it adds one slot
write per reference field per native-allocated object.

---

## Related

* [`../security/crypto-failure-contract.md`](../security/crypto-failure-contract.md)
  — the crypto-side guards (`check_rounds`, `demand_rounds`) that instance (a)
  was upstream of.
* `native-api/src/registry.rs:2173-2180` — the trait doc for both accessors.
  It documents the "not found" case but not the descriptor asymmetry; worth
  amending alongside 6.1.
* memory `native-field-by-name-read-is-not-descriptor-aware` — the note that
  first recorded this asymmetry, and the decision to keep the accessor
  unchanged pending a lane of its own.
