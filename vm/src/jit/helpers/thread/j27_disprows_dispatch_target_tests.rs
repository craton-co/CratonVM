//! j27 wave 4 (lane DISPROWS, MEGA3-17-3): the one publication condition
//! of the MIC arms and the blind arm; SELECT17-5: the retarget itself.
use super::*;

fn target(cacheable_receiver: bool, selected_cid: u32) -> CachedDispatchTarget {
    CachedDispatchTarget {
        class_name: std::rc::Rc::from("p/Recv"),
        cacheable_receiver,
        globally_named: true,
        selection_error: false,
        selected_cid,
    }
}

#[test]
fn a_receiver_named_target_publishes_its_own_class() {
    let t = target(true, 0);
    assert!(t.publishable());
    assert_eq!(t.compile_cid(0x51), 0x51);
}

#[test]
fn a_retargeted_receiver_publishes_the_selected_class() {
    let t = target(false, 0x77);
    assert!(t.publishable());
    assert_eq!(t.compile_cid(0x51), 0x77);
}

#[test]
fn a_by_name_target_never_publishes() {
    let t = target(false, 0);
    assert!(!t.publishable());
    assert_eq!(t.compile_cid(0x51), 0x51);
}

/// A version-52 class `name extends sup` with one method `m()I`
/// (`iconst_1; ireturn`) of access `flags`. Shared with
/// `j27_t1ic_mega_bake_access_tests` (j27 wave 7, lane T1IC).
pub(super) fn one_method_class(name: &str, sup: &str, flags: u16) -> Vec<u8> {
    fn utf8(out: &mut Vec<u8>, s: &str) {
        out.push(1);
        out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(0).to_be_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
    b.extend_from_slice(&8u16.to_be_bytes()); // constant_pool_count
    utf8(&mut b, name); // #1
    b.extend_from_slice(&[7, 0, 1]); // #2 Class #1
    utf8(&mut b, sup); // #3
    b.extend_from_slice(&[7, 0, 3]); // #4 Class #3
    utf8(&mut b, "m"); // #5
    utf8(&mut b, "()I"); // #6
    utf8(&mut b, "Code"); // #7
    b.extend_from_slice(&[0x00, 0x21]); // ACC_PUBLIC | ACC_SUPER
    b.extend_from_slice(&[0, 2, 0, 4]); // this_class, super_class
    b.extend_from_slice(&[0, 0, 0, 0]); // interfaces, fields
    b.extend_from_slice(&[0, 1]); // methods_count
    b.extend_from_slice(&flags.to_be_bytes());
    b.extend_from_slice(&[0, 5, 0, 6, 0, 1]); // m ()I, one attribute
    let code = [0x04, 0xac]; // iconst_1; ireturn
    b.extend_from_slice(&[0, 7]); // "Code"
    b.extend_from_slice(&(12 + code.len() as u32).to_be_bytes());
    b.extend_from_slice(&[0, 1, 0, 1]); // max_stack 1, max_locals 1
    b.extend_from_slice(&(code.len() as u32).to_be_bytes());
    b.extend_from_slice(&code);
    b.extend_from_slice(&[0, 0, 0, 0]); // no handlers, no attributes
    b.extend_from_slice(&[0, 0]); // no class attributes
    b    }