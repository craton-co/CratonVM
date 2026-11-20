//! Interpreter round i3 wave 72, lane L7: the two-field update
//! `this.a = this.b + k` (`CRATONVM_INTERP_FUSED_FIELD_UPDATE_TWO_FIELDS`,
//! `FIELD_INC_TWO_FIELDS`, `fused_two_field_update`). A hit stores what
//! the unfused `aload_0; aload_0; getfield #G; <operand>; iadd | isub;
//! putfield #F` stores into `#F` and leaves `#G` and the stack alone; a
//! decline writes nothing. Each test passes its mode bits explicitly.
use super::*;
use crate::runtime::interpreter::site_cache::PinnedSites;

/// Pinned sites (wave 73, lane L7): see `i68_l7_fused_field_increment_tests`.
type PinnedFast = PinnedSites<FastFieldSite>;

const GET: u16 = 0x0102;
const PUT: u16 = 0x0103;

/// `aload_0; aload_0; getfield #GET; <operand>; <arith>; <tail..>;
/// putfield #<put>; return`.
fn group(operand: &[u8], arith: u8, tail: &[u8], put: u16) -> Vec<u8> {
    let mut code = vec![0x2a, 0x2a, 0xb4, 0x01, 0x02];
    code.extend_from_slice(operand);
    code.push(arith);
    code.extend_from_slice(tail);
    // Cast: the test's two-byte pool index.
    code.extend_from_slice(&[0xb5, (put >> 8) as u8, put as u8, 0xb1]);
    code
}

/// An instance `(I)V` frame: local 0 = `recv`, local 1 = `5`.
fn frame(class_id: ClassId, code: Vec<u8>, recv: Value) -> Frame {
    Frame::new(
        class_id,
        "cratonvm/test/i72l7/TwoFields".to_string(),
        "m".to_string(),
        "(I)V".to_string(),
        None,
        code,
        vec![],
        4,
        3,
        &[recv, Value::Int(5)],
    )
}

fn fill(sites: &mut PinnedFast, class_id: ClassId, cp: u16, site: FastFieldSite) -> bool {
    sites.pin(class_id, cp, site)
}

/// A legacy two-field receiver (`b` in field 0, `a` in field 1, both
/// `Int`) and the sites `fill_site` would record for them under the
/// descriptors given; `None` when the allocation failed or came out
/// compact.
fn receiver(
    zgc: &crate::memory::vm_heap::VmHeap,
    class: u32,
    b: Value,
    a: Value,
    b_desc: u8,
    a_desc: u8,
) -> Option<(u64, FastFieldSite, FastFieldSite)> {
    let obj = zgc.try_alloc_object(ClassId::new(class), 2)?;
    let ptr = obj.as_ptr() as u64;
    // SAFETY: a fresh allocation starts with its header.
    let header = unsafe { &*(ptr as *const ObjectHeader) };
    if cratonvm_types::is_compact_object(header) {
        return None;
    }
    let site = |field_index: u32, desc_byte: u8| FastFieldSite {
        receiver_class_id: header.class_id,
        num_slots: header.num_slots(),
        offset: u32::try_from(HEADER_SIZE + field_index as usize * SLOT_SIZE).unwrap_or(0),
        storage: None,
        field_index,
        desc_byte,
        is_reference: false,
        final_put: FastFieldSite::PUT_ANY,
    };
    set(ptr, 0, b);
    set(ptr, 1, a);
    Some((ptr, site(0, b_desc), site(1, a_desc)))
}

fn set(ptr: u64, field: usize, v: Value) {
    // SAFETY: field `field`'s 16-byte cell of the live legacy object.
    unsafe {
        cratonvm_types::write_value_atomic(
            (ptr as *mut u8).add(HEADER_SIZE + field * SLOT_SIZE) as *mut Value,
            v,
        )
    };
}

fn get(ptr: u64, field: usize) -> Option<Value> {
    // SAFETY: as in `set`.
    unsafe {
        cratonvm_types::read_value_checked_atomic(
            (ptr as *mut u8).add(HEADER_SIZE + field * SLOT_SIZE) as *const Value,
        )
    }
}

const MODE: u8 = FIELD_INC_ON | FIELD_INC_WIDE | FIELD_INC_NO_DUP | FIELD_INC_TWO_FIELDS;

fn run(zgc: &crate::memory::vm_heap::VmHeap, sites: &mut PinnedFast, f: &mut Frame, mode: u8) -> Option<usize> {
    let code_len = f.code.len() - 2;
    sites.run(|cache| fused_field_increment_on(zgc, cache, f, code_len, 0, mode, Some(false)))
}

#[test]
fn the_bit_is_set_only_on_top_of_no_dup() {
    assert_eq!(with_two_fields(0, true), 0, "a switched-off mode stays off");
    assert_eq!(with_two_fields(FIELD_INC_ON, true), FIELD_INC_ON);
    assert_eq!(
        with_two_fields(FIELD_INC_ON | FIELD_INC_NO_DUP, true),
        FIELD_INC_ON | FIELD_INC_NO_DUP | FIELD_INC_TWO_FIELDS
    );
    assert_eq!(
        with_two_fields(FIELD_INC_ON | FIELD_INC_NO_DUP, false),
        FIELD_INC_ON | FIELD_INC_NO_DUP
    );
    for other in [
        FIELD_INC_CENSUS,
        FIELD_INC_PHASES,
        FIELD_INC_WIDE,
        FIELD_INC_EARLY_DECLINE,
        FIELD_INC_NO_DUP,
        FIELD_INC_VALUE,
        FIELD_INC_ON,
    ] {
        assert_eq!(other & FIELD_INC_TWO_FIELDS, 0);
    }
}

#[test]
fn the_loop_screen_passes_another_index_only_under_the_bit() {
    let two = group(&[0x04], 0x60, &[], PUT);
    let one = group(&[0x04], 0x60, &[], GET);
    assert!(!no_dup_group_may_fuse(&two, 0, false));
    assert!(no_dup_group_may_fuse(&two, 0, true));
    assert!(no_dup_group_may_fuse(&one, 0, false));
    assert!(no_dup_group_may_fuse(&one, 0, true));
    // A load operand (`a = b + c`) is still refused at the operand byte.
    let three = vec![
        0x2a, 0x2a, 0xb4, 0x01, 0x02, 0x2a, 0xb4, 0x01, 0x04, 0x60, 0xb5, 0x01, 0x03,
    ];
    assert!(!no_dup_group_may_fuse(&three, 0, true));
}

#[test]
fn the_decoder_takes_two_indexes_of_one_receiver() {
    let class_id = ClassId::new(0x00ab_0722);
    let f = frame(
        class_id,
        group(&[0x1b], 0x64, &[], PUT),
        Value::Object(None),
    );
    let code_len = f.code.len() - 2;
    let end = code_len - 1;
    assert_eq!(
        decode_two_field_update(&f, &f.code, code_len, 0, false),
        Some((
            GET,
            PUT,
            0,
            crate::runtime::interpreter::IncrementOperand::Int(5),
            true,
            None,
            end
        ))
    );
    let refuses = |code: Vec<u8>, wide: bool| {
        let f = frame(class_id, code, Value::Object(None));
        let code_len = f.code.len() - 2;
        assert_eq!(
            decode_two_field_update(&f, &f.code, code_len, 0, wide),
            None
        );
    };
    // One index: the one-field increment's.
    refuses(group(&[0x04], 0x60, &[], GET), true);
    // Two receivers.
    refuses(
        vec![
            0x2a, 0x2b, 0xb4, 0x01, 0x02, 0x04, 0x60, 0xb5, 0x01, 0x03, 0xb1,
        ],
        true,
    );
    // A conversion without `wide`.
    refuses(group(&[0x04], 0x60, &[0x91], PUT), false);
    // `imul` is not an increment.
    refuses(group(&[0x04], 0x68, &[], PUT), true);
    let f = frame(
        class_id,
        group(&[0x04], 0x60, &[0x91], PUT),
        Value::Object(None),
    );
    let code_len = f.code.len() - 2;
    assert_eq!(
        decode_two_field_update(&f, &f.code, code_len, 0, true).map(|d| d.5),
        Some(Some(0x91))
    );
}

#[test]
fn a_hit_stores_the_other_field_and_leaves_the_read_one() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let class_id = ClassId::new(0x00ab_0723);
    let Some((ptr, b_site, a_site)) =
        receiver(&zgc, 7231, Value::Int(41), Value::Int(0), b'I', b'I')
    else {
        return;
    };
    let mut sites = PinnedFast::new();
    if !fill(&mut sites, class_id, GET, b_site) || !fill(&mut sites, class_id, PUT, a_site) {
        return;
    }
    // SAFETY: `ptr` is the live object `receiver` allocated.
    let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
    let code = group(&[0x04], 0x60, &[], PUT);
    let end = code.len() - 1;
    let mut f = frame(class_id, code, recv);
    // Without the bit: a decline, nothing written.
    assert_eq!(
        run(&zgc, &mut sites, &mut f, MODE & !FIELD_INC_TWO_FIELDS),
        None
    );
    assert_eq!(get(ptr, 1), Some(Value::Int(0)));
    assert_eq!(run(&zgc, &mut sites, &mut f, MODE), Some(end));
    assert_eq!(get(ptr, 1), Some(Value::Int(42)), "a = b + 1");
    assert_eq!(get(ptr, 0), Some(Value::Int(41)), "b untouched");
    assert_eq!(f.stack.len(), 0, "nothing pushed");
    // Wrapping, and an `iload` operand (local 1 = 5) with `isub`.
    set(ptr, 0, Value::Int(i32::MIN));
    let mut f = frame(class_id, group(&[0x1b], 0x64, &[], PUT), recv);
    assert!(run(&zgc, &mut sites, &mut f, MODE).is_some());
    assert_eq!(get(ptr, 1), Some(Value::Int(i32::MIN.wrapping_sub(5))));
}

#[test]
fn a_store_site_of_another_width_declines_having_written_nothing() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let class_id = ClassId::new(0x00ab_0724);
    // `a` is a `byte` field but the group has no `i2b`; then an `I` read
    // into a `J` store; then a cold store site.
    for (a_desc, fill_put) in [(b'B', true), (b'J', true), (b'I', false)] {
        let Some((ptr, b_site, a_site)) =
            receiver(&zgc, 7241, Value::Int(7), Value::Int(3), b'I', a_desc)
        else {
            return;
        };
        let mut sites = PinnedFast::new();
        if !fill(&mut sites, class_id, GET, b_site)
            || (fill_put && !fill(&mut sites, class_id, PUT, a_site))
        {
            return;
        }
        // SAFETY: `ptr` is the live object `receiver` allocated.
        let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
        let mut f = frame(class_id, group(&[0x04], 0x60, &[], PUT), recv);
        assert_eq!(
            run(&zgc, &mut sites, &mut f, MODE),
            None,
            "{}",
            a_desc as char
        );
        assert_eq!(get(ptr, 1), Some(Value::Int(3)), "{}", a_desc as char);
        assert_eq!(get(ptr, 0), Some(Value::Int(7)));
    }
    // A null receiver: a decline (the unfused `getfield` throws).
    let mut sites = PinnedFast::new();
    let mut f = frame(
        class_id,
        group(&[0x04], 0x60, &[], PUT),
        Value::Object(None),
    );
    assert_eq!(run(&zgc, &mut sites, &mut f, MODE), None);
}

#[test]
fn a_narrow_store_is_stored_narrowed() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let class_id = ClassId::new(0x00ab_0725);
    let Some((ptr, b_site, a_site)) =
        receiver(&zgc, 7251, Value::Int(127), Value::Int(0), b'I', b'B')
    else {
        return;
    };
    let mut sites = PinnedFast::new();
    if !fill(&mut sites, class_id, GET, b_site) || !fill(&mut sites, class_id, PUT, a_site) {
        return;
    }
    // SAFETY: `ptr` is the live object `receiver` allocated.
    let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
    let mut f = frame(class_id, group(&[0x04], 0x60, &[0x91], PUT), recv);
    assert!(run(&zgc, &mut sites, &mut f, MODE).is_some());
    assert_eq!(get(ptr, 1), Some(Value::Int(-128)), "(byte) (127 + 1)");
    // Without `wide` the conversion is refused.
    set(ptr, 1, Value::Int(0));
    let mut f = frame(class_id, group(&[0x04], 0x60, &[0x91], PUT), recv);
    assert_eq!(run(&zgc, &mut sites, &mut f, MODE & !FIELD_INC_WIDE), None);
    assert_eq!(get(ptr, 1), Some(Value::Int(0)));
}
