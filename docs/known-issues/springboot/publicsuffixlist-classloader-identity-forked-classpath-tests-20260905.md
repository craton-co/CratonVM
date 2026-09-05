# PublicSuffixList fails to cast to itself in every Spring Boot test that forks/modifies the classpath

## Status

**OPEN.** Symptom confirmed reproducible and GC-independent across all three
collectors; root cause narrowed to Spring Boot's own classloader-forking test
infrastructure, but the specific CratonVM classloading gap is not yet pinned
to a source line.

## The symptom

Four classes, identical stack frame, full 3-GC Spring Boot suite run
(2026-09-04/05, `full_{generational,g1,zgc}_mysession_20260904_041104`):

```
java.lang.ClassCastException: class org.apache.hc.client5.http.psl.PublicSuffixList
  cannot be cast to class org.apache.hc.client5.http.psl.PublicSuffixList
	at org.apache.hc.client5.http.psl.PublicSuffixMatcher.<init>(PublicSuffixMatcher.java:88)
```

- `org.springframework.boot.restclient.autoconfigure.RestClientObservationAutoConfigurationWithoutMetricsTests`
- `org.springframework.boot.restclient.autoconfigure.RestTemplateObservationAutoConfigurationWithoutMetricsTests`
- `org.springframework.boot.security.autoconfigure.web.servlet.SecurityFilterAutoConfigurationEarlyInitializationTests`
- `org.springframework.boot.servlet.autoconfigure.MultipartAutoConfigurationTests`

`FAIL` on Generational, G1, and ZGC alike (one of the four showed `CRASH` on
Generational specifically, but that was the now-fixed generational-heap
`is_object_address` bug masking the identical underlying `FAIL` — G1 and ZGC
on the same run already showed the real failure cleanly for that class).

A class failing to cast to itself, by name, is the textbook signature of the
same class having been loaded by two different classloaders — `Class`
identity in the JVM is `(name, defining loader)`, not name alone.

## Two more plausible mechanisms checked and ruled out

This project has a **prior, unrelated** bug with the exact same misleading
message shape — `bug-h2-testmultithread-mvstore-writer-object-identity-20260816.md`'s
retired `"cannot be cast to class org.h2.util.CloseWatcher"` — but that one
was a GC-relocation stale-address reuse (a vacated address getting recycled
for an unrelated object under ZGC compaction), not real classloader
duplication. Before assuming this is the same *kind* of bug under a new
name, both alternate mechanisms from that precedent were checked here and
ruled out:

- **Not GC-relocation-dependent.** The H2 bug was compaction-correlated
  (2/6 with ZGC relocation on, 0/14 with it off). This failure is identical
  on Generational, G1, and ZGC — a GC-timing-dependent stale pointer would
  not produce the same deterministic failure on three collectors with very
  different compaction/relocation behavior.
- **Not a duplicate jar on the classpath.** `spring-boot-restclient`'s own
  `cratonvm-test-cp.txt` carries exactly one `httpclient5-5.6.3.jar` and one
  `httpcore5-5.4.3.jar` — no second copy of `PublicSuffixList` anywhere on
  the flat classpath to shadow the first (unlike the Netty BouncyCastle jar
  ordering bug this session already fixed).

## What all four classes have in common

All four import and use Spring Boot's own test classloader-forking
infrastructure:

- `RestClientObservationAutoConfigurationWithoutMetricsTests` and
  `RestTemplateObservationAutoConfigurationWithoutMetricsTests`:
  `org.springframework.boot.testsupport.classpath.ClassPathExclusions`
- `SecurityFilterAutoConfigurationEarlyInitializationTests`:
  `org.springframework.boot.testsupport.classpath.ClassPathExclusions`
- `MultipartAutoConfigurationTests`:
  `org.springframework.boot.testsupport.classpath.ForkedClassPath`

Both annotations drive Spring Boot's `ModifiedClassPathExtension` / forked
test-classloader machinery — the test body runs under a **child classloader
built with a deliberately modified classpath** (excluding or replacing
specific jars), rather than the harness's normal application classloader.
No other failing or passing class sampled in this run's FAIL set uses this
mechanism.

**Working hypothesis (not yet confirmed at the CratonVM source level):**
something in CratonVM's classloader delegation does not correctly keep
`PublicSuffixList` scoped to one loader across the parent/child classloader
boundary Spring Boot's fork introduces — most likely the child (forked)
loader ends up loading its own copy of a class that should have delegated to
the parent, or vice versa. This has not been traced to a specific
classloading code path; it is inferred from every failing class sharing this
one test-infrastructure feature and no failing or passing class in this run
lacking it having the same crash.

## What is NOT claimed

- The exact CratonVM classloader code responsible has not been identified.
- Whether *only* `PublicSuffixList` is affected, or any class loaded early
  enough in a forked-classpath test would show the same symptom, is
  untested — `PublicSuffixMatcher.<init>` is simply the first place in this
  run's failures that happened to construct one.
- Not cross-checked against HotSpot on this same harness — plausible but
  unconfirmed that HotSpot's classloader delegation handles Spring Boot's
  forked-classpath extension differently (correctly) here.

## Repro

```bash
cd apps/spring-boot-suite-runner
CV_BIN=<binary> ./run-spring-boot-suite.sh -Category all \
  -ClassList <(printf 'org.springframework.boot.servlet.autoconfigure.MultipartAutoConfigurationTests\n')
grep -c 'PublicSuffixList$' <output>/logs/*.out.log
```

Reproduces on all three collectors; not GC-specific, so a single-collector
run is sufficient to confirm.
