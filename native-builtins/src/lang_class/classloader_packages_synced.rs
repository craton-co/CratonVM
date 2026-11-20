#[allow(unused_imports)]
use super::*;

/// `ClassLoader.packages()`, a reviewed `Intrinsic` registered over a real JDK
/// with `CRATONVM_BUILTIN_LOADER_PACKAGE_SYNC`: for a built-in loader,
/// [`sync_builtin_loader_packages`] first, then the JDK's own body. It adds
/// entries the JDK's `BuiltinClassLoader.defineClass` would have written,
/// never changes how the body answers.
pub fn classloader_packages_synced(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(Value::Object(Some(loader))) = args.first() {
        let pin = ctx.pin_native_root(*loader);
        sync_builtin_loader_packages(ctx, *loader);
        let loader = ctx.read_native_pin(pin, *loader);
        ctx.unpin_native_roots(pin);
        return ctx.invoke_special_bytecode_only(
            "java/lang/ClassLoader",
            "packages",
            "()Ljava/util/stream/Stream;",
            &[Value::Object(Some(loader))],
        );
    }
    ctx.invoke_special_bytecode_only(
        "java/lang/ClassLoader",
        "packages",
        "()Ljava/util/stream/Stream;",
        args,
    )
}

/// Put the memo's `Package` into the built-in loader's own `packages` map for
/// the class just defined. This is HotSpot's `ClassLoader.postDefineClass`
/// (`getNamedPackage`) bookkeeping, and uses the just-defined class as its
/// representative before it is guaranteed to appear in the loader index.
/// Runs Java (the map's `get` / `putIfAbsent`); callers refresh rooted objects.
pub fn sync_builtin_loader_package_for_class(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    name: &str,
    class_id: cratonvm_types::ClassId,
) -> Result<bool, MethodCallFailed> {
    sync_builtin_loader_package_with_representative(ctx, loader, name, Some(class_id))
}

pub fn sync_builtin_loader_package_with_representative(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    name: &str,
    representative: Option<cratonvm_types::ClassId>,
) -> Result<bool, MethodCallFailed> {
    let vm = ctx.vm_identity();
    if name.contains('/')
        || !ctx.builtin_loader_package_sync()
        || !crate::classloader::loader_is_builtin(ctx, loader)
    {
        return Ok(false);
    }
    let ns = crate::classloader::loader_namespace_id(ctx, loader);
    let platform = match ns {
        cratonvm_types::ClassLoaderId::NATIVE_APPLICATION => false,
        cratonvm_types::ClassLoaderId::NATIVE_EXTENSION => true,
        _ => return Ok(false),
    };
    let key = (vm, ns, name.to_string());
    let sync_lock = builtin_loader_package_sync_lock(
        crate::classloader::loader_tables(ctx),
        &key,
    );
    let _sync_guard = sync_lock.lock().unwrap_or_else(|e| e.into_inner());
    let synced = {
        defined_package_memo(crate::classloader::loader_tables(ctx))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .is_some_and(|row| row.in_loader_map)
    };
    if synced {
        return Ok(true);
    }
    // The representative, as in [`sync_builtin_loader_packages`]: the
    // loader's own tag, then the boot tag (0) that an image class of a module
    // the JDK's table defines to this loader carries.
    let slashed = name.replace('.', "/");
    let mut rep = representative.or_else(|| ctx.loaded_package_representative(ns, &slashed));
    if rep.is_none()
        && !slashed.is_empty()
        && crate::classloader::image_package_defined_to(ctx, &slashed, platform)
    {
        rep = ctx.loaded_package_representative(0, &slashed);
    }
    let Some(cid) = rep else {
        return Ok(false);
    };
    if false {
        eprintln!("[builtin-loader-packages] ns={ns}: sync {name:?} for getDefinedPackage");    }
    // GC: every step below can run Java. The loader is pinned and re-read
    // before each use; the `Package` is pinned across the key's allocation.
    let loader_pin = ctx.pin_native_root(loader);
    let marked = (|| -> Result<bool, MethodCallFailed> {
        let loader = ctx.read_native_pin(loader_pin, loader);
        if let Some(entry) = real_defined_package(ctx, Some(loader), name) {
            // A built-in loader can already hold a lazy NamedPackage or a
            // Package created by its JDK body. Make that object the shared
            // memo value too, so Class.getPackage and getDefinedPackage keep
            // the identity HotSpot's NamedPackage cache provides.
            let package = match entry {
                RealPackageEntry::Package(package) => Some(package),
                RealPackageEntry::Named(_) => {
                    let key = ctx.intern_string(name);
                    let key_pin = ctx.pin_native_root(key);
                    let loader = ctx.read_native_pin(loader_pin, loader);
                    let key = ctx.read_native_pin(key_pin, key);
                    let value = ctx.invoke_virtual(
                        loader,
                        "getDefinedPackage",
                        "(Ljava/lang/String;)Ljava/lang/Package;",
                        &[Value::Object(Some(key))],
                    );
                    ctx.unpin_native_roots(key_pin);
                    match value? {
                        Some(Value::Object(Some(package))) => Some(package),
                        _ => None,
                    }
                }
            };
            if let Some(package) = package {
                publish_defined_package(
                    crate::classloader::loader_tables(ctx),
                    key.clone(),
                    package,
                    true,
                    (None, None),
                );
                if let Some(row) = defined_package_memo(crate::classloader::loader_tables(ctx))
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_mut(&key)
                {
                    row.in_loader_map = true;
                }
                return Ok(true);
            }
            return Ok(false);
        }
        let mirror = ctx.get_class_mirror(cid);
        let Some(Value::Object(Some(pkg))) =
            native_class_get_package(ctx, &[Value::Object(Some(mirror))])?
        else {
            return Ok(false);
        };
        let pkg_pin = ctx.pin_native_root(pkg);
        // Interned, as the JDK's key is (`Class.getPackageName()` interns).
        let key = ctx.intern_string(name);
        let pkg = ctx.read_native_pin(pkg_pin, pkg);
        ctx.unpin_native_roots(pkg_pin);
        let loader = ctx.read_native_pin(loader_pin, loader);
        let Value::Object(Some(map)) = ctx.get_field_by_name(loader, "packages") else {
            return Ok(false);
        };
        ctx.invoke_virtual(
            map,
            "putIfAbsent",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(key)), Value::Object(Some(pkg))],
        )?;
        Ok(true)
    })();
    ctx.unpin_native_roots(loader_pin);
    if marked.as_ref().is_ok_and(|marked| *marked) {
        if let Some(row) = defined_package_memo(crate::classloader::loader_tables(ctx))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&key)
        {
            row.in_loader_map = true;
        }
    }
    marked
}

/// The JDK 25 body of `ClassLoader.getDefinedPackage(name)` for a non-empty
/// `name`, where its answer is decided by `packages.get(name)` alone:
///
/// ```java
/// NamedPackage p = packages.get(name);          // the one read made here
/// if (p == null) return null;                    // -> Some(Ok(null))
/// return definePackage(name, p.module());        // definePackage: a second
///     // packages.get(name); `instanceof Package` -> that Package
/// ```
///
/// What the receiver's own `packages` map holds for a name.
pub enum RealPackageEntry {
    /// A full `java.lang.Package` — hand back this exact object, so
    /// `getDefinedPackage(p) == definePackage(p, ...)` as the JDK guarantees.
    Package(ObjectRef),
    /// The JDK's lazy `NamedPackage` form (the entry). The package IS
    /// defined; we supply the `Package` object ourselves, memoised for
    /// identity stability, in the entry's module.
    Named(ObjectRef),
}

/// `ClassLoader.getDefinedPackage(String name) -> Package`.
///
/// Answers, in order: the receiver's own real `packages` map (what
/// `definePackage` actually wrote — see [`real_defined_package`]), then a memo
/// of packages this VM has synthesised, then class files visible **to the
/// receiving loader**, memoising one `Package` identity per (loader namespace,
/// package name).
///
/// The real-map step exists because the other two cannot see a loader that
/// defines classes from bytes rather than from the classpath. The classpath
/// step exists because a loader's map is not always readable — it can be
/// uninitialised for a user-created loader, the original I2/ByteBuddy blocker.
/// Neither is sufficient alone.
///
/// Returning null unconditionally, as this once did, violates the application
/// loader's contract: Spring Boot's `BeanDefinitionLoader` uses this probe both
/// to distinguish a package directory from an XML resource
/// (`isLoadCandidate`) and as the sole result of `findPackage`, so an
/// always-null answer turned every package-name `SpringApplication` source into
/// `IllegalArgumentException: Invalid source '<pkg>'`.
pub fn i2_classloader_get_defined_package(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // A NULL package name is a NullPointerException, not "no such package".
    // MEASURED in both modes: HotSpot NPE, this VM answered null -- and null is
    // also this method's ordinary answer for an absent package, so the two were
    // indistinguishable to the caller.
    let Some(Value::Object(Some(name_obj))) = args.get(1) else {
        // JDK 25: `Objects.requireNonNull(name, "name cannot be null")`
        // (interpreter round i2 wave 53, lane L3; probe
        // `L3W53ReflectionNullArgs`); its frames are the funnel's mark's.
        return Err(cratonvm_types::error::RuntimeError::NullPointerException {
            message: Some("name cannot be null".to_string()),
        }
        .into());
    };
    let package_name = ctx.read_string(*name_obj).unwrap_or_default();
    if package_name.contains('/') {
        return Ok(Some(Value::Object(None)));
    }
    let loader = match args.first() {
        Some(Value::Object(Some(l))) => Some(*l),
        _ => None,
    };

    // `--real-jdk` (SBR3 `refmisc`): the package this loader was already
    // answered for is the memo's, and it is the one `Class.getPackage()` hands
    // out. The JDK's own `Class.getPackage` body reaches this for every boot
    // and platform class, so the answer must not cost a Java `packages.get`
    // upcall each time. A package's real `definePackage` happens at its first
    // class's definition, before anything can ask for it by class, so no later
    // real object can disagree with a row published here.
    if ctx.is_jdk_only() {
        if let Some(l) = loader {
            let ns = crate::classloader::loader_namespace_id(ctx, l);
            let hit = defined_package_memo(crate::classloader::loader_tables(ctx))
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&(ctx.vm_identity(), ns, package_name.clone()))
                .map(|row| row.pkg);
            if let Some(pkg) = hit {
                return Ok(Some(Value::Object(Some(pkg))));
            }
        }
    }

    // The loader's own record first: it is the same state `definePackage`
    // writes, so agreeing with it is the whole contract.
    // gen r4w3/rooting: `real_defined_package` runs `packages.get` (Java);
    // pin the loader across it and use the re-read address afterwards.
    let loader_pin = loader.map(|l| ctx.pin_native_root(l));
    let real = real_defined_package(ctx, loader, &package_name);
    let loader = match (loader, loader_pin) {
        (Some(l), Some(pin)) => Some(ctx.read_native_pin(pin, l)),
        _ => None,
    };
    if let Some(pin) = loader_pin {
        ctx.unpin_native_roots(pin);
    }
    if let Some(RealPackageEntry::Package(pkg)) = real {
        return Ok(Some(Value::Object(Some(pkg))));
    }
    let defined_lazily = matches!(real, Some(RealPackageEntry::Named(_)));
    // The entry's module: the JDK's `definePackage(name, p.module())` makes
    // the `Package` in it, and `Module.getPackages()` of an unnamed module
    // keeps only `p.module() == this`. The `Package` built below took the
    // canonical unnamed module, so a user loader's unnamed module listed no
    // package (interpreter round i2 wave 57, lane L5; probe
    // `L5W44LoaderReview`, `unnamed-module`). Read now (no GC point since the
    // map read) and pinned across the allocations below.
    let named_module_pin = match real {
        Some(RealPackageEntry::Named(entry)) => match ctx.get_field_by_name(entry, "module") {
            Value::Object(Some(m)) => Some((ctx.pin_native_root(m), m)),
            _ => None,
        },
        _ => None,
    };
    // The default package is a package, and a built-in loader that has loaded
    // a class from it HAS defined it (row C02). A custom loader has not, and
    // must not inherit the global classpath's answer (row N02).
    if package_name.is_empty()
        && !defined_lazily
        && !loader.is_some_and(|l| crate::classloader::loader_is_builtin(ctx, l))
    {
        // ... UNLESS this loader has itself defined a class in the default
        // package, which is the one way a custom loader DOES define it.
        // MEASURED on HotSpot: `new ClassLoader(null){}.getDefinedPackage("")`
        // is `null`, and becomes the unnamed `Package` the moment the loader
        // defines a class with no package. The unconditional `null` here was
        // right about row N02 (a loader that defined nothing must not inherit
        // the class path's answer) and wrong about this one.
        let defined_here = loader
            .map(|l| crate::classloader::loader_namespace_id(ctx, l))
            .filter(|ns| *ns >= cratonvm_types::ClassLoaderId::NATIVE_FIRST_USER_DEFINED)
            .is_some_and(|ns| ctx.any_loaded_class_in_package_for_loader("", ns));
        if !defined_here {
            return Ok(Some(Value::Object(None)));
        }
    }
    let ns = loader.map_or(0, |l| crate::classloader::loader_namespace_id(ctx, l));
    let vm = ctx.vm_identity();

    // Current: rows are re-addressed after every moving collection, and a
    // dead loader's rows are dropped in that collection's epilogue
    // (gc-common w31-a).
    let cached = defined_package_memo(crate::classloader::loader_tables(ctx))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(vm, ns, package_name.clone()))
        .map(|row| row.pkg);
    if let Some(pkg) = cached {
        let pkg = adopt_named_package_module(ctx, pkg, named_module_pin);
        return Ok(Some(Value::Object(Some(pkg))));
    }

    // `CRATONVM_JDK_BUILT_PACKAGES` (interpreter round i2 wave 58, lane L5):
    // the application loader has defined a class-path package exactly when it
    // has defined a class of it (HotSpot's `BuiltinClassLoader.defineClass`
    // runs `defineOrCheckPackage` first), so its map -- read above -- is the
    // answer once [`native_class_get_package`] of such a class has built the
    // JDK's `Package`; a package none of its classes is in is `null`, not the
    // class-file probe's fabricated `Package` below.
    if !defined_lazily
        && ns == cratonvm_types::ClassLoaderId::NATIVE_APPLICATION
        && ctx.jdk_built_packages()
    {
        // The listing's class for this one package, without the listing
        // (wave 94, L5: same answer, no `String` per package; one probe with
        // `CRATONVM_LOADER_PACKAGE_INDEX`).
        let slashed = package_name.replace('.', "/");
        let rep = ctx.loaded_package_representative(ns, &slashed);
        let Some(cid) = rep else {
            return Ok(Some(Value::Object(None)));
        };
        let mirror = ctx.get_class_mirror(cid);
        return native_class_get_package(ctx, &[Value::Object(Some(mirror))]);
    }

    // The platform loader, the same rule (interpreter round i4 wave 92, lane
    // L5; `i55-L5-proposal-built-in-loaders-answer-packages-from-their-map`):
    // HotSpot's platform loader puts a package into its map when a class of
    // it is defined (`ClassLoader.postDefineClass` -> `getNamedPackage`, or
    // CDS's `define_shared_package`), and `getDefinedPackage` is that map. So a
    // package with a loaded class is the `Package` the listing sync puts there
    // (the memo's, through [`native_class_get_package`] of that class), and a
    // package with none is `null` -- not the class-file probe's fabricated
    // `Package` below (`java.sql` before `java.sql.Date` loads). An image
    // class of a platform module carries the boot tag (0), as in
    // [`sync_builtin_loader_packages`]. `CRATONVM_PLATFORM_LOADER_PACKAGE_MAP=0`
    // keeps the probe.
    if !defined_lazily
        && ns == cratonvm_types::ClassLoaderId::NATIVE_EXTENSION
        && ctx.builtin_loader_package_sync()
    {
        let slashed = package_name.replace('.', "/");
        let mut rep = ctx.loaded_package_representative(ns, &slashed);
        if rep.is_none()
            && !slashed.is_empty()
            && crate::classloader::image_package_defined_to(ctx, &slashed, true)
        {
            rep = ctx.loaded_package_representative(0, &slashed);
        }
        let Some(cid) = rep else {
            return Ok(Some(Value::Object(None)));
        };
        let mirror = ctx.get_class_mirror(cid);
        return native_class_get_package(ctx, &[Value::Object(Some(mirror))]);
    }

    // A user `URLClassLoader` whose own bodies run (`--real-jdk`, or
    // `--compatible` with the `JdkUrlClassLoaders` row) defines its packages
    // through the JDK's `URLClassLoader.defineClass` -> `definePackage(name,
    // manifest, url)`, so its `packages` map, read above, is the whole answer,
    // as in the JDK's body. The class-file probe below fabricated a `Package`
    // before the loader defined anything, which made Spring Boot's
    // `definePackageIfNecessary` and `getAndVerifyPackage` skip the
    // manifest-carrying `definePackage`, and cost a glob over every entry of
    // every jar of the loader per miss (interpreter round i2 wave 53, lane L5;
    // probe `L5W53UrlLoaderDefinedPackage`).
    if !defined_lazily
        && ctx.jdk_url_class_loaders()
        && loader.is_some_and(|l| {
            crate::classloader::is_user_defined_loader(ctx, l)
                && crate::classloader::object_extends(ctx, l, "java/net/URLClassLoader")
        })
    {
        if !ctx.is_jdk_only() {
            ctx.note_alignment_divergence(cratonvm_types::HotSpotAlignment::JdkUrlClassLoaders);
        }
        return Ok(Some(Value::Object(None)));
    }

    // A class file immediately below the package path is a conservative,
    // classpath-backed proof that the package exists. It avoids synthesising
    // packages for arbitrary names or resource-only directories. The probe is
    // scoped to the receiver because `getDefinedPackage` does NOT delegate —
    // see `package_class_files_visible_to_loader`.
    if !defined_lazily {
        let class_glob = package_class_glob(&package_name);
        if !crate::classloader::package_class_files_visible_to_loader(
            ctx,
            loader,
            &package_name,
            &class_glob,
        ) {
            return Ok(Some(Value::Object(None)));
        }
    }
    let package = match i2_alloc_synthetic_package(ctx, &package_name) {
        Ok(package) => package,
        Err(e) => {
            if let Some((pin, _)) = named_module_pin {
                ctx.unpin_native_roots(pin);
            }
            return Err(e);
        }
    };
    let package = adopt_named_package_module(ctx, package, named_module_pin);
    // gc-common w31-a: the owner is looked up AFTER the allocation (a GC
    // point), from the remapped namespace store, never from the `loader`
    // local read before it; nothing between here and the insert is a GC point.
    let owner = defined_package_owner(ctx, ns, None);
    let tables = crate::classloader::loader_tables(ctx);
    publish_defined_package(tables, (vm, ns, package_name), package, false, owner);
    Ok(Some(Value::Object(Some(package))))
}

/// Give `pkg` the module of the loader's `NamedPackage` entry (pinned by
/// [`i2_classloader_get_defined_package`]) and release the pin. No GC point.
pub fn adopt_named_package_module(
    ctx: &mut dyn NativeContext,
    pkg: ObjectRef,
    named_module_pin: Option<(usize, ObjectRef)>,
) -> ObjectRef {
    if let Some((pin, module)) = named_module_pin {
        let module = ctx.read_native_pin(pin, module);
        ctx.unpin_native_roots(pin);
        ctx.set_field_by_name(pkg, "module", Value::Object(Some(module)));
    }
    pkg
}

/// `ClassLoader.getDefinedPackages() -> Package[]` — the packages this loader
/// has actually answered `getDefinedPackage` for, so the two agree.
///
/// Note this is deliberately NOT wired to `ClassLoader.getPackages()`, which
/// stays an empty array: that override exists to stop the real JDK's
/// stream-pipeline bytecode leaking a `ReferencePipeline$Head` into a caller's
/// `Package[]` local (jboss-modules' `ConcurrentClassLoader.<clinit>`), a
/// separate concern from this method's own contract.
pub fn i2_classloader_get_defined_packages(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mut loader = match args.first() {
        Some(Value::Object(Some(l))) => Some(*l),
        _ => None,
    };
    // A built-in loader lists every package of a class it defined, as
    // HotSpot's does (interpreter round i2 wave 55, lane L5): put them in
    // its map first. Runs Java: the loader is re-read after it.
    if let Some(l) = loader {
        let sync_pin = ctx.pin_native_root(l);
        sync_builtin_loader_packages(ctx, l);
        loader = Some(ctx.read_native_pin(sync_pin, l));
        ctx.unpin_native_roots(sync_pin);
    }
    let ns = loader.map_or(0, |l| crate::classloader::loader_namespace_id(ctx, l));
    let vm = ctx.vm_identity();

    // gen r4w3/rooting: `real_defined_package_names` and every
    // `getDefinedPackage` below run Java (GC points). The loader is pinned
    // and re-read per call, each package the loop produces is pinned when
    // produced, and the memo's rows are read only after the last upcall
    // (gc-common w31-a: they hold the `Package`s themselves, current after
    // every collection; the values read are bare locals, not roots).
    let loader_pin = loader.map(|l| ctx.pin_native_root(l));
    let names = real_defined_package_names(ctx, loader);
    let mut loop_pkgs: Vec<(usize, ObjectRef)> = Vec::new();

    // Union with the loader's own map, so the plural method reports every
    // package the singular one would answer for. Names come from the map;
    // each is then routed back through `getDefinedPackage` so both methods
    // hand out the SAME object per package rather than two identities.
    for name in names {
        // gc-common w20-f: the key is allocated BEFORE the loader is re-read;
        // the re-read used to come first, so the loader passed on was the
        // address from before the key's allocation.
        let key = ctx.create_string(&name);
        let loader = match (loader, loader_pin) {
            (Some(l), Some(pin)) => Some(ctx.read_native_pin(pin, l)),
            _ => None,
        };
        let single = i2_classloader_get_defined_package(
            ctx,
            &[
                loader.map_or(Value::Object(None), |l| Value::Object(Some(l))),
                Value::Object(Some(key)),
            ],
        );
        if let Ok(Some(Value::Object(Some(pkg)))) = single {
            loop_pkgs.push((ctx.pin_native_root(pkg), pkg));
        }
    }
    let mut packages: Vec<ObjectRef> = defined_package_memo(crate::classloader::loader_tables(ctx))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|((entry_vm, entry_ns, _), _)| *entry_vm == vm && *entry_ns == ns)
        .map(|(_, row)| row.pkg)
        .collect();
    for (pin, pkg) in &loop_pkgs {
        let pkg = ctx.read_native_pin(*pin, *pkg);
        if !packages.contains(&pkg) {
            packages.push(pkg);
        }
    }
    // gen r4w3/rooting: `alloc_package_array` may run `Package`'s <clinit>
    // (GC point); pin every element across it and store the re-read value.
    let elem_pins: Vec<usize> = packages.iter().map(|p| ctx.pin_native_root(*p)).collect();
    // `new_array` can allocate and therefore move; the elements are global
    // roots, so re-resolving is unnecessary, but the array itself must be
    // filled only after it exists.
    //
    // TYPED, not `new_array(Reference, n)`. That allocator produces an array
    // whose runtime component type is `java.lang.Object`, and the JDK declares
    // this method as returning `Package[]` — so a caller doing
    // `Package[] ps = cl.getDefinedPackages()` receives an array that fails
    // `getClass().getComponentType() == Package.class`, and any `aastore` into
    // it or `checkcast` of it is a wrong answer waiting for its first caller.
    // MEASURED by `regression-suite/src/RJdkLangPackages.java`, which is the
    // first vector in this corpus to call the method at all:
    // `expected [class java.lang.Package] got [class java.lang.Object]`.
    let arr = alloc_package_array(ctx, packages.len());
    for (i, (pkg, pin)) in packages.iter().zip(elem_pins.iter()).enumerate() {
        let pkg = ctx.read_native_pin(*pin, *pkg);
        ctx.set_array_element(arr, i, Value::Object(Some(pkg)));
    }
    // gen r4w3/rooting: release from the lowest handle taken above.
    let lowest = loader_pin
        .or_else(|| loop_pkgs.first().map(|(pin, _)| *pin))
        .or_else(|| elem_pins.first().copied());
    if let Some(pin) = lowest {
        ctx.unpin_native_roots(pin);
    }
    Ok(Some(Value::Object(Some(arr))))
}

/// The resource glob matching class files immediately inside `package_name`.
///
/// The default package's class files sit at the classpath ROOT, so its glob is
/// `*.class`. Deriving it uniformly would give `"".replace('.', "/")` +
/// `"/*.class"` = `/*.class` — an absolute path that matches nothing, which
/// reads as "the default package has no classes" on every classpath.
pub fn package_class_glob(package_name: &str) -> String {
    if package_name.is_empty() {
        "*.class".to_string()
    } else {
        format!("{}/*.class", package_name.replace('.', "/"))
    }
}

/// Every package name in the receiver's own `packages` map.
///
/// Empty for a loader whose map is absent, null or unwalkable — the same
/// fail-soft rule as [`real_defined_package`], for the same reason: this
/// override exists precisely because that field cannot be relied upon.
pub fn real_defined_package_names(
    ctx: &mut dyn NativeContext,
    loader: Option<ObjectRef>,
) -> Vec<String> {
    let Some(loader) = loader else {
        return Vec::new();
    };
    let map = match classloader_packages_field(ctx, loader) {
        Value::Object(Some(map)) => map,
        _ => return Vec::new(),
    };
    let mut out = Vec::new();
    let Ok(Some(Value::Object(Some(set)))) =
        ctx.invoke_virtual(map, "keySet", "()Ljava/util/Set;", &[])
    else {
        return out;
    };
    let Ok(Some(Value::Object(Some(it)))) =
        ctx.invoke_virtual(set, "iterator", "()Ljava/util/Iterator;", &[])
    else {
        return out;
    };
    // gen r4w3/rooting: `hasNext`/`next` run Java; the iterator is pinned for
    // the loop and re-read before every call.
    let it_pin = ctx.pin_native_root(it);
    // Bounded: a runaway iterator must not hang a serviceability call. No real
    // loader defines anywhere near this many packages.
    for _ in 0..4096 {
        let it = ctx.read_native_pin(it_pin, it);
        match ctx.invoke_virtual(it, "hasNext", "()Z", &[]) {
            Ok(Some(Value::Int(1))) => {}
            _ => break,
        }
        let it = ctx.read_native_pin(it_pin, it);
        let Ok(Some(Value::Object(Some(k)))) =
            ctx.invoke_virtual(it, "next", "()Ljava/lang/Object;", &[])
        else {
            break;
        };
        if let Some(name) = ctx.read_string(k) {
            out.push(name);
        }
    }
    ctx.unpin_native_roots(it_pin);
    out
}

/// A `java.lang.Package[]` of `len` elements.
///
/// Falls back to the untyped reference allocator only when the class model
/// cannot name `java/lang/Package` — which on any real boot it can, since every
/// caller here has just produced `Package` instances.
pub fn alloc_package_array(ctx: &mut dyn NativeContext, len: usize) -> ObjectRef {
    // LOAD `java.lang.Package`, do not merely look it up.
    //
    // `class_id_by_name` answers only for a class already LOADED, and nothing
    // on the ordinary path to `getDefinedPackages()` loads `java.lang.Package`
    // first -- so the lookup missed, the fallback produced an `Object[]`, and
    // the typed-array comment above this function described a fix that was not
    // in force. MEASURED in BOTH modes
    // (`probes/ClassLoaderShadowSweep.java`):
    //
    //   APP.getDefinedPackages().getClass().getName()
    //     HotSpot  [Ljava.lang.Package;      CratonVM  [Ljava.lang.Object;
    //
    // This is the same shape as the `$RustJvmImpl` superclass bug fixed earlier
    // in this campaign: a lookup-only helper standing where a load belongs, so
    // the guard is present, reached, and inert. `ensure_class_initialized` is
    // the loading form already used elsewhere in this crate for exactly this.
    //
    // The `Reference` fallback is kept for the case where even the load fails:
    // an array of the wrong component type is still better than no array, and
    // the caller's `arraylength` works either way.
    let cid = ctx
        .class_id_by_name("java/lang/Package")
        .or_else(|| ctx.ensure_class_initialized("java/lang/Package").ok());
    match cid {
        Some(cid) => ctx.new_ref_array(cid, len),
        None => ctx.new_array(cratonvm_types::ArrayElementType::Reference, len),
    }
}

/// The single canonical **unnamed** `java.lang.Module` mirror for this VM.
///
/// Every application-classpath class reports the unnamed module from
/// `Class.getModule()`, and the JDK compares `Module`s by identity (it has no
/// `equals` override), so there must be exactly ONE such object. The
/// `Class.getModule()` override in `lib.rs::register_essential_natives` already
/// publishes it under the `None` module-name key; this helper is that same
/// get-or-create, exposed so the `java.lang.Package` builders below and
/// `ClassLoader.getUnnamedModule()` can all hand back the same instance.
///
/// Why every synthesised `Package` MUST carry a non-null `module`: real
/// `Package.getPackageInfo()` (JDK 25, `Package.java:417`) does
/// `module().getClassLoader()` with no null check, so a module-less `Package`
/// NPEs the instant anything asks it for an annotation. TestNG's
/// `IgnoreListener.findAnnotation(Package)` walks a test class's package chain
/// with the deprecated static `Package.getPackage(String)` ->
/// `ClassLoader.getPackage` -> `getDefinedPackage` (overridden below), which
/// returned exactly such a module-less `Package` -- taking out TestNG-engine
/// discovery for the whole `org.springframework.test.context.testng` package
/// with `JUnitException: TestEngine with ID 'testng' failed to discover tests`.
///
/// `loader` is wired to the application class loader because that is what
/// HotSpot reports for an unnamed module (`Module.getClassLoader()` returns the
/// defining loader, never null, for the classpath's unnamed module). Besides
/// matching the JDK, it makes `getPackageInfo()`'s
/// `Class.forName(<pkg>.package-info, false, loader)` resolve against the
/// classpath instead of the bootstrap loader, so package-level annotations
/// (JSpecify `@NullMarked`, TestNG `@Ignore`, ...) are actually visible on a
/// `Package` obtained through any path other than `Class.getPackage()`.
pub fn canonical_unnamed_module(
    ctx: &mut dyn NativeContext,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(cached) = ctx.get_cached_module_mirror(None) {
        return Ok(cached);
    }
    let m = try_alloc_concurrent_synthetic(ctx, "java/lang/Module", 2)?;
    let pin = ctx.pin_native_root(m);
    // Unnamed: BOTH the synthetic 2-field contract's slot 0 and the real
    // `name` field stay null, so `Module.isNamed()` reports false whichever
    // shape the reader assumes (see `module_name_of_mirror` in `lib.rs`).
    ctx.set_field(m, 0, Value::Object(None));
    ctx.set_field_by_name(m, "name", Value::Object(None));
    // `get_or_create_app_loader` allocates (and can therefore move `m`), so
    // re-read the pin before the write. It is a Rust-side singleton with no
    // path back into `Class.getModule()`, so there is no re-entrancy here.
    let loader = crate::classloader::get_or_create_app_loader(ctx);
    // Adopting the JDK's application loader publishes ITS unnamed module as the canonical one
    // (`classloader::unify_app_unnamed_module`): the one minted above is then not the canonical one.
    if let Some(published) = ctx.get_cached_module_mirror(None) {
        ctx.unpin_native_roots(pin);
        return Ok(published);
    }
    let m = ctx.read_native_pin(pin, m);
    ctx.set_field_by_name(m, "loader", Value::Object(Some(loader?)));
    ctx.unpin_native_roots(pin);
    // Publish so `Class.getModule()`, `ClassLoader.getUnnamedModule()` and the
    // Package builders all observe one identity. The VM keeps the cached
    // mirror as a permanent GC root.
    ctx.cache_module_mirror(None, m);
    let m_pin = ctx.pin_native_root(m);
    grant_unnamed_native_access(ctx);
    let m = ctx.read_native_pin(m_pin, m);
    ctx.unpin_native_roots(m_pin);
    Ok(m)
}

/// SBR wave 5 (reflmeta): `--enable-native-access=ALL-UNNAMED` applied to the
/// REAL `Module` state, as `ModuleBootstrap.addEnableNativeAccess` applies it
/// (`JLA.addEnableNativeAccessToAllUnnamed()` ->
/// `Module.implAddEnableNativeAccessToAllUnnamed()`, which sets
/// `ALL_UNNAMED_MODULE.enableNativeAccess`). This VM never runs
/// `ModuleBootstrap.boot`, so the grant lived only in
/// `panama::NativeAccessPolicy`, which the native `Reflection.ensureNativeAccess`
/// consulted; the real `Reflection.ensureNativeAccess` ->
/// `Module.ensureNativeAccess` reads `moduleForNativeAccess()` -- for every
/// unnamed module the `ALL_UNNAMED_MODULE` static -- and warned despite the
/// flag. Run once per VM, from the first unnamed-module mint (a restricted
/// caller in an unnamed module reaches `Class.getModule()` before
/// `Module.ensureNativeAccess`). Both runtime modes; nothing without a grant.
pub fn grant_unnamed_native_access(ctx: &mut dyn NativeContext) {
    if !crate::panama::module_native_access_enabled(ctx, None) {
        return;
    }
    let Ok(module_class) = ctx.ensure_class_initialized("java/lang/Module") else {
        return;
    };
    let Some(idx) = ctx.static_field_index_by_name(module_class, "ALL_UNNAMED_MODULE") else {
        return;
    };
    if let Value::Object(Some(all_unnamed)) = ctx.get_static_field(module_class, idx) {
        ctx.set_field_by_name(all_unnamed, "enableNativeAccess", Value::Int(1));
    }
}

/// SBR wave 5 (reflmeta): does a NAMED module start with native access, as the
/// real `ModuleBootstrap.boot` leaves it? `java.base` always
/// (`JLA.addEnableNativeAccess(baseModule)`), the JDK's own
/// `ModuleLoaderMap.nativeAccessModules()` (JDK 25's list, transcribed from
/// `javap -c ModuleLoaderMap$Modules`; granting a name an older image lacks is
/// inert), and whatever `--enable-native-access` names. Read by the named
/// `Class.getModule()` mint, which writes the real `enableNativeAccess` field
/// so the real `Module.ensureNativeAccess` stays silent for the JDK's own
/// restricted callers exactly as on HotSpot.
pub fn module_starts_with_native_access(ctx: &dyn NativeContext, name: &str) -> bool {
    const JDK_NATIVE_ACCESS_MODULES: &[&str] = &[
        "java.base",
        "java.desktop",
        "java.instrument",
        "java.management",
        "java.prefs",
        "java.rmi",
        "java.security.jgss",
        "java.smartcardio",
        "jdk.accessibility",
        "jdk.attach",
        "jdk.compiler",
        "jdk.crypto.cryptoki",
        "jdk.crypto.mscapi",
        "jdk.hotspot.agent",
        "jdk.internal.le",
        "jdk.internal.vm.ci",
        "jdk.jdi",
        "jdk.jfr",
        "jdk.jpackage",
        "jdk.management",
        "jdk.management.agent",
        "jdk.net",
        "jdk.sctp",
        "jdk.security.auth",
    ];
    JDK_NATIVE_ACCESS_MODULES.contains(&name)
        || crate::panama::module_native_access_enabled(ctx, Some(name))
}

/// The unnamed `java.lang.Module` of `loader`.
///
/// HotSpot gives EVERY `ClassLoader` its own unnamed module -- that is what
/// `ClassLoader.getUnnamedModule()` returns and what `Class.getModule()`
/// reports for a class the loader defined -- and `Module.getClassLoader()`
/// answers that loader. CratonVM had ONE process-wide unnamed module whose
/// `loader` was always the application loader, so every caller-sensitive JDK
/// API that derives a `ClassLoader` from the *caller's module* looked in the
/// wrong place.
///
/// `ResourceBundle.getBundle(String)` is the canonical victim:
/// `getBundleImpl` resolves the search loader as
/// `getLoader(caller.getModule())`, so a class defined by a custom loader
/// searched the APPLICATION classpath for its bundle and threw
/// `MissingResourceException` even though its own loader could serve the
/// resource. Found on the Keycloak 26.6.1 boot -- Liquibase's
/// `ResourceBundle.getBundle("liquibase/i18n/liquibase-core")` runs from
/// classes defined by Quarkus's `RunnerClassLoader`, whose jars are not on
/// the CratonVM process classpath at all.
///
/// Built-in loaders keep the single [`canonical_unnamed_module`] so the
/// identity contract it documents (Mockito's `assureCanReadMockito`,
/// `Throwable.validateSuppressedExceptionsList`, the `java.lang.Package`
/// builders) is untouched; only genuinely user-defined loaders get their own.
pub fn unnamed_module_for_loader(
    ctx: &mut dyn NativeContext,
    loader: Option<ObjectRef>,
) -> Result<ObjectRef, MethodCallFailed> {
    let Some(loader) = loader else {
        return Ok(canonical_unnamed_module(ctx)?);
    };
    // The application loader keeps the ONE canonical unnamed module, so every
    // identity contract `canonical_unnamed_module` documents is untouched for
    // ordinary classpath classes.
    // gen r4w3/rooting: `get_or_create_app_loader` may allocate/initialise
    // (GC point); pin `loader` across it and compare/use the re-read address.
    let loader_pin = ctx.pin_native_root(loader);
    let app = crate::classloader::get_or_create_app_loader(ctx);
    let loader = ctx.read_native_pin(loader_pin, loader);
    let app = match app {
        Ok(app) => app,
        Err(e) => {
            ctx.unpin_native_roots(loader_pin);
            return Err(e);
        }
    };
    if app.as_ptr() == loader.as_ptr() {
        ctx.unpin_native_roots(loader_pin);
        return Ok(canonical_unnamed_module(ctx)?);
    }
    // Memoise on the loader's OWN `java.lang.ClassLoader.unnamedModule` field --
    // the JDK's own storage. Identity is then exact per loader, the module is
    // GC-rooted by the loader that owns it, and a real `ClassLoader` whose
    // constructor already built its unnamed module hands back THAT object.
    if let Value::Object(Some(existing)) = ctx.get_field_by_name(loader, "unnamedModule") {
        if ctx
            .class_name_arc_of_id(ctx.class_id_of_object(existing))
            .as_deref()
            == Some("java/lang/Module")
        {
            ctx.unpin_native_roots(loader_pin);
            return Ok(existing);
        }
    }
    let m = try_alloc_concurrent_synthetic(ctx, "java/lang/Module", 2)?;
    let pin = ctx.pin_native_root(m);
    // Unnamed: slot 0 AND the real `name` field stay null -- see
    // `canonical_unnamed_module` for why both shapes must be written.
    ctx.set_field(m, 0, Value::Object(None));
    ctx.set_field_by_name(m, "name", Value::Object(None));
    let loader = ctx.read_native_pin(loader_pin, loader);
    let m = ctx.read_native_pin(pin, m);
    ctx.set_field_by_name(m, "loader", Value::Object(Some(loader)));
    let m = ctx.read_native_pin(pin, m);
    let loader = ctx.read_native_pin(loader_pin, loader);
    ctx.set_field_by_name(loader, "unnamedModule", Value::Object(Some(m)));
    // `set_field_by_name` no-ops when the field is absent (the synthetic
    // ClassLoader shape). Without somewhere to memoise we would mint a fresh
    // Module per call and break identity outright, so degrade to the canonical
    // one instead -- exactly the pre-fix behaviour for those loaders.
    let stored = match ctx.get_field_by_name(loader, "unnamedModule") {
        Value::Object(Some(x)) => x.as_ptr() == m.as_ptr(),
        _ => false,
    };
    ctx.unpin_native_roots(loader_pin);
    if stored {
        Ok(m)
    } else {
        canonical_unnamed_module(ctx)
    }
}

/// Internal helper: synthesise a `java/lang/Package` whose `name` slot is
/// set to the requested package name (dotted). Mirrors the layout used by
/// `native_class_get_package` so callers that subsequently invoke
/// `Package.getName()` see the right value.
pub fn i2_alloc_synthetic_package(
    ctx: &mut dyn NativeContext,
    name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let pkg = try_alloc_concurrent_synthetic(ctx, "java/lang/Package", 12)?;
    let pkg_pin = ctx.pin_native_root(pkg);
    let name_str = ctx.create_string(name);
    let pkg = ctx.read_native_pin(pkg_pin, pkg);
    ctx.set_field(pkg, 0, Value::Object(Some(name_str)));
    ctx.set_field_by_name(pkg, "name", Value::Object(Some(name_str)));
    // Always wire a non-null `module`. Real `Package.getPackageInfo()` derefs
    // it unconditionally, so a module-less Package is a latent NPE for every
    // `getAnnotation` / `getDeclaredAnnotations` / `isAnnotationPresent` caller
    // -- see `canonical_unnamed_module` for the TestNG-discovery blowup this
    // caused. Callers that KNOW the real module (`definePackage(String,Module)`,
    // `getNamedPackage`) overwrite this immediately after.
    let module = canonical_unnamed_module(ctx);
    let pkg = ctx.read_native_pin(pkg_pin, pkg);
    ctx.set_field_by_name(pkg, "module", Value::Object(Some(module?)));
    // A package of a NAMED module (a platform or `--module-path` module, not a
    // class-path jar) is in that module, as HotSpot's `Package(name, module)`
    // is, so `Package.isSealed()` (`module().isNamed()`) is `true` for
    // `java.lang` (SBR3 `refmisc`: the JDK's own `Class.getPackage` reaches
    // this producer for a boot class).
    if let Some(named) = ctx
        .module_for_package(&name.replace('.', "/"))
        .filter(|m| !ctx.module_is_class_path_only(m) && !ctx.module_is_automatic(m))
    {
        if let Some(mirror) = ctx.get_cached_module_mirror(Some(&named)) {
            let pkg = ctx.read_native_pin(pkg_pin, pkg);
            ctx.set_field_by_name(pkg, "module", Value::Object(Some(mirror)));
        }
    }
    // Same reasoning for `versionInfo`: the real `Package(String, Module)`
    // constructor always stores `Package$VersionInfo.NULL_VERSION_INFO`, and
    // `getSpecificationTitle()` / `getImplementationVersion()` / `isSealed()`
    // all deref it without a null check. `native_class_get_package` already
    // does this; the ClassLoader-side builders did not, so a Package reached
    // through `Package.getPackage(String)` NPEd on those accessors.
    if let Ok(vi_cid) = ctx.ensure_class_initialized("java/lang/Package$VersionInfo") {
        if let Some(idx) = ctx.static_field_index_by_name(vi_cid, "NULL_VERSION_INFO") {
            let null_version_info = ctx.get_static_field(vi_cid, idx);
            if matches!(null_version_info, Value::Object(Some(_))) {
                let pkg = ctx.read_native_pin(pkg_pin, pkg);
                ctx.set_field_by_name(pkg, "versionInfo", null_version_info);
            }
        }
    }
    let pkg = ctx.read_native_pin(pkg_pin, pkg);
    ctx.unpin_native_roots(pkg_pin);
    Ok(pkg)
}

/// JDK 25 `ClassLoader.getNamedPackage`'s body against `loader`'s own
/// `packages` map: the entry for `pkg_name`, else a new
/// `NamedPackage(pkg_name, module)` put if absent (the winner of a race is
/// returned). `None` when the map is null or a Java call fails (the caller
/// keeps its synthesised answer). Every object read across a Java call is
/// pinned; all pins are released together.
pub fn jdk_only_named_package_in_map(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    pkg_name: &str,
    module: Option<ObjectRef>,
) -> Option<ObjectRef> {
    let map = match classloader_packages_field(ctx, loader) {
        Value::Object(Some(map)) => map,
        _ => return None,
    };
    let map_pin = ctx.pin_native_root(map);
    let module_pin = module.map(|m| ctx.pin_native_root(m));
    let found = named_package_get_or_put(ctx, (map_pin, map), pkg_name, module_pin.zip(module));
    ctx.unpin_native_roots(map_pin);
    found
}

/// The body of [`jdk_only_named_package_in_map`]; the caller releases the pins.
pub fn named_package_get_or_put(
    ctx: &mut dyn NativeContext,
    (map_pin, map): (usize, ObjectRef),
    pkg_name: &str,
    module: Option<(usize, ObjectRef)>,
) -> Option<ObjectRef> {
    let key = ctx.create_string_uninterned(pkg_name);
    let key_pin = ctx.pin_native_root(key);
    let map_now = ctx.read_native_pin(map_pin, map);
    let key = ctx.read_native_pin(key_pin, key);
    match ctx.invoke_virtual(
        map_now,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(key))],
    ) {
        Ok(Some(Value::Object(Some(existing)))) => return Some(existing),
        Ok(_) => {}
        Err(_) => return None,
    }
    let key = ctx.read_native_pin(key_pin, key);
    let module_now = match module {
        Some((pin, m)) => Value::Object(Some(ctx.read_native_pin(pin, m))),
        None => Value::Object(None),
    };
    let np = match ctx.new_object_initialized(
        "java/lang/NamedPackage",
        "(Ljava/lang/String;Ljava/lang/Module;)V",
        &[Value::Object(Some(key)), module_now],
    ) {
        Ok(Some(Value::Object(Some(np)))) => np,
        _ => return None,
    };
    let np_pin = ctx.pin_native_root(np);
    let key = ctx.read_native_pin(key_pin, key);
    let map_now = ctx.read_native_pin(map_pin, map);
    let np = ctx.read_native_pin(np_pin, np);
    let prior = ctx.invoke_virtual(
        map_now,
        "putIfAbsent",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(key)), Value::Object(Some(np))],
    );
    let np = ctx.read_native_pin(np_pin, np);
    match prior {
        Ok(Some(Value::Object(Some(winner)))) => Some(winner),
        Ok(_) => Some(np),
        Err(_) => None,
    }
}

/// `ClassLoader.getNamedPackage(String packageName, Module m) -> NamedPackage`
/// (private, called from `postDefineClass`). The JDK implementation reads
/// the `packages` map and inserts a new `NamedPackage` if absent; we
/// synthesise a fresh one without ever touching `packages`. Matches
/// `Package` (which extends `NamedPackage`) so the cast at the call site
/// is benign вЂ” callers ignore the result anyway (the value is `pop`ped
/// in `postDefineClass`).
pub fn i2_classloader_get_named_package(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let in_map = ctx.is_jdk_only();
    get_named_package(ctx, args, in_map)
}

/// [`i2_classloader_get_named_package`] with the loader's `packages` map in
/// both modes: what `i2_register_classloader_package_natives` registers under
/// `--compatible` with `CRATONVM_COMPAT_NAMED_PACKAGES` (interpreter round i1
/// wave 49, lane L6;
/// `interpreter-L6-compatible-get-named-package-does-not-fill-the-loaders-package-map-FIXED-20261013.md`).
pub fn i2_classloader_get_named_package_in_map(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    get_named_package(ctx, args, true)
}

pub fn get_named_package(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    in_map: bool,
) -> MethodCallResult {
    let pkg_name = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    // `--real-jdk`: do what the JDK's body does when the loader's `packages`
    // map exists -- get, else put a `NamedPackage` -- so a class definition
    // defines its package in the loader (`postDefineClass`), and a later
    // `definePackage` of it throws `IllegalArgumentException`, and
    // `getDefinedPackages` lists it (interpreter round i1 wave 42 follow-up,
    // lane L5; `L5W42LoaderSurface`, `define-package-after-class`,
    // `defined-packages`). A loader whose map is null (the ByteBuddy loader
    // this override was written for) keeps the synthesised answer below.
    // `--compatible` too under `CRATONVM_COMPAT_NAMED_PACKAGES` (`in_map`).
    if in_map {
        if let (Some(Value::Object(Some(loader))), Some(module)) = (args.first(), args.get(2)) {
            let module = match module {
                Value::Object(m) => *m,
                _ => None,
            };
            if let Some(named) = jdk_only_named_package_in_map(ctx, *loader, &pkg_name, module) {
                return Ok(Some(Value::Object(Some(named))));
            }
        }
    }
    // gen r4w3/rooting: `i2_alloc_synthetic_package` can run <clinit> (GC
    // point) and `args[2]` is not refreshed by it; pin the module across it.
    let module_arg = args.get(2).copied();
    let module_pin = match module_arg {
        Some(Value::Object(Some(m))) => Some((ctx.pin_native_root(m), m)),
        _ => None,
    };
    let pkg = i2_alloc_synthetic_package(ctx, &pkg_name);
    let module_arg = match module_pin {
        Some((pin, m)) => {
            let m = ctx.read_native_pin(pin, m);
            ctx.unpin_native_roots(pin);
            Some(Value::Object(Some(m)))
        }
        None => module_arg,
    };
    let pkg = pkg?;
    let pkg_pin = ctx.pin_native_root(pkg);
    // Persist the module reference too so `Package.module()` returns the
    // caller-supplied module if it does get queried later.
    if let Some(module_val) = module_arg {
        ctx.set_field_by_name(pkg, "module", module_val);
    }
    ctx.unpin_native_roots(pkg_pin);
    Ok(Some(Value::Object(Some(pkg))))
}

/// `ClassLoader.definePackage(String name, Module m) -> Package` вЂ” same
/// shape as `getNamedPackage` but typed as `Package`. The JDK's bytecode
/// also reads the `packages` map; we bypass it entirely.
pub fn i2_classloader_define_package_string_module(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let pkg_name = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    // gen r4w3/rooting: as in `i2_classloader_get_named_package` -- pin the
    // `args[2]` module across the allocating/initialising helper.
    let module_arg = args.get(2).copied();
    let module_pin = match module_arg {
        Some(Value::Object(Some(m))) => Some((ctx.pin_native_root(m), m)),
        _ => None,
    };
    // The JDK's body answers the `Package` the loader already holds for the
    // name (`packages.get(name) instanceof Package`), and this native minted a
    // fresh one per call: `ClassLoader.packages()` maps every entry through
    // it, so `Package.getPackages()`'s unnamed package was not the object
    // `Class.getPackage()` answers (HotSpot: the same; probe
    // `L5W55UnnamedModulePackages`, `all-*`). Answer `getDefinedPackage`'s
    // object first -- the map's, else the memo's -- under
    // `CRATONVM_BUILTIN_LOADER_PACKAGE_SYNC` (interpreter round i2 wave 55,
    // lane L5).
    if ctx.builtin_loader_package_sync() {
        if let (Some(Value::Object(Some(loader))), Some(Value::Object(Some(name)))) =
            (args.first(), args.get(1))
        {
            if let Ok(Some(Value::Object(Some(pkg)))) = i2_classloader_get_defined_package(
                ctx,
                &[Value::Object(Some(*loader)), Value::Object(Some(*name))],
            ) {
                if let Some((pin, _)) = module_pin {
                    ctx.unpin_native_roots(pin);
                }
                return Ok(Some(Value::Object(Some(pkg))));
            }
        }
    }
    let pkg = i2_alloc_synthetic_package(ctx, &pkg_name);
    let module_arg = match module_pin {
        Some((pin, m)) => {
            let m = ctx.read_native_pin(pin, m);
            ctx.unpin_native_roots(pin);
            Some(Value::Object(Some(m)))
        }
        None => module_arg,
    };
    let pkg = pkg?;
    let pkg_pin = ctx.pin_native_root(pkg);
    if let Some(module_val) = module_arg {
        ctx.set_field_by_name(pkg, "module", module_val);
    }
    ctx.unpin_native_roots(pkg_pin);
    Ok(Some(Value::Object(Some(pkg))))
}

/// `ClassLoader.definePackage(Class<?>) -> Package` вЂ” derives the package
/// name from `c.getPackageName()` and synthesises a Package. This is the
/// single-arg overload called by `Class.getPackage()`'s JDK Java body
/// when the native override (registered separately for `Class.getPackage`)
/// isn't taken.
///
/// Also populates the manifest-derived fields (`implementationTitle`,
/// `implementationVersion`, `specification*`, `implementationVendor`) from
/// the class's source jar вЂ” same data path as `native_class_get_package`.
/// Without this, callers like `Foo.class.getPackage().getImplementationVersion()`
/// in real-JDK mode see slot 5 = null (because real `Class.getPackage()`
/// delegates here), even though the synthetic-jdk override correctly
/// populated those fields. This caused Spring Boot 2.x to NPE in
/// `SpringBootVersion.determineSpringBootVersion()` on the JarURLConnection
/// fallback path.
pub fn i2_classloader_define_package_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = this (ClassLoader), args[1] = c (Class)
    let class_arg = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // SBR2 wave 9 (modload), `--real-jdk`: `Class.getPackage()` runs the JDK's
    // own body (`cl.definePackage(this)`), which lands HERE. The one `Package`
    // per (loader, package) -- manifest attributes, `packageInfo` and module
    // included -- is `native_class_get_package`'s memo, shared with
    // `getDefinedPackage`; minting a fresh object per call below made two
    // `getPackage()` calls on one class disagree (the wave-8 trial finding).
    if ctx.is_jdk_only() {
        // The loader's OWN `Package` for the name comes first (SBR3 `refmisc`):
        // when the JDK's `defineOrCheckPackage` defined it from the jar's
        // manifest (a `Sealed: true` jar's package is sealed, with its
        // implementation attributes), `getDefinedPackage` already answers that
        // object, and `getPackage()` must agree with it, as HotSpot's one
        // interned `Package` per (loader, name) does.
        if let (Some(Value::Object(Some(loader))), Some(class_id)) =
            (args.first(), mirror_class_id(ctx, class_arg))
        {
            let class_name = mirror_class_name(ctx, class_arg).unwrap_or_default();
            if !class_name.is_empty() && !class_name.starts_with('[') {
                let pkg_name = package_name_of(ctx.vm_identity(), class_id, &class_name);
                // The memo's namespace is the loader OBJECT's (`getDefinedPackage`
                // keys it so; a platform class is tagged bootstrap, `0`).
                let ns = crate::classloader::loader_namespace_id(ctx, *loader);
                let memo_key = (ctx.vm_identity(), ns, pkg_name.to_string());
                // A FULLY BUILT memo row is the answer, as it is for
                // `native_class_get_package` (one hash lookup, no Java call).
                if let Some(pkg) = defined_package_memo(crate::classloader::loader_tables(ctx))
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&memo_key)
                    .filter(|row| row.rich)
                    .map(|row| row.pkg)
                {
                    return Ok(Some(Value::Object(Some(pkg))));
                }
                let loader = *loader;
                let pin = ctx.pin_native_root(loader);
                let real = real_defined_package(ctx, Some(loader), &pkg_name);
                ctx.unpin_native_roots(pin);
                if let Some(RealPackageEntry::Package(pkg)) = real {
                    // Remember it, so the next call is the memo hit above.
                    if ns != 0 {
                        let owner = defined_package_owner(ctx, ns, None);
                        let tables = crate::classloader::loader_tables(ctx);
                        publish_defined_package(tables, memo_key, pkg, true, owner);
                    }
                    return Ok(Some(Value::Object(Some(pkg))));
                }
            }
        }
        return native_class_get_package(ctx, &[Value::Object(Some(class_arg))]);
    }
    let class_name = mirror_class_name(ctx, class_arg).unwrap_or_default();
    if class_name.is_empty() || class_name.starts_with('[') {
        return Ok(Some(Value::Object(None)));
    }
    // Cache the dotted package prefix per `ClassId` for VM-registered
    // mirrors only вЂ” same rationale as `native_class_get_package`.
    let pkg_name: Arc<str> = if let Some(class_id) = ctx.class_id_from_mirror(class_arg) {
        if let Some(arc) = cache_get(&PACKAGE_NAME_CACHE, ctx.vm_identity(), class_id) {
            arc        } else {
            package_name_of(ctx.vm_identity(), class_id, &class_name)        }    } else if let Some(pos) = class_name.rfind('/') {
        Arc::from(class_name[..pos].replace('/', "."))    } else {
        Arc::from("")
    };
    // Read manifest attributes (best-effort) from the class's source jar.
    let (impl_title, impl_version, spec_title, spec_version, spec_vendor, impl_vendor, seal_base) =
        if let Some(class_id) = mirror_class_id(ctx, class_arg) {
            (
                t19_h10_class_manifest_attr(ctx, class_id, "Implementation-Title"),
                t19_h10_class_manifest_attr(ctx, class_id, "Implementation-Version"),
                t19_h10_class_manifest_attr(ctx, class_id, "Specification-Title"),
                t19_h10_class_manifest_attr(ctx, class_id, "Specification-Version"),
                t19_h10_class_manifest_attr(ctx, class_id, "Specification-Vendor"),
                t19_h10_class_manifest_attr(ctx, class_id, "Implementation-Vendor"),
                package_seal_base(ctx, class_id),
            )
        } else {
            (None, None, None, None, None, None, None)        };
    let pkg = i2_alloc_synthetic_package(ctx, &pkg_name)?;
    // By-name only вЂ” NOT by raw slot index. See the corruption note on
    // `native_class_get_package` above: real JDK 9+ `Package` has no flat
    // `specTitle`/etc. fields at slots 1-6 (those live inside the nested
    // `versionInfo` object; slots 1-3 are actually `module`/`versionInfo`/
    // `packageInfo`), so a by-index write here clobbers them exactly like
    // the `native_class_get_package` bug did.
    let pkg_pin = ctx.pin_native_root(pkg);
    if let Ok(Some(version_info)) = package_version_info(
        ctx,
        spec_title,
        spec_version,
        spec_vendor,
        impl_title,
        impl_version,
        impl_vendor,
        seal_base,
    ) {
        let pkg = ctx.read_native_pin(pkg_pin, pkg);
        ctx.set_field(pkg, 2, version_info);
    } else if let Ok(vi_cid) = ctx.ensure_class_initialized("java/lang/Package$VersionInfo") {
        if let Some(idx) = ctx.static_field_index_by_name(vi_cid, "NULL_VERSION_INFO") {
            let null_version_info = ctx.get_static_field(vi_cid, idx);
            if matches!(null_version_info, Value::Object(Some(_))) {
                let pkg = ctx.read_native_pin(pkg_pin, pkg);
                ctx.set_field(pkg, 2, null_version_info);
            }
        }
    }
    let pkg = ctx.read_native_pin(pkg_pin, pkg);
    ctx.unpin_native_roots(pkg_pin);
    Ok(Some(Value::Object(Some(pkg))))
}

pub fn package_name_from_package_obj(ctx: &dyn NativeContext, pkg: ObjectRef) -> Option<String> {
    match ctx.get_field(pkg, 0) {
        Value::Object(Some(name_obj)) => ctx.read_string(name_obj),
        _ => None,
    }
}

pub fn java_string_hash(s: &str) -> i32 {
    let mut h = 0i32;
    for ch in s.encode_utf16() {
        h = h.wrapping_mul(31).wrapping_add(ch as i32);
    }
    h
}

pub fn native_package_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    if this.as_ptr() == other.as_ptr() {
        return Ok(Some(Value::Int(1)));
    }
    // `equals(Object)` takes ANY object: without this class check a non-Package
    // argument whose slot 0 happens to hold a String equal to our package name
    // would compare equal. (This check lived only in a second, duplicate
    // `Package.equals` registration that the registry's last-registration-wins
    // rule silently shadowed with this body -- see the registration site.)
    let other_cid = ctx.class_id_of_object(other);
    if ctx.class_name_of_id(other_cid).unwrap_or_default() != "java/lang/Package" {
        return Ok(Some(Value::Int(0)));
    }
    let this_name = package_name_from_package_obj(ctx, this);
    let other_name = package_name_from_package_obj(ctx, other);
    // Two LOADERS' same-named packages are two packages: HotSpot's `equals`
    // is identity over one `Package` per (loader, name), so they are unequal.
    // A package's module is its loader's (a user loader's unnamed module is
    // its own, `getUnnamedModule`), so same name AND same module is "the same
    // package" without trusting identity to the VM's own minting. A package
    // minted without a module keeps the by-name answer
    // (interpreter round i2 wave 53, lane L5; probe `L5W53PackageIdentity`,
    // `cross`).
    let same_module = match (
        ctx.get_field_by_name(this, "module"),
        ctx.get_field_by_name(other, "module"),
    ) {
        (Value::Object(Some(a)), Value::Object(Some(b))) => a.as_ptr() == b.as_ptr(),
        // A side minted without a module, or a layout without the field:
        // the old by-name answer.
        _ => true,
    };
    Ok(Some(Value::Int(
        (this_name.is_some() && this_name == other_name && same_module) as i32,
    )))
}

pub fn native_package_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let hash = package_name_from_package_obj(ctx, this)
        .map(|name| java_string_hash(&name))
        .unwrap_or(0);
    Ok(Some(Value::Int(hash)))
}

/// Wire up I2 ClassLoader package overrides. Invoked from
/// `lang_invoke::register_t28_method_handle_completeness` so they land in
/// both real-JDK and synthetic-jdk registration paths without any
/// edits to `lib.rs` / `vm_init.rs` (per I2 surface rules).
pub fn i2_register_classloader_package_natives(r: &mut cratonvm_native_api::NativeMethodRegistry) {
    let cl = "java/lang/ClassLoader";
    r.register(
        cl,
        "getDefinedPackage",
        "(Ljava/lang/String;)Ljava/lang/Package;",
        i2_classloader_get_defined_package,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cl,
        "getDefinedPackages",
        "()[Ljava/lang/Package;",
        i2_classloader_get_defined_packages, cratonvm_native_api::NativeKind::Bridge,
    );
    // `ClassLoader.packages()`: a reviewed `Intrinsic`, stated here rather
    // than inherited (AGENTS.md), over a real JDK with
    // `CRATONVM_BUILTIN_LOADER_PACKAGE_SYNC` (interpreter round i2 wave 55,
    // lane L5). It fills a built-in loader's `packages` map with the packages
    // of the classes the VM defined for it and then runs the JDK's body, so an
    // unnamed module's `Module.getPackages()` lists them as on HotSpot.
    if r.builtin_loader_package_sync() {
        r.register(
            cl,
            "packages",
            "()Ljava/util/stream/Stream;",
            classloader_packages_synced,
            cratonvm_native_api::NativeKind::Intrinsic,
        );
    }
    // `Package.equals(Object)` -- real JDK has NO override (inherits identity
    // `Object.equals`), which is correct there because HotSpot INTERNS one
    // `Package` instance per (loader, package name): every `Class.getPackage()`
    // call for the same package returns the SAME object, so identity equality
    // is suffient. Our `native_class_get_package` (and the sibling package-
    // mirror builders) allocate a FRESH synthetic `Package` object on every
    // call -- no interning -- so two `Package`s for the literal same package
    // name are never `==`, and since `equals` falls back to identity, they
    // also never `.equals()`. Spring's `HandlerMethodValidationException`
    // test helper `MvcParamPredicate.hasMvcAnnotation` (and any other code
    // doing `someAnnotation.annotationType().getPackage().equals(SomeOther
    // Annotation.class.getPackage())`) depends on same-named packages
    // comparing equal, so this silently misclassified annotations by
    // declaring package.
    //
    // Fix: override `equals` to compare by package name (`Package.hashCode()`
    // already runs real bytecode hashing `getName()`, so this keeps the
    // equals/hashCode contract intact). Two different loaders' same-named
    // packages compared equal here, where HotSpot's interned-per-loader
    // objects do not; since interpreter round i2 wave 53 (lane L5) the name
    // must come with the same module, i.e. the same loader
    // (`native_package_equals`).
    //
    // Registered ONCE. There used to be two `Package.equals` registrations
    // here -- an inline closure with the class check, immediately followed by
    // `native_package_equals` without it. `NativeMethodRegistry::register` is
    // last-registration-wins, so the closure (and its class check) was dead
    // code from the moment it was written. The check now lives in
    // `native_package_equals` itself; do not re-add a second registration.
    r.register(
        "java/lang/Package",
        "equals",
        "(Ljava/lang/Object;)Z",
        native_package_equals, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "java/lang/Package",
        "hashCode",
        "()I",
        native_package_hash_code, cratonvm_native_api::NativeKind::Bridge,
    );
    // `--compatible` fills the loader's `packages` map as `--real-jdk` does,
    // under `CRATONVM_COMPAT_NAMED_PACKAGES`, read once here, per VM
    // (interpreter round i1 wave 49, lane L6;
    // `interpreter-L6-compatible-get-named-package-does-not-fill-the-loaders-package-map-FIXED-20261013.md`).
    let named_package_in_map: cratonvm_native_api::NativeCallback =
        // The HotSpot-alignment row `named-packages` since interpreter round
        // i6 wave 118, lane L5 (`CRATONVM_COMPAT_NAMED_PACKAGES` its legacy
        // key; in force under `--real-jdk`).
        if r.aligned(cratonvm_types::HotSpotAlignment::CompatNamedPackages) {
            i2_classloader_get_named_package_in_map
        } else {
            i2_classloader_get_named_package
        };
    r.register(
        cl,
        "getNamedPackage",
        "(Ljava/lang/String;Ljava/lang/Module;)Ljava/lang/NamedPackage;",
        named_package_in_map, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cl,
        "definePackage",
        "(Ljava/lang/String;Ljava/lang/Module;)Ljava/lang/Package;",
        i2_classloader_define_package_string_module, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cl,
        "definePackage",
        "(Ljava/lang/Class;)Ljava/lang/Package;",
        i2_classloader_define_package_class, cratonvm_native_api::NativeKind::Bridge,
    );
    // I2: see `i2_classloader_check_certs` doc-comment.
    r.register(
        cl,
        "checkCerts",
        "(Ljava/lang/String;Ljava/security/CodeSource;)V",
        i2_classloader_check_certs, cratonvm_native_api::NativeKind::Bridge,
    );
}

/// `ClassLoader.checkCerts(String, CodeSource)` (private) вЂ” no-op. The JDK's
/// implementation reads the `package2certs: ConcurrentHashMap` field which
/// also stays null on user-instantiated subclasses, NPE-ing inside
/// `preDefineClass`. We skip the check entirely (we don't enforce
/// signer-coherency anyway).
pub fn i2_classloader_check_certs(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
}

/// `Class.isSynthetic()` — `(getModifiers() & ACC_SYNTHETIC) != 0`, which is
/// verbatim how `java.lang.Class` defines it. Was `native_return_false`, so
/// under `synthetic-jdk` NOTHING was ever synthetic — most visibly a lambda's
/// hidden class, which Spring's `ClassUtils.isLambdaClass()` gates on.
///
/// Delegating to `native_class_get_modifiers` rather than reading
/// `class_access_flags` directly is deliberate: that native already models
/// every case HotSpot's `JVM_GetClassModifiers` does — the
/// `ACC_FINAL|ACC_SYNTHETIC` synthesis for lambda proxy classes (which carry
/// no class-store access flags of their own), the JVMS §4.7.6 substitution of
/// the `InnerClasses` entry's flags for a nested class, and the primitive and
/// array forms — so this cannot drift from it. Note both of those synthesised
/// forms deliberately exclude ACC_SYNTHETIC for primitives and arrays, which
/// matches HotSpot: an array of a synthetic class is not itself synthetic.
pub fn native_class_is_synthetic(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    const ACC_SYNTHETIC: i32 = 0x1000;
    match native_class_get_modifiers(ctx, args)? {
        Some(Value::Int(modifiers)) => Ok(Some(Value::Int(i32::from(
            (modifiers & ACC_SYNTHETIC) != 0,
        )))),
        _ => Ok(Some(Value::Int(0))),
    }
}

/// JVMS nesting kind of a class, read off its own `InnerClasses` entry (see
/// `own_inner_class_entry`). This is the classification behind
/// `Class.isAnonymousClass()` / `isLocalClass()` / `isMemberClass()`, which
/// are mutually exclusive: at most one of the three is ever true.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NestingKind {
    /// Not nested at all — a genuine top-level class, an array, a primitive,
    /// or any class with no `InnerClasses` entry of its own (a
    /// dynamically-generated proxy, a lambda hidden class). All three
    /// predicates are false.
    TopLevel,
    /// `inner_name_index == 0` — anonymous (`new Runnable() {...}`, and also
    /// an enum constant's class body).
    Anonymous,
    /// Named, but `outer_class_info_index == 0` — local, i.e. declared inside
    /// a method/constructor/initializer.
    Local,
    /// Both set — a member (nested) class.
    Member,
}

/// Classify `class_id`. Arrays and primitives are never
/// anonymous/local/member: HotSpot reports `false` for all three on
/// `Member[].class` and `Anon[].class` even though the COMPONENT type is a
/// member/anonymous class, because `Class.isLocalOrAnonymousClass()` and
/// `getDeclaringClass0()` consult attributes an array class does not have.
pub fn class_nesting_kind(ctx: &mut dyn NativeContext, class_id: ClassId) -> NestingKind {
    let Some(name) = ctx.class_name_of_id(class_id) else {
        return NestingKind::TopLevel;
    };
    if name.starts_with('[') {
        return NestingKind::TopLevel;
    }
    class_nesting_of(ctx, class_id, &name).0
}

/// The nesting kind of a NON-array class `name`, and its own `InnerClasses`
/// row `(outer class, simple name)` (the first self row,
/// `own_inner_class_entry`). The one decision behind the `--compatible`
/// nesting predicates, `getSimpleName` ([`resolve_simple_name`]) and
/// `getCanonicalName` ([`canonical_class_name`]).
///
/// JIT round 21 wave 19, lane innerclasses
/// (`CRATONVM_COMPAT_NESTING_FOLLOWS_ENCLOSING_METHOD`; page
/// `r21-review-w18-findings-FIXED-20261020.md` item 3), as JDK 25 decides:
/// - `isLocalOrAnonymousClass` is `hasEnclosingMethodInfo`, `EnclosingMethod`
///   alone. So `EnclosingMethod` with no self row is ANONYMOUS
///   (`getSimpleBinaryName0` is null): simple name `""`, canonical null.
/// - A self row with no outer class and no `EnclosingMethod` is TOP LEVEL
///   (`isTopLevelClass`): its simple name is never read, so `getSimpleName`
///   is the binary-name tail and `getCanonicalName` is `getName()`.
pub fn class_nesting_of(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    name: &str,
) -> (NestingKind, Option<(String, String)>) {
    let row = own_inner_class_entry(ctx, class_id, name);
    let any_outer_row = declaring_class_any_outer_row_enabled(&*ctx);
    let em_decides = nesting_follows_enclosing_method_enabled(&*ctx);
    let has_enclosing_method =
        (any_outer_row || em_decides) && ctx.enclosing_method(class_id).is_some();
    let kind = match &row {
        Some((outer_class, inner_name)) => row_nesting_kind(
            outer_class.is_empty(),
            inner_name.is_empty(),
            has_enclosing_method,
            any_outer_row,
            em_decides,
        ),
        None => no_row_nesting_kind(has_enclosing_method, em_decides),
    };
    (kind, row)
}

/// [`class_nesting_of`] for a class with no `InnerClasses` row of its own:
/// anonymous when it has `EnclosingMethod` and `em_decides`
/// (`CRATONVM_COMPAT_NESTING_FOLLOWS_ENCLOSING_METHOD`), else top level.
pub fn no_row_nesting_kind(has_enclosing_method: bool, em_decides: bool) -> NestingKind {
    if em_decides && has_enclosing_method {
        NestingKind::Anonymous
    } else {
        NestingKind::TopLevel
    }
}

/// [`class_nesting_kind`] of a class with an `InnerClasses` row of its own.
///
/// With `any_outer_row` (`CRATONVM_DECLARING_CLASS_ANY_OUTER_ROW`, JIT round 21
/// wave 16, lane innerclasses) it is HotSpot's answer:
/// - `isLocalOrAnonymousClass` is `hasEnclosingMethodInfo`. With
///   `EnclosingMethod` the class is anonymous without a simple name and local
///   with one, whatever its outer class.
/// - Without `EnclosingMethod`, an outer class makes it a member, named or not
///   (`isMemberClass` = `!isLocalOrAnonymousClass() && getDeclaringClass0() !=
///   null`; [`declaring_member_row_outer`]).
/// - With neither: TOP LEVEL with `em_decides` (wave 19,
///   `CRATONVM_COMPAT_NESTING_FOLLOWS_ENCLOSING_METHOD`: `isTopLevelClass`).
///   Without it, wave 16's reading: the row's own shape decides, as javac
///   writes it (an anonymous or local row always comes with `EnclosingMethod`).
///
/// Off, the old row-shape rule.
pub fn row_nesting_kind(
    outer_empty: bool,
    name_empty: bool,
    has_enclosing_method: bool,
    any_outer_row: bool,
    em_decides: bool,
) -> NestingKind {
    if any_outer_row {
        if has_enclosing_method {
            return if name_empty { NestingKind::Anonymous } else { NestingKind::Local };
        }
        if !outer_empty {
            return NestingKind::Member;
        }
        if em_decides {
            return NestingKind::TopLevel;
        }
    }
    if name_empty {
        NestingKind::Anonymous
    } else if outer_empty {
        NestingKind::Local
    } else {
        NestingKind::Member
    }
}

/// Shared body of the three nesting predicates: `true` iff the receiver's
/// nesting kind is exactly `want`.
pub fn class_nesting_predicate(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    want: NestingKind,
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let Some(class_id) = mirror_class_id(ctx, this) else {
        return Ok(Some(Value::Int(0)));
    };
    let matches = class_nesting_kind(ctx, class_id) == want;
    // `isMemberClass` is `getDeclaringClass0() != null` for a member class,
    // which asks the declaring class's `InnerClasses` to agree (interpreter
    // round i2 wave 59, lane L3; `CRATONVM_INNER_CLASSES_AGREEMENT`).
    if matches && want == NestingKind::Member {
        if let Some(refusal) = member_class_inner_classes_disagreement(ctx, class_id) {
            return Err(refusal);
        }
    }
    Ok(Some(Value::Int(i32::from(matches))))
}

/// `Class.isAnonymousClass()`. Was hardcoded to `false` in
/// `register_synthetic_overrides`, so under the `synthetic-jdk` feature every
/// anonymous class claimed to be a normal named one.
pub fn native_class_is_anonymous_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    class_nesting_predicate(ctx, args, NestingKind::Anonymous)
}

/// `Class.isLocalClass()`. Was hardcoded to `false` — see
/// `native_class_is_anonymous_class`.
pub fn native_class_is_local_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    class_nesting_predicate(ctx, args, NestingKind::Local)
}

/// `Class.isMemberClass()`. Was hardcoded to `false`, which is the most
/// visible of the three: every nested class in the program reported `false`.
pub fn native_class_is_member_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    class_nesting_predicate(ctx, args, NestingKind::Member)
}

pub fn native_class_is_enum(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(if class_is_declared_enum(ctx, class_id) {
        1
    } else {
        0
    })))
}

pub fn class_is_declared_enum(ctx: &dyn NativeContext, class_id: ClassId) -> bool {
    if (ctx.class_access_flags(class_id) & ACC_ENUM) == 0 {
        return false;
    }
    let Some(parent_id) = ctx.superclass_of(class_id) else {
        return false;
    };
    if let Some(enum_id) = ctx.class_id_by_name("java/lang/Enum") {
        return parent_id == enum_id;
    }
    matches!(
        ctx.class_name_arc_of_id(parent_id).as_deref(),
        Some("java/lang/Enum")
    )
}

pub fn native_class_get_canonical_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // JIT round 17 wave 8, lane mh7: the bridge `--compatible` dispatches
    // raises what the JDK body's `getDeclaringClass0` raises.
    // JIT round 21 wave 21, lane innerclasses (item 7): `getCanonicalName0`
    // answers null for `isHidden() || isLocalOrAnonymousClass()` before it
    // reaches `getEnclosingClass` ([`name_bridge_asks_declaring_class`]).
    //
    // JIT round 23 wave 1, lane L6 (REVIEW22-2): a class whose checks once
    // passed skips them ([`name_bridge_verdict_probe`]).
    let verdict = name_bridge_verdict_probe(ctx, this, true);
    // The memo skips only the checks below; the inner-classes agreement check
    // further down is never cached, so it re-asks when the memo hit.
    let mut asks_declaring_known = None;
    if !verdict.passed {
        let asks_declaring = name_bridge_asks_declaring_class(ctx, this, true);
        asks_declaring_known = Some(asks_declaring);
        if asks_declaring {
            if let Some(icce) = inner_classes_icce_for_mirror(ctx, this) {
                return Err(icce);
            }
        } else if let Some(failed) =
            name_bridge_enclosing_method_error(ctx, this, true, &CANONICAL_CLASS_NAME_CACHE)
        {
            // Wave 22, lane hidden-resid (item 1): after `isHidden()`,
            // `isLocalOrAnonymousClass` resolves the EnclosingMethod class.
            return Err(failed);
        }
        // JIT round 18 wave 11, lane mhresid (MHRESID18-3).
        if asks_declaring {
            if let Some(failed) =
                name_bridge_outer_resolution_error(ctx, this, &CANONICAL_CLASS_NAME_CACHE)
            {
                return Err(failed);
            }
        }
        name_bridge_note_passed(ctx, verdict);
    }
    // Lambda proxies are JVM hidden classes вЂ” HotSpot's `getCanonicalName()`
    // returns null for them (no canonical name). Without this the fallback
    // below would leak "unknown_<id>". bug-06 fam5 #1.
    if let Some(class_id) = mirror_class_id(ctx, this) {
        if is_lambda_proxy_id(class_id) && ctx.lambda_proxy_host(class_id).is_some() {
            return Ok(Some(Value::Object(None)));
        }
    }
    // Keep the canonical identity aligned with getName()/getSimpleName() for
    // Craton-generated Spring configuration proxies. In particular, preserve
    // the double-dollar CGLIB separators: Spring TypeReference equality is
    // based on this exact canonical name.
    if let Some(class_id) = ctx.class_id_from_mirror(this) {
        if let Some(name) = ctx.class_name_of_id(class_id) {
            if let Some(display_name) =
                spring_configuration_cglib_display_name(ctx, class_id, &name)
            {
                return Ok(Some(Value::Object(Some(
                    ctx.create_string(&display_name.replace('/', ".")),
                ))));
            }
        }
    }
    if let Some(class_id) = ctx.class_id_from_mirror(this) {
        // An empty canonical name is `canonical_class_name`'s sentinel for
        // "this class has no canonical name" (local / anonymous / nested in
        // one, JLS 6.7) — `Class.getCanonicalName()` returns null for those.
        if let Some(arc) = cache_get(&CANONICAL_CLASS_NAME_CACHE, ctx.vm_identity(), class_id) {
            if arc.is_empty() {
                return Ok(Some(Value::Object(None)));
            }
            return Ok(Some(Value::Object(Some(ctx.create_string(&arc)))));
        }
        // `getCanonicalName0` -> `getEnclosingClass()` -> `getDeclaringClass0()`
        // (interpreter round i2 wave 59, lane L3;
        // `CRATONVM_INNER_CLASSES_AGREEMENT`). A refusal is never cached.
        let asks_declaring = asks_declaring_known
            .unwrap_or_else(|| name_bridge_asks_declaring_class(ctx, this, true));
        if let Some(refusal) = asks_declaring
            .then(|| member_class_inner_classes_disagreement(ctx, class_id))
            .flatten()
        {
            return Err(refusal);
        }
        if let Some(name) = ctx.class_name_of_id(class_id) {
            let canonical = canonical_class_name(ctx, class_id, &name);
            if canonical.is_empty() {
                return Ok(Some(Value::Object(None)));
            }
            return Ok(Some(Value::Object(Some(ctx.create_string(&canonical)))));
        }
    }
    let name = mirror_class_name(ctx, this).unwrap_or_default();
    let canonical = if name.starts_with('[') {
        array_descriptor_to_canonical_name(&name).unwrap_or_default()
    } else {
        name.replace(['/', '$'], ".")
    };
    Ok(Some(Value::Object(Some(ctx.create_string(&canonical)))))
}

/// The exact refusal text `java.lang.Enum.valueOf` builds for a name that is
/// not a constant of the enum:
///
/// ```text
/// "No enum constant " + enumType.getCanonicalName() + "." + name
/// ```
///
/// Two things about that expression are easy to get wrong and were:
///
/// * The type must be present at all. `Enum.valueOf` here used to raise a bare
///   `No enum constant MAUVE`, which tells a reader nothing about WHICH enum
///   refused and does not match HotSpot.
/// * It is `getCanonicalName()`, not `getName()`. For a nested enum the
///   canonical name uses DOTS — `ShadowDifferentialProbe.Color` — so building
///   the message from the binary name yields `ShadowDifferentialProbe$Color`,
///   which is a different divergence rather than a fix.
///
/// Five sites in this crate raise this message (`Enum.valueOf` plus the
/// synthetic `java.time.Month` / `java.time.DayOfWeek` /
/// `java.time.temporal.ChronoUnit` / `java.math.RoundingMode` `valueOf`
/// bodies). They go through this one function so the shape cannot drift
/// between them — three per-site patches of one idiom is the recurring waste
/// this codebase keeps paying. See
/// W7-44-numberformat-enum-and-double-tostring.md.
pub fn no_enum_constant_message(canonical_type_name: &str, constant: &str) -> String {
    format!("No enum constant {canonical_type_name}.{constant}")
}

/// [`no_enum_constant_message`], resolving the type name from the enum's
/// `Class` mirror the way `Enum.valueOf` does.
///
/// A local or anonymous enum has NO canonical name (JLS 6.7), and
/// `Class.getCanonicalName()` returns Java `null` there. The JDK concatenates
/// that null straight into the message, producing a literal `"null."` prefix;
/// this reproduces that rather than silently substituting the binary name,
/// because the point of the message is to match HotSpot exactly.
pub fn no_enum_constant_message_for_mirror(
    ctx: &mut dyn NativeContext,
    enum_mirror: ObjectRef,
    constant: &str,
) -> String {
    // Bound to a `let` before the match on purpose: a `&mut ctx` reborrow left
    // in a match SCRUTINEE stays alive for the whole match, and the arm below
    // needs `ctx` again for `read_string`.
    let resolved = native_class_get_canonical_name(ctx, &[Value::Object(Some(enum_mirror))]);
    let canonical: String = match resolved {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_else(|| "null".into()),
        // `getCanonicalName()` returned Java null (local / anonymous enum), or
        // the mirror was not a class at all. Either way the JDK's string
        // concatenation renders it "null".
        _ => "null".to_string(),
    };
    no_enum_constant_message(&canonical, constant)
}

pub fn native_class_get_type_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Lambda proxies: `getTypeName()` mirrors `getName()` (the hidden-class
    // `Host$$Lambda/0x..` name), not "unknown_<id>". bug-06 fam5 #1.
    if let Some(class_id) = mirror_class_id(ctx, this) {
        if let Some(lname) = lambda_proxy_class_name(ctx, class_id) {
            return Ok(Some(Value::Object(Some(ctx.create_string(&lname)))));
        }
    }
    // Arrays: `getTypeName()` is the element's `getName()` plus one `[]` per
    // dimension вЂ” e.g. `[[Ljava/lang/String;` -> `java.lang.String[][]` and
    // `[I` -> `int[]`. This differs from `getName()`'s descriptor form
    // (`[[Ljava.lang.String;`), so it must NOT reuse the
    // `DOTTED_CLASS_NAME_CACHE` (which mirrors `getName()`). Surfaced by
    // Spring's `ClassEditor.getAsText()` on multi-dimensional arrays.
    {
        let arr_name = ctx
            .class_id_from_mirror(this)
            .and_then(|cid| ctx.class_name_of_id(cid))
            .or_else(|| mirror_class_name(ctx, this));
        if let Some(name) = arr_name {
            if name.starts_with('[') {
                if let Some(tn) = array_descriptor_to_type_name(&name) {
                    return Ok(Some(Value::Object(Some(ctx.create_string(&tn)))));
                }
            }
        }
    }
    // Non-array configuration proxies use the same public CGLIB name for
    // getTypeName() as for getName(); do this before the internal-name cache.
    if let Some(class_id) = ctx.class_id_from_mirror(this) {
        if let Some(name) = ctx.class_name_of_id(class_id) {
            if let Some(display_name) =
                spring_configuration_cglib_display_name(ctx, class_id, &name)
            {
                return Ok(Some(Value::Object(Some(
                    ctx.create_string(&display_name.replace('/', ".")),
                ))));
            }
        }
    }
    // `Class.getTypeName()` returns the dotted form for non-array refs. It is a
    // pure derivation from the slashed internal name and equals what
    // `dotted_class_name` produces вЂ” share the same cache as `Class.getName()`.
    if let Some(class_id) = ctx.class_id_from_mirror(this) {
        if let Some(arc) = cache_get(&DOTTED_CLASS_NAME_CACHE, ctx.vm_identity(), class_id) {
            return Ok(Some(Value::Object(Some(ctx.create_string(&arc)))));
        }
        if let Some(name) = ctx.class_name_of_id(class_id) {
            let type_name = dotted_class_name(ctx.vm_identity(), class_id, &name);
            return Ok(Some(Value::Object(Some(ctx.create_string(&type_name)))));
        }
    }
    let name = mirror_class_name(ctx, this).unwrap_or_default();
    let type_name = if name.contains('/') {
        dotted_binary_name(&name)
    } else {
        name
    };
    Ok(Some(Value::Object(Some(ctx.create_string(&type_name)))))
}

/// `Class.getTypeName()` as the JDK's own body computes it, reviewed
/// `cratonvm_native_api::NativeKind::Intrinsic` (SBR round 5, lane `walker`; record
/// `docs/feature-designs/native-dispatch-architecture.md`).
///
/// The JDK body is
/// `if (isArray()) { cl = this; d = 0; do { d++; cl = cl.getComponentType(); } while (cl.isArray());
///   return cl.getName().concat("[]".repeat(d)); } return getName();`
/// and this runs EXACTLY those steps, through the registered natives the bytecode would call
/// (`isArray`, `getComponentType`, `getName`), so a hidden class, a lambda proxy, a
/// user-loader component or any other name `getName` reports is reported here too, and a
/// non-array answers `getName()`'s own (interned) object, as on HotSpot. What it removes is
/// the seven bytecode-level calls (measured 5.5 us against 0.9 us for `String[][]`).
pub fn native_class_get_type_name_exact(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        // A null receiver is the interpreter's NPE before any body runs.
        _ => return Ok(Some(Value::Object(None))),
    };
    fn is_array(
        ctx: &mut dyn NativeContext,
        c: cratonvm_types::ObjectRef,
    ) -> Result<bool, MethodCallFailed> {
        Ok(matches!(
            native_class_is_array(ctx, &[Value::Object(Some(c))])?,
            Some(Value::Int(n)) if n != 0
        ))
    }
    // Fast path (no Java-visible difference from the steps below): a registered class's own
    // internal name already says whether it is an array and what its innermost element is, so
    // the three native calls per dimension (each 0.2-0.6 us here) collapse to one name read.
    // Anything unusual (an unregistered mirror, a lambda proxy element, a CratonVM
    // CGLIB display-name element) takes the step-by-step path below.
    if let Some(cid) = ctx.class_id_from_mirror(this) {
        if let Some(internal) = ctx.class_name_of_id(cid) {
            let dims = internal.bytes().take_while(|&b| b == b'[').count();
            if dims == 0 {
                return native_class_get_name(ctx, &[Value::Object(Some(this))]);
            }
            let elem = &internal[dims..];
            let base = match elem.strip_prefix('L').and_then(|r| r.strip_suffix(';')) {
                Some(inner)
                    if !inner.starts_with("unknown_")
                        && !inner.contains(CRATONVM_CONFIG_CGLIB_MARKER) =>
                {
                    // Wave 25, lane hexname (`CRATONVM_CLASS_NAME_HIDDEN_BY_CLASS`):
                    // the element's `/0x<hex>` tail by its class's hidden flag.
                    Some(dotted_binary_name_of_class(&*ctx, Some(cid), inner))
                }
                Some(_) => None,
                None => primitive_descriptor_name(elem).map(str::to_string),
            };
            if let Some(mut out) = base {
                out.reserve(dims * 2);
                for _ in 0..dims {
                    out.push_str("[]");
                }
                return Ok(Some(Value::Object(Some(
                    ctx.create_string_uninterned(&out),
                ))));
            }
        }
    }
    if !is_array(ctx, this)? {
        return native_class_get_name(ctx, &[Value::Object(Some(this))]);
    }
    let mut cl = this;
    let mut dims = 0usize;
    loop {
        dims += 1;
        cl = match native_class_get_component_type(ctx, &[Value::Object(Some(cl))])? {
            Some(Value::Object(Some(c))) => c,
            _ => return native_class_get_name(ctx, &[Value::Object(Some(this))]),
        };
        if !is_array(ctx, cl)? {
            break;
        }
    }
    let name = match native_class_get_name(ctx, &[Value::Object(Some(cl))])? {
        Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let mut out = String::with_capacity(name.len() + dims * 2);
    out.push_str(&name);
    for _ in 0..dims {
        out.push_str("[]");
    }
    Ok(Some(Value::Object(Some(
        ctx.create_string_uninterned(&out),
    ))))
}

/// The three JDK enums the synthetic model did not carry, in `javap`
/// declaration order — which IS the ordinal, and is not alphabetical.
///
/// These lists are the SAME data as `class_manager.rs`'s `enum_constant_fields`
/// rows and must stay identical to them: that side declares the statics so
/// `GETSTATIC` resolves, this side populates them so the resolved value is not
/// null, and either half alone is worse than neither.
#[cfg(feature = "synthetic-jdk")]
pub const DAY_OF_WEEK_CONSTANTS: &[&str] = &[
    "MONDAY",
    "TUESDAY",
    "WEDNESDAY",
    "THURSDAY",
    "FRIDAY",
    "SATURDAY",
    "SUNDAY",
];
#[cfg(feature = "synthetic-jdk")]
pub const HTTP_VERSION_CONSTANTS: &[&str] = &["HTTP_1_1", "HTTP_2"];
#[cfg(feature = "synthetic-jdk")]
pub const HTTP_REDIRECT_CONSTANTS: &[&str] = &["NEVER", "ALWAYS", "NORMAL"];
#[cfg(feature = "synthetic-jdk")]
synthetic_enum_pair!(
    day_of_week_clinit,
    day_of_week_values,
    "java/time/DayOfWeek",
    DAY_OF_WEEK_CONSTANTS
);
#[cfg(feature = "synthetic-jdk")]
synthetic_enum_pair!(
    http_version_clinit,
    http_version_values,
    "java/net/http/HttpClient$Version",
    HTTP_VERSION_CONSTANTS
);
#[cfg(feature = "synthetic-jdk")]
synthetic_enum_pair!(
    http_redirect_clinit,
    http_redirect_values,
    "java/net/http/HttpClient$Redirect",
    HTTP_REDIRECT_CONSTANTS
);

/// Mint and publish a synthetic JDK enum's constants, exactly as its real
/// `<clinit>` would, then publish `$VALUES`.
///
/// ONE implementation, parameterised, rather than a copy per enum. That is the
/// whole point: `posix_publish_constants` — the model this generalises — is
/// itself the survivor of a copy that went wrong. Its doc records that the
/// `name`/`ordinal` slot fallbacks in the two sibling copies were INVERTED
/// relative to each other, latent only because `class_manager` happens to give
/// these stubs `java/lang/Enum` as a superclass, and that "one enum copied from
/// this model without that row" would make every constant NAMELESS — non-null,
/// and matching nothing in `valueOf`. Adding three more enums by copy would
/// have been three more chances at that.
///
/// The three obligations that model states, kept here:
///
/// 1. **`name` and `ordinal` come from [`enum_name_ordinal_slots`]**, which
///    resolves both against `java/lang/Enum`, so they cannot be got the wrong
///    way round at one site without being wrong at all of them.
/// 2. **Declaration order IS the ordinal.** `compareTo`, `EnumMap` and
///    `EnumSet` all key on it, so `constants` must be given in `javap` order.
/// 3. **`$VALUES` is re-READ out of the statics**, not filled from the refs
///    minted in pass one: `new_ref_array` allocates and can move them. The
///    re-read is also what makes `values()[i] == CONSTANT`, the identity
///    `Enum.valueOf`, `Class.getEnumConstants` and `EnumSet` rely on.
///
/// Runtime-gated on `is_class_synthetic_stub` rather than on the Cargo feature,
/// so a feature-enabled binary running real-JDK mode leaves the JDK's own
/// interned constants alone.
pub fn publish_synthetic_enum_constants(
    ctx: &mut dyn NativeContext,
    class: &str,
    constants: &[&str],
) -> MethodCallResult {
    if !ctx.is_class_synthetic_stub(class) {
        return Ok(None);
    }
    let Some(cid) = ctx.class_id_by_name(class) else {
        return Ok(None);
    };
    // Idempotence: `<clinit>` runs once per class by construction, but a second
    // entry through any path must not replace live constants with fresh objects
    // that then fail `==`.
    if let Some(first) = constants.first() {
        if let Some(slot) = ctx.static_field_index_by_name(cid, first) {
            if matches!(ctx.get_static_field(cid, slot), Value::Object(Some(_))) {
                return Ok(None);
            }
        }
    }
    let (name_idx, ord_idx) = crate::phases_late::nio_file::enum_name_ordinal_slots(ctx);
    let nfields = ctx.class_num_total_fields(cid).max(2);
    for (ord, &name) in constants.iter().enumerate() {
        let obj = ctx.alloc_object(cid, nfields);
        // Pin across `create_string`: a moving young GC there would relocate
        // the fresh constant while this frame still holds `obj`. Nothing
        // allocates between the field writes and the publish to the static,
        // which is a GC root, so the constant is never unreachable-but-live.
        let obj_pin = ctx.pin_native_root(obj);
        let name_obj = ctx.create_string(name);
        let obj = ctx.read_native_pin(obj_pin, obj);
        ctx.unpin_native_roots(obj_pin);
        ctx.set_field(obj, name_idx, Value::Object(Some(name_obj)));
        // Cast: no JDK enum modelled here is near `i32`.
        ctx.set_field(obj, ord_idx, Value::Int(ord as i32));
        ctx.set_static_field_by_name(class, name, Value::Object(Some(obj)));
    }
    let values_array = ctx.new_ref_array(cid, constants.len());
    for (idx, &name) in constants.iter().enumerate() {
        let published = match ctx.static_field_index_by_name(cid, name) {
            Some(slot) => ctx.get_static_field(cid, slot),
            None => Value::Object(None),
        };
        // `set_array_element` does not allocate, so `values_array` cannot move
        // underneath this loop.
        ctx.set_array_element(values_array, idx, published);
    }
    // Both spellings: javac emits `$VALUES`, ecj emits `ENUM$VALUES`, and a
    // write to an undeclared static is a silent no-op, so writing both is free.
    ctx.set_static_field_by_name(class, "$VALUES", Value::Object(Some(values_array)));
    ctx.set_static_field_by_name(class, "ENUM$VALUES", Value::Object(Some(values_array)));
    Ok(None)
}

/// `<Enum>.values()` — a FRESH array each call, holding the interned constants.
///
/// Freshness is not stylistic: the real method is `$VALUES.clone()`, so
/// `values() != values()` while `values()[0] == CONSTANT`. Handing back one
/// shared array would let a single caller's `values()[0] = null` corrupt every
/// later caller.
///
/// Reads through the STATICS rather than `$VALUES` so it is correct even if the
/// `$VALUES` declaration is ever missing from a stub's field table — the shape
/// `posix_file_permission_values` documents as a live gap for its own enum.
pub fn synthetic_enum_values(
    ctx: &mut dyn NativeContext,
    class: &str,
    constants: &[&str],
) -> MethodCallResult {
    let cid = ctx.ensure_class_initialized(class)?;
    let arr = ctx.new_ref_array(cid, constants.len());
    for (idx, &name) in constants.iter().enumerate() {
        let published = match ctx.static_field_index_by_name(cid, name) {
            Some(slot) => ctx.get_static_field(cid, slot),
            None => Value::Object(None),
        };
        ctx.set_array_element(arr, idx, published);    }
    Ok(Some(Value::Object(Some(arr))))
}

/// `java.lang.Enum.valueOf(Class, String)` — **`--synthetic-jdk` only**.
///
/// `java/lang/Enum`'s five natives were retired on 2026-08-23 (`80d60e911`),
/// correctly: the retirement was measured against the real-JDK regression
/// suite, where `Enum.valueOf`'s real bytecode serves every call and a native
/// only shadows it.
///
/// `--synthetic-jdk` has no bytecode to fall back on, so the same retirement
/// left `Enum.valueOf` an `UnsatisfiedLinkError` in that mode — for EVERY enum,
/// including user-defined ones. Measured 2026-09-02 with
/// `apps/probes/SyntheticEnumSurface`: real-JDK identical to HotSpot on all 20
/// rows, `--synthetic-jdk` failing this one on a three-constant local enum
/// whose `values()`, `name()`, `ordinal()` and `getEnumConstants()` all worked.
///
/// That is the third time this session one retirement has behaved this way
/// (`ArrayDeque.iterator()` and `Collections.unmodifiableSortedMap` are the
/// others): **a native retired because real bytecode covers it is retired in
/// BOTH modes, and only one of them has the bytecode.** Hence the `cfg` —
/// real-JDK keeps the retirement exactly as measured.
///
/// Built on the same `$VALUES` path as `native_class_get_enum_constants`
/// rather than a second constant source, so the two cannot disagree about
/// which constants an enum has.
#[cfg(feature = "synthetic-jdk")]
pub fn native_enum_value_of(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let (cls, name_obj) = match (args.first(), args.get(1)) {
        (Some(Value::Object(Some(c))), Some(Value::Object(Some(n)))) => (*c, *n),
        // Both arguments are `@NotNull` in the JDK and it NPEs on either.
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("Name is null".to_string()),
            }
            .into())
        }
    };
    let wanted = ctx.read_string(name_obj).unwrap_or_default();
    let class_name = mirror_class_id(ctx, cls)
        .and_then(|id| ctx.class_name_of_id(id))
        .unwrap_or_default();

    // Reuse the shared constant source; `Some(array)` here is exactly what
    // `Class.getEnumConstants()` would hand back.
    let constants = native_class_get_enum_constants(ctx, &[Value::Object(Some(cls))])?;
    if let Some(Value::Object(Some(arr))) = constants {
        let n = ctx.array_length(arr);
        for i in 0..n {
            let Value::Object(Some(k)) = ctx.get_array_element(arr, i) else {
                continue;
            };
            // Slot 0 is `Enum.name` — the same index `classloader.rs`'s
            // `resolve_field_index("java/lang/Enum", "name")` test pins.
            if let Value::Object(Some(nm)) = ctx.get_field(k, 0) {
                if ctx.read_string(nm).as_deref() == Some(wanted.as_str()) {
                    return Ok(Some(Value::Object(Some(k))));
                }
            }
        }
    }
    // HotSpot's exact wording, so a caller matching on the message still works:
    //   No enum constant com.example.Colour.MAUVE
    Err(
        cratonvm_types::error::RuntimeError::IllegalArgumentException {
            message: format!("No enum constant {}.{wanted}", class_name.replace('/', ".")),
        }
        .into(),
    )
}

pub fn native_class_get_enum_constants(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Real implementation: read the synthetic `$VALUES` static field
    // populated by the enum's `<clinit>`, then return a fresh Object[]
    // clone so callers (e.g. EnumMap.getKeyUniverse в†’ arraylength) see a
    // non-null, fully-populated array.
    //
    // This is the target of both `Class.getEnumConstants()` and the
    // package-private `Class.getEnumConstantsShared()` вЂ” both bytecode
    // paths in the real JDK ultimately need the enum's `$VALUES` array,
    // and our earlier stub that returned an empty array broke every
    // `EnumMap.<init>(Class)` call (e.g. StreamOpFlag.<clinit>).
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };
    // Non-enum classes must return null (per JDK spec).
    if !class_is_declared_enum(ctx, class_id) {
        return Ok(Some(Value::Object(None)));
    }
    // Ensure the enum's <clinit> has run so `$VALUES` is populated.
    let class_name = match ctx.class_name_of_id(class_id) {
        Some(n) => n,
        None => return Ok(Some(Value::Object(None))),
    };
    if let Err(_e) = ctx.ensure_class_initialized_with_class_id(class_id) {
        // Fall through вЂ” we can still try to read $VALUES if it was
        // populated before the failure (common after silent-swallow).
    }
    // Read the synthetic enum-values array static field.  `javac` names it
    // `$VALUES`; the Eclipse JDT compiler (`ecj`) вЂ” used to build several
    // WildFly modules вЂ” emits `ENUM$VALUES` (ACC_SYNTHETIC) instead.  The
    // real JDK sidesteps the name entirely by invoking the generated
    // `values()` accessor reflectively, so it is compiler-agnostic; we read
    // the field directly (our reflective `values()` invoke historically
    // returned null), so we must accept both spellings or ecj-compiled app
    // enums (e.g. org/wildfly/extension/health/HealthSubsystemSchema) fail
    // with a spurious "not an enum" CCE in EnumSet.allOf / EnumMap.<init>.
    let idx = match ctx
        .static_field_index_by_name(class_id, "$VALUES")
        .or_else(|| ctx.static_field_index_by_name(class_id, "ENUM$VALUES"))
    {
        Some(i) => i,
        None => {
            tracing::warn!(
                "native_class_get_enum_constants: no $VALUES/ENUM$VALUES field for class={}",
                class_name
            );
            return Ok(Some(Value::Object(None)));
        }
    };
    let values_val = ctx.get_static_field(class_id, idx);
    let src_arr: ObjectRef = match values_val {
        Value::Object(Some(a)) => a,
        _ => {
            tracing::warn!(
                "native_class_get_enum_constants: $VALUES is null/non-object for class={}",
                class_name
            );
            return Ok(Some(Value::Object(None)));
        }
    };
    // Clone into a new array whose component type is the enum class
    // itself (matches `getEnumConstantsShared().clone()` semantics вЂ”
    // `$VALUES` is typed `[LEnumClass;`). Callers of `getEnumConstants`
    // then `checkcast [Ljava/lang/Enum;`, which requires the component
    // class to be a subclass of `java/lang/Enum`. Using a plain
    // `Object[]` makes that checkcast fail silently (returning the array
    // as-is from non-strict casts elsewhere, then yielding 0-length
    // streams downstream вЂ” observed as
    // `Utils.enumOptions(SecurityProtocol.class)` returning empty,
    // which caused Kafka's `ReplicationConfigs.<clinit>` to throw
    // `ConfigException: Invalid value PLAINTEXT for configuration
    // security.inter.broker.protocol: String must be one of: `).
    let len = ctx.array_length(src_arr);
    // Cross-call GC-safety (2026-08-04): `new_ref_array` ALLOCATES, and a
    // moving young collection there relocates `$VALUES`. Reading the source
    // elements back through the pre-allocation `src_arr` then walks a vacated
    // from-space array and copies nulls, so `getEnumConstants()` handed back a
    // correctly-SIZED array of nulls — and `Enum.valueOf`, which scans it,
    // threw `IllegalArgumentException: No enum constant <NAME>` for a constant
    // that plainly exists. Reproduced under `CRATONVM_GC=stress=65536` as
    // `jdk/internal/util/OperatingSystem.<clinit>` → `valueOf("WINDOWS")`
    // failing inside `sun/nio/ch/Net.<clinit>`, which made `new ServerSocket(0)`
    // die with `ExceptionInInitializerError` under `CRATONVM_REAL=net-sockets`
    // (see serversocket-bind-null-inetaddress-net-sockets, 2026-08-03).
    // Root both arrays and re-read them around every allocating call.
    let mut scope = NativeHandleScope::new(ctx);
    let src_h = scope.root(src_arr);
    let out_arr = scope.new_ref_array(class_id, len);
    let out_h = scope.root(out_arr);
    for i in 0..len {
        let src_cur = scope.get(&src_h);
        let v = scope.get_array_element(src_cur, i);
        let out_cur = scope.get(&out_h);
        scope.set_array_element(out_cur, i, v);
    }
    let out = scope.get(&out_h);
    Ok(Some(Value::Object(Some(out))))
}

/// `java.lang.Class.cast(Object)`.
///
/// The JDK body is two lines — `if (obj != null && !isInstance(obj)) throw new
/// ClassCastException(cannotCastMsg(obj)); return (T) obj;` — and this native
/// implemented only the second of them. It took `_ctx`, so it could not have
/// checked anything: every cast succeeded.
///
/// That matters because `Class.cast` is not a convenience wrapper, it is the
/// ONLY runtime type barrier for the reflective containers built on it —
/// `Collections.checkedList/checkedMap`, Guava's `ClassToInstanceMap`,
/// Jackson's converters. With the check absent a heterogeneous container
/// accepts a wrong-typed element silently and the `ClassCastException` surfaces
/// at some unrelated later `checkcast`, with a stack trace pointing nowhere
/// near the insertion that caused it.
///
/// This is the same repair the sibling `native_class_as_subclass` (below)
/// already received — it too "unconditionally returned `this`" until a caller
/// that used the CCE for control flow exposed it. The two are one JVMS rule
/// implemented twice; only one side had been fixed.
pub fn native_class_cast(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let obj = args.get(1).copied().unwrap_or(Value::Object(None));
    // `cast(null)` is null for every class, primitive mirrors included — the
    // JDK short-circuits on `obj != null` before consulting `isInstance`.
    let Value::Object(Some(value)) = obj else {
        return Ok(Some(obj));
    };
    let this = match args.first() {
        Some(Value::Object(Some(mirror))) => *mirror,
        // No receiver mirror to test against. Keep the historic pass-through
        // rather than invent a refusal out of a missing argument — the same
        // stance `asSubclass` takes for its null/absent target.
        _ => return Ok(Some(obj)),
    };
    let is_instance = matches!(
        native_class_is_instance(ctx, &[Value::Object(Some(this)), obj])?,
        Some(Value::Int(v)) if v != 0
    );
    if is_instance {
        return Ok(Some(obj));
    }
    // HotSpot's `Class.cannotCastMsg`: "Cannot cast <actual> to <target>",
    // both dotted.
    // Wave 25, lane hexname: the target by its class's hidden flag.
    let target_name = reflect_text_class_name_of_class(
        &*ctx,
        mirror_class_id(&*ctx, this),
        &mirror_class_name(&*ctx, this).unwrap_or_default(),
    );
    // NOT `class_name_of_id(class_id_of_object(..))`: on an ARRAY that answers
    // the COMPONENT's class (`ClassId(0)` for a primitive array), so casting an
    // `int[]` reported "Cannot cast java.lang.Object" — G69-1's finding, and
    // this is the one live site sweep 11 found for it out of 51 rows.
    let actual_name = class_get_name_of(&*ctx, value);
    Err(cratonvm_types::error::RuntimeError::ClassCastException {
        message: format!("Cannot cast {actual_name} to {target_name}"),
    }
    .into())
}

/// `class_id_by_name("java/lang/ClassLoader")`, memoized per thread and VM
/// (gc-common w21-d).
///
/// PERF: `Class.getClassLoader()` asked it on EVERY call that found a
/// `classLoader` field set -- a class-manager read lock plus a name-hash lookup
/// for a constant -- and frameworks call `getClassLoader()` per class they
/// touch (`ServiceLoader`, Spring's `ClassUtils`, every `Class.forName(name,
/// init, c.getClassLoader())`). A `ClassId` is never reissued within a VM and
/// the stub-to-real upgrade keeps the id, so only a RESOLVED answer is cached;
/// a miss (the class not loaded yet) is asked again next time, as before.
/// Keyed by `vm_identity` like `primitive_field_slot`: ids restart per VM.
pub fn class_loader_class_id(ctx: &dyn NativeContext) -> Option<ClassId> {
    thread_local! {
        static MEMO: std::cell::Cell<Option<(usize, ClassId)>> =
            const { std::cell::Cell::new(None) };
    }
    let vm = ctx.vm_identity();
    if vm == 0 {
        // No identity (the trait default: mock and test contexts, which all
        // share it and number their classes independently). Never memoized.
        return ctx.class_id_by_name("java/lang/ClassLoader");
    }
    if let Some((cached_vm, cid)) = MEMO.with(|c| c.get()) {
        if cached_vm == vm {
            return Some(cid);
        }
    }
    let cid = ctx.class_id_by_name("java/lang/ClassLoader")?;
    MEMO.with(|c| c.set(Some((vm, cid))));
    Some(cid)
}

/// Native override for `java.lang.Class.getClassLoader()`.
///
/// JVM spec В§5.3: classes loaded by the bootstrap loader return `null`,
/// otherwise return the defining loader.  In CratonVM:
///
/// - Bootstrap classes (java/*, javax/*, jdk/*, sun/*, com/sun/*) в†’ null.
/// - Anything else (app classpath via `-c`, user-defined hidden classes,
///   synthetic test fixtures, primitive-mirror lookups that arrive here) в†’
///   the singleton application `ClassLoader` instance.  This must be
///   non-null so callers like `commons-logging`'s
///   `LogFactory.<clinit>` (which does
///   `LogFactory.class.getClassLoader().loadClass(IMPL)`) don't NPE on
///   the next `loadClass` invocation.
///
/// The fix lives in `register_essential_natives` (real-JDK mode); the
/// JDK 25 bytecode for `Class.getClassLoader()` is just
/// `getfield classLoader; areturn`, which would always read `null`
/// (the field is never populated by VM-internal mirror creation).  The
/// native dispatch path in `try_stackless_invoke` / `invoke_or_native`
/// checks the registry first, so this override wins over the bytecode.
pub fn native_class_get_class_loader(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mirror = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // A PRIMITIVE class has no defining loader: `Class.getClassLoader()` is
    // specified to answer null for `int.class` and friends, exactly as it does
    // for a bootstrap-loaded class. This has to be checked FIRST, before the
    // `classLoader` field and the reverse map below, because a primitive mirror
    // is created by the VM and can pick up whichever loader minted it --
    // MEASURED: `int.class.getClassLoader()` answered
    // `ClassLoaders$AppClassLoader` against HotSpot's null, in BOTH modes
    // (`probes/LoaderModuleSweep.java`).
    //
    // The direction matters: a non-null loader on a primitive makes a caller
    // believe the type is application-defined, and the usual next move --
    // `loader.loadClass(name)` or a loader-keyed cache -- then keys `int` under
    // the wrong loader.
    if mirror_is_primitive(ctx, mirror) {
        return Ok(Some(Value::Object(None)));
    }
    // AN ARRAY'S loader is its COMPONENT's loader, recursively.
    //
    // `String[].class.getClassLoader()` is null because `String`'s is;
    // `MyClass[].class.getClassLoader()` is the loader that defined `MyClass`.
    // An array class is not defined by any loader of its own, so reading the
    // mirror's own `classLoader` field or the reverse map answers about the
    // wrong thing. MEASURED in BOTH modes (`probes/ClassLoaderShadowSweep.java`):
    //
    //   ClassLoaderShadowSweep[].class.getClassLoader() == APP
    //     HotSpot  true      CratonVM  false
    //
    // The direction is the damaging one for a framework: an application array
    // type reported as bootstrap-loaded looks like a JDK type, and the usual
    // next moves -- a loader-keyed cache, an `isAssignableFrom` check against a
    // loader-scoped class, a serializer choosing a resolver -- all key it wrong.
    // `String[]` answering null is CORRECT and is why this must recurse to the
    // component rather than simply return the receiver's own loader.
    if mirror_is_array(ctx, mirror) {
        if let Some(arr_cid) = ctx.class_id_from_mirror(mirror) {
            if let Some(comp_cid) = ctx.array_component_class_id(arr_cid) {
                let comp_mirror = ctx.get_class_mirror(comp_cid);
                return native_class_get_class_loader(ctx, &[Value::Object(Some(comp_mirror))]);
            }
        }
    }
    // In real-JDK mode `getClassLoader()` is intercepted before its ordinary
    // field-reading bytecode runs. Honor a loader recorded on the mirror by
    // ClassLoader.defineClass before consulting the VM side table or package
    // fallback; this also makes a JDK-owned class intentionally defined by a
    // private loader (MethodUtil's Trampoline) retain that ownership.
    if let Value::Object(Some(loader)) = ctx.get_field_by_name(mirror, "classLoader") {
        let is_loader = class_loader_class_id(ctx)
            .map(|loader_class_id| {
                let actual_class_id = ctx.class_id_of_object(loader);
                actual_class_id == loader_class_id
                    || ctx.is_subclass(actual_class_id, loader_class_id)
            })
            .unwrap_or(false)
            || crate::classloader::is_user_defined_loader(ctx, loader);
        if is_loader {
            return Ok(Some(Value::Object(Some(loader))));
        }
    }
    // Prefer the authoritative reverse map; fall back to the legacy
    // slot-0 ClassId encoding for synthetic test fixtures.
    let class_id_opt = ctx.class_id_from_mirror(mirror).or_else(|| {
        if let Value::Int(v) = ctx.get_field(mirror, 0) {
            if v > 0 {
                return Some(cratonvm_types::ClassId::new(v as u32));
            }
        }
        None
    });
    let class_id = match class_id_opt {
        Some(cid) => cid,
        None => {
            // No resolvable ClassId вЂ” return the app loader so
            // `Class.getClassLoader()` is never null for a non-bootstrap
            // class.  The only path that yields null is the explicit
            // bootstrap-package case below.
            let cl = crate::classloader::get_or_create_app_loader(ctx);
            return Ok(Some(Value::Object(Some(cl?))));
        }
    };
    // A lambda / method-reference proxy is a hidden class defined by its HOST's
    // loader (HotSpot's `InnerClassLambdaMetafactory` defines it through a lookup
    // on the host), so `getClassLoader()` answers the host's: null for a
    // `java.base` lambda. The proxy has no defining-loader record of its own, and
    // the image-class rule below named every proxy of a JDK host the application
    // loader. JDK 21's `JDKEventTask` refuses a periodic hook whose lambda's loader
    // is not null (`SecurityException`), which silently lost `jdk.ExceptionStatistics`.
    if is_lambda_proxy_id(class_id) {
        if let Some(host_cid) = lambda_proxy_host_class(&*ctx, class_id) {
            if host_cid != class_id {
                let host_mirror = ctx.get_class_mirror(host_cid);
                return native_class_get_class_loader(ctx, &[Value::Object(Some(host_mirror))]);
            }
        }
    }
    // A class defined through a user-defined `ClassLoader.defineClass` records
    // its exact defining loader instance вЂ” return that, not the app-loader
    // fallback below. Without this, ByteBuddy's `ByteArrayClassLoader.load`
    // sanity check (`Class.forName(name, false, cl).getClassLoader() == cl`)
    // fails with "Class already loaded" and Hibernate's proxy generation breaks.
    if let Some(loader) =
        crate::classloader::defining_loader_for(crate::classloader::loader_tables(ctx), class_id.as_u32())
    {
        // This reverse relation is written only by successful defineClass paths
        // and reconciled across GC; it is the authoritative loader identity.
        return Ok(Some(Value::Object(Some(loader))));
    }
    let loader_type = ctx.loader_id_of_class(class_id);
    let class_name = ctx.class_name_of_id(class_id).unwrap_or_default();
    // No `sun/reflect/misc/Trampoline` exception any more (interpreter round
    // i7 wave 137, lane L5; zs4 page): `MethodUtil`'s copy answers above (its
    // mirror's `classLoader`, its defining-loader record, or its user
    // namespace id below), so only the BOOT copy -- `loader_type == 0` --
    // reaches this predicate, and both `loader_type == 0` arms answer it
    // `null` (`builtin_loader_for_image_class` names no built-in loader for
    // a `java.base` package).
    let is_jdk_pkg = class_name.starts_with("java/")
        || class_name.starts_with("javax/")
        || class_name.starts_with("jdk/")
        || class_name.starts_with("sun/")
        || class_name.starts_with("com/sun/");
    if loader_type == 0 && is_jdk_pkg {
        // NOT every image class is boot-loaded -- see
        // `classloader::builtin_loader_for_image_class`.
        // GC: the loader's first ask may make it (`get_or_create_platform_loader`
        // adopts the JDK's) and `package_builtin_kind` may run Java; `mirror`
        // is written after, so it is pinned across and re-read (interpreter
        // round i7 wave 137, lane L5, review of wave 136).
        let mirror_pin = ctx.pin_native_root(mirror);
        let builtin = crate::classloader::builtin_loader_for_image_class(ctx, &class_name);
        let mirror = ctx.read_native_pin(mirror_pin, mirror);
        ctx.unpin_native_roots(mirror_pin);
        if let Some(platform) = builtin {
            // SBR2-LOADERS: record it where the JDK's `getClassLoader0` reads.
            if crate::classloader::builtin_loaders_adopted(crate::classloader::loader_tables(&*ctx)) {
                ctx.set_field_by_name(mirror, "classLoader", Value::Object(Some(platform)));
            }
            return Ok(Some(Value::Object(Some(platform))));
        }
        // Bootstrap loader -> null per JVM spec.
        return Ok(Some(Value::Object(None)));
    }
    if loader_type == 0 && !is_jdk_pkg {
        // A bootstrap-tagged class outside the JDK packages is one of:
        //   (a) a class genuinely injected into the bootstrap search via
        //       `Instrumentation.appendToBootstrapClassLoaderSearch`
        //       (Mockito's inline mock maker injects `MockMethodDispatcher`
        //       there and then asserts `getClassLoader() == null`), or
        //   (b) a synthetic stub of an app-level class (those default to the
        //       bootstrap loader id but are really application placeholders).
        // For (a) вЂ” a real, fully-loaded class вЂ” the spec says the loader is
        // null. For (b) keep the app-loader fallback below so a stubbed app
        // class never spuriously reports a null loader.
        //
        // Or (c): an image class of a PLATFORM module whose package is outside
        // the prefixes above (`org.ietf.jgss`, `org.jcp.xml.dsig.internal.dom`,
        // `org.w3c.dom.css`, `netscape.javascript`): the image's module map
        // answers it as it does in the prefix arm, where `null` was reported
        // (interpreter round i7 wave 136, lane L5; probe
        // `L5W136ImageLoaderOutsideJdkPrefixes`). `None` for a package of no
        // image module (an appended boot class, a stub's).
        // `CRATONVM_IMAGE_LOADER_OUTSIDE_JDK_PREFIXES=0` restores `null`.
        {
            // GC: as in the prefix arm above, `mirror` is pinned across the
            // ask, which may make the loader or run Java (wave 137, lane L5).
            let mirror_pin = ctx.pin_native_root(mirror);
            let found = crate::classloader::builtin_loader_for_image_class(ctx, &class_name);
            let mirror = ctx.read_native_pin(mirror_pin, mirror);
            ctx.unpin_native_roots(mirror_pin);
            if let Some(builtin) = found {
                if crate::classloader::builtin_loaders_adopted(crate::classloader::loader_tables(&*ctx)) {
                    ctx.set_field_by_name(mirror, "classLoader", Value::Object(Some(builtin)));
                }
                return Ok(Some(Value::Object(Some(builtin))));
            }
        }
        if !ctx.is_class_synthetic_stub(&class_name) {
            return Ok(Some(Value::Object(None)));
        }
    }
    // A class registered under a USER-DEFINED loader namespace (id >= 3)
    // belongs to that loader, even when no explicit `register_defining_loader`
    // ran for it -- natives that define straight into a namespace
    // (`define_class_full`, e.g. the config-class enhancer) never touch that
    // side table. Falling through to the app-loader singleton below made
    // `enhance(reloadedConfigClass, customLoader).getClassLoader()` report the
    // app loader (`context.annotation.ConfigurationClassEnhancerTests
    // .enhanceReloadedClass`).
    if loader_type >= 3 {
        if let Some(loader) = crate::classloader::loader_object_for_namespace_id_in(
            crate::classloader::loader_tables(ctx),
            loader_type as u32,
        ) {
            return Ok(Some(Value::Object(Some(loader))));
        }
    }
    if loader_type == 1 {
        // Platform/extension loader вЂ” return singleton.
        let cl = crate::classloader::get_or_create_platform_loader(ctx);
        return Ok(Some(Value::Object(Some(cl?))));
    }
    // Application class (`-c` classpath), user-defined loader, or a
    // class whose stored loader id is bootstrap but whose name is NOT
    // in a JDK package (= app classpath class registered before the
    // loader-id plumbing was wired up).  Return the singleton app
    // loader so `loadClass` works.
    let cl = crate::classloader::get_or_create_app_loader(ctx);
    Ok(Some(Value::Object(Some(cl?))))
}

pub fn native_class_as_subclass(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // `Class.asSubclass(clazz)` contract: return this Class object if it
    // represents a subtype of `clazz`, otherwise throw ClassCastException.
    // The previous implementation unconditionally returned `this`, which
    // broke callers that USE the CCE for control flow — e.g. Lucene's
    // `LuceneTestCase.newFSDirectory` relies on
    // `CommandLineUtil.loadFSDirectoryClass(...)` throwing CCE for a
    // non-FSDirectory `tests.directory` to fall back to a random
    // FSDirectory; with the lenient version it proceeded to
    // `getConstructor(Path.class, LockFactory.class)` on a directory class
    // without that ctor and failed the test with an uncaught
    // `NoSuchMethodException: <init>` (residual-doc item 5).
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    };
    let target = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        // Null / missing target: keep the historic lenient return (the
        // real JDK would NPE inside isAssignableFrom; some synthetic-mode
        // callers pass no argument at all).
        _ => return Ok(Some(Value::Object(Some(this)))),
    };
    let assignable = matches!(
        native_class_is_assignable_from(
            ctx,
            &[Value::Object(Some(target)), Value::Object(Some(this))],
        )?,
        Some(Value::Int(v)) if v != 0
    );
    if assignable {
        Ok(Some(Value::Object(Some(this))))
    } else {
        // HotSpot: `throw new ClassCastException(this.toString())`.
        let name =
            reflect_text_class_name(&*ctx, &mirror_class_name(&*ctx, this).unwrap_or_default());
        if crate::nbflags().dbg_cce_bt {
            let this_cid_map = ctx.class_id_from_mirror(this);
            let this_field0 = ctx.get_field(this, 0);
            let this_field1 = ctx.get_field(this, 1);
            let target_cid_map = ctx.class_id_from_mirror(target);
            let target_field0 = ctx.get_field(target, 0);
            let target_name = mirror_class_name(ctx, target).unwrap_or_default();
            eprintln!(
                "CRATONVM_DBG_CCE_BT: site=asSubclass this=0x{:x} this_cid_from_mirror={:?} this_field0={:?} this_field1={:?} this_name={:?} | target=0x{:x} target_cid_from_mirror={:?} target_field0={:?} target_name={:?}\n{}",
                this.as_ptr() as usize,
                this_cid_map,
                this_field0,
                this_field1,
                name,
                target.as_ptr() as usize,
                target_cid_map,
                target_field0,
                target_name,
                std::backtrace::Backtrace::force_capture()
            );
        }
        Err(cratonvm_types::error::RuntimeError::ClassCastException {
            message: format!("class {name}"),
        }
        .into())
    }
}

pub fn native_class_descriptor_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let name = mirror_class_name(ctx, this).unwrap_or_default();
    // A hidden class (`p.H/0x1f`, and an array of one) keeps the `.` before
    // its suffix, as JDK 25's body writes it: `Lp/H.0x1f;`. Translating every
    // `.` to `/` and leaving the suffix's `/` alone spelled `Lp/H/0x1f;`
    // (`R19ClassComponentCensus` `hidden-array`, `--compatible`). A package
    // segment cannot start with a digit, so a last segment `0x..` is the
    // hidden-class suffix in either name form.
    //
    // JIT round 21 wave 25, lane hexname (`CRATONVM_CLASS_NAME_HIDDEN_BY_CLASS`):
    // only when that class (an array's bottom class) is hidden. An ORDINARY
    // class literally named `fz9/0x2A` is `Lfz9/0x2A;` (the body's non-hidden
    // `"L" + getName().replace('.', '/') + ";"`); the shape alone spelled
    // `Lfz9.0x2A;`. An unknown flag, or `=0`, keeps the shape rule.
    let ordinary = hidden_name_suffix_split(&name).is_some()
        && mirror_class_id(&*ctx, this)
            .and_then(|id| bottom_class_hidden_of(&*ctx, id, &name))
            == Some(false);
    let hidden = name
        .rfind('/')
        .filter(|&i| !ordinary && name[i + 1..].starts_with("0x"))
        .map(|i| (&name[..i], &name[i + 1..]));
    if let Some((base, suffix)) = hidden {
        let base = base.replace('.', "/");
        // An array's suffix still carries its closing `;`.
        let desc = if base.starts_with('[') {
            format!("{base}.{suffix}")
        } else {
            format!("L{base}.{suffix};")
        };
        return Ok(Some(Value::Object(Some(ctx.create_string(&desc)))));
    }
    let desc = match name.as_str() {
        "int" => "I".to_string(),
        "long" => "J".to_string(),
        "float" => "F".to_string(),
        "double" => "D".to_string(),
        "boolean" => "Z".to_string(),
        "byte" => "B".to_string(),
        "char" => "C".to_string(),
        "short" => "S".to_string(),
        "void" => "V".to_string(),
        // Array class names are already in descriptor form ("[I",
        // "[Ljava.lang.String;"); normalize any package dots to slashes.
        _ if name.starts_with('[') => name.replace('.', "/"),
        _ => format!("L{};", name.replace('.', "/")),
    };
    Ok(Some(Value::Object(Some(ctx.create_string(&desc)))))
}

// ---------------------------------------------------------------------------
// T13 вЂ” java/lang/Class JDK 25 native method implementations
// ---------------------------------------------------------------------------

/// `java/lang/Class.getDeclaringClass0()Ljava/lang/Class;`
///
/// Returns the Class object of the outer class if this class is an inner class,
/// or null if it is a top-level class. Reads from the InnerClasses attribute.
pub fn native_class_get_declaring_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // bytebuddy_probe (agent-bb4) вЂ” Object short-circuit. The real JDK
    // returns null for Object.getDeclaringClass(). If our reverse-map is
    // aliased so that a synthetic mirror points at Object's ClassId, and
    // Object's `declaring_class` accidentally resolves to a non-null
    // value somewhere downstream, ByteBuddy's hierarchy walker can chain
    // through `getDeclaringClass()` and re-introduce a cycle. Read the
    // name via the STRICT helper (slot 1 first) and short-circuit null
    // when this mirror identifies as Object.
    let strict_name = mirror_class_name_strict(ctx, this).unwrap_or_default();
    if strict_name == "java/lang/Object" || strict_name == "java.lang.Object" {
        return Ok(Some(Value::Object(None)));
    }
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Object(None))),
    };
    // JIT round 18 wave 11 (lane mhresid; MHLOADER18-4): the outer class's
    // resolution error is thrown, as `compute_enclosing_class`'s `klass_at`
    // does (`CRATONVM_CLASS_OUTER_RESOLUTION_ERRORS`).
    let Some(outer_id) = declaring_class_id_checked(ctx, class_id, true)? else {
        // JIT round 21 wave 26, lane nesting: a local or anonymous class is
        // still checked against its EnclosingMethod class
        // (`CRATONVM_CLASS_LOCAL_INNER_CLASSES_CHECK`).
        if let Some(message) = local_class_inner_classes_disagreement(ctx, class_id) {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "java/lang/IncompatibleClassChangeError",
                &message,
            ));
        }
        return Ok(Some(Value::Object(None)));
    };
    // HotSpot's `JVM_GetDeclaringClass` -> `compute_enclosing_class` ->
    // `Reflection::check_for_inner_class` (JIT round 17 wave 8, lane mh7).
    if let Some(message) = inner_classes_disagreement(ctx, class_id, outer_id) {
        return Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/lang/IncompatibleClassChangeError",
            &message,
        ));
    }
    let mirror = ctx.get_class_mirror(outer_id);
    Ok(Some(Value::Object(Some(mirror))))
}

/// `CRATONVM_CLASS_INNER_CLASSES_CHECK` -- correctness, default ON, `=0`
/// answers the outer class without HotSpot's consistency check (JIT round 17
/// wave 8, lane mh7; [`inner_classes_disagreement`]). Read only for a nested
/// class whose outer class another loader defines.
pub fn class_inner_classes_check_enabled() -> bool {
    true
}

/// `CRATONVM_CLASS_LOCAL_INNER_CLASSES_CHECK` -- correctness, both modes,
/// default ON, `=0` answers null as before (JIT round 21 wave 26, lane
/// nesting): [`local_class_inner_classes_disagreement`].
pub fn class_local_inner_classes_check_enabled() -> bool {
    true
}

/// HotSpot's `Reflection::check_for_inner_class` with `inner_is_member`
/// false, the arm `JVM_GetDeclaringClass` takes for a LOCAL or ANONYMOUS
/// class (JIT round 21 wave 26, lane nesting;
/// `CRATONVM_CLASS_LOCAL_INNER_CLASSES_CHECK`): the ICCE text `"<outer> and
/// <inner> disagree on InnerClasses attribute"`, or `None`.
///
/// `compute_enclosing_class`: when the class's FIRST own `InnerClasses` row
/// (`find_inner_classes_attr`) has no outer class, the `EnclosingMethod`
/// class is resolved (`klass_at`; its error is
/// [`declaring_class_id_checked`]'s) and `check_for_inner_class(outer, this,
/// false)` runs BEFORE `JVM_GetDeclaringClass` answers null for a
/// non-member. That check passes only when the outer class's own
/// `InnerClasses` has a row whose inner entry names this class and whose
/// outer entry is 0. javac lists every local and anonymous class in its
/// enclosing class, so it passes there. It fails for:
/// - a hidden class: its own row names `this_class`, which the parser
///   renames to the hidden name, and no outer class can list that name;
/// - a hand-built class whose EnclosingMethod class does not list it.
///
/// Measured on HotSpot 25: `R21Fz8Nesting1`'s hidden HLoc / HAnon and
/// `R21Fz8ReviewEmGone1`'s $C throw ICCE out of `getDeclaringClass`, while
/// `R21Fz9EmMissing1`'s H$C and D$X, whose hosts list them, answer null. Only
/// `getDeclaringClass` reaches it: `getSimpleName`, `isMemberClass` and
/// `getEnclosingClass` stop at `hasEnclosingMethodInfo` first.
///
/// Asked only for a class whose own bytes are real (`ClassOrigin`, never by
/// name), and, unless the class is hidden, whose outer class's bytes are real
/// too: a VM-made stand-in has no table HotSpot could read. The comparison is
/// by name within the outer class's table; a cross-loader identity mismatch
/// is not modelled here.
pub fn local_class_inner_classes_disagreement(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
) -> Option<String> {
    if !class_local_inner_classes_check_enabled() || !class_inner_classes_check_enabled() {
        return None;
    }
    // The cheap question first: almost every class has no EnclosingMethod.
    let em_class = ctx.enclosing_method_class_name(class_id)?;
    if em_class.is_empty() || !ctx.class_has_real_bytes(class_id) {
        return None;
    }
    let inner_name = ctx.class_name_of_id(class_id)?;
    let hidden = ctx.is_class_hidden(class_id);
    let inner_rows = ctx.inner_classes(class_id);
    // The first self row decides; it must be a non-member row.
    let (_, first_outer, _, _) = inner_rows.iter().find(|(i, _, _, _)| *i == inner_name)?;
    if !first_outer.is_empty() {
        return None;
    }
    let outer = class_seen_by_referencing_class_checked(ctx, class_id, &em_class).ok()?;
    if !hidden && !ctx.class_has_real_bytes(outer) {
        return None;
    }
    let outer_name = ctx.class_name_of_id(outer)?;
    let outer_rows = ctx.inner_classes(outer);
    local_class_disagreement_text(&inner_name, &outer_name, hidden, &outer_rows)
}

/// The decision of [`local_class_inner_classes_disagreement`] over the
/// names and the outer class's `InnerClasses` rows `(inner, outer, simple
/// name, flags)`.
pub fn local_class_disagreement_text(
    inner_name: &str,
    outer_name: &str,
    inner_hidden: bool,
    outer_rows: &[(String, String, String, u16)],
) -> Option<String> {
    let listed = !inner_hidden
        && outer_rows
            .iter()
            .any(|(i, o, _, _)| i == inner_name && o.is_empty());
    (!listed).then(|| {
        format!(
            "{} and {} disagree on InnerClasses attribute",
            hidden_aware_external_name(outer_name),
            hidden_aware_external_name(inner_name)
        )
    })
}

/// HotSpot's `Klass::external_name` of an internal name: `/` to `.`, except
/// a hidden class's `/0x..` suffix, which keeps its `/`.
pub fn hidden_aware_external_name(internal: &str) -> String {
    match internal.rfind("/0x") {
        Some(at) => format!("{}{}", internal[..at].replace('/', "."), &internal[at..]),
        None => internal.replace('/', "."),
    }
}

/// HotSpot's `IncompatibleClassChangeError` message `"<Outer> and <Inner>
/// disagree on InnerClasses attribute"` when the member class `inner`, whose
/// own `InnerClasses` entry names `outer` (already resolved through `inner`'s
/// loader), is not the class `outer`'s entry for that name resolves to, or
/// `None` (JIT round 17 wave 8, lane mh7;
/// `r17-mh-door-loader-residuals-after-mh3-FIXED-20261006.md`, the `-pub` lines of
/// `R17Mh6Door`). `Reflection::check_for_inner_class` resolves the inner name
/// through the OUTER class's constant pool and compares identities: a nested
/// class a child-first loader defines while its outer class is the parent's
/// is the parent's `Outer$Inner` to the outer class, so `getDeclaringClass0`
/// (and through it `getSimpleName`, `getCanonicalName`, `isMemberClass`,
/// `getEnclosingClass`, `MethodType.toString`) throws.
///
/// Asked only when the two classes have different defining loaders and one of
/// them is user-defined (one defining loader always answers its own
/// definition). A built-in outer loader never delegates to a user-defined one,
/// so a user-defined `inner` under a built-in `outer` disagrees without a
/// load (when the defining-loader side table agrees). A user-defined outer loader is asked (its known answer first, then a
/// load through it, as `klass_at` does); every unknown is an accept. An outer
/// class whose `InnerClasses` table does not name `inner` at all (a spoofed
/// member, HotSpot's other ICCE) was also an accept, because the VM's
/// synthetic stubs carry name-guessed tables that must not throw. Since JIT
/// round 21 wave 14 (lane innerclasses) it is refused for every class that
/// real bytes back (by `ClassOrigin`, never by name), with any loaders
/// ([`spoofed_member_disagreement`],
/// `CRATONVM_CLASS_INNER_CLASSES_SPOOF_CHECK`).
pub fn inner_classes_disagreement(
    ctx: &mut dyn NativeContext,
    inner: cratonvm_types::ClassId,
    outer: cratonvm_types::ClassId,
) -> Option<String> {
    // Interpreter round i5 wave 102, lane L3
    // (`i102-L3-hidden-class-from-member-bytes-shape`): a hidden class whose
    // `InnerClasses` self entry makes it a member (the define renames that
    // entry, as HotSpot's parser renames `this_class`) is never listed by its
    // outer class under the mangled name, so `check_for_inner_class` always
    // refuses it, whatever the loaders.
    if ctx.is_class_hidden(inner) && class_inner_classes_check_enabled() {
        if let (Some(inner_name), Some(outer_name)) =
            (ctx.class_name_of_id(inner), ctx.class_name_of_id(outer))
        {
            return Some(format!(
                "{} and {} disagree on InnerClasses attribute",
                hidden_aware_external_name(&outer_name),
                hidden_aware_external_name(&inner_name)
            ));
        }
    }
    // JIT round 21 wave 14, lane innerclasses
    // (`CRATONVM_CLASS_INNER_CLASSES_SPOOF_CHECK`; page
    // `r21-spoofed-inner-classes-not-refused-20261019.md`): a member class
    // that the outer class's own `InnerClasses` does not list with that outer
    // class is refused whatever the loaders (`check_for_inner_class`'s first
    // arm finds no row), as `R21LookupInSpoofedMember` measured on HotSpot 25.
    if let Some(message) = spoofed_member_disagreement(ctx, inner, outer) {
        return Some(message);
    }
    let inner_loader = ctx.loader_id_of_class(inner);
    let outer_loader = ctx.loader_id_of_class(outer);
    if inner_loader == outer_loader || (inner_loader < 3 && outer_loader < 3) {
        return None;
    }
    if !class_inner_classes_check_enabled() {
        return None;
    }
    let inner_name = ctx.class_name_of_id(inner)?;
    let outer_name = ctx.class_name_of_id(outer)?;
    let declared = ctx
        .inner_classes(outer)
        .iter()
        .any(|(i, o, _, _)| *i == inner_name && *o == outer_name);
    if !declared {
        return None;
    }
    // `outer` must be what `inner`'s own constant pool resolves the outer
    // name to (`compute_enclosing_class`'s `klass_at`). When the VM's
    // declaring-class lookup answered another loader's copy instead, the
    // defect is that lookup, not the nested class: no ICCE
    // (`r17-class-declaring-class-resolves-outer-globally-FIXED-20261006.md`).
    if ctx
        .class_id_by_name_via_referencing_class(inner, &outer_name)
        .ok()
        != Some(outer)
    {
        return None;
    }
    if outer_loader >= 3 {
        let seen = match ctx.class_id_seen_by_class_without_loading(&inner_name, outer) {
            Some(id) => id,
            None => ctx
                .class_id_by_name_via_referencing_class(outer, &inner_name)
                .ok()?,
        };
        if seen == inner {
            return None;
        }
    } else {
        // Both witnesses must agree that the outer class is built-in and the
        // inner one user-defined: the class manager's loader id can lag the
        // defining-loader side table for a forked loader, and a wrong ICCE
        // is worse than a missed one.
        let tables = crate::classloader::loader_tables(ctx);
        if crate::classloader::defining_loader_for(tables, inner.as_u32()).is_none()
            || crate::classloader::defining_loader_for(tables, outer.as_u32()).is_some()
        {
            return None;
        }
    }
    Some(format!(
        "{} and {} disagree on InnerClasses attribute",
        outer_name.replace('/', "."),
        inner_name.replace('/', ".")
    ))
}

/// `CRATONVM_CLASS_INNER_CLASSES_SPOOF_CHECK` -- correctness, both modes,
/// default ON, `=0` off (JIT round 21 wave 14, lane innerclasses; page
/// `docs/feature-designs/native-dispatch-architecture.md`):
/// [`spoofed_member_disagreement`]. Read once per VM (the door caches); a
/// context with no cache slot (mock contexts) reads the flag table.
pub fn inner_classes_spoof_check_enabled(ctx: &dyn NativeContext) -> bool {
    match reflect_door_caches(ctx) {
        Some(c) => c.inner_classes_spoof_check,
        None => true,
    }
}

/// `CRATONVM_DECLARING_CLASS_ANY_OUTER_ROW` -- correctness, both modes,
/// default ON, `=0` restores the old rule (JIT round 21 wave 16, lane
/// innerclasses; page `r21-review-w15-native-findings-FIXED-20261020.md` item 9):
/// [`declaring_member_row_outer`]. Read once per VM (the door caches); a
/// context with no cache slot (mock contexts) reads the flag table. Public
/// for the VM's `NativeClassAccess::declaring_class`.
pub fn declaring_class_any_outer_row_enabled(ctx: &dyn NativeContext) -> bool {
    match reflect_door_caches(ctx) {
        Some(c) => c.declaring_class_any_outer_row,
        None => true,
    }
}

/// `CRATONVM_COMPAT_NESTING_FOLLOWS_ENCLOSING_METHOD` -- correctness,
/// `--compatible` (the bridges), default ON, `=0` restores wave 17's answers
/// (JIT round 21 wave 19, lane innerclasses; page
/// `r21-review-w18-findings-FIXED-20261020.md` item 3): [`class_nesting_of`].
/// Read once per VM; a context with no cache slot reads the flag table.
pub fn nesting_follows_enclosing_method_enabled(ctx: &dyn NativeContext) -> bool {
    match reflect_door_caches(ctx) {
        Some(c) => c.nesting_follows_enclosing_method,
        None => true,
    }
}

/// The `InnerClasses` generation a fill started at (JIT round 21 wave 19, lane
/// innerclasses; page `r21-review-w18-findings-FIXED-20261020.md` item 4;
/// `CRATONVM_REFLECT_INNER_TABLES_GENERATION`, default ON).
///
/// The race: a reflection call reads the OLD tables, a redefinition swaps
/// them (`CRATONVM_REDEFINE_SWAPS_INNER_CLASSES`) and runs
/// [`forget_redefined_inner_class_answers`], and then the call inserts its
/// stale answer, which would stay for the life of the VM. The forget bumps
/// the generation BEFORE it clears, and a fill takes its guard BEFORE it reads
/// any table:
/// - a fill that sees the bump before it inserts does not insert;
/// - one that inserts first re-reads the generation after the insert and
///   removes its row on a mismatch;
/// - otherwise its insert precedes the bump, so it precedes the clear, which
///   removes it.
///
/// `None` (no check) for a context with no cache slot (mocks) or with the
/// switch off.
#[derive(Clone, Copy)]
pub struct InnerTablesFill(pub Option<u64>);

/// [`InnerTablesFill`] at the start of a computation.
pub fn inner_tables_fill_guard(ctx: &dyn NativeContext) -> InnerTablesFill {
    match reflect_door_caches(ctx) {
        Some(c) if c.inner_tables_generation_check => InnerTablesFill(Some(
            c.inner_tables_generation
                .load(std::sync::atomic::Ordering::SeqCst),
        )),
        _ => InnerTablesFill(None),
    }
}

impl InnerTablesFill {
    /// Did a redefinition's forget start since the guard was taken?
    pub fn moved(self, ctx: &dyn NativeContext) -> bool {
        let Some(at) = self.0 else {
            return false;
        };
        reflect_door_caches(ctx).is_some_and(|c| {
            c.inner_tables_generation
                .load(std::sync::atomic::Ordering::SeqCst)
                != at
        })
    }
}

/// [`cache_insert`] for an answer computed from the `InnerClasses` /
/// `EnclosingMethod` tables after `fill` was taken ([`InnerTablesFill`]).
pub fn cache_insert_inner_tables(
    cache: &ClassNameCache,
    ctx: &dyn NativeContext,
    class_id: ClassId,
    value: Arc<str>,
    fill: InnerTablesFill,
) -> Arc<str> {
    if fill.moved(ctx) {
        return value;
    }
    let vm = ctx.vm_identity();
    cache_insert(cache, vm, class_id, Arc::clone(&value));
    if fill.moved(ctx) {
        cache_get_or_init(cache).write().remove(&(vm, class_id.as_u32()));
    }
    value
}

/// The outer class name of `this_name`'s MEMBER link in its own
/// `InnerClasses` rows `(inner, outer, simple name)`, or `None` when the class
/// is not a member class (top-level, local or anonymous).
///
/// HotSpot (`InstanceKlass::find_inner_classes_attr`, then
/// `compute_enclosing_class`): the FIRST row whose inner class is this class
/// decides, and it is a member link exactly when its `outer_class_info_index`
/// is non-zero (`inner_is_member = true`). The simple name plays no part, so a
/// row with an outer class and no simple name is a member: `getDeclaringClass`,
/// `getEnclosingClass` and `isMemberClass` answer the outer class (after
/// `check_for_inner_class`), `getSimpleName` is `""` and `getCanonicalName`
/// is `"<outer canonical>."`. javac never writes such a row (an anonymous
/// class has no outer class); hand-written or generated bytes can.
///
/// With `any_outer_row` off ([`declaring_class_any_outer_row_enabled`] `=0`)
/// the old rule: the first self row with both an outer class and a simple
/// name, anywhere in the table.
pub fn declaring_member_row_outer<'a>(
    rows: impl IntoIterator<Item = (&'a str, &'a str, &'a str)>,
    this_name: &str,
    any_outer_row: bool,
) -> Option<&'a str> {
    let mut rows = rows.into_iter();
    if any_outer_row {
        rows.find(|(inner, _, _)| *inner == this_name)
            .and_then(|(_, outer, _)| (!outer.is_empty()).then_some(outer))
    } else {
        rows.find(|(inner, outer, name)| {
            *inner == this_name && !outer.is_empty() && !name.is_empty()
        })
        .map(|(_, outer, _)| outer)
    }
}

/// [`declaring_member_row_outer`] over the [`NativeContext::inner_classes`]
/// tuples, with this VM's switch. The one copy of the member rule: the
/// `Lookup.in` outermost walk (`classloader::lk_outermost_enclosing`) and the
/// `--compatible` `getEnclosingClass` bridge (`lang_reflect`) call it too
/// (wave 19; `r21w19_member_rule_has_one_copy` pins that).
pub fn member_row_outer_of(
    ctx: &dyn NativeContext,
    rows: &[(String, String, String, u16)],
    this_name: &str,
) -> Option<String> {
    declaring_member_row_outer(
        rows.iter()
            .map(|(i, o, n, _)| (i.as_str(), o.as_str(), n.as_str())),
        this_name,
        declaring_class_any_outer_row_enabled(ctx),
    )
    .map(str::to_string)
}

/// The per-VM memo of [`spoofed_member_disagreement`]'s verdicts
/// ([`ReflectDoorCaches::spoof_verdicts`]).
pub type SpoofVerdictMemo =
    cratonvm_types::lock_order::OrderedPlRwLock<FxHashMap<u64, Option<Arc<str>>>>;

/// Drop this VM's answers derived from `InnerClasses` / `EnclosingMethod`
/// after a redefinition of `class_id` (JIT round 21 wave 17, lane
/// innerclasses; page `r21-review-w16-findings-FIXED-20261020.md` item 5).
///
/// Since `CRATONVM_REDEFINE_SWAPS_INNER_CLASSES` a redefinition installs the
/// new bytes' tables, as HotSpot's `redefine_single_class` does, so these
/// memos can no longer assume the tables are fixed for the life of a class id:
/// - The spoof verdicts ([`ReflectDoorCaches::spoof_verdicts`]) are cleared
///   whole. A row keyed `(inner << 32) | SPOOF_BY_INNER` depends on the
///   DECLARING class's table, and its key does not name that class.
/// - The simple and canonical names: a member class's canonical name is built
///   from its outer class's, so every row of the VM goes. HotSpot keeps both in
///   `Class.reflectionData`, which a changed `classRedefinedCount` discards.
/// - The redefined class's modifiers row (a nested class's modifiers are its
///   `InnerClasses` row's flags).
///
/// A redefinition is rare, so whole-VM clears cost nothing that matters.
pub fn forget_redefined_inner_class_answers(ctx: &dyn NativeContext, class_id: ClassId) {
    if let Some(caches) = reflect_door_caches(ctx) {
        // Wave 19 (item 4): the generation moves BEFORE anything is cleared,
        // so a fill that read the old tables either sees it or is cleared
        // ([`InnerTablesFill`]).
        caches
            .inner_tables_generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        caches.spoof_verdicts.write().clear();
        // REVIEW22-2 (JIT r23 w1 L6): a member class's verdict reads its
        // outer class's table too, so the whole VM's verdicts go.
        caches.name_bridge_verdicts.write().clear();
        // The redefined class has a new constant pool: its recorded
        // resolution failures name entries of the old one (wave 19, item 7).
        caches
            .outer_resolution_failures
            .write()
            .retain(|(cid, _), _| *cid != class_id.as_u32());
    }
    let vm = ctx.vm_identity();
    for cache in [&SIMPLE_CLASS_NAME_CACHE, &CANONICAL_CLASS_NAME_CACHE] {
        cache_get_or_init(cache).write().retain(|(v, _), _| *v != vm);
    }
    if let Some(modifiers) = CLASS_MODIFIERS_CACHE.get() {
        modifiers
            .write()
            .remove(&(vm, class_id.as_u32()));
    }
}

/// The low half of a [`SpoofVerdictMemo`] key that stands for "the declaring
/// class the VM has loaded" (the bridges' question) rather than a given outer
/// class id. No class id reaches it.
pub const SPOOF_BY_INNER: u64 = 0xFFFF_FFFF;

/// The memoized verdict for `key`, or the result of `compute`, which is kept
/// when `keep` says so. The lock is never held across `compute` (it reads the
/// class store), so the memo stays a leaf. Contexts without a cache slot (the
/// mocks) compute every time.
pub fn spoof_verdict_memo(
    ctx: &dyn NativeContext,
    key: u64,
    compute: impl FnOnce() -> (Option<String>, bool),
) -> Option<String> {
    let Some(caches) = reflect_door_caches(ctx) else {
        return compute().0;
    };
    if let Some(hit) = caches.spoof_verdicts.read().get(&key) {
        return hit.as_deref().map(str::to_string);
    }
    // Wave 19 (item 4): a verdict computed across a redefinition's swap is
    // not kept ([`InnerTablesFill`]).
    let fill = inner_tables_fill_guard(ctx);
    let (verdict, keep) = compute();
    if keep && !fill.moved(ctx) {
        caches
            .spoof_verdicts
            .write()
            .insert(key, verdict.as_deref().map(Arc::from));
        if fill.moved(ctx) {
            caches.spoof_verdicts.write().remove(&key);
        }
    }
    verdict
}

/// HotSpot's `Reflection::check_for_inner_class` for a MEMBER link of an
/// ordinary (not hidden) class (JIT round 21 wave 14, lane innerclasses;
/// `CRATONVM_CLASS_INNER_CLASSES_SPOOF_CHECK`). `inner`'s own `InnerClasses`
/// self entry names `outer` as its outer class (the link
/// `compute_enclosing_class` follows with `inner_is_member`). The check
/// succeeds only when `outer`'s own `InnerClasses` has a row whose inner and
/// outer entries name `inner` and `outer`. Otherwise the answer is the ICCE
/// text `"<outer> and <inner> disagree on InnerClasses attribute"`, external
/// names. Two cases fail it: an outer class with no row for the inner class (a
/// spoofed member), and a row that names another outer class.
///
/// Measured, HotSpot 25 (`R21LookupInSpoofedMember.java`):
/// `getDeclaringClass`, `getEnclosingClass` and `Lookup.in` both ways throw
/// it for both cases.
///
/// Names only: the row's entries are compared by name, and that name resolves
/// to `outer` / `inner` through `outer`'s loader. That holds whenever the two
/// classes share a defining loader; the cross-loader identity question stays
/// [`inner_classes_disagreement`]'s own arms. Reads only, no load and no GC
/// point, so `classloader::lk_outermost_enclosing` may ask it.
///
/// `None` when the arm does not apply: a hidden class (its own arm), a class
/// with no self member entry naming `outer` (a link `getDeclaringClass0` does
/// not take), a class without real bytes on either side (by `ClassOrigin`,
/// [`spoofed_member_uncached`]), or the switches off.
///
/// Memoized per VM per `(inner, outer)` pair: both tables are the classes'
/// parsed attributes. A redefinition can replace them (wave 17,
/// `CRATONVM_REDEFINE_SWAPS_INNER_CLASSES`), and it clears this memo
/// ([`forget_redefined_inner_class_answers`], called by the VM after every
/// redefinition).
pub fn spoofed_member_disagreement(
    ctx: &dyn NativeContext,
    inner: ClassId,
    outer: ClassId,
) -> Option<String> {
    if !inner_classes_spoof_check_enabled(ctx) {
        return None;
    }
    let key = (u64::from(inner.as_u32()) << 32) | u64::from(outer.as_u32());
    // `CRATONVM_CLASS_INNER_CLASSES_CHECK` is asked on a miss only: it is
    // process-stable, so a kept verdict already reflects it.
    spoof_verdict_memo(ctx, key, || {
        if !class_inner_classes_check_enabled() {
            return (None, true);
        }
        // A provenance exemption is not kept (wave 15 review, rev-w14-native
        // item 3): `upgrade_synthetic_class` gives a stub real bytes, and its
        // parsed `InnerClasses`, under the same class id.
        let keep = spoofed_member_pair_settled(ctx, inner, outer);
        (spoofed_member_uncached(ctx, inner, outer), keep)
    })
}

/// May a [`spoofed_member_uncached`] verdict for the pair be memoized? Only
/// once real bytes back both classes: a `CompatibilityStub` is upgraded in
/// place to real bytes (`ClassManager::upgrade_synthetic_class`, same id, new
/// `InnerClasses`), so an exemption by provenance can change. A hidden inner
/// class stays exempt for life (wave 15 review, rev-w14-native item 3).
pub fn spoofed_member_pair_settled(ctx: &dyn NativeContext, inner: ClassId, outer: ClassId) -> bool {
    ctx.is_class_hidden(inner) || (ctx.class_has_real_bytes(inner) && ctx.class_has_real_bytes(outer))
}

/// [`spoofed_member_disagreement`] without the memo or the switches.
///
/// The exemption is by provenance, never by name: a pair is asked only when
/// real class bytes back both classes (`NativeContext::class_has_real_bytes`,
/// the recorded `ClassOrigin`). A VM-created or fabricated class
/// (`CompatibilityStub`, `VmInternal`, generated lambdas, proxies and
/// accessors, arrays) has no class file, so it has no `InnerClasses`
/// attribute HotSpot could check. Real JDK bytes are asked like any others:
/// javac's tables are consistent, as HotSpot's own check shows on every
/// `getDeclaringClass` it serves.
pub fn spoofed_member_uncached(ctx: &dyn NativeContext, inner: ClassId, outer: ClassId) -> Option<String> {
    if ctx.is_class_hidden(inner)
        || !ctx.class_has_real_bytes(inner)
        || !ctx.class_has_real_bytes(outer)
    {
        return None;
    }
    let inner_name = ctx.class_name_of_id(inner)?;
    let outer_name = ctx.class_name_of_id(outer)?;
    spoofed_member_text(
        &inner_name,
        &outer_name,
        &ctx.inner_classes(inner),
        &ctx.inner_classes(outer),
    )
}

/// The decision of [`spoofed_member_disagreement`] over the two classes'
/// names and their `InnerClasses` rows `(inner, outer, simple name, flags)`.
pub fn spoofed_member_text(
    inner_name: &str,
    outer_name: &str,
    inner_rows: &[(String, String, String, u16)],
    outer_rows: &[(String, String, String, u16)],
) -> Option<String> {
    let member_link = inner_rows
        .iter()
        .any(|(i, o, _, _)| i == inner_name && o == outer_name);
    if !member_link {
        return None;
    }
    let listed = outer_rows
        .iter()
        .any(|(i, o, _, _)| i == inner_name && o == outer_name);
    (!listed).then(|| {
        format!(
            "{} and {} disagree on InnerClasses attribute",
            outer_name.replace('/', "."),
            inner_name.replace('/', ".")
        )
    })
}

/// [`spoofed_member_disagreement`] for `class_id` against the declaring class
/// the VM already has loaded (`NativeContext::declaring_class`, a lookup that
/// never loads), for the `--compatible` name and enclosing-class bridges while
/// no user-defined loader exists. Those bridges used to skip the question
/// entirely in that state. An outer class that is not loaded yet answers
/// `None`, as before (JIT round 21 wave 14, lane innerclasses).
///
/// Memoized per VM by the inner class alone, once its declaring class
/// resolved, so a bridge's repeat call is one hash lookup. An unresolved
/// outer class is not memoized: it can load later.
pub fn spoofed_member_without_loaders(ctx: &dyn NativeContext, class_id: ClassId) -> Option<String> {
    if !inner_classes_spoof_check_enabled(ctx) {
        return None;
    }
    let key = (u64::from(class_id.as_u32()) << 32) | SPOOF_BY_INNER;
    spoof_verdict_memo(ctx, key, || {
        if !class_inner_classes_check_enabled() {
            return (None, true);
        }
        // A stub can be upgraded to real bytes under the same id (wave 15
        // review, rev-w14-native item 3): its exemption is asked again.
        if !ctx.class_has_real_bytes(class_id) {
            return (None, false);
        }
        match ctx.declaring_class(class_id) {
            Some(outer) => (
                spoofed_member_uncached(ctx, class_id, outer),
                spoofed_member_pair_settled(ctx, class_id, outer),
            ),
            // Not a member class at all (no self entry naming an outer
            // class): fixed, kept. A member whose outer class is not loaded
            // yet: asked again next time.
            None => {
                // The member rule is `NativeContext::declaring_class`'s own
                // (wave 16, lane innerclasses: any self row with an outer
                // class, `CRATONVM_DECLARING_CLASS_ANY_OUTER_ROW`).
                let name = ctx.class_name_of_id(class_id);
                let member = name.is_some_and(|name| {
                    member_row_outer_of(ctx, &ctx.inner_classes(class_id), &name).is_some()
                });
                (None, !member)
            }
        }
    })
}

/// The `IncompatibleClassChangeError` message `Class.getSimpleName()` would
/// raise for `class_id` (an array's: its element's), or `None`: its declaring
/// class as `getDeclaringClass0` resolves it, then
/// [`inner_classes_disagreement`]. For the `MethodHandle` doors, which build
/// `MethodType.toString()` in Rust (JIT round 17 wave 8, lane mh7). Cheap for
/// every class no user-defined loader is involved with.
pub fn simple_name_inner_classes_disagreement(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
) -> Option<String> {
    let mut id = class_id;
    // 255 is the JVMS dimension limit.
    for _ in 0..256 {
        match ctx.array_component_class_id(id) {
            Some(component) => id = component,
            None => break,
        }
    }
    // A hidden member class disagrees with any loaders (wave 102, lane L3).
    // With no user-defined loader, only the spoofed-member arm is asked, over
    // already-loaded classes (JIT round 21 wave 14, lane innerclasses:
    // [`spoofed_member_without_loaders`]; no load, no GC point).
    if !ctx.is_class_hidden(id)
        && !crate::classloader::any_defining_loader_registered(crate::classloader::loader_tables(ctx))
    {
        return spoofed_member_without_loaders(&*ctx, id);
    }
    let outer = declaring_class_id_of(ctx, id)?;
    inner_classes_disagreement(ctx, id, outer)
}

/// What [`name_bridge_verdict_probe`] found: whether the bridge may skip its
/// checks, and what [`name_bridge_note_passed`] records once they pass.
#[derive(Clone, Copy)]
pub struct NameBridgeVerdict {
    /// The class to record under; `None` = nothing to memoize (no class id,
    /// no cache slot, or the switch off).
    pub class_id: Option<ClassId>,
    pub bit: u8,
    pub passed: bool,
    pub fill: InnerTablesFill,
}

/// The [`ReflectDoorCaches::name_bridge_verdicts`] bit of one bridge
/// (`canonical`) under one user-loader state (`loaders`: any user-defined
/// loader registered). The checks a bridge runs differ between the two loader
/// states ([`inner_classes_icce_for_mirror`] asks only the spoofed-member arm
/// while there is none, [`name_bridge_outer_resolution_error`] only then), and
/// the state goes from "none" to "some" once, so a verdict of the first state
/// is never read in the second.
pub fn name_bridge_verdict_bit(canonical: bool, loaders: bool) -> u8 {
    (if canonical { 0b10 } else { 0b01 }) << if loaders { 2 } else { 0 }
}

/// JIT round 23 wave 1, lane L6 (REVIEW22-2 of `jit-r22-review-proposals.md`):
/// may the `--compatible` `getSimpleName` (`canonical` false) /
/// `getCanonicalName` (`canonical` true) bridge of the class `mirror` names skip
/// its pre-checks ([`name_bridge_asks_declaring_class`], the ICCE / NCDFE
/// arms), because they passed for this class before?
///
/// Every check answers from data that does not change once it has passed: the
/// `InnerClasses` / `EnclosingMethod` tables (replaced only by a redefinition,
/// which clears the memo in [`forget_redefined_inner_class_answers`] and moves
/// the generation a fill is guarded by), the outer / EnclosingMethod class's
/// resolution (HotSpot's: once resolved, it stays resolved), and the
/// user-loader state (part of the key, [`name_bridge_verdict_bit`]). A failing
/// check is never memoized: the bridge throws a new error each call, as before.
/// Only a class with real bytes (or a hidden one) is memoized, for the reason
/// [`spoofed_member_pair_settled`] gives: a stand-in's tables can be upgraded in
/// place under the same id. An unload drops the row
/// ([`forget_unloaded_reflect_door_rows`]).
///
/// The point is the cost the review measured: one `read_recursive` and one
/// `String` clone per call before the name caches were reached.
/// `CRATONVM_COMPAT_NAME_BRIDGE_VERDICT_MEMO=0` asks every time, as before.
pub fn name_bridge_verdict_probe(
    ctx: &dyn NativeContext,
    mirror: ObjectRef,
    canonical: bool,
) -> NameBridgeVerdict {
    let miss = NameBridgeVerdict {
        class_id: None,
        bit: 0,
        passed: false,
        fill: InnerTablesFill(None),
    };
    let Some(caches) = reflect_door_caches(ctx) else {
        return miss;
    };
    if !caches.name_bridge_verdict_memo {
        return miss;
    }
    let Some(class_id) = ctx.class_id_from_mirror(mirror) else {
        return miss;
    };
    let loaders =
        crate::classloader::any_defining_loader_registered(crate::classloader::loader_tables(ctx));
    let bit = name_bridge_verdict_bit(canonical, loaders);
    if name_bridge_verdicts_hold(&caches.name_bridge_verdicts, class_id, bit) {
        return NameBridgeVerdict {
            class_id: Some(class_id),
            bit,
            passed: true,
            fill: InnerTablesFill(None),
        };
    }
    // The guard is taken BEFORE the checks read any table ([`InnerTablesFill`]).
    NameBridgeVerdict {
        class_id: Some(class_id),
        bit,
        passed: false,
        fill: inner_tables_fill_guard(ctx),
    }
}

/// Record that the checks [`name_bridge_verdict_probe`] did not skip passed.
pub fn name_bridge_note_passed(ctx: &dyn NativeContext, verdict: NameBridgeVerdict) {
    let Some(class_id) = verdict.class_id else {
        return;
    };
    let Some(caches) = reflect_door_caches(ctx) else {
        return;
    };
    let mut element = class_id;
    // 255 is the JVMS dimension limit.
    for _ in 0..256 {
        match ctx.array_component_class_id(element) {
            Some(component) => element = component,
            None => break,
        }
    }
    let hidden = ctx.is_class_hidden(element);
    if !(hidden || ctx.class_has_real_bytes(element)) {
        return;
    }
    // With no user-defined loader the ICCE arm is
    // [`spoofed_member_without_loaders`], which asks over already-loaded classes
    // only and does not keep its verdict while a member's outer class is not
    // loaded: such a pass is not settled, so it is not memoized either. Its
    // kept verdict (a `None`: the check passed) is the proof.
    if verdict.bit & 0b11 != 0 && !hidden && inner_classes_spoof_check_enabled(ctx) {
        let key = (u64::from(element.as_u32()) << 32) | SPOOF_BY_INNER;
        if !caches.spoof_verdicts.read().contains_key(&key) {
            return;
        }
    }
    if verdict.fill.moved(ctx) {
        return;
    }
    name_bridge_verdicts_insert(&caches.name_bridge_verdicts, class_id, verdict.bit);
    if verdict.fill.moved(ctx) {
        name_bridge_verdicts_forget(&caches.name_bridge_verdicts, class_id);
    }
}

pub fn name_bridge_verdicts_hold(memo: &NameBridgeVerdicts, class_id: ClassId, bit: u8) -> bool {
    memo.read()
        .get(&class_id.as_u32())
        .is_some_and(|bits| bits & bit != 0)
}

pub fn name_bridge_verdicts_insert(memo: &NameBridgeVerdicts, class_id: ClassId, bit: u8) {
    *memo.write().entry(class_id.as_u32()).or_insert(0) |= bit;
}

pub fn name_bridge_verdicts_forget(memo: &NameBridgeVerdicts, class_id: ClassId) {
    memo.write().remove(&class_id.as_u32());
}

/// Does JDK 25's `getSimpleName()` (`canonical` false) or
/// `getCanonicalName()` (`canonical` true) of the class `mirror` names reach
/// `getDeclaringClass0`, and so its ICCE / outer-resolution checks? JIT round
/// 21 wave 21, lane innerclasses (page `r21-review-w21-findings-FIXED-20261020.md`
/// item 7; `CRATONVM_COMPAT_NESTING_FOLLOWS_ENCLOSING_METHOD`, whose off arm
/// answers `true` as before).
///
/// An array asks for its element (`getComponentType().getSimpleName()` /
/// `getCanonicalName()`). Then:
/// - `getSimpleName0` -> `getSimpleBinaryName` -> `isTopLevelClass`, which is
///   `!isLocalOrAnonymousClass() && getDeclaringClass0() == null`. With
///   `EnclosingMethod` (`isLocalOrAnonymousClass` is `hasEnclosingMethodInfo`)
///   the `&&` short-circuits.
/// - `getCanonicalName0` answers null for `isHidden() ||
///   isLocalOrAnonymousClass()` before `getEnclosingClass`.
///
/// So a class with `EnclosingMethod`, and for the canonical name also a hidden
/// class, is never asked. HotSpot throws the ICCE of `R21Fz7InnerRowsEm1`'s X$7 /
/// X$8 and the NCDFE of its X$9 only from `getDeclaringClass`.
pub fn name_bridge_asks_declaring_class(
    ctx: &mut dyn NativeContext,
    mirror: ObjectRef,
    canonical: bool,
) -> bool {
    if !nesting_follows_enclosing_method_enabled(&*ctx) {
        return true;
    }
    let Some(mut element) = mirror_class_id(ctx, mirror) else {
        return true;
    };
    // 255 is the JVMS dimension limit.
    for _ in 0..256 {
        match ctx.array_component_class_id(element) {
            Some(component) => element = component,
            None => break,
        }
    }
    if canonical && ctx.is_class_hidden(element) {
        return false;
    }
    // One `String`, not three (wave 22, lane hidden-resid; item 4 of
    // `r21-review-w22-findings-FIXED-20261020.md`).
    ctx.enclosing_method_class_name(element).is_none()
}

/// The EnclosingMethod class's resolution error that JDK 25's
/// `getSimpleName()` / `getCanonicalName()` throw for a class the
/// declaring-class checks skip ([`name_bridge_asks_declaring_class`] said
/// `false`): `isLocalOrAnonymousClass()` is `hasEnclosingMethodInfo()`, whose
/// `getEnclosingMethod0` (HotSpot `JVM_GetEnclosingMethodInfo`) resolves the
/// `EnclosingMethod` class with `klass_at(.., CHECK_NULL)` before the `&&`
/// of `isTopLevelClass` can short-circuit. So a missing EnclosingMethod class
/// is a `NoClassDefFoundError` (caused by `ClassNotFoundException`) out of
/// both bridges, with or without an `InnerClasses` self row. A hidden class
/// in the canonical bridge is answered null first (`isHidden()`), so it is
/// never resolved there.
///
/// JIT round 21 wave 22, lane hidden-resid (item 1 of
/// `r21-review-w22-findings-FIXED-20261020.md`, the regression of wave 21's item 7),
/// under `CRATONVM_COMPAT_NESTING_FOLLOWS_ENCLOSING_METHOD`, whose `=0` arm
/// asks the declaring class instead and never reaches this. Through the
/// recorded door ([`class_seen_by_referencing_class_checked`]), so a later
/// ask rethrows the same error, as HotSpot's resolution-error table does.
/// Same exemptions as the outer-class errors
/// ([`outer_resolution_errors_apply`]: JDK classes keep the old answer), and,
/// as [`name_bridge_outer_resolution_error`], not asked once `cache` holds
/// the class's name.
pub fn name_bridge_enclosing_method_error(
    ctx: &mut dyn NativeContext,
    mirror: ObjectRef,
    canonical: bool,
    cache: &ClassNameCache,
) -> Option<MethodCallFailed> {
    if let Some(class_id) = ctx.class_id_from_mirror(mirror) {
        if cache_get(cache, ctx.vm_identity(), class_id).is_some() {
            return None;
        }
    }
    let mut element = mirror_class_id(ctx, mirror)?;
    // 255 is the JVMS dimension limit.
    for _ in 0..256 {
        match ctx.array_component_class_id(element) {
            Some(component) => element = component,
            None => break,
        }
    }
    if canonical && ctx.is_class_hidden(element) {
        return None;
    }
    let em_class = ctx.enclosing_method_class_name(element)?;
    if em_class.is_empty() || !outer_resolution_errors_apply(ctx, element) {
        return None;
    }
    class_seen_by_referencing_class_checked(ctx, element, &em_class).err()
}

/// The thrown `IncompatibleClassChangeError` of
/// [`simple_name_inner_classes_disagreement`] for the class `mirror` names,
/// or `None`. For the `getSimpleName` / `getCanonicalName` bridges
/// `--compatible` dispatches (JIT round 17 wave 8, lane mh7, second pass).
pub fn inner_classes_icce_for_mirror(
    ctx: &mut dyn NativeContext,
    mirror: ObjectRef,
) -> Option<cratonvm_types::error::MethodCallFailed> {
    let class_id = mirror_class_id(ctx, mirror)?;
    // JIT round 18 wave 11 (lane mhresid; MHLOADER18-4): `getSimpleName` /
    // `getCanonicalName` reach `getDeclaringClass0` in the JDK body, whose
    // outer-class resolution error is thrown before the consistency check.
    // [`simple_name_inner_classes_disagreement`] inlined, so the outer class
    // is looked up once.
    let mut element = class_id;
    // 255 is the JVMS dimension limit.
    for _ in 0..256 {
        match ctx.array_component_class_id(element) {
            Some(component) => element = component,
            None => break,
        }
    }
    // A hidden member class disagrees with any loaders (wave 102, lane L3).
    // With no user-defined loader, only the spoofed-member arm is asked, over
    // already-loaded classes (JIT round 21 wave 14, lane innerclasses:
    // [`spoofed_member_without_loaders`]; no load, no GC point).
    if !ctx.is_class_hidden(element)
        && !crate::classloader::any_defining_loader_registered(crate::classloader::loader_tables(ctx))
    {
        let message = spoofed_member_without_loaders(&*ctx, element)?;
        return Some(crate::phases_early::throw_jca_exc(
            ctx,
            "java/lang/IncompatibleClassChangeError",
            &message,
        ));
    }
    let outer = if outer_resolution_errors_apply(ctx, element) {
        match declaring_class_id_checked(ctx, element, true) {
            Ok(outer) => outer?,
            Err(failed) => return Some(failed),
        }
    } else {
        declaring_class_id_of(ctx, element)?
    };
    let message = inner_classes_disagreement(ctx, element, outer)?;
    Some(crate::phases_early::throw_jca_exc(
        ctx,
        "java/lang/IncompatibleClassChangeError",
        &message,
    ))
}

/// `CRATONVM_CLASS_NAME_BRIDGE_OUTER_ERRORS` -- correctness, default ON, `=0`
/// restores round 18 wave 11's first answer (JIT round 18 wave 11, lane
/// mhresid; MHRESID18-3): see [`name_bridge_outer_resolution_error`]. Read
/// only on a name-cache miss while no user-defined loader exists.
pub fn class_name_bridge_outer_errors_enabled() -> bool {
    true
}

/// The outer / enclosing class's resolution error (HotSpot's
/// `NoClassDefFoundError` out of `getDeclaringClass0` / `getEnclosingMethod0`)
/// that the `--compatible` `getSimpleName` / `getCanonicalName` bridges throw
/// while NO user-defined loader exists (JIT round 18 wave 11, lane mhresid;
/// MHRESID18-3): a nested, local or anonymous class whose outer class is
/// missing from the application class path. With a user-defined loader
/// [`inner_classes_icce_for_mirror`] asks the same question first.
///
/// Asked only when `cache` (the bridge's own per-class name cache) has no
/// answer for the class: HotSpot caches the simple name only on success, and
/// a resolution that once succeeded keeps succeeding, so a cached name means
/// nothing to throw. Same exemptions as the other paths
/// ([`outer_resolution_errors_apply`]: JDK classes keep the old answer).
pub fn name_bridge_outer_resolution_error(
    ctx: &mut dyn NativeContext,
    mirror: ObjectRef,
    cache: &ClassNameCache,
) -> Option<MethodCallFailed> {
    if crate::classloader::any_defining_loader_registered(crate::classloader::loader_tables(ctx))
        || !class_name_bridge_outer_errors_enabled()
    {
        return None;
    }
    let class_id = ctx.class_id_from_mirror(mirror)?;
    if cache_get(cache, ctx.vm_identity(), class_id).is_some() {
        return None;
    }
    let mut element = class_id;
    // 255 is the JVMS dimension limit.
    for _ in 0..256 {
        match ctx.array_component_class_id(element) {
            Some(component) => element = component,
            None => break,
        }
    }
    if !outer_resolution_errors_apply(ctx, element) {
        return None;
    }
    declaring_class_id_checked(ctx, element, true).err()
}

/// [`inner_classes_disagreement`] against the declaring class
/// `getDeclaringClass0` answers for `class_id` (no array unwrapping: an
/// array has no declaring class), as a thrown error. For the
/// `getEnclosingClass` bridge (`lang_reflect.rs`; JIT round 17 wave 8, lane
/// mh7, second pass).
pub fn declaring_class_icce(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
) -> Option<cratonvm_types::error::MethodCallFailed> {
    if ctx.array_component_class_id(class_id).is_some() {
        return None;
    }
    // JIT round 21 wave 14, lane innerclasses: with no user-defined loader,
    // only the spoofed-member arm, over already-loaded classes
    // ([`spoofed_member_without_loaders`]; no load, no GC point).
    if !crate::classloader::any_defining_loader_registered(crate::classloader::loader_tables(ctx)) {
        let message = spoofed_member_without_loaders(&*ctx, class_id)?;
        return Some(crate::phases_early::throw_jca_exc(
            ctx,
            "java/lang/IncompatibleClassChangeError",
            &message,
        ));
    }
    let outer = declaring_class_id_of(ctx, class_id)?;
    let message = inner_classes_disagreement(ctx, class_id, outer)?;
    Some(crate::phases_early::throw_jca_exc(
        ctx,
        "java/lang/IncompatibleClassChangeError",
        &message,
    ))
}

/// The declaring class `getDeclaringClass0` answers for `class_id`, before
/// HotSpot's consistency check (`None` for a top-level, local or anonymous
/// class). Split out of [`native_class_get_declaring_class`] by JIT round 17
/// wave 8, lane mh7, unchanged. A resolution failure is `None` here (the
/// ICCE checks' reader); `getDeclaringClass0` itself asks
/// [`declaring_class_id_checked`].
pub fn declaring_class_id_of(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
) -> Option<cratonvm_types::ClassId> {
    declaring_class_id_checked(ctx, class_id, false)
        .ok()
        .flatten()
}

/// `CRATONVM_CLASS_OUTER_RESOLUTION_ERRORS` -- correctness, default ON, `=0`
/// restores the old answers (JIT round 18 wave 11, lane mhresid; proposal
/// MHLOADER18-4 of `jit-r18-mhloader-proposals.md`). HotSpot resolves a
/// nested class's outer class (`JVM_GetDeclaringClass` ->
/// `compute_enclosing_class`) and a local or anonymous class's enclosing class
/// (`JVM_GetEnclosingMethodInfo`, and `compute_enclosing_class` for a local
/// class's declaring-class query) with `klass_at` and `CHECK_NULL`: a name the
/// class's own loader cannot load is that resolution's `NoClassDefFoundError`
/// (or other `LinkageError`), thrown out of `getDeclaringClass0` /
/// `getEnclosingMethod0` and so out of `getSimpleName`, `getDeclaringClass`,
/// `getEnclosingClass`, `getEnclosingMethod`, `isMemberClass`,
/// `getCanonicalName`. CratonVM answered `null` (or another loader's copy of
/// the name). Read only on a lookup that did not find the class through the
/// class's own loader.
pub fn class_outer_resolution_errors_enabled() -> bool {
    true
}

/// Whether the outer / enclosing class lookups for `class_id` throw their
/// resolution error ([`class_outer_resolution_errors_enabled`]): also needs
/// `CRATONVM_CLASS_OUTER_VIA_OWN_LOADER` (the resolution through the class's
/// own constant pool, whose error this is), and not a JDK class (the VM's
/// image and its synthetic stand-ins can lack an outer class HotSpot's image
/// has; the old answer stays there).
pub fn outer_resolution_errors_apply(
    ctx: &dyn NativeContext,
    class_id: cratonvm_types::ClassId,
) -> bool {
    if !class_outer_resolution_errors_enabled() || !class_outer_via_own_loader_enabled() {
        return false;
    }
    match ctx.class_name_of_id(class_id) {
        Some(name) => !["java/", "javax/", "jdk/", "sun/", "com/sun/"]
            .iter()
            .any(|p| name.starts_with(p)),
        None => false,
    }
}

/// [`declaring_class_id_of`] that, with `propagate` and
/// [`outer_resolution_errors_apply`], answers the outer class's resolution
/// error (`Err`) where HotSpot's `compute_enclosing_class` throws it: a
/// member class whose outer name its own constant pool cannot resolve, and a
/// local or anonymous class (an `InnerClasses` self entry without an outer
/// class) whose `EnclosingMethod` class it cannot resolve. Every success answers exactly
/// what [`declaring_class_id_of`] answers (JIT round 18 wave 11, lane
/// mhresid).
pub fn declaring_class_id_checked(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
    propagate: bool,
) -> Result<Option<cratonvm_types::ClassId>, MethodCallFailed> {
    // Loader-aware path FIRST: a nested class redefined under an isolating
    // loader must resolve ITS OWN loader's copy of its enclosing class, not
    // whichever loader's copy of the same-named outer class the flat global
    // store happens to already hold (see `declaring_class_loader_aware`'s
    // doc comment). Only consulted for classes with a registered, eligible
    // defining loader -- every ordinary class falls straight through to the
    // unchanged fast/slow paths below.
    if let Some(outer_id) = declaring_class_loader_aware(ctx, class_id) {
        return Ok(Some(outer_id));
    }

    // Fast path: the enclosing class is already loaded, so the VM's
    // `find_class_by_name`-backed `declaring_class` resolves it directly.
    let candidate = ctx.declaring_class(class_id);
    // JIT round 18 wave 9, lane mhloader
    // (`r17-class-declaring-class-resolves-outer-globally-FIXED-20261006.md`): a
    // nested class a user-defined loader defines resolves its outer name
    // through ITS OWN constant pool (HotSpot `compute_enclosing_class`'s
    // `klass_at`), not the first-loaded copy. The flat lookup above is kept
    // when it already answers the nested class's own loader's class.
    //
    // JIT round 18 wave 11 (lane mhresid): when that resolution FAILS, the
    // flat answer is another loader's copy HotSpot never sees; with
    // `propagate` the failure is the answer.
    match declaring_class_via_own_loader(ctx, class_id, candidate) {
        Ok(Some(outer_id)) => return Ok(Some(outer_id)),
        Err(failed) if propagate && outer_resolution_errors_apply(ctx, class_id) => {
            return Err(failed)
        }
        _ => {}
    }
    if let Some(outer_id) = candidate {
        return Ok(Some(outer_id));
    }

    // Slow path (SBR-07): the enclosing class is NOT yet loaded. This is the
    // common shape for `Class.forName("Pkg.Outer$Inner")` where the inner class
    // is resolved by name without ever referencing `Outer` вЂ” e.g. Kotlin's
    // protobuf-generated `ProtoBuf$StringTable`. `Vm::declaring_class` resolves
    // the outer-class name through `find_class_by_name`, which only sees
    // already-loaded classes, so it returns `None` and the real-JDK
    // `getSimpleName()`/`getCanonicalName()` bytecode then mistakes the class
    // for a top-level type (yielding `Outer$Inner` instead of `Inner`). HotSpot
    // *loads* the enclosing class here; mirror that by walking this class's own
    // `InnerClasses` attribute for the entry naming itself and loading the
    // recorded outer class (same resolve-then-load pattern as
    // `getDeclaredClasses0`). A non-member class (anonymous/local вЂ” empty
    // `outer_class`/`inner_name`) has no such entry and correctly stays `null`.
    let Some(class_name) = ctx.class_name_of_id(class_id) else {
        return Ok(None);
    };
    let inner_classes = ctx.inner_classes(class_id);
    // JIT round 21 wave 16, lane innerclasses
    // (`CRATONVM_DECLARING_CLASS_ANY_OUTER_ROW`): the first self row decides,
    // and any outer class makes it a member link, named or not
    // ([`declaring_member_row_outer`]).
    let any_outer_row = declaring_class_any_outer_row_enabled(&*ctx);
    for (inner_class, outer_class, inner_name, _flags) in &inner_classes {
        let member_row = !outer_class.is_empty() && (any_outer_row || !inner_name.is_empty());
        if inner_class == &class_name && member_row {
            // JIT round 21 wave 26, lane reserr
            // (`CRATONVM_DECLARING_LOADER_AWARE_VIA_ENTRY`): an entry whose
            // resolution already failed is not loaded again (JVMS 5.4.3); with
            // `propagate` the recorded error answers below.
            let failed_before = declaring_loader_aware_via_entry_enabled()
                && outer_resolution_failure_is_recorded(&*ctx, class_id, outer_class);
            let outer_id = match ctx.class_id_by_name(outer_class) {
                Some(id) => Some(id),
                None if failed_before => None,
                None => match ctx.load_class(outer_class) {
                    Ok(_) => ctx.class_id_by_name(outer_class),
                    Err(_) => None,
                },
            };
            if outer_id.is_some() {
                return Ok(outer_id);
            }
            // Found the member entry but couldn't load the outer class. HotSpot
            // does not suppress this (`klass_at(ooff, CHECK_NULL)`): with
            // `propagate`, the class's own constant-pool resolution answers,
            // and its error is thrown (round 18 wave 11, lane mhresid).
            if propagate && outer_resolution_errors_apply(ctx, class_id) {
                return class_seen_by_referencing_class_checked(ctx, class_id, outer_class)
                    .map(Some);
            }
            break;
        }
        if propagate
            && inner_class == &class_name
            && outer_class.is_empty()
            && outer_resolution_errors_apply(ctx, class_id)
        {
            // A LOCAL or ANONYMOUS class: `compute_enclosing_class` resolves its
            // `EnclosingMethod` class (`klass_at`, `CHECK_NULL`) and then
            // answers no declaring class. Only the error is taken here.
            if let Some((enclosing, _, _)) = ctx.enclosing_method(class_id) {
                class_seen_by_referencing_class_checked(ctx, class_id, &enclosing)?;
            }
            break;
        }
        if any_outer_row && inner_class == &class_name {
            // Not a member link, and the first self row decides.
            break;
        }
    }
    Ok(None)
}

/// `CRATONVM_CLASS_OUTER_VIA_OWN_LOADER` -- correctness, default ON, `=0`
/// restores the flat lookups (JIT round 18 wave 9, lane mhloader;
/// `r17-class-declaring-class-resolves-outer-globally-FIXED-20261006.md`). A nested
/// or local class's outer / enclosing class is resolved the way its own
/// constant pool resolves the name (HotSpot `klass_at`: through the class's
/// defining loader, loading but not initializing): `getDeclaringClass0`
/// ([`declaring_class_id_of`]), `getEnclosingMethod0` and the
/// `getEnclosingClass` / `getEnclosingMethod` bridges
/// (`lang_reflect::loader_scoped_enclosing_lookup`).
pub fn class_outer_via_own_loader_enabled() -> bool {
    true
}

/// `CRATONVM_DECLARING_LOADER_AWARE_VIA_ENTRY` -- correctness, default ON,
/// `=0` restores the bare `loadClass` of `declaring_class_loader_aware` and
/// the unguarded slow-path load of [`declaring_class_id_checked`] (JIT round
/// 21 wave 26, lane reserr). HotSpot resolves a member class's outer class
/// through the class's constant pool (`compute_enclosing_class` ->
/// `klass_at`), and JVMS 5.4.3 makes a failed resolution of that entry final:
/// every later `getDeclaringClass0` / `getEnclosingClass` / `getSimpleName` /
/// `Lookup.in` rethrows the recorded error without asking the loader again
/// (`R21Fz6LookupInLoad1`: `asks(Gone)=1` throughout). Both loader-asking
/// steps that ran before the recorded door now go through it, or skip their
/// load once the entry has failed. Read on a declaring-class question of a
/// class with a user-defined loader only.
pub fn declaring_loader_aware_via_entry_enabled() -> bool {
    true
}

/// Has [`class_seen_by_referencing_class_checked`] recorded a failed
/// resolution of `name` for `referencing` (JVMS 5.4.3)? One read guard; no
/// allocation, no Java. JIT round 21 wave 26, lane reserr.
pub fn outer_resolution_failure_is_recorded(
    ctx: &dyn NativeContext,
    referencing: cratonvm_types::ClassId,
    name: &str,
) -> bool {
    reflect_door_caches(ctx).is_some_and(|c| failure_recorded_in(c, referencing.as_u32(), name))
}

/// [`outer_resolution_failure_is_recorded`] over one VM's caches.
pub fn failure_recorded_in(caches: &ReflectDoorCaches, referencing: u32, name: &str) -> bool {
    if !caches.outer_resolution_failure_sticky {
        return false;
    }
    let failures = caches.outer_resolution_failures.read();
    !failures.is_empty() && failures.contains_key(&(referencing, Box::from(name)))
}

/// Is `class_id` defined by a user-defined loader, by either witness (the
/// class manager's loader id, or the defining-loader side table, which the
/// class manager's id can lag for a forked loader)?
pub fn class_has_user_defining_loader(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
) -> bool {
    ctx.loader_id_of_class(class_id) >= 3
        || crate::classloader::defining_loader_for(crate::classloader::loader_tables(ctx), class_id.as_u32()).is_some()
}

/// The class `referencing`'s constant pool resolves `name` to: what its
/// loader already answered (no Java), else the constant-pool door itself
/// (`class_id_by_name_via_referencing_class`, which drives the defining
/// loader's `loadClass` and does not initialize). `Err` is that door's
/// resolution error; the callers decide whether to throw it or keep their
/// previous fallback (JIT round 18 wave 11, lane mhresid: this was the
/// `Option` reader `class_seen_by_referencing_class`).
pub fn class_seen_by_referencing_class_checked(
    ctx: &mut dyn NativeContext,
    referencing: cratonvm_types::ClassId,
    name: &str,
) -> Result<cratonvm_types::ClassId, MethodCallFailed> {
    // JIT round 21 wave 19, lane innerclasses
    // (`CRATONVM_OUTER_RESOLUTION_FAILURE_STICKY`; page
    // `r21-review-w18-findings-FIXED-20261020.md` item 7): every caller resolves an
    // `InnerClasses` or `EnclosingMethod` class entry, which HotSpot does with
    // `klass_at`. JVMS 5.4.3: a failed resolution is recorded against the
    // entry and every later attempt throws the same error again, without
    // asking any loader, even once the class could be found
    // (`R21Fz6LookupInLoad1`: `asks(Gone)` stays 1).
    //
    // Wave 21 (page `r21-review-w21-findings-FIXED-20261020.md` item 6b): only the
    // `klass_at` callers come here. A record component's descriptor type is
    // resolved per call by HotSpot (`find_java_mirror_for_type`), with no
    // resolution error recorded, through
    // [`class_seen_by_referencing_class_unrecorded`].
    if let Some(recorded) = recorded_outer_resolution_failure(ctx, referencing, name) {
        return Err(recorded);
    }
    class_seen_by_referencing_class_unrecorded(ctx, referencing, name)
        .map_err(|failed| record_outer_resolution_failure(ctx, referencing, name, failed))
}

/// [`class_seen_by_referencing_class_checked`] without the JVMS 5.4.3 memo: a
/// resolution that is not a constant-pool entry's (a record component's
/// descriptor type, `getRecordComponents`), asked afresh every time and never
/// recorded (JIT round 21 wave 21, lane innerclasses).
pub fn class_seen_by_referencing_class_unrecorded(
    ctx: &mut dyn NativeContext,
    referencing: cratonvm_types::ClassId,
    name: &str,
) -> Result<cratonvm_types::ClassId, MethodCallFailed> {
    if let Some(id) = ctx.class_id_seen_by_class_without_loading(name, referencing) {
        return Ok(id);
    }
    ctx.class_id_by_name_via_referencing_class(referencing, name)
        .map_err(class_not_found_is_no_class_def_found)
}

/// A built-in loader's lookup miss comes back from the resolver as the VM's
/// own `ClassFileError::ClassNotFound`, an internal error that no Java handler
/// sees: returned from a reflection native it ended the thread ("Error in
/// thread "main" class file error: class not found: p/Outer$Gone"). HotSpot's
/// `klass_at` throws `NoClassDefFoundError: <internal name>` there
/// (`getDeclaredClasses()` / `getDeclaringClass()` of an application class
/// whose member class file is missing; t1 wave 13, lane I24W, probe
/// `T1W13I24WStructuralFormat.java` rows S17 / S48 in `app`
/// mode). A user-defined loader's miss was already its Java
/// `ClassNotFoundException`, turned into that error by the resolver.
pub fn class_not_found_is_no_class_def_found(failed: MethodCallFailed) -> MethodCallFailed {
    match failed {
        MethodCallFailed::InternalError(cratonvm_types::error::VmError::ClassFile(
            cratonvm_types::error::ClassFileError::ClassNotFound { class_name },
        )) => MethodCallFailed::InternalError(cratonvm_types::error::VmError::Linkage(
            cratonvm_types::error::LinkageError::NoClassDefFoundError { class_name },
        )),
        other => other,
    }
}

/// Drop this VM's reflection-door rows that name an unloaded class (JIT round
/// 21 wave 21, lane innerclasses; page `r21-review-w21-findings-FIXED-20261020.md`
/// item 6a): the JVMS 5.4.3 outer-resolution failures recorded for a dead
/// referencing class (its constant pool is gone, and the map would otherwise
/// grow with every define-and-unload of a class with a missing outer), and
/// the spoof verdicts that name it on either side. Called by the VM's unload
/// sweep (`vm/src/memory/gc.rs` `unload_dead_class_metadata`) with this VM's
/// door-cache slot; a slot never filled holds nothing to drop.
pub fn forget_unloaded_reflect_door_rows(
    slot: &std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>,
    dead: &dyn Fn(u32) -> bool,
) {
    let Some(caches) = slot
        .get()
        .and_then(|boxed| (**boxed).downcast_ref::<ReflectDoorCaches>())
    else {
        return;
    };
    caches
        .outer_resolution_failures
        .write()
        .retain(|(cid, _), _| !dead(*cid));
    caches.spoof_verdicts.write().retain(|key, _| {
        let inner = (*key >> 32) as u32;
        let outer = (*key & 0xFFFF_FFFF) as u32;
        !dead(inner) && (*key & 0xFFFF_FFFF == SPOOF_BY_INNER || !dead(outer))
    });
    // REVIEW22-2 (JIT r23 w1 L6): a reused class id starts with no verdict.
    caches.name_bridge_verdicts.write().retain(|cid, _| !dead(*cid));
}

/// The error a failed [`class_seen_by_referencing_class_checked`] recorded for
/// `(referencing, name)`, as a NEW throwable of its class and message (as
/// HotSpot's `ConstantPool::throw_resolution_error` does), or `None`.
///
/// Keyed by the entry's NAME, not its constant-pool index: the parsed
/// `InnerClasses` / `EnclosingMethod` tables keep names only. That is the same
/// entry whenever the pool has one `CONSTANT_Class` per name (javac's pools
/// always do). Not shared with the interpreter's own failure table, so an
/// `ldc` of the same entry and this door each record their own first failure.
pub fn recorded_outer_resolution_failure(
    ctx: &mut dyn NativeContext,
    referencing: cratonvm_types::ClassId,
    name: &str,
) -> Option<MethodCallFailed> {
    let caches = reflect_door_caches(&*ctx)?;
    if !caches.outer_resolution_failure_sticky {
        return None;
    }
    let (class, message, cause) = {
        let failures = caches.outer_resolution_failures.read();
        if failures.is_empty() {
            return None;
        }
        let row = failures.get(&(referencing.as_u32(), Box::from(name)))?;
        (
            row.class.to_string(),
            row.message.as_deref().map(str::to_string),
            row.cause
                .as_ref()
                .map(|(c, m)| (c.to_string(), m.as_deref().map(str::to_string))),
        )
    };
    // HotSpot builds the cause first and hands it to the new error
    // (`throw_resolution_error` -> `THROW_MSG_CAUSE`): `getCause()` of the
    // rethrown `NoClassDefFoundError` is a `ClassNotFoundException` with the
    // first one's message (wave 21, item 6c).
    let cause = match cause {
        Some((cause_class, cause_message)) => {
            match new_recorded_throwable(ctx, &cause_class, cause_message.as_deref()) {
                Ok(cause) => Some(cause),
                Err(failed) => return Some(failed),
            }
        }
        None => None,
    };
    let cause_pin = cause.map(|c| ctx.pin_native_root(c));
    let error = new_recorded_throwable(ctx, &class, message.as_deref());
    let cause = match (cause_pin, cause) {
        (Some(pin), Some(c)) => {
            let c = ctx.read_native_pin(pin, c);
            ctx.unpin_native_roots(pin);
            Some(c)        }
        _ => None,
    };
    Some(match error {
        Ok(error) => {
            if let Some(cause) = cause {
                ctx.set_field_by_name(error, "cause", Value::Object(Some(cause)));
            }
            MethodCallFailed::ExceptionThrown(error)
        }
        Err(failed) => failed,
    })
}

/// [`recorded_outer_resolution_failure`] for the `getEnclosingMethod0` /
/// `getEnclosingClass` lookup (`lang_reflect::loader_scoped_enclosing_lookup_checked`),
/// which asks it before its exact-namespace probe (JIT round 21 wave 26, lane
/// nesting; [`enclosing_lookup_sticky_first_enabled`]).
pub fn recorded_outer_resolution_failure_of(
    ctx: &mut dyn NativeContext,
    referencing: ClassId,
    name: &str,
) -> Option<MethodCallFailed> {
    recorded_outer_resolution_failure(ctx, referencing, name)
}

/// `CRATONVM_ENCLOSING_LOOKUP_STICKY_FIRST` -- correctness, both modes,
/// default ON, `=0` lets the exact-namespace probe answer first as before
/// (JIT round 21 wave 26, lane nesting): a user-loader class's recorded
/// EnclosingMethod resolution error is rethrown before the loader's later
/// definition of that class is found
/// ([`recorded_outer_resolution_failure_of`]).
pub fn enclosing_lookup_sticky_first_enabled() -> bool {
    true
}

/// A new throwable of `class` with `message` (the no-argument constructor
/// without one), for [`recorded_outer_resolution_failure`]; `Err` is what its
/// construction threw.
pub fn new_recorded_throwable(
    ctx: &mut dyn NativeContext,
    class: &str,
    message: Option<&str>,
) -> Result<ObjectRef, MethodCallFailed> {
    match message {
        Some(message) => match crate::phases_early::throw_jca_exc(ctx, class, message) {
            MethodCallFailed::ExceptionThrown(made)
                if ctx.class_name_of_id(ctx.class_id_of_object(made)).as_deref() == Some(class) =>
            {
                Ok(made)
            }
            other => Err(other),
        },
        None => match ctx.new_object_initialized(class, "()V", &[]) {
            Ok(Some(Value::Object(Some(made)))) => Ok(made),
            Err(failed) => Err(failed),
            Ok(_) => Err(crate::phases_early::throw_jca_exc(ctx, class, "")),
        },
    }
}

/// Record `failed` for `(referencing, name)` when it is a thrown
/// `LinkageError` (JVMS 5.4.3 records nothing else: a `StackOverflowError` or
/// `OutOfMemoryError` must not poison the entry), and hand it back unchanged.
pub fn record_outer_resolution_failure(
    ctx: &mut dyn NativeContext,
    referencing: cratonvm_types::ClassId,
    name: &str,
    failed: MethodCallFailed,
) -> MethodCallFailed {
    if !reflect_door_caches(&*ctx).is_some_and(|c| c.outer_resolution_failure_sticky) {
        return failed;
    }
    let MethodCallFailed::ExceptionThrown(error) = &failed else {
        return failed;
    };
    let error = *error;
    let error_class = ctx.class_id_of_object(error);
    let is_linkage_error = ctx
        .class_id_by_name("java/lang/LinkageError")
        .is_some_and(|linkage| ctx.is_subclass(error_class, linkage));
    if !is_linkage_error {
        return failed;
    }
    let Some(class) = ctx.class_name_of_id(error_class) else {
        return failed;
    };
    let message = throwable_detail_message(ctx, error);
    // HotSpot's `add_resolution_error` keeps the cause's class and message too
    // (`ResolutionErrorEntry::_cause` / `_cause_msg`; wave 21, item 6c). A
    // throwable whose `cause` is itself has none.
    let cause = match ctx.get_field_by_name(error, "cause") {
        Value::Object(Some(cause)) if cause != error => ctx
            .class_name_of_id(ctx.class_id_of_object(cause))
            .map(|cause_class| {
                (
                    Box::from(cause_class.as_str()),
                    throwable_detail_message(ctx, cause).map(Box::from),
                )            }),
        _ => None,
    };
    // A first failure wins, as HotSpot's `add_resolution_error` keeps the
    // first recorded error of an entry.
    if let Some(caches) = reflect_door_caches(&*ctx) {
        caches
            .outer_resolution_failures
            .write()
            .entry((referencing.as_u32(), Box::from(name)))
            .or_insert(RecordedResolutionFailure {
                class: Box::from(class.as_str()),
                message: message.map(Box::from),
                cause,
            });
    }
    failed
}

/// `Throwable.detailMessage` of `t`, as text.
pub fn throwable_detail_message(ctx: &mut dyn NativeContext, t: ObjectRef) -> Option<String> {
    match ctx.get_field_by_name(t, "detailMessage") {
        Value::Object(Some(text)) => ctx.read_string(text),
        _ => None,
    }
}

/// [`declaring_class_id_of`]'s own-loader step: `Ok(None)` (keep the flat
/// answer `candidate`) unless the switch is on, `class_id` has a
/// user-defined loader, and `candidate` is missing or defined by another
/// loader; then the outer name of `class_id`'s own `InnerClasses` member
/// entry, resolved as `class_id`'s constant pool resolves it
/// ([`class_seen_by_referencing_class_checked`]), whose failure is the `Err`
/// (round 18 wave 11, lane mhresid; the caller decides whether to throw it).
pub fn declaring_class_via_own_loader(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
    candidate: Option<cratonvm_types::ClassId>,
) -> Result<Option<cratonvm_types::ClassId>, MethodCallFailed> {
    if !class_has_user_defining_loader(ctx, class_id) {
        return Ok(None);
    }
    if let Some(found) = candidate {
        if ctx.loader_id_of_class(found) == ctx.loader_id_of_class(class_id) {
            return Ok(None);
        }
    }
    if !class_outer_via_own_loader_enabled() {
        return Ok(None);
    }
    let Some(class_name) = ctx.class_name_of_id(class_id) else {
        return Ok(None);
    };
    // The member rule of `NativeContext::declaring_class` (wave 16, lane
    // innerclasses: [`declaring_member_row_outer`]).
    let rows = ctx.inner_classes(class_id);
    let Some(outer_class) = member_row_outer_of(&*ctx, &rows, &class_name) else {
        return Ok(None);
    };
    class_seen_by_referencing_class_checked(ctx, class_id, &outer_class).map(Some)}

