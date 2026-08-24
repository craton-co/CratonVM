# `WebClientIntegrationTests` — the reactive exchange gap — RETIRED 2026-08-23

| | |
|---|---|
| **Status** | **RETIRED — not fixed.** Every hypothesis this page raised is now answered, including the one it left open, and the answer to that one is a COUNT rather than another timing run. What remains is the VM-wide interpreter wall, which has its own pages |
| **Opened** | 2026-08-22 on `dev` `651fa3256` |
| **Retired by** | `perf/filechannel-vector-webclient-residuals-20260823` |
| **The gap** | **Still ~7.7x CPU per exchange.** This page is retired because nothing left in it is specific to this class, not because the number moved |

## Why a page can be retired without its number moving

This was never a defect page. Its own opening said so: *"This page exists so
the next reader does not re-derive the seven things that are already ruled out,
and does not repeat the two measurements that lie."* Its value was the
ruled-out list and the two traps, and both survive here verbatim. What it left
OPEN was one item — and that item is now closed with evidence, which is what
makes the page finished.

## The one open item, and the count that closes it

The page's §1 "ruled out" entry read:

> **The JIT is not the lever, in either direction.** ABBA over 6 rounds: JIT
> 24.62 ms/op, `--nojit` 25.75 ms/op. It costs 12.6% and returns about the
> same. **WHY it is a wash was root-caused 2026-08-22 and is a separate page:**
> `jit-compiled-caller-to-interpreted-callee-costs-1900ns-20260822.md` (now
> `performance/jit-compiled-caller-to-interpreted-callee-FIXED-20260823.md`).

That separate page is now fully fixed — all four dispatch kinds, `invokestatic`
on 2026-08-22 and `invokevirtual`/`invokeinterface`/`invokespecial` here, with
the isolated transition measured **2 044 -> 440 ns/op** and now cheaper than
not compiling the caller at all
(`performance/jit-compiled-caller-to-interpreted-callee-FIXED-20260823.md`).

**It does not move this class, and the reason is a count, not a hypothesis.**
`CRATONVM_DBG=mic-prof` over 300 `ExchangeProbe` exchanges on Azure host 2:

```
disp_calls=10232   mic_calls=4270   hit_entry=0   hit_noentry=3280
out_virt_bc=44     out_virt_bc_refused=3298
out_special_bc=0   out_special_bc_refused=0
```

Ten thousand `jit_invoke_dispatch` calls for three hundred exchanges, and
`hit_entry=0` — not one inline-cache dispatch found a compiled callee to enter.
The compiled→interpreted transition this page blamed is reached about **11
times per exchange**, and at the ~1 600 ns the fix saves per transition that is
**~18 µs against ~30 000 µs: 0.06%**.

So "the JIT is a wash" was right, and the mechanism named for it was real, but
it was never this class's lever — **because this class barely enters compiled
code at all.** That is the sentence the page was missing, and it converts its
own §1 from a measurement plus a hypothesis into a measurement plus an
arithmetic bound.

It also retires a measurement that would otherwise be re-run forever. Two
attempts on this branch confirm the page's own warning that this probe's wall
clock is unreadable on a shared host: three ABBA rounds of the memo A/B gave
33.3 / 33.0 / 46.3 / 49.6 / 34.1 ms per exchange with the memo ON against
31.3 / 45.3 / 39.4 / 30.8 with it OFF, and the startup-free `perf stat
-e task-clock` marginal form was no better (31.8-61.6 ms ON, 1.3-49.4 OFF, the
1.3 being a run that failed outright). **The spread inside one arm exceeds any
difference between arms.** The census is not noisy, and it is what a future
reader should reach for.

## The seven ruled-out items are unchanged

They are still ruled out and still worth not re-testing: the JIT (now with the
count above), the native-shadow caller seal, the C1 threshold, native call
volume (3 732 per exchange, ~5% at most), lock contention (1.27% of the
profile), per-iteration class loading (3 485 definitions at n=10 against 3 489
at n=60), and `java.time.Instant` (1 240 308 calls, and HotSpot makes the same
1.24 M).

Both traps also stand. **Trap 1**: phase C, a GET on a kept-alive connection,
costs 43 ms on HOTSPOT and 50 on CratonVM — a protocol-level delay on both
VMs, not a VM signal; measure phase E. **Trap 2**: the raw `perf stat` total
says CratonVM is 8% slower where the wall says 8.6x, because HotSpot's total is
dominated by its own C2 and GC threads; difference two loop counts.

## What is left, and where it lives now

A flat profile with a **2.65% ceiling**, 1 955 distinct symbols, and 98%
interpreted execution. The page's own words for it:

> the cost is the per-bytecode and per-dispatch price of the interpreter across
> a very deep, very allocation-heavy call graph

That is the VM-wide interpreter wall, and it is not a `WebClient` fact. It
belongs to [[jit-entries-per-call-cost-is-the-call-dense-wall]] and
[[profile-before-calling-it-the-interpreter-throughput-wall]], and the
`hit_entry=0` census above is a useful new datum FOR those pages: on a reactive
workload the JIT's 12.6% is being spent almost entirely on bookkeeping for
compiled code that is not being entered.

The named sub-items are recorded here so they are not lost, with what is now
known about each. **None is a step change, which is what the page said and what
still holds** — their sum is under 10% of a 770% gap:

| item | self% | status |
|---|---:|---|
| `InvokeCache::get` | 2.65% | **halved 2026-08-22** — it hashed the key twice per hit |
| `resolve_field_ref_loader_aware` | 1.98% | the page attributes this to "the hit path still clones `ResolvedField`". That is not where the cost is: `ResolvedField` is a `ClassId`, a `usize`, three flags and a byte — its `clone` is a memcpy with no `Arc` in it. The 1.98% is the function's own prologue and its two epoch loads, on a path already reduced to an array index and four compares |
| `conservative_roots::native_stack_has_jit_frame` | 1.59% | already generation-memoised with an address-envelope prefilter and an 8 MiB cap. The remaining cost is the raw word scan itself, which is intrinsic to conservative root scanning. A per-thread "has ever entered compiled code" short-circuit is the obvious next idea and is deliberately NOT taken here: it is a change to GC root scanning, and this host cannot measure a 1.6% effect |
| `resolve_method_metadata` | 1.22% | untouched |
| `intern_arc` | 0.97% | untouched |
| `CachedInvokeTarget::clone` | 0.77% | real: `dispatch_static.rs`'s hit path clones the enum to release the `thread` borrow, and its variants hold several `Arc`s, so a hit costs 4-8 atomic RMWs instead of one. The fix is to store `Arc<CachedInvokeTarget>` in the map — 46 `put` sites and two `get` sites, mechanical but broad. Not taken for 0.77% |
| `register_jit_code_range_inner` + sorts | ~0.7% | untouched |

## Reproducing

```
# probes live in apps/spring-suite-runner/ and probes/ on this branch
CRATONVM_BIN=<bin> bash probes/wcit-exchange-ab.sh run cv ExchangeProbe jetty 200
CRATONVM_BIN=<bin> bash probes/wcit-exchange-ab.sh marginal jetty 40 200
CRATONVM_DBG=mic-prof   # the [DISP_CENSUS] / [MIC_PROF] lines, which do not lie
```

Always interleave, and read `disp_calls` and `hit_entry` before believing any
JIT-related conclusion about this class.

Related: [[jit-entries-per-call-cost-is-the-call-dense-wall]],
[[profile-before-calling-it-the-interpreter-throughput-wall]],
`known-issues/netty/adaptive-bytebuf-allocator-throughput-20260812.md`,
`performance/jit-compiled-caller-to-interpreted-callee-FIXED-20260823.md`.
