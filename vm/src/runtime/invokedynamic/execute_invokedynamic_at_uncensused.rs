#[allow(unused_imports)]
use super::*;

/// [`execute_invokedynamic_at`] proper (its census wrapper times it).
pub(in crate::runtime::invokedynamic) fn execute_invokedynamic_at_uncensused(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    singleton_site: Option<(u64, usize)>,
) -> Result<(), MethodCallFailed> {
    let current_class_id = thread.frames[frame_idx].class_id;

    // --- Fast path: check call site cache ---
    //
    // Lock-order discipline (the H2 TestScript class-resolution DEADLOCK):
    // clone the cached site OUT of the `resolution_cache` read guard and
    // DROP the guard before executing. Holding it across
    // `execute_cached_call_site` runs arbitrary code under the lock —
    // StringConcat allocates Strings (`alloc_java_string_object` takes
    // `class_manager.write()`), lambdas invoke Java — while
    // `resolve_field_ref` takes class_manager THEN resolution_cache: a
    // textbook ABBA inversion that wedged main-vm against a concat worker
    // with every resolver queued behind the writers.
    //
    // A lambda site -- the hottest cached shape after string concatenation --
    // needs only two `Copy` facts to execute (the proxy class id and the
    // capture count), so it is not cloned at all: a whole `LambdaCallSite`
    // clone is seven `Arc` refcount round-trips plus a `Vec<char>` allocation,
    // paid on every evaluation of every lambda expression. The other shapes
    // are taken out as the cache's own `Arc` -- one refcount bump, where a
    // clone of a `TypeSwitch` / `EnumSwitch` / `RecordObjectMethod` site was a
    // `Vec` allocation (three for a record method) per execution.
    //
    // Before either: this thread's own copy of the site (`IndySiteCache`),
    // which takes no lock and no hash probe — and, since wave 16, no refcount
    // (an `Other` entry lends its `Arc` out, see `CachedIndySite::Other`) and
    // no hash of the method name for a zero-capture lambda's singleton key
    // (memoised in the entry, checked against the frame's pc).
    if let Some(site) = thread.indy_sites.get_mut(current_class_id, cp_index) {
        match site {
            CachedIndySite::Lambda {
                proxy_class_id,
                num_captures,
                singleton_key,
                singleton,
            } => {
                let (proxy_class_id, num_captures, memo) =
                    (*proxy_class_id, *num_captures, *singleton_key);
                let pc = thread.frames[frame_idx].pc;
                let memo = memo.filter(|&(_, memo_pc)| memo_pc == pc);
                // Wave 20: a zero-capture hit at the memo's pc reads the
                // singleton from the slot this entry learnt, with no lock on
                // `LAMBDA_SINGLETON_CACHE`; the first such hit on this thread
                // asks the table and learns the slot.
                if let (None, 0, Some(key)) = (singleton_site, num_captures, memo) {
                    if let Some(obj) = singleton.as_deref().and_then(LambdaSingletonSlot::get) {
                        thread.frames[frame_idx]
                            .stack
                            .push(Value::Object(Some(obj)))?;
                        return Ok(());
                    }
                    allocate_lambda_proxy(shared, thread, frame_idx, proxy_class_id, 0, Some(key))?;
                    let slot = lambda_singleton_slot(shared, proxy_class_id, key);
                    if let Some(CachedIndySite::Lambda {
                        proxy_class_id: entry_proxy,
                        singleton_key: Some(entry_key),
                        singleton,
                        ..
                    }) = thread.indy_sites.get_mut(current_class_id, cp_index)
                    {
                        // The same row still (a re-entrant evaluation may
                        // have refilled the entry).
                        if *entry_proxy == proxy_class_id && *entry_key == key {
                            *singleton = slot;
                        }
                    }
                    return Ok(());
                }
                return allocate_lambda_proxy(
                    shared,
                    thread,
                    frame_idx,
                    proxy_class_id,
                    num_captures,
                    singleton_site.or(memo),
                );
            }
            CachedIndySite::Other(lent) => {
                if let Some(site) = lent.take() {
                    let outcome = execute_cached_call_site(shared, thread, frame_idx, &site);
                    return_lent_indy_site(thread, current_class_id, cp_index, site);
                    return outcome;
                }
                // Lent to an enclosing execution of this site on this thread:
                // the shared path below answers (and refills the entry).
            }
        }
    }
    // Then this thread's copy of a PER-INSTRUCTION linkage (a shared lambda
    // entry, a Groovy cast; `ThreadInstructionSites`): no lock, no hash of the
    // method key. Before the per-entry `resolution_cache` probe, which such
    // an entry always misses — it never has a per-entry row
    // (`publish_lambda_instruction_site`, and a generic bootstrap writes none),
    // so the order changes no answer. Only for an interpreter frame, whose
    // method and pc ARE the instruction.
    if singleton_site.is_none() && ANY_GENERIC_INDY_SITE.load(std::sync::atomic::Ordering::Acquire)
    {
        let ident = InstructionIdent::of_frame(
            shared,
            &thread.frames[frame_idx],
            current_class_id,
            cp_index,
        );
        if let Some((link, method_key)) = probe_thread_instruction_site(&ident) {
            if link != ThreadInstructionLink::LinkedRow {
                return run_thread_instruction_link(shared, thread, frame_idx, link, method_key);
            }
            // Wave 133 L2b (`CRATONVM_INDY_GENERIC_THREAD_ROW`): only filled
            // with the switch on. A gone row falls through to the shared path.
            if let Some(outcome) = run_thread_linked_row(
                shared,
                thread,
                frame_idx,
                current_class_id,
                cp_index,
                method_key,
            ) {
                return outcome;
            }
        }
    }
    // Read BEFORE the `resolution_cache` probe; see `SiteCache::put`.
    let epochs_at_entry = IndySiteCache::epochs_for(shared);
    let cached_site = {
        let cache = shared.classes.resolution_cache.read();
        cache
            .get_call_site_arc(current_class_id, cp_index)
            .map(|site| match &**site {
                ResolvedCallSite::Lambda(lcs) => CachedIndySite::Lambda {
                    proxy_class_id: lcs.proxy_class_id,
                    num_captures: lcs.capture_types.len(),
                    singleton_key: None,
                    singleton: None,
                },
                _ => CachedIndySite::Other(Some(Arc::clone(site))),
            })
    };
    match cached_site {
        Some(CachedIndySite::Lambda {
            proxy_class_id,
            num_captures,
            ..
        }) => {
            // An interpreter frame's zero-capture singleton key, computed once
            // here and memoised in the entry. The compiled bridge's frame
            // names no instruction (`singleton_site` given): no memo.
            let singleton_key = match singleton_site {
                None if num_captures == 0 => {
                    let frame = &thread.frames[frame_idx];
                    Some((lambda_site_method_key(frame), frame.pc))
                }
                _ => None,
            };
            thread.indy_sites.put(
                current_class_id,
                cp_index,
                epochs_at_entry,
                CachedIndySite::Lambda {
                    proxy_class_id,
                    num_captures,
                    singleton_key,
                    singleton: None,
                },
            );
            return allocate_lambda_proxy(
                shared,
                thread,
                frame_idx,
                proxy_class_id,
                num_captures,
                singleton_site.or(singleton_key),
            );
        }
        Some(CachedIndySite::Other(Some(site))) => {
            thread.indy_sites.put(
                current_class_id,
                cp_index,
                epochs_at_entry,
                CachedIndySite::Other(Some(Arc::clone(&site))),
            );
            return execute_cached_call_site(shared, thread, frame_idx, &site);
        }
        Some(CachedIndySite::Other(None)) | None => {}
    }

    // --- Fast path 2: a linkage published for THIS instruction ---
    //
    // Not in `resolution_cache`: that is keyed per constant-pool entry, and a
    // generic linkage -- or the lambda linkage of an entry several
    // instructions share -- is per instruction (see `GENERIC_INDY_SITES`).
    if ANY_GENERIC_INDY_SITE.load(std::sync::atomic::Ordering::Acquire) {
        let key = indy_instruction_key(
            shared,
            &thread.frames[frame_idx],
            current_class_id,
            cp_index,
            singleton_site,
        );
        if let Some((link, shape, gate)) = probe_generic_indy_site_gated(&key) {
            if singleton_site.is_none() {
                let ident = InstructionIdent::of_frame(
                    shared,
                    &thread.frames[frame_idx],
                    current_class_id,
                    cp_index,
                );
                fill_thread_instruction_site_with(
                    &ident,
                    gate,
                    &link,
                    shared.config.indy_generic_thread_row,
                );
            }
            return run_generic_indy_link(shared, thread, frame_idx, link, &shape, singleton_site);
        }
    }

    // --- Slow path: bootstrap the call site ---
    // Taken BEFORE the constant pool is read: a redefinition of the caller
    // that lands before the fill below is not published over the new pool's
    // index (`ResolutionCache::put_call_site_as_of`, interpreter round i1
    // wave 23, lane L5).
    let fill_as_of = crate::classloading::resolution::ResolutionCache::fill_snapshot();
    // Interpreter round i5 wave 101, lane L4 (`CRATONVM_DBG=mh-finder-census`):
    // the link's phases, `indy-link <phase>` rows. `None` (one flag read)
    // otherwise.
    let link_clock = || {
        cratonvm_native_builtins::mh_finder_census::indy_census_on()
            .then(std::time::Instant::now)
    };
    let link_note = |phase: &str, start: Option<std::time::Instant>| {
        if let Some(start) = start {
            cratonvm_native_builtins::mh_finder_census::note_indy_link(phase, start);
        }
    };
    let info_clock = link_clock();
    // Extract all needed data under the class_manager read lock, then drop it.
    // This avoids deadlocking when create_java_string needs a write lock.
    // `unloaded_class_constants`: the concat `Class` constants whose class is
    // not loaded yet, resolved after the lock is dropped (below).
    let (mut info, unloaded_class_constants) = {
        let cm = shared.classes.class_manager.read();
        let class = cm
            .get_class(current_class_id)
            .ok_or_else(|| VmError::Internal {
                message: format!("invokedynamic: class {current_class_id} not found"),
            })?;

        // 1. Resolve the InvokeDynamic CP entry
        let (bsm_index, nat_index) = match class.constant_pool.get(cp_index) {
            Some(ConstantPoolEntry::InvokeDynamic {
                bootstrap_method_attr_index,
                name_and_type_index,
            }) => (*bootstrap_method_attr_index, *name_and_type_index),
            _ => {
                return Err(VmError::Internal {
                    message: format!("invokedynamic cp#{cp_index}: not an InvokeDynamic entry"),
                }
                .into());
            }
        };

        // 2. Get the target name and descriptor from NameAndType
        let (target_name, target_descriptor) = class
            .constant_pool
            .get_name_and_type(nat_index)
            .ok_or_else(|| VmError::Internal {
                message: format!("invokedynamic: invalid name_and_type at cp#{nat_index}"),
            })?;

        // 3. Look up the bootstrap method
        let bsm = class
            .bootstrap_methods
            .get(bsm_index as usize)
            .ok_or_else(|| VmError::Internal {
                message: format!(
                    "invokedynamic: bootstrap method index {bsm_index} out of bounds (have {})",
                    class.bootstrap_methods.len()
                ),
            })?;

        // 4. Resolve the MethodHandle to determine which bootstrap method it is
        let bsm_handle =
            resolve_method_handle_full(&class.constant_pool, bsm.bootstrap_method_ref)?;
        let bsm_class = bsm_handle.class_name.to_string();
        let bsm_method = bsm_handle.member_name.to_string();
        let bsm_descriptor_is_lmf = bsm_class == LAMBDA_METAFACTORY
            && matches!(
                (bsm_method.as_str(), &*bsm_handle.descriptor),
                (METAFACTORY, METAFACTORY_DESCRIPTOR)
                    | (ALT_METAFACTORY, ALT_METAFACTORY_DESCRIPTOR)
            );

        // 5. Extract recipe and constant args -- for `makeConcatWithConstants`
        // only, the one arm that reads them. Every other bootstrap used to pay
        // for rendering each of its static arguments as concat text (a
        // `String` per argument), and a generic site whose `CallSite` is not
        // cached (Groovy's `MutableCallSite`s under the default
        // `CRATONVM_INDY_CALLSITE_CACHE`) paid it on every execution.
        let is_concat_with_constants = &*bsm_handle.class_name == STRING_CONCAT_FACTORY
            && &*bsm_handle.member_name == MAKE_CONCAT_WITH_CONSTANTS;
        let recipe: Vec<u16> = match bsm.bootstrap_arguments.first() {
            Some(&arg_index) if is_concat_with_constants => {
                resolve_concat_constant_units(&class.constant_pool, arg_index).unwrap_or_default()
            }
            _ => Vec::new(),
        };

        // The `\u0002` (TAG_CONST) entries in the recipe consume these trailing
        // bootstrap static arguments in order. Per `java.lang.invoke.
        // StringConcatFactory`, a constant may be *any* loadable constant
        // (String, but also int/long/float/double or a Class), and is folded in
        // via its `String.valueOf` form — NOT only `String`/`Utf8`. The previous
        // `resolve_string_constant` returned `None` (→ empty) for every numeric
        // constant, silently dropping it. `resolve_concat_constant` converts each
        // loadable-constant kind to its HotSpot-identical text (reusing
        // `format_float`/`format_double` so e.g. `1.0f` → "1.0", not "1").
        let mut unloaded_class_constants: Vec<(usize, String)> = Vec::new();
        let constant_args: Vec<Vec<u16>> = if is_concat_with_constants {
            bsm.bootstrap_arguments
                .iter()
                .skip(1) // skip recipe
                .enumerate()
                .map(|(position, &idx)| {
                    // A `Class` constant renders as `String.valueOf(Class)`
                    // does: `interface X` for an interface (interpreter round
                    // i1 wave 35; from the loaded class, `class X` when it is
                    // not loaded yet).
                    if let Some(ConstantPoolEntry::ClassReference { name_index }) =
                        class.constant_pool.get(idx)
                    {
                        if let Some(name) = class.constant_pool.get_utf8(*name_index) {
                            let loaded = cm.find_class_by_name_for_class(name, current_class_id);
                            let is_interface = loaded
                                .and_then(|id| cm.get_class(id))
                                .is_some_and(|c| c.is_interface());
                            if is_interface {
                                let text = format!("interface {}", name.replace('/', "."));
                                return text.encode_utf16().collect();
                            }
                            if loaded.is_none() && !name.starts_with('[') {
                                unloaded_class_constants.push((position, name.to_string()));
                            }
                        }
                    }
                    resolve_concat_constant_units(&class.constant_pool, idx).unwrap_or_default()
                })
                .collect()
        } else {
            Vec::new()
        };

        let bootstrap_arg_indices = bsm.bootstrap_arguments.clone();

        (
            IndyInfo {
                bsm_class,
                bsm_method,
                target_name: target_name.to_string(),
                target_descriptor: target_descriptor.to_string(),
                recipe,
                constant_args,
                bootstrap_arg_indices,
                bsm_descriptor_is_lmf,
                fill_as_of,
            },
            unloaded_class_constants,
        )
    }; // cm read lock dropped here

    // `--real-jdk` (interpreter round i1 wave 38, lane L4; item 1 of
    // `i37-L4-review-of-waves-30-36-invoke-and-indy-small-divergences`): a
    // `Class` concat constant whose class was not loaded yet is resolved as
    // HotSpot resolves every static argument before the bootstrap runs,
    // through the caller's loader: an interface then renders as
    // `interface X` (it rendered `class X`), and a missing class is the
    // resolution's `NoClassDefFoundError` (it rendered `class X` and linked).
    // `--compatible` keeps the text rendering and loads nothing.
    if !unloaded_class_constants.is_empty() && shared.classes.indy_link_checks {
        for (position, name) in &unloaded_class_constants {
            let id = crate::runtime::interpreter::resolve_class_loader_aware(
                shared,
                thread,
                current_class_id,
                name,
            )
            .map_err(|e| {
                crate::runtime::exceptions::convert_class_not_found_for(
                    shared,
                    thread,
                    Some(current_class_id),
                    name,
                    e,
                )
            })?;
            let is_interface = shared
                .classes
                .class_manager
                .read()
                .get_class(id)
                .is_some_and(|c| c.is_interface());
            if is_interface {
                if let Some(slot) = info.constant_args.get_mut(*position) {
                    *slot = format!("interface {}", name.replace('/', "."))
                        .encode_utf16()
                        .collect();
                }
            }
        }
    }

    link_note("info", info_clock);
    // Wave 101 (`CRATONVM_INDY_LINK_FILLS_THREAD_SITE`, default off): read once
    // per link.
    let fills_thread_site = true;

    if dbg_indy_all() {
        let caller_name = {
            let cm = shared.classes.class_manager.read();
            cm.get_class(current_class_id)
                .map(|c| c.name.to_string())
                .unwrap_or_default()
        };
        eprintln!(
            "[indy-all] caller={caller_name} bsm={}.{} target={}{}",
            info.bsm_class, info.bsm_method, info.target_name, info.target_descriptor
        );
    }
    // `--real-jdk` (interpreter round i1 wave 35, item 1 of
    // `i29-L4-native-indy-linkage-skips-the-jdks-validation`): a concat site
    // the native linkage would accept although `StringConcatFactory` refuses
    // it, or whose constants it does not render as the JDK does, links
    // through the real factory instead, which raises the exact
    // `BootstrapMethodError` (or renders the constants) and records it.
    // Wave 48: in every mode when `indy_other_shapes_to_jdk` is on (both
    // modes are held to HotSpot), through the factory's own bytecode.
    let other_shapes_to_jdk = shared.classes.indy_other_shapes_to_jdk;
    // The lambda, switch and record arms' generic route is not taken from
    // the compiled bridge (see the lambda arm).
    let to_jdk = other_shapes_to_jdk && singleton_site.is_none();
    let concat_checks_clock = link_clock();
    if info.bsm_class == STRING_CONCAT_FACTORY
        && (info.bsm_method == MAKE_CONCAT_WITH_CONSTANTS || info.bsm_method == MAKE_CONCAT)
        && (shared.classes.indy_link_checks || other_shapes_to_jdk)
        && concat_needs_the_jdk_factory(shared, current_class_id, &info)
    {
        if other_shapes_to_jdk {
            return bootstrap_generic_jdk(
                shared,
                thread,
                frame_idx,
                cp_index,
                &info,
                current_class_id,
            );
        }
        return bootstrap_generic(shared, thread, frame_idx, cp_index, &info, current_class_id);
    }
    let is_concat = info.bsm_class == STRING_CONCAT_FACTORY
        && (info.bsm_method == MAKE_CONCAT_WITH_CONSTANTS || info.bsm_method == MAKE_CONCAT);
    if is_concat {
        link_note("concat-checks", concat_checks_clock);
    }
    let concat_clock = if is_concat { link_clock() } else { None };
    if info.bsm_class == STRING_CONCAT_FACTORY && info.bsm_method == MAKE_CONCAT_WITH_CONSTANTS {
        // Cache the StringConcat call site
        // INTERP719-1: the Latin-1 shape, decided once for the cached site.
        let latin1_shape = concat_latin1::site_shape_admits_for_descriptor(
            &info.recipe,
            info.constant_args.as_slice(),
            &info.target_descriptor,
        );
        let site = ResolvedCallSite::StringConcat {
            recipe: Arc::from(info.recipe.as_slice()),
            constant_args: info
                .constant_args
                .iter()
                .map(|u| Arc::from(u.as_slice()))
                .collect(),
            target_descriptor: Arc::from(info.target_descriptor.clone()),
            latin1_shape,
        };
        let linked_row = put_concat_site_keeping_row(
            shared,
            (current_class_id, cp_index),
            site,
            info.fill_as_of,
            fills_thread_site,
        );

        let executed = execute_string_concat(
            shared,
            thread,
            frame_idx,
            &info.recipe,
            info.constant_args.as_slice(),
            &info.target_descriptor,
            Some(latin1_shape),
        );
        if executed.is_ok() {
            if let Some(row) = linked_row {
                fill_thread_other_site_after_link(
                    shared,
                    thread,
                    (current_class_id, cp_index),
                    epochs_at_entry,
                    row,
                );
            }
        }
        link_note("concat-link", concat_clock);
        executed
    } else if info.bsm_class == STRING_CONCAT_FACTORY && info.bsm_method == MAKE_CONCAT {
        // makeConcat has no recipe — all arguments are simply concatenated in order.
        // Synthesize a recipe of all \u{0001} placeholders so the existing concat
        // logic works unchanged.
        let arg_types = parse_descriptor_args(&info.target_descriptor);
        let synthetic_recipe: Vec<u16> = vec![TAG_ARG_UNIT; arg_types.len()];

        const NO_CONSTANTS: &[Arc<[u16]>] = &[];
        // INTERP719-1: the Latin-1 shape, decided once for the cached site.
        let latin1_shape = concat_latin1::site_shape_admits_for_descriptor(
            &synthetic_recipe,
            NO_CONSTANTS,
            &info.target_descriptor,
        );
        let site = ResolvedCallSite::StringConcat {
            recipe: Arc::from(synthetic_recipe.as_slice()),
            constant_args: vec![],
            target_descriptor: Arc::from(info.target_descriptor.as_str()),
            latin1_shape,
        };
        let linked_row = put_concat_site_keeping_row(
            shared,
            (current_class_id, cp_index),
            site,
            info.fill_as_of,
            fills_thread_site,
        );

        // A synthesized all-argument recipe is U+0001 repeated, so it holds no
        // TAG_CONST (U+0002) placeholders and the constant list is empty. The
        // whole-`IndyInfo` clone (`patched_info`) this branch used to build was
        // only ever read for the three values passed below.
        let executed = execute_string_concat(
            shared,
            thread,
            frame_idx,
            &synthetic_recipe,
            NO_CONSTANTS,
            &info.target_descriptor,
            Some(latin1_shape),
        );
        if executed.is_ok() {
            if let Some(row) = linked_row {
                fill_thread_other_site_after_link(
                    shared,
                    thread,
                    (current_class_id, cp_index),
                    epochs_at_entry,
                    row,
                );
            }
        }
        link_note("concat-link", concat_clock);
        executed
    } else if info.bsm_class == LAMBDA_METAFACTORY
        && (info.bsm_method == METAFACTORY || info.bsm_method == ALT_METAFACTORY)
    {
        // altMetafactory has additional bootstrap arguments (flags, marker interfaces,
        // bridges) beyond the 3 standard ones. The core lambda proxy creation is
        // identical, but none of the extras is advisory: the FLAG_SERIALIZABLE bit
        // and the MARKER INTERFACE list change the spun proxy's interface list, so
        // `instanceof`/`checkcast`/`Class.isInstance` against a marker must succeed,
        // and a FLAG_BRIDGES descriptor is one more way into the body (lambda
        // dispatch admits only the descriptors the proxy implements).
        // `bootstrap_lambda` reads all three.
        //
        // `--real-jdk` (interpreter round i1 waves 33, 36, 37): the static
        // arguments resolve first, so a missing implementation method is a
        // `NoSuchMethodError` here, unwrapped (JVMS 5.4.3.5); then a site the
        // JDK refuses fails linkage with the refusal HotSpot raises (a
        // `LambdaConversionException`, or `BootstrapMethodInvoker`'s and
        // `altMetafactory`'s own argument errors), wrapped in
        // `BootstrapMethodError`, instead of linking. Raised here with the
        // JDK's own message: `bootstrap_generic` would reach CratonVM's
        // `LambdaMetafactory` shim, which does not validate. A shape the
        // native linkage cannot build at all (fewer than three static
        // arguments, or the wrong kind among them) is refused in every mode:
        // it used to be an uncatchable internal error.
        let jdk_only = shared.classes.indy_link_checks;
        let impl_clock = link_clock();
        // Every mode (wave 41, lane L4; row 4 of `i37-L4-...`): a dynamic
        // constant among the static arguments resolves first, in argument
        // order; its failure is the site's, and its value is what the checks
        // and the linkage read (a valid `MethodType` used to be an
        // uncatchable internal error). Empty for every javac site.
        let condy = match lambda_resolve_dynamic_args(shared, thread, current_class_id, &info) {
            Ok(condy) => condy,
            Err(failed) => {
                return settle_native_link_failure(
                    shared,
                    thread,
                    frame_idx,
                    current_class_id,
                    cp_index,
                    singleton_site,
                    &info.target_descriptor,
                    failed,
                );
            }
        };
        if jdk_only {
            let impl_handle = {
                let cm = shared.classes.class_manager.read();
                cm.get_class(current_class_id).and_then(|class| {
                    let index = *info.bootstrap_arg_indices.get(1)?;
                    resolve_method_handle_full(&class.constant_pool, index).ok()
                })
            };
            if let Some(impl_handle) = impl_handle {
                if let Err(failed) =
                    lambda_impl_resolves(shared, thread, current_class_id, &impl_handle)
                {
                    return settle_native_link_failure(
                        shared,
                        thread,
                        frame_idx,
                        current_class_id,
                        cp_index,
                        singleton_site,
                        &info.target_descriptor,
                        failed,
                    );
                }
            }
        }
        link_note("impl-resolves", impl_clock);
        let refusal_clock = link_clock();
        let mut undecided = false;
        let refusal = match lambda_site_refusal(
            shared,
            thread,
            current_class_id,
            &info,
            jdk_only,
            &condy,
            &mut undecided,
        ) {
            Ok(refusal) => refusal,
            // A class a `MethodType` names could not be loaded (wave 39).
            Err(failed) => {
                return settle_native_link_failure(
                    shared,
                    thread,
                    frame_idx,
                    current_class_id,
                    cp_index,
                    singleton_site,
                    &info.target_descriptor,
                    failed,
                );
            }
        };
        // Wave 48 (`i38-L4-proposal-native-bootstraps-link-only-javac-shapes-
        // natively`, behind `indy_other_shapes_to_jdk`): a site the checks
        // cannot decide (javac emits none: a dynamic constant whose handle the
        // linkage cannot read back, a same-named class of another loader, a
        // JDK handle value, ...) links through the JDK's own
        // `LambdaMetafactory`, which builds the JDK's own proxy or raises the
        // JDK's own error; it linked natively with the pool's reading, or
        // failed with an uncatchable internal error. A site the checks REFUSE
        // keeps their exact text: the generic route's `BootstrapMethodInvoker`
        // does not yet reproduce HotSpot's arity (`WrongMethodTypeException`)
        // and `checkcast` texts, nor a `DirectMethodHandle`'s class name
        // (measured, `L4W37LambdaSiteValidation` / `L4W38LambdaSiteRecords`
        // with every refusal routed; see the proposal's Progress). Not from
        // the compiled bridge (`singleton_site`), whose synthetic frame names
        // no instruction for the generic route's per-instruction record: it
        // admits javac's pool shape only (`make_jit_indy_bridge_site_from_parts`).
        // Interpreter round i6 wave 119, lane L4 (`CRATONVM_INDY_GENERIC_VIA_MAKESITE`):
        // with the generic route going through the JDK's `CallSite.makeSite` /
        // `BootstrapMethodInvoker`, a REFUSED site takes it too: the JDK raises
        // its own arity, cast and conversion errors, which this function's
        // `lambda_site_refusal` copies (kept for the switch off) reproduce.
        let route_refusal = refusal.is_some();
        if to_jdk && (undecided || route_refusal) {
            return bootstrap_generic_jdk(
                shared,
                thread,
                frame_idx,
                cp_index,
                &info,
                current_class_id,
            );
        }
        if let Some(refusal) = refusal {
            // An empty message is a null one (`lambda_null_argument_refusal`).
            let failed = bootstrap_method_error_with_new_cause_opt(
                shared,
                thread,
                "bootstrap method initialization exception",
                refusal.cause_class,
                (!refusal.message.is_empty()).then_some(refusal.message.as_str()),
            );
            return settle_native_link_failure(
                shared,
                thread,
                frame_idx,
                current_class_id,
                cp_index,
                singleton_site,
                &info.target_descriptor,
                failed,
            );
        }
        link_note("site-refusal", refusal_clock);
        let bootstrap_clock = link_clock();
        let linked = bootstrap_lambda(
            shared,
            thread,
            frame_idx,
            cp_index,
            &info,
            singleton_site,
            &condy,
        );
        if linked.is_ok() && fills_thread_site {
            fill_thread_indy_site_after_link(
                shared,
                thread,
                frame_idx,
                (current_class_id, cp_index),
                epochs_at_entry,
                singleton_site,
            );
        }
        link_note("bootstrap-lambda", bootstrap_clock);
        linked
    } else if info.bsm_class == SWITCH_BOOTSTRAPS
        && (info.bsm_method == TYPE_SWITCH || info.bsm_method == ENUM_SWITCH)
    {
        // `SwitchBootstraps`' own argument checks (interpreter round i1 wave
        // 38, lane L4): the invocation type in every mode (the native
        // switches pop two operands and push an `int` whatever the call site
        // says), the selector type and the labels under `--real-jdk`.
        let jdk_only = shared.classes.indy_link_checks;
        // Wave 48, behind `indy_other_shapes_to_jdk`: a site the checks
        // refuse (all of them, in every mode) links through the JDK's own
        // `SwitchBootstraps`, which raises its own error.
        if to_jdk && switch_site_refusal(shared, current_class_id, &info, true).is_some() {
            return bootstrap_generic_jdk(
                shared,
                thread,
                frame_idx,
                cp_index,
                &info,
                current_class_id,
            );
        }
        if let Some(refusal) = switch_site_refusal(shared, current_class_id, &info, jdk_only) {
            let failed = bootstrap_method_error_with_new_cause(
                shared,
                thread,
                "bootstrap method initialization exception",
                refusal.cause_class,
                &refusal.message,
            );
            return settle_native_link_failure(
                shared,
                thread,
                frame_idx,
                current_class_id,
                cp_index,
                singleton_site,
                &info.target_descriptor,
                failed,
            );
        }
        if info.bsm_method == TYPE_SWITCH {
            bootstrap_type_switch(shared, thread, frame_idx, cp_index, &info)
        } else {
            bootstrap_enum_switch(shared, thread, frame_idx, cp_index, &info)
        }
    } else if info.bsm_class == OBJECT_METHODS && info.bsm_method == BOOTSTRAP {
        // `ObjectMethods.bootstrap`'s own checks (interpreter round i1 wave
        // 38, lane L4): the method name and the type's shape in every mode,
        // the record class in the type and the name list under `--real-jdk`.
        let jdk_only = shared.classes.indy_link_checks;
        // Wave 48, behind `indy_other_shapes_to_jdk`: a site the checks refuse
        // (all of them, in every mode) links through the JDK's own
        // `ObjectMethods`, which raises its own error. A site whose getters
        // are not javac's keeps the native getter-driven linkage (and the
        // `toString` getter refusal below): the JDK's route over this
        // model's combinators links shapes HotSpot refuses (measured,
        // `L4W42ObjectMethodsStaticSpecialGetters` with them routed).
        if to_jdk && object_methods_site_refusal(shared, current_class_id, &info, true).is_some() {
            return bootstrap_generic_jdk(
                shared,
                thread,
                frame_idx,
                cp_index,
                &info,
                current_class_id,
            );
        }
        // `--real-jdk` (wave 39): the getters of a site that is not javac's
        // shape resolve first, from the calling class, as HotSpot resolves
        // every static argument before the bootstrap runs: an inaccessible
        // or missing getter is its `IllegalAccessError` / `NoSuchFieldError`.
        if jdk_only {
            if let Err(failed) =
                object_methods_resolve_getters(shared, thread, current_class_id, &info)
            {
                return settle_native_link_failure(
                    shared,
                    thread,
                    frame_idx,
                    current_class_id,
                    cp_index,
                    singleton_site,
                    &info.target_descriptor,
                    failed,
                );
            }
        }
        if let Some(refusal) =
            object_methods_site_refusal(shared, current_class_id, &info, jdk_only)
        {
            let failed = bootstrap_method_error_with_new_cause(
                shared,
                thread,
                "bootstrap method initialization exception",
                refusal.cause_class,
                &refusal.message,
            );
            return settle_native_link_failure(
                shared,
                thread,
                frame_idx,
                current_class_id,
                cp_index,
                singleton_site,
                &info.target_descriptor,
                failed,
            );
        }
        // `--real-jdk` (wave 42): a `toString` getter whose handle type is
        // not `(R)T` is refused as `makeToString` refuses it, rather than
        // handed to the JDK's route, which fails on CratonVM in
        // `MethodHandle.copyWith` (`AbstractMethodError`).
        if jdk_only && info.target_name == "toString" {
            if let Some(refusal) =
                object_methods_to_string_getter_refusal(shared, current_class_id, &info)
            {
                if dbg_indy_all() {
                    eprintln!(
                        "[indy-all] object-methods toString cp#{cp_index}: getter refused: {}",
                        refusal.message
                    );
                }
                let failed = object_methods_to_string_failure(shared, thread, &refusal);
                return settle_native_link_failure(
                    shared,
                    thread,
                    frame_idx,
                    current_class_id,
                    cp_index,
                    singleton_site,
                    &info.target_descriptor,
                    failed,
                );
            }
        }
        bootstrap_record_object_method(shared, thread, frame_idx, cp_index, &info)
    } else if shared.natives.native_methods.has_app_workaround(
        cratonvm_native_api::AppWorkaroundKind::IndyBooleanCastRedirect,
        &info.bsm_class,
        &info.target_name,
        "",
    ) && info.target_descriptor.ends_with(")Z")
        && matches!(parse_descriptor_args(&info.target_descriptor).as_slice(),
                    [c] if *c == 'L' || *c == '[')
    {
        // Groovy "cast"-to-boolean coercion (`if (x.f())`, `!x`, ternaries …).
        // Groovy emits a dedicated `cast:(Object)Z` invokedynamic and routes it
        // through `IndyInterface` → `Selector$CastSelector.handleBoolean`, which
        // builds a `guardWithTest(IS_NULL, FALSE, asBoolean(…))` MethodHandle.
        // CratonVM's MH dispatch of that particular adapter chain does not reach
        // the runtime receiver's `asBoolean` (so a Groovy-falsy-but-non-null value
        // — `Boolean.FALSE`, `""`, `[]`, `0` — read as TRUE), which silently flips
        // `if`/`!` branches (e.g. Spring Boot's `SpringRepositoriesExtension`
        // `if (!"commercial".equalsIgnoreCase(buildType))` and
        // `if (version.endsWith("-SNAPSHOT"))`). Dispatch Groovy's own
        // `DefaultTypeTransformation.castToBoolean(Object)` directly instead — it
        // is real Groovy bytecode implementing "Groovy truth" and dispatches
        // `asBoolean` correctly on CratonVM (verified == HotSpot). This bypasses
        // the broken CastSelector adapter for the boolean case only; all other
        // cast targets keep the generic bootstrap path.
        //
        // The decision is a pure function of the constant pool, so it is
        // published per instruction like a generic linkage: the steady state
        // then skips rebuilding this `IndyInfo` (four `String`s and a recipe
        // walk) on every Groovy truth test.
        if generic_indy_cache_mode() != GenericIndyCacheMode::Off {
            let key = generic_indy_site_key(
                shared,
                &thread.frames[frame_idx],
                current_class_id,
                cp_index,
            );
            publish_generic_indy_site(
                key,
                GenericIndySlot {
                    link: GenericIndyLink::GroovyCastToBoolean,
                    shape: Arc::new(GenericIndyShape::new(&info.target_descriptor)),
                    gate: generic_indy_gate(shared, current_class_id),
                },
            );
        }
        groovy_cast_to_boolean(shared, thread, frame_idx)
    } else {
        // Generic invokedynamic: a bootstrap method outside the hardcoded JDK
        // factory set above (e.g. Groovy's
        // `org.codehaus.groovy.vmplugin.v8.IndyInterface.bootstrap`). Per JVMS
        // §5.4.3.6 the linkage runs the bootstrap method itself to obtain a
        // `CallSite`, then invokes that call site's target `MethodHandle` with
        // the dynamic arguments. `bootstrap_generic` does exactly that; if the
        // bootstrap genuinely cannot be executed it surfaces a loud error
        // (BootstrapMethodError / InternalError), never a silent wrong value.
        bootstrap_generic(shared, thread, frame_idx, cp_index, &info, current_class_id)
    }
}

/// A bootstrap static argument resolved out of the constant pool, captured in a
/// lock-free form so it can be materialised into a `Value` *after* the
/// `class_manager` read lock is dropped (materialisation may allocate / load
/// classes / run `<clinit>`).
pub(in crate::runtime::invokedynamic) enum StaticArg {
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    /// gci i33/sc: carries its own `CONSTANT_String` index, so the class's
    /// constant record owns the String as `ldc` of that entry records it
    /// (`constants::condy_string_arg`).
    Str {
        cp_index: u16,
        text: String,
    },
    /// gci i33/sc: a `CONSTANT_String` carrying lone surrogates, kept as its
    /// exact UTF-16 units (the `Arc<str>` form turns each into U+FFFD; HotSpot
    /// passes the exact String). `constants::condy_wide_string_arg`.
    WideStr {
        cp_index: u16,
        units: Vec<u16>,
    },
    Class(String),
    /// `CONSTANT_MethodType`, resolved through the same per-entry record `ldc`
    /// of that index uses, so the two agree on identity.
    MType {
        cp_index: u16,
        descriptor: String,
    },
    /// `CONSTANT_MethodHandle`, resolved and recorded like `ldc` of that index.
    MHandle {
        cp_index: u16,
        handle: MethodHandle,
    },
    /// A `CONSTANT_Dynamic`, resolved (running its own bootstrap) and recorded
    /// per entry by `constants::resolve_condy_constant`.
    Dynamic(u16),
}

/// Resolve a single bootstrap static-argument CP entry to a [`StaticArg`].
///
/// JVMS §4.7.23: a static argument is any loadable constant (§4.4), so every
/// loadable tag has an arm. Anything else is a malformed `BootstrapMethods`
/// attribute, reported as the `ClassFormatError` the condy decoder
/// (`constants::decode_condy`) raises for the same defect — not the
/// uncatchable VM-internal error it used to be.
pub(in crate::runtime::invokedynamic) fn resolve_static_arg_kind(
    cp: &ConstantPool,
    class_name: &str,
    index: u16,
) -> Result<StaticArg, MethodCallFailed> {
    let malformed = || -> MethodCallFailed {
        VmError::Linkage(crate::error::LinkageError::ClassFormatError {
            class_name: class_name.to_string(),
            message: format!("invokedynamic: unusable bootstrap argument cp#{index}"),
        })
        .into()
    };
    let entry = cp.get(index).ok_or_else(malformed)?;
    Ok(match entry {
        ConstantPoolEntry::Integer(i) => StaticArg::Int(*i),
        ConstantPoolEntry::Long(l) => StaticArg::Long(l.get()),
        ConstantPoolEntry::Float(f) => StaticArg::Float(*f),
        ConstantPoolEntry::Double(d) => StaticArg::Double(d.get()),
        ConstantPoolEntry::StringReference { string_index } => {
            // gci i33/sc: the exact units first (`constants::decode_condy`'s
            // order); the lossy `Arc<str>` text only for the lossless case.
            match cp.get_utf8_wide(*string_index) {
                Some(units) => StaticArg::WideStr {
                    cp_index: index,
                    units: units.to_vec(),
                },
                None => StaticArg::Str {
                    cp_index: index,
                    text: resolve_string_constant(cp, index).unwrap_or_default(),
                },
            }
        }
        ConstantPoolEntry::ClassReference { .. } => {
            let name = cp.get_class_name(index).ok_or_else(malformed)?;
            StaticArg::Class(name.to_string())
        }
        ConstantPoolEntry::MethodType { .. } => StaticArg::MType {
            cp_index: index,
            descriptor: resolve_method_type(cp, index).ok_or_else(malformed)?,
        },
        ConstantPoolEntry::MethodHandle { .. } => StaticArg::MHandle {
            cp_index: index,
            handle: resolve_method_handle_full(cp, index)?,
        },
        ConstantPoolEntry::Dynamic { .. } => StaticArg::Dynamic(index),
        _ => return Err(malformed()),
    })
}

/// A `CONSTANT_MethodType` / `CONSTANT_MethodHandle` static argument, resolved
/// on the same terms as `ldc` of that entry (`constants::execute_ldc`): the
/// recorded value if there is one, the recorded `LinkageError` rethrown if the
/// entry already failed, else `resolve` — its `LinkageError` recorded against
/// the entry, its value recorded in the store `ldc` uses for the tag.
///
/// `permanent` is `true` for a method handle: re-resolving one does not yield
/// the same object, so, as `ldc` does, it goes to the never-evicted,
/// insert-if-absent record and the FIRST recorded handle is the answer.
/// A method type goes there too unless `CRATONVM_RESOLUTION_LINKED_PERMANENT=0`
/// (then the capped record, as before); its intern set is weak, so only the
/// record holds it for the class's lifetime (nio21 wave 11 L4).
///
/// `fill_as_of` is the `IndyInfo::fill_as_of` snapshot, taken before the
/// constant pool was read for this bootstrap: every record here is made under
/// it, so a redefinition of the class in between records nothing
/// (i22-L5, interpreter round i1 wave 24).
pub(in crate::runtime::invokedynamic) fn resolve_recorded_static_arg(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    cp_index: u16,
    permanent: bool,
    fill_as_of: u64,
    resolve: impl FnOnce(&mut JvmThread) -> Result<ObjectRef, MethodCallFailed>,
) -> Result<Value, MethodCallFailed> {
    use crate::runtime::interpreter::constants as k;
    if let Some(v) = k::probe_recorded_cp_constant(shared, class_id, cp_index) {
        return Ok(v);
    }
    if let Some(recorded) = k::recorded_resolution_failure(shared, thread, class_id, cp_index) {
        return Err(recorded);
    }
    let obj = resolve(thread).map_err(|e| {
        k::record_resolution_failure_as_of(shared, class_id, cp_index, e, fill_as_of)
    })?;
    // JVMS §5.4.3: a failure another thread recorded for the entry meanwhile
    // is its outcome (interpreter round i1 wave 38, lane L5; `--real-jdk`).
    // Allocates only on that path, where `obj` is dropped.
    if let Some(recorded) =
        k::recorded_resolution_failure_after_success(shared, thread, class_id, cp_index)
    {
        return Err(recorded);
    }
    let v = Value::Object(Some(obj));
    if permanent {
        return Ok(k::record_cp_constant_permanent_as_of(
            shared, class_id, cp_index, v, fill_as_of,
        ));
    }
    k::store_recorded_cp_constant_as_of(shared, class_id, cp_index, v, fill_as_of);
    Ok(v)
}

/// `Class.cast`'s message (`Cannot cast java.lang.Integer to java.lang.String`)
/// when the bootstrap static argument `value` cannot be passed to a parameter
/// (or collected into an array component) of type `param`, `None` when it can
/// or when it is not judged. Judged only where the answer is certain: a
/// parameter of a `java/` class other than `Object` (only the boot loader
/// defines one), and a value of a class a loadable constant produces
/// (`String`, the four boxes, `Class`, `MethodType`); a raw primitive is
/// judged as its box, which the collection or the fixed boxing makes.
/// Interpreter round i1 wave 41, lane L4.
pub(in crate::runtime::invokedynamic) fn bootstrap_static_arg_cast_failure(
    shared: &SharedVm,
    value: Value,
    param: &str,
) -> Option<String> {
    let want = param.strip_prefix('L')?.strip_suffix(';')?;
    if want == "java/lang/Object" || !want.starts_with("java/") {
        return None;
    }
    let cm = shared.classes.class_manager.read();
    let (cid, actual): (ClassId, &str) = match value {
        Value::Object(Some(o)) => {
            if shared.mem.heap.kind_of(o) != cratonvm_types::ObjectKind::Object {
                return None;
            }
            let cid = shared.mem.heap.class_id_of(o);
            let name = cm.get_class(cid)?.name.clone();
            let actual = match &*name {
                "java/lang/String" => "java/lang/String",
                "java/lang/Integer" => "java/lang/Integer",
                "java/lang/Long" => "java/lang/Long",
                "java/lang/Float" => "java/lang/Float",
                "java/lang/Double" => "java/lang/Double",
                "java/lang/Class" => "java/lang/Class",
                "java/lang/invoke/MethodType" => "java/lang/invoke/MethodType",
                _ => return None,
            };
            (cid, actual)
        }
        Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_) => {
            let boxed = match value {
                Value::Int(_) => "java/lang/Integer",
                Value::Long(_) => "java/lang/Long",
                Value::Float(_) => "java/lang/Float",
                _ => "java/lang/Double",            };
            (cm.get_loaded_class_id(boxed)?, boxed)        }
        _ => return None,
    };
    if cm.is_assignable_to_name(cid, want) {
        return None;
    }
    Some(format!(
        "Cannot cast {} to {}",
        crate::runtime::exceptions::hotspot_external_name(&actual),
        crate::runtime::exceptions::hotspot_external_name(&want)
    ))
}

/// What a bootstrap argument needs to match one fixed declared parameter.
#[derive(Debug)]
pub(in crate::runtime::invokedynamic) enum BootstrapArgFit {
    Keep,
    /// A primitive bound to a reference parameter: box it (`valueOf`).
    Box,
    /// A primitive bound to a wider primitive parameter (JLS §5.1.2).
    Widen(Value),
}

/// The `asType` conversion for a bootstrap argument `v` bound to a parameter
/// whose [`parse_descriptor_args`] tag is `tag` (`L` for any reference).
/// Mirrors `constants::invoke_condy_bootstrap_pinned`'s step 5, plus
/// `long -> float`, which that list omits.
pub(in crate::runtime::invokedynamic) fn bootstrap_arg_fit(tag: char, v: Value) -> BootstrapArgFit {
    match (tag, v) {
        ('L', Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_)) => {
            BootstrapArgFit::Box
        }
        ('J', Value::Int(x)) => BootstrapArgFit::Widen(Value::Long(i64::from(x))),
        // Cast: JLS §5.1.2 widening int -> float (may round, as Java does)
        ('F', Value::Int(x)) => BootstrapArgFit::Widen(Value::Float(x as f32)),
        // Cast: JLS §5.1.2 widening long -> float (may round, as Java does)
        ('F', Value::Long(x)) => BootstrapArgFit::Widen(Value::Float(x as f32)),
        ('D', Value::Int(x)) => BootstrapArgFit::Widen(Value::Double(f64::from(x))),
        // Cast: JLS §5.1.2 widening long -> double (may round, as Java does)
        ('D', Value::Long(x)) => BootstrapArgFit::Widen(Value::Double(x as f64)),
        ('D', Value::Float(x)) => BootstrapArgFit::Widen(Value::Double(f64::from(x))),
        _ => BootstrapArgFit::Keep,
    }
}

/// Box a primitive bootstrap argument the way `MethodHandle.invoke` does for a
/// reference parameter or an `Object...` element; a reference passes through.
pub(in crate::runtime::invokedynamic) fn box_bootstrap_primitive(
    shared: &SharedVm,
    thread: &mut JvmThread,
    v: Value,
) -> Result<Value, MethodCallFailed> {
    let (class, desc) = match v {
        Value::Int(_) => ("java/lang/Integer", "(I)Ljava/lang/Integer;"),
        Value::Long(_) => ("java/lang/Long", "(J)Ljava/lang/Long;"),
        Value::Float(_) => ("java/lang/Float", "(F)Ljava/lang/Float;"),
        Value::Double(_) => ("java/lang/Double", "(D)Ljava/lang/Double;"),
        other => return Ok(other),
    };
    Ok(
        crate::vm::invoke_shared(shared, thread, class, "valueOf", desc, &[v])?
            .unwrap_or(Value::Object(None)),
    )
}

/// Groovy `cast:(Object)Z` coercion → `DefaultTypeTransformation.castToBoolean`.
///
/// Pops the single operand and dispatches Groovy's real "Groovy truth" helper,
/// pushing the resulting `boolean`. See the call site in `execute_invokedynamic`
/// for why the generic CastSelector path is bypassed for the boolean case.
pub(in crate::runtime::invokedynamic) fn groovy_cast_to_boolean(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
) -> Result<(), MethodCallFailed> {
    // Single reference operand (guaranteed by the dispatch guard).
    let cv = thread.frames[frame_idx].stack.pop_compact();
    let arg = cv.decode_by_descriptor(b'L');
    // Pin the operand across the helper call (castToBoolean runs Java bytecode
    // that may allocate / GC).
    let pin_base = thread.native_pin_roots.len();
    if let Value::Object(Some(o)) = arg {
        thread.native_pin_roots.push(o);
    }
    let Some(target_class) = shared.natives.native_methods.find_app_workaround_aux(
        cratonvm_native_api::AppWorkaroundKind::IndyBooleanCastRedirect,
        // Single registered redirect target
        shared
            .natives
            .native_methods
            .first_app_workaround_class(
                cratonvm_native_api::AppWorkaroundKind::IndyBooleanCastRedirect,
            )
            .unwrap_or(""),
    ) else {
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::NotImplemented {
                feature: "IndyBooleanCastRedirect missing target".to_string(),
            },
        )));
    };
    let result = crate::vm::invoke_shared(
        shared,
        thread,
        target_class,
        "castToBoolean",
        "(Ljava/lang/Object;)Z",
        &[arg],
    );
    thread.native_pin_roots.truncate(pin_base);
    let b = match result? {
        Some(Value::Int(v)) => v,
        Some(Value::Object(Some(o))) => {
            // Defensive: a boxed Boolean — unbox via field 0.
            match shared.mem.heap.get_field(o, 0) {
                Value::Int(v) => v,
                _ => 1,
            }
        }
        _ => 0,
    };
    thread.frames[frame_idx].stack.push(Value::Int(b))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Generic invokedynamic call-site cache (JVMS §5.4.3.6, per instruction)
//
// A bootstrap outside the hard-coded JDK factory set (Groovy's `IndyInterface`,
// JRuby, Nashorn/Dynalink, byte-buddy advice, hand-written bootstraps) used to
// run on EVERY execution of its instruction: `bootstrap_generic` never
// published anything. JVMS links each `invokedynamic` instruction once, so a
// bootstrap with side effects observed N calls instead of 1, a failed linkage
// was retried instead of rethrown, and a `MutableCallSite` retargeted with
// `setTarget` was thrown away with the next fresh bootstrap.
//
// This table caches the linked `CallSite` OBJECT per instruction — never its
// target, which `setTarget` may change and which is therefore read from the
// call site on every execution — plus a recorded `LinkageError` for a failed
// linkage. Keyed like `LAMBDA_SINGLETON_CACHE` (method key + pc name the
// instruction; two instructions sharing one `CONSTANT_InvokeDynamic` entry link
// separately, as HotSpot's per-bytecode `ResolvedIndyEntry` does).
//
// Liveness: a cached `CallSite` is a GC root through `gc_scan_generic_indy_roots`
// (the `"indy-call-sites"` row of `memory::native_roots`), conditional on the
// caller's defining loader exactly as the resolution cache's condy values are,
// so a cached site does not pin a user loader. Rows of unloaded classes are
// dropped by `forget_unloaded_generic_indy_sites` (`memory::gc`); a redefined
// caller's rows go stale through their `RedefineGate`.
// ---------------------------------------------------------------------------

/// `CRATONVM_INDY_CALLSITE_CACHE`: what [`bootstrap_generic`] publishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::runtime::invokedynamic) enum GenericIndyCacheMode {
    /// `=0`: nothing is cached. The bootstrap runs on every execution, as it
    /// did before this cache existed.
    Off,
    /// `=constant` (the default until interpreter round i1 wave 47): cache
    /// every `CallSite` and record every `LinkageError` linkage
    /// failure — EXCEPT a `MutableCallSite` / `VolatileCallSite` produced by
    /// Groovy's `IndyInterface` bootstrap, which is still re-bootstrapped per
    /// execution. Caching one makes every call run whatever target the language
    /// runtime later installs with `setTarget` — for Groovy the relinked
    /// `guardWithTest`/`SwitchPoint` chain, which the per-execution bootstrap
    /// has so far kept CratonVM from ever executing, and whose adapter chains
    /// this file already documents one wrong dispatch of
    /// (`groovy_cast_to_boolean`). See
    /// `docs/feature-designs/real-jdk-mode.md`
    /// (wave 7) for the suite measurement that has to precede making `All` the
    /// default (history:
    /// `interpreter-L6-generic-indy-rebootstraps-every-execution-RETIRED-20260924.md`).
    /// Until wave 6 every
    /// non-constant call site was excluded, whatever its bootstrap.
    Constant,
    /// Default since wave 47 (unset, `=all`, `=1`): cache every `CallSite`,
    /// mutable ones included. The JVMS shape. Decided by the Spring Framework
    /// Groovy classes (18 classes, 140 tests): all pass with
    /// and without the JIT, and the run is 10-22% shorter than `=constant`.
    All,
}

impl GenericIndyCacheMode {
    /// Whether a linkage by `bsm_class` that produced a NON-constant call site
    /// is published (a `ConstantCallSite` is published in every mode but
    /// `Off`).
    pub(in crate::runtime::invokedynamic) fn caches_mutable_call_site_of(self, bsm_class: &str) -> bool {
        match self {
            GenericIndyCacheMode::Off => false,
            GenericIndyCacheMode::Constant => !is_groovy_indy_bootstrap(bsm_class),
            GenericIndyCacheMode::All => true,
        }
    }
}

/// Groovy's invokedynamic bootstrap class: `vmplugin/v8` (Groovy 3+) and the
/// `vmplugin/v7` spelling Groovy 2.x shipped.
pub(in crate::runtime::invokedynamic) fn is_groovy_indy_bootstrap(bsm_class: &str) -> bool {
    bsm_class == GROOVY_INDY_INTERFACE
        || bsm_class == "org/codehaus/groovy/vmplugin/v7/IndyInterface"
}

pub(in crate::runtime::invokedynamic) fn generic_indy_cache_mode_from(value: Option<&str>) -> GenericIndyCacheMode {
    match value.map(str::trim) {
        Some(v) if v == "0" || v.eq_ignore_ascii_case("off") || v.eq_ignore_ascii_case("false") => {
            GenericIndyCacheMode::Off
        }
        Some(v) if v.eq_ignore_ascii_case("constant") => GenericIndyCacheMode::Constant,
        _ => GenericIndyCacheMode::All,
    }
}

/// Read once per process, like every other gate in this file.
pub(in crate::runtime::invokedynamic) fn generic_indy_cache_mode() -> GenericIndyCacheMode {
    static MODE: std::sync::OnceLock<GenericIndyCacheMode> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| {
        generic_indy_cache_mode_from(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).ok()
                .as_deref(),
        )
    })
}

/// Linkage of one generic `invokedynamic` instruction.
#[derive(Clone)]
pub(in crate::runtime::invokedynamic) enum GenericIndyLink {
    /// The published `CallSite`. Its target is read on every execution.
    Linked(ObjectRef),
    /// A linkage that failed with a `LinkageError`; every later execution
    /// throws a new error of the same class and message (JVMS §5.4.3).
    Failed(Arc<RecordedLinkageError>),
    /// Groovy's `cast:(Object)Z`, served by [`groovy_cast_to_boolean`]
    /// without a bootstrap. Cached only so the steady state skips re-deriving
    /// that decision from the constant pool on every Groovy truth test.
    GroovyCastToBoolean,
    /// A `LambdaMetafactory` linkage of ONE instruction whose
    /// `CONSTANT_InvokeDynamic` entry other instructions of the class share
    /// (javac folds identical method references onto one entry). JVMS §5.4.3.6
    /// links each instruction separately and HotSpot spins one class per
    /// linkage, so each such instruction gets its own proxy class here, and
    /// the entry is kept out of the per-entry `resolution_cache` row that every
    /// unshared lambda uses. See [`lambda_entry_is_shared`].
    Lambda {
        proxy_class_id: ClassId,
        num_captures: usize,
    },
}

impl GenericIndyLink {
    pub(in crate::runtime::invokedynamic) fn mentions_class(&self, dead: &rustc_hash::FxHashSet<ClassId>) -> bool {
        match self {
            GenericIndyLink::Failed(rec) => rec.error_class.is_some_and(|c| dead.contains(&c)),
            // A proxy id is synthetic and never unloaded; the row goes with its
            // caller class, which is in the key.
            GenericIndyLink::Linked(_)
            | GenericIndyLink::GroovyCastToBoolean
            | GenericIndyLink::Lambda { .. } => false,
        }
    }
}

/// What HotSpot's resolution-error table keeps for a failed indy linkage
/// (`ConstantPoolCache::save_and_throw_indy_exc`): the error's class name and
/// detail message only — not the throwable, and not its cause. That path calls
/// the four-argument `SystemDictionary::add_resolution_error`, so the row's
/// cause is null (only `ConstantPool::save_and_throw_exception`, the condy and
/// class/member path, records a cause). A later execution throws a NEW,
/// cause-less error of that class and message
/// (`ConstantPool::throw_resolution_error`), so `==` between the first and the
/// second error is `false` and the second's `getCause()` is `null` on
/// HotSpot 25.
#[derive(Debug)]
pub(in crate::runtime::invokedynamic) struct RecordedLinkageError {
    /// The class to re-create, or `None` to load `error_class_name` through
    /// the boot chain at the rethrow: the `NoClassDefFoundError` a
    /// non-bootstrap error class is recorded as, which need not be loaded yet
    /// when the record is made (t1 wave 12, lane STRREC2; the record runs
    /// allocation-free, so it cannot load it).
    pub(in crate::runtime::invokedynamic) error_class: Option<ClassId>,
    pub(in crate::runtime::invokedynamic) error_class_name: String,
    pub(in crate::runtime::invokedynamic) message: Option<String>,
}

/// The per-site facts every execution needs, computed once at linkage.
pub(in crate::runtime::invokedynamic) struct GenericIndyShape {
    pub(in crate::runtime::invokedynamic) target_descriptor: Box<str>,
    pub(in crate::runtime::invokedynamic) arg_types: Box<[char]>,
    /// First byte of the return descriptor (`b'V'` for none).
    pub(in crate::runtime::invokedynamic) ret_byte: u8,
    /// `java/lang/invoke/MethodHandle`'s class id in the VM whose row holds
    /// this shape (a shape belongs to one row, and a row to one VM), resolved
    /// on the first execution (wave 16, stage 2a of
    /// `i2-L6-proposal-generic-indy-invoke-path`). A bootstrap class, so the
    /// id never changes for the VM's life.
    pub(in crate::runtime::invokedynamic) method_handle_class: std::sync::OnceLock<ClassId>,
    /// The last target-handle class id [`GenericIndyShape::direct_invoke_admits`]
    /// admitted (`0`: none yet), so a monomorphic site asks the class manager
    /// once (interpreter round i6 wave 117, lane L4;
    /// `VmConfig::indy_generic_direct_invoke`). A shape belongs to one VM.
    pub(in crate::runtime::invokedynamic) direct_invoke_receiver: std::sync::atomic::AtomicU32,
    /// The `MethodHandle.invoke([Object])Object` bridge's registry id and the
    /// registry generation it was resolved under, packed
    /// `(generation << 32) | (id + 1)` (`0`: none yet), for
    /// [`GenericIndyShape::direct_invoke_callback`] (interpreter round i6 wave
    /// 119, lane L4; stage 2 proper of `i2-L6-proposal-generic-indy-invoke-path`).
    pub(in crate::runtime::invokedynamic) direct_invoke_native: std::sync::atomic::AtomicU64,
}

impl GenericIndyShape {
    /// Interpreter round i6 wave 117, lane L4 (`CRATONVM_INDY_GENERIC_DIRECT_INVOKE`):
    /// may the target `target_mh` take the direct `MethodHandle.invoke` native
    /// call? Exactly the receivers for which `invoke_on_class_shared_inner`'s
    /// signature-polymorphic block runs `MethodHandle`'s own generic bridge: a
    /// non-array object of a class the block treats as a method-handle receiver
    /// (`is_method_handle_signature_polymorphic_receiver`), and not a Panama
    /// `DowncallHandle` (the block's exact-class / downcall arm). The
    /// admitted class id is remembered.
    /// The registry's `MethodHandle.invoke([Ljava/lang/Object;)Ljava/lang/Object;`
    /// bridge for the direct call, resolved to its slot id once per registry
    /// generation and redeemed from the id on every call (so an in-place
    /// re-registration is followed and a new triple re-resolves), as the
    /// lambda door's `lambda_receiver_native_id`. Interpreter round i6 wave
    /// 119, lane L4: the per-call `find` (a triple hash) before.
    pub(in crate::runtime::invokedynamic) fn direct_invoke_callback(
        &self,
        shared: &SharedVm,
    ) -> Option<cratonvm_native_api::NativeCallback> {
        use std::sync::atomic::Ordering;
        let registry = &shared.natives.native_methods;
        let generation = registry.generation();
        let word = self.direct_invoke_native.load(Ordering::Relaxed);
        if word != 0 && (word >> 32) as u32 == generation {
            let id = cratonvm_native_api::NativeMethodId::from_u32((word as u32).wrapping_sub(1));
            return registry.callback_of(id);
        }
        let id = registry.resolve_id(
            "java/lang/invoke/MethodHandle",
            "invoke",
            "([Ljava/lang/Object;)Ljava/lang/Object;",
        )?;
        let packed = (u64::from(generation) << 32) | u64::from(id.as_u32().wrapping_add(1));
        self.direct_invoke_native.store(packed, Ordering::Relaxed);
        registry.callback_of(id)
    }

    pub(in crate::runtime::invokedynamic) fn direct_invoke_admits(&self, shared: &SharedVm, target_mh: ObjectRef) -> bool {
        use std::sync::atomic::Ordering;
        if shared.mem.heap.kind_of(target_mh) == cratonvm_types::ObjectKind::Array {
            return false;
        }
        let cid = shared.mem.heap.class_id_of(target_mh);
        if cid == ClassId::new(0) {
            return false;
        }
        if self.direct_invoke_receiver.load(Ordering::Relaxed) == cid.as_u32() {
            return true;
        }
        let admitted = shared
            .classes
            .class_manager
            .read()
            .get_class(cid)
            .is_some_and(|class| {
                let name: &str = class.name.as_ref();
                name != "java/lang/foreign/DowncallHandle"
                    && crate::vm::vm_exec::is_method_handle_signature_polymorphic_receiver(name)
            });
        if admitted {
            self.direct_invoke_receiver
                .store(cid.as_u32(), Ordering::Relaxed);
        }
        admitted
    }

    pub(in crate::runtime::invokedynamic) fn new(target_descriptor: &str) -> Self {
        let ret_byte = target_descriptor
            .rfind(')')
            .and_then(|i| target_descriptor.as_bytes().get(i + 1).copied())
            .unwrap_or(b'V');
        Self {
            target_descriptor: Box::from(target_descriptor),
            arg_types: parse_descriptor_args(target_descriptor).into_boxed_slice(),
            ret_byte,
            method_handle_class: std::sync::OnceLock::new(),
            direct_invoke_receiver: std::sync::atomic::AtomicU32::new(0),
            direct_invoke_native: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// `MethodHandle`'s class id, loaded on the first call (a safepoint the
    /// caller must have its references pinned across), memoised after.
    pub(in crate::runtime::invokedynamic) fn method_handle_class(&self, shared: &SharedVm) -> Result<ClassId, VmError> {
        if let Some(&id) = self.method_handle_class.get() {
            return Ok(id);
        }
        let id = shared.load_class_concurrent("java/lang/invoke/MethodHandle")?;
        Ok(*self.method_handle_class.get_or_init(|| id))
    }
}

pub(in crate::runtime::invokedynamic) struct GenericIndySlot {
    pub(in crate::runtime::invokedynamic) link: GenericIndyLink,
    pub(in crate::runtime::invokedynamic) shape: Arc<GenericIndyShape>,
    /// The caller class's redefinition generation at linkage. A redefined
    /// class gets a new constant pool, whose instructions link afresh.
    pub(in crate::runtime::invokedynamic) gate: crate::classloading::resolution::RedefineGate,
}

/// `(vm_identity, caller class, cp index, method key, pc after the instruction)`.
pub(in crate::runtime::invokedynamic) type GenericIndyKey = (usize, ClassId, u16, u64, usize);
pub(in crate::runtime::invokedynamic) type GenericIndyMap = rustc_hash::FxHashMap<GenericIndyKey, GenericIndySlot>;

/// Past this many rows a newly linked site was simply not cached: it behaved
/// as it did before the cache existed (its bootstrap re-ran on every
/// execution, a new `CallSite` each time) rather than growing the table.
/// Since nio21 wave 11 L4 that is only the `CRATONVM_RESOLUTION_LINKED_PERMANENT=0`
/// arm ([`generic_indy_cap_refuses_links`]): JVMS §5.4.3.6 links an
/// instruction once, and the table is bounded by the loaded code anyway --
/// rows go with their class (`forget_unloaded_generic_indy_sites`) and their
/// VM, as `publish_lambda_instruction_site`'s rows always have.
pub(in crate::runtime::invokedynamic) const GENERIC_INDY_SITE_CAP: usize = 1 << 16;

/// Whether [`GENERIC_INDY_SITE_CAP`] still refuses to publish a new linkage:
/// only under `CRATONVM_RESOLUTION_LINKED_PERMANENT=0` (nio21 wave 11 L4).
/// Read only when the table is at the cap, so not on any path a program below
/// 65 536 generic sites ever takes.
pub(in crate::runtime::invokedynamic) fn generic_indy_cap_refuses_links() -> bool {
    !true}

pub(in crate::runtime::invokedynamic) static GENERIC_INDY_SITES: std::sync::OnceLock<parking_lot::RwLock<GenericIndyMap>> =
    std::sync::OnceLock::new();

/// `true` once any row was ever published. Read before the lock, so a program
/// with no generic `invokedynamic` pays one relaxed load per first-time
/// bootstrap of a JDK-factory site and nothing on the GC scan.
pub(in crate::runtime::invokedynamic) static ANY_GENERIC_INDY_SITE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub(in crate::runtime::invokedynamic) fn generic_indy_sites() -> &'static parking_lot::RwLock<GenericIndyMap> {
    GENERIC_INDY_SITES.get_or_init(|| parking_lot::RwLock::new(GenericIndyMap::default()))
}

pub(in crate::runtime::invokedynamic) fn generic_indy_site_key(
    shared: &SharedVm,
    frame: &Frame,
    class_id: ClassId,
    cp_index: u16,
) -> GenericIndyKey {
    (
        shared.vm_identity,
        class_id,
        cp_index,
        lambda_site_method_key(frame),
        frame.pc,
    )
}

/// The live row for `key`, if any. A stale row (its caller was redefined) is a
/// miss; the relink that follows replaces it.
pub(in crate::runtime::invokedynamic) fn probe_generic_indy_site(
    key: &GenericIndyKey,
) -> Option<(GenericIndyLink, Arc<GenericIndyShape>)> {
    let table = generic_indy_sites().read();
    let slot = table.get(key)?;
    if slot.gate.is_stale() {
        return None;
    }
    Some((slot.link.clone(), Arc::clone(&slot.shape)))
}

/// [`probe_generic_indy_site`] plus the row's redefinition gate, for the
/// per-thread copy ([`ThreadInstructionSites`]).
pub(in crate::runtime::invokedynamic) fn probe_generic_indy_site_gated(
    key: &GenericIndyKey,
) -> Option<(
    GenericIndyLink,
    Arc<GenericIndyShape>,
    crate::classloading::resolution::RedefineGate,
)> {
    let table = generic_indy_sites().read();
    let slot = table.get(key)?;
    if slot.gate.is_stale() {
        return None;
    }
    Some((
        slot.link.clone(),
        Arc::clone(&slot.shape),
        slot.gate.clone(),
    ))
}

// ---------------------------------------------------------------------------
// Per-thread copy of heap-free per-instruction linkages
//
// A shared lambda entry (`GenericIndyLink::Lambda`) and Groovy's
// `cast:(Object)Z` (`GroovyCastToBoolean`) are answered from
// `GENERIC_INDY_SITES` on every execution, which took the table's VM-wide
// `RwLock` (two atomic RMWs on a cache line every thread shares) and Fx-hashed
// the executing method's name and descriptor to build the key — after first
// missing the per-entry `resolution_cache` under ITS VM-wide lock. This small
// direct-mapped table per OS thread sits in front of both (it is filled at
// the `GENERIC_INDY_SITES` hit) for exactly those two
// shapes, keyed by the WHOLE instruction: VM, class, CP index, pc and the
// method (its interned name / descriptor `Arc`s, compared by pointer first
// and by content only on a pointer miss — never hashed).
//
// Validity, by the table's own gates: the row's `RedefineGate` is copied and
// checked on every hit (a redefined caller relinks); a class id and a
// `vm_identity` are never reused, so an unloaded caller's or a disposed VM's
// entry can never match again; a row is never replaced in the table while
// its gate is fresh (first writer wins), so a copy of a fresh row stays the
// instruction's linkage. Only shapes that carry no heap reference are copied —
// a `Linked` `CallSite` would need a GC root here — and a copy is made only
// from a table HIT, so threads converge on the published row. The compiled
// bridge's synthetic frame (`singleton_site` given) does not use it.
// ---------------------------------------------------------------------------

/// Number of direct-mapped slots per thread. Per-instruction rows are rare
/// (shared lambda entries, Groovy casts); a small table covers a hot loop.
pub(in crate::runtime::invokedynamic) const THREAD_INSTRUCTION_SLOTS: usize = 64;

/// The heap-reference-free linkages a thread may copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::runtime::invokedynamic) enum ThreadInstructionLink {
    Lambda {
        proxy_class_id: ClassId,
        num_captures: usize,
    },
    GroovyCastToBoolean,
    /// Interpreter round i6 wave 133, lane L2b (`CRATONVM_INDY_GENERIC_THREAD_ROW`,
    /// default OFF; stage 3 of `i2-L6-proposal-generic-indy-invoke-path`):
    /// the instruction's published row is a `Linked` generic `CallSite`. The
    /// copy holds NO heap reference (the `CallSite` stays rooted and remapped
    /// in `GENERIC_INDY_SITES` only); it records that the per-entry
    /// `resolution_cache` probe misses for this instruction and carries the
    /// method key, so a hit goes straight to the instruction's table row
    /// ([`run_thread_linked_row`]) without the `resolution_cache` read lock or
    /// a re-hash of the method name and descriptor.
    LinkedRow,
}

impl ThreadInstructionLink {
    /// [`Self::of`], plus [`Self::LinkedRow`] for a `Linked` row when
    /// `linked_rows` (the VM's `indy_generic_thread_row`).
    pub(in crate::runtime::invokedynamic) fn of_with(link: &GenericIndyLink, linked_rows: bool) -> Option<Self> {
        match link {
            GenericIndyLink::Linked(_) if linked_rows => Some(Self::LinkedRow),
            other => Self::of(other),
        }
    }

    pub(in crate::runtime::invokedynamic) fn of(link: &GenericIndyLink) -> Option<Self> {
        match link {
            GenericIndyLink::Lambda {
                proxy_class_id,
                num_captures,
            } => Some(Self::Lambda {
                proxy_class_id: *proxy_class_id,
                num_captures: *num_captures,
            }),
            GenericIndyLink::GroovyCastToBoolean => Some(Self::GroovyCastToBoolean),
            GenericIndyLink::Linked(_) | GenericIndyLink::Failed(_) => None,
        }
    }
}

pub(in crate::runtime::invokedynamic) struct ThreadInstructionSite {
    pub(in crate::runtime::invokedynamic) vm_identity: usize,
    pub(in crate::runtime::invokedynamic) class_id: ClassId,
    pub(in crate::runtime::invokedynamic) cp_index: u16,
    pub(in crate::runtime::invokedynamic) pc: usize,
    pub(in crate::runtime::invokedynamic) method_name: Arc<str>,
    pub(in crate::runtime::invokedynamic) method_descriptor: Arc<str>,
    /// `lambda_site_method_key_of(method_name, method_descriptor)`, hashed
    /// once at the fill (wave 16): a zero-capture lambda's singleton key is
    /// `(method_key, pc)`, which every hit used to re-hash from the frame.
    pub(in crate::runtime::invokedynamic) method_key: u64,
    pub(in crate::runtime::invokedynamic) gate: crate::classloading::resolution::RedefineGate,
    pub(in crate::runtime::invokedynamic) link: ThreadInstructionLink,
}

/// The instruction identity a probe or fill is keyed by.
pub(in crate::runtime::invokedynamic) struct InstructionIdent<'a> {
    pub(in crate::runtime::invokedynamic) vm_identity: usize,
    pub(in crate::runtime::invokedynamic) class_id: ClassId,
    pub(in crate::runtime::invokedynamic) cp_index: u16,
    pub(in crate::runtime::invokedynamic) pc: usize,
    pub(in crate::runtime::invokedynamic) method_name: &'a Arc<str>,
    pub(in crate::runtime::invokedynamic) method_descriptor: &'a Arc<str>,
}

impl<'a> InstructionIdent<'a> {
    pub(in crate::runtime::invokedynamic) fn of_frame(shared: &SharedVm, frame: &'a Frame, class_id: ClassId, cp_index: u16) -> Self {
        Self {
            vm_identity: shared.vm_identity,
            class_id,
            cp_index,
            pc: frame.pc,
            method_name: frame.method_name_arc_ref(),
            method_descriptor: frame.method_descriptor_arc_ref(),
        }
    }

    pub(in crate::runtime::invokedynamic) fn slot(&self) -> usize {
        let key = (((self.class_id.as_u32() as u64) << 16) | self.cp_index as u64)
            ^ (self.pc as u64).rotate_left(40);
        (key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 58) as usize & (THREAD_INSTRUCTION_SLOTS - 1)
    }
}

/// A lazily allocated direct-mapped table; see the section note above.
#[derive(Default)]
pub(in crate::runtime::invokedynamic) struct ThreadInstructionSites {
    pub(in crate::runtime::invokedynamic) slots: Vec<Option<ThreadInstructionSite>>,
}

impl ThreadInstructionSites {
    #[cfg(test)]
    pub(in crate::runtime::invokedynamic) fn get(&self, ident: &InstructionIdent<'_>) -> Option<ThreadInstructionLink> {
        self.get_keyed(ident).map(|(link, _)| link)
    }

    /// The instruction's link and its method key (see
    /// [`ThreadInstructionSite::method_key`]).
    pub(in crate::runtime::invokedynamic) fn get_keyed(&self, ident: &InstructionIdent<'_>) -> Option<(ThreadInstructionLink, u64)> {
        let site = self.slots.get(ident.slot())?.as_ref()?;
        let same_method =
            |held: &Arc<str>, want: &Arc<str>| Arc::ptr_eq(held, want) || **held == **want;
        if site.vm_identity != ident.vm_identity
            || site.class_id != ident.class_id
            || site.cp_index != ident.cp_index
            || site.pc != ident.pc
            || !same_method(&site.method_name, ident.method_name)
            || !same_method(&site.method_descriptor, ident.method_descriptor)
            || site.gate.is_stale()
        {
            return None;
        }
        Some((site.link, site.method_key))
    }

    pub(in crate::runtime::invokedynamic) fn put(
        &mut self,
        ident: &InstructionIdent<'_>,
        gate: crate::classloading::resolution::RedefineGate,
        link: ThreadInstructionLink,
    ) {
        if self.slots.is_empty() {
            self.slots = (0..THREAD_INSTRUCTION_SLOTS).map(|_| None).collect();
        }
        let idx = ident.slot();
        if let Some(slot) = self.slots.get_mut(idx) {
            *slot = Some(ThreadInstructionSite {
                vm_identity: ident.vm_identity,
                class_id: ident.class_id,
                cp_index: ident.cp_index,
                pc: ident.pc,
                method_name: Arc::clone(ident.method_name),
                method_descriptor: Arc::clone(ident.method_descriptor),
                method_key: lambda_site_method_key_of(ident.method_name, ident.method_descriptor),
                gate,
                link,
            });
        }
    }
}

thread_local! {
    pub(in crate::runtime::invokedynamic) static THREAD_INSTRUCTION_SITES: std::cell::RefCell<ThreadInstructionSites> =
        std::cell::RefCell::new(ThreadInstructionSites::default());
}

/// This thread's copy of the instruction's linkage and the instruction's
/// method key, if any. `None` also when the thread-local is being torn down.
pub(in crate::runtime::invokedynamic) fn probe_thread_instruction_site(
    ident: &InstructionIdent<'_>,
) -> Option<(ThreadInstructionLink, u64)> {
    THREAD_INSTRUCTION_SITES
        .try_with(|sites| sites.try_borrow().ok().and_then(|s| s.get_keyed(ident)))
        .ok()
        .flatten()
}

/// Copy a fresh table row this thread just read; `linked_rows` (the VM's
/// `indy_generic_thread_row`) also copies a `Linked` row, as the heap-free
/// [`ThreadInstructionLink::LinkedRow`] marker.
pub(in crate::runtime::invokedynamic) fn fill_thread_instruction_site_with(
    ident: &InstructionIdent<'_>,
    gate: crate::classloading::resolution::RedefineGate,
    link: &GenericIndyLink,
    linked_rows: bool,
) {
    let Some(link) = ThreadInstructionLink::of_with(link, linked_rows) else {
        return;
    };
    let _ = THREAD_INSTRUCTION_SITES.try_with(|sites| {
        if let Ok(mut s) = sites.try_borrow_mut() {
            s.put(ident, gate, link);
        }
    });
}

/// Execute a per-thread copy; the same arms as [`run_generic_indy_link`].
/// `method_key` is the executing method's (the copy matched it by name and
/// descriptor), so a zero-capture lambda's singleton key is
/// `(method_key, pc)` without hashing the method again.
pub(in crate::runtime::invokedynamic) fn run_thread_instruction_link(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    link: ThreadInstructionLink,
    method_key: u64,
) -> Result<(), MethodCallFailed> {
    match link {
        ThreadInstructionLink::Lambda {
            proxy_class_id,
            num_captures,
        } => {
            let singleton_site =
                (num_captures == 0).then(|| (method_key, thread.frames[frame_idx].pc));
            allocate_lambda_proxy(
                shared,
                thread,
                frame_idx,
                proxy_class_id,
                num_captures,
                singleton_site,
            )
        }
        ThreadInstructionLink::GroovyCastToBoolean => {
            groovy_cast_to_boolean(shared, thread, frame_idx)
        }
        // Served by `run_thread_linked_row` at the probe, which falls through
        // to the shared path when the row is gone; never routed here.
        ThreadInstructionLink::LinkedRow => Err(VmError::Internal {
            message: "invokedynamic: a linked-row thread copy reached the copy runner".to_owned(),
        }
        .into()),
    }
}

/// Interpreter round i6 wave 133, lane L2b (`CRATONVM_INDY_GENERIC_THREAD_ROW`):
/// run the instruction's `GENERIC_INDY_SITES` row from a
/// [`ThreadInstructionLink::LinkedRow`] copy, keyed with the copy's method key
/// and the frame's pc -- exactly the key [`generic_indy_site_key`] derives,
/// without re-hashing the method, and without the per-entry
/// `resolution_cache` probe (which a generic per-instruction linkage never
/// has a row in). `None` when the row is gone or stale: the caller takes the
/// shared path, which relinks. The `CallSite` read here is held unpinned only
/// until [`invoke_generic_call_site`] pins it, with no safepoint between, as
/// on the shared fast path.
pub(in crate::runtime::invokedynamic) fn run_thread_linked_row(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    class_id: ClassId,
    cp_index: u16,
    method_key: u64,
) -> Option<Result<(), MethodCallFailed>> {
    let key: GenericIndyKey = (
        shared.vm_identity,
        class_id,
        cp_index,
        method_key,
        thread.frames[frame_idx].pc,
    );
    let (link, shape) = probe_generic_indy_site(&key)?;
    Some(run_generic_indy_link(shared, thread, frame_idx, link, &shape, None))
}

/// Publish `slot` for `key`, FIRST WRITER WINS, and return the link that is
/// actually published. Two threads that link one instruction concurrently both
/// run the bootstrap, but only one result is installed and both use it —
/// HotSpot's `MethodHandleNatives.linkCallSite` publication. A success and a
/// failure race the same way: whichever is published first is the linkage.
pub(in crate::runtime::invokedynamic) fn publish_generic_indy_site(key: GenericIndyKey, slot: GenericIndySlot) -> GenericIndyLink {
    let mut table = generic_indy_sites().write();
    match table.get(&key) {
        Some(existing) if !existing.gate.is_stale() => return existing.link.clone(),
        Some(_) => {}
        None if table.len() >= GENERIC_INDY_SITE_CAP && generic_indy_cap_refuses_links() => {
            return slot.link
        }
        None => {}
    }
    let link = slot.link.clone();
    table.insert(key, slot);
    drop(table);
    ANY_GENERIC_INDY_SITE.store(true, std::sync::atomic::Ordering::Release);
    link
}

/// GC root scan for the cached `CallSite`s of the VM `vm_identity`: hands
/// `visit` each linked call site with the id of the class whose instruction
/// linked it. Driven by the `"indy-call-sites"` row of `memory::native_roots`,
/// which decides per call site whether to root it or pin it to that class's
/// loader (`native_roots::defer_or_root`).
///
/// Same conditional shape as the resolution cache's condy values
/// (`memory::roots` step 13) and the `ClassValue` cache: while the collector
/// runs in loader-weak mode, a call site of a class defined by a user loader is
/// reachable only through that loader (`metadata_pin`), so caching it does not
/// keep the loader — and every class it defined — alive.
///
/// The deferral used to be decided here, from the process-global
/// `metadata_weak_mode()` and the VM-less `loader_pin_addr`; both can answer
/// for another live VM (gc-common w5-b,
/// `docs/feature-designs/real-jdk-mode.md`).
pub fn gc_scan_generic_indy_roots(vm_identity: usize, visit: &mut dyn FnMut(u32, ObjectRef)) {
    if !ANY_GENERIC_INDY_SITE.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    let table = generic_indy_sites().read();
    for (&(vid, class_id, _, _, _), slot) in table.iter() {
        if vid != vm_identity {
            continue;
        }
        let GenericIndyLink::Linked(call_site) = &slot.link else {
            continue;
        };
        visit(class_id.as_u32(), *call_site);
    }
}

/// GC post-move remap for [`gc_scan_generic_indy_roots`]'s call sites.
pub fn gc_update_generic_indy_refs(vm_identity: usize, pointer_map: &cratonvm_types::PointerMap) {
    if pointer_map.is_empty() || !ANY_GENERIC_INDY_SITE.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    let mut table = generic_indy_sites().write();
    remap_generic_indy_rows(&mut table, vm_identity, pointer_map);
}

pub(in crate::runtime::invokedynamic) fn remap_generic_indy_rows(
    table: &mut GenericIndyMap,
    vm_identity: usize,
    pointer_map: &cratonvm_types::PointerMap,
) {
    for (&(vid, _, _, _, _), slot) in table.iter_mut() {
        if vid != vm_identity {
            continue;
        }
        if let GenericIndyLink::Linked(call_site) = &mut slot.link {
            if let Some(&new_addr) = pointer_map.get(&(call_site.as_ptr() as usize)) {
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                *call_site = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    }
}

/// Drop every row of `vm_identity` whose caller class — or whose recorded
/// error class — was just unloaded. Called from
/// `memory::gc::unload_dead_class_metadata`. Required, not hygiene: a row whose
/// loader died was not marked through `metadata_pin`, so its `CallSite` address
/// no longer names an object and must never be scanned again.
pub fn forget_unloaded_generic_indy_sites(
    vm_identity: usize,
    unloaded: &rustc_hash::FxHashSet<ClassId>,
) {
    // The lambda private-impl memo rides this hook (same caller, same set).
    crate::runtime::interpreter::forget_unloaded_private_impl_memo(vm_identity, unloaded);
    if unloaded.is_empty() {
        return;
    }
    // So does the shared-entry memo: a class id is not reused after an unload,
    // so this is hygiene, not correctness.
    if let Some(memo) = SHARED_INDY_ENTRIES.get() {
        memo.write()
            .retain(|&(vid, class_id), _| vid != vm_identity || !unloaded.contains(&class_id));
    }
    if !ANY_GENERIC_INDY_SITE.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    let mut table = generic_indy_sites().write();
    retain_live_generic_indy_rows(&mut table, vm_identity, unloaded);
}

/// Class-unload hook for the lambda-proxy side tables, called from
/// `memory::gc::unload_dead_class_metadata` with the set of unloaded classes.
///
/// A proxy id is synthetic and never itself in `unloaded`; what dies with a
/// class is every proxy its `invokedynamic` instructions spun (its "host",
/// `lambda_proxy_hosts`). Dropped here:
///
/// * the `lambda_proxy_hosts` rows naming an unloaded host;
/// * `ClassRealm::lambda_impl_owner_memo` rows whose proxy's host, or whose
///   implementation owner, unloaded. The host half is new: a row whose owner
///   outlives the host (a JDK method reference such as `String::length`
///   written in an unloaded class) used to stay forever. It is a pure memo, so
///   a proxy instance still reachable after its host unloaded recomputes it;
/// * the zero-capture singleton rows of those proxies. The host's
///   instructions can never execute again, and the row would otherwise name
///   an instance this collection reclaimed (it is pinned to the host's loader,
///   `gc_scan_lambda_singleton_roots`);
/// * since wave 8, the proxies themselves: the `lambda_proxies` call site, the
///   `altMetafactory` marker and bridge lists, the user-loader pin list and
///   the proxy-keyed dispatch memos. A proxy spun by a user-loader host has a
///   `loader_pin` row naming that loader (`pin_lambda_proxy_to_host_loader`),
///   so while any instance of it is live the loader is marked and the host
///   cannot unload; an unloaded host therefore has no live proxy instance
///   left to dispatch. (Before that row existed these were kept for orphaned
///   instances, and accumulated towards `MAX_LAMBDA_PROXIES` in a redeploy
///   loop.) A built-in-loader host never unloads. Since wave 9 the reflective
///   metafactory path records its lookup class as the host
///   (`record_reflective_lambda_proxy_host`), so its proxies go here too;
/// * the reflective `LambdaMetafactory` `CallSite` cache rows keyed by an
///   unloaded host (`lang_invoke::forget_lambda_callsites_of_unloaded_hosts`):
///   each row's factory handle names one of the proxies dropped above, and
///   a cache hit would otherwise allocate instances of an unregistered id.
pub fn forget_unloaded_lambda_proxy_rows(
    shared: &SharedVm,
    unloaded: &rustc_hash::FxHashSet<ClassId>,
) {
    if unloaded.is_empty() {
        return;
    }
    let mut dead_host: rustc_hash::FxHashSet<ClassId> = rustc_hash::FxHashSet::default();
    shared
        .classes
        .lambda_proxy_hosts
        .write()
        .retain(|proxy, host| {
            let dead = unloaded.contains(proxy) || unloaded.contains(host);
            if dead {
                dead_host.insert(*proxy);
            }
            !dead
        });
    shared
        .classes
        .lambda_impl_owner_memo
        .write()
        .retain(|proxy, owner| {
            !unloaded.contains(proxy) && !unloaded.contains(owner) && !dead_host.contains(proxy)
        });
    // Keyed by the lookup class, which is the host recorded above; asked
    // before the early return so a row never depends on that record.
    cratonvm_native_builtins::lang_invoke::forget_lambda_callsites_of_unloaded_hosts(
        shared.vm_identity,
        &|host| unloaded.contains(&ClassId::new(host)),
    );
    if dead_host.is_empty() {
        return;
    }
    let vm_identity = shared.vm_identity;
    lambda_singleton_cache()
        .write()
        .retain(|&(vid, proxy, _, _), slot| {
            let keep = vid != vm_identity || !dead_host.contains(&proxy);
            if !keep {
                // A warm-path memo may still hold the slot.
                slot.retire();
            }
            keep
        });
    // JIT round 20 wave 12 (lane qLam, W15L5-7): a removed row's lock-free
    // hit bit is cleared under the same write guard.
    shared.classes.lambda_proxies.write().retain(|proxy, _| {
        let keep = !dead_host.contains(proxy);
        if !keep {
            shared.classes.forget_lambda_proxy_hit(*proxy);
        }
        keep
    });
    if ANY_LAMBDA_MARKERS.load(std::sync::atomic::Ordering::Acquire) {
        lambda_proxy_markers()
            .lock()
            .retain(|&(vid, proxy), _| vid != vm_identity || !dead_host.contains(&proxy));
    }
    if ANY_LAMBDA_BRIDGES.load(std::sync::atomic::Ordering::Acquire) {
        lambda_proxy_bridges()
            .lock()
            .retain(|&(vid, proxy), _| vid != vm_identity || !dead_host.contains(&proxy));
    }
    if ANY_USER_LOADER_LAMBDA.load(std::sync::atomic::Ordering::Acquire) {
        if let Some(rows) = user_loader_lambdas().lock().get_mut(&vm_identity) {
            rows.retain(|(proxy, _)| !dead_host.contains(proxy));
        }
    }
    crate::runtime::interpreter::forget_dead_lambda_proxy_memo_rows(vm_identity, &dead_host);
}

/// Drop every row of a disposed VM (`vm::vm_init::release_vm_native_state`).
pub fn forget_vm_generic_indy_sites(vm_identity: usize) {
    // The lambda private-impl memo rides this hook (same caller).
    crate::runtime::interpreter::forget_vm_private_impl_memo(vm_identity);
    // So do the two process-global lambda side tables keyed by VM. Their rows
    // were never dropped: a disposed VM's singletons stayed in a table the
    // GC scans on every collection of every other VM, and its marker lists
    // stayed allocated. `vm_identity` is never reused, so this is a leak fix.
    lambda_singleton_cache()
        .write()
        .retain(|&(vid, _, _, _), slot| {
            if vid == vm_identity {
                slot.retire();
            }
            vid != vm_identity
        });
    if ANY_LAMBDA_MARKERS.load(std::sync::atomic::Ordering::Acquire) {
        lambda_proxy_markers()
            .lock()
            .retain(|&(vid, _), _| vid != vm_identity);
    }
    if ANY_LAMBDA_BRIDGES.load(std::sync::atomic::Ordering::Acquire) {
        lambda_proxy_bridges()
            .lock()
            .retain(|&(vid, _), _| vid != vm_identity);
    }
    // The VM's loader-pin rows go with `loader_pin::forget_vm_loader_pins`.
    if ANY_USER_LOADER_LAMBDA.load(std::sync::atomic::Ordering::Acquire) {
        user_loader_lambdas().lock().remove(&vm_identity);
    }
    if let Some(memo) = SHARED_INDY_ENTRIES.get() {
        memo.write().retain(|&(vid, _), _| vid != vm_identity);
    }
    if !ANY_GENERIC_INDY_SITE.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    generic_indy_sites()
        .write()
        .retain(|&(vid, _, _, _, _), _| vid != vm_identity);
}

pub(in crate::runtime::invokedynamic) fn retain_live_generic_indy_rows(
    table: &mut GenericIndyMap,
    vm_identity: usize,
    unloaded: &rustc_hash::FxHashSet<ClassId>,
) {
    table.retain(|&(vid, class_id, _, _, _), slot| {
        vid != vm_identity || !(unloaded.contains(&class_id) || slot.link.mentions_class(unloaded))
    });
}

/// Execute a cached per-instruction linkage.
///
/// `singleton_site`: see [`execute_invokedynamic_at`]; only a zero-capture
/// [`GenericIndyLink::Lambda`] reads it.
pub(in crate::runtime::invokedynamic) fn run_generic_indy_link(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    link: GenericIndyLink,
    shape: &GenericIndyShape,
    singleton_site: Option<(u64, usize)>,
) -> Result<(), MethodCallFailed> {
    match link {
        GenericIndyLink::Linked(call_site) => {
            invoke_generic_call_site(shared, thread, frame_idx, call_site, shape)
        }
        GenericIndyLink::Failed(recorded) => {
            Err(raise_recorded_linkage_error(shared, thread, &recorded))
        }
        GenericIndyLink::GroovyCastToBoolean => groovy_cast_to_boolean(shared, thread, frame_idx),
        GenericIndyLink::Lambda {
            proxy_class_id,
            num_captures,
        } => allocate_lambda_proxy(
            shared,
            thread,
            frame_idx,
            proxy_class_id,
            num_captures,
            singleton_site,
        ),
    }
}

/// The per-instruction table key of the `invokedynamic` executing in `frame`.
///
/// `singleton_site` is the instruction's `(method key, pc after it)` when the
/// caller's frame names no instruction — the compiled bridge's synthetic frame
/// — and `None` for an interpreter frame, whose method and pc ARE the
/// instruction. The same two halves as the zero-capture singleton key, so a
/// compiled site and the interpreted instruction find the same row.
pub(in crate::runtime::invokedynamic) fn indy_instruction_key(
    shared: &SharedVm,
    frame: &Frame,
    class_id: ClassId,
    cp_index: u16,
    singleton_site: Option<(u64, usize)>,
) -> GenericIndyKey {
    match singleton_site {
        Some((method_key, pc)) => (shared.vm_identity, class_id, cp_index, method_key, pc),
        None => generic_indy_site_key(shared, frame, class_id, cp_index),
    }
}

/// The published per-instruction lambda linkage for `key`, as the two facts an
/// allocation needs. The compiled bridge's frame-free fast path asks this after
/// the per-entry caches miss.
pub(in crate::runtime::invokedynamic) fn probe_lambda_instruction_site(key: &GenericIndyKey) -> Option<(ClassId, usize)> {
    if !ANY_GENERIC_INDY_SITE.load(std::sync::atomic::Ordering::Acquire) {
        return None;
    }
    match probe_generic_indy_site(key)?.0 {
        GenericIndyLink::Lambda {
            proxy_class_id,
            num_captures,
        } => Some((proxy_class_id, num_captures)),
        _ => None,
    }
}

/// Publish this bootstrap's proxy as the linkage of the instruction `key`,
/// FIRST WRITER WINS, and return the proxy class the instruction uses — the
/// winner's, when another thread linked it first.
///
/// NOT subject to `GENERIC_INDY_SITE_CAP` (wave 7). At the cap this used to
/// publish nothing and the caller fell back to the PER-ENTRY
/// `resolution_cache` row — which every other instruction of the shared
/// entry then found FIRST (the per-entry probe precedes the per-instruction
/// one), so an instruction that already had its own proxy switched to the
/// capped one's: a different `getClass()` and, for a non-capturing lambda, a
/// new singleton for the same instruction. A lambda row is one per
/// shared-entry instruction of loaded code, removed with its class, so the
/// table stays bounded by the code without the cap; and a shared entry now
/// never has a per-entry row, which is what lets the per-thread copy
/// (`ThreadInstructionSites`) be served before the per-entry probe.
pub(in crate::runtime::invokedynamic) fn publish_lambda_instruction_site(
    shared: &SharedVm,
    key: GenericIndyKey,
    target_descriptor: &str,
    proxy_class_id: ClassId,
    num_captures: usize,
) -> ClassId {
    let gate = generic_indy_gate(shared, key.1);
    let mut table = generic_indy_sites().write();
    if let Some(existing) = table.get(&key) {
        if !existing.gate.is_stale() {
            if let GenericIndyLink::Lambda {
                proxy_class_id: installed,
                ..
            } = existing.link
            {
                return installed;
            }
            // A row of another kind for a lambda instruction cannot exist (the
            // constant pool decides the bootstrap); replace it.
        }
    }
    table.insert(
        key,
        GenericIndySlot {
            link: GenericIndyLink::Lambda {
                proxy_class_id,
                num_captures,
            },
            shape: Arc::new(GenericIndyShape::new(target_descriptor)),
            gate,
        },
    );
    drop(table);
    ANY_GENERIC_INDY_SITE.store(true, std::sync::atomic::Ordering::Release);
    proxy_class_id
}

// ---------------------------------------------------------------------------
// Shared `CONSTANT_InvokeDynamic` entries
//
// javac deduplicates constant-pool entries, so two identical method references
// in one class (`String::length` written twice, in one method or in two)
// compile to two `invokedynamic` instructions naming ONE entry. Every lambda
// body has its own entry (its own implementation method), so shared entries
// are rare, and only they need per-instruction linkage. Which entries a class
// shares is a pure function of its bytecode: computed once per class, on its
// first lambda bootstrap, and memoised per VM behind the class's redefinition
// gate (a redefinition replaces the bytecode).
// ---------------------------------------------------------------------------

/// `(vm_identity, class)` -> the class's shared `invokedynamic` CP indices,
/// sorted, and the redefinition gate they were computed under.
pub(in crate::runtime::invokedynamic) type SharedIndyEntryMap = rustc_hash::FxHashMap<
    (usize, ClassId),
    (crate::classloading::resolution::RedefineGate, Arc<[u16]>),
>;

pub(in crate::runtime::invokedynamic) static SHARED_INDY_ENTRIES: std::sync::OnceLock<parking_lot::RwLock<SharedIndyEntryMap>> =
    std::sync::OnceLock::new();

pub(in crate::runtime::invokedynamic) fn shared_indy_entries() -> &'static parking_lot::RwLock<SharedIndyEntryMap> {
    SHARED_INDY_ENTRIES.get_or_init(|| parking_lot::RwLock::new(SharedIndyEntryMap::default()))
}

/// The CP indices that two or more `invokedynamic` instructions of the class
/// (across all its methods) name, sorted.
pub(in crate::runtime::invokedynamic) fn scan_shared_indy_entries<'a>(codes: impl Iterator<Item = &'a [u8]>) -> Vec<u16> {
    const INVOKEDYNAMIC: u8 = 0xba;
    let mut uses: rustc_hash::FxHashMap<u16, u32> = rustc_hash::FxHashMap::default();
    for code in codes {
        let mut pc = 0usize;
        while pc < code.len() {
            if code[pc] == INVOKEDYNAMIC && pc + 2 < code.len() {
                let idx = u16::from_be_bytes([code[pc + 1], code[pc + 2]]);
                *uses.entry(idx).or_insert(0) += 1;
            }
            // The walk every other bytecode scan in the VM uses (switch padding,
            // `wide`); `max(1)` guarantees progress on a malformed tail.
            pc += cratonvm_jit::bytecode_insn_len(code, pc).max(1);
        }
    }
    let mut shared: Vec<u16> = uses
        .into_iter()
        .filter(|&(_, n)| n > 1)
        .map(|(idx, _)| idx)
        .collect();
    shared.sort_unstable();
    shared
}

/// `true` when another `invokedynamic` instruction of `class_id` names the same
/// CP entry `cp_index`. Cold: asked once per lambda bootstrap.
pub(in crate::runtime::invokedynamic) fn lambda_entry_is_shared(shared: &SharedVm, class_id: ClassId, cp_index: u16) -> bool {
    let key = (shared.vm_identity, class_id);
    let memo = shared_indy_entries()
        .read()
        .get(&key)
        .filter(|(gate, _)| !gate.is_stale())
        .map(|(_, entries)| Arc::clone(entries));
    let entries = match memo {
        Some(entries) => entries,
        None => {
            // Gate BEFORE the scan: a redefinition racing the scan then leaves
            // the row stale rather than current over the old bytecode.
            let gate = generic_indy_gate(shared, class_id);
            let scanned: Arc<[u16]> = {
                let cm = shared.classes.class_manager.read();
                let Some(class) = cm.get_class(class_id) else {
                    return false;
                };
                Arc::from(scan_shared_indy_entries(
                    class
                        .methods
                        .iter()
                        .filter_map(|m| m.code())
                        .map(|c| &c.code[..]),
                ))
            };
            shared_indy_entries()
                .write()
                .insert(key, (gate, Arc::clone(&scanned)));
            scanned
        }
    };
    entries.binary_search(&cp_index).is_ok()
}

/// Snapshot of `current_class_id`'s redefinition generation, for a new row.
pub(in crate::runtime::invokedynamic) fn generic_indy_gate(
    shared: &SharedVm,
    current_class_id: ClassId,
) -> crate::classloading::resolution::RedefineGate {
    let counter = shared
        .classes
        .class_manager
        .read()
        .class_redefine_generation_handle(current_class_id);
    crate::classloading::resolution::RedefineGate::snapshot(counter)
}

/// `true` when `call_site` is a `java.lang.invoke.ConstantCallSite`.
pub(in crate::runtime::invokedynamic) fn is_constant_call_site(shared: &SharedVm, call_site: ObjectRef) -> bool {
    let cm = shared.classes.class_manager.read();
    let Some(ccs) = cm.get_loaded_class_id("java/lang/invoke/ConstantCallSite") else {
        return false;
    };
    cm.is_subclass_of(shared.mem.heap.class_id_of(call_site), ccs)}

/// `Throwable.detailMessage` of `throwable` (the field, as HotSpot's
/// `java_lang_Throwable::message_as_utf8` reads it — not `getMessage()`).
pub(in crate::runtime::invokedynamic) fn throwable_detail_message(shared: &SharedVm, throwable: ObjectRef) -> Option<String> {
    let class_id = shared.mem.heap.class_id_of(throwable);
    let slot =
        crate::vm::vm_exec::resolve_field_slot_by_name_cached(shared, class_id, "detailMessage")?;
    match shared.mem.heap.get_field(throwable, slot) {
        Value::Object(Some(s)) => read_java_string(&shared.mem.heap, s),
        _ => None,
    }
}

/// What to record for a linkage that failed with `thrown`, or `None` when it is
/// not a `LinkageError`. HotSpot records only those
/// (`ConstantPool::save_and_throw_indy_exc`); anything else — an
/// `OutOfMemoryError`, a `StackOverflowError` — leaves the instruction
/// unlinked, and the next execution runs the bootstrap again.
///
/// Allocation-free: it runs between a bootstrap's return and the pin of its
/// result.
pub(in crate::runtime::invokedynamic) fn record_linkage_error(shared: &SharedVm, thrown: ObjectRef) -> Option<RecordedLinkageError> {
    let error_class = shared.mem.heap.class_id_of(thrown);
    let (error_class, error_class_name, not_boot_visible) = {
        let cm = shared.classes.class_manager.read();
        let linkage = cm.get_loaded_class_id("java/lang/LinkageError")?;
        if !cm.is_subclass_of(error_class, linkage) {
            return None;
        }
        let name = cm.get_class(error_class)?.name.to_string();
        // t1 wave 9, lane STRREC (page
        // `t1-I1-indy-record-rethrows-a-user-linkage-error-class-20261026`;
        // `CRATONVM_LOADER_RESOLUTION_ERROR_RECORD_HOTSPOT`, default ON):
        // HotSpot records only the error's class NAME
        // (`save_and_throw_indy_exc`) and its rethrow resolves that name in
        // the bootstrap loader (`Exceptions::new_exception`, null loader). A
        // class another loader defined is not found there, so every later
        // execution throws `NoClassDefFoundError: <internal name>`. A
        // `java/` class is always the bootstrap's.
        let hotspot =
            crate::runtime::interpreter::constants::resolution_error_record_hotspot_enabled();
        let not_boot = hotspot
            && !name.starts_with("java/")
            && !matches!(
                cm.get_loader_id(error_class),
                None | Some(cratonvm_types::ClassLoaderId::Bootstrap)
            );
        // t1 wave 12, lane STRREC2: `NoClassDefFoundError` need not be loaded
        // yet (nothing preloads it; the wave-9 record fell back to the
        // original class whenever no NCDFE had been thrown before, so runs
        // #2/#3 of `T1W9StrrecIndyUserLinkageError` rethrew `MyLinkError:
        // custom`). Unloaded, the class is left to the rethrow, which loads
        // it by name; this record must not load anything (it runs between a
        // bootstrap's return and the pin of its result).
        if not_boot {
            let ncdfe = cm.get_loaded_class_id("java/lang/NoClassDefFoundError");
            (ncdfe, "java/lang/NoClassDefFoundError".to_string(), Some(name))
        } else {
            (Some(error_class), name, None)
        }
    };
    // The NoClassDefFoundError's message is the class's internal name; the
    // original message is not kept (HotSpot's record has no room for both).
    let message = match not_boot_visible {
        Some(internal_name) => Some(internal_name),
        None => throwable_detail_message(shared, thrown),
    };
    Some(RecordedLinkageError {
        error_class,
        error_class_name,
        message,
    })
}

/// Throw the recorded failure of an instruction whose linkage already failed:
/// a NEW error of the recorded class and message and no cause —
/// `ConstantPool::throw_resolution_error` over an indy row, whose cause
/// `save_and_throw_indy_exc` never records.
#[cold]
pub(in crate::runtime::invokedynamic) fn raise_recorded_linkage_error(
    shared: &SharedVm,
    thread: &mut JvmThread,
    recorded: &RecordedLinkageError,
) -> MethodCallFailed {
    let created = match recorded.error_class {
        Some(class_id) => crate::runtime::exceptions::create_exception_object_for_class(
            shared,
            thread,
            class_id,
            &recorded.error_class_name,
            recorded.message.as_deref(),
        ),
        // A bootstrap class not loaded when the failure was recorded
        // (`NoClassDefFoundError`, see `record_linkage_error`): by name,
        // through the boot chain, as HotSpot's `Exceptions::new_exception`.
        None => crate::runtime::exceptions::create_exception_object(
            shared,
            thread,
            &recorded.error_class_name,
            recorded.message.as_deref(),
        ),
    };
    match created {
        Ok(error) => MethodCallFailed::ExceptionThrown(error),
        Err(e) => e,
    }
}

/// Turn this execution's own linkage outcome into the instruction's published
/// linkage, and return the `CallSite` to invoke.
///
/// * A `CallSite` the mode caches is published; if another thread published
///   first, ITS call site is used, so every thread observes one `CallSite`.
/// * A `LinkageError` is recorded and published. The publishing thread throws
///   its original error object; a thread that loses to a published success
///   uses that success instead of failing (HotSpot does the same).
/// * Anything else — an uncached mutable call site, a non-`LinkageError`
///   failure, a VM-internal error — is returned unchanged and not published.
///
/// Must not reach a safepoint on the success path: the caller holds the
/// returned `CallSite` unpinned until `invoke_generic_call_site` pins it.
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::invokedynamic) fn settle_generic_link(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    current_class_id: ClassId,
    cp_index: u16,
    cache_mutable: bool,
    shape: &Arc<GenericIndyShape>,
    linked: Result<ObjectRef, MethodCallFailed>,
) -> Result<ObjectRef, MethodCallFailed> {
    let own = match &linked {
        Ok(call_site) => (cache_mutable || is_constant_call_site(shared, *call_site))
            .then_some(GenericIndyLink::Linked(*call_site)),
        Err(MethodCallFailed::ExceptionThrown(thrown)) => {
            record_linkage_error(shared, *thrown).map(|r| GenericIndyLink::Failed(Arc::new(r)))
        }
        Err(_) => None,
    };
    let Some(own) = own else {
        return linked;
    };
    let key = generic_indy_site_key(
        shared,
        &thread.frames[frame_idx],
        current_class_id,
        cp_index,
    );
    let slot = GenericIndySlot {
        link: own.clone(),
        shape: Arc::clone(shape),
        gate: generic_indy_gate(shared, current_class_id),
    };
    match (publish_generic_indy_site(key, slot), own) {
        (GenericIndyLink::Linked(call_site), _) => Ok(call_site),
        // Ours is the published failure: throw the original error object.
        (GenericIndyLink::Failed(published), GenericIndyLink::Failed(mine))
            if Arc::ptr_eq(&published, &mine) =>
        {
            linked
        }
        (GenericIndyLink::Failed(published), _) => {
            Err(raise_recorded_linkage_error(shared, thread, &published))
        }
        // A cast-to-boolean or lambda row for the same instruction cannot
        // exist (the constant pool decides the shape); keep this execution's
        // own outcome.
        (GenericIndyLink::GroovyCastToBoolean | GenericIndyLink::Lambda { .. }, _) => linked,
    }
}

/// Publish a natively linked site's linkage failure (`LambdaMetafactory`,
/// `SwitchBootstraps`, `ObjectMethods`) for its instruction,
/// as [`settle_generic_link`] publishes a generic one (JVMS §5.4.3: every
/// later execution of a failed `invokedynamic` throws a new error of the same
/// class and message, `ConstantPool::save_and_throw_indy_exc`). This
/// execution throws `failed` itself; a thread that lost the publication runs
/// the published link. Only a thrown `LinkageError` is recorded (a
/// `BootstrapMethodError` refusal, a resolution error the interpreter already
/// raised); a VM-side error such as the `NoSuchMethodError` that
/// [`lambda_impl_resolves`] returns unconverted, and every failure while the
/// per-instruction table is switched off, is returned unchanged, and the next
/// execution validates again. Interpreter round i1 wave 38, lane L4 (item 6
/// of `interpreter-L4-lambda-site-validation-leaves-undecidable-shapes-linked-FIXED-20261012`).
///
/// Allocation-free between `failed`'s creation and its return, like
/// [`settle_generic_link`]: the thrown object is held unpinned.
#[cold]
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::invokedynamic) fn settle_native_link_failure(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    current_class_id: ClassId,
    cp_index: u16,
    singleton_site: Option<(u64, usize)>,
    target_descriptor: &str,
    failed: MethodCallFailed,
) -> Result<(), MethodCallFailed> {
    if generic_indy_cache_mode() == GenericIndyCacheMode::Off {
        return Err(failed);
    }
    let MethodCallFailed::ExceptionThrown(thrown) = &failed else {
        return Err(failed);
    };
    let Some(recorded) = record_linkage_error(shared, *thrown) else {
        return Err(failed);
    };
    let mine = Arc::new(recorded);
    let key = indy_instruction_key(
        shared,
        &thread.frames[frame_idx],
        current_class_id,
        cp_index,
        singleton_site,
    );
    let shape = Arc::new(GenericIndyShape::new(target_descriptor));
    let slot = GenericIndySlot {
        link: GenericIndyLink::Failed(Arc::clone(&mine)),
        shape: Arc::clone(&shape),
        gate: generic_indy_gate(shared, current_class_id),
    };
    match publish_generic_indy_site(key, slot) {
        GenericIndyLink::Failed(published) if Arc::ptr_eq(&published, &mine) => {
            if dbg_lambda_dispatch() {
                eprintln!(
                    "[DBG_LAMBDA] link-check recorded {} for cp#{cp_index}",
                    mine.error_class_name
                );
            }
            Err(failed)
        }
        other => run_generic_indy_link(shared, thread, frame_idx, other, &shape, singleton_site),
    }
}

/// `java.lang.invoke.LambdaConversionException`, the cause of every refusal
/// `AbstractValidatingLambdaMetafactory` raises.
pub(in crate::runtime::invokedynamic) const LAMBDA_CONVERSION_EXCEPTION: &str = "java/lang/invoke/LambdaConversionException";

/// `LambdaMetafactory.metafactory`'s descriptor. A bootstrap handle naming
/// the method with any other descriptor is not validated here.
pub(in crate::runtime::invokedynamic) const METAFACTORY_DESCRIPTOR: &str = "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;\
Ljava/lang/invoke/MethodType;Ljava/lang/invoke/MethodType;Ljava/lang/invoke/MethodHandle;\
Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/CallSite;";

/// `LambdaMetafactory.altMetafactory`'s descriptor.
pub(in crate::runtime::invokedynamic) const ALT_METAFACTORY_DESCRIPTOR: &str =
    "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;\
Ljava/lang/invoke/MethodType;[Ljava/lang/Object;)Ljava/lang/invoke/CallSite;";

/// Why a `LambdaMetafactory` call site fails linkage: the cause of the
/// `BootstrapMethodError` HotSpot raises, with its class and message.
pub(in crate::runtime::invokedynamic) struct LambdaSiteRefusal {
    pub(in crate::runtime::invokedynamic) cause_class: &'static str,
    pub(in crate::runtime::invokedynamic) message: String,
    /// The native linkage cannot build this shape at all (before interpreter
    /// round i1 wave 37 it was an uncatchable `VmError::Internal`), so the
    /// refusal is raised in every mode, not only under `--real-jdk`.
    pub(in crate::runtime::invokedynamic) every_mode: bool,
}

/// Why [`lambda_site_verdict`] stopped: the site fails a check, or the class
/// store cannot decide one as `java.lang.Class` would (an unloaded class, a
/// hierarchy with a class that has no real bytes). An undecided check ends the
/// validation with no refusal: a later check cannot be reported while an
/// earlier one might fail first.
pub(in crate::runtime::invokedynamic) enum LambdaStop {
    Refuse(LambdaSiteRefusal),
    Unknown(&'static str),
}

pub(in crate::runtime::invokedynamic) fn lambda_known<T>(value: Option<T>, what: &'static str) -> Result<T, LambdaStop> {
    value.ok_or(LambdaStop::Unknown(what))
}

pub(in crate::runtime::invokedynamic) fn lambda_conversion_refusal(message: String) -> LambdaStop {
    LambdaStop::Refuse(LambdaSiteRefusal {
        cause_class: LAMBDA_CONVERSION_EXCEPTION,
        message,
        every_mode: false,
    })
}

/// What a bootstrap static argument is at run time, as far as
/// `BootstrapMethodInvoker`'s casts and `altMetafactory`'s `extractArg` look.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::runtime::invokedynamic) enum LambdaStaticArg {
    MethodType,
    MethodHandle,
    Integer,
    Class,
    /// Another loadable constant, named by its `java.lang` class.
    Other(&'static str),
    /// A dynamic constant (its own bootstrap runs first, and may fail or
    /// answer anything, `null` included) or a malformed entry.
    Undecided,
    /// A dynamic constant that answered `null` (wave 41; see
    /// [`LambdaCondyArg`]): it passes the invoker's casts and fails
    /// `Objects.requireNonNull` (`metafactory`) or `extractArg`
    /// (`altMetafactory`).
    Null,
    /// A dynamic constant that answered an object of a class none of the
    /// other kinds names (a `java.util` or user class, a `Boolean`, ...):
    /// the invoker's cast refuses it with a message naming both classes'
    /// modules and loaders, which [`LambdaCondyArg::cast_messages`] carries
    /// (wave 46); `altMetafactory`'s `extractArg` refuses it as `argument has
    /// wrong type`.
    Foreign,
}

/// The value of a `CONSTANT_Dynamic` static argument of a lambda site,
/// resolved before the checks as HotSpot resolves every static argument
/// before the bootstrap runs ([`lambda_resolve_dynamic_args`]). Interpreter
/// round i1 wave 41, lane L4 (row 4 of
/// `interpreter-L4-lambda-site-validation-leaves-undecidable-shapes-linked-FIXED-20261012`).
#[derive(Clone)]
pub(in crate::runtime::invokedynamic) struct LambdaCondyArg {
    /// What the value is, as the casts see it (`Undecided` for a value the
    /// checks do not classify: a `MethodHandle` whose member the linkage
    /// cannot read back, a hidden class, a primitive).
    pub(in crate::runtime::invokedynamic) kind: LambdaStaticArg,
    /// The descriptor of a `MethodType` value.
    pub(in crate::runtime::invokedynamic) method_type: Option<String>,
    /// The member of a direct `MethodHandle` value (wave 42,
    /// `lang_invoke::method_handle_value_member`): a `findStatic` /
    /// `findVirtual` handle, read as the `CONSTANT_MethodHandle` that would
    /// name it. `None` for any other value.
    pub(in crate::runtime::invokedynamic) method_handle: Option<MethodHandle>,
    /// A `MethodHandle` value HotSpot's `Lookup.revealDirect` cannot crack
    /// (wave 43, `lang_invoke::method_handle_value_not_direct`): its
    /// `toString()`, which the `LambdaConversionException` quotes. `None`
    /// for a direct handle and for one this model cannot tell.
    pub(in crate::runtime::invokedynamic) not_direct: Option<String>,
    /// For a [`LambdaStaticArg::Foreign`] value: HotSpot's
    /// `ClassCastException` messages for a cast of it to
    /// `java.lang.invoke.MethodType` (0) and to `java.lang.invoke.MethodHandle`
    /// (1) (`exceptions::hotspot_class_cast_message`), computed where the
    /// value's class is in hand. Wave 46, lane L4.
    pub(in crate::runtime::invokedynamic) cast_messages: Option<Box<[String; 2]>>,
    /// The `int` of an `Integer` value (`altMetafactory`'s flags and counts)
    /// and the internal name of a `Class` value (a marker interface). Wave 47,
    /// lane L4: the tail's readers read these before the pool.
    pub(in crate::runtime::invokedynamic) int_value: Option<i32>,
    pub(in crate::runtime::invokedynamic) class_name: Option<Arc<str>>,
}

impl LambdaCondyArg {
    /// A value of `kind` that carries nothing the readers use.
    pub(in crate::runtime::invokedynamic) fn bare(kind: LambdaStaticArg) -> Self {
        Self {
            kind,
            method_type: None,
            method_handle: None,
            not_direct: None,
            cast_messages: None,
            int_value: None,
            class_name: None,
        }
    }
}

/// The `int` static argument at `position` (`altMetafactory`'s flags or a
/// count): a dynamic constant's `Integer` value when one was resolved there
/// (wave 47), else the constant-pool `CONSTANT_Integer`.
pub(in crate::runtime::invokedynamic) fn lambda_int_arg(
    cp: &ConstantPool,
    args: &[u16],
    condy: &[Option<LambdaCondyArg>],
    position: usize,
) -> Option<i32> {
    match condy.get(position) {
        Some(Some(arg)) => arg.int_value,
        _ => match cp.get(*args.get(position)?) {
            Some(ConstantPoolEntry::Integer(value)) => Some(*value),
            _ => None,
        },
    }
}

/// The `Class` static argument at `position` (an `altMetafactory` marker), as
/// an internal name: a dynamic constant's `Class` value when one was resolved
/// there (wave 47), else the constant-pool `CONSTANT_Class`.
pub(in crate::runtime::invokedynamic) fn lambda_class_arg(
    cp: &ConstantPool,
    args: &[u16],
    condy: &[Option<LambdaCondyArg>],
    position: usize,
) -> Option<Arc<str>> {
    match condy.get(position) {
        Some(Some(arg)) => arg.class_name.clone(),
        _ => cp.get_class_name_arc(*args.get(position)?),
    }
}

/// The internal name of the class a `Class` value (a dynamic constant's)
/// stands for, when the checks can name it: `None` for a primitive or hidden
/// class and for a mirror this VM cannot map back (the readers then stop
/// undecided). Wave 47, lane L4.
pub(in crate::runtime::invokedynamic) fn lambda_class_value_name(shared: &SharedVm, mirror: ObjectRef) -> Option<Arc<str>> {
    let class_id = crate::vm::class_id_from_mirror(shared, mirror)?;
    let cm = shared.classes.class_manager.read();
    let class = cm.get_class(class_id)?;
    if class.is_hidden() || primitive_class_name(&class.name) {
        return None;
    }
    Some(Arc::from(&*class.name))
}

/// A primitive (or `void`) class's name, as a mirror's class may carry it.
pub(in crate::runtime::invokedynamic) fn primitive_class_name(name: &str) -> bool {
    matches!(
        name,
        "boolean" | "byte" | "char" | "short" | "int" | "long" | "float" | "double" | "void"
    ) || (name.len() == 1 && primitive_type_name(name).is_some())
}

/// The `MethodType` static argument at `position`: a dynamic constant's value
/// when one was resolved there, else the constant-pool entry.
pub(in crate::runtime::invokedynamic) fn lambda_method_type_arg(
    cp: &ConstantPool,
    args: &[u16],
    condy: &[Option<LambdaCondyArg>],
    position: usize,
) -> Option<String> {
    match condy.get(position) {
        Some(Some(arg)) => arg.method_type.clone(),
        _ => resolve_method_type(cp, *args.get(position)?),
    }
}

/// The implementation handle (static argument 1): a dynamic constant's direct
/// `MethodHandle` value when one was resolved there (wave 42), else the
/// constant-pool entry. `None` when neither reads as a direct member.
pub(in crate::runtime::invokedynamic) fn lambda_impl_handle_arg(
    cp: &ConstantPool,
    args: &[u16],
    condy: &[Option<LambdaCondyArg>],
) -> Option<MethodHandle> {
    match condy.get(1) {
        Some(Some(arg)) => arg.method_handle.clone(),
        _ => resolve_method_handle_full(cp, *args.get(1)?).ok(),
    }
}

impl LambdaStaticArg {
    pub(in crate::runtime::invokedynamic) fn of(cp: &ConstantPool, index: u16) -> Self {
        match cp.get(index) {
            Some(ConstantPoolEntry::MethodType { .. }) => Self::MethodType,
            Some(ConstantPoolEntry::MethodHandle { .. }) => Self::MethodHandle,
            Some(ConstantPoolEntry::Integer(_)) => Self::Integer,
            Some(ConstantPoolEntry::ClassReference { .. }) => Self::Class,
            Some(ConstantPoolEntry::StringReference { .. }) => Self::Other("java.lang.String"),
            Some(ConstantPoolEntry::Long(_)) => Self::Other("java.lang.Long"),
            Some(ConstantPoolEntry::Float(_)) => Self::Other("java.lang.Float"),
            Some(ConstantPoolEntry::Double(_)) => Self::Other("java.lang.Double"),
            _ => Self::Undecided,
        }
    }

    /// The class a `ClassCastException` names for this constant. `None` for a
    /// `MethodHandle` constant, whose class is an implementation class of the
    /// JDK's choosing (`DirectMethodHandle`, `DirectMethodHandle$Special`, ...).
    pub(in crate::runtime::invokedynamic) fn java_class(self) -> Option<&'static str> {
        match self {
            Self::MethodType => Some("java.lang.invoke.MethodType"),
            Self::Integer => Some("java.lang.Integer"),
            Self::Class => Some("java.lang.Class"),
            Self::Other(name) => Some(name),
            Self::MethodHandle | Self::Undecided | Self::Null | Self::Foreign => None,
        }
    }
}

/// `BootstrapMethodInvoker` calling `metafactory` with these static
/// arguments. A count other than three goes through the generic `invoke`,
/// whose call type has one `Object` per static argument (all `Object`, the
/// lookup, name and type included, beyond six, where it is
/// `invokeWithArguments`), so it is a `WrongMethodTypeException`; three are
/// cast in order to `MethodType`, `MethodHandle`, `MethodType`
/// (`ClassCastException`). Fewer than three and a wrong kind in the three
/// were an internal error of the native linkage; more than three linked,
/// ignoring the rest, which `--compatible` kept until wave 47 (now refused
/// in every mode). A `MethodHandle` constant
/// where a `MethodType` belongs names its implementation class
/// ([`LambdaTypes::method_handle_constant_class`], wave 38).
pub(in crate::runtime::invokedynamic) fn metafactory_static_args(
    types: &LambdaTypes<'_>,
    cp: &ConstantPool,
    args: &[u16],
    kinds: &[LambdaStaticArg],
    condy: &[Option<LambdaCondyArg>],
) -> Result<(), LambdaStop> {
    let count = kinds.len();
    if count != 3 {
        let call = if count <= 6 {
            format!(
                "(Lookup,String,MethodType{})Object",
                ",Object".repeat(count)
            )
        } else {
            format!("({})Object", vec!["Object"; count + 3].join(","))
        };
        return Err(LambdaStop::Refuse(LambdaSiteRefusal {
            cause_class: "java/lang/invoke/WrongMethodTypeException",
            message: format!(
                "cannot convert MethodHandle(Lookup,String,MethodType,MethodType,MethodHandle,\
                 MethodType)CallSite to {call}"
            ),
            // Every mode since wave 47 (more than three linked under
            // `--compatible`, ignoring the rest).
            every_mode: true,
        }));
    }
    let expected = [
        (LambdaStaticArg::MethodType, "java.lang.invoke.MethodType"),
        (
            LambdaStaticArg::MethodHandle,
            "java.lang.invoke.MethodHandle",
        ),
        (LambdaStaticArg::MethodType, "java.lang.invoke.MethodType"),
    ];
    for (index, (&kind, (want, want_name))) in kinds.iter().zip(expected).enumerate() {
        // A `null` (a dynamic constant's value, wave 41) passes the cast.
        if kind == want || kind == LambdaStaticArg::Null {
            continue;
        }
        // A value of a class outside the named ones: HotSpot's own message,
        // computed when the value was resolved (wave 46).
        if kind == LambdaStaticArg::Foreign {
            let slot = usize::from(want == LambdaStaticArg::MethodHandle);
            let message = condy
                .get(index)
                .and_then(Option::as_ref)
                .and_then(|arg| arg.cast_messages.as_ref())
                .map(|messages| messages[slot].clone());
            return Err(LambdaStop::Refuse(LambdaSiteRefusal {
                cause_class: "java/lang/ClassCastException",
                message: lambda_known(message, "a dynamic constant's class")?,
                every_mode: true,
            }));
        }
        let actual = match kind {
            LambdaStaticArg::MethodHandle => {
                let handle = args
                    .get(index)
                    .and_then(|&i| resolve_method_handle_full(cp, i).ok());
                handle.and_then(|h| types.method_handle_constant_class(&h))
            }
            _ => kind.java_class(),
        };
        let actual = lambda_known(actual, "a MethodHandle constant's class")?;
        return Err(LambdaStop::Refuse(LambdaSiteRefusal {
            cause_class: "java/lang/ClassCastException",
            message: format!(
                "class {actual} cannot be cast to class {want_name} ({actual} and {want_name} \
                 are in module java.base of loader 'bootstrap')"
            ),
            every_mode: true,
        }));
    }
    // `metafactory`'s `Objects.requireNonNull` of each (measured on HotSpot
    // 25: a `NullPointerException` with no message; wave 41).
    if kinds.contains(&LambdaStaticArg::Null) {
        return Err(LambdaStop::Refuse(lambda_null_argument_refusal(true)));
    }
    Ok(())
}

/// The refusal of a `null` static argument (a dynamic constant's value):
/// `Objects.requireNonNull`'s `NullPointerException`, whose message is `null`
/// (an empty `message` is raised as a null one; see
/// [`bootstrap_method_error_with_new_cause_opt`]). Interpreter round i1 wave
/// 41, lane L4.
pub(in crate::runtime::invokedynamic) fn lambda_null_argument_refusal(every_mode: bool) -> LambdaSiteRefusal {
    LambdaSiteRefusal {
        cause_class: "java/lang/NullPointerException",
        message: String::new(),
        every_mode,
    }
}

/// The marker interfaces (internal names) and bridge descriptors an
/// `altMetafactory` site names.
#[derive(Default)]
pub(in crate::runtime::invokedynamic) struct AltMetafactoryTail {
    pub(in crate::runtime::invokedynamic) markers: Vec<String>,
    pub(in crate::runtime::invokedynamic) bridges: Vec<String>,
}

/// `altMetafactory`'s own reading of its static arguments (`extractArg`): a
/// missing one, one of the wrong class, a negative count or one left over is
/// an `IllegalArgumentException` with the JDK's message. A defect among the
/// first three is a shape the native linkage could not build (an internal
/// error before wave 37); the others used to link under `--compatible` until
/// wave 47, which refuses them in every mode (both modes are held to HotSpot;
/// these read static arguments only and load nothing).
pub(in crate::runtime::invokedynamic) fn alt_metafactory_static_args(
    cp: &ConstantPool,
    args: &[u16],
    kinds: &[LambdaStaticArg],
    condy: &[Option<LambdaCondyArg>],
) -> Result<AltMetafactoryTail, LambdaStop> {
    let refuse = |message: &str, _index: usize| {
        LambdaStop::Refuse(LambdaSiteRefusal {
            cause_class: "java/lang/IllegalArgumentException",
            message: message.to_string(),
            every_mode: true,
        })
    };
    let expect = |index: usize, want: LambdaStaticArg| match kinds.get(index) {
        None => Err(refuse("missing argument", index)),
        Some(&kind) if kind == want => Ok(()),
        // `extractArg`'s `Objects.requireNonNull` (wave 41).
        Some(LambdaStaticArg::Null) => Err(LambdaStop::Refuse(lambda_null_argument_refusal(true))),
        Some(_) => Err(refuse("argument has wrong type", index)),
    };
    let int_at = |index: usize| -> Result<i32, LambdaStop> {
        expect(index, LambdaStaticArg::Integer)?;
        // A dynamic constant's `Integer` value is read before the pool
        // (wave 47).
        lambda_known(
            lambda_int_arg(cp, args, condy, index),
            "an altMetafactory int",
        )
    };
    // A count no class file can satisfy is left alone: `extractArgs` sizes
    // its array first, and a huge one is an `OutOfMemoryError`, not the
    // "missing argument" a short tail would be.
    let count_at = |index: usize| -> Result<usize, LambdaStop> {
        let count = int_at(index)?;
        if count < 0 {
            return Err(refuse("negative argument count", index));
        }
        let count = count as usize;
        if count > usize::from(u16::MAX) {
            return Err(LambdaStop::Unknown("an altMetafactory count"));
        }
        Ok(count)
    };
    expect(0, LambdaStaticArg::MethodType)?;
    expect(1, LambdaStaticArg::MethodHandle)?;
    expect(2, LambdaStaticArg::MethodType)?;
    let flags = int_at(3)?;
    let mut next = 4;
    let mut tail = AltMetafactoryTail::default();
    if flags & FLAG_MARKERS != 0 {
        let count = count_at(next)?;
        next += 1;
        for _ in 0..count {
            expect(next, LambdaStaticArg::Class)?;
            let name = lambda_class_arg(cp, args, condy, next);
            tail.markers
                .push(lambda_known(name, "a marker interface")?.to_string());
            next += 1;
        }
    }
    if flags & FLAG_BRIDGES != 0 {
        let count = count_at(next)?;
        next += 1;
        for _ in 0..count {
            expect(next, LambdaStaticArg::MethodType)?;
            let bridge = lambda_method_type_arg(cp, args, condy, next);
            tail.bridges
                .push(lambda_known(bridge, "a bridge signature")?);
            next += 1;
        }
    }
    if next < kinds.len() {
        return Err(refuse("too many arguments", next));
    }
    Ok(tail)
}

/// `Class.getName()` of the type a field descriptor names; it needs no loaded
/// class (`java.lang.String`, `[Ljava.lang.String;`, `int`).
pub(crate) fn descriptor_class_name(desc: &str) -> String {
    match desc.as_bytes().first() {
        Some(b'L') => desc[1..].trim_end_matches(';').replace('/', "."),
        Some(b'[') => desc.replace('/', "."),
        _ => primitive_type_name(desc).unwrap_or(desc).to_string(),
    }
}

/// The Java name of a primitive (or `void`) descriptor.
pub(in crate::runtime::invokedynamic) fn primitive_type_name(desc: &str) -> Option<&'static str> {
    Some(match desc {
        "B" => "byte",
        "C" => "char",
        "D" => "double",
        "F" => "float",
        "I" => "int",
        "J" => "long",
        "S" => "short",
        "Z" => "boolean",
        "V" => "void",
        _ => return None,
    })
}

/// `sun.invoke.util.Wrapper.isConvertibleFrom` between two primitives: the
/// same type, or a widening primitive conversion (JLS 5.1.2). `boolean` and
/// `void` convert only to themselves.
pub(in crate::runtime::invokedynamic) fn primitive_widens(from: &str, to: &str) -> bool {
    from == to
        || match from {
            "B" => matches!(to, "S" | "I" | "J" | "F" | "D"),
            "S" | "C" => matches!(to, "I" | "J" | "F" | "D"),
            "I" => matches!(to, "J" | "F" | "D"),
            "J" => matches!(to, "F" | "D"),
            "F" => to == "D",
            _ => false,
        }
}

/// The wrapper class descriptor of a primitive (`Wrapper.wrapperType`).
pub(in crate::runtime::invokedynamic) fn primitive_wrapper(prim: &str) -> Option<&'static str> {
    Some(match prim {
        "B" => "Ljava/lang/Byte;",
        "C" => "Ljava/lang/Character;",
        "D" => "Ljava/lang/Double;",
        "F" => "Ljava/lang/Float;",
        "I" => "Ljava/lang/Integer;",
        "J" => "Ljava/lang/Long;",
        "S" => "Ljava/lang/Short;",
        "Z" => "Ljava/lang/Boolean;",
        "V" => "Ljava/lang/Void;",
        _ => return None,
    })
}

/// The primitive a wrapper class descriptor unboxes to. A `java.lang`
/// wrapper name is decisive: no loader but the bootstrap one defines it.
pub(in crate::runtime::invokedynamic) fn wrapper_primitive(desc: &str) -> Option<&'static str> {
    Some(match desc {
        "Ljava/lang/Byte;" => "B",
        "Ljava/lang/Character;" => "C",
        "Ljava/lang/Double;" => "D",
        "Ljava/lang/Float;" => "F",
        "Ljava/lang/Integer;" => "I",
        "Ljava/lang/Long;" => "J",
        "Ljava/lang/Short;" => "S",
        "Ljava/lang/Boolean;" => "Z",
        "Ljava/lang/Void;" => "V",
        _ => return None,
    })
}

/// `java.lang.Class` questions about descriptor types, as the caller of a
/// lambda site sees them (its defining loader). Every answer is `None` when
/// the class store cannot give `Class`'s own answer.
pub(in crate::runtime::invokedynamic) struct LambdaTypes<'a> {
    pub(in crate::runtime::invokedynamic) cm: &'a crate::classloading::ClassManager,
    pub(in crate::runtime::invokedynamic) caller: ClassId,
    /// Classes the caller's loader answered for a name the class store cannot
    /// place in the caller's own namespace (see [`lambda_site_load_types`]).
    pub(in crate::runtime::invokedynamic) resolved: &'a std::collections::HashMap<String, ClassId>,
}

impl LambdaTypes<'_> {
    pub(in crate::runtime::invokedynamic) fn class_named(&self, name: &str) -> Option<ClassId> {
        self.cm
            .find_class_by_name_for_class(name, self.caller)
            .or_else(|| self.resolved.get(name).copied())
    }

    pub(in crate::runtime::invokedynamic) fn class_of(&self, desc: &str) -> Option<ClassId> {
        self.class_named(desc.strip_prefix('L')?.strip_suffix(';')?)
    }

    /// `Class.isInterface()` of a class named by its internal name (an array
    /// class is not one).
    pub(in crate::runtime::invokedynamic) fn is_interface(&self, name: &str) -> Option<bool> {
        if name.starts_with('[') {
            return Some(false);
        }
        Some(self.cm.get_class(self.class_named(name)?)?.is_interface())
    }

    /// `Class.toString()` of the type `desc` names.
    pub(in crate::runtime::invokedynamic) fn display(&self, desc: &str) -> Option<String> {
        match desc.as_bytes().first()? {
            b'L' => {
                let class = self.cm.get_class(self.class_of(desc)?)?;
                let kind = if class.is_interface() {
                    "interface"
                } else {
                    "class"
                };
                Some(format!("{kind} {}", descriptor_class_name(desc)))
            }
            b'[' => Some(format!("class {}", descriptor_class_name(desc))),
            _ => primitive_type_name(desc).map(str::to_string),
        }
    }

    /// Every class above `id` is in the store with real bytes, so a `false`
    /// from `is_subclass_of` is `Class.isAssignableFrom`'s own answer.
    pub(in crate::runtime::invokedynamic) fn hierarchy_is_decisive(&self, id: ClassId) -> bool {
        let store = &self.cm.class_store;
        let mut stack = vec![id];
        let mut seen: Vec<ClassId> = Vec::new();
        while let Some(id) = stack.pop() {
            if seen.contains(&id) {
                continue;
            }
            if seen.len() >= 512 {
                return false;
            }
            seen.push(id);
            let Some(class) = store.get(id) else {
                return false;
            };
            if !class.origin.has_real_bytes() {
                return false;
            }
            stack.extend(class.superclass);
            stack.extend_from_slice(&class.interfaces);
        }
        true
    }

    /// `to.isAssignableFrom(from)`, both descriptors.
    pub(in crate::runtime::invokedynamic) fn is_assignable(&self, to: &str, from: &str) -> Option<bool> {
        let reference = |d: &str| matches!(d.as_bytes().first(), Some(b'L' | b'['));
        if !reference(to) || !reference(from) {
            return Some(to == from);
        }
        if to == from || to == "Ljava/lang/Object;" {
            return Some(true);
        }
        match (from.starts_with('['), to.starts_with('[')) {
            (true, true) => self.is_assignable(&to[1..], &from[1..]),
            (true, false) => Some(matches!(
                to,
                "Ljava/lang/Cloneable;" | "Ljava/io/Serializable;"
            )),
            (false, true) => Some(false),
            (false, false) => {
                let from_id = self.class_of(from)?;
                let to_id = self.class_of(to)?;
                if self.cm.is_subclass_of(from_id, to_id) {
                    return Some(true);
                }
                // A same-named class elsewhere in the hierarchy (another
                // loader's copy) is a loader question this does not settle.
                let to_name = &to[1..to.len() - 1];
                (self.hierarchy_is_decisive(from_id)
                    && !self.cm.is_assignable_to_name(from_id, to_name))
                .then_some(false)
            }
        }
    }

    /// `AbstractValidatingLambdaMetafactory.isAdaptableTo(from, to, strict)`.
    pub(in crate::runtime::invokedynamic) fn adaptable(&self, from: &str, to: &str, strict: bool) -> Option<bool> {
        if from == to {
            return Some(true);
        }
        match (primitive_type_name(from), primitive_type_name(to)) {
            (Some(_), Some(_)) => Some(primitive_widens(from, to)),
            (Some(_), None) => self.is_assignable(to, primitive_wrapper(from)?),
            (None, Some(_)) => Some(match wrapper_primitive(from) {
                Some(unboxed) => primitive_widens(unboxed, to),
                None => !strict,
            }),
            (None, None) => {
                if strict {
                    self.is_assignable(to, from)
                } else {
                    Some(true)
                }
            }
        }
    }

    /// The class `revealDirect` reports as declaring the implementation
    /// member: the resolved method's class (`Base` for a handle naming
    /// `Impl.m` that `Impl` inherits), the named class for a constructor.
    pub(in crate::runtime::invokedynamic) fn declaring_class(&self, handle: &MethodHandle) -> Option<String> {
        if matches!(handle.kind, MethodHandleKind::NewInvokeSpecial) {
            return Some(handle.class_name.to_string());
        }
        let owner = self.class_named(&handle.class_name)?;
        let declaring = crate::runtime::resolve::selection::resolve_declaring(
            &self.cm.class_store,
            owner,
            &handle.member_name,
            &handle.descriptor,
        )?;
        Some(self.cm.get_class(declaring)?.name.to_string())
    }

    /// The class that declares the field a field handle names (JVMS
    /// §5.4.3.2 from the named class), `None` when it is not loaded or has no
    /// such field.
    pub(in crate::runtime::invokedynamic) fn field_declaring_class(&self, handle: &MethodHandle) -> Option<ClassId> {
        let owner = self.class_named(&handle.class_name)?;
        crate::classloading::find_field_recursive_by_descriptor(
            owner,
            &handle.member_name,
            &handle.descriptor,
            &self.cm.class_store,
        )
        .map(|(_, _, declaring)| declaring)
    }

    /// `MethodHandleInfo.toString()` of the implementation handle:
    /// `getStatic p.C.f:()String` for a field handle, whose type is the
    /// accessor's (`()T` to read, `(T)void` to write; wave 38, measured).
    pub(in crate::runtime::invokedynamic) fn impl_info(&self, handle: &MethodHandle) -> Option<String> {
        let (kind, field_type) = match handle.kind {
            MethodHandleKind::InvokeVirtual => ("invokeVirtual", None),
            MethodHandleKind::InvokeStatic => ("invokeStatic", None),
            MethodHandleKind::InvokeSpecial => ("invokeSpecial", None),
            MethodHandleKind::NewInvokeSpecial => ("newInvokeSpecial", None),
            MethodHandleKind::InvokeInterface => ("invokeInterface", None),
            MethodHandleKind::GetField => ("getField", Some(format!("(){}", handle.descriptor))),
            MethodHandleKind::GetStatic => ("getStatic", Some(format!("(){}", handle.descriptor))),
            MethodHandleKind::PutField => ("putField", Some(format!("({})V", handle.descriptor))),
            MethodHandleKind::PutStatic => ("putStatic", Some(format!("({})V", handle.descriptor))),
        };
        let (declaring, method_type) = match field_type {
            Some(accessor_type) => {
                let declaring = self.field_declaring_class(handle)?;
                (
                    self.cm.get_class(declaring)?.name.to_string(),
                    accessor_type,
                )
            }
            None => (self.declaring_class(handle)?, handle.descriptor.to_string()),
        };
        Some(format!(
            "{kind} {}.{}:{}",
            declaring.replace('/', "."),
            handle.member_name,
            method_type_display_for(self.cm, self.caller, &method_type),
        ))
    }

    /// The class of the `MethodHandle` object HotSpot 25 resolves a
    /// `CONSTANT_MethodHandle` to (`DirectMethodHandle.make` /
    /// `makeAllocator`), which a `ClassCastException` names when the constant
    /// sits where a `MethodType` belongs. Measured per reference kind
    /// (interpreter round i1 wave 38, lane L4): a field reader or writer is
    /// `DirectMethodHandle$Accessor` (`$StaticAccessor` for a static field), a
    /// constructor `$Constructor`, an `invokeInterface` `$Interface` (a default
    /// method too), an `invokeStatic` or `invokeVirtual` `DirectMethodHandle`.
    ///
    /// `None` wherever resolving the constant could fail first (the member is
    /// not found in loaded classes, or it or its class is not public: HotSpot
    /// resolves every static argument before `BootstrapMethodInvoker` casts),
    /// for `invokeSpecial` (its access and receiver rules), and for a JDK
    /// class's method, which may be caller-sensitive
    /// (`MethodHandleImpl$WrappedMember`, measured for `Class.forName`).
    pub(in crate::runtime::invokedynamic) fn method_handle_constant_class(&self, handle: &MethodHandle) -> Option<&'static str> {
        use cratonvm_reader::class_access_flags::{FieldAccessFlags, MethodAccessFlags};
        let owner = self.class_named(&handle.class_name)?;
        if !self.cm.get_class(owner)?.is_public() {
            return None;
        }
        let public_field = |declaring: ClassId| {
            let class = self.cm.get_class(declaring)?;
            let field = class
                .fields
                .iter()
                .find(|f| *f.name == *handle.member_name && *f.descriptor == *handle.descriptor)?;
            field
                .access_flags
                .contains(FieldAccessFlags::PUBLIC)
                .then_some(())
        };
        let public_method = |declaring: ClassId| {
            let method = self
                .cm
                .get_class(declaring)?
                .find_method(&handle.member_name, &handle.descriptor)?;
            method
                .access_flags
                .contains(MethodAccessFlags::PUBLIC)
                .then_some(())
        };
        let jdk_owner = ["java/", "jdk/", "sun/", "com/sun/"]
            .iter()
            .any(|prefix| handle.class_name.starts_with(prefix));
        match handle.kind {
            MethodHandleKind::GetField | MethodHandleKind::PutField => {
                public_field(self.field_declaring_class(handle)?)?;
                Some("java.lang.invoke.DirectMethodHandle$Accessor")
            }
            MethodHandleKind::GetStatic | MethodHandleKind::PutStatic => {
                public_field(self.field_declaring_class(handle)?)?;
                Some("java.lang.invoke.DirectMethodHandle$StaticAccessor")
            }
            MethodHandleKind::NewInvokeSpecial => {
                public_method(owner)?;
                Some("java.lang.invoke.DirectMethodHandle$Constructor")
            }
            MethodHandleKind::InvokeSpecial => None,
            MethodHandleKind::InvokeInterface
            | MethodHandleKind::InvokeStatic
            | MethodHandleKind::InvokeVirtual => {
                if jdk_owner {
                    return None;
                }
                let declaring = crate::runtime::resolve::selection::resolve_declaring(
                    &self.cm.class_store,
                    owner,
                    &handle.member_name,
                    &handle.descriptor,
                )?;
                public_method(declaring)?;
                Some(
                    if matches!(handle.kind, MethodHandleKind::InvokeInterface) {
                        "java.lang.invoke.DirectMethodHandle$Interface"
                    } else {
                        "java.lang.invoke.DirectMethodHandle"
                    },
                )
            }
        }
    }
}

/// `AbstractValidatingLambdaMetafactory`'s constructor checks and
/// `validateMetafactoryArgs`, in their order, with their messages. Every
/// check reads only the constant pool and loaded classes; one it cannot
/// decide stops the walk (`LambdaStop::Unknown`).
pub(in crate::runtime::invokedynamic) fn lambda_conversion_checks(
    types: &LambdaTypes<'_>,
    cp: &ConstantPool,
    info: &IndyInfo,
    tail: &AltMetafactoryTail,
    condy: &[Option<LambdaCondyArg>],
) -> Result<(), LambdaStop> {
    use crate::runtime::interpreter::split_method_descriptor_ref as split;
    let args = &info.bootstrap_arg_indices;
    let arg = |i: usize| lambda_known(args.get(i).copied(), "a static argument");
    let interface_mt = lambda_known(
        lambda_method_type_arg(cp, args, condy, 0),
        "the interface type",
    )?;
    let _ = arg(1)?;
    let handle = lambda_known(
        lambda_impl_handle_arg(cp, args, condy),
        "the implementation handle",
    )?;
    let dynamic_mt = lambda_known(
        lambda_method_type_arg(cp, args, condy, 2),
        "the dynamic type",
    )?;

    // The constructor.
    let instance = match handle.kind {
        MethodHandleKind::InvokeVirtual
        | MethodHandleKind::InvokeInterface
        | MethodHandleKind::InvokeSpecial => true,
        MethodHandleKind::InvokeStatic | MethodHandleKind::NewInvokeSpecial => false,
        // The constructor's `default` arm, before any other check (wave 38,
        // measured: `Unsupported MethodHandle kind: getStatic p.C.f:()String`).
        MethodHandleKind::GetField
        | MethodHandleKind::GetStatic
        | MethodHandleKind::PutField
        | MethodHandleKind::PutStatic => {
            let info_text =
                lambda_known(types.impl_info(&handle), "a field implementation handle")?;
            return Err(lambda_conversion_refusal(format!(
                "Unsupported MethodHandle kind: {info_text}"
            )));
        }
    };
    if matches!(
        &*handle.class_name,
        "java/lang/invoke/MethodHandle" | "java/lang/invoke/VarHandle"
    ) {
        return Err(LambdaStop::Unknown(
            "a signature-polymorphic implementation",
        ));
    }
    let name = info.target_name.as_str();
    if name.is_empty()
        || name
            .chars()
            .any(|c| matches!(c, '.' | ';' | '[' | '/' | '<' | '>'))
    {
        return Err(lambda_conversion_refusal(format!(
            "Method name '{name}' is not legal"
        )));
    }
    let (factory_params, factory_ret) = split(&info.target_descriptor);
    let interface_ok = match factory_ret
        .strip_prefix('L')
        .and_then(|n| n.strip_suffix(';'))
    {
        Some(iface) => lambda_known(types.is_interface(iface), "the functional interface")?,
        None => false,
    };
    if !interface_ok {
        return Err(lambda_conversion_refusal(format!(
            "{} is not an interface",
            descriptor_class_name(factory_ret)
        )));
    }
    for marker in &tail.markers {
        if !lambda_known(types.is_interface(marker), "a marker interface")? {
            return Err(lambda_conversion_refusal(format!(
                "{} is not an interface",
                marker.replace('/', ".")
            )));
        }
    }

    // `validateMetafactoryArgs`: the arities.
    let owner_desc = format!("L{};", handle.class_name);
    let (impl_params, impl_declared_ret) = split(&handle.descriptor);
    // `implementation.type()`: an instance method's receiver leads (index 0 is
    // never compared below, only the receiver check reads the class), and a
    // constructor returns its class.
    let mut impl_type: Vec<&str> = Vec::with_capacity(impl_params.len() + 1);
    if instance {
        impl_type.push(owner_desc.as_str());
    }
    impl_type.extend(impl_params.iter().copied());
    let impl_ret: &str = if matches!(handle.kind, MethodHandleKind::NewInvokeSpecial) {
        owner_desc.as_str()
    } else {
        impl_declared_ret
    };
    let (interface_params, _) = split(&interface_mt);
    let (dynamic_params, dynamic_ret) = split(&dynamic_mt);
    let impl_arity = impl_type.len();
    let captured = factory_params.len();
    let sam = interface_params.len();
    let dynamic = dynamic_params.len();
    let kind_word = if instance { "instance" } else { "static" };
    if impl_arity != captured + sam {
        let info_text = lambda_known(
            types.impl_info(&handle),
            "the implementation's declaring class",
        )?;
        return Err(lambda_conversion_refusal(format!(
            "Incorrect number of parameters for {kind_word} method {info_text}; {captured} \
             captured parameters, {sam} functional interface method parameters, {impl_arity} \
             implementation parameters"
        )));
    }
    if dynamic != sam {
        let info_text = lambda_known(
            types.impl_info(&handle),
            "the implementation's declaring class",
        )?;
        return Err(lambda_conversion_refusal(format!(
            "Incorrect number of parameters for {kind_word} method {info_text}; {dynamic} \
             dynamic parameters, {sam} functional interface method parameters"
        )));
    }
    for bridge in &tail.bridges {
        if split(bridge).0.len() != sam {
            return Err(lambda_conversion_refusal(format!(
                "Incorrect number of parameters for bridge signature {}; incompatible with {}",
                method_type_display_for(types.cm, types.caller, bridge),
                method_type_display_for(types.cm, types.caller, &interface_mt)
            )));
        }
    }

    // The receiver, the captures, the SAM arguments and the return.
    let display = |desc: &str| lambda_known(types.display(desc), "a type's class");
    let (captured_start, sam_start) = if instance {
        let (captured_start, sam_start, receiver) = if captured == 0 {
            (0, 1, dynamic_params.first())
        } else {
            (1, captured, factory_params.first())
        };
        let receiver = *lambda_known(receiver, "the receiver")?;
        let impl_class = if matches!(handle.kind, MethodHandleKind::InvokeSpecial) {
            let declaring = lambda_known(types.declaring_class(&handle), "the declaring class")?;
            format!("L{declaring};")
        } else {
            owner_desc.clone()
        };
        if !lambda_known(
            types.is_assignable(&impl_class, receiver),
            "the receiver type",
        )? {
            return Err(lambda_conversion_refusal(format!(
                "Invalid receiver type {}; not a subtype of implementation type {}",
                display(receiver)?,
                display(&impl_class)?
            )));
        }
        (captured_start, sam_start)
    } else {
        (0, captured)
    };
    for i in captured_start..captured {
        let impl_param = *lambda_known(impl_type.get(i), "a captured parameter")?;
        let captured_param = *lambda_known(factory_params.get(i), "a captured parameter")?;
        if impl_param != captured_param {
            return Err(lambda_conversion_refusal(format!(
                "Type mismatch in captured lambda parameter {i}: expecting {}, found {}",
                display(captured_param)?,
                display(impl_param)?
            )));
        }
    }
    for i in sam_start..impl_arity {
        let impl_param = *lambda_known(impl_type.get(i), "a lambda parameter")?;
        let dynamic_param = *lambda_known(dynamic_params.get(i - captured), "a lambda parameter")?;
        if !lambda_known(
            types.adaptable(dynamic_param, impl_param, true),
            "a lambda argument",
        )? {
            return Err(lambda_conversion_refusal(format!(
                "Type mismatch for lambda argument {i}: {} is not convertible to {}",
                display(dynamic_param)?,
                display(impl_param)?
            )));
        }
    }
    let return_ok = dynamic_ret == "V"
        || (impl_ret != "V"
            && lambda_known(
                types.adaptable(impl_ret, dynamic_ret, false),
                "the lambda return",
            )?);
    if !return_ok {
        return Err(lambda_conversion_refusal(format!(
            "Type mismatch for lambda return: {} is not convertible to {}",
            display(impl_ret)?,
            display(dynamic_ret)?
        )));
    }

    // `checkDescriptor`, for the interface type and then each bridge.
    for descriptor in
        std::iter::once(interface_mt.as_str()).chain(tail.bridges.iter().map(String::as_str))
    {
        let (params, ret) = split(descriptor);
        for (i, &dynamic_param) in dynamic_params.iter().enumerate() {
            let param = *lambda_known(params.get(i), "a dynamic parameter")?;
            if !lambda_known(
                types.is_assignable(param, dynamic_param),
                "a dynamic parameter",
            )? {
                return Err(lambda_conversion_refusal(format!(
                    "Type mismatch for dynamic parameter {i}: {} is not a subtype of {}",
                    display(dynamic_param)?,
                    display(param)?
                )));
            }
        }
        let ret_ok = if dynamic_ret == "V" || ret == "V" {
            dynamic_ret == ret
        } else {
            lambda_known(
                types.adaptable(dynamic_ret, ret, true),
                "the expected return",
            )?
        };
        if !ret_ok {
            return Err(lambda_conversion_refusal(format!(
                "Type mismatch for lambda expected return: {} is not convertible to {}",
                display(dynamic_ret)?,
                display(ret)?
            )));
        }
    }
    Ok(())
}

/// Does this `LambdaMetafactory` site link on HotSpot? The whole of item 2
/// of `i29-L4-native-indy-linkage-skips-the-jdks-validation`: first the
/// static arguments as `BootstrapMethodInvoker` passes them
/// ([`metafactory_static_args`], [`alt_metafactory_static_args`]), then,
/// under `--real-jdk`, the factory's own checks
/// ([`lambda_conversion_checks`]).
/// Does this `LambdaMetafactory` site have a shape javac emits, read from the
/// constant pool alone? `metafactory` (its own descriptor) with exactly
/// `(MethodType, MethodHandle, MethodType)`, the handle a method or
/// constructor reference (kinds 5 to 9); `altMetafactory` with the same three,
/// then an `Integer` flags word of the three known bits, a marker block
/// (`Integer` count, that many classes) when `FLAG_MARKERS` is set, a bridge
/// block (`Integer` count, that many method types) when `FLAG_BRIDGES` is,
/// and nothing after. No dynamic constant anywhere. Interpreter round i1
/// wave 48, lane L4
/// (`i38-L4-proposal-native-bootstraps-link-only-javac-shapes-natively`,
/// step 1 for `LambdaMetafactory`): the JIT bridge's admission test for a
/// lambda site. A site it refuses keeps its trap and links interpreted,
/// where an undecidable one may take the JDK's own bootstrap
/// (`bootstrap_generic_jdk`), the generic route the bridge does not admit.
pub(in crate::runtime::invokedynamic) fn lambda_site_is_javac_shape(
    cp: &ConstantPool,
    bsm_method: &str,
    bsm_descriptor: &str,
    args: &[u16],
) -> bool {
    let kind = |i: usize| args.get(i).map(|&index| LambdaStaticArg::of(cp, index));
    let int_at = |i: usize| match args.get(i).and_then(|&index| cp.get(index)) {
        Some(ConstantPoolEntry::Integer(v)) => Some(*v),
        _ => None,
    };
    let direct_handle = matches!(
        args.get(1).and_then(|&index| cp.get(index)),
        Some(ConstantPoolEntry::MethodHandle {
            reference_kind: 5..=9,
            ..
        })
    );
    if kind(0) != Some(LambdaStaticArg::MethodType)
        || !direct_handle
        || kind(2) != Some(LambdaStaticArg::MethodType)
    {
        return false;
    }
    match (bsm_method, bsm_descriptor) {
        (METAFACTORY, METAFACTORY_DESCRIPTOR) => args.len() == 3,
        (ALT_METAFACTORY, ALT_METAFACTORY_DESCRIPTOR) => {
            let Some(flags) = int_at(3) else {
                return false;
            };
            if flags & !(FLAG_SERIALIZABLE | FLAG_MARKERS | FLAG_BRIDGES) != 0 {
                return false;
            }
            let mut at = 4usize;
            for (bit, element) in [
                (FLAG_MARKERS, LambdaStaticArg::Class),
                (FLAG_BRIDGES, LambdaStaticArg::MethodType),
            ] {
                if flags & bit == 0 {
                    continue;
                }
                let Some(count) = int_at(at).and_then(|c| usize::try_from(c).ok()) else {
                    return false;
                };
                at += 1;
                if (at..at + count).any(|i| kind(i) != Some(element)) {
                    return false;
                }
                at += count;
            }
            at == args.len()
        }
        _ => false,
    }
}

pub(in crate::runtime::invokedynamic) fn lambda_site_verdict(
    cm: &crate::classloading::ClassManager,
    class_id: ClassId,
    info: &IndyInfo,
    jdk_only: bool,
    condy: &[Option<LambdaCondyArg>],
    resolved: &std::collections::HashMap<String, ClassId>,
) -> Result<(), LambdaStop> {
    let class = lambda_known(cm.get_class(class_id), "the caller")?;
    let cp = &class.constant_pool;
    let args = &info.bootstrap_arg_indices;
    // A dynamic constant counts as the value it resolved to (wave 41).
    let kinds: Vec<LambdaStaticArg> = args
        .iter()
        .enumerate()
        .map(|(position, &i)| match condy.get(position) {
            Some(Some(arg)) => arg.kind,
            _ => LambdaStaticArg::of(cp, i),
        })
        .collect();
    if kinds.contains(&LambdaStaticArg::Undecided) {
        return Err(LambdaStop::Unknown("a dynamic static argument"));
    }
    let types = LambdaTypes {
        cm,
        caller: class_id,
        resolved,
    };
    let tail = if info.bsm_method == ALT_METAFACTORY {
        alt_metafactory_static_args(cp, args, &kinds, condy)?
    } else {
        metafactory_static_args(&types, cp, args, &kinds, condy)?;
        AltMetafactoryTail::default()
    };
    // `AbstractValidatingLambdaMetafactory`'s constructor cracks the
    // implementation handle (`caller.revealDirect`) before any other check.
    // A dynamic constant's handle that cannot be cracked is refused in every
    // mode: the native linkage cannot build such a site at all (it was the
    // uncatchable internal error `cp#N is not a MethodHandle`). Wave 43.
    if let Some(Some(LambdaCondyArg {
        not_direct: Some(shown),
        ..
    })) = condy.get(1)
    {
        return Err(LambdaStop::Refuse(LambdaSiteRefusal {
            cause_class: LAMBDA_CONVERSION_EXCEPTION,
            message: format!("{shown} is not direct or cannot be cracked"),
            every_mode: true,
        }));
    }
    // A dynamic constant's handle whose member this model cannot read back
    // (a special handle of a superclass method, a JDK `DirectMethodHandle`):
    // the native linkage cannot build the site in any mode (it was the
    // uncatchable `cp#N is not a MethodHandle`), so it is undecided in every
    // mode, which `indy_other_shapes_to_jdk` sends to the JDK's own
    // `LambdaMetafactory` (wave 48).
    if let Some(Some(LambdaCondyArg {
        kind: LambdaStaticArg::MethodHandle,
        method_handle: None,
        ..
    })) = condy.get(1)
    {
        return Err(LambdaStop::Unknown(
            "a MethodHandle value the linkage cannot read back",
        ));
    }
    if !jdk_only {
        return Ok(());
    }
    lambda_conversion_checks(&types, cp, info, &tail, condy)
}

/// The failure HotSpot raises linking this `LambdaMetafactory` site (the
/// cause of its `BootstrapMethodError`), or `None` when it links or when the
/// class store cannot decide. Under `--compatible` only the shapes the
/// native linkage cannot build at all are refused (`every_mode`).
///
/// `CRATONVM_DBG_LAMBDA_DISPATCH=1` prints one `[DBG_LAMBDA] link-check` line
/// per site: `refused`, `validated`, or the check left `undecided`.
///
/// javac emits none of the refused shapes; each is a hand-assembled site.
///
/// `--real-jdk` (interpreter round i1 wave 39, lane L4; row 1 of
/// `interpreter-L4-lambda-site-validation-leaves-undecidable-shapes-linked-FIXED-20261012`): when the
/// checks cannot decide, every class the call site's type and the site's
/// `MethodType` static arguments name is loaded through the caller's loader
/// ([`lambda_site_load_types`]), as HotSpot's resolution of those constants
/// loads them before the bootstrap runs, and the checks run once more. A
/// class that cannot be loaded is the `Err`: its `NoClassDefFoundError`.
pub(in crate::runtime::invokedynamic) fn lambda_site_refusal(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    info: &IndyInfo,
    jdk_only: bool,
    condy: &[Option<LambdaCondyArg>],
    undecided: &mut bool,
) -> Result<Option<LambdaSiteRefusal>, MethodCallFailed> {
    if !info.bsm_descriptor_is_lmf {
        return Ok(None);
    }
    let verdict_now = |resolved: &std::collections::HashMap<String, ClassId>| {
        let cm = shared.classes.class_manager.read();
        lambda_site_verdict(&cm, class_id, info, jdk_only, condy, resolved)
    };
    let mut verdict = verdict_now(&std::collections::HashMap::new());
    if jdk_only && matches!(verdict, Err(LambdaStop::Unknown(_))) {
        let (loaded, resolved) = lambda_site_load_types(shared, thread, class_id, info)?;
        if dbg_lambda_dispatch() {
            eprintln!(
                "[DBG_LAMBDA] link-check {}{}: loaded {loaded} type(s), checking again",
                info.target_name, info.target_descriptor
            );
        }
        // gci i4/jd: check again even when this thread loaded nothing. A class
        // the first verdict could not find may have been published by another
        // thread before `lambda_site_load_types` asked for it (it is then
        // "known" and skipped), and without this re-check the site stayed
        // undecided and took the JDK route only under concurrency: the
        // concurrent javac first round's `findPackageNameOf` / `accepts` sites
        // (vi3-jd `jd3_jt4r1_dbg_1`). A sequential run re-checks to the same
        // verdict, so only the undecided path pays one more verdict.
        verdict = verdict_now(&resolved);
    }
    let refusal = match verdict {
        Ok(()) => {
            if dbg_lambda_dispatch() {
                eprintln!(
                    "[DBG_LAMBDA] link-check {}{}: validated",
                    info.target_name, info.target_descriptor
                );
            }
            None
        }
        Err(LambdaStop::Unknown(what)) => {
            if dbg_lambda_dispatch() {
                eprintln!(
                    "[DBG_LAMBDA] link-check {}{}: undecided ({what})",
                    info.target_name, info.target_descriptor
                );
            }
            *undecided = true;
            None
        }
        Err(LambdaStop::Refuse(refusal)) => (jdk_only || refusal.every_mode).then_some(refusal),    };
    if let Some(refusal) = &refusal {
        if dbg_lambda_dispatch() {
            eprintln!(
                "[DBG_LAMBDA] link-check {}{}: refused {}: {}",
                info.target_name, info.target_descriptor, refusal.cause_class, refusal.message
            );
        }
    }
    Ok(refusal)
}

/// Load, through `class_id`'s loader, every class not yet loaded that the
/// call site's type and the `MethodType` static arguments of a lambda site
/// name (an array type's element class), in HotSpot's order: the call site's
/// type, then each static argument; within a type, the parameters, then the
/// return. Returns how many were loaded; a class that cannot be loaded is its
/// `NoClassDefFoundError` (JVMS 5.4.3.5: resolving a `MethodType` resolves
/// every class its descriptor names). Interpreter round i1 wave 39, lane L4.
pub(in crate::runtime::invokedynamic) fn lambda_site_load_types(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    info: &IndyInfo,
) -> Result<(usize, std::collections::HashMap<String, ClassId>), MethodCallFailed> {
    let descriptors: Vec<String> = {
        let cm = shared.classes.class_manager.read();
        let Some(class) = cm.get_class(class_id) else {
            return Ok((0, std::collections::HashMap::new()));
        };
        let cp = &class.constant_pool;
        // A `MethodHandle` static argument (the implementation) resolves its
        // class and then its member's type, which loads the classes the
        // member's descriptor names (wave 39 follow-up: the implementation's
        // parameter type was left unloaded, so the check stayed undecided
        // and the site linked). Each becomes a pseudo method descriptor so
        // one walk below serves every kind.
        let mut descriptors = vec![info.target_descriptor.clone()];
        for &i in &info.bootstrap_arg_indices {
            match cp.get(i) {
                Some(ConstantPoolEntry::MethodType { .. }) => {
                    descriptors.extend(resolve_method_type(cp, i));
                }
                Some(ConstantPoolEntry::MethodHandle { .. }) => {
                    let Ok(handle) = resolve_method_handle_full(cp, i) else {
                        continue;
                    };
                    if !handle.class_name.starts_with('[') {
                        descriptors.push(format!("(L{};)V", handle.class_name));
                    }
                    descriptors.push(if handle.descriptor.starts_with('(') {
                        handle.descriptor.to_string()
                    } else {
                        format!("(){}", handle.descriptor)
                    });
                }
                _ => {}
            }
        }
        descriptors
    };
    let mut loaded = 0usize;
    let mut resolved: std::collections::HashMap<String, ClassId> = std::collections::HashMap::new();
    for descriptor in &descriptors {
        let (params, ret) = crate::runtime::interpreter::split_method_descriptor_ref(descriptor);
        for token in params.iter().copied().chain(std::iter::once(ret)) {
            let Some(name) = token
                .trim_start_matches('[')
                .strip_prefix('L')
                .and_then(|t| t.strip_suffix(';'))
            else {
                continue;
            };
            let known = shared
                .classes
                .class_manager
                .read()
                .find_class_by_name_for_class(name, class_id)
                .is_some();
            if known {
                continue;
            }
            let answered = crate::runtime::interpreter::resolve_class_loader_aware(
                shared, thread, class_id, name,
            )
            .map_err(|e| {
                crate::runtime::exceptions::convert_class_not_found_for(
                    shared,
                    thread,
                    Some(class_id),
                    name,
                    e,
                )
            })?;
            // The caller's loader may have answered with a class another loader
            // defined (Spring Boot's `ModifiedClassPathClassLoader` hands every
            // `org.junit.` name to the application loader's copy), which the class
            // store cannot place in the caller's own namespace: keep the answer, so
            // the checks that follow decide on it rather than staying undecided
            // and sending the site to the JDK's own factory.
            resolved.insert(name.to_string(), answered);
            loaded += 1;
        }
    }
    Ok((loaded, resolved))
}

/// Resolve every `CONSTANT_Dynamic` static argument of a lambda site, in
/// order, as `ldc` of the same entry does (`constants::resolve_condy_constant`,
/// whose failure is recorded against the entry), and classify each value as
/// `BootstrapMethodInvoker`'s casts and `metafactory` see it. HotSpot resolves
/// every static argument before the bootstrap runs, so a failing condy is the
/// site's failure. Empty (no allocation) for a site with no dynamic constant,
/// i.e. every javac site. Interpreter round i1 wave 41, lane L4 (row 4 of
/// `interpreter-L4-lambda-site-validation-leaves-undecidable-shapes-linked-FIXED-20261012`): the
/// native linkage read positions 0 and 2 from the constant pool, so even a
/// valid condy `MethodType` was an uncatchable internal error.
pub(in crate::runtime::invokedynamic) fn lambda_resolve_dynamic_args(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    info: &IndyInfo,
) -> Result<Vec<Option<LambdaCondyArg>>, MethodCallFailed> {
    // Per position: `None` for a non-dynamic argument, else the first byte of
    // the dynamic constant's field descriptor (`0` when unreadable), which
    // says how the invoker boxes a primitive value (wave 47).
    let dynamic: Vec<Option<u8>> = {
        let cm = shared.classes.class_manager.read();
        let Some(class) = cm.get_class(class_id) else {
            return Ok(Vec::new());
        };
        let cp = &class.constant_pool;
        info.bootstrap_arg_indices
            .iter()
            .map(|&i| match cp.get(i) {
                Some(ConstantPoolEntry::Dynamic {
                    name_and_type_index,
                    ..
                }) => Some(
                    cp.get_name_and_type(*name_and_type_index)
                        .and_then(|(_, descriptor)| descriptor.bytes().next())
                        .unwrap_or(0),
                ),
                _ => None,
            })
            .collect()
    };
    if dynamic.iter().all(Option::is_none) {
        return Ok(Vec::new());
    }
    let mut out: Vec<Option<LambdaCondyArg>> = vec![None; dynamic.len()];
    for (position, &descriptor_tag) in dynamic.iter().enumerate() {
        let Some(descriptor_tag) = descriptor_tag else {
            continue;
        };
        let index = info.bootstrap_arg_indices[position];
        let value = crate::runtime::interpreter::constants::resolve_condy_constant(
            shared, thread, class_id, index,
        )?;
        let arg = match value {
            Value::Object(None) => LambdaCondyArg::bare(LambdaStaticArg::Null),
            Value::Object(Some(obj)) => {
                let cid = shared.mem.heap.class_id_of(obj);
                let (name, is_method_handle, hidden, cast_targets) = {
                    let cm = shared.classes.class_manager.read();
                    let class = cm.get_class(cid);
                    let name = class.map(|c| c.name.to_string()).unwrap_or_default();
                    let hidden = class.is_some_and(|c| c.is_hidden());
                    let method_handle = cm.get_loaded_class_id("java/lang/invoke/MethodHandle");
                    let is_method_handle =
                        method_handle.is_some_and(|mh| cm.is_subclass_of(cid, mh));
                    let method_type = cm.get_loaded_class_id("java/lang/invoke/MethodType");
                    (name, is_method_handle, hidden, (method_type, method_handle))
                };
                let kind = if is_method_handle {
                    LambdaStaticArg::MethodHandle
                } else {
                    // Only classes of `java.lang` (all in `java.base`, of the
                    // boot loader), whose cast message the checks can write.
                    match name.as_str() {
                        "java/lang/invoke/MethodType" => LambdaStaticArg::MethodType,
                        "java/lang/Integer" => LambdaStaticArg::Integer,
                        "java/lang/Class" => LambdaStaticArg::Class,
                        "java/lang/String" => LambdaStaticArg::Other("java.lang.String"),
                        "java/lang/Long" => LambdaStaticArg::Other("java.lang.Long"),
                        "java/lang/Float" => LambdaStaticArg::Other("java.lang.Float"),
                        "java/lang/Double" => LambdaStaticArg::Other("java.lang.Double"),
                        "java/lang/Object" => LambdaStaticArg::Other("java.lang.Object"),
                        // Wave 46: any other class, when HotSpot's message for
                        // both casts can be written (not a hidden class, whose
                        // external name this model does not spell).
                        _ if !hidden && !name.is_empty() => LambdaStaticArg::Foreign,
                        _ => LambdaStaticArg::Undecided,
                    }
                };
                let cast_messages = if kind == LambdaStaticArg::Foreign {
                    let from = name.replace('/', ".");
                    let thread_now: &JvmThread = &*thread;
                    let message = |to: &str, to_id: Option<ClassId>| {
                        crate::runtime::exceptions::hotspot_class_cast_message(
                            shared,
                            thread_now,
                            &from,
                            to,
                            to_id.map(|to_id| (cid, to_id)),
                        )
                    };
                    match (
                        message("java.lang.invoke.MethodType", cast_targets.0),
                        message("java.lang.invoke.MethodHandle", cast_targets.1),
                    ) {
                        (Some(to_type), Some(to_handle)) => Some(Box::new([to_type, to_handle])),
                        _ => None,
                    }
                } else {
                    None
                };
                // A class whose message cannot be written is not classified.
                let kind = if kind == LambdaStaticArg::Foreign && cast_messages.is_none() {
                    LambdaStaticArg::Undecided
                } else {
                    kind
                };
                let method_type = if kind == LambdaStaticArg::MethodType {
                    let ctx = NativeContextImpl {
                        shared,
                        thread: &mut *thread,
                    };
                    cratonvm_native_builtins::lang_invoke::method_type_object_descriptor(&ctx, obj)
                } else {
                    None
                };
                // A `MethodType` whose descriptor cannot be read back is not
                // classified: the checks stop undecided.
                let kind = if kind == LambdaStaticArg::MethodType && method_type.is_none() {
                    LambdaStaticArg::Undecided
                } else {
                    kind
                };
                // A direct handle's member (wave 42): what the checks and
                // `bootstrap_lambda` read at position 1 in place of the pool.
                let method_handle = if kind == LambdaStaticArg::MethodHandle {
                    let mut ctx = NativeContextImpl {
                        shared,
                        thread: &mut *thread,
                    };
                    cratonvm_native_builtins::lang_invoke::method_handle_value_member(&mut ctx, obj)
                        .and_then(|(tag, class, name, descriptor)| {
                            Some(MethodHandle {
                                kind: MethodHandleKind::from_tag(tag)?,
                                class_name: Arc::from(class),
                                member_name: Arc::from(name),
                                descriptor: Arc::from(descriptor),
                            })
                        })
                } else {
                    None
                };
                // Wave 43: a handle HotSpot cannot crack (bound, adapted, a
                // combinator), which `lambda_site_verdict` refuses.
                let not_direct = if kind == LambdaStaticArg::MethodHandle && method_handle.is_none()
                {
                    let mut ctx = NativeContextImpl {
                        shared,
                        thread: &mut *thread,
                    };
                    cratonvm_native_builtins::lang_invoke::method_handle_value_not_direct(
                        &mut ctx, obj,
                    )
                } else {
                    None
                };
                // Wave 47: what `altMetafactory`'s tail reads of an `Integer`
                // (a flags word, a count) or a `Class` (a marker interface).
                let int_value = if kind == LambdaStaticArg::Integer {
                    match (NativeContextImpl {
                        shared,
                        thread: &mut *thread,
                    })
                    .get_field_by_name(obj, "value")
                    {
                        Value::Int(value) => Some(value),
                        _ => None,
                    }
                } else {
                    None
                };
                let class_name = if kind == LambdaStaticArg::Class {
                    lambda_class_value_name(shared, obj)
                } else {
                    None
                };
                LambdaCondyArg {
                    kind,
                    method_type,
                    method_handle,
                    not_direct,
                    cast_messages,
                    int_value,
                    class_name,
                }
            }
            // A primitive-typed dynamic constant, boxed by the invoker as its
            // descriptor says (wave 47): an `int` is an `Integer` the
            // `altMetafactory` tail can read; the other wrappers are
            // `java.lang` classes whose cast messages the checks write.
            Value::Int(value) if descriptor_tag == b'I' => LambdaCondyArg {
                int_value: Some(value),
                ..LambdaCondyArg::bare(LambdaStaticArg::Integer)
            },
            Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_) => {
                let wrapper = match descriptor_tag {
                    b'Z' => Some("java.lang.Boolean"),
                    b'B' => Some("java.lang.Byte"),
                    b'S' => Some("java.lang.Short"),
                    b'C' => Some("java.lang.Character"),
                    b'J' => Some("java.lang.Long"),
                    b'F' => Some("java.lang.Float"),
                    b'D' => Some("java.lang.Double"),
                    _ => None,
                };
                LambdaCondyArg::bare(
                    wrapper.map_or(LambdaStaticArg::Undecided, LambdaStaticArg::Other),
                )
            }
            _ => LambdaCondyArg::bare(LambdaStaticArg::Undecided),
        };
        if dbg_lambda_dispatch() {
            let what = match arg.kind {
                LambdaStaticArg::MethodType => "a MethodType",
                LambdaStaticArg::MethodHandle => "a MethodHandle",
                LambdaStaticArg::Null => "null",
                LambdaStaticArg::Undecided => "not classified",
                LambdaStaticArg::Foreign => "an object of another class",
                _ => "a constant of another class",
            };
            let member = arg
                .method_handle
                .as_ref()
                .map(|h| {
                    format!(
                        " ({} {}.{}{})",
                        h.kind, h.class_name, h.member_name, h.descriptor
                    )
                })
                .or_else(|| {
                    arg.not_direct
                        .as_ref()
                        .map(|t| format!(" (not direct: {t})"))
                })
                .unwrap_or_default();
            eprintln!(
                "[DBG_LAMBDA] link-check {}{}: dynamic static argument {position} is {what}{member}",
                info.target_name, info.target_descriptor
            );
        }
        out[position] = Some(arg);
    }
    Ok(out)
}

/// The failure HotSpot raises linking this `SwitchBootstraps.typeSwitch` /
/// `enumSwitch` site (the cause of its `BootstrapMethodError`, always an
/// `IllegalArgumentException`), or `None` when it links or the class store
/// cannot decide. Interpreter round i1 wave 38, lane L4; the messages were
/// measured on HotSpot 25 (probe `L4W38SwitchSiteValidation.java`).
///
/// In the JDK's order: the invocation type (`Illegal invocation type
/// (Object)int`), then each label. The invocation type's arity and its two
/// `int`s are checked in EVERY mode: the native switches pop a selector and
/// a restart index and push an `int` whatever the call site declares, so a
/// site of another shape unbalanced the caller's operand stack. The
/// selector type (`enumSwitch` wants an enum) and the labels are checked
/// under `--real-jdk`. javac emits none of the refused shapes.
pub(in crate::runtime::invokedynamic) fn switch_site_refusal(
    shared: &SharedVm,
    class_id: ClassId,
    info: &IndyInfo,
    jdk_only: bool,
) -> Option<LambdaSiteRefusal> {
    use crate::runtime::interpreter::split_method_descriptor_ref as split;
    let illegal = |message: String, every_mode: bool| LambdaSiteRefusal {
        cause_class: "java/lang/IllegalArgumentException",
        message,
        every_mode,
    };
    let type_switch = info.bsm_method == TYPE_SWITCH;
    let (params, ret) = split(&info.target_descriptor);
    let invocation_type = |cm: &crate::classloading::ClassManager| {
        format!(
            "Illegal invocation type {}",
            method_type_display_for(cm, class_id, &info.target_descriptor)
        )
    };
    // `typeSwitch` reads `parameterType(0)` before its check (measured:
    // `ArrayIndexOutOfBoundsException: Index 0 out of bounds for length 0`).
    if type_switch && params.is_empty() {
        return Some(LambdaSiteRefusal {
            cause_class: "java/lang/ArrayIndexOutOfBoundsException",
            message: "Index 0 out of bounds for length 0".to_string(),
            every_mode: true,
        });
    }
    if params.len() != 2 || ret != "I" || params[1] != "I" {
        return Some(illegal(
            invocation_type(&*shared.classes.class_manager.read()),
            true,
        ));
    }
    if !jdk_only {
        return None;
    }
    let selector = params[0];
    let guard = shared.classes.class_manager.read();
    let cm: &crate::classloading::ClassManager = &guard;
    let no_resolved = std::collections::HashMap::new();
    let types = LambdaTypes {
        cm,
        caller: class_id,
        resolved: &no_resolved,
    };
    let class = cm.get_class(class_id)?;
    let cp = &class.constant_pool;
    let enum_class = if type_switch {
        None
    } else {
        // `enumSwitch`: `!parameterType(0).isPrimitive() && isEnum()`.
        if !selector.starts_with('L') {
            return Some(illegal(invocation_type(cm), false));
        }
        let selector_class = cm.get_class(types.class_of(selector)?)?;
        let is_enum = selector_class
            .access_flags
            .contains(cratonvm_reader::class_access_flags::ClassAccessFlags::ENUM)
            && selector_class
                .superclass
                .and_then(|s| cm.get_class(s))
                .is_some_and(|s| &*s.name == "java/lang/Enum");
        if !is_enum {
            return Some(illegal(invocation_type(cm), false));
        }
        Some(selector_class.name.to_string())
    };
    // A label's `getClass()`, as `Class.toString()` prints it; `None` for a
    // label whose class the class store cannot name.
    let label_class = |index: u16| -> Option<String> {
        Some(match cp.get(index)? {
            ConstantPoolEntry::ClassReference { .. } => "class java.lang.Class".to_string(),
            ConstantPoolEntry::StringReference { .. } => "class java.lang.String".to_string(),
            ConstantPoolEntry::Integer(_) => "class java.lang.Integer".to_string(),
            ConstantPoolEntry::Long(_) => "class java.lang.Long".to_string(),
            ConstantPoolEntry::Float(_) => "class java.lang.Float".to_string(),
            ConstantPoolEntry::Double(_) => "class java.lang.Double".to_string(),
            ConstantPoolEntry::MethodType { .. } => "class java.lang.invoke.MethodType".to_string(),
            ConstantPoolEntry::MethodHandle { .. } => {
                let handle = resolve_method_handle_full(cp, index).ok()?;
                format!("class {}", types.method_handle_constant_class(&handle)?)
            }
            // A dynamic constant (a primitive class or an `EnumDesc` from
            // javac; anything from its bootstrap otherwise) is not decided.
            _ => return None,
        })
    };
    for &index in &info.bootstrap_arg_indices {
        let entry = cp.get(index)?;
        match &enum_class {
            None => {
                // `typeSwitch`'s `verifyLabel`: `Class`, `String`, `Integer`,
                // `EnumDesc`; `Long`/`Float`/`Double`/`Boolean` only with
                // preview features on and a selector other than `boolean`.
                let accepted = match entry {
                    ConstantPoolEntry::ClassReference { .. }
                    | ConstantPoolEntry::StringReference { .. }
                    | ConstantPoolEntry::Integer(_) => true,
                    ConstantPoolEntry::Long(_)
                    | ConstantPoolEntry::Float(_)
                    | ConstantPoolEntry::Double(_) => {
                        shared.config.enable_preview
                            && !matches!(selector, "Z" | "Ljava/lang/Boolean;")
                    }
                    ConstantPoolEntry::Dynamic { .. } => return None,
                    _ => false,
                };
                if !accepted {
                    return Some(illegal(
                        format!("label with illegal type found: {}", label_class(index)?),
                        false,
                    ));
                }
            }
            Some(enum_name) => match entry {
                // `convertEnumConstants`: a name, or the enum class itself.
                ConstantPoolEntry::StringReference { .. } => {}
                ConstantPoolEntry::ClassReference { .. } => {
                    let label = cp.get_class_name(index)?;
                    if label != enum_name.as_str() {
                        return Some(illegal(
                            format!(
                                "the Class label: {}, expected the provided enum class: {}",
                                types.display(&format!("L{label};"))?,
                                types.display(&format!("L{enum_name};"))?
                            ),
                            false,
                        ));
                    }
                }
                ConstantPoolEntry::Dynamic { .. } => return None,
                _ => {
                    return Some(illegal(
                        format!(
                            "label with illegal type found: {}, expected label of type either \
                             String or Class",
                            label_class(index)?
                        ),
                        false,
                    ));
                }
            },
        }
    }
    None
}

/// The failure HotSpot raises linking this `ObjectMethods.bootstrap` site
/// (the cause of its `BootstrapMethodError`, an `IllegalArgumentException`),
/// or `None` when it links or the constant pool cannot decide. Interpreter
/// round i1 wave 38, lane L4; messages measured on HotSpot 25 (probe
/// `L4W38ObjectMethodsSiteValidation.java`).
///
/// In the JDK's order: the method name (`IllegalArgumentException(name)`;
/// it was an internal error here, which Java cannot catch), the method type
/// (`Bad method type: (R)long`), then for `toString` the name list against
/// the accessors (`Name list and accessor list do not match`, counted as
/// `String.split(";")` counts). The name, and a type whose SHAPE is wrong (its
/// arity or return), are refused in every mode: the native record methods pop
/// and push by the name, whatever the site declares. A type that differs only
/// in its classes, and the name list, under `--real-jdk`.
pub(in crate::runtime::invokedynamic) fn object_methods_site_refusal(
    shared: &SharedVm,
    class_id: ClassId,
    info: &IndyInfo,
    jdk_only: bool,
) -> Option<LambdaSiteRefusal> {
    use crate::runtime::interpreter::split_method_descriptor_ref as split;
    let illegal = |message: String, every_mode: bool| LambdaSiteRefusal {
        cause_class: "java/lang/IllegalArgumentException",
        message,
        every_mode,
    };
    let (want_params, want_ret): (usize, &str) = match info.target_name.as_str() {
        "equals" => (2, "Z"),
        "hashCode" => (1, "I"),
        "toString" => (1, "Ljava/lang/String;"),
        other => return Some(illegal(other.to_string(), true)),
    };
    let (params, ret) = split(&info.target_descriptor);
    let bad_type = |cm: &crate::classloading::ClassManager| {
        format!(
            "Bad method type: {}",
            method_type_display_for(cm, class_id, &info.target_descriptor)
        )
    };
    if params.len() != want_params || ret != want_ret {
        return Some(illegal(
            bad_type(&*shared.classes.class_manager.read()),
            true,
        ));
    }
    if !jdk_only {
        return None;
    }
    let cm = shared.classes.class_manager.read();
    let class = cm.get_class(class_id)?;
    let cp = &class.constant_pool;
    let args = &info.bootstrap_arg_indices;
    let record = cp.get_class_name(*args.first()?)?;
    let record_desc = format!("L{record};");
    if params[0] != record_desc || (want_params == 2 && params[1] != "Ljava/lang/Object;") {
        return Some(illegal(bad_type(&*cm), false));
    }
    if info.target_name == "toString" {
        let names = resolve_string_constant(cp, *args.get(1)?)?;
        // `"".equals(names) ? List.of() : List.of(names.split(";"))`: Java's
        // `split` drops the trailing empty strings.
        let count = if names.is_empty() {
            0
        } else {
            let parts: Vec<&str> = names.split(';').collect();
            parts.len() - parts.iter().rev().take_while(|p| p.is_empty()).count()
        };
        if count != args.len().saturating_sub(2) {
            return Some(illegal(
                "Name list and accessor list do not match".to_string(),
                false,
            ));
        }
    }
    None
}

/// `true` when every getter (static argument 2 onward) of an
/// `ObjectMethods.bootstrap` site is a `REF_getField` whose field reference
/// names the record class (static argument 0), read from the pool alone.
/// Such a site links natively in every mode (javac's shape, or getter-driven);
/// any other may take the JDK's own bootstrap under `--real-jdk`.
pub(in crate::runtime::invokedynamic) fn object_methods_getters_are_record_fields(pool: &ConstantPool, args: &[u16]) -> bool {
    let Some(record) = args.first().and_then(|&i| pool.get_class_name(i)) else {
        return false;
    };
    args.iter().skip(2).all(|&i| match pool.get(i) {
        Some(ConstantPoolEntry::MethodHandle {
            reference_kind: 1,
            reference_index,
        }) => matches!(
            pool.get(*reference_index),
            Some(ConstantPoolEntry::FieldReference { class_index, .. })
                if pool.get_class_name(*class_index) == Some(record)
        ),
        _ => false,
    })
}

