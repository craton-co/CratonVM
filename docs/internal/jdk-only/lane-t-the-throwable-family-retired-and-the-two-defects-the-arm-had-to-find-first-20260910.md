# Lane T — the throwable family retired, and the two defects the arm had to find first

**Status: CLOSED.** 2026-09-10, branch
`claude/lane-t-cross-cutting-registrars-20260910`, worktree
`.claude/worktrees/gc-cross-collector-issues-9b41cd` on the Windows host.
Oracle: HotSpot `jdk-25.0.3+9`, the same image CratonVM ran against.

Lane T of the nine-lane `--jdk-only` campaign owned **1,100 §1.4 shadow rows
over 87 classes from 57 registration call sites** — the only lane whose
ownership is not a class-name prefix. This is what it found, what it retired,
and what it could not.

Headline: **906 triples retired**, two present-tense defects fixed (both in
BOTH compatibility modes), one census mis-classification corrected, one security
defect filed, and four registrar groups classified as blocked with the blocker
named.

**The largest of those is not a `--jdk-only` defect at all.** The frame skip
this wave needed for its constructor rows turns out to be what
`java.lang.Throwable` owes every *application* exception: on the shipping
binary, in both modes, `new MyException("x")` reported
`MyException.<init>` as its own throw site. Every framework exception hierarchy
has that shape. §1 has the measurement.

---

## 0. The scope, regenerated rather than trusted

The lane page said to regenerate the registrar list from a fresh dump. Done, on
`--dump-native-registry --explain-jdk-only` from a binary built at
`origin/dev` `39a90d2f4`, with the bucket rules of
`lane-0-integration-and-gates.md` §1 and its lane prefix sets:

```text
   total registrations                12,849 -> (this tree) 12,918
   goal population, A+B eligible       5,553
   cross-LANE registrars                  57 sites, 1,100 rows, 87 classes
```

**57 / 1,100 / 87 reproduces lane 0's table exactly**, which is what makes the
rest of this page comparable to the campaign's own numbers. The seven groups:

| group | sites | rows | classes | lanes spanned | verdict |
|---|---:|---:|---:|---|---|
| **throwable family** (`lang_misc.rs` ×11, `lib.rs` ×2) | 13 | **872** | 63 | L0–L6 | **RETIRED** (§2) |
| **keystore** (`keystore.rs` ×17) | 17 | 136 | 8 | —, L6 | BLOCKED (§4.1) |
| **HashSet family** (`native-collections/lib.rs` ×21) | 21 | 59 | 3 | L1, L5 | BLOCKED (§4.2) |
| **panama segments** (`panama.rs` ×2) | 2 | 23 | 5 | L2, L4 | BLOCKED (§4.3) |
| **jmx** (`jmx.rs` ×3) | 3 | 6 | 4 | —, L2 | NOT A SHADOW (§3) |
| **FFM layouts** (`foreign_ffm.rs` ×1) | 1 | 4 | 4 | L2, L4 | BLOCKED (§4.4) |
| | **57** | **1,100** | **87** | | |

### The lane page named a registrar that is not in this set

`lane-T-cross-cutting-registrars.md` §1 and §5 assign
`native-io/src/concrete_receiver.rs:185` ("191 rows / 21 classes") to this lane
and spend a whole section on it. It is **not cross-lane**: its 191 goal rows are
every one of them under `sun/nio/`, which is L4's prefix set, so lane 0 §2's own
rule puts it in L4 — and lane 0's 1,100 excludes it (1,100 + 191 ≠ 1,100).
`lane-4-io-nio-foreign.md` §3 repeats the claim and tells L4 to leave ~100 of
its channel rows alone.

The census settles it: 191 rows, 44 classes registered by that site in total,
**one lane**. `mirror_class_registrations` is class-parameterised, which is what
makes it invisible to the drift scanner, but class-parameterised is not
cross-lane. **L4 owns it, and the hold lane 4's page was asked to observe was
never lane T's to place.**

---

## 1. The two defects, and why the arm was the only thing that could find them

The instrument is `CRATONVM_ENFORCE_NATIVE_SHADOW` over the 62 class names of
`THROWABLE_FAMILY_CLASSES`, run against the 64 promoted probes of
`scripts/baselines/jdk-only-strict-corpus-25-windows.probes` with a HotSpot
control. One binary, no build.

**The first arming was VACUOUS and said so.** The scope was joined with `+`
(`blast-radius.sh`'s group syntax) instead of commas, and read out of a CRLF
file, so it became one 2,219-byte prefix that matched nothing. The probe tree
came back byte-identical — a perfect result — and `enforcement_dial.reached` was
**0**. Comma-separated and CR-stripped:

```text
   reached 25,772   yielded 24,880   declined_no_bytecode 892
   ThrowableFamilySweep   0 -> 8 differing lines
   IoSystemSweep          0 -> 2 differing lines
   the other 62 probes    0 -> 0
```

Ten lines, two root causes, and **both are present-tense defects in BOTH
compatibility modes** rather than artefacts of the arming:

### LT-1 — `Throwable.fillInStackTrace()` reported ITSELF as the throw site

The public no-arg `fillInStackTrace()` is real bytecode in every mode; only the
private `fillInStackTrace(int)` it calls is a native. That native captured the
whole Java stack, including the `Throwable.fillInStackTrace` frame directly
above it. Measured on the shipping binary, both modes:

```text
   Throwable u = new RuntimeException("y"); u.fillInStackTrace();
   u.getStackTrace()[0]
     HotSpot   FillTopFrame.main
     CratonVM  java.lang.Throwable.fillInStackTrace
```

HotSpot's `java_lang_Throwable::fill_in_stack_trace` drops two prefixes of the
innermost end: `fillInStackTrace*` frames, then `<init>` frames, in both cases
only where the throwable `is_a` the frame's holder.
`vm/src/runtime/stackwalker.rs::trim_throwable_fill_frames` now does the same,
applied at `NativeContextImpl::capture_throwable_stack_trace` — the single entry
point both the twelve `native_exc_init_*` bodies and `fillInStackTrace(int)`
funnel through, so neither can be fixed while the other is left reporting the
filling machinery.

### The `<init>` half is not a retirement detail — it was already broken for every application exception

The skip was written for the constructor rows this wave retires: once those
registrations yield, `Throwable.<init>` and its `super(...)` chain are ordinary
Java frames on top of the throw site, and `IoSystemSweep`'s `stack top is this
class |true|` row is what moved on it.

**But it was never only about the retirement.** An application exception's own
constructor IS real bytecode today — `class MyEx extends RuntimeException {
MyEx(String m) { super(m); } }` — and the native `super(m)` is where the trace
is captured, so `MyEx.<init>` was already sitting on top of every trace. On the
shipping binary, both modes:

```text
   static Throwable a() { return new MyEx("a"); }
     HotSpot   SubclassTop.a
     CratonVM  SubclassTop$MyEx.<init>

   static Throwable b() { return new Deep("b"); }   // Deep extends MyEx
     HotSpot   SubclassTop.b
     CratonVM  SubclassTop$MyEx.<init>              // both levels wrong

   try { throw new Deep("c"); } catch (Throwable t) { ... }
     HotSpot   SubclassTop.main
     CratonVM  SubclassTop$MyEx.<init>
```

Every framework exception hierarchy has that shape, so **every stack trace of
every application exception in this VM named the wrong top frame**, and no probe
in the tree asked. Only `java.*` throwables looked right, because their
constructors are the natives that do the capturing and push no frame. That is
the whole reason the `<init>` half of HotSpot's rule exists, and it is why the
vector asserts three subclass shapes as well as the explicit-refill one.

**The frame order was the trap, and the comment was wrong about it.**
`capture_full_trace`'s doc said "top-of-stack → bottom (innermost to
outermost)". It is OUTERMOST-first — `thread.frames[0]` is the bottom frame —
so the trim pops from the END. Measured with `CRATONVM_DBG_STTRACE=1` before a
line of the fix was written, and the comment is corrected in the same commit.

### LT-2 — `arr.clone()` on a null array said `clone on null`

```text
   static String[] sa;   sa.clone()
     HotSpot   NullPointerException: Cannot invoke "[Ljava.lang.String;.clone()"
                 because "NpeCloneProbe.sa" is null
     CratonVM  NullPointerException: clone on null
```

An array-typed call site is dispatched by a branch in
`interpreter/invoke.rs` chosen BEFORE the receiver-driven `match` that holds the
`Value::Object(None)` arm, so the JEP 358 message was never built.
`Object.clone` is on `force_native_over_real_jdk_bytecode`'s list, so dispatch
reached `native_object_clone`, whose null arm is inside a native and has no
bytecode context to name an expression from. `arraylength` and `aaload` on the
same null field were already right, which is exactly why this one row hid.

`regression-suite/src/RThrowableFillFrames.java` (17 checks) is the vector for
both, and it passes on HotSpot, fails on the pre-fix binary in both modes, and
passes after. Neither the explicit-refill row nor the null-array clone is
visible without arming something — nothing in the tree calls
`fillInStackTrace()` explicitly, and nothing clones a null array — and the
application-subclass rows were visible all along to anything that had thought to
look.

### With both fixed, the arm is clean

Re-run on the fixed binary, same 64 probes, same scope:

```text
   64 of 64 probes: armed delta 0.  ThrowableFamilySweep 8 -> 0,
   IoSystemSweep 2 -> 0, and no probe moved in the other direction.
   (FfmCarrierProbe stays at its pre-existing 5, armed and unarmed alike.)
```

---

## 2. The retirement: 906 triples, class-scoped

`native-api/src/retired_shadow.rs::RETIRED_SHADOW_LT_TRIPLES`.

**The scope is the class set, not the registrar**, and that is deliberate: the
dial is keyed on the RECEIVER's class, so a registrar-scoped table would retire
a set the arm never measured and would leave a mixture behind — 34 rows on the
same 62 classes come from `reflect_annotations.rs` and from `lib.rs` sites
outside the two loops. `H0-4`'s standing finding is that uniform-native works,
uniform-bytecode works, and the mixture is the configuration with evidence
against it.

Every LIVE (`owns_slot`) `Bridge` row on those 62 classes whose image target
carries `Code`, declared or inherited: **906 of the 1,026 registered rows**. The
120 that are not there:

```text
   115  superseded (owns_slot=false) -- covered anyway; the retirement is
        per-TRIPLE and `register`'s re-tag fires at every ordinal
     2  bucket D, ACC_NATIVE in the image -- §1.5 says `Bridge` is CORRECT:
        Throwable.fillInStackTrace(I) and NullPointerException.getExtendedNPEMessage()
     2  bucket F, no such method on JDK 25 -- Throwable.getStackTraceDepth()
        and .getStackTraceElement(I), a JDK 8 pair the image dropped
     1  already SyntheticStub
```

All four exclusions are asserted in
`the_four_throwable_rows_that_are_not_shadows_stay_bridges`, because all four
look exactly like the entries around them and nothing else in the tree says why
they are absent.

### Prefixes: 50 class names, not three packages

`RETIRED_SHADOW_PREFIXES` gains the 62 class names less the twelve already
covered by `java/util/` and `java/text/`. Lane 0 §4 asks for the narrowest
prefix that covers the entry, and the reason is a guard, not tidiness: the list
is deliberately redundant with the tables and `a_prefix_alone_retires_nothing`
asserts that `java/lang/ref/Reference.clear()V` is not admitted by it. A blanket
`java/lang/` would keep that test green and delete the guard.

### The gates it moves, all read off the failing assertion

```text
   BASELINE_SYNTHETIC_STUBS_MANAGEMENT      1894 -> 2915   +1021
   BASELINE_SYNTHETIC_STUBS_NO_MANAGEMENT   1883 -> 2904   +1021
   BASELINE_SYNTHETIC_STUBS_SYNTHETIC_JDK   1883 -> 2904   +1021
   STRICT_MIN_TOTAL_REGISTRATIONS         10_900 -> 10_400
   jdk-only-kind-map-25-linux.tsv          1157 rows bridge -> synthetic-stub,
                                           kind_stated 0 -> 1
```

**The same +1021 on three independently-measured arms, and it is the paired
number rather than arithmetic**: the census counts 1,021 registrations of those
906 triples (906 owning + 115 superseded), every one an ambient `register()`
under a `set_category(Bridge)` scope (`kind_stated: false`, `kind_chosen: true`
on all 1,021), so the re-tag reaches every one and none is left over.

The strict floor is the one that needed an argument, because lowering a collapse
detector is the move it exists to make suspicious. The 2026-08-11 entry on that
constant says a re-tag "moved this total by zero" — true of the COMPATIBLE
census it measured, false of the strict one, where a re-tagged `SyntheticStub`
is refused by `allowed_in(JdkOnly)` and leaves the registry. The test prints the
whole account: `compatible 13623 rows (2904 stubs) -> strict 10716 rows; 2907
rows dropped, 2925 refusals recorded`. 2,907 dropped for 2,904 stubs; the extra
three are the alias fallout `strict_registry_drops_only_the_stubs` documents.

---

## 3. The 23 rows that were never shadows: an inherited constructor is not a target

`ImageMethodVerdict` resolved every registration up the class hierarchy when the
named class did not declare the method. That is right for a virtual method and
**wrong for `<init>`**: JVMS §6.5 says an `invokespecial` whose resolved
instance-initialization method is declared somewhere other than the class named
by the instruction throws `NoSuchMethodError`. A constructor is not inherited.

Measured on JDK 25: **31 registrations carried an inherited `<init>` verdict, 23
of them live `Bridge` rows in the goal population, and 17 of those inherited
`java/lang/Object.<init>()V`** — which initialises nothing.

```text
   java/lang/management/{ClassLoading,Compilation,GarbageCollector,Memory,
       OperatingSystem,PlatformLogging,Runtime,Thread}MXBean   <- Object.<init>
   com/sun/management/{OperatingSystem,Thread}MXBean            <- Object.<init>
   javax/management/MBeanServer, java/lang/management/MemoryUsage
   java/net/HttpURLConnection, sun/net/www/protocol/http(s)/*
   jdk/internal/net/http/{Http1Exchange,HttpClientImpl,HttpRequestImpl}
   sun/security/ssl/SSLEngineImpl, java/util/TreeMap$KeySet
```

Retiring one of these refuses a native that populates a VM-minted receiver's
fields and yields to a body that writes none of them — a **silent field-init
loss**, which is the failure mode §1.4's remedy exists to avoid. They are
bucket F, "class present, method absent", and always were.

`classloading/src/class_manager.rs::adjudicate_natives_against_image` no longer
walks the hierarchy for `<init>`/`<clinit>`. Consequences:

* **the campaign's goal population is 5,530, not 5,553** — 23 rows leave it;
* four of lane T's own six `jmx` rows leave with them, which closes that group
  by classification rather than by measurement (and is the only way it could
  have been closed: `enforcement_dial.reached` is **0** for those four classes
  across all 64 probes, so nothing in the tree dispatches them);
* `bridge.shadows_bytecode_anywhere` in the bridge ratchet falls by up to 31.
  A fall passes the `<=` ratchet and prints a re-freeze instruction; it is
  **not** auto-lowered, per `stub-ratchet.md`.

---

## 4. The four blocked groups, each with the blocker named

Every one was armed alone on the same 64 probes, on the same binary.

### 4.1 keystore — 136 rows, and arming it makes the surface BETTER

`apps/probes/KeyStoreTypeProbe.java` is byte-identical armed (368 dispatches,
368 yields) — and it exercises seven of the seventeen `engine*` methods and none
of the private-key half. `apps/probes/LTKeyStoreEngineSweep.java` was written
for that gap, and the answer is not the one the green cell suggested:

```text
   unarmed   24 differing lines of 52   (four rows x JKS, PKCS12, JCEKS)
   armed      8 differing lines of 52   (the same four rows, JKS only)
   dial       930 reached, 930 yielded; ZERO keystore natives invoked
```

`KeyStore.getKey(alias, wrongPassword)` returns the private key on all three
store types, and for JKS it returns the still-encrypted
`EncryptedPrivateKeyInfo` bytes wrapped as a `PrivateKey`. Retiring fixes
PKCS12 and JCEKS outright.

**Blocked anyway, and the blocker is out of band:** these natives are the
producer of a Rust-side store the TLS stack reads —
`crypto_impl::rsa_key_store`, `t27_tls::install_identity_from_der`,
`keystore_get_private_key()`. Yield them and the real objects are correct while
that store stays empty, with nothing reporting it: mTLS stops working and no
probe in the corpus can see it. Full record, with the reproduction:
`docs/known-issues/jdk-only/keystore-getkey-accepts-any-password-and-the-key-does-not-round-trip-20260910.md`.

### 4.2 HashSet family — 59 rows, an EMPTY iteration

```text
   IoSystemSweep                cow set sorted |[a, b]|  ->  |[]|
                                cow set after add sorted |[a, b, c]|  ->  |[]|
   LinkedSequencedShadowSweep   99 set removeFirst |ok a [b]|  ->  |ok a [a]|
```

`CopyOnWriteArraySet`'s real bytecode reads a `CopyOnWriteArrayList al` field
these natives never populate, so a yielded set iterates empty — the silent
wrong answer, not an exception. `register_set_view_carrier_natives`' own
CLUSTER NOTE (H4-1) already says the four `MAP_KEY_ITR_CARRIERS` cannot be
refused while any `SET_VIEW_CARRIERS` entry outside the moving family is still
`Bridge`; this measurement is the same finding from the set side.

### 4.3 panama segments — 23 rows, a `NoSuchMethodError`

```text
   FfmCarrierProbe   SECTION-ABORTED segments java.lang.NoSuchMethodError
                     (104 rows -> 101; the "segments" section stops)
```

`MemorySegment`/`Arena`/`SegmentAllocator` are a VM-side object model. The
`allocateFrom` rows the registrar loops over seven descriptors × three
allocators cannot yield to bytecode that expects a real
`HeapMemorySegmentImpl`. Same family as the nine FFM defects already recorded in
`ffm-segment-surface-nine-behavioural-defects-...-20260829.md`.

### 4.4 FFM layouts — 4 rows, zero members

```text
   FfmLayoutProbe   struct members |2| -> |0|
                    member 0 name |Optional[b]| -> THREW IndexOutOfBoundsException
                    byteOffset(c)  |4| -> THREW IllegalArgumentException:
                        Bad layout path: cannot resolve 'c' in layout [i4(b)i4(c)]
```

`GroupLayout.memberLayouts()` yielded reads a field the native never populates.
The layout's `toString` still shows both members, which is the tell: the
VM-side state is right and the real accessor cannot see it.

---

## 5. What was measured, and what it cost

```text
   builds                3 release builds (48, 49, 27 minutes)
   arms with no build    6 dial arms x 64 probes x 3 (HotSpot, cratonvm,
                         cratonvm-armed) = 1,152 probe runs
   probes written        LTKeyStoreEngineSweep (52 rows)
   vectors written       RThrowableFillFrames (13 checks)
```

Every arm here priced a retirement for one environment variable and no build,
which is the point of the instrument. What it cannot price is in
`docs/contributing/jdk-only-lane-operations.md` — three permanent differences
from a real retirement — and this wave was accepted on a trial binary carrying
the table, never on a dial arm.

## 6. Residuals this page hands on

* **`concrete_receiver.rs:185` belongs to L4** (§0). Lane 4's page carries a
  hold that was never lane T's to place; its 191 rows are L4's to price.
* **The JKS half of the keystore defect is in `sun/security/`** (§4.1) and
  survives the arm, so it is lane 6's, not this registrar's.
* **`registrar_drift`'s `the_drift_baseline_has_no_stale_rows` is RED on
  `origin/dev` `39a90d2f4`** and not from this branch: one recorded pair,
  `java/lang/Class.getModule`, no longer drifts because commit `0b2791ac7`
  tagged it `Intrinsic` without the four-edit remedy `lane-0` §7 describes.
  Verified by reverting this branch's `retired_shadow.rs` to `HEAD` and
  re-running: the same one stale pair. It is L0's row and L0's remedy.
* **The goal denominator is 5,530** after §3, and the campaign's published
  5,549 / 5,553 both include the 23 inherited-constructor rows.
