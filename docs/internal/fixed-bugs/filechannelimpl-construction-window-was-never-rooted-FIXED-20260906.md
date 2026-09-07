# The FileChannelImpl the young sweep zeroed was never rooted in the first place

| | |
|---|---|
| **Status** | FIXED 2026-09-06 — the producer is `native_fcimpl_open`, not the collector. |
| **Scope** | `native-io`'s `sun/nio/ch/FileChannelImpl` construction bridge. Worst under `--XX:UseGc Generational` (the non-moving young sweep ZEROES an unrooted object) but the same window is a stale-local hazard under every moving collector. |
| **Was** | the "The defect" and "The lead" halves of `known-issues/springboot/generational-young-sweep-frees-an-interpreter-held-object-20260906.md` |
| **Still open** | that page's REMAINING half — the Kafka test also fails for a reason none of these arms touched. See "What this does not fix". |

## What it was

`CRATONVM_DBG_SWEEP_ZERO=1` named a live `sun/nio/ch/FileChannelImpl` and its
`sun/nio/ch/NativeThreadSet` zeroed by the young sweep, and its message offered
the standing hypothesis — *"the live ref was a register/native-stack root the
marker missed"*. It was simpler than that: **nothing rooted them at all.**

`native_fcimpl_open` builds the channel by allocating five Java objects in a row
and holding each in a Rust local until a field store publishes it:

```rust
let channel       = new_object_ref(ctx, "sun/nio/ch/FileChannelImpl")?;
let close_lock    = new_object_ref(ctx, "java/lang/Object")?;   // ← can collect
let position_lock = new_object_ref(ctx, "java/lang/Object")?;
let dispatcher    = new_object_ref(ctx, "sun/nio/ch/FileDispatcherImpl")?;
let threads       = new_native_thread_set(ctx)?;                // allocates twice more
ctx.set_field_by_name(channel, "closeLock", ...);               // four allocations later
```

Until that first store nothing in Java refers to `channel`, so a young
collection inside the window finds it unreachable. `new_native_thread_set` has
the same shape one level down — allocate the set, then allocate its `elts`
array, then store. The two classes the sweep-zero probe named are exactly the
two objects this window leaves unrooted.

Under a moving collector that is the ordinary stale-local defect. Under the
Generational collector's NON-MOVING young sweep it is worse: an unreachable
object is not moved, it is **zeroed in place**, and the channel comes back with
`fd` 0 — `FileChannel.map: invalid fd`.

## A SECOND, independent instance of the same window, one layer up

`native_fcimpl_open` is not the only unrooted `FileChannelImpl` construction.
`FileSystemProvider.newFileChannel` — the native `FileChannel.open` dispatches
to, registered in `native-builtins/src/phases_late/nio_file.rs` — had BOTH
halves of the family in one closure, and survived the 34-site sweep in
`a189643cc`:

* it allocated a `java.io.FileDescriptor`, then allocated a path `String`, then
  ran `FileChannelImpl`'s class initializer, and only then passed the
  descriptor into `FileChannelImpl.open` — the same unrooted window this page
  describes, with the same victim class;
* and it read `args.get(2)` (the `Set<OpenOption>`) AFTER `p57_read_path`
  allocated, then dereferenced it twice — the argument-snapshot shape from
  `native-arg-snapshot-stale-across-java-reentry-FIXED-20260906.md`.

### Why the audit missed THIS one, which is a different reason

This page's answer is scope: `native-io/src` was outside the 2026-08-25 run.
That does not explain this site, which is in `native-builtins/src` and so was
inside both that run and `a189643cc`'s.

The reason is structural: it is a **closure registered inside
`register_phase57_nio_file`**, and both `scripts/unpinned-native-local-audit.py`
and `scripts/stale-receiver-audit.py` index by top-level `fn`. Everything in a
13,000-line `register_*` function is attributed to that one name. The audit's
own docs already record the symptom from the other direction — "23 `#[test]`
bodies in lang_string.rs were reported under `register_phase52_string_buffer`" —
but the consequence for REGISTERED NATIVES, which is where most of them live,
was not drawn. Any rule that indexes by `fn` sees a fraction of this tree's
natives.

### A local reproducer, no Kafka and no Azure

`test_classes/gc/NioChannelChurn.java` opens a `FileChannel` in a loop while
background threads allocate. The concurrency is load-bearing: a single-threaded
loop only collects where it itself allocates, and this window is inside the
native — which is why the original report needed a broker and this needs 30
seconds.

```bash
cratonvm --java-home <jdk25> -cp test_classes/gc -Xmx32m     -XX:+UseGenerationalGC NioChannelChurn 300 200 4
```

| arm | before | after |
|---|---|---|
| Generational | 4/5 crash | **0/5** |
| Generational `--nojit` | 5/5 crash | **0/5** |
| `OpenCloseOnly` (no `size()`/`read()`), Gen `--nojit` | 5/5 crash | **0/5** |
| `ChurnOnly` — same allocations, same threads, NO file I/O | 0/5 | 0/5 |
| ZGC, G1, HotSpot | pass | pass |

`ChurnOnly` is the control that attributes the crash to the file path rather
than to the churn. The two halves were landed and measured separately: the
allocation half alone took Generational from 4/5 to 1/5 and left `--nojit` at
5/5, which is what said a second producer was still there.

**This does not close the Kafka test either** — `3d8c8edd8` ruled the
argument-snapshot mechanism out for it at 6/8 on the tip, and these producers
fail under `--nojit`, where that test passes. Same family, same path, same
symptom; a different failure.

## Why the audit missed it

The `unpinned-native-locals-audit-48-fixed-20260825` write-up defines this exact
family and fixed 48 instances of it. Its two rules ran over **`native-builtins/src`**.
`FileChannelImpl` lives in `native-io/src/file_channel.rs`, which was outside
that scope.

The file was not unaware of the hazard — it already pinned `channel` around two
LATER calls, with a comment naming the "native stale-local family". It pinned
the two call sites someone had thought about and left the five allocations above
them unrooted, which is precisely the failure mode the handle-scope block warns
about: *"Pinning at each call site is weakest of all: it is what the next audit
has to find again."*

## The fix

Both functions now use `NativeHandleScope`, the discipline `NativeContext`
documents for this: a handle is an opaque slot rather than an `ObjectRef`, so
the pre-GC local cannot be read back by mistake, and `handle_slots` is scanned
as a root set by `roots.rs`. Every handle is re-read immediately before each use
— uniformly, rather than reasoning per store about which calls can collect. The two
ad-hoc `pin_native_root`/`read_native_pin` pairs are gone; the scope subsumes
them.

Also rooted, found while converting: the `Cleaner`/`Closer` pair in the
parent-null branch, and the `Cleanable` that `register` returns and the next
line stores.

Re-running the audit's rule over the patched file reports no unrooted binding
that survives an allocating call.

## A claim in the first version of this page was wrong

It said the handles are re-read before each use because "`set_field_by_name`
resolves a field name and can itself allocate". **It cannot.**
`NativeContextImpl::set_field_by_name` takes `&self`, resolves a field index out
of the class store under a read lock, and stores through `heap.set_field`. It
neither allocates nor runs Java.

The re-reads are still there and are still correct — re-reading a handle costs a
slot load and removes the need to be right about which of the trait's ~600
methods can collect — but the reason given for them was false, and a later
reader sizing a window around "field stores can collect" would have got it
wrong. The `native-io` audit that followed depends on the opposite fact:
`set_field_by_name` is deliberately NOT in its GC-capable set, which is what
keeps the window between an allocation and the stores that follow it small
enough to read.

## Measurement

* **The reclaim's own signal.** `KafkaAutoConfigurationIntegrationTests#testEndToEndWithRetryTopics`
  under `CRATONVM_NO_MOVING_YOUNG=1` (every young collection is then the sweep)
  with `CRATONVM_DBG_SWEEP_ZERO=1`, four reps each, round-robin: **one
  `RECLAIMED-LIVE … sun/nio/ch/FileChannelImpl` on the unpatched binary, zero on
  the patched one.** That is one event, not a rate — stated as such.
* **No new crash.** The `CRATONVM_NO_MOVING_YOUNG=1` + `CRATONVM_DBG_GC_STRESS`
  configuration crashes on its own; 40 rounds ROUND-ROBIN give **19/40 nonzero
  on both binaries**, identical. Two earlier sequential batches suggested the
  patched binary was worse (0/12 then 6/18 then 4/20 for the SAME unpatched
  binary); that was host-load drift landing on whichever arm ran later, and
  round-robin removes it.
* **Gates.** `cargo test -p cratonvm-native-io` 534 passed / 0 failed,
  `cargo test -p cratonvm-types` green, `regression-suite/run.sh` 92 of 92.

## What this does not fix, and what would not have shown it

Two synthetic probes were written and neither reproduces the window: one opened
and mapped a channel 200 times, the other added allocation churn to make the
stress threshold cross inside the window. Both report **zero** reclaims on the
UNPATCHED binary. The window is a handful of instructions, and a probe that does
not allocate at the right rate simply never lands in it — which is why the
before/after on those probes reads as "no difference" and means nothing. The
Kafka workload, which opens many files under real allocation pressure, is the
only thing that has ever shown the reclaim.

And the Kafka test still fails, on both binaries, at roughly the rate it did
before. Its remaining cause is untouched by this: the reclaim is real and this
removes it, but it was never established that the reclaim is what fails that
test. That half of the page stays open.
