//! zt t27/zc10: the table by hand, then on a real heap (the mark's answer
//! for a member, a non-member of a member class, and an object of another
//! class; the kill arm; an unregistered member switching it off).

use super::skip_class_filter::*;
use cratonvm_gc_core::collector::GarbageCollector;
use crate::zgc::ZgcRealHeap;
use cratonvm_types::ClassId;


/// By hand: class ids 7 and 4 103 share bit 7 (4 103 = 4 096 + 7), so a
/// fill with member classes {7, 900} answers yes for 7, 900 and 4 103 (the
/// collision, a false positive) and no for 8 and 901. An unclassifiable
/// member switches the table off (every answer yes). A refresh adds class
/// 8 and keeps 7 and 900; a fresh fill with {8} drops them. The kill arm
/// never makes the table usable.
#[test]
fn zt27zc10_the_table_by_hand() {
    assert_eq!(ZSkipClassFilter::bit_of(7), (0, 1 << 7));
    assert_eq!(ZSkipClassFilter::bit_of(4_103), (0, 1 << 7));
    assert_eq!(ZSkipClassFilter::bit_of(900), (14, 1 << 4)); // 900 = 14*64 + 4
    let class = |a: usize| -> Option<u32> {
        match a {
            0x100 => Some(7),
            0x200 => Some(900),
            0x300 => Some(8),
            _ => None,
        }
    };
    let f = ZSkipClassFilter::new(true);
    assert!(f.may_contain(12345), "control: not usable before a fill");
    f.fill([0x100usize, 0x200, 0].into_iter(), false, class);
    assert!(f.usable());
    assert!(f.may_contain(7) && f.may_contain(900) && f.may_contain(4_103));
    assert!(!f.may_contain(8) && !f.may_contain(901));
    let r = f.report();
    assert_eq!((r.fills, r.off, r.members, r.classes), (1, 0, 2, 2));
    // A refresh only adds.
    f.fill([0x300usize].into_iter(), true, class);
    assert!(f.may_contain(7) && f.may_contain(900) && f.may_contain(8));
    // A fresh fill clears first.
    f.fill([0x300usize].into_iter(), false, class);
    assert!(f.may_contain(8) && !f.may_contain(7));
    // An unclassifiable member: off for the cycle, and a refresh keeps it off.
    f.fill([0x300usize, 0x999].into_iter(), false, class);
    assert!(!f.usable() && f.may_contain(12345));
    f.fill([0x300usize].into_iter(), true, class);
    assert!(!f.usable(), "a refresh never switches an off table back on");
    assert_eq!(f.report().off, 2);
    // The kill arm.
    let k = ZSkipClassFilter::new(false);
    k.fill([0x100usize].into_iter(), false, class);
    assert!(!k.usable() && k.may_contain(8));
    assert_eq!(k.report().fills, 0);
}

/// On a real heap: a `Reference`-shaped member (class 62), another object
/// of the same class that is NOT a member, and an object of class 63. With
/// the table: the member keeps its hole (`Some(0)`), the other two trace
/// their slot 0 (`None`), and the class-63 object is answered by the table
/// alone. Kill arm: the same three answers through the Bloom path. And a
/// stale (unregistered) member switches the table off without changing an
/// answer.
#[test]
fn zt27zc10_the_mark_answers_the_same_with_and_without_the_table() {
    for on in [true, false] {
        let heap = ZgcRealHeap::with_capacity(16 * 1024 * 1024);
        heap.counters.skipcls.set_enabled_for_test(on);
        // zt t31/zc11: the table fronts the hash-set path only.
        heap.counters.skipbits.set_enabled_for_test(false);
        let member = heap.alloc_object(ClassId::new(62), 2).as_ptr() as usize;
        let same_class = heap.alloc_object(ClassId::new(62), 2).as_ptr() as usize;
        let other = heap.alloc_object(ClassId::new(63), 2).as_ptr() as usize;
        heap.set_stw_vm_reference_skip(vec![member]);
        let _snap = heap.begin_concurrent_mark_cycle();
        assert_eq!(heap.counters.skipcls.usable(), on, "on={on}");
        assert_eq!(heap.mark_skip_index(member), Some(0), "on={on}: the member");
        assert_eq!(heap.mark_skip_index(same_class), None, "on={on}");
        assert_eq!(heap.mark_skip_index(other), None, "on={on}");
        assert_eq!(heap.mark_skip_index_classed(other, Some(63)), None);
        if on {
            assert!(
                !heap.counters.skipcls.may_contain(63),
                "answered by the table"
            );
            assert!(heap.counters.skipcls.may_contain(62));
        }
        heap.end_concurrent_mark_cycle();
        heap.clear_stw_vm_reference_skip();

        // A stale member: off for the cycle, the answers unchanged.
        heap.set_stw_vm_reference_skip(vec![member, 0x8]);
        let _snap = heap.begin_concurrent_mark_cycle();
        assert!(!heap.counters.skipcls.usable());
        assert_eq!(heap.mark_skip_index(member), Some(0));
        assert_eq!(heap.mark_skip_index(other), None);
        heap.end_concurrent_mark_cycle();
        heap.clear_stw_vm_reference_skip();
    }
}
