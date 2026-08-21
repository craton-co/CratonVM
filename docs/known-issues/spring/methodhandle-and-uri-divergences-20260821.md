# Nine measured `MethodHandle` / `java.net.URI` divergences left open on 2026-08-21

## Status
**OPEN, all nine MEASURED against HotSpot 25.0.3+9, none currently failing a
suite class.** Four groups: the inexact `invoke` door (C03/C07/C09), two
places CratonVM is more forgiving than HotSpot about argument shape (B06, I06),
`asType`'s impurity (I13/I14), and two hierarchical `URI.resolve` behaviours
(R13/R15). Found while fixing the two 2026-08-21 Spring FAILs (the retired
`two-new-fails-20260821-fullsuite-rerun` write-up) — each is a row of a probe
that ships with the fix, so re-checking any of them is one command, not a
re-derivation. Left open deliberately; the reason is given per row.

```bash
javac -d /tmp/cls probes/MhVarargsNullProbe.java probes/OpaqueUriProbe.java \
                  probes/MhIdentityProbe.java
java -cp /tmp/cls MhVarargsNullProbe > /tmp/hs.txt
<cratonvm-bin> --java-home $JDK25 -cp /tmp/cls MhVarargsNullProbe 2>/dev/null > /tmp/cvm.txt
diff /tmp/hs.txt /tmp/cvm.txt
```

`2>/dev/null` is load-bearing: CratonVM's tracing goes to stderr, and `2>&1`
puts it in the diff.

---

## A. The inexact `invoke` door cannot see its call-site type

`MhVarargsNullProbe` rows **C03**, **C07**, **C09**.

```
C03 vf.invoke((Object)null)      HotSpot [null]     CratonVM null
C07 mx.invoke(a,(Object)null)    HotSpot a|[null]   CratonVM a|null
C09 ov.invoke((Object)null)      HotSpot [null]     CratonVM null
```

A `null` in a varargs collector's trailing array slot is ambiguous — the array
itself, or one element to wrap — and the JDK decides from the CALL SITE's
static parameter type (`AsVarargsCollector.asType` takes the passthrough
shortcut only when that type is assignable to the array type). The
2026-08-21 fix supplies that distinction for `invokeWithArguments`, whose type
is always `genericMethodType(n)`, and leaves `invokeExact`, whose type is always
the handle's own. `invoke` is the door where it genuinely varies:

```java
mh.invoke((String[]) null)   ->  passthrough  ->  "null"     (row C02, correct today)
mh.invoke((Object)   null)   ->  collect      ->  "[null]"   (row C03, wrong today)
```

**Why it is open.** The two rows cannot both be right without the call-site
descriptor, and there is no channel for it. `vm_exec.rs`'s
signature-polymorphic block HAS it — it is the `descriptor` local it already
passes to `unbox_poly_return_checked` — but a `NativeMethodRegistry` callback
receives only `&[Value]`. Supplying it means a new thread-local in
`cratonvm-native-api` (the crate both `cratonvm-vm` and
`cratonvm-native-builtins` can see), armed at the three `safe_native_call` sites
in that block and taken by the `invoke` registration.

**One thing that makes this smaller than it looks:** the JIT's
`resolve_signature_polymorphic_native_site` (`vm/src/jit/helpers.rs`) admits
**VarHandle receivers only**, so `MethodHandle.invoke` has exactly one dispatch
door to instrument. A `take`-shaped read also makes a missed arming degrade to
today's behaviour rather than to a wrong one.

**What NOT to do:** simply make the `invoke` door collect nulls like the generic
one. That fixes C03/C07/C09 and breaks C02. Note that CratonVM already treats a
NON-null Object-typed argument as generic on this door (`scalar_needs_wrap` in
`collect_trailing_varargs` wraps it), so the VM is currently inconsistent
between null and non-null here — which is an argument for the plumbing, not for
guessing.

---

## B. CratonVM is more forgiving than HotSpot about argument SHAPE

`MhVarargsNullProbe` row **B06**, `MhIdentityProbe` row **I06**.

```
B06 vf.iwa(new String[0])
      HotSpot   THREW ClassCastException: Cannot cast [Ljava.lang.String; to java.lang.String
      CratonVM  []
I06 b.iwa(x,y)                         (b = vf.asFixedArity(), NOT a collector)
      HotSpot   THREW WrongMethodTypeException: cannot convert MethodHandle(String[])String
                to (Object,Object)Object
      CratonVM  [x, y]
```

HotSpot collects **unconditionally** on the generic entry, so an already-packed
array becomes the single ELEMENT of a new one and the cast throws; and a
fixed-arity handle refuses an over-long argument list outright. CratonVM keeps
its packed-call passthrough and gathers by arity in both cases.

**Why it is open.** Both divergences are in the forgiving direction. Matching
HotSpot would turn programs that work today into ones that throw, and nothing
in tree or in any suite asks for the throw. The `null` half of the same rule was
fixed because it goes the other way: it turns a working program into one that
computes a WRONG ANSWER, silently.

Spring itself demonstrates why the packed case rarely bites — `FunctionReference
.executeFunctionViaMethodHandle` unpacks an `Object[]` argument before invoking,
with the comment *"MethodHandle.invokeWithArguments(Object...) does not expect
varargs to be packaged in an array"*. Code that would hit B06 is already broken
on HotSpot.

---

## C. `MethodHandle.asType` still adapts the receiver in place

`MhIdentityProbe` rows **I13**, **I14**.

```
I13 e.type AFTER asType    HotSpot (String[])String    CratonVM (Object)Object
I14 e==f                   HotSpot false               CratonVM true
```

`asType` returns a NEW handle on every JDK. CratonVM stamps the new type onto
the receiver and returns it, so `e.asType(...)` mutates `e` for every other
holder — the same class of bug as the `asFixedArity`/`asVarargsCollector`
impurity that WAS fixed on 2026-08-21 (`mh_clone_handle`).

**Why it is open.** `asType` is on every hot MethodHandle path, and several
shims are written against the in-place stamp rather than merely tolerating it —
`MethodHandles.explicitCastArguments` does the identical `set_field_by_name(t,
"type", …)`, and `mh_astype_refusal`'s own doc records that "this body's own
type mutation makes a second `asType` on the same reference look unconvertible".
Converting it is its own change with its own measurement, and it is not a
residual of either 2026-08-21 failure.

The machinery is now in place, though: `mh_clone_handle` is exactly the helper
such a change would use.

`MhIdentityProbe` also carries the controls that already pass — I05 and
I17-I26: `asCollector`, `asSpreader` and `dropArguments` DO mint new handles.

---

## D. Two hierarchical `URI.resolve` behaviours

`OpaqueUriProbe` rows **R13**, **R15**.

```
R13 URI.create("https://h/a/b?q=1").resolve("")
      HotSpot   https://h/a/            CratonVM  https://h/a/b?q=1
R15 URI.create("https://h/a/b").resolve("https://x/p/../q")
      HotSpot   https://x/p/../q        CratonVM  https://x/q
```

* **R13.** The JDK has no "empty reference returns the base" shortcut. An empty
  child path goes through RFC 3986 §5.2(6a) — `URI.resolvePath` keeps the base's
  DIRECTORY, `base.substring(0, base.lastIndexOf('/') + 1)` — and §5.2's
  `ru.query = child.query` then drops the base's query. So `/a/b?q=1` resolves
  to `/a/`, not to itself. `uri_resolve_ref` opens with
  `if reference.is_empty() { return Ok(base.to_string()); }`.

* **R15.** A reference that carries its own scheme is returned by the JDK as the
  argument OBJECT (`if (child.scheme != null) return child;`), so it is never
  normalized. `uri_resolve_ref` rebuilds it through `uri_remove_dot_segments`
  and loses the `p/../q`.

**Why they are open.** Both are outside the opaque family the 2026-08-21 change
fixed, no suite class asks for either, and `URI.resolve` sits under class
loading, `URL` handling and every Spring resource path — the blast radius of
changing what `resolve("")` returns is much wider than what the measurement
justifies. R15 is the smaller and safer of the two: it only REMOVES a
normalization the JDK does not do.
