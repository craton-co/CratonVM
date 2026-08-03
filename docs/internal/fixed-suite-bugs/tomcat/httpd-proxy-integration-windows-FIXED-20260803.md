# httpd reverse-proxy integration on Windows — 9 classes — FIXED 2026-08-03

| | |
|---|---|
| **Status** | ✅ **FIXED** — two fixture gaps *and* one real CratonVM defect |
| **Original verdict** | "Fixture gap, NOT a CratonVM bug" — **wrong**; see below |
| **Filed** | 2026-08-03, rerunning the 07-31 4-shard FAIL/HANG set after merging `dev` (`c1fe51a24`) |
| **Closed** | 2026-08-03, branch `fix/tomcat-httpd-proxy-windows-20260803` |

## The original verdict was wrong

The doc filed this family as a pure fixture gap on the strength of "HotSpot
fails identically". The observation was correct but not conclusive: **with no
httpd on the box, both VMs fail before any VM-specific code runs**, so a shared
red says nothing about what happens once httpd is reachable. Standing the
fixture up turned the family into 9 HotSpot PASSes against 9 CratonVM FAILs, all
on one CratonVM defect the fixture gap had been hiding for as long as the family
had been red.

Worth generalising: a both-VMs-red family only proves "not a CratonVM bug" when
the *shared* failure is the last one. Closing the fixture gap is how you find
out — not reasoning about it.

## Symptom (as filed)

7 classes with a client-side `ConnectException` to a loopback port:

```
1) testBasicProxying(org.apache.tomcat.integration.httpd.TestBasicProxy)
java.net.ConnectException: connect [::1]:54501: ... (os error 10061)
```

Same for `TestFullReverseProxy`, `TestLargePayloadWithProxy`,
`TestRemoteIpValveWithProxy`, `TestSessionWithProxy`, `TestSSLValveWithProxy01`
and `TestSSLValveWithProxy02`. `TestErrorHandling` — a 9th class in the family
that the original doc did not list — fails the same way.

## Three separate causes

### 1. Fixture gap — no httpd on the Windows box

Apache publishes no Windows binaries and nothing on this host provided one.
`TesterHttpd` looks for a literal `httpd` on `PATH` unless
`-Dtomcat.test.httpd.path` names one. This is the Windows analogue of the
Linux/Azure gap in `docs/internal/tomcat/missing-httpd-binary.md`.

**Fixed by** `apps/tomcat-suite-runner/setup-httpd-windows.ps1`: downloads the
Apache Lounge VS17 build (`httpd-2.4.66-251206-Win64-VS17.zip`), verifies its
published SHA-256, and unpacks it to `C:\craton\tools\Apache24`. No service, no
registry entry, no `PATH` change — deleting the directory undoes it. VS17 rather
than the newer VS18 build because VS17 links the VC++ 2015-2022 runtime already
present on this host.

Module paths in the generated per-test configs are relative
(`modules/mod_proxy.so`); httpd on Windows resolves them against the directory
above its own `.exe`, so no `ServerRoot` is needed and the tree works unpacked
anywhere. The family loads `mod_authz_core`, `mod_env`, `mod_headers`,
`mod_proxy`, `mod_proxy_http` and `mod_ssl`, all present in that build — the
setup script smoke-tests exactly that set with `httpd -t` so a bad unpack or a
missing VC++ runtime surfaces there rather than as 9 opaque test failures.

`-Dtomcat.test.httpd.path` is now passed by `run-tomcat-suite.ps1`,
`run-tomcat-suite.sh` (via `HTTPD_PATH`, defaulting to `command -v httpd`) and
`run-one.ps1`. It is inert for every other class.

### 2. Fixture gap — upstream's 1000 ms readiness deadline is short on Windows

With httpd installed, all 9 classes still failed, ~1 s in:

```
java.lang.IllegalStateException: Httpd has not been started.
	at org.apache.tomcat.integration.httpd.TesterHttpd.isHttpdReady(TesterHttpd.java:145)
```

`TesterHttpd.isHttpdReady()` hard-codes a 1000 ms deadline for httpd to bind its
listener. Measured on this host over three runs: **1.13 s, 1.53 s, 1.02 s** —
MPM WinNT startup (process creation, module init, "Child: Starting 64 worker
threads") costs more than a second, every time. On Linux httpd binds in tens of
milliseconds, which is why upstream never noticed.

**Fixed by** `apps/tomcat-suite-runner/fixtures/httpd-ready-timeout.patch`,
applied (idempotently) by the setup script: default 30 s, overridable with
`-Dtomcat.test.httpd.readyTimeoutMs`, and the poll loop now aborts early with
the child's exit code if httpd dies instead of waiting out the deadline and
reporting a timeout. It ships as a patch file because `apps/tomcat` is not
git-tracked.

*This one is an upstream Tomcat bug, not a CratonVM one, and is worth sending
upstream.*

### 3. Real CratonVM defect — the synthetic Process aliases `java.lang.Process`'s own fields

With httpd up, HotSpot went 9/9 green and CratonVM 9/9 red — every class on the
identical exception:

```
java.lang.NullPointerException: Cannot invoke "java.nio.charset.Charset.equals(Object)"
        because "this.inputCharset" is null
        at java.lang.Process.inputReader(Process.java:338)
        at org.apache.tomcat.integration.httpd.TesterHttpd.start(TesterHttpd.java:87)
```

`ProcessBuilder.start()` hands back a `cratonvm/synthetic/Process`. That class is
linked to `java/lang/Process` as a **supertype** (in
`class_manager::jdk_interfaces`, so `checkcast`/`instanceof` stay honest) but
never as a **superclass** — deliberately, because `java.lang.Process` is not
field-less and a superclass link would have aliased its fields onto the synthetic
layout.

The aliasing happened anyway. Since JDK 17 `java.lang.Process` declares six
instance fields — `outputWriter`, `outputCharset`, `inputReader`, `inputCharset`,
`errorReader`, `errorCharset` — the caches behind the **final concrete** methods
`inputReader()`, `errorReader()` and `outputWriter()`. Those methods are real JDK
bytecode and run against whatever receiver they are handed. An instance field
resolves to an *absolute* slot (superclass field count + declaration index), and
`java.lang.Process` extends `Object`, so those six are slots 0..5 of the
receiver — and the synthetic Process kept its own state (exit code, three pipe
fds, pid, handle) at exactly those slots.

So `p.inputReader()` read the stdout pipe fd as `inputReader`, saw a non-null
value, took the "reader already created" branch, and dereferenced the still-null
`inputCharset`. `outputWriter()` failed the same way one field pair over.
`TesterHttpd.start` pumps httpd's stdout and stderr through `p.inputReader()` /
`p.errorReader()`, which is why the whole family died in `setUp`.

**Fixed by** reserving slots 0..5 on both synthetic Process layouts —
`native-io::process` (the live one) and `native-builtins::phases_late` (the
superseded one) — and moving their own state to 6.. Both had to shift together:
`legacy_captured_stream` reads the `phases_late` layout's stdout/stderr string
slots through `native-io`'s own field constants, an alias that only holds while
both layouts start at the same offset.

Regression witness: `apps/tomcat-suite-runner/probes/ProcessReaderProbe.java` —
10 checks over `inputReader`/`errorReader`/`outputWriter` plus the synthetic
Process's own `waitFor`/`pid`/`isAlive`, so a future re-aliasing shows up
without any Tomcat fixture. 10/10 FAIL before, 10/10 PASS after, 10/10 PASS on
HotSpot.

## The `TestChunkedTransferEncodingWithProxy` OOM was not a defect either

The original doc flagged CratonVM's differing failure shape for that one class —
`OutOfMemoryError: Java heap space (native primitive array of length
1048576000)` — as worth a follow-up. It isn't a bogus length: the test's own
`PAYLOAD_SIZE = 10 * 1024 * 1024 * 100` **is** 1 048 576 000, and
`TomcatBaseTest.postUrl` needs a second buffer the same size. HotSpot fits both
in the suite default `-Xmx2g` (26.6 s measured); CratonVM's default Generational
collector caps old-gen at `Xmx/2`, so a 1 GiB humongous array cannot land.

This is the known fixed-split limitation, not a new defect, and it gets the same
treatment as the `*LargeHeap` family: both runners now give this one class 4g
(110 s measured). `-Xmx2g -XX:+UseG1GC` also passes — CratonVM's own G1 has no
fixed split — but takes 275 s, close enough to the 300 s default timeout to
score as a HANG, so the heap bump is preferred.

## Verification

Windows 11 host, `apps/tomcat` fixture, one process per class, serial (the family
serialises on `test/org/apache/tomcat/integration/httpd/httpd-binary.lock`).
HotSpot = Adoptium JDK 25.0.3.9. CratonVM = `cratonvm-httpdproxy-20260803.exe`
(release, JIT on, real JDK) with the suite's four `CRATONVM_*` variables.
"before" = the same binary built from the parent commit.

| Class | HotSpot | CratonVM before | CratonVM after, JIT | CratonVM after, `--nojit` |
|---|---|---|---|---|
| TestBasicProxy | PASS 9.0 s | FAIL — NPE | **PASS 18.9 s** | **PASS 16.5 s** |
| TestChunkedTransferEncodingWithProxy | PASS 26.6 s @2g | FAIL — NPE | **PASS 110 s @4g** (OOM @2g, see above) | **PASS 264 s @4g** |
| TestErrorHandling | PASS 8.0 s | FAIL — NPE ×2 | **PASS 17.9 s** | **PASS 22.4 s** |
| TestFullReverseProxy | PASS 11.0 s | FAIL — NPE | **PASS 13.6 s** | **PASS 18.9 s** |
| TestLargePayloadWithProxy | PASS 8.3 s | FAIL — NPE ×2 | **PASS 19.8 s** | **PASS 28.1 s** |
| TestRemoteIpValveWithProxy | PASS 4.4 s | FAIL — NPE | **PASS 12.0 s** | **PASS 15.8 s** |
| TestSSLValveWithProxy01 | PASS 4.7 s | FAIL — NPE | **PASS 10.4 s** | **PASS 18.0 s** |
| TestSSLValveWithProxy02 | PASS 5.0 s | FAIL — NPE | **PASS 10.3 s** | **PASS 19.7 s** |
| TestSessionWithProxy | PASS 9.0 s | FAIL — NPE ×2 | **PASS 15.4 s** | **PASS 22.4 s** |

**9/9 PASS on both VMs, JIT on and off.**

CratonVM's wall times run 2-3x HotSpot's. That is the known embedded-server
throughput wall (group 04 / 30), not anything this family introduced — every
class is well inside the 300 s suite timeout except the chunked one under
`--nojit`, which the runner's per-class rule already covers.

Rust-side regression check: `cargo test --release -p cratonvm-native-io` — 378
passed, 0 failed; `-p cratonvm-native-builtins` — 3250 passed, 0 failed across
its five test binaries. (That second run's `--doc` phase fails to link against
four unrelated crates. Pre-existing: the doctest is in `net_phase_e.rs`, last
touched 2026-08-02 by an unrelated commit, and this change adds no doctest at
all — its only fenced block is a `text` block.)

Re-verified end to end on the post-merge tree (`e1ecbcf30`, which picked up
another session's JIT/OSR work): 9/9 PASS again, probe 10/10.

## Reproducing

```powershell
pwsh apps\tomcat-suite-runner\setup-httpd-windows.ps1
pwsh apps\tomcat-suite-runner\run-one.ps1 -Vm craton -Exe <exe> `
     -Class org.apache.tomcat.integration.httpd.TestBasicProxy
```

The Process-field regression on its own, with no Tomcat fixture at all:

```powershell
javac -d <dir> apps\tomcat-suite-runner\probes\ProcessReaderProbe.java
<exe> -cp <dir> ProcessReaderProbe          # CratonVM
java  -cp <dir> ProcessReaderProbe          # HotSpot control
```
