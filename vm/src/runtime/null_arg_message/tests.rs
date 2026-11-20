use super::*;
use std::path::PathBuf;

#[test]
fn param_slots_skip_this_and_count_wide_parameters_twice() {
    assert_eq!(param_slot("(Ljava/util/Collection;)Z", false, 0), Some(1));
    assert_eq!(param_slot("(ILjava/util/Collection;)Z", false, 1), Some(2));
    assert_eq!(param_slot("(JLjava/lang/Object;)V", true, 1), Some(2));
    assert_eq!(param_slot("(D[I)V", false, 1), Some(3));
    // A primitive has nothing to dereference; past the end is no answer.
    assert_eq!(param_slot("(ILjava/util/Collection;)Z", false, 0), None);
    assert_eq!(param_slot("()V", false, 0), None);
    assert_eq!(
        param_tokens("([[Ljava/lang/String;JZ)V"),
        Some(vec!["[[Ljava/lang/String;", "J", "Z"])
    );
    assert_eq!(param_tokens("not a descriptor"), None);
}

/// `java.base`'s parsed classes, as a [`MethodSource`].
struct JmodClasses {
    path: cratonvm_classloading::ClassPath,
    parsed: std::cell::RefCell<
        std::collections::HashMap<String, std::rc::Rc<cratonvm_reader::ClassFile>>,
    >,
}

impl JmodClasses {
    fn class(&self, name: &str) -> Option<std::rc::Rc<cratonvm_reader::ClassFile>> {
        if let Some(c) = self.parsed.borrow().get(name) {
            return Some(c.clone());
        }
        let bytes = self.path.find_class(name).ok()?;
        let mut class = cratonvm_reader::read_class_shared(bytes).ok()?;
        let cp = &class.constant_pool;
        for m in class.methods.iter_mut() {
            cratonvm_reader::force_decode_all(&mut m.attributes, cp).ok()?;
        }
        let class = std::rc::Rc::new(class);
        self.parsed
            .borrow_mut()
            .insert(name.to_string(), class.clone());
        Some(class)
    }
}

impl MethodSource for JmodClasses {
    fn with_body(
        &self,
        class: &str,
        name: &str,
        descriptor: &str,
        f: &mut dyn FnMut(&MethodBody<'_>) -> Option<String>,
    ) -> Option<String> {
        let parsed = self.class(class)?;
        let method = parsed.find_method(name, descriptor)?;
        let code = method.code()?;
        f(&MethodBody {
            class_name: &parsed.this_class,
            descriptor,
            is_static: method.is_static(),
            cp: &parsed.constant_pool,
            code,
        })
    }
}

fn java_base_jmod() -> Option<PathBuf> {
    let mut homes: Vec<PathBuf> = Vec::new();
    if let Ok(home) = cratonvm_types::flags::runtime_var("JAVA_HOME") {
        homes.push(PathBuf::from(home));
    }
    for candidate in [
        "C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot",
        "C:/Program Files/Java/jdk-25",
        "/data/toolchain/jdk-25",
    ] {
        homes.push(PathBuf::from(candidate));
    }
    homes
        .into_iter()
        .map(|h| h.join("jmods").join("java.base.jmod"))
        .find(|p| p.exists())
}

/// C5W4-1's first step: the query reproduces, from JDK 25's own bytecode,
/// the lines HotSpot 25 printed for `R15Compat4NullArgMessages` rows
/// 0-5, 8-9 and `R15Compat5NullArgMore` rows 4, 9 (transcribed in
/// `native-collections` as `NULL_C_TO_ARRAY` / `NULL_C_SIZE` /
/// `NULL_C_ITERATOR` / `NULL_A_LENGTH` / `NULL_M_COMPARATOR`; re-measured
/// by `R16CompatdrainNullArgs`), and declines the bodies whose answer is
/// not on a straight-line prefix or is not a dereference at all.
#[test]
fn the_query_reproduces_hotspots_null_argument_lines_from_jdk_bytecode() {
    let Some(jmod) = java_base_jmod() else {
        assert!(
            cratonvm_types::flags::runtime_var_os("CRATONVM_REQUIRE_E2E").is_none(),
            "no java.base.jmod (set JAVA_HOME)"
        );
        eprintln!("SKIP the_query_reproduces_hotspots_null_argument_lines: no java.base.jmod");
        return;
    };
    let src = JmodClasses {
        path: cratonvm_classloading::ClassPath::new(&[jmod.to_string_lossy().into_owned()]),
        parsed: Default::default(),
    };
    let to_array = "Cannot invoke \"java.util.Collection.toArray()\" because \"c\" is null";
    let size = "Cannot invoke \"java.util.Collection.size()\" because \"c\" is null";
    let iterator = "Cannot invoke \"java.util.Collection.iterator()\" because \"c\" is null";
    let length = "Cannot read the array length because \"a\" is null";
    let comparator = "Cannot invoke \"java.util.SortedMap.comparator()\" because \"m\" is null";
    let coll = "(Ljava/util/Collection;)Z";
    let ctor = "(Ljava/util/Collection;)V";
    let rows: [(&str, &str, &str, Option<&str>); 16] = [
        ("java/util/ArrayList", "addAll", coll, Some(to_array)),
        ("java/util/Vector", "addAll", coll, Some(to_array)),
        ("java/util/LinkedList", "addAll", coll, Some(to_array)),
        ("java/util/ArrayDeque", "addAll", coll, Some(size)),
        ("java/util/ArrayDeque", "<init>", ctor, Some(size)),
        ("java/util/LinkedList", "<init>", ctor, Some(to_array)),
        ("java/util/ArrayList", "<init>", ctor, Some(to_array)),
        ("java/util/HashSet", "<init>", ctor, Some(size)),
        (
            "java/util/AbstractCollection",
            "containsAll",
            "(Ljava/util/Collection;)Z",
            Some(iterator),
        ),
        (
            "java/util/ArrayList",
            "toArray",
            "([Ljava/lang/Object;)[Ljava/lang/Object;",
            Some(length),
        ),
        // j27 CD16-1: the last wired site (`NULL_ARG_LL_TO_ARRAY`).
        (
            "java/util/LinkedList",
            "toArray",
            "([Ljava/lang/Object;)[Ljava/lang/Object;",
            Some(length),
        ),
        (
            "java/util/TreeMap",
            "<init>",
            "(Ljava/util/SortedMap;)V",
            Some(comparator),
        ),
        // `Objects.requireNonNull(c)` (through `batchRemove`): HotSpot's
        // bare NPE, which the query leaves to the caller.
        (
            "java/util/ArrayList",
            "removeAll",
            "(Ljava/util/Collection;)Z",
            None,
        ),
        (
            "java/util/ArrayDeque",
            "removeAll",
            "(Ljava/util/Collection;)Z",
            None,
        ),
        // A branch on the receiver's state comes first.
        ("java/util/TreeSet", "addAll", coll, None),
        // `a` goes to another class (`HashMap.prepareArray`).
        (
            "java/util/HashSet",
            "toArray",
            "([Ljava/lang/Object;)[Ljava/lang/Object;",
            None,
        ),
    ];
    for (class, name, desc, want) in rows {
        let got = null_arg_message_in(&src, class, name, desc, 0);
        assert_eq!(got.as_deref(), want, "{class}.{name}{desc}");
    }
    // `LinkedList.addAll(int, c)`: the second parameter.
    assert_eq!(
        null_arg_message_in(
            &src,
            "java/util/LinkedList",
            "addAll",
            "(ILjava/util/Collection;)Z",
            1
        )
        .as_deref(),
        Some(to_array)
    );
}
