# Bug C — `WeakHashMap.values().stream()` infinite-loops (log4j2 init hang) ⏱️ HANG cluster

| | |
|---|---|
| **Severity** | High (root cause of the largest hang cluster in the suite) |
| **Kind** | Hang / infinite loop (TIMEOUT @ 600s) |
| **Surfaced by** | The whole `org.apache.kafka.clients.consumer.internals.*` TIMEOUT cluster (13+ classes), starting with the trivial `ConsumerRecordsTest` |
| **CratonVM** | TIMEOUT · **HotSpot** OK |
| **Status** | ✅ **HANG FIXED** (2026-06-18, dev `1cd0ab26`) via targeted JIT ban — verified. Underlying `dup_x1` codegen defect remains OPEN (general fix). |
| **Recommendation** | Done (workaround shipped). Follow-up: fix the `dup_x1` field-post-increment codegen so the ban can be lifted. |

## ✅ Resolution (2026-06-18) — it was a JIT miscompile, not a stream/native-masking bug

The original "fix direction" below (native-mask `values()` like the other maps, or
fix the stream/Sink engine) was **wrong**. Reproduced and bisected on a built VM:

- **JIT-specific:** `--nojit` passes; JIT-on hangs. Not GC-triggered (`-Xmx4g` still
  hangs) and not invocation-tier-up (`CRATONVM_JIT_THRESHOLD=1000000` still hangs).
- **The watchdog shows the main thread stuck in *native Rust* code** with the dispatch
  ring on `ReferenceQueue.poll`/`WeakReference.<init>` — a red herring; the real spin
  is the JIT'd traversal loop.
- **Bisected to one method** with `CRATONVM_JIT_BISECT_ONLY=java/util/WeakHashMap` then
  `CRATONVM_JIT_BISECT_SKIP=...$ValueSpliterator.tryAdvance` → skipping *only* that
  method makes the repro return (`getFence`/`size`/`expungeStaleEntries` skips do not).
- **Root cause:** JIT miscompiles `ValueSpliterator.tryAdvance`'s `current = tab[index++]`
  field-post-increment. Bytecode 60-77 is `aload_0; aload tab; aload_0; dup; getfield
  index; dup_x1; iconst_1; iadd; putfield index; aaload; putfield current`. The
  `putfield index` (the `++` store) is effectively dropped under JIT, so the inner
  `while (index < hi || current != null)` loop never advances `index` past a null table
  slot and spins forever. It's a **`dup_x1` field-post-increment codegen defect**, the
  same class as the dup_x family in memory.

**Workaround shipped (`1cd0ab26`):** extend the existing Tomcat-Bug-B WeakHashMap
*iterator* ban (`skip_list.rs::is_known_miscompile`) to the *spliterator/stream*
siblings — `Value/Key/EntrySpliterator` × `tryAdvance`/`forEachRemaining` (all share the
identical `tab[index++]` loop). **Verified:** `WeakHashMap.values()/keySet()/entrySet()
.stream()` × `sum/count/forEach/collect` all match HotSpot, JIT-on, `rc=0`.

**Still OPEN (follow-up) — refined 2026-06-18.** A direct attempt to fix the codegen
established that this is **NOT a standalone `dup_x1` lowering bug**. Three increasingly
faithful standalone reproducers of the `this.cur = localTab[this.idx++]` shape — (1) `int[]`,
(2) `Object[]` + GC write-barrier + two-condition `while (idx<hi || cur!=null)` loop, (3) the
full external-driver pattern (linked `Entry`, null table slot, `fence`, one-emit-per-call
driven by an external `while(tryStep())` loop) — **all JIT-compile correctly** and match
HotSpot. So the `dup_x1`/`tab[index++]` sequence is lowered correctly in isolation; the defect
only manifests inside the real `tryAdvance` compilation.

That places it in the **context-sensitive register-allocation clobber family** the skip_list
already manages with ~30 sibling bans (W2-CHM / RBC.1 / SPB.1-3 / EXEC.1 / NETTY.1 / Tomcat
Bug B/D), whose comments pin the shared cause to *"the regalloc clobber in
`patch_self_calls`/`emit_invoke_virtual` leaves a stale pointer in a callee-saved register"* —
here, one of `tryAdvance`'s JIT'd calls (`getFence`/`Consumer.accept`) clobbers a callee-saved
register holding a live value (`index`/`this`/`tab`) across the call, so the loop never
advances. Skipping `getFence`/`size`/`expungeStaleEntries` does **not** fix it (the clobber is
inside `tryAdvance`'s own body), only skipping `tryAdvance` does. Disassembly captured
(`CRATONVM_JIT_ALLOW_PACKAGES=java/util/ CRATONVM_DBG_JIT_DISASM=1`, `len=2570`).

**Why no codegen fix was shipped:** without a standalone reproducer a codegen change cannot be
verified (only hang/no-hang against the real method) and cannot get a regression test, and a
speculative regalloc change risks miscompiling the VM's hot paths. The correct home for the
general fix is the **precise-JIT-maps / regalloc project** (Stage B/C, deferred), which is also
what closes the whole sibling-ban family. Until then the targeted ban is the right disposition;
once the regalloc clobber is fixed, lift the six entries and re-run the repro to confirm.

## Symptom

A large set of `consumer.internals` classes (and `ConsumerRecordsTest`, which is a trivial
data-class test with no Mockito) TIMEOUT at 600 s under CratonVM while HotSpot passes them
in seconds. No output, no exception — a silent hang.

## Root cause

A `--stack-dump-on-timeout` capture of `ConsumerRecordsTest` showed the hang is in **log4j2
initialization** — `AbstractConfiguration.start()` / `hasAsyncLoggers()` /
`InternalLoggerRegistry.expungeStaleEntries()` — with the tail of the dispatch trace
spinning in:

```
WeakHashMap$ValueSpliterator.tryAdvance / WeakHashMap$WeakHashMapSpliterator.getFence / WeakHashMap.size
```

log4j2's `InternalLoggerRegistry` keeps its loggers in a `WeakHashMap` and streams it during
`LoggerContext`/`Configuration` startup. The first test to force full log4j2-core
initialization (e.g. via `LogCaptureAppender`, used by `ConsumerRecordsTest`) hangs there,
and so does every `consumer.internals` test that triggers the same init.

## Minimal repro (3 lines, no Kafka, no log4j)

```java
WeakHashMap<String,String> m = new WeakHashMap<>();
for (int i = 0; i < 5; i++) m.put("k"+i, "v"+i);
m.values().stream().count();   // HotSpot: 5    CratonVM: hangs forever
```

`ksuite/repro/WeakStream.java`, `WS_count/forEach/collect.java`, `WeakSpl.java`, `MapStreams.java`.

## Scoping (what works vs hangs)

| Operation on a 5-entry WeakHashMap | CratonVM |
|---|---|
| `size()`, `values().size()` | ✅ 5 |
| `values().iterator()` loop / `entrySet()` for-each | ✅ 5 |
| `values().spliterator().tryAdvance(...)` (manual loop) | ✅ 5 |
| `values().spliterator().forEachRemaining(...)` | ✅ 5 |
| `values().spliterator().trySplit()` / `characteristics()` | ✅ matches HotSpot |
| **`values().stream().count()` / `.forEach()` / `.collect()`** | ❌ **hang** |
| `HashMap` / `TreeMap` / `LinkedHashMap` / `IdentityHashMap` `values().stream()` | ✅ 5 |

So it is **WeakHashMap-specific** and only via the **`stream()` pipeline**. Important: the
raw spliterator primitives all work **in isolation** — verified clean (no load):

- `tryAdvance` (manual `while`), `forEachRemaining`, `trySplit` (terminates after 4, returns
  null), `estimateSize`/`characteristics` — all correct.
- `WeakHashMap.size()` in a loop and after `System.gc()`, and `spliterator().estimateSize()`
  (which drives `getFence()→size()→expungeStaleEntries()→ReferenceQueue.poll()`) — all
  return 5 and terminate.

The hang appears **only when CratonVM's stream/Sink machinery drives the
`WeakHashMap.ValueSpliterator`**. A clean `--stack-dump-on-timeout` trace shows the loop
`ValueSpliterator.tryAdvance → getFence → WeakHashMap.size → expungeStaleEntries →
ReferenceQueue.poll`, repeating and never exhausting — even though those exact calls
terminate when invoked directly. `WeakHashMap` is the only `Map` whose `values()`/`keySet()`/
`entrySet()` are **not** natively shadowed (HashMap/TreeMap/LHM/CHM return synthetic snapshot
collections, whose streams work), which is exactly why it is the only one exposing this
stream-driving defect.

Note (corrected): earlier guesses that this was a `trySplit`/`getFence`-field-persistence or
assignment-expression bug were **refuted** under clean (no-load) re-testing — those repros
pass; only the stream-driven traversal fails. The defect is in the stream→spliterator
driving interaction, not the spliterator or any language primitive.

### Earlier diagnostic caveat
Some intermediate runs showed a fast ~1–2 GB `raw_vec` OOM rather than a pure hang; others
showed false timeouts. The false timeouts were **load contention** (the HS baseline suite was
running concurrently, making cratonvm cold-start take >8 s, so short-timeout probes reported
spurious hangs). Confirmed-clean result: `values().stream()` does not terminate.

## Impact

This single defect accounts for the dominant `consumer.internals` TIMEOUT cluster (each
costing the full 600 s). Fixing it should convert many of those TIMEOUTs to real
pass/fail results and dramatically cut suite wall-time.

## Suggested fix direction

Two options:

1. **Mask like the other maps (pragmatic, matches existing architecture).** Register native
   `values()`/`keySet()`/`entrySet()` for `java/util/WeakHashMap` returning synthetic
   snapshot collections (as HashMap/TreeMap/LHM/CHM already do), so their `stream()` runs the
   already-working `ArrayList`/`HashSet` spliterator path instead of the real
   `WeakHashMap.ValueSpliterator`. Caveat: `WeakHashMap.Entry` extends `WeakReference` (key is
   the referent via `get()`, not a plain field), so the collector must read that layout
   correctly (or collect via the map's own working iterator), unlike the HashMap `Node`
   layout `map_collect_values` currently assumes.

2. **Fix the real defect (deeper).** Make CratonVM's stream/Sink driving terminate over the
   real `WeakHashMap.ValueSpliterator` — the primitives work standalone, so the bug is in how
   the `StreamSupport`/`AbstractPipeline.copyInto` loop invokes `forEachRemaining`/`tryAdvance`
   on this spliterator. Needs deeper stream-engine instrumentation.

This is **not** a small localized fix like Bug A; recommend either approach with care + a
targeted regression test (`WeakHashMap.values().stream().count()`).
