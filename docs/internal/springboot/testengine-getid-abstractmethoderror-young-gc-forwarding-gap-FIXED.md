# AbstractMethodError on TestEngine.getId() — young-GC pre-forwarding walk drops live objects under heap pressure, masked as interface-dispatch failure

Status: OPEN
Severity: CRITICAL (fatal process crash — zero tests reported for the whole class)

## Summary

Two Spring Boot test classes fatally crash the CratonVM process during JUnit
Platform engine discovery/registration (before any `@Test` runs), both with
the identical final error:

```
Exception in thread "main" java/lang/AbstractMethodError: method org/junit/platform/engine/TestEngine.getId()Ljava/lang/String; has no Code attribute
	at SbRunner.main(SbRunner.java:36)
	at org/junit/platform/launcher/core/SessionPerRequestLauncher.execute(...)
	...
	at org/junit/platform/launcher/core/EngineExecutionOrchestrator.executeEngine(EngineExecutionOrchestrator.java:258)
```

`TestEngine` is a real SPI interface; its concrete implementations
(`JupiterTestEngine`, `VintageTestEngine`) have real, non-abstract `getId()`
bytecode and this bootstrap path works correctly in thousands of other
classes in the same rerun. This is **not** a missing-native-registration gap.

The immediate mechanism (confirmed from source) is a real invokevirtual
dispatch fallback in `vm/src/runtime/interpreter.rs` firing on a **corrupted
receiver** whose object header reads as all-zero. But the corruption itself
is upstream, in a **brand-new young-GC pre-forwarding walk added the same
day** (`gc/src/gen_heap.rs`), which is the actual root cause. This is a
**new regression from the just-merged `dev` GC work**, not a long-standing
interpreter bug.

## Symptom (full evidence)

Affected logs (module `spring-boot-hazelcast`, `spring-boot-http-client`,
rerun `rerun-20260716`, shard4):

- `apps/spring-boot-suite-runner/.suite/results/rerun-20260716/shard4/logs/module_spring-boot-hazelcast.org.springframework.boot.hazelcast.autoconfigure.HazelcastAutoConfig-813ae134e663.err.log`
  (class `HazelcastAutoConfigurationServerTests`)
- `apps/spring-boot-suite-runner/.suite/results/rerun-20260716/shard4/logs/module_spring-boot-http-client.org.springframework.boot.http.client.reactive.HttpComponentsClient-a40cd4dd99cd.err.log`
  (class `HttpComponentsClientHttpConnectorBuilderTests`)

**Both** logs show the identical GC warning immediately before the crash
cascade:

```
hazelcast: WARN cratonvm_gc::gen_heap: GC: young object-start walk stopped at an implausible extent young_cursor=14040 young_used=344079120
http-client: WARN cratonvm_gc::gen_heap: GC: young object-start walk stopped at an implausible extent young_cursor=664   young_used=420038960
```

In the http-client log (2.6 MB, 8530 lines), the timeline is:

- Line 98 (`20:49:19.380170`): the walk above fires — it registered only
  **664 bytes** of valid object-start addresses out of **420,038,960 bytes**
  of live young-gen data (0.0002% coverage).
- Lines 99-8493 (`20:49:19.484` → `20:49:19.631`, ~150ms, 6320 occurrences):
  a storm of
  `gen_heap::get_field/set_field: out-of-bounds field write/read dropped`
  (receivers reporting `class_id=0 num_slots=0 class_name=java/lang/Object`
  — i.e. all-zero headers) and
  `Stale pointer detected in invokevirtual receiver (ptr=..., all-zero
  header) — falling back to CP class <X>` for many distinct receivers
  (`AbstractQueuedSynchronizer$ConditionNode`, `jdk/internal/misc/Unsafe`,
  etc. — ordinary objects that happened to be live young-gen data at GC
  time, unrelated to JUnit).
- Line 8494 (same burst, no visible gap): the fatal
  `AbstractMethodError: TestEngine.getId() has no Code attribute`, from
  `EngineExecutionOrchestrator.executeEngine` — i.e. one of the corrupted
  receivers in this same burst was a `JupiterTestEngine`/`VintageTestEngine`
  instance.

The hazelcast log shows the same shape (33 "Stale pointer detected" lines,
same GC warning, same terminal `AbstractMethodError`), confirming this is
one cluster, not two coincidentally-identical bugs.

Ordering answer to the investigation prompt: the "Stale pointer" warning is
**not** later/unrelated noise — it (and the whole out-of-bounds storm) is the
direct, same-GC-cycle cause of the crash, occurring within ~150ms of, and
immediately following, the walk-abort warning.

## Root cause

### 1. The corruption source: `gen_heap.rs` pre-forwarding object-start walk

`gc/src/gen_heap.rs` around line 3606-3642 (added **today**, commit
`1c4aaa069f "fix(gc): close stream ArrayList pressure corruption"`, patched
same day by `fb15be63f6 "fix(gc): GAP_FILLER_CLASS_ID not special-cased in
new young-GC exact-walk loops"` — both are in this worktree's freshly-merged
`dev` history):

```rust
let mut young_object_starts: FxHashSet<usize> = FxHashSet::default();
let young_base = young_from.base_ptr() as usize;
let young_used = young_from.used();
let mut young_cursor = 0usize;
while young_cursor < young_used {
    let obj_ptr = (young_base + young_cursor) as *mut u8;
    let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
    if header.class_id.as_u32() == crate::tlab::GAP_FILLER_CLASS_ID.as_u32() {
        // ... skip GAP_FILLER sentinel gap ...
        continue; // or break if the sentinel's gap length looks wrong
    }
    let size = gen_object_total_size(header);
    if size < HEADER_SIZE || young_cursor.checked_add(size).is_none_or(|end| end > young_used) {
        tracing::warn!(young_cursor, young_used, "GC: young object-start walk stopped at an implausible extent");
        break;   // <-- walk aborts HERE, for the rest of the minor GC cycle
    }
    young_object_starts.insert(obj_ptr as usize);
    young_cursor += size;
}
```

Per the comment above this walk, `young_object_starts` exists so that "only
allocator-written headers can ever receive a forwarding pointer" during the
semispace copy — i.e. this set gates which live objects get forwarded when
the young generation is evacuated. When the walk **breaks early**, every
object located after the break point is excluded from
`young_object_starts`, and (per the stated design intent — "an interior
false positive must merely be ignored") never gets a forwarding pointer
written for it, even though it is live and referenced.

After the semispace flip, the old from-space memory (containing all those
un-forwarded-but-still-live objects) is reused/reclaimed as free space.
Any surviving reference into that region then reads back an all-zero (or
partially overwritten) header — exactly the flood of `class_id=0
num_slots=0 java/lang/Object` reads and `Stale pointer detected` messages
seen starting 100μs after the walk-abort warning.

**Why the walk aborts almost immediately** (664 bytes / 14040 bytes into a
300-400MB young generation): a second, older, and better-tested walk exists
in the same file (`gc/src/gen_heap.rs`, the `exact_cursor` loop around line
4631-4672, used by the mark phase) that walks the *identical* young-gen
layout but additionally calls `skip_free_blocks(&mut exact_cursor,
&mut exact_free_iter)` — consulting a real free-list/TLAB-remnant range
list — before ever interpreting bytes as an `ObjectHeader`. That mechanism
exists precisely because of the fragmented-TLAB-remnant work referenced in
`reference_tlab_remnant_fragmentation_wedge` (commit `cc268d700 "fix(gc):
serve fragmented young free-list remnants as smaller TLABs — bimodal
bt18"`) — young gen legitimately contains gaps that are free/TLAB-remnant
ranges, not all of which are overwritten with a `GAP_FILLER_CLASS_ID`
sentinel header.

The **new** `young_object_starts` walk added today only special-cases the
`GAP_FILLER_CLASS_ID` sentinel (via the `fb15be63f6` patch) — it never calls
`skip_free_blocks`/consults the free-list the way the established
`exact_cursor` walk does. When it encounters a free/TLAB-remnant range that
isn't marked with a `GAP_FILLER` sentinel, it misreads raw/stale bytes as an
`ObjectHeader`, computes a bogus `gen_object_total_size`, fails the extent
check, and **aborts the entire walk** rather than skipping just that one gap
— which is why coverage drops to a few hundred/thousand bytes instead of
hundreds of megabytes.

### 2. The masking mechanism: interpreter CP-class fallback (`vm/src/runtime/interpreter.rs` ~18242-18351)

When `execute_invoke` sees an invokevirtual/invokeinterface receiver with an
all-zero header, it does **not** treat this as fatal by default. It falls
back to resolving the method against `method_class_name` — the
constant-pool-declared class of the *call site* (e.g. `TestEngine`, from
`engine.getId()` inside `EngineExecutionOrchestrator`) — instead of the
receiver's real runtime class. This fallback is deliberate and, for
`java/lang/ClassLoader`, `java/util/Set`, `java/util/Map`, etc. (concrete or
resolvable-enough CP classes), recovers silently or with only a debug-level
log (see the `is_object_member`/`ClassLoader` special-casing at
interpreter.rs:18242-18351). But when the CP-declared class is an
**abstract class or interface** (as `org.junit.platform.engine.TestEngine`
is), the fallback method lookup finds only the interface's own abstract
`method_info`, which has no `Code` attribute — producing exactly
`AbstractMethodError: ... has no Code attribute`.

This fallback is a reasonable best-effort recovery for genuinely resolvable
cases and is not itself the root cause; it is the reason the corruption
surfaces as a semantically-plausible (if wrong) `AbstractMethodError`
instead of a null-deref/SIGSEGV. **Fixing the interpreter fallback alone
would not fix this bug** — it would just turn this specific symptom into a
different failure mode (a genuinely wrong `TestEngine` dispatch, or a
silent skip) while leaving the underlying GC data loss in place. The fix
belongs in `gen_heap.rs`'s new walk.

### Is this a regression?

Yes — with high confidence. `git blame` on the exact lines shows the
`young_object_starts` walk and its `GAP_FILLER_CLASS_ID` handling were
authored today (`victor-craton`, timestamps `2026-07-16T16:36:52Z` and
`2026-07-16T18:06:09Z`), landing in `dev` as commits `1c4aaa069f` and
`fb15be63f6`, both present in `git log --oneline -15 -- gc/src/gen_heap.rs`
for this worktree. This is exactly the kind of newly-merged GC change the
task description flagged as suspect. The existing, older `exact_cursor`
walk in the same file (which correctly consults the free-list) was
apparently not reused/mirrored when the new pre-forwarding walk was
written, reintroducing a class of gap the older walk had already solved.

### Why it's timing/pressure-dependent (why only 2 of thousands of classes)

Both crashing classes show a very large `young_used` at the moment of the
failing minor GC (`344,079,120` and `420,038,960` bytes — i.e. the young
generation was nearly full, implying heavy allocation pressure right as
JUnit's `EngineExecutionOrchestrator`/`ServiceLoader` machinery was
allocating engine/discovery objects). This is consistent with the bug being
triggered by encountering a fragmented free/TLAB-remnant range that only
appears under sustained allocation pressure (per the `cc268d700`
"fragmented young free-list remnants" mechanism) — not every minor GC hits
such a range early in the young generation's layout. This is **not** a
cross-thread race: `-Parallel` in `run-spring-boot-suite.ps1` is
process-level (one JVM/class per OS process; see
`apps/spring-boot-suite-runner/run-spring-boot-suite.md`), and nothing in
this evidence implicates JUnit's own internal parallel-execution engine
(neither log shows Jupiter parallel-execution configuration markers) — the
corruption is a single-threaded (single mutator, single GC) minor-GC
walk-abort bug.

## Repro

Both classes reproduce the crash deterministically as part of the shard4
rerun. To reproduce directly:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -Exe C:\craton\CratonVM-spring-boot-crashfail-20260714\target\release\cratonvm-spring-boot-rerun-20260716.exe `
  -Vm craton -Jit on `
  -ClassList <path-to-a-class-list-file-containing-just-these-two-rows> `
  -Parallel 1 -TimeoutSec 300
```

Where the class-list file contains the two rows (module, class) for:
- `spring-boot-hazelcast` / `org.springframework.boot.hazelcast.autoconfigure.HazelcastAutoConfigurationServerTests`
- `spring-boot-http-client` / `org.springframework.boot.http.client.reactive.HttpComponentsClientHttpConnectorBuilderTests`

(`-ClassList` accepts the same row format produced by `Build-ClassLists`;
see `apps/spring-boot-suite-runner/run-spring-boot-suite.ps1:36,458` and
`run-spring-boot-suite.md`.) Because the trigger is allocation-pressure/
layout dependent, a bare rerun of just these two classes may not reproduce
every time — the existing shard4 logs in `rerun-20260716` are the
authoritative repro artifacts; also try `-CratonArgs @('CRATONVM_DBG_A2=1')`
or lowering `-MaxHeap` (e.g. `512m`) to increase young-GC frequency and
odds of hitting a fragmented free range early in the walk.

To confirm the mechanism directly, add a debug assertion/print in
`gc/src/gen_heap.rs`'s new `young_object_starts` walk (~line 3636) dumping
the raw header bytes and the state of the corresponding free-list at the
break point, and cross-reference with `skip_free_blocks`'s free-range list
at the same offset — expect the break point to fall inside a known
free/TLAB-remnant range that is not `GAP_FILLER`-tagged.

## Suggested fix direction (not implemented — do not claim fixed)

Make the new `young_object_starts` walk in `gen_heap.rs` (~line 3606-3642)
consult the same free-list/TLAB-remnant range source that the established
`exact_cursor` walk (~line 4631-4672) already uses via `skip_free_blocks`,
instead of only special-casing the `GAP_FILLER_CLASS_ID` sentinel. Ideally
unify the two walks (or have the new one delegate to the old one's
resync logic) so this class of gap can't be missed by one walk while
handled by the other.

## Related

- `docs/internal/comparison-handoff/bug-interface-method-dispatch-no-code-attribute.md`
  — a **different**, already-documented interface-dispatch bug
  (`Collector.accumulator()`, `ServiceLoader$Provider.type()`) where the
  receiver is NOT corrupted and itable/vtable selection itself picks the
  wrong (abstract) slot. That bug is a pure dispatch-resolution defect;
  this one is corruption-triggered CP-class fallback on an otherwise-normal
  dispatch path. Worth cross-checking once one is fixed, since both produce
  the identical `AbstractMethodError: ... has no Code attribute` shape and
  could be confused for each other.
- `docs/internal/fixed-suite-bugs/gen-heap-young-gc-live-object-reclaim-rrwl-holdcount-FIXED.md`
  — an earlier, FIXED instance of the same *family* (live young objects
  reclaimed/zeroed while still referenced), but via the **non-moving**
  sweep/selective-promotion path, not this **moving**-young-GC
  pre-forwarding walk. Confirms "live object reclaim" is a recurring GC
  hazard class in this codebase, this time in a brand-new code path.
- `reference_tlab_remnant_fragmentation_wedge` (memory) — the fragmented
  free/TLAB-remnant mechanism (`cc268d700`) whose free-list the new walk
  fails to consult.
- `reference_osr_main_corruptor` (memory) — "many corruptor bugs are
  actually the non-moving young sweep"; this one is the moving-young-GC
  analogue of that pattern.
- Commits to inspect directly: `1c4aaa069f` (introduced the walk),
  `fb15be63f6` (patched it same-day for `GAP_FILLER_CLASS_ID`, insufficient).
