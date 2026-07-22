# `LockTest.testFindWithPessimisticWriteLockTimeoutException` 5-second regression — fixed

| | |
|---|---|
| **Status** | Fixed and moved from `docs/known-issues` on 2026-07-22. |
| **Affected test** | `org.hibernate.orm.test.jpa.lock.LockTest.testFindWithPessimisticWriteLockTimeoutException` (and the complete `LockTest` class). |
| **Root cause** | The cold `java.lang.StringUTF16.getChars(byte[], int, int, char[], int)` bytecode loop copied compact UTF-16 values one code unit at a time during Hibernate metamodel/bootstrap work inside the method's five-second timeout. It was CPU-bound interpreter work, not H2 no-wait locking, GC root scanning, or JIT compilation latency. |
| **Fix** | Register a bulk Rust native for `StringUTF16.getChars`, then force it in both the interpreter cached-dispatch gate and `vm_exec` cold-dispatch gate. |

## Diagnosis

The original issue reproduced 8/8 times, including on a quiet host. Targeted phase instrumentation showed that entity persistence consumed almost all of the timeout budget while the nested H2 `PESSIMISTIC_WRITE` / no-wait operation itself took roughly 130 ms. CPU sampling restricted to the timed test's stack showed `java/lang/StringUTF16.getChars` as the dominant frame.

OpenJDK implements this method as a Java loop over every UTF-16 code unit. Hibernate invokes it heavily while materializing annotation and proxy metadata; the cold call sites therefore remain in the interpreter long enough to exceed the test's fixed timeout.

## Implementation details

`native-builtins/src/lang_string.rs` now bulk-reads the compact UTF-16 byte array and writes decoded code units into the destination `char[]`. The native preserves the JDK method's observable edge behavior:

- an empty/reversed source range returns without dereferencing either array;
- source validation happens before destination dereference;
- source errors use `StringIndexOutOfBoundsException` and destination errors use `ArrayIndexOutOfBoundsException`;
- subtraction follows JVM wrapping `int` arithmetic before bounds validation.

The method is explicitly selected in both dispatch paths. Registering the native alone is insufficient because real-JDK bytecode can otherwise win on a cold path or after a vtable call site becomes cached.

## Verification

Focused Rust coverage passed:

```text
cargo test -p cratonvm-native-builtins string_utf16 --lib -- --nocapture
# 5 passed

cargo test -p cratonvm-vm string_utf16_get_chars_force_native_covers_cached_dispatch --lib -- --nocapture
# 1 passed
```

The release binary was built in the task-specific target directory and copied as `cratonvm-hib-locktimeout-utf16-20260722-019f8a10.exe`:

```text
SHA-256  7A1F816E5048CCEEED9278D017DA7A45BD077E7FF33E61204BE77A6430EB6195
```

Using `apps/hib-suite-runner`, with a fresh process for each run:

```text
<binary> --java-home <JDK 25> --Xmx 1500m @common.args -Dcraton.batch=1 CratonRunner passed.txt 2266
<binary> --nojit --java-home <JDK 25> --Xmx 1500m @common.args -Dcraton.batch=1 CratonRunner passed.txt 2266
```

| Mode | Runs | Result |
|---|---:|---|
| JIT | 3/3 | `LockTest`: `found=23 started=15 ok=15 failed=0 aborted=0 skipped=8` (10028–10979 ms full class) |
| `--nojit` | 3/3 | `LockTest`: `found=23 started=15 ok=15 failed=0 aborted=0 skipped=8` (11291–12799 ms full class) |

The former timeout method and all structurally similar sibling lock tests are covered by those complete-class runs. No residual failure from this issue remains.
