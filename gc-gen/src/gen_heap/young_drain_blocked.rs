#[allow(unused_imports)]
use super::*;

impl GenerationalHeap {
    /// gcd d9/c — could the last COMPLETED young cycle drain young at all?
    ///
    /// `true` after a non-moving sweep whose promotion walk was gated off.
    /// Under a live JIT the sweep's selective promotion is young's ONLY drain
    /// into old gen, and `gc_quiescence::unrewritable_peer_state` (a blocked
    /// peer's REFUSING helper window, or a frozen take-over peer) closes it for
    /// the cycle. While that holds, young can shed only what DIED since the
    /// last cycle, however much room old gen has.
    ///
    /// Why the readers need it (`gcd-d8x-heap-full-oome-shapes-livelock-on-fallback-young-cycles`):
    /// two rules assume "old gen has room, so a young cycle makes progress".
    ///
    /// * The trigger's anti-livelock floor (`young_trigger_floor_after_collection`)
    ///   stands down only when old gen is WEDGED. With young's live set past
    ///   the floor's cap and the drain blocked, the cap fires on every check:
    ///   one non-moving cycle per TLAB carve, each freeing only that carve, and
    ///   the allocation never fails, so the ladder (futile verdict, overhead
    ///   limit, `OutOfMemoryError`) is never entered.
    /// * The futile-young verdict (`note_forced_young_cycle_outcome`) calls a
    ///   forced cycle futile only when young cannot serve the ONE object that
    ///   missed. A sweep that frees the few objects the mutator allocated since
    ///   the last cycle always can, so the doors force one cycle per sliver.
    ///
    /// Both loops are the `[moving-young] fallback #N ...
    /// xt-helper-window-conservative-scan` signature: thousands of non-moving
    /// cycles, no error. `CRATONVM_GC_YOUNG_DRAIN_BLOCKED=0` answers `false`.
    pub fn young_drain_blocked(&self) -> bool {
        self.last_cycle_drain_blocked.load(Ordering::Relaxed)
    }

    /// gcd d9/c — young cannot drain ([`Self::young_drain_blocked`], or, gcj
    /// j1yg-thrash, an old generation wedged against young's capacity, the
    /// pair the trigger's floor stands down for) AND young's free room (bump
    /// tail plus reclaimed free-list bytes) is under `min_free` bytes: the
    /// last cycle left young starved and could not have done better. For the
    /// futile-young verdict, with the backoff's first quantum as `min_free`:
    /// such a cycle is futile even though the object that missed fits. The
    /// room is read like [`Self::young_bump_headroom`] reads it (the published
    /// triple, or the locked one under `CRATONVM_GC_TRIGGER_LOCKFREE=0`).
    pub fn young_starved_behind_blocked_drain(&self, min_free: usize) -> bool {
        let (used, free_list, capacity) = if trigger_lockfree_enabled() {
            self.young_from_published()
        } else {
            let from = self.lock_young_from();
            (from.used(), from.free_list_bytes(), from.capacity())
        };
        let cannot_drain = self.young_drain_blocked() || self.old_gen_wedged_against(capacity);
        young_starved_behind_blocked_drain_rule(cannot_drain, used, free_list, capacity, min_free)
    }

}
