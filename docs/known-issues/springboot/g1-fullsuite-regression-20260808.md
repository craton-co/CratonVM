# G1 vs. Generational, full Spring Boot suite — the 14-class diff re-measured, 2026-08-10

**Status: OPEN, but rewritten — and the defect is now known to be JIT-dependent (§3b).** The 2026-08-08 page (kept verbatim at the
bottom) framed this as "14 classes changed status, G1 essentially at parity".
Re-running all 14 on a fresh `dev` binary, two ABBA-interleaved rounds per
class, says something different and much narrower:

* **8 of the 14 rows do not reproduce at all.** Most of those classes pass on
  *both* collectors, several of them at 400–900 s — i.e. far past the suite's
  300 s per-class budget, which is what made a single-run comparison flip them.
* **3 rows are one G1 defect**, not three: a live object's header reads back
  all zeroes under G1 and never under Generational.
* **1 row reproduces with the polarity reversed** — the page credits G1 with an
  improvement that is really a *Generational* failure.
* **1 row reproduces but is already owned** by its own open page.

The G1 defect is characterized much further than the original page managed, but
it is **not fixed**, which is why this page stays open. See §4 for the exact
open question, the reproducer, and the levers.

## 1. Method

Binary `cratonvm-g1reg-20260809.exe` built at `dev@11889c718`; the diagnostic
arm is `cratonvm-g1diag-20260810.exe` (same tree plus the `WalkTrail` commit).
Windows, `-Xmx 2g`, one class per process, `default` vs `-XX:+UseG1GC` only.

Two deliberate departures from the original run, both of which matter:

* **900 s budget, not 300 s.** At 300 s, "slow" and "stuck" are the same
  observation. Every run below records wall time, per-process CPU time and a
  host-load counter, so a TIMEOUT with `cpu ≈ wall` (burning CPU) is
  distinguishable from one that is blocked.
* **Two ABBA-interleaved rounds per class** (round 1 default-then-G1, round 2
  G1-then-default), because this box is shared — other sessions were running
  their own VM binaries throughout, at load 15–53.

## 2. Results

| Class | 2026-08-08 page | default (this run) | G1 (this run) | Verdict |
|---|---|---|---|---|
| `CacheAutoConfigurationTests` | PASS → FAIL | **PASS 3/3** (592/810/836 s) | **5 bad / 6** — FAIL 2/59 ×4, SIGSEGV ×1, PASS ×1 | **reproduces — G1 defect §3** |
| `ChildManagementContextInitializerAotTests` | PASS → FAIL | **PASS 5/5** (395–567 s) | **FAIL 3/5** | **reproduces — G1 defect §3** |
| `Log4J2LoggingSystemTests` | FAIL → HANG | **PASS 61/61** (744 s) | **FAIL 7/51 + containersFailed=1**, 445 guard hits | **reproduces — G1 defect §3** (louder than the page's row) |
| `CloudFoundryActuatorAutoConfigurationTests` | HANG → PASS | **TIMEOUT 4/4** (900 s, cpu 827–898 s) | **PASS 3/3** (307/341/359 s) | **reproduces, polarity reversed — §5** |
| `QuartzEndpointWebIntegrationTests` | HANG → FAIL | TIMEOUT (cpu 1017 s) | FAIL 29/45 | reproduces; already owned — §6 |
| `SpringApplicationTests` | PASS → HANG | PASS 544 / 658 s | PASS 641 / 695 s | does not reproduce |
| `BindConverterTests` | PASS → FAIL | PASS 9.4 / 11.1 s | PASS 10.6 / 12.8 s | does not reproduce |
| `HikariDataSourceConfigurationTests` | PASS → HANG | PASS 485 s | PASS 615 s | does not reproduce |
| `ConfigDataEnvironmentPostProcessorIntegrationTests` | HANG → PASS | PASS 402 s | PASS 409 s | does not reproduce |
| `ConfigurationPropertySourcesTests` | HANG → PASS | TIMEOUT 900 s (cpu 882) | TIMEOUT 900 s (cpu 886) | does not reproduce |
| `JettyWebServerFactoryCustomizerTests` | HANG → PASS | PASS 148 s | PASS 141 s | does not reproduce |
| `KafkaAutoConfigurationTests` | HANG → PASS | PASS 375 s | PASS 330 s | does not reproduce |
| `ConfigurationMetadataAnnotationProcessorTests` | HANG → PASS | FAIL 61/65 (33 s) | FAIL 61/65 (37 s) | does not reproduce (fails identically on both arms here — a local-environment gap, not a collector effect; not chased) |

**On the eight non-reproducing rows.** Note the wall times: `SpringApplicationTests`
544–695 s, `HikariDataSourceConfigurationTests` 485–615 s, `ConfigData…` 402–409 s,
`KafkaAutoConfigurationTests` 330–375 s — all PASS on both arms, all well past
300 s. A class that needs ~2× the budget is a coin flip at that budget on either
collector, and a one-run-per-arm comparison records the coin flip as a collector
regression. That is the correct reading of most of this page's original table, and
it is also what the superseded 2026-08-07 page found for two of these same classes
by the same method.

Historical support, from the 283 tracked `results.tsv` files (all
**default-collector** runs, no G1 variable anywhere): 11 of these 14 classes
already flip status run-to-run with no collector change at all — e.g.
`SpringApplicationTests` PASS×6/HANG×4/FAIL×1, `Log4J2LoggingSystemTests`
PASS×4/FAIL×4/HANG×3, `ChildManagementContextInitializerAotTests` PASS×2/FAIL×3.

## 3. The one real G1 defect: a live object's header reads back all zeroes

Three of the classes above are the same bug. Under G1 only:

```
WARN cratonvm::gc::guard: g1::get_field: out-of-bounds field read dropped
(returning null) obj=0x27ddb7c7408 index=20 num_slots=0 class_id=ClassId(0)
```

`class_id=0, num_slots=0` is a zeroed header on an object the mutator still
holds a reference to. `g1::get_field`'s guard then returns null rather than
striding a bogus grid, so Java sees a null field.

| Class | G1 guard hits | default guard hits |
|---|---:|---:|
| `Log4J2LoggingSystemTests` | **445** | 0 |
| `CacheAutoConfigurationTests` | 4–8 | 0 |

Java-visible consequences, all zero on the default arm:

* `NullPointerException: Cannot invoke "ch.qos.logback.classic.spi.TurboFilterList.size()"
  because "this.turboFilterList" is null` (×10–11) — a field logback assigns in
  its field initializer, so it can never legitimately be null;
* `ISPN000327: Cannot find a parser for element 'infinispan'` (×9) — Infinispan's
  parser registry coming up empty;
* one `EXCEPTION_ACCESS_VIOLATION (0xC0000005)` at 272 s, whose dump has ASCII
  class-name bytes (`"g/Objec"`, `"statisti"`, `"ctor"`) in shadow-stack slots —
  reads landing in a region that was reset and reused.

`CacheAutoConfigurationTests` fails **identically** on every bad G1 run: the same
two tests (`infinispanAsJCacheWithConfig`, `infinispanCacheWithConfig`).

### Where it comes from

`CRATONVM_G1_DBG_REACH=1` reports the upstream event directly:

```
[g1] rset-source walk DESYNCED: region=167 type=Eden reuse_epoch=0
  recycled_in_generation=0 offset=0xcad0 cursor=0x100000 — the bytes there are
  not an object header ... Abandoning the walk; the pause continues.

[g1][DBG-REACH] pause=0 young-serial: LIVE-REACHABLE field[0]
  holder=0x1fce0811a00 (cid=146 slots=4 region=Some(168) off=0x70a00)
  -> 0x1fce0811a50 is ZEROED (region=Some(168))
```

An Eden region that was **never recycled** (`reuse_epoch=0`) has a hole in its
object grid. A linear walk cannot resynchronize, so it is abandoned — and every
reference living past that offset is therefore never rewritten by the pause,
which is what leaves live objects pointing at memory that a later pause resets
and zero-fills (`G1Region::reset` → `self.data.fill(0)`).

The `WalkTrail` diagnostic added on this branch names the step. Both clean cases:

```
region=7   ... 0x53be0+0x40(cid=472) 0x53c20+0x40(cid=4044482304,k=1) 0x53c60+0x10(cid=0,k=0)
           break at 0x53c70  obj_size=0x100010 slots=65536
region=116 ... 0x20500+0x190(cid=51) 0x20690+0x168(cid=4044482304,k=1) 0x207f8+0x10(cid=0,k=0)
           break at 0x20808  obj_size=0x100010 slots=65536
```

`cid=4044482304` is `0xF111E700`, the walkable `int[]` TLAB-retire filler (one
below `GAP_FILLER_CLASS_ID = 0xF111E701`). So in both regions the walk strides a
retired TLAB's filler correctly, then lands on a **zeroed 16-byte pseudo-object**
(`HEADER_SIZE` = 16, `num_slots=0` ⇒ recorded size 16) which is embedded in real
data — 16 bytes later the bytes decode as `slots=65536`, and the walk breaks.

## 3b. The corruption is JIT-dependent — bisect so far

The single most useful fact about this defect, established after the first
write-up of this page. Same binary, same class, G1 throughout:

| arm | result | `gc::guard` hits | walk desyncs |
|---|---|---:|---:|
| G1 + JIT | FAIL 18/61 | **563** | many |
| G1 + **`--nojit`** | **PASS 61/61** | **0** | **0** |
| G1 + JIT, `CRATONVM_NO_JIT_INLINE_TLAB_NEW=1` | FAIL 16/61 | 307 | 12 |
| G1 + JIT, `CRATONVM_NO_JIT_TLAB_ZERO_ELISION=1` | **SIGSEGV** | 97 | 8 |
| G1 + JIT, `CRATONVM_JIT_DISABLE_INLINE_NEW=1` | still corrupt † | 0 † | 8 |

† this arm produced **zero** `g1::get_field` guard hits but is not clean: a
*different* guard fires instead —
`interpreter::invoke: Stale pointer detected in invokevirtual receiver
(ptr=..., all-zero header)`, repeatedly, on the same address. Reading a
single guard's count as "fixed" would have been a false green here.

So: **turning the JIT off makes G1 completely clean on a class that fails 18/61
with it on** — but none of the three JIT allocation gates accounts for it. The
inline object allocator (`emit_inline_tlab_new`, `jit/src/x64/objects.rs:634`) is
therefore *not* the source, despite its own comment documenting a previously
confirmed heap corruption of exactly this shape (a compact `body_size` baked as
an immediate at compile time going stale when the layout is replaced). Worth
noting for whoever picks this up: that emitter's `layout_replace_guard` is emitted
**only when `compact_snapshot` is `Some`**, and by its own comment "new class
REGISTRATIONS don't bump it, only replacements do" — a site compiled while the
class had no registered compact layout bakes the LEGACY size and emits no guard
at all. That is a real hole; it is simply not the one causing this.

### The shape the trails converge on

Every desync ends the same way — the last step before the break is a `0x10`-sized
`cid=0,k=0` read, i.e. the walker consuming **exactly one 16-byte all-zero
header** (`HEADER_SIZE` = 16) and then landing inside that object's body, reading
body bytes as a header:

```
region=119 ... 0x19188+0x28(cid=0,k=1)  0x191b0+0x20(cid=4044482304,k=1)  0x191d0+0x10(cid=0,k=0)
           break at 0x191e0
region=7   ... 0xa4340+0x20(cid=148,k=0) 0xa4360+0x10(cid=0,k=0)
           break at 0xa4370
```

Note `0x19188+0x28(cid=0,k=1)` — an Array with `class_id=0` but a real size.
So the object grid is acquiring entries whose **header is zero while their body
holds real data**: space reserved and neighbours allocated after it, but
`class_id`/`num_slots`/`kind` never written (or cleared). That is a
partially-initialized object made visible to a heap walk, and it is what both the
`g1::get_field` guard and the `invokevirtual` stale-receiver guard are seeing from
their two different directions.

## 4. What is ruled out, and the open question

Three hypotheses were tested and **refuted** — each is recorded because each one
looked right:

1. **"G1 defers young statics out of the root set."**
   `VmHeap::metadata_pin_deferrable` answers `is_old_gen_addr(addr)` for
   Generational but **`true` unconditionally for G1**, while G1's only consumer
   of `metadata_pin::roots_for_loader` is inside the *concurrent marker*
   (`g1.rs`), never an evacuating pause. That is a real asymmetry and it fits the
   symptom (both Java victims are static-held). It is **not the cause**: a control
   run with `CRATONVM_LOADER_UNLOAD=0` — which makes `conditional_loader_metadata`
   false so nothing is ever deferred — still produced 8 guard hits and the same
   two test failures. (The asymmetry may still deserve its own look; it is simply
   not this bug.)
2. **"A walker is missing a TLAB skip-site."** All nine G1 walk sites check
   `jit_tlab_skip_span_len` then `gap_filler_len`, in that order (g1.rs 755/760,
   4516/4523, 4702/4709, 4866/4871, 5057/5062, 5157/5162, 5867/5872, 6404/6409,
   8159/8164). Audited, consistent.
3. **"Unaligned object sizes desync the stride."** `array_data_size` rounds
   payloads up to 8 and `HEADER_SIZE`/`ARRAY_DATA_OFFSET` are 16, so
   `object_total_size` is 8-aligned; the walker's stride and the allocator agree.

4. **"It is one of the JIT's allocation fast paths."** Three gates tested
   individually, all still corrupt — see §3b. `emit_inline_tlab_new` is out.
5. **"A dying worker thread abandons its TLAB without retiring it."** The most
   promising lead yet, and wrong. The evidence for it was strong: a
   `WALKBRK-COVER` hexdump showed the break sitting in a ~900 KB **zeroed** span
   below `region_cursor=0xfffa8`, immediately after a correctly-retired TLAB's
   8-byte `GAP_FILLER` sentinel, with **"ZERO published skip spans this pause"** —
   i.e. no live thread claimed that memory:

   ```
   0x207f0 = 0x00000008f111e701   GAP_FILLER sentinel (cid=0xF111E701, len=8)
   0x207f8 = 0x0000000000000000
   0x20800 = 0x0000000a00000000
 >>0x20808 = 0x0001000000000000<< class_id=0 num_slots=0x10000 -> obj_size 0x100010, BREAK
   0x20810..0x20830 all zero
   ```

   And `vm/src/vm/vm_exec.rs` really did have the asymmetry: the main-thread
   teardown retires before its `clear_tlab_addr`, the spawned-worker teardown
   did not — so a worker withdrew from `collect_reserved_tlab_tails` having
   never stamped a filler. Adding `jvm_thread.tlab.retire()` there **did not
   fix it**: 18/61 failures, 239 and 252 guard hits, 8 walk breaks per run,
   unchanged. The retire is kept (it closes the asymmetry on its own terms and
   is idempotent) but is explicitly *not* the fix.

**Open question:** with the JIT on, what leaves an object in an Eden region whose
**header is all zero while its body holds real data**, with neighbours allocated
after it (so the region cursor moved past it)? `--nojit` never produces one. The
region's `[0, cursor)` covers the whole TLAB carve including any un-initialized
part, which is what `Tlab::reserved_tail` + `collect_reserved_tlab_tails` +
`G1Collector::set_jit_tlab_skip_regions` exist to cover — but those spans are a
per-pause transient, cleared after the collection, whereas what is observed here
desyncs the same region on later pauses.

Two concrete next steps, in order:

* Report, at the desync, whether the landing offset lies inside any *currently
  published* skip span. If it does not, the span belongs to no live thread —
  pointing at a TLAB abandoned without a retire (thread teardown) rather than at
  a walker gap.
* Walk the remaining JIT gates the same way §3b walked the allocation ones,
  starting with `CRATONVM_NO_JIT_ALLOC_CLASS_CACHE=1`.
* **Explain the ~16 zero bytes that sit immediately after a TLAB filler.** This
  is the one invariant across every trail, and it is the desync point:

  ```
  region=119  0x191b0+0x20(cid=4044482304,k=1)   int[] TLAB-retire filler, ends 0x191d0
              0x191d0+0x10(cid=0,k=0)            16 all-zero bytes
              break at 0x191e0
  region=7    0x5f120+0x28(filler)  0x5f148+0x10  0x5f158+0x10  break at 0x5f168
  region=117  0x207f0+0x8(k=71 gap sentinel)  0x207f8+0x10  break at 0x20808
  ```

  A retired TLAB's filler covers `[cursor, end)`; the next allocation in the
  region should begin exactly at `end`. Instead there is a whole-`HEADER_SIZE`
  multiple of zeroes first, and then live data. `Tlab::new` trims `end` down to
  8-alignment, which can only account for ≤7 bytes — so the gap between a
  TLAB's `end` and the region's next object is the thing to instrument.
  Compare `G1Collector::refill_tlab`'s `actual = requested_size.min(remaining)`
  and `G1Region::bump_alloc`'s start-alignment against what `Tlab::new` then
  claims to own.

  **Not the clue it first looks like:** a trail entry such as
  `0x19188+0x28(cid=0,k=1)` — an Array with kind and size but `class_id = 0` —
  is *strode correctly* (0x19188 + 0x28 = 0x191b0) and is not the desync point.
  Worth explaining eventually, but it is not what breaks the walk.

  Two candidate producers are already **excluded**, so do not re-tread them:
  - The inline object emitter. `emit_inline_tlab_new`
    (`jit/src/x64/objects.rs`) writes `class_id` **and** `num_slots` inline and
    then commits the TLAB bump last — *"Commit the bump LAST … x86-64 TSO
    preserves the required header/body-before-cursor store order"* — on both
    the `skip_post_init_helper` and `tlab_post_init` arms. Consistent with §3b,
    where disabling it did not stop the corruption. **The comment at
    `jit/src/x64/bytecode_walk.rs:10040` still describes the pre-redesign
    sequence ("writes `class_id` … then tail-calls `jit_post_tlab_init` to
    finish header") and is stale — delete it; it cost this investigation a
    wrong lead.**
  - An inline array allocator. There isn't one: `newarray`
    (`bytecode_walk.rs:9950`) and `anewarray` (`:10173`) both go through
    runtime helpers, which allocate via the normal `alloc_array` path.

  So the producer writes an Array header through some path that sets kind and
  length but leaves `class_id` zero, or zeroes `class_id` afterwards. Grep the
  writers of `ObjectKind::Array` headers and the evacuation copy path, and
  consider a `ClassId(0)` argument reaching `alloc_array` rather than a torn
  write.

**Do not read a single guard's count as a verdict** — the
`CRATONVM_JIT_DISABLE_INLINE_NEW=1` arm shows `g1::get_field` hits dropping to
zero while the `invokevirtual` stale-receiver guard fires throughout.

**Reproducer:** `Log4J2LoggingSystemTests` (`core/spring-boot`) under
`-XX:+UseG1GC` — 445 guard hits, ~510 s, far cheaper and louder than
`CacheAutoConfigurationTests`. Add `CRATONVM_G1_DBG_REACH=1` for the trail.
Other levers: `CRATONVM_DBG_G1DIAG=1` (region census),
`CRATONVM_G1_COVERAGE_PIN=1` (if the failure survives G1 moving nothing, it is
not a relocation the root set failed to cover).

## 5. `CloudFoundryActuatorAutoConfigurationTests` — the page has this backwards

| arm | result |
|---|---|
| default | **TIMEOUT 4/4** at 900 s (cpu 827–898 s, still logging progress at the kill) |
| G1 | **PASS 3/3** — 14/14 tests in 307 / 341 / 359 s |

This is a stable ≥3× asymmetry in G1's favour, so the page's "HANG → PASS" row is
real — but it is a **Generational** throughput failure, not a G1 improvement, and
it belongs on the Generational side of the ledger.

The default arm logs `[moving-young] fallback #1024` — every young collection
diverting to the degraded non-moving sweep, reasons
`innermost-rbp-belongs-to-unguarded-callee` and `unregistered-jit-frame-on-stack` —
while the G1 arm logs none. **That is suggestive, not proven**: a CLOSED Tomcat
page (`fixed-suite-bugs/tomcat/gc-moving-young-persistent-nonmoving-fallback-regression-CLOSED.md`)
already disproved that exact mechanism for a different workload, where forcing
`CRATONVM_NO_MOVING_YOUNG=1` cost nothing measurable and GC-side work was 0.4 % of
the run. The A/B/C that would settle it here (default vs
`CRATONVM_NO_MOVING_YOUNG=1` vs G1, with `--dump-phase-report=` and
`CRATONVM_GC_STATS=1`) is set up but has not produced a clean set yet.

## 6. Cross-references

* `QuartzEndpointWebIntegrationTests` — its own open page,
  [`quartzendpointwebintegrationtests-recurring-timeout-hang-20260807.md`](quartzendpointwebintegrationtests-recurring-timeout-hang-20260807.md),
  whose history table already records HANG under default, G1 *and* ZGC on
  2026-08-07. Not a collector row.
* `Log4J2LoggingSystemTests` also has
  [`log4j2-logback-loggingsystemtests-modifiedclasspath-throughput-hang-20260807.md`](log4j2-logback-loggingsystemtests-modifiedclasspath-throughput-hang-20260807.md)
  for its *throughput* behaviour. The G1 failure in §3 is a different, harder
  symptom (memory corruption, not slowness) and belongs here.
* `HikariDataSourceConfigurationTests` —
  [`hikaridatasourceconfigurationtests-pool-start-hang-20260807.md`](hikaridatasourceconfigurationtests-pool-start-hang-20260807.md).
  It passes on both arms here (485 / 615 s), so this page adds nothing to it.
* The superseded page,
  [`g1-fullsuite-regression-RETIRED-20260807.md`](../../internal/fixed-suite-bugs/springboot/g1-fullsuite-regression-RETIRED-20260807.md),
  closed the previous edition by exactly this method and had already found
  `BindConverterTests` green under G1 and
  `ChildManagementContextInitializerAotTests` *worse* under the default collector.
* The ZGC companion, [`zgc-real-fullsuite-regression-20260808.md`](zgc-real-fullsuite-regression-20260808.md),
  shares 9 of these rows. Its "collector-agnostic timeout-boundary noise" reading
  is consistent with §2 for the non-reproducing rows; its own rows have not been
  re-measured here.

## 7. Affected classes

`CacheAutoConfigurationTests`, `ChildManagementContextInitializerAotTests` and
`Log4J2LoggingSystemTests` under `-XX:+UseG1GC` (§3, one defect).
`CloudFoundryActuatorAutoConfigurationTests` under the default collector (§5).
Everything else in the 2026-08-08 table is disposed of in §2.

---

## The 2026-08-08 page, verbatim

# G1 vs. Generational, full Spring Boot suite — rerun 2026-08-08 on a clean binary

**Status: OPEN — characterized, not root-caused.** Supersedes
[`g1-fullsuite-regression-RETIRED-20260807.md`](../../internal/fixed-suite-bugs/springboot/g1-fullsuite-regression-RETIRED-20260807.md)
(that page's own findings are still valid and closed; this is a fresh
comparison, not a reopening). The prior comparison ran against a binary that
turned out to have a near-total heap-corruption bug (`gen_heap::read_slot:
corrupt Value cell`, fixed 2026-08-07 by two commits — the inline allocator's
mark-word write and a thin-unlock quartet clobber). This run is the first
clean, apples-to-apples G1-vs-Generational comparison since those fixes
landed.

## Summary

Same binary (`cratonvm-*-20260807e.exe`, built at `dev@7ea883be8`), same
1975-class Windows full suite, `-Xmx 2g`, 300s/class timeout, single shard
(no sharding-related host-contention variable) — the only difference between
runs is `-XX:+UseG1GC` vs. the default (unspecified → Generational):

| | Generational (default) | G1 |
|---|---:|---:|
| PASS | 1853 (93.8%) | 1854 (93.9%) |
| FAIL | 65 | 69 |
| HANG | 13 | 8 |
| CRASH | 0 | 0 |
| EMPTY | 43 | 43 |
| BOTH-FAIL | 1 | 1 |
| Wall time | 32412s (~9.0h) | 28213s (~7.8h) |
| **Total** | **1975** | **1975** |

Only **14 classes changed status** — far tighter than the pre-fix
comparison's ~50, and G1 is essentially at parity with Generational now (69
vs 65 FAIL, and *fewer* HANGs: 8 vs 13). This matches the "G1 is wired into
the safepoint driver" maturity note from the original page.

Results:
`apps/spring-boot-suite-runner/.suite/results/craton-fullsuite-default-20260807e/all-jit/results.tsv`
vs.
`apps/spring-boot-suite-runner/.suite/results/craton-fullsuite-g1-20260807e/all-jit/results.tsv`.

## The 14 changes

| Class | Module | Generational | G1 |
|---|---|---|---|
| `SpringApplicationTests` | `core/spring-boot` | PASS | HANG |
| `BindConverterTests` | `core/spring-boot` | PASS | FAIL |
| `ChildManagementContextInitializerAotTests` | `module/spring-boot-actuator-autoconfigure` | PASS | FAIL |
| `CacheAutoConfigurationTests` | `module/spring-boot-cache` | PASS | FAIL |
| `HikariDataSourceConfigurationTests` | `module/spring-boot-jdbc` | PASS | HANG |
| `ConfigurationMetadataAnnotationProcessorTests` | `configuration-metadata/spring-boot-configuration-processor` | HANG | PASS |
| `ConfigDataEnvironmentPostProcessorIntegrationTests` | `core/spring-boot` | HANG | PASS |
| `ConfigurationPropertySourcesTests` | `core/spring-boot` | HANG | PASS |
| `CloudFoundryActuatorAutoConfigurationTests` | `module/spring-boot-cloudfoundry` | HANG | PASS |
| `JettyWebServerFactoryCustomizerTests` | `module/spring-boot-jetty` | HANG | PASS |
| `KafkaAutoConfigurationTests` | `module/spring-boot-kafka` | HANG | PASS |
| `Log4J2LoggingSystemTests` | `core/spring-boot` | FAIL | HANG |
| `QuartzEndpointWebIntegrationTests` | `module/spring-boot-quartz` | HANG | FAIL |
| `BasicErrorControllerIntegrationTests` | `module/spring-boot-webmvc` | HANG | FAIL |

5 genuine regressions (PASS -> FAIL/HANG), 6 classes that improve
(HANG -> PASS), 3 that swap one bad status for another (still not a clean
pass either way).

## Cross-reference: 9 of these 14 are the *identical* change under ZGC too

See the companion doc, retired 2026-08-10:
[`zgc-real-fullsuite-regression-RETIRED-20260808.md`](../../internal/fixed-suite-bugs/springboot/zgc-real-fullsuite-regression-RETIRED-20260808.md).
Two **collector-agnostic** defects were root-caused there, and one of them is
live for G1 too, so this page's own residuals are worth re-measuring before
being triaged as G1 behaviour: a generated-`$ProxyN` cache that kept handing
out `ClassId`s after class unloading had removed them, which surfaces as
`ClassCastException: ? cannot be cast to …` in any collector that reaches
`memory::roots::conditional_loader_metadata` — G1 included, via
`gc_quiescence::class_unload_marking()`. (The other, ZGC's missing
reference-array un-box, is G1's already-fixed `42ce72b18` hole seen from the
read side, so G1 is unaffected by it.)
`ConfigurationMetadataAnnotationProcessorTests`, `SpringApplicationTests`,
`ConfigurationPropertySourcesTests`, `Log4J2LoggingSystemTests`,
`CloudFoundryActuatorAutoConfigurationTests`,
`JettyWebServerFactoryCustomizerTests`, `KafkaAutoConfigurationTests`,
`QuartzEndpointWebIntegrationTests` and `BasicErrorControllerIntegrationTests`
all move in the exact same direction under both G1 and ZGC. That is strong
evidence these 9 are **collector-agnostic** — either timeout-boundary noise
(the 6 HANG->PASS ones are all classes close enough to the 300s ceiling under
Generational that a different collector's timing nudges them under budget —
consistent with the "borderline-slow, tipped over under load" pattern this
suite has already documented for several of these exact classes) or a shared
non-Generational-path bug (the 3 that get worse: `Log4J2LoggingSystemTests`
FAIL->HANG, `QuartzEndpointWebIntegrationTests` and
`BasicErrorControllerIntegrationTests` HANG->FAIL). Not independently
confirmed which explanation applies to which class this round.

Two classes appear in both diffs but with a *different* concrete outcome —
worth noting as still-linked, not coincidence:

- `ConfigDataEnvironmentPostProcessorIntegrationTests`: G1 HANG->PASS, ZGC
  HANG->FAIL. Same starting HANG, different collector-specific landing spot.
- `HikariDataSourceConfigurationTests`: G1 PASS->HANG (300.183s, TIMEOUT), ZGC
  PASS->FAIL (256.111s, `AssertionError`). This class already has an open,
  unresolved doc from 2026-08-07
  (`hikaridatasourceconfigurationtests-pool-start-hang-20260807.md`) — the
  ZGC arm finally erroring out at 256s rather than running the full 300s is
  consistent with the same underlying slow/stuck mechanism, not a new one.

## Not investigated further this round

None of the 5 G1-only regressions were individually root-caused — this doc
is a characterization pass, matching the ZGC companion doc's scope. Worth a
follow-up triage pass the way the earlier 08-06/08-07 default-GC HANG/FAIL
classes got one.

**Update 2026-08-10** (from the ZGC page's retirement round, same binary,
`-XX:+UseG1GC`, 2-way parallel): `BindConverterTests` now PASSes (8.9s).
`ChildManagementContextInitializerAotTests` (296s) and
`CacheAutoConfigurationTests` (337s, 15 failed) still fail; the latter fails on
Infinispan JCache context startup
(`UnsatisfiedDependencyException` behind `infinispanAsJCacheWithConfig`), which
is a different family from anything on this page. `HikariDataSourceConfiguration-
Tests` and `SpringApplicationTests` reproduce on the DEFAULT collector too in
that round, so they are not G1-only either. That leaves this page with two
genuinely open rows, not five.

## Affected classes

See the table above. Full per-class raw data in the two `results.tsv` files
linked at the top.
