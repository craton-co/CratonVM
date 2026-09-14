# L4 residuals closed — the "dispatch finding" was a boolean, and the Path carrier had two type-confused slots

**Status: MEASURED AND FIXED, 2026-09-10.**
Closes every residual carried by
`L4-the-io-and-nio-worklist-49-defects-and-a-bounds-check-that-killed-the-vm-20260828.md`.
Worktree `/data/wt-l4io-20260910`, branch `claude/l4-io-nio-residuals-20260910`,
Linux (Azure host `vm1`), oracle **Temurin 25.0.4+7** (`/data/toolchain/jdk-25`).

---

## 0. What was carried, and what each turned out to be

| L4 record | carried as | what it actually was |
| --- | --- | --- |
| §4.3 / §P6.7 `FileInputStream.skip`, 4 rows | "a DISPATCH finding — the superclass body is entered for a method the subclass declares and overrides", twice measured unfixable | one native answering **false** for every file, because it read `args[0]` |
| §P6.8 `sun/nio/fs/UnixPath`, 102 armed rows | "the repair is the whole thing … a change to the hottest allocation in `java.nio.file`" | two constants; the layout is resolvable by name |
| §4.4 / §P6.6 the fabricated abstract provider, `Files` 6 armed rows | "nominated out of this lane … the fix is to mint the real classes" | the remedy this codebase already runs for twelve other `java.nio` classes |
| `WORKER-4-NOTE-6` N2, the typed view classes | "at every width, over both backings and BOTH byte orders" | one order was covered; one row of the other |

---

## 1. The residual was a boolean, not a dispatch defect

`FileInputStream.skip` in JDK 25 is not one body, it is a branch:

```text
public long skip(long) throws java.io.IOException;
     0: aload_0
     1: invokevirtual  isRegularFile:()Z
     4: ifeq           13
     7: aload_0
     8: lload_1
     9: invokevirtual  skip0:(J)J        <- one lseek
    12: lreturn
    13: aload_0
    14: lload_1
    15: invokespecial  java/io/InputStream.skip:(J)J   <- read-and-discard
    18: lreturn
```

Both prior investigations observed the read-and-discard answers, and both
concluded the VM was entering the SUPERCLASS's body for a method the subclass
overrides. It was entering the subclass's body, which chose the superclass's on
a false. `apps/probes/L4SkipDiag.java` asks the predicate directly:

```text
                     HotSpot     CratonVM (before)
isRegularFile()      true        false
isRegularFile0       true        false
length0              2           2
position0            0           0
skip(4) at EOF       4           0
channel position     6           2
skip0(4) direct      4           2
skip(-1)             IOException 0
```

`length0` and `position0` are right, so the descriptor is fine and the fd table
is fine. Only the predicate is wrong, and its body says why:

```rust
/// `java.io.FileInputStream.isRegularFile0(FileDescriptor)Z` — static, so
/// `args[0]` is the descriptor rather than a receiver.
```

**It is not static.** `javap` shows `private native boolean
isRegularFile0(java.io.FileDescriptor)` with no `ACC_STATIC`, and the call site
is `aload_0; aload_0; getfield fd; invokevirtual` — a receiver AND an argument.
So `args[0]` is the `FileInputStream`, `args[1]` is the descriptor.

The reason nothing failed loudly is the part worth keeping: the body asked
`args[0]` for a field called **`fd`**, and `java.io.FileInputStream` *declares* a
field called `fd`. The read SUCCEEDED. It returned `Value::Object(<the
FileDescriptor>)` where the match wanted `Value::Int`, fell through both arms,
and produced `None` — "no descriptor" — which `is_some_and` turned into `false`.
An `args[0]` that resolves is not an `args[0]` that is right, and a receiver and
its first parameter that share a field name is the shape where the two are
indistinguishable at the read.

### What that did to the twice-reverted fix

A seek-based `skip0` was written on 2026-08-28 and again on 2026-08-30, and
measured INERT both times — correctly, because the branch in front of it was
never taken. The second reversion is recorded in the L4 record as a general
rule: *"code that cannot run is worse than the absence of it"*. The rule is
right and it was applied to the wrong half. **Inert code is evidence about its
guard, not only about itself**; two correct fixes were deleted because the thing
in front of them was never suspected.

### The three edits

* `native_fis_is_regular_file0` reads the declared parameter (`args[1]`), with
  the receiver's own `fd` field as a fallback — which is what the call site
  passes anyway.
* `native_fis_skip` becomes HotSpot's `skip0`: `cur = position; end =
  seek(Current(n)); return end - cur`, with either failure raised as
  `IOException`. `BufReader`'s `Seek` is logical (it subtracts what it holds
  buffered), so `SeekFrom::Current(n)` means to a Java caller what `lseek` means
  to HotSpot, which reads through no buffer at all. A non-seekable descriptor —
  a pipe, a socket, stdin — keeps the read-and-discard body, because the real
  `skip` would not have called `skip0` for one, and this native answers the
  `skip(J)J` triple too (as a `SyntheticStub`, for the synthetic-JDK build where
  there is no bytecode to make that choice).
* `native_fis_position0` stops being `length - available`. That identity breaks
  at exactly the position this residual is about: an `lseek` may leave the
  cursor PAST the end, `available()` clamps at zero there, so the subtraction
  pinned the answer to `length`. It is now `rw_position`, an ftell.

---

## 2. `Path`'s two type-confused slots

A Path this VM hands out is allocated stamped with the INTERFACE
`java/nio/file/Path`, and the `get_class_display` alias table makes `getClass()`
report `sun.nio.fs.UnixPath`. That class is real and fully loaded here — 52
declared methods, 7 declared fields, byte-identical to HotSpot's. Its instance
layout is

```text
fs(0)   path(1)   stringValue(2)   hash(3)   offsets(4)
```

and the natives wrote into it with `P57_PATH_FIELD = 0` and
`P57_PATH_FS_FIELD = 1` on an object **two slots wide**: the path STRING where a
`UnixFileSystem` belongs, the owning filesystem where a `byte[]` belongs, and
nothing at all in the three slots the real bodies read.

### Why the earlier attempts were inert, and what the difference is

Two attempts wrote `stringValue` **by name through the object** —
`set_field_by_name`, then `resolve_field_index_by_class_id(class_id_of_object(obj))`
— and both were no-ops, correctly: the object's stamp is the interface, and
every name-based route resolves against the stamp.

Resolving the index against the **implementation class** and writing it by index
is not the same operation, and it is the one the real bytecode itself performs:
`getfield` resolves against its own constant pool's class, never against the
receiver's stamp. So an object stamped `java/nio/file/Path`, allocated five
slots wide, with a `byte[]` at index 1 and a `String` at index 2, is exactly what
`UnixPath.toString()` and `UnixPath.compareTo` read.

### The map is resolved, not transcribed

`cratonvm_native_api::path_layout` asks the implementation class for `fs`,
`stringValue` and `path` by name and caches the answer once per process. That
also removes a `cfg`: `WindowsPath` has no `stringValue` at all — its `path`
field IS the `String` — so the Unix pair (encoded bytes plus a lazily built
String) and the Windows single slot are the same two questions asked of
whichever class is really there.

### Both slots publish together

`stringValue` alone would have been worse than the defect. Real
`UnixPath.compareTo` (so `equals`), `hashCode` and `initOffsets` — which
`getFileName`/`getParent`/`getNameCount` all go through — read the `byte[]`, and
a null there is an NPE where the old layout gave a wrong answer. This is the
`install_real_stream_fields` rule from the same record's §P6.1, one class over:
the slots of one object publish together or not at all.

### It lives in `native-api` because the carrier has three producers

`p57_alloc_path` was not the only site. `native-builtins` allocates a Path at
five more places (the jar and jrt walkers), and `native-io` at three. All eight
wrote the String at slot 0. A slot map kept in either crate would have been a
second, drifting copy in the other — the `two-producers-of-one-carrier-class`
family — so it went where `appended_slots` and `instantiable` already live, and
every producer now goes through one writer.

---

## 3. The fabricated abstract provider

`p57_alloc_provider` minted the default `FileSystemProvider` as an instance of
the ABSTRACT `java/nio/file/spi/FileSystemProvider` — a class JVMS §6.5 says
`new` cannot produce. `Files.probeContentType`'s real bytecode reaches
`DefaultFileTypeDetector.create()`, which calls `getFileTypeDetector()` on
`DefaultFileSystemProvider.instance()`; the abstract base declares no such
method, so the call died with a `NoSuchMethodError` and took a probe run with
it. The record nominated it out of the lane as "the same shape as the roadmap's
Phase-1 `MemorySegment` row … the fix is to mint the real classes".

**That fix is a module in this workspace, and twelve other `java.nio` classes
already use it.** `native-io/src/concrete_receiver.rs` mints the first
instantiable candidate of a platform-ordered list and, in the same breath,
mirrors the registrations onto it — because native dispatch keys on the
receiver's runtime class, so moving a receiver without moving its rows runs the
JDK's own bodies against state this VM never initialised. `FILE_STORE_IMPLS` and
`DIR_STREAM_IMPLS` are two lists in this very file, twenty lines apart from the
provider allocator that did not have one.

The scheme String moves with it: slot 0 on the real class is `theFileSystem`, so
the literal `0` was the same type-confused-slot species as `Path`'s, one class
over. It is now a private slot above the real layout, through
`appended_slots::base_for_object`, which the `getScheme` reader also calls.

---

## 4. A defect the byte-order arm found on the way past

`WORKER-4-NOTE-6` N2 asked for the typed VIEW classes "at every width, over
BOTH backings and BOTH byte orders". `L4TypedBufferSweep` had the widths and the
backings and **one row of one order**. Adding the second order — and, with it, a
read-only view of each order, because `ByteBufferAs<T>Buffer R{B,L}` is a
second implementation class per pair — found this:

```text
viewRo = ByteBuffer.allocate(16).asIntBuffer().asReadOnlyBuffer()

                                  HotSpot                    this VM
viewRo.isReadOnly()               true                       true
IntBuffer.isReadOnly (the FIELD)  false                      false
viewRo.put(0)                     ReadOnlyBufferException    ReadOnlyBufferException
viewRo.put(new int[]{5}, 0, 1)    ReadOnlyBufferException    no-throw
bb.getInt(0) afterwards           0                          5
```

**A silent write through a read-only handle, into the caller's own
`ByteBuffer`.** `buffer_is_read_only` reads the `isReadOnly` FIELD by name,
which is the whole answer for `HeapByteBufferR` and its siblings — their
constructors set it — and is NOT the answer for the view classes, which leave
the field false and override `isReadOnly()` to return `true`. The field is false
on HotSpot too, which is why reading it is not a defect anyone would spot by
inspection.

**Why one of the three `put` overloads and not the other two.** The JDK's
read-only view classes DECLARE `put(int)` and `put(int,int)`, so those dispatch
to their own bodies and refuse. They do NOT declare `put(int[],int,int)`, whose
most-derived declaration is on the abstract `IntBuffer` this VM registers
against — and *the dispatch door asks about the DECLARING class*. So one door of
three was ours, and it was the bulk one.

The guard now asks the receiver: the field first (our own carriers keep it
there, and answer without a virtual call), and `isReadOnly()` for a receiver
this VM did not allocate.

---

## 5. The measurements

### 5.1 The thirteen differential probes, three arms each

Same runner, same oracle, same host as the 2026-08-28 record
(`apps/probes/l4run.sh`, Temurin 25.0.4+7, `vm1`). The BEFORE column is this
tree's own `origin/dev` tip, built and measured first, not the record's number.

```text
probe                    rows    before          after
L4CensusTail              126    0               0
L4BridgeSweep             497    0               0
L4TailSweep2              187    0               0
L4TypedBufferSweep        991    0  (501 rows)   0    <- widened, §4
L4FileSweep               486    0               0
L4FilesSweep              395    0               0
L4ByteBufferSweep         404    0               0
L4PrintStreamSweep        123    0               0
L4StreamTailSweep         212    8               0    <- the residual
TailFamilySweep           117    0               0
IoSystemSweep             154    0               0
FilesSweep                 39    0               0
FilePathSweep             666    0               0
                        -----
                         4397    8 differing      0
```

**Every row identical to HotSpot in BOTH modes**, where the lane's own record
ended at "2920 of 2924, four residual". The four residual rows are the eight
differing lines in `L4StreamTailSweep`, and they are the whole of §1.

### 5.2 The retirement dial, per family

`apps/probes/l4famsweep.sh`, one family armed at a time, thirteen probes each,
oracle captured once. **The floor was `DIFF 2` — the `FileInputStream.skip`
residual, present in every column. It is now 0.**

```text
SCOPE                                    before   after
java/io/PrintStream                           2       0
java/io/File                                  2       0
java/io/FileInputStream                       2       0
java/io/FileOutputStream                      2       0
java/io/ByteArrayInputStream                  2       0
java/io/ByteArrayOutputStream                 2       0
java/io/DataInputStream                       2       0
java/io/DataOutputStream                      2       0
java/io/BufferedReader                        2       0
java/io/BufferedWriter                        2       0
java/io/FilterOutputStream                    2       0
java/nio/ByteBuffer                           2       0
java/nio/CharBuffer                           2       0
java/nio/file/spi/FileSystemProvider          2       0
java/nio/file/attribute/                      2       0
java/nio/channels/FileChannel                 2       0
java/nio/file/Files                           6       4
java/nio/file/Path                          102     100
```

**Sixteen of eighteen families arm at zero**, and now zero means zero rather
than "the floor".

### 5.3 What the last two are, exactly — and why the layout fix could not move them

Narrowed per probe rather than quoted as a total, because a total hides which
question is being missed.

**`Files` is `probeContentType`, and nothing else.** One row, two lines:

```text
Files.probeContentType(<a .txt>)
  HotSpot / unarmed   no-throw
  armed               NoSuchMethodError
```

That is §3's provider, reached the only way it can be reached. Every other row
of every other probe is 0 under an armed `java/nio/file/Files`.

**`Path` is `Object.toString()` running, in all 100.**

```text
path[] toString        HotSpot ""      armed  sun.nio.fs.UnixPath@0
path[.] toString       HotSpot "."     armed  sun.nio.fs.UnixPath@2e
path[] equals-self     HotSpot true    armed  false
```

**And this is the honest half of §2.** The layout fix put the right bytes in the
right slots — provable by reflection, which resolves through the real class:

```text
field         HotSpot                      before            after
path          byte[3] = a/b                NULL              byte[3] = a/b
stringValue   NULL                         NULL              "a/b"
fs            sun.nio.fs.LinuxFileSystem   String = "a/b"    NULL
```

— and it moved **no row of any probe, armed or unarmed**, because the armed
dial's failure is not about the fields. `UnixPath.toString()` is not being
entered at all: the object's STAMP is the interface `java/nio/file/Path`, which
declares no `toString` body, so the virtual walk goes straight to
`java.lang.Object`. The identity string with a hash of `0` is the tell.

So the Path carrier had TWO defects, not one:

1. **the contents** — two type-confused slots, fixed here, and a real defect on
   its own terms (the owning `FileSystem` was stored where a `byte[]` belongs);
2. **the stamp** — an interface, so no real body is reachable and no
   `instanceof UnixPath` succeeds. Untouched, and it is the whole of the
   remaining 100.

Fixing (1) does not move a number today. It is recorded as such rather than
claimed as a win, and it is a genuine prerequisite: minting the concrete class
without the right slot map would hand real `UnixPath` bodies a `byte[]` field
holding a `UnixFileSystem`, which is worse than what they get now.

### 5.4 The two remaining rows are ONE nomination, and its order is measured

§3's provider mint was written, built and run — the whole thing, both halves,
including mirroring twenty-five registrations onto seven concrete classes. It
does not stand alone:

```text
Files.isExecutable(p), with the provider minted concrete
  at sun/nio/fs/UnixPath.toUnixPath(UnixPath.java:177)
  at sun/nio/fs/UnixFileSystemProvider.isExecutable(UnixFileSystemProvider.java:347)
  -> java.nio.file.ProviderMismatchException

L4FilesSweep   0 -> 172 differing lines, dead at row 226 of 395
```

Mirroring moves the rows this VM registers; it cannot move the ones it does
not. `isExecutable` has no native here, so a concrete provider hands it to the
JDK's own body, whose first act is `instanceof UnixPath` — which an
interface-stamped Path fails. **So the provider needs the Path stamp first**,
and the Path stamp needs ~100 registrations mirrored onto `sun/nio/fs/UnixPath`
in the same commit, because a native registered on an INTERFACE fires through no
door once the receiver is a concrete class.

Left as ONE nomination with both halves named and the order measured, rather
than as two that look independent. The provider's scheme slot moved to a private
index above the real layout in this change anyway, so the layout half of it is
already in place and the mint is a three-line edit when the Path half is ready.


---

## 6. Reproduce

```bash
# the four-row residual, all three arms
javac -d apps/probes/out apps/probes/L4SkipDiag.java
java --add-opens java.base/java.io=ALL-UNNAMED -cp apps/probes/out L4SkipDiag
cratonvm --java-home "$JDK" --jdk-only --add-opens java.base/java.io=ALL-UNNAMED \
    -cp apps/probes/out L4SkipDiag

# the thirteen differential probes, three arms each
CV=/data/vm-l4res bash apps/probes/l4run.sh L4CensusTail L4BridgeSweep \
    L4TailSweep2 L4TypedBufferSweep L4FileSweep L4FilesSweep L4ByteBufferSweep \
    L4PrintStreamSweep L4StreamTailSweep TailFamilySweep IoSystemSweep \
    FilesSweep FilePathSweep

# per-family retirement cost, one family armed at a time
bash apps/probes/l4famsweep.sh
```
