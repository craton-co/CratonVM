use super::{classify, tally, Tallies, ROW_CAP};

#[test]
fn i64_l5_ended_counts_one_per_finished_call_and_the_row_cap_holds() {
    let mut t = Tallies::default();
    let key = ("zipfile-entry", "App.main".to_string());
    tally(&mut t, key.clone(), 100, false);
    tally(&mut t, key.clone(), 28, true);
    tally(&mut t, ("direct", "App.main".to_string()), 5, true);
    assert_eq!((t.total.calls, t.total.bytes, t.total.ended), (3, 133, 2));
    let row = t.rows[&key];
    assert_eq!((row.calls, row.bytes, row.ended), (2, 128, 1));
    for i in 0..ROW_CAP + 3 {
        tally(&mut t, ("direct", format!("C{i}.m")), 1, false);
    }
    assert_eq!(t.rows.len(), ROW_CAP);
    assert!(t.dropped >= 3);
    // Dropped rows still reach the whole-run total.
    assert_eq!(t.total.calls, 3 + (ROW_CAP as u64 + 3));
}

#[test]
fn i116_a_class_read_and_a_resource_stream_are_told_apart() {
    // The real shape: ZipFileInflaterInputStream does not override `read`,
    // so only `InflaterInputStream.read` is on the stack.
    let class_read = [
        ("java/util/zip/Inflater", "inflate"),
        ("java/util/zip/InflaterInputStream", "read"),
        ("java/io/InputStream", "readNBytes"),
        ("jdk/internal/loader/Resource", "getBytes"),
        ("jdk/internal/loader/BuiltinClassLoader", "defineClass"),
    ];
    assert_eq!(
        classify(class_read.iter().copied()),
        ("inflater-stream", "Resource.getBytes".to_string())
    );
    let properties = [
        ("java/util/zip/Inflater", "inflate"),
        ("java/util/zip/InflaterInputStream", "read"),
        ("sun/nio/cs/StreamDecoder", "readBytes"),
        ("java/io/InputStreamReader", "read"),
        ("java/util/Properties$LineReader", "readLine"),
        ("java/util/Properties", "load0"),
    ];
    assert_eq!(
        classify(properties.iter().copied()),
        ("inflater-stream", "java/util/Properties$LineReader.readLine".to_string())
    );
    // A JDK whose ZipFileInflaterInputStream did override `read` is still told apart.
    let overriding = [
        ("java/util/zip/Inflater", "inflate"),
        ("java/util/zip/InflaterInputStream", "read"),
        ("java/util/zip/ZipFile$ZipFileInflaterInputStream", "read"),
        ("App", "main"),
    ];
    assert_eq!(
        classify(overriding.iter().copied()),
        ("zipfile-entry", "App.main".to_string())
    );
    let gzip = [
        ("java/util/zip/Inflater", "inflate"),
        ("java/util/zip/InflaterInputStream", "read"),
        ("java/util/zip/GZIPInputStream", "read"),
        ("App", "main"),
    ];
    assert_eq!(classify(gzip.iter().copied()), ("gzip", "App.main".to_string()));
    let direct = [("java/util/zip/Inflater", "inflate"), ("App", "main")];
    assert_eq!(classify(direct.iter().copied()), ("direct", "App.main".to_string()));
}
