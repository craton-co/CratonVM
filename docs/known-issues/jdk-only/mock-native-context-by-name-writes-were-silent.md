# `MockNativeContext::set_field_by_name` was a silent no-op for most modelled classes

**Status:** FIXED 2026-08-05. Reads and writes first, then the third entry
point — see §3, whose "moves five unrelated tests" turned out to be two, and
whose real blocker was somewhere else entirely.

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
