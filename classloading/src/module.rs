// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Java Platform Module System (JPMS) support — Phase N.
//!
//! Implements the module graph, readability, exports/opens access control,
//! and the unnamed-module compatibility layer needed to load Java 9+ code.
//!
//! # Key concepts
//!
//! * **Named module** — declared by a `module-info.class`; has an explicit name
//!   and controls which packages it exports and to whom.
//! * **Unnamed module** — every class loaded from the classpath (no module
//!   declaration) belongs to the unnamed module.  The unnamed module reads every
//!   named module and the named modules export everything to it (classpath-mode
//!   compatibility).
//! * **Readability** — module A *reads* module B if A has a `requires B` edge
//!   (directly or transitively).
//! * **Exports** — module B *exports* package `p` to module A if `p` appears in
//!   B's `exports` table with `A` in the qualifier list, or the list is empty
//!   (unqualified export).

#![allow(dead_code)]

use rustc_hash::{FxHashMap, FxHashSet};

// ---------------------------------------------------------------------------
// Module flag constants (from JVMS 4.7.25)
// ---------------------------------------------------------------------------

/// Module declaration flag: all packages are open (open module).
pub const ACC_MODULE_OPEN: u16 = 0x0020;
/// `requires` flag: transitive read edge.
pub const ACC_REQUIRES_TRANSITIVE: u16 = 0x0020;
/// `requires` flag: static (compile-time only) dependency.
pub const ACC_REQUIRES_STATIC: u16 = 0x0040;
/// Synthetic element (compiler-generated).
pub const ACC_SYNTHETIC: u16 = 0x1000;
/// Mandated element (implicitly declared by spec).
pub const ACC_MANDATED: u16 = 0x8000;

// ---------------------------------------------------------------------------
// Module descriptor components
// ---------------------------------------------------------------------------

/// A single `requires` directive in a module declaration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModuleRequiresEntry {
    /// Binary module name, e.g. `"java.base"`.
    pub module_name: String,
    /// True if this is `requires transitive` — callers of this module also read
    /// the required module.
    pub is_transitive: bool,
    /// True if this is `requires static` — dependency is compile-time only.
    pub is_static: bool,
    /// True if `ACC_SYNTHETIC` (0x1000) is set on the directive — the class-file
    /// spelling of `Requires.Modifier.SYNTHETIC`.
    pub is_synthetic: bool,
    /// True if `ACC_MANDATED` (0x8000) is set — `Requires.Modifier.MANDATED`.
    ///
    /// This is the one modifier bit that really occurs in the wild. Scanning
    /// JDK 25's own module declarations (`javap -v --module <m> module-info`,
    /// grepping the `Module:` section for `ACC_MANDATED`/`ACC_SYNTHETIC` across
    /// java.base, java.desktop, java.logging, java.sql and jdk.jfr) finds it on
    /// **`requires java.base`, in every module, and nowhere else** — zero hits
    /// on any `exports` or `opens` directive. So dropping it here is the loss
    /// that visibly disagrees with HotSpot: there `java.base`'s
    /// `Requires.modifiers()` is `[MANDATED]`, and it was `[]` here.
    pub is_mandated: bool,
    /// The `requires_version` string recorded by the producer, if any
    /// (`requires_version_index != 0`).
    ///
    /// Raw, unparsed text — which is precisely what
    /// `Requires.rawCompiledVersion()` answers; `Requires.compiledVersion()`
    /// is the same text through `ModuleDescriptor.Version.parse`. The index was
    /// previously discarded at parse time, leaving both accessors with no data
    /// source anywhere in the VM.
    pub compiled_version: Option<String>,
}

/// A single `exports` directive.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModuleExportsEntry {
    /// Exported package in slash format, e.g. `"java/lang"`.
    pub package_name: String,
    /// Qualified targets.  Empty ⇒ unqualified (exported to all modules).
    pub to_modules: Vec<String>,
    /// `ACC_SYNTHETIC` on the directive — `Exports.Modifier.SYNTHETIC`.
    ///
    /// No JDK 25 module declaration sets this or [`is_mandated`](Self::is_mandated)
    /// on an `exports` (see `ModuleRequiresEntry::is_mandated` for the scan), so
    /// an empty `Exports.modifiers()` is the right answer for every `javac`- or
    /// `jlink`-emitted directive. It is parsed anyway so that the empty set is a
    /// *measured* zero rather than a hardcoded one: a producer that does set the
    /// bit is now representable instead of silently flattened.
    pub is_synthetic: bool,
    /// `ACC_MANDATED` on the directive — `Exports.Modifier.MANDATED`.
    pub is_mandated: bool,
}

impl ModuleExportsEntry {
    /// Does this entry export the package to `requester`?
    pub fn accessible_to(&self, requester: &str) -> bool {
        self.to_modules.is_empty() || self.to_modules.iter().any(|m| m == requester)
    }
}

/// A single `opens` directive.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModuleOpensEntry {
    /// Opened package in slash format.
    pub package_name: String,
    /// Qualified targets.  Empty ⇒ unqualified open.
    pub to_modules: Vec<String>,
    /// `ACC_SYNTHETIC` on the directive — `Opens.Modifier.SYNTHETIC`. Same
    /// measured-zero rationale as [`ModuleExportsEntry::is_synthetic`].
    pub is_synthetic: bool,
    /// `ACC_MANDATED` on the directive — `Opens.Modifier.MANDATED`.
    pub is_mandated: bool,
}

impl ModuleOpensEntry {
    /// Is the package open to `requester`?
    pub fn open_to(&self, requester: &str) -> bool {
        self.to_modules.is_empty() || self.to_modules.iter().any(|m| m == requester)
    }
}

/// A single `provides` directive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleProvidesEntry {
    /// Service interface (binary class name).
    pub service: String,
    /// Implementing classes (binary class names).
    pub with: Vec<String>,
}

// ---------------------------------------------------------------------------
// ModuleDescriptor
// ---------------------------------------------------------------------------

/// Full parsed representation of a `module-info.class` Module attribute.
#[derive(Debug, Clone)]
pub struct ModuleDescriptor {
    /// Module name, e.g. `"java.base"`.
    pub name: String,
    /// Optional version string.
    pub version: Option<String>,
    /// True if declared as `open module`.
    pub is_open: bool,
    /// Direct `requires` edges.
    pub requires: Vec<ModuleRequiresEntry>,
    /// Exported packages.
    pub exports: Vec<ModuleExportsEntry>,
    /// Opened packages (for deep reflection).
    pub opens: Vec<ModuleOpensEntry>,
    /// Service types used by this module.
    pub uses: Vec<String>,
    /// Service implementations provided.
    pub provides: Vec<ModuleProvidesEntry>,
    /// True if this descriptor was registered for a jar on the **class path**
    /// rather than a real module path. CratonVM has no module path, so any
    /// `module-info.class` found in an application-classpath jar describes a jar
    /// that the real JDK would place in the *unnamed* module. We still keep the
    /// descriptor (for `provides`/`uses` service discovery and module labelling),
    /// but such modules get JDK *automatic-module* access semantics: they read
    /// every other module and export/open all their packages. Without this, e.g.
    /// `org.jboss.logging` (which does not `requires org.apache.logging.log4j`)
    /// would fail readability and break logging init for the whole app.
    pub automatic: bool,
    /// The `ModuleMainClass` attribute's class, in INTERNAL (slash) form.
    ///
    /// A sibling attribute of `Module`, not part of it, so it is resolved in
    /// `parse_module_info` (which sees the whole attribute list) rather than
    /// in `descriptor_from_module_attribute` (which only sees the `Module`
    /// attribute). Backs `ModuleDescriptor.mainClass()`.
    pub main_class: Option<String>,
}

impl ModuleDescriptor {
    /// Returns true if this module exports `pkg` to module `requester`.
    ///
    /// An open module exports every package to everyone.
    pub fn exports_package_to(&self, pkg: &str, requester: &str) -> bool {
        // Automatic (classpath) modules export every package unqualified.
        if self.automatic || self.is_open {
            return true;
        }
        self.exports
            .iter()
            .any(|e| e.package_name == pkg && e.accessible_to(requester))
    }

    /// Returns true if this module opens `pkg` to module `requester` (for
    /// reflective access).
    ///
    /// An open module opens every package to everyone.
    pub fn opens_package_to(&self, pkg: &str, requester: &str) -> bool {
        // Automatic (classpath) modules open every package for deep reflection.
        if self.automatic || self.is_open {
            return true;
        }
        self.opens
            .iter()
            .any(|o| o.package_name == pkg && o.open_to(requester))
    }
}

// ---------------------------------------------------------------------------
// ModuleRegistry
// ---------------------------------------------------------------------------

/// The unnamed module sentinel.  All classpath classes that lack a
/// `module-info.class` belong to this logical module.
pub const UNNAMED_MODULE: &str = "";

/// The `java.base` module — every named module implicitly reads it.
pub const JAVA_BASE: &str = "java.base";

/// The literal target token `--add-exports`/`--add-opens` accept to mean "every
/// *unnamed* module".
///
/// It is deliberately NOT the same thing as an unqualified edge, and the
/// distinction is observable: `--add-opens java.base/java.net=ALL-UNNAMED`
/// leaves `Module.isOpen("java.net")` answering **false** on HotSpot, because
/// the package is open to the unnamed module rather than to everyone. The
/// launcher used to fold `ALL-UNNAMED` into the empty string on the way in, and
/// the empty string is [`add_opens`](ModuleRegistry::add_opens)' unqualified
/// marker, so the flag over-granted: it reached named modules too, and
/// `isOpen(pkg)` answered true.
///
/// That is the same conflation, one layer up, that
/// `UNRESOLVED_TARGET_MODULE` (native-builtins) already fixed for the
/// `Module.addOpens(String, Module)` path after it broke
/// `AotIntegrationTests#endToEndTestsForBeanOverrides`. [`add_opens`] and
/// [`add_exports`](ModuleRegistry::add_exports) resolve this token to
/// [`UNNAMED_MODULE`] as a *qualified* target, which grants exactly the
/// classpath's unnamed module and nobody else.
pub const ALL_UNNAMED_TARGET: &str = "ALL-UNNAMED";

/// The dynamic-edge target (both modes since interpreter round i1 wave 47)
/// naming ONE loader's unnamed module
/// (`Module.implAddOpens(pn, loader.getUnnamedModule())`, reached through
/// `Instrumentation.redefineModule` / `Modules.addOpens`), by the loader's
/// namespace id ([`cratonvm_types::ClassLoaderId::to_native_id`]). The name
/// map has no name for an unnamed module; before interpreter round i1 wave
/// 46 (lane L5) such an edge was recorded as granting nobody
/// (`UNRESOLVED_TARGET_MODULE`), so `Module.isOpen(pn, thatModule)` and the
/// deep-reflection gate refused what HotSpot grants (JaCoCo's
/// `InjectedClassRuntime` then failed its `privateLookupIn`). No module name
/// can collide with it (`:` is not legal in one).
pub fn unnamed_module_of_loader_target(loader_ns: u32) -> String {
    format!("cratonvm.unnamed-module-of-loader:{loader_ns}")
}

/// Resolve a raw dynamic-edge target string into the stored
/// [`DynamicExport::to_module`].
///
/// * `""`             → `None`, genuinely unqualified (`opens p;`).
/// * `"ALL-UNNAMED"`  → `Some("")`, qualified to the unnamed module only.
/// * anything else    → `Some(name)`, qualified to that named module.
fn resolve_edge_target(target: &str) -> Option<String> {
    if target.is_empty() {
        None
    } else if target == ALL_UNNAMED_TARGET {
        Some(UNNAMED_MODULE.to_string())
    } else {
        Some(target.to_string())
    }
}

/// A dynamic export or open edge added at runtime via `Module.addExports()`,
/// `Module.addOpens()`, or CLI `--add-exports`/`--add-opens`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DynamicExport {
    /// Package name in slash format.
    pub package: String,
    /// Target module.  `None` = unqualified (open/export to all).
    pub to_module: Option<String>,
    /// Recorded from the command line (`--add-exports` / `--add-opens`,
    /// [`ModuleRegistry::add_command_line_exports`]). Such a target is
    /// resolved in the BOOT layer (`ModuleBootstrap.addExtraExportsOrOpens`
    /// -> `bootLayer.findModule`), so by identity it never names a module of
    /// a non-boot layer, whatever that module is called (interpreter round i6
    /// wave 124, lane L5).
    pub command_line: bool,
    /// The target's defining loader namespace when the run-time native
    /// recorded the edge to a module of a non-boot `ModuleLayer`
    /// ([`ModuleRegistry::add_exports_to_layer_module`]: `addExports0(from,
    /// pn, other)` with `other` a user-layer module); `None` for every other
    /// edge, whose named target is then a name-map (boot-layer) module.
    /// Recorded in every mode; read only by [`layer_edge_admits`] and the
    /// name-keyed questions under `CRATONVM_MODULE_ACCESS_EXPORT_ONE_QUESTION`
    /// (interpreter round i6 wave 125, lane L5).
    pub layer_target: Option<u32>,
}

/// Does the dynamic edge `de` reach `pkg` for the non-boot layer's module
/// named `layer_module` ([`ModuleRegistry::is_package_exported_to_layer_module`]
/// and its `opens` twin)? Only an edge qualified to that NAME. `by_identity`
/// (`CRATONVM_MODULE_ACCESS_EXPORT_ONE_QUESTION`) also asks the target's
/// identity:
/// * a command-line edge never reaches it (its target is the boot layer's
///   module, wave 124);
/// * with the asked module's loader namespace `layer_ns` known (wave 125), a
///   run-time edge reaches it only when it was recorded to a layer module of
///   that same loader ([`DynamicExport::layer_target`]): an edge to the boot
///   layer's `m`, or to another layer's `m`, is another module's edge, as on
///   HotSpot (`addExports0` takes the target `Module` itself);
/// * `layer_ns` unknown: by name, as before (permissive only).
fn layer_edge_admits(
    de: &DynamicExport,
    pkg: &str,
    layer_module: &str,
    layer_ns: Option<u32>,
    by_identity: bool,
) -> bool {
    if de.package != pkg || de.to_module.as_deref() != Some(layer_module) {
        return false;
    }
    if !by_identity {
        return true;
    }
    if de.command_line {
        return false;
    }
    match layer_ns {
        Some(ns) => de.layer_target == Some(ns),
        None => true,
    }
}

/// A module as the JVMS §5.4.4 export clause sees it: by IDENTITY, not by
/// name alone. A module of a non-boot `ModuleLayer` is named by its defining
/// loader's namespace id together with its name (a loader cannot hold two
/// modules of one name, while two layers can each hold a module `m`); every
/// other named module by its registry name; an unnamed module by its loader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleIdent<'a> {
    /// A module of this registry's name map (boot layer, `--module-path`).
    Named(&'a str),
    /// A module of a non-boot layer ([`ModuleRegistry::define_layer_module`]).
    Layer { loader_ns: u32, name: &'a str },
    /// The unnamed module of the loader with this namespace id
    /// (`ClassLoaderId::to_native_id`).
    Unnamed { loader_ns: u32 },
}

impl ModuleIdent<'_> {
    /// HotSpot's spelling of the module in an access message
    /// (`module m` / `unnamed module`; HotSpot appends an identity hash to
    /// the unnamed form, which this VM cannot reproduce).
    pub fn describe(&self) -> String {
        match self {
            ModuleIdent::Named(name) | ModuleIdent::Layer { name, .. } => format!("module {name}"),
            ModuleIdent::Unnamed { .. } => "unnamed module".to_string(),
        }
    }
}

/// The target of an export a layer module's `addExports0` /
/// `addExportsToAll0` / `addExportsToAllUnnamed0` recorded (HotSpot's
/// `PackageEntry` export list), by identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayerExportTarget {
    /// `addExportsToAll0`: every module.
    Everyone,
    /// `addExportsToAllUnnamed0`: every unnamed module.
    AllUnnamed,
    /// `addExports0` to a module of the name map.
    Named(String),
    /// `addExports0` to a module of a non-boot layer.
    Layer { loader_ns: u32, name: String },
    /// `addExports0` to one loader's unnamed module.
    Unnamed { loader_ns: u32 },
}

impl LayerExportTarget {
    fn admits(&self, accessor: ModuleIdent<'_>) -> bool {
        match (self, accessor) {
            (LayerExportTarget::Everyone, _) => true,
            (LayerExportTarget::AllUnnamed, ModuleIdent::Unnamed { .. }) => true,
            (LayerExportTarget::Named(target), ModuleIdent::Named(name)) => target == name,
            (
                LayerExportTarget::Layer {
                    loader_ns,
                    name: target,
                },
                ModuleIdent::Layer {
                    loader_ns: ns,
                    name,
                },
            ) => *loader_ns == ns && target == name,
            (LayerExportTarget::Unnamed { loader_ns }, ModuleIdent::Unnamed { loader_ns: ns }) => {
                *loader_ns == ns
            }
            _ => false,
        }
    }
}

/// A module of a non-boot `ModuleLayer` defined to a user-defined loader, as
/// `Module.defineModule0` and the `addExports*0` natives describe it to the VM
/// (`--jdk-only`; interpreter round i1 wave 44, lane L5).
#[derive(Debug, Clone, Default)]
struct LayerModule {
    /// `defineModule0`'s `isOpen` (an open or automatic module): every
    /// package exported and open to every module.
    is_open: bool,
    /// package (internal form) → the targets it is exported to.
    exports: FxHashMap<String, Vec<LayerExportTarget>>,
    /// The descriptor's version (`defineModule0`'s `version`), for the
    /// `moduleVersion` of a stack trace element.
    version: Option<String>,
    /// The modules it reads, as `addReads0` told the VM (`Module.defineModules`
    /// calls it for every module the configuration resolved it to read,
    /// `java.base` always among them; `implAddReads` for a run-time edge,
    /// `AllUnnamed` for `ALL_UNNAMED_MODULE`). `Everyone` is never recorded.
    reads: Vec<LayerExportTarget>,
}

/// Tracks all registered modules, the package-to-module mapping, and the
/// pre-computed readability graph.
pub struct ModuleRegistry {
    /// module name → descriptor
    modules: FxHashMap<String, ModuleDescriptor>,

    /// package (slash format) → module name that owns it.
    package_to_module: FxHashMap<String, std::sync::Arc<str>>,

    /// Readability graph after transitive closure: module → set of modules it
    /// can read.  Only populated after `build_readability_graph()`.
    readable: FxHashMap<String, FxHashSet<String>>,

    /// True once `build_readability_graph` has been called.
    graph_built: bool,

    /// Dynamic read edges added at runtime (`Module.addReads`, `--add-reads`).
    extra_reads: FxHashMap<String, FxHashSet<String>>,

    /// Dynamic exports added at runtime (`Module.addExports`, `--add-exports`).
    extra_exports: FxHashMap<String, Vec<DynamicExport>>,

    /// Dynamic opens added at runtime (`Module.addOpens`, `--add-opens`).
    extra_opens: FxHashMap<String, Vec<DynamicExport>>,

    /// `--jdk-only`: the modules of non-boot layers defined to user-defined
    /// loaders, keyed by `(loader namespace id, module name)` -- their
    /// identity. Kept apart from `modules` (keyed by name alone), which two
    /// layers' same-named modules would collide in. Empty in `--compatible`,
    /// where nothing records a layer module.
    layer_modules: FxHashMap<(u32, String), LayerModule>,

    /// `--jdk-only`: loader namespace id → package (internal form) → the name
    /// of the layer module that holds the package for that loader
    /// (HotSpot's per-loader package-to-module map, filled by
    /// `defineModule0`). A class a user-defined loader defines in such a
    /// package is a member of that module.
    layer_packages: FxHashMap<u32, FxHashMap<String, String>>,

    /// The module names a module's `ModuleHashes` attribute records (the
    /// modules `jlink`/`jmod` hashed into `java.base`), by module name. Only
    /// modules whose `module-info.class` carries the attribute have a row.
    /// Backs `StackTraceElement.HashedModules` through the boot
    /// `ModuleReferenceImpl.recordedHashes()`.
    recorded_hash_names: FxHashMap<String, Vec<String>>,
    /// The JDK image modules the JDK's own table
    /// (`jdk.internal.module.ModuleLoaderMap$Modules.platformModules`) defines
    /// to the PLATFORM loader (`java.sql`, `java.net.http`, ...), handed in
    /// once at boot from the VM's memo of that table
    /// ([`Self::set_platform_loader_modules`]). This VM defines every image
    /// class in its boot namespace; a HotSpot-worded message about such a
    /// class names the loader `'platform'` (interpreter round i1 wave 48,
    /// lane L5). `None` until handed in: messages keep `'bootstrap'`.
    platform_loader_modules: Option<FxHashSet<String>>,
    /// The JDK image modules the same table defines to the BOOTSTRAP loader
    /// (`ModuleLoaderMap$Modules.bootModules`), handed in with
    /// [`Self::platform_loader_modules`] ([`Self::set_boot_loader_modules`]).
    /// With both, an image module in neither set is an application-loader
    /// module (`jdk.compiler`, ...): [`Self::image_module_builtin_loader`].
    /// zt df4 §9c (the initiating rows of boot-tagged image classes).
    boot_loader_modules: Option<FxHashSet<String>>,
    /// The system (image) modules the boot configuration does NOT resolve
    /// ([`Self::settle_boot_configuration`]): not reached from JEP 261's
    /// default roots, `--add-modules` or a module-path module by `requires`
    /// or service binding. Empty until settled (everything in, as before).
    outside_boot_configuration: FxHashSet<String>,
    /// Every registered module the boot configuration resolves, exactly
    /// ([`Self::settle_boot_configuration`]'s `resolved`, not narrowed the way
    /// `outside_boot_configuration` is): HotSpot's `ModuleLayer.boot()` module
    /// set. Empty until settled. Interpreter round i4 wave 99, lane L3
    /// (`BuiltinClassLoader.findMiscResource("module-info.class")`).
    boot_configuration: FxHashSet<String>,
    /// `--patch-module`: per module, the patch paths (wave 87, lane L5).
    patches: Vec<(String, Vec<String>)>,
    /// The modules resolved from the `--module-path` (`vm_init`), by name:
    /// the boot-layer modules whose HotSpot `ModuleEntry` has a `file:`
    /// location, so a message shows their version
    /// ([`Self::shown_module_version`]). Interpreter round i4 wave 99, lane L5.
    module_path_modules: FxHashSet<String>,
    /// The module path's AUTOMATIC modules (a JAR with no `module-info`,
    /// [`ModulePathModule::automatic`]), which read every module.
    automatic_modules: FxHashSet<String>,
}

impl Default for ModuleRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ModuleRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            modules: FxHashMap::with_capacity_and_hasher(128, Default::default()),
            package_to_module: FxHashMap::with_capacity_and_hasher(1024, Default::default()),
            readable: FxHashMap::with_capacity_and_hasher(16, Default::default()),
            graph_built: false,
            extra_reads: FxHashMap::default(),
            extra_exports: FxHashMap::default(),
            extra_opens: FxHashMap::default(),
            layer_modules: FxHashMap::default(),
            layer_packages: FxHashMap::default(),
            recorded_hash_names: FxHashMap::default(),
            platform_loader_modules: None,
            boot_loader_modules: None,
            outside_boot_configuration: FxHashSet::default(),
            boot_configuration: FxHashSet::default(),
            patches: Vec::new(),
            module_path_modules: FxHashSet::default(),
            automatic_modules: FxHashSet::default(),
        }
    }

    /// Record that the module `name` was resolved from the `--module-path`
    /// (`automatic`: a JPMS automatic module, which reads every module).
    /// Interpreter round i4 wave 99, lane L5.
    pub fn note_module_path_module(&mut self, name: &str, automatic: bool) {
        self.module_path_modules.insert(name.to_string());
        if automatic {
            self.automatic_modules.insert(name.to_string());
        } else {
            self.automatic_modules.remove(name);
        }
    }

    /// Is `name` an automatic module of the module path?
    pub fn is_automatic_module(&self, name: &str) -> bool {
        self.automatic_modules.contains(name)
    }

    /// The version HotSpot shows after a boot-layer module's name in a
    /// message (`Klass::class_in_module_of_loader`'s `module m@v`), under
    /// `ModuleEntry::should_show_version()`: the module has a version AND a
    /// location, and is not a JDK module of the run-time image (location
    /// `jrt:/java.*` of the boot or platform loader, or `jrt:/jdk.*` of a
    /// built-in loader at `java.base`'s version). This VM records a location
    /// for the module path's modules only (a `file:` URI there: the version
    /// shows); an image module's is `jrt:/<name>`, and every module of a
    /// stock JDK image is `java.*` or `jdk.*` at `java.base`'s version, so it
    /// shows none. (A `jlink`ed image's own non-JDK modules would show theirs
    /// on HotSpot; this VM does not tell them apart.) Interpreter round i4
    /// wave 99, lane L5.
    pub fn shown_module_version(&self, name: &str) -> Option<&str> {
        if !self.module_path_modules.contains(name) {
            return None;
        }
        self.modules
            .get(name)?
            .version
            .as_deref()
            .filter(|v| !v.is_empty())
    }

    /// Record the `--patch-module` directives (interpreter round i4 wave 87,
    /// lane L5).
    pub fn set_patches(&mut self, patches: Vec<(String, Vec<String>)>) {
        self.patches = patches;
    }

    /// Every `--patch-module` directive, `(module, paths)`, in command-line
    /// order (empty without the option).
    pub fn patches(&self) -> &[(String, Vec<String>)] {
        &self.patches
    }

    /// The `--patch-module` paths of `module`, empty when it is not patched.
    pub fn patch_paths(&self, module: &str) -> &[String] {
        self.patches
            .iter()
            .find(|(m, _)| m == module)
            .map(|(_, paths)| paths.as_slice())
            .unwrap_or(&[])
    }

    /// Settle which system modules the boot configuration resolves, as the
    /// JDK's `ModuleBootstrap.boot` does for a class-path launch
    /// (`DefaultRoots.compute` + `resolveAndBind`; JEP 261):
    ///
    /// * roots: every system module that exports at least one package
    ///   unqualified and is not resolved-by-default-forbidden (the
    ///   `jdk.incubator.*` modules, whose `ModuleResolution` attribute says
    ///   so -- the registry does not keep that attribute, so the name is the
    ///   test), every module-path module the registry holds explicitly, and
    ///   `add_modules` (`ALL-SYSTEM` = every system module, `ALL-DEFAULT` = the
    ///   default roots);
    /// * the closure over non-`static` `requires` and service binding (a
    ///   module that `provides` a service a resolved module `uses`).
    ///
    /// Every other system module is recorded as outside the boot
    /// configuration ([`Self::is_outside_boot_configuration`]): an aggregator
    /// such as `java.se` (it exports nothing, nothing requires it) and an
    /// incubator module not named by `--add-modules`, measured on HotSpot 25.
    /// Interpreter round i4 wave 86, lane L5 (probe `L5W86ModuleLayers`).
    ///
    /// `limit_modules` (`--limit-modules`, wave 87): when not empty, only the
    /// `requires` closure of these, `add_modules` and `java.base` is
    /// observable; a system module outside it is outside the configuration
    /// whatever it holds.
    pub fn settle_boot_configuration(&mut self, add_modules: &[String], limit_modules: &[String]) {
        let is_system = |name: &str, desc: &ModuleDescriptor| {
            !desc.automatic && is_platform_module_name(name)
        };
        let default_root = |name: &str, desc: &ModuleDescriptor| {
            is_system(name, desc)
                && !name.starts_with("jdk.incubator.")
                && desc.exports.iter().any(|e| e.to_modules.is_empty())
        };
        let mut all_system = false;
        let mut all_default = false;
        let mut extra: Vec<String> = Vec::new();
        for token in add_modules.iter().flat_map(|s| s.split(',')).map(str::trim) {
            match token {
                "" => {}
                "ALL-SYSTEM" => all_system = true,
                "ALL-DEFAULT" => all_default = true,
                other => extra.push(other.to_string()),
            }
        }
        let _ = all_default; // the default roots are always in for a class-path launch
        // `--limit-modules`: the observable set, `None` when unlimited.
        let observable: Option<FxHashSet<String>> = if limit_modules.is_empty() {
            None
        } else {
            let mut seen: FxHashSet<String> = FxHashSet::default();
            let mut stack: Vec<String> = limit_modules.to_vec();
            stack.extend(extra.iter().cloned());
            stack.push("java.base".to_string());
            while let Some(name) = stack.pop() {
                if !seen.insert(name.clone()) {
                    continue;
                }
                if let Some(desc) = self.modules.get(&name) {
                    for req in desc.requires.iter().filter(|r| !r.is_static) {
                        if !seen.contains(&req.module_name) {
                            stack.push(req.module_name.clone());
                        }
                    }
                }
            }
            Some(seen)
        };
        let is_observable = |name: &str, desc: &ModuleDescriptor| {
            !is_system(name, desc) || observable.as_ref().map_or(true, |o| o.contains(name))
        };
        let mut resolved: FxHashSet<String> = FxHashSet::default();
        let mut work: Vec<String> = Vec::new();
        for (name, desc) in &self.modules {
            let root = if is_system(name, desc) {
                is_observable(name, desc) && (all_system || default_root(name, desc))
            } else {
                !desc.automatic
            };
            if root {
                work.push(name.clone());
            }
        }
        work.extend(extra);
        // `java.base` is in every configuration.
        work.push("java.base".to_string());
        loop {
            while let Some(name) = work.pop() {
                if self
                    .modules
                    .get(&name)
                    .is_some_and(|desc| !is_observable(&name, desc))
                {
                    continue;
                }
                if !resolved.insert(name.clone()) {
                    continue;
                }
                if let Some(desc) = self.modules.get(&name) {
                    for req in desc.requires.iter().filter(|r| !r.is_static) {
                        if !resolved.contains(&req.module_name) {
                            work.push(req.module_name.clone());
                        }
                    }
                }
            }
            // Service binding: a module providing a service a resolved
            // module uses joins, then its own requires.
            let used: FxHashSet<&str> = resolved
                .iter()
                .filter_map(|n| self.modules.get(n))
                .flat_map(|d| d.uses.iter().map(String::as_str))
                .collect();
            for (name, desc) in &self.modules {
                if !resolved.contains(name)
                    && !desc.automatic
                    && is_observable(name, desc)
                    && desc.provides.iter().any(|p| used.contains(p.service.as_str()))
                {
                    work.push(name.clone());
                }
            }
            if work.is_empty() {
                break;
            }
        }
        // Kept narrow on purpose (w86a battery: a reflective access check
        // turned permissive once every unresolved image module left the boot
        // layer): only a module with no package of its own (an aggregator
        // such as `java.se`) or an incubator module goes out. An internal
        // module that only exports to named friends (`jdk.internal.vm.ci`)
        // stays in, as before, though HotSpot would not resolve it either.
        let with_packages: FxHashSet<&str> =
            self.package_to_module.values().map(|m| &**m).collect();
        self.outside_boot_configuration = self
            .modules
            .iter()
            .filter(|(name, desc)| {
                is_system(name, desc)
                    && !resolved.contains(*name)
                    && (name.starts_with("jdk.incubator.")
                        || !with_packages.contains(name.as_str())
                        || !is_observable(name, desc))
            })
            .map(|(name, _)| name.clone())
            .collect();
        self.boot_configuration = resolved
            .into_iter()
            .filter(|name| self.modules.contains_key(name))
            .collect();
    }

    /// The registered modules the settled boot configuration resolves
    /// ([`Self::settle_boot_configuration`]), sorted; empty before it is
    /// settled. Unlike [`Self::is_outside_boot_configuration`] this is the
    /// resolver's exact answer: on Temurin 25 a class-path launch leaves out
    /// `java.se`, `jdk.crypto.ec`, `jdk.graal.compiler`,
    /// `jdk.graal.compiler.management`, `jdk.hotspot.agent`,
    /// `jdk.incubator.vector`, `jdk.internal.vm.ci` and `jdk.jcmd` (62 of the
    /// image's 70 modules in). Interpreter round i4 wave 99, lane L3.
    pub fn boot_configuration_modules(&self) -> Vec<String> {
        let mut names: Vec<String> = self.boot_configuration.iter().cloned().collect();
        names.sort();
        names
    }

    /// The incubator modules the settled boot configuration resolves (named
    /// by `--add-modules`), sorted: `ModuleBootstrap` warns `Using incubator
    /// modules: <a>, <b>` for them. Meaningful only after
    /// [`Self::settle_boot_configuration`] (the VM asks right after it).
    pub fn resolved_incubator_modules(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .modules
            .iter()
            .filter(|(name, desc)| {
                !desc.automatic
                    && name.starts_with("jdk.incubator.")
                    && !self.outside_boot_configuration.contains(*name)
            })
            .map(|(name, _)| name.clone())
            .collect();
        names.sort();
        names
    }

    /// Is the class `internal_name` (slash form) in a package of a system
    /// module the boot configuration does not resolve? Such a class is not
    /// loadable (HotSpot: no loader maps the package), interpreter round i4
    /// wave 87, lane L5. One empty-set test before it is settled.
    pub fn class_outside_boot_configuration(&self, internal_name: &str) -> bool {
        if self.outside_boot_configuration.is_empty() {
            return false;
        }
        let Some((pkg, _)) = internal_name.rsplit_once('/') else {
            return false;
        };
        self.package_to_module
            .get(pkg)
            .is_some_and(|m| self.outside_boot_configuration.contains(&**m))
    }

    /// Is `module` a system module the boot configuration does not resolve
    /// ([`Self::settle_boot_configuration`])? `false` before it is settled.
    pub fn is_outside_boot_configuration(&self, module: &str) -> bool {
        self.outside_boot_configuration.contains(module)
    }

    /// Record the module names of `module`'s `ModuleHashes` attribute.
    pub fn set_recorded_hash_names(&mut self, module: &str, names: Vec<String>) {
        self.recorded_hash_names.insert(module.to_string(), names);
    }

    /// The module names `module`'s `ModuleHashes` attribute records, if it
    /// carries one.
    pub fn recorded_hash_names(&self, module: &str) -> Option<&[String]> {
        self.recorded_hash_names.get(module).map(Vec::as_slice)
    }

    /// Hand in the modules the JDK's table defines to the platform loader
    /// (see the field). Called once per VM at boot; a later call replaces.
    pub fn set_platform_loader_modules<I: IntoIterator<Item = String>>(&mut self, modules: I) {
        self.platform_loader_modules = Some(modules.into_iter().collect());
    }

    /// Is `module` one the JDK's table defines to the platform loader?
    /// `false` when the table was not handed in.
    pub fn is_platform_loader_module(&self, module: &str) -> bool {
        self.platform_loader_modules
            .as_ref()
            .is_some_and(|set| set.contains(module))
    }

    /// Hand in the modules the JDK's table defines to the bootstrap loader
    /// (see the field). Called once per VM at boot; a later call replaces.
    pub fn set_boot_loader_modules<I: IntoIterator<Item = String>>(&mut self, modules: I) {
        self.boot_loader_modules = Some(modules.into_iter().collect());
    }

    /// zt df4 §9c: the built-in loader the JDK's `ModuleLoaderMap` defines
    /// the image module `module` to, for a class this VM defined in its boot
    /// namespace: `Extension` for a platform module; `Application` for a
    /// module this registry holds that is in neither table, once the boot
    /// set is handed in (the launcher hands it in only under
    /// `CRATONVM_IMAGE_APP_MODULE_LOADERS`); else `None` (a boot module, a
    /// module this registry does not hold, or the tables not handed in: the
    /// class is the bootstrap loader's, as tagged).
    pub fn image_module_builtin_loader(&self, module: &str) -> Option<cratonvm_types::ClassLoaderId> {
        if module == "java.base" {
            return None;
        }
        let platform = self.platform_loader_modules.as_ref()?;
        if platform.contains(module) {
            return Some(cratonvm_types::ClassLoaderId::Extension);
        }
        let boot = self.boot_loader_modules.as_ref()?;
        (!boot.contains(module) && self.modules.contains_key(module))
            .then_some(cratonvm_types::ClassLoaderId::Application)
    }

    // -----------------------------------------------------------------------
    // Modules of non-boot layers, by identity (`--jdk-only`)
    // -----------------------------------------------------------------------

    /// Record the module `name` of a non-boot layer defined to the
    /// user-defined loader `loader_ns`, holding `packages` (internal form):
    /// `Module.defineModule0`'s VM half. `is_open` is its `isOpen` argument
    /// (an open or automatic module). A second definition of the same
    /// `(loader_ns, name)` replaces the first; a package already recorded for
    /// the loader moves to this module (last writer wins, as the record
    /// `Class.getModule()` reads).
    pub fn define_layer_module(
        &mut self,
        loader_ns: u32,
        name: &str,
        is_open: bool,
        packages: &[String],
    ) {
        self.layer_modules.insert(
            (loader_ns, name.to_string()),
            LayerModule {
                is_open,
                exports: FxHashMap::default(),
                version: None,
                reads: Vec::new(),
            },
        );
        let map = self.layer_packages.entry(loader_ns).or_default();
        for pkg in packages {
            map.insert(pkg.clone(), name.to_string());
        }
    }

    /// Record that the layer module `(loader_ns, name)` exports `pkg`
    /// (internal form) to `target`: the VM half of `Module.addExports0` /
    /// `addExportsToAll0` / `addExportsToAllUnnamed0` for such a module (the
    /// JDK calls them for the module's `exports` and `opens` directives and
    /// for every run-time edge). Ignored for a module never defined.
    pub fn add_layer_export(
        &mut self,
        loader_ns: u32,
        name: &str,
        pkg: &str,
        target: LayerExportTarget,
    ) {
        if let Some(module) = self.layer_modules.get_mut(&(loader_ns, name.to_string())) {
            let targets = module.exports.entry(pkg.to_string()).or_default();
            if !targets.contains(&target) {
                targets.push(target);
            }
        }
    }

    /// Record that the layer module `(loader_ns, name)` reads `target`: the
    /// VM half of `Module.addReads0` for such a module (interpreter round i1
    /// wave 45, lane L5). Ignored for a module never defined.
    pub fn add_layer_read(&mut self, loader_ns: u32, name: &str, target: LayerExportTarget) {
        if let Some(module) = self.layer_modules.get_mut(&(loader_ns, name.to_string())) {
            if !module.reads.contains(&target) {
                module.reads.push(target);
            }
        }
    }

    /// Does the layer module `(loader_ns, name)` read `provider` (JVMS
    /// §5.4.4's readability clause, HotSpot's `MODULE_NOT_READABLE`)? Itself
    /// and `java.base` always; otherwise a module `addReads0` named for it.
    /// A module the registry never saw defined, or one for which no read was
    /// recorded (the JDK records `java.base` for every module, so an empty
    /// list means the VM missed them), answers `true`: nothing to decide
    /// from.
    pub fn layer_module_reads(
        &self,
        loader_ns: u32,
        name: &str,
        provider: ModuleIdent<'_>,
    ) -> bool {
        if provider == (ModuleIdent::Layer { loader_ns, name })
            || provider == ModuleIdent::Named(JAVA_BASE)
        {
            return true;
        }
        let Some(module) = self.layer_modules.get(&(loader_ns, name.to_string())) else {
            return true;
        };
        module.reads.is_empty() || module.reads.iter().any(|t| t.admits(provider))
    }

    /// Record the descriptor version of the layer module `(loader_ns, name)`
    /// (`defineModule0`'s `version`). Ignored for a module never defined.
    pub fn set_layer_module_version(&mut self, loader_ns: u32, name: &str, version: &str) {
        if let Some(module) = self.layer_modules.get_mut(&(loader_ns, name.to_string())) {
            module.version = (!version.is_empty()).then(|| version.to_string());
        }
    }

    /// The descriptor version of the layer module `(loader_ns, name)`, if it
    /// has one.
    pub fn layer_module_version(&self, loader_ns: u32, name: &str) -> Option<&str> {
        self.layer_modules
            .get(&(loader_ns, name.to_string()))?
            .version
            .as_deref()
    }

    /// Whether any layer module was recorded: the one test every access
    /// question about a user-defined loader's class makes before it looks
    /// further. Always `false` in `--compatible`.
    #[inline]
    pub fn has_layer_modules(&self) -> bool {
        !self.layer_modules.is_empty()
    }

    /// The name of the layer module holding `pkg` (internal form) for the
    /// user-defined loader `loader_ns`, if one does.
    pub fn layer_module_for_package(&self, loader_ns: u32, pkg: &str) -> Option<&str> {
        self.layer_packages
            .get(&loader_ns)?
            .get(pkg)
            .map(String::as_str)
    }

    /// Is `pkg` of the layer module `(loader_ns, name)` exported to
    /// `accessor`? The module itself, an open module, and a module the
    /// registry never saw defined (nothing to decide from) all answer `true`.
    /// Is the layer module `(loader_ns, name)` an OPEN module (`open module
    /// m`), which opens every package to every module? `false` for a module
    /// never defined. The only opens a layer module's record answers: its
    /// `opens` directives and run-time `addOpens` live in the JDK's `Module`
    /// fields (`access_control::module_access`).
    pub fn is_layer_module_open(&self, loader_ns: u32, name: &str) -> bool {
        self.layer_modules
            .get(&(loader_ns, name.to_string()))
            .is_some_and(|module| module.is_open)
    }

    pub fn is_layer_package_exported_to(
        &self,
        loader_ns: u32,
        name: &str,
        pkg: &str,
        accessor: ModuleIdent<'_>,
    ) -> bool {
        if accessor == (ModuleIdent::Layer { loader_ns, name }) {
            return true;
        }
        let Some(module) = self.layer_modules.get(&(loader_ns, name.to_string())) else {
            return true;
        };
        if module.is_open {
            return true;
        }
        module
            .exports
            .get(pkg)
            .is_some_and(|targets| targets.iter().any(|t| t.admits(accessor)))
    }

    /// Does the non-boot layer's module `(loader_ns, name)` export `pkg` to
    /// EVERY module (`Module.isExported(pn)`): open, or an `addExportsToAll0`
    /// edge (`LayerExportTarget::Everyone`) for the package. `None` when the
    /// VM holds no record of the module (the caller asks the JDK's `Module`).
    /// Interpreter round i1 wave 49, lane L6
    /// (`interpreter-L6-proposal-an-unqualified-export-question-by-identity-FIXED-20261013.md`).
    pub fn is_layer_package_exported_to_all(
        &self,
        loader_ns: u32,
        name: &str,
        pkg: &str,
    ) -> Option<bool> {
        let module = self.layer_modules.get(&(loader_ns, name.to_string()))?;
        Some(
            module.is_open
                || module
                    .exports
                    .get(pkg)
                    .is_some_and(|targets| targets.contains(&LayerExportTarget::Everyone)),
        )
    }

    /// Register a module and index its packages.
    ///
    /// If a module with the same name was already registered it is silently
    /// replaced (last writer wins — callers should avoid duplicates).
    pub fn register(&mut self, desc: ModuleDescriptor, packages: Vec<String>) {
                let name = desc.name.clone();
        let shared: std::sync::Arc<str> = std::sync::Arc::from(name.as_str());
        // Index packages
        for pkg in packages {
            self.package_to_module
                .entry(pkg)
                .or_insert_with(|| shared.clone());
        }
        self.modules.insert(name, desc);
        // Invalidate graph when new modules arrive
        self.graph_built = false;
        self.readable.clear();
    }

    /// Register a package-to-module mapping directly with a pre-shared module Arc.
    pub fn add_package_to_module_arc(&mut self, package: &str, module: std::sync::Arc<str>) {
        self.package_to_module
            .entry(package.to_string())
            .or_insert(module);
    }

    /// Look up a module descriptor by name.
    pub fn get(&self, name: &str) -> Option<&ModuleDescriptor> {
        self.modules.get(name)
    }

    /// Return an iterator over all registered module descriptors.
    pub fn all(&self) -> impl Iterator<Item = &ModuleDescriptor> {
        self.modules.values()
    }

    /// Number of registered modules.
    pub fn len(&self) -> usize {
        self.modules.len()
    }

    /// True if no modules have been registered.
    pub fn is_empty(&self) -> bool {
        self.modules.is_empty()
    }

    /// Determine which named module owns `pkg` (slash format, e.g. `"java/lang"`).
    ///
    /// Returns `None` if no registered module declares that package — meaning
    /// the class belongs to the unnamed module.
    pub fn module_for_package(&self, pkg: &str) -> Option<&str> {
        self.package_to_module.get(pkg).map(|s| &**s)
    }

    /// [`Self::module_for_package`], but answering the question a CLASS's
    /// module membership asks: which module does a class in `pkg` belong to?
    ///
    /// The difference is the class path. A modular jar reached through `-cp`
    /// has a `module-info.class` and this registry keeps its descriptor — for
    /// service discovery, for labelling, for `packages_of` — but **a real JVM
    /// ignores that descriptor outright**: a modular JAR on the class path is
    /// treated as an ordinary JAR and its classes land in the UNNAMED module.
    /// [`Self::service_providers`] already filters on exactly this argument, in
    /// exactly these words; class membership is the surface that was left
    /// reading the raw map.
    ///
    /// MEASURED 2026-09-05. Tomcat's Linux suite runs `-c .../catalina.jar`,
    /// and `catalina.jar` carries a `module-info.class` declaring
    /// `org.apache.tomcat.catalina`. With the raw map,
    /// `WebappClassLoaderBase.class.getModule()` answered that named module, so
    /// `--add-opens java.base/java.util=ALL-UNNAMED` — which is qualified to
    /// the unnamed module, see [`ALL_UNNAMED_TARGET`] — did not reach it, and
    /// `clearReferencesStopTimerThread`'s `setAccessible` threw
    /// `InaccessibleObjectException: module java.base does not "opens
    /// java.util" to org.apache.tomcat.catalina`. HotSpot passes the identical
    /// classpath and flags. That is
    /// `TestWebappClassLoaderMemoryLeak`/`…ExecutorMemoryLeak`; the failure is
    /// invisible when the same classes are reached through EXPLODED directories
    /// (the Windows suite's classpath), because a directory carries no
    /// `module-info` to register — which is why one host reproduced it 1/1 and
    /// the other 0/1 on the same commit.
    ///
    /// `--module-path` modules are unaffected: `vm_init` re-registers each one
    /// with `automatic = false` right after `ClassManager::new`, so
    /// [`Self::is_class_path_only`] is false for them and they keep their
    /// names. Platform modules are registered `automatic = false` by the
    /// boot/ext scan for the same reason.
    pub fn named_module_for_package(&self, pkg: &str) -> Option<&str> {
        let name = self.module_for_package(pkg)?;
        if self.is_class_path_only(name) {
            return None;
        }
        Some(name)
    }

    /// Compute the transitive readability closure and cache it.
    ///
    /// After this call, `reads()` and `can_access()` become meaningful.
    /// Safe to call multiple times; subsequent calls are no-ops if no new
    /// modules have been registered since the last call.
    ///
    /// # Performance — round-8 HIGH classloading finding
    ///
    /// This is an O(N²) operation in the number of registered modules
    /// (each fixpoint iteration scans every module's readable-set
    /// against every transitive edge). It is invalidated every time
    /// `register()` is called, so a workload that registers modules
    /// one-by-one through `Module.defineModule` (JVM JPMS API, Jigsaw
    /// agents) pays the rebuild cost on each call.
    ///
    /// Since the boot module registry became eagerly populated (the
    /// `CRATONVM_BOOT_MODULE_REGISTRY` path scans every boot/ext/app
    /// `module-info.class` at `ClassManager::new`), the steady-state count is
    /// no longer ~15: the JDK ships ~70 named modules and a large modular app
    /// (e.g. the Hibernate ORM suite) pushes the total to ~140. The O(N²)
    /// rebuild is still trivial at these scales — a few hundred modules is
    /// sub-millisecond and dwarfed by per-module class-parse cost — so the
    /// previous "fewer than 50" assumption (and the `<= 100` tripwire) was a
    /// pre-eager-registration estimate that normal runs now legitimately
    /// exceed. The `debug_assert!` below is retained only to catch genuine
    /// *runaway* growth (a Jigsaw agent / fuzzer defining thousands of modules
    /// one-by-one through `Module.defineModule`, where the O(N³) of
    /// register-then-rebuild per call would bite); at that point the
    /// incremental-rebuild fix (only recompute readability for newly added
    /// modules, intersect with existing closure) becomes warranted.
    pub fn build_readability_graph(&mut self) {
        debug_assert!(
            self.modules.len() <= 4096,
            "ModuleRegistry: O(N²) readability rebuild grew to {} modules — \
             runaway module registration; time to switch to incremental update \
             (see round-8 HIGH finding)",
            self.modules.len()
        );
        if self.graph_built {
            return;
        }

        // Step 1 — seed each module's readable set with direct requires
        // and dynamic addReads edges.
        let module_names: Vec<String> = self.modules.keys().cloned().collect();
        let mut readable: FxHashMap<String, FxHashSet<String>> =
            FxHashMap::with_capacity_and_hasher(16, Default::default());

        for name in &module_names {
            let set = readable.entry(name.clone()).or_default();
            // A module always reads itself.
            set.insert(name.clone());
            // Every module reads java.base (mandated).
            set.insert(JAVA_BASE.to_string());

            if let Some(desc) = self.modules.get(name) {
                for req in &desc.requires {
                    if !req.is_static {
                        set.insert(req.module_name.clone());
                    }
                }
            }

            // Incorporate dynamic reads from add_reads() / --add-reads.
            if let Some(extras) = self.extra_reads.get(name) {
                for target in extras {
                    set.insert(target.clone());
                }
            }
        }

        // Step 2 — propagate `requires transitive`.
        // If M `requires transitive` B, then every module that reads M also
        // reads B.  Iterate until stable.
        //
        // Pre-compute the transitive-requires edges once (they don't change
        // during propagation) to avoid repeated HashMap lookups and String
        // clones inside the fixpoint loop.
        let transitive_edges: Vec<(usize, Vec<usize>)> = {
            // Build name-to-index map for compact representation
            let name_to_idx: FxHashMap<&str, usize> = module_names
                .iter()
                .enumerate()
                .map(|(i, n)| (n.as_str(), i))
                .collect();

            module_names
                .iter()
                .enumerate()
                .filter_map(|(m_idx, m_name)| {
                    let desc = self.modules.get(m_name)?;
                    let trans: Vec<usize> = desc
                        .requires
                        .iter()
                        .filter(|r| r.is_transitive && !r.is_static)
                        .filter_map(|r| name_to_idx.get(r.module_name.as_str()).copied())
                        .collect();
                    if trans.is_empty() {
                        None
                    } else {
                        Some((m_idx, trans))
                    }
                })
                .collect()
        };

        let mut changed = true;
        while changed {
            changed = false;
            for &(m_idx, ref trans_idxs) in &transitive_edges {
                let m_name = &module_names[m_idx];

                // Find every module X that reads M (by index)
                let reader_idxs: Vec<usize> = module_names
                    .iter()
                    .enumerate()
                    .filter(|(_, name)| {
                        readable
                            .get(name.as_str())
                            .is_some_and(|set| set.contains(m_name.as_str()))
                    })
                    .map(|(i, _)| i)
                    .collect();

                for reader_idx in reader_idxs {
                    let reader_name = &module_names[reader_idx];
                    for &dep_idx in trans_idxs {
                        let dep_name = &module_names[dep_idx];
                        if readable
                            .entry(reader_name.clone())
                            .or_default()
                            .insert(dep_name.clone())
                        {
                            changed = true;
                        }
                    }
                }
            }
        }

        self.readable = readable;
        self.graph_built = true;
    }

    /// Detect cycles in the `requires` graph (excluding self-loops via
    /// `java.base` which every module reads).
    ///
    /// Returns a list of cycles, where each cycle is a list of module names
    /// forming the cycle. An empty Vec means no cycles.
    ///
    /// JPMS specification does not forbid cycles in the `requires` graph
    /// (only circular dependencies during class loading are errors), but
    /// detecting them is useful for diagnostics and spec conformance testing.
    pub fn detect_cycles(&self) -> Vec<Vec<String>> {
        let names: Vec<&str> = self.modules.keys().map(|s| s.as_str()).collect();
        let name_to_idx: FxHashMap<&str, usize> =
            names.iter().enumerate().map(|(i, n)| (*n, i)).collect();
        let n = names.len();

        // Build adjacency list (only real requires, not self/java.base).
        let mut adj: Vec<Vec<usize>> = vec![vec![]; n];
        for (i, name) in names.iter().enumerate() {
            if let Some(desc) = self.modules.get(*name) {
                for req in &desc.requires {
                    if !req.is_static {
                        if let Some(&j) = name_to_idx.get(req.module_name.as_str()) {
                            if i != j {
                                adj[i].push(j);
                            }
                        }
                    }
                }
            }
        }

        // Standard DFS-based cycle detection (Tarjan-flavored).
        let mut visited = vec![false; n];
        let mut on_stack = vec![false; n];
        let mut stack: Vec<usize> = Vec::new();
        let mut cycles: Vec<Vec<String>> = Vec::new();

        fn dfs(
            v: usize,
            adj: &[Vec<usize>],
            visited: &mut [bool],
            on_stack: &mut [bool],
            stack: &mut Vec<usize>,
            cycles: &mut Vec<Vec<String>>,
            names: &[&str],
        ) {
            visited[v] = true;
            on_stack[v] = true;
            stack.push(v);

            for &w in &adj[v] {
                if !visited[w] {
                    dfs(w, adj, visited, on_stack, stack, cycles, names);
                } else if on_stack[w] {
                    // Found a cycle: extract the cycle from the stack.
                    let pos = stack.iter().position(|&x| x == w).unwrap_or(0);
                    let cycle: Vec<String> =
                        stack[pos..].iter().map(|&i| names[i].to_string()).collect();
                    cycles.push(cycle);
                }
            }

            stack.pop();
            on_stack[v] = false;
        }

        for i in 0..n {
            if !visited[i] {
                dfs(
                    i,
                    &adj,
                    &mut visited,
                    &mut on_stack,
                    &mut stack,
                    &mut cycles,
                    &names,
                );
            }
        }

        cycles
    }

    /// Does module `reader` read module `provider`?
    ///
    /// The unnamed module reads everything.  Every module reads `java.base`
    /// and itself.  If the graph has not been built yet this falls back to
    /// checking only direct `requires` edges.
    ///
    /// # The unnamed-module rule is DIRECTIONAL
    ///
    /// The classpath-compatibility escape hatch is the `reader ==
    /// UNNAMED_MODULE` arm below, and only that arm. This function used to
    /// carry a second, symmetric arm — `provider == UNNAMED_MODULE` — so a
    /// NAMED module implicitly read the unnamed module too. JPMS says it does
    /// not, and the JDK agrees; measured on HotSpot 25:
    ///
    /// ```text
    ///   unnamed.canRead(java.logging)      = true
    ///   java.logging.canRead(unnamed)      = false
    ///   java.logging.canRead(java.base)    = true
    ///   java.base.canRead(java.logging)    = false
    ///   // and with --add-reads java.logging=ALL-UNNAMED:
    ///   java.logging.canRead(unnamed)      = true
    /// ```
    ///
    /// (`regression-suite/src/RJdkModule.java:124`,
    /// `check(!svc.canRead(unnamed), "a named module must NOT implicitly read
    /// the unnamed module")`, failed in BOTH `--real-jdk` and `--jdk-only` on
    /// the symmetric rule.)
    ///
    /// Dropping the symmetry does not weaken the escape hatch, because no
    /// access check ever reaches this arm:
    ///
    /// * [`Self::check_module_access`] returns `Ok(())` for `accessor_module ==
    ///   UNNAMED_MODULE || target_module == UNNAMED_MODULE` *before* it calls
    ///   `reads`.
    /// * [`Self::check_deep_reflection_access`] returns `Ok(())` for
    ///   `target_module == UNNAMED_MODULE` *before* it calls `reads`.
    /// * A classpath jar carrying a `module-info.class` is registered
    ///   `automatic`, and the automatic arm further down returns `true` for
    ///   every provider including the unnamed one — so `org.jboss.logging` and
    ///   friends are unaffected.
    /// * A module with no registered descriptor still gets the open-world
    ///   `true`.
    /// * An explicit grant still works: `--add-reads m=ALL-UNNAMED` and the
    ///   `Module.addReads0` VM-sync hook (Mockito's
    ///   `InlineBytecodeGenerator.assureCanReadMockito` makes `java.base` read
    ///   the unnamed module this way) both land in `extra_reads` keyed on the
    ///   empty-string sentinel and are honoured on both the built-graph and the
    ///   un-built-fallback paths below.
    ///
    /// What is left is the JPMS query surface — `NativeContext::reads_module`,
    /// i.e. `java.lang.Module.canRead` — which is exactly where the directional
    /// answer is the correct one.
    pub fn reads(&self, reader: &str, provider: &str) -> bool {
        // Unnamed module reads all named modules (classpath compat). This is
        // the escape hatch, and it is one-way — see the doc comment.
        if reader == UNNAMED_MODULE {
            return true;
        }
        // Every module reads itself and java.base.
        if reader == provider || provider == JAVA_BASE {
            return true;
        }

        // If the reader module has no registered descriptor, use open-world
        // assumption: unknown modules can read anything.
        if !self.modules.contains_key(reader) {
            return true;
        }

        // Automatic (classpath) modules read every other module — JPMS gives an
        // automatic module an implicit `requires transitive` on every other
        // module (and on the unnamed module). On the class path (CratonVM's only
        // loading mode for app jars) `org.jboss.logging` must be able to reach
        // `org.apache.logging.log4j` even though its module-info does not
        // `requires` it.
        if self.modules.get(reader).is_some_and(|d| d.automatic) {
            return true;
        }
        // A module path's JPMS automatic module reads every module too
        // (interpreter round i4 wave 99, lane L5).
        if self.automatic_modules.contains(reader) {
            return true;
        }

        if self.graph_built {
            let set = self.readable.get(reader);
            if set.is_some_and(|set| set.contains(provider)) {
                return true;
            }
            // A module that reads one automatic module of the module path
            // reads them all: every automatic module grants implied
            // readability to every other automatic module (`java.lang.module`
            // `Configuration` / `Resolver.makeGraph`; interpreter round i5
            // wave 100, lane L5).
            return self.automatic_modules.contains(provider)
                && set.is_some_and(|set| {
                    set.iter()
                        .any(|module| self.automatic_modules.contains(module.as_str()))
                });
        }

        // ── Un-built fallback ────────────────────────────────────────────
        //
        // The cached closure is gone: either it was never computed, or
        // `register` invalidated it (`graph_built = false; readable.clear()`)
        // and nothing has rebuilt it yet. Two defects used to live here.
        //
        // 1. `requires static` edges were honoured. `build_readability_graph`
        //    step 1 filters them out (`if !req.is_static`), because a
        //    compile-time-only dependency is never readable at runtime. The
        //    fallback therefore *granted* readability that the built graph
        //    denies — the same query flipping answer purely on whether the
        //    closure happened to be cached. (`add_reads_requires_static_not_
        //    propagated` pinned the built-graph half of this only.)
        //
        // 2. Dynamic reads edges (`Module.addReads`, `--add-reads`,
        //    `NativeContext::module_add_reads`) were ignored entirely.
        //    `add_reads` patches `readable` only `if self.graph_built`,
        //    parking the edge in `extra_reads` otherwise — and nothing here
        //    consulted `extra_reads`. Repro: build the graph, `add_reads(A,
        //    B)` (patched in, `reads(A, B) == true`), then load any
        //    `module-info` class — `register` clears the closure and
        //    `reads(A, B)` silently reverts to `false` until something calls
        //    `build_readability_graph` again. `check_module_access` /
        //    `check_deep_reflection_access` both gate on `reads`, so the
        //    window turns a user's `--add-reads` into a spurious
        //    `IllegalAccessError` / `InaccessibleObjectException`.
        //
        // Mirror `build_readability_graph`'s seeding (non-static direct
        // requires) plus `add_reads`'s cached-graph patch (the edge itself and
        // the `requires transitive` closure it implies).
        if self.modules.get(reader).is_some_and(|desc| {
            desc.requires
                .iter()
                .any(|r| !r.is_static && r.module_name == provider)
        }) {
            return true;
        }
        if let Some(extras) = self.extra_reads.get(reader) {
            if extras.contains(provider) {
                return true;
            }
            for e in extras {
                let mut implied: FxHashSet<String> = FxHashSet::default();
                self.collect_transitive_requires(e, &mut implied);
                if implied.contains(provider) {
                    return true;
                }
            }
        }
        false
    }

    // -----------------------------------------------------------------------
    // Dynamic mutations (JPMS §5.4.4, java.lang.Module API)
    // -----------------------------------------------------------------------

    /// Collect the `requires transitive` closure of `module`, i.e. every module
    /// that becomes readable *implicitly* when some other module gains a reads
    /// edge to `module`.
    ///
    /// Per JPMS: a reads edge to M implies reading every module M `requires
    /// transitive`, and (recursively) every module those modules `requires
    /// transitive`. `module` itself is *not* included — only its implied
    /// dependencies.
    ///
    /// A local `visited` set bounds the iterative walk so it terminates on
    /// cyclic `requires transitive` graphs (JPMS permits cycles in the requires
    /// graph).
    fn collect_transitive_requires(&self, module: &str, out: &mut FxHashSet<String>) {
        let mut stack: Vec<String> = vec![module.to_string()];
        let mut visited: FxHashSet<String> = FxHashSet::default();
        visited.insert(module.to_string());

        while let Some(cur) = stack.pop() {
            let Some(desc) = self.modules.get(&cur) else {
                continue;
            };
            for req in &desc.requires {
                // Only `requires transitive` edges propagate implied readability;
                // `requires static` are compile-time only and never readable.
                if req.is_transitive && !req.is_static {
                    let dep = &req.module_name;
                    // `insert` returns false if already present → already walked,
                    // so we never re-push it (cycle-safe).
                    if visited.insert(dep.clone()) {
                        out.insert(dep.clone());
                        stack.push(dep.clone());
                    }
                }
            }
        }
    }

    /// Add a dynamic read edge: `reader` module reads `provider`.
    ///
    /// This is the backing store for `java.lang.Module.addReads()` and the
    /// `--add-reads` CLI flag. If the readability graph has already been built
    /// the edge is inserted directly; otherwise it will be picked up on the
    /// next `build_readability_graph` call via the `extra_reads` list.
    ///
    /// Per JPMS semantics, a reads edge to `provider` also makes `reader` read
    /// everything `provider` (transitively) `requires transitive`. We patch that
    /// implied closure into the cached graph here so a freshly-added edge has the
    /// same readability fan-out it would get from a full rebuild. (The full
    /// rebuild in `build_readability_graph` already propagates this via its
    /// fixpoint, so the un-built `extra_reads` path needs no extra work.)
    pub fn add_reads(&mut self, reader: &str, provider: &str) {
        self.extra_reads
            .entry(reader.to_string())
            .or_default()
            .insert(provider.to_string());
        // Patch the cached graph if it was already computed.
        if self.graph_built {
            // Compute the implied `requires transitive` closure of `provider`
            // before borrowing `readable` mutably (avoids a borrow conflict and
            // is cycle-safe via the visited set inside the helper).
            let mut implied: FxHashSet<String> = FxHashSet::default();
            self.collect_transitive_requires(provider, &mut implied);

            let set = self.readable.entry(reader.to_string()).or_default();
            set.insert(provider.to_string());
            for m in implied {
                set.insert(m);
            }
        }
    }

    /// The dynamic read edges recorded for `reader` (`--add-reads`,
    /// `Module.addReads0`), provider names; [`UNNAMED_MODULE`] (`""`) is the
    /// unnamed module. Sorted, so a replay is deterministic.
    pub fn dynamic_reads_of(&self, reader: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .extra_reads
            .get(reader)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default();
        out.sort();
        out
    }

    /// The dynamic exports recorded for `module_name` (`--add-exports`,
    /// `Module.addExports`), in recording order, as `(package, target)`: the
    /// stored [`DynamicExport::to_module`] (`None` = everyone, `Some("")` = the
    /// unnamed module).
    pub fn dynamic_exports_of(&self, module_name: &str) -> Vec<(String, Option<String>)> {
        self.extra_exports
            .get(module_name)
            .map(|v| {
                v.iter()
                    .map(|e| (e.package.clone(), e.to_module.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The dynamic opens recorded for `module_name`; the shape of
    /// [`Self::dynamic_exports_of`].
    pub fn dynamic_opens_of(&self, module_name: &str) -> Vec<(String, Option<String>)> {
        self.extra_opens
            .get(module_name)
            .map(|v| {
                v.iter()
                    .map(|e| (e.package.clone(), e.to_module.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Add a dynamic export: `module_name` now exports `pkg` to `target`
    /// (empty `target` = unqualified, to all modules;
    /// [`ALL_UNNAMED_TARGET`] = the unnamed module only).
    ///
    /// Backing store for `java.lang.Module.addExports()` and `--add-exports`.
    pub fn add_exports(&mut self, module_name: &str, pkg: &str, target: &str) {
        self.extra_exports
            .entry(module_name.to_string())
            .or_default()
            .push(DynamicExport {
                package: pkg.to_string(),
                to_module: resolve_edge_target(target),
                command_line: false,
                layer_target: None,
            });
    }

    /// [`Self::add_exports`] for a run-time edge whose target is the module
    /// `target` of a non-boot `ModuleLayer`, defined to the loader
    /// `target_loader_ns` (`native_module_add_exports0` with a user-layer
    /// `other`): recorded by that identity ([`DynamicExport::layer_target`]),
    /// so the layer questions under `CRATONVM_MODULE_ACCESS_EXPORT_ONE_QUESTION`
    /// tell it from an edge to the boot layer's or another layer's `target`.
    /// The name-keyed questions see it by name with the switch off, as before
    /// (interpreter round i6 wave 125, lane L5).
    pub fn add_exports_to_layer_module(
        &mut self,
        module_name: &str,
        pkg: &str,
        target: &str,
        target_loader_ns: u32,
    ) {
        self.extra_exports
            .entry(module_name.to_string())
            .or_default()
            .push(DynamicExport {
                package: pkg.to_string(),
                to_module: resolve_edge_target(target),
                command_line: false,
                layer_target: Some(target_loader_ns),
            });
    }

    /// [`Self::add_exports_to_layer_module`]'s `opens` twin.
    pub fn add_opens_to_layer_module(
        &mut self,
        module_name: &str,
        pkg: &str,
        target: &str,
        target_loader_ns: u32,
    ) {
        self.extra_opens
            .entry(module_name.to_string())
            .or_default()
            .push(DynamicExport {
                package: pkg.to_string(),
                to_module: resolve_edge_target(target),
                command_line: false,
                layer_target: Some(target_loader_ns),
            });
    }

    /// [`Self::add_exports`] for a command-line `--add-exports` edge: its
    /// target is a boot-layer module ([`DynamicExport::command_line`]).
    pub fn add_command_line_exports(&mut self, module_name: &str, pkg: &str, target: &str) {
        self.extra_exports
            .entry(module_name.to_string())
            .or_default()
            .push(DynamicExport {
                package: pkg.to_string(),
                to_module: resolve_edge_target(target),
                command_line: true,
                layer_target: None,
            });
    }

    /// [`Self::add_opens`] for a command-line `--add-opens` edge: its target
    /// is a boot-layer module ([`DynamicExport::command_line`]).
    pub fn add_command_line_opens(&mut self, module_name: &str, pkg: &str, target: &str) {
        self.extra_opens
            .entry(module_name.to_string())
            .or_default()
            .push(DynamicExport {
                package: pkg.to_string(),
                to_module: resolve_edge_target(target),
                command_line: true,
                layer_target: None,
            });
    }

    /// Add a dynamic open: `module_name` now opens `pkg` to `target`
    /// (empty `target` = unqualified, to all modules;
    /// [`ALL_UNNAMED_TARGET`] = the unnamed module only).
    ///
    /// Backing store for `java.lang.Module.addOpens()` and `--add-opens`.
    pub fn add_opens(&mut self, module_name: &str, pkg: &str, target: &str) {
        self.extra_opens
            .entry(module_name.to_string())
            .or_default()
            .push(DynamicExport {
                package: pkg.to_string(),
                to_module: resolve_edge_target(target),
                command_line: false,
                layer_target: None,
            });
    }

    /// Check deep reflection access (JPMS `opens`, JEP 403 "strong encapsulation").
    ///
    /// This is called by reflection code (`Method.invoke`, `Field.set/get`,
    /// `Constructor.newInstance`, `AccessibleObject.setAccessible`) to enforce
    /// that `accessor_module` has deep reflective access to `target_pkg` in
    /// `target_module`.
    ///
    /// Rules (JEP 403, JPMS §5.4.4, JDK 17+):
    /// 1. Same module → allowed.
    /// 2. Target is unnamed (classpath class) → allowed. An unnamed module
    ///    cannot encapsulate — every one of its packages is implicitly open.
    /// 3. Target is an open module → allowed (`module M { open ... }`).
    /// 4. Target explicitly `opens target_pkg [to accessor]` in its
    ///    module-info → allowed.
    /// 5. A dynamic `add_opens` edge exists (either from the
    ///    `--add-opens` CLI flag or the `java.lang.Module.addOpens` runtime
    ///    API) → allowed.
    /// 6. Otherwise → `Err(...)`. The reflection native turns this into an
    ///    `InaccessibleObjectException` (for `setAccessible`) or
    ///    `IllegalAccessException` (for direct `invoke`/`get`/`set`).
    ///
    /// Note: an unnamed *accessor* is NOT automatically allowed. Pre-JEP-403
    /// JDKs (9–16) treated classpath code as a special case, but modular
    /// JDK 17+ strong encapsulation requires `--add-opens` to reach
    /// non-opened packages of a named module — this matches HotSpot's
    /// `Module::can_access_member` behavior.
    pub fn check_deep_reflection_access(
        &self,
        accessor_module: &str,
        target_module: &str,
        target_pkg: &str,
    ) -> Result<(), String> {
        // Rule 1: same module always wins.
        if accessor_module == target_module {
            return Ok(());
        }

        // Rule 2: target unnamed — no encapsulation to enforce.
        if target_module == UNNAMED_MODULE {
            return Ok(());
        }

        // Readability required before deep access can be meaningful.
        if !self.reads(accessor_module, target_module) {
            return Err(format!(
                "module {accessor_module} does not read module {target_module}"
            ));
        }

        // Rules 3 + 4: declared opens on the target module.
        if let Some(desc) = self.modules.get(target_module) {
            if desc.opens_package_to(target_pkg, accessor_module) {
                return Ok(());
            }
        }

        // Rule 5: dynamic opens (from --add-opens or Module.addOpens).
        if let Some(extras) = self.extra_opens.get(target_module) {
            for de in extras {
                if de.package == target_pkg
                    && (de.to_module.is_none() || de.to_module.as_deref() == Some(accessor_module))
                {
                    return Ok(());
                }
            }
        }

        // Rule 6: denied.
        let accessor_label = if accessor_module.is_empty() {
            "unnamed module"
        } else {
            accessor_module
        };
        Err(format!(
            "module {target_module} does not \"opens {}\" to {accessor_label}",
            target_pkg.replace('/', ".")
        ))
    }

    // -----------------------------------------------------------------------
    // Single-module queries (for java.lang.Module native methods)
    // -----------------------------------------------------------------------

    /// Is `pkg` exported by `module_name` unconditionally (to all modules)?
    ///
    /// This implements `Module.isExported(String)` — checks whether the package
    /// is in the unqualified exports list or the module is open.
    pub fn is_package_exported_unqualified(&self, module_name: &str, pkg: &str) -> bool {
        if module_name == UNNAMED_MODULE {
            return true; // unnamed module exports everything
        }
        if let Some(desc) = self.modules.get(module_name) {
            // Automatic modules (classpath jars carrying a module-info) export
            // every package to everyone, matching `exports_package_to`.
            if desc.automatic || desc.is_open {
                return true;
            }
            if desc
                .exports
                .iter()
                .any(|e| e.package_name == pkg && e.to_modules.is_empty())
            {
                return true;
            }
        }
        // Check dynamic exports (unqualified).
        if let Some(extras) = self.extra_exports.get(module_name) {
            if extras
                .iter()
                .any(|de| de.package == pkg && de.to_module.is_none())
            {
                return true;
            }
        }
        // An unqualified OPEN is an unqualified export (JIT round 20 lane
        // superAcc; see `is_package_exported_to`): `Module.isExported(pn)`
        // answers true, and the JDK tells the VM with `addExportsToAll0`.
        if let Some(desc) = self.modules.get(module_name) {
            if desc
                .opens
                .iter()
                .any(|o| o.package_name == pkg && o.to_modules.is_empty())
            {
                return true;
            }
        }
        if let Some(extras) = self.extra_opens.get(module_name) {
            if extras
                .iter()
                .any(|de| de.package == pkg && de.to_module.is_none())
            {
                return true;
            }
        }
        false
    }

    /// Is `pkg` exported by `module_name` to `to_module`?
    ///
    /// This implements `Module.isExported(String, Module)`.
    pub fn is_package_exported_to(&self, module_name: &str, pkg: &str, to_module: &str) -> bool {
        if module_name == UNNAMED_MODULE || module_name == to_module {
            return true;
        }
        if let Some(desc) = self.modules.get(module_name) {
            if desc.exports_package_to(pkg, to_module) {
                return true;
            }
        }
        // Check dynamic exports.
        if let Some(extras) = self.extra_exports.get(module_name) {
            if extras.iter().any(|de| {
                de.package == pkg
                    && (de.to_module.is_none() || de.to_module.as_deref() == Some(to_module))
            }) {
                return true;
            }
        }
        // An OPENED package is exported too (JIT round 20 lane superAcc):
        // `Module.isExported(pn, other)` answers true for a package open to
        // `other`, and the JDK tells HotSpot's VM of every open as an export
        // (`Module.initExportsAndOpens` / `implAddExportsOrOpens` call
        // `addExports0` / `addExportsToAll0` for opens), so `--add-opens
        // m/p=ALL-UNNAMED` alone, or a declared `opens p`, admits a
        // `CONSTANT_Class` or supertype access to `p`'s public classes there.
        // The declared opens, then the dynamic ones (`--add-opens`,
        // `Module.addOpens`), with the exports' target rule.
        if let Some(desc) = self.modules.get(module_name) {
            if desc
                .opens
                .iter()
                .any(|o| o.package_name == pkg && o.open_to(to_module))
            {
                return true;
            }
        }
        if let Some(extras) = self.extra_opens.get(module_name) {
            if extras.iter().any(|de| {
                de.package == pkg
                    && (de.to_module.is_none() || de.to_module.as_deref() == Some(to_module))
            }) {
                return true;
            }
        }
        false
    }

    /// Is `pkg` of the name-map module `module_name` exported to the module
    /// `layer_module` of a non-boot `ModuleLayer` (`--jdk-only`; recorded by
    /// [`Self::define_layer_module`])? Only an unqualified export (or an open
    /// or automatic module), and a run-time `addExports0` to that module --
    /// which the natives record by the target's NAME, so a same-named module
    /// of another layer is admitted too (permissive only). A qualified
    /// `exports p to m` of the descriptor does not reach it: the JDK resolves
    /// such a target in the module's own configuration
    /// (`Module.initExportsAndOpens` -> `findModule`), which holds no layer
    /// module, and `ALL-UNNAMED` reaches unnamed modules alone
    /// (interpreter round i1 wave 45, lane L5).
    ///
    /// `CRATONVM_MODULE_ACCESS_EXPORT_ONE_QUESTION` (default off; interpreter
    /// round i6 wave 124, lane L5): a COMMAND-LINE edge (`--add-exports
    /// m/p=t`) does not reach a layer module named `t`. Its target is the
    /// boot layer's `t` by identity ([`DynamicExport::command_line`]); HotSpot
    /// answers `false` for a layer module of that name.
    ///
    /// Wave 125 (same switch): `layer_ns` is the asked layer module's
    /// defining loader namespace (the accessor's identity, `(layer_ns,
    /// layer_module)`). A run-time edge then reaches it only when it was
    /// recorded to that very module ([`Self::add_exports_to_layer_module`]);
    /// `None` keeps the name match (see [`layer_edge_admits`]).
    pub fn is_package_exported_to_layer_module(
        &self,
        module_name: &str,
        pkg: &str,
        layer_module: &str,
        layer_ns: Option<u32>,
    ) -> bool {
        if self.is_package_exported_unqualified(module_name, pkg) {
            return true;
        }
        let by_identity = crate::loader_flags().module_access_export_one_question;
        self.extra_exports.get(module_name).is_some_and(|extras| {
            extras
                .iter()
                .any(|de| layer_edge_admits(de, pkg, layer_module, layer_ns, by_identity))
        })
    }

    /// [`Self::is_package_exported_to_layer_module`]'s `opens` twin: is
    /// `pkg` of the name-map module `module_name` open to the layer module
    /// `layer_module`? An unqualified open (or an open or automatic module)
    /// or a run-time `addOpens` to that module's name; never an `opens p to
    /// m` of the descriptor nor `ALL-UNNAMED`.
    pub fn is_package_open_to_layer_module(
        &self,
        module_name: &str,
        pkg: &str,
        layer_module: &str,
        layer_ns: Option<u32>,
    ) -> bool {
        if self.is_package_open_unqualified(module_name, pkg) {
            return true;
        }
        let by_identity = crate::loader_flags().module_access_export_one_question;
        self.extra_opens.get(module_name).is_some_and(|extras| {
            extras
                .iter()
                .any(|de| layer_edge_admits(de, pkg, layer_module, layer_ns, by_identity))
        })
    }

    /// Is `pkg` opened by `module_name` unconditionally?
    ///
    /// This implements `Module.isOpen(String)`.
    pub fn is_package_open_unqualified(&self, module_name: &str, pkg: &str) -> bool {
        if module_name == UNNAMED_MODULE {
            return true; // unnamed module opens everything
        }
        if let Some(desc) = self.modules.get(module_name) {
            // Automatic modules open every package to everyone, matching
            // `opens_package_to`.
            if desc.automatic || desc.is_open {
                return true;
            }
            if desc
                .opens
                .iter()
                .any(|o| o.package_name == pkg && o.to_modules.is_empty())
            {
                return true;
            }
        }
        // Check dynamic opens (unqualified).
        if let Some(extras) = self.extra_opens.get(module_name) {
            if extras
                .iter()
                .any(|de| de.package == pkg && de.to_module.is_none())
            {
                return true;
            }
        }
        false
    }

    /// Is `pkg` opened by `module_name` to `to_module`?
    ///
    /// This implements `Module.isOpen(String, Module)`.
    pub fn is_package_open_to(&self, module_name: &str, pkg: &str, to_module: &str) -> bool {
        if module_name == UNNAMED_MODULE || module_name == to_module {
            return true;
        }
        if let Some(desc) = self.modules.get(module_name) {
            if desc.opens_package_to(pkg, to_module) {
                return true;
            }
        }
        // Check dynamic opens.
        if let Some(extras) = self.extra_opens.get(module_name) {
            if extras.iter().any(|de| {
                de.package == pkg
                    && (de.to_module.is_none() || de.to_module.as_deref() == Some(to_module))
            }) {
                return true;
            }
        }
        false
    }

    /// Return all packages owned by `module_name`.
    pub fn packages_of(&self, module_name: &str) -> Vec<String> {
        self.package_to_module
            .iter()
            .filter(|(_, m)| &***m == module_name)
            .map(|(pkg, _)| pkg.clone())
            .collect()
    }

    /// Return all registered module names.
    pub fn module_names(&self) -> Vec<String> {
        self.modules.keys().cloned().collect()
    }

    /// True when this module was registered by the APPLICATION class-path scan
    /// and nothing re-registered it as explicit.
    ///
    /// `ClassManager::new` stamps `automatic = true` for every
    /// `module-info.class` it finds on the app class path; `vm_init` then
    /// re-registers each genuine `--module-path` module with
    /// `automatic = false`. So within this VM `automatic` is not a JPMS
    /// automatic-module flag in the JDK's sense — it is the record of WHERE the
    /// descriptor came from, and that is the question the module system has to
    /// ask before treating a descriptor as real.
    pub fn is_class_path_only(&self, module_name: &str) -> bool {
        self.modules
            .get(module_name)
            .is_some_and(|desc| desc.automatic)
    }

    /// Return all provider implementation classes for a given service interface.
    ///
    /// Walks every DECLARED module's `provides` declarations looking for
    /// entries whose service matches `service_class` (binary class name,
    /// e.g. `"com/example/MyService"`), and skips every module whose only
    /// source is the application CLASS path.
    ///
    /// # Why the filter is here and not at the consumer
    ///
    /// A modular jar reached through `-cp` is an unnamed-module citizen, and a
    /// real JVM ignores its `module-info` outright — every `provides` clause
    /// included. This is the same rule [`Self::is_class_path_only`] states and
    /// that `populate_boot_layer_modules` already applies through
    /// `NativeContext::module_is_class_path_only`; the two doors onto the
    /// module source were fixed a day apart and only the boot-layer one got the
    /// module-shaped predicate.
    ///
    /// The other door is `service_loader.rs`'s provider-collection arm, whose
    /// guard is `loader_view_is_exhaustive` — a property of the **LOADER**,
    /// where the rule is about the **MODULE**. The system application loader
    /// carries no recorded URL list, so that guard is false and the arm fires.
    /// It cannot be repaired at the consumer either: this function returns
    /// provider NAMES with the owning module discarded, so by the time
    /// `service_loader.rs` sees the strings the module they came from is gone.
    /// Hence the filter lives at the source, where the module is still in hand,
    /// and all three ServiceLoader-shaped consumers inherit it.
    ///
    /// # Why this is not a second, additive method
    ///
    /// There is no legitimate consumer of the unfiltered walk — the JDK rule
    /// admits no exception — and a correct twin sitting beside a wrong original
    /// is the shape where one of ten call sites gets the fix. One function,
    /// correct.
    ///
    /// # Platform and `--module-path` modules are unaffected — MEASURED
    ///
    /// This is the load-bearing safety property, because the platform leans on
    /// the module source far harder than the app does. MEASURED 2026-08-21 on
    /// `cratonvm-r8.exe` with `CRATONVM_DIAG_SERVICELOADER=1`: `descriptors=0
    /// providers=8` for `java.security.Provider`, `descriptors=0 providers=9`
    /// for `java.util.spi.ToolProvider`, `descriptors=0 providers=1` for
    /// `CharsetProvider`. Every one of those arrives through a `provides`
    /// clause and through nothing else, so a filter that caught them would
    /// silently delete the whole JCA provider set and
    /// `ToolProvider.getSystemJavaCompiler()`.
    ///
    /// It does not catch them. `automatic` is stamped in exactly two places
    /// (grep-exhaustive over the workspace) and both answer `false` for a
    /// platform module: `ClassManager::new`'s eager scan hardcodes `automatic =
    /// true` for the application class path ONLY (bootstrap and extension get
    /// `false`), and the lazy path computes `!already_explicit &&
    /// !is_platform_module_name(&desc.name)`, which is `false` for every
    /// `java.*` / `jdk.*` name. Genuine `--module-path` modules are the
    /// `already_explicit` half: `vm_init` re-registers them with
    /// `automatic = false`.
    ///
    /// docs/jdk-only/H24-1-the-module-source-door-and-the-two-modules-a-boot-layer-probe-could-not-see-20260821.md
    pub fn service_providers(&self, service_class: &str) -> Vec<String> {
        let mut providers = Vec::new();
        for desc in self.modules.values() {
            // The rule is about the MODULE, not the loader that asked.
            if desc.automatic {
                continue;
            }
            for p in &desc.provides {
                if p.service == service_class {
                    providers.extend(p.with.iter().cloned());
                }
            }
        }
        providers
    }

    // -----------------------------------------------------------------------
    // Standard access check
    // -----------------------------------------------------------------------

    /// Check whether code in module `accessor_module` (accessing package
    /// `target_pkg` of module `target_module`) is permitted by JPMS rules.
    ///
    /// Returns `Ok(())` if access is allowed, `Err(reason)` otherwise.
    ///
    /// # The unnamed-accessor arm is a deliberate escape hatch, not an oversight
    ///
    /// JPMS (JEP 261/403) says classpath code may reach a named module's
    /// package only if that package is `exports`ed — unqualified, or qualified
    /// to `ALL-UNNAMED`. The `accessor_module == UNNAMED_MODULE` arm below is
    /// deliberately *more* permissive than that. Do not delete it on the
    /// strength of the spec alone; audit the callers first. As of 2026-08-07
    /// they are:
    ///
    /// | caller | live? | what tightening would refuse |
    /// |---|---|---|
    /// | `access_control::check_module_access(&Class, &Class, &ModuleRegistry)` | the only wrapper | — |
    /// | ↳ `check_class_access_with_modules` | **no production call site** | — |
    /// | ↳ `check_field_access_with_modules` | **no production call site** | — |
    /// | ↳ `check_method_access_with_modules` | **no production call site** | — |
    /// | ↳ `check_module_access_by_id` | **live** | see below |
    /// | ↳↳ `runtime/resolve/mod.rs` (`AccessPolicy::ModuleOnly`, the one implementation behind BOTH field and method resolution) | **live** | every classpath `invoke*` / `getfield` / `putfield` naming a non-exported package |
    /// | ↳↳ `vm/vm_init.rs` (5 sites) | self-test/probe only | — |
    ///
    /// The two live rows are bytecode resolution, and under `--real-jdk` the
    /// registry carries java.base's REAL descriptor (parsed from the jimage's
    /// `module-info.class`; `CRATONVM_BOOT_MODULE_REGISTRY`, default on), which
    /// exports `java.lang`/`java.util`/… but NOT `jdk.internal.*` / `sun.nio.*`.
    /// Tightening this arm therefore turns every classpath reference to
    /// `jdk.internal.misc.Unsafe` & friends into an `IllegalAccessError` at
    /// resolution time — a broad break, on the hot path, that no measured
    /// vector asks for. The arm stays.
    ///
    /// # This function is NOT the reflection gate
    ///
    /// `RJdkModule.java:182` (a public no-arg constructor on a public class in
    /// the one package the module neither exports nor opens must be refused
    /// with `IllegalAccessException`) does **not** route through here.
    /// `Constructor.newInstance` is served by
    /// `native-builtins/src/lang_class.rs::native_constructor_new_instance`,
    /// and the exports question it asks reaches this registry through
    /// [`Self::is_package_exported_to`] (via
    /// `NativeContext::reflective_export_to_accessor`), which has no
    /// unnamed-*accessor* arm and already answers correctly. See
    /// `docs/feature-designs/class-resolution-and-caching.md`.
    ///
    /// So the asymmetry with [`Self::check_deep_reflection_access`] (which does
    /// refuse an unnamed accessor) is not an inconsistency to resolve: the two
    /// serve different subsystems. Deep reflection is caller-sensitive and
    /// JEP-403-governed; this one is bytecode linkage, where the hatch is what
    /// keeps mixed classpath/module-path applications running.
    pub fn check_module_access(
        &self,
        accessor_module: &str,
        target_module: &str,
        target_pkg: &str,
    ) -> Result<(), String> {
        // Same module — always allowed.
        if accessor_module == target_module {
            return Ok(());
        }
        // Unnamed module is always allowed (classpath compat).
        if accessor_module == UNNAMED_MODULE || target_module == UNNAMED_MODULE {
            return Ok(());
        }

        // 1. Readability check.
        if !self.reads(accessor_module, target_module) {
            return Err(format!(
                "module {accessor_module} does not read module {target_module}"
            ));
        }

        // 2. Exports check.
        // If neither module is registered, use open-world assumption → allow.
        if self.modules.get(accessor_module).is_none() && self.modules.get(target_module).is_none()
        {
            return Ok(());
        }
        if let Some(target_desc) = self.modules.get(target_module) {
            if !target_desc.exports_package_to(target_pkg, accessor_module) {
                // Check dynamic exports before rejecting.
                let has_dynamic = self.extra_exports.get(target_module).is_some_and(|extras| {
                    extras.iter().any(|de| {
                        de.package == target_pkg
                            && (de.to_module.is_none()
                                || de.to_module.as_deref() == Some(accessor_module))
                    })
                });
                if !has_dynamic {
                    return Err(format!(
                        "module {target_module} does not export package {target_pkg} to {accessor_module}"
                    ));
                }
            }
        }
        // If we have no descriptor for target_module, allow (unknown module — open world).

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Descriptor construction from reader types
// ---------------------------------------------------------------------------

/// Build a [`ModuleDescriptor`] from a parsed `Attribute::Module`.
///
/// Returns `None` if the attribute is not a Module attribute.
pub fn descriptor_from_module_attribute(
    attr: &cratonvm_reader::attribute::Attribute,
    cp: &cratonvm_reader::constant_pool::ConstantPool,
) -> Option<ModuleDescriptor> {
    use cratonvm_reader::attribute::Attribute;
    use cratonvm_reader::constant_pool::ConstantPoolEntry;

    let (
        name_index,
        flags,
        version_index,
        requires_raw,
        exports_raw,
        opens_raw,
        uses_raw,
        provides_raw,
    ) = match attr {
        Attribute::Module(m) => (
            &m.name_index,
            &m.flags,
            &m.version_index,
            &m.requires,
            &m.exports,
            &m.opens,
            &m.uses,
            &m.provides,
        ),
        _ => return None,
    };

    // Helper: resolve a CONSTANT_Module or CONSTANT_Package entry's name.
    // The Module attribute references modules and packages through
    // CONSTANT_Module_info / CONSTANT_Package_info indirections (JVMS §4.7.25),
    // NOT direct Utf8 — so every index here must go through this resolver,
    // including `module_name_index` itself.
    let resolve_name = |idx: u16| -> Option<String> {
        match cp.get(idx) {
            Some(ConstantPoolEntry::Module { name_index }) => {
                cp.get_utf8(*name_index).map(|s| s.to_string())
            }
            Some(ConstantPoolEntry::Package { name_index }) => {
                cp.get_utf8(*name_index).map(|s| s.to_string())
            }
            // Some implementations store the name directly as Utf8
            Some(ConstantPoolEntry::Utf8(s)) => Some(s.to_string()),
            _ => None,
        }
    };

    // `module_name_index` points to a CONSTANT_Module_info, NOT a Utf8 — the
    // previous `cp.get_utf8(*name_index)` returned None for every real
    // module-info, so the eager boot-module scan registered ZERO modules (the
    // whole module graph fell back to a single synthetic java.base entry, and
    // module-declared service providers like jdk.compiler's
    // `provides javax.tools.JavaCompiler` were invisible to ServiceLoader).
    let name = resolve_name(*name_index)?;

    let version = if *version_index != 0 {
        cp.get_utf8(*version_index).map(|s| s.to_string())
    } else {
        None
    };

    let is_open = (flags & ACC_MODULE_OPEN) != 0;

    // Every directive's flag word and the `requires` version index are carried
    // through here. They used to be dropped on the floor, which is what left
    // `Requires.modifiers()`, `Exports.modifiers()`, `Opens.modifiers()` and
    // `Requires.{compiledVersion,rawCompiledVersion}()` with no data source
    // anywhere in the VM — the Java mirror could only fabricate an empty answer
    // because nothing upstream had kept the bits to answer with.
    let requires: Vec<ModuleRequiresEntry> = requires_raw
        .iter()
        .filter_map(|r| {
            let module_name = resolve_name(r.requires_index)?;
            Some(ModuleRequiresEntry {
                module_name,
                is_transitive: (r.requires_flags & ACC_REQUIRES_TRANSITIVE) != 0,
                is_static: (r.requires_flags & ACC_REQUIRES_STATIC) != 0,
                is_synthetic: (r.requires_flags & ACC_SYNTHETIC) != 0,
                is_mandated: (r.requires_flags & ACC_MANDATED) != 0,
                // `requires_version_index` is a plain Utf8 index (JVMS §4.7.25),
                // NOT one of the CONSTANT_Module/Package indirections the
                // `resolve_name` helper above exists for, so it is read
                // directly. 0 means "no version recorded".
                compiled_version: if r.requires_version_index != 0 {
                    cp.get_utf8(r.requires_version_index).map(|s| s.to_string())
                } else {
                    None
                },
            })
        })
        .collect();

    let exports: Vec<ModuleExportsEntry> = exports_raw
        .iter()
        .filter_map(|e| {
            let package_name = resolve_name(e.exports_index)?;
            let to_modules: Vec<String> = e
                .exports_to
                .iter()
                .filter_map(|&idx| resolve_name(idx))
                .collect();
            Some(ModuleExportsEntry {
                package_name,
                to_modules,
                is_synthetic: (e.exports_flags & ACC_SYNTHETIC) != 0,
                is_mandated: (e.exports_flags & ACC_MANDATED) != 0,
            })
        })
        .collect();

    let opens: Vec<ModuleOpensEntry> = opens_raw
        .iter()
        .filter_map(|o| {
            let package_name = resolve_name(o.opens_index)?;
            let to_modules: Vec<String> = o
                .opens_to
                .iter()
                .filter_map(|&idx| resolve_name(idx))
                .collect();
            Some(ModuleOpensEntry {
                package_name,
                to_modules,
                is_synthetic: (o.opens_flags & ACC_SYNTHETIC) != 0,
                is_mandated: (o.opens_flags & ACC_MANDATED) != 0,
            })
        })
        .collect();

    // `uses` entries reference CONSTANT_Class
    let uses: Vec<String> = uses_raw
        .iter()
        .filter_map(|&idx| cp.get_class_name(idx).map(|s| s.to_string()))
        .collect();

    let provides: Vec<ModuleProvidesEntry> = provides_raw
        .iter()
        .filter_map(|p| {
            let service = cp.get_class_name(p.provides_index).map(|s| s.to_string())?;
            let with: Vec<String> = p
                .provides_with
                .iter()
                .filter_map(|&idx| cp.get_class_name(idx).map(|s| s.to_string()))
                .collect();
            Some(ModuleProvidesEntry { service, with })
        })
        .collect();

    Some(ModuleDescriptor {
        name,
        version,
        is_open,
        requires,
        exports,
        opens,
        uses,
        provides,
        // Source-dependent; the caller (`try_register_module_info`) sets this to
        // true for application-classpath jars.
        automatic: false,
        // `ModuleMainClass` is a sibling attribute of `Module`, not visible
        // here; `parse_module_info` fills it in once it has already.
        main_class: None,
    })
}

/// Extract the list of packages from a `ModulePackages` attribute.
///
/// Returns the packages as slash-format strings (e.g. `"java/lang"`).
pub fn packages_from_module_packages_attribute(
    attr: &cratonvm_reader::attribute::Attribute,
    cp: &cratonvm_reader::constant_pool::ConstantPool,
) -> Option<Vec<String>> {
    use cratonvm_reader::attribute::Attribute;
    use cratonvm_reader::constant_pool::ConstantPoolEntry;

    if let Attribute::ModulePackages { packages } = attr {
        let names: Vec<String> = packages
            .iter()
            .filter_map(|&idx| {
                // CONSTANT_Package { name_index } → Utf8
                match cp.get(idx) {
                    Some(ConstantPoolEntry::Package { name_index }) => {
                        cp.get_utf8(*name_index).map(|s| s.to_string())
                    }
                    Some(ConstantPoolEntry::Utf8(s)) => Some(s.to_string()),
                    _ => None,
                }
            })
            .collect();
        Some(names)
    } else {
        None
    }
}

/// Extract the package name from a binary class name.
///
/// `"java/lang/Object"` → `"java/lang"`, `"Foo"` → `""`.
pub fn package_of(class_name: &str) -> &str {
    match class_name.rfind('/') {
        Some(pos) => &class_name[..pos],
        None => "",
    }
}

/// True if `name` is a genuine JDK/platform module name (`java.*`, `jdk.*`,
/// `javafx.*`, `oracle.*`). Everything else is an application/library module that
/// — under CratonVM's class-path-only loading model — should get automatic-module
/// access semantics (read/export/open all) rather than strict JPMS encapsulation.
pub fn is_platform_module_name(name: &str) -> bool {
    name == "java.base"
        || name.starts_with("java.")
        || name.starts_with("jdk.")
        || name.starts_with("javafx.")
        || name.starts_with("oracle.")
}

// ---------------------------------------------------------------------------
// `--module-path` / `--add-modules` resolution
// ---------------------------------------------------------------------------
//
// Background — why this exists at all.
//
// `--module-path` and `--add-modules` were parsed by the launcher into
// `VmConfig::module_path` / `VmConfig::add_modules` and then read by NOBODY:
// `grep -rn '\.module_path\b|\.add_modules\b'` over the whole workspace found
// exactly two hits, both *writes* in `vm-cli/src/main.rs`. Same disease as the
// already-recorded `--add-opens was parsed then ignored`. The consequence is
// that a launch of the shape
//
//     --module-path <dir> --add-modules <name> -cp <dir> Main
//
// resolved no module at all: the module's classes were not on any search path,
// and `ModuleLayer.boot().findModule(<name>)` had no module to find. (It
// answered `Optional.of(...)` regardless, because the synthetic
// `ModuleLayer.findModule` native fabricates a Module for any syntactically
// valid name — see `native-builtins/src/jboss_jdkspecific.rs`.)
//
// This module does the *resolution* half: turn the raw `--module-path` entries
// into the set of named modules `--add-modules` actually selects, together with
// the filesystem root each one lives at and the package set it declares. The
// caller (`vm_init`) puts those roots on the application search path — the real
// JDK also defines module-path classes to the application loader — and
// registers the descriptors here with `automatic == false`, i.e. as EXPLICIT
// modules whose `exports`/`opens` are enforced. That last part is the whole
// point of a module path: a jar on the *class* path gets automatic-module
// semantics (read/export/open everything, see `ModuleDescriptor::automatic`),
// and a module resolved from a *module* path does not.

/// One named module found on the `--module-path`.
#[derive(Debug, Clone)]
pub struct ModulePathModule {
    /// Filesystem root to add to the class search path: an exploded module
    /// directory, or a modular JAR.
    pub root: String,
    /// The parsed `module-info.class` descriptor. `automatic` is always
    /// `false` — a module resolved from a real module path is explicit.
    pub descriptor: ModuleDescriptor,
    /// Packages the module contains, slash format (`"com/example/svc"`).
    pub packages: Vec<String>,
    /// A JPMS AUTOMATIC module: a JAR with no `module-info.class`, whose
    /// name and version the JDK's `ModulePath.deriveModuleDescriptor`
    /// derives ([`automatic_jar_module`]). Its descriptor is registered open
    /// (every package exported and open to every module) and NOT
    /// `automatic` in this registry's sense (that flag marks a class-path
    /// jar, whose classes are unnamed); [`ModuleRegistry::note_module_path_module`]
    /// records that it reads every module. Interpreter round i4 wave 99,
    /// lane L5.
    pub automatic: bool,
}

/// `--add-modules ALL-MODULE-PATH` (JEP 261): resolve every observable module
/// on the module path, whether or not anything requires it.
pub const ALL_MODULE_PATH: &str = "ALL-MODULE-PATH";

/// Parse `module-info.class` bytes into a descriptor plus whatever packages
/// its `ModulePackages` attribute declares.
///
/// The package list is frequently EMPTY and that is not an error: `javac` does
/// not emit `ModulePackages` for an exploded compilation. Verified with
/// `javap -v regression-suite/build-modules/cratonvm.jdkonly.svc/module-info.class`
/// — the attribute list is `SourceFile` + `Module`, nothing else. Only `jar` /
/// `jlink` add it. Callers must fall back to scanning the tree
/// ([`exploded_packages`] / the JAR entry list), exactly as the real JDK's
/// `jdk.internal.module.ModulePath` does.
pub fn parse_module_info(bytes: &[u8]) -> Option<(ModuleDescriptor, Vec<String>)> {
    let mut class_file = cratonvm_reader::read_class(bytes).ok()?;
    cratonvm_reader::attribute::force_decode_all(
        &mut class_file.attributes,
        &class_file.constant_pool,
    )
    .ok()?;
    let mut desc = class_file.attributes.iter().find_map(|a| {
        a.as_decoded()
            .and_then(|d| descriptor_from_module_attribute(d, &class_file.constant_pool))
    })?;
    desc.main_class = class_file
        .attributes
        .iter()
        .find_map(|a| match a.as_decoded() {
            Some(cratonvm_reader::attribute::Attribute::ModuleMainClass { main_class_index }) => {
                class_file
                    .constant_pool
                    .get_class_name(*main_class_index)
                    .map(|s| s.to_string())
            }
            _ => None,
        });
    let packages = class_file
        .attributes
        .iter()
        .find_map(|a| {
            a.as_decoded()
                .and_then(|d| packages_from_module_packages_attribute(d, &class_file.constant_pool))
        })
        .unwrap_or_default();
    Some((desc, packages))
}

/// The module names a `module-info.class`'s `ModuleHashes` attribute records
/// (JDK `ModuleInfo.readModuleHashesAttribute`: `u2 algorithm`, `u2 count`,
/// then per module `u2 CONSTANT_Module`, `u2 length`, `length` hash bytes).
/// `None` when the attribute is absent or malformed. The attributes must be
/// decoded already (`force_decode_all`).
pub fn module_hashes_names(
    attributes: &[cratonvm_reader::attribute::LazyAttribute],
    cp: &cratonvm_reader::constant_pool::ConstantPool,
) -> Option<Vec<String>> {
    use cratonvm_reader::attribute::Attribute;
    use cratonvm_reader::constant_pool::ConstantPoolEntry;
    let data = attributes.iter().find_map(|a| match a.as_decoded() {
        Some(Attribute::Unknown { name, data }) if &**name == "ModuleHashes" => Some(data),
        _ => None,
    })?;
    let bytes: &[u8] = data;
    let u2 = |at: usize| -> Option<usize> {
        let hi = *bytes.get(at)?;
        let lo = *bytes.get(at + 1)?;
        Some(((hi as usize) << 8) | lo as usize)
    };
    let count = u2(2)?;
    let mut at = 4;
    let mut names = Vec::with_capacity(count);
    for _ in 0..count {
        let idx = u2(at)?;
        let len = u2(at + 2)?;
        at += 4 + len;
        if at > bytes.len() {
            return None;
        }
        let name = match cp.get(idx as u16) {
            Some(ConstantPoolEntry::Module { name_index }) => cp.get_utf8(*name_index)?.to_string(),
            _ => return None,
        };
        names.push(name);
    }
    Some(names)
}

/// The module names `java.base` records hashes for, read from a JLINKED
/// image's generated `jdk.internal.module.SystemModules$all` class (SBR3
/// `refmisc`). `jlink` moves the `ModuleHashes` attribute of `java.base`'s
/// `module-info.class` into that class: `moduleHashes()` builds one
/// `ModuleHashes` per module, and the FIRST one built belongs to module index 0,
/// `java.base`. Its names are the `ldc` string constants between the
/// `"SHA-256"` algorithm and the first `aastore` (the store of the built hashes
/// into the result array; the hash bytes themselves are `bastore`s). `None`
/// when the class or method is absent, or its shape is not that one.
pub fn system_modules_java_base_hash_names(bytes: &[u8]) -> Option<Vec<String>> {
    use cratonvm_reader::constant_pool::ConstantPoolEntry;
    use cratonvm_reader::instruction::Instruction;
    let mut class_file = cratonvm_reader::read_class(bytes).ok()?;
    let method = class_file.methods.iter_mut().find(|m| {
        &*m.name == "moduleHashes" && &*m.descriptor == "()[Ljdk/internal/module/ModuleHashes;"
    })?;
    cratonvm_reader::attribute::force_decode_all(&mut method.attributes, &class_file.constant_pool)
        .ok()?;
    let code = method.code()?;
    let cp = &class_file.constant_pool;
    let bytes: &[u8] = &code.code;
    let mut pc = 0usize;
    let mut names: Vec<String> = Vec::new();
    while pc < bytes.len() {
        let (instr, next) = Instruction::decode(bytes, pc).ok()?;
        pc = next;
        let idx = match instr {
            Instruction::Ldc(i) => i as u16,
            Instruction::LdcW(i) => i,
            Instruction::Aastore => break,
            _ => continue,
        };
        if let Some(ConstantPoolEntry::StringReference { string_index }) = cp.get(idx) {
            let s = cp.get_utf8(*string_index)?;
            if s != "SHA-256" {
                names.push(s.to_string());
            }
        }
    }
    (!names.is_empty()).then_some(names)
}

/// `jdk.internal.module.Checks.RESERVED`: keywords, boolean and null
/// literals, and `_`, none of which is a Java identifier.
const RESERVED_IDENTIFIERS: [&str; 54] = [
    "abstract", "assert", "boolean", "break", "byte", "case", "catch", "char", "class", "const",
    "continue", "default", "do", "double", "else", "enum", "extends", "final", "finally", "float",
    "for", "goto", "if", "implements", "import", "instanceof", "int", "interface", "long",
    "native", "new", "package", "private", "protected", "public", "return", "short", "static",
    "strictfp", "super", "switch", "synchronized", "this", "throw", "throws", "transient", "try",
    "void", "volatile", "while", "true", "false", "null", "_",
];

/// `Checks.requireModuleName`'s refusal text for `name`, or `None` when it is
/// a legal module name (every dot-separated part a Java identifier).
fn module_name_refusal(name: &str) -> Option<String> {
    name.split('.')
        .find(|id| !is_package_segment(id))
        .map(|id| format!("{name}: Invalid module name: '{id}' is not a Java identifier"))
}

/// A JAR entry name as `JarFile.versionedStream()` presents it
/// (`JarFile.getBasename`): in a multi-release jar a
/// `META-INF/versions/<n>/<rest>` entry is `<rest>` when `n` is a canonical
/// decimal in `8..=feature` (and `feature > 8`), and dropped otherwise (also the version directories
/// themselves); any other name is itself. Not multi-release: every name as is.
fn versioned_entry_name(name: &str, multi_release: bool, feature: u32) -> Option<&str> {
    if !multi_release {
        return Some(name);
    }
    let Some(rest) = name.strip_prefix("META-INF/versions/") else {
        return Some(name);
    };
    let (version, base) = rest.split_once('/')?;
    if base.is_empty() {
        return None;
    }
    // `versionedStream` maps the name to its base (`getBasename`: any
    // `Integer.parseInt` value up to `feature`) and then keeps it only when
    // `getJarEntry(base)` finds an entry. That lookup (`getVersionedEntry`)
    // probes only the canonical versions `ZipFile.Source.getMetaVersion`
    // registers (digits, no sign, no leading zero) from `feature` down to 8,
    // never for a `META-INF/` name, then the base entry. So a versioned entry
    // contributes its base name by itself exactly when it is one of those
    // probes; any other one (`versions/5/`, `versions/+9/`, `versions/09/`)
    // is kept only if the base entry exists, which counts on its own (wave
    // 104, lane L5: these were mapped to their base before, a package HotSpot
    // does not list).
    let canonical = !version.is_empty()
        && !version.starts_with('0')
        && version.bytes().all(|b| b.is_ascii_digit());
    let parsed = version.parse::<u32>().ok().filter(|_| canonical)?;
    const BASE_VERSION_FEATURE: u32 = 8;
    (feature > BASE_VERSION_FEATURE
        && (BASE_VERSION_FEATURE..=feature).contains(&parsed)
        && !base.starts_with("META-INF/"))
    .then_some(base)
}

/// Is the JAR manifest `manifest` a `Multi-Release: true` one (main section,
/// value compared ignoring case, as `JarFile.isMultiRelease`)?
fn manifest_is_multi_release(manifest: &[u8]) -> bool {
    main_manifest_attribute(manifest, "Multi-Release")
        .is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

/// The feature version module-path JARs are read at when the caller does not
/// know the booted library's (`ModulePath`'s `releaseVersion`,
/// `JarFile.runtimeVersion()`): this VM's newest. The VM passes the booted
/// library's version, capped by `-Djdk.util.jar.version`, through the `_at`
/// entry points ([`scan_module_path_at`], [`resolve_module_path_at`],
/// [`module_path_find_error_at`]; interpreter round i5 wave 104, lane L5, the
/// `i100-L5-multi-release-jars-read-at-version-25-whatever-jdk-boots` /
/// i98-orch residue).
pub const MODULE_PATH_JAR_FEATURE: u32 = 25;

/// Is `segment` usable as one dot-separated component of a package name?
///
/// The real JDK drops directories that cannot be package components rather
/// than inventing an illegal package name — this is what keeps `META-INF`
/// (illegal: contains `-`) out of a module's package set.
fn is_package_segment(segment: &str) -> bool {
    // `Checks.isJavaIdentifier`: keywords, the literals and `_` are not
    // identifiers (wave 103).
    if RESERVED_IDENTIFIERS.contains(&segment) {
        return false;
    }
    let mut chars = segment.chars();
    match chars.next() {
        Some(c) if c == '_' || c == '$' || c.is_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c == '$' || c.is_alphanumeric())
}

/// Walk an exploded module directory and collect the packages it contains,
/// in slash format.
///
/// Counts a directory as a package when it holds at least one regular file —
/// not only `.class` files. That matches `ModulePath::explodedPackages`, and
/// it matters here: a package that is opened purely to expose a resource still
/// has to appear in `ModuleDescriptor.packages()`.
///
/// Symlinks are not followed (`file_type()` reports `is_symlink`, so they are
/// neither descended into nor counted), which bounds the walk on a cyclic
/// tree.
fn exploded_packages(root: &std::path::Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut stack: Vec<(std::path::PathBuf, String)> = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, pkg)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut has_file = false;
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !is_package_segment(&name) {
                    continue;
                }
                let child = if pkg.is_empty() {
                    name
                } else {
                    format!("{pkg}/{name}")
                };
                stack.push((entry.path(), child));
            } else if file_type.is_file() {
                has_file = true;
            }
        }
        if has_file && !pkg.is_empty() && !out.contains(&pkg) {
            out.push(pkg);
        }
    }
    out.sort();
    out
}

/// Read `module-info.class` out of a modular JAR, together with the package
/// set derived from its entry names.
fn modular_jar_module(
    path: &std::path::Path,
    feature: u32,
) -> Option<(ModuleDescriptor, Vec<String>)> {
    let file = std::fs::File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;

    // Collect names first: `by_name` needs `&mut archive`, so the borrow of
    // `file_names()` must be finished before the read below.
    let names: Vec<String> = archive.file_names().map(|n| n.to_string()).collect();

    let mut manifest = Vec::new();
    if let Ok(mut entry) = archive.by_name("META-INF/MANIFEST.MF") {
        use std::io::Read as _;
        if entry.read_to_end(&mut manifest).is_err() {
            manifest.clear();
        }
    }
    let multi_release = manifest_is_multi_release(&manifest);

    // `ModulePath.readJar`: `jf.getJarEntry("module-info.class")`, which in a
    // multi-release jar is the versioned lookup (`META-INF/versions/<n>/` for
    // `n` from `feature` down to 8, then the base entry). Wave 104, lane L5:
    // only the base entry was read, so a jar whose descriptor lives only under
    // a version directory was an automatic module here and an explicit one on
    // HotSpot.
    let info_name = multi_release
        .then(|| {
            names
                .iter()
                .filter(|n| n.ends_with("/module-info.class"))
                .filter_map(|n| {
                    let base = versioned_entry_name(n, true, feature)?;
                    (base == "module-info.class").then(|| {
                        let version = n["META-INF/versions/".len()..]
                            .split('/')
                            .next()
                            .and_then(|v| v.parse::<u32>().ok())
                            .unwrap_or(0);
                        (version, n.as_str())
                    })
                })
                .max_by_key(|(version, _)| *version)
                .map(|(_, n)| n.to_string())
        })
        .flatten()
        .unwrap_or_else(|| "module-info.class".to_string());

    let mut bytes = Vec::new();
    {
        use std::io::Read as _;
        let mut entry = archive.by_name(&info_name).ok()?;
        entry.read_to_end(&mut bytes).ok()?;
    }
    let (descriptor, declared) = parse_module_info(&bytes)?;

    let packages = if declared.is_empty() {
        // `jarPackages`: the names of `versionedStream()` (wave 103: a
        // multi-release jar's versioned entries count under their base name).
        let mut pkgs: Vec<String> = Vec::new();
        for name in &names {
            if name.ends_with('/') {
                continue;
            }
            let Some(name) = versioned_entry_name(name, multi_release, feature) else {
                continue;
            };
            let Some(pos) = name.rfind('/') else {
                continue;
            };
            let pkg = &name[..pos];
            if pkg.split('/').all(is_package_segment) && !pkgs.iter().any(|p| p == pkg) {
                pkgs.push(pkg.to_string());
            }
        }
        pkgs.sort();
        pkgs
    } else {
        declared
    };
    Some((descriptor, packages))
}

/// Try to read `path` as a single module root (exploded directory or modular
/// JAR). Returns `None` when it carries no `module-info.class`.
fn module_at(path: &std::path::Path, feature: u32) -> Option<ModulePathModule> {
    let root = path.to_string_lossy().into_owned();
    if path.is_dir() {
        let info = path.join("module-info.class");
        let bytes = std::fs::read(&info).ok()?;
        let (mut descriptor, declared) = parse_module_info(&bytes)?;
        descriptor.automatic = false;
        let packages = if declared.is_empty() {
            exploded_packages(path)
        } else {
            declared
        };
        return Some(ModulePathModule {
            root,
            descriptor,
            packages,
            automatic: false,
        });
    }
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if ext.eq_ignore_ascii_case("jar") {
        let Some((mut descriptor, packages)) = modular_jar_module(path, feature) else {
            // No `module-info.class`: an automatic module (wave 99, lane L5).
            return automatic_jar_module(path, feature);
        };
        descriptor.automatic = false;
        return Some(ModulePathModule {
            root,
            descriptor,
            packages,
            automatic: false,
        });
    }
    None
}

/// The automatic module of a module-path JAR with no `module-info.class`,
/// as the JDK's `ModulePath.deriveModuleDescriptor` derives it: the name is
/// the main section's `Automatic-Module-Name`, else the file name (`.jar`
/// dropped) up to the first `-<digits>` followed by `.` or the end, cleaned
/// (`ModulePath.cleanModuleName`: every non-alphanumeric becomes `.`, runs of
/// `.` collapse, leading and trailing `.` go); the version is the tail after
/// that `-`, whichever way the name was found (`lib-3.4.jar` -> `lib`,
/// `3.4`). Its packages are those of its `.class` entries whose every
/// directory segment can be a package name (`META-INF` cannot); resource-only
/// directories are none (wave 100); a multi-release jar's
/// `META-INF/versions/<N>/` classes count under their base names, as the JDK's
/// `versionedStream` presents them (wave 103). `None` when the JAR cannot be
/// read, or the JDK would refuse it (an illegal name, a top-level class): the
/// launcher reports that refusal first as the boot layer's `FindException`
/// ([`module_path_find_error`], wave 103), so a VM reaching this scan never
/// sees it.
///
/// Since wave 103 also derived: the `provides` of `META-INF/services`, the
/// `Main-Class` main class, and the version only when `Version.parse`
/// accepts the tail ([`module_version_parses`]). Interpreter round i4 wave 99, lane L5
/// (`i98-orch-module-clauses-omit-version-of-named-application-modules`):
/// a module path's automatic module was not observable at all, so a module
/// that `requires` one did not launch.
fn automatic_jar_module(path: &std::path::Path, feature: u32) -> Option<ModulePathModule> {
    automatic_jar_module_checked(path, feature).ok().flatten()
}

/// [`automatic_jar_module`], with the JDK's refusals: `Err` is the text of
/// the boot layer's `java.lang.module.FindException` (`ModulePath.readJar`:
/// "Unable to derive module descriptor for <jar>" and its `Caused by:` line),
/// for an illegal module name (derived or `Automatic-Module-Name`) and for a
/// class file in the top-level directory. `Ok(None)`: the JAR cannot be read
/// (HotSpot: "Error reading <jar>", not modelled). Interpreter round i5 wave
/// 103, lane L5 (`i98-orch-module-clauses-omit-version-...` residue).
fn automatic_jar_module_checked(
    path: &std::path::Path,
    feature: u32,
) -> Result<Option<ModulePathModule>, String> {
    let Ok(file) = std::fs::File::open(path) else {
        return Ok(None);
    };
    let Ok(mut archive) = zip::ZipArchive::new(file) else {
        return Ok(None);
    };
    let names: Vec<String> = archive.file_names().map(|n| n.to_string()).collect();
    let mut manifest = Vec::new();
    if let Ok(mut entry) = archive.by_name("META-INF/MANIFEST.MF") {
        use std::io::Read as _;
        if entry.read_to_end(&mut manifest).is_err() {
            manifest.clear();
        }
    }
    let Some(file_name) = path.file_name().and_then(|f| f.to_str()) else {
        return Ok(None);
    };
    let Some(stem) = file_name
        .len()
        .checked_sub(4)
        .and_then(|len| file_name.get(..len))
    else {
        return Ok(None);
    };
    let refused = |cause: String| {
        format!(
            "Unable to derive module descriptor for {}\nCaused by: {cause}",
            java_path_text(path)
        )
    };
    let (derived, version) = automatic_module_name_and_version(stem);
    let name = match main_manifest_attribute(&manifest, "Automatic-Module-Name") {
        Some(declared) => {
            if let Some(why) = module_name_refusal(&declared) {
                return Err(refused(format!(
                    "java.lang.module.FindException: Automatic-Module-Name: {why}"
                )));
            }
            declared
        }
        None => {
            if let Some(why) = module_name_refusal(&derived) {
                return Err(refused(format!("java.lang.IllegalArgumentException: {why}")));
            }
            derived
        }
    };
    // The packages of an AUTOMATIC module are the packages of its CLASS
    // files only (`deriveModuleDescriptor`: `classFiles.stream().map(
    // this::toPackageName)`): a directory holding only resources
    // (`templates/`, `config/`) is no package there, unlike an explicit
    // module's (interpreter round i5 wave 100, lane L5). The names are
    // `versionedStream()`'s: a multi-release jar's versioned class counts
    // under its base name (wave 103).
    let multi_release = manifest_is_multi_release(&manifest);
    let mut packages: Vec<String> = Vec::new();
    let mut top_level: Vec<&str> = Vec::new();
    for entry in &names {
        if entry.ends_with('/') {
            continue;
        }
        let Some(entry) = versioned_entry_name(entry, multi_release, feature)
        else {
            continue;
        };
        if !entry.ends_with(".class") || entry.starts_with("META-INF/services/") {
            continue;
        }
        let Some(pos) = entry.rfind('/') else {
            // `toPackageName`: a class file in the top-level directory
            // (not `module-info.class`) is an invalid descriptor.
            if entry != "module-info.class" {
                top_level.push(entry);
            }
            continue;
        };
        let pkg = &entry[..pos];
        if pkg.split('/').all(is_package_segment) && !packages.iter().any(|p| p == pkg) {
            packages.push(pkg.to_string());
        }
    }
    if let Some(first) = top_level.iter().min() {
        return Err(refused(format!(
            "java.lang.module.InvalidModuleDescriptorException: {first} found in top-level \
             directory (unnamed package not allowed in module)"
        )));
    }
    // `META-INF/services/<S>` files are `provides S with ...` (each provider's
    // package must be one of the JAR's, its name a legal class name), and a
    // `Main-Class` in one of its packages is the main class
    // (`deriveModuleDescriptor` steps 4 and 5; wave 103: neither was derived,
    // so `ServiceLoader` over the boot layer missed an automatic module's
    // providers and `-m <automatic module>` had no main class).
    let mut service_names: Vec<&str> = Vec::new();
    for entry in &names {
        if entry.ends_with('/') || entry.ends_with(".class") {
            continue;
        }
        let Some(entry) = versioned_entry_name(entry, multi_release, feature)
        else {
            continue;
        };
        let Some(service) = entry.strip_prefix("META-INF/services/") else {
            continue;
        };
        if !service.is_empty()
            && !service.contains('/')
            && service.split('.').all(is_package_segment)
            && !service_names.contains(&service)
        {
            service_names.push(service);
        }
    }
    service_names.sort();
    let mut provides: Vec<ModuleProvidesEntry> = Vec::new();
    for service in service_names {
        let mut text = Vec::new();
        if let Ok(mut entry) = archive.by_name(&format!("META-INF/services/{service}")) {
            use std::io::Read as _;
            if entry.read_to_end(&mut text).is_err() {
                text.clear();
            }
        }
        let text = String::from_utf8_lossy(&text);
        let mut with: Vec<String> = Vec::new();
        // `BufferedReader.readLine` lines, `#` comments cut, `String.trim`.
        for line in text.split(['\n', '\r']) {
            let line = line.split('#').next().unwrap_or("");
            let provider = line.trim_matches(|c: char| c <= ' ');
            if provider.is_empty() {
                continue;
            }
            let package = provider.rsplit_once('.').map_or("", |(p, _)| p).replace('.', "/");
            if !packages.contains(&package) {
                return Err(refused(format!(
                    "java.lang.module.InvalidModuleDescriptorException: Provider class \
                     {provider} not in JAR file {file_name}"
                )));
            }
            with.push(provider.to_string());
        }
        if with.is_empty() {
            continue;
        }
        // `Builder.provides` (strict): every provider a legal class name.
        for provider in &with {
            if let Some(id) = provider.split('.').find(|id| !is_package_segment(id)) {
                return Err(refused(format!(
                    "java.lang.IllegalArgumentException: {provider}: Invalid service provider \
                     name: '{id}' is not a Java identifier"
                )));
            }
        }
        provides.push(ModuleProvidesEntry {
            service: service.replace('.', "/"),
            with: with.iter().map(|c| c.replace('.', "/")).collect(),
        });
    }
    let main_class = main_manifest_attribute(&manifest, "Main-Class")
        .map(|c| c.replace('/', "."))
        .filter(|c| c.split('.').all(is_package_segment))
        .map(|c| c.replace('.', "/"))
        .filter(|c| {
            c.rsplit_once('/')
                .is_some_and(|(package, _)| packages.iter().any(|p| p == package))
        });
    packages.sort();
    Ok(Some(ModulePathModule {
        root: path.to_string_lossy().into_owned(),
        descriptor: ModuleDescriptor {
            name,
            version,
            is_open: true,
            requires: Vec::new(),
            exports: Vec::new(),
            opens: Vec::new(),
            uses: Vec::new(),
            provides,
            automatic: false,
            main_class,
        },
        packages,
        automatic: true,
    }))
}

/// A path as `java.nio.file.Path.toString()` spells it on this platform
/// (backslash separators on Windows), for the boot layer's `FindException`
/// texts.
fn java_path_text(path: &std::path::Path) -> String {
    let text = path.to_string_lossy();
    if cfg!(windows) {
        text.replace('/', "\\")
    } else {
        text.into_owned()
    }
}

/// Is `path` named `*.jar` (any case, as [`module_at`] reads it)?
fn has_jar_extension(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("jar"))
}

/// [`module_at`] with [`automatic_jar_module_checked`]'s refusals.
fn module_at_checked(
    path: &std::path::Path,
    feature: u32,
) -> Result<Option<ModulePathModule>, String> {
    if has_jar_extension(path) && path.is_file() && modular_jar_module(path, feature).is_none() {
        return automatic_jar_module_checked(path, feature);
    }
    Ok(module_at(path, feature))
}

/// The boot layer's `java.lang.module.FindException` text for a module path
/// HotSpot refuses at startup, or `None`. Scans `entries` as `ModulePath`
/// does (each entry in order; a directory without `module-info.class` is a
/// directory of modules, read in the OS listing order, unsorted): a JAR whose automatic
/// module cannot be derived ([`automatic_jar_module_checked`]), two modules
/// of one name in ONE directory (`scanDirectory`: "Two versions of module
/// <m> found in <dir> (<later> and <earlier>)"), and a regular file that is
/// not a JAR ("Module format not recognized: <entry>"). The caller prints
/// `Error occurred during initialization of boot layer` and
/// `java.lang.module.FindException: ` before it and exits 1. A module of one
/// name in two DIFFERENT entries is no error (the first wins, as in
/// [`resolve_module_path`]). `ModulePath.scanDirectory` reads a directory
/// with `Files.newDirectoryStream` (`readdir` on Linux, `FindFirstFile` /
/// `FindNextFile` on Windows), which is exactly `std::fs::read_dir`'s order,
/// so this scan does NOT sort: the duplicate text names (the entry listed
/// second, the entry listed first) and the first refusal is the one the JDK
/// meets first (wave 103 hand-back: sorted, Linux printed the pair the other
/// way round). Interpreter round i5 wave 103, lane L5.
pub fn module_path_find_error(entries: &[String]) -> Option<String> {
    module_path_find_error_at(entries, MODULE_PATH_JAR_FEATURE)
}

/// [`module_path_find_error`] with multi-release JARs read at `feature`
/// (`JarFile.runtimeVersion()` of the booted library). Interpreter round i5
/// wave 104, lane L5.
pub fn module_path_find_error_at(entries: &[String], feature: u32) -> Option<String> {
    for entry in entries {
        let path = std::path::Path::new(entry);
        let Ok(meta) = std::fs::metadata(path) else {
            continue;
        };
        if meta.is_dir() && !path.join("module-info.class").exists() {
            let Ok(children) = std::fs::read_dir(path) else {
                continue;
            };
            // The OS listing order, as `Files.newDirectoryStream` sees it.
            let child_paths: Vec<std::path::PathBuf> =
                children.flatten().map(|c| c.path()).collect();
            let mut seen: Vec<(String, std::path::PathBuf)> = Vec::new();
            for child in child_paths {
                let module = match module_at_checked(&child, feature) {
                    Ok(Some(m)) => m,
                    Ok(None) => continue,
                    Err(text) => return Some(text),
                };
                let name = module.descriptor.name;
                if let Some((_, previous)) = seen.iter().find(|(n, _)| *n == name) {
                    let file_of = |p: &std::path::Path| {
                        p.file_name()
                            .map(|f| f.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    };
                    return Some(format!(
                        "Two versions of module {name} found in {} ({} and {})",
                        java_path_text(path),
                        file_of(child.as_path()),
                        file_of(previous.as_path())
                    ));
                }
                seen.push((name, child));
            }
            continue;
        }
        if meta.is_file() {
            match module_at_checked(path, feature) {
                Ok(Some(_)) => {}
                Ok(None) if !has_jar_extension(path) => {
                    return Some(format!(
                        "Module format not recognized: {}",
                        java_path_text(path)
                    ));
                }
                Ok(None) => {}
                Err(text) => return Some(text),
            }
        }
    }
    None
}

/// [`automatic_jar_module`]'s name and version from a JAR's file name with
/// `.jar` dropped (`ModulePath.deriveModuleDescriptor`'s `DASH_VERSION`,
/// `-(\d+(\.|$))`, and `cleanModuleName`).
fn automatic_module_name_and_version(stem: &str) -> (String, Option<String>) {
    let bytes = stem.as_bytes();
    let dash = (0..bytes.len()).find(|&i| {
        if bytes[i] != b'-' {
            return false;
        }
        let digits_end = (i + 1..bytes.len())
            .find(|&j| !bytes[j].is_ascii_digit())
            .unwrap_or(bytes.len());
        digits_end > i + 1 && (digits_end == bytes.len() || bytes[digits_end] == b'.')
    });
    // `ModuleDescriptor.Version.parse(tail)`: a tail it refuses gives no
    // version, the name is cut at the dash all the same (wave 103).
    let (raw, version) = match dash {
        Some(i) => (
            &stem[..i],
            module_version_parses(&stem[i + 1..]).then(|| stem[i + 1..].to_string()),
        ),
        None => (stem, None),
    };
    let mut name = String::with_capacity(raw.len());
    for c in raw.chars() {
        let c = if c.is_ascii_alphanumeric() { c } else { '.' };
        if c == '.' && name.ends_with('.') {
            continue;
        }
        name.push(c);
    }
    let name = name.trim_matches('.').to_string();
    (name, version)
}

/// Would `ModuleDescriptor.Version.parse(v)` accept `v`? JDK 25's
/// constructor refuses an empty string, a first character that is not a
/// digit, and an empty pre-release or build part (a `-` or `+` that ends the
/// string where that part would start). Interpreter round i5 wave 103, lane L5.
fn module_version_parses(v: &str) -> bool {
    let b = v.as_bytes();
    let n = b.len();
    if n == 0 || !b[0].is_ascii_digit() {
        return false;
    }
    let number = |mut i: usize| {
        i += 1;
        while i < n && b[i].is_ascii_digit() {
            i += 1;
        }
        i
    };
    let string = |mut i: usize| {
        i += 1;
        while i < n && !matches!(b[i], b'.' | b'-' | b'+' | b'0'..=b'9') {
            i += 1;
        }
        i
    };
    let mut c = b[0];
    let mut i = number(0);
    while i < n {
        c = b[i];
        if c == b'.' {
            i += 1;
            continue;
        }
        if c == b'-' || c == b'+' {
            i += 1;
            break;
        }
        i = if c.is_ascii_digit() { number(i) } else { string(i) };
    }
    if c == b'-' && i >= n {
        return false;
    }
    while i < n {
        c = b[i];
        if c == b'.' || c == b'-' {
            i += 1;
            continue;
        }
        if c == b'+' {
            i += 1;
            break;
        }
        i = if c.is_ascii_digit() { number(i) } else { string(i) };
    }
    !(c == b'+' && i >= n)
}

/// A main-section attribute of a JAR manifest (header names ignore case,
/// continuation lines joined), or `None`.
fn main_manifest_attribute(manifest: &[u8], header: &str) -> Option<String> {
    let text = String::from_utf8_lossy(manifest).replace("\r\n", "\n");
    let mut lines: Vec<String> = Vec::new();
    for line in text.split('\n') {
        if line.is_empty() {
            break;
        }
        match line.strip_prefix(' ') {
            Some(continuation) => {
                if let Some(previous) = lines.last_mut() {
                    previous.push_str(continuation);
                }
            }
            None => lines.push(line.to_string()),
        }
    }
    lines.iter().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(header)
            .then(|| value.trim().to_string())
            .filter(|v| !v.is_empty())
    })
}

/// Every module *observable* on `entries` — i.e. present on the module path,
/// whether or not `--add-modules` selects it.
///
/// A `--module-path` entry is either a module root itself (a directory holding
/// `module-info.class`, or a modular JAR) or a directory *of* module roots;
/// `java` accepts both spellings and so does this.
pub fn scan_module_path(entries: &[String]) -> Vec<ModulePathModule> {
    scan_module_path_at(entries, MODULE_PATH_JAR_FEATURE)
}

/// [`scan_module_path`] with multi-release JARs read at `feature`
/// (`JarFile.runtimeVersion()` of the booted library: which
/// `META-INF/versions/<n>/` entries count under their base names for the
/// package set, `versionedStream`). Interpreter round i5 wave 104, lane L5.
pub fn scan_module_path_at(entries: &[String], feature: u32) -> Vec<ModulePathModule> {
    let mut found: Vec<ModulePathModule> = Vec::new();
    for entry in entries {
        let path = std::path::Path::new(entry);
        if let Some(m) = module_at(path, feature) {
            found.push(m);
            continue;
        }
        if !path.is_dir() {
            continue;
        }
        let Ok(children) = std::fs::read_dir(path) else {
            continue;
        };
        let mut child_paths: Vec<std::path::PathBuf> =
            children.flatten().map(|c| c.path()).collect();
        // `read_dir` order is filesystem-dependent; sort so the resulting
        // search-path order (and therefore split-package shadowing) is the
        // same on every run and every OS.
        child_paths.sort();
        for child in child_paths {
            if let Some(m) = module_at(&child, feature) {
                found.push(m);
            }
        }
    }
    found
}

/// Resolve the `--add-modules` root set against the modules observable on
/// `--module-path`, returning the selected modules plus everything they
/// (transitively) require that also lives on the module path.
///
/// Returns an empty vec when nothing is selected — which is the correct answer
/// for a plain `-cp` launch, and the reason this is safe to call
/// unconditionally at VM init.
///
/// `--add-modules` accepts a comma-separated list per occurrence, so tokens are
/// split on `,` here as well as across occurrences. `ALL-MODULE-PATH` selects
/// every observable module. `ALL-DEFAULT` / `ALL-SYSTEM` select system modules,
/// none of which are on a module path, so they select nothing here.
pub fn resolve_module_path(entries: &[String], add_modules: &[String]) -> Vec<ModulePathModule> {
    resolve_module_path_at(entries, add_modules, MODULE_PATH_JAR_FEATURE)
}

/// [`resolve_module_path`] with multi-release JARs read at `feature`
/// ([`scan_module_path_at`]). Interpreter round i5 wave 104, lane L5.
pub fn resolve_module_path_at(
    entries: &[String],
    add_modules: &[String],
    feature: u32,
) -> Vec<ModulePathModule> {
    if entries.is_empty() {
        return Vec::new();
    }
    let mut observable = scan_module_path_at(entries, feature);
    if observable.is_empty() {
        return Vec::new();
    }
    // The FIRST module of a name on the module path is the observable one
    // (`ModulePath` searches its entries in order and a later module of the
    // same name is shadowed). Since wave 99 every plain jar is an automatic
    // module, so `lib-1.0.jar` and `lib-2.0.jar` in one directory are two
    // modules `lib`; both were registered (interpreter round i5 wave 100,
    // lane L5).
    {
        let mut seen: FxHashSet<String> = FxHashSet::default();
        observable.retain(|m| seen.insert(m.descriptor.name.clone()));
    }

    let mut roots: Vec<String> = Vec::new();
    let mut all = false;
    for spec in add_modules {
        for token in spec.split(',') {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            if token == ALL_MODULE_PATH {
                all = true;
            } else {
                roots.push(token.to_string());
            }
        }
    }

    if all {
        return observable;
    }

    // Transitive closure of `requires`, restricted to what is observable.
    // `requires static` is compile-time only and does NOT pull a module into
    // the graph at run time, matching `build_readability_graph`'s seeding.
    let mut selected: FxHashSet<String> = FxHashSet::default();
    let mut work: Vec<String> = roots;
    while let Some(name) = work.pop() {
        if !selected.insert(name.clone()) {
            continue;
        }
        if let Some(m) = observable.iter().find(|m| m.descriptor.name == name) {
            for req in &m.descriptor.requires {
                if !req.is_static && !selected.contains(&req.module_name) {
                    work.push(req.module_name.clone());
                }
            }
            // Resolving one automatic module resolves every observable
            // automatic module (`java.lang.module.Resolver`: an automatic
            // module reads, and so requires, all of them; interpreter round
            // i5 wave 100, lane L5).
            if m.automatic {
                for other in observable.iter().filter(|o| o.automatic) {
                    if !selected.contains(&other.descriptor.name) {
                        work.push(other.descriptor.name.clone());
                    }
                }
            }
        }
    }

    let mut out: Vec<ModulePathModule> = observable
        .into_iter()
        .filter(|m| selected.contains(&m.descriptor.name))
        .collect();
    out.sort_by(|a, b| a.descriptor.name.cmp(&b.descriptor.name));
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    /// zt df4 §9c: the built-in loader of an image module: the platform set,
    /// then (with the boot set handed in) any other registered module that is
    /// not a boot one; nothing for `java.base`, a boot module, an unknown one
    /// or before the tables are handed in.
    #[test]
    fn zt31_image_module_builtin_loader() {
        use cratonvm_types::ClassLoaderId;
        let mut registry = ModuleRegistry::new();
        for name in ["java.base", "java.sql", "jdk.compiler", "java.logging"] {
            registry.register(sample_desc(name), vec![]);
        }
        assert_eq!(registry.image_module_builtin_loader("java.sql"), None, "tables not handed in");
        registry.set_platform_loader_modules(["java.sql".to_string()]);
        assert_eq!(
            registry.image_module_builtin_loader("java.sql"),
            Some(ClassLoaderId::Extension)
        );
        assert_eq!(registry.image_module_builtin_loader("jdk.compiler"), None, "no boot set");
        registry.set_boot_loader_modules(["java.base".to_string(), "java.logging".to_string()]);
        assert_eq!(
            registry.image_module_builtin_loader("jdk.compiler"),
            Some(ClassLoaderId::Application)
        );
        assert_eq!(registry.image_module_builtin_loader("java.logging"), None);
        assert_eq!(registry.image_module_builtin_loader("java.base"), None);
        assert_eq!(registry.image_module_builtin_loader("no.such.module"), None);
    }

    use super::*;

    fn sample_desc(name: &str) -> ModuleDescriptor {
        ModuleDescriptor {
            name: name.to_string(),
            version: None,
            is_open: false,
            requires: vec![],
            exports: vec![],
            opens: vec![],
            uses: vec![],
            provides: vec![],
            automatic: false,
            main_class: None,
        }
    }

    /// Interpreter round i4 wave 86, lane L5: the boot configuration of a
    /// class-path launch leaves out an aggregator (it exports nothing) and an
    /// incubator module unless `--add-modules` names it; `requires` and
    /// service binding pull qualified-only modules in.
    #[test]
    fn i86_boot_configuration_follows_the_default_roots() {
        let exporting = |name: &str, pkg: &str| {
            let mut d = sample_desc(name);
            d.exports.push(ModuleExportsEntry {
                package_name: pkg.to_string(),
                ..Default::default()
            });
            d
        };
        let requiring = |mut d: ModuleDescriptor, target: &str| {
            d.requires.push(ModuleRequiresEntry {
                module_name: target.to_string(),
                ..Default::default()
            });
            d
        };
        let build = |add: &[String]| {
            let mut reg = ModuleRegistry::new();
            reg.register(exporting("java.base", "java/lang"), vec![]);
            reg.register(
                requiring(exporting("java.sql", "java/sql"), "jdk.internal.q"),
                vec![],
            );
            reg.register(sample_desc("jdk.internal.q"), vec![]);
            reg.register(requiring(sample_desc("java.se"), "java.sql"), vec![]);
            reg.register(exporting("jdk.incubator.v", "jdk/incubator/v"), vec![]);
            let mut user = sample_desc("jdk.user.svc");
            user.uses.push("java/sql/Driver".to_string());
            reg.register(requiring(user, "java.base"), vec![]);
            reg.register(
                ModuleDescriptor {
                    provides: vec![ModuleProvidesEntry {
                        service: "java/sql/Driver".to_string(),
                        with: vec!["p/D".to_string()],
                    }],
                    ..sample_desc("jdk.provider")
                },
                vec![],
            );
            reg.settle_boot_configuration(add, &[]);
            reg
        };
        let reg = build(&[]);
        assert!(reg.is_outside_boot_configuration("java.se"));
        assert!(reg.is_outside_boot_configuration("jdk.incubator.v"));
        assert!(!reg.is_outside_boot_configuration("java.sql"));
        assert!(!reg.is_outside_boot_configuration("jdk.internal.q"), "required");
        assert!(
            reg.is_outside_boot_configuration("jdk.user.svc"),
            "exports nothing, required by nothing"
        );
        assert!(reg.resolved_incubator_modules().is_empty());

        let reg = build(&["jdk.incubator.v,jdk.user.svc".to_string()]);
        assert!(!reg.is_outside_boot_configuration("jdk.incubator.v"));
        assert!(
            !reg.is_outside_boot_configuration("jdk.provider"),
            "bound: provides a service a resolved module uses"
        );
        assert_eq!(reg.resolved_incubator_modules(), vec!["jdk.incubator.v".to_string()]);
    }

    /// Interpreter round i4 wave 87, lane L5: `--limit-modules java.base`
    /// leaves every other system module outside, packages or not, and their
    /// classes are then not loadable.
    #[test]
    fn i87_limit_modules_shrinks_the_observable_set() {
        let mut reg = ModuleRegistry::new();
        let mut base = sample_desc("java.base");
        base.exports.push(ModuleExportsEntry {
            package_name: "java/lang".to_string(),
            ..Default::default()
        });
        reg.register(base, vec!["java/lang".to_string()]);
        let mut sql = sample_desc("java.sql");
        sql.exports.push(ModuleExportsEntry {
            package_name: "java/sql".to_string(),
            ..Default::default()
        });
        reg.register(sql, vec!["java/sql".to_string()]);
        reg.settle_boot_configuration(&[], &["java.base".to_string()]);
        assert!(reg.is_outside_boot_configuration("java.sql"));
        assert!(!reg.is_outside_boot_configuration("java.base"));
        assert!(reg.class_outside_boot_configuration("java/sql/Connection"));
        assert!(!reg.class_outside_boot_configuration("java/lang/String"));

        let mut reg2 = ModuleRegistry::new();
        let mut sql2 = sample_desc("java.sql");
        sql2.exports.push(ModuleExportsEntry {
            package_name: "java/sql".to_string(),
            ..Default::default()
        });
        reg2.register(sql2, vec!["java/sql".to_string()]);
        reg2.settle_boot_configuration(&[], &[]);
        assert!(!reg2.class_outside_boot_configuration("java/sql/Connection"));
    }

    #[test]
    fn unnamed_reads_everything() {
        let reg = ModuleRegistry::new();
        assert!(reg.reads(UNNAMED_MODULE, "java.base"));
        assert!(reg.reads(UNNAMED_MODULE, "java.logging"));
        assert!(reg.reads(UNNAMED_MODULE, "com.example.mymod"));
    }

    /// The other direction of `unnamed_reads_everything` — and the half that
    /// was wrong. `reads` used to return `true` whenever `provider ==
    /// UNNAMED_MODULE`, making the rule symmetric where JPMS makes it
    /// directional. Measured on HotSpot 25: `unnamed.canRead(java.logging)` is
    /// `true`, `java.logging.canRead(unnamed)` is `false`.
    /// `regression-suite/src/RJdkModule.java:124` asserts exactly that and
    /// failed in both `--real-jdk` and `--jdk-only`.
    #[test]
    fn named_module_does_not_implicitly_read_the_unnamed_module() {
        let mut reg = ModuleRegistry::new();
        reg.register(sample_desc("com.example"), vec![]);
        reg.build_readability_graph();

        assert!(
            reg.reads(UNNAMED_MODULE, "com.example"),
            "the unnamed module reads every resolved module"
        );
        assert!(
            !reg.reads("com.example", UNNAMED_MODULE),
            "a named module must NOT implicitly read the unnamed module"
        );
        // ...but the explicit grant (`--add-reads m=ALL-UNNAMED`, or the
        // `Module.addReads0` VM-sync hook) still works, on both the built-graph
        // and un-built-fallback paths.
        reg.add_reads("com.example", UNNAMED_MODULE);
        assert!(reg.reads("com.example", UNNAMED_MODULE));
        reg.register(sample_desc("mod.late"), vec![]); // invalidates the closure
        assert!(reg.reads("com.example", UNNAMED_MODULE));
    }

    /// SBR3 `refmisc`: a jlinked image's generated `SystemModules$all` carries
    /// `java.base`'s recorded hashes. Skipped where no such image is installed.
    #[test]
    fn a_jlinked_images_system_modules_class_yields_java_base_hashes() {
        for home in [
            "/data/jdkimages/jdk25-linux/jdk-25.0.4+7",
            "C:/Program Files/Java/jdk-25",
        ] {
            let modules = std::path::Path::new(home).join("lib").join("modules");
            if !modules.exists() {
                continue;
            }
            let mut cp = crate::class_path::ClassPath::new(&[]);
            cp.add_jimage(&modules).expect("add_jimage");
            let bytes = match cp.find_class("jdk/internal/module/SystemModules$all") {
                Ok(b) => b,
                Err(e) => panic!("no SystemModules$all in {home}: {e:?}"),
            };
            let names = system_modules_java_base_hash_names(&bytes)
                .unwrap_or_else(|| panic!("no hashes read from {home}"));
            assert!(names.iter().any(|n| n == "java.sql"), "names: {names:?}");
            assert!(names.len() > 30, "names: {names:?}");
        }
    }

    /// The classpath-compatibility escape hatch the symmetric rule was there
    /// for. None of its users go through the deleted arm: an automatic module
    /// reads everything by its own arm, an unregistered module gets the
    /// open-world `true`, and both access checks short-circuit on an unnamed
    /// participant before `reads` is ever consulted.
    #[test]
    fn classpath_escape_hatch_survives_the_directional_rule() {
        let mut automatic = sample_desc("org.jboss.logging");
        automatic.automatic = true;
        let mut reg = ModuleRegistry::new();
        reg.register(automatic, vec![]);
        reg.register(sample_desc("com.example"), vec![]);
        reg.build_readability_graph();

        // Automatic (classpath jar carrying a module-info) reads everything.
        assert!(reg.reads("org.jboss.logging", UNNAMED_MODULE));
        // A module the registry never saw keeps the open-world answer.
        assert!(reg.reads("mod.unregistered", UNNAMED_MODULE));
        // Neither access check consults `reads` when either side is unnamed.
        assert!(reg
            .check_module_access("com.example", UNNAMED_MODULE, "com/whatever")
            .is_ok());
        assert!(reg
            .check_deep_reflection_access("com.example", UNNAMED_MODULE, "com/whatever")
            .is_ok());
    }

    #[test]
    fn module_reads_itself_and_java_base() {
        let mut reg = ModuleRegistry::new();
        reg.register(sample_desc("com.example"), vec![]);
        reg.build_readability_graph();
        assert!(reg.reads("com.example", "com.example"));
        assert!(reg.reads("com.example", JAVA_BASE));
    }

    #[test]
    fn direct_requires_edge() {
        let mut desc_a = sample_desc("modA");
        desc_a.requires.push(ModuleRequiresEntry {
            module_name: "modB".to_string(),
            is_transitive: false,
            is_static: false,
            ..Default::default()
        });
        let desc_b = sample_desc("modB");

        let mut reg = ModuleRegistry::new();
        reg.register(desc_a, vec![]);
        reg.register(desc_b, vec![]);
        reg.build_readability_graph();

        assert!(reg.reads("modA", "modB"));
        assert!(!reg.reads("modB", "modA")); // not symmetric
    }

    #[test]
    fn transitive_requires_propagates() {
        // A requires transitive B, C requires A → C should also read B
        let mut desc_a = sample_desc("modA");
        desc_a.requires.push(ModuleRequiresEntry {
            module_name: "modB".to_string(),
            is_transitive: true,
            is_static: false,
            ..Default::default()
        });

        let mut desc_c = sample_desc("modC");
        desc_c.requires.push(ModuleRequiresEntry {
            module_name: "modA".to_string(),
            is_transitive: false,
            is_static: false,
            ..Default::default()
        });

        let mut reg = ModuleRegistry::new();
        reg.register(desc_a, vec![]);
        reg.register(sample_desc("modB"), vec![]);
        reg.register(desc_c, vec![]);
        reg.build_readability_graph();

        assert!(reg.reads("modC", "modA"));
        assert!(reg.reads("modC", "modB")); // via transitive
    }

    #[test]
    fn package_to_module_lookup() {
        let mut reg = ModuleRegistry::new();
        reg.register(
            sample_desc("java.base"),
            vec!["java/lang".to_string(), "java/util".to_string()],
        );
        assert_eq!(reg.module_for_package("java/lang"), Some("java.base"));
        assert_eq!(reg.module_for_package("java/util"), Some("java.base"));
        assert_eq!(reg.module_for_package("com/example"), None);
    }

    #[test]
    fn exports_unqualified() {
        let mut desc = sample_desc("modA");
        desc.exports.push(ModuleExportsEntry {
            package_name: "com/foo".to_string(),
            to_modules: vec![],
            ..Default::default()
        });
        assert!(desc.exports_package_to("com/foo", "modB"));
        assert!(desc.exports_package_to("com/foo", "modC"));
        assert!(!desc.exports_package_to("com/bar", "modB"));
    }

    #[test]
    fn exports_qualified() {
        let mut desc = sample_desc("modA");
        desc.exports.push(ModuleExportsEntry {
            package_name: "com/foo".to_string(),
            to_modules: vec!["modB".to_string()],
            ..Default::default()
        });
        assert!(desc.exports_package_to("com/foo", "modB"));
        assert!(!desc.exports_package_to("com/foo", "modC"));
    }

    #[test]
    fn open_module_exports_everything() {
        let mut desc = sample_desc("modA");
        desc.is_open = true;
        assert!(desc.exports_package_to("any/package", "any.module"));
        assert!(desc.opens_package_to("any/package", "any.module"));
    }

    #[test]
    fn check_access_same_module() {
        let reg = ModuleRegistry::new();
        assert!(reg.check_module_access("modA", "modA", "pkg").is_ok());
    }

    #[test]
    fn check_access_unnamed_always_ok() {
        let reg = ModuleRegistry::new();
        assert!(reg
            .check_module_access(UNNAMED_MODULE, "java.base", "java/lang")
            .is_ok());
        assert!(reg
            .check_module_access("java.base", UNNAMED_MODULE, "java/lang")
            .is_ok());
    }

    #[test]
    fn check_access_unregistered_module_allowed() {
        let reg = ModuleRegistry::new();
        // No module descriptors → open-world assumption
        assert!(reg.check_module_access("modA", "modB", "pkg").is_ok());
    }

    #[test]
    fn package_of_helper() {
        assert_eq!(package_of("java/lang/Object"), "java/lang");
        assert_eq!(package_of("Foo"), "");
        assert_eq!(package_of("com/example/Foo"), "com/example");
    }

    // -----------------------------------------------------------------------
    // Phase B: Dynamic mutations
    // -----------------------------------------------------------------------

    #[test]
    fn add_reads_dynamic() {
        let mut reg = ModuleRegistry::new();
        reg.register(sample_desc("modA"), vec![]);
        reg.register(sample_desc("modB"), vec![]);
        reg.build_readability_graph();

        // Before addReads, modA does not read modB (no requires edge).
        assert!(!reg.reads("modA", "modB"));

        reg.add_reads("modA", "modB");
        assert!(reg.reads("modA", "modB"));
    }

    #[test]
    fn add_reads_propagates_requires_transitive() {
        // modB `requires transitive` modC, and modC `requires transitive` modD.
        // After modA dynamically `addReads modB`, modA must also read modC and
        // modD (implied transitive closure), matching a full rebuild.
        let mut desc_b = sample_desc("modB");
        desc_b.requires.push(ModuleRequiresEntry {
            module_name: "modC".to_string(),
            is_transitive: true,
            is_static: false,
            ..Default::default()
        });
        let mut desc_c = sample_desc("modC");
        desc_c.requires.push(ModuleRequiresEntry {
            module_name: "modD".to_string(),
            is_transitive: true,
            is_static: false,
            ..Default::default()
        });

        let mut reg = ModuleRegistry::new();
        reg.register(sample_desc("modA"), vec![]);
        reg.register(desc_b, vec![]);
        reg.register(desc_c, vec![]);
        reg.register(sample_desc("modD"), vec![]);
        reg.build_readability_graph();

        // Before addReads, modA reads none of B/C/D.
        assert!(!reg.reads("modA", "modB"));
        assert!(!reg.reads("modA", "modC"));
        assert!(!reg.reads("modA", "modD"));

        reg.add_reads("modA", "modB");
        assert!(reg.reads("modA", "modB"));
        assert!(reg.reads("modA", "modC")); // implied by modB requires transitive
        assert!(reg.reads("modA", "modD")); // implied transitively via modC
    }

    #[test]
    fn add_reads_requires_transitive_cycle_terminates() {
        // Cyclic `requires transitive`: modB ⇄ modC. add_reads must not loop
        // forever and must include both in the implied closure.
        let mut desc_b = sample_desc("modB");
        desc_b.requires.push(ModuleRequiresEntry {
            module_name: "modC".to_string(),
            is_transitive: true,
            is_static: false,
            ..Default::default()
        });
        let mut desc_c = sample_desc("modC");
        desc_c.requires.push(ModuleRequiresEntry {
            module_name: "modB".to_string(),
            is_transitive: true,
            is_static: false,
            ..Default::default()
        });

        let mut reg = ModuleRegistry::new();
        reg.register(sample_desc("modA"), vec![]);
        reg.register(desc_b, vec![]);
        reg.register(desc_c, vec![]);
        reg.build_readability_graph();

        reg.add_reads("modA", "modB");
        assert!(reg.reads("modA", "modB"));
        assert!(reg.reads("modA", "modC")); // via modB requires transitive (cycle-safe)
    }

    #[test]
    fn add_reads_requires_static_not_propagated() {
        // `requires static` is compile-time only — a reads edge to modB must
        // NOT pull in modB's `requires static` dependency.
        let mut desc_b = sample_desc("modB");
        desc_b.requires.push(ModuleRequiresEntry {
            module_name: "modC".to_string(),
            is_transitive: true,
            is_static: true, // transitive + static → still compile-time only
            ..Default::default()
        });

        let mut reg = ModuleRegistry::new();
        reg.register(sample_desc("modA"), vec![]);
        reg.register(desc_b, vec![]);
        reg.register(sample_desc("modC"), vec![]);
        reg.build_readability_graph();

        reg.add_reads("modA", "modB");
        assert!(reg.reads("modA", "modB"));
        assert!(!reg.reads("modA", "modC")); // static dep not implied
    }

    // -----------------------------------------------------------------------
    // Regression: `reads()` with the readability closure invalidated
    // -----------------------------------------------------------------------

    #[test]
    fn dynamic_reads_survive_graph_invalidation_by_register() {
        // Repro for the un-built-fallback defect. `add_reads` patches the
        // cached closure only while `graph_built` is true; `register`
        // (every lazily-loaded `module-info.class` goes through it) sets
        // `graph_built = false` and clears `readable`. The fallback used to
        // consult `desc.requires` only, so the dynamic edge vanished until
        // something happened to call `build_readability_graph` again.
        //
        // `check_module_access` and `check_deep_reflection_access` both gate
        // on `reads`, so inside that window a user's `--add-reads` /
        // `Module.addReads` turns into a spurious IllegalAccessError.
        let mut reg = ModuleRegistry::new();
        reg.register(sample_desc("modA"), vec![]);
        reg.register(sample_desc("modB"), vec![]);
        reg.build_readability_graph();

        reg.add_reads("modA", "modB");
        assert!(reg.reads("modA", "modB"), "patched into the built closure");

        // A later module-info load invalidates the closure without rebuilding.
        reg.register(sample_desc("modLate"), vec![]);
        assert!(
            reg.reads("modA", "modB"),
            "dynamic addReads edge must survive closure invalidation"
        );

        // …and a rebuild must agree with the fallback.
        reg.build_readability_graph();
        assert!(reg.reads("modA", "modB"));
    }

    #[test]
    fn dynamic_reads_imply_requires_transitive_before_rebuild() {
        // Same window, but for the closure `add_reads` would have patched in:
        // a reads edge to modB also implies everything modB `requires
        // transitive`. The fallback must agree with the patched cache.
        let mut desc_b = sample_desc("modB");
        desc_b.requires.push(ModuleRequiresEntry {
            module_name: "modC".to_string(),
            is_transitive: true,
            is_static: false,
            ..Default::default()
        });

        let mut reg = ModuleRegistry::new();
        reg.register(sample_desc("modA"), vec![]);
        reg.register(desc_b, vec![]);
        reg.register(sample_desc("modC"), vec![]);
        reg.build_readability_graph();
        reg.add_reads("modA", "modB");

        reg.register(sample_desc("modLate"), vec![]); // invalidates
        assert!(reg.reads("modA", "modB"));
        assert!(
            reg.reads("modA", "modC"),
            "implied `requires transitive` closure must survive invalidation"
        );
    }

    #[test]
    fn requires_static_is_not_readable_with_or_without_the_closure() {
        // `requires static` is compile-time only. `build_readability_graph`
        // step 1 filters it (`if !req.is_static`), but the un-built fallback
        // used to match on `module_name` alone — so the SAME query answered
        // `true` before the closure was built and `false` after it.
        let mut desc_a = sample_desc("modA");
        desc_a.requires.push(ModuleRequiresEntry {
            module_name: "modB".to_string(),
            is_transitive: false,
            is_static: true,
            ..Default::default()
        });

        let mut reg = ModuleRegistry::new();
        reg.register(desc_a, vec![]);
        reg.register(sample_desc("modB"), vec![]);

        // Closure not built yet — the fallback path.
        assert!(
            !reg.reads("modA", "modB"),
            "`requires static` must not grant runtime readability"
        );

        reg.build_readability_graph();
        assert!(!reg.reads("modA", "modB"), "and the built graph must agree");
    }

    #[test]
    fn non_static_direct_requires_still_readable_without_the_closure() {
        // Guard the other half of the same expression: the fallback must keep
        // honouring ordinary (non-static) direct `requires`.
        let mut desc_a = sample_desc("modA");
        desc_a.requires.push(ModuleRequiresEntry {
            module_name: "modB".to_string(),
            is_transitive: false,
            is_static: false,
            ..Default::default()
        });

        let mut reg = ModuleRegistry::new();
        reg.register(desc_a, vec![]);
        reg.register(sample_desc("modB"), vec![]);

        assert!(reg.reads("modA", "modB"));
        reg.build_readability_graph();
        assert!(reg.reads("modA", "modB"));
    }

    #[test]
    fn add_exports_dynamic() {
        let mut desc = sample_desc("modA");
        desc.exports.push(ModuleExportsEntry {
            package_name: "com/internal".to_string(),
            to_modules: vec!["modB".to_string()], // only exported to modB
            ..Default::default()
        });
        let mut reg = ModuleRegistry::new();
        reg.register(desc, vec!["com/internal".to_string()]);
        reg.register(sample_desc("modB"), vec![]);
        reg.register(sample_desc("modC"), vec![]);

        // modC cannot access com/internal initially.
        assert!(!reg.is_package_exported_to("modA", "com/internal", "modC"));

        // Add dynamic export to modC.
        reg.add_exports("modA", "com/internal", "modC");
        assert!(reg.is_package_exported_to("modA", "com/internal", "modC"));
    }

    #[test]
    fn add_opens_dynamic() {
        let mut reg = ModuleRegistry::new();
        reg.register(sample_desc("modA"), vec!["com/secret".to_string()]);
        reg.register(sample_desc("modB"), vec![]);

        assert!(!reg.is_package_open_to("modA", "com/secret", "modB"));

        reg.add_opens("modA", "com/secret", "modB");
        assert!(reg.is_package_open_to("modA", "com/secret", "modB"));
    }

    // -----------------------------------------------------------------------
    // Phase B: isExported / isOpen queries
    // -----------------------------------------------------------------------

    #[test]
    fn is_package_exported_unqualified_named_module() {
        let mut desc = sample_desc("modA");
        desc.exports.push(ModuleExportsEntry {
            package_name: "com/public".to_string(),
            to_modules: vec![], // unqualified
            ..Default::default()
        });
        desc.exports.push(ModuleExportsEntry {
            package_name: "com/private".to_string(),
            to_modules: vec!["modB".to_string()], // qualified
            ..Default::default()
        });
        let mut reg = ModuleRegistry::new();
        reg.register(desc, vec![]);

        assert!(reg.is_package_exported_unqualified("modA", "com/public"));
        assert!(!reg.is_package_exported_unqualified("modA", "com/private"));
        assert!(!reg.is_package_exported_unqualified("modA", "com/nonexistent"));
    }

    #[test]
    fn is_package_exported_to_qualified() {
        let mut desc = sample_desc("modA");
        desc.exports.push(ModuleExportsEntry {
            package_name: "com/foo".to_string(),
            to_modules: vec!["modB".to_string()],
            ..Default::default()
        });
        let mut reg = ModuleRegistry::new();
        reg.register(desc, vec![]);

        assert!(reg.is_package_exported_to("modA", "com/foo", "modB"));
        assert!(!reg.is_package_exported_to("modA", "com/foo", "modC"));
    }

    #[test]
    fn unnamed_module_exports_and_opens_everything() {
        let reg = ModuleRegistry::new();
        assert!(reg.is_package_exported_unqualified(UNNAMED_MODULE, "anything"));
        assert!(reg.is_package_open_unqualified(UNNAMED_MODULE, "anything"));
        assert!(reg.is_package_exported_to(UNNAMED_MODULE, "anything", "modX"));
        assert!(reg.is_package_open_to(UNNAMED_MODULE, "anything", "modX"));
    }

    #[test]
    fn open_module_opens_and_exports_all() {
        let mut desc = sample_desc("modA");
        desc.is_open = true;
        let mut reg = ModuleRegistry::new();
        reg.register(desc, vec![]);

        assert!(reg.is_package_exported_unqualified("modA", "any/pkg"));
        assert!(reg.is_package_open_unqualified("modA", "any/pkg"));
        assert!(reg.is_package_exported_to("modA", "any/pkg", "modB"));
        assert!(reg.is_package_open_to("modA", "any/pkg", "modB"));
    }

    #[test]
    fn is_package_open_unqualified_declared() {
        let mut desc = sample_desc("modA");
        desc.opens.push(ModuleOpensEntry {
            package_name: "com/reflect".to_string(),
            to_modules: vec![], // unqualified
            ..Default::default()
        });
        let mut reg = ModuleRegistry::new();
        reg.register(desc, vec![]);

        assert!(reg.is_package_open_unqualified("modA", "com/reflect"));
        assert!(!reg.is_package_open_unqualified("modA", "com/other"));
    }

    // -----------------------------------------------------------------------
    // Phase B: packages_of / module_names / service_providers
    // -----------------------------------------------------------------------

    #[test]
    fn packages_of_module() {
        let mut reg = ModuleRegistry::new();
        reg.register(
            sample_desc("java.base"),
            vec![
                "java/lang".to_string(),
                "java/util".to_string(),
                "java/io".to_string(),
            ],
        );
        reg.register(sample_desc("modB"), vec!["com/b".to_string()]);

        let mut pkgs = reg.packages_of("java.base");
        pkgs.sort();
        assert_eq!(pkgs, vec!["java/io", "java/lang", "java/util"]);
        assert_eq!(reg.packages_of("modB"), vec!["com/b"]);
        assert!(reg.packages_of("nonexistent").is_empty());
    }

    #[test]
    fn module_names_list() {
        let mut reg = ModuleRegistry::new();
        reg.register(sample_desc("modA"), vec![]);
        reg.register(sample_desc("modB"), vec![]);
        let mut names = reg.module_names();
        names.sort();
        assert_eq!(names, vec!["modA", "modB"]);
    }

    #[test]
    fn service_providers_from_provides() {
        let mut desc = sample_desc("modA");
        desc.provides.push(ModuleProvidesEntry {
            service: "com/example/SPI".to_string(),
            with: vec!["com/example/SPIImpl".to_string()],
        });
        let mut desc_b = sample_desc("modB");
        desc_b.provides.push(ModuleProvidesEntry {
            service: "com/example/SPI".to_string(),
            with: vec!["com/other/SPIImpl2".to_string()],
        });
        desc_b.provides.push(ModuleProvidesEntry {
            service: "com/example/OtherSPI".to_string(),
            with: vec!["com/other/OtherImpl".to_string()],
        });

        let mut reg = ModuleRegistry::new();
        reg.register(desc, vec![]);
        reg.register(desc_b, vec![]);

        let mut providers = reg.service_providers("com/example/SPI");
        providers.sort();
        assert_eq!(providers, vec!["com/example/SPIImpl", "com/other/SPIImpl2"]);

        let other = reg.service_providers("com/example/OtherSPI");
        assert_eq!(other, vec!["com/other/OtherImpl"]);

        assert!(reg.service_providers("nonexistent/SPI").is_empty());
    }

    /// A modular jar reached through `-cp` is an unnamed-module citizen and a
    /// real JVM ignores its `module-info` outright, `provides` included. Before
    /// 2026-08-21 `service_providers` walked every descriptor with no filter,
    /// so `regression-suite/src/RServiceLoaderDoubleSource.java` measured
    /// `descriptors=0 providers=2` where HotSpot 25.0.3+9 answers 0.
    ///
    /// The negative control is the half that matters and it is in this same
    /// test on purpose: a filter that also caught DECLARED modules would delete
    /// the platform's entire JCA provider set, which arrives through `provides`
    /// clauses and through nothing else. Asserting only the exclusion would
    /// pass on a function that returns an empty vector unconditionally.
    #[test]
    fn service_providers_skips_class_path_only_modules() {
        let mut declared = sample_desc("declared.mod");
        declared.provides.push(ModuleProvidesEntry {
            service: "com/example/SPI".to_string(),
            with: vec!["com/example/DeclaredImpl".to_string()],
        });

        let mut cp_only = sample_desc("cponly.mod");
        cp_only.automatic = true;
        cp_only.provides.push(ModuleProvidesEntry {
            service: "com/example/SPI".to_string(),
            with: vec!["com/example/ClassPathImpl".to_string()],
        });

        let mut reg = ModuleRegistry::new();
        reg.register(declared, vec![]);
        reg.register(cp_only, vec![]);

        // Excluded: the `-cp` module's provider must not reach ServiceLoader.
        // Included: the declared module's provider must still arrive.
        assert_eq!(
            reg.service_providers("com/example/SPI"),
            vec!["com/example/DeclaredImpl"],
        );
        assert!(reg.is_class_path_only("cponly.mod"));
        assert!(!reg.is_class_path_only("declared.mod"));
    }

    // -----------------------------------------------------------------------
    // Phase B: Cycle detection
    // -----------------------------------------------------------------------

    #[test]
    fn detect_cycles_no_cycles() {
        let mut desc_a = sample_desc("modA");
        desc_a.requires.push(ModuleRequiresEntry {
            module_name: "modB".to_string(),
            is_transitive: false,
            is_static: false,
            ..Default::default()
        });
        let mut reg = ModuleRegistry::new();
        reg.register(desc_a, vec![]);
        reg.register(sample_desc("modB"), vec![]);

        assert!(reg.detect_cycles().is_empty());
    }

    #[test]
    fn detect_cycles_simple_cycle() {
        let mut desc_a = sample_desc("modA");
        desc_a.requires.push(ModuleRequiresEntry {
            module_name: "modB".to_string(),
            is_transitive: false,
            is_static: false,
            ..Default::default()
        });
        let mut desc_b = sample_desc("modB");
        desc_b.requires.push(ModuleRequiresEntry {
            module_name: "modA".to_string(),
            is_transitive: false,
            is_static: false,
            ..Default::default()
        });
        let mut reg = ModuleRegistry::new();
        reg.register(desc_a, vec![]);
        reg.register(desc_b, vec![]);

        let cycles = reg.detect_cycles();
        assert!(!cycles.is_empty());
    }

    // -----------------------------------------------------------------------
    // Phase B: Deep reflection access
    // -----------------------------------------------------------------------

    #[test]
    fn deep_reflection_same_module_ok() {
        let reg = ModuleRegistry::new();
        assert!(reg
            .check_deep_reflection_access("modA", "modA", "com/foo")
            .is_ok());
    }

    #[test]
    fn deep_reflection_unnamed_target_always_ok() {
        // A classpath (unnamed) target has nothing to encapsulate —
        // reflection into it always succeeds regardless of accessor.
        let mut reg = ModuleRegistry::new();
        reg.register(sample_desc("modA"), vec![]);
        assert!(reg
            .check_deep_reflection_access("modA", UNNAMED_MODULE, "com/foo")
            .is_ok());
        assert!(reg
            .check_deep_reflection_access(UNNAMED_MODULE, UNNAMED_MODULE, "anything")
            .is_ok());
    }

    #[test]
    fn deep_reflection_unnamed_accessor_denied_by_default_jep403() {
        // JEP 403 (JDK 17+): classpath code CANNOT deep-reflect into a
        // named module's non-opened package by default. Pre-JEP-403 JDKs
        // treated unnamed accessors as unconditionally allowed — that is
        // the behavior CratonVM explicitly rejects here.
        let mut reg = ModuleRegistry::new();
        reg.register(sample_desc("modA"), vec!["com/secret".to_string()]);
        reg.build_readability_graph();
        assert!(
            reg.check_deep_reflection_access(UNNAMED_MODULE, "modA", "com/secret")
                .is_err(),
            "unnamed accessor must not bypass strong encapsulation"
        );
    }

    #[test]
    fn deep_reflection_unnamed_accessor_allowed_with_add_opens() {
        // Same denied case, but an unqualified `opens` (empty target) grants
        // access to every accessor, the unnamed one included.
        let mut reg = ModuleRegistry::new();
        reg.register(sample_desc("modA"), vec!["com/secret".to_string()]);
        reg.build_readability_graph();
        // add_opens with empty target = unqualified = open to all (incl. unnamed)
        reg.add_opens("modA", "com/secret", "");
        assert!(
            reg.check_deep_reflection_access(UNNAMED_MODULE, "modA", "com/secret")
                .is_ok(),
            "an unqualified open should grant unnamed accessor deep access"
        );
    }

    /// `--add-opens modA/com.secret=ALL-UNNAMED` grants the unnamed module and
    /// **only** the unnamed module.
    ///
    /// The three assertions are one fact each, and the flag is only correct if
    /// all three hold — a version that merely grants the unnamed accessor
    /// (assertion 1) passes the reflection path while still lying to
    /// `Module.isOpen` and over-granting every named module.
    ///
    /// Oracle: Temurin 25 under
    /// `--add-opens=java.base/java.net=ALL-UNNAMED`, via
    /// `probes/AddOpensFlagProbe.java` (2026-08-09) —
    /// `open java.net unqualified=false`, `open java.net toSelf=true`.
    #[test]
    fn add_opens_all_unnamed_is_qualified_to_the_unnamed_module_only() {
        let mut reg = ModuleRegistry::new();
        reg.register(sample_desc("modA"), vec!["com/secret".to_string()]);
        reg.register(sample_desc("modB"), vec![]);
        reg.build_readability_graph();
        reg.add_opens("modA", "com/secret", ALL_UNNAMED_TARGET);

        assert!(
            reg.check_deep_reflection_access(UNNAMED_MODULE, "modA", "com/secret")
                .is_ok(),
            "ALL-UNNAMED must grant the unnamed accessor deep access"
        );
        assert!(
            !reg.is_package_open_unqualified("modA", "com/secret"),
            "ALL-UNNAMED is a qualified open; Module.isOpen(pkg) must stay false"
        );
        assert!(
            !reg.is_package_open_to("modA", "com/secret", "modB"),
            "ALL-UNNAMED must not reach a named module"
        );
    }

    /// A modular jar reached through the CLASS path does not give its classes a
    /// module NAME, and `--add-opens …=ALL-UNNAMED` therefore reaches them.
    ///
    /// This is the whole of the 2026-09-05 Tomcat fix, in its smallest form:
    /// `catalina.jar` is on `-cp` and carries a `module-info.class` declaring
    /// `org.apache.tomcat.catalina`, so before
    /// [`ModuleRegistry::named_module_for_package`] existed, every class in it
    /// was labelled with that name and `--add-opens
    /// java.base/java.util=ALL-UNNAMED` — qualified to the unnamed module —
    /// could not grant it. HotSpot passes the identical classpath and flags.
    ///
    /// Both halves are asserted, because either alone is satisfiable by a wrong
    /// implementation: dropping the name for EVERY module would also pass the
    /// first assertion, and keeping it for every module would pass the second.
    #[test]
    fn a_class_path_jars_module_info_does_not_name_its_classes_module() {
        let mut reg = ModuleRegistry::new();
        // A `--module-path` module: `vm_init` re-registers these with
        // `automatic = false`, which is what `is_class_path_only` reads.
        reg.register(
            sample_desc("mod.on.module.path"),
            vec!["com/mp".to_string()],
        );
        // A modular jar found by the APPLICATION class-path scan.
        let mut cp_jar = sample_desc("org.apache.tomcat.catalina");
        cp_jar.automatic = true;
        reg.register(cp_jar, vec!["org/apache/catalina/loader".to_string()]);
        reg.build_readability_graph();

        assert_eq!(
            reg.module_for_package("org/apache/catalina/loader"),
            Some("org.apache.tomcat.catalina"),
            "the descriptor itself stays in the registry — service discovery \
             and labelling still want it"
        );
        assert_eq!(
            reg.named_module_for_package("org/apache/catalina/loader"),
            None,
            "but a class from that jar is in the UNNAMED module, as on HotSpot"
        );
        assert_eq!(
            reg.named_module_for_package("com/mp"),
            Some("mod.on.module.path"),
            "a genuine --module-path module keeps its name"
        );
    }

    /// JIT round 20 lane superAcc: an open is an export to the same target
    /// (`Module.isExported(pn, other)`; HotSpot's VM is told of every open by
    /// `addExports0`), declared or dynamic, and reaches no other module.
    #[test]
    fn an_opened_package_is_exported_to_its_open_target() {
        let mut desc = sample_desc("modA");
        desc.opens.push(ModuleOpensEntry {
            package_name: "com/declared".to_string(),
            to_modules: vec!["modB".to_string()],
            ..Default::default()
        });
        let mut reg = ModuleRegistry::new();
        reg.register(
            desc,
            vec!["com/declared".to_string(), "com/secret".to_string()],
        );
        reg.register(sample_desc("modB"), vec![]);
        reg.register(sample_desc("modC"), vec![]);
        reg.build_readability_graph();
        assert!(reg.is_package_exported_to("modA", "com/declared", "modB"));
        assert!(!reg.is_package_exported_to("modA", "com/declared", "modC"));
        assert!(!reg.is_package_exported_to("modA", "com/secret", UNNAMED_MODULE));
        reg.add_opens("modA", "com/secret", ALL_UNNAMED_TARGET);
        assert!(reg.is_package_exported_to("modA", "com/secret", UNNAMED_MODULE));
        assert!(!reg.is_package_exported_to("modA", "com/secret", "modC"));
    }

    /// The `--add-exports` half of the same token, so a later edit cannot fix
    /// one direction and leave the other conflated.
    #[test]
    fn add_exports_all_unnamed_is_qualified_to_the_unnamed_module_only() {
        let mut reg = ModuleRegistry::new();
        reg.register(sample_desc("modA"), vec!["com/secret".to_string()]);
        reg.register(sample_desc("modB"), vec![]);
        reg.build_readability_graph();
        reg.add_exports("modA", "com/secret", ALL_UNNAMED_TARGET);

        assert!(
            reg.is_package_exported_to("modA", "com/secret", UNNAMED_MODULE),
            "ALL-UNNAMED must export to the unnamed module"
        );
        assert!(
            !reg.is_package_exported_unqualified("modA", "com/secret"),
            "ALL-UNNAMED is a qualified export; Module.isExported(pkg) must stay false"
        );
        assert!(
            !reg.is_package_exported_to("modA", "com/secret", "modB"),
            "ALL-UNNAMED must not reach a named module"
        );
    }

    /// Wave 124 (lane L5): a command-line edge's target is a boot-layer
    /// module by identity, so under `CRATONVM_MODULE_ACCESS_EXPORT_ONE_QUESTION`
    /// it does not reach a layer module that shares the name; a run-time edge
    /// recorded by name still does, and the boot module keeps the edge.
    #[test]
    fn command_line_edge_does_not_reach_a_same_named_layer_module_by_identity() {
        let mut reg = ModuleRegistry::new();
        reg.register(sample_desc("modA"), vec!["com/secret".to_string()]);
        reg.register(sample_desc("modB"), vec![]);
        reg.build_readability_graph();
        reg.add_command_line_exports("modA", "com/secret", "modB");
        reg.add_command_line_opens("modA", "com/secret", "modB");
        let cl = &reg.extra_exports["modA"][0];
        assert!(cl.command_line);
        assert!(layer_edge_admits(cl, "com/secret", "modB", None, false));
        assert!(!layer_edge_admits(cl, "com/secret", "modB", None, true));
        let cl_open = &reg.extra_opens["modA"][0];
        assert!(!layer_edge_admits(cl_open, "com/secret", "modB", None, true));
        // The boot module `modB` (the name-keyed question) keeps the edge.
        assert!(reg.is_package_exported_to("modA", "com/secret", "modB"));
        assert!(reg.is_package_open_to("modA", "com/secret", "modB"));
        reg.add_exports("modA", "com/secret", "modC");
        let rt = &reg.extra_exports["modA"][1];
        assert!(!rt.command_line);
        assert!(layer_edge_admits(rt, "com/secret", "modC", None, true));
        assert!(!layer_edge_admits(rt, "com/other", "modC", None, true));
    }

    /// Wave 125 (lane L5): a run-time edge recorded to a layer module by
    /// identity (`addExports0` with a user-layer `other`) reaches that module
    /// alone under `CRATONVM_MODULE_ACCESS_EXPORT_ONE_QUESTION` once the asked
    /// module's loader is known; an edge to the boot layer's module of that
    /// name reaches no layer module; with the switch off (or the loader
    /// unknown) both match by name, as before.
    #[test]
    fn run_time_edge_reaches_only_the_layer_module_it_was_recorded_to() {
        let mut reg = ModuleRegistry::new();
        reg.register(sample_desc("modA"), vec!["com/secret".to_string()]);
        reg.build_readability_graph();
        reg.add_exports_to_layer_module("modA", "com/secret", "m", 7);
        reg.add_opens_to_layer_module("modA", "com/secret", "m", 7);
        reg.add_exports("modA", "com/boot", "m");
        let to_layer = &reg.extra_exports["modA"][0];
        assert_eq!(to_layer.layer_target, Some(7));
        assert!(!to_layer.command_line);
        // Identity known: the recorded module only.
        assert!(layer_edge_admits(to_layer, "com/secret", "m", Some(7), true));
        assert!(!layer_edge_admits(to_layer, "com/secret", "m", Some(8), true));
        let to_layer_open = &reg.extra_opens["modA"][0];
        assert!(layer_edge_admits(to_layer_open, "com/secret", "m", Some(7), true));
        assert!(!layer_edge_admits(to_layer_open, "com/secret", "m", Some(8), true));
        // An edge to the boot layer's `m` reaches no layer `m` by identity.
        let to_boot = &reg.extra_exports["modA"][1];
        assert_eq!(to_boot.layer_target, None);
        assert!(!layer_edge_admits(to_boot, "com/boot", "m", Some(7), true));
        // Switch off, or the loader unknown: by name, as before.
        assert!(layer_edge_admits(to_layer, "com/secret", "m", Some(8), false));
        assert!(layer_edge_admits(to_boot, "com/boot", "m", Some(7), false));
        assert!(layer_edge_admits(to_layer, "com/secret", "m", None, true));
        assert!(layer_edge_admits(to_boot, "com/boot", "m", None, true));
        // The name-keyed questions are unchanged.
        assert!(reg.is_package_exported_to("modA", "com/secret", "m"));
        assert!(reg.is_package_exported_to("modA", "com/boot", "m"));
    }

    /// The launcher's parse and the registry's resolution have to agree on the
    /// token. They live in different crates (`vm::config` and this one) and the
    /// bug was precisely that they disagreed, so pin the spelling here too.
    #[test]
    fn all_unnamed_token_matches_the_jdk_spelling() {
        assert_eq!(ALL_UNNAMED_TARGET, "ALL-UNNAMED");
        assert_eq!(resolve_edge_target(""), None);
        assert_eq!(
            resolve_edge_target(ALL_UNNAMED_TARGET),
            Some(UNNAMED_MODULE.to_string())
        );
        assert_eq!(resolve_edge_target("modB"), Some("modB".to_string()));
    }

    #[test]
    fn deep_reflection_denied_without_opens() {
        let mut desc_a = sample_desc("modA");
        desc_a.requires.push(ModuleRequiresEntry {
            module_name: "modB".to_string(),
            is_transitive: false,
            is_static: false,
            ..Default::default()
        });
        let desc_b = sample_desc("modB");
        let mut reg = ModuleRegistry::new();
        reg.register(desc_a, vec![]);
        reg.register(desc_b, vec!["com/secret".to_string()]);
        reg.build_readability_graph();

        // modA reads modB, but modB does not open com/secret → denied.
        assert!(reg
            .check_deep_reflection_access("modA", "modB", "com/secret")
            .is_err());
    }

    #[test]
    fn deep_reflection_allowed_with_opens() {
        let mut desc_a = sample_desc("modA");
        desc_a.requires.push(ModuleRequiresEntry {
            module_name: "modB".to_string(),
            is_transitive: false,
            is_static: false,
            ..Default::default()
        });
        let mut desc_b = sample_desc("modB");
        desc_b.opens.push(ModuleOpensEntry {
            package_name: "com/secret".to_string(),
            to_modules: vec!["modA".to_string()],
            ..Default::default()
        });
        let mut reg = ModuleRegistry::new();
        reg.register(desc_a, vec![]);
        reg.register(desc_b, vec!["com/secret".to_string()]);
        reg.build_readability_graph();

        assert!(reg
            .check_deep_reflection_access("modA", "modB", "com/secret")
            .is_ok());
    }

    #[test]
    fn deep_reflection_dynamic_add_opens() {
        let mut desc_a = sample_desc("modA");
        desc_a.requires.push(ModuleRequiresEntry {
            module_name: "modB".to_string(),
            is_transitive: false,
            is_static: false,
            ..Default::default()
        });
        let desc_b = sample_desc("modB");
        let mut reg = ModuleRegistry::new();
        reg.register(desc_a, vec![]);
        reg.register(desc_b, vec!["com/secret".to_string()]);
        reg.build_readability_graph();

        // Initially denied.
        assert!(reg
            .check_deep_reflection_access("modA", "modB", "com/secret")
            .is_err());

        // Dynamic addOpens.
        reg.add_opens("modB", "com/secret", "modA");
        assert!(reg
            .check_deep_reflection_access("modA", "modB", "com/secret")
            .is_ok());
    }

    // -----------------------------------------------------------------------
    // Phase B: Dynamic export affects check_module_access
    // -----------------------------------------------------------------------

    #[test]
    fn dynamic_export_enables_access() {
        let mut desc_a = sample_desc("modA");
        desc_a.requires.push(ModuleRequiresEntry {
            module_name: "modB".to_string(),
            is_transitive: false,
            is_static: false,
            ..Default::default()
        });
        let desc_b = sample_desc("modB"); // no exports
        let mut reg = ModuleRegistry::new();
        reg.register(desc_a, vec![]);
        reg.register(desc_b, vec!["com/internal".to_string()]);
        reg.build_readability_graph();

        // modA reads modB, but modB doesn't export com/internal → denied.
        assert!(reg
            .check_module_access("modA", "modB", "com/internal")
            .is_err());

        // Dynamic addExports.
        reg.add_exports("modB", "com/internal", "modA");
        assert!(reg
            .check_module_access("modA", "modB", "com/internal")
            .is_ok());
    }

    // -----------------------------------------------------------------------
    // `--module-path` resolution
    // -----------------------------------------------------------------------

    #[test]
    fn package_segment_rejects_non_identifiers() {
        // The filter that keeps `META-INF` (and `build-modules`-style names)
        // out of a module's package set.
        assert!(is_package_segment("svc"));
        assert!(is_package_segment("_x"));
        assert!(is_package_segment("$y"));
        assert!(is_package_segment("a1"));
        assert!(!is_package_segment("META-INF"));
        assert!(!is_package_segment("1abc"));
        assert!(!is_package_segment(""));
        // Wave 103: `Checks.RESERVED` words are no identifiers.
        assert!(!is_package_segment("int"));
        assert!(!is_package_segment("_"));
        assert!(!is_package_segment("null"));
    }

    /// Interpreter round i5 wave 103, lane L5: `Checks.requireModuleName`'s
    /// text, and `JarFile.getBasename`'s versioned view.
    #[test]
    fn i103_module_name_refusal_and_versioned_names_follow_the_jdk() {
        assert_eq!(module_name_refusal("foo.bar"), None);
        assert_eq!(
            module_name_refusal("foo.1"),
            Some("foo.1: Invalid module name: '1' is not a Java identifier".to_string())
        );
        assert_eq!(
            module_name_refusal(""),
            Some(": Invalid module name: '' is not a Java identifier".to_string())
        );
        assert_eq!(
            module_name_refusal("a.int"),
            Some("a.int: Invalid module name: 'int' is not a Java identifier".to_string())
        );
        let v = |n: &'static str, mr: bool| versioned_entry_name(n, mr, 21);
        assert_eq!(v("p/A.class", true), Some("p/A.class"));
        assert_eq!(v("META-INF/versions/11/q/B.class", true), Some("q/B.class"));
        assert_eq!(v("META-INF/versions/21/q/B.class", true), Some("q/B.class"));
        assert_eq!(v("META-INF/versions/22/q/B.class", true), None);
        assert_eq!(v("META-INF/versions/x/q/B.class", true), None);
        assert_eq!(v("META-INF/versions/11/", true), None);
        assert_eq!(
            v("META-INF/versions/11/q/B.class", false),
            Some("META-INF/versions/11/q/B.class")
        );
        // Wave 104: only what `getVersionedEntry` probes contributes by itself
        // (canonical versions 8..=feature, no `META-INF/` base); feature 8 is
        // the base view.
        assert_eq!(v("META-INF/versions/8/q/B.class", true), Some("q/B.class"));
        assert_eq!(v("META-INF/versions/5/q/B.class", true), None);
        assert_eq!(v("META-INF/versions/09/q/B.class", true), None);
        assert_eq!(v("META-INF/versions/+9/q/B.class", true), None);
        assert_eq!(v("META-INF/versions/11/META-INF/services/S", true), None);
        assert_eq!(versioned_entry_name("META-INF/versions/8/q/B.class", true, 8), None);
    }

    /// Interpreter round i5 wave 104, lane L5: a module-path jar is read at the
    /// version the VM passes (the booted library's), not always 25.
    #[test]
    fn i104_module_path_jars_are_read_at_the_given_feature() {
        use std::io::Write as _;
        let base = std::env::temp_dir().join(format!(
            "cratonvm_i104_module_path_feature_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let path = base.join("mrf-1.0.jar");
        {
            let out = std::fs::File::create(&path).unwrap();
            let mut zip = zip::ZipWriter::new(out);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
            zip.write_all(b"Manifest-Version: 1.0\r\nMulti-Release: true\r\n\r\n")
                .unwrap();
            for e in ["p/A.class", "META-INF/versions/21/q/B.class", "META-INF/versions/22/r/C.class"]
            {
                zip.start_file(e, opts).unwrap();
                zip.write_all(b"\xCA\xFE\xBA\xBE").unwrap();
            }
            zip.finish().unwrap();
        }
        let entries = vec![path.to_string_lossy().into_owned()];
        let packages = |feature: u32| {
            scan_module_path_at(&entries, feature)
                .into_iter()
                .next()
                .map(|m| m.packages)
                .unwrap_or_default()
        };
        assert_eq!(packages(25), vec!["p".to_string(), "q".to_string(), "r".to_string()]);
        assert_eq!(packages(21), vec!["p".to_string(), "q".to_string()]);
        assert_eq!(packages(8), vec!["p".to_string()]);
        assert_eq!(scan_module_path(&entries)[0].packages, packages(25));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Interpreter round i5 wave 103, lane L5: the module path refusals
    /// HotSpot's boot layer raises as `FindException`, and a multi-release
    /// automatic jar's versioned-only package.
    #[test]
    fn i103_module_path_find_errors_and_multi_release_packages() {
        use std::io::Write as _;
        let base = std::env::temp_dir().join(format!(
            "cratonvm_i103_module_path_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let jar = |dir: &std::path::Path, file: &str, manifest: &str, entries: &[&str]| {
            std::fs::create_dir_all(dir).unwrap();
            let out = std::fs::File::create(dir.join(file)).unwrap();
            let mut zip = zip::ZipWriter::new(out);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
            zip.write_all(manifest.as_bytes()).unwrap();
            for e in entries {
                zip.start_file(*e, opts).unwrap();
                zip.write_all(b"\xCA\xFE\xBA\xBE").unwrap();
            }
            zip.finish().unwrap();
        };
        let entry = |p: &std::path::Path| vec![p.to_string_lossy().into_owned()];

        // A good directory: no error; the multi-release jar's versioned-only
        // package counts.
        let good = base.join("good");
        jar(
            &good,
            "mr-1.0.jar",
            "Manifest-Version: 1.0\r\nMulti-Release: true\r\n\r\n",
            &["p/A.class", "META-INF/versions/11/q/B.class", "META-INF/versions/99/r/C.class"],
        );
        assert_eq!(module_path_find_error(&entry(&good)), None);
        let m = automatic_jar_module(&good.join("mr-1.0.jar"), MODULE_PATH_JAR_FEATURE).unwrap();
        assert_eq!(m.descriptor.name, "mr");
        assert_eq!(m.packages, vec!["p".to_string(), "q".to_string()]);

        // Two versions of one module in one directory.
        let dup = base.join("dup");
        jar(&dup, "lib-1.0.jar", "Manifest-Version: 1.0\r\n\r\n", &["l/A.class"]);
        jar(&dup, "lib-2.0.jar", "Manifest-Version: 1.0\r\n\r\n", &["l/A.class"]);
        let text = module_path_find_error(&entry(&dup)).unwrap();
        // The pair comes in `read_dir` order (the JDK's): second listed first.
        let listed: Vec<String> = std::fs::read_dir(&dup)
            .unwrap()
            .flatten()
            .map(|c| c.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            text.starts_with("Two versions of module lib found in ")
                && text.ends_with(&format!(" ({} and {})", listed[1], listed[0])),
            "{text}"
        );
        // ... but not across two entries.
        let other = base.join("other");
        jar(&other, "lib-3.0.jar", "Manifest-Version: 1.0\r\n\r\n", &["l/A.class"]);
        let two = vec![
            good.join("mr-1.0.jar").to_string_lossy().into_owned(),
            other.to_string_lossy().into_owned(),
            good.to_string_lossy().into_owned(),
        ];
        assert_eq!(module_path_find_error(&two), None);

        // An illegal derived name, an illegal Automatic-Module-Name, a
        // top-level class.
        let bad = base.join("bad");
        jar(&bad, "1x.jar", "Manifest-Version: 1.0\r\n\r\n", &["p/A.class"]);
        let text = module_path_find_error(&entry(&bad.join("1x.jar"))).unwrap();
        assert!(
            text.starts_with("Unable to derive module descriptor for ")
                && text.ends_with(
                    "1x.jar\nCaused by: java.lang.IllegalArgumentException: 1x: Invalid module \
                     name: '1x' is not a Java identifier"
                ),
            "{text}"
        );
        jar(
            &bad,
            "named.jar",
            "Manifest-Version: 1.0\r\nAutomatic-Module-Name: a.b-c\r\n\r\n",
            &["p/A.class"],
        );
        let text = module_path_find_error(&entry(&bad.join("named.jar"))).unwrap();
        assert!(
            text.ends_with(
                "named.jar\nCaused by: java.lang.module.FindException: Automatic-Module-Name: \
                 a.b-c: Invalid module name: 'b-c' is not a Java identifier"
            ),
            "{text}"
        );
        jar(&bad, "top.jar", "Manifest-Version: 1.0\r\n\r\n", &["Main.class", "p/A.class"]);
        let text = module_path_find_error(&entry(&bad.join("top.jar"))).unwrap();
        assert!(
            text.ends_with(
                "top.jar\nCaused by: java.lang.module.InvalidModuleDescriptorException: \
                 Main.class found in top-level directory (unnamed package not allowed in module)"
            ),
            "{text}"
        );
        assert!(automatic_jar_module(&bad.join("top.jar"), MODULE_PATH_JAR_FEATURE).is_none());

        // `META-INF/services` provides and `Main-Class`.
        let svc_jar = |file: &str, services: &str| {
            let svc = base.join("svc");
            std::fs::create_dir_all(&svc).unwrap();
            let out = std::fs::File::create(svc.join(file)).unwrap();
            let mut zip = zip::ZipWriter::new(out);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
            zip.write_all(b"Manifest-Version: 1.0\r\nMain-Class: impl.Main\r\n\r\n")
                .unwrap();
            zip.start_file("impl/Main.class", opts).unwrap();
            zip.write_all(b"\xCA\xFE\xBA\xBE").unwrap();
            zip.start_file("META-INF/services/java.lang.Runnable", opts).unwrap();
            zip.write_all(services.as_bytes()).unwrap();
            zip.finish().unwrap();
            svc.join(file)
        };
        let good_svc = svc_jar("impl-1.0.jar", "# comment\r\n impl.Main # trailing\r\n\r\n");
        let m = automatic_jar_module(&good_svc, MODULE_PATH_JAR_FEATURE).unwrap();
        assert_eq!(m.descriptor.main_class.as_deref(), Some("impl/Main"));
        assert_eq!(m.descriptor.provides.len(), 1);
        assert_eq!(m.descriptor.provides[0].service, "java/lang/Runnable");
        assert_eq!(m.descriptor.provides[0].with, vec!["impl/Main".to_string()]);
        let bad_svc = svc_jar("other-1.0.jar", "elsewhere.Impl\n");
        let text = module_path_find_error(&entry(&bad_svc)).unwrap();
        assert!(
            text.ends_with(
                "other-1.0.jar\nCaused by: java.lang.module.InvalidModuleDescriptorException: \
                 Provider class elsewhere.Impl not in JAR file other-1.0.jar"
            ),
            "{text}"
        );

        // A regular file that is no JAR.
        std::fs::write(bad.join("notes.txt"), b"x").unwrap();
        let text = module_path_find_error(&entry(&bad.join("notes.txt"))).unwrap();
        assert!(text.starts_with("Module format not recognized: "), "{text}");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn empty_module_path_resolves_nothing() {
        // The unconditional call at VM init must be a no-op for a plain
        // `-cp` launch — no filesystem probing, no modules selected.
        assert!(resolve_module_path(&[], &[]).is_empty());
        assert!(resolve_module_path(&[], &["some.module".to_string()]).is_empty());
    }

    /// Interpreter round i4 wave 99, lane L5: an automatic module's name and
    /// version from its jar's file name, as `ModulePath.deriveModuleDescriptor`
    /// derives them.
    #[test]
    fn i99_automatic_module_name_and_version_follow_the_jdk() {
        let derive = automatic_module_name_and_version;
        assert_eq!(derive("lib-3.4"), ("lib".to_string(), Some("3.4".to_string())));
        assert_eq!(
            derive("foo-bar-1.2.3-SNAPSHOT"),
            ("foo.bar".to_string(), Some("1.2.3-SNAPSHOT".to_string()))
        );
        assert_eq!(derive("foo-bar"), ("foo.bar".to_string(), None));
        assert_eq!(derive("a--b..c_"), ("a.b.c".to_string(), None));
        assert_eq!(derive("x-1a"), ("x.1a".to_string(), None));
        assert_eq!(derive("x-12"), ("x".to_string(), Some("12".to_string())));
        // Wave 103: a tail `Version.parse` refuses keeps no version.
        assert_eq!(derive("lib-1.0-"), ("lib".to_string(), None));
        assert_eq!(derive("lib-1.0+"), ("lib".to_string(), None));
        assert_eq!(derive("lib-1.0-rc+"), ("lib".to_string(), None));
        assert_eq!(derive("lib-1.0-rc+7"), ("lib".to_string(), Some("1.0-rc+7".to_string())));
        assert_eq!(derive("lib-2."), ("lib".to_string(), Some("2.".to_string())));
        let manifest = concat!(
            "Manifest-Version: 1.0\r\n",
            "automatic-module-name: org.ex\r\n ample\r\n\r\n",
            "Name: p/\r\n"
        );
        assert_eq!(
            main_manifest_attribute(manifest.as_bytes(), "Automatic-Module-Name"),
            Some("org.example".to_string())
        );
        assert_eq!(main_manifest_attribute(b"", "Automatic-Module-Name"), None);
    }

    /// Interpreter round i4 wave 99, lane L5: only a module-path module shows
    /// its version; an automatic one reads every module.
    #[test]
    fn i99_module_path_modules_show_their_version() {
        let mut reg = ModuleRegistry::new();
        let versioned = |name: &str| ModuleDescriptor {
            version: Some("1.2".to_string()),
            ..sample_desc(name)
        };
        reg.register(versioned("java.sql"), vec!["java/sql".to_string()]);
        reg.register(versioned("app"), vec!["app".to_string()]);
        reg.register(versioned("lib"), vec!["lib".to_string()]);
        reg.register(sample_desc("unv"), vec!["unv".to_string()]);
        reg.note_module_path_module("app", false);
        reg.note_module_path_module("lib", true);
        reg.note_module_path_module("unv", false);
        reg.build_readability_graph();
        assert_eq!(reg.shown_module_version("app"), Some("1.2"));
        assert_eq!(reg.shown_module_version("lib"), Some("1.2"));
        assert_eq!(reg.shown_module_version("unv"), None);
        assert_eq!(reg.shown_module_version("java.sql"), None);
        assert!(reg.is_automatic_module("lib"));
        assert!(reg.reads("lib", "app"));
        assert!(!reg.reads("app", "lib"));
    }

    /// Interpreter round i5 wave 100, lane L5: a module that reads one
    /// automatic module reads every automatic module (implied readability);
    /// one that reads none reads none of them.
    #[test]
    fn i100_reading_one_automatic_module_reads_them_all() {
        let mut reg = ModuleRegistry::new();
        let mut app = sample_desc("app");
        app.requires.push(ModuleRequiresEntry {
            module_name: "auto.a".to_string(),
            ..Default::default()
        });
        reg.register(app, vec!["app".to_string()]);
        reg.register(sample_desc("plain"), vec!["plain".to_string()]);
        reg.register(sample_desc("auto.a"), vec!["auto/a".to_string()]);
        reg.register(sample_desc("auto.b"), vec!["auto/b".to_string()]);
        reg.note_module_path_module("app", false);
        reg.note_module_path_module("plain", false);
        reg.note_module_path_module("auto.a", true);
        reg.note_module_path_module("auto.b", true);
        reg.build_readability_graph();
        assert!(reg.reads("app", "auto.a"));
        assert!(reg.reads("app", "auto.b"), "implied by reading auto.a");
        assert!(!reg.reads("plain", "auto.b"));
        assert!(!reg.reads("app", "plain"));
    }
}
