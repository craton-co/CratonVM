# GC_STRESS - residual corruption faces (FIXED)

**Status:** FIXED on dev (2026-07-09). The remaining Fork6Hard
`CRATONVM_REAL_FORKJOINPOOL=1` + `CRATONVM_DBG_GC_STRESS=65536` residual was
not a JIT allocation producer or compact-ref-field race. The real-FJP lane was
splitting ForkJoin state between real JDK queue/status bytecode and CratonVM
Bridge side-table natives, while GC did not scan/remap the side table and the
root-snapshot cache could reuse stale roots across recursive native re-entry.

**Fix:** keep and force the real-JDK ForkJoinPool/ForkJoinTask Bridge surface as
a single model (`commonPool`, `invoke`, `submit`, `externalSubmit`,
`fork`/`join`/`get`/result/status helpers), route recursive task execution
through the side-table path, scan/remap the side-table task/result references
during GC, pin the task across native `compute()` re-entry, and bypass/remap-clear
the frozen-frame root snapshot cache in the opt-in real-FJP lane. This removes
the real WorkQueue/CAS path from the repro while keeping real-FJP bootstrap
compatibility.

**Verification (Azure Linux probe host, branch
`codex/fix-gcstress-residual-faces-20260709-024700`, binary
`/data/data/cratonvm-probes/cratonvm-gcsres-fjpfullbridge-20260709-024700`):**

- `cargo test -p cratonvm-native-builtins fjp_gc_tests::gc_hooks_scan_result_and_remap_key_and_result -- --nocapture` - PASS.
- `cargo build --release -p cratonvm-cli` - PASS.
- `Fork6Hard 128 20`, default JIT, 12/12 - `ALL-OK`, 0 `mark_young` corrupt-header markers, 0 timeouts, 0 bad signatures.
- `Fork6Hard 128 20`, default JIT, 48/48 - `ALL-OK`, 0 markers, 0 timeouts, 0 bad signatures.
- `Fork6Hard 128 20`, `CRATONVM_DISABLE_JIT=1`, 12/12 - `ALL-OK`, 0 markers, 0 timeouts, 0 bad signatures.

Moved from `../../known-issues` to `..` per project policy. The
historical investigation below is retained for provenance.

## Repro

```
CRATONVM_REAL_FORKJOINPOOL=1 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 \
CRATONVM_DBG_GC_STRESS=65536 \
  <cratonvm>.exe --java-home "C:/Program Files/Java/jdk-25" \
  -cp docs/known-issues/repros/A4-fork6 Fork6Hard 128 20   # also --nojit
```

Load-nondeterministic: per the fork6-fjp doc's own history, distinguishing a
partial fix from noise on this lane needs ≳100 interleaved runs. A single
run failing is not by itself evidence against a fix.

### 2026-07-07 — MUCH cheaper repro: Hibernate `ZonedDateTimeTest` (no GC_STRESS lane needed)

Found while validating the archived stale-local doc: on the Linux probe host
(`hibernate-orm-harness`, default heap, default JIT, binary `cvgc0706-fix2` =
dev `3240cb75`), `org.hibernate.orm.test.type.temporal.ZonedDateTimeTest`
**livelocks before its first test result** in a continuous
`mark_young: rejecting object at 0x… with implausible extent NNN (kind=0, …)` +
`[A2] BREADCRUMB — NO allocation record covers 0x…` loop (one pair every ~1.5 s,
same address for the whole run — a garbage-header object stays reachable by the
young mark across every collection; the guard rejects it so there is no crash,
just no progress; 0 of the stale-local family's signatures present).
`OffsetDateTimeTest` intermittently logs 2 such rejections at CHANGING addresses
and still completes (exit=0) — the face is intermittent there, persistent-address
in Zoned. Sibling classes (`InstantTests`, `LocalDateTimeTest`, `OffsetTimeTest`)
are clean. Command shape:

```
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cratonvm> --java-home <jdk25> \
  "@<fixed common.args>" CratonRunner <listfile-with-ZonedDateTimeTest> 0
```

This gives face-1-style triage (implausible header / no allocation record) a
single-class, default-heap, watchable repro instead of the ≳100-run stress lane.

## 2026-07-07 re-assessment on current dev (commit `3a1a95b5`)

Re-ran the lane on the Azure Linux probe host (real-JDK jdk25, shared host
under load) after ~600 commits of dev movement since the 2026-07-03 rounds.
Binary `cratonvm-gcs-20260707`, worktree `wt-gcstress-20260707`.

### Current reproduction rate (56 Fork6Hard `128 20` GC_STRESS=65536 runs)

- **0 crashes** (no SIGSEGV, no `CompactValue` panic, no `exceeds 47-bit`) —
  the severe faces the doc opened for are gone. The intervening fixes that
  matter: the map-node stale-local family + `2dfdfddc` plain-field 16-byte
  slot-tearing (landed 2026-07-06, *after* this doc's last round) + the GC
  guard/recovery band-aids now **contain** the corruption instead of
  crashing.
- 19/56 completed fully clean (`ALL-OK reps=20`); 32/56 completed but with
  ≥1 rep failure (workload caught the stale receiver as
  `ClassCastException`, `NoSuchMethodError` e.g. `String.fork()`, or
  `IllegalStateException: nullchild[a,b)`); 5/56 wedged (STW takeover waits
  forever after a worker died on a stale receiver → `rc=124`).
- nojit is now *much* cleaner than the doc's era (1/10 corrupt + 1 wedge vs
  "dozens of corrupt-cell reads"); JIT still shows the face at ~4/12.

### Both leading producer hypotheses REFUTED

Controlled 12-run lanes, identical config, only the lever changed:

| lane | runs with corruption |
|------|----------------------|
| baseline | 4/12 |
| `CRATONVM_JIT_DISABLE_INLINE_NEW=1` | 3/12 |
| `CRATONVM_COMPACT_REF_FIELDS=0` | 3/12 |

Disabling JIT inline-`new` does **not** reduce the corruption, so the
JIT-inline-allocation header-ordering path — the prime suspect carried in
`docs/known-issues/jit-inline-alloc-array-header-corruption-hibernate-batch.md`
and its memory note — is **not** the producer of *this* Fork6 residual.
Disabling compact-ref-fields likewise does nothing, ruling out the
compact-object `GC_FLAG_COMPACT`/`array_length` write window. And the face
reproduces under `--nojit` too, so it is not JIT codegen at all — it is in
the shared core (interpreter write path / GC root-scan / remap).

### Corrupt-header signature decoded

The `mark_young: rejecting object … implausible extent 0 (kind=0,
array_len=513, num_slots=0)` warnings all point at a young address whose
`array_length` dword equals the **high 32 bits of the address itself**
(e.g. addr `0x201_07000620` → `array_length=0x201=513`). Reading a Value
cell `[disc|payload32|payload64]` as an `ObjectHeader` puts the payload's
high dword at the header's `array_length` offset (12). So the walker is
following a **stale/wild reference in a live object's slot** into young
memory that holds another young pointer, and mis-decoding it — i.e. the
doc's face-1 "stale pointer in a Value cell," confirmed, not a torn header
write. `[A2] BREADCRUMB — NO allocation record covers …
(freed+reused past the ring)` confirms the target was a real allocation
that died and had its slot reused while a live holder still referenced it:
a **missed root / missed remap**, the same class as the blocked-thread /
`fork6-fjp` A4 register-resident-root family
([fork6-fjp-multithread-jit-root-reclamation.md](fork6-fjp-multithread-jit-root-reclamation.md),
[dohead-jit-heap-corruption-register-invisibility.md](dohead-jit-heap-corruption-register-invisibility.md)).

### The cheap ZonedDateTimeTest repro face is now CLOSED

The 2026-07-03 "much cheaper repro" (Hibernate `ZonedDateTimeTest`
livelocking forever in an `implausible extent` / `BREADCRUMB` loop at a
persistent address) **no longer reproduces**: the class now completes in
~170 s with **0 corruption markers, 0 stale-pointer warnings**. Its
remaining failures are an unrelated functional bug (441× `NullPointerException:
… "typeNamePattern" is null` — a `DatabaseMetaData`-shaped null, no GC
signal), not this corruption family. Drop ZonedDateTimeTest as a face-1
repro; it was closed by the same 2026-07-06 slot-tearing / map-node wave.

### Face 2 did not reproduce

`expected object reference, got int(512)` (the JIT lost-tag int-in-ref-slot
face) did **not** appear in any of the 56 runs. Either it is rarer than the
sampled window or was also closed by the intervening wave; it stays listed
below but is now unconfirmed on current dev.

### Precise-JIT-oop-maps do NOT fix this residual (tested)

Correcting an earlier characterization of this bug as "the deferred
precise-jit-stack-maps work": that machinery is **built, not deferred**, and
turning it on does **not** close this residual.

- Provenance: built by `d6bf8104` (Stage 3 exact-RBP frame reg + safepoint-id
  + precise relocation); flipped **default-ON** by `5b8864a0` ("fixes
  SB-CRASH-04 A3 / ReflRepro A2 / Fork6 A4"); flipped **default-OFF** by
  `d53c0e96` because the per-call/per-safepoint codegen is a ~6× throughput
  tax (BUG-01,
  `docs/internal/app-jvm-bugs/bug-01-junit-reflection-heavy-jit-frame-scan-throughput.md`).
  As of 2026-07-07 `precise_jit_maps_enabled()` was **re-flipped default-ON**
  (opt out `CRATONVM_NO_PRECISE_JIT_MAPS=1`) because the BUG-01 ~6× tax is gone on
  current dev; `precise_maps = precise_jit_maps_enabled() || moving_young_enabled()`.
  Turning precise on does NOT fix this residual either way (below), and an
  interleaved load-controlled Fork6 A/B is 23/25 ALL-OK on vs 22/25 off (neutral).
- Even when precise maps map every frame *slot*, `OopMapEntry` still has **no
  register-oop bitmap** (`jit/src/lib.rs:52-73`): a register-only oop is
  covered not by the map but by `emit_pre_safepoint_spill` (spills locals to
  slots, only at *call* safepoints) + the xt-takeover's conservative 16-GPR
  scan of *running* in-JIT peers. The leak is a caller-saved/RAX/
  operand-scratch oop live at a *non-call* (loop back-edge) safepoint or in a
  skipped caller frame.
- **Empirical test on this lane** (`CRATONVM_PRECISE_JIT_MAPS=1`, 12 runs):
  corruption did **not** drop — **10/12 runs corrupt vs baseline 4/12**, with
  per-run marker counts jumping to 8-215 (still 1 wedge + 2 rep-fails).
  Precise-on roots more frames, so the young mark follows *more* of the same
  stale references (over-retention), surfacing more guard rejections rather
  than fewer. The lane is load-nondeterministic (grade the ratio loosely) but
  the direction is unambiguous: `CRATONVM_PRECISE_JIT_MAPS=1` is **not** the
  fix here. (It IS the fix for bt18/A3 — golden checksums — so this Fork6
  multi-threaded residual is either a partly-different bug or a gap the maps
  don't cover.)

### Net

The residual is a stale-reference / missed-remap bug in the multi-threaded
`REAL_FORKJOINPOOL` GC path — **not** the JIT allocation codegen (refuted),
and **not** closed by turning precise JIT oop maps back on (refuted, above).
It presents mostly as contained guard warnings, sometimes as a rep-level
Java exception, rarely as a wedge; it never crashes on current dev. Two
distinct sub-problems remain: (JIT) a register-only oop escaping the
spill+takeover coverage at a non-call safepoint — needs the register-oop
bitmap `OopMapEntry` still lacks, not just the existing frame-slot maps; and
(`--nojit`, zero JIT frames) a separate interpreter-side missed-root / A1
stale-local, sparse after the map-node + `2dfdfddc` slot-tearing wave. Kept
OPEN here rather than retired because the underlying corruption is unfixed.

## Residual faces

> **2026-07-03 investigation round (branch `fix/gcstress-residual-diag-20260703`,
> binaries `cvmp-residual-diag{,2,3}-20260703.exe` with symbols
> [`strip="none"`+`debug="line-tables-only"`, local Cargo.toml tweak, not
> committed]):** new `CRATONVM_DBG_CELLCORRUPT` holder-identification
> diagnostics landed, TWO more real bugs fixed, face 3 root-caused and
> CLOSED, and face 1 narrowed to a specific writer hunt. Details below.

1. **Stale bootstrap-era raw pointer in Value cells — NARROWED, writer still
   unidentified.** `CRATONVM_DBG_CELLCORRUPT` identifies the holders: real,
   live objects (`Fork6Hard$StrTask` num_slots=6 at indexes 0/2/4/5;
   `ForkJoinWorkerThread$InnocuousForkJoinWorkerThread` Thread mirror
   num_slots=22 at index 19 ≈ `Thread.holder`) whose 16-byte Value cells
   contain `{raw 8-byte pointer, 0}` where the pointer targets a REAL early
   bootstrap object (a `java/lang/String`, plain Objects) — usually in the
   FLIPPED young semispace (`young_to`), i.e. a pre-Cheney address. The
   tagged reader rejects the untagged cell → returns null → the
   `nullchild` / "Cannot read field threadStatus because holder is null" /
   NPE faces; remap walkers can't decode the cell either, so it stays stale
   forever. Corruption happens while the holder is YOUNG; promotion then
   copies the corrupt cell verbatim to old gen.
   **Excluded writers:** `set_array_element` on a stale array ref (a
   CELLCORRUPT-gated non-array trap was live in the run that produced a
   corrupt cell and did not fire); the `ensure_system_stdin_object` stale
   write (fixed, see below — removed SOME producers: post-fix runs show
   fewer/none in some windows, but the face still reproduces).
   **Remaining suspects:** raw 8-byte writers — `write_prim_element`
   callers outside set_array_element, `Unsafe.putLong/putReference*` byte-
   vs-slot offset translation, arraycopy fast paths. Next: write-side trap
   in `write_prim_element` (Reference writes with holder-header check) or a
   memory watchpoint on a corrupt-cell address (they are deterministic).
   **2026-07-03c:** a `Fork6Hard StrTask`-triggered occurrence of this face
   was observed to fully HANG the process (ForkJoinPool workers parked
   forever, ~45s total CPU burned over 3+ hours wall-clock — not a spin
   loop), not just surface as NPE/nullchild as previously documented. The
   worker that hit the stale-pointer receiver fell back to a placeholder
   `java/lang/Throwable` (interpreter's stale-pointer recovery path) and
   the pool then appears to wait forever for that task's real completion
   signal, which the fallback never produces. Repro binary
   `cvmp-mapfix2-20260703.exe` (unrelated fix on board — see below), lane
   `Fork6Hard 128 3 --nojit GC_STRESS=65536`, hang began ~11s into the run
   and was killed after 3h+ idle. Not yet confirmed whether non-stress or
   shorter Fork6Hard runs also hang on this face or only fail fast; add to
   the fork6-fjp A4 investigation as a new symptom class.
2. **JIT lost-tag int-in-ref-slot.** `JIT dispatch into ForkJoinTask.doExec
   failed: expected object reference, got int(512)` + walker headers like
   `kind=Object array_length=512 num_slots=4 class_id=6` — an A4-family
   tag/layout confusion under JIT, distinct from GC reclamation. May be
   related to the still-open fork6-fjp A4 register-only residual (see
   [fork6-fjp-multithread-jit-root-reclamation.md](fork6-fjp-multithread-jit-root-reclamation.md)) —
   unconfirmed. Untouched this round (all diagnosis ran `--nojit`).
3. **Bootstrap `set_field` OOB write-drop — ROOT-CAUSED AND FIXED
   (2026-07-03).** The `CRATONVM_DBG_OOBFIELD=Object` backtrace named
   `ensure_system_stdin_object` (vm_util.rs): the freshly allocated
   System.in FileInputStream was held in a Rust local across the
   FileDescriptor class-load/`<clinit>`/alloc window (the A1 "Rust local
   across allocating calls" family); a stress-triggered MOVING young GC in
   the window relocated it and the subsequent `set_field` wrote through the
   stale pre-move address — then cached the stale ref in
   `shared.system_in`, which was ALSO missing from both the root scan and
   the GC remap (out/err had both), poisoning every later use. Fixed
   (pin + re-read via `native_pin_roots`; `system_in` rooted + remapped;
   plus a latent RwLock self-deadlock found & fixed in the process: the GC
   paths now `try_read`/`try_write` the stream caches because the
   initializers hold the write guard across allocating calls — the naive
   `read()` wedged instantly, 0-output). Post-fix, the bootstrap write-drop
   is gone from all runs (0 hits vs deterministic-first-line before).

4. **Map/HashMap Node alloc-then-stale-local family — FIXED, unrelated to
   face 1.** Separately from this investigation, an audit of native map
   mutators found the classic A1 "bare `ObjectRef`/`Value` local held
   across an allocating call" bug in `native_map_put`, `map_resize`,
   `native_lhm_put`, `lhm_alloc_node`, `native_tm_put`, and
   `map_alloc_node` (`../../../native-collections/src/lib.rs`) — each wrote a
   key/value/bucket-array local into a freshly `alloc_object`'d node
   *after* the allocation, without pinning, so a moving young GC during
   the alloc could relocate the arg and the write would store a stale
   (dangling) reference. Fixed by pinning + re-reading via
   `native_pin_roots` across each allocation (six commits on
   `fix/gcstress-residual-diag-20260703`). Validated on the aggressive
   GC_STRESS lane: went from dozens of corrupt-Value-cell reads + many
   CELLCORRUPT holder dumps + nullchild/NPE THREWs (pre-fix) to zero
   escalating corruption (0 corrupt-cell reads, 0 holder dumps) across two
   full runs post-fix. One PRE-COPY-only flag persisted **byte-identically**
   (same `src_obj`/`cell`/`raw` bytes) in both the pre- and post-
   `map_alloc_node` runs — since it never escalates to an actual corrupt
   read and its class (`class_id=414`, `num_slots=7`) doesn't match the
   map `$Node` layout (`NODE_NUM_FIELDS=4`), it appears to be an unrelated,
   likely-benign, deterministic bootstrap-time guard trip — not pursued
   further here.

`CRATONVM_DBG_MTROOTS=1` (per-GC initiator dump + blocked census),
`CRATONVM_GC_ARRAY_GUARD_BT=1`, `CRATONVM_DBG_SWEEP_ZERO=1` (returned no hits
on this lane — that detector covers the young non-moving sweep, not the
old-gen concurrent sweep, so it would not have caught the now-fixed defects
either; confirm before trusting a "clean" `SWEEP_ZERO` run on any old-gen
corruption).

5. **ES `BufferUnderflowException.cause` -> live-but-wrong `Object`, single-
   threaded, no `GC_STRESS` needed (2026-07-10).** Found independently while
   investigating
   `docs/known-issues/elasticsearch-suite/ES-FAIL-FAMILY-20260710-vector-codec-exception-cause-object.md`
   (`ES93FlatBFloat16VectorFormatTests.testMultiClose`, default heap, no
   `CRATONVM_REAL_FORKJOINPOOL`/`CRATONVM_DBG_GC_STRESS`, reproduces under
   both JIT-on and `--nojit`). Different symptom shape from faces 1-4 (a
   *valid, live* `Value::Object` pointer lands in the field, not a raw
   untagged pointer or a lost tag) but the same `CRATONVM_DBG_CELLCORRUPT`
   tool caught the same underlying signature: a `dump_corrupt_cell_holder`
   shift-test proving an **8-byte element-addressing misalignment** on a
   1-element `Object[]` array (its element decodes as garbage at the nominal
   offset, valid 8 bytes earlier) — the same "header/field-cell aliasing,
   off-by-8-within-a-16-byte-`Value`-cell" shape as
   `../gaps/bc-math-ec-gc-0x4-handoff.md` §4's structural clue.
   Confirmed NOT a GC-relocation/self-forwarding bug (new regression test
   `self_referential_field_survives_promotion` in `../../../gc/src/gen_heap.rs`
   passes cleanly; the victim object's address is provably identical at
   write and read time on the real repro) and NOT a plain interpreted
   `putfield` (new `[WATCHFIELD]` java-stack hook at `Instruction::Putfield`
   never fires). Full evidence, a new dynamic-watchpoint mechanism
   (`cratonvm_gc::heap::{set_dynamic_watch,dynamic_watch_addr}` +
   `NativeContext::dbg_set_watch_cell`), and `CRATONVM_DBG_CAUSE` are on
   branch `fix/es-vector-codec-exception-cause-object-20260710` (not yet
   merged). **Worth trying against this face-1/bc-math-ec cluster too**: the
   ES repro is single-threaded and deterministic (no stress lane, no load
   dependence) — likely the cheapest reliable repro this whole investigation
   has had so far for the shared "off-by-8" structural clue.

## Next steps

1. Reproduce each face in isolation with a targeted, deterministic repro
   (parallel to `HwBlocked.java` in `repros/A4-fork6`) rather than the noisy
   aggressive stress lane. **Face 5's ES repro may already BE this** — it is
   already single-threaded, default-heap, and deterministic; try it first
   before building a new one.
2. For face 1: check the young-arena `grow()`/realloc path for a stale
   pre-grow address baked into any long-lived structure (thread-local cache,
   GC diagnostic sample, etc.) that survives past the grow.
3. For face 2: determine whether this is the SAME register-only oop gap as
   A4, or a distinct tag-tracking bug in the compact-ref-fields layout under
   `CRATONVM_REAL_FORKJOINPOOL`.
4. For face 5 (and possibly 1): build the hardware watchpoint
   `bc-math-ec-gc-0x4-handoff.md` §6.2 recommended but never built (VEH
   infra already in `../../../vm/src/runtime/crash_handler.rs`) — software
   watchpoints have now been tried and found wanting on two separate
   corruption hunts in this codebase.
