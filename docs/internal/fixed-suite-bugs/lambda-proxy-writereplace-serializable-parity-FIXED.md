# Every lambda proxy claimed to be `Serializable` — `writeReplace()` on all of them, and `instanceof Serializable` unconditionally true

**Status: FIXED — 2026-08-01**

Branch `fix/lambda-writereplace-serializable-20260801`, commits
`01360ca9ee` (fix) and `ff16baa7ea` (guard test). Worktree
`/data/data/wt-lambdaser-20260801` on the Azure host; probe and sweep results
under `/data/data/lambdaser-20260801/`.

Found while closing
`springboot/lambda-getgenericinterfaces-fabricates-parameterizedtype-FIXED.md`
— it was the single remaining difference in that doc's reflection-surface probe.

## Symptom

Two coupled surfaces on **every** CratonVM lambda proxy, serializable or not:

- `Class.getDeclaredMethods()` reported a synthetic
  `writeReplace()Ljava/lang/Object;` (and `getDeclaredMethod("writeReplace")`
  found it), so every lambda looked serializable to a reflective method scan.
- The `instanceof` / `checkcast` fast path returned `true` for
  `java.io.Serializable` unconditionally. Its own comment said so: *"We don't
  currently track the altMetafactory flags, so accept Serializable
  universally"* (`vm/src/runtime/interpreter/typecheck.rs`).

Measured against `jdk-25.0.3.9-hotspot` with `LambdaSerProbe.java` (5 lambda
shapes x {`instanceof Serializable`, `getInterfaces`, `getDeclaredMethods`,
`getDeclaredMethod("writeReplace")`, serialization round-trip}):

| `Supplier<String> s = () -> "x"` | HotSpot | CratonVM (before) |
|---|---|---|
| `instanceof Serializable` | false | **true** |
| `getDeclaredMethod("writeReplace")` | NoSuchMethodException | **found** |
| `(Serializable) s` | ClassCastException | **succeeds** |
| `writeObject(s)` | NotSerializableException | **InvalidObjectException** (a wrong, later failure) |

And, in the other direction, a genuinely serializable lambda was *under*-reported:

| `(Runnable & Serializable) () -> {}` | HotSpot | CratonVM (before) |
|---|---|---|
| `getInterfaces()` | `[java.io.Serializable, java.lang.Runnable]` | **`[java.lang.Runnable]`** |
| `writeReplace` modifiers | `private final` | **`private`** |

## Root cause

The JDK decides this in `AbstractValidatingLambdaMetafactory`: a lambda is
serializable when its call site passed `LambdaMetafactory.FLAG_SERIALIZABLE`
(0x1, the 4th static bootstrap argument of `altMetafactory` — emitted for an
`(Iface & Serializable)` intersection cast) **or** its functional interface
already has `java.io.Serializable` as a supertype. `InnerClassLambdaMetafactory`
then spins a `private final Object writeReplace()` only for those, and *adds*
`Serializable` to the spun class's interface list only when the flag (not
inheritance) is what made it serializable
(`isSerializable && !foundSerializableSupertype`).

CratonVM never recorded the flag. `bootstrap_lambda`
(`vm/src/runtime/invokedynamic.rs`) dropped `altMetafactory`'s extra bootstrap
arguments with the comment *"extra args are advisory and not needed for
dispatch"*, so both consumers had nothing to consult and each hard-coded
"yes": `declared_methods_with_synthetic` pushed `writeReplace` for any lambda
proxy with a host class, and the `checkcast` path returned `true` for
`Serializable` outright.

**The two are coupled, which is why they had to be fixed together.** Removing
the bogus `writeReplace` alone would have left plain lambdas passing
`instanceof Serializable` with no substitution hook, so `ObjectOutputStream`
would have tried to serialize the un-loadable `$$Lambda` proxy by name instead
of failing cleanly with `NotSerializableException` the way HotSpot does.

## Fix

- **`classloading/src/resolution.rs`** — `LambdaCallSite` gains
  `serializable_flag`, documented as *only* the explicit flag; the inheritance
  half is evaluated lazily because the functional interface is not necessarily
  loaded at bootstrap time.
- **`vm/src/runtime/invokedynamic.rs`** — `bootstrap_lambda` reads the bitmask
  from `altMetafactory`'s 4th static argument. Plain `metafactory` has no flags
  word and records `false`.
- **`native-builtins/src/lang_invoke.rs`** — the reflective
  `LambdaMetafactory.altMetafactory` native unboxes the same bitmask out of its
  packed `Object[]` and threads it through `build_reflective_lambda_callsite`.
- **`native-builtins/src/serialization.rs`** —
  `reconstruct_serialized_lambda` passes `true`: a proxy being rebuilt *from* a
  `SerializedLambda` is serializable by construction, and without this a
  round-tripped lambda could not be serialized a second time.
- **`vm/src/vm/vm_init.rs`** — `SharedVm::lambda_proxy_serializability` is the
  single owner of the combined rule and returns the JDK's own three-way answer:
  `NotSerializable` / `ByInheritance` (writeReplace, no added marker) /
  `ByFlag` (writeReplace **and** `Serializable` appended to the interface list).
- **`native-api/src/registry.rs`** — the `LambdaSerializability` enum plus
  `NativeContext::lambda_proxy_serializability`, defaulting to
  `NotSerializable`.
- **`native-builtins/src/lang_class.rs`** — `getDeclaredMethods`,
  `getInterfaces` and `getGenericInterfaces` all consult it. The synthetic
  `writeReplace` is now reported `ACC_PRIVATE | ACC_FINAL`, as the real spun
  method is.
- **`vm/src/runtime/interpreter/typecheck.rs`** — the universal `Serializable`
  acceptance is gone; the stale comment that documented it is replaced.

`RegisterLambdaProxy`'s new `serializable` parameter makes every construction
site state its answer explicitly rather than inherit a default.

## Verification

**Differential probe** (`LambdaSerProbe.java`, CratonVM vs
`jdk-25.0.3.9-hotspot`, identical classpath): every lambda-proxy case now
matches HotSpot exactly — plain lambda and plain method reference not
serializable with no `writeReplace` and a clean `NotSerializableException`;
`SerSupplier extends Supplier, Serializable` (the `ByInheritance` arm)
serializable with `private final writeReplace` and a working round-trip;
`(Runnable & Serializable)` (the `ByFlag` arm) additionally reporting
`[java.io.Serializable, java.lang.Runnable]` from `getInterfaces()`. Identical
output from the debug and release builds.

**The predecessor probe closed too**: `LambdaGenProbe2` (from
`springboot/lambda-getgenericinterfaces-fabricates-parameterizedtype-FIXED.md`)
is now **byte-identical** to HotSpot — `writeReplace` was its last remaining
difference.

**Java suite sweeps**, fixed release binary vs the branch's merge-base binary,
same harness, 600 s timeout:

| module | classes | result |
|---|---|---|
| `core/spring-boot` | 351 | 338 PASS / 10 FAIL / 1 HANG / 2 CRASH — **zero per-class differences** |
| `core/spring-boot-test` | 80 | 78 PASS / 2 FAIL — **zero per-class differences** |

**Rust unit tests**: `cargo test --no-fail-fast -p cratonvm-native-builtins
-p cratonvm-native-api -p cratonvm-vm -p cratonvm-classloading` — all suites
green (classloading 719, vm 2337) except the two pre-existing
`cratonvm-native-builtins` failures (`panama::tests::test_85_4_upcall_handle_and_invoke`,
`tls_deny::tests::every_plaintext_base_overload_is_accounted_for`) already
confirmed unrelated on the parent commit.

**Guard test**: `runtime::lambda_proxy::tests::lambda_proxy_serializability_follows_the_recorded_flag`.
It is deliberately in `vm/src/runtime/lambda_proxy.rs` and **not** next to the
sibling lambda-proxy tests in `vm/src/vm.rs` — that whole module sits behind
`#[cfg(all(test, feature = "synthetic-jdk"))]` and never runs by default, so a
guard placed there would have been invisible.

## Notes for future sessions

- **`Comparator.comparing` / `comparingInt` are not lambda proxies on CratonVM
  at all** — they return a `java.util.Comparator$Native`, with
  `getDeclaredMethods() == []` and `instanceof Serializable == false`, where
  HotSpot returns a `FLAG_SERIALIZABLE` lambda. Unchanged by this commit
  (identical before and after) and out of scope here; it is a gap in the native
  `Comparator` implementation, not in lambda-proxy metadata. It also means the
  old `typecheck.rs` comment's stated justification for accepting `Serializable`
  universally ("keeps the checkcast at pc=11 in `Comparator.comparing(Function)`
  from failing") had already been moot for some time — the native path never
  reaches that check.
- The `ByInheritance` arm is the one a bare Rust fixture cannot decide (it needs
  a real `Serializable`-extending interface loaded); the differential probe
  covers it. Keep the probe if this area is touched again.
