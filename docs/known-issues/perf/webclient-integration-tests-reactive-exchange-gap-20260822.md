# `WebClientIntegrationTests` — one WebClient exchange costs 30 ms of CPU against HotSpot's 3.8 ms

**Status: OPEN — characterised 2026-08-22 on `dev` `651fa3256` (Azure Linux
host 2). Throughput, not correctness: the class scores `169/170 succ, 0 fail,
1 skip`, same as HotSpot, and finishes. The gap is 7.7x on CPU per exchange and
it is FLAT — no single symbol is above 3% of a profile. This page exists so the
next reader does not re-derive the seven things that are already ruled out, and
does not repeat the two measurements that lie.**

The one defect this investigation did find and fix is a different bug and did
not move this class:
`internal/fixed-suite-bugs/registered-native-that-always-yields-cost-the-jit-and-the-inline-cache-FIXED-20260822.md`.

## The number

`apps/spring-suite-runner`, `KRunT` (per-test timing launcher), Azure host 2,
ABBA-interleaved because a same-config wall time on this box swings 50%:

| | HotSpot 25 | CratonVM | ratio |
|---|---:|---:|---:|
| whole class, wall | 3.0-4.7 s | 15.2-24.1 s | ~5x |
| whole class, CPU | 9.5 s | 28-32 s | ~3.1x |
| sum of the 169 per-test times | 1.83 s | 10.98 s | 6.0x |

Per connector (`sum of that connector's 42 tests`, idle box): Reactor Netty
4.3x, Jetty 7.2x, JDK 7.9x, HttpComponents 8.9x. Reactor Netty is the largest
in absolute terms and the *smallest* ratio, which is the first hint that the
connector is not the variable — the shared per-test scaffolding is.

## Reduce it to one exchange

Every one of the 170 parameterized tests does the same thing: `new
MockWebServer` + `start()`, build a `WebClient` on the connector under test,
one request, `close()`. `probes/ExchangeProbe.java` is exactly that shape with
no JUnit, and it reproduces the whole gap: **HotSpot 3.48 ms/op, CratonVM
28.28 ms/op** (Jetty; 3.98 / 31.14 for Reactor Netty).

`probes/ExchangePhases.java` splits it, and this is where the first trap is:

| phase | HotSpot | CratonVM |
|---|---:|---:|
| A `new MockWebServer` + start + close | 0.33 ms | 0.73 ms |
| B `WebClient.builder()…build()` | 0.02 ms | 0.24 ms |
| C GET on a **reused** server + client | **43.10 ms** | **49.92 ms** |
| D assemble the `Mono`, never subscribe | 0.04 ms | 0.40 ms |
| E whole shape (fresh server + client + GET) | **2.31 ms** | **26.79 ms** |

**Trap 1 — phase C is not a VM signal.** Repeating a GET against a *kept-alive*
connection costs 43 ms on HOTSPOT and 50 ms on CratonVM: a fixed protocol-level
delay (the classic delayed-ACK / Nagle interaction on a small keep-alive
request), present on both VMs, that swamps the 7 ms of real difference. An
earlier probe in this investigation measured only this shape and reported
"CratonVM 54 ms vs HotSpot 43 ms, 1.25x" — which is neither the gap nor a
CratonVM property. Measure phase E.

## Where the 30 ms goes: nowhere in particular

**Trap 2 — the raw `perf stat` total says CratonVM is barely slower, and that
is wrong.** For 105 exchanges: HotSpot 4059 ms CPU, CratonVM 4404 ms — 8% apart,
against 8.6x on wall. That reads as "CratonVM is waiting, not computing". It is
not: HotSpot's total is dominated by its own startup (C2 compiler threads, GC
threads). Difference two loop counts to remove startup —
`probes/wcit-marginal.sh` runs n=40 and n=200 and subtracts:

| | HotSpot | CratonVM |
|---|---:|---:|
| marginal wall / exchange | 1.48-1.64 ms | 25.34-25.59 ms |
| marginal **CPU** / exchange | 3.53-4.11 ms | 29.86-30.23 ms |

So it is CPU: **30 ms per exchange against 3.8 ms, 7.7x.** (The wall ratio is
worse, 16x, because HotSpot runs this at ~2.4 cores and CratonVM at ~1.2 — but
see "ruled out" below; that is not lock contention.)

`perf record -F 999 -g` over a 500-exchange run, self% summed by owner
(`probes/bucket.py`; 76.8% of samples classified, 1955 distinct symbols, top
single symbol **2.65%**):

| subsystem | self% | share |
|---|---:|---:|
| unclassified long tail | 13.6% | 17.7% |
| interpreter core | 13.5% | 17.5% |
| class-load / resolve | 12.7% | 16.6% |
| JIT runtime (bookkeeping, not compiled code) | 9.7% | 12.6% |
| native dispatch | 6.5% | 8.5% |
| kernel | 5.7% | 7.4% |
| GC / heap | 5.7% | 7.4% |
| allocator | 3.6% | 4.7% |
| libc mem | 3.2% | 4.2% |
| hashing | 2.1% | 2.8% |

**This workload is ~98% interpreted.** By DSO, `[JIT] tid …` is **1.66%** of
samples while the JIT *runtime* costs 12.6% — roughly 7 units of bookkeeping per
unit of compiled code executed. That is the shape
[[jit-entries-per-call-cost-is-the-call-dense-wall]] describes, and it is why
the levers below are all flat.

## Ruled out — do not re-test these

Each was measured on this workload, interleaved, on this branch:

1. **The JIT is not the lever, in either direction.** ABBA over 6 rounds:
   JIT 24.62 ms/op, `--nojit` 25.75 ms/op. It costs 12.6% and returns about the
   same. (On the whole class the JIT is ~18% ahead, so do not turn it off
   either.)

   **WHY it is a wash was root-caused 2026-08-22, and the cause is now
   FIXED for all four invoke kinds (2026-08-23):** a compiled caller calling a
   callee the JIT did NOT compile fell into the fully name-keyed
   `invoke_or_native` path and cost **1902 ns** against **385 ns** for the same
   call with the caller left interpreted — so on a partially-compiled call
   graph the JIT's wins and this loss cancelled. Each invoke kind now enters an
   uncompiled callee through the call site's own cached interpreter frame
   template (`CRATONVM_JIT_VIRTUAL_BYTECODE_CALLEE` /
   `CRATONVM_JIT_STATIC_BYTECODE_CALLEE` are the kill switches), at ~420 ns —
   below the both-interpreted cost. **The exchange has NOT been re-measured
   against that, and this page's numbers predate it.** What that page also
   explains, and what is NOT fixed, is why reactive code is hit hardest: a
   method containing an
   unbridged `invokedynamic` is denied OSR outright and retired with
   `MakeNotCompilable` after its first compiled execution, so every
   lambda-creating method becomes exactly that interpreted callee.
2. **The native-shadow caller seal is not the lever.** 1179 methods are sealed
   before any compile (`clinit=771`, `calls-native-shadowed-method=408`) against
   155 compiled. `CRATONVM_JIT=-native-shadow-caller-seal` measured 28.43 ms/op
   against 26.79 base — no better. This confirms the netty measurement already
   quoted in `jit_invoke_targets_native_shadow`'s own comment on a second
   workload.
3. **The C1 threshold is not the lever.** Default 500; a request-path method is
   called ~60 times in this probe and ~170 times in the class, so most never
   qualify. `CRATONVM_TIER_C1_THRESHOLD=150` -> 29.11 ms/op, `=50` -> 28.53
   against 26.79 base. Lowering it does not pay for itself.
4. **Native call volume is not the bulk.** `--dump-native-registry` differenced
   between n=40 and n=200: **3732 native invocations per exchange**, led by
   `Object.<init>` (499), `Class.isInstance` (263), `Enum.ordinal` (148),
   `Objects.requireNonNull` (220 across both overloads). At the in-tree measured
   ~120 ns/native-call that is ~0.45 ms of 30 ms; even at the pessimistic
   ~350-490 ns end it is ~1.3-1.8 ms. 5% at most.
5. **It is not lock contention.** All lock/park/futex symbols together are 1.27%
   of the profile, `RawMutex::lock_slow` 0.02%. The 1.2-vs-2.4 core difference is
   HotSpot burning cores on its own compiler threads, not CratonVM blocking.
6. **Classes are not being re-loaded per iteration.** `CRATONVM_DBG=define-census`
   reports 3485 definitions at n=10 and 3489 at n=60 — four more classes for
   fifty more exchanges. The `verified_code` / `ZipArchive::new` symbols visible
   in a short profile are startup; they fall to ~0.5% once the loop dominates.
7. **`java.time.Instant` is not the cause**, despite being the hottest method in
   the class by a factor of 475 (`Instant.now()`, **1 240 308** invocations per
   run). See the FIXED page above for the full sizing error; the short version
   is that the suite reaches it from interpreted callers at ~250-500 ns, not the
   ~7 µs a JIT-compiled loop pays, and HotSpot makes the same 1.24 M calls.

## What is actually left

A flat profile with a 2.65% ceiling and no lever above means the cost is the
per-bytecode and per-dispatch price of the interpreter across a very deep,
very allocation-heavy call graph (Reactor assembly + WebFlux codecs + the
connector's own stack). The named items worth having, in descending order, none
of them a step change:

| item | self% | note |
|---|---:|---|
| `InvokeCache::get` | 2.65% | halved on this branch — it hashed the key twice per hit |
| `resolve_field_ref_loader_aware` | 1.98% | per-`getfield` site cache; the hit path still clones `ResolvedField` |
| `conservative_roots::native_stack_has_jit_frame` | 1.59% | GC root scan cost that exists only because JIT frames might be present |
| `resolve_method_metadata` | 1.22% | |
| `CachedInvokeTarget::clone` | 0.77% | every inline-cache hit clones the target enum |
| `intern_arc` | 0.97% | |
| `register_jit_code_range_inner` + its sorts | ~0.7% | a sorted `Vec` of code ranges rebuilt per registration |

## Reproducing

```
# probes live in apps/spring-suite-runner/ on this branch
bash /data/wcit-probe.sh cv  <log> ExchangeProbe jetty 200   # CRATONVM_BIN=…
bash /data/wcit-probe.sh hs  <log> ExchangeProbe jetty 200
bash /data/wcit-marginal.sh jetty 40 200                     # startup-free CPU/exchange
bash /data/wcit-lever.sh <bin> 60 3 jit "-" nojit "CRATONVM_DISABLE_JIT=1"
```

Always interleave. This box runs ~20 agents; consecutive same-config runs of
`ExchangeProbe` ranged 23.4-31.4 ms/op in one four-minute window.

Related: [[jit-entries-per-call-cost-is-the-call-dense-wall]],
[[profile-before-calling-it-the-interpreter-throughput-wall]],
`known-issues/netty/adaptive-bytebuf-allocator-throughput-20260812.md`.
