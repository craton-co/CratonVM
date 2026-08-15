# `handler.ssl` classes discovered far fewer test instances than HotSpot — FIXED 2026-08-13

**Status:** ✅ FIXED. Root cause was single and shared by all five classes:
`OpenSsl.isAvailable()` was permanently `false` on CratonVM, so netty's own
`@MethodSource` data providers never generated their `SslProvider.OPENSSL` /
`OPENSSL_REFCNT` parameters. Retired from `docs/known-issues/netty/`.

## What the original doc reported

Five classes reported a drastically smaller `found` count than HotSpot on the
identical classpath — a JUnit-discovery-time gap, not a pass/fail difference:

| class | before | HotSpot | after (this fix) |
|---|---|---|---|
| `ParameterizedSslHandlerTest` | 7 / 7 | 63 / 63 | **63 found, 63 ok** |
| `SniClientTest` | 3 / 3 | 27 found | **27 found, 27 ok** |
| `SniHandlerTest` | 12 / 12 | 50 found | **50 found, 40 ok** |
| `SslErrorTest` | 0 (NOTESTS) | 72 / 72 | **72 found, 72 ok** |
| `OpenSslPrivateKeyMethodTest` | 0 (NOTESTS) | 24 found, 2 ok | **24 found, 0 ok** |

("before" re-measured on Linux 2026-08-13 with the same tcnative-bearing
classpath HotSpot had, so the two columns are comparable; the original doc's
numbers came from Windows.)

## Root cause

Every one of those classes gates its parameter list on `OpenSsl.isAvailable()`:

```java
// ParameterizedSslHandlerTest.data(), SslErrorTest.data(),
// SniClientTest.parameters(), SniHandlerTest.data()
if (OpenSsl.isAvailable()) {
    providers.add(SslProvider.OPENSSL);
    providers.add(SslProvider.OPENSSL_REFCNT);
}
providers.add(SslProvider.JDK);
```

3 providers → 9 client/server combinations; 1 provider → 1. That is exactly the
11 % ratio the original doc measured, and `SslErrorTest`'s list is
OpenSSL-only, which is why it reported zero.

`OpenSsl.isAvailable()` was false because `vm/src/vm/vm_exec.rs`'s
`load_native_library` skipped `JNI_OnLoad` for **any** library whose basename
contained `tcnative`, and the dispatch site refused symbol resolution for
`io/netty/internal/tcnative/**`. The stated reason was a Windows
`0xC0000005` inside `JNI_OnLoad`/`RegisterNatives`. That blocker was stale:
`RegisterNatives` could not serve the `FindClass` + `RegisterNatives` idiom at
all when the skip was written (see `native/jni.rs`'s index-215 note), and it has
since been fixed.

## What the fix took (four defects, in the order they surfaced)

1. **The skip itself** (`vm/src/vm/vm_exec.rs`). `netty_tcnative` is now
   distinguished from Tomcat's APR `tcnative-1` by the `netty` prefix and its
   `JNI_OnLoad` runs. Success is recorded on the per-VM
   `NativeRealm::netty_tcnative_real`; `CRATONVM_SYNTHETIC_NETTY_TCNATIVE=1`
   restores the old behaviour.

2. **The stubs had to stand down in ONE place, not at a dispatch site**
   (`native-api/src/registry.rs`). Suppressing the
   `register_netty_internal_tcnative_natives` stand-ins only in `vm_exec`'s
   general `is_native` arm produced a *half-real* package:
   `try_stackless_invoke`'s `resolve_step1_native` reaches the registry first
   for a plain zero-arg static, which is exactly `Library.initialize0()Z`. Real
   `aprVersionString` answered `1.7.5` and real `SSL.versionString` answered
   `BoringSSL`, while the stubbed `initialize0` returned `true` without ever
   calling `apr_initialize` — so `tcn_global_pool` stayed NULL and the first
   real `SSLContext.make` took a SIGSEGV inside `apr_pool_create_ex` with a
   NULL parent pool. The retirement now lives on `slot_index_for_key`, the one
   edge every resolution route shares.

3. **`Unsafe`-arena handles reaching C through a plain `long` parameter**
   (`vm/src/native/jni.rs`). `GetDirectBufferAddress` already translated a
   tagged arena handle to a real address; a library that instead takes the
   address as a `jlong` argument — `SSL.bioWrite(long bio, long address, int
   len)` — got the raw handle. `BUF_MEM_append` took a SIGSEGV with
   `0x4000_0010_0000_0010` in RSI. `jni_long_arg_bits` now translates a tagged
   handle that a live arena block covers, and passes anything else through
   untouched.

4. **`dlclose` at VM teardown** (`vm/src/vm/realms/native_realm.rs`).
   BoringSSL and APR register per-thread cleanup with `pthread_key_create`, and
   glibc runs those destructors in `__nptl_deallocate_tsd` as the thread
   finishes exiting — after `run()` has returned and the realm has dropped.
   With the library unmapped the destructor address dangles, and every
   OpenSSL-touching netty class took a SIGSEGV *after* printing `@@RESULT`.
   `NativeRealm`'s `Drop` now leaks the handles, which is HotSpot's own rule.

## Verification

Azure host 2, Linux x86_64, JDK 25, branch built from `origin/dev` @ `c4c972da7`.
The harness classpath had to be given `netty-tcnative-boringssl-static` first:
the reactor resolved the *dynamic* `netty-tcnative`, whose `.so` needs
`OPENSSL_3.2.0` that this host's `libssl.so.3` does not export, so HotSpot could
not load it either. With the static jar on the classpath HotSpot reproduces the
original doc's HotSpot column exactly (`SslContextBuilderTest` 21/21,
`CloseNotifyTest` 4/4, `OpenSslPrivateKeyMethodTest` 24 found / 2 ok), which is
what makes the two columns comparable.

Across all 66 `io.netty.handler.ssl.*` classes, same classpath, 400 s cap:

| | found | ok | failed | hang |
|---|---|---|---|---|
| CratonVM before | 450 | 229 | 41 | 1 |
| CratonVM after | **682** | **536** | 90 | 5 |
| HotSpot 25 | 2187 | 2013 | 51 | 4 |

(Hung classes are excluded from the totals on the row they hang in.)

No class regressed. Every per-class difference is an improvement, a class that
previously reported `started=0` and now runs, or one of four —
`OpenSslEngineTest`, `ReferenceCountedOpenSslEngineTest`,
`JdkOpenSslEngineInteroptTest`, `OpenSslJdkSslEngineInteroptTest` — that went
from `started=0` (the whole class assumption-skipped) to HANG at the 400 s cap.
**HotSpot hangs on exactly the same four at the same cap**, so that is "the
class now has hundreds of tests to run", not a correctness change; they need a
per-class timeout override, not a fix.

The `failed` rise is the newly-executed OpenSSL half — 89–100 % of those
classes had never run before — and is tracked in
`docs/known-issues/netty/openssl-key-material-and-engine-residuals-20260813.md`.

## Repro (kept for the sibling doc)

```bash
cd apps/netty-suite-runner
printf '%s\n' io.netty.handler.ssl.ParameterizedSslHandlerTest io.netty.handler.ssl.SniClientTest io.netty.handler.ssl.SniHandlerTest io.netty.handler.ssl.SslErrorTest io.netty.handler.ssl.OpenSslPrivateKeyMethodTest > /tmp/discovery.txt
CV_BIN=bin/cratonvm-netty-zgc bash run-netty-suite.sh --list /tmp/discovery.txt --gc zgc --shards 1 --timeout 400 --out /tmp/repro
```

`common.args` must carry `netty-tcnative-boringssl-static-<ver>-<os>.jar`, or
`OpenSsl.isAvailable()` is false for the same reason on both VMs and the
comparison is vacuous.
