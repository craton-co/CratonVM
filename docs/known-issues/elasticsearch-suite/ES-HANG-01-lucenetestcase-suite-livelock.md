# ES-HANG-01 — Systemic JIT livelock: every `LuceneTestCase`/`ESTestCase` suite hangs in RandomizedRunner setup

**Status:** ✅ FIXED on `dev` by commit **`1cd0ab26`** ("fix(jit): ban WeakHashMap spliterator tryAdvance/forEachRemaining (kafka-bug-C hang)"), which landed during this session. Same root cause and same fix reached independently here (kafka-bug-C and ES-HANG-01 are the same JIT miscompile). The suite binary was built from `16b69363`, which predates `1cd0ab26` and therefore hung.
**Severity:** was CRITICAL (dominant default-config blocker — every class extending `ESTestCase`).
**Date:** 2026-06-18

## Root cause (pinned)

JIT miscompile of **`java.util.WeakHashMap$ValueSpliterator.tryAdvance`** — its table-walk loop (`current = tab[index++]`, emitted as a `dup_x1` stack dance interleaving the `index` putfield with the array load) never terminates once JIT-compiled. Lucene's `RandomizedRunner` setup iterates a `WeakHashMap` via the values spliterator, so under the JIT every `ESTestCase`/`LuceneTestCase` suite livelocks during setup. Permanent (process still hung at 650 s); `--nojit` runs fine.

## How it was pinned (independent confirmation of `1cd0ab26`)
- Bisection: `CRATONVM_JIT_BISECT_ONLY=java/util/WeakHashMap` still hangs; adding `CRATONVM_JIT_BISECT_SKIP=…ValueSpliterator.tryAdvance` runs → the defect is in that method's JIT code (not a compilation-shift artifact).
- cdb native stack of the hung thread = a CPU-bound interpreter/JIT execution loop (not a deadlock).
- `--nojit` is a complete workaround.

## Fix (on dev)

`dev` bans the WeakHashMap iterator + spliterator walk methods in the targeted `is_known_miscompile` skip-list (`vm/src/jit/skip_list.rs`) so they run in the interpreter (correct) while every other method stays JIT-eligible. Same family as the NETTY.1 `Arrays.fill` and HashMap hot-loop bans. A root-cause loop/putfield codegen fix would let these be lifted later (as NETTY.1 was).

## Verified (rebuilt binary, default JIT on)
- `LuceneOnlyTest` / `MurmurHash3Tests`: rc 124 (hang) → run.
- 12/12 previously-hanging `:server` classes → 0 hangs.
- WeakHashMap keySet/values/entrySet/stream iteration byte-identical to HotSpot (no regression).

> A parallel branch `fix/es-hang-01-weakhashmap-spliterator-jit` was created here before noticing `dev` already had `1cd0ab26`; it is redundant and was **not** merged.
