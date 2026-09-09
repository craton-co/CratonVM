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

#3 is mode-independent, so it is not a strict-mode question. #4 is
strict-only and **not** diagnosed here — the control's separators are the
locale's and CratonVM's are US-style, which is a lead about locale data, not a
finding.

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
* **`21-linux` is untouched.** This is one platform. The 21 Linux leg still
  refuses, and a baseline from another key cannot adjudicate it.
* **#4 is an observation.** No locale mechanism is identified.
* **The 87-method claim is about the class, not about CratonVM.** Only
  `parkVirtualThread(long)` and `encodeASCII` were observed failing; that the
  other 85 would fail the same way is a prediction from the mechanism, not a
  measurement.
* **Two binaries, one result.** The 16 sections were identical from a binary
  built at `0d79de121` and one at `6502772c4` (22 Rust files apart), so this is
  not an artefact of one build.
