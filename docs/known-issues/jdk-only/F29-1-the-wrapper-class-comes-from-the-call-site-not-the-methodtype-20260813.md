# F29-1 — The wrapper class comes from the CALL SITE, not the `MethodType`; the one bound in the boxing family that is configurable; and the widener the VarHandle path still does not call

**Status: FIXED-UNVERIFIED (`native-builtins/src/lang_invoke.rs`,
`native-builtins/src/lang_math.rs` — this lane's two files); NOMINATED (the
rest). Partly fixed with a named live residual, so: OPEN.**

**Prov: every HotSpot row MEASURED on this host** against
`openjdk 25.0.3 2026-04-21 LTS (25.0.3+9-LTS)` (Microsoft build
`Microsoft-13877124`), from `scratchpad/f29/CollBox.java`, `CollBox2.java`,
`CacheHigh.java` and `VhLong.java`, **before** any of it was written down.
`CollBox`+`CollBox2` are byte-identical over three runs, under `-Xint`, and
under `-XX:-UseCompressedOops` (one md5 for all five). `VhLong` is identical
under `-Xint`. **Every CratonVM "after" is PREDICTED** — this lane may not
build or run the VM. Both edited Rust files were parse-checked with
`rustfmt --edition 2021 --emit stdout` on a **copy** in the scratchpad (rc=0
both) and the check was **mutation-verified**: turning
`pub fn canonical_wrapper_if_cached(` into `pub fn canonical_wrapper_if_cached( {`
makes it report `mismatched closing delimiter` at that line. Parsing is not
type-checking.

**2026-08-13, lane F29.** Takes F19-1's three residuals N1, N2, N3 and one
more open record (F1-1 N3). Applies to `lang_invoke.rs` and `lang_math.rs`
only, plus this record.

---

> **VERIFIED AGAINST A BINARY 2026-09-04. The stated Acceptance is met on both
> arms.** Status was **FIXED-UNVERIFIED** for this lane's two files, with every
> CratonVM row PREDICTED. §"Acceptance, MEASURED (§4)" names three requirements;
> `probes/VhBoxProbe.java` prints exactly those and nothing else:
>
> ```text
>                                 HotSpot 25    compatible   --jdk-only
> vh.fieldLong      class         java.lang.Long   identical   identical
>                   value         5
>                   == Long.valueOf(5)    true
> vh.getAndSetLong  class         java.lang.Long
>                   value         5
>                   == Long.valueOf(5)    true
> vh.fieldDouble    class         java.lang.Double
>                   value         1.5
>                   != Double.valueOf(1.5)  true
> ```
>
> Both CratonVM arms are **byte-identical to the oracle** across the whole
> transcript.
>
> **The two identity rows are the ones that carry the result**, and they point
> in opposite directions on purpose: `Long.valueOf` caches −128..127, so a
> correctly boxed `5` must BE the cached instance; `Double.valueOf` caches
> nothing, so a correctly boxed `1.5` must NOT be. A VarHandle that returned a
> raw primitive-shaped value, or that boxed through the wrong wrapper class,
> would move one of these without moving the other. Neither moved. That is what
> makes this stronger than the `equals` rows above them, which a wrong-but-equal
> value would pass.
>
> `RJdkReflBox`, the vector this record's NOMINATION 4 is about, passes in both
> Compatible and `--jdk-only` in a full 129-vector run.
>
> **This record stays OPEN, and the status block is right that it should.** It
> says *"Partly fixed with a named live residual"* — the widener the VarHandle
> path still does not call. Nothing here closes that; the Acceptance above is
> the boxing family, not the widening. NOMINATION 4a's *"two new targets and
> four rows"* were not added to `RJdkReflBox`, so the vector still does not
> cover what this record wanted it to cover, and its green is correspondingly
> narrow. The HotSpot rows from `scratchpad/f29/{CollBox,CollBox2,CacheHigh,
> VhLong}.java` are the oracle and were not re-derived; that scratchpad did not
> survive its session.

## 0. Verdict

| | |
|---|---|
| **F19-1 N2's diagnosis is wrong on the measurement, and that is this record's headline** | It says fixing the collector arms *"needs the target handle's `MethodType`"*. MEASURED: `coll.type()` is `(Object)Object` and `coll.invoke(aChar)` is still a `Character`. The `MethodType` does not carry it. The **call site's static parameter type** does. §1 |
| what IS fixable at the arm, and is now fixed | a **wrapper-typed** array component (`Character[]`, `Boolean[]`, …) settles the element class, and HotSpot **refuses** every other static argument type for such a collector — so the rule is total for every call that runs at all, not a heuristic. §2 |
| what is NOT fixable there | the ordinary `Object[]` collector. NOMINATION 2, against `vm/src/vm/vm_exec.rs`, which already reads the call-site descriptor and does not pass it on. §2.3 |
| F19-1 N1 (`canonical_wrapper_if_cached`) | **applied**, read-only, six caches, no new storage. `Z` is deliberately absent and §3.2 says why. Call site nominated (NOMINATION 1) |
| F19-1 N3 (`vh.fieldLong`) | F19-1 §5.5 called this row **unknown**. It is now **MEASURED**, the direction is confirmed, and the set is **four** arms not three — `vh_box_access_result` is in it. The fix still needs one word in a file this lane does not own: NOMINATION 3 (atomic pair). §4 |
| F1-1 N3 (`IntegerCache.high`) | **applied**. `-D…IntegerCache.high=1000` now widens `Integer.valueOf` to -128..=1000 and moves **nothing else** — MEASURED, including the four caches that must not move. §5 |
| Rust tests added | **11** (4 in `lang_invoke.rs`, 7 in `lang_math.rs`), all mutation-reasoned; none of them is equality-shaped where identity is the question |
| fixture rows | handed back as NOMINATION 4, in two variants, with the measurement that says which one to take **now** and which one is red until NOMINATION 2 lands |

---

## 1. The measurement that overturns N2

`MethodHandle.asCollector(Object[].class, 1)` on a target of type
`(Object[])Object`. F19-1 measured that the element is CANONICAL
(`mhcoll.char` = true) and inferred that the wrapper CLASS must come from the
target's `MethodType`. It does not. `CollBox.java` prints the type alongside
the class:

```text
coll.type                = (Object)Object          <-- the collector's MethodType
A.callsiteChar.class     = java.lang.Character
A.callsiteInt.class      = java.lang.Integer
A.callsiteBool.class     = java.lang.Boolean
A.callsiteByte.class     = java.lang.Byte
A.callsiteShort.class    = java.lang.Short
A.callsiteLong.class     = java.lang.Long
mhcoll.char = true   mhcoll.bool = true   mhcoll.byte = true
mhcoll.short = true  mhcoll.int  = true
```

Six different wrapper classes out of one handle whose `MethodType` says
`Object` for all six. The `char` and the `int` rows are the pair that settles
it: `CA` is `'a'` and `IA` is `97`, i.e. **the same bits**, and the two answers
differ. The target's own type is `(Object[])Object`, whose component is also
`Object`, so it cannot supply it either.

Section E removes the last alternative explanation — that `invoke`'s
polymorphic dispatch is doing something a normal adapter would not:

```text
typed = coll.asType((char)Object)
typed.type          = (char)Object
E.asTypeChar.class  = java.lang.Character
E.asTypeInt.class   = java.lang.Integer
```

The boxing is done by the **`asType` adapter**, from the adapter's declared
parameter type. `invoke` is signature-polymorphic, so its call-site descriptor
IS that type; there is no third source. CratonVM's `asType` is a passthrough
shim, which is precisely why the collector arm has to box at all — and why it
has nothing to box *by*.

**Where the information lives in this tree.** `vm/src/vm/vm_exec.rs` has the
call-site descriptor at the signature-polymorphic dispatch (it is the local
`descriptor`, already consumed by `unbox_poly_return_checked`). It is not
passed to the native, and `NativeContext` has no accessor for it — grepped.
`mh_read_desc(ctx, this)` inside the `invoke` registration reads the HANDLE's
descriptor, not the call site's.

## 2. What the component settles, and why it is total

`CollBox2.java`, section G. The left column is the collector's array type; the
cells are `getClass().getName()` of the element the target received:

| collector | from `char` | from `int` | from `long` | identity |
|---|---|---|---|---|
| `Character[]` | `Character` | **`WrongMethodTypeException`** | — | true |
| `Boolean[]` | — | (`Boolean` from `boolean`) | — | true |
| `Byte[]` / `Short[]` | — | per type | — | — |
| `Integer[]` | **WMTE** | `Integer` | **WMTE** | — |
| `Long[]` | — | **WMTE** | `Long` | true |
| `Float[]` | — | — | — | **false** |
| `String[]` | **WMTE** | — | — | — |
| `Object[]` / `Comparable[]` | `Character` | `Integer` | `Long` | true |
| `Number[]` | **WMTE** | `Integer` | — | — |

Three things follow, and none of them is guessable from the others:

1. **A wrapper-typed component is a total rule, not a heuristic.** Every
   cross-type call is a `WrongMethodTypeException` on HotSpot, so for every
   call that executes at all, the component IS the answer. This is the half
   that is now implemented.
2. **`Float[]` settles the CLASS and not the identity** (`G.FloatComp.id` =
   false). `Float`/`Double` have no cache — the same asymmetry F19-1 recorded
   for the `F`/`D` collector arms and for `neg.floatValueOf`. The site
   therefore keeps spelling those two `box_value`.
3. **`Number[]`/`Comparable[]` are NOT a ninth row.** A reference component
   that is not itself a wrapper carries no primitive, so
   `collector_element_box_desc` answers `None` for them and the variant
   fallback runs — which is already right for the arms HotSpot accepts
   (`C.componentNumber.fromInt` = `Integer`) and irrelevant for the arms it
   refuses.

Section H says the same about varargs, which is a different door in this tree
(`build_varargs_array`, reached from the varargs-adaptation path, not from
`MH_KIND_COLLECT`):

```text
H.vChar.fromChar = java.lang.Character     H.vChar.id      = true
H.vObj.fromChar  = java.lang.Character     H.vObj.fromInt  = java.lang.Integer
H.vLong.fromLong = java.lang.Long          H.vLong.id      = true
H.vLong.fromInt  -> WrongMethodTypeException
```

### 2.1 What changed in `lang_invoke.rs`

Two new private functions beside `widen_primitive_to_descriptor`:

* `collector_element_box_desc(component: &str, v: Value) -> Option<&'static str>`
  — **pure**, no `ctx`. Maps the eight wrapper-class component descriptors to
  their primitive descriptor, and `None` otherwise. The `Value` variant is
  matched as well as the component, for the reason `box_value_canonical`
  matches it: `("Ljava/lang/Long;", Value::Int(5))` routed on the descriptor
  alone reaches `native_long_value_of`, which reads `Some(Value::Long(v))` and
  **defaults to 0** — an identity fix converted into a wrong answer. HotSpot
  refuses that pair anyway (`G.LongComp.fromInt` throws), so declining costs
  nothing.
* `box_collector_element(ctx, v, component) -> Value` — the ONE
  implementation of the rule. `mh_dispatch`'s `MH_KIND_COLLECT` arm and
  `build_varargs_array`'s reference arm were two copies of it; they are now
  one call each. Two copies of one rule drifting is the species this directory
  has the most records about, and these two were already a copy-and-paste pair
  by their own comments' admission.

### 2.2 The four `F`/`D` arms F19-1 deliberately left, and why they are still visible

F19-1 §3.1 left sites 14/15/18/19 spelled `box_value` so the measured
asymmetry would be readable at the site. Collapsing the two loops into
`box_collector_element` would have hidden that inside a helper — so the helper
keeps the branch **and the reason** at the point where it chooses:

```rust
return if desc == DESC_FLOAT || desc == DESC_DOUBLE {
    crate::lang_class::box_value(ctx, v, desc)
} else {
    crate::lang_class::box_value_canonical(ctx, v, desc)
};
```

`box_value_canonical` would delegate `F`/`D` back to `box_value` anyway; the
spelling is documentation that costs nothing.

### 2.3 The residual, stated so it cannot be mistaken for done

`asCollector(Object[].class, n)` — the Groovy-indy shape, and the shape
`RJdkReflBox.mhcollect` uses — **still boxes a `char` as an `Integer`.** It is
a wrong CLASS, not a wrong identity, it is unchanged by this lane, and closing
it needs the call-site descriptor (NOMINATION 2). PREDICTED: `mhcoll.char`,
`mhcoll.bool` and `mhvar.char` are **red on CratonVM** after this change, which
is why §7's NOMINATION 4 does not put them in the vector yet.

## 3. `canonical_wrapper_if_cached` (F19-1 N1)

```rust
pub fn canonical_wrapper_if_cached(
    vm_identity: usize,
    desc: &str,
    v: Value,
) -> Option<cratonvm_types::ObjectRef>
```

Read-only. No allocation, no `ensure_class_initialized`, no population. That
is a correctness requirement, not a performance one: populating needs
`alloc_wrapper` → `ensure_class_initialized` → `<clinit>`, and a proxy
invocation is not a legal place to trigger class initialisation. A miss is
`None` and the caller keeps its existing allocation — **never** a `null`,
which is the defect already recorded above `lang_class::create_method_object`.

The `Value` variant is matched as well as the descriptor. `("J",
Value::Int(5))` must MISS, and the test that proves it first populates
`Long.valueOf(0)` so the wrong answer is actually *available* to be returned —
otherwise the test passes for the wrong reason.

### 3.1 Index math, per cache, because the bounds all differ

| desc | variant | admitted | index | cache |
|---|---|---|---|---|
| `I` | `Value::Int` | `>= -128`, then the STORE's length | `x + 128` | `INTEGER_CACHE` (configurable, §5) |
| `J` | `Value::Long` | `-128..=127` | `x + 128` | `LONG_CACHE` |
| `C` | `Value::Int` | `0..=127` | `x` — **no offset**, `CharacterCache` has no negative half | `CHARACTER_CACHE` |
| `B` | `Value::Int` | `-128..=127` | `x + 128` | `BYTE_CACHE` |
| `S` | `Value::Int` | `-128..=127` | `x + 128` | `SHORT_CACHE` |

The `I` row is the one that is not a constant: its bound is the backing
store's LENGTH, so the probe's `get(idx)` **is** the bound test and no separate
memo read is needed. A VM that has not boxed an `int` yet has neither a store
nor a memo, which is a miss either way.

### 3.2 `Z` is absent, and it is the arm a reader will want to add

`Boolean.valueOf` returns the live `Boolean.TRUE`/`FALSE` **static fields**,
not a privately minted twin — `native_boolean_value_of` resolves them that way
and `BOOLEAN_CACHE` is only its bootstrap fallback. An entry in that fallback
is therefore not guaranteed to be the instance the rest of the VM calls
canonical, and returning it would reintroduce exactly the Xerces
`fFeatures.get(...) == Boolean.TRUE` defect F19-1 closed. `vm_exec.rs` already
resolves the statics itself (`proxy_canonical_boolean`) and needs nothing from
here. `F`/`D` are absent because HotSpot caches neither.

### 3.3 GC

**No new cached storage.** The probe reads the six caches that already live in
`lang_math.rs`, are already reported by `gc_scan_value_of_cache_roots` and
already remapped by `gc_update_value_of_cache_refs`, which
`vm/src/memory/native_roots.rs:222`–`:225` registers as one
`VmRootSource { scan, remap }` pair — so the two cannot be wired
independently. §5 changes the SHAPE of one of those six and §5.3 is the test
that the widened region reaches both hooks.

## 4. F19-1 N3 — measured, and it is four arms, not three

F19-1 §5.5 listed `vh.fieldLong`'s HotSpot verdict as **unknown**. MEASURED
(`VhLong.java`, identical under `-Xint`):

```text
vh.fieldLong.class     = java.lang.Long     vh.fieldLong.value     = 5     id = true
vh.fieldLongZero.value = 0                                                 id = true
vh.fieldDouble.class   = java.lang.Double   vh.fieldDouble.value   = 1.5   id = FALSE
vh.getAndSetLong.class = java.lang.Long     vh.getAndSetLong.value = 5     id = true
field.long.class       = java.lang.Long     field.long.value       = 5     id = true
```

So the direction is settled and it is `Field.get`'s: **widen first, box
canonically second.** The reverse order — box then widen — has no meaning, and
"box canonically without widening" is the state the tree is in.

**And the set is four, not three.** `vh.getAndSetLong` hands back the old value
`5` as a canonical `Long`, so `vh_box_access_result` — the RMW funnel — takes
the same raw slot through `getAndSet` and produces the same `Long` wrapper
carrying compact-`Int` bits. F19-1 N3 scoped the fix to `varhandle_get`'s
three field arms; the funnel is the fourth. `VH_KIND_STATIC`'s
`get_static_field` is as raw as `get_field`, which is the third.

**`Double` is not an identity row but IS a value row.** `id = false` means no
cache; it does not mean no coercion. A `double` slot carrying its bits as a
`Value::Long` must be *reinterpreted*, and this file's own
`widen_primitive_to_descriptor` would get that arm **wrong** — it converts
numerically (`Long(v) -> Double(v as f64)`) where
`coerce_reflective_field_value` reinterprets (`Double(f64::from_bits(v as u64))`).
That is why the fix must call `lang_class`'s function rather than grow a
second widener here, and it is why this lane did not "just do it locally".

**Not applied, and deliberately so.** `coerce_reflective_field_value` is a
private `fn` in `native-builtins/src/lang_class.rs`, a file this lane does not
own. Landing only the `lang_invoke.rs` half would leave the crate
**non-compiling**, so both halves are handed back as ONE atomic nomination
(NOMINATION 3). What IS applied here: the four sites now carry the measurement
and the four-not-three correction in place, so the next reader does not
re-derive it.

## 5. F1-1 N3 — `IntegerCache.high` is configurable, and it is the only one

The JDK rule, `jdk25src/java.base/java/lang/Integer.java`:

```java
int h = 127;
String v = VM.getSavedProperty("java.lang.Integer.IntegerCache.high");
if (v != null) {
    try {
        h = Math.max(parseInt(v), 127);
        h = Math.min(h, Integer.MAX_VALUE - (-low) - 1);
    } catch (NumberFormatException nfe) { }
}
```

MEASURED (`CacheHigh.java`), one run per configuration:

| configuration | `int.127` | `int.128` | `int.1000` | `int.1001` | `int.-129` | `long.128` | `short.128` | `char.128` |
|---|---|---|---|---|---|---|---|---|
| default | true | false | false | false | false | false | false | false |
| `-Djava.lang.Integer.IntegerCache.high=1000` | true | **true** | **true** | false | false | false | false | false |
| `-XX:AutoBoxCacheMax=1000` | true | **true** | **true** | false | false | false | false | false |
| `=abc` | true | false | false | false | false | false | false | false |
| `=50` | true | **false** | false | false | false | false | false | false |

Five things each of which needed its own row:

1. `=1000` widens the **upper** bound only; `int.-129` stays false, because
   `low` is a literal `-128` with no property behind it.
2. `=50` does **not narrow** — `Math.max(v, 127)` floors it. A reader who
   implemented `high = parsed` would make a narrowing configuration
   observable, which HotSpot never does.
3. `=abc` is ignored, not fatal.
4. `-XX:AutoBoxCacheMax=1000` is byte-identical to the `-D` form. CratonVM
   parses neither `-XX:AutoBoxCacheMax` nor `VM.getSavedProperty`; only the
   `-D` road is wired here. §8.
5. `System.getProperty("java.lang.Integer.IntegerCache.high")` answers
   **`null` in every one of the five runs**, including the two where the cache
   really was widened. F1-1's warning about this is confirmed. It is a HotSpot
   fact about the *saved-property split*, not a fact about the cache — CratonVM
   has no such split, `-D` lands in `shared.system_properties`, and
   `get_system_property` reads it.

### 5.1 What changed

* `INTEGER_CACHE` becomes `HashMap<usize, Vec<Option<ObjectRef>>>`
  (`ScopedIntegerCache`). It is the only one of the six whose bound is not a
  literal, so it is the only one that cannot be a `[Option<ObjectRef>; N]`.
  The `Vec` is sized once, at bound-resolution time, and never resized — so an
  index computed against the latched bound is always in range.
* `integer_cache_bound(ctx)` resolves and **latches** the bound per VM.
  Latching on first use is HotSpot's own timing, not an approximation of it:
  `IntegerCache.high` is a `static final` assigned in `IntegerCache.<clinit>`,
  which runs at the first autobox and never again. A later `System.setProperty`
  moves neither.
* The memo is **VM-scoped**, not a process-global `OnceLock`. A `OnceLock`
  latches the first VM's answer for the life of the process, and this crate's
  tests build several independent VMs in one binary.
* `parse_integer_cache_high` uses this file's own `java_parse_signed`, not
  `str::parse`: the property is read by `Integer.parseInt`, whose grammar
  accepts a leading `+` and rejects surrounding whitespace. One grammar, one
  implementation.

### 5.2 The `try_reserve_exact`, which is not defensive padding

`high` is permitted up to `Integer.MAX_VALUE - 129`, i.e. a backing store of
~17 GB. HotSpot answers that configuration with an `OutOfMemoryError` from
`new Integer[...]`; a `vec![None; len]` here would **abort the process**,
which is strictly worse than any Java outcome. On a refusal the bound falls
back to the JDK default 127 rather than to something in between, so the VM
stays in a state the oracle can also produce.

### 5.3 The GC hazard this created, and the test for it

`scan_one_cache`/`update_one_cache` were `<const N: usize>` over
`ScopedValueCache<N>`. A `Vec`-backed cache does not satisfy that, and the
tempting fix — a second scan/remap pair for `INTEGER_CACHE` — is the exact
failure the functions were factored out to prevent. They are now generic over
`AsRef<[Option<ObjectRef>]>` / `AsMut<…>`, which both `[T; N]` and `Vec<T>`
satisfy, so **the six call sites are unchanged and there is still one scan and
one remap**. `the_widened_integer_region_is_both_scanned_and_remapped` boxes
`900` under a widened bound, asserts it is reported as a root, remaps it, and
asserts the cache hands back the new address. A bound that grows past a hook
still walking 256 slots is a use-after-move that only a compacting collection
reveals.

## 6. The tests, and what each one would catch

**`lang_invoke.rs` (4).** All four drive `collector_element_box_desc`, which is
a pure function of `(component, Value)`. No mock, no VM, no slot table — the
`MockNativeContext` name-to-slot fallback measures the mock, not the rule.

| test | fails when |
|---|---|
| `a_wrapper_typed_component_settles_the_element_class` | any of the eight component→primitive rows is dropped or mistyped |
| `a_non_wrapper_component_settles_nothing` | someone "improves" the helper into a general reference-descriptor guesser; `Object[]` must stay `None` |
| `the_component_is_not_trusted_against_a_mismatched_value_variant` | the variant guard is dropped — the `("Ljava/lang/Long;", Value::Int(5))` → `Long.valueOf(0)` wrong-answer route |
| `char_and_int_separate_on_the_component_though_the_variant_cannot` | a later edit collapses the helper back to a variant-only match, i.e. re-creates the state this lane found. **`Value::Int(97)` is the same operand in both halves**, so nothing but the component can separate them |

**`lang_math.rs` (7).**

| test | fails when |
|---|---|
| `integer_cache_high_follows_the_jdks_three_clauses` | `Math.max(v,127)` (the `=50` row), the `Math.min` clamp, the `catch`, or the Java parse grammar is dropped. Pure function |
| `the_integer_cache_widens_on_the_property_and_nothing_else_moves` | the property never reaches the cache — **or** it drags `Long`/`Short`/`Character` with it, which is the "make the family consistent" edit |
| `the_integer_cache_bound_is_per_vm_not_per_process` | the memo becomes a bare `OnceLock` |
| `the_widened_integer_region_is_both_scanned_and_remapped` | the widened region is scanned but not remapped, or neither |
| `canonical_wrapper_if_cached_never_populates_and_agrees_when_it_hits` | the probe populates on demand (a cold VM would answer `Some`), or answers with a private twin instead of the object the native itself returns |
| `canonical_wrapper_if_cached_matches_the_variant_not_only_the_descriptor` | the variant guard is dropped. `Long.valueOf(0)` is populated FIRST so the wrong answer is available |
| `canonical_wrapper_if_cached_declines_z_f_d_and_the_out_of_bound_arms` | a `Z` arm is added, an `F`/`D` cache is "completed", or a bound is widened past its measured value |
| `canonical_wrapper_if_cached_follows_the_configured_integer_bound` | the probe hard-codes `-128..=127` and so cannot see a widened cache — the row that ties §3 and §5 together |

**Not one assertion in this record is equality-shaped where identity is the
question.** Every "fresh" value in this family is still `.equals`-equal to its
canonical twin; F19-1 §2.1 measured that (`blind.equalsOob`,
`blind.equalsFloat`, `array.blindEquals` all TRUE). The Rust tests compare
`ObjectRef`s, which is `==` identity.

---

## 7. NOMINATIONS

### NOMINATION 1 — `vm/src/vm/vm_exec.rs`: route `C B S I J` through the probe

Closes F19-1 N1's call-site half. The path is proved by an existing caller of
the same shape: `vm/src/memory/native_roots.rs:222` already calls
`cratonvm_native_builtins::lang_math::gc_scan_value_of_cache_roots(shared.vm_identity, roots)`,
so both the crate path and the `shared.vm_identity` argument are established.

**OLD** (`proxy_box_value_for_desc`, the lines between the `Z` block and the
wrapper table):

```rust
            // Fall through to the allocating path — never to `null`. A boxing
            // failure that becomes a null argument is a defect already
            // recorded above `lang_class::create_method_object`.
        }
        let wrapper = match pdesc {
```

**NEW**:

```rust
            // Fall through to the allocating path — never to `null`. A boxing
            // failure that becomes a null argument is a defect already
            // recorded above `lang_class::create_method_object`.
        }
        // `C B S I`: the canonical instance IF one is already cached, read out
        // of the six caches in `lang_math.rs`. Read-only — it cannot allocate
        // and cannot run `<clinit>`, which is what makes it legal here.
        // Measured canonical on HotSpot: `proxy.char` / `byte` / `short` /
        // `int` all true (F19-1 §2). A miss falls through to the allocating
        // table below, never to `null`.
        if let Some(obj) = cratonvm_native_builtins::lang_math::canonical_wrapper_if_cached(
            shared.vm_identity,
            pdesc,
            Value::Int(v),
        ) {
            return Value::Object(Some(obj));
        }
        let wrapper = match pdesc {
```

**OLD** (the tail of the same function):

```rust
    // Long/Float/Double and reference values: descriptor-independent.
    proxy_box_value(shared, value)
}
```

**NEW**:

```rust
    // `J` has no `Value::Int` arm above, so it is taken here rather than in
    // the wrapper table. Measured `proxy.long` = true. `F`/`D` are NOT taken:
    // HotSpot caches neither (`neg.floatValueOf` = false), so
    // `canonical_wrapper_if_cached` declines them and `proxy_box_value`'s
    // fresh allocation is the correct answer, not a fallback.
    if pdesc == "J" {
        if let Some(obj) =
            cratonvm_native_builtins::lang_math::canonical_wrapper_if_cached(
                shared.vm_identity,
                pdesc,
                value,
            )
        {
            return Value::Object(Some(obj));
        }
    }
    // Long/Float/Double and reference values: descriptor-independent.
    proxy_box_value(shared, value)
}
```

Note the `J` arm passes `value`, **not** `Value::Int(v)` — it is outside the
`if let Value::Int(v) = value` block, and passing the wrong one is how a
`Value::Long(5)` becomes a lookup for `Long.valueOf(0)`.

### NOMINATION 2 — the call-site descriptor, so an `Object[]` collector can box by type

The residual of §2.3, and the largest item here. `vm_exec.rs`'s
signature-polymorphic dispatch has the call-site `descriptor` and does not pass
it to the native; `NativeContext` has no accessor for it (grepped). Closing
§2.3 needs one, e.g. a `fn current_polymorphic_call_descriptor(&self) ->
Option<&str>` set around `safe_native_call` for the `is_mh || is_vh` branch,
which `mh_dispatch`'s collector arm and `build_varargs_array` would then prefer
over the component.

**Not written out as literal old/new text, deliberately** — it spans
`native-api/src/registry.rs` (a trait method), `vm/src/vm/vm_exec.rs` (the
setter and the three `find` loops) and `native-builtins/src/lang_invoke.rs`
(the reader), and this lane cannot build any of them. What it CAN hand over is
the acceptance test, which is exact: after the change,
`asCollector(Object[].class, 1).invoke(aChar)` must deliver a
`java.lang.Character`, and `.invoke(anInt)` a `java.lang.Integer`, for the
same 97. Both are MEASURED HotSpot rows (§1).

Sequencing: NOMINATION 4b becomes takeable only when this lands.

### NOMINATION 3 — F19-1 N3, as ONE atomic pair (two files)

Landing either half alone is wrong: the `lang_class.rs` half alone is inert,
and the `lang_invoke.rs` half alone **does not compile**.

**3a — `native-builtins/src/lang_class.rs`, one word.** The precedent for the
visibility is in this same crate: `lang_math::java_math_min_f64` is
`pub(crate)` and is called from `phases_early.rs`.

OLD:
```rust
fn coerce_reflective_field_value(value: Value, descriptor: &str) -> Value {
```
NEW:
```rust
pub(crate) fn coerce_reflective_field_value(value: Value, descriptor: &str) -> Value {
```

**3b — `native-builtins/src/lang_invoke.rs`, four sites.** Each is the same
one-line insertion: coerce the raw slot before boxing.

`varhandle_get`, `VH_KIND_INSTANCE`, by index — OLD:
```rust
                let val = ctx.get_field(receiver, field_idx as usize);
```
NEW:
```rust
                let val = crate::lang_class::coerce_reflective_field_value(
                    ctx.get_field(receiver, field_idx as usize),
                    &td,
                );
```

`varhandle_get`, `VH_KIND_INSTANCE`, by name — OLD:
```rust
                        let val = ctx.get_field(receiver, idx);
```
NEW:
```rust
                        let val = crate::lang_class::coerce_reflective_field_value(
                            ctx.get_field(receiver, idx),
                            &td,
                        );
```

`varhandle_get`, `VH_KIND_STATIC` — OLD:
```rust
            let val = match vh_static_slot(ctx, &class, &field) {
                Some((cid, sidx)) => ctx.get_static_field(cid, sidx),
                None => return Ok(Some(Value::Object(None))),
            };
            let td = match meta.as_deref() {
                Some(m) => vh_type_desc_from_meta(m),
                None => vh_type_desc(ctx, this),
            };
```
NEW:
```rust
            let raw = match vh_static_slot(ctx, &class, &field) {
                Some((cid, sidx)) => ctx.get_static_field(cid, sidx),
                None => return Ok(Some(Value::Object(None))),
            };
            let td = match meta.as_deref() {
                Some(m) => vh_type_desc_from_meta(m),
                None => vh_type_desc(ctx, this),
            };
            let val = crate::lang_class::coerce_reflective_field_value(raw, &td);
```

`vh_box_access_result` — the fourth member, which F19-1 N3 did not name (§4)
— OLD:
```rust
    let desc = vh_access_value_desc(ctx, args);
```
NEW:
```rust
    let desc = vh_access_value_desc(ctx, args);
    let value = crate::lang_class::coerce_reflective_field_value(value, &desc);
```

(The `let value` here shadows the binding made twelve lines up, after the
`matches!(value, Value::Object(_))` early return has already excluded
references — so the coercion only ever sees a primitive, which is its
contract.)

Acceptance, MEASURED (§4): `vh.fieldLong` must be a `java.lang.Long` carrying
**5** and `== Long.valueOf(5)`; `vh.getAndSetLong` the same; `vh.fieldDouble`
must carry `1.5` and be `!=` `Double.valueOf(1.5)`.

### NOMINATION 4 — `regression-suite/src/RJdkReflBox.java`

The file is **LF**, not CRLF. Two variants; **4a is takeable today, 4b is not.**

**4a — the rows this lane's change turns green.** A wrapper-typed collector
component, which §2 measured as a total rule. Needs two new targets and four
new rows; `mhcollect`'s denominator moves 4 → 8.

OLD (the two helper-free lines at the end of `mhcollect`):
```java
        MethodHandle var = idn.asVarargsCollector(Object[].class);
        ck("mhvar.int", var.invoke(I7) == Integer.valueOf(I7), true);
        sectionEnd("mhcollect", 4);
    }
```
NEW:
```java
        MethodHandle var = idn.asVarargsCollector(Object[].class);
        ck("mhvar.int", var.invoke(I7) == Integer.valueOf(I7), true);

        // The WRAPPER CLASS, which is a different question from the identity
        // and is settled by the component here. MEASURED on HotSpot 25.0.3+9
        // (F29-1 §2): for a `Character[]` collector every other static
        // argument type is a WrongMethodTypeException, so the component is
        // the answer for every call that runs at all.
        MethodHandle collC = lk.findStatic(RJdkReflBox.class, "firstOfChar",
                MethodType.methodType(Object.class, Character[].class))
                .asCollector(Character[].class, 1);
        ck("mhcollc.charClass", collC.invoke(CA) instanceof Character, true);
        ck("mhcollc.charId", collC.invoke(CA) == Character.valueOf(CA), true);
        MethodHandle collL = lk.findStatic(RJdkReflBox.class, "firstOfLong",
                MethodType.methodType(Object.class, Long[].class))
                .asCollector(Long[].class, 1);
        ck("mhcolll.longClass", collL.invoke(J5) instanceof Long, true);
        ck("mhcolll.longId", collL.invoke(J5) == Long.valueOf(J5), true);
        sectionEnd("mhcollect", 8);
    }

    public static Object firstOfChar(Character[] xs) {
        return xs[0];
    }

    public static Object firstOfLong(Long[] xs) {
        return xs[0];
    }
```

`instanceof Character` rather than `getClass().getName().equals(...)`: the
dialect wants one boolean observable per `CK` line, and `instanceof` is the
narrowest thing that fails when the VM boxes a `char` as an `Integer`. The
`Id` row beside it is what keeps the identity question asserted — neither row
stands in for the other.

**4a was compiled and run, not just written.** `scratchpad/f29/Nom4a.java` is
this exact block in a standalone class: `javac -Xlint:all` reports **no
warnings**, and all five rows (the existing `mhvar.int` plus the four new ones)
answer **true** on HotSpot 25.0.3+9, byte-identical under `-Xint`. So the
denominator `8` is arithmetic that has been checked, and a red row on CratonVM
after 4a lands is a VM answer rather than a fixture bug.

**4b — F19-1's three rows. DO NOT TAKE YET.** These are the `Object[]`
collector, which §2.3 leaves broken; PREDICTED red on CratonVM. They become
takeable when NOMINATION 2 lands, and the header comment above `mhcollect`
(which currently cites F19-1 N2's superseded `MethodType` diagnosis) should be
repointed at this record's §1 at the same time.

```java
        ck("mhcoll.char", coll.invoke(CA) == Character.valueOf(CA), true);
        ck("mhcoll.bool", coll.invoke(ZT) == Boolean.valueOf(ZT), true);
        ck("mhvar.char", var.invoke(CA) == Character.valueOf(CA), true);
```

with `sectionEnd("mhcollect", 8)` becoming `sectionEnd("mhcollect", 11)` if 4a
is already in.

### NOMINATION 5 — `-XX:AutoBoxCacheMax`

MEASURED byte-identical to `-Djava.lang.Integer.IntegerCache.high` on HotSpot
(§5). CratonVM's `-D` road is now wired; the `-XX:` flag is not parsed
anywhere (`vm-cli/src/main.rs`). One line in the argument parser, mapping
`-XX:AutoBoxCacheMax=N` onto that system property, would close it. Not this
lane's file and not measured against CratonVM, so it is a nomination rather
than a claim.

---

## 8. Residuals

* **Nothing in this record claims a CratonVM behaviour was observed.** Every
  "after" is PREDICTED and is written so one run can falsify it.
* **The `Object[]` collector still boxes `char` as `Integer`** (§2.3). This is
  the headline residual and it is unchanged by this lane.
* **F19-1 N3 is not applied** (§4), by a scope rule rather than a technical
  one, and is handed back as an atomic pair.
* **`VM.getSavedProperty` has no analogue here.** CratonVM has no
  saved-property split, so a Java program that does
  `System.setProperty("java.lang.Integer.IntegerCache.high", "1000")` *before*
  the first autobox would widen this VM's cache and would not widen HotSpot's.
  Latching at first use bounds the window but does not close it; closing it
  needs a saved-property table, which is a `vm/` change.
* **`integer_cache_bound` reads the property through
  `ctx.get_system_property`, whose table is populated from `VmConfig` at
  `SharedVm::new`.** That it is populated before the first `Integer.valueOf`
  is READ from the boot path, not measured on a run.
* **The `try_reserve_exact` fallback is untested** — provoking it needs a real
  17 GB refusal, which no unit test should attempt. It is reasoned, not
  measured (§5.2).
* **The eleven Rust tests were never executed.** `cargo` is the
  orchestrator's; the files were parse-checked only, on copies.
* Both files remain pure CRLF (14,398 and 8,515 lines, zero bare LF), and no
  added line exceeds 100 columns.
