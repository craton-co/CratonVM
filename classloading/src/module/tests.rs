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
/// failed in both `--fast-boot` and `--real-jdk`.
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

/// Interpreter round i45x, lane L5: the JDK's `implIsExportedOrOpen` has no
/// readability test, so deep reflection into a package a module OPENS to
/// the accessor is admitted whether or not the accessor reads the module;
/// a refusal names the accessor as `Module.toString()` does.
#[test]
fn i45x_deep_reflection_asks_no_readability_and_names_the_accessor() {
    let desc_a = sample_desc("modA"); // requires nothing: does not read modB
    let mut desc_b = sample_desc("modB");
    desc_b.opens.push(ModuleOpensEntry {
        package_name: "com/secret".to_string(),
        to_modules: vec!["modA".to_string()],
        ..Default::default()
    });
    let mut reg = ModuleRegistry::new();
    reg.register(desc_a, vec![]);
    reg.register(
        desc_b,
        vec!["com/secret".to_string(), "com/hidden".to_string()],
    );
    reg.build_readability_graph();
    assert!(!reg.reads("modA", "modB"), "fixture: modA must not read modB");
    assert_eq!(
        reg.check_deep_reflection_access("modA", "modB", "com/secret"),
        Ok(())
    );
    assert_eq!(
        reg.check_deep_reflection_access("modA", "modB", "com/hidden"),
        Err("module modB does not \"opens com.hidden\" to module modA".to_string())
    );
    assert_eq!(
        reg.check_deep_reflection_access(UNNAMED_MODULE, "modB", "com/hidden"),
        Err("module modB does not \"opens com.hidden\" to unnamed module".to_string())
    );
}

/// Interpreter round i45x, lane L5: a package OPENED to the accessor is
/// exported to it for linkage too (the JDK tells HotSpot of every `opens`
/// as an `addExports0`), as `is_package_exported_to` and
/// `class_export_denial` already answer.
#[test]
fn i45x_linkage_admits_a_package_opened_to_the_accessor() {
    let mut desc_a = sample_desc("modA");
    desc_a.requires.push(ModuleRequiresEntry {
        module_name: "modB".to_string(),
        ..Default::default()
    });
    let mut desc_b = sample_desc("modB");
    desc_b.opens.push(ModuleOpensEntry {
        package_name: "com/opened".to_string(),
        to_modules: vec!["modA".to_string()],
        ..Default::default()
    });
    let mut reg = ModuleRegistry::new();
    reg.register(desc_a, vec![]);
    reg.register(
        desc_b,
        vec!["com/opened".to_string(), "com/closed".to_string()],
    );
    reg.build_readability_graph();
    assert_eq!(
        reg.check_module_access("modA", "modB", "com/opened"),
        Ok(())
    );
    assert!(reg
        .check_module_access("modA", "modB", "com/closed")
        .is_err());
    // A dynamic `addOpens` is an export as well.
    reg.add_opens("modB", "com/closed", "modA");
    assert_eq!(
        reg.check_module_access("modA", "modB", "com/closed"),
        Ok(())
    );
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
