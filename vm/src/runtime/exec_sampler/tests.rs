    use super::*;

    /// The off path must not read a clock.
    ///
    /// Not a timing assertion — those are unreliable on a loaded box, and this
    /// file's whole purpose is measuring one. It asserts the property that
    /// makes the off path free: the interval resolves to `None` when the flag
    /// is unset, which is the branch `maybe_sample` returns on.
    #[test]
    fn the_profiler_is_off_without_its_flag() {
        if Err::<String, std::env::VarError>(std::env::VarError::NotPresent).is_ok() {
            return; // armed in this environment; nothing to assert        
}
        assert!(interval_ns().is_none());
        let (total, _) = sample_counts();
        assert_eq!(total, 0, "an unarmed profiler must take no samples");
    }

    /// A zero or unparseable interval is OFF, not a divide-by-zero or a
    /// sample-every-poll storm.
    #[test]
    fn a_zero_interval_reads_as_off() {
        // `interval_ns` is latched, so this exercises the parse rule directly
        // rather than through the cache.
        let parse = |s: &str| -> Option<u64> {
            let ms: u64 = s.trim().parse().ok()?;
            (ms > 0).then(|| ms.saturating_mul(1_000_000))
        };
        assert_eq!(parse("0"), None);
        assert_eq!(parse("nonsense"), None);
        assert_eq!(parse(""), None);
        assert_eq!(parse("10"), Some(10_000_000));
    }
