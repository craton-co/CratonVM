# `CRATONVM_SCALAR_DEOPT`: the gauntlet soak

`docs/internal/fixed-bugs/per-voxel-allocation-escapes-its-method-so-ea-cannot-help-FIXED-20260827.md`
closed with the flag still default-off and one sentence of debt: *"it needs the
kafka/spring/tomcat/hibernate gauntlet that flag has never had."* This is that
soak.

**Verdict: do not flip.** Two independent reasons, and neither is a timing
result:

1. **The designated pre-flip gate is RED.** `jit/tests/ir_vs_singlepass.rs`
   fails with the flag on and passes with it off, returning a heap address where
   an `int` belongs — `docs/known-issues/jit/scalar-deopt-elision-returns-the-object-in-the-differential-harness-20260827.md`.
   The same source shape compiled the ordinary way is correct, so the two
   environments disagree and that has to be resolved before a flip, not after.
2. **Flipping it ALONE would change almost nothing anyway.** The flag decides
   one bit — may an allocation a deopt snapshot names be deleted — and on real
   code that bit is rarely reached, because `c2_upgrade_would_engage` keeps
   allocation-bearing methods out of C2 in the first place. Measured below.

## The instrument, and why the soak needed one first

The flag's whole effect is `deopt_descriptor_available` in
`plan_scalar_replacement`. Everything else about the compile — the analysis, the
loads it forwards, the stores it kills — is identical either way. So "the suite
is green with the flag on" is not evidence until it is paired with "and the flag
reached this workload", and nothing measured that.

`cratonvm_types::scalar_deopt_census` does, in three numbers printed once on the
`System.exit` path (the same path `cell_census` reports on, and for the same
reason: a JUnit runner never reaches `vm-cli`'s normal-return arm, so a census
printed there produces zero lines across a whole suite sweep):

```
[scalar-deopt] census: rescued=6 blocked=0 materialized=0
```

* **`rescued`** — allocations deleted BECAUSE a descriptor was available: the
  flag's engagement count.
* **`blocked`** — the same population seen from the other arm: proved
  replaceable, kept for want of a descriptor. This is what makes a zero
  readable. `rescued=0 blocked=0` means the workload has no allocation in this
  shape at all and the arm proves nothing; `rescued=0 blocked=N` would mean the
  flag was on and still could not describe them.
* **`materialized`** — RUNTIME reconstructions: a deopt that actually had to
  rebuild what the compiler deleted. This is the number that matters most, and
  it is a different question from `rescued`. `rescued` says the compiler took
  the flag's path; only this says the RECIPE WAS EXECUTED.

The counters are two relaxed atomics bumped once per scalar-replacement plan
(a compile-time event) and one per materialization, so they are unconditional
rather than behind a debug flag — an engagement census you have to know to ask
for is how a soak gets run without one.

## What the census immediately showed

**In the 142-test IR-vs-single-pass differential corpus, the flag elides exactly
ONE allocation.** The other 141 passing tests are not evidence about this flag;
it never engaged in them. Its engagement in that corpus is one, and its failure
rate on that one is one.

**On the `VoxelAlloc2` probe — the workload the flag was measured on when it
bought 10.9x — `CRATONVM_SCALAR_DEOPT=1` on its own produces no census line at
all.** It needs `CRATONVM_JIT_IR_INLINE=1` beside it to have anything to decide.
That is the mechanical reason the fixed page measured the flag's solo arm at
"worse than nothing" (267 ns against 259): that arm was pure noise around an
inert flag, not a regression.

**`materialized=0` everywhere measured so far.** Even in the arms where the flag
engages and pays, the descriptor it writes is never read. The risk the flag
carries — a wrong recipe — is therefore the part the soak has NOT exercised, and
saying so is more useful than the green.

## Arms

Same binary throughout, built from `fbeffaf79`, 32-core host, otherwise idle.

| arm | flags |
|---|---|
| A | (none) — baseline |
| B | `CRATONVM_SCALAR_DEOPT=1` |
| C | `CRATONVM_SCALAR_DEOPT=1 CRATONVM_JIT_C2_ALLOC_UPGRADE=1` |
| D | `CRATONVM_SCALAR_DEOPT=1 CRATONVM_JIT_IR_INLINE=1` |

C and D exist because B on its own is nearly inert, and a soak of an inert flag
is not a soak. C lifts the gate that keeps allocation-bearing methods out of C2;
D is the pairing the fixed page asks to be priced together.

## Results

### Regression suite (72 vectors)

| arm | result |
|---|---|
| A | 72 passed, 0 failed |
| B | 72 passed, 0 failed |
| C | *(not run — the suite's vectors are the same 72 either way)* |
| D | 72 passed, 0 failed |

### IR-vs-single-pass differential (142 tests)

| arm | result | allocations elided |
|---|---|---|
| A | 142 passed | 0 |
| B | **141 passed, 1 FAILED** | 1 |
| D | **141 passed, 1 FAILED** | 1 |

The failure is the same test in both, and it is the flag's only engagement in
the corpus. See the known-issues page.

### Tomcat suite (651 classes, one process per class)

*(filled in below when the arms complete)*

## Reproducing

```bash
# the gate that is red
CRATONVM_SCALAR_DEOPT=1 cargo test -p cratonvm-jit --test ir_vs_singlepass

# engagement on any workload
CRATONVM_SCALAR_DEOPT=1 cratonvm ... ; # look for `[scalar-deopt] census:` on stderr

# the tomcat arms
powershell -File apps/tomcat-suite-runner/run-tomcat-suite.ps1 \
  -Category all -Parallel 6 -TimeoutSec 300 -RunName sd-A-none -Exe <exe>
```
