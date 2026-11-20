use super::{
    class_hash_memo_slot, native_class_hash, native_class_hash_memoized,
    CLASS_HASH_MEMO_NAME_MAX, CLASS_HASH_MEMO_WAYS,
};

#[test]
fn the_memo_answers_the_walks_state_for_every_shape() {
    let long = "a/".repeat(CLASS_HASH_MEMO_NAME_MAX);
    let names = [
        "java/lang/Object",
        "java/util/concurrent/locks/ReentrantLock",
        "java/util/concurrent/locks/AbstractQueuedSynchronizer",
        "a",
        "",
        "abcdefgh",
        "abcdefghi",
        long.as_str(),
    ];
    // Twice: the first round fills, the second answers from the memo.
    for _ in 0..2 {
        for name in names {
            assert_eq!(
                native_class_hash_memoized(name),
                native_class_hash(name),
                "{name:?}"
            );
        }
    }
}

#[test]
fn two_names_in_one_way_never_answer_for_each_other() {
    // Find two distinct names that share a way, then alternate them.
    let base = "java/util/Clash";
    let first = format!("{base}0");
    let slot = class_hash_memo_slot(first.as_bytes());
    let second = (1..100_000)
        .map(|i| format!("{base}{i}"))
        .find(|n| class_hash_memo_slot(n.as_bytes()) == slot)
        .expect("a second name in the same way");
    for _ in 0..3 {
        assert_eq!(
            native_class_hash_memoized(&first),
            native_class_hash(&first)
        );
        assert_eq!(
            native_class_hash_memoized(&second),
            native_class_hash(&second)
        );
    }
    // Same length and a one-byte difference in the middle.
    let a = "java/util/HashMap$Node";
    let b = "java/util/HashMap$Nade";
    for _ in 0..2 {
        assert_eq!(native_class_hash_memoized(a), native_class_hash(a));
        assert_eq!(native_class_hash_memoized(b), native_class_hash(b));
    }
}

#[test]
fn the_way_index_is_in_range() {
    for name in ["", "x", "java/lang/String", "a/b/c/d/e/f/g/h/i/j/k/l/m/n"] {
        assert!(class_hash_memo_slot(name.as_bytes()) < CLASS_HASH_MEMO_WAYS);
    }
}
