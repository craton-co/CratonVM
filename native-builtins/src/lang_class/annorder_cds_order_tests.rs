use super::*;

/// One 8-aligned archived `Symbol` record: permanent refcount (`FF FF`),
/// an arbitrary hash, the length, the body.
fn put_symbol(buf: &mut Vec<u8>, at: usize, name: &str, refcount: [u8; 2]) {
    if buf.len() < at + 6 + name.len() {
        buf.resize(at + 6 + name.len() + 8, 0);
    }
    buf[at..at + 2].copy_from_slice(&refcount);
    buf[at + 2..at + 4].copy_from_slice(&[0x5a, 0xa5]);
    buf[at + 4..at + 6].copy_from_slice(&(name.len() as u16).to_le_bytes());
    buf[at + 6..at + 6 + name.len()].copy_from_slice(name.as_bytes());
}

/// The offsets `classes.jsa` of Microsoft OpenJDK 25.0.3.9 gives the
/// member names of the two round-16 probes (`ratio` is not archived).
fn jdk25_index() -> CdsSymbolIndex {
    let mut offsets: FxHashMap<u64, u32> = FxHashMap::default();
    for (n, off) in [
        ("name", 0x4d3ae8u32),
        ("priority", 0x4d3af8),
        ("value", 0x4d3fa0),
        ("s", 0x4dd8e8),
        ("n", 0x4ddce0),
        ("enabled", 0x4e7b80),
        ("q", 0x4eb290),
        ("groups", 0x4ed988),
        ("k", 0x4edc80),
        ("timeout", 0x4ee428),
        ("tags", 0x520f20),
        ("description", 0x6202a8),
    ] {
        offsets.insert(CdsSymbolIndex::hash_name(n.as_bytes()), off);
    }
    CdsSymbolIndex { offsets }
}

fn hotspot_order(index: &CdsSymbolIndex, names: &[&str]) -> Vec<String> {
    hotspot_declared_method_order(index, names)
        .into_iter()
        .map(|i| names[i].to_string())
        .collect()
}

#[test]
fn archive_scan_keeps_permanent_identifier_symbols_only() {
    let mut buf = Vec::new();
    put_symbol(&mut buf, 16, "timeout", [0xFF, 0xFF]);
    put_symbol(&mut buf, 40, "enabled", [0xFF, 0xFF]);
    // Not permanent: a run-time symbol image, or unrelated data.
    put_symbol(&mut buf, 64, "ratio", [0x01, 0x00]);
    // Not 8-aligned: never a symbol.
    put_symbol(&mut buf, 92, "tags", [0xFF, 0xFF]);
    // Not an identifier (a signature).
    put_symbol(&mut buf, 112, "()I", [0xFF, 0xFF]);
    // A second record of an indexed name keeps the first offset.
    put_symbol(&mut buf, 136, "timeout", [0xFF, 0xFF]);
    let index = CdsSymbolIndex::from_archive_bytes(&buf);
    assert_eq!(index.offset_of("timeout"), Some(16));
    assert_eq!(index.offset_of("enabled"), Some(40));
    assert_eq!(index.offset_of("ratio"), None);
    assert_eq!(index.offset_of("tags"), None);
    assert_eq!(index.offset_of("()I"), None);
    assert_eq!(index.offsets.len(), 2);
}

/// `getDeclaredMethods()` of `R16ProxyAnnOrder.E` on HotSpot 25: archived
/// names in archive order, the run-time name (`ratio`) after them. An empty
/// index (no archive) keeps class-file order.
#[test]
fn declared_method_order_puts_archived_names_first_in_archive_order() {
    let declared = [
        "timeout",
        "name",
        "groups",
        "enabled",
        "description",
        "priority",
        "tags",
        "ratio",
        "q",
    ];
    assert_eq!(
        hotspot_order(&jdk25_index(), &declared),
        [
            "name",
            "priority",
            "enabled",
            "q",
            "groups",
            "timeout",
            "tags",
            "description",
            "ratio"
        ]
    );
    assert_eq!(
        hotspot_order(&CdsSymbolIndex::default(), &declared),
        declared.map(str::to_string)
    );
    // Measured `T1$C { int k(); int n(); int s(); }` -> `s n k`.
    assert_eq!(
        hotspot_order(&jdk25_index(), &["k", "n", "s"]),
        ["s", "n", "k"]
    );
    // Run-time names keep class-file order among themselves.
    assert_eq!(
        hotspot_order(&jdk25_index(), &["zzqb", "value", "zzqa"]),
        ["value", "zzqb", "zzqa"]
    );
}

/// `CRATONVM_REFLECT_HOTSPOT_METHOD_ORDER` (interpreter round i5 wave 101,
/// lane L3): run-time names rank by their constant-pool position, after
/// the archived names; names in neither keep first appearance after them.
#[test]
fn ranked_method_array_order_ranks_run_time_names_by_pool_position() {
    let names = ["zzqb", "value", "zzqa", "zzqc"];
    let mut pool: FxHashMap<String, usize> = FxHashMap::default();
    pool.insert("zzqa".to_string(), 5);
    pool.insert("zzqb".to_string(), 9);
    let order: Vec<&str> = hotspot_method_array_order_ranked(&jdk25_index(), &names, &pool)
        .into_iter()
        .map(|i| names[i])
        .collect();
    assert_eq!(order, ["value", "zzqa", "zzqb", "zzqc"]);
    // With no pool ranks it is `hotspot_method_array_order`.
    assert_eq!(
        hotspot_method_array_order_ranked(&jdk25_index(), &names, &FxHashMap::default()),
        hotspot_method_array_order(&jdk25_index(), &names)
    );
}

/// Interpreter round i5 wave 102, lane L3: `java.lang.Object`'s method
/// names at their offsets in Temurin 25.0.3's Windows
/// `bin/server/classes.jsa` (read off the file with the
/// `CdsSymbolIndex` scan), in class-file order, give HotSpot's
/// `Object.class.getDeclaredMethods()` order exactly, the three `wait`
/// overloads included (`L3W101DeclaredMethodOrder` row `jdk
/// java.lang.Object`). The model is right; a wrong row is the order not
/// being applied.
#[test]
fn object_method_array_order_matches_hotspot_25() {
    let mut offsets: FxHashMap<u64, u32> = FxHashMap::default();
    for (n, off) in [
        ("<init>", 5_061_240u32),
        ("finalize", 5_061_552),
        ("wait0", 5_062_432),
        ("equals", 5_062_696),
        ("toString", 5_062_744),
        ("hashCode", 5_064_264),
        ("getClass", 5_064_280),
        ("clone", 5_064_296),
        ("notify", 5_064_312),
        ("notifyAll", 5_064_328),
        ("wait", 5_090_176),
    ] {
        offsets.insert(CdsSymbolIndex::hash_name(n.as_bytes()), off);
    }
    let index = CdsSymbolIndex { offsets };
    // Class-file order, each with its arity to tell the overloads apart.
    let methods = [
        ("<init>", 0),
        ("getClass", 0),
        ("hashCode", 0),
        ("equals", 1),
        ("clone", 0),
        ("toString", 0),
        ("notify", 0),
        ("notifyAll", 0),
        ("wait", 0),
        ("wait", 1),
        ("wait0", 1),
        ("wait", 2),
        ("finalize", 0),
    ];
    let names: Vec<&str> = methods.iter().map(|(n, _)| *n).collect();
    let order: Vec<String> = hotspot_method_array_order(&index, &names)
        .into_iter()
        .map(|i| format!("{}/{}", methods[i].0, methods[i].1))
        .filter(|s| !s.starts_with('<'))
        .collect();
    assert_eq!(
        order,
        [
            "finalize/0",
            "wait0/1",
            "equals/1",
            "toString/0",
            "hashCode/0",
            "getClass/0",
            "clone/0",
            "notify/0",
            "notifyAll/0",
            "wait/1",
            "wait/2",
            "wait/0"
        ]
    );
}

/// The two HotSpot 25 reference lines (`ref16/R16ProxyAnnOrder.ref` H7,
/// `ref16/R16ProxyIdentity.ref` `ann`), defaulted part: `memberDefaults`
/// filled in `getDeclaredMethods()` order, iterated bucket by bucket.
#[test]
fn member_defaults_order_matches_the_hotspot_reference_lines() {
    let e = [
        "timeout",
        "name",
        "groups",
        "enabled",
        "description",
        "priority",
        "tags",
        "ratio",
    ];
    assert!(member_defaults_share_a_bucket(&e.map(str::to_string)));
    let put = hotspot_order(&jdk25_index(), &e);
    assert_eq!(
        member_defaults_iteration_order(put),
        [
            "name",
            "groups",
            "description",
            "priority",
            "enabled",
            "timeout",
            "tags",
            "ratio"
        ]
    );
    let r = hotspot_order(&jdk25_index(), &["k", "n", "s"]);
    assert_eq!(member_defaults_iteration_order(r), ["n", "s", "k"]);
    // `R16ProxyAnnOrder.A`'s defaulted `alpha`, `mid`: different buckets.
    let a_defaults = ["alpha".to_string(), "mid".to_string()];
    assert!(!member_defaults_share_a_bucket(&a_defaults));
}
