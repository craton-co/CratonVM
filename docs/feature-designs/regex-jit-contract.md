# Compiled regex and dispatch-chain contracts

Real `Pattern` and `Matcher` bytecode follows the foundational OpenJDK execution
policy. `CRATONVM_REGEX_MATCHER_BYTECODE` defaults on: real-layout
`Matcher.find`, `start`, `end`, and `group` natives yield to JDK bytecode.

## Inlining and loop proofs

The optimizing tier can splice profile-guarded dispatch chains, including nested
calls with the same selector. A receiver guard must select the actual profiled
callee and preserve the original dispatch or deoptimization path on mismatch.
Loop-invariant field, class, coder, and backing-array facts may be reused only
while their alias and memory-token proofs hold. These are general compiler
rules; regex class names do not authorize bypassing them.

## Frames and roots

Primitive-colored frame slots can be omitted from fresh-frame clearing only
when the compiled body publishes its primitive-offset claim. The collector must
honor that claim at every poll, including polls without a current safepoint id.
Unpublished or rootable slots retain the normal clearing and scanning rules.

The owning implementations are `jit/src/ir_lower.rs`, `jit/src/ir_optimize.rs`,
`jit/src/lib.rs`, and `types/src/flags.rs`. See [profile-guided inlining](profile-guided-inlining.md),
[precise maps](precise-jit-maps-default.md), and [loop transforms](../jit/loop-transforms.md).
