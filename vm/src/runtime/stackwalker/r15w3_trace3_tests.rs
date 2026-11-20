use super::r13_trace3_standin_tests::{add_class, java, pool};
use super::*;

const CNFE: &str = "java/lang/ClassNotFoundException";
const ONE_ARG: &str = "(Ljava/lang/String;)Ljava/lang/Class;";
const RESOLVE: &str = "(Ljava/lang/String;Z)Ljava/lang/Class;";

fn rows(frames: Option<Vec<BacktraceFrame>>) -> Option<Vec<(String, i32, i32)>> {
    frames.map(|v| {
        v.iter()
            .map(|f| {
                let e = f.to_entry();
                (
                    format!("{}.{}", e.class_name, e.method_name),
                    e.line_number,
                    e.byte_code_index,
                )
            })
            .collect()
    })
}

fn row(frame: &str, line: i32, bci: i32) -> (String, i32, i32) {
    (frame.to_string(), line, bci)
}

/// `URLClassLoader.findClass` with its two raises: `(String, Throwable)`
/// at pc 6 (line 360) and the plain `(String)` at pc 15 (line 366).
fn jdk25_find_class() -> ClassFileMethod {
    java(
        "findClass",
        ONE_ARG,
        // 0 new 3 dup 4 aload_1 5 aconst_null 6 invokespecial(#6) 9 athrow
        // 10 new 13 dup 14 aload_1 15 invokespecial(#12) 18 athrow
        vec![
            0xbb, 0, 2, 0x59, 0x2b, 0x01, 0xb7, 0, 6, 0xbf, 0xbb, 0, 2, 0x59, 0x2b, 0xb7, 0,
            12, 0xbf,
        ],
        &[(0, 360), (10, 366)],
    )
}

struct Loaders {
    store: ClassStore,
    /// `java.net.URLClassLoader` with JDK 25's `findClass`.
    url: ClassId,
    /// A subclass with its own `findClass` (the Rust never ran it).
    own_find: ClassId,
}

/// JDK-25-shaped `ClassLoader`: `loadClass(String)` calls `loadClass(name,
/// false)` at pc 3 (line 490); `loadClass(String, boolean)` calls
/// `parent.loadClass(name, false)` at pc 3 and `findClass` at pc 9 (line 557).
fn loaders(find_class: ClassFileMethod) -> Loaders {
    let mut store = ClassStore::new();
    let object = add_class(&mut store, "java/lang/Object", None, pool(&[]), vec![]);
    let class_loader = add_class(
        &mut store,
        "java/lang/ClassLoader",
        Some(object),
        pool(&[
            ("java/lang/ClassLoader", "loadClass", RESOLVE),
            ("java/lang/ClassLoader", "findClass", ONE_ARG),
        ]),
        vec![
            java(
                "loadClass",
                ONE_ARG,
                vec![0x2a, 0x2b, 0x03, 0xb6, 0, 6, 0xb0],
                &[(0, 490)],
            ),
            // 0 aload_0 1 aload_1 2 iconst_0 3 invokevirtual(loadClass) 6 pop
            // 7 aload_0 8 aload_1 9 invokevirtual(findClass) 12 areturn
            java(
                "loadClass",
                RESOLVE,
                vec![
                    0x2a, 0x2b, 0x03, 0xb6, 0, 6, 0x57, 0x2a, 0x2b, 0xb6, 0, 12, 0xb0,
                ],
                &[(0, 545), (7, 557)],
            ),
        ],
    );
    let secure = add_class(
        &mut store,
        "java/security/SecureClassLoader",
        Some(class_loader),
        pool(&[]),
        vec![],
    );
    let url = add_class(
        &mut store,
        "java/net/URLClassLoader",
        Some(secure),
        pool(&[
            (CNFE, "<init>", "(Ljava/lang/String;Ljava/lang/Throwable;)V"),
            (CNFE, "<init>", "(Ljava/lang/String;)V"),
        ]),
        vec![find_class],
    );
    let own_find = add_class(
        &mut store,
        "probe/OwnFindLoader",
        Some(url),
        pool(&[]),
        vec![java("findClass", ONE_ARG, vec![0x01, 0xb0], &[(0, 20)])],
    );
    Loaders {
        store,
        url,
        own_find,
    }
}

#[test]
fn a_served_url_loader_miss_gets_its_loader_frames() {
    let fx = loaders(jdk25_find_class());
    assert_eq!(
        rows(served_loader_miss_frames(&fx.store, fx.url, CNFE)),
        Some(vec![
            row("java/lang/ClassLoader.loadClass", 490, 3),
            row("java/lang/ClassLoader.loadClass", 557, 9),
            row("java/net/URLClassLoader.findClass", 366, 15),
        ])
    );
}

/// A `findClass` override the Rust never ran, a `findClass` without the
/// plain raise (a JDK whose miss is inside a `doPrivileged` action), or
/// another throwable: nothing (fail-closed).
#[test]
fn other_url_loader_shapes_get_no_loader_frames() {
    let fx = loaders(jdk25_find_class());
    assert!(served_loader_miss_frames(&fx.store, fx.own_find, CNFE).is_none());
    assert!(served_loader_miss_frames(&fx.store, fx.url, "java/lang/LinkageError").is_none());
    let privileged = loaders(java("findClass", ONE_ARG, vec![0x01, 0xb0], &[(0, 30)]));
    assert!(served_loader_miss_frames(&privileged.store, privileged.url, CNFE).is_none());
}
