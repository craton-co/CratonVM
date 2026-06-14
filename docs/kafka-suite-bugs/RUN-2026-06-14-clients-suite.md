# Kafka 4.4 `clients` suite on CratonVM — run 2026-06-14 (IN PROGRESS)

Fresh full run of the Apache Kafka **4.4.0-SNAPSHOT** `kafka-clients` unit suite
(**439 top-level `*Test` classes**) under CratonVM, on a quiet machine, booting
**JDK 25** (the bug-19 fix — see [bug-19](bug-19-bufferpool-blocking-hang.md);
without a JDK ≥19 boot, every Runnable-target thread no-ops and the whole suite
hangs). This supersedes the 3.7.0 numbers in [HANDOFF.md](HANDOFF.md).

## Harness (`apps/kafka/ksuite/`)
- `KRun.java` — programmatic JUnit-Platform launcher; runs every class passed as
  an arg in ONE JVM, flushing a `RESULT <class> found/succ/fail/skip/abort status`
  line per class. Disables ServiceLoader auto-registration of launcher extensions
  (kafka's `KafkaPostDiscoveryFilter` fails to instantiate standalone).
- `run.sh` — boots JDK 25, runs classes **batched** (15/JVM) to amortize
  CratonVM's large per-JVM JUnit-init cost; on a batch crash/hang it records the
  dying class and **re-runs the batch survivors individually** (crash isolation).
  Singleton lock so duplicate launches can't contaminate results. Never stops on
  failure. Outputs: `results/results.tsv`, `results/crashes.log`,
  `results/failcauses.log`.
- Build: `clients:testClasses` + `testFixturesJar` + `test-common-util:jar` via
  gradle (JDK 17), classpath from `clients/build/testcp.txt` (+ testFixtures dirs).

## HotSpot baseline (classpath sanity)
BufferPoolTest 13/13, MemoryRecordsTest 484/484, UtilsTest 71/72 (1 env test) —
so CratonVM-only failures below are meaningful.

## Status classes
`OK` all pass · `FAIL` ≥1 test/container failure · `ABEND` VM exited with no
RESULT · `TIMEOUT` external timeout (true hang) · `EMPTY` no tests discovered ·
`LOADERR` class load/discovery threw.

## ✅ RECHECK after dev fixes (binary 2026-06-14 00:48, watchdog OFF) — 21 known-bad classes
Re-ran the 21 first-pass FAIL/ABEND classes against the rebuilt `dev` binary
(landed: JIT escape-analysis on primitive loads (bug-25), reflect+collections
**primitive generic-param mirrors + real Hashtable$Entry enumeration**, GC
forwarding/TLAB fixes). Result: **9 of 21 fixed or unblocked.**

**FIXED → now OK:**
- `NetworkClientTest` — was ABEND (Mockito **subclass-mock** hang) → **OK 43/43**
- `AdminFetchMetricsManagerTest` — was FAIL (NPE `tags on null`) → **OK 2/2**
- `AllBrokersStrategyTest` — was FAIL (NPE `brokers on null`) → **OK 5/5**
- `ListTransactionsHandlerTest` — was FAIL (NPE `filteredProducerIds`) → **OK 10/10**
- `ListConsumerGroupOffsetsHandlerTest` — was FAIL (ordering) → **OK 14/14**

→ **B-B (generated `*RequestData` null) and the Mockito subclass-mock path (B-A)
are largely fixed by the reflect+collections change.**

**IMPROVED (hang→completes, or fewer fails):**
- `ClientUtilsTest` — ABEND (Mockito **static-mock** hang) → **FAIL 12/13** (now
  completes; 1 real `WrongTypeOfReturnValue`). Static-mock hang gone.
- `NodeApiVersionsTest` — ABEND(false) → **FAIL 11/13** (2 NPE `apiKey on null`)
- `ListOffsetsHandlerTest` 2→1 fail · `FetchSessionHandlerTest` 6→5 fail

**STILL FAILING (correctness, unchanged):** `MetadataTest` (concurrency),
`AdminApiDriverTest` + `DescribeConsumerGroupsHandlerTest` (B-E ordering),
`PartitionLeaderStrategyIntegrationTest` (caching), `ConsumerConfigTest`
(B-C path-traversal), `FetchSessionHandlerTest` (B-D Uuid topicId),
`NodeApiVersionsTest`/`ListOffsetsHandlerTest` (residual B-B null).

**STILL CRASH/HANG (consumer + discovery family):**
- `KafkaAdminClientTest`, `ConsumerRecordsTest` → **TIMEOUT** (>300 s; B-G discovery / slow)
- `KafkaConsumerTest`, `KafkaShareConsumerTest`, `KafkaShareConsumerMetricsTest`,
  `RangeAssignorTest` → **ABEND** (genuine crash now — watchdog off)
- `CooperativeStickyAssignorTest` → **LOADERR**

### B-I — ServiceLoader 4-arg ctor `NoSuchMethodError` — BENIGN (noise) — FIXED
Original hypothesis (ServiceLoader signature mismatch crashes the consumer tests)
was **wrong**. `native-builtins/src/service_loader.rs` already probes the 4-arg
`ProviderImpl.<init>(…,AccessControlContext)` then falls back to the JDK-24+ 3-arg
form, so the `NoSuchMethodError` is a harmless WARN — proof: `RangeAssignorTest`
**passes 44/44 standalone with that WARN present**. The crashing classes only
ABEND/LOADERR in *batched* runs (collateral from B-J corruption + leftover-process
CPU starvation), not from ServiceLoader.
**Fix landed:** `service_loader.rs` now selects the ctor via `method_exists(...)`
instead of invoke-and-catch, so the misleading WARN (which caused this very
misdiagnosis) no longer fires on JDK 24+.

### B-J (new, real) — GC zeroes live `Class`/`Iterator` mirrors → all-zero header → misdispatch — Critical
Run any heavy consumer class standalone (`KafkaShareConsumerMetricsTest`,
`KafkaConsumerTest`, `ConsumerRecordsTest`) with the watchdog off and the heap
corrupts mid-run:
```
WARN interpreter: Stale pointer detected in invokevirtual receiver
     (ptr=0x…, all-zero header) — falling back to CP class java/lang/Class
WARN vm_exec: NoSuchMethodError method="java/lang/Object.isInstance(Ljava/lang/Object;)Z"
WARN vm_exec: NoSuchMethodError method="java/lang/Object.cast(Ljava/lang/Object;)Ljava/lang/Object;"
… ClassCastException: java/lang/Object cannot be cast to …TestExecutionResult$Status
```
**ROOT-CAUSED (2026-06-14, `CRATONVM_DBG_STALE_RECV=1` + forced GC `--Xmx 128m`):**
the stale receiver is **always a `java.lang.invoke.VarHandle`** — of 1008 captured
stale-recv events, **1000 are `VarHandle.set`, 8 `VarHandle.compareAndSet`, 0
anything else**. First trigger: `ConcurrentLinkedDeque.linkLast` →
`NEXT.compareAndSet(...)` where the static-final `VarHandle NEXT` reads back with
an all-zero header. The consumer tests hit it because `MockClient` /
`NetworkClientDelegate` use `ConcurrentLinkedDeque` for their request queue.

So B-J is specifically: **`VarHandle` objects get an all-zero header after a GC
move** — they are collected, or moved without remapping the `static final` slot
that holds them. Ruled out: the `VH_META_TABLE` side table is keyed by
`identity_hash_code` (stable across moves), so it is **not** the cause; the
object itself is gone.

**Recommended fix:** treat `VarHandle` objects as permanent GC roots the same way
`vm/src/memory/roots.rs` already does for class mirrors / interned strings /
primitive mirrors — register every VarHandle created by `lang_invoke.rs`
(`alloc_static_var_handle` / `alloc_instance_var_handle` / `findVarHandle`) in a
`SharedVm` registry that `collect_roots` scans **and** that the post-move remap
step rewrites. (Equivalently: pin VarHandles non-moving.) This matches the
existing convention for long-lived native-created singletons.

**FIX LANDED + VERIFIED (2026-06-14).** Added a `SharedVm::var_handle_roots`
registry (keyed by identity hash); every VarHandle is registered there at
creation via the single `lang_invoke.rs::vh_meta_put` chokepoint, the registry is
scanned in `memory/roots.rs::collect_roots` (keep-alive → object gets copied into
the GC pointer-map) and remapped in `memory/gc.rs::update_all_roots` (alongside
class mirrors). A new no-op-default `NativeContext::register_var_handle_root` is
overridden in `vm_exec.rs`. Files: `vm_init.rs`, `roots.rs`, `gc.rs`,
`native-api/registry.rs`, `vm_exec.rs`, `lang_invoke.rs`.

Verification (isolated build `cratonvm-bjtarget`, watchdog off):
- forced GC `--Xmx 128m` on `KafkaShareConsumerMetricsTest`: VarHandle stale-recv
  **1008 → 0** (was 1000 `set` + 8 `compareAndSet`).
- default heap: **0 `Stale pointer` warnings, 0 panics** (was the dominant crash).
  The class now *times out* (slow — B-G/B-H + CPU contention) instead of
  corrupting → the corruption is gone, the remaining failure is unrelated.

### B-K (residual, separate) — moving young-GC runs under live conservative JIT frames → stale slot — Medium, deep
Under *extreme* forced GC (`--Xmx 128m`) **3** stale-recv events remain after the
VarHandle fix, on **transient** objects in **hot, JIT-compiled** methods:
`net.bytebuddy.jar.asm.ClassReader.readUTF8/readUnsignedShort` (ByteBuddy class
parsing, called thousands of times) and `ConsumerNetworkThread.cleanup`.

**ROOT-CAUSED (code analysis):** JIT frames have no precise oop maps yet
(`jit/conservative_roots.rs`), so the GC sees their object refs only via a
*conservative* stack scan — those refs keep objects alive but **cannot be
remapped** (a stack qword that looks like a pointer might be an `i64`). To stay
safe the young GC must run **non-moving** while JIT frames are live; the gate is
`gen_heap.rs:1990` → `if gc_quiescence::is_active() { sweep_young_non_moving() }
else { moving Cheney }`. B-K is the case where a **moving** collection runs while
a conservative JIT frame is live → the JIT-held object is relocated and its
conservative slot is left pointing at the zeroed from-space (all-zero header).

This happens because the gate's signal (`gc_quiescence`, gc-crate) can **desync**
from the actual JIT frame chain (`conservative_roots::push/pop_jit_entry`,
vm-crate): `is_active()` reads `false` while a frame is still live. The original
(mis-attributed) bug-19 watchdog dump is direct evidence — `quiescence depth=5
enter_count=43210 leave_count=43205`, a 5-deep enter/leave **imbalance**.

**Why NOT fixed here (unlike B-J):** these are transient objects, so the
`var_handle_roots` permanent-root pattern does **not** apply. The real fix is one
of: (a) balance/unify `gc_quiescence` enter/leave with the JIT frame push/pop so
they can't desync (or drive the gate off a gc-crate atomic frame-counter updated
by `push/pop_jit_entry`); or (b) land precise JIT oop maps. Both are changes to
the VM's most delicate subsystem (JIT/GC quiescence), and B-K is **flaky to
reproduce** (≈50 % at `--Xmx 128m`; smaller heaps no longer boot JDK 25 under the
current ~3 GB-free contention) — so a fix **cannot be verified in this session**
and must not be landed blind. Deferred to a dedicated JIT/GC session on an
uncontended machine.

**`--nojit` CONFIRMATION (2026-06-14, clean HEAD build, `--Xmx 128m` forced GC):**
| config | B-K stale-recv | outcome |
|--------|---------------:|---------|
| WITH JIT | **199** (regex `Pattern$Node.match/study`, `CharSequence.toString`, String/Thread/JUnit/Mockito objects; **0 VarHandle**) | corrupts, no RESULT |
| `--nojit` | **1** | **completes — `RESULT found=9 … status=FAIL`** |
A 199→1 collapse plus the class *running to completion* under `--nojit` empirically
confirms B-K is the conservative-JIT-frame-root gap: with no JIT frames every root
is precise, so a moving GC remaps correctly and the corruption disappears. The
single `--nojit` residual is a separate, far rarer gap (likely a native-call
pinning hole). So the real B-K fix is precise JIT oop maps **or** hardening the
moving-vs-non-moving gate so it can never run a moving collection while *any*
conservative JIT frame is live (the `gc_quiescence` enter/leave imbalance). Not a
registry-style fix; needs a dedicated JIT/GC session. This is also the same defect
class the dropped consumer/discovery classes (B-G/B-H neighbours) suffer under
load.

> Side note from this confirmation: the 10:56 rebuild that briefly showed VarHandle
> corruption "back" was a **build corruption** from the concurrent `CratonVM-clients`
> session (disk-full / git-swap mid-build), NOT a regression — a clean HEAD rebuild
> shows **0 VarHandle** stale-recv, i.e. the B-J fix is intact and effective on HEAD.

> NOTE: the fix is in the shared source tree but built/verified only in the
> isolated `cratonvm-bjtarget` (the main-target `.exe` is held by the concurrent
> `CratonVM-clients` session). A normal main-target rebuild will include it.
> Disk on `C:` hit 100% from the 3.1 GB + 1.1 GB STALE_RECV dumps — those have
> been deleted; do not run with `CRATONVM_DBG_STALE_RECV=1` without capping output.

---

## ⚠ METHODOLOGY CORRECTION (run v2) — the in-VM watchdog must be OFF
The first pass left CratonVM's **120 s in-VM watchdog enabled**, which fires on
*slow-but-finishing* classes (CratonVM's cold per-class boot + JUnit init is slow)
and aborts them as `ABEND` — **masking their real result**. Proof:
`NodeApiVersionsTest` was `ABEND` (leaf frame `Long.numberOfTrailingZeros`,
implying an infinite loop) but, re-run with `CRATONVM_DISABLE_DEFAULT_WATCHDOG=1`,
it **finishes** as `FAIL` with 2 real NPEs (`Cannot invoke apiKey on null`,
B-B family). The bit intrinsics themselves are **correct** (verified
`Long/Integer.numberOfTrailingZeros/numberOfLeadingZeros/bitCount/highestOneBit`
== HotSpot) — that leaf frame was just the watchdog's snapshot point.

Therefore run v2 sets `CRATONVM_DISABLE_DEFAULT_WATCHDOG=1` and uses the external
`timeout` as the sole hang detector (batch 12, batch_to 700 s, one_to 300 s).
**The first-pass `ABEND` counts below overstate true hangs; v2 numbers supersede.**
Note also: batched JVMs **accumulate threads across classes** (a consumer test's
background threads persist into later classes in the same JVM), so the
"1160/1221 threads dumped" counts are cumulative, not per-test.

## Running tally — first-pass partial (SUPERSEDED — full 439 run not completed)
The full 439-class run was deliberately **stopped early** to pivot into the
targeted fix work (recheck of the 21 known-bad classes → B-I/B-J fixes → B-K
root-cause). So there is **no completed full-suite AGGREGATE** for 4.4; the
numbers below are the first-pass *partial* (first ~63 classes, watchdog-ON, so
ABEND overstates hangs — see the methodology correction) and should not be read
as final totals.

| status (first ~63, partial) | count |
|--------|------:|
| OK | 45 |
| FAIL | 11 |
| ABEND (slow-or-hang, watchdog-ON) | 6 |
| EMPTY | 1 |

A clean full-suite tally requires a **watchdog-OFF** run on an uncontended
machine (the concurrent `CratonVM-clients` session held the main `.exe` + CPU
throughout this session). The actionable output of this run was the **bug
families B-A … B-K**, not a headline pass/fail count.

---

# Bugs found (CratonVM-specific; grouped by family)

## B-A — Mockito/ByteBuddy mock generation HANGS (watchdog abort) — High
Classes hang past the 120 s watchdog inside ByteBuddy bytecode generation:
- `ClientUtilsTest` — `Mockito.mockStatic` → `InlineBytecodeGenerator.transform`
  → `triggerRetransformation` → `MethodDescription.hashCode` (static mock /
  class **redefinition** path). 507 threads at abort.
- `NetworkClientTest` — `Mockito.mock` → `SubclassBytecodeGenerator.mockClass`
  → `MethodGraph$Compiler$Default…asGraph` → `MethodGraph…Key.hashCode`
  (subclass mock / **MethodGraph compile** path). 548 threads at abort.

Both wedge in ByteBuddy graph/redefinition work. The 500+ "threads dumped" hints
at runaway thread creation or the watchdog enumerating a huge thread set. This is
the dominant blocker for the Mockito-heavy clients tests (cf. bug-09, bug-24).
**Likely also the cause of most remaining ABENDs/TIMEOUTs in Mockito-using
classes.**

## B-B — generated `*RequestData` accessor returns null → NPE in `buildRequest` — High
`AdminApiHandler.buildRequest(...)` builds a request whose generated data accessor
is null on CratonVM:
- `AdminFetchMetricsManagerTest` — `Cannot invoke tags on null` (×2)
- `AllBrokersStrategyTest` — `Cannot invoke brokers on null`
- `ListOffsetsHandlerTest` — `Cannot invoke timeoutMs on null` (×2)
- `ListTransactionsHandlerTest` — `Cannot invoke filteredProducerIds / filteredProducerIdPattern on null` (×2)

Pattern: a schema-generated message/builder object (`*RequestData`) is null where
HotSpot has a populated instance — a defect in the generated-message builder path
(same family as the bug-11/bug-12 generated-enum/message NPEs).

## B-C — path-traversal guard false-positive blocks relative config path — ✅ FIXED
`ConsumerConfigTest.testValidateConfigPropertiesFile` failed with
`AssertionFailedError: ... Path traversal detected:
C:\craton\CratonVM\apps\kafka\..\config\consumer.properties`. CratonVM's file-I/O
security guard rejected a legitimate `..`-containing relative path that HotSpot
opens fine — over-strict canonicalization. CratonVM-specific (HotSpot has no such
guard message).

**FIX (worktree `fix/kafka-clients-suite-bugs`, commit `6f990e8f`):**
`native-io/src/lib.rs::validate_path` had an always-on guard rejecting **any**
`..` *component*. But an *interior* `..` that merely cancels a preceding name
(`apps/kafka/../config/x` → `apps/config/x`) never escapes its anchor and is
opened fine by the JDK — so a faithful general-purpose JVM must accept it. New
helper `has_escaping_parent_segment` rejects only **net-escaping** `..` (a leading
`..` that climbs above a relative path's start; absolute paths clamp at root and
remain contained by the canonicalize-and-contain check when CWD confinement is
on). The pinned security tests still pass (`../escapes`, `../../etc/passwd` still
rejected); added regression tests for the interior-`..` accept and the
net-escape reject cases. **Verified end-to-end:** `ConsumerConfigTest`
20/21 → **21/21 OK** on a `dev + B-C` binary, run with `user.dir = …/clients`
(as Gradle/HotSpot do) so `../config/consumer.properties` resolves to the real
file at `apps/kafka/config/consumer.properties`.

> Note: the `run.sh`/`recheck.sh` harness launches the VM with `user.dir`
> = `apps/kafka`, so the test's `${user.dir}/../config/consumer.properties`
> would resolve to the non-existent `apps/config/...` even on HotSpot; the test
> only loads the file when `user.dir` is the `clients` module dir. The B-C
> *guard* defect is independent of that and is fixed.

## B-D — `Map.Entry.setValue()` lost write-through → stale session PartitionData — ✅ FIXED
`FetchSessionHandlerTest` (6 fails) — fetch-session diffing produces wrong
`PartitionData`:
- topicId replaced with the wrong Uuid (`y5hflvtjS2…` vs `_cIasUJb…`) or zeroed
  (`AAAAAAAAAAAAAAAAAAAAAA` = all-zero Uuid),
- `logStartOffset` carried wrong (120 vs 110).

**ROOT CAUSE (not bug-18/topicId, not ordering):** CratonVM's
`map.entrySet().iterator().next().setValue(v)` updated only the detached entry
snapshot and **never wrote through to the backing map** — broken for HashMap,
LinkedHashMap, *and* TreeMap (a general defect, B-D is one victim).
`FetchSessionHandler.Builder.build()`'s incremental diff calls
`entry.setValue(nextData)` on the session `LinkedHashMap` to record an
**altered** partition; the lost write-through left the session cache holding the
stale `PartitionData` (`logStartOffset 110` not `120`), and the next build then
re-sent the "unchanged" partition (and mis-handled the topicId-replace diff).

**FIX (worktree `fix/bug-d-entry-setvalue`, commit `54211831`,
`native-collections/src/lib.rs`):** entrySet entries now carry a 3rd field = the
source map; `native_entry_set_value` writes through via `native_map_put`.
Detached entries (`firstEntry`/`lastEntry` snapshots, unmodifiable views,
standalone `SimpleEntry`) stay 2-field → fail-safe slot-2 read returns null → no
write-through (matches JDK immutable-entry semantics). Applied to all six
entrySet creation/iteration paths (generic, LinkedHashMap, TreeMap, CHM, and the
two view-resync paths).

**Verified == HotSpot** via standalone probes (`apps/kafka/ksuite/`):
- `EntrySetValueProbe` — `setValue` writes through for HashMap/LinkedHashMap/
  TreeMap (`get(b)=99`, `get(c)=77`).
- `FetchSessionIncrProbe` — incremental session: `sessionFoo1.logStartOffset`
  now `120` (was `110`); topicId-replace build sends **only** `foo-0` (was also
  wrongly re-sending the stale `foo-1`).

> The full `FetchSessionHandlerTest` can't confirm in-harness: it flaky-hangs /
> crashes (rc=124/rc=1) under the separate **B-J GC-mirror-zeroing** instability,
> independent of B-D. The probe is the deterministic proof.

## B-E — HashSet iteration order ≠ HotSpot (capacity) — ✅ FIXED; 2 residual non-ordering fails split out
Originally filed as "pure ordering". Split after investigation:

**ORDERING ROOT CAUSE — ✅ FIXED (worktree `fix/bug-e-map-capacity`, commit
`46105481`, `native-collections/src/lib.rs`):** `native_map_init_capacity`
rounded `ceil(c*4/3)` up to a power of two (a "hold N mappings without resizing"
optimisation), so `new HashSet<>()` (backing passes cap 16) and
`new HashMap<>(16)` allocated **32** buckets where HotSpot allocates **16**
(`tableSizeFor(16)=16`; HotSpot resizes on the 13th insert). The 2× capacity
shifts every key's bucket index → **HashSet iteration order diverged from
HotSpot** (HashMap was already correct via the no-arg path). Fix = match the JDK
`tableSizeFor`. **Verified == HotSpot** via `OrderProbe`/`OrderProbe3`
(`apps/kafka/ksuite/`): HashSet string-key order now matches HotSpot and
`HashMap.keySet` across toString/toArray/iterator/stream/ArrayList-copy.
- `ListConsumerGroupOffsetsHandlerTest` — **OK 14/14**.

**RESIDUAL — NOT ordering (the capacity fix corrected the order but these still
fail on a different root cause; need separate work):**
- `AdminApiDriverTest` (2) — `testCoalescedStaticAndDynamicFulfillment`
  expected `[bar]` got `[bar, foo]` (HotSpot order now, but an **extra key
  `foo`** survives → a **coalescing/dedup** bug, likely a `Map`-keyed-by-`Set`
  grouping, cf. [bug-17](bug-17-assignor-assignment-mismatch.md)); `testKeyLookupRetry`
  same coalescing surplus.
- `DescribeConsumerGroupsHandlerTest` (1) — `ConsumerGroupDescription.equals`
  returns **false for two value-equal objects whose toStrings are byte-identical**
  (verified by diff) → a **`Collection.equals`** divergence on the `members`
  field, not order.

## B-F — assorted single correctness failures (to triage)
- `MetadataTest.testConcurrentUpdateAndFetchForSnapshotAndCluster` — expected
  true was false (concurrency).
- `PartitionLeaderStrategyIntegrationTest.testCachingOverlappingRequests` —
  expected false was true (request caching/coalescing).
- ~~`NodeApiVersionsTest` ABEND~~ → **reclassified**: not a hang. Under watchdog-off
  it is `FAIL` 11/13 with 2 NPEs `Cannot invoke apiKey on null` → folds into **B-B**
  (generated `ApiVersion`/`ApiMessageType` returns null).

---

# Findings beyond B-A..B-F (owned here)

## B-G — JUnit reflective discovery is pathologically slow → false hang — High (throughput)
`KafkaAdminClientTest`, `KafkaConsumerTest`, `RangeAssignorTest` first-pass `ABEND`
with the main thread in JUnit **discovery** (`MethodSelectorResolver.resolve` →
`ReflectionUtils.isMethodPresent`; `NamespacedHierarchicalStore.getOrComputeIfAbsent`
→ `CompositeKey.hashCode`). Not a deadlock — CratonVM's reflective discovery on
large test classes exceeds 120 s. This is the bug-23 throughput gap; with the
watchdog off these should complete (status pending v2). Biggest single lever for
suite wall-clock.

## B-H — consumer background threads are never reclaimed across a JVM — High (resource)
Batched consumer classes accumulate **1000+ live threads** in one JVM
(`KafkaShareConsumerTest` 1160, `KafkaShareConsumerMetricsTest` 1221), leaf frames
in `NetworkClientDelegate.addAll` / `UnsentRequest.setEnqueueTimeMs`. The counts
are cumulative across classes in the shared JVM → a consumer's background network
threads survive after its tests end (not joined/closed). Even per-class this is a
thread-lifecycle leak worth confirming with a fresh-JVM watchdog-off run; in batch
it snowballs into CPU/heap pressure that drags later classes (amplifying B-G).

> NOTE: FAIL classes not yet cross-checked against HotSpot individually; the guard
> message (B-C), null generated accessors (B-B), Uuid (B-D, =bug-18) and ordering
> (B-E, =bug-17) are CratonVM-specific or match known families. v2 (watchdog-off)
> per-class numbers + a HotSpot diff will be appended on completion.
