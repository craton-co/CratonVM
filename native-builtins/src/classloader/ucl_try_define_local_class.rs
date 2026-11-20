#[allow(unused_imports)]
use super::*;

/// Try to resolve `URLClassLoader.findClass(name)` from the receiver's own
/// URL set (local filesystem paths and/or real HTTP(S) fetches) and define
/// the resulting class under that receiver's loader namespace.
///
/// Returns `None` only when this loader has NO usable URL entries at all (no
/// local paths, no http(s) bases) -- leaving callers free to use their
/// historical fallback path. Returns `Some(Err(ClassNotFoundException))` when
/// the loader DOES have http(s) entries but none of them produced the class
/// (e.g. every base answered non-200, or refused the connection) -- this is a
/// definitive miss against the loader's own recorded sources and callers
/// must propagate it rather than falling back to the flat global class
/// store, which would let a `URLClassLoader(urls, null)` resolve application
/// classes its own (failed) URL search should have hidden from it. See
/// docs/feature-designs/native-dispatch-architecture.md.
pub fn ucl_try_define_local_class(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    internal_name: &str,
) -> Option<MethodCallResult> {
    // `--real-jdk`: the loader's own `findClass` defines its classes. The
    // `URLClassLoader` family is retired there, so that is the JDK's
    // `URLClassLoader.findClass` over its real `URLClassPath`, which reads the
    // class through the URL's handler and passes its `ProtectionDomain` to
    // `defineClass`. This shortcut defined the class from the loader's recorded
    // URLs itself, with neither. Callers treat `None` as "not handled here" and
    // go on to `findClass`. `--compatible` over a real JDK with the
    // `JdkUrlClassLoaders` row too (interpreter round i1 wave 52, lane L5):
    // the family's stand-ins are not registered there either.
    if ctx.jdk_url_class_loaders() {
        if !ctx.is_jdk_only() {
            ctx.note_alignment_divergence(cratonvm_types::HotSpotAlignment::JdkUrlClassLoaders);
        }
        return None;
    }
    if Err::<String, std::env::VarError>(std::env::VarError::NotPresent).is_ok() {
        let loader_cid = ctx.class_id_of_object(loader);
        let loader_class = ctx.class_name_of_id(loader_cid).unwrap_or_default();
        let paths = loader_constructor_url_paths(ctx, loader);
        eprintln!(
            "[UCLTRACE] name={internal_name} loader_ptr={:?} loader_class={loader_class} n_paths={} paths={:?}",
            loader.as_ptr(),
            paths.len(),
            paths,
        );
    }
    if let Some(mirror) = find_loaded_class_for_loader(ctx, loader, internal_name) {
        return Some(Ok(Some(Value::Object(Some(mirror)))));
    }

    // Serialize concurrent definers of this exact (loader, name) pair -- see
    // `url_classloader_define_locks`. `loader` is freshly passed in by the
    // caller (not yet pinned across any GC-unsafe window), so reading its
    // namespace id here is exactly as safe as the `find_loaded_class_for_loader`
    // probe just above.
    let define_lock_id = loader_namespace_id(ctx, loader);
    let define_lock_key = (define_lock_id, internal_name.to_string());
    let release_key = define_lock_key.clone();
    let define_lock = {
        let mut locks = url_classloader_define_locks()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        locks
            .entry(define_lock_key)
            .or_insert_with(|| std::sync::Arc::new((Mutex::new(false), std::sync::Condvar::new())))
            .clone()
    };
    let _define_lock_release = DefineLockEntryRelease {
        key: release_key,
        lock_addr: std::sync::Arc::as_ptr(&define_lock) as usize,
    };
    let (define_lock_mutex, define_lock_cvar) = &*define_lock;
    let mut in_progress = define_lock_mutex.lock().unwrap_or_else(|e| e.into_inner());
    // GC-safety: this loop WAITS on a condvar, which is the widest window there
    // is -- a peer thread's collection is exactly what runs while this one is
    // parked -- and then re-probes with `loader`, a bare Rust local carried in
    // from outside. `find_loaded_class_for_loader` can itself allocate, so even
    // without the wait the second turn would be reading a pre-GC address. Pin
    // it and re-read at the top of each turn.
    //
    // gc-common w9-a: EVERY return from here on releases this pin. Eight of
    // the early returns below used to leave it behind, so the loader stayed
    // rooted (and the pin slot occupied) until the enclosing native returned.
    let loader_pin = ctx.pin_native_root(loader);
    while *in_progress {
        let (guard, timeout) = define_lock_cvar
            .wait_timeout(in_progress, std::time::Duration::from_secs(30))
            .unwrap_or_else(|e| e.into_inner());
        in_progress = guard;
        let loader = ctx.read_native_pin(loader_pin, loader);
        // The other thread may have finished defining it (success -- return
        // its result) or failed (we should try ourselves rather than loop
        // forever on a definition that will never arrive).
        if let Some(mirror) = find_loaded_class_for_loader(ctx, loader, internal_name) {
            ctx.unpin_native_roots(loader_pin);
            return Some(Ok(Some(Value::Object(Some(mirror)))));
        }
        if timeout.timed_out() {
            break;
        }
    }
    // Double-checked probe under the define lock. The pre-lock
    // `find_loaded_class_for_loader` above can miss and this thread still lose
    // the race: another thread may define `(loader, internal_name)` and CLEAR
    // `in_progress` in the window between that probe and our acquiring the
    // mutex, so the `while *in_progress` wait loop (which does re-probe) never
    // runs even once. Defining again then fails with
    // `IncompatibleClassChangeError: already defined by user-defined(N)
    // loader`, which this function reports as `ClassFormatError` -- and for an
    // isolated loader `resolve_class_loader_aware` turns any `findClass`
    // failure into a hard `NoClassDefFoundError` with no global fallback. Seen
    // as a nondeterministic `NoClassDefFoundError:
    // org.springframework.boot.autoconfigure.condition.ConditionOutcome` under
    // Spring Boot's `ModifiedClassPathClassLoader`, where
    // `OnClassCondition$ThreadedOutcomesResolver` evaluates half the
    // auto-configuration conditions on a second thread and races the main one
    // for exactly these classes. HotSpot has no such window: `loadClass`
    // re-checks `findLoadedClass` after taking `getClassLoadingLock(name)`.
    let loader = ctx.read_native_pin(loader_pin, loader);
    if let Some(mirror) = find_loaded_class_for_loader(ctx, loader, internal_name) {
        ctx.unpin_native_roots(loader_pin);
        return Some(Ok(Some(Value::Object(Some(mirror)))));
    }
    *in_progress = true;
    drop(in_progress);

    /// Clears the in-progress flag and wakes any waiters, including on an
    /// unwind, so a panic mid-define doesn't strand other threads on the
    /// 30s wait forever.
    struct DefineInProgressGuard<'a> {
        mutex: &'a Mutex<bool>,
        cvar: &'a std::sync::Condvar,
    }
    impl<'a> Drop for DefineInProgressGuard<'a> {
        fn drop(&mut self) {
            let mut in_progress = self.mutex.lock().unwrap_or_else(|e| e.into_inner());
            *in_progress = false;
            self.cvar.notify_all();
        }
    }
    let _define_in_progress_guard = DefineInProgressGuard {
        mutex: define_lock_mutex,
        cvar: define_lock_cvar,
    };

    let resource_name = format!("{internal_name}.class");
    let paths = loader_constructor_url_paths(ctx, loader);
    // Keep the source metadata coupled to the exact classpath that supplied
    // the bytes. Falling back to ClassManager's process-wide lookup after a
    // successful local definition can attach a same-named application JAR as
    // this class's CodeSource (for example, a URLClassLoader override JAR).
    let local_class_path = if paths.is_empty() {
        None
    } else {
        Some(cached_class_path_for_paths(&*ctx, &paths, define_lock_id))
    };
    let (bytes, local_code_source) = match local_class_path.as_ref() {
        Some(class_path) => match class_path.find_resource(&resource_name) {
            Some(bytes) => (
                Some(bytes),
                class_path.find_class_code_source_info(internal_name),
            ),
            None => (None, None),
        },
        None => (None, None),
    };
    let http_bases = loader_constructor_http_bases(ctx, loader);
    let bytes = match bytes {
        Some(b) => Some(b),
        None if !http_bases.is_empty() => fetch_http_resource(ctx, &http_bases, &resource_name),
        None => None,
    };
    // gen r4w3/rooting: `fetch_http_resource` enters a blocking region, where
    // a peer thread's moving collection can run; refresh `loader` from the pin
    // taken above before any further use.
    let loader = ctx.read_native_pin(loader_pin, loader);

    let bytes = match bytes {
        Some(b) => b,
        None if http_bases.is_empty() => {
            // A class dynamically appended to the BOOTSTRAP search
            // (`Instrumentation.appendToBootstrapClassLoaderSearch` — e.g.
            // Mockito's inline mock maker injecting `MockMethodDispatcher`/
            // `MockMethodAdvice`) is visible to every loader via real
            // parent-delegation semantics (an isolated `URLClassLoader`'s
            // parent is null, i.e. the bootstrap loader itself — NOT "no
            // parent at all"). Defer to the caller's own fallback (which
            // consults the global class store, itself searching the
            // bootstrap path) instead of making the miss authoritative here.
            if url_classloader_isolated_from_app(ctx, loader)
                && !ctx.is_bootstrap_appended_class(internal_name)
            {
                let exception = ucl_local_miss_exception(ctx, loader, internal_name);
                ctx.unpin_native_roots(loader_pin);
                return Some(exception.and_then(|exception| {
                    Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                        exception,
                    ))
                }));
            }
            ctx.unpin_native_roots(loader_pin);
            return None;
        }
        None => {
            let exc = ucl_local_miss_exception(ctx, loader, internal_name);
            ctx.unpin_native_roots(loader_pin);
            return Some(exc.and_then(|exc| {
                Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                    exc,
                ))
            }));
        }
    };

    if url_classloader_isolated_from_app(ctx, loader) {
        if let Err(error) =
            crate::lang_system::preload_isolated_loader_supertypes(ctx, loader, &bytes)
        {
            ctx.unpin_native_roots(loader_pin);
            return Some(Err(error));
        }
    }

    // gen r4w3/rooting: do NOT re-pin `loader` here -- after
    // `preload_isolated_loader_supertypes` (class loading, GC-capable) the
    // local is already stale, and a fresh pin would root (and then hand back)
    // the vacated address. Read through the pin taken before the wait loop.
    let loader_live = ctx.read_native_pin(loader_pin, loader);
    let loader_id = loader_namespace_id(ctx, loader_live);
    let mut opts = match local_code_source {
        Some((code_source_url, code_source_certificates)) => cratonvm_native_api::DefineClassFull {
            code_source_url: Some(code_source_url),
            code_source_certificates,
            ..Default::default()
        },
        None => cratonvm_native_api::DefineClassFull::default(),
    };
    // zs s8/nd7: the mirror object before the define. The loader is read
    // through `loader_pin` below; the reservation sits above it and is
    // released with it on every arm but the success one.
    let reservation =
        match crate::lang_system::reserve_mirror_for_define(ctx, internal_name, &mut opts) {
            Ok(r) => r,
            Err(e) => {
                ctx.unpin_native_roots(loader_pin);
                return Some(Err(e));
            }
        };
    let define_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ctx.define_class_full(internal_name, &bytes, loader_id, opts)
    }));

    let result = match define_result {
        Ok(Ok(cid)) => {
            let loader_live = ctx.read_native_pin(loader_pin, loader);
            register_defining_loader(loader_tables(ctx), cid.as_u32(), loader_live);
            let reserved = crate::lang_system::take_mirror_reservation(ctx, reservation);
            let mirror = crate::lang_system::defined_class_mirror_reserved(ctx, cid, reserved);
            Ok(Some(Value::Object(Some(mirror))))
        }
        // Matches BOTH renderings. `define_class_full` hands this boundary a
        // `String` built with `format!("{e:?}")`, so the test is on the VmError's
        // DEBUG text. The duplicate raise site now produces
        // `DuplicateClassDefinition { .. }` (HotSpot throws
        // `java.lang.LinkageError` itself) where it used to produce an
        // `IncompatibleClassChangeError` whose message read "already defined
        // by" -- and this arm keyed on that wording, so correcting the type
        // without touching it here would have silently turned a RECOVERED
        // concurrent-definition race into a hard failure. The older spelling is
        // kept because this same arm serves other producers of that text.
        Ok(Err(msg))
            if msg.contains("DuplicateClassDefinition") || msg.contains("already defined by") =>
        {
            // Benign concurrent-definition race: ANOTHER thread (e.g. a
            // background thread pool eagerly resolving classes, or a
            // recursive supertype/interface resolution nested inside a
            // DIFFERENT class's `define_class_full` -- see the sibling fix
            // in `classloading::ClassManager`'s `resolve_supertype`)
            // independently defined this exact (loader, name) pair between
            // our `find_loaded_class_for_loader` pre-check above and this
            // `define_class_full` call actually completing. The
            // `url_classloader_define_locks` mutex above only serializes
            // OTHER callers of THIS function for the same name; it cannot
            // see a concurrent definer that reached `define_class_full`
            // through a different, unlocked path. Recover by returning the
            // winner's already-registered mirror instead of surfacing a
            // spurious `NoClassDefFoundError`.
            let loader_live = ctx.read_native_pin(loader_pin, loader);
            match find_loaded_class_for_loader(ctx, loader_live, internal_name) {
                Some(mirror) => Ok(Some(Value::Object(Some(mirror)))),
                None => {
                    tracing::warn!(
                        "URLClassLoader.findClass({internal_name}) define failed: {msg}"
                    );
                    Err(cratonvm_types::error::LinkageError::ClassFormatError {
                        class_name: internal_name.to_string(),
                        message: format!("URLClassLoader.findClass: {msg}"),
                    }
                    .into())
                }
            }
        }
        Ok(Err(msg)) => {
            // Backstop for the same race as the double-checked probe above,
            // covering definers that do NOT go through this function's lock
            // (`Lookup`/`Unsafe.defineClass`, `preload_isolated_loader_super`
            // `types`, a parent loader's own defining path). If the class is
            // now defined under this loader, the "already defined" error means
            // we merely LOST the race -- JVMS SS5.3.5 says the loser observes
            // the winner's class, not a linkage error. Returning it is what
            // HotSpot's `defineClass`-under-`getClassLoadingLock` does.
            // gen r4w3/rooting: `define_class_full` is GC-capable; re-read.
            let loader_live = ctx.read_native_pin(loader_pin, loader);
            if let Some(mirror) = find_loaded_class_for_loader(ctx, loader_live, internal_name) {
                tracing::debug!(
                    "URLClassLoader.findClass({internal_name}) lost a define race ({msg}); \
                     returning the winning definition"
                );
                ctx.unpin_native_roots(loader_pin);
                return Some(Ok(Some(Value::Object(Some(mirror)))));
            }
            tracing::warn!("URLClassLoader.findClass({internal_name}) define failed: {msg}");
            Err(cratonvm_types::error::LinkageError::ClassFormatError {
                class_name: internal_name.to_string(),
                message: format!("URLClassLoader.findClass: {msg}"),
            }
            .into())
        }
        Err(_) => {
            tracing::error!(
                "URLClassLoader.findClass({internal_name}) panicked while defining local class"
            );
            Err(cratonvm_types::error::LinkageError::ClassFormatError {
                class_name: internal_name.to_string(),
                message: "URLClassLoader.findClass: panic inside backend".into(),
            }
            .into())
        }
    };
    ctx.unpin_native_roots(loader_pin);
    Some(result)
}

/// `jdk.internal.loader.URLClassPath.addURL(URL)` for real-JDK mode.
///
/// `URLClassLoader.addURL` is inherited and almost always invoked via the
/// SUBCLASS as `this.addURL(url)` (e.g. ShrinkWrap's `addArchive`), so its CP
/// methodref names the subclass — the `check_override` / force-native gates
/// (which key on the static call-site class) can't recognise it. But the body
/// `URLClassLoader.addURL` is just `ucp.addURL(url)`, and `ucp` is typed
/// `jdk.internal.loader.URLClassPath`, so shimming `URLClassPath.addURL`
/// intercepts the same operation through a call-site the gates DO match. `this`
/// here is the `URLClassPath` (the loader's `ucp`). See the module banner.
pub fn record_real_ucl_url(ctx: &mut dyn NativeContext, ucp: ObjectRef, url: ObjectRef) {
    // Record the base URL on `ucp.path` so `ucl_find_resource(s)` can resolve
    // against custom-handler URLs. Pin `url` across the re-entrant recording so
    // the protocol read below still sees a live ref.
    let p_url = ctx.pin_native_root(url);
    let url_live = ctx.read_native_pin(p_url, url);
    record_url_on_path(ctx, ucp, url_live);

    // Ordinary file:/jar: URLs additionally extend the global dynamic
    // classpath so classes/resources inside them load (mirrors the `<init>`
    // natives' `register_url_array`). Custom schemes (archive:, …) have no
    // filesystem path and are served via their handler instead.
    // `addURL` changes this loader only on HotSpot. Keep ordinary file:/jar:
    // URLs out of the global application classpath as well; local resolver
    // paths serve them without leaking into siblings or parents.
    ctx.unpin_native_roots(p_url);
}

/// Record a URL appended through the real `URLClassLoader.addURL(URL)`
/// wrapper.  Subclasses such as ShrinkWrap invoke that protected inherited
/// method with a subclass-owned call site, so the ordinary static-class native
/// gate cannot see it.  Keeping the original URL is essential for application
/// URLStreamHandler-backed schemes such as `archive:`.
pub fn ucl_add_url_real(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let url = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    if let Value::Object(Some(ucp)) = ctx.get_field_by_name(this, "ucp") {
        record_real_ucl_url(ctx, ucp, url);
    }
    Ok(None)
}

pub fn ucp_add_url(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let ucp = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let url = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    record_real_ucl_url(ctx, ucp, url);
    Ok(None)
}

pub fn ucl_find_resource(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first().copied() {
        // A CLOSED loader finds nothing new — `URLClassLoader.close()` shuts its
        // `URLClassPath`, and this native is what stands in for that search in
        // real-JDK mode (the `ucp` CratonVM hands the loader is never
        // populated, so closing it has no effect on its own).
        if ucl_is_closed(ctx, this) {
            return Ok(Some(Value::Object(None)));
        }
    }
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let name = ctx.read_string(name_obj).unwrap_or_default();
    // A LEADING SLASH matches nothing, as in `cl_get_resource`: the JDK's
    // `URLClassPath` loaders use the name verbatim (`new URL(base, "/x")`
    // leaves the base, `JarFile.getEntry("/x")` is null), so a direct
    // `URLClassLoader.findResource("/x")` is null on HotSpot. The strip below
    // served `x` (interpreter round i5 wave 100, lane L3;
    // `L3W100UrlLoaderLeadingSlash`).
    if name.starts_with('/') {
        return Ok(Some(Value::Object(None)));
    }
    let resource_name = name.trim_start_matches('/');
    // A `*` / `?` is literal in a Java resource name (`is_glob_resource_name`;
    // interpreter round i5 wave 100, lane L3).
    let glob = is_glob_resource_name(resource_name);
    if let Some(Value::Object(Some(this))) = args.first().copied() {
        let mut local_urls = loader_local_resource_urls(ctx, this, resource_name);
        if glob {
            local_urls.retain(|url| url_names_literal_resource(url, resource_name));
        }
        if let Some(first) = local_urls.first() {
            let url = crate::jboss_module_loader::build_synthetic_url(ctx, first);
            return Ok(Some(Value::Object(Some(url?))));
        }
        // A URLClassLoader's `findResource` is strictly local. Its public
        // `getResource` caller has already performed parent-first delegation;
        // consulting the process-wide classpath here leaks entries that a
        // temporary child deliberately removed. In particular, Spring Boot's
        // ModifiedClassPathClassLoader excludes Hibernate Validator from its
        // recorded URL list, but can have a non-null platform-loader parent on
        // real JDKs, so the narrower `isolated_from_app` predicate is not a
        // sufficient guard.
        if object_extends(ctx, this, "java/net/URLClassLoader") {
            return Ok(Some(Value::Object(None)));
        }
    }
    // Mirror cl_get_resource's lookup order: structured URL walk FIRST.
    // URLClassLoader-constructor URLs are registered into the global walk
    // but NOT into the raw-bytes `find_resource` store, so consulting only
    // the latter made findResource return null for any resource living in a
    // loader-supplied jar while getResource (the walk) found it. Canonical
    // victim: Gradle's VisitableURLClassLoader("runtime-api-info") looking
    // up gradle-plugins.properties from gradle-runtime-api-info.jar —
    // "Cannot find resource ... in classloader" killed every ProjectBuilder
    // bootstrap (Spring Boot buildSrc suite). Returning the walk's URL also
    // keeps findResource/getResource spec-consistent (same URL form).
    let mut urls = ctx.find_all_resource_urls(resource_name);
    if glob {
        urls.retain(|url| url_names_literal_resource(url, resource_name));
    }
    if let Some(first) = urls.first() {
        let url = crate::jboss_module_loader::build_synthetic_url(ctx, first);
        return Ok(Some(Value::Object(Some(url?))));
    }
    let raw = if glob {
        None
    } else {
        ctx.find_resource(resource_name)
    };
    match raw {
        Some(_) => {
            let spec = format!("classpath:{name}");
            let url = crate::jboss_module_loader::build_synthetic_url(ctx, &spec);
            Ok(Some(Value::Object(Some(url?))))
        }
        None => {
            // Custom-handler fallback: resources behind an app-supplied
            // `URLStreamHandler` (ShrinkWrap `archive:`) that `addURL` recorded
            // on `ucp.path`. Return the first base URL that resolves `name`.
            if let Some(Value::Object(Some(this))) = args.first().copied() {
                if let Some(list) = build_custom_handler_url_list(ctx, this, resource_name) {
                    let p_list = ctx.pin_native_root(list);
                    let list = ctx.read_native_pin(p_list, list);
                    let first =
                        ctx.invoke_virtual(list, "get", "(I)Ljava/lang/Object;", &[Value::Int(0)]);
                    ctx.unpin_native_roots(p_list);
                    if let Ok(Some(v @ Value::Object(Some(_)))) = first {
                        return Ok(Some(v));
                    }
                }
            }
            Ok(Some(Value::Object(None)))
        }
    }
}

/// Append the elements of a `java.util.ArrayList<URL>` to the URL array carried
/// by a synthetic `Enumeration$Impl`, returning a fresh `Enumeration$Impl` over
/// the union. `std_enum` is the standard flat-classpath enumeration (may be
/// `None`); `custom_list` holds the custom-handler resolved URLs. All
/// re-entrant calls are pinned for moving-GC safety.
pub fn merge_enum_with_list(
    ctx: &mut dyn NativeContext,
    std_enum: Option<ObjectRef>,
    custom_list: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    if !is_array_list_object(ctx, custom_list) {
        return match std_enum {
            Some(e) => Ok(e),
            // `unwrap_or_else` cannot carry the refusal out of its closure,
            // and building the empty enumeration is now fallible.
            None => empty_enumeration_impl(ctx),
        };
    }
    let p_custom = ctx.pin_native_root(custom_list);
    // Standard enumeration's backing URL[] (field 0 of `Enumeration$Impl`).
    let std_arr = match std_enum {
        Some(e) => match ctx.get_field(e, 0) {
            Value::Object(Some(a)) => Some(a),
            _ => None,        },
        None => None,
    };
    let alen = std_arr.map(|a| ctx.array_length(a)).unwrap_or(0);
    let p_std = std_arr.map(|a| ctx.pin_native_root(a));
    let custom_list = ctx.read_native_pin(p_custom, custom_list);
    let clen = match ctx.invoke_virtual(custom_list, "size", "()I", &[]) {
        Ok(Some(Value::Int(n))) => n.max(0) as usize,
        _ => 0,
    };
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, alen + clen);
    let p_arr = ctx.pin_native_root(arr);
    if let (Some(sa), Some(psa)) = (std_arr, p_std) {
        let sa = ctx.read_native_pin(psa, sa);
        let arr = ctx.read_native_pin(p_arr, arr);
        for i in 0..alen {
            let e = ctx.get_array_element(sa, i);
            ctx.set_array_element(arr, i, e);
        }
    }
    for i in 0..clen {
        let custom_list = ctx.read_native_pin(p_custom, custom_list);
        let e = match ctx.invoke_virtual(
            custom_list,
            "get",
            "(I)Ljava/lang/Object;",
            &[Value::Int(i as i32)],
        ) {
            Ok(Some(v)) => v,
            _ => Value::Object(None),
        };
        let arr = ctx.read_native_pin(p_arr, arr);
        ctx.set_array_element(arr, alen + i, e);
    }
    let arr = ctx.read_native_pin(p_arr, arr);
    let enm = crate::classloader::make_snapshot_enumeration(ctx, arr)?;
    ctx.unpin_native_roots(p_custom); // releases p_custom, p_std, p_arr
    Ok(enm)
}

pub fn ucl_find_resources(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // `findResources` is the terminal flat-classpath scan — it must NOT re-delegate
    // to `findResources` (which would recurse forever for a URLClassLoader subclass
    // that doesn't override it, e.g. GroovyClassLoader). See SB-13.
    //
    // Compute the custom-handler matches FIRST: when there are none (the common
    // case for ordinary file:/jar: loaders) the standard scan is returned
    // verbatim, leaving existing behaviour byte-for-byte unchanged.
    let this = match args.first().copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return cl_get_resources_impl(ctx, args, false),
    };
    // `close()` makes this loader incapable of discovering any further
    // resources. Do this before the shared flat-classpath scan: consulting
    // that scan after close leaks resources from unrelated live loaders.
    if ucl_is_closed(ctx, this) {
        return Ok(Some(Value::Object(Some(empty_enumeration_impl(ctx)?))));
    }
    let name = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => return cl_get_resources_impl(ctx, args, false),
    };
    // A LEADING SLASH matches nothing (see `ucl_find_resource`; interpreter
    // round i5 wave 100, lane L3): `URLClassLoader.findResources("/x")`, and
    // so the local half of its `getResources("/x")`, is empty on HotSpot.
    if name.starts_with('/') {
        return Ok(Some(Value::Object(Some(empty_enumeration_impl(ctx)?))));
    }
    let resource_name = name.trim_start_matches('/').to_string();

    let p_this = ctx.pin_native_root(this);

    // Does this receiver's OWN URL list answer the question? That decides what
    // an EMPTY local scan MEANS, and the two meanings need opposite handling:
    //
    //   * URL list knowable, scan empty  -> the resource genuinely is not on
    //     this loader's classpath. Returning it anyway is a leak.
    //   * URL list not knowable at all   -> the local scan could not run; the
    //     historical process-wide scan is all there is.
    //
    // Only the first case is new. It is what Spring Boot's
    // `ModifiedClassPathClassLoader` builds on purpose: it filters
    // `hibernate-validator-*.jar` / `logback-*.jar` out of its own `URL[]` so a
    // `@ClassPathExclusions` test sees a classpath without them. Falling back to
    // the flat scan handed that jar's `META-INF/services` entry straight back,
    // while `loadClass` still (correctly) refused the class it names --
    // `ServiceLoader` then read a registration for a provider it could not load
    // and raised `ServiceConfigurationError: ... Provider ... not found` where
    // HotSpot finds no providers at all. See
    // `classpath-exclusions-flat-scan-and-module-provides-leak-FIXED-20260810.md`.
    //
    // The SINGULAR `findResource` has drawn this line since the ModifiedClassPath
    // work (see its `object_extends(.., "java/net/URLClassLoader")` early return);
    // this is the plural half of the same rule, kept narrower so a loader whose
    // URLs CratonVM cannot see behaves exactly as before.
    let this_probe = ctx.read_native_pin(p_this, this);
    let has_own_urls = !loader_constructor_url_paths(ctx, this_probe).is_empty();

    // Run the standard flat-classpath scan first while the arguments are
    // still fresh; custom-handler/local probing below can allocate. Skipped
    // outright when the receiver answers for itself -- besides being the leak
    // above, it is a full process-wide walk per `findResources` call.
    let std_enum = if has_own_urls {
        None
    } else {
        cl_get_resources_impl(ctx, args, false)?
    };
    let std_ref = match std_enum {
        Some(Value::Object(Some(e))) => Some(e),
        _ => None,
    };
    let p_std = std_ref.map(|e| ctx.pin_native_root(e));

    let this = ctx.read_native_pin(p_this, this);
    let mut local_urls = loader_local_resource_urls(ctx, this, &resource_name);
    // A `*` / `?` is literal in a Java resource name (`is_glob_resource_name`;
    // interpreter round i5 wave 100, lane L3).
    if is_glob_resource_name(&resource_name) {
        local_urls.retain(|url| url_names_literal_resource(url, &resource_name));
    }
    let local_enum = if local_urls.is_empty() {
        None
    } else {
        let arr = ctx.new_array(
            cratonvm_types::ArrayElementType::Reference,
            local_urls.len(),
        );
        // gen r4w3/rooting: `build_synthetic_url` is GC-capable; keep `arr`
        // rooted and re-read it before every store and before the wrap.
        let p_arr = ctx.pin_native_root(arr);
        for (i, url) in local_urls.iter().enumerate() {
            let url_obj = crate::jboss_module_loader::build_synthetic_url(ctx, url)?;
            let arr = ctx.read_native_pin(p_arr, arr);
            ctx.set_array_element(arr, i, Value::Object(Some(url_obj)));
        }
        let arr = ctx.read_native_pin(p_arr, arr);
        ctx.unpin_native_roots(p_arr);
        let enm = crate::classloader::make_snapshot_enumeration(ctx, arr)?;
        Some(enm)
    };
    let p_local = local_enum.map(|e| ctx.pin_native_root(e));

    // Custom-handler matches (ShrinkWrap `archive:`), recorded by `addURL`.
    let this = ctx.read_native_pin(p_this, this);
    let custom = build_custom_handler_url_list(ctx, this, &resource_name);

    let local_ref = match (local_enum, p_local) {
        (Some(e), Some(p)) => Some(ctx.read_native_pin(p, e)),
        _ => None,
    };
    let std_ref = match (std_ref, p_std) {
        (Some(e), Some(p)) => Some(ctx.read_native_pin(p, e)),
        _ => None,
    };
    let result = match custom {
        Some(custom) => Some(Value::Object(Some(merge_enum_with_list(
            ctx,
            local_ref.or(std_ref),
            custom,
        )?))),
        None => match local_ref.or(std_ref) {
            Some(e) => Some(Value::Object(Some(e))),
            // `std_enum` is `None` in exactly the `has_own_urls` case, so the
            // empty enumeration below is this loader's own authoritative "no
            // matches" -- not a dropped result.
            None => match std_enum {
                Some(e) => Some(e),
                None => Some(Value::Object(Some(empty_enumeration_impl(ctx)?))),
            },
        },
    };
    ctx.unpin_native_roots(p_this);
    Ok(result)
}

/// `URLClassLoader.getURLs()` — the URLs this loader was constructed with,
/// plus anything `addURL` appended. Nothing else.
///
/// It must NOT expand a jar's manifest `Class-Path`. The real JDK resolves
/// `Class-Path` lazily inside `URLClassPath`, never through this public
/// accessor, and the expansion is observable: it drops the referring jar
/// itself, invents entries for `Class-Path` names that do not exist on disk,
/// and re-percent-encodes an already-encoded entry (`project%20space` came
/// back as `project%2520space`). Spring Boot's `ChangeableUrls.fromClassLoader`
/// walks `getURLs()` and then expands each jar's manifest itself, so a
/// pre-expanded list made it report six directories where five were expected
/// — `ChangeableUrlsTests.urlsFromJarClassPathAreConsidered`.
///
/// The expansion was added for Spring Boot's `ModifiedClassPathClassLoader`
/// (`@ClassPathExclusions`), which is handed the suite runner's manifest-only
/// pathing JAR and would otherwise filter a class path different from the one
/// the loader searches. That never applied here: the application loader is
/// `jdk.internal.loader.ClassLoaders$AppClassLoader`, which is not a
/// `URLClassLoader`, so `ModifiedClassPathClassLoader.doExtractUrls` reads
/// `ManagementFactory.getRuntimeMXBean().getClassPath()` and never reaches
/// this native. `probes/DevtoolsChangeableUrlsProbe.java` covers the contract;
/// `probes/RuntimeClassPathProbe.java` shows the app-loader shape.
pub fn ucl_get_urls(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Value::Object(Some(ucp)) = ctx.get_field_by_name(this, "ucp") {
        if let Some(result) = ucp_path_urls(ctx, ucp) {
            return Ok(Some(Value::Object(Some(result))));
        }
    }
    // Everything below reads the SYNTHETIC slot indices. On a real
    // `java.net.URLClassLoader` (`javap -p`, superclass fields first:
    // `java.lang.ClassLoader` declares `parent`(0) `name`(1) `unnamedModule`(2)
    // `nameAndId`(3) `parallelLockMap`(4) …, then `SecureClassLoader.pdcache`,
    // then `ucp` and `closeables`) slot 2 is a `Module` and slot 4 a
    // `ConcurrentHashMap`, where this module means URL-COUNT and URL-ARRAY.
    //
    // W7-7 left these raw reads unguarded as benign, and the arithmetic did
    // hold: slot 2 reads back as a reference, the `Value::Int` arm misses, the
    // count lands 0, and the slot-4 CHM is never indexed. But "right because
    // the tag happened not to match" is one layout change away from
    // `array_length(ConcurrentHashMap)`, and it is not the reason the code is
    // correct — the layout is. Say so. A real-layout loader's URLs are the
    // `ucp.path` list read above and nothing else, so the answer here is the
    // same empty array the count-0 arithmetic already produced.
    // `URL[]`, the declared type (i6-L2), on every return below.
    let url_component = crate::lang_class::reflection_component_id(ctx, "java/net/URL");
    if !cl_has_synthetic_layout(ctx, this) {
        let empty = ctx.new_ref_array(url_component, 0);
        return Ok(Some(Value::Object(Some(empty))));
    }
    let count = match ctx.get_field(this, UCL_URL_COUNT) {
        Value::Int(n) => n.max(0) as usize,
        _ => 0,
    };
    // Synthetic-JDK path: URLs live in the per-instance slots (`ucl_setup`/
    // `ucl_add_url`). Keep the legacy raw-slot fallback for old placeholders —
    // but only when the `ucp` is itself a fabricated stub, because on a real
    // `URLClassPath` slot 0 is the `path` ArrayList and `array_length` of an
    // ArrayList is not a URL count.
    if count == 0 {
        if let Value::Object(Some(ucp)) = ctx.get_field_by_name(this, "ucp") {
            if let Value::Object(Some(stashed)) = ctx.get_field(ucp, UCP_STASHED_URLS) {
                if ucp_stash_is_reference_array(ctx, stashed) {
                    let n = ctx.array_length(stashed);
                    // GC-safety (Family-1 stale ObjectRef): `stashed` is read
                    // out of the heap, so unlike `this`/`args` it is not rooted
                    // by the interpreter frame. `new_array` below can trigger a
                    // moving collection, after which the pre-allocation
                    // `stashed` would name the old address and the loop would
                    // copy from a stale object. Pin across the allocation and
                    // re-read through the pin, exactly as `ucl_add_url` does
                    // for `urls_arr`/`url_obj`. `n` is a plain length, so it
                    // survives the move; `result` is freshly allocated and the
                    // loop below allocates nothing, so neither needs a pin.
                    let stashed_pin = ctx.pin_native_root(stashed);
                    let result = ctx.new_ref_array(url_component, n);
                    let stashed = ctx.read_native_pin(stashed_pin, stashed);
                    ctx.unpin_native_roots(stashed_pin);
                    for i in 0..n {
                        let url = ctx.get_array_element(stashed, i);
                        ctx.set_array_element(result, i, url);
                    }
                    return Ok(Some(Value::Object(Some(result))));
                }
            }
        }
    }
    // Copy stored URLs into a new array of the exact size.
    //
    // GC-safety: `new_array` is the only allocation, and `urls_arr` is read out
    // of the heap *after* it, so there is no pre-allocation array reference
    // left to go stale. The loop allocates nothing. `this` DOES need the pin:
    // the interpreter frame roots the argument, but a moving collection updates
    // the frame's slot, not this Rust copy of it (gc-common w20-f; the comment
    // here used to say the frame made `this` safe on its own).
    let this_pin = ctx.pin_native_root(this);
    let result = ctx.new_ref_array(url_component, count);
    let this: ObjectRef = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    if let Value::Object(Some(urls_arr)) = ctx.get_field(this, UCL_URLS_ARRAY) {
        for i in 0..count {
            let url = ctx.get_array_element(urls_arr, i);
            ctx.set_array_element(result, i, url);
        }
    }
    Ok(Some(Value::Object(Some(result))))
}

pub fn ucl_add_url(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;

    // Check if closed
    if ucl_is_closed(ctx, this) {
        tracing::warn!("URLClassLoader.addURL called on closed loader");
        return Ok(None);
    }

    let count = match ctx.get_field(this, UCL_URL_COUNT) {
        Value::Int(n) => n,
        _ => 0,
    };

    // Store the URL object in the URLs array
    if let Some(Value::Object(Some(url_obj))) = args.get(1) {
        let mut url_obj = *url_obj;
        if let Value::Object(Some(urls_arr)) = ctx.get_field(this, UCL_URLS_ARRAY) {
            let arr_len = ctx.array_length(urls_arr);
            if (count as usize) < arr_len {
                ctx.set_array_element(urls_arr, count as usize, Value::Object(Some(url_obj)));
            } else {
                // GC-safety: growing the array below (`new_array`) can
                // trigger a moving GC; `urls_arr` (copied FROM) and
                // `url_obj` (the new entry) are both reused afterward,
                // unpinned otherwise.
                //
                // gc-common w20-f: `this` too -- the new array and the count
                // were stored through the receiver's pre-allocation address.
                let this_pin = ctx.pin_native_root(this);
                let urls_arr_pin = ctx.pin_native_root(urls_arr);
                let url_obj_pin = ctx.pin_native_root(url_obj);
                let new_cap = arr_len * 2;
                let new_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, new_cap);
                this = ctx.read_native_pin(this_pin, this);
                let urls_arr = ctx.read_native_pin(urls_arr_pin, urls_arr);
                url_obj = ctx.read_native_pin(url_obj_pin, url_obj);
                ctx.unpin_native_roots(this_pin);
                for i in 0..arr_len {
                    let elem = ctx.get_array_element(urls_arr, i);
                    ctx.set_array_element(new_arr, i, elem);
                }
                ctx.set_array_element(new_arr, count as usize, Value::Object(Some(url_obj)));
                ctx.set_field(this, UCL_URLS_ARRAY, Value::Object(Some(new_arr)));
            }
        }

        // Extract the URL path and extend the classpath dynamically.
        if let Some(p) = extract_url_path(ctx, url_obj) {
            ctx.register_dynamic_classpath(&[p.clone()]);
            tracing::debug!("URLClassLoader.addURL: {} (classpath extended)", p);
        }
    }

    ctx.set_field(this, UCL_URL_COUNT, Value::Int(count + 1));
    Ok(None)
}

pub fn ucl_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // The identity-keyed state is authoritative for both synthetic and real
    // URLClassLoader layouts. Writing synthetic slot 3 on a real JDK object
    // would instead corrupt an implementation-private field.
    ucl_mark_closed(ctx, this);
    Ok(None)
}

pub fn ucl_new_instance(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let urls = args.first().copied().unwrap_or(Value::Object(None));
    let obj = alloc_url_classloader(ctx)?;
    // FIX: previously this only stored UCL_URL_COUNT and dropped the URL[]
    // entirely, so the returned loader couldn't search the supplied URLs.
    // Route through `ucl_setup` (the same code the `<init>` natives use) so
    // the URLs are copied into UCL_URLS_ARRAY and their paths registered on
    // the dynamic classpath. The loader id assigned by `alloc_url_classloader`
    // is preserved (ucl_setup doesn't touch UCL_LOADER_ID).
    //
    // GC-safety: `ucl_setup` allocates/copies the URL array and can trigger a
    // moving GC; `obj` is returned afterward, unpinned otherwise.
    let obj_pin = ctx.pin_native_root(obj);
    ucl_setup(ctx, obj, urls, Value::Object(None));
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.unpin_native_roots(obj_pin);
    Ok(Some(Value::Object(Some(obj))))
}

pub fn ucl_new_instance_parent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let urls = args.first().copied().unwrap_or(Value::Object(None));
    let parent = args.get(1).copied().unwrap_or(Value::Object(None));
    let obj = alloc_url_classloader(ctx)?;
    // FIX: mirror the `<init>(URL[], ClassLoader)` path — store the URL[] and
    // register its paths so the loader actually searches them (was dropping
    // the URLs and only recording their count). See `ucl_new_instance`.
    //
    // GC-safety: `ucl_setup` allocates/copies the URL array and can trigger a
    // moving GC; `obj` is returned afterward, unpinned otherwise.
    let obj_pin = ctx.pin_native_root(obj);
    ucl_setup(ctx, obj, urls, parent);
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.unpin_native_roots(obj_pin);
    Ok(Some(Value::Object(Some(obj))))
}

// ---------------------------------------------------------------------------
// java.lang.invoke.MethodHandles$Lookup natives
// ---------------------------------------------------------------------------

pub fn lk_lookup(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let obj = alloc_lookup(ctx, LK_FULL_POWER);
    Ok(Some(Value::Object(Some(obj?))))
}

/// `privateLookupIn(targetClass, caller)` — registered here on
/// `MethodHandles$Lookup`, which is **not the class that declares it**.
///
/// Reachability, re-derived rather than inherited. `javap -p
/// java.lang.invoke.MethodHandles` declares
/// `public static Lookup privateLookupIn(Class<?>, Lookup)`; `javap -p
/// java.lang.invoke.MethodHandles$Lookup` does not declare it at all. The
/// registry keys exactly on `(class, name, descriptor)` and only ever relaxes
/// the DESCRIPTOR (`find_with_descriptor_quirks`), never the class, and
/// `MethodHandles$Lookup` is a nested class — not a supertype of
/// `MethodHandles` — so it never appears on the static-resolution chain for
/// `MethodHandles.privateLookupIn`. (`synthetic_stub_superclass` puts
/// `MethodHandles` under `java/lang/Object` in synthetic mode too.) No
/// bytecode can name this triple and no in-tree caller does, so this
/// registration is **unreachable** — a stronger verdict than "not force
/// listed", and one the corrected registration-is-the-gate rule does not
/// disturb: that rule decides which implementation WINS for a triple, it does
/// not conjure a triple the language cannot spell.
///
/// The live copy is `lang_invoke.rs`'s registration on
/// `java/lang/invoke/MethodHandles`, whose `pli_enforce` carries the measured
/// OpenJDK 25.0.3 contract. This body is kept (never removed — standing
/// constraint) and brought into line with it so that a future registration on
/// the correct class key, or a force-list entry, cannot silently turn a
/// full-power grant back on. Two things it was getting wrong independently of
/// the access question:
///
/// * **It granted `LK_FULL_POWER` (0x5F, 95).** Re-measured for this lane on
///   OpenJDK 25.0.3: `privateLookupIn` answers **31** — `PUBLIC|PRIVATE|
///   PROTECTED|PACKAGE|MODULE`, i.e. [`LK_FULL_POWER_MODES`]. ORIGINAL is
///   dropped. Granting 95 hands out a mode bit the JDK never grants here.
/// * **The `target_class` `ObjectRef` was used after an allocation.**
///   `alloc_lookup` can run a moving GC, so the ref taken from `args` above it
///   may be stale by the time it is written into `lookupClass` — the Family-1
///   defect `lk_ensure_initialized` below already guards against.
///
/// The mode gate reproduces `pli_enforce`'s, valve included: refuse only on a
/// POSITIVELY read, nonzero, weak mode word. `lk_modes_of` answers 0 both for
/// a genuinely modeless Lookup and for one whose modes it could not read, and
/// refusing our "cannot tell" 0 would turn every Lookup this VM does not model
/// into an `IllegalAccessException`. The module `canRead`/`isOpen` gate is
/// [`private_lookup_in_module_refusal`], asked in both modes (since
/// interpreter round i1 wave 47; `--real-jdk` since wave 45).
///
/// The two implementations must be collapsed onto `pli_enforce` (one
/// `pub` on it) rather than forked again; `lang_invoke.rs` is owned by
/// another lane. Same standing note as [`lk_in_modes`].
pub fn lk_private_lookup_in(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut target_class = args.first().copied().unwrap_or(Value::Object(None));
    let caller = args.get(1).copied().unwrap_or(Value::Object(None));
    // The caller's lookup class when the target is in another module (the
    // JDK's `newPreviousClass`; `private_lookup_in_module_refusal`).
    let mut prev_class: Option<ObjectRef> = None;

    // (1) `caller.allowedModes` is the JDK's FIRST dereference — measured: a
    //     null caller with a primitive target still raises the caller NPE, not
    //     the primitive `IllegalArgumentException`.
    let caller_ref = match caller {
        Value::Object(Some(o)) => o,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some(
                    "Cannot read field \"allowedModes\" because \"caller\" is null".to_string(),
                ),
            }
            .into());
        }
    };
    let modes = lk_modes_of(ctx, caller_ref);
    // (2) TRUSTED (-1) returns `new Lookup(targetClass)` from the top of the
    //     JDK method, before any check below.
    if modes != -1 {
        // (3) `targetClass.isPrimitive()`.
        let target_ref = match target_class {
            Value::Object(Some(o)) => o,
            _ => {
                return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                    message: Some(
                        "Cannot invoke \"java.lang.Class.isPrimitive()\" because \"targetClass\" is null"
                            .to_string(),
                    ),
                }
                .into());
            }
        };
        let target_name = crate::lang_class::mirror_class_name(ctx, target_ref);
        if matches!(
            crate::lang_class::native_class_is_primitive(ctx, &[Value::Object(Some(target_ref))]),
            Ok(Some(Value::Int(1)))
        ) {
            // `Class.toString()` of a primitive is bare — "int", "void" — so the
            // JDK's message has no "class " prefix.
            let name = target_name.unwrap_or_else(|| "?".to_string());
            return Err(
                cratonvm_types::error::RuntimeError::IllegalArgumentException {
                    message: format!("{name} is a primitive class"),
                }
                .into(),
            );
        }
        // (4) `targetClass.isArray()`. Array mirrors are the ones whose name
        //     starts with '['; `Class.toString()` of an array IS prefixed and
        //     prints the binary name ("class [I", "class [Lp.Mate;").
        if let Some(name) = target_name {
            if name.starts_with('[') {
                return Err(
                    cratonvm_types::error::RuntimeError::IllegalArgumentException {
                        message: format!("class {} is an array class", name.replace('/', ".")),
                    }
                    .into(),
                );
            }
        }
        // (5) the mode gate, with the `modes == 0` valve documented above.
        if modes != 0 && (modes & (LK_PRIVATE | LK_MODULE)) != (LK_PRIVATE | LK_MODULE) {
            return Err(
                cratonvm_types::error::RuntimeError::IllegalAccessException {
                    message: "caller does not have PRIVATE and MODULE lookup mode".to_string(),
                }
                .into(),
            );
        }
        // (6) the module half (both modes). It runs Java, so the target is
        //     read back from it.
        let (target_now, prev) = private_lookup_in_module_refusal(ctx, target_ref, caller_ref)?;
        target_class = Value::Object(Some(target_now));
        prev_class = prev;
    }

    // Root `target_class` across the allocation — `alloc_lookup` can move it.
    let pinned = match target_class {
        Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
        _ => None,
    };
    // Pinned after the target, so read and released first (a release frees
    // every pin from its handle onward).
    let prev_pinned = prev_class.map(|o| (ctx.pin_native_root(o), o));
    // Across modules the JDK drops `MODULE` (15, not 31).
    let modes = if prev_class.is_some() {
        LK_FULL_POWER_MODES & !LK_MODULE
    } else {
        LK_FULL_POWER_MODES
    };
    let obj = alloc_lookup(ctx, modes);
    let prev_class = match prev_pinned {
        Some((handle, o)) => {
            let current = ctx.read_native_pin(handle, o);
            ctx.unpin_native_roots(handle);
            Some(current)        }
        None => None,
    };
    let target_class = match pinned {
        Some((handle, o)) => {
            let current = ctx.read_native_pin(handle, o);
            ctx.unpin_native_roots(handle);
            Value::Object(Some(current))
        }
        None => target_class,
    };
    let obj = obj?;
    ctx.set_field(obj, LK_LOOKUP_CLASS_REF, target_class);
    if prev_class.is_some() {
        ctx.set_field_by_name(obj, "prevLookupClass", Value::Object(prev_class));
    }
    Ok(Some(Value::Object(Some(obj))))
}

/// `--real-jdk`: the module half of `MethodHandles.privateLookupIn(targetClass,
/// caller)`, asked after its mode gate admitted `caller`. JDK 25:
///
/// ```text
/// if (targetModule != callerModule) {
///     if (!callerModule.canRead(targetModule))
///         throw new IllegalAccessException(callerModule + " does not read " + targetModule);
///     if (targetModule.isNamed() && !targetModule.isOpen(pn, callerModule))
///         throw new IllegalAccessException(targetModule + " does not open " + pn + " to " + callerModule);
/// }
/// ```
///
/// The natives never asked it, so `privateLookupIn(Object.class, lookup())`
/// from the class path answered a full-power lookup on `java.lang`
/// (`tools/probes/interp/L5/L5W44LoaderReview.java`, `lookup-define-java-lang`;
/// `L5W45PrivateLookupInOpens`; interpreter round i1 wave 45, lane L5). Asked
/// through the `Module` methods themselves (`getModule`, `canRead`, `isOpen`,
/// `toString`), which answer a layer module from the JDK's fields and a boot
/// module from the registry (`--add-opens` included), so this adds no second
/// copy of the module rules. Both modes since interpreter round i1 wave 47,
/// lane L6: `--compatible` asked nothing before, so
/// `privateLookupIn(Object.class, lookup())` from the class path answered a
/// full-power lookup where HotSpot refuses
/// (`L6W47CompatibleAgentModuleOpens.java`). A
/// refusal is traced under `CRATONVM_DBG=access`.
///
/// Answers the target (read back: this runs Java) and, for an admitted
/// target of ANOTHER module, the caller's lookup class: the JDK then drops
/// `MODULE` and records that class as the new lookup's `prevLookupClass`
/// (`newModes &= ~Lookup.MODULE`, modes 15; interpreter round i1 wave 46,
/// lane L4). `None` for a same-module target, a TRUSTED caller, and every
/// "cannot tell" exit, which keep the same-module shape (modes 31, no
/// previous lookup class).
pub fn private_lookup_in_module_refusal(
    ctx: &mut dyn NativeContext,
    target_class: ObjectRef,
    caller_lookup: ObjectRef,
) -> Result<(ObjectRef, Option<ObjectRef>), MethodCallFailed> {
    use cratonvm_native_api::{NativeHandle, NativeHandleScope};
    fn truthy(r: Option<Value>) -> bool {
        matches!(r, Some(Value::Int(v)) if v != 0)
    }
    /// `Module.toString()` of the module `h` roots.
    fn describe(
        scope: &mut NativeHandleScope<'_>,
        h: &NativeHandle,
    ) -> Result<String, MethodCallFailed> {
        let m = scope.get(h);
        Ok(
            match scope.invoke_virtual(m, "toString", "()Ljava/lang/String;", &[])? {
                Some(Value::Object(Some(s))) => scope.read_string(s).unwrap_or_default(),
                _ => String::new(),
            },
        )
    }
    fn refuse(message: String) -> Result<(ObjectRef, Option<ObjectRef>), MethodCallFailed> {
        if crate::jboss_jdkspecific::dbg_layer_modules() {
            eprintln!("[ACCESS-DBG] privateLookupIn refused: {message}");
        }
        Err(cratonvm_types::error::RuntimeError::IllegalAccessException { message }.into())
    }
    // A TRUSTED caller returns `new Lookup(targetClass)` before any check.
    if lk_modes_of(ctx, caller_lookup) == -1 {
        return Ok((target_class, None));
    }
    let caller_class = match ctx.get_field_by_name(caller_lookup, "lookupClass") {
        Value::Object(Some(c)) => c,
        _ => match ctx.get_field(caller_lookup, LK_LOOKUP_CLASS_REF) {
            Value::Object(Some(c)) => c,
            _ => return Ok((target_class, None)),
        },
    };
    // `CRATONVM_MODULE_ACCESS_BY_IDENTITY`: the one module question, asked of
    // the VM's registry by the modules' identities, decides; the `Module`
    // methods below are asked only when it cannot answer, and to spell a
    // refusal's message. Nothing has run Java yet, so both refs are current.
    let by_identity = {
        private_lookup_in_by_identity(ctx, caller_class, target_class)
    };
    // A POSITIVE registry verdict decides. A NEGATIVE one (`NotReadable`,
    // `NotOpen`) is only "the registry holds no such edge": over a real JDK
    // `Module.addReads` / `addOpens` are the JDK's bytecode (SBR round 5 lane
    // compat2, `jdk-module-access`), which records an edge in
    // `ReflectionData` and tells the VM through `addExports0` only (exports,
    // never opens), so the registry is blind to every run-time opening. JFR's
    // `SecuritySupport.makeVisibleToJFR` opens `jdk.internal.event` to
    // `jdk.jfr` that way and `ensureClassIsInitialized` then
    // `privateLookupIn`s it (SBR round 5 lane fix1). The JDK's own `Module`
    // methods, asked below, are authoritative for a refusal (and answer from
    // the registry when `Module` is the native shadow), as HotSpot's
    // `privateLookupIn` asks them.
    let by_identity = match by_identity {
        Some(PrivateLookupInVerdict::SameModule) => return Ok((target_class, None)),
        Some(PrivateLookupInVerdict::CrossModule) => {
            return Ok((target_class, Some(caller_class)));
        }
        Some(PrivateLookupInVerdict::NotReadable | PrivateLookupInVerdict::NotOpen) | None => None,
    };
    let mut scope = NativeHandleScope::new(ctx);
    let target_h = scope.root(target_class);
    let caller_class_h = scope.root(caller_class);
    let Some(Value::Object(Some(caller_module))) =
        scope.invoke_virtual(caller_class, "getModule", "()Ljava/lang/Module;", &[])?
    else {
        return Ok((scope.get(&target_h), None));
    };
    let caller_module_h = scope.root(caller_module);
    let target_now = scope.get(&target_h);
    let Some(Value::Object(Some(target_module))) =
        scope.invoke_virtual(target_now, "getModule", "()Ljava/lang/Module;", &[])?
    else {
        return Ok((scope.get(&target_h), None));
    };
    let target_module_h = scope.root(target_module);
    if by_identity.is_none() && target_module == scope.get(&caller_module_h) {
        return Ok((scope.get(&target_h), None));
    }
    let reads = match by_identity {
        Some(PrivateLookupInVerdict::NotReadable) => false,
        Some(_) => true,
        None => {
            let caller_now = scope.get(&caller_module_h);
            let target_now = scope.get(&target_module_h);
            truthy(scope.invoke_virtual(
                caller_now,
                "canRead",
                "(Ljava/lang/Module;)Z",
                &[Value::Object(Some(target_now))],
            )?)
        }
    };
    if !reads {
        let caller_text = describe(&mut scope, &caller_module_h)?;
        let target_text = describe(&mut scope, &target_module_h)?;
        return refuse(format!("{caller_text} does not read {target_text}"));
    }
    let target_now = scope.get(&target_module_h);
    if by_identity.is_none()
        && !matches!(
            scope.get_field_by_name(target_now, "name"),
            Value::Object(Some(_))
        )
    {
        // An unnamed target module opens every package: admitted, across
        // modules.
        return Ok((scope.get(&target_h), Some(scope.get(&caller_class_h))));
    }
    let target_class_now = scope.get(&target_h);
    let Some(Value::Object(Some(pn))) = scope.invoke_virtual(
        target_class_now,
        "getPackageName",
        "()Ljava/lang/String;",
        &[],
    )?
    else {
        return Ok((scope.get(&target_h), None));
    };
    let pn_text = scope.read_string(pn).unwrap_or_default();
    let open = match by_identity {
        Some(_) => false, // `NotOpen`: the only verdict that reaches here
        None => {
            let target_now = scope.get(&target_module_h);
            let caller_now = scope.get(&caller_module_h);
            truthy(scope.invoke_virtual(
                target_now,
                "isOpen",
                "(Ljava/lang/String;Ljava/lang/Module;)Z",
                &[Value::Object(Some(pn)), Value::Object(Some(caller_now))],
            )?)
        }
    };
    if open {
        return Ok((scope.get(&target_h), Some(scope.get(&caller_class_h))));
    }
    let target_text = describe(&mut scope, &target_module_h)?;
    let caller_text = describe(&mut scope, &caller_module_h)?;
    refuse(format!(
        "{target_text} does not open {pn_text} to {caller_text}"
    ))
}



/// [`private_lookup_in_by_identity`]'s answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivateLookupInVerdict {
    /// The target is in the caller's module: the same-module lookup.
    SameModule,
    /// Another module, which the caller's reads and which opens the target's
    /// package to it: the cross-module lookup (modes 15, `prevLookupClass`).
    CrossModule,
    /// `IllegalAccessException: <caller module> does not read <target module>`.
    NotReadable,
    /// `IllegalAccessException: <target module> does not open <pn> to <caller module>`.
    NotOpen,
}

/// `privateLookupIn`'s module half (`targetModule != callerModule`, then
/// `canRead`, then `isOpen(pn, callerModule)` for a named target) as the one
/// module question by identity (`NativeClassAccess::module_access`,
/// `cratonvm_classloading::access_control::module_access`). `None` when a
/// question has no registry answer (a layer module's opens, a mirror with no
/// class): the caller asks the JDK's `Module` methods, as without the switch.
/// Pure reads; runs no Java. A decision is traced under `CRATONVM_DBG=access`
/// (the positive control).
pub fn private_lookup_in_by_identity(
    ctx: &mut dyn NativeContext,
    caller_class: ObjectRef,
    target_class: ObjectRef,
) -> Option<PrivateLookupInVerdict> {
    use cratonvm_native_api::ModuleQuestion;
    let caller = crate::lang_class::mirror_class_id(ctx, caller_class)?;
    let target = crate::lang_class::mirror_class_id(ctx, target_class)?;
    let verdict = if ctx.module_access(caller, target, ModuleQuestion::Same)? {
        PrivateLookupInVerdict::SameModule
    } else if !ctx.module_access(caller, target, ModuleQuestion::Read)? {
        PrivateLookupInVerdict::NotReadable
    } else if ctx.module_access(caller, target, ModuleQuestion::Open)? {
        PrivateLookupInVerdict::CrossModule
    } else {
        PrivateLookupInVerdict::NotOpen
    };
    if crate::jboss_jdkspecific::dbg_layer_modules() {
        eprintln!(
            "[ACCESS-DBG] privateLookupIn by identity: {} -> {} {verdict:?}",
            ctx.class_name_of_id(caller).unwrap_or_default(),
            ctx.class_name_of_id(target).unwrap_or_default(),
        );
    }
    Some(verdict)
}

/// `publicLookup()`'s modes are **exactly** `UNCONDITIONAL` (0x20).
///
/// Verified against JDK 25 `java.base/java/lang/invoke/MethodHandles.java`:
/// `publicLookup()` returns `Lookup.PUBLIC_LOOKUP`, which is
/// `new Lookup(Object.class, null, UNCONDITIONAL)`, and `lookupModes()`
/// returns `allowedModes & ALL_MODES` (ALL_MODES includes UNCONDITIONAL).
/// This used to answer `PUBLIC|UNCONDITIONAL` (0x21) — a combination the JDK
/// itself treats as impossible: `Lookup.toString()` switches on the exact
/// mode word, has a `case UNCONDITIONAL` arm and no `PUBLIC|UNCONDITIONAL`
/// arm, and its `default:` branch asserts false.
///
/// Dropping the PUBLIC bit does not narrow access here:
/// `lang_invoke::lk_enforce_find_access` — the gate that actually runs — needs
/// no mode bit for a `public` member of a `public` class, and requires
/// `PRIVATE` for a non-public one, which this lookup never had. What it DOES
/// key on is `modes == UNCONDITIONAL` exactly, so leaving the PUBLIC bit set
/// would have skipped that arm and let `publicLookup()` reach public members
/// of package-private classes.
///
/// (This registration sits on `MethodHandles$Lookup`; the live
/// `MethodHandles.publicLookup()` static is `lang_invoke.rs`'s. Both now agree.)
pub fn lk_public_lookup(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let obj = alloc_lookup(ctx, LK_UNCONDITIONAL);
    Ok(Some(Value::Object(Some(obj?))))
}

pub fn lk_lookup_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(ctx.get_field(this, LK_LOOKUP_CLASS_REF)))
}

/// `Lookup.previousLookupClass()` — the lookup class of the Lookup this one
/// was derived from by a MODULE-CROSSING `in()`, or null.
///
/// Measured on OpenJDK 25.0.3: it is null for `MethodHandles.lookup()`,
/// `publicLookup()`, `dropLookupMode(PRIVATE)`, `in(<the lookup class
/// itself>)` and `in(<a nestmate>)` — every lookup that never left its
/// module — and becomes non-null only once `in()` crosses a module boundary
/// (`lookup().in(String.class)` reports the original lookup class, and its
/// `toString()` renders as `java.lang.String/PrevLk/public`). CratonVM does
/// not model modules, and neither `alloc_lookup` nor `lk_in_method` ever
/// populates the field, so **null is the correct answer for every Lookup this
/// VM hands out**. What matters here is only that the answer is a REFERENCE.
///
/// The slot has to be chosen by layout, not assumed. `LK_PREVIOUS_LOOKUP_CLASS`
/// is 2, and on the real JDK 25 layout slot 2 is `allowedModes`, an `int`
/// (`javap -p java.lang.invoke.MethodHandles$Lookup`, instance fields in
/// declaration order: `lookupClass`(0), `prevLookupClass`(1),
/// `allowedModes`(2), `cachedProtectionDomain`(3)). Reading it raw therefore
/// returned an `Int` — the mode word, 95 for a full-power lookup — out of a
/// native whose descriptor is `()Ljava/lang/Class;`. A caller storing that
/// into a `Class` local holds a type-confused value, and the reference slot it
/// lands in is one the GC scans as an oop.
///
/// The witness is the same CLASS-side one `lk_real_allowed_modes_slot` uses,
/// so the two never disagree about which layout the receiver has.
pub fn lk_previous_lookup_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let value = match lk_real_prev_lookup_class_slot(ctx, this) {
        Some(slot) => ctx.get_field(this, slot),
        None => ctx.get_field(this, LK_PREVIOUS_LOOKUP_CLASS),
    };
    // Whatever the layout turned out to be, a `()Ljava/lang/Class;` native must
    // not return a primitive. A non-reference here means a layout this VM does
    // not model, and null is this method's own legal answer for "no previous
    // lookup class" — the answer the real JDK gives for every non-module-
    // crossing Lookup, which is all of them here.
    Ok(Some(match value {
        Value::Object(_) => value,
        _ => Value::Object(None),
    }))
}

pub fn lk_lookup_modes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(Value::Int(lk_modes_of(ctx, this))))
}

pub fn lk_has_full_privilege_access(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let modes = lk_modes_of(ctx, this);
    let full = (modes & LK_PRIVATE) != 0 && (modes & LK_MODULE) != 0;
    Ok(Some(Value::Int(if full { 1 } else { 0 })))
}

/// JDK 25: `hasPrivateAccess() { return hasFullPrivilegeAccess(); }` --
/// `PRIVATE` and `MODULE`, not `PRIVATE` alone. The two differ only for a
/// lookup with `PRIVATE` and no `MODULE`, which a cross-module
/// `privateLookupIn` makes (modes 15; measured `false`,
/// `L4W46PrivateLookupInAcrossModules.java` row
/// `full-priv`; interpreter round i1 wave 46, lane L4).
pub fn lk_has_private_access(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    lk_has_full_privilege_access(ctx, args)
}

pub fn lk_ensure_initialized(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let _this = obj_arg(args, 0)?;
    let target_class = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("Lookup.ensureInitialized target class is null".to_string()),
            }
            .into());
        }
    };
    let class_id = crate::lang_class::mirror_class_id(ctx, target_class).ok_or_else(|| {
        cratonvm_types::error::RuntimeError::IllegalArgumentException {
            message: "Lookup.ensureInitialized target is not a Class mirror".to_string(),
        }
    })?;
    // Family-1 stale-ObjectRef fix (2026-07-13): `ctx.initialize_class` runs
    // the target's `<clinit>`, which can allocate and trigger a moving GC.
    // `target_class` is a raw `ObjectRef` captured above and was being
    // returned again after this call without being refreshed — exactly the
    // "held across a GC-triggering call" pattern documented in
    // wildfly-parallel-boot-stale-objectref-residual.md.
    // Root and re-read it around the call.
    let target_class_pin = ctx.pin_native_root(target_class);
    // HIB-CV-26 fix (2026-07-16): propagate the real `<clinit>` failure
    // instead of re-wrapping it as an unrecoverable `VmError::Internal` —
    // matches real JDK `Lookup.ensureInitialized`, which throws
    // `ExceptionInInitializerError` for a failed initializer. Interpreter
    // round i2 wave 58, lane L3: listed under the JDK frames HotSpot raises
    // it in.
    crate::lang_class::lookup_ensure_initialized_class(ctx, class_id)?;
    let target_class = ctx.read_native_pin(target_class_pin, target_class);
    ctx.unpin_native_roots(target_class_pin);
    Ok(Some(Value::Object(Some(target_class))))
}

pub fn lk_define_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Lookup.defineClass(byte[]) -> Class
    // WP2.3: routes through `define_class_full`. Differs from
    // defineHiddenClass: this path defines a NORMAL class under the
    // lookup class's loader and namespace, using the class's own
    // `this_class` name (no mangling). Throws an
    // IllegalArgumentException on bad magic / parse error per JLS
    // §5.3.5.
    use cratonvm_types::error::RuntimeError;

    let byte_array = match args.get(1) {
        Some(Value::Object(Some(arr))) => *arr,
        Some(Value::Object(None)) => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "Lookup.defineClass: bytes must not be null".into(),
            }
            .into());
        }
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "Lookup.defineClass: missing bytes argument".into(),
            }
            .into());
        }
    };
    let length = ctx.array_length(byte_array);
    // Defensive: catch any panic inside the copy loop so a corrupt
    // byte[] from a cglib path returns an IAE instead of SIGABRT.
    let class_bytes_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut bytes = Vec::with_capacity(length);
        for i in 0..length {
            match ctx.get_array_element(byte_array, i) {
                Value::Int(b) => bytes.push(b as u8),
                _ => bytes.push(0),
            }
        }
        bytes
    }));
    let class_bytes = match class_bytes_result {
        Ok(b) => b,
        Err(_) => {
            tracing::error!(
                "Lookup.defineClass: panic while reading byte array (len={length}); \
                 raising IllegalArgumentException"
            );
            return Err(RuntimeError::IllegalArgumentException {
                message: "Lookup.defineClass: panic while reading byte array".into(),
            }
            .into());
        }
    };

    // Validate magic + minimal length (8 bytes = magic + minor + major).
    // BAD BYTES ARE A `ClassFormatError`, not an `IllegalArgumentException`.
    // The `ClassLoader.defineClass` door three thousand lines above already
    // raises `LinkageError::ClassFormatError` here; these `Lookup` doors were
    // the two that did not, and a caller cannot catch what it is not thrown.
    // `ClassFormatError` is an `Error`; `IllegalArgumentException` is a
    // RuntimeException -- a bytecode generator that guards its emit with
    // `catch (ClassFormatError)` (which is what you write, because that is what
    // the JVM throws) sees nothing and lets a genuinely malformed class escape
    // as an unrelated runtime failure somewhere else. The text is the JDK's
    // own `Lookup.validateAndFindInternalName` refusal (interpreter round i4
    // wave 95, lane L5; `lookup_define::lookup_header_refusal`).
    if let Some(refusal) = crate::lookup_define::lookup_header_refusal(ctx, &class_bytes) {
        return Err(refusal);
    }

    // Sniff the class file's own `this_class` name so the cglib guard
    // can match on `$$EnhancerByCGLIB$$` even when the caller passes
    // no explicit name. Empty / parse-fail → empty string (guard noop).
    let sniffed_name = extract_this_class_name(&class_bytes).unwrap_or_default();

    // cglib SEGV guard — Lookup.defineClass is another entry point
    // that ASM-emitted proxies may use on modern JDK targets.
    if let Some(v) = cglib_guard_value(ctx, &sniffed_name, &class_bytes) {
        return Ok(Some(v));
    }

    // Extract the class file's own name; the backend will validate it
    // and reject mismatches with NoClassDefFoundError. We pass an
    // empty name so the backend skips its name-mismatch check (the
    // class file's `this_class` is authoritative here per JEP 274).
    let mut opts = cratonvm_native_api::DefineClassFull::default();
    // zs s8/nd7: the mirror object before the define (nothing raw is used
    // after this point); the sniffed name is only the pool hint.
    let reservation = crate::lang_system::reserve_mirror_for_define(ctx, &sniffed_name, &mut opts)?;
    // catch_unwind: backend may panic on malformed bytecode.
    let define_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ctx.define_class_full("", &class_bytes, 0, opts)
    }));
    let reserved = crate::lang_system::take_mirror_reservation(ctx, reservation);
    let define_result = match define_result {
        Ok(r) => r,
        Err(_) => {
            tracing::error!(
                "Lookup.defineClass: panic inside define_class_full; \
                 raising IllegalArgumentException"
            );
            return Err(RuntimeError::IllegalArgumentException {
                message: "Lookup.defineClass: panic inside backend".into(),
            }
            .into());
        }
    };
    match define_result {
        Ok(cid) => {
            let mirror = crate::lang_system::defined_class_mirror_reserved(ctx, cid, reserved);
            Ok(Some(Value::Object(Some(mirror))))
        }
        Err(msg) => Err(RuntimeError::IllegalArgumentException {
            message: format!("Lookup.defineClass: {msg}"),
        }
        .into()),
    }
}

/// Extract the internal form of the `this_class` constant pool entry from
/// a raw `.class` file. Returns `None` if the header is malformed or the
/// indices are out of range. Used by [`lk_define_hidden_class`] to derive
/// a HotSpot-style mangled name (`OriginalName/0x<id>`) without having
/// to fully parse the class file.
pub fn extract_this_class_name(bytes: &[u8]) -> Option<String> {
    // Class file layout prefix:
    //   u4 magic
    //   u2 minor_version
    //   u2 major_version
    //   u2 constant_pool_count
    //   cp_info constant_pool[constant_pool_count - 1]
    //   u2 access_flags
    //   u2 this_class        <-- we want this
    //   ...
    if bytes.len() < 10 {
        return None;
    }
    if bytes[0..4] != CLASS_FILE_MAGIC {
        return None;
    }
    let cp_count = u16::from_be_bytes([bytes[8], bytes[9]]) as usize;
    if cp_count == 0 {
        return None;
    }
    // PERF: we only need the single `this_class` name, not the whole pool.
    // The previous implementation allocated two HashMaps and a heap
    // `String` for *every* Utf8 entry on each defineClass-family call —
    // very hot for cglib/ByteBuddy-heavy apps that define many proxies.
    //
    // Instead we do one cheap linear scan that records only each constant
    // pool entry's *start byte offset* into a flat `Vec<u32>` (one slot
    // per pool index — no per-entry hashing, no per-Utf8 String alloc).
    // After the pool we read `this_class`, follow it to the CONSTANT_Class
    // entry's `name_index`, and decode exactly one Utf8 string. Behavior
    // (including all `None`/out-of-range failure cases) is identical to
    // the old HashMap version; only the one resolved name is allocated.
    //
    // offsets[i] = byte offset of constant pool entry `i` (the tag byte).
    // Index 0 is unused (the pool is 1-based); long/double entries leave
    // their second slot at the sentinel `u32::MAX` (unusable index).
    let mut offsets: Vec<u32> = vec![u32::MAX; cp_count];
    let mut pos = 10usize;
    let mut idx: usize = 1;
    while idx < cp_count {
        if pos >= bytes.len() {
            return None;
        }
        offsets[idx] = pos as u32;
        let tag = bytes[pos];
        pos += 1;
        match tag {
            1 => {
                // CONSTANT_Utf8 — u2 length, [u1]* bytes. We deliberately
                // do NOT decode the bytes here (the old code decoded every
                // Utf8); we only need to skip past it.
                if pos + 2 > bytes.len() {
                    return None;
                }
                let len = u16::from_be_bytes([bytes[pos], bytes[pos + 1]]) as usize;
                pos += 2;
                if pos + len > bytes.len() {
                    return None;
                }
                pos += len;
                idx += 1;
            }
            3 | 4 => {
                // CONSTANT_Integer / CONSTANT_Float — u4
                pos += 4;
                idx += 1;
            }
            5 | 6 => {
                // CONSTANT_Long / CONSTANT_Double — u8, consumes two indices
                pos += 8;
                idx += 2;
            }
            7 => {
                // CONSTANT_Class — u2 name_index (→ Utf8)
                if pos + 2 > bytes.len() {
                    return None;
                }
                pos += 2;
                idx += 1;
            }
            8 => {
                // CONSTANT_String — u2 string_index
                pos += 2;
                idx += 1;
            }
            9 | 10 | 11 | 12 | 17 | 18 => {
                // Fieldref / Methodref / InterfaceMethodref /
                // NameAndType / InvokeDynamic / Dynamic — u2 + u2
                pos += 4;
                idx += 1;
            }
            15 => {
                // CONSTANT_MethodHandle — u1 reference_kind + u2 reference_index
                pos += 3;
                idx += 1;
            }
            16 | 19 | 20 => {
                // MethodType / Module / Package — u2
                pos += 2;
                idx += 1;
            }
            _ => {
                // Unknown tag; we cannot safely continue parsing.
                return None;
            }
        }
    }

    // After the constant pool: u2 access_flags, u2 this_class.
    if pos + 4 > bytes.len() {
        return None;
    }
    let this_class_idx = u16::from_be_bytes([bytes[pos + 2], bytes[pos + 3]]) as usize;

    // Resolve `this_class` (a CONSTANT_Class) → its `name_index` Utf8.
    // Bounds-check the index and verify the tag matches, mirroring the
    // old code which returned `None` for a missing/mismatched entry.
    let class_off = *offsets.get(this_class_idx)? as usize;
    if class_off == u32::MAX as usize || class_off + 3 > bytes.len() {
        return None;
    }
    if bytes[class_off] != 7 {
        // `this_class` did not point at a CONSTANT_Class. The old code
        // returned `None` here (the name-index HashMap had no entry).
        return None;
    }
    let name_idx = u16::from_be_bytes([bytes[class_off + 1], bytes[class_off + 2]]) as usize;

    let name_off = *offsets.get(name_idx)? as usize;
    if name_off == u32::MAX as usize || name_off + 3 > bytes.len() {
        return None;
    }
    if bytes[name_off] != 1 {
        // name_index did not point at a CONSTANT_Utf8. The old code
        // returned `None` (the utf8 HashMap had no entry for it).
        return None;
    }
    let len = u16::from_be_bytes([bytes[name_off + 1], bytes[name_off + 2]]) as usize;
    let start = name_off + 3;
    let end = start.checked_add(len)?;
    if end > bytes.len() {
        return None;
    }
    // zt t27/lg1 (`CRATONVM_DEFINE_THIS_CLASS_MUTF8`, page
    // `zgc-zt13x10-review-residuals-*` item 7): the name is Modified UTF-8
    // (JVMS 4.4.7): a supplementary character is a surrogate pair of
    // three-byte forms and NUL is `C0 80`, both of which the lenient UTF-8
    // decode below turned into U+FFFD, so the nameless adoption looked up a
    // name no class has. Decoded exactly now; a malformed name falls back to
    // the lenient decode, as before. Only this one string is allocated.
    let raw = &bytes[start..end];
    // The common case, a plain ASCII name, reads the same either way.
    if raw.iter().all(|b| *b != 0 && b.is_ascii()) {
        return std::str::from_utf8(raw).ok().map(str::to_string);
    }
    {
        if let Some(name) = decode_modified_utf8_exact(raw) {
            return Some(name);
        }
    }
    // Lenient UTF-8: for the subset used in internal class names
    // (ASCII-safe `foo/Bar$Inner`) the modified and standard forms agree.
    Some(String::from_utf8_lossy(raw).into_owned())
}



/// zt t27/lg1: a Modified UTF-8 string (JVMS 4.4.7: one-, two- and
/// three-byte forms of UTF-16 units, `C0 80` for NUL, a supplementary
/// character as its two surrogates) as a Rust string; `None` for a malformed
/// sequence or an unpaired surrogate. Pure.
pub fn decode_modified_utf8_exact(raw: &[u8]) -> Option<String> {
    let mut units: Vec<u16> = Vec::with_capacity(raw.len());
    let mut i = 0usize;
    while i < raw.len() {
        let b0 = raw[i];
        let cont = |k: usize| raw.get(i + k).copied().filter(|b| b & 0xC0 == 0x80);
        if b0 & 0x80 == 0 && b0 != 0 {
            units.push(u16::from(b0));
            i += 1;
        } else if b0 & 0xE0 == 0xC0 {
            let b1 = cont(1)?;
            units.push((u16::from(b0 & 0x1F) << 6) | u16::from(b1 & 0x3F));
            i += 2;
        } else if b0 & 0xF0 == 0xE0 {
            let (b1, b2) = (cont(1)?, cont(2)?);
            units.push(
                (u16::from(b0 & 0x0F) << 12) | (u16::from(b1 & 0x3F) << 6) | u16::from(b2 & 0x3F),
            );
            i += 3;
        } else {
            return None;        }    }
    String::from_utf16(&units).ok()}


pub fn lk_define_hidden_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Lookup.defineHiddenClass(byte[], boolean, ClassOption...) → Lookup
    //
    // JVMS / JEP 371:
    //   * Parses the byte array, defines the class under a unique
    //     mangled name ("Foo/0x<counter>"), marks it hidden atomically.
    //   * Honors the `initialize` flag: if true, runs <clinit> now.
    //   * Honors the `NESTMATE` ClassOption: copies the lookup class's
    //     nest-host / nest-members onto the new class.
    //   * On any failure, throws the appropriate Java exception
    //     (IllegalArgumentException, ClassFormatError) so the caller
    //     observes a typed error rather than a silently-empty Lookup.
    use cratonvm_types::error::RuntimeError;

    // --- 1. Extract `this` (the defining Lookup) and the bytecode array. ---
    let this_lookup = obj_arg(args, 0)?;
    let byte_array = match args.get(1) {
        Some(Value::Object(Some(arr))) => *arr,
        Some(Value::Object(None)) => {
            // A NULL array is an NPE, as `Objects.requireNonNull(bytes)` in the
            // JDK's own body gives. Measured on jdk-25.0.3.9-hotspot.
            return Err(RuntimeError::NullPointerException {
                message: Some("defineHiddenClass: bytes must not be null".to_string()),
            }
            .into());
        }
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "defineHiddenClass: missing bytes argument".into(),
            }
            .into());
        }
    };

    let length = ctx.array_length(byte_array);
    // Defensive: catch any panic inside the copy loop so a corrupt
    // byte[] (cglib emits 10-50 KB bytecode buffers via ASM) returns
    // an IAE instead of SIGABRT.
    let class_bytes_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut bytes = Vec::with_capacity(length);
        for i in 0..length {
            match ctx.get_array_element(byte_array, i) {
                Value::Int(b) => bytes.push(b as u8),
                _ => bytes.push(0),
            }
        }
        bytes
    }));
    let class_bytes = match class_bytes_result {
        Ok(b) => b,
        Err(_) => {
            tracing::error!(
                "defineHiddenClass: panic while reading byte array (len={length}); \
                 raising IllegalArgumentException"
            );
            return Err(RuntimeError::IllegalArgumentException {
                message: "defineHiddenClass: panic while reading byte array".into(),
            }
            .into());
        }
    };

    // --- 2. Validate the class file magic + minimal header length. ---
    // Same rule as `Lookup.defineClass` above: malformed bytes are a
    // `ClassFormatError`, and an EMPTY array takes this path too (HotSpot
    // measures as `ClassFormatError`, not as a null-argument complaint).
    // The JDK's `validateAndFindInternalName` text (wave 95, lane L5).
    if let Some(refusal) = crate::lookup_define::lookup_header_refusal(ctx, &class_bytes) {
        return Err(refusal);
    }

    // --- 3. Extract the original `this_class` name for mangled naming.
    //        If extraction fails we fall back to "HiddenClass" — the
    //        mangle suffix still guarantees uniqueness.
    let original_name =
        extract_this_class_name(&class_bytes).unwrap_or_else(|| "HiddenClass".to_string());
    let id = HIDDEN_CLASS_COUNTER.fetch_add(1, Ordering::Relaxed);
    let hidden_name = format!("{original_name}/0x{id:016x}");

    // --- 4. Parse the `initialize` flag (arg 2). ---
    let initialize = matches!(args.get(2), Some(Value::Int(n)) if *n != 0);

    // --- 5. Parse the ClassOption[] varargs (arg 3). Each element is a
    //        synthetic ClassOption object; we read field 0 (ordinal) to
    //        detect NESTMATE (ordinal 0) vs STRONG (ordinal 1). The
    //        ordinal convention matches the JDK's enum declaration.
    let mut nestmate = false;
    // gc-common w18-d: STRONG (ordinal 1) is no longer advisory. Without it
    // the class unloads with its mirror.
    let mut strong = false;
    if let Some(Value::Object(Some(options_arr))) = args.get(3) {
        let opt_count = ctx.array_length(*options_arr);
        for i in 0..opt_count {
            if let Value::Object(Some(opt)) = ctx.get_array_element(*options_arr, i) {
                if let Value::Int(ord) = ctx.get_field(opt, 0) {
                    if ord == 0 {
                        nestmate = true;
                    }
                    if ord == 1 {
                        strong = true;
                    }
                }
            }
        }
    }

    // --- 6. WP2.3: Define the class via `define_class_full` with
    //        override_name + hidden + nest_host_class_name set
    //        atomically. The backend takes care of name mangling,
    //        hidden-flag stamping, nest attribution, JIT
    //        invalidation, and ProtectionDomain inheritance from the
    //        lookup class.

    // If NESTMATE was requested, look up the lookup class's name now
    // so we can pass it to the backend. The backend resolves it to
    // the actual nest_host (handles transitive nest membership).
    let nest_host_class_name = if nestmate {
        if let Value::Object(Some(lookup_mirror)) = ctx.get_field(this_lookup, LK_LOOKUP_CLASS_REF)
        {
            crate::lang_class::mirror_class_id(ctx, lookup_mirror)
                .and_then(|cid| ctx.class_name_of_id(cid))
        } else {
            None
        }
    } else {
        None
    };

    let opts = cratonvm_native_api::DefineClassFull {
        override_name: Some(hidden_name.clone()),
        hidden: true,
        // gci i11/cl: the NESTMATE arm passes the lookup class itself.
        lookup_class_name: nest_host_class_name.clone(),
        nest_host_class_name,
        initialize,
        non_strong_hidden: !strong,
        ..Default::default()
    };
    let non_strong_hidden = opts.unloads_with_its_mirror();

    // cglib SEGV guard — JEP 371 hidden-class path is used by some
    // ASM frameworks (incl. byte-buddy when configured to use hidden
    // classes). If cglib ever routes here, short-circuit before define.
    if let Some(v) = cglib_guard_value(ctx, &original_name, &class_bytes) {
        // Wrap the placeholder mirror back into a Lookup so the caller
        // gets the contractual return type. If the placeholder is null,
        // fall through to the normal path (which will fail cleanly).
        if let Value::Object(Some(mirror)) = v {
            // gc-common w20-f: the placeholder mirror was stored at its
            // address from before the Lookup's allocation (the w18-d fix
            // below covered only the main path). Pinned across it.
            let guard_pin = ctx.pin_native_root(mirror);
            let lookup_alloc = alloc_lookup(ctx, LK_FULL_POWER);
            let mirror = ctx.read_native_pin(guard_pin, mirror);
            ctx.unpin_native_roots(guard_pin);
            let lookup: ObjectRef = lookup_alloc?;
            ctx.set_field(lookup, LK_LOOKUP_CLASS_REF, Value::Object(Some(mirror)));
            return Ok(Some(Value::Object(Some(lookup))));
        }
    }

    // zs s8/nd7: the mirror object before the define (no reference is used
    // after this point but the mirror; a hidden class pools no name).
    let mut opts = opts;
    let reservation = crate::lang_system::reserve_mirror_for_define(ctx, &hidden_name, &mut opts)?;
    // catch_unwind: backend may panic on malformed bytecode.
    let define_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ctx.define_class_full(&hidden_name, &class_bytes, 0, opts)
    }));
    let reserved = crate::lang_system::take_mirror_reservation(ctx, reservation);
    let define_result = match define_result {
        Ok(r) => r,
        Err(_) => {
            tracing::error!(
                "defineHiddenClass({hidden_name}): panic inside define_class_full; \
                 raising IllegalArgumentException"
            );
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("defineHiddenClass({hidden_name}): panic inside backend"),
            }
            .into());
        }
    };
    let cid = match define_result {
        Ok(cid) => cid,
        Err(msg) => {
            // initialize=true failures: see
            // `lang_system::exception_in_initializer_error`'s doc comment
            // (JVMS 5.5 — HotSpot raises `ExceptionInInitializerError`
            // wrapping the real `<clinit>` exception). Other errors are
            // IllegalArgumentException per JLS §5.3.5.
            if msg.contains("initialize after define failed") {
                return Err(crate::lang_system::exception_in_initializer_error(
                    ctx,
                    &hidden_name,
                    &msg,
                ));
            }
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("defineHiddenClass({hidden_name}): {msg}"),
            }
            .into());
        }
    };

    // --- 8. Build the return value: a fresh Lookup whose lookup class
    //        is the hidden class's mirror. Full power mode lets the
    //        caller look up private members via the returned Lookup.
    //
    // gc-common w18-d: the mirror is pinned across the Lookup's allocation.
    // It was held raw, so a moving collection there left the Lookup naming
    // the mirror's old address.
    //
    // gc-common w20-f: the pin was never released (it stayed until the
    // enclosing native returned) and leaked on the allocation's `?` exit.
    let mirror = crate::lang_system::defined_class_mirror_reserved(ctx, cid, reserved);
    let mirror_pin = ctx.pin_native_root(mirror);
    let lookup_alloc = alloc_lookup(ctx, LK_FULL_POWER);
    let mirror = ctx.read_native_pin(mirror_pin, mirror);
    ctx.unpin_native_roots(mirror_pin);
    let obj: ObjectRef = lookup_alloc?;
    ctx.set_field(obj, LK_LOOKUP_CLASS_REF, Value::Object(Some(mirror)));
    // Last: nothing allocates after this, and the returned Lookup holds the
    // mirror.
    if non_strong_hidden {
        register_non_strong_hidden_class(ctx, cid);
    }
    Ok(Some(Value::Object(Some(obj))))
}

// MethodHandle synthetic layout:
// C19: anchor our synthetic slots past the real JDK's instance-field count
// (6: type, form, asTypeCache, asTypeSoftCache, customizationCount,
// updateInProgress) so that `set_field_by_name(mh, "type", ...)` — which
// resolves to the real-JDK slot 0 — does not clobber our data. Matches the
// layout used by lang_invoke::alloc_method_handle (MH_BASE = 16).
//   MH_BASE+0: kind (Int: 0=virtual, 1=static, 2=constructor, 3=getter, 4=setter,
//                    5=static_getter, 6=static_setter, 7=special)
//   MH_BASE+1: target_class (Object: Class mirror of the declaring class)
//   MH_BASE+2: name (Object: String — method or field name)
//   MH_BASE+3: type (Object: MethodType mirror or descriptor string)
//   MH_BASE+4: resolved_class_id (Int: ClassId.raw() for fast dispatch)
pub const MH_BASE: usize = 16;
pub const MH_KIND: usize = MH_BASE + 0;
pub const MH_TARGET_CLASS: usize = MH_BASE + 1;
pub const MH_NAME: usize = MH_BASE + 2;
pub const MH_TYPE: usize = MH_BASE + 3;
pub const MH_CLASS_ID: usize = MH_BASE + 4;
pub const MH_FIELD_COUNT: usize = MH_BASE + 5;

pub fn alloc_method_handle(
    ctx: &mut dyn NativeContext,
    kind: i32,
    class_mirror: Option<ObjectRef>,
    name: Option<ObjectRef>,
    method_type: Option<ObjectRef>,
) -> Result<ObjectRef, MethodCallFailed> {
    // gen r4w3/rooting: `try_alloc_concurrent_synthetic` (class init) and
    // `mirror_class_id` are GC points and the three caller-supplied refs are
    // stored after them; root them first (lowest pins) and re-read.
    let class_mirror_pin = class_mirror.map(|r| (ctx.pin_native_root(r), r));
    let name_pin = name.map(|r| (ctx.pin_native_root(r), r));
    let method_type_pin = method_type.map(|r| (ctx.pin_native_root(r), r));
    let inputs_base = [class_mirror_pin, name_pin, method_type_pin]
        .iter()
        .flatten()
        .map(|(p, _)| *p)
        .min();
    let mh = try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodHandle", MH_FIELD_COUNT)?;
    let class_mirror = class_mirror_pin.map(|(p, r)| ctx.read_native_pin(p, r));
    let name = name_pin.map(|(p, r)| ctx.read_native_pin(p, r));
    let method_type = method_type_pin.map(|(p, r)| ctx.read_native_pin(p, r));
    // GC-safety: `mirror_class_id`/`build_method_type_from_descriptor` below
    // can trigger a moving GC (classloading); `mh` is reused in the final
    // `set_field_by_name` unpinned otherwise.
    let mh_pin = ctx.pin_native_root(mh);
    ctx.set_field(mh, MH_KIND, Value::Int(kind));
    ctx.set_field(
        mh,
        MH_TARGET_CLASS,
        match class_mirror {
            Some(r) => Value::Object(Some(r)),
            None => Value::Object(None),
        },
    );
    ctx.set_field(
        mh,
        MH_NAME,
        match name {
            Some(r) => Value::Object(Some(r)),
            None => Value::Object(None),
        },
    );
    ctx.set_field(
        mh,
        MH_TYPE,
        match method_type {
            Some(r) => Value::Object(Some(r)),
            None => Value::Object(None),
        },
    );
    // Resolve class ID if class mirror is available
    if let Some(mirror) = class_mirror {
        if let Some(cid) = crate::lang_class::mirror_class_id(ctx, mirror) {
            let mh = ctx.read_native_pin(mh_pin, mh);
            ctx.set_field(mh, MH_CLASS_ID, Value::Int(cid.as_u32() as i32));
        }
    }
    // C19/C21: Populate the real-JDK `MethodHandle.type:MethodType` field
    // (resolved by name to slot 0) so `mh.type()` and JDK-internal reads
    // (`erasedType`, `parameterSlotCount`, LambdaForm walks) see a non-null
    // MethodType. Prefer the caller-provided method_type (a MethodType
    // mirror from the Lookup.findXxx JVM call); fall back to a synthetic
    // `()V` MethodType when nothing was supplied (e.g. lk_unreflect, where
    // the Java caller did not pass an explicit MethodType).
    // gen r4w3/rooting: `mirror_class_id` above may have collected; the
    // caller-supplied MethodType is read back through its pin.
    let method_type = method_type_pin.map(|(p, r)| ctx.read_native_pin(p, r));
    let mt_to_store = match method_type {
        Some(mt) => Some(mt),
        None => crate::lang_invoke::build_method_type_from_descriptor(ctx, "()V")?,
    };
    let mh = ctx.read_native_pin(mh_pin, mh);
    ctx.unpin_native_roots(mh_pin);
    if let Some(base) = inputs_base {
        ctx.unpin_native_roots(base);
    }
    if let Some(mt) = mt_to_store {
        ctx.set_field_by_name(mh, "type", Value::Object(Some(mt)));
    }
    Ok(mh)
}

// -----------------------------------------------------------------------
// DELETED 2026-08-12: `lk_member_access_flags`, `enforce_lookup_access` and
// the eleven `lk_find_*` natives that called it.
//
// SUPERSEDED, not merely unused. Every one of those triples —
// `findVirtual`, `findStatic`, `findConstructor`, `findGetter`, `findSetter`,
// `findStaticGetter`, `findStaticSetter`, `findSpecial`, `findVarHandle`,
// `findStaticVarHandle` — is registered by
// `lang_invoke::register_p63_method_handles_lookup`, and the registration
// site a few hundred lines below says in its own comment that this module
// must NOT re-register them ("that would overwrite the real implementations
// with incompatible stubs"). So these bodies had no registration in any
// mode: not `--fast-boot`, not `--real-jdk`, not `synthetic-jdk`. Nothing
// dispatched into them and nothing ever had.
//
// `dead_code` is allowed crate-wide (`lib.rs`), so nothing warned. What kept
// them looking alive was five unit tests aimed straight at them, green on
// every run since W3-1 and guarding an access check the VM does not invoke —
// which is precisely the defect W4-1 was filed for: `publicLookup()` reached
// a private method while a fully-written check for that exact case sat here
// passing its own tests. Deleting the tests alone would have left the next
// reader concluding the check exists. Both went, together, and the five
// assertions were re-pointed at `lang_invoke::lk_enforce_find_access` — the
// gate that actually runs, called as the first statement of all ten
// `lookup_find_*`. See that module's `#[cfg(test)]` block.
//
// NOT deleted, against what W4-1's own patch block prescribes:
// `lk_public_lookup`. It IS registered, on `MethodHandles$Lookup.publicLookup`
// a few hundred lines below, and its doc comment states the JDK 25 mode word
// it answers. W4-1's deletion list is wrong on that one entry.
// W7-62-ratchets-and-dead-code.md · W4-1-publiclookup-allowedmodes-never-checked.md
// -----------------------------------------------------------------------

pub fn lk_unreflect(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // unreflect(Method) -> MethodHandle — extract class/name from the Method object
    let method = match args.get(1) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    // C6: Method/Constructor use real-JDK field layout; read by name.
    let class_mirror = method
        .map(|m| ctx.get_field_by_name(m, "clazz"))
        .and_then(|v| match v {
            Value::Object(Some(r)) => Some(r),
            _ => None,
        });
    let name = method
        .map(|m| ctx.get_field_by_name(m, "name"))
        .and_then(|v| match v {
            Value::Object(Some(r)) => Some(r),
            _ => None,
        });
    let mh = alloc_method_handle(ctx, 0, class_mirror, name, None);
    Ok(Some(Value::Object(Some(mh?))))
}

pub fn lk_unreflect_special(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    lk_unreflect(ctx, args)
}

/// Two class mirrors' relationship, for [`lk_in_modes`]: `(same_class,
/// same_package, same_nest)`.
///
/// `same_nest` is the JDK's `VerifyAccess.isSamePackageMember`: same package
/// AND same outermost enclosing class. The JDK walks `getEnclosingClass()`;
/// we take the internal name up to the first `$`, which agrees with it for
/// every nested/inner/anonymous form the compiler emits (`p/Outer$Inner`,
/// `p/Outer$1`) — the one shape it over-approximates is a top-level class
/// whose SOURCE name literally contains `$`, which is legal but not a name
/// anything in this codebase produces.
///
/// An unresolvable mirror answers `(false, true, true)` — "a different class,
/// but do not additionally strip package or private access" — matching
/// `lang_invoke::lk_same_package`'s permissive default. Guessing "different
/// package" for a mirror we simply could not name would silently demote a
/// legitimate lookup to PUBLIC and turn every subsequent non-public
/// `find*` into a spurious `IllegalAccessException`.
///
/// JIT round 21 wave 10, lane reflect (`CRATONVM_LOOKUP_IN_PACKAGE_MEMBER`,
/// default ON; page `r21-review-w68-native-findings-20261019.md` item 1): when
/// both mirrors name a class, the answer is JDK 25's own. `same_class` is
/// class identity (`requestedLookupClass == this.lookupClass`), not the name;
/// `same_package` is `VerifyAccess.isSamePackage` (same defining loader, same
/// `getPackageName`), from `lang_reflect::runtime_package_of`, which places a
/// hidden class `p/Host$$Lambda/0x..` in `p`, not in `p/Host$$Lambda`;
/// `same_nest` is `isSamePackageMember`: same package and the same
/// `getOutermostEnclosingClass`, the `getEnclosingClass()` chain
/// ([`lk_outermost_enclosing`]). Not the nest host: JDK 25's `Lookup.in` never
/// reads `getNestHost`, so a NESTMATE hidden class (no `InnerClasses` /
/// `EnclosingMethod` entry naming it) is its own outermost class and loses
/// PRIVATE|PROTECTED moving to its host, as on HotSpot. Every read is a
/// lookup of already-loaded state: no class is loaded, so no GC point (both
/// callers read their argument mirrors again afterwards). `=0` restores the
/// name split below, which put a hidden class in the package named by its
/// `/0x` tail and guessed the nest from the first `$`.
///
/// `private_kept` (JIT round 21 wave 12, lane reflect; page
/// `r21-review-w10-findings-FIXED-20261020.md` item 3) says the caller's PRIVATE
/// mode survives `in`'s module step, so that JDK 25 consults
/// `isSamePackageMember` -- and through it `getEnclosingClass()` -- for a
/// same-package pair. Only then is the walk's refusal an error: the
/// `IncompatibleClassChangeError` HotSpot's `getDeclaringClass0` raises for a
/// hidden class whose `InnerClasses` self entry its outer class cannot
/// confirm (measured, HotSpot 25: `in()` both ways between such a class and
/// its outer class throws it). The error is built without allocating; the
/// callers propagate it at once, before their own allocation.
///
/// This entry never loads: an enclosing class that is not loaded yet is
/// compared by name. [`lk_class_relation_rooted`] is the loading form.
pub fn lk_class_relation(
    ctx: &mut dyn NativeContext,
    a: Value,
    b: Value,
    private_kept: bool,
) -> Result<(bool, bool, bool), MethodCallFailed> {
    lk_class_relation_in(ctx, a, b, private_kept, false)
}

/// [`lk_class_relation`] that, while `private_kept` and
/// `CRATONVM_LOOKUP_IN_LOADS_OUTER` is on, loads an enclosing class that is
/// not loaded yet, as JDK 25's `getEnclosingClass()` does on HotSpot (JIT
/// round 21 wave 16, lane lookup-in; page `r21-review-w10-findings-FIXED-20261020.md`
/// residual 3R). `getDeclaringClass0` (`compute_enclosing_class`) and
/// `getEnclosingMethod0` resolve the outer class through the inner class's
/// constant pool -- its defining loader, loading, not initializing -- so a
/// missing outer class is that resolution's error (`NoClassDefFoundError`),
/// and a spoofed pair whose outer class was not loaded yet still reaches
/// `check_for_inner_class`'s `IncompatibleClassChangeError`.
///
/// A load is a GC point, so every reference the caller reads after this call
/// must be in `roots`: each is pinned across the walk and written back with
/// its current address, `Ok` or `Err`. `a` and `b` are read only before the
/// first load (the walk works on class ids).
pub fn lk_class_relation_rooted(
    ctx: &mut dyn NativeContext,
    a: Value,
    b: Value,
    private_kept: bool,
    roots: &mut [Value],
) -> Result<(bool, bool, bool), MethodCallFailed> {
    if !private_kept || !lk_in_loads_outer_enabled(&*ctx) {
        return lk_class_relation_in(ctx, a, b, private_kept, false);
    }
    let mut base = None;
    let mut pins = Vec::with_capacity(roots.len());
    for v in roots.iter() {
        match *v {
            Value::Object(Some(o)) => {
                let handle = ctx.pin_native_root(o);
                base.get_or_insert(handle);
                pins.push(Some(handle));
            }
            _ => pins.push(None),
        }
    }
    let result = lk_class_relation_in(ctx, a, b, private_kept, true);
    for (v, pin) in roots.iter_mut().zip(pins) {
        if let (Some(handle), Value::Object(Some(o))) = (pin, *v) {
            *v = Value::Object(Some(ctx.read_native_pin(handle, o)));
        }
    }
    if let Some(base) = base {
        ctx.unpin_native_roots(base);
    }
    result
}

/// `CRATONVM_LOOKUP_IN_LOADS_OUTER` -- correctness, both modes, default ON,
/// `=0` keeps the by-name answer for an enclosing class that is not loaded
/// yet ([`lk_class_relation_rooted`]). Read once per VM (the VM's
/// [`LoaderIdentityTables`]); a context without VM tables (mock contexts)
/// reads the flag table every time.
pub fn lk_in_loads_outer_enabled(ctx: &dyn NativeContext) -> bool {
    const FLAG: &str = "CRATONVM_LOOKUP_IN_LOADS_OUTER";
    match ctx
        .loader_identity_tables()
        .and_then(|any| any.downcast_ref::<LoaderIdentityTables>())
    {
        Some(tables) => *tables
            .lookup_in_loads_outer
            .get_or_init(|| cratonvm_types::flags::runtime_flag_default_on(FLAG)),
        None => cratonvm_types::flags::runtime_flag_default_on(FLAG),
    }
}

/// [`lk_class_relation`] with the walk's `load` permission
/// ([`lk_outermost_enclosing`]).
pub fn lk_class_relation_in(
    ctx: &mut dyn NativeContext,
    a: Value,
    b: Value,
    private_kept: bool,
    load: bool,
) -> Result<(bool, bool, bool), MethodCallFailed> {
    if lk_in_package_member_enabled(&*ctx) {
        let id_of = |ctx: &dyn NativeContext, v: Value| match v {
            Value::Object(Some(m)) => crate::lang_class::mirror_class_id(ctx, m),
            _ => None,
        };
        let ai = id_of(&*ctx, a);
        let bi = id_of(&*ctx, b);
        if let (Some(ai), Some(bi)) = (ai, bi) {
            return lk_class_relation_by_id(ctx, ai, bi, private_kept, load);
        }
    }
    let ctx: &dyn NativeContext = &*ctx;
    let name_of = |v: Value| match v {
        Value::Object(Some(m)) => crate::lang_class::mirror_class_name(ctx, m),
        _ => None,
    };
    let (Some(an), Some(bn)) = (name_of(a), name_of(b)) else {
        return Ok((false, true, true));
    };
    if an == bn {
        return Ok((true, true, true));
    }
    let package_of = |n: &str| match n.rfind('/') {
        Some(i) => n[..i].to_string(),
        None => String::new(),
    };
    let outermost_of = |n: &str| match n.find('$') {
        Some(i) => n[..i].to_string(),
        None => n.to_string(),
    };
    let same_package = package_of(&an) == package_of(&bn);
    let same_nest = same_package && outermost_of(&an) == outermost_of(&bn);
    Ok((false, same_package, same_nest))
}

/// `CRATONVM_LOOKUP_IN_PACKAGE_MEMBER` (default ON; JIT round 21 wave 10, lane
/// reflect): [`lk_class_relation`] answers from class identity, the
/// hidden-aware runtime package and the `getEnclosingClass` chain. `0` restores
/// the internal-name split and the `$` nest guess.
///
/// Read once per VM since JIT round 21 wave 13 (review rev-native item 5;
/// `lang_class::lookup_in_package_member_per_vm`), not on every `Lookup.in`.
pub fn lk_in_package_member_enabled(ctx: &dyn NativeContext) -> bool {
    crate::lang_class::lookup_in_package_member_per_vm(ctx)
}

/// [`lk_class_relation`] for two resolved classes: `(same_class,
/// same_package, same_nest)` as JDK 25's `Lookup.in` computes them. A class
/// whose name cannot be read keeps the permissive `(false, true, true)` of an
/// unresolvable mirror. The outermost classes are taken in the JDK's order
/// (`getOutermostEnclosingClass(class1)`, the lookup class, first), and only
/// for a same-package pair, as `isSamePackageMember` does.
pub fn lk_class_relation_by_id(
    ctx: &mut dyn NativeContext,
    a: cratonvm_types::ClassId,
    b: cratonvm_types::ClassId,
    private_kept: bool,
    load: bool,
) -> Result<(bool, bool, bool), MethodCallFailed> {
    if a == b {
        return Ok((true, true, true));
    }
    let (Some(ap), Some(bp)) = (
        crate::lang_reflect::runtime_package_of(&*ctx, a),
        crate::lang_reflect::runtime_package_of(&*ctx, b),
    ) else {
        return Ok((false, true, true));
    };
    if ap != bp {
        return Ok((false, false, false));
    }
    let outer_a = lk_outermost_enclosing(ctx, a, private_kept, load)?;
    let outer_b = lk_outermost_enclosing(ctx, b, private_kept, load)?;
    Ok((false, true, outer_a == outer_b))
}

/// Bound on [`lk_outermost_enclosing`]'s walk; a cyclic attribute chain stops
/// there instead of hanging `Lookup.in`.
pub const LK_ENCLOSING_WALK_LIMIT: usize = 64;

/// `VerifyAccess.getOutermostEnclosingClass` (JDK 25), as an internal name:
/// follow `getEnclosingClass()` -- the `EnclosingMethod` class first, else the
/// declaring class of the class's own `InnerClasses` entry -- to the class
/// that has none. A link to a class that is not loaded yet answers that link's
/// name: the walk resolves only through already-loaded state (the class's own
/// loader's definition, else what the VM sees near it), because a load here
/// would be a GC point under its callers' unrooted mirrors. Two classes of one
/// loader whose chains reach the same name reach the same class, which is all
/// [`lk_class_relation_by_id`] compares (it checks the loader first).
///
/// A hidden class is matched by its stored `/0x` name, which no attribute
/// names, so it is its own outermost class -- `getEnclosingClass()` of a lambda
/// proxy or a `defineHiddenClass` class without such attributes is null on
/// HotSpot too.
///
/// A hidden class never takes its `InnerClasses` link (JIT round 21 wave 11,
/// lane reflect; page `r21-review-w10-findings-FIXED-20261020.md` item 3). Defined
/// from member-class bytes, its self entry names the mangled name (the define
/// renames it as HotSpot's parser renames `this_class`), so it matches here;
/// but JDK 25's `getEnclosingClass` reaches `getDeclaringClass0` for it, and
/// HotSpot's `Reflection::check_for_inner_class` refuses the link because the
/// outer class can only list the class-file name (this VM's
/// `lang_class::inner_classes_disagreement` refuses it the same way). Its
/// `EnclosingMethod` class, which `getEnclosingClass` takes without that
/// check, is still followed.
///
/// With `refuse` (wave 12: the caller's PRIVATE survives, so the JDK consults
/// this chain), that refused link is the error HotSpot throws: the
/// `IncompatibleClassChangeError` whose text is
/// `lang_class::inner_classes_disagreement`'s, the one this VM's
/// `getDeclaringClass0` raises, so `getDeclaringClass()` and `in()` agree.
/// Without `refuse`, or when the outer class is not loaded and `load` is off
/// (HotSpot would load it here), the hidden class is its own outermost class.
/// Under `CRATONVM_CLASS_INNER_CLASSES_CHECK=0`, where `getDeclaringClass0`
/// answers the outer class, the link is followed like any other.
///
/// With `load` as well (JIT round 21 wave 16, lane lookup-in;
/// `CRATONVM_LOOKUP_IN_LOADS_OUTER`, through [`lk_class_relation_rooted`]
/// only, whose caller roots its references), a link whose class is not loaded
/// yet is resolved as the class's own constant pool resolves it
/// (`lang_class::class_seen_by_referencing_class_checked`: its defining
/// loader, loading, not initializing), the step HotSpot's
/// `compute_enclosing_class` / `JVM_GetEnclosingMethodInfo` take with
/// `klass_at`. A failed resolution is that error (`NoClassDefFoundError` for a
/// missing outer class) under the same rule `getDeclaringClass0` throws it by
/// (`lang_class::outer_resolution_errors_apply`), so `in()` and
/// `getDeclaringClass()` agree; otherwise the walk keeps the by-name answer.
/// The loaded outer class then meets the hidden and spoofed-member checks
/// below and the walk continues through it.
pub fn lk_outermost_enclosing(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
    refuse: bool,
    load: bool,
) -> Result<Option<String>, MethodCallFailed> {
    let load = load && refuse;
    let mut cur = class_id;
    let Some(mut name) = ctx.class_name_of_id(cur) else {
        return Ok(None);
    };
    for _ in 0..LK_ENCLOSING_WALK_LIMIT {
        let (enclosing, member_link) = match ctx.enclosing_method(cur) {
            Some((enc, _, _)) if !enc.is_empty() => (enc, false),
            // JIT round 21 wave 19, lane innerclasses (page
            // `r21-review-w18-findings-FIXED-20261020.md` item 2): the member rule of
            // `getDeclaringClass0`, the FIRST self row deciding
            // (`lang_class::member_row_outer_of`,
            // `CRATONVM_DECLARING_CLASS_ANY_OUTER_ROW`).
            _ => match crate::lang_class::member_row_outer_of(
                &*ctx,
                &ctx.inner_classes(cur),
                &name,
            ) {
                Some(outer) => (outer, true),
                None => return Ok(Some(name)),
            },
        };
        let refused_member = member_link
            && ctx.is_class_hidden(cur)
            && crate::lang_class::class_inner_classes_check_enabled();
        let loader = ctx.loader_id_of_class(cur);
        let own = if loader > 2 {
            ctx.class_id_defined_by_loader_exact(&enclosing, loader as u32)
        } else {
            None
        };
        let mut next = own.or_else(|| ctx.class_id_by_name_near(&enclosing, cur));
        if next.is_none() && load {
            match crate::lang_class::class_seen_by_referencing_class_checked(ctx, cur, &enclosing) {
                Ok(id) => next = Some(id),
                Err(failed) if crate::lang_class::outer_resolution_errors_apply(&*ctx, cur) => {
                    return Err(failed);
                }
                Err(_) => {}
            }
        }
        if refused_member {
            // The hidden-class arm of `inner_classes_disagreement` reads two
            // names and returns; it is asked only when both names are known,
            // so none of its other (loading) arms is reached from here.
            if let Some(outer) = next {
                if refuse && ctx.class_name_of_id(outer).is_some() {
                    if let Some(message) =
                        crate::lang_class::inner_classes_disagreement(ctx, cur, outer)
                    {
                        return Err(
                            cratonvm_types::error::LinkageError::IncompatibleClassChangeError {
                                message,
                            }
                            .into(),
                        );
                    }
                }
            }
            return Ok(Some(name));
        }
        // JIT round 21 wave 14, lane innerclasses
        // (`CRATONVM_CLASS_INNER_CLASSES_SPOOF_CHECK`; page
        // `r21-spoofed-inner-classes-not-refused-20261019.md`): an ordinary
        // class's member link that the outer class's `InnerClasses` does not
        // confirm is `getDeclaringClass0`'s ICCE on HotSpot 25, so `in()`
        // throws it while PRIVATE survives (`R21LookupInSpoofedMember`). The
        // arm reads names and rows only: no load, no GC point.
        if member_link && refuse {
            if let Some(outer) = next {
                if let Some(message) =
                    crate::lang_class::spoofed_member_disagreement(&*ctx, cur, outer)
                {
                    return Err(
                        cratonvm_types::error::LinkageError::IncompatibleClassChangeError {
                            message,
                        }
                        .into(),
                    );
                }
            }
        }
        match next {
            Some(next) if next != cur => {
                name = ctx.class_name_of_id(next).unwrap_or(enclosing);
                cur = next;
            }
            _ => return Ok(Some(enclosing)),
        }
    }
    Ok(Some(name))
}

/// `MethodHandles.Lookup.in`'s TARGET-CLASS validity test, shared by both
/// registrations of the method.
///
/// The JDK opens `in` with three rejections, before any mode arithmetic:
///
/// ```java
/// Objects.requireNonNull(requestedLookupClass);
/// if (requestedLookupClass.isPrimitive())
///     throw new IllegalArgumentException(requestedLookupClass + " is a primitive class");
/// if (requestedLookupClass.isArray())
///     throw new IllegalArgumentException(requestedLookupClass + " is an array class");
/// ```
///
/// A native that only computes modes drops all three, and the drop is silent:
/// `lookup().in(int.class)` returns a Lookup over a primitive instead of
/// raising, and every later `find*` on it fails with a message naming the wrong
/// thing. Both `in` registrations — this file's (synthetic-JDK mode) and
/// `lang_invoke.rs::register_p63_method_handles_lookup`'s (both real-JDK arms)
/// — call this, for the same reason they share [`lk_in_modes`]: two copies of a
/// rule drift, and the drift is only visible from outside the VM.
///
/// Measured on OpenJDK 25.0.3: `lookup().in(int.class)` and
/// `lookup().in(String[].class)` both raise `IllegalArgumentException`;
/// `lookup().in(null)` raises `NullPointerException`. `regression-suite/src/
/// RJdkLookupIn.java` is the vector.
pub fn lk_check_in_target(
    ctx: &dyn NativeContext,
    target: Value,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    let mirror = match target {
        Value::Object(Some(m)) => m,
        // `in(null)` is an NPE in the JDK, not an IAE and not a silent
        // full-power Lookup over nothing. `Objects.requireNonNull`'s: no
        // message (measured, HotSpot 25: `getMessage()` is null; interpreter
        // round i1 wave 43, lane L4, `L4W43LookupTeleport` row `in-null`).
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: None,
            }
            .into());
        }
    };
    // `requestedLookupClass + "..."` is `Class.toString()`: bare for a
    // primitive ("int"), `class ` + the binary name for an array ("class
    // [Ljava.lang.String;"; measured, HotSpot 25, `L4W43LookupTeleport` row
    // `in-array`; interpreter round i1 wave 48, lane L4).
    let describe = |prefix: &str, kind: &str| {
        let name = crate::lang_class::mirror_class_name(ctx, mirror)
            .map(|n| n.replace('/', "."))
            .unwrap_or_else(|| "?".to_string());
        cratonvm_types::error::RuntimeError::IllegalArgumentException {
            message: format!("{prefix}{name} is {kind}"),
        }
    };
    if crate::lang_class::mirror_is_primitive(ctx, mirror) {
        return Err(describe("", "a primitive class").into());
    }
    if crate::lang_class::mirror_is_array(ctx, mirror) {
        return Err(describe("class ", "an array class").into());
    }
    Ok(())
}

pub fn lk_in_method(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let target = args.get(1).copied().unwrap_or(Value::Object(None));
    lk_check_in_target(ctx, target)?;
    // Read via `lk_modes_of`, which is correct against BOTH layouts. The old
    // `get_field(this, LK_ALLOWED_MODES)` here was the synthetic slot only; on
    // a real Lookup slot 1 is `prevLookupClass`, a reference, so the `Int` arm
    // missed and the modes silently defaulted.
    let modes = lk_modes_of(ctx, this);
    let lookup_class = ctx.get_field(this, LK_LOOKUP_CLASS_REF);
    // JDK 25 consults `isSamePackageMember` only while PRIVATE survives (no
    // module step in this registration; UNCONDITIONAL replaces every mode).
    let private_kept = modes & LK_PRIVATE != 0 && modes & LK_UNCONDITIONAL == 0;
    // The walk may load an enclosing class (GC point,
    // `CRATONVM_LOOKUP_IN_LOADS_OUTER`): `target` is the only reference read
    // after it, so it is the root.
    let mut roots = [target];
    let (same_class, same_package, same_nest) =
        lk_class_relation_rooted(ctx, lookup_class, target, private_kept, &mut roots)?;
    let target = roots[0];
    // A Lookup reporting 0 has no modes to narrow. Keep it at 0 rather than
    // inventing PUBLIC: `in()` never GRANTS access the receiver did not have.
    // Measured: `lookup().dropLookupMode(PUBLIC)` is 0, and `.in(String.class)`
    // / `.in(<package-mate>)` / `.in(<its own lookup class>)` are all 0.
    let target_is_public = match target {
        Value::Object(Some(m)) => crate::lang_class::mirror_is_public(ctx, m),
        _ => false,
    };
    let new_modes = if modes == 0 {
        0
    } else {
        lk_in_modes(modes, same_class, same_package, same_nest, target_is_public)
    };
    // gen r4w3/rooting: `alloc_lookup` can initialise the Lookup class (GC
    // point); the target mirror is stored after it, so root and re-read it.
    let target_pin = match target {
        Value::Object(Some(m)) => Some((ctx.pin_native_root(m), m)),
        _ => None,
    };
    let new_lk = alloc_lookup(ctx, new_modes)?;
    let target = match target_pin {
        Some((pin, m)) => {
            let m = ctx.read_native_pin(pin, m);
            ctx.unpin_native_roots(pin);
            Value::Object(Some(m))
        }
        None => target,
    };
    ctx.set_field(new_lk, LK_LOOKUP_CLASS_REF, target);
    Ok(Some(Value::Object(Some(new_lk))))
}

pub fn lk_drop_lookup_mode(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let drop_mode = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
    // See `lk_in_method` for why this must not read `LK_ALLOWED_MODES` raw.
    // A 0 stays 0: dropping a mode never GRANTS one, and the previous code
    // reached the same answer for a fresh synthetic Lookup (whose slot 1 reads
    // back `Int(0)`, so its `LK_FULL_POWER` default arm never fired either).
    let modes = lk_modes_of(ctx, this);
    let new_modes = match lk_drop_modes(modes, drop_mode) {
        Some(m) => m,
        // Measured on JDK 25: the message is exactly this.
        None => {
            return Err(
                cratonvm_types::error::RuntimeError::IllegalArgumentException {
                    message: format!("{drop_mode} is not a valid mode to drop"),
                }
                .into(),
            );
        }
    };
    // gen r4w3/rooting: `alloc_lookup` can initialise the Lookup class (GC
    // point); `this` is read after it, so root and re-read it.
    let this_pin = ctx.pin_native_root(this);
    let new_lk = alloc_lookup(ctx, new_modes)?;
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    let cls = ctx.get_field(this, LK_LOOKUP_CLASS_REF);
    ctx.set_field(new_lk, LK_LOOKUP_CLASS_REF, cls);
    Ok(Some(Value::Object(Some(new_lk))))
}

// ---------------------------------------------------------------------------
// java.security.ProtectionDomain natives
// ---------------------------------------------------------------------------

pub fn pd_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let code_source = args.get(1).copied().unwrap_or(Value::Object(None));
    let permissions = args.get(2).copied().unwrap_or(Value::Object(None));
    ctx.set_field(this, PD_CODE_SOURCE_REF, code_source);
    ctx.set_field(this, PD_PERMISSIONS_REF, permissions);
    ctx.set_field(this, PD_CLASS_LOADER_REF, Value::Object(None));
    Ok(None)
}

pub fn pd_get_code_source(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(ctx.get_field(this, PD_CODE_SOURCE_REF)))
}

pub fn pd_get_permissions(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(ctx.get_field(this, PD_PERMISSIONS_REF)))
}

pub fn pd_get_class_loader(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(ctx.get_field(this, PD_CLASS_LOADER_REF)))
}

pub fn pd_implies(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // All permissions implied by default in our stub
    Ok(Some(Value::Int(1)))
}

// ---------------------------------------------------------------------------
// java.security.CodeSource natives
// ---------------------------------------------------------------------------

pub fn cs_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let location = args.get(1).copied().unwrap_or(Value::Object(None));
    let certs = args.get(2).copied().unwrap_or(Value::Object(None));
    ctx.set_field(this, CS_LOCATION_REF, location);
    ctx.set_field(this, CS_CERTIFICATES_REF, certs);
    Ok(None)
}

pub fn cs_get_location(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(ctx.get_field(this, CS_LOCATION_REF)))
}

pub fn cs_get_certificates(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(ctx.get_field(this, CS_CERTIFICATES_REF)))
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register_classloader_natives(r: &mut NativeMethodRegistry) {
    // -----------------------------------------------------------------------
    // java/lang/ClassLoader
    // -----------------------------------------------------------------------
    let cl = CL_CLASS;

    r.register(cl, "<init>", "()V", cl_init_default, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(cl, "<init>", "(Ljava/lang/ClassLoader;)V", cl_init_parent, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        cl,
        "<init>",
        "(Ljava/lang/String;Ljava/lang/ClassLoader;)V",
        cl_init_name_parent, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        cl,
        "loadClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        cl_load_class, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        cl,
        "loadClass",
        "(Ljava/lang/String;Z)Ljava/lang/Class;",
        cl_load_class_resolve, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    // T19_H12_LOADCLASS_MODULE — JDK 25 package-private overload used by
    // `Class.forName(Module, String)`'s stock bytecode. Registering on
    // ClassLoader keeps real ClassLoader receivers correct; the
    // `Class.forName(Module, String)` native (lang_class.rs) bypasses
    // the broken JDK bytecode path entirely so we never dispatch this
    // virtual call onto a synthetic Module whose receiver-class drifts.
    r.register(
        cl,
        "loadClass",
        "(Ljava/lang/Module;Ljava/lang/String;)Ljava/lang/Class;",
        cl_load_class_module, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        cl,
        "findClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        cl_find_class, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        cl,
        "findClass",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/Class;",
        cl_find_class_module, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        cl,
        "defineClass",
        "(Ljava/lang/String;[BII)Ljava/lang/Class;",
        cl_define_class_basic, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        cl,
        "defineClass",
        "(Ljava/lang/String;[BIILjava/security/ProtectionDomain;)Ljava/lang/Class;",
        cl_define_class_pd, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(cl, "defineClass", "(Ljava/lang/String;Ljava/nio/ByteBuffer;Ljava/security/ProtectionDomain;)Ljava/lang/Class;", cl_define_class_bb, cratonvm_native_api::NativeKind::SyntheticStub);
    // WP2.3-C: JDK-internal defineClass0/1/2 natives that the public
    // overloads route through. CGLIB / direct user code typically calls
    // these via the public Java wrappers.
    register_classloader_define_class(r);

    // Round-16 (agent 16): defensive Unsafe.defineClass shim for cglib.
    // cglib proxy generation goes through `sun.misc.Unsafe.defineClass`,
    // which previously SEGV'd on null/oversized bytecode in the non-JIT
    // path. This shim validates args up-front and routes through the
    // shared `define_class_full` backend.
    // jdk.internal.misc.Unsafe — JDK 9+ public path that user code can't
    // reach directly but `jdk.internal.misc.Unsafe.getUnsafe()` callers
    // (some bytecode-manipulation libs) hit. Same shim covers both.
    r.register(
        "jdk/internal/misc/Unsafe",
        "defineClass",
        "(Ljava/lang/String;[BIILjava/lang/ClassLoader;Ljava/security/ProtectionDomain;)Ljava/lang/Class;",
        unsafe_define_class_defensive, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(cl, "resolveClass", "(Ljava/lang/Class;)V", cl_resolve_class, cratonvm_native_api::NativeKind::SyntheticStub);
    // `CRATONVM_LOADER_FIND_LOADED_NO_STANDIN` (default OFF): not registered
    // under `--real-jdk` (`classloader_real::find_loaded_standin_dropped`).
    if !crate::classloader_real::find_loaded_standin_dropped(r) {
        r.register(
            cl,
            "findLoadedClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            cl_find_loaded_class, cratonvm_native_api::NativeKind::SyntheticStub,
        );
    }
    r.register(cl, "getParent", "()Ljava/lang/ClassLoader;", cl_get_parent, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(cl, "getName", "()Ljava/lang/String;", cl_get_name, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        cl,
        "getSystemClassLoader",
        "()Ljava/lang/ClassLoader;",
        cl_get_system_class_loader, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        cl,
        "getPlatformClassLoader",
        "()Ljava/lang/ClassLoader;",
        cl_get_platform_class_loader, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        cl,
        "getResource",
        "(Ljava/lang/String;)Ljava/net/URL;",
        cl_get_resource, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        cl,
        "getResources",
        "(Ljava/lang/String;)Ljava/util/Enumeration;",
        cl_get_resources, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        cl,
        "getSystemResources",
        "(Ljava/lang/String;)Ljava/util/Enumeration;",
        cl_get_system_resources, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        cl,
        "getResourceAsStream",
        "(Ljava/lang/String;)Ljava/io/InputStream;",
        cl_get_resource_as_stream, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    // Install the exact URLClassLoader declarations here as well. This
    // registrar runs after the early servlet/S1 setup in real-JDK mode, so it
    // is the authoritative callback for concrete URLClassLoader resource
    // methods and their subclasses.
    r.register(
        UCL_CLASS,
        "getResource",
        "(Ljava/lang/String;)Ljava/net/URL;",
        cl_get_resource, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        UCL_CLASS,
        "getResources",
        "(Ljava/lang/String;)Ljava/util/Enumeration;",
        cl_get_resources, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        UCL_CLASS,
        "getResourceAsStream",
        "(Ljava/lang/String;)Ljava/io/InputStream;",
        cl_get_resource_as_stream, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    for builtin_cl in [
        "jdk/internal/loader/BuiltinClassLoader",
        "jdk/internal/loader/ClassLoaders$AppClassLoader",
        "jdk/internal/loader/ClassLoaders$PlatformClassLoader",
    ] {
        r.register(
            builtin_cl,
            "getResourceAsStream",
            "(Ljava/lang/String;)Ljava/io/InputStream;",
            cl_get_resource_as_stream, cratonvm_native_api::NativeKind::SyntheticStub,
        );
    }
    r.register(
        cl,
        "getDefinedPackage",
        "(Ljava/lang/String;)Ljava/lang/Package;",
        cl_get_defined_package, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        cl,
        "getDefinedPackages",
        "()[Ljava/lang/Package;",
        cl_get_defined_packages, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        cl,
        "setDefaultAssertionStatus",
        "(Z)V",
        cl_set_default_assertion_status, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        cl,
        "registerAsParallelCapable",
        "()Z",
        cl_register_as_parallel_capable, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        cl,
        "isRegisteredAsParallelCapable",
        "()Z",
        cl_is_registered_as_parallel_capable, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // -----------------------------------------------------------------------
    // java/net/URLClassLoader
    // -----------------------------------------------------------------------
    let ucl = UCL_CLASS;

    r.register(ucl, "<init>", "([Ljava/net/URL;)V", ucl_init_urls, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        ucl,
        "<init>",
        "([Ljava/net/URL;Ljava/lang/ClassLoader;)V",
        ucl_init_urls_parent, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        ucl,
        "<init>",
        "([Ljava/net/URL;Ljava/lang/ClassLoader;Ljava/net/URLStreamHandlerFactory;)V",
        ucl_init_urls_parent_factory, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        ucl,
        "findClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        ucl_find_class, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        ucl,
        "findResource",
        "(Ljava/lang/String;)Ljava/net/URL;",
        ucl_find_resource, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        ucl,
        "findResources",
        "(Ljava/lang/String;)Ljava/util/Enumeration;",
        ucl_find_resources, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(ucl, "getURLs", "()[Ljava/net/URL;", ucl_get_urls, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(ucl, "addURL", "(Ljava/net/URL;)V", ucl_add_url, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(ucl, "close", "()V", ucl_close, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        ucl,
        "newInstance",
        "([Ljava/net/URL;)Ljava/net/URLClassLoader;",
        ucl_new_instance, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        ucl,
        "newInstance",
        "([Ljava/net/URL;Ljava/lang/ClassLoader;)Ljava/net/URLClassLoader;",
        ucl_new_instance_parent, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // -----------------------------------------------------------------------
    // java/lang/invoke/MethodHandles$Lookup
    // -----------------------------------------------------------------------
    let lk = LK_CLASS;

    r.register(
        lk,
        "lookup",
        "()Ljava/lang/invoke/MethodHandles$Lookup;",
        lk_lookup, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(lk, "privateLookupIn", "(Ljava/lang/Class;Ljava/lang/invoke/MethodHandles$Lookup;)Ljava/lang/invoke/MethodHandles$Lookup;", lk_private_lookup_in, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        lk,
        "publicLookup",
        "()Ljava/lang/invoke/MethodHandles$Lookup;",
        lk_public_lookup, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(lk, "lookupClass", "()Ljava/lang/Class;", lk_lookup_class, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        lk,
        "previousLookupClass",
        "()Ljava/lang/Class;",
        lk_previous_lookup_class, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(lk, "lookupModes", "()I", lk_lookup_modes, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        lk,
        "hasFullPrivilegeAccess",
        "()Z",
        lk_has_full_privilege_access, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(lk, "hasPrivateAccess", "()Z", lk_has_private_access, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        lk,
        "ensureInitialized",
        "(Ljava/lang/Class;)Ljava/lang/Class;",
        lk_ensure_initialized, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    // BOTH of these lose their slot. `lookup_define::register_lookup_define_class`
    // re-registers the identical triples on the WP2.3-B implementations, and
    // runs after this function from both registrars (`lib.rs` and
    // `reflect_annotations.rs`), so `lk_define_class` / `lk_define_hidden_class`
    // below are never dispatched — `--dump-native-registry` reports
    // `owns_slot: true` on `lookup_define.rs:901` and `:909`.
    //
    // Kept, and kept in step with the winner, because that is cheaper than
    // re-deriving them if the ordering is ever reversed. Do NOT fix a measured
    // `Lookup.define*` defect here: a fix applied to this pair changes nothing
    // observable. (It cost a full 34-minute rebuild to learn that once.)
    r.register(lk, "defineClass", "([B)Ljava/lang/Class;", lk_define_class, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(lk, "defineHiddenClass", "([BZ[Ljava/lang/invoke/MethodHandles$Lookup$ClassOption;)Ljava/lang/invoke/MethodHandles$Lookup;", lk_define_hidden_class, cratonvm_native_api::NativeKind::SyntheticStub);
    // findVirtual/findStatic/findConstructor/findGetter/findSetter/findSpecial/
    // findVarHandle/findStaticVarHandle are all registered in
    // lang_invoke::register_p63_method_handles_lookup — do NOT re-register here
    // as that would overwrite the real implementations with incompatible stubs.
    //
    // `in` and `dropLookupMode` are the EXCEPTION this comment used to omit,
    // and the omission mattered: `lang_invoke::register_p63_method_handles_lookup`
    // registers `in` too (lang_invoke.rs, "Lookup.in(targetClass)"), so there are
    // two implementations and which one runs depends on the mode.
    //   * synthetic-JDK mode: `register_synthetic_overrides` calls
    //     `register_phase63_natives` (lib.rs) BEFORE
    //     `classloader::register_classloader_natives`, so THIS registration wins.
    //   * real-JDK mode: `register_synthetic_overrides` is a no-op and vm_init's
    //     real arm calls `register_p63_method_handles_lookup` directly, so
    //     lang_invoke's wins.
    // Keep this one: measured against OpenJDK 25.0.3 (see `lk_in_modes`),
    // lang_invoke's copy answers 31 for `in(<the lookup class itself>)` where
    // the JDK answers 95, and it treats a receiver reporting 0 modes as
    // FULL_POWER — i.e. `in()` GRANTS access. lang_invoke.rs is owned by another
    // lane; the two must be collapsed onto `lk_in_modes` there, not forked again
    // here.
    r.register(
        lk,
        "unreflect",
        "(Ljava/lang/reflect/Method;)Ljava/lang/invoke/MethodHandle;",
        lk_unreflect, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        lk,
        "unreflectSpecial",
        "(Ljava/lang/reflect/Method;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
        lk_unreflect_special, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        lk,
        "in",
        "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandles$Lookup;",
        lk_in_method, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        lk,
        "dropLookupMode",
        "(I)Ljava/lang/invoke/MethodHandles$Lookup;",
        lk_drop_lookup_mode, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // -----------------------------------------------------------------------
    // java/lang/ClassLoader$HiddenClass (stub)
    // -----------------------------------------------------------------------
    // No natives needed — just a data holder (2-field synthetic)

    // -----------------------------------------------------------------------
    // java/security/ProtectionDomain
    // -----------------------------------------------------------------------
    let pd = PD_CLASS;

    r.register(
        pd,
        "<init>",
        "(Ljava/security/CodeSource;Ljava/security/PermissionCollection;)V",
        pd_init, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        pd,
        "getCodeSource",
        "()Ljava/security/CodeSource;",
        pd_get_code_source, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        pd,
        "getPermissions",
        "()Ljava/security/PermissionCollection;",
        pd_get_permissions, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        pd,
        "getClassLoader",
        "()Ljava/lang/ClassLoader;",
        pd_get_class_loader, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(pd, "implies", "(Ljava/security/Permission;)Z", pd_implies, cratonvm_native_api::NativeKind::SyntheticStub);

    // -----------------------------------------------------------------------
    // java/security/CodeSource
    // -----------------------------------------------------------------------
    let cs = CS_CLASS;

    r.register(
        cs,
        "<init>",
        "(Ljava/net/URL;[Ljava/security/cert/Certificate;)V",
        cs_init, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(cs, "getLocation", "()Ljava/net/URL;", cs_get_location, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        cs,
        "getCertificates",
        "()[Ljava/security/cert/Certificate;",
        cs_get_certificates, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // -----------------------------------------------------------------------
    // java/io/ByteArrayInputStream — 4-field (buf=0, pos=1, mark=2, count=3)
    // -----------------------------------------------------------------------
    let bais = "java/io/ByteArrayInputStream";
    r.register(bais, "<init>", "([B)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let buf = args.get(1).copied().unwrap_or(Value::Object(None));
        let len = match buf {
            Value::Object(Some(arr)) => ctx.array_length(arr) as i32,
            _ => 0,
        };
        ctx.set_field(this, 0, buf); // buf
        ctx.set_field(this, 1, Value::Int(0)); // pos
        ctx.set_field(this, 2, Value::Int(0)); // mark
        ctx.set_field(this, 3, Value::Int(len)); // count
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bais, "<init>", "([BII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let buf = args.get(1).copied().unwrap_or(Value::Object(None));
        let off = args[2].as_int().unwrap_or(0);
        let len = args[3].as_int().unwrap_or(0);
        ctx.set_field(this, 0, buf); // buf
        ctx.set_field(this, 1, Value::Int(off)); // pos
        ctx.set_field(this, 2, Value::Int(off)); // mark
        ctx.set_field(this, 3, Value::Int(off + len)); // count
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bais, "read", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = ctx.get_field(this, 1).as_int().unwrap_or(0);
        let count = ctx.get_field(this, 3).as_int().unwrap_or(0);
        if pos >= count {
            return Ok(Some(Value::Int(-1)));
        }
        let arr = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Int(-1))),
        };
        let b = ctx
            .get_array_element(arr, pos as usize)
            .as_int()
            .unwrap_or(0);
        ctx.set_field(this, 1, Value::Int(pos + 1));
        Ok(Some(Value::Int(b & 0xff)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bais, "read", "([BII)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let dst = obj_arg(args, 1)?;
        let off = args[2].as_int().unwrap_or(0) as usize;
        let len = args[3].as_int().unwrap_or(0) as usize;
        let pos = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
        let count = ctx.get_field(this, 3).as_int().unwrap_or(0) as usize;
        if pos >= count {
            return Ok(Some(Value::Int(-1)));
        }
        let avail = count - pos;
        let n = len.min(avail);
        let arr = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Int(-1))),
        };
        let mut bytes = vec![0u8; n];
        let copied = ctx.read_byte_array_into(arr, pos, &mut bytes);
        if copied > 0 {
            ctx.write_byte_array_from(dst, off, &bytes[..copied]);
        }
        ctx.set_field(this, 1, Value::Int((pos + copied) as i32));
        Ok(Some(Value::Int(copied as i32)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bais, "available", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = ctx.get_field(this, 1).as_int().unwrap_or(0);
        let count = ctx.get_field(this, 3).as_int().unwrap_or(0);
        Ok(Some(Value::Int((count - pos).max(0))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bais, "skip", "(J)J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let n = match args.get(1) {
            Some(Value::Long(v)) => *v,
            Some(Value::Int(v)) => *v as i64,
            _ => 0,
        };
        let pos = ctx.get_field(this, 1).as_int().unwrap_or(0);
        let count = ctx.get_field(this, 3).as_int().unwrap_or(0);
        let avail = (count - pos).max(0) as i64;
        let skipped = n.min(avail);
        ctx.set_field(this, 1, Value::Int(pos + skipped as i32));
        Ok(Some(Value::Long(skipped)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // DELETED wave 4 (2026-07-28): `ByteArrayInputStream.close()V` was
    // registered here as a no-op. It was a DEAD registration —
    // `native-io::register_io_natives` registers the same triple (bound to
    // `native_bais_close`, itself `Ok(None)`) and runs strictly after this
    // registrar in every VM init path (`vm/src/vm/vm_init.rs`). The surviving
    // native-io copy carries the justification: the real JDK body is empty and
    // the class documents that closing has no effect.
    r.register(bais, "reset", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let mark = ctx.get_field(this, 2).as_int().unwrap_or(0);
        ctx.set_field(this, 1, Value::Int(mark));
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bais, "mark", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = ctx.get_field(this, 1).as_int().unwrap_or(0);
        ctx.set_field(this, 2, Value::Int(pos)); // mark = pos
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // KEEP (real JDK body is `return true;`) — re-verified wave 4, 2026-07-28.
    // `ByteArrayInputStream` is backed by an in-memory byte[], so mark/reset
    // is always supported; the `mark`/`reset` natives registered just above
    // genuinely implement it against slot 2, and this triple has no rival
    // registration anywhere in the tree (native-io registers BAIS
    // read/available/skip/reset/close but NOT markSupported), so this really
    // is the answer callers get.
    r.register(bais, "markSupported", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // -----------------------------------------------------------------------
    // java/io/FilterInputStream — field 0 = wrapped InputStream
    // -----------------------------------------------------------------------
    let fis = "java/io/FilterInputStream";
    r.register(fis, "<init>", "(Ljava/io/InputStream;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stream = args.get(1).copied().unwrap_or(Value::Object(None));
        ctx.set_field(this, 0, stream); // in
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // -----------------------------------------------------------------------
    // java/io/DataInputStream — extends FilterInputStream
    //   field 0 = in (from FilterInputStream)
    //   field 1 = readBuffer (byte[8], allocated in <init>)
    //   field 2 = bytearr
    //   field 3 = chararr
    //
    // We provide native readInt/readShort/readUnsignedShort/readLong/readUTF/
    // readBoolean/readByte/readFully that recursively unwrap FilterInputStream
    // chains (DIS → BIS → BAIS) and read directly from the BAIS data.
    // -----------------------------------------------------------------------
    let dis = "java/io/DataInputStream";

    // Helper: read a single byte from a stream object, advancing pos.
    // Recursively unwraps FilterInputStreams until it finds a BAIS.
    fn dis_read_byte(ctx: &mut dyn NativeContext, stream: ObjectRef) -> Option<u8> {
        let cid = ctx.class_id_of_object(stream);
        let cname = ctx.class_name_of_id(cid).unwrap_or_default();
        if cname == "java/io/ByteArrayInputStream" {
            // BAIS: buf=0, pos=1, mark=2, count=3
            let pos = ctx.get_field(stream, 1).as_int().unwrap_or(0);
            let count = ctx.get_field(stream, 3).as_int().unwrap_or(0);
            if pos >= count {
                return None;
            }
            let buf = match ctx.get_field(stream, 0) {
                Value::Object(Some(a)) => a,
                _ => return None,
            };
            let b = ctx
                .get_array_element(buf, pos as usize)
                .as_int()
                .unwrap_or(0);
            ctx.set_field(stream, 1, Value::Int(pos + 1));
            Some((b & 0xFF) as u8)
        } else if cname == "java/io/BufferedInputStream" {
            let pos = ctx
                .get_field_by_name(stream, "pos")
                .as_int()
                .unwrap_or(0)
                .max(0) as usize;
            let count = ctx
                .get_field_by_name(stream, "count")
                .as_int()
                .unwrap_or(0)
                .max(0) as usize;
            if pos < count {
                if let Value::Object(Some(buf)) = ctx.get_field_by_name(stream, "buf") {
                    let b = ctx.get_array_element(buf, pos).as_int().unwrap_or(0);
                    ctx.set_field_by_name(stream, "pos", Value::Int((pos + 1) as i32));
                    return Some((b & 0xFF) as u8);
                }
            }
            let inner = match ctx.get_field_by_name(stream, "in") {
                Value::Object(Some(inner)) => Some(inner),
                _ => match ctx.get_field(stream, 0) {
                    Value::Object(Some(inner)) => Some(inner),
                    _ => None,
                },
            }?;
            let b = dis_read_byte(ctx, inner)?;
            let markpos = ctx
                .get_field_by_name(stream, "markpos")
                .as_int()
                .unwrap_or(-1);
            if markpos >= 0 {
                if let Value::Object(Some(buf)) = ctx.get_field_by_name(stream, "buf") {
                    let cap = ctx.array_length(buf);
                    let count = ctx
                        .get_field_by_name(stream, "count")
                        .as_int()
                        .unwrap_or(0)
                        .max(0) as usize;
                    let marklimit = ctx
                        .get_field_by_name(stream, "marklimit")
                        .as_int()
                        .unwrap_or(0)
                        .max(0) as usize;
                    if count < cap && count.saturating_sub(markpos as usize) < marklimit {
                        ctx.set_array_element(buf, count, Value::Int(b as i8 as i32));
                        ctx.set_field_by_name(stream, "count", Value::Int((count + 1) as i32));
                        ctx.set_field_by_name(stream, "pos", Value::Int((count + 1) as i32));
                    } else {
                        ctx.set_field_by_name(stream, "markpos", Value::Int(-1));
                    }
                }
            }
            Some(b)
        } else {
            // FilterInputStream: field 0 = in
            match ctx.get_field(stream, 0) {
                Value::Object(Some(inner)) => dis_read_byte(ctx, inner),
                _ => None,
            }
        }
    }

    fn dis_read_n(ctx: &mut dyn NativeContext, stream: ObjectRef, n: usize) -> Vec<u8> {
        let mut buf = Vec::with_capacity(n);
        for _ in 0..n {
            match dis_read_byte(ctx, stream) {
                Some(b) => buf.push(b),
                None => break,
            }
        }
        buf
    }

    r.register(dis, "<init>", "(Ljava/io/InputStream;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stream = args.get(1).copied().unwrap_or(Value::Object(None));
        ctx.set_field(this, 0, stream); // in (FilterInputStream.in)
                                        // readBuffer = new byte[8]
        let this_pin = ctx.pin_native_root(this);
        let rb = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 8);
        // gc-common w20-f: the receiver is pinned across the buffer's
        // allocation (it was written through its pre-allocation address).
        let this: ObjectRef = ctx.read_native_pin(this_pin, this);
        ctx.unpin_native_roots(this_pin);
        ctx.set_field(this, 1, Value::Object(Some(rb)));
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    r.register(dis, "readInt", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = dis_read_n(ctx, this, 4);
        if bytes.len() < 4 {
            return Err(cratonvm_types::error::RuntimeError::IOException {
                message: "EOF in readInt".into(),
            }
            .into());
        }
        let v = i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        Ok(Some(Value::Int(v)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    r.register(dis, "readShort", "()S", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = dis_read_n(ctx, this, 2);
        if bytes.len() < 2 {
            return Err(cratonvm_types::error::RuntimeError::IOException {
                message: "EOF in readShort".into(),
            }
            .into());
        }
        let v = i16::from_be_bytes([bytes[0], bytes[1]]);
        Ok(Some(Value::Int(v as i32)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    r.register(dis, "readUnsignedShort", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = dis_read_n(ctx, this, 2);
        if bytes.len() < 2 {
            return Err(cratonvm_types::error::RuntimeError::IOException {
                message: "EOF in readUnsignedShort".into(),
            }
            .into());
        }
        let v = u16::from_be_bytes([bytes[0], bytes[1]]);
        Ok(Some(Value::Int(v as i32)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    r.register(dis, "readLong", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = dis_read_n(ctx, this, 8);
        if bytes.len() < 8 {
            return Err(cratonvm_types::error::RuntimeError::IOException {
                message: "EOF in readLong".into(),
            }
            .into());
        }
        let v = i64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]);
        Ok(Some(Value::Long(v)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    r.register(dis, "readBoolean", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = dis_read_n(ctx, this, 1);
        if bytes.is_empty() {
            return Err(cratonvm_types::error::RuntimeError::IOException {
                message: "EOF in readBoolean".into(),
            }
            .into());
        }
        Ok(Some(Value::Int(if bytes[0] != 0 { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    r.register(dis, "readByte", "()B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = dis_read_n(ctx, this, 1);
        if bytes.is_empty() {
            return Err(cratonvm_types::error::RuntimeError::IOException {
                message: "EOF in readByte".into(),
            }
            .into());
        }
        Ok(Some(Value::Int(bytes[0] as i8 as i32)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    r.register(dis, "readUnsignedByte", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = dis_read_n(ctx, this, 1);
        if bytes.is_empty() {
            return Err(cratonvm_types::error::RuntimeError::IOException {
                message: "EOF in readUnsignedByte".into(),
            }
            .into());
        }
        Ok(Some(Value::Int(bytes[0] as i32)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    r.register(dis, "readChar", "()C", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = dis_read_n(ctx, this, 2);
        if bytes.len() < 2 {
            return Err(cratonvm_types::error::RuntimeError::IOException {
                message: "EOF in readChar".into(),
            }
            .into());
        }
        let v = u16::from_be_bytes([bytes[0], bytes[1]]);
        Ok(Some(Value::Int(v as i32)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    r.register(dis, "readFloat", "()F", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = dis_read_n(ctx, this, 4);
        if bytes.len() < 4 {
            return Err(cratonvm_types::error::RuntimeError::IOException {
                message: "EOF in readFloat".into(),
            }
            .into());
        }
        let bits = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        Ok(Some(Value::Float(f32::from_bits(bits))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    r.register(dis, "readDouble", "()D", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = dis_read_n(ctx, this, 8);
        if bytes.len() < 8 {
            return Err(cratonvm_types::error::RuntimeError::IOException {
                message: "EOF in readDouble".into(),
            }
            .into());
        }
        let bits = u64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]);
        Ok(Some(Value::Double(f64::from_bits(bits))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    r.register(dis, "readFully", "([BII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let dst = obj_arg(args, 1)?;
        let off = args[2].as_int().unwrap_or(0) as usize;
        let len = args[3].as_int().unwrap_or(0) as usize;
        let bytes = dis_read_n(ctx, this, len);
        if bytes.len() < len {
            return Err(cratonvm_types::error::RuntimeError::IOException {
                message: "EOF in readFully".into(),
            }
            .into());
        }
        for (i, &b) in bytes.iter().enumerate() {
            ctx.set_array_element(dst, off + i, Value::Int(b as i8 as i32));        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    r.register(dis, "readFully", "([B)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let dst = obj_arg(args, 1)?;
        let len = ctx.array_length(dst);
        let bytes = dis_read_n(ctx, this, len);
        if bytes.len() < len {
            return Err(cratonvm_types::error::RuntimeError::IOException {
                message: "EOF in readFully".into(),
            }
            .into());
        }
        for (i, &b) in bytes.iter().enumerate() {
            ctx.set_array_element(dst, i, Value::Int(b as i8 as i32));        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // readUTF() — reads modified UTF-8 string (2-byte length prefix + data)
    r.register(dis, "readUTF", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Read 2-byte length prefix
        let len_bytes = dis_read_n(ctx, this, 2);
        if len_bytes.len() < 2 {
            return Err(cratonvm_types::error::RuntimeError::IOException {
                message: "EOF in readUTF".into(),
            }
            .into());
        }
        let utf_len = u16::from_be_bytes([len_bytes[0], len_bytes[1]]) as usize;
        let data = dis_read_n(ctx, this, utf_len);
        if data.len() < utf_len {
            return Err(cratonvm_types::error::RuntimeError::IOException {
                message: "EOF in readUTF data".into(),
            }
            .into());
        }
        // Decode modified UTF-8
        let mut chars = Vec::new();
        let mut i = 0;
        while i < data.len() {
            let b = data[i];
            if b == 0 {
                break;
            }
            if b < 0x80 {
                chars.push(b as char);
                i += 1;
            } else if b & 0xE0 == 0xC0 {
                if i + 1 >= data.len() {
                    break;
                }
                let c = ((b as u32 & 0x1F) << 6) | (data[i + 1] as u32 & 0x3F);
                chars.push(char::from_u32(c).unwrap_or('?'));
                i += 2;
            } else if b & 0xF0 == 0xE0 {
                if i + 2 >= data.len() {
                    break;
                }
                let c = ((b as u32 & 0x0F) << 12)
                    | ((data[i + 1] as u32 & 0x3F) << 6)
                    | (data[i + 2] as u32 & 0x3F);
                chars.push(char::from_u32(c).unwrap_or('?'));
                i += 3;
            } else {
                chars.push('?');
                i += 1;
            }
        }
        let s: String = chars.into_iter().collect();
        let obj = ctx.create_string(&s);
        Ok(Some(Value::Object(Some(obj))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // read()I — single byte
    r.register(dis, "read", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        match dis_read_byte(ctx, this) {
            Some(b) => Ok(Some(Value::Int(b as i32))),
            None => Ok(Some(Value::Int(-1))),
        }
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // read([BII)I — bulk read
    r.register(dis, "read", "([BII)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let dst = obj_arg(args, 1)?;
        let off = args[2].as_int().unwrap_or(0) as usize;
        let len = args[3].as_int().unwrap_or(0) as usize;
        let bytes = dis_read_n(ctx, this, len);
        if bytes.is_empty() {
            return Ok(Some(Value::Int(-1)));
        }
        for (i, &b) in bytes.iter().enumerate() {
            ctx.set_array_element(dst, off + i, Value::Int(b as i8 as i32));
        }
        Ok(Some(Value::Int(bytes.len() as i32)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // skipBytes(int)int
    r.register(dis, "skipBytes", "(I)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let n = args[1].as_int().unwrap_or(0).max(0) as usize;
        let bytes = dis_read_n(ctx, this, n);
        Ok(Some(Value::Int(bytes.len() as i32)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // available()I — delegate to underlying stream
    r.register(dis, "available", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Walk down to BAIS
        fn avail(ctx: &dyn NativeContext, s: ObjectRef) -> i32 {
            let cid = ctx.class_id_of_object(s);
            let cname = ctx.class_name_of_id(cid).unwrap_or_default();
            if cname == "java/io/ByteArrayInputStream" {
                let pos = ctx.get_field(s, 1).as_int().unwrap_or(0);
                let count = ctx.get_field(s, 3).as_int().unwrap_or(0);
                (count - pos).max(0)
            } else {
                match ctx.get_field(s, 0) {
                    Value::Object(Some(inner)) => avail(ctx, inner),
                    _ => 0,
                }
            }
        }
        Ok(Some(Value::Int(avail(ctx, this))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // `DataInputStream` declares no bytecode of its own for `close()` (real
    // JDK inherits `FilterInputStream.close()` → `in.close()`); registering
    // a native directly on `DataInputStream` pre-empts that inherited real
    // bytecode. This registration is normally DEAD in practice — real-JDK
    // mode's boot sequence calls `native-io::register_io_natives` (which
    // registers its own, now-fixed `DataInputStream.close` →
    // `native_dis_close`) AFTER whatever calls this function, so that
    // registration wins. Fixed here too for consistency / in case
    // registration order ever changes; see `native-io/src/lib.rs`'s
    // `native_dis_close` for the full root-cause writeup (a `FileDataBlock`
    // handle leak in Spring Boot loader's
    // `SecurityInfoTests`/`NestedJarFileTests`, root-caused via a minimal
    // Spring-Boot-independent repro).
    r.register(dis, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let underlying = match ctx.get_field_by_name(this, "in") {
            Value::Object(Some(u)) => Some(u),
            _ => match ctx.get_field(this, 0) {
                Value::Object(Some(u)) => Some(u),
                _ => None,
            },
        };
        // `DataInputStream` inherits `FilterInputStream.close()`, which is a
        // bare `in.close()` under `throws IOException` with no `catch`, so the
        // delegated failure PROPAGATES. Dropping it hid exactly the class of
        // fault this registration was added to stop leaking.
        // W7-57-close-flush-swallow-sweep.md
        if let Some(u) = underlying {
            ctx.invoke_virtual(u, "close", "()V", &[])?;        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // -----------------------------------------------------------------------
    // java/io/BufferedInputStream — extends FilterInputStream
    //   field 0 = in (FilterInputStream.in)
    //   Plus: initialSize, buf, count, pos, markpos, marklimit
    // We provide <init> and delegate read() to the underlying stream directly.
    // -----------------------------------------------------------------------
    let bis = "java/io/BufferedInputStream";
    r.register(bis, "<init>", "(Ljava/io/InputStream;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stream = args.get(1).copied().unwrap_or(Value::Object(None));
        ctx.set_field(this, 0, stream); // in
        ctx.set_field_by_name(this, "in", stream);
        // gc-common w20-f: receiver pinned across the buffer's allocation.
        let this_pin = ctx.pin_native_root(this);
        let buf = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 8192);
        let this: ObjectRef = ctx.read_native_pin(this_pin, this);
        ctx.unpin_native_roots(this_pin);
        ctx.set_field_by_name(this, "initialSize", Value::Int(8192));
        ctx.set_field_by_name(this, "buf", Value::Object(Some(buf)));
        ctx.set_field_by_name(this, "count", Value::Int(0));
        ctx.set_field_by_name(this, "pos", Value::Int(0));
        ctx.set_field_by_name(this, "markpos", Value::Int(-1));
        ctx.set_field_by_name(this, "marklimit", Value::Int(0));
        ctx.set_field(this, 1, Value::Object(Some(buf)));
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bis, "<init>", "(Ljava/io/InputStream;I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stream = args.get(1).copied().unwrap_or(Value::Object(None));
        let size = args.get(2).and_then(Value::as_int).unwrap_or(8192).max(1);
        ctx.set_field(this, 0, stream); // in
        ctx.set_field_by_name(this, "in", stream);
        // gc-common w20-f: receiver pinned across the buffer's allocation.
        let this_pin = ctx.pin_native_root(this);
        let buf = ctx.new_array(cratonvm_types::ArrayElementType::Byte, size as usize);
        let this: ObjectRef = ctx.read_native_pin(this_pin, this);
        ctx.unpin_native_roots(this_pin);
        ctx.set_field_by_name(this, "initialSize", Value::Int(size));
        ctx.set_field_by_name(this, "buf", Value::Object(Some(buf)));
        ctx.set_field_by_name(this, "count", Value::Int(0));
        ctx.set_field_by_name(this, "pos", Value::Int(0));
        ctx.set_field_by_name(this, "markpos", Value::Int(-1));
        ctx.set_field_by_name(this, "marklimit", Value::Int(0));
        ctx.set_field(this, 1, Value::Object(Some(buf)));
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bis, "read", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        match dis_read_byte(ctx, this) {
            Some(b) => Ok(Some(Value::Int(b as i32))),
            None => Ok(Some(Value::Int(-1))),
        }
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bis, "read", "([BII)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let dst = obj_arg(args, 1)?;
        let off = args[2].as_int().unwrap_or(0) as usize;
        let len = args[3].as_int().unwrap_or(0) as usize;
        let bytes = dis_read_n(ctx, this, len);
        if bytes.is_empty() {
            return Ok(Some(Value::Int(-1)));
        }
        for (i, &b) in bytes.iter().enumerate() {
            ctx.set_array_element(dst, off + i, Value::Int(b as i8 as i32));
        }
        Ok(Some(Value::Int(bytes.len() as i32)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bis, "skip", "(J)J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let n = args.get(1).and_then(Value::as_long).unwrap_or(0).max(0) as usize;
        let bytes = dis_read_n(ctx, this, n);
        Ok(Some(Value::Long(bytes.len() as i64)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bis, "mark", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let readlimit = args.get(1).and_then(Value::as_int).unwrap_or(0).max(0);
        let pos = ctx
            .get_field_by_name(this, "pos")
            .as_int()
            .unwrap_or(0)
            .max(0);
        ctx.set_field_by_name(this, "marklimit", Value::Int(readlimit));
        ctx.set_field_by_name(this, "markpos", Value::Int(pos));
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bis, "reset", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let markpos = ctx
            .get_field_by_name(this, "markpos")
            .as_int()
            .unwrap_or(-1);
        if markpos >= 0 {
            ctx.set_field_by_name(this, "pos", Value::Int(markpos));        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // KEEP (real JDK body is `return true;`) — re-verified wave 4, 2026-07-28.
    // `BufferedInputStream` overrides `markSupported` with an unconditional
    // `true`, and the `mark`/`reset` natives registered just above genuinely
    // implement it against `markpos`/`marklimit`. Shadowing note: `lib.rs`
    // registers this same triple (also `1`) from the essential-natives path;
    // the two agree, so last-registration-wins is harmless — but change both
    // together if the answer ever stops being constant.
    r.register(bis, "markSupported", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bis, "available", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        fn avail(ctx: &dyn NativeContext, s: ObjectRef) -> i32 {
            let cid = ctx.class_id_of_object(s);
            let cname = ctx.class_name_of_id(cid).unwrap_or_default();
            if cname == "java/io/ByteArrayInputStream" {
                let pos = ctx.get_field(s, 1).as_int().unwrap_or(0);
                let count = ctx.get_field(s, 3).as_int().unwrap_or(0);
                (count - pos).max(0)
            } else {
                match ctx.get_field(s, 0) {
                    Value::Object(Some(inner)) => avail(ctx, inner),
                    _ => 0,
                }
            }
        }
        Ok(Some(Value::Int(avail(ctx, this))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // Unlike `DataInputStream`, `BufferedInputStream` DOES declare its own
    // `close()` in real JDK bytecode (`bufUpdater.compareAndSet(...) ...
    // input.close()`) — it doesn't inherit from `FilterInputStream`, so the
    // interpreter's dispatch correctly prefers that real bytecode over this
    // registration regardless (this native is not reached in practice under
    // real-JDK mode). Left as a no-op intentionally: `native-io`'s Wave2 H2
    // fix explicitly relies on real BIS bytecode (`Unsafe
    // .compareAndSetReference`-backed lazy `buf` allocation) and its comment
    // there asks future changes NOT to add more layout-coupled natives for
    // this class without a demonstrated regression.
    //
    // STUB-REMOVAL (wave 3): the no-op body is still wrong wherever this DOES
    // win (synthetic-JDK mode, where there is no real BIS bytecode to yield
    // to): real `BufferedInputStream.close()` closes the wrapped stream, so a
    // no-op leaked the underlying stream/fd on every `try (var in = new
    // BufferedInputStream(...))`. Delegate to the wrapped stream by NAME (`in`,
    // inherited from `FilterInputStream`) so no field-index coupling is added,
    // per the note above. `buf` is deliberately left alone — nulling it is what
    // real JDK does, but the native-io H2 fix relies on the lazy `buf`
    // allocation and this native must not disturb it.
    r.register(bis, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(inner)) = ctx.get_field_by_name(this, "in") {
            ctx.invoke_virtual(inner, "close", "()V", &[])?;        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // -----------------------------------------------------------------------
    // cglib probe — formerly short-circuited (CglibProbe.main / <clinit> /
    // <init> / Greeter no-ops) to dodge a failure in the `defineClass`
    // path. Root cause fixed: custom `ClassLoader` subclasses had a null
    // `defaultDomain` field because the simplified real-JDK
    // `ClassLoader.<init>` natives skipped the real ctor's
    // `defaultDomain = new ProtectionDomain(...)` initialiser; the JDK
    // `preDefineClass` bytecode then NPE'd on `defaultDomain.getCodeSource()`.
    // `classloader_real.rs::init_classloader_common_fields` now builds a
    // non-null `defaultDomain`, so the real cglib `Enhancer.create()` →
    // `defineClass` path runs. No probe shims registered here.
    // -----------------------------------------------------------------------
    // Enumeration$Impl — 2-field (array=0, index=1)
    // Used by getResources() to return an Enumeration over URL[].
    // Delegated to the shared registrar so real-JDK mode gets the same
    // natives without duplicating the closures here.
    // -----------------------------------------------------------------------
    register_enumeration_impl_natives(r);
}

