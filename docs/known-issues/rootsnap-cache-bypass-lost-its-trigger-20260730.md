# The root-snapshot cache bypass stopped firing when real ForkJoinPool became the default

| | |
|---|---|
| **Status** | OPEN. Behaviour is unchanged from before the default flip; what is missing is the evidence to decide which way it should go. |
| **Category** | GC-CORRECTNESS / THROUGHPUT (root snapshot caching, native re-entry) |
| **Found** | 2026-07-30, verifying the deep-audit dispositions against the code. |

## The mechanism

`vm/src/runtime/env_cache.rs` exposes `real_forkjoinpool()`, built with
`cached_is_set!`, i.e. it is true iff `CRATONVM_REAL_FORKJOINPOOL` is *present*
in the environment. Two sites read it:

- `vm/src/runtime/interpreter.rs`, `update_root_snapshot` — the frozen-frame
  cache is used only when `rootsnap_cache() && !conservative_locals &&
  !real_forkjoinpool()`.
- `vm/src/runtime/interpreter.rs`, `remap_rs_cache_after_gc` — the cache is
  cleared outright when `real_forkjoinpool()`.

The intent, stated in the comments at both sites, is "in the real ForkJoinPool
lane, do not trust the frozen-frame cache": those native overrides recursively
re-enter Java from `fork`/`join`/`submit`, and frames that look prefix-stable to
the cache can still expose changing local/operand roots around the native
returns.

That predicate was written when real ForkJoinPool was **opt-in**, so "the env
var is set" and "we are in the real lane" were the same statement.

Real ForkJoinPool is now the **default** (`flags().natives.real_forkjoinpool`,
default `true`, with `CRATONVM_SYNTHETIC_FORKJOINPOOL` as the opt-out). Nobody
sets `CRATONVM_REAL_FORKJOINPOOL` any more. So the predicate is false on exactly
the configuration it was written to catch, and **the bypass has not fired on a
default run since the flip**.

`conservative_locals_enabled()` does not cover the gap. It reads the resolved
flag, but it is additionally gated on `cratonvm_gc::gc_quiescence::is_active()`
(`vm/src/memory/roots.rs`), so it is not universally true either.

## Why this is filed rather than fixed

The obvious fix is to make the predicate honest:

```rust
pub fn real_forkjoinpool() -> bool {
    cratonvm_types::flags::flags().natives.real_forkjoinpool
}
```

**That was tried and it is wrong as a standalone change.** The flag is
default-true, so the bypass then fires *always*, the frozen-frame cache becomes
dead code on every run, and
`runtime::interpreter::root_snapshot_cache_tests::local_write_invalidates_cached_deep_frame_roots`
fails with 0 cached deep-frame roots where it expects 2. Measured, not
predicted. The cache exists for deep-stack native-heavy workloads and was
introduced to fix a real hang; silently retiring it to close a predicate bug is
a bad trade made without data.

So there are two coherent positions and no evidence to choose between them:

1. **The hazard is now universal.** Real FJP is always on, so the cache is
   always unsound and should be removed along with its test.
2. **The hazard was specific to the opt-in lane** — for instance because it only
   materialises when FJP worker threads are actually running — and the bypass
   needs a narrower, dynamic trigger rather than a process-global flag.

Position 2 is more likely to be right, because the bypass predates the flip and
nothing in the intervening period reported the root-corruption it guards
against. But "more likely" is not evidence.

## What would settle it

Run the GC-stress workloads that motivated the bypass with the cache forced on
in the real lane, under `CRATONVM_ROOTSNAP_CACHE` plus GC stress, and see
whether roots are lost. If they are, position 1 wins and the cache goes. If they
are not, the bypass wants a trigger tied to actual FJP worker activity rather
than to a process-global flag.

## The generalisable lesson

After a default flip, **every `is_set`/presence test on the old opt-in
environment variable is a suspect**. It does not fail loudly; it silently starts
answering "was this explicitly requested?" when the caller is asking "is this
active?", and those two questions had the same answer right up until the flip.
`grep` for `cached_is_set!` and `runtime_var_os(...).is_some()` against any flag
whose default has moved.

This one was found by re-reading the code against a claim, not by a test —
there is no gate for "a predicate quietly changed meaning".
