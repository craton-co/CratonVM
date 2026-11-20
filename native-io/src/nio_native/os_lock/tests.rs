    use super::range_len;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    #[test]
    fn whole_file_size_zero_locks_max_range() {
        assert_eq!(range_len(0, 0), u64::MAX);
        assert_eq!(range_len(10, 0), u64::MAX - 10);
    }

    #[test]
    fn whole_file_long_max_size_does_not_overflow() {
        // JDK passes Long.MAX_VALUE for a whole-file lock.
        let r = range_len(100, i64::MAX);
        assert!(r <= u64::MAX - 100);
        assert_eq!(r, u64::MAX - 100);
    }

    #[test]
    fn bounded_range_is_preserved() {
        assert_eq!(range_len(5, 20), 20);
    }

    #[test]
    fn negative_size_treated_as_whole_file() {
        assert_eq!(range_len(0, -1), u64::MAX);
    }
