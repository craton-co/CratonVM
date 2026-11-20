    use super::ir_ic_cascade_admits_kind;

    #[test]
    fn a_bound_site_takes_its_cache_and_a_static_row_never_does() {
        assert!(ir_ic_cascade_admits_kind(0));
        assert!(ir_ic_cascade_admits_kind(1), "bound-site cache (kind 1)");
        assert!(ir_ic_cascade_admits_kind(2));
        assert!(!ir_ic_cascade_admits_kind(3), "a folded handle's static box call");
        assert!(!ir_ic_cascade_admits_kind(4));
    }
