# `Path.toUri().getRawPath()` returned an unencoded path while `toString()` on the same URI was correctly encoded — FIXED, and the "do the other raw accessors share it?" residual is swept

## Status
**FIXED and CLOSED 2026-08-28.** The defect landed as `eaa81f90d`
(`fix(net,nio): URI.getRawPath() answered the DECODED path on a Path.toUri()
result`). This page's own two open items are now discharged:

* *"rerun `NestedPathTests` to confirm 31/31"* — **done**, on the real Spring
  Boot suite runner, not a synthetic repro: `tests=31 failed=0 aborted=0
  skipped=0`, PASS in 2.0 s.
* *"check whether the other raw accessors share the same inconsistency"* —
  **done**, and the answer is **no**. An exhaustive accessor sweep is
  byte-identical to HotSpot 25 across every shape, all 245 lines of it.

## The defect, and the fix

`NestedPath.toUri()` (real Spring Boot source, unmodified) builds a fresh URI
string out of `getJarPath().toUri().getRawPath()`, relying on the JDK's
documented `Path.toUri()` contract that the path component comes back already
percent-encoded:

```
                    toUri()                              getRawPath()
HotSpot:   file:///tmp/te%20st....jar        /tmp/te%20st....jar       (encoded)
CratonVM:  file:///tmp/te%20st....jar        /tmp/te st....jar         (NOT encoded)
```

so the concatenation produced
`IOError: URISyntaxException: Illegal character in path at index 40`.

Two defects, one on top of the other, both corrected by `eaa81f90d`:

1. `uri_publish_named` wrote **one** string into two fields that are not the
   same thing. `path` is the raw component (`getRawPath()`); `decodedPath` is
   its percent-decoded twin (`getPath()`). Whichever spelling the caller handed
   it, the other accessor was wrong. `decodedPath` is now derived by decoding.
2. Both `Path.toUri()` registrations passed the **decoded** path as that
   override, having computed the encoded one for the URI text two lines above.
   `java/nio/file/Path.toUri()` is registered **twice** (the phase-57 registrar
   and the phases-late one) and the later wins, so fixing the first copy alone
   changed nothing observable — which is how the duplicate was found.

## The residual, swept: no other raw accessor shares the inconsistency

The question this page left open was a real one, and it could not be answered by
reading: the `getRawPath()` defect was invisible on any path that needed no
escaping, so the only sound test is to put a reserved character in **each**
component in turn and diff the whole accessor table.

`probes/UriRawAccessorSweepProbe.java` (added with this record) prints all
nineteen accessors — `toString`, `toASCIIString`, `getScheme`, `isOpaque`,
`isAbsolute`, and the six raw/decoded pairs `SchemeSpecificPart`, `Authority`,
`UserInfo`, `Path`, `Query`, `Fragment`, plus `getHost`/`getPort` — for eleven
URI shapes:

| shape | why it is in the set |
|---|---|
| `Path.toUri` on a path with a space **and** a literal `%` | the original defect's route, plus the `%` → `%25` case a decode-then-re-encode would lose |
| `Files.createTempFile(...).toUri()` | exactly what `NestedPathTests` does |
| escape in path / query / fragment / userinfo / authority | one component at a time, so no wrong accessor can hide behind another's right answer |
| escape everywhere | all six at once |
| opaque with escapes (`mailto:`) | the null-path branch |
| relative with escape | the no-scheme, no-authority branch |
| no escapes at all | the control: raw and decoded must coincide |
| authority-only (`http://h`), empty authority (`file:///tmp/x`) | the `""`-vs-`null` path distinction |

and finally a **round trip**: rebuild a URI string from `getScheme` +
`getRawAuthority` + `getRawPath` + `getRawQuery` + `getRawFragment` and assert
it re-parses `.equals()` to the original. That is the property
`NestedPath.toUri()` actually depends on, stated directly.

```
$ java     -cp classes UriRawAccessorSweepProbe > hotspot-uri.txt   # 245 lines
$ cratonvm --java-home <jdk25> -cp classes UriRawAccessorSweepProbe > craton-uri.txt
$ diff hotspot-uri.txt craton-uri.txt && echo IDENTICAL
IDENTICAL
```

Byte for byte, including the rows that were already correct — the point of a
sweep is that the control rows are in it. Why the other accessors were never
exposed is visible in the code once you look: `getRawQuery`, `getRawFragment`,
`getRawAuthority`, `getRawUserInfo` and `getRawSchemeSpecificPart` all
**re-parse the raw URI text** (`net_phase_e.rs`, the units-carrying
`uri_raw_units` route) rather than reading a stored component field, so there
was never a second representation for them to disagree with. `path` was the
one component with a stored raw field *and* a stored decoded twin, and that is
precisely where the write went wrong.

## The original failing class, re-run

Not a synthetic repro — the real suite runner, real Gradle classpath, real
module:

```
index  module                     class                  status  seconds  tests  failed  aborted
1      loader/spring-boot-loader  ...nio.file.NestedPathTests   PASS   2.036     31       0        0
```

The sibling class this page adjudicated in the same sweep behaves exactly as it
said, and is not a CratonVM bug:

```
2      loader/spring-boot-loader  ...zip.ZipContentTests        FAIL  45.619     29       0        1
```

29 tests, **zero failures**, one `TestAbortedException: Assumption failed:
Insufficient disk space` on `openWhenZip64ThatExceedsZipSizeLimitOpensZip`. The
harness's classifier counts an ABORTED-containing run as FAIL; the JUnit result
has no failure in it. That adjudication is already recorded in
`../../../known-issues/springboot/not-cratonvm-bugs-consolidated.md`.

## Repro (the sweep, not just the original symptom)

```bash
javac -d classes probes/UriRawAccessorSweepProbe.java
java                          -cp classes UriRawAccessorSweepProbe > hotspot.txt
cratonvm --java-home <jdk25>  -cp classes UriRawAccessorSweepProbe > craton.txt
diff hotspot.txt craton.txt
```

## Related
- `probes/UriRawAccessorSweepProbe.java` — the sweep above.
- `probes/UriRawPathProbe.java` — the narrower probe added with the fix, which prints both construction routes side by side.
- `../../../known-issues/springboot/not-cratonvm-bugs-consolidated.md` — `ZipContentTests`.
