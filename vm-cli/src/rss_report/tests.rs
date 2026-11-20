    use super::*;

    const SMAPS: &str = "\
55d0a0000000-55d0a2000000 r--p 00000000 08:01 123 /opt/cvm/cratonvm
Size:              32768 kB
Rss:                8000 kB
Private_Dirty:         0 kB
AnonHugePages:         0 kB
55d0a2000000-55d0a6000000 r-xp 02000000 08:01 123 /opt/cvm/cratonvm
Rss:               30000 kB
55d0a6000000-55d0a6400000 rw-p 06000000 08:01 123 /opt/cvm/cratonvm
Rss:                1000 kB
Private_Dirty:      1000 kB
20000000000-20040000000 rw-p 00000000 00:00 0 [anon:mimalloc]
Rss:              141312 kB
Private_Dirty:    141312 kB
AnonHugePages:    141312 kB
VmFlags: rd wr mr mw me ac hg
7f0000000000-7f0000001000 ---p 00000000 00:00 0
Rss:                   0 kB
7f0000001000-7f0000201000 rw-p 00000000 00:00 0
Rss:                  40 kB
7f1000000000-7f1000010000 rwxp 00000000 00:00 0
Rss:                  64 kB
7f2000000000-7f2009000000 r--s 00000000 08:01 77 /usr/lib/jvm/jdk-25/lib/modules
Rss:                9900 kB
7f3000000000-7f3000100000 rw-p 00000000 00:00 0
Rss:                 300 kB
7ffd00000000-7ffd00021000 rw-p 00000000 00:00 0 [stack]
Rss:                 100 kB
";

    #[test]
    fn zs6rs_smaps_classes_follow_the_script() {
        let maps = parse_smaps(SMAPS);
        assert_eq!(maps.len(), 10);
        assert_eq!(maps[3].rss_kb, 141_312);
        assert_eq!(maps[3].anon_huge_kb, 141_312);
        assert_eq!(maps[3].lo, 0x200_0000_0000);
        let exe = "/opt/cvm/cratonvm";
        let classes: Vec<String> = (0..maps.len()).map(|i| classify(&maps, i, exe)).collect();
        let want = [
            "exe:rodata",
            "exe:text",
            "exe:data",
            "alloc:mimalloc",
            "anon:reserved",
            "anon:thread-stack",
            "anon:exec(jit-code)",
            "jdk:jimage",
            "anon:other",
            "stack:main",
        ];
        assert_eq!(classes, want);
        // Without the exe path the binary's pages are plain files.
        assert_eq!(classify(&maps, 1, ""), "files:other");
    }

    #[test]
    fn zs6rs_rollup_and_classes_sum_rss() {
        let status = "Name:\tcratonvm\nThreads:\t13\nVmHWM:\t  218848 kB\nVmRSS:\t  200000 kB\nRssAnon:\t  150000 kB\nRssFile:\t   50000 kB\n";
        let lines = render(status, SMAPS, "/opt/cvm/cratonvm", 3);
        assert!(lines[0].starts_with("[cratonvm] rss-rollup: vmrss_kb=200000 vmhwm_kb=218848"));
        assert!(lines[0].contains("threads=13"));
        assert!(lines[0].contains("anon_huge_kb=141312"));
        assert_eq!(
            lines[1],
            "[cratonvm] rss-class: class=alloc:mimalloc maps=1 rss_kb=141312 \
             anon_huge_kb=141312 priv_dirty_kb=141312"
        );
        assert!(lines
            .iter()
            .any(|l| l.contains("class=exe:text maps=1 rss_kb=30000")));
        assert!(lines
            .iter()
            .any(|l| l.contains("class=jdk:jimage maps=1 rss_kb=9900")));
        let tops: Vec<_> = lines.iter().filter(|l| l.contains("rss-top")).collect();
        assert_eq!(tops.len(), 3);
        assert!(tops[0].contains("rss_kb=141312 size_kb=1048576"));
        assert!(tops[0].contains("name=[anon:mimalloc]"));
    }

    #[test]
    fn zs6rs_status_keys_and_missing_keys() {
        assert_eq!(status_kb("VmHWM:\t  12 kB\n", "VmHWM"), 12);
        assert_eq!(status_kb("VmHWM:\t  12 kB\n", "VmRSS"), -1);
        // A key that is a prefix of another is not confused with it.
        assert_eq!(status_kb("VmRSSx:\t 5 kB\nVmRSS:\t 7 kB\n", "VmRSS"), 7);
    }
