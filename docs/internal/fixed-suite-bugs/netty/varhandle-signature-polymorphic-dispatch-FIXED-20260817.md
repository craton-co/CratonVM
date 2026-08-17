# `VarHandle` access modes cost 2 µs each — a signature-polymorphic site the JIT's native cache could never resolve

**Status: FIXED 2026-08-17** on `perf/netty-per-call-inline-20260817`
(branched from `dev` `cf141b8a8`), measured on the Azure Linux host
`20.80.105.49`, JDK 25 as the oracle. Regression coverage:
`regression-suite/src/RVarHandleAccess.java`.

`VarHandle.get` on an ordinary `int` field measured **1 979 ns** on CratonVM
against **0.3 ns** on HotSpot 25 and **3.0 ns** for the same field read written
as `obj.field` *in the same VM*. It is now **155 ns**.

That single defect is most of what three `docs/known-issues/netty` pages
described as a "VM-wide per-call cost ceiling". It is not per-call cost. It is
one call shape that could not be cached.

## 1. How it reaches netty — every `ByteBuf` accessor, twice

`AbstractByteBuf.writeByte(int)` is three lines:

```java
public ByteBuf writeByte(int value) {
    ensureWritable0(1);
    _setByte(writerIndex++, value);
    return this;
}
```

`ensureWritable0` calls `ensureAccessible()` and `capacity()`, and
`UnpooledHeapByteBuf.capacity()` calls `ensureAccessible()` again. Each of
those is

`ensureAccessible()` → `isAccessible()` → `RefCnt.isLiveNonVolatile(refCnt)`
→ `VarHandleRefCnt.isLiveNonVolatile` → `VH.get(instance)`.

netty picks the `VarHandle` refcount implementation because
`PlatformDependent.hasUnsafe()` is **false** on JDK 25 — on HotSpot too, so
this is not a CratonVM-specific configuration. It means **two `VarHandle.get`
calls per `ByteBuf.writeByte`**, and at least one on essentially every other
`ByteBuf` accessor.

Two 2 µs reads is the whole of a `writeByte`:

| `ByteBuf.writeByte` into a 64 KiB heap buffer | ns/iteration |
| --- | ---: |
| HotSpot 25, C2 | 2.2 |
| HotSpot 25, `-Xint` | ~2 030 |
| CratonVM, before | 2 440–2 578 |
| CratonVM, after | **368–654** |

## 2. The defect

`VarHandle.get` is signature-polymorphic (JVMS §5.4.3.4). The call site names
its own descriptor — `invokevirtual java/lang/invoke/VarHandle.get:(LRefCnt;)Z`
— while the native is registered under the erased
`([Ljava/lang/Object;)Ljava/lang/Object;`. **No lookup by the call-site triple
can ever find it.**

`vm_exec::invoke_on_class_shared_inner` knows this and re-probes with the three
erased descriptors — but only at the very bottom of its cascade, in the `None`
arm of hierarchy resolution. Nothing above it caches the answer:

* the JIT's per-call-site native cache
  (`jit::helpers::resolve_native_owner_for_receiver`) asked for the triple,
  missed, and recorded refusal reason 4, *"no native for the triple on the
  receiver or its supers"* — a **negative cached forever**;
* so every call fell through to `jit_invoke_virtual_mic`, which first ran
  `try_jit_compile_callee` (a registered native can never compile — it returned
  `None` every time) and then `invoke_or_native`'s full gate cascade, a
  class-manager read lock, a `format!` of the class name,
  `find_method_recursive`, and up to nine more registry probes.

`CRATONVM_DBG=mic-prof` states it exactly. On 600 k `VH.get` calls:

```
mic_calls=505971  hit_entry=0  hit_noentry=499464  miss=36
pub_probe_none=499464 (probe_returned_none=499464)
cyc_invoke=2114594606  cyc_compile_probe=389124684
```

`hit_entry=0` with `hit_noentry` climbing is the signature the counter's own
doc comment names: *"the slot never learns a target, so every call re-pays the
compile probe."*

A flat `perf record` over the microbenchmark agrees, and shows there is no
single hot site to fix — the cost is the cascade itself:

| share | family |
| ---: | --- |
| ~21% | native-registry probing (`find` 10.7, `find_with_kind` 2.8, `memcmp` 3.6, `slot_index_for_key` 2.1, `resolve_id_with_descriptor_quirks` 1.9) |
| ~16% | `invoke_or_native` / `invoke_on_class_shared_inner` / `find_method_recursive` |
| 3.7% | `try_jit_compile_callee` — the futile compile probe |
| ~8% | wrapper allocation for the erased `Object` return |
| ~2% | `varhandle_get` — the native that does the actual work |

## 3. The fix, in two parts

### 3.1 Rule 4 — the site cache learns the erased registration

`resolve_native_owner_for_receiver` gains a fourth rule after its
receiver/bytecode/superclass walk fails: if the receiver is a `VarHandle` and
the method name is signature-polymorphic, re-probe with
`SIGNATURE_POLYMORPHIC_NATIVE_DESCRIPTORS` (base class first, then the exact
receiver class), reproducing `invoke_on_class_shared_inner`'s own order. The
predicate, the name list and the descriptor list are now `pub(crate)` in
`vm_exec` and shared by both, so they cannot drift.

The entry carries `poly: true`; the dispatch side then runs
`unbox_poly_return_checked` — not `coerce_native_return` — so the erased
`Object` is unboxed against the call site's descriptor and the W6-1
`WrongMethodTypeException` rule still fires.

`MethodHandle.invoke` / `invokeExact` / `invokeBasic` are **not** included:
`site_name_is_special_cased` refuses them one level above, and this change does
not widen that.

**1 979 ns → 595 ns.**

### 3.2 A `VarHandle` instance-field read is a field load

With the site resolved, the remaining cost was the shape of the call, not the
work: build a `NativeContextImpl`, record a thread transition, run
`varhandle_get`'s access-shape cascade, **allocate a wrapper** for the erased
return, then unbox it straight back out and discard the wrapper.

`jit::helpers::try_varhandle_instance_field_read` serves the read modes
(`get`, `getVolatile`, `getOpaque`, `getAcquire`) directly: look the handle up
in the same GC-stable side table `vh_meta_get` uses
(`lang_invoke::varhandle_instance_field_plan`), and if it names a resolved
INSTANCE field slot, read it with `get_field_as` against the descriptor the
handle already carries.

It refuses — and falls through to the funnel — for every other shape: static,
array-element, byte-array and `ByteBuffer` view handles, `SegmentVarHandle`, a
handle whose slot is not resolved yet (the native resolves and memoises it, so
the *second* call qualifies), and any primitive/reference mismatch between the
variable and the call site, which is precisely the shape
`unbox_poly_return_checked` turns into `WrongMethodTypeException`.

**595 ns → 155 ns.**

### 3.3 One adjacent defect the same profile exposed

`MessageDigest.update(byte[], off, len)` read the array **one element at a
time** through `ctx.get_array_element` — a `Value`-returning virtual call into
the collector per byte. `ZgcRealHeap::get_array_element` plus its
`NativeContextImpl` wrapper were 16% of that microbenchmark's profile. The
`NativeContext` trait has had bulk `read_byte_array_into` /
`write_byte_array_from` (VM override: one `copy_nonoverlapping`) all along, and
`registry.rs`'s own comment names these callers as the migration that was left
undone. Migrated, plus `class_name_arc_of_id` in place of
`class_name_of_id` (which copied the class name into a fresh `String` on every
`update`).

`update(byte[],0,64)`: **1 519–1 529 ns → 315–320 ns**, interleaved, twice.

## 4. What it is worth, measured

Microbenchmarks, `regression-suite`-style loops, ABBA-interleaved on one host
(`probes` on the host under `/data/nperf-probe`):

| | before | after | ratio |
| --- | ---: | ---: | ---: |
| `VarHandle.get` (int field) | 1 979 ns | **155 ns** | 12.8× |
| `VarHandle.getAcquire` | 1 991 ns | 154 ns | 12.9× |
| netty `ByteBuf.writeByte` | 2 440 / 2 578 ns | **368 / 654 ns** | 6.6× / 4.0× |
| `MessageDigest.update(byte[],0,64)` | 1 519–1 529 ns | 315–320 ns | 4.8× |
| plain `obj.field` read (control) | 3.0 ns | 3.0 ns | — |
| user-written 5-deep call chain (control) | 147 ns | 147 ns | — |

The two controls are the point: nothing generic got faster. One call shape did.

`writeByte` carries two figures because it was measured twice — interleaved
against the `base` arm both times, once at host load ~9 and once at ~20. The
"after" arm degrades more under load than the "before" arm does, so **quote the
pair, not one ratio**. `MessageDigest.update(byte)`, the single-byte overload,
is *not* in this table: it sits at the ordinary native-dispatch floor
(~170-210 ns before and after) and this change does not touch it.

Class-level, whole suite classes, same host, `-Xmx 1500m`, no per-method cap so
the class completes rather than being clipped. **Read the CPU column**: this box
carried unrelated load of 8-35 throughout and its wall clock is worth ±20% at
best (`performance/vm-per-call-dispatch-cost-RETIRED-20260817.md` §6). Every
class passes exactly what it passed before.

| class | tests | before wall / CPU | after wall / CPU | ratio (CPU) |
| --- | ---: | ---: | ---: | ---: |
| `JdkZlibIntegrationTest#testHugeDecompress`, solo | 1/1 | 1 063 s / 1 023 s | **455–528 s / 453–517 s** | ~2.1× |
| `AdaptiveByteBufAllocatorTest` | 127/127 | 635–725 s / 638–727 s | **419–432 s / 421–435 s** | 1.51–1.67× (n=2 pairs) |
| `AdaptiveByteBufAllocatorGrowthTest` | 400/400 | 1 081 s / 2 963 s | **645 s / 2 042 s** | 1.45× |
| `AdaptiveByteBufAllocatorUseCacheForNonEventLoopThreadsTest` | 128/128 | 752 s / 754 s | **435 s / 432 s** | 1.75× |
| `search.SearchProcessorTest` | 15/15 | 153 s / 141 s | **131–139 s / 130–135 s** | ~1.07× |
| `BigEndianHeapByteBufTest` | 414/414 | 37.4 s / 56.5 s | 38.4 s / **48.5 s** | 1.17× |

The last row is the useful negative: a class whose wall is dominated by JUnit
discovery rather than buffer work moves 14% in CPU and not at all in wall. The
fix is worth what the workload's `VarHandle` share is, and nothing more.

**The `AdaptiveByteBufAllocatorTest` row is the second useful negative, and it
is about method.** An un-paired run of it on the fixed binary returned 270 s,
which reads as 2.6×. Interleaved against `base` on the same host it is 432 s,
with 419, 426 and 475 s on further runs against a `base` of 725 s and 635 s —
i.e. the 270 s was the box. The microbenchmark rows above are ratios of 3 ns-to-2 µs quantities and survive
this box; the class rows are minutes and do not. Every class row here is a
`base`/`after` pair run adjacently, and where it is a range, that is the
observed spread, not a choice.

## 5. What this does NOT close

The three pages this came out of describe classes that exceed the netty suite's
**180 s per-class wall cap**. This fix moves every one of them a long way and
takes none of them under that cap. Their residual is the ordinary
native-dispatch floor — ~150-210 ns per registered native call from compiled
code, of which `MessageDigest.update(byte)` is the clean specimen at
~170-210 ns against HotSpot's 6.4 ns, before and after this change.

That floor is `performance/vm-per-call-dispatch-cost-RETIRED-20260817.md`,
**reopened on 2026-08-17 for exactly this reason**: it had been retired as "a
characterisation, not an open defect", and it is now the named cause of live
suite failures. Its own conclusion about its §2 lever 2 — that
one-lookup-per-invoke is worth ~1.5% of CPU on a box whose measurement floor is
~5%, and so is not worth building — is unchanged and not disputed here.

That page's §3 warning is worth restating beside this record, because this fix
is the counter-example it did not have: **a percentage in a flat profile of
this VM is a lead, not a quantity** — but a *missing cache* is not a
percentage, and this one was worth 12.8×. The way to tell them apart is a
counter, not a profile. `hit_entry=0` was the counter.

## 6. Repro

```bash
# the microbenchmarks
<cratonvm> --java-home <jdk25> --Xmx 2000m -cp <netty-cp>:. VhProbe 200000
<cratonvm> --java-home <jdk25> --Xmx 2000m -cp <netty-cp>:. NperfProbe 200000 3 netty
<cratonvm> --java-home <jdk25> --Xmx 1500m -cp . MdProbe 200000

# the counter that names the defect (before) and the fix (after)
CRATONVM_DBG=mic-prof       <cratonvm> … VhProbe 100000   # hit_entry=0 == broken
CRATONVM_DBG=intrinsic-stats <cratonvm> … VhProbe 200000  # "VarHandle instance-field reads served directly"

# the regression class, which must agree with HotSpot exactly
<jdk25>/bin/java -cp regression-suite/build RVarHandleAccess     # RVarHandleAccess OK
<cratonvm> --java-home <jdk25> -cp regression-suite/build RVarHandleAccess
```

`RVarHandleAccess` is not a vacuous green: on the fixed binary
`CRATONVM_DBG=intrinsic-stats` reports **1 122 002** of its reads served by the
new direct path, so the test provably exercises what it covers.

## Related

* `fixed-suite-bugs/netty/compression-testhugedecompress-shared-timeout-20260816.md` — the eleven `codec.compression` classes this was found from.
* `fixed-suite-bugs/netty/adaptivebytebufallocator-searchprocessor-180s-wall-20260816.md` — the four `io.netty.buffer` classes, and the throughput ceiling this fix partly explains and partly does not.
* `performance/vm-per-call-dispatch-cost-RETIRED-20260817.md` — the residual, and the measurement hygiene this record follows.
* `performance/netty-per-call-throughput-20260813.md` — the earlier measurement record whose "826 M calls at a few hundred ns each" arithmetic was right about the *count* and wrong about the *cause* for the `VarHandle` share of it.
