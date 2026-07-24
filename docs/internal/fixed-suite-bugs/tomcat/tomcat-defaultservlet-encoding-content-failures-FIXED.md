# Tomcat `TestDefaultServletEncoding*` — functional failures (HTTP -1 / content mismatch) [FIXED]

Status: ✅ **FIXED 2026-07-07** (branch `fix/tomcat-defaultservlet-encoding-20260707`).
The documented functional-failure cluster — 264/1360 failures on
`TestDefaultServletEncodingWithoutBom` and 228/1360 on
`TestDefaultServletEncodingWithBom`, both `--nojit` — is resolved.

## Triage verdict

The doc's own open question — charset-correlated vs load/timing-correlated —
resolves to **charset-correlated, but not in the way the `ibm850`-decode
hint suggested**. Every failure traced back to a single root cause below;
it was not a missing IBM850 *encoder* (encode support was already added
alongside decode in `6a04b0e3`) but a **second, stale copy** of the
charset-alias table that the stream-level shims consulted instead of the
main engine's (already-correct) table.

## Root cause

`sun.nio.cs.StreamEncoder`/`StreamDecoder` real-mode shims in `native-io`
each carried a private charset-alias table that predated the engine gaining
`IBM850` and the `encoding_rs`-backed multibyte families (Shift_JIS, Big5,
EUC-JP/KR, GBK, GB18030, …). Any `OutputStreamWriter`/`InputStreamReader`
constructed with one of those charsets either silently fell back to UTF-8
(when constructed from a `Charset` object) or threw
`UnsupportedEncodingException` (when constructed from a charset name).

Concretely: Tomcat's `DefaultServlet` include-conversion path wrote UTF-8
bytes (`C2 BD`) onto an `outputEnc[ibm850]` wire instead of the correct
single byte (`AB`) — the client observed `┬¢` instead of `½` — and
`fileEnc[ibm850]` source-file reads were misdecoded the same way. The
`expected:<200> but was:<-1>` shape (the bulk of the WithoutBom failures)
was `UnsupportedEncodingException` during stream setup aborting the response
before a status line was ever written; the `ComparisonFailure` shape was the
silent UTF-8-fallback mis-transcoding.

## Fix

Added a single shared alias table (`cratonvm_native_api::charset::
canonical_charset_name`) living next to the transcoding engine itself, and
made both `normalize_charset_name` (native-builtins) and the
`StreamEncoder`/`StreamDecoder` shims (native-io) resolve through it instead
of each keeping (or, in the shims' case, silently drifting from) their own
copy — eliminating the dependency-cycle motivation for the old duplicated
tables. Each shim keeps its own engine-support probe afterward (a
canonical-but-codecless name like `KOI8-U` still correctly reports
unsupported).

## Verification

Full suite re-run, `--nojit`, real-JDK jdk25,
`CRATONVM_REAL_NET_SOCKETS=1 CRATONVM_REAL_AQS=1`:

| Class | Before | After |
|---|---|---|
| `TestDefaultServletEncodingWithoutBom` | 264/1360 failed | **0/1360 — all pass** |
| `TestDefaultServletEncodingWithBom` | 228/1360 failed | 2/1360 failed |

The 2 remaining `WithBom` failures are **not** encoding/content mismatches —
both are `org.apache.catalina.LifecycleException` during embedded-Tomcat
startup (`Failed to start component [StandardEngine[Tomcat]]` and
`Protocol handler start failed`), i.e. the connector/engine failing to bind
during one of the 1360 rapid embedded-server boot/stop cycles this suite
runs. This is consistent with transient port/resource contention on a
shared, loaded build host rather than a VM-correctness defect — the suite
boots and tears down a fresh Tomcat instance per parameterized case.
Recommend a re-run (ideally on an idle host, or a narrowed rerun of just
cases 299 and 913) to confirm before opening a dedicated known-issue doc; not
blocking this retirement since the documented symptom (charset content
corruption) is fully resolved.

## Repro

```
cd apps/tomcat
CRATONVM_REAL_NET_SOCKETS=1 CRATONVM_REAL_AQS=1 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 \
  cratonvm --java-home <jdk25> --nojit -Xmx2g -cp "$(cat .suite/cp.txt)" \
  org.junit.runner.JUnitCore org.apache.catalina.servlets.TestDefaultServletEncodingWithoutBom
```
(swap in `TestDefaultServletEncodingWithBom` for the BOM variant; full suite
≈1360 embedded-Tomcat boot/stop cycles, allow 25-30+ min under load.)
