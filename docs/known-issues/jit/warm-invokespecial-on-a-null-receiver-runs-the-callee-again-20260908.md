# A warm `invokespecial` on a null receiver runs the callee with `this == null` again — the JVMS §6.5 regression its own test was written to stop

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
