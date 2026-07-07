# http.server bug cluster (12 classes) — fixes landed [FIXED, retrospective]

Status: the bulk of this investigation is FIXED and merged to `dev`. This
doc is the historical fix record; kept in `docs/internal/` per the
known-issues convention. One residual remains genuinely OPEN — see
`docs/known-issues/http-server-sslengine-identity-singleton-clobber.md`.

## Summary

10 of 12 classes fully fixed. `ZeroCopyIntegrationTests`'s flakiness (an
ABBA lock-order deadlock, the same bug independently rediscovered in a
sibling http.client investigation — see below) is now FIXED.
`ServerHttpsRequestIntegrationTests` had FOUR real bugs found and fixed
across sessions but still fails for a fifth, distinct reason — see the
dedicated OPEN doc.

## Root causes fixed

1. **`Collections.emptyListIterator()` mis-stamped as `Collections$EmptyIterator`**
   (an `Iterator`, not a `ListIterator`) instead of `Collections$EmptyListIterator`.
   Any caller holding the result as a `ListIterator` (e.g. Jetty's
   `ContextHandler.notifyExitScope`) hit `NoSuchMethodError:
   Collections$EmptyIterator.hasPrevious()Z` on the **main thread** — an
   uncaught linkage error there aborts the whole VM process. This was THE
   dominant bug: crashed 8 of 9 ABEND classes. Fixed by splitting off
   `native_empty_list_iterator` with its own `EMPTY_ITERATOR` singleton
   field — `native-collections/src/lib.rs`.
2. **`Collections.newSetFromMap(LinkedCaseInsensitiveMap)` lost
   case-insensitive key semantics** (same class of bug as the
   `IdentityHashMap` case, `HIB-CV-28`) — extended the existing
   `IdentityHashMap` special-case to also cover
   `org/springframework/util/LinkedCaseInsensitiveMap`, routing both
   through the real `Collections$SetFromMap`. Follow-on regression also
   fixed: `native_set_from_map_size` misread a real backing map's `table`
   field via the synthetic-HashMap-layout heuristic instead of delegating
   to the real `size()` — `native-builtins/src/lib.rs`,
   `native-collections/src/lib.rs`.
3. **`java.net.URI`'s single-string parser accepted malformed `%` escapes**
   (no check for `"%" hex hex`) — fixed `uri_first_illegal_index`.
4. **`java.net.URI`'s multi-argument constructors didn't quote the query
   component at all** — added `quote_uric`, mirroring real JDK's private
   `quote()`.
5. **`URLDecoder`/`URLEncoder`'s charset-aware overloads ignored the
   requested charset**, always UTF-8. Split percent-decoding (bytes) from
   the bytes↔String charset conversion step.

### `ServerHttpsRequestIntegrationTests` — four bugs found + fixed (fifth remains open)

1. `java.security.Provider.putService(Provider$Service)` had no native
   shim, so BouncyCastle's `addAlgorithm`-style service registration
   (`GOST3411$Mappings.configure()` etc.) ran against never-initialized
   synthetic-`Provider` fields. Compounded by a `containsKey` shim keyed
   only by provider **name** (shared across every same-named `Provider`
   instance) throwing a spurious `IllegalStateException: duplicate
   provider key` when BC legitimately constructs a second independent
   instance in one process. Fixed: added `putService`
   (`native-builtins/src/jca/provider_chain.rs`) plus a per-instance
   `containsKey` side table keyed by `(identity_hash_of_receiver, key)`.
2. `CertificateFactory.generateCertificate(s)` always ran a hardcoded
   synthetic DER parser regardless of the real `certFacSpi` a real
   `getInstance(algo, Provider)` call had wired up, so BC-issued
   certificates lost their concrete class/behavior. Fixed
   (`register_p68_security_cert`, `native-builtins/src/phases_late.rs`):
   delegate to the real `certFacSpi` via `invoke_virtual` when present.
3. PKCS12/PBE empty-password guard threw instead of producing a
   0-length key (real JDK explicitly allows this,
   `com.sun.crypto.provider.PBEKey`'s own doc comment: "Should allow an
   empty password"). Fixed in `pbe_generate_secret`
   (`native-builtins/src/phases_early.rs`).
4. `register_p68_ssl` (backs `SecretKeyFactory`/`SSLContext`/
   `SSLSocketFactory`/`SSLEngine`/`SSLSocket` real-TLS natives) was only
   reachable from a `#[cfg(feature = "synthetic-jdk")]`-gated function,
   compiled entirely out of the default real-JDK `cratonvm-cli` build —
   so NONE of these natives were ever registered in real-JDK mode, the
   default every `--jdk real` suite run uses. Fixed: call
   `register_p68_ssl` directly from `register_essential_natives`,
   positioned before `net_phase_e::register_phase_e_networking` so the
   latter's correct rustls-backed `SSLEngine` registration still wins via
   last-writer-wins over `register_p68_ssl`'s fake non-cryptographic
   stub. ("Narrow, not broad" — calling the whole `register_phase68_natives`
   umbrella regressed Tomcat's SAX parsing via a bundled synthetic-only
   `register_p68_xml`; narrowed to just `register_p68_ssl`.)
5. **Fifth bug, NOT part of this fixed set — see the dedicated OPEN doc**:
   `KeyStore.setKeyEntry` staging + a process-wide `RUNTIME_TLS_IDENTITY`
   singleton clobbering bug now root-caused (deeper than the originally
   suspected `do_wrap`/`do_unwrap` semantics issue) — see
   `docs/known-issues/http-server-sslengine-identity-singleton-clobber.md`.

Also fixed alongside: `KeyStore.setKeyEntry(String, Key, char[],
Certificate[])` was entirely unregistered (native-builtins/src/keystore.rs);
`keystore_set_pending_km_identity` unwrapped the wrong object (always read
store-id 0); `keystore_get_first_private_key`'s fallback now scans the live
registry instead of only a stale load-time snapshot;
`SSLEngine.getSupportedCipherSuites`/`getSupportedProtocols` were missing
(present on `SSLSocket`/`SSLSocketFactory` already).

### `ZeroCopyIntegrationTests` — flaky teardown/dynamic-class-generation hang, FIXED

Originally documented as "flaky, ~40-60% hang, pre-existing, not this
session's regression, unrelated ByteBuddy dynamic-class-generation timing
issue." Root-caused in a later, dedicated session: the SAME
`class_manager`/`vtable_manager` AB-BA lock-order deadlock documented in
`docs/internal/fixed-suite-bugs/http-client-cluster-redefine-dispatch-fixes-FIXED.md`
(`execute_invokevirtual_vtable_fast` holding `vtable_manager` read while
acquiring `class_manager` read, opposite the class-loading path's write
order). ByteBuddy's dynamic-class generation (`TypeWriter$Default.make`)
is a reliable trigger because it's deeply recursive reflective/lambda
dispatch that both defines classes (write side) and does ordinary virtual
dispatch (read side) in tight succession — but the bug is general, not
specific to ByteBuddy or this test.

Fixed by the same change as the http.client cluster (drop the
`vtable_manager` guard before acquiring `class_manager` in
`execute_invokevirtual_vtable_fast`). Verified: 9/15 (60%, light host
load) to 17/17 (100%, heavy host load) hangs before → **20/20 clean runs,
0 hangs** after, each completing in 8-14s (vs. hitting the 60s timeout
before).

## Verification

- `ServletServerHttpRequestTests`: 17/17 OK (was FAIL 14/17).
- `HeadersAdaptersTests`: 90/90 OK (was FAIL 88/90).
- `AsyncIntegrationTests`, `CookieIntegrationTests`,
  `EchoHandlerIntegrationTests`, `ErrorHandlerIntegrationTests`,
  `MultipartHttpHandlerIntegrationTests`, `RandomHandlerIntegrationTests`,
  `ServerHttpRequestIntegrationTests`, `WriteOnlyHandlerIntegrationTests`:
  no longer ABEND.
- Full `--only 'http\.server\.'` cluster (33 classes) regression-checked
  after the SSL-registration fix: `LOADERR=2 (pre-existing classpath gap,
  unrelated), OK=24, FAIL=2, TIMEOUT=5` — the 5 TIMEOUTs and 1 FAIL
  independently confirmed to reproduce identically on an unmodified `dev`
  baseline (pre-existing shared-host/ByteBuddy contention flakiness, not a
  regression from this cluster's fixes).

## Reproduction

```bash
cd /c/craton/cratonvm/apps/spring-suite-runner
export MSYS2_ARG_CONV_EXCL='*' MSYS_NO_PATHCONV=1
CRATONVM_BIN=<your-built-cratonvm.exe> KRUN_STACK=1 \
  ./run-suite.sh run --jdk real --jit on --batch 1 --only 'http\.server\.'
```

On the Azure host, `run-suite.sh` needs `JDK25_WIN=/home/victor/jdk25` and
a `cygpath` shim on `PATH`; the shared checkout's classpath join at
`apps/spring-suite-runner/run-suite.sh:194` uses `;` (Windows separator),
which breaks on Linux — **do not edit the shared checkout**, copy
`apps/spring-suite-runner` to scratch space and change that one line.
