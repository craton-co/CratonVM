# `CRATONVM_SCALAR_DEOPT`: the gauntlet soak

`docs/internal/fixed-bugs/per-voxel-allocation-escapes-its-method-so-ea-cannot-help-FIXED-20260827.md`
closed with the flag still default-off and one sentence of debt: *"it needs the
kafka/spring/tomcat/hibernate gauntlet that flag has never had."* This is that
soak.

**Verdict: do not flip.** Three independent reasons, and none of them is a
timing result:

1. **It would not buy anything.** Across the 651-class Tomcat suite the flag
   fires ZERO times — in every arm, including the two that lift the gates
   upstream of it. Real server code's allocations escape (22 of 22 in a sampled
   class, all `GlobalEscape`), so the predicate the flag controls is never
   reached. Flipping it default-on would change no compiled code on that
   workload while carrying reason 2 below.
2. **The designated pre-flip gate is RED.** `jit/tests/ir_vs_singlepass.rs`
   fails with the flag on and passes with it off, returning a heap address where
   an `int` belongs — `docs/known-issues/jit/scalar-deopt-elision-returns-the-object-in-the-differential-harness-20260827.md`.
   The same source shape compiled the ordinary way is correct, so the two
   environments disagree and that has to be resolved before a flip, not after.
3. **Even where the lane DOES pay, this flag alone is not what pays.** It
   decides one bit — may an allocation a deopt snapshot names be deleted — and
   reaching that bit needs a DIFFERENT gate lifted first: `c2_upgrade_would_engage`
   keeps allocation-bearing methods out of C2 at all. On the one workload where
   the flag does engage it is worth 10.9x, and only alongside
   `CRATONVM_JIT_IR_INLINE`. Measured below.

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

Same binary, same host, `-Parallel 6`, 300 s cap, run back to back.

| arm | PASS | FAIL | HANG | CRASH | wall | **allocations elided** |
|---|---:|---:|---:|---:|---:|---:|
| A (none) | 620 | 27 | 4 | 0 | 45.9 min | **0** |
| B `SCALAR_DEOPT` | 617 | 28 | 5 | 1 | 44.8 min | **0** |
| C `+ C2_ALLOC_UPGRADE` | 611 | 30 | 10 | 0 | 49.1 min | **0** |
| D `+ IR_INLINE` | 618 | 27 | 6 | 0 | 48.0 min | **0** |

**The flag never fired once, in any arm, across 651 classes.** The last column is
the census, and it is the only column that licenses a reading of the others: with
engagement zero the treatment arms compiled the same code as the baseline, so
every status difference between them is noise by construction.

The flags did reach the workers — each arm's logs carry the launcher's own
deprecation line naming them (`CRATONVM_JIT=scalar-deopt`,
`CRATONVM_JIT=c2-alloc-upgrade`, `CRATONVM_JIT=ir-inline`) — so this is a real
result about the flag, not a broken experiment.

#### Why it never fires: the allocations escape

One Tomcat class run by hand with `CRATONVM_DBG_SCALAR_NEW=1` and both C-arm
flags (`org.apache.catalina.mapper.TestMapperPerformance`):

```
11 allocation-bearing methods reached escape analysis at C2
22 allocations, every one:  refused Escapes(GlobalEscape)
0 scalar-replaceable  ->  plan_scalar_replacement never runs
                      ->  the flag's gate is never reached
```

`java.util.Objects.requireNonNull`, `MessageBytes.<init>`,
`CopyOnWriteArrayList.<init>`, `StringUTF16.getChar` — ordinary server code, and
not one allocation stays local. That is the structural reason the flag is inert
here, and it is worth more than the pass counts: the scalar-replacement lane
works on the accessor-wrapper shape it was built for (kfusion's `Short2`) and
finds **nothing at all** in a real Tomcat class.

#### A free noise floor for this suite

Four runs of effectively identical code gave 620 / 617 / 611 / 618 PASS — a
**±9-class swing with no code change**. The classes that churned are the ones
the harness header warns about, and they moved in BOTH directions (two arms
"fixed" a class the baseline failed):

```
TestMulticastPackages, TestTcpFailureDetector, TestNonBlockingAPI,
TestHostConfigAutomaticDeployment{Addition,Modification}, TestOcsp*,
TestGenerator, TestDefaultServletEncoding*
```

multicast, TCP failure detection, file-watching with sleeps, an OCSP responder.
Anyone reading a future Tomcat A/B on this host should treat a swing of this
size as nothing, and should say which classes moved rather than only how many.

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
