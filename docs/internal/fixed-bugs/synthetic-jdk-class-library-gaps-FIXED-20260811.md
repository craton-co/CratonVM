# The 11 synthetic-JDK "class-library gaps" were switched-off, unlinked or mis-shaped code — not missing code

| | |
|---|---|
| **Status** | FIXED 2026-08-11. `KNOWN_SYNTHETIC_JDK_GAPS` is empty; the extended interpreter corpus is 924/924. |
| **Area** | `java.io` serialization, generic reflection, `Arrays.asList`, `Field.get`, all under the `synthetic-jdk` class library |
| **Was pinned in** | `KNOWN_SYNTHETIC_JDK_GAPS`, `vm/tests/interpreter_tests.rs` (kept, empty, as the ratchet) |
| **Discovered** | 2026-08-02, when the extended interpreter corpus was made runnable. See the retired `extended-interpreter-corpus-is-a-synthetic-jdk-corpus-FIXED-20260805` write-up for why it had been dark. |

## The headline

Every one of the eleven was filed as *"the ~5,200-stub class library does not
implement this"*. Not one of them was. In each case the implementation was in
the binary and running:

* **Serialization (4).** `serialization.rs` — 7,647 lines of it — was compiled
  into the corpus build and its `register_serialization_natives` was never
  called, because that one call site carried a `#[cfg(feature =
  "experimental-serialization")]` on top of the `synthetic-jdk` gate the
  enclosing function already has. The corpus build enables `synthetic-jdk`
  only. `ObjectOutputStream` and `ObjectInputStream` were therefore present
  and INERT, which reads exactly like "not implemented": `writeObject` left a
  5-byte stub where the wire form should be (95 bytes once the natives were
  registered), `readObject` handed back `null`, and every field read on the
  result was a `NullPointerException`.
* **Generic reflection (4).** The WP2.8 reifier ran, parsed the `Signature`
  attribute, resolved `java/util/List` and `java/lang/String` correctly, and
  returned them on a `sun.reflect.generics.reflectiveObjects.
  ParameterizedTypeImpl`. In a synthetic build that class name has no class
  file, so it is fabricated — with no interfaces and no fields. So
  `t instanceof ParameterizedType` was `false`, and `getRawType()` answered
  `null` on an object whose raw type had just been computed.
* **`Arrays.asList` (1).** `java/util/Arrays$ArrayList` has a `$` in its name,
  which is the heuristic for "this is probably an interface". It was
  fabricated as an ABSTRACT INTERFACE with zero fields, so the array the
  `invokespecial` was meant to store had nowhere to go and `size()` answered 0
  for every list.
* **`Field.get` on an own private-final field (1).** The fixture was an
  INSTANCE method in a corpus whose harness only ever calls
  `vm.invoke(class, method, "()I", &[])`. There is no receiver to pass, so
  `this` was `null` and `Field.get` refused it — correctly. The oracle
  (`probes/CorpusOracle`) constructs a receiver for a non-static fixture
  method, so the two sides were asking different questions. CratonVM answers
  `42` the moment it is asked the fixture's actual question.
* **The finalizer observation (1).** Already closed before this branch started.
  A pristine run of current dev reported `FinalizerTest.testNoFinalizeOnLive
  now produces the expected Int(1)` — the list's unexpected-pass arm doing its
  job.

The common shape: **an inert implementation is indistinguishable from a
missing one from the outside, and the corpus only ever looked from the
outside.** A single `--synthetic-jdk` probe printing `getClass().getName()`
beside `instanceof` separated the two in one run — the `ParameterizedTypeImpl`
answered its own class name while `instanceof ParameterizedType` said no,
which is not a shape a missing feature can produce.

## Baseline, measured on current dev before touching anything

`CRATONVM_RUN_EXTENDED_INTERPRETER_TESTS=1 cargo test --release -p cratonvm-vm
--features synthetic-jdk --test interpreter_tests -- --test-threads=1`, all 11
entries still pinned:

```
test result: FAILED. 922 passed; 2 failed
    test_s17_method_invoke_private          expected Int(49), returned Int(-1)
    test_s27_no_finalize_on_live            now produces the expected Int(1) [unexpected pass]
```

Both matter. The second is the gate reporting a closed gap. The first is a
failure that is **not** one of the eleven and was already there: see
"The twelfth" below.

## What each fix was

### 1. Serialization: remove the second gate

`native-builtins/src/lib.rs` — `register_serialization_natives(registry)` no
longer carries `#[cfg(feature = "experimental-serialization")]`. It sits
inside `register_synthetic_overrides`, which is `#[cfg(feature =
"synthetic-jdk")]`, so it can only run in a synthetic-library build, and in
that build the classes it serves are fabricated stubs that do nothing without
it. The real-JDK path is untouched: it never calls this function, and its own
entry point (`register_reflection_factory_serialization`) keeps its
`experimental-serialization` gate.

Two residuals surfaced once the natives were actually running:

**1a. The rejection had the wrong class.** A non-`Serializable` write raised
`RuntimeError::IOException` whose *message text* was
`"java.io.NotSerializableException: …"`. `catch (NotSerializableException e)`
is written against the class and could never match it. Now it throws a real
`java/io/NotSerializableException`, and `jdk_superclass` gained the
`ObjectStreamException` subtree (`NotSerializableException`,
`InvalidClassException`, `InvalidObjectException`, `StreamCorruptedException`,
`OptionalDataException`, `WriteAbortedException` → `ObjectStreamException` →
`IOException`) so the coarser handlers still match. Without that chain a
fabricated `NotSerializableException` extends `java.lang.Object` and not even
`catch (Exception)` matches it.

**1b. Recycled addresses inherited dead streams' state.** Every side table in
`serialization.rs` is keyed by the stream object's raw address. Addresses are
recycled — by the heap within one VM, and by whole arenas in a test process
that builds ~900 of them. `ObjectInputStream.<init>` cleared none of them, so
a fresh stream could pick up a dead stream's wire-handle table and resolve a
`TC_REFERENCE` to the previous stream's object:

```
cratonvm/SerializeBasic::testSimpleRoundTrip — expected Int(42), threw
java.lang.ClassCastException: cratonvm.SerializeBasic$Nested cannot be cast to cratonvm.SerializeBasic
```

`testNestedObject` ran first and left its `Nested` behind under handle 0. Both
fixtures pass in isolation; only the full corpus recycles the address. Fixed by
clearing the handle table, the JEP-290 filter state and the byte buffer in
`ObjectInputStream.<init>`, and resetting the byte buffer in
`ObjectOutputStream.<init>` (whose `write_stream_header` APPENDS, so a stale
buffer put a second `AC ED 00 05` mid-stream). Clearing on construction rather
than only on `close()` is the point: neither fixture — nor most code wrapping a
`ByteArrayInputStream` — ever closes the stream.

### 2. Generic reflection: link the reified types, then give them slots

`classloading/src/class_manager.rs`, two tables:

* `jdk_interfaces` — `ParameterizedTypeImpl`, `WildcardTypeImpl`,
  `TypeVariableImpl` and `GenericArrayTypeImpl` now declare their interface
  plus `java/lang/reflect/Type` (they are fabricated with `Object` as
  superclass, so they inherit nothing, and every `Type[]` element store needs
  the supertype to hold). The bare-interface synthetics the reifier mints on
  its fallback paths got `Type` for the same reason.
* `synthetic_stub_fields` — the same four classes now declare their JDK fields
  by name and in the JDK's own order (`actualTypeArguments`, `rawType`,
  `ownerType` for the PTI — the order recorded in the `pti_real` note in
  `lang_reflect.rs`). Without them `class_num_total_fields` was 0, the
  `.max(2)`/`.max(3)` allocation floors handed out anonymous `_fN` slots, and
  every `set_field_by_name` write was silently discarded.

Residual: `WildcardTypeImpl` had neither `toString` nor `getTypeName`
registered and `render_type_name` had no arm for it, so `List<? extends
Number>` rendered as
`java.util.List<sun.reflect.generics.reflectiveObjects.WildcardTypeImpl@6>`
after logging a `NoSuchMethodError`. Both added.

### 3. `Arrays.asList`

`java/util/Arrays$ArrayList` joins the `is_concrete_dollar_class` carve-out so
the `$` heuristic stops classifying it as an interface, and declares its one
real field, `a`. `native_arrays_as_list` also stores the array itself when the
constructor did not — a no-op on the real-JDK path, where the real
`Arrays$ArrayList(E[])` bytecode has already run, and the only thing that
stores it in a synthetic build. Doing it there rather than as a native
`<init>` avoids shadowing a real JDK constructor for every instance.

### 4. `Field.get` on an own private-final field: fix the fixture's shape

`testFieldGetOwnPrivateFinalReferenceWithoutSetAccessible` is now `static` and
constructs its own receiver. Access is unchanged — the calling class is still
`ReflectionComplete`, which is what `Field.get`'s check reads — and CratonVM
returns 42.

The lesson generalises past this one entry: **a non-static fixture in this
corpus is measured against a different question than the oracle measures.**
`CorpusOracle` does `Modifier.isStatic(m) ? null :
getDeclaredConstructor().newInstance()`; `Vm::invoke` has no receiver
argument. Any future fixture that is not `static` will reproduce this exactly.

### 5. The finalizer entry

Closed before this branch; deleted with the rest. No code change.

## The twelfth: a stale expectation, not a gap

`ReflectionComplete.testMethodInvokePrivateViaReflection` expected
`Method.invoke` to throw `IllegalAccessException` when a class reflects on its
**own** private static method, and to return 49 only after
`setAccessible(true)`. A class may always reflect on its own private members.
Measured on Temurin 25.0.4, the old shape returns **-1** and the corrected
shape returns 49; CratonVM's `Method.invoke` agrees with HotSpot on both, and
so did the pre-branch binary.

The fixture had been passing for the wrong reason and stopped when dev made
that check caller-sensitive (`caller_may_access_member`, `lang_class.rs`). The
identical correction, with the identical reasoning, is already written on
`testFieldGetPrivate` two methods below it in the same file — the field form of
the question was fixed on 2026-08-02 and the method form was left behind.
Corrected the same way.

This is why check 1 of the pinning protocol is not optional: **CratonVM
agreeing with a fixture is not evidence the fixture is right.**

## Verification

| run | result |
|---|---|
| pristine dev, 11 pinned | 922 passed, 2 failed (one real, one unexpected-pass) |
| this branch, list empty, `--test-threads=1` | 924 passed |
| this branch, list empty, default parallelism | 924 passed |
| after merging dev (`147740f0e`), both thread counts | 924 passed |

Alongside: `cratonvm-classloading` default and `--features synthetic-jdk`;
`cratonvm-native-builtins --lib` synthetic (3,589) and default (3,406);
`cratonvm-native-collections --lib` synthetic (111); `cratonvm-vm --test
tier1_tests --features synthetic-jdk` (58, including the
`t9c_synthetic_field_tables_cover_their_factories` gate that ties
`synthetic_stub_fields` to every `alloc_concurrent_synthetic` factory site);
`cratonvm-types --test doc_citation_paths`; and a default-feature
`cratonvm-cli` build, since the real-JDK path must be untouched.

**Not fixed here, and not caused here** — closed separately the same day, see
fixed-bugs/vm-lib-tests-did-not-compile-under-synthetic-jdk-FIXED-20260811.md:
`cargo test -p cratonvm-vm --lib --features synthetic-jdk` did not compile on
dev. `MonomorphicInlineCache::update` (`jit/src/lib.rs`) grew a fifth
`jdk_only: bool` parameter and the ten call sites in `vm/src/vm/tests.rs` were
not updated (E0061). The feature flag is load-bearing in that sentence: the
module is `#[cfg(all(test, feature = "synthetic-jdk"))]`, so a DEFAULT `--lib`
run compiles and passes 2,487 tests without ever seeing it. Confirmed against a
pristine checkout; this branch touches neither file.

Plus a `--synthetic-jdk` probe (`GapProbe`) covering all nine observations,
run against Temurin 25.0.4 and against CratonVM before and after, so each
assertion has an oracle line beside it rather than a "returns 0".

## Why the list stays

`KNOWN_SYNTHETIC_JDK_GAPS` is kept, empty. An empty list still fails the run on
any mismatch, and adding an entry back costs one line of deliberate
bookkeeping — which is the whole design. The predecessor corpus reported a
green `924 passed` while running none of itself; a baseline whose entries can
close silently leaves the next regression with nothing to fail against.

The gate has now paid for itself twice: five proxy/annotation tests came off
the list when it reported them passing, and `testNoFinalizeOnLive` came off the
same way — reported closed before anyone went looking for a cause.

## Reproducing

```bash
CRATONVM_RUN_EXTENDED_INTERPRETER_TESTS=1 cargo test --release -p cratonvm-vm --features synthetic-jdk --test interpreter_tests
```

For a fast loop on any single behaviour, build the launcher with the same
feature and run a probe directly — seconds per iteration instead of a corpus
run, and it prints values rather than a boolean:

```bash
cargo build --release -p cratonvm-cli --features synthetic-jdk --bin cratonvm
```
