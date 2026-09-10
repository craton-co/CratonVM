# Lane 2 — `java.lang` values, `System`, `java.math`

**Status: OPEN, and the numbers below are measured rather than planned.**
Wave 1 landed 2026-09-10: **13 rows retired, 14 held per-triple with a named
cause, 322 blocked with a named blocker, 7 reclassified as not-shadows, and 41
still holding only a corpus screen.** The lane cannot retire until those 41 have
a disposition; §9 says exactly what each needs.

Read [`lane-0-integration-and-gates.md`](lane-0-integration-and-gates.md) §2-§6 first. Method, preconditions and
landing protocol: [`../jdk-only-lane-operations.md`](../../contributing/jdk-only-lane-operations.md).

---

## 1. The lane is 390 rows over 57 classes, not 434 over 68

Lane 0's table says 434/68/303. Measured 2026-09-10 from
`--dump-native-registry --explain-jdk-only` on `7a8b79526`, the prefix set holds
**992** live bucket-A/B `Bridge` rows over 99 classes — and most of that excess
is lane T's, not this lane's.

Lane 0 §3 says a cross-cutting registrar belongs to lane T whole. The test it
gives is precise: `register_throwable_subclass_natives` is lane T's because its
61 classes **span seven lanes' prefixes**. So the carve-out is *spans more than
one lane*, and NOT *more than one class* — a registrar parameterised over two
classes that are both L2's crosses no boundary. Applying the wrong one of those
two rules moves 212 rows:

```text
  L2 rows under the prefix set                992  over 99 classes, 322 sites
    registrar crosses a LANE boundary (T's)   602  over 48 classes,  43 sites
    registrar stays inside L2 (this lane)     390  over 57 classes, 279 sites

  the same split done by "more than one CLASS"
    ...would hand lane T 813 rows, including
       StringBuilder + AbstractStringBuilder (123) and System$1 (28)
```

`register_string_builder_natives(registry, class)` is called for
`AbstractStringBuilder` and `StringBuilder`; the `JavaLangAccess` registrar loops
over `System$1` and `System$2`. Both are two-class registrars entirely inside
this prefix set, and §4 of the old version of this page claimed the first pair
explicitly as one unit. Neither is lane T's.

The per-class shape reproduces this page's original §1 table exactly — 62
`StringBuilder`, 61 `AbstractStringBuilder`, 28 `System$1`, 24 `BigInteger`, 23
`System`, 21 `AssertionError` — which is what says the funnel matches the one
lane 0 counted with.

**The "roughly 250 rows" this page used to estimate for the lane was low by
about 140**, and the missing families are the ones it never named:
`java/lang/foreign/` (61), `java/lang/ref/` (34), `java/lang/management/` (23)
and the `StackWalker` family (23).

### `java/lang/foreign/` is this lane's by the prefix rule, and it is L4's by subject

Lane 0 gives L4 `jdk/internal/foreign`; the public `java/lang/foreign/` API falls
to "java/lang/ remainder", which is here. 61 rows is 16% of the lane. They are
dispositioned in §5 rather than handed over, because the disposition was already
decided elsewhere and moving them would not change it.

## 2. Wave 1 — 13 rows retired

`RETIRED_SHADOW_L2_TRIPLES` in `native-api/src/retired_shadow.rs`.

| class | retired | held | why the holds |
|---|---:|---:|---|
| `java/lang/Character` | 3 | 0 | — |
| `java/math/BigInteger` | 10 | 14 | §4 |

**`Character`**: `isJavaLetter`, `isJavaLetterOrDigit`, `isSpace`. The first two
are one-line delegations to `isJavaIdentifierStart` / `isJavaIdentifierPart`,
which carry no native and so already run real bytecode; `isSpace` calls nothing
at all. Armed alone over the 40-vector `--jdk-only` corpus: 40 passed, 0 failed.

**`BigInteger`**: 10 of 24 — every row whose signature takes no reference
parameter. `BigIntegerSweep` is 13253 rows and 0 diffs with those 10 retired,
and the refusal report reads **18 refusals, 0 with a survivor**, so none of them
is inert.

## 3. The finding: two `@IntrinsicCandidate` workers ran one iteration short

This is the wave's real content, and the lane only reached it because a
retirement made it visible.

`retired_shadow.rs` recorded `java/math/` costing one corpus vector of 36 on the
2026-08-19 package screen, which is why the prefix was never admitted. Re-taken
per CLASS on 2026-09-10, the cost is `BigInteger`'s and the vector is
`RJdkSecurity`, asserting **`2^127-1 must be prime`**.

Chasing it down one layer at a time:

```text
  (2^127-2) >> 1   HotSpot  85070591730234615865843651857942052863
                   yielded  85070591730234615865843651853647085568   <- short by 2^32
```

The receiver's `mag[]` is byte-identical to HotSpot's going in; the top limb of
the result comes back zero. A pure-bytecode reproduction of the JDK's shift loop
is byte-identical on this VM, so the loop is right and its callee is not: JDK 25
does that shift in `shiftRightImplWorker`, an `@IntrinsicCandidate` that
`native-builtins/src/biginteger_intrinsics.rs` registers a native over.

**The census cannot see it.** That registration is `NativeKind::Intrinsic`, and
an `Intrinsic` is exempt at every dispatch door and exempt from the shadow
census by construction. The native blocking this lane's retirement was, by the
instrument's own design, invisible to the lane. Nothing in the funnel would ever
have named it.

Calling all five registered intrinsics reflectively
(`apps/probes/L2IntrinsicProbe.java` — reflection is the only instrument that
asks these and nothing else, because the public methods are shadowed by a
*different* native):

```text
  shiftRightImplWorker   16 of its rows differ from HotSpot 25.0.4+7
  shiftLeftImplWorker    17
  implSquareToLen         0
  implMulAdd              0
  mulAdd                  0
                         --
                         33 of 60 rows, every one the top word left at zero
```

Both loops ran `numIter - 1` times where OpenJDK runs `numIter`, and both
returned early for `numIter < 2` where OpenJDK does one pass for `numIter == 1`.
The left worker carried a comment asserting the JDK "does NOT write the final
word ... So we iterate `numIter - 1` times like the JDK" — true of its one
caller, which passes `len - 1` precisely so it can patch the last word itself,
and false of the worker. The subtraction was applied twice, and the tests froze
it: tests 6 and 7 rebuilt their expectations from the same expression the
implementation used, so they agreed with it by construction.

Fixed, with the tests re-keyed to the JDK contract and new ones pinned to
measured HotSpot rows rather than recomputed. `L2IntrinsicProbe` is now 0-diff,
and **`RJdkSecurity` passes with `BigInteger` retired**: the 2026-08-19 screen
result was this defect all along.

## 4. Nine `BigInteger` rows are held, for two different reasons

With all 24 retired the probe-tree A/B on two binaries — control
`db988f6a76365f1f` without the table, trial `06307f0eca53c714` with it, same tree
otherwise — reads:

```text
  BigIntegerSweep   control (no table, JIT on)    0 differing lines
                    trial   (table,    JIT on)   18   = 9 rows
                    trial   (table,   --nojit)    6   = 3 rows
```

A positive delta is the one result that is a reason not to retire, so the
affected rows are held per-TRIPLE — the instrument the `java/util/logging` wave
used for `Logger.log`'s eighth overload.

**`add`, `subtract`, `multiply` — a SURVIVOR, and the retirement was inert.**
All three are registered twice: `phases_late.rs` owns the slot as a `Bridge`,
and `math_bignum.rs` registered an `Intrinsic` first which owns no slot and is
therefore *dead in compatible mode*. Refusing the `Bridge` under `--jdk-only`
does not reach bytecode — it wakes the dead loser, which returns **null** for a
null argument where the real body throws.

```text
  --jdk-only-report, BigIntegerSweep run:
    27 synthetic-native-registered refusals on the retired classes
     3 of them carrying a survivor
```

The probe rows moved, so the wave *looked* measurable. `Zero survivors is the
answer you need` earned its place here. Retiring these three means removing the
dead `math_bignum.rs` registrations first, which changes nothing in compatible
mode because they own no slot.

**`remainder`, `mod`, `gcd`, `and`, `or`, `xor` — the JIT drops the message.**
These do reach bytecode and interpreted they are HotSpot-exact. Once the body is
JIT-compiled the `NullPointerException` arrives with no message at all. It is not
a `BigInteger` fact: `apps/probes/L2JitNpeProbe.java` asks five null-deref shapes
cold and hot with no JDK class involved, and every one loses its message when
hot. Filed as
[`../jit/the-helpful-npe-message-is-lost-in-compiled-code-20260910.md`](../jit/the-helpful-npe-message-is-lost-in-compiled-code-20260910.md).
### The held set is a RULE, because the affected rows move between runs

The six above are what regressed on the 24-triple binary. Rebuilt with those six
held, the sweep regressed on `modInverse` and `modPow` instead — the same defect
on different rows, because which bodies the JIT has compiled by the time the
probe's null section runs is not fixed between runs. Holding exactly the rows in
one run's diff would be freezing a coin flip, and would have read as a clean
wave twice in a row while the population it was chasing moved underneath it.

So the hold is structural: **a row is exposed if its real body can dereference a
null reference ARGUMENT**, and wave 1 retires only signatures that take none.
That is the twelve reference-taking methods plus the two `byte[]` constructors,
which the sweep never asks with null and so cannot vouch for either way. What
remains is ten value-shaped rows — `bitCount`, `bitLength`, `intValueExact`,
`isProbablePrime`, `longValueExact`, `not`, `shiftLeft`, `shiftRight`,
`testBit`, `toByteArray` — 0-diff over the whole sweep with the JIT on.
`the_l2_table_holds_only_what_lane_2_measured` asserts the rule over the table
rather than over a list of names, so a row added later has to satisfy it too.

**Any shadow retirement that moves a reference-argument method from a native to
real bytecode will meet this wall**, so it is a campaign-level blocker rather
than a lane-2 one.

## 5. Blocked, with the blocker named — 322 rows

Every arm below is `CRATONVM_ENFORCE_NATIVE_SHADOW` over the 40-vector
`--jdk-only` corpus on the trial binary, against its own 40/40 baseline taken on
the same binary in the same conditions.

| family | rows | corpus armed | blocker |
|---|---:|---|---|
| `StringBuilder` + `AbstractStringBuilder` | 123 | **40/0** | the JIT intrinsic door, and cost |
| `java/lang/System` | 23 | 38/2 `RJdkSecurity` `RJdkProxyIface` | the property store is the authority |
| `java/lang/System$1` | 28 | 39/1 `RJdkProxyIface` | hidden-class `defineClass0` |
| `java/lang/foreign/` | 61 | 39/1 `RJdkForeign` | the FFM carrier is the VM's own shape |
| `java/lang/ref/` | 34 | **40/0** | reference discovery — and see the warning below |
| `java/lang/SecurityManager` | 13 | **40/0** | the exec/Panama security model |
| `StackWalker` + `StackFrameInfo` + `StackTraceElement` | 23 | 38/2 `RJdkReflect` `RJdkLogging` | frame-walk state |
| `java/lang/Runtime` + `Shutdown` | 10 | 39/1 `RJdkJni` | native library loading |
| — held per-triple (§4) | 14 | — | survivor / JIT NPE |

**Four of those nine arms are corpus-clean, and three of them are blocked
anyway.** That is the single most important line on this page. The corpus is not
a verdict, and `java/lang/ref/` is the worked example the campaign already
owns: `retired_shadow.rs`'s header records it passing the 36-vector screen
in 2026-08-19 and being rejected by the 102-vector ARM on
`RClassUnloadSweep{,Gen}`. My 40-vector screen reproduces the same false clean
three weeks later. **A clean arm here means the corpus did not ask.**

- **`StringBuilder` (123).** Correctness is free — measured 2026-08-28 and
  reproduced here at 40/0 — and the retirement is still declined. `java/lang/StringBuilder`
  is the class the interpreter's `JitIntrinsic::StringBuilder*` door keys on
  (`native-builtins/src/intrinsics/mod.rs`, verified present on `7a8b79526`), so
  a retirement has two moving parts, and the measured cost of yielding was
  2.0x-3.4x on five of six shapes. A lane that wants these rows needs a JIT
  intrinsic for the bytecode path first. Record:
  [`../jdk-only/l2-strings-residuals-the-migration-is-unpriced-20260828.md`](../jdk-only/l2-strings-residuals-the-migration-is-unpriced-20260828.md) N2.
- **`java/lang/ref/` (34).** `Reference.<init>` and the three subclass
  constructors are the VM's only call to `NativeContext::discover_reference`; a
  reference whose constructor yields is never discovered and never cleared.
  Retiring only the accessors is not an escape hatch — `Reference.get()`'s SATB
  keep-alive, `SoftReference.get()`'s LRU touch and `Reference.enqueue()`'s
  manual-enqueue mark have no bytecode equivalent. `the_reference_subsystem_stays_whole`
  is the guard, and it will fail anyone who tries.
- **`java/lang/foreign/` (61).** The carrier is this VM's own allocation shape,
  decided 2026-08-29 with five independent signals; and Phase 2 measured
  `ArenaImpl` armed taking `FfmSegmentSweep` from 40 diffs to 181 *and* killing
  it at row 18 of 199. `RJdkForeign` here is the same wall at corpus scale.
- **`java/lang/SecurityManager` (13).** `security_manager.rs` states it: the
  installed-manager check is what gates exec and Panama, and "the two cannot be
  separated, which is why this is a security-model decision and not a stub to
  remove".
- **`java/lang/System$1` (28), and this one is new.** This page used to call it
  the best value in the lane, on the strength of the carrier having all 88
  members with bytecode. That is still true, and every ACC_NATIVE method the
  carrier's bodies delegate to (`Class.getConstantPool`, `ClassLoader.defineClass0`
  / `defineClass1`, `Thread.currentCarrierThread`, …) is registered here, so the
  retirement does not trade a shadow for an `UnsatisfiedLinkError` — that was
  checked first, and it passed 9 of 9. It fails somewhere else:

  ```text
  ClassLoader.defineClass0(jdk/MHProxy1/RJdkProxyIface$Greeter/0x0) failed:
    initialize after define failed ... 7 of 9 steps failed
  ```

  Real `System$1.defineClass` calls `ClassLoader.defineClass0` with
  `initialize = true` and the hidden-class flags, and this VM's `defineClass0`
  cannot initialize the resulting `MethodHandleProxies` class. The error named
  no exception — it printed a raw heap pointer — so this wave also fixed that
  diagnostic (`vm/src/vm/vm_exec.rs`); the next reader gets the exception's class
  and message instead of `ref(0x7b5b041c3720)`.

## 6. Seven rows are not shadows at all, and the campaign's bucket B is inflated

Bucket B is "inherits `Code` from a supertype". A **constructor is never
inherited in Java**, so a `<init>` row resolved by walking up the hierarchy is a
phantom: the image resolver finds `Object.<init>()V` and reports a target that
cannot be dispatched.

All seven of this lane's bucket-B `<init>` rows are that:

```text
  java/lang/management/ClassLoadingMXBean.<init>()V       interface, no constructor
  ...CompilationMXBean, GarbageCollectorMXBean,
     MemoryMXBean, PlatformLoggingMXBean, RuntimeMXBean    all interfaces
  java/lang/management/MemoryUsage.<init>()V               class HAS constructors,
                                                           but (JJJJ)V and (CompositeData)V
```

Retiring one would trade a shadow for a `NoSuchMethodError`, which is the
`Logger.log` eighth-overload shape. They belong in bucket C/F.

Campaign-wide the same query finds **23 phantom-constructor rows across 21
classes** of 1719 bucket-B rows — 1.3%, and concentrated on exactly the rows
where retiring is a defect. Small, but it should come off lane 0's bucket-B
count rather than be discovered once per lane.

## 7. What lane 0's table should say

`lane-0-integration-and-gates.md` §2 gives L2 `434 / 68 / 303`. Measured on
`7a8b79526` with lane T carved out by the lane rule rather than the class rule,
it is **390 / 57 / 279**. The campaign total moved the same way and for the same
kind of reason: §1's table says 5,549 and the same dump says **5,584**. Neither
is a correction of the other's method — three weeks of `dev` separate them — but
a lane should re-derive its own row rather than plan against the table's.

## 8. Traps this lane confirmed

- **`System.out`/`System.err`** — unchanged advice, and the `System` arm's
  `RJdkSecurity` failure is a reminder that this class's blast radius is the
  whole suite.
- **A comment saying the VM lacks a capability outlives the day it was built.**
  Confirmed twice here, both in `biginteger_intrinsics.rs`: the left worker's
  "matches OpenJDK 25's intrinsic exactly" comment was wrong, and the test below
  it encoded the same wrong belief.
- **`java/lang/Throwable` itself** is yours, its 61 subclasses are lane T's,
  and the two frame-walk APIs still order results oppositely.

## 9. What is left: 41 rows with a corpus screen and no disposition

These arms are 40/0 and have **no** independent record behind them, so they are
neither retired nor blocked, and this page cannot retire until they are:

| family | rows | what it needs |
|---|---:|---|
| `java/lang/management/` less the 7 phantoms | 16 | `ManagementFactory`'s 11 factories hand back VM MXBean carriers; the question is the FFM one — is the carrier a stand-in or the VM's own shape? |
| `java/lang/Object` | 6 | `wait`/`equals`/`toString`/`finalize`/`<init>`: identity and monitor primitives; needs a probe, not an arm |
| `vmerrors` (`VirtualMachineError`, `ExceptionInInitializerError`, `IllegalThreadStateException`, `UnsatisfiedLinkError`, `NullPointerException`) | 12 | constructor tables; check against lane T's registrar first |
| `java/lang/Package`, `StringUTF16`, `Throwable.initCause` | 5 | one probe each |
| `java/lang/Runtime$Version` | 2 | rides with `Runtime` |

Each needs the same treatment `BigInteger` got and the corpus cannot give: a
trial binary carrying the triples, the whole probe tree scored against a control
binary, and the refusal-survivor column read before any probe row. Budget one
build per wave.

Two campaign-level blockers stand in front of some of them:
**the JIT helpful-NPE gap** (§4) applies to every reference-argument method, and
**the `Intrinsic` exemption** (§3) means the funnel cannot show you the native
that will actually be in the way.

## 10. Acceptance, wave 1

Binary `09d7ad35d2deea4a` unless stated; control for the probe A/B is
`db988f6a76365f1f`, the same tree without the table.

```text
  refusal survivors                 18 refusals, 0 with a survivor
  BigIntegerSweep    13255 rows     0 differing lines   (control also 0)
  L2IntrinsicProbe      60 rows     0                   (was 66 before the fix)
  L2PrimeProbe         212 rows     0
  L2NpeProbe            22 rows     0
  CRATONVM_ARGS=--jdk-only          40 passed,   0 failed
  SUITE=all                        132 passed,   0 failed
  SUITE=core                        92 passed,   0 failed
```

`SUITE=all` at 132/0 also closes the live item of
[`../jdk-only/l2-strings-residuals-the-migration-is-unpriced-20260828.md`](../jdk-only/l2-strings-residuals-the-migration-is-unpriced-20260828.md)
§4: `RJdkEnumerations` was recorded there as "the last red on the `--jdk-only`
and `SUITE=all` arms that every lane has been writing off as known", and it
passes on both here.
