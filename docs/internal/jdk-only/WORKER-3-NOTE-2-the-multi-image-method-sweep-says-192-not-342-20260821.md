# WORKER-3-NOTE-2 — the multi-image sweep `H25-1` called a precondition RAN, and it says **192**, not 342: 46% of that population is declared by a supported image and must not be retired

**Status: MEASURED.** Nine JDK images (`javap -p -s --system`), one
`--dump-native-registry --explain-jdk-only --jdk-only` dump from a clean build
of `22cb4338d`. New instrument committed as
`scripts/jdk-only-no-image-methods.py` with a `--selftest`. WORKER 3,
2026-08-21, on the Linux build host.

`H25-1` named a fourth verb — a registration whose METHOD no image declares —
sized it at 342, and then refused to retire a single row, for one reason:

> **342 is a ONE-IMAGE upper bound.** … `StringUTF16.isBigEndian` is the
> standing witness that a member of this population can be deliberate and
> correct. … **N1 — run the multi-image sweep FIRST; it is a precondition, not
> a follow-up.** This host has one JDK installed and this lane could not run it.

**This host has nine.** `/data/jdkimages` on the Linux build host carries
Temurin **17.0.20, 21.0.12 and 25.0.4** × **linux / windows / macos-x64**, and
`javap --system <image>` reads any of them without installing anything. The
sweep ran. `H25-1`'s refusal was right, and this record is the measurement that
says by how much.

---

## 1. The instrument

`scripts/jdk-only-no-image-methods.py` is the METHOD-granular sibling of
`scripts/jdk-only-no-image-receivers.py`. For every registration in a registry
dump it asks each image, through `javap -p -s --system`, whether the class
declares that exact descriptor, whether a supertype does, or whether neither,
and partitions the population:

| bucket | meaning |
|---|---|
| `LIVE` | declared (or inherited) by **every** image. Nothing to say. |
| `PARTIAL` | declared by **some** image only — the `isBigEndian` shape. |
| `NEAR_MISS` | some image declares the **name** on the class, but no overload with this descriptor. `H25-1` §2.2's population. |
| `DEAD_EVERYWHERE` | no image declares it anywhere on the hierarchy. **The retirable set.** |
| `CLASS_ABSENT_EVERYWHERE` | the receiver class is on no image — already covered at class granularity by `NO_IMAGE_JDK_RECEIVERS`. |

It **reports and never removes**, and exits 0 on a finding: a retirement is a
source change carrying the duplicate-registration hazard (`H22`, trap 4) that no
census can see, and a script cannot check that.

### 1.1 The two parser bugs this record found in its own instrument, before it found anything in the tree

Both were caught by running the tool against rows whose answer was already
known, which is the only reason they are in this section rather than in the
results.

* **`java/lang/Object` was excluded from the supertype walk**, to bound the
  recursion. The first run then reported
  `java/lang/Package.equals(Ljava/lang/Object;)Z` as **declared by no image** —
  when every image declares it on `Object`, and the registration is
  `H25-2` §3.3's *deliberate override of an inherited method*, where relocating
  or retiring it would delete a documented Spring fix. A sweep that produces
  that row is a sweep that would have deleted it.
* **The member/descriptor pairing has to be positional, not by name.** Pair by
  name and every overload collapses, so a `NEAR_MISS` — the entire
  interception-that-never-fires species — reads as `LIVE`.

Both are pinned by `--selftest`, which runs the parse over a frozen `javap`
sample and asserts, among others, that
`repeat(Ljava/lang/String;I)` is **not** in the parsed set of a class that
declares `repeat(CI)` and `repeat(Ljava/lang/CharSequence;I)`.

**`setup lies`** is the standing note and it applies to this file: a probe's own
setup is code that can be wrong. Nothing in either bug would have shown up as an
error; both produce confident, well-formed, wrong tables.

## 2. The result, over `H25-1`'s own population

Every registration the dump's single-image adjudication calls undeclared
(355 rows at this tip; `H25-1` measured 342 at `025780ff7`):

| bucket | rows | share |
|---|---:|---:|
| **`DEAD_EVERYWHERE`** — the retirable set | **192** | 54.1% |
| `NEAR_MISS` | 63 | 17.7% |
| `PARTIAL` — declared by a supported image | 62 | 17.5% |
| `LIVE` — declared by every image | 38 | 10.7% |

**A lane that had retired this population on the strength of the JDK-25 census
would have been wrong about 100 of 355 rows — 28% — and would have removed 62
registrations that a supported image needs.** Adding the near-misses, which are
a different verb again, 46% of the population is not a `DEAD_EVERYWHERE` row.

### 2.1 The 38 `LIVE` rows are FIELDS, and that is a finding about the dump, not about the images

`java/lang/foreign/ValueLayout.JAVA_INT`, `java/util/Locale.US`,
`java/nio/ByteOrder.BIG_ENDIAN`, `java/util/jar/Attributes$Name.MAIN_CLASS` — 38
registrations whose "descriptor" has no parentheses. They are the field-shaped
rows `E36-1` and `E21-1` describe. `image_declaring_method` adjudicates
**methods**, so it reports every one of them as declared nowhere; the images
declare all 38 as fields.

They are neither dead nor a defect this record can price. They are noise in the
342, and the honest denominator for "registrations standing in front of a method
that does not exist" is **317**, not 355.

### 2.2 The `PARTIAL` rows, and why the shape is not rare

62 rows are declared by some images and not others. Sampled and confirmed by
hand:

```text
  java/lang/Thread.stop0(Ljava/lang/Object;)V     JDK 17 only
  java/lang/Thread.suspend0()V                    JDK 17 only
  java/lang/Thread.resume0()V                     JDK 17 only
  java/lang/Thread.countStackFrames()I            JDK 17 and 21
  java/lang/StringUTF16.isBigEndian()Z            JDK 17 and 21   <- H25-1's witness
  java/lang/invoke/MethodHandles.dropArgumentsTrusted(...)   JDK 21 and 25, NOT 17
  java/lang/AbstractStringBuilder.repeat(II)…     JDK 21 and 25, NOT 17
```

Note the last two: the shape runs in **both directions**. A registration can be
correct because an OLDER image declares the method, and equally because a NEWER
one does. A sweep that only checked "is this in JDK 17" would have made the
opposite mistake.

## 3. `java/lang`, the block this lane was pointed at

`H14-2` §4 sized the unclaimed `java.lang` core at 168 rows. Over all 2,592
`java/lang*` registrations in the strict dump:

| bucket | rows |
|---|---:|
| `LIVE` | 2,416 |
| `PARTIAL` | 118 |
| `DEAD_EVERYWHERE` | 31 |
| `NEAR_MISS` | 16 |
| `CLASS_ABSENT_EVERYWHERE` | 11 |

The 31 `DEAD_EVERYWHERE` rows in full, with their owners, because **this lane
owns three of them and nobody owns the rest**:

```text
  java/lang/System.runFinalizersOnExit(Z)V            deprecated_lang.rs   RETIRED, this lane
  java/lang/Thread.destroy()V                         deprecated_lang.rs   RETIRED, this lane
  java/lang/Class.hasRealParameterData()Z             lib.rs
  java/lang/Object.registerNatives()V                 lib.rs
  java/lang/Throwable.getStackTraceDepth()I           lib.rs
  java/lang/Throwable.getStackTraceElement(I)…        lib.rs
  java/lang/Object.{supplier,accumulator,finisher,combiner}()…   native-collections/lib.rs   (x4)
  java/lang/System$1.currentThread0()…                lib.rs AND shared_secrets_bridge.rs  (registered TWICE, dead in both)
  java/lang/System$1.{findNative, getBytesNoRepl, getBytesUtf8NoRepl,
      getMethodsOrNull, getReflectionFactory, isCarrierThreadLocalPresent,
      newStackTraceElement, newStringUtf8NoRepl}      shared_secrets_bridge.rs   (x8)
  java/lang/invoke/MethodHandleImpl$1.{findMethodHandleType,
      linkMethodHandleConstant, makeClassValueMap}    shared_secrets_bridge.rs   (x3)
  java/lang/reflect/ReflectAccess.{newAccessibleObject, newParameter}
                                                      shared_secrets_bridge.rs   (x2)
  java/lang/StackStreamFactory$AbstractStackWalker.checkStackWalkModes()Z  stack_walker.rs
  java/lang/StackWalker$Option.<clinit>()V            stack_walker.rs
  java/lang/foreign/MemorySegment.allocateFrom(Arena,ValueLayout,{I,J})…   panama.rs (x2)
  java/lang/foreign/ValueLayout.<clinit>()V           phases_late/foreign_ffm.rs
  java/lang/module/ResolvedModule.getDescriptor()…    jboss_jdkspecific.rs
```

The four `java/lang/Object` rows carry `H25-1` §1.5's caveat and are excluded
from any unreachability claim: a native on `Object` is a catch-all, and
`check_override_chain` admits 19 subclass families.

`java/lang/System$1.currentThread0()` is worth naming on its own: it is
registered **twice, in two files**, and **both copies are dead on every
image**. Retiring one of them moves no behaviour and no census row; retiring
both removes a duplicate that a future lane would otherwise have to adjudicate.

### 3.1 The 16 `java/lang` near-misses

```text
  AbstractStringBuilder / StringBuilder / StringBuffer .repeat(Ljava/lang/String;I)…   RETIRED, this lane (x3)
  ProcessBuilder$Redirect.{INHERIT,PIPE}()…                        phases_late.rs
  StackStreamFactory$AbstractStackWalker.{callStackWalk x2, fetchStackFrames}   lang_stackwalker.rs
  System$1.{blockedOn, getDeclaredPublicMethods, getEnumConstantsShared}        shared_secrets_bridge.rs
  MemorySegment.{get, set, getAtIndex, setAtIndex, toArray(OfBoolean)}          panama.rs
```

Each is an interception somebody wrote and that has **never once executed**, on
any of the nine images. They are invisible to the census by construction — no
dispatch, so no shadow row — and `H25-1` §3's prediction holds: **retiring them
predicts a census delta of ZERO, and that is a PASS.**

## 4. What this does NOT establish

* **Nine images is not every image.** The set is Temurin 17.0.20 / 21.0.12 /
  25.0.4 on three platforms. A JDK 11 consumer, a non-Temurin vendor, or a
  future 26 would each move rows between `PARTIAL` and `DEAD_EVERYWHERE`. The
  script takes the image list as an argument for that reason, and the tree's
  own supported set is what should drive it — this record did not go looking for
  where that set is declared, and **did not reconcile its nine against
  `scripts/jdk-only-no-image-receivers.py`'s six.** That reconciliation is
  nominated below and is a real gap: the two sweeps currently disagree about
  what "supported" means.
* **`DEAD_EVERYWHERE` is a licence to consider a retirement, not to make one.**
  Three further conditions apply to every row and this record checks none of
  them: is the triple registered more than once (trap 4); does a test or
  manifest assert its presence (`H25-3` R2, which cost two crates); is the
  registrar reached on the `--synthetic-jdk` arm, where a fabricated carrier
  CAN declare a method the real image does not (`flag≠mode drops it`).
* **The 62 `PARTIAL` rows were not individually adjudicated for CORRECTNESS on
  the image that does declare them.** "JDK 17 declares `suspend0`" says the
  registration can fire there; it says nothing about whether the body is right
  when it does. This lane did not run a JDK 17 arm — CratonVM's `--java-home`
  was pointed at 25 throughout.
* **The dump is one boot of one workload.** `invocations` is a lower bound
  (`G33-1`) and is used here only as corroboration, never as an argument.
* **No `--synthetic-jdk` dump was taken.** Every figure is `--jdk-only`.

## 5. NOMINATIONS

* **N1 — reconcile the supported-image set between the two sweeps.**
  `jdk-only-no-image-receivers.py`'s docstring names six images (21 and 25 ×
  three platforms); `/data/jdkimages` carries nine, including 17. If 17 is
  supported, the class-granular table may be retiring receivers that JDK 17
  declares; if it is not, 62 of the 355 rows here become retirable and
  `Thread.stop0`/`suspend0`/`resume0`/`isBigEndian` all change verdict. **The
  two sweeps must not answer to different denominators**, and this one deferred
  to the wider set on purpose, because over-retention is the cheap error.
* **N2 — the 192 `DEAD_EVERYWHERE` rows are now a work list**, and 29 of them
  are `java/lang` rows in files no lane owns (§3). The largest single owner is
  `shared_secrets_bridge.rs` with 13.
* **N3 — the field-shaped rows need their own adjudication** (§2.1). 38 of them
  sit inside a population everybody is reading as "methods that do not exist",
  and they are neither. Either `image_declaring_method` grows a field arm or the
  triage tool labels them, but they should stop inflating a number people quote.
* **N4 — the near-miss population deserves a REGISTRATION-TIME warning**, which
  is `H25-1` N3a and is cheaper than either sweep: a descriptor-level diff
  against the image at the moment `register()` is called would have caught all
  63 the day each was written, and needs no multi-image anything to be useful.
* **N5 — `System$1.currentThread0()` is registered twice and both copies are
  dead** (§3). Neither file is this lane's.

---

## INDEX ROWS (for H0 to move into `INDEX.md`)

* `WORKER-3-NOTE-2` — the multi-image method sweep ran: `H25-1`'s 342 is **192**
  retirable, 62 are declared by a supported image, 63 are near-misses and 38 are
  fields. New instrument `scripts/jdk-only-no-image-methods.py`. **OPEN** —
  N1 (image-set reconciliation) blocks quoting either sweep's number as final.
