# common-w28a proposal: shard the lock-key registry mutex by identity hash

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 20
> of 54).** Not built (`native-builtins/src/lib.rs::lock_key_registry` is one
> mutex). Absorbed `common-w18e` earlier (retired). **Gate:** a 32-thread
> loopback byte-read bench showing the registry mutex in a contention profile
> before and not after; the lock-discipline ratchet updated. **Size:** S-M.

- **Status:** PROPOSAL (not a defect). Filed 2026-09-26, gc-common round
  2026-09-23, wave 28, lane A28.
- **Owner:** `native-builtins/src/lib.rs` (`lock_key_registry`,
  `lock_key_for`, `existing_weak_lock_key`, the three sweeps).

## Why now

Waves 23-28 moved every identity-hash-keyed side table onto weak lock keys
(`gc_stable_weak_lock_key` / `existing_weak_lock_key`). Each lookup takes ONE
process-wide `parking_lot::Mutex` (`lock_key_registry`). Since w28-a that
includes per-call paths of the socket surface in
`native-builtins/src/net_phase_e.rs`:

- `Socket$SocketInputStream.read` / `SocketOutputStream.write` (every call,
  `stream_owner_get` -> `existing_native_obj_key`), including `read()I`, which
  byte-at-a-time readers call once per byte;
- `sock_stream_id_for_upcall` (the per-call fallback under
  `SSLSocketInputStream.read()I`);
- every `Socket` accessor (`sock_peek` / `sock_get`).

A server with many connection threads now serialises every socket read on the
registry mutex as well as on `sock_side_table`'s (which it already took).
Lesson (jj) of the round names the trade-off; this page is the structural fix.

## Proposal

The registry is already bucketed by identity hash (`buckets:
FxHashMap<u32, Vec<LockKeyEntry>>`), and every lookup names exactly one hash.
Split it into N shards (`[Mutex<Shard>; 64]`, shard = `hash % 64`), each with
its own `buckets`. `lock_key_for` / `existing_weak_lock_key` /
`lock_key_owner_vm` / `release_lock_key` touch one shard. The generation
counter becomes an `AtomicU32` (the "skip a key still in use" loop then runs
under the shard's lock, which holds every slot of that hash; the tombstones
move into the shard too, keyed by the packed key whose high half is the hash).
The sweeps and `forget_vm_lock_keys` walk the shards one at a time, which they
can already do: they never need a consistent view across hashes.

## What it would take / retire

A bench (e.g. `tools/probes/MtChurnProbe` style: 32 threads each reading a
loopback socket byte-at-a-time) showing the registry mutex in a contention
profile before and not after; the lock-discipline ratchet updated for the
shard array (64 locks, one declaration).
