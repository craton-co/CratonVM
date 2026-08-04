# Spring Boot loader/zip: a JIT-only failure cluster on `dev` (2026-08-04)

**Status: OPEN.** Found while regression-sweeping the `java.nio.file.Path`
trailing-separator fix
([retired doc](../../internal/fixed-suite-bugs/springboot/resourcestests-trailing-slash-path-normalization-FIXED-20260804.md));
it is **not** caused by that fix — it reproduces on an unmodified `dev`
binary. Filed separately because it is a JIT defect, not a `Path`/`Files` one.

## The finding

Four Spring Boot classes fail with the JIT on and **pass with `--nojit`**, on
the same binary, same host, same classpath:

| Module | Class | JIT on | `--nojit` |
|---|---|---|---|
| `loader/spring-boot-loader-tools` | `ImagePackagerTests` | FAIL 5.4s | **PASS 9.1s** |
| `loader/spring-boot-loader-tools` | `RepackagerTests` | FAIL 20.1s | **PASS 38.6s** |
| `loader/spring-boot-loader` | `NestedJarFileTests` | FAIL 4.2s | **PASS 23.2s** |
| `loader/spring-boot-loader` | `ZipContentTests` | CRASH 210.9s | **PASS 143.2s** |
| `core/spring-boot` | `OriginTrackedYamlLoaderTests` | FAIL 53.8s | **PASS 385.2s** |

(`ZipContentTests`' `--nojit` PASS was measured on the patched binary, its
JIT-on CRASH on the unpatched one; it is the same class either way — FAIL at
149.2s in the 08-02 baseline, CRASH at 156.8s in the 08-04 residual triage.)

Binary: `/data/data/cratonvm/target/release/cratonvm`, `dev` @ `6b6fc9a0dc`
(**unpatched**). Host: Azure Linux, JDK 25 (`/data/jdk25-real-20260717`).
Runner: `apps/spring-boot-suite-runner/run-spring-boot-suite.ps1`,
`-SpringBootRoot /data/data/springboot-jsonreader-deprecation-20260718`.

Runs: `.suite/results/suspect6-baseline-A` (JIT on) and
`.suite/results/zip5-base-nojit` (`-Jit off`).

`loader/spring-boot-loader` `SecurityInfoTests` fails in **both** modes
(FAIL 2.6s with JIT, FAIL 13.5s without) — a separate, non-JIT defect that
happens to share the same symptom family; do not fold it into this one.

All five were **PASS** in the 2026-08-02 Azure full-suite run
(`.suite/results/craton-fullsuite-azure-20260802`), so this is a regression
introduced by `dev` commits between 08-02 and 08-04 — not a long-standing gap.
That range is the natural bisect window.

## Symptom

Every failure surfaces as the JDK's own ZIP reader rejecting a stream it just
read, i.e. the bytes reaching `java.util.zip` are wrong:

```
java.util.zip.ZipException: invalid entry compressed size (expected 4259840 but got 2 bytes)
   java.util.zip.ZipInputStream.readEnd(ZipInputStream.java:639)
   java.util.zip.ZipInputStream.read(ZipInputStream.java:415)
   java.util.jar.JarInputStream.read(JarInputStream.java:270)
   java.util.zip.ZipInputStream.closeEntry(ZipInputStream.java:176)
   java.util.zip.ZipInputStream.getNextEntry(ZipInputStream.java:154)
   java.util.jar.JarInputStream.<init>(JarInputStream.java:138)
   org.springframework.boot.loader.tools.AbstractJarWriter.writeLoaderClasses(AbstractJarWriter.java:218)
```

Other spellings seen in the same run: `invalid compression method`,
`only DEFLATED entries can have EXT descriptor`,
`invalid entry size (expected 80 but got 1321 bytes)`. The yaml case is the
same shape one layer up — snakeyaml's scanner hitting a broken key at
line 142539 of a 3 MB generated document (`canLoadFilesBiggerThan3Mb`).

The input is **not** the problem. The loader jar that `writeLoaderClasses`
reads is byte-identical under HotSpot and under CratonVM:

```
url=file:/…/spring-boot-loader-tools/build/generated-resources/main/META-INF/loader/spring-boot-loader.jar
first16=504b0304140000080800000041000000 total=203918
```

and reading it to EOF through `new BufferedInputStream(getResourceAsStream(…))`
yields the same 203918 bytes in the same 26 reads, with no zero-length read
and `-1` at the end, on both VMs (`~/trailsep/ResProbe.java`,
`~/trailsep/EofProbe.java`). So the corruption appears *between* a correct
byte stream and `java.util.zip`'s view of it — which is where the JIT sits.

## The trailing-separator fix turns two of these into HANGs

With `fix/nio-path-trailing-separator-20260804` applied,
`ImagePackagerTests`/`RepackagerTests` stop failing in seconds and instead burn
the whole per-class timeout (600s, reproduced at 200s/240s/250s ceilings, 5/5
runs). That is a change of *symptom*, not of cause: the same two classes still
pass with `--nojit` on the patched binary (`ImagePackagerTests` PASS 20.2s).
The path fix lets the test get further into the same broken code before the
corrupt stream shows up, and the state it reaches there happens to be an
unexitable loop instead of a throw.

Watchdog stack dump (`--stack-dump-on-timeout`), identical at 30s and 120s
except for the `Inflater.inflate` bci, i.e. spinning, not progressing:

```
[78] org/springframework/boot/loader/tools/AbstractJarWriter.writeLoaderClasses@60
[79] java/util/jar/JarInputStream.getNextJarEntry@4
[81] java/util/zip/ZipInputStream.getNextEntry@15
[82] java/util/zip/ZipInputStream.closeEntry@18       <- while (read(buf) != -1);
[84] java/util/zip/ZipInputStream.read@67
[85] java/util/zip/InflaterInputStream.read@91        <- while ((n = inf.inflate(...)) == 0)
[86] java/util/zip/Inflater.inflate@48 / @67
```

## What was already ruled out (do not redo)

* **Not the input stream.** See the two probes above.
* **Not `Path`/`Files`.** The failures reproduce on an unpatched `dev` binary.
* **Not the `Inflater` natives' error handling.** Two candidate fixes were
  written, built and measured, and **neither changed the hang**:
  1. `zip_streams.rs` (`inflater_advance`, shipped): the synthetic-layout
     inflater kept its input cursor short of the end when zlib made no
     progress, so `needsInput()` stayed false forever. A real fix on its own
     merits — but the wrong subsystem for this bug, because that registration
     is not the live one in real-JDK mode (the live path is the real
     `java.util.zip.Inflater` bytecode calling `zip_real.rs`'s
     `inflateBytesBytes`).
  2. `zip_real.rs`: that live native reports `Z_DATA_ERROR` as
     `(0 consumed, 0 produced, not finished, no dictionary)` where HotSpot
     throws `DataFormatException` — which is another unexitable-loop shape on
     paper. Making it throw was built and measured too: `ImagePackagerTests`
     still hung, so the change was not kept. **It is still a real JDK-fidelity
     gap** and is worth fixing on its own terms; it is simply not this bug, and
     it should not be re-attempted as a fix for this bug.

  The spin is upstream of both. With the JIT off, the same natives see the same
  jar and the tests pass.

## Next step

Bisect `dev` between the 08-02 full-suite commit and `6b6fc9a0dc` with
`ImagePackagerTests` as the oracle — it is a 9-second `--nojit` PASS versus a
5-second JIT FAIL, so each bisect step is cheap. The JIT lever to reach for is
`CRATONVM_DBG=jit-bisect-only=` (the DBG group — the `CRATONVM_JIT=` spelling
is rejected on stderr and every arm then reads as "no effect"), and remember a
`jit-compiled` census of 0 does not prove nothing compiled: check
`CRATONVM_DBG=osr` too.
