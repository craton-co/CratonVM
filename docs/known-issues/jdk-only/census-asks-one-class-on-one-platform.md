# The census asks one class, on one platform — and ~2,000 rows were filed under the wrong verdict

**Status:** OPEN as a *reading* problem; the two instruments that close it now
exist and ship (`scripts/jdk-only-inherited-decl.sh`,
`scripts/jdk-only-platform-diff.py`). What is still open is that every
consumer of `image_declaring_method` — including
[`scripts/jdk-only-adjudicate.py`](../../../scripts/jdk-only-adjudicate.py)'s
summary table, L6's ratchet narrative, and three records in this directory —
was written against the narrower reading. Filed 2026-08-05 while closing the
L5/L5b/L5c residuals.

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

## What is still open

1. **`scripts/jdk-only-adjudicate.py`'s section 2 labels the bucket "class
   present, method not declared" and its consumers gloss that as "dead".** The
   script is L6's file and is not changed here beyond a pointer at the new
   tool; the honest fix is a fourth column.
2. **L6's ratchet counts `has_code` on the named class only.** By the wider
   reading the true shadow population is 4,696 + 1,612 = **6,308**, not 4,696.
   The ratchet's *number* is well defined and unmoved — do not re-baseline it —
   but any prose that calls 4,696 "the shadows" understates by a third.
3. **308 inherited-abstract rows** are the `register_interface_natives` hazard
   in a second place: a native on a method that is abstract on a supertype
   intercepts every implementor, including user subclasses. Nothing has counted
   which of them are dispatched against a user class.
4. **1,735 rows dead on both platforms.** That is a deletion list, and deleting
   is a different wave than stating.

## Reproducing

```sh
# the census (both instruments consume it)
cratonvm --real-jdk --java-home "$JAVA_HOME" --explain-jdk-only \
    --dump-native-registry census.json -cp probes L5rProbe

# 1. inheritance
JAVA_HOME=<same image> sh scripts/jdk-only-inherited-decl.sh census.json

# 2. platform — unpack a Windows JDK of the SAME version, then
cratonvm --real-jdk --java-home <windows-jdk> --explain-jdk-only \
    --dump-native-registry census-win.json -cp probes L5rProbe
python3 scripts/jdk-only-platform-diff.py census.json census-win.json linux windows
```

Both refuse rather than print zeroes when the census lacks
`--explain-jdk-only`, and the platform diff refuses if the two censuses do not
correspond row-for-row.
