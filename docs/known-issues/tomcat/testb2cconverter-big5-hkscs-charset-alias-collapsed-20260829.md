# `TestB2CConverter.testLeftoverSize` — `Big5-HKSCS` charset alias collapsed to plain `Big5`

## Status
New finding, 2026-08-29. Not previously tracked: `apps/tomcat-suite-runner/baseline.tsv`
lists `org.apache.tomcat.util.buf.TestB2CConverter` as `PASS`. Reproduced on a
quiet single-shard host (0.6s, not contention). Root cause identified in
CratonVM's own charset alias table; not yet fixed.

## Symptom

```
1) testLeftoverSize(org.apache.tomcat.util.buf.TestB2CConverter)
java.lang.ExceptionInInitializerError
	at sun.nio.cs.ext.Big5_HKSCS.newEncoder(Big5_HKSCS.java:58)
	at org.apache.tomcat.util.buf.TestB2CConverter.testLeftoverSize(TestB2CConverter.java:82)
```

The test explicitly exercises the JDK's extended `Big5_HKSCS` charset
(`sun.nio.cs.ext`), which fails its own class initializer under CratonVM.

## Root cause

`native-api/src/charset.rs:133` — CratonVM's own charset-name normalization
table:
```rust
"BIG5" | "CSBIG5" | "BIG5HKSCS" => "Big5",
```
`BIG5HKSCS` (the alias for `Big5-HKSCS`) is folded into plain `"Big5"`,
discarding the HKSCS (Hong Kong Supplementary Character Set) extension
entirely. Whatever CratonVM's charset engine does with that collapsed name is
consistent for lookups that go through CratonVM's own alias resolution — but
this test path reaches the **JDK's real `sun.nio.cs.ext.Big5_HKSCS` class**
directly (`newEncoder()`), which has its own static-init path presumably
depending on data/tables CratonVM's collapsed alias never provisions for, and
that static initializer throws.

## Not yet done
- Confirm exactly what `Big5_HKSCS`'s class initializer needs that isn't
  present — likely a native/table resource CratonVM's charset backend keys
  by the (lost) `HKSCS` distinction.
- Decide whether the fix is to stop collapsing `BIG5HKSCS` → `Big5` in the
  alias table, to add a real `Big5-HKSCS` backend, or both.
- Check for sibling extended-charset classes with the same collapse pattern
  (the alias table at `native-api/src/charset.rs` has other multi-alias rows
  worth auditing for the same kind of lossy fold).

## Repro

```bash
cd apps/tomcat-suite-runner
cratonvm.exe --java-home <jdk25> -XX:+UseZGC <tomcat classpath+args> \
  JUnitRunner org.apache.tomcat.util.buf.TestB2CConverter
```
