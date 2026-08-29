# L4 — the `java.io` / `java.nio` worklist: 199 native-won triples, 52 defects, 8 shadows retired, and a bounds check that killed the VM

**Status: MEASURED AND FIXED, 2026-08-28.** Lane L4 of
`HANDOFF-20260828-SCOPE.md`. Worktree `/data/cvm-l4io-20260828`, branch
`claude/l4-io-nio-20260828`.

**Provenance.** Linux (Azure host `vm1`), oracle **Temurin 25.0.4+7**
(`/data/toolchain/jdk-25`). Every row below is one program run on three VMs —
HotSpot, `cratonvm --jdk-only`, and `cratonvm` in the default mode — diffed on
**stdout only**. Probes, all new in this lane:

| probe | rows | covers |
| --- | ---: | --- |
| `probes/L4FileSweep.java` | 486 | the 50 `java/io/File` triples |
| `probes/L4FilesSweep.java` | 395 | `java/nio/file/Files` 31, `Path` 9, `Paths` 1 |
| `probes/L4ByteBufferSweep.java` | 404 | `ByteBuffer` 11, `DirectByteBuffer` 9, `HeapByteBuffer` 4 |
| `probes/L4PrintStreamSweep.java` | 123 | `PrintStream` 25, `PrintWriter` 1 |
| `probes/L4StreamTailSweep.java` | 208 | the `java.io` stream tail and the `java.nio` tail |
| `probes/L4Reach.java` | — | NOT a differential probe; it exists only to make the census |

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
    -cp probes/out L4Reach

# the five differential probes, three arms each -- plus the four EXISTING
# java.io probes, which is where §3.8 came from
bash probes/l4run.sh L4FileSweep L4FilesSweep L4ByteBufferSweep \
                     L4PrintStreamSweep L4StreamTailSweep \
                     TailFamilySweep IoSystemSweep FilesSweep FilePathSweep
```

`FilesSweep`'s two differing lines are its own harness artefact — it prints the
random name of the temporary directory it created, which no two runs share.
