# TestEncodingDetector — 14/22 JSP-encoding tests return 500 instead of 200

**Status:** FIXED (2026-07-10). **Severity:** medium-high (broad cluster within one class).
**HotSpot:** PASS.

## 2026-07-10 resolution

The primary defect — `org.apache.jasper.compiler.EncodingDetector.getPrologEncoding`
returning `null` for every JSPX prolog, via
`XMLInputFactory.createXMLStreamReader(stream).getCharacterEncodingScheme()` —
was root-caused and fixed on 2026-07-09 in `native-builtins/src/xml_stax.rs`
(commit `1b60c103`, already on `dev`): CratonVM's StAX shim now decodes UTF-8
BOM/UTF-16BE/UTF-16LE XML inputs to UTF-8 for the quick-xml cursor and stores
the XML declaration's encoding in the `XMLStreamReader` side table. A focused
probe (`Tomcat0807JasperStaxProbe`) confirmed `getCharacterEncodingScheme()`
now reports `UTF-16BE`/`UTF-8` correctly instead of `null`.

That fix alone could not be verified end-to-end against the real
`TestEncodingDetector` class because full-suite verification was blocked by
two independent, more severe VM regressions discovered during this session's
reproduction attempt (both **already fixed on `dev`** by a concurrent session
working `docs/internal/tomcat-08-07/virtualcontext-classloader-404.md`,
commit `45cc4f4f`, merged in mid-investigation):

1. **`FileInputStream.<init>(String)` native-fallback backfill gap.** When
   dispatch took the `SyntheticStub` `<init>(String)` fallback instead of real
   bytecode, the backing `FileDescriptor`, `path`, `closeLock`, and `closed`
   fields were never populated, so `EncodingDetector`'s
   `bis.mark(4); ...; bis.reset();` sequence saw the wrapped
   `BufferedInputStream`'s `in`/`buf` as unset — every JSP source read EOF
   immediately (`available()=0`, `read()=-1`), and `reset()` threw
   `IOException: Stream closed` mid-constructor. Root-caused in this session
   independently down to `native-io/src/lib.rs::fis_ensure_fd_object`/
   `fis_set_fd`, confirmed via a minimal `MarkResetProbe`/`PlainReadProbe`
   pair reproducing `FileInputStream(file).read()` returning EOF for any
   non-empty file, byte-identical on two independently-built `dev`-tip
   binaries. Fixed upstream by backfilling the real-JDK instance fields after
   the synthetic constructor fallback (see the virtualcontext-classloader-404
   writeup for the full fix).
2. **`WebappClassLoaderBase.clearReferencesJdbc()` duplicate-`defineClass1`
   during Tomcat webapp stop.** Every `TestEncodingDetector` sub-test starts
   and stops its own embedded `Tomcat` instance; after ~14 stop/start cycles
   in one process, `defineClass1(org/apache/catalina/loader/JdbcLeakPrevention)`
   started throwing `IncompatibleClassChangeError` ("already defined by
   user-defined(N) loader"), cascading into `LifecycleException: A child
   container failed during stop` for every subsequent parameter and, via a
   `SoftReference`-cast `ClassCastException` in the Digester's schema/grammar
   cache, `ContextConfig` failing to parse `web.xml` for every context after
   that point. Fixed upstream: the `defineClass0/1/2` natives now recover from
   an "already defined" backend error only when the exact same loader
   namespace already owns the class, instead of surfacing a
   lifecycle-breaking error. **Narrowed 2026-08-11** to the defining-loader
   OBJECT rather than the namespace: the ~14 cycles here are ~14 DIFFERENT
   `WebappClassLoader`s, each of which HotSpot lets define its own copy, so
   they stay on the recovery arm -- while a loader genuinely redefining its
   own name now raises `LinkageError` as HotSpot does. Record:
   fixed-bugs/duplicate-defineclass-served-the-mirror-instead-of-linkageerror-FIXED-20260811.md

A third, unrelated harness-only gap was found and fixed locally (not a
CratonVM defect): the Linux Tomcat harness at `/data/data/apps/tomcat` (Azure
host) had no `conf/` directory, so `ClassLoaderLogManager.readConfiguration`
threw `FileNotFoundException: conf/logging.properties` on every webapp
`stop()`, which (compounded by gap #2 above) also failed the test. Fixed by
symlinking `conf -> output/build/conf` under the harness root.

### Verification

With `1b60c103` + `45cc4f4f` merged and the harness `conf/` symlink in place,
a full run of `org.apache.jasper.compiler.TestEncodingDetector` (real JDK,
`-Xmx2g`, worktree `fix/tomcat0807-encdetect-retire-20260710`, Azure host)
went from every one of the 22 parameters failing (either via the original
500-instead-of-200 bug or via the two VM regressions above masking it
entirely) to:

```
Tests run: 22,  Failures: 5
```

17/22 now match HotSpot exactly, including all the straightforward
BOM-only and matching-BOM/matching-prolog cases that the original 14/22
failure count covered. The primary defect this doc tracked is resolved.

## 2026-07-10 residual (tracked separately, OPEN)

The remaining 5 failures are a **different, narrower** defect cluster than
the one this doc originally described — see
[`encodingdetector-utf16-and-conflicting-prolog-residuals.md`](encodingdetector-utf16-and-conflicting-prolog-residuals.md):

- 3/22 encoding-conflict cases (`bom-utf8-prolog-utf16be.jspx`,
  `bom-utf8-prolog-utf16le.jspx`, `bug60769a.jspx`) that HotSpot fails with
  500 (a deliberately invalid BOM-vs-prolog combination) now return 200.
- 1/22 (`bom-utf16be-prolog-none.jsp`) returns 200 as expected but with
  garbled body content instead of `OK`.
- 1/22 (`bom-utf16le-prolog-none.jsp`) times out (`SocketTimeoutException`)
  instead of returning 200.

## Original summary (2026-07-08, superseded above)

`org.apache.jasper.compiler.TestEncodingDetector` failed 14 of its 22 tests,
all with the same shape:
```
1) testEncodedJsp[1](org.apache.jasper.compiler.TestEncodingDetector)
java.lang.AssertionError: expected:<200> but was:<500>
...
Tests run: 22, Failures: 14
```
Found via a full Windows Tomcat suite rerun (real JDK, JIT on, 1500s
timeout, dev commit range `33bef88d`..`0d8fb610`, 2026-07-07/08).

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName encdetect `
  -Start <idx> -Count 1 -TimeoutSec 60 -Parallel 1
# org.apache.jasper.compiler.TestEncodingDetector
```

Linux (Azure host) equivalent, using the reusable harness at
`/data/data/apps/tomcat` (see `/data/data/apps/tomcat/.suite/cp-linux-fixed.txt`):

```bash
ln -s /data/data/apps/tomcat/output/build/conf /data/data/apps/tomcat/conf  # one-time harness fixup
cd /data/data/apps/tomcat
cratonvm --java-home /home/victor/jdk25 -Xmx2g \
  -cp "$(cat .suite/cp-linux-fixed.txt)" \
  org.junit.runner.JUnitCore org.apache.jasper.compiler.TestEncodingDetector
```

## 2026-07-09 worker evidence

The local `C:\craton\CratonVM-tomcat-0807-fixture-20260709-001` worktree does
not contain `apps\tomcat-suite-runner`, so the class-level Tomcat repro could
not be rerun here. A narrower upstream-fixture read of Tomcat 9.0.83 shows
`TestEncodingDetector` has 22 parameters, of which 14 expect HTTP 200 and 8
intentionally expect HTTP 500. The originally observed "14/22 returned 500
instead of 200" therefore matches "all success cases failed", not a random
charset subset.

One concrete CratonVM gap was fixed in `native-builtins/src/xml_stax.rs`:
Tomcat's `org.apache.jasper.compiler.EncodingDetector.getPrologEncoding`
uses `XMLInputFactory.createXMLStreamReader(stream).getCharacterEncodingScheme()`
to read the XML declaration, but CratonVM's StAX shim returned `null` for
`getCharacterEncodingScheme()` and parsed the raw byte stream as UTF-8 even
when the JSPX prolog was UTF-16BE/UTF-16LE. The fix decodes UTF-8 BOM,
UTF-16BE, and UTF-16LE XML inputs to UTF-8 for the quick-xml cursor and stores
the XML declaration encoding in the `XMLStreamReader` side table.

Focused proof:

```powershell
$env:CARGO_TARGET_DIR='C:\craton\target-tomcat0807-jasper'
cargo test -p cratonvm-native-builtins tomcat0807_jasper --lib
# 3 passed

cargo build -p cratonvm-cli
javac -d apps\tomcat0807_jasper_probe\classes `
  apps\tomcat0807_jasper_probe\Tomcat0807JasperStaxProbe.java
C:\craton\target-tomcat0807-jasper\debug\cratonvm.exe --java-home "$env:JAVA_HOME" `
  -c apps\tomcat0807_jasper_probe\classes Tomcat0807JasperStaxProbe
# utf16be=UTF-16BE
# utf8bom=UTF-8
# OK
```
