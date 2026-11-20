#[allow(unused_imports)]
use super::*;

/// Default soft cap for [`ClassManager::class_bytes_cache`]. 16 MiB.
///
/// Empirically covers JVMTI / `getResourceAsStream` re-fetch on the agents
/// we ship — those overwhelmingly target *recently-defined* classes (the
/// FIFO retention window). Apps with large agents that retransform
/// already-old classes should raise this via
/// [`ClassManager::set_class_bytes_cache_cap`].
pub const DEFAULT_CLASS_BYTES_CACHE_CAP: usize = 16 * 1024 * 1024;

/// Default bound on the pinned retransformation bases of
/// [`ClassManager::class_bytes_cache`] (`ClassManager::insert_class_bytes_pinned`):
/// class files the class path cannot give back. 128 MiB. Nothing is pinned on a
/// run with no agent that transforms or redefines; a coverage agent that
/// instruments every class at load pins one file per class, as HotSpot keeps
/// one (`InstanceKlass::_cached_class_file`, unbounded there).
pub const DEFAULT_PINNED_CLASS_BASES_CAP: usize = 128 * 1024 * 1024;

/// [`ClassOrigin::CompatibilityStub`] reason for the `ensure_*_synthetic_class`
/// family: a class the VM was *told* to conjure, with no class file consulted
/// at all. Distinct from the three `load_class`-chain reasons below, which all
/// mean "we looked for real bytes and did not find them".
pub(in crate::class_manager) const ENSURE_SYNTHETIC_STUB_REASON: &str =
    "VM-requested stand-in: ensure_synthetic_class called with no class file on any classpath entry";

/// The refusal [`ClassManager::fabricate_class`] returns when it is asked to
/// mint a compatibility stand-in for a name that two or more **distinct**
/// classes already carry.
///
/// # Why this is a `LinkageError` and not a `ClassNotFound`
///
/// `ClassFileError::ClassNotFound` is what the `--real-jdk` policy refusal
/// returns, and it is also what "absent" looks like everywhere else in this
/// file. Reusing it here would put ambiguity back on top of absence at the call
/// site — the exact collapse [`ClassManager::classify_loaded_name`] exists to
/// undo, one layer up. A caller must be able to tell "there is no such class,
/// go and load one" from "there are two, and you may not pick", because only
/// the second is fixed by re-asking with an initiating loader.
///
/// `IncompatibleClassChangeError` is the `LinkageError` family HotSpot uses for
/// loader-constraint violations, which is the same disease: one name, two
/// runtime types.
pub(in crate::class_manager) fn ambiguous_stand_in_refused(name: &str, definitions: usize) -> VmError {
    VmError::Linkage(LinkageError::IncompatibleClassChangeError {
        message: format!(
            "ambiguous class name {name}: {definitions} distinct classes are already defined \
             under it, so a synthetic stand-in filed under the bootstrap loader would shadow \
             every one of them; refusing to fabricate. Re-ask with an initiating loader \
             (ClassManager::find_class_by_name_for_loader / find_class_by_name_for_class)."
        ),
    })
}

/// Reason for the `java/lang/ProcessHandle` / `ProcessHandle$Info` exemption:
/// the host JDK image may not carry these class files, but CratonVM implements
/// their API through native bridges, so `load_class` fabricates a carrier for
/// the native surface.
pub(in crate::class_manager) const NATIVE_BACKED_STUB_REASON: &str =
    "native-backed JDK stub: no class file in the boot image; the API is supplied by native bridges";

/// Reason for the enterprise-prefix fallback (`org/jboss`, `io/quarkus`,
/// `io/smallrye`, …): a dependency jar is missing from the classpath and a
/// skeleton is fabricated so unrelated bytecode still links.
pub(in crate::class_manager) const ENTERPRISE_PREFIX_STUB_REASON: &str =
    "enterprise-prefix fallback: no class file on any classpath entry (add the missing jar)";

/// Reason for the residual case: a JDK-namespace name with no class file and
/// no boot image to have found it in (synthetic-JDK mode, or a `ClassManager`
/// built with an empty boot classpath).
pub(in crate::class_manager) const MISSING_CLASS_FILE_STUB_REASON: &str = "no class file on any classpath entry";

/// Storage type for the `(loader_id, class_name) → ClassId` index.
///
/// C34 audit fix (HIGH): switched from `rustc_hash::FxHashMap`
/// (std HashMap) to `hashbrown::HashMap` so the hot-path lookup helper
/// [`loaded_classes_probe`] can use the stable `raw_entry` API to probe
/// a `(ClassLoaderId, &str)` key against the stored
/// `(ClassLoaderId, Arc<str>)` key without allocating a fresh
/// `Arc<str>` per call. Insert / remove / iter sites are API-compatible
/// (hashbrown's `HashMap` is the implementation underlying std's).
pub(in crate::class_manager) type LoadedClassesMap =
    hashbrown::HashMap<(ClassLoaderId, Arc<str>), ClassId, crate::fx_hash::FxBuildHasher>;

/// Hash a borrowed `(ClassLoaderId, &str)` lookup key against the given
/// `FxBuildHasher`, producing a hash byte-identical to what the owned
/// `(ClassLoaderId, Arc<str>)` storage key would produce.
///
/// Soundness: the tuple `Hash` impl walks each field in order; `Arc<str>`
/// derefs to `str` and `Hash for str` walks the same bytes as a `&str`,
/// so the borrowed and owned forms yield identical hashes.
#[inline]
pub(in crate::class_manager) fn hash_loaded_classes_key(
    hasher: &crate::fx_hash::FxBuildHasher,
    loader_id: ClassLoaderId,
    name: &str,
) -> u64 {
    use std::hash::{BuildHasher, Hash, Hasher};
    let mut h = hasher.build_hasher();
    loader_id.hash(&mut h);
    name.hash(&mut h);
    h.finish()
}

/// Zero-allocation probe into [`LoadedClassesMap`] by borrowed
/// `(ClassLoaderId, &str)` key. Returns `Some(class_id)` on hit.
///
/// C34 audit fix (HIGH): replaces the round-9-CRIT-2-half-fixed pattern
/// of `Arc::from(name)` (or `intern_arc(name)`) followed by `HashMap::get`
/// — both of which paid a per-call allocation (`Arc::from`) or a global
/// pool lock (`intern_arc`) on every dynamic dispatch / `Class.forName`
/// / `Constant_Class` resolution. Uses hashbrown's stable `raw_entry`
/// API to compute the hash once and walk the bucket via a custom
/// equality closure: cache hit costs a hash + one or two pointer / byte
/// comparisons, with no `Arc<str>` ever materialised.
#[inline]
pub(in crate::class_manager) fn loaded_classes_probe(
    map: &LoadedClassesMap,
    loader_id: ClassLoaderId,
    name: &str,
) -> Option<ClassId> {
    let hash = hash_loaded_classes_key(map.hasher(), loader_id, name);
    map.raw_entry()
        .from_hash(hash, |(k_loader, k_name)| {
            *k_loader == loader_id && k_name.as_ref() == name
        })
        .map(|(_, &id)| id)
}

/// Probe `name` against the delegation ANCESTORS of a user-defined loader
/// (nearest parent first), skipping the loader's own namespace and the
/// built-in chain — both of which every caller probes separately.
///
/// This is the Rust-side half of parent delegation for *resolution*, as opposed
/// to *finding bytes*: see `loaders::UserLoaderParents` for why it did not
/// exist before 2026-07-30 and what its absence caused. `parents` is the
/// VM's own table (`ClassManager::user_loader_parents`).
pub(in crate::class_manager) fn loaded_class_via_parent_chain(
    map: &LoadedClassesMap,
    parents: &crate::loaders::UserLoaderParents,
    requesting_loader: ClassLoaderId,
    name: &str,
) -> Option<ClassId> {
    if !crate::loaders::loader_parent_chain_enabled() || !parents.has_any() {
        return None;
    }
    let ClassLoaderId::UserDefined(ns) = requesting_loader else {
        return None;
    };
    let mut ancestors = [0u32; crate::loaders::MAX_USER_LOADER_DEPTH];
    let n = parents.ancestors(ns, &mut ancestors);
    for &parent in &ancestors[..n] {
        if let Some(id) = loaded_classes_probe(map, ClassLoaderId::UserDefined(parent), name) {
            if crate::loaders::dbg_loader_chain() {
                eprintln!(
                    "[loader-chain] {name}: ns {ns} resolved through parent ns {parent} -> cid {}",
                    id.as_u32()
                );
            }
            return Some(id);
        }
    }
    None
}

/// Resolve an already-defined class through one requesting loader's reachable
/// namespaces. Built-in loaders are strictly parent-first and never delegate
/// down to a child. A user loader may prefer its own definition when its Java
/// `loadClass` policy is known to be child-first; otherwise its simplified
/// parent is the complete built-in chain.
#[inline]
pub(in crate::class_manager) fn loaded_class_for_requesting_loader(
    map: &LoadedClassesMap,
    parents: &crate::loaders::UserLoaderParents,
    requesting_loader: ClassLoaderId,
    name: &str,
    user_own_first: bool,
) -> Option<ClassId> {
    match requesting_loader {
        ClassLoaderId::Bootstrap | ClassLoaderId::Extension | ClassLoaderId::Application => {
            for loader_id in BUILTIN_LOADER_DELEGATION_CHAIN {
                if let Some(id) = loaded_classes_probe(map, *loader_id, name) {
                    return Some(id);
                }
                if *loader_id == requesting_loader {
                    break;
                }
            }
            None
        }
        ClassLoaderId::UserDefined(ns) => {
            if user_own_first {
                if let Some(id) = loaded_classes_probe(map, requesting_loader, name) {
                    return Some(id);
                }
            }
            // A registered user-loader ANCESTOR outranks the built-in chain:
            // the built-in chain is this loader's parent only when nothing
            // closer was recorded. Probing it first is what let a fork
            // loader's child resolve the application copy of a name its own
            // parent had already defined.
            if let Some(id) = loaded_class_via_parent_chain(map, parents, requesting_loader, name) {
                return Some(id);
            }
            // The built-in chain this loader's delegation may reach is
            // bounded by its OWN recorded terminal parent, not always the
            // whole chain. A `ModifiedClassPathClassLoader` (parent =
            // platform/Extension, specifically to exclude Application — e.g.
            // Spring's `@ClassPathExclusions`) must stop at Extension: probing
            // Application anyway resolves a same-named class through the
            // wrong loader, which is how a `PropertiesPropertySource` built
            // by isolated-loader code ended up an `Application`-loaded
            // instance while its own compiled `checkcast` site (correctly)
            // named the isolated loader's `EnumerablePropertySource` —
            // `ClassCastException` between two genuinely different classes.
            // `None` (parent unrecorded, or the chain did not bottom out) is
            // the permissive default: probe every built-in loader, as before.
            let reachable: &[ClassLoaderId] = match parents.builtin_parent(ns) {
                Some(terminal) => {
                    let end = (terminal as usize + 1).min(BUILTIN_LOADER_DELEGATION_CHAIN.len());
                    &BUILTIN_LOADER_DELEGATION_CHAIN[..end]
                }
                None => BUILTIN_LOADER_DELEGATION_CHAIN,
            };
            for loader_id in reachable {
                if let Some(id) = loaded_classes_probe(map, *loader_id, name) {
                    return Some(id);
                }
            }
            if user_own_first {
                None
            } else {
                loaded_classes_probe(map, requesting_loader, name)
            }
        }
    }
}

/// Drop one reference to `(name -> id)` from the name index, retiring the id
/// (and then the name) once nothing points at it any more.
pub(in crate::class_manager) fn release_name_definition(
    index: &mut FxHashMap<Arc<str>, FxHashMap<ClassId, u32>>,
    name: &str,
    id: ClassId,
) {
    let Some(ids) = index.get_mut(name) else {
        return;
    };
    if let Some(count) = ids.get_mut(&id) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            ids.remove(&id);
        }
    }
    if ids.is_empty() {
        index.remove(name);
    }
}


/// `CRATONVM_LOADER_AWARE_RESOLUTION` gate (default ON) — **the single
/// source of truth**. `vm::runtime::env_cache::loader_aware_resolution` and
/// the `native-builtins::classloader::loader_aware_resolution` twin both now
/// delegate here (see their doc comments) instead of re-parsing the env var
/// through their own `OnceLock`, so the three copies cannot drift again.
///
/// **History (why this consolidation happened):** this crate's copy had
/// drifted out of lock-step with the VM copy — it stayed default **OFF**
/// after `env_cache::loader_aware_resolution` flipped to default **ON** for
/// the `context.groovy` bug-cluster fix, which silently disabled this
/// crate's share of the loader-faithful fixes (superclass/interface
/// linking, verifier hierarchy lookup) by default even though the
/// interpreter half of the same fix was live — a production desync between
/// three independently-read env-var copies. See
/// `hib-bytecode-enhancement-loader-faithful-linking-FIXED.md`
/// and `loader-identity.md` for the consolidation. Flip on
/// links an enhanced subclass to its same-loader (enhanced) supertype copy
/// rather than the un-enhanced global one returned by `get_loaded_class_id`.
///
/// Empty / `"0"` ⇒ off; any other value (including unset) ⇒ on. Read once
/// and cached in a `OnceLock` — the env var is not re-read after first use.
pub fn loader_aware_resolution() -> bool {
    loader_flags().loader_aware_resolution
}

/// Diagnostic-only gate mirroring `CRATONVM_TRACE_UNIMPLEMENTED` (see
/// `vm/src/vm/vm_exec.rs`): when set, print one line per enterprise-prefix
/// class (`io/smallrye/`, `io/quarkus/`, `org/jboss/`, …) that falls back to
/// an empty synthetic stub because it was not found on any classpath entry.
/// This case is otherwise invisible in release builds — the `tracing` crate
/// here is built with `max_level_info`, so the existing `debug!` call just
/// above `create_synthetic_stub` never executes — and a stubbed class whose
/// methods are later invoked surfaces only as a confusing `NoSuchMethodError`
/// (or, if the gap breaks a superinterface/superclass resolution, a bare
/// `NoClassDefFoundError` with no further detail). Off by default to avoid
/// spamming normal runs.
/// Cached `CRATONVM_DBG_STUB_BT` filter (see the synthetic-stub fallback in
/// [`ClassManager::load_class`]).
pub(in crate::class_manager) fn dbg_stub_bt_filter() -> Option<&'static str> {
    static FILTER: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    FILTER
        .get_or_init(|| Err::<String, std::env::VarError>(std::env::VarError::NotPresent).ok())
        .as_deref()}

pub(in crate::class_manager) fn trace_stub_fallback() -> bool {
    loader_flags().trace_unimplemented
}

/// H5 (HIGH): return `true` if `internal_name` (a `/`-separated internal
/// class name) lives in a runtime package that only the bootstrap loader
/// is permitted to define classes into.
///
/// This is HotSpot 25's protected namespace, `java/` only: a loader other than
/// boot or platform that defines a class here is attempting to masquerade as
/// core-library code. HotSpot enforces exactly this set via
/// `ClassLoader.preDefineClass` and `SystemDictionary::check_prohibited_package`
/// ("Prohibited package name: java.*"); `sun.*` and `jdk.internal.*` are NOT
/// refused at define time there (i4 wave 97).
///
/// Matching is on package *boundaries* (`prefix` exactly, or `prefix/...`)
/// so a benign top-level class such as `javax/Foo` or a user package like
/// `javaland/Foo` is not falsely rejected.
pub(in crate::class_manager) fn is_prohibited_package_name(internal_name: &str) -> bool {
    // Interpreter round i4 wave 97 (lane L3; probe
    // `L3/L3W96CallerSensitiveByLoader`, row `user-sun`): HotSpot 25 prohibits
    // ONLY `java/` for a non-boot, non-platform loader -- `ClassLoader.
    // preDefineClass` (`name.startsWith("java.")`) and `SystemDictionary::
    // check_prohibited_package` (`java/` + `/`). A user loader defining
    // `sun.l3w96.Sensitive` or a `jdk.internal.*` class succeeds there; the
    // `sun/` and `jdk/internal/` prefixes this guard used to add refused it
    // with a `SecurityException` HotSpot never throws. Package encapsulation
    // of the real `sun.*` / `jdk.internal.*` packages is the module system's
    // job (a user loader's class lands in its own unnamed module and its own
    // runtime package), not this guard's. The two exemptions below are kept
    // as documentation of the cases that first exposed the over-broad set;
    // they are now subsumed by the `java/`-only rule.
    // Exemption: `sun.reflect.misc.MethodUtil` is a JDK-internal reflection
    // helper (a `SecureClassLoader` subclass in `java.base`) whose sole job is
    // to `defineClass` a companion `sun.reflect.misc.Trampoline` into its own
    // `sun.reflect.misc` package and invoke target methods through it. The JDK
    // reaches this path from `javax.management` (`RequiredModelMBean` /
    // `StandardMBean` reflective attribute + operation dispatch), so blocking
    // it breaks real-JDK JMX. HotSpot's `ClassLoader.preDefineClass` actually
    // only rejects the `java.*` prefix for non-platform loaders — it does NOT
    // reject `sun.*` here (package encapsulation is enforced elsewhere), so
    // this Trampoline define is legitimate. Exempt exactly that package so the
    // broader `sun/**` spoofing guard below still stands.
    if internal_name.starts_with("sun/reflect/misc/") {
        return false;
    }
    // Exemption: `jdk.internal.reflect` is where the JDK generates
    // `GeneratedMethodAccessorN` / `GeneratedConstructorAccessorN` /
    // `GeneratedSerializationConstructorAccessorN` classes — for reflective
    // `Method.invoke`/`Constructor.newInstance` past the inflation threshold,
    // and (unconditionally, no threshold) for `ObjectStreamClass`'s
    // serialization constructor, which must invoke the nearest
    // non-serializable superclass's no-arg constructor and has no other way
    // to do so. `jdk.internal.reflect.ClassDefiner.defineClass` performs this
    // define through a throwaway `DelegatingClassLoader` — NOT the bootstrap
    // loader. Verified on real JDK 21: this define succeeds there, so
    // blocking it here is a false rejection, not a security fix — it broke
    // ALL first-use reflective invocation past inflation and ALL
    // serialization of a class whose nearest non-serializable ancestor has
    // any constructor logic, with `NoClassDefFoundError: IllegalName: ...`
    // (when routed through real bytecode `preDefineClass`) or `SecurityException:
    // Prohibited package name` (native path) — both symptoms of this same
    // over-broad prefix.
    if internal_name.starts_with("jdk/internal/reflect/") {
        return false;
    }
    const PROHIBITED_PREFIXES: [&str; 1] = ["java"];
    for prefix in PROHIBITED_PREFIXES {
        if let Some(rest) = internal_name.strip_prefix(prefix) {
            // Exact prefix as a full package segment must be followed by a
            // `/` (i.e. there is at least a class name after the package).
            // `internal_name == prefix` (no `/`) is a default-package class
            // literally *named* "java"/"sun" — not in the package, so allow.
            if rest.starts_with('/') {
                return true;
            }
        }
    }
    false
}

/// Round 9 wave 6 (`vmhelp6`): whether a PRIVILEGED define (`Unsafe.defineClass`,
/// `JavaLangAccess.defineClass`) of `internal_name` into the non-bootstrap
/// `loader_id` forges a core `java/` class. HotSpot refuses every `java/` class
/// from a loader other than boot/platform; CratonVM's native bridges map a null
/// loader to `Application`, so the rule is applied to what can be told apart: a
/// genuine `UserDefined` loader, or a name the bootstrap loader has already
/// defined (`bootstrap_has_name`), i.e. a duplicate of a loaded core class. A
/// brand-new `java/` name under `Application`/`Extension` (ByteBuddy's accessor
/// injection, JDK species classes) stays allowed.
pub(in crate::class_manager) fn privileged_define_forges_core_class(
    internal_name: &str,
    loader_id: ClassLoaderId,
    bootstrap_has_name: bool,
) -> bool {
    if !internal_name.starts_with("java/") {
        return false;
    }
    matches!(loader_id, ClassLoaderId::UserDefined(_)) || bootstrap_has_name
}

/// HotSpot's `IncompatibleClassChangeError` message for a class `this_class`
/// (internal name) whose direct superclass is an interface or final, or one
/// of whose direct superinterfaces is a class (JVMS §5.3.5 steps 3-4; the
/// final-superclass check is `ClassFileParser`'s). `None` when the shapes are
/// legal. A supertype without real class bytes is not judged: a compatibility
/// stub's access flags are fabricated. The `can not implement` message ends
/// with HotSpot's `(<C> is in <module> of loader <loader>)` clause
/// (`Klass::class_in_module_of_loader`), which `clause` renders for `<C>`
/// (interpreter round i7 wave 141, lane L5; `CRATONVM_SUPERTYPE_SHAPE_MODULE_CLAUSE`,
/// default ON, `=0` drops the clause as before; probe `L5W141LoaderHunt`
/// row `d-impl-class`). `clause` answers `None` to leave it out.
pub(in crate::class_manager) fn supertype_shape_violation<'a>(
    this_class: &str,
    superclass: Option<&Class>,
    interfaces: impl Iterator<Item = &'a Class>,
    clause: impl Fn(&Class) -> Option<String>,
) -> Option<String> {
    let dotted = |name: &str| name.replace('/', ".");
    if let Some(sup) = superclass.filter(|c| c.origin.has_real_bytes()) {
        if sup.is_interface() {
            return Some(format!(
                "class {} has interface {} as super class",
                dotted(this_class),
                dotted(&*sup.name)
            ));
        }
        if sup.is_final() {
            return Some(format!(
                "class {} cannot inherit from final class {}",
                dotted(this_class),
                dotted(&*sup.name)
            ));
        }
    }
    for iface in interfaces {
        if iface.origin.has_real_bytes() && !iface.is_interface() {
            let mut message = format!(
                "class {} can not implement {}, because it is not an interface",
                dotted(this_class),
                dotted(&*iface.name)
            );
            if let Some(text) = clause(iface) {
                message.push_str(" (");
                message.push_str(&text);
                message.push(')');
            }
            return Some(message);
        }
    }
    None
}



/// The internal-form package of `name` (`""` for the unnamed package).
pub(in crate::class_manager) fn package_of_internal(name: &str) -> &str {
    name.rsplit_once('/').map(|(p, _)| p).unwrap_or("")
}

/// HotSpot `ClassFileParser::check_final_method_override`: the
/// `IncompatibleClassChangeError` message for a class `this_class` (internal
/// name, defined by `loader`) one of whose non-private, non-static methods
/// other than `<init>` overrides a final, non-static, non-private method that
/// a superclass declares and that the class can access (public, protected, or
/// package-private in the same runtime package: same loader and package):
/// `class <C> overrides final method <D>.<name><descriptor>`, `C` and the
/// DECLARING class `D` in external form. Every superclass is asked, nearest
/// first (HotSpot continues from each match's holder's superclass). A
/// superclass without real class bytes is not judged (a compatibility stub's
/// flags are fabricated), and `jdk.jfr.Event`'s five instrumented methods are
/// exempt as before. Interpreter round i4 wave 84, lane L5 (probe
/// `L5W84FinalOverride`): the check lived only in the link-time structural
/// verifier, which a user-defined loader's class skips, and said
/// `VerifyError: ... cannot override final method <direct superclass>...`.
pub(in crate::class_manager) fn final_method_override_violation(
    this_class: &str,
    loader: ClassLoaderId,
    methods: &[ClassFileMethod],
    superclass: Option<ClassId>,
    store: &ClassStore,
) -> Option<String> {
    // The superclasses' final instance methods, nearest first -- usually a
    // handful, so each of the class's methods is compared with those alone
    // (HotSpot skips a superclass with no final method the same way).
    let mut finals: Vec<(&Class, &ClassFileMethod)> = Vec::new();
    let mut current = superclass;
    let mut hops = 0usize;
    while let Some(id) = current {
        hops += 1;
        if hops > 4096 {
            break;
        }
        let Some(ancestor) = store.get(id) else {
            break;
        };
        if ancestor.origin.has_real_bytes() {
            finals.extend(
                ancestor
                    .methods
                    .iter()
                    .filter(|m| {
                        m.access_flags.contains(MethodAccessFlags::FINAL)
                            && !m
                                .access_flags
                                .intersects(MethodAccessFlags::STATIC | MethodAccessFlags::PRIVATE)
                    })
                    .map(|m| (ancestor, m)),
            );
        }
        current = ancestor.superclass;
    }
    if finals.is_empty() {
        return None;
    }
    for method in methods {
        let flags = method.access_flags;
        if flags.intersects(MethodAccessFlags::PRIVATE | MethodAccessFlags::STATIC)
            || &*method.name == "<init>"
        {
            continue;
        }
        for &(ancestor, found) in &finals {
            if found.name != method.name || found.descriptor != method.descriptor {
                continue;
            }
            let jfr_exempt = &*ancestor.name == "jdk/jfr/Event"
                && matches!(
                    &*method.name,
                    "begin" | "end" | "commit" | "isEnabled" | "shouldCommit"
                );
            let package_private = !found
                .access_flags
                .intersects(MethodAccessFlags::PUBLIC | MethodAccessFlags::PROTECTED);
            let accessible = !package_private
                || (ancestor.loader_id == loader
                    && package_of_internal(&ancestor.name) == package_of_internal(this_class));
            if accessible && !jfr_exempt {
                return Some(format!(
                    "class {} overrides final method {}.{}{}",
                    this_class.replace('/', "."),
                    ancestor.name.replace('/', "."),
                    method.name,
                    method.descriptor
                ));
            }
        }
    }
    None
}

/// `JVM_RECOGNIZED_CLASS_MODIFIERS`: the class `access_flags` bits HotSpot's
/// `ClassFileParser` keeps (public, final, super, interface, abstract,
/// synthetic, annotation, enum); `ACC_MODULE` (`0x8000`) joins them from
/// class file version 53.
pub(in crate::class_manager) const RECOGNIZED_CLASS_MODIFIERS: u16 = 0x7631;
/// `JVM_RECOGNIZED_FIELD_MODIFIERS` (public, private, protected, static,
/// final, volatile, transient, synthetic, enum).
pub(in crate::class_manager) const RECOGNIZED_FIELD_MODIFIERS: u16 = 0x50DF;
/// `RECOGNIZED_INNER_CLASS_MODIFIERS` (`classFileParser.cpp`): the class
/// modifiers plus private, protected and static.
pub(in crate::class_manager) const RECOGNIZED_INNER_CLASS_MODIFIERS: u16 = RECOGNIZED_CLASS_MODIFIERS | 0x000E;

/// zs s4/rp: the class `access_flags` HotSpot's `ClassFileParser` STORES, and
/// so every reflective answer derived from them (`Class.getModifiers`, the
/// mirror's `modifiers` field, `Reflection.getClassAccessFlags`, JVMTI /
/// JDWP class modifiers): the class file's flags masked to
/// `JVM_RECOGNIZED_CLASS_MODIFIERS` (`| ACC_MODULE` from version 53), with
/// `ACC_ABSTRACT` added to a pre-50 interface. An unrecognized bit is
/// dropped: jlink's `GenerateJLIClassesPlugin` writes the four
/// `java.lang.invoke.*$Holder` classes of `lib/modules` with
/// `ACC_PRIVATE | ACC_FINAL | ACC_SUPER`, which HotSpot answers as `final`
/// (16) and this VM answered as `private final` (18).
pub(crate) fn hotspot_stored_class_flags(raw: u16, major: u16) -> u16 {
    let mut flags = raw & (RECOGNIZED_CLASS_MODIFIERS | if major >= 53 { 0x8000 } else { 0 });
    if flags & 0x0200 != 0 && major < 50 {
        flags |= 0x0400;
    }
    flags
}

/// zs s4/rp: an `InnerClasses` entry's `inner_class_access_flags` as HotSpot's
/// `ClassFileParser` stores them (`RECOGNIZED_INNER_CLASS_MODIFIERS`,
/// `| ACC_MODULE` from version 53, `ACC_ABSTRACT` added to a pre-50
/// interface). See [`hotspot_stored_class_flags`].
pub(crate) fn hotspot_stored_inner_class_flags(raw: u16, major: u16) -> u16 {
    let mut flags = raw & (RECOGNIZED_INNER_CLASS_MODIFIERS | if major >= 53 { 0x8000 } else { 0 });
    if flags & 0x0200 != 0 && major < 50 {
        flags |= 0x0400;
    }
    flags
}

/// [`hotspot_stored_inner_class_flags`] over every entry of a class's
/// `InnerClasses`.
pub(in crate::class_manager) fn hotspot_stored_inner_classes(
    mut entries: Vec<crate::class::InnerClassEntry>,
    major: u16,
) -> Vec<crate::class::InnerClassEntry> {
    for entry in &mut entries {
        entry.access_flags = hotspot_stored_inner_class_flags(entry.access_flags, major);
    }
    entries
}

/// HotSpot compares the stored (masked) rows for duplicates, rather than
/// their original raw flag words. Keep the common already-normalized path
/// allocation-free.
pub(in crate::class_manager) fn stored_inner_classes_verdict(
    entries: &[cratonvm_reader::attribute::InnerClassInfo],
    cp: &ConstantPool,
    major: u16,
) -> cratonvm_reader::attribute::InnerClassesVerdict {
    use std::borrow::Cow;
    let entries = if entries.len() > 1 && entries.iter().any(|e|
        hotspot_stored_inner_class_flags(e.inner_class_access_flags, major) != e.inner_class_access_flags)
    {
        let mut normalized = entries.to_vec();
        for entry in &mut normalized {
            entry.inner_class_access_flags = hotspot_stored_inner_class_flags(entry.inner_class_access_flags, major);
        }
        Cow::Owned(normalized)
    } else { Cow::Borrowed(entries) };
    let verdict = cratonvm_reader::attribute::inner_classes_verdict(&entries, cp);
    if major < 49 && verdict == cratonvm_reader::attribute::InnerClassesVerdict::DuplicateEntry {
        cratonvm_reader::attribute::InnerClassesVerdict::Ignore
    } else { verdict }
}

/// The class flag set, by a name no `use` in this file shadows.
pub(in crate::class_manager) type ClassAccessFlagsBits = cratonvm_reader::class_access_flags::ClassAccessFlags;

/// More than one of public / protected / private (`has_illegal_visibility`).
pub(in crate::class_manager) fn has_illegal_visibility(flags: u16) -> bool {
    (flags & 0x0007).count_ones() > 1
}

/// HotSpot `ClassFileParser`'s class-level `access_flags` refusals, raised
/// right after the constant pool, before `this_class` is read (so before the
/// wrong-name check), naming the REQUESTED class (`<Unknown>` for none):
///
/// * `ACC_MODULE` (version 53+), in every loader: `NoClassDefFoundError:
///   <name> is not a class because access_flag ACC_MODULE is set`;
/// * with `need_verify` (any loader but the boot loader,
///   `BytecodeVerificationRemote`), `verify_legal_class_modifiers`:
///   `ClassFormatError: Illegal class modifiers in class <name>: 0x<FLAGS>`
///   for abstract+final, an interface without abstract, an interface with
///   super or enum, or an annotation that is not an interface (the last two
///   from version 49), the flags masked to the recognized set.
///
/// Interpreter round i4 wave 84, lane L5 (probe `L5W84ClassFileModifiers`).
/// Before it the module flag was not looked at (a `module-info.class` handed
/// to `defineClass` became a class), and the other shapes surfaced, if at
/// all, from the link-time structural verifier as a `VerifyError` or a
/// `ClassFormatError` in this VM's own words -- and never for a class a
/// user-defined loader defined, whose structural pass is deferred.
pub(in crate::class_manager) fn class_modifier_refusal(
    class_file: &cratonvm_reader::ClassFile,
    requested: &str,
    need_verify: bool,
) -> Option<LinkageError> {
    let major = class_file.version.major;
    let raw = class_file.access_flags.bits();
    let flags = raw & (RECOGNIZED_CLASS_MODIFIERS | if major >= 53 { 0x8000 } else { 0 });
    legal_class_modifier_refusal(flags, major, requested, need_verify)
}

/// Shared by the class header and each masked InnerClasses row. Inner rows
/// retain private/protected/static bits in the diagnostic, unlike the header.
pub(in crate::class_manager) fn legal_class_modifier_refusal(
    flags: u16,
    major: u16,
    requested: &str,
    need_verify: bool,
) -> Option<LinkageError> {
    let named = if requested.is_empty() {
        "<Unknown>"
    } else {
        requested
    };
    if flags & 0x8000 != 0 {
        return Some(LinkageError::NoClassDefFoundError {
            class_name: format!("{named} is not a class because access_flag ACC_MODULE is set"),
        });
    }
    if !need_verify {
        return None;
    }
    // An old (pre-50) interface gets ACC_ABSTRACT for backward compatibility
    // before the check.
    let flags = if flags & 0x0200 != 0 && major < 50 {
        flags | 0x0400
    } else {
        flags
    };
    let is_interface = flags & 0x0200 != 0;
    let is_abstract = flags & 0x0400 != 0;
    let is_final = flags & 0x0010 != 0;
    let is_super = flags & 0x0020 != 0;
    let is_enum = flags & 0x4000 != 0;
    let is_annotation = flags & 0x2000 != 0;
    let gte_1_5 = major >= 49;
    let illegal = (is_abstract && is_final)
        || (is_interface && !is_abstract)
        || (is_interface && gte_1_5 && (is_super || is_enum))
        || (!is_interface && gte_1_5 && is_annotation);
    illegal.then(|| LinkageError::ClassFormatError {
        class_name: requested.to_string(),
        message: format!("Illegal class modifiers in class {named}: 0x{flags:X}"),
    })
}

/// Entry checks precede table circularity/duplicate handling. ACC_MODULE is
/// rejected even without verification, just like the header. Entries before
/// an illegal row may already have the both-outer-and-inner refusal.
///
/// Per entry, in HotSpot's `parse_classfile_inner_classes_attribute` order
/// (t1 wave 13, lane I24W; measured on HotSpot 25.0.4.1, probe
/// `T1W13I24WStructuralFormat.java` rows S15-S17, S27,
/// S46-S48), each a `ClassFormatError` with the ` in class file <C>` tail:
/// * in EVERY loader, verified or not: `inner_class_info_index <n> has bad
///   constant type` (not a `CONSTANT_Class`), `outer_class_info_index <n> has
///   bad constant type` (neither 0 nor a `CONSTANT_Class`), `inner_name_index
///   <n> has bad constant type` (neither 0 nor a `CONSTANT_Utf8`). Before wave
///   13 nothing checked them, so a user loader defined such a table;
/// * with `structural` (`_need_verify`): `Class is both outer and inner class`;
/// * with `modifiers_need_verify`: the row's class modifier legality.
pub(in crate::class_manager) fn inner_class_modifier_refusal(
    attributes: &[cratonvm_reader::attribute::LazyAttribute],
    cp: &ConstantPool,
    major: u16,
    name: &str,
    modifiers_need_verify: bool,
    structural: bool,
) -> Option<LinkageError> {
    use cratonvm_reader::constant_pool::ConstantPoolEntry as E;
    let bad_type = |what: &str, index: u16| {
        Some(LinkageError::ClassFormatError {
            class_name: name.to_string(),
            message: format!("{what} {index} has bad constant type in class file {name}"),
        })
    };
    let is_class = |index: u16| matches!(cp.get(index), Some(E::ClassReference { .. }));
    for attr in attributes {
        if let Some(Attribute::InnerClasses(entries)) = attr.as_decoded() {
            for entry in entries {
                if !is_class(entry.inner_class_info_index) {
                    return bad_type("inner_class_info_index", entry.inner_class_info_index);
                }
                if entry.outer_class_info_index != 0 && !is_class(entry.outer_class_info_index) {
                    return bad_type("outer_class_info_index", entry.outer_class_info_index);
                }
                if entry.inner_name_index != 0
                    && !matches!(cp.get(entry.inner_name_index), Some(E::Utf8(_)))
                {
                    return bad_type("inner_name_index", entry.inner_name_index);
                }
                if structural && entry.inner_class_info_index == entry.outer_class_info_index {
                    return Some(LinkageError::ClassFormatError {
                        class_name: name.to_string(),
                        message: format!(
                            "Class is both outer and inner class in class file {name}"
                        ),
                    });
                }
                let flags = hotspot_stored_inner_class_flags(entry.inner_class_access_flags, major);
                if let Some(error) =
                    legal_class_modifier_refusal(flags, major, name, modifiers_need_verify)
                {
                    return Some(error);
                }
            }
        }
    }
    None
}

/// HotSpot `ClassFileParser::verify_legal_class_name` over every
/// `CONSTANT_Class` entry, in pool order, when `need_verify` (a loader other
/// than boot and platform; `_need_verify && !_relax_verify`):
///
/// * `ClassFormatError: Illegal class name "<name>" in class file <requested>`
///   for an empty name, a name with `.`, `;` or `[` in a segment, or with a
///   leading, trailing or doubled `/` (`verify_unqualified_name`, `LegalClass`);
///   from version 57 (`CONSTANT_CLASS_DESCRIPTORS`) a name ending in `;` is
///   checked without its first and last character;
/// * an array descriptor must be one field signature exactly; a bad element
///   class name is `Class name is empty or contains illegal character in
///   descriptor in class file <requested>`.
///
/// `<requested>` is `<Unknown>` without a requested name. Classes older than
/// 49 (HotSpot's stricter Java-identifier rules) and descriptors of more than
/// 255 dimensions are not judged here. Interpreter round i5 wave 106, lane L5
/// (probe `tools/probes/interp/L5/L5W106IllegalClassNames.java`).
pub(in crate::class_manager) fn class_name_refusal(
    class_file: &cratonvm_reader::ClassFile,
    requested: &str,
    need_verify: bool,
) -> Option<LinkageError> {
    if !need_verify {
        return None;
    }
    let major = class_file.version.major;
    if major < 49 {
        return None;
    }
    let named = if requested.is_empty() {
        "<Unknown>"
    } else {
        requested
    };
    let cp = &class_file.constant_pool;
    for index in 1..cp.len() {
        let Ok(index) = u16::try_from(index) else {
            break;
        };
        let Some(class_name) = cp.get_class_name(index) else {
            continue;
        };
        match legal_class_name(class_name, major) {
            ClassNameVerdict::Legal => {}
            ClassNameVerdict::Illegal => {
                return Some(LinkageError::ClassFormatError {
                    class_name: requested.to_string(),
                    message: format!("Illegal class name \"{class_name}\" in class file {named}"),
                });
            }
            ClassNameVerdict::IllegalDescriptorElement => {
                return Some(LinkageError::ClassFormatError {
                    class_name: requested.to_string(),
                    message: format!(
                        "Class name is empty or contains illegal character in descriptor in \
                         class file {named}"
                    ),
                });
            }
        }
    }
    None
}

/// [`legal_class_name`]'s answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::class_manager) enum ClassNameVerdict {
    Legal,
    Illegal,
    IllegalDescriptorElement,
}

/// HotSpot `verify_unqualified_name(name, LegalClass)`: non-empty, no `.`,
/// `;` or `[`, and every `/` between two non-empty segments.
pub(in crate::class_manager) fn unqualified_class_name_is_legal(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'.' | b';' | b'[' => return false,
            b'/' => {
                if i == 0 || i + 1 >= bytes.len() || bytes[i + 1] == b'/' {
                    return false;
                }
            }
            _ => {}
        }
    }
    true
}

/// `verify_legal_class_name` for one `CONSTANT_Class` name of a class file of
/// version `major` (49 or later).
pub(in crate::class_manager) fn legal_class_name(name: &str, major: u16) -> ClassNameVerdict {
    if name.is_empty() {
        return ClassNameVerdict::Illegal;
    }
    let bytes = name.as_bytes();
    if bytes[0] == b'[' {
        let dims = bytes.iter().take_while(|&&b| b == b'[').count();
        if dims > 255 {
            return ClassNameVerdict::Legal;
        }
        let rest = &name[dims..];
        return match rest.as_bytes().first() {
            Some(b'B' | b'C' | b'D' | b'F' | b'I' | b'J' | b'S' | b'Z') if rest.len() == 1 => {
                ClassNameVerdict::Legal
            }
            Some(b'L') => match rest[1..].find(';') {
                Some(end) => {
                    if !unqualified_class_name_is_legal(&rest.as_bytes()[1..1 + end]) {
                        ClassNameVerdict::IllegalDescriptorElement
                    } else if 1 + end + 1 == rest.len() {
                        ClassNameVerdict::Legal
                    } else {
                        ClassNameVerdict::Illegal
                    }
                }
                None => ClassNameVerdict::Illegal,
            },
            _ => ClassNameVerdict::Illegal,
        };
    }
    if major >= 57 && bytes[bytes.len() - 1] == b';' {
        if bytes.len() < 2 {
            return ClassNameVerdict::Illegal;
        }
        return if unqualified_class_name_is_legal(&bytes[1..bytes.len() - 1]) {
            ClassNameVerdict::Legal
        } else {
            ClassNameVerdict::Illegal
        };
    }
    if unqualified_class_name_is_legal(bytes) {
        ClassNameVerdict::Legal
    } else {
        ClassNameVerdict::Illegal
    }
}


/// HotSpot `ClassFileParser`'s field and method refusals that precede
/// supertype resolution, in its order (every field, then every method in
/// declaration order), naming the class file's own `this_class`:
///
/// * with `need_verify`, `verify_legal_field_modifiers`: `Illegal field
///   modifiers in class <name>: 0x<FLAGS>` (masked to the recognized set) for
///   two visibilities or final+volatile, and in an interface for anything but
///   public static final (no private / protected / volatile / transient /
///   enum);
/// * `<clinit>` (version 51+) without `ACC_STATIC`, in every loader:
///   `Method <clinit> is not static in class file <name>`; any other method,
///   with `need_verify`, `verify_legal_method_modifiers`: `Method <m> in
///   class <name> has illegal modifiers: 0x<FLAGS>` (the raw flags);
/// * an interface `<init>`, in every loader: `Interface cannot have a method
///   named <init>, class file <name>`.
///
/// All are `ClassFormatError`s. Interpreter round i4 wave 84, lane L5.
///
/// The member names and descriptors are judged too ([`member_refusal`]),
/// names in the same scope as the modifiers here.
pub(in crate::class_manager) fn member_modifier_refusal(
    class_file: &cratonvm_reader::ClassFile,
    need_verify: bool,
) -> Option<LinkageError> {
    member_refusal(class_file, need_verify, need_verify, need_verify, need_verify, false)
}

/// HotSpot `ClassFileParser::parse_constant_pool`'s checks of the entries
/// that name other entries, made while the pool is parsed (before the access
/// flags), naming the requested class (`<Unknown>` for none). Interpreter
/// round i5 wave 110, lane L5 (probe `L5W110ClassFileParserMore`); before it
/// a dangling or mistyped reference was met at resolution, or never.
///
/// The index pass, in pool order (`Invalid constant pool index <n> in class
/// file <C>` unless said otherwise):
/// * `Fieldref` / `Methodref` / `InterfaceMethodref`: the class must be a
///   `CONSTANT_Class`, the name-and-type a `CONSTANT_NameAndType`;
/// * `NameAndType`: name and descriptor `CONSTANT_Utf8`s;
/// * `MethodType`: its descriptor a `CONSTANT_Utf8`; `Dynamic` /
///   `InvokeDynamic`: a `CONSTANT_NameAndType`;
/// * `MethodHandle`: its reference inside the pool; kinds 1-4 a `Fieldref`
///   (`... (not a field)`), 5 and 8 a `Methodref` (`... (not a method)`), 6
///   and 7 a `Methodref` or, from version 52, an `InterfaceMethodref`, 9 an
///   `InterfaceMethodref` (`... (not an interface method)`), any other kind
///   `Bad method handle kind at constant pool index <entry> in class file <C>`.
///
/// Then the format pass: a `NameAndType` with an empty descriptor or name is
/// `Illegal zero length constant pool entry at <n> in class <C>`; a method
/// descriptor's name and descriptor are judged as a method's, any other as a
/// field's (`Illegal method|field name "<n>" in class <C>` with
/// `names_need_verify`, `Method|Field "<n>" in class <C> has illegal
/// signature "<s>"`); a `Fieldref` naming a method descriptor, or a
/// `Methodref` / `InterfaceMethodref` naming a field descriptor, has an
/// illegal signature. HotSpot makes the `MethodType`, `Dynamic` and
/// `MethodHandle` index checks in every loader and the rest under
/// `_need_verify`; the caller asks only where `need_verify` holds, so a boot
/// class is never judged more strictly than HotSpot judges it.
///
/// `structural` is that `_need_verify` (t1 wave 13, lane I24W): false under
/// `-Xverify:none` ([`ClassManager::set_noverify_relaxes_structural_checks`]),
/// which leaves only the `MethodType`, `Dynamic` / `InvokeDynamic` and
/// `MethodHandle` index checks (measured on HotSpot 25.0.4.1, probe
/// `T1W13I24WStructuralFormat.java` rows S06-S08, S22, S23,
/// S33-S36). Nothing reads the relaxed entries before resolution, which looks
/// each one up by tag and refuses a mismatch.
pub(in crate::class_manager) fn constant_pool_refusal(
    class_file: &cratonvm_reader::ClassFile,
    requested: &str,
    names_need_verify: bool,
    structural: bool,
) -> Option<LinkageError> {
    use cratonvm_reader::constant_pool::ConstantPoolEntry as E;
    let named = if requested.is_empty() {
        "<Unknown>"
    } else {
        requested
    };
    let cp = &class_file.constant_pool;
    let major = class_file.version.major;
    let len = cp.len();
    let refuse = |message: String| {
        Some(LinkageError::ClassFormatError {
            class_name: requested.to_string(),
            message,
        })
    };
    let bad_index = |index: u16| format!("Invalid constant pool index {index} in class file {named}");
    let utf8 = |index: u16| matches!(cp.get(index), Some(E::Utf8(_)));
    let class_ref = |index: u16| matches!(cp.get(index), Some(E::ClassReference { .. }));
    let nat = |index: u16| matches!(cp.get(index), Some(E::NameAndType { .. }));
    for index in 1..len {
        let Ok(index) = u16::try_from(index) else {
            break;
        };
        match cp.get(index) {
            Some(
                E::FieldReference {
                    class_index,
                    name_and_type_index,
                }
                | E::MethodReference {
                    class_index,
                    name_and_type_index,
                }
                | E::InterfaceMethodReference {
                    class_index,
                    name_and_type_index,
                },
            ) if structural => {
                if !class_ref(*class_index) {
                    return refuse(bad_index(*class_index));
                }
                if !nat(*name_and_type_index) {
                    return refuse(bad_index(*name_and_type_index));
                }
            }
            Some(E::NameAndType {
                name_index,
                descriptor_index,
            }) if structural => {
                if !utf8(*name_index) {
                    return refuse(bad_index(*name_index));
                }
                if !utf8(*descriptor_index) {
                    return refuse(bad_index(*descriptor_index));
                }
            }
            Some(E::MethodType { descriptor_index }) => {
                if !utf8(*descriptor_index) {
                    return refuse(bad_index(*descriptor_index));
                }
            }
            Some(
                E::Dynamic {
                    name_and_type_index,
                    ..
                }
                | E::InvokeDynamic {
                    name_and_type_index,
                    ..
                },
            ) => {
                if !nat(*name_and_type_index) {
                    return refuse(bad_index(*name_and_type_index));
                }
            }
            Some(E::MethodHandle {
                reference_kind,
                reference_index,
            }) => {
                let reference = *reference_index;
                if reference == 0 || usize::from(reference) >= len {
                    return refuse(bad_index(reference));
                }
                let target = cp.get(reference);
                let is_field = matches!(target, Some(E::FieldReference { .. }));
                let is_method = matches!(target, Some(E::MethodReference { .. }));
                let is_interface_method =
                    matches!(target, Some(E::InterfaceMethodReference { .. }));
                let mismatch = match *reference_kind {
                    1..=4 => (!is_field).then_some("not a field"),
                    5 | 8 => (!is_method).then_some("not a method"),
                    6 | 7 => (!(is_method || (major >= 52 && is_interface_method)))
                        .then_some("not a method"),
                    9 => (!is_interface_method).then_some("not an interface method"),
                    _ => {
                        return refuse(format!(
                            "Bad method handle kind at constant pool index {index} in class file {named}"
                        ));
                    }
                };
                if let Some(what) = mismatch {
                    return refuse(format!("{} ({what})", bad_index(reference)));
                }
            }
            _ => {}
        }
    }
    // HotSpot returns before its second pass without `_need_verify`.
    if !structural {
        return None;
    }
    // The format pass: every index above is now known good.
    let nat_parts = |index: u16| match cp.get(index) {
        Some(E::NameAndType {
            name_index,
            descriptor_index,
        }) => Some((
            *name_index,
            *descriptor_index,
            cp.get_utf8(*name_index).unwrap_or(""),
            cp.get_utf8(*descriptor_index).unwrap_or(""),
        )),
        _ => None,
    };
    for index in 1..len {
        let Ok(index) = u16::try_from(index) else {
            break;
        };
        match cp.get(index) {
            Some(E::NameAndType { .. }) => {
                let Some((name_index, sig_index, name, sig)) = nat_parts(index) else {
                    continue;
                };
                if sig.is_empty() {
                    return refuse(format!(
                        "Illegal zero length constant pool entry at {sig_index} in class {named}"
                    ));
                }
                if name.is_empty() {
                    return refuse(format!(
                        "Illegal zero length constant pool entry at {name_index} in class {named}"
                    ));
                }
                if major < 49 {
                    continue;
                }
                if sig.starts_with('(') {
                    if names_need_verify && !legal_method_name(name) {
                        return refuse(format!("Illegal method name \"{name}\" in class {named}"));
                    }
                    // `verify_legal_method_signature` alone: no `this`
                    // slot count and no `<init>`-returns-void rule here.
                    let verdict = if name == "<clinit>" && sig != "()V" && major >= 51 {
                        SignatureVerdict::Illegal
                    } else {
                        match method_signature_verdict("m", sig, major, true) {
                            SignatureVerdict::TooManyArguments => SignatureVerdict::Legal,
                            other => other,
                        }
                    };
                    if let Some(message) = signature_refusal_text(verdict, "Method", name, sig, named)
                    {
                        return refuse(message);
                    }
                } else {
                    if names_need_verify && !unqualified_member_name_is_legal(name.as_bytes(), false)
                    {
                        return refuse(format!("Illegal field name \"{name}\" in class {named}"));
                    }
                    if let Some(message) =
                        signature_refusal_text(field_signature_verdict(sig), "Field", name, sig, named)
                    {
                        return refuse(message);
                    }
                }
            }
            Some(E::FieldReference {
                name_and_type_index,
                ..
            }) => {
                if let Some((_, _, name, sig)) = nat_parts(*name_and_type_index) {
                    if sig.starts_with('(') {
                        return refuse(format!(
                            "Field \"{name}\" in class {named} has illegal signature \"{sig}\""
                        ));
                    }
                }
            }
            Some(
                E::MethodReference {
                    name_and_type_index,
                    ..
                }
                | E::InterfaceMethodReference {
                    name_and_type_index,
                    ..
                },
            ) => {
                if let Some((_, _, name, sig)) = nat_parts(*name_and_type_index) {
                    if !sig.starts_with('(') {
                        return refuse(format!(
                            "Method \"{name}\" in class {named} has illegal signature \"{sig}\""
                        ));
                    }
                }
            }
            // `verify_legal_method_signature(vmSymbols::type_name(), sig)`
            // (interpreter round i5 wave 111, lane L5).
            Some(E::MethodType { descriptor_index }) if major >= 49 => {
                let sig = cp.get_utf8(*descriptor_index).unwrap_or("");
                let verdict = match method_signature_verdict("type", sig, major, true) {
                    SignatureVerdict::TooManyArguments => SignatureVerdict::Legal,
                    other => other,
                };
                if let Some(message) = signature_refusal_text(verdict, "Method", "type", sig, named) {
                    return refuse(message);
                }
            }
            // The invoke kinds' target names (wave 111, L5): `<init>` only for
            // `newInvokeSpecial` (`Bad constructor name at constant pool index
            // <name Utf8>`), never for the others (`Bad method name ...`).
            Some(E::MethodHandle {
                reference_kind,
                reference_index,
            }) if matches!(*reference_kind, 5..=8) => {
                let target_nat = match cp.get(*reference_index) {
                    Some(
                        E::MethodReference {
                            name_and_type_index,
                            ..
                        }
                        | E::InterfaceMethodReference {
                            name_and_type_index,
                            ..
                        },
                    ) => *name_and_type_index,
                    _ => continue,
                };
                if let Some((name_index, _, name, _)) = nat_parts(target_nat) {
                    if *reference_kind == 8 && name != "<init>" {
                        return refuse(format!(
                            "Bad constructor name at constant pool index {name_index} in class file {named}"
                        ));
                    }
                    if *reference_kind != 8 && name == "<init>" {
                        return refuse(format!(
                            "Bad method name at constant pool index {name_index} in class file {named}"
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// HotSpot `parse_stream`'s refusal of a `CONSTANT_Module` /
/// `CONSTANT_Package` in a class file (version 53+) that is not a module
/// descriptor, made after the access flags (so a module descriptor is the
/// `NoClassDefFoundError` instead): `Unknown constant tag <tag> in class file
/// <C>`, the first such tag. Below 53 the reader refuses the tag itself.
/// Interpreter round i5 wave 110, lane L5 (probe `L5W110ClassFileParserMore`,
/// row `module-tag-in-class`).
pub(in crate::class_manager) fn bad_constant_refusal(class_file: &cratonvm_reader::ClassFile, requested: &str) -> Option<LinkageError> {
    use cratonvm_reader::constant_pool::ConstantPoolEntry as E;
    if class_file.access_flags.bits() & 0x8000 != 0 {
        return None;
    }
    let cp = &class_file.constant_pool;
    let tag = (1..cp.len()).find_map(|i| match cp.get(u16::try_from(i).ok()?) {
        Some(E::Module { .. }) => Some(19),
        Some(E::Package { .. }) => Some(20),
        _ => None,
    })?;
    let named = if requested.is_empty() {
        "<Unknown>"
    } else {
        requested
    };
    Some(LinkageError::ClassFormatError {
        class_name: requested.to_string(),
        message: format!("Unknown constant tag {tag} in class file {named}"),
    })
}

/// HotSpot `ClassFileParser::parse_interfaces`' `_need_verify` check:
/// `Duplicate interface name "<I>" in class file <C>` (internal forms) for
/// the first interface named a second time. Interpreter round i5 wave 109,
/// lane L5 (probe `L5W109ClassFileParserTexts`, row `dup-interface`); a
/// duplicate was defined silently before.
pub(in crate::class_manager) fn duplicate_interface_refusal(class_file: &cratonvm_reader::ClassFile) -> Option<LinkageError> {
    let interfaces = &class_file.interfaces;
    if interfaces.len() < 2 {
        return None;
    }
    // A handful of interfaces is the common case: compare pairwise without
    // allocating; a long list through a set.
    let dup = if interfaces.len() <= 16 {
        interfaces
            .iter()
            .enumerate()
            .find(|(i, a)| interfaces[..*i].iter().any(|b| ***a == **b))
            .map(|(_, a)| a)?
    } else {
        let mut seen = std::collections::HashSet::with_capacity(interfaces.len());
        interfaces.iter().find(|i| !seen.insert(&***i))?
    };
    Some(LinkageError::ClassFormatError {
        class_name: class_file.this_class.to_string(),
        message: format!(
            "Duplicate interface name \"{dup}\" in class file {}",
            class_file.this_class
        ),
    })
}

/// HotSpot `ClassFileParser::parse_classfile_attributes`' refusals of the
/// nest and sealed attributes (interpreter round i5 wave 109, lane L5; probe
/// `L5W109ClassFileParserTexts`), in its order -- while the attributes are
/// walked, then the `NestMembers` and `PermittedSubclasses` entries:
///
/// * from version 55: `Multiple NestHost attributes`, `Multiple NestMembers
///   attributes`, `Conflicting NestMembers and NestHost attributes` (a
///   `NestHost` after a `NestMembers`), `Conflicting NestHost and NestMembers
///   attributes` (the other order), `Nest-host class_info_index <n> has bad
///   constant type`, `Nest member class_info_index <n> has bad constant type`;
/// * from version 61: `Multiple PermittedSubclasses attributes`,
///   `PermittedSubclasses attribute in final class file <C>`, `Permitted
///   subclass class_info_index <n> has bad constant type`;
///
/// each a `ClassFormatError` with the ` in class file <C>` tail (the final
/// one has its own). The lengths are the reader's (`Wrong NestHost attribute
/// length`, ...). Before wave 109 these attributes were taken as read, a bad
/// index failing later or never.
///
/// Wave 110 (lane L5; probe `L5W110ClassFileParserMore`) adds, in the same
/// walk: `Multiple SourceFile attributes`; from version 49 `Multiple
/// EnclosingMethod attributes`, `Invalid class index in EnclosingMethod
/// attribute` (class index 0), `Invalid or out-of-bounds class index in
/// EnclosingMethod attribute`, `Invalid or out-of-bounds method index in
/// EnclosingMethod attribute` (a non-zero index that is not a
/// `NameAndType`); from version 51 `Multiple BootstrapMethods attributes`,
/// `Short length on BootstrapMethods` (fewer methods than the highest index a
/// `Dynamic` / `InvokeDynamic` constant names), `bootstrap_method_index <n>
/// has bad constant type` (not a `MethodHandle`), `argument_index <n> has bad
/// constant type` (not a loadable constant); and after the walk `Missing
/// BootstrapMethods attribute` for a pool with such a constant and no table.
///
/// Wave 112 (lane L5; probe `L5W112ClassFileParserRecord`): from version 60
/// `Multiple Record attributes`, then `parse_classfile_record_attribute`'s
/// component checks, in order: `Invalid constant pool index <n> for name in
/// Record attribute`, the name's legality (`Illegal field name "<n>" in class
/// <C>`, with `names_need_verify`), `Invalid constant pool index <n> for
/// descriptor in Record attribute`, the descriptor's legality (`Field "<n>"
/// in class <C> has illegal signature "<d>"`), and `Multiple
/// Signature|RuntimeVisibleAnnotations|RuntimeInvisibleAnnotations|
/// RuntimeVisibleTypeAnnotations|RuntimeInvisibleTypeAnnotations attributes
/// for Record component`. The length and a component Signature's length are
/// the reader's.
pub(in crate::class_manager) fn class_attribute_refusal(
    class_file: &cratonvm_reader::ClassFile,
    names_need_verify: bool,
) -> Option<LinkageError> {
    use cratonvm_reader::constant_pool::ConstantPoolEntry;
    let major = class_file.version.major;
    let class: &str = &class_file.this_class;
    let cp = &class_file.constant_pool;
    let refuse = |message: String| {
        Some(LinkageError::ClassFormatError {
            class_name: class.to_string(),
            message,
        })
    };
    let is_class_ref = |index: u16| matches!(cp.get(index), Some(ConstantPoolEntry::ClassReference { .. }));
    let mut nest_host = false;
    let mut nest_members: Option<&[u16]> = None;
    let mut permitted: Option<&[u16]> = None;
    // `parse_classfile_attributes`' repeated attributes not checked below
    // (interpreter round i5 wave 111, lane L5).
    let mut repeated: Vec<(&str, &str)> = vec![
        ("SourceDebugExtension", "Multiple SourceDebugExtension attributes"),
        ("InnerClasses", "Multiple InnerClasses attributes"),
    ];
    if major >= 49 {
        repeated.extend([
            ("Signature", "Multiple Signature attributes"),
            ("RuntimeVisibleAnnotations", "Multiple RuntimeVisibleAnnotations attributes"),
            (
                "RuntimeVisibleTypeAnnotations",
                "Multiple RuntimeVisibleTypeAnnotations attributes",
            ),
            // JDK 25 refuses a repeated invisible one too, whatever
            // `PreserveAllAnnotations` says (wave 112, L5).
            (
                "RuntimeInvisibleAnnotations",
                "Multiple RuntimeInvisibleAnnotations attributes",
            ),
            (
                "RuntimeInvisibleTypeAnnotations",
                "Multiple RuntimeInvisibleTypeAnnotations attributes",
            ),
        ]);
    }
    if let Some(message) =
        repeated_attribute_refusal(class_file.attributes.iter().map(|a| a.name()), &repeated, class)
    {
        return refuse(message);
    }
    let mut source_file = false;
    let mut record: Option<&[cratonvm_reader::attribute::RecordComponent]> = None;
    let mut enclosing_method = false;
    let mut bootstrap_methods = false;
    // The highest bootstrap index a `Dynamic` / `InvokeDynamic` names.
    let max_bootstrap_index = (1..cp.len())
        .filter_map(|i| match cp.get(u16::try_from(i).ok()?) {
            Some(
                ConstantPoolEntry::Dynamic {
                    bootstrap_method_attr_index,
                    ..
                }
                | ConstantPoolEntry::InvokeDynamic {
                    bootstrap_method_attr_index,
                    ..
                },
            ) => Some(*bootstrap_method_attr_index),
            _ => None,
        })
        .max();
    let loadable = |index: u16| {
        matches!(
            cp.get(index),
            Some(
                ConstantPoolEntry::Integer(_)
                    | ConstantPoolEntry::Float(_)
                    | ConstantPoolEntry::Long(_)
                    | ConstantPoolEntry::Double(_)
                    | ConstantPoolEntry::ClassReference { .. }
                    | ConstantPoolEntry::StringReference { .. }
                    | ConstantPoolEntry::MethodHandle { .. }
                    | ConstantPoolEntry::MethodType { .. }
                    | ConstantPoolEntry::Dynamic { .. }
            )
        )
    };
    for attr in &class_file.attributes {
        match attr.as_decoded() {
            Some(Attribute::SourceFile(_)) => {
                if source_file {
                    return refuse(format!("Multiple SourceFile attributes in class file {class}"));
                }
                source_file = true;
            }
            Some(Attribute::EnclosingMethod {
                class_index,
                method_index,
            }) if major >= 49 => {
                if enclosing_method {
                    return refuse(format!(
                        "Multiple EnclosingMethod attributes in class file {class}"
                    ));
                }
                enclosing_method = true;
                if *class_index == 0 {
                    return refuse(format!(
                        "Invalid class index in EnclosingMethod attribute in class file {class}"
                    ));
                }
                if !is_class_ref(*class_index) {
                    return refuse(format!(
                        "Invalid or out-of-bounds class index in EnclosingMethod attribute in class file {class}"
                    ));
                }
                if *method_index != 0
                    && !matches!(cp.get(*method_index), Some(ConstantPoolEntry::NameAndType { .. }))
                {
                    return refuse(format!(
                        "Invalid or out-of-bounds method index in EnclosingMethod attribute in class file {class}"
                    ));
                }
            }
            Some(Attribute::BootstrapMethods(methods)) if major >= 51 => {
                if bootstrap_methods {
                    return refuse(format!(
                        "Multiple BootstrapMethods attributes in class file {class}"
                    ));
                }
                bootstrap_methods = true;
                if max_bootstrap_index.is_some_and(|max| usize::from(max) >= methods.len()) {
                    return refuse(format!("Short length on BootstrapMethods in class file {class}"));
                }
                for method in methods {
                    let mh = method.bootstrap_method_ref;
                    if !matches!(cp.get(mh), Some(ConstantPoolEntry::MethodHandle { .. })) {
                        return refuse(format!(
                            "bootstrap_method_index {mh} has bad constant type in class file {class}"
                        ));
                    }
                    if let Some(bad) = method.bootstrap_arguments.iter().find(|&&a| !loadable(a)) {
                        return refuse(format!(
                            "argument_index {bad} has bad constant type in class file {class}"
                        ));
                    }
                }
            }
            // Read only for a class whose superclass is `java/lang/Record`.
            Some(Attribute::Record(components))
                if major >= 60 && class_file.super_class.as_deref() == Some("java/lang/Record") =>
            {
                if record.is_some() {
                    return refuse(format!("Multiple Record attributes in class file {class}"));
                }
                record = Some(components.as_slice());
            }
            Some(Attribute::NestHost { host_class_index }) if major >= 55 => {
                if nest_host {
                    return refuse(format!("Multiple NestHost attributes in class file {class}"));
                }
                nest_host = true;
                if nest_members.is_some() {
                    return refuse(format!(
                        "Conflicting NestMembers and NestHost attributes in class file {class}"
                    ));
                }
                if !is_class_ref(*host_class_index) {
                    return refuse(format!(
                        "Nest-host class_info_index {host_class_index} has bad constant type in class file {class}"
                    ));
                }
            }
            Some(Attribute::NestMembers { classes }) if major >= 55 => {
                if nest_members.is_some() {
                    return refuse(format!("Multiple NestMembers attributes in class file {class}"));
                }
                nest_members = Some(classes.as_slice());
                if nest_host {
                    return refuse(format!(
                        "Conflicting NestHost and NestMembers attributes in class file {class}"
                    ));
                }
            }
            Some(Attribute::PermittedSubclasses { classes }) if major >= 61 => {
                if permitted.is_some() {
                    return refuse(format!(
                        "Multiple PermittedSubclasses attributes in class file {class}"
                    ));
                }
                if class_file.access_flags.bits() & 0x0010 != 0 {
                    return refuse(format!("PermittedSubclasses attribute in final class file {class}"));
                }
                permitted = Some(classes.as_slice());
            }
            _ => {}
        }
    }
    if let Some(bad) = nest_members.and_then(|c| c.iter().find(|&&i| !is_class_ref(i))) {
        return refuse(format!(
            "Nest member class_info_index {bad} has bad constant type in class file {class}"
        ));
    }
    for component in record.unwrap_or_default() {
        let Some(name) = cp.get_utf8(component.name_index) else {
            return refuse(format!(
                "Invalid constant pool index {} for name in Record attribute in class file {class}",
                component.name_index
            ));
        };
        if names_need_verify && !unqualified_member_name_is_legal(name.as_bytes(), false) {
            return refuse(format!("Illegal field name \"{name}\" in class {class}"));
        }
        let Some(descriptor) = cp.get_utf8(component.descriptor_index) else {
            return refuse(format!(
                "Invalid constant pool index {} for descriptor in Record attribute in class file {class}",
                component.descriptor_index
            ));
        };
        if let Some(message) = signature_refusal_text(
            field_signature_verdict(descriptor),
            "Field",
            name,
            descriptor,
            class,
        ) {
            return refuse(message);
        }
        if let Some(message) = repeated_attribute_refusal(
            component
                .attributes
                .iter()
                .map(cratonvm_reader::attribute::attribute_canonical_name),
            &[
                ("Signature", "Multiple Signature attributes for Record component"),
                (
                    "RuntimeVisibleAnnotations",
                    "Multiple RuntimeVisibleAnnotations attributes for Record component",
                ),
                (
                    "RuntimeInvisibleAnnotations",
                    "Multiple RuntimeInvisibleAnnotations attributes for Record component",
                ),
                (
                    "RuntimeVisibleTypeAnnotations",
                    "Multiple RuntimeVisibleTypeAnnotations attributes for Record component",
                ),
                (
                    "RuntimeInvisibleTypeAnnotations",
                    "Multiple RuntimeInvisibleTypeAnnotations attributes for Record component",
                ),
            ],
            class,
        ) {
            return refuse(message);
        }
    }
    if let Some(bad) = permitted.and_then(|c| c.iter().find(|&&i| !is_class_ref(i))) {
        return refuse(format!(
            "Permitted subclass class_info_index {bad} has bad constant type in class file {class}"
        ));
    }
    // By name: the define natives' pre-pass asks before the attributes are
    // decoded (`define_refused_while_parsing`).
    if max_bootstrap_index.is_some()
        && !bootstrap_methods
        && !class_file.attributes.iter().any(|a| a.name() == "BootstrapMethods")
    {
        return refuse(format!("Missing BootstrapMethods attribute in class file {class}"));    }
    None
}

/// HotSpot `verify_unqualified_name` for a field (`LegalField`) or method
/// (`LegalMethod`) name: non-empty, no `.`, `;`, `[` or `/`, and in a
/// method name no `<` or `>`.
pub(in crate::class_manager) fn unqualified_member_name_is_legal(bytes: &[u8], method: bool) -> bool {
    !bytes.is_empty()
        && bytes.iter().all(|&b| match b {
            b'.' | b';' | b'[' | b'/' => false,
            b'<' | b'>' => !method,
            _ => true,
        })
}

/// HotSpot `verify_legal_method_name` (version 49+): `<init>` and
/// `<clinit>` are the only names starting with `<`.
pub(in crate::class_manager) fn legal_method_name(name: &str) -> bool {
    if name.starts_with('<') {
        name == "<init>" || name == "<clinit>"
    } else {
        unqualified_member_name_is_legal(name.as_bytes(), true)
    }
}

/// HotSpot `ClassFileParser::skip_over_field_signature`'s answer (version
/// 49+).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::class_manager) enum FieldSignatureSkip {
    /// One field type of this many bytes starts the input.
    Len(usize),
    /// No field type starts the input (the caller's "illegal signature").
    Illegal,
    /// `L<name>;` whose name is not a legal class name: `Class name is empty
    /// or contains illegal character in descriptor`.
    BadClassName,
    /// More than 255 `[`: `Array type descriptor has more than 255
    /// dimensions`.
    TooManyDimensions,
}

pub(in crate::class_manager) fn skip_field_signature(sig: &[u8], void_ok: bool) -> FieldSignatureSkip {
    let mut i = 0usize;
    let mut dims = 0usize;
    let mut void_ok = void_ok;
    loop {
        let Some(&c) = sig.get(i) else {
            return FieldSignatureSkip::Illegal;
        };
        match c {
            b'V' if !void_ok => return FieldSignatureSkip::Illegal,
            b'V' | b'B' | b'C' | b'D' | b'F' | b'I' | b'J' | b'S' | b'Z' => {
                return FieldSignatureSkip::Len(i + 1);
            }
            b'L' => {
                let rest = &sig[i + 1..];
                return match rest.iter().position(|&b| b == b';') {
                    Some(n) if unqualified_class_name_is_legal(&rest[..n]) => {
                        FieldSignatureSkip::Len(i + 1 + n + 1)
                    }
                    Some(_) => FieldSignatureSkip::BadClassName,
                    None => FieldSignatureSkip::Illegal,
                };
            }
            b'[' => {
                dims += 1;
                if dims > 255 {
                    return FieldSignatureSkip::TooManyDimensions;
                }
                void_ok = false;
                i += 1;
            }
            _ => return FieldSignatureSkip::Illegal,
        }
    }
}

/// A member descriptor's `ClassFileParser` refusal, without the class tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::class_manager) enum SignatureVerdict {
    Legal,
    /// `<Field|Method> "<name>" in class <C> has illegal signature "<sig>"`.
    Illegal,
    BadClassName,
    TooManyDimensions,
    /// `Too many arguments in method signature` (more than 255 slots, `this`
    /// included).
    TooManyArguments,
}

impl From<FieldSignatureSkip> for SignatureVerdict {
    fn from(skip: FieldSignatureSkip) -> Self {
        match skip {
            FieldSignatureSkip::Len(_) | FieldSignatureSkip::Illegal => Self::Illegal,
            FieldSignatureSkip::BadClassName => Self::BadClassName,
            FieldSignatureSkip::TooManyDimensions => Self::TooManyDimensions,
        }
    }
}

/// HotSpot `verify_legal_field_signature`: exactly one field type.
pub(in crate::class_manager) fn field_signature_verdict(sig: &str) -> SignatureVerdict {
    match skip_field_signature(sig.as_bytes(), false) {
        FieldSignatureSkip::Len(n) if n == sig.len() => SignatureVerdict::Legal,
        other => other.into(),
    }
}

/// HotSpot `verify_legal_name_with_signature` + `verify_legal_method_signature`
/// (+ the `MAX_ARGS_SIZE` check) for method `name` of a version-`major`
/// class file; `is_static` adds no `this` slot.
pub(in crate::class_manager) fn method_signature_verdict(name: &str, sig: &str, major: u16, is_static: bool) -> SignatureVerdict {
    if name == "<clinit>" && sig != "()V" && major >= 51 {
        return SignatureVerdict::Illegal;
    }
    if name.starts_with('<') && !sig.is_empty() && !sig.ends_with('V') {
        return SignatureVerdict::Illegal;
    }
    let bytes = sig.as_bytes();
    if bytes.first() != Some(&b'(') {
        return SignatureVerdict::Illegal;
    }
    let mut p = 1usize;
    let mut slots = usize::from(!is_static);
    loop {
        match skip_field_signature(&bytes[p..], false) {
            FieldSignatureSkip::Len(n) => {
                slots += if matches!(bytes[p], b'J' | b'D') { 2 } else { 1 };
                p += n;
            }
            FieldSignatureSkip::Illegal => break,
            other => return other.into(),
        }
    }
    if bytes.get(p) != Some(&b')') {
        return SignatureVerdict::Illegal;
    }
    p += 1;
    match skip_field_signature(&bytes[p..], true) {
        FieldSignatureSkip::Len(n) if p + n == bytes.len() => {}
        FieldSignatureSkip::Len(_) | FieldSignatureSkip::Illegal => {
            return SignatureVerdict::Illegal;
        }
        other => return other.into(),
    }
    if slots > 255 {
        return SignatureVerdict::TooManyArguments;
    }
    SignatureVerdict::Legal
}

/// The message of a non-[`SignatureVerdict::Legal`] verdict for `kind`
/// (`Field` / `Method`) `member` of class `class` (internal form).
pub(in crate::class_manager) fn signature_refusal_text(
    verdict: SignatureVerdict,
    kind: &str,
    member: &str,
    sig: &str,
    class: &str,
) -> Option<String> {
    match verdict {
        SignatureVerdict::Legal => None,
        SignatureVerdict::Illegal => Some(format!(
            "{kind} \"{member}\" in class {class} has illegal signature \"{sig}\""
        )),
        SignatureVerdict::BadClassName => Some(format!(
            "Class name is empty or contains illegal character in descriptor in class file {class}"
        )),
        SignatureVerdict::TooManyDimensions => Some(format!(
            "Array type descriptor has more than 255 dimensions in class file {class}"
        )),
        SignatureVerdict::TooManyArguments => Some(format!(
            "Too many arguments in method signature in class file {class}"
        )),
    }
}

/// [`member_modifier_refusal`] plus HotSpot's member name and descriptor
/// checks (interpreter round i5 wave 109, lane L5; probe
/// `tools/probes/interp/L5/L5W109ClassFileParserTexts.java`), in
/// `parse_fields` / `parse_method` order: a field's modifiers, name,
/// descriptor; a method's name, then its modifiers, then its descriptor.
///
/// * `names_need_verify` (`_need_verify && !_relax_verify`, as
///   [`class_name_refusal`]): `verify_legal_field_name` / `_method_name`,
///   `Illegal field name "<n>" in class <C>` / `Illegal method name
///   "<n>" in class <C>` (no "file", unlike most texts);
/// * `need_verify`: `verify_legal_field_signature` /
///   `verify_legal_method_signature`, `Field|Method "<n>" in class <C> has
///   illegal signature "<sig>"` (or the descriptor's class-name and
///   dimension refusals), and `Too many arguments in method signature`.
///
/// Class files older than 49 (HotSpot's Java-identifier rules) are not judged
/// by these two. Before wave 109 a user loader defined a field named `a.b` or
/// a method with descriptor `(I` silently.
pub(in crate::class_manager) fn member_refusal(
    class_file: &cratonvm_reader::ClassFile,
    need_verify: bool,
    // The field / method modifier legality (`verify_legal_field_modifiers` /
    // `verify_legal_method_modifiers`); `need_verify` except under
    // `-Xverify:none` (t1 wave 10, lane I24W,
    // [`ClassManager::set_noverify_relaxes_format_checks`]).
    modifiers_need_verify: bool,
    names_need_verify: bool,
    // HotSpot's `_need_verify` for the structural member checks: the
    // descriptors, `ConstantValue` types, `max_locals`, the local variable
    // tables, `Code` presence and duplicate members. `need_verify` except
    // under `-Xverify:none` (t1 wave 13, lane I24W,
    // [`ClassManager::set_noverify_relaxes_structural_checks`]); the rest
    // (repeated attributes, `Exceptions`, `LineNumberTable` pcs, a field
    // descriptor with no layout type) stays in `need_verify`'s scope.
    structural: bool,
    // The per-method attribute-table rules ([`method_attribute_refusal`]),
    // which need the decoded attributes: the define path only.
    attribute_rules: bool,
) -> Option<LinkageError> {
    let major = class_file.version.major;
    let judge_names = names_need_verify && major >= 49;
    let judge_signatures = need_verify && major >= 49;
    let judge_structure = structural && need_verify;
    let is_interface = class_file.access_flags.bits() & 0x0200 != 0;
    let name: &str = &class_file.this_class;
    let refuse = |message: String| {
        Some(LinkageError::ClassFormatError {
            class_name: name.to_string(),
            message,
        })
    };
    if need_verify {
        for field in &class_file.fields {
            let flags = field.access_flags.bits() & RECOGNIZED_FIELD_MODIFIERS;
            let is_public = flags & 0x0001 != 0;
            let is_private = flags & 0x0002 != 0;
            let is_protected = flags & 0x0004 != 0;
            let is_static = flags & 0x0008 != 0;
            let is_final = flags & 0x0010 != 0;
            let is_volatile = flags & 0x0040 != 0;
            let is_transient = flags & 0x0080 != 0;
            let is_enum = flags & 0x4000 != 0;
            let illegal = if is_interface {
                !is_public
                    || !is_static
                    || !is_final
                    || is_private
                    || is_protected
                    || is_volatile
                    || is_transient
                    || (major >= 49 && is_enum)
            } else {
                has_illegal_visibility(flags) || (is_final && is_volatile)
            };
            if illegal && modifiers_need_verify {
                return refuse(format!("Illegal field modifiers in class {name}: 0x{flags:X}"));
            }
            let field_name: &str = &field.name;
            if judge_names && !unqualified_member_name_is_legal(field_name.as_bytes(), false) {
                return refuse(format!("Illegal field name \"{field_name}\" in class {name}"));
            }
            // Under `-Xverify:none` HotSpot lays a field out by its
            // descriptor's first character and dies on one that names no type
            // (`fieldLayoutBuilder.cpp` "Something wrong?", probe row S09);
            // such a descriptor keeps its refusal, any other one loads (S30).
            if judge_signatures
                && (judge_structure || !field_descriptor_has_layout_type(&field.descriptor))
            {
                if let Some(message) = signature_refusal_text(
                    field_signature_verdict(&field.descriptor),
                    "Field",
                    field_name,
                    &field.descriptor,
                    name,
                ) {
                    return refuse(message);
                }
            }
            if judge_structure {
                if let Some(message) = constant_value_refusal(class_file, field) {
                    return refuse(format!("{message} in class file {name}"));
                }
            }
            // `parse_field_attributes`' repeated attributes (wave 111, L5).
            if major >= 49 {
                if let Some(message) = repeated_attribute_refusal(
                    field.attributes.iter().map(|a| a.name()),
                    &[
                        ("Signature", "Multiple Signature attributes for field"),
                        (
                            "RuntimeVisibleAnnotations",
                            "Multiple RuntimeVisibleAnnotations attributes for field",
                        ),
                        (
                            "RuntimeVisibleTypeAnnotations",
                            "Multiple RuntimeVisibleTypeAnnotations attributes for field",
                        ),
                        (
                            "RuntimeInvisibleAnnotations",
                            "Multiple RuntimeInvisibleAnnotations attributes for field",
                        ),
                        (
                            "RuntimeInvisibleTypeAnnotations",
                            "Multiple RuntimeInvisibleTypeAnnotations attributes for field",
                        ),
                    ],
                    name,
                ) {
                    return refuse(message);
                }
            }
        }
        // Without `judge_structure` HotSpot keeps both (probe rows S02 / S29).
        let duplicate = judge_structure
            .then(|| first_duplicate_member(class_file.fields.iter().map(|f| (&*f.name, &*f.descriptor))))
            .flatten();
        if let Some((dup_name, dup_sig)) = duplicate {
            return refuse(format!(
                "Duplicate field name \"{dup_name}\" with signature \"{}\" in class file {name}",
                dup_sig.replace('/', ".")
            ));
        }
    }
    for method in &class_file.methods {
        let flags = method.access_flags.bits();
        let method_name: &str = &method.name;
        if judge_names && !legal_method_name(method_name) {
            return refuse(format!("Illegal method name \"{method_name}\" in class {name}"));
        }
        if method_name == "<clinit>" {
            if major >= 51 && flags & 0x0008 == 0 {
                return refuse(format!("Method <clinit> is not static in class file {name}"));
            }
        } else if modifiers_need_verify
            && method_modifiers_illegal(flags, is_interface, method_name, major)
        {
            return refuse(format!(
                "Method {method_name} in class {name} has illegal modifiers: 0x{flags:X}"
            ));
        }
        if is_interface && method_name == "<init>" {
            return refuse(format!(
                "Interface cannot have a method named <init>, class file {name}"
            ));
        }
        if judge_signatures && judge_structure {
            // HotSpot's `flags` here: a pre-51 `<clinit>` reads as static.
            let is_static = flags & 0x0008 != 0 || method_name == "<clinit>";
            if let Some(message) = signature_refusal_text(
                method_signature_verdict(method_name, &method.descriptor, major, is_static),
                "Method",
                method_name,
                &method.descriptor,
                name,
            ) {
                return refuse(message);
            }
        }
        // `parse_method`'s Code attribute, under `_need_verify`: the
        // arguments (`this` included) must fit in `max_locals`. A format
        // check, at define, unlike the verifier's local-index rules
        // (interpreter round i5 wave 103, lane L7).
        if need_verify {
            if let Some(max_locals) = code_max_locals(method).filter(|_| judge_structure) {
                let is_static = flags & 0x0008 != 0 || (method_name == "<clinit>" && major < 51);
                let args = crate::vtype::descriptor_param_slots(&method.descriptor)
                    + usize::from(!is_static);
                if args > usize::from(max_locals) {
                    return refuse(format!("Arguments can't fit into locals in class file {name}"));
                }
            }
            if let Some(code) = method.code() {
                if let Some(message) = code_table_refusal(
                    class_file,
                    code,
                    judge_names,
                    judge_signatures,
                    judge_structure,
                ) {
                    return refuse(message);
                }
            }
            if let Some(message) = exceptions_attribute_refusal(class_file, method) {
                return refuse(message);
            }
            if attribute_rules {
                if let Some(message) = method_attribute_refusal(class_file, method, judge_structure)
                {
                    return refuse(message);
                }
            }
        }
    }
    if judge_structure {
        if let Some((dup_name, dup_sig)) = first_duplicate_member(
            class_file
                .methods
                .iter()
                .map(|m| (&*m.name, &*m.descriptor)),
        ) {
            return refuse(format!(
                "Duplicate method name \"{dup_name}\" with signature \"{}\" in class file {name}",
                dup_sig.replace('/', ".")
            ));
        }
    }
    None
}

/// HotSpot `ClassFileParser::parse_method`'s `_need_verify` checks of a
/// `Code` attribute's `LineNumberTable` (`parse_linenumber_table`) and
/// `LocalVariableTable` / `LocalVariableTypeTable`
/// (`parse_localvariable_table`), in attribute order, as the whole
/// `ClassFormatError` message (interpreter round i5 wave 109, lane L5; probe
/// `tools/probes/interp/L5/L5W109ClassFileParserTexts.java`):
///
/// * `Invalid pc in LineNumberTable` (`start_pc >= code_length`);
/// * `Invalid start_pc <n> in <table>`, `Invalid length <n> in <table>`
///   (`start_pc + length > code_length`), `Name index <n> in <table> has bad
///   constant type`, `Signature index <n> in <table> has bad constant type`,
///   then the entry's name (`verify_legal_field_name`, `judge_names`) and, in
///   a `LocalVariableTable`, its descriptor (`verify_legal_field_signature`,
///   `judge_signatures`), and `Invalid index <n> in <table>` (`index`, plus one
///   for a `J` / `D`, must be below `max_locals`);
///
/// each with HotSpot's ` in class file <C>` tail. Before wave 109 these tables
/// were kept as read (a debugger met their out-of-range entries).
///
/// Without `structural` (`-Xverify:none`, t1 wave 13, lane I24W) only the
/// `LineNumberTable` pc, the tables' lengths and `Multiple StackMapTable
/// attributes` are judged, as by HotSpot 25.0.4.1 (probe
/// `T1W13I24WStructuralFormat.java` rows S12, S13, S37-S39,
/// S43, S45); the local variable tables are kept as read. Their readers (the
/// JVMTI / JDWP variable tables) bounds-check every entry they use.
pub(in crate::class_manager) fn code_table_refusal(
    class_file: &cratonvm_reader::ClassFile,
    code: &cratonvm_reader::attribute::CodeAttribute,
    judge_names: bool,
    judge_signatures: bool,
    structural: bool,
) -> Option<String> {
    let class: &str = &class_file.this_class;
    let cp = &class_file.constant_pool;
    let code_length = code.code.len();
    let max_locals = usize::from(code.max_locals);
    // One `LocalVariableTable` / `LocalVariableTypeTable` entry.
    let check = |table: &str,
                 is_lvtt: bool,
                 start_pc: u16,
                 length: u16,
                 name_index: u16,
                 sig_index: u16,
                 index: u16|
     -> Option<String> {
        if usize::from(start_pc) >= code_length {
            return Some(format!(
                "Invalid start_pc {start_pc} in {table} in class file {class}"
            ));
        }
        if usize::from(start_pc) + usize::from(length) > code_length {
            return Some(format!("Invalid length {length} in {table} in class file {class}"));
        }
        let Some(var_name) = cp.get_utf8(name_index) else {
            return Some(format!(
                "Name index {name_index} in {table} has bad constant type in class file {class}"
            ));
        };
        let Some(sig) = cp.get_utf8(sig_index) else {
            return Some(format!(
                "Signature index {sig_index} in {table} has bad constant type in class file {class}"
            ));
        };
        if judge_names && !unqualified_member_name_is_legal(var_name.as_bytes(), false) {
            return Some(format!("Illegal field name \"{var_name}\" in class {class}"));
        }
        let mut extra_slot = 0usize;
        if !is_lvtt {
            if judge_signatures {
                if let Some(message) =
                    signature_refusal_text(field_signature_verdict(sig), "Field", var_name, sig, class)
                {
                    return Some(message);
                }
            }
            if sig == "J" || sig == "D" {
                extra_slot = 1;
            }
        }
        if usize::from(index) + extra_slot >= max_locals {
            return Some(format!("Invalid index {index} in {table} in class file {class}"));
        }
        None
    };
    for attr in &code.attributes {
        let refusal = match attr {
            Attribute::LineNumberTable(lines) => lines
                .iter()
                .any(|l| usize::from(l.start_pc) >= code_length)
                .then(|| format!("Invalid pc in LineNumberTable in class file {class}")),
            Attribute::LocalVariableTable(vars) if structural => vars.iter().find_map(|v| {
                check(
                    "LocalVariableTable",
                    false,
                    v.start_pc,
                    v.length,
                    v.name_index,
                    v.descriptor_index,
                    v.index,
                )
            }),
            // HotSpot parses a `LocalVariableTypeTable` from version 49 only.
            Attribute::LocalVariableTypeTable(vars) if structural && class_file.version.major >= 49 => {
                vars.iter().find_map(|v| {
                    check(
                        "LocalVariableTypeTable",
                        true,
                        v.start_pc,
                        v.length,
                        v.name_index,
                        v.signature_index,
                        v.index,
                    )
                })
            }
            // A table the reader could not decode (kept opaque since wave
            // 112): HotSpot's length refusals (`parse_linenumber_table`,
            // `parse_localvariable_table`; interpreter round i5 wave 112, lane
            // L5). A `LocalVariableTypeTable` below 49 and a `StackMapTable`
            // below 50 are skipped there, as by HotSpot.
            Attribute::Unknown { name, .. } => match &**name {
                "LineNumberTable" => Some(format!(
                    "LineNumberTable attribute has wrong length in class file {class}"
                )),
                "LocalVariableTable" => {
                    Some(format!("LocalVariableTable has wrong length in class file {class}"))
                }
                "LocalVariableTypeTable" if class_file.version.major >= 49 => Some(format!(
                    "LocalVariableTypeTable has wrong length in class file {class}"
                )),
                _ => None,
            },
            _ => None,
        };
        if refusal.is_some() {
            return refusal;
        }
    }
    // `Multiple StackMapTable attributes` (version 50+; wave 112, L5).
    if class_file.version.major >= 50
        && code
            .attributes
            .iter()
            .filter(|a| matches!(a, Attribute::StackMapTable { .. }))
            .count()
            > 1
    {
        return Some(format!("Multiple StackMapTable attributes in class file {class}"));
    }
    if !structural {
        return None;
    }
    // `copy_localvariable_table` (version 49+, `_need_verify`; interpreter
    // round i5 wave 110, lane L5): over every `LocalVariableTable` of the
    // `Code` (when it has any entry), an entry with the same start, length,
    // name index and slot as an earlier one is `Duplicated
    // LocalVariableTable attribute entry for '<name>'`; each
    // `LocalVariableTypeTable` entry must match one of them (`LVTT entry for
    // '<name>' in class file <C> does not match any LVT entry`), and only one
    // (`Duplicated LocalVariableTypeTable attribute entry for '<name>'`).
    if class_file.version.major < 49 {
        return None;
    }
    let lvt_entries = code
        .attributes
        .iter()
        .filter_map(|a| match a {
            Attribute::LocalVariableTable(vars) => Some(vars.as_slice()),
            _ => None,
        })
        .flatten();
    let mut keys: FxHashMap<(u16, u16, u16, u16), bool> = FxHashMap::default();
    for v in lvt_entries {
        if keys
            .insert((v.start_pc, v.length, v.name_index, v.index), false)
            .is_some()
        {
            let name = cp.get_utf8(v.name_index).unwrap_or("");
            return Some(format!(
                "Duplicated LocalVariableTable attribute entry for '{name}' in class file {class}"
            ));
        }
    }
    if keys.is_empty() {
        return None;
    }
    for attr in &code.attributes {
        let Attribute::LocalVariableTypeTable(vars) = attr else {
            continue;
        };
        for v in vars {
            let name = cp.get_utf8(v.name_index).unwrap_or("");
            match keys.get_mut(&(v.start_pc, v.length, v.name_index, v.index)) {
                None => {
                    return Some(format!(
                        "LVTT entry for '{name}' in class file {class} does not match any LVT entry"
                    ));
                }
                Some(typed) if *typed => {
                    return Some(format!(
                        "Duplicated LocalVariableTypeTable attribute entry for '{name}' in class file {class}"
                    ));
                }
                Some(typed) => *typed = true,            }        }    }
    None
}

/// The first attribute of `attributes` whose name is one of `texts`' and was
/// met before: `<text> in class file <class>` (HotSpot's `Multiple ...
/// attributes` refusals; interpreter round i5 wave 111, lane L5). At most 16
/// names.
/// [`method_attribute_refusal`]'s repeated-attribute rows: the first two for
/// every version, the rest from version 49 (`ClassFileParser::parse_method`).
pub(in crate::class_manager) const METHOD_REPEATED_ATTRIBUTE_TEXTS: [(&str, &str); 10] = [
    ("Code", "Multiple Code attributes"),
    ("MethodParameters", "Multiple MethodParameters attributes"),
    ("Signature", "Multiple Signature attributes for method"),
    (
        "RuntimeVisibleAnnotations",
        "Multiple RuntimeVisibleAnnotations attributes for method",
    ),
    (
        "RuntimeVisibleParameterAnnotations",
        "Multiple RuntimeVisibleParameterAnnotations attributes for method",
    ),
    ("AnnotationDefault", "Multiple AnnotationDefault attributes for method"),
    (
        "RuntimeVisibleTypeAnnotations",
        "Multiple RuntimeVisibleTypeAnnotations attributes for method",
    ),
    (
        "RuntimeInvisibleAnnotations",
        "Multiple RuntimeInvisibleAnnotations attributes for method",
    ),
    (
        "RuntimeInvisibleParameterAnnotations",
        "Multiple RuntimeInvisibleParameterAnnotations attributes for method",
    ),
    (
        "RuntimeInvisibleTypeAnnotations",
        "Multiple RuntimeInvisibleTypeAnnotations attributes for method",
    ),
];

/// The [`METHOD_REPEATED_ATTRIBUTE_TEXTS`] rows a class of major version
/// `major` is held to.
pub(in crate::class_manager) fn method_repeated_attribute_texts(major: u16) -> &'static [(&'static str, &'static str)] {
    if major >= 49 {
        &METHOD_REPEATED_ATTRIBUTE_TEXTS
    } else {
        &METHOD_REPEATED_ATTRIBUTE_TEXTS[..2]
    }
}


pub(in crate::class_manager) fn repeated_attribute_refusal<'n>(
    names: impl IntoIterator<Item = &'n str>,
    texts: &[(&str, &str)],
    class: &str,
) -> Option<String> {
    let mut seen = [false; 16];
    for name in names {
        if let Some(i) = texts.iter().take(16).position(|(n, _)| *n == name) {
            if seen[i] {
                return Some(format!("{} in class file {class}", texts[i].1));            }
            seen[i] = true;        }    }
    None
}

/// HotSpot `ClassFileParser::parse_method`'s per-method attribute rules
/// beyond `Exceptions`, under `_need_verify` (the caller's), interpreter round
/// i5 wave 111, lane L5 (probe `L5W111ClassFileParserLast`):
/// * `Code attribute in native or abstract methods`, `Multiple Code
///   attributes`, and after the table `Absent Code attribute in method that
///   is not native or abstract`;
/// * `Multiple MethodParameters attributes`; from version 49 `Multiple
///   Signature|RuntimeVisibleAnnotations|RuntimeVisibleParameterAnnotations|
///   AnnotationDefault|RuntimeVisibleTypeAnnotations attributes for method`;
///
/// each with the ` in class file <C>` tail.
///
/// The two `Code` presence rules are `structural` (`_need_verify`; off under
/// `-Xverify:none`, t1 wave 13, lane I24W, probe rows S40 / S41): HotSpot then
/// keeps an abstract method's `Code` and defines a concrete method without one.
/// The repeated-attribute rules hold in every case (rows S44, S21).
pub(in crate::class_manager) fn method_attribute_refusal(
    class_file: &cratonvm_reader::ClassFile,
    method: &cratonvm_reader::method::ClassFileMethod,
    structural: bool,
) -> Option<String> {
    let class: &str = &class_file.this_class;
    let flags = method.access_flags.bits();
    let native_or_abstract = flags & (0x0100 | 0x0400) != 0;
    let has_code = method.attributes.iter().any(|a| a.name() == "Code");
    if structural && native_or_abstract && has_code {
        return Some(format!(
            "Code attribute in native or abstract methods in class file {class}"
        ));
    }
    // JIT round 23 wave 10 (lane L1, proxy-define page): one static table,
    // sliced by version, instead of a `Vec` built (and grown) per method of
    // every define. Same entries, same order.
    let texts = method_repeated_attribute_texts(class_file.version.major);
    if let Some(message) =
        repeated_attribute_refusal(method.attributes.iter().map(|a| a.name()), texts, class)
    {
        return Some(message);
    }
    if structural && !native_or_abstract && !has_code {
        return Some(format!(
            "Absent Code attribute in method that is not native or abstract in class file {class}"
        ));
    }
    None
}

/// HotSpot `ClassFileParser::parse_method`'s `Exceptions` attribute checks
/// (interpreter round i5 wave 110, lane L5; probe
/// `L5W110ClassFileParserMore`): a second one is `Multiple Exceptions
/// attributes in class file <C>`; under `_need_verify` (the caller's) each
/// entry must be a `CONSTANT_Class`, else `Exception name has bad type at
/// constant pool <n> in class file <C>`. The length is the reader's
/// (`Exceptions attribute has wrong length`).
pub(in crate::class_manager) fn exceptions_attribute_refusal(
    class_file: &cratonvm_reader::ClassFile,
    method: &cratonvm_reader::method::ClassFileMethod,
) -> Option<String> {
    use cratonvm_reader::constant_pool::ConstantPoolEntry;
    let class: &str = &class_file.this_class;
    let cp = &class_file.constant_pool;
    let mut seen = false;
    for attr in &method.attributes {
        let Some(Attribute::Exceptions { exception_indices }) = attr.as_decoded() else {
            continue;
        };
        if seen {
            return Some(format!("Multiple Exceptions attributes in class file {class}"));
        }
        seen = true;
        if let Some(bad) = exception_indices
            .iter()
            .find(|&&i| !matches!(cp.get(i), Some(ConstantPoolEntry::ClassReference { .. })))
        {
            return Some(format!(
                "Exception name has bad type at constant pool {bad} in class file {class}"
            ));
        }
    }
    None
}

/// Does a field descriptor start with a character HotSpot's field layout
/// gives a type (`Signature::basic_type`: a primitive, `L` or `[`)? Under
/// `-Xverify:none` a descriptor that does not is HotSpot's own crash, so this
/// VM keeps refusing it (t1 wave 13, lane I24W).
pub(in crate::class_manager) fn field_descriptor_has_layout_type(descriptor: &str) -> bool {
    matches!(
        descriptor.as_bytes().first(),
        Some(b'B' | b'C' | b'D' | b'F' | b'I' | b'J' | b'S' | b'Z' | b'L' | b'[')
    )
}

/// Under `-Xverify:none` (the structural checks relaxed) HotSpot defines a
/// method whose arguments do not fit in its `max_locals` (no `Arguments can't
/// fit into locals`, probe row S11) and links it with no verifier. This VM's
/// frames hold `max_locals` slots and its link-time structural scan refuses
/// such a method, which would have been HotSpot's undefined behaviour at run
/// time and is a `VerifyError` here at the first reflective link. Widen each
/// such `max_locals` to the argument slots instead: the frame then holds every
/// argument, the scan passes, and the class links as on HotSpot (t1 wave 13,
/// lane I24W). Methods whose arguments fit are untouched.
pub(in crate::class_manager) fn widen_max_locals_to_arguments(class_file: &mut cratonvm_reader::ClassFile) {
    use cratonvm_reader::attribute::LazyAttribute;
    let major = class_file.version.major;
    for method in class_file.methods.iter_mut() {
        let flags = method.access_flags.bits();
        let is_static = flags & 0x0008 != 0 || (&*method.name == "<clinit>" && major < 51);
        let args = crate::vtype::descriptor_param_slots(&method.descriptor) + usize::from(!is_static);
        for attr in method.attributes.iter_mut() {
            if let LazyAttribute::Decoded(Attribute::Code(code)) = attr {
                if args > usize::from(code.max_locals) {
                    code.max_locals = u16::try_from(args).unwrap_or(u16::MAX);
                }
            }
        }
    }
}

/// The `max_locals` of `method`'s `Code` attribute, read from the attribute
/// body when it is not decoded yet (no decode: bytes 2..4 of the body).
pub(in crate::class_manager) fn code_max_locals(method: &cratonvm_reader::method::ClassFileMethod) -> Option<u16> {
    use cratonvm_reader::attribute::LazyAttribute;
    method.attributes.iter().find_map(|a| match a {
        LazyAttribute::Decoded(Attribute::Code(code)) => Some(code.max_locals),
        LazyAttribute::Raw(raw) if &*raw.name == "Code" => {
            let at = raw.range.start;
            if raw.range.end < at + 4 {
                return None;
            }
            let body: &[u8] = &raw.source;
            let bytes = body.get(at + 2..at + 4)?;
            Some(u16::from_be_bytes([bytes[0], bytes[1]]))
        }
        _ => None,
    })
}

/// HotSpot `ClassFileParser::parse_field_attributes` for one field's
/// `ConstantValue` attributes, under `_need_verify` (the caller's): the
/// attribute counts only on a STATIC field (an instance field's is ignored);
/// each one is checked as it is met by `verify_constantvalue`, and a second
/// one is `Duplicate ConstantValue attribute`. The rule (JDK 25):
///
/// * index 0 or past the pool: `Bad initial value index <n> in ConstantValue
///   attribute`;
/// * `J` wants a `Long` entry, `F` a `Float`, `D` a `Double`, and `B C S Z I`
///   an `Integer` (any value; the format check asks only for the tag), else
///   `Inconsistent constant value type`;
/// * an object descriptor wants exactly `Ljava/lang/String;` with a `String`
///   entry, else `Bad string initial value`;
/// * an array descriptor: `Unable to set initial value <n>`.
///
/// Returns the message without HotSpot's ` in class file <name>` tail, which
/// the caller appends. Every one is a `ClassFormatError`. Interpreter round i4
/// wave 98, lane L5 (routed from L7; probe
/// `L7W98ConstantValueNarrowing.java`, rows `format`).
/// Before it the entry was stored whatever its tag (a `Value::Int` in a
/// `long` slot, a `String` in an `Object` static).
pub(in crate::class_manager) fn constant_value_refusal(
    class_file: &cratonvm_reader::ClassFile,
    field: &cratonvm_reader::field::ClassFileField,
) -> Option<String> {
    use cratonvm_reader::constant_pool::ConstantPoolEntry as E;
    if !field.is_static() {
        return None;
    }
    let cp = &class_file.constant_pool;
    let mut seen = false;
    for attr in &field.attributes {
        if attr.name() != "ConstantValue" {
            continue;
        }
        if seen {
            return Some("Duplicate ConstantValue attribute".to_string());
        }
        seen = true;
        let index = match attr.decoded_or_decode(cp).as_deref() {
            Some(Attribute::ConstantValue {
                constant_value_index,
            }) => *constant_value_index,
            // A body the reader cannot decode was already refused by it.
            _ => continue,
        };
        if index == 0 || usize::from(index) >= cp.len() {
            return Some(format!(
                "Bad initial value index {index} in ConstantValue attribute"
            ));
        }
        let entry = cp.get(index);
        let ok = match field.descriptor.as_bytes().first() {
            Some(b'J') => matches!(entry, Some(E::Long(_))),
            Some(b'F') => matches!(entry, Some(E::Float(_))),
            Some(b'D') => matches!(entry, Some(E::Double(_))),
            Some(b'B' | b'C' | b'S' | b'Z' | b'I') => matches!(entry, Some(E::Integer(_))),
            Some(b'L') => {
                if &*field.descriptor == "Ljava/lang/String;"
                    && matches!(entry, Some(E::StringReference { .. }))
                {
                    true
                } else {
                    return Some("Bad string initial value".to_string());
                }
            }
            _ => return Some(format!("Unable to set initial value {index}")),
        };
        if !ok {
            return Some("Inconsistent constant value type".to_string());        }    }
    None
}

/// HotSpot's `ClassFormatError` text for a reader refusal met while a
/// member's attributes are decoded after the parse, when the reader knows
/// HotSpot's wording for it (the exception table's `Catch type in exception
/// table has bad constant type in class file C`); `None` keeps the define
/// path's own `... attribute decode failed: ...` text. Interpreter round i5
/// wave 109, lane L5.
pub(in crate::class_manager) fn decode_refusal_hotspot_text(
    e: &cratonvm_reader::class_reader_error::ClassReaderError,
    requested: &str,
) -> Option<String> {
    match e {
        cratonvm_reader::class_reader_error::ClassReaderError::ClassFileParser { .. } => {
            e.hotspot_class_format_message(requested)
        }
        _ => None,
    }
}

/// The first `(name, descriptor)` pair met a second time, in declaration
/// order: HotSpot `ClassFileParser::parse_fields` / `parse_methods`'
/// `_need_verify` duplicate check (`put_after_lookup` over the members in
/// order). Its `ClassFormatError` prints the name as is and the descriptor
/// through `Symbol::as_klass_external_name` (every `/` a `.`), then the
/// class's internal name: `Duplicate field name "f" with signature
/// "Ljava.lang.String;" in class file p/X`. Before interpreter round i4 wave
/// 98 (lane L5; `tools/probes/interp/L5/L5W98ClassFileParseTexts.java`) a
/// duplicate member was defined silently, the later one shadowing the first.
pub(in crate::class_manager) fn first_duplicate_member<'a>(
    members: impl ExactSizeIterator<Item = (&'a str, &'a str)>,
) -> Option<(&'a str, &'a str)> {
    if members.len() < 2 {
        return None;
    }
    let mut seen = std::collections::HashSet::with_capacity(members.len());
    for pair in members {
        if !seen.insert(pair) {
            return Some(pair);        }    }
    None
}

/// HotSpot `ClassFileParser::post_process_parsed_stream`'s
/// `guarantee_property` (every loader, before the superclass is resolved):
/// `ClassFormatError: Interfaces must have java.lang.Object as superclass in
/// class file <internal name>` for an interface whose `super_class` names any
/// other class (measured on JDK 25.0.3, `L5W141LoaderHunt` row
/// `d-iface-super-not-object`). CratonVM defined such an interface. Interpreter
/// round i7 wave 141, lane L5; `CRATONVM_INTERFACE_SUPER_OBJECT_CHECK`
/// (default ON, `=0` off).
pub(in crate::class_manager) fn interface_superclass_refusal(class_file: &cratonvm_reader::ClassFile) -> Option<LinkageError> {
    if class_file.access_flags.bits() & 0x0200 == 0 {
        return None;
    }
    let sup = class_file.super_class.as_deref()?;
    if sup == "java/lang/Object" || false {
        return None;
    }
    Some(LinkageError::ClassFormatError {
        class_name: class_file.this_class.to_string(),
        message: format!(
            "Interfaces must have java.lang.Object as superclass in class file {}",
            class_file.this_class
        ),
    })
}



/// Will a define of `class_file` under the requested `name` (internal form,
/// empty for none) by a loader other than the boot loader be refused while
/// HotSpot's `ClassFileParser` parses it -- its class flags, the wrong-name
/// check, its fields' and methods' flags -- before any supertype is
/// resolved? The define natives' supertype pre-pass asks, so that it does not
/// run a loader's `loadClass` for the supertypes of a class HotSpot never
/// gets as far as linking (interpreter round i4 wave 84, lane L5).
///
/// `relaxation` is the defining VM's [`ClassManager::noverify_relaxation`]
/// (t1 wave 13, lane I24W): under `-Xverify:none` the define accepts what the
/// relaxed checks would refuse, so the pre-pass must ask for its supertypes
/// as HotSpot's `resolve_super_or_fail` does. `(false, false)` is the strict
/// scope.
pub fn define_refused_while_parsing(
    class_file: &cratonvm_reader::ClassFile,
    name: &str,
    relaxation: (bool, bool),
) -> bool {
    let legality = !relaxation.0;
    let structural = !relaxation.1;
    constant_pool_refusal(class_file, name, legality, structural).is_some()
        || bad_constant_refusal(class_file, name).is_some()
        || class_name_refusal(class_file, name, structural).is_some()
        || class_modifier_refusal(class_file, name, legality).is_some()
        || (structural && class_file.this_class.starts_with('['))
        || (!name.is_empty() && &*class_file.this_class != name)
        || (structural && duplicate_interface_refusal(class_file).is_some())
        || interface_superclass_refusal(class_file).is_some()
        || member_refusal(class_file, true, legality, legality, structural, false).is_some()
        || class_attribute_refusal(class_file, legality).is_some()
        || inner_class_modifier_refusal(
            &class_file.attributes,
            &class_file.constant_pool,
            class_file.version.major,
            &class_file.this_class,
            legality,
            structural,
        )
        .is_some()
}

/// zt t16/df6 (`docs/feature-designs/zgc-architecture.md`
/// §3): will a define of `bytes` alone (no requested name) by a user-defined
/// loader be refused while HotSpot's `ClassFileParser` parses it, before the
/// class exists? The structural parse (the reader's refusal), the attribute
/// decode the define forces (class, method and field tables), then
/// [`define_refused_while_parsing`]'s format checks: the constant pool's
/// references and names, the class's and its members' modifiers, an array
/// `this_class`, duplicate interfaces, the class attributes. HotSpot makes all
/// of them before it allocates the mirror or calls `ClassLoader.addClass`, so
/// a define door that would refuse an earlier step with `OutOfMemoryError`
/// (the ZGC `classes` presize, the mirror reservation) yields to the define's
/// `ClassFormatError` when this answers `true`
/// (`native-builtins::classloader::refusal_yields_to_format_error`).
///
/// The scope is the user-defined loader's (`_need_verify`, names checked):
/// the boot and platform loaders relax some checks, so for them a `true`
/// can name bytes the define then accepts, and that define runs without the
/// skipped step (the order before zt t6, an `OutOfMemoryError` after the
/// define instead of before it). The wrong-name check needs the requested
/// name: [`define_refused_by_format_as`] makes it. Parses and decodes the
/// bytes: callers ask only on a refusal, never on a served define.
pub fn define_refused_by_format(bytes: &[u8]) -> bool {
    define_refused_by_format_as(bytes, "")
}

/// zt t20/df8 (define page §3): [`define_refused_by_format`] for a define
/// that requested `name` (internal form; empty for none), which adds
/// `ClassFileParser`'s wrong-name check: bytes whose `this_class` is not
/// `name` are HotSpot's `NoClassDefFoundError: <name> (wrong name: <this>)`,
/// raised while it parses, before the mirror or the `addClass` upcall. A
/// hidden define (whose registered name is not the bytes' own) passes `""`.
pub fn define_refused_by_format_as(bytes: &[u8], name: &str) -> bool {
    let mut class_file = match cratonvm_reader::read_class(bytes) {
        Ok(class_file) => class_file,
        Err(_) => return true,
    };
    if force_decode_all(&mut class_file.attributes, &class_file.constant_pool).is_err() {
        return true;
    }
    for method in class_file.methods.iter_mut() {
        if force_decode_all(&mut method.attributes, &class_file.constant_pool).is_err() {
            return true;
        }
    }
    for field in class_file.fields.iter_mut() {
        if force_decode_all(&mut field.attributes, &class_file.constant_pool).is_err() {
            return true;
        }
    }
    define_refused_while_parsing(&class_file, name, (false, false))
}

/// HotSpot's `verify_legal_method_modifiers` for a method other than
/// `<clinit>`.
pub(in crate::class_manager) fn method_modifiers_illegal(flags: u16, is_interface: bool, name: &str, major: u16) -> bool {
    let is_public = flags & 0x0001 != 0;
    let is_private = flags & 0x0002 != 0;
    let is_protected = flags & 0x0004 != 0;
    let is_static = flags & 0x0008 != 0;
    let is_final = flags & 0x0010 != 0;
    let is_synchronized = flags & 0x0020 != 0;
    let is_bridge = flags & 0x0040 != 0;
    let is_native = flags & 0x0100 != 0;
    let is_abstract = flags & 0x0400 != 0;
    let is_strict = flags & 0x0800 != 0;
    let gte_1_5 = major >= 49;
    let gte_8 = major >= 52;
    let gte_17 = major >= 61;
    if is_interface {
        if gte_8 {
            (is_public == is_private)
                || is_native
                || is_protected
                || is_final
                || is_synchronized
                || (is_abstract && (is_private || is_static || (!gte_17 && is_strict)))
        } else if gte_1_5 {
            !is_public
                || is_private
                || is_protected
                || is_static
                || is_final
                || is_synchronized
                || is_native
                || !is_abstract
                || is_strict
        } else {
            !is_public || is_static || is_final || is_native || !is_abstract
        }
    } else if has_illegal_visibility(flags) {
        true
    } else if name == "<init>" {
        is_static || is_final || is_synchronized || is_native || is_abstract || (gte_1_5 && is_bridge)
    } else if is_abstract {
        is_final
            || is_native
            || is_private
            || is_static
            || (gte_1_5 && (is_synchronized || (!gte_17 && is_strict)))
    } else {
        false
    }
}


/// Adapter implementing [`ClassHierarchy`] over the `ClassManager`'s
/// `ClassStore` + name-to-id index. Used by the verifier (Pass 2 / Pass 3)
/// during `define_class_with_options`.
///
/// Audit fix (HIGH #1) — verify-before-store ordering: the class being
/// defined is verified *before* it is inserted into `class_store`, so any
/// hierarchy query about the class-under-verification (its own name, its
/// own `ClassId`) would otherwise miss. `in_flight` carries that class so
/// self-references resolve during verification without having to register
/// a not-yet-verified class in the store. This is the lower-risk option:
/// no partially-registered class is ever visible, and a class that fails
/// verification is simply dropped.
pub(in crate::class_manager) struct ClassStoreHierarchy<'a> {
    pub(in crate::class_manager) class_store: &'a ClassStore,
    pub(in crate::class_manager) loaded_classes: &'a LoadedClassesMap,
    /// The VM's user-loader parent links (`ClassManager::user_loader_parents`).
    pub(in crate::class_manager) user_loader_parents: &'a crate::loaders::UserLoaderParents,
    /// The class currently being verified (not yet in `class_store`).
    pub(in crate::class_manager) in_flight: Option<&'a Class>,
    /// The parsed direct-superclass name of the class currently being
    /// verified. Its resolved ClassId can still refer to an incomplete
    /// bootstrap placeholder, so constructor verification needs this exact
    /// symbolic edge before the class is registered.
    pub(in crate::class_manager) in_flight_super_name: Option<&'a str>,
    /// The defining/initiating loader of the class being verified, used to
    /// make `lookup` loader-aware (CL-CLASSMANAGER fix). When `Some`, a
    /// name is resolved by walking that loader's parent-delegation chain
    /// first, so two loaders that define the same name resolve to the
    /// *correct* `ClassId` for the requesting loader rather than a global
    /// first-match. `None` falls back to the legacy delegation-chain probe
    /// (used only where no requesting loader is available).
    pub(in crate::class_manager) requesting_loader: Option<ClassLoaderId>,
    /// The names HotSpot's verifier would load through the defining loader
    /// (`ClassHierarchy::note_verifier_load`), first-seen order, each once.
    /// `Some` only for a define-time verification whose class owes those
    /// loads at link (`ClassManager::verifier_link_loads`).
    pub(in crate::class_manager) verifier_loads: Option<&'a std::cell::RefCell<Vec<crate::vtype::VerifierLinkLoad>>>,
    pub app_workarounds: &'a [cratonvm_native_api::AppWorkaroundEntry],
}

impl<'a> ClassStoreHierarchy<'a> {
    pub(in crate::class_manager) fn lookup(&self, name: &str) -> Option<ClassId> {
        // The class-under-verification is not yet in `loaded_classes`;
        // resolve self-references to its reserved id.
        if let Some(inflight) = self.in_flight {
            if &*inflight.name == name {
                return Some(inflight.id);
            }
        }
        // CL-CLASSMANAGER fix: loader-aware resolution. The previous code
        // probed the built-in delegation chain and then fell back to a
        // *global first-match-by-name* scan over every loader. That
        // first-match could bind a referenced name to the WRONG class when
        // two distinct loaders define the same name (the JVMS "class loader
        // constraint" / runtime-package identity is keyed on the
        // *defining* loader, not the bare name). We now resolve through the
        // requesting class's loader-delegation order first.
        //
        // C34 audit fix (HIGH): zero-allocation probe. The previous
        // `Arc::from(name)` minted a fresh `Arc<str>` on EVERY lookup
        // (round-9 CRIT-2 carry — class lookup happens at every dynamic
        // dispatch / `Class.forName` / `Constant_Class` resolution; Spring
        // cold start measured >100k hits). With hashbrown's `raw_entry`
        // API we hash the borrowed `(loader_id, &str)` tuple directly and
        // probe the bucket using a custom equality closure — no `Arc<str>`
        // is allocated unless we are about to insert.
        if let Some(req) = self.requesting_loader {
            if let Some(id) = loaded_class_for_requesting_loader(
                self.loaded_classes,
                self.user_loader_parents,
                req,
                name,
                loader_aware_resolution(),
            ) {
                return Some(id);
            }
            // Not found along the requesting loader's delegation path. Per
            // the loader-faithful model this reference is unresolved here
            // (the caller — `is_subclass` — then falls back to the static
            // JDK hierarchy / optimism for genuinely-unloaded forward
            // references). Do NOT scan unrelated loaders: a same-named
            // class defined by a loader outside this delegation path is a
            // *different* runtime type and must not satisfy the reference.
            return None;
        }
        // No requesting loader available (e.g. a redefine path that did not
        // supply one). Fall back to the legacy built-in delegation-chain
        // probe.
        //
        // Round 9 audit fix (HIGH #6): iterate over the canonical
        // `BUILTIN_LOADER_DELEGATION_CHAIN` constant instead of re-inlining
        // the same 3-element array (Bootstrap, Extension, Application).
        for loader_id in BUILTIN_LOADER_DELEGATION_CHAIN {
            if let Some(id) = loaded_classes_probe(self.loaded_classes, *loader_id, name) {
                return Some(id);
            }
        }
        // Last-resort fallback for custom loaders when no requesting loader
        // was supplied. NARROWED (CL-CLASSMANAGER): only resolve by bare
        // name when the definition is *unique* across all loaders. If two
        // or more loaders defined this name we cannot pick a correct one
        // without the requesting loader's identity, so we return `None`
        // (unresolved) rather than silently binding to an arbitrary
        // first-match — which could be the wrong runtime type.
        let mut found: Option<ClassId> = None;
        for ((_, class_name), &id) in self.loaded_classes.iter() {
            if &**class_name == name {
                if found.is_some() {
                    // Ambiguous: more than one loader defines this name and
                    // we have no requesting-loader context to disambiguate.
                    return None;
                }
                found = Some(id);
            }
        }
        found
    }

    /// Structural class lookup across loaders for verifier hierarchy queries.
    ///
    /// Verification frames retain binary class names rather than loader-qualified
    /// identities. When a custom class loader is defining a class that references
    /// other classes not yet defined by that custom loader (or defined by the
    /// application/system loader), falling back to the built-in delegation chain
    /// allows the verifier to prove structural subtype relationships instead of
    /// falsely rejecting valid bytecode with VerifyError.
    pub(in crate::class_manager) fn lookup_structural(&self, name: &str) -> Option<ClassId> {
        if let Some(id) = self.lookup(name) {
            return Some(id);
        }
        // If name is a nested class of the in-flight class being defined,
        // it cannot be defined yet for in_flight's loader. Any copy in
        // loaded_classes belongs to a different loader/version.
        if self.in_flight.is_some_and(|c| {
            name.starts_with(&*c.name) && name.as_bytes().get(c.name.len()) == Some(&b'$')
        }) {
            return None;
        }
        for loader_id in BUILTIN_LOADER_DELEGATION_CHAIN {
            if let Some(id) = loaded_classes_probe(self.loaded_classes, *loader_id, name) {
                return Some(id);
            }
        }
        let mut found: Option<ClassId> = None;
        for ((_, class_name), &id) in self.loaded_classes.iter() {
            if &**class_name == name {
                if found.is_some() {
                    return None;
                }
                found = Some(id);
            }
        }
        found
    }

    /// Resolve a `ClassId` to its `Class`, transparently returning the
    /// in-flight (not-yet-stored) class when the id matches it.
    pub(in crate::class_manager) fn class_for(&self, id: ClassId) -> Option<&Class> {
        if let Some(inflight) = self.in_flight {
            if inflight.id == id {
                return Some(inflight);
            }
        }
        self.class_store.get(id)
    }

    /// The class under verification when it is `current` and HotSpot's
    /// verifier would verify it, for the JVMS §4.10.1.8 / §4.10.1.9 linkage
    /// rules ([`ClassHierarchy::protected_receiver_rule`],
    /// [`ClassHierarchy::invokespecial_target`]); `None` = no rule.
    ///
    /// HotSpot (`Verifier::is_eligible_for_verification`) does not verify the
    /// boot loader's classes (`BytecodeVerificationLocal` off) nor the
    /// reflection serialization-constructor accessors (subclasses of
    /// `jdk/internal/reflect/SerializationConstructorAccessorImpl`, formerly
    /// `MagicAccessorImpl`). Hidden classes and every VM-generated or
    /// fabricated origin (lambda / proxy / accessor shapes, VM-internal
    /// types, compatibility stubs) are excluded too: their bytes are this
    /// VM's, not a class file a loader handed in.
    ///
    /// JIT round 20 wave 17 (lane rev15): so is a class of the JDK image
    /// (`BootImage`). HotSpot does verify the platform and application
    /// loaders' JDK classes, but they are consistent by construction, and
    /// this VM records an image class's loader as `Bootstrap` or
    /// `Application` where HotSpot has another (`method_override` module
    /// docs): a rule judged on such a label could only refuse wrongly.
    pub(in crate::class_manager) fn linkage_rule_class(&self, current: &str) -> Option<&'a Class> {
        let class = self.in_flight?;
        if &*class.name != current
            || class.loader_id == ClassLoaderId::Bootstrap
            || class.hidden
            || !matches!(
                class.origin,
                ClassOrigin::ApplicationClassPath { .. } | ClassOrigin::UserDefined { .. }
            )
        {
            return None;
        }
        let mut next = class.superclass;
        let mut depth = 0usize;
        while let Some(id) = next {
            let Some(sup) = self.class_store.get(id) else {
                break;
            };
            if matches!(
                &*sup.name,
                "jdk/internal/reflect/MagicAccessorImpl"
                    | "jdk/internal/reflect/SerializationConstructorAccessorImpl"
            ) {
                return None;
            }
            depth += 1;
            if depth > 256 {
                break;
            }
            next = sup.superclass;
        }
        Some(class)
    }

    /// Every linked superclass of `class` has a readable record (so a name
    /// [`Self::super_named`] did not find is not a superclass).
    pub(in crate::class_manager) fn superclass_chain_readable(&self, class: &Class) -> bool {
        let mut next = class.superclass;
        let mut depth = 0usize;
        while let Some(id) = next {
            let Some(sup) = self.class_store.get(id) else {
                return false;
            };
            depth += 1;
            if depth > 256 {
                return false;
            }
            next = sup.superclass;
        }
        true
    }

    /// The linked proper superclass of `class` named `name` (HotSpot's
    /// `name_in_supers`), walking the `ClassId` chain the define resolved.
    pub(in crate::class_manager) fn super_named(&self, class: &Class, name: &str) -> Option<&'a Class> {
        let mut next = class.superclass;
        let mut depth = 0usize;
        while let Some(id) = next {
            let sup = self.class_store.get(id)?;
            if &*sup.name == name {
                return Some(sup);
            }
            depth += 1;
            if depth > 256 {
                return None;
            }
            next = sup.superclass;
        }
        None
    }
}

impl<'a> ClassHierarchy for ClassStoreHierarchy<'a> {
    fn type_map_store(&self) -> Option<&crate::type_maps::TypeMapStore> {
        // The verifier publishes this VM's maps into this VM's store.
        Some(self.class_store.type_maps())
    }

    fn records_verifier_loads(&self) -> bool {
        self.verifier_loads.is_some()
    }

    fn note_verifier_load(&self, name: &str) {
        let Some(loads) = self.verifier_loads else {
            return;
        };
        // The class being verified answers its own name (HotSpot resolves it
        // through its loader too, which finds the class itself).
        if self.in_flight.is_some_and(|c| &*c.name == name) {
            return;
        }
        let mut loads = loads.borrow_mut();
        if !loads
            .iter()
            .any(|seen| matches!(seen, crate::vtype::VerifierLinkLoad::Name(n) if &**n == name))
        {
            loads.push(crate::vtype::VerifierLinkLoad::Name(
                cratonvm_types::intern_arc(name),
            ));
        }
    }

    fn catch_type_is_throwable(&self, name: &str) -> Option<bool> {
        // Interpreter round i5 wave 103, lane L7: only a catch type this
        // class's loader already sees is answered (the verifier loads
        // nothing); `is_subclass` keeps its loader tolerances.
        self.lookup(name)?;
        self.lookup("java/lang/Throwable")?;
        Some(self.is_subclass(name, "java/lang/Throwable"))
    }

    fn note_verifier_load_unless_interface(&self, target: &str, source: &str) {
        let Some(loads) = self.verifier_loads else {
            return;
        };
        if self.in_flight.is_some_and(|c| &*c.name == source) {
            return;
        }
        let mut loads = loads.borrow_mut();
        // An unconditional load of the source already covers this one.
        let covered = loads.iter().any(|seen| match seen {
            crate::vtype::VerifierLinkLoad::Name(n) => &**n == source,
            crate::vtype::VerifierLinkLoad::SourceUnlessInterface {
                target: t,
                source: s,
            } => &**t == target && &**s == source,
        });
        if !covered {
            loads.push(crate::vtype::VerifierLinkLoad::SourceUnlessInterface {
                target: cratonvm_types::intern_arc(target),
                source: cratonvm_types::intern_arc(source),
            });
        }
    }

    fn is_subclass(&self, child: &str, parent: &str) -> bool {
        if child == parent || parent == "java/lang/Object" {
            return true;
        }
        // A user loader can define an outer class while its bytecode first
        // references a nested subclass. If an app-loader copy of that nested
        // class already exists, name resolution sees both classes as loaded but
        // cannot observe the pending user-loader copy. The nested class will be
        // defined by the same loader before execution; reject neither javac's
        // valid outer→nested relationship nor resolve it against the app copy.
        if self.in_flight.is_some_and(|c| {
            (child.starts_with(&*c.name) && child.as_bytes().get(c.name.len()) == Some(&b'$'))
                || (c.name.as_ref() == parent
                    && child.starts_with(parent)
                    && child.as_bytes().get(parent.len()) == Some(&b'$'))
        }) {
            return true;
        }
        // Audit fix (HIGH #2): a missing class must NOT be unconditionally
        // reported as a subtype. Returning `true` for *every* unresolved
        // reference silently bypassed Pass-3 type-assignability checks for
        // untrusted classes.
        //
        // Regression fix: the blanket-`false` replacement was too strict —
        // it rejected valid bytecode where a not-yet-loaded JDK class (e.g.
        // `java/security/NoSuchAlgorithmException`) appears where a
        // supertype (`java/lang/Throwable`) is expected. The verifier runs
        // at class-define time and exception-table catch types do not
        // trigger class loading, so the child class is frequently absent
        // from `class_store` at verify time.
        //
        // The JDK's own class hierarchy is fixed and known, so when a
        // store-backed walk is not possible we fall back to walking the
        // static `jdk_superclass` table. That walk terminates at
        // `java/lang/Object` and only returns `true` for a *genuine*
        // ancestor relationship — an unrelated pair (e.g. `String` /
        // `Integer`) still resolves to `false`.
        let (child_id, parent_id) = (self.lookup_structural(child), self.lookup_structural(parent));
        if let (Some(child_id), Some(parent_id)) = (child_id, parent_id) {
            if let Some(c) = self.class_for(child_id) {
                if c.is_subclass_of(parent_id, self.class_store) {
                    return true;
                }
            }
            // A user loader can own both the class being verified and the
            // return/interface type by name, while an earlier class-linking
            // edge was conservatively bound to the app-loader copy before
            // the child copy was defined. Preserve the exact loader namespace
            // at verification time: walk the child's already-linked
            // superclass/interface graph and accept a structural edge whose
            // binary name is the requested type. Verification frames retain
            // binary names rather than loader-qualified identities, so this
            // structural proof is valid for any initiating loader.
            let mut pending = vec![child_id];
            let mut seen = Vec::new();
            while let Some(id) = pending.pop() {
                if seen.contains(&id) {
                    continue;
                }
                seen.push(id);
                let Some(class) = self.class_for(id) else {
                    continue;
                };
                if class.name.as_ref() == parent {
                    return true;
                }
                pending.extend(class.interfaces.iter().copied());
                if let Some(super_id) = class.superclass {
                    pending.push(super_id);
                }
            }
        }
        // Fallback: walk the static JDK superclass chain. Used when either
        // class is not yet loaded (so the store walk above could not run or
        // could not prove the relationship through unloaded ancestors).
        if jdk_name_is_subclass_with_hints(child, parent, self.app_workarounds) {
            return true;
        }
        // Last resort: a *non-JDK* class that is referenced but not yet
        // loaded cannot be proven here. javac-emitted bytecode under
        // verification is type-correct by construction; returning `false`
        // rejects valid forward references — e.g. `addShutdownHook(new
        // Thread(){ ... })`, where the anonymous `Foo$1 extends Thread` is
        // referenced by `Foo` before `Foo$1` is itself defined (Apache
        // Felix's `Main.main` does exactly this). Fall back to optimism for
        // that case only — when at least one side is genuinely unresolved —
        // matching the optimistic `ClassStoreHierarchy` in
        // `vm/src/vm/vm_util.rs`.
        if child_id.is_none() || parent_id.is_none() {
            return true;
        }
        false
    }

    fn is_direct_superclass(&self, child: &str, parent: &str) -> bool {
        if self
            .in_flight
            .is_some_and(|in_flight| in_flight.name.as_ref() == child)
        {
            return self.in_flight_super_name == Some(parent);
        }
        let Some(child_id) = self.lookup(child) else {
            return false;
        };
        let Some(child_class) = self.class_for(child_id) else {
            return false;
        };
        let Some(super_id) = child_class.superclass else {
            return false;
        };
        // The hierarchy is queried while `child_class` is still in flight.
        // Its direct parent can have a resolved ClassId before its Class
        // record is materialized in this store (notably java/lang/Object for
        // a freshly defined application class). Comparing the loader-aware
        // resolved ids retains the exact direct edge without requiring the
        // parent record itself to be present.
        if self
            .lookup(parent)
            .is_some_and(|parent_id| parent_id == super_id)
        {
            return true;
        }
        // The parent may have been resolved by a different initiating loader
        // and therefore not be returned by `lookup(parent)` for the current
        // verifier context. The loaded-class index still retains the exact
        // ClassId-to-binary-name association; use that reverse proof instead
        // of weakening constructor verification to any ancestor.
        if self
            .loaded_classes
            .iter()
            .any(|((_, name), id)| *id == super_id && name.as_ref() == parent)
        {
            return true;
        }
        if let Some(super_class) = self.class_for(super_id) {
            return super_class.name.as_ref() == parent;
        }
        // The bootstrap Object edge is represented by a reserved ClassId in
        // a few early definition paths, before an Object Class record is
        // materialized or indexed. A non-Object direct parent is loaded and
        // therefore handled by one of the exact checks above; accept only
        // this bootstrap representation, never a general ancestor lookup.
        parent == "java/lang/Object"
    }

    fn common_superclass(&self, a: &str, b: &str) -> String {
        if a == b {
            return a.to_string();
        }
        let fallback = || static_common_superclass_lookup(a, b);
        // Optimistic assignability of unresolved types cannot prove a common ancestor.
        if !self.is_resolvable(a) || !self.is_resolvable(b) {
            return fallback();
        }
        if self.is_subclass(a, b) {
            return b.to_string();
        }
        if self.is_subclass(b, a) {
            return a.to_string();
        }
        let fallback = || static_common_superclass_lookup_with_hints(a, b, self.app_workarounds);
        let Some(a_id) = self.lookup_structural(a) else {
            return fallback();
        };
        let a_cls = match self.class_for(a_id) {
            Some(c) => c,
            None => return fallback(),
        };
        let mut current = a_cls.superclass.and_then(|sid| self.class_for(sid));
        let mut depth = 0usize;
        while let Some(cls) = current {
            if depth > 256 {
                break;
            }
            depth += 1;
            if self.is_subclass(b, &cls.name) {
                return cls.name.to_string();
            }
            current = cls.superclass.and_then(|sid| self.class_for(sid));
        }
        if let Some(b_cls) = self.lookup_structural(b).and_then(|id| self.class_for(id)) {
            let mut current_b = b_cls.superclass.and_then(|sid| self.class_for(sid));
            let mut depth_b = 0usize;
            while let Some(cls) = current_b {
                if depth_b > 256 {
                    break;
                }
                depth_b += 1;
                if self.is_subclass(a, &cls.name) {
                    return cls.name.to_string();
                }
                current_b = cls.superclass.and_then(|sid| self.class_for(sid));
            }
        }
        fallback()
    }

    fn is_interface(&self, name: &str) -> bool {
        match self.lookup_structural(name).and_then(|id| self.class_for(id)) {
            Some(c) => c.is_interface(),
            None => false,
        }
    }

    fn is_resolvable(&self, name: &str) -> bool {
        // A reference type is "resolvable" for verification purposes when
        // it is already loaded (present in the store / in-flight) — only
        // then can `is_subclass` / `is_interface` give a definitive
        // answer. Array types are always considered resolvable (their
        // element resolution is handled elsewhere) so array assignability
        // is not loosened by this hook.
        if name.starts_with('[') {
            return true;
        }
        self.lookup_structural(name).is_some()
    }

    fn invokespecial_target(
        &self,
        current: &str,
        ref_class: &str,
    ) -> Option<crate::vtype::SpecialTarget> {
        use crate::vtype::SpecialTarget;
        let class = self.linkage_rule_class(current)?;
        if ref_class == current || ref_class.starts_with('[') {
            return Some(SpecialTarget::SameOrDirect);
        }
        // Every class is a subclass of Object (its record can be a bootstrap
        // placeholder the chain walk below cannot read).
        if ref_class == "java/lang/Object" {
            return Some(SpecialTarget::Assignable);
        }
        // `ref_class_type.equals(current_class()->super()->name())`: the
        // declared direct superclass name, which the define resolved.
        let direct_super = self.in_flight_super_name.or_else(|| {
            class
                .superclass
                .and_then(|id| self.class_store.get(id))
                .map(|sup| &*sup.name)
        });
        if direct_super == Some(ref_class) {
            return Some(SpecialTarget::SameOrDirect);
        }
        // `is_same_or_direct_interface`: the LOCAL interfaces only. A local
        // interface without a readable record leaves the question open.
        let mut local_interface_unknown = false;
        for &id in &class.interfaces {
            match self.class_store.get(id) {
                Some(iface) if &*iface.name == ref_class => {
                    return Some(SpecialTarget::SameOrDirect);
                }
                Some(_) => {}
                None => local_interface_unknown = true,
            }
        }
        // `ref_class_type.is_assignable_from(current_type())`: a farther
        // linked superclass by name (the loader resolves that name to the
        // superclass it linked), else the loaded class the name resolves to
        // through this loader: an interface is assignable from any class in
        // the split verifier, a class only through the superclass chain
        // already walked. An unloaded name gets no answer (HotSpot would load
        // it here; this verifier loads nothing).
        if self.super_named(class, ref_class).is_some() {
            return Some(SpecialTarget::Assignable);
        }
        if local_interface_unknown || !self.superclass_chain_readable(class) {
            return None;
        }
        let target = self.lookup(ref_class).and_then(|id| self.class_for(id))?;
        Some(if target.is_interface() {
            SpecialTarget::Assignable
        } else {
            SpecialTarget::NotAssignable
        })
    }

    fn protected_receiver_rule(
        &self,
        current: &str,
        ref_class: &str,
        name: &str,
        descriptor: &str,
        is_method: bool,
    ) -> bool {
        let Some(class) = self.linkage_rule_class(current) else {
            return false;
        };
        // `name_in_supers`; `load_class(ref_class_type)` then resolves the
        // name to that same linked superclass.
        let Some(target) = self.super_named(class, ref_class) else {
            return false;
        };
        // `is_protected_access`: the member the reference resolves to, from
        // the target upward (`uncached_lookup_method` walks the superclass
        // chain; `find_field` the JVMS §5.4.3.2 order), protected and in
        // another run-time package than the current class.
        //
        // JIT round 20 wave 17 (lane rev15): "another run-time package" as
        // the define-time supertype check and the override predicate judge
        // it (`method_override::same_defining_loader_in`: the built-in
        // loaders are one outside a split boot package), not by raw loader
        // ids, which this VM does not record as HotSpot's loaders one for
        // one; and only for a holder with real class bytes (a compatibility
        // stand-in's flags are fabricated). Either doubt answers "no rule":
        // a false refusal of a legal class is the worst outcome here.
        let other_runtime_package = |holder: &Class| -> bool {
            if !holder.origin.has_real_bytes() {
                return false;
            }
            let package = crate::access_control::runtime_package_name(class);
            package != crate::access_control::runtime_package_name(holder)
                || !crate::method_override::same_defining_loader_in(
                    self.class_store,
                    class.loader_id,
                    holder.loader_id,
                    package,
                )
        };
        if is_method {
            let mut next = Some(target);
            let mut depth = 0usize;
            while let Some(holder) = next {
                if let Some(method) = holder.find_method(name, descriptor) {
                    return method.access_flags.contains(MethodAccessFlags::PROTECTED)
                        && other_runtime_package(holder);
                }
                depth += 1;
                if depth > 256 {
                    return false;
                }
                next = holder.superclass.and_then(|id| self.class_store.get(id));
            }
            false
        } else {
            match crate::class::find_field_recursive_by_descriptor(
                target.id,
                name,
                descriptor,
                self.class_store,
            ) {
                Some((_, field, holder_id)) => {
                    field.access_flags.contains(FieldAccessFlags::PROTECTED)
                        && self
                            .class_store
                            .get(holder_id)
                            .is_some_and(other_runtime_package)
                }
                None => false,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// T6.3.1 — JVMTI class-lifecycle hook registry
// ---------------------------------------------------------------------------
//
// The class loader must notify JVMTI agents of ClassLoad (fired after the
// class is registered in the class store). ClassPrepare is not the class
// loader's to post: a class is prepared when it is LINKED, which the VM does
// (`vm_util::link_claimed_class`, which posts it to every consumer). Until
// interpreter round i1 wave 17 a second hook queued ClassPrepare here, at
// definition time, for every class defined, linked or not.
//
// `classloading` has no dependency on the VM crate where the JVMTI manager
// lives, so the VM registers a function pointer at boot time. When
// no hook is registered the class loader's hot path pays a single
// `AtomicBool` load and branches past the call — the cost of the hook is
// zero for embedded/test scenarios that do not attach an agent.
//
// The hooks receive `(class_id_u32, class_name, thread_id, owner_domain)`.
// `thread_id` is `current_thread_id(owner_domain)` (see below) when a VM
// thread of the defining store's VM has registered itself on this OS thread
// via [`set_current_thread_id`], else 0 — the documented "unknown/bootstrap"
// sentinel that the JVMTI layer routes to the VM-init thread. The three VM
// thread-creation sites (main thread in `vm_init.rs`, `Thread.start` workers
// in `vm_exec.rs`, foreign attach in `native/jni.rs`) call
// `set_current_thread_id` once, at registration, on the OS thread that will
// go on to run that Java thread's bytecode — so ordinary interpreter-driven
// class loading reports the real `jthread` an agent can key on to skip its
// own instrumentation thread.
//
// DEFERRED FIRING (2026-07-26, obsaudit D1). `fire_class_load_hook` does
// not call the installed hook synchronously. It pushes onto a thread-local
// queue; the queue is drained — and the hook
// actually invoked — only after the caller's L10 `ClassRealm::class_manager`
// write guard (`vm/src/vm/realms/class_realm.rs`) has been released. See
// `ClassRealm::class_manager_write` / `ClassManagerWriteGuard::drop`, which
// is now the *only* way to acquire that write lock (every former
// `.class_manager_write()` call site was mechanically renamed to
// `.class_manager_write()` so no site can bypass the drain).
//
// This was previously documented as a deliberate "fire under the guard"
// design, on the theory that deferring would require buffering and draining
// at every one of the ~50 `class_manager.write()` sites in `vm/`, any one of
// which could "miss the drain" and leak a stale event. That risk is what the
// guard-wrapper closes: there is now exactly one place a write guard can be
// obtained, and exactly one place it is dropped, so there is no site left
// that could miss the drain. `define_class` recursion through `load_class`
// (superclass/interface resolution) still holds `&mut self` across nested
// calls without re-locking, so nested class loads queue several events and
// they fire together, in push order, the instant the *outermost* guard for
// that acquisition is released — which is also the earliest instant another
// thread could observe the new class via a fresh `.read()`/`.write()`, so a
// listener that reacts to ClassLoad by immediately querying the class (e.g.
// `GetClassSignature`) no longer blocks on a lock its own event delivery is
// still holding.
//
// A hook may now freely re-enter the class manager (take a fresh read or
// write lock, call back into `load_class`, etc.) — by the time it runs, the
// lock that used to make that a self-deadlock has already been released on
// this thread. This matches the contract already documented for
// [`VtableInstallHookScoped`] below only insofar as both fire from the same
// place; unlike that hook, class-lifecycle hooks are no longer required to
// avoid calling back in.
//
// The in-tree adapter (`class_load_adapter`, `vm/src/vm/vm_init.rs`)
// forwards to `runtime::jvmti::fire_class_load_for_vm` with the VM named by
// the event's owner token, which takes that VM's JVMTI manager's own
// `callbacks` lock and invokes a `Box<dyn Fn>`.

/// Signature of the JVMTI class-lifecycle hook installed by the VM crate.
/// Parameters: `(class_id_u32, class_name, thread_id, owner_domain)`.
///
/// `owner_domain` is the defining manager's `ClassStore::layout_domain()`,
/// the owner token every classloading hook that needs a VM carries (the JIT
/// and resolution invalidate hooks above): `ClassId`s are allocated per
/// store, so without it the adapter cannot tell which VM's class was loaded,
/// and with two VMs in one process the event reached no agent at all
/// (interpreter round i1 wave 22, lane L5,
/// `interpreter-L5-classload-hook-carries-no-owner-token-FIXED-20260926.md`).
///
/// Called after the class manager's write guard for this define has been
/// released (see the DEFERRED FIRING notes on the hook registry above) — a
/// conforming hook may safely call back into the class manager.
pub type JvmtiClassHook = fn(u32, &str, u64, u32);

pub(in crate::class_manager) static CLASS_LOAD_HOOK: OnceLock<JvmtiClassHook> = OnceLock::new();
pub(in crate::class_manager) static CLASS_HOOKS_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Install the JVMTI `ClassLoad` hook. Idempotent — only the first install
/// wins. Called once by the VM during `SharedVm::new`.
pub fn install_class_load_hook(hook: JvmtiClassHook) {
    if CLASS_LOAD_HOOK.set(hook).is_ok() {
        CLASS_HOOKS_ACTIVE.store(true, Ordering::Release);
    }
}

thread_local! {
    /// This OS thread's `ThreadId.0` in each VM it registered with, as
    /// `(owner layout domain, id)` pairs set by [`set_current_thread_id`],
    /// most recent last. A VM absent from the list reads 0, which means "no
    /// VM thread has registered on this OS thread yet" — bootstrap class
    /// loading before `SharedVm::new` finishes, or a background/pool thread
    /// that never became a Java thread.
    ///
    /// Keyed by the VM (its class store's layout domain, the owner token the
    /// class-load hook carries) since interpreter round i1 wave 23, lane L5:
    /// one `Cell<u64>` per OS thread made an embedder thread that attached to
    /// VM A and then to VM B (JNI `AttachCurrentThread` into both, or VM B
    /// created on it) report VM B's `ThreadId` in VM A's `ClassLoad` events.
    /// Bounded by [`MAX_THREAD_ID_BINDINGS`]: a thread that outlives many
    /// sequential VMs drops the oldest binding (domains are never reused, so
    /// a dropped VM's entry can only go stale, never answer for another VM).
    pub(in crate::class_manager) static CURRENT_VM_THREAD_IDS: RefCell<Vec<(u32, u64)>> = const { RefCell::new(Vec::new()) };

    /// Class-lifecycle events queued by [`fire_class_load_hook`] on this OS
    /// thread, awaiting drain by
    /// [`drain_pending_class_hooks`] once the write guard that produced them
    /// is released. See the DEFERRED FIRING notes above.
    pub(in crate::class_manager) static PENDING_CLASS_HOOKS: RefCell<Vec<PendingClassHook>> = const { RefCell::new(Vec::new()) };
}

pub(in crate::class_manager) enum PendingClassHook {
    /// `(class_id, class_name, thread_id, owner_domain)`; see [`JvmtiClassHook`].
    Load(u32, String, u64, u32),
}

/// How many VMs one OS thread's [`CURRENT_VM_THREAD_IDS`] remembers.
pub(in crate::class_manager) const MAX_THREAD_ID_BINDINGS: usize = 16;

/// Bind this OS thread's current `jthread` in the VM whose class store has
/// layout domain `owner_domain`, for class-lifecycle event attribution.
/// Called once by the VM at Java-thread registration (main thread,
/// `Thread.start` workers, foreign JNI attach, the VM's service threads) —
/// see the hook registry notes above. `id` is that VM's `ThreadId.0`; a
/// binding in another VM is left alone.
pub fn set_current_thread_id(owner_domain: u32, id: u64) {
    CURRENT_VM_THREAD_IDS.with(|c| {
        let mut bindings = c.borrow_mut();
        bindings.retain(|&(domain, _)| domain != owner_domain);
        if bindings.len() >= MAX_THREAD_ID_BINDINGS {
            bindings.remove(0);
        }
        bindings.push((owner_domain, id));
    });
}

/// This OS thread's bound `jthread` in the VM that owns `owner_domain`, or 0
/// if none has registered there (bootstrap / non-Java thread) — the
/// documented "unknown" sentinel.
pub(in crate::class_manager) fn current_thread_id(owner_domain: u32) -> u64 {
    CURRENT_VM_THREAD_IDS.with(|c| {
        c.borrow()
            .iter()
            .rev()
            .find(|&&(domain, _)| domain == owner_domain)
            .map_or(0, |&(_, id)| id)
    })
}

/// Invoke the class-load hook if one is installed. Hot path: a single
/// relaxed atomic load + branch when no agent is attached. Queues rather
/// than fires — see the DEFERRED FIRING notes above. `owner_domain` is the
/// defining store's layout domain ([`JvmtiClassHook`]).
#[inline]
pub(in crate::class_manager) fn fire_class_load_hook(class_id: u32, class_name: &str, thread_id: u64, owner_domain: u32) {
    if !CLASS_HOOKS_ACTIVE.load(Ordering::Acquire) {
        return;
    }
    if CLASS_LOAD_HOOK.get().is_some() {
        PENDING_CLASS_HOOKS.with(|q| {
            q.borrow_mut().push(PendingClassHook::Load(
                class_id,
                class_name.to_string(),
                thread_id,
                owner_domain,
            ))
        });
    }
}

/// Drain and fire every class-lifecycle event queued on this OS thread by
/// [`fire_class_load_hook`]. Must be called
/// after (never while holding) the L10 `class_manager` write guard that
/// produced the events — [`ClassManagerWriteGuard`] in
/// `vm/src/vm/realms/class_realm.rs` is the sole caller in production code.
///
/// Drains in FIFO (push) order via `Vec::drain`, so a hook that itself
/// triggers new class loads (now legal — see DEFERRED FIRING above) has its
/// own queued events appended and drained within the same call rather than
/// interleaved with the ones already in flight.
pub fn drain_pending_class_hooks() {
    if !CLASS_HOOKS_ACTIVE.load(Ordering::Acquire) {
        return;
    }
    PENDING_CLASS_HOOKS.with(|q| {
        let mut i = 0;
        loop {
            let next = {
                let mut queue = q.borrow_mut();
                if i >= queue.len() {
                    if i > 0 {
                        queue.clear();
                    }
                    break;
                }
                std::mem::replace(
                    &mut queue[i],
                    PendingClassHook::Load(0, String::new(), 0, 0),
                )
            };
            i += 1;
            match next {
                PendingClassHook::Load(class_id, name, thread_id, owner_domain) => {
                    if let Some(hook) = CLASS_LOAD_HOOK.get() {
                        hook(class_id, &name, thread_id, owner_domain);
                    }
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// WP2.4-B — JVMTI ClassFileLoadHook hook registry
// ---------------------------------------------------------------------------
//
// The JVMTI `ClassFileLoadHook` event fires whenever class bytes are about
// to be installed into the VM, with a chance for an instrumentation agent
// to substitute its own bytes. The hook receives the OLD bytes (or empty
// for the initial define) and the NEW bytes; the agent may return a
// modified Vec<u8>, which the class loader then uses in place of the
// supplied `new_bytes`.
//
// `Vec<u8>` is used instead of `&[u8]` for the return value so the hook can
// hand back ownership of a freshly-allocated transformed buffer without a
// borrow-vs-lifetime puzzle. An empty returned vec means "no transform —
// keep the original new_bytes". The class loader detects this and skips
// the substitution.

/// Signature of the JVMTI `ClassFileLoadHook` callback installed by the VM.
///
/// Parameters: `(class_id, class_name, old_bytes, new_bytes)`.
/// Return: an optional transformed byte vec. `None` (or an empty `Some`) =
/// no transform; the loader uses `new_bytes` as-is.
///
/// `old_bytes` is empty for the initial class load (no prior bytes exist).
/// On `redefine_class`, `old_bytes` is the previously-installed class file.
pub type ClassFileLoadHook = fn(u32, &str, &[u8], &[u8]) -> Option<Vec<u8>>;

pub(in crate::class_manager) static CLASS_FILE_LOAD_HOOK: OnceLock<ClassFileLoadHook> = OnceLock::new();
pub(in crate::class_manager) static CLASS_FILE_LOAD_HOOK_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Install the JVMTI `ClassFileLoadHook`. Idempotent — only the first
/// install wins. Called once by the VM during `SharedVm::new` if a JVMTI
/// agent has registered for this event.
pub fn install_class_file_load_hook(hook: ClassFileLoadHook) {
    if CLASS_FILE_LOAD_HOOK.set(hook).is_ok() {
        CLASS_FILE_LOAD_HOOK_ACTIVE.store(true, Ordering::Release);
    }
}

/// Invoke the ClassFileLoadHook if one is installed. When no hook is
/// registered the cost is a single relaxed atomic load + branch.
///
/// Returns `Some(new_vec)` if the hook transformed the bytes, otherwise
/// `None` (caller should use the original `new_bytes`).
#[inline]
pub(in crate::class_manager) fn fire_class_file_load_hook(
    class_id: u32,
    class_name: &str,
    old_bytes: &[u8],
    new_bytes: &[u8],
) -> Option<Vec<u8>> {
    if !CLASS_FILE_LOAD_HOOK_ACTIVE.load(Ordering::Acquire) {
        return None;
    }
    let hook = CLASS_FILE_LOAD_HOOK.get()?;
    let out = hook(class_id, class_name, old_bytes, new_bytes)?;
    if out.is_empty() {
        // Hook returned an empty vec → "no transform". Treat as None so the
        // caller uses the original bytes (which are guaranteed non-empty
        // by the upstream `bytes.len() < 8` rejection).
        None
    } else {
        Some(out)
    }
}

/// Fingerprint a class file as `(length, content hash)`.
///
/// Non-cryptographic on purpose: this distinguishes two *builds* of the same
/// class, not an attacker's forgery — the bytes it guards are already going
/// through the full class-file parser and the Pass-2/Pass-3 verifier. FxHash
/// over a class file is a handful of microseconds, which matters because this
/// runs once per `define_class`.
pub(in crate::class_manager) fn class_bytes_digest(bytes: &[u8]) -> (u32, u64) {
    use std::hash::Hasher;
    let mut hasher = crate::fx_hash::FxHasher::default();
    hasher.write(bytes);
    // Length is carried separately rather than folded in: a length mismatch is
    // the common case and reads far better in a diagnostic than a hash diff.
    (bytes.len() as u32, hasher.finish())
}

/// zr z11/md (2026-10-13), `CRATONVM_CLASS_CODE_COMPACT` (default ON, `=0`
/// kill): re-home every byte view a decoded member attribute holds into
/// `class_bytes` -- each method's `Code` bytecode, the `StackMapTable` and
/// unknown attributes nested in it, and unknown member attributes -- into one
/// compact buffer of their own, so the class's metadata stops pinning the
/// whole class file.
///
/// The reader builds those views over the class file without copying
/// (`ByteView`); after the define, nothing else holds the file but the
/// class-bytes cache, so with the views pinning it the file was resident for
/// the class's lifetime even after the cache evicted it (or, with
/// `CRATONVM_BOOT_CLASS_BYTES_REFETCH`, never kept it). For a java.base class
/// that is ~4.1 KB pinned to keep ~0.56 KB of bytecode and stack maps
/// (`classloading/tests/zr8_fm_class_metadata_census.rs`, `zr11md-inside-buffers`).
///
/// Every view keeps its exact bytes; only the buffer behind it changes. Views
/// into some other buffer, and a class file backed by an external mapping (a
/// stored JAR entry: file pages, not heap), are left alone. Returns the number
/// of bytes the compact buffer holds (0 when nothing was re-homed).
pub(crate) fn compact_member_attribute_views(
    class_file: &mut cratonvm_reader::ClassFile,
    class_bytes: &SharedBytes,
) -> usize {
    use cratonvm_reader::attribute::{Attribute, LazyAttribute};
    use cratonvm_reader::ByteView;

    fn attribute_views<'a>(a: &'a mut Attribute, out: &mut Vec<&'a mut ByteView>) {
        match a {
            Attribute::Code(code) => {
                let code = &mut **code;
                out.push(&mut code.code);
                for nested in code.attributes.iter_mut() {
                    attribute_views(nested, out);
                }
            }
            Attribute::StackMapTable { entries } => out.push(&mut **entries),
            Attribute::Unknown { data, .. } => out.push(&mut **data),
            _ => {}
        }
    }

    if class_bytes.is_external() {
        return 0;
    }
    let file_at = class_bytes.as_ref().as_ptr();
    let file_len = class_bytes.len();
    let mut views: Vec<&mut ByteView> = Vec::new();
    let members = class_file
        .methods
        .iter_mut()
        .map(|m| &mut m.attributes)
        .chain(class_file.fields.iter_mut().map(|f| &mut f.attributes));
    for attributes in members {
        for lazy in attributes.iter_mut() {
            if let LazyAttribute::Decoded(attribute) = lazy {
                attribute_views(attribute, &mut views);
            }
        }
    }
    views.retain(|v| {
        let backing = v.backing();
        backing.as_ref().as_ptr() == file_at && backing.len() == file_len
    });
    let total: usize = views.iter().map(|v| v.len()).sum();
    if views.is_empty() || total >= file_len {
        return 0;
    }
    let mut packed = Vec::with_capacity(total);
    let mut ranges = Vec::with_capacity(views.len());
    for v in &views {
        let start = packed.len();
        packed.extend_from_slice(v.as_bytes());
        ranges.push(start..packed.len());
    }
    let packed = SharedBytes::from(packed);
    for (v, range) in views.into_iter().zip(ranges) {
        // In range by construction: `ranges` partition `packed`.
        if let Ok(view) = ByteView::try_new(packed.clone(), range) {
            *v = view;
        }
    }
    total
}

/// Write the class bytes a `redefine_class` verification rejected to
/// `$CRATONVM_DBG_REDEFINE_DUMP/<mangled-name>.class`, so the rejection can be
/// disassembled with `javap` instead of reasoned about from an offset.
///
/// Off unless the variable names a directory; every failure is best-effort and
/// silent, because this runs on a path that is already returning an error and
/// must not turn a diagnosable rejection into a second one.
pub(in crate::class_manager) fn dump_rejected_redefine_bytes(class_name: &str, bytes: &[u8]) {
    let Ok(dir) = Err::<String, std::env::VarError>(std::env::VarError::NotPresent) else {
        return;
    };
    if dir.is_empty() {
        return;
    }
    let _ = std::fs::create_dir_all(&dir);
    let safe: String = class_name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    // A class can be retransformed more than once in a run and fail every
    // time; a counter keeps the later attempts from overwriting the first.
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let path = std::path::Path::new(&dir).join(format!("{safe}.{n}.class"));
    if std::fs::write(&path, bytes).is_ok() {
        eprintln!(
            "[REDEFINE-DUMP] rejected bytes for {class_name} ({} bytes) -> {}",
            bytes.len(),
            path.display()
        );
    }
}

// ---------------------------------------------------------------------------
// WP2.4-B — JIT cache invalidation hook
// ---------------------------------------------------------------------------
//
// When a class is redefined, every JIT-compiled body keyed on its old
// (class_id, method_index) pair must be discarded so the next call
// recompiles from the new bytecode. The VM owns the JIT cache; the class
// loader knows when a redefine completes — a hook bridges the two.

/// Signature of the JIT-invalidation hook fired by `redefine_class`.
///
/// Parameter: the `ClassId` (as `u32`) whose JIT entries must be evicted.
/// The VM-side adapter walks `shared.jit.jit_cache`, `shared.tiered`, and any
/// per-thread invoke caches that key by class id and removes matching
/// entries. Method-index granularity is intentionally NOT exposed here —
/// at redefine time we conservatively evict every method body for the
/// class because their bodies all changed.
///
/// Second parameter: the firing manager's `ClassStore::layout_domain()`, an
/// owner token naming the VM whose class it is. `ClassId`s are allocated per
/// store, so the same id names an unrelated class in every other VM; the
/// adapter flushes only the VM that owns the token, and every VM when no VM
/// does (interpreter round i1 wave 18,
/// `interpreter-layout-change-hook-flushes-every-live-vm-FIXED-20260925.md`).
pub type JitInvalidateHook = fn(u32, u32);

pub(in crate::class_manager) static JIT_INVALIDATE_HOOK: OnceLock<JitInvalidateHook> = OnceLock::new();
pub(in crate::class_manager) static JIT_INVALIDATE_HOOK_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Set the first time ANY class is redefined in place (JVMTI
/// RedefineClasses / RetransformClasses). The interpreter's native/intrinsic
/// shadowing guards read this as a one-instruction fast-path: in the common
/// case (no agent ever redefines a class) it stays `false` and the per-class
/// generation check is skipped entirely. Once an agent (e.g. Mockito's inline
/// mock maker) redefines a JDK class, the woven bytecode must run so the
/// instrumentation advice fires — the shadowing guards then consult the
/// per-class `redefine_generations` counter. Never reset (a redefined class
/// stays observably redefined for the VM's lifetime).
pub(in crate::class_manager) static ANY_CLASS_REDEFINED: AtomicBool = AtomicBool::new(false);

/// True once any class has been redefined in place. Single relaxed load —
/// the guard fast-path for native/intrinsic shadow suppression.
#[inline]
pub fn any_class_redefined() -> bool {
    ANY_CLASS_REDEFINED.load(Ordering::Relaxed)
}

/// How many times a class has been redefined in place, counting twice per
/// redefinition: [`ClassManager::redefine_class`] advances it before it
/// replaces the class's constant pool and again after it has swept the
/// shared resolution memo, and the VM's `redefine_class_with` advances it
/// once more after the JIT's own flush. Process-wide, like
/// [`class_definition_epoch`]: a redefinition in another VM only costs a
/// cache flush here.
///
/// It is a cache-validity stamp, not policy. A redefinition replaces a
/// class's constant pool under an unchanged `ClassId`, so a cache keyed on
/// the CALLER's `(class, cp index)` may now name another member — the
/// invoke cache's `(caller, cp index)` entries and the compiled `ldc` slot
/// memo among them (interpreter round i1 wave 18, lane L4). Such a cache
/// snapshots this counter before it resolves and drops what it filled once
/// the counter moves, where it used to switch itself off for the rest of the
/// process on [`any_class_redefined`]. Since interpreter round i1 wave 20
/// every advance also records WHICH class it was for
/// ([`for_each_redefined_class_between`]), so such a cache can drop only the
/// redefined callers' entries.
pub(in crate::class_manager) static CLASS_REDEFINITIONS: AtomicU64 = AtomicU64::new(0);

/// See `CLASS_REDEFINITIONS`. One `Acquire` load; pairs with the `Release`
/// advance, so a reader that sees the new count also sees the class state the
/// redefinition wrote before advancing it.
#[inline]
pub fn class_redefinition_count() -> u64 {
    CLASS_REDEFINITIONS.load(Ordering::Acquire)
}

/// Advance `CLASS_REDEFINITIONS` for a redefinition this caller cannot name:
/// its record in the [`for_each_redefined_class_between`] ring retires every
/// caller. Prefer [`bump_class_redefinition_count_of`].
#[inline]
pub fn bump_class_redefinition_count() {
    record_redefinition(REDEFINED_ANY_CLASS, None);
}

/// Advance `CLASS_REDEFINITIONS` for a redefinition of `class_id`, and record
/// the class in the ring, so a cache keyed on callers retires only
/// `class_id`'s entries (interpreter round i1 wave 20, lane L2). Called only
/// by the redefinition path.
#[inline]
pub fn bump_class_redefinition_count_of(class_id: ClassId) {
    record_redefinition(class_id.as_u32(), None);
}

/// Slots in the redefinition ring. Each redefinition takes three (see
/// `CLASS_REDEFINITIONS`), so a reader may fall 85 redefinitions behind
/// before it has to retire everything.
pub(in crate::class_manager) const REDEFINITION_RING_LEN: usize = 256;

/// The class id a record carries when the bumper named no class.
pub(in crate::class_manager) const REDEFINED_ANY_CLASS: u32 = u32::MAX;

/// One ring slot: the count the record was published under (`0` while a
/// writer is replacing it; counts start at 1) and the redefined class id.
pub(in crate::class_manager) struct RedefinitionRecord {
    pub(in crate::class_manager) count: AtomicU64,
    pub(in crate::class_manager) class_id: AtomicU32,
}

pub(in crate::class_manager) static REDEFINITION_RING: [RedefinitionRecord; REDEFINITION_RING_LEN] = [const {
    RedefinitionRecord {
        count: AtomicU64::new(0),
        class_id: AtomicU32::new(0),
    }
}; REDEFINITION_RING_LEN];

/// Serialises the ring's writers, so a slot is never written by two of them
/// at once and the count moves by exactly one per record. A leaf lock held
/// for four stores; redefinitions are rare.
pub(in crate::class_manager) static REDEFINITION_RING_WRITER: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// Publish the record for the next count, THEN advance the count, so a reader
/// that sees the count finds its record unless the ring has lapped it.
///
/// `vm` is the redefining VM's own log (nio21 wave 3 L1, see
/// [`RedefinitionLog`]): it gets the same record, stamped with the same
/// count, before the count is advanced, under the same writer lock. `None`
/// for a bumper with no VM at hand (the unattributed
/// [`bump_class_redefinition_count`] and the free
/// [`bump_class_redefinition_count_of`]): only the process ring then knows,
/// and a per-VM reader that relies on its log alone would miss it -- which is
/// why every redefinition path of a VM goes through
/// [`ClassManager::bump_redefinition_count_of`].
pub(in crate::class_manager) fn record_redefinition(class_id: u32, vm: Option<&RedefinitionLog>) {
    let _writer = REDEFINITION_RING_WRITER.lock();
    let next = CLASS_REDEFINITIONS.load(Ordering::Relaxed) + 1;
    let record = &REDEFINITION_RING[next as usize % REDEFINITION_RING_LEN];
    // Seqlock write: invalidate, fence, payload, publish.
    record.count.store(0, Ordering::Relaxed);
    std::sync::atomic::fence(Ordering::Release);
    record.class_id.store(class_id, Ordering::Relaxed);
    record.count.store(next, Ordering::Release);
    if let Some(log) = vm {
        log.record(next, class_id);
    }
    CLASS_REDEFINITIONS.store(next, Ordering::Release);
}

/// One VM's redefinitions, stamped on the process clock (nio21 wave 3 L1;
/// `docs/feature-designs/class-resolution-and-caching.md`).
///
/// # Why the clock stays process-wide and the record does not
///
/// [`class_redefinition_count`] is read as a STAMP at about 150 sites in five
/// crates (fill snapshots, frame stamps, compiled-code `pool_generation`s,
/// the JIT's constant-pool stamp source), and every one compares its stamps
/// with the same counter. Moving that counter onto the VM in one step means
/// every reader in one change, or a reader left on a counter that never moves
/// again: a stale cache with no error. So the counter stays the CLOCK -- one
/// monotonic number every stamp in the process is taken from, which is all a
/// stamp needs -- and what was process-wide about the answer, "which classes
/// were redefined since my stamp", moves here: this VM's own ring of
/// `(clock stamp, class)` records. A reader that holds its VM's log asks it
/// instead of the process ring, keeps its stamps exactly as before, and then
/// retires only for its own VM's classes; a reader that does not is
/// unchanged (correct, and over-eager across VMs). The readers migrate one at
/// a time.
///
/// Records are published under the process ring's writer lock (one writer at a
/// time), each BEFORE the clock is advanced to its stamp, so a reader that sees
/// the clock at `t` finds every record of this VM stamped `<= t` unless the
/// ring has lapped it. A record stamped past the reader's `upto` is ignored:
/// the reader's next look, at a later clock, sees it.
pub struct RedefinitionLog {
    /// Records published so far (the newest record's sequence number; `0`
    /// while this VM has redefined nothing).
    pub(in crate::class_manager) seq: AtomicU64,
    pub(in crate::class_manager) ring: Box<[VmRedefinitionRecord]>,
}

/// One [`RedefinitionLog`] slot: the sequence number it was published under
/// (`0` while a writer replaces it), the clock stamp and the class id.
pub(in crate::class_manager) struct VmRedefinitionRecord {
    pub(in crate::class_manager) seq: AtomicU64,
    pub(in crate::class_manager) stamp: AtomicU64,
    pub(in crate::class_manager) class_id: AtomicU32,
}

impl RedefinitionLog {
    /// An empty log, shared by `Arc` between the class manager and the VM.
    pub fn new_shared() -> Arc<Self> {
        Arc::new(Self {
            seq: AtomicU64::new(0),
            ring: (0..REDEFINITION_RING_LEN)
                .map(|_| VmRedefinitionRecord {
                    seq: AtomicU64::new(0),
                    stamp: AtomicU64::new(0),
                    class_id: AtomicU32::new(0),
                })
                .collect(),
        })
    }

    /// How many redefinition records this VM has published (each
    /// redefinition publishes three, as on the process clock).
    #[inline]
    pub fn records(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }

    /// Publish the record stamped `stamp`. Caller: [`record_redefinition`],
    /// under the writer lock (one writer at a time).
    pub(in crate::class_manager) fn record(&self, stamp: u64, class_id: u32) {
        let next = self.seq.load(Ordering::Relaxed) + 1;
        let slot = &self.ring[next as usize % REDEFINITION_RING_LEN];
        // Seqlock write, as the process ring's.
        slot.seq.store(0, Ordering::Relaxed);
        std::sync::atomic::fence(Ordering::Release);
        slot.stamp.store(stamp, Ordering::Relaxed);
        slot.class_id.store(class_id, Ordering::Relaxed);
        slot.seq.store(next, Ordering::Release);
        self.seq.store(next, Ordering::Release);
    }

    /// The record numbered `seq`: `(stamp, class id)`, or `None` when the
    /// ring no longer holds it.
    pub(in crate::class_manager) fn read(&self, seq: u64) -> Option<(u64, u32)> {
        let slot = &self.ring[seq as usize % REDEFINITION_RING_LEN];
        if slot.seq.load(Ordering::Acquire) != seq {
            return None;
        }
        let stamp = slot.stamp.load(Ordering::Relaxed);
        let class_id = slot.class_id.load(Ordering::Relaxed);
        std::sync::atomic::fence(Ordering::Acquire);
        if slot.seq.load(Ordering::Relaxed) != seq {
            return None;
        }
        Some((stamp, class_id))
    }

    /// Visit the class of every redefinition of THIS VM stamped in `(after,
    /// upto]` on the process clock (a class may be visited more than once).
    /// `true` with no visit: this VM redefined nothing in the span, whatever
    /// other VMs did. `false` when the log cannot answer -- a backwards span,
    /// a record the ring has lapped, or a record that named no class -- and
    /// the caller must treat every class of this VM as redefined.
    pub fn for_each_between(&self, after: u64, upto: u64, mut visit: impl FnMut(ClassId)) -> bool {
        if upto < after {
            return false;
        }
        let newest = self.seq.load(Ordering::Acquire);
        let len = REDEFINITION_RING_LEN as u64;
        let oldest_kept = if newest >= len { newest - len + 1 } else { 1 };
        let mut seq = newest;
        while seq >= oldest_kept && seq > 0 {
            let Some((stamp, class_id)) = self.read(seq) else {
                return false;
            };
            if stamp <= after {
                return true;
            }
            if stamp <= upto {
                if class_id == REDEFINED_ANY_CLASS {
                    return false;
                }
                visit(ClassId::new(class_id));
            }
            seq -= 1;
        }
        // Every kept record is newer than `after`: answerable only when the
        // log never dropped one (records `1..=len` all fit the ring).
        newest <= len
    }

    /// Test-only: publish a record stamped `stamp` without the process clock
    /// (the classloading tests' per-thread stand-in clock pairs with it).
    #[cfg(test)]
    pub(crate) fn record_for_test(&self, stamp: u64, class_id: ClassId) {
        let _writer = REDEFINITION_RING_WRITER.lock();
        self.record(stamp, class_id.as_u32());
    }
}

/// The class the redefinition numbered `count` recorded: `None` when the ring
/// no longer (or not yet) holds it.
pub(in crate::class_manager) fn redefinition_record(count: u64) -> Option<u32> {
    let record = &REDEFINITION_RING[count as usize % REDEFINITION_RING_LEN];
    if record.count.load(Ordering::Acquire) != count {
        return None;
    }
    let class_id = record.class_id.load(Ordering::Relaxed);
    std::sync::atomic::fence(Ordering::Acquire);
    if record.count.load(Ordering::Relaxed) != count {
        return None;
    }
    Some(class_id)
}

/// Visit the class of every redefinition counted in `(after, upto]` (a class
/// may be visited more than once). `false` when the ring cannot answer —
/// the span is longer than the ring, a record was overwritten, or a bumper
/// named no class — and the caller must treat every class as redefined.
///
/// A `ClassId` is per class store, so a visited id may be another VM's
/// class: retiring by it over-retires there, which costs a re-resolve.
pub fn for_each_redefined_class_between(
    after: u64,
    upto: u64,
    mut visit: impl FnMut(ClassId),
) -> bool {
    if upto < after || upto - after > REDEFINITION_RING_LEN as u64 {
        return false;
    }
    for count in after + 1..=upto {
        match redefinition_record(count) {
            Some(REDEFINED_ANY_CLASS) | None => return false,
            Some(class_id) => visit(ClassId::new(class_id)),        }    }
    true
}

/// Set the first time two *distinct* `ClassId`s are simultaneously loaded
/// under the same binary name — i.e. the first time a loader split produces a
/// duplicate-name class.
///
/// # Why this exists
///
/// `interpreter::field_access::retarget_instance_field_to_receiver` runs on
/// **every** `getfield`/`putfield` whose receiver class differs from the
/// resolved field's declaring class — the ordinary case for any *inherited*
/// field, which is most of them in real OO bytecode. Its whole job is the
/// loader-split case: it bails immediately unless the receiver class and the
/// resolved declaring class have the **same name** under **different**
/// `ClassId`s. Reaching that bail costs a `class_manager` read lock, three
/// class lookups and a constant-pool walk, and it is paid per field access.
///
/// Measured on `probes/FieldShape.java` (`--nojit`, min-of-9, arms
/// interleaved in both orders): an inherited-field get+put pair cost 54 / 78
/// / 103 ns more than an own-class pair across three runs, with nothing else
/// differing between the arms. `CRATONVM_DBG_HOTPATH_COUNTS=1` reported
/// `retarget_field=3000000` on a run with exactly three million field
/// accesses — one call per access.
///
/// This latch answers "can that situation exist at all in this process" with
/// one relaxed load. It is maintained precisely rather than heuristically:
/// `ClassManager::name_definitions` already indexes name -> set of defining
/// `ClassId`s (two loaders mapping one name to the *same* id still reads as
/// unique, by design), so the condition is exactly "some name has more than
/// one distinct id".
///
/// # Never reset
///
/// Unloading a duplicate can take a name back down to one id, but the latch
/// stays raised. That direction is the safe one: a raised latch merely
/// restores the pre-latch behaviour (the full retarget walk runs and answers
/// correctly), whereas a lowered one could skip a retarget that was still
/// needed. Same policy, and the same reasoning, as [`any_class_redefined`].
pub(in crate::class_manager) static ANY_DUPLICATE_CLASS_NAME: AtomicBool = AtomicBool::new(false);

/// Words in a class-id bitmap ([`HiddenClassBits`]): 1 M class ids, 128 KiB
/// per class manager (zeroed on allocation; pages a VM never defines into stay
/// untouched).
///
/// The first such bitmap, `JavaUtilClassBits` ("this class's name starts with
/// `java/util/`", read by the virtual-invoke door's tier-up gate), was deleted
/// with that class-name allow-list by JIT r22 INLINE22-5.
pub(in crate::class_manager) const CLASS_ID_BITMAP_WORDS: usize = 1 << 14;

/// Which of one class manager's class ids were defined (or later marked)
/// HIDDEN, readable without the class-manager lock (JIT round 21 wave 14, lane
/// hidden-static; `docs/feature-designs/class-resolution-and-caching.md`).
/// The VM's `ClassRealm::hidden_classes` holds the handle
/// ([`ClassManager::hidden_class_bits`]); the compiled dispatch paths ask it
/// before they take the lock to compare a hidden caller's name with a
/// constant-pool owner (`jit::helpers::jit_hidden_self_owner`).
///
/// A prefilter, never an answer: a set bit is confirmed under the lock, so
/// only a MISSING bit could matter, and every way a class becomes hidden sets
/// it (the define with `DefineClassOptions::hidden`, and
/// [`ClassManager::mark_class_hidden`]). Bits are only ever set, and a store
/// never reuses a `ClassId`. Capacity [`CLASS_ID_BITMAP_WORDS`]. An id past
/// the capacity (including the synthetic
/// lambda-proxy ids at `0x8000_0000` and up) answers `Some(false)` while no
/// hidden class past the capacity was ever noted ([`Self::past_capacity`]),
/// and `None` (the caller asks under the lock) only after one was: so an
/// ordinary class pays one load whatever its id (JIT round 21 wave 21, item 5
/// of `r21-review-w21-findings-FIXED-20261020.md`).
#[derive(Clone)]
pub struct HiddenClassBits {
    pub(in crate::class_manager) words: Arc<[AtomicU64]>,
    /// Set (`Release`, before the class is published) when a hidden class's
    /// id falls past the bitmap; until then every such id is known ordinary.
    pub(in crate::class_manager) past_capacity: Arc<AtomicBool>,
    /// `CRATONVM_HIDDEN_SELF_CP_BY_ID`, read once per VM on its first ask
    /// ([`Self::self_cp_by_id`]): `0` not read yet, `1` on, `2` off. Shared by
    /// every handle of this manager's bitmap, so it is per VM, never a
    /// process global (JIT round 21 wave 20, lane hidden-self-resolve).
    pub(in crate::class_manager) self_cp_by_id: Arc<std::sync::atomic::AtomicU8>,
    /// The lock-free self-name prefilter (JIT round 21 wave 22, lane
    /// hidden-resid; item 6 of `r21-review-w15-native-findings-FIXED-20261020.md`):
    /// a direct-mapped table, slot `id % HIDDEN_SELF_NAME_SLOTS`, of
    /// [`hidden_self_name_slot`] words -- a hidden class's id and a 32-bit
    /// hash of its RECORDED class-file name -- written under the class-manager
    /// write lock wherever the recorded name is set
    /// ([`Self::record_self_name`]). See [`Self::self_name_rules_out`].
    pub(in crate::class_manager) self_names: Arc<[AtomicU64]>,
    /// `CRATONVM_JIT_HIDDEN_SELF_NAME_PREFILTER` (opt-in, perf), read once per
    /// VM like [`Self::self_cp_by_id`]: `0` not read yet, `1` on, `2` off.
    pub(in crate::class_manager) self_name_prefilter: Arc<std::sync::atomic::AtomicU8>,
    /// `CRATONVM_JIT_HIDDEN_SELF_RESOLVE_BY_ID` (JIT r22 wave 2/3, lane inline;
    /// perf, opt-in), read once per VM like [`Self::self_name_prefilter`]:
    /// `0` not read yet, `1` on, `2` off. See [`Self::jit_self_resolve_by_id`].
    pub(in crate::class_manager) jit_self_resolve_by_id: Arc<std::sync::atomic::AtomicU8>,
    /// `CRATONVM_JIT_HIDDEN_SELF_HINT_BY_ID` (JIT r22, lane inline; perf,
    /// opt-in), read once per VM: `0` not read yet, `1` on, `2` off. See
    /// [`Self::jit_self_hint_by_id`].
    pub(in crate::class_manager) jit_self_hint_by_id: Arc<std::sync::atomic::AtomicU8>,
}

/// Slots of [`HiddenClassBits::self_names`]: 32 KiB per VM.
pub(in crate::class_manager) const HIDDEN_SELF_NAME_SLOTS: usize = 4096;

/// The 32-bit FNV-1a hash of a class-file name, for
/// [`HiddenClassBits::self_names`]. Equal names hash equal, so a differing
/// hash proves the names differ; an equal one proves nothing.
#[inline]
pub(in crate::class_manager) fn hidden_self_name_hash(name: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in name.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// One [`HiddenClassBits::self_names`] word: bit 63 set (never `0`, the empty
/// slot), the id in bits 32..63, the name hash below. `None` for an id at or
/// above `1 << 31`, which the word cannot carry (those ids are never
/// prefiltered).
#[inline]
pub(in crate::class_manager) fn hidden_self_name_slot(id: ClassId, name: &str) -> Option<u64> {
    let raw = id.as_u32();
    (raw < (1 << 31))
        .then(|| (1u64 << 63) | (u64::from(raw) << 32) | u64::from(hidden_self_name_hash(name)))
}

impl HiddenClassBits {
    pub(in crate::class_manager) fn new() -> Self {
        // JIT round 22 wave 1, lane review (item 8 of
        // `r21-review-merge-w25-findings-FIXED-20261020.md`): both tables allocated
        // zeroed like the deleted `JavaUtilClassBits` (zs s6/rs, `CRATONVM_LAZY_ZERO_TABLES`),
        // so a VM that defines no hidden class never touches their pages.
        Self {
            words: cratonvm_types::zeroed_table::zeroed_arc_slice(CLASS_ID_BITMAP_WORDS, || {
                AtomicU64::new(0)
            }),
            past_capacity: Arc::new(AtomicBool::new(false)),
            self_cp_by_id: Arc::new(std::sync::atomic::AtomicU8::new(0)),
            self_names: cratonvm_types::zeroed_table::zeroed_arc_slice(HIDDEN_SELF_NAME_SLOTS, || {
                AtomicU64::new(0)
            }),
            self_name_prefilter: Arc::new(std::sync::atomic::AtomicU8::new(0)),
            jit_self_resolve_by_id: Arc::new(std::sync::atomic::AtomicU8::new(0)),
            jit_self_hint_by_id: Arc::new(std::sync::atomic::AtomicU8::new(0)),
        }
    }

    /// `CRATONVM_JIT_HIDDEN_SELF_HINT_BY_ID` (JIT round 22, lane inline;
    /// perf, OPT-IN): the VM's by-name callee door probes a hint naming a
    /// hidden class under that class's id when the asked name is its own
    /// class-file name. Read once per VM (wave 3, item 3 of
    /// `r22-review-w3-intrinsics-on-inline-FIXED-20261020.md`).
    pub fn jit_self_hint_by_id(&self) -> bool {
        match self.jit_self_hint_by_id.load(Ordering::Relaxed) {
            1 => true,
            2 => false,
            _ => {
                let on =
                    // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
                    true;
                self.set_jit_self_hint_by_id(on);
                on
            }
        }
    }

    /// Pin [`Self::jit_self_hint_by_id`]'s answer for this VM (tests, embedders).
    pub fn set_jit_self_hint_by_id(&self, on: bool) {
        self.jit_self_hint_by_id.store(if on { 1 } else { 2 }, Ordering::Relaxed);
    }

    /// `CRATONVM_JIT_HIDDEN_SELF_RESOLVE_BY_ID` (JIT round 22, lane inline;
    /// perf, OPT-IN): the VM's `class_resolved_without_loading` answers a
    /// hidden class's reference to its own class-file name with the hidden
    /// class's id instead of deferring it. Read once per VM (wave 3, item 3
    /// of `r22-review-w3-intrinsics-on-inline-FIXED-20261020.md`: the compiled
    /// routes ask it per call).
    pub fn jit_self_resolve_by_id(&self) -> bool {
        match self.jit_self_resolve_by_id.load(Ordering::Relaxed) {
            1 => true,
            2 => false,
            _ => {
                // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
                let on = true;
                self.set_jit_self_resolve_by_id(on);
                on
            }
        }
    }

    /// Pin [`Self::jit_self_resolve_by_id`]'s answer for this VM (tests, embedders).
    pub fn set_jit_self_resolve_by_id(&self, on: bool) {
        self.jit_self_resolve_by_id.store(if on { 1 } else { 2 }, Ordering::Relaxed);
    }

    /// `CRATONVM_JIT_HIDDEN_SELF_NAME_PREFILTER` (JIT round 21 wave 22, lane
    /// hidden-resid; perf, OPT-IN, default off): the compiled routes' hidden
    /// self-reference guard (`jit::helpers::jit_hidden_self_owner`) asks
    /// [`Self::self_name_rules_out`] before it takes the class-manager read,
    /// so a hidden caller's site naming ANOTHER class (a lambda proxy calling
    /// its target) costs two loads and a hash, no lock. Read once per VM.
    pub fn self_name_prefilter(&self) -> bool {
        match self.self_name_prefilter.load(Ordering::Relaxed) {
            1 => true,
            2 => false,
            _ => {
                let on = false;
                self.set_self_name_prefilter(on);
                on
            }
        }
    }

    /// Pin [`Self::self_name_prefilter`]'s answer for this VM (tests, embedders).
    pub fn set_self_name_prefilter(&self, on: bool) {
        self.self_name_prefilter.store(if on { 1 } else { 2 }, Ordering::Relaxed);
    }

    /// Record (or, with `None`, forget) hidden class `id`'s recorded
    /// class-file name in the prefilter table. Called under the class-manager
    /// write lock wherever the recorded class-file name is set, before the
    /// class is published. `Release`, paired with
    /// [`Self::self_name_rules_out`]'s `Acquire`. A slot another id holds is
    /// overwritten; the evicted id then just misses the prefilter.
    pub(in crate::class_manager) fn record_self_name(&self, id: ClassId, recorded: Option<&str>) {
        let slot = &self.self_names[id.as_u32() as usize % HIDDEN_SELF_NAME_SLOTS];
        match recorded.and_then(|name| hidden_self_name_slot(id, name)) {
            Some(word) => slot.store(word, Ordering::Release),
            None => {
                // Clear only this id's own word: another id's stays useful.
                let current = slot.load(Ordering::Acquire);
                if current != 0 && (current >> 32) & 0x7fff_ffff == u64::from(id.as_u32()) {
                    let _ = slot.compare_exchange(current, 0, Ordering::AcqRel, Ordering::Relaxed);
                }
            }
        }
    }

    /// `true` when `referenced` is PROVABLY not hidden class `id`'s recorded
    /// class-file name: `id`'s slot holds `id` with a different name hash.
    /// `false` (ask under the lock) for every other case -- no word for `id`
    /// (no recorded name, an evicted slot, an id the word cannot carry) or an
    /// equal hash. One load and a hash, no lock. Sound because the rule
    /// ([`hidden_self_name_rule`]) answers a recorded name by equality only.
    #[inline]
    pub fn self_name_rules_out(&self, id: ClassId, referenced: &str) -> bool {
        let raw = id.as_u32();
        if raw >= (1 << 31) {
            return false;
        }
        let word = self.self_names[raw as usize % HIDDEN_SELF_NAME_SLOTS].load(Ordering::Acquire);
        word != 0
            && (word >> 32) & 0x7fff_ffff == u64::from(raw)
            && word as u32 != hidden_self_name_hash(referenced)
    }

    /// `CRATONVM_HIDDEN_SELF_CP_BY_ID` (JIT round 21 wave 20, lane
    /// hidden-self-resolve; correctness, default ON, `=0` restores the
    /// by-name answers): a hidden class's constant-pool reference to its own
    /// class-FILE name resolves to the hidden class itself
    /// ([`ClassManager::hidden_self_cp_answer`]), as JVMS 5.4.3.1 and
    /// `Lookup::defineHiddenClass` specify, never to an ordinary class of the
    /// same name loaded later. Read once per VM (this bitmap's), on the first
    /// hidden self-reference asked; ordinary classes never reach it.
    ///
    /// Its reach (wave 21, item 5 of `r21-review-w21-findings-FIXED-20261020.md`):
    /// `=0` restores the by-name answer at every caller of
    /// [`ClassManager::hidden_self_cp_answer`] / [`ClassManager::find_cp_class_for_class`]
    /// -- the member-reference owner (`owner_in_referencing_namespace`,
    /// `resolve_method_metadata`), the field peek and loader-aware field
    /// resolution, the `invokespecial` owner and dispatch override, the
    /// private / `Object` invokevirtual site predicates, the 5.4.6 selection
    /// input, the invoke-at-pc readers (`gc_and_alloc`, `interpreter`,
    /// `jvmti_events`) and the JIT site doors in `jit::helpers`. It does NOT
    /// reach the older self doors, which keep answering by the class's id:
    /// the `CONSTANT_Class` door (`constants::hidden_self_reference`), the
    /// self-call doors (`is_self_class_reference[_in]`: `dispatch_static`,
    /// the private-invokevirtual pin, the constructor-receiver match), the
    /// unwinder's constructor-target test, or the compiled static routes of
    /// `CRATONVM_JIT_HIDDEN_SELF_BY_ID` (`jit_hidden_self_owner`,
    /// `class_resolved_without_loading`).
    pub fn self_cp_by_id(&self) -> bool {
        match self.self_cp_by_id.load(Ordering::Relaxed) {
            1 => true,
            2 => false,
            _ => {
                let on =
                    true;
                self.set_self_cp_by_id(on);
                on
            }
        }
    }

    /// Pin [`Self::self_cp_by_id`]'s answer for this VM (tests, embedders).
    pub fn set_self_cp_by_id(&self, on: bool) {
        self.self_cp_by_id.store(if on { 1 } else { 2 }, Ordering::Relaxed);
    }

    /// Record that `id` is hidden; an id out of range raises the
    /// past-capacity latch instead. `Release`, paired with
    /// [`Self::contains`]'s `Acquire`.
    pub(in crate::class_manager) fn note(&self, id: ClassId) {
        let i = id.as_u32() as usize;
        match self.words.get(i >> 6) {
            Some(word) => {
                word.fetch_or(1u64 << (i & 63), Ordering::Release);
            }
            None => self.past_capacity.store(true, Ordering::Release),
        }
    }

    /// Whether class `id` of this manager is hidden: `Some(bool)` for ids the
    /// bitmap covers, and for every id past it while no hidden class past it
    /// exists (`Some(false)`); `None` (ask under the lock) otherwise. One or
    /// two loads, no lock.
    #[inline]
    pub fn contains(&self, id: ClassId) -> Option<bool> {
        let i = id.as_u32() as usize;
        match self.words.get(i >> 6) {
            Some(word) => Some(word.load(Ordering::Acquire) & (1u64 << (i & 63)) != 0),
            None if !self.past_capacity.load(Ordering::Acquire) => Some(false),
            None => None,
        }
    }
}

/// Does `stored` -- a hidden class's stored name -- carry `referenced` as its
/// class-FILE name, i.e. is it exactly `<referenced>/0x<hex digits>`? The
/// mint sites write `<this_class>/0x<counter>` (`Unsafe.defineAnonymousClass`
/// minted from the HOST's name until wave 21); `A/0x1$Inner` is another
/// class, so anything after the hex refuses (JIT round 21 wave 20, lane
/// hidden-self-resolve). The fallback of [`hidden_self_name_rule`] when no
/// class-file name was recorded; the vm's `constants::hidden_stored_name_is_self`
/// and `exception_dispatch`'s constructor-target test call this, so there is
/// one copy (wave 21, item 4 of `r21-review-w21-findings-FIXED-20261020.md`).
pub fn hidden_stored_name_is_class_file_name(stored: &str, referenced: &str) -> bool {
    match stored
        .strip_prefix(referenced)
        .and_then(|rest| rest.strip_prefix("/0x"))
    {
        Some(hex) => !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()),
        None => false,
    }
}

/// THE hidden self-name rule (JIT round 21 wave 21, items 3-4 of
/// `r21-review-w21-findings-FIXED-20261020.md`): is `referenced`, a name from the
/// constant pool of the class stored as `stored`, that HIDDEN class's own
/// class-file name? `recorded` is the class-file name recorded at define time
/// ([`ClassManager::hidden_class_file_name`]), authoritative when present, so
/// the answer does not depend on how a mint site spelled the stored name;
/// without one (a class marked hidden after its define, a pure caller with no
/// class manager) the stored name's `/0x<hex>` suffix is read
/// ([`hidden_stored_name_is_class_file_name`]). Never `true` for an ordinary
/// class: its own name is found by name.
///
/// # Constraint for later changes (r21 flip review #12)
///
/// If the self-name rule ever accepts a SECOND spelling (for example the
/// stored `/0x..` name next to a recorded one), the prefilter
/// (`CRATONVM_JIT_HIDDEN_SELF_NAME_PREFILTER`) must hash that spelling too.
/// Otherwise it becomes a false-negative self test, which is exactly the
/// `R21Fz5HiddenOsr1` `IllegalAccessError`.
pub fn hidden_self_name_rule(
    stored: &str,
    hidden: bool,
    recorded: Option<&str>,
    referenced: &str,
) -> bool {
    hidden
        && match recorded {
            Some(file_name) => file_name == referenced,
            None => hidden_stored_name_is_class_file_name(stored, referenced),
        }
}

