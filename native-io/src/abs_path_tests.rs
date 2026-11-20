use super::*;
use crate::test_support::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// Build a synthetic `java.io.File` whose slot-0 path string is `path`.
fn make_file(ctx: &mut MockNativeContext, path: &str) -> ObjectRef {
    let f = ctx.alloc_object(1);
    let s = ctx.attach_string(path);
    ctx.set_field(f, 0, Value::Object(Some(s)));
    f
}

#[test]
fn get_absolute_path_returns_absolute_input_verbatim() {
    // Use a platform-appropriate absolute path that does not exist on
    // disk. The pre-fix code ran `fs::canonicalize` first; for a path
    // that resolves it would have injected a `\\?\` prefix / followed
    // symlinks. The fix returns an already-absolute path unchanged.
    let input = if cfg!(windows) {
        r"C:\craton\does\not\exist\abs_path_probe.txt"
    } else {
        "/craton/does/not/exist/abs_path_probe.txt"
    };
    let mut ctx = MockNativeContext::new();
    let f = make_file(&mut ctx, input);
    let r = native_file_get_absolute_path(&mut ctx, &[Value::Object(Some(f))])
        .expect("native ok")
        .expect("returns a string");
    let out = match r {
        Value::Object(Some(o)) => ctx.read_string(o).expect("string value"),
        other => panic!("expected String, got {other:?}"),
    };
    assert_eq!(
        out, input,
        "already-absolute path must be returned verbatim"
    );
    assert!(
        !out.starts_with(r"\\?\"),
        "getAbsolutePath must not add the canonicalize-only \\\\?\\ prefix"
    );
}

// -----------------------------------------------------------------------
// RegexLru (delimiter regex cache) tests
// -----------------------------------------------------------------------

fn re(pattern: &str) -> regex::Regex {
    regex::Regex::new(pattern).unwrap()
}

/// Validate the LRU's internal doubly-linked list is consistent: the
/// `index` and the `head..tail` chain describe the same set, in order.
fn lru_order(lru: &RegexLru) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = lru.head;
    while i != LRU_NIL {
        out.push(lru.nodes[i].pattern.clone());
        i = lru.nodes[i].next;
    }
    out
}

#[test]
fn regex_lru_hit_and_miss() {
    let mut lru = RegexLru::new();
    assert!(lru.get("a+").is_none(), "empty cache misses");
    lru.insert("a+".to_string(), re("a+"));
    assert!(lru.get("a+").is_some(), "inserted pattern hits");
    assert!(lru.get("b+").is_none(), "other pattern still misses");
}

#[test]
fn regex_lru_insert_is_idempotent() {
    let mut lru = RegexLru::new();
    lru.insert("x".to_string(), re("x"));
    lru.insert("x".to_string(), re("x"));
    assert_eq!(lru.index.len(), 1, "duplicate insert must not grow cache");
    assert_eq!(lru_order(&lru), vec!["x".to_string()]);
}

#[test]
fn regex_lru_get_marks_mru() {
    let mut lru = RegexLru::new();
    lru.insert("a".to_string(), re("a"));
    lru.insert("b".to_string(), re("b"));
    lru.insert("c".to_string(), re("c"));
    // Order so far (LRU..MRU): a, b, c
    assert_eq!(lru_order(&lru), vec!["a", "b", "c"]);
    // Touching "a" moves it to MRU.
    assert!(lru.get("a").is_some());
    assert_eq!(lru_order(&lru), vec!["b", "c", "a"]);
}

#[test]
fn regex_lru_evicts_least_recently_used() {
    let mut lru = RegexLru::new();
    // Fill to capacity with distinct patterns p0..p{CAP-1}.
    for i in 0..REGEX_CACHE_CAP {
        let p = format!("p{i}");
        lru.insert(p.clone(), re(&p));
    }
    assert_eq!(lru.index.len(), REGEX_CACHE_CAP);
    assert_eq!(lru.nodes.len(), REGEX_CACHE_CAP);
    // p0 is the LRU. Inserting one more evicts exactly p0.
    lru.insert("new".to_string(), re("new"));
    assert_eq!(lru.index.len(), REGEX_CACHE_CAP, "stays bounded");
    assert_eq!(lru.nodes.len(), REGEX_CACHE_CAP, "slots are recycled");
    assert!(lru.get("p0").is_none(), "LRU victim was evicted");
    assert!(lru.get("p1").is_some(), "next-oldest retained");
    assert!(lru.get("new").is_some(), "newest retained");
}

#[test]
fn regex_lru_touch_changes_eviction_victim() {
    let mut lru = RegexLru::new();
    for i in 0..REGEX_CACHE_CAP {
        let p = format!("q{i}");
        lru.insert(p.clone(), re(&p));
    }
    // Touch q0 so it is no longer the LRU; q1 becomes the victim.
    assert!(lru.get("q0").is_some());
    lru.insert("extra".to_string(), re("extra"));
    assert!(lru.get("q0").is_some(), "recently-touched entry survives");
    assert!(lru.get("q1").is_none(), "new LRU victim evicted");
}

#[test]
fn cached_regex_default_whitespace_fast_path() {
    // The default delimiter never touches the LRU and always compiles.
    let r = cached_regex(r"\s+").unwrap();
    assert!(r.is_match("a b"));
}
