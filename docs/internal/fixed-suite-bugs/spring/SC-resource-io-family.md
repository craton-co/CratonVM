# SC-resource-io-family — Spring `spring-core` Resource / IO cluster

> **TRIAGE 2026-06-22 (against current `dev`, repro `test_classes/ResourceIoRepro`).**
> The doc was static-analysis-only; verified status:
> - **Cause A (write/read byte-channel `AbstractMethodError`)**: ✅ **FIXED on `dev`**
>   (`fd14c4ea` + `bbfaa35e`, merge into dev). `FileSystemProvider.newByteChannel` now
>   delegates real-file opens to the sibling `newFileChannel` shim → returns a working
>   fd_table-backed `FileChannel` (implements `SeekableByteChannel`); jar entries keep the
>   in-memory path; the missing-file `NoSuchFileException` contract is preserved. Verified vs
>   HotSpot (JDK 25, `ResourceIoRepro` 11/11: write/read/size/seek/truncate + missing→NSFE + CREATE).
> - **Cause B (`Path.toUri()` authority `file://`)**: ⚪ **NOT A BUG** — HotSpot also emits
>   `file:///C:/…` for `Path.toUri()` on Windows (and `file:/C:/…` for `File.toURI()`); CratonVM
>   matches BOTH. The doc's "HotSpot single-slash for `Path.toUri`" premise is wrong for Windows;
>   "fixing" it would *introduce* a divergence. (The original Spring assertion likely normalizes
>   differently — re-check against the actual `ResourceTests` if it still fails.)
> - **Cause C (`newOutputStream` on a directory)**: ✅ **FIXED on `dev`** (`9e14cc2d`).
>   `fsp_new_output_stream` now maps `open_write` errors by `ErrorKind` to the TYPED nio
>   exception HotSpot throws: PermissionDenied → `AccessDeniedException` (directory open on
>   Windows, os error 5; new `p57_access_denied`), NotFound → `NoSuchFileException`, else the
>   generic `IOException`. (NB the doc's "expects `FileNotFoundException`" premise was wrong —
>   HotSpot throws `AccessDeniedException` here.) Verified vs HotSpot (JDK 25,
>   `test_classes/NewOutputStreamRepro` 3/3).
> - **Causes D (harness CWD) / E,H (URL parse) / F (HTTP openStream) / G (ModuleResource)**:
>   unchanged — env (D) / separate-subsystem handoffs.

## Title
CratonVM divergences in the `java.nio.file` Resource / IO family: a write-channel stub
with no `write` method, `Path.toUri()` emitting an authority component (`file://…`),
a `newOutputStream`-on-directory exception-type mismatch, plus a *harness* working-directory
artifact that masquerades as many file-not-found failures.

## Symptom
25 failing tests across 5 classes in `spring-core`. The headline exception signatures are:
- `java.lang.AbstractMethodError: method java/nio/channels/WritableByteChannel.write(Ljava/nio/ByteBuffer;)I has no Code attribute`
- `java.io.FileNotFoundException: src\test\resources\org\springframework\core\io\example.properties (no such file or directory)`
  and `… src/test/resources/org/springframework/core/io/example.properties` (note: same logical
  path rendered with *different* separators in different code paths)
- `java.io.IOException: URL.openStream failed: <Windows winsock error>` (os error 10049 / 11003)
- bare `AssertionFailedError` / `AssertionError` (empty message) on `exists()` / `isReadable()` /
  `getFile()` / URI-normalization / module-resource assertions.

## Affected tests
PathResourceTests (10 fail / 38):
- `dirExists()`, `fileExists()`, `fileIsReadable()`, `fileIsWritable()`, `getFile()`,
  `getInputStream()`, `getReadableByteChannel()`, `getReadableByteChannelForDir()`
  → **Cause D (harness CWD)**, several compounded by Cause C/A.
- `getWritableChannel(Path)` → **Cause A** (write-channel stub).
- `getOutputStreamForDirectory()` → **Cause C** (wrong exception for dir-open).

ResourceTests (9 fail):
- `urlAndUriAreNormalizedWhenCreatedFromFile()`, `urlAndUriAreNormalizedWhenCreatedFromPath()`
  → **Cause B** (`file://` authority in `Path.toUri()` / `File.toURI()`).
- `filenameIsExtractedFromFilePath()` → **Cause E** (UrlResource filename extraction for
  `file:` URLs with `?query` and `\`-separators; URL parse divergence — lower confidence).
- `remoteResourceExists()`, `remoteResourceExistsFallback()`, `canCustomizeHttpUrlConnectionForExists()`,
  `canCustomizeHttpUrlConnectionForExistsFallback()`, `canCustomizeHttpUrlConnectionForRead()`,
  `useUserInfoToSetBasicAuth()` → **Cause F** (MockWebServer / HTTP `URL.openStream`;
  see winsock `os error 10049`/`11003` — networking, not file IO).

ModuleResourceTests (3 fail): `existingClassFileResource()`, `nonExistingResource()`,
`equalsAndHashCode()` → **Cause G** (ModuleResource over a JDK module — content/length/exists).

ResourceUtilsTests (2 fail): `extractJarFileURL()`, `extractArchiveURL()` → **Cause E/H**
(URL construction `new URL(null,"jar:myjar.jar!/mypath",handler)` → expected `file:/myjar.jar`;
URL parse/equals divergence for the no-scheme-handler form). `isJarURL()` passes.

ResourceEncoderTests (1 fail): `encode()` → **out-of-family**: operates on an in-memory
`ByteArrayResource` through Reactor `Flux`/`StepVerifier`/`DataBuffer`; no filesystem involved.
Belongs to the reactive-streams / DataBuffer subsystem, filed here only by package proximity.

## Root cause(s)

### Cause A — `newByteChannel` returns a stub whose class is the *interface*, with no `write` (HIGH)
`FileSystemProvider.newByteChannel` is shimmed to return a synthetic object **allocated as the
interface type** `java/nio/channels/SeekableByteChannel`, carrying only `[data, position, size]`
fields and **no method implementations**:
- `native-builtins/src/phases_late.rs:5833-5870` (registration), specifically
  `let channel = alloc_concurrent_synthetic(ctx, "java/nio/channels/SeekableByteChannel", 3);`
  at **phases_late.rs:5847**.

No `read`/`write`/`close`/`position`/`size` natives are registered against
`java/nio/channels/SeekableByteChannel` anywhere in the tree (verified: the only references to
`SeekableByteChannel`/`Readable`/`WritableByteChannel` in native-builtins are this allocation and
`FileChannel.transferTo/transferFrom` at phases_late.rs:10942 / 11032). Spring
`PathResource.writableChannel()` returns this stub; the test then calls `channel.write(buffer)`
through the `WritableByteChannel` interface. Because the stub's runtime class *is* the abstract
interface and `write` is never overridden by real bytecode or a native, dispatch lands on the
abstract method → `AbstractMethodError: WritableByteChannel.write … has no Code attribute`.
(The same gap means `getReadableByteChannel()`'s `channel.read(buffer)` would also `AbstractMethodError`
were it not short-circuited earlier by Cause D's FileNotFoundException.)

### Cause B — `Path.toUri()` emits an authority component `file://…` (HIGH)
HotSpot's `WindowsUriSupport` renders an absolute path as `file:/C:/…` (single slash, **no**
authority). CratonVM's `Path.toUri` natives build `format!("file://{abs}")`:
- `native-builtins/src/phases_late.rs:6839` (the live registration; last-registration-wins) and the
  earlier duplicate at **phases_late.rs:4229**.
With `abs = /C:/…/test1.txt` this yields `file:///C:/…/test1.txt` (triple slash). The test asserts
`getURL()`/`getURI()` match `^file:\/[^\/].+test1\.txt$` (single slash, next char non-slash) — the
extra `//` authority fails the regex for both the File- and Path-constructed `FileSystemResource`.
Note that `File.toURI()` was *already* fixed to emit single-slash form (see the comment at
phases_late.rs:10553-10556), but the `Path.toUri()` registrations were not brought in line — a
straightforward inconsistency to close.

### Cause C — `newOutputStream` on a directory throws the wrong exception (MEDIUM)
`getOutputStreamForDirectory()` expects `FileNotFoundException` when opening a *directory* for
output. `fsp_new_output_stream` calls `fd_table().open_write(&p, append)` and on error wraps it as a
generic `RuntimeError::IOException { "newOutputStream(<p>): <e>" }`:
- `native-builtins/src/phases_late.rs:7694-7729` (esp. the `open_write` + `map_err` at 7711-7716).
There is no directory pre-check and the error type is `IOException`, not the
`FileNotFoundException` Spring's `assertThatExceptionOfType(FileNotFoundException.class)` requires →
`AssertionError`. (This test is *also* downstream of Cause D, since `TEST_DIR` is the relative path.)

### Cause D — Harness working-directory artifact for relative `src/test/resources/…` paths (HIGH — NOT a VM bug)
`PathResourceTests` builds resources from **relative** paths
`src/test/resources/org/springframework/core/io[/example.properties]`
(`PathResourceTests.java:56-60`). These resolve against the process CWD. The suite harness
`spring-suite/run-all-modules.sh` launches the VM **without `cd`-ing into the module dir**
(`run-all-modules.sh:76-78` / `94-96`); the CWD is the worktree area, while the file actually lives
at `apps/spring-framework/spring-core/src/test/resources/…` (confirmed present on disk). Under real
Gradle the test's CWD *is* the module root, so the relative path resolves.

CratonVM itself is behaving correctly here: it sets `user.dir` to the real process CWD
(`native-builtins/src/system_bootstrap.rs:166-167`), and `p57_read_path` resolves relative paths via
`std::fs::read` / `std::path::Path::exists` against that CWD
(`phases_late.rs:7342-7348`, `5841-5845`, `5945`). So `fileExists/dirExists/fileIsReadable/getFile/
getInputStream/getReadableByteChannel/getReadableByteChannelForDir` fail purely because the file is
not under the harness CWD. This is an **env/harness artifact**, not a CratonVM divergence — though it
masks the genuine Cause A/C bugs behind it. The two different separators in the FNFE messages
(`src\test\…` vs `src/test/…`) are a cosmetic rendering inconsistency between the `getInputStream`
and `readableChannel` paths, not a functional bug.

### Cause E/H — `UrlResource` / `ResourceUtils` URL parse & equals for `file:`/`jar:` no-handler URLs (MEDIUM)
`filenameIsExtractedFromFilePath()` exercises `new UrlResource("file:…?argh")` and `\`-separated
file URLs; `extractJarFileURL()/extractArchiveURL()` build `new URL(null,"jar:myjar.jar!/mypath",
handler)` and expect equality with `new URL("file:/myjar.jar")`. The passing `isJarURL()` shows
scheme detection works, so the divergence is in URL **parsing/normalization/equals** for the
no-authority `jar:`/`file:` forms (notably the spec-less `jar:myjar.jar!/…` → `file:/myjar.jar`
promotion and `?query`/`\`-separator filename trimming). Not yet pinned to a single file:line —
needs the synthetic `java.net.URL` parse path traced (lower confidence than A/B/C).

### Cause F — MockWebServer / HTTP `URL.openStream` (MEDIUM — networking, not file IO)
The 6 `UrlResourceTests` failures hit a live `MockWebServer` over localhost HTTP. Two surface as
`IOException: URL.openStream failed: <winsock 10049 WSAEADDRNOTAVAIL / 11003 WSANO_RECOVERY>`; the
rest assert HEAD/GET method + headers. Root cause is in the HTTP `URL.openStream` / `HttpURLConnection`
networking stack (native-io socket/host-resolution), or MockWebServer failing to bind on this box —
distinct subsystem from the file/Path family. Flagged for the networking cluster owner.

### Cause G — ModuleResource over a JDK module (MEDIUM)
`ModuleResource(Introspector.class.getModule(), "java/beans/Introspector.class")` must locate, stat,
and read a class file *out of a named JDK module*. The 3 failures (existing read+length, non-existing
exists/readable, equals/hashCode) indicate the module-resource read path
(`Module.getResourceAsStream` / `ModuleReader`) is not wired to return module content under CratonVM.
Not file:line-pinned here; shares no code with the `PathResource` family above, so treat as its own
fix item.

## Reproduction sketch (for later manual verification — DO NOT run during the active suite)

Cause A (write-channel AbstractMethodError):
```java
// RepWriteChan.java
import java.nio.*; import java.nio.channels.*; import java.nio.file.*;
public class RepWriteChan {
  public static void main(String[] a) throws Exception {
    Path p = Files.createTempFile("cv", ".bin");
    try (SeekableByteChannel ch = Files.newByteChannel(p, java.util.Set.of(StandardOpenOption.WRITE))) {
      ch.write(ByteBuffer.wrap("test".getBytes()));   // expect: 4 bytes written
    }                                                   // actual: AbstractMethodError WritableByteChannel.write
    System.out.println("len=" + Files.size(p));
  }
}
// cratonvm.exe --java-home "<jdk25>" -cp . RepWriteChan
```

Cause B (Path.toUri authority):
```java
// RepUri.java
import java.nio.file.*;
public class RepUri {
  public static void main(String[] a) {
    Path p = Path.of("x.txt").toAbsolutePath();
    System.out.println(p.toUri());      // expect file:/C:/.../x.txt   actual file:///C:/.../x.txt
  }
}
// cratonvm.exe --java-home "<jdk25>" -cp . RepUri
```

Cause C (newOutputStream on dir):
```java
// RepDirOut.java
import java.nio.file.*; import java.io.*;
public class RepDirOut {
  public static void main(String[] a) throws Exception {
    Path dir = Files.createTempDirectory("cv");
    try { Files.newOutputStream(dir); System.out.println("NO EXCEPTION (wrong)"); }
    catch (FileNotFoundException e) { System.out.println("FNFE (correct)"); }
    catch (IOException e) { System.out.println("IOException (wrong type): " + e); }
  }
}
// cratonvm.exe --java-home "<jdk25>" -cp . RepDirOut
```

Cause D (harness CWD — confirm it is env, not VM): run the PathResource tests with the working
directory set to `apps/spring-framework/spring-core`; the 8 relative-path failures that are *only*
Cause D should pass (leaving A/C as the genuine residuals). i.e. the harness should
`cd "$ROOT/$MOD"` before launching the VM, or pass an absolute resource root.

## Suspected subsystem
Primary: `native-builtins` synthetic `java.nio.file` layer (FileSystemProvider channel/output-stream
shims, `Path.toUri`) — `native-builtins/src/phases_late.rs`. Secondary (separate owners):
`java.net.URL` parsing (Cause E/H), HTTP/socket `URL.openStream` (Cause F, native-io networking),
module-resource reading (Cause G), and the suite harness (Cause D, `spring-suite/run-all-modules.sh`).

## Severity
- Cause A: **high** — a fundamental NIO write-channel API (`Files.newByteChannel(...WRITE)` →
  `WritableByteChannel.write`) is unusable; any framework writing through a channel breaks.
- Cause B: **medium** — wrong `Path.toUri()`/URL form breaks `file:`-URL round-trips and
  URLClassLoader-from-Path usage; high blast radius but a one-line rendering fix.
- Cause C: **low/medium** — narrow contract mismatch (exception type on dir-open).
- Cause D: **n/a (env)** — masks A/C; fix the harness, not the VM.
- Causes E/F/G: **medium**, but out of this family's core (URL/networking/module subsystems).
- ResourceEncoder `encode()`: out-of-family.
Overall cluster severity: **medium** (one high-impact VM bug A + one broad rendering bug B).

## Confidence
- Cause A: **high** (allocation + absent natives both verified by grep across the tree).
- Cause B: **high** (exact `format!("file://{abs}")` lines + the regex the test asserts).
- Cause C: **medium-high** (code path clear; have not confirmed `open_write` doesn't itself ENOENT first).
- Cause D: **high** (harness has no `cd`; `user.dir`/relative-resolution confirmed correct in VM; file confirmed present off-CWD).
- Causes E/H, F, G: **medium/low** (symptom-level; not file:line-pinned).

## Recommendation
**Fix** (in this checkout's `native-builtins`): A — register real `read`/`write`/`close`/`position`/
`truncate`/`size` natives for the `newByteChannel` stub (or back it by a `FileChannel`/`fd_table`
handle as `newOutputStream` already does), so `WritableByteChannel.write` resolves; B — change both
`Path.toUri` registrations (phases_late.rs:4229 and 6839) to emit single-slash `file:/…` matching the
already-fixed `File.toURI`; C — pre-check `is_dir` (or map the dir-open error) to a typed
`FileNotFoundException` in `fsp_new_output_stream`. A+B+C are small, local, and recover ~3 genuine
PathResource/ResourceTests failures directly. **Harness fix** (D): make `run-all-modules.sh` `cd`
into the module dir before launching, which should clear the 8 relative-path artifacts and unmask the
real residuals. **Handoff**: E/H (URL parse/equals) → URL/net owner; F (HTTP openStream / winsock) →
networking owner; G (ModuleResource) → module-loading owner; ResourceEncoder `encode()` →
Reactor/DataBuffer owner. Reason: those four touch subsystems disjoint from the `java.nio.file`
shims and warrant their own focused investigation.

## Open questions
1. Does `fd_table().open_write` on a directory already error (and with what kind) — i.e. is Cause C
   purely an error-type remap, or does it currently *succeed* and return a writable fd to a dir?
2. After the harness `cd` fix (D), do `fileExists/dirExists/fileIsReadable/getFile/getInputStream/
   getReadableByteChannel/getReadableByteChannelForDir` all pass, isolating A/C as the only true VM residuals?
3. For Cause B, do any *other* suites depend on the current `file://`-authority form (i.e. is anything
   relying on the buggy double-slash)? Check before flipping, given last-registration-wins has two copies.
4. Cause F: is the winsock `10049`/`11003` from MockWebServer failing to bind on this box, or from
   CratonVM's client-side host resolution? (Distinguishes a VM bug from an env limitation.)
5. Cause G: does CratonVM model `Module.getResourceAsStream` / a `ModuleReader` at all, or does
   ModuleResource need a synthetic content path keyed on the boot module's class files?
