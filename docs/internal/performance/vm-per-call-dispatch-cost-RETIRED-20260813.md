# The VM-wide per-call cost, and why its profile does not convert into time

**Status:** RETIRED 2026-08-13 — a characterisation, not an open defect. This is
what was left of
[the netty `io.netty.buffer` throughput page](netty-per-call-throughput-20260813.md)
after its netty-specific half was closed. Nothing here is netty: it is the cost
CratonVM pays on every transfer of control, measured on the netty classes
because they are the densest call workload in the tree.

The one open item this page carried was §2's lever 2 — "a real fix is one lookup
per invoke, handed down the chain" — together with the instruction to **get the
per-invoke lookup COUNT before building it**. That count now exists
(`CRATONVM_DBG_NATIVE_LOOKUPS=1`), it has been taken, and §2.1 records what it
says: the divisor is **~4.2**, worth ~1.5% of CPU on a box whose measurement
floor is ~5%. The lever is real and it is too small to build. With that answered
nothing here is actionable, so the page moves out of `known-issues` and stays as
the reference it always was: what the number is, how it was measured, and which
five hypotheses it rules out.

**Read §3 before optimising anything on this page.** Two changes were made on
2026-08-13 that removed ~5.5% and ~10% of the attributed profile samples
respectively, and neither moved CPU by a measurable amount. A percentage in a
flat profile of this VM is a lead, not a quantity.

## 1. The shape

`CRATONVM_DBG=jit-scan-prof` reports `jit_entries` — transfers of control into
compiled code. Wall time tracks call volume, not any one site:

| class | tests | CratonVM | `jit_entries` | entries/test | wall ÷ entries |
| --- | --- | --- | --- | --- | --- |
| `BigEndianHeapByteBufTest` | 414 | 35.3 s | 116,816,944 | 282 k | 302 ns |
| `AdaptiveBigEndianHeapByteBufTest` | 415 | 62.3 s | 120,381,490 | 290 k | 517 ns |
| `AdaptiveByteBufAllocatorTest` | 127 | 551 s | **826,764,658** | **6.51 M** | 667 ns |

826 million calls at a few hundred nanoseconds each is the whole answer. HotSpot
inlines the same calls to roughly nothing, which is why its column barely moves
(~12 ms/test on *both* of the first two classes — that is JUnit overhead, not
buffer work).

`ns ÷ entries` is only comparable between workloads of similar call granularity.
`PcapWriteHandlerTest.writePcapGreaterThan4Gb` sits at 1371 ns/entry because
each of its entries covers a 65 KB traversal, not because it is a different
defect.

**Quote the `-Xint` ratio, not the C2 one.** On that pcap test: HotSpot C2
1.15 s, HotSpot `-Xint` 41.0 s, CratonVM 104 s — 90× versus C2 is a statement
about having no optimising compiler; **2.5× versus `-Xint`** is the statement
about this VM.

## 2. Where the CPU goes

Flat `perf record -F 499` (no call graph — a DWARF unwind produces a 1.3 GB file
that resolves nothing), `AdaptiveByteBufAllocatorTest`, 2026-08-13, on a build
carrying every fix listed in §3:

| share | symbol |
| ---: | --- |
| 8.5% | `NativeMethodRegistry::slot_for_exact` |
| 3.7% | `interpreter::execute_frame_from_index` — the interpreter itself |
| 2.9% | `VmHeap::is_object_address` |
| 2.5% | `jit_bridge::try_jit_compile_callee` |
| 2.4% | `__memcmp_evex_movbe` (the registry's name verification) |
| 2.4% | `invoke_on_class_shared_inner` |
| 2.3% | `invoke_or_native` |
| 2.2% | `ZObjectStarts::contains` |
| 2.1% | `NativeMethodRegistry::find` |
| 2.1% | `dispatch_virtual::execute_invokevirtual_cached` |
| 2.0% | `resolve_id_with_descriptor_quirks` |
| 1.9% | `class::find_method_recursive` |
| 1.4% | `conservative_roots::push_entry_full` |

Two families dominate, and they are the two levers:

**Lever 1 — the JIT entry/exit machinery**, `push_entry_full` 1.4% +
`pop_jit_entry` + `pin_jit_code_range_owner` + `validate_code_ptr` +
`record_transition` + `compute_jit_key_hash` + `JitCache::get` ≈ **7%**. Each
entry does a boundary-generation bump, a TLS `RefCell` push, a striped global
depth counter, a thread-state transition record, `jit_execution_enter` and
`gc_quiescence::enter` — then the mirror image on the way out. Most of it exists
so the GC can walk or defer around compiled frames, which is why it is not a
matter of deleting lines. Note this is *half* what the 2026-08-12 revision of
this page measured (~15%); the gate-pass memo landing accounts for the drop.

**Lever 2 — the native-registry probe on every invoke**, `slot_for_exact` 8.5% +
`memcmp` 2.4% + `slot_index_for_key` + `resolve_id_with_descriptor_quirks` 2.0%
≈ **14%**. It hashes class+method+descriptor byte-at-a-time and then memcmps all
three to verify. `native_class_hash`'s own comment records the same site at
8.75% of a live H2 profile.

**The thing to understand about lever 2 before touching it** (2026-08-13, and
this is new): the cost is not one lookup per invoke. It is *many*.
`invoke_or_native` is a long chain of `(class, method, descriptor)` string
comparisons, each arm of which may issue its own `find`, and `native_override`'s
force-native chain issues ~10 more. `resolve_id` — the every-invoke entry point
— was given the class prefilter it never had, and the whole family still only
gave back **4.7% of CPU** (`BigEndianHeapByteBufTest`, ABBA, n=10/arm, per-arm
spread 54–78 s, so read that with its error bar). `slot_for_exact` itself barely
moved, because the callers that dominate it are `find`/`find_with_kind` from
those chains.

A real fix is **one lookup per invoke, handed down the chain** — a restructuring
of the dispatch entry points, not a change to the registry. Anyone starting
there should first get the per-invoke lookup COUNT (there is no counter today;
`CRATONVM_DBG_DISPATCH_TALLY` gives callees, not lookups per callee), because
that number, not the profile share, is what a restructuring would divide.

## 2.1 The count, taken — and why the restructuring is not worth building

The counter the paragraph above asks for is `CRATONVM_DBG_NATIVE_LOOKUPS=1`
(`cratonvm_native_api::registry::lookup_census`), added 2026-08-13. It counts
both sides — every entry point INTO the registry, and every invoke that reaches
the two dispatchers those entry points hang off — and prints one line at exit.

`BigEndianHeapByteBufTest`, 414 tests, 44.6 s, real JDK 25:

```text
[native-lookups exit] lookups=19586621 invokes(general)=4700839
    find=14487379  find_with_kind=4801400  resolve_id=297842
    resolve_id_by_key=0  quirks=13225674  invokes(stackless)=89955
```

`UnpooledTest`, 41 tests, 3.6 s, as a second point:

```text
[native-lookups exit] lookups=716885 invokes(general)=163574
    find=443644  find_with_kind=199206  resolve_id=74035
    resolve_id_by_key=0  quirks=262099  invokes(stackless)=22163
```

Four things, in the order they matter.

**The divisor is ~4.2, and it is per GENERAL dispatch, not per invoke.**
19 586 621 / 4 700 839 = **4.17** registry probes per `invoke_or_native` call
(4.38 on `UnpooledTest`). The page's guess — "the cost is not one lookup per
invoke, it is *many*" — was right in direction; "many" is four, not ten or fifty.

**`invoke_or_native` is not the majority path.** 4.7 M calls against the 116.8 M
`jit_entries` §1 measures for the same class: the inline caches serve **96%** of
transfers, and everything in lever 2 lives in the remaining 4%. This is the
number that decides the question, and no profile could have produced it —
`slot_for_exact` samples do not carry their caller.

**So the arithmetic.** 19.6 M lookups per 44.6 s run. Taking 4.2 to 1 removes
~14.7 M of them; at the ~50 ns a hash-plus-memcmp costs on this box that is
~0.7 s of 44.6, i.e. **~1.5% of CPU** — and §3's one change that DID pay
returned a third of what its profile share promised, so the honest expectation is
lower still. The page's own measurability floor is ~5% at n=10 on this box. A
restructuring of `invoke_or_native` — the VM's most delicate function, ~1000
lines of `(class, method, descriptor)` comparison arms, each with its own
correctness history — cannot be justified by a lever that cannot be measured
after it lands.

**Two findings for whoever comes back to this anyway.**

* **91% of `find` probes are NEGATIVE.** `quirks=13 225 674` against
  `find=14 487 379`: nine out of ten lookups pass the class prefilter, miss the
  exact slot, and enter the `#[cold]` descriptor-rewrite arm — which answers
  `None` after one backward byte pass. The work is "is there a native for this
  triple?", asked four times per dispatch, answered `no` almost every time.
* **`resolve_id_by_key=0`.** The memoized-digest entry point — the one
  `NativeCallSite` exists to feed, negatives included — is reached **zero** times
  in either workload. The mechanism that would make a repeated negative free is
  built and this path never gets to it. That, not the registry's hashing, is
  where a cheap version of lever 2 would start.

## 3. What was tried and did NOT convert — read this first

Every one of these removed the work it targeted — the symbols disappear from the
profile afterwards — and the CPU column is what they were worth:

| change | samples removed | CPU effect, ABBA-interleaved |
| --- | --- | --- |
| 8-way `record_object_ref_payload` memo + wrapper-test skip | ~5.5% | **0.4%** — noise (n=16/arm, `AlSizeOnly`) |
| class prefilter in `resolve_id`, single-pass quirk precheck | ~2% attributed | **4.7%** (n=10/arm, `BigEndianHeapByteBufTest`) |
| …the same change, first cut, hashing the class name twice on a miss | — | **2.2% SLOWER** |

The middle row is the only one that paid, and it paid a third of what the
profile promised. The third row is the warning: a "prefilter" that re-derives
the key it is filtering on is a pessimisation, and only the CPU measurement
caught it — the profile looked fine.

The instrument is CPU time (user+sys) of a fixed workload, ABBA-interleaved,
because this box's wall clock is useless — load averaged 1.0 to 19 across these
runs and the same class measured 54 s to 78 s on one binary. Per-arm spread is
±15%, so nothing below ~5% is measurable here at n=10, and a claimed 2% needs
n≫10 or a different box.

The honest reading is that this workload is latency-bound, not
instruction-throughput-bound: removing instructions from a dependent chain of
pointer chases and thread-local lookups frees no time. **Size a lever by
building it and measuring CPU, never by summing profile lines.** A change worth
making here has to remove a *round trip* — a lookup, a lock, a cache miss — not
a few hundred instructions.

## 4. Two questions this page has already answered

* **`push_entry_full`'s per-entry cost.** ~200 ns of bookkeeping, 826 M times on
  the allocator class. Paid per transfer, not per compiled method.
* **Does the conservative-root scan cache ever hit?** `note_jit_boundary()` is
  called from `push_entry_full`, so the boundary generation is bumped on every
  entry. Measured `cache_hits=0 (0.0%)` in **every** run — 1,022 scans on the
  allocator class, 41,894 on `BigEndianHeapByteBufTest`, 60,163 on the adaptive
  heap class. The answer is 0%, not "small". It is not the cost here
  (`band_words=0`), but the cache as built cannot hit.

## 5. What is ruled out

* **The `sun.misc.Unsafe` bulk-access natives.** `--dump-native-registry` over
  the allocator run: 838 distinct natives, 17,689,213 invocations, of which
  `Unsafe` is **54 distinct / 15,281 invocations — 0.086%**. At ~120 ns per
  native call that is under 2 ms of a 551 s run.
* **The JIT.** `--nojit` is *slower* (>660 s, killed). Compiled code helps;
  there is simply not enough of the run inside it.
* **`jit_method_calls_native_shadowed`.** Turning the whole seal off compiles
  626 more methods and changes the wall clock by 0.5%.
* **Any single hot site worth more than a few percent.** §1's arithmetic is the
  answer.

## 6. Measurement hygiene

This box runs many concurrent agents. Anything that needs better than ±20% must
be ABBA-interleaved, repeated, and measured in CPU time on a fixed workload. A
ratio against the same method body written in plain Java **in the same process**
is the one figure immune to load, and is what
[the ArrayList record](../fixed-suite-bugs/netty/arraylist-native-overhead-and-view-carrier-FIXED-20260813.md)
quotes.

A **count** is immune to load outright, which is why §2.1 is a count and not a
timing. `CRATONVM_DBG_NATIVE_LOOKUPS=1` costs a contended `fetch_add` per probe
while it is on — useless for a timing, exactly right for the question it answers,
and the reason that question could be settled in two runs instead of twenty.

## 7. Repro

```bash
cd apps/netty-suite-runner
CLS=io.netty.buffer.AdaptiveByteBufAllocatorTest

CRATONVM_DBG=jit-scan-prof /usr/bin/time -f "%e s" <cratonvm> \
    --java-home <jdk25> --Xmx 1500m @common.args -Dcraton.batch=1 CratonRunner $CLS

<cratonvm> --java-home <jdk25> --Xmx 1500m --dump-native-registry census.json \
    @common.args -Dcraton.batch=1 CratonRunner $CLS

perf record -F 499 -o p.data -- timeout 120 <cratonvm> … CratonRunner $CLS
perf report -i p.data --stdio --no-children -g none --percent-limit 0.7

# §2.1 — lookups per dispatch. One line at exit; no perf, no ABBA, no error bar.
CRATONVM_DBG=native-lookups <cratonvm> --java-home <jdk25> --Xmx 1500m \
    @common.args -Dcraton.batch=1 CratonRunner io.netty.buffer.BigEndianHeapByteBufTest \
  2>&1 | grep native-lookups
```
