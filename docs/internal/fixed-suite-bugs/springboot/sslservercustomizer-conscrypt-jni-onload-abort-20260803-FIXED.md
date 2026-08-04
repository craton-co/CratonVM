# `SslServerCustomizerTests` CRASH — Conscrypt's `JNI_OnLoad` aborts on non-Windows — FIXED 2026-08-03

**Status: FIXED.** Found in the 2026-08-02 Azure Linux full-suite run
(`RESULTS-20260802-azure-fullsuite.md`, the sole CRASH:
`module/spring-boot-jetty` · `SslServerCustomizerTests`, 1.4s).

## Signature

```
FINE [org.conscrypt.NativeLibraryLoader] -Dorg.conscrypt.native.workdir: /tmp
failed to find class 'java/lang/Object'

#
# A fatal error has been detected by the CratonVM Runtime Environment:
#  SIGABRT at pc=0x..., addr=0x..., pid=..., tid=...
#  jdk mode: real-jdk
```

`failed to find class 'java/lang/Object'` is **not** a CratonVM message —
grepping the whole tree for it turns up nothing. It is printed by
Conscrypt's own native library, `libconscrypt_openjdk_jni-linux-x86_64*.so`,
from inside its `JNI_OnLoad` → `conscrypt::jniutil::init(JavaVM*, JNIEnv*)`,
which calls `abort()` when one of its own bootstrap `FindClass` lookups
comes back null. Confirmed with a `gdb -batch -ex "handle SIGABRT stop
nopass" -ex run -ex "bt full"` wrapper in place of the `cratonvm` binary
(pointed at via the suite runner's `-Exe`) — the crashing thread's stack is:

```
#0  __pthread_kill_implementation
...
#4  __GI_abort ()
#5  conscrypt::jniutil::init(JavaVM_*, JNIEnv_*) () from libconscrypt_openjdk_jni-...so
#6  JNI_OnLoad () from libconscrypt_openjdk_jni-...so
#7  load_native_library () at vm/src/vm/vm_exec.rs:13269
#8  {closure#7} () at native-builtins/src/lang_system.rs:1385   [System.loadLibrary]
```

No Rust panic, no backtrace from `RUST_BACKTRACE=full` (there wasn't one to
print) — this is a foreign `.so` deliberately aborting the process, not a
CratonVM-side fault.

## Root cause

`load_native_library` (`vm/src/vm/vm_exec.rs`) already knew about this
**exact** failure mode for a **different** native library and had already
fixed it — on Windows only:

```rust
#[cfg(windows)]
// Conscrypt's extracted OpenJDK JNI DLL uses the same unsafe
// RegisterNatives-on-load pattern as tcnative on CratonVM.
let skip_jni_onload_tcnative =
    basename_lc.contains("tcnative") || basename_lc.contains("conscrypt_openjdk_jni");
#[cfg(not(windows))]
let skip_jni_onload_tcnative = false;
```

The comment *names Conscrypt explicitly* as sharing tcnative's problem — our
JNI implementation isn't ABI-complete enough for either library's
`JNI_OnLoad`/`RegisterNatives` sequence — but the `#[cfg(not(windows))]` arm
hard-codes `false`, so on Linux (and any other non-Windows target)
`JNI_OnLoad` was still invoked for `libconscrypt_openjdk_jni*.so`, and it
still isn't ABI-safe there. This is not a new defect, just an old one that
was only ever worked around for one platform. The bug had presumably gone
unnoticed on Linux until the first full Spring Boot suite run actually
executed there (`RESULTS-20260802-azure-fullsuite.md`'s own header: "First
full-suite run on the Azure Linux host").

## Fix

Removed the `#[cfg(windows)]` / `#[cfg(not(windows))]` split; the same
basename check now applies on every platform. Conscrypt's Java-side entry
points it actually needs are already covered by
`register_conscrypt_native_bridges` in `native-builtins/src/tls.rs`
(`org/conscrypt/NativeCrypto.clinit` is a no-op, `get_cipher_names` and
`EVP_has_aes_hardware` are Rust natives) — Jetty only needs a provider
object that can advertise its ALPN processor while building a connector;
the real TLS engine is CratonVM's own `t27_tls.rs` surface regardless of
whether Conscrypt's native library ever finishes initializing.

## Validation

Azure host, real JDK 25 (`/data/jdk25-real-20260717/jdk-25.0.3+9`), fresh
worktree/binary off `origin/dev`, JIT on, one process per class:

| run | result |
|---|---|
| before fix, 1 run | CRASH, 1.0-1.7s (SIGABRT, reproduces every time) |
| after fix, 4 runs | **PASS 6/6 tests, 0 failed**, 1.4-2.9s each |

Sanity check on the same fixed binary, same module (SSL/native-adjacent,
already known clean on current `dev`):
`JettyServletWebServerFactoryTests` — **PASS**, 351.4s, no regression.

Unit tests: `cratonvm-vm --lib` (debug profile — the shared host OOM-killed
a `--release` LTO test build under concurrent load, a host artifact, not a
test failure) — **2378 passed, 0 failed, 111 ignored**.

## Affected classes

- `module/spring-boot-jetty` — `org.springframework.boot.jetty.SslServerCustomizerTests`

Any other native library matching `tcnative` or `conscrypt_openjdk_jni` in
its filename is affected the same way on non-Windows platforms; none other
were observed crashing in the available suite results, but the fix is not
scoped to this one class.
