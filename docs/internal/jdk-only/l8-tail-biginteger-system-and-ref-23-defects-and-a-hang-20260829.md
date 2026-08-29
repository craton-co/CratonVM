# L8 tail — `BigInteger`, `System`/`Runtime`/`Object`, and `java.lang.ref`: 23 defects, one of them a hang

**Status: CLOSED, with four recorded residuals.** 2026-08-29, branch
`claude/l8-tail-20260829`, worktree `/data/cvm-l2s-20260828`. Oracle: HotSpot
`jdk-25.0.4+7`, the same image CratonVM ran against.

Batches three, four and five of the long tail, landed together because they
share one build. Three probes, **13440 rows**, and the batch's two most
interesting findings are both about how a defect announced itself rather than
about what it was.

| probe | rows | before | after |
| --- | ---: | ---: | ---: |
| `apps/probes/BigIntegerSweep.java` | 13255 | 5 | **0** |
| `apps/probes/SystemRuntimeObjectSweep.java` | 125 | 22 | **5 / 3** (recorded, §4) |
| `apps/probes/RefFamilySweep.java` | 60 | *truncated at 54* | **0** |
| `apps/probes/HelpfulNpeProbe.java` | 20 | 2 | **0** |

---

## 1. `java.math.BigInteger` — 24 rows, and an exhaustive sweep was cheap

Arithmetic is the easiest surface in this campaign to sweep exhaustively,
because it is pure: no clock, no locale, no file system, no allocation order a
caller can see. So the probe does not sample. It takes 24 values chosen to put
one on each side of every representation boundary the implementation could
plausibly have — the `int` and `long` limits, the 32-bit word boundary the JDK's
`mag[]` is built from, the sign-magnitude discontinuity at zero, a value whose
two's-complement `toByteArray` needs a leading pad byte and one whose does not —
and runs **every ordered pair through every binary operation**, plus the algebra
those operations owe each other (`a.divide(b).multiply(b).add(a.remainder(b))`
is `a`; `and`/`or`/`xor` cover; `not` is an involution).

**13255 rows and five differences.** Which is the useful result: the limb
arithmetic is right, and everything wrong with it was on an error path.

### D1 — `modPow` accepted a negative modulus

```text
a.modPow(TWO, -7)   HotSpot  ArithmeticException: BigInteger: modulus not positive
                    was      1
a.modPow(-3,  -7)   HotSpot  ArithmeticException     was 1
a.modPow(TWO, -1)   HotSpot  ArithmeticException     was 0
```

The body took `|m|` on the strength of its own comment: *"a negative modulus is
outside the BigInteger spec, which requires m > 0"*. **Outside the spec is
exactly what the spec refuses**, so the reasoning that justified computing
anyway is the reasoning for throwing. And `math_bignum.rs`'s doc comment
recorded the right answer the whole time — `3.modPow(2, -7) !! ArithmeticException:
BigInteger: modulus not positive` — with nothing linking it to the body that
disagreed.

The sweep did not find this by itself: every modulus in its modular section was
positive. It took a row written after reading the code.

### D2 — the zero-modulus message

`modulus is zero` against the JDK's `BigInteger: modulus not positive`, with the
sibling `modInverse` thirty lines below already carrying the right one.

### D3 — thirteen methods flattened HotSpot's helpful NPE

```text
BigInteger.ONE.add(null)
  HotSpot   NullPointerException: Cannot read field "signum" because "val" is null
  was       NullPointerException: null object argument
```

The first reading was that CratonVM does not implement JEP 358 helpful NPEs at
all. `apps/probes/HelpfulNpeProbe.java` was written to size that, and refuted
it: **thirteen general shapes — a null local invoked, a null field read and
written, an array length, a load, a store, an unbox, an `athrow`, a
`monitorenter`, an interface call — all match HotSpot exactly.** The machinery
is right.

What flattens the message is the SHADOW. A native intercepts before the bytecode
that would have produced it, and the shared `obj_arg` helper — some 3900 call
sites — has only a generic string to give. The proof is inside the probe's own
output:

```text
andNot, min, max, compareTo, divideAndRemainder    no native   message CORRECT
add, subtract, multiply, divide, remainder, mod,   shadowed    message WRONG
gcd, and, or, xor, modInverse, modPow x2
```

Five unshadowed methods on the same class, taking the same argument type, with
the right answer. **The unshadowed neighbours are the control, and they were
sitting in the list already.**

Fixed by giving `BigInteger`'s thirteen a `bi_obj_arg` that carries the message
the real body's first dereference produces — five constants, harvested from
HotSpot one row per method. **The general shape is NOT fixed and is the batch's
largest recorded residual:** every other shadowed native taking an object
argument still answers `null object argument` where the JDK names the field.

---

## 2. `System` / `Runtime` / `Object` — 43 rows, 22 differences, 18 fixed

The most-used surface in the tail and the least probed, which is the expected
combination once you look.

The probe is mostly an exercise in **not** asking. `System.exit` and
`Runtime.exit` are never called — a probe that exits has no tail. `java.vm.name`
and the identity hash inside `Object.toString()` are *supposed* to differ and
are asked as shapes instead of values. Every mutator restores what it found and
the row after it checks the restore took, because a probe that corrupts
`System.out` halfway through writes a short file, and a short file reads as a
clean diff for every row it never printed.

| # | what | rows |
| --- | --- | ---: |
| D4 | `checkKey` was missing: an empty or null property key answered instead of throwing | 7 |
| D5 | `System.setProperties(p)` replaced the map and `getProperty` answered from the bootstrap fallback anyway | 1 |
| D6 | `System.getenv(null)` answered null | 1 |
| D7 | **`System.setIn` could not be observed at all** | 1 |
| D8 | `System.getLogger(null)` / `getLogger(name, null)` answered a logger | 2 |
| D9 | `System.load` skipped the absolute-path check, `loadLibrary` the separator check | 2 |
| D10 | `notifyAll` without the monitor carried a different message from `notify` beside it | 1 |
| D11 | **`Object.clone()` cloned a receiver that is not `Cloneable`** | 1 |
| D12 | `java.version` / `java.runtime.version` were hardcoded to a version the image is not | 5 |

### D7 — one `if`/`else if` chain, three fields, and only two of them read the field

`System.setIn(x)` wrote the static field correctly — reflection on
`System.in` confirmed it — and every `getstatic System.in` still answered the
original stream. The getstatic intercept has an arm for `out`/`err` that reads
the static field first and falls back to the canonical stream, and an arm for
`in` that went straight to `ensure_system_stdin_object` and never looked.

**Feeding stdin through `System.setIn` is the ordinary way to do it in a test
harness**, and it silently read the real process stdin instead. The three fields
have identical semantics and sat in the same chain; one of them was not asked
the question.

The diagnosis took one twelve-line probe, and it is worth noting what made it
short: asking the FIELD and the GETSTATIC separately. A probe that only asked
`System.in` would have said "setIn does not work" and sent a reader to `setIn`.

### D11 — `Object.clone()` on a non-`Cloneable`

```text
new Object(){ Object call() throws Throwable { return super.clone(); } }.call()
  HotSpot   CloneNotSupportedException
  was       a copy
```

A fabricated success, and the kind that surfaces much later as two objects where
the program expects one. The check is a transitive walk of the superclass chain
and the interfaces at each level, written out rather than routed through an
assignability helper because the superclass chain and the declared interfaces
are the two facts the `NativeContext` trait exposes, and `Cloneable` is a marker
with nothing else to ask about.

### D12 — one constant, five rows

`java.version` was seeded as `"25.0.1"` regardless of which image `--java-home`
points at, so a run against `25.0.4+7` reported `25.0.1` — which is what every
version-sniffing library reads. `Runtime.version()` parses
`java.runtime.version`, so the same constant was also
`Runtime.version().update()`, `.build()` and `.toString()`.

The image states both in its own `release` file, and the VM already had a reader
for it in another crate. `java.vm.name` and `java.vm.version` are left alone:
those name CratonVM, and they are *supposed* to differ.

---

## 3. `java.lang.ref` — 19 rows, and the probe stopped writing

`RefFamilySweep` is 60 rows and CratonVM wrote 54. **A hang announces itself as a
short file**, and a short file's missing tail reads as an ordinary run of `<`
lines in a diff — which is why the row counts are printed before the diff and
compared first.

### D13 — `ReferenceQueue.remove(-1)` waited forever

The timeout is read as `*v as u64`, so `-1` becomes `u64::MAX`. The JDK's first
statement is `if (timeout < 0) throw new IllegalArgumentException("Negative
timeout value")`.

### D14 — `ReferenceQueue.remove(0)` polled and returned

Zero means NO timeout — the same convention as `Object.wait(0)`, and the JDK
spells it in the body: `long start = (timeout == 0) ? 0 : System.nanoTime()`,
then a `lock.await(timeout)` that waits indefinitely. A caller asking to block
until something arrives got an immediate null. Fixed by delegating to the
blocking overload, which already existed — the correct implementation was ten
lines away and the zero case simply never reached it.

Everything else about the family was already right, and it is a surface where
that is worth saying: `get` versus `refersTo` on a phantom reference, `clear`
not disturbing a sibling reference to the same referent, enqueue ordering,
enqueue-once, and a reference enqueued to one queue staying out of another.

**The probe deliberately never calls `System.gc()`.** Almost every interesting
question about a reference is answered by the collector, and two collectors may
answer differently at any moment; a probe that gc'd and then asked `get()` would
be measuring policy and would be right about something different each run. Every
row here is one the specification fixes with the referent strongly held.

---

## 4. The four residuals, and why each is recorded rather than fixed

**R1 — `System.setOut(null)` cannot be observed.** HotSpot lets `System.out`
become null; here the getstatic intercept treats an absent field as "not
overridden yet" and answers the canonical stream. The two states — never
initialised and explicitly nulled — are the same value at that site, and telling
them apart needs a tri-state the streams do not currently carry. A genuinely
null `System.out` is also a state no program wants and one the VM's own
diagnostics would not survive. Recorded with its cost rather than fixed on a
row whose value is a curiosity.

**R2 — `System.setSecurityManager(null)` succeeds where HotSpot throws
`UnsupportedOperationException`.** This is a *documented* decision in
`security_manager.rs`: the installed-manager check is what gates exec and
Panama, and its own comment says "revisit only together with a replacement for
the exec/Panama gating — the two cannot be separated, which is why this is a
security-model decision and not a stub to remove". Changing it from outside that
lane would be dismantling a security model to turn one row green.

**R3, R4 — the `System.LoggerFinder` in `--jdk-only` is the wrong one**, and
this is the batch's largest open finding. `System.getLogger` returns:

```text
HotSpot            sun.util.logging.internal.LoggingProviderImpl$JULWrapper
CratonVM jdk-only  jdk.internal.logger.SimpleConsoleLogger     <- real JDK class
CratonVM compat    cratonvm.internal.SystemLogger
```

In `--jdk-only` the real `LoggerFinderLoader` runs and **falls back to the
no-service-found console logger**, so every `System.Logger` in that mode bypasses
JUL and ignores any logging configuration. That is a `ServiceLoader`/module-graph
defect (the `java.logging` module's `LoggerFinder` provider is not visible),
several sizes larger than the two rows it shows up on here, and not this batch's
to fix.

It is worth recording HOW it was found, because two rows nearly hid it. The
visible symptoms were `isLoggable(OFF)` and the method named in a null-`Level`
NPE, and both looked like small CratonVM bugs in its own logger. A previous lane
had already met the first one, MEASURED it, and deliberately left it:

> Which one is the oracle for a given CratonVM run depends on whether
> `java.logging` is resolved, which this lane could not determine without
> running the VM. **The `OFF` arm is therefore left as it is and recorded, not
> changed.**

That was the right call on the evidence it had, and it is the same shape as the
`URI` deferral this lane's first batch closed: *a deferral whose stated reason
is "I could not determine X" is a request for a measurement.* Running the VM
took one twenty-line probe and did not answer the question as asked — it
replaced it. The `OFF` arm is now conditional on which provider is in force
(which fixes it in compatible mode, where CratonVM's own logger is used), and
the real answer is that `--jdk-only` selects the wrong provider entirely.

**A probe row that asks the CAUSE now sits above the two symptoms**
(`getLogger provider is the JUL wrapper`), so the next reader does not have to
re-derive it.

---

## 5. What this does NOT establish

* **`obj_arg`'s generic NPE message is fixed for `BigInteger` only.** ~3900 other
  call sites still answer `null object argument` where the JDK names a field and
  a parameter. Sizing that is a lane of its own; the shape and the control
  (unshadowed neighbours) are recorded here.
* **`ReferenceQueue.remove(0)` now BLOCKS where it used to return.** That is the
  specified behaviour and it is what the corpus was run against — all three arms
  green — but it converts a returning call into an indefinite wait for any caller
  that was relying on the old answer.
* **No performance measurement was taken.** The property path gained one atomic
  load on the missing-property branch, and `Object.clone` gained a hierarchy walk
  on the non-array path. Both are arguments, not numbers.
* **The `--jdk-only` LoggerFinder defect is characterised, not diagnosed.** What
  is measured is which class comes back; WHY the service is invisible is not.
