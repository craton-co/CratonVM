# F20-1 — the three unguarded rescales, the scale that negates into a panic, and two twins that shadow their own fix

**Status: PARTIAL. 2026-08-13, lane F20.** The only source file this lane
edited is `native-builtins/src/lib.rs` (plus this record). **This lane did not
build or run CratonVM**: every CratonVM "after" below is explicitly
**PREDICTED**, and every "before" is a fact read out of the tree. Every HotSpot
value is **MEASURED** on this host against `openjdk 25.0.3 2026-04-21 LTS
(25.0.3+9-LTS)` (Microsoft build), from `scratchpad/f20/{Bd,Pow10,Plain,Ind,ArrGet,Slog}.java`,
before any of it was written down.

---

## 1. Verdict

| | |
|---|---|
| brief's task 1 — `bigint_mul_pow10`'s three unguarded call sites | **all three are in `math_bignum.rs`, not this lane's file** → §3, NOMINATION N1 |
| what landed in `lib.rs` for task 1 | the `n`-byte `String` + O(n²) decimal parse is **gone**; the refusal boundary is measured and written at the site |
| a **fourth** site of the same shape, found by grepping `(-x) as usize` | `apply_scale` — `-i32::MIN` was a **guaranteed VM abort**, now total (§4) |
| brief's task 2 — say why `native_array_get` allocates | done, and the measurement **re-taken independently** rather than inherited (§5) |
| task 3 pick A — `String.indent` | a `register_synthetic_overrides` twin **shadowed W7-95a's landed fix**: wrong answer + VM abort, live in synthetic-JDK mode (§6) |
| task 3 pick B — `System.Logger.isLoggable(null)` | fabricated permission: answered `true`, HotSpot **NPEs**. Fixed across all 10 entry points (§7) |
| a HotSpot claim in a live in-tree test that this lane **falsified** | E40-1's "unconditional `true`" and `vm/src/vm/tests.rs`'s "the severity comparison this VM has nowhere to do" — both over-broad (§7.3) |
| a divergence found and deliberately **NOT** changed | `isLoggable(OFF)` — the two JDK implementations disagree with each other (§7.4) |
| NOMINATIONS raised | **4** |

---

## 2. The boundary, measured before any guard was written

The brief's instruction was to measure HotSpot at the boundary rather than
invent a cap. `BigDecimal`'s `10^n` factor is `bigTenToThe(n)`, which past its
table is `BigInteger.TEN.pow(n)`, so the refusal is `BigInteger.pow`'s own.
`scratchpad/f20/Pow10.java`, `-Xmx48m`:

```text
TEN.pow(715827881) !! java.lang.OutOfMemoryError: Java heap space                     [61916 ms]
TEN.pow(715827882) !! java.lang.OutOfMemoryError: Java heap space                     [73069 ms]
TEN.pow(715827883) !! ArithmeticException: BigInteger would overflow supported range  [    1 ms]
TEN.pow(715827884) !! ArithmeticException: BigInteger would overflow supported range  [    0 ms]
```

**The refusal is at `n >= 715_827_883` and not one below it.** 715827882 really
tries — 73 seconds of work before the heap ran out — so a cap anywhere under
715827883 refuses where HotSpot succeeds. `math_bignum::bd_pow_ten_check`'s
existing predicate is exactly this: for base 10, `getLowestSetBit()` is 1 and
`bitLength(5)` is 3, so `bi_pow_check_range` reduces to `3n >= Integer.MAX_VALUE`,
i.e. `n >= 715827883`. **The existing helper is right; nothing needed inventing.**
(715827883 is also the modulus in F7's own open residual
`1.5.divide(2, 715827883, HALF_UP)`, which is not a coincidence — that is the
same predicate seen from the `divide` side.)

The per-operation boundaries, `scratchpad/f20/Bd.java`:

| call | HotSpot 25.0.3+9 | ms |
|---|---|---|
| `new BigDecimal(ONE, MAX).add(ONE)` | `ArithmeticException: BigInteger would overflow supported range` | 0 |
| `new BigDecimal(ONE, MIN).add(new BigDecimal(ONE, MAX))` | `ArithmeticException: Underflow` | 1 |
| `new BigDecimal(ONE, MIN).subtract(new BigDecimal(ONE, MAX))` | `ArithmeticException: Underflow` | 3 |
| `new BigDecimal(ONE, MIN).toBigInteger()` | `ArithmeticException: Underflow` | 2 |
| `new BigDecimal(ONE, MIN+1).toBigInteger()` | `ArithmeticException: BigInteger would overflow supported range` | 0 |
| **`new BigDecimal(ONE, MIN).intValue()`** | **`0`** | 0 |
| **`new BigDecimal(ONE, MIN).longValue()`** | **`0`** | 1 |
| **`new BigDecimal(ONE, MAX).intValue()`** | **`0`** | 0 |
| `new BigDecimal(ONE, MAX).add(new BigDecimal(ONE, MAX))` | `2E-2147483647` | 0 |
| `new BigDecimal(ZERO, MIN).add(ONE)` | `1` | 0 |
| `new BigDecimal(ONE, -1000000).toBigInteger()` | 1000001-digit answer | 3738 |

**The three bold rows are the trap.** `intValue`/`longValue` do **not** throw —
they answer `0` in under a millisecond, because `BigDecimal.longValue()`
(`BigDecimal.java:3553-3571`) fast-paths `signum()==0 || fractionOnly() || scale <= -64`
*before* reaching `toBigInteger()`. A guard that made `bd_truncated_bigint`
throw would be a fresh divergence on exactly the two callers that must not
throw. This is why N1 below is three different edits and not one.

---

## 3. Task 1 — `bigint_mul_pow10`

### 3.1 What was there

```rust
    let mut p = String::with_capacity(1 + n as usize);
    p.push('1');
    for _ in 0..n { p.push('0'); }
    bi.mul(&crate::bigint::BigInt::from_decimal(&p))
```

`n` is a difference of two caller-chosen `BigDecimal` scales. An `n`-byte
`String` — up to ~2 GB from one ordinary call — followed by
`BigInt::from_decimal`, which is a digit-at-a-time multiply-and-add, O(n²) in
digits.

### 3.2 What is there now — and why it is not a rewrite

```rust
    bi.mul(&bigint_pow5(n as u32).shl(n as u32))
```

`BigInteger.pow` factors `2^getLowestSetBit()` out of the base, exponentiates
the odd part by repeated squaring, and shifts the powers of two back at the
end. For base 10 that is `10 = 2·5`, so **`TEN.pow(n)` literally computes
`5^n << n`**. `bi_pow_check_range`'s own doc comment already leans on that
factorisation to bound the answer's bit length, so this is the JDK's algorithm
spelled out, not a substitution for it. `bigint_pow5` already existed in the
same file with one caller, so no second square-and-multiply loop was written.

No `n`-sized `String` survives. The remaining cost is the multiplication, which
is the cost of the answer.

### 3.3 What did NOT land, and why

`bigint_mul_pow10` returns `BigInt`, not `Result`, so **it cannot refuse**.
Making it fallible means editing its callers, and all five textual call sites
live in `native-builtins/src/math_bignum.rs`, which belongs to another live
lane this session. Landing the signature change alone would leave the tree
non-compiling for the orchestrator's build. So the guard is N1, the
obligation is now written at the function, and the file that must apply it
already owns the correct helper (`bd_pow_ten_check`) and one correct example of
its use (`bd_set_scale_impl`).

---

## 4. The fourth site, found by grepping the SHAPE — `apply_scale`

The brief asked for other argument-driven-allocation sites in this file. The
grep was **not** for `BigDecimal` or for `repeat`; it was for the *shape*
`(-x) as usize` — a negation of a signed argument fed to a length. Two hits in
`lib.rs`, and both were defects (the other is §6):

```rust
        format!("{}{}{}", sign, abs, "0".repeat((-scale) as usize))
```

`scale` comes straight off the object's `scale` field, which
`new BigDecimal(BigInteger, int)` lets a caller set to `Integer.MIN_VALUE`.
`-i32::MIN` **panics in a debug build** ("attempt to negate with overflow") and
in release wraps to `i32::MIN`, which then **sign-extends** through `as usize`
to 18446744071562067968 and is handed to `String::repeat`. Either way a Rust
panic, which is not a Java throwable: it takes the VM down.

The reachable caller is `math_bignum::bd_read` → `toPlainString()`.
`scratchpad/f20/Plain.java`:

```text
new BigDecimal(ZERO, MIN).toPlainString() = 0                                    [90 ms]
new BigDecimal(ONE,  MIN).toPlainString() !! ArithmeticException: Overflow       [ 0 ms]
new BigDecimal(ONE,  MAX).toPlainString() !! OutOfMemoryError: too large to fit in a String [0 ms]
```

Three things that reading alone would have got wrong:

* Row 1 says the existing `unscaled == "0" && scale < 0` short-circuit is the
  **contract**, not an optimisation — `toPlainString` tests `signum()==0`
  before it validates the scale.
* Row 2's message is **`"Overflow"`, not `"Underflow"`**. It comes from the
  *static* `checkScaleNonZero`, which always throws, and is passed
  `-(long) scale` = +2147483648, whose `(int)` cast is negative. `setScale`'s
  instance `checkScale` (which F7 ported, correctly, as `"Underflow"`) is a
  different function with a different sign convention and a zero-value
  exemption. Two guards, two messages; do not copy one onto the other.
* Row 3 is a **length screen**, not an attempted allocation: 0 ms with 3 GB
  available.

**Fixed here:** `scale.unsigned_abs()` replaces `-scale`, and the double copy
(`"0".repeat(..)` then `format!`) is replaced by one pre-sized `String`. The
VM abort is gone. **Not fixed here:** the refusal itself, for the same
signature reason as §3.3 — `apply_scale` returns `String`. See N2.

---

## 5. Task 2 — why `native_array_get` allocates

Comment-only, at `native-builtins/src/lib.rs::native_array_get`. The brief
handed this over as lane F11's finding; this lane **re-took the measurement**
rather than cite it, because the whole point of the comment is to be checkable.
`scratchpad/f20/ArrGet.java`:

```text
Array.get int[]      == Integer.valueOf(7)   : false
Array.get char[]     == Character.valueOf(a) : false
Array.get byte[]     == Byte.valueOf(3)      : false
Array.get short[]    == Short.valueOf(9)     : false
Array.get long[]     == Long.valueOf(5)      : false
Array.get boolean[]  == Boolean.TRUE         : false
Array.get float[]    == Float.valueOf(1f)    : false
Array.get double[]   == Double.valueOf(1d)   : false
Array.get int[] twice self-identity          : false
Array.get int[] equals Integer.valueOf(7)    : true
Integer.valueOf(Array.getInt(ia,0)) == Integer.valueOf(7) : true
Field.get int        == Integer.valueOf(7)   : true
Method.invoke ()I    == Integer.valueOf(7)   : true
Array.get boolean[]  == Boolean.FALSE        : false
```

Agrees with F11-1 row for row, and adds one F11 did not list: the `false`
element is not `Boolean.FALSE` either. Two rows carry the warning and are in
the comment: `Array.get` is **not self-identical**, so "canonical" is not
merely unspecified there but false; and every fresh row is still
`.equals`-equal, so no equality-shaped assertion — which is what nearly every
reflective check in this tree writes — can see a dedup that breaks it.

The JDK's reason is in the comment too: `Field.get`/`Method.invoke` run on
`MethodHandle` accessors whose boxing step is a handle to `X.valueOf`;
`java.lang.reflect.Array.get` is `Reflection::array_get`, which boxes with
`java_lang_boxing_object::create`, a function that allocates and never consults
a cache. The `Boolean` arm is called out separately, because `Boolean.TRUE`
identity is exactly what `native_boolean_value_of`'s own comment documents
Xerces relying on — a dedup of that one arm would be right for every other
boxing caller in this tree and wrong here.

**No behaviour change. The eight arms are byte-for-byte as they were.**

---

## 6. Task 3, pick A — `String.indent` was fixed, and then shadowed by its own file

Category: **wrong answer + VM abort**. This is W7-95a's `indent(-1)`-over-NBSP
row, which `INDEX.md`'s "Contradictions found and NOT resolved" §2 says is *not*
covered by the "no family aborts the VM" result. It is now located.

`java/lang/String.indent(I)Ljava/lang/String;` was registered **twice**:

| where | body | registrar | runs |
|---|---|---|---|
| `lib.rs:14679` → `lang_math::register_wrapper_natives` | `lang_string::native_string_indent` — the **fixed** W7-95a body | `register_essential_natives_with_shims` | first |
| `lib.rs:23330` (was) | an inline closure — the **pre-W7-95a** body | `register_synthetic_overrides` | **last** |

`register_builtins` is `register_essential_natives(registry);` then
`register_synthetic_overrides(registry);` and `register()` is last-write-wins,
so **in synthetic-JDK mode the un-fixed copy won** — silently reverting the fix
for exactly the mode whose gate is blocking. The closure's own comment had also
gone stale: it said "CratonVM's reflection layer does not enforce the flag" of a
sibling and described writing "the same slot" as a registration that has since
grown a real gate.

Measured, `scratchpad/f20/Ind.java`:

| call | HotSpot | old closure |
|---|---|---|
| `"abc".indent(-1)` | `"abc\n"` | `"bc\n"` — wrong answer |
| `"  abc".indent(-5)` | `"abc\n"` | `""` — wrong answer |
| `"  abc".indent(-1)` | `" abc\n"` | `" abc\n"` — right by accident |
| `(U+00A0)+"abc".indent(-1)` | the same string back | **VM ABORT** |
| `"abc".indent(Integer.MIN_VALUE)` | `"abc\n"` | **VM ABORT** (debug) |
| `"  abc".indent(Integer.MIN_VALUE)` | `"abc\n"` | **VM ABORT** (debug) |

Java's rule is `s.substring(Math.min(-n, s.indexOfNonWhitespace()))` — a count
of **characters**, over `Character.isWhitespace`, which is **false for U+00A0**
(measured: `Character.isWhitespace(U+00A0) = false`, and
`(U+00A0)+"abc".stripLeading()` returns the string unchanged). The closure counted
**bytes** and did `&line[1..]`, which lands inside U+00A0's two-byte UTF-8
encoding: `byte index 1 is not a char boundary` — a Rust panic where HotSpot
returns a string. The same abort fires for any leading non-ASCII character
(`"éx".indent(-1)`, `"中x".indent(-1)` — both measured green on HotSpot).
Row 5/6 is `-i32::MIN`; the JDK has an explicit `n == Integer.MIN_VALUE` arm
(`stripLeading()`, `String.java:4036`) for that exact reason.

**Fix:** the slot now registers `crate::lang_string::native_string_indent` —
the arm that already gets all six right. A re-point, not a re-implementation: a
third copy of one rule is how the first two drifted.

### 6.1 The sweep this came out of, and what else it found

The shadow was found mechanically, not by reading: every
`(class, method, descriptor)` registered inside `register_synthetic_overrides`
was matched against every registration elsewhere in the crate, keeping only the
pairs whose **bodies differ**. `String.indent` was the only closure-vs-named-fn
pair. Widening it to named-vs-named surfaced one more that is **not fixed
here**, because it is a capability question this lane could not settle without
running the VM:

`java/lang/reflect/AccessibleObject.setAccessible(Z)V` is registered **three**
times — `lib.rs:15341` (`lang_reflect::native_accessible_set_accessible`, which
raises `InaccessibleObjectException`), `lib.rs:16396`
(`native_set_accessible_write_override`, which calls
`lang_class::enforce_set_accessible_gate` and wins in real-JDK mode), and a
closure in `register_synthetic_overrides` **with no gate at all**, which wins in
synthetic mode. `Field.setAccessible` and `Method.setAccessible` have their own
synthetic-mode rows pointing at gated bodies, so the exposed receivers are the
base class and `Constructor` (which has an essential-mode row but no
synthetic-mode one). Adding the gate to synthetic mode is a behaviour widening
in the *refusing* direction on a mode whose blocking gate must stay at zero
fails, and this lane cannot run it. **Recorded, not changed** — N4.

---

## 7. Task 3, pick B — `System.Logger.isLoggable(null)` fabricated a permission

Category: **wrong capability**. Applies E40-1 §5 N3.

### 7.1 The oracle

`scratchpad/f20/Slog.java`:

```text
impl class = sun.util.logging.internal.LoggingProviderImpl$JULWrapper
isLoggable(ALL)     = false
isLoggable(TRACE)   = false
isLoggable(DEBUG)   = false
isLoggable(INFO)    = true
isLoggable(WARNING) = true
isLoggable(ERROR)   = true
isLoggable(OFF)     = true
isLoggable(null)   !! NullPointerException: Cannot invoke "java.util.logging.Level.intValue()" because "level" is null
log(null, "m")     !! NullPointerException: Cannot invoke "java.util.logging.Level.intValue()" because "level" is null
log(INFO, (String) null) = returned normally, printed `INFO: null`
```

### 7.2 The defect and the fix

A null `Level` fell through `system_logger_level_name` → `None` →
`system_logger_severity_of` → `.unwrap_or(SYSTEM_LOGGER_SEVERITY_INFO)`, and
`isLoggable(null)` answered **`true`**: the permissive answer, which is the one
a caller cannot see is wrong. The `log` family then published at INFO instead of
throwing.

Both implementations a JDK can hand out agree here, which is what made this
safe to fix without a run:

* `JULWrapper.isLoggable` is `julLogger.isLoggable(toJUL(level))`; `toJUL(null)`
  is null and `java.util.logging.Logger.isLoggable` calls `level.intValue()`.
* `SimpleConsoleLogger.isLoggable` is
  `isLoggable(PlatformLogger.toPlatformLevel(level))`; `toPlatformLevel(null)`
  returns null (`PlatformLogger.java:511`) and the next line is
  `level.ordinal()`.

Added `system_logger_require_level`, applied at **all ten** entry points: the
`isLoggable` native, `system_logger_emit` (7 call sites, now `?`), and — before
the `Supplier` is evaluated — both supplier `log` overloads. Three details that
are load-bearing:

1. **Order.** The NPE is raised before the `message == None` early-return.
   HotSpot dereferences the level inside `isLoggable`, which every `log`
   overload calls first, so `log(null, (String) null)` throws where this used to
   return silently.
2. **Null argument, not unreadable name.** `system_logger_level_arg_is_null`
   tests for an absent reference only. A minted or short receiver whose `Level`
   has no readable `name` is a shape problem to absorb, not a caller error to
   throw at; conflating them would turn a fabrication into a spurious throw.
3. **The message is `JULWrapper`'s**, because that is the road a stock JDK
   takes and therefore the string an oracle diff holds. The `SimpleConsoleLogger`
   road NPEs at a different expression. The TYPE is what both agree on and what
   any `catch` sees.

### 7.3 Two in-tree claims this lane falsified

* **E40-1 §1c / N3 call this "the unconditional `true`". It is not.**
  `system_logger_severity_of` carries the real severities and
  `system_logger_threshold` defaults to INFO, so `isLoggable(DEBUG)` on a real
  `Level` object already answers `false`, matching HotSpot. Only the **null**
  arm was wrong. The correction matters: the fix is a null contract, not a
  severity table.
* **`vm/src/vm/tests.rs:50974-50977` says fixing it "needs the severity
  comparison this VM has nowhere to do yet".** That comparison is
  `system_logger_is_loggable`'s last line and has been there all along. The
  test reads as unconditional only because it passes
  `&[Value::Object(None), Value::Object(None)]` — a null receiver *and* a null
  level, which is the one input that took the default.

### 7.4 One divergence measured and deliberately NOT changed

`isLoggable(OFF)` is **`true`** on the logger `System.getLogger` actually
returns, because JUL's rule is
`level.intValue() >= levelValue && levelValue != offValue` — the `OFF`
exclusion is on the **logger's** level, not the argument's, and
`Level.OFF.intValue()` is `Integer.MAX_VALUE`. `SimpleConsoleLogger.isLoggable`
(`SimpleConsoleLogger.java:127-131`) is
`level != PlatformLogger.Level.OFF && level.ordinal() >= effectiveLevel.ordinal()`
and answers **`false`**.

**The two JDK implementations disagree with each other**, and which one is the
oracle for a given CratonVM run depends on whether `java.logging` is resolved —
which this lane could not determine without running the VM, and which this
file's own strict fallback deliberately answers with a real
`SimpleConsoleLogger`. So the `OFF` arm is left as it is and the measurement is
written beside it. Changing it would be the guess the brief forbids. Both
facts are now in the doc comment on `system_logger_is_loggable`, whose previous
claim ("never true for `OFF`") stated one implementation's rule as the contract.

---

## 8. What should move, PREDICTED

Nothing in this section was executed.

* **`String.indent`** — this is the one item where a check that is currently
  *aborting* becomes *reachable*, and those are different claims. Any
  synthetic-JDK vector that calls `indent` with a negative count over non-ASCII
  text currently kills the VM; after this it returns. No suite row is known to
  do so, so the predicted effect is **no verdict change and one fewer abort
  road**, not a green row appearing.
* **`System.Logger`** — `vm/src/vm/tests.rs::system_logger_p67` **will fail**
  (N3). That is the intended, labelled consequence: the test is an explicit
  divergence pin, `.unwrap()`s the native's `Result`, and will now panic on the
  `Err`. Nothing in `regression-suite/src` was found asserting
  `System.Logger` null behaviour, so no suite row is predicted to move. The
  risk to watch is the opposite direction: any in-VM road that reaches these
  natives with a missing `args[1]` now throws where it used to log at INFO.
* **`bigint_mul_pow10` / `apply_scale`** — no answer changes. `10^n` by
  square-and-multiply is the same integer as `from_decimal("1" + "0"*n)`, and
  `scale.unsigned_abs()` equals `-scale` everywhere `-scale` did not overflow.
  What changes is a removed 2 GB `String` per rescale, a removed O(n²) parse,
  and one removed guaranteed VM abort. **No refusal was added**, so no row that
  currently succeeds can start failing.
* **`native_array_get`** — comment only. Zero rows.

---

## 9. NOMINATIONS

### N1 — `native-builtins/src/math_bignum.rs`: three unguarded rescales, three *different* edits

The helper (`bd_pow_ten_check`) and one correct example (`bd_set_scale_impl`)
are already in that file. §2's table is the authority for why these are not one
patch.

**(a) `native_bd_add`.** Measured: raise = `Integer.MAX_VALUE` throws
`ArithmeticException: BigInteger would overflow supported range`; a scale
difference beyond `int` throws `ArithmeticException: Underflow`.

*exact literal old text:*
```rust
    let s = sa.max(sb);
    let sum = bigint_mul_pow10(&ua, s - sa).add(&bigint_mul_pow10(&ub, s - sb));
```
*exact literal new text:*
```rust
    let s = sa.max(sb);
    // `s - sa` and `s - sb` are differences of two caller-chosen scales and
    // OVERFLOW `i32` (`new BigDecimal(ONE, MIN).add(new BigDecimal(ONE, MAX))`).
    // MEASURED on OpenJDK 25.0.3+9 (scratchpad/f20/Bd.java): that pair throws
    // `ArithmeticException: Underflow` in 1 ms, and a representable-but-huge
    // raise throws `BigInteger would overflow supported range` in 0 ms.
    let raise_a = i64::from(s) - i64::from(sa);
    let raise_b = i64::from(s) - i64::from(sb);
    if raise_a > i64::from(i32::MAX) || raise_b > i64::from(i32::MAX) {
        return Err(RuntimeError::ArithmeticException {
            message: "Underflow".to_string(),
        }
        .into());
    }
    bd_pow_ten_check(raise_a as i32)?;
    bd_pow_ten_check(raise_b as i32)?;
    let sum = bigint_mul_pow10(&ua, raise_a as i32).add(&bigint_mul_pow10(&ub, raise_b as i32));
```

**(b) `native_bd_subtract`.** Identical, with `sub` for `add` and `diff` for
`sum`:

*exact literal old text:*
```rust
    let s = sa.max(sb);
    let diff = bigint_mul_pow10(&ua, s - sa).sub(&bigint_mul_pow10(&ub, s - sb));
```
*exact literal new text:* as (a), with the final line
```rust
    let diff = bigint_mul_pow10(&ua, raise_a as i32).sub(&bigint_mul_pow10(&ub, raise_b as i32));
```

**(c) `bd_truncated_bigint` — DO NOT simply add a guard here.** This helper is
shared by `toBigInteger()`, `intValue()` and `longValue()`, and §2 measured
that the last two **must not throw**: `new BigDecimal(ONE, MIN).intValue()` is
`0` in 0 ms on HotSpot, while this helper's `bigint_mul_pow10(&u, -scale)`
negates `i32::MIN` (debug panic; release wrap → `n <= 0` → returns the unscaled
value → `intValue()` answers **1**, a wrong answer). The JDK's own split is
`BigDecimal.longValue()` (`BigDecimal.java:3553-3571`):

```java
if (this.signum() == 0 || fractionOnly() || scale <= -64) { return 0; }
else { return toBigInteger().longValue(); }
```

So (c) is two edits: give `bd_truncated_bigint` a `scale <= -64 → BigInt::zero()`
fast path plus `fractionOnly()` (`precision() <= scale`), which makes the
`intValue`/`longValue` callers total; and only then guard the remaining
`toBigInteger()` road with the `Underflow` / `bd_pow_ten_check` pair from (a),
since `toBigInteger()` is `setScale(0)` and inherits `checkScale`'s refusal.
Sizing and exact text are that file's lane's — the measured targets are §2's
table, and the `intValue`/`longValue` rows are the ones a naive guard breaks.

### N2 — `native-builtins/src/math_bignum.rs`: `bd_read` must be able to refuse

§4 removed the VM abort from `apply_scale` but not the refusal, because
`apply_scale` returns `String`. HotSpot: `new BigDecimal(ONE, MIN).toPlainString()`
throws `ArithmeticException: Overflow` — the **static** `checkScaleNonZero`,
message `"Overflow"`, distinct from `setScale`'s `"Underflow"` (§4). The edit is
to make `bd_read`'s real-JDK-layout arm raise that before calling
`apply_scale`, for `scale == i32::MIN` with a nonzero unscaled value:

*File:* `native-builtins/src/math_bignum.rs`, in `bd_read`
*exact literal old text:*
```rust
    if let Some((unscaled, scale)) = bd_read_parts(ctx, this) {
        return apply_scale(&unscaled, scale);
    }
```
This needs `bd_read` to become fallible, which its own callers force; that
ripple is why it is nominated rather than applied. If the lane taking it
prefers to keep `bd_read` infallible, the alternative is to move the check into
`native_bd_to_plain_string`, which already returns `MethodCallResult`.

### N3 — `vm/src/vm/tests.rs:50979-50996`: the divergence pin now pins a fixed defect

The test's own comment says "Change this only together with the fix", and this
is the fix. It calls `.unwrap()` on the native's `Result`, so it will **panic**,
not merely assert-fail.

*exact literal old text:*
```rust
        let loggable = call_native(
            &shared,
            &mut thread,
            "java/lang/System$Logger",
            "isLoggable",
            "(Ljava/lang/System$Logger$Level;)Z",
            &[Value::Object(None), Value::Object(None)],
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            loggable,
            Value::Int(1),
            "isLoggable is a fabricated unconditional `true`; HotSpot answers \
             false for ALL/TRACE/DEBUG on a default logger and NPEs on a null \
             level. Change this only together with the fix, not to match a \
             different fabrication"
        );
```
*exact literal new text:*
```rust
        // A null `Level` is an NPE on BOTH JDK implementations (F20-1 §7):
        // `JULWrapper` reaches `java.util.logging.Level.intValue()` and
        // `SimpleConsoleLogger` reaches `PlatformLogger.Level.ordinal()`.
        // MEASURED on 25.0.3+9: `isLoggable(null)` throws
        // `NullPointerException: Cannot invoke
        // "java.util.logging.Level.intValue()" because "level" is null`.
        // This asserts the REFUSAL, so it goes red if the fabricated `true`
        // ever comes back.
        let loggable = call_native(
            &shared,
            &mut thread,
            "java/lang/System$Logger",
            "isLoggable",
            "(Ljava/lang/System$Logger$Level;)Z",
            &[Value::Object(None), Value::Object(None)],
        );
        assert!(
            loggable.is_err(),
            "isLoggable(null) must raise NullPointerException, not answer a \
             fabricated `true`: HotSpot NPEs on a null level on both the \
             JULWrapper and the SimpleConsoleLogger road"
        );
```

The stale half of the comment above it — "This row answers `true` for every
level" and "the severity comparison this VM has nowhere to do yet" — should go
with it (§7.3): the severity comparison is `system_logger_is_loggable`'s last
line, and the row read as unconditional only because the call passes a null
receiver *and* a null level.

### N4 — `native-builtins/src/lib.rs` (this lane's own file), deliberately deferred: the ungated `setAccessible` in synthetic mode

§6.1. `AccessibleObject.setAccessible(Z)V`'s `register_synthetic_overrides`
closure writes the `override` field with **no** JEP 403 gate, and wins in
synthetic-JDK mode; both other registrations of the same triple call a gate.
The obvious edit is to re-point it at `native_set_accessible_write_override` the
way §6 re-pointed `indent`. It is **not** taken here because it is a widening in
the *refusing* direction into the mode whose gate is blocking at zero fails, and
this lane could not run the VM to see whether
`lang_class::enforce_set_accessible_gate` fails open on synthetic-mode receivers
that have no module. Whoever takes it should first answer that one question;
if the gate fails open without module data, the re-point is safe and mechanical.

---

## 10. Residuals

* **`log(INFO, (String) null)`** publishes `INFO: null` on HotSpot (measured)
  and emits nothing here. Recorded at the site in `system_logger_emit`, not
  changed: `message: Option<String>` is that helper's "nothing to say" channel
  for several callers, so the fix is a caller-side one and has no permission
  component.
* **`" ".repeat(n)` for a large positive `indent`** — HotSpot OOMEs (catchable);
  the shared body loops. Inherited from `lang_string::native_string_indent` by
  the re-point, unchanged on purpose: matching one body is the point.
* **Rust `str::lines()` vs Java `String.lines()`** — Rust splits on `\n`
  (stripping a trailing `\r`), Java also splits on a lone `\r`. Present in both
  `indent` bodies before and after; not this lane's change and not measured.
* **`apply_scale` at a large positive scale** still tries where HotSpot's length
  screen refuses in 0 ms (§4 row 3). Same signature blocker as N2.
* Nothing in this record claims a CratonVM behaviour was observed. §8 is written
  so that one run can falsify it.
