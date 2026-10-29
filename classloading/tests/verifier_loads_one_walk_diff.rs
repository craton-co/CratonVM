// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 23 wave 10 (lane L1; R23-W10-FLIP): the verifier loads the link's
//! ONE walk records, pinned to the `loadClass` calls HotSpot 25 makes.
//!
//! A user-loader class's link replays the verifier's class loads through its
//! defining loader's `loadClass` -- Java-visible. Since
//! `CRATONVM_USER_PASS3_RECORDS_LINK_LOADS` is default ON (a parity fix), they
//! come from the Pass 3 verdict walk
//! (`verifier::verify_user_loader_class_bytecode_recording`). The two-walk route
//! it replaced (the type-map walk, `verifier::collect_class_type_maps`) misses
//! loads HotSpot makes: page
//! `docs/feature-designs/class-resolution-and-caching.md`.
//!
//! The expected lists are HotSpot's, measured with
//! `tools/probes/jit/r23/VLoad.java` on Temurin 25.0.3: a user loader that
//! logs every `loadClass`, defines the class, then links it
//! (`getDeclaredConstructors`). Those LINK lines include the loads a define of
//! one of the class's own nested classes makes for ITS supertypes (the
//! interfaces, then the superclass), so this test replays the recorded list
//! the way the VM does ([`replay`]): each name once, a conditional entry
//! resolved after its target loaded, and a corpus class's own supertypes asked
//! when it is defined. They also include the probe's own reflection:
//! `getDeclaredConstructors` resolves the declared constructors' parameter
//! classes after the link (`$Proxy0`'s `InvocationHandler`, tagged
//! `(reflection)` by HotSpot's `-Xlog:class+resolve=debug`; the VM loads it in
//! the same place), so [`replay`] appends those last.
//!
//! The hierarchy is the classes of `vm/tests/resources/cratonvm` and the
//! proxy-row fixture, plus the JDK classes they name ([`JDK_ROWS`], read off
//! Temurin 25.0.3 by reflection), so the test is deterministic. A corpus class
//! other than the one being linked is not loaded yet at its link (as in the
//! VM), so the walk sees it as unresolvable.
//!
//! Exact order is pinned for all five classes: the walk visits `<init>`, then
//! `<clinit>`, then the other methods (`CRATONVM_VERIFY_INITIALIZERS_FIRST`),
//! the part of HotSpot's method order every HotSpot run agrees on.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use cratonvm_classloading::class_origin::ClassOrigin;
use cratonvm_classloading::type_maps::TypeMapStore;
use cratonvm_classloading::verifier::verify_user_loader_class_bytecode_recording;
use cratonvm_classloading::vtype::{ClassHierarchy, VerifierLinkLoad};
use cratonvm_classloading::{Class, ClassId, ClassLoaderId, ClassState};
use cratonvm_reader::class_file::ClassFile;
use cratonvm_reader::{force_decode_all, read_class};

/// `name|super|interfaces|I or C` for every JDK class the checked classes
/// name, closed over supertypes (Temurin 25.0.3, by reflection; an
/// interface's superclass written as `java/lang/Object`, as its class file
/// says).
const JDK_ROWS: &[&str] = &[
    "java/io/Closeable|java/lang/Object|java/lang/AutoCloseable|I",
    "java/io/FilterOutputStream|java/io/OutputStream||C",
    "java/io/Flushable|java/lang/Object||I",
    "java/io/OutputStream|java/lang/Object|java/io/Closeable,java/io/Flushable|C",
    "java/io/PrintStream|java/io/FilterOutputStream|java/lang/Appendable,java/io/Closeable|C",
    "java/io/Serializable|java/lang/Object||I",
    "java/lang/AbstractStringBuilder|java/lang/Object|java/lang/Appendable,java/lang/CharSequence|C",
    "java/lang/Appendable|java/lang/Object||I",
    "java/lang/AutoCloseable|java/lang/Object||I",
    "java/lang/Boolean|java/lang/Object|java/io/Serializable,java/lang/Comparable,java/lang/constant/Constable|C",
    "java/lang/Byte|java/lang/Number|java/lang/Comparable,java/lang/constant/Constable|C",
    "java/lang/CharSequence|java/lang/Object||I",
    "java/lang/Character|java/lang/Object|java/io/Serializable,java/lang/Comparable,java/lang/constant/Constable|C",
    "java/lang/Class|java/lang/Object|java/io/Serializable,java/lang/reflect/GenericDeclaration,java/lang/reflect/Type,java/lang/reflect/AnnotatedElement,java/lang/invoke/TypeDescriptor$OfField,java/lang/constant/Constable|C",
    "java/lang/ClassLoader|java/lang/Object||C",
    "java/lang/ClassNotFoundException|java/lang/ReflectiveOperationException||C",
    "java/lang/Cloneable|java/lang/Object||I",
    "java/lang/Comparable|java/lang/Object||I",
    "java/lang/Double|java/lang/Number|java/lang/Comparable,java/lang/constant/Constable,java/lang/constant/ConstantDesc|C",
    "java/lang/Enum|java/lang/Object|java/lang/constant/Constable,java/lang/Comparable,java/io/Serializable|C",
    "java/lang/Error|java/lang/Throwable||C",
    "java/lang/Exception|java/lang/Throwable||C",
    "java/lang/Float|java/lang/Number|java/lang/Comparable,java/lang/constant/Constable,java/lang/constant/ConstantDesc|C",
    "java/lang/IllegalAccessException|java/lang/ReflectiveOperationException||C",
    "java/lang/IllegalArgumentException|java/lang/RuntimeException||C",
    "java/lang/IllegalStateException|java/lang/RuntimeException||C",
    "java/lang/IncompatibleClassChangeError|java/lang/LinkageError||C",
    "java/lang/Integer|java/lang/Number|java/lang/Comparable,java/lang/constant/Constable,java/lang/constant/ConstantDesc|C",
    "java/lang/Iterable|java/lang/Object||I",
    "java/lang/LinkageError|java/lang/Error||C",
    "java/lang/Long|java/lang/Number|java/lang/Comparable,java/lang/constant/Constable,java/lang/constant/ConstantDesc|C",
    "java/lang/Math|java/lang/Object||C",
    "java/lang/NoClassDefFoundError|java/lang/LinkageError||C",
    "java/lang/NoSuchMethodError|java/lang/IncompatibleClassChangeError||C",
    "java/lang/NoSuchMethodException|java/lang/ReflectiveOperationException||C",
    "java/lang/NullPointerException|java/lang/RuntimeException||C",
    "java/lang/Number|java/lang/Object|java/io/Serializable|C",
    "java/lang/Object|||C",
    "java/lang/ReflectiveOperationException|java/lang/Exception||C",
    "java/lang/Runnable|java/lang/Object||I",
    "java/lang/Runtime|java/lang/Object||C",
    "java/lang/RuntimeException|java/lang/Exception||C",
    "java/lang/Short|java/lang/Number|java/lang/Comparable,java/lang/constant/Constable|C",
    "java/lang/String|java/lang/Object|java/io/Serializable,java/lang/Comparable,java/lang/CharSequence,java/lang/constant/Constable,java/lang/constant/ConstantDesc|C",
    "java/lang/StringBuilder|java/lang/AbstractStringBuilder|java/lang/Appendable,java/io/Serializable,java/lang/Comparable,java/lang/CharSequence|C",
    "java/lang/System|java/lang/Object||C",
    "java/lang/Thread|java/lang/Object|java/lang/Runnable|C",
    "java/lang/Throwable|java/lang/Object|java/io/Serializable|C",
    "java/lang/constant/Constable|java/lang/Object||I",
    "java/lang/constant/ConstantDesc|java/lang/Object||I",
    "java/lang/invoke/CallSite|java/lang/Object||C",
    "java/lang/invoke/LambdaMetafactory|java/lang/Object||C",
    "java/lang/invoke/MethodHandle|java/lang/Object|java/lang/constant/Constable|C",
    "java/lang/invoke/MethodHandles|java/lang/Object||C",
    "java/lang/invoke/MethodHandles$Lookup|java/lang/Object||C",
    "java/lang/invoke/MethodType|java/lang/Object|java/lang/constant/Constable,java/lang/invoke/TypeDescriptor$OfMethod,java/io/Serializable|C",
    "java/lang/invoke/StringConcatFactory|java/lang/Object||C",
    "java/lang/invoke/TypeDescriptor|java/lang/Object||I",
    "java/lang/invoke/TypeDescriptor$OfField|java/lang/Object|java/lang/invoke/TypeDescriptor|I",
    "java/lang/invoke/TypeDescriptor$OfMethod|java/lang/Object|java/lang/invoke/TypeDescriptor|I",
    "java/lang/reflect/AccessibleObject|java/lang/Object|java/lang/reflect/AnnotatedElement|C",
    "java/lang/reflect/AnnotatedElement|java/lang/Object||I",
    "java/lang/reflect/Executable|java/lang/reflect/AccessibleObject|java/lang/reflect/Member,java/lang/reflect/GenericDeclaration|C",
    "java/lang/reflect/GenericDeclaration|java/lang/Object|java/lang/reflect/AnnotatedElement|I",
    "java/lang/reflect/InvocationHandler|java/lang/Object||I",
    "java/lang/reflect/Member|java/lang/Object||I",
    "java/lang/reflect/Method|java/lang/reflect/Executable||C",
    "java/lang/reflect/Proxy|java/lang/Object|java/io/Serializable|C",
    "java/lang/reflect/Type|java/lang/Object||I",
    "java/lang/reflect/UndeclaredThrowableException|java/lang/RuntimeException||C",
    "java/util/AbstractCollection|java/lang/Object|java/util/Collection|C",
    "java/util/AbstractList|java/util/AbstractCollection|java/util/List|C",
    "java/util/ArrayList|java/util/AbstractList|java/util/List,java/util/RandomAccess,java/lang/Cloneable,java/io/Serializable|C",
    "java/util/Arrays|java/lang/Object||C",
    "java/util/Collection|java/lang/Object|java/lang/Iterable|I",
    "java/util/Iterator|java/lang/Object||I",
    "java/util/List|java/lang/Object|java/util/SequencedCollection|I",
    "java/util/RandomAccess|java/lang/Object||I",
    "java/util/SequencedCollection|java/lang/Object|java/util/Collection|I",
    "java/util/concurrent/AbstractExecutorService|java/lang/Object|java/util/concurrent/ExecutorService|C",
    "java/util/concurrent/Callable|java/lang/Object||I",
    "java/util/concurrent/ExecutionException|java/lang/Exception||C",
    "java/util/concurrent/Executor|java/lang/Object||I",
    "java/util/concurrent/ExecutorService|java/lang/Object|java/util/concurrent/Executor,java/lang/AutoCloseable|I",
    "java/util/concurrent/ForkJoinPool|java/util/concurrent/AbstractExecutorService|java/util/concurrent/ScheduledExecutorService|C",
    "java/util/concurrent/ForkJoinTask|java/lang/Object|java/util/concurrent/Future,java/io/Serializable|C",
    "java/util/concurrent/Future|java/lang/Object||I",
    "java/util/concurrent/ScheduledExecutorService|java/lang/Object|java/util/concurrent/ExecutorService|I",
    "java/util/concurrent/TimeUnit|java/lang/Enum||C",
    "java/util/concurrent/atomic/AtomicInteger|java/lang/Number|java/io/Serializable|C",
];

struct Info {
    super_class: Option<Arc<str>>,
    interfaces: Vec<Arc<str>>,
    is_interface: bool,
}

/// A fixed hierarchy that records verifier loads the way the VM's
/// `ClassStoreHierarchy` does, with the corpus classes other than the one
/// being linked not loaded yet.
struct Recording {
    map: HashMap<Arc<str>, Info>,
    corpus: HashSet<Arc<str>>,
    store: TypeMapStore,
    in_flight: RefCell<Option<Arc<str>>>,
    loads: RefCell<Vec<VerifierLinkLoad>>,
}

impl Recording {
    fn begin(&self, class: &str) {
        *self.in_flight.borrow_mut() = Some(Arc::from(class));
        self.loads.borrow_mut().clear();
    }

    fn take(&self) -> Vec<VerifierLinkLoad> {
        std::mem::take(&mut *self.loads.borrow_mut())
    }

    fn is_own(&self, name: &str) -> bool {
        self.in_flight.borrow().as_deref() == Some(name)
    }

    /// The class as the walk may see it: `None` for a corpus class that is
    /// not the one being linked (not loaded yet) and for an unknown name.
    fn loaded(&self, name: &str) -> Option<&Info> {
        if self.corpus.contains(name) && !self.is_own(name) {
            return None;
        }
        self.map.get(name)
    }

    fn walk_is_sub(&self, child: &str, parent: &str, depth: usize) -> bool {
        if child == parent {
            return true;
        }
        if depth > 64 {
            return false;
        }
        let Some(info) = self.loaded(child) else {
            return false;
        };
        info.super_class
            .as_deref()
            .is_some_and(|s| self.walk_is_sub(s, parent, depth + 1))
            || info
                .interfaces
                .iter()
                .any(|i| self.walk_is_sub(i, parent, depth + 1))
    }
}

impl ClassHierarchy for Recording {
    fn is_subclass(&self, child: &str, parent: &str) -> bool {
        if child == parent || parent == "java/lang/Object" {
            return true;
        }
        if self.loaded(child).is_none() || self.loaded(parent).is_none() {
            return true; // unresolved: optimistic, like the VM's adapter
        }
        self.walk_is_sub(child, parent, 0)
    }

    fn is_direct_superclass(&self, child: &str, parent: &str) -> bool {
        self.loaded(child)
            .and_then(|i| i.super_class.as_deref())
            .is_some_and(|s| s == parent)
            || (parent == "java/lang/Object" && self.loaded(child).is_none())
    }

    fn common_superclass(&self, a: &str, b: &str) -> String {
        if a == b {
            return a.to_string();
        }
        let mut cur: Option<&str> = Some(a);
        let mut depth = 0;
        while let Some(c) = cur {
            if depth > 64 {
                break;
            }
            depth += 1;
            if self.walk_is_sub(b, c, 0) {
                return c.to_string();
            }
            cur = self.loaded(c).and_then(|i| i.super_class.as_deref());
        }
        "java/lang/Object".to_string()
    }

    fn is_interface(&self, name: &str) -> bool {
        self.loaded(name).is_some_and(|i| i.is_interface)
    }

    fn is_resolvable(&self, name: &str) -> bool {
        self.loaded(name).is_some()
    }

    fn type_map_store(&self) -> Option<&TypeMapStore> {
        Some(&self.store)
    }

    fn records_verifier_loads(&self) -> bool {
        true
    }

    fn note_verifier_load(&self, name: &str) {
        if self.is_own(name) {
            return;
        }
        let mut loads = self.loads.borrow_mut();
        if !loads
            .iter()
            .any(|seen| matches!(seen, VerifierLinkLoad::Name(n) if &**n == name))
        {
            loads.push(VerifierLinkLoad::Name(Arc::from(name)));
        }
    }

    fn note_verifier_load_unless_interface(&self, target: &str, source: &str) {
        if self.is_own(source) {
            return;
        }
        let mut loads = self.loads.borrow_mut();
        let covered = loads.iter().any(|seen| match seen {
            VerifierLinkLoad::Name(n) => &**n == source,
            VerifierLinkLoad::SourceUnlessInterface {
                target: t,
                source: s,
            } => &**t == target && &**s == source,
        });
        if !covered {
            loads.push(VerifierLinkLoad::SourceUnlessInterface {
                target: Arc::from(target),
                source: Arc::from(source),
            });
        }
    }
}

fn make_class(id: ClassId, cf: ClassFile) -> Class {
    let ClassFile {
        version,
        constant_pool,
        access_flags,
        this_class,
        methods,
        ..
    } = cf;
    Class {
        id,
        loader_id: ClassLoaderId::Application,
        name: this_class,
        source_file: None,
        version,
        state: ClassState::Loaded,
        initializing_thread: None,
        constant_pool,
        access_flags,
        superclass: None,
        interfaces: vec![],
        fields: vec![],
        methods,
        first_field_index: 0,
        num_total_fields: 0,
        bootstrap_methods: vec![],
        signature: None,
        annotations: Vec::new(),
        nest_host: None,
        nest_members: Vec::new(),
        record_components: Vec::new(),
        permitted_subclasses: Vec::new(),
        inner_classes: Vec::new(),
        enclosing_method: None,
        hidden: false,
        module_name: None,
        origin: ClassOrigin::VmInternal,
        has_finalizer: false,
        code_source: None,
        array_info: None,
        init_state: Arc::new(AtomicU8::new(0)),
        record_object_methods: std::sync::atomic::AtomicU8::new(0),
    }
}

fn read_decoded(bytes: &[u8]) -> Option<ClassFile> {
    let mut parsed = read_class(bytes).ok()?;
    for method in parsed.methods.iter_mut() {
        force_decode_all(&mut method.attributes, &parsed.constant_pool).ok()?;
    }
    Some(parsed)
}

/// Every `.class` file under `dir`, recursively, in path order.
fn class_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            class_files(&p, out);
        } else if p.extension().is_some_and(|e| e == "class")
            && !p.file_name().is_some_and(|n| n == "module-info.class")
        {
            out.push(p);
        }
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

/// The hierarchy over the corpus and [`JDK_ROWS`], and the corpus's bytes by
/// internal class name.
fn hierarchy() -> (Recording, HashMap<String, Vec<u8>>) {
    let mut map: HashMap<Arc<str>, Info> = HashMap::new();
    for row in JDK_ROWS {
        let mut parts = row.split('|');
        let name = parts.next().expect("name");
        let sup = parts.next().unwrap_or("");
        let ifaces = parts.next().unwrap_or("");
        let kind = parts.next().unwrap_or("C");
        map.insert(
            Arc::from(name),
            Info {
                super_class: (!sup.is_empty()).then(|| Arc::from(sup)),
                interfaces: ifaces
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(Arc::from)
                    .collect(),
                is_interface: kind == "I",
            },
        );
    }
    let root = workspace_root();
    let mut files = Vec::new();
    class_files(&root.join("vm/tests/resources/cratonvm"), &mut files);
    files.push(root.join("classloading/tests/fixtures/proxy_row/Proxy0_Runnable.class"));
    let mut corpus = HashSet::new();
    let mut bytes_by_name = HashMap::new();
    for path in files {
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let Ok(cf) = read_class(&bytes) else { continue };
        let name: Arc<str> = cf.this_class.clone();
        corpus.insert(name.clone());
        map.insert(
            name.clone(),
            Info {
                super_class: cf.super_class.clone(),
                interfaces: cf.interfaces.clone(),
                is_interface: cf.is_interface(),
            },
        );
        bytes_by_name.insert(name.to_string(), bytes);
    }
    let h = Recording {
        map,
        corpus,
        store: TypeMapStore::new(),
        in_flight: RefCell::new(None),
        loads: RefCell::new(Vec::new()),
    };
    (h, bytes_by_name)
}

/// The `loadClass` calls the link of `class` makes on its defining loader:
/// the recorded loads in order, each name once (the class itself and its own
/// supertypes were initiated by its define), a conditional entry resolved once
/// its target is loaded, and every corpus class this loader defines asking for
/// its own supertypes (the interfaces, then the superclass) as HotSpot's
/// define does.
///
/// `reflected` are the probe's own loads after the link: its
/// `getDeclaredConstructors` resolves each declared constructor's parameter
/// classes through the same loader (HotSpot's `-Xlog:class+resolve=debug` tags
/// them `(reflection)`, not `(verification)`; the VM makes the same calls). They
/// come last and are deduplicated against everything before.
fn replay(
    h: &Recording,
    class: &str,
    loads: &[VerifierLinkLoad],
    reflected: &[String],
) -> Vec<String> {
    fn call(h: &Recording, name: &str, seen: &mut HashSet<String>, out: &mut Vec<String>) {
        if !seen.insert(name.to_string()) {
            return;
        }
        out.push(name.to_string());
        if h.corpus.contains(name) {
            if let Some(info) = h.map.get(name) {
                for i in &info.interfaces {
                    call(h, i, seen, out);
                }
                if let Some(s) = &info.super_class {
                    call(h, s, seen, out);
                }
            }
        }
    }
    let mut seen: HashSet<String> = HashSet::new();
    seen.insert(class.to_string());
    if let Some(info) = h.map.get(class) {
        if let Some(s) = &info.super_class {
            seen.insert(s.to_string());
        }
        seen.extend(info.interfaces.iter().map(|i| i.to_string()));
    }
    let mut out = Vec::new();
    for load in loads {
        match load {
            VerifierLinkLoad::Name(n) => call(h, n, &mut seen, &mut out),
            VerifierLinkLoad::SourceUnlessInterface { target, source } => {
                if !h.map.get(&**target).is_some_and(|i| i.is_interface) {
                    call(h, source, &mut seen, &mut out);
                }
            }
        }
    }
    for n in reflected {
        call(h, n, &mut seen, &mut out);
    }
    out
}

/// The classes a method descriptor's parameters name, in order (an array
/// parameter names its element class), each once.
fn descriptor_parameter_classes(descriptor: &str) -> Vec<String> {
    let params = descriptor
        .strip_prefix('(')
        .and_then(|d| d.split(')').next())
        .unwrap_or("");
    let mut out: Vec<String> = Vec::new();
    let mut rest = params;
    while let Some(c) = rest.chars().next() {
        if c == 'L' {
            let end = rest.find(';').expect("a class descriptor ends with ';'");
            let n = rest[1..end].to_string();
            if !out.contains(&n) {
                out.push(n);
            }
            rest = &rest[end + 1..];
        } else {
            rest = &rest[1..]; // a primitive or an array marker
        }
    }
    out
}

/// The parameter classes of `cf`'s declared constructors, in declaration
/// order: what the probe's `getDeclaredConstructors` loads after the link.
fn constructor_parameter_classes(cf: &ClassFile) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for m in cf.methods.iter().filter(|m| &*m.name == "<init>") {
        for n in descriptor_parameter_classes(&m.descriptor) {
            if !out.contains(&n) {
                out.push(n);
            }
        }
    }
    out
}

/// The `loadClass` calls the probe sees after the define for the corpus class
/// `name` under the one-walk route: the link's, then its reflection's.
fn one_walk_calls(h: &Recording, bytes: &HashMap<String, Vec<u8>>, name: &str, id: u32) -> Vec<String> {
    let cf = read_decoded(bytes.get(name).unwrap_or_else(|| panic!("{name} in the corpus")))
        .expect("parses");
    let reflected = constructor_parameter_classes(&cf);
    let class = make_class(ClassId::new(id), cf);
    h.begin(name);
    verify_user_loader_class_bytecode_recording(&class, h)
        .unwrap_or_else(|e| panic!("{name} verifies: {e}"));
    let loads = h.take();
    replay(h, name, &loads, &reflected)
}

#[test]
fn descriptor_parameter_classes_reads_class_and_array_parameters() {
    assert_eq!(
        descriptor_parameter_classes("(ILjava/lang/reflect/InvocationHandler;[[Ljava/lang/String;J)V"),
        vec!["java/lang/reflect/InvocationHandler".to_string(), "java/lang/String".to_string()]
    );
    assert!(descriptor_parameter_classes("()V").is_empty());
}

/// HotSpot's LINK lines, exact order. `InlineCacheTest` and `$Proxy0` are
/// HotSpot 25.0.4.1 on Linux, stable across 12 runs (CDS on and off, `-Xint`,
/// SerialGC): HotSpot verifies its methods in `Symbol`-address order, which
/// puts `<init>` and `<clinit>` first on every run and the rest in an
/// allocator-dependent order (Windows Temurin 25.0.3 put `testDeepHierarchy`
/// early, and `-Xshare:off` reorders it again), so only the initializers-first
/// rule is modelled (`CRATONVM_VERIFY_INITIALIZERS_FIRST`) and these classes'
/// other methods are in class-file order on Linux.
#[test]
fn one_walk_link_loads_match_hotspot_in_order() {
    let (h, bytes) = hierarchy();
    let cases: [(&str, &[&str]); 5] = [
        (
            "cratonvm/TckLoading",
            &[
                "cratonvm/TckLoading$Base",
                "cratonvm/TckLoading$Leaf",
                "cratonvm/TckLoading$Mid",
            ],
        ),
        (
            "cratonvm/TckLang",
            &[
                "java/lang/CharSequence",
                "java/lang/Exception",
                "java/lang/RuntimeException",
                "java/lang/Throwable",
                "java/lang/IllegalArgumentException",
            ],
        ),
        (
            "cratonvm/RealFjp",
            &[
                "java/lang/Throwable",
                "java/util/concurrent/ExecutionException",
                "java/util/concurrent/Future",
                "java/util/List",
                "java/util/Collection",
                "java/util/concurrent/ExecutorService",
                "java/lang/IllegalStateException",
            ],
        ),
        (
            "cratonvm/InlineCacheTest",
            &[
                "cratonvm/InlineCacheTest$Shape",
                "cratonvm/InlineCacheTest$Circle",
                "cratonvm/InlineCacheTest$Square",
                "cratonvm/InlineCacheTest$Computable",
                "java/lang/Throwable",
                "java/lang/Exception",
                "java/lang/Runnable",
                "cratonvm/InlineCacheTest$Animal",
                "cratonvm/InlineCacheTest$Dog",
                "cratonvm/InlineCacheTest$Mammal",
                "cratonvm/InlineCacheTest$Cat",
            ],
        ),
        (
            // `<clinit>` (verified second, after `<init>`) before the four
            // `Object` overrides: its handlers' and throws' loads come first.
            "jdk/proxy1/$Proxy0",
            &[
                "java/lang/Throwable",
                "java/lang/NoSuchMethodException",
                "java/lang/ClassNotFoundException",
                "java/lang/NoSuchMethodError",
                "java/lang/NoClassDefFoundError",
                "java/lang/Error",
                "java/lang/RuntimeException",
                "java/lang/reflect/UndeclaredThrowableException",
                "java/lang/IllegalAccessException",
                // Not a verifier load: the probe's getDeclaredConstructors
                // resolves `<init>(InvocationHandler)` (see [`replay`]).
                "java/lang/reflect/InvocationHandler",
            ],
        ),
    ];
    for (i, (class, hotspot)) in cases.iter().enumerate() {
        let id = 0x0092_0000 + u32::try_from(i).expect("small");
        let got = one_walk_calls(&h, &bytes, class, id);
        let want: Vec<String> = hotspot.iter().map(|s| s.to_string()).collect();
        assert_eq!(got, want, "{class}: one-walk link loads vs HotSpot 25");
    }
}
