# The census asks one class, on one platform — and ~2,000 rows were filed under the wrong verdict

**Status:** the reading is fixed and the instruments ship. `undecl` is broken
out by `scripts/jdk-only-adjudicate.py --inherited`, the platform question is
answered by `scripts/jdk-only-platform-diff.py`, and the abstract-interception
blast radius by `scripts/jdk-only-interception.py`. **What remains OPEN is a
disposition question, not a measurement one:** 1,612 rows are shadows nobody has
adjudicated, 11 natives were measured standing in front of an *application*
class, and 254 registrations are dead on every JDK 25 image. Filed 2026-08-05
while closing the L5/L5b/L5c residuals; the three follow-ups were closed the
same day and are recorded under *What the follow-ups measured* below.

**Nothing here is a crash and no native's kind changed.** The defect is in what
the measurement *means*.

## The two questions `image_declaring_method` cannot answer

It resolves a registration's `(class, name, descriptor)` triple against the
bytes of **one class** in **one image**. Two things it therefore cannot see:

1. **An inherited declaration.** A native registered on
   `sun/nio/ch/SocketDispatcher.close(Ljava/io/FileDescriptor;)V` comes back
   `declared: false`. The method is concrete bytecode on
   `sun.nio.ch.UnixDispatcher`, two frames up, and CratonVM's receiver-driven
   dispatch finds the registration first — so the row is a contract §1.4
   **shadow of inherited bytecode**, and it was **dispatched three times** in
   the census run that reported it as undeclared.
2. **The other platform.** `jdk/net/WindowsSocketOptions.*`,
   `java/io/WinNTFileSystem.*` and `sun/awt/PlatformGraphicsInfo.hasDisplays0`
   score `ABSENT`/`UNDECL` on a Linux image for the same reason a genuinely
   dead registration does: the class or method is not in *that* image.

Both mistakes push rows into the bucket the adjudication script labels *class
present, method **not declared** — dead or misdescribed*, and both make a
correct registration look like a defect.

## What the instruments say

### Inheritance — `sh scripts/jdk-only-inherited-decl.sh <census.json>`

Runs `probes/InheritedDeclProbe.java` on real HotSpot against the same image,
resolving each `declared: false` triple up the class-and-interface chain.
JDK 25.0.4, Linux, 2026-08-05 — **2,542 rows in the bucket**:

| what it actually resolves to | rows | what the row really is |
|---|---:|---|
| inherited, concrete bytecode | 1,612 | a §1.4 **shadow** |
| inherited, abstract | 308 | intercepts **every** implementor, incl. user subclasses |
| inherited, `ACC_NATIVE` | **19** | a §1.5 **bridge the census did not credit** |
| genuinely nowhere in the hierarchy | 603 | a dead registration |

**76% of the bucket is not dead.** And the 19 are the sharp end: 18 are
`sun/nio/ch/FileDispatcherImpl.*` inheriting the syscall surface from
`UnixFileDispatcherImpl`, and one is
`java/awt/image/ComponentSampleModel.initIDs()V` inheriting a native `initIDs`
from `java.awt.image.SampleModel`.

### Platform — `python3 scripts/jdk-only-platform-diff.py <a> <b>`

The obstacle was assumed, not checked: **CratonVM adjudicates an image it
cannot run.** The pass parses class bytes off the module image and never
executes them, so a Windows JDK unpacked on the Linux host produces a complete,
image-adjudicated census. Two censuses, same binary, same workload, same JDK
version (25.0.4+7), only `--java-home` differing:

* **230 of 11,916 rows** get a different verdict.
* **59 rows are a genuine `ACC_NATIVE` bridge only on Windows** — all nine
  `WindowsSocketOptions` entries, the `WinNTFileSystem` family, and
  `PlatformGraphicsInfo.hasDisplays0`.
* **78 rows are one only on Linux** — the `UnixFileSystem` /
  `UnixFileDispatcherImpl` counterparts.
* **1,735 rows are `ABSENT` on both** — dead on every platform, and the honest
  deletion candidates.

## Three things this corrects in existing records

* **`sun/awt/PlatformGraphicsInfo.hasDisplays0()Z`.** Its
  `JDK-ONLY-CLASSIFY: bridge` marker was **right**, and L5b's rewrite of that
  marker — "bridge on Windows/macOS, DEAD on this image" — was right about the
  image and needlessly pessimistic about the verdict. It is a bridge; the Linux
  census simply could not say so. It states its kind now.
* **`sun/nio/ch/WindowsFileDispatcherImpl` exists on NEITHER image.** L5 left
  28 rows under that spelling inherited on the theory that they are "the same
  natives on their own image". They are not: the Windows JDK calls its class
  `FileDispatcherImpl` too. Those 28 are dead on every JDK 25 platform.
* **`java.io.RandomAccessFile.close0()V` and
  `sun/nio/ch/UnixFileDispatcherImpl.setDirect0(..CharBuffer..)I`** survive the
  wider reading: `NOT-FOUND` in the hierarchy, `ABSENT` on both platforms.
  The L5 record's verdict on them stands.

## What the follow-ups measured

### 1. `jdk-only-adjudicate.py` no longer lets `undecl` read as "dead"

Its header now states that the column is a superset, and
`--inherited <tsv>` adds a section **2b** that breaks the bucket into
`INHERITED code` / `INHERITED abstract` / `INHERITED native` / `NOT-FOUND`, and
lists the 19 rows section 2 counts as unadjudicated and should not. Without
the flag it prints, in place of the section, a line saying the split was not
run — an absent measurement announces itself rather than looking clean.

### 2. The abstract-interception blast radius, measured

`--dump-class-origins` rows now carry a **`supertypes`** column (direct
superclass and interfaces), which is the piece the join was missing: the native
census names the declaring class, the class census names every class the run
loaded, and only the supertypes edge connects them.
`scripts/jdk-only-interception.py` walks it.

On `JdkOnlyCensusLoadProbe` (JDK 25, 2026-08-05) — 705 classes loaded, 1,635
abstract-target natives, and **11 of them stand in front of a class the
application defined**:

| native | intercepts |
|---|---|
| `java/util/Collection.{isEmpty,iterator,size,toArray}` | `JdkOnlyCensusLoadProbe$MyCollection` |
| `java/util/Map.{containsKey,entrySet,get,keySet,put,size,values}` | `JdkOnlyCensusLoadProbe$MyMap` |

That is the `register_interface_natives` hazard, reproduced with a named
example instead of a worry: a user class implements `Map`, and eleven of its
methods resolve to a VM native written for `java.util`'s own implementations.
`invocations` is 0 for all eleven in this run — the probe builds the classes
but does not exercise every method — which is exactly why the surface matters
more than a receiver log.

**What it measures:** the interception *surface* — every loaded class that
inherits the intercepted method. Not a per-invocation receiver log. The VM's
dispatch sites do not uniformly hold the receiver, and threading one through
them would put work on the hottest path in the interpreter to buy a narrower
answer: the surface is what the registration *can* capture, and it does not
depend on whether this workload happened to call it.

### 3. "Dead on both images" was six times smaller than the count suggested

The 1,735 figure is **not** a deletion list, and this record said it was. Split
by namespace (`scripts/jdk-only-platform-diff.py` now prints this):

* **254** are in a JDK namespace (`java.`, `javax.`, `jdk.`, `sun.`,
  `com.sun.`) — names that should be in the image and are in neither. Those are
  the deletion candidates. Concentrated in `plain_socket.rs` (34),
  `atomic_updater.rs` (32), `nio_native.rs` (28), `native-collections` (21),
  `shared_secrets_bridge.rs` (18), `watch.rs` (18).
* **1,481** are third-party or VM-minted names — `org.springframework.`,
  `io.netty.`, `groovy.`, `cratonvm/synthetic/…`. They are absent from a JDK
  image *by construction* and live whenever the application supplies them.
  Deleting one because a JDK census called it `ABSENT` would remove a working
  native.

And the 254 carry their own caveat, which the tool now prints: **a registration
dead on JDK 25 may be the live one on JDK 21.** The diff sees one version. A
deletion wave has to sweep the versions the project supports first.

## What is still open

1. **L6's ratchet counts `has_code` on the named class only.** By the wider
   reading the true shadow population is 4,693 + 1,612 = **6,305**. The
   ratchet's *number* is well defined — it is a frozen count of a precisely
   named thing, and it is what the gate should keep measuring — but prose that
   calls it "the shadows" understates by a third.
2. **1,612 inherited shadows have no disposition.** Measured now, adjudicated
   by nobody. Contract §1.4 lets a `Bridge` lose to concrete bytecode, so none
   of them is wrong *today*; each is a registration standing in front of real
   Java that someone should either justify or delete.
3. **The 11 natives that intercept an application class.** The measurement
   exists; the decision does not. `java.util.Map`/`Collection` natives
   intercepting a user implementor is the `register_interface_natives` verdict
   this directory has been deferring since the ambient-category audit.
4. **The 254 JDK-namespace dead registrations**, pending a multi-version sweep.
5. **A workload broader than one probe.** Every interception set above is as
   wide as what `JdkOnlyCensusLoadProbe` loaded — 705 classes. Take the same
   three artefacts from H2 or Spring Boot and the user-implementor list will
   grow; an empty set means "nothing loaded under it here", never "nothing
   can".

## Reproducing

```sh
# the census (all three instruments consume it; add --dump-class-origins for
# the interception join)
cratonvm --real-jdk --java-home "$JAVA_HOME" --explain-jdk-only \
    --dump-native-registry census.json -cp probes L5rProbe

# 1. inheritance
JAVA_HOME=<same image> sh scripts/jdk-only-inherited-decl.sh census.json

# 2. platform — unpack a Windows JDK of the SAME version, then
cratonvm --real-jdk --java-home <windows-jdk> --explain-jdk-only \
    --dump-native-registry census-win.json -cp probes L5rProbe
python3 scripts/jdk-only-platform-diff.py census.json census-win.json linux windows

# 3. abstract-method interception, including application classes
python3 scripts/jdk-only-interception.py --registry census.json \
    --classes classes.json --inherited undecl-out.tsv --only-user

# and the adjudication table with the bucket broken out
python3 scripts/jdk-only-adjudicate.py census.json --inherited undecl-out.tsv
```

Both refuse rather than print zeroes when the census lacks
`--explain-jdk-only`, and the platform diff refuses if the two censuses do not
correspond row-for-row.
