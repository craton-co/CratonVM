# Fix note — vm-threading-mediums

Agent: `vm-threading-mediums`
Branch: `dev`
Date: 2026-06-10
Source report: `docs/reviews/fable-2026-06-10/vm-threading.md` (findings B3, B4)

Owned files edited:
- `vm/src/threading/virtual_threads.rs`
- `vm/src/threading/virtual_scheduler.rs`

B5 (`wait_for_non_daemon_threads`, in `thread_registry.rs`) and B7
(`WakeableCondvar::park`, in `event_loop.rs`) are NOT in my owned files —
left untouched.

---

## B3 (Medium) — `schedule_wakeup` leaked `wakeup_signals` entries on timer fire

File: `vm/src/threading/virtual_threads.rs`.

### Problem
`schedule_wakeup` inserted a `(vt_id, Arc<(Mutex<bool>, Condvar)>)` entry into
`self.wakeup_signals` and spawned a timer thread, but on timer fire the entry
was never removed — only `cancel_wakeup` removed entries. So every fired
`Thread.sleep` on a virtual thread permanently leaked one map entry.

### Fix
- Changed the field type from
  `Mutex<FxHashMap<u64, Arc<(Mutex<bool>, Condvar)>>>`
  to
  `Arc<Mutex<FxHashMap<u64, Arc<(Mutex<bool>, Condvar)>>>>`
  so a handle to the map can be moved into the `'static` timer closure
  without borrowing `&self`. The three other accessors
  (`schedule_wakeup` insert, `cancel_wakeup`, `get_wakeup_signal`) call
  `.lock()` which auto-derefs through the `Arc`, so they are unchanged
  except the inner-field type. The single constructor (`new`, delegated to
  by `with_default_parallelism`) now wraps the map in `Arc::new(...)`.
- The timer closure clones the `Arc<Mutex<map>>` and, after the timed wait
  completes (fired or cancelled), removes **its own** entry. Removal is by
  identity (`Arc::ptr_eq` against the signal it inserted) so a concurrent
  re-registration of the same `vt_id` by a fresh `schedule_wakeup` is never
  clobbered, and the cancel path (which already removed the entry) results in
  a no-op rather than a double-remove. This is idempotent and race-safe with
  `cancel_wakeup`.

### Blast radius
On the dead `VirtualThreadManager` path (report S1), so live blast radius is
currently nil; this is a correctness/leak fix that matters once the manager is
wired up. No behavior change to live code.

---

## B4 (Medium) — `VirtualThreadScheduler::release` could over-shoot `carrier_count`

File: `vm/src/threading/virtual_scheduler.rs`.

### Problem
`release()` did `state.available += 1` unconditionally. An unbalanced or
double `release()` (one not paired with a prior `acquire()`) raised
`available` above `carrier_count`, permanently weakening the concurrency bound
the carrier semaphore enforces (and in the limit could wrap on overflow).

### Fix
Clamp the increment to `carrier_count`:
`let next = (state.available + 1).min(self.carrier_count);`
Only when `next` actually increased do we store it and `notify_one()` — so a
clamped (already-full) stray release becomes a true no-op and does not
spuriously wake a waiter. Correct paired `acquire()`/`release()` behavior is
unchanged: `available` only ever rises back toward `carrier_count`, never past
it.

### Test added
`release_cannot_exceed_carrier_count` in the existing `#[cfg(test)] mod tests`:
constructs a 2-carrier scheduler, fires three stray releases (asserts
`available` stays at 2), drains both permits, then double-releases and asserts
exactly one permit is restored. Added a `#[cfg(test)]`-only `available(&self)`
introspection accessor (reads `state.available` under the lock) used by this
test; it is compiled out of all non-test builds.

---

## Config / compile safety
Both files are plain `vm` crate code with no `cfg`-feature gating on the
edited regions; the changes compile identically under default, app-stubs, and
synthetic-jdk configs. No new dependencies; `Arc`/`Mutex`/`Condvar` were
already imported and in use in both files.
