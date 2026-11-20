use super::{
    note_osr_refused_activation, note_osr_refused_activation_at, osr_loop_filter_bit,
    osr_loop_key, osr_loop_may_have_record, osr_loop_offer_allowed, JitBridgeState,
    OSR_REFUSED_ACTIVATIONS_BUDGET,
};
use crate::config::VmConfig;
use crate::runtime::frame::Frame;
use crate::vm::SharedVm;
use cratonvm_types::ClassId;
use std::sync::atomic::Ordering;
use std::sync::Arc;

const CLASS: &str = "cratonvm/test/L5LoopKey";

fn owned_frame(class_id: u32, method: &str) -> Frame {
    Frame::new(
        ClassId::new(class_id),
        CLASS.to_string(),
        method.to_string(),
        "(I)V".to_string(),
        None,
        vec![0xb1],
        vec![],
        1,
        2,
        &[],
    )
}

fn cached_frame(class_id: u32, method: &str) -> Frame {
    let cached = cratonvm_jit_api::CachedBytecodeMethod::from_parts(
        cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: ClassId::new(class_id),
            class_name: Arc::from(CLASS),
            method_name: Arc::from(method),
            method_descriptor: Arc::from("(I)V"),
            source_file: None,
            code: Arc::from(vec![0xb1u8]),
            exception_table: Arc::from(Vec::new()),
            max_stack: 1,
            max_locals: 2,
            num_params: 1,
            is_synchronized: false,
            is_static: true,
        },
    );
    let (mut locals, mut stacks) = (Vec::new(), Vec::new());
    Frame::new_pooled_cached_compact(Arc::new(cached), &[], &mut locals, &mut stacks)
}

fn filter_words(bridge: &JitBridgeState) -> [u64; 4] {
    let mut words = [0u64; 4];
    for (word, slot) in words.iter_mut().zip(&bridge.osr_refused_loop_filter) {
        *word = slot.load(Ordering::Acquire);
    }
    words
}

/// A cached frame reaches the key through its entry's memoised
/// invocation key; an owned frame of the same method computes the same
/// key, so the two share one budget record. Name, descriptor, class and
/// pc still tell headers apart.
#[test]
fn a_cached_and_an_owned_frame_of_one_method_share_the_loop_key() {
    let cached = cached_frame(0x1520, "spin");
    assert!(cached.cached_method().is_some());
    let owned = owned_frame(0x1520, "spin");
    assert!(owned.cached_method().is_none());
    assert_eq!(osr_loop_key(&cached, 6), osr_loop_key(&owned, 6));
    assert_ne!(osr_loop_key(&cached, 6), osr_loop_key(&owned, 7));
    assert_ne!(
        osr_loop_key(&cached, 6),
        osr_loop_key(&cached_frame(0x1520, "spun"), 6)
    );
    assert_ne!(
        osr_loop_key(&cached, 6),
        osr_loop_key(&cached_frame(0x1521, "spin"), 6)
    );
    // A budget spent through one kind of frame is spent for the other.
    let shared = SharedVm::new(VmConfig::default());
    for _ in 0..OSR_REFUSED_ACTIVATIONS_BUDGET {
        note_osr_refused_activation(&shared, &cached, 6);
    }
    assert!(!osr_loop_offer_allowed(&shared, &owned, 6));
    assert!(osr_loop_offer_allowed(&shared, &owned, 7));
}

/// The filter holds exactly the bits of the recorded headers, follows
/// every change of the map, and is empty again with it.
#[test]
fn the_filter_follows_the_refused_loop_map() {
    let shared = SharedVm::new(VmConfig::default());
    let bridge = &shared.jit.bridge;
    let epoch = shared.jit.code_state_epoch();
    let a = osr_loop_key(&owned_frame(0x1522, "a"), 4);
    assert_eq!(filter_words(bridge), [0; 4]);
    assert!(!osr_loop_may_have_record(bridge, a));
    note_osr_refused_activation_at(bridge, a, epoch);
    assert!(osr_loop_may_have_record(bridge, a));
    let (word, bit) = osr_loop_filter_bit(a);
    let mut expected = [0u64; 4];
    expected[word] = bit;
    assert_eq!(filter_words(bridge), expected, "one record, one bit");
    // A header whose bit differs reads "no record" without the map.
    let other = (4..4096)
        .map(|pc| osr_loop_key(&owned_frame(0x1522, "a"), pc))
        .find(|key| osr_loop_filter_bit(*key) != (word, bit))
        .expect("256 bits: some pc maps elsewhere");
    assert!(!osr_loop_may_have_record(bridge, other));
    // An entry forgets the record; the filter empties with the map.
    let frame = owned_frame(0x1522, "a");
    super::note_osr_loop_entered(&shared, &frame, 4);
    assert_eq!(bridge.osr_refused_loop_count.load(Ordering::Acquire), 0);
    assert_eq!(filter_words(bridge), [0; 4]);
    // A record of an older epoch purged by a newer refusal takes its bit
    // with it.
    note_osr_refused_activation_at(bridge, a, epoch);
    note_osr_refused_activation_at(bridge, other, epoch + 1);
    let (other_word, other_bit) = osr_loop_filter_bit(other);
    let mut expected = [0u64; 4];
    expected[other_word] = other_bit;
    assert_eq!(filter_words(bridge), expected);
}

/// A consult for a header with no record takes no lock, even though the
/// VM has a record elsewhere: it answers while another thread holds the
/// map's lock. (Before wave 20 it hashed the names and waited here.)
#[test]
fn a_header_without_a_record_is_answered_without_the_lock() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let recorded = owned_frame(0x1523, "hot");
    for _ in 0..OSR_REFUSED_ACTIVATIONS_BUDGET {
        note_osr_refused_activation(&shared, &recorded, 4);
    }
    let (word_bit, spare_pc) = {
        let bit = osr_loop_filter_bit(osr_loop_key(&recorded, 4));
        let pc = (5..4096)
            .find(|pc| osr_loop_filter_bit(osr_loop_key(&recorded, *pc)) != bit)
            .expect("256 bits: some pc maps elsewhere");
        (bit, pc)
    };
    assert_ne!(
        osr_loop_filter_bit(osr_loop_key(&recorded, spare_pc)),
        word_bit
    );
    let guard = shared.jit.bridge.osr_refused_loops.lock();
    let (tx, rx) = std::sync::mpsc::channel();
    let asker = {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            let frame = owned_frame(0x1523, "hot");
            let _ = tx.send(osr_loop_offer_allowed(&shared, &frame, spare_pc));
        })
    };
    let answer = rx.recv_timeout(std::time::Duration::from_secs(20));
    drop(guard);
    let _ = asker.join();
    assert_eq!(answer.ok(), Some(true), "answered without the map's lock");
    assert!(!osr_loop_offer_allowed(&shared, &recorded, 4));
}
