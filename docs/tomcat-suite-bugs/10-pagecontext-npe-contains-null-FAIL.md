# Bug 10 — TestPageContext "contains on null": a six-layer onion — ✅ FIXED (OK (1 test))

**Status: FULLY FIXED — `TestPageContext.testBug49196` passes (`OK (1 test)`).**
The original `PageContext`/EL hypothesis was WRONG. `res.toString()` was null
because the JSP produced an empty body, which peeled back through six layers:
HTTP serving → the in-process HTTP client → JDK resource loading → the Eclipse
JDT compiler → a **fundamental VM bug in `Unsafe`/`Arrays.equals`** → and finally
a **synthetic `java.io.CharArrayWriter` shadow** that swallowed every JSP body.

| # | Layer | Root cause | Fix |
|---|---|---|---|
| 1 | NIO connector reset | `SocketChannel.setOption` AbstractMethodError (covariant desc) | `socket_channel.rs` (merged) |
| 2 | in-process HTTP client | native HUC read a real `sun.net.www` obj w/ synthetic layout → code -1 | `48183599` (merged) |
| 3 | boot-class resource load | `getResourceAsStream("java/lang/String.class")` null (jmod `classes/` prefix in `URL.openStream`) | `net_phase_e.rs` + jrt arm (merged) |
| 5 | ecj generic compile | `Arrays.equals(char[]/long[])` compared only element 0 → `"Signature".equals("Synthetic")`=true → generic methods tagged `ACC_SYNTHETIC` & dropped | `1cddae8f` (Unsafe/vectorizedMismatch) |
| 6 | empty JSP body | synthetic `CharArrayWriter` shadow: `write([CII)` no-op + `write(String)` NPE on null `lock` → `JspReader.toCharArray()` empty → empty servlet | `b628d8d7` (run real bytecode) |

**The "group 04 NioEndpoint serving wall" was a MISDIAGNOSIS.** Static files
served `HTTP 200` end-to-end the whole time (`/test/index.html` → 200/957 bytes
in-process); JSPs returned `200` with a `0`-byte body. The empty body was the
CharArrayWriter bug (layer 6), not the connector. There is no serving wall for
this test.

## Layer 6 root cause (the final blocker — commit `b628d8d7`)

CratonVM had a synthetic 2-field `CharArrayWriter` (slots buf=0/count=1) that
shadowed only some methods. The unshadowed ones (`write(String)`, `append`,
`writeTo`) ran real JDK bytecode against a REAL `CharArrayWriter`
(layout: `Writer.lock` + `buf` + `count`). The synthetic `<init>` never set the
inherited `lock`, so `write(String)` NPE'd on `synchronized (lock)`, and the
slot-based bulk write no-op'd. Jasper's `JspReader` does
`caw.write(buf,0,n); … caw.toCharArray()` — which returned EMPTY → zero JSP
nodes → empty servlet → `200`/empty. Fix: gate the synthetic natives under
`synthetic-jdk` and run the real, self-contained JDK bytecode (its ctor chains
through `Writer()` which sets `lock = this`). Verified byte-identical to HotSpot;
diagnosed with `scratch/rec0910/CawProbe.java`.

## Layer 5 root cause (THE fundamental bug — commit `1cddae8f`)

ecj reported phantom `Duplicate field` + `method undefined` for generic methods
when compiling on CratonVM. Narrowing from the JSP all the way down (driving
ecj's `Compiler`/`Parser`/`ClassFileReader`/`SignatureWrapper` directly, then a
3-line pure-Java repro) found:

**`java.util.Arrays.equals(char[],char[])` and `Arrays.equals(long[],long[])`
compared only element 0** — returning `true` for arrays differing at any index
except the first. (`int[]`/`byte[]` happened to be saved by the JDK's scalar
fallback; scales 1 and 3 were not.) Two intrinsic bugs:
- `ArraysSupport.vectorizedMismatch` native (`phases_early.rs`) computed the
  element index as `offset / scale` without subtracting the array base offset
  (ABASE=16): `16/2 = 8` for `char[]` → out-of-range guard → reported "equal".
- `Unsafe.get{Char,Short,Int,Long}[Unaligned]` (`lib.rs`) assembled multi-byte
  reads only for `byte[]`; for typed arrays it read one element zero-extended,
  so `getLongUnaligned(char[],…)` returned 1 char instead of 8 bytes.

The cascade up to the symptom: ecj's `CharOperation.equals` **is**
`Arrays.equals(char[])` → `"Signature".equals("Synthetic")` returned `true` →
in `MethodInfo.readModifierRelatedAttributes` a method's `Signature` attribute
was mis-read as the `Synthetic` attribute → the method got `ACC_SYNTHETIC`
(`0x401`→`0x1401`) → `createMethods` dropped every generic method → "method
undefined" + the phantom "Duplicate field" cascade → JSP won't compile → empty
body → `null.contains("OK")`.

Verified vs HotSpot (byte-identical): Arrays.equals/hashCode/sort/mismatch,
ByteBuffer get/put, String equals/hashCode, HashMap; ecj resolves all generic
methods and compiles the generated JSP with no errors. This was a serious
LATENT VM bug affecting any char[]/long[] comparison and any generic-heavy Java
compilation — not Tomcat-specific.

## Remaining blocker (layer 6 = group 04 serving wall)

With JSP compilation unblocked, `getUrl(...)` still returns a null/empty body:
the embedded `Http11NioProtocol`/`NioEndpoint` connector accepts but does not
drive the accepted `SocketChannel` through read→process→write. No new VM bug
surfaced here yet; tracked under group 04.

## Layer summary (symptom → cause → fix)

1. **Connector reset (FIXED, `socket_channel.rs`).** NIO connector RST'd every
   request: `SocketChannel.setOption` AbstractMethodError (covariant-return
   descriptor missing). Now serves HTTP 200 to external `curl`.
2. **In-process HTTP client (FIXED, commit `48183599`).** `getUrl` →
   `HttpURLConnection`: the native `http_url_connection.rs` read a real
   `sun.net.www` object with its *synthetic* field layout → `getResponseCode()`
   = -1, no request. Fixed by detecting real JDK-constructed HUCs (field 0 is a
   `java/net/URL`) and performing the request from the real URL.
3. **Boot-class resource loading (FIXED, this branch, `net_phase_e.rs`).**
   `ClassLoader.getResourceAsStream("java/lang/String.class")` returned **null**
   on CratonVM (HotSpot returns the 50116-byte class). `getResource` already
   returned a correct URL — `jar:file:/…/java.base.jmod!/java/lang/String.class`
   — but `URL.openStream()` threw `FileNotFoundException`: a `.jmod` archive
   stores classes under a top-level `classes/` prefix (which `find_resource`
   applies internally but the jar-URL stream handler did not). Fix: in the
   single-level `jar:file:` branch of `URL.openStream`, resolve the
   `classes/`-prefixed entry for `.jmod` containers (dev had independently
   landed this same jmod fix for Hibernate's Jandex indexer — kept dev's
   version on merge); plus a genuinely-new `jrt:` scheme arm (for jimage-based
   boot classpaths). Verified vs HotSpot:
   `sysCL.getResourceAsStream` now serves Object/String/IOException, delegation
   through child/grandchild loaders works, and app classes + `file:`/`classpath:`
   URLs are unaffected. This unblocked ecj's type resolution — the
   `"The type java.lang.String cannot be resolved"` JSP error is gone.
4. *(layer 1 above is the server; layers 2–3 are the client + its dependencies.)*
5. **ecj binder duplicate-field miscompile (OPEN — remaining blocker).** With
   layers 1–3 fixed, the JSP now reaches actual compilation and ecj
   (`ecj-4.39`, driven by Jasper's `JDTCompiler` via the `Compiler` API) reports
   **phantom** `Duplicate field bug49196_jsp._jspx_imports_classes` and
   `Duplicate field bug49196_jsp._el_expressionfactory`, then a cascade of
   `_el_expressionfactory cannot be resolved to a variable` → JSP fails to
   compile → empty body → `null.contains("OK")`. Findings:
   - The generated `bug49196_jsp.java` is **valid** — each field is declared
     exactly once, no duplicate class (captured copy:
     `scratch/rec0910/bug49196_jsp.captured.java`).
   - The source is **read correctly** — `FileInputStream → InputStreamReader →
     BufferedReader` returns byte-identical content on CratonVM and HotSpot
     (`scratch/rec0910/mini/RdProbe.java`).
   - **Not a JIT bug** — identical failure with `--nojit` (deterministic,
     interpreter-level).
   - ecj's **parser is correct** — driving `org.eclipse.jdt…parser.Parser`
     directly yields an identical, duplicate-free `TypeDeclaration.fields[]`
     (`[a, b, c, <initializer>, d, e]`) on both VMs
     (`scratch/rec0910/ecjrepro/ParseRepro.java`).
   - So the defect is in ecj's **binding phase**
     (`SourceTypeBinding.buildFields` duplicate detection). The flagged fields
     are exactly the two **adjacent to the `static{}` initializer block** (the
     last field before it and the first after it) — strongly suggesting
     mis-iteration around the initializer slot during field-binding.
   - Reproducing the binder in isolation is itself blocked by CratonVM's
     incomplete JDK **module system**: ecj's batch `Main` rejects `java.home`
     ("invalid location for system libraries" — needs the `jrt` NIO
     filesystem), and the `Compiler`-API path needs a module-aware name
     environment. These module-system gaps are a related sub-area.

   **Next step:** get ecj's binding phase running on a minimal class (fields
   before + after a `static{}` block) — either by implementing enough of the
   `jrt` NIO filesystem for ecj batch `Main`, or a module-aware
   `INameEnvironment` harness — then instrument `SourceTypeBinding.buildFields`
   / its `HashtableOfObject` to see why `c` and `d` are seen twice.

   **Re-confirmed on the fresh dev worktree build (srun, 2026-06-15):** identical
   failure — `Duplicate field _jspx_imports_classes` + `_el_expressionfactory`
   (the field immediately before and immediately after the `static{}` block in the
   captured `bug49196_jsp.java`), `_el_expressionfactory cannot be resolved` ×8,
   under both JIT and `--nojit`. The valid-single-field source was re-captured.
   No regression and no progress on layer 5 — still blocked on the ecj-binding
   isolation harness (jrt module-system gap). Deep, NOT a quick fix.

---

## Historical diagnosis (layers 1–2, retained for context)

**Not** a `PageContext`/EL bug (the original hypothesis). Three findings, none
"pure perf":
- **Layer 1 — connector reset (FIXED, `fix/tomcat-suite-bugs-09-10`).** The NIO
  connector reset every accepted request before reading it, because
  `NioEndpoint.setSocketOptions` → `SocketChannel.setOption` hit an
  `AbstractMethodError` (the `sc_set_option` native was registered only with the
  `NetworkChannel` covariant-return descriptor, not the `SocketChannel` one). Fix
  in `native-io/src/socket_channel.rs`.
- **The embedded SERVER serves HTTP correctly — PROVEN.** With layer 1, running a
  minimal embedded Tomcat (programmatic servlet) on CratonVM and hitting it with
  an **external `curl`** returns `HTTP/1.1 200` + the body, servlet `doGet`
  invoked, connection healthy. So group 04's "servers don't serve" framing is
  refuted for the connector — it serves; the prior failures were the `setOption`
  reset.
- **Remaining blocker — the native `HttpURLConnection` natives corrupt a REAL
  `sun.net.www` object (field-layout mismatch). ROOT-CAUSED.** `TomcatBaseTest.getUrl`
  uses `HttpURLConnection`. For a real `http://` URL, `url.openConnection()` runs
  the genuine JDK bytecode (NOT the synthetic resource-URL intercept in
  `net_phase_e`) and returns a real `sun.net.www.protocol.http.HttpURLConnection`.
  `getResponseCode()` then dispatches to a NATIVE — `http_url_connection.rs::
  huc_get_response_code` (registered last, at `lib.rs:5423`, so it wins over both
  the other two native impls AND the real bytecode). That native reads the object
  with its *own synthetic* field layout (`HUC_CONNECTED`=field 7,
  `HUC_CONN_ID`=field 0, `HUC_URL_STR`=field 1), but the real object's layout is
  completely different. Confirmed by tracing the live call:
  `HUC_CONNECTED(f7)=Int(1)` (a real field that happens to be 1) → `ensure_connected`
  **early-returns without making any request**; `HUC_CONN_ID(f0)=Object` (the real
  `URLConnection.url` field, not an int conn-id) → `with_state` returns `None` →
  `getResponseCode()` yields **`-1`**. That is why there is NO socket I/O, NO
  `ssc_accept`, NO `doGet` — the request is never even attempted. (The earlier
  "GC-starvation" hypothesis was DISPROVEN: `begin_blocking_region` had no effect
  because the client `perform`/`huc_perform` functions it was wrapped around are
  never reached — `ensure_connected` bails first.)
**There are THREE competing native `HttpURLConnection` implementations** — in
`http_url_connection.rs`, `net_phase_e.rs` (`huc_perform`), and `phases_early.rs`
(`p54_huc_do_request`) — each with a *different* synthetic field layout, all
registered on `java/net/HttpURLConnection`/`sun/net/www/...`; last-writer-wins
picks the `http_url_connection.rs` one, which then corrupts real JDK objects.
**Severity:** Medium-High (blocks every getUrl-based embedded-HTTP test).
**Repro class:** `jakarta.servlet.jsp.TestPageContext` — `Tests run: 1, Failures: 1`.

## Next step for the remaining blocker

The clean fix is to stop the synthetic `HttpURLConnection` natives from hijacking
real `sun.net.www` objects. Options, in rough order of safety:
1. Consolidate the THREE native impls into one and make the method natives detect
   a real (JDK-constructed) `HttpURLConnection` — e.g. field 0 is a `java/net/URL`
   object (the real `URLConnection.url`) rather than an int conn-id — and in that
   case read the URL from the real layout and perform the request (the real
   `java.net.Socket` path already works in-process: an in-process raw
   `java.net.Socket` client gets `doGet` invoked), instead of trusting the
   synthetic `HUC_*` slots.
2. OR drop the method natives on `sun/net/www/protocol/http(s).HttpURLConnection`
   so real bytecode runs for real HTTP, while keeping the synthetic carrier +
   natives ONLY for the `net_phase_e` resource-URL path (`file:`/`jar:`/
   `classpath:` via `getInputStream`→`openStream`).
RISK: the synthetic path is load-bearing for resource loading (Spring
`spring.factories`, jar URLs, WildFly bootstrap), so any change MUST preserve it
(verify those don't regress). Minimal repro: `scratch/rec0910/mini/MiniHU.java`
(CratonVM `HttpURLConnection` client + CratonVM server in one process → code=-1)
vs the external-`curl` success; `[GRC-A]` trace showed `HUC_CONNECTED(f7)=Int(1)`
on the real object.

## Symptom

```
java.lang.NullPointerException: Cannot invoke contains on null
  at jakarta.servlet.jsp.TestPageContext.testBug49196(TestPageContext.java:34)
```

Line 34 is `Assert.assertTrue(result.contains("OK"))`, where
`result = res.toString()` and `res = getUrl(".../bug49nnn/bug49196.jsp")`. So
`res.toString()` is **null** — i.e. the HTTP GET returned an **empty body**.

## Root cause (re-diagnosed — original "EL/PageContext null" hypothesis was WRONG)

The JSP `bug49196.jsp` is trivial (`pageContext.getErrorData()` then prints
`OK`) and `getErrorData()` already null-guards the status code, so the JSP is not
the problem. The real failure is one layer down, in HTTP serving:

The embedded Tomcat NIO connector **starts and binds a port**, **accepts the TCP
connection**, but then **resets the connection without ever sending an HTTP
response** — for *every* request, static or JSP.

Diagnosis (custom `TomcatBaseTest` subclass `TPCDiag`, both real-net + AQS env on):

| request | via | result |
|---|---|---|
| `/test/index.html` (**static**) | `HttpURLConnection` | `getResponseCode() == -1`, body `null` |
| `/test/bug49196.jsp` (**JSP**)  | `HttpURLConnection` | `getResponseCode() == -1`, body `null` |
| `/test/index.html` (**static**) | **raw `java.net.Socket`** | connects OK, then **`SocketException: Connection reset` (os err 10054) on read** |
| `/test/bug49196.jsp` (**JSP**)  | **raw `java.net.Socket`** | same connection reset |

Key conclusions:
- A **static file** fails identically to the JSP → this is **not** JSP/Jasper/EL
  related. The original hypothesis (a `PageContext`/EL accessor returning null,
  shared with bugs 07/08) is **refuted**.
- The raw socket **connects** (TCP accept works) but the server **RSTs the
  connection on read** — so the accepted socket is never read/processed/answered.
  This is a server-side **NIO-connector** defect, downstream of accept.
- Server log corroboration during startup: `StandardWrapperValve[Container is
  null]` / `StandardEngineValve[Container is null]` stop() messages — the
  container pipeline / wrapper wiring is not fully initialised, consistent with
  accepted requests not reaching a working servlet pipeline.

So the chain is: connector accepts → accepted `SocketChannel` is not driven
through the `NioEndpoint` poller/read→process→write cycle → connection reset →
client reads empty/`-1` → `ByteChunk.toString()==null` → `null.contains("OK")`.

This is the same wall as group 04 (embedded-server serving): basic blocking
`Socket` round-trips work (`SockProbe`), but the full `Http11NioProtocol` /
`NioEndpoint` poller + `Selector` + non-blocking `SocketChannel` read/write cycle
does not complete a request. `native-io/src/nio_selector.rs` is a mature, heavily
debugged implementation (deadlock fixes, GC-stable lock ordering), so the
remaining defect is a subtle accept→register→read interaction, not a missing
primitive.

## Why this is NOT a surgical fix

Fixing it means making the Tomcat NIO connector actually serve an HTTP request
end-to-end under CratonVM — i.e. solving group 04, the dominant OPEN wall. That
is a large, high-risk effort in the mature NIO/connector subsystem, well beyond a
per-test fix. Tracking it under group 04; the per-class FAIL/HANG/NOSUMMARY
flakiness of `TestPageContext` across suite runs is consistent with this serving
wall plus timing.

## Next steps (for the group-04 effort)

- Instrument the `NioEndpoint` accept→`Poller.register`→`Selector.select` path:
  confirm whether the accepted `SocketChannel` is registered with the poller's
  `Selector` and whether a read-ready event is ever delivered for it.
- Check the `StandardWrapperValve[Container is null]` pipeline wiring — whether
  the context/wrapper container is actually started for `addWebapp` deployments.
- A passing end-to-end raw-socket `GET` against an embedded `Tomcat` is the
  minimal green target before any JSP/servlet-level test can pass.

## Reproduction

```
cratonvm.exe -Xmx2g -cp <cp> org.junit.runner.JUnitCore \
  jakarta.servlet.jsp.TestPageContext   # CWD: apps/tomcat
# -> Tests run: 1, Failures: 1 ; HotSpot: PASS
# Root cause: embedded NIO connector resets accepted connections without
# responding (status -1 / empty body) for ALL requests, static or JSP.
```
