# L4 — the `java.io` / `java.nio` worklist: 199 native-won triples, 85 defects, 8 shadows retired, and a bounds check that killed the VM

**Status: MEASURED AND FIXED, 2026-08-28.** Lane L4 of
`HANDOFF-20260828-SCOPE.md`. Worktree `/data/cvm-l4io-20260828`, branch
`claude/l4-io-nio-20260828`.

**Provenance.** Linux (Azure host `vm1`), oracle **Temurin 25.0.4+7**
(`/data/toolchain/jdk-25`). Every row below is one program run on three VMs —
HotSpot, `cratonvm --jdk-only`, and `cratonvm` in the default mode — diffed on
**stdout only**. Probes, all new in this lane:

| probe | rows | covers |
| --- | ---: | --- |
| `apps/probes/L4FileSweep.java` | 486 | the 50 `java/io/File` triples |
| `apps/probes/L4FilesSweep.java` | 395 | `java/nio/file/Files` 31, `Path` 9, `Paths` 1 |
| `apps/probes/L4ByteBufferSweep.java` | 404 | `ByteBuffer` 11, `DirectByteBuffer` 9, `HeapByteBuffer` 4 |
| `apps/probes/L4PrintStreamSweep.java` | 123 | `PrintStream` 25, `PrintWriter` 1 |
| `apps/probes/L4StreamTailSweep.java` | 208 | the `java.io` stream tail and the `java.nio` tail |
| `apps/probes/L4Reach.java` | — | NOT a differential probe; it exists only to make the census |

**1616 differential rows, 1615 identical in both modes.** The one residual is
§4, and it is not a missing fix — it is a resolution finding no registrar edit
can move.

Four EXISTING probes were re-run on the final binary as a control, because a
lane that only runs its own probes cannot see what it broke — and one of them
found work: `TailFamilySweep` 0, `IoSystemSweep` 0, **`FilePathSweep` 94 → 0**
(§3.8), `FilesSweep` 0 apart from its own random temp-directory name.

---

## 1. The worklist was mined, not chosen

`L4Reach` touches 370 distinct `java.io` / `java.nio` operations and asserts
nothing; its only job is to give `--jdk-only-report` something to observe.
Filtering that report's `native-shadows-bytecode` rows to
`outcome == "native-won"` and to the two packages gives the lane's real surface:

```text
199 distinct native-won triples in java/io + java/nio

  50  java/io/File                 31  java/nio/file/Files
  25  java/io/PrintStream          11  java/nio/ByteBuffer
  10  java/io/DataOutputStream      9  java/io/ByteArrayOutputStream
   9  java/nio/DirectByteBuffer     9  java/nio/file/Path
   8  java/io/DataInputStream       7  java/io/ByteArrayInputStream
   5  java/io/BufferedWriter        5  java/io/FileOutputStream
   4  java/io/BufferedOutputStream  4  java/nio/HeapByteBuffer
   + a one-and-two-row tail: CoderResult, ByteOrder, CharBuffer, Buffer,
     Bits, FileTime, FileDescriptor, FilterInputStream, PrintWriter, Paths,
     FileSystemProvider
```

The same run's counts are the definition-of-done predicate and they hold:
`compatibility_classes: 0`, `synthetic_stub_invocations: 0`.

**`java.nio.ByteBuffer` was clean on the first run and never moved** — 404 rows
over three backings (heap, direct, and a wrapped array with a non-zero offset),
0 differences before any fix in this lane. That is worth stating as loudly as
the defects; §6 has the list.

---

## 2. The panic: a bounds check that was not there at all

```text
PrintStream ps = new PrintStream(sink, false, UTF_8);
ps.write(new byte[]{1}, 0, -1);

  HotSpot   IndexOutOfBoundsException
  CratonVM  thread 'main-vm' panicked at raw_vec/mod.rs:28: capacity overflow
```

`native_printstream_write` read the length as `*l as usize`, and `-1 as usize`
on a 64-bit host is 18 446 744 073 709 551 615 — a `vec![0u8; len]` request for
eighteen exabytes. The panic escapes as `internal error: native method panic`,
which no Java handler can catch.

Two things follow, and the second matters more:

* the row **before** it, `write(b, -1, 1)`, did not panic. It read
  `arr[usize::MAX]`, got zero, and **printed a NUL byte** where HotSpot throws.
  A silent wrong byte on the way to a stream is the worse of the two;
* the panic killed the probe **72 rows before its end**, and nine of those 72
  rows were themselves defects. *A crash early in a probe masks every later
  defect* is the scope doc's warning, and this is what it looks like: fixing the
  crash did not finish the family, it opened it.

---

## 3. Fifty-two defects, and where they cluster

Every one is on a contract edge — nulls, bounds, refusal TYPES, argument
validation, callback boundaries, platform predicates. **Not one is a wrong
answer to an ordinary call**, in any of the six families.

### 3.1 `java.io.File` — 8

| | HotSpot | CratonVM |
| --- | --- | --- |
| `new File("a/./b").getParentFile()` | `a/.` | `a` |
| `new File("/.").getParentFile()` | `/` | `null` |
| `new File((String) null, "c")` | `c` | `/c` |
| `new File((String) null)` | NPE | no-throw |
| `new File("a", (String) null)` | NPE | no-throw |
| `f.compareTo(null)` | NPE | **0** |
| `f.renameTo(null)` | NPE | false |
| `f.setLastModified(-1)` | IAE | no-throw |
| `new File(new URI("foo/bar"))` | IAE | no-throw |
| `new File(new URI("http://x/y"))` | IAE | no-throw |
| `new File((URI) null)` | NPE | no-throw |

**`getParentFile` is the one that reaches ordinary code.** It used
`std::path::Path::parent`, whose COMPONENTS normalise `.` away, while
`getParent()` transcribes the JDK's own last-separator split — so two methods of
the same object disagreed about the same path. The JDK's body makes that
impossible: `String p = getParent(); if (p == null) return null; return new
File(p, this.prefixLength);`. The second row is the one that bites, because
`new File(".").getAbsoluteFile().getParentFile()` is the ordinary way to name
the working directory and it answered the directory ABOVE it.

That same defect **broke the probe's own diff token**: `L4FileSweep` built its
`<CWD>` replacement with `getParentFile()`, so one defect in the method under
test renamed the token and reported six unrelated rows as differences. The token
is now built with a string operation. *Do not normalise a probe's output with a
method the probe is testing.*

`compareTo(null)` answering **0** is the one to keep. Zero means "these two
files are the same", which is the single answer a sort or a `TreeSet` acts on
destructively.

The null-parent rule is two branches the JDK keeps apart and this VM had merged:
`parent == null` normalises the child, `parent == ""` resolves it against the
default parent. Merging them moved every relative path to the filesystem ROOT.

### 3.2 `java.nio.file.Path` — 4

```text
Paths.get("a/bc").startsWith("a/b")   HotSpot false   CratonVM true
Paths.get("ab/c").endsWith("b/c")     HotSpot false   CratonVM true
Paths.get("a/b/c").startsWith("")     HotSpot false   CratonVM true
Paths.get("a/b/c").equals("a/b/c")    HotSpot false   CratonVM true   <- a String
Paths.get("/").getFileName()          HotSpot null    CratonVM ""
```

`startsWith` and `endsWith` were `String::starts_with` / `String::ends_with`.
**A path is not text**: it is a root plus a sequence of NAMES. The first row is
the dangerous one — `startsWith` is how a program asks "is this file inside that
directory", the shape of nearly every path-traversal check ever written, and a
text prefix answers yes for `/appsecret` under `/app`.

The two rules are not mirror images, and the asymmetry is the part a paraphrase
drops: `startsWith` requires the SAME root on both sides, while `endsWith`
accepts a relative suffix of an absolute path and demands a root match only when
`other` is itself absolute.

`equals` had no type test at all, so it was not symmetric — `path.equals(str)`
true and `str.equals(path)` false gives a `HashSet` and a `List.contains`
different answers depending on which side holds which. The replacement test is
deliberately generous (this VM's own `java/nio/file/Path`, anything
`synthetic_implements_declared` admits, or a class whose simple name ends
`Path` — which is every real JDK implementation), because a false here would be
a REGRESSION for a real `sun.nio.fs.UnixPath` reaching the same native.

`getFileName()` at a root is null in the JDK and this VM answered `""`. The
non-null was DELIBERATE, with a comment naming a Keycloak consumer that NPEs on
a null. That consumer would NPE on HotSpot too — so the workaround was hiding
whatever hands it a root path rather than fixing it. The repair is a ROOT test,
not a blanket null, because `Paths.get("").getFileName()` is a non-null EMPTY
path in the JDK and that row was already right.

### 3.3 `java.nio.file.Files` — 15

**The largest single defect is a type that is not an `IOException` at all.**

```text
Files.copy(<missing>, t)             HotSpot NoSuchFileException
                                     CratonVM IllegalStateException
Files.move(s, <missing dir>/x)       the same pair
```

`copy` and `move` both ended `Err(e) => Err(IllegalStateException { message:
format!("IOException: {e}") })`. The message shows the intent — it knew which
exception it meant and could not spell it — and the consequence is that `catch
(IOException)`, the handler the compiler REQUIRES around both methods, does not
match, so the failure unwinds straight past the code written to handle it. Both
now map through one `p57_fs_error` that answers `NoSuchFileException` /
`AccessDeniedException` / `FileAlreadyExistsException` /
`DirectoryNotEmptyException` / `NotDirectoryException` / `FileSystemException`
by errno.

| | HotSpot | CratonVM |
| --- | --- | --- |
| `Files.exists(null)`, `isDirectory(null)`, `isRegularFile(null)` | NPE | **false** |
| `Files.size(null)` | NPE | NoSuchFileException |
| `Files.copy(a, b, (CopyOption[]) null)` | NPE | no-throw |
| `Files.readAttributes(f, (Class) null)` | NPE | the basic view |
| `Files.readAllLines(<missing>)`, `lines(<missing>)` | NoSuchFileException | IOException |
| `Files.readString(<broken UTF-8>)` | MalformedInputException | no-throw, U+FFFD |
| `Files.write(p, b, READ)` | IAE | opened and TRUNCATED |
| `Files.copy(dir, <non-empty dir>, REPLACE_EXISTING)` | DirectoryNotEmptyException | no-throw |
| `Files.walkFileTree(<missing>, v)` | NoSuchFileException | no-throw |
| `Files.getFileStore(<missing>)` | NoSuchFileException | a FileStore |
| `Files.setLastModifiedTime(<missing>, t)` | NoSuchFileException | IOException |
| `Files.newOutputStream(<a directory>)` | FileSystemException | IOException |
| `Files.newByteChannel(<a directory>, WRITE)` | FileSystemException | IOException |
| `Files.probeContentType(f)` | a type, or null | **NoSuchMethodError** |

`Files.exists(null)` answering **false** is the shape that makes
`if (!Files.exists(p)) create(p)` take the create branch for a caller whose `p`
is null by mistake.

`Files.write(p, b, READ)` is the half-fixed pair in miniature. The refusal
helper (`fsp_output_stream_option_refusal`) already existed and
`newOutputStream` already called it; `Files.write` did not. The same option was
refused through one door and, through the other, opened the file for WRITING and
truncated it.

`Files.getFileStore` is the same shape found by reading the registry rather than
the source: the `Files`-level copy carried the existence check and the PROVIDER
copy — the one the real bytecode reaches, because `Files.getFileStore` has no
`Files`-level native — did not.

### 3.4 `SimpleFileVisitor` swallowed every walk error

```java
Files.walkFileTree(<missing directory>, new SimpleFileVisitor<Path>() {});
  HotSpot   NoSuchFileException
  CratonVM  returned normally
```

Two defects, and they had to be fixed together.

`Files.walkFileTree` never looked at the start node: a path that is not there
produced a completed walk over nothing. The JDK reads the start node's
attributes first and, when that fails, calls `visitor.visitFileFailed(start,
ioe)` — through the visitor, because a visitor is entitled to override it.

And `SimpleFileVisitor.visitFileFailed` / `postVisitDirectory` — the class the
javadoc recommends, and the one every walk that does not care about errors uses
— **answered `CONTINUE` where the JDK's one-line bodies rethrow.** So even after
the start-node repair the callback fired and threw the exception away. The same
pair silently skipped any unreadable subdirectory in the middle of a real walk,
and the caller saw a completed traversal.

That repair cost a build to a **reversed argument index**: `args[0]` is the
RECEIVER, so the `IOException` is `args[2]`, and reading `args[1]` threw the
PATH — a `java/nio/file/Path` handed to the interpreter as a Throwable, which
surfaced as `Exception in thread "main" java/nio/file/Path` with no message and
no frames and killed the probe 79 rows early. **A native's `args[1]` is the
first PARAMETER, not the first argument of the Java signature.**

### 3.5 `FileChannelImpl.truncate(-1)` deleted the file

```text
Files.newByteChannel(p, WRITE).truncate(-1)
  HotSpot   IllegalArgumentException: Negative size
  CratonVM  no-throw — and the file's contents were gone
```

`java/nio/channels/FileChannel.truncate` had carried the check since it was
found there. `sun/nio/ch/FileChannelImpl.truncate` — the CONCRETE copy, which is
what a real receiver reaches and therefore what every
`Files.newByteChannel(..).truncate(..)` runs — still had `.max(0)`. **The
half-fixed duplicate pair the scope doc's §5 predicts**, found by measuring the
concrete receiver rather than by reading either body.

### 3.6 `java.io.PrintStream` — 8

Beyond the panic in §2:

```text
new PrintStream(sink, true, ISO_8859_1).print("é中")
  HotSpot   e9 3f              (latin1: e-acute, '?' for the unmappable char)
  CratonVM  c3 a9 e4 b8 ad     (UTF-8)

new PrintStream(sink, true, US_ASCII).print("a中b")
  HotSpot   61 3f 62
  CratonVM  61 e4 b8 ad 62
```

**The stream's charset was ignored on every text write.** The second row is the
one to keep: a stream declared `US_ASCII` emitted a three-byte sequence with the
high bit set, so anything downstream that trusts the declared encoding — a
fixed-width record writer, a protocol framer, a terminal — is handed bytes it
cannot represent, and the byte length no longer matches the character length.
The encoder discriminates on the charset object's CLASS NAME (allocation free,
the standard charsets each have their own final class) and falls through to the
JDK's own `String.getBytes(Charset)` for anything but UTF-8 / ISO-8859-1 /
US-ASCII.

| | HotSpot | CratonVM |
| --- | --- | --- |
| `new PrintStream(sink, true).print("a")` → sink flushes | 1 | **0** |
| `… .println("b")` | flushed | not flushed |
| `… .write('\n')` | flushed | not flushed |
| `ps.close(); ps.print("z"); ps.checkError()` | true | **false** |
| `new PrintStream((OutputStream) null)` | NPE | no-throw |
| `ps.print(<Object whose toString() returns null>)` | NPE | printed "null" |
| `new PrintStream("<missing dir>/x")` | FileNotFoundException | IOException |
| `pw.close(); pw.print("x"); pw.checkError()` | true | **false** |

The autoflush was not a write-path bug: the constructor **never stored
`autoFlush`**, so the field was null and no consultation of it could have
worked. A `new PrintStream(socket.getOutputStream(), true)` — the ordinary way
to write a line protocol — buffered indefinitely. Its three rows also need three
different rules, which is why one hook does not serve them:
`write(byte[], off, len)` flushes UNCONDITIONALLY, `write(int b)` only for
`'\n'`, and every `print`/`println` reaches the first of those through the
internal `OutputStreamWriter`.

`checkError()` after `close()` is the sharper one. `PrintStream` swallows every
`IOException` into that flag, and the flag is the class's ONLY channel for
reporting failure — a `PrintStream` whose `checkError()` cannot become true has
no error reporting at all. The repair is the JDK's own `ensureOpen()`: a write
that finds neither a Java sink nor a descriptor is `IOException("Stream
closed")`, caught into `trouble`.

`print(Object)` is the one row where a null must NOT be substituted:
`String.valueOf(obj)` is specified as `obj == null ? "null" : obj.toString()`,
so a null ARGUMENT prints "null" and a null RESULT is an NPE. The VM's shared
`invoke_to_string` coerces — correctly, for `StringBuilder.append(Object)` — so
the distinction had to be made at this call site.

### 3.7 the `java.io` stream tail — 13

| | HotSpot | CratonVM |
| --- | --- | --- |
| `new ByteArrayInputStream(null)` | NPE | no-throw |
| `bais.read(null, 0, 1)` | NPE | **-1** |
| `dis.readFully(null)` | NPE | returned normally |
| `dos.writeUTF(null)` | NPE | wrote an empty record |
| `dos.writeUTF(<70 000 chars>)` | UTFDataFormatException | IOException |
| `new DataOutputStream(null).writeInt(1)` | NPE | counted 4 bytes written |
| `bos.write(null, 0, 1)`, `bos.write(b, -1, 1)` | NPE / AIOOBE | no-throw |
| `new BufferedOutputStream(os, 0)` | IAE | no-throw *(default mode only)* |
| `bw.write((String) null, 0, 1)` | NPE | **wrote the character `n`** |
| `bw.write("ab", -1, 1)` | StringIndexOutOfBounds | IndexOutOfBounds |
| `fos.write(null)`, `fos.write(null, 0, 1)` | NPE | no-throw |
| `fos.write(b, 0, 9)` | IndexOutOfBounds | ArrayIndexOutOfBounds |
| `new FileOutputStream("<missing dir>/x")` | FileNotFoundException | IOException |
| `new FileDescriptor().sync()` | SyncFailedException | no-throw |

`bais.read(null, 0, 1)` answering **-1** told the caller the stream had ended.
`dis.readFully(null)` returning normally reported a SUCCESSFUL full read of a
record that was never read. `new DataOutputStream(null).writeInt(1)` counted
four bytes into `size()` and wrote them nowhere. Every one of those is the same
species: a failure reported as an ordinary, plausible success.

`bw.write((String) null, 0, 1)` is the sharpest. The `BufferedWriter` natives
DELEGATE to the wrapped writer and forwarded the null blind; somewhere down the
chain it was stringified and **one character of the word "null" was written to
the file as data**.

The two type rows run the OTHER way from the rest of this campaign:
`ArrayIndexOutOfBoundsException` is a SUBCLASS, so `catch
(ArrayIndexOutOfBoundsException)` matched at `FileOutputStream.write` where
HotSpot's does not. Its `BufferedOutputStream` neighbour genuinely answers
BOTH, depending on which branch of `implWrite` the length picks —
`System.arraycopy` for a short write, `Objects.checkFromIndexSize` in the
delegate for a long one — so that branch is reproduced rather than a single type
picked.

### 3.8 A BACKSLASH IS NOT A SEPARATOR ON UNIX — 47 rows in one existing probe

This one was not on the mined worklist. It came from re-running four EXISTING
`java.io` probes on the final binary as a control, which is a step a lane that
only runs its own probes never takes. `probes/FilePathSweep.java` was **94
differing lines**, and every one of them had a backslash in the input.

```text
new File("..\\..\\up").getName()      HotSpot "..\..\up"   CratonVM "up"
new File("..\\..\\up").getParent()    HotSpot null         CratonVM "..\.."
new File("\\x").isAbsolute()          HotSpot false        CratonVM true
new File("trailing\\").getName()      HotSpot "trailing\"  CratonVM "trailing"
new File("relative\\win\\path").toURI()
                        HotSpot   file:/…/relative%5Cwin%5Cpath
                        CratonVM  file:/…/relative/win/path
```

Two causes, both a platform predicate written as if it were universal:

**`u_is_sep` answered "`/` or `\`, on every platform"**, with a comment saying
so. It is not true. `java.io.File` delegates to `FileSystem`, and
`UnixFileSystem`'s separator is `/` alone — on Unix a backslash is an ORDINARY
FILENAME CHARACTER and a file really can be called `a\b`. That predicate feeds
`getName`, `getParent`, `getParentFile`, `isAbsolute`, the prefix-length rule
and both `File(parent, child)` joins, which is why one wrong answer produced
forty-six differing rows.

The second row is the one that reaches ordinary code: a filename containing a
backslash — which a Windows-authored name copied onto a Linux box routinely has
— was split into a directory and a basename that do not exist, so
`getParentFile().mkdirs()` created a WRONG DIRECTORY and the file was written
somewhere nobody asked for.

**`File.toURI()` slashified unconditionally.** `slashify` is
`WinNTFileSystem`'s; `UnixFileSystem`'s is the identity, and what happens to a
backslash there is that `ParseUtil.encodePath` escapes it (`\` is not a URI path
character). Rewriting it to `/` does not merely render differently — round-
tripped through `new File(uri)` it names a DIFFERENT FILE, three levels down a
tree that does not exist.

The Windows arm of both is unchanged, and it is what the 23 `#[cfg(windows)]`
tests in `nio_file.rs` pin. `file_uri_reject`'s own separator scan was moved to
a new `u_is_sep_either`, because a URI's syntax genuinely is platform-
independent and the `file:\C:\…` spelling that constructor is documented to
receive must keep working on a Linux host too.

`L4FileSweep` now carries fourteen backslash-bearing inputs and a `toURI` row
for every path shape, so this class of row cannot go missing again from a
Linux-shaped probe: **331 rows → 486**.

### 3.9 `File.toURI()` percent-encoded characters `java.net.URI` leaves alone

```text
new File("unicode/é中文").toURI().toString()
  HotSpot   file:/…/unicode/é中文
  CratonVM  file:/…/unicode/%C3%A9%E4%B8%AD%E6%96%87
```

`File.toURI()` is `new URI("file", null, slashify(path), null)`, and the
multi-argument constructor renders through `URI.quote`, which escapes a
character below U+0080 only when the mask rejects it and **appends everything
above it unchanged**; only `toASCIIString()` encodes the rest. Encoding it here
made `toString()` and `getPath()` disagree with every real JDK, and made a URI
built from a non-ASCII filename compare unequal to the one HotSpot builds from
the same file. The one exception `URI.quote` itself carries is kept: a
non-ASCII space or control character is still escaped.

The probe asks both halves — a space is `%20`, an e-acute is itself, and
`toASCIIString()` encodes both — because a shim that percent-encodes everything
gets the first row right and the second wrong, which is exactly what this one
did.

### 3.10 One defect the DEFAULT mode has and `--jdk-only` does not

```text
new BufferedOutputStream(sink, 0)
  --jdk-only  IllegalArgumentException: Buffer size <= 0     <- correct
  compatible  no-throw                                        <- wrong
```

The constructor is registered as a `SyntheticStub`, so strict mode drops it and
the real bytecode's `if (size <= 0) throw` runs. That is another place where
strict is right and the default is not, and the fix again brings the default
into line with strict rather than the reverse.

---

## 4. Eight shadows retired — and the one residual row

### 4.1 `Files.newBufferedReader`, both overloads

The two bodies read the WHOLE FILE with `p57_read_to_string` and handed back a
reader over the resulting string. The real method is

```java
new BufferedReader(new InputStreamReader(Files.newInputStream(path), cs))
```

which STREAMS. The shim was buying a whole-file read, in memory, at open, in
exchange for a refusal at the wrong moment:

```text
Files.newBufferedReader(<a directory>)
  HotSpot   no-throw at open; IOException on the first read
  shim      IOException at open      (std::fs::read of a directory)
```

Retired. `Files.newInputStream` is registered a few lines above and matches
HotSpot on every input this lane measured, including the directory row, so both
halves are fixed by removing the shim rather than by patching it.
`L4FilesSweep` went to 0 differences on the same run.

### 4.2 the six `java/io/BufferedWriter` registrations

`write(I)`, `write(String,II)`, `write([CII)`, `newLine`, `flush`, `close` — all
gated to `synthetic-jdk`, which is the only build where their second arm can
fire.

Each had exactly two: a delegating arm forwarding to the wrapped `out`, and an
fd-backed arm reached through `bw_synthetic_fd` — and **that helper is itself
`#[cfg(feature = "synthetic-jdk")]` and answers `None` in every shipping
build**, because `Files.newBufferedWriter`'s fd path was deleted on 2026-08-05.
So in `--jdk-only` and `--real-jdk` these six were pure pass-throughs standing
in front of the real class, and they cost the BUFFERING (`new
BufferedWriter(sw, 4).write("ab")` reached the delegate immediately, where
HotSpot holds it) and the class's OWN argument contract, which is not the
delegate's — the `write((String) null, 0, 1)` row in §3.7.

Retired, and verified against the probe that pins the family's close semantics
as well as this lane's own: `TailFamilySweep` and `L4StreamTailSweep` are both
0-diff in both modes with the real bytecode running instead.

### 4.3 THE RESIDUAL — `FileInputStream.skip` past end of file

```text
new FileInputStream(<2-byte file>); read × 3; skip(4)
  HotSpot   4   and the channel position is 6
  CratonVM  0   and the channel position is 2
```

Contract-legal — `InputStream.skip` is specified to "skip over some smaller
number of bytes, possibly zero" — but the CAUSE is worth naming, because a fix
to either native here is INERT and was **measured** to be:

```text
--jdk-only   java/io/FileInputStream.skip (J)J   not registered at all
default      java/io/FileInputStream.skip (J)J   registered, invocations 0
both         java/io/FileInputStream.skip0(J)J   registered, invocations 0
             java/io/FileInputStream.readBytes   invocations 4
```

Four `readBytes` for two skips over a two-byte file is `java.io.InputStream
.skip`'s read-and-discard default, arithmetically. So `FileInputStream.skip`
resolves to its SUPERCLASS's method and the descriptor is never seeked.
**NOMINATION for a dispatch/resolution lane**; no registrar edit can move it. A
seek WAS written here first and reverted when the registry showed it could not
fire — the `owns_slot` discipline applied to the invocation column.

### 4.4 The fabricated abstract provider behind `probeContentType`

The entry point is repaired: `Files.probeContentType` is now registered and
answers from an extension table or null, which is the documented contract ("the
content type, or null if the content type cannot be determined") and is what an
installed JDK answers on a host with no `mime.types`.

The CAUSE is not, and it is not L4's:

```text
Files.probeContentType(p)
  -> sun.nio.fs.DefaultFileTypeDetector.create()
  -> DefaultFileSystemProvider.instance().getFileTypeDetector()
  -> NoSuchMethodError: java.nio.file.spi.FileSystemProvider.getFileTypeDetector()
```

`p57_alloc_provider` mints the default provider as an instance of the ABSTRACT
`java/nio/file/spi/FileSystemProvider` itself. **NOMINATION**: this is the same
shape as the roadmap's Phase-1 `MemorySegment`-as-an-instance's-class row and
`H5-1`'s fabricated abstract receivers — an object whose class is a type the
Java object model says cannot be instantiated, so any real bytecode that calls a
concrete-subclass method on it dies. Before this lane it took a whole probe run
with it.

### 4.5 What a Linux host can and cannot ask

§3.8 is what a Linux host CAN ask that a Windows-shaped intuition does not: the
`\`-as-a-filename-character rows exist only there, and they were the largest
single cause in the lane.

What it cannot ask is the mirror set — a drive-absolute path, a driveless-rooted
`\x`, a UNC path — which the L4 lane doc warns about and which are covered by
the 23 `#[cfg(windows)]` regression tests `nio_file.rs` already carries. Both
repairs in §3.8 leave the Windows arm byte-identical and are `cfg`-selected, so
those tests still pin exactly what they pinned.

---

## 5. Four process notes, each paid for

**A probe must not normalise with a method it is testing.** `L4FileSweep` built
its `<CWD>` token with `getParentFile()`. One defect in that method renamed the
token and reported six unrelated rows as differences — a harness artefact that
looks exactly like six defects.

**`args[1]` is the first PARAMETER, not the first argument.** The
`SimpleFileVisitor` repair read the `IOException` from `args[1]`, which is the
path, and threw a `java/nio/file/Path` as a Throwable. It produced an exception
with no message and no frames, and cost a full build.

**The registry answers "will my edit fire?" for INVOCATIONS too, not only for
`owns_slot`.** The scope doc's §5 says to read `owns_slot` before editing. This
lane needed the neighbouring column twice: once for `Files.getFileStore`, where
the check sat on the copy nothing runs, and once for `FileInputStream.skip`,
where both candidate natives report `invocations: 0` and no edit to either could
have moved the answer.

**Re-run the family's EXISTING probes on the final binary, not only your own.**
The lane's five probes were 0-diff and the work looked finished; running the
four `java.io` probes that were already in the tree found `FilePathSweep` at 94
differing lines and the largest single cause in the lane (§3.8). A new probe
asks the questions its author thought of, and this one was written on a Linux
host by someone who did not think of backslashes.

---

## 6. What PASSED, because it says where the work is not

**`java.nio.ByteBuffer` is right.** 404 rows over three backings, 0 differences
before any fix in this lane. Every invariant (`0 <= mark <= position <= limit <=
capacity`); all six distinct refusal types (`IllegalArgumentException` for an
out-of-range position or limit, `InvalidMarkException` for a reset with no mark,
`BufferUnderflowException` / `BufferOverflowException` for relative access past
the limit, `IndexOutOfBoundsException` for ABSOLUTE access, and
`ReadOnlyBufferException` for every mutator on a read-only view); `mark`
discarded by `clear` / `rewind` / `flip` / a lower `limit`; `slice` /
`duplicate` / `asReadOnlyBuffer` sharing and order inheritance; `compact`;
`array` / `arrayOffset` refused on a direct buffer; the six typed views;
absolute-vs-relative bounds; `wrap` with an offset; and the remaining-elements
definition of `equals` / `hashCode` / `compareTo` / `mismatch`, including a heap
buffer equal to a direct one with the same contents.

**`java.io.File`'s path-string surface is right** — now including the
backslash rows: 38 path shapes × 9 accessors. `getCanonicalPath` vs
`getAbsolutePath` on a non-existent file and the whole `toURI` round trip are
right. `list` / `listFiles` on a FILE answer null rather than an empty array,
with and without each filter overload, and a null filter means "no filtering"
rather than NPE. `mkdir` on an existing directory, `delete` on a non-empty one
and `renameTo` from a missing source all answer FALSE rather than throwing.
Every `createTempFile` validation row was already right.

**`java.nio.file.Path`'s text operations are right** apart from the four rows in
§3.2: `normalize`, `resolve` (including the absolute-child rule),
`resolveSibling`, `relativize` in all four directions plus its two refusals,
`subpath` and `getName` bounds, the empty path's one-name rule, and iteration.

**`Files`' directory surface is right**: `createDirectory` vs
`createDirectories` on an existing directory (they differ, and both were right),
`createFile`, `delete` vs `deleteIfExists` across the three cases each
distinguishes, `newDirectoryStream` with and without a glob, `list`, `walk` at
depths 0 / 1 / unbounded and on a plain file, `find`, and `walkFileTree`'s event
ORDER.

**`Files`' attribute surface is right**: `readAttributes` by type and by name,
`getAttribute` and its two refusals, `FileTime` round-tripping through
`setLastModifiedTime`, the file store, and the default filesystem's own shape
including `getPathMatcher`'s glob and regex syntaxes and its two refusals.

**`DataInputStream` / `DataOutputStream`'s wire format is right**, which is the
part with the most room to be wrong: every width, big-endian byte order,
modified UTF-8 including the two-byte NUL and the six-byte supplementary pair,
and `EOFException` — not `IOException`, not `-1` — from every one of the nine
`readX` methods at end of stream and from a PARTIAL record.

**`ByteArrayInputStream`'s cursor rules are right**, including the two a
from-memory implementation gets wrong in opposite directions: `close()` is a
no-op and the stream stays readable, and `read(b, 0, 0)` at EOF answers 0 rather
than -1.

**`PrintStream`'s conversion and formatting are right**: every numeric rendering
including `NaN`, the infinities and `-0.0`; the `null` substitutions that ARE
correct (`print((String) null)`, `append((CharSequence) null)`); every `append`
subrange refusal; and the whole `java.util.Formatter` surface including all six
`IllegalFormatException` subtypes the bad-format rows ask for.

---

## 7. Reproduce

```bash
# the census
cratonvm --java-home "$JDK" --jdk-only --explain-jdk-only \
    --jdk-only-report rep.json --dump-native-registry reg.json \
    -cp apps/probes/out L4Reach

# the five differential probes, three arms each -- plus the four EXISTING
# java.io probes, which is where §3.8 came from
bash apps/probes/l4run.sh L4FileSweep L4FilesSweep L4ByteBufferSweep \
                     L4PrintStreamSweep L4StreamTailSweep \
                     TailFamilySweep IoSystemSweep FilesSweep FilePathSweep
```

`FilesSweep`'s two differing lines are its own harness artefact — it prints the
random name of the temporary directory it created, which no two runs share.

---

# PART TWO — the completeness question, and what asking it found

**Added 2026-08-29.** Part one closed the 199 `native-won` triples. This part
asks the question part one could not: *those are the rows a program MET — what
about the rest of the family?* Answering it took three more defects out, one of
them a data-corruption bug, and cleared a compile break on `dev`'s tip.

**The filename says 49 defects and the title now says 55.** The slug is left
alone deliberately — the scope doc and the retired lane brief both cite it, and
a rename costs more than it explains.

## P2.1 The number, and the instrument reading that made my first one wrong

Unioned over eleven probe runs, filtering `java.io` + `java.nio` to rows that
are `bridge`, own their slot, and shadow a real method that is declared, has
`Code` and is not `ACC_NATIVE`:

```text
405  the static adjudication surface for this lane
246  reached by at least one probe
159  never reached by any of them
  3  of those 159 are FLOORS, not totals (invocations_complete false)
```

**My first pass said 414 and it was wrong by 2.6×.** The filter read `has_code`
out of `image_declaring_method`, and

> **`--dump-native-registry` leaves `image_declaring_method` NULL unless
> `--explain-jdk-only` is also passed.**

Ten of the eleven dumps were plain `--dump-native-registry`, so every one of
their rows failed the filter, every dump contributed zero, and the "union" was a
single run wearing a union's clothes. The tell was a `reached 0` for a probe
whose raw rows showed `invocations: 6` two commands earlier. `real_declaring_
method` is populated in both shapes and is what a census should filter on.

This matters beyond one lane: the scope doc's §5 sends every lane to that dump,
and a census script that filters on the image block will silently report zero
without erroring.

The three floors are `SimpleFileVisitor`'s callbacks — which part one's
`walkFileTree` repair provably calls, so their zero is a floor in exactly the
way `invocations_complete` says.

## P2.2 `CharBuffer.get(int)` was position-relative — the wrong characters, with every number right

`apps/probes/L4TypedBufferSweep.java` is new: **501 rows**, six typed buffers
(`Int`, `Long`, `Short`, `Float`, `Double`, `Char`) × three backings, covering
~50 census rows no probe in the tree had ever reached. One row differed.

```text
CharBuffer cb = ByteBuffer.allocate(8).asCharBuffer();   // "abcd"
cb.flip(); cb.get();                                      // position is now 1
cb.get(new char[4], 1, 2);
  HotSpot   .bc.
  CratonVM  .cd.
```

It reads as a bulk-copy defect and is not one. **Every visible number was
correct** — position 1 after the single `get()`, position 3 after the bulk read,
`charAt`, `toString` and `length` all right, and the `IntBuffer` and
`ShortBuffer` views correct on the identical sequence. Only the characters were
wrong.

`CharBuffer.get(char[],int,int)` has no native, so the real bytecode ran and
called the absolute `get(int)` once per element with the right indices — onto a
body that added the position to them again. `charAt(int)` and `get(int)` were
sharing one implementation, and they are two different contracts:

```text
CharSequence.charAt(i)   ->  get(position() + checkIndex(i, 1))   RELATIVE
Buffer      .get(index)  ->  Objects.checkIndex(index, limit)     ABSOLUTE
```

At position 0 they agree, which is why every earlier probe missed it and why
the new rows ask both at a NON-ZERO position. It is an `Intrinsic`, so
`--jdk-only` does not drop it and the defect was identical in both modes.

## P2.3 Two latent defects the retirement dial exposed

`CRATONVM_ENFORCE_NATIVE_SHADOW=<prefix>` makes `Bridge` natives on a receiver
under that prefix yield to real bytecode under `--jdk-only`, which is how Phase
2 prices a retirement without building anything. Armed at `java/io/File`:

```text
L4FilesSweep   DIED 119 rows early
  NullPointerException: Cannot enter synchronized block because
                        "this.closeLock" is null
      at java/io/FileOutputStream.close(FileOutputStream.java:383)
      at java/nio/file/Files.copy(Files.java:2865)
L4StreamTailSweep   fos.write(null, 0, 1)   NPE -> no-throw
```

**Read the prefix carefully: `covers()` is `starts_with`**, so `java/io/File`
also armed `FileOutputStream`, `FileInputStream` and `FileDescriptor`. That run
priced the `java/io/File*` FAMILY, not `java.io.File`. Both blockers turned out
to be in the siblings, and both are real:

* **`FileOutputStream.closeLock` is never initialised** on a stream this VM
  mints. `fsp_new_output_stream` allocates the object and wires the fd by hand;
  the constructor never runs, so its instance initialisers never do either.
  `FileOutputStream.close()` opens `synchronized (closeLock)`. This is the
  **twin** of a repair that already existed — `fsp_new_input_stream` sets
  `closeLock`, `path` and `closed` a hundred lines above, *with a comment saying
  exactly why* — applied to one side of a pair and not the other.
* **`writeBytes([BIIZ)V` accepted a null buffer** and wrote nothing. Its
  `write([BII)V` twin got the NPE in part one; `writeBytes` is the door the real
  `FileOutputStream.write(byte[],int,int)` bytecode arrives at, and it kept the
  silent no-op.

Both are invisible today, because the `close()` and `write()` natives above them
never let the real bytecode through. Both would turn a future retirement from
free into a crash, and both are one hunk.

With them fixed the armed run is **fully green**: seven probes, 1671 lines,
armed diffs identical to baseline. So the `java/io/File*` retirement is now
priced and unblocked — **NOMINATION**, not taken here, because the dial's own
doc is explicit that "an armed FAILURE is real; an armed ZERO is unreliable":
the dial covers the doors it reaches, and a real retirement removes the
registration so every door misses.

## P2.4 `dev`'s tip did not compile, and it was two `#[cfg]` lines

> **CREDIT, added at the merge.** Both repairs in P2.4 and P2.7 were made
> INDEPENDENTLY and landed on `dev` first — the `#[cfg]` restoration as
> `43088b840`, and the array-CNFE element name in the same window. When this
> lane merged, both of its copies conflicted with the ones already there and
> were resolved to `dev`'s side and dropped. What is left here is the
> DIAGNOSIS, which stands either way: the control build that proved the reds
> were not this lane's, and the two general lessons — an attribute belongs to
> the item that follows it, and a branch that bypasses a delegation inherits
> every contract the delegation used to satisfy.
>
> Two sessions finding the same two defects within an hour is itself worth
> noting: a red tip is expensive precisely because every lane pays to
> rediscover it.


`cargo check -p cratonvm-native-builtins` was **17 errors** on pristine
`origin/dev` — `cannot find type Value`, `arg_long` inaccessible,
`state::` unresolved, across `craton_gpu.rs`, `xnio_async.rs` and
`compression_native.rs`.

The module is `gpu-offload`-only. Two new functions had been inserted **between
an existing function's doc comment and its body**, so the
`#[cfg(feature = "gpu-offload")]` stayed with the comment, gated the newcomer,
and left its neighbour bare — `builtin_future_status` and
`builtin_array_to_host`. A default build then compiled two bodies full of
feature-only imports.

Restoring the two attributes takes both arms green (default and
`--features gpu-offload`). Not this lane's code; fixed because a red tip blocks
every lane, which is what the scope doc's §5 asks for.

Worth generalising: **an attribute belongs to the item that follows it, and
inserting an item after a doc comment silently steals it.** Nothing warns.


## P2.7 `dev`'s tip was red a second way, and this one was a behaviour regression

The `#[cfg]` repair in P2.4 made `dev` COMPILE. Running the three arms on it
then showed two vectors failing that had passed on this lane's part-one tree:

```text
RExceptions   AssertionError: ...naming the element, not the descriptor,
                              got: [Lcom.cratonvm.absent.NoSuchClass20260812;
RJdkFailure   AssertionError: an array CNFE must name the element,
                              not the descriptor: [Lcom.cratonvm.absent.NoSuchClass20260731;
```

**Proven not this lane's**, by building a CONTROL: `origin/dev` plus the
`#[cfg]` fix and nothing else — because pristine `dev` cannot be built at all —
and running the two vectors on it. Both failed identically there. This lane's
part-two changes (typed buffers, `FileOutputStream`, `writeBytes`) cannot reach
class-name rendering, and the control says so rather than the reasoning.

The cause is in L5's landing (`c6ccccbc8`), whose own commit message names the
change:

> `Class.forName("[I")` resolves; `ClassLoader.loadClass("[I")` throws. This VM
> implemented forName by DELEGATING to loader.loadClass … The real defect was
> the delegation: forName needs no loader for an array descriptor and now
> resolves one directly.

That new direct-resolution branch builds its `ClassNotFoundException` from
`dotted_name` — the whole descriptor. HotSpot never hands an array descriptor to
a loader, so what fails to resolve, and what the exception reports, is the
COMPONENT:

```text
Class.forName("[Lp.X;")    HotSpot  CNFE msg="p.X"
Class.forName("[[Lp.X;")   HotSpot  CNFE msg="p.X"    (every dimension stripped)
```

Fixed by stripping the dimensions and the `L…;` wrapper before building the
message. Both vectors pass.

**Two lessons, and the second is the one that cost this lane a build.**

A new branch that BYPASSES a delegation inherits every contract the delegation
used to satisfy. L5's change was right — the two doors genuinely disagree — and
the message shape came along for the ride, unasked, because it had previously
been produced by the loader it no longer calls.

And: **this lane pushed a merge it had not built.** Part one's gates and three
arms were green on `87fc7eeb7`; `origin/dev` then moved, the merge was taken,
only the docs changed after it, and the result was pushed without a rebuild. The
merge is what carried both of dev's reds in. That is exactly what the scope
doc's landing protocol says to do and this lane did not: **re-run the gate set
on the MERGED tree, not on the tree you tested before the merge.**

## P2.5 What is left, and who it belongs to

Of the 159 never-reached rows, after this part:

| slice | rows | disposition |
| --- | ---: | --- |
| typed buffers + `Buffer` base | ~50 | **covered now** by `L4TypedBufferSweep` |
| `java/io/UnixFileSystem` | 12 | **covered indirectly** — the armed run drives all 12 through `java.io.File`'s real bytecode, 190 invocations, 0-diff |
| `java/nio/Buffer$2`, `java/io/FileDescriptor$1` | 22 | `SharedSecrets` access bridges, reachable only from JDK-internal callers — adjacent, not L4's |
| `java/nio/channels/{Socket,ServerSocket,Datagram}Channel` | ~30 | network-shaped and unclaimed; `AsynchronousFileChannel` is explicitly L6's |
| `java.io` exception classes | 86 | the shared `Throwable` table, one table serving every package |
| the rest | ~40 | `Files`/`Path` charset and `Iterable` overloads, `FilterOutputStream`, `MappedByteBuffer` |

## P2.6 Final state

```text
2117 differential rows across six probes, both modes
2116 identical to HotSpot 25.0.4+7
   1 residual — FileInputStream.skip past EOF (§4.3), contract-legal,
     and a resolution finding no registrar edit can move
```

Plus four probes already in the tree re-run as a control on every binary:
`TailFamilySweep`, `IoSystemSweep`, `FilePathSweep` all 0, `FilesSweep` 0 apart
from the random temp-directory name it prints itself.


---

# PART THREE — the rows nothing had ever reached

Part two ended with a number I did not like: of the 395 static rows in the
completeness census, **142 had never been reached by any probe in the tree**.
A row nothing executes is not a passing row; it is an unasked question. This
part is what asking them found.

The instrument is `apps/probes/L4TailSweep2.java` (187 rows). It is not a
broader sweep of the same shapes — it is aimed specifically at the never-reached
population, and one design choice in it did most of the work:

```java
static void bridges(String k, Buffer b) {
    Buffer r1 = b.position(1);
    ...
```

**The reference is typed `Buffer`, not `ByteBuffer`, on purpose.** `javac` emits
`clear()Ljava/nio/Buffer;` only when the static type of the receiver is
`Buffer`; through a `ByteBuffer` reference it emits
`clear()Ljava/nio/ByteBuffer;` and a completely different registered slot
answers. Every earlier probe in this lane held a `ByteBuffer`, so the covariant
bridge descriptors had never been dispatched to at all — 30-odd registrations
that no test in the repository could reach. That is not a gap in coverage of a
method; it is a gap in coverage of a *descriptor*.

## P3.1 Twelve defects measured, and five more fixed on the argument

**Measured** — each of these appeared as a differing row against HotSpot
25.0.4+7 in a run I can point at:

| # | Method | This VM | HotSpot |
|---|---|---|---|
| 1 | `Buffer.reset()` with no mark, through a `Buffer` reference | `IllegalStateException` whose *message* is the string `"InvalidMarkException"` | `java.nio.InvalidMarkException` |
| 2 | `Files.readString(Path, Charset)` | ignored the charset | decodes with it |
| 3 | `Files.readString(p, US_ASCII)` over a byte > 0x7F | (my own first fix) U+FFFD | `MalformedInputException` |
| 4 | `File.setReadable(false, true)` then `canRead()` | `true` | `false` |
| 5 | `File.setExecutable(true, true)` then `canExecute()` | `false` | `true` |
| 6 | `FilterOutputStream.write((byte[]) null)` | silently nothing | `NullPointerException` |
| 7 | `new FilterOutputStream(null).write(1)` | silently nothing | `NullPointerException` |
| 8 | `ByteArrayOutputStream.toString((String) null)` | a decoded string | `NullPointerException` |
| 9 | `ByteArrayOutputStream.toString((Charset) null)` | a decoded string | `NullPointerException` |
| 10 | `Files.walkFileTree(d1, {}, 1, v)` with `d1/d2` a directory | `preVisitDirectory(d1/d2)` | `visitFile(d1/d2)` |
| 11 | `Files.walkFileTree(p, {}, -1, v)` | walks the whole tree | `IllegalArgumentException` |
| 12 | `Files.walkFileTree(p, null, 1, v)` | walks | `NullPointerException` |
| 13 | `FileSystems.newFileSystem(p, (Map) null)` | returns a filesystem | `NullPointerException` |

**Fixed on the argument** — same body or same family as a measured row, no
separate measurement of its own, and stated here so the distinction is not lost:
`Files.readAllLines(Path, Charset)` (shares one callback with its measured
twin), `File.canWrite`/`canExecute` (the same wrong question as `canRead`),
`File.setWritable` (the same fabricated success as `setReadable`), and
`FilterOutputStream.write(byte[], int, int)`.

## P3.2 The covariant bridge and its target disagreed

Defect 1 is the one the `Buffer`-typed reference existed to ask, and it is worth
more than its row.

`InvalidMarkException extends IllegalStateException`. So the *supertype* handler
matched, nothing failed loudly, and the defect was invisible to any test that
did not name the exact type. What made it findable is that
`ByteBuffer.reset()Ljava/nio/Buffer;` is a **separate registered slot** from the
bytecode a `ByteBuffer`-typed call reaches:

```text
  ByteBuffer bb = ...;  bb.reset()    InvalidMarkException   (real bytecode)
  Buffer     b  = bb;   b.reset()     IllegalStateException  (this VM's bridge)
```

**One method, two answers, decided by the static type at the call site.** A
program that catches `InvalidMarkException` — the only handler anyone writes for
this — worked or did not work depending on how a local variable was declared.

The general lesson, which the `has_code`/retirement work in this campaign keeps
re-learning from a different direction: a native registered for a covariant
bridge descriptor is not "the same method" as the one the ordinary call site
reaches. It has to be checked separately, and a probe that never types a
reference as the base class cannot check it.

## P3.3 The setter was not the liar — reading the state back found the real one

Defect 4 cost a build to attribute, and the way it went wrong is the useful part.

`File.setReadable(false, true)` returned `true` and `canRead()` still said
`true`, so I fixed `setReadable`: it had read its argument into a variable named
`_readable` and then answered `std::fs::metadata(&path).is_ok()` — "does this
file exist?". That was a real fabricated success and the fix was right.

**It did not close the row.** The rebuild still answered `canRead() == true`.
The micro-probe that settled it asked the filesystem directly:

```text
                     oracle          this VM
  mode0              rw-rw-r--       rw-rw-r--
  setReadable ret    true            true
  mode1              -w-rw-r--       -w-rw-r--      <- the chmod was CORRECT
  canRead            false           true           <- the reader is the liar
```

The setter had been fixed and the mode bits agreed with HotSpot byte for byte.
`canRead` was the defect, and it is a different *kind* of defect: not a
fabricated success, but the **wrong question**.

```rust
canRead     std::fs::metadata(path).is_ok()      // "does it exist?"
canWrite    !permissions().readonly()            // any write bit, for anyone
canExecute  mode() & 0o111 != 0                  // any execute bit, for anyone
```

A permission method asks about **this process**, not about the file. The three
now call `fs_check_access` — `access(2)` — which is what
`UnixFileSystem.checkAccess` in the same file has always used. That helper was
correct and simply had no caller here: the fourth instance in this lane of *a
correct helper the winning door does not consult*.

`canExecute` is worth one more line, because it **passed** the probe. With mode
`0o100` set and cleared, "any execute bit" and "the owner's execute bit" give
the same answer — the row was green by luck. `canWrite` and `canExecute` are
fixed on the argument, not on a measurement, and I would rather say so than
claim a green row proved them.

The reusable form: **`setX` and `getX` are two natives, and a probe that calls
one and asserts its return value tests neither.** Reading the state back through
a *second* method is what separated a correct writer from a lying reader. An
identity-only probe would have reported both as fine.

## P3.4 My own part-one fix turned a wrong answer into a wrong refusal

Defect 2 has two halves, and I own the second one.

`Files.readString(Path, Charset)` ignored its charset — the body said so, in a
comment: `// Ignore charset, always UTF-8`. Before this lane that produced a
silently wrong string. **Part one of this lane made the UTF-8 decode strict**
(§3.4, so that malformed input raises `MalformedInputException` instead of
returning U+FFFD), which fixed the no-charset overload and, in the same motion,
changed the charset overloads from a wrong *answer* into a wrong *refusal*:

```text
  Files.readString(<0xC3 0x28>, ISO_8859_1)
    HotSpot    a 2-character String        (latin-1 cannot fail, ever)
    CratonVM   MalformedInputException     <- after part one
```

That is worse in one way and better in another — it is louder — but it is still
wrong, and it was **introduced by a fix in this same lane**. A change that makes
one path strict has to be walked to every caller that shares the path, including
the overloads that should never have been on it.

## P3.5 The oracle refused a row this VM answered

Defect 3 is a fix of mine that was wrong when I wrote it, caught the same day.

Writing the charset decoder, I gave `US-ASCII` the JDK's REPLACE action: bytes
above 0x7F become U+FFFD. Then the probe row `readString(bad, US_ASCII)` **killed
the HotSpot run** — because the JDK's `Files.readString` decodes with a
`CharsetDecoder` left on its default action, which is REPORT. US-ASCII refuses.

I had written the row as a value row (`p(...)`) because I expected an answer.
The oracle threw, the run stopped, and the truncated tail is exactly the shape
the runner's `lines=` counter exists to catch.

```text
  the same two bytes, three single-byte charsets, three different answers
    ISO_8859_1   a 2-character String   (cannot fail)
    UTF_8        MalformedInputException
    US_ASCII     MalformedInputException
```

The row is now a refusal row and the decoder REPORTs. The residual approximation
is stated in the code rather than hidden: for any charset outside those three,
the fallback goes through `new String(byte[], Charset)`, which uses REPLACE where
`readString` would REPORT. Driving a real `CharsetDecoder` would cost four
re-entrant calls per read; this is the cheaper half of that trade and it is right
for every well-formed input.

## P3.6 Three overloads, and the null check I put in the wrong one

Defects 6 and 7 were in the first fix batch. They did not close, and the reason
is embarrassing enough to be worth writing down.

`FilterOutputStream.write` is **three separate registered slots**:

```text
  write([B)V     -> native_output_stream_write_all   (lib.rs:9622)
  write([BII)V   -> an inline closure                (lib.rs:10093)
  write(I)V      -> an inline closure                (lib.rs:10066)
```

My probe rows call `write((byte[]) null)` and `write(1)`. I put the null checks
in `write([BII)V` — the one overload neither row touches — and the fix compiled,
built, and changed nothing. The registry dump is what named it: `inv=2` on
`([B)V` and `inv=3` on `(I)V` while my edited slot showed `inv=4` from unrelated
traffic.

**Read the descriptor the failing row dispatches on, not the method name.** All
three are fixed now; two of them were silent no-ops, which for a write is the
worst available failure — the caller writes, gets no exception, closes the
stream, and believes it holds the bytes.

## P3.7 At the depth limit, a directory is a file

Defect 10 is a contract detail with a sharp edge.

`FileTreeWalker` only opens a directory it is allowed to descend into. An entry
sitting *at* `maxDepth` is handed to `visitFile` with its real attributes
(`attrs.isDirectory()` true) and never sees the
`preVisitDirectory`/`postVisitDirectory` pair. This VM called
`preVisitDirectory` first and checked the depth afterwards — one callback too
late:

```text
  walkFileTree(d1, {}, 1, v)   with d1/d2 a directory
    HotSpot    pre:d1, file:d1/d2
    this VM    pre:d1, pre:d1/d2     (and no post — it did not descend either)
```

The visitor shape this hurts is the common one: a `SimpleFileVisitor` that
overrides only `visitFile` **never saw the leaf at all**. Bounding a walk is
usually done precisely to count or collect the things at the boundary.

The same guard fixes `maxDepth == 0`, where the root itself is the entry at the
limit and the JDK reports it through a single `visitFile`.

Defect 11 is the fourth "correct helper with no caller" in this lane:
`p57_max_depth_refusal` already existed and already raised
`IllegalArgumentException: 'maxDepth' is negative` — `Files.walk` and
`Files.find` call it, `walkFileTree` did not, and a `.max(0)` silently turned
the refusal into an unbounded walk.

## P3.8 Four probes my own record cited were producing zero lines

A process defect, found by the runner and not by me.

Part one's §3.8 came from re-running four probes that already existed in the
tree — `TailFamilySweep`, `IoSystemSweep`, `FilesSweep`, `FilePathSweep`. On
this part's first sweep they reported:

```text
=== TailFamilySweep   rc oracle=1 strict=1 compat=1
    lines  oracle=0  strict=0  compat=0
    DIFF   strict=0  compat=0
```

**`DIFF strict=0` on a run that produced nothing.** Commit `3b2901531` ("major
doc consistency update before the release") deleted 856 files under `probes/`
the night before, including all four. The class files were gone, all three arms
failed identically, and a diff of two empty files is zero.

Two things came out of it:

* The four are restored under `apps/probes/`, where the reorg kept the surviving
  probes, and are now **tracked** (`apps/` is gitignored — they need `git add
  -f`, and a plain `git add -A` reports nothing while the commit looks complete).
* `l4run.sh` already printed `lines=` above the diff for exactly this reason, and
  it is the only reason I noticed. **A diff count is not a result unless a row
  count stands next to it.**

While there, two more instrument repairs:

* `grep -c '^rows '` answered nothing on `L4TypedBufferSweep`, whose output
  contains a NUL byte — `grep` treats the file as binary and prints a *message*
  instead of a count. `grep -ac` restores the number.
* `FilesSweep` prints its own `Files.createTempDirectory` name inside an
  exception message, so three runs meant three names and a permanent 1-row
  "defect" that is not there. The runner now collapses `/tmp/<word><6+ digits>`
  in all three arms identically — narrowly, so a real path difference still
  diffs. The first version of that regex used `[a-z][a-z0-9]{2,}` for the word
  and greedily ate the digits it was meant to strip; it took the same
  measurement twice to notice.

## P3.9 Final state

```text
2304 differential rows across seven L4 probes, both modes
2303 identical to HotSpot 25.0.4+7
   1 residual — FileInputStream.skip past EOF (§4.3), contract-legal, and a
     resolution finding no registrar edit can move
```

and the four pre-existing probes, now genuinely running:

```text
TailFamilySweep  117 lines   0 diff
IoSystemSweep    154 lines   0 diff
FilesSweep        39 lines   0 diff
FilePathSweep    666 lines   0 diff
```

Reproduce:

```bash
CV=/data/vm-l4io OUT=/data/l4out bash apps/probes/l4run.sh \
  L4TailSweep2 L4TypedBufferSweep L4FileSweep L4FilesSweep \
  L4ByteBufferSweep L4PrintStreamSweep L4StreamTailSweep \
  TailFamilySweep IoSystemSweep FilesSweep FilePathSweep
```

## P3.10 What part three did not fix

Stated so the next reader does not have to re-derive it:

* **`FileSystems.newFileSystem(path, env)` over a non-archive file** with a
  *valid* environment should raise `ProviderNotFoundException`; it still hands
  back a jar filesystem whose every later operation fails somewhere far from the
  mistake. Only the null-`env` NPE is fixed. The jar loading paths reach those
  three registrations constantly and this lane has no measurement of the blast
  radius — it is a real defect and a deliberately unclaimed one.
* **`native-io`'s `native_file_can_read`/`can_write`** do not own their slots
  (`owns_slot=false`; `nio_file.rs` overwrites them) and are left alone. They
  already do a real open, so they are not fabrications — but they are a second
  spelling of the same question, and a build where they *did* win would answer
  differently from the one measured here.
* The `US-ASCII`/`ISO-8859-1`/`UTF-8` trio is decoded directly; every other
  charset takes the REPLACE-vs-REPORT approximation described in P3.5.


---

# PART FOUR — the rest of the covariant bridges

Part three found one defect behind a covariant bridge descriptor and drew the
general conclusion: `javac` emits `reset()Ljava/nio/Buffer;` only through a
`Buffer`-typed reference, so those registrations are unreachable from ordinary
code and were never dispatched to by anything in this repository.

**I then fixed the one my probe happened to catch and moved on.** That is a
finding applied to a single row. This part applies it to the population.

## P4.1 Enumerating them, instead of noticing them

The registry dump can name the whole set without guessing, in two shapes:

* **(A) both spellings registered** — group every entry by
  `(class, name, argument-list)` and keep the groups with more than one *return
  type*. That is a covariant pair by construction.
* **(B) only the base-typed spelling registered** — a single entry whose return
  type is a supertype of the declaring class (`Ljava/nio/Buffer;`,
  `Ljava/lang/Object;`). These are the dangerous ones and shape (A) **cannot
  see them**: `ByteBuffer.reset()`, the original defect, is one of these, because
  the `ByteBuffer`-typed spelling is real bytecode and only the bridge is
  registered.

Over the 2173 `java/io` + `java/nio` + `sun/nio` registrations, and after
`L4TailSweep2` had already run:

```text
java/nio/ByteBuffer    flip   ()Ljava/nio/Buffer;   inv=0     (A)
java/nio/ByteBuffer    mark   ()Ljava/nio/Buffer;   inv=0     (A)
java/nio/ByteBuffer    rewind ()Ljava/nio/Buffer;   inv=0     (A)
java/nio/DoubleBuffer  clear  ()Ljava/nio/Buffer;   inv=0     (B)
java/nio/DoubleBuffer  flip   ()Ljava/nio/Buffer;   inv=0     (B)
java/nio/FloatBuffer   clear/flip                   inv=0     (B)
java/nio/IntBuffer     flip                         inv=0     (B)
java/nio/LongBuffer    clear/flip                   inv=0     (B)
java/nio/ShortBuffer   clear/flip                   inv=0     (B)
    ... plus fileKey/getAttribute/value, all `-> Object`, all inv=0
```

`apps/probes/L4BridgeSweep.java` (497 rows) drives every one of them. Its whole
instrument is one parameter declaration:

```java
static void bridges(String k, Buffer b) { ... }
```

called once per buffer class across heap, direct, read-only, view,
`wrap(byte[])`, `slice()` and `wrap(CharSequence)` arms. **Retyping that
parameter to the concrete buffer class silently converts the method into a test
of a different set of registrations** — which is exactly what
`L4TypedBufferSweep`'s 501 rows over the same classes already were.

Result: **20 differing rows, seven defects.**

## P4.2 One missing line, copied five ways

Eleven of the twenty rows are a single omission:

```rust
// servlet.rs, the typed-buffer loop (Short/Int/Long/Float/DoubleBuffer)
r.register(cls, "flip", "()Ljava/nio/Buffer;", |ctx, args| {
    ctx.set_field(this, BB_LIMIT, Value::Int(pos));
    ctx.set_field(this, BB_POS, Value::Int(0));
    //  <- s2_bb_set_mark(ctx, this, -1);   MISSING
```

`flip`, `clear` and `rewind` are specified to **discard the mark**. The
`ByteBuffer` twin forty lines up has the line; `charset_buffers.rs`'s
`CharBuffer` copy has it; this loop — the one serving five classes — does not.

What makes it invisible from inside the class is that `mark()` and `reset()` are
**not registered** for the typed buffers. So the real `Buffer` bytecode sets and
reads the real `mark` field, while these natives move position and limit in side
slots, and nothing reconciles the two:

```text
IntBuffer ib = ...; Buffer b = ib;
b.mark(); b.flip(); b.reset();
  HotSpot   InvalidMarkException
  this VM   no throw — and the position jumps back into the region flip
            had just excluded, leaving a buffer whose reads are off the end
```

A mixed model, where one half of an object's state is ours and the other half is
the JDK's, is only correct while every operation maintains both. This one had
five copies of an invariant and four of them were right.

`CharBuffer` is the same contract at the other end: `limit(int)` ends
`if (mark > newLimit) mark = -1;` and ours did not, so a mark left *above* the
new limit survived and `reset()` set the position past the limit. `position(int)`
carries the identical two lines and is fixed here **on the argument** — the
probe's ordering never leaves a mark above a lowered position, and I would
rather say that than imply a green row proved it.

## P4.3 A refusal in front of a measurement the class already had

`FileStore.getAttribute(String)` was an unconditional throw:

```rust
|_ctx, _args| Err(RuntimeError::UnsupportedOperationException {
    message: "no such attribute".into() }.into())
```

Thirty lines above it, `getTotalSpace()`, `getUsableSpace()` and
`getUnallocatedSpace()` are registered and answer correctly. `getAttribute` is
how `FileStore` is specified to expose *those same three*, and it is the only
way to reach an attribute by name:

```text
fs.getTotalSpace()              a real byte count
fs.getAttribute("totalSpace")   UnsupportedOperationException
```

Asking the typed accessors in the same probe is what identifies this as a defect
in the **door** rather than in the measurement underneath it. A null name is now
an NPE rather than a refusal — telling a caller an attribute is unsupported when
what actually happened is that they passed nothing is a wrong answer to a
question they did not ask.

## P4.4 One file, two identities

`fileKey()` exists for exactly one purpose: deciding whether two paths name the
same file. This VM had **two producers of it that disagreed twice over.**

```text
readAttributes(f, BasicFileAttributes.class).fileKey()
    HotSpot (dev=10301,ino=123946)  sun.nio.fs.UnixFileKey
    this VM (dev=10301,ino=123945)  sun.nio.fs.UnixFileKey
readAttributes(f, PosixFileAttributes.class).fileKey()
    HotSpot (dev=10301,ino=123946)  sun.nio.fs.UnixFileKey
    this VM (dev=66305,ino=123945)  java.lang.String
```

`0x10301 == 66305`. One producer rendered `dev` in hex — which is what
`UnixFileKey.toString` does — and the other in decimal. Three producers of that
string exist in the file; two were decimal.

Fixing the rendering was not enough, and the probe said so: the row uses
`Objects.equals`, and a `String` never equals a `UnixFileKey` however identically
the two print. The basic door reaches real JDK bytecode and mints a real key;
the posix door reaches our native, which minted a string. `fileKey()` now builds
the real `sun.nio.fs.UnixFileKey` (or `WindowsFileKey`) through its real
constructor, falling back to the string only where the class is absent.

**A value that prints correctly and compares unequal is worse than one that does
neither**, because it survives every eyeball check. The `toString` was the part I
could see and the `equals` was the part callers use.

## P4.5 An attribute that changes after you build it

`PosixFilePermissions.asFileAttribute(perms)` stored **the caller's own set**:

```java
Set<PosixFilePermission> perms = PosixFilePermissions.fromString("rw-r-----");
FileAttribute<?> attr = PosixFilePermissions.asFileAttribute(perms);
attr.value() == perms      // HotSpot false, this VM true
```

The JDK closes over `Set.copyOf(perms)`. Aliasing means a later `perms.add(...)`
retroactively changes the mode an already-constructed attribute will request —
and since `Files.createFile(p, attr)` reads the value at *creation* time, the
change lands on a file created afterwards with no visible cause at the site that
made it.

## P4.6 A value row that survives a refusal

A probe-design note, because it cost a measurement.

`p(tag, expr)` evaluates its argument *before* the call. On this probe's first
pass `FileStore.getAttribute("totalSpace")` threw where I expected a value, the
throw was uncaught, and **the last 14 rows — a different family entirely —
never ran**:

```text
lines  oracle=491  strict=477  compat=477      DIFF strict=42
```

The diff counted 42 and the truth was 40-plus-a-crash. The line count beside it
is the only reason that was visible. The probe now has `pt(tag, supplier)` — a
value row that prints `THREW <type>` instead of ending the run — for every row
where the *answer* is the point but a refusal is a possible outcome; `t()` stays
for rows where the refusal **is** the point.

## P4.7 What this part deliberately did not take

The census also named the channel and selector families — `SocketChannel.bind`,
`configureBlocking`, `SelectionKey.attach`/`channel`, and a fleet of
`getOption(SocketOption)` rows, all `-> Object` or `-> SelectableChannel`, all at
zero invocations. They carry the same descriptor shape and very likely the same
class of defect.

They are network-shaped and unclaimed by this lane (§P2.5), so this part does not
touch them. **The census is the deliverable there**: the two queries in P4.1 run
against any `--dump-native-registry` output and will name that population for
whoever owns it, without their having to rediscover the mechanism.

## P4.8 Final state

```text
2801 differential rows across eight L4 probes, both modes
2800 identical to HotSpot 25.0.4+7
   1 residual — FileInputStream.skip past EOF (§4.3), unchanged
```

and the four pre-existing family probes still 0-diff. The bridges are now
demonstrably *reached* rather than merely fixed — the same registry query that
found them, re-run after the probe:

```text
java/nio/ByteBuffer   flip  ()Ljava/nio/Buffer;  inv=5    (was 0)
java/nio/IntBuffer    flip  ()Ljava/nio/Buffer;  inv=2    (was 0)
java/nio/LongBuffer   clear ()Ljava/nio/Buffer;  inv=8    (was 0)
    ... 9 of 10, and the tenth is a receiver-class question, not a miss:
        `fileKey` answers on `sun/nio/fs/UnixFileAttributes` (inv=1),
        which is the registration the fix changed.
```


---

# PART FIVE — the census closed, and what the tail actually is

Part two asked the completeness question and left a number on the table: **159
of 405 adjudication rows had never been reached by any probe.** Parts three and
four each answered a slice of it without re-taking the measurement. This part
re-takes it, closes what is closable, and — more usefully — says what the
remainder *is*, because most of it turned out not to be a coverage gap at all.

## P5.1 The re-take

Same filter as §P2.1 (a `Bridge` that owns its slot and stands in front of a
real method that is declared, has `Code`, and is not itself native), unioned
over **fourteen** probe runs, each with `--explain-jdk-only` — the flag whose
absence made part two's first count wrong by 2.6×:

```text
             part two        now
surface         405          454      (parts three and four added registrations)
reached         246          329
never           159          125
```

The surface grows because fixing a defect often means registering something. The
number that matters is the third row, and moving it 159 → 125 took a fifth
probe, `apps/probes/L4CensusTail.java` (119 rows), aimed only at rows nothing
had executed.

**Eight defects, all measured:**

| # | Method | This VM | HotSpot |
|---|---|---|---|
| 1 | `new StringBufferInputStream("abé中")` | 7 bytes, UTF-8 | 4 bytes, the low byte of each char |
| 2 | `LineNumberInputStream.available()` | the raw count | `(in.available() + 1) / 2` |
| 3 | `LineNumberInputStream.read(b, -1, 1)` | silently nothing | `IndexOutOfBoundsException` |
| 4 | `DataOutputStream.write(null, 0, 1)` / bad range | silently nothing | NPE / `IndexOutOfBoundsException` |
| 5 | `FileVisitResult.valueOf("NOPE")` | **`CONTINUE`** | `IllegalArgumentException` |
| 6 | `FileVisitResult.valueOf(null)` | `CONTINUE` | NPE |
| 7 | `Path.of(URI)` — no scheme / unknown scheme | a Path | IAE / `FileSystemNotFoundException` |
| 8 | `FileTime.from((Instant) null)` | epoch 0 | NPE |

plus the residual part three declined, now measured and fixed — see P5.4.

Two are worth more than a table row. `FileVisitResult.valueOf` answering
**`CONTINUE`** for an unrecognised name is the worst available default: a walk
that asked to `TERMINATE` through a misspelt or externally-supplied name kept
walking. And `StringBufferInputStream` is a *deliberately lossy* class — the low
byte of each char, which is exactly why it is deprecated — so "fixing" it into
UTF-8 changed the byte values and the length together, and a caller that sized a
buffer from `available()` read a different number of different bytes.

## P5.2 I patched the dead copy

The first pass of fixes 1–3 went into `native-builtins/src/deprecated_io_util.rs`,
compiled green, built, and **changed nothing**. The registry said why:

```text
java/io/StringBufferInputStream read ()I  inv=0   own=False  deprecated_io_util.rs:1322
java/io/StringBufferInputStream read ()I  inv=11  own=True   deprecated_util.rs:2285
```

**Two files register the same triples, and `deprecated_util.rs` wins every
one.** Every overlapping row in `deprecated_io_util.rs` is `owns_slot=false,
invocations=0` — a complete shadowed duplicate of both classes. (Not entirely
dead: `LineNumberInputStream.mark(I)V` is registered *only* there, and does own
its slot. A file can be 90% dead and still load-bearing.)

This is the third time in this lane that "which registrar wins" was the answer
and the fourth time overall — `canRead` in part three, `FilterOutputStream`'s
three descriptors in part three, `File.canRead`'s four doors. **The rule that
keeps paying: before editing a native, dump the registry and confirm the row you
are about to change has `owns_slot=true` and non-zero invocations.** Both copies
are corrected here, so a future change to registration order cannot resurrect
the defect, and the live file now says in a banner which one it is.

## P5.3 Most of the remaining 125 is not a coverage gap

This is the part worth carrying out of the lane. Classified:

```text
 34  channels / selectors        network-shaped, unclaimed (§P2.5)
 24  SharedSecrets access bridges  reachable only from JDK-internal callers
 12  java.io exception classes    the shared Throwable table, all packages
 12  java/io/UnixFileSystem       driven indirectly through java.io.File
  7  abstract receivers           java/nio/Buffer, java/io/OutputStream, ...
 36  the rest
```

And "the rest" is mostly **not unexercised either.** Four of them, measured:

```text
java/nio/DoubleBuffer   array        ()[D       inv=0  own=True  single registration
java/nio/HeapCharBuffer toString     (II)...    inv=0  own=True  single registration
java/nio/MappedByteBuffer force      ()...      inv=0  own=True  single registration
java/nio/file/FileStore getBlockSize ()J        inv=0  own=True  single registration
```

`L4CensusTail` calls all four, and all four rows are 0-diff — the methods ran and
answered correctly. There is no competing registration. So the native was never
consulted, and the reason is the one this campaign already has a name for:
**native dispatch keys on the RECEIVER's runtime class** (H11-1). A registration
on `java/nio/DoubleBuffer` cannot be selected for a `HeapDoubleBuffer` receiver;
`MappedByteBuffer.force` cannot be selected for the `DirectByteBuffer` a mapped
buffer actually is; `FileStore.getBlockSize` cannot be selected for
`LinuxFileStore`.

> **PARTLY WITHDRAWN — see §P5.6.** The receiver-class cause above is confirmed
> (`DoubleBuffer.allocate(3).getClass()` is `java.nio.HeapDoubleBuffer` in both
> VMs). What this section leaves implied — that a registration on the concrete
> class takes over — is **false**. There is no such registration for any of the
> 125. §P5.6 measures the whole set instead of four of it.

**`invocations: 0` on a row whose method demonstrably ran is not a coverage
statement — it is a statement that the registration is unreachable.** Reading it
as "needs a probe" is what part two did, and it is why 125 still looks like work
outstanding when much of it is a registrar-placement question instead. I
measured four, not all thirty-six, so this is the dominant explanation rather
than a proven partition — but it changes what the number means, and any future
attempt to "cover" this tail should check reachability before writing a probe.

## P5.4 The residual part three declined, measured

§P3.10 recorded `FileSystems.newFileSystem` over a non-archive as an unclaimed
residual, deferred for blast radius: those three registrations are on the hot
path for every jar this VM opens. **It was measurable the whole time** — the
oracle answers in one row:

```text
FileSystems.newFileSystem(<a text file>, (ClassLoader) null)
  HotSpot   ProviderNotFoundException
  this VM   a jar filesystem over a text file
```

The blast-radius concern was real and survives; what was wrong was treating it
as a reason not to *ask*. The fix is scoped so it cannot touch the hot path: a
two-byte magic test, refusing only a **readable regular file whose first bytes
are not `PK`**. A real jar takes exactly the path it took before, and so does
anything unreadable, absent, a directory, or one of this VM's own `jar:`
sentinels.

Returning a filesystem for a non-archive is worse than it sounds: the failure
does not appear at the mount, it appears at the first entry lookup, in a caller
with no idea the mount was bogus.

*(Recorded because it is the fourth deferral reason of mine to die on contact,
and the pattern is always the same: the reason was about the FIX, and I let it
stop the MEASUREMENT.)*

## P5.5 Final state

```text
2920 differential rows across nine L4 probes, both modes
2919 identical to HotSpot 25.0.4+7
   1 residual — FileInputStream.skip past EOF (§4.3), unchanged
```

plus the four pre-existing family probes, 0-diff.

```bash
CV=/data/vm-l4io OUT=/data/l4out bash apps/probes/l4run.sh \
  L4CensusTail L4BridgeSweep L4TailSweep2 L4TypedBufferSweep L4FileSweep \
  L4FilesSweep L4ByteBufferSweep L4PrintStreamSweep L4StreamTailSweep \
  TailFamilySweep IoSystemSweep FilesSweep FilePathSweep
```

The census itself is reproducible and is the thing to re-run rather than
re-derive — `--explain-jdk-only` plus `--dump-native-registry` per probe, unioned
under the §P2.1 filter. Its two queries for covariant bridges are in §P4.1.


## P5.6 The whole tail measured — and the claim in P5.3 corrected

§P5.3 measured four rows and generalised from them. The generalisation was half
right, and the wrong half is the kind that quietly misleads a later reader, so
here is the whole set.

For each of the 125 never-reached rows, ask whether the same
`(name, descriptor)` is served by **any** registration on **any** class, in the
union of all fourteen dumps:

```text
125  never-reached rows
  0  SERVED ELSEWHERE — another class's registration runs this triple
125  INERT — no registration anywhere serves it
```

**Zero.** Not one of them is picked up by a concrete-class twin. §P5.3 said the
native "was never consulted" — true — and implied that something else registered
took over. Nothing did. What answers these calls is the **real JDK bytecode**,
which is what `--jdk-only` exists to run.

Confirmed rather than inferred, three ways:

```text
DoubleBuffer.allocate(3).getClass()   java.nio.HeapDoubleBuffer   (both VMs)
Files.getFileStore(".").getClass()    sun.nio.fs.LinuxFileStore   (both VMs)
violations naming java/nio/DoubleBuffer in the jdk-only report:  0
```

A zero-length violation list is the direct evidence: the report records a row
when a native stands in front of bytecode, and for these it records nothing.

### What that makes them

**A registration nothing dispatches to is a change that is not happening.** For
each row the question is which of two things it is:

* the bytecode is right anyway → the registration is dead weight, and a
  *retirement candidate*;
* the registration encoded a fix → that fix is inert, and whatever it was meant
  to correct is still wrong.

For the subset this lane's probes actually exercise, the answer is the first,
and it is measured: **0-diff against HotSpot on the row, and `invocations: 0`
across all fourteen runs including a probe written to reach it.**

| Registration | Exercised by | Rows |
|---|---|---|
| `{Short,Int,Long,Float,Double}Buffer.array()`, `.get([XII)` | `L4CensusTail` | 9 |
| `ByteBuffer.get([BII)`, `.toString()` | `L4CensusTail` | 2 |
| `{Heap,HeapR,String}CharBuffer.toString(II)` | `L4CensusTail` | 3 |
| `ByteBufferAsCharBuffer{B,L,RB,RL}.toString(II)` | `L4CensusTail` | 4 |
| `MappedByteBuffer.{force,load,isLoaded}` | `L4CensusTail` | 3 |
| `FileStore.getBlockSize()` | `L4CensusTail` | 1 |
| `SimpleFileVisitor` erased bridges | `L4CensusTail` | 3 |
| `FileSystemProvider.newFileSystem(Path,Map)` | `L4CensusTail` | 1 |

**26 registrations nominated for retirement, and NOT retired here.** Three
reasons, all of which have burned this campaign before:

1. **Fourteen probes are not the corpus.** `invocations: 0` here bounds what
   *these* workloads reach. Spring, Tomcat and H2 reach `java.nio` constantly and
   were not run *at the time this was written*. A shadow unreached by a probe
   suite is not a shadow unreached. **(They have since been run — §P5.8. All
   four green, all rows still zero.)**
2. **A 0-diff argues KEEP as often as RETIRE** (the `StrictMath` adjudication,
   69 rows). Agreeing with HotSpot is what a *correct* shadow also does.
3. **The enforcement dial is the wrong instrument here, and saying why is the
   point.** `CRATONVM_ENFORCE_NATIVE_SHADOW=<prefix>` prices a retirement by
   making a Bridge native yield to real bytecode and measuring what changes. A
   native that **never fires** yields nothing: arming these 26 is a no-op, every
   probe stays green, and the green means only that a dial was set. That is the
   vacuous-green shape this campaign already names — the dial's own rule is
   *prove it FIRED before reading the green*, and for an `invocations: 0` row it
   cannot fire by construction.

   What they need first is a workload that **reaches** them. Run the corpus arms
   with `--dump-native-registry` and read the same 26 rows:

   * still `0` → nothing this VM runs dispatches to them, and they can be
     retired on that evidence;
   * now `> 0` → they are live after all, this lane's probe suite simply never
     went there, and *then* the dial is the right instrument to price them.

The list above is the input to that measurement, not a substitute for it. It is
also the reason to run it: a shadow that fourteen targeted probes cannot reach
is either dead weight or a blind spot, and the two look identical from here.

### The measurement, taken

Ten nio-facing regression vectors, each run under `--jdk-only` with its own
`--dump-native-registry`, unioned and read against the nominated rows
(`ONLY=<vector>` is the suite's filter — `VECTORS=` is silently ignored, which
is worth knowing before trusting a "filtered" run):

```text
RJdkNio  RJdkForeign  RJdkAsyncChannel  RJdkWatchService  RFileChannelFastIo
RSegmentBulkCopy  RJdkProcess  RJdkFailure  RJdkHandles  RJdkCollections
        all ten green

41 registration rows across the nominated class+method pairs
 0 moved off zero
41 still zero
```

**Not one of them fired.** The evidence for the nomination is now fourteen
targeted probes *and* ten real regression vectors, all zero, on rows whose
methods this lane's probes demonstrably call and get right.

That is a much stronger case than §P5.6 opened with, and it is still **not a
retirement**, for the reason that had not changed at this point: the regression
suite is not the corpus either. Spring, Tomcat and H2 are where `java.nio` gets
used in anger.

**§P5.8 runs them.** Four real applications, all green on this lane's binary,
all 29 rows still zero, against a control of 114 262 `java.io`/`java.nio` native
invocations. The reason the list is still not applied is no longer a gap in the
evidence — it is §P5.7's layering caveat.

The pattern is worth stating once: **`invocations: 0` from one workload is a
floor, and the way to raise confidence is more DIFFERENT workloads, not more
runs of the same one.** Fourteen probes written by the same author to reach the
same rows are close to one measurement; ten regression vectors written by other
lanes for other reasons are a genuinely independent second.

### The rest of the 125

The other 99 are inert for reasons this lane already classified and does not
own: 34 channels/selectors, 24 `SharedSecrets` access bridges reachable only
from JDK-internal callers, 12 `java.io` exception classes served by the shared
`Throwable` table, 12 `UnixFileSystem` rows driven indirectly through
`java.io.File`'s bytecode, and 7 on abstract receivers (`java/nio/Buffer`,
`java/io/OutputStream`) that no live object's class can ever match.

**The number to carry forward is not 125.** It is: 26 measured-inert and
nominated, 99 inert for classified reasons, and — after five parts — *zero*
rows in this lane's families that a probe reaches and gets wrong.


## P5.7 Two statements in this record that look contradictory, and are not

§P2.5 says `java/io/UnixFileSystem`'s 12 rows are **covered indirectly — the
armed run drives all 12 through `java.io.File`'s real bytecode, 190
invocations, 0-diff**. §P5.6 says those same rows are **inert, `invocations: 0`,
nothing serves them**. A reader hitting both is entitled to think one is wrong.

Neither is. Measured, same probe (`L4FileSweep`), same binary, one variable:

```text
                          unarmed                armed (java/io/File yields)
java/io/File          827 inv / 60 rows          28 inv / 10 rows
java/io/UnixFileSystem  1 inv /  1 row          190 inv / 16 rows
```

**The work moves one layer down.** Unarmed, `java.io.File`'s natives answer
directly and `UnixFileSystem` is never reached — §P5.6's reading. Armed,
`File`'s Bridge natives yield to real `java.io.File` bytecode, which calls
`fs.<op>(...)`, and the `UnixFileSystem` natives light up — §P2.5's reading, and
the 190 reproduces exactly.

### Why this matters for the 26 nominations

It is not just bookkeeping. It says **"inert" is a property of the CURRENT
registration set, not of the row.**

Every one of the 26 is inert because something above it answers first — real
bytecode, in their case. Retire a native one layer up and rows below it can
start firing, exactly as `UnixFileSystem` did. So a retirement worklist cannot
be applied top-down without re-measuring after each step: the rows you cleared
as "never invoked" are measured against a VM that still had the layer above
them.

That is the same trap as §P2.3's *half-applied retirement is worse than either
endpoint*, seen from the other side. The order to work in is bottom-up, or
top-down with a re-census between steps — and this record's numbers, like any
census, describe the binary they were taken on.

*(`java/io/File` keeps 28 invocations across 10 rows even when armed: the dial
moves `Bridge` natives, and what remains is the rows it does not cover. An
armed run is not an empty one, and a retirement priced from it inherits that
gap.)*


## P5.8 The corpus arm, run

§P5.6 said the nominations needed "a corpus arm (Spring/Tomcat/H2), not another
probe", and left it there. It is runnable on this host, so here it is.

`/data/dod-out/cmd-<workload>-strict.txt` holds complete, already-validated
`--jdk-only` command lines for the DoD workloads. Reused verbatim except for
three substitutions — the binary swapped to this lane's frozen `/data/vm-l4io`,
the report path moved so another lane's files are untouched, and
`--dump-native-registry` added. Classpath, workload class and `--Xmx 2g` (whose
size must be a separate argument or no report is written) left exactly as the
lane that validated them wrote them.

**All four ran green on this lane's binary:**

```text
h2jdbc     DOD TOTAL ok=12 failed=0/12      H2's own JDBC test suite, 12 classes
sbsimple   DOD CONTEXT-UP beans=55          Spring Boot application context
tcssl      Graceful shutdown complete       Tomcat over SSL
jdbc       DOD RESULT OK checks=92          JDBC end-to-end
```

That is worth stating on its own: **the 82 defects this lane fixed did not break
four real applications.** Nothing in parts one to five had established that —
the evidence until now was probes and 116 regression vectors.

### The control, first

A zero is worth nothing if the workload never went near the family. It did:

```text
h2jdbc    90621 java.io/java.nio native invocations across 124 distinct rows
tcssl     12625                                            120
jdbc       6795                                             92
sbsimple   4221                                             79
```

**114 262 invocations**, against a probe suite whose whole `java.nio` traffic is
a few thousand. The instrument fires.

### The answer

```text
29  rows nominated (by EXACT descriptor)
29  present in the corpus dumps
 0  moved off zero
```

So the evidence for the nomination is now **fourteen targeted probes, ten
regression vectors, and four real applications** — H2's JDBC suite, Spring Boot,
Tomcat over SSL, and a JDBC workload — all green, all zero, with a control
showing six figures of traffic through the same two packages.

One neighbour did move, and it is the useful part of the result:

```text
java/nio/ByteBuffer  get([B)Ljava/nio/ByteBuffer;   inv=527   in h2jdbc
```

The nominated row is `get([BII)`. The **one-argument** bulk get is heavily live
in H2 and the **three-argument** one is not reached at all. Liveness is a
property of the DESCRIPTOR, not the method — the same thing this lane found in
`FilterOutputStream`'s three `write` slots, in the covariant `reset()` bridges,
and in `getNameMax0`'s two widths. A retirement list keyed on method names would
have taken out a slot doing 527 calls in the first workload tried.

### Still a nomination

The list is not applied here, and the reason is now §P5.7's rather than a lack
of evidence: **"inert" is a property of the current registration set.** These 29
are unreached while every layer above them is in place; retiring one of those
layers can make them fire, exactly as `java/io/UnixFileSystem` went 1 → 190 when
`java.io.File` yielded. Whoever applies this list should work bottom-up, or
re-census between steps.

What has changed is that the next person does not need to re-derive it. The
worklist, the reproduction, and a corpus-backed zero are all here.

```bash
# the corpus arm, reproducible (both scripts are tracked)
bash    apps/probes/l4corp.sh       # rewrites the DoD command lines, dumps the registry
python3 apps/probes/l4corptally.py  # the control, then the nominated rows
```


---

# PART SIX — the dial was measuring nothing, and what it says now that it works

Parts two to five all leaned on `CRATONVM_ENFORCE_NATIVE_SHADOW` — the dial that
makes a Bridge native yield to real JDK bytecode, and the instrument this
campaign prices every retirement with. **None of those armed measurements were
real.** This part is what happened when the lane's thirteen probes were finally
run against HotSpot with it armed.

## P6.1 Twelve of thirteen probes printed nothing

```text
L4CensusTail   rc oracle=0 armed=0   lines 122/0   DIFF 122
```

Zero lines and a clean exit — and the VM said `main-vm run() returned Ok — VM
main exiting normally`. A probe that emits nothing is not a probe that passed,
but a diff of an empty file against an empty file is zero, and that is what an
armed run had been reporting.

`main` DID run: a workload that writes a side file wrote it. Narrowed to
`java/io/PrintStream`, three writes settled the mechanism:

```text
System.out.println("A")                                          nothing, checkError()==true
new PrintStream(new FileOutputStream(FileDescriptor.out), true)  works
new FileOutputStream(FileDescriptor.out).write(...)              works
```

Not stdout, not `PrintStream` — *this* `System.out`. Reflection on the live
object (`--add-opens java.base/java.io=ALL-UNNAMED`) against HotSpot:

```text
              HotSpot                  this VM
out           BufferedOutputStream     NULL
charOut       OutputStreamWriter       NULL
textOut       BufferedWriter           NULL
charset       sun.nio.cs.UTF_8         sun.nio.cs.UTF_8
```

The synthetic `PrintStream` minted for the system streams carried only
`charset` and `autoFlush`. Nothing notices while this VM's own natives answer —
they write to the host stream and never read those fields. The moment real
bytecode runs, `PrintStream.writeln` calls `ensureOpen()`, finds `out == null`,
throws `IOException`, and catches it into `trouble = true`. **Every write
discarded, exit code 0.**

`install_real_stream_fields` now builds all three from the real constructors.
The three publish together: a half-wired stream would turn a silent discard into
an NPE inside `writeln`, worse than either endpoint.

**The generalisation is the point.** The dial's own rule is *prove it FIRED
before reading the green*. That rule assumes the VM can still report. A family
whose output vanishes when armed produces a green made of no rows at all, and
§P2.3's "armed run fully green across seven probes / 1671 lines" was measured on
a VM in exactly that state.

## P6.2 Two defects that only a working dial could show

**`FileSystemProvider.checkAccess` threw a bare `IOException`** whose message
read `"NoSuchFileException: <path>"`. `Files.createDirectories` walks up for a
live parent with `catch (NoSuchFileException x) { }`; a supertype instance is not
caught, so it escaped the walk and the call failed naming the *parent*:

```text
Files.createDirectories("lvl1/lvl2")
  HotSpot   creates both
  this VM   IOException: NoSuchFileException: <cwd>/lvl1
```

Third refusal in this lane with the right message and the wrong class, after
`Buffer.reset` and `Files.copy`. `p57_no_such_file` had existed all along. Two
sibling sites — the jar and jrt arms of `readAttributes` — had the same shape;
their comment said "FileTreeWalker catches it", which is true and is the trap:
the walker catches `IOException`, so it worked for the walker and nobody else.

**`FileSystemProvider.newByteChannel` ignored `CREATE_NEW`.**
`fsp_new_output_stream` checks it; the real `newOutputStream` bytecode does not
call that native, it calls `newByteChannel` — so the guarantee held only while
our own `newOutputStream` answered:

```text
Files.createFile(<existing>)                   no throw, owed FileAlreadyExists
Files.newOutputStream(<existing>, CREATE_NEW)  no throw, owed FileAlreadyExists
Files.copy(in, <existing>)                     no throw, owed FileAlreadyExists
```

Three rows, two dial scopes, one missing check. `CREATE_NEW` is how a caller
says *I must be the one who creates this*; an exclusive create that quietly
opens the existing file is a lost update, not a wrong exception.

## P6.3 Which retirements are safe today

With the dial working, the question it exists to answer can finally be asked per
FAMILY — the earlier sweep armed all of `java/io/` and `java/nio/` at once,
which is not a retirement anyone would perform. Thirteen probes, one family
armed at a time, oracle captured once (`apps/probes/l4famsweep.sh`):

```text
SCOPE                                   DIFF   TRUNCATED
java/io/PrintStream                        2           0
java/io/File                               2           0
java/io/FileInputStream                    2           0
java/io/FileOutputStream                   2           0
java/io/ByteArrayInputStream               2           0
java/io/ByteArrayOutputStream              2           0
java/io/DataInputStream                    2           0
java/io/DataOutputStream                   2           0
java/io/BufferedReader                     2           0
java/io/BufferedWriter                     2           0
java/io/FilterOutputStream                 2           0
java/nio/CharBuffer                        2           0
java/nio/file/spi/FileSystemProvider       2           0
java/nio/file/attribute/                   2           0
java/nio/channels/FileChannel              2           0
java/nio/ByteBuffer                        8           0
java/nio/file/Files                       14           0
java/nio/file/Path                       102           0
```

`DIFF 2` is the floor — the known `FileInputStream.skip` residual (§4.3) — so
**fifteen of eighteen families arm at zero cost across 2920 probe rows**,
including every `java.io` family. That is a retirement worklist with a
measurement behind it, which is what §P5.6 could only nominate.

`FileSystemProvider` reached the floor *because of* the `CREATE_NEW` fix above:
it scored 6 before and 2 after, in the same run.

## P6.4 The three that are not free, and one I got wrong

* **`java/nio/file/Path` (102)** — 51 rows, one cause: armed, `Path.toString()`
  answers `sun.nio.fs.UnixPath@0` and `equals`-self is false. The same
  unpopulated-real-fields shape as `System.out`, in a class this VM mints on
  every path operation. It needs its own change, not a rider.
* **`java/nio/file/Files` (14)** — `readAllBytes(<dir>)` raises
  `OutOfMemoryError` where HotSpot raises `IOException`; `readAttributes(p,
  null)` does not NPE; `probeContentType` hits `NoSuchMethodError:
  FileSystemProvider.getFileTypeDetector()`; `getOwner` answers
  `UnsupportedOperationException`.
* **`java/nio/ByteBuffer` (8)** — `wrap(b, -1, 2)` and friends raise
  `ArrayIndexOutOfBoundsException` where the JDK raises plain
  `IndexOutOfBoundsException`.

**And a process note I would rather record than hide.** The first attempt at the
`readAllBytes` OOME refused a directory at open. That is wrong twice: Linux
*allows* opening a directory for READ (the failure is `EISDIR` at the first
read, which is why HotSpot's answer is a plain `IOException`), and the WRITE
case was already correct — HotSpot raises `FileSystemException` and so did this
VM, until a blanket refusal downgraded it to a bare `IOException`. It turned a
green row red, and it is the same right-behaviour-wrong-type defect the same
commit fixes three of. Reverted; the OOME is left OPEN with its cause named
rather than papered over from the wrong layer.
