# H22-2 — two of `H14-3`'s five free retirements are not free: one is fatal on the first call, the other breaks every stack trace

**Status: OPEN — MEASURED, eight differential arms of one 517-assertion probe
against HotSpot 25.0.3+9, plus one exact-invocation census.** All on the
prebuilt `C:/craton/cratonvm-r5.exe` (2026-08-20 21:57). **No source change
was made for either family** — that is the finding. Oracle
`C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot`, diffed on **stdout only**.

Lane H22, 2026-08-21. This record contradicts `H14-3` N1 and N2, which are the
two largest items on that record's plan, and it does so with the instrument
`H14-3` §6 explicitly asked someone to build:

> A zero is permission to attempt and measure, not permission to skip
> measuring. Five zero-cost cells mean *these 104 vectors raise no objection*.

The 104 vectors raise no objection. A 517-line probe does.

---

## 1. The table

The probe is one Java class, 517 printed assertions, run under `--jdk-only`
with `CRATONVM_ENFORCE_NATIVE_SHADOW=<class-list>` — the same dial `H14-3` used,
armed on **the full class list each registrar actually registers**, which is
where the first surprise is (§4).

| # | armed class set | source | probe lines | **diff vs HotSpot** | verdict |
|---:|---|---|---:|---:|---|
| 0 | *(unarmed baseline)* | — | 517 | 16 | 3 pre-existing defects |
| 1 | `java/util/HexFormat` | `H22-1` | 517 | **16** | **free** |
| 2 | `java/lang/StringBuilder`, `java/lang/StringBuffer` | `H14-3` row 2's exact set | 517 | **16** | free |
| 3 | `java/lang/AbstractStringBuilder` | — | 517 | **16** | free |
| 4 | **rows 2 + 3 together** | what the registrar registers | **0** | **518** | **FATAL — `rc=1`** |
| 5 | `java/lang/StringBuilder` + `java/lang/AbstractStringBuilder` | the minimal fatal pair | **0** | **518** | **FATAL — `rc=1`** |
| 6 | `register_throwable_subclass_natives`'s **62** classes | what the registrar registers | 517 | **248** | **61 regressions** |
| 7 | rows 1 + 4 + 6 | — | 0 | 518 | fatal (row 4 dominates) |

Rows 4 and 5 do not produce a wrong answer. They produce **no answer**: the VM
exits `rc=1` before printing its first line.

## 2. `register_string_builder_natives` is fatal, and each half of it is free

MEASURED, row 5, on the very first `StringBuilder.append(char[])` in the probe:

```
Exception in thread "main" java/lang/ArrayStoreException:
    arraycopy: type mismatch: can not copy byte[] into char[]
        at H22Probe.stringBuilders(H22Probe.java:23)
        at java/lang/StringBuilder.append(StringBuilder.java:218)
        at java/lang/AbstractStringBuilder.append(AbstractStringBuilder.java:763)
        at java/lang/AbstractStringBuilder.appendChars(AbstractStringBuilder.java:1991)
```

**The tree already knows why**, in a comment written by a different
investigation about a different symptom —
`vm/src/runtime/interpreter/native_override.rs:5129`:

> the redefinition evicted the shadow and handed them back to real JDK bodies
> that index a compact `byte[] value` / `byte coder` / `int count` layout
> **CratonVM's two-field `char[]`/`int` builder does not have**.

That is the whole diagnosis. `register_string_builder_natives` is not a shadow
over equivalent bytecode. It is **the accessor set for a different object
layout**, and JDK 25's compact-strings `AbstractStringBuilder` bytecode cannot
run against a `char[]`/`int` builder. This is a P0-shaped object-model item
wearing a registration item's clothes — the same class of mistake
`jdk-only-p0-rows-are-object-model-not-tagging-problems` records for the
collection cluster.

### 2a. Why `H14-3` measured zero, and why the zero was correct

`H14-3` armed `StringBuilder` and `StringBuffer`. Prefix matching is textual,
and `java/lang/StringBuilder` **does not prefix-match**
`java/lang/AbstractStringBuilder`. So in that arm:

```
StringBuilder.append(char[])          -> bytecode  (armed)
  AbstractStringBuilder.append(char[]) -> NATIVE   (not armed)  <-- catches it
```

The native at the superclass caught every call before it reached compact-string
bytecode. The cell is 103/104 net 0 and it is **an honest measurement of the
wrong thing**: it priced 128 of the registrar's 192 registrations and left the
64 that hold the layout together in place.

`register_string_builder_natives` is called with three class names
(`lib.rs:18487–18489`) and again with two (`lib.rs:23816–23817`). A source
retirement deletes all of them at once and lands in row 5.

### 2b. The 64 that never ran, and the census trap in the partial

MEASURED with `--nojit CRATONVM_DISABLE_INTRINSICS=1` (the configuration
`G33-1` requires for an exact `invocations` column), over the probe:

| class | registrations | with `invocations > 0` | total dispatches |
|---|---:|---:|---:|
| `java/lang/StringBuilder` | 64 | 35 | 93 |
| `java/lang/StringBuffer` | 64 | 12 | 28 |
| **`java/lang/AbstractStringBuilder`** | **64** | **0** | **0** |

Zero, and paired with a positive control in the same run (the other two rows,
and 39 HexFormat dispatches). This is `H11-1` — dispatch keys on the receiver —
holding exactly: nothing is ever *a* `java/lang/AbstractStringBuilder`, so
while the subclass natives win, the superclass registrations are unreachable.

Row 2 is therefore available as a **partial** retirement: delete the
`StringBuilder` and `StringBuffer` call sites, keep the `AbstractStringBuilder`
one. It is measured free. **But it is close to census-neutral, and anyone
quoting it as "57 rows" will be wrong.** A `native-shadows-bytecode` row is
recorded when a native wins over bytecode for a triple. After the partial,
`StringBuilder.append` is bytecode and immediately calls
`AbstractStringBuilder.append`, where a native wins — so the rows do not
disappear, they **migrate to `java/lang/AbstractStringBuilder`**, a class that
contributes 0 rows today precisely because the natives above it absorb every
call. **PREDICTED, not measured** (it needs a build): the census falls by far
fewer than 57, and `AbstractStringBuilder` appears in the class histogram for
the first time.

That prediction is the cheapest possible test of whether the partial is worth
landing, and it should be run before the PR, not after.

## 3. `register_throwable_subclass_natives` costs 61 stack traces, and it is one VM defect

Row 6: 62 classes armed, **248 differing lines**. Decomposed, MEASURED:

| what moved | lines | direction |
|---|---:|---|
| `getStackTrace()[0].getMethodName()` on **61 of 61** throwables | 244 | **regression** |
| `setStackTrace(null)` | 4 | unchanged — wrong armed *and* unarmed |
| `TypeNotPresentException.getMessage()` / `.getLocalizedMessage()` / `.toString()` | 8 | **fixed** |

Nothing else moves. `getMessage`, `getLocalizedMessage`, `toString`,
`getCause`, `getSuppressed`, `addSuppressed`, `initCause` (including both
`IllegalStateException` contracts), `InvocationTargetException.getTargetException`,
the no-message `toString` form and the suppressed-self / suppressed-null
contracts are **byte-identical to HotSpot across all 62 classes** under the arm.
The three lines that get *better* are the positive control that the dial bit.

### 3a. The mechanism, and it is one function

HotSpot's `Throwable.<init>` calls `fillInStackTrace()`, which skips every
frame up to and including the last `<init>` of a `Throwable` subclass.

CratonVM has two paths into the same capture:

* **Unarmed** — the native `<init>` (`native_exc_init_*` in `lang_misc.rs`)
  calls `capture_throwable_trace(ctx, this)` **from a native frame**, so the
  top Java frame is already the caller. Correct by construction — **but only
  when the class being instantiated is the one whose `<init>` is the native.
  See §3b.**
* **Armed / retired** — real `Throwable.<init>` bytecode runs, calls
  `fillInStackTrace()`, which reaches `Throwable.fillInStackTrace(I)` —
  `ACC_NATIVE` in the image, registered at `native-builtins/src/lib.rs:14535`,
  `owns_slot: true` — which calls **the same `capture_throwable_trace`**, now
  from *inside* the constructor frames. Nothing skips them.

The split in the observed values is the signature: 54 classes report `<init>`
and 7 (`Throwable`, `Exception`, `Error`, `UncheckedIOException`,
`ClassNotFoundException`, `ExceptionInInitializerError`,
`FormatterClosedException`) report `fillInStackTrace`, which is exactly what
differing constructor-chain depths do to an off-by-N frame skip.

### 3b. The unarmed path is NOT correct by construction, 2026-09-11

Measured by lane L3 on `cratonvm-p18`, plain `--jdk-only`, **no dial**, with
`apps/probes/ThrowableCtorFrameSkip.java` — 9 rows, 20 seconds:

```text
row                              HotSpot                    CratonVM
A JDK direct RuntimeException    ThrowableCtorFrameSkip.jdkDirect     same
D JDK direct IOException         ...jdkChecked                        same
E JDK InaccessibleObjectException ...jdkReflective                    same
F new Exception                  ...main                              same
B app subclass depth 1           ...appDepth1        ThrowableCtorFrameSkip$D1.<init>
C app subclass depth 2           ...appDepth2        ThrowableCtorFrameSkip$D1.<init>
G thrown and caught              ...main             ThrowableCtorFrameSkip$Custom.<init>
H explicit fillInStackTrace()    ...main             java.lang.Throwable.fillInStackTrace
```

**4 of 9 differ, and the mechanism is the one §3 names — one step earlier than
§3 places it.** Capturing from a native frame drops the constructors that ARE
natives, which is the JDK half of the chain. Every *application-level* `<init>`
survives. Row C is the proof it is a chain and not an off-by-one: at depth 2 the
top frame is `D1.<init>`, the native's immediate caller, not `D2.<init>`.

So the correct statement is narrower than "unarmed is correct": unarmed is
correct exactly when the instantiated class's own `<init>` is the registered
native. A JDK throwable created directly qualifies. **A user-defined exception
subclass never does, at any depth** — and that is most exceptions in real
application code, which makes this a live defect in the shipping configuration
rather than only a retirement blocker. Row H says an explicit
`fillInStackTrace()` does not skip itself either.

**This raises N2's priority.** The nomination is written as "unblocks 906
registrations over 62 classes"; it also fixes `getStackTrace()[0]` for every
application exception in the VM today. The 105-vector corpus checks neither,
which is why two lanes reached this from opposite directions before anything
went red.

Not fixed here: lane L3 found it while measuring reflection retirements, the
fix is in `capture_throwable_trace` (`lang_misc.rs`) and belongs with N2's
owner, and a change to stack-trace capture wants the whole gate set and the
arms behind it rather than a ride on a reflection wave.

**So the price of the largest zero-cost cell in `H14-3` is one missing frame
skip in `capture_throwable_trace`.** Fix that and 906 registrations over 62
classes become retirable; leave it and the retirement breaks the top frame of
every exception in the VM, which the 105-vector corpus does not check even once.

### 3c. The registrar's stated premise has expired

Its call site says it is for *synthetic-stub* Throwable subclasses — a
`catch (Throwable t)` whose `t` is a fabricated stub with no bytecode behind
`getMessage`. MEASURED, `--dump-native-registry --explain-jdk-only` in
**Compatible** mode (`schema 5`, `mode: compatible`, 1693 synthetic-stub
natives present): `image_has_class: true` for **every** registration on
`java/lang/Throwable`, `java/lang/AssertionError`,
`java/lang/NoClassDefFoundError`, `java/text/ParseException` and the rest.
When a real JDK is on the class path the fabrication path is not taken, in
*either* mode. The premise holds only for the `synthetic-jdk` feature build,
where the class library is absent by construction.

This does **not** license deleting the registrar — §3 already forbids that —
but it does mean the correct eventual shape is the one `H22-1` used for
`register_p64_hex_format`: **not reachable from the shipping path, still
reachable from the synthetic arm.**

## 4. The scope gap that produced both surprises

`H14-3` §6 says the attribution to a registrar "is only as tight as the class
list". MEASURED, how tight it actually was:

| registrar | classes `H14-3` armed | classes the registrar registers | registrations priced | registrations a retirement deletes |
|---|---:|---:|---:|---:|
| `register_string_builder_natives` | 2 | **3** | 128 | **192** |
| `register_throwable_subclass_natives` | 26 | **62** | ~390 | **906** |

Both gaps were invisible from the shadow CSV, because the shadow population is
what the *corpus observed* — and the corpus never observes
`java/lang/AbstractStringBuilder` (0 dispatches, §2b) or 36 of the 62 exception
classes. **The class list for pricing a retirement must come from the
registry dump, not from the rows.**

`H14-3`'s headline "174 rows, 12.4% of the defect, for nothing" is built on
five cells. One (`HexFormat`) is confirmed and landed. Two are refuted here.
The remaining two — `ArrayDeque` (31) and `Optional` (20) — live in
`native-collections/**`, which this lane was forbidden to touch and did not
price; **they have the same scope question open** and nobody has answered it.

## 5. What this does NOT establish

* **No source change was made for either family.** The two negative results
  are measurements of the *dial*, which `H14-3` §6 correctly calls **strictly
  stronger** than deleting one registrar (it suppresses every native on the
  armed class, from every registrar). For §3 that gap is closed by inspection:
  `register_throwable_subclass_natives` is the `owns_slot: true` registrant of
  `getStackTrace` on those classes, so the deletion and the arm agree on the
  triple that regressed. For §2 it is closed by the exception's own stack: the
  frames named are `StringBuilder.append` → `AbstractStringBuilder.append` →
  `appendChars`, all triples this registrar owns.
* **The probe is one thread, one process, no reflection, no JIT tier-up
  pressure.** `H12`'s three-door result means a method that is hot enough may
  bind through a door this probe never opened.
* **`fillInStackTrace` was not fixed and the fix was not attempted.** §3a is a
  diagnosis, not a patch. `capture_throwable_trace` is in this lane's paths;
  the reason it is untouched is that this lane may not build, and a blind
  change to stack-trace capture for every exception in the VM is not a change
  worth landing unverified.
* **`setStackTrace(null)` is wrong on both paths** and is not explained here.
* **Nothing was run under `SUITE=all` or `SUITE=core`.** This lane took no
  `regression-suite/run.sh` invocation at all — see `H22-3` §5 for why that was
  the right call and what it costs.

## 6. NOMINATIONS

* **N1 — strike `H14-3` N1 and N2 from the plan, and re-rank.** They are the
  two largest items on it and they are 42 + 57 = 99 of its 174 rows. What
  remains measured-free is `HexFormat` (24, landed) and, at the *dial* level
  only, `ArrayDeque` (31) and `Optional` (20) — neither re-priced on its full
  class list.
* **N2 — fix the frame skip in `capture_throwable_trace`** (`lang_misc.rs`), so
  that a trace captured from inside `Throwable.<init>`/`fillInStackTrace(I)`
  drops the constructor frames. Then re-run arm 6. If it comes back at 4
  differing lines (the `setStackTrace(null)` pair), **906 registrations over 62
  classes become retirable in one commit** and it is the largest single item in
  the population. The arm is one env var and 20 seconds; the fix is the work.
* **N3 — before ANY partial `StringBuilder` retirement, run the §2b census
  prediction.** If the rows migrate to `AbstractStringBuilder` rather than
  vanishing, the partial buys a smaller census win than its row count and
  should be ranked accordingly.
* **N4 — the `StringBuilder` object model is the real item.** A `char[]`/`int`
  builder against JDK 25 compact strings is not retirable at any granularity
  until the layout matches. This belongs on the P0 table beside the collection
  cluster, not on a retirement list.
* **N5 — re-price `ArrayDeque` and `Optional` on their registry class lists**
  before landing either. `native-collections/**` was outside this lane's paths.
* **N6 — put the probe in the tree.** It is 517 assertions over three families
  the corpus barely touches, it found three defects unarmed and two blocking
  results armed, and it exists only because this lane typed it into a
  scratchpad. `regression-suite/probes/` is where its neighbours live.

## 7. `INDEX.md` row

```markdown
- [H22-2](H22-2-two-of-the-five-free-retirements-are-not-free-20260821.md) — `OPEN` · **MEASURED, 8 differential arms + 1 exact-invocation census.** `H14-3`'s two largest free retirements are not free, and a 517-assertion probe against HotSpot 25.0.3+9 says so where 104 vectors said nothing. **`register_string_builder_natives` is FATAL**: armed on the 3 classes it actually registers, the VM exits `rc=1` on the first `append(char[])` with `ArrayStoreException: can not copy byte[] into char[]` — JDK 25's compact-strings `AbstractStringBuilder` cannot run against CratonVM's two-field `char[]`/`int` builder, which `native_override.rs:5129` already says out loud. Each half is free ALONE (`StringBuilder`+`StringBuffer` = 0, `AbstractStringBuilder` = 0), so this is also the first measured counter-example to `H14-3` §6's untested "their sum is not the cost of arming several". **`register_throwable_subclass_natives` costs 61 of 61 stack traces**: armed, every `getStackTrace()[0]` becomes `<init>` or `fillInStackTrace`, because the retired path routes capture through `Throwable.fillInStackTrace(I)` from *inside* the constructor frames and `capture_throwable_trace` skips none of them — one missing frame skip standing between the plan and 906 registrations. Everything else in that arm is byte-identical, and it FIXES `TypeNotPresentException.getMessage`. §4 is the method finding: `H14-3` armed 2 of 3 and 26 of 62 classes, because a shadow CSV lists what the corpus observed and `AbstractStringBuilder` is observed 0 times (`H11-1`, with positive control).
```

---

## INDEPENDENT CHECK (lane H0, 2026-08-21) — the conclusion holds; the failure MODE is worse than reported

This record's structural finding — **each half is free alone, together they
break** — is **CONFIRMED**, isolated to the exact pair. But the mode I measure is
not an exception. It is **silent total data loss**.

Probe: `new StringBuilder()`, `append("x")`, print, `append(new char[]{'a','b','c'})`,
print. Oracle HotSpot 25.0.3+9 gives `x` then `xabc`.

| armed prefixes | `append(String)` | `append(char[])` | rc |
|---|---|---|---:|
| *(unarmed control)* | `x` | `xabc` | 0 |
| `java/lang/StringBuilder` alone | `x` | `xabc` | 0 |
| `java/lang/AbstractStringBuilder` alone | `x` | `xabc` | 0 |
| **both together** | **`` (empty)** | **`` (empty)** | **0** |
| all three incl. `StringBuffer` | **`` (empty)** | **`` (empty)** | **0** |

**Every append is discarded and `toString()` returns empty, with no exception
and exit code 0.**

### Why this is worse than the `rc=1` this record reports

An `ArrayStoreException` is loud: it stops the program at the defect. **A
`StringBuilder` that silently accepts every append and returns the empty string
is the single worst shape a defect can have in `java.lang`** — every log line,
every generated message, every `String.join`, every `toString()` in the process
becomes empty, and **nothing throws**. A test that asserts "no exception" passes.
A test that prints a result prints nothing and may still be scored on its exit
code.

This does not contradict the record: both are real, and the difference is almost
certainly which append overload the probe drives — this record's hits the
`byte[]`/`char[]` compact-string copy, mine hits a path that no-ops. **Two
probes, two modes, one cause.** It does mean the row should not be summarised as
"it crashes", because the shape a reader plans against changes the priority.

### Ruled out: this is NOT a regression from `H18`

The obvious suspect was `H18-1`, which landed changes in
`vm/src/runtime/interpreter/typecheck.rs` between the two binaries and could
plausibly have converted a failing array-store type check into a passing one —
turning a crash into silence.

**Tested and DISPROVED.** Identical arming, identical probe:

```
cratonvm-r5.exe  (pre-H18)   append(String) ok:      append(char[]) ok:
cratonvm-r6.exe  (post-H18)  append(String) ok:      append(char[]) ok:
```

Byte-identical. The silence predates `H18` and belongs to the arming, not to any
recent change. Recorded because the hypothesis was cheap, plausible, and would
have been a serious accusation to leave standing untested.

### What this adds to the plan

`H14-3` priced `StringBuilder`/`StringBuffer` at **zero vectors** and this record
explains why: the corpus never checks the *content* of a built string under the
dial. **A zero-cost cell measured over a corpus that does not assert the value
is not evidence that the value is right** — which is this directory's oldest
standing lesson (*a green gate is evidence about the question it asked*) landing
on the newest instrument.
