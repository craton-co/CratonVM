# The shape of the bytecode → PTX lowering, and what it costs

Written 2026-09-02, alongside the fixes in `fix(gpu)` /
`perf(gpu)` commits of that date. Those commits took every improvement
that did not require changing this shape. This note is about the ones
that do, so the next person deciding has the analysis rather than the
conclusion.

## Where we are

`jit-cuda` is a second Java bytecode front-end. It walks the byte array
with a hand-rolled operand-stack simulator and emits target text
directly, with no intermediate representation:

```
class file → analyzer::scan_bytecode  → OffloadVerdict
           → loop_recog::detect_loop  → LoopShape
           → lowering::emit::Emitter  → String of PTX
```

The workspace's other bytecode front-end, `jit`, builds a sea-of-nodes
graph and runs range analysis, SCEV, loop analysis, escape analysis and
3,800 lines of bounds-check elimination over it (`jit/src/x64/bce.rs`).
`GraphBuilder::build(code, code_len)` in `jit/src/ir.rs` is the entry
point. None of it is reachable from here.

That is a deliberate decision — the two backends have different
constraints and the GPU one shipped first as a narrow, safe subset — and
it is worth being explicit about what it has cost, because the file
says so itself in several places without connecting them.

### Symptom 1: two layers that must agree, by hand

`analyzer.rs` repeatedly explains that it rejects a shape only because
the emitter cannot lower it:

> the analyzer and the lowering emitter MUST agree on what they accept:
> a method admitted here only to be rejected by `walk` wastes an
> analyze→lower round-trip and pollutes the per-method blacklist

Every opcode band in `classify` is a restatement of what `emit_op` has
an arm for, maintained by reading both. With one IR there is one
acceptance predicate, and "can this node be lowered" is a match on
`Op`.

### Symptom 2: pattern matching where analysis belongs

`loop_recog` recognises exactly two shapes — a canonical counted loop
and a rectangular two-level nest — by matching the instruction sequence
javac emits. Anything else is refused. Its module doc is a careful
account of which javac spellings are accepted, which is a description
of a compiler's output, not of a language.

On a graph, "is this a reducible loop with an affine induction
variable" is a query (`loop_analysis`, `scev`), not a template match,
and the two-level restriction disappears along with the template.

### Symptom 3: the emitter analyses its own output text

Three places read the PTX string because there is no data structure to
ask:

- `speculation_cost` scores an if-conversion candidate with
  `str::contains` over emitted instructions, including a weight table
  keyed on mnemonic substrings.
- `check_every_branch_has_its_label` scans the finished body for a
  `bra` whose label was never emitted.
- `try_emit_if_converted` speculates by `mem::take`-ing the body
  `String`, emitting into it, and swapping it back.
- `Emitter::unconditional_since_guard` (added 2026-09-02) asks whether
  a branch has been emitted since the dispatch guard by searching the
  body tail.

Each is a sound workaround. Together they are the same workaround four
times.

## What was harvested without changing the shape

The 2026-09-02 pass took the optimisations that fit inside a text
emitter, and they were not small. On `out[i] = a[i] + b[i]`, the loop
body went from 31 instructions to 7:

| | before | after |
|---|---:|---:|
| bounds checks (3 accesses) | 18 | 0 |
| address arithmetic (3 accesses) | 9 | 3 |
| loads, add, store | 4 | 4 |
| **body total** | **31** | **7** |

by way of: one unsigned compare instead of two signed ones; the length
loaded once in the prologue instead of per access; `mad.wide.s32`
instead of `cvt`+`mul`+`add`; and retiring the check entirely against
what the dispatch guard already proved (`Emitter::prove_index_within_param`).

The last of those is a bounds-check elimination, and it is worth naming
what kind: a single dominance fact, hand-checked, for one shape. It is
not `bce.rs`. It does not know about `a[i+1]`, or an index derived from
two loop variables, or a bound held in a local across a conditional. It
cannot be applied to the nested shape at all, because that guard is
`tid < R * C` computed in `s32` and the proof would rest on a product
that can overflow. Those are exactly the cases SCEV answers.

**One item from the review does not survive contact and should be
struck**: "no register reuse — ptxas spills rather than argue". PTX
virtual registers are allocated by `ptxas`, which does its own register
allocation; declaring many of them is close to free. What costs is
*live ranges*, and the three 64-bit temporaries each array access used
to hold simultaneously were a real live-range problem — which the
`mad.wide.s32` fold removed. There is no separate register-reuse work
to do here.

### What that is worth on hardware

Measured on an RTX 2060 (driver 610.88, CUDA 13.3) against a binary
built from the same tree at the branch point, five interleaved rounds —
interleaved because this box runs other builds and a
whole-arm-then-whole-arm layout measures whatever the load did in
between. Medians of `best_ms`:

| workload | base | new | |
|---|---:|---:|---|
| transfer floor, 2.76M (control) | 1.47 | 1.52 | unchanged |
| ray tracer 640x480 | 0.367 | 0.330 | faster 5/5 |
| ray tracer 1280x960 | 0.844 | 0.791 | faster 4/5 |
| ray tracer 1920x1440 | 1.605 | 1.487 | faster 5/5 |
| GpuWarm 2^24 | 7 ms | 8 ms | unchanged |
| GpuCompute 2^26 | ~410 ms | ~402 ms | within noise |

Every checksum matched between the two binaries, on every workload, in
every round.

The shape of that table is the result, not the ray tracer's 7-10%. The
control does not move, which is what makes the rest readable. The two
arms that do not move are the two where the overhead removed is not what
the kernel spends its time on: `GpuWarm` is transfer-dominated with a
warm residency cache, and `GpuCompute` is 128 data-dependent multiplies
per element, against which twenty-odd instructions of address and bounds
arithmetic is noise. `GpuCompute` first appeared to regress 5/5 by ~4%;
re-measured with more rounds it reversed to 3/4 the other way, which is
what a 12-22% run-to-run spread does to a 4% difference. The ray tracer
is the arm with real index arithmetic per pixel and it is the arm that
moves — and its variance collapses too (1.40-1.51 against 1.54-2.54).

## The two options

### Smaller: an instruction list instead of a `String`

Give the emitter an internal `Vec<PtxInstr>` — an enum per mnemonic with
typed register operands — and render to text once at the end.

Every text-scanning workaround above becomes a list operation.
Peepholes become possible at all. The three codegen fixes above would
have been passes rather than edits at six emit sites, and the next one
would be cheaper still. CSE of repeated addresses (visible today: a
kernel reading `in[i]` twice emits `mad.wide.s32` twice) becomes a
dozen lines.

It does not buy general control flow, and it does not buy SCEV. It is
maybe a week, and it makes the rest of this file's future cheaper
whether or not the larger option is ever taken.

### Larger: consume `jit::ir::Graph`

A GPU backend over the existing graph:

- eligibility becomes a walk over `Op` variants against an allowlist;
- the counted loop comes from `loop_analysis`;
- the launch bound comes from SCEV's trip count, which also removes
  `locate_bound`'s bytecode archaeology;
- bounds checks come from `bce.rs`, including every case the hand-rolled
  version above cannot reach;
- the emitter becomes a per-node PTX printer over scheduled blocks —
  closer in size to `ir_lower.rs`'s x86 emitter than to today's
  4,600-line `emit.rs`.

And general reducible control flow, rather than two templates.

The cost is real and should not be undersold: the IR carries JVM
semantics the GPU has no answer for (exceptions, safepoints, GC-visible
allocation, calls), so the allowlist has to be written carefully, and
the deopt contract has to be re-expressed against IR nodes rather than
bytecode PCs. It is a quarter, not a sprint.

The argument for it is not that today's kernels would get faster. It is
that today's kernels are the ones someone hand-matched, and every
optimisation the CPU JIT gains is currently invisible here.

## Two more things deliberately not done on 2026-09-02

### Pinned, asynchronous H2D

`DeviceBuffer::from_host` host-blocks on the copy stream and reads the
pageable JVM heap arena directly. `cuda-bridge` already measured the
alternative and the numbers are in `PinnedHostBuffer`'s doc comment:
12.95 GB/s into page-locked memory against 3.98 GB/s pageable on this
box, with the staging memcpy running at ~26 GB/s. Staging through a
pooled pinned buffer would give roughly 8.7 GB/s effective, and going
async on top would let a kernel's arguments upload concurrently and
overlap the previous dispatch.

Two reasons it is not in that commit:

1. **The measured comparison is against the wrong baseline.** 3.98 GB/s
   was measured for an *async* copy into pageable memory. The upload
   path is *synchronous*, and the driver stages a synchronous pageable
   copy through its own pinned buffer, which is faster than that.
   Nobody has measured the number this change would actually beat.
2. **It breaks a load-bearing invariant.** The zero-copy path hands the
   device a JVM heap address. That is safe *only* because the copy has
   retired before the marshalling call returns, while the caller's
   `SafepointToken` is still held. `cuda_bridge::critical` and
   `gpu_marshal` now both say so explicitly; they previously said the
   opposite (that "the device never holds a JVM heap address"), which
   was true of the staged path and false of the one that runs. Going
   async needs either the staged copy back for that arm, or a token
   declaring `Relocation::Forbidden` held until the upload event fires.

Both are tractable. Neither should be done blind, on a box where the
result cannot be measured.

### A cubin cache

`DeviceModule::from_ptx` hands text to the driver's JIT on every process
start. The kernel set is small and stable, so a cache keyed on (PTX
hash, driver version, `sm_XX`) via `cuLink*` would make warm starts
near-instant. It is a contained change to `cuda-bridge` and it needs a
GPU to validate, which is the only reason it is not here.

## What the tests now hold

So that the next change to this file knows what it may not break:

- `elementwise_body_has_no_per_access_bounds_check_or_address_chain` —
  the exact per-element instruction count, asserted in both directions.
  Fewer means something the kernel needs went missing.
- `a_shorter_secondary_array_still_reaches_the_failure_flag` — the
  safety half. Retiring a check is only sound because a precondition
  proves the same thing; if that precondition stopped being emitted the
  count test above would still pass, and simply count fewer.
- `a_conditional_access_keeps_its_check_instead_of_hoisting_a_precondition`
  — the gate that stops a precondition escaping a branch and deopting
  launches Java would not have thrown on.
- `every_modern_target_renders_a_loadable_header` and
  `ptxas_round_trip_every_modern_target` — the `.version` / `.target`
  pairing, with and without a CUDA toolkit.
- `rust_enum_names_match_the_java_definitions` — the annotation constant
  names, against the compiled classes of a project versioned in another
  repository.
