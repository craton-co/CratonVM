# `MockNativeContext::set_field_by_name` was a silent no-op for most modelled classes

**Status:** half FIXED 2026-08-05 (reads and writes); the third entry point is
still open, deliberately — see §3.

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

## 3. Still open: `resolve_field_index_by_class_id`

The same fallback belongs on `resolve_field_index_by_class_id`, whose own doc
comment already makes the argument ("a predicate of the form *does this class
declare a field only the REAL JDK class has* is unfalsifiable under the mock").
It is **not** wired, because doing so moves five unrelated tests:

* `lang_class::tests::c5_field_get_declaring_class_returns_declared_not_object`
* `lang_class::tests::c6_method_get_declaring_class_returns_declared_not_object`
* `xnio_worker::tests::wf_domain_create_tcp_connection_server_returns_accepting_channel`
* `xnio_worker::tests::wf_domain_real_nio_worker_is_adopted_without_slot_handle`
* `xnio_worker::tests::wf_domain_stream_connection_worker_identity_uses_io_thread_mirror`

Those move because production code takes a different branch once the lookup
answers `Some`, which is the whole point — each needs reading to decide whether
the new behaviour or the old assertion is right. That is its own change, not a
rider on a layout fix.

## How to check whether a native is affected

A native whose fields are written twice — once by index, once by name — has
untested by-name behaviour under the mock unless its class has a
`mock_*_field_slot` helper or a `set_declared_fields` call in the test. Grep for
`set_field_by_name` next to `set_field(` on the same receiver; the pairs in
`lang_class.rs` and `security_manager.rs` are the dense ones.
