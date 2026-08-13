# `MockNativeContext::set_field_by_name` was a silent no-op for most modelled classes

**Status:** RETIRED 2026-08-06. Reads and writes first, then the third entry
point — see §3, whose "moves five unrelated tests" turned out to be two, and
whose real blocker was somewhere else entirely. Then §4–§7, 2026-08-06: the
same defect with the sign flipped (the mock answering a *different* slot rather
than none), a fourth entry point nobody had counted, and two reader/writer
splits — all measured, all gated.

## What was wrong

`native-builtins/src/test_utils.rs` resolves a field name to a slot through a
chain of hand-written `mock_*_field_slot` helpers — one per class family someone
needed. For a class with no helper, `set_field_by_name` resolved `None` and
**silently did nothing**, and `get_field_by_name` returned `Int(0)`.

That is the documented behaviour of the real VM for a name the class does not
declare. The problem is that these classes *do* declare it: they have a
fabricated model in `ClassManager::synthetic_stub_fields`, which is exactly what
the VM resolves names against when a class has no real bytes. The mock was
answering "no such field" for fields the VM resolves.

## Why it matters — the pattern it hid

`native-builtins` is full of **dual writes**: a native writes a field by raw
slot index for the fabricated layout, then again by name for the real-JDK
layout. Under the mock the by-name half did nothing, so **every such native was
tested on its raw half only** — the half that is wrong whenever the model and
the image disagree.

Found while fixing the `java.security.ProtectionDomain` slot rotation. The
populator wrote slots 0..3 in the constructor's argument order and then the same
four by name, relying on the by-name pass running last to correct the first
three. Removing the raw pass — provably dead in the VM — turned
`t19_n1_class_get_protection_domain0_with_code_source_returns_pd` red, because
under the mock the raw pass was the only pass there was.

The same test also asserted that `CodeSource`'s slot 0 held a **String**. The VM
writes a String there by raw index and then a `java.net.URL` by name, and the
URL wins on both layouts — so the String the test expected is a value the VM has
not produced for as long as both writes have existed. The expectation was frozen
around the mock's blind spot, not around the code.

## What changed

`get_field_by_name` and `set_field_by_name` now fall back to
`mock_stub_model_field_slot`, which resolves against the production
`synthetic_stub_field_model(class_name)`. Every hand-written mapping still wins;
the fallback only fires where the mock previously said "no such field".

Blast radius: exactly one test, updated to assert what the VM produces.

## 3. `resolve_field_index_by_class_id` — fixed, and what it actually cost

The same fallback belongs on `resolve_field_index_by_class_id`, whose own doc
comment already made the argument ("a predicate of the form *does this class
declare a field only the REAL JDK class has* is unfalsifiable under the mock").
This page deferred it as "moves five unrelated tests … its own change with its
own investigation". Doing the investigation:

**It moved two, not five.** The three `xnio_worker` cases were fixed on `dev` in
the interim. The survivors were
`lang_class::tests::c5_field_get_declaring_class_returns_declared_not_object`
and its `c6_method_*` sibling.

**And they did not move for the reason this page assumed.** The problem was not
"production takes a different branch once the lookup answers `Some`". It was
that `mock_jdk_field_slot` — a deliberately arbitrary shared name→slot namespace
for the `Field`/`Method`/`Constructor`/`MemberName` mirrors — **disagrees with
the fabricated model on every one of those names**, and
`get_field_by_name`/`set_field_by_name` consult it FIRST. Adding the model to
`resolve_field_index_by_class_id` without matching that order gave a reader and
a writer two different answers for the same name: `create_method_object` wrote
`modifiers` to one slot and `method_modifiers_value` read it from another.

Putting the model ahead of the hand-written namespace in the by-name chains
instead — the "obvious" reconciliation — is worse, not better: it swaps which
two tests fail and adds a third (`g2_create_method_object_populates_parameter_types_non_null`),
because production carries a FOURTH mapping of its own
(`METHOD_LEGACY_SLOT_*` in `lang_class.rs`) as a fallback for exactly these
reads.

**The fix is one line and an ordering constraint.**
`resolve_field_index_by_class_id` now ends with the same tail the by-name chains
use, in the same order: `mock_undertow_exchange_field_slot` →
`mock_jdk_field_slot` → `mock_stub_model_field_slot`. The hand-written namespace
keeps winning where it exists, because a reader and a writer that disagree is
worse than either mapping being "wrong"; the model fills the genuine gaps, which
is what the fallback was for. `native-builtins --lib` is 3273/3273.

Two tests in `classloader.rs` pin it, and the first was verified to go **red**
without the fallback:

* `resolve_by_class_id_sees_the_fabricated_model` — `ProtectionDomain`'s four
  modelled fields resolve to their modelled slots, and a name the class does not
  declare still answers `None` (a fallback that answered `Some` for everything
  would be as useless as one answering `None`).
* `the_hand_written_namespace_still_wins_for_the_reflect_mirrors` — pins the
  ordering, so the reconciliation that looks tidier cannot be applied without
  the test that measured it failing.

## How to check whether a native is affected

A native whose fields are written twice — once by index, once by name — has
untested by-name behaviour under the mock unless its class has a
`mock_*_field_slot` helper or a `set_declared_fields` call in the test. Grep for
`set_field_by_name` next to `set_field(` on the same receiver; the pairs in
`lang_class.rs` and `security_manager.rs` are the dense ones.

## 4. The other direction — and it was live on twenty-eight (class, field) pairs

Everything above is about the mock answering *"no such field"* for a field the
VM resolves. The mirror-image mistake was never looked for: the mock answering
**a different slot** for a field the VM resolves.

`mock_jdk_field_slot` is the deliberately arbitrary flat namespace for the
`Field` / `Method` / `Constructor` / `MemberName` mirrors. It takes no class
argument, and it was consulted **ahead of** `mock_stub_model_field_slot` — so it
answered for *any* modelled class that happens to declare one of its fifteen
names. Measured across the 333 classes `synthetic_stub_field_model` models:

| what | rows |
|---|---:|
| `name` shadowed at slot 1 where the model says 0 | 26 |
| `java.util.logging.Logger.name` (model slot 2) | 1 |
| `io.undertow.server.HttpServerExchange.responseHeaders` (mock 3, model 5) | 1 |
| the reflect mirrors themselves — deliberate, see below | 20 |

The twenty-seven `name` rows are `java.lang.Enum`, `java.security.Permission`
and `BasicPermission`, `java.util.PropertyPermission`,
`java.util.jar.Attributes$Name`, `javax.naming.Binding`,
`javax.management.ObjectInstance`, `javax.security.auth.login.LoginContext`,
`java.nio.charset.CodingErrorAction`, `org.xnio.Xnio` / `XnioWorker` /
`NioXnioWorker`, `org.jboss.modules.Module`, `org.jboss.logmanager.Logger` /
`Level`, `org.jboss.msc.service.ServiceController`,
`org.jboss.threads.EnhancedQueueExecutor`, and eight more. Any native writing
`name` by name on one of those was tested against a slot the VM does not use.

**The mirrors keep the flat namespace, and the exemption is enumerated rather
than implied** (`mock_reflect_mirror_field_slot`). Production keeps that layout
too: `create_method_object` writes the `METHOD_LEGACY_SLOT_*` indices and every
reader goes through `method_*_field_value_or_legacy`, which falls back to
exactly those indices. The mirror is allocated at
`METHOD_NUM_FIELDS_LEGACY_FLOOR = 8`, so the model's slot for `Method.modifiers`
(10) is past the end of the object and `set_field` would discard the write in
silence. That is the measured reason the "obvious" reordering turned three tests
red in §3, and it applies to six classes, not to every class.

## 5. Two reader/writer splits, found by unifying the chains

§3 fixed `resolve_field_index_by_class_id` by giving it the same tail as the
by-name chains. There were **four** entry points, not three, and they had drifted
in two more places:

* **`java/lang/reflect/Parameter.name` answered two slots.**
  `get_field_by_name` / `set_field_by_name` special-cased the class and used
  `mock_parameter_field_slot` (slot 0); `resolve_field_index_by_class_id` did
  not, and fell through to `mock_jdk_field_slot` (slot 1). One name, a writer,
  a reader, two slots — the exact thing §3's ordering constraint exists to
  prevent, sitting in the mock the whole time.
* **`resolve_field_index` — the fourth entry point — consulted ONE table.**
  `mock_undertow_exchange_field_slot`, and nothing else, so it answered `None`
  for every other class in the tree. Production reaches for it constantly:
  `java/lang/Enum.name`, `java/lang/Throwable.detailMessage`,
  `java/lang/StackTraceElement.declaringClass`, and the whole
  `jdk.internal.foreign` memory-segment family. Every branch behind those calls
  was unreachable under the mock — the same unfalsifiable predicate §3 fixed,
  one entry point over.

All four now resolve through one function, `mock_field_slot`, in one order:
the mirrors, then the production model, then the hand-written per-class tables
(which model *real* library classes the fabricated model does not describe — a
real Undertow exchange has ~30 fields, the model is a seven-field stand-in),
then the class-blind namespace last.

## 6. Gates, each shown to fail

In `native-builtins/src/classloader.rs`, next to §3's two:

* `the_mock_slot_tables_do_not_shadow_the_fabricated_model` — harvests the class
  names out of `class_manager.rs` itself (a hand-written list goes stale in
  silence, which is this record's whole subject), resolves every modelled
  instance field, and requires the mirrors to be the only disagreement. **27
  failures on the unfixed tree.**
* `resolve_field_index_by_name_sees_the_fabricated_model` — and it is still
  falsifiable downward: a name no class declares must still answer `None`.
* `every_by_name_entry_point_resolves_one_name_to_one_slot` — reported
  `Some(1)` vs `None` for `Parameter.name` on the unfixed tree.

`cargo test --release -p cratonvm-native-builtins --lib`: 3302 passed, 0 failed.

## 7. What this still cannot see

`java.lang.Throwable` has real class bytes and therefore **no fabricated
model**, so `resolve_field_index("java/lang/Throwable", "detailMessage")` is
still `None` and `lang_misc.rs`'s three resolutions of it remain unfalsifiable
under the mock. A first draft of the gate asserted otherwise and went red, which
is the useful way to find out. The chain is now as good as the model is; where
the model says nothing, the mock still cannot.
