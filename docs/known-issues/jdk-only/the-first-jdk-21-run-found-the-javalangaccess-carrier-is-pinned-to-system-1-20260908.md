# The first JDK 21 strict-corpus run: `JavaLangAccess` is pinned to `java.lang.System$1`, which on 21 is a `PrivilegedAction`

**Status: OPEN — MEASURED 2026-09-08.** Windows 11, Temurin `21.0.12.1+1`
(downloaded for this run; this host had only JDK 25), release binary built from
`dev` @ `6502772c4`. Three arms: HotSpot 21 control, `--real-jdk`, `--jdk-only`.

> **VERIFIED AGAINST A BINARY 2026-09-08.** Every row on this page came off a
> real three-arm run, not a reading of the source. `scripts/jdk-only-strict-probes.sh`
> with `CV` = a release `cratonvm` built from `dev` @ `6502772c4` (timestamp
> checked against the build, and the same 16 sections came off a second binary
> built at `0d79de121`), `JAVA_HOME` = Temurin `21.0.12.1+1`:
> **16 divergent sections, 3 runs, `RESULT: PASS` at 16 observed / 16 baselined**,
> and `RESULT: FAIL` rc=5 naming the row when one baselined row is removed.
> The `NoSuchMethodError` texts in §1 are quoted from
> `logs/JdkOnlyPlatformProbe.strict.txt` and `JdkOnlyCensusLoadProbe.strict.txt`
> of that run. The `javap` method tables are from the two JDK images themselves.
>
> **This page proposes no fix**, so there is no fix to verify — §4 says so. The
> phrase "never been run" below is about the JDK 21 *image* never having been
> put through the corpus, not about an unverified repair.

**This is what minting the `21-windows` key bought.** Both JDK 21 legs of the CI
matrix had refused since the gate was written, for want of a baseline — so the
corpus had never been run against a 21 image at all, on any platform. It was
run once and found four defects.

---

## 1. The headline: a carrier class pinned by name to the wrong JDK's shape

Two of the four are the same defect. `--jdk-only` on JDK 21 dies with:

```text
java.lang.NoSuchMethodError: 'void java.lang.System$1.parkVirtualThread(long)'
    at jdk.internal.misc.VirtualThreads.park(VirtualThreads.java:67)
    at java.util.concurrent.locks.LockSupport.parkNanos(LockSupport.java:408)
java.lang.NoSuchMethodError: 'int java.lang.System$1.encodeASCII(char[], int, byte[], int, int)'
```

`java.lang.System$1` is the JDK's anonymous inner class implementing
`jdk.internal.access.JavaLangAccess` — **on JDK 25.** On JDK 21 it is a
different class entirely, and the `JavaLangAccess` implementation is `System$2`:

```text
JDK 25   class java.lang.System$1 implements jdk.internal.access.JavaLangAccess   (89 methods)
JDK 21   class java.lang.System$1 implements java.security.PrivilegedAction<Object>  (2 methods)
JDK 21   class java.lang.System$2 implements jdk.internal.access.JavaLangAccess   (87 methods)
```

and `System$2` is where the two missing methods live:

```text
  public int  encodeASCII(char[], int, byte[], int, int);
  public void parkVirtualThread();
  public void parkVirtualThread(long);
  public void unparkVirtualThread(java.lang.Thread);
```

CratonVM spells the carrier as a literal. `java/lang/System$1` appears **33
times** across the Rust tree — `shared_secrets_bridge.rs` (registrations,
factory, and its own contract assertions), `native-builtins/src/lib.rs`,
`classloader.rs`, `class_manager.rs`, `native_override.rs`, `vm_exec.rs`.
`java/lang/System$2` appears **zero** times.

So on JDK 21 every `JavaLangAccess` call routes to a `PrivilegedAction` that
has none of the 87 methods. The two seen here are simply the two these probes
exercise; the other 85 are reachable by any program that gets that far.

**Why `--jdk-only` only.** In compatible mode CratonVM's own natives answer
these calls and the carrier's identity never comes up. Strict mode drops the
natives and runs the real bytecode, which dispatches through `JavaLangAccess` —
and finds the name pinned to the wrong class. The `--real-jdk` arm has neither
row.

## 2. The other two

| # | probe/section | modes | what happens |
|---|---|---|---|
| 3 | `JdkOnlyBreadthProbe` / `serialization` | **BOTH** | `SECTION-FAILED serialization: java.lang.RuntimeException: java.io.InvalidClassException: java.util.ArrayList; unable to create instance` |
| 4 | `JdkOnlyBreadthProbe` / `textformat` | strict | grouping and decimal separators differ from the control: HotSpot 21 gives `df=1<nbsp>234,50`, CratonVM gives `df=1,234.50` |

> **#3 NARROWED 2026-09-09 — it is two symptoms, not one, and the other one is
> silent.** The section aborts at its first throw, so the `ArrayList` row above
> hid the rest of it: on JDK 21 `Integer`, `Long` and `Boolean` also round-trip
> wrongly, returning a bare `java.lang.Object` and throwing **nothing**. Both
> are one contract violation — the serialization constructor allocates the
> declaring superclass instead of the target — and whether it is loud or silent
> is decided only by whether that ancestor is abstract (`AbstractList` throws;
> `Object` does not). Reproducer, the 3x4 matrix, and two ruled-out mechanisms:
> jdk-21-serialization-round-trip-returns-the-wrong-class-20260909.md. Reading
> the row above as "one small divergence" understates it.
>
> **FIXED 2026-09-09.** The page moved to
> `docs/internal/retired/jdk-21-serialization-round-trip-returns-the-wrong-class-FIXED-20260909.md`.
> The mechanism was the accessor object itself: JDK 21 installs a
> run-time-generated `GeneratedSerializationConstructorAccessorN` (no
> fields at all), JDK 25 a `DirectConstructorHandleAccessor` (carries the
> target type), and the VM recognised only the latter. **It was NOT this
> page's carrier defect** -- the two 21 findings really are separate
> mechanisms, as section 6 of that page guessed. The `io` and `vthreads`
> rows below are still the carrier, and are still open.

#3 is mode-independent, so it is not a strict-mode question. #4 is
strict-only and **not** diagnosed here — the control's separators are the
locale's and CratonVM's are US-style, which is a lead about locale data, not a
finding.

> **#4 NARROWED 2026-09-09 — it is the locale DATA, not the default locale.**
> The lead above ("a lead about locale data") is now measured, and the other
> reading is ruled out: on the failing image `Locale.getDefault()` is `ru_RU`,
> `Locale.getDefault(FORMAT)` is `ru_RU`, and `user.language`/`user.country`
> are `ru`/`RU` — all correct, in strict mode. What is wrong is that a locale
> asked for **explicitly by name** answers with US separators.
> `DecimalFormatSymbols.getInstance(Locale.GERMANY)`:
>
> ```text
>                              grouping   decimal
>   HotSpot 21                 U+002E     U+002C
>   CratonVM --real-jdk  21    U+002E     U+002C
>   CratonVM --jdk-only  21    U+002C     U+002E   <-- the one wrong cell
>   CratonVM --real-jdk  25    U+002E     U+002C
>   CratonVM --jdk-only  25    U+002E     U+002C
> ```
>
> So it is strict-only AND 21-only, and it is not reachable through
> `user.*` properties — those are right. Every non-US locale collapses to US
> separators, which is the shape of locale data resolving to root/US rather
> than of a locale being chosen wrongly. Mechanism not yet identified.
> `probes/Jdk21StrictLocaleData.java` reproduces it; it prints separators as
> code points because ru-RU's is U+00A0 and "looks like a space" is not a
> measurement (it also makes `grep` treat the transcript as binary).

Neither reproduces on JDK 25: the `25-windows` and `25-linux` keys carry two
sections each, both `vthreads`, and neither of these.

## 3. The baseline this produced, and what it does and does not mean

`scripts/baselines/jdk-only-strict-corpus-21-windows.txt` — 16 sections against
the 25 keys' 2. **13 of the 16 are the four defects above**, and they are
frozen so the gate can say "no worse", not because they are acceptable. The
note in the file says so and points here.

Minting it is still the right move: with no key the 21 legs exit 2 and measure
nothing, so every one of these four was invisible. With it they gate.

Verified as a gate, not just written:

```text
run 1..3   RESULT: PASS    16 observed, 16 baselined      (stable; no intermittent row)
remove one baselined row -> RESULT: FAIL, rc=5, "NEW DIVERGENCES — the ratchet fired: + JdkOnlyPlatformProbe/strict/vthreads"
```

The fixture halves both build on 21: the agent jar, and the JNI `.dll` through
the MSVC fallback. No degraded-fixture declaration was needed, so no section
was silently absent.

## 4. What this does NOT claim

* **No fix is proposed.** §1 names the mechanism and the 33 call sites; it does
  not say what the repair is. Resolving the carrier by asking which class
  implements `jdk.internal.access.JavaLangAccess`, rather than by name, is the
  obvious shape and is exactly the kind of thing that needs its own lane — the
  literal is load-bearing in a registrar, a factory, a classloader path and
  that bridge's own contract tests.
> **SUPERSEDED 2026-09-09 -- `21-linux` is minted and gating.** Run on an
> azure Linux host against Temurin 21.0.12+8 with a release binary built from
> `origin/dev` @ 5487287dc. All four matrix legs now adjudicate; none refuses.
>
> The Linux key carries **16 sections, the same count as `21-windows` but not
> the same set**: Linux does not diverge on `textformat` (that row is locale
> data, and the Windows measurement was taken on a host whose default locale is
> ru_RU -- see the #4 note above), and Linux carries a `real/vthreads` row
> Windows does not.
>
> **It was not accepted from one mint, and the first two attempts would have
> been a gate that went red at random.** The vthreads section is an
> intermittent race, and because the baseline records section IDENTITY, a
> baseline minted from a run that saw the smaller set reads the larger set as
> NEW. Attempt 1 minted 15 sections, passed one gate run and failed the next.
> The accepted baseline is the MAXIMAL observed set -- every later run is then
> a subset, and GONE always passes -- and it was held to ten consecutive
> passing gate runs before acceptance, with the paired ratchet then re-run
> against it (remove one row -> rc=5 naming that row; restore -> rc=0).
>
> Two of the four defects on this page are therefore now frozen on BOTH
> platforms rather than one.

* **`21-linux` is untouched.** This is one platform. The 21 Linux leg still
  refuses, and a baseline from another key cannot adjudicate it.
* **#4 is an observation.** No locale mechanism is identified.
  **SUPERSEDED 2026-09-09 — the mechanism is now identified.** CratonVM’s
  CLDR locale adapter reports only **5** supported locales where HotSpot
  reports **1,063**, and the five are exactly the set that ships inside
  `java.base`; the rest live in the `jdk.localedata` module, which is not
  being picked up. `LocaleProviderAdapter.getAdapter` therefore finds no
  adapter claiming `de-DE` and falls through to
  `FallbackLocaleProviderAdapter`, whose root/English data IS the US
  separators. Nothing throws anywhere, which is why this never appeared in
  a stack trace. Full measurement, and an UNRESOLVED conflict with the
  `--real-jdk` cell recorded above (Linux shows the fallback in BOTH modes,
  Windows recorded `--real-jdk` correct):
  ../serviceloader-loadinstalled-finds-nothing-so-every-platform-loader-service-is-empty-20260909.md
  **CORRECTED the same day:** the cause is NOT `jdk.localedata` failing to
  load — that module is present and its data is intact. It is
  `ServiceLoader.loadInstalled` returning NOTHING for every service (the
  platform-loader lookup), which is what the CLDR adapter uses to find its
  supplementary metadata. Not a locale defect at all.
* **The 87-method claim is about the class, not about CratonVM.** Only
  `parkVirtualThread(long)` and `encodeASCII` were observed failing; that the
  other 85 would fail the same way is a prediction from the mechanism, not a
  measurement.
* **Two binaries, one result.** The 16 sections were identical from a binary
  built at `0d79de121` and one at `6502772c4` (22 Rust files apart), so this is
  not an artefact of one build.
