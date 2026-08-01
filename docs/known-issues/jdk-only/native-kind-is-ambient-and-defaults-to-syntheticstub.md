# `NativeKind` is ambient state, not a `register()` argument — and one line can mis-tag a thousand registrations

**Status:** OPEN — JDK-only wave-2 work item, filed 2026-07-31, re-verified
against the re-landed tree the same day. **DANGEROUS: causes silent
misclassification, not a clean failure, and it misclassifies in both
directions.**

## What is wrong

A native's `NativeKind` is never stated at its registration site. It is
inherited from a mutable field on the registry that the *enclosing* registrar
function happens to have set.

`native-api/src/registry.rs`, `NativeMethodRegistry::register` (currently
~4667, `#[track_caller]`):

```rust
#[track_caller]
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

* declared as a plain field (`current_category: NativeKind`, ~4351);
* initialised in `new()` to **`NativeKind::SyntheticStub`** (~4480);
* mutated by `set_category(kind)` (~4553) and by the scoped
  `with_category(kind, |r| { ... })` (~4566).

The type's own doc comment (~4125) states the intent explicitly:

> The registry's `current_category` defaults to `SyntheticStub` — the
> conservative choice, so anything an author forgets to tag stays visible to the
> audit and gateable, never silently trusted.

The intent is defensible. The consequence is not, and it runs in **both**
directions.

## Direction A — the dominant one: wholesale over-tagging as `Bridge`

This is the finding that inverts the original premise of this record. It came
out of the ambient-category audit
([`docs/jdk-only-ambient-category-audit.md`](../../jdk-only-ambient-category-audit.md))
and is recorded in code as a `JDK-ONLY-CLASSIFY` verdict on
`register_collections_natives` in `native-collections/src/lib.rs` (~1484):

> **JDK-ONLY-CLASSIFY: stub — crate-wide verdict, applied by one line.** The
> `set_category(Bridge)` below is not a per-registration judgement: it is a
> dynamic ambient assignment that every callee in this file inherits, and it
> covers 1,195 of the crate's 1,219 registrations. Cross-checked against JDK 25
> with `javap -p -s`, **not one** of those 1,195 targets an `ACC_NATIVE` method
> (622 target methods with concrete bytecode, 214 are abstract interface
> methods, the remainder are absent from the image or use a class name this
> audit could not resolve statically). `java.util` is pure Java: there is no
> VM/OS boundary in this crate to bridge to.

One `set_category(Bridge)` line, at the head of `register_collections_natives`,
tags 1,195 registrations. Contract §1.5 defines a `Bridge` as what an
`ACC_NATIVE` method binds to; none of these are. Contract §1.4 lets a `Bridge`
lose to concrete bytecode, so `Compatible` mode is not wrong today — but the
census says "1,195 legitimate bridges" where the truth is "1,195 registrations
nobody adjudicated", and `--jdk-only` admits every one of them.

The same marker states the constraint on fixing it, and it must be respected:

> **DO NOT flip this line to `SyntheticStub` as a bulk edit.** That is the exact
> shape of the 2026-07-14 regression, at ~8x the blast radius, and the 214
> abstract-interface registrations additionally decide dispatch for every USER
> subclass, not just for `java.util` classes.

`JDK-ONLY-CLASSIFY` markers now exist in 18 files (12 in
`native-awt/src/natives.rs`, 7 in `native-collections/src/lib.rs`, 6 in
`native-io/src/lib.rs`, the rest one or two each across `native-io`,
`native-builtins-security`, `native-builtins-crypto` and `native-awt`). They are
the per-registrar verdicts; read them before touching a `set_category` line.

## Direction B — under-tagging, which drops the registration

`register()` does not merely *label* a stub. Under the strict policy it refuses
it (~4689), and under `CRATONVM_NO_STUBS` it drops it (~4721):

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

### Prior occurrence — this is not hypothetical

The drop path carries its own historical note inside `register`:

> `CRATONVM_DBG_DROPPED_STUBS=1`: list every registration this mode silently
> drops. Added 2026-07-14 while chasing a real-JDK-mode bootstrap regression
> (`InternalError: null property: java.home`) that traced back to a whole
> `register_*` function's worth of permanent bridges (`java.util.Properties`'
> side-table natives) being mis-tagged `SyntheticStub` by inheriting the wrong
> ambient category at one of its call sites.

One ambient-category mistake cost an entire registrar's worth of permanent
bridges and presented as an unrelated bootstrap `InternalError`.

## What has already been retagged — do not redo it

**JMX and `java.util.function.Function$Identity` are `NativeKind::Bridge`
today.** They are not in the residual 157. The retags are in
`native-builtins/src/jmx.rs` (`set_category(Bridge)` at the head of each
registrar) and `native-builtins/src/lib.rs`'s
`register_function_identity_natives` (~36914), which carries the reasoning:

> Tagging this `SyntheticStub` broke real-JDK-mode boot the moment
> `set_drop_synthetic_stubs(true)` started actually dropping `SyntheticStub`
> registrations (dev `d8092acb`, 2026-07-14): WildFly's very first
> `getRuntimeMXBean()`-adjacent lambda hit `UnsatisfiedLinkError:
> Function$Identity.andThen`. Tag as `Bridge` (needed in both modes) instead —
> same class of bug/fix as the `native-builtins/src/jmx.rs` JMX cluster retag,
> both landed together.

`vm/src/vm/vm_init.rs` (~1467) records the same conclusion from the contract's
side: these are *permanent bridges with no real-bytecode fallback*, which by §1's
terminology table makes them `Bridge`, not stubs.

### The successor defect: `Function$Identity` now survives a class §5 forbids

Retagging fixed the registration. It created a new inconsistency that nobody has
adjudicated:

* The five `Function$Identity` natives are now `Bridge`, so **`--jdk-only`
  registers and invokes them**.
* `java/util/function/Function$Identity` **has no class file anywhere**. The
  registrar's own comment says so: *"there is no real classfile named
  `Function$Identity` to fall back to at all."* It is minted by
  `alloc_concurrent_synthetic(ctx, "java/util/function/Function$Identity", 0)`
  (`native-builtins/src/lib.rs` ~36608, `phases_late/streams.rs` ~2898), which
  bottoms out in `ensure_synthetic_class`.
* Contract §5 forbids fabricating exactly that class under `JdkOnly`, and
  `ClassManager::try_ensure_synthetic_class`'s doc comment names it as the
  *chief* intended caller of the refusing entry point.

So under `--jdk-only` the surviving `Bridge` native is a bridge to a receiver
whose class the policy says may not exist. Today nothing breaks, because
`ensure_synthetic_class` records the violation and fabricates anyway (see
[`ensure_synthetic_class` cannot enforce](ensure-synthetic-class-cannot-enforce-only-record.md)) —
the two defects are cancelling each other out. Fixing either one alone exposes
the other. The real fix is upstream: `Function.identity()` should return the
real lambda, not a VM-minted stand-in.

## Scale

`set_category(` / `with_category(` appear **1,169 times across 123 files**
(ripgrep over the workspace, re-counted 2026-07-31 against the re-landed tree),
concentrated in `native-builtins/src/phases_early.rs` (121),
`native-collections/src/lib.rs` (104), `native-builtins/src/phases_late.rs` (75),
`native-builtins/src/phases_late/bouncycastle.rs` (68) and
`native-builtins/src/jmx.rs` (50). Every one of those is a scope whose
*interior* — including anything it transitively calls — silently adopts a kind.

*(The original record said 1,174 across 134 files. The difference is the
re-land plus the retags, not a measurement error in either direction; treat both
as "about twelve hundred".)*

## Why it was not fixed in wave 1

Contract §8 says explicitly: *"Do not edit `native-builtins/src/lib.rs`; the
157-stub reclassification is a separate wave with its own subsystem-per-PR
discipline."* Reclassifying is also not a refactor that can be done blind — it
requires knowing, per registration, whether the tag was *chosen* or *inherited*.

## What specifically must change

1. **Provenance is now captured — use it.** `register` is `#[track_caller]` and
   `NativeCensusEntry.registered_by` is populated from
   `core::panic::Location`, redacted unless `--explain-jdk-only`. The census
   can now distinguish "tagged deliberately at this line" from "inherited from
   a `set_category` three frames up". That was the blocking prerequisite in the
   original filing and it is met.
2. Add an explicit-kind entry point (`register_with_kind(class, method, desc,
   cb, kind)`) and migrate registrars to it subsystem by subsystem, so the kind
   is a local fact rather than a property of the call stack. **Start with the
   `JDK-ONLY-CLASSIFY`-marked registrars**, which already carry an adjudicated
   verdict.
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
  invariant. Its strict sibling asserts `strict_total >= 7_500` for the same
  reason: to catch a registry that reports zero stubs because it is empty.
* `strict_registry_has_zero_synthetic_stubs` passes today, and the test says why
  in terms worth keeping: *"not because the 157 stubs are gone, but because
  `register()` refuses them at the door under `JdkOnly`."* A reclassification
  that makes it pass for a different reason has changed the meaning of the gate.
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
  it is invisible, and — per the `native-collections` verdict above — **it is
  already the majority disposition in the tree**, not a hypothetical.
* **Bridge → Stub (too strict):** exactly the 2026-07-14 regression — a whole
  registrar disappears under `CRATONVM_NO_STUBS` / `--jdk-only` and surfaces as
  an unrelated error much later in boot.

Because both directions are silent at the point of the mistake, reclassification
must be done in reviewable subsystem-sized batches with the ratchet re-run each
time, never as a bulk sweep. The 1,195-registration `set_category` line is the
proof that a one-line "fix" here is a one-line thousand-registration change.

## Evidence needed that we do not have

Which of the 157 baseline entries are *deliberate* stubs and which merely
inherited the default is now **answerable** — `registered_by` and
`real_declaring_method` are both populated in the schema-2 census (see
[the observability record](observability-surface-has-three-unfilled-holes.md)).
What does not exist yet is the census *taken from a real-JDK boot* and the
per-entry adjudication built on it. Do not guess a per-entry disposition before
that run exists.
