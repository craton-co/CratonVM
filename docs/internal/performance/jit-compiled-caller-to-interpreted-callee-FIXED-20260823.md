# A compiled caller calling an INTERPRETED callee — FIXED for all four invoke kinds

**Status: FIXED 2026-08-23.** `invokestatic` was closed on 2026-08-22
(1310 → 259 ns/op). `invokevirtual`, `invokeinterface` and `invokespecial` are
closed here, each by the same mechanism: the call site's own cached interpreter
frame template, entered instead of the fully name-keyed generic dispatch. All
three are now BELOW the both-interpreted cost, so compiling the caller is no
longer a pessimisation at any invoke kind.

This was never a lambda bug, a reactive bug or a `java.time` bug: it was the
JIT's interpreter-fallback path, and it applied to EVERY compiled method that
called a callee the JIT did not compile — which, on any real application, is
most callees.

## The measurement

`probes/XferProbe2.java`, one binary, `CRATONVM_JIT_VIRTUAL_BYTECODE_CALLEE` as
the A/B, `CRATONVM_JIT_DENY` naming the callee so it stays interpreted while
the caller is still compiled. Three interleaved rounds, medians:

| arm | compiled → compiled | compiled → INTERPRETED, **before** | compiled → INTERPRETED, **after** | both interpreted (`--nojit`) |
|---|---:|---:|---:|---:|
| `invokevirtual` | 38.6 | **3698** | **420** | 536 |
| `invokeinterface` | 37.3 | **3488** | **417** | 466 |
| `invokespecial` | 50.2 | **3035** | **429** | 940 |

8.8x / 8.4x / 7.1x, and `sink` bit-identical in every arm.

**Read the RATIO, not the absolute.** This host's wall clock is not stable
across a day — a second interleaved session the same evening read
1682 / 1645 / 2311 for the same OFF arms and 247 / 251 / 360 for the same ON
arms, i.e. 6.4–6.8x for the identical binaries. The by-name arm is the noisy
one (2255–10787 ns/op observed across sessions) because it allocates and takes
a global `Mutex` on every call; the memo arm is tight (295–636) because it does
neither. What is invariant across every session run: **ON is below the
`--nojit` control in all three kinds**, which is the property that matters —
the JIT was actively losing to the interpreter it replaced.

The `--nojit` row is the control that makes the point. Before the fix,
compiling the caller and not the callee was 6.9x (virtual), 7.5x (interface)
and 3.2x (special) slower than compiling *neither*.

## Why it was slow

A compiled caller's call to an uncompiled callee fell out of
`jit_invoke_dispatch`'s fast arms into `crate::vm::invoke_or_native`, the fully
name-keyed generic dispatch: class lookup BY NAME, native-override arbitration,
a `CodeAttribute` clone, bytecode re-padding behind a global `Mutex` with a
full-body `memcmp`, and ~11% mimalloc. Every one of those is re-derivation of a
constant — the call site is monomorphic and the callee never changes.

The interpreter does none of it: its call sites hold a
`CachedInvokeTarget::Bytecode` with a prebuilt `Arc<CachedBytecodeMethod>`.
**The JIT's dispatch helper had a cache for a COMPILED callee
(`DISPATCH_CACHE`, `VIRTUAL_DISPATCH_CACHE`) and no cache at all for an
interpreted one.**

## The three new memos

All three enter the callee through `Frame::new_pooled_cached` +
`execute_prebuilt_frame`, the same shape `lambda.rs::try_invoke_cached_lambda_impl`
uses, and all three are `site_keyed_memos!` members, so the generation and
class-identity flushes that drop every other dispatch memo drop them too.

### `try_jit_virtual_bytecode_callee` — kinds 0 and 2

Keyed on **`(JitSiteKey, receiver ClassId)`**. The static half had no receiver,
hence no dispatch retarget, no interface rules and no receiver guard; all three
come back here and each is discharged rather than approximated:

* **The retarget IS the resolution.** `build_lambda_impl_cached` resolves
  through `find_method_recursive` starting at the RECEIVER'S OWN class id — the
  same walk `invoke_on_class_shared` performs after its retarget, and the same
  one the MIC's compile probe uses. A callee it cannot land on (an abstract
  declaration, an interface method with no implementation on this receiver) has
  no `Code` attribute and is refused rather than guessed at.
* **The receiver guard IS the key.** A hit is by construction an answer
  resolved for exactly this receiver class, so a site that goes polymorphic
  gets one entry per class instead of serving the wrong body.
* **Loader identity is not in play at all**, and this is the one place this path
  is STRICTLY safer than its static sibling. That one resolves its owner
  through `get_loaded_class_id(info.class_name)` — a class NAME, which two
  loaders can both define — and had to join
  `flush_class_identity_dispatch_memos` for it. Here the owner comes from the
  receiver's header. The memo joins that flush anyway, because a `ClassId` is
  reissued when a class is unloaded.

Array receivers are refused (`ObjectKind::Object` only): an array header carries
its COMPONENT class id, so `(site, class id)` does not identify one — the same
invariant `VIRTUAL_TARGET_CACHE` and the KC26 `array.clone()` note turn on.

### `try_jit_special_bytecode_callee` — kind 1

Keyed on **the call site alone**, and that is the definition of the opcode: the
target is chosen by the resolved constant-pool class, never by the receiver's
runtime class. A receiver-keyed memo here would be modelling a retarget
`invokespecial` explicitly does not perform — the one that turns a super-call
into a self-call and recurses until the stack ends (the picocli
`StackOverflowError` the kind-1 arm was written for).

It refuses unless `declaring_class_id` is present AND
`resolve_class_loader_aware` answers, so the owner is always a `ClassId`
resolved through the CALLER's loader
(BUG-JIT-INVOKESPECIAL-LOADER-20260726). It refuses `<init>` / `<clinit>` by
name rather than by consequence: `invoke_on_class_shared_inner` carries explicit
carve-outs for both, an `<init>` is where object initialisation and this VM's
synthetic layouts meet, and neither is a throughput path worth opening that
surface for.

### Shared refusals

Every one is the static half's, verbatim and for the same reason:

* **no native anywhere has this `(name, descriptor)`**
  (`might_have_method_descriptor`) — which removes the native-override,
  `SyntheticStub`-yield, redefine-shadow and
  `force_native_over_real_jdk_bytecode` questions rather than reproducing them;
* the DECLARING class is already initialised — `invoke_shared` would run
  `<clinit>` on the way in, and this path must never be the thing that skips it;
* `site_name_is_special_cased`;
* the resolved method is not `static`, and the decoded argument count matches
  `receiver + declared parameters`.

Per hit only what can change is re-tested: the declaring class's `RedefineGate`
and the process-wide `any_class_redefined` latch.

## Where they are spliced in, and the one gate that is not obvious

Three call sites: the generic dispatcher's 0/2 arm and its 1 arm — both of
which are the TAIL, i.e. `OUT_TAIL` is already counted and every
compiled-callee probe (`DISPATCH_CACHE`, `VIRTUAL_DISPATCH_CACHE`, `JitCache`,
the tier-up arm) has already declined — and both of
`jit_invoke_virtual_mic`'s resolving arms.

The MIC arms carry an extra gate: **the memo is taken only when the compile
probe produced no entry** (`callee_has_compiled_code` / `entry_ptr == 0`). The
by-name `invoke_or_native` below them WOULD have entered compiled code if the
callee had any, so an ungated memo could divert a compiled callee into the
interpreter. That is the one way this change could have made something slower,
and it is closed by a boolean.

## What the probe had to be fixed to measure

`probes/XferProbe2.java` as it stood could not measure two of the three kinds,
and both failures read like results:

* **the `iface` arm was measuring `invokestatic`.** Its suggested
  `CRATONVM_JIT_DENY=XferProbe2.calleeIface` denies a STATIC method that the
  interface body calls, not the interface call. Denying the interface body
  itself (`XferProbe2$Impl.apply`) then produced `out_virtual_bc=0
  out_virtual_bc_refused=1048575` — a refusal on every call — because `apply`
  is on `site_name_is_special_cased`'s deliberately over-broad list (it is one
  of the names `invoke_or_native`'s opening cascade can claim, via the
  `ToIntFunction.apply` SAM bridges). Correct behaviour, but a name-wide
  refusal reading like a kind-wide one. The new `iface2` arm names its SAM
  `compute`, and running both is what tells the two apart.
* **the `special` arm's callee was being INLINED.** A one-line
  `super.calleeSpecial` was `inline-planned … cost=4 budget_left=750` and
  spliced outright, so `CRATONVM_JIT_DENY` on it was a no-op — 24.2 ns denied
  against 25.8 ns undenied, which is the tell that the lever was not engaged.
  The callee is now a 64-arm `tableswitch`: far past the inline budget, still
  O(1) to execute, and every arm returns the same value so `sink` stays
  bit-identical across all arms — which is the check that the switch costs a
  dispatch and not a body.

## Instruments

`CRATONVM_DBG=mic-prof` reports `out_virtual_bc` / `out_virtual_bc_refused` and
`out_special_bc` / `out_special_bc_refused` beside the static pair. A version of
any of these that never fires is indistinguishable from one that fires and buys
nothing, which is the failure mode this census exists for.

`CRATONVM_JIT_VIRTUAL_BYTECODE_CALLEE=0` restores the by-name path for all
three kinds — verified to reproduce the pre-fix binary's numbers within its
noise, which is what makes the A/B a one-binary A/B.

## §3 — why this landed on reactive code hardest (unchanged, and still true)

A method containing an unbridged `invokedynamic` cannot run compiled, so it
becomes exactly the interpreted callee above. Two mechanisms, both named by the
VM itself under `CRATONVM_DBG=jitc`:

* **OSR is refused outright and permanently** — `osr-DENY (unbridged
  invokedynamic)`, then the method is OSR-denied for the rest of the process.
  The guard admits only `StringConcatFactory` sites, because every other
  bootstrap lowers to an unconditional frame-deopt and an OSR frame cannot take
  that trap safely. It is method-wide: one indy anywhere denies every loop in
  the method.
* **Whole-method compiles are undone at runtime** — the method compiles,
  executes the indy, hits the reason-8 stub, and
  `DeoptimizationController::deoptimize` with `action=MakeNotCompilable`
  retires it permanently.

`probes/IndyScopeProbe.java` isolates it. The SAM call itself is fine (32–35 ns
once the calling method is compiled); what is broken is that a method
*containing* an `invokedynamic` never stays compiled. **That is a separate open
item and is NOT closed by this page** — what this page closes is the cost of
calling such a method, which was 1900–3700 ns and is now ~420.

## What this is NOT

* Not lambda *dispatch*. `[LAMBDA-PROF]` / `[LAMBDA-JIT]` show
  `compiled_hits=1017675 declines=0`.
* Not tier-up thresholds: `CRATONVM_JIT_LAMBDA_TIERUP=0` moves the lambda arm 4%.
* Not the field-site cache. Its hit rate on the WebClient exchange is 83.9% at
  1024 slots and saturates at 92.6% by 32768, and buying those 95 000 misses
  back moved neither the exchange (30.2 → 30.2 ms/op) nor `ReactorProbe`
  (118k → 115k ns/op, inside noise). Sized and rejected.

## The one population number that WAS taken, and what it says

`CRATONVM_DBG=mic-prof` on `regression-suite`'s `RJitGc` — a GC-stress class,
not a microbenchmark:

```
kind_special=1_769_271  out_tail=1040
out_special_bc=0  out_special_bc_refused=1040
```

Two things follow, and the second is uncomfortable enough to state plainly:

* the fast arms already serve 99.94% of that class's `invokespecial` calls, so
  the tail this page is about is a thin slice of the total. A per-call
  multiplier on the tail is not a workload multiplier — see
  `a-multiplier-and-a-population-are-different-measurements`;
* **every one of the 1040 tail calls was REFUSED.** The most likely reason is
  the `<init>` / `<clinit>` refusal — a GC-stress class's `invokespecial` tail
  is overwhelmingly constructors — but that attribution is **not verified**:
  the census counts refusals without naming which gate took them, and
  `might_have_method_descriptor` would look identical here.

So the `invokespecial` half is proven on `probes/XferProbe2`'s `special` arm
(3035 → 429 ns/op) and has **zero measured population on the one real class
censused**. If it turns out `<init>` is where the volume is, admitting it is
the next piece of work — and it is deliberately the piece this pass did not
open, because `invoke_on_class_shared_inner` carries explicit carve-outs for
both names and an `<init>` is where object initialisation and this VM's
synthetic layouts meet.

Naming which gate refuses would take one counter per refusal reason, and is the
cheapest next instrument here.

## The suite-level population — ASKED above, ANSWERED here (2026-08-23)

The section this replaces closed by naming the next question: *"the
`out_virtual_bc` share of a real reactive workload's tail is unmeasured …
`CRATONVM_DBG=mic-prof` answers it in one run on any host that can boot the
workload."* It was run, on the Azure host that does have spring-webflux's test
classpath, over **300 `ExchangeProbe` exchanges** — the unit of work
`webclient-integration-tests-reactive-exchange-gap` is built from:

```
disp_calls=10232   mic_calls=4270   hit_entry=0   hit_noentry=3280
out_virt_bc=44     out_virt_bc_refused=3298
out_special_bc=0   out_special_bc_refused=0
```

(Verbatim from the run, which was taken on the branch binary before the merge
that kept dev's spelling: the slot dev ships is `out_virtual_bc`, and it is the
same counter. Grep for that one.)

**Ten thousand `jit_invoke_dispatch` calls for three hundred exchanges, and
`hit_entry=0`** — not one inline-cache dispatch found a compiled callee to
enter. The transition this page is about is reached about **11 times per
exchange**. At the ~1 600 ns it now saves per transition, that is **~18 µs
against ~30 000 µs of CPU per exchange: 0.06%.**

So the population answer for the workload this page was opened to explain is
"almost none", for the same reason `RJitGc`'s was: the fast arms and the
interpreter between them serve nearly everything, and **a reactive workload
barely enters compiled code at all**. The isolated per-call numbers are real
and the workload-level silence is real, and they do not contradict each other.

That also settles a measurement that would otherwise be re-run indefinitely.
Three ABBA rounds of the memo A/B on the exchange gave 33.3 / 33.0 / 46.3 /
49.6 / 34.1 ms per exchange with the memos ON against 31.3 / 45.3 / 39.4 / 30.8
with them OFF, and the startup-free `perf stat -e task-clock` marginal form was
no better (31.8-61.6 ON, 1.3-49.4 OFF, the 1.3 being a run that failed
outright). **The spread inside one arm exceeds any difference between arms**;
the census does not have that problem, and it is what a future reader should
reach for. **Do not re-open this page because a reactive workload did not
move — read `disp_calls` and `hit_entry` first.**

Still unmeasured: `probes/ReactorProbe.java`'s own tail split, which needs
reactor compiled against the probe rather than against the suite classpath.

Related:
`performance/webclient-integration-tests-reactive-exchange-gap-RETIRED-20260823.md`,
[[jit-entries-per-call-cost-is-the-call-dense-wall]].

## Reproducing

```bash
CV=<bin>
for arm in virtual iface2 special; do
  case $arm in
    virtual) D='XferProbe2$Impl.calleeVirtual' ;;
    iface2)  D='XferProbe2$Impl.compute' ;;
    special) D='XferProbe2$Base.calleeSpecial' ;;
  esac
  $CV --cp <probes> XferProbe2 2000000 $arm                                   # compiled -> compiled
  CRATONVM_JIT_DENY=$D $CV --cp <probes> XferProbe2 2000000 $arm              # ON
  CRATONVM_JIT_VIRTUAL_BYTECODE_CALLEE=0 CRATONVM_JIT_DENY=$D \
    $CV --cp <probes> XferProbe2 2000000 $arm                                 # OFF
  $CV --nojit --cp <probes> XferProbe2 2000000 $arm                           # both interpreted
done
```

Interleave the arms. All are pure CPU with no sockets, but this host's absolute
wall clock still moves by 2x across a session — see the note under the table.
