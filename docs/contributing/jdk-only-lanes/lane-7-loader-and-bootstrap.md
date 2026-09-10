# Lane 7 — the class loader, the bootstrap, and the failure triage

**Scope: 20 §1.4 shadows over 6 classes — and the campaign's critical path.**
Prefixes: `java/lang/ClassLoader*`, `jdk/internal/loader/`.

This is **not primarily a retirement lane.** Its 20 rows are the smallest scope
in the campaign; its job is to unblock the other eight and to own the triage
that routes work to them. Read [`lane-0-integration-and-gates.md`](lane-0-integration-and-gates.md) §2-§6 first.

---

## 1. Why this lane is the critical path

Under `CRATONVM_ENFORCE_NATIVE_SHADOW=all`, 108 of 132 corpus vectors fail (25
pass as of the last measured wave). All 108 were classified by the exception the
VM actually reported — not sampled, because the harness's per-vector message is
the last line of stderr and for 44 of them that line is the dial's own door
census, which says nothing about the cause:

```text
 41  AssertionError                 a real behavioural difference
 22  NullPointerException           of which 12 are a null java.lang.Module
 11  IllegalStateException          ALL of them "Not yet initialized"
 10  <no exception line>            an output diff, not a crash
  6  IllegalArgumentException
  4  InternalError
  3  ServiceConfigurationError
  2  each: UnsatisfiedLinkError, RuntimeException, AbstractMethodError
  1  each: SocketException, ClassNotFoundException, ArithmeticException,
        FileNotFoundException
```

The `IllegalStateException` family and the `ServiceConfigurationError` family
have both since been closed (`VM.savedProps` published; `Class.getName` tagged).
What remains for this lane is the two structural families below.

## 2. Target 1: `jdk/internal/loader/BuiltinClassLoader` will not link

**Ten vectors, and no field publish reaches it.** After `VM.savedProps` was
published, `ClassLoaders.<clinit>` gets eleven lines further — from line 66 to
line 77 — and then dies constructing the builtin loader hierarchy:

```text
NoClassDefFoundError: jdk/internal/loader/BuiltinClassLoader
```

The class **is in the image**; this VM cannot load or link it. That makes it
structural class-loading work, which is why it was recorded rather than
attempted by the field-publishing waves.

Before pricing it, apply the rule that has already corrected one claim in this
campaign: **read the image before concluding a surface is missing.** `javap -p`
the class and its supertypes. A VM shim registering a handful of an interface's
methods says nothing about what the real carrier can do — `java.lang.System$1`
turned out to have all 88 members with bytecode, and the publish alone took the
`all` arm from 5 passing to 24.

This blocks **L6's** service-loading rows and parts of **L1's** locale
providers. Report progress to both.

## 3. Target 2: the null `java.lang.Module`, 12 vectors

`ClassLoader.getUnnamedModule()` answers null, surfacing through
`ClassLoader.postDefineClass` → `NamedPackage.<init>`. It may still be
field-shaped: `ClassLoader.unnamedModule` is written by the real `ClassLoader`
constructor.

Three published-static precedents to copy, all in the same cluster and all
**absent-or-complete** on purpose — a half-filled field stops throwing and
starts answering null, which is worse:

- `java/lang/System.props` — stamped only when the real map fill succeeds.
- `SharedSecrets.javaLangAccess` / `.javaLangReflectAccess`.
- `jdk/internal/misc/VM.savedProps` — a real `java/util/HashMap`, built with
  `new_object_initialized` and filled through its own `put` bytecode, because
  real code calls `get` on it and a carrier will not do.

And the ordering rule that cost a build: **a published static must beat the
`<clinit>` that copies it.** `SharedSecrets` published *after* the `initPhase1`
body cleared `ConstantUtils.JLA` but left `sun.nio.cs.UTF_8.JLA` null, because
the body installs charsets and `UTF_8.<clinit>` captures the getter on the way
past — and the cleared half hid the failure.

A caution on `Module` specifically: `Module.getLayer()` on the *unnamed* module
currently answers non-null where HotSpot answers null. That is recorded, not
frozen, and `apps/probes/ClassModuleSweep.java` is checked in with the row.
L0 owns `java/lang/Module`; hand it the finding rather than tagging it.

## 4. Target 3: own the triage, route the 41 `AssertionError`s

The 41 `AssertionError`s are individual semantic gaps in other lanes'
territory. This lane's job is to **classify and route**, not to fix them:

- `RJdkForkJoin` — `CountedCompleter leaves: 128` → **L5** (already routed).
- Each remaining one: name the failing assertion, the class family, and the
  owning lane, in a table on this page.

Two rules the triage must follow, both learned the hard way here:

- **A first-failure count cannot score a fix in a chain.** The 2026-09-09 wave
  cleared **eleven** vectors' first failure and moved the passing total by
  **one**, because ten of the eleven walked straight into the
  `BuiltinClassLoader` blocker. Report "N first-failures removed, M new
  blockers named", never just the delta.
- **A triage page is stale the day after it is written.** Re-run the
  classification before citing it, and before investigating any row run the two
  commands: `git show origin/dev:<page>` (your copy is stale) and a
  case-insensitive recursive grep for the symptom.

## 5. This lane's 20 retirement rows

```text
   9  jdk/internal/loader/URLClassPath
   5  jdk/internal/loader/AbstractClassLoaderValue
   2  jdk/internal/loader/BootLoader
   2  jdk/internal/loader/BuiltinClassLoader
   1  each: ClassLoaders$AppClassLoader, ClassLoaders$PlatformClassLoader
```

Do them **last**. Retiring a loader native while the loader hierarchy does not
link changes which failure you see without changing whether it fails, and the
27 `java/lang/ClassLoader` rows are L0's — hand back anything whose remedy turns
out to be a `Class` question.

## 6. Traps

- **A refused `SyntheticStub` falls through to an older native, not to
  bytecode.** So a refusal count is not a retirement count: check
  `JdkOnlyViolation::SyntheticNativeRegistered.survivor` and report
  `N refusals, 0 survivors`.
- **The shadow dial cannot see a call that starts inside a native.** A fix that
  works only once the registration is *gone* reads as breakage on a dial arm.
  This is the single most misleading instrument in the campaign for this lane,
  because bootstrap calls originate inside natives constantly.
- **The regression suite cannot reach `jdk.internal.misc`** — probes that need
  it require `--add-opens java.base/java.lang=ALL-UNNAMED`, and **without it
  both VMs throw `InaccessibleObjectException` with different messages**, which
  reads as a screen of meaningless diffs rather than as a mute instrument.
  `apps/probes/SysPropsStaticProbe.java` documents the requirement and is
  deliberately kept out of the standard sweep.
- **`<clinit>` failures cascade.** The first throw hides the silent ones; run
  each row independently.

## 7. Done

The `BuiltinClassLoader` link failure is either fixed or reduced to a named,
priced sub-problem; the null-`Module` family is closed or classified; all 41
`AssertionError`s are routed to an owning lane in a table on this page; and the
20 loader rows are retired or classified. This lane reports the `all`-arm count
each wave, together with the first-failure/blocker breakdown that keeps that
number honest.
