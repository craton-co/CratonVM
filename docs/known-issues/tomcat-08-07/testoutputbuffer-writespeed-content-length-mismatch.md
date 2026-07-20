# TestOutputBuffer.testWriteSpeed — deterministic content-length mismatch (OPEN)

**Status: OPEN 2026-07-19.** Found opportunistically while sweeping the
Tomcat connector package to check for RBC.6-related throughput residuals
(see `docs/feature-designs/jit-local-exception-handlers.md`). This is a
**separate, unrelated bug** — flagged here for follow-up, not investigated
to a root cause in this session.

## Symptom

`org.apache.catalina.connector.TestOutputBuffer.testWriteSpeed` fails
deterministically (reproduced 3/3 runs, both in isolation and as part of a
larger batch) with:

```
java.lang.AssertionError: expected:<100000> but was:<382000>
	at org.apache.catalina.connector.TestOutputBuffer.testWriteSpeed(TestOutputBuffer.java:64)
```

The test registers six `WritingServlet(i)` instances for `i` in
`{1, 10, 100, 1000, 10000, 100000}` (each divides `100000` evenly), each
configured to write `writeCount = 100000 / i` copies of an `i`-char string
via `java.io.Writer.write(String)` — so every servlet's total response body
is exactly 100000 bytes by construction, both with and without an extra
`BufferedWriter` wrapper (`useBuffer` request param). The test asserts
`ByteChunk.getLength() == 100000` after each of the twelve requests (six
`i` values × plain/buffered). One of these iterations returns `382000`
instead — a content-length of roughly **3.82×** the expected size, with no
server-side exception logged.

`382000` is not a clean multiple or sum of any subset of the per-iteration
totals (all of which are 100000), so it isn't simple duplicate-request or
leaked-prior-response concatenation in any obvious way. Not yet
determined which of the twelve requests actually produces the bad reading
(the harness stops at first failure).

## Why this is NOT RBC.6

`WritingServlet.doGet()` (decompiled via `javap`) has **no exception
table** — no try/catch anywhere in the method or in `testWriteSpeed`
itself. RBC.6's gate relaxation and the new
`local_handler_reads_unsafe_local` / `handler_has_unsafe_local_read`
safety checks only apply to methods combining `athrow` with a non-empty
exception table; they cannot fire on this code path. Confirmed the
`vmfix-rbc6-sweep-20260719` binary (dev tip as of 2026-07-19, includes all
RBC.6 session-1/session-2 fixes) reproduces this — but that does not
implicate RBC.6 specifically, since no isolation against a pre-RBC6
baseline binary was performed (see Next steps).

## Repro

```bash
cd /data/data/apps/tomcat
CP=$(cat .suite/cp-linux-fixed.txt)
timeout 120 <vm-binary> --java-home /home/victor/jdk25 -Xmx2g -cp "$CP" \
  org.junit.runner.JUnitCore org.apache.catalina.connector.TestOutputBuffer
```

Fully deterministic: `expected:<100000> but was:<382000>` on every run
observed so far.

## Next steps (not done this session)

- Build a pre-RBC6 (or otherwise older) `dev` baseline binary and confirm
  whether this is a pre-existing bug or a same-day regression from
  unrelated concurrent work landing on `dev` — several other sessions
  landed fixes to native IO / Writer / GC-root families on 2026-07-19 (see
  `MEMORY.md`'s "Natives" and "GC / roots" sections), any of which could be
  the actual cause.
- Determine which of the twelve requests actually mismatches (bisect via a
  filtered single-parameterization JUnit driver, same technique as
  `docs/internal/fixed-suite-bugs/dohead-streamencoder-eager-flush-commit-threshold-FIXED.md`'s repro section).
- Check whether `PrintWriter`/`BufferedWriter` buffering (native-io
  `stream_encoder.rs` side-table, same family as the linked doc) is
  involved, or whether this is instead a `writeCount` field read / loop
  bound issue.
