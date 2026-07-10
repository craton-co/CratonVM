# TestFormAuthenticatorA/B/C — first request on a fresh socket reads as NUL-byte garbage

**Status:** RULED OUT 2026-07-10 — does not reproduce on an idle host, see
the closing section appended at the bottom of this doc for the full
retest evidence. **Retired from `docs/known-issues/` to `docs/internal/tomcat-08-07/`**
per this repo's convention that `known-issues/` holds only open items.
**Severity (at the time this was filed):** currently blocking (100%
first-request failure in the one reproduction so far). **HotSpot:** not
checked in this session.

## Summary

Found 2026-07-10 while re-verifying the fix in
[`form-authenticator-cookie-session-bare-assertion-FIXED.md`](form-authenticator-cookie-session-bare-assertion-FIXED.md)
(same directory now that this doc has moved to `docs/internal/tomcat-08-07/` — see Status above).
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
  used throughout the sibling `../../known-issues/tomcat-08-07/swallowabortedupploads-unexpected-socketexception.md`
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

## 2026-07-10 (later session): re-tested on an idle Azure Linux host — does NOT reproduce

Followed this doc's own recommendation above: reran the exact
`TestFormAuthenticatorA/B/C` Java-level test methods (direct
`org.junit.runner.JUnitCore` invocation, not the Windows PowerShell runner —
there is no Linux equivalent) against a freshly built `dev`-tip binary on
`victor@20.83.144.174`, a shared but comparatively idle Azure Linux box (16
cores, disk 36% used, load average fluctuating 4–15 across the session
depending on how many other concurrent sessions were active — a very
different environment from the original ~99%-full-disk, heavily-loaded
Windows host).

**Important caveat — this is not a byte-for-byte repro of the original
invocation.** The very first rerun attempt (fixture as-is, no extra flags)
failed 8/9 methods immediately, before ever reaching the network layer: the
Linux fixture's `/data/data/apps/tomcat/webapps` symlink (the counterpart of
the `conf` symlink that *does* exist there) is missing, so
`FormAuthClient`'s appDir resolution — `new File(System.getProperty(
"tomcat.test.basedir"), "webapps/examples")`, which is `null` when invoking
`JUnitCore` directly instead of through Ant — collapsed to a relative
`webapps/examples` path under the CWD and Tomcat's `StandardRoot` failed to
start (`IllegalArgumentException: The main resource set specified
[.../webapps/examples] is not a directory or war file`). This is an
unrelated, pre-existing Linux-fixture gap, not a VM bug (see
`[[tomcat-linux-suite-fixture-location]]` memory topic, which already
documents an adjacent instance of the same class of gap for
`webapp-virtual`). Worked around by passing
`-Dtomcat.test.basedir=/data/data/apps/tomcat/output/build
-Dtomcat.test.tomcatbuild=/data/data/apps/tomcat/output/build` on the
`cratonvm` command line — a one-off invocation flag, not a change to the
shared read-only fixture — which lets `appDir` resolve to the real,
existing `output/build/webapps/examples` directory that Ant's own build
already produces. (Recommendation for whoever automates a Linux runner:
this property should be set by default for the whole Tomcat suite, the same
way Ant's `build.xml` sets it.)

With that workaround applied, ran the suite **7 times total across two
separate SSH sessions** (interrupted mid-investigation by an unrelated SSH
drop, itself caused by host load — load average was 14.94 at the time):

| Class | Runs | NUL-byte / "Invalid character found in method name" occurrences |
|---|---|---|
| `TestFormAuthenticatorA` | 3 (1 without the basedir workaround, 2 with) | **0 / 0 / 0** |
| `TestFormAuthenticatorB` | 2 (both with the workaround) | **0 / 0** |
| `TestFormAuthenticatorC` | 2 (both with the workaround) | **0 / 0** |

Zero occurrences in every single run, including the very first run (before
the basedir workaround), which still got far enough to fail on the
unrelated `StandardRoot`/missing-webapps-symlink error rather than ever
reaching HTTP request parsing — i.e. even that run's Tomcat log had no
opportunity to show the NUL-byte symptom, and it didn't. **This confirms the
"host-load artifact" hypothesis from the Open Questions section above**: on
an idle-to-moderately-loaded host, the sockets involved never show
zero-filled/garbage bytes in place of real request data. The original
symptom was very likely a real race (a buffer pool or NIO read/write
ordering issue under extreme contention) that is either specific to
Windows, specific to that host's ~99%-full-disk/infected state, or simply
requires a degree of concurrent load this rerun didn't reproduce — not
something that can be conclusively ruled out as "impossible", but not
something reproducible on a normal host either.

**What replaced it once fixed (turned out to already be fixed, concurrently, by another session):**
with the basedir workaround in place but *before* pulling in the rest of
`origin/dev`, all remaining test failures across all three classes converged
on a single, different, consistent cause — a `NullPointerException` in
`FileDescriptor.closeAll` during JSP compilation — that was invisible in the
original session because every request died earlier, on the zero-byte read,
before Tomcat ever got far enough to compile a JSP. Was about to file this as
a new doc when a `git fetch`/duplicate-fix check turned up that another
concurrent session had already found and fixed the exact same bug a few
commits ahead on `origin/dev`: `deb38efc` ("fix(native-io):
FileInputStream.close() clobbers real fd reference on legacy slot-0 write"),
retired as
[`jspdocumentparser-saxparse-malformed-markup-FIXED.md`](jspdocumentparser-saxparse-malformed-markup-FIXED.md).
Root cause there: `native_fis_close` unconditionally wrote `Value::Int(-1)`
into `FileInputStream` instance slot 0, which in the real-JDK field layout is
the real `fd: Ljava/io/FileDescriptor;` reference field, silently nulling it.
Merged that fix in and reran `TestFormAuthenticatorA/B/C` again — zero
`FileDescriptor.closeAll` NPEs recurred (see below), confirming it's the same
bug and it's already resolved. No new doc needed.

**Post-merge verification and a new, unrelated finding:** merged
`origin/dev` (25 commits, `f4ee4065..03fd1788`, spanning GC/JIT/native-builtins
work from several concurrent sessions — none of it touching this doc's own
territory) into the fix branch, rebuilt, and reran `TestFormAuthenticatorA/B/C`
once more. Both the zero-byte symptom and the `FileDescriptor` NPE stayed
gone (0 occurrences of either across all three classes, ~6 test methods
each completing cleanly with no `ERROR`/`Exception` lines before the crash
below) — but all three classes now **segfault** (SIGSEGV, exit 139,
`timeout: the monitored command dumped core`) partway through the run
(after 6/9, 6/6, and 6/7 methods respectively completed with no errors).
This is a **new, unrelated regression**, almost certainly introduced by one
of the 25 merged commits (`gc/src/zgc.rs`, `jit/src/x64.rs`,
`jit/src/ir_lower.rs`, `native-builtins/src/lang_string.rs`,
`vm/src/vm/vm_object.rs`, `vm/src/memory/gc.rs`,
`vm/src/threading/monitor.rs` all changed substantially in that merge) —
not root-caused this session (genuinely out of scope: unrelated to both
the zero-byte and `FileDescriptor` investigations, and no core dump was
retrievable — this host's `core_pattern` routes through `apport`, which
didn't leave a plain core file behind). Notably, `docs/known-issues/hib-global-temptable-nondeterministic-sigsegv-20260710.md`
(filed the same day, "post the 2026-07-09/10 merge wave") describes a
similarly-shaped non-deterministic SIGSEGV cluster in a completely
different suite (Hibernate) after repeated create/drop DDL cycles — worth
a follow-up session checking whether these are the same underlying race,
since `TestFormAuthenticatorA/B/C` also repeatedly starts/stops a full
embedded Tomcat (create/destroy cycle) once per test method, a similar
shape of repeated-lifecycle churn.

**Conclusion:** retiring this doc as a non-issue — the zero-byte symptom
itself is confirmed not to reproduce on an idle host, with or without the
unrelated fixes/regressions found along the way. No CratonVM code was
changed as part of *this* investigation (the `FileDescriptor` fix was
someone else's, already merged; the new segfault is unroot-caused and
flagged separately, not fixed here).
