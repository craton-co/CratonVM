# Three strict failures, one refusal: `java/lang/annotation/AnnotationProxy`

**Status: DIAGNOSED and MEASURED 2026-08-11. The fix is specified below but
NOT APPLIED** — the one line that has to change lives in a file this lane does
not own, and nothing here was rebuilt. Every number below came out of the
already-built `dev` binary at `C:/craton/CratonVM`; no claim is made about code
that has not been compiled.

`W7-11-strict-baseline-remeasured.md` names six pre-existing `--jdk-only`
failures and groups three of them as "annotations — one cause, three vectors,
most likely". This record turns that "most likely" into a measurement, and
picks between the two resolutions.

## The three vectors, and what each one actually says

```sh
./target/release/cratonvm.exe --jdk-only \
  --java-home "<jdk-25-home>" -cp regression-suite/build <VECTOR>
```

| vector | strict failure | `--real-jdk` |
|---|---|---|
| `RJdkJmx` | `NoClassDefFoundError: java/lang/annotation/AnnotationProxy`, thrown out of `Introspector.descriptorForElement` → `ConvertingMethod.getDescriptor` | PASS |
| `RReflect` | `AssertionError: getAnnotation present` (`RReflect.java:35`) | PASS |
| `RJdkReflect` | `AssertionError: runtime annotation on the type` (`RJdkReflect.java:267`) | PASS |

Both assertions are the same shape: `check(cls.getAnnotation(Tag.class) != null)`.
So two vectors see a **null annotation** and one sees a **thrown
`NoClassDefFoundError`**, which is why the baseline record could only guess they
were related.

## They are one cause. The census says so, not the names.

A slash-form `NoClassDefFoundError` is not a `<clinit>` verdict
(`L16-classnotfound-vs-noclassdeffound-shapes.md`), and the refusal banner's
"the natives bound to it are unreachable" is a claim about registration, not
about need — so the name was not taken as evidence. `--jdk-only-report` was.
All three runs record **exactly one** row for this class, byte-identical,
from the same Rust call site:

```json
{"kind":"compatibility-class-requested",
 "class":"java/lang/annotation/AnnotationProxy",
 "initiating_loader":"bootstrap",
 "requester":"native-builtins\\src\\lang_class.rs:13667",
 "reason":"VM-requested stand-in: ensure_synthetic_class called with no class file on any classpath entry"}
```

(The line number is the built binary's; in this worktree the site is
`create_annotation_proxy_with_type`, `native-builtins/src/lang_class.rs:13685`.
Anchor on the function name.)

`admit_compatibility_class` dedupes by class name, and `fabricate_class`
returns before it for a name that is already loaded or whose real bytes are on
the classpath — so one row means the request was reached and refused, and it
means the class was never minted at all. The annotation *type* resolved fine:
`resolve_annotation_type_class_id` runs first and returns `Ok(None)` with no
violation when it fails, so a row here proves the admission filter passed and
the **builder** was the thing that failed.

## Why one cause wears two faces

`create_annotation_proxy_with_type` opens by allocating the carrier:

```rust
    let mut proxy = try_alloc_concurrent_synthetic(
        ctx,
        "java/lang/annotation/AnnotationProxy",
        ANN_PROXY_FIELDS,
    )?;
```

Under `--jdk-only` that `?` is a `NoClassDefFoundError`. Its callers split into
two populations, and the split is the whole explanation:

* **Propagating** — the array-valued entry points
  (`build_annotation_array_for`, `build_class_annotation_array`, the
  `getAnnotationsByType` builders, the nested-member builder) all use `?`. JMX's
  `Introspector.descriptorForElement` calls `getAnnotations()`, so `RJdkJmx`
  gets the error verbatim, naming the class.
* **Swallowing** — the single-annotation entry points do not. Four sites in
  `native-builtins/src/lang_class.rs` are spelled
  `if let Ok(Some(proxy)) = …` — the three in `native_class_get_annotation`
  (own class, plus the two arms of the `@Inherited` superclass walk) and the one
  in `native_field_get_annotation` — and each falls through to
  `Ok(Some(Value::Object(None)))`. `getAnnotation` therefore answers **null**,
  and the vector reports an assertion three frames from the cause.

That is a defect in its own right and is filed as residual R1 below: the same
`if let Ok(..)` also swallows `MethodCallFailed::ExceptionThrown`, so a genuine
pending exception raised while building a proxy is converted into "this
annotation is absent".

## `java/lang/annotation/AnnotationProxy` is not a JDK class

Against the JDK 25 image on this host:

```
$ javap java.lang.annotation.AnnotationProxy
Error: class not found: java.lang.annotation.AnnotationProxy
```

No JDK declares it; no class file can ever back it. It is CratonVM's own
4-field carrier (`ANN_PROXY_FIELDS`: type descriptor, type mirror, element
names, element values) and it plays the role HotSpot gives to
`sun.reflect.annotation.AnnotationInvocationHandler` — which *does* exist on
this image, as does the public `sun.reflect.annotation.AnnotationParser
.annotationForMap(Class, Map)` that builds a real annotation from one.

So this is not a missing class. It is strict mode declining to fabricate a
compatibility stand-in for a name that is standing in for nobody.

## The two resolutions, and why one is wrong *here*

### Resolution 1 — "use the real mechanism" is already half true, and the other half is a redesign

CratonVM's annotations are **already real `Proxy` instances** by default:
`real_annotations_enabled()` is on, and `wrap_annotation_in_real_proxy`
(`lang_class.rs`) hands the carrier to `define_or_get_proxy_class`
(`native-builtins/src/reflect_annotations.rs`) and returns a generated
`$ProxyN` whose `getClass()` is the proxy class, matching HotSpot. That path
demonstrably works in strict mode — measured, not assumed:

```
$ cratonvm.exe --jdk-only … RJdkProxy
PASS RJdkProxy (36 checks)
```

What is *not* real is the **invocation handler**. Making that real means
constructing an `AnnotationInvocationHandler` (or calling `annotationForMap`)
and letting real JDK bytecode serve every member access. Three reasons that is
the wrong move for this defect:

1. **It does not remove the split; it moves it.** The handler still has to be
   built by a native out of parsed class-file annotation bytes. The strict/
   compatible asymmetry is at the mint, and the mint stays.
2. **It rewrites `Compatible` mode wholesale**, which contract §5/§10 and this
   wave's constraint forbid. Every consumer keys on the carrier class:
   `annotation_proxy_dispatch_impl` and its `equals`/`hashCode`/`toString`/
   `getClass` arms, `invoke_or_native`'s `effective_class` arm,
   `execute_invoke_kind`'s S111r18 array-component guard, three
   `dispatch_virtual.rs` arms, the JIT retarget in `vm/src/jit/helpers.rs`,
   the `jdk_interfaces` edge that makes `AnnotationProxy[]` an
   `Annotation[]`, and this lane's own second call site
   (`native_proxy_dispatch_invoke`). All three vectors pass under `--real-jdk`
   today; a handler swap puts every one of those at risk to fix a strict-mode
   refusal.
3. **It is a design, not a bug fix.** It belongs in `docs/feature-designs/`
   with its own soak, alongside the existing `CRATONVM_REAL_PROXY*` gates —
   which is exactly how the proxy half of this was landed.

### Resolution 2 — mint it through the door it belongs to (CHOSEN)

`docs/architecture/natives-over-real-jdk-classes.md` §1–§2 and
`ClassManager::try_ensure_synthetic_class`'s own doc give the taxonomy. Case 1
is "a legitimately-generated VM class (a lambda, **a proxy or its
`Proxy$Instance` superclass**, a reflection accessor, an array shape) →
`ensure_generated_class`, never refused in either mode, per contract §1 item 6;
natives reach it as `NativeContext::ensure_vm_internal_class`". Case 2 is "a
stand-in for a class whose real bytes should have been found".

`AnnotationProxy` is case 1 filed under case 2. Its sibling is already
corrected in two places, and this record is the third instance of one species:

* `is_vm_proxy_supertype_name` / `fabricated_origin_for_name`
  (`classloading/src/class_manager.rs`) route
  `java/lang/reflect/Proxy$Instance` to `ClassOrigin::VmInternal` by name;
* `define_or_get_proxy_class` (`native-builtins/src/reflect_annotations.rs`)
  calls `ctx.ensure_vm_internal_class("java/lang/reflect/Proxy$Instance", 3)`
  at the mint point, with a comment that says routing it through the
  compatibility door "was the mislabel".

`ensure_vm_internal_class`'s doc carries the guardrail — *"Do not reach for it
to silence a refusal… The decision is whether the JVM specification says a
class file must exist for this name."* Answered on the record: no JVM spec, no
JDK, and no vendor declares this name (`javap`, above), it lives in no
classpath entry, and the §11 zero-stub census stays falsifiable because
nothing is being substituted **for** it — the class whose implementation
CratonVM replaces here is `AnnotationInvocationHandler`, whose real bytes are
on the boot classpath, present, and simply not used. That is a substitution of
*implementation*, which the VM makes for proxies, lambdas and iterators too,
not a substitution of *class*, which is what §5 counts.

## Out-of-file patch (not applied)

Not built, not run. Two hunks; the first is the fix, the second is the
belt-and-braces the `Proxy$Instance` precedent also carries.

### Hunk 1 — `native-builtins/src/lang_class.rs`, `create_annotation_proxy_with_type`

Anchor on the function name; the line number rots.

```rust
    // Allocating the proxy itself can relocate a user-defined declaring
    // loader before the first loader-aware annotation-type lookup.
    let container_loader_pin =
        container_loader.map(|loader| ctx.pin_native_root(loader));
    // `ensure_vm_internal_class`, not the compatibility door (W7-12).
    // `java/lang/annotation/AnnotationProxy` is a name NO JDK declares
    // (`javap java.lang.annotation.AnnotationProxy` → class not found), so no
    // class file can ever back it: it is the invocation-handler carrier the VM
    // mints for every generated annotation proxy, the exact sibling of
    // `java/lang/reflect/Proxy$Instance` a few files over, and contract §1
    // item 6 lists both among the shapes the VM legitimately generates. Minted
    // through `try_alloc_concurrent_synthetic` alone it looked like a §5
    // stand-in, and `--jdk-only` refused it — taking `getAnnotation` to null on
    // RReflect/RJdkReflect and to `NoClassDefFoundError` on RJdkJmx. Pre-mint
    // it here: `fabricate_class` returns early for an already-loaded name, so
    // the allocation below is byte-for-byte unchanged in either mode and only
    // the class's recorded ORIGIN moves.
    ctx.ensure_vm_internal_class("java/lang/annotation/AnnotationProxy", ANN_PROXY_FIELDS);
    let mut proxy = try_alloc_concurrent_synthetic(
        ctx,
        "java/lang/annotation/AnnotationProxy",
        ANN_PROXY_FIELDS,
    )?;
```

Why a pre-mint rather than replacing the allocation outright:
`try_alloc_concurrent_synthetic` also does the real-vs-requested field-count
widening, the W4-4 layout-alias report and the GC-safe allocation retry. Those
must not be re-implemented at one call site. `ClassManager::fabricate_class`
returns the existing `ClassId` before it reaches `admit_compatibility_class`,
so once the class is minted through the VM-internal door the funnel below finds
it loaded and behaves exactly as it does today.

### Hunk 2 (recommended) — `classloading/src/class_manager.rs`, `fabricated_origin_for_name`

```rust
fn fabricated_origin_for_name(name: &str) -> ClassOrigin {
    if is_vm_proxy_supertype_name(name) {
        return ClassOrigin::VmInternal;
    }
    // W7-12 — the annotation carrier is the same species as the proxy
    // supertype above: a name no JDK declares, minted by
    // `create_annotation_proxy_with_type` as the InvocationHandler of every
    // generated annotation `$ProxyN`. `GeneratedProxy` is wrong for the same
    // reason it is wrong for `Proxy$Instance` — it carries an `interfaces`
    // list this carrier has no value for.
    if name == "java/lang/annotation/AnnotationProxy" {
        return ClassOrigin::VmInternal;
    }
```

**On "never bind by name."** This lane's rule was paid for five times, and
every instance was a *dispatch* or *identity* decision — an `invokestatic`
owner, a `$ProxyN`'s interface, a reflect stub's rendered name, a MIC's owner,
a shape test on a class name — where a same-named copy from another loader was
the correct answer and the string was not. This is neither. `fabricated_origin_for_name`
is by construction a classifier of *names the VM itself invents*, documented as
"the origin a fabricated class deserves on the strength of its name alone"; the
class has no bytes, no loader ambiguity and no other identity, and it is minted
under `ClassLoaderId::Bootstrap` in a namespace no class file occupies. Hunk 1
is the authoritative one precisely because it names the shape at the point of
minting rather than inferring it; hunk 2 exists so a second minting route
cannot re-acquire the wrong label silently, which is exactly the pairing
`Proxy$Instance` already has.

## Which mode each change affects

`--jdk-only` (`CompatibilityMode::JdkOnly`) is the only mode whose **behaviour**
changes: the refusal stops, the class is minted, and the annotation path runs
what `Compatible` already runs.

`Compatible` keeps minting the class as it always has — the same `ClassId`, the
same four slots, the same superclass and the same `java/lang/annotation/Annotation`
superinterface edge, because `jdk_interfaces(name)` and
`synthetic_stub_access_flags(name)` are applied in `fabricate_class` for every
origin. Two **observable** things move in Compatible, and neither is a vector-
level behaviour:

1. **The census label.** `--dump-class-origins` / `--jdk-only-report` report
   this class as `vm-internal` instead of `compatibility-stub`, and the
   `NoSuchMethodError` diagnostic hint "[class not found on any classpath
   entry — synthetic stub, add the missing jar]" stops being appended for it.
   Both are corrections: it is not a classpath gap.
2. **`Class::dispatch_lacks_class_file()` flips `true` → `false`** for this
   class, because the `VmInternal` arm is `methods.iter().any(|m| m.is_native())`
   and this class declares no methods (there is no
   `synthetic_stub_ctor_methods` arm for it).

Point 2 is the one that needed evidence rather than reasoning, and the
predicate's own doc supplies the test: *"only the ones that carry native-backed
methods under their own name need the exact-name lookup… An `AnonymousObject$N`
has an empty method table and no registration — nothing to find under its own
name, so it belongs on the non-stub arm."* Measured on a real-JDK boot:

```
$ cratonvm.exe --dump-native-registry <FILE> --java-home <jdk-25> -cp … RReflect
$ grep -i annotationproxy <FILE>        # → no rows
$ grep 'Proxy\$Instance' <FILE>         # → 2 rows
```

**Zero** natives are registered under `java/lang/annotation/AnnotationProxy`.
The predicate has three read sites, and the bit is inert at all three for this
class:

* `invoke_or_native`'s exact-name native preference and
  `invoke_on_class_shared_inner`'s `prefer_exact_class_native` — both do a
  `native_methods.find(<this class name>, …)`, which has nothing to return;
* `validate_native_coverage` (`vm/src/vm/vm_object.rs`) skips
  dispatch-lacking-a-class-file classes, so after the flip it would scan this
  one — over a method table with **zero** entries. Zero iterations either way.

Every real door into the class is keyed on the **name**, not the origin:
`invoke_or_native`'s `effective_class` arm,
`invoke_on_class_shared_inner`'s terminal-miss rescue (which is what serves
`reflect_annotations.rs`'s `ctx.invoke("java/lang/annotation/AnnotationProxy",
…)` second call site), `execute_invoke_kind`'s S111r18 arm, the three
`dispatch_virtual.rs` arms and the JIT retarget.

The invariant `classloading/tests/jdk_only_class_origin.rs::dispatch_predicate_matches_the_stub_bit`
pins — the predicate equals the stub bit for every class except
`Proxy$Instance` — survives, because for this class both sides flip together
(no compatibility stub, and no native-flagged method to keep it on the stub
arm). Its sibling test in the same file asserts the §11 acceptance criterion:
**zero `CompatibilityStub` classes for JDK/application/dependency classes**,
which this patch moves one class closer to rather than away from. (That file
is under `classloading/tests/`, not `vm/tests/` — `classloading/src/class.rs`'s
own doc comment names it without a path, and the first place this record looked
was wrong.)

## Before landing: the question-1 sites, unverified

`origin.is_compatibility_stub()` answers a different question (§ the predicate
doc: "is this a compatibility substitution?"), and it also flips. This lane
could not build, so the following is a **reading**, not a result. Each site was
read; none is believed reachable with `AnnotationProxy` as its class, and the
reason is the same in every case — the name never appears in any constant pool,
because no class file mentions it, so a *declared* class name can never be it:

| site | what it gates | why it should not move |
|---|---|---|
| `vm/src/jit/helpers.rs` (JIT direct-bind), `interpreter/native_override.rs` (`check_override`) | refuse a direct bind on a stub | take a **declared** class name |
| `interpreter/invoke.rs` `try_stackless_invoke` (inline cache) | stubs take the recursive path | receiver-derived; the AnnotationProxy arm in `execute_invoke_kind` is upstream, and an empty method table still misses for members |
| `interpreter.rs`, `interpreter/jit_bridge.rs` | scan a method's `new`/`anewarray` ops before compiling | the class has no methods to compile |
| `vm_exec.rs` `method_exists`, `resolve_field_descriptor_byte_cached`, the `Thread` layout probes | "assume natives exist" / field-descriptor caching / thread layout | take a declared name, or a `Thread` receiver |
| `vm_exec.rs` `stub_hint` | diagnostic text | intended |

**The measurement that settles it is a `--real-jdk` run, not a `--jdk-only`
one** — `Compatible` is the mode at risk from this patch, and it is the mode
that is green today. Land it against `RReflect`, `RJdkReflect`, `RJdkJmx`,
`RJdkProxy` and an annotation-heavy Spring workload in **both** modes before
believing either half.

## Residuals this record does not fix

* **R1 — `getAnnotation` swallows the failure.** The four
  `if let Ok(Some(proxy)) = …` sites in `native-builtins/src/lang_class.rs`
  turn any `Err` from the builder into "annotation absent". That is how a
  refusal naming a class arrived at `RReflect.java:35` as a bare
  `AssertionError`, and it also discards a real `MethodCallFailed::ExceptionThrown`.
  The array-valued siblings a few hundred lines up already use `?`; these four
  should too, or should at minimum re-raise a thrown exception. Owned by the
  `lang_class.rs` lane.
* **R2 — resolution 1 remains open as a design.** `AnnotationInvocationHandler`
  and the public `AnnotationParser.annotationForMap(Class, Map)` both exist on
  JDK 25 (verified with `javap` on this host), and `RJdkProxy` passing under
  `--jdk-only` shows the generated-`$ProxyN` half already works in strict mode.
  Retiring the carrier in favour of the real handler is a real option; it is a
  `Compatible`-mode redesign with its own gate and soak, not this fix.
* **R3 — the other three of W7-11's six are untouched.** `RChmKeySetView`,
  `RJdkForkJoin` and `RJdkHandles` are separate causes;
  `__mh_insert_wrapper__` is the only other VM-minted name among the six and is
  worth re-reading against §1 item 6 with this record's taxonomy in hand.

## Not a hole: the proxy shim's fallback arms

Checked because §5 of `natives-over-real-jdk-classes.md` says to read every
fallback after fixing a success path. `native_proxy_new_instance`'s
`Degrade` and `Failed` arms both allocate
`try_alloc_concurrent_synthetic(ctx, "java/lang/reflect/Proxy$Instance", 3)`
through the compatibility door, and `define_or_get_proxy_class`'s
`ensure_vm_internal_class` call sits **after** the `!real_proxy_enabled()`
early return — so on the gate-off path the door is never named. It is safe
anyway, because `fabricated_origin_for_name` classifies that name `VmInternal`
independently. Measured rather than assumed:

```
$ CRATONVM_REAL_PROXY=0 cratonvm.exe --jdk-only --jdk-only-report <F> … RJdkProxy
  → AssertionError: isProxyClass          # the gate-off shim is not a $ProxyN
$ grep 'Proxy\$Instance' <F>              # → 0 rows: never refused
```

The `isProxyClass` failure is what `CRATONVM_REAL_PROXY=0` is *for*, not a
strict-mode refusal. Left alone.

## How to re-take all of this

```sh
# the three vectors, strict and compatible
./target/release/cratonvm.exe --jdk-only  --java-home <jdk-25> -cp regression-suite/build RJdkJmx
./target/release/cratonvm.exe --real-jdk  --java-home <jdk-25> -cp regression-suite/build RJdkJmx
# the census that attributes a refusal to a Rust call site
./target/release/cratonvm.exe --jdk-only --jdk-only-report <FILE> … RReflect
# the registration question, which is not the same as the need question
./target/release/cratonvm.exe --dump-native-registry <FILE> … RReflect
```

`regression-suite/run.sh` supplies `--java-home` for every invocation; a
hand-run that omits it measures the host's default JDK and has inverted a
per-mode verdict before (`W7-11`).
