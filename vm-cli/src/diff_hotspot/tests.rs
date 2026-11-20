    use super::*;

    #[test]
    fn a_trailing_newline_is_not_a_divergence() {
        assert_eq!(lines_of("a\nb\n"), lines_of("a\nb"));
        assert_eq!(lines_of("a\r\nb\r\n"), lines_of("a\nb"));
    }

    #[test]
    fn identity_hashes_are_masked_but_short_at_suffixes_are_not() {
        assert_eq!(
            mask_identity_hash("java.lang.Object@1b6d3586 end"),
            "java.lang.Object@<idhash> end"
        );
        assert_eq!(mask_identity_hash("user@ab"), "user@ab");
        assert_eq!(mask_identity_hash("@1b6d3586"), "@1b6d3586");
    }

    #[test]
    fn addresses_and_thread_ordinals_are_masked() {
        assert_eq!(mask_hex_address("at 0xdeadbeef!"), "at 0x<addr>!");
        assert_eq!(
            mask_thread_id("pool-1-thread-13 ran"),
            "pool-<n>-thread-<n> ran"
        );
    }

    #[test]
    fn timestamps_and_absolute_paths_are_masked() {
        assert_eq!(mask_timestamp("2026-09-01T12:34:56.789Z"), "<date>T<time>Z");
        assert_eq!(
            mask_abs_path("read /home/me/x/Foo.txt ok"),
            "read Foo.txt ok"
        );
        assert_eq!(mask_abs_path("ratio 3/4 ok"), "ratio 3/4 ok");
    }

    /// The bug this decode replaced: `String::from_utf8_lossy` turned
    /// HotSpot's *own correct* cp1252/ISO-8859-1 `é` — the single byte
    /// `0xE9` — into U+FFFD, and the harness then reported a divergence
    /// against a line HotSpot never wrote. Nothing is lost now, and the
    /// mapping is injective, which is what makes the byte-exact verdict
    /// byte-exact.
    #[test]
    fn a_non_utf8_reference_byte_survives_the_decode() {
        assert_eq!(for_display(&decode_lossless(b"h\xE9llo")), "h\\xE9llo");
        // Injective: two different byte streams cannot decode alike.
        assert_ne!(decode_lossless(b"h\xE9llo"), decode_lossless(b"h\xEAllo"));
        // Valid UTF-8 is untouched, so the common path is unchanged.
        assert_eq!(decode_lossless("héllo".as_bytes()), "héllo");
        // A truncated sequence escapes byte by byte and still terminates.
        assert_eq!(for_display(&decode_lossless(b"a\xEF\xB7b")), "a\\xEF\\xB7b");
    }

    /// The sentinel is escaped as its own three bytes, or a program that
    /// printed U+FDD0 would be indistinguishable from a raw byte.
    #[test]
    fn the_escape_sentinel_escapes_itself() {
        let decoded = decode_lossless("a\u{FDD0}b".as_bytes());
        assert_eq!(for_display(&decoded), "a\\xEF\\xB7\\x90b");
        assert_ne!(decoded, "a\u{FDD0}b");
    }

    /// A hint, and only where it belongs: the encoding shape is recognised
    /// on the witness from the known-issue page (both the `?`-substituting
    /// and the raw-byte spellings of the reference side), and an ordinary
    /// ASCII difference does not trip it.
    #[test]
    fn an_encoding_shaped_divergence_is_recognised_and_an_ascii_one_is_not() {
        assert!(differs_only_outside_ascii(
            "hello, é中😀 world",
            "hello, ??? world"
        ));
        // cp1252: `é` reaches us as one raw byte, the rest substitute.
        assert!(differs_only_outside_ascii(
            "hello, é中😀 world",
            &decode_lossless(b"hello, \xE9?? world")
        ));
        // A numeric divergence is not encoding, and neither is a `?` count.
        assert!(!differs_only_outside_ascii("0.3", "0.30000000000000004"));
        assert!(!differs_only_outside_ascii("x?y", "x??y"));
        assert!(!differs_only_outside_ascii("same", "same"));
    }

    #[test]
    fn ignore_patterns_are_substrings_with_a_star_wildcard() {
        assert!(pattern_matches("elapsed", "total elapsed 5ms"));
        assert!(pattern_matches("took*ms", "it took 5 ms"));
        assert!(!pattern_matches("took*ns", "it took 5 ms"));
    }

    #[test]
    fn a_line_that_moves_between_two_cratonvm_runs_is_unstable() {
        let a = vec!["x".to_string(), "1".to_string()];
        let b = vec!["x".to_string(), "2".to_string()];
        assert_eq!(instability(&[a, b]), vec![false, true]);
    }

    /// The whole point of the dedicated scan: a `--diff-hotspot` in the
    /// program's own argument tail belongs to the program, and the flags
    /// this mode owns never reach the launcher's argv pipeline.
    #[test]
    fn the_scan_leaves_a_normal_launch_alone_and_strips_only_its_own_flags() {
        let normal: Vec<String> = ["cratonvm", "-cp", "b", "Main", "--diff-hotspot"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        assert!(scan(&normal).is_none());

        let asked: Vec<String> = [
            "cratonvm",
            "--diff-hotspot",
            "--diff-ignore",
            "noise",
            "-cp",
            "b",
            "Main",
            "--diff-hotspot",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        let req = scan(&asked).expect("requested").expect("parsed");
        assert_eq!(req.ignores, vec!["noise".to_string()]);
        assert_eq!(req.runs, 2);
        assert_eq!(
            req.child_argv,
            vec![
                "cratonvm".to_string(),
                "-cp".to_string(),
                "b".to_string(),
                "Main".to_string(),
                "--diff-hotspot".to_string(),
            ]
        );
    }

    /// `--opt=value` and `--opt value` both work, and a `--jar` selector
    /// ends the launcher section just as it does for the real parse.
    #[test]
    fn inline_values_and_the_jar_selector() {
        let asked: Vec<String> = [
            "cratonvm",
            "--diff-hotspot",
            "--diff-runs=1",
            "--diff-strict",
            "--jar",
            "app.jar",
            "--diff-ignore",
            "mine",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        let req = scan(&asked).expect("requested").expect("parsed");
        assert_eq!(req.runs, 1);
        assert!(req.strict);
        // The `--diff-ignore` after the jar is the program's argument.
        assert!(req.ignores.is_empty());
        assert_eq!(
            req.child_argv,
            vec![
                "cratonvm".to_string(),
                "--jar".to_string(),
                "app.jar".to_string(),
                "--diff-ignore".to_string(),
                "mine".to_string(),
            ]
        );
    }
