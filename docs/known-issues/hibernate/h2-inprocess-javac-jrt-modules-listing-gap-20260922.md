# H2's in-process `javac` (via `CREATE ALIAS ... AS $$ ... $$`) — the jrt/platform-module chain is FIXED; a separate real-JAR classpath-scan gap remains

**Status:** the ORIGINAL bug this doc tracked — real in-process `javac` unable
to bootstrap its platform (`jrt:`) classpath at all — is now **fully fixed**
(§1–§4, closed across this session and one concurrent session). Getting past
that revealed a **different, unrelated** blocker in ordinary JAR classpath
scanning (§5, open) which is the reason the 5 classes below still fail.
Confirmed by direct measurement: `JavacProbe`'s standalone repro now compiles
cleanly (`compiler.run rc=0`, no platform-classpath error at all).

**Classes affected (5), all sharing §5's mechanism now (not §1–§4's):**
- `org.hibernate.orm.test.sql.storedproc.ResultMappingTest`
- `org.hibernate.orm.test.sql.storedproc.StoredProcedureTest`
- `org.hibernate.orm.test.sql.storedproc.StoredProcedureResultSetMappingTest`
- `org.hibernate.orm.test.jpa.procedure.StoredProcedureResultSetMappingTest`
- `org.hibernate.orm.test.delegation.SessionDelegatorBaseImplTest`

All five **pass cleanly on real HotSpot** with the identical classpath —
a genuine CratonVM defect, not a test-fixture or environment artifact.

## The recurring shape that closed §1–§4

Two separate, individually-correct decisions collided repeatedly: HIB-CV-27
(2026-09-16/22) stamps every jar/jrt `Path`/`DirectoryStream`/`FileSystem`
with the REAL concrete `sun.nio.fs.*` class name for `getClass()`/`instanceof`
fidelity; an older, unrelated Lane 4 wave retired a long list of
`WindowsPath`/`WindowsDirectoryStream`/`WindowsFileSystem` methods so a REAL
host filesystem operation runs as real bytecode end to end. A jar/jrt object
wearing that class is then subject to methods contracted to run as real
bytecode, which read fields this VM's allocator never populated the way a
real constructor would — a different concrete break on whichever method the
next javac call path happened to reach. Fixed, one collision at a time:

1. **`DirectoryStream`** (`close`/`iterator`) — mint a jar/jrt stream against
   the bare abstract interface instead of the concrete class, so there is no
   real Code to lose dispatch to. Landed twice, independently, the same day
   (this session and a concurrent one via a Spring Boot bug); merged, the
   more complete version kept. See `docs/internal/fixed-suite-bugs/hibernate/
   jrt-directorystream-shares-a-retired-shadow-class-with-real-host-streams-FIXED-20260922.md`.
2. **Six `WindowsPath` methods** (`toAbsolutePath`, `toRealPath`,
   `getFileName`, `resolve`, `equals`, `hashCode`) — `register_with_kind(...,
   NativeKind::Intrinsic)` (bypasses the auto-downgrade-to-`SyntheticStub` a
   plain `register()` gets on a retired triple) with a receiver-aware body:
   jar/jrt reuses this VM's own already-correct logic, anything else
   delegates unchanged to `ctx.invoke_virtual_bytecode_only(...)`. See
   `docs/internal/fixed-suite-bugs/hibernate/
   jrt-windowspath-retired-shadow-collisions-FIXED-20260922.md`.
3. **A concurrent session's independent, more direct fix**, same day
   (commit `6dac5f080`, found via Spring's `TestCompiler`/
   `CandidateComponentsIndexer`): `WindowsFileSystemProvider` carrier-sharing
   (`getScheme`, `newFileSystem`), missing `defaultDirectory()`/
   `defaultRoot()` natives for the synthetic default `FileSystem`,
   unpopulated `root`/`kind`/`offsets` on jar/jrt `Path` objects, and —
   independently of this session's `getFileName` Intrinsic fix — the
   ACTUAL root cause of that symptom: real `WindowsPath.getFileName()`/
   `.getParent()` scan `path` for a **literal `\`**, independent of the
   `offsets` cache entirely (`javap`-verified), so a jar/jrt sentinel path
   stored in this VM's internal `/` form always returned the whole sentinel
   string as "the file name." Fixed by storing a vfs Path's Java-visible
   string with `\` (Windows only) and undoing the conversion centrally in
   `vfs_decode` so Rust-side readers are unaffected. Merged cleanly (no
   conflicts) with this session's six-method fix — the two approaches
   overlap in places (both now correctly answer `getFileName`, via different
   mechanisms) but do not contradict.
4. **This session's own follow-up fix, needed to close the loop**: the
   stored-form-to-`\` change in #3 left `FileSystem.getSeparator()` for a
   jar/jrt filesystem still unconditionally answering `/` — a real
   inconsistency between what the Java-visible string now contains and what
   the filesystem claims its own separator is. Real
   `PathFileObject$SimpleFileObject.inferBinaryName`'s `toBinaryName` does
   `relativePath.toString().replace(sep, ".")`; with `sep` wrong, nothing
   matched and the binary name came back with the literal separators still
   in it (`"java\lang\AbstractMethodError"`, not a valid identifier), so
   `ClassFinder.fillIn`'s `SourceVersion.isIdentifier` check silently
   dropped every file and `java.lang`'s `PackageSymbol` completed with zero
   members. Fixed in `nio_file.rs`'s `getSeparator` registration: answer
   `\` for a virtual (jar/jrt) filesystem on Windows too, matching #3's
   stored-form convention instead of the pre-#3 assumption that stored form
   and separator were both always `/`.

**Verified end to end**: standalone `JavacProbe.java` (`ToolProvider
.getSystemJavaCompiler().run(...)` compiling a trivial one-class source with
no explicit classpath) now returns `compiler.run rc=0` — a full, real,
in-process javac compile against the real `jrt:`-backed platform classpath,
with zero errors. This was the literal original symptom the whole
investigation started from (`NotDirectoryException`/"Unable to find package
java.lang in platform classes"); it is gone. Module discovery finds all 70
platform modules under their real names; `java.lang`'s package listing
returns its correct 299-member content.

## 5. NOT fixed: real host JAR classpath scanning via `FileSystemProvider.newFileSystem(Path, Map, ClassLoader)`

Once §1–§4 closed the platform-classpath bootstrap, the 5 target Hibernate
classes get measurably further — no more platform-classpath error — and now
fail with a **different, ordinary-looking** javac diagnostic:

```
error: package org.h2.tools does not exist
import org.h2.tools.SimpleResultSet;
                   ^
error: cannot find symbol
    SimpleResultSet rs = new SimpleResultSet();
```

`org.h2.tools.SimpleResultSet` is a class inside H2's OWN jar
(`h2-2.4.240.jar`), which IS on the compile classpath — this is not a
platform/`jrt:` module at all, and none of §1–§4's fixes apply here.

**Isolated, precisely, to one call:** H2's `SourceCompiler.javaxToolsJavac`
(`javap`-read from the actual `h2-2.4.240.jar` on this classpath) calls
`compiler.getTask(writer, fileManager, null, null, null, compilationUnits)`
— no explicit `-classpath` option, so `StandardJavaFileManager`'s DEFAULT
`StandardLocation.CLASS_PATH` (populated from `java.class.path`) is what has
to work. Measured directly (`ClasspathListProbe.java`, this session's
scratchpad):

- `fm.hasLocation(StandardLocation.CLASS_PATH)` → `true`.
- `fm.getLocationAsPaths(StandardLocation.CLASS_PATH)` correctly lists
  `h2-2.4.240.jar` among its entries.
- `fm.list(StandardLocation.CLASS_PATH, "org.h2.tools", EnumSet.of(CLASS),
  false)` → **0 files**, no exception.

Narrowed further (`JarFsProbe.java`): opening the SAME jar directly via
`FileSystems.newFileSystem(URI.create("jar:file:///" + jarPath),
Collections.emptyMap())` and listing `/org/h2/tools` works completely
correctly — 32 entries, `Files.exists`/`Files.isDirectory` both `true`. So
the zip-mounting and directory-listing machinery itself is fine when reached
this way.

The difference: `javap`-read from the real `jdk.compiler` module,
`JavacFileManager$ArchiveContainer`'s constructor does NOT use the
URI-based `FileSystems.newFileSystem(URI, Map)` overload my probe used — it
uses the **`Path`-based** overload,
`FileSystems.newFileSystem(Path, Map, ClassLoader)`. That overload's real
contract is to iterate `FileSystemProvider.installedProviders()` and call
`.newFileSystem(path, env)` on each until one succeeds. This VM's own
synthetic "jar" provider (`p57_alloc_provider(ctx, "jar")`, abstract-stamped,
present in `installedProviders()` specifically so `FileSystems.newFileSystem
(jarUri, ...)`'s scheme-based lookup finds a "jar" scheme at all — see
`p57_alloc_provider`'s own 2026-09-16 doc comment) is a candidate this
iteration would also reach. Whether it (a) has a registered
`newFileSystem(Path, Map)` native that returns something structurally valid
but non-functional for enumeration, ahead of the real zip provider getting a
turn, or (b) something else in this specific overload's dispatch is what
produces the silent empty listing, has NOT been determined — this is as far
as this session got.

**Reproduction**:

```bash
JDK="C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot"
"$JDK/bin/javac" -d <outdir> ClasspathListProbe.java   # source: fm.list(StandardLocation.CLASS_PATH, "org.h2.tools", EnumSet.of(JavaFileObject.Kind.CLASS), false) over fm.getStandardFileManager(null,null,null), plus fm.getLocationAsPaths(CLASS_PATH) to confirm the jar is listed
cd <hib-suite-runner, so @common.args resolves>
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 "$CV" --java-home "$JDK" @common.args ClasspathListProbe
```

Real class, same failure (now at `org.h2.tools`, not platform classes):

```bash
CV_BIN=<cratonvm.exe> JDK=<jdk-home> ./run-hib.sh --list <(printf 'org.hibernate.orm.test.sql.storedproc.StoredProcedureTest\n') --timeout 180
```

## Related

- `docs/internal/fixed-suite-bugs/hibernate/
  jrt-directorystream-shares-a-retired-shadow-class-with-real-host-streams-FIXED-20260922.md`
- `docs/internal/fixed-suite-bugs/hibernate/
  jrt-windowspath-retired-shadow-collisions-FIXED-20260922.md`
- `docs/known-issues/h2/
  fs-cluster-needtoresolveagainstdefaultdirectory-investigation-20260922.md`
  — a DIFFERENT, Linux-specific default-`FileSystem`-singleton race in the
  same general neighborhood (H2 in-process javac), not the same bug as §5.
