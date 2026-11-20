use super::*;
use cratonvm_types::ClassId;

fn ids(complete: bool) -> SubIds {
    SubIds {
        sub: ClassId::new(7),
        class: Some(ClassId::new(8)),
        array_list: complete.then(|| ClassId::new(9)),
        this0: 1,
        key: 2,
    }
}

/// `shape_locks` rebuilds exactly the `locks` vector `sub_key_shape` builds
/// beside each of its three shapes.
#[test]
fn shape_locks_matches_the_three_shapes() {
    // A `Class` key: `(vec![1, k], vec![k])`.
    assert_eq!(shape_locks(&[1, 0x55]), vec![0x55]);
    // Anything by identity: `(vec![3, k], vec![k])`.
    assert_eq!(shape_locks(&[3, 0x66]), vec![0x66]);
    // An `Arrays$ArrayList` of n classes: `([2, n, k1..kn], [k1..kn])`.
    assert_eq!(shape_locks(&[2, 3, 0x10, 0x20, 0x30]), vec![0x10, 0x20, 0x30]);
    assert_eq!(shape_locks(&[2, 0]), Vec::<usize>::new());
    assert_eq!(shape_locks(&[]), Vec::<usize>::new());
}

/// The memo holds only a complete row, answers only its own VM, and the
/// latest complete row replaces the earlier one (a thread serving two VMs).
#[test]
fn the_memo_answers_only_a_complete_row_of_its_own_vm() {
    const VM_A: usize = 0x7155_0001;
    const VM_B: usize = 0x7155_0002;
    std::thread::spawn(|| {
        assert!(sub_ids_memo_get(VM_A).is_none());
        sub_ids_memo_put(VM_A, ids(false));
        assert!(sub_ids_memo_get(VM_A).is_none(), "a partial row was memoized");
        sub_ids_memo_put(VM_A, ids(true));
        let got = sub_ids_memo_get(VM_A).expect("complete row");
        assert_eq!(got.array_list, Some(ClassId::new(9)));
        assert!(sub_ids_memo_get(VM_B).is_none(), "VM B read VM A's row");
        sub_ids_memo_put(VM_B, ids(true));
        assert!(sub_ids_memo_get(VM_B).is_some());
        assert!(sub_ids_memo_get(VM_A).is_none(), "one row per thread");
    })
    .join()
    .unwrap();
}

/// A mock VM that has loaded `Sub`, `Class` and `Arrays$ArrayList` (so
/// its [`SubIds`] row is complete), with the switch set as given.
fn vm_with_sub(vm: usize, fast: bool) -> crate::test_utils::MockNativeContext {
    use cratonvm_native_api::FieldMetadata;
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeSystemAccess};
    let mut c = crate::test_utils::mock_ctx();
    c.set_vm_identity(vm);
    c.clv_sub_fast_key_flag.set(fast);
    let sub = c.ensure_class_initialized(SUB_CLASS).expect("Sub");
    c.ensure_class_initialized("java/lang/Class").expect("Class");
    c.ensure_class_initialized("java/util/Arrays$ArrayList")
        .expect("Arrays$ArrayList");
    let field = |name: &str, slot: usize| FieldMetadata {
        name: name.to_string(),
        descriptor: "Ljava/lang/Object;".to_string(),
        access_flags: 0,
        slot_index: slot,
        declaring_class_id: sub,
        is_static: false,
    };
    c.set_declared_fields(sub, vec![field("this$0", 1), field("key", 2)]);
    c
}

/// t1 wave 6, lane Y3: the switch is read per VM, not latched process-wide
/// by the first VM to ask. In one process (and on one thread), VM A with the
/// switch on memoizes its complete row; VM B with it off answers the same
/// complete row from the table and never touches the memo, whichever VM
/// asks first, and A keeps its own setting after B asked.
#[test]
fn two_vms_in_one_process_keep_their_own_setting() {
    const VM_FAST: usize = 0x7953_0001;
    const VM_SLOW: usize = 0x7953_0002;
    std::thread::spawn(|| {
        let slow = vm_with_sub(VM_SLOW, false);
        let fast = vm_with_sub(VM_FAST, true);
        assert!(!clv_sub_fast_key_enabled(&slow));
        assert!(clv_sub_fast_key_enabled(&fast));

        // The slow VM asks first: under the old process latch its `false`
        // would have been every VM's answer.
        let s = sub_ids(&slow).expect("slow VM's complete row");
        assert!(s.class.is_some() && s.array_list.is_some());
        assert!(
            sub_ids_memo_get(VM_SLOW).is_none(),
            "the switched-off VM memoized its row"
        );

        let f = sub_ids(&fast).expect("fast VM's complete row");
        assert_eq!(
            sub_ids_memo_get(VM_FAST).map(|i| i.sub),
            Some(f.sub),
            "the switched-on VM did not memoize its row"
        );

        // The slow VM asking again leaves the fast VM's memo alone.
        assert!(sub_ids(&slow).is_some());
        assert!(sub_ids_memo_get(VM_SLOW).is_none());
        assert!(sub_ids_memo_get(VM_FAST).is_some());
        assert!(clv_sub_fast_key_enabled(&fast) && !clv_sub_fast_key_enabled(&slow));
    })
    .join()
    .unwrap();    }