# Lane 2 — `java.lang` values, `System`, `java.math`: RETIRED 2026-09-11

**Status: RETIRED.** Every bucket-A/B row in the prefix set has a disposition,
which is what [`lane-0-integration-and-gates.md`](../../known-issues/jdk-only-lanes/lane-0-integration-and-gates.md)
§8 asks for. Two waves, both landed on `dev`.

```text
  the lane's population            390 shadows over 57 classes
    RETIRED                         50
    NOT SHADOWS (reclassified)       9
    BLOCKED, blocker named         331
```

The lane's §3 structural dependency — the `System` property store being the
authority rather than a cache — is recorded below and is unchanged.

**The two most useful things here are not retirements.** Both waves were held up
by a defect in code the shadows were standing in front of, and in both cases the
shadow was why nobody had seen it.

---

## 1. The lane was 390 rows, not 434, and the carve-out rule is the reason

Lane 0's table said 434/68/303. Measured from `--dump-native-registry
--explain-jdk-only`, the prefix set holds **992** live bucket-A/B `Bridge` rows
over 99 classes, and most of the excess is lane T's.

Lane 0 §3's test is precise: `register_throwable_subclass_natives` is lane T's
because its 61 classes **span seven lanes' prefixes**. So the carve-out is
*spans more than one LANE*, not *more than one CLASS* — and the difference is
212 rows:

```text
  rows under L2's prefix set                992  over 99 classes, 322 sites
    registrar crosses a LANE boundary (T's) 602  over 48 classes,  43 sites
    registrar stays inside L2               390  over 57 classes, 279 sites

  the same split by "more than one CLASS" would hand lane T
  StringBuilder + AbstractStringBuilder (123) and System$1 (28) — two-class
  registrars wholly inside this prefix set, and both claimed by this page.
```

The per-class shape reproduces the original §1 table exactly — 62
`StringBuilder`, 61 `AbstractStringBuilder`, 28 `System$1`, 24 `BigInteger`, 23
`System`, 21 `AssertionError` — which is what says the funnel matches the one
lane 0 counted with.

**`java/lang/foreign/` (61 rows) is this lane's by the prefix rule and L4's by
subject.** Dispositioned here rather than handed over, because the disposition
was already decided elsewhere and moving the rows would not change it.

## 2. Wave 1 — 13 rows, and a defect the census is built so it cannot show you

`java/lang/Character`'s three deprecated statics and ten value-shaped
`BigInteger` rows.

`retired_shadow.rs` recorded `java/math/` costing one corpus vector of 36 on the
2026-08-19 package screen, which is why the prefix was never admitted. Re-taken
per CLASS, the cost is `BigInteger`'s and the vector is `RJdkSecurity`, asserting
**`2^127-1 must be prime`**.

Chasing it one layer at a time:

```text
  (2^127-2) >> 1   HotSpot  85070591730234615865843651857942052863
                   yielded  85070591730234615865843651853647085568   <- short by 2^32
```

The receiver's `mag[]` is byte-identical to HotSpot's going in and the top limb
of the result is zero. A pure-bytecode reproduction of the JDK's shift loop is
byte-identical on this VM, so the loop is right and its callee is not: JDK 25
does that shift in `shiftRightImplWorker`, an `@IntrinsicCandidate` that
`native-builtins/src/biginteger_intrinsics.rs` registers a native over.

**The census cannot see it.** That registration is `NativeKind::Intrinsic`, and
an `Intrinsic` is exempt at every dispatch door and exempt from the shadow census
*by construction*. The native blocking this lane was, by the instrument's own
design, invisible to the lane. Reflection is the only thing that can ask it,
because the public method is shadowed by a *different* native
(`apps/probes/L2IntrinsicProbe.java`):

```text
  shiftRightImplWorker   16 rows differ from HotSpot 25.0.4+7
  shiftLeftImplWorker    17
  implSquareToLen         0
  implMulAdd              0
  mulAdd                  0
                         --
                         33 of 60, every one the top word left at zero
```

Both loops ran `numIter - 1` times where OpenJDK runs `numIter`, and both
returned early for `numIter < 2` where OpenJDK does one pass for `numIter == 1`.
The left worker carried a comment asserting OpenJDK "does NOT write the final
word … So we iterate `numIter - 1` times like the JDK" — true of its one caller,
which passes `len - 1` so it can patch the last word itself, and false of the
worker. The subtraction was applied twice, and the tests froze it: tests 6 and 7
rebuilt their expectations from the same expression the implementation used, so
they agreed with it by construction.

Fixed, tests re-keyed to the JDK contract with new cases pinned to measured
HotSpot rows. `RJdkSecurity` passes with `BigInteger` retired: the 2026-08-19
screen result was this defect all along.

### Fourteen `BigInteger` rows are held, and the held set is a RULE

```text
  BigIntegerSweep   control (no table, JIT on)    0 differing lines
                    trial   (table,    JIT on)   18   = 9 rows
                    trial   (table,   --nojit)    6   = 3 rows
```

**`add`, `subtract`, `multiply` — a SURVIVOR, and the retirement was inert.**
All three are registered twice: `phases_late.rs` owns the slot as a `Bridge`,
and `math_bignum.rs` registered an `Intrinsic` first which owns no slot and is
*dead in compatible mode*. Refusing the `Bridge` under `--jdk-only` does not
reach bytecode — it wakes the dead loser, which returns **null** for a null
argument where the real body throws. 3 of 27 refusals carried a survivor. The
probe rows moved, so the wave *looked* measurable and retired nothing.

**The rest — the JIT dropped the helpful NPE message. BLOCKER CLEARED
2026-09-11, and the rows still do not move.** Interpreted they were
HotSpot-exact; once the body was JIT-compiled the `NullPointerException` arrived
with no message at all. Fixed by `0d013f359` and retired as
[`../fixed-bugs/the-helpful-npe-message-is-lost-in-compiled-code-FIXED-20260911.md`](../fixed-bugs/the-helpful-npe-message-is-lost-in-compiled-code-FIXED-20260911.md),
pinned by `vm/tests/jit_npe_message_hot_equals_cold.rs`.

Clearing the blocker is not the same as retiring the rows, and the JIT lane said
so in this page before it moved: **the hold is STRUCTURAL** — every
reference-argument row, not the six that happened to regress — so lifting it owes
the same `BigIntegerSweep` measurement with the JIT on that put it there. This
lane did not re-take it, so the 14 rows stay held and the tally in §5 stands.

**Which rows show it moves between runs**, so the hold is structural rather than
copied out of one diff: six rows regressed on the 24-triple binary, and
`modInverse`/`modPow` regressed instead on the 13-triple one — the same defect on
different rows, because which bodies the JIT has compiled by the time the probe's
null section runs is not fixed. **A row is exposed if its real body can
dereference a null reference ARGUMENT**, so wave 1 retires only signatures that
take none, and the guard asserts that over the TABLE rather than over a list of
names.

## 3. Wave 2 — 37 rows, and a defect the shadows were hiding

The families a corpus screen called clean and nothing else had measured:
`java.lang.Object`, `Package`, `StringUTF16`, `Throwable.initCause`, five
throwable constructor sets, and `java.lang.management`'s factories.

The corpus cannot adjudicate these, so the image was asked two questions it
cannot: **is the shadowed method reachable at all**, and **does the real body
reach an ACC_NATIVE method this VM does not register**. Nine rows failed the
first (§5); none failed the second.

The other 37 went into a trial binary measured against a control built from the
same tree. It **fixed four rows** — `new MemoryUsage(1,5,3,4)` and `(1,-2,3,4)`
now throw `IllegalArgumentException` instead of accepting used > committed and a
negative size, and both `getPlatformMXBean` rows start behaving — and **broke
eight**: every throwable constructor lost its stack-trace top frame.

```text
  new VirtualMachineError() {}   HotSpot   depth=1  L2FrameProbe.main
                                 control   depth=2  L2FrameProbe$1.<init> / main
                                 trial     depth=3  java.lang.VirtualMachineError.<init> / ...
```

**The CONTROL row is the finding.** `depth=2` before any retirement means this
was live on `dev`: `capture_throwable_stack_trace` took the throwable and never
used it, storing the raw stack, constructor frames and all. The
throwable-constructor shadows run *instead of* `<init>`, so they push no
constructor frame and there is nothing to trim — but a subclass whose own
constructor is ordinary bytecode already got a contaminated trace. The shadow
was hiding a defect in the very bytecode it shadowed.

Fixed to HotSpot's own rule: skip a leading `<init>` frame while **the throwable
is an instance of that frame's declaring class**. The obvious simplification —
skip any leading `<init>` of a `Throwable` subclass — is wrong in a case the JDK
gets right:

```text
  class Bad extends RuntimeException { Bad() { throw new IllegalStateException(); } }
```

the `IllegalStateException` is not a `Bad`, so the JDK stops there and keeps
`Bad.<init>`; the simplification would delete it and point at `main`.

With the fix the frame probe is **0-diff** and the family probe goes 12 → 4.

## 4. Blocked, with the blocker named — 331 rows

Every arm is `CRATONVM_ENFORCE_NATIVE_SHADOW` over the 40-vector `--jdk-only`
corpus, against a 40/40 baseline taken on the same binary.

| family | rows | corpus armed | blocker |
|---|---:|---|---|
| `StringBuilder` + `AbstractStringBuilder` | 123 | **40/0** | the JIT intrinsic door, and cost |
| `java/lang/System` | 23 | 38/2 `RJdkSecurity` `RJdkProxyIface` | the property store is the authority |
| `java/lang/System$1` | 28 | 39/1 `RJdkProxyIface` | hidden-class `defineClass0` |
| `java/lang/foreign/` | 61 | 39/1 `RJdkForeign` | the FFM carrier is the VM's own shape |
| `java/lang/ref/` | 35 | **40/0** | reference discovery — see the warning |
| `java/lang/SecurityManager` | 13 | **40/0** | the exec/Panama security model |
| `StackWalker` + `StackFrameInfo` + `StackTraceElement` | 23 | 38/2 `RJdkReflect` `RJdkLogging` | frame-walk state |
| `java/lang/Runtime` + `Shutdown` | 11 | 39/1 `RJdkJni` | native library loading |
| `java/math/BigInteger`, held per-triple | 14 | — | survivor / JIT NPE (§2) |

**Four of those arms are corpus-clean and three are blocked anyway.** That is the
most important line on this page. `java/lang/ref/` is the campaign's own worked
example: `retired_shadow.rs` records it passing the 36-vector screen in August
and being rejected by the 102-vector ARM on `RClassUnloadSweep{,Gen}`, and this
lane's 40-vector screen reproduces the same false clean three weeks later.
**A clean arm means the corpus did not ask.**

- **`StringBuilder` (123).** Correctness is free — measured 2026-08-28 and
  reproduced here at 40/0 — and the retirement is still declined.
  `java/lang/StringBuilder` is the class the interpreter's
  `JitIntrinsic::StringBuilder*` door keys on, so a retirement has two moving
  parts, and the measured cost of yielding was 2.0x-3.4x on five of six shapes.
  A lane that wants these rows needs a JIT intrinsic for the bytecode path first.
- **`java/lang/ref/` (35).** `Reference.<init>` and the three subclass
  constructors are the VM's only call to `NativeContext::discover_reference`; a
  reference whose constructor yields is never discovered and never cleared.
  Retiring only the accessors is not an escape hatch — `Reference.get()`'s SATB
  keep-alive, `SoftReference.get()`'s LRU touch and `Reference.enqueue()`'s
  manual-enqueue mark have no bytecode equivalent.
  `the_reference_subsystem_stays_whole` is the guard.
- **`java/lang/foreign/` (61).** The carrier is this VM's own allocation shape,
  decided 2026-08-29 on five independent signals; Phase 2 measured `ArenaImpl`
  armed taking `FfmSegmentSweep` from 40 diffs to 181 *and* killing it at row 18
  of 199. `RJdkForeign` is the same wall at corpus scale.
- **`java/lang/SecurityManager` (13).** `security_manager.rs` states it: the
  installed-manager check gates exec and Panama, and "the two cannot be
  separated, which is why this is a security-model decision and not a stub to
  remove".
- **`java/lang/System` (23) — the lane's §3 structural dependency, unchanged.**
  The property natives cannot be retired while the Rust store is the *authority*
  rather than a cache of the object. `replace_real_map` landed 2026-09-09 and is
  the precondition, not the inversion.
- **`java/lang/System$1` (28), and this is new.** This page used to call it the
  best value in the lane, on the strength of the carrier having all 88 members
  with bytecode. That is still true, and every ACC_NATIVE method the carrier's
  bodies delegate to is registered here — checked first, 9 of 9. It fails
  elsewhere: real `System$1.defineClass` calls `ClassLoader.defineClass0` with
  `initialize = true` and the hidden-class flags, and this VM cannot initialize
  the resulting `MethodHandleProxies` class (`7 of 9 steps failed`). That error
  named no exception — it printed a raw heap pointer — so this wave also fixed
  the diagnostic in `vm/src/vm/vm_exec.rs`.

## 5. Nine rows are not shadows, and the distinction cost a correction

Bucket B is "inherits `Code` from a supertype". **A constructor is never
inherited in Java**, so an `<init>` row resolved by walking up the hierarchy is a
phantom: the resolver finds `Object.<init>()V` and reports a target nothing can
dispatch. Retiring one trades a shadow for a `NoSuchMethodError`.

```text
  ClassLoadingMXBean, CompilationMXBean, GarbageCollectorMXBean,
  MemoryMXBean, PlatformLoggingMXBean, RuntimeMXBean   <init>()V   INTERFACES
  ManagementFactory.<init>()V                                      private
  ManagementFactory.loadNativeLib()V                               private
  MemoryUsage.<init>()V                          the class declares (JJJJ)V only
```

**And the correction.** The first pass of that funnel asked `javap` about the
RECEIVER class only, so it also excluded `Package.equals` and
`ExceptionInInitializerError.initCause` as "no such signature". Those resolve to
`Object.equals` and `Throwable.initCause`, both concrete — **genuine bucket-B
shadows**, and wave 2 retires them. The rule that separates the two cases is the
one above: a constructor is never inherited, an ordinary method is.

Campaign-wide the same query finds **23 phantom-constructor rows across 21
classes** of 1719 bucket-B rows. Small, and concentrated on exactly the rows
where retiring is a defect.

## 6. What this lane hands on

- ~~**The JIT drops JEP 358's helpful NPE message** in compiled code.~~ FIXED
  2026-09-11 by `0d013f359` (`probes/L2JitNpeProbe.java`, now six shapes, and
  `vm/tests/jit_npe_message_hot_equals_cold.rs`). The 14 `BigInteger` rows it
  blocked are still held: the rule is "no reference parameter", which was
  derived from rows MOVING between runs, and nobody has re-measured with the
  defect gone. That re-measurement is the whole of what is left here.
- **Five `java/util/logging` triples are INERT.** The full refusal report on the
  final binary reads 2029 refusals, 7 rows / 5 distinct triples with a survivor,
  all `java/util/logging`, none in lane 2 — an older `phases_early.rs` intrinsic
  still owns the slot after the 2026-08-11 retirement refuses the bridge, so the
  real bytecode never runs. Same shape as this lane's `BigInteger` survivor, in
  an already-landed wave.
- **`Package.getPackages()` returns empty**, on the control and after the
  retirement alike — so it is neither caused nor fixed here. 2 probe rows.
- **`the_drift_baseline_has_no_stale_rows` is red on `origin/dev`**, from lane
  0's `Class.getModule` `Intrinsic` re-tag. Attributed and recorded in
  `bug-two-drift-gates-are-red-on-pristine-dev-from-a-class-parameterised-registrar-20260822.md`.
- **Lane 0's L2 row** should read 390/57/279.

## 7. Acceptance

Wave 1, binary `3defefd5fbbe0d6d`; wave 2, `cratonvm-w2final` against control
`fdea7aefa38ab3eb` — same tree, the table and the trace fix the only variables.

```text
  refusal survivors        0 on every retired triple, both waves
  BigIntegerSweep          13255 rows, 0 differing lines
  L2IntrinsicProbe         0        (66 before the worker fix)
  L2FrameProbe             0        (control 2 — the pre-existing defect)
  L2Wave2Probe             4        (control 12; the 4 are Package.getPackages)
  whole probe tree         122 measured, 0 worse, 1 better
  CRATONVM_ARGS=--jdk-only  40 passed, 0 failed
  SUITE=all                132 passed, 0 failed
  SUITE=core                92 passed, 0 failed
```

The probe tree was re-taken: the first attempt ran while the host carried load
average 221 on 8 cores with 1G free, and three of its four moved rows had an
EMPTY HotSpot arm — `d(hs,trial)=0` with both files empty reads as a perfect
score. Re-run on a calm host with every arm complete, the one WORSE was refuted
(`PropertiesShadowSweep` 184/184/184, delta 0).
