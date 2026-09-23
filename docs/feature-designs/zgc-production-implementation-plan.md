# Production ZGC

**Status:** Partial, and the default collector. It can **mark concurrently**
as of 2026-08-16 (opt-in, `CRATONVM_ZGC_CONC_START=60`) and it **compacts** as
of 2026-08-13; the sweep is still
stop-the-world, marking is snapshot-at-the-beginning rather than ZGC's load
barrier, and the generational machinery is written, unit-tested and **not
adopted**. See
[the concurrent+generational plan](zgc-concurrent-and-generational-plan-20260813.md)
for what landed and what is left.

## What is built

- **The feature and its wiring.** `zgc = []` in `gc/Cargo.toml`, forwarded as
  `zgc = ["cratonvm-gc/zgc"]` (`vm/Cargo.toml`) and
  `zgc = ["cratonvm-vm/zgc"]` (`vm-cli/Cargo.toml`). Nothing defaults to it.
  The `GcAlgorithm::Zgc` variant and the `"z" | "zgc"` arm of
  `parse_gc_algorithm` are themselves `#[cfg(feature = "zgc")]`, so
  `-XX:+UseZGC` selects a real backend only in a build that compiled it.
- **`ZgcRealHeap` is real, not a stub** (`gc/src/zgc.rs`). It backs every
  allocation with owned memory, writes real object headers, and implements
  `GarbageCollector` with the same bounds-checked semantics as its siblings.
  Its `collect_garbage` traces the live graph, marks survivors and reclaims
  onto a free list. It is **non-moving**, so it reclaims but does not compact,
  and it returns an empty pointer map.
- **The submodules exist and are unit-tested**: `vaddr`, `page`, `barrier`,
  `forwarding`, `metrics`, `remembered`, `mark`, `generation`, `relocate`,
  `tlab`, `census`, `adapters`.
- **Partial adoption has begun.** `ZgcRealHeap` implements `ZTlabHeapHooks` and
  `mark::ZMarkContext` and owns a `census::ZSlotCensus`.

## What is not built yet

- **Nothing concurrent runs.** `gc/src/zgc_concurrent.rs` has zero non-doc
  callers; no `ZMarkContext`-backed coordinator is spawned for `ZgcRealHeap`.
- **Nothing relocates.** `barrier`, `forwarding`, `relocate`, `page`,
  `generation`, and `vaddr`-as-a-slot-encoding are unadopted. There is no
  production `impl ZBarrierContext` anywhere — only test implementations
  inside `gc/src/zgc/barrier.rs`. `ZgcRealHeap`'s `ZMarkContext` returns
  `vaddr::Z_REMAPPED` unconditionally, because the heap stores raw pointers in
  its slots, never a colored word.
- **The JIT emits no load barrier.** See
  [`zgc-jit-load-barrier.md`](zgc-jit-load-barrier.md).
- **Reference slots are not colored.** See
  [`zgc-reference-slot-representation.md`](zgc-reference-slot-representation.md).
- **`ZgcCollector` / `ZgcHeap` / `GenerationalZgc` / `ColoredPointer` /
  `LoadBarrier` are a metadata-only simulation** of OpenJDK's model — synthetic
  page addresses with no backing storage, a forwarding table that copies no
  bytes, a `&mut self` barrier rather than an atomic self-healing CAS.
  Selecting one emits a one-time warning so it cannot masquerade as production
  ZGC.

**It is no longer default-off.** As of 2026-08-10 the default `GcAlgorithm` IS
`Zgc` (`vm/src/config.rs`), on the strength of the three-way Tomcat suite
comparison; the paragraph that used to stand here explained why it was
default-off and is kept below only so the reversal is visible rather than
silently edited away:

> *Why it is default-off: not because nothing uses it, but because it is not at
> pass-rate parity with Generational.*

That parity question was never answered — it was overtaken. Re-establishing it
is Phase 1 of
[`zgc-maturity-assessment-and-plan-20260813.md`](zgc-maturity-assessment-and-plan-20260813.md),
which also carries the current state of everything below.

Default-off was never uncompiled either, and that part still holds: CI's
`experimental-features` job builds the `cratonvm-cli` binary under the feature
and runs the gc crate's unit tests with it, because this configuration once
stopped compiling entirely and nobody noticed.

## 1. Where ZGC actually is

> **Superseded in its FIGURES, not its verdict — dated 2026-08-07 (later the
> same day).** This section was written before the modules in §0.2 landed, and
> every line number and size in it has drifted. Known-stale as of a recount from
> disk: `zgc.rs` is **3788** lines, not 3454; `ZgcRealHeap` begins at **:1448**,
> not 1396; `ZPage` **:378**, `ZgcHeap` **:499**, `ZgcCollector` **:711**,
> `GenerationalZgc` **:1075**; the one-time `tracing::warn!` is at **:155**, not
> :103. The `gc/Cargo.toml` / `vm/Cargo.toml` line cites at the end of this
> section are stale too — find the feature by name (`^zgc = `), not by line.
>
> **One substantive claim, not just a number, has changed:** the paragraph below
> saying `gc/src/zgc_concurrent.rs` "(505 lines)" drives `Arc<Mutex<ZgcCollector>>`,
> "i.e. **the simulation**" describes the *pre-rewrite* file. It is now **1531**
> lines and drives the real `zgc::mark` engine (`ZMarkCoordinator`) instead. The
> sentence that follows it — *"It has never been pointed at `ZgcRealHeap`"* —
> **remains true**: no `ZMarkContext` for `ZgcRealHeap` exists, so the driver is
> still off the execution path (§0.1).
>
> **A second substantive claim has since changed — dated 2026-08-13.** The
> "no TLABs" clause below (and in the sentence this note replaced, which
> re-verified it on 2026-08-07) is **no longer true**. `ZgcRealHeap` grew its
> own thread-local allocation buffers on 2026-08-08 (`gc/src/zgc/tlab.rs`,
> `ZgcRealHeap::alloc_raw_tlab`), they are **on by default**, and
> `CRATONVM_ZGC_TLAB=0` is the kill switch. The `vm_heap.rs` cite the old claim
> rested on is still literally accurate — `VmHeap::refill_tlab` does return
> `None` for `VmHeap::Zgc` — which is precisely why the claim survived so long:
> **the generic TLAB door is shut and the backend has its own.** A TLAB chunk
> is *reserved* space no collection can reclaim while its thread lives, and
> failing to bound that reservation by the thread count was the dominant defect
> behind the 2026-08-13 Tomcat OOM.
>
> Everything else here — the two-halves-in-one-file shape, the simulation's
> character, `ZgcRealHeap` being real but
> non-moving/non-concurrent/non-generational, and the measured suite cost — was
> re-verified and **stands**.

`gc/src/zgc.rs` (3454 lines) is two different things sharing a file. Lines
1–1395 are a **metadata-only simulation** (`ColoredPointer`, `LoadBarrier`,
`ZPage` at `:326`, `ZgcHeap` at `:447`, `ZgcCollector` at `:659`,
`GenerationalZgc` at `:1023`): `ZPage::virtual_start`/`physical_start` are
synthetic `u64` counters with no `*mut u8` behind them, `concurrent_relocate`
records a forwarding entry but copies no bytes because there are no bytes,
"concurrent" phases run synchronously inside `trigger_gc`, and
`LoadBarrier::slow_path` takes `&mut self` and mutates a plain `ColoredPointer`
value rather than performing an atomic self-healing CAS on an in-memory oop.
Selecting it emits a one-time `tracing::warn!` (`zgc.rs:103`) so it can never
masquerade as production ZGC. **This half is scaffolding, not a collector.**

Line 1396 onward is `ZgcRealHeap`, and it is genuinely real: `Arena`-backed
storage, real `ObjectHeader`s, real `java.lang.ref` reference processing, real
bounds-checked field/array semantics, and a real stop-the-world **non-moving
whole-heap mark-sweep**. It implements `GarbageCollector` in full and is wired
end to end — `GcAlgorithm::Zgc` (`vm/src/config.rs:51`) → `GcBackend::Zgc`
(`vm/src/vm/vm_init.rs:1357`) → `VmHeap::Zgc(ZgcRealHeap)`
(`gc/src/vm_heap.rs:214`, `:278`) — and is selectable with `-XX:+UseZGC` /
`--XX:UseGc ZGC`. It has no compaction, no concurrency, and no generational
split. (The words "no TLABs (`vm_heap.rs:2114` returns `None` from
`refill_tlab`)" stood here until 2026-08-13; see the correction at the head of
this section — the cite is right and the conclusion drawn from it is wrong.)

`gc/src/zgc_concurrent.rs` (505 lines) has a `ZgcConcurrentMarkController`
(`:174`) with a real background worker thread — but it drives
`Arc<Mutex<ZgcCollector>>` (`:198`), i.e. **the simulation**. It has never been
pointed at `ZgcRealHeap`.

All of the above is behind the `zgc` Cargo feature (find it by name, `^zgc = `,
in `gc/Cargo.toml` and `vm/Cargo.toml`). The gate hides an enum variant and a
`parse_gc_algorithm` arm. **It was default-off when this was written and has
been default-ON since 2026-08-10** — it has to be, because the default
`GcAlgorithm` is now the variant it gates. Only `--no-default-features` reaches
the fallback this paragraph describes, where `-XX:+UseZGC` warns and silently
selects Generational.

**The measured cost of the gap** (2026-08-07, 1975-class Spring Boot suite,
same binary, `-Xmx 2g`, only `-XX:+UseZGC` varied): PASS 1902 → 1860, HANG
18 → 49, FAIL 11 → 22. **50 classes changed status, 46 of them regressions**;
35 were PASS → HANG at the 300 s ceiling, overwhelmingly `*AutoConfigurationTests`
— the build-and-tear-down-many-`ApplicationContext`s shape, i.e. allocation
churn. That is the signature a whole-heap STW mark-sweep with no young-gen
fast path produces. **It is a working hypothesis, not a root cause** — no
GC-count or pause-time instrumentation was pulled from any of those 35 logs.
Phase 0 exists to fix that, and Phase 2 is the phase most likely to move the
number.

---

## 2. Phases and their workstreams

Each workstream names its **owned files**. Two workstreams that share no owned
file can run at the same time by different agents.

### Phase 0 — unblock and instrument *(no dependencies)*

| WS | What | Owned files | Gated on | Status |
|---|---|---|---|---|
| **0a** | CI must build the *binary*, not just the libs, under `--features zgc` | `.github/workflows/ci.yml` | — | **DONE** |
| **0b** | Pause/phase/GC-count instrumentation for `ZgcRealHeap` | `gc/src/zgc/metrics.rs` (new) | — | in progress |
| **0c** | De-stale the ZGC docs | `docs/GC.md`, `docs/gc-tuning.md`, `gc-crate-audit.md` | — | **DONE** |

**0a is already landed and is the precedent that motivates the rest.**
`ZgcRealHeap::with_capacity()` was left out of the change that gave the
compact-layout registry a `layout_domain` — its three sibling collectors
(`Heap`, `G1Collector`, `GenHeap`) all got the field, it did not — and the
feature stopped compiling with `E0063` on 2026-08-05. It was found on
2026-08-07 by a human running a full suite, **not by CI**, and fixed in
`18ae88a21`. Two gaps let it through, and `ci.yml:491`/`:495` now close both:
nothing built `cratonvm-cli` with the feature (every `zgc` step was `-p
cratonvm-vm -p cratonvm-native-builtins`, i.e. libraries, and `zgc` gates whole
dispatch paths a default build resolves to nothing), and the collector's own
**89 `#[test]`s** (86 in `zgc.rs`, 3 in `zgc_concurrent.rs`) executed nowhere.
Every module this plan adds must land inside a build CI actually runs — see the
feature-gate-rot risk in §4.

**0b is the gate on believing anything in Phase 2.** The 35-class HANG
hypothesis is unverified. Until `ZgcRealHeap` reports GC count, per-phase
(mark/sweep) time and bytes reclaimed per cycle, a Phase 2 improvement cannot be
distinguished from host noise. Precedent for the shape: `gc/src/gc_metrics.rs`
and the young-pause phase-share work — note that on a shared host the *shares*
survive a sizing change but the absolute figures do not, so report shares.

### Phase 1 — core moving-GC mechanics

These four are **deliberately decoupled and are being written in parallel right
now** as self-contained submodules under a new `gc/src/zgc/` directory. The
decoupling is the design: the barrier depends on a **trait**, not on the other
modules' concrete types, so all four can land independently and in any order.
None of them is wired into `ZgcRealHeap` as it lands — wiring is Phase 3/4 work.

| WS | What | Owned files | Gated on |
|---|---|---|---|
| **1a** | Real colored pointers + multi-mapped virtual address space | `gc/src/zgc/vaddr.rs` (new) | — |
| **1b** | Atomic self-healing load barrier, **interpreter** side | `gc/src/zgc/barrier.rs` (new) | — (trait-only) |
| **1c** | The same barrier, **JIT** side | JIT emit sites (see §4) | 1b's trait |
| **1d** | Real region/page management with backing storage | `gc/src/zgc/page.rs` (new) | — |
| **1e** | Lock-free forwarding table + relocation-set selection | `gc/src/zgc/forwarding.rs` (new) | — |

**1b and 1c are separate tracks on purpose.** An interpreter barrier is a Rust
function on the load path; a JIT barrier is emitted machine code at every
reference load in compiled code. They fail differently, they are tested
differently, and a barrier that covers only the interpreter is a correctness
hole the moment a method tiers up — see the JIT-coverage risk in §4.

`ZPage` in the simulation is the shape 1d should inherit, but not the code:
it has no backing storage. `1d` is `ZPage`'s API over a real `Arena`.

### Phase 2 — generational split *(highest value for the measured regression)*

| WS | What | Owned files | Gated on |
|---|---|---|---|
| **2a** | Real young/old storage under the existing policy | `gc/src/zgc/generation.rs` (new) | 1d |
| **2b** | Remembered set / cross-generational edge tracking | `gc/src/zgc/remset.rs` (new) | 1d |

**There is a real head start here, and it is a policy head start, not a code
head start.** `GenerationalZgc` (`zgc.rs:1023`) already implements the
JEP-439-shaped policy: minor/major scheduling, per-page promotion aging
(`page_ages`, `promotion_age`, `promotion_count`), occupancy thresholds, and a
`remembered_set` as a card-table substitute. It needs **real storage under it,
not a new policy design.** The `with_total_size` / `with_default_split`
constructors and `DEFAULT_YOUNG_FRACTION` are reusable as-is.

This is the phase that should move the 35 HANG classes, because the hypothesis
those 35 classes support is specifically "every collection scans the entire live
set instead of a small young generation". Phase 0b's instrumentation is what
turns that from a hypothesis into a measurement.

### Phase 3 — concurrency

| WS | What | Owned files | Gated on |
|---|---|---|---|
| **3a** | Rewire the concurrent-mark controller from the simulation onto `ZgcRealHeap` | `gc/src/zgc_concurrent.rs` | 0b, 1b |
| **3b** | Concurrent relocation / compaction | `gc/src/zgc/relocate.rs` (new) | 1a, 1b, 1c, 1d, 1e, 3a |

**3a has a good in-tree precedent.** `gc/src/g1_concurrent.rs` (675 lines) plus
`gc/src/satb.rs` (1234 lines) is a working background-worker concurrent marker
against a real collector, including the drain contract that
`concurrent-gc-maturation.md` Step 3 fixed (G1's `remark` must call
`flush_all_thread_satb_buffers` before the shard drain, or a mutator's unspilled
thread-local buffer is excluded from the remark snapshot → UAF). ZGC's
concurrent mark will need the equivalent, and should reuse `satb.rs` rather than
grow a second implementation.

**3b is the highest-risk item in this plan and has NO in-tree precedent.**
G1's evacuation is stop-the-world and single-threaded under one big lock
(`g1.rs` `young_collection`/`mixed_collection` hold `self.regions.lock()` for
the entire pause); the generational young gen's Cheney copy is also STW. Nothing
in this tree has ever moved an object while mutators were running. That means
3b is simultaneously the piece with the most novel correctness surface and the
piece with no working example to copy. Sequence it last, land it behind its own
sub-flag, and expect it to be the phase that needs the most validation budget.

### Phase 4 — integration debt *(correctness landmine, not a nicety)*

| WS | What | Owned files | Gated on |
|---|---|---|---|
| **4a** | TLAB support for the ZGC backend | `gc/src/tlab.rs`, `gc/src/vm_heap.rs` | 1d |
| **4b** | Audit and populate every neutral `VmHeap::Zgc` arm | `gc/src/vm_heap.rs` | 1e |

`gc/src/vm_heap.rs` has **58 literal `VmHeap::Zgc` arms** plus the `dispatch!`
macro's own arm (`vm_heap.rs:228`), which expands across ~40 more call sites.
Many of the literal arms are neutral placeholders chosen because
`ZgcRealHeap` is non-moving, and **they stop being correct the instant ZGC moves
an object.** The specific landmine:

* **`ZgcRealHeap::collect_garbage` returns an always-empty pointer map**
  (`gc/src/zgc.rs:2464`: `let pointer_map: HashMap<usize, usize> = HashMap::new();`,
  returned in `GcResult` at `:2478`). Its own doc comment (`zgc.rs:1394`) says
  this is "exactly correct for a non-compacting collector" — which it is,
  today. A moving ZGC that still returns an empty map means every consumer of
  the map (`monitors.remap_after_gc`, and the `pointer_map`-keyed liveness
  checks at `vm_heap.rs:2189` and `:2375`) silently sees "nothing moved" while
  objects moved. That is a use-after-free with no error path.
* The **affirmative** arms are worse than the `None` ones, because a `None`
  routes the caller to a slow generic path while a `true` asserts a fact:
  `vm_heap.rs:1809`, `:2304`, `:2344` answer `true` for ZGC, and `:1694`/`:1711`
  answer `!self.needs_gc()`. Each needs re-deriving under a moving collector.
* `conservative_addr_span` (`vm_heap.rs:527`) returns `None` because ZGC keeps
  live bases in a registry rather than a contiguous arena — with real pages
  (1d) this becomes answerable, and answering it is a stack-scan speedup.
* `jit_card_table_info` (`:1381`), `old_gen_info` (`:1594`), `old_gen_lock`
  (`:1605`), `collect_young_to_old_roots` (`:1619`) and `enable_concurrent_gc`
  (`:1633`) are all "generational-only, ZGC returns nothing" — every one of them
  becomes live work in Phase 2/3.

**4b is not a cleanup task. It is the phase that decides whether Phase 3b
corrupts the heap.** Do the audit before 3b lands, not after.

`4a`: `ZgcRealHeap` took the arena lock on every allocation when this was
written. **Done, and ahead of this plan: TLABs landed 2026-08-08** — see
`gc/src/zgc/tlab.rs` and `ZgcRealHeap::alloc_raw_tlab`, default-on, kill switch
`CRATONVM_ZGC_TLAB=0`. The hypothesis above (that lock contention accounted for
part of the 35-class HANG group) was never tested against the HANG group before
the mechanism changed underneath it, so treat the group as un-attributed rather
than as explained. What TLABs *did* introduce is a **reservation** the live-byte
GC trigger cannot see, which is a new failure mode of its own and cost a Tomcat
class the whole 2 GB heap on 2026-08-13.

### Phase 5 — validation and rollout

| WS | What | Gated on |
|---|---|---|
| **5a** | Re-run the 1975-class Spring Boot suite per phase; track PASS/HANG/FAIL against the 2026-08-07 baseline | any of 2–4 |
| **5b** | Triage the 11 PASS → FAIL classes, which are **probably not all GC** | 0b |
| **5c** | First-ever Hibernate ORM run against ZGC | 2 |
| **5d** | Default-on decision | 5a parity |

**5b first, because it is cheap and it may shrink the problem.** The 11 FAIL
classes fail fast (1–24 s, not timeouts), so they are a different mechanism from
the HANG group. The one class inspected closely — `VirtualZipDataBlockTests` —
reads like a fixture/working-directory or zip-content bug, not memory
corruption. Re-run those 11 under Generational with the same binary back to back
before assuming they share a cause with each other or with the HANG group.

**5c: the Hibernate ORM suite has never been run against ZGC.** The only
full-suite ZGC data that exists is the Spring Boot autoconfig set in the
2026-08-07 record. Hibernate is a different allocation profile and is the more
likely place a moving collector's remaining root-coverage gaps surface.

**5d gates on pass-rate parity with Generational, not on a date.** The bar is
the Generational arm of the same suite on the same host (1902/1975 = 96.3%),
and ZGC must reach it with no new HANG class. Until then `zgc` stays default-off
and the docs keep saying so.

> **5d was taken out of this plan's hands — dated 2026-08-13.** The default was
> flipped to ZGC on 2026-08-10 on the strength of the *Tomcat* three-way
> comparison, not this bar: the Spring Boot parity gate above **was never
> satisfied and was never formally waived — it was overtaken**. Re-establishing
> it is Phase 1 of
> [`zgc-maturity-assessment-and-plan-20260813.md`](zgc-maturity-assessment-and-plan-20260813.md),
> and that phase carries this bar forward verbatim, including its consequence:
> if ZGC is not at parity, the default is what should be revisited.

---

## 3. Dependency graph

```
Phase 0 ──────────────────────────────────────────────────────────
  0a CI binary build            [DONE]
  0b metrics.rs                 [no blockers — START NOW]
  0c doc de-staling             [DONE]

Phase 1 ── all four START NOW, no blockers, no shared files ──────
  1a vaddr.rs      ─┐
  1b barrier.rs    ─┼─ (1b exposes a trait; 1c consumes only the trait)
  1d page.rs       ─┤
  1e forwarding.rs ─┘
  1c JIT barrier    ← 1b (trait only)

Phase 2 ── needs real pages ──────────────────────────────────────
  2a generation.rs  ← 1d
  2b remset.rs      ← 1d

Phase 3 ─────────────────────────────────────────────────────────
  3a concurrent mark on real heap ← 0b, 1b
  3b concurrent relocate          ← 1a,1b,1c,1d,1e,3a   [HIGHEST RISK]

Phase 4 ─────────────────────────────────────────────────────────
  4a TLABs                        ← 1d
  4b vm_heap arm audit            ← 1e   [MUST precede 3b]

Phase 5 ─────────────────────────────────────────────────────────
  5b FAIL triage   ← 0b
  5a suite re-runs ← any of 2,3,4
  5c Hibernate     ← 2
  5d default-on    ← 5a parity
```

**Startable immediately with zero blockers: 0b, 1a, 1b, 1d, 1e** — five
workstreams, five agents, no shared owned file. 1c unblocks as soon as 1b's
trait exists (not its implementation). Everything in Phase 2 and 4a unblocks on
1d alone, which makes **1d the critical path** — it is the single highest-leverage
module in the plan and should be staffed first if staffing is scarce.

`gc/src/vm_heap.rs` is contended: **4a and 4b both touch it**, and so does
anything that adds a `VmHeap` method. Serialize them or hold the file.

---

## 4. Risk register

| # | Risk | Why it matters | Mitigation |
|---|---|---|---|
| **R1** | **The young-gen hypothesis is unverified.** | The entire justification for prioritizing Phase 2 is one unmeasured inference from 35 timeouts. If the 35 HANG classes are actually the arena-lock/no-TLAB path, or a livelock relative of the `gc_rearm` mode already fixed in `zgc.rs`, then Phase 2 is a large investment against the wrong cause. | 0b before 2a. Pull GC count + phase times from a sample of the 35 before committing. |
| **R2** | **Compressed oops are structurally incompatible with colored pointers.** | `gc/src/compressed_oops.rs` narrows a pointer to 32 bits with a shift; ZGC colored pointers spend high bits on mark/remap/finalizable colors and rely on multi-mapping the same physical page at several virtual addresses. Both want the same bits. HotSpot resolves this by making ZGC and compressed oops mutually exclusive. | Decide explicitly in 1a. Most likely outcome: `-XX:+UseZGC` forces compressed oops off, and that must be enforced at config-parse time with a clear message, not discovered as corruption. |
| **R3** | **JIT barrier coverage is all-or-nothing.** | A load barrier that the interpreter honors and compiled code does not is not a partial barrier — it is a broken one, because the first tier-up silently drops the invariant. This is why 1c is its own workstream. | 1c ships with a coverage assertion over JIT reference-load emit sites, and a debug mode that refuses to compile a method containing an unbarriered reference load. |
| **R4** | **Feature-gate rot.** | Already happened once: `zgc` was uncompilable from 2026-08-05 to 2026-08-07 and CI did not notice, because every `zgc` job was library-scoped. `synthetic-jdk` lost 1,522 tests the same way. Every new `gc/src/zgc/*.rs` module is behind the same gate. **The risk INVERTED on 2026-08-10** when `zgc` joined the default set: ordinary CI now compiles and tests it, and the configuration nothing builds is `--no-default-features` — i.e. the Generational-only fallback that every ZGC-less deployment runs. | 0a's two `ci.yml` steps must stay, and they are now the *cheap* half. Any new module needs its tests inside `cargo test -p cratonvm-gc --lib --features zgc` (`ci.yml:495`), and any new *flag* needs the binary build (`ci.yml:491`). The compile hole this row was written about is closed from the other side too: `feature-matrix.yml` runs `cargo check --workspace --all-targets --no-default-features`, so a `#[cfg(feature = "zgc")]` typo cannot go unnoticed. What is still uncovered is *running* that configuration — no job executes a `--no-default-features` binary, so a ZGC-less launcher is compiled-but-never-started on every commit. |
| **R5** | **Concurrent relocation has no in-tree precedent.** | Every existing CratonVM collector moves objects only at a stop-the-world. 3b is novel correctness surface with no working example, and it depends on five other new modules being simultaneously correct. | Sequence 3b last. Land it behind its own sub-flag, default-off — and note that since 2026-08-10 this is the ONLY thing keeping it off a user's machine, because the `zgc` feature that used to be the outer default-off gate is now default-ON. The sub-flag is no longer belt-and-braces; it is the belt. |
| **R6** | **The `VmHeap::Zgc` neutral arms are silent.** | 58 arms plus a macro; the affirmative ones (`true`, `(0,0)`) assert facts that are only true for a non-moving collector, and none of them will fail loudly when they become wrong. | 4b before 3b. Prefer `unimplemented!()` over a neutral value for any arm whose correct moving-collector answer is not yet known — a panic is cheaper than a UAF. **The original wording of this cell said "a panic in a default-off feature", and that qualifier expired on 2026-08-10**: this feature is default-on, so any such panic reaches every user on the default collector. It must therefore sit behind 3b's own default-off sub-flag, not behind the `zgc` feature. |

---

## 5. What this plan does not cover

* **The simulation half of `zgc.rs` (lines 1–1395) is not deleted by this plan.**
  It is the API shape Phase 1 is re-implementing against real storage, and
  `GenerationalZgc`'s policy is directly reused by 2a. It can be retired once
  Phase 2 lands, and not before. Its `warn_zgc_simulation_selected()` (`zgc.rs:103`)
  must survive as long as any type in it is constructible.
* **G1 maturation** — owned by [`concurrent-gc-maturation.md`](concurrent-gc-maturation.md),
  which picks G1 as its target and explicitly defers production ZGC to its §8.
* **The regression record** —
  `zgc-real-fullsuite-regression-RETIRED-20260808.md`
  is a run record. It is history. Phase 5 adds new records beside it rather than
  editing it.
* **`gc/src/zgc.rs`'s address-keyed state has never been reviewed.** The
  address-keyed review that covers the rest of the `gc` crate explicitly
  excluded it as "a distinct model that deserves its own pass". That pass is
  still owed, and Phase 1 makes it larger, not smaller.
