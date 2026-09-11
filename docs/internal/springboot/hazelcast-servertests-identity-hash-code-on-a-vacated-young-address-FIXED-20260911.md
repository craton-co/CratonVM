# `identityHashCode` on an evacuated young address — `HazelcastAutoConfigurationServerTests`

| | |
|---|---|
| **Status** | **FIXED**, 2026-09-11. Root-caused; the producer is named and the crash no longer reproduces. |
| **Fix** | `native-builtins/src/http_url_connection.rs` — `huc_real_object_url` and `huc_real_perform` now take `this: &mut ObjectRef` and hand the CALLER the post-move address. |
| **Was** | `docs/known-issues/springboot/hazelcast-servertests-generational-identity-hash-code-faults-on-a-vacated-young-address-20260909.md` |
| **Family** | The `BindableTests` stale-`ObjectRef`-across-an-allocation family, [`bindabletests-stale-objectref-family-across-allocation-20260909.md`](bindabletests-stale-objectref-family-across-allocation-20260909.md). Eleventh member, and the first reached through a BLOCKING REGION rather than an allocation. |
| **Supersedes** | the Generational rows of the retired [Hazelcast crash-or-hang page](hazelcast-autoconfiguration-crash-hang-RETIRED-20260909.md). |

## The page said the reader was named exactly and the producer was not. Both are now.

The predecessor symbolised the faulting `pc` on three runs and two binaries and
got `NativeContextImpl::identity_hash_code` every time, faulting at `rsi+8` — the
mark word — on an address inside a span `GenerationalHeap::uncommit_evacuated_young`
had released (`site=unbumped-middle`, *and NOT re-committed since*). It could not
say who put that address in `rsi`, and it said so.

It also named the one measurement that would close it, and why nobody could take
it: `CRATONVM_DBG_VACATED_FRAMES` **dilates this workload about ninefold**, past
the point where the crash window is reachable. Twelve armed runs and 5.5 armed
hours produced no crash and not one `VACATED REGISTER` line.

The way through was the page's own closing suggestion — *a verdict cheap enough
to stay armed* — and the reason there wasn't one already is a gap in an
instrument that looked like it already covered this.

## Why `CRATONVM_DBG_DEADRECV` was silent on the collector it was written for

`deadrecv_check` asks, at `identity_hash_code` and two sibling sites, whether the
receiver is an address this process already freed — **before the first
dereference**, which is the whole point, because this defect's face is a fault on
the read every other consumer performs first.

It asked only the two reclamation RINGS. Both are fed exclusively by a
**non-moving** reclamation:

| ring | fed by |
|---|---|
| `old_freed_lookup_covering` | the old-gen in-place sweep, and mark-compact |
| `young_freed_lookup` | `sweep_young_non_moving`'s `record_young_span_freed` |

A **moving** young cycle writes to neither. It evacuates the semispace and hands
the span straight back to the OS. So on `--XX:UseGc Generational` the guard
answered "live" for exactly the addresses that fault, one instruction before they
faulted.

`gen_heap::young_geometry_span` closes it. It classifies an address against the
semispace geometry `publish_young_geometry` already maintains and **dereferences
nothing** — four relaxed loads and two range compares — which is what lets it
answer on a granule `CRATONVM_GEN_UNCOMMIT` has released. That is not a detail:
`dead_young_ref_reason_global`'s `ZERO-HEADER-IN-YOUNG` arm reads the header, so
it cannot be asked about the released granule this defect's victims are found in.

On its own it says only "a young address". It is a **verdict** because
`deadrecv_check` has already screened through `is_object_address` — itself
commit-bitmap-screened and dereference-free for a released granule — and a live
young reference always names an object start.

Cost, which is the property that mattered: twelve armed arms and twelve unarmed
arms are indistinguishable, 16 Hazelcast lifecycles each. Against ninefold.

## What it caught

```
site="identity_hash_code" obj="0x7631bceb5180" semispace="INACTIVE-SEMISPACE"
  receiver points into RECLAIMED memory … location=young TO-space (the inactive semispace)
  …and NO live heap object holds this address in a decoded reference slot.
     The holder is therefore a frame local, a register, or a native side table.
  java_frames=java/net/URLConnection.getContent()Ljava/lang/Object; pc=4
    <- com/hazelcast/internal/util/phonehome/PhoneHome.postPhoneHomeData(Ljava/lang/String;)V pc=111
    <- PhoneHome.phoneHome(Z)Ljava/util/Map; pc=25
    <- PhoneHome.lambda$start$0()V pc=5
```

The Java stack is the second half of the fix and was added for it: the reclaim
guard proves no live heap object holds the address, which leaves "a frame local,
a register, or a native side table" and says nothing about **which**. The
receiver was on some frame's operand stack one bytecode ago, so the innermost
frames bound where it came from.

`javap -c java.net.URLConnection` puts **pc=4 immediately after `invokevirtual
getInputStream`**:

```text
  0: aload_0
  1: invokevirtual  // Method getInputStream:()Ljava/io/InputStream;
  4: pop
```

so the callee is CratonVM's `huc_get_input_stream`.

## The defect

`huc_get_input_stream` makes two calls that can complete a moving young
collection, and keeps using its bare `this` after both:

* **`huc_real_object_url`** drives `URL.toExternalForm()` — ordinary Java, and it
  allocates.
* **`huc_real_perform`** parks the thread in a **GC-blocking region** for every
  socket wait.

`huc_real_perform` already knew. It pins `this` across its redirect loop, under a
comment naming this precise hazard — *"`perform` parks this thread in a
GC-blocking region for every socket wait … so a moving collection can relocate
`this` mid-exchange"* — and re-reads the forwarded address at the top of each
hop.

What it never did was **tell the caller**. The pin is released at the end, and
the `ObjectRef` the caller passed by value still names the address the collection
moved away from. Every caller then reaches `identity_hash_code(this)` within a
statement or two:

```rust
let status = huc_real_perform(ctx, this, &url_str)?;   // blocking region: a cycle can complete
let key = ctx.identity_hash_code(this);                // reads the mark word at the OLD address
```

On the Generational collector that address is in the semispace the flip emptied,
whose granule `CRATONVM_GEN_UNCOMMIT` has already returned to the OS, so the read
is a **SIGSEGV** and not a wrong answer.

This is the family's signature stated exactly once more: *the object survives —
the caller's `safe_native_call` pin keeps it alive and the collector remaps THAT
pin — but the native's private copy is never rewritten.* Pinning for your own use
fixes nothing while the caller keeps the pre-move copy. That is why an internal
pin and a live defect coexisted in one function.

### Why this workload, and why intermittently

Hazelcast's `PhoneHome` calls a host that does not answer in a test environment,
so `perform` parks in that blocking region for the **whole connect timeout** — a
moving young collection has all of it to complete in. It runs on a scheduled
executor, which is why the victim surfaces around member shutdown rather than
during a test body.

Intermittent because the fault needs two things, not one: the reference must go
stale AND the vacated granule must still be unmapped when it is read. Whenever
the allocator had re-served the span, the same defect read stale bytes and
nothing looked wrong — which is why 36 arms produced the verdict once and no
crash at all before the instrument existed to ask.

## The fix

Both helpers take `this: &mut ObjectRef` and rewrite the caller's copy through
the pin. Twenty-three call sites across twelve natives; the borrow checker is
what makes that exhaustive rather than a list someone kept. The wrapper's pin is
taken BELOW the inner body's own, because `unpin_native_roots(base)` releases its
base and everything above it.

Also fixed, one field away: `huc_get_input_stream` read `maybe_url` out of the
connection's field BEFORE `huc_real_object_url` ran Java, then dispatched
`openStream` on it afterwards. It re-reads the field now.

## Verification

`HazelcastAutoConfigurationServerTests`, Linux x86-64, `--XX:UseGc Generational`,
JIT on, `--Xmx 2g`, real JDK 25, **eight concurrent arms**, armed with
`CRATONVM_DBG_DEADRECV=1 CRATONVM_DBG_DEADREF_STORE=1`.

<!--VERIFY-TABLE-->

Before the fix, the crashing arm is the one that reported the verdict: three hits
on one victim at 16:31:53, then the fault, `site=unbumped-middle` and not
re-committed. That is the page's crash reproduced **with its cause named in the
same log**, which it never had.

### The unarmed rounds that came first

Four six-arm rounds on the pre-fix binary — 36 arms — passed 20/20 with no
SIGSEGV, each reaching **16 Hazelcast lifecycles**, the same depth at which the
predecessor page lost 2 of 6. The page's stated retirement condition was
therefore met before the defect was found, and retiring on it would have been
wrong. The instrument is what turned "the crash stopped happening" into "the
stale read is still there, here it is, and here is who wrote it".

### What does NOT work, recorded so it is not retried

`CRATONVM_DBG_GC_STRESS=16777216` dilates this workload about twentyfold — **1
Hazelcast lifecycle in 28 minutes against 16 in ~45** — and the victim surfaces
at lifecycle ~15 of 16. Raising GC pressure moves the window out of reach rather
than closer, exactly as `CRATONVM_DBG_VACATED_FRAMES` does. Eight-arm unstressed
rounds are the instrument that works on this class.

## A standing audit, from the same shape

`end_blocking_region_refs(&mut [Value])` exists for precisely this hazard — it
rewrites a native's raw `Value` locals through the accumulated blocked-GC fixup —
and `huc_real_perform` used the plain `end_blocking_region()` and an internal pin
instead. Every `begin_blocking_region` in `native-builtins` whose function uses a
heap reference afterwards is the same question; `CRATONVM_DBG_DEADRECV` is the
screen, and it is now cheap enough to leave armed while asking it.

## Repro

```bash
CRATONVM_DBG_DEADRECV=1 CRATONVM_DBG_DEADREF_STORE=1 \
  /data/hzid-nway.sh <cratonvm> Generational tag 8 7200
```

Eight concurrent arms, at least 7200 s each. One arm at a time passes 20/20
indefinitely; the page's own warning that a short budget reports a timeout, not a
hang, still holds — on a loaded host an arm needs 2300–4800 s.
