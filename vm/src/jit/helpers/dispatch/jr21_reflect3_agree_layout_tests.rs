//! JIT round 21 lane reflect3 (`CRATONVM_JIT_MH_COMPILED_LINK_AGREE_MEMO`):
//! the memo's layout key tells the three shapes' calls apart.
use super::*;

#[test]
fn each_shape_layout_has_its_own_key() {
    let linker = jr21_reflect3_agree_layout(1, 0, 0, 1);
    let basic = jr21_reflect3_agree_layout(1, 0, 0, 0);
    let link_to_static = jr21_reflect3_agree_layout(0, 0, 1, 0);
    let link_to_virtual = jr21_reflect3_agree_layout(0, 1, 1, 0);
    let keys = [linker, basic, link_to_static, link_to_virtual];
    assert!(keys.iter().all(Option::is_some));
    for (i, a) in keys.iter().enumerate() {
        for b in &keys[i + 1..] {
            assert_ne!(a, b);
        }
    }
    // A count wider than two bits is not memoised.
    assert_eq!(jr21_reflect3_agree_layout(4, 0, 0, 0), None);
    assert_eq!(jr21_reflect3_agree_layout(3, 3, 3, 3), Some(0xFF));
}
