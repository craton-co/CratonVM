# Bug C — `WeakHashMap.values()/keySet()/entrySet().stream()` JIT hang ✅ FIXED

| | |
|---|---|
| **Kind** | Hang / infinite loop |
| **CratonVM** | was TIMEOUT · **HotSpot** OK |
| **Status** | ✅ FIXED on dev — `f328fa9a` (and the existing `1cd0ab26`/`7ddda253` skip-list lineage) |

## Root cause

`WeakHashMap.values().stream()` (any terminal) infinite-loops under JIT but completes with
`--nojit`. WeakHashMap is the only `Map` whose views are **not** natively snapshot-shadowed
(HashMap/TreeMap/LHM/CHM return synthetic snapshots), so its stream runs the real-JDK
`WeakHashMap$ValueSpliterator`. The JIT miscompiles `ValueSpliterator.tryAdvance`: its loop
advances purely via the instance fields `index` (`current = tab[index++]`) and `current`, and
the JIT'd body never persists those field writes → `index` stuck at 0 → spins forever
(re-calling `getFence()→size()→expungeStaleEntries()→ReferenceQueue.poll()` each iteration,
per `--stack-dump-on-timeout`). Same field-write miscompile family as the banned
`HashMap$HashIterator` methods. Pinpointed by `CRATONVM_JIT_BISECT_SKIP` keep-only bisection.

## Fix

Skip-list `WeakHashMap$ValueSpliterator/KeySpliterator/EntrySpliterator.tryAdvance`
(+ `forEachRemaining`) in `vm/src/jit/skip_list.rs::is_known_miscompile`. Validated:
`WeakHashMap.values()/keySet()/entrySet().stream().count() == 5` with JIT on. Underlying JIT
instance-field-write codegen defect remains for a general fix (shared with HashIterator).
