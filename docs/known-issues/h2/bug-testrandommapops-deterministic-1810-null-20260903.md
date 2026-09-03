# `TestRandomMapOps` returns the wrong answer deterministically — `(1810, null)`

## Status

**MITIGATED 2026-09-03 (the door is default-OFF); root cause still OPEN.**
Narrowed from 92 commits to ONE feature switch: the monomorphic invoke fast
door. Not yet narrowed to a line. This is a NEW
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

## Narrowed to ONE door, and mitigated

`CRATONVM_DISABLE_JIT=1` runs clean to a 200 s cap while the default fails at
14 s, so it is the JIT. (`-Xint` does NOT test that: `normalize_java_launcher_argv`
STRIPS it — the launcher's own test asserts `["java","-Xint",...,"Main"]`
normalises to `["java","Main"]`. An `-Xint` arm on this VM is vacuous, and this
one was, until the real switch replaced it.)

From there, a binary search over the 54 JIT feature tokens added on 2026-09-02,
six rounds, each arm ~20 s because the failure is deterministic:

```
54 candidates
  26 of 53 disabled -> FAIL     narrowed to 27
  13 of 27 disabled -> CLEAN    narrowed to 13
   6 of 13 disabled -> CLEAN    narrowed to 6
   3 of  6 disabled -> FAIL     narrowed to 3
   1 of  3 disabled -> FAIL     narrowed to 2
   1 of  2 disabled -> CLEAN    narrowed to 1
CULPRIT -descriptor-facts
```

The invariant holds at both ends: all 54 disabled is clean, none disabled fails.

`descriptor-facts` turned out to be the wrong NAME for the culprit rather than
the wrong answer. Its flag comment says off merely *"routes `ParamTags::for_method`
and `Frame::return_tag` back through the per-call descriptor scans they
replaced"* — a pure relocation of work. It is not: `invoke_fast.rs` and
`dispatch_virtual.rs` also consult `descriptor_facts_disabled()`, and there it
is the ENABLING CONDITION for a whole fast path (`return None` declines the
door). A switch documented as a memo actually gates behaviour.

Testing the narrower door switches separated it:

| arm | result |
|---|---|
| `CRATONVM_JIT=-invoke-fast-door` | **clean to the 200 s cap** |
| `CRATONVM_JIT=-nonvirtual-fast-door` | fails, 12 s |
| `CRATONVM_JIT=-field-fast-path` | fails, 12 s |

So the defect is the **monomorphic `invokevirtual`/`invokeinterface` fast door**
(borrowed cache entry, verbatim `CompactValue` argument transfer), and
`-descriptor-facts` only helped because it switches that door off as a side
effect.

**Mitigation:** the door is now opt-in —
`CRATONVM_JIT_INVOKE_FAST_DOOR=1` turns it back on, which is how the root-cause
work should run it; `CRATONVM_JIT_NO_INVOKE_FAST_DOOR=1` still forces it off.
Verified with its own positive control in the same batch: default clean to
200 s, `CRATONVM_JIT_INVOKE_FAST_DOOR=1` fails at 12 s.

## Where the root cause is NOT

Checked and excluded inside the door, so the next person does not repeat them:

* **The two tokenisations agree.** An audit computing `ParamTags::of` and
  `from_facts` on every `for_method` and printing any disagreement reported
  ZERO — though note that arm had no engagement counter, so read it as "no
  evidence of disagreement" rather than proof.
* **The return-tag scans are byte-identical.** `scan_return_tag`,
  `cratonvm_jit::return_type` and `DescriptorFacts::of`'s `ret_tag` loop are the
  same five lines three times.
* **Category-2 arguments are expanded correctly.**
  `Frame::new_pooled_cached_compact` gives `J`/`D` two local slots and pads with
  `uninitialized()`, so it is not a long/double slot-count error.
* **Overflow is guarded.** Every consumer refuses when
  `param_tags_overflow` is set or the arity disagrees with `param_tag_len`.

What is left is the door's own transfer: `read_args_verbatim` validates each
operand against the descriptor tag and stores `(CompactValue, tag)` pairs, and
`push_frame_verbatim` builds the callee frame from them without going through
`pop_arg_for_descriptor_checked` — the coercion path whose comment says it
"owns its three recorded corruption bugs". A representation the verbatim path
accepts but the general path would have coerced is the shape to look for.

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
