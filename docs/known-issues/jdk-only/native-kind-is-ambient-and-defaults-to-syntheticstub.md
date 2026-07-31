# `NativeKind` is ambient state, not a `register()` argument — and it defaults to `SyntheticStub`

**Status:** OPEN — JDK-only wave-2 work item, filed 2026-07-31. **DANGEROUS:
causes silent misclassification, not a clean failure.** This is the most likely
mechanical root cause of mis-tagged permanent bridges inside the 157-entry
`synthetic-stub` baseline, and it has already produced one production boot
regression (see *Prior occurrence* below).

## What is wrong

A native's `NativeKind` is never stated at its registration site. It is
inherited from a mutable field on the registry that the *enclosing* registrar
function happens to have set.

`native-api/src/registry.rs`:

```rust
pub fn register(
    &mut self,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    callback: NativeCallback,
) {
```

— four arguments, none of them a kind. The kind comes from
`self.current_category`, which is:

* declared as a plain field (`current_category: NativeKind`, next to the
  index-parallel `categories: Vec<NativeKind>`);
* initialised in `new()` to **`NativeKind::SyntheticStub`**;
* mutated by `set_category(kind)` and by the scoped
  `with_category(kind, |r| { ... })`.

The type's own doc comment states the intent explicitly:

> The registry's `current_category` defaults to `SyntheticStub` — the
> conservative choice, so anything an author forgets to tag stays visible to the
> audit and gateable, never silently trusted.

The intent is defensible. The consequence is not: a genuine
`NativeKind::Bridge` registered from a helper that is called *outside* any
`with_category(Bridge, …)` scope — or after a `set_category` that was never
restored — is recorded as `SyntheticStub` with no diagnostic, and every
downstream consumer believes it.

That matters because `register()` does not merely *label* a stub. It **drops**
it:

```rust
if self.drop_synthetic_stubs && self.current_category == NativeKind::SyntheticStub {
    …
    return;
}
```

So under `CRATONVM_NO_STUBS` — and under `--jdk-only`, which the contract
defines as a stricter superset of the same rule — a mis-tagged bridge is not
registered at all. The method then falls through to real bytecode that may not
exist, or to a `NoSuchMethodError`, at a point far from the registration.

## Prior occurrence — this is not hypothetical

The drop path carries its own historical note (`native-api/src/registry.rs`,
inside `register`):

> `CRATONVM_DBG_DROPPED_STUBS=1`: list every registration this mode silently
> drops. Added 2026-07-14 while chasing a real-JDK-mode bootstrap regression
> (`InternalError: null property: java.home`) that traced back to a whole
> `register_*` function's worth of permanent bridges (`java.util.Properties`'
> side-table natives) being mis-tagged `SyntheticStub` by inheriting the wrong
> ambient category at one of its call sites — this made the drop visible in
> seconds instead of a multi-round bisection.

One ambient-category mistake cost an entire registrar's worth of permanent
bridges and presented as an unrelated bootstrap `InternalError`. There is no
reason to believe 2026-07-14 was the only instance; it is the only one that
happened to break the boot loudly enough to be chased.

## Scale

`set_category(` / `with_category(` appear **1,174 times across 134 files**
(ripgrep over the workspace, 2026-07-31), concentrated in `native-collections`
(103), `native-io/src/lib.rs` (31), `native-builtins/src/vector_api.rs` (24),
`native-builtins/src/util_concurrent_ext.rs` (17) and `vm/src/vm/vm_init.rs`
(12). Every one of those is a scope whose *interior* — including anything it
transitively calls — silently adopts a kind.

## Why it was not fixed in wave 1

Contract §8 says explicitly: *"Do not edit `native-builtins/src/lib.rs`; the
157-stub reclassification is a separate wave with its own subsystem-per-PR
discipline."* Reclassifying is also not a refactor that can be done blind — it
requires knowing, per registration, whether the tag was *chosen* or *inherited*,
and nothing in the tree records that today.

## What specifically must change

1. **Capture provenance before reclassifying anything.** The wave-1 contract
   (§4) already specifies the mechanism: `NativeCensusEntry.registered_by:
   Option<String>`, populated with `#[track_caller]` +
   `core::panic::Location` rather than a string built per registration. Until
   that column exists, "tagged `SyntheticStub` deliberately" and "registered
   outside any category scope" are indistinguishable in the census.
2. Add an explicit-kind entry point (`register_with_kind(class, method, desc,
   cb, kind)`) and migrate registrars to it subsystem by subsystem, so the kind
   is a local fact rather than a property of the call stack.
3. Only then reclassify. Flip the default last: once every registration states
   its kind, `current_category` can default to something that fails loudly (or
   be deleted).

## How to verify a fix

* `native-builtins/tests/stub_ratchet.rs` asserts
  `BASELINE_SYNTHETIC_STUBS = 157` **exactly**, with `SLACK = 0`. Any
  reclassification changes that number; re-baseline it to the count the test
  itself prints, and keep `SLACK` at zero.
* The same file's `essential_registry_is_populated` asserts only
  `total >= MIN_TOTAL_REGISTRATIONS` (`8_000`). That is a vacuity floor, **not**
  a claim about the exact total — do not treat any specific total as a verified
  invariant.
* `CRATONVM_DBG_DROPPED_STUBS=1` on a real-JDK boot lists every registration the
  strict path removes. A reclassification is wrong if this list gains an entry
  whose method the boot then needs.
* Per-subsystem: run with `CRATONVM_NO_STUBS=1` before and after. A newly-broken
  boot means a bridge was mis-tagged; a newly-*working* path that used to return
  a fake means a stub was correctly unmasked.

## Blast radius if done wrong

* **Stub → Bridge (too generous):** the fake survives `--jdk-only`. Contract §11
  ("final native registry contains zero `SyntheticStub` entries", "zero
  synthetic-stub invocations through any path") becomes false while the census
  reports green. This is the failure mode that makes the whole gate worthless,
  and it is invisible.
* **Bridge → Stub (too strict):** exactly the 2026-07-14 regression — a whole
  registrar disappears under `CRATONVM_NO_STUBS` / `--jdk-only` and surfaces as
  an unrelated error much later in boot.

Because both directions are silent at the point of the mistake, reclassification
must be done in reviewable subsystem-sized batches with the ratchet re-run each
time, never as a bulk sweep.

## Evidence needed that we do not have

Which of the 157 baseline entries are *deliberate* stubs and which merely
inherited the default is **not answerable from the current tree**. It needs the
schema-2 census with the `registered_by` column populated (contract §4), taken
from a real-JDK boot. Do not guess a per-entry disposition before that data
exists.
