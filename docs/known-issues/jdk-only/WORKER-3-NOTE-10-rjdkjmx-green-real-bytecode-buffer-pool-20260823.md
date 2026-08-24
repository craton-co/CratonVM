# WORKER-3-NOTE-10 — `RJdkJmx` is GREEN: the direct buffer pool is built by real bytecode now

**2026-08-23**, MEASURED on `697754cb6`. This takes resolution 2 of
`WORKER-3-NOTE-9` §2 — *stop intercepting, let real `java.nio` bytecode build the
pool* — and it turned out to be the cheap one.

## 1. `javap` made the decision, not a judgement call

```text
java.nio.Buffer$2.getDirectBufferPool()
   0: getstatic  java/nio/Bits.BUFFER_POOL:Ljdk/internal/misc/VM$BufferPool;
   3: areturn
```

Two instructions returning a **real object real bytecode already builds**. The
native that stood in front of it existed only to hand back a *fabricated*
carrier — and under `--jdk-only` that fabrication is refused by
`class_manager`'s blanket policy, out of
`jdk/internal/misc/VM$BufferPoolsHolder.<clinit>`, where a refusal is not
catchable: it poisons `jdk.internal.misc.VM` for the process and takes the whole
platform MBeanServer with it.

So there was never a trade to make here. **The interception was strictly worse
than nothing**: it required a fabrication the strict contract refuses, in order
to replace a getstatic.

`VM$BufferPoolsHolder.<clinit>` disassembles to `getDirectBufferPool` +
`FileChannelImpl.getMappedBufferPool` + `getSyncMappedBufferPool` — which also
corrects `NOTE-9` §2, where I named the caller `getBufferPool()`. The fatal call
is `getDirectBufferPool()`.

## 2. What was retired

| registration | why |
|---|---|
| `getDirectBufferPool()Ljdk/internal/misc/VM$BufferPool;` on `java/nio/Buffer$2`, `java/nio/Buffer$1`, `jdk/internal/access/JavaNioAccess` | the fatal one; real bytecode is a `getstatic` |
| `getBufferPool()Ljava/lang/management/BufferPoolMXBean;` on `java/nio/Buffer$2` | **`javap -p` says `Buffer$2` declares no such method on JDK 25.** A dead registration — `H25-1`'s population — that has never once executed |

`jnio_get_buffer_pool`, `jnio_get_direct_buffer_pool` and
`alloc_direct_buffer_pool` went with them. `alloc_buffer_pool` and
`alloc_all_buffer_pools` stay: `getPlatformMXBeans` is a different caller, not a
`<clinit>`, and already degrades to an empty list on refusal.

## 3. `RJdkJmx` is green, and byte-identical to HotSpot

```text
cratonvm --jdk-only --java-home <jdk-25> -cp . RJdkJmx   rc=0
diff <hotspot stdout> <cratonvm stdout>                  IDENTICAL
```

All 67 checks, same output as HotSpot 25.

## 4. The thing `9d3f78943` was protecting is BETTER, not preserved

That commit made the pools real because a constant-zero pool "reports a PERFECT
temporary-buffer cache no matter what the VM is doing". Retiring the native
could have reintroduced exactly that, so it was measured rather than assumed —
and `DirectBufferCacheProbe` is not enough on its own, because it asserts a
**delta**, which a frozen counter satisfies as happily as a live one.

`probes/PoolLive.java` allocates eight 1 MiB direct buffers and reads the bean
across them:

| | count | used | `counters_live` | beans |
|---|---|---|---|---|
| HotSpot 25 | 0 → 8 | 0 → 8388608 | **true** | 3 — `mapped`, `direct`, `mapped - 'non-volatile memory'` |
| CratonVM, compatible | 0 → 8 | 0 → 8388608 | **true** | 3, same names, same order |

**Exactly HotSpot**, because the counters are now real `java.nio.Bits` counters
tracking real allocations rather than anything this VM maintains. And
`DirectBufferCacheProbe` still returns `CACHE_WORKING` on both.

## 5. What is still not right, and is not a regression

Under `--jdk-only`, `getPlatformMXBeans(BufferPoolMXBean.class)` answers an
empty list (`PoolLive` prints `direct=ABSENT`), because that call is a
*separate* interception in `jmx.rs` which fabricates its beans and whose
fabrication strict mode refuses. That was true before this change too — it
threw then, and returns empty since `697754cb6`. HotSpot answers 3.

**The fix is now obvious and cheap**, which it was not before: real bytecode can
serve that list, so the `BufferPoolMXBean` arm in `jmx.rs`'s `getPlatformMXBeans`
should delegate to `sun/management/ManagementFactoryHelper.getBufferPoolMXBeans`
rather than fabricate. Left as a follow-up — it is a different registrar and
this note is already at its claim.

## 6. Verification

| arm | result |
|---|---|
| `CRATONVM_ARGS=--jdk-only` | 106/107 — **`RTreeRangeGc`**, the intermittent GC record. **`RJdkJmx` no longer appears** |
| `SUITE=all` | **107/107** |
| `SUITE=core` | **67/67** |

Build clean, zero dead-code warnings after removing the three functions.

## Index rows for `INDEX.md` (H0 to place)

* `WORKER-3-NOTE-10` — `RJdkJmx` GREEN. `getDirectBufferPool` was a native in
  front of a two-instruction `getstatic`, and it needed a fabrication that
  `--jdk-only` refuses inside a `<clinit>` — poisoning `jdk.internal.misc.VM`
  and the whole platform MBeanServer. Retiring it is strictly better than
  keeping it
* `WORKER-3-NOTE-10` §4 — direct-pool observability is now EXACT rather than
  merely preserved: `count 0→8`, `used 0→8388608`, identical to HotSpot,
  because real `java.nio.Bits` counters do the accounting. `DirectBufferCacheProbe`
  asserts a delta and cannot tell a live counter from a frozen one — `PoolLive`
  can
* `WORKER-3-NOTE-10` §2 — `Buffer$2.getBufferPool()` is declared by no JDK 25
  image; retired as part of this, free
* `WORKER-3-NOTE-10` §5 — `getPlatformMXBeans(BufferPoolMXBean.class)` still
  answers empty under `--jdk-only` (a separate `jmx.rs` fabrication); it can now
  delegate to `ManagementFactoryHelper.getBufferPoolMXBeans`
