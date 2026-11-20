use super::*;

/// gcj j2/ob2: `VM.stringtable`'s figures follow the table's texts;
/// literal sizes are HotSpot's `StringTable::literal_size` (a 24-byte
/// String plus its value array: 16-byte header, payload, rounded to 8).
#[test]
fn gcj_j2_ob2_stringtable_census_counts_the_live_texts() {
    let texts = ["b", "a", "\u{e9}t\u{e9}", "\u{4e2d}"];
    let census = stringtable_census(texts.iter().copied(), true);
    assert_eq!(census.entries, 4);
    // Latin-1: one byte per char ("\u{e9}t\u{e9}" is three); otherwise two
    // per UTF-16 unit. Every array here rounds to 24 bytes.
    assert_eq!(census.literal_bytes, 4 * (24 + 24));
    assert_eq!(census.items, vec!["a", "b", "\u{e9}t\u{e9}", "\u{4e2d}"]);
    // A 9-char Latin-1 text needs a 32-byte array.
    assert_eq!(hotspot_string_literal_size("123456789"), 24 + 32);
    let census = stringtable_census(texts.iter().copied(), false);
    assert_eq!(census.entries, 4);
    assert!(census.items.is_empty());
    let text = default_vm_stringtable_text(&census);
    assert!(text.contains("Number of entries       :         4"), "{text}");
    assert!(stat_line(&text, "Number of literals : 4 = 192 bytes, avg 48.000"), "{text}");
    assert!(stat_line(&text, "Maximum bucket size : 1"), "{text}");
}

/// `text` has a line whose words are `words` (column padding ignored).
fn stat_line(text: &str, words: &str) -> bool {
    text.lines()
        .any(|l| l.split_whitespace().collect::<Vec<_>>().join(" ") == words)
}

/// gcj-j3x2-jcmd-stringtable retirement: the bucket statistics are
/// `NumberSeq`'s over `String.hashCode() & 65535`. "Aa" and "BB" share a
/// hash (2112), so they share a bucket.
#[test]
fn stringtable_bucket_statistics_are_hotspots() {
    let census = stringtable_census(["Aa", "BB", "c"].iter().copied(), false);
    assert_eq!(census.max_bucket, 2);
    assert_eq!(census.bucket_sum_of_squares, 4 + 1);
    // A `-Xshare:off -XX:+UseSerialGC` HotSpot 25.0.3 run's figures, which
    // this census reproduces from that run's own `-verbose` dump: 2578
    // entries, 56 buckets holding two. The variance is
    // `sum_sq / n - avg^2`, not the average.
    let census = StringTableCensus {
        buckets: 0,
        entries: 2578,
        literal_bytes: 161_368,
        max_bucket: 2,
        bucket_sum_of_squares: 2690,
        items: Vec::new(),
    };
    let want = "StringTable statistics:\n\
                Number of buckets       :     65536 =    524288 bytes, each 8\n\
                Number of entries       :      2578 =     41248 bytes, each 16\n\
                Number of literals      :      2578 =    161368 bytes, avg  62.000\n\
                Total footprint         :           =    726904 bytes\n\
                Average bucket size     :     0.039\n\
                Variance of bucket size :     0.039\n\
                Std. dev. of bucket size:     0.199\n\
                Maximum bucket size     :         2\n";
    assert_eq!(default_vm_stringtable_text(&census), want);
    // The empty table (the kill's text).
    let text = default_vm_stringtable_text(&StringTableCensus::default());
    assert!(stat_line(&text, "Variance of bucket size : 0.000"), "{text}");
    assert!(stat_line(&text, "Maximum bucket size : 0"), "{text}");
}

/// gcj j1/st item 2: the Symbol table census against HotSpot 25.0.4's own
/// arithmetic (rows on the Linux host, `VM.symboltable -verbose` of a
/// `-Xshare:off` run): a symbol is 8 bytes plus its length less 2, in
/// whole words; "" and 1- or 2-byte symbols are one word.
#[test]
fn gcj_j1st_symboltable_census_is_hotspots() {
    let census = symboltable_census(["", "a", "ab", "abc", "(Ljava/lang/invoke/MethodHandle;)V"].iter().copied(), true);
    assert_eq!(census.entries, 5);
    assert_eq!(census.buckets, SYMBOLTABLE_BUCKETS);
    // 8, 8, 8, 16 (9 rounded), 40 (8 + 32).
    assert_eq!(census.literal_bytes, 8 + 8 + 8 + 16 + 40);
    let text = default_vm_symboltable_text(&census);
    assert!(text.starts_with("SymbolTable statistics:\n"), "{text}");
    assert!(stat_line(&text, "Number of buckets : 32768 = 262144 bytes, each 8"), "{text}");
    assert!(stat_line(&text, "Number of literals : 5 = 80 bytes, avg 16.000"), "{text}");
    // The bucket hash is over the bytes as signed `jbyte`s; "Aa" / "BB"
    // collide as in the String table.
    let census = symboltable_census(["Aa", "BB"].iter().copied(), false);
    assert_eq!(census.max_bucket, 2);
    let texts = ["\t".to_string(), "\u{e9}".to_string(), "\0".to_string()];
    assert_eq!(
        hotspot_symboltable_verbose_text(&texts),
        "VERSION: 1.1\n1 65535: \\t\n2 65535: \\xc3\\xa9\n2 65535: \\xc0\\x80\n"
    );
    // The empty table.
    let text = default_vm_symboltable_text(&StringTableCensus::default());
    assert!(stat_line(&text, "Number of buckets : 32768 = 262144 bytes, each 8"), "{text}");
    assert!(stat_line(&text, "Maximum bucket size : 0"), "{text}");
}

/// gcj j1/st item 2: dead entries not yet cleaned count as entries and in
/// their buckets, with no literal bytes (HotSpot's `-verbose` skips them),
/// and a grown table's bucket count is printed.
#[test]
fn gcj_j1st_stringtable_census_counts_uncleaned_entries() {
    let dead = [string_table_bucket_hash("Aa")];
    let census = stringtable_census_with(["BB"].iter().copied(), &dead, 131_072, true);
    assert_eq!(census.entries, 2);
    assert_eq!(census.literal_bytes, hotspot_string_literal_size("BB"));
    assert_eq!(census.max_bucket, 2);
    assert_eq!(census.items, vec!["BB"]);
    let text = default_vm_stringtable_text(&census);
    assert!(stat_line(&text, "Number of buckets : 131072 = 1048576 bytes, each 8"), "{text}");
    assert!(stat_line(&text, "Number of literals : 2 = 48 bytes, avg 24.000"), "{text}");
}

/// gcj j1/st: C's `%g`, as HotSpot's `stringtable` lines print the
/// factors and seconds (captures on the Linux host).
#[test]
fn gcj_j1st_c_format_g_is_printfs() {
    assert_eq!(c_format_g(302_136.0 / 65_536.0), "4.61023");
    assert_eq!(c_format_g(302_137.0 / 65_536.0), "4.61024");
    assert_eq!(c_format_g(225_001.0 / 65_536.0), "3.43324");
    assert_eq!(c_format_g(75_001.0 / 65_536.0), "1.14442");
    assert_eq!(c_format_g(0.0664483), "0.0664483");
    assert_eq!(c_format_g(2.0), "2");
    assert_eq!(c_format_g(0.0), "0");
    assert_eq!(c_format_g(0.00001), "1e-05");
    assert_eq!(c_format_g(1_234_567.0), "1.23457e+06");
    assert_eq!(c_format_g(999_999.5), "1e+06");
    assert_eq!(c_format_g(123_456.0), "123456");
}

/// gcj j2/ob2: the `-verbose` option as HotSpot's `BOOLEAN` argument.
#[test]
fn gcj_j2_ob2_dcmd_verbose_option() {
    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert!(!dcmd_verbose_option(&args(&[])));
    assert!(dcmd_verbose_option(&args(&["-verbose"])));
    assert!(dcmd_verbose_option(&args(&["-verbose=true"])));
    assert!(!dcmd_verbose_option(&args(&["-verbose", "-verbose=false"])));
}

/// HotSpot 25.0.3's `VM.flags -all` shape: the `[Global flags]` table.
#[test]
fn vm_flags_texts_are_hotspots() {
    assert_eq!(vm_flags_all_text(&["row".to_string()]), "[Global flags]\nrow\n");
    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert!(dcmd_boolean_option(&args(&["-all"]), "-all"));
    assert!(!dcmd_boolean_option(&args(&["-all", "-all=false"]), "-all"));
    assert!(!dcmd_boolean_option(&args(&["-allx"]), "-all"));
}


/// gcj j2/ob2 rows on j2a (`ob2_cap_st_verbose`): HotSpot's `-verbose`
/// dump, modified-UTF-8 lengths and its escapes.
#[test]
fn gcj_j2_ob2_stringtable_verbose_text_is_hotspots_dump() {
    let texts: Vec<String> = ["message", "\u{1}\u{1}", "\r\n", "\t", "a\\\"b", "\u{402}", "\0"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let text = hotspot_stringtable_verbose_text(&texts);
    let want = "VERSION: 1.1\n7: message\n2: \\x01\\x01\n2: \\r\\n\n1: \\t\n4: a\\\\\"b\n\
                2: \\xd0\\x82\n2: \\xc0\\x80\n";
    assert_eq!(text, want);
}

/// gcj j2/ob2 rows on j2a: the statistics' literal average is an integer
/// division (`1672 bytes, avg  47.000` for 35 literals).
#[test]
fn gcj_j2_ob2_stringtable_average_is_an_integer_division() {
    let census = StringTableCensus {
        entries: 35,
        literal_bytes: 1672,
        ..StringTableCensus::default()
    };
    let text = default_vm_stringtable_text(&census);
    assert!(
        text.contains("Number of literals      :        35 =      1672 bytes, avg  47.000"),
        "{text}"
    );
}

/// gcj j2/ob2 rows on j2a (`ob2_cap_vmuptime`, `ob2_cap_vmcmd`):
/// HotSpot's `VM.uptime` and `VM.command_line` shapes.
#[test]
fn gcj_j2_ob2_vm_uptime_and_command_line_texts() {
    assert_eq!(hotspot_vm_uptime_text(0.2104), "0.210 s\n");
    let args = vec!["-XX:+UseSerialGC".to_string(), "-Xmx256m".to_string()];
    assert_eq!(
        hotspot_vm_command_line_text(&args, Some("Main a"), "C:/cp", "SUN_STANDARD"),
        "VM Arguments:\njvm_args: -XX:+UseSerialGC -Xmx256m \njava_command: Main a\n\
         java_class_path (initial): C:/cp\nLauncher Type: SUN_STANDARD\n"
    );
    assert_eq!(
        hotspot_vm_command_line_text(&[], None, "", "SUN_STANDARD"),
        "VM Arguments:\njava_command: <unknown>\njava_class_path (initial): <not set>\n\
         Launcher Type: SUN_STANDARD\n"
    );
    assert!(gen_jcmd_hotspot_texts());
}
