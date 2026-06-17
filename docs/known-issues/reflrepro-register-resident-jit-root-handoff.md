# Handoff — ReflRepro GC corruption = register-resident missed JIT root (OPEN)

**Status: OPEN.** JIT-on-only heap corruption under GC stress. A live
reflection-result object is reclaimed by the non-moving young sweep because its
only reference, at sweep time, is invisible to the conservative root scan — it
sits in a JIT **register** (and/or on the native stack *above* the per-JIT-entry
scan band). The slot is reused as a bare `java/lang/Object` → crash
(`implausible object size` sweep abort, rc=132/139) or a silent wrong result
(`MISMATCH: java.lang.Object@…`).

This is the long-standing residual of
[`jit-junit-discovery-reflection-corruption.md`](jit-junit-discovery-reflection-corruption.md)
(bug-06; the reflection mirror-array *pinning* part is fixed). It is the same
class as the precise-JIT-stack-maps work — see
[`fork6-fjp-multithread-jit-root-reclamation.md`](fork6-fjp-multithread-jit-root-reclamation.md).
Detailed writeup: `docs/wildfly-suite-bugs/bug-06b-jit-scan-cache-unsound.md`.

## Reproduce

```
# wildfly-suite/repro/ReflRepro.java (committed). JDK 25 boot.
CRATONVM_DBG_GC_STRESS=65536 target/release/cratonvm.exe \
  --java-home "C:\Program Files\Eclipse Adoptium\jdk-25.0.2.10-hotspot" \
  -cp wildfly-suite/repro  ReflRepro 8000        # rc=132/139, deterministic
```

`ReflRepro.scan` is JIT-compiled; it iterates `Class.getDeclaredFields()` /
`getDeclaredMethods()` (allocating natives, dispatched from a JIT frame) and
builds strings with `StringBuilder`. `GC_STRESS=65536` forces a young GC every
64 KB so the corruption is deterministic. Clean with `CRATONVM_DISABLE_JIT=1`.

## Decisive evidence (run on current dev, df304353+)

| Toggle | Result | Reading |
|---|---|---|
| default | crash (rc=132/139) | the bug |
| `CRATONVM_DISABLE_JIT=1` | clean | JIT-specific |
| `CRATONVM_NO_JIT_SCAN_CACHE=1` (cache off) | **crash** | NOT the JIT-scan cache |
| `CRATONVM_JIT_SCAN_CACHE=1` (cache on) | crash | NOT the cache |
| `CRATONVM_DBG_FULLSTACK_SCAN=1` (scan whole native stack as roots) | **no crash, but `bad=1`** (`java.lang.Object` mismatch persists ~1/8000) | root is mostly *above the per-entry band*, occasionally *not on the stack at all* |
| `CRATONVM_DBG_FORCE_MOVING=1` (moving collector) | no crash | the non-moving sweep is the manifestation |
| `CRATONVM_SHADOW_STACK=1` (publish register oops) | no crash (slow) | the missed root is a register oop |
| `CRATONVM_DBG_SWEEP_EDGES=1` | `root=0 young-survivor=0 old-gen=0` every sweep | reclaimed node has NO heap/root/card edge ⇒ register/native-stack root |

The `FULLSTACK_SCAN` result is the key: scanning the **whole** native stack
removes the crash but still leaves `bad=1`. So:
1. Usually the root has spilled to the stack but lives **above the JIT entry's
   `[scanner_sp, entry_sp]` band** (in the interpreter/native/Rust frame that
   invoked the JIT) — the per-entry conservative scan stops at `entry_sp` and
   misses it; a whole-stack scan finds it.
2. Occasionally the root is **truly register-resident** (`rax` holding a native
   call's return value before it is spilled) — no stack scan can see it; this is
   the residual `bad=1`.

## Root cause

`getDeclaredFields()` / `getDeclaredMethods()` are dispatched **from a JIT
frame**; their object result returns in a register. The non-moving young sweep
is forced whenever a JIT frame is live (`gc_quiescence`), because conservatively
discovered roots cannot be relocated. That sweep marks from the conservative
root set, which scans only the **stack** (and only the per-JIT-entry band). A
register-resident oop — or one on the stack above `entry_sp` — is therefore not a
root, gets reclaimed, and its slot is reused (→ bare `java/lang/Object`).

## What does NOT fix it (verified dead ends — don't repeat)

- **Disabling the JIT-scan cache.** A prior pass mis-attributed the crash to the
  cache being unsound and shipped `jit_scan_cache_enabled()` default-off. On the
  base it was developed against (`0e3f0398`) that *appeared* deterministic, but
  it was only a **GC-timing perturbation**: on current dev the crash is identical
  cache-on and cache-off. **Reverted** in `b41c0484`. (Kept the genuine, separate
  `collection_count` cache key — stops the cache republishing a freed address
  across a GC.) **Lesson: verify a timing-sensitive GC fix on the actual target
  branch HEAD, not an old worktree base; a "deterministic" env toggle can be a
  timing mask.**
- **A whole-native-stack conservative scan** (`scan_full_native_stack` added to
  `collect_roots`/`safepoint_check`, or `DBG_FULLSTACK_SCAN`). Removes the crash
  but not the corruption (`bad=1`) — the residual root is in a register.
  Subtlety found while testing: a full-stack scan in `collect_roots` *alone* did
  NOT remove the crash; only the full-stack scan inside `scan_active_jit_frames`
  (which also runs on the `update_root_snapshot` per-native-call path) did. So
  the above-`entry_sp` root is captured at native-return time and persisted in
  the published snapshot, not re-found at GC time — worth understanding before
  picking an insertion point.

## The real fix (required)

The root set the non-moving sweep marks from must include **register-resident
oops** at the safepoint. Options, in rough order of cleanliness:

1. **Complete the shadow stack** (`gc/src/shadow_stack.rs`,
   `jit/src/x64.rs::shadow_stack_maps_enabled` and the push/reload codegen). It
   already pushes "every live oop (operand-stack entries AND oop locals)" before
   GC-capable calls — exactly what's needed — but is gated off and **globally
   incomplete**: per the memory it currently routes to the *moving* collector
   (which under-counts bintrees, 68199090) and "its push/reload codegen is
   incompatible with the non-moving sweep." Making the shadow push/reload work
   *with* the non-moving sweep (pin shadow oops instead of relocate — see the
   `CRATONVM_SHADOW_PIN` experiment) and enabling it by default is the principled
   fix. See `precise-jit-stack-maps-fork6-findings.md`.
2. **Precise oop maps** for JIT frames (Stage B/C, deferred) — describe exactly
   which slots/registers hold oops at each safepoint.
3. **Narrow interim:** have JIT codegen spill the live-oop set (at minimum every
   invoke/native-call object return value) to a stack spill slot the conservative
   scan covers, before the *next* GC-capable call. Candidate gap to investigate:
   the **MIC/PIC direct-call fast path** (`call r11` in `jit/src/x64.rs`, the
   inline-cache hit that bypasses `jit_invoke_*_mic`) — confirm whether it spills
   the caller's live oops before the call the way the helper path does. If it
   doesn't, that is a plausible localized source of the register-resident window.

Whichever path: the verification bar is **`bad=0`** on `ReflRepro 8000` under
`CRATONVM_DBG_GC_STRESS=65536` (not just "no crash"), plus no regression on
bintrees (`bench/BenchSuite bintrees18`) and the WildFly/Spring suites.

## State on dev

- `b41c0484 fix(gc): revert misdiagnosed JIT-scan-cache default-off; correct
  ReflRepro residual diagnosis` — cache re-enabled, `collection_count` keying
  kept, `bug-06b` doc corrected. No code change attempts the real fix.

## Diagnostic env reference

`CRATONVM_DBG_GC_STRESS=<bytes>` (force young GC), `CRATONVM_DISABLE_JIT`,
`CRATONVM_JIT_BISECT_ONLY=<class-prefix>` / `CRATONVM_JIT_BISECT_SKIP=<Class.method>`
(narrow which methods JIT), `CRATONVM_DBG_SWEEP_EDGES` (classify the reclaimed
node's inbound edge), `CRATONVM_DBG_SWEEP_ZERO` (record swept objects' original
class), `CRATONVM_DBG_FULLSTACK_SCAN`, `CRATONVM_DBG_FORCE_MOVING`,
`CRATONVM_SHADOW_STACK` (+ `_NOPUSH`/`_NORELOAD`/`_PIN` bisect toggles),
`CRATONVM_DBG_CORRUPT_FRAMES` (mutator Java stack at first sweep corruption).
