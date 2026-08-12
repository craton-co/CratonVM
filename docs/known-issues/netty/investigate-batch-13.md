# netty — investigate batch 13 of 13

**RESOLVED 2026-08-12 — all 4 classes now pass.** Single root cause, fixed on
`fix/netty-util-batch1213-20260812`: CratonVM's native `<clinit>` for
`java.lang.StackWalker$Option` built nameless enum constants, so
`Enum.valueOf` threw `IllegalArgumentException: No enum constant` for **every**
name. Mockito's `Java9PlusLocationImpl.<clinit>` does exactly that lookup, died
with `ExceptionInInitializerError`, and took every Mockito-based test in these
classes with it. See "Resolution" below.

Part of a 184-class FAIL/HANG list split across 13 pages (see [investigate-INDEX.md](investigate-INDEX.md)) so work doesn't overlap. This page owns exactly the 4 classes below — do not touch classes listed in other batch pages.

Found during the full 657-class, 3-GC-variant (default/G1/ZGC) suite run on Windows (binary built from an isolated worktree at commit `70c8b8cd6`). "status seen" reflects what each GC variant's run actually recorded — a class can be `FAIL` in one variant and `HANG` in another (shown as `FAIL/HANG` in the status column); that's raw data, not yet explained. Cross-check against stock HotSpot (`--hotspot` flag) before concluding anything is CratonVM-specific — the already-confirmed CratonVM bugs (JNI-native-codec SIGSEGV, buffer-test throughput gap) are documented separately in `docs/known-issues/netty/jni-native-codec-sigsegv-20260812.md`; the classes on these pages are NOT yet confirmed to be CratonVM defects.

## Classes

| class | status seen | GC variant(s) | after fix (Linux, JDK 25) |
|---|---|---|---|
| `io.netty.util.internal.logging.CommonsLoggerTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL | **PASS 15/15** (was 0/15) |
| `io.netty.util.internal.logging.InternalLoggerFactoryTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL | **PASS 17/17** (was 0/17) |
| `io.netty.util.internal.logging.Slf4JLoggerFactoryTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL | **PASS 4/4** (was 1/4) |
| `io.netty.util.internal.logging.Slf4JLoggerTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL | **PASS 15/15** (was 0/15) |

## Resolution

`native-builtins/src/stack_walker.rs::native_option_clinit` allocated the
`StackWalker.Option` constants with `alloc_concurrent_synthetic(ctx, class, 0)`
— zero instance slots — and never wrote `Enum.name`/`ordinal`. It also built
only three constants, while JDK 25 declares four. Observed against HotSpot:

| | HotSpot JDK 25 | CratonVM (before) |
|---|---|---|
| `Option.values().length` | 4 | 3 |
| `name()` | `RETAIN_CLASS_REFERENCE`, … | `null` (all) |
| `ordinal()` | 0, 1, 2, 3 | 0, 0, 0 |
| `valueOf("SHOW_REFLECT_FRAMES")` | the constant | `IllegalArgumentException` |
| `Option.SHOW_REFLECT_FRAMES` | the constant | `null` |

The real JDK class *was* loaded (CratonVM reported all four declared fields plus
`$VALUES`, identical to HotSpot) — the native `<clinit>` overrode the real one
and under-populated it.

Fixed by reading the constant list from the loaded class's declared static
fields (so it tracks whichever JDK is in use — `DROP_METHOD_INFO` only exists
from JDK 22), allocating each with the `java.lang.Enum` `(name, ordinal)`
layout, and building `$VALUES` in declaration order. CratonVM now matches
HotSpot on all six checks above.

Note `InternalLoggerFactoryTest` **fails 6/17 on stock HotSpot JDK 25** on this
host (test-ordering interaction with Mockito's static default-factory reset);
CratonVM now passes 17/17, i.e. at least as good as the oracle. Do not treat
HotSpot's 11/17 as the target.

## Repro

```bash
cd apps/netty-suite-runner
echo <ClassName> > /tmp/one.txt
CV_BIN=bin/cratonvm-netty-default.exe bash run-netty-suite.sh --list /tmp/one.txt --gc default --shards 1 --timeout 180 --out /tmp/repro
# swap --gc default for g1 / zgc to match the variant(s) that showed the failure
# HotSpot cross-check: bash run-netty-suite.sh --list /tmp/one.txt --hotspot --shards 1 --timeout 180 --out /tmp/repro-hs
```

