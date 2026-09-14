# `ServiceLoader` had no `provider()` factory form, and discarded the `setAccessible` it depended on

> **STATUS 2026-08-12 (second pass), read this first: the last live row is
> ARMED IN SOURCE, unrun.** Both halves of it — the **constructor-form subtype
> check** and the **public no-arg constructor** requirement — now exist on BOTH
> provider paths, with a negative fixture for each. The census the record kept
> waiting for was answered by reading rather than by running; see *The census,
> taken* below. What is left is a build and a run. Everything from the previous
> status block down is kept because its analysis is what the fix was built
> from — but its "STILL OPEN" verdicts on those two rows are superseded by this
> block, not by anything further down.
>
> **STATUS 2026-08-12 (first pass): ONE live row, not two.** The stream-path
> subtype check is **fixed** — W7-85-serviceloader-stream-validation.md owns it
> and owns the population sweep it produced. What was still open here is the
> **constructor-form** subtype check (and, newly, the public-no-arg-constructor
> requirement beside it), both absent on both paths and both waiting on the same
> unmade measurement. The `44/44` numbers below are historical: the vector was
> `104/104` on HotSpot 25.0.3.9 before this pass and grows again with it.

## The census, taken — and it is a reading, not a run

The blocker on both rows was stated as *"a census of `isAssignableFrom` **and**
constructor visibility over the boot modules' `provides` clauses"*, with the
fear that arming either gate *"would newly hard-fail every module-declared
service in the JDK's own boot modules"*. That census does not need a VM,
because **javac already took it**.

JLS §7.7.4 makes both rules compile-time errors on the `provides` directive
itself:

* the implementation must be a subtype of the service, *or* declare a public
  static no-arg `provider()` whose return type is;
* if it is discovered through its constructor, it must have a **public no-arg
  constructor**;
* and the implementation must be **in the same module** as the directive, so
  javac always has the class in hand to check.

Every `provides` clause in the JDK's own boot modules is javac output.
Therefore **no boot-module provider can be in the set either gate refuses** —
the population is zero by construction, exactly as W7-85's blast-radius
condition 3 argued for the factory return type (*"javac refuses to compile that
`provides` clause, so no module built by javac can be in this set"*). The gates
exist for the separately-compiled and hand-assembled module, which is the only
thing `loadProvider` carries them for.

This is a *population* census, and it is worth being precise about what it does
and does not settle. It settles: no legal boot-module provider is in the refused
set. It does not settle: whether **CratonVM's own** `isAssignableFrom` or
`Constructor.getModifiers()` might answer wrongly for one of them. That residual
is bounded three ways, and none of them is a hope:

1. **Both gates are `module_declared`-only**, the same gate the factory block
   already draws. Everything on the classpath — all of Spring, Hibernate,
   Tomcat, Elasticsearch, WildFly boot, hundreds of providers per run — never
   enters the block. The JDK's *own* classpath-path copies of these two rules
   stay unarmed and are a separate row.
2. **Neither gate can fire on an unanswered question.** `constructor_form_is_subtype`
   accepts when the service mirror is unreadable and reuses `service_accepts_type`,
   which answers `true` on an unreadable `isAssignableFrom`;
   `constructor_is_public` answers `true` on an unreadable `getModifiers()`.
   Same discipline W7-85 stated once and this reuses: *a widening into a throw
   must never fire on a question that went unanswered.*
3. **`--jdk-only` is unaffected.** `register_service_loader_natives` states
   `NativeKind::SyntheticStub` for its whole block and `JdkOnly` drops exactly
   that kind at registration, so strict has always run the real
   `java.util.ServiceLoader` bytecode and has always enforced both rules. This
   is a Compatible-mode change, and it qualifies under the contract's
   HotSpot-parity exception for the same reason W7-85 did.

## What landed, 2026-08-12

All of it in `native-builtins/src/service_loader.rs`. No out-of-file patch.

| helper | role |
| --- | --- |
| `provider_class_display` | `String.valueOf(clazz)` — `loadProvider` interpolates the **Class**, not `getName()`; falls back to the FQN |
| `ConstructorForm` | two-state verdict; unanswerable folds into `Accepted` |
| `constructor_form_is_subtype` | `if (!service.isAssignableFrom(clazz)) fail(service, clazz + " not a subtype")` |
| `constructor_is_public` | `getConstructor()` is public-only; this file asks `getDeclaredConstructor()` |
| `no_public_no_arg_ctor_error` | the **three**-argument `fail(service, cn + " Unable to get public no-arg constructor", x)`, cause included |

Both gates are called from `native_sl_iterator` **and** `native_sl_stream`, in
the same change, for the reason W7-85 exists: a validation installed on one of
two siblings is validated by whichever fixture walks the other one.

The `no_public_no_arg_ctor_error` cause is not decoration. `getConstructor()`
throws `NoSuchMethodException("<fqn>.<init>()")` and `fail`'s three-argument
form carries it; a bare message here would be a second, quieter divergence
standing in for the one being closed. It is built explicitly, and the fixture
asserts it. When the `NoSuchMethodException` itself cannot be built the
cause-less form is raised anyway — losing the cause beats losing the refusal.

### Why `getDeclaredConstructor()` was kept

The JDK asks `clazz.getConstructor()`. Switching to it would have made the fix
depend on a second native being registered and correct, on a path where a
regression is silent (an unreachable provider looks like a missing one).
Constructors are not inherited, so `getDeclaredConstructor()` + an `ACC_PUBLIC`
test on the result **is** `getConstructor()` for the no-arg case, and it leaves
the classpath path's existing call untouched.

### The negative fixture — it exists, and it is scheduled

`regression-suite/src/RJdkModule.java::moduleServiceConstructorRejects`, plus
two new services and four new module sources. Both illegal shapes need the
two-source overlay device `WrongFactory` established, and for exactly the reason
the census above turns on — **javac refuses to express either of them in a
`provides` clause**:

* **`Unsub`** ← `internal.NotSubProvider`. `modules/` has it implementing
  `Unsub`; `modules-overlay/` recompiles it implementing **nothing**, with a
  public constructor and no `provider()`, so only the subtype rule can fire.
* **`Ctored`** ← `internal.HiddenCtor`. `modules/` gives it a public no-arg
  constructor; `modules-overlay/` recompiles it with a **private** one, still
  implementing `Ctored`, so only the constructor-visibility rule can fire.

`descriptor()`'s `provides` list is a control on both, not a duplicate: the
descriptor must record all five clauses, because these rules are enforced at
**load**, not at resolution.

**The vector's check count is 155.** It was 44 when this record was written and
104 after W7-85; the 51 added on 2026-08-12 are 38 in
`moduleServiceConstructorRejects`, 10 in `arrayModules` (W4-2's array-module
fix) and 3 in `encapsulation` (W6-8's `Method.invoke` witness). Anything that
reports a smaller number has an unscheduled or half-compiled fixture, not a
passing VM.

What the old behaviour did, and therefore what the checks catch: `Unsub`'s
`iterator()` handed out a `NotSubProvider` and the consumer's own implicit
checkcast threw `ClassCastException` — an exception, so "did it throw?" would
have passed — while `stream().map(Provider::type)` threw nothing at all and
answered `[…NotSubProvider]`. `Ctored` was constructed through its private
constructor by `grant_reflective_override` and handed out on both paths with no
error whatsoever. The checks assert the **exact** `ServiceConfigurationError`
class, the message opening/provider/tail, the cause (null for `Unsub`,
`NoSuchMethodException` for `Ctored`), that `iterator()` and `stream()` emit the
**same** string, and that `Greeter` still loads afterwards.

The message is asserted by opening + provider name + tail rather than verbatim,
deliberately: the JDK's two provider paths render the class differently for this
one rule — `clazz` (so `Class.toString()`, with its `class `/`interface `
prefix) in `loadProvider`, `clazz.getName()` in `LazyClassPathLookupIterator` —
and the shape is what is being pinned.

**Out-of-file edit this fixture needs:** `regression-suite/run.sh`'s
`compile_modules` ground-truths only `WrongFactory`'s overlay with `javap`. The
two new overlays have no such guard, and a silently-unapplied overlay makes the
new checks fail on **both** VMs — the harness-error-as-VM-defect
misclassification this record's own *Verify* section warns about. The exact
addition is in the lane report. `compile_modules` needs no other change: it
globs `modules-overlay/**/*.java` with `find` and compiles the module off the
module source path.

**Unverified against a binary.** This lane could not build or run. The single
falsifying observation is at the end of this record.

**Status (reconciled 2026-08-12 — W7-55-record-reconciliation.md):**

* **Headline: CLOSED, verified, and the wall is gone.** This was the **sixth**
  consecutive wall on `regression-suite/src/RJdkModule.java`; the five before it
  each moved the vector forward (check 1 → 4 → ~14 → ~20 → ~26 → ~30 of 44). The
  verification this record said it could not take was taken 2026-08-12 against
  the dev binary at `ba65f1a19`: `RJdkModule` is **44 of 44** in **both**
  `--jdk-only` and `--real-jdk`. The vector needs `--module-path
  regression-suite/build-modules --add-modules cratonvm.jdkonly.svc` (`run.sh`
  supplies these via `class_args`); without them it fails on a harness error,
  not a VM defect — the misclassification this record's *Verify* section warns
  about.
* **Residual: none unapplied.** The fix is confined to
  `native-builtins/src/service_loader.rs` and needed no out-of-file patch.
* **Two deliberate scope decisions stand, and neither is pending work** — see
  *Deliberately scoped to module-declared providers* and *Deliberately NOT done*
  below. The second one is a real, argued refusal (adding the constructor-form
  subtype check would hard-fail every module-declared service in the JDK's own
  boot modules if our `isAssignableFrom` disagrees on any one of them), not an
  unfinished task.
* **A question this record left open is now answered.** *"Where `--jdk-only`
  stops next on this vector"* — it does not stop. Strict drops this file's
  `SyntheticStub` rows at registration and runs the real
  `java.util.ServiceLoader` bytecode all the way to 44/44.

**AMENDED AGAIN 2026-08-12 (W7-85-serviceloader-stream-validation.md) — the
first of the two rows below is CLOSED. This record now carries exactly ONE live
row, the second one. It is still NOT retired.**

* **Row 1, the stream-path subtype check: FIXED**, on branch
  `fix/serviceloader-stream-accepts-type-20260812`. `service_accepts_type` now
  has one caller, `factory_return_is_subtype`, and *that* has two —
  `native_sl_iterator` and `native_sl_stream`. The `44/44` this record was proud
  of is `104/104` on HotSpot 25.0.3.9: `RJdkModule` gained a `Rejected` service
  whose module-declared `provider()` returns a non-subtype and a `Nulled`
  service whose `provider()` returns null, and asserts
  `ServiceConfigurationError` — exact type, null cause, HotSpot's exact message
  — from BOTH families, plus that the two families emit the **same** message.
  The RED was measured on the dev binary first, and it was worse than "the wrong
  `Provider.type()`": `stream().map(Provider::get)` handed out a `String` for a
  service interface, which nothing downstream can catch because
  `invokeFactoryMethod`'s `(S)` cast is erased. The fix, the pin order, the
  registration argument, the population sweep of this file's other one-path
  validations, and the blast radius are all in
  W7-85-serviceloader-stream-validation.md. **Do not re-derive any of it from the
  text below**, which is kept only because its analysis is correct and is what
  the fix was built from.
* **Row 2, the constructor-form subtype check: SUPERSEDED — see the second-pass
  status block above.** As written when it was found: still open, still
  "deferred for want of a measurement". W7-85's sweep confirmed it independently
  and added a sibling: the JDK's requirement that a constructor-form provider
  have a **public** no-arg constructor is also enforced on neither path (both
  paths use `getDeclaredConstructor` and silently skip on absence). One census
  settles both — `isAssignableFrom` **and** constructor visibility over the boot
  modules' `provides` clauses. **That census has now been taken by reading javac's
  own compile-time rules, and both rows are armed with a negative fixture.**

**AMENDED 2026-08-12 (RETIREMENT-20260812.md §3) — this record was nominated for
retirement and is NOT retired. It carries two live rows.**

* **CLOSED 2026-08-12 by W7-85; see the amendment above. As written when it was
  found: the factory-return-type subtype check is applied on ONE of the two
  provider paths.** `service_accepts_type` (`native-builtins/src/service_loader.rs:1677`)
  has exactly one caller, at `:1897`, inside the **iterator** path
  (`native_sl_iterator`). The **stream** path (`native_sl_stream`, factory block
  at `:2401-2420`) calls `factory_return_type` and uses the result only to build
  the wrapper — it never asks `service_accepts_type` and never raises. So a
  module-declared provider whose `provider()` return type is **not** a subtype
  of the service raises `ServiceConfigurationError` from `iterator()`, as the
  JDK requires, and is **quietly handed out** by `stream()`.

  **Why 44/44 does not see it, and why nothing else would have:** the fixture's
  `FactoryGreeter.provider()` returns `Greeter` — a *correct* subtype. The
  vector exercises only the positive case, so a check that is present on the
  path the test walks and absent on the path it does not cannot go red. Do not
  read the green as covering this.

  Not fixed in the lane that found it: the `:1881-1910` block it has to mirror
  re-reads `sl` and the return-type mirror through `read_native_pin` **after**
  the allocating `factory_return_type` call, in the order the comment at
  `:1886-1889` spells out, and that lane could not compile. It is also a
  `Compatible`-mode change — permitted, because raising
  `ServiceConfigurationError` there is genuine HotSpot parity, but only behind a
  measurement. The negative fixture and the exact command are in
  RETIREMENT-20260812.md §3.

* **STILL OPEN, downgraded from "argued refusal" to "deferred for want of a
  measurement":** the constructor-form subtype check under *Deliberately NOT
  done* below. Confirmed absent — there is no `service_accepts_type` call on
  either constructor path (`:2038`, `:2465` grant the reflective override and go
  straight to `newInstance`). Its own stated reason is *"this lane cannot
  measure that"*, which is a deferral, not a decision. The measurement that
  settles it is a census of `isAssignableFrom` over the boot modules' `provides`
  clauses; nobody has taken it.

## The failure

`RJdkModule.moduleServices()` (`:213`-`:243`). HotSpot 25 prints

```
CK RJdkModule services=[module-factory, module-hello] types=[EnGreeter, Greeter]
```

and the vector passes with 44 checks. CratonVM was predicted to stop at `:223`

```
check(greets.equals(Arrays.asList("module-factory", "module-hello")),
        "module service providers: " + greets);
```

with `module service providers: []`, then at `:234` (`Provider.type()`), then at
`:242` (the layer-scoped overload).

## The fixture, which is the specification

`regression-suite/modules/cratonvm.jdkonly.svc/module-info.java`:

```
provides com.cratonvm.jdkonly.svc.Greeter
        with com.cratonvm.jdkonly.svc.internal.EnGreeter,
             com.cratonvm.jdkonly.svc.internal.FactoryGreeter;
```

`com.cratonvm.jdkonly.svc.internal` is neither `exports`ed nor `opens`ed.

* `EnGreeter` — public class, **public** no-arg constructor, implements
  `Greeter`. Reachable only through `ServiceLoader`.
* `FactoryGreeter` — public class, **private** constructor, **does not
  implement `Greeter` at all**, and declares
  `public static Greeter provider()`. Reachable *only* through that factory.

## Two independent causes, both in `native-builtins/src/service_loader.rs`

### 1. The `provider()` static-factory form was not supported at all

`native_sl_iterator` and `native_sl_stream` only ever did
`Class.getDeclaredConstructor()` + `Constructor.newInstance()`. For
`FactoryGreeter` that is not merely a miss — `getDeclaredConstructor()`
*succeeds* (it returns the **private** constructor), so the code was one
successful `setAccessible` away from constructing an object that is not a
`Greeter` and handing it to the caller. `stream()` was worse: it built
`ServiceLoader$ProviderImpl(service, FactoryGreeter.class, ctor)`, so
`Provider.type()` answered `FactoryGreeter` where the JDK answers `Greeter`.

JDK 25 `java.util.ServiceLoader.loadProvider` (verified by `javap -p -c` against
`C:\Program Files\Microsoft\jdk-25.0.3.9-hotspot`):

* `findStaticProviderMethod(clazz)` = `getDeclaredPublicMethods(clazz,
  "provider")`, keep the unique **static** one, `m.setAccessible(true)`.
* If found, `type = factoryMethod.getReturnType()` and the wrapper is built with
  `ProviderImpl(Class service, Class type, Method factoryMethod)` — a **distinct
  3-arg constructor** from the `(Class, Class, Constructor)` one this file
  already drove. `ProviderImpl.get()` branches on `factoryMethod != null`.
* The factory form is honoured **only** on the module path: the classpath
  iterator (`LazyClassPathLookupIterator.nextService`) never looks for it and
  requires `service.isAssignableFrom(clazz)`.

### 2. `setAccessible(true)`'s result was discarded — and it was being refused

Both loops called

```rust
let _ = ctx.invoke("java/lang/reflect/AccessibleObject", "setAccessible", "(Z)V", ...);
```

and ignored the return. Since wave 4, `lang_class::enforce_set_accessible_gate`
correctly refuses that call: `resolve_caller_class_id` sees the **application**
frame (`RJdkModule`), because `service_loader.rs` is a Rust native and pushes no
`java.util.ServiceLoader` Java frame, and
`com.cratonvm.jdkonly.svc.internal` is neither exported nor opened to the
unnamed module. So `override` stayed 0, and the subsequent
`Constructor.newInstance` was refused in turn by
`native_constructor_new_instance`'s `check_reflection_export_access_with_target_id`
(a **public** ctor of a **public** class still needs `exports` — JEP 261, and
`RJdkModule.java:172` is the check that pins that behaviour and already passes).

HotSpot does not meet this because *its* `ServiceLoader` is java.base and takes
the JDK-internal caller bypass. `getConstructor` there is literally
`if (inExplicitModule(clazz)) ctor.setAccessible(true);`.

**Remedy (as specified by an earlier lane):** write `override = 1` on the
reflective object directly instead of routing through the caller-sensitive
`setAccessible` invoke. `read_constructor_accessible` /
`read_method_accessible` / `accessible_override_is_set` all consult the
JDK-inherited `override` field **first**, so this is the same grant by the same
door, minus the caller identity a Rust native cannot supply.

## What changed

Only `native-builtins/src/service_loader.rs`. No out-of-file patch was needed.

New helpers:

| helper | role |
| --- | --- |
| `service_configuration_error` | `ServiceLoader.fail(service, msg)`; `provider_not_found_error` now delegates to it |
| `sl_service_mirror` | the `service` `Class`, named field then legacy slot 0 |
| `module_declared_providers` | the provider FQNs a JPMS `provides` clause declared for this service |
| `grant_reflective_override` | `override = 1` + the CratonVM extra slot, no `setAccessible` invoke |
| `provider_factory_method` | `findStaticProviderMethod`: declared, public, static, no-arg `provider()` |
| `factory_return_type` | `factoryMethod.getReturnType()` |
| `service_accepts_type` | `service.isAssignableFrom(candidate)` |

`native_sl_iterator`: for a module-declared provider, look for the factory
first. If present, validate the return type against the service (a mismatch is
a `ServiceConfigurationError`, not a skip), grant the override on the `Method`,
`Method.invoke(null, new Object[0])`, and treat a `null` return as a
`ServiceConfigurationError` — `ProviderImpl.invokeFactoryMethod` does exactly
that, and silently dropping the provider would be this campaign's dominant
species (a fabricated success where the spec mandates a failure). Otherwise fall
through to the constructor path, where a module-declared provider now gets
`grant_reflective_override` in place of the doomed `setAccessible` invoke.

`native_sl_stream`: the same detection, then build
`ProviderImpl(service, returnType, Method)` via the `(Class, Class, Method)`
descriptor (tag 3, never cached in the `CTOR_FORM` probe, which stays a
4-arg-vs-3-arg *constructor* question). `getDeclaredConstructor` is skipped
entirely on the factory path, so `FactoryGreeter`'s private constructor is never
touched.

### Deliberately scoped to module-declared providers

Every new behaviour is gated on `module_declared`, i.e. on the provider having
come from `ctx.service_providers_from_modules` rather than a
`../../../apps/META-INF/services` descriptor. That is the JDK's `clazz.getModule().isNamed()`
test asked of the *descriptor*: a provider named in `module-info` is in a named
module by construction. Asking the descriptor rather than the class costs no
Java dispatch on the hot classpath path (Spring/Tomcat/Elasticsearch/WildFly
boot walks hundreds of providers, none module-declared) and keeps classpath
discovery byte-for-byte unchanged.

The registry half already worked and was **not** touched:
`service_loader.rs:1227` -> `ctx.service_providers_from_modules` ->
`ModuleRegistry::service_providers` (`classloading/src/module.rs:969`). The
`CK RJdkModule ... provides=[com.cratonvm.jdkonly.svc.Greeter->2]` line already
matches HotSpot in both CratonVM modes, so both FQNs were reaching
`discover_providers`; they died at instantiation.

### Deliberately NOT done — REVERSED 2026-08-12, see the second-pass status block

As written: *"`ServiceLoader.loadProvider` also fails when a **constructor**-form
provider in a named module is not a subtype of the service. That check is not
added: it would newly hard-fail every module-declared service in the JDK's own
boot modules (`javax.tools.JavaCompiler`, the charset/zipfs/sql providers, ...)
if this VM's `isAssignableFrom` disagrees on any one of them, and this lane
cannot measure that."*

The premise is what fell, not the caution. **javac enforces the same rule at
compile time on the `provides` directive itself** (JLS §7.7.4), so the boot
modules — all javac output — cannot contain a provider the gate refuses. The
named examples were the wrong worry: `javax.tools.JavaCompiler`'s provider and
the charset/zipfs/sql providers are subtypes with public no-arg constructors
because they could not have been compiled otherwise. What survives of the
caution is the *disagreement* risk, and that is bounded by the three conditions
in the status block, not by this refusal.

Still NOT done, and now the only unarmed copies of these two rules: the
**classpath** path's own spellings (`LazyClassPathLookupIterator`:
`fail(service, clazz.getName() + " not a subtype")`, and its constructor
lookup). Those really do sit under Spring/Tomcat/Elasticsearch/WildFly boot,
they have hundreds of users per run, and nothing measures them. Separate row,
separate blast radius.

## Mode scope — read this before believing a `--jdk-only` result

`register_service_loader_natives` sets `NativeKind::SyntheticStub`, and
`--jdk-only` strict drops SyntheticStub rows **at registration**. So in strict
mode none of this file runs: the real `java.util.ServiceLoader` bytecode does,
reaching `ModuleServicesLookupIterator` -> `jdk.internal.module.ServicesCatalog`
-> `BootLoader.getServicesCatalog()`. **This fix moves the `--real-jdk`
(Compatible, default) arm only.** Where `--jdk-only` stops next on this vector
is an open, separate question.

## Verify

The module flags are REQUIRED; omitting them reproduces an earlier
misclassification in which the oracle itself appeared to fail.

```
cd regression-suite
<cratonvm> --java-home "C:\Program Files\Microsoft\jdk-25.0.3.9-hotspot" \
    --module-path build-modules --add-modules cratonvm.jdkonly.svc \
    -cp build RJdkModule
```

`CRATONVM_DIAG_SERVICELOADER=1` (the `diag_serviceloader` flag; `one_true_yes_exact`,
so only exact `1`/`true`/`yes`) now also prints
`[SL-DBG]   built via provider() factory: <fqn>`.

## The falsifying observation for the 2026-08-12 constructor-form gates

If `moduleServiceConstructorRejects` goes red on the `stillGood` check — i.e.
`ServiceLoader.load(Greeter.class)` stops producing
`[module-factory, module-hello]` **after** the two refusals — then a gate is
firing on a legal provider and the fault is in the gate, not in the fixture.
Read which one from the `CK RJdkModule ctorRejects=` line. `EnGreeter` is the
only provider that reaches `constructor_is_public`, so a red there means
`Constructor.getModifiers()` is not answering `ACC_PUBLIC` for a manifestly
public constructor; `module-factory` disappearing instead means
`constructor_form_is_subtype` is being reached on the factory path, which it
must not be (`factory.is_none()` gates it on `stream()`, `built_via_factory` on
`iterator()`).

If instead BOTH new services fail on both VMs, suspect the harness before the
VM: `modules-overlay/` did not land, so `NotSubProvider` still implements
`Unsub` and `HiddenCtor` still has a public constructor. `javap -p -classpath
regression-suite/build-modules/cratonvm.jdkonly.svc
com.cratonvm.jdkonly.svc.internal.HiddenCtor` settles it in one command — a
`public HiddenCtor()` there means the overlay pass did nothing. That guard is
the out-of-file `run.sh` edit this change asks for.

## The single falsifying observation

If `:223` still reports `module service providers: []` **and** the new
`[SL-DBG] built via provider() factory` line never prints while
`[SL-DBG] ServiceLoader service=com.cratonvm.jdkonly.svc.Greeter ... providers=2`
does, then the gate is `module_declared` and the analysis above is wrong about
where the two FQNs come from — i.e. `ModuleRegistry::service_providers` is
answering for `Module.getDescriptor()` but not for
`ctx.service_providers_from_modules`, and the fix belongs one layer down, not
here.

## Expected next wall

`:242`, the layer-scoped `ServiceLoader.load(ModuleLayer, Class)` overload,
which is **not** registered in `service_loader.rs` and therefore runs real JDK
bytecode: `Objects.requireNonNull` x3, then `checkCaller` ->
`jdk.internal.reflect.Reflection.verifyMemberAccess` + `Module.canUse`. The
private constructor itself is trivial (four field writes; `newLookupIterator` is
lazy and our `iterator()` native never calls it), and `discover_providers`
already reads the `layer` field, so if `:242` fails it will be inside
`verifyMemberAccess`/`canUse`, not in the discovery. Registering the overload
natively was considered and rejected: it would have to ignore its `ModuleLayer`
argument, which is a fabricated success for any non-boot layer.
