# F31-1 — three roads out of one scale, the zero operand that is exempt, and two words that must not be unified

**Status: LANDED (code), PREDICTED (behaviour). 2026-08-13, lane F31.** The only
source files this lane edited are `native-builtins/src/math_bignum.rs` and
`vm/src/vm/tests.rs` (plus this record). **This lane did not build or run
CratonVM**: every CratonVM "after" below is explicitly **PREDICTED**, and every
"before" is a fact read out of the working tree. Every HotSpot value is
**MEASURED** on this host against `openjdk 25.0.3 2026-04-21 LTS (25.0.3+9-LTS)`
(Microsoft build), from `scratchpad/f31/{Bd,Bd2,Bd3,Bd4,Bd5,Bd6,Bd7,Bi13,Mp}.java`, before any
guard was written.

Notation throughout: `bd(u,s)` is `new BigDecimal(BigInteger.valueOf(u), s)`,
`MIN`/`MAX` are `Integer.MIN_VALUE`/`Integer.MAX_VALUE`.

---

## 1. Verdict

| | |
|---|---|
| F20-1 N1 — `bigint_mul_pow10`'s three unguarded call sites | **all three closed**, as three genuinely different edits (§3, §4) |
| F20-1 N2 — `bd_read` must be able to refuse | **closed.** `bd_read` is `Result<String, MethodCallFailed>` with **one** caller; the other twelve take `bd_read_unchecked`, and §5.4 is why that is not a shortcut |
| F20-1 N3 — the `isLoggable` divergence pin | **closed**, and its `.unwrap()` panic with it (§6) |
| a claim in the brief this lane **falsified** | "`apply_scale` currently computes `-i32::MIN`, a guaranteed VM abort" — F20 had already landed `unsigned_abs` in `lib.rs`; the abort was gone, the *divergence* was not (§5.1) |
| a claim in F20-1 N1(a)'s proposed patch text this lane **falsified** | it would have refused **four** rows HotSpot answers, because it guards the *raise* and not the *raised operand* (§4.2) |
| a mistake this lane made and then caught by measuring | the first cut of N2 put a `?` on all 13 `bd_read` call sites. **Twelve of them are methods HotSpot never refuses** — that would have been 12 fresh divergences (§5.4) |
| task 4 — one more open record landing in these files | E40-1 **N6**, the `BigInteger` member: 13 `registry.find(…).is_some()` assertions replaced with a 30-row oracle diff (§8.1) |
| a boundary F20-1 got right and this lane re-measured rather than inherited | `TEN.pow` refuses at `n >= 715_827_883` and not below (§2) |
| an argument-driven allocation found by grepping the SHAPE, not on the brief | `bd_set_scale_impl`'s drop divisor — 4th copy of `"1" + "0"*n` → `from_decimal` (§7) |
| NOMINATIONS raised | **3**; two doc-only, one a live VM abort in a helper with call sites in two files this lane does not own. **None blocks compilation** (§9) |

---

## 2. One scale, three different JDK answers

The brief said `bd_truncated_bigint` "has a trap that inverts the obvious fix".
It does, and the trap is bigger than one method: **the same `(unscaled, scale)`
pair is refused by one JDK method, answered `0` by a second, and answered with a
2 GB string by a third.** All three used to go through this file's *one* helper.

Measured, `scratchpad/f31/Bd.java`:

| receiver | `toBigInteger()` | `intValue()` / `longValue()` | `toPlainString()` |
|---|---|---|---|
| `bd(1,MIN)` | `!! ArithmeticException: **Underflow**` [0 ms] | `0` [0 ms] | `!! ArithmeticException: **Overflow**` [0 ms] |
| `bd(1,MAX)` | `!! ArithmeticException: BigInteger would overflow supported range` | `0` [0 ms] | `!! OutOfMemoryError: too large to fit in a String` |
| `bd(0,MIN)` | `0` | `0` | `0` |
| `bd(0,MAX)` | `0` | `0` | `!! OutOfMemoryError: too large to fit in a String` |
| `bd(1,715827883)` | `!! …would overflow supported range` | `0` | `!! OutOfMemoryError: too large to fit in a String` |

Read the first row across. It is one receiver. It refuses with **`Underflow`**,
answers **`0`**, and refuses with **`Overflow`** — depending only on which
method you asked. Read the third and fourth rows down: a zero value is exempt
from the scale check on two roads and not on the third.

Why, from `BigDecimal.java`:

* **`toBigInteger()`** is `setScale(0, ROUND_DOWN).inflated()` (`:3515`).
  `setScale` returns early for `signum() == 0` (`:2885`), then calls the
  **instance** `checkScale` (`:4568`), which **clamps** an out-of-`int`
  difference to `Integer.MAX_VALUE` before choosing the word — so `asInt > 0`
  and the word is **`"Underflow"`**. Past that it builds `bigTenToThe(drop)`,
  i.e. `TEN.pow`, which is where the range refusal comes from — **on the
  positive side too**: `bd(1,MAX).toBigInteger()` refuses because `setScale(0)`
  needs `10^MAX` as a *divisor*.
* **`longValue()`** (`:3553`) never gets there for these rows:
  `if (this.signum() == 0 || fractionOnly() || scale <= -64) return 0;`.
  `intValue()` is `(int) longValue()`.
* **`toPlainString()`** (`:3425`) short-circuits `signum() == 0` **only on the
  negative-scale road**, then calls the **static** `checkScaleNonZero` (`:4674`),
  which **casts**: `(int) 2147483648L` is `Integer.MIN_VALUE`, which is
  negative, so the word is **`"Overflow"`**. Its size screen is
  `if (len < 0) throw new OutOfMemoryError("too large to fit in a String")`.

**Do not unify the two words.** They come from two helpers that differ by one
line — clamp versus cast — and a lane that "tidies" them will make exactly one
of §8's two tests go red.

### 2.1 The `TEN.pow` boundary, re-measured

F20-1 §2 measured it; this lane did not inherit it. `bd_pow_ten_check`'s
predicate (`3n >= Integer.MAX_VALUE`) is confirmed at the boundary through three
independent entry points:

```text
bd(1,-715827883).toBigInteger()  !! ArithmeticException: BigInteger would overflow supported range [0 ms]
bd(1,715827883).toBigInteger()   !! ArithmeticException: BigInteger would overflow supported range [0 ms]
bd(1,0).add(bd(1,715827883))     !! ArithmeticException: BigInteger would overflow supported range [0 ms]
bd(1,0).add(bd(1,715827882))      — HotSpot really tries; did not return inside the 400 s probe cap
```

The last row is the point: at `715827882` HotSpot **starts**, exactly as F20
found (`TEN.pow(715827882)` OOMEs after 73 s). Nothing under `715827883` may be
capped, or CratonVM refuses where HotSpot answers.

---

## 3. Task 1(c) — the split, and what it costs

`bd_truncated_bigint` is now three functions:

| function | fallible? | who calls it | what it may cost |
|---|---|---|---|
| `bd_truncate(u, scale)` | **no** | both roads | whatever the caller has already bounded |
| `bd_to_big_integer_check(u, scale)` | yes | `toBigInteger()` | — |
| `bd_narrowing_truncates_to_zero(u, scale)` | no (bool) | `intValue()`/`longValue()` | — |

`bd_truncate` is **total**: the negative arm is `u.mul(&bigint_pow10(scale.unsigned_abs()))`,
not `bigint_mul_pow10(&u, -scale)`. That matters even though both callers now
guard, because `-i32::MIN` is a debug panic and a release wrap, and a Rust panic
is not a Java throwable — it takes the VM down and no `catch` sees it. A
precondition comment is not a guard; the arithmetic is simply total now.

### 3.1 Why `intValue`/`longValue` need no refusal at all

After the three fast paths, the remaining work is bounded by the **operand**:

* `scale <= -64` is handled, so a surviving negative scale is in `[-63, -1]` and
  the factor is at most `10^63`.
* `scale > 0` survives only when `fractionOnly()` is false, i.e. `precision() > scale`,
  so `10^scale` is no bigger than the unscaled value itself.

`fractionOnly()` is `precision() <= scale`, and computing `precision()` exactly
costs the decimal conversion this lane is removing. The implementation uses an
**upper bound**, `bits/3 + 1 >= digits` (safe because `log10(2) < 1/3`). An upper
bound can only make the fast path fire *less* often than the JDK's, never more,
and when it does not fire the fall-through divides by `10^scale` with
`scale < bits/3 + 1` — same `0`, still operand-bounded. The test at §8 pins the
row where the two disagree:

```text
new BigDecimal(TEN.pow(30), 30).longValue() = 1     (precision 31 > scale 30 — both agree)
new BigDecimal(TEN.pow(30), 31).longValue() = 0     (JDK fast-paths; this divides, same answer)
```

### 3.2 What the old body actually answered

`bd(1,MIN).intValue()`: `bigint_mul_pow10(&u, -scale)` with `scale == i32::MIN`.
Debug: `-i32::MIN` panics — VM down. Release: wraps back to `i32::MIN`, which is
`<= 0`, so `bigint_mul_pow10` returns the operand **unscaled** and `intValue()`
answers **`1`**. HotSpot answers `0`. That is a wrong answer sitting under a
crash, and it is the row the brief predicted.

---

## 4. Task 1(a)/(b) — `add` / `subtract`

### 4.1 The `i32` subtraction

`BigDecimal.add`'s first line is `long sdiff = (long) scale1 - scale2;`
(`:5247`, and identically in the two other `add` overloads) — a `long`,
deliberately. This file computed `s - sa` in `i32`. `bd(1,MAX).add(bd(1,MIN))`
made that `MAX - MIN`: debug panic, release wrap to `-1`, and `-1` is
`bigint_mul_pow10`'s "nothing to do" arm — so the release build returned the
operand **unscaled**, silently. Both differences are now `i64`.

### 4.2 The zero-operand exemption — where F20-1 N1(a)'s text was wrong

F20-1 N1(a) proposed guarding the raise alone:

```rust
if raise_a > i64::from(i32::MAX) || raise_b > i64::from(i32::MAX) { …Underflow… }
bd_pow_ten_check(raise_a as i32)?;
bd_pow_ten_check(raise_b as i32)?;
```

Measured, that refuses **four rows HotSpot answers**:

```text
bd(0,MIN).add(bd(1,MAX))          = 1E-2147483647    [0 ms]      ← N1(a) text: Underflow
bd(0,0).add(bd(1,MAX))            = 1E-2147483647    [0 ms]      ← N1(a) text: Underflow
bd(1,MAX).add(bd(0,0))            = 1E-2147483647    [0 ms]      ← N1(a) text: Underflow
bd(0,0).add(bd(1,715827883))      = 1E-715827883    [57 ms]      ← N1(a) text: …overflow supported range
bd(0,0).subtract(bd(1,715827883)) = -1E-715827883    [0 ms]      ← N1(a) text: …overflow supported range
```

Because `checkScale` throws only `if (intCompact != 0)` /
`if (intVal.signum() != 0)` (`:4573`, `:4696`) and `longMultiplyPowerTen` returns
`val` unchanged when `val == 0` (`:4365`). **A zero operand is never scaled and
never refused, however large the raise.**

And it is the *raised* operand — the one with the smaller scale — whose
zeroness matters, not either operand's. The pair that settles it, differing in
nothing else:

```text
bd(1,MIN).add(bd(0,MAX))  !! ArithmeticException: Underflow   [0 ms]   (the raised operand is the 1)
bd(0,MIN).add(bd(1,MAX))   = 1E-2147483647                    [0 ms]   (the raised operand is the 0)
bd(0,0).add(bd(1,MIN))    !! ArithmeticException: Underflow   [0 ms]   (max scale is 0; the 1 is raised)
bd(0,715827883).add(bd(1,0)) !! …would overflow supported range [0 ms] (the 1 is raised)
```

So the guard lives in `bd_rescale_operand(u, raise)`, which takes the operand
*and* the raise, and orders the tests `raise <= 0` → `u.is_zero()` →
`raise > i32::MAX` → `bd_pow_ten_check`. Putting `is_zero` after the range test
would refuse row 1 above; putting it before is what HotSpot does.

This is the same class of error the brief warned about — a guessed cap where
HotSpot succeeds is a new divergence — arriving through a *nomination* rather
than through a guess.

---

## 5. Task 2 — `bd_read` is fallible

`bd_read` is now `Result<String, MethodCallFailed>` and calls
`bd_plain_string_check(&unscaled, scale)?` before `apply_scale`. It has
**exactly one caller**, `native_bd_to_plain_string`. The other twelve call sites
take a new `bd_read_unchecked`, which is the old body — see §5.4, which is the
part of this section that matters. Nothing outside this file moves and
**nothing blocks compilation elsewhere**.

### 5.1 The brief's premise was already stale

> "`apply_scale` currently computes `-i32::MIN`, a guaranteed VM abort"

It does not. F20 landed `scale.unsigned_abs()` in `lib.rs` and its doc comment
says so; the working tree carries it. What F20 left is the **divergence**, not
the abort: `bd(1,MIN).toPlainString()` returned a ~2 GB string of zeros where
HotSpot throws in 0 ms. That is what §5.2 closes. Acting on the brief's stated
premise would have produced a second `unsigned_abs`.

### 5.2 The three rules of the screen, each with a pair that isolates it

Guessing any one of these wrong produces a divergence, so each is pinned by two
rows that differ in one character:

| rule | rows |
|---|---|
| the `signum()==0` short-circuit is on the **negative-scale road only** | `bd(0,MIN)` = `0`, `bd(0,MAX)` `!! too large to fit in a String` |
| negative road: `len = str.length() + trailingZeros`, `str` is the **signed** rendering | `bd(1,-2147483646)` tries, `bd(12,-2147483646)` and `bd(-1,-2147483646)` refuse |
| positive road: `len = (signum < 0 ? 3 : 2) + scale`, with **no digits in it** | `bd(1,2147483645)` tries, `bd(-1,2147483645)` refuses |

### 5.3 Deliberately NOT ported

```text
bd(1,-2147483646).toPlainString()  !! OutOfMemoryError: Requested array size exceeds VM limit  [0 ms]
bd(1,2147483645).toPlainString()   !! OutOfMemoryError: Requested array size exceeds VM limit  [0 ms]
bd(-1,-2147483645).toPlainString() !! OutOfMemoryError: Requested array size exceeds VM limit  [0 ms]
```

These are HotSpot's **array-length ceiling** firing inside `StringBuilder`, not a
`BigDecimal` screen — there is no such line in `BigDecimal.java`. Porting it here
would be inventing a cap in the direction F20 warned about, and it belongs to
whatever models array limits, not to this file. **PREDICTED** for these rows:
CratonVM builds a ~2.1 GB Rust `String` and then meets its own allocator. The
size is bounded by `i32::MAX` and no longer by `usize` (that was the
`-i32::MIN` sign-extension F20 removed), but it is a real allocation.

### 5.4 The first cut of this was wrong, and measuring caught it

F20-1 N2 says "make `bd_read` fallible, which its own callers force". The
obvious reading — one fallible `bd_read`, thirteen `?` — is what this lane
wrote first. Then the callers were measured, and **only one of the thirteen is a
method HotSpot ever refuses**. `scratchpad/f31/{Bd6,Bd7}.java`, on the very
receivers `bd_plain_string_check` refuses:

```text
bd(1,MIN).equals(bd(1,MIN))     = true            [0 ms]
bd(1,MIN).hashCode()            = -2147483617     [0 ms]
bd(1,MIN).doubleValue()         = Infinity        [0 ms]   bd(1,MAX).doubleValue()  = 0.0
bd(-1,MIN).doubleValue()        = -Infinity       [0 ms]   bd(-1,MAX).doubleValue() = -0.0
bd(1,MIN).floatValue()          = Infinity        [0 ms]   bd(1,MAX).floatValue()   = 0.0
bd(1,MIN).stripTrailingZeros()  = 1E+2147483648   [5 ms]
bd(1,MIN).compareTo(bd(1,MAX))  = 1               [0 ms]   bd(1,MAX).compareTo(bd(1,MAX)) = 0
bd(1,MIN).divide(bd(1,MIN))     = 1               [0 ms]   bd(1,MAX).divide(bd(1,MAX))    = 1
bd(1,MIN).divide(bd(2,0),2,HALF_UP) = 0.01        [0 ms]

bd(1,MIN).toPlainString()      !! ArithmeticException: Overflow                     [0 ms]
```

Because none of those methods renders the value: `equals` compares `scale` then
the unscaled `BigInteger`, `hashCode` is `31*intVal.hashCode() + scale`,
`doubleValue` has its own fast paths, `compareTo` compares magnitudes. Only
`toPlainString` builds a string, so only `toPlainString` can hit a string-size
screen.

**A `?` on all thirteen would have been twelve new divergences** — the exact
failure mode this lane spent §4.2 documenting in someone else's proposed patch,
arriving one level out in its own. The split is `bd_read` (fallible, one caller)
and `bd_read_unchecked` (the old body, twelve callers), with both names carrying
the transcript.

What the split does *not* fix is that those twelve still build the
`scale`-sized string HotSpot never builds. That residual is §10; it cannot be
closed by refusing, only by not rendering.

---

## 6. Task 3 — the pin at `vm/src/vm/tests.rs`

The `isLoggable` row now asserts the **refusal**:
`.expect_err(...)` plus a `matches!` on
`RuntimeError::NullPointerException { message: Some(_) }`.

Two things about the pin's own comment were wrong, and both are recorded in the
replacement because they are the shape a pin goes stale in:

* **"a fabricated unconditional `true`"** — it was never unconditional.
  `system_logger_is_loggable` does compare severities; the row read as
  unconditional only because the call passes a null **receiver** *and* a null
  **level**, and a null level fell through to an `INFO` default. (F20-1 §7.3
  reached the same verdict independently.)
* **"the severity comparison this VM has nowhere to do yet"** — it was already
  that function's last line when the sentence was written.

The mechanical part matters too: F20's `system_logger_require_level` makes the
native return `Err`, so the old `.unwrap().unwrap()` would have **panicked**,
not assert-failed. That is noted at the site so the next reader does not
"restore" it.

### 6.1 The rest of the neighbourhood — a finding, not a chore

The brief asked for more tests that freeze CratonVM's output. In the
BigDecimal/logging neighbourhood of this file:

* **`system_logger_p67`'s `Level` section is clean.** It asserts *nothing* and
  says why, at length, including the measured oracle severities
  (`ALL=-2147483648 … OFF=2147483647`) and the reason a `<clinit>` conversion
  would trade a dead row for a live regression. A dead row with a recorded
  blocker is not a pin; left alone.
* **`isLoggable(OFF)`** is a divergence that is *recorded and deliberately not
  asserted*, because the two JDK implementations disagree with **each other**
  (`JULWrapper` says `true`, `SimpleConsoleLogger` says `false`) and which one
  is the oracle depends on whether `java.logging` resolves. Correctly left as
  prose at `system_logger_is_loggable` rather than frozen in a test. Confirmed,
  not changed.
* **The Phase 27 / Phase 93.2 `BigDecimal` fixtures** (`bigdecimal_init_and_to_string`,
  `bigdecimal_add_and_subtract`, and the `divide`/`compareTo` block near
  `java/math/BigDecimal` at ~74040) assert ordinary decimal values (`3.14`,
  `10.5 + 3.2`) that agree with HotSpot. They are thin, not frozen.

The two divergence pins converted by lane F16 (`panama_struct_layout_pe2` and
its sibling) were already flipped and are labelled as such. **No further pin was
found in this neighbourhood** — that is the finding.

### 6.2 What was added

`bigdecimal_extreme_scale_refusals_f31` drives all three roads end-to-end
through the registered natives on one receiver, `bd(1,MIN)`:
`intValue()` and `longValue()` must **answer 0**, `toBigInteger()` must refuse
with `Underflow`, and `bd(1,MIN).add(bd(1,MAX))` must refuse with `Underflow`.

It uses the **synthetic-stub layout** (slot 0 = decimal `String`, slot 1 =
`scale`), which is the only layout reachable from that module — `bd_layout`
falls back to it when the real `java.math.BigDecimal` is not loaded. That is
sufficient because every guard added here sits between `bd_unscaled_bigint` and
the arithmetic, and `bd_unscaled_bigint` reads exactly those two slots. The
`bd_read` screen (§5) is **not** reachable from that module, because
`bd_read_parts` returns `None` in the stub layout and `apply_scale` is never
called; its coverage is the pure-predicate test in §8 instead.

---

## 7. A fourth site, found by grepping the SHAPE

Not on the brief. `bd_set_scale_impl`'s scale-drop road built its divisor as

```rust
let mut divisor_dec = String::with_capacity(drop + 1);
divisor_dec.push('1');
divisor_dec.push_str(&"0".repeat(drop));
let divisor = BigInt::from_decimal(&divisor_dec);
```

— the same `"1" + "0"*n` → O(n²) parse F20 removed from `bigint_mul_pow10` and
this lane removed from `bd_truncated_bigint`, in a function that was already
cited as "the model" for being guarded. It **is** guarded: `bd_pow_ten_check`
runs first. But that check *admits* `drop` up to `715_827_882`, so the admitted
path was a 715 MB `String` plus a digit-at-a-time parse. Now `bigint_pow10(drop)`.

Same answer, and it was the fourth copy of one spelling. The lesson is the one
in `[1 of 10 callsites]`: the guard being present says nothing about the body
behind it.

While there, `bd_set_scale_impl`'s inline `ArithmeticException("Underflow")` —
the third literal copy of `checkScale`'s word — now calls the shared
`bd_underflow()`.

---

## 8. Tests, and what each one is for

All in `native-builtins/src/math_bignum.rs`, module `argument_driven_range_tests`.
Every expected value is a line of a `java` transcript, taken before the code was
written.

| test | pins |
|---|---|
| `plain_string_refusals_match_hotspot` | 18 rows of `toPlainString`, including the three one-character pairs of §5.2 |
| `to_big_integer_refusals_match_hotspot` | 13 rows; both boundary sides of `715_827_883`; `Underflow` for the same scale that §5's test asserts `Overflow` for |
| `narrowing_conversions_match_hotspot` | 24 rows of `intValue`/`longValue`, composed exactly as the natives compose them; the `-64`/`-63` line; the upper-bound row of §3.1 |
| `add_scale_alignment_matches_hotspot` | 14 rows; the five refusals, the four zero-operand exemptions of §4.2, and two ordinary alignments |
| `bigdecimal_extreme_scale_refusals_f31` (`vm/src/vm/tests.rs`) | the three roads end-to-end through the registry (§6.2) |
| `g12_biginteger_new_natives_answer_hotspot` (`vm/src/vm/tests.rs`) | ~30 `BigInteger` rows that replace 13 `registry.find(…).is_some()` assertions (§8.1) |

**Mutation checks** (what goes red if a rule is removed), reasoned per rule:

| mutation | rows that fail |
|---|---|
| drop `u.is_zero()` from `bd_rescale_operand` | the 4 exemption rows of `add_scale_alignment_…` |
| move `u.is_zero()` after the range test | none — it is checked *before* `raise > i32::MAX`, and `bd(1,MIN).add(bd(0,MAX))` covers the ordering the other way |
| compute `s - sa` in `i32` again | `add_scale_alignment_…` **panics** in debug (that is the abort, made visible) |
| unify `"Underflow"` and `"Overflow"` | exactly one of `to_big_integer_refusals_…` / `plain_string_refusals_…` |
| put the narrowing fast paths in `bd_truncated_bigint` instead | `narrowing_conversions_…` rows `bd(1,MIN)`, `bd(1,MAX)`, `bd(1,715827883)` |
| change `scale <= -64` to `< -64` or `<= -63` | `narrow_long("7",-64)` / `narrow_long("7",-63)` |
| use the unsigned digit length (drop the `'-'`) on the negative `toPlainString` road | `plain("-1", -2147483646)` |
| add the unscaled digits to the positive road's `len` | `plain("1", 2147483645)` |
| short-circuit `unscaled == "0"` for positive scales too | `plain("0", i32::MAX)` |

### 8.1 Task 4 — E40-1 NOMINATION N6, the `BigInteger` member

`vm/src/vm/tests.rs`'s `g12_biginteger_new_natives_registered` was thirteen
assertions of the form

```rust
assert!(registry.find(bi, "gcd", "(L…BigInteger;)L…BigInteger;").is_some());
```

— the tree checked against itself. E40-1 §4a censused 24 such tests (130
`is_some()` / 12 `is_none()` assertions) and its N6 asked for exactly this
conversion; F9-1 confirmed N6 still open. It is the same defect class as Task 3,
one notch worse: a divergence pin at least *records* an answer, whereas an
existence check never looks at one. All thirteen natives could have returned zero
and it would have stayed green.

It is now `g12_biginteger_new_natives_answer_hotspot`, ~30 measured rows
(`scratchpad/f31/{Bi13,Mp}.java`). The conversion loses nothing: `call_native`
panics with `"<class>.<method><descriptor> not registered"` when a triple is
missing, so every row asserts the registration *and* the answer.

Four rows are rules a census could not have held, and each is a plausible thing
to get wrong:

```text
(-1).bitLength()        = 0      (not 1 — two's-complement excess over the sign bit)
(-1).bitCount()         = 0      (bits DIFFERING from the sign bit)
(3).shiftRight(-4)      = 48     (a negative count reverses direction; not an error)
(4).isProbablePrime(0)  = true   (`if (certainty <= 0) return true;` — for a composite)
```

plus three refusals with their exact text: `testBit(-1)` →
`ArithmeticException: Negative bit address`, `modPow(2,3,0)` →
`ArithmeticException: BigInteger: modulus not positive`, `modInverse(2,8)` →
`ArithmeticException: BigInteger not invertible.` (note the trailing period).

**Which bodies the rows reach was checked, not assumed.** `register_builtins` is
`register_essential_natives` then `register_synthetic_overrides`, and
`register()` is last-write-wins, so `math_bignum::register_biginteger_natives`
wins for the four triples it still registers (`gcd`, `isProbablePrime`,
`modPow`, `modInverse`) and the nine E38-1/F2 deleted from it resolve to
`phases_late::register_p71_biginteger_extras`. Both bodies were read before the
expectations were written — in particular `phases_late`'s `testBit` already
raises `Negative bit address`, so W8-F7-1 §4's note about that arm answering
`false` is about the **unregistered** `bi_test_bit_str` helper, not the live
path.

**Not covered by any test in this lane**, because it needs a running VM: whether
these refusals reach Java as catchable `ArithmeticException`/`OutOfMemoryError`
rather than as `MethodCallFailed::InternalError`. The assertions test the value
this file produces; the conversion is the VM's.

---

## 9. NOMINATIONS

Both are **doc-only** and **neither blocks compilation**.
`native-builtins/src/lib.rs` was being edited by another session while this lane
ran, so nothing there was touched.

### N1 — `native-builtins/src/lib.rs`: `bigint_mul_pow10`'s doc comment is now stale

Its "# `n` comes from an ARGUMENT, and this function cannot refuse" section
names the three unguarded call sites as a present-tense fact. They are guarded
as of this record, and the function now has exactly two callers, both in
`math_bignum.rs` and both guarded (`bd_rescale_operand`, `bd_set_scale_impl`).

*exact literal old text:*
```rust
/// a `Result`, so **the refusal has to be at the call sites**. Every caller
/// must have run `math_bignum::bd_pow_ten_check(n)?` first. Three had not, as
/// of 2026-08-13 (lane F20 — see
/// `docs/known-issues/jdk-only/F20-1-the-three-unguarded-rescales-and-the-scale-that-negates-into-a-panic-20260813.md`):
/// `native_bd_add`, `native_bd_subtract` and `bd_truncated_bigint`, all in
/// `native-builtins/src/math_bignum.rs`. `bd_set_scale_impl` is guarded and is
/// the model.
```
*exact literal new text:*
```rust
/// a `Result`, so **the refusal has to be at the call sites**. Every caller
/// must have run `math_bignum::bd_pow_ten_check(n)?` first. Three had not as of
/// 2026-08-13 (lane F20 — `native_bd_add`, `native_bd_subtract` and
/// `bd_truncated_bigint`); lane F31 closed all three, and the guard for `add`/
/// `subtract` had to be `bd_rescale_operand`, which takes the OPERAND as well
/// as `n` because a zero operand is exempt from the JDK's own check. See
/// `docs/known-issues/jdk-only/F31-1-three-roads-out-of-one-scale-and-the-zero-operand-that-is-exempt-20260813.md`.
/// The two surviving callers are `math_bignum::bd_rescale_operand` and
/// `math_bignum::bd_set_scale_impl`, both guarded.
```

### N2 — `native-builtins/src/lib.rs`: `apply_scale`'s doc comment records a refusal that has landed

Its closing paragraph says the `scale == i32::MIN` divergence and the large
positive scale "are the two divergences left here", and item 2 of its numbered
list points at F20-1 for the missing refusal. The first is now refused by
`math_bignum::bd_plain_string_check`, one caller up.

*exact literal old text:*
```rust
/// 2. **The remaining size is still `scale`-driven**, and this function
///    returns a `String`, not a `Result`, so the *refusal* has to be at the
///    caller. It is not there yet — see the NOMINATION in
///    `docs/known-issues/jdk-only/F20-1-the-three-unguarded-rescales-and-the-scale-that-negates-into-a-panic-20260813.md`.
```
*exact literal new text:*
```rust
/// 2. **The remaining size is still `scale`-driven**, and this function
///    returns a `String`, not a `Result`, so the *refusal* has to be at the
///    caller. It is there as of 2026-08-13:
///    `math_bignum::bd_plain_string_check`, which `math_bignum::bd_read` runs
///    before calling this. It ports `toPlainString`'s `checkScaleNonZero`
///    ("Overflow" — the CASTING form, not `setScale`'s clamping "Underflow")
///    and its `len < 0` → `OutOfMemoryError("too large to fit in a String")`
///    screen. See
///    `docs/known-issues/jdk-only/F31-1-three-roads-out-of-one-scale-and-the-zero-operand-that-is-exempt-20260813.md`.
```

### N3 — `native-builtins/src/math_bignum.rs` + two files this lane does not own: a `panic!` in a `pub(crate)` helper

**This one is a live VM abort, not a doc fix**, and it is nominated only because
closing it properly changes a signature with call sites in `bigint.rs` and
`phases_late.rs`. E38-1 §3 found it and left it "flagged":

```rust
// native-builtins/src/math_bignum.rs, in `bi_mod_pow_str`
if e_neg {
    // …
    panic!("bi_mod_pow_str: negative exponent — caller must compute modInverse first");
}
```

Its own comment says it chose to "panic to be loud". A Rust panic is not a Java
throwable: it is the loudest possible thing and the least catchable. All five
call sites strip the sign first today (`math_bignum:1141` Miller-Rabin,
`math_bignum`'s `modPow` native, `phases_late:8802`, and two differential tests
in `bigint.rs`), so it is a landmine rather than a live defect — but a
`pub(crate)` helper with five callers is one careless sixth away.

MEASURED, `scratchpad/f31/Mp.java` — a negative exponent is perfectly legal
`BigInteger`:

```text
2.modPow(-1, 7)   = 4        3.modPow(-2, 10) = 9       2.modPow(-3, 7) = 1
2.modPow(-1, 8)  !! ArithmeticException: BigInteger not invertible.
0.modPow(-5, 7)  !! ArithmeticException: BigInteger not invertible.
2.modPow(3, 0)   !! ArithmeticException: BigInteger: modulus not positive
2.modPow(-1, -7) !! ArithmeticException: BigInteger: modulus not positive
```

So there is exactly one shape a `-> String` signature cannot express: a negative
exponent over a non-invertible base. The fix is `-> Option<String>` (`None` for
that one case), with the negative-exponent branch doing what `modPow` does —
`bi_mod_inverse_str` is thirty lines further down the same file.

*File:* `native-builtins/src/math_bignum.rs`
*exact literal old text:*
```rust
    // exp must be non-negative for plain modPow.
    let (e_neg, e_abs) = bi_parse_sign(exp);
    if e_neg {
        // Caller is responsible for inverting base first.
        // Fallback: treat as |exp| (consistent with our previous buggy
        // wrapping_mul behavior is not OK; instead return 0 sentinel — but
        // returning 0 is itself a synthetic stub, so panic to be loud).
        panic!("bi_mod_pow_str: negative exponent — caller must compute modInverse first");
    }
```
*exact literal new text:*
```rust
    // A negative exponent is LEGAL `BigInteger` — `modPow` inverts the base and
    // raises the inverse to |exp| (`BigInteger.java:2916-2918`). MEASURED on
    // 25.0.3+9: `2.modPow(-1,7)` = 4, `3.modPow(-2,10)` = 9, and
    // `2.modPow(-1,8)` !! `ArithmeticException: BigInteger not invertible.`
    // This used to `panic!` here "to be loud"; a Rust panic is not a Java
    // throwable, so it was the one failure mode no `catch` could ever see.
    let (e_neg, e_abs) = bi_parse_sign(exp);
    if e_neg {
        let inv = bi_mod_inverse_str(&b, m_abs)?;
        return bi_mod_pow_str(&inv, e_abs, m);
    }
```
(with the signature becoming
`pub(crate) fn bi_mod_pow_str(base: &str, exp: &str, m: &str) -> Option<String>`,
`return "0".to_string()` → `return Some("0".to_string())` and the final
`result` → `Some(result)`.)

**Ripple — three call sites outside this file.** Each is a `?` or an `.expect`,
and **the change does block compilation until all three land**, which is why it
was not taken here:

* `native-builtins/src/phases_late.rs:8802` — `let res = bi_mod_pow_str(&inv, pos_exp, &m);`
  → `let Some(res) = bi_mod_pow_str(&inv, pos_exp, &m) else { return Err(RuntimeError::ArithmeticException { message: "BigInteger not invertible.".to_string() }.into()); };`
  (the exponent there is already positive, so the `else` is unreachable and the
  message is the JDK's for the shape that would reach it).
* `native-builtins/src/bigint.rs:1071` — `let want = bi_mod_pow_str(ba, e, m);`
  → `let want = bi_mod_pow_str(ba, e, m).expect("positive exponent");`
* `native-builtins/src/bigint.rs:1090` — `bi_mod_pow_str(&ba, &e, &m),`
  → `bi_mod_pow_str(&ba, &e, &m).expect("positive exponent"),`

The two `math_bignum.rs` call sites (`:1141`, and the `modPow` native's two
arms) are this file's own and would land with it.

---

## 10. Residuals

* **`Requested array size exceeds VM limit`** (§5.3). Deliberate: it is
  HotSpot's array ceiling, not a `BigDecimal` screen.
* **`bd_alloc_bigint`'s synthetic-stub arm** renders the exact result through
  `apply_scale`, so in **synthetic-jdk mode only**, `bd(0,0).add(bd(1,MAX))` —
  a row HotSpot answers `1E-2147483647` — asks for a 2.1 GB string. This is not
  fixable by refusing (HotSpot does not refuse); it is the stub layout being
  unable to represent an exact `(unscaled, scale)` pair. Not reachable under
  `--jdk-only`, which uses the real layout. Recorded, not changed.
* **`native_bd_divide_scale`'s positive half** — W8-F7-1 §6's residual, unchanged.
  Its boundary is still not measurable in reasonable time (`1.5.divide(2, 715827883, HALF_UP)`
  did not return in 240 s on HotSpot), and this lane reproduced the same wall:
  `bd(1,0).add(bd(1,715827882))` did not return inside a 400 s cap either.
  Refusing where HotSpot merely takes forever would be a divergence.
* **`bd_read_unchecked`'s twelve callers still render a `scale`-sized string**
  (§5.4) — `equals`, `hashCode`, `compareTo`, `divide`, `divide(…,scale,…)`,
  `doubleValue`, `floatValue`, `stripTrailingZeros`. HotSpot answers all of them
  in 0 ms without rendering anything, so this **cannot be closed by refusing**;
  it closes by not rendering. Concretely: `equals` wants `scale` then the
  unscaled `BigInt` (the file already has `bd_scale_of` and
  `bd_unscaled_bigint`); the four `f64` consumers want the value computed from
  `(unscaled, scale)`, where the adjusted exponent alone decides every case
  outside `±10^±400` (`> 400` is `±Infinity`, `< -400` is `±0.0` — both
  provable, not guessed, since `f64::MAX` is `1.8e308`), which bounds the
  rendering at roughly `2·digits + 400` characters; `hashCode` wants the JDK's
  `31*intVal.hashCode() + scale`. Not taken here: those are precision-axis
  changes to natives this lane has no oracle sweep for, and a blind rewrite of
  `compareTo` is a worse trade than a recorded DoS.
* **`native_bd_compare_to` / `native_bd_divide` / `doubleValue` / `floatValue`
  still route through `f64`.** `compareTo` therefore ties for two `BigDecimal`s
  that differ past ~15-16 significant digits, where the exact `(unscaled, scale)`
  machinery this file already has would not. Out of this lane's scope (the brief
  is the argument-driven-allocation axis, and this is a precision axis), and it
  is a *pre-existing* shape, not one this lane introduced or moved.
* **Nothing in this record claims a CratonVM behaviour was observed.** §1's
  "after" column and every PREDICTED label are written so that one run can
  falsify them.
