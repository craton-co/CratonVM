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

### Symptom 1b: a THIRD layer that must agree, and the one that bit

The analyzer/emitter pair above is at least documented as a pair. There
is a third participant nobody wrote down: the VM's marshaller.

For a kernel to actually run on a device, three independent places must
admit its parameter types:

| layer | where | what it decides |
|---|---|---|
| analyzer | `jit-cuda/src/analyzer.rs`, `ParamKind::from_field` | is this method a kernel at all |
| emitter | `jit-cuda/src/lowering/emit.rs` | can this body be lowered to PTX |
| marshaller | `vm/src/runtime/offload.rs`, `marshal_array_arg` | can this array be pushed to a device |

The analyzer/emitter disagreement is loud: it "wastes an analyze→lower
round-trip and pollutes the per-method blacklist", and a lowering
refusal is logged. **The analyzer/marshaller disagreement is silent,
and it is silent in a way no differential test can detect.**

When the marshaller refuses a type the analyzer admitted, the kernel is
analyzed, lowered, compiled by `ptxas` and cached — and then the
dispatch fails, the VM falls back to the interpreter, and the method
returns the right answer. Every value matches HotSpot. Every value
matches the `--nojit` control. The arm under test is comparing the
interpreter with itself, and reports a pass.

That is exactly what happened to `short[]` and `byte[]`. `ParamKind`
had `I16Array`/`I8Array` from the start; `gpu_marshal` generated the
complete `upload_obj_i16`/`download_obj_i16` pair from the same
`direct_xfer!` macro as the other four, with `host_view_i16` /
`write_back_i16` unit-tested against a real `SharedVm` heap and a
comment arguing the zero-copy reinterpret is sound for them. The
marshal loop's `match element_type` had four arms and a catch-all. Not
one `short[]` or `byte[]` kernel ever reached a GPU, from the day that
marshalling was written until 2026-09-02, and a test note in the tree
asserted the opposite without ever exercising it.

Two things close it:

- `offload::is_marshallable_array_element` states the marshaller's set
  as data, a `debug_assert!` in the catch-all ties it to the match it
  describes, and `analyzer_and_marshaller_admit_the_same_arrays` pins it
  equal to `ParamKind::from_field`. It needs no device and fails in
  microseconds, naming the offending type.
- `bench-gpu/marshal-stress.sh` counts H2D transfers **per kernel** and
  fails when any of the six never dispatched.

The general lesson, and the reason this sits in a document about
lowering: **a correctness differential over a fallback path is vacuous
by construction.** Wherever the system's response to "I cannot do this"
is "do it correctly somewhere else", comparing outputs proves nothing
and you must count engagement instead. One IR removes the
analyzer/emitter half of this; it does not remove the marshaller half,
because that lives in the VM and is about heap layout rather than about
lowering. It needs its own guard either way.

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

Every text-scanning workaround above becomes a list operation, and
peepholes become possible at all. The three codegen fixes above would
have been passes rather than edits at six emit sites, and the next one
would be cheaper still.

It does not buy general control flow, and it does not buy SCEV. It is
maybe a week, and it makes the rest of this file's future cheaper
whether or not the larger option is ever taken.

**It does not buy CSE, and the earlier version of this note was wrong to
promise it.** See below.

## Retired: three residuals `ptxas` already handles

Measured 2026-09-02 with `ptxas -O3 -v` and `cuobjdump -sass` from CUDA
13.3, hand-writing the "after" form of each and comparing the generated
SASS. All three had been listed here as work worth doing. None of them
are.

**Redundant address and load computation.** The branching-loop kernel
emits `mad.wide.s32` three times for the same `in[i]`, and
`ld.global.s32` three times with it — once in the guard block and once
in each arm. Hand-CSE'd to one of each:

```text
                    SASS instructions   IMAD.WIDE   LDG.E   registers
  as emitted                       24           -       1           8
  hand-CSE'd                       24           -       1           8
```

Byte-for-byte identical. `ptxas` performs its own CSE and redundant-load
elimination over the whole function; three PTX loads of the same address
become one `LDG.E`. Emitter-level CSE would cost a dominance-aware side
table, an extension to `is_reserved_reg` so a join cannot clobber a
cached address, and buy nothing.

**`emit_tid` on a straight-line kernel.** The seven instructions that
compute `tid` are dead there — nothing reads the result. With and
without them: 8 SASS instructions, 4 registers, identical. `ptxas`
DCEs them.

**The `bra L_done;` immediately before `L_done:`.** Same experiment,
same answer: identical SASS.

The general point is worth keeping even though the specific items are
closed: `ptxas` is a real optimising compiler, so the emitter's job is to
avoid emitting things `ptxas` **cannot** fix. It cannot remove a bounds
check it cannot prove dead — which is why the work that DID move the
needle was the bounds-check elimination and the `mad.wide.s32` fold, not
tidying.

## Retired: bounds-check elimination for the nested shape

Also listed here as future work, and also closed by looking at the
output. The 2-D lowering recovers `i = tid / C` and `j = tid % C`, so a
proof of `0 <= i < R` and `0 <= j < C` is available. It buys nothing,
because no accepted nested kernel indexes an array by `i` or `j`
directly — every one of them uses the linearised `i * C + j`:

```text
    div.s32 %r8,  %r5, %r2;     // i
    rem.s32 %r9,  %r5, %r2;     // j
    mul.lo.s32 %r10, %r8, %r2;  // i * C
    add.s32 %r11, %r10, %r9;    // i * C + j   <- the index
    setp.ge.u32 %p1, %r11, %r0;
```

`prove_index_within_param` matches on the index REGISTER being the
induction register, and `%r11` is neither. Discharging it instead needs a
linear-index proof — recognise `i*C + j` and check `pN_len >= R*C` once —
which is a different and larger piece of work, and it has a prerequisite:
the nested guard computes `R * C` with `mul.lo.s32`, which silently wraps.
Nothing depends on that today because the per-access check is the
backstop; removing the check without widening the multiply first would
turn a wrap into an out-of-bounds write.

## Not attempted: `.maxntid`

`ptxas` budgets registers against an assumed maximum block size, and a
kernel that declares its own can be given more. The declaration is only
sound when the launch honours it, which is true for exactly one case —
an explicit `@GpuKernel(blockX = ...)`, which seeds the launch memo with
the same number. Every other kernel picks its block size from
`cuOccupancyMaxPotentialBlockSize` at launch, long after the module was
compiled, and a `.maxntid` smaller than the launch's block is a launch
failure rather than a slow kernel. So it applies only to kernels whose
author wrote a block size by hand, and it needs `block_x` plumbed through
`lower_method`'s signature to get there. Narrow benefit, public API
change, and a failure mode worse than the problem: not done.



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

**2026-09-02, later the same day:** the second obstacle is gone —
`GcCriticalGuard` is a registry token and the marshal window declares
`Relocation::Forbidden`, so an async upload would now be a matter of
holding that token until the upload event fires. The pinned half was
prototyped as SYNCHRONOUS staging and measured the same day: 10-23%
slower than the pageable copy at every size from 1 to 128 MiB
(`cuda-bridge/tests/transfer_bandwidth_it.rs`), so it was removed. What
remains open is the ASYNC upload, which is a different question —
overlap with a kernel, not bandwidth — and still needs the measurement
described above.

### A cubin cache

`DeviceModule::from_ptx` hands text to the driver's JIT on every process
start. The kernel set is small and stable, so a cache keyed on (PTX
hash, driver version, `sm_XX`) via `cuLink*` would make warm starts
near-instant. It is a contained change to `cuda-bridge` and it needs a
GPU to validate, which is the only reason it is not here.

## Float semantics: what is guaranteed and what is not

Added 2026-09-02, after differentially testing the emitter against
HotSpot on an RTX 2060 rather than reading its comments
(`bench-gpu/arith-differential.sh`, fixtures
`test_classes/gpu/GpuArithDifferential.java` and `GpuArithProbe.java`).

**Guaranteed, and it was wrong until that run.** `(int) NaN` and
`(long) NaN` are zero — JLS §5.1.3 says so with no room. The device was
returning the destination type's MIN_VALUE: 474 of 4096 elements wrong
for `d2i`/`d2l`, 584 for `f2l`. `f2i` happened to be correct on this
device, which is exactly why the bug survived — the PTX ISA is silent on
the NaN case for these conversions, so three of the four diverged and the
fourth did not. All four now carry a `setp.nan` + `selp` guard.

**Not guaranteed, and deliberately left alone.** NaN payloads are not
preserved by `add`/`mul`/`div`/`neg` on the device: every NaN comes back
as `0x7fffffff`, CUDA's canonical NaN, whatever went in. HotSpot
propagates the payload and flips only the sign bit for negation. Both
conform — JLS §4.2.3 does not specify the pattern and the PTX ISA says
"NaN inputs yield an unspecified NaN" — and the only way a program can
observe it is `floatToRawIntBits`. See `Emitter::unop_f32` for why
fixing `fneg` alone would be worse than documenting all four.

**Measured correct, so worth recording as tested rather than assumed:**
the shift masks (JLS §15.19 masks the count; PTX clamps it — the emitter
masks explicitly), integer division including `MIN_VALUE / -1`, the
narrowing `i2b`/`i2c`/`i2s`, float→int saturation at every boundary, and
`ineg`/`lneg` at MIN_VALUE. 27 kernels, 4096 elements each, against a
CPU control.

**The control is not optional.** The first run of this harness reported
the emitter as diverging on five kernels. Two of them were the host: a
`String.equals` miscompile under CratonVM's own JIT made the harness take
the wrong `printf` arm partway through the loop, so it was comparing
different quantities. `cratonvm --nojit` as a middle arm is what
separated them, and the harness now refuses to report a device
difference at all while the control itself disagrees with HotSpot.

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
