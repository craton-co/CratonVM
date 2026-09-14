# H15-3 — three stand-ins Compatible mode keeps and does not need, proved by the strict arm that already runs the real bytecode

**Status: DIAGNOSED, NOT FIXED.** No Rust written (diagnosis lane). Three
patches written out, applied nowhere.

**Date** 2026-08-20
**Lane** H15
**Subject** `RJdkProxyIface`, `RJdkFunctionCombinators`, `RJdkEnumerations` —
mechanism A of `H15-1` §3
**Companion** `H15-1` (the mode framing and `RServiceLoaderDoubleSource`),
`H15-2` (`RImmutableFactoryTypes`)
**Binary** `C:/craton/target-jdkonly-h2/release/cratonvm.exe` @ `fe59bf9d9`
**Oracle** HotSpot 25.0.3+9, `/c/Program Files/Microsoft/jdk-25.0.3.9-hotspot`

Claims are **MEASURED** (ran it) or **ARGUED** (read it).

---

## 0. The shared mechanism, and the proof that it is shared

All three vectors fail in Compatible mode on a **`NativeKind::SyntheticStub`
registration that strict mode drops**. `register_inner` refuses `SyntheticStub`
under `JdkOnly`; `NativeKind::allowed_in` returns an unconditional `true` for
`Compatible` (`H4-1` §2, re-read here and unchanged). So the two modes take
different code, and the real JDK bytecode only runs in one of them.

The grouping is not inferred from the symptom — it is **MEASURED per vector**,
and the measurement is the same one that proves the stand-ins are unnecessary:

| vector | Compatible | `--jdk-only` (stand-in dropped, real bytecode runs) | HotSpot |
|---|---|---|---|
| `RJdkProxyIface` | FAIL, 7 of 9 steps | **PASS, 38 checks / 9 steps** | PASS, 38 / 9 |
| `RJdkFunctionCombinators` | FAIL at `Function.andThen` | **PASS, 452 checks** | PASS, 452 |
| `RJdkEnumerations` | FAIL at empty `Hashtable.keys()` | **PASS, 70 checks** | PASS, 70 |

**Every check the stand-in exists to satisfy is satisfied without it.** That is
the finding. It is not an argument from principle — the strict arm is the
experiment, it has already been run, and it is green.

For two of the three, the tree says so itself:

> *"All three mint a `Function$Compose` / `Function$AndThen` /
> `Function$Identity` stand-in, and no JDK declares any of those names: the real
> `Function.compose`/`andThen` are default methods that return a lambda …
> **So there IS a working real-bytecode fallback**"*
> — `native-builtins/src/phases_late/streams.rs:3203-3208`

> *"The native shim (`lang_invoke.rs::register_p68_invoke_extras`) returns the
> METHOD HANDLE ITSELF as the 'proxy', which is not an instance of the requested
> interface. **That is a declared simplification, not a decoder bug.**"*
> — `regression-suite/src/RJdkProxyIface.java:24-30`

**Where they stop being one thing.** The three remedies are genuinely different,
and lumping them would be wrong: one is a deletion, one is a deletion of a
family whose full extent is unmeasured, and one is **not a deletion at all** —
its stand-in is reached through a fallback the strict arm never enters, so
deleting the registration would not help. §1, §2, §3 in that order.

---

## 1. `RJdkProxyIface` — `asInterfaceInstance` returns its own argument

### 1.1 The divergence

**MEASURED.** Compatible mode, 7 of 9 steps fail, six of them with a
`ClassCastException` whose text is the whole diagnosis:

```
FAIL RJdkProxyIface step invokesTheTarget:
  java.lang.ClassCastException: class java.lang.invoke.MethodHandle cannot be
  cast to class RJdkProxyIface$Greeter (java.lang.invoke.MethodHandle is in
  module java.base of loader 'bootstrap'; RJdkProxyIface$Greeter is in unnamed
  module of loader 'app')
```

(the same for `$Sink`, `$Risky`, and three more `$Greeter` steps; the seventh,
`refusals`, fails with `AssertionError: unreachable` because its negative
control — *"a raw `MethodHandle` is NOT a wrapper instance"* — is exactly what
the shim makes true).

| | `MethodHandleProxies.asInterfaceInstance(Greeter.class, mh)` |
|---|---|
| **HotSpot 25.0.3+9** | a generated proxy implementing `Greeter`; `greet("bob")` → `"hi bob"` |
| **CratonVM, Compatible** | **`mh` itself** — the `MethodHandle` argument, unchanged |
| **CratonVM, `--jdk-only`** | a generated proxy implementing `Greeter` (PASS, 38 checks) |

### 1.2 The named function

`native-builtins/src/lang_invoke.rs:8060-8071`, inside
`register_p68_invoke_extras`, under `r.set_category(NativeKind::SyntheticStub)`:

```rust
    r.register(
        mhp,
        "asInterfaceInstance",
        "(Ljava/lang/Class;Ljava/lang/invoke/MethodHandle;)Ljava/lang/Object;",
        |_ctx, args| {
            // Return the method handle as the proxy (simplified)
            Ok(Some(args.get(1).copied().unwrap_or(Value::Object(None))))
        },
    );
```

It ignores `args[0]` (the interface) entirely and returns `args[1]`. Two
siblings were then made *consistent with the lie* rather than correct
(`isWrapperInstance` at `:8092`, `wrapperInstanceTarget` at `:8101`): both
answer off "a wrapper instance IS a `MethodHandle`", which is why the vector's
`refusals` step — the one asserting a raw handle is **not** a wrapper — is red
too. `wrapperInstanceType` is deliberately unregistered (`:8113-8125`).

### 1.3 Why this one is safe to simply delete

The historical reason the shim existed was that the **real** path did not work:
`MethodHandleProxies` spins a proxy class whose `<init>` does
`callerBoundTarget.asType(<MT>)` off an `ldc` of a `CONSTANT_MethodType`, and
the interpreter used to refuse that constant-pool tag
(`ClassFormatError: ldc: unsupported constant pool entry type at #26` — the
vector's own header records it).

**That is fixed, and the vector proves it in the same run.** Its last two steps
build a two-method class with the `java.lang.classfile` API and define it via
`Lookup.defineClass` specifically to ask the decoder question directly.
**MEASURED**, `--jdk-only`:

```
CK RJdkProxyIface ldc-methodtype (String)String
CK RJdkProxyIface ldc-methodhandle (String)String
```

Both green. And the constant-pool decoder
(`vm/src/runtime/interpreter/constants.rs:319`, whose comment names the
`MethodHandleProxies.asInterfaceInstance` proxy template as its reason for
existing) is **mode-independent** — there is no `jdk_only` branch in it
(**ARGUED**, read). So the premise for the simplification is gone in both modes,
and only the strict mode has noticed.

### 1.4 The patch — written out, NOT applied

Delete three registrations from `native-builtins/src/lang_invoke.rs`
(`:8060-8112`): `asInterfaceInstance`, `isWrapperInstance`,
`wrapperInstanceTarget`, together with the `mhp_wrapper_handle` helper at
`:8079-8091` that only they use, and the `let mhp = …` binding if nothing else
in the block uses it. Replace the block with:

```rust
    // MethodHandleProxies — DELIBERATELY UNREGISTERED.
    //
    // `asInterfaceInstance` was a simplification that returned the METHOD
    // HANDLE ITSELF as the "proxy" (it read `args[1]` and ignored `args[0]`,
    // the interface), and `isWrapperInstance` / `wrapperInstanceTarget` were
    // then made consistent with that instead of correct. Compatible mode
    // therefore threw `ClassCastException: java.lang.invoke.MethodHandle cannot
    // be cast to <the requested interface>` at every call site, and answered
    // `true` for "is this raw MethodHandle a wrapper instance?".
    //
    // The reason the shim existed was that the REAL `MethodHandleProxies`
    // bytecode spins a proxy whose `<init>` does `target.asType(<MT>)` off an
    // `ldc` of a `CONSTANT_MethodType`, which the interpreter used to refuse.
    // That is fixed: `constants.rs` decodes both tags, mode-independently, and
    // `RJdkProxyIface`'s `ldcMethodType` / `ldcMethodHandle` steps assert it
    // directly against a class built with `java.lang.classfile`. MEASURED
    // 2026-08-20 under `--jdk-only`, where these registrations are already
    // dropped as `SyntheticStub`: 38 checks, 9 of 9 steps, byte-identical to
    // HotSpot 25.0.3+9. Compatible mode now takes the same path.
    //
    // docs/known-issues/jdk-only/H15-3-three-stand-ins-compatible-mode-does-not-need-20260820.md §1
```

**Do not "fix" the shim instead.** Building a correct proxy in Rust would be a
fourth implementation of proxy generation in a tree that already has three, and
the real one is measured working.

### 1.5 Re-measure

**PREDICTED**: `RJdkProxyIface` GREEN in `SUITE=all` (38 checks, 9 steps).
`--jdk-only` unmoved (the registrations were already dropped there — this patch
is a **no-op** for strict mode by construction, which makes it the safest of the
three). Falsifier: a `ClassFormatError` in the Compatible arm, which would mean
`constants.rs` is not as mode-independent as §1.3 argues.

**Watch for**: any in-tree consumer that relied on `isWrapperInstance` answering
`true` for a raw handle. `grep -rn "isWrapperInstance\|wrapperInstanceTarget"`
finds only the registrations, `vm/src/vm/tests.rs:51827`, and the vector — so
the audit is small, but **the test at `tests.rs:51827` will need updating and is
the reason this patch is not a pure deletion.**

---

## 2. `RJdkFunctionCombinators` — eleven fabricated combinator names, three registrars, one measurement

### 2.1 The divergence

**MEASURED.** Compatible mode dies at:

```
AssertionError: Function.andThen is the fabricated compatibility class
                java.util.function.Function$AndThen
```

| | `f.andThen(g).getClass().getName()` |
|---|---|
| **HotSpot 25.0.3+9** | a generated `…$$Lambda/0x…` |
| **CratonVM, Compatible** | `java.util.function.Function$AndThen` |
| **CratonVM, `--jdk-only`** | a generated lambda (PASS, 452 checks) |

The vector's screen is an **equality against a name list**, not a `!= null` and
not a `contains("$$Lambda")` — deliberately, because the stand-in
`java.util.function.Predicate$$Lambda$And` satisfies the `contains` predicate
that `RJdkLambdas` uses. It also asserts every combinator's **computed value and
short-circuit behaviour**, so a stand-in that merely returns an object fails.

### 2.2 The named functions

`native-builtins/src/phases_late/streams.rs:3221-3300`, in the block that
`r.set_category(NativeKind::SyntheticStub)` opens at `:3221`:

```rust
    r.register(
        func,
        "andThen",
        "(Ljava/util/function/Function;)Ljava/util/function/Function;",
        |ctx, args| {
            …
            let composite = crate::util_concurrent_ext::try_alloc_concurrent_synthetic(
                ctx,
                "java/util/function/Function$AndThen",
                2,
            )?;
```

### 2.3 What I did NOT measure, stated plainly

**The Compatible run stops at the FIRST fabrication it meets, and that is the
third of seven families the vector exercises.** `predicateCombinators()` and the
`Consumer` family both printed their `CK` lines before the failure — so
`Predicate.and`/`or`/`negate` and `Consumer.andThen` are **NOT** taking their
stand-ins in this build, or are taking ones the vector does not name. Everything
after `Function.andThen` — `maxBy`/`minBy`, the primitive combinators, the
`Comparator` family, the null-rejection census — **was never reached**.

By grep (**ARGUED**, not measured), the stand-ins that remain registered are:

| stand-in name | registrar |
|---|---|
| `Predicate$$Lambda$And` | `phases_late/streams.rs:3090` |
| `Predicate$$Lambda$Or` | `phases_late/streams.rs:3135` |
| `Predicate$$Lambda$Negate` | `phases_late/streams.rs:3180` |
| `Function$Compose` | `phases_late/streams.rs:3237` |
| `Function$AndThen` | `phases_late/streams.rs:3263` |
| `Function$Identity` | `phases_late/streams.rs:3286` **and** `lib.rs:41188` (last-write-wins; `lib.rs` is the copy Compatible dispatches) |
| `UnaryOperator$Identity` | `phases_late/streams.rs:2818` |
| `BinaryOperator$MaxBy` / `$MinBy` | `phases_late/streams.rs:2843` |
| `Consumer$AndThen` | `phases_late/streams.rs:3295` |
| `Comparator$Native` | `native-collections/src/lib.rs:33384` |

**So this is one fix or it is up to five, and I cannot tell you which without a
build.** That uncertainty is the honest state and it should not be rounded off:
a lane that deletes only `andThen` and re-runs may well find the vector dies two
lines later on `Function.compose`.

### 2.4 The patch — written out, NOT applied

Delete the `Function.compose` / `Function.andThen` / `Function.identity`
registrations at `phases_late/streams.rs:3232-3292` and the second
`Function$Identity` registration at `lib.rs:41188`. Then re-run and repeat for
whatever the vector reports next, **in that order**, because each deletion
uncovers the next assertion. Replacement comment for the deleted block:

```rust
    // Function.compose / andThen / identity — DELIBERATELY UNREGISTERED.
    //
    // The comment that stood here already said the decisive thing: "there IS a
    // working real-bytecode fallback". `Function.compose`/`andThen` are DEFAULT
    // METHODS with real bodies in every supported image and `identity()`
    // returns `t -> t`; the stand-ins (`Function$Compose`, `$AndThen`,
    // `$Identity`) are names no JDK declares. Tagging them `SyntheticStub` in
    // 2026-08-05 made strict mode run the real methods and left Compatible mode
    // dispatching a stand-in it does not need.
    //
    // MEASURED 2026-08-20 on that strict arm: RJdkFunctionCombinators is
    // 452/452, byte-identical to HotSpot 25.0.3+9, with these registrations
    // dropped. Compatible mode now takes the same path.
    //
    // The `$Compose`/`$AndThen`/`$Identity` `apply` natives further down stay
    // registered on purpose: they are unreachable once nothing mints their
    // receivers, and deleting a native whose class is gone is a separate
    // (safe, cosmetic) change with its own ratchet consequences.
    //
    // NOT A COMPLETE CLOSURE OF THE FAMILY. Nine other stand-in names in this
    // file and one in native-collections are still registered; the vector stops
    // at its first fabrication so their status in Compatible mode is UNMEASURED.
    // docs/known-issues/jdk-only/H15-3-three-stand-ins-compatible-mode-does-not-need-20260820.md §2
```

### 2.5 Re-measure

**PREDICTED**: `RJdkFunctionCombinators` moves **past** `functionCombinators()`.
Whether it reaches `PASS (452 checks)` is **not predicted**. `--jdk-only`
unmoved (no-op there by construction). Falsifier for the whole approach: a
`NoSuchMethodError` or `AbstractMethodError` on `Function.andThen` in Compatible
mode, which would mean the default-method dispatch differs between the modes for
a reason nobody has named.

**Ratchet note**: deleting registrations moves
`native-builtins/tests/stub_ratchet.rs` (`:158` names this exact family). That
test must be re-frozen on a **MEASURED, fully attributed** delta — the rule
`e6d642f3b` landed for.

---

## 3. `RJdkEnumerations` — the empty container is the only one that still fabricates, and deleting a registration will NOT fix it

### 3.1 The divergence, and where it is NOT

**MEASURED.** Compatible mode gets **further than the record's summary
suggests**:

```
CK RJdkEnumerations properties names=14 own=13 first=base.only     ← PASSES
CK RJdkEnumerations chm keys=17 vals=17 dup=[other, same, same]    ← PASSES
AssertionError: empty Hashtable.keys(): the carrier is a fabricated
                compatibility class, java.util.Enumeration$Impl
```

Three things follow, and the third is the finding:

1. **The `Properties` family passes** — `own=13`, not `0`. The brief's lead
   quoted `Properties own key count: 0` from the
   `CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/Hashtable` **armed** run. **The
   unarmed failure does not share that mechanism** (`H15-1` §4c). Checked, not
   assumed.
2. **The `ConcurrentHashMap` family passes.**
3. **The 11-entry `Hashtable` passes and the EMPTY one fails.** The vector
   drains `h.keys()` on an 11-entry table at `:243` and only reaches the empty
   table at `:267`. So the defect is **conditioned on the container being
   empty** — which is exactly the case the vector's own header calls out as
   *"the one a broken carrier most easily gets right by accident"*.

### 3.1b The carrier, measured directly — and a second divergence at the same site

Probe (compile with the oracle's `javac`, run on both):

```java
Hashtable<String,String> empty = new Hashtable<>();
Enumeration<String> k = empty.keys();
System.out.println("empty keys carrier=" + k.getClass().getName()
        + " isIterator=" + (k instanceof Iterator));
System.out.println("empty keys hasMore=" + k.hasMoreElements());
try { k.nextElement(); System.out.println("nextElement did NOT throw"); }
catch (NoSuchElementException x) { System.out.println("nextElement threw NoSuchElementException"); }
Hashtable<String,String> full = new Hashtable<>(); full.put("a","1"); full.put("b","2");
System.out.println("full keys carrier=" + full.keys().getClass().getName());
```

**MEASURED**, both VMs:

| | HotSpot 25.0.3+9 | CratonVM, Compatible |
|---|---|---|
| `new Hashtable<>().keys().getClass()` | `java.util.Collections$EmptyEnumeration` | **`java.util.Enumeration$Impl`** |
| …`instanceof Iterator` | **`false`** | **`true`** |
| …`.hasMoreElements()` | `false` | `false` |
| …`.nextElement()` past the end | **`NoSuchElementException`** | **returns, does not throw** |
| non-empty `keys().getClass()` | `java.util.Hashtable$Enumerator` | `java.util.Hashtable$Enumerator` ✓ |

Three things this settles:

1. The **non-empty** carrier is already the real `Hashtable$Enumerator` on both
   VMs — confirming §3.1's reading that the defect is empty-conditioned.
2. The fabricated empty carrier is wrong in **three** ways, not one: wrong class
   name, wrong `Iterator`-ness, and **it does not throw
   `NoSuchElementException` past the end**. The vector asserts that contract
   explicitly ("a carrier that returns `null` there fails") and simply never
   reaches it, because `carrierIsReal` fires first. **A fix that only renamed
   the carrier would leave the third defect standing**; landing the real
   `Collections$EmptyEnumeration` closes all three at once, which is the
   argument for §3.4's shape over any patch that keeps the snapshot arm.
3. `isIterator=true` on CratonVM against `false` on HotSpot is the exact
   distinction §3.3 warns must not be broken in the *other* direction.

*(Aside, not a defect this lane can close: an empty `Properties`'s carrier is
`java.util.Collections$3` on HotSpot and `java.util.Vector$1` on CratonVM. Both
are real JDK classes, both drain empty, and no assertion in the corpus covers
it — noted so the next reader does not mistake it for a new find.)*

### 3.2 The named functions, and why §1's remedy does not apply here

`native-builtins/src/deprecated_util.rs`:

```rust
fn native_hashtable_keys(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {   // :1453
    let mut this = obj_arg(args, 0)?;
    if hashtable_has_entry(ctx, this) {                                                       // :1455
        let (enm, refreshed) = real_hashtable_enumerator(ctx, this, HASHTABLE_TYPE_KEYS)?;
        if let Some(enm) = enm {
            return Ok(Some(Value::Object(Some(enm))));
        }
        this = refreshed;
    }
    let keys = collect_hashtable(ctx, this, true);
    make_hashtable_enumeration(ctx, keys, 1)                                                  // :1466
}
```

`hashtable_has_entry` (`:1212`) is **false for an empty table**, so the real
`java.util.Hashtable$Enumerator` landing is skipped and control reaches
`make_hashtable_enumeration` (`:1345`), whose primary arm is:

```rust
    let en = match try_alloc_concurrent_synthetic(ctx, "java/util/Enumeration$Impl", 5) {      // :1412
        Ok(en)          => { … }                       ← Compatible mode takes this
        Err(refusal)    => match crate::classloader::real_snapshot_enumeration(ctx, arr)? { … }
                                                       ← strict mode takes this
    };
```

**This is why §1's "delete the registration" does not transfer.** There is no
`SyntheticStub` registration to delete: `Hashtable.keys`/`elements` are
registered normally (`:2151-2164`) and are *needed* — CratonVM populates the
table with its own bucket nodes, so the real `keys()` body would fail. The
strict/Compatible split here is inside `try_alloc_concurrent_synthetic`, which
**refuses** the fabricated class name under `--jdk-only` and **succeeds** under
Compatible. Strict mode is right by accident of the refusal; Compatible mode
takes the arm that is still wrong.

### 3.3 The guard is correct; the fallback behind it is not

**This is the part worth reading carefully, because the obvious fix is wrong.**
`hashtable_has_entry`'s doc block (`:1188-1210`) states its premise, and the
premise is **true** and **verified against the oracle**:

> *"an EMPTY `Hashtable`, where java.base itself does not build an `Enumerator`
> (`getEnumeration` short-circuits to `Collections.emptyEnumeration()` when
> `count == 0`, and that carrier is deliberately not an `Iterator`)"*

Confirmed by the oracle column in §3.1: HotSpot returns
`Collections$EmptyEnumeration`, **not** a `Hashtable$Enumerator`. So **do not
"fix" the guard by letting an empty table build a real `Enumerator`** — that
would produce a carrier that IS an `Iterator` where HotSpot's is not, and the
vector's `carriers dual=4 sharedCursor=2` family exists to catch precisely that
distinction.

The guard's second reason is also live: `Properties` answers `false` here too
(its entries live in `properties_sidetable`, not the slot-0 buckets), and it
**must** keep the snapshot arm. Any fix must therefore not key on
"`hashtable_has_entry` was false" but on "**the snapshot is actually empty**".

### 3.4 The patch — written out, NOT applied

`native-builtins/src/deprecated_util.rs`. One helper plus one line in each of
`native_hashtable_keys` and `native_hashtable_elements`.

```rust
/// The JDK's own `Collections.emptyEnumeration()` carrier, or `None` when this
/// image cannot produce one.
///
/// This is the carrier HotSpot 25 hands back from `Hashtable.keys()` /
/// `elements()` on an EMPTY table: `Hashtable.getEnumeration(int)`
/// short-circuits to `Collections.emptyEnumeration()` when `count == 0` rather
/// than building an `Enumerator`, and that carrier is deliberately NOT an
/// `Iterator`. MEASURED 2026-08-20: `new Hashtable<>().keys().getClass()` is
/// `java.util.Collections$EmptyEnumeration` on HotSpot 25.0.3+9 and was
/// `java.util.Enumeration$Impl` on CratonVM in Compatible mode.
///
/// Same rule as `real_hashtable_enumerator` and
/// `classloader::real_snapshot_enumeration`: prefer a real class the JDK builds
/// itself over a name no image declares. `None` (never a fabrication) when the
/// method is absent — the synthetic-JDK shape, where the caller keeps the
/// snapshot carrier it has always had.
///
/// docs/known-issues/jdk-only/H15-3-three-stand-ins-compatible-mode-does-not-need-20260820.md §3
fn real_empty_enumeration(
    ctx: &mut dyn NativeContext,
) -> Result<Option<ObjectRef>, cratonvm_types::error::MethodCallFailed> {
    if !ctx.method_exists(
        "java/util/Collections",
        "emptyEnumeration",
        "()Ljava/util/Enumeration;",
    ) {
        return Ok(None);
    }
    match ctx.invoke(
        "java/util/Collections",
        "emptyEnumeration",
        "()Ljava/util/Enumeration;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(e)))) => Ok(Some(e)),
        // A failure here is not the application's problem: fall back to the
        // snapshot carrier rather than surfacing it at `Hashtable.keys()`.
        _ => Ok(None),
    }
}
```

and, in **both** `native_hashtable_keys` (`:1453`, the `make_hashtable_enumeration`
call at `:1466`) and `native_hashtable_elements` (`:1432`, its call at `:1443`),
between the collect and the
`make_hashtable_enumeration` call:

```rust
     let keys = collect_hashtable(ctx, this, true);
+    // An EMPTY container — whether a `Hashtable` with no buckets or a
+    // `Properties` whose side table is empty — must yield the JDK's own
+    // `Collections$EmptyEnumeration`, which is what HotSpot returns and what
+    // `hashtable_has_entry`'s own doc names as the short-circuit it is
+    // modelling. Keyed on the SNAPSHOT being empty rather than on
+    // `hashtable_has_entry`, because that predicate also answers false for a
+    // NON-empty `Properties`, which must keep the snapshot arm below.
+    if keys.is_empty() {
+        if let Some(e) = real_empty_enumeration(ctx)? {
+            return Ok(Some(Value::Object(Some(e))));
+        }
+    }
     make_hashtable_enumeration(ctx, keys, 1)
```

**GC note**: `real_empty_enumeration` runs Java code (`Collections.<clinit>` on
first call) and can move the heap. It is called **after** the last use of
`this` and its result is returned immediately, so nothing stale is read — but
that is a property of these two call sites, and a third caller would need a pin.
Stated because the surrounding functions are full of exactly this hazard
(`real_hashtable_enumerator` returns the receiver read back through its pin for
this reason).

### 3.5 Re-measure

**PREDICTED**: `RJdkEnumerations` GREEN in `SUITE=all` (70 checks).
Falsifiers, in order of likelihood:

1. It fails later, on `Collections.list(empty.keys())` or on the
   `carriers dual/sharedCursor` family — meaning
   `Collections$EmptyEnumeration` is being drained differently than the snapshot
   carrier was. The vector drains with a **cap** and asserts
   `NoSuchElementException` past the end, so this is a real risk and is the
   thing to look at first.
2. `Properties` regresses (`own=13` → something else) — meaning an empty
   `Properties` is now taking the new arm where it should not. It should:
   HotSpot gives `emptyEnumeration` there too. But `Properties` in real-JDK mode
   declares its own `keys()`/`elements()` and never reaches this native at all
   (`hashtable_has_entry`'s doc, `:1206-1210`), so any movement in the
   `Properties` line means that resolution claim is false and is a finding in
   its own right.
3. `--jdk-only` moves. It should not: the strict arm already reaches
   `real_snapshot_enumeration` through the `Err(refusal)` branch and never
   allocates the fabrication, so `keys.is_empty()` would divert it to a
   *different* real carrier. **This is the one behavioural change the patch makes
   to strict mode**, and it is why this patch — unlike §1 and §2 — is **not** a
   strict-mode no-op. `RJdkEnumerations` must be re-run in both arms.

---

## 4. NOMINATIONS

**N1 — the `SyntheticStub` tag has been treated as "strict mode fixes it" and
never as "Compatible mode is now known-wrong here".** `H15-1` §0 shows five of
five gate failures are Compatible-mode-only, and §0/§1/§2 above show that for
three of them the strict arm is a **completed experiment proving the stand-in is
unnecessary**. There is a census waiting to be run: for every
`NativeKind::SyntheticStub` registration, does a vector cover it, and does that
vector pass under `--jdk-only`? Every `yes/yes` pair is a stand-in that can be
deleted with the evidence already in hand. `--jdk-only-report` already emits
`SyntheticNativeRegistered` violations naming the site, so the left-hand column
is one run away.

**N2 — `RJdkFunctionCombinators` stops at its first fabrication and cannot
report a family.** §2.3: it names eleven stand-ins and reports one. The vector
should `expect`/`drain` (the shape `RImmutableFactoryTypes` uses) rather than
throw at the first, so one run reports every fabricated combinator instead of
one per rebuild. That is a vector change, not a VM change, and it is worth more
than the first deletion because it turns "one fix or five" into a measurement.

**N3 — `try_alloc_concurrent_synthetic`'s `Err(refusal)` arm is a
mode-conditioned FIX and nobody has counted how many.** §3.2: at
`deprecated_util.rs:1412` the refusal path lands on
`real_snapshot_enumeration`, i.e. the **correct** carrier, and the success path
lands on the fabrication. That inversion — the error arm being the right answer
— almost certainly recurs. `grep -c try_alloc_concurrent_synthetic` across
`native-builtins` bounds the population; each site where the `Err` arm reaches a
real class is a Compatible-mode defect with its remedy already written beside
it.

**N4 — `vm/src/vm/tests.rs` pins the stand-ins §1 and §2 delete, and cannot
simply be re-pointed.** Read, verbatim:

* `m3_function_and_then_apply_chains_correctly` (`:65447-65476`) calls the
  native and asserts `assert_eq!(name, Some("java/util/function/Function$AndThen"))`
  on the result's class. `call_native(...).unwrap()` panics outright if the
  registration is gone, so §2 breaks it **hard**, not subtly.
* `m3_function_and_then_creates_composite` (`:65431-65444`) asserts
  `Function$AndThen` is loaded with a non-empty interface list.
* `method_handle_proxies_p68` (`:51818-51834`) asserts
  `isWrapperInstance(null) == 0`, which §1 removes the registration for.

These encode the **defect** as the contract. And they cannot be inverted into
"assert it is a lambda", because the in-tree test VM has **no class library**
(`SharedVm::new(VmConfig::default())`, no JDK image) — there is no real
`Function.andThen` bytecode for them to reach. So the honest options are
deletion with a retirement comment, or conversion into a registry-shape
assertion that the name is **not** registered. `tests.rs:51805-51816` already
contains a worked precedent for the first (a retired pseudo-method, retired
rather than re-pointed, with the reasoning written down). **Leaving them is not
an option**, and discovering that at build time is the avoidable version of this
note.

---

## 5. What I did NOT solve

* **§2 is a first-failure diagnosis, not a family closure.** Stated at length in
  §2.3 because it is the weakest claim in this record.
* **The armed (`CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/Hashtable`) failure of
  `RJdkEnumerations`** — `Cannot read field "modCount" because "this.this$0" is
  null`, a view carrier with a deliberately-null outer reference — is
  **untouched**. §3.1 establishes only that it is a *different* mechanism from
  the unarmed one. Same for `RJdkProxyIface`'s armed-ConcurrentHashMap failure.
* **Nothing here has been compiled.** Line numbers are from `fe59bf9d9` and all
  three patches add comment blocks, so they have already shifted. Grep the
  literal.
