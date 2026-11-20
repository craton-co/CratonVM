use super::*;
use crate::test_utils::{
    mock_heap_is_zgc, mock_take_keep_alive_log, MockKeepAlive, MockNativeContext,
};
#[allow(unused_imports)]
use cratonvm_native_api::NativeHeapAccess;
use cratonvm_types::ClassId;

/// gcj j1/rf: `slot0_read_keep_alive` keeps ZGC's slot-0 rule byte for
/// byte (`gc_reference_keep_alive` for any slot-0 object read) and hands
/// every other heap's slot-0 object read to the VM's
/// `gc_referent_slot_read_keep_alive` WITH the receiver (the VM's
/// Generational arm decides the precise `Reference`-subclass test). A
/// read of any other slot, or of a non-object, calls neither; the value
/// always comes back unchanged.
#[test]
fn gcj_j1rf_slot0_reads_route_zgc_unchanged_and_others_with_the_receiver() {
    let mut ctx = MockNativeContext::new();
    let receiver = ctx.alloc_object(ClassId::new(1), 2);
    let referent = ctx.alloc_object(ClassId::new(2), 1);
    let value = Value::Object(Some(referent));
    let (recv_addr, ref_addr) = (receiver.as_ptr() as usize, referent.as_ptr() as usize);
    let _ = mock_take_keep_alive_log();

    mock_heap_is_zgc(true);
    assert_eq!(slot0_read_keep_alive(&mut ctx, receiver, 0, value), value);
    assert_eq!(slot0_read_keep_alive(&mut ctx, receiver, 1, value), value);
    assert_eq!(
        mock_take_keep_alive_log(),
        vec![MockKeepAlive::Reference(ref_addr)],
        "ZGC: the slot-0 rule, unchanged"
    );

    mock_heap_is_zgc(false);
    assert_eq!(slot0_read_keep_alive(&mut ctx, receiver, 0, value), value);
    assert_eq!(slot0_read_keep_alive(&mut ctx, receiver, 1, value), value);
    assert_eq!(
        slot0_read_keep_alive(&mut ctx, receiver, 0, Value::Object(None)),
        Value::Object(None)
    );
    assert_eq!(slot0_read_keep_alive(&mut ctx, receiver, 0, Value::Int(7)), Value::Int(7));
    assert_eq!(
        mock_take_keep_alive_log(),
        vec![MockKeepAlive::ReferentSlot {
            receiver: recv_addr,
            slot: 0,
            referent: ref_addr,
        }],
        "G1 / Generational: one VM call, with the receiver, for the slot-0 object"
    );
}

/// gcj j2/x1: a `compareAndExchange` on slot 0 hands its witness (the
/// value it read, on a failed and on a successful exchange) to the VM's
/// referent keep-alive with the receiver on G1 / Generational; ZGC keeps
/// its slot-0 rule. Another slot or an array element never calls it.
#[test]
fn gcj2x1_slot0_cas_witness_publishes_before_exchange_on_every_collector() {
    use crate::unsafe_natives_ext::unsafe_compare_exchange_heap_slot as unsafe_cmpxchg;
    let mut ctx = MockNativeContext::new();
    let receiver = ctx.alloc_object(ClassId::new(1), 2);
    let held = ctx.alloc_object(ClassId::new(2), 1);
    let other = ctx.alloc_object(ClassId::new(2), 1);
    ctx.set_field(receiver, 0, Value::Object(Some(held)));
    ctx.set_field(receiver, 1, Value::Object(Some(held)));
    let (recv_addr, held_addr) = (receiver.as_ptr() as usize, held.as_ptr() as usize);
    let witness = Value::Object(Some(held));
    let miss = Value::Object(Some(other));
    let _ = mock_take_keep_alive_log();

    mock_heap_is_zgc(true);
    assert_eq!(
        unsafe_cmpxchg(&mut ctx, receiver, 0, miss, Value::Object(None)),
        witness
    );
    assert_eq!(
        mock_take_keep_alive_log(),
        vec![MockKeepAlive::Reference(held_addr)],
        "ZGC keeps its slot-0 witness alive"
    );

    mock_heap_is_zgc(false);
    // A failed exchange: the witness is the slot's referent.
    assert_eq!(
        unsafe_cmpxchg(&mut ctx, receiver, 0, miss, Value::Object(None)),
        witness
    );
    // Slot 1 is no referent slot.
    assert_eq!(
        unsafe_cmpxchg(&mut ctx, receiver, 1, miss, Value::Object(None)),
        witness
    );
    // A successful exchange hands back the same witness.
    assert_eq!(
        unsafe_cmpxchg(&mut ctx, receiver, 0, witness, miss),
        witness
    );
    assert_eq!(ctx.get_field(receiver, 0), miss, "the exchange happened");
    let one = MockKeepAlive::ReferentSlot {
        receiver: recv_addr,
        slot: 0,
        referent: held_addr,
    };
    assert_eq!(mock_take_keep_alive_log(), vec![one, one]);
}
