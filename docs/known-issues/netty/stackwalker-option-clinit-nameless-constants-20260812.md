# `StackWalker$Option.<clinit>` fabricated nameless constants — every Mockito-backed test failed

**Status:** FIXED (2026-08-12). The defect was found and fixed twice the same
day by two sessions working different pages of
[investigate-INDEX.md](investigate-INDEX.md), from opposite ends of the netty
suite: `67c5e048c` (batches 12/13, `io.netty.util`) landed the fix; this page
is the batch-07 (`io.netty.handler.codec.http2`) investigation, which reached
the same `native_option_clinit` independently and contributes the cross-page
blast-radius measurement below plus two hardening follow-ups.

## Symptom

All 15 classes on [investigate-batch-07.md](investigate-batch-07.md) failed on
CratonVM and passed on stock HotSpot (JDK 25, same classpath). The per-class
shape was `ok=0, failed=<all>` — a whole class dying, not scattered assertion
failures:

```
Http2ConnectionHandlerTest        found=44 ok=0 failed=44
Http2MultiplexCodecTest           found=63 ok=0 failed=63
DefaultHttp2FrameWriterTest       found=13 ok=0 failed=13
...
```

Each log carried one `ExceptionInInitializerError` followed by N−1
`NoClassDefFoundError: org/mockito/internal/debugging/Java9PlusLocationImpl`
(the JVM's standard "this class already failed to initialise" follow-up). The
VM named the underlying cause itself:

```
WARN <clinit> failed — wrapping in ExceptionInInitializerError
     class=org/mockito/internal/debugging/Java9PlusLocationImpl
     cause=java/lang/IllegalArgumentException
           No enum constant java.lang.StackWalker.Option.SHOW_REFLECT_FRAMES
```

## Root cause

`native-builtins/src/stack_walker.rs::native_option_clinit` substitutes the
real `java.lang.StackWalker$Option.<clinit>` (it is registered as a native
`<clinit>`, and `classloading/src/class_manager.rs` injects the matching method
declaration). It fabricated the constants like this:

```rust
for name in ["RETAIN_CLASS_REFERENCE", "SHOW_HIDDEN_FRAMES", "SHOW_REFLECT_FRAMES"] {
    let option = try_alloc_concurrent_synthetic(ctx, class_name, 0)?;   // ← ZERO fields
    ctx.set_static_field_by_name(class_name, name, Value::Object(Some(option)));
    values.push(option);
}
```

Three defects, in descending order of damage:

1. **Zero fields — so every constant's `Enum.name()` was `null`.** The
   constants were allocated with no slots, so the `name`/`ordinal` state that
   `java.lang.Enum` declares was never written. This is not cosmetic: the real
   `Enum.valueOf(Class, String)` bytecode resolves through
   `Class.enumConstantDirectory()`, whose lookup map is keyed by
   `constant.name()`. A null name means **no name ever matches**, so
   `Enum.valueOf(StackWalker.Option.class, <anything>)` threw
   `IllegalArgumentException: No enum constant ...` for constants sitting right
   there in `$VALUES`.
2. **`DROP_METHOD_INFO` was missing.** JDK 22 added it; the hard-coded triple
   left `Option.values()` one short (3 vs 4) on every JDK ≥ 22.
3. **Wrong ordinal order.** The list put `SHOW_HIDDEN_FRAMES` before
   `SHOW_REFLECT_FRAMES`; every JDK since 9 declares them the other way round,
   so the two constants' ordinals were swapped.

Mockito 4.11 (what the netty fixture resolves) hits defect 1 directly.
`Java9PlusLocationImpl.<clinit>` builds its `StackWalker` reflectively so it can
compile against Java 8:

```java
Class optionClazz = Class.forName("java.lang.StackWalker$Option");
...
Set opts = Collections.singleton(Enum.valueOf(optionClazz, "SHOW_REFLECT_FRAMES"));
```

That `Enum.valueOf` is the throw site. `LocationFactory` calls
`Java9PlusLocationImpl` on **every mock interception**, so the failure is not
confined to tests that touch stack walking — it takes out every test that
touches a Mockito mock at all.

Note the shape of the miss: this is *not* a native shadowing a working real-JDK
class for no reason. The real class file was loaded and correct — a reflection
dump showed all four constants and `$VALUES` — and only its `<clinit>` was
substituted. So every reflective *metadata* query answered correctly while
every *value* query answered wrong, which is why the failure surfaced four
layers from its cause, and why a probe that only interrogated the `Class`
object would have reported no defect:

| query | HotSpot JDK 25 | CratonVM (before) | CratonVM (after) |
|---|---|---|---|
| `Class.forName("...$Option").getDeclaredFields()` | 4 + `$VALUES` | 4 + `$VALUES` | 4 + `$VALUES` |
| `Option.values().length` | 4 | 3 | 4 |
| `values()[0].name()` | `RETAIN_CLASS_REFERENCE` | `null` | `RETAIN_CLASS_REFERENCE` |
| `values()[1].name()` | `DROP_METHOD_INFO` | `null` | `DROP_METHOD_INFO` |
| `values()[2].name()` | `SHOW_REFLECT_FRAMES` | `null` | `SHOW_REFLECT_FRAMES` |
| `Enum.valueOf(Option.class,"SHOW_REFLECT_FRAMES")` | ok | `IllegalArgumentException` | ok |

## Fix

`native_option_clinit` now reads the constant names off the **loaded class**
(`option_constant_names` — the static fields whose descriptor is the enum type
itself, which excludes `$VALUES`), so the set and its order come from whichever
JDK is running instead of a hard-coded three; and it allocates each constant
with `name`/`ordinal` populated.

Two hardening follow-ups from this investigation:

* **The constants are rooted in a `NativeHandleScope` for the whole clinit**,
  not only across their own `create_string`. The per-constant pin covers one
  allocation inside an iteration, but every later iteration allocates two more
  objects (the next constant, its name `String`) and `new_ref_array` allocates
  a third, with the earlier constants live only as bare `ObjectRef`s in a
  `Vec`. One moving young collection in any of those and `$VALUES` is filled
  with vacated from-space addresses.
* **The unit test now asserts `name` and `ordinal`.** The original asserted
  only that `$VALUES[i]` was the same object as the i-th static field — true
  for *any* instances the native invents — and froze the three-name list *in
  its own wrong order* as the expected output, so it stayed green throughout
  the bug. It is now a shared assertion over `$VALUES` length, ordering, and
  each constant's name/ordinal, instantiated for both the JDK 22+
  four-constant class and the pre-22 three-constant one.

  The test also declares Enum's `name`/`ordinal` as *instance* fields on the
  mocked class. Without that the mock resolves those names through a generic
  fallback table that places them arbitrarily, and the assertions would have
  been measuring the mock rather than the native.

  Checked against two mutants to confirm it can actually fail: dropping the
  `name`/`ordinal` writes (the original bug) and hard-coding the old
  three-name list in the old order both make it FAIL; restoring makes it pass.

## Result

All 15 batch-07 classes pass, matching HotSpot test-for-test — 343 tests, 0
failures, including the same 2 assumption-aborts HotSpot reports in
`Http2FrameCodecTest`:

```
DefaultHttp2FrameWriterTest                 found=13 ok=13 failed=0
DefaultHttp2LocalFlowControllerTest         found=19 ok=19 failed=0
HpackDecoderTest                            found=58 ok=58 failed=0
HpackEncoderTest                            found=10 ok=10 failed=0
Http2ConnectionHandlerTest                  found=44 ok=44 failed=0
Http2ConnectionRoundtripTest                found=21 ok=21 failed=0
Http2ControlFrameLimitEncoderTest           found=5  ok=5  failed=0
Http2EmptyDataFrameConnectionDecoderTest    found=2  ok=2  failed=0
Http2EmptyDataFrameListenerTest             found=5  ok=5  failed=0
Http2FrameCodecTest                         found=43 ok=41 failed=0 aborted=2
Http2FrameRoundtripTest                     found=28 ok=28 failed=0
Http2MaxRstFrameConnectionDecoderTest       found=2  ok=2  failed=0
Http2MaxRstFrameLimitEncoderTest            found=28 ok=28 failed=0
Http2MaxRstFrameListenerTest                found=2  ok=2  failed=0
Http2MultiplexCodecTest                     found=63 ok=63 failed=0
```

## Blast radius — measured

A 62-class netty slice (every 12th class of `testlist.txt`), run
ABBA-interleaved against a pristine-dev build, bounds how far this one bug
reaches. **Every difference is an improvement; there are no regressions.** Five
of the seven changed classes are on *other* batch pages:

| class | dev baseline | with fix | owning page |
|---|---|---|---|
| `DefaultHttp2LocalFlowControllerTest` | ok=0 failed=19 | ok=19 failed=0 | batch 07 |
| `Http2ConnectionHandlerTest` | ok=0 failed=44 | ok=44 failed=0 | batch 07 |
| `StreamBufferingEncoderTest` | ok=0 failed=23 | ok=23 failed=0 | batch 08 |
| `PromiseCombinerTest` | ok=2 failed=10 | ok=12 failed=0 | other |
| `HttpProxyHandlerTest` | ok=2 failed=13 | ok=15 failed=0 | batch 09 |
| `ReadOnlyByteBufTest` | ok=25 failed=2 | ok=27 failed=0 | batch 02 |
| `LittleEndianCompositeByteBufTest` | ok=482 failed=5 | ok=484 failed=3 | batch 01 |

The last row is a partial repair — that class has residual failures unrelated
to this bug. **Re-run the remaining batch pages against a build carrying this
fix before investigating them.** A slice this thin turning up five extra
repaired classes means the FAIL counts on those pages are inflated by this one
cause; the two sessions that hit it independently (batches 07 and 12/13) are
themselves evidence of how broadly it spread.

## Repro (Linux host)

```bash
cd /data/cratonvm/apps/netty-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv-bin> --java-home "$JAVA_HOME" --Xmx 1500m \
    @common.args -Dcraton.batch=1 CratonRunner \
    io.netty.handler.codec.http2.Http2EmptyDataFrameConnectionDecoderTest
```
