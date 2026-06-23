# Tomcat full-suite triage — CratonVM `dev` (2026-06-22)

> **Run in progress (incremental).** Worktree `C:/craton/CratonVM-tctest`,
> branch `chore/tomcat-fullsuite-20260622`, binary
> `target/release/cratonvm-tcfull-0622.exe` built from `dev df11ac00`.
> Harness: `.tooling/run-suite.ps1` (parallel 8, 120s/class timeout), all 651
> JUnit classes, JIT-on (default). HotSpot baseline reused: `results/full/hotspot`
> (635 PASS / 14 FAIL / 1 HANG / 1 NOSUMMARY).

## Status / numbers

**✅ COMPLETE — all 651 classes ran.**

| Status | Count (of 651) | HotSpot baseline |
|--------|---------------:|-----------------:|
| PASS | 273 | 635 |
| HANG | 261 | 1 |
| FAIL | 108 | 14 |
| NOSUMMARY | 9 | 1 |

Cross-diff vs the HotSpot `full` baseline: **268 CratonVM-only crashes/hangs/
no-summary** (HotSpot completes them) + **101 divergent FAILs** (CratonVM FAIL,
HotSpot PASS) = **369 classes that pass on HotSpot but not CratonVM** (0 the other
way). Full clustering: [tcfull0622-analysis-FINAL.txt](../.tooling/tcfull0622-analysis-FINAL.txt);
raw CSV: [results-final.csv](../.tooling/results/tcfull0622/craton/results-final.csv).

- **Wall time:** two passes, ~2h05m total wall (8-way parallel; the first pass
  aborted at 497 on a CSV lock and was resumed). Sum of per-class times = 718 min,
  i.e. ~90 min ideal at 8× — the extra is hang-serialization tail. **261 HANGs ×
  120 s each dominate the wall time.**
- **⚠️ Caveat on HANGs:** each hang burns the full 120s under an **8-way parallel**
  load; CPU contention inflates timings (cf. the project rule "never measure VM
  wall-time with other load running"). Some HANGs are genuine deadlocks; others
  are throughput timeouts that would pass when run **serially**. The HANG cluster
  is being re-verified one-by-one (serial, no contention) before any hang gets a
  bug doc. **FAILs are reliable** (deterministic captured stacktraces) and are
  what the bug docs below are built on.

## New bug docs (this run) — triage table

Each links a separate `.md`. **Fix** = bounded VM-side change recommended for me
to do; **Handoff** = deeper subsystem (GC / crypto / connector semantics) better
owned by a specialist.

| # | Bug | Kind | Affected | Rec | Doc |
|---|-----|------|----------|-----|-----|
| 1 | `String.contentEquals(StringBuilder)` SIOOBE — synthetic StringBuilder `getValue()/getCoder()` layout mismatch (BUG-M family) | FAIL | 1+ | **FIX** | [contentequals-bounds](BUG-TC0622-corsfilter-contentequals-bounds.md) |
| 2 | `HttpURLConnection.getHeaderFields()` native missing → null content-type → NPE | FAIL | 8 | ✅ **FIXED** dev `8e44e8c5` | [getheaderfields](BUG-TC0622-addcharsetfilter-contenttype-null.md) |
| 3 | Streaming charset encoder drops split surrogate pairs → U+10000 corrupts to `EF BF BD` ×2 | FAIL | 1+ | **FIX** | [supplementary-encode](BUG-TC0622-outputbuffer-supplementary-char-encode.md) |
| 4 | `HttpURLConnection` real-JDK carrier vs synthetic `HUC_*` slots: write-after-connect + GET-not-POST (405) | FAIL | 3 | ✅ **FIXED** dev `8e44e8c5` | [write-after-connect](BUG-TC0622-httpurlconnection-write-after-connect.md) |
| 5 | JSP runtime compile/load: `org.apache.jsp.*` `ClassNotFoundException` + JSTL TLV superclass not found → 500 on `.jsp` | FAIL/HANG | 8+ | **FIX** | [jsp-compile-load](BUG-TC0622-jsp-pagecontext-contains-npe.md) |
| 6a | Class identity keyed on **name only**, not `(loader,name)` (JVMS §5.3) → ClassFileTransformer weaving ineffective | FAIL | 1+ | **FIX** | [exit1-cluster](BUG-TC0622-process-exit1-cluster.md) |
| 6b | `JSESSIONID` not accepted across contexts (cookie path `/`) | FAIL | 1 | Handoff | [exit1-cluster](BUG-TC0622-process-exit1-cluster.md) |
| 7 | `PBEWithMD5AndDES` SecretKeyFactory (PKCS#5 v1.5 KDF) unimplemented | FAIL | 1+ | **Handoff (crypto)** | [pbe-missing](BUG-TC0622-pbe-secretkeyfactory-missing.md) |
| 8 | `RandomAccessFile` handle-map vs nio `fd_table` are disjoint registries → `FileChannel.map`→`size0` "bad fd" 500 | FAIL | 1+ | **FIX** | [raf-fd-registry](BUG-TC0622-servlet-500-cluster.md) |
| 9 | `HttpURLConnection` silently drops `setRequestProperty` headers → no `Authorization`/`Origin` → 401/403; **also explains the Digest/Basic auth FAIL cluster** | FAIL | 10+ | ✅ **FIXED** dev `8e44e8c5` | [auth-401-403](BUG-TC0622-authenticator-401-403-cluster.md) |

### ✅ High-leverage theme — the `HttpURLConnection` shim (#2, #4, #9) — FIXED & MERGED

Bugs #2, #4 and #9 were three symptoms of **one subsystem**: CratonVM's
`HttpURLConnection` natives (`native-builtins/src/http_url_connection.rs`)
assumed a **synthetic `HUC_*` field layout**, but the test harness is handed a
**real-JDK** `sun.net.www.protocol.http.HttpURLConnection` whose slots don't
match. **Fixed in one change** (branch `fix/huc-real-jdk-carrier`, commit
`7b8f37d1`, merged to `dev` `8e44e8c5` on 2026-06-23): the real-JDK carrier is
now first-class — request method/headers/body and the cached response
(status+headers+body) live in identity-keyed side-tables; `huc_real_perform`
sends them; `getHeaderFields()`/`getRequestProperty()` registered; GET→POST
promoted on `getOutputStream`; every synthetic-slot setter guarded against
clobbering real fields.

**Validated via `run-suite.ps1` (same args/env as the baseline), 42 → 11
individual test-failures across the affected classes, four flipped fully green,
zero regressions:**

| Class | Baseline | After fix |
|-------|---:|---:|
| TestAuthInfoResponseHeaders | 2 FAIL | **PASS** |
| TestInputBuffer | 2 FAIL | **PASS** (9500-byte POST echo) |
| TestCoyoteInputStream | 1 FAIL | **PASS** |
| TestDigestAuthenticator | 17 FAIL | **0 (serial 18/18 PASS)**; 4 under parallel-contention |
| TestAddCharSetFilter | 8 FAIL | 1 (residual = `ISO-8859-3` charset) |
| TestPropertiesRoleMappingListener | 6 FAIL | 2 |
| TestApplicationDispatcher | 3 FAIL | 1 |
| TestAuthenticatorBaseCorsPreflight | 2 FAIL | 1 |
| TestRestCsrfPreventionFilter2 | 2 FAIL | 1 |

Residual partials (CorsPreflight/RestCsrf/PropertiesRoleMapping/Dispatcher) are
separate auth-flow / dispatch edge cases, not the shared root cause. The merged
`dev` `cargo check`s clean and the 22 module unit tests pass. **Note:** the
`net_phase_e.rs` synthetic-carrier path (`java/net/HttpURLConnection` from
`URL.openConnection`) was left unchanged — only the real `sun.net.www` carrier
needed first-class treatment for these tests.

## Newly surfaced on the full run (docs incoming)

The complete run exposed distinct defects the 497-partial didn't reach. Top NEW
clusters by blast radius (from the FINAL clustering):

| New defect | Signature | Kind | Classes | Rec |
|------------|-----------|------|---------|-----|
| **[SSLSession AbstractMethodError](BUG-TC0622-sslsession-buffersize-abstractmethod.md)** | `AbstractMethodError: javax/net/ssl/SSLSession.{getApplicationBufferSize,getPacketBufferSize,getProtocol,getCipherSuite}` has no Code → kills NioEndpoint TLS selector. **⚠️ NOT a missing-registration quick win** — probing shows NO `SSLSession` native dispatches on the `SSLEngine.getSession()` object (it's allocated as the abstract *interface* type); registration attempt built+reverted | HANG/FAIL | 18 | **Handoff (VM dispatch/alloc)** |
| Derby boot failure | `org.apache.derby.shared.common.error.StandardException: XBM01.D` (DB create/boot) | FAIL | 4 | Handoff |
| UDP `DatagramChannel.setSendBufferSize(I)V` missing | linkage error: no such method | NOSUMMARY | 3 | **FIX** |
| GC relocate-OOM | `FATAL OutOfMemoryError: GC could not relocate a live object — young to-space is full` | HANG/CRASH | 2 | Handoff (GC) |
| `cross_thread_jit_gap` multi-thread-in-JIT-under-STW root gap | `scan_active_jit_frames: another thread holds live JIT frames…` | HANG | 3+ | Handoff (GC/JIT) |
| `classpath:` URL protocol unknown / mbeans-descriptors not resolvable | `MalformedURLException: unknown protocol: classpath` | FAIL | 2 | FIX |
| `String.setFilter(...)`/other logger linkage gaps | linkage error: no such method | NOSUMMARY | 1+ | FIX |

### HANG breakdown (261) — ✅ serially re-verified: ~94% are NOT defects

[BUG-TC0622-hang-triage.md](BUG-TC0622-hang-triage.md) re-ran representatives of
every major HANG cluster **serially** (no contention, longer timeout). Result:
**~245 of 261 HANGs (≈94%) are throughput / CPU-contention artifacts, not
deadlocks** — under parallel=8 each Catalina/Coyote/JSP test starts+stops a full
embedded Tomcat per method, so 20–140-method classes blow the 120 s budget while
progressing normally (proof: `TestWebXml` is a 3.8 s unit test mislabeled HANG;
`TestGenerator` EXITS at 204 s/82 tests; `DoHead*` were actively advancing when
killed). **Only ~16 HANGs trace to one genuine defect — the SSLSession TLS
`AbstractMethodError`** (documented below), which presents as HANG under
contention but EXITS as FAILURES serially. No new genuine deadlock was found.
→ Re-running at 2–4 workers and/or ≥300 s timeout would collapse the HANG count
toward those ~16 SSL classes.

## Singletons — now investigated & documented

Each below has its own `.md` with a pinned root cause and fix-vs-handoff call.
Several upgraded from "handoff" to **bounded FIX** once root-caused.

| Defect | Doc | Rec |
|--------|-----|-----|
| JMX MBeans invisible (synthetic MBeanServer `queryNames` returns empty) | [jmx-mbean-registration-missing](BUG-TC0622-jmx-mbean-registration-missing.md) | Handoff (JMX subsystem) |
| JNDIRealm start fails — **not LDAP**; `Hashtable.clone()` casts synthetic entries to `Hashtable$Entry` | [jndirealm-ldap-start-failure](BUG-TC0622-jndirealm-ldap-start-failure.md) | **FIX** (register `Hashtable.clone`) |
| Webapp reload thread-leak — child threads don't inherit parent `contextClassLoader` | [webapp-classloader-timer-thread-leak](BUG-TC0622-webapp-classloader-timer-thread-leak.md) | **FIX** (inherit CCL; +1 sibling) |
| UTF-8 decoder ignores `CodingErrorAction.REPLACE` (malformed seqs error instead of U+FFFD) | [utf8-decoder-malformed-sequences](BUG-TC0622-utf8-decoder-malformed-sequences.md) | **FIX** (`charset.rs`) |
| Webdav bounded `ByteArrayOutputStream` — BAOS intrinsic shadows subclass `write` (BUG-J family) | [remoteip-and-bounded-stream](BUG-TC0622-remoteip-and-bounded-stream.md) §B | **FIX** (gate intrinsic on non-overriding receiver) |
| RemoteIpFilter — netmask msg is a red herring; real cause = loopback peer not matched as internal | [remoteip-and-bounded-stream](BUG-TC0622-remoteip-and-bounded-stream.md) §A | Handoff (InetAddress/loopback) |

## ✅ Fixes landed this session (merged to `dev`)

| Fix | Merge | Effect |
|-----|-------|--------|
| **HttpURLConnection real-JDK carrier** (#2/#4/#9) | `8e44e8c5` | 42→11 test-failures across the affected classes; AuthInfo/InputBuffer/CoyoteInputStream/Digest-serial fully green; range-206 GET case fixed |
| **HEAD response body-read** | `873355f1` | `TestDefaultServletRfc9110Section14` FAIL→**PASS**; all HEAD requests through the shared client unblocked |
| **`jdk.internal.misc.VM.isBooted`** | `873355f1` | JASPIC `ResourcesMgr` `InternalError` eliminated ([doc](BUG-TC0622-vm-isbooted-jaspic.md)); revealed a follow-up `Subject.privCredentials` gap |

**Bounded FIXes documented & ready as the next batch** (not yet implemented):
`Hashtable.clone`, Thread `contextClassLoader` inheritance, UTF-8 decoder REPLACE,
Webdav BAOS intrinsic gating, `Subject.privCredentials` init.

**Reclassified as deeper (handoff):** SSLSession `AbstractMethodError` — the
"register two natives" approach was attempted, built, and **reverted** when
probing proved no `SSLSession` native dispatches on the `SSLEngine.getSession()`
object (a VM dispatch/allocation issue, not a registration gap).

## Reproduce any class

```powershell
cd C:\craton\CratonVM\apps\tomcat
$cp = (Get-Content .tooling\cp.txt -Raw).Trim()
$env:CRATONVM_REAL_NET_SOCKETS=1; $env:CRATONVM_REAL_AQS=1; $env:CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
C:\craton\CratonVM-tctest\target\release\cratonvm-tcfull-0622.exe -Xmx2g -cp $cp `
  org.junit.runner.JUnitCore <FQCN>
```
