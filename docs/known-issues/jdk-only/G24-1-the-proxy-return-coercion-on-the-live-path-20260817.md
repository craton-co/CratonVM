# G24-1 — the proxy return coercion, on the path that actually runs

> **RECONCILED 2026-08-17 (lane G40) — the fix is CONFIRMED; one supporting
> inference is not.**
>
> * **Confirmed.** `RJdkProxy` passes, **MEASURED** on the `783685c34` binary
>   under `--jdk-only`. §8's pending-a-binary state is closed.
> * **Not sufficient.** §7.3 says of `Proxy$Dispatch.invokeProxy` that *"its doc
>   comment says it is dead, and `invocations=0` confirms it"*. It does not
>   confirm it. `G33-1` established that `invocations` is a **floor** — it counts
>   only registry-resolved dispatches, so `invocations == 0` proves nothing about
>   liveness. The doc comment and the interpreter's `is_proxy_dispatch`
>   interception are the load-bearing evidence here; the zero adds nothing. The
>   companion case is `G18-1`'s reading of `MethodHandle.asType`, which the same
>   reasoning got **wrong** — `G31-1` proved that body live and fixed it in
>   `2944095fe`. See `INDEX.md` §B.1.

**Status:** FIXED, before-state MEASURED, after-state PENDING-A-BINARY (§8).
**Provenance:** MEAS on both VMs for every "before" cell. HotSpot
25.0.3+9-LTS (`$JAVA_HOME`) is the oracle throughout; CratonVM is
`C:/craton/target-fcheck/release/cratonvm.exe`, `--jdk-only`, binary stamped
`2026-08-17 00:44:49` — the same binary G18-1 measured on, and it does **not**
contain this change. Probe (kept out of the repo, in this session's
scratchpad): `g24/PRet.java`, 59 rows over one `InvocationHandler` and eighteen
declared return types.

This lane owns `vm/src/runtime/interpreter/invoke.rs` and
`vm/src/vm/vm_exec.rs`. G18-1 handed it a measured 26-row divergence and the
registry evidence for where the live body is; §1 re-derives that from the
binary rather than taking it, §2 and §3 re-measure the contract with a wider
probe, §4 is the fix, and §5 is the part the assignment cared about most — the
enumeration of which currently-green vectors reach the changed code and why
each is safe.

---

## 0. The headline

| vector | before (MEASURED) | after |
|---|---|---|
| `RJdkProxy` | aborts at check **32 of 37**, `AssertionError: null for an int-returning proxy method must NPE` | §8 |
| `RJdkProxyIface` | **37 checks / oracle 38**, `FAIL step refusals` | unchanged — the defect is `asType`, and §7.1 now names the file |
| proxy return-coercion family | **32 of 59** probe rows diverge, **0** refusals raised where HotSpot raises 29 | 29 of the 32 closed by construction, 3 deliberately left (§6) |

---

## 1. Which body runs — re-derived, not inherited

G18-1 §1 settled that `Proxy$Dispatch.invokeProxy` is dead
(`invocations=0`) and that the interpreter intercepts first. Nothing here
contradicts it. What this lane adds is the *shape* of the live path, because
"the fix lives in `invoke.rs`" turned out to be half the answer:

```
interpreter invokevirtual/invokeinterface
  -> invoke.rs::execute_invoke_kind, `is_proxy_dispatch` block   (~1914)
       -> vm_exec.rs::proxy_invoke_handler_shared
            |-- AnnotationProxy handler  -> annotation dispatch -> LENIENT unbox
            |-- lambda handler           -> try_lambda_dispatch
            \-- object handler           -> InvocationHandler.invoke

JIT invokevirtual/invokeinterface cache miss
  -> vm_exec.rs (~17459), keyed on `effective_class == Proxy$Instance`
       -> the same `proxy_invoke_handler_shared`

NativeContextImpl virtual dispatch
  -> vm_exec.rs (~10200) -> proxy_invoke_handler   (the same three arms again)
```

Three call sites, one shared body, and **the annotation arm sits inside that
shared body**. That is the fact that decided where the fix goes. G18-1's N1
said `proxy_unbox_primitive_return` "is wired only into the AnnotationProxy
arm"; it is in fact wired into five places, two of which are annotation arms
and three of which are user-handler arms. Making *it* strict would have put
`RJdkStrict`'s 359 annotation checks in the blast radius for no gain.

---

## 2. The oracle — `ret == null` (MEASURED, both VMs, `PRet`)

| declared T | HotSpot | CratonVM (before) |
|---|---|---|
| `int` | NPE `[Cannot invoke "java.lang.Integer.intValue()" because the return value of "java.lang.reflect.InvocationHandler.invoke(Object, java.lang.reflect.Method, Object[])" is null]` | NO-THROW, `0` |
| `long` | NPE, same shape, `java.lang.Long.longValue()` | NO-THROW, `0` |
| `short` | NPE, `java.lang.Short.shortValue()` | NO-THROW, `0` |
| `byte` | NPE, `java.lang.Byte.byteValue()` | NO-THROW, `0` |
| `char` | NPE, `java.lang.Character.charValue()` | NO-THROW, `\0` |
| `float` | NPE, `java.lang.Float.floatValue()` | NO-THROW, `0.0` |
| `double` | NPE, `java.lang.Double.doubleValue()` | NO-THROW, `0.0` |
| `boolean` | NPE, `java.lang.Boolean.booleanValue()` | NO-THROW, `false` |
| `void` | NO-THROW | NO-THROW |
| `Object` / `String` / `int[]` | NO-THROW, `null` | NO-THROW, `null` |

**8 of 12 diverge.** The eight message texts were transcribed one at a time
from the probe output and are asserted one at a time in
`the_null_return_npe_text_is_transcribed_per_primitive`. They are not derivable:
the accessor is not the lowercased wrapper (`Character` -> `charValue`, not
`characterValue`), and the invariant `because` clause spells the handler
signature `invoke(Object, java.lang.reflect.Method, Object[])` — two of three
parameters shortened, the middle one fully qualified.

One divergence from G18-1 §2a worth flagging rather than reconciling: it
records CratonVM answering **`NaN`** for a `double` return of `null`, on the
same binary; `PRet` gets `0.0`. The two probes differ in handler shape
(`PJdkProxy` uses an anonymous `InvocationHandler` class, `PRet` a lambda) and
those are two different arms of `proxy_invoke_handler_shared`, so it is
plausible both cells are real. Nothing depends on which — both are the same
missing NPE — and both arms are fixed here; recorded so the next reader does
not chase it as a contradiction.

---

## 3. The oracle — wrong type (MEASURED, both VMs, `PRet`)

Every refusal below is `ClassCastException` on HotSpot with the standard cast
message; CratonVM raised **none** of them.

### 3a. wrapper -> primitive — there is NO widening

| ret -> T | HotSpot | CratonVM value |
|---|---|---|
| `Integer` -> `int` | NO-THROW | `7` |
| `Integer` -> `long` | CCE Integer/Long | `7` |
| `Integer` -> `short` | CCE Integer/Short | `7` |
| `Integer` -> `byte` | CCE Integer/Byte | `7` |
| `Integer` -> `char` | CCE Integer/Character | `\a` (7) |
| `Integer` -> `float` | CCE Integer/Float | **`9.8E-45`** |
| `Integer` -> `double` | CCE Integer/Double | `7.0` |
| `Integer` -> `boolean` | CCE Integer/Boolean | `true` |
| `Byte` -> `int` | CCE Byte/Integer | `7` |
| `Byte` -> `short` | CCE Byte/Short | `7` |
| `Byte` -> `byte` | NO-THROW | `7` |
| `Short` -> `int` | CCE Short/Integer | `7` |
| `Character` -> `int` | CCE Character/Integer | `65` |
| `Character` -> `long` | CCE Character/Long | `65` |
| `Long` -> `int` | CCE Long/Integer | `7` |
| `Boolean` -> `int` | CCE Boolean/Integer | `1` |
| `Float` -> `double` | CCE Float/Double | **`NaN`** |
| `Double` -> `float` | CCE Double/Float | `0.0` |
| `String` -> `int` | CCE String/Integer | `0` |
| `Integer` -> `void` / `String` -> `void` | NO-THROW | NO-THROW |

`9.8E-45` is `Float.intBitsToFloat(7)` and `NaN` is the low half of a `double`
read as a `float`: the pre-fix code read slot 0 of whatever object arrived and
handed the raw bits to a differently-typed stack slot. A caller that boxed the
wrong wrapper got a plausible-looking number rather than an exception.

**`Integer` for a `long`-declared method is a `ClassCastException`.** It is not
an assignment-widening site and there is no conversion anywhere in the
`InvocationHandler` contract — the generated body does
`checkcast java/lang/Long`. This is the row that a careful reading of the JDK
gets wrong, and G18-1 measured it before this lane existed; it was re-measured
here and holds.

### 3b. reference -> reference

| ret -> T | HotSpot | CratonVM |
|---|---|---|
| `Integer` -> `String` | CCE Integer/String | NO-THROW, a raw `Integer` pushed as a `String` |
| `Integer` -> `Object` / `Number` / `Comparable` / `Integer` | NO-THROW | NO-THROW |
| `String` -> `Number` | CCE String/Number | NO-THROW |
| `String` -> `Runnable` | CCE String/Runnable | NO-THROW |
| `String` -> `Object` / `String` / `Comparable` | NO-THROW | NO-THROW |
| `Byte` -> `Number` | NO-THROW | NO-THROW |
| lambda -> `Runnable` | NO-THROW | NO-THROW |
| lambda -> `String` | CCE `PRet$$Lambda/0x…`/String | NO-THROW |
| proxy -> `Object` | NO-THROW | NO-THROW |

The `Comparable` and `Number` rows are why this cannot be a name equality test:
`Integer` -> `Comparable` and `String` -> `Comparable` are both legal and both
go through the interface DAG, while `String` -> `Number` and `String` ->
`Runnable` are refusals reached by the same walk.

`Integer` -> `String` is `RJdkProxy` check **33**, and it is a *reference*
return. A fix that only handled primitives would have moved the vector from 32
to 33 and stopped.

### 3c. arrays

| ret -> T | HotSpot | CratonVM |
|---|---|---|
| `String[]` -> `String[]` / `Object[]` / `Object` | NO-THROW | NO-THROW |
| `String[]` -> `int[]` | CCE `[Ljava.lang.String;`/`[I` | NO-THROW |
| `int[]` -> `int[]` | NO-THROW | NO-THROW |
| `int[]` -> `Object[]` | CCE `[I`/`[Ljava.lang.Object;` | NO-THROW |
| `Integer` -> `int[]` | CCE Integer/`[I` | NO-THROW |

Note both spellings HotSpot mixes in one message: operands are
`Klass::external_name()` (dotted, arrays kept as descriptors —
`[Ljava.lang.String;`, never `String[]`), which is a different renderer from
the JEP 358 one the NPE texts in §2 use.

### 3d. what the handler throws — 6 of 6 already MATCH

Unchanged from G18-1 §2c and **not re-measured**; the
`UndeclaredThrowableException` family including the null message was already
correct. §4 keeps `proxy_wrap_undeclared_if_needed` strictly innermost so an
`Err` is wrapped *before* the new coercion sees it, and the coercion passes
every `Err` through untouched.

---

## 4. The fix

### 4.1 `vm/src/vm/vm_exec.rs` — one strict coercion, three user-handler sites

`proxy_coerce_handler_return(shared, descriptor, result)` applies the contract:

* **primitive return**: `Object(None)` -> NPE with the transcribed per-wrapper
  text; `Object(Some(o))` whose runtime class **is exactly** the wrapper ->
  slot 0; any other object -> CCE; an already-raw `Value` -> through unchanged.
* **`V`**: through unchanged.
* **reference return**: `null` -> through; otherwise refuse iff the value is
  provably not an instance of the declared type.
* **`Err`**: through unchanged, so the exception wrap stays authoritative.

Wired into the three points where a **user's** `InvocationHandler` result comes
back — `proxy_invoke_handler_shared`'s lambda arm and object-handler tail, and
`proxy_invoke_handler`'s object-handler tail. The lambda arm returned the
handler's value *verbatim*, leaving the coercion entirely to its caller, and a
`(proxy, m, a) -> …` lambda passed straight to `newProxyInstance` is exactly
what `RJdkProxy` uses — so the vector's `null` travelled from that arm into
`invoke.rs`'s `else { value }` and onto the operand stack as `0`.

`proxy_unbox_primitive_return` is **unchanged** and still serves the two
annotation arms. That separation is the whole safety argument of §5.3.

### 4.2 The refusal is admit-biased by construction

A wrong refusal turns a working proxy into a thrown exception, which is
strictly worse than the missing refusal it replaces. So the predicate answers
"no opinion" — and the caller accepts — for:

* an **array** value (`class_id_of` on an array header carries the *component*
  class, so a `String[]` would answer `is_assignable_to_name("java/lang/String")`
  — true, for the wrong reason) and an **array** declared type;
* a **lambda proxy** value (synthetic `ClassId`, no `interfaces` list);
* anything whose superclass chain reaches `Proxy$Instance` or
  `java/lang/reflect/Proxy` (a fabricated `$ProxyN` records
  `GeneratedProxy { interfaces: [] }`, which is "no record", not "implements
  nothing" — the distinction `recorded_proxy_interface_set` in
  `interpreter/typecheck.rs` exists to draw);
* a class the class manager cannot produce;
* a declared type this VM has no unique definition for — then "not assignable"
  is a statement about the class store, not about the value;
* anything `synthetic_implements_public` or `annotation_proxy_satisfies_target`
  admits. Both are borrowed from `interpreter/typecheck.rs` rather than
  reimplemented, and they are called at exactly the point
  `aastore_element_assignable` calls them: after a by-name walk has already
  declined, admit-only.

Assignability itself is `Class::is_assignable_to_name` — supers **and** the
interface DAG, compared by NAME. By name is not a shortcut here, it is the
requirement: it needs no `ClassId` for the declared type, so **no class loading
happens on a dispatch path**. `Ljava/lang/Object;` short-circuits before any
lock is taken.

### 4.3 `vm/src/runtime/interpreter/invoke.rs` — the five copies collapse

The `is_proxy_dispatch` block held five copies of one
`if let Value::Object(Some(obj)) = value { get_field(obj, 0) } else { value }`,
one per return-descriptor group. Both defects lived in it, and the duplication
was not either of them. They are now gone from this file entirely: by the time
a value reaches this line, `proxy_invoke_handler_shared` has already coerced it
through one arm or the other. The lenient helper is still called — a no-op on
an already-raw value — because this is the documented boundary between the
shared dispatch hook and the interpreter's operand stack, and 30 lines of
five-way duplication reading as if it were the coercion is how the next lane
looks in the wrong file.

### 4.4 The message text is built to be finished by the existing funnel

`runtime::exceptions::throw_runtime_error` is the single funnel every VM-raised
`RuntimeError` passes through, and it already rewrites a bare
`class X cannot be cast to class Y` into HotSpot's full form with the
`(… are in module java.base of loader 'bootstrap')` parenthetical. Its splitter
is deliberately strict: both operands whitespace-free, nothing else in the
message. So the refusals here are minted in exactly that bare shape, and
`the_cast_refusal_is_in_the_shape_the_message_funnel_rewrites` asserts the
splitter's own preconditions rather than the finished string — a test that
asserted the parenthetical would be testing the funnel, and would pass while
this message quietly stopped qualifying for it.

### 4.5 Unit tests

Six, in `vm_exec.rs`'s existing `mod tests`, all VM-cheap:

* the eight NPE texts, spelled out;
* only the eight primitive descriptors have a wrapper (`V` in particular must
  not — `null` for a `void` proxy method is legal, MEASURED);
* each primitive maps to its own wrapper, which is what "no widening" *means*;
* the return-descriptor split, including the no-`)` input the previous
  `rsplit(')')` idiom answered wrongly;
* the cast message satisfies the funnel's splitter, arrays included;
* the coercion's heap-free arms: `Ok(None)`, null over a reference return, null
  over each of four primitive returns, raw `Int`/`Double` pass-through, `Err`
  pass-through; and `Ljava/lang/Object;`/array descriptors never refusing.

`invoke.rs` has no `#[cfg(test)]` module and this lane did not add one: the
change there is a call-site collapse with no new logic to pin.

---

## 5. Which currently-green vectors reach the changed code, and why each is safe

The changed code is reached by **exactly one thing**: a call whose dispatch
class is `java/lang/reflect/Proxy$Instance`, or whose receiver's superclass
chain reaches it. That predicate is unchanged by this lane and it is computed
identically before and after. No ordinary method call enters the block, and the
`is_proxy_dispatch` test itself — a literal string compare plus, only on a
miss, a bounded superclass walk on the receiver — is not touched.

### 5.1 The eight vectors, before this change (MEASURED, this binary)

| vector | before |
|---|---|
| `RJdkProxy` | FAIL at check 32 of 37 |
| `RJdkProxyIface` | FAIL `step refusals`, 37 checks |
| `RJdkStrict` | PASS, 359 checks |
| `RJdkReflBox` | PASS, 107 checks |
| `RJdkHandles` | PASS, 331 checks (40 steps) |
| `RJdkReflect` | PASS, 67 checks |
| `RReflect` | PASS, 40 checks |
| `RArrayStoreInterfaces` | PASS, 108 checks |
| `RForeignLayoutJdkInterfaces` | FAIL, `a heap segment's scope is stable` (pre-existing, unrelated) |
| `RJdkOptionalShape` | FAIL, `AbstractMethodError: java/net/http/HttpRequest.version()` (pre-existing, unrelated) |

The last two are not in the assignment's list. They are here because
`grep -l 'Proxy\|InvocationHandler' regression-suite/src/*.java` returns
**exactly eight** files over the 101 vectors, and those eight are the entire
population that can reach the changed code. Six were already on the list; these
two were not, and both are already red for reasons in other subsystems. The
requirement for them is not "green" but "no earlier": their failure point must
not move.

`RForeignLayoutJdkInterfaces` is the most exposed of the ten and the reason to
have gone looking. It proxies nineteen JDK interfaces (`java.sql.Connection`,
`MemoryLayout`, …) with one handler that answers by `m.getReturnType()`:
`Boolean`/`Integer`/`Long`/`Double`/`Float`/`Short`/`Byte`/`Character` for the
eight primitives — via javac autoboxing off correctly-typed constants, so the
wrapper is exact in every case — a `String` for a `String` return, and `null`
for every other reference return. Every one of those is accepted by §4.1 and
none is accepted by accident: the primitive rows pass the exact-wrapper test,
the `String` rows pass the by-name walk, the `null` rows are the legal
reference-null arm. It is also, for the same reason, the vector that would
report first if this VM's autoboxing ever produced the wrong wrapper.

### 5.2 The six green ones, one at a time

* **`RJdkReflBox` (107), `RJdkReflect` (67), `RReflect` (40)** — reflection and
  reflective boxing. None constructs a `Proxy`; `is_proxy_dispatch` is false on
  every call they make, so they never enter the block. `RJdkReflBox` matters
  anyway as the *canary for the other direction*: it pins which wrapper this VM
  boxes each primitive into, and the strict arm's exact-wrapper test is only
  correct if that boxing is. If CratonVM ever boxed a `char` as `Integer`, this
  change would turn that into a thrown CCE — so `RJdkReflBox` staying green is
  a precondition of the fix, not merely an absence of regression.
* **`RArrayStoreInterfaces` (108)** — `aastore` covariance. It shares
  `synthetic_implements_public` and `annotation_proxy_satisfies_target` with
  this lane, but only as *readers*: both are called here, neither is modified,
  and the direction of use is the same (admit-only, after a by-name walk has
  declined). No proxies, so the block is never entered.
* **`RJdkHandles` (331, 40 steps)** — `java.lang.invoke`. `MethodHandle`
  invocation does not route through `Proxy$Instance` dispatch; the one place
  the two families meet is `MethodHandleProxies.asInterfaceInstance`, which
  `RJdkProxyIface` exercises and this vector does not.
* **`RJdkStrict` (359)** — annotations, and the single largest exposure. Real
  annotations under `CRATONVM_REAL_ANNOTATIONS` **are** `$ProxyN` instances, so
  they *do* enter the block and *do* reach
  `proxy_invoke_handler_shared`. They are safe because that function's
  AnnotationProxy arm returns at its top, before any of the three points the
  strict coercion was wired into — annotation member data keeps the lenient
  unbox verbatim. Had the fix gone where G18-1's N1 pointed
  (`proxy_unbox_primitive_return`, or the coercion in `invoke.rs`), every
  annotation member would have taken the strict path and a member whose stored
  wrapper this VM records with the wrong type — or not at all — would have
  become a thrown exception instead of a wrong value. That is the single
  decision in this lane that most changes its risk, and it is the reason the
  fix is *not* in the file the nomination named.
* **`RJdkProxyIface` (37/38)** — already red, and red for a reason outside this
  lane (§7.1). It is the one vector that both constructs proxies **and** drives
  them through `MethodHandleProxies`, so it is the most likely place for a
  spurious refusal to appear; its post-change state is reported in §8 whatever
  it is.

### 5.3 The three residual risks, named rather than argued away

1. **A handler that legitimately returns `null` for a primitive return, where
   HotSpot's `$ProxyN` would too.** There is no such case — HotSpot NPEs — but
   there *is* a CratonVM-internal case: a handler body the VM stubs, returning
   `Ok(Some(Object(None)))` where real bytecode would have returned a wrapper.
   That now throws instead of returning `0`. It is the intended behaviour and
   also the most plausible new-failure shape.
2. **An incomplete `interfaces` list on a class the store did parse.** The
   by-name walk is only as complete as the class store. Under `--jdk-only` the
   bytes are real, which is precisely the mode where this is least likely; the
   admit-only heuristics in §4.2 are the hatch for the rest.
3. **A reference-return refusal on a JIT-compiled path.** The JIT cache-miss
   resolver at `vm_exec.rs:~17459` wraps the same shared body, so it inherits
   the coercion without a second copy — deliberate, but it means a refusal can
   now originate inside a JIT-serviced dispatch. The `RuntimeError` is minted
   the same way and takes the same funnel.

---

## 6. What this lane did NOT do

* **Arrays are not checked, in either position.** Three measured rows stay red
  (`String[]` -> `int[]`, `int[]` -> `Object[]`, `Integer` -> `int[]`). An
  array's `class_id_of` is its *component* class, so the cheap check would be
  confidently wrong rather than merely absent, and no vector asserts these.
* **A lambda returned for a type it does not satisfy is not refused.** One
  measured row (lambda -> `String`). Its SAM interface is not on any
  `interfaces` list this predicate can walk, and inventing the refusal from the
  `$$Lambda` name is the `vn.contains("$Proxy")` failure mode G18-1 §4 already
  records once.
* **A proxy returned for a type it does not implement is not refused.**
  Symmetric to the above, and it is what keeps a real `$ProxyN` — whose
  recorded interface set may legitimately be empty — from being refused for its
  own interface.
* **Did not touch `proxy_wrap_undeclared_if_needed` or re-measure §3d.**
* **Did not touch `proxy_unbox_primitive_return`'s behaviour**, only its
  visibility.
* **Did not implement the `checkcast` for a proxy method's *parameters*.** Only
  the return value is coerced; the argument side was not measured and no row
  asked for it.

---

## 7. NOMINATIONS

### 7.1 `native-builtins/src/lang_invoke.rs:11491` — `asType` is a passthrough

G18-1 §6.2 could not name the live body for `RJdkProxyIface`'s single missing
check and recorded `MethodHandle.asType` as having `invocations=0`. **That
measurement was workload-dependent and the workload was the wrong one.**
Re-dumped with `--dump-native-registry` and `RJdkProxyIface` as the workload:

```
class                        name    registered_by                          owns_slot  invocations
java/lang/invoke/MethodHandle asType  native-builtins/src/lang_invoke.rs:11491  true      ** 24 **
```

`owns_slot=true` **and** `invocations=24`: it is the live body. No
`MethodHandleProxies` native is registered under this workload either, which is
consistent with G18-1 — but that only ever ruled out one of the two candidates.

The body is a deliberate passthrough. It returns `args[0]` after writing the
supplied `MethodType` into the receiver's `type` field, with one guard for
Panama downcall handles and **no convertibility check at all**:

```rust
if let (Some(Value::Object(Some(this))), Some(Value::Object(Some(mt)))) = (…) {
    ctx.set_field_by_name(*this, "type", Value::Object(Some(*mt)));
}
Ok(Some(args[0]))
```

So `asType` accepts every conversion, which is exactly the divergence:

| | |
|---|---|
| call | `MethodHandleProxies.asInterfaceInstance(Subtractor.class, mh)`, `mh` = `(String)String`, SAM = `(int,int)int` |
| HotSpot | `WrongMethodTypeException: cannot convert MethodHandle(String)String to (int,int)int` |
| CratonVM | NO-THROW |

**Not this lane's file.** The arithmetic in G18-1 §6.2 pinning it to
sub-check 2 of `refusals` still holds. Whoever takes it should note that the
comment above the registration ("invoke()/invokeExact handle the actual
argument coercions") is the design this refusal has to be added *beside*
without turning every legal adaptation into a throw — `invocations=24` on a
single vector run is how often it fires.

### 7.2 `vm/src/runtime/interpreter/typecheck.rs` — a reusable "does this object
satisfy this type name" entry point

`aastore_element_assignable` is the hardened predicate for this question, with
every measured hatch on it, and it is unreachable from anywhere that does not
have an *array reference* to derive a component descriptor from. This lane
needed the same relation for a return descriptor and had to assemble a narrower
copy out of `is_assignable_to_name` plus two of the same admit heuristics. The
JIT's `jit_typecheck_resolve` in `vm/src/jit/helpers.rs` is a third assembly of
the same parts. Extracting the object-vs-name core of
`aastore_element_assignable` — everything below its array arms — would let all
three share one predicate and one set of hatches. Not attempted here: it is
another lane's file and the extraction is only safe with the array-family
vectors in hand.

### 7.3 `native-builtins/src/reflect_annotations.rs` — `Proxy$Dispatch.invokeProxy`
still reads as the contract it is not

Unchanged from G18-1 §1, restated because this lane just spent its first hour
on it: `invokeProxy` has the wrapping, the boxing and the `Method` threading,
its doc comment says it is dead, and `invocations=0` confirms it. It is now
also *wrong* — it does not implement §2 or §3, while the live path does. A
reader who finds it first will conclude the VM has no return contract at all.
Either delete it or point its doc comment at
`vm_exec.rs::proxy_coerce_handler_return`.

---

## 8. What could not be settled

* **The after-state — and *why* it could not be measured, which is not the
  usual reason.** The lane was forbidden `cargo build`, but the binary DID turn
  over while it ran: `2026-08-17 00:44:49` -> `2026-08-17 01:41`. The new
  binary does not contain this change. `head -1 target-fcheck/release/cratonvm.d`
  says why — the build's source root is **`C:\craton\cvm-mergecheck`**, an
  orchestrator-owned merge tree, not the lane's `C:\craton\CratonVM1`. A lane
  that edits its own repo and then measures the shared binary is measuring
  somebody else's tree, and this is the shape that would let a lane report a
  fix as landed when nothing of it had been compiled.

  Two things were checked so the merge is a formality rather than a question:

  * `C:\craton\cvm-mergecheck\vm\src\vm\vm_exec.rs` and
    `…\runtime\interpreter\invoke.rs` are **byte-identical to this lane's
    `HEAD`** — no other lane has touched either file, so this patch applies
    clean;
  * the 01:41 binary DOES contain other lanes' work
    (`RForeignLayoutJdkInterfaces` moved from red to **PASS, 172 checks** across
    it, with nothing from this lane in between), so merges into that tree are
    happening and this one simply had not landed yet. G18-1's own after-state,
    taken on the same 01:41 binary, independently reports `RJdkProxy`
    **unchanged at 32 of 37** — which is the same statement from the other
    side: that binary contains G18-1's refusal fixes and none of this record's.

  To finish this record: merge, rebuild, re-run `g24/PRet.java` on both VMs and
  diff with `tr -d '\r\000'` (the probe prints a `char`, so a plain
  `tr -d '\r'` leaves a NUL that makes `grep` call the stream binary), and
  re-run the ten vectors in §5.1. §2 and §3's HotSpot columns are the expected
  `PRet` output except for the six rows §6 leaves open; §5.1's table is the
  expected vector state with `RJdkProxy` at 37.
* **Whether `RJdkProxy` reaches 37 or stops at a later check.** Checks 32 and
  33 are addressed by construction — 32 by the primitive-null arm on the lambda
  handler path, 33 by the reference-return arm — but 34 through 37
  (`Proxy` over a non-interface, then `reflectionOverProxy`'s three) were never
  reached on this binary and their state is unmeasured. `reflectionOverProxy`
  drives `Method.invoke` *on* a proxy, which enters the changed code with a
  `String` return and an `IOException` throw; both should pass, and neither was
  observed.
* **Whether any of the three residual risks in §5.3 fires.** They are named
  because they are the shapes to look for first if a vector outside the eight
  goes red, not because any was observed.
* **The argument side of the contract.** HotSpot's `$ProxyN` body also boxes
  arguments per the formal descriptor; `proxy_box_value_for_desc` does that and
  was not measured against the oracle here.
