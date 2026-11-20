use super::*;
use cratonvm_jit_api as api;
use std::sync::atomic::AtomicUsize;

fn fake(addr: usize) -> ObjectRef {
    // SAFETY: only compared and hashed by address; never dereferenced.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn word(stack: &JmxLockStack, off: usize) -> &AtomicUsize {
    // SAFETY: `off` is a pinned word offset inside the `#[repr(C)]` stack
    // (the const assertions beside `JmxLockStack` check every one), and
    // each of those words is an 8-byte, 8-aligned atomic.
    unsafe { &*((stack as *const JmxLockStack as *const u8).add(off) as *const AtomicUsize) }
}

fn spilled(stack: &JmxLockStack) -> bool {
    let p = (stack as *const JmxLockStack as *const u8)
        .wrapping_add(api::JIT_LOCK_STACK_SPILLED_OFFSET) as *const AtomicBool;
    // SAFETY: as in `word`, for the one-byte `spilled` flag.
    unsafe { (*p).load(Ordering::Relaxed) }
}

/// What the JIT's inline `monitorenter` (`runtime_lowering::
/// emit_inline_thin_lock`, both tiers) does after its CAS won.
/// `false` = it would have taken the helper instead.
fn jit_push(stack: &JmxLockStack, a: usize) -> bool {
    jit_push_as(stack, a, false)
}

/// What the inline `monitorexit` does after its CAS won. `false` = helper.
fn jit_pop(stack: &JmxLockStack, a: usize) -> bool {
    jit_pop_as(stack, a, false)
}

/// [`jit_push`] in either compiled shape: the odd/even bracket, or
/// (`single`, round 13 wave 12, `CRATONVM_JIT_LOCK_STACK_SINGLE_BUMP`)
/// slot, `top`, then one `seq += 2`. The single shape's stores are
/// `Release`, which is what the emitted `MOV`s are on x86-TSO (every
/// store is visible after the ones before it).
fn jit_push_as(stack: &JmxLockStack, a: usize, single: bool) -> bool {
    let top = word(stack, api::JIT_LOCK_STACK_TOP_OFFSET).load(Ordering::Relaxed);
    if spilled(stack) || top >= api::JIT_LOCK_STACK_SLOTS {
        return false;
    }
    let seq = word(stack, api::JIT_LOCK_STACK_SEQ_OFFSET);
    let s = seq.load(Ordering::Relaxed);
    let slot = word(stack, api::JIT_LOCK_STACK_SLOTS_OFFSET + 8 * top);
    let top_word = word(stack, api::JIT_LOCK_STACK_TOP_OFFSET);
    if single {
        slot.store(a, Ordering::Release);
        top_word.store(top + 1, Ordering::Release);
        seq.store(s + 2, Ordering::Release);
        return true;
    }
    seq.store(s + 1, Ordering::Relaxed);
    slot.store(a, Ordering::Relaxed);
    top_word.store(top + 1, Ordering::Relaxed);
    seq.store(s + 2, Ordering::Relaxed);
    true
}

/// [`jit_pop`] in either compiled shape (see [`jit_push_as`]).
fn jit_pop_as(stack: &JmxLockStack, a: usize, single: bool) -> bool {
    let top = word(stack, api::JIT_LOCK_STACK_TOP_OFFSET).load(Ordering::Relaxed);
    if top == 0 || top > api::JIT_LOCK_STACK_SLOTS {
        return false;
    }
    let slot = word(stack, api::JIT_LOCK_STACK_SLOTS_OFFSET + 8 * (top - 1));
    if slot.load(Ordering::Relaxed) != a {
        return false;
    }
    let seq = word(stack, api::JIT_LOCK_STACK_SEQ_OFFSET);
    let s = seq.load(Ordering::Relaxed);
    let top_word = word(stack, api::JIT_LOCK_STACK_TOP_OFFSET);
    if single {
        slot.store(0, Ordering::Release);
        top_word.store(top - 1, Ordering::Release);
        seq.store(s + 2, Ordering::Release);
        return true;
    }
    seq.store(s + 1, Ordering::Relaxed);
    slot.store(0, Ordering::Relaxed);
    top_word.store(top - 1, Ordering::Relaxed);
    seq.store(s + 2, Ordering::Relaxed);
    true
}

/// Round 13 wave 12 (lane monitor3, proposal S7-4): a single-bump
/// compiled writer racing a cross-thread [`JmxLockStack::snapshot`]
/// never shows the reader a set the owner did not hold at one instant.
/// The writer cycles `push x_i; push y_i; pop y_i; pop x_i` over four
/// pairs, so the only true states are `[]`, `[x_i]` and `[x_i, y_i]`; a
/// torn read would mix two pairs (`[x_i, y_j]`) or show a `y` alone.
#[test]
fn a_single_bump_writer_never_shows_a_reader_a_torn_set() {
    const X0: usize = 0x70_000;
    const PAIR_STRIDE: usize = 0x100;
    const Y_OFF: usize = 0x40;
    let stack = JmxLockStack::default();
    let done = AtomicBool::new(false);
    let is_x = |v: usize| v >= X0 && v < X0 + 4 * PAIR_STRIDE && (v - X0) % PAIR_STRIDE == 0;
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for i in 0..200_000usize {
                let x = X0 + (i % 4) * PAIR_STRIDE;
                let y = x + Y_OFF;
                assert!(jit_push_as(&stack, x, true));
                assert!(jit_push_as(&stack, y, true));
                assert!(jit_pop_as(&stack, y, true));
                assert!(jit_pop_as(&stack, x, true));
            }
            done.store(true, Ordering::Release);
        });
        let mut reads = 0u64;
        while !done.load(Ordering::Acquire) || reads == 0 {
            let snap: Vec<usize> = stack
                .snapshot()
                .iter()
                .map(|o| o.as_ptr() as usize)
                .collect();
            let consistent = match snap.as_slice() {
                [] => true,
                [x] => is_x(*x),
                [x, y] => is_x(*x) && *y == *x + Y_OFF,
                _ => false,
            };
            assert!(consistent, "torn snapshot {snap:x?} after {reads} reads");
            reads += 1;
        }
    });
    let seq = word(&stack, api::JIT_LOCK_STACK_SEQ_OFFSET).load(Ordering::Relaxed);
    assert_eq!(seq, 4 * 2 * 200_000, "every write moved seq by exactly 2");
    assert_eq!(stack.snapshot().len(), 0);
}

/// Compiled and helper acquisitions/releases interleaved at random must
/// leave the stack reporting exactly the held set, in acquisition order,
/// to both the JMX snapshot and the collector: the model the helper-only
/// test in `tests` checks, with the inline writer added.
#[test]
fn inline_and_helper_writers_agree_on_one_lock_stack() {
    let stack = JmxLockStack::default();
    let mut model: Vec<usize> = Vec::new();
    let (mut pushed, mut popped) = (0u32, 0u32);
    let mut x: u64 = 0xD1B5_4A32_D192_ED03;
    for step in 0..20_000u32 {
        x = x
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let a = 0x30_000usize + ((x >> 33) % 20) as usize * 0x40;
        match (x >> 59) & 7 {
            // A compiled `monitorenter`: inline only for a monitor this
            // thread does not hold (its mark word is NEUTRAL); held means
            // recursion, which is the helper's publish (a dedupe).
            0..=2 => {
                if model.contains(&a) {
                    stack.publish(fake(a));
                } else {
                    // Both compiled shapes, alternating (round 13 wave 12).
                    if jit_push_as(&stack, a, step % 2 == 1) {
                        pushed += 1;
                    } else {
                        stack.publish(fake(a));
                    }
                    model.push(a);
                }
            }
            // A compiled outermost `monitorexit` of the innermost monitor.
            3 | 4 => {
                if let Some(&last) = model.last() {
                    if jit_pop_as(&stack, last, step % 2 == 1) {
                        popped += 1;
                    } else {
                        stack.retract(fake(last));
                    }
                    model.retain(|&m| m != last);
                }
            }
            // A helper-path acquisition, and an out-of-order release.
            5 => {
                stack.publish(fake(a));
                if !model.contains(&a) {
                    model.push(a);
                }
            }
            _ => {
                stack.retract(fake(a));
                model.retain(|&m| m != a);
            }
        }
        let seq = word(&stack, api::JIT_LOCK_STACK_SEQ_OFFSET).load(Ordering::Relaxed);
        assert_eq!(seq & 1, 0, "step {step}: a write left the seqlock open");
        let snap: Vec<usize> = stack
            .snapshot()
            .iter()
            .map(|o| o.as_ptr() as usize)
            .collect();
        assert_eq!(snap, model, "step {step}: the snapshot diverged");
        let mut roots = Vec::new();
        stack.extend_roots_stw(&mut roots);
        let roots: Vec<usize> = roots.iter().map(|o| o.as_ptr() as usize).collect();
        assert_eq!(roots, model, "step {step}: the GC roots diverged");
    }
    if monitor_fastpath_enabled() {
        assert!(pushed > 0 && popped > 0, "the inline writer never ran");
    }
}

fn block_word(block: &crate::threading::jvm_thread::JitMonitorBlock, off: usize) -> u64 {
    let p = (block as *const crate::threading::jvm_thread::JitMonitorBlock as *const u8)
        .wrapping_add(off) as *const u64;
    // SAFETY: `off` is one of the two pinned `JIT_MONITOR_BLOCK_*` offsets
    // of the `#[repr(C)]` block, each an 8-byte, 8-aligned word.
    unsafe { *p }
}

/// Arming publishes the thread's thin-lock LEASE (owner bits and `held`
/// counter, from the VM's `MonitorTable`) and the address of the SAME lock
/// stack the helpers write, and refuses what the helpers refuse.
#[test]
fn arming_points_compiled_code_at_the_helpers_lock_stack() {
    use crate::threading::jvm_thread::JitMonitorBlock;
    use crate::threading::monitor::MonitorTable;
    let registry = ThreadRegistry::new();
    let table = MonitorTable::new();
    let tid = registry.next_thread_id();
    let mut block = JitMonitorBlock::unarmed();
    let owner = |b: &JitMonitorBlock| block_word(b, api::JIT_MONITOR_BLOCK_THIN_OWNER_OFFSET);
    // Unregistered: stays unarmed. Since round 11 wave 16 the lease is
    // asked FIRST (a refused thread skips the registry lookup), so an
    // unregistered thread may lease its slot here: the same slot its
    // helper-path `monitorenter` would lease anyway, and the one the
    // lookup below answers again.
    registry.arm_jit_monitor_block(&mut block, tid, || table.jit_thin_lease(tid));
    assert_eq!(owner(&block), u64::MAX);
    registry.register(tid, "armed", None);
    // No slot to lease (every one taken): stays unarmed.
    registry.arm_jit_monitor_block(&mut block, tid, || None);
    assert_eq!(owner(&block), u64::MAX);
    registry.arm_jit_monitor_block(&mut block, tid, || table.jit_thin_lease(tid));
    if !monitor_fastpath_enabled() {
        // The kill switch leaves every thread on the helper path.
        assert_eq!(owner(&block), u64::MAX);
        return;
    }
    let lease = table
        .jit_thin_lease(tid)
        .expect("a fresh table has free slots");
    assert_eq!(owner(&block), u64::from(lease.owner_bits));
    assert_eq!(
        owner(&block) & u64::from(cratonvm_types::MARK_STATE_MASK),
        u64::from(cratonvm_types::MARK_THIN_LOCKED),
        "the owner bits are a THIN_LOCKED word at recursion 0"
    );
    assert_eq!(
        block_word(&block, api::JIT_MONITOR_BLOCK_HELD_OFFSET) as usize,
        lease.held
    );
    assert_ne!(lease.held, 0);
    let ls = block_word(&block, api::JIT_MONITOR_BLOCK_LOCK_STACK_OFFSET) as usize;
    assert_ne!(ls, 0);
    // SAFETY: the block holds an `Arc` of the book `ls` points into.
    let stack = unsafe { &*(ls as *const JmxLockStack) };
    assert!(jit_push(stack, 0x40_000));
    registry.complete_jmx_monitor_enter(tid, fake(0x40_040));
    let seen = registry
        .with_self_jmx_monitors(tid, |b| b.locked.snapshot())
        .expect("registered");
    let seen: Vec<usize> = seen.iter().map(|o| o.as_ptr() as usize).collect();
    assert_eq!(seen, vec![0x40_000, 0x40_040], "one stack, two writers");
    registry.remove_jmx_locked_monitor(tid, fake(0x40_040));
    assert!(jit_pop(stack, 0x40_000));
    // Re-arming from the same registry is a no-op, and asks for no lease.
    registry.arm_jit_monitor_block(&mut block, tid, || {
        panic!("an armed block must not lease again")
    });
    assert_eq!(
        block_word(&block, api::JIT_MONITOR_BLOCK_LOCK_STACK_OFFSET) as usize,
        ls
    );
    // A block armed from another registry is never left pointing at it.
    let other = ThreadRegistry::new();
    other.arm_jit_monitor_block(&mut block, tid, || table.jit_thin_lease(tid));
    assert_eq!(owner(&block), u64::MAX);
    assert_eq!(block_word(&block, api::JIT_MONITOR_BLOCK_HELD_OFFSET), 0);
}
