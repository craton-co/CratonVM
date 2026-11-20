    use super::*;

    /// i1 wave 7, lane L4: the exit summary's columns partition the
    /// loose roots the way the page's decision reads them.
    #[test]
    fn census_columns_split_minted_object_start_and_interior() {
        let g = |c: &AtomicUsize| c.load(Ordering::Relaxed);
        let before = [
            g(&N),
            g(&UNMINTED),
            g(&UNMINTED_OBJECT_START),
            g(&UNMINTED_PRIMITIVE_KIND),
            g(&INTERIOR),
        ];
        count(true, true, true); // a minted handle: none of the columns
        count(false, true, false); // mintable unregistered producer
        count(false, false, true); // interior word from a `long` push
        let after = [
            g(&N),
            g(&UNMINTED),
            g(&UNMINTED_OBJECT_START),
            g(&UNMINTED_PRIMITIVE_KIND),
            g(&INTERIOR),
        ];
        // Other tests may count concurrently, so lower bounds only.
        assert!(after[0] >= before[0] + 3);
        assert!(after[1] >= before[1] + 2);
        assert!(after[2] >= before[2] + 1);
        assert!(after[3] >= before[3] + 1);
        assert!(after[4] >= before[4] + 1);
    }
