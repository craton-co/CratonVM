# Lane T — the throwable family retired, and the three defects the arm had to find first

**Status: CLOSED.** 2026-09-10, branch
`claude/lane-t-cross-cutting-registrars-20260910`, worktree
`.claude/worktrees/gc-cross-collector-issues-9b41cd` on the Windows host.
Oracle: HotSpot `jdk-25.0.3+9`, the same image CratonVM ran against.

Lane T of the nine-lane `--jdk-only` campaign owned **1,100 §1.4 shadow rows
over 87 classes from 57 registration call sites** — the only lane whose
ownership is not a class-name prefix. This is what it found, what it retired,
and what it could not.

Headline: **906 triples retired**, three present-tense defects fixed (all three
in BOTH compatibility modes), one census mis-classification corrected, one
security defect filed, and four registrar groups classified as blocked with the
blocker named.

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
   total registrations                12,889
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

[`lane-T-cross-cutting-registrars-RETIRED-20260910.md`](../jdk-only-lanes/lane-T-cross-cutting-registrars-RETIRED-20260910.md) §1 and §5 assign
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

## 1. The three defects, and which instrument found each

The instrument is `CRATONVM_ENFORCE_NATIVE_SHADOW` over the 62 class names of
`THROWABLE_FAMILY_CLASSES`, run against the 64 promoted probes of
`scripts/baselines/jdk-only-strict-corpus-25-windows.probes` with a HotSpot
control. One binary, no build.

**The first arming was VACUOUS and said so.** The scope was joined with `+`
(`scripts/jdk-only-blast-radius.sh`'s group syntax) instead of commas, and read out of a CRLF
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
compatibility modes** rather than artefacts of the arming. A third (LT-3) the
dial structurally could not see, and only the trial binary found:

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

### LT-3 — the one the DIAL could not find, and the trial binary did

The armed arm was clean on all 64 probes. The trial binary carrying the table
was not: `ThrowableFamilySweep` moved by 2.

```text
   Class.forName("no.such.Klass") -> e.getMessage()
     HotSpot                  no.such.Klass
     control (lt2)            no.such.Klass
     trial   (lt3)            null
   ... and `--real-jdk` on the same trial binary is unaffected.
```

`jboss_module_loader::alloc_single_message_exception` — 31 call sites for
`ClassNotFoundException`, `NoClassDefFoundError` and `NullPointerException` —
writes the message into **slot 0**, which is the SYNTHETIC-stub layout's
`detailMessage` and the REAL layout's `Throwable.backtrace`. It read back
correctly only because `native_throwable_get_message` also reads slot 0 whenever
the receiver's own class declares no `detailMessage` (`resolve_field_index` does
not walk to `Throwable`) and the slot happens to hold a `String`. **Two wrongs
cancelling**, and retiring the shadow removed the second one.

This is the difference between a dial arm and a retirement that
`jdk-only-lane-operations.md` states as permanent: the loader mints its CNFE
from inside a native, and *a call that originates in a native is not one of the
dial's doors*. Armed, `alloc_single_message_exception` still reached the shadow
`getMessage`; retired, the registration is gone. **The wave was accepted on a
trial binary and could not have been accepted on a dial arm** — the same
sentence the phase-3 CHM wave had to write, for the same reason, from the other
direction.

The fix keeps BOTH writes. The slot-0 write is what keeps compatible mode
byte-for-byte identical (the shadow still runs there and still reads slot 0);
`write_throwable_detail_message` puts the message where real
`Throwable.getMessage()` bytecode looks, and falls back to slot 0 on a synthetic
layout, writing the same value. The honest fix — constructing through
`<init>(String)` like `create_exception_object` does, which would also give
these throwables the stack trace and `cause` sentinel they do not have — is
wider than a shadow retirement should carry and is left named rather than
attempted.

**And it closed a SECOND divergence nobody was chasing.**
`ClassNotFoundException.toString()` on a loader-minted instance had been
`java.lang.ClassNotFoundException` where HotSpot has
`java.lang.ClassNotFoundException: no.such.Klass` — both modes, both binaries,
before this wave. `toString` composes the header from `getLocalizedMessage()`,
which is `getMessage()`, so a message written into the wrong field was missing
from it too. MEASURED on the acceptance binary, both modes:

```text
   forName getMessage   no.such.Klass
   forName toString     java.lang.ClassNotFoundException: no.such.Klass
```

`ThrowableFamilySweep` never asked that question of a loader-minted throwable,
and it is worth saying why the row was invisible rather than merely absent: the
probe builds its throwables with `new`, and `new` was the one path that had
always been right.

---

## 2. The retirement: 906 triples, class-scoped — now 783, see §8

`native-api/src/retired_shadow.rs::RETIRED_SHADOW_LT_TRIPLES`.

**This section describes the table as first measured, 2026-09-10.** §8 pulls
123 of these 906 rows back the next day, on evidence from a SIBLING lane's
test rather than this one's own corpus — read this section for the shape of
the retirement, and §8 for what changed.

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

### What the trial binary measured

Control is the same tree WITHOUT the table (`cratonvm-lt2`); trial is the same
tree WITH it (`cratonvm-lt3`). Both `--jdk-only`, both against a HotSpot 25.0.3+9
oracle on the same class files.

```text
   refusals on the 62 classes   1022, and ZERO with a survivor
   the 64 promoted probes       64 of 64 at delta 0 (after LT-3)
   RThrowableFillFrames         PASS in both modes on the trial binary
   goal population              5,553 -> 4,624
   lane T's own scope           57 sites / 1,100 rows / 87 classes
                                -> 42 sites /   224 rows / 22 classes
```

And on the ACCEPTANCE binary — the merged tree with all three fixes and the
table — the same 64 probes are compared against HotSpot in absolute terms
rather than as a delta:

```text
   64 of 64 probes byte-identical to HotSpot, except FfmCarrierProbe at its
   pre-existing 5 (identical armed and unarmed, before and after this wave).
```

**1,022 refusals and no survivor** is the check
`jdk-only-lane-operations.md` puts first after a build: a refusal is a
retirement only when nothing already owns the triple. The census predicted it —
all 1,021 registrations of these triples are ambient `register()` under a
`set_category(Bridge)` scope — and the report confirms it, plus the one row that
was already a `SyntheticStub`.

**The goal population moves by 929, and the two halves account for all of it**:
906 retired + 23 reclassified by §3. Nothing else moved, which is the arithmetic
saying the table retires exactly what it names.

Lane T's remaining 224 rows, all four groups blocked:

| group | sites | rows | classes | blocker |
|---|---:|---:|---:|---|
| keystore | 17 | 136 | 8 | the TLS identity side table (§4.1) |
| HashSet family | 21 | 59 | 3 | `CopyOnWriteArraySet` iterates empty (§4.2) |
| panama segments | 2 | 23 | 5 | `NoSuchMethodError` (§4.3) |
| FFM layouts | 1 | 4 | 4 | zero members (§4.4) |
| jmx | 1 | 2 | 2 | `enforcement_dial.reached` is **0** — no instrument in the tree dispatches `ThreadMXBean.getThreadInfo([JZZI)`, so precondition 4 cannot be met |
| | **42** | **224** | **22** | |

### The corpus, which is the acceptance measurement

`SUITE=all TIMEOUT=600`, 133 scheduled vectors, sequential arms on one host.
Control is `origin/dev` `39a90d2f4` (`cratonvm-lt1`); trial is the merged tree
with the wave (`cratonvm-lt4`).

```text
                                  control (dev)      trial
   SUITE=all --jdk-only           132 / 133          133 / 133
   the one control failure        RThrowableFillFrames — the vector this wave
                                  adds, on a binary without the fixes
```

**The trial is green on the whole strict corpus**, and the control's single
failure is the new vector demonstrating its own sensitivity rather than a
regression: every other vector passes in both arms, `RMapGcStress` included.

The per-vector `--jdk-only-report` census the suite unions over all 133 vectors
is the third independent measurement of the wave's size:

```text
                                        control      trial
   native-shadows-bytecode, native-won     1418        1351     -67
   ... bytecode-won                         459         448
   synthetic-native-registered, UNION      1858        2879   +1021
   interpreter_shadow_unenforced, SUM     13131       13070
```

**+1021 again**, and this one is a union of per-vector refusal sets over a real
workload rather than a registration replay — the same number the three
stub-ratchet arms and the census's own registration count give. `native-won`,
the column the campaign calls "the defect", falls by 67: the throwable natives
this corpus actually dispatched.

The DEFAULT-mode arms, which is where "compatible mode stays byte-for-byte
unchanged" is checked:

```text
                                  control (dev)      trial
   SUITE=all (default)            132 / 133          133 / 133
   the control's one failure      RThrowableFillFrames, again
```

The trial's default arm read 132/133 the FIRST time, with `RMapGcStress` at
`rc=124`, and that was a load artefact of this session rather than a result:
that arm was the one running while `cargo test` and `cargo clippy --workspace`
were compiling. `RMapGcStress` passed on the same binary in the strict arm and
on the control in both, and alone at idle it takes **3m04s wall including the
javac** against a 900 s budget. Re-run with nothing else on the host the arm is
**133 / 133, rc=0** — measured, not inferred. *A hang cell is a claim about your
timeout.*

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
[`ffm-segment-surface-nine-behavioural-defects-and-the-interface-classed-family-20260829.md`](ffm-segment-surface-nine-behavioural-defects-and-the-interface-classed-family-20260829.md).

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
   builds                4 release builds, 48 / 50 / 50 minutes and the
                         acceptance build on the merged tree
   arms with no build    6 dial arms x 64 probes x 3 (HotSpot, cratonvm,
                         cratonvm-armed) = 1,152 probe runs, plus one
                         control-vs-trial pass of the same 64
   probes written        LTKeyStoreEngineSweep (52 rows)
   vectors written       RThrowableFillFrames (17 checks)
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
  `origin/dev`** and not from this branch: one recorded pair,
  `java/lang/Class.getModule`, no longer drifts because commit `0b2791ac7`
  tagged it `Intrinsic` without the four-edit remedy `lane-0` §7 describes.
  It is L0's row and L0's remedy.

  **Re-measured on the merged tree rather than re-cited**, because dev had
  moved 40 commits since the first attribution and a cited verdict decays. The
  gate scans SOURCE, and its own header documents a standalone build, so the
  same question can be put to two revisions for the price of two `rustc`
  invocations and no workspace build:

  ```bash
  git archive origin/dev native-builtins native-collections native-io       native-awt vm native-builtins-crypto native-builtins-security | tar -x -C "$D"
  CARGO_MANIFEST_DIR="$D/native-builtins"       rustc --edition 2021 --test -O -o drift-dev.exe       "$D/native-builtins/tests/registrar_drift.rs"
  ./drift-dev.exe the_drift_baseline_has_no_stale_rows --test-threads=1
  ```

  Pristine `origin/dev` fails with the SAME single pair, from the same
  `register_p59_module` at the same `reflect_invoke.rs:3616`. No commit on this
  branch touches `registrar_drift.rs` or `reflect_invoke.rs`, and no lane-T
  table entry names `java/lang/Class` — the 42 `java/lang/` classes in
  `RETIRED_SHADOW_LT_TRIPLES` are all throwables.

  One consequence for every lane reading a workspace run: `cargo test
  --workspace` is FAIL-FAST, so this red truncates the run. The `89 ok / 1
  FAILED` it prints is a PREFIX of the suite, not a summary of it, and a lane
  that reads it as "everything else passed" is reading a run that stopped.
  `--no-fail-fast` is what answers the question.
* **CI's BLOCKING `Clippy` step is RED on `origin/dev`**, and this is worth a
  line because a gate nobody can go green through is the shape `H3-1` found
  once already. `cargo clippy --workspace --all-targets -- -D warnings` fails
  with four `drop_non_drop` errors in
  `jit/tests/intrinsic_string_narrow_oops.rs:425-428`, from `534e1921b`
  ("the narrow-oop fixtures were four mallocs"), which
  `git merge-base --is-ancestor 534e1921b origin/dev` confirms is on `dev`.
  Nothing on this branch touches that file. The fix is four lines and it is the
  JIT lane's.
* **The goal denominator is 5,530** after §3, and the campaign's published
  5,549 / 5,553 both include the 23 inherited-constructor rows.
* **`alloc_single_message_exception` still does not construct.** ~~LT-3's fix
  puts the message in both places; it does not give a loader-minted
  `ClassNotFoundException` a stack trace or a `cause` sentinel, because it never
  runs a constructor.~~ **CLOSED the same day — §7.** The funnel now runs the
  class's own `<init>(String)`, in both compatibility modes, with the flat path
  kept as the fallback.
* **`apps/probes/LTKeyStoreEngineSweep.java` is NOT promoted**, and must not be
  while it is red: promoting a divergent probe freezes the divergence as
  acceptable, which is the bar
  `scripts/baselines/jdk-only-strict-corpus-25-*.probes` states in its own
  header. Promote it in the commit that closes §4.1's four rows.

---

## 7. LT-4: the residual, closed — a loader-minted exception now runs its constructor

§6 handed on one residual that was this lane's own rather than another lane's:
`alloc_single_message_exception` **allocates and stamps**. LT-3 put the detail
message in both places a `getMessage` might look, and stopped there, because a
shadow retirement should not carry a constructor rewrite. This is that rewrite.

### 7.1 What the application actually caught

Thirty-one call sites across the class loader, the JBoss module loader,
`Class.forName` and constant-pool resolution mint exactly four classes this
way — `ClassNotFoundException` (21 sites), `NullPointerException` (6),
`NoClassDefFoundError` (3), `org/jboss/modules/ModuleNotFoundException` (1).
None of them had ever run a constructor, and `regression-suite/src/RLoaderExceptionShape.java`
is the vector that says what that costs. On the binary that preceded this
change, in **both** compatibility modes:

```text
   RLoaderExceptionShape        AssertionError: askForwardName: the trace is not empty
   HotSpot                      PASS RLoaderExceptionShape (23 checks)
```

The message, `toString` and `getCause()` checks pass on that binary — LT-3's
fix holding — so the vector fails on the first check that is about the
*constructor* rather than the message. Three things were missing:

* **`getStackTrace()` was empty.** Every logging framework prints that trace,
  and a plugin loader that reports *which of my callers asked for this class*
  reads it. HotSpot's is the real call stack.
* **`cause` was the unset sentinel**, so `initCause` SUCCEEDED where HotSpot
  refuses it. `ClassNotFoundException(String)` is `super(s, null)`: the cause
  is set, to null, and a second one is an `IllegalStateException`. That check
  is the sharpest of the twenty-three, because it cannot be satisfied by
  writing a field — only by running the constructor that writes it.
* **Nothing else the class's own `<init>` does** ran either.

### 7.2 The fix is one funnel and a fallback that is not decoration

`try_construct_single_message_exception` runs
`<init>(Ljava/lang/String;)V` through `NativeContext::new_object_initialized`
— the GC-safe constructor entry every reflective path already uses — and
returns `None` on any refusal, at which point the historical
allocate-and-stamp body runs unchanged. The three refusals are all real shapes
this funnel has to survive: a synthetic class with no such constructor (the
`--synthetic-jdk` arm's `ModuleNotFoundException`), a constructor that throws,
and a **re-entrant mint**.

The re-entrancy guard is the load-bearing part. The callers of this funnel are
the class loader; running a constructor from inside one can load a class, and a
load that fails comes back through this same funnel. Without the guard that is
unbounded recursion on the one input the funnel exists to report. With it, the
inner mint takes the flat path, which allocates and cannot re-enter.

**Both modes gain, for different reasons**, which is why the vector is a
regression-suite vector and not a `--jdk-only` probe:

| mode | what `<init>(String)` resolves to | what captures the trace |
|---|---|---|
| `--jdk-only` | real `Throwable` bytecode | `fillInStackTrace()`, and LT-1's trim |
| `compatible` | the family's `<init>` shadow | `capture_throwable_trace`, same trim |

`ClassNotFoundException`'s shadow row is `native_exc_init_message_null_cause`
— `super(s, null)` — so the `initCause` check is satisfied in compatible mode
by the shadow that was already written to model it, not by the retirement.

### 7.3 What it measured

Acceptance binary `cratonvm-lt6.exe`: the merged tree, LT-4, and KS-1.
Control `cratonvm-lt4.exe`: this lane's wave without either.

```text
   RLoaderExceptionShape          default      --jdk-only
     control (lt4)                AssertionError  AssertionError
     trial   (lt6)                PASS 23         PASS 23
     HotSpot                      PASS 23

   64 promoted probes, lt4 -> lt6, against a HotSpot oracle
     --jdk-only                   0 of 64 differ   ctrl 5 / trial 5
     default                      0 of 64 differ   ctrl 5 / trial 5

   SUITE=all, 134 vectors, TIMEOUT=900
     --jdk-only                   134 / 134
     default                      134 / 134

   jdk-only-refusal-survivors.sh  5 rows, matches baseline (3,007 refusals)
   jdk-only-census.sh             rc=0
   stub_ratchet x3 feature arms   green after the lane-2 re-freeze
   jdk_only_registry              green
   jdk_only_class_origin          green
   jdk_only_dispatch              green
   cratonvm-types                 green after three doc numbers
```

The `ctrl 5 / trial 5` is the whole probe result rather than a summary of it:
both arms diff from HotSpot by exactly five lines, which are
`FfmCarrierProbe`'s pre-existing five, and the totals are equal, so nothing
moved in either direction. That is taken with `diff -a`. The script this lane
inherited used a bare `diff`, which prints ONE "Binary files differ" line for a
single NUL byte and scores it as ZERO through `grep -c '^[<>]'` — so a
crashing arm reads as perfect. Re-running the earlier wave's own numbers under
`-a` reproduced them, which is the only reason they still stand.

**134, not 133**, because `RLoaderExceptionShape` joins the corpus in this
wave. The control fails it by construction; that is what a vector is for.

### 7.4 What the vector does NOT cover

It exercises the 21 `ClassNotFoundException` sites. The other ten — six
`NullPointerException`, three `NoClassDefFoundError`, one
`ModuleNotFoundException` — take the same funnel and the same constructor
call, but no portable probe reaches them: a `NoClassDefFoundError` from
constant-pool resolution needs a class compiled against a class that is then
deleted, which the suite's single-shot `javac` cannot express, and the loader
NPEs are reached only through internal states. They are covered by
construction, not by measurement, and that distinction is the honest one.

## 8. 123 rows pulled back, 2026-09-11 — a sibling lane's test found what this
one's own corpus could not

A 93-commit merge of `origin/dev` brought lane 1's waves 3 and 4 alongside
this branch's KS-5/KS-6 work (see the keystore known-issues page). Wave 4's
own test, `wave_four_is_six_classes_and_refuses_jarfile_breakiterator_
dateformat`, asserts that two triples must NOT be retired by ANY table:

```text
("java/text/ParseException", "printStackTrace", "(Ljava/io/PrintWriter;)V")
("java/text/ParseException", "setStackTrace",   "([Ljava/lang/StackTraceElement;)V")
```

Its own comment states why, and the reasoning does not stop at
`ParseException`: **"13 of its 14 triples are Throwable's inherited surface,
so the defect is Throwable's and not `java/text/`'s"** — armed alone on a
probe that does `e.setStackTrace(new StackTraceElement[0]);
e.printStackTrace(w)`, HotSpot prints one header line and this VM prints the
full internal trace, because this VM's `Throwable` model does not read back a
`stackTrace` array bytecode wrote through `setStackTrace`. That is a defect
BELOW the shadow/bytecode boundary this whole campaign turns on: yielding to
real bytecode does not fix it, because the real bytecode's `printStackTrace`
reads the VM's own internal stack-trace state, which `setStackTrace` never
updated correctly in the first place.

`RETIRED_SHADOW_LT_TRIPLES` retired both triples for **all 62**
`THROWABLE_FAMILY_CLASSES`, not just `ParseException` — the table is
class-scoped (§2), so the same exposure sits on every sibling. Pulled back
for all 62 rather than left active on the 61 the wave-4 author's own test
does not name: **123 rows removed** (62 × `printStackTrace(PrintWriter)`, 61
× `setStackTrace` — `InvocationTargetException` never had a `setStackTrace`
row to begin with). `RETIRED_SHADOW_LT_TRIPLES` is now **783 triples**, not
906; §2 is left as first measured and this section is the update.

**This was not caught by lane T's own corpus** — `SUITE=all` scored 134/134
on `cratonvm-lt6.exe` through `cratonvm-lt8.exe`, all three including this
exact regression, because no vector in the corpus happens to call
`setStackTrace` immediately before `printStackTrace` on a throwable. The
defect was live on every binary this lane shipped before this merge, and the
only reason it surfaced now is that a sibling lane's author reasoned about
the SAME 906-row table from the outside and wrote a test naming the two
triples specifically — worth recording as a gap in this lane's own
instrumentation, not only as a fix. No probe in this repo currently exercises
`setStackTrace` immediately before `printStackTrace`; closing that gap
(a `RThrowableSetStackTraceThenPrint` vector, or similar) is left for
whichever lane next touches the `Throwable` family, since fixing the
underlying VM-side stack-trace model is out of scope for a registrar lane.

Both class-guard tests still pass after the carve-out
(`no_non_throwable_sibling_is_retired_by_lane_t`,
`the_lane_t_table_is_disjoint_from_every_sibling`), confirming the removal
touched only these two triples and created no new overlap with any sibling
table.
