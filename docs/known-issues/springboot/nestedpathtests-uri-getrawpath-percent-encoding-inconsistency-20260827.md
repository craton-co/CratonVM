# `Path.toUri().getRawPath()` returns an unencoded path while `toString()` on the same URI is correctly encoded

## Status
**OPEN, confirmed CratonVM-specific, root-caused with a minimal repro
independent of Spring Boot.** Found triaging Spring Boot's `loader/spring-boot-loader`
residual (`org.springframework.boot.loader.nio.file.NestedPathTests`).
Differential-verified against stock HotSpot 25: passes cleanly.

## Symptom
```
NestedPathTests.toUriWhenHasSpecialCharsReturnsEncodedUri()
java.io.IOError: java.net.URISyntaxException: Illegal character in path
  at index 40: nested:/tmp/junit-.../te st.jar/!ne%20sted.jar
	at org.springframework.boot.loader.nio.file.NestedPath.toUri(NestedPath.java:148)
```
HotSpot: 31/31 tests pass. CratonVM: this one test fails.

## Root cause

`NestedPath.toUri()` (real Spring Boot source, unmodified):
```java
String uri = "nested:" + this.fileSystem.getJarPath().toUri().getRawPath();
if (this.nestedEntryName != null) {
    uri += "/!" + UriPathEncoder.encode(this.nestedEntryName);
}
return new URI(uri);
```
It builds the new URI string from `getJarPath().toUri().getRawPath()` — relying
on the JDK's documented `Path.toUri()` contract, which returns a URI whose
path component is already properly percent-encoded, so `getRawPath()` should
be safe to concatenate directly. The nested entry name is separately encoded
by hand via `UriPathEncoder.encode(...)`.

**Minimal, Spring-free repro** isolating exactly this:
```java
Path p = Files.createTempFile("te st", ".jar");
URI uri = p.toUri();
System.out.println("toUri(): " + uri);
System.out.println("getRawPath(): " + uri.getRawPath());
```
```
                    toUri()                              getRawPath()
HotSpot:   file:///tmp/te%20st....jar        /tmp/te%20st....jar       (encoded)
CratonVM:  file:///tmp/te%20st....jar        /tmp/te st....jar         (NOT encoded)
```
**`toString()` on the exact same `URI` object is correctly percent-encoded on
both VMs — only `getRawPath()` differs.** This means CratonVM's `URI`
implementation carries two internally inconsistent representations of the
same path: whatever `toString()` renders from is correctly escaped, but the
field/accessor `getRawPath()` reads from is not — the literal, unescaped
constructor input, as if `URI`'s raw-path field is populated with something
other than the value its own `toString()` uses. Any code that trusts
`getRawPath()`'s documented contract (as `NestedPath.toUri()` legitimately
does) and concatenates it into a new URI string is exposed; code that only
ever calls `toString()` never notices.

## Why this is worth fixing generally, not just for Spring Boot
This isn't Spring-Boot-specific — it's a `java.net.URI` / `java.nio.file.Path.toUri()`
correctness bug reachable by any code following the JDK's own documented
`getRawPath()` contract. `Files.createTempFile` (and any path containing a
space, `%`, or other URI-reserved character) triggers it.

## Related, ruled out
`ZipContentTests` (same `loader/spring-boot-loader` module, seen failing in
the same sweep) is unrelated and not a CratonVM bug at all: 28/29 tests pass,
the sole non-pass is `TestAbortedException: Assumption failed: Insufficient
disk space` on `openWhenZip64ThatExceedsZipSizeLimitOpensZip` — a host
resource constraint (that test needs several GB of scratch space to build a
Zip64 archive past the standard size limit), which the harness's status
classifier counts as `FAIL` even though the JUnit result itself has zero
actual failures. Not investigated further; flagged so the harness's ABORTED-
counts-as-FAIL classification doesn't get mistaken for a VM defect again.

## Next steps
* Find where CratonVM constructs the `URI` object's internal raw-path field
  vs. what `toString()` recomputes from — likely two separate fields/code
  paths in the native `URI` constructor or in `Path.toUri()`'s own
  string-building, one of which escapes and one of which doesn't.
* Check whether other `URI` accessors (`getRawSchemeSpecificPart()`,
  `getRawQuery()`, `getRawFragment()`, `getRawAuthority()`) share the same
  inconsistency, since they'd plausibly come from the same underlying field.
* Once fixed, rerun `NestedPathTests` to confirm 31/31.

## Repro
```bash
source <toolchain env>
javac -d probeclasses PathToUriProbe.java
<cratonvm-bin> --java-home <jdk25-home> -c probeclasses PathToUriProbe
# compare against: <jdk25-home>/bin/java -cp probeclasses PathToUriProbe
```
```java
import java.nio.file.Files;
import java.nio.file.Path;
import java.net.URI;

public class PathToUriProbe {
    public static void main(String[] args) throws Exception {
        Path p = Files.createTempFile("te st", ".jar");
        URI uri = p.toUri();
        System.out.println("toUri(): " + uri);
        System.out.println("getRawPath(): " + uri.getRawPath());
        System.out.println("contains literal space in raw path: " + uri.getRawPath().contains(" "));
        Files.deleteIfExists(p);
    }
}
```
