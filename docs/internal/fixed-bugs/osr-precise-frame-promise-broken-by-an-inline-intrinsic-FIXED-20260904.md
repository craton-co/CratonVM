# An inline intrinsic claimed a protected site the OSR admission had already promised

## Status

**FIXED 2026-09-04.** `compile_osr_artifact` no longer resolves the BOX_UNBOX
intrinsic for a call inside a `try` range. Regression vector:
`vm/tests/lambda_jit_tierup_tests.rs::test_npe_from_body`, **run alone** — the
arm that engages, and the one a full-file run does not.

## The symptom

A real `NullPointerException` walked straight past the `catch` in the very
method that raised it:

```java
Function<String, Integer> lengthOf = s -> s.length();
for (int i = 0; i < 200_000; i++) {
    String s = (i % 500 == 499) ? null : "abc";
    try { sum += lengthOf.apply(s) + stepFn(lengthOf, s); }
    catch (NullPointerException e) { caught++; }        // <-- not entered
}
```

HotSpot: `1210000`. CratonVM: the NPE escapes `npeChecksum` entirely, always at
`i = 2499`, 3 runs of 3. It is the ORIGINAL exception — `Cannot invoke
"String.length()" because "<local0>" is null` — not a second one.

`test_npe_from_body` passes when the file's twelve tests run together, and fails
alone. That is the same trap
`internal/fixed-bugs/jit-superseded-implicit-npe-leak-FIXED-20260903.md`
recorded for the same test: the siblings keep the compiler busy and the arm
never engages. A green file was not evidence; the whole diagnosis started from
running the one test by itself.

## What made it appear

Nothing in the exception machinery changed. `069e67b43` put the BOX_UNBOX
intrinsic family back to default-ON, and this shape's `Integer.intValue()` — the
unbox javac inserts for `lengthOf.apply(s) + ...` — is one of its two triples.

Three independent switches each made it clean, which is what named the site
rather than any one subsystem:

| arm | result |
|---|---|
| default | escapes, 3/3 |
| `CRATONVM_JIT_NO_BOX_UNBOX_INTRINSIC=1` | clean |
| `CRATONVM_JIT_OSR_EXC_TABLE=0` | clean |
| `CRATONVM_DEOPT_REAL=0` | clean |
| `--nojit`, `CRATONVM_JIT_THRESHOLD=1000000`, `CRATONVM_BG_COMPILE=0`, `CRATONVM_JIT_OSR=0`, `CRATONVM_JIT_LAMBDA_SITE=0`, `CRATONVM_JIT_LAMBDA_TIERUP=0` | clean |
| `CRATONVM_JIT_IR=0` | escapes |

Three of those four levers name one thing between them: an OSR compile, of a
method with an exception table, whose `intValue()` is an inline intrinsic.

## The mechanism

`compile_osr_artifact` admits a method with a non-empty exception table on ONE
promise (RBC.6b, `osr_exception_table_allowed`): every throwing site inside a
protected range publishes a **reason-9** (`DeoptReason::PendingException`)
precise frame, so a handler that reads locals gets real ones. The predicate that
checks it is `first_unsupported_precise_frame_site`, and it reads the
**bytecode** — where this site is an ordinary `invokevirtual` and passes.

Then, further down the same function, the BOX_UNBOX region replaces that invoke
with an inline load whose null-receiver edge is a **reason-6**
(`ReceiverTypeChanged`) deopt. The compile keeps an admission it no longer
satisfies, and the gate that would have refused the method has already run.

An optimisation that claims a site after the gate behind it has passed is
invisible to that gate. The gate is not wrong; its subject moved.

## The fix

Refuse the intrinsic for a `pc` inside the method's exception table, in
`compile_osr_artifact`'s BOX_UNBOX region — before the resolution, not after.
The two thin direct binds immediately below still take the site: they emit a
CALL through `jit_invoke_dispatch`'s ordinary machinery, which is what the
precise-frame contract is written for.

**The refusal has to be there, not in the codegen.** Declining inside
`jit/src/x64/bytecode_walk.rs`'s BOX_UNBOX region leaves `callee_entry` holding
the intrinsic sentinel and falls through to the plain direct-call path, which
emits a `CALL` to that sentinel value:

```
#  SIGSEGV at pc=0xffffffffffffffc7, addr=0xffffffffffffffc7
#  fault pc is in NO live registered code buffer
```

— measured on the first attempt at this fix. A site the resolver has already
claimed cannot be un-claimed downstream.

## What it costs

Nothing measurable on the workload the intrinsic exists for.
`probes/BlobStreamCostCpu.java`, `CRATONVM_DBG_ATOMIC_INTRINSIC=1`: **10
resolved sites before the fix, 10 after.** An autoboxed counter in a hot loop is
not inside a `try`; the sites this refuses are the ones whose exceptions have to
be catchable.

## Verification

| check | result |
|---|---|
| `test_npe_from_body` ALONE (`-- --exact`) | ok, 3 of 3 |
| `regression-suite/run.sh` (incl. `RJitLambdaNpeSupersede`) | 90 passed, 0 failed |
| `lambda_jit_tierup_tests`, all 12 | 12 passed |
| `pgo02_guarded_virtual_inline` | ok |
| `jit_guarded_inline_native_shadow` | ok |
| reduced probe vs HotSpot | `1210000` == `1210000` |

## The lesson worth keeping

**A gate that reads the bytecode is a claim about the bytecode, not about what
the compiler decides to emit for it.** Every admission predicate of this shape —
"every site of kind X publishes Y" — has to be re-asked, or made unreachable, by
each later stage that can substitute its own lowering for a site of kind X. The
BOX_UNBOX region is one such stage; the STRING_ACCESS, ARRAYCOPY, ATOMIC_INT,
ATOMIC_LONG and FFM_SEGMENT regions beside it are others, and this page does not
claim they are clean — only that this one was measured and is now refused where
the promise binds.
