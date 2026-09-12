# JIT verdicts and the IR refusal memo are still keyed by class name, not by loader

**Status:** OPEN (degraded tiering in multi-loader deployments, not a
correctness bug). Residual of the 2026-09-12 review finding "Tier state, OSR
denials and unload invalidation are keyed by class name, not loader".

## What was fixed

- `MethodKey` in `jit/src/tiered.rs` carries the declaring class's `ClassId`.
  Tier state, OSR denials and unload invalidation match by identity.
- The background OSR compile resolves its inputs by `ClassId` first, and its
  redefinition probe consults the id as well as the name.
- Verdicts expire when the process redefine epoch or the install epoch moves.
  They are forgotten per class on unload and redefinition.

## What is left

The bail list, bail reasons and OSR-entry rejects (`JIT_VERDICTS` in
`jit/src/lib.rs`), and the IR refusal memo, are keyed by a hash of
`(class name, method, descriptor)` with the class id left at `ClassId(0)`.
The public recording functions still take names only.

Consequences:

- Two same-named classes in different loaders share verdicts. If one webapp's
  copy of `com.acme.Foo.m()` is bail-listed, the other webapp's copy of the
  same name is treated as bail-listed too. Examples are two webapps, a devtools
  restart loader, and Groovy or JSR-223 scripts. The result is a method that
  stays interpreted, not a wrong answer.
- `forget_jit_verdicts_for_class(name)` drops every loader's entries for that
  name. The redefinition or unload of one copy therefore also clears the
  other's verdicts. That direction is benign, since a verdict is only a
  refusal to retry.

## The fix

Thread the `ClassId` through the verdict API:

- `mark_jit_bailed`, `jit_bail_reason`, `mark_osr_entry_rejected_by` and
  `ir_refusal_memo_key` gain a `class_id` parameter.
- Every caller already holds a `MethodKey` or a `CachedBytecodeMethod` that
  carries the id.
- Keep the stored full names for hit verification.
- `forget_jit_verdicts_for_class` becomes by-id.
