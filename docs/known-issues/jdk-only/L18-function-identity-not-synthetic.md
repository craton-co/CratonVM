# `Function.identity()` answers a fabricated class in `--real-jdk`

Status: fix written 2026-08-06 (lane L18) as an **out-of-file patch** to
`native-api/src/registry.rs`. Not built or run — this worktree cannot build.
Verification commands at the bottom.

## The failure

`regression-suite/src/RJdkLambdas.java` fails in `--real-jdk` at line 61.
HotSpot 25 passes the whole class. `--jdk-only` gets **past** line 61 and fails
much later (line 224, marker interfaces — lane L11's defect), so the strict arm
is a working oracle for exactly this behaviour.

    --real-jdk
    Exception in thread "main" java/lang/AssertionError: identity implementation class must be synthetic (generated)
        at RJdkLambdas.main(RJdkLambdas.java:234)
        at RJdkLambdas.functionIdentity(RJdkLambdas.java:61)

    --jdk-only  (and HotSpot)
    CK RJdkLambdas identity synthetic=true lambdaShaped=true sameRef=true

## The predicate

Three assertions, not one — the fix has to satisfy all three, which rules out
renaming the stand-in or flipping a synthetic bit on it:

    // RJdkLambdas.java:59-67
    Class<?> k = id.getClass();
    check(Function.class.isAssignableFrom(k), ...);                       // 60, passes today
    check(k.isSynthetic(), "identity implementation class must be synthetic (generated)"); // 61, FAILS
    check(!k.getName().equals("java.util.function.Function$Identity"), ...);               // 62, would fail
    check(k.getName().contains("$$Lambda"), ...);                                          // 66, would fail

Line 62 names the fabricated class outright. No amount of dressing up
`Function$Identity` can pass it: the object must be a genuine generated lambda.

## Root cause

`Function.identity()` is intercepted by a native that returns a hand-made
stand-in whose class is an ordinary named synthetic class,
`java/util/function/Function$Identity`. Two registration sites, both
`NativeKind::SyntheticStub`, last-write-wins:

* `native-builtins/src/lib.rs:37936` — `register_function_identity_natives`,
  `Function.identity` + `UnaryOperator.identity` → `native_function_identity`
  (`lib.rs:37540`), which allocates `java/util/function/Function$Identity`.
  This is the copy a Compatible run dispatches.
* `native-builtins/src/phases_late/streams.rs:2786` — `Function.identity`, and
  `:2236` — `UnaryOperator.identity` → `java/util/function/UnaryOperator$Identity`.

`SyntheticStub` is dropped at registration under `CompatibilityMode::JdkOnly`
(`native-api/src/registry.rs`), which is precisely why the strict arm passes:
with no native, `java.base`'s own bytecode runs and the VM spins a real lambda
proxy. `Compatible` keeps every `SyntheticStub` registration, so it still gets
the stand-in.

The real bytecode is self-contained — verified against the JDK 25 image with
`javap -p -c java.util.function.Function`:

    public static <T> Function<T, T> identity();
         0: invokedynamic #12,  0   // InvokeDynamic #2:apply:()Ljava/util/function/Function;
         5: areturn
    private static java.lang.Object lambda$identity$0(java.lang.Object);
         0: aload_0
         1: areturn

`UnaryOperator.identity()` is byte-for-byte the same shape. Neither touches any
other class; there is no missing-native dependency behind them.

## (a) stop intercepting, not (b) make the stand-in honest

Fix **(a)**. The reasons, in order of weight:

1. **The real bytecode demonstrably works.** The strict arm runs it today and
   prints the HotSpot line verbatim. There is no gap to bridge.
2. **The `invokedynamic` opcode is short-circuited in
   `vm/src/runtime/invokedynamic.rs` and never calls the `LambdaMetafactory`
   natives** (stated at `native-builtins/src/lang_invoke.rs:5045-5050`). So the
   lambda-proxy machinery that produces the `$$Lambda` name and the synthetic
   bit is *the same code in both modes* — the strict arm's success transfers
   directly to Compatible. Nothing about strict mode's other drops is
   load-bearing for this call.
3. **(b) cannot pass line 62 anyway.** A truthful `$$Lambda` name on the
   stand-in would still be a fabricated class the test names and rejects, and
   `Function$Identity` has no class file in any JDK image — `--jdk-only`
   contract §5 forbids fabricating it at all.
4. It removes a divergence instead of adding one, and matches lane L10's
   `Executors` pool-factory drop immediately above the new block.

### How `synthetic-jdk` keeps working (STANDING RULE)

Nothing is deleted. The gate is `drop_real_layout_synthetic`, a registry flag
set **only** by `vm_init`'s real-JDK arms:

* `vm/src/vm/vm_init.rs:1601` — the `#[cfg(feature = "synthetic-jdk")]` block's
  `else` (real-JDK) branch.
* `vm/src/vm/vm_init.rs:2104` — the `#[cfg(not(feature = "synthetic-jdk"))]`
  default build.

A `synthetic-jdk`-feature binary running in synthetic mode never sets it, so all
five `Function$Identity` / `UnaryOperator$Identity` registrations stay registered
and the stand-in keeps working there — where it has to, because that mode has no
real `java.util.function.Function` bytecode to fall back to.

Only the two **factories** are dropped. `Function$Identity.{apply,andThen,
compose}` keep their registrations; with no factory they are simply unreachable
in real-JDK mode. This matters: the 2026-07-14 `d8092acb` regression
(`UnsatisfiedLinkError: Function$Identity.andThen` on WildFly boot) was caused by
dropping the *instance methods* while one of the two *factory* copies survived.
Dropping at registration by class+method covers **both** factory copies at once,
so that split cannot recur.

`Function.compose` / `Function.andThen` are deliberately left registered in
Compatible. Their composites (`Function$Compose` / `Function$AndThen`) reach the
receiver through `ctx.invoke_virtual(.., "apply", ..)`, which already supports
lambda-proxy dispatch, and RJdkLambdas lines 53-55 exercise exactly that chain
and pass today.

## The patch (out-of-file — `native-api/src/registry.rs`)

Inserted in `NativeMethodRegistry::register`, immediately after the L10
`Executors` pool-factory drop and before the regex block. `register_with_kind`
funnels through `register` (`registry.rs:5219`), so one filter covers both
registration sites.

    if self.drop_real_layout_synthetic
        && matches!(
            class_name,
            "java/util/function/Function" | "java/util/function/UnaryOperator"
        )
        && method_name == "identity"
    {
        return;
    }

Scoped by method NAME, like the `Executors` precedent. `identity` is the only
static factory on either interface; `apply`, `compose` and `andThen` keep
whatever registration they have.

Ratchet impact: `native-builtins/tests/stub_ratchet.rs` asserts
`synthetic <= BASELINE_SYNTHETIC_STUBS` and its boot census runs behind
`set_drop_real_layout_synthetic`, so a count that *falls* by two is fine. No
baseline edit needed.

## Verify

    javac -d regression-suite/build regression-suite/src/RJdkLambdas.java
    ./target/release/cratonvm --real-jdk -cp regression-suite/build RJdkLambdas
    ./target/release/cratonvm --jdk-only -cp regression-suite/build RJdkLambdas

or through the suite runner (which also diffs against HotSpot):

    JDK_ONLY=1 ONLY=RJdkLambdas bash regression-suite/run.sh
    CRATONVM_ARGS=--jdk-only ONLY=RJdkLambdas bash regression-suite/run.sh

The line this fix owns, expected in BOTH arms and matching HotSpot:

    CK RJdkLambdas identity synthetic=true lambdaShaped=true sameRef=true

Full pass in both arms additionally requires lane L11's marker-interface fix
(line 224):

    CK RJdkLambdas checks=35
    PASS RJdkLambdas (35 checks)

## What the `--real-jdk` arm should hit next

It has never executed anything past line 61, so lines 62-227 are unmeasured in
that mode. Ranked by risk:

1. **Line 188** — `Comparator.comparingInt(String::length)
   .thenComparing(Function.identity())`. Confirmed by `javap` to bind the
   `thenComparing(Ljava/util/function/Function;)Ljava/util/Comparator;`
   overload. Compatible keeps the fabricated `java/util/Comparator$Native`
   machinery (`native-collections/src/lib.rs:27584`), which strict refuses; the
   key extractor it stores is now a lambda proxy instead of a
   `Function$Identity`. That path already has a lambda-proxy arm
   (`native-collections/src/lib.rs:27616-27625`), so this should hold, but it is
   the one place where the fix changes an argument's shape for a native that
   only Compatible runs.
2. **Line 224** — `a2 instanceof Cloneable`, altMetafactory markers. Lane L11's
   defect, mode-independent (the `altMetafactory` native is `Bridge`, and
   `lambda_proxy_satisfies` is a single choke point), so this arm will fail here
   too until L11 lands.
3. Lines 99-162 (`capture`, `methodReferences`) and 167-184 (`bridges`,
   reflection) use no `SyntheticStub` surface that strict drops; strict passes
   all of them, so they should pass unchanged.

## Falsifier

If, with the two natives dropped, `--real-jdk` still fails at line 61 or 66,
then the lambda-proxy naming/synthetic-bit machinery is NOT shared between the
modes and the strict arm's `synthetic=true lambdaShaped=true` came from a
different path — in which case the `invokedynamic.rs` short-circuit claim above
is wrong and the fix must become (b), routing the stand-in through
`lambda_proxy_class_name` (`native-builtins/src/lang_class.rs`).
