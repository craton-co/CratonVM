# Acyclic loop-body branch lowering

The CUDA emitter lowers forward control flow inside a counted-loop body. It
discovers basic blocks, emits `L_body_<pc>` PTX labels, lowers
`if*`/`if_icmp*`/`ifnull`/`ifnonnull` to `setp` plus predicated branches, and
lowers forward `goto`/`goto_w` to PTX `bra` instructions.

Each target block has canonical registers for its live JVM locals and
operand-stack entries. Every incoming edge copies its state into those
registers, giving join points explicit phi-style semantics without relying on
the old linear simulated stack.

The one-thread-per-iteration model intentionally rejects interior backward
edges, branches leaving the loop body (`break`), and returns in the body.
Those shapes need a different iteration-space and completion model. Canonical
loop back-edges remain handled by the existing per-thread loop guard.

`EligibleBranchingLoop.java` includes an `if`/`else` local merge and a
one-arm branch. The `branching_loop_*` tests and
`ptxas_round_trip_branching_loops` cover emitted control-flow PTX.

## If-conversion: a ternary is not a branch

`cond ? a : b` is not control flow in the source; it becomes control flow in
the class file, because `javac` has nothing else to emit. Lowered literally
it becomes a PTX `bra`, and `ptxas` wraps the divergent region in a
`BSSY`/`BSYNC` pair so the warp reconverges afterwards. On the ray-tracer
kernel that came to 67 `BRA` plus 32 `BSSY`/`BSYNC`/`BMOV`, 18% of the
kernel's SASS, around arms that are three instructions of float arithmetic.
The kernel is written branchlessly on purpose; the lowerer was putting the
branches back.

`Emitter::plan_if_conversion` recognises exactly the shape `javac` emits for
a ternary — a conditional branch over the then-expression, a `goto` over the
else-expression, and a join both fall into — and `try_emit_if_converted`
runs both arms unconditionally and merges the differing state slots with
`selp`.

Four conditions, and each of them is load-bearing:

* **Both arms are single basic blocks.** An arm carrying its own branch is
  refused, so a nested ternary converts its inner diamond and keeps the
  outer. That is a boundary rather than a limitation: every conversion is
  independently sound, and the innermost arms are the shortest.
* **Neither arm has any other predecessor.** A short-circuit
  `(x > 0 && y > x) ? p : q` compiles to TWO conditional branches to the
  SAME else-label. The second one's diamond passes every other test, and
  consuming the else-block would leave the first branch jumping at a label
  nothing emits — PTX `ptxas` rejects, which the VM turns into a blacklisted
  method and a silent CPU fallback with the right answer. Refused.
* **Neither arm's emitted PTX holds a label, a branch, a predicated
  instruction, or a memory access.** The screen reads the PTX rather than an
  allow-list of opcodes, because whether an arm can run unconditionally is a
  property of what it LOWERS TO — and an opcode list would be a second copy
  of the opcode-to-lowering map, free to drift from the first. An array
  access is caught by its own bounds check's `bra`; a store is caught twice
  over.
* **Both arms are short.** Not correctness — both arms run on every lane, so
  converting a long one trades a branch the warp might never have diverged
  on for arithmetic it certainly executes.

`selp` operand order is `selp d, else, then, p`, because `p` is true on the
branch-TAKEN path and the taken path is the else-arm. Reversing it produces
a kernel that computes both right values and keeps the wrong one, which no
shape test would catch — the `ptxas` round trips and the frame-level
bit-exactness check are what stand behind it.

`check_every_branch_has_its_label` refuses any lowered body containing a
`bra` to a label it never emits. It is a whole-body invariant rather than a
check inside the transform, because the point is to catch the next one.

## It is off by default, and that is the measurement talking

The transform does what it was built to do. On the ray tracer, PTX branches
fall from 51 to 21 and SASS branch machinery from 17.4% of the kernel to
15.4%, and the frame stays bit-identical to HotSpot. It also makes that
kernel **slower**: 0.128 ms to 0.199 ms on the compute half, 56% worse, in 8
of 8 interleaved rounds against a transfer-floor control that held within 3%.

The mechanism is the trade the fourth condition names. A branch a warp does
not diverge on costs almost nothing — all 32 lanes skip the untaken arm
together — while `selp` makes every lane compute both. Four of that kernel's
ternaries are `disc > 0f ? (float) Math.sqrt(disc) : 1e9f`, one per sphere,
and most of a frame is background where a whole warp misses every sphere.
Converted, those warps compute four square roots they had been skipping.

That is also why the budget is weighted rather than counted: the arms in
question are ONE instruction each, so a length cap cannot tell them from a
pair of `mov`s. `sqrt`/`div`/`rcp`/`ex2` count 16 and everything else 1.
Sweeping the budget at 1920x1440 found no setting that wins — 8 ties with
the feature off, and 2, 4, 16, 32 and unbounded are all worse.

So: `CRATONVM_GPU_IF_CONVERT=1` turns it on at the default budget,
`CRATONVM_GPU_IF_CONVERT_MAX_OPS=<n>` turns it on at `n`, and unset does
nothing. It is kept and kept reachable because that is one kernel — a shape
with cheap arms and heavy divergence is exactly what it is for, and these
flags are how someone measures whether theirs is one.

`EligibleTernary.java` is the fixture — `select` converts, `nested` converts
its inner diamond only, `shortCircuit` and `withStore` are refused — and
`ptxas_round_trip_if_converted_ternaries` assembles all four with the real
NVIDIA assembler. The tests pass the budget explicitly rather than reading
the flag, because a test that silently exercises a non-default path is not
testing what it says it is.
