# TestFormAuthenticatorA/B/C — first request on a fresh socket reads as NUL-byte garbage

**Status:** OPEN, not investigated. **Severity:** currently blocking (100%
first-request failure in the one reproduction so far). **HotSpot:** not
checked in this session.

## Summary

Found 2026-07-10 while re-verifying the fix in
[`form-authenticator-cookie-session-bare-assertion-FIXED.md`](../../internal/tomcat-08-07/form-authenticator-cookie-session-bare-assertion-FIXED.md).
Every method of `org.apache.catalina.authenticator.TestFormAuthenticatorA`,
`TestFormAuthenticatorB`, `TestFormAuthenticatorC` fails its first request —
`Assert.assertTrue(client.isResponse200())`, the very first unauthenticated
GET — because the server never returns a real response. The server-side log
shows:

```
INFO [org.apache.coyote.http11.Http11Processor] Error parsing HTTP request header
 Note: further occurrences of HTTP request parsing errors will be logged at DEBUG level. (java/lang/IllegalArgumentException: Invalid character found in method name [0x000x000x00...]. HTTP method names must be tokens)
```

— a few hundred NUL (`0x00`) bytes in place of an HTTP method line
(`GET /examples/... HTTP/1.1`). This happens on a **freshly accepted socket**
on a **freshly initialized `Http11NioProtocol`** (each test method starts its
own embedded Tomcat on an auto-assigned port — this is not a stale/reused
keep-alive connection carrying over garbage from a prior test). It reproduced
on every single test method across all three classes (8, 6, and 7 occurrences
respectively, matching the method count).

## Not caused by the `StreamDecoder` field-index fix

Confirmed via a same-`dev`-tip baseline comparison, run back-to-back on the
same host:

| Binary | dev tip | `TestFormAuthenticatorA/B/C` result |
|---|---|---|
| Unmodified `dev` (`CratonVM-jspdocparser-sax-20260710-001`) | `36359f2cb` | **HANG** × 3 (120s timeout every class) |
| `StreamDecoder` field-index fix (`e5f20c9bf`, branched from `98f428a27`, a strict descendant of `36359f2cb`) | `98f428a27` + fix | **FAIL** × 3 (203s/162s/180s — completed, no hang, but same zero-byte symptom on every method) |

Both binaries hit the identical NUL-byte symptom; the unmodified baseline
fared *worse* (hung instead of completing). This rules out the
`StreamDecoder` fix as the cause — it is a pre-existing condition on current
`dev`, not a regression introduced by that fix.

## Open questions — not investigated this session

- **Host-load artifact vs. real VM bug.** This session's host
  (`C:\craton\CratonVM`, Windows) was under heavy concurrent load throughout
  (multiple simultaneous `cargo build`/`cratonvm.exe` processes from other
  sessions, disk at ~99% full, and a separately-reported elevated-CPU
  infection on this same box) when this was found. NUL bytes
  arriving in place of real request data is a stronger signal than typical
  "just slow" flakiness (a `HANG`/timeout would be the expected shape of pure
  CPU starvation; a deterministic zero-filled buffer instead of client data
  looks more like a genuine race — e.g. a buffer pool handing out an
  unwritten/zeroed buffer under contention, or a read landing before a write
  completes). Not confirmed either way — needs a rerun on an idle host before
  concluding it's a real, always-reproducible VM bug rather than an artifact
  of this specific overloaded moment.
- **Is this Tomcat's NIO connector, CratonVM's socket layer, or the test
  client?** Not narrowed at all. `SimpleHttpClient`/`FormAuthClient` (the
  test's own raw-socket HTTP client) is one candidate (never verified it
  actually wrote real bytes rather than a zeroed buffer); CratonVM's
  `native-io/src/socket_channel.rs` NIO accept/read path is another.
- No stack trace/exception was captured beyond Tomcat's own one-line JULI log
  of the parse failure — root-causing this will need the same
  "recompile with a temporary instrumented copy on the classpath" technique
  used throughout the sibling `swallowabortedupploads-unexpected-socketexception.md`
  and `form-authenticator-cookie-session-bare-assertion-FIXED.md` investigations,
  applied to `Http11InputBuffer`/the NIO accept path instead.

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName <name> `
  -Start 113 -Count 3 -TimeoutSec 240 -Parallel 1 -Exe <any recent dev-tip cratonvm.exe>
```
(`-Start 113 -Count 3` selects `TestFormAuthenticatorA/B/C` in
`apps\tomcat\.suite\all-tests.txt` as of 2026-07-10; re-verify the index if
the class list has been regenerated since.) Grep any class's
`.log.err` for `Invalid character found in method name` to confirm.

## Recommendation

Re-run on an idle host first (see the "host-load artifact" question above) —
if it clears up with no other changes, this was contention/disk-pressure on a
shared box, not a VM bug, and this doc can close as a non-issue. If it
persists on an idle host, root-cause via instrumentation of
`Http11InputBuffer`'s fill path and CratonVM's NIO accept/read path, per the
methodology in the sibling docs referenced above.
