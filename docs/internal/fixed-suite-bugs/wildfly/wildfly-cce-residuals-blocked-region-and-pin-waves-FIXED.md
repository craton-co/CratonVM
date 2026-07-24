# FIXED: WildFly boot CCE long tail — blocked-region census leak, fixup-chain stranding, stream/collections stale-at-store waves, and the pin-stack truncate-misuse class

**Status: FIXED 2026-07-17** (worktree `/data/data/wt-cceres3-20260717`, branch
`fix/wildfly-cce0079-residuals3-20260717`, forked from `origin/dev @ 56728b1a`;
recovered from the destroyed `wt-cceres2` worktree after a shared-host sweep
deleted the prior clone mid-session).

Closes the remaining OPEN residuals of
`docs/known-issues/wildfly-standalone-boot-attributeaccess-cce-register-invisible-root.md`
(the domain no-JIT long tail: stale-String `aastore` in
`SubsystemResourceDescriptionResolver.<init>`, the `cid=0`-receiver CCE in
`AddStepHandler.recordCapabilitiesAndRequirements`, the unidentified
`class_id_of` canary capture) and the JIT SIGSEGV bucket previously parked
with the SB-CRASH-04/precise-maps roadmap.

## The five defect classes (each captured live)

### 1. `refresh_root_snapshot` raised `in_blocked_region` on a running thread (census leak)

Called per-iteration by the native-collections stream drain loops, it
delegated to the raise=true `deposit_root_snapshot()`. The flag went up with
no consuming exit; the identity census then EXCLUDED the still-running thread
from every subsequent STW pause; moving collections completed under its feet
while `gc_block_state.fixup` accumulated unconsumed. Captured live: an EQE
worker deposited raise=true four times with `fixup_pending=44`, then ran from
43 frames down to 1 on stale refs — poisoning every object graph it touched
(the `OperationContext`/registration islands behind the
`AbstractAddStepHandler` / `ParallelBootTask` / `ServerService.boot` /
`DomainModelControllerService.boot` captures). Fix: delegate to
`deposit_root_snapshot_no_flag()`. Hardening: safepoint-publish self-heal
(applies leaked pending fixups + clears a stuck flag on a running thread) and
poison-tolerant waits in `net_phase_e`'s response collector.

### 2. Blocked-window fixup chain could strand frame slots forever

`fold_pointer_map_into_blocked` chains `frame-held address → current address`
keyed by each object's FIRST-move address, seeded only from the FILTERED root
snapshot; any missed seed stranded the slot permanently (~40 EQE workers with
stranded `ThreadBody.run` locals, ~10k verifier hits per boot; the domain
Controller Boot Thread's own locals). Fix: exact per-slot tracking
(`GcBlockState::slot_origins`) — deposit records every Object frame slot,
folds advance each entry per collection, the wake write-back stores it into
the exact slot. Validated: `writeback healed` fires 10–279×/boot on slots the
chain missed; stranded-slot verifiers dropped from ~10k/boot to 0.

### 3. Stream + collections natives stored refs captured across GC-capable calls

`native_stream_concat` (store-canary capture with full symbols) drained
stream A, then drained stream B (arbitrary Java), then stored A+B unpinned.
Audit-driven waves fixed ~75 sites total with the pin/re-read/unpin idiom:
all Collectors tag arms, min/max/reduce, matchers, eager
filter/map/flatMap/peek, collect_3arg + primitive collects,
`make_int/long/double_stream` + `make_primitive_iterator`, mapToObj/boxed,
primitive lambda loops, limit/skip/sorted/distinct stale-`this`,
close-handler runner, `Collections.addAll/nCopies/reverse/max/min`,
`Arrays.copyOf/toString`, the from-collection/from-map constructors
(HashSet/HashMap/LinkedHashMap/LinkedList/ArrayList), `putAll`s, the
entrySet/keySet/values view materializers, `make_map_of` internals,
`native_em_init` (EnumMap), `native_ts_to_array`, and jboss-msc
`native_service_builder_install`'s `sn_for_mirror` store.

### 4. Pin-stack truncate-misuse (`unpin_native_roots` is a TRUNCATE)

Three live-captured instances of the same misuse, found via the new
PIN-DANGLING canary (`read_native_pin` with a handle past the stack fires a
named backtrace instead of silently returning the raw, possibly-stale
fallback):
- `stream_apply_chain_full` pinned emitted elements from INSIDE the
  `stream_pull` callback — above the pull machinery's own per-element bases,
  so its legitimate unpin-to-base wiped them. Fixed by rooting accumulated
  elements in the JNI global-root table (stack-independent).
- `resync_view_set`'s entrySet rebuild pinned all (key,value) pairs up front
  and then "unpinned" iteration 0's pins at the end of iteration 0 —
  truncating iterations 1..N's handles. Fixed: single truncate at fn end.
- `chm_extra_entries` (Properties) unpinned its iterator pin — below every
  accumulated entry pin — before the read-back. Fixed: single truncate after
  the read-back. (Its stale pairs flowed into the static entrySet view and
  onward through `HashMap.putAll(props)`/compute — the domain
  `compute_if_present`/`wire_provides_injectors` captures.)

Rule established: the pin stack is strictly LIFO per scope. Callbacks must
not pin into a callee's scope; per-element cleanup must not unpin pins that
sit below later elements' pins; use one truncate from the scope's first pin.

### 5. The JIT SIGSEGV bucket was inline-cache ABI-flag tearing, not a GC root gap

The "SB-CRASH-04 register-invisible" SIGSEGV bucket was a JIT inline cache
publishing a new `entry_ptr` while readers could pair it with the OLD
`needs_context`/`needs_heap` ABI flag — shifting every argument by one in the
callee. Fixed by invalidating the class-id guard before retarget republish
and re-deriving the flag from the entry's own `CompiledMethod` at all three
marshalling choke points. 0 SIGSEGV across every post-fix JIT batch
(baseline ~29%).

## Diagnostics landed (default-off, `CRATONVM_DBG_BLOCKGC`-gated)

DEPOSIT/WAKE/ARRIVE/SAFEPOINT/INITIATOR-STALE frame verifiers
(`debug_forwarded_target`, ring-aware), PIN-STALE (stale at pin time),
PIN-DANGLING (handle past the pin stack), deposit-with-pending backtrace,
SAFEPOINT-HEAL/FLAG-CLEAR, native-unblock DISCARDS, writeback-healed trace.

## Verification (Azure host, shared load; marker-based counts)

| Batch (build) | Standalone | Domain (no-JIT) |
|---|---|---|
| Baseline plain (dev+ABI fix only) | 2 CCE / 12, 0 SIGSEGV | 4/4 CCE |
| + root-fix + write-back (d5) | 1 CCE / 12, 0 SIGSEGV | 4/4 CCE (stream family) |
| + stream wave (d11) plain | **0 CCE / 12, 0 SIGSEGV** | 1 CCE / 6, 2× both-servers OK |
| d11/d12 canary | 8/8 clean (d11); 0 firings /8 (d12) | resync/properties captures → fixed |
| d13 round 12 | canary 6/6 clean | properties producer captured -> fixed |
| Final (d17) round 15 | canary 6/6 clean; plain batches clean at merge time | plain 0 CCE / first 3 (1 full both-servers boot); canary-only tree-key tail remains (see known-issues/wildfly-domain-nojit-stale-ref-long-tail.md) |

Canary batches ran `CRATONVM_DBG_STALE_OBJREF=1 CYCLES=8 CRATONVM_DBG_BLOCKGC=1`;
every firing was root-caused and fixed (none left unexplained).

Unit tests (final tree): cratonvm-gc, cratonvm-native-collections (73 lib +
all integration bins), cratonvm-native-builtins --lib, cratonvm-native-io
--lib all pass; cratonvm-vm --lib 2200 pass with only the pre-existing
failures (7 `jit::skip_list` + 9 debug-only `lock_order`, both from other
sessions; `buffered_input_stream` was reconciled with 995ff48c in this
branch; `process_vm_publish_and_resolve` is a parallel-run flake that passes
in isolation).

## Related

- `docs/known-issues/wildfly-standalone-boot-attributeaccess-cce-register-invisible-root.md`
  — the parent investigation; retired to docs/internal by this branch.
- `wildfly-cce0079-young-start-set-truncation-FIXED.md`
  — the 2026-07-16 dominant-mechanism fix this work builds on.
