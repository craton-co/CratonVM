# `LockTest.testFindWithPessimisticWriteLockTimeoutException` 5-second regression — fixed

| | |
|---|---|
| **Status** | Fixed and moved from `../../../known-issues` on 2026-07-22. |
| **Affected test** | `org.hibernate.orm.test.jpa.lock.LockTest.testFindWithPessimisticWriteLockTimeoutException` (and the complete `LockTest` class). |
| **Root cause** | The cold `java.lang.StringUTF16.getChars(byte[], int, int, char[], int)` bytecode loop copied compact UTF-16 values one code unit at a time during Hibernate metamodel/bootstrap work inside the method's five-second timeout. It was CPU-bound interpreter work, not H2 no-wait locking, GC root scanning, or JIT compilation latency. |
| **Fix** | Register a bulk Rust native for `StringUTF16.getChars`, then force it in both the interpreter cached-dispatch gate and `vm_exec` cold-dispatch gate. |

## Diagnosis

The original issue reproduced 8/8 times, including on a quiet host. Targeted phase instrumentation showed that entity persistence consumed almost all of the timeout budget while the nested H2 `PESSIMISTIC_WRITE` / no-wait operation itself took roughly 130 ms. CPU sampling restricted to the timed test's stack showed `java/lang/StringUTF16.getChars` as the dominant frame.

OpenJDK implements this method as a Java loop over every UTF-16 code unit. Hibernate invokes it heavily while materializing annotation and proxy metadata; the cold call sites therefore remain in the interpreter long enough to exceed the test's fixed timeout.

## Implementation details

`../../../../native-builtins/src/lang_string.rs` now bulk-reads the compact UTF-16 byte array, decodes it into Rust UTF-16 units, and uses the VM's checked `write_char_array_from` memcpy path for the destination `char[]`. The element-by-element write remains only as a fallback for test/mock contexts and heaps that decline the bulk hook. The native preserves the JDK method's observable edge behavior:

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

The final merged release binary was built in the task-specific integration target directory and copied as `cratonvm-hib-locktimeout-final-20260722-019f8a10.exe`:

```text
SHA-256  E678B01D14E0D53CA7525FD978EC0E176991D64EBCF182C20EDF289D2843E607
```

Using `../../../../apps/hib-suite-runner`, with a fresh process for each run:

```text
<binary> --java-home <JDK 25> --Xmx 1500m @common.args -Dcraton.batch=1 CratonRunner passed.txt 2266
<binary> --nojit --java-home <JDK 25> --Xmx 1500m @common.args -Dcraton.batch=1 CratonRunner passed.txt 2266
```

| Mode | Runs | Result |
|---|---:|---|
| JIT | 3/3 initial fresh processes; final merged quiet-core run | `LockTest`: `found=23 started=15 ok=15 failed=0 aborted=0 skipped=8` (initial 10028–10979 ms; final 11732 ms full class) |
| `--nojit` | 3/3 initial fresh processes; final merged quiet-core run | `LockTest`: `found=23 started=15 ok=15 failed=0 aborted=0 skipped=8` (initial 11291–12799 ms; final 13166 ms full class) |

The final quiet-core runs used six otherwise idle logical CPUs while unrelated suites and Rust release builds occupied other cores. The earlier per-element native could pass on a quiet host but still missed the internal timeout during shared-host contention; the bulk destination copy removed that residual and passed both modes under the same moderate load before final integration.

The former timeout method and all structurally similar sibling lock tests are covered by those complete-class runs. No residual failure from this issue remains.

## Recurrence check (2026-07-27): overshoot narrows further to 2869ms, trend continues

A 282-class rerun on `dev` merged through `13055f75c` (worktree
`CratonVM-hib-local-0712`, run `run-20260726-235842-passed`) hit `LockTest`
again:

```
@@FAIL org.hibernate.orm.test.jpa.lock.LockTest :: org.opentest4j.AssertionFailedError:
execution exceeded timeout of 5000 ms by 2869 ms
@@RESULT ... found=23 started=15 ok=14 failed=1 aborted=0 skipped=8 ms=21107
```

This continues the same narrowing trend this doc (and its predecessor
investigations) have tracked since the original regression was filed:

| Reading | Overshoot past the 5000ms internal timeout |
|---|---:|
| Original finding (2026-07-16/21) | 10087ms |
| After an earlier `dev` merge (2026-07-22-ish) | 3290ms |
| This recheck (2026-07-27, `dev@13055f75c`+) | **2869ms** |

The overshoot keeps shrinking release over release, consistent with this
class being dominated by cold-path interpreter/bootstrap cost (the
`StringUTF16.getChars` bulk-native fix above, plus the unrelated conservative-
GC-root-scan fix `f377eb69` referenced in
[`hib-120s-junit-timeout-cluster-20260716.md`](hib-120s-junit-timeout-cluster-20260716.md))
rather than a single discrete regression -- each unrelated throughput/GC fix
landed on `dev` shaves a bit more off the margin, but a small residual
overshoot is still present on this run. This class was not re-run in
isolation this session (no isolated repro was needed to corroborate the
narrowing-trend characterization, which is already well established by prior
sessions' repeated measurements); the suite-run `@@FAIL` line above is taken
at face value consistent with how this doc's prior updates were recorded.
Status remains effectively fixed/narrowing, not reopened -- if a future
session sees the overshoot widen again rather than continue shrinking, that
would be the signal to re-investigate as a genuine regression rather than
residual noise.
