// `super::` here is the enclosing `invoke` module, which is where this
// helper now lives.
use super::allocate_dynamic_dispatch_slots;

#[test]
fn caches_virtual_and_interface_sites_but_not_static_or_special_sites() {
    for (kind, expected) in [(0, 1), (1, 0), (2, 1), (3, 0)] {
        let mut mic = Vec::new();
        let mut owned_mic = Vec::new();
        let mut pic = Vec::new();
        let mut owned_pic = Vec::new();
        allocate_dynamic_dispatch_slots(
            kind,
            27,
            &mut mic,
            &mut owned_mic,
            &mut pic,
            &mut owned_pic,
        );
        assert_eq!(mic.len(), expected);
        assert_eq!(owned_mic.len(), expected);
        assert_eq!(pic.len(), expected);
        assert_eq!(owned_pic.len(), expected);
        if expected == 1 {
            assert_eq!(mic[0].0, 27);
            assert_eq!(pic[0].0, 27);
        }
    }
}
