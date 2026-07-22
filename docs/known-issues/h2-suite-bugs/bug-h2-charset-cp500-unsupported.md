# H2 — `Unsupported charset: cp500` / `Unsupported charset: CP500` (extended `jdk.charsets` provider missing)

## Status
**OPEN** — rediscovery of a previously-documented issue. The original doc
(`docs/known-issues/h2-suite-bugs/bug-h2-charset-cp500-unsupported.md`) was
deleted on `dev` by `b71e7402f` ("docs refactor remove stale bugs",
2026-06-22) as part of a bulk documentation cleanup, **not because the gap
was fixed** — it reproduces byte-for-byte identically today. Recreated here
after confirming it's still live against current `dev` (`origin/dev @
b21de8501`, 2026-07-21).

## Severity
**MEDIUM** — any code that requests an IBM/EBCDIC code-page charset
(`cp500`/`IBM500` and likely the rest of the `sun.nio.cs.ext` family: other
EBCDIC pages, ISO-2022, GB18030, etc.) fails; two H2 suite classes hit it.

## Affected test classes (h2database-suite-runner, `jit-real`, real-JDK25)
- `org.h2.test.db.TestSetCollation` (`testCp500Collator`) —
  `SET COLLATION CP500` throws `UnsupportedCharsetException: CP500` deep in
  `CompareMode.getCollator`.
- `org.h2.test.unit.TestCharsetCollator` — constructs a collator directly
  against `cp500`; same underlying gap.

Both PASS on the HotSpot JDK25 baseline (SunJCE/`jdk.charsets` module
provides `cp500` there).

## Symptom
```
org.h2.jdbc.JdbcSQLNonTransientException: General error:
  "java.nio.charset.UnsupportedCharsetException: CP500"
	at org/h2/value/CompareMode.getCollator(CompareMode.java:208)
	at org/h2/command/Parser.parseSetCollation(Parser.java:7744)
	...
```
(`TestCharsetCollator` throws the same `UnsupportedCharsetException` — or,
per the original doc, an `ExceptionInInitializerError` wrapping it — directly
from `Charset.forName("cp500")` at construction time.)

## Root cause
CratonVM's `Charset.forName`/provider-lookup path only resolves the
`java.base` standard charset set (`UTF-8`, `ISO-8859-1`, `US-ASCII`,
`UTF-16*`, `windows-1252`, etc.). The extended charsets the real JDK ships in
the **`jdk.charsets`** module (`sun.nio.cs.ext.*` — IBM/EBCDIC code pages
including `cp500`/`IBM500`, the ISO-2022 family, GB18030, Big5-HKSCS, …) are
never registered with CratonVM's charset provider, so `Charset.forName`
throws `UnsupportedCharsetException` for any of them.

## Verification (2026-07-21 rerun)
Standalone repro, no H2 involved:
```java
java.nio.charset.Charset.forName("cp500");
```
throws `UnsupportedCharsetException: cp500` under CratonVM
(`cratonvm-h2-fail-triage-20260721 --java-home /home/victor/jdk25`), and
succeeds (`IBM500`) on the HotSpot JDK25 baseline. `--nojit` behaves
identically — this is a provider-registration gap, not a JIT bug.

## Fix options (unchanged from the original doc)
- Register the extended `sun.nio.cs.ext` charset provider (or at minimum a
  curated subset covering the IBM/EBCDIC code pages H2's test suite and
  common enterprise workloads use) so `Charset.forName` resolves them.
- Until then, the checked `UnsupportedCharsetException` the JDK contract
  promises is already what's thrown (this part is correct) — the gap is
  purely "charset not available", not an incorrect exception type.

## Repro
```bash
cd apps/h2database-suite-runner && ./run-h2-suite.sh setup   # once
cd ../h2database/h2
<cratonvm-bin> --java-home /home/victor/jdk25 \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.unit.TestCharsetCollator
```
or simply `Charset.forName("cp500")` in any standalone program.
