# http.server bug cluster (12 classes) — fixes landed + residuals

## Status

**Mostly fixed** on branch `fix/http-server-cluster` (off dev `81762470`).
Reproduced and iterated on both Windows + JDK25 (the platform the original
bug report was captured on, via `apps/spring-suite-runner`) and the Azure
Linux host (`victor@20.84.156.31`, `/data/wt/wt-httpserver-cluster`, JDK25 at
`/home/victor/jdk25`) for fast iteration. 10 of 12 classes are fully fixed;
2 have residual issues documented below.

## Root causes fixed

1. **`Collections.emptyListIterator()` mis-stamped as `Collections$EmptyIterator`**
   (an `Iterator`, not a `ListIterator`) instead of `Collections$EmptyListIterator`.
   `native_empty_iterator` served both `emptyIterator()` and `emptyListIterator()`
   with the same class stamp. Any caller holding the result as a `ListIterator`
   (e.g. Jetty's `ContextHandler.notifyExitScope` walking an empty listener
   list via `list.listIterator(list.size())`) hit `NoSuchMethodError:
   Collections$EmptyIterator.hasPrevious()Z` on the **main thread**, which is
   an uncaught linkage error there — it aborts the whole VM process instead of
   just failing one test. This was THE dominant bug: it crashed 8 of the 9
   ABEND classes (`AsyncIntegrationTests`, `CookieIntegrationTests`,
   `EchoHandlerIntegrationTests`, `ErrorHandlerIntegrationTests`,
   `MultipartHttpHandlerIntegrationTests`, `RandomHandlerIntegrationTests`,
   `ServerHttpRequestIntegrationTests`, `WriteOnlyHandlerIntegrationTests`) —
   all exercise the same 4-way parameterized `AbstractHttpHandlerIntegrationTests`
   including a Jetty (Servlet) backend, and Jetty's `ContextHandler` calls
   `notifyExitScope` on every request. Fixed by splitting off
   `native_empty_list_iterator`, which stamps `Collections$EmptyListIterator`
   and uses its own `EMPTY_ITERATOR` singleton field.
   `native-collections/src/lib.rs`.

2. **`Collections.newSetFromMap(LinkedCaseInsensitiveMap)` lost case-insensitive
   key semantics.** The native returned a synthetic value-hash `HashSet`
   regardless of the backing map's own key-equality semantics (same class of
   bug as the already-fixed `IdentityHashMap` case, see
   `docs/internal/h2-suite-bugs/run-20260622/HIB-CV-28-...md`) — a value-hash
   `HashSet<String>` doesn't replicate `LinkedCaseInsensitiveMap`'s
   case-folding, so `HttpComponentsHeadersAdapter`'s header-name
   `Set` (`Collections.newSetFromMap(new LinkedCaseInsensitiveMap<>(...))`)
   kept "TestHeader" and "TestHEADER" as two distinct entries instead of one.
   Fixed `HeadersAdaptersTests` (`shouldRemoveCaseInsensitiveFromKeySet`,
   `headerSetEntryCanSetList`). Extended the existing `IdentityHashMap`
   special-case in the `newSetFromMap` native to also cover
   `org/springframework/util/LinkedCaseInsensitiveMap`, routing both through
   the real `Collections$SetFromMap` (whose `contains`/`toArray`/`iterator`
   natives already delegate to the live backing map via `invoke_virtual`).
   `native-builtins/src/lib.rs`.

   **Follow-on regression, also fixed**: `native_set_from_map_size` routed
   through `native_map_size`'s synthetic-HashMap-layout heuristic
   (`map_state`'s "is slot 0 an array" probe) instead of delegating to the
   backing map's real `size()` via `invoke_virtual` like its
   `contains`/`toArray`/`iterator` siblings already did. A real
   `LinkedCaseInsensitiveMap`'s own `table` field happens to satisfy that
   "is it an array" probe, so it was misread as CratonVM's synthetic bucket
   layout and always reported size 0 — this broke every
   `hasSize(N)`-style `HeadersAdaptersTests` assertion once fix #2 started
   returning a real `SetFromMap` instead of the old (wrong-semantics but
   correctly-sized) synthetic `HashSet`. `native-collections/src/lib.rs`.

3. **`java.net.URI`'s single-string parser accepted malformed `%` escapes.**
   `uri_first_illegal_index` treated `%` as always legal, with no check that
   it's followed by two hex digits (RFC 2396 `escaped = "%" hex hex`). Real
   JDK throws `URISyntaxException` for a bare/malformed `%` (`"foo%%x"`,
   `"/p%th"`); ours silently accepted it. This broke
   `ServletServerHttpRequest.initURI`'s malformed-query catch-and-reencode
   fallback (never triggered, since the first parse attempt never threw) and
   the "malformed path must throw `IllegalStateException`" contract. Fixed
   `ServletServerHttpRequestTests.getUriWithMalformedQueryParam`/
   `getUriWithMalformedPath`. `native-builtins/src/lib.rs`.

4. **`java.net.URI`'s multi-argument constructors didn't quote the query
   component at all.** `native_uri_init_5`/`native_uri_init_7` spliced the
   raw query string directly into the full URI string with no escaping, so
   `encodeQuery`'s `new URI(null, null, "", query, null).getRawQuery()` (the
   fallback `initURI` uses to fix up a malformed query) was a no-op — a bare
   `%` was never turned into `%25`. Added `quote_uric`, mirroring
   `java.net.URI`'s private `quote(String, L_URIC, H_URIC)`: percent-escape
   any character outside RFC 2396 `uric` (reserved | unreserved), including a
   literal `%` (which real JDK always escapes here — these constructors have
   no "already escaped" concept). `native-builtins/src/lib.rs`.

5. **`URLDecoder.decode(String, String)` / `(String, Charset)` and
   `URLEncoder.encode(String, String)` / `(String, Charset)` ignored the
   requested charset entirely**, always UTF-8-(lossy-)decoding/encoding the
   percent-escaped bytes regardless of the charset argument (both the active
   `deprecated_io_util.rs` registrations and the dormant duplicate
   registrations in `phases_early.rs`). A non-UTF-8 charset like
   `windows-1251` therefore produced U+FFFD replacement characters on decode
   (`ServletServerHttpRequestTests.getFormBodyWithNotUtf8Charset`) or UTF-8
   bytes instead of the charset's own single-byte encoding on encode. Split
   percent-decoding (bytes) from the bytes-to-`String`/`String`-to-bytes
   charset conversion step and routed the charset-aware overloads through
   the existing `charset::decode_str_named`/`encode_str_named`/
   `charset_name_of` helpers. `native-builtins/src/deprecated_io_util.rs`,
   `native-builtins/src/phases_early.rs`.

## Verification

- `ServletServerHttpRequestTests`: **17/17 OK** (was FAIL 14/17).
- `HeadersAdaptersTests`: **90/90 OK** (was FAIL 88/90, then transiently
  85/90 after fix #2 before the size() follow-on was found).
- `AsyncIntegrationTests`, `CookieIntegrationTests`,
  `EchoHandlerIntegrationTests`, `ErrorHandlerIntegrationTests`,
  `MultipartHttpHandlerIntegrationTests`, `RandomHandlerIntegrationTests`,
  `ServerHttpRequestIntegrationTests`, `WriteOnlyHandlerIntegrationTests`:
  no longer ABEND; all methods pass on the Jetty/Jetty-Core/Tomcat backends.
  On the Azure Linux verification host only, the "Reactor Netty" backend
  parameterization fails with `UnsatisfiedLinkError:
  sun/nio/ch/IOUtil.fdLimit()I` and/or `NoClassDefFoundError:
  sun/nio/ch/EPollSelectorImpl` — both are **pre-existing, documented,
  Linux-only native gaps** (see
  `docs/known-issues/http-client-cluster-redefine-dispatch-and-jdk21-gaps.md`'s
  "Linux-only NIO gaps" residual), unrelated to this cluster's fixes and not
  expected to reproduce on the Windows+JDK25 target the original bug report
  was captured on.

## Residual — NOT fixed this session

### `ServerHttpsRequestIntegrationTests` — `CertificateEncodingException`

```
java.security.cert.CertificateEncodingException: java.security.cert.CertificateException:
Could not parse certificate: java.io.IOException: java.lang.IllegalArgumentException:
Illegal base64 character 0
    at io.netty.handler.ssl.util.SelfSignedCertificate.<init>(SelfSignedCertificate.java:242)
```

Traced (not fixed) to: the test's `ReactorHttpsServer.initServer()` calls
`new SelfSignedCertificate()`, which — with BouncyCastle unavailable —
falls back to `OpenJdkSelfSignedCertGenerator` (reflection into
`sun.security.x509.*` to build and sign an `X509CertImpl`), then
`SelfSignedCertificate.newSelfSignedCertificate` PEM-encodes `cert.getEncoded()`
via Netty's own `io.netty.handler.codec.base64.Base64` + `ByteBuf.toString()`,
writes it to a temp `.crt` file via plain `FileOutputStream`, and the outer
constructor reads it back via `FileInputStream` +
`CertificateFactory.getInstance("X509").generateCertificate(...)` (real
`sun.security.provider.X509Factory` bytecode — the literal error string
`"Could not parse certificate: "` is hardcoded there, confirmed via
`src.zip`, not this codebase).

**Ruled out** (isolated minimal repros, both byte-identical to HotSpot on
CratonVM):
- `Unpooled.wrappedBuffer(bytes)` → `Base64.encode(buf, true)` →
  `toString(US_ASCII)` — no NUL corruption, 0 in every test.
- Plain `FileOutputStream.write(pemBytes)` → `FileInputStream` →
  `CertificateFactory.generateCertificate` round-trip on synthetic DER — byte
  for byte identical to what was written, same benign parse error as
  HotSpot for deliberately-invalid fake DER.

Both the general Netty-ByteBuf/Base64 path and the general file-I/O +
CertificateFactory path are clean. The remaining suspects (not yet isolated):
the OpenJDK reflective cert-signing chain itself (`X509CertInfo`/
`X509CertImpl.sign` via reflection — does `cert.getEncoded()` on the
resulting object return correct DER when the object was built this way under
CratonVM?), or something specific to the exact interleaving of
`Unpooled.wrappedBuffer(cert.getEncoded())` immediately after that reflective
construction. Old leftover `keyutil_localhost_*.crt` temp files found on the
Azure host (from an unrelated prior run, likely without BouncyCastle on the
classpath either) show a suspicious pattern worth following up on: correct
total file size, correct `-----BEGIN CERTIFICATE-----\n` header (28 bytes),
then **all-NUL content** for the rest of the file — i.e. the header write
landed but the base64 payload write did not, which doesn't match either of
the two ruled-out paths and points at something upstream of both (possibly
the reflective sign()/getEncoded() chain, or PlatformDependent's temp-file
creation path specifically). Needs a repro run with BouncyCastle actually on
the classpath (spring-framework's real test dependency) captured live, not a
simplified standalone harness.

### `ZeroCopyIntegrationTests` — Jetty Core backend, `written 0 < N content-length`

```
java.lang.AssertionError: <html>...<title>Error 500 java.io.IOException: written 0 &lt; 951 content-length</title>...
```

Only the "Jetty Core" parameterization fails (Reactor Netty's failure on
Linux is the `fdLimit` gap above, not this). `JettyCoreServerHttpResponse.writeWith(Path, long, long)`
uses Jetty's own `Content.copy(Content.Source.from(null, file, position, count), this.response, callback)`
— a different code path from the `FileChannel.transferTo`-based zero-copy
natives in `native-io/src/file_channel.rs` (which are exercised by the
Reactor Netty backend and are known-correct there: real
`copy_file_range`/`sendfile` on Linux, a correctness-preserving userspace
copy loop on Windows). Jetty's `Content.Source`/`IteratingCallback`/
`Callback.Completable` machinery for a `Path`-backed source wasn't traced
this session — the "0 bytes written" symptom means either the `FileChannel`
read inside Jetty's `Content.Source` came back empty, or the write to the
response (`Response.write`) silently dropped the buffer. Needs a dedicated
trace of Jetty 12's `PathContentSource`/`Response.write` native touchpoints.

## Repro

```bash
cd /c/craton/cratonvm/apps/spring-suite-runner
export MSYS2_ARG_CONV_EXCL='*' MSYS_NO_PATHCONV=1
CRATONVM_BIN=<your-built-cratonvm.exe> KRUN_STACK=1 \
  ./run-suite.sh run --jdk real --jit on --batch 1 --only 'http\.server\.'
```

On the Azure host, run-suite.sh needs `JDK25_WIN=/home/victor/jdk25` and a
`cygpath` shim on `PATH` (`/home/victor/localbin/cygpath` already exists —
it's a no-op passthrough since Linux needs no Windows-path translation) —
and the shared checkout's classpath join at
`apps/spring-suite-runner/run-suite.sh:194` uses `;` (Windows classpath
separator), which breaks on Linux. **Do not edit the shared checkout** — copy
`apps/spring-suite-runner` to scratch space (e.g. `~/my-suite-runner`) and
change that one line's `;` to `:` in your own copy instead.
