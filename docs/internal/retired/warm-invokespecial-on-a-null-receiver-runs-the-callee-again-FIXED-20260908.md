# A warm `invokespecial` on a null receiver runs the callee with `this == null` again — FIXED, and this page's "where to look" named the wrong tier

**Status:** FIXED and retired 2026-09-08, the day it was opened. Both defects
this page recorded are closed, and both of its diagnoses were wrong; the
corrections are the useful part.

```
warm-invokespecial=NPE
warm-invokevirtual=NPE
warm-invokeinterface=NPE
```

`vm/tests/null_receiver_cached_invoke.rs` — **5 runs, 5 green**, both arms
(interpreted and JIT), against the 4-of-5 red recorded below.
`vm/tests/lambda_safe_unmodifiable_map_classcast.rs` — **3 of 3**, against 3 of
3 red. Release binary, Linux build host, real JDK 25.

---

## Defect 1: it was the OPTIMIZING tier's own inliner, not `execute_invokevirtual_cached`

This page sent the next reader to `execute_invokevirtual_cached`'s `Bytecode`
arm and its null-receiver guard, "whether it still runs". It does — and so does
the single-pass backend's direct-call guard (`cd451facc`). Neither was reached,
because neither emitted the code that ran.

**The measurement that settled it.** A probe with two private callees behind one
50 000-call warming loop — one trivially small, one 80 statements long and far
past any inline budget — reported `NO-THROW` for BOTH:

```
warm-small=NO-THROW(3)     HotSpot: NPE
warm-big=NO-THROW(440)     HotSpot: NPE
```

which rules out the single-pass inliner, since that one refuses the large callee
outright (`inline-refused … reason=callee-too-large`). **That arm is now checked
in**, as `warm-invokespecial-big` in `null_receiver_cached_invoke.rs`: a small
callee cannot tell the two inliners apart — both claim it, and a fix in either
makes the site green — which is why the guard that landed in the single-pass
backend made this file look permanently fixed. Then `CRATONVM_DBG_JITC=1` on the
same probe named the artifact that actually ran:

```
[ir] inline-plan NRProbe$Impl.callBig: 1 site(s), 1 spliced body, 244 bytes appended
[ir] spliced 1 callee body into NRProbe$Impl.callBig
[cratonvm-jitc] full-compile NRProbe$Impl.callBig entry=… len=415
```

The C2/IR tier has an inliner of its own, with a budget of its own
(`IR_INLINE_MAX_TOTAL_BYTES`, four times the single-pass limit).
`IrBuilder::begin_splice` (`jit/src/ir.rs`) bound argument 0 straight into callee
local 0 and walked into the body. JVMS §6.5's NPE lives at the invoke; splicing
deletes the invoke; nothing put the check back. A callee body is not obliged to
touch `this` — `private int small() { return 3; }` does not — so there was no
fault to fall back on either. That is the whole mechanism, and it is why the
guard the single-pass backend gained was invisible on every method the
optimizing tier claimed.

**The fix.** `IrInlineSite` now carries `receiver_is_arg0` (`!callee_is_static`,
supplied by `append_ir_inline_site`), and `begin_splice` emits, ahead of the
splice, an `Op::Guard { bci: pc }` whose condition is
`Cmp(Ne, receiver, aconst_null)` — `aconst_null` and not `iconst(0)`, because
`ir_lower`'s `Op::Cmp` widens to a 64-bit compare only when an operand is
`Ref`-typed and a 32-bit one would read a pointer whose low word is zero as
null. The `ifnull` arm makes the same choice for the same reason.

Taking the guard reconstructs the frame for that bci and the interpreter
re-executes the invoke, where the existing null-receiver guard in
`execute_invokevirtual_cached` sends it to the slow path and the canonical NPE —
message, JEP 358 action and all — is raised by the code that owns it.
Re-executing is sound because the guard fires before a single byte of the callee
has run. This is HotSpot's own answer to the same question.

**And the guard is skipped when the receiver cannot be null**, which is not
only two saved instructions. `Op::Guard`'s reference inputs are `GlobalEscape`
to escape analysis — `ir_op_to_ea_op` funnels `Op::Cmp` into `EaOp::Other`,
whose arm republishes every reference operand — so guarding the receiver of
`new Vec3(…).add(…)` would take scalar replacement away from exactly the shape
the IR inliner exists to enable, the `per-voxel-allocation` case where splicing
the accessor chain is what lets the object die where it is used. The predicate
is `ir_check_elim::definitely_non_null`, reused rather than copied: a successful
`Op::New`/`Op::NewArray` returns a non-null reference and a failed one returns
the deopt sentinel and never reaches a use, so nothing is given up.

Two conditions on the guard, both structural rather than argued:

* **the frame state must exist.** `resolve_frame_state_for_bci` answers a bci it
  has no snapshot for with an EMPTY `FrameState` rather than an error, which
  would park the interpreter at the invoke with no operands. The main walk
  pushes a snapshot at the top of every bci, so this holds by construction for a
  top-level splice; it is checked anyway, and a miss refuses the compile.
* **a guard inside an already-open splice** resolves through `resume_bci` to the
  OUTERMOST invoke, so taking it re-executes that whole call including any
  spliced prefix that already ran. `spliced_bodies_pure` is the clause that
  makes that harmless — the same clause `trap_replay_is_safe` asks — and when it
  does not hold the graph raises `splice_guard_seen` and is refused, exactly as
  `add_div_zero_guard` and `plant_uncommon_trap` already do.

**Ruled out correctly, for the right reason.** The page's elimination of the
2026-09-07 deopt-sink family with `CRATONVM_JIT_DEOPT_SINK_RESUME=0` was sound —
the failure was identical with the switch on and off — and so was its reading of
the three-way split: `invokevirtual` and `invokeinterface` really are correct
only incidentally, through their inline cache's receiver-class test. The step
that did not follow is "so look at the `Bytecode` arm". That arm was innocent,
and a `grep` for the guard found it present and correct, which is exactly the
shape of evidence that sends a reader in a circle.

---

## Defect 2: `lambda_safe_unmodifiable_map_classcast` — the probe encoded a contract no JVM honours

This page recorded it as "delivers the wrong object". It does not. The object is
right; the exception's **wording** is what varied, and the probe's rule was
wrong in two independent ways.

**The oracle, which nobody had run.** Under Temurin 25.0.4 the probe's own
source fails on its FIRST call:

```
Exception in thread "main" java.lang.ClassCastException:
  class java.util.ImmutableCollections$MapN cannot be cast to class java.lang.String
  (java.util.ImmutableCollections$MapN and java.lang.String are in module java.base
   of loader 'bootstrap')
	at LambdaSafeUnmodifiableMapClassCastProbe.safelyApply(…:12)
```

The probe asked `ex.getMessage().startsWith(value.getClass().getName())`.
HotSpot has prefixed cast messages with `class ` since JDK 11, so that test is
false on every JVM. Spring Boot's real `LambdaSafe.startsWithArgumentClassName`
accepts **both** prefixes — the bare name and `"class " + name + " "` — and the
probe had copied only half of it. The probe is now Spring's rule.

**And CratonVM's message was load-order dependent**, which is why the probe ever
passed and why it started failing. `klass_origin`
(`vm/src/runtime/exceptions.rs`) can only produce HotSpot's wording when it can
name both operands' module and loader, and it did that by looking the class up
in the LOADED set. CratonVM's immutable collections are internal stamps that
`cce_display_class_name` deliberately renders as the JDK class they stand in for
— a name with no loaded class behind it — so one run printed two different
messages for the same failure:

```
empty: [java.util.ImmutableCollections$MapN cannot be cast to java.lang.String]
many:  [class java.util.ImmutableCollections$MapN cannot be cast to class java.lang.String
        (… are in module java.base of loader 'bootstrap')]
```

decided by nothing the program did: `Map.of(k,v,k,v)` loads the real `MapN`,
`Map.of()` does not. Confirmed by forcing the load — a `Class.forName` on `MapN`
ahead of the first call flips `empty` to the full wording and leaves `Map1` and
`Collections$UnmodifiableMap`, still unloaded, on the bare one.

**The fix.** `klass_origin` answers an unloaded name whose package the module
registry places in **`java.base`** with java.base / `'bootstrap'`. That is not a
guess: `java.base` has exactly one defining loader in every JVM, and the module
system's own bootstrap depends on it. Deliberately `java.base` ALONE — every
other module's loader is a real question (`java.sql` is the platform loader, an
application module's is the app loader) and answering it from a name would put a
fabrication in a string that log-scrapers parse; those keep today's bare
wording. All five shapes now print byte-identically to HotSpot.

The test also gained the assertion its own title always claimed and never made:
that the **message** names the Java-visible concrete class, not only that
`getClass()` does. `LambdaSafe` reads the message, so a private stamp leaking
into it is the failure that page was written about, and nothing checked for it.

---

## What the two defects had in common, which the page half-saw

Its closing line asked whether three dispatch defects in one week were "a
coincidence or a shape; nothing here settles which". They are a shape, and it is
not lambdas. **Both are a check that exists on one lowering path and not on its
sibling** — the null-receiver guard present in the interpreter and the
single-pass backend and absent from the IR tier's inliner; the callee-identity
check present at the lambda-site door and absent from the dispatch helper. The
third member of the set (`lambda-callee-deopt-is-orphaned-by-the-sam-name-check`)
is the same shape again. A fix that lands on one door and is not carried to the
others reads as fixed by every test that exercises the door it landed on.

## The gate

`cargo test --workspace` on this branch, and the five files directly at issue:
`null_receiver_cached_invoke` (2/2, ×5 runs), `jit_null_receiver_npe`,
`lambda_safe_unmodifiable_map_classcast` (×3), `jit_lambda_door_deopt_resumes`
(un-ignored, ×12) and `jit_bridge_sink_resumes_instead_of_rerunning`.

Everything below is the original record, kept for its reasoning and its
measurements — including the two diagnoses corrected above.

---

## The original record

| | |
|---|---|
| **Status** | OPEN, and it has a test that already fails. `vm/tests/null_receiver_cached_invoke.rs::warm_null_receiver_invokes_throw_npe_jit`. |
| **Severity** | **High.** Silent JVMS §6.5 violation — a private/super call executes its body with a null `this`. No exception. The named consequence, from the test's own header, is a bogus `NullPointerException: Cannot read field "interfaces" because "rd" is null` at `Class.java:1217` instead of a plain NPE. |
| **Opened** | 2026-09-08 |
| **Found by** | Running `cargo test --workspace` as a landing gate for unrelated work. It is not a new test and not a new gate; it is failing now. |

## What it reports

```
warm-invokespecial=NO-THROW(3)
warm-invokevirtual=NPE
warm-invokeinterface=NPE
```

`NO-THROW(3)` is the private method's body returning its value, executed with
`this == null`, after the call site's monomorphic inline cache is warm. The
other two invoke kinds are correct, so this is specific to the `Bytecode` arm of
`execute_invokevirtual_cached` — the arm that serves `invokespecial`, i.e.
**every private and `super` call**.

That is exactly the defect the file was written to pin. From its own header:

> `execute_invokevirtual_cached`'s `VirtualBytecode` arm has always deferred a
> `Value::Object(None)` receiver to the slow path … Its `Bytecode` arm — which
> serves `invokespecial` — and its `Native` arm never got the same guard.
>
> Measured before the fix: the FIRST `callPrivateOn(null)` throws NPE correctly,
> and after 50 000 warming calls the SAME site returns `3`.

So the guard that fixed it is not holding any more, or is not being reached.

## Rate

`cargo test -p cratonvm-vm --test null_receiver_cached_invoke`, run alone:

| runs | result |
|---|---|
| 5 alone | **4 failed**, 1 passed |
| 5 with the deopt-sink resume ON | **5 failed** |
| 2 with the deopt-sink resume OFF | **2 failed** |

The cold half of the same file (`1 passed` in every run) keeps passing — a cold
`invokespecial(null)` still throws. It is the warm answer that is wrong, which is
the asymmetry the file's last line warns about: *"a cold-only test passed
throughout the entire lifetime of the bug."*

## It is NOT the 2026-09-07 deopt-sink family

The obvious suspicion, given the date, is that one of the four sink fixes did
it — a null receiver on an `invokespecial` IS a deopt guard in the optimizing
tier (`emit_deopt_if_zero(bci, DeoptReason::NullCheck)` on the receiver), and
those sinks changed what happens when such a guard fires.

**Ruled out with the family's own kill switch.** `CRATONVM_JIT_DEOPT_SINK_RESUME=0`
restores every one of those four sinks to its pre-fix behaviour, and the failure
is **identical with it on and off** (5 of 5 against 2 of 2 above). Whatever this
is, it is upstream of, or beside, the resume machinery.

## Where to look

`execute_invokevirtual_cached`'s `Bytecode` arm and its null-receiver guard —
whether it still runs, and whether the warm path now reaches the callee by a
route that bypasses it (a direct compiled entry, a MIC/PIC slot, or the JIT
dispatch helper) rather than through the arm the guard lives in. The three-way
split in the output is the strongest hint available: `invokevirtual` and
`invokeinterface` go through the `VirtualBytecode` arm and are correct;
`invokespecial` goes through `Bytecode` and is not.

## Reproducing

```bash
CRATONVM_BIN=<a release cratonvm> \
cargo test -p cratonvm-vm --test null_receiver_cached_invoke
```

Expect `warm-invokespecial=NPE`. `NO-THROW(3)` is the defect. Run it more than
once — it is ~80% and the cold arm always passes.

## A second one from the same gate run, unrelated mechanism

`cargo test --workspace` on `dev` is red for two independent reasons today. The
other is `lambda_safe_unmodifiable_map_classcast`, and unlike the one above it
is **deterministic — 3 of 3 alone**:

```
java/lang/ClassCastException: class java.util.ImmutableCollections$MapN
    cannot be cast to class java.lang.String
  at LambdaSafeUnmodifiableMapClassCastProbe.check(…:24)
  at LambdaSafeUnmodifiableMapClassCastProbe.safelyApply(…:12)
```

A `Map` reaching a `String` cast through a lambda's `safelyApply`. Different
mechanism from the null-receiver defect above — that one skips a check, this one
delivers the wrong object — but recorded here rather than on its own page
because the useful fact is the pair: **the workspace gate is not green on `dev`,
for two reasons, neither of them anybody's landing.** Whoever picks either up
should run `cargo test --workspace` first and see what else has joined them.

Both are lambda- or dispatch-adjacent, as is
`lambda-callee-deopt-is-orphaned-by-the-sam-name-check-20260908.md` found the
same day. Three defects in one week in the dispatch paths is either a coincidence
or a shape; nothing here settles which.

## Related

- `sealed-derencodable-getinterfaces-npe-mockito-x509-FIXED.md` — the field
  instance the test header cites, and what this failing again puts back in play.
