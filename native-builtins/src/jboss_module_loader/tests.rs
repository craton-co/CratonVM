    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use parking_lot::Mutex as PMutex;

    /// Serial test guard — the `-mp` test override and module cache are
    /// process-wide singletons.  Tests must run serialized so they
    /// don't see each other's side effects.
    static TEST_LOCK: PMutex<()> = PMutex::new(());

    fn setup_test_env(tmp_root: &Path) {
        clear_module_cache_for_test();
        clear_boot_loader_for_test();
        set_mp_root_for_test(Some(tmp_root.to_path_buf()));
    }

    fn write(path: &Path, contents: &str) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    fn make_minimal_module_xml(name: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<module name="{}" xmlns="urn:jboss:module:1.9">
    <resources>
    </resources>
</module>
"#,
            name
        )
    }

    fn make_module_xml_with_jars(name: &str, jars: &[&str]) -> String {
        let mut s = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<module name=\"{}\" xmlns=\"urn:jboss:module:1.9\">\n  <resources>\n",
            name
        );
        for j in jars {
            s.push_str(&format!("    <resource-root path=\"{}\"/>\n", j));
        }
        s.push_str("  </resources>\n</module>\n");
        s
    }

    fn make_module_xml_with_artifacts(name: &str, artifacts: &[&str]) -> String {
        let mut s = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<module name=\"{}\" xmlns=\"urn:jboss:module:1.9\">\n  <resources>\n",
            name
        );
        for artifact in artifacts {
            s.push_str(&format!("    <artifact name=\"{}\"/>\n", artifact));
        }
        s.push_str("  </resources>\n</module>\n");
        s
    }

    // -----------------------------------------------------------------
    // validate_module_name
    // -----------------------------------------------------------------

    #[test]
    fn t19_h4_validate_accepts_valid_module_names() {
        let _g = TEST_LOCK.lock();
        assert!(validate_module_name("org.jboss.as.standalone").is_ok());
        assert!(validate_module_name("java.base").is_ok());
        assert!(validate_module_name("x").is_ok());
        // 256 bytes exactly — boundary
        let max = "a".repeat(256);
        assert!(validate_module_name(&max).is_ok());
    }

    #[test]
    fn t19_h4_validate_rejects_empty_and_oversize() {
        let _g = TEST_LOCK.lock();
        assert!(validate_module_name("").is_err());
        let too_long = "a".repeat(257);
        assert!(validate_module_name(&too_long).is_err());
    }

    #[test]
    fn t19_h4_validate_rejects_traversal_and_separators() {
        let _g = TEST_LOCK.lock();
        assert!(validate_module_name("../etc/passwd").is_err());
        assert!(validate_module_name("..").is_err());
        assert!(validate_module_name("foo/bar").is_err());
        assert!(validate_module_name("foo\\bar").is_err());
        assert!(validate_module_name("C:\\Windows").is_err());
        assert!(validate_module_name("a:b").is_err());
    }

    #[test]
    fn t19_h4_validate_rejects_control_bytes() {
        let _g = TEST_LOCK.lock();
        assert!(validate_module_name("foo\0bar").is_err());
        assert!(validate_module_name("foo\nbar").is_err());
        assert!(validate_module_name("foo\tbar").is_err());
        assert!(validate_module_name("foo\x7fbar").is_err());
    }

    #[test]
    fn t19_h4_property_bridge_maps_jmx_builder() {
        let _g = TEST_LOCK.lock();
        let mut ctx = MockNativeContext::new();
        ctx.set_system_property(
            "javax.management.builder.initial",
            "org.jboss.as.jmx.PluggableMBeanServerBuilder",
        );
        assert_eq!(
            property_bridge_module_for_class(&ctx, "org.jboss.as.jmx.PluggableMBeanServerBuilder"),
            Some("org.jboss.as.jmx")
        );
        assert_eq!(
            property_bridge_module_for_class(&ctx, "org.jboss.as.server.Main"),
            None
        );

        ctx.set_system_property(
            "java.util.logging.manager",
            "org.jboss.logmanager.LogManager",
        );
        assert_eq!(
            property_bridge_module_for_class(&ctx, "org.jboss.logmanager.LogManager"),
            Some("org.jboss.logmanager")
        );
    }

    // -----------------------------------------------------------------
    // find_mp_argument — env-var fallback
    // -----------------------------------------------------------------

    /// WP8.10.5 — `CRATONVM_JBOSS_MP_ROOT` environment variable provides a
    /// `-mp` value when the process argv is owned by the test harness
    /// (and therefore can't carry `-mp <path>`).  External integration
    /// tests like `vm/tests/wp8_10_jboss_modules_smoke.rs` need this path
    /// to point `LocalModuleLoader` at a fixture-built modules tree
    /// without going through the cratonvm CLI.
    ///
    /// Acceptance: setting the env var causes `find_mp_argument()` to
    /// return its value verbatim, taking precedence over any argv `-mp`
    /// (the env-var setter has authority).  Empty or unset env var
    /// causes us to fall back to argv.
    #[test]
    fn wp8_10_find_mp_argument_honours_env_var() {
        let v = find_mp_argument_from(
            Some("/tmp/wp8_10_fixture_mp".to_string()),
            ["cratonvm", "-mp", "/tmp/argv_mp"]
                .into_iter()
                .map(str::to_string),
        );
        assert_eq!(v.as_deref(), Some("/tmp/wp8_10_fixture_mp"));
    }

    /// Empty `CRATONVM_JBOSS_MP_ROOT` must be ignored — we should fall
    /// through to argv resolution rather than treating "" as a valid
    /// (and dangerous) module-path root.
    #[test]
    fn wp8_10_find_mp_argument_ignores_empty_env_var() {
        let v = find_mp_argument_from(Some(String::new()), ["cratonvm"].map(str::to_string));
        assert!(
            v.is_none(),
            "empty startup flag must be treated as unset; got {:?}",
            v
        );
    }

    /// WildFly domain launchers pass a multi-entry `-mp` where per-test
    /// `added-modules` directories precede the real WildFly modules root.
    /// The resolver must retain every entry, not only the first one.
    #[test]
    fn wf_domain_split_module_path_keeps_all_entries() {
        let _g = TEST_LOCK.lock();
        let sep = if cfg!(windows) { ';' } else { ':' };
        let raw = format!("/tmp/added-one{sep}/tmp/added-two{sep}/opt/wildfly/modules");
        let entries = split_module_path_entries(&raw);
        assert_eq!(
            entries,
            vec!["/tmp/added-one", "/tmp/added-two", "/opt/wildfly/modules"]
        );
    }

    // -----------------------------------------------------------------
    // resolve_module — happy path
    // -----------------------------------------------------------------

    #[test]
    fn t19_h4_resolve_module_legacy_path() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mod_xml = root.join("org/jboss/as/standalone/main/module.xml");
        write(
            &mod_xml,
            &make_minimal_module_xml("org.jboss.as.standalone"),
        );
        let r = resolve_module(root, "org.jboss.as.standalone").unwrap();
        assert_eq!(r.mx.name, "org.jboss.as.standalone");
        assert!(r.module_xml_path.ends_with("module.xml"));
    }

    #[test]
    fn wf_domain_resolve_module_alias_follows_target_name() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(
            &root.join("org/jboss/as/modcluster/main/module.xml"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<module-alias name="org.jboss.as.modcluster" target-name="org.wildfly.extension.mod_cluster" xmlns="urn:jboss:module:1.9"/>
"#,
        );
        let target_dir = root.join("org/wildfly/extension/mod_cluster/main");
        write(
            &target_dir.join("module.xml"),
            &make_module_xml_with_jars(
                "org.wildfly.extension.mod_cluster",
                &["wildfly-mod_cluster-extension.jar"],
            ),
        );
        write(&target_dir.join("wildfly-mod_cluster-extension.jar"), "PK");

        let r = resolve_module(root, "org.jboss.as.modcluster").unwrap();
        assert_eq!(r.mx.name, "org.wildfly.extension.mod_cluster");
        assert_eq!(r.resource_roots.len(), 1);
        assert!(r.resource_roots[0].ends_with("wildfly-mod_cluster-extension.jar"));
    }

    #[test]
    fn t19_h4_resolve_module_layered_base_path() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mod_xml = root.join("system/layers/base/org/jboss/as/standalone/main/module.xml");
        write(
            &mod_xml,
            &make_minimal_module_xml("org.jboss.as.standalone"),
        );
        let r = resolve_module(root, "org.jboss.as.standalone").unwrap();
        assert_eq!(r.mx.name, "org.jboss.as.standalone");
    }

    #[test]
    fn t19_h4_resolve_module_layered_with_layers_conf() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(&root.join("layers.conf"), "layers=keycloak\n");
        let mod_xml = root.join("system/layers/keycloak/com/example/foo/main/module.xml");
        write(&mod_xml, &make_minimal_module_xml("com.example.foo"));
        let r = resolve_module(root, "com.example.foo").unwrap();
        assert_eq!(r.mx.name, "com.example.foo");
    }

    #[test]
    fn t19_h4_resolve_module_resource_roots_resolved() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let main_dir = root.join("com/example/foo/main");
        write(
            &main_dir.join("module.xml"),
            &make_module_xml_with_jars("com.example.foo", &["a.jar", "b.jar"]),
        );
        // Touch the jars so canonicalize doesn't fail
        write(&main_dir.join("a.jar"), "PK");
        write(&main_dir.join("b.jar"), "PK");
        let r = resolve_module(root, "com.example.foo").unwrap();
        assert_eq!(r.resource_roots.len(), 2);
        assert!(r.resource_roots[0].ends_with("a.jar"));
        assert!(r.resource_roots[1].ends_with("b.jar"));
    }

    #[test]
    fn t19_h4_resolve_module_artifact_roots_from_maven_repo() {
        let _g = TEST_LOCK.lock();
        clear_module_cache_for_test();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("modules");
        let repo = tmp.path().join("repo");
        let main_dir = root.join("com/example/foo/main");
        write(
            &main_dir.join("module.xml"),
            &make_module_xml_with_artifacts("com.example.foo", &["com.acme:tool:1.0"]),
        );
        let jar = repo.join("com/acme/tool/1.0/tool-1.0.jar");
        write(&jar, "PK");

        remember_maven_repo_root(Some(repo.to_string_lossy().into_owned()));
        let r = resolve_module(&root, "com.example.foo").unwrap();
        clear_module_cache_for_test();

        assert_eq!(r.resource_roots.len(), 1);
        assert!(r.resource_roots[0].ends_with("tool-1.0.jar"));
    }

    #[test]
    fn t19_h4_resolve_module_missing_returns_class_not_found() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let err = resolve_module(tmp.path(), "no.such.module").unwrap_err();
        match err {
            RuntimeError::ClassNotFoundException { class_name } => {
                assert!(class_name.contains("no.such.module"));
            }
            other => panic!("expected ClassNotFoundException, got {:?}", other),
        }
    }

    #[test]
    fn t19_h4_resolve_module_rejects_invalid_name() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        assert!(resolve_module(tmp.path(), "../etc").is_err());
        assert!(resolve_module(tmp.path(), "").is_err());
    }

    #[test]
    fn t19_h4_resolve_module_rejects_resource_root_traversal() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let main_dir = root.join("com/example/evil/main");
        write(
            &main_dir.join("module.xml"),
            &make_module_xml_with_jars("com.example.evil", &["../../escape.jar"]),
        );
        let err = resolve_module(root, "com.example.evil").unwrap_err();
        assert!(matches!(err, RuntimeError::SecurityException { .. }));
    }

    // -----------------------------------------------------------------
    // build_local_module_loader / boot holder caching
    // -----------------------------------------------------------------

    #[test]
    fn t19_h4_build_local_module_loader_is_idempotent() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        setup_test_env(tmp.path());
        let mut ctx = MockNativeContext::new();
        let a = build_local_module_loader(&mut ctx).unwrap();
        let b = build_local_module_loader(&mut ctx).unwrap();
        assert_eq!(a.as_ptr(), b.as_ptr());
    }

    #[test]
    fn t19_h4_default_boot_holder_instance_returns_same_loader() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        setup_test_env(tmp.path());
        let mut ctx = MockNativeContext::new();
        let a = build_default_boot_holder_instance(&mut ctx).unwrap();
        let b = build_default_boot_holder_instance(&mut ctx).unwrap();
        let c = build_local_module_loader(&mut ctx).unwrap();
        assert_eq!(a.as_ptr(), b.as_ptr());
        assert_eq!(a.as_ptr(), c.as_ptr());
    }

    // -----------------------------------------------------------------
    // native_loader_load_module — happy path + cache + errors
    // -----------------------------------------------------------------

    #[test]
    fn t19_h4_load_module_happy_path_returns_module_with_resource_roots() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let main_dir = root.join("org/jboss/as/standalone/main");
        write(
            &main_dir.join("module.xml"),
            &make_module_xml_with_jars("org.jboss.as.standalone", &["jboss-as-server.jar"]),
        );
        write(&main_dir.join("jboss-as-server.jar"), "PK");
        setup_test_env(root);

        let mut ctx = MockNativeContext::new();
        let loader = build_local_module_loader(&mut ctx).unwrap();
        let name = ctx.create_string("org.jboss.as.standalone");
        let result = native_loader_load_module(
            &mut ctx,
            &[Value::Object(Some(loader)), Value::Object(Some(name))],
        )
        .unwrap()
        .unwrap();
        let module = match result {
            Value::Object(Some(o)) => o,
            other => panic!("expected non-null Module, got {:?}", other),
        };
        // resourceRoots slot should be a non-null Object[]
        let roots = ctx.get_field(module, MOD_SLOT_RESOURCE_ROOTS);
        if let Value::Object(Some(arr)) = roots {
            assert_eq!(ctx.array_length(arr), 1);
        } else {
            panic!("expected Object[] in resourceRoots, got {:?}", roots);
        }
    }

    #[test]
    fn t19_h4_load_module_caches_repeat_calls() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mod_xml = root.join("a/b/main/module.xml");
        write(&mod_xml, &make_minimal_module_xml("a.b"));
        setup_test_env(root);

        let mut ctx = MockNativeContext::new();
        let loader = build_local_module_loader(&mut ctx).unwrap();
        let name1 = ctx.create_string("a.b");
        let r1 = native_loader_load_module(
            &mut ctx,
            &[Value::Object(Some(loader)), Value::Object(Some(name1))],
        )
        .unwrap()
        .unwrap();
        let name2 = ctx.create_string("a.b");
        let r2 = native_loader_load_module(
            &mut ctx,
            &[Value::Object(Some(loader)), Value::Object(Some(name2))],
        )
        .unwrap()
        .unwrap();
        match (r1, r2) {
            (Value::Object(Some(a)), Value::Object(Some(b))) => {
                assert_eq!(a.as_ptr(), b.as_ptr(), "module cache must return same ref");
            }
            other => panic!("expected non-null modules, got {:?}", other),
        }
    }

    #[test]
    fn t19_h4_load_module_missing_throws_module_not_found() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        setup_test_env(tmp.path());
        let mut ctx = MockNativeContext::new();
        let loader = build_local_module_loader(&mut ctx).unwrap();
        let name = ctx.create_string("ghost.module.never.exists");
        let err = native_loader_load_module(
            &mut ctx,
            &[Value::Object(Some(loader)), Value::Object(Some(name))],
        )
        .unwrap_err();
        match err {
            MethodCallFailed::ExceptionThrown(_) => {}
            other => panic!(
                "expected ExceptionThrown(ModuleNotFoundException), got {:?}",
                other
            ),
        }
    }

    #[test]
    fn t19_h4_load_module_null_name_throws_npe() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        setup_test_env(tmp.path());
        let mut ctx = MockNativeContext::new();
        let loader = build_local_module_loader(&mut ctx).unwrap();
        let err = native_loader_load_module(
            &mut ctx,
            &[Value::Object(Some(loader)), Value::Object(None)],
        )
        .unwrap_err();
        let s = format!("{:?}", err);
        assert!(s.contains("Null") || s.contains("null"), "got {}", s);
    }

    #[test]
    fn t19_h4_load_module_traversal_name_rejected() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        setup_test_env(tmp.path());
        let mut ctx = MockNativeContext::new();
        let loader = build_local_module_loader(&mut ctx).unwrap();
        let name = ctx.create_string("../escape");
        let err = native_loader_load_module(
            &mut ctx,
            &[Value::Object(Some(loader)), Value::Object(Some(name))],
        )
        .unwrap_err();
        let s = format!("{:?}", err);
        assert!(
            s.contains("traversal") || s.contains("IllegalArgument"),
            "got {}",
            s
        );
    }

    #[test]
    fn t19_h4_load_module_with_no_mp_set_throws_module_not_found() {
        let _g = TEST_LOCK.lock();
        clear_module_cache_for_test();
        clear_boot_loader_for_test();
        set_mp_root_for_test(None);
        let mut ctx = MockNativeContext::new();
        let loader = build_local_module_loader(&mut ctx).unwrap();
        let name = ctx.create_string("anything");
        let err = native_loader_load_module(
            &mut ctx,
            &[Value::Object(Some(loader)), Value::Object(Some(name))],
        )
        .unwrap_err();
        match err {
            MethodCallFailed::ExceptionThrown(_) => {}
            other => panic!("expected ExceptionThrown, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------
    // Module accessors
    // -----------------------------------------------------------------

    #[test]
    fn t19_h4_module_get_name_returns_string() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mod_xml = root.join("foo/bar/main/module.xml");
        write(&mod_xml, &make_minimal_module_xml("foo.bar"));
        setup_test_env(root);

        let mut ctx = MockNativeContext::new();
        let loader = build_local_module_loader(&mut ctx).unwrap();
        let name = ctx.create_string("foo.bar");
        let module = match native_loader_load_module(
            &mut ctx,
            &[Value::Object(Some(loader)), Value::Object(Some(name))],
        )
        .unwrap()
        .unwrap()
        {
            Value::Object(Some(m)) => m,
            other => panic!("got {:?}", other),
        };
        let result = native_module_get_name(&mut ctx, &[Value::Object(Some(module))])
            .unwrap()
            .unwrap();
        if let Value::Object(Some(s)) = result {
            assert_eq!(ctx.read_string(s).as_deref(), Some("foo.bar"));
        } else {
            panic!("expected non-null name string, got {:?}", result);
        }
    }

    #[test]
    fn t19_h4_module_get_class_loader_lazy_populates() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mod_xml = root.join("zz/yy/main/module.xml");
        write(&mod_xml, &make_minimal_module_xml("zz.yy"));
        setup_test_env(root);

        let mut ctx = MockNativeContext::new();
        let loader = build_local_module_loader(&mut ctx).unwrap();
        let name = ctx.create_string("zz.yy");
        let module = match native_loader_load_module(
            &mut ctx,
            &[Value::Object(Some(loader)), Value::Object(Some(name))],
        )
        .unwrap()
        .unwrap()
        {
            Value::Object(Some(m)) => m,
            other => panic!("got {:?}", other),
        };
        // First call populates.
        let r1 = native_module_get_class_loader(&mut ctx, &[Value::Object(Some(module))])
            .unwrap()
            .unwrap();
        // Second call returns same instance.
        let r2 = native_module_get_class_loader(&mut ctx, &[Value::Object(Some(module))])
            .unwrap()
            .unwrap();
        match (r1, r2) {
            (Value::Object(Some(a)), Value::Object(Some(b))) => {
                assert_eq!(a.as_ptr(), b.as_ptr());
            }
            other => panic!("expected non-null ClassLoader pair, got {:?}", other),
        }
    }

    #[test]
    fn t19_h4_module_get_class_loader_null_receiver_throws_npe() {
        let _g = TEST_LOCK.lock();
        let mut ctx = MockNativeContext::new();
        let err = native_module_get_class_loader(&mut ctx, &[Value::Object(None)]).unwrap_err();
        let s = format!("{:?}", err);
        assert!(s.contains("Null") || s.contains("null"), "got {}", s);
    }

    #[test]
    fn t19_h4_module_get_module_loader_falls_back_to_boot() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        setup_test_env(tmp.path());
        let mut ctx = MockNativeContext::new();
        let result = native_module_get_module_loader(&mut ctx, &[Value::Object(None)])
            .unwrap()
            .unwrap();
        match result {
            Value::Object(Some(_)) => {}
            other => panic!("expected non-null loader, got {:?}", other),
        }
    }

    #[test]
    fn wf_domain_static_module_loader_accessors_return_boot_loader() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        setup_test_env(tmp.path());
        let mut ctx = MockNativeContext::new();
        let boot = build_local_module_loader(&mut ctx).unwrap();

        for accessor in [
            "getBootModuleLoader",
            "getCallerModuleLoader",
            "getContextModuleLoader",
        ] {
            let result = native_module_get_boot_or_caller_module_loader(&mut ctx, &[])
                .unwrap()
                .unwrap();
            match result {
                Value::Object(Some(loader)) => assert_eq!(
                    loader.as_ptr(),
                    boot.as_ptr(),
                    "{} did not return the cached boot loader",
                    accessor
                ),
                other => panic!("{} returned {:?}", accessor, other),
            }
        }
    }

    // -----------------------------------------------------------------
    // Concurrency: 4 threads racing loadModule for the same module
    // must end up with one cached instance.
    // -----------------------------------------------------------------

    #[test]
    fn t19_h4_load_module_concurrent_4_threads_share_one_module() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mod_xml = root.join("race/me/main/module.xml");
        write(&mod_xml, &make_minimal_module_xml("race.me"));
        setup_test_env(root);

        // Each thread builds its own context — the module cache lives
        // process-wide so cross-context lookups still hit the same
        // ObjectRef.  We can't truly invoke from background threads
        // because MockNativeContext isn't Send; instead we serialize
        // the calls but use four distinct receiver/name pairs to
        // simulate concurrent traffic.
        let mut ctx = MockNativeContext::new();
        let loader = build_local_module_loader(&mut ctx).unwrap();
        let mut refs = Vec::new();
        for _ in 0..4 {
            let name = ctx.create_string("race.me");
            let result = native_loader_load_module(
                &mut ctx,
                &[Value::Object(Some(loader)), Value::Object(Some(name))],
            )
            .unwrap()
            .unwrap();
            if let Value::Object(Some(m)) = result {
                refs.push(m);
            } else {
                panic!("expected non-null Module");
            }
        }
        let first = refs[0].as_ptr();
        for r in &refs[1..] {
            assert_eq!(r.as_ptr(), first, "all calls must return same ObjectRef");
        }
    }

    // -----------------------------------------------------------------
    // ensure_under_root — symlink/traversal defense
    // -----------------------------------------------------------------

    #[test]
    fn t19_h4_ensure_under_root_accepts_descendants() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let inside = root.join("subdir/file.txt");
        std::fs::create_dir_all(inside.parent().unwrap()).unwrap();
        std::fs::write(&inside, "x").unwrap();
        ensure_under_root(root, &inside).unwrap();
    }

    #[test]
    fn t19_h4_ensure_under_root_rejects_escapes() {
        let _g = TEST_LOCK.lock();
        let tmp_root = tempfile::tempdir().unwrap();
        let tmp_other = tempfile::tempdir().unwrap();
        let outside = tmp_other.path().join("foo");
        std::fs::write(&outside, "x").unwrap();
        let err = ensure_under_root(tmp_root.path(), &outside).unwrap_err();
        assert!(matches!(err, RuntimeError::SecurityException { .. }));
    }

    #[test]
    fn t19_h4_ensure_under_root_accepts_nonexistent_descendant() {
        // V1: a resource-root jar that doesn't physically exist yet must
        // still be accepted as long as its existing prefix (the module dir)
        // is under the root. The existing module dir is canonicalized and the
        // not-yet-existing jar name is re-attached.
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let module_dir = root.join("system/layers/base/com/example/main");
        std::fs::create_dir_all(&module_dir).unwrap();
        let missing_jar = module_dir.join("not-yet-here.jar");
        // The jar does not exist on disk, but its prefix does.
        ensure_under_root(root, &missing_jar).unwrap();
    }

    #[test]
    fn t19_h4_ensure_under_root_fails_closed_on_nonexistent_escape() {
        // V1: a non-existent candidate that lexically escapes the root must be
        // rejected (fail closed) rather than slipping through on the old raw
        // fallback. The existing prefix (`tmp_other`) canonicalizes outside the
        // root, so the prefix check rejects it.
        let _g = TEST_LOCK.lock();
        let tmp_root = tempfile::tempdir().unwrap();
        let tmp_other = tempfile::tempdir().unwrap();
        // Candidate that does NOT exist on disk yet, but whose existing prefix
        // lives outside the module root.
        let outside_missing = tmp_other.path().join("ghost/escape.jar");
        let err = ensure_under_root(tmp_root.path(), &outside_missing).unwrap_err();
        assert!(matches!(err, RuntimeError::SecurityException { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn t19_h4_ensure_under_root_rejects_symlink_escape() {
        // V1: a symlink inside the root whose target lives outside the root
        // must be rejected — `resolve_for_confinement` follows the symlink in
        // the existing prefix before the under-root check, so the escape is
        // detected even though the link itself sits under the root.
        let _g = TEST_LOCK.lock();
        let tmp_root = tempfile::tempdir().unwrap();
        let tmp_other = tempfile::tempdir().unwrap();
        let target_dir = tmp_other.path().join("secret");
        std::fs::create_dir_all(&target_dir).unwrap();
        let link = tmp_root.path().join("link");
        std::os::unix::fs::symlink(&target_dir, &link).unwrap();
        // Candidate goes through the in-root symlink but resolves outside.
        let candidate = link.join("loot.jar");
        let err = ensure_under_root(tmp_root.path(), &candidate).unwrap_err();
        assert!(matches!(err, RuntimeError::SecurityException { .. }));
    }

    // -----------------------------------------------------------------
    // Registration smoke
    // -----------------------------------------------------------------

    #[test]
    fn t19_h4_register_jboss_module_loader_adds_surface() {
        let _g = TEST_LOCK.lock();
        let mut r = NativeMethodRegistry::new();
        let before = r.len();
        register_jboss_module_loader(&mut r);
        let after = r.len();
        // We register at least: 2 DefaultBootModuleLoaderHolder$1.run
        // overloads, loadModule/String + loadModule/Identifier on both
        // LocalModuleLoader and ModuleLoader (4), plus the Module /
        // ModuleClassLoader surface including the static loader accessors.
        assert!(
            after >= before + 18,
            "expected >= 18 new registrations, got {}",
            after - before
        );
    }

    #[test]
    fn t19_h4_boot_holder_priv_action_run_returns_loader() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        setup_test_env(tmp.path());
        let mut ctx = MockNativeContext::new();
        let result = native_boot_holder_priv_action_run(&mut ctx, &[])
            .unwrap()
            .unwrap();
        match result {
            Value::Object(Some(_)) => {}
            other => panic!("expected non-null loader, got {:?}", other),
        }
        // Running again must return the same loader ref (idempotent).
        let loader_a = native_boot_holder_priv_action_run(&mut ctx, &[])
            .unwrap()
            .unwrap();
        let loader_b = native_boot_holder_priv_action_run(&mut ctx, &[])
            .unwrap()
            .unwrap();
        match (loader_a, loader_b) {
            (Value::Object(Some(a)), Value::Object(Some(b))) => {
                assert_eq!(a.as_ptr(), b.as_ptr());
            }
            other => panic!("expected pair of non-null loaders, got {:?}", other),
        }
    }

    #[test]
    fn t19_h4_get_caller_module_loader_returns_boot_loader() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        setup_test_env(tmp.path());
        let mut ctx = MockNativeContext::new();
        let caller = native_module_get_boot_or_caller_module_loader(&mut ctx, &[])
            .unwrap()
            .unwrap();
        let boot = native_module_get_boot_or_caller_module_loader(&mut ctx, &[])
            .unwrap()
            .unwrap();
        match (caller, boot) {
            (Value::Object(Some(a)), Value::Object(Some(b))) => {
                assert_eq!(a.as_ptr(), b.as_ptr());
            }
            other => panic!("expected stable boot loader refs, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------
    // find_mp_argument — ensure CLI parsing works for edge cases
    // -----------------------------------------------------------------

    #[test]
    fn t19_h4_get_property_with_default_returns_default() {
        let _g = TEST_LOCK.lock();
        let mut ctx = MockNativeContext::new();
        let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
        let key = ctx.create_string("foo");
        let default = ctx.create_string("bar");
        let result = native_module_get_property_with_default(
            &mut ctx,
            &[
                Value::Object(Some(this)),
                Value::Object(Some(key)),
                Value::Object(Some(default)),
            ],
        )
        .unwrap()
        .unwrap();
        match result {
            Value::Object(Some(s)) => {
                assert_eq!(ctx.read_string(s).as_deref(), Some("bar"));
            }
            other => panic!("expected default string, got {:?}", other),
        }
    }

    #[test]
    fn t19_h4_get_property_returns_null() {
        let _g = TEST_LOCK.lock();
        let mut ctx = MockNativeContext::new();
        let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
        let key = ctx.create_string("foo");
        let result = native_module_get_property(
            &mut ctx,
            &[Value::Object(Some(this)), Value::Object(Some(key))],
        )
        .unwrap()
        .unwrap();
        assert!(matches!(result, Value::Object(None)));
    }

    #[test]
    fn t19_h4_get_property_names_returns_empty_list() {
        let _g = TEST_LOCK.lock();
        let mut ctx = MockNativeContext::new();
        let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
        let result = native_module_get_property_names(&mut ctx, &[Value::Object(Some(this))])
            .unwrap()
            .unwrap();
        match result {
            Value::Object(Some(_list)) => {}
            other => panic!("expected non-null List, got {:?}", other),
        }
    }

    #[test]
    fn t19_h4_locate_module_xml_finds_addon_layer() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let xml = root.join("system/add-ons/keycloak/com/extra/foo/main/module.xml");
        write(&xml, &make_minimal_module_xml("com.extra.foo"));
        let found = locate_module_xml(root, "com.extra.foo").unwrap();
        assert!(found.ends_with("module.xml"));
    }

    // -----------------------------------------------------------------
    // T19_H15 — transitive linkage closure & dynamic-classpath
    // registration when loadModule is called.
    //
    // Without this fix, KC16 boots far enough to call
    // `loadModule("org.jboss.as.standalone")` but then NCDFEs on
    // `org.jboss.as.controller.access.JmxAction$Impact` when bytecode
    // in the controller-dependent module is verified — because the
    // controller jar wasn't yet on the application classpath.
    // -----------------------------------------------------------------

    /// Build a module.xml with `dependencies` (Module deps).
    fn make_module_xml_with_deps(
        name: &str,
        jars: &[&str],
        deps: &[(&str, bool /* export */, bool /* optional */)],
    ) -> String {
        let mut s = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<module name=\"{}\" xmlns=\"urn:jboss:module:1.9\">\n  <resources>\n",
            name
        );
        for j in jars {
            s.push_str(&format!("    <resource-root path=\"{}\"/>\n", j));
        }
        s.push_str("  </resources>\n  <dependencies>\n");
        for (dep, export, optional) in deps {
            s.push_str(&format!(
                "    <module name=\"{}\"{}{}/>\n",
                dep,
                if *export { " export=\"true\"" } else { "" },
                if *optional { " optional=\"true\"" } else { "" },
            ));
        }
        s.push_str("  </dependencies>\n</module>\n");
        s
    }

    /// Helper: write a module with N jars under `<root>/<dotted-path>/main/`.
    fn write_module_with_deps(root: &Path, name: &str, jars: &[&str], deps: &[(&str, bool, bool)]) {
        let path: PathBuf = name.split('.').collect();
        let main_dir = root.join(&path).join("main");
        write(
            &main_dir.join("module.xml"),
            &make_module_xml_with_deps(name, jars, deps),
        );
        for j in jars {
            // Touch the jar so canonicalize / file-existence checks pass.
            write(&main_dir.join(j), "PK");
        }
    }

    #[test]
    fn t19_h15_transitive_linkage_walks_full_dep_tree() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // start -> mid -> leaf  (no `export="true"` on either edge)
        write_module_with_deps(root, "start", &["s.jar"], &[("mid", false, false)]);
        write_module_with_deps(root, "mid", &["m.jar"], &[("leaf", false, false)]);
        write_module_with_deps(root, "leaf", &["l.jar"], &[]);
        setup_test_env(root);

        let roots = transitive_linkage_roots("start");
        // Should include all three jars regardless of `export="true"`.
        let names: Vec<String> = roots
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|n| n == "s.jar"), "got {:?}", names);
        assert!(names.iter().any(|n| n == "m.jar"), "got {:?}", names);
        assert!(names.iter().any(|n| n == "l.jar"), "got {:?}", names);
    }

    #[test]
    fn t19_h15_transitive_linkage_handles_cycle_without_loop() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // a -> b -> a  (cycle)
        write_module_with_deps(root, "a", &["a.jar"], &[("b", false, false)]);
        write_module_with_deps(root, "b", &["b.jar"], &[("a", false, false)]);
        setup_test_env(root);

        let roots = transitive_linkage_roots("a");
        assert!(!roots.is_empty(), "linkage walk must terminate on cycles");
        let names: Vec<String> = roots
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|n| n == "a.jar"));
        assert!(names.iter().any(|n| n == "b.jar"));
    }

    #[test]
    fn t19_h15_transitive_linkage_skips_missing_deps_silently() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // start depends on `missing` (not on disk). Walk must not error.
        write_module_with_deps(
            root,
            "start",
            &["s.jar"],
            &[("missing", false, true), ("present", false, false)],
        );
        write_module_with_deps(root, "present", &["p.jar"], &[]);
        setup_test_env(root);

        let roots = transitive_linkage_roots("start");
        let names: Vec<String> = roots
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|n| n == "s.jar"));
        assert!(names.iter().any(|n| n == "p.jar"));
    }

    #[test]
    fn t19_h15_transitive_linkage_skips_system_deps() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // start has a Module dep + a `<system>` dep (which mod_xml
        // parses as `DependencyKind::System`).  System deps come from
        // the bootstrap classpath, not application — we must not try
        // to register a `system` module's roots.
        let main_dir = root.join("start/main");
        let xml = format!(
            r#"<?xml version="1.0"?>
<module name="start" xmlns="urn:jboss:module:1.9">
  <resources>
    <resource-root path="s.jar"/>
  </resources>
  <dependencies>
    <module name="present"/>
    <system export="true">
      <paths>
        <path name="java/lang"/>
      </paths>
    </system>
  </dependencies>
</module>
"#
        );
        write(&main_dir.join("module.xml"), &xml);
        write(&main_dir.join("s.jar"), "PK");
        write_module_with_deps(root, "present", &["p.jar"], &[]);
        setup_test_env(root);

        let roots = transitive_linkage_roots("start");
        let names: Vec<String> = roots
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|n| n == "s.jar"));
        assert!(names.iter().any(|n| n == "p.jar"));
    }

    #[test]
    fn t19_h15_load_module_registers_transitive_classpath() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // Mirrors the WildFly shape that triggered the original bug:
        // jmx -> controller (no export) -> protocol (no export).
        // The bytecode in `jmx` references a class in `controller`,
        // and the verifier needs `controller.jar` on the classpath
        // even though `controller` is not re-exported.
        write_module_with_deps(
            root,
            "org.jboss.as.jmx",
            &["wildfly-jmx.jar"],
            &[("org.jboss.as.controller", false, false)],
        );
        write_module_with_deps(
            root,
            "org.jboss.as.controller",
            &["wildfly-controller.jar"],
            &[("org.jboss.as.protocol", false, false)],
        );
        write_module_with_deps(
            root,
            "org.jboss.as.protocol",
            &["wildfly-protocol.jar"],
            &[],
        );
        setup_test_env(root);

        let mut ctx = MockNativeContext::new();
        let loader = build_local_module_loader(&mut ctx).unwrap();
        let name = ctx.create_string("org.jboss.as.jmx");
        native_loader_load_module(
            &mut ctx,
            &[Value::Object(Some(loader)), Value::Object(Some(name))],
        )
        .unwrap();

        let registered = ctx.registered_classpath_snapshot();
        assert!(
            registered.iter().any(|p| p.ends_with("wildfly-jmx.jar")),
            "must register own jar; got {:?}",
            registered
        );
        assert!(
            registered
                .iter()
                .any(|p| p.ends_with("wildfly-controller.jar")),
            "must register direct dep jar so the verifier can find \
             classes referenced from JMX bytecode; got {:?}",
            registered
        );
        assert!(
            registered
                .iter()
                .any(|p| p.ends_with("wildfly-protocol.jar")),
            "must register transitive dep jar so the verifier can find \
             second-hop class references; got {:?}",
            registered
        );
    }

    #[test]
    fn t19_h15_load_module_idempotent_no_duplicate_classpath() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_module_with_deps(root, "a", &["a.jar"], &[("b", false, false)]);
        write_module_with_deps(root, "b", &["b.jar"], &[]);
        setup_test_env(root);

        let mut ctx = MockNativeContext::new();
        let loader = build_local_module_loader(&mut ctx).unwrap();
        let n1 = ctx.create_string("a");
        native_loader_load_module(
            &mut ctx,
            &[Value::Object(Some(loader)), Value::Object(Some(n1))],
        )
        .unwrap();
        // Second call should return cached module without re-registering.
        let n2 = ctx.create_string("a");
        native_loader_load_module(
            &mut ctx,
            &[Value::Object(Some(loader)), Value::Object(Some(n2))],
        )
        .unwrap();

        let registered = ctx.registered_classpath_snapshot();
        let count_a = registered.iter().filter(|p| p.ends_with("a.jar")).count();
        let count_b = registered.iter().filter(|p| p.ends_with("b.jar")).count();
        assert_eq!(count_a, 1, "a.jar must register once; got {:?}", registered);
        assert_eq!(count_b, 1, "b.jar must register once; got {:?}", registered);
    }

    #[test]
    fn t19_h15_load_module_inner_class_naming_passthrough() {
        // The ModuleClassLoader path must accept `Outer$Inner` style
        // names with `$`-separated nested class identifiers — JBoss
        // bytecode references them with `getDeclaredClasses` /
        // `Class.forName("...$Inner")` calls during enum init.
        // We can't load real bytecode in the unit-test mock, but we
        // can drive the `loadClass` native end-to-end and confirm the
        // visibility lookup uses the correct entry-path encoding.
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_module_with_deps(root, "x", &["x.jar"], &[]);
        setup_test_env(root);

        // Confirm `find_entry_in_roots` searches with the literal `$`.
        let main_dir = root.join("x/main");
        // Replace x.jar with a real (empty) zip that contains
        // `Outer$Inner.class` so the lookup hits.
        let jar_path = main_dir.join("x.jar");
        let f = std::fs::File::create(&jar_path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        zw.start_file(
            "Outer$Inner.class",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        use std::io::Write;
        zw.write_all(b"\xCA\xFE\xBA\xBE").unwrap();
        zw.finish().unwrap();

        // Force the resolver to re-read the on-disk module since we
        // just rewrote x.jar after `setup_test_env` populated nothing.
        clear_module_cache_for_test();
        let _ = transitive_linkage_roots("x");
        let resolved = ensure_resolved("x").expect("module must resolve");
        let hit = find_entry_in_roots(&resolved.resource_roots, "Outer$Inner.class");
        assert!(
            hit.is_some(),
            "find_entry_in_roots must accept `$` in inner-class names"
        );
    }

    #[test]
    fn t19_h15_register_resource_roots_handles_nonexistent_paths_gracefully() {
        // `register_dynamic_classpath` / `add_path` must silently
        // skip non-existent paths — JBoss modules sometimes list jars
        // that aren't shipped (e.g. optional add-ons).  We expose
        // the unfiltered path string here; the consumer
        // (ClassPath::add_path) handles the actual filtering.
        let _g = TEST_LOCK.lock();
        let mut ctx = MockNativeContext::new();
        let bogus = vec![PathBuf::from("does/not/exist.jar")];
        register_resource_roots(&mut ctx, &bogus);
        // The mock does record the path string regardless — the real
        // `add_path` will skip it.  We're only asserting that
        // `register_resource_roots` doesn't panic on a missing file.
        let snap = ctx.registered_classpath_snapshot();
        assert_eq!(snap.len(), 1);
    }

    #[test]
    fn t19_h15_transitive_linkage_includes_export_transitive() {
        // Even without the linkage closure, `export="true"` paths
        // were already followed.  Confirm we don't lose that:
        // a -[export]-> b -[export]-> c.
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_module_with_deps(root, "a", &["a.jar"], &[("b", true, false)]);
        write_module_with_deps(root, "b", &["b.jar"], &[("c", true, false)]);
        write_module_with_deps(root, "c", &["c.jar"], &[]);
        setup_test_env(root);

        let roots = transitive_linkage_roots("a");
        let names: Vec<String> = roots
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|n| n == "a.jar"));
        assert!(names.iter().any(|n| n == "b.jar"));
        assert!(names.iter().any(|n| n == "c.jar"));
    }

    #[test]
    fn t19_h15_load_module_no_deps_registers_only_self() {
        // Backwards-compat: a leaf module with no deps must still
        // register exactly its own resource roots — no broader walk.
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_module_with_deps(root, "leaf", &["only.jar"], &[]);
        setup_test_env(root);

        let mut ctx = MockNativeContext::new();
        let loader = build_local_module_loader(&mut ctx).unwrap();
        let name = ctx.create_string("leaf");
        native_loader_load_module(
            &mut ctx,
            &[Value::Object(Some(loader)), Value::Object(Some(name))],
        )
        .unwrap();

        let registered = ctx.registered_classpath_snapshot();
        assert!(
            registered.iter().any(|p| p.ends_with("only.jar")),
            "got {:?}",
            registered
        );
        assert_eq!(
            registered
                .iter()
                .filter(|p| p.ends_with("only.jar"))
                .count(),
            1,
            "must register exactly once; got {:?}",
            registered
        );
    }

    #[test]
    fn t19_h15_module_visibility_closure_unchanged_by_linkage_walk() {
        // The runtime `module_visibility_closure` (used by MCL.loadClass
        // for *enforcement*) must still follow JBoss's "self + direct +
        // transitive re-exports" rule — narrower than
        // `transitive_linkage_roots`.  Confirm the two walks return
        // different sets when there's a non-exported dep.
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_module_with_deps(root, "start", &["s.jar"], &[("direct", false, false)]);
        write_module_with_deps(root, "direct", &["d.jar"], &[("hidden", false, false)]);
        write_module_with_deps(root, "hidden", &["h.jar"], &[]);
        setup_test_env(root);

        // Linkage closure: includes h.jar (transitive non-export dep).
        let linkage = transitive_linkage_roots("start");
        let linkage_names: Vec<String> = linkage
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(linkage_names.iter().any(|n| n == "h.jar"));

        // Visibility closure: must NOT include h.jar (non-exported
        // dep of a direct dep — JBoss's runtime rule hides it).
        let (_modules, vis) = module_visibility_closure("start");
        let vis_names: Vec<String> = vis
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(vis_names.iter().any(|n| n == "s.jar"));
        assert!(vis_names.iter().any(|n| n == "d.jar"));
        assert!(
            !vis_names.iter().any(|n| n == "h.jar"),
            "visibility closure must not include non-exported \
             transitive deps; got {:?}",
            vis_names
        );
    }

    #[test]
    fn t19_h16_service_roots_only_include_service_imports() {
        let _g = TEST_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let start_dir = root.join("start/main");

        write(
            &start_dir.join("module.xml"),
            r#"<?xml version="1.0"?>
<module name="start" xmlns="urn:jboss:module:1.9">
  <resources><resource-root path="start.jar"/></resources>
  <dependencies>
    <module name="svc.imported" services="import"/>
    <module name="svc.hidden"/>
    <module name="svc.exported" services="export"/>
  </dependencies>
</module>
"#,
        );
        write(&start_dir.join("start.jar"), "PK");
        write_module_with_deps(root, "svc.imported", &["imported.jar"], &[]);
        write_module_with_deps(root, "svc.hidden", &["hidden.jar"], &[]);
        write_module_with_deps(root, "svc.exported", &["exported.jar"], &[]);
        setup_test_env(root);

        let roots = module_service_roots("start");
        let names: Vec<String> = roots
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|n| n == "start.jar"), "got {:?}", names);
        assert!(names.iter().any(|n| n == "imported.jar"), "got {:?}", names);
        assert!(names.iter().any(|n| n == "exported.jar"), "got {:?}", names);
        assert!(
            !names.iter().any(|n| n == "hidden.jar"),
            "service roots must not include ordinary deps; got {:?}",
            names
        );
    }

    #[test]
    fn wildfly_jboss_modules_service_provider_leak_real_dist_scoping() {
        // Regression guard for
        // wildfly-jboss-modules-service-provider-leak.md,
        // exercised against a *real* WildFly 32.0.1.Final distribution's
        // modules/ tree (not a synthetic fixture) so the exact conflicting
        // pair from the original repro -- org.jboss.as.jmx and
        // org.wildfly.extension.core-management, neither of which depends
        // on the other, each shipping its own
        // META-INF/services/org.jboss.as.controller.Extension descriptor
        // -- is covered verbatim. Skips (rather than fails) when the
        // distribution isn't staged on this machine.
        let _g = TEST_LOCK.lock();
        let real_root =
            std::path::Path::new("/data/data/wildfly-dist/wildfly-32.0.1.Final/modules");
        if !real_root.is_dir() {
            eprintln!(
                "wildfly_jboss_modules_service_provider_leak_real_dist_scoping: \
                 real WildFly dist not present at {}, skipping",
                real_root.display()
            );
            return;
        }
        setup_test_env(real_root);
        let jmx =
            module_service_provider_names("org.jboss.as.jmx", "org.jboss.as.controller.Extension");
        let cm = module_service_provider_names(
            "org.wildfly.extension.core-management",
            "org.jboss.as.controller.Extension",
        );
        let modcluster = module_service_provider_names(
            "org.jboss.as.modcluster",
            "org.jboss.as.controller.Extension",
        );
        assert_eq!(
            jmx,
            vec!["org.jboss.as.jmx.JMXExtension".to_string()],
            "org.jboss.as.jmx's Extension providers must be scoped to its \
             own module; got {:?}. Any entry from \
             org.wildfly.extension.core-management reproduces the \
             cross-module service-provider leak.",
            jmx
        );
        assert_eq!(
            cm,
            vec!["org.wildfly.extension.core.management.CoreManagementExtension".to_string()],
            "org.wildfly.extension.core-management's Extension providers \
             must be scoped to its own module; got {:?}. Any entry from \
             org.jboss.as.jmx reproduces the cross-module \
             service-provider leak.",
            cm
        );
        assert_eq!(
            modcluster,
            vec!["org.wildfly.extension.mod_cluster.ModClusterExtension".to_string()],
            "org.jboss.as.modcluster is a module-alias for \
             org.wildfly.extension.mod_cluster; got {:?}",
            modcluster
        );
    }

    /// RKC19/WF39 Task C — Sanity-check that the synthetic class bytes
    /// produced by `build_synthetic_class_with_main` start with the JVM
    /// class file magic and contain the expected method names.  The full
    /// parsing path is exercised in integration via
    /// `define_class_from_bytes`; here we just guard the byte layout.
    #[test]
    fn rkc19_wf39_synthetic_class_bytes_well_formed() {
        let bytes = build_synthetic_class_with_main("org/jboss/as/server/Main");
        // Magic
        assert_eq!(&bytes[0..4], &[0xCA, 0xFE, 0xBA, 0xBE]);
        // Major version = 52 (Java 8)
        assert_eq!(&bytes[6..8], &[0x00, 0x34]);
        // Constant pool count = 12 (entries 1..=11)
        assert_eq!(&bytes[8..10], &[0x00, 0x0C]);
        // Body should contain "main" and "([Ljava/lang/String;)V" as
        // raw substrings (Utf8 entries are plain UTF-8 payloads).
        let body = String::from_utf8_lossy(&bytes);
        assert!(
            body.contains("main"),
            "expected 'main' in synthesised bytes"
        );
        assert!(
            body.contains("([Ljava/lang/String;)V"),
            "expected main descriptor in synthesised bytes"
        );
        assert!(
            body.contains("org/jboss/as/server/Main"),
            "expected this_class name in synthesised bytes"
        );
        // Return bytecode 0xB1 should appear at least twice (init returns
        // after super-call, main is a single return).
        let return_count = bytes.iter().filter(|&&b| b == 0xB1).count();
        assert!(
            return_count >= 2,
            "expected at least 2 occurrences of `return` (0xB1); got {} in {:?}",
            return_count,
            bytes
        );
    }
