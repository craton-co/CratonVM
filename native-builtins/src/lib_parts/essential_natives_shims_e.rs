#[allow(unused_imports)]
use super::super::*;

#[allow(unused_variables)]
pub fn register_essential_natives_with_shims_e(registry: &mut NativeMethodRegistry, shim_selection: app_shims::ShimSelection) {
    registry.register(
        "java/lang/ClassLoader",
        "getResources",
        "(Ljava/lang/String;)Ljava/util/Enumeration;",
        classloader::cl_get_resources_essential, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/ClassLoader",
        "getSystemResources",
        "(Ljava/lang/String;)Ljava/util/Enumeration;",
        classloader::cl_get_resources_essential, cratonvm_native_api::NativeKind::Bridge,
    );
    // Wave-1 Task B: install the singular `getResource(String)` override
    // alongside the bulk `getResources` one so the two stay consistent in
    // real-JDK mode. Without this, the JDK's own implementation runs and
    // returns null whenever URLClassPath.<clinit> swallowed in real-JDK
    // bootstrap (the same root cause RSLF4J.1 fixed for the bulk path).
    registry.register(
        "java/lang/ClassLoader",
        "getResource",
        "(Ljava/lang/String;)Ljava/net/URL;",
        classloader::cl_get_resource_essential, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/ClassLoader",
        "getSystemResource",
        "(Ljava/lang/String;)Ljava/net/URL;",
        classloader::cl_get_resource_essential, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/ClassLoader",
        "getResourceAsStream",
        "(Ljava/lang/String;)Ljava/io/InputStream;",
        classloader::cl_get_resource_as_stream_essential, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/ClassLoader",
        "getSystemResourceAsStream",
        "(Ljava/lang/String;)Ljava/io/InputStream;",
        classloader::cl_get_resource_as_stream_essential, cratonvm_native_api::NativeKind::Bridge,
    );
    for cls in [
        "jdk/internal/loader/BuiltinClassLoader",
        "jdk/internal/loader/ClassLoaders$AppClassLoader",
        "jdk/internal/loader/ClassLoaders$PlatformClassLoader",
    ] {
        registry.register(
            cls,
            "getResourceAsStream",
            "(Ljava/lang/String;)Ljava/io/InputStream;",
            classloader::cl_get_resource_as_stream_essential, cratonvm_native_api::NativeKind::Bridge,
        );
    }
    // `java.net.URLClassLoader` declares its OWN `getResourceAsStream`
    // override in real bytecode (unlike `getResource`/`getResources`/
    // `findResource`, left to `ClassLoader`/its own extension point), so the
    // `java/lang/ClassLoader` registration above never gets consulted for a
    // URLClassLoader-typed receiver — dispatch resolves the declaring class
    // to `java/net/URLClassLoader` itself. The equivalent registration for
    // this exact class previously lived ONLY in
    // `classloader::register_classloader_natives`/`servlet::register_s1_classloading`,
    // both nested inside `register_synthetic_overrides` — a no-op stub in
    // the default real-JDK `cratonvm-cli` build (`vm/src/native/builtins.rs`,
    // `#[cfg(feature = "synthetic-jdk")]`-gated) — so this override was
    // effectively dead in real-JDK mode despite `force_native_over_real_jdk_bytecode`
    // (vm/src/runtime/interpreter.rs) and the `check_override` allow-list
    // (vm/src/vm/vm_exec.rs) both correctly forcing native dispatch: the
    // force-check passed but the registry lookup then found nothing to call,
    // silently falling through to real (broken, unpopulated-`ucp`-backed,
    // non-parent-delegating) bytecode. Concretely this broke Spring Boot's
    // `StaticResourceJarsTests`-adjacent `ServletComponentScanIntegrationTests
    // .indexedComponentsAreRegistered` / `MockWebEnvironmentServletComponentScanIntegrationTests`:
    // a `new URLClassLoader(urls, parent)` wrapping only a `@TempDir`-generated
    // `META-INF/spring.components` index (parented to the real test
    // classloader) resolved `.class` resources fine via `getResource` but got
    // `null` from `getResourceAsStream` for every one the PARENT actually
    // holds, so `ClassPathResource.getInputStream()` threw
    // `FileNotFoundException` reading an indexed component that plainly
    // exists. Register the always-real-JDK-mode-active essential native here
    // instead.
    registry.register(
        "java/net/URLClassLoader",
        "getResourceAsStream",
        "(Ljava/lang/String;)Ljava/io/InputStream;",
        classloader::cl_get_resource_as_stream_essential, cratonvm_native_api::NativeKind::Bridge,
    );
    // JDK 25's URLClassLoader.getURLs() is concrete bytecode that reads the
    // shimmed URLClassPath directly. Route it through the receiver-local
    // native so manifest class-path entries are visible to callers before
    // they construct filtered child loaders.
    registry.register(
        "java/net/URLClassLoader",
        "getURLs",
        "()[Ljava/net/URL;",
        classloader::ucl_get_urls, cratonvm_native_api::NativeKind::Bridge,
    );
    // Real URLClassLoader.close() must set the same receiver-local state
    // consulted by findClass/findResource/findResources.
    registry.register(
        "java/net/URLClassLoader",
        "close",
        "()V",
        classloader::ucl_close, cratonvm_native_api::NativeKind::Bridge,
    );
    // SB-15: `java.lang.Module.getResourceAsStream(String)`. kotlin-reflect's
    // multi-release `BuiltInsResourceLoader.loadResource` (JDK 9+ variant)
    // resolves the `.kotlin_builtins` protobuf resources via
    // `kotlin.Unit.class.getModule().getResourceAsStream(path)`. The real-JDK
    // bytecode walks module/loader internals CratonVM leaves unpopulated and
    // returns null, so the kotlin built-ins module ends up empty and
    // `getBuiltInClassByName("Int")` fails ("Built-in class kotlin.Int is not
    // found"). For classpath classes the unnamed module delegates to the app
    // loader, so resolve via the same classpath scan as the ClassLoader-side
    // native. Registered unconditionally (real-JDK + synthetic) like the
    // getResource overrides above.
    // ENCAPSULATION (wave 5): this registration is the LAST one for this
    // triple, so it decides the callback. It used to point at
    // `classloader::module_get_resource_as_stream`, silently overwriting the
    // encapsulating native `register_jboss_jdkspecific` had installed ~9k
    // lines earlier in this same function — so a resource in a named module's
    // non-open package was served to any caller (`RJdkModule.java:198`,
    // HotSpot refuses). The jboss native applies the
    // `java.lang.Module#getResourceAsStream` opens check and then DELEGATES to
    // `classloader::module_get_resource_as_stream` for the actual bytes, so
    // the kotlin-reflect behaviour this site was added for is unchanged.
    registry.register(
        "java/lang/Module",
        "getResourceAsStream",
        "(Ljava/lang/String;)Ljava/io/InputStream;",
        jboss_jdkspecific::native_module_get_resource_as_stream, cratonvm_native_api::NativeKind::Bridge,
    );
    // JIT round 20 wave 4, lane walkcompat
    // (`docs/feature-designs/native-dispatch-architecture.md`):
    // under `--compatible` this row and `BuiltinClassLoader.findResourceAsStream`
    // below are stated `Intrinsic`s. The JDK bodies ask the built-in loaders'
    // `nameToModule`, which only `--real-jdk` fills (`real_builtin_loaders_enabled`
    // adopts the JDK's loaders and defines every boot module to them);
    // `--compatible` never does, so once dev's bytecode-first dispatch let the
    // JDK body beat a `Bridge` here, `Class.getResourceAsStream` of every named
    // module's own resource answered null (`Character.getName`: `uniName.dat`;
    // `Normalizer`'s ICU data, `currency.data`, `content-types.properties`).
    // The JDK's `Class` / `Module` bodies keep running first, so the
    // encapsulation checks are HotSpot's; only the final byte lookup is the
    // VM's image index. `--real-jdk`: registered exactly as before the round
    // (the registrar's ambient `Bridge`, not restated: the boot row is a
    // retired shadow there and dropped; the built-in loader row loses to the
    // real loaders' bytecode, which answers). Both modes register the row; only
    // the stated kind differs. Restating it `Bridge` under `--real-jdk` (wave 4)
    // added a stated-`Bridge`-over-bytecode row to the bridge ratchet's
    // `bridge.shadows_bytecode` count (wave 6).
    if registry.compatibility_mode().is_jdk_only() {
        registry.register(
            "jdk/internal/loader/BootLoader",
            "findResourceAsStream",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/io/InputStream;",
            classloader::bootloader_find_resource_as_stream, cratonvm_native_api::NativeKind::Bridge,
        );
    } else {
        registry.register(
            "jdk/internal/loader/BootLoader",
            "findResourceAsStream",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/io/InputStream;",
            classloader::bootloader_find_resource_as_stream,
            NativeKind::Intrinsic,
        );
        // Interpreter round i4 wave 95, lane L3: the URL sibling, for JDK
        // 25's `Class.getResource` of a named boot module's class
        // (`L3W94CallerSensitiveMatrix` `res-class-direct`; HotSpot `found`,
        // `--compatible` null). `--compatible` only, as the row above, for
        // the same reason (only `--real-jdk` fills `nameToModule`); a stated
        // `Intrinsic` so `resolve_dispatch` admits it over the JDK body.
        registry.register(
            "jdk/internal/loader/BootLoader",
            "findResource",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/net/URL;",
            classloader::bootloader_find_module_resource,
            NativeKind::Intrinsic,
        );
    }
    // `--real-jdk` only, and `Bridge` stated rather than inherited: the JDK
    // bodies ask a `BuiltinClassLoader` boot loader this VM never builds, and
    // answer null for every name. `--compatible` answers user loaders' boot
    // lookups in the `ClassLoader.getResource*` natives instead, so it keeps
    // the JDK bodies and its registry is unchanged -- except over a real JDK
    // with the `JdkUrlClassLoaders` row (interpreter round i1 wave 52, lane
    // L5), where user loaders' `getResource*` run the JDK's `ClassLoader`
    // body as under `--real-jdk` and so reach these two.
    //
    // Interpreter round i5 wave 100, lane L3 (`L3W99NullParentResources`
    // `--compatible`: a null-parent `URLClassLoader`'s
    // `getResource("java/lang/Object.class")` read null, HotSpot
    // `jrt:/java.base/...`): under `--compatible` a stated `Bridge` loses to
    // the JDK body (bytecode first), and that body asks a boot
    // `BuiltinClassLoader` whose `nameToModule` only `--real-jdk` fills, so it
    // answered null for every name. Stated `Intrinsic` there, as the
    // `findResource(String,String)` / `findResourceAsStream` rows above;
    // `--real-jdk` keeps the `Bridge` (its real boot loader answers).
    if registry.jdk_url_class_loaders() {
        let kind = if registry.compatibility_mode().is_jdk_only() {
            NativeKind::Bridge
        } else {
            NativeKind::Intrinsic
        };
        registry.register(
            "jdk/internal/loader/BootLoader",
            "findResource",
            "(Ljava/lang/String;)Ljava/net/URL;",
            classloader::bootloader_find_resource,
            kind,
        );
        registry.register(
            "jdk/internal/loader/BootLoader",
            "findResources",
            "(Ljava/lang/String;)Ljava/util/Enumeration;",
            classloader::bootloader_find_resources,
            kind,
        );
    }
    // The `URLClassPath$Loader` walk index (`CRATONVM_NESTED_JAR_WALK_INDEX`;
    // interpreter round i2 wave 54, lane L5): a reviewed `Intrinsic`, stated
    // here rather than inherited (AGENTS.md), over the JDK's generic loader's
    // two lookups. It runs the JDK body unless the VM's index of a Spring Boot
    // `jar:nested:` jar proves the body's answer is `null`
    // (`classloader::nested_jar_walk_loader_get_resource`). Only where the
    // `URLClassLoader` family runs the JDK's bodies (the registry predicate
    // includes `jdk_url_class_loaders`).
    //
    // `CRATONVM_NESTED_JAR_WALK_INDEX_STAMP` (default on, `=0` off; interpreter round
    // i2 wave 65, lane L5) picks the twin callbacks whose per-VM index rows
    // are re-stat'ed and rebuilt when the fat jar was rewritten on disk: the
    // same reviewed `Intrinsic` kind, chosen at registration so the default
    // path pays no per-call flag read.
    if registry.nested_jar_walk_index() {
        let stamped = registry.nested_jar_walk_index_stamp();
        let get_resource: cratonvm_native_api::NativeCallback = if stamped {
            classloader::nested_jar_walk_loader_get_resource_stamped
        } else {
            classloader::nested_jar_walk_loader_get_resource
        };
        let find_resource: cratonvm_native_api::NativeCallback = if stamped {
            classloader::nested_jar_walk_loader_find_resource_stamped
        } else {
            classloader::nested_jar_walk_loader_find_resource
        };
        registry.register(
            "jdk/internal/loader/URLClassPath$Loader",
            "getResource",
            "(Ljava/lang/String;)Ljdk/internal/loader/Resource;",
            get_resource,
            NativeKind::Intrinsic,
        );
        registry.register(
            "jdk/internal/loader/URLClassPath$Loader",
            "findResource",
            "(Ljava/lang/String;)Ljava/net/URL;",
            find_resource,
            NativeKind::Intrinsic,
        );
    }
    // The nested-jar bytes index (`CRATONVM_NESTED_JAR_BYTES_INDEX`, default
    // off; interpreter round i2 wave 63, lane L5): a reviewed `Intrinsic`,
    // stated here rather than inherited (AGENTS.md), over the JDK's concrete
    // `Resource.getBytes()`. It answers the generic `URLClassPath$Loader`'s
    // `Resource` over a Spring Boot `jar:nested:` URL from the VM's copy of a
    // plain nested jar, and runs the JDK body for every other receiver
    // (`classloader::nested_jar_resource_get_bytes`). Only where the
    // `URLClassLoader` family runs the JDK's bodies.
    //
    // The jar-loader bytes index (`CRATONVM_JAR_LOADER_BYTES_INDEX`, default
    // off; interpreter round i2 wave 64, lane L5) is the same reviewed
    // `Intrinsic` on the same triple with a second half: a
    // `URLClassPath$JarLoader` `Resource` over a plain `java.util.jar.JarFile`
    // is answered from the VM's mapping of that jar
    // (`classloader::jar_loader_resource_get_bytes`). One callback per switch
    // combination, registered with its kind stated.
    let resource_get_bytes: Option<cratonvm_native_api::NativeCallback> = match (
        registry.nested_jar_bytes_index(),
        registry.jar_loader_bytes_index(),
    ) {
        (true, true) => Some(classloader::nested_and_jar_loader_resource_get_bytes),
        (true, false) => Some(classloader::nested_jar_resource_get_bytes),
        (false, true) => Some(classloader::jar_loader_resource_get_bytes),
        (false, false) => None,
    };
    if let Some(resource_get_bytes) = resource_get_bytes {
        registry.register(
            "jdk/internal/loader/Resource",
            "getBytes",
            "()[B",
            resource_get_bytes,
            NativeKind::Intrinsic,
        );
    }
    // Reviewed intrinsic: retain the real body and module ownership checks;
    // supply system-module bytes when Compatible has only native graph rows.
    // The explicit kind lets resolve_dispatch select it over concrete bytecode.
    registry.register(
        "jdk/internal/loader/BuiltinClassLoader",
        "findResourceAsStream",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/io/InputStream;",
        classloader::builtin_classloader_find_resource_as_stream,
        NativeKind::Intrinsic,
    );
    // Interpreter round i4 wave 96, lane L3: the URL sibling, for JDK 25's
    // `Class.getResource` of a class of a named platform or application
    // module (`java.sql.Date.class.getResource("Date.class")`: HotSpot the
    // `jrt:` URL, `--compatible` null; `L3W96PlatformModuleResource`).
    // `--compatible` only, as the `BootLoader.findResource(mn, name)` row:
    // only `--real-jdk` fills `nameToModule`, and there the JDK body answers
    // alone. A stated `Intrinsic` (AGENTS.md) so `resolve_dispatch` admits it
    // over the JDK body, which it runs first.
    if !registry.compatibility_mode().is_jdk_only() {
        registry.register(
            "jdk/internal/loader/BuiltinClassLoader",
            "findResource",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/net/URL;",
            classloader::builtin_classloader_find_module_resource,
            NativeKind::Intrinsic,
        );
    }
    // URLClassLoader.findResource/findResources — the real bytecode walks
    // URLClassPath, whose essential-mode stubs return null/empty, so a
    // direct findResource() call (or a getResource() override delegating to
    // it, e.g. Gradle's VisitableURLClassLoader) NEVER finds anything even
    // though getResource() (the structured walk above) does. Previously
    // these natives were only registered via register_classloader_natives,
    // which is synthetic-Real-JDK — real-JDK mode silently lacked them
    // ("Cannot find resource 'gradle-plugins.properties'", every Gradle
    // ProjectBuilder bootstrap).
    registry.register(
        "java/net/URLClassLoader",
        "findResource",
        "(Ljava/lang/String;)Ljava/net/URL;",
        classloader::ucl_find_resource, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/net/URLClassLoader",
        "findResources",
        "(Ljava/lang/String;)Ljava/util/Enumeration;",
        classloader::ucl_find_resources, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/net/URLClassLoader",
        "addURL",
        "(Ljava/net/URL;)V",
        classloader::ucl_add_url_real, cratonvm_native_api::NativeKind::Bridge,
    );
    // URLClassLoader.findClass — the real bytecode resolves via the shimmed
    // `ucp` (URLClassPath) and so throws CNFE for everything. Serve it from the
    // receiver's own recorded URLs first so the class is defined under that
    // loader, then keep the global dynamic classpath fallback for legacy paths
    // whose loader `<init>` registered URLs there. Reached when a subclass
    // overrides `loadClass` and calls `findClass` directly (Jasper's
    // `JasperLoader` loading runtime-compiled `org.apache.jsp.*_jsp` servlets);
    // forced onto this native by
    // `intercept_urlclassloader_subclass_find_class`. Only registered here
    // (real-JDK mode); synthetic-JDK mode registers `ucl_find_class` via
    // `register_classloader_natives`.
    registry.register(
        "java/net/URLClassLoader",
        "findClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        classloader_real::ucl_real_find_class, cratonvm_native_api::NativeKind::Bridge,
    );
    // URLClassPath.addURL — `URLClassLoader.addURL`'s body is `ucp.addURL(url)`,
    // whose real bytecode does `synchronized (unopenedUrls)` and NPEs because
    // CratonVM's `ucp` is a bare synthetic `URLClassPath` (null instance
    // fields). Register a native that records the URL on `ucp.path` (so
    // `findResource(s)` can resolve custom-handler URLs) and extends the global
    // dynamic classpath for ordinary file:/jar: URLs. We shim `URLClassPath`
    // (not `URLClassLoader`) because `addURL` is usually invoked via a SUBCLASS
    // `this.addURL(url)` (e.g. ShrinkWrap), whose CP methodref names the
    // subclass and so escapes the force-native gates; `ucp.addURL` always names
    // `URLClassPath`. Without this, ANY real-mode `URLClassLoader` subclass that
    // calls the protected `addURL` (ShrinkWrap's `ShrinkWrapClassLoader`,
    // Hibernate `NoDepthTests` JPA) throws
    // `NullPointerException: ... "this.unopenedUrls" is null`.
    for cls in &["jdk/internal/loader/URLClassPath", "sun/misc/URLClassPath"] {
        registry.register(cls, "addURL", "(Ljava/net/URL;)V", classloader::ucp_add_url, cratonvm_native_api::NativeKind::Bridge);
    }
    // Natives for the synthetic `java/util/Enumeration$Impl` helper the
    // getResources override returns.  These are idempotent (re-registered
    // by `register_classloader_natives` in synthetic-JDK mode).
    classloader::register_enumeration_impl_natives(registry);

    // S111r6: URLClassPath.<init>(URL[],URLStreamHandlerFactory) NPEs in
    // real-JDK mode when invoked indirectly via SB2 launcher's
    // LaunchedURLClassLoader -> URLClassLoader.<init>(URL[], ClassLoader)
    // -> URLClassPath.<init>(URL[]) at line 131 (`new ArrayList<>(urls.length)`)
    // because `urls` arrives as null. Install a defensive constructor that
    // treats null-or-empty URL[] as empty and initialises every instance
    // field that the real Java code would populate (either through inline
    // field initializers folded into the bytecode constructor, or via the
    // explicit `this.path = ...` / `this.unopenedUrls = ...` assignments).
    //
    // SB3 (S111r15): Spring's CandidateComponentsIndexLoader enumerates
    // `META-INF/spring.components` via ClassLoader.getResources, which
    // descends into URLClassPath.getLoader(int). Line 393 reads
    // `loaders.size()`; if `loaders` is null we NPE. Real Java relies on
    // the inline field initializers at URLClassPath.java:108/111/117
    //     private final ArrayList<Loader> loaders = new ArrayList<>();
    //     private final HashMap<String, Loader> lmap = new HashMap<>();
    //     private boolean closed = false;
    // which the bytecode compiler folds into the constructor before any
    // explicit assignments. Our native replaces the constructor outright,
    // so those initializers never run unless we do them by hand.
    fn ucp_init_2(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        let mut this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        let urls_val = args.get(1).copied().unwrap_or(Value::Object(None));
        // Construct path = new ArrayList<>(len)
        let mut path = match ctx.new_object("java/util/ArrayList")? {
            Some(Value::Object(Some(o))) => o,
            _ => return Ok(None),
        };
        ctx.invoke(
            "java/util/ArrayList",
            "<init>",
            "()V",
            &[Value::Object(Some(path))],
        )?;
        // Construct unopenedUrls = new ArrayDeque<>(len)
        let mut unopened = match ctx.new_object("java/util/ArrayDeque")? {
            Some(Value::Object(Some(o))) => o,
            _ => return Ok(None),
        };
        ctx.invoke(
            "java/util/ArrayDeque",
            "<init>",
            "()V",
            &[Value::Object(Some(unopened))],
        )?;
        // Construct loaders = new ArrayList<>() — required for getLoader(int)
        // which reads loaders.size() at URLClassPath.java:393.
        let mut loaders = match ctx.new_object("java/util/ArrayList")? {
            Some(Value::Object(Some(o))) => o,
            _ => return Ok(None),
        };
        ctx.invoke(
            "java/util/ArrayList",
            "<init>",
            "()V",
            &[Value::Object(Some(loaders))],
        )?;
        // Construct lmap = new HashMap<>() — guards getLoader(int)'s
        // `lmap.containsKey(...)` check at URLClassPath.java:402.
        let mut lmap = match ctx.new_object("java/util/HashMap")? {
            Some(Value::Object(Some(o))) => o,
            _ => return Ok(None),
        };
        ctx.invoke(
            "java/util/HashMap",
            "<init>",
            "()V",
            &[Value::Object(Some(lmap))],
        )?;
        // Iterate URLs (if non-null) and seed both collections.
        //
        // GC-safety: `ArrayList.add` and `ArrayDeque.add` are real bytecode and
        // each can grow a backing array, so every reference this loop carries
        // in -- both collections, the source array, and `this`, which is stored
        // into below -- is a pre-GC address from the second turn on. `elem` is
        // re-read from the (refreshed) array inside the turn, and the SECOND
        // `add` needs it re-read again because the first one allocated.
        if let Value::Object(Some(arr)) = urls_val {
            let len = ctx.array_length(arr);
            let this_pin = ctx.pin_native_root(this);
            let path_pin = ctx.pin_native_root(path);
            let unopened_pin = ctx.pin_native_root(unopened);
            let loaders_pin = ctx.pin_native_root(loaders);
            let lmap_pin = ctx.pin_native_root(lmap);
            let arr_pin = ctx.pin_native_root(arr);
            for i in 0..len {
                let arr = ctx.read_native_pin(arr_pin, arr);
                let elem = ctx.get_array_element(arr, i);
                let elem_pin = match elem {
                    Value::Object(Some(o)) => ctx.pin_native_root(o),
                    _ => usize::MAX,
                };
                let path = ctx.read_native_pin(path_pin, path);
                ctx.invoke(
                    "java/util/ArrayList",
                    "add",
                    "(Ljava/lang/Object;)Z",
                    &[Value::Object(Some(path)), elem],
                )?;
                let elem = match (elem, elem_pin) {
                    (Value::Object(Some(o)), p) if p != usize::MAX => {
                        Value::Object(Some(ctx.read_native_pin(p, o)))
                    }
                    (other, _) => other,
                };
                let unopened = ctx.read_native_pin(unopened_pin, unopened);
                ctx.invoke(
                    "java/util/ArrayDeque",
                    "add",
                    "(Ljava/lang/Object;)Z",
                    &[Value::Object(Some(unopened)), elem],
                )?;
                if elem_pin != usize::MAX {
                    ctx.unpin_native_roots(elem_pin);
                }
            }
            this = ctx.read_native_pin(this_pin, this);
            path = ctx.read_native_pin(path_pin, path);
            unopened = ctx.read_native_pin(unopened_pin, unopened);
            loaders = ctx.read_native_pin(loaders_pin, loaders);
            lmap = ctx.read_native_pin(lmap_pin, lmap);
            ctx.unpin_native_roots(this_pin);
        }
        ctx.set_field_by_name(this, "path", Value::Object(Some(path)));
        ctx.set_field_by_name(this, "unopenedUrls", Value::Object(Some(unopened)));
        ctx.set_field_by_name(this, "loaders", Value::Object(Some(loaders)));
        ctx.set_field_by_name(this, "lmap", Value::Object(Some(lmap)));
        ctx.set_field_by_name(this, "jarHandler", Value::Object(None));
        ctx.set_field_by_name(this, "closed", Value::Int(0));
        Ok(None)
    }
    fn ucp_init_1(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        // Single-arg ctor: forward to the 2-arg form with factory=null.
        let this = args.first().copied().unwrap_or(Value::Object(None));
        let urls = args.get(1).copied().unwrap_or(Value::Object(None));
        ucp_init_2(ctx, &[this, urls, Value::Object(None)])
    }
    // SB3 (URLClassPath fix): Spring Boot's launcher creates a URLClassPath
    // instance whose `<init>` never runs through any of our dispatch paths
    // (likely via fat-jar reflective allocation / sun.misc.Unsafe path),
    // so the instance fields `path`, `loaders`, `lmap`, `unopenedUrls`
    // remain at their default `null` value. The next call into
    // `findResources(String)` reads `loaders` (or `path`) and NPEs.
    //
    // Defensively populate any missing instance fields on every entry to
    // `findResources` and `getResource` (and the inner-class Enumeration's
    // `getLoader(int)`) so the bytecode method body can continue to run
    // safely.  Fields are populated by name (no slot-index guessing) so
    // any future shape drift is benign.
    // gc-common w20-c: the allocation and the `<init>` call can each collect,
    // and `this` (then the new collection) was written raw after them; each
    // call also left the next one a stale `this`. `ucp_ensure_field` pins
    // both and answers `this`'s current address.
    fn ucp_ensure_field(
        ctx: &mut dyn NativeContext,
        this: ObjectRef,
        field: &str,
        init_class: &str,
    ) -> Result<ObjectRef, MethodCallFailed> {
        if let Value::Object(Some(_)) = ctx.get_field_by_name(this, field) {
            return Ok(this);
        }
        let mut scope = NativeHandleScope::new(ctx);
        let this_h = scope.root(this);
        let new_obj = match scope.new_object_initialized(init_class, "()V", &[])? {
            Some(Value::Object(Some(o))) => o,
            _ => return Ok(scope.get(&this_h)),
        };
        let this = scope.get(&this_h);
        scope.set_field_by_name(this, field, Value::Object(Some(new_obj)));
        Ok(this)
    }
    fn ucp_lazy_init_fields(
        ctx: &mut dyn NativeContext,
        this: ObjectRef,
    ) -> Result<(), MethodCallFailed> {
        let this = ucp_ensure_field(ctx, this, "path", "java/util/ArrayList")?;
        let this = ucp_ensure_field(ctx, this, "loaders", "java/util/ArrayList")?;
        let this = ucp_ensure_field(ctx, this, "lmap", "java/util/HashMap")?;
        let _ = ucp_ensure_field(ctx, this, "unopenedUrls", "java/util/ArrayDeque")?;
        Ok(())
    }

    registry.register(
        "jdk/internal/loader/URLClassPath",
        "<init>",
        "([Ljava/net/URL;Ljava/net/URLStreamHandlerFactory;)V",
        ucp_init_2, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "jdk/internal/loader/URLClassPath",
        "<init>",
        "([Ljava/net/URL;)V",
        ucp_init_1, cratonvm_native_api::NativeKind::Bridge,
    );

    // SB3 (URLClassPath fix continued): override `getLoader(I)` and
    // `findResources(String)` to be NPE-tolerant. When the URLClassPath
    // instance has its `loaders`/`path`/`lmap`/`unopenedUrls` fields
    // unset (because the bytecode `<init>` never executed for this
    // instance — Spring Boot's launcher allocates URLClassPath objects
    // through a path our dispatcher doesn't see), the bytecode for
    // `getLoader(int)` reads `loaders` and NPEs on `loaders.size()`.
    // Lazily populate the fields here, then return null (== no loader at
    // this index, equivalent to past-end-of-collection).
    fn ucp_get_loader_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        ucp_lazy_init_fields(ctx, this)?;
        // After ensuring fields, replicate the JDK semantics: return null
        // when there's no loader for the requested index. Our lazy-init
        // produces empty collections, so every index is past-end, which
        // matches the "no more URLs to open" exit path.
        Ok(Some(Value::Object(None)))
    }
    fn ucp_find_resources(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => {
                // Return an empty Enumeration — synthetic Enumeration$Impl
                // with size=0.
                let enm = ctx.new_object("java/util/Enumeration$Impl")?.and_then(|v| {
                    if let Value::Object(Some(o)) = v {
                        Some(o)
                    } else {
                        None
                    }
                });
                return Ok(enm.map(|o| Value::Object(Some(o))));
            }
        };
        ucp_lazy_init_fields(ctx, this)?;
        // Empty enumeration is correct after lazy-init (no loaders, no
        // resources). Caller (CompoundEnumeration) will move on to the
        // next URLClassPath in the chain.
        let enm = ctx.new_object("java/util/Enumeration$Impl")?;
        Ok(enm)
    }
    registry.register(
        "jdk/internal/loader/URLClassPath",
        "getLoader",
        "(I)Ljdk/internal/loader/URLClassPath$Loader;",
        ucp_get_loader_int, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "jdk/internal/loader/URLClassPath",
        "findResources",
        "(Ljava/lang/String;)Ljava/util/Enumeration;",
        ucp_find_resources, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- java.io.ObjectStreamClass native bindings (real-JDK mode) ---
    // ObjectStreamClass.<clinit> calls initNative(); without this registered
    // the class is flagged "initialized after non-critical exception" and
    // later reflective access to serialVersionUID throws InternalError
    // ("null::serialVersionUID cannot be accessed reflectively before
    //  java.lang.invoke is initialized"). In modern OpenJDK initNative() is
    // a no-op (legacy native state migrated to Java). hasStaticInitializer
    // must report the TRUTH: ObjectStreamClass.computeDefaultSUID folds
    // "<clinit>"/STATIC/"()V" into the SHA-1 digest iff the class declares a
    // static initializer. Returning a blanket false produces a serialVersionUID
    // that disagrees with the real JDK for any class that HAS a <clinit>,
    // breaking cross-VM deserialization (e.g. the Gradle test worker reading a
    // HotSpot-written WorkerConfig stream).
    registry.register(
        "java/io/ObjectStreamClass",
        "initNative",
        "()V",
        native_noop,
        NativeKind::Bridge,
    );
    registry.register(
        "java/io/ObjectStreamClass",
        "hasStaticInitializer",
        "(Ljava/lang/Class;Z)Z",
        |ctx, args| {
            Ok(Some(Value::Int(
                essential_class_has_static_initializer(ctx, args)? as i32,
            )))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/io/ObjectStreamClass",
        "hasStaticInitializer",
        "(Ljava/lang/Class;)Z",
        |ctx, args| {
            Ok(Some(Value::Int(
                essential_class_has_static_initializer(ctx, args)? as i32,
            )))
        },
        NativeKind::Bridge,
    );

    // Force VM.isJavaLangInvokeInited() to return true. In a normal JVM,
    // MethodHandleNatives.<clinit> eventually calls VM.setJavaLangInvokeInited()
    // to flip this flag; without it MethodHandleAccessorFactory.newFieldAccessor
    // throws InternalError("<class>::<field> cannot be accessed reflectively
    // before java.lang.invoke is initialized"). This breaks any reflective
    // field access during bootstrap (e.g. ObjectStreamClass SUID computation).
    // Our MH subsystem is effectively always initialized from the JDK's
    // accessor-factory perspective.
    registry.register(
        "jdk/internal/misc/VM",
        "isJavaLangInvokeInited",
        "()Z",
        |_ctx, _args| Ok(Some(Value::Int(1))), cratonvm_native_api::NativeKind::Bridge,
    );
    // The setter's only effect is to flip the flag the getter above reports,
    // and that getter is pinned to `true` unconditionally (see the rationale
    // immediately above). With the state it would write already forced, the
    // setter has nothing left to do.
    registry.register(
        "jdk/internal/misc/VM",
        "setJavaLangInvokeInited",
        "()V",
        native_noop, cratonvm_native_api::NativeKind::Bridge,
    );

    registry.register(
        "org/jboss/dmr/ModelNode",
        "<init>",
        "()V",
        native_dmr_model_node_init, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/jboss/dmr/ModelNode",
        "getType",
        "()Lorg/jboss/dmr/ModelType;",
        native_dmr_model_node_get_type, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/jboss/dmr/ModelNode",
        "isDefined",
        "()Z",
        native_dmr_model_node_is_defined, cratonvm_native_api::NativeKind::Bridge,
    );

    // C20: Locale bootstrap overrides — bypass the JRELocaleProviderAdapter
    // "should not come down here" fallback that blocks Jackson, H2 and
    // anything touching Locale.getDefault() formatting paths.
    locale_bootstrap::register(registry);

    // S-SB: Spring ApplicationStartup bootstrap — ensures
    // AbstractApplicationContext.getApplicationStartup() never returns null
    // even when ApplicationStartup.<clinit> was swallowed.
    spring_startup_bootstrap::register(registry);

    // `Locale.ROOT` / `ENGLISH` / `CANADA` / `US` were registered here with the
    // FIELD descriptor `Ljava/util/Locale;` and are gone (round 13 wave 13,
    // lane shadow5, SH4-2; `US`, held by a vm test until then, in round 14
    // wave 2, lane shadow): no door can dispatch a field-shaped key -- every
    // method descriptor starts with `(`, and `getstatic` has no registry path
    // in the interpreter or either JIT lowering
    // (`docs/feature-designs/native-dispatch-architecture.md`).

    // C22: ResourceBundle locale-data overrides — provide synthetic
    // English/US bundles for `sun.text.resources.FormatData` and friends
    // so Jackson's StdDateFormat / SimpleDateFormat bootstrap doesn't NPE
    // through the JDK Module / LocaleProviderAdapter chain.
    locale_resources::register(registry);

    // C24: Annotation natives — override JDK bytecode that would otherwise
    // call `AnnotationParser.parseAnnotations(getRawAnnotations(), getConstantPool(), this)`.
    // We don't expose the full `jdk.internal.reflect.ConstantPool` native API
    // (getUTF8At etc.), so the JDK path can't decode our raw bytes.  By
    // intercepting the public Class/Field/Method annotation methods directly
    // and returning AnnotationProxy objects built from our parsed
    // `class.annotations` data, we avoid the AnnotationFormatError entirely.
    register_annotation_overrides(registry);

    // T19.H5 (essential): AtomicReferenceFieldUpdater / AtomicIntegerFieldUpdater
    // / AtomicLongFieldUpdater.newUpdater factories.  Must be in
    // register_essential_natives because real-JDK mode never calls
    // register_synthetic_overrides, and `org.jboss.logmanager.ExtHandler.<clinit>`
    // (KC26 boot path) runs ARFU.newUpdater inside a class-init that
    // throws ClassCastException without our override.
    crate::atomic_updater::register_atomic_updater_natives(registry);

    // T19.3 / T19.H3 / T19.H5 (essential): Quarkus runtime + bootstrap
    // runner natives — `SerializedApplication.read`, `RunnerClassLoader.loadClass`,
    // `RuntimeValue/StartupContext/Timing` static-init replay.  Must be
    // in register_essential_natives so KC26 / Quarkus apps work in real-JDK mode.
    crate::quarkus_staticinit::register_quarkus_staticinit_natives(registry);

    // WP6.1 + WP6.2: JCA provider chain + MessageDigest natives. Required
    // in real-JDK mode because the JDK 25 bytecode for
    // `Security.getProviders` / `MessageDigest.getInstance` reaches into
    // `sun.security.util.Debug`, `Provider$ServiceKey`, and the spiMap
    // bootstrap which we don't fully support; intercepting at the public
    // surface lets `apps/digest_probe/DigestProbe.java` (and any other
    // `MessageDigest`-using app) run unmodified.  Companion class-init
    // shim for `javax/crypto/Cipher`, `java/security/Security`,
    // `java/security/Provider`, `sun/security/util/Debug`,
    // `sun/security/jca/Providers` lives in `jca::cipher`.
    crate::jca::register_jca_natives(registry);
    crate::jca::cipher::register_cipher_clinit_shim(registry);

    // WP6.3 dependency — `SecureRandom` is needed by the Cipher probe's
    // `new SecureRandom()` call.  The dedicated `securerandom` module
    // registers `<init>()` / `nextBytes` / etc. against the OS CSPRNG,
    // bypassing the real-JDK `SecureRandom.<init>` path that otherwise
    // calls `getDefaultPRNG` → `Providers.getProviderList()` (NPE
    // because we no-op `Providers.<clinit>`).  Originally registered
    // only in `register_synthetic_overrides`; promoted here so it
    // works in real-JDK mode too.
    crate::securerandom::register_random_and_securerandom_natives(registry);

    // S111r17: `Class.getPackage()` real-JDK override. The JDK Java body of
    // `Class.getPackage()` delegates to `ClassLoader.definePackage(c)` →
    // `getDefinedPackage(name)` (we override — see
    // `lang_class::i2_classloader_get_defined_package`) → `getNamedPackage(
    // name, module)` (we override) — but `getNamedPackage` doesn't have
    // access to the Class and so cannot populate
    // `implementationVersion` from the source jar's manifest. Result:
    // `SpringBootVersion.class.getPackage().getImplementationVersion()`
    // returns null, the JarURLConnection fallback path NPEs at line 61,
    // and the SB2.3.8 banner crashes before printing the version.
    // Registering the same `native_class_get_package` body that
    // synthetic-jdk uses fixes the version probe at the root.
    registry.register(
        "java/lang/Class",
        "getPackage",
        "()Ljava/lang/Package;",
        crate::lang_class::native_class_get_package, cratonvm_native_api::NativeKind::Bridge,
    );
    // JDK 11+ `Class.getPackageName()` — Spring / library probes call this
    // directly; it lived only under `register_synthetic_overrides` so
    // real-JDK mode hit `NoSuchMethodError` after StackWalker fixes.
    registry.register(
        "java/lang/Class",
        "getPackageName",
        "()Ljava/lang/String;",
        crate::lang_class::native_class_get_package_name, cratonvm_native_api::NativeKind::Bridge,
    );
    // Spring Boot loader calls `URL.setURLStreamHandlerFactory` very early.
    // Full `register_net_natives` is not always on the minimal classpath
    // ordering that `cratonvm-cli` uses before linkage runs.
    registry.register(
        "java/net/URL",
        "setURLStreamHandlerFactory",
        "(Ljava/net/URLStreamHandlerFactory;)V",
        native_url_set_stream_handler_factory_guard, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/net/URLConnection",
        "setContentHandlerFactory",
        "(Ljava/net/ContentHandlerFactory;)V",
        native_urlconnection_set_content_handler_factory, cratonvm_native_api::NativeKind::Bridge,
    );
    for permissions_cls in [
        "java/security/PermissionCollection",
        "java/security/Permissions",
    ] {
        registry.register(
            permissions_cls,
            "add",
            "(Ljava/security/Permission;)V",
            native_permissions_add, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            permissions_cls,
            "setReadOnly",
            "()V",
            native_permissions_set_read_only, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            permissions_cls,
            "isReadOnly",
            "()Z",
            native_permissions_is_read_only, cratonvm_native_api::NativeKind::Bridge,
        );
    }
    // `URI(String)` — same story: `JarFileArchive` and SB loader parse nested
    // jar URLs before Phase-30 net registration is guaranteed to run.
    registry.register(
        "java/net/URI",
        "<init>",
        "(Ljava/lang/String;)V",
        native_uri_init, cratonvm_native_api::NativeKind::Bridge,
    );

    // RETIRED 2026-08-21 (H22-1) — `register_hex_format_real_jdk_natives` used
    // to be called here, and it called `phases_late::register_p64_hex_format`
    // last. Between them they registered 42 natives on `java/util/HexFormat`
    // (16 + 26), of which 26 owned their slot and 25 of those 26 are declared
    // with a `Code` attribute in the JDK 25 image; the whole class is now left
    // to real JDK bytecode in every shipping mode.
    //
    // The premise above ("real-JDK bytecode for HexFormat produces empty
    // strings") is a 2026 statement about a Formatter static-init chain that
    // has since been wired. MEASURED 2026-08-21 on `cratonvm-r5.exe` with
    // `CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/HexFormat`, which makes
    // contract §1.4 enforced and is exactly what this deletion does
    // permanently: a 30-assertion HexFormat probe (`formatHex`, ranged
    // `formatHex`, `parseHex`, upper/lower, delimiter, prefix, suffix, all
    // five `toHexDigits` overloads, `fromHexDigits*`, `isHexDigit`,
    // `toString`, `equals`, `hashCode`, and the two panic paths W8-C15-2
    // named) is byte-identical to HotSpot 25.0.3+9 on stdout. `H14-3` §1
    // priced the same prefix over the 104-vector corpus at a net cost of ZERO.
    //
    // `register_p64_hex_format` itself is NOT deleted: under the
    // `synthetic-jdk` feature there is no `java/util/HexFormat` bytecode to
    // fall back to, and it stays reachable from `register_phase64_natives`,
    // whose `DELIBERATE_SYNTHETIC_ONLY_FAMILIES` verdict is already "REAL-JDK
    // BYTECODE: all 2 exclusive classes exist in JDK 25". This is the
    // difference between retiring a native and deleting it.
    // docs/known-issues/jdk-only/H22-1-*.md

    // String.format / Formatter — same root cause family as HexFormat.
    // Real-JDK bytecode for `String.format(String, Object...)` and
    // `java.util.Formatter.format(String, Object...)` produces ` 0:0:`
    // or empty output in real-JDK mode because the `Formatter`
    // dependency chain (`sun.util.locale.provider.LocaleProviderAdapter`,
    // `Locale.ENGLISH` static-init, etc.) isn't fully wired.  These
    // natives call into the existing `lang_string::native_string_format`
    // implementation which already supports `%s`, `%d`, `%x`, `%X`,
    // `%02x`, `%-10s`, `%5d`, `%n`, `%%`, `%f`, `%c`, `%b`, `%e`, `%E`
    // with flags, width, and precision — see `apps/format_probe`.
    //
    // JIT r23 wave 3, lane L6 (`CRATONVM_JDK_FORMATTER_BODIES`; R23-W4-FLIP:
    // default ON since wave 4, `=0` registers the stand-ins): the JDK's own
    // `String.format` / `formatted` / `java.util.Formatter` bodies run instead
    // of these stand-ins, so a format error carries the `Formatter` frames
    // (page `r23-w2-native-string-formatter-exceptions-lose-jdk-frames-FIXED-20261021.md`).
    if !jdk_formatter_bodies_enabled() {
        register_string_format_real_jdk_natives(registry);
    }

    // T19_H11_LOGGER_FINDER_BOOT_BYPASS — KC16 boot path
    // (`com.sun.jmx.defaults.JmxProperties.<clinit>`) reaches
    // `jdk.internal.logger.LazyLoggers.getLogger(name, module)` with a
    // null `module` because our `Reflection.getCallerClass()` stub
    // returns null on some boot frames. The downstream
    // `DefaultLoggerFinder.isSystem(Module)` then NPEs on
    // `m.getClassLoader()`. Two layered overrides:
    //
    //   1. `LazyLoggers.getLogger(String, Module)` — short-circuit by
    //      returning a CratonVM-minted `System.Logger` (W5: a working one,
    //      see `register_system_logger_methods`; it used to be an inert
    //      instance of the interface itself). KC16 only needs the return
    //      value to be non-null for `JmxProperties.<clinit>` to populate
    //      its static fields.
    //   2. `DefaultLoggerFinder.isSystem(Module)` — defensive: if any
    //      caller path bypasses (1) and reaches isSystem with null,
    //      return true (system-loader semantics for boot frames).
    //   3. `System.getLogger(String)` — same shape as (1), invoked by
    //      classes that don't pre-resolve `module`.
    //
    // KEEP (W3) on the two `isSystem` constants below. The JDK body is
    // `m.getClassLoader() == null || == platformClassLoader()` — a null-hostile
    // read of exactly the field that is unreliable here (that NPE is why (1)
    // and (2) exist), and its only effect is choosing between the system and
    // the application logger *dispatch path*. CratonVM has no SecurityManager
    // and no module-scoped logging isolation, so both paths end in the same
    // handler set; answering "system" keeps the boot frames alive instead of
    // NPE-ing on a module we synthesized.
    registry.register(
        "jdk/internal/logger/DefaultLoggerFinder",
        "isSystem",
        "(Ljava/lang/Module;)Z",
        |_ctx, _args| Ok(Some(Value::Int(1))), cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "jdk/internal/logger/LazyLoggers",
        "getLogger",
        "(Ljava/lang/String;Ljava/lang/Module;)Ljava/lang/System$Logger;",
        native_system_get_logger, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/System",
        "getLogger",
        "(Ljava/lang/String;)Ljava/lang/System$Logger;",
        native_system_get_logger, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/System",
        "getLogger",
        "(Ljava/lang/String;Ljava/util/ResourceBundle;)Ljava/lang/System$Logger;",
        native_system_get_logger_with_bundle, cratonvm_native_api::NativeKind::Bridge,
    );

    // T19_H11_SYSTEM_LOGGER_INTERFACE — the instance side of the logger the
    // three registrations above mint. See `register_system_logger_methods`
    // (and the block comment above it) for what the receiver is, why it is no
    // longer stamped with the `java/lang/System$Logger` INTERFACE, and why the
    // same method set stays registered on the interface name as well.
    register_system_logger_methods(registry, CRATON_SYSTEM_LOGGER_CLASS);
    register_system_logger_methods(registry, "java/lang/System$Logger");

    // T19_H14_OPENMBEAN — JMX OpenType / MXBean introspector translation
    // natives. Filters `java.lang.Object` methods out of
    // `MBeanIntrospector.getMethods(Class)` so KC16's `MemoryMXBean`
    // registration doesn't recurse through `Object.getClass` →
    // `Class.getAnnotatedInterfaces()` → `AnnotatedType[]` →
    // OpenDataException. Defence-in-depth: also short-circuits
    // `ConvertingMethod.from` and `MXBeanMappingFactory.mappingForType`
    // for problematic types.
    //
    // T19_M1_PLATFORM_MXBEANS — extends T19_H14 with a thread-local
    // visited-type cycle detector (depth ≤ MAX_MAPPING_DEPTH = 3) and
    // pre-registered CompositeType schemas for the 5 well-known
    // platform-MXBean composite types: `MemoryUsage`, `ThreadInfo`,
    // `LockInfo`, `MonitorInfo`, `StackTraceElement`. Hardens all 8
    // platform MXBeans (Memory, Thread, Runtime, ClassLoading,
    // Compilation, OperatingSystem, MemoryPool, MemoryManager,
    // GarbageCollector) against OpenDataException recursion at
    // `ManagementFactory.getPlatformMBeanServer()`.
    if let Some(h) = crate::hooks::JMX_HOOKS.get() { (h.register_jmx_openmbean_natives)(registry); }

    // WP7.1 — wire the proper `ServiceLoader` natives so JDBC drivers
    // declared in `META-INF/services/java.sql.Driver` are reachable to
    // `DriverManager.getConnection`. Registered LAST so it overrides any
    // earlier stub registrations of the same triples.
    jdbc::register_jdbc_driver_natives(registry);

    // Wave1.D — javax.xml.stream (StAX) cursor API backed by quick-xml.
    // Enables Maven, Spring XML config, Hibernate `hibernate.cfg.xml`,
    // and any tool that consumes XML to parse arbitrary documents
    // without depending on the JDK Xerces/XMLStringBuffer code path.
    xml_stax::register(registry);

    // Wave 2 D — DirectByteBuffer / Cleaner checkcast guard.
    //
    // JDK 25's `java.nio.Buffer.session()` performs a `getfield segment`
    // followed by a `checkcast jdk/internal/foreign/AbstractMemorySegmentImpl`.
    // The DirectByteBuffer constructor chain leaves the `segment` slot in
    // a state where a subsequent read returns a `Value::Long`-encoded
    // pointer (the long `address` field's bit pattern leaks through the
    // operand stack's CompactValue NaN-boxing because untagged 64-bit
    // slots cannot distinguish Long from Double). The checkcast then
    // hits a `Value::Double` on the operand stack and aborts the VM
    // with `internal error: checkcast: not an object reference` before
    // any println reaches the host.
    //
    // Routing `session()` through a native shim that returns null short-
    // circuits the bad checkcast — `ScopedMemoryAccess.putIntUnaligned`
    // already accepts a null session (it skips the
    // `checkValidStateRaw()` call) and the rest of the JDK's
    // direct-buffer surface treats null sessions as "no scope check
    // required". This is consistent with the existing
    // `Buffer$1.acquireSession` shim in
    // `native-builtins/src/shared_secrets_bridge.rs`, which already
    // returns null for the same scope-validation paths.
    //
    // Native dispatch is keyed on the *receiver* class for
    // `invokevirtual`, so the shim is registered for every concrete
    // subclass that the JDK 25 NIO hierarchy uses for direct/heap
    // buffers. (Buffer itself covers any rare invokespecial-on-Buffer
    // sites that bypass the virtual cache.)
    registry.register(
        "java/nio/FloatBuffer",
        "order",
        "()Ljava/nio/ByteOrder;",
        native_float_buffer_order, cratonvm_native_api::NativeKind::Bridge,
    );

    // JIT round 17 wave 1, lane nio: `checkSession()` checks a REAL session too
    // (`CRATONVM_BUFFER_CHECK_SESSION_SCOPE`, default ON, `=0` restores the old
    // body). Read once per registry, so the choice is per VM and no static
    // caches it. See `native_buffer_check_session_scoped`.
    //
    // JIT round 17 wave 2, lane nio2: under `--real-jdk` every `session` /
    // `checkSession` row this loop registers is a retired shadow (SBR3 NIOBUF,
    // lane 4's `CharBuffer` pair, `RETIRED_SHADOW_R17_NIO2_SESSION_TRIPLES`), so
    // `register()` lands them on `SyntheticStub` and the strict registry drops
    // them: `Buffer.session()`'s own bytecode answers every receiver, typed views
    // included. They still serve `--compatible`, where the VM's synthetic FFM
    // carriers exist; `isReadOnly` / `isDirect` stay Bridges (abstract on these
    // classes, reached only by VM-minted carriers).
    let check_session_scoped =
        true;
    for buf in [
        "java/nio/Buffer",
        "java/nio/ByteBuffer",
        "java/nio/MappedByteBuffer",
        "java/nio/DirectByteBuffer",
        "java/nio/HeapByteBuffer",
        "java/nio/CharBuffer",
        "java/nio/IntBuffer",
        "java/nio/LongBuffer",
        "java/nio/FloatBuffer",
        "java/nio/DoubleBuffer",
        "java/nio/ShortBuffer",
    ] {
        registry.register(
            buf,
            "session",
            "()Ljdk/internal/foreign/MemorySessionImpl;",
            |ctx, args| {
                // Record which receiver class this shim actually answered for,
                // so the JIT's thin `session()` helper can answer for the SAME
                // classes without the ~160 ns generic native funnel — and for
                // no others. See `crate::buffer_session` for why the eleven
                // registered names are not the right population to read.
                let Some(cratonvm_types::Value::Object(Some(this))) = args.first().copied() else {
                    return Ok(Some(cratonvm_types::Value::Object(None)));
                };
                crate::buffer_session::note_served_for_vm(ctx.vm_identity(), ctx.class_id_of_object(this).as_u32());
                // W7-89 follow-up (the `MemorySegment.asByteBuffer()` /
                // ByteBuffer liveness gap): the constant-null answer this shim
                // gives EVERY receiver is what makes it safe against the crash
                // above, but it is also why an FFM-arena-backed buffer's
                // `getInt`/`putInt`/etc never saw the arena close -- `session()`
                // never having a session to give is indistinguishable from a
                // real one it declined to report.
                //
                // Read `segment` the same defensive way `get_field` always
                // has: `resolve_field_index` + `get_field` returns whatever
                // `Value` variant this object's slot actually holds, typed by
                // the HEAP's own storage (not the bytecode operand stack's
                // NaN-boxing, which is what the crash above was in) -- so a
                // buffer whose `segment` slot holds the raw `Long`-encoded
                // address this comment describes reads back as `Value::Long`
                // here, not as an object, and falls through to the same null
                // answer as before. Only a genuine `Value::Object(Some(_))`
                // -- the shape `MemorySegment.asByteBuffer()`
                // (`panama.rs::pe_segment_as_byte_buffer`) actually writes --
                // resolves a session at all.
                let Some(seg_idx) = ctx.resolve_field_index("java/nio/Buffer", "segment") else {
                    return Ok(Some(cratonvm_types::Value::Object(None)));
                };
                match ctx.get_field(this, seg_idx) {
                    Value::Object(Some(segment)) => Ok(Some(
                        crate::phases_late::foreign_ffm::p67_receiver_session(ctx, segment)?,
                    )),
                    _ => Ok(Some(cratonvm_types::Value::Object(None))),
                }
            }, cratonvm_native_api::NativeKind::Bridge,
        );
        // `checkSession()` mirrors the real `Buffer.checkSession()` bytecode
        // (`session(); if (session != null) session.checkValidState();`) now
        // that `session()` above can answer non-null for a segment-backed
        // buffer. When it still answers null -- every buffer this was
        // originally written for -- this stays the same no-op it always was.
        if check_session_scoped {
            registry.register(
                buf,
                "checkSession",
                "()V",
                native_buffer_check_session_scoped, cratonvm_native_api::NativeKind::Bridge,
            );
        } else {
            registry.register(buf, "checkSession", "()V", |ctx, args| {
                let Some(Value::Object(Some(this))) = args.first().copied() else {
                    return Ok(None);
                };
                let Some(seg_idx) = ctx.resolve_field_index("java/nio/Buffer", "segment") else {
                    return Ok(None);
                };
                if let Value::Object(Some(segment)) = ctx.get_field(this, seg_idx) {
                    if let Value::Object(Some(mut session)) =
                        crate::phases_late::foreign_ffm::p67_receiver_session(ctx, segment)?
                    {
                        crate::phases_late::foreign_ffm::p67_session_check_valid(
                            ctx,
                            &mut session,
                        )?;
                    }
                }
                Ok(None)
            }, cratonvm_native_api::NativeKind::Bridge);
        }
        registry.register(buf, "isReadOnly", "()Z", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            match ctx.get_field_by_name(this, "isReadOnly") {
                Value::Int(v) => Ok(Some(Value::Int(if v != 0 { 1 } else { 0 }))),
                _ => {
                    let cname = ctx
                        .class_name_of_id(ctx.class_id_of_object(this))
                        .unwrap_or_default();
                    Ok(Some(Value::Int(
                        if cname.ends_with('R') || cname.contains("ReadOnly") {
                            1
                        } else {
                            0
                        },
                    )))
                }
            }
        }, cratonvm_native_api::NativeKind::Bridge);
        registry.register(buf, "isDirect", "()Z", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let cname = ctx
                .class_name_of_id(ctx.class_id_of_object(this))
                .unwrap_or_default();
            Ok(Some(Value::Int(
                if cname.contains("Direct") || cname.contains("Mapped") {
                    1
                } else {
                    0
                },
            )))
        }, cratonvm_native_api::NativeKind::Bridge);
    }

    // The four `Character.toLowerCase`/`toUpperCase` overrides that lived here
    // were DELETED on 2026-08-12. They were verbatim `Bridge` copies of the
    // `Intrinsic` bodies in `lang_math.rs`, and being registered later they WON
    // under last-write-wins. Measured with `--dump-native-registry`: the
    // `lang_math.rs` rows read `owns=false, invocations=0` while these read
    // `owns=true, invocations=32, overwrote:intrinsic`. Every fix applied to
    // the intrinsic bodies was therefore inert until this block went.
    //
    // Their stated reason -- a JIT miscompile of `CharacterData.of(I)` that
    // returned 0 and corrupted Spring's `BeanPropertyName.toDashedForm` -- has
    // since been fixed at its root in `jit/src/x64/driver.rs`
    // (`has_dispatch` / `set_jit_thread`). Re-measured 2026-08-12: 6,000,000
    // cold-vs-hot invocations each of the UNSHADOWED `getType`, `toTitleCase`,
    // `isSpaceChar`, `isAlphabetic` and `getDirectionality` -- 0 mismatches,
    // byte-identical to HotSpot. The workaround outlived its defect.
    //
    // They were also wrong on their own terms. `char::to_lowercase()` yields
    // the Unicode FULL mapping (SpecialCasing) where Java specifies the SIMPLE
    // one (UnicodeData field 12), and `char::from_u32(surrogate)` is `None`, so
    // `.unwrap_or(cp)` answered NUL for a lone surrogate -- a legal `char`.
    // A BMP sweep over all 65,536 code points measured 2,051 wrong
    // `toLowerCase` answers and 2,153 wrong `toUpperCase`.

    // ParameterFormatter-fix: real-JDK mode bypass for ZoneId.systemDefault.
    //
    // In real-JDK mode the JDK bytecode for ZoneId.systemDefault() walks
    // TimeZone.getDefault() → ZoneInfo.getTimeZone(...) → triggers
    // sun/util/calendar/ZoneInfoFile.<clinit>, which fails to load
    // ${java.home}/lib/tzdb.dat in CratonVM (DataInputStream/binary
    // parsing path is incomplete) and is silently swallowed as
    // java/lang/Error.  After the swallow, ZoneInfoFile is marked
    // Initialized but its `zones` map is empty, so all subsequent
    // TimeZone lookups return null and ZoneId.systemDefault() returns
    // null.  Downstream, log4j's
    // ParameterFormatter.<clinit> calls
    //   DateTimeFormatter.ofPattern(...).withZone(ZoneId.systemDefault())
    // and `withZone(null)` immediately NPEs, blowing up the entire
    // log4j logging stack and stalling WildFly bootstrap.
    //
    // HHH-10372 correctness fix (2026-07-17): the original version of this
    // bypass (still present as the fallback below) *always* returned a
    // hardcoded UTC ZoneOffset, ignoring any `TimeZone.setDefault(...)`
    // call the running program had made — a host-timezone-leak that
    // silently broke every caller relying on a *settable* JVM default,
    // not just log4j's boot-time NPE guard. Concretely: Hibernate's
    // `ZonedDateTimeTest`/`LocalDateTimeTest` (`Timezones.withDefaultTimeZone()`
    // test helper) call `TimeZone.setDefault(...)` then read it back via
    // `ZoneId.systemDefault()` — with the old hardcoded-UTC bypass, that
    // always resolved to UTC regardless of what was set, producing
    // timezone-offset-sized value corruption (63/608 parameterized
    // failures, e.g. `expected: <2017-11-06 09:19:01.0> but was:
    // <2017-11-06 01:19:01.0>`, matching whatever offset `TimeZone.setDefault`
    // was called with).
    //
    // Fix: first try to resolve the *actual current* default zone via
    // `TimeZone.getDefault()` (also natively overridden below in the
    // "Round 54" fix, and confirmed to correctly track `setDefault(...)`
    // — it does not touch the broken ZoneInfoFile/tzdb.dat path) and hand
    // its id to the real bytecode `ZoneId.of(String)` (unregistered/native-free
    // in real-JDK mode, so it runs the actual JDK parsing+tzdb-rules logic,
    // which already works correctly for explicit `ZoneId.of(...)` calls
    // elsewhere in this same test suite — DST rules included). Only fall
    // back to the original hardcoded synthetic UTC ZoneOffset (preserving
    // the exact previous behavior) if that chain fails for any reason
    // (e.g. too early in boot for TimeZone/ZoneId to be ready yet) — the
    // log4j NPE guard this bypass exists for must never regress.
    registry.register(
        "java/time/ZoneId",
        "systemDefault",
        "()Ljava/time/ZoneId;",
        |ctx, _args| {
            if let Ok(Some(Value::Object(Some(tz)))) = ctx.invoke(
                "java/util/TimeZone",
                "getDefault",
                "()Ljava/util/TimeZone;",
                &[],
            ) {
                if let Ok(Some(Value::Object(Some(id_str)))) = ctx.invoke(
                    "java/util/TimeZone",
                    "getID",
                    "()Ljava/lang/String;",
                    &[Value::Object(Some(tz))],
                ) {
                    let id_string = ctx.read_string(id_str).filter(|s| !s.is_empty());
                    if let Some(id_string) = id_string {
                        // TZDB-OFFSET (2026-07-22): `ZoneId.of(String)`'s
                        // single-arg overload does not understand the
                        // legacy 3-letter `ZoneId.SHORT_IDS` aliases (e.g.
                        // "CTT") that `java.util.TimeZone` still accepts —
                        // it would throw, which this native's `if let Ok`
                        // guard silently swallowed, falling through to the
                        // hardcoded-UTC bypass below and making
                        // `ZoneId.systemDefault()` return UTC instead of
                        // the zone `TimeZone.setDefault` was just set to.
                        // Resolve through the same real tzdb.dat catalog
                        // used for offset computation first so the id
                        // passed to `ZoneId.of` is always a canonical IANA
                        // name it accepts unaided; fall back to the raw id
                        // (still correct for "UTC"/"GMT"/custom "+HH:MM"
                        // forms `ZoneId.of` handles natively) if this
                        // catalog doesn't recognize it. See
                        // bug-h2-timezone-zonerules-offset-miscalculation-FIXED.md.
                        // Try the id verbatim first (preserves display
                        // names/behavior for everything that already
                        // worked, e.g. "UTC"/"GMT"/"Zulu"/"+08:00"/plain
                        // IANA names); only fall back to the tzdb-resolved
                        // canonical name for legacy short-id aliases the
                        // single-arg overload alone doesn't understand.
                        let orig_str = ctx.create_string(&id_string);
                        if let Ok(Some(zone @ Value::Object(Some(_)))) = ctx.invoke(
                            "java/time/ZoneId",
                            "of",
                            "(Ljava/lang/String;)Ljava/time/ZoneId;",
                            &[Value::Object(Some(orig_str))],
                        ) {
                            return Ok(Some(zone));
                        }
                        if let Some(resolved_id) = crate::tzdb::canonical_zone_id(ctx, &id_string) {
                            let resolved_str = ctx.create_string(&resolved_id);
                            if let Ok(Some(zone @ Value::Object(Some(_)))) = ctx.invoke(
                                "java/time/ZoneId",
                                "of",
                                "(Ljava/lang/String;)Ljava/time/ZoneId;",
                                &[Value::Object(Some(resolved_str))],
                            ) {
                                return Ok(Some(zone));
                            }
                        }
                    }
                }
            }
            // Fallback: original hardcoded-UTC synthetic bypass (see doc
            // comment above) — allocate a synthetic instance of ZoneOffset
            // (a concrete ZoneId subclass with a single int totalSeconds
            // field). Using the abstract ZoneId class directly may break
            // instanceof checks downstream; ZoneOffset.UTC is the
            // canonical way to get a non-null ZoneId without touching
            // ZoneInfoFile.
            let cid = match ctx.ensure_class_initialized("java/time/ZoneOffset") {
                Ok(id) => id,
                Err(_) => return Ok(Some(Value::Object(None))),
            };
            // ZoneOffset has fields: totalSeconds(I), id(String).
            // Allocate enough slots — we conservatively use 8 to
            // tolerate JDK-version field drift, then set totalSeconds=0
            // and id="Z" via slot 0/1.  The JDK uses the id field for
            // toString(); 0-init for the rest is safe since ZoneOffset
            // does not consult any other fields in withZone() path.
            // Best-effort field set — failures are non-fatal because
            // the JVM has already had the constructor skipped.  Any
            // downstream getter that requires a fully-initialised
            // ZoneOffset would have failed via the ZoneInfoFile path
            // anyway.
            //
            // gc-common w20-c: the string is allocated FIRST and pinned
            // across the object's allocation; allocated second, it could
            // collect and leave `obj` stale for the stores below.
            let s = ctx.create_string("Z");
            let s_pin = ctx.pin_native_root(s);
            let obj: ObjectRef = ctx.alloc_object(cid, 8);
            let s: ObjectRef = ctx.read_native_pin(s_pin, s);
            ctx.unpin_native_roots(s_pin);
            ctx.set_field(obj, 0, Value::Int(0));
            ctx.set_field(obj, 1, Value::Object(Some(s)));
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // Round 54: real-JDK mode bypass for java.util.TimeZone.getTimeZone /
    // getDefault / setDefaultZone and sun/util/calendar/ZoneInfoFile.
    //
    // Jackson 2.x `MapperBuilder.<clinit>` -> `BaseSettings.<clinit>`
    // calls `TimeZone.getTimeZone("UTC")`, which triggers the real JDK
    // bytecode path `TimeZone.getTimeZone -> ZoneInfo.getTimeZone ->
    // ZoneInfoFile.getZoneInfo -> ZoneInfoFile.getZoneInfo0`, and the
    // latter calls `Arrays.binarySearch(regions, ...)` on a NULL
    // `regions` static array because the ZoneInfoFile.<clinit> failed
    // earlier to load `${java.home}/lib/tzdb.dat` (silently swallowed
    // as `java/lang/Error`). The NPE then propagates out as
    // `RuntimeException("Invalid binary time-zone data: TZDB:UTC")`,
    // blocking Spring's Jackson `ObjectMapper` bean, which transitively
    // blocks `ServletWebServerFactoryAutoConfiguration` —
    // `MissingWebServerFactoryBeanException`.
    //
    // Round 13 fixed `ZoneId.systemDefault()` the same way (returned a
    // synthetic `ZoneOffset` short-circuiting the failing path); this
    // is the matching fix for the legacy `java.util.TimeZone` API.
    //
    // The fix: register `TimeZone.getTimeZone(String)` and
    // `TimeZone.getDefault()` to allocate a concrete TimeZone subclass
    // instance (`sun/util/calendar/ZoneInfo` extends `TimeZone`) with
    // the given ID and a zero offset. Jackson does not use the returned
    // TimeZone for arithmetic at clinit time — just needs a non-null
    // object whose `getID()` works.
    //
    // Also register `ZoneInfoFile.getZoneInfo0(String)` as a safety net
    // for any direct caller — returns null so callers fall through to
    // GMT.
    // HIB-CV-34: standard (non-DST) UTC offset in seconds for common IANA zone
    // ids. Mirrors `util_time::iana_zone_offset_seconds`, duplicated here because
    // that module is `#[cfg(feature = "synthetic-jdk")]`-gated and not compiled
    // in the default real-JDK build, where `alloc_synth_timezone` lives.
    fn tz_standard_offset_seconds(zone_id: &str) -> Option<i32> {
        match zone_id {
            "UTC" | "GMT" | "Etc/UTC" | "Etc/GMT" | "Z" | "Atlantic/Reykjavik"
            | "Europe/London" | "GB" => Some(0),
            "US/Eastern" | "America/New_York" | "America/Bogota" => Some(-5 * 3600),
            "US/Central" | "America/Chicago" | "America/Mexico_City" => Some(-6 * 3600),
            "US/Mountain" | "America/Denver" => Some(-7 * 3600),
            "US/Pacific" | "America/Los_Angeles" => Some(-8 * 3600),
            "America/Anchorage" => Some(-9 * 3600),
            "Pacific/Honolulu" | "US/Hawaii" => Some(-10 * 3600),
            // Deprecated three-letter abbreviations still accepted by
            // `TimeZone.getTimeZone` (real-JDK rawOffset is fixed/non-DST):
            // EST=-5, CST=-6, MST=-7, PST=-8, HST=-10. Without these the
            // synthetic zone falls back to rawOffset 0, shifting per-session
            // `jdbcTimeZone("CST")` JDBC timestamps by the full offset
            // (DatabaseTimeZoneMultiTenancyTest: read-back +6h).
            "EST" => Some(-5 * 3600),
            "CST" => Some(-6 * 3600),
            "MST" => Some(-7 * 3600),
            "PST" => Some(-8 * 3600),
            "HST" => Some(-10 * 3600),
            "America/Sao_Paulo" | "America/Argentina/Buenos_Aires" => Some(-3 * 3600),
            "America/Santiago" => Some(-4 * 3600),
            "Europe/Paris" | "Europe/Berlin" | "Europe/Rome" | "Europe/Madrid" | "CET"
            | "Europe/Oslo" | "Europe/Amsterdam" | "Europe/Brussels" | "Europe/Vienna"
            | "Europe/Copenhagen" | "Europe/Stockholm" | "Europe/Zurich" | "Europe/Warsaw"
            | "Europe/Prague" | "Europe/Budapest" => Some(3600),
            "Europe/Athens" | "Europe/Bucharest" | "Europe/Helsinki" | "EET" => Some(2 * 3600),
            "Europe/Moscow" | "Europe/Istanbul" => Some(3 * 3600),
            "Asia/Dubai" => Some(4 * 3600),
            "Asia/Karachi" => Some(5 * 3600),
            "Asia/Kolkata" | "Asia/Calcutta" => Some(5 * 3600 + 1800),
            "Asia/Dhaka" => Some(6 * 3600),
            "Asia/Bangkok" | "Asia/Jakarta" => Some(7 * 3600),
            "Asia/Shanghai" | "Asia/Hong_Kong" | "Asia/Singapore" | "Asia/Taipei" | "PRC"
            | "Australia/Perth" => Some(8 * 3600),
            "Asia/Tokyo" | "Japan" | "Asia/Seoul" | "ROK" => Some(9 * 3600),
            "Australia/Adelaide" => Some(9 * 3600 + 1800),
            "Australia/Sydney" | "Australia/Melbourne" => Some(10 * 3600),
            "Pacific/Auckland" | "NZ" | "Pacific/Fiji" => Some(12 * 3600),
            "Africa/Cairo" | "Africa/Johannesburg" => Some(2 * 3600),
            "Africa/Lagos" => Some(3600),
            "Africa/Nairobi" => Some(3 * 3600),
            _ => {
                // GMT+HH:MM / -HH:MM and Etc/GMT±N (inverted sign).
                if let Some(rest) = zone_id.strip_prefix("Etc/GMT") {
                    rest.parse::<i32>().ok().map(|h| -h * 3600)
                } else {
                    let body = zone_id
                        .strip_prefix("GMT")
                        .or_else(|| zone_id.strip_prefix("UTC"))
                        .unwrap_or(zone_id);
                    if body.starts_with('+') || body.starts_with('-') {
                        let sign = if body.starts_with('-') { -1 } else { 1 };
                        let parts: Vec<&str> = body[1..].split(':').collect();
                        let h: i32 = parts.first()?.parse().ok()?;
                        let m: i32 = parts.get(1).and_then(|p| p.parse().ok()).unwrap_or(0);
                        Some(sign * (h * 3600 + m * 60))
                    } else {
                        None
                    }
                }
            }
        }
    }

    // Real JDK `TimeZone.getTimeZone(id)` canonicalises a custom GMT offset id
    // ("GMT+2", "GMT+0800", "GMT-5:30", ...) into the form "GMT±HH:MM" (e.g.
    // "GMT+2" -> "GMT+02:00"), and `getID()` returns that normalised string.
    // This is a faithful port of `java.util.TimeZone.parseCustomTimeZone`. IANA
    // ids ("America/New_York"), "UTC"/"GMT" alone, and anything that fails the
    // custom-offset grammar return `None` and are kept verbatim by the caller.
    fn normalize_gmt_custom_id(id: &str) -> Option<String> {
        const GMT_ID_LENGTH: usize = 3;
        if !id.starts_with("GMT") {
            return None;
        }
        let chars: Vec<char> = id.chars().collect();
        let length = chars.len();
        if length < GMT_ID_LENGTH + 1 {
            return None; // "GMT" alone is the named GMT zone, not a custom offset.
        }
        let mut index = GMT_ID_LENGTH;
        let negative = match chars[index] {
            '-' => true,
            '+' => false,
            _ => return None,
        };
        index += 1;
        let mut hours: i32 = 0;
        let mut num: i32 = 0;
        let mut count_delim = 0;
        let mut len = 0;
        while index < length {
            let c = chars[index];
            index += 1;
            if c == ':' {
                if count_delim > 0 || len > 2 {
                    return None;
                }
                hours = num;
                count_delim += 1;
                num = 0;
                len = 0;
                continue;
            }
            if !c.is_ascii_digit() {
                return None;
            }
            num = num * 10 + (c as i32 - '0' as i32);
            len += 1;
        }
        if count_delim == 0 {
            if len <= 2 {
                hours = num;
                num = 0;
            } else {
                hours = num / 100;
                num %= 100;
            }
        } else if len != 2 {
            return None;
        }
        if hours > 23 || num > 59 {
            return None;
        }
        let sign = if negative { '-' } else { '+' };
        Some(format!("GMT{}{:02}:{:02}", sign, hours, num))
    }

    /// DST rule for a zone id, as the trailing 11 int args of
    /// `java.util.SimpleTimeZone`'s full constructor:
    /// `(startMonth, startDay, startDayOfWeek, startTime, startTimeMode,
    ///   endMonth, endDay, endDayOfWeek, endTime, endTimeMode, dstSavings)`.
    /// Month/day-of-week values are `java.util.Calendar` constants
    /// (JANUARY=0..; SUNDAY=1); `day = -1` means "last <dayOfWeek> of month";
    /// time-mode 0 = WALL_TIME, 2 = UTC_TIME. Only zones whose CURRENT
    /// (2018+) recurring rule SimpleTimeZone can express exactly are listed —
    /// the rest keep the standard-offset-year-round approximation (the
    /// HIB-CV-34 known limitation). Historical rule changes are NOT modeled.
    fn tz_dst_rule(zone_id: &str) -> Option<[i32; 11]> {
        const SUNDAY: i32 = 1;
        const MARCH: i32 = 2;
        const APRIL: i32 = 3;
        const SEPTEMBER: i32 = 8;
        const OCTOBER: i32 = 9;
        const NOVEMBER: i32 = 10;
        const WALL_TIME: i32 = 0;
        const UTC_TIME: i32 = 2;
        const HOUR: i32 = 3_600_000;
        match zone_id {
            // EU rule: DST from last Sunday of March 01:00 UTC to last Sunday
            // of October 01:00 UTC, +1h. Applies to the whole CET family and
            // (with a 0 standard offset) Europe/London.
            "CET" | "Europe/Paris" | "Europe/Berlin" | "Europe/Rome" | "Europe/Madrid"
            | "Europe/Oslo" | "Europe/Amsterdam" | "Europe/Brussels" | "Europe/Vienna"
            | "Europe/Copenhagen" | "Europe/Stockholm" | "Europe/Zurich" | "Europe/Warsaw"
            | "Europe/Prague" | "Europe/Budapest" | "Europe/London" | "GB" | "Europe/Athens"
            | "Europe/Bucharest" | "Europe/Helsinki" => Some([
                MARCH, -1, SUNDAY, HOUR, UTC_TIME, OCTOBER, -1, SUNDAY, HOUR, UTC_TIME, HOUR,
            ]),
            // US rule (2007+): DST from second Sunday of March 02:00
            // wall time to first Sunday of November 02:00 wall time, +1h.
            "US/Pacific" | "America/Los_Angeles" | "PST" => Some([
                MARCH,
                8,
                -SUNDAY,
                2 * HOUR,
                WALL_TIME,
                NOVEMBER,
                1,
                -SUNDAY,
                2 * HOUR,
                WALL_TIME,
                HOUR,
            ]),
            // New Zealand rule: DST from last Sunday of September 02:00 wall
            // to first Sunday of April 03:00 wall (02:00 standard), +1h.
            "Pacific/Auckland" | "NZ" => Some([
                SEPTEMBER,
                -1,
                SUNDAY,
                2 * HOUR,
                WALL_TIME,
                APRIL,
                1,
                SUNDAY,
                3 * HOUR,
                WALL_TIME,
                HOUR,
            ]),
            _ => None,
        }
    }

    // HIB-DST-STARTYEAR (2026-07-17): historical DST-adoption year for
    // zones in the `tz_dst_rule` "EU rule" branch above.
    //
    // `tz_dst_rule` only knows the zone's CURRENT/modern recurring DST
    // rule and applies it to every date unconditionally — including dates
    // long before the zone had daylight saving at all. Confirmed live
    // (standalone repro against real HotSpot JDK 25, cross-checked with
    // `sun.util.calendar.ZoneInfo`'s own legacy `TimeZone.getOffset(long)`
    // path, not just `java.time`):
    // `TimeZone.getTimeZone("Europe/Amsterdam").getOffset(epochMillisFor(
    // "1892-04-01T00:00:00Z"))` returned 7200000ms (+2h, modern DST rule
    // wrongly applied) on CratonVM vs 3600000ms (+1h, no DST — HotSpot's
    // own zone data correctly has no DST that far back) on real HotSpot.
    // This is a distinct bug from HIB-PARIS-LMT above: that one is a
    // one-time historical rawOffset cutover (LMT precision); this one is
    // the recurring DST *rule itself* being retroactively misapplied to
    // an era before the zone had DST in any form.
    //
    // Each value below is the first year real HotSpot JDK 25's own legacy
    // `TimeZone.getOffset(long)` path reports ANY winter/summer offset
    // split for that zone at all — i.e. the smallest year where
    // `getOffset()` at a fixed mid-January instant differs from
    // `getOffset()` at a fixed mid-July instant of the same year — found
    // by scanning year-by-year from 1850 against real HotSpot JDK 25
    // directly (not derived from a general historical claim: per this
    // project's own hard-won lesson from HIB-PARIS-LMT, HotSpot's compiled
    // legacy tzdata does not always carry the textbook-historical answer;
    // what this project targets is matching HotSpot, not the real world).
    //
    // `alloc_synth_timezone` gates the constructed `SimpleTimeZone` with
    // real `setStartYear(int)` bytecode using this value — the exact
    // real-JDK mechanism for exactly this purpose (`SimpleTimeZone`'s own
    // `getOffset`/`getOffsets` bytecode checks `year < startYear` and
    // returns `rawOffset` with no DST applied when it's before the start
    // year — see `javap -c java.util.SimpleTimeZone`), so no calendar-math
    // reimplementation is needed on the Rust side; the real class does the
    // gating itself, exactly as it would for a real HotSpot-constructed
    // `SimpleTimeZone`.
    //
    // SCOPE LIMIT: this is a single flip year per zone, not full
    // historical tzdata. Real HotSpot's own zone data for every zone below
    // has a much messier history AFTER this adoption year — WWI-era DST
    // suspended again in some zones during the interwar years, WWII
    // occupation-driven changes to the *winter* (raw) offset itself
    // (independent of any DST rule), and a widespread POST-WWII
    // suspension of DST across Europe not reintroduced until the 1970s
    // oil-crisis era / the 1996 EU-wide harmonization that `tz_dst_rule`'s
    // modern rule actually models. Gating on just the first-ever-adoption
    // year does NOT make CratonVM match HotSpot for that entire messy
    // 1916(ish)-1980(ish) middle era — it only removes the strictly-wrong
    // "DST applied to a date before the zone had DST at all" case, which
    // is this fix's actual target (pre-20th-century dates, and more
    // generally any date before each zone's real first-ever DST year).
    // That intermediate-era imperfection is pre-existing — CratonVM's flat
    // modern-rule model could never have matched that era, gated or not —
    // and is unchanged by this fix, not a new regression.
    fn dst_start_year(zone_id: &str) -> Option<i32> {
        match zone_id {
            // Europe/Paris: real HotSpot's legacy path first shows a
            // winter/summer offset split in 1911 — the same year as the
            // HIB-PARIS-LMT LMT->WET rawOffset cutover above (France
            // adopted WET, then DST, in short order).
            "Europe/Paris" => Some(1911),
            // WWI-era DST adoption block: CET, Germany, Italy, Norway,
            // Netherlands, Belgium, Austria, Denmark, Sweden, Poland,
            // Czechia, Hungary, UK all first show a winter/summer split
            // in 1916 against real HotSpot.
            "CET" | "Europe/Berlin" | "Europe/Rome" | "Europe/Oslo" | "Europe/Amsterdam"
            | "Europe/Brussels" | "Europe/Vienna" | "Europe/Copenhagen" | "Europe/Stockholm"
            | "Europe/Warsaw" | "Europe/Prague" | "Europe/Budapest" | "Europe/London" | "GB" => {
                Some(1916)
            }
            // Spain: real HotSpot's legacy path shows an earlier (1901)
            // Madrid-Mean-Time -> WET rawOffset switch that is NOT a DST
            // split (winter == summer that year); the first genuine
            // winter/summer split is 1918.
            "Europe/Madrid" => Some(1918),
            "Europe/Helsinki" => Some(1921),
            "Europe/Bucharest" => Some(1932),
            // Switzerland: real HotSpot shows no DST split at all until
            // the short-lived 1941 wartime DST.
            "Europe/Zurich" => Some(1941),
            // Greece: real HotSpot's legacy path shows an earlier (1917)
            // LMT -> EET rawOffset switch that is NOT a DST split (winter
            // == summer that year); the first genuine winter/summer split
            // is 1943 (wartime).
            "Europe/Athens" => Some(1943),
            _ => None,
        }
    }

    // HIB-PARIS-LMT (2026-07-17, superseded 2026-07-22): this used to be a
    // small hand-picked table of pre-standardization Local Mean Time
    // cutovers (see git history for the original `historical_lmt_offset`).
    // Superseded by the TZDB-OFFSET fix below, which reads the real
    // historical cutover for every zone directly from tzdb.dat instead of
    // hand-listing one zone at a time — see
    // `bug-h2-timezone-zonerules-offset-miscalculation-FIXED.md`.

    fn alloc_synth_timezone(
        ctx: &mut dyn NativeContext,
        id_str: &str,
    ) -> Result<cratonvm_types::Value, MethodCallFailed> {
        // C12-1 (2026-08-12): the DST-aware `tz_dst_rule` branch that used to
        // stand here — construct a real `java.util.SimpleTimeZone` through its
        // 13-arg constructor and gate it with `setStartYear` — is DELETED.
        //
        // Three independent reasons, in order of weight:
        //
        //  1. It is superseded. The TZDB-OFFSET note below states it in the
        //     file's own words: the tzdb path "Supersedes the previous
        //     `tz_dst_rule`/`dst_start_year`/`historical_lmt_offset`
        //     hand-rolled approximations (a ~20-zone allowlist modeling only
        //     each zone's *current* recurring DST rule) — this covers every
        //     zone's full historical transition table instead." The
        //     approximation's consumers were retired then; the branch that
        //     PRODUCED it was not.
        //  2. It disagrees with the oracle. Measured on HotSpot 25.0.3+9-LTS:
        //     `TimeZone.getTimeZone("America/New_York").getClass()` is
        //     `sun.util.calendar.ZoneInfo`, for every id tried and never a
        //     `SimpleTimeZone`. The `ZoneInfo` path below is the shape real
        //     HotSpot returns, so this deletion moves toward the oracle.
        //  3. It is what made the `SimpleTimeZone` native family look
        //     justified. Those four natives existed to make THIS fabricated
        //     object answer from tzdb, and being per-CLASS they also captured
        //     every `new SimpleTimeZone(rawOffset, id)` an application builds
        //     — see the unregistration below and
        //     `docs/feature-designs/native-dispatch-architecture.md`.
        //     With no fabricated `SimpleTimeZone` left to serve, the
        //     registration has no remaining constituency at all.
        //
        // `tz_dst_rule` and `dst_start_year` above are now unreferenced. They
        // are left in place only so this diff is one behaviour change rather
        // than two; they have NO callers and must not acquire new ones —
        // `crate::tzdb` is the single source of transition data.
        // Prefer sun/util/calendar/ZoneInfo (concrete subclass of TimeZone).
        // Fall back to allocating with class-id 0 if init fails — the
        // caller only needs an object whose `getID()` returns id_str.
        let cid = match ctx.ensure_class_initialized("sun/util/calendar/ZoneInfo") {
            Ok(c) => c,
            Err(_) => match ctx.ensure_class_initialized("java/util/TimeZone") {
                Ok(c) => c,
                Err(_) => return Ok(cratonvm_types::Value::Object(None)),
            },
        };
        // Resolved BEFORE the allocation, deliberately. `tzdb::raw_offset_
        // seconds` reads a process-global cache and, on the first call,
        // `$JAVA_HOME/lib/tzdb.dat` off the filesystem — it never enters Java
        // and cannot move a heap object today. Computing it up here means the
        // next reader does not have to re-establish that, and it costs
        // nothing.
        let raw_offset_ms = crate::tzdb::raw_offset_seconds(ctx, id_str)
            .or_else(|| tz_standard_offset_seconds(id_str))
            .unwrap_or(0)
            .saturating_mul(1000);
        // Field count: be generous (16) to cover both TimeZone (ID) and
        // ZoneInfo subclass fields (rawOffset, transitions, etc.).
        // gc-common w20-c: the string first, pinned across the object's
        // allocation (allocated second, it could leave `obj` stale).
        let s = ctx.create_string(id_str);
        let s_pin = ctx.pin_native_root(s);
        let obj: ObjectRef = ctx.alloc_object(cid, 16);
        let s: ObjectRef = ctx.read_native_pin(s_pin, s);
        ctx.unpin_native_roots(s_pin);
        // TimeZone.ID is the only field readers consult; set by name to
        // resolve into the correct inherited slot.
        ctx.set_field_by_name(obj, "ID", Value::Object(Some(s)));
        // ZoneInfo subclass fields — best-effort, no-op if missing.
        // HIB-CV-34: wire the standard UTC offset (ms) so the `rawOffset`
        // FIELD carries the real zone offset instead of 0. This fixes the
        // silent N-hour shift that corrupted every custom-zone JDBC timestamp
        // (America/Los_Angeles read 0 instead of -28800000).
        //
        // TWO PRODUCERS OF ONE QUANTITY, and the retired one was still
        // writing. `getRawOffset()` and `getOffset(long)` do NOT read this
        // field — they are natives (`register_tzdb_offset_natives_for`, below)
        // that answer from `crate::tzdb`, and MEASURED on the d2e127930 binary
        // they are correct on all 632 ids `getAvailableIDs()` returns. The
        // field was seeded from `tz_standard_offset_seconds`, a ~50-entry hand
        // table whose own successor note calls it superseded, so the field and
        // the accessor disagreed on every id outside those 50 — the field read
        // 0 while the accessor read the truth.
        //
        // That is not cosmetic. Three real-JDK bytecode paths read the FIELD
        // and never go near the natives, all SOURCE-VERIFIED against JDK 25's
        // `sun/util/calendar/ZoneInfo.java` (`$JAVA_HOME/lib/src.zip`):
        //
        //   * `getOffset(era, y, m, d, dayOfWeek, ms)` — the six-arg overload,
        //     which is NOT one of the registered descriptors. MEASURED: on the
        //     old seeding `TimeZone.getTimeZone("America/New_York")` answered
        //     it wrongly while the one-arg form answered correctly, which is
        //     the row `RSimpleTimeZoneRaw`'s `tz.sixarg.New_York.*` pins.
        //   * `getLastRawOffset()` = `rawOffset + rawOffsetDiff`, which
        //     `getOffsets` and `toString()` both go through.
        //   * `inDaylightTime(Date)` in `java.util.SimpleTimeZone` (a
        //     different class, same trap): `getOffset(t) != this.rawOffset`,
        //     the field read against the hijacked accessor.
        //
        // So the field is now seeded from the SAME source the accessors use,
        // and the hand table is demoted to the fallback it always should have
        // been — it still answers for the deprecated three-letter aliases
        // (`EST`, `CST`, `PST`, `MST`, `HST`) and the `GMT±HH:MM` /
        // `Etc/GMT±N` synthetic spellings, 28 of which `ZoneId.of` cannot
        // resolve at all.
        ctx.set_field_by_name(obj, "rawOffset", Value::Int(raw_offset_ms));
        ctx.set_field_by_name(obj, "rawOffsetDiff", Value::Int(0));
        // `dstSavings` STAYS 0 HERE, and that is a deliberate refusal rather
        // than an oversight. See the nomination in
        // `docs/feature-designs/native-dispatch-architecture.md`
        // (N-TZ-1): the value real `ZoneInfoFile` stores is the saving of the
        // zone's LAST rule, which lives in `ZoneRulesData.last_rules` — a
        // PRIVATE field of `crate::tzdb`, so it cannot be read from here, and
        // `tzdb` exposes no `dst_savings_ms`.
        //
        // Deriving it here instead, by sampling `tzdb::legacy_offsets_ms`
        // across a fixed year and taking the maximum saving, was tried and
        // MEASURED against the oracle on all 632 ids: it agrees on 601 of the
        // 604 that `ZoneId.of` resolves and is WRONG on `Africa/Casablanca`,
        // `Africa/El_Aaiun` and `Africa/Windhoek` (it reports 3600000 where
        // HotSpot reports 0). Those three are right today, by accident, at 0.
        // A second producer that is 99.5% accurate is what the comment above
        // is about; it is not an improvement on having one producer.
        //
        // NOTE ALSO, SOURCE-VERIFIED, because the obvious expectation is
        // wrong: seeding `dstSavings` correctly would fix `getDSTSavings()`
        // AND NOTHING ELSE. `ZoneInfo.useDaylightTime()` is
        // `return (simpleTimeZoneParams != null);` and
        // `observesDaylightTime()`/`inDaylightTime(Date)` both read
        // `transitions` — none of the three consults `dstSavings`. The DST
        // family needs `transitions`/`simpleTimeZoneParams` populated or a
        // native override; a field seeding cannot reach it.
        ctx.set_field_by_name(obj, "dstSavings", Value::Int(0));
        Ok(cratonvm_types::Value::Object(Some(obj)))
    }

    /// The id to use when `TimeZone.setDefault(...)` has never run this
    /// process, in the same precedence order real `TimeZone.setDefaultZone()`
    /// uses: the configured `user.timezone`, then the HOST's own zone, then
    /// UTC.
    ///
    /// W7-92 added the middle step. Without it this native — which is what
    /// `TimeZone.getDefault()` resolves to in `Compatible`/`--fast-boot` mode,
    /// where the real `getDefaultRef`/`setDefaultZone` bytecode, and therefore
    /// `getSystemTimeZoneID`, never runs — answered UTC on every host whose
    /// `user.timezone` was unset, which on Windows is every host: `vm_init`
    /// seeds that property from `$TZ` alone and Windows does not set `$TZ`.
    ///
    /// Two reads for `user.timezone`, deliberately. `get_system_property` is
    /// the one that can succeed: `vm_init` writes the key into the VM's
    /// property table (as `""` when there is no `$TZ`, hence the emptiness
    /// filter, which also matches `setDefaultZone`'s own
    /// `zoneID == null || zoneID.isEmpty()`). The `runtime_var` read below it
    /// is the pre-W7-92 behaviour, kept so an embedder relying on it does not
    /// regress — but note that `user.timezone` is not a declared flag name, so
    /// `runtime_var` falls through to a live environment read of that name —
    /// i.e. it
    /// looks for an ENVIRONMENT VARIABLE of that name and never saw the system
    /// property its own comment claimed to honour.
    fn timezone_fallback_zone_id(ctx: &mut dyn NativeContext) -> String {
        let configured = ctx
            .get_system_property("user.timezone")
            .filter(|s| !s.is_empty())
            .or_else(|| {
                cratonvm_types::flags::runtime_var("user.timezone")
                    .ok()
                    .filter(|s| !s.is_empty())
            });
        if let Some(id) = configured {
            return id;
        }
        match crate::tzdb::system_zone_id(ctx, None) {
            Some(id) => {
                // Exactly what the real `setDefaultZone()` does with the id it
                // just resolved (`props.setProperty("user.timezone", id)`), so
                // `System.getProperty("user.timezone")` reports it as HotSpot
                // does — and so the branch above answers every later call
                // without re-reading `tzmappings`.
                ctx.set_system_property("user.timezone", &id);
                id
            }
            None => "UTC".to_string(),
        }
    }

    fn timezone_default_ref(
        ctx: &mut dyn NativeContext,
    ) -> Result<cratonvm_types::Value, MethodCallFailed> {
        // The fallback id is resolved LAZILY, in each branch that needs it,
        // because the common case by far is the first one — the static field is
        // already populated — and that case must stay free of the file reads
        // `timezone_fallback_zone_id` can make.
        if let Some(class_id) = ctx.class_id_by_name("java/util/TimeZone") {
            if let Some(field_index) = ctx.static_field_index_by_name(class_id, "defaultTimeZone") {
                let current = ctx.get_static_field(class_id, field_index);
                if matches!(current, Value::Object(Some(_))) {
                    return Ok(current);
                }
                let fallback_id = timezone_fallback_zone_id(ctx);
                let fallback = alloc_synth_timezone(ctx, &fallback_id)?;
                if matches!(fallback, Value::Object(Some(_))) {
                    ctx.set_static_field(class_id, field_index, fallback);
                }
                return Ok(fallback);
            }
        }
        let fallback_id = timezone_fallback_zone_id(ctx);
        Ok(alloc_synth_timezone(ctx, &fallback_id)?)
    }

    /// Localized display name for a synthetic TimeZone, honouring its `ID` and
    /// the requested style. The previous natives hard-coded "UTC" for every
    /// zone and style, so `TimeZone.getTimeZone("GMT").getDisplayName(false,
    /// SHORT, US)` — and, via `SimpleDateFormat`'s `z` field which calls
    /// `getDisplayName(daylight, style, locale)`, every HTTP `Date`/`Expires`
    /// header that formats the GMT zone — rendered "UTC" instead of "GMT"
    /// (Tomcat `TestCookieProcessorGeneration.testMaxAgeZero`). `getZoneStrings`
    /// already returns the correct rows, but `SimpleDateFormat` skips them when
    /// the zone strings are not explicitly set and falls through to
    /// `getDisplayName`, so the fix must live here. `style` follows
    /// `TimeZone.SHORT` (0) / `TimeZone.LONG` (1).
    /// The real CLDR row for `(zone id, locale)`, cached.
    ///
    /// `[id, longStandard, shortStandard, longDaylight, shortDaylight,
    /// longGeneric, shortGeneric]` — the layout `TimeZoneNamesBundle` stores and
    /// `DateFormatSymbols.getZoneStrings()` reproduces.
    ///
    /// `sun.util.resources.cldr.ext.TimeZoneNames` (in `jdk.localedata`) carries
    /// every locale but English; `sun.util.resources.cldr.TimeZoneNames` (in
    /// `java.base`) carries English. Both load here — measured — which is what
    /// makes localized zone names available at all: the table below this
    /// function is English-only, so `getDisplayName(false, LONG, Locale.GERMAN)`
    /// for `GMT` answered `Greenwich Mean Time` where HotSpot answers `Mittlere
    /// Greenwich-Zeit`, and bc-java's `i18n` suite compares the whole formatted
    /// German string.
    ///
    /// Cached because `SimpleDateFormat`'s `z` field calls `getDisplayName` on
    /// every format — this is the HTTP `Date`-header path — and a
    /// `ResourceBundle.getBundle` per call would be a real cost there.
    fn tz_cldr_row(
        ctx: &mut dyn NativeContext,
        id: &str,
        locale: ObjectRef,
    ) -> Option<Vec<String>> {
        use std::sync::OnceLock;
        static CACHE: OnceLock<
            parking_lot::Mutex<rustc_hash::FxHashMap<(String, String), Option<Vec<String>>>>,
        > = OnceLock::new();
        let cache = CACHE.get_or_init(Default::default);

        // gc-common w20-c: `toLanguageTag` is Java, and `locale` is passed on
        // after it; read it back from a pin.
        let locale_pin = ctx.pin_native_root(locale);
        let tag = ctx.invoke_virtual(locale, "toLanguageTag", "()Ljava/lang/String;", &[]);
        let locale = ctx.read_native_pin(locale_pin, locale);
        ctx.unpin_native_roots(locale_pin);
        let tag = match tag {
            Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
            _ => return None,
        };
        let key = (tag, id.to_string());
        if let Some(hit) = cache.lock().get(&key) {
            return hit.clone();
        }
        let row = tz_cldr_row_uncached(ctx, id, locale);
        cache.lock().insert(key, row.clone());
        row
    }

    fn tz_cldr_row_uncached(
        ctx: &mut dyn NativeContext,
        id: &str,
        locale: ObjectRef,
    ) -> Option<Vec<String>> {
        // gc-common w20-c: ONE pin for the whole loop. It used to be taken per
        // iteration from the raw `locale`, which the first iteration's Java
        // calls may already have moved.
        let locale_pin = ctx.pin_native_root(locale);
        let row = tz_cldr_row_scan(ctx, id, locale, locale_pin);
        ctx.unpin_native_roots(locale_pin);
        row
    }

    fn tz_cldr_row_scan(
        ctx: &mut dyn NativeContext,
        id: &str,
        locale: ObjectRef,
        locale_pin: usize,
    ) -> Option<Vec<String>> {
        for base in [
            "sun.util.resources.cldr.ext.TimeZoneNames",
            "sun.util.resources.cldr.TimeZoneNames",
        ] {
            let base_s = ctx.create_string(base);
            let locale_now = ctx.read_native_pin(locale_pin, locale);
            let bundle = ctx.invoke(
                "java/util/ResourceBundle",
                "getBundle",
                "(Ljava/lang/String;Ljava/util/Locale;)Ljava/util/ResourceBundle;",
                &[Value::Object(Some(base_s)), Value::Object(Some(locale_now))],
            );
            let Ok(Some(Value::Object(Some(bundle)))) = bundle else {
                continue;
            };
            let bundle_pin = ctx.pin_native_root(bundle);
            let id_s = ctx.create_string(id);
            let bundle_now = ctx.read_native_pin(bundle_pin, bundle);
            // `getObject` RAISES for a missing key, and a zone the ext bundle
            // does not carry is an ordinary outcome (the English rows live in
            // the other base name), so ask first.
            let has = matches!(
                ctx.invoke_virtual(
                    bundle_now,
                    "containsKey",
                    "(Ljava/lang/String;)Z",
                    &[Value::Object(Some(id_s))],
                ),
                Ok(Some(Value::Int(1)))
            );
            if !has {
                ctx.unpin_native_roots(bundle_pin);
                continue;
            }
            let id_s = ctx.create_string(id);
            let bundle_now = ctx.read_native_pin(bundle_pin, bundle);
            let arr = ctx.invoke_virtual(
                bundle_now,
                "getStringArray",
                "(Ljava/lang/String;)[Ljava/lang/String;",
                &[Value::Object(Some(id_s))],
            );
            ctx.unpin_native_roots(bundle_pin);
            let Ok(Some(Value::Object(Some(arr)))) = arr else {
                continue;
            };
            let n = ctx.array_length(arr);
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                match ctx.get_array_element(arr, i) {
                    Value::Object(Some(s)) => out.push(ctx.read_string(s).unwrap_or_default()),
                    _ => out.push(String::new()),
                }
            }
            if !out.is_empty() {
                return Some(out);
            }
        }
        None
    }

    /// The CLDR display name for `(zone id, daylight, style, locale)`, or `None`
    /// when CLDR has no row for that zone.
    fn tz_display_name_from_jdk(
        ctx: &mut dyn NativeContext,
        id: &str,
        daylight: bool,
        long_style: bool,
        locale: Option<ObjectRef>,
    ) -> Option<String> {
        if id.is_empty() {
            return None;
        }
        let locale = match locale {
            Some(l) => l,
            None => match ctx.invoke(
                "java/util/Locale",
                "getDefault",
                "()Ljava/util/Locale;",
                &[],
            ) {
                Ok(Some(Value::Object(Some(l)))) => l,
                _ => return None,
            },
        };
        let row = tz_cldr_row(ctx, id, locale)?;
        // Row layout is `[id, longStd, shortStd, longDst, shortDst, ...]`.
        let idx = match (daylight, long_style) {
            (false, true) => 1,
            (false, false) => 2,
            (true, true) => 3,
            (true, false) => 4,
        };
        row.get(idx).filter(|t| !t.is_empty()).cloned()
    }

    /// `TimeZoneNameUtility.retrieveDisplayName(id, daylight, style, locale)`,
    /// run on the JDK's own bytecode.
    ///
    /// `Some(Some(name))` is a name, `Some(None)` is the utility's own `null`
    /// (the caller then takes `TimeZone.getDisplayName`'s fallbacks), and
    /// `None` means the utility could not be run at all -- a synthetic-JDK
    /// image has no such class -- so the caller keeps its pre-existing path.
    /// A `null` locale is `Locale.getDefault(Locale.Category.DISPLAY)`, which is
    /// what `getDisplayName()` passes.
    fn tz_display_name_via_utility(
        ctx: &mut dyn NativeContext,
        id: &str,
        daylight: bool,
        long_style: bool,
        locale: Option<ObjectRef>,
    ) -> Option<Option<String>> {
        let locale = match locale {
            Some(l) => l,
            None => {
                let display = ctx
                    .ensure_class_initialized("java/util/Locale$Category")
                    .ok()
                    .and_then(|cid| {
                        ctx.static_field_index_by_name(cid, "DISPLAY")
                            .map(|idx| ctx.get_static_field(cid, idx))
                    });
                let by_category = match display {
                    Some(cat @ Value::Object(Some(_))) => ctx
                        .invoke(
                            "java/util/Locale",
                            "getDefault",
                            "(Ljava/util/Locale$Category;)Ljava/util/Locale;",
                            &[cat],
                        )
                        .ok()
                        .flatten(),
                    _ => None,
                };
                let resolved = match by_category {
                    Some(v @ Value::Object(Some(_))) => Some(v),
                    _ => ctx
                        .invoke(
                            "java/util/Locale",
                            "getDefault",
                            "()Ljava/util/Locale;",
                            &[],
                        )
                        .ok()
                        .flatten(),
                };
                match resolved {
                    Some(Value::Object(Some(l))) => l,
                    _ => return None,
                }
            }
        };
        let locale_pin = ctx.pin_native_root(locale);
        let id_s = ctx.create_string(id);
        let locale_now = ctx.read_native_pin(locale_pin, locale);
        ctx.unpin_native_roots(locale_pin);
        let answer = ctx.invoke(
            "sun/util/locale/provider/TimeZoneNameUtility",
            "retrieveDisplayName",
            "(Ljava/lang/String;ZILjava/util/Locale;)Ljava/lang/String;",
            &[
                Value::Object(Some(id_s)),
                Value::Int(i32::from(daylight)),
                Value::Int(i32::from(long_style)),
                Value::Object(Some(locale_now)),
            ],
        );
        match answer {
            Ok(Some(Value::Object(Some(s)))) => Some(ctx.read_string(s)),
            Ok(Some(Value::Object(None))) => Some(None),
            _ => None,
        }
    }

    /// The tail of `TimeZone.getDisplayName(boolean, int, Locale)` once the
    /// name utility has answered `null`: a custom `GMT±...` id verbatim,
    /// otherwise `ZoneInfoFile.toCustomID(getRawOffset() [+ getDSTSavings()])`.
    fn tz_display_name_jdk_fallback(
        ctx: &mut dyn NativeContext,
        this: ObjectRef,
        id: &str,
        daylight: bool,
    ) -> String {
        if id.len() > 3 && id.starts_with("GMT") && matches!(id.as_bytes()[3], b'+' | b'-') {
            return id.to_string();
        }
        let this_pin = ctx.pin_native_root(this);
        let mut offset = ctx
            .invoke_virtual(this, "getRawOffset", "()I", &[])
            .ok()
            .flatten()
            .and_then(|v| v.as_int())
            .unwrap_or(0);
        if daylight {
            let this_now = ctx.read_native_pin(this_pin, this);
            offset += ctx
                .invoke_virtual(this_now, "getDSTSavings", "()I", &[])
                .ok()
                .flatten()
                .and_then(|v| v.as_int())
                .unwrap_or(0);
        }
        ctx.unpin_native_roots(this_pin);
        tz_custom_id(offset)
    }

    /// `sun.util.calendar.ZoneInfoFile.toCustomID(int)`, transcribed.
    fn tz_custom_id(gmt_offset_ms: i32) -> String {
        let mut offset = gmt_offset_ms / 1_000;
        let sign = if offset >= 0 {
            '+'
        } else {
            offset = -offset;
            '-'
        };
        let (hh, mm, ss) = (offset / 3_600, (offset % 3_600) / 60, offset % 60);
        let mut id = format!("GMT{sign}{hh:02}:{mm:02}");
        if ss != 0 {
            id.push_str(&format!(":{ss:02}"));
        }
        id
    }

    fn tz_display_name_full(
        ctx: &mut dyn NativeContext,
        this: ObjectRef,
        daylight: bool,
        long_style: bool,
        locale: Option<ObjectRef>,
    ) -> Result<String, MethodCallFailed> {
        // `this` crosses the name utility's bytecode below; read it back
        // through a pin before the fallback uses it.
        let this_pin = ctx.pin_native_root(this);
        let id = match ctx.get_field_by_name(this, "ID") {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        // `TimeZone.getDisplayName(boolean, int, Locale)`'s own algorithm, when
        // the image has the JDK's name utility to run it on.
        //
        // 2026-09-12 (Tomcat `TestConcurrentDateFormat` residual): the CLDR row
        // read below leaves every name CLDR INHERITS empty, and this native then
        // answered its hard-coded "UTC" -- measured against HotSpot 25.0.3 on 14
        // zones x 5 locales x 4 styles, 143 of 280 answers differed:
        // `America/Buenos_Aires` SHORT was "UTC" where HotSpot says
        // "GMT-03:00" (so `new Date(0).toString()` printed `UTC` on a `-03:00`
        // host), `Etc/GMT+5` and `Africa/Casablanca` were "UTC" in every style,
        // and every `Locale.ROOT` name was "Coordinated Universal Time".
        // `TimeZoneNameUtility.retrieveDisplayName` derives those names through
        // `CLDRTimeZoneNameProviderImpl`, which became reachable the same day
        // (`locale_bootstrap::jre_adapter_get_locale_service_provider`'s
        // `TimeZoneNameProvider` arm).
        let answer = if id.is_empty() {
            None
        } else {
            tz_display_name_via_utility(ctx, &id, daylight, long_style, locale)
        };
        let this = ctx.read_native_pin(this_pin, this);
        ctx.unpin_native_roots(this_pin);
        match answer {
            Some(Some(name)) => return Ok(name),
            Some(None) => return Ok(tz_display_name_jdk_fallback(ctx, this, &id, daylight)),
            None => {}
        }
        // A custom `GMT+hh:mm` id has no CLDR name and displays verbatim, which
        // is what HotSpot does for a `ZoneInfo` with no localized name.
        if !(id.starts_with("GMT+") || id.starts_with("GMT-")) {
            if let Some(name) = tz_display_name_from_jdk(ctx, &id, daylight, long_style, locale) {
                return Ok(name);
            }
        }
        tz_display_name(ctx, this, long_style)
    }

    fn tz_display_name(
        ctx: &mut dyn NativeContext,
        this: ObjectRef,
        long_style: bool,
    ) -> Result<String, MethodCallFailed> {
        let id = match ctx.get_field_by_name(this, "ID") {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        Ok(match id.as_str() {
            "GMT" => {
                if long_style {
                    "Greenwich Mean Time"
                } else {
                    "GMT"
                }
            }
            "UTC" | "Etc/UTC" | "Etc/UCT" | "UCT" | "Zulu" | "Universal" | "Etc/Universal" => {
                if long_style {
                    "Coordinated Universal Time"
                } else {
                    "UTC"
                }
            }
            _ => {
                // Custom GMT-offset ids ("GMT+02:00") display verbatim, as
                // HotSpot does for a ZoneInfo with no localized name. Unknown
                // named zones keep the conservative UTC fallback this native
                // used before (CratonVM models most zones by their standard
                // offset, without per-zone CLDR display names).
                if id.starts_with("GMT+") || id.starts_with("GMT-") {
                    return Ok(id);
                }
                return if long_style {
                    Ok("Coordinated Universal Time".to_string())
                } else {
                    Ok("UTC".to_string())
                };
            }
        }
        .to_string())
    }

    registry.register(
        "java/util/TimeZone",
        "getTimeZone",
        "(Ljava/lang/String;)Ljava/util/TimeZone;",
        |ctx, args| {
            // A null id is an NPE, and it is NOT the same thing as an unknown
            // one: `TimeZone.getTimeZone("Not/AZone")` legitimately answers GMT
            // (measured, and reproduced here), so substituting UTC for null made
            // the fabricated answer indistinguishable from the documented one.
            // MEASURED no-throw (apps/probes/LocaleDateTzShadowSweep 94).
            if matches!(args.first(), Some(Value::Object(None))) {
                return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                    message: None,
                }
                .into());
            }
            let id = match args.first() {
                Some(Value::Object(Some(o))) => {
                    ctx.read_string(*o).unwrap_or_else(|| "UTC".to_string())
                }
                _ => "UTC".to_string(),
            };
            // JDK 25's `TimeZone.getTimeZone(String, boolean)` opens with this
            // warning for every `ZoneId.SHORT_IDS` key, on every call, before
            // resolving anything. This native replaces that body, so the
            // warning was never printed -- measured 2026-09-12 as the one line
            // separating this VM from HotSpot 25.0.3 on a 280-name
            // `TimeZone.getDisplayName` sweep that asks for "PST".
            const SHORT_IDS: &[&str] = &[
                "ACT", "AET", "AGT", "ART", "AST", "BET", "BST", "CAT", "CNT", "CST", "CTT", "EAT",
                "ECT", "IET", "IST", "JST", "MIT", "NET", "NST", "PLT", "PNT", "PRT", "PST", "SST",
                "VST", "EST", "MST", "HST",
            ];
            if SHORT_IDS.contains(&id.as_str()) {
                let err = ctx
                    .ensure_class_initialized("java/lang/System")
                    .ok()
                    .and_then(|cid| {
                        ctx.static_field_index_by_name(cid, "err")
                            .map(|idx| ctx.get_static_field(cid, idx))
                    });
                if let Some(Value::Object(Some(err))) = err {
                    let err_pin = ctx.pin_native_root(err);
                    let line = ctx.create_string(&format!(
                        "WARNING: Use of the three-letter time zone ID \"{id}\" is deprecated \
                         and it will be removed in a future release"
                    ));
                    let err = ctx.read_native_pin(err_pin, err);
                    ctx.unpin_native_roots(err_pin);
                    let _ = ctx.invoke_virtual(
                        err,
                        "println",
                        "(Ljava/lang/String;)V",
                        &[Value::Object(Some(line))],
                    );
                }
            }
            // Canonicalise custom GMT-offset ids ("GMT+2" -> "GMT+02:00") so
            // getID() matches the real JDK; IANA/named ids are kept verbatim.
            let custom_gmt = normalize_gmt_custom_id(&id);
            // Real JDK's TimeZone.getTimeZone falls back to the "GMT" zone
            // (getID() == "GMT") for any string it can't resolve against
            // tzdata — callers like StringUtils.parseTimeZoneString rely on
            // exactly that fallback to detect a bogus zone id (e.g. "foo")
            // and throw IllegalArgumentException. Without it, CratonVM used
            // to echo the bogus string back as the ID, so that detection
            // never fired (Jackson2ObjectMapperBuilderTests
            // .wrongTimeZoneStringSetter expected the throw and never got
            // it).
            //
            // TZDB-OFFSET (2026-07-22): "resolvable" used to mean "in the
            // ~40-zone curated `tz_standard_offset_seconds` table or
            // contains a '/'" — which silently misrecognised any valid
            // slash-free zone id outside that table (e.g. "CST6CDT",
            // "EST5EDT", "MST7MDT", "PST8PDT" — the four POSIX-rule zone
            // names `TimeZone.getAvailableIDs()` itself returns) as bogus,
            // collapsing them to "GMT"/UTC+0 — see
            // bug-h2-timezone-zonerules-offset-miscalculation-FIXED.md.
            // Now backed by the real tzdb.dat catalog (604 zones + legacy
            // aliases), so this matches exactly what real HotSpot resolves.
            let recognized = id == "GMT"
                || custom_gmt.is_some()
                || crate::tzdb::get_zone_rules(ctx, &id).is_some();
            let id = if recognized {
                custom_gmt.unwrap_or(id)
            } else {
                "GMT".to_string()
            };
            Ok(Some(alloc_synth_timezone(ctx, &id)?))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/util/TimeZone",
        "getDefault",
        "()Ljava/util/TimeZone;",
        |ctx, _args| {
            // nio20 wave 5 (L4): HotSpot's body is `(TimeZone)
            // getDefaultRef().clone()`, so `getDefault() != getDefault()` and a
            // caller's `setRawOffset` / `setID` on the answer never reaches the
            // default (`getDefaultRef`, below, is the shared one). This native
            // handed out the shared default itself. The clone is the JDK's own
            // (`ZoneInfo.clone` clears `lastRule`); where it does not answer an
            // object (the synthetic library), the shared default is the answer
            // as before, re-read through its pin (the call can collect).
            let shared = timezone_default_ref(ctx)?;
            let Value::Object(Some(tz)) = shared else {
                return Ok(Some(shared));
            };
            let tz_pin = ctx.pin_native_root(tz);
            let copy = ctx.invoke_virtual(tz, "clone", "()Ljava/lang/Object;", &[]);
            let tz = ctx.read_native_pin(tz_pin, tz);
            ctx.unpin_native_roots(tz_pin);
            match copy {
                Ok(Some(copy @ Value::Object(Some(_)))) => Ok(Some(copy)),
                _ => Ok(Some(Value::Object(Some(tz)))),
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/util/TimeZone",
        "getDefaultRef",
        "()Ljava/util/TimeZone;",
        |ctx, _args| Ok(Some(timezone_default_ref(ctx)?)), cratonvm_native_api::NativeKind::Bridge,
    );
    // `setDefaultZone()`, `getDisplayName()` and `getDisplayName(Locale)`
    // deleted 2026-09-20: all three are registered on the ABSTRACT
    // `java/util/TimeZone` class itself, and every concrete receiver this VM
    // ever hands back (`sun.util.calendar.ZoneInfo`, `SimpleTimeZone`) either
    // inherits real JDK bytecode for them (`getDisplayName()`/`(Locale)` are
    // both `final` and neither subclass declares its own; the registry
    // census's `real_declaring_method.has_code` is `true` for both) or never
    // reaches this slot at all (`setDefaultZone` just re-calls
    // `timezone_default_ref`, the same body `getDefault`/`getDefaultRef`
    // already run -- this VM's own `getDefault()` never goes through real
    // `TimeZone` bytecode to get there). Measured 0 invocations both direct
    // (`TzDirectCheck.java`) and via a method reference (`DeadDoorProbe`),
    // both modes. `l3-followups-the-carrier-identity-and-the-dead-registrations-20260830.md`
    // §2. The 3-arg `getDisplayName(boolean, int, Locale)` a few lines below
    // is NOT one of these three -- it is the variant `SimpleDateFormat`'s `z`
    // field calls and stays.
    // getDisplayName(boolean daylight, int style, Locale): style follows
    // TimeZone.SHORT (0) / TimeZone.LONG (1). This is the variant
    // SimpleDateFormat's `z` field calls.
    registry.register(
        "java/util/TimeZone",
        "getDisplayName",
        "(ZILjava/util/Locale;)Ljava/lang/String;",
        |ctx, args| {
            let daylight = matches!(args.get(1), Some(Value::Int(v)) if *v != 0);
            let long_style = matches!(args.get(2), Some(Value::Int(v)) if *v != 0);
            let locale = match args.get(3) {
                Some(Value::Object(Some(l))) => Some(*l),
                _ => None,
            };
            let name = match args.first() {
                Some(Value::Object(Some(o))) => {
                    tz_display_name_full(ctx, *o, daylight, long_style, locale)
                }
                _ => Ok("UTC".to_string()),
            };
            Ok(Some(Value::Object(Some(ctx.create_string(&name?)))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // W4: was an unconditional `null`, justified as "synthesizing a real
    // ZoneInfo layout is work nothing needs". That work already exists —
    // `alloc_synth_timezone` (below) builds a `sun/util/calendar/ZoneInfo`
    // with its `ID`/`rawOffset`/`dstSavings` slots populated from the same
    // tzdb this file parses, and the sibling public `ZoneInfoFile.getZoneInfo`
    // native is registered against exactly that. So answer the private entry
    // point the same way, and keep `null` for its real meaning: an id tzdb
    // does not know. (`getZoneInfo` clones the result and re-stamps the id, so
    // a fresh object per call is what the JDK expects here.)
    registry.register(
        "sun/util/calendar/ZoneInfoFile",
        "getZoneInfo0",
        "(Ljava/lang/String;)Lsun/util/calendar/ZoneInfo;",
        |ctx, args| {
            let Some(Value::Object(Some(id_obj))) = args.first().copied() else {
                return Ok(Some(Value::Object(None)));
            };
            let Some(id) = ctx.read_string(id_obj) else {
                return Ok(Some(Value::Object(None)));
            };
            // JDK contract: null for an unknown zone id — the caller
            // (`getZoneInfo`) turns that into "no such TimeZone".
            if crate::tzdb::canonical_zone_id(ctx, &id).is_none()
                && tz_standard_offset_seconds(&id).is_none()
            {
                return Ok(Some(Value::Object(None)));
            }
            Ok(Some(alloc_synth_timezone(ctx, &id)?))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // TZDB-OFFSET (2026-07-22): java.util.TimeZone offset queries backed by
    // the real IANA tzdb data (`native_builtins::tzdb`, parsed from
    // `${java.home}/lib/tzdb.dat` — the exact same file and binary format
    // `java.time.zone.ZoneRules`/`sun.util.calendar.ZoneInfoFile` use,
    // cross-checked bit-for-bit against real HotSpot JDK 25 across all 604
    // zones). Supersedes the previous `tz_dst_rule`/`dst_start_year`/
    // `historical_lmt_offset` hand-rolled approximations (a ~20-zone
    // allowlist modeling only each zone's *current* recurring DST rule) —
    // this covers every zone's full historical transition table instead.
    //
    // `GregorianCalendar.computeTime()`/`computeFields()` dispatch to one of
    // two shapes depending on the receiver's concrete class: a `ZoneInfo`
    // gets `getOffsets`/`getOffsetsByWall` called directly (downcast in
    // `GregorianCalendar`'s own bytecode), anything else (e.g.
    // `SimpleTimeZone`) goes through the generic `TimeZone.getOffset(long)`.
    // Only `ZoneInfo` and the abstract `TimeZone` itself are registered below:
    // C12-1 removed `java/util/SimpleTimeZone`, whose id is an opaque LABEL and
    // whose offset is the caller's `rawOffset`, and `alloc_synth_timezone` no
    // longer constructs one for any id. See
    // `docs/feature-designs/native-dispatch-architecture.md`.
    fn tzdb_offsets(
        ctx: &mut dyn NativeContext,
        this: cratonvm_types::ObjectRef,
        date_millis: i64,
    ) -> (i32, i32) {
        crate::tzdb::with_zone_rules_and_offsets(ctx, this, |rules_opt, raw_field, diff| {
            let (total, dst) = match rules_opt {
                Some(rules) => crate::tzdb::legacy_offsets_ms_of(rules, date_millis),
                None => (raw_field, 0),
            };
            (total.saturating_add(diff), dst)
        })
    }

    // `sun.util.calendar.ZoneInfoFile`'s real conversion from tzdb rules to
    // the legacy `ZoneInfo` representation only ever models transitions from
    // 1900 onward (`UTC1900` in that class) — any query before that floor
    // falls through to `getLastRawOffset()` (the zone's CURRENT/modern
    // standard offset, dstSavings=0), not the deep historical offset
    // `java.time.zone.ZoneRules` (unfloored) would report. Mirrored here so
    // the legacy `TimeZone`/`GregorianCalendar` path matches real HotSpot's
    // legacy behavior bug-for-bug, same as it does post-1900 — see
    // `bug-h2-timezone-zonerules-offset-miscalculation-FIXED.md`.
    const ZONEINFO_LEGACY_FLOOR_EPOCH_SEC: i64 = -2_208_988_800; // 1900-01-01T00:00:00Z

    // KIND: `Intrinsic`, explicitly (`register_with_kind`), on every
    // registration below -- not `Bridge`, not `SyntheticStub`. Investigated
    // 2026-09-21 while re-freezing the kind-map baseline.
    //
    // Started as `Bridge`: `--dump-native-registry` showed these rows as
    // `bridge` but UNSTATED, and `javap -p` on the real JDK 25 image
    // confirms none of `sun.util.calendar.ZoneInfo`'s methods are
    // `ACC_NATIVE`, which reads as a mis-tag against contract Sec1.5's
    // narrow "binds to a native target" definition of Bridge. Stating it as
    // `Bridge` explicitly promptly failed `scripts/jdk-only-bridge-ratchet.py`'s
    // `bridge.stated_shadows_bytecode` ratchet (383 against a frozen,
    // slack-0 baseline of 382) -- the gate's own message is unambiguous:
    // *"Fix the registration -- state its kind with register_with_kind() and
    // make it SyntheticStub or Intrinsic, or delete it so the real bytecode
    // runs -- do NOT just raise the baseline."* Bridge was never one of the
    // two options it offers, and for good reason: `bridges = [r for r in
    // rows if r.get("kind") == "bridge"]` (the ratchet's own filter) is the
    // whole "remove synthetic bridges shadowing real bytecode" campaign's
    // scope, and this native genuinely is what that campaign is about --
    // just one it cannot safely act on, which `Intrinsic`'s "never gated"
    // semantics leave alone while still exempting it from the ratchet.
    //
    // NOT `SyntheticStub` either, and this one is not a close call:
    // `SyntheticStub` is the ONLY kind `--real-jdk` rejects
    // (`NativeMethodRegistry::effective_category` /
    // `CompatibilityMode::JdkOnly`), so tagging these `SyntheticStub` would
    // DROP them under `--real-jdk` and let real `ZoneInfo` bytecode run
    // instead. It cannot: `alloc_synth_timezone` (this file) allocates every
    // `ZoneInfo` with only `ID`/`rawOffset`/`rawOffsetDiff` populated --
    // `transitions`, `offsets` and `simpleTimeZoneParams` are left
    // null/zero, deliberately (`dstSavings` too: N-TZ-1,
    // `docs/feature-designs/native-dispatch-architecture.md`
    // -- the real value lives in a private `tzdb` field this crate cannot
    // read). Real `ZoneInfo` bytecode for every method below either
    // binary-searches `transitions` (`getTransitionIndex`) or reads
    // `offsets`/`simpleTimeZoneParams` directly, so dropping these natives
    // would NPE or silently answer wrong on a null array for every zone --
    // it would not "let the real implementation run", it would break every
    // `--real-jdk` program that reads a time zone.
    //
    // `Intrinsic` is the honest fit: this crate's `tzdb` module computes the
    // SAME answer real `ZoneInfo` bytecode would if its fields were the
    // real JDK's TZif-derived data instead of the empty shell this VM
    // allocates -- a different mechanism reaching the oracle's answer, which
    // is exactly `NativeKind::Intrinsic`'s own definition above ("returns
    // the same answer the real bytecode would, just [by another route]").
    // The `tzdb::tests` module's own `*_matches_hotspot*` tests are the
    // measurement backing that claim, not an assertion taken on faith.
    // `register_with_kind` records this as a decision, not an inherited
    // default, so a future kind-map re-freeze does not lose it again the
    // way the unstated form did.
    fn register_tzdb_offset_natives_for(
        registry: &mut NativeMethodRegistry,
        class_name: &'static str,
    ) {
        registry.with_leaf(true, |registry| {
            registry.register(
                class_name,
                "getOffset",
                "(J)I",
                |ctx, args| {
                    let this = match args.first() {
                        Some(Value::Object(Some(o))) => *o,
                        _ => return Ok(Some(Value::Int(0))),
                    };
                    let date = match args.get(1) {
                        Some(Value::Long(v)) => *v,
                        _ => 0,
                    };
                    let (total_ms, _dst_ms) = tzdb_offsets(ctx, this, date);
                    Ok(Some(Value::Int(total_ms)))
                },
                cratonvm_native_api::NativeKind::Intrinsic,
            );
            registry.register(
                class_name,
                "getOffsets",
                "(J[I)I",
                crate::tzdb::native_zone_info_get_offsets,
                cratonvm_native_api::NativeKind::Intrinsic,
            );
            registry.register(
                class_name,
                "getOffsetsByWall",
                "(J[I)I",
                crate::tzdb::native_zone_info_get_offsets_by_wall,
                cratonvm_native_api::NativeKind::Intrinsic,
            );
            registry.register(
                class_name,
                "getRawOffset",
                "()I",
                |ctx, args| {
                    let this = match args.first() {
                        Some(Value::Object(Some(o))) => *o,
                        _ => return Ok(Some(Value::Int(0))),
                    };
                    let ms = crate::tzdb::with_zone_rules_and_offsets(
                        ctx,
                        this,
                        |rules_opt, raw_field, diff| {
                            let raw = match rules_opt {
                                Some(r) => crate::tzdb::raw_offset(r).saturating_mul(1000),
                                None => raw_field,
                            };
                            raw.saturating_add(diff)
                        },
                    );
                    Ok(Some(Value::Int(ms)))
                },
                cratonvm_native_api::NativeKind::Intrinsic,
            );
        });
    }
    register_tzdb_offset_natives_for(registry, "sun/util/calendar/ZoneInfo");
    // C12-1 (2026-08-12): `java/util/SimpleTimeZone` is DELIBERATELY NOT HERE.
    //
    // These four bodies answer from the RECEIVER'S `ID` FIELD, resolved against
    // tzdb. That is right for a `ZoneInfo`, whose id IS the zone. It is wrong
    // for a `java.util.SimpleTimeZone`, whose id is by contract an opaque LABEL
    // and whose offset is the `rawOffset` its constructor stored:
    // `new SimpleTimeZone(18000000, "America/Buenos_Aires").getRawOffset()` is
    // `18000000` on HotSpot 25 and was `-10800000` here, because the label won.
    // `SimpleTimeZone.getRawOffset()` is one instruction — `getfield rawOffset`
    // — and this registration shadowed it. See
    // `docs/feature-designs/native-dispatch-architecture.md`
    // for the full measurement set, including the silent half: unregistered
    // `inDaylightTime(Date)` is `getOffset(t) != this.rawOffset` in real
    // bytecode, so a hijacked `getOffset` against the real field made a zone
    // with NO DST rule report that it IS in daylight time.
    //
    // WHY THE BASE REGISTRATION BELOW DOES NOT SIMPLY INHERIT THE DEFECT.
    // `NativeMethodRegistry::find` is an exact `(class, method, descriptor)`
    // lookup with no hierarchy walk of its own; the walk lives in the caller.
    // For an invokevirtual, `try_stackless_invoke`'s `class_name` is the
    // RECEIVER's runtime class (`invoke.rs`, `invoke_class`), `walk_native_hierarchy`
    // is `false`, and the superclass climb is skipped outright when the
    // receiver's own class declares the method
    // (`invoke.rs`: `if has_own_bytecode { return None; }`). Real
    // `java.util.SimpleTimeZone` DECLARES all three live members of this family
    // — `getRawOffset()I`, `getOffset(J)I` and `getOffsets(J[I)I` (`javap -p`,
    // JDK 25) — so with the exact-class registration gone each one runs its own
    // bytecode and never reaches `java/util/TimeZone`'s copy. The late
    // native-override check at step 6 keys on the DECLARING class of the
    // resolved method, which is `SimpleTimeZone` for the same reason.
    //
    // The base registration therefore still does its job (a `TimeZone`-typed
    // receiver with no bytecode of its own — the synthetic-JDK stub, and the
    // `ZoneInfo`-less fallback object this function's tail allocates) without
    // capturing a real `SimpleTimeZone`. `getOffsetsByWall(J[I)I` is the one
    // member `SimpleTimeZone` does not declare, and it cannot be reached on one:
    // `SimpleTimeZone` is not a `ZoneInfo`, and the only caller
    // (`GregorianCalendar`) downcasts before invoking it.
    //
    // The abstract base. Only the concrete subclass above carries the offset
    // family, so anything holding a `TimeZone`-typed reference to a receiver
    // with no bytecode of its own — which is how the API is normally used —
    // would otherwise have no `getRawOffset`.
    register_tzdb_offset_natives_for(registry, "java/util/TimeZone");

    // `TimeZone.getID()` reads the `ID` field, exactly as the real base-class
    // method does, so it is faithful for every subclass that inherits it.
    registry.register(
        "java/util/TimeZone",
        "getID",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            if let Value::Object(Some(s)) = ctx.get_field_by_name(this, "ID") {
                return Ok(Some(Value::Object(Some(s))));
            }
            // Synthetic TimeZones built by `getDefault` below keep the id in
            // slot 0; a real one that never had `ID` populated reports UTC
            // rather than null, which callers concatenate into messages.
            if ctx.object_num_fields(this) > 0 {
                if let Value::Object(Some(s)) = ctx.get_field(this, 0) {
                    return Ok(Some(Value::Object(Some(s))));
                }
            }
            Ok(Some(Value::Object(Some(ctx.create_string("UTC")))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // `TimeZone.getDefault()` is intentionally NOT re-registered here.
    //
    // REGRESSION 2026-08-03: this spot used to carry a second
    // `registry.register("java/util/TimeZone", "getDefault", ...)` (added
    // 2026-07-31, `98878a6dd`) that allocated a fresh synthetic TimeZone from
    // the `user.timezone` system property on every call. `register()` is
    // documented last-registration-wins on the exact (class, method,
    // descriptor) triple (see `NativeMethodRegistry::register`), and the real
    // implementation — `timezone_default_ref` above, registered earlier in
    // this same function at the `"getDefault"` site next to `getDefaultRef`
    // — reads the actual `TimeZone.defaultTimeZone` static field, which is
    // what `TimeZone.setDefault(...)`'s real bytecode writes. The later
    // registration silently shadowed it, so every `TimeZone.getDefault()`
    // call after a `setDefault(...)` (and everything built on it —
    // `ZoneId.systemDefault()` above, every JDBC/native path that consults
    // the JVM default zone) went back to reporting the VM's *startup* zone
    // instead of whatever the running program had set. Hibernate's
    // `Timezones.withDefaultTimeZone()`-based temporal tests
    // (confirmed on `OffsetDateTimeTest`: `failed=0` -> `failed=60`,
    // identically under the JIT and `--nojit`) went back to reporting
    // timezone-offset-sized value corruption — this is native-registration
    // shadowing, not a JIT or GC defect. `timezone_default_ref` now also
    // honours `user.timezone` as its
    // own fallback (used only before the first `setDefault` call), so the
    // duplicate's one legitimate feature is preserved without reintroducing
    // the shadow. Lesson: "this native looks wrong / returns null" is never
    // grounds for a fresh `register()` call on a triple without first
    // grepping whether an earlier one already owns it — the earlier one may
    // be the correct implementation, and the later call always wins silently.
    registry.register(
        "sun/util/calendar/ZoneInfoFile",
        "getZoneInfo",
        "(Ljava/lang/String;)Lsun/util/calendar/ZoneInfo;",
        |ctx, args| {
            let id = match args.first() {
                Some(Value::Object(Some(o))) => {
                    ctx.read_string(*o).unwrap_or_else(|| "UTC".to_string())
                }
                _ => "UTC".to_string(),
            };
            Ok(Some(alloc_synth_timezone(ctx, &id)?))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // CleanerFactory.<clinit> NPE fix — real-JDK
    // `Cleaner.create(...)` bytecode allocates an `InnocuousThread`
    // and calls `t.setPriority(...)` which dereferences a null
    // `holder:FieldHolder` field, killing `CleanerFactory.<clinit>`
    // (and thereby blocking WildFly's boot via the silent-swallowed
    // exception in `Bootstrap.bootstrap`). Wire our synthetic
    // `Cleaner.create / register / clean` natives so the bytecode
    // never reaches the InnocuousThread path. Paired with the
    // `check_override` allow-list entry for `java/lang/ref/Cleaner`
    // in `vm/src/vm/vm_exec.rs`.
    crate::phases_late::register_p69_cleaner(registry);
    // No `FileCleanable.register` here (interpreter round i3 wave 83; page
    // `i83-orch-compatible-thread-dump-lacks-common-cleaner`). A no-op stood
    // in for it, once because the cleaner's `InnocuousThread` tripped
    // ThreadGroup layout differences; `populate_real_thread_holder` and the
    // system/main group parentage fixed that, and `--real-jdk` has run the
    // real body since it was retired (`RETIRED_SHADOW_L4_BACKED_OUT_TRIPLES`).
    // Under `--compatible` the no-op kept the `Common-Cleaner` thread from
    // starting where HotSpot starts it -- the app class loader's first
    // `FileInputStream` (the main class's bytes) -- so a thread dump lacked
    // it on Linux, and on Windows listed it late with an id the program
    // never saw (it started inside `jcmd`'s `ProcessBuilder`).

    // UUID-OVERRIDE: java.util.UUID natives. Real JDK 25 `UUID.toString()`
    // bytecode uses `jdk/internal/util/ByteArrayLittleEndian.{setLong,setInt}`
    // + `JavaLangAccess.uncheckedNewStringNoRepl` which we don't implement,
    // producing an empty string. That empty string fails `UUID.fromString`
    // in `ProcessEnvironment.obtainProcessUUID` ("Invalid UUID string")
    // during Keycloak 16 boot. Wire our native overrides (canonical
    // 8-4-4-4-12 hex via `format!`) into the essential-natives registry so
    // they're present in real-JDK mode (previously they were only
    // registered inside `register_synthetic_overrides`).
    register_uuid_natives(registry);

    // --- WP4.8 / NEW-15 Loom natives (real-JDK mode) ---
    // `jdk/internal/vm/ContinuationSupport.isSupported0()Z` must return false
    // so `Continuation.<clinit>` does not throw UnsatisfiedLinkError during
    // JBoss server bootstrap (`EnhancedQueueExecutor`/`JBossExecutors` <clinit>).
    // Previously this was only registered via `register_synthetic_overrides`
    // (line 7662 in the synthetic-only branch); move it here so real-JDK mode
    // (Keycloak 16 boot) gets the native too.
    crate::phases_late::register_new15_loom(registry);

    // Arrays.stream(Object[]) — real-JDK-mode override.
    //
    // Real JDK's `Arrays.stream(T[])` → `Arrays.stream(T[],int,int)` →
    // `StreamSupport.stream(ArraySpliterator, false)` produces a
    // `ReferencePipeline$Head` that under CratonVM walks the spliterator
    // pipeline incorrectly: `count()` and `forEach` happen to work, but
    // `collect(Collector)` and `toList()` surface 0 elements. Kafka 4.2.0's
    // `ServerTopicConfigSynonyms.<clinit>` (Kafka.scala:75 →
    // KafkaConfig$.<clinit> → ServerConfigs.<clinit> →
    // ServerTopicConfigSynonyms.lambda$static$0) builds its synonym map via
    // `Arrays.stream(arr).map(...).collect(toList())` and then does
    // `list.get(0).name()`, NPE-ing on the empty list.
    //
    // Workaround: delegate `Arrays.stream(T[])` to
    // `Arrays.asList(arr).stream()`, which under CratonVM produces a working
    // `Collection`-backed stream (verified: `Arrays.asList(arr).stream()
    // .collect(toList())` returns the right elements). Same semantics —
    // a sequential reference stream over the array's elements — just routed
    // through the working backing.
    registry.register(
        "java/util/Arrays",
        "stream",
        "([Ljava/lang/Object;)Ljava/util/stream/Stream;",
        |ctx, args| {
            let arr_val = args.first().copied().unwrap_or(Value::Object(None));
            // Arrays.asList(arr) -> List (the call's own result, so current).
            let list_ref: ObjectRef = match ctx.invoke(
                "java/util/Arrays",
                "asList",
                "([Ljava/lang/Object;)Ljava/util/List;",
                &[arr_val],
            )? {
                Some(Value::Object(Some(o))) => o,
                _ => return Ok(Some(Value::Object(None))),
            };
            // list.stream() -> Stream  (invokeinterface on Collection.stream)
            ctx.invoke_virtual(list_ref, "stream", "()Ljava/util/stream/Stream;", &[])
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/util/Arrays",
        "copyOf",
        "([Ljava/lang/Object;I)[Ljava/lang/Object;",
        |ctx, args| {
            let src = arrays_src_or_npe(args, "copyOf")?;
            let new_len = match args.get(1) {
                Some(Value::Int(n)) => arrays_len_or_negative(*n)?,
                _ => 0,
            };
            let src_len = ctx.array_length(src);
            let comp_cid = ctx.class_id_of_object(src);
            // gc-common w8-b: reclaim and retry before the OOME, with `src`
            // (the only reference held) refreshed across the collection.
            let mut live = [src];
            let dst = new_ref_array_reclaiming(ctx, comp_cid, new_len, &mut live, "Arrays.copyOf")?;
            let [src] = live;
            crate::arrays_copy_elements(ctx, src, 0, dst, src_len.min(new_len), true);
            Ok(Some(Value::Object(Some(dst))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/util/Arrays",
        "copyOf",
        "([Ljava/lang/Object;ILjava/lang/Class;)[Ljava/lang/Object;",
        |ctx, args| {
            let src = arrays_src_or_npe(args, "copyOf")?;
            let new_len = match args.get(1) {
                Some(Value::Int(n)) => arrays_len_or_negative(*n)?,
                _ => 0,
            };
            let src_len = ctx.array_length(src);
            // A NULL target class is a NullPointerException, not a silent
            // fallback to `Object[]`. MEASURED: HotSpot NPE, this VM no-throw.
            let Some(Value::Object(Some(mirror))) = args.get(2) else {
                return Err(RuntimeError::NullPointerException {
                    message: Some("Arrays.copyOf: newType is null".to_string()),
                }
                .into());
            };
            let comp_cid = crate::lang_class::mirror_class_id(ctx, *mirror)
                .and_then(|arr_cid| ctx.array_component_class_id(arr_cid));
            // gc-common w8-b: reclaim and retry before the OOME. `src` is the
            // only reference used after this point (`mirror` is not), and it
            // is refreshed across the collection.
            let mut live = [src];
            let dst = match comp_cid {
                Some(cid) => {
                    new_ref_array_reclaiming(ctx, cid, new_len, &mut live, "Arrays.copyOf")?
                }
                None => new_array_reclaiming(
                    ctx,
                    cratonvm_types::ArrayElementType::Reference,
                    new_len,
                    &mut live,
                    "Arrays.copyOf",
                )?,
            };
            let [src] = live;
            // PER-ELEMENT STORE CHECK. The 3-arg form is specified to throw
            // `ArrayStoreException` when an element of `original` is not
            // assignable to `newType`'s component type -- it is the one copy
            // method that can NARROW the type. MEASURED in both modes:
            //
            //   Arrays.copyOf(new Object[]{"s", Integer.valueOf(1)}, 2, String[].class)
            //     HotSpot  ArrayStoreException     CratonVM  no-throw
            //
            // Without it the native manufactures a `String[]` containing an
            // `Integer` -- an array whose contents contradict its own type, which
            // every later reader is entitled to assume cannot exist. Skipped
            // when the component type is `java/lang/Object`, which accepts all.
            //
            // The check itself is the VM's `aastore` predicate, not a
            // `ClassId` comparison. A `ClassId` comparison is wrong in both
            // directions here. On a REFERENCE ARRAY the header's class id holds
            // the COMPONENT class, so an element that is itself an array
            // (`String[]`) answers `java/lang/String` while the destination
            // component of a `String[][]` is `[Ljava/lang/String;` — they can
            // never be equal, and `Arrays.copyOf(.., String[][].class)` threw a
            // FALSE `ArrayStoreException` naming `java.lang.String`. That is
            // the whole of H2's `SortOrder.sort` failure: `rows.toArray(new
            // Value[0][])` reaches here once the `ArrayList.toArray` synthetic
            // stub is refused under `--real-jdk`. It also misses every hedge the
            // predicate carries for interface components, `$Proxy` values and
            // cross-loader same-named classes — each of which was paid for by a
            // regression. `None` is a mock with no hierarchy: keep the exact
            // check there rather than silently widening it.
            let comp_is_object = comp_cid.and_then(|c| ctx.class_name_of_id(c)).as_deref()
                == Some("java/lang/Object");
            for i in 0..src_len.min(new_len) {
                let e = ctx.get_array_element(src, i);
                if let (Some(cid), Value::Object(Some(v)), false) = (comp_cid, e, comp_is_object) {
                    let assignable = match ctx.aastore_element_assignable(dst, v) {
                        Some(verdict) => verdict,
                        None => {
                            let actual = ctx.class_id_of_object(v);
                            actual == cid || ctx.is_subclass(actual, cid)
                        }
                    };
                    if !assignable {
                        // HotSpot reaches this exception through the
                        // `System.arraycopy` inside `Arrays.copyOf`'s own
                        // bytecode, so the text is arraycopy's. MEASURED:
                        //   arraycopy: element type mismatch: can not cast one
                        //   of the elements of java.lang.Object[] to the type
                        //   of the destination array, java.lang.String
                        // The old message was the offending element's class
                        // name alone, which no HotSpot path produces.
                        return Err(RuntimeError::ArrayStoreException {
                            message: crate::lang_system::element_type_mismatch_message(
                                ctx, src, cid,
                            ),                        }
                        .into());                    }                }
                ctx.set_array_element(dst, i, e);            }
            Ok(Some(Value::Object(Some(dst))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/util/Arrays",
        "copyOfRange",
        "([Ljava/lang/Object;II)[Ljava/lang/Object;",
        |ctx, args| {
            let src = arrays_src_or_npe(args, "copyOfRange")?;
            let from_i = match args.get(1) {
                Some(Value::Int(n)) => *n,
                _ => 0,
            };
            let to_i = match args.get(2) {
                Some(Value::Int(n)) => *n,
                _ => 0,
            };
            // Bounds BEFORE the length, and against the source's real length:
            // `to` past the end is legal and pads, `from` past the end is not.
            let (from, new_len) = arrays_range_or_throw(from_i, to_i, ctx.array_length(src))?;
            let comp_cid = ctx.class_id_of_object(src);
            // gc-common w8-b: reclaim and retry, `src` refreshed.
            let mut live = [src];
            let dst =
                new_ref_array_reclaiming(ctx, comp_cid, new_len, &mut live, "Arrays.copyOfRange")?;
            let [src] = live;
            let src_len = ctx.array_length(src);
            crate::arrays_copy_elements(
                ctx,
                src,
                from,
                dst,
                new_len.min(src_len.saturating_sub(from)),
                true,
            );
            Ok(Some(Value::Object(Some(dst))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register("java/util/Arrays", "copyOf", "([II)[I", |ctx, args| {
        let src = arrays_src_or_npe(args, "copyOf")?;
        let new_len = match args.get(1) {
            Some(Value::Int(n)) => arrays_len_or_negative(*n)?,
            _ => 0,
        };
        let src_len = ctx.array_length(src);
        // gc-common w8-b: reclaim and retry, `src` refreshed.
        let mut live = [src];
        let dst = new_array_reclaiming(
            ctx,
            cratonvm_types::ArrayElementType::Int,
            new_len,
            &mut live,
            "Arrays.copyOf",
        )?;
        let [src] = live;
        crate::arrays_copy_elements(ctx, src, 0, dst, src_len.min(new_len), false);
        Ok(Some(Value::Object(Some(dst))))
    }, cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        "java/util/Arrays",
        "copyOfRange",
        "([BII)[B",
        |ctx, args| {
            let src = arrays_src_or_npe(args, "copyOfRange")?;
            let from_i = match args.get(1) {
                Some(Value::Int(n)) => *n,
                _ => 0,
            };
            let to_i = match args.get(2) {
                Some(Value::Int(n)) => *n,
                _ => 0,
            };
            // Bounds BEFORE the length, and against the source's real length:
            // `to` past the end is legal and pads, `from` past the end is not.
            let (from, new_len) = arrays_range_or_throw(from_i, to_i, ctx.array_length(src))?;
            // gc-common w8-b: reclaim and retry, `src` refreshed.
            let mut live = [src];
            let dst = new_array_reclaiming(
                ctx,
                cratonvm_types::ArrayElementType::Byte,
                new_len,
                &mut live,
                "Arrays.copyOfRange",
            )?;
            let [src] = live;
            let src_len = ctx.array_length(src);
            crate::arrays_copy_elements(
                ctx,
                src,
                from,
                dst,
                new_len.min(src_len.saturating_sub(from)),
                false,
            );
            Ok(Some(Value::Object(Some(dst))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register("java/util/Arrays", "copyOf", "([BI)[B", |ctx, args| {
        let src = arrays_src_or_npe(args, "copyOf")?;
        let new_len = match args.get(1) {
            Some(Value::Int(n)) => arrays_len_or_negative(*n)?,
            _ => 0,
        };
        let src_len = ctx.array_length(src);
        // gc-common w8-b: reclaim and retry, `src` refreshed.
        let mut live = [src];
        let dst = new_array_reclaiming(
            ctx,
            cratonvm_types::ArrayElementType::Byte,
            new_len,
            &mut live,
            "Arrays.copyOf",
        )?;
        let [src] = live;
        crate::arrays_copy_elements(ctx, src, 0, dst, src_len.min(new_len), false);
        Ok(Some(Value::Object(Some(dst))))
    }, cratonvm_native_api::NativeKind::Bridge);

    // CharBuffer.toString / toString(II) — abstract on CharBuffer, our
    // subSequence and wrap natives return synthetic `java/nio/CharBuffer`
    // instances. In real-JDK mode the full `register_p62_char_buffer`
    // (which lives in phases_late) isn't called, so the toString natives
    // never get installed. Without them, ICU normalization data load
    // (`ICUBinary.getString` -> `CharBuffer.subSequence().toString()`)
    // throws AbstractMethodError and cascades into the Jetty
    // `Main.processCommandLine` NPE. Wire the essentials here so the
    // real-JDK path picks them up too.
    crate::phases_late::register_p62_char_buffer(registry);

    // DELETED (W3): the `jdk/net/LinuxSocketOptions` family used to be
    // registered here as well, byte-identical to the copy in
    // `native-io/src/net.rs` (`register_sun_nio_ch_net`). That copy always won:
    // `vm/src/vm/vm_init.rs` calls `register_essential_natives_with_shims`
    // (this function) BEFORE `register_io_natives` on every path - the
    // synthetic arm (~L1252/1253), the real-JDK arm (~L1338/1523) and the third
    // arm (~L1768/1953) - and registration is last-write-wins. Keeping a second,
    // silently-dead copy only sets a trap for the next reordering, so
    // `native-io/src/net.rs` is now the single owner: it reports the capability
    // probes false and throws UnsupportedOperationException from the get/set
    // natives rather than swallowing them.

    // `sun/nio/ch/Net.shouldShutdownWriteBeforeClose0()Z` — queried once
    // during `NioSocketImpl.<clinit>` (same static-init chain as the
    // LinuxSocketOptions/NativeThread/UnixDispatcher natives above) to
    // decide whether closing a socket should shutdown(SHUT_WR) first for
    // graceful half-close delivery. `true` mirrors real Linux's answer
    // (this is the platform we're emulating here); our own socket I/O
    // path does its own close/shutdown handling independent of this
    // JDK-internal flag, so the exact value mainly needs to avoid the
    // UnsatisfiedLinkError, not drive real behavior.
    registry.register(
        "sun/nio/ch/Net",
        "shouldShutdownWriteBeforeClose0",
        "()Z",
        |_ctx, _args| Ok(Some(Value::Int(1))),
        NativeKind::Bridge,
    );

    // `sun/net/dns/ResolverConfigurationImpl.{init0,loadDNSconfig0,notifyAddrChange0}`
    // — reads the OS's configured DNS search list/nameservers (on Windows, via
    // the IP Helper API). Netty's `DnsServerAddressStreamProviders` consults
    // this as a courtesy default-resolver config; without any of these three
    // natives registered, `init0()` (called from static init, before the
    // other two) threw `UnsatisfiedLinkError`, leaving
    // `sun.net.dns.ResolverConfigurationImpl`/`ResolverConfiguration` in the
    // JVM's permanent "erroneous class" state — cascading to
    // `NoClassDefFoundError: io/netty/resolver/dns/DnsServerAddressStreamProviders$DefaultProviderHolder`
    // for every later reference, breaking Reactor-Netty-based HTTP client
    // tests entirely. We don't surface real OS DNS config (no IP Helper API
    // integration), so publish empty *strings* for both OS-provided values.
    // This is deliberately not null: the real JDK's `loadConfig()` calls
    // `stringToList(os_nameservers)` unconditionally, and its implementation
    // calls `String.split` without a null guard. An empty string preserves the
    // intended empty-config semantics for Netty while also allowing the
    // MongoDB driver's JNDI TXT resolver to reach its ordinary no-nameserver
    // path instead of failing during resolver initialization.
    // `notifyAddrChange0()` drives the optional network-change listener —
    // see its own registration below for why it must NOT answer 0.
    registry.register(
        "sun/net/dns/ResolverConfigurationImpl",
        "init0",
        "()V",
        |ctx, _args| {
            let searchlist = ctx.create_string("");
            ctx.set_static_field_by_name(
                "sun/net/dns/ResolverConfigurationImpl",
                "os_searchlist",
                Value::Object(Some(searchlist)),
            );
            let nameservers = ctx.create_string(&os_dns_nameservers_string());
            ctx.set_static_field_by_name(
                "sun/net/dns/ResolverConfigurationImpl",
                "os_nameservers",
                Value::Object(Some(nameservers)),
            );
            Ok(None)
        },
        NativeKind::Bridge,
    );
    registry.register(
        "sun/net/dns/ResolverConfigurationImpl",
        "loadDNSconfig0",
        "()V",
        |ctx, _args| {
            let searchlist = ctx.create_string("");
            ctx.set_static_field_by_name(
                "sun/net/dns/ResolverConfigurationImpl",
                "os_searchlist",
                Value::Object(Some(searchlist)),
            );
            let nameservers = ctx.create_string(&os_dns_nameservers_string());
            ctx.set_static_field_by_name(
                "sun/net/dns/ResolverConfigurationImpl",
                "os_nameservers",
                Value::Object(Some(nameservers)),
            );
            Ok(None)
        },
        NativeKind::Bridge,
    );
    // `notifyAddrChange0()` is NOT a passive query — it is a BLOCKING wait,
    // and `ResolverConfigurationImpl$AddressChangeListener.run()` is
    //     for (;;) { if (notifyAddrChange0() != 0) return;
    //                synchronized (lock) { changed = true; } }
    // so 0 means "an address change happened, keep watching". Returning it
    // immediately turned that listener — a daemon thread the class's
    // <clinit> starts at Thread.MAX_PRIORITY — into a hot spin that burned a
    // core for the life of the process, and marked the resolver config dirty
    // on every iteration. A non-zero result is the documented "no
    // notification mechanism" answer: the listener returns and the thread
    // exits. CratonVM's resolver configuration is read once at init0 and
    // never reloaded, so there is nothing for a listener to observe anyway.
    registry.register(
        "sun/net/dns/ResolverConfigurationImpl",
        "notifyAddrChange0",
        "()I",
        |_ctx, _args| Ok(Some(Value::Int(-1))),
        NativeKind::Bridge,
    );
    registry.register(
        "sun/net/dns/ResolverConfigurationImpl",
        "fallbackDomain0",
        "()Ljava/lang/String;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
        NativeKind::Bridge,
    );

    // JNDI DNS uses PortConfig to select a UDP source port. These are native
    // JDK methods (not Java fallbacks), so real-JDK mode otherwise stops at an
    // UnsatisfiedLinkError before the TXT query can be issued.
    registry.register(
        "sun/net/PortConfig",
        "getLower0",
        "()I",
        |_ctx, _args| Ok(Some(Value::Int(system_ephemeral_port_range().0))),
        NativeKind::Bridge,
    );
    registry.register(
        "sun/net/PortConfig",
        "getUpper0",
        "()I",
        |_ctx, _args| Ok(Some(Value::Int(system_ephemeral_port_range().1))),
        NativeKind::Bridge,
    );

    // NOT registered here: `EventExecutorGroup.shutdownGracefully()`.
    //
    // The zero-quiet-period shutdown that MongoDB Reactive Streams' lifecycle
    // needs lives in `native_netty_event_executor_group_shutdown_gracefully`,
    // and `native_springboot_mongo_reactive_customizer_destroy` — itself a
    // full native replacement of the one Spring bean method that hung — calls
    // it DIRECTLY. Registering it against `EventExecutorGroup`,
    // `AbstractEventExecutorGroup` and `MultiThreadIoEventLoopGroup` as well
    // was meant to be "restricted to that concrete Netty 4.2 group", but
    // `MultiThreadIoEventLoopGroup` is the ordinary group every Netty 4.2
    // application constructs, so the restriction admitted everything: EVERY
    // `group.shutdownGracefully()` in the process became
    // `shutdownGracefully(0, 0, MILLISECONDS)`.
    //
    // A zero quiet period does not drain. Netty runs a closed channel's
    // deregistration — and therefore `ChannelHandler.handlerRemoved` — as a
    // task on the event loop, so with the quiet period gone the loop
    // terminated with that task still queued. `PcapWriteHandler` writes its
    // fake TCP FIN/FIN-ACK/ACK sequence from `handlerRemoved`, so the capture
    // came out byte-exact through the last data packet and then simply
    // stopped: 522 bytes instead of 732 (docs/feature-designs/native-dispatch-architecture.md, residual 2). Anything
    // that relies on graceful shutdown actually being graceful had the same
    // hole. Ordinary groups keep their own bytecode, and their own defaults.
    registry.register(
        "org/springframework/boot/mongodb/autoconfigure/MongoReactiveAutoConfiguration$NettyDriverMongoClientSettingsBuilderCustomizer",
        "customize",
        "(Lcom/mongodb/MongoClientSettings$Builder;)V",
        native_springboot_mongo_reactive_customizer_customize, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/springframework/boot/mongodb/autoconfigure/MongoReactiveAutoConfiguration$NettyDriverMongoClientSettingsBuilderCustomizer",
        "destroy",
        "()V",
        native_springboot_mongo_reactive_customizer_destroy, cratonvm_native_api::NativeKind::Bridge,
    );

    // `sun/nio/ch/NativeThread.supportPendingSignals0()Z` — a Linux-only
    // probe (`UnixDispatcher.<clinit>` -> `NativeThread.supportPendingSignals()`)
    // for whether this platform can deliver a pending POSIX signal to
    // interrupt a thread blocked in native I/O. Real Linux supports this
    // (that mechanism is the whole reason `UnixDispatcher`/`NativeThread`
    // exist), so `true` matches HotSpot's real answer on this OS — unlike
    // the `false` stubs elsewhere in this block, this is not a "safe
    // default for an absent feature" but the actual Linux capability.
    // Without it, `NioSocketImpl.<clinit>` (reached the first time ANY
    // code touches a real `java.net.Socket`'s `SocketImpl`, e.g.
    // `Socket.supportedOptions()` during `jdk.net.Sockets.optionSets()`)
    // threw `UnsatisfiedLinkError`, leaving `NioSocketImpl` erroneous and
    // cascading to `NoClassDefFoundError` for `DefaultHttpClientConnectionOperator`
    // — breaking Apache HttpClient5-based tests entirely on this host.
    registry.register(
        "sun/nio/ch/NativeThread",
        "supportPendingSignals0",
        "()Z",
        |_ctx, _args| Ok(Some(Value::Int(1))),
        NativeKind::Bridge,
    );

    // `sun/nio/ch/UnixDispatcher.init()V` — one-time native bookkeeping
    // init for the Unix blocking-I/O dispatcher (reached right after the
    // `supportPendingSignals0` check above, same `<clinit>` chain). We
    // don't need any native-side state for this — our socket I/O doesn't
    // route through a `UnixDispatcher` table — so a no-op keeps
    // `NioSocketImpl.<clinit>` progressing.
    registry.register(
        "sun/nio/ch/UnixDispatcher",
        "init",
        "()V",
        |_ctx, _args| Ok(None),
        NativeKind::Bridge,
    );

    // `sun/nio/ch/sctp/SctpChannelImpl.initIDs()V`, `SctpNet.init()V`, and
    // `SctpNet.socket0(Z)I`: initIDs/init succeed during `<clinit>` while
    // `socket0` throws UnsupportedOperationException when libsctp.so.1 is absent.
    for (cls, method) in [
        ("sun/nio/ch/sctp/SctpChannelImpl", "initIDs"),
        ("sun/nio/ch/sctp/SctpNet", "init"),
    ] {
        registry.register(
            cls,
            method,
            "()V",
            |_ctx, _args| Ok(None),
            NativeKind::Bridge,
        );
    }
    registry.register(
        "sun/nio/ch/sctp/SctpNet",
        "socket0",
        "(Z)I",
        |_ctx, _args| {
            Err(RuntimeError::UnsupportedOperationException {
                message: "libsctp.so.1: cannot open shared object file: No such file or directory".to_string(),
            }
            .into())
        },
        NativeKind::Bridge,
    );

    crate::phases_late::register_p66_file_visitor(registry);
    // The session carriers are `SSLSessionImpl` instances: every `SSLSession` row
    // is mirrored onto it, after the last registrar that writes one.
    t27_tls::mirror_session_rows_onto_impl(registry);
    // The socket and factory carriers likewise (`tls_carriers`).
    tls_carriers::mirror_socket_family_rows_onto_impls(registry);
    // Restore the caller's category so later registrars keep their intended tag.
}
