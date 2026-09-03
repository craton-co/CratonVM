# `TestRandomMapOps` returns the wrong answer deterministically — `(1810, null)`

## Status

**OPEN, bisected to one commit, 2026-09-03.** Not root-caused. This is a NEW
regression, distinct from every other failure on this workload, and it is the
easiest of them to work on because it is **deterministic**.

## The repro

```
cratonvm --java-home $JDK25 --Xmx 256m -c "$H2_CP" \
  org.h2.test.store.TestRandomMapOps
```

Fails in **13-23 seconds, 3 runs out of 3**, always identically:

```
seed:0 op:1033 java.lang.AssertionError: (1810, null)
```

`seed:0` — the test's own first, fixed seed. Same op, same expected value, same
`null`, every run. A map `get` that should return 1810 returns nothing.

**This is a WRONG ANSWER, not a crash**, and it is the third of the three faces
`bug-h2-testrandommapops-small-heap-corruption-20260829.md` describes. Unlike
that page's failures it needs no quiet host, no luck and no soak.

## It is not the collector

| arm | result |
|---|---|
| default | AssertionError 3/3, 13-18 s |
| `CRATONVM_ZGC_RELOCATE=0` | AssertionError 3/3, same `seed:0 op:1033` |

Relocation off changes nothing — not the timing, not the op, not the value. So
this is not the relocation defect the sibling pages chase, and it is not a race:
turning off the moving collector would perturb any timing-dependent failure and
it perturbs nothing.

## The bisect

Range `221a383f2..08a1711e5` (92 commits). Good endpoint re-verified in the same
build profile first. BAD is the exact string `AssertionError: (1810, null)`; the
150 s cap keeps the pre-existing flaky `NullPointerException` and the
fragmentation `OutOfMemoryError` (both minutes-scale) out of the signal.

**First bad commit: `b4f2e8042`** — a merge that hand-resolved conflicts in

```
jit-api/src/helpers_abi.rs
jit-api/src/lib.rs
```

Two branches had each appended a field to the JIT runtime-helper table:
`ref_store_post_skip_mask` (from dev) and `tlab_registration_required` (from
`perf/jit-eight-findings-20260902`). The resolution kept both, bumped
`JIT_HELPERS_ABI_VERSION` 12 → 13 and the field count 75 → 76.

The resolution LOOKS right and may well be: both fields appear in the same order
in the struct and in the ABI table, so the offsets agree, and
`cargo test -p cratonvm-jit-api` (56 tests, including the ABI revision and
golden-offset checks) is green. That the bisect lands on a merge of two
independently-good branches is the same shape as
`known-issues/jit/bug-box-unbox-intrinsic-segv-under-relocation-20260902.md`,
whose first bad commit is also a merge with both parents good.

**Not yet done: testing the two parents.** `6a1ab7de0` and `c3d4da8a8` are each
two builds away and would say whether this is a resolution defect or an
interaction. That is the next step and it is mechanical.

## What has been ruled out

Each of these is 3 runs, same binary, same host:

| switched off | AssertionError |
|---|---|
| `CRATONVM_JIT_BOX_UNBOX_INTRINSIC` (i.e. the shipped default) | **3/3 — still fails** |
| `CRATONVM_ZGC_RELOCATE` | 3/3 |
| `CRATONVM_JIT_GATED_REF_STORE` + `IR_GATED_REF_STORE` + `IR_REF_STORE` + `GC_JIT_REF_STORE_GATES` | 3/3 |
| `CRATONVM_NO_JIT_INLINE_TLAB_NEW=1` | 3/3 |

The ref-store family was the strongest guess — `829985d59 perf(jit): the
single-pass reference store gets both cell shapes` is in the range, and a
reference stored to the wrong cell shape reads back as `null`, which is exactly
the symptom. It is not it.

The inline-TLAB guess came from a real code/comment contradiction found on the
way, and worth fixing on its own account even though it is not this bug:
`gc/src/zgc.rs` publishes

```rust
cratonvm_types::set_jit_tlab_registration_required(heap.vm_tlab_enabled());
```

under a comment that says the requirement is unconditional — *"This collector
finds objects through its allocation-base registry, never by walking a TLAB
chunk, so the JIT's inline allocator must keep calling the post-init helper that
announces each one."* The registry requirement does not depend on whether the VM
TLAB is enabled. Disabling the inline-TLAB `new` does not fix THIS defect, so
the two are independent.

## Why this one is worth doing first

It masks the others. Any investigation of the box/unbox SIGSEGV on current dev
dies of this in ~15 s, well before that crash's 25-183 s window — one arm of that
investigation was voided exactly this way. And a deterministic wrong answer at a
fixed seed and a fixed op is the cheapest kind of defect to bisect INSIDE the
VM: the same op fails every time, so a trace of op 1033 can be diffed against a
good build directly.
