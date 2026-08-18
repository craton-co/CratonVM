# A lambda SAM call bypassed the cached invoke path — RETIRED 2026-08-18

| | |
|---|---|
| **Status** | RETIRED — both defects it named are fixed, shipped and pinned, and the residual it left (capturing lambdas) is closed too |
| **Opened** | 2026-08-17 as `known-issues/perf/lambda-sam-dispatch-bypasses-the-cached-invoke-path-20260817.md`; §5 added the same day |
| **Closed by** | `fix/lambda-sam-jit-tierup-20260817`, then `perf/lambda-mic-adapter-20260818`, then `perf/lambda-capturing-adapter-20260818` for the capturing residual |
| **Measured effect** | **37x** on `probes/SamHotLoopProbe.java`'s lambda row — 379 → 10.2 ns/op, against a named-class control of 10.3 — same binary, three-arm ABBA, six runs an arm. The gap this page was filed about is GONE, not narrowed. The capturing row followed on 2026-08-18: **17.4x**, 125.1 → 7.2 ns/op against a control of 6.6 (see §4) |
| **…on a real workload** | **Not measurable.** On Tomcat's JUnit suite the feature engages (7 of 24 classes install thunks, a third of them capturing) but the capturing thunk moves wall time 2% with fully overlapping ranges — a few hundred `site_calls` per process against a ~140 ns saving is tens of microseconds in a 45-second run. Quote the ns/op figures as microbenchmark numbers, not workload numbers; see §4 "Does any of this reach a real workload?" |
| **Kill switches** | `CRATONVM_JIT_LAMBDA_TIERUP=0` (everything), `CRATONVM_JIT_LAMBDA_SITE=0` (the compiled-caller Rust arm), `CRATONVM_JIT_LAMBDA_ADAPTER=0` (the inline-cache thunk), `CRATONVM_JIT_LAMBDA_CAPTURE_ADAPTER=0` (just the capturing half of it) |

The page asked for one thing in its §4 — *"giving lambda call sites a cached
invoke target of their own"* — and reported in §5 that the attempt at the other
half crashed the VM. Both are now done. What made them doable was not a new
idea; it was an instrument that could say which of two indistinguishable things
was happening.

## 1. The measurement that named the defect

§4 reasoned from a flat profile: `try_lambda_dispatch` reached
`invoke_on_class_shared_inner`, which allocates, pins, takes the
`lambda_proxies` lock a third time and resolves by name — while an ordinary
`invokeinterface` "goes through the resolved-callsite cache and the JIT's
monomorphic inline cache, which is why it is 9 ns."

That is right about the destination and wrong about the size, and the way to
tell is `CRATONVM_DBG=mic-prof` on one shape at a time
(`probes/SamHotLoopProbe.java`, written for this page because
`SamDispatchDecompositionProbe` runs eight rows in one process and its boxing
row owns any profile taken over the whole thing):

| receiver | ns/op | `mic_calls` | `hit_entry` | `lambda` |
|---|---:|---:|---:|---:|
| named class | 11.7 | **1** | 0 | 0 |
| lambda | 352.5 | **2 197 000** | 0 | 2 197 000 |

**One helper entry per 2.2 million dispatches, against one per dispatch.** For
a named class, `jit_invoke_virtual_mic` runs ONCE per call site: it resolves the
callee, fills the monomorphic inline cache, and from then on the cascade emitted
in `jit/src/x64.rs` calls the callee from machine code and never returns to
Rust. A lambda receiver is caught by an arm that sits BEFORE that cache and
returns from inside it, so the slot is never populated and never probed —
`hit_entry=0`, `miss=0`, ~819 cycles a call, forever.

So the defect was not "the generic dispatch machinery is expensive". It was
**one early `return` standing between a SAM call site and the same inline cache
every other interface call site gets** — and no amount of memoizing inside that
machinery could have reached it, which is exactly why §3's `OnceLock<ClassId>`
memo moved nothing.

## 2. What was built

Two halves, because a SAM call has two kinds of caller and they are served by
different code.

**The compiled caller** — `jit::helpers::try_lambda_site_direct_call`. The
inline cache cannot itself hold a lambda: its cascade passes the caller's own
argument registers straight through, and a SAM call's registers are not the
impl method's (the proxy receiver has to go, the captured values have to
arrive). So the call site's target is cached one level out, in Rust, per proxy
`ClassId`, in a thread-local. Spending it is: read the captures out of the
proxy's fields, put the SAM's already-decoded raw arguments after them, call the
compiled impl through `try_call_compiled_entry_reentrant_owned` — the same
primitive the monomorphic hit path uses, with the same `i64::MIN` deopt
handling and the same "an escaping exception is left in `jit_pending_exception`
for the compiled caller's own post-invoke check".

Everything that makes a lambda dispatch complicated is decided ONCE, when the
site is built, and a shape that needs any of it is cached as ineligible and
never asked again. The eligibility rule is that coercion must be provably the
identity — which `coerce_arg` is for equal tokens *and* for two reference
tokens, so `Function<Integer,Integer>` (the generic shape, where javac's bridge
would have inserted a `checkcast`) qualifies with the cast replayed per call and
a cast that would FAIL simply declining to the generic path, which then throws
the `ClassCastException` with the message it has always built.

**The interpreted caller** — §5's missing tier-up, plus the primitive its first
attempt lacked. The warmup counter is the same shape as the invokestatic and
invokevirtual twins. Entering the compiled body is NOT: `execute_jit_call_decoded`
pushes its result onto a caller's operand stack and, on a routed exception or a
precise-resume deopt, pushes an interpreter FRAME and returns
`CachedCallResult::FramePushed`, meaning "the stepping loop will run it". A
dispatch helper is not that loop. §5.3's crash — a `usize::MAX` operand-stack
underflow, thousands of calls later, inside an unrelated interpreted run of the
same body — was an orphaned frame from exactly that mismatch, and §5.4 named the
two ways out. This took (b): `jit_bridge::execute_jit_call_oneshot`, which keeps
that function's run/signal/deopt logic verbatim and differs in the two places
that matter — a normal return is CONVERTED to a `Value` and returned rather than
pushed, and a sink that materialises a frame has that frame RUN TO COMPLETION
here (`run_pushed_frame_to_completion`, split out of `execute_prebuilt_frame`),
so the handler or resumed body finishes as part of the call and nothing is left
on `thread.frames`.

Two smaller things fell out. `CRATONVM_BG_COMPILE=0` — the documented opt-out
that restores inline compilation — did nothing on the lambda path in the first
cut, so the off-switch would have silently disabled the feature rather than
changed how it compiles; it now compiles inline like the twins. And the TDigest
`get(I)D` special case in `lambda.rs`, which entered a compiled body directly
and ignored every out-of-band signal it might raise, is gone: the general path
subsumes it and drains them.

## 3. The numbers, beside the count of calls that produced them

Same binary, kill switch, ABBA, three rounds of A-B-B-A, six runs an arm, Azure
8-core under other sessions' load (which is why the absolute numbers are above
the page's original 355 ns; the ratios are the statement).

| row | tier-up ON | OFF | |
|---|---:|---:|---|
| named class (control) | 12.6 | 11.8 | unmoved, as it must be |
| `ifaceLambda` | **191.7** | 434.4 | **2.27x** |
| method reference | **195.3** | 435.1 | 2.23x |
| capturing | **199.6** | 552.0 | **2.77x** |

Ranges do not overlap on any lambda row: ON `[183.9 … 207.2]` against OFF
`[414.0 … 466.0]` for `ifaceLambda`.

And the engagement, printed beside them (`CRATONVM_DBG=lambda-jit`), because a
flat A/B on this path cannot tell "the direct call did not help" from "no direct
call ever happened":

```
[LAMBDA-JIT] eligible=2999 compiled_hits=2103 fast_returns=2103 declines=0
             site_calls=1100000 site_direct=1100000 site_no_code=0
             site_refused=0 site_deopted=0 site_arity=0
```

1 100 000 of 1 100 000 — every dispatch after warmup took the direct arm, none
refused, none declined.

## 4. The rest of the gap, closed: a lambda receiver the inline cache CAN hold

The two halves above left the lambda row at 15x the named-class row, and section
1 already said what the remainder was: a receiver the inline cache cannot hold.
That was written as this page's successor work. It is done, and it turned out
to be smaller than it looked.

**The obstacle was an argument shuffle, not anything about caching.** The
cascade passes what the CALL SITE has — `(proxy, samArg1, …)` — and a
non-capturing lambda's impl wants `(samArg1, …)`, because javac compiles the
body to a private static synthetic that never sees the proxy. One register too
many, in the wrong place.

So `jit/src/lambda_adapter.rs` emits a per-(proxy class, impl) THUNK that
performs exactly that shuffle and tail-jumps to the impl, and the MIC/PIC slot
holds the thunk:

```text
    mov  ARG0, ARG1        ; drop the receiver, slide the SAM args down
    mov  r11, <impl entry>
    jmp  r11
```

Thirteen bytes for a one-argument SAM. Three properties keep it that small:

* **It tail-JUMPS**, so the impl sees byte-identical stack state to a direct
  call, the caller's return address is what it returns to, Windows shadow space
  and alignment are inherited, and a conservative stack walk never sees the
  thunk at all — after the jump the machine state is indistinguishable from the
  cascade having called the impl directly, which is why GC safety needs no new
  argument.
* **The slide is type-blind.** This VM's JIT ABI gives every Java argument one
  INTEGER register — `execute_jit_call` passes a `double` as `to_bits()` and the
  cascade loads each operand-stack slot into `ARG_REGS[i]` without consulting
  its type — so no type information is needed. `LambdaAdapterProbe`'s
  double-argument arm is the check that this stays true.
* **It touches no memory**, which is what confined it to NON-CAPTURING lambdas
  *when this was written*: reading a captured field from a hand-emitted thunk
  looked like it would mean reproducing the compact/legacy body-layout branch
  and every per-type width the `getfield` arms handle. A capturing lambda kept
  the Rust arm at ~200 ns.

  **Superseded 2026-08-18.** The first half of that is not true of a lambda
  proxy. See §4's "the one that is still open", which is now closed, and which
  says what the obstacle actually was.

Invalidation is the same commitment a JIT'd caller's baked direct call makes,
and is registered the same way: the thunk's `_direct_callee_entries` names the
impl, and the cache's existing invalidation closure — which already promotes any
method whose baked callee is being removed — now reaches thunks through
`adapters_reaching`, so a slot holding one is cleared when its impl is evicted.

### What it measures

Three arms, one binary, ABBA, six runs an arm:

| row | thunk | Rust arm only | feature off |
|---|---:|---:|---:|
| named class (control) | 10.3 | 10.2 | 9.7 |
| `ifaceLambda` | **10.2** | 175.1 | 379.4 |
| method reference | **10.0** | 177.1 | 372.4 |

**A lambda SAM call now costs what an interface call on an ordinary class costs**
— 10.2 against 10.3 — where this page opened at 40x. And the census that named
the defect in section 1 now reads, for a lambda:

```
[MIC_PROF] mic_calls=1 hit_entry=0 miss=0 lambda=1 …
```

`mic_calls=1` across 1 100 000 dispatches, which is exactly the named-class line
from section 1's table. The helper is entered once per call site, the thunk is
installed, and nothing returns to Rust again.

### What is emphatically NOT closed by any of this

The workload this page was filed from. `residual-seven-after-the-afc-fix-20260817.md` put ~55% of
`MultithreadedInsertionTest`'s samples in `CompletableFuture` composition, and
this page inherited the inference that composition is slow because SAM dispatch
is slow. It is not. `probes/LambdaCompositionProbe.java` — `thenApply` /
`thenCompose` chains, the real shape — measures:

| | CratonVM | HotSpot | |
|---|---:|---:|---|
| `thenApply` | 12 286 ns/stage | 82.5 | 149x |
| `thenCompose` | 11 943 ns/stage | 113.4 | 105x |

A SAM dispatch is ~190 ns even before this fix's 2.3x, so it cannot be more than
a low single-digit percentage of 12 µs — and the A/B agrees, moving those rows
−1.1% and −3.3%. Two further facts name where the time is instead:

* `site_calls=0` on that probe. The composition path's SAM calls come from
  INTERPRETED callers, so the compiled-caller half never runs; only the
  interpreted half applies, and it is the smaller of the two.
* `--nojit` measures 11 470 / 17 443 ns/stage — **the same**. A workload the JIT
  does not change is not a workload whose cost is dispatch.

So the hibernate-reactive composition residual needs its own investigation,
starting from a profile of `LambdaCompositionProbe` (flat: the interpreter loop
at 7%, the native registry's three lookup functions at ~5.8%, allocation ~3%),
and it should not be filed as a lambda problem.

### The one that was still open — closed 2026-08-18

A CAPTURING lambda kept the Rust arm and its ~200 ns, because the thunk may not
read a captured field without reproducing the compact/legacy body-layout branch.

**Most of that premise was wrong, and it was wrong in a way worth recording,
because it is the same mistake this page's §3 memo made: reasoning about a
general obstacle instead of asking what the specific object looks like.**

A lambda proxy's class id comes from `alloc_lambda_proxy_id`, which counts up
from `0x8000_0000` — disjoint from every id class definition hands out. Nothing
registers a `CompactLayout` for one, and `plan_object_alloc` sets
`GC_FLAG_COMPACT` only when a registered layout matches the allocation's field
count. So every lambda proxy in this VM is a uniform 16-byte-cell object, its
capture offsets are the compile-time constants
`HEADER_SIZE + i * SLOT_SIZE + payload`, and **the branch that was the stated
blocker never needed emitting at all.** What remained was the small half: three
loads cover every Java type, the same three the `getfield` legacy arm emits.

That is a fact about this VM rather than a property of thunks, so
`lambda_adapter_entry` asks `class_layout_for_fields` at build time and refuses
if it ever answers otherwise — the feature disables itself rather than reading
captures at the wrong offsets.

The thunk therefore grew a prologue rather than a branch: save the receiver to
`r11`, slide the SAM arguments to sit *after* the captures, load the captures
into the registers the slide vacated, tail-jump. The arguments now move by
`captures - 1` registers — down one for none, not at all for one, up for more —
and the slide runs in whichever direction reads each register before the step
that writes it.

One restriction appeared to survive, about the collector rather than the layout:
a REFERENCE capture was refused while `narrow_oops_block_inline_fields()` held —
compressed oops on, or ZGC's read barrier armed — by analogy with the inline
`getfield` codegen. **It was removed on the same day, because the analogy did
not hold and the gate was inert anyway.** See "The reference-capture gate"
below; there is now no capture shape this thunk refuses on the collector's
account.

`CRATONVM_JIT_LAMBDA_CAPTURE_ADAPTER=0` is the kill switch, kept separate from
`CRATONVM_JIT_LAMBDA_ADAPTER` so a same-binary A/B can hold the non-capturing
thunk fixed while moving only this.

#### The numbers

Same binary throughout (`cratonvm-lamcap`, md5 `107ebef71211a5f334b96864b62fef53`),
three arms selected by kill switch, order `A B C C B A` within each of three
rounds so drift in the box's load falls on every arm equally. Six runs an arm,
`probes/SamHotLoopProbe.java`, 2 000 000 ops, Azure 8-core — and unlike §3's
table, a quiet one, which is why every absolute number here is about half of
that table's.

| row | A: both on | B: capture thunk off | C: no thunk at all |
|---|---:|---:|---:|
| `klass` (named class, control) | 6.6 | 6.7 | 6.6 |
| `lambda` (non-capturing) | 6.9 | 6.9 | 122.0 |
| `mref` | 6.9 | 6.9 | 119.7 |
| **`cap` (capturing)** | **7.2** | **125.1** | 124.0 |

**17.4x on the capturing row**, and it lands at the named-class control plus
0.6 ns — which is about what one load off the receiver should cost. Ranges do
not overlap: A `[7.0 … 7.3]` against B `[123.3 … 128.2]`.

Three controls make that a measurement rather than a number:

* `klass` is unmoved across all three arms, as it must be — nothing here
  touches a named class's call site.
* `lambda` and `mref` are IDENTICAL in A and B (6.9 both). The capture switch
  moved only what it claims to; had it moved the non-capturing rows, the arms
  would not be measuring what their names say.
* `cap` in B ≈ `cap` in C (125.1 against 124.0). For a capturing lambda,
  turning off the capture half alone is the same as turning off the thunk
  entirely — which is the statement that B is a real "before".

Every run of all seventy-two printed the same `sink=71449096416`.

`probes/LambdaCaptureAdapterProbe.java` — seventeen capture shapes, including a
negative `byte`, a `char` above `0x7FFF`, a `null` reference, three captures at
once, and two instances of one lambda holding different values — is
byte-identical to HotSpot's output on both arms, with
`site_adapters=15 site_cap_adapters=14` printed beside it. The second number is
the one that matters: fourteen CAPTURING sites were dispatching through a thunk
while those lines were produced.

Its engagement is modest on purpose — `site_no_code=895000` of
`site_calls=1095000`, because 300 000 iterations across seventeen distinct impls
does not give the background compiler time to publish them all. The probe's job
is agreement across shapes; the fixture pair in §5 carries the engagement
burden.

Its captures go through one-line identity methods (`i32`, `i64`, …) for a
reason worth repeating: `final int k = 7;` is a *constant variable* in the JLS
sense and javac inlines it before desugaring the lambda, so the obvious way to
write this file produces seventeen NON-capturing lambdas whose comments claim
otherwise. `javap -p` on the class is the check — every `lambda$main$N` must
take more parameters than its SAM.

#### The reference-capture gate: inert AND unnecessary

The capturing thunk shipped with one restriction — a REFERENCE capture was
refused whenever `narrow_oops_block_inline_fields()` held (compressed oops on,
or ZGC's read barrier armed), by analogy with the inline `getfield` codegen,
which refuses under exactly that condition.

Two things were wrong with it, pointing in opposite directions.

**It was inert.** Compressed oops is opt-in (`CRATONVM_COMPRESSED_OOPS`) and
ZGC's barrier never arms in a default run, so the predicate is false throughout
and reference captures were already being thunked. Nothing about the default
configuration changed when the gate came out, and no number below should be read
as saying otherwise.

**It was also unnecessary where it did fire.** That predicate guards the
emission of a COMPACT slot read — a compact reference field narrows to four
bytes under compressed oops, and it is the compact and array decode paths that
ZGC's colouring reaches. This emitter never emits one: the compact-layout
refusal above guarantees every capture load addresses a legacy 16-byte `Value`
cell. A legacy cell is neither narrowed (`narrow_oop::ref_field_size` is
documented as the width of a *compact* instance field) nor barriered — ZGC
applies `load_barrier_slot` in `get_array_element`, while `get_field`'s legacy
arm is a bare `std::ptr::read::<Value>`. The refusal diverted a reference
capture to a Rust arm that reads the identical word in the identical way.

`gc/tests/lambda_proxy_capture_word.rs` makes that a checked claim rather than a
code reading: it compares the emitter's baked address and width against **each
collector's own `get_field`**, with compressed oops on and with the ZGC barrier
armed, having first asserted a proxy is legacy-laid-out on every backend so the
rest cannot agree about the wrong object. Reading the wide payload at the tag
word instead turns five of its six tests red.

One binary, four arms, `A B C D D C B A` per round, three rounds, on a busier
box than the table above — hence the wider spreads; the separation is 15x and
the noise is 2x:

| row | A: default, thunk | B: default, Rust | C: **oops on**, thunk | D: oops on, Rust |
|---|---:|---:|---:|---:|
| `klass` (control) | 7.9 | 8.0 | 8.8 | 9.4 |
| `lambda` | 8.6 | 8.9 | 8.8 | 8.5 |
| `cap` (`int` capture) | 9.1 | 154.8 | 9.2 | 156.6 |
| **`capref` (reference capture)** | **9.9** | 150.8 | **8.7** | 148.0 |

Column C is the configuration the gate used to refuse; a reference capture costs
the same there as anywhere else. All 96 runs printed `sink=71449096416`.

Engagement, from the pair that states it best — `capref` under compressed oops,
20 000 000 dispatches:

* thunk ON: **zero** `[LAMBDA-JIT]` census lines. The census prints every N
  *direct* calls and there were none, so Rust is not on the path at all.
* thunk OFF: `site_calls=20100000 site_direct=20100000 site_cap_adapters=0` —
  every one of them through Rust.

That asymmetry is the engagement statement here, and it is the shape
`lambda_site_prof::SITE_ADAPTERS`'s own comment predicts: a per-call counter
necessarily goes quiet exactly when the fast path starts working.
`jit::lambda_adapter`'s
`a_reference_capture_is_still_served_under_compressed_oops` pins the behaviour
directly, since nothing in a default run can tell the two versions apart.

#### Does any of this reach a real workload? Mostly not — measured

Every number above is a microbenchmark. `getResources` laziness was 20x on its
microbench and 0% on the workload it was built for, so the question has to be
asked rather than assumed.

**First the instrument had to be fixed, and how it failed is the more useful
half.** `CRATONVM_DBG=lambda-jit` printed every 200 000 *eligible* dispatches or
100 000 *direct* calls — thresholds sized for a probe doing millions of one
shape. A census over 24 Tomcat JUnit classes (129 s of real work) printed
**nothing at all**, and the obvious reading — "no lambda activity" — is one the
instrument cannot support: 199 999 eligible dispatches with fifty installed
thunks looks identical. Silence below a threshold no application reaches is not
evidence. The census now also dumps once at exit
(`report_lambda_census_at_exit`), which is what made everything below
measurable.

With that, real Tomcat code does reach the feature:

| class | eligible | fast_returns | site_calls | adapters | **capturing** |
|---|---:|---:|---:|---:|---:|
| `TestFilterValve` | 3 498 | 719 | 90 | 6 | **2** |
| `TestHttpServletDoHead…1024` | 87 086 | 82 111 | 771 | 4 | **1** |
| `TestHttp11InputBuffer` | 8 308 | 4 622 | 358 | 3 | **1** |

Across the 24-class sample, 7 classes installed thunks, 24 sites in all, and
roughly a third of those are capturing. So capturing SAM sites are not a
microbenchmark artefact — ordinary framework code has them, and they do get
thunks.

**And it does not matter.** Same binary, kill switches, `A B C C B A`, three
rounds, on the class with the most lambda traffic in the census:

| arm | wall (ms) | range |
|---|---:|---|
| A — everything on | 45 655 | [39 386 … 53 373] |
| B — capture thunk off | 46 646 | [38 767 … 51 551] |
| C — whole lambda tier-up off | 50 646 | [44 902 … 58 766] |

A against B is 2%, with ranges that overlap almost entirely: **the capturing
thunk's effect on this workload is below the noise floor**, and the honest
statement is that this measurement cannot see it. A against C suggests ~10% for
the feature family as a whole, but those ranges overlap too and six runs on a
shared box that varies 39–58 s for identical work cannot resolve it — it is a
hypothesis for a quieter box, not a result.

The arithmetic says why, and would have predicted it: `site_calls` is in the
hundreds per process, and the thunk saves ~140 ns a call. That is tens of
microseconds against a 45-second run. The microbenchmark is 2 000 000 calls of
one shape; a JUnit class is a few hundred, because a short-lived process barely
compiles its callers — note `fast_returns=82111` against `site_direct=197` on
the DoHead class, i.e. the *interpreter's* one-shot arm served four hundred
times more lambda calls than the JIT-side one did.

**So the value of this work is not in Tomcat's test suite.** It is in the shape
of workload where a SAM call site is genuinely hot and the caller is genuinely
compiled — a long-lived server loop, a stream pipeline over a large collection —
which is what the microbenchmark stands in for and what this suite is not. That
is a claim about applicability and it is still unmeasured; anyone extending this
page should measure a long-running workload before quoting the 17x as anything
other than what it is.

#### What the fixture had to learn

`captureShapesChecksum` covers one capture of each width, and every lambda in it
captures exactly ONE value. A deliberate break that ignored the capture index
and read every capture from cell 0 therefore passed the whole suite, engagement
assertions included — the offsets were all zero anyway. `multiCaptureChecksum`
exists for that: three lambdas holding two captures each, combined
non-commutatively, one pair of equal width so the index is isolated from the
load. With it, the same break fails with an access violation.

A second break — emitting the int-category load zero-extending instead of
sign-extending — passed, and that one is *correct*: an int-category parameter is
stored to a frame local and read back 32 bits at a time, so the upper half is
don't-care. `MOVSXD` is emitted because it is what the neighbouring code emits,
not because anything can see it. The comment on `CaptureLoad::Int` says so.

## 5. What pins it

For the capturing thunk, a PAIR:
`vm/tests/lambda_capture_adapter_tests.rs` runs three fixtures through the
thunk and asserts both the values and
`lambda_jit_capture_adapter_installs() > 0`;
`lambda_capture_adapter_off_tests.rs` runs the same three with
`CRATONVM_JIT_LAMBDA_CAPTURE_ADAPTER=0`, asserts the SAME values, and asserts
zero installs. Neither is worth much alone — the first could agree with a
broken Rust arm, the second could pass while no thunk was ever built. Together
they say the two independent implementations of "read the captures and call the
impl" agree, and that both ran. The expected values are computed in Rust and
were checked against HotSpot before being written down.

`CRATONVM_DBG=lambda-jit` prints `site_cap_adapters` beside `site_adapters` for
the same reason the pair exists: the total stays healthy on a workload full of
non-capturing lambdas whatever happens to the capturing ones.

For the original two halves:
`vm/tests/lambda_jit_tierup_tests.rs` and `lambda_jit_oneshot_tests.rs` — the
same twelve golden checksums from a real JDK, run against each half (the second
sets `CRATONVM_JIT_LAMBDA_SITE=0`, which sends a compiled caller's SAM call back
down the generic path and therefore through the interpreted one-shot) — plus
`lambda_jit_engagement_tests.rs` and its `_oneshot_` mirror, which assert that
the half under test actually served the calls.

**That last pair exists because the first version of this suite was worthless
and said nothing about it.** Twelve tests, all green, at 4 000 iterations an arm
— and with a deliberate off-by-one planted in the one-shot's return conversion
and its implicit-NPE drain deleted outright, **eleven of the twelve still
passed**. They had computed their answers in the interpreter and agreed with
HotSpot about a path they never took. Three separate things were wrong and all
three had to be fixed:

1. **4 000 iterations** cannot outlast an asynchronous compile. Now 200 000.
2. **Which half serves a call is decided by whether the CALLING frame is
   compiled**, and a loop sitting directly in a test method leaves that to an
   OSR race the test cannot see. Every SAM call now also goes through a one-line
   static `step` hop, which compiles on its own invocation count, so both call
   shapes are exercised by every arm.
3. The suite had no way to say whether any of it happened. The engagement tests
   read the counters and assert a floor.

A fourth attempt was made and REVERTED, and it is the useful one to record:
`CRATONVM_BG_COMPILE=0` looks like the way to make "the body is compiled by
iteration ~501" deterministic — compilation moves onto the mutator at the
threshold instead of racing a worker. Measured across this fixture it compiles
about **6%** of what the background worker does (`eligible=6 200 000` against
`compiled_hits=398 209`), because the inline `try_jit_upgrade_with_gate` route
declines bodies the worker admits. Determinism bought by suppressing the thing
under test is not determinism.

What the suite proves now, on the default configuration:

* Both engagement tests pass, and the census they print separates the halves
  cleanly. Default configuration: `site_direct=398 907` of the 400 000 SAM calls
  that come through the compiled `step` hop, with the other 400 000 (called
  straight from the interpreted loop) served by the one-shot,
  `fast_returns=399 116`. With `CRATONVM_JIT_LAMBDA_SITE=0`: `site_calls=0` and
  `fast_returns=599 109` — every dispatch through the interpreted half, which is
  what that configuration is for.
* Breaking the JIT-side arm (`Some(rc)` → `Some(rc + 1)`) turns
  `lambda_jit_tierup_tests` red; breaking the one-shot's return conversion turns
  `lambda_jit_oneshot_tests` red. Both were re-run after every change to the
  fixture, and both are red for the current one.
* The thunk has its own pair. `lambda_jit_adapter_engagement_tests` asserts both
  that a site got one and that Rust LEFT the path (`site_direct < 10 000` of
  800 000 dispatches), and `probes/LambdaAdapterProbe.java` diffs twelve
  argument shapes — two and three arguments, `long`, mixed widths, references,
  `void`, zero arguments, a `double`, a polymorphic site, a throwing body —
  against HotSpot's own output. Reversing the slide and removing it entirely
  both turn the engagement test red with wrong sums, which is what says the
  shuffle is under test rather than merely present.

That last check also caught the probe testing nothing: with all twelve loops in
`main` the census read `site_calls=0 site_adapters=0` across 3 400 000
dispatches — the frame was too large to compile, so no inline-cache call site
existed and the whole file agreed with HotSpot about the interpreter. One tight
loop per method, taking the SAM as a parameter, is what made it real.

One honest limitation: under the default asynchronous compiler, a given run
turns exactly the arm that won the compile race red — one test of the twelve,
and a different one each time. The suites catch a break; the engagement tests,
not the checksums, are what carry the "and the path really ran" burden.
