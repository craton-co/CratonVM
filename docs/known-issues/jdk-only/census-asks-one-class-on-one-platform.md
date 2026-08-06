# The census asks one class, on one platform — and ~2,000 rows were filed under the wrong verdict

**Status:** the reading is fixed and the instruments ship. `undecl` is broken
out by `scripts/jdk-only-adjudicate.py --inherited`, the platform question is
answered by `scripts/jdk-only-platform-diff.py`, and the abstract-interception
blast radius by `scripts/jdk-only-interception.py`. **The dispositions are taken too** — see
*What the dispositions decided* — leaving one open item, which is a deletion
wave with a list attached rather than a question. Filed 2026-08-05
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

## What the dispositions decided

### The 1,612 inherited shadows: differential-tested, and one of them was wrong

Counting shadows says nothing about whether any is *wrong*, so
`probes/ShadowDifferentialProbe.java` exercises the most-reached shadowed
surface — `List/Set/Map.of`, `copyOf`, `unmodifiable*`, `subList`,
`LinkedHashMap`'s entry views including write-through `setValue`, and
`Map.entry` — and prints every observable so the run diffs byte-for-byte
against HotSpot 25.

**Everything matched except `Map.entry`, and it was wrong in two ways:**

| | HotSpot 25 | CratonVM (before) |
|---|---|---|
| `Map.entry("k", 7).toString()` | `k=7` | `java.util.Map$Entry@6c` |
| `.setValue(9)` | `UnsupportedOperationException` | **succeeded** |

The second is the dangerous one: `Map.entry` is specified to return an
immutable entry, and a caller that defensively mutates a copy got no signal
that it had mutated something nobody would read. **Disposition: deleted.**
`java.util.Map.entry` is a static interface method with ordinary bytecode, so
removing the registration is the whole fix — contract §1.4's "the real bytecode
wins", applied by removing the thing that was winning. Both probes now match
HotSpot byte for byte, and `stub_ratchet` fell 554 → 553.

That is the disposition shape for the rest of the population: *a shadow is
adjudicated by a differential, not by a count.* The probe stays in `probes/` as
the regression test.

### The 11 application-class interceptions: measured inert

`probes/UserImplementorInterceptProbe.java` hands the VM a `Map` and a
`Collection` the probe wrote itself, whose methods answer values no JDK
implementation ever would (`get` returns `"LOUD:" + key`, `size` returns 4242),
and calls them through the interface type — the exact shape the natives are
registered on.

**Every answer comes from the application's bytecode.** The interception
surface is real and the dispatch does not use it. So the eleven registrations
stay, with evidence rather than a worry attached, and the probe is the
regression test that keeps it true. (The one divergence this probe did show was
`Map.entry`'s `toString`, above — not an interception at all.)

### The 796 dead registrations: a list, not a guess

`scripts/jdk-only-dead-sweep.py` intersects a census per image — JDK 21.0.12
and 25.0.4, linux and windows — filters `UNDECL` through the hierarchy, and
subtracts everything three workloads dispatched (`JdkOnlyCensusLoadProbe`,
`JdkOnlyBreadthProbe`, and an H2 in-memory SQL workload; 787 slots between
them). What survives is committed at
`scripts/baselines/jdk-only-dead-everywhere.tsv`:

* **243** whose class no supported image contains;
* **553** whose method is nowhere in its hierarchy on any of them;
* concentrated in `lang_string.rs` (119), `plain_socket.rs` (51),
  `nio_native.rs` (45), `shared_secrets_bridge.rs` (44).

**The dispatch filter removed nine, and every one was a VM-minted class wearing
a JDK name** — `java/util/HashMap$KeyItr` (1,209 dispatches),
`java/util/TreeSet$Itr` (501), `AtomicIntegerFieldUpdater$RustJvmImpl`,
`Function$Identity`. A census saying a `java.util` class does not exist can be
right about the JDK and wrong about this VM, and that is the third distinct way
this measurement has been misread.

Deleting them is the stub-removal wave's job, not this one's. What changed is
that the wave now has a list with four images and three workloads behind it.

## What is still open

1. **Delete the 796.** The list is measured and committed; removing the
   registrations is a stub-removal change with its own subsystem-per-PR
   discipline, and it should re-run the sweep afterwards rather than trusting
   this file.
2. **The differential covers what it covers.** `ShadowDifferentialProbe`
   exercises `java.util`'s factories and views. The other ~1,600 inherited
   shadows are unprobed, and the honest reading of "they match" is "the ones
   anybody looked at match". Widening that probe is the cheapest way to keep
   finding `Map.entry`-shaped defects.
3. **`jdk-only-adjudicate.py` section 3 now prints the inherited shadows as a
   separate addend** rather than folding them in, because L6's ratchet counts
   `has_code` on the named class and that number must keep meaning exactly
   that. Prose calling it "the shadows" still understates by ~1,600; the script
   now says so on every run.

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
