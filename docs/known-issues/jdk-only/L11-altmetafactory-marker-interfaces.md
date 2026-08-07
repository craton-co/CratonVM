# altMetafactory marker interfaces were parsed nowhere and modelled nowhere

Status: fix written 2026-08-06 (lane L11). Not yet built or run — this worktree
cannot build. Verification command at the bottom.

## The failure

`regression-suite/src/RJdkLambdas.java` fails in **both** `--real-jdk` and
`--jdk-only`; HotSpot 25 passes it (exit 0). Ordinary Compatible-mode defect.

    CK RJdkLambdas capture sum=30
    CK RJdkLambdas methodrefs ok
    CK RJdkLambdas bridges=1 sorted=[a, bb, dd, ccc]
    Exception in thread "main" java/lang/AssertionError: altMetafactory marker interface not applied
        at RJdkLambdas.main(RJdkLambdas.java:238)
        at RJdkLambdas.explicitMetafactory(RJdkLambdas.java:224)
        at RJdkLambdas.check(RJdkLambdas.java:37)

Ordinary lambda capture, method references and bridges all pass. Only the
explicit-metafactory case fails, and only its last assertion:

    // RJdkLambdas.java:210-224
    CallSite cs2 = LambdaMetafactory.altMetafactory(
        lookup, "add", MethodType.methodType(Adder.class),
        new Object[] { (II)I, impl, (II)I,
                       LambdaMetafactory.FLAG_MARKERS, 1, Cloneable.class });
    Adder a2 = (Adder) cs2.getTarget().invokeExact();
    check(a2.add(1, 2) == 3, "altMetafactory call site");          // PASSES
    check(a2 instanceof Cloneable, "altMetafactory marker ...");   // FAILS

`a2.add(1,2) == 3` passing is the load-bearing detail: the call site links and
the SAM dispatches. Only the proxy's *interface list* is wrong.

## Root cause

Three independent gaps, all on the same axis. The brief's hypothesis (b) — "we
ignore altMetafactory's extra arguments entirely" — is correct for the
*bootstrap* path; the reflective path this test actually takes has that gap plus
two more.

### 1. The data model has no place for markers

`LambdaCallSite` (`classloading/src/resolution.rs:302-344`) records exactly one
interface — `functional_interface: Arc<str>` — plus a `serializable_flag: bool`
added in an earlier fix. There is no marker list, and a CratonVM lambda proxy is
a *synthetic* `ClassId` (`>= 0x8000_0000`) deliberately absent from the
ClassStore, so it has no `interfaces` vector to append to either.

This is the recorded "a fabricated stand-in declares no interfaces, so every type
test against it fails" shape, in its lambda-proxy form.

### 2. The type test never asks about markers

`lambda_proxy_satisfies` (`vm/src/runtime/interpreter/typecheck.rs:234-307`) is
the single choke point for `checkcast` / `instanceof` / `Class.isInstance` /
`Class.isAssignableFrom` on a lambda proxy — reached from
`runtime/interpreter/opcodes.rs:3160` and `:3720`,
`runtime/interpreter/lambda.rs:300` and `:654`, `jit/helpers.rs:6827` and
`:6892`, and `vm/vm_exec.rs:6798`. It consults `call_site.functional_interface`
and one special case for `java/io/Serializable`. Nothing else. No marker can ever
answer `true`.

### 3. Neither producer parses the FLAG_MARKERS block

`altMetafactory`'s bootstrap-argument block is:

    args[0] samMethodType, args[1] implMethod, args[2] instantiatedMethodType,
    args[3] flags,
    if flags & FLAG_MARKERS (0x2): markerCount, then that many Class
    if flags & FLAG_BRIDGES (0x4): bridgeCount, then that many MethodType

* **Bootstrap path** — `bootstrap_lambda` in `vm/src/runtime/invokedynamic.rs`
  read `bootstrap_arg_indices[3]`, masked it with `0x1`, and discarded the rest.
  The dispatch comment at the `LAMBDA_METAFACTORY` branch even asserted the
  extra args were "advisory and not needed for dispatch" — true of the bridge
  block, false of the marker block.
* **Reflective path** — the one this test takes.
  `LambdaMetafactory.altMetafactory` is a registered `Bridge` native
  (`native-builtins/src/lang_invoke.rs:5138`); the `invokedynamic` opcode is
  short-circuited in `invokedynamic.rs` and never reaches it, but a direct Java
  call like this test's does. That native reads `arr[3]`, masks `0x1`, and drops
  `arr[4..]` on the floor before calling `build_reflective_lambda_callsite`
  (`lang_invoke.rs:6029`), whose `ctx.register_lambda_proxy(...)` has no marker
  parameter at all (`native-api/src/registry.rs:578-607`,
  `vm/src/vm/vm_exec.rs:6649-6695`).

### 3b. …and the reflective CallSite cache aliases the two call sites

Found while fixing 3. `LambdaKey` (`lang_invoke.rs:5604`) keys only on the
*object identities* of `(invokedType, samMethodType, implMethod,
instantiatedType)`. The JDK interns `MethodType`s, so this test's

    LambdaMetafactory.metafactory(lookup, "add", ()LAdder;, (II)I, impl, (II)I)

and

    LambdaMetafactory.altMetafactory(lookup, "add", ()LAdder;,
                                     {(II)I, impl, (II)I, FLAG_MARKERS, 1, Cloneable})

produce a **bit-identical cache key** while needing different proxy interface
lists. Without a guard, the `altMetafactory` call returns the `metafactory`
call's cached `ConstantCallSite` and every marker fix above is invisible. The
same latent aliasing applies to `FLAG_SERIALIZABLE`.

## What changed

### `vm/src/runtime/invokedynamic.rs` (owned by this lane)

* Added `FLAG_SERIALIZABLE` / `FLAG_MARKERS` constants next to `ALT_METAFACTORY`,
  with the block layout documented (including where `FLAG_BRIDGES` sits, so the
  marker block's extent is derivable).
* Corrected the dispatch-branch comment that claimed the extra args were
  advisory.
* `bootstrap_lambda` now keeps the **whole** flags word (`alt_flags`) instead of
  just bit 0, derives `serializable_flag` from it, and when `FLAG_MARKERS` is set
  calls the new `read_marker_interfaces` to pull `markerCount` from
  `bootstrap_arg_indices[4]` and that many `CONSTANT_Class` names from `[5..]`.
  A short or malformed block yields the prefix it could read: a class file that
  disagrees with its own flags word is broken, but refusing to link would turn a
  wrong `instanceof` answer into a hard failure of an otherwise-working lambda.
* Added the marker side table, keyed `(vm_identity, proxy_class_id)`, mirroring
  the `LAMBDA_SINGLETON_CACHE` already in this file. It holds interned names, not
  `ObjectRef`s, so unlike that cache it needs **no** GC root scan and no
  post-compaction remap. An `ANY_LAMBDA_MARKERS` atomic gate keeps the hot
  `instanceof`-on-a-lambda path from taking the mutex in the (overwhelmingly
  common) no-markers-anywhere case. Bounded by `MAX_LAMBDA_PROXIES`.
* New public API: `record_lambda_proxy_markers`, `lambda_proxy_marker_interfaces`,
  and `lambda_proxy_marker_satisfies(shared, proxy, target_id, target_name)` —
  the last is what the type test calls. A marker satisfies the target when it
  *is* the target or extends it, so a marker of `java/util/List` also answers
  `instanceof Collection`.

Why a side table rather than a `marker_interfaces` field on `LambdaCallSite`:
that struct has **14 construction sites** across `classloading/src/resolution.rs`,
`vm/src/vm.rs` (8 of them), `vm/src/vm/vm_exec.rs`, `vm/src/runtime/lambda_proxy.rs`,
`vm/src/runtime/invokedynamic.rs` and `vm/src/runtime/interpreter/tests.rs`, and
Rust struct literals cannot take a default. Adding the field means eleven
out-of-file edits in files three other lanes are editing concurrently. The side
table costs one out-of-file line at the read site instead. If the lanes ever
converge, folding the table into `LambdaCallSite` is the right end state.

### Out-of-file (patches handed to the orchestrator)

* `vm/src/runtime/interpreter/typecheck.rs` — `lambda_proxy_satisfies` asks
  `lambda_proxy_marker_satisfies` before falling through to the functional
  interface. One insertion; fixes all eight consumers listed in §2 at once.
* `native-api/src/registry.rs` — new `register_lambda_proxy_markers` trait method
  with a no-op default body (same shape as `register_lambda_proxy`'s own default),
  so no other `NativeContext` implementor breaks.
* `vm/src/vm/vm_exec.rs` — implements it, forwarding to
  `record_lambda_proxy_markers`.
* `native-builtins/src/lang_invoke.rs` — the `altMetafactory` native parses the
  `FLAG_MARKERS` block out of `Object[]` (mirror → name via the existing
  `mirror_class_name`), passes it through `build_reflective_lambda_callsite`, and
  **skips the `LambdaKey` identity cache entirely when `flags != 0`** (§3b).

## Does the second lambda dispatcher share the gap?

The recorded "two lambda dispatchers" finding is about SAM **dispatch**
(`runtime/interpreter`'s `try_lambda_dispatch` using the *driven* loader override
vs `vm_exec.rs`'s `NativeContextImpl::invoke_virtual` using the *passive* one).
That split does not apply here: markers change **type tests**, not dispatch, and
type tests have a single choke point, `lambda_proxy_satisfies`. Both dispatchers,
the JIT `instanceof` helper, and the reflective `isAssignableFrom` in
`vm_exec.rs:6798` all route through it, so the one insertion covers every one of
them.

The duplication that *does* bite here is on the **producer** side: there are two
places that register a lambda proxy (`invokedynamic.rs::bootstrap_lambda` for the
opcode, `lang_invoke.rs::build_reflective_lambda_callsite` for a direct Java call)
and both dropped markers. Both are fixed. A third producer,
`native-builtins/src/serialization.rs:1250` (deserializing a `SerializedLambda`),
is intentionally untouched: `SerializedLambda` does not carry marker interfaces,
so there is nothing to restore.

## Verify

    javac -d regression-suite/classes regression-suite/src/RJdkLambdas.java
    ./target/release/cratonvm --real-jdk -cp regression-suite/classes RJdkLambdas
    ./target/release/cratonvm --jdk-only -cp regression-suite/classes RJdkLambdas

Expected in both arms, matching HotSpot:

    CK RJdkLambdas metafactory=42 marker=true
    CK RJdkLambdas checks=35
    PASS RJdkLambdas (35 checks)

Note `--real-jdk` currently fails EARLIER, at `RJdkLambdas.java:61`
(`identity implementation class must be synthetic (generated)`), on a
`Function.identity()` stand-in — a separate defect on a separate path. Fixing
markers will not clear that; use the `--jdk-only` arm (which gets past line 61)
as the marker oracle, or drive `explicitMetafactory()` alone.
