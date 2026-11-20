use super::*;


/// The claim scope is thread-local, rests off and is lowered on every
/// path out, like `NoRootsPause`.
#[test]
fn the_no_roots_claim_scope_is_scoped() {
    assert!(!no_roots_lazy_claim_active());
    {
        let _g = NoRootsLazyClaim::enter();
        assert!(no_roots_lazy_claim_active());
        let other = std::thread::spawn(no_roots_lazy_claim_active)
            .join()
            .unwrap();
        assert!(!other, "another thread is unaffected");        }
    assert!(!no_roots_lazy_claim_active());    }