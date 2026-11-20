/// The invoke doors' profile coalescing: the `nth` call in a row of one
/// (site, receiver) is recorded when this answers `Some(weight)`, the calls
/// that record stands for (itself and the skipped ones before it); `None`
/// skips it. The first [`COALESCE_FULL`] calls are each recorded, then every
/// 32nd. The weight keeps the profile in calls, so the optimizing tier's
/// evidence bars (`MIN_GUARDED_RECEIVER_EVIDENCE`, the hot/cold splice order)
/// read the same counts an unsampled profile would.
#[inline(always)]
fn coalesced_record_weight(nth: u32) -> Option<u32> {
    const EVERY: u32 = 32;
    if nth <= COALESCE_FULL {
        return Some(1);
    }
    (nth % EVERY == 0).then(|| nth - nth.saturating_sub(EVERY).max(COALESCE_FULL))
}

/// Calls of a (site, receiver) run recorded one by one before
/// [`coalesced_record_weight`] samples.
const COALESCE_FULL: u32 = 4;

/// The per-method streak layer of the coalescing: `streak` is the method's
/// `(last pc, last receiver, run length, carried calls)`. Takes one record of
/// `weight` calls at `(pc, recv)` and answers the calls to record there now
/// (`None`: carried to the run's next record) and an ended run's carried
/// calls, to record at that run's own `(pc, receiver)`. No call is lost.
#[inline(always)]
fn coalesce_streak_step(
    streak: &mut (usize, u32, u16, u32),
    pc: usize,
    recv: u32,
    weight: u32,
) -> (Option<u32>, Option<(usize, u32, u32)>) {
    let (last_pc, last_recv, run, carried) = streak;
    if *last_pc == pc && *last_recv == recv {
        *run = run.wrapping_add(1);
        if coalesced_record_weight(u32::from(*run)).is_none() {
            *carried = carried.saturating_add(weight);
            return (None, None);
        }
        return (Some(weight.saturating_add(std::mem::take(carried))), None);
    }
    let ended = (*carried != 0).then(|| (*last_pc, *last_recv, *carried));
    *streak = (pc, recv, 1, 0);
    (Some(weight), ended)
}

/// Record `n` calls at `pc` on the method's handle: a call-site count for the
/// call-site sentinel `recv == u32::MAX`, a receiver row otherwise. `false`:
/// the slot was reclaimed.
#[inline(always)]
fn record_coalesced(
    rec: &cratonvm_jit::profile::ReceiverRecorder,
    pc: usize,
    recv: u32,
    frame_code_len: usize,
    n: u32,
) -> bool {
    if recv == u32::MAX {
        rec.record_call_site_n(pc, frame_code_len, n)
    } else {
        rec.record_n(pc, recv, frame_code_len, n)
    }
}
