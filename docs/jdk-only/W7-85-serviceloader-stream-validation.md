# `ServiceLoader` validated the factory provider on `iterator()` and not on `stream()`

**Status: FIXED 2026-08-12** on branch
`fix/serviceloader-stream-accepts-type-20260812`. Compatible (`--real-jdk`)
mode only — the whole of `native-builtins/src/service_loader.rs` registers as
`NativeKind::SyntheticStub` and `--jdk-only` drops it at registration, so strict
already ran real `java.util.ServiceLoader` bytecode and already enforced both
paths.

> **RE-AUDITED 2026-08-12 (A13, VM-internal doors + strict fallbacks). The
> record's central strict-mode claim holds, and the checking is recorded because
> the campaign's other two records in this lane did not survive the same test.**
>
> The claim under test is the status line's: *"`--jdk-only` drops it at
> registration, so strict already ran real `java.util.ServiceLoader` bytecode
> and already enforced both paths."* Three independent confirmations, none of
> them a re-read of this record:
>
> * **The frozen kind map.** `scripts/baselines/jdk-only-kind-map-25-linux.tsv`
>   is a per-registration census whose unit is one `(class, name, descriptor,
>   ordinal)` triple. `java/util/ServiceLoader$Itr`'s registrations read
>   `synthetic-stub`, and `NativeKind::allowed_in` drops exactly that kind under
>   `JdkOnly`. The kind is **stated**, not ambient — §"Registration" below says
>   so and the map agrees — so it cannot move by a `set_category` block boundary
>   drifting.
> * **The independent screen.** The campaign's 33-probe `--jdk-only`
>   reachability screen (2026-08-12, HotSpot 25 as oracle at 33/33) finds
>   `ServiceLoader` **iteration passes** under `--jdk-only`, and neither
>   `ServiceLoader$Itr` nor any `service_loader.rs` receiver is in the
>   five-family blocking set. A record claiming a live strict break in plain
>   iteration would have been contradicted by that screen; this one is not.
> * **The door test, applied.** `java/util/ServiceLoader$Itr` is on
>   `NO_IMAGE_JDK_RECEIVERS`, so gate 1 and gate 2 are both shut for it — but
>   the minting native (`ServiceLoader.iterator`/`stream`) is `synthetic-stub`
>   and is therefore **dropped before the mint**. That is the structurally-dead
>   arm of `W7-17` §5.0's two-term predicate: unreachable by construction, not
>   merely unreached by a corpus. It is the strongest of the three verdicts and
>   the only one that survives somebody re-tagging the receiver.
>
> **Scheduling — the question this record was right to make easy.** Unlike this
> lane's other two records, W7-85's evidence is not a `probes/` run.
> `probes/` is executed by no `SUITE=` value of `regression-suite/run.sh`, so a
> record resting on one cannot be discharged by a suite run however green. This
> record's vector is `regression-suite/src/RJdkModule.java` in
> `JDKONLY_CLASSES`, with its `--module-path` / `--add-modules` supplied by
> `run.sh`'s `class_args`, and its overlay is ground-truthed by
> `compile_modules` with a guard the record confirms it made fail on purpose.
> **That is scheduled evidence.** Nothing here needs re-running by hand.
>
> **Not verified by this lane:** the fix compiles or works. Source read only;
> no build, no run.

This record also carries the **population sweep** the fix was asked for: every
other `ServiceLoader` validation, and which of the two provider paths enforces
it. That table is the deliverable as much as the fix, because the defect was not
a missing feature — it was a guard installed on one of two siblings.

## The defect

`service_accepts_type` — the `service.isAssignableFrom(returnType)` gate that
`W6-2` added for the module-path `provider()` factory form — had **exactly one
caller**, inside `native_sl_iterator`. `native_sl_stream` computed
`factory_return_type` and used the answer only to build the
`ServiceLoader$ProviderImpl` wrapper. It never asked.

Verified before touching anything, and the counts in
`W7-78-inherited-residual-closeout.md` were exact, not rotted:
`service_accepts_type` at `service_loader.rs:1677`, its one caller at `:1897`
inside `native_sl_iterator`, the stream block at `:2401-2420`.

Measured on the dev binary (Compatible mode, `--java-home` a real JDK 25),
against a module that declares one provider whose `provider()` returns `Object`
for a service interface:

```
iterator-loop         -> ServiceConfigurationError   (correct)
findFirst             -> ServiceConfigurationError   (correct)
forEach               -> ServiceConfigurationError   (correct)
stream().map(type)    -> [java.lang.Object]          <-- handed out
stream().map(get)     -> [not-a-Rejected]            <-- a String, for an interface
stream().findFirst()  -> Optional[ProviderImpl@...]  <-- handed out
```

The third line is the one that matters: `stream()` did not merely report the
wrong `Provider.type()`, it handed the caller a live object of the wrong type
for the service. **Nothing downstream can catch that.** `stream()` builds a real
`ProviderImpl`, so `get()` runs the JDK's own `invokeFactoryMethod` — and that
method's `(S) result` cast is *erased*. It checks nothing. `loadProvider`'s
return-type gate is the only gate there is. The wrong-typed object surfaces as a
`ClassCastException` at whatever unrelated site first assigns it.

## Why 44/44 could not see it, and why no audit would have

`RJdkModule` passed 44 of 44 in both modes. Its factory provider,
`FactoryGreeter.provider()`, returns `Greeter` — a **correct** subtype. The
vector walked only the positive case, so a check present on the path it took and
absent on the path it did not read green forever.

This is a distinct entry for this directory's vacuous-green catalogue. It is not
a probe that cannot fail, not a run that never ran, and not a blind instrument.
It is **a guard installed on one of two siblings, validated by a fixture that
only ever satisfies it.** The positive case exercises the code; it cannot
exercise the refusal.

The technique that found it was not reading the record. It was asking `grep -n
"service_accepts_type"` *who calls this*, and comparing the number of answers to
the number of paths the feature has. One caller, two paths.

## What the JDK actually does, measured

`java.util.ServiceLoader.loadProvider` (JDK 25 `lib/src.zip`):

```java
if (inExplicitModule(clazz)) {
    Method factoryMethod = findStaticProviderMethod(clazz);
    if (factoryMethod != null) {
        Class<?> returnType = factoryMethod.getReturnType();
        if (!service.isAssignableFrom(returnType))
            fail(service, factoryMethod + " return type not a subtype");
        return new ProviderImpl<S>(service, (Class<? extends S>) returnType, factoryMethod);
    }
}
```

`fail(service, msg)` is `throw new ServiceConfigurationError(service.getName() +
": " + msg)` — the **one-argument** constructor, so `getCause()` is `null`.
Measured on HotSpot 25.0.3.9:

```
java.util.ServiceConfigurationError: com.cratonvm.jdkonly.svc.Rejected:
  public static java.lang.Object
  com.cratonvm.jdkonly.svc.internal.WrongFactory.provider()
  return type not a subtype                        getCause() == null
```

CratonVM's message was prose of its own (`provider() of <fqn> returns a type
that is not a subtype`) on the one path that raised at all. It now renders the
JDK's text verbatim on both, via `factory_method_display`
(`Method.toString()`, falling back to `<fqn>.provider()`).

### Timing: HotSpot is lazy, we are eager, and the fixture is written for both

On HotSpot the error surfaces during **traversal**, never at the call that hands
back the iterator or the stream. Measured, every line of it:

| call | HotSpot |
| --- | --- |
| `ServiceLoader.load(Rejected.class)` | no throw |
| `.iterator()` | no throw |
| `.iterator().hasNext()` | **no throw** — returns `true` |
| the for-each loop (`hasNext` + `next`) | `ServiceConfigurationError` |
| `.stream()` | no throw |
| any terminal on that stream | `ServiceConfigurationError` |

`hasNext()` not throwing is not an accident of laziness:
`ModuleServicesLookupIterator.hasNext` **catches** the error into `nextError` and
`next()` rethrows it. `stream()` pulls through `ProviderSpliterator.tryAdvance`,
which drives that same iterator.

CratonVM's `native_sl_iterator` and `native_sl_stream` both **materialise
eagerly**, so both raise from the call that returns the iterator / the stream.
That is a real residual and it is **not fixed here** — the synthetic stream
backing is an `Object[]` in field 0 by construction, and making either path lazy
is a different change of a different size. It is observable exactly once: a
caller that asks for `stream()` and never consumes it sees a throw on CratonVM
and nothing on HotSpot. It was already true of `iterator()` before this record
and the suite has always been green over it.

`RJdkModule`'s new checks therefore wrap the **whole traversal**, never the
individual call, so an eager VM and a lazy VM both satisfy them. Asserting
HotSpot's exact throw *point* would have made a correct VM red for a reason
unrelated to the check.

## The fix

One function, `factory_return_is_subtype` (`service_loader.rs:1787`), called by
`native_sl_iterator` and `native_sl_stream`. It answers a three-state
`FactoryReturn`:

| variant | meaning |
| --- | --- |
| `Accepted(type)` | legal; carries the mirror `ProviderImpl` records as `type` |
| `Unreadable` | `getReturnType()` could not be driven — keep the historic accept |
| `Rejected(Option<MethodCallFailed>)` | not a subtype; `None` when the error object itself could not be built |

The two non-rejecting states are deliberate and they are the same refusal twice:
**an interrogation this VM cannot drive must never MANUFACTURE a refusal.** An
unreadable `getReturnType()` and an unreadable service mirror both keep the
provider, exactly as `service_accepts_type` has always answered `true` on an
unreadable `isAssignableFrom`. A widening into a throw must not be able to fire
on a question that went unanswered.

### Pin order, which is the part that bites

`W7-78` flagged this block as pin-order-sensitive and it is. Every step
allocates and re-enters Java, so no `ObjectRef` may be held raw across a call.
Stated once, in the helper:

* `factory` is re-read through the caller's `factory_pin` before **each** use;
* the service mirror is fetched with the non-allocating `sl_service_mirror`
  **after** `factory_return_type` has returned, never held across it;
* the return mirror takes its own pin for the duration of `isAssignableFrom`,
  and is read back through that pin **before** the pin drops — so `Accepted`
  names the forwarded address, not the pre-GC one;
* `Method.toString()` and `sl_service_name` are driven while the caller's pins
  are still standing, i.e. the message is built *before* anything is released;
* the caller's `sl_pin` and `factory_pin` are left exactly as found.

`unpin_native_roots` is **truncate-to-base**, not release-one
(`vm/src/vm/vm_exec.rs`, `self.thread.native_pin_roots.truncate(base)`). That is
why `native_sl_stream`'s reject arm truncates to `type_pin` — this iteration's
first pin — and takes `method_pin` and everything after it down with it, and why
the null-return arm of `native_sl_iterator` had to move its
`unpin_native_roots(class_pin)` *inside* the match: `class_pin` was taken before
`factory_pin`, and rendering `Method.toString()` needs the Method.

`grant_reflective_override` on the stream path moved to **after** the gate.
There is no reason to open a factory the loader is about to refuse.

## Registration — read, not brace-scanned

Three registrars write `java/util/ServiceLoader.stream()Ljava/util/stream/Stream;`:

| registrar | reachable from |
| --- | --- |
| `phases_late::register_p63_service_loader` | `register_phase63_natives` <- `register_synthetic_overrides` |
| `servlet::register_s1_classloading` | `register_synthetic_overrides` |
| `service_loader::register_service_loader_natives` | `jdbc::register_jdbc_driver_natives`, and `vm_init::init_service_loader_bootstrap` |

`register_synthetic_overrides` is `#[cfg(feature = "synthetic-jdk")]`, and that
feature is **not** in the default set (`vm/Cargo.toml`, `native-builtins/Cargo.toml`
`default = []`); in the shipping build it is a no-op shim at
`vm/src/native/builtins.rs`. So the first two are not merely out-voted, they are
not called. Even under the feature they run before that same function's **last**
ServiceLoader statement, `jdbc::register_jdbc_driver_natives` at
`native-builtins/src/lib.rs:24188`, which re-registers this file's rows.

The decisive site is later than any of them: `vm_init.rs` calls
`init_service_loader_bootstrap` at `:2356` (synthetic branch) and `:3052`
(default branch), both **after** `register_builtins` (`:1839`) and
`register_essential_natives_with_shims` (`:1960` / `:2498`). Registration is
last-write-wins, so `native_sl_stream` and `native_sl_iterator` are the winners
on every boot path.

`register_service_loader_natives` sets `NativeKind::SyntheticStub` explicitly for
its whole block, and `NativeKind::allowed_in` drops exactly that kind under
`JdkOnly` (`native-api/src/registry.rs`). The kind is **stated**, not ambient, so
this change cannot leak into strict mode by a block boundary moving.

## The population: every `ServiceLoader` validation, and where it is enforced

"Iterator family" means `iterator()`, and with it `forEach`, `findFirst()` and
`spliterator()` — all three delegate to `native_sl_iterator`. `stream()` is the
only second path. (HotSpot's `findFirst()` is `stream().findFirst()`, so the two
VMs reach it through different families; both refuse, which is what the vector
checks.)

| JDK rule (`loadProvider` / `ProviderImpl` / `LazyClassPathLookupIterator`) | iterator family | `stream()` |
| --- | --- | --- |
| factory `provider()` return type is a subtype | **yes** | **yes, as of this fix** (was: none) |
| factory `provider()` returned `null` | yes, natively | yes, via the real `ProviderImpl.get()` bytecode |
| — and the two now emit the **same** message | fixed here | already the JDK's |
| constructor-form provider is a subtype of the service | **none** | **none** |
| provider class must be `public` | **none** | **none** |
| provider must have a **public** no-arg constructor | **none** — silently skipped, and a *non-public* declared ctor is accepted | **none** — same |
| provider class cannot be resolved | partial: raises only when **no** provider resolved (deliberately narrowed) | **none** — silently skipped |
| provider constructor threw | yes (`provider_construction_error`) | yes, via the real `ProviderImpl.get()` |
| — message shape | diverges: no `<service>: ` prefix | already the JDK's |
| `provider()` declared more than once | none — unreachable from javac output | none |
| caller module declares `uses` (`checkCaller` / `Module.canUse`) | **none** | **none** |
| provider module reads the service module (`canRead`) | **none** | **none** |
| classpath descriptor syntax / illegal provider-class name | **none** | **none** |

Two things this table is careful about:

* **A shape I predicted and then measured away.** `provider()` **arity** looked
  like a divergence: HotSpot's `findStaticProviderMethod` calls
  `LANG_ACCESS.getDeclaredPublicMethods(clazz, "provider")` and *appears* to
  filter only on name/public/static, while `provider_factory_method` asks
  `getDeclaredMethod("provider", new Class[0])`. It is not a divergence — that
  `getDeclaredPublicMethods` overload is varargs on `parameterTypes`, so calling
  it with none means *no-arg*. Measured directly: a provider declaring
  `public static Object provider(int)` alongside a legal constructor is taken
  through the **constructor** on HotSpot, which is what CratonVM does. The
  "declares more than one" failure is defensive against hand-assembled class
  files, not reachable from javac.
* **`stream()` is not uniformly weaker.** Where a rule lives in
  `ProviderImpl.get()` rather than `loadProvider`, `stream()` is *stronger* —
  it builds a real `ProviderImpl`, so the JDK's own bytecode enforces it with
  the JDK's own message, while the iterator reimplements it. That is why the
  null-return and constructor-threw rows read the way they do. A sweep that
  assumed "native path = weaker" would have got two rows backwards.

The two `none / none` rows worth naming as future work are the **constructor-form
subtype check** (`W6-2`'s deferred row — its own stated reason is "this lane
cannot measure that", which is a deferral, not a decision) and the **public
no-arg constructor** requirement. Both would newly hard-fail providers that work
today, both are on the classpath path that Spring/Tomcat/Elasticsearch/WildFly
walk hundreds of times per boot, and neither has been measured. Do not arm either
without a census of the boot modules' `provides` clauses first.

## Blast radius

This is a Compatible-mode change and Compatible is contractually frozen except
for genuine HotSpot parity. It qualifies, and it is a **widening into a throw**:
something that used to be handed a wrong-typed provider now gets an error. The
radius is small and it is bounded by three independent conditions that must all
hold:

1. **The provider is module-declared.** The whole factory block is gated on
   `module_declared`, i.e. on the FQN having come from
   `ctx.service_providers_from_modules` rather than a `../../../apps/META-INF/services`
   descriptor. Everything on the classpath — which is all of Spring, Hibernate,
   Tomcat, Elasticsearch, WildFly boot — never enters the block at all. Their
   behaviour is byte-for-byte unchanged.
2. **The provider declares a public static no-arg `provider()`.** A
   module-declared provider with an ordinary constructor still takes the
   constructor path, untouched.
3. **Its return type is not assignable to the service.** javac **refuses** to
   compile that `provides` clause, so no module built by javac can be in this
   set. Only a separately-compiled or hand-assembled module can — which is
   exactly the case `loadProvider` carries the runtime check for.

And a fourth, in the other direction: the gate cannot fire on an unanswered
question. An unreadable `getReturnType()` or an unreadable service mirror both
keep the provider.

The strongest statement available is this: any provider that this change would
newly refuse from `stream()` is **already** being refused from `iterator()`, and
has been since `W6-2` landed. A workload that survives `iterator()` today is
untouched. A workload that would newly break is one that calls `stream()` on a
service whose provider `iterator()` already rejects — i.e. one that is already
half-broken and getting a wrong-typed object out of the working half.

`--jdk-only` is unaffected: the rows are dropped at registration and the real
`java.util.ServiceLoader` bytecode already enforced both paths.

## The vector

`regression-suite/src/RJdkModule.java` (`JDKONLY_CLASSES`; needs `--module-path
regression-suite/build-modules --add-modules cratonvm.jdkonly.svc`, which
`run.sh`'s `class_args` supplies — **without them it fails on a harness error,
not a VM defect**). The existing 44 checks are unchanged. HotSpot 25.0.3.9 runs
104 of 104.

Two new services, because the two illegal factory shapes have to be built two
different ways:

* **`Rejected`**, provided by `internal.WrongFactory`, whose `provider()`
  returns `Object`. javac refuses this inside a `provides` clause — *"the
  provider method return type must be a subtype of the service interface
  type"* — and that refusal is precisely why `loadProvider` carries the rule at
  runtime. The suite reproduces the separately-compiled module honestly:
  `regression-suite/modules/` holds a javac-satisfying `WrongFactory`, and
  `regression-suite/modules-overlay/` is recompiled **over** the module output
  on a plain classpath in a second `javac` pass, where no `provides` clause is
  in scope. `run.sh`'s `compile_modules` then ground-truths the result with
  `javap` and fails the build if the overlay did not land — checked by moving
  `modules-overlay/` aside and confirming the guard fires, because a guard that
  cannot fail is the species this suite exists to catch. `harness-selfcheck.sh`
  carries the same second pass.
* **`Nulled`**, provided by `internal.NullProvider`, whose `provider()` returns
  `null`. javac accepts this, so it needs no overlay. It fails at a *different
  moment* — `ProviderImpl.invokeFactoryMethod` only runs once the factory has
  been called — and the vector asserts that: `stream().map(Provider::type)` must
  answer `[...Nulled]` **without throwing**, while `map(Provider::get)` throws. A
  VM that refused a null-returning factory while building the wrapper would
  satisfy "get() throws" and still be wrong; that is the check that makes this
  half non-vacuous.

What is asserted, and what was deliberately *not*:

* the exact type `ServiceConfigurationError` (not `instanceof`, and not "a
  subclass"), a null cause, and the message's opening, the provider name in it,
  and its tail. Asserting only that `stream()` "threw something" would pass
  against a `ClassCastException` raised somewhere else entirely, which is the
  outcome this whole record is about;
* that `iterator()` and `stream()` produce **the same message string**, for both
  illegal services. That comparison needs no hardcoded oracle and catches
  exactly the failure mode here: two paths that disagree;
* that the legal service still loads and still streams **after** the refusals —
  otherwise a "fix" that refuses everything would pass every check above;
* **not** the throw point, for the laziness reason above.

## Verify

```
cd regression-suite
ONLY=RJdkModule CV=<cratonvm.exe> JDK="C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot" \
    bash run.sh
```

`CRATONVM_DIAG_SERVICELOADER=1` prints the `[SL-DBG]` trace, including
`built via provider() factory: <fqn>` for the legal one.

## The single falsifying observation

If `stream()` on the `Rejected` service now raises `ServiceConfigurationError`
but `stream()` on `Greeter` **also** raises, the gate is firing on a legal
provider and the fault is in `factory_return_is_subtype`'s pin order, not its
logic: the most likely shape is `Accepted` naming a pre-GC address, or the
service mirror being read across `factory_return_type`'s allocation. The
`stillGood` checks at the end of `moduleServiceRejects` are there to catch
exactly that, and `CK RJdkModule rejected=... stillGood=[module-factory,
module-hello]` is the line to read.
