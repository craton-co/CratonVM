# Lane 2 — `java.lang` values, `System`, `java.math`

**Scope: 434 §1.4 shadows over 68 classes, from 303 registration sites.**
Prefixes: `java/lang/` and `java/math/`, **minus** the prefixes L0, L3, L5 and
L7 own (`Class*`, `Module*`, `reflect/`, `invoke/`, `Thread*`, `ClassLoader*`).

Read [`lane-0-integration-and-gates.md`](lane-0-integration-and-gates.md) §2-§6 first. Method, preconditions and
landing protocol: [`../jdk-only-lane-operations.md`](../../contributing/jdk-only-lane-operations.md).

---

> **Lane T closed 2026-09-10.** Its throwable-family rows are RETIRED
> (`RETIRED_SHADOW_LT_TRIPLES`, 906 triples over 62 classes), so a triple this
> page defers to lane T is either already retired or classified as blocked —
> check the table before treating it as unowned. Record: [the lane T record](../../internal/jdk-only/lane-t-the-throwable-family-retired-and-the-two-defects-the-arm-had-to-find-first-20260910.md).

## 1. Shape of the lane

```text
  62  java/lang/StringBuilder          23  java/lang/System
  61  java/lang/AbstractStringBuilder  24  java/math/BigInteger
  28  java/lang/System$1               21  java/lang/AssertionError  <- lane T
```

The exception classes in your prefix (`AssertionError`, `Error`, `Exception`,
`IllegalArgumentException`, `IllegalStateException`, `IndexOutOfBoundsException`,
`InternalError`, each 16-21 rows) are produced by **lane T's** throwable
registrar. They are not yours. Removing them leaves roughly 250 rows that
genuinely belong to this lane, concentrated in three families.

## 2. First target: `java/lang/System$1`, 28 rows

Best value in the lane, and the reason is already measured. `System$1` is the
`JavaLangAccess` carrier, and the natural assumption — that the VM's shim
implements only a few of its methods, so yielding would trade an NPE for an
`AbstractMethodError` — **is false**:

```text
$ javap -p 'java.lang.System$1'
class java.lang.System$1 implements jdk.internal.access.JavaLangAccess {
  ...  88 members, all with bytecode
```

The VM's shim registers three methods (`currentCarrierThread`,
`currentThread0`, `layers`) because those must win in *compatible* mode. That
number is a fact about the mode, not about the class. Publishing the real
carrier alone took the `all` corpus arm from 5 passing to 24.

So these 28 rows are shadows over a class with full bytecode. **Read the image
with `javap -p` before pricing any of them as "we would have to implement N
methods".**

One version caveat: the carrier is `System$1` **only on JDK 25**. On JDK 21
`System$1` is a `PrivilegedAction` and `System$2` is the `JavaLangAccess`. Any
test that names the class is version-pinned; key it on the JDK under test.

## 3. `java/lang/System` (23 rows) — the property store must be inverted first

This is the lane's hard item and it is **blocked by design, not by effort**.

The property natives cannot be retired while the Rust property store is the
*authority*. Today `system_property_read` reads the real map first, then the VM
store, then a fallback; `setProperty` and `clearProperty` mirror into the real
map; `getProperties` fills the real map via `replace_real_map`. That was enough
to retire `java/util/Properties`, but it is not enough here:

> Retiring `System.getProperty` requires the Rust store to become a **cache of
> the object** rather than the object's source of truth. Until then, a
> Rust-side `set_system_property` stays masked until the next `getProperties()`
> — a residual that is documented, tolerable while the native wins, and a
> correctness bug the moment bytecode serves the read.

Three static fields in this neighbourhood are already published and must not
regress; each is absent-or-complete on purpose:

- `java/lang/System.props` — only stamped when `replace_real_map` succeeds.
- `SharedSecrets.javaLangAccess` and `.javaLangReflectAccess`.
- `jdk/internal/misc/VM.savedProps` — a real `java/util/HashMap`, because
  `VM.getSavedProperty` **throws** `IllegalStateException("Not yet
  initialized")` when the map is absent rather than answering null. A partially
  filled map would stop throwing and start answering null, which is the
  2026-07-14 `InternalError: null property: java.home` regression.

**Publishing order is load-bearing.** `SharedSecrets` must be published *before*
the `initPhase1` body, not after: the body installs charsets, and
`sun.nio.cs.UTF_8.<clinit>` captures the accessor on the way past. Publishing
afterwards cleared `ConstantUtils.JLA` but left `UTF_8.JLA` null — and the
cleared half hid the failure. A published static must beat the `<clinit>` that
copies it.

## 4. `StringBuilder` + `AbstractStringBuilder`, 123 rows

The largest single family and the most mechanical. They are one unit: retire
them together, because `StringBuilder` inherits most of its surface from
`AbstractStringBuilder` (bucket B) and a split wave leaves the two disagreeing
about the same buffer.

What you gain is worth naming: **the retirement surface here is the JDK's
argument-validation layer.** Real `AbstractStringBuilder` does the index and
capacity checks that a hand-written native tends to approximate. Probe the
*failure* cases — negative index, `start > end`, capacity overflow, null
`CharSequence`, surrogate pairs at a boundary — not just the happy path, and
print the exception message, since that is the observable that differs.

## 5. `java/math/BigInteger`, 24 rows

Self-contained, pure value semantics, no VM-filled state. A good early wave to
build confidence. Probe the shapes where implementations diverge: zero and
negative zero, `signum` on zero, radix boundaries in
`toString(int)`/`BigInteger(String,int)`, `divideAndRemainder` sign rules,
`modPow` with a negative exponent, and `bitLength`/`bitCount` on negatives.

## 6. Traps

- **`java/lang/Throwable` itself** is yours, but its 61 *subclasses* are lane
  T's. Coordinate before touching `fillInStackTrace`, `getStackTrace` or
  `setStackTrace`: the two frame-walk APIs in this VM order results oppositely
  (`capture_stack_trace` outermost-first, `frame_class_ids` innermost-first).
- **A comment saying the VM lacks a capability outlives the day it was built.**
  Several registrations in this file justify themselves with a limitation that
  may no longer hold. Verify against the current tree, not the comment.
- **`System.out`/`System.err` are how every other lane reads its probes.** If a
  change here can affect `PrintStream` behaviour (L4's class, but reached
  through your fields), say so in the commit — a broken stream reads as every
  lane's probes failing at once.

## 7. The increment loop

1. Funnel from a dump: owns slot, kind `Bridge`, image `Code`, and
   `invocations > 0` in **your** instrument's run.
2. Probe + HotSpot oracle. No build needed.
3. Fill `RETIRED_SHADOW_L2_TRIPLES`, sorted and unique.
4. Build token (L0 §5); one build per wave.
5. `N refusals, 0 survivors` from the survivor check.
6. Probe-tree A/B, `--jdk-only` corpus, `SUITE=all` at `TIMEOUT=600`, `all`-arm
   count.
7. Full gate set. Kind-map rows. Commit. Do not push.

## 8. Done

Every bucket-A/B row in the prefix set is retired, classified, reviewed as an
`Intrinsic` with its probe, or blocked with the blocker named — and the
`System` property inversion is either done or recorded as this lane's one
outstanding structural dependency.
