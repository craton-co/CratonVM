# G18-1 — the proxy invocation contract, and the two vectors that ride on it

> **RECONCILED 2026-08-17 (lane G40) — one "dead body" conclusion is FALSIFIED,
> and the reasoning behind the other is no longer sufficient.**
>
> * **FALSIFIED.** This record reads *"`java/lang/invoke/MethodHandle.asType` **is**
>   registered (`lang_invoke.rs:11491`, `owns_slot=true`) with `invocations=0`, so
>   that native is not the live body either."* **It is the live body.** `G31-1`
>   re-derived it from `owns_slot` plus a behavioural probe and found a
>   passthrough with no convertibility check; the fix landed there, in
>   `2944095fe`.
> * **No longer sufficient.** §1's conclusion that `Proxy$Dispatch.invokeProxy`
>   is dead rests on `invocations=0`. `G33-1` established that `invocations` is a
>   **floor**: it counts only registry-resolved dispatches, and the intrinsic
>   cache and the JIT's direct-call helpers do not go through the registry.
>   `invocations > 0` still proves a body ran; `invocations == 0` proves nothing.
>   That §1 conclusion is independently supported here by the interpreter's
>   `is_proxy_dispatch` interception, and `G24-1` §7.3 reaches the same place —
>   so it is probably right, but **not on the evidence given**.
>
> Everything measured on both VMs in §2 onward is unaffected. `RJdkProxy` has
> since gone green, MEASURED at `783685c34`. See `INDEX.md` §B.1.

**Status:** PART-FIXED-MEASURED. Everything below is MEASURED on both VMs,
**before and after**, and each row says which. **Provenance:** MEAS on both VMs.
HotSpot 25.0.3+9-LTS (`$JAVA_HOME`) is the oracle throughout; CratonVM is
`C:/craton/target-fcheck/release/cratonvm.exe`, `--jdk-only`. The **before**
binary is stamped `2026-08-17 00:44:49`; the **after** binary is
`2026-08-17 01:41:07`, and no `.rs` in the tree is newer than it (the last write
to `reflect_annotations.rs` was `00:59:35`, 42 minutes before the link). Probes (kept out of the repo, in this session's
scratchpad): `PJdkProxy.java` (return + refusal families), `POrder.java`
(refusal ORDER and return-type compatibility), `PSig.java` (the exact
`shortSignature` text), `PMhProxy.java` (`MethodHandleProxies` refusals).

The lane owned three files — `native-builtins/src/{lang_reflect,
reflect_annotations,generics}.rs`. **Both vectors' first divergences turned out
to live outside them**, so the fix here is the *other* half of the surface: the
eleven measured `Proxy.newProxyInstance` / `getProxyClass` refusals CratonVM did
not apply. The two vector-blocking defects are NOMINATIONS in §6, with the
registry evidence for which body to change.

---

## 0. The headline

| surface | before (MEASURED) | after (MEASURED) |
|---|---|---|
| `RJdkProxy` | aborts at check **32 of 37**, `AssertionError: null for an int-returning proxy method must NPE` | **unchanged, 32 of 37** — the defect is in `invoke.rs` (§6.1) |
| `RJdkProxyIface` | **37 checks / oracle 38**, `FAIL step refusals` | **unchanged, 37 of 38** — the defect is in `asType` (§6.2) |
| `newProxyInstance` refusals | **11 of 17** rows diverge | **0 of 17** — differential-clean, messages included |
| `getProxyClass` refusals | **5 of 6** rows diverge | **0 of 6** |
| `isProxyClass` / `getInvocationHandler` | **2 of 5** rows diverge | **0 of 5** |
| refusal ORDER (`POrder`, 12 rows) | 8 diverge | **0** |
| return-type compatibility (`POrder` 13 + `PSig` 6) | 19 diverge | **0** |
| `Proxy` return coercion | **26 of 30** rows diverge | **26 of 30** — NOMINATED, not touched (§6.1) |
| proxy class flags (`isSynthetic`, `ACC_PUBLIC`) | 2 diverge | 2 — NOMINATED (§6.3) |

Vectors held green on the after binary: `RJdkReflect` 67, `RReflect` 40,
`RJdkHandles` 331 (40 steps), `RJdkStrict` 359, `RJdkReflBox` 107,
`RArrayStoreInterfaces` 108 — the last four being every other vector that calls
`java.lang.reflect.Proxy`.

`PJdkProxy`'s D/E/F sections, the whole of `POrder`, and the whole of `PSig` now
diff clean against HotSpot with `tr -d '\r'`. The only residue in those three
probes is the `$ProxyN` counter in a printed class name, which the probes print
and no vector asserts.

`RJdkProxy` reaches check 32 because `exceptionSemantics()` is the fourth of
four sections: 20 checks in `basics()`, 8 in `loaderIdentityAndCaching()`, then
`declared`, `undeclared`, `runtime` — and the 32nd, `null for an int-returning
proxy method must NPE`, is the first that fails.

---

## 1. Which body runs — settled before anything was written

```
class                             name           owns_slot  invocations
java/lang/reflect/Proxy           newProxyInstance   true        6
java/lang/reflect/Proxy           getProxyClass      true        4
java/lang/reflect/Proxy           isProxyClass       true        2
java/lang/reflect/Proxy           getInvocationHandler true      1
java/lang/reflect/InvocationHandler invokeDefault    true        1
java/lang/reflect/Proxy$Dispatch  invokeProxy        true      ** 0 **
java/lang/reflect/Proxy$Instance  <init>             true      ** 0 **
java/lang/reflect/Proxy$Instance  getProxyInterfacesNative true ** 0 **
```

(`--dump-native-registry` **before** the main class, `RJdkProxy` as the
workload. All eight registered from `native-builtins/src/reflect_annotations.rs`,
all `overwrote=null`.)

This is §5 of the handoff firing again, and it decided the whole shape of this
lane. `Proxy$Dispatch.invokeProxy` is where a reader would put the invocation
contract — it has the UndeclaredThrowableException wrapping, the boxing, the
`Method` threading — and its own doc comment calls it "dead code on the hot
path". **It is.** `invocations=0`. The generated `$ProxyN` method bodies never
execute; the interpreter intercepts the call first
(`vm/src/runtime/interpreter/invoke.rs`, the `is_proxy_dispatch` block) and
routes to `vm/src/vm/vm_exec.rs::proxy_invoke_handler_shared`. A fix to
`invokeProxy` would have compiled, built, and done nothing.

The four `Proxy` statics, by contrast, are live with non-zero `invocations` —
which is why §4's fixes go there.

---

## 2. The oracle table — return values (MEASURED, both VMs)

An `InvocationHandler` returning `ret` for a method whose declared return type
is `T`. `[x]` is a message, bare `null` is a null message.

### 2a. `ret == null`

| declared T | HotSpot | CratonVM |
|---|---|---|
| `int` | NPE `[Cannot invoke "java.lang.Integer.intValue()" because the return value of "java.lang.reflect.InvocationHandler.invoke(Object, java.lang.reflect.Method, Object[])" is null]` | NO-THROW, value `0` |
| `long` | NPE, same shape with `java.lang.Long.longValue()` | NO-THROW, `0` |
| `short` | NPE, `java.lang.Short.shortValue()` | NO-THROW, `0` |
| `byte` | NPE, `java.lang.Byte.byteValue()` | NO-THROW, `0` |
| `char` | NPE, `java.lang.Character.charValue()` | NO-THROW, `0` |
| `float` | NPE, `java.lang.Float.floatValue()` | NO-THROW, `0.0` |
| `double` | NPE, `java.lang.Double.doubleValue()` | NO-THROW, **`NaN`** |
| `boolean` | NPE, `java.lang.Boolean.booleanValue()` | NO-THROW, `false` |
| `void` | NO-THROW | NO-THROW |
| `Object` | NO-THROW, `null` | NO-THROW, `null` |
| `String` | NO-THROW, `null` | NO-THROW, `null` |

**8 of 11 diverge.** The message is not derivable — it is the JDK's helpful-NPE
text for the `intValue()` call site inside the generated `$ProxyN` body, and the
wrapper class in it changes per return type.

### 2b. `ret` is a non-exact type — there is NO widening

Every one of the eighteen mismatches below is `ClassCastException` on HotSpot,
with the standard cast message
`class <A> cannot be cast to class <B> (<A> and <B> are in module java.base of
loader 'bootstrap')`. CratonVM throws on none of them.

| ret -> declared T | HotSpot | CratonVM value |
|---|---|---|
| `String` -> `int` | CCE String/Integer | `0` |
| `Integer` -> `String` | CCE Integer/String | `1` (a raw `Integer` pushed as a reference) |
| `Integer` -> `long` | CCE Integer/Long | `7` |
| `Integer` -> `double` | CCE Integer/Double | **`NaN`** |
| `Integer` -> `float` | CCE Integer/Float | **`9.8E-45`** |
| `Byte` -> `int` | CCE Byte/Integer | `7` |
| `Short` -> `int` | CCE Short/Integer | `7` |
| `Character` -> `int` | CCE Character/Integer | `65` |
| `Integer` -> `char` | CCE Integer/Character | `65` |
| `Integer` -> `short` | CCE Integer/Short | `7` |
| `Integer` -> `byte` | CCE Integer/Byte | `7` |
| `Long` -> `int` | CCE Long/Integer | `7` |
| `Integer` -> `boolean` | CCE Integer/Boolean | `true` |
| `Boolean` -> `int` | CCE Boolean/Integer | `1` |
| `Byte` -> `short` | CCE Byte/Short | `7` |
| `Character` -> `long` | CCE Character/Long | `65` |
| `Float` -> `double` | CCE Float/Double | **`NaN`** |
| `Double` -> `float` | CCE Double/Float | `0.0` |
| `String` -> `void` | NO-THROW | NO-THROW |
| `Integer` -> `Object` | NO-THROW, `3` | NO-THROW, `3` |
| `String` -> `Object` | NO-THROW, `sv` | NO-THROW, `sv` |

The three `NaN` / `9.8E-45` cells are the interesting ones: they are not "the
wrong number", they are the wrapper's `int` payload **reinterpreted as float
bits** (`9.8E-45` is `Float.intBitsToFloat(7)`). A caller that boxed the wrong
wrapper gets a plausible-looking value instead of an exception.

I expected `Integer` -> `long` to be legal (assignment widening) and it is not.
That is the row the assignment said to measure rather than assume, and
measuring it is what stopped a "widen where the JDK widens" arm being written.

### 2c. what the handler throws — 6 of 6 already MATCH

| handler throws | both VMs |
|---|---|
| `ParseException("undeclared-checked")` | `UndeclaredThrowableException`, message **null**, cause = the original |
| `IllegalStateException("unchecked")` | verbatim |
| `StackOverflowError("an-error")` | verbatim |
| `Throwable("bare-throwable")` | wrapped in `UndeclaredThrowableException` |
| `IOException("io-undeclared")` | wrapped |
| `ParseException(null)` | wrapped, cause message **null** |

`proxy_wrap_undeclared_if_needed` in `vm_exec.rs` is correct, message included.
Nothing to do here — recorded so the next lane does not re-measure it.

---

## 3. The oracle table — refusals (MEASURED, both VMs)

### 3a. `Proxy.newProxyInstance`

| case | HotSpot | CratonVM (before) |
|---|---|---|
| null loader, public app iface | IAE `[PJdkProxy$Prims referenced from a method is not visible from class loader: null]` | NO-THROW |
| null iface array | NPE `[Cannot read the array length because "interfaces" is null]` | NO-THROW |
| array containing null | NPE `[Cannot invoke "java.lang.Class.getMethods()" because "intf" is null]` | NO-THROW |
| `{Iface, null}` | same NPE | NO-THROW |
| empty iface array | NO-THROW, a 0-interface proxy | NO-THROW |
| null handler | NPE, message **null** | NO-THROW |
| non-interface `Class` | IAE `[java.lang.String is not an interface]` | **matches** |
| primitive `Class` | IAE `[int is not an interface]` | NO-THROW |
| array `Class` | IAE `[[Ljava.lang.String; is not an interface]` | **matches** |
| duplicate interface | IAE `[repeated interface: PJdkProxy$Prims]` | NO-THROW |
| incompatible return types | IAE `[methods with same signature m() but incompatible return types: [class java.lang.String, class java.lang.Integer]]` | NO-THROW |
| same signature, same return | NO-THROW | NO-THROW |
| non-public iface, our loader | NO-THROW, class in **that** package | NO-THROW |
| non-public + public iface | NO-THROW | NO-THROW |
| non-public iface, null loader | IAE `... not visible from class loader: null` | NO-THROW |
| iface not visible to loader | IAE `... not visible from class loader: null` | NO-THROW |
| annotation type as iface | NO-THROW | NO-THROW |

**11 of 17 diverge.**

### 3b. `Proxy.getProxyClass` — the same `ProxyBuilder`, so the same messages

| case | HotSpot | CratonVM (before) |
|---|---|---|
| null loader | IAE `... not visible from class loader: null` | NO-THROW |
| null array | NPE `[Cannot read the array length because "interfaces" is null]` | NO-THROW |
| containing null | NPE `[Cannot invoke "java.lang.Class.getMethods()" because "intf" is null]` | NO-THROW |
| non-interface | IAE `[java.lang.String is not an interface]` | NO-THROW |
| duplicate | IAE `[repeated interface: PJdkProxy$Prims]` | NO-THROW |
| empty | NO-THROW | NO-THROW |

**5 of 6 diverge.** `getProxyClass` had **no** validation at all — not even the
one refusal `newProxyInstance` did apply.

### 3c. `isProxyClass` / `getInvocationHandler`

| case | HotSpot | CratonVM (before) |
|---|---|---|
| `isProxyClass(null)` | NPE, message **null** | NO-THROW, `false` |
| `isProxyClass(int.class)` | `false` | `false` |
| `isProxyClass(Proxy.class)` | `false` | `false` |
| `getInvocationHandler(null)` | NPE `[Cannot invoke "Object.getClass()" because "proxy" is null]` | NPE `[proxy is null]` |
| `getInvocationHandler("x")` | IAE `[not a proxy instance]` | **matches** |

### 3d. the ORDER, pinned by putting two violations in one call

This is the part reading cannot give you, and getting it wrong produces the
right exception class with the wrong message.

| call | HotSpot answers |
|---|---|
| null handler **and** null array | NPE, message **null** — `requireNonNull(h)` is first |
| null handler and a non-interface | NPE, message null |
| `{String.class, null}` | the `getMethods()` NPE — the referenced-types pass sweeps **every** element before any per-element check |
| `{null, String.class}` | the same NPE |
| `{String.class, String.class}` | `java.lang.String is not an interface` |
| `{A, Other, A}` | `repeated interface: POrder$A` |
| `{int.class, A}` | `int is not an interface` |
| null loader + `POrder` (an app **class**) | `POrder is not an interface` |
| null loader + `{A, A}` (an app **interface**) | `POrder$A referenced from ... class loader: null` |
| null loader + `Runnable` | NO-THROW |
| null loader + `{Runnable, Runnable}` | `repeated interface: java.lang.Runnable` |

The last four settle it: within the per-element loop it is
**isInterface -> ensureVisible -> duplicate**, not the order
`ProxyBuilder.validateProxyInterfaces` reads like if you skim it.

### 3e. return-type compatibility

| interfaces | HotSpot |
|---|---|
| `String m()` + `Integer m()` | IAE `[... incompatible return types: [class java.lang.String, class java.lang.Integer]]` |
| `Integer m()` + `String m()` | `[class java.lang.Integer, class java.lang.String]` — **argument order** |
| `String m()` + `Object m()` | NO-THROW (covariant) |
| `Object m()` + `String m()` | NO-THROW |
| `String m()` + `CharSequence m()` | NO-THROW |
| `String m()` + `int m()` | `... incompatible return types: int and others` |
| `int m()` + `String m()` | `int and others` — the **primitive** is named either way |
| `void s()` + `String s()` | `void and others` |
| `A` + `E extends A` | NO-THROW |
| `String` + `Integer` + `Object` | `[class java.lang.String, class java.lang.Integer]` |
| `Object` + `CharSequence` + `String` | NO-THROW |
| `String[] t()` + `Integer[] t()` | `[class [Ljava.lang.String;, class [Ljava.lang.Integer;]` |
| `Object[] u()` + `String[] u()` | **NO-THROW** — array covariance |

and the signature text itself, which is `Method.toShortSignature()`:

| method | text in the message |
|---|---|
| `String m(int, String, long[])` | `m(int,java.lang.String,long[])` |
| `String q(List<String>)` | `q(java.util.List)` — erased |
| `int r()` | `r()` |

Note the two spellings the message mixes: parameters use
`Class.getTypeName()` (`long[]`) while the return-type list uses
`Class.toString()` over `Class.getName()` (`class [Ljava.lang.String;`).
Using one spelling for both would have been wrong in both places.

---

## 4. What was fixed, in `native-builtins/src/reflect_annotations.rs`

One shared validator, `proxy_validate_interfaces`, now applies steps 2-5 of the
contract for **both** `newProxyInstance` and `getProxyClass`, plus step 1
(`requireNonNull(h)`) in `newProxyInstance` alone. `isProxyClass(null)` and
`getInvocationHandler(null)`'s message were corrected in place.

Three decisions in it are deliberate narrowings, each because a wrong refusal is
worse than a missing one:

* **A `Class` mirror with no `ClassId` is refused only if its name is one of
  the nine primitives.** `int.class` is the shape the JDK refuses here and it
  carries its name in mirror slot 1 with no class id at all. Any *other*
  unresolvable mirror is skipped exactly as the loop skipped it before —
  inventing "X is not an interface" from a name this VM merely failed to
  resolve is the failure mode the sibling lane's `vn.contains("$Proxy")` bug
  was.
* **`ensureVisible` is implemented only for the null-loader arm, and needs two
  independent signals to agree.** `NativeContext::loader_id_of_class` reports
  `Application` both for a genuine application class and for a class it has no
  record of, and that conflation would turn the extremely common
  `newProxyInstance(jdkIface.getClassLoader(), …)` — whose loader argument *is*
  null — into a spurious `IllegalArgumentException`. The second signal is the
  module name: a class in the JDK image has one, a classpath class does not.
  Both must say "not bootstrap" before it refuses. That this VM *can* tell the
  two apart was measured rather than assumed (`PMod.java`, `--jdk-only`):

  | class | CratonVM `getModule().getName()` | CratonVM `getClassLoader()` |
  |---|---|---|
  | `java.lang.Runnable` | `java.base` | `null` (bootstrap) |
  | `java.util.List` | `java.base` | `null` |
  | `java.sql.Connection` | `java.sql` | `null` — HotSpot says **PlatformClassLoader** |
  | app interface | `null` | `AppClassLoader` |

  So a JDK interface passed with a null loader keeps its module name and is
  never refused, which is the case that had to stay working. (The
  `java.sql.Connection` row is a separate, pre-existing divergence — CratonVM
  puts every JDK-image class on the bootstrap loader where HotSpot uses the
  platform loader for non-`java.base` modules. Nothing asserts it, and it makes
  this arm *more* conservative, not less.)
* **`checkReturnTypes` runs only for two or more interfaces**, and fails open on
  any return type it cannot resolve. A conflict needs two same-signature
  methods whose return types are mutually unassignable; inside one interface
  hierarchy javac rejects that at compile time, and all five measured conflict
  rows pass two interfaces. It also keeps `declared_methods` — a
  `Vec<MethodMetadata>` allocation per class — off the overwhelmingly common
  single-interface `newProxyInstance` path.

Array covariance is implemented rather than collapsed, because the two measured
array rows disagree: `{Object[] u(), String[] u()}` is accepted and
`{String[] t(), Integer[] t()}` is refused. A "the arrays differ, so it is a
conflict" rule would have refused a proxy HotSpot builds.

Unit tests were added in a new `proxy_refusal_contract_tests` module for the
pieces that need no VM: the two `Class` name spellings, `toShortSignature`, the
descriptor splitters, and the context-free half of the assignability walk
(`proxy_desc_assignable_opt(None, …)`), which is where the two array rows are
decided. The `None`-means-fail-open contract is asserted directly.

---

## 5. What this lane did NOT do

* **Did not touch the return-value coercion.** It is in
  `vm/src/runtime/interpreter/invoke.rs`, which this lane does not own. §2a and
  §2b are measured and handed over unfixed; that is 26 diverging rows and it is
  the whole of `RJdkProxy`'s remaining gap.
* **Did not touch `MethodHandleProxies` / `asType`.** `RJdkProxyIface`'s single
  missing check is there (§6.2), also outside the three files.
* **Did not change `Proxy$Dispatch.invokeProxy`** even though it reads like the
  place for all of this. `invocations=0`.
* **Did not implement `ensureVisible` for a non-null loader.** The JDK's real
  rule is "`Class.forName(intf.getName(), false, loader) == intf`"; CratonVM
  can answer that for the bootstrap loader and not, with confidence, for an
  arbitrary user-defined one. Both measured rows use a null loader, so this
  costs no row — but a proxy built over an interface the supplied child loader
  cannot see will still be accepted here and refused by HotSpot.
* **Did not test more than 65535 interfaces.** Generating 65536 distinct
  interfaces is not cheap and no measured behaviour depends on it.
* **Did not check `intf.isHidden()`**, which sits between `isInterface` and
  `ensureVisible` in the JDK's loop (`<name> is a hidden interface`). No probe
  row exercised it; adding the refusal without a measured row would be a
  prediction.
* **Did not re-measure §2c.** The UndeclaredThrowableException family already
  matched on all six rows, message text included.
* **Did not build the binary it measured.** `cargo build` was forbidden. The
  lane measured the before-state on the `00:44:49` binary, wrote the fix, and
  re-measured on the `01:41:07` binary the orchestrator produced. That the fix
  is in that binary is established by BEHAVIOUR, not by timestamps: eleven
  refusals that previously did not fire now fire with the exact measured
  messages, and `POrder` and `PSig` went from 27 diverging rows to zero.

---

## 6. NOMINATIONS

### 6.1 `vm/src/runtime/interpreter/invoke.rs` — the proxy return coercion

> **Taken up.** `G24-1-the-proxy-return-coercion-on-the-live-path-20260817.md`
> is the lane that owns `invoke.rs` and `vm_exec.rs` acting on this. It
> re-derived the live body from the registry rather than taking it on trust,
> widened the probe to 59 rows, and reports 29 of the 32 divergences closed.
> Its fix is NOT in the `01:41:07` binary, which is why `RJdkProxy` still
> measures 32 of 37 in §0.

In the `is_proxy_dispatch` block (around line 1915, immediately after
`crate::vm::proxy_invoke_handler_shared`), the result is coerced with five
copies of the same shape, one per return-descriptor group:

```rust
'I' | 'Z' | 'B' | 'C' | 'S' => {
    if let Value::Object(Some(obj)) = value {
        shared.mem.heap.get_field(obj, 0)
    } else {
        value                      // <-- null lands here
    }
}
```

Two defects:

1. **`Value::Object(None)` takes the `else` arm** and is pushed as the
   primitive return value. §2a: 8 of 8 rows. HotSpot throws
   `NullPointerException` with the transcribed message in §2a — note the
   wrapper class in the text varies with the return type.
2. **No wrapper-class check.** `get_field(obj, 0)` reads slot 0 of whatever
   object arrived, so §2b's 18 rows all silently succeed, three of them by
   reinterpreting an `int` payload as float bits. HotSpot throws
   `ClassCastException`, and it does **not** widen: `Integer` for a `long`
   return is a CCE.

These are `RJdkProxy` checks 32 and 33. A helper with the right shape already
exists one file over — `vm/src/vm/vm_exec.rs::proxy_unbox_primitive_return`
(around line 19158) — but it is wired only into the `AnnotationProxy` arm of
`proxy_invoke_handler_shared` and has both holes itself, so it needs the fix
too rather than merely being called from more places.

Do **not** fix this in `native-builtins/.../reflect_annotations.rs
::native_proxy_dispatch_invoke`: `invocations=0` (§1).

### 6.2 `java.lang.invoke` — `asType` does not refuse an unconvertible handle

`RJdkProxyIface`'s `refusals` step, sub-check 2:

| | |
|---|---|
| call | `MethodHandleProxies.asInterfaceInstance(Subtractor.class, mh)`, `mh` = `(String)String`, SAM = `(int,int)int` |
| HotSpot | `WrongMethodTypeException: cannot convert MethodHandle(String)String to (int,int)int` |
| CratonVM | NO-THROW — returns `jdk.MHProxy1.PMhProxy$Subtractor` |

That is the whole difference between 37 and 38 checks. The arithmetic confirms
which sub-check it is: `refusals` counts 3 checks when it passes; a failure at
sub-check 1 would cost 2 checks, at sub-check 3 would cost 0, and only a
failure at sub-check 2 costs exactly 1.

No `MethodHandleProxies` native is registered, so real JDK bytecode runs and
the missing refusal is in the `asType` / `asTypeUncached` conversion path.
`java/lang/invoke/MethodHandle.asType` **is** registered
(`native-builtins/src/lang_invoke.rs:11491`, `owns_slot=true`) with
`invocations=0`, so that native is not the live body either — settle the live
one with the registry before editing.

Everything else in that family already matches HotSpot exactly, message
included: non-interface (`not a public interface: java.lang.String`), null
target (NPE, null message), null interface (`Cannot invoke
"java.lang.Class.isInterface()" because "intfc" is null`), two abstract methods
(`not a single-method interface: PMhProxy$TwoSam`), no abstract method (`no
method in : PMhProxy$NoSam`), primitive class, widening return, narrowing
param.

### 6.3 `classloading/src/proxy_gen.rs` — two class flags on the generated `$ProxyN`

Neither vector asserts these; both are measured.

| | HotSpot | CratonVM |
|---|---|---|
| `proxyClass.isSynthetic()` | `false` | **`true`** |
| proxy over a **non-public** interface: `Modifier.isPublic(proxyClass.getModifiers())` | `false` | **`true`** |

The package placement for the non-public case is already right (`pkgb`, and the
class loader is the one that was asked for) — only the two flags differ.
HotSpot clears `ACC_PUBLIC` when any proxied interface is non-public.

---

## 7. Two things here that are easy to disbelieve

* **`Integer` returned for a `long`-returning proxy method is a
  `ClassCastException`, not a widening conversion.** The `InvocationHandler`
  contract is `Class.cast`-exact per the declared return type; there is no
  assignment conversion anywhere in it.
* **`Proxy.newProxyInstance(loader, new Class<?>[0], h)` succeeds** and yields a
  zero-interface proxy whose superclass is `java.lang.reflect.Proxy`. It is
  `getProxyClass()` with no interfaces at all that also succeeds. Nothing in the
  refusal set treats "no interfaces" as an error, so a validator that adds that
  check to look thorough breaks a legal call.

---

## 8. What could not be settled

* **Whether `ensureVisible` is right for a NON-null loader.** Its null-loader
  arm is measured working in both directions on the after binary — it refuses
  `{PJdkProxy$Prims}` and `{pkgb.Hidden}` and does not refuse `{Runnable}` —
  but a loader that is non-null and still cannot see the interface is accepted
  here and refused by HotSpot (§5).
* **Where `asType`'s conversion check lives.** §6.2 rules out the two natives
  it would obviously be (`MethodHandleProxies` has none registered;
  `MethodHandle.asType` has `invocations=0`), but does not name the live body.
* **`java.sql.Connection`'s defining loader** (§4) — bootstrap on CratonVM,
  PlatformClassLoader on HotSpot. Observed in passing; no vector asserts it and
  it was not investigated.
* **Whether the single-interface narrowing of `checkReturnTypes` (§4) ever
  costs a row.** No measured case needs it, and javac forbids the shape inside
  one hierarchy — but a hand-built classfile could carry it, and this would
  accept it.
