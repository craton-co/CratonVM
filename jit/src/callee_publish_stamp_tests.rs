use super::*;

/// Interpreter round i4 wave 92, lane L7: a publication moves only its own
/// `(method, descriptor)` bucket's stamp; a flush and a redefinition move
/// every bucket's. The VM's by-name callee door relies on both halves
/// (`CRATONVM_JIT_CALLEE_NEG_KEYED_STAMP`).
#[test]
fn a_publication_moves_its_bucket_and_a_flush_moves_every_bucket() {
    let cache = JitCache::new();
    let (name, desc) = ("get", "(I)Ljava/lang/Object;");
    let mine = callee_publish_bucket(name, desc);
    let mut other = String::new();
    for i in 0..4096 {
        let candidate = format!("m{i}");
        if callee_publish_bucket(&candidate, "()V") != mine {
            other = candidate;
            break;
        }
    }
    assert!(!other.is_empty(), "some name lands in another bucket");
    let before = cache.callee_publish_stamp(name, desc);
    cache.bump_callee_publish_bucket(callee_publish_bucket(&other, "()V"));
    assert_eq!(
        cache.callee_publish_stamp(name, desc),
        before,
        "another name's publication leaves this stamp alone"
    );
    cache.bump_callee_publish_bucket(mine);
    let published = cache.callee_publish_stamp(name, desc);
    assert_ne!(published, before, "this name's publication moves it");
    cache.clear_all();
    let flushed = cache.callee_publish_stamp(name, desc);
    assert_ne!(flushed, published, "a flush moves every bucket");
    cache.bump_redefine_epoch();
    assert_ne!(
        cache.callee_publish_stamp(name, desc),
        flushed,
        "a redefinition moves every bucket"
    );
}
