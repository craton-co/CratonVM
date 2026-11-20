use super::*;

const U: &str = "jdk/internal/misc/Unsafe";

/// Every nio21 wave-3 row (lane L4) is recognised with the shape its
/// helper's signature row declares.
#[test]
fn nio21_rows_are_recognised_with_their_shapes() {
    let rows: [(&str, &str, &str, UnsafeAccessorOp, usize, u8); 9] = [
        (
            U,
            "getIntVolatile",
            "(Ljava/lang/Object;J)I",
            UnsafeAccessorOp::GetIntVolatile,
            2,
            b'I',
        ),
        (
            U,
            "getLongVolatile",
            "(Ljava/lang/Object;J)J",
            UnsafeAccessorOp::GetLongVolatile,
            2,
            b'J',
        ),
        (
            U,
            "putIntVolatile",
            "(Ljava/lang/Object;JI)V",
            UnsafeAccessorOp::PutIntVolatile,
            3,
            b'V',
        ),
        (
            U,
            "putLongVolatile",
            "(Ljava/lang/Object;JJ)V",
            UnsafeAccessorOp::PutLongVolatile,
            3,
            b'V',
        ),
        (
            U,
            "putReferenceVolatile",
            "(Ljava/lang/Object;JLjava/lang/Object;)V",
            UnsafeAccessorOp::PutReferenceVolatile,
            3,
            b'V',
        ),
        (
            U,
            "compareAndSetReference",
            "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
            UnsafeAccessorOp::CasReference,
            4,
            b'Z',
        ),
        (U, "fullFence", "()V", UnsafeAccessorOp::FullFence, 0, b'V'),
        (
            "java/lang/ref/Reference",
            "refersTo0",
            "(Ljava/lang/Object;)Z",
            UnsafeAccessorOp::RefersTo0,
            1,
            b'Z',
        ),
        (
            "java/lang/ref/PhantomReference",
            "refersTo0",
            "(Ljava/lang/Object;)Z",
            UnsafeAccessorOp::PhantomRefersTo0,
            1,
            b'Z',
        ),
    ];
    for (class, name, desc, op, params, ret) in rows {
        assert_eq!(
            is_unsafe_accessor_op(class, name, desc),
            Some(op),
            "{class}.{name}{desc}"
        );
        assert_eq!(op.num_params(), params, "{op:?}");
        assert_eq!(op.return_type(), ret, "{op:?}");
        assert!(op.is_nio21_row(), "{op:?}");
        assert_eq!(op.is_cas(), op == UnsafeAccessorOp::CasReference, "{op:?}");
        assert_eq!(
            op.is_refers_to(),
            matches!(
                op,
                UnsafeAccessorOp::RefersTo0 | UnsafeAccessorOp::PhantomRefersTo0
            ),
            "{op:?}"
        );
    }
}

/// Each row reads its OWN table slot: a declined `PhantomReference` call
/// must not dispatch as `Reference`'s, nor a `putLongVolatile` as a get.
#[test]
fn every_nio21_row_names_its_own_table_slot() {
    let table = DirectHelperTable {
        unsafe_get_int_volatile: 0x10,
        unsafe_get_long_volatile: 0x18,
        unsafe_put_int_volatile: 0x20,
        unsafe_put_long_volatile: 0x28,
        unsafe_put_reference_volatile: 0x30,
        unsafe_cas_reference: 0x38,
        unsafe_full_fence: 0x40,
        reference_refers_to0: 0x48,
        phantom_reference_refers_to0: 0x50,
        ..DirectHelperTable::EMPTY
    };
    let ops = [
        UnsafeAccessorOp::GetIntVolatile,
        UnsafeAccessorOp::GetLongVolatile,
        UnsafeAccessorOp::PutIntVolatile,
        UnsafeAccessorOp::PutLongVolatile,
        UnsafeAccessorOp::PutReferenceVolatile,
        UnsafeAccessorOp::CasReference,
        UnsafeAccessorOp::FullFence,
        UnsafeAccessorOp::RefersTo0,
        UnsafeAccessorOp::PhantomRefersTo0,
    ];
    let mut seen: Vec<usize> = ops.iter().map(|op| op.helper_entry(&table)).collect();
    assert!(seen.iter().all(|&e| e != 0));
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen.len(), ops.len());
    for op in ops {
        assert_eq!(
            op.helper_entry(&DirectHelperTable::EMPTY),
            0,
            "{op:?} is wired in EMPTY"
        );
    }
}

/// The Java-bodied neighbours stay unrecognised: binding one would shadow
/// real class bytes.
#[test]
fn java_bodied_neighbours_are_not_recognised() {
    for (class, name, desc) in [
        (
            U,
            "putReferenceRelease",
            "(Ljava/lang/Object;JLjava/lang/Object;)V",
        ),
        (
            U,
            "putReferenceOpaque",
            "(Ljava/lang/Object;JLjava/lang/Object;)V",
        ),
        (U, "getLongAcquire", "(Ljava/lang/Object;J)J"),
        (U, "putLongRelease", "(Ljava/lang/Object;JJ)V"),
        (U, "getAndAddLong", "(Ljava/lang/Object;JJ)J"),
        (U, "weakCompareAndSetLong", "(Ljava/lang/Object;JJJ)Z"),
        (U, "loadFence", "()V"),
        (
            "sun/misc/Unsafe",
            "getLongVolatile",
            "(Ljava/lang/Object;J)J",
        ),
        (
            "java/lang/ref/Reference",
            "refersTo",
            "(Ljava/lang/Object;)Z",
        ),
        (
            "java/lang/ref/WeakReference",
            "refersTo0",
            "(Ljava/lang/Object;)Z",
        ),
        (
            "java/lang/ref/Reference",
            "refersTo0",
            "(Ljava/lang/Object;)I",
        ),
    ] {
        assert_eq!(
            is_unsafe_accessor_op(class, name, desc),
            None,
            "{class}.{name}{desc}"
        );
    }
}
