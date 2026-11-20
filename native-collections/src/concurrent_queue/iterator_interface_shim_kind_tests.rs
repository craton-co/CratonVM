use super::register_iterator_protocol_natives;
use cratonvm_native_api::{NativeKind, NativeMethodRegistry};
use cratonvm_types::compat::CompatibilityMode;

const ITR: &str = "java/util/Iterator";
const SHIM: [(&str, &str); 2] = [("hasNext", "()Z"), ("next", "()Ljava/lang/Object;")];

fn registry(mode: CompatibilityMode) -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    r.set_compatibility_mode(mode);
    register_iterator_protocol_natives(&mut r);
    r
}




// JIT round 16 wave 8 (lane collections): the SUB-interface shims,
// `iterator_subinterface_shim_kind`.
const SUB_SHIM: [(&str, &str, &str); 18] = [
    ("java/util/ListIterator", "hasNext", "()Z"),
    ("java/util/ListIterator", "next", "()Ljava/lang/Object;"),
    ("java/util/ListIterator", "hasPrevious", "()Z"),
    ("java/util/ListIterator", "previous", "()Ljava/lang/Object;"),
    ("java/util/ListIterator", "nextIndex", "()I"),
    ("java/util/ListIterator", "previousIndex", "()I"),
    ("java/util/ListIterator", "remove", "()V"),
    ("java/util/ListIterator", "set", "(Ljava/lang/Object;)V"),
    ("java/util/ListIterator", "add", "(Ljava/lang/Object;)V"),
    ("java/util/PrimitiveIterator$OfInt", "hasNext", "()Z"),
    (
        "java/util/PrimitiveIterator$OfInt",
        "next",
        "()Ljava/lang/Object;",
    ),
    ("java/util/PrimitiveIterator$OfInt", "nextInt", "()I"),
    ("java/util/PrimitiveIterator$OfLong", "hasNext", "()Z"),
    (
        "java/util/PrimitiveIterator$OfLong",
        "next",
        "()Ljava/lang/Object;",
    ),
    ("java/util/PrimitiveIterator$OfLong", "nextLong", "()J"),
    ("java/util/PrimitiveIterator$OfDouble", "hasNext", "()Z"),
    (
        "java/util/PrimitiveIterator$OfDouble",
        "next",
        "()Ljava/lang/Object;",
    ),
    ("java/util/PrimitiveIterator$OfDouble", "nextDouble", "()D"),
];




const SPL: &str = "java/util/Spliterator";
const SPL_SHIM: [(&str, &str); 6] = [
    ("estimateSize", "()J"),
    ("characteristics", "()I"),
    ("getComparator", "()Ljava/util/Comparator;"),
    ("tryAdvance", "(Ljava/util/function/Consumer;)Z"),
    ("trySplit", "()Ljava/util/Spliterator;"),
    ("forEachRemaining", "(Ljava/util/function/Consumer;)V"),
];