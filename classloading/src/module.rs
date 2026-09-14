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
}

/// Tracks all registered modules, the package-to-module mapping, and the
/// pre-computed readability graph.
pub struct ModuleRegistry {
    /// module name → descriptor
    modules: FxHashMap<String, ModuleDescriptor>,

    /// package (slash format) → module name that owns it.
    package_to_module: FxHashMap<String, String>,

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
            modules: FxHashMap::with_capacity_and_hasher(16, Default::default()),
            package_to_module: FxHashMap::with_capacity_and_hasher(16, Default::default()),
            readable: FxHashMap::with_capacity_and_hasher(16, Default::default()),
            graph_built: false,
            extra_reads: FxHashMap::default(),
            extra_exports: FxHashMap::default(),
            extra_opens: FxHashMap::default(),
        }
    }

    /// Register a module and index its packages.
    ///
    /// If a module with the same name was already registered it is silently
    /// replaced (last writer wins — callers should avoid duplicates).
    pub fn register(&mut self, desc: ModuleDescriptor, packages: Vec<String>) {
        let name = desc.name.clone();
        // Index packages
        for pkg in packages {
            self.package_to_module
                .entry(pkg)
                .or_insert_with(|| name.clone());
        }
        self.modules.insert(name, desc);
        // Invalidate graph when new modules arrive
        self.graph_built = false;
        self.readable.clear();
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
        self.package_to_module.get(pkg).map(|s| s.as_str())
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

        if self.graph_built {
            return self
                .readable
                .get(reader)
                .is_some_and(|set| set.contains(provider));
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
        false
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
            .filter(|(_, m)| m.as_str() == module_name)
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
    /// docs/known-issues/jdk-only/H24-1-the-module-source-door-and-the-two-modules-a-boot-layer-probe-could-not-see-20260821.md
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
    /// `docs/known-issues/jdk-only/W4-2-unnamed-accessor-bypasses-encapsulation.md`.
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
        Attribute::Module {
            name_index,
            flags,
            version_index,
            requires,
            exports,
            opens,
            uses,
            provides,
        } => (
            name_index,
            flags,
            version_index,
            requires,
            exports,
            opens,
            uses,
            provides,
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
    let desc = class_file.attributes.iter().find_map(|a| {
        a.as_decoded()
            .and_then(|d| descriptor_from_module_attribute(d, &class_file.constant_pool))
    })?;
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

/// Is `segment` usable as one dot-separated component of a package name?
///
/// The real JDK drops directories that cannot be package components rather
/// than inventing an illegal package name — this is what keeps `META-INF`
/// (illegal: contains `-`) out of a module's package set.
fn is_package_segment(segment: &str) -> bool {
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
fn modular_jar_module(path: &std::path::Path) -> Option<(ModuleDescriptor, Vec<String>)> {
    let file = std::fs::File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;

    // Collect names first: `by_name` needs `&mut archive`, so the borrow of
    // `file_names()` must be finished before the read below.
    let names: Vec<String> = archive.file_names().map(|n| n.to_string()).collect();

    let mut bytes = Vec::new();
    {
        use std::io::Read as _;
        let mut entry = archive.by_name("module-info.class").ok()?;
        entry.read_to_end(&mut bytes).ok()?;
    }
    let (descriptor, declared) = parse_module_info(&bytes)?;

    let packages = if declared.is_empty() {
        let mut pkgs: Vec<String> = Vec::new();
        for name in &names {
            if name.ends_with('/') {
                continue;
            }
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
fn module_at(path: &std::path::Path) -> Option<ModulePathModule> {
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
        });
    }
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if ext.eq_ignore_ascii_case("jar") {
        let (mut descriptor, packages) = modular_jar_module(path)?;
        descriptor.automatic = false;
        return Some(ModulePathModule {
            root,
            descriptor,
            packages,
        });
    }
    None
}

/// Every module *observable* on `entries` — i.e. present on the module path,
/// whether or not `--add-modules` selects it.
///
/// A `--module-path` entry is either a module root itself (a directory holding
/// `module-info.class`, or a modular JAR) or a directory *of* module roots;
/// `java` accepts both spellings and so does this.
pub fn scan_module_path(entries: &[String]) -> Vec<ModulePathModule> {
    let mut found: Vec<ModulePathModule> = Vec::new();
    for entry in entries {
        let path = std::path::Path::new(entry);
        if let Some(m) = module_at(path) {
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
            if let Some(m) = module_at(&child) {
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
    if entries.is_empty() {
        return Vec::new();
    }
    let observable = scan_module_path(entries);
    if observable.is_empty() {
        return Vec::new();
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
        }
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
            "the descriptor itself stays in the registry — service discovery              and labelling still want it"
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
    }

    #[test]
    fn empty_module_path_resolves_nothing() {
        // The unconditional call at VM init must be a no-op for a plain
        // `-cp` launch — no filesystem probing, no modules selected.
        assert!(resolve_module_path(&[], &[]).is_empty());
        assert!(resolve_module_path(&[], &["some.module".to_string()]).is_empty());
    }
}
