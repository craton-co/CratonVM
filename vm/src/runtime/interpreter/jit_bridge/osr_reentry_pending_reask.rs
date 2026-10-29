//! j27 lane ONESHOT (`CRATONVM_JIT_OSR_REENTRY_PENDING_REASK`, opt-in;
//! page `docs/internal/jit/j27-osr-reentry-memo-pending-verdict-never-reasked-FIXED-20261029.md`):
//! a re-entry memo entry put while the worker still owed its optimizing OSR
//! verdict (`OsrReentryMemo::optimizing_pending`) re-asks that verdict now and
//! then, instead of waiting for an answer that may never come.
//!
//! The race. `try_osr`'s background arm queues an OSR task for the pc and
//! enters the single-pass body; the memo entry it puts is marked pending.
//! Every later hit asks `osr_reentry_memo_defers_to_optimizing`, which yields
//! only when the worker CACHED an accepted body, and clears the mark only
//! when the worker memoised a REFUSAL. A worker that declined for a transient
//! reason (a resolver miss, a transient compile decline, an unpinned baked
//! callee, a stale epoch, a dependency change: each "not memoised; retried
//! later"), or a request the manager refused because the OSR slot was held,
//! leaves neither. The entry then stays pending, every hit stands, and a hit
//! skips the whole optimizing arm, the only place that queues the task again.
//! Only a JIT-cache generation move or a deopt anywhere in the VM ends it.
//! The worker answering before the slot frees is the good order. The other
//! order leaves the loop on its single-pass body.
//!
//! The fix. Re-ask the worker on the 16th, 32nd, 64th, ... pending hit of one
//! entry ([`pending_hit_reasks`]): at most about 28 asks per entry over a
//! `u32` count. Each ask is the arm's own idempotent `request_osr`. The hit
//! still stands, so the re-entry stays as cheap as the memo makes it. Once the
//! worker caches a body, the next hit yields to it as before.

use super::*;

/// The first pending hit that re-asks. Small hits are the worker still
/// building: asking then would only be refused while the slot is held.
const FIRST_REASK_HIT: u32 = 16;

/// `CRATONVM_JIT_OSR_REENTRY_PENDING_REASK` (opt-in, `=1` enables): opt-in
/// because the extra worker builds it can cost were not measured. Read only on
/// a hit that would re-ask (cold).
fn pending_reask_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_OSR_REENTRY_PENDING_REASK")
}

/// Does pending hit number `n` (1-based) re-ask the worker? Pure.
pub(super) fn pending_hit_reasks(n: u32) -> bool {
    n >= FIRST_REASK_HIT && n.is_power_of_two()
}

/// Is the manager's answer to a re-ask final (the verdict is no longer owed)?
/// Only `Refused`: `Pending` is a held slot, `Queued` a build still to judge.
fn reask_answer_is_final(answer: &crate::jit::tiered::OsrRequest) -> bool {
    matches!(answer, crate::jit::tiered::OsrRequest::Refused)
}

/// A pending hit of the thread's current memo entry whose verdict is still
/// owed (nothing cached, nothing refused). The hit is counted on the entry
/// itself (`OsrReentryMemo::pending_hits`), so a new entry restarts the
/// count. `Some(queued)` when this hit re-asked the worker, `queued` saying
/// whether the manager took the task; `None` otherwise.
pub(super) fn reask_owed_optimizing_verdict(
    shared: &SharedVm,
    class_id: ClassId,
    (class_name, method_name, descriptor): (&str, &str, &str),
    entry_pc: usize,
) -> Option<bool> {
    // 0 without an entry: never a re-ask.
    let n = osr_reentry_memo_note_pending_hit();
    if !pending_hit_reasks(n) || !pending_reask_enabled() {
        return None;
    }
    // The pending mark is put only by the background arm, which started the
    // compiler, so this is that arm's request and nothing more.
    let answer = shared.jit.tiered_manager.request_osr_classified(
        &crate::jit::tiered::MethodKey::with_class_id(
            class_id,
            class_name,
            method_name,
            descriptor,
        ),
        // Cast: a bci fits u32 (`IR_MAX_BYTECODE_SIZE` is far below it).
        entry_pc as u32,
    );
    let queued = matches!(answer, crate::jit::tiered::OsrRequest::Queued(_));
    // j27 wave 8 (lane W8OSR): a `Refused` answer (OSR denied, `c2_bailout`,
    // retry budget spent, tiering off) is the manager's final verdict until a
    // redefinition or an install-epoch move, the same as a memoised refusal:
    // clear the mark so later hits stop probing and stop re-asking.
    if reask_answer_is_final(&answer) {
        osr_reentry_memo_set_optimizing_pending(false);
    }
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] osr reentry memo RE-ASKS the owed optimizing verdict \
             {class_name}.{method_name}{descriptor} pc={entry_pc} (pending hit #{n}): OSR task {}",
            if queued { "queued" } else { "not queued" }
        );
    }
    Some(queued)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "CRATONVM_JIT_OSR_REENTRY_PENDING_REASK";

    #[test]
    fn only_the_16th_and_later_powers_of_two_reask() {
        let asks: Vec<u32> = (1..=200).filter(|&n| pending_hit_reasks(n)).collect();
        assert_eq!(asks, vec![16, 32, 64, 128]);
        assert!(!pending_hit_reasks(u32::MAX), "a saturated count stops asking");
    }

    /// The losing order: the worker's verdict came back transient, so the
    /// entry stays pending with nothing cached and nothing refused. Off,
    /// no hit ever asks again (the stuck state). On, the 16th, 32nd and 64th
    /// hits do, and a queued re-ask is a task the worker will judge.
    /// Put a fresh pending memo entry for `cid` on this thread (the state
    /// `try_osr`'s background arm leaves); `false` when the memo is off.
    fn put_pending_entry(shared: &SharedVm, cid: ClassId, method: &str) -> bool {
        let Some(stamp) = osr_reentry_stamp(shared) else {
            return false; // CRATONVM_JIT_OSR_REENTRY_MEMO=0
        };
        let single_pass = {
            let mut buf = cratonvm_jit::ExecutableBuffer::new(64).unwrap();
            buf.emit(&[0xC3]); // ret
            let mut cm = cratonvm_jit::CompiledMethod::new(buf);
            cm.compiled_via_osr = true;
            cm.osr_pc_to_native = Some(vec![0; 32]);
            Arc::new(cm)
        };
        let (m, d): (Arc<str>, Arc<str>) = (Arc::from(method), Arc::from("()V"));
        osr_reentry_memo_put(shared, stamp, 0x4747, cid, &m, &d, 7, &single_pass);
        osr_reentry_memo_set_optimizing_pending(true);
        true
    }

    #[test]
    fn a_pending_entry_with_no_verdict_reasks_only_with_the_switch() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let names = ("j27/OneShotLoop", "run", "()V");
        let cid = ClassId::new(4747);
        if !put_pending_entry(&shared, cid, names.1) {
            return;
        }
        let run = || -> Vec<u32> {
            (1..=64)
                .filter(|_| reask_owed_optimizing_verdict(&shared, cid, names, 7).is_some())
                .collect()
        };
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            assert!(run().is_empty(), "off: never re-asked");
        });
        assert!(put_pending_entry(&shared, cid, names.1));
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("1"))], || {
            let mut asked = Vec::new();
            for n in 1..=64u32 {
                if let Some(queued) = reask_owed_optimizing_verdict(&shared, cid, names, 7) {
                    asked.push(n);
                    if queued {
                        assert!(
                            !shared.jit.tiered_manager.queue_empty(),
                            "a queued re-ask leaves a task for the worker"
                        );
                    }
                }
            }
            assert_eq!(asked, vec![16, 32, 64]);
        });
        osr_reentry_memo_clear();
    }

    /// The count lives on the entry: a new entry restarts it, and with no
    /// entry there is no hit to count (so never a re-ask).
    #[test]
    fn another_entry_restarts_the_count() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let cid = ClassId::new(4949);
        if !put_pending_entry(&shared, cid, "a") {
            return;
        }
        for _ in 0..15 {
            osr_reentry_memo_note_pending_hit();
        }
        assert_eq!(osr_reentry_memo_note_pending_hit(), 16);
        assert!(put_pending_entry(&shared, cid, "b"));
        assert_eq!(osr_reentry_memo_note_pending_hit(), 1, "b's entry starts at 1");
        osr_reentry_memo_clear();
        assert_eq!(osr_reentry_memo_note_pending_hit(), 0, "no entry, no count");
    }

    /// j27 wave 8 (lane W8OSR): a re-ask the manager REFUSES (here: OSR
    /// denied for the method) is a final verdict, so the entry's pending mark
    /// clears and later hits are exact. Old code: the mark stayed set and
    /// every later power-of-two hit re-asked a manager that would never queue.
    #[test]
    fn a_refused_reask_clears_the_pending_mark() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let Some(stamp) = osr_reentry_stamp(&shared) else {
            return; // CRATONVM_JIT_OSR_REENTRY_MEMO=0
        };
        let (cname, mname, desc) = ("j27/W8Refused", "run", "()V");
        let cid = ClassId::new(4848);
        let single_pass = {
            let mut buf = cratonvm_jit::ExecutableBuffer::new(64).unwrap();
            buf.emit(&[0xC3]); // ret
            let mut cm = cratonvm_jit::CompiledMethod::new(buf);
            cm.compiled_via_osr = true;
            cm.osr_pc_to_native = Some(vec![0; 32]);
            Arc::new(cm)
        };
        let (m, d): (Arc<str>, Arc<str>) = (Arc::from(mname), Arc::from(desc));
        osr_reentry_memo_put(&shared, stamp, 0x4848, cid, &m, &d, 7, &single_pass);
        osr_reentry_memo_set_optimizing_pending(true);
        shared
            .jit
            .tiered_manager
            .mark_osr_denied(crate::jit::tiered::MethodKey::with_class_id(
                cid, cname, mname, desc,
            ));
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("1"))], || {
            for _ in 0..16 {
                let _ = reask_owed_optimizing_verdict(&shared, cid, (cname, mname, desc), 7);
            }
        });
        assert!(
            !osr_reentry_memo_optimizing_pending(),
            "a refused re-ask is the final verdict: the entry is exact again"
        );
        osr_reentry_memo_clear();
    }
}
