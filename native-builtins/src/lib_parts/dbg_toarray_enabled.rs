#[allow(unused_imports)]
use super::super::*;


/// Whether the array-class diagnostic is enabled. The environment is fixed for
/// a VM process, so read it once: `Object.getClass()` is hot in reflection
/// workloads and querying the process environment allocates and locks.
#[inline]
pub fn dbg_toarray_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| crate::nbflags().dbg_toarray)
}

/// `CRATONVM_DBG_NPE_TRACE` (`CRATONVM_DBG=npe-trace`) — the same flag
/// `vm/src/runtime/exceptions.rs` gates its NPE-origin dumps with, read here
/// for the `[SUREFIRE-NPE]` forensic in `native_exception_init_message`.
///
/// Read once: the environment is fixed for a VM process, and this sits on the
/// constructor of ~50 exception subclasses, which framework code throws as
/// control flow.
#[inline]
pub fn surefire_npe_trace_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED
        .get_or_init(|| None::<std::ffi::OsString>.is_some())}

/// Normalise property keys after `read_string` (trim stray control/NUL).
#[inline]
pub fn normalize_java_property_key(key: &str) -> String {
    key.trim_matches(|c: char| c.is_ascii_control() || c == '\0')
        .to_string()
}

/// Defaults for critical bootstrap keys when the VM store misses a lookup.
#[inline]
pub fn jboss_home_from_modules_dir(path: &std::path::Path) -> Option<String> {
    if path.file_name().and_then(|n| n.to_str()) == Some("modules") {
        return path.parent().map(|p| p.to_string_lossy().into_owned());
    }
    None
}

#[inline]
pub fn jboss_home_from_home_dir(path: &std::path::Path) -> Option<String> {
    if path.join("modules").is_dir() && path.join("jboss-modules.jar").is_file() {
        return Some(path.to_string_lossy().into_owned());
    }
    None
}

pub fn jboss_home_from_module_path(module_path: &str) -> Option<String> {
    std::env::split_paths(module_path).find_map(|p| jboss_home_from_modules_dir(&p))
}

pub fn jboss_home_from_class_path(class_path: &str) -> Option<String> {
    std::env::split_paths(class_path).find_map(|p| {
        if p.file_name().and_then(|n| n.to_str()) == Some("jboss-modules.jar") {
            return p
                .parent()
                .map(|parent| parent.to_string_lossy().into_owned());
        }
        None
    })
}

pub fn jboss_home_dir_fallback() -> Option<String> {
    if let Ok(home) = cratonvm_types::flags::runtime_var("JBOSS_HOME") {
        if !home.is_empty() {
            return Some(home);
        }
    }
    cratonvm_types::flags::runtime_var("CRATONVM_JBOSS_MP_ROOT")
        .ok()
        .and_then(|mp_root| jboss_home_from_modules_dir(std::path::Path::new(&mp_root)))
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .and_then(|cwd| jboss_home_from_home_dir(&cwd))
        })
}

/// Has the application called `System.setProperties`?
///
/// One process-global latch, set once and never cleared, read only by
/// [`system_property_fallback`]. It is a latch rather than a comparison against
/// the map's contents because the question is about an EVENT ("the application
/// took ownership of the property map"), not about a state -- and because the
/// alternative, listing the whole map on every missing-property lookup, is a
/// per-lookup cost on a path that misses often.
pub static SYSTEM_PROPERTIES_REPLACED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn system_properties_explicitly_replaced() -> bool {
    SYSTEM_PROPERTIES_REPLACED.load(std::sync::atomic::Ordering::Relaxed)
}

/// `java.lang.System.checkKey` -- the two-line validation every property entry
/// point runs before it touches the map:
///
/// ```java
/// private static void checkKey(String key) {
///     if (key == null) throw new NullPointerException("key can't be null");
///     if (key.isEmpty()) throw new IllegalArgumentException("key can't be empty");
/// }
/// ```
///
/// `getProperty(String)` had the null half and none of the four had the empty
/// half, so `System.getProperty("")` answered null and `System.clearProperty("")`
/// actually removed a key stored under the empty name -- a value where the JDK
/// raises. MEASURED by `SystemRuntimeObjectSweep.java`.
pub fn system_property_check_key(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Result<String, cratonvm_types::error::MethodCallFailed> {
    let key_obj = match args.first() {
        Some(Value::Object(Some(k))) => *k,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("key can't be null".to_string()),
            }
            .into())
        }
    };
    let key = property_key_from_java_string(ctx, key_obj);
    if key.is_empty() {
        return Err(RuntimeError::IllegalArgumentException {
            message: "key can't be empty".to_string(),
        }
        .into());
    }
    Ok(key)
}

/// What `System.getProperty` answers, in precedence order.
///
/// Three sources, and the order is the whole content of this function:
///
///  1. the system `Properties` object's own real `map`, when it has one;
///  2. the VM's system-property store;
///  3. [`system_property_fallback`], the bootstrap defaults.
///
/// (1) is new with the Phase 3 `java/util/Properties` retirement and is the
/// half a mirror cannot do. `System.setProperty` and `System.clearProperty`
/// write BOTH stores, so those two never disagree. What has no mirror is real
/// JDK bytecode reached through a reference the caller already holds --
/// `System.getProperties().setProperty(k, v)` -- which writes the map and
/// nothing else. Asking the map first is what makes that write visible here.
///
/// It cannot be the ONLY source: the map is refilled from the store by
/// `System.getProperties()`, so between two such calls it legitimately lags a
/// property the VM has set, and every miss has to fall through. The two orders
/// are not interchangeable -- store-first would answer the STALE value for a
/// key written through the receiver, which is the update half of the same
/// defect and would have left `SystemRuntimeObjectSweep` row 39 green for the
/// wrong reason (it adds a key rather than changing one).
///
/// # Which of the two the map is asked for, and why this order and not the other
///
/// "Map first" is narrower than it sounds, and deliberately so. The map is
/// consulted first only for an entry it can have an OPINION about -- one the
/// application wrote through the receiver, which
/// `properties_sidetable::map_entry_is_a_receiver_write` identifies by
/// comparing the live value against what `replace_real_map` published. An entry
/// equal to the published value is this VM's own snapshot echoed back; it falls
/// through to the store.
///
/// **That test is a fix, not a refinement.** Without it a property written into
/// the VM store by RUST -- `set_system_property` from somewhere in the VM, with
/// no `System.setProperty` and therefore no mirror into the map -- was masked
/// here by the older published value, and then DESTROYED by the next
/// `System.getProperties()`, whose harvest folded that older value back into the
/// store before the refill re-published it.
///
/// The reverse order -- store first -- is still wrong, and worse: it masks every
/// write through a held `Properties` reference, which is Java-visible.
///
/// The tie-break is not a guess about which is rarer. On a real JDK there is
/// exactly one store and it IS the `Properties` object -- `System.getProperty`
/// is literally `props.getProperty(key)`. This VM's own store is the shim, so
/// when the two disagree the object is the one telling the truth about what
/// Java did, and the store is the one that has to catch up. That is the same
/// direction the whole §1.4 retirement campaign moves in.
///
/// Cost: one `invoke_virtual` per call, and only once a system `Properties`
/// singleton exists with a filled `map`. MEASURED as affordable rather than
/// assumed -- `System.getProperty`'s native carries `invocations: 0` in the
/// probe-tree census, because the VM's own reads go through
/// `NativeContext::get_system_property` in Rust and JDK callers go through
/// `StaticProperty`'s cached statics. This is not a hot path.
pub fn system_property_read(ctx: &mut dyn NativeContext, key: &str) -> Option<String> {
    if let Some(props) = crate::lang_system::system_props_singleton(ctx.vm_identity()) {
        if let Some(v) = crate::properties_sidetable::lookup_in_real_map(ctx, props, key) {
            // …but only when the map is telling us something the store does not
            // already know. An entry identical to what this VM published is its
            // own value echoed back; answering from it would mask a Rust-side
            // `set_system_property` made since the publish, which is the
            // residual the doc comment above describes and which
            // `properties_sidetable::last_published` now closes by remembering
            // the published VALUE rather than just the key.
            if crate::properties_sidetable::map_entry_is_a_receiver_write(
                ctx.vm_identity(),
                key,
                &v,
            ) {
                return Some(v);
            }
        }
        // The real map is the one the application can `remove` from; a key this
        // VM published there that is no longer in it was deleted, and the store
        // below still holds the value it had.
        if crate::sysprops_merge::jdk_bodies_own_system_props(&*ctx)
            && crate::properties_sidetable::removed_through_receiver(ctx, key)
        {
            return None;
        }
    }
    ctx.get_system_property(key)
        .or_else(|| system_property_fallback(ctx, key))
}

pub fn system_property_fallback(ctx: &dyn NativeContext, key: &str) -> Option<String> {
    // An explicit `System.setProperties(p)` REPLACES the property map, and a
    // key absent from the replacement is absent -- not a cue to fall back to a
    // platform default.
    //
    // [`bootstrap_property_fallback`] says of itself that it "is reached only
    // before that map exists". That premise holds for the boot path it was
    // written for and fails for this one: after `setProperties` the map exists
    // and is simply missing the key, so the fallback answered `"/"` for a
    // `file.separator` the caller had just removed, where HotSpot answers null.
    // MEASURED by `SystemRuntimeObjectSweep.java`.
    //
    // Narrow on purpose. The latch is set only by `setProperties`, so nothing
    // changes for a program that never calls it -- which is every boot path
    // this fallback exists for.
    if system_properties_explicitly_replaced() {
        return None;
    }
    if key == "jboss.home.dir" {
        return bootstrap_property_fallback(key)
            .or_else(|| {
                ctx.get_system_property("module.path")
                    .or_else(|| ctx.get_system_property("modules.path"))
                    .and_then(|mp| jboss_home_from_module_path(&mp))
            })
            .or_else(|| {
                ctx.get_system_property("java.class.path")
                    .and_then(|cp| jboss_home_from_class_path(&cp))
            });
    }
    bootstrap_property_fallback(key)
}

#[inline]
pub fn bootstrap_property_fallback(key: &str) -> Option<String> {
    match key {
        "file.separator" => Some(std::path::MAIN_SEPARATOR.to_string()),
        "path.separator" => Some(if cfg!(windows) {
            ";".to_string()
        } else {
            ":".to_string()
        }),
        // Both arms used to be `"\n"`, which is a `cfg!(windows)` that decides
        // nothing — the shape a reader takes for a platform split and a compiler
        // takes for a constant. `file.separator` and `path.separator` beside it
        // are platform-correct, which is what makes the wrong one easy to miss.
        // The authoritative seed is `vm_init`'s property map; this fallback is
        // reached only before that map exists, and answering `"\n"` there put
        // LF into anything that terminated a line during bootstrap.
        "line.separator" => Some(if cfg!(windows) {
            "\r\n".to_string()
        } else {
            "\n".to_string()
        }),
        "jboss.home.dir" => jboss_home_dir_fallback(),
        // Reactor Netty normally sizes workers from host CPU count. Keep a small
        // deterministic pool, but use at least four workers: smaller pools can
        // strand simultaneous STOMP connects under real-JDK mode.
        "reactor.netty.ioWorkerCount" => Some("4".to_string()),
        _ => None,
    }
}

pub fn native_spring_default_document_loader_create_document_builder_factory(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let validation_mode = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
    let namespace_aware = args.get(2).and_then(|v| v.as_int()).unwrap_or(0) != 0;
    let factory = match ctx.invoke(
        "javax/xml/parsers/DocumentBuilderFactory",
        "newInstance",
        "()Ljavax/xml/parsers/DocumentBuilderFactory;",
        &[],
    )? {
        Some(Value::Object(Some(factory))) => factory,
        _ => return Ok(Some(Value::Object(None))),
    };

    // Every setter below dispatches into Java and can collect.  Keep the
    // factory rooted across the whole configuration sequence and refresh it
    // before every forwarded use (including the final return value).
    let factory_pin = ctx.pin_native_root(factory);
    let result = (|| -> MethodCallResult {
        let factory = ctx.read_native_pin(factory_pin, factory);
        spring_xml_set_factory_bool(ctx, factory, "setNamespaceAware", namespace_aware)?;
        if validation_mode != 0 {
            let factory = ctx.read_native_pin(factory_pin, factory);
            spring_xml_set_factory_bool(ctx, factory, "setValidating", true)?;
            if validation_mode == 3 {
                let factory = ctx.read_native_pin(factory_pin, factory);
                spring_xml_set_factory_bool(ctx, factory, "setNamespaceAware", true)?;
                let schema = ctx.create_string("http://www.w3.org/2001/XMLSchema");
                let factory = ctx.read_native_pin(factory_pin, factory);
                spring_xml_set_factory_attribute(
                    ctx,
                    factory,
                    "http://java.sun.com/xml/jaxp/properties/schemaLanguage",
                    Value::Object(Some(schema)),
                )?;
                if let Some(pool) = spring_xml_shared_grammar_pool(ctx) {
                    let factory = ctx.read_native_pin(factory_pin, factory);
                    spring_xml_set_factory_attribute(
                        ctx,
                        factory,
                        "http://apache.org/xml/properties/internal/grammar-pool",
                        Value::Object(Some(pool)),
                    )?;
                }
            }
        }

        Ok(Some(Value::Object(Some(
            ctx.read_native_pin(factory_pin, factory),
        ))))
    })();
    ctx.unpin_native_roots(factory_pin);
    result
}

pub fn native_output_stream_writer_write_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let text = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        Some(Value::Object(None)) => "null".to_string(),
        _ => String::new(),
    };
    let bytes = text.into_bytes();
    write_bytes_from_output_stream_writer(ctx, this, &bytes)
}

pub fn native_output_stream_writer_write_string_range(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let full = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        Some(Value::Object(None)) => "null".to_string(),
        _ => String::new(),
    };
    let off = match args.get(2) {
        Some(Value::Int(v)) => (*v).max(0) as usize,
        _ => 0,
    };
    let len = match args.get(3) {
        Some(Value::Int(v)) => (*v).max(0) as usize,
        _ => full.encode_utf16().count(),
    };
    let units: Vec<u16> = full.encode_utf16().collect();
    let end = off.saturating_add(len).min(units.len());
    let start = off.min(end);
    let text = String::from_utf16_lossy(&units[start..end]);
    let bytes = text.into_bytes();
    write_bytes_from_output_stream_writer(ctx, this, &bytes)
}

pub fn native_output_stream_writer_write_chars(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let chars = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
    };
    let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
    let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
    let arr_len = ctx.array_length(chars) as i32;
    let end = off.checked_add(len).unwrap_or(i32::MAX);
    if off < 0 || len < 0 || end > arr_len {
        return Err(RuntimeError::aioobe_index_only(end).into());
    }
    if len == 0 {
        return Ok(None);
    }

    let mut units = Vec::with_capacity(len as usize);
    for i in off..end {
        let ch = match ctx.get_array_element(chars, i as usize) {
            Value::Int(v) => v as u16,
            _ => 0,
        };
        units.push(ch);
    }
    let text = String::from_utf16_lossy(&units);
    let bytes = text.into_bytes();
    write_bytes_from_output_stream_writer(ctx, this, &bytes)
}

pub fn native_output_stream_writer_write_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let ch = match args.get(1) {
        Some(Value::Int(v)) => char::from_u32(*v as u32).unwrap_or('\u{fffd}'),
        _ => '\0',
    };
    let mut buf = [0u8; 4];
    let bytes = ch.encode_utf8(&mut buf).as_bytes();
    write_bytes_from_output_stream_writer(ctx, this, bytes)
}

pub fn native_output_stream_writer_flush(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    if let Some(out) = osw_wrapped_output(ctx, this) {
        let _ = ctx.invoke_virtual(out, "flush", "()V", &[])?;
    }
    Ok(None)
}

pub fn native_output_stream_writer_close(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    if let Some(out) = osw_wrapped_output(ctx, this) {
        let _ = ctx.invoke_virtual(out, "close", "()V", &[])?;
    }
    Ok(None)
}

// `java/io/InputStreamReader.close()` has NO body here, deliberately.
//
// `native_input_stream_reader_close` lived here from 29539defc until it was
// deleted, and its `register` call was removed by aaf64a5de -- the commit that
// tore out the synthetic `<init>`/`read([CII)I`/`close` block because it
// "unconditionally shadowed that real bytecode for EVERY InputStreamReader"
// and "broke every multi-byte decode". The function body outlived its
// registration by an oversight; the deadness was the intent.
//
// Do not restore it. Two live bodies already own this triple, and this one
// agreed with NEITHER:
//
//   * `native-io/src/lib.rs::native_isr_close` wins on the real-JDK path. It
//     reads the wrapped stream from slot 1 with a legacy fd fallback at slot 0,
//     and drops the reader's pending UTF-8 decode state.
//   * the synthetic body registered in `servlet.rs` reads slot 0 -- the layout
//     its own three `<init>`s park the stream in -- and, crucially, NULLS slot 0
//     afterwards so a subsequent `read()` reports EOF.
//
// The deleted function read slot 0 and did not null it, so it matched the
// SYNTHETIC layout while missing that layout's close semantics: re-registering
// it would have kept reading from a closed reader. Against the real-JDK layout
// slot 0 is the fd, not the stream, so it would have closed nothing at all.
// Registration is last-write-wins, so wiring it up would not have added a
// fallback -- it would have replaced a correct winner with a wrong one.
// W7-57-close-flush-swallow-sweep.md row 12.

#[derive(Default)]
pub struct BufferedInputStreamMarkState {
    pub mark_limit: usize,
    pub mark_active: bool,
    pub replay_pos: usize,
    pub bytes: Vec<u8>,
}

// gc-common w20-c (`common-w19a-lib-rs-identity-hash-keyed-side-tables`):
// the `BufferedInputStream` mark table, the JUL `useParentHandlers` table, the
// `MessageBytes.toChars` memo, the Netty jctools queue store and the
// `LongAdder` cell strips (and, found by the same lane, the `Thread`
// explicit-null context-loader set) were each one process-wide table keyed
// by the object's bare identity hash. Identity hashes are numbered from the
// same seed in every heap, so the Nth stream marked in VM A and the Nth in VM
// B shared a row (one replayed the other's bytes, two adders summed one
// strip, one Netty queue polled the other's element and resolved its global
// handle in the wrong VM's table), and no row ever went away with its object.
//
// Each is now keyed by the object's WEAK lock key
// ([`gc_stable_weak_lock_key`]): minted per VM, unique across VMs, stable
// across moves, and freed when the object dies or its VM goes. The registry
// hands every freed key to [`forget_lib_side_table_keys`], which drops the
// rows filed under it (a dead Netty queue's element handles are released by
// the next queue call of that VM). A write mints the key; a read or remove
// asks [`existing_weak_lock_key`], so an object never written has no slot.

/// The weak lock key a `lib.rs` side table files `obj`'s row under, minting
/// it on first use.
pub fn lib_side_key(ctx: &dyn NativeContext, obj: ObjectRef) -> usize {
    gc_stable_weak_lock_key(ctx, obj)
        .unwrap_or_else(|_| unreachable!("gc_stable_weak_lock_key never fails"))
}

/// Drop every `lib.rs` side-table row filed under `keys`, which the lock-key
/// registry has just freed (the object died, or its VM is being torn down)
/// and will never mint again. Called by `sweep_lock_keys` and
/// `forget_vm_lock_keys` with the registry lock released; each table lock is
/// a leaf, taken and released on its own, and poison-tolerant because this
/// runs in the collection epilogue. A dead Netty queue's element handles are
/// moved to its VM's retired list, not released here: releasing needs that
/// VM's context.
pub fn forget_lib_side_table_keys(keys: &[usize]) {
    if keys.is_empty() {
        return;
    }
    {
        let mut marks = buffered_input_stream_marks()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !marks.is_empty() {
            for k in keys {
                marks.remove(k);
            }
        }
    }
    {
        let mut flags = jul_use_parent_handlers_rows()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !flags.is_empty() {
            for k in keys {
                flags.remove(k);
            }
        }
    }
    {
        let mut memo = message_bytes_to_chars_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !memo.is_empty() {
            for k in keys {
                memo.remove(k);
            }
        }
    }
    {
        let mut cells = long_adder_cells_table().lock();
        if !cells.is_empty() {
            for k in keys {
                cells.remove(k);
            }
        }
    }
    {
        let mut nulls = explicit_null_context_class_loader_store()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !nulls.is_empty() {
            for k in keys {
                nulls.remove(k);
            }
        }
    }
    forget_netty_queue_keys(keys);
}

pub fn buffered_input_stream_marks(
) -> &'static std::sync::Mutex<std::collections::HashMap<usize, BufferedInputStreamMarkState>> {
    static STORE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<usize, BufferedInputStreamMarkState>>,
    > = std::sync::OnceLock::new();
    STORE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// `java.util.logging.Logger.useParentHandlers`, keyed by the logger's weak
/// lock key (gc-common w20-c; it was the bare identity hash). Read it through
/// [`jul_logger_use_parent_handlers_flag`], write it through
/// [`jul_logger_set_use_parent_handlers`].
///
/// Same shape, and the same reason, as `logging_shims::jul_logger_handlers_*`
/// / `jul_logger_parent_*`: real JDK 25 keeps this flag inside
/// `Logger$ConfigurationData` (reached through the logger's `config` slot),
/// while the flat synthetic loggers `logmanager::allocate_logger` mints hold
/// their NAME in that slot — so no raw field index is safe for both shapes.
/// Unlike those two tables this stores a plain `bool`, so there is no
/// ObjectRef to keep alive and no `add_global_root` is required.
///
/// `pub` because the WRITE and the READ used to live in different
/// modules and never met: `setUseParentHandlers` recorded here while the
/// publication walk (`logmanager::jul_use_parent_handlers`) read
/// `config.useParentHandlers`, which is null on every logger this bridge
/// mints — so `getUseParentHandlers()` reported `false` correctly while the
/// record went to the parent's handlers anyway. An accessor agreeing with
/// itself is not the consumer agreeing with it; the consumer must read the
/// same table the setter wrote. Measured, `Compatible`, before the change:
/// `setUseParentHandlers(false)` then `info(...)` still reached the parent's
/// Handler, where HotSpot and `--real-jdk` (real bytecode, no native holds
/// the triple) both stop the walk. See
/// docs/feature-designs/native-dispatch-architecture.md.
pub fn jul_use_parent_handlers_rows(
) -> &'static std::sync::Mutex<std::collections::HashMap<usize, bool>> {
    static STORE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<usize, bool>>> =
        std::sync::OnceLock::new();
    STORE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// The `useParentHandlers` flag `Logger.setUseParentHandlers(Z)V` recorded
/// for `logger` in this VM, or `None` when the setter was never called on it
/// (the caller then applies the JDK default, `true`, or reads `config`).
/// Mints no key.
pub fn jul_logger_use_parent_handlers_flag(
    ctx: &dyn NativeContext,
    logger: ObjectRef,
) -> Option<bool> {
    let key = existing_weak_lock_key(ctx, logger)?;
    jul_use_parent_handlers_rows()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .copied()
}

/// Record `Logger.setUseParentHandlers(flag)` for `logger`.
pub fn jul_logger_set_use_parent_handlers(ctx: &dyn NativeContext, logger: ObjectRef, flag: bool) {
    let key = lib_side_key(ctx, logger);
    jul_use_parent_handlers_rows()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, flag);
}

pub fn buffered_input_stream_input(ctx: &dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    match ctx.get_field_by_name(this, "in") {
        Value::Object(Some(o)) => Some(o),
        _ => match ctx.get_field(this, 0) {
            Value::Object(Some(o)) => Some(o),
            _ => None,
        },
    }
}

pub fn buffered_input_stream_init(ctx: &mut dyn NativeContext, this: ObjectRef, input: Value) {
    ctx.set_field_by_name(this, "in", input);
    ctx.set_field(this, 0, input);
    // Native reads delegate directly to `in`, but inherited real-JDK methods
    // such as FilterInputStream.skip can still enter BufferedInputStream.skip.
    // Keep the real layout in a valid empty-buffer state so ensureOpen() does
    // not interpret this freshly constructed stream as closed.
    //
    // gc-common w19-a: the buffer allocation can collect and move `this` (the
    // funnel's argument pin forwards only its own copy), so re-read it.
    let this_pin = ctx.pin_native_root(this);
    let buffer = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 8192);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    ctx.set_field_by_name(this, "buf", Value::Object(Some(buffer)));
    ctx.set_field_by_name(this, "count", Value::Int(0));
    ctx.set_field_by_name(this, "pos", Value::Int(0));
    ctx.set_field_by_name(this, "markpos", Value::Int(-1));
    ctx.set_field_by_name(this, "marklimit", Value::Int(0));
    if let Some(key) = existing_weak_lock_key(ctx, this) {
        buffered_input_stream_marks()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key);
    }
}

pub fn byte_array_input_stream_skip(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    requested: i64,
) -> i64 {
    if requested <= 0 {
        return 0;
    }
    let pos = ctx
        .get_field_by_name(this, "pos")
        .as_int()
        .unwrap_or(0)
        .max(0) as i64;
    let count = ctx
        .get_field_by_name(this, "count")
        .as_int()
        .unwrap_or(0)
        .max(0) as i64;
    let skipped = requested.min(count.saturating_sub(pos));
    ctx.set_field_by_name(this, "pos", Value::Int((pos + skipped) as i32));
    skipped
}

// `buffered_input_stream_skip` was DELETED 2026-08-22 (WORKER 4) with the
// `java/io/BufferedInputStream.skip(J)J` registration that was its only caller.
//
// # It returned the right COUNT and left the stream in the wrong PLACE
//
// MEASURED, `regression-suite/probes/W4Buffers.java`, Linux/JDK 25.0.4, both
// modes. A `BufferedInputStream(bais, 4)` over `"0123456789"`, then
// `read(); mark(8); read(buf,0,4); reset(); read(); skip(3); readAllBytes()`:
//
//     read          48    <- agreed
//     read(buf,0,4)  4:1234   <- agreed
//     reset(); read 49    <- agreed
//     skip(3)        3    <- agreed
//     readAllBytes  "56789" on HotSpot,  "23489" on CratonVM
//
// Every step agreed and the last one returned different BYTES, which is the
// signature of an accounting error rather than a logic one.
//
// # Why: this class was modelled TWICE and the halves did not share state
//
// `mark`, `reset`, `read` and `readAllBytes` are NOT registered on
// `BufferedInputStream` in this build — they run the JDK's own bytecode over
// the real `buf` / `pos` / `count` / `markpos` fields. `skip` was, and it drove
// a completely different model: `buffered_input_stream_read_one`, a side table
// of replayed bytes keyed off the object, falling through to
// `invoke_virtual(input, "read", "()I")` on the UNDERLYING stream.
//
// So `skip(3)` pulled three bytes out of the SOURCE (index 5 -> 8) while the
// JDK's own `buf` still held `"234"` unread at `pos`. `readAllBytes` then
// returned the buffer's leftovers followed by the source's new position:
// `"234"` + `"89"`. The count was right because three bytes really were
// consumed — from the wrong place.
//
// `[native-backed state is invisible to real JDK bytecode]`, in its sharpest
// form: not a native and bytecode disagreeing about a value, but a native and
// bytecode maintaining two independent CURSORS over one stream.
//
// # Trap 4, checked
//
// `BufferedInputStream extends FilterInputStream`, and
// `java/io/FilterInputStream.skip(J)J` IS registered
// (`filter_input_stream_skip`), so the superclass walk promotes it. That is
// deliberate and it is why this deletion is safe: `filter_input_stream_skip`
// discards through `invoke_virtual(this, "read", "([BII)I")` on the RECEIVER,
// which is `BufferedInputStream`'s own real `read` over its own real buffer.
// It cannot desynchronise the two cursors because it only ever moves one.

pub fn filter_input_stream_skip(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    requested: i64,
) -> Result<i64, MethodCallFailed> {
    if requested <= 0 {
        return Ok(0);
    }
    // Match InputStream.skip's bounded discard buffer instead of issuing one
    // virtual read per byte. Besides avoiding quadratic class-file scanning,
    // invoking read([BII) on the receiver preserves FilterInputStream's virtual
    // delegation semantics for DataInputStream and other subclasses.
    let capacity = requested.min(2048) as usize;
    let buffer = ctx.new_array(cratonvm_types::ArrayElementType::Byte, capacity);
    let buffer_pin = ctx.pin_native_root(buffer);
    let this_pin = ctx.pin_native_root(this);
    let mut skipped = 0i64;
    while skipped < requested {
        let current_this = ctx.read_native_pin(this_pin, this);
        let current_buffer = ctx.read_native_pin(buffer_pin, buffer);
        let chunk = (requested - skipped).min(capacity as i64) as i32;
        let result = ctx.invoke_virtual(
            current_this,
            "read",
            "([BII)I",
            &[
                Value::Object(Some(current_buffer)),
                Value::Int(0),
                Value::Int(chunk),
            ],
        );
        let read = match result {
            Ok(Some(Value::Int(read))) => read,
            Ok(_) => -1,
            Err(error) => {
                ctx.unpin_native_roots(buffer_pin);
                return Err(error);
            }
        };
        if read <= 0 {
            break;
        }
        skipped += read as i64;
    }
    ctx.unpin_native_roots(buffer_pin);
    Ok(skipped)
}

/// Record one byte read from the delegate into the mark filed under `key`
/// (the stream's weak lock key, resolved by the caller BEFORE the delegate's
/// `read()` ran: that is Java, and the stream may have moved since).
pub fn buffered_input_stream_record_byte(key: usize, byte: u8) {
    let mut marks = buffered_input_stream_marks()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(state) = marks.get_mut(&key) {
        if !state.mark_active {
            return;
        }
        if state.bytes.len() < state.mark_limit {
            state.bytes.push(byte);
        } else {
            state.mark_active = false;
            state.bytes.clear();
            state.replay_pos = 0;
        }
    }
}

pub fn buffered_input_stream_replay_byte(ctx: &dyn NativeContext, this: ObjectRef) -> Option<u8> {
    let key = existing_weak_lock_key(ctx, this)?;
    let mut marks = buffered_input_stream_marks()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let state = marks.get_mut(&key)?;
    if state.replay_pos < state.bytes.len() {
        let b = state.bytes[state.replay_pos];
        state.replay_pos += 1;
        Some(b)
    } else {
        None
    }
}

pub fn buffered_input_stream_can_delegate_bulk(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    let Some(key) = existing_weak_lock_key(ctx, this) else {
        return true;
    };
    buffered_input_stream_marks()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .is_none_or(|state| !state.mark_active)
}

/// `BufferedInputStream.mark(limit)`: start recording (replacing any earlier
/// mark).
pub fn buffered_input_stream_mark(ctx: &dyn NativeContext, this: ObjectRef, limit: usize) {
    let key = lib_side_key(ctx, this);
    buffered_input_stream_marks()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            key,
            BufferedInputStreamMarkState {
                mark_limit: limit,
                mark_active: true,
                replay_pos: 0,
                bytes: Vec::new(),
            },
        );
}

/// `BufferedInputStream.reset()`: rewind the replay to the mark, or `false`
/// when there is no valid mark (the caller throws "Resetting to invalid
/// mark").
pub fn buffered_input_stream_rewind(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    let Some(key) = existing_weak_lock_key(ctx, this) else {
        return false;
    };
    let mut marks = buffered_input_stream_marks()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    match marks.get_mut(&key) {
        Some(state) if state.mark_active => {
            state.replay_pos = 0;
            true
        }
        _ => false,
    }
}

pub fn buffered_input_stream_read_one(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<i32, MethodCallFailed> {
    if let Some(byte) = buffered_input_stream_replay_byte(ctx, this) {
        return Ok(byte as i32);
    }
    let Some(input) = buffered_input_stream_input(ctx, this) else {
        return Ok(-1);
    };
    // gc-common w20-c: the delegate's `read()` is Java, so `this` is stale
    // after it (it used to be hashed there); resolve the mark row first.
    let mark_key = existing_weak_lock_key(ctx, this);
    let value = ctx
        .invoke_virtual(input, "read", "()I", &[])?
        .unwrap_or(Value::Int(-1))
        .as_int()
        .unwrap_or(-1);
    if let (true, Some(key)) = (value >= 0, mark_key) {
        buffered_input_stream_record_byte(key, (value & 0xff) as u8);
    }
    Ok(value)
}

pub fn buffered_input_stream_invalid_mark() -> MethodCallFailed {
    RuntimeError::IOException {
        message: "Resetting to invalid mark".to_string(),
    }
    .into()
}

/// `MessageBytes.toChars()` memo: the `MessageBytes`' weak lock key ->
/// (identity hash of the `strValue` last converted, its length in UTF-16
/// units, that String's address, `gc_address_epoch()` when it was read).
/// gc-common w20-c: keyed by the bare identity hash before. gc-common w33-a:
/// the address and the count validate the String exactly -- its hash alone
/// is shared by another String every 2^20 mints since the 8-byte header.
pub fn message_bytes_to_chars_cache(
) -> &'static std::sync::Mutex<std::collections::HashMap<usize, (i32, usize, usize, u64)>> {
    static STORE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<usize, (i32, usize, usize, u64)>>,
    > = std::sync::OnceLock::new();
    STORE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Forget `mb`'s `toChars()` memo row, if it has one (mints no key).
pub fn message_bytes_forget_chars_memo(ctx: &dyn NativeContext, mb: ObjectRef) {
    message_bytes_forget_chars_key(existing_weak_lock_key(ctx, mb));
}

/// Forget the `toChars()` memo row filed under `key` (a `MessageBytes`' weak
/// lock key, `None` when it was never keyed).
pub fn message_bytes_forget_chars_key(key: Option<usize>) {
    if let Some(key) = key {
        message_bytes_to_chars_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key);
    }
}

/// What one `Mapper.internalMap` call is a function of, as far as the memo is
/// concerned: the Mapper and its host tables, the version String, and the
/// host and URI TEXT.
///
/// gc-common w34-b (`common-w33a-tomcat-mapper-memo-ignores-the-uri-text`):
/// the row used to be matched by the identity hashes of the two `CharChunk`s
/// plus their `[start, end)` offsets. A coyote `Request` is recycled for every
/// request its processor serves and `MessageBytes.toChars()` refills the SAME
/// chunk (and, when it fits, the same `char[]`), so a second request whose URI
/// merely had the first one's LENGTH was served the first one's context and
/// wrapper; and since the 8-byte header two chunks share a hash every 2^20
/// mints, another processor's request could hit the row too. The text is now
/// the key, so neither the chunks' identity nor their offsets matter.
///
/// The `ObjectRef`s are compared by address. That is exact while `epoch`
/// (`gc_address_epoch()` when the building call STARTED; gci i2/wr) is current: no
/// collection has moved or freed anything since (gc-common w19-a), so an equal
/// address is the same object, and nothing else is needed to tell two
/// Mappers, `hosts` arrays or version Strings apart.
pub struct MapperMemoKey {
    pub epoch: u64,
    pub mapper: ObjectRef,
    /// `Mapper.hosts`, replaced copy-on-write by `addHost` / `removeHost`.
    pub hosts: ObjectRef,
    /// `Mapper.defaultHost`, the fallback when no host name matches.
    pub default_host: Option<ObjectRef>,
    pub version: Option<ObjectRef>,
    /// The host chunk's `[start, end)` characters when the row was built.
    pub host_units: Box<[u16]>,
    /// The URI chunk's `[start, end)` characters when the row was built.
    pub uri_units: Box<[u16]>,
}

/// The `ContextVersion` fields a memo row depends on, in the order
/// [`mapper_version_dispatch_state`] reads them: `paused`, then the five
/// dispatch tables Tomcat's `Mapper` replaces copy-on-write when a wrapper or
/// welcome file is added or removed.
pub const MAPPER_VERSION_DISPATCH_FIELDS: [&str; 6] = [
    "paused",
    "exactWrappers",
    "wildcardWrappers",
    "extensionWrappers",
    "welcomeResources",
    "defaultWrapper",
];

/// `(paused, [exactWrappers, wildcardWrappers, extensionWrappers,
/// welcomeResources, defaultWrapper])` of a `ContextVersion`, in one read.
pub fn mapper_version_dispatch_state(
    ctx: &dyn NativeContext,
    version: ObjectRef,
) -> (bool, [Option<ObjectRef>; 5]) {
    let mut f = [Value::Object(None); 6];
    ctx.get_fields_by_name(version, &MAPPER_VERSION_DISPATCH_FIELDS, &mut f);
    let obj = |v: Value| match v {
        Value::Object(o) => o,
        _ => None,
    };
    (
        f[0].as_int().unwrap_or(0) != 0,
        [obj(f[1]), obj(f[2]), obj(f[3]), obj(f[4]), obj(f[5])],
    )
}

pub struct MapperInternalFastEntry {
    /// The inputs the row was computed for; see [`MapperMemoKey`]. Its
    /// `epoch` also guards every raw `ObjectRef` below: nothing roots or
    /// remaps them, so a row is served only while no collection has completed
    /// since the building call started (gc-common w19-a).
    pub key: MapperMemoKey,
    pub mapped_host: ObjectRef,
    /// `mapped_host.contextList` (a `ContextList` is immutable and replaced
    /// wholesale, so its address stands for its `contexts` and `nesting`).
    pub context_list: Option<ObjectRef>,
    pub selected_context: Option<ObjectRef>,
    /// `selected_context.versions`, replaced copy-on-write.
    pub versions: Option<ObjectRef>,
    pub selected_version: Option<ObjectRef>,
    /// `selected_version`'s dispatch tables (see
    /// [`MAPPER_VERSION_DISPATCH_FIELDS`]) when the row was built. The hit
    /// test used to record `exactWrappers` / `wildcardWrappers` and never
    /// compare them, so a servlet mapping added after the row was written was
    /// ignored until the next collection (gc-common w34-b).
    pub dispatch: [Option<ObjectRef>; 5],
    pub context_path_len: usize,
    pub wrapper_len: usize,
    pub host: Option<ObjectRef>,
    pub context: Option<ObjectRef>,
    pub wrapper: Option<ObjectRef>,
    pub wrapper_name: Option<ObjectRef>,
    pub path_match: Option<ObjectRef>,
    pub context_slash_count: i32,
    pub no_context: bool,
    pub default_mapping: bool,
}

/// The current inputs of one `internalMap` call, matched against a row by
/// [`mapper_fast_entry_serves`]. `host` / `uri` are the chunks' `(buff,
/// start, end)`.
pub struct MapperMemoProbe {
    pub epoch: u64,
    pub mapper: ObjectRef,
    pub hosts: ObjectRef,
    pub default_host: Option<ObjectRef>,
    pub version: Option<ObjectRef>,
    pub host: (ObjectRef, usize, usize),
    pub uri: (ObjectRef, usize, usize),
}

/// `units` equals `buff[start..end)` unit for unit. Reads through the bulk
/// reader in fixed-size pieces, so a memo hit allocates nothing; a range
/// running past the array's end is unequal (the reader stops short).
pub fn char_array_range_equals_units(
    ctx: &dyn NativeContext,
    buff: ObjectRef,
    start: usize,
    end: usize,
    units: &[u16],
) -> bool {
    if end < start || end - start != units.len() {
        return false;
    }
    let mut piece = [0u16; 64];
    let mut done = 0usize;
    while done < units.len() {
        let want = (units.len() - done).min(piece.len());
        let read = ctx.read_char_array_into(buff, start + done, &mut piece[..want]);
        if read != want || piece[..want] != units[done..done + want] {
            return false;
        }
        done += want;
    }
    true
}

/// Copy `buff[start..end)` out of the heap for a memo row. Shortened to what
/// was actually read when the range runs past the array's end, which makes
/// the row unmatchable ([`char_array_range_equals_units`] compares lengths
/// first) rather than wrong.
pub fn char_array_range_units(
    ctx: &dyn NativeContext,
    buff: ObjectRef,
    start: usize,
    end: usize,
) -> Box<[u16]> {
    let mut out = vec![0u16; end.saturating_sub(start)];
    let read = ctx.read_char_array_into(buff, start, &mut out);
    out.truncate(read);
    out.into_boxed_slice()
}

/// Does `entry` answer the call `probe` describes? Cheap address compares
/// first, then the host and URI text, then the few Mapper fields Tomcat
/// replaces copy-on-write while it serves requests. Runs no Java and
/// allocates nothing.
pub fn mapper_fast_entry_serves(
    ctx: &dyn NativeContext,
    entry: &MapperInternalFastEntry,
    probe: &MapperMemoProbe,
) -> bool {
    let key = &entry.key;
    if key.epoch != probe.epoch
        || key.mapper != probe.mapper
        || key.hosts != probe.hosts
        || key.default_host != probe.default_host
        || key.version != probe.version
    {
        return false;
    }
    let (uri_buff, uri_start, uri_end) = probe.uri;
    let (host_buff, host_start, host_end) = probe.host;
    if !char_array_range_equals_units(ctx, uri_buff, uri_start, uri_end, &key.uri_units)
        || !char_array_range_equals_units(ctx, host_buff, host_start, host_end, &key.host_units)
    {
        return false;
    }
    let context_list_now = match ctx.get_field_by_name(entry.mapped_host, "contextList") {
        Value::Object(o) => o,
        _ => None,
    };
    if context_list_now != entry.context_list {
        return false;
    }
    if entry.no_context {
        return true;
    }
    let (Some(selected_context), Some(selected_version)) =
        (entry.selected_context, entry.selected_version)
    else {
        return false;
    };
    let versions_now = match ctx.get_field_by_name(selected_context, "versions") {
        Value::Object(o) => o,
        _ => None,
    };
    if versions_now != entry.versions {
        return false;
    }
    let (paused, dispatch) = mapper_version_dispatch_state(ctx, selected_version);
    !paused && dispatch == entry.dispatch
}

/// `native_mapper_internal_map`'s memo: one row per `(vm_identity, Mapper
/// identity hash)` bucket. The VM is part of the key (gc-common w19-a); the
/// hash only picks the bucket -- a row names its Mapper by address and
/// [`mapper_fast_entry_serves`] compares it (gc-common w34-b), so two Mappers
/// sharing a hash only take turns. Rows are `Arc`s so a hit clones a pointer,
/// not the row's text. Dropped per VM at teardown
/// (`forget_vm_native_root_stores`).
pub fn mapper_internal_fast_cache() -> &'static std::sync::Mutex<
    std::collections::HashMap<(usize, i32), std::sync::Arc<MapperInternalFastEntry>>,
> {
    static STORE: std::sync::OnceLock<
        std::sync::Mutex<
            std::collections::HashMap<(usize, i32), std::sync::Arc<MapperInternalFastEntry>>,
        >,
    > = std::sync::OnceLock::new();
    STORE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// File `entry` as `(vm, mapper_key)`'s row, replacing the bucket's previous
/// one.
pub fn mapper_memo_store(vm: usize, mapper_key: i32, entry: MapperInternalFastEntry) {
    mapper_internal_fast_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert((vm, mapper_key), std::sync::Arc::new(entry));
}

pub fn set_char_chunk_from_units(ctx: &mut dyn NativeContext, char_c: ObjectRef, units: &[u16]) {
    // gc-common w20-c: the buffer allocation can collect and move `char_c`,
    // which is written below; re-read it.
    let char_c_pin = ctx.pin_native_root(char_c);
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Char, units.len());
    let char_c = ctx.read_native_pin(char_c_pin, char_c);
    ctx.unpin_native_roots(char_c_pin);
    for (i, unit) in units.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(*unit as i32));
    }
    ctx.set_fields_by_name(
        char_c,
        &[
            ("buff", Value::Object(Some(arr))),
            ("start", Value::Int(0)),
            ("end", Value::Int(units.len() as i32)),
            ("isSet", Value::Int(1)),
            ("hasHashCode", Value::Int(0)),
        ],
    );
}

pub fn set_char_chunk_view(
    ctx: &mut dyn NativeContext,
    char_c: ObjectRef,
    buff: ObjectRef,
    start: usize,
    len: usize,
) {
    ctx.set_fields_by_name(
        char_c,
        &[
            ("buff", Value::Object(Some(buff))),
            ("start", Value::Int(start as i32)),
            ("end", Value::Int((start + len) as i32)),
            ("isSet", Value::Int(1)),
            ("hasHashCode", Value::Int(0)),
        ],
    );
}

pub fn message_bytes_set_chars(
    ctx: &mut dyn NativeContext,
    mb: ObjectRef,
    buff: ObjectRef,
    start: usize,
    len: usize,
) {
    if let Value::Object(Some(char_c)) = ctx.get_field_by_name(mb, "charC") {
        set_char_chunk_view(ctx, char_c, buff, start, len);
    }
    ctx.set_fields_by_name(
        mb,
        &[
            ("type", Value::Int(3)),
            ("hasHashCode", Value::Int(0)),
            ("hasLongValue", Value::Int(0)),
        ],
    );
    message_bytes_forget_chars_memo(ctx, mb);
}

pub fn message_bytes_set_string_object(
    ctx: &mut dyn NativeContext,
    mb: ObjectRef,
    value: Option<ObjectRef>,
) {
    ctx.set_fields_by_name(
        mb,
        &[
            ("strValue", Value::Object(value)),
            ("hasHashCode", Value::Int(0)),
            ("hasLongValue", Value::Int(0)),
            ("type", Value::Int(if value.is_some() { 1 } else { 0 })),
        ],
    );
    message_bytes_forget_chars_memo(ctx, mb);
}

pub fn char_chunk_parts(ctx: &dyn NativeContext, this: ObjectRef) -> Option<(ObjectRef, usize, usize)> {
    let mut fields = [Value::Object(None); 3];
    ctx.get_fields_by_name(this, &["buff", "start", "end"], &mut fields);
    let buff = match fields[0] {
        Value::Object(Some(o)) => o,
        _ => return None,
    };
    let start = fields[1].as_int().unwrap_or(0).max(0) as usize;
    let end = fields[2].as_int().unwrap_or(0).max(0) as usize;
    if end < start {
        return None;
    }
    Some((buff, start, end))
}

/// The `isSet` half of `AbstractChunk.isNull()` (`end > 0 ? false : !isSet`).
///
/// The "no data" state is NOT the same as "no buffer". `AbstractChunk
/// ::recycle()` clears `isSet`/`start`/`end` but deliberately keeps `buff`
/// alive for reuse, so a recycled chunk still has a non-null buffer while
/// being `isNull()`. Any native reproducing a Tomcat method's null-vs-empty
/// contract must consult this rather than the `buff == null` test in
/// [`char_chunk_parts`].
///
/// Callers must short-circuit on `end > 0` themselves before calling this, so
/// that the extra field read stays off the non-empty (hot) path — see
/// [`native_char_chunk_to_string`].
pub fn char_chunk_is_set(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    ctx.get_field_by_name(this, "isSet").as_int().unwrap_or(0) != 0
}

/// Whether the chunk's `buff` field is non-null.
///
/// `CharChunk`'s comparison methods all start with `char[] c = buff;` and
/// treat `c == null` as "no match" — a weaker condition than `isNull()`, and
/// the one that decides whether a null String argument reaches `s.length()`.
pub fn char_chunk_has_buffer(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    matches!(ctx.get_field_by_name(this, "buff"), Value::Object(Some(_)))
}

/// The NPE Tomcat's own `CharChunk.equals` / `equalsIgnoreCase` /
/// `startsWith` / `startsWithIgnoreCase` bytecode raises for a null `String`
/// argument: `s.length()` on the parameter `s` (Tomcat's jars carry a
/// `LocalVariableTable`), so HotSpot's JEP 358 text is exactly
/// `Cannot invoke "String.length()" because "s" is null`. This used to append
/// ` (CharChunk.<method>)`, a suffix no HotSpot message has (interpreter round
/// i4 wave 98, lane L9; probe `L9W98TomcatCharChunkNpe.java`).
/// `_method` stays for the call sites' readability.
pub fn char_chunk_null_arg_npe(_method: &str) -> MethodCallFailed {
    RuntimeError::NullPointerException {
        message: Some("Cannot invoke \"String.length()\" because \"s\" is null".to_string()),
    }
    .into()
}

pub fn char_chunk_char_at(ctx: &dyn NativeContext, buff: ObjectRef, index: usize) -> u16 {
    ctx.get_array_element(buff, index).as_int().unwrap_or(0) as u16
}

/// Are the `org/apache/catalina/mapper/Mapper` native shadows registered?
/// Default-ON; `CRATONVM_TOMCAT_MAPPER_NATIVES=0` runs Tomcat's own bytecode.
/// See the registration site for why the switch exists.
pub fn mapper_natives_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(
        || match Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => true,
        },
    )
}

/// `CRATONVM_DBG_MAPPER=1` reports `native_mapper_internal_map`'s memo hit rate.
///
/// Known-issue tomcat/32 recorded the memo's effectiveness as unmeasured and
/// guessed it was permanently unusable for a host that selects a context but no
/// version. The counters below exist so that question is answered by a number
/// rather than by reading the validity condition.
pub fn mapper_dbg_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(
        || match Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => false,
        },
    )
}

pub static MAPPER_MEMO_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static MAPPER_MEMO_MISSES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Record one `internalMap` outcome and print a running tally every 250k calls.
pub fn mapper_memo_record(hit: bool) {
    use std::sync::atomic::Ordering;
    let counter = if hit {
        &MAPPER_MEMO_HITS
    } else {
        &MAPPER_MEMO_MISSES
    };
    counter.fetch_add(1, Ordering::Relaxed);
    let hits = MAPPER_MEMO_HITS.load(Ordering::Relaxed);
    let misses = MAPPER_MEMO_MISSES.load(Ordering::Relaxed);
    let total = hits + misses;
    if total % 250_000 == 0 {
        eprintln!(
            "[DBG_MAPPER] internalMap calls={total} memo_hits={hits} memo_misses={misses} \
             hit_rate={:.4}",
            hits as f64 / total as f64
        );
    }
}

/// Copy a `CharChunk`'s `[start, end)` characters out of the heap ONCE, using
/// the bulk reader, so a caller that scans the same range repeatedly does not
/// pay a `get_array_element` VM round trip per character per comparison.
///
/// Why this exists (tomcat doc 04, 2026-07-27): `Mapper.map` is served by a
/// Rust shadow, and its binary searches used to run
/// `char_chunk_parts` (3 `get_field_by_name`) + a per-character
/// `get_array_element` for EVERY probe. `TestMapperPerformance` does 10^6
/// `mapper.map()` calls against an absolute 5 s budget and measured 3.3 ms/1000
/// (easiest hostname) to 7.9 ms/1000 (`iowejoiejfoiew`) — the latter blowing
/// the budget — against HotSpot's 0.10/0.37. The chunk being searched is
/// immutable for the duration of one search, so one bulk copy replaces all of
/// it.
///
/// Returns `None` exactly when [`char_chunk_parts`] does (absent `buff`,
/// inverted range), so callers keep their existing failure behaviour. A range
/// running past the array end is truncated by `read_char_array_into`, and the
/// snapshot is shortened to what was actually read — never zero-filled, which
/// would compare as U+0000 characters that are not in the heap.
pub fn char_chunk_snapshot_range(
    ctx: &dyn NativeContext,
    chunk: ObjectRef,
    start: usize,
    end: usize,
) -> Option<Vec<u16>> {
    let (buff, _, _) = char_chunk_parts(ctx, chunk)?;
    let len = end.saturating_sub(start);
    let mut out = vec![0u16; len];
    let read = ctx.read_char_array_into(buff, start, &mut out);
    out.truncate(read);
    Some(out)
}

/// `mapper_compare_chunk_to_string` over an already-snapshotted range.
///
/// Byte-identical ordering to the chunk-reading version: compare UTF-16 code
/// units up to the shorter length, then order by length. The name is consumed
/// as an `encode_utf16()` iterator rather than collected, so a binary-search
/// probe allocates nothing.
pub fn compare_units_to_str(units: &[u16], name: &str, ignore_case: bool) -> i32 {
    let mut name_units = name.encode_utf16();
    for &actual in units.iter() {
        let Some(expected) = name_units.next() else {
            // The name ran out first: it is a strict prefix of the chunk.
            return 1;
        };
        let (a, e) = if ignore_case {
            (ascii_lower_char(actual), ascii_lower_char(expected))
        } else {
            (actual, expected)
        };
        if a > e {
            return 1;
        }
        if a < e {
            return -1;
        }
    }
    // The chunk ran out (or both did). Longer name means the chunk sorts first.
    if name_units.next().is_some() {
        -1
    } else {
        0
    }
}

/// `char_chunk_range_equals_string` over an already-snapshotted range.
pub fn units_equal_str(units: &[u16], s: &str, ignore_case: bool) -> bool {
    let mut it = s.encode_utf16();
    for &actual in units {
        let Some(expected) = it.next() else {
            return false;
        };
        if ignore_case {
            if ascii_lower_char(actual) != ascii_lower_char(expected) {
                return false;
            }
        } else if actual != expected {
            return false;
        }
    }
    it.next().is_none()
}

pub fn char_chunk_nth_slash(
    ctx: &dyn NativeContext,
    buff: ObjectRef,
    start: usize,
    end: usize,
    n: usize,
) -> usize {
    let mut slash_count = 0usize;
    let mut pos = start;
    while pos < end {
        if char_chunk_char_at(ctx, buff, pos) == b'/' as u16 {
            slash_count += 1;
            if slash_count == n {
                return pos;
            }
        }
        pos += 1;
    }
    end
}

pub fn char_chunk_last_slash(
    ctx: &dyn NativeContext,
    buff: ObjectRef,
    start: usize,
    end: usize,
) -> usize {
    let mut pos = end;
    while pos > start {
        pos -= 1;
        if char_chunk_char_at(ctx, buff, pos) == b'/' as u16 {
            return pos;
        }
    }
    start
}

pub fn char_chunk_range_starts_with(
    ctx: &dyn NativeContext,
    buff: ObjectRef,
    start: usize,
    end: usize,
    s: &str,
    pos: usize,
    ignore_case: bool,
) -> bool {
    let units: Vec<u16> = s.encode_utf16().collect();
    if units.len() + pos > end.saturating_sub(start) {
        return false;
    }
    for (i, expected) in units.iter().enumerate() {
        let actual = char_chunk_char_at(ctx, buff, start + pos + i);
        if ignore_case {
            if ascii_lower_char(actual) != ascii_lower_char(*expected) {
                return false;
            }
        } else if actual != *expected {
            return false;
        }
    }
    true
}

// `char_chunk_range_equals_string` was removed with its last caller
// (`mapper_exact_find_chunk_range`) — see `units_equal_str`, which does the
// same comparison against the range snapshot that caller already holds.

pub const MAPPING_MATCH_CLASS: &str = "jakarta/servlet/http/MappingMatch";

/// Run `MappingMatch.<clinit>` -- when it has not yet completed in this VM --
/// BEFORE a mapper native binds its arguments, with every object argument
/// pinned across it, and answer the arguments re-read.
///
/// gc-common w20-c (`common-w19a-lib-rs-identity-hash-keyed-side-tables`):
/// [`mapping_match_static`] initialises the class on first use, and that is
/// Java, so a GC point. The mapper natives call it late, after binding
/// `mapping_data`, the URI chunk, the selected version and the rest as raw
/// locals, and kept using them after it: a VM's first mapping could write
/// through (and memoise) addresses a collection during `<clinit>` had moved.
/// Hoisting the one-time initialisation here makes every later
/// `mapping_match_static` call Java-free.
///
/// Once a thread has seen the initialisation succeed in its VM (the thread's
/// first mapping), this is one thread-local read and `args` comes back
/// borrowed: the hot `internalMap` path pays no pins and no class lookup. A
/// by-name `class_id_by_name` probe would not do, because `MappingMatch` is
/// usually defined by a web-application loader, where a loader-blind lookup
/// misses; `ensure_class_initialized` (the lookup `mapping_match_static`
/// itself uses) is the one that finds it. VM identities are never reused, so
/// a stale thread-local value can only cost one more pinned call.
pub fn mapper_args_with_mapping_match_ready<'a>(
    ctx: &mut dyn NativeContext,
    args: &'a [Value],
) -> std::borrow::Cow<'a, [Value]> {
    thread_local! {
        static MAPPING_MATCH_READY_IN_VM: std::cell::Cell<usize> =
            const { std::cell::Cell::new(0) };
    }
    let vm = ctx.vm_identity();
    if vm != 0 && MAPPING_MATCH_READY_IN_VM.with(|c| c.get()) == vm {
        return std::borrow::Cow::Borrowed(args);
    }
    let mut pins: Vec<Option<usize>> = Vec::with_capacity(args.len());
    for value in args {
        pins.push(match value {
            Value::Object(Some(o)) => Some(ctx.pin_native_root(*o)),
            _ => None,
        });
    }
    if ctx.ensure_class_initialized(MAPPING_MATCH_CLASS).is_ok() && vm != 0 {
        MAPPING_MATCH_READY_IN_VM.with(|c| c.set(vm));
    }
    let mut fresh: Vec<Value> = Vec::with_capacity(args.len());
    for (value, pin) in args.iter().zip(&pins) {
        fresh.push(match (value, pin) {
            (Value::Object(Some(o)), Some(pin)) => {
                Value::Object(Some(ctx.read_native_pin(*pin, *o)))
            }
            _ => *value,
        });
    }
    if let Some(&base) = pins.iter().flatten().next() {
        ctx.unpin_native_roots(base);
    }
    std::borrow::Cow::Owned(fresh)
}

/// The `MappingMatch` constant `name`. Runs `MappingMatch.<clinit>` (Java, a GC
/// point) when the class is not yet initialised: a native that holds raw
/// references across this call first passes its arguments through
/// [`mapper_args_with_mapping_match_ready`].
pub fn mapping_match_static(ctx: &mut dyn NativeContext, name: &str) -> Option<ObjectRef> {
    // `Mapper` can be reached before any Java bytecode has touched
    // `MappingMatch`. Loading its metadata alone leaves the enum constants
    // null, which makes `ApplicationMapping` report a null mapping type and
    // Spring MVC treat an otherwise valid servlet-path request as unmapped.
    let class_id = ctx.ensure_class_initialized(MAPPING_MATCH_CLASS).ok()?;
    let idx = ctx.static_field_index_by_name(class_id, name)?;
    match ctx.get_static_field(class_id, idx) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

/// Per-char lowercase exactly as `Mapper.compareIgnoreCase` does it:
/// `c > 0xFF ? Character.toLowerCase(c) : Ascii.toLower(c)`, decided
/// independently for each side of the comparison. `Ascii.toLower` is an
/// ASCII-only table (`A`-`Z` only), so Latin-1 letters like `À` are
/// deliberately left alone.
///
/// NOTE: `CharChunk.equalsIgnoreCase`/`startsWithIgnoreCase` use a *paired*
/// branch instead (`if (c1 > 0xFF || c2 > 0xFF)`), which is not the same
/// function — use [`char_chunk_chars_equal_ignore_case`] for those.
pub fn ascii_lower_char(ch: u16) -> u16 {
    if ch <= 0xff {
        let b = ch as u8;
        if b.is_ascii_uppercase() {
            (b + 32) as u16
        } else {
            ch
        }
    } else {
        unicode_lower_char(ch)
    }
}

/// `Character.toLowerCase(char)` for a single UTF-16 unit.
pub fn unicode_lower_char(ch: u16) -> u16 {
    char::from_u32(ch as u32)
        .and_then(|c| c.to_lowercase().next())
        .map(|c| c as u32 as u16)
        .unwrap_or(ch)
}

/// Case-insensitive char equality as `CharChunk.equalsIgnoreCase` and
/// `CharChunk.startsWithIgnoreCase` define it:
///
/// ```java
/// if (c1 > 0xFF || c2 > 0xFF) {
///     if (Character.toLowerCase(c1) != Character.toLowerCase(c2)) { return false; }
/// } else if (Ascii.toLower(c1) != Ascii.toLower(c2)) { return false; }
/// ```
///
/// The `||` matters: one side being non-Latin-1 pulls the *other* side onto
/// `Character.toLowerCase` too. Lowering each side independently (the
/// `Mapper` rule, [`ascii_lower_char`]) makes e.g. `U+212B ANGSTROM SIGN`
/// vs `U+00C5 Å` compare unequal, where Tomcat folds both to `U+00E5 å`.
pub fn char_chunk_chars_equal_ignore_case(c1: u16, c2: u16) -> bool {
    if c1 > 0xff || c2 > 0xff {
        unicode_lower_char(c1) == unicode_lower_char(c2)
    } else {
        ascii_lower_char(c1) == ascii_lower_char(c2)
    }
}

pub fn char_chunk_equals_string(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    s: &str,
    ignore_case: bool,
) -> bool {
    let Some((buff, start, end)) = char_chunk_parts(ctx, this) else {
        return false;
    };
    let units: Vec<u16> = s.encode_utf16().collect();
    if end - start != units.len() {
        return false;
    }
    for (i, expected) in units.iter().enumerate() {
        let actual = char_chunk_char_at(ctx, buff, start + i);
        if ignore_case {
            if !char_chunk_chars_equal_ignore_case(actual, *expected) {
                return false;
            }
        } else if actual != *expected {
            return false;
        }
    }
    true
}

pub fn char_chunk_starts_with(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    s: &str,
    pos: usize,
    ignore_case: bool,
) -> bool {
    let Some((buff, start, end)) = char_chunk_parts(ctx, this) else {
        return false;
    };
    let units: Vec<u16> = s.encode_utf16().collect();
    if units.len() + pos > end - start {
        return false;
    }
    for (i, expected) in units.iter().enumerate() {
        let actual = char_chunk_char_at(ctx, buff, start + pos + i);
        if ignore_case {
            if !char_chunk_chars_equal_ignore_case(actual, *expected) {
                return false;
            }
        } else if actual != *expected {
            return false;
        }
    }
    true
}

/// Restricts the native Response fast path to characters that require no
/// Tomcat URL encoding or path normalization.
pub fn simple_absolute_component(text: &str, allow_slash: bool) -> bool {
    !text.is_empty()
        && text.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(b, b'.' | b'-' | b'_' | b'~')
                || (allow_slash && b == b'/')
        })
}

/// Fast path for Tomcat's overwhelmingly common simple relative redirect.
/// All non-trivial forms execute the original bytecode, preserving Tomcat's
/// full escaping and normalization contract.
pub fn native_response_to_absolute_fallback(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    args: &[Value],
) -> MethodCallResult {
    ctx.invoke_virtual_bytecode_only(
        this,
        "toAbsolute",
        "(Ljava/lang/String;)Ljava/lang/String;",
        &args[1..],
    )
}

pub fn native_response_request_string(
    ctx: &mut dyn NativeContext,
    request: ObjectRef,
    name: &str,
) -> Option<String> {
    match ctx
        .invoke_virtual(request, name, "()Ljava/lang/String;", &[])
        .ok()?
    {
        Some(Value::Object(Some(o))) => ctx.read_string(o),
        _ => None,
    }
}

pub fn native_response_to_absolute(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let location = match args.get(1) {
        Some(Value::Object(Some(o))) => match ctx.read_string(*o) {
            Some(s) => s,
            None => return native_response_to_absolute_fallback(ctx, this, args),
        },
        Some(Value::Object(None)) => return Ok(Some(Value::Object(None))),
        _ => return native_response_to_absolute_fallback(ctx, this, args),
    };
    if !simple_absolute_component(&location, false)
        || location.starts_with('.')
        || !location.contains('.')
    {
        return native_response_to_absolute_fallback(ctx, this, args);
    }
    let request = match ctx.get_field_by_name(this, "request") {
        Value::Object(Some(o)) => o,
        _ => return native_response_to_absolute_fallback(ctx, this, args),
    };
    let scheme = match native_response_request_string(ctx, request, "getScheme") {
        Some(v) => v,
        None => return native_response_to_absolute_fallback(ctx, this, args),
    };
    let server = match native_response_request_string(ctx, request, "getServerName") {
        Some(v) => v,
        None => return native_response_to_absolute_fallback(ctx, this, args),
    };
    let path = match native_response_request_string(ctx, request, "getDecodedRequestURI") {
        Some(v) => v,
        None => return native_response_to_absolute_fallback(ctx, this, args),
    };
    let port = match ctx.invoke_virtual(request, "getServerPort", "()I", &[])? {
        Some(Value::Int(v)) if v > 0 => v,
        _ => return native_response_to_absolute_fallback(ctx, this, args),
    };
    if !simple_absolute_component(&scheme, false)
        || !simple_absolute_component(&server, false)
        || !path.starts_with('/')
        || !simple_absolute_component(path.trim_start_matches('/'), true)
        || path.contains("//")
        || path.contains("/./")
        || path.contains("/../")
    {
        return native_response_to_absolute_fallback(ctx, this, args);
    }
    let Some(slash) = path.rfind('/') else {
        return native_response_to_absolute_fallback(ctx, this, args);
    };
    let default_port = (scheme == "http" && port == 80) || (scheme == "https" && port == 443);
    let mut absolute =
        String::with_capacity(scheme.len() + server.len() + path.len() + location.len() + 16);
    absolute.push_str(&scheme);
    absolute.push_str("://");
    absolute.push_str(&server);
    if !default_port {
        absolute.push(':');
        absolute.push_str(&port.to_string());
    }
    absolute.push_str(&path[..slash + 1]);
    absolute.push_str(&location);
    Ok(Some(Value::Object(Some(
        ctx.create_string_uninterned_gc_safe(&absolute),
    ))))
}

/// Fast native materialization for Tomcat's `CharChunk.toString()`.
///
/// The method is already listed as a force-native hot path, but previously
/// had no matching registration and consequently paid StringCache plus Java
/// bytecode dispatch for every response URL. Dynamic output stays uninterned.
///
/// The Java body this replaces is a three-way decision, not a two-way one:
///
/// ```java
/// if (isNull()) { return null; }
/// else if (end - start == 0) { return ""; }
/// return StringCache.toString(this);
/// ```
///
/// and `isNull()` is `end > 0 ? false : !isSet` — a *state* test, not a
/// buffer-null test. `recycle()` clears `isSet`/`start`/`end` but keeps
/// `buff`, so keying "null" off `buff == null` alone reported a recycled
/// chunk as empty-but-set and returned `""` where Tomcat returns `null`
/// (`TestCharChunk.testToString`,
/// `25-charchunk-tostring-null-vs-empty-FIXED.md`).
///
/// PERF: `end` is tested first and `isSet` is read **only** when `end == 0`,
/// so the ordinary non-empty path still performs exactly three
/// `get_field_by_name` lookups (`end`, `buff`, `start`) — the same count as
/// before the null-vs-empty fix. This method is hot enough (every response
/// URL) that adding an unconditional fourth name-keyed field lookup here
/// would be a measurable regression, so do not hoist the `isSet` read.
pub fn native_char_chunk_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let end = ctx
        .get_field_by_name(this, "end")
        .as_int()
        .unwrap_or(0)
        .max(0) as usize;
    if end == 0 {
        // `isNull()` reduces to `!isSet` here. A never-set or recycled chunk
        // is null; a chunk that IS set but holds no characters is "".
        return Ok(Some(if char_chunk_is_set(ctx, this) {
            Value::Object(Some(ctx.create_string("")))
        } else {
            Value::Object(None)
        }));
    }
    // `end > 0` => not `isNull()`, so from here every outcome is a String.
    let buff = match ctx.get_field_by_name(this, "buff") {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(Value::Object(Some(ctx.create_string(""))))),
    };
    let start = ctx
        .get_field_by_name(this, "start")
        .as_int()
        .unwrap_or(0)
        .max(0) as usize;
    if end <= start {
        return Ok(Some(Value::Object(Some(ctx.create_string("")))));
    }
    let mut units = Vec::with_capacity(end - start);
    for i in start..end {
        units.push(ctx.get_array_element(buff, i).as_int().unwrap_or(0) as u16);
    }
    let text = String::from_utf16_lossy(&units);
    Ok(Some(Value::Object(Some(
        ctx.create_string_uninterned_gc_safe(&text),
    ))))
}

pub fn native_char_chunk_equals_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let s = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        // `equals(String)` computes `len` from the chunk first and then
        // short-circuits on `c == null || len != s.length()`: a null
        // argument is silently `false` when there is no buffer, but an NPE
        // once there is one. Returning `false` unconditionally swallowed a
        // real NPE the Tomcat bytecode raises.
        _ => {
            return if char_chunk_has_buffer(ctx, this) {
                Err(char_chunk_null_arg_npe("equals"))
            } else {
                Ok(Some(Value::Int(0)))
            }
        }
    };
    Ok(Some(Value::Int(
        if char_chunk_equals_string(ctx, this, &s, false) {
            1
        } else {
            0
        },
    )))
}

pub fn native_char_chunk_equals_ignore_case(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let s = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        // Same short-circuit shape as `equals(String)` above.
        _ => {
            return if char_chunk_has_buffer(ctx, this) {
                Err(char_chunk_null_arg_npe("equalsIgnoreCase"))
            } else {
                Ok(Some(Value::Int(0)))
            }
        }
    };
    Ok(Some(Value::Int(
        if char_chunk_equals_string(ctx, this, &s, true) {
            1
        } else {
            0
        },
    )))
}

pub fn native_char_chunk_starts_with(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let s = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        // Unlike `equals`, `startsWith` reads `s.length()` into a local
        // BEFORE the `c == null` test, so a null argument always NPEs —
        // buffer or no buffer.
        _ => return Err(char_chunk_null_arg_npe("startsWith")),
    };
    Ok(Some(Value::Int(
        if char_chunk_starts_with(ctx, this, &s, 0, false) {
            1
        } else {
            0
        },
    )))
}

pub fn native_char_chunk_starts_with_ignore_case(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let s = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        // Same pre-null-check `s.length()` as `startsWith` above.
        _ => return Err(char_chunk_null_arg_npe("startsWithIgnoreCase")),
    };
    let pos = args.get(2).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    Ok(Some(Value::Int(
        if char_chunk_starts_with(ctx, this, &s, pos, true) {
            1
        } else {
            0
        },
    )))
}

pub fn mapper_map_element_name(ctx: &dyn NativeContext, elem: ObjectRef) -> String {
    match ctx.get_field_by_name(elem, "name") {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    }
}

// `mapper_compare_chunk_to_string` was removed with its last callers (the two
// binary searches below) — `compare_units_to_str` is the same comparison
// against the range snapshot they now take once per search instead of once per
// probe.

pub fn mapper_find_chunk_range(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    chunk: ObjectRef,
    start: usize,
    end: usize,
    ignore_case: bool,
) -> i32 {
    // Snapshot the searched range ONCE (see `char_chunk_snapshot_range`); the
    // chunk cannot change under a binary search, and re-reading it per probe
    // was the dominant cost of `Mapper.map` (tomcat doc 04).
    match char_chunk_snapshot_range(ctx, chunk, start, end) {
        Some(units) => mapper_find_units(ctx, arr, &units, ignore_case),
        // `mapper_compare_chunk_to_string` treats an unreadable chunk as
        // "less than any non-empty name", which makes the very first probe
        // below return -1 unless element 0's name is empty. Preserve that.
        None => mapper_find_units(ctx, arr, &[], ignore_case),
    }
}

/// Binary search shared by [`mapper_find_chunk_range`] and its exact-match
/// sibling, over a snapshotted key. Same probe sequence and same return
/// convention as the original chunk-reading loop ("index of the greatest
/// element `<=` key, or -1").
pub fn mapper_find_units(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    units: &[u16],
    ignore_case: bool,
) -> i32 {
    let len = ctx.array_length(arr);
    if len == 0 {
        return -1;
    }
    let mut low = 0usize;
    let mut high = len - 1;
    let Value::Object(Some(first)) = ctx.get_array_element(arr, 0) else {
        return -1;
    };
    if compare_units_to_str(units, &mapper_map_element_name(ctx, first), ignore_case) < 0 {
        return -1;
    }
    if high == 0 {
        return 0;
    }
    loop {
        let mid = (low + high) >> 1;
        let Value::Object(Some(elem)) = ctx.get_array_element(arr, mid) else {
            return low as i32;
        };
        let cmp = compare_units_to_str(units, &mapper_map_element_name(ctx, elem), ignore_case);
        if cmp > 0 {
            low = mid;
        } else if cmp == 0 {
            return mid as i32;
        } else {
            high = mid;
        }
        if high - low == 1 {
            let Value::Object(Some(elem)) = ctx.get_array_element(arr, high) else {
                return low as i32;
            };
            let cmp = compare_units_to_str(units, &mapper_map_element_name(ctx, elem), ignore_case);
            return if cmp < 0 { low as i32 } else { high as i32 };
        }
    }
}

pub fn mapper_find_string(ctx: &dyn NativeContext, arr: ObjectRef, name: &str) -> i32 {
    let len = ctx.array_length(arr);
    if len == 0 {
        return -1;
    }
    let mut low = 0usize;
    let mut high = len - 1;
    let Value::Object(Some(first)) = ctx.get_array_element(arr, 0) else {
        return -1;
    };
    if name < mapper_map_element_name(ctx, first).as_str() {
        return -1;
    }
    if high == 0 {
        return 0;
    }
    loop {
        let mid = (low + high) >> 1;
        let Value::Object(Some(elem)) = ctx.get_array_element(arr, mid) else {
            return low as i32;
        };
        let elem_name = mapper_map_element_name(ctx, elem);
        match name.cmp(elem_name.as_str()) {
            std::cmp::Ordering::Greater => low = mid,
            std::cmp::Ordering::Equal => return mid as i32,
            std::cmp::Ordering::Less => high = mid,
        }
        if high - low == 1 {
            let Value::Object(Some(elem)) = ctx.get_array_element(arr, high) else {
                return low as i32;
            };
            let elem_name = mapper_map_element_name(ctx, elem);
            return if name < elem_name.as_str() {
                low as i32
            } else {
                high as i32
            };
        }
    }
}

pub fn mapper_exact_find_chunk_range(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    chunk: ObjectRef,
    start: usize,
    end: usize,
    ignore_case: bool,
) -> Option<ObjectRef> {
    // One snapshot serves both the binary search and the final exact-equality
    // check — the previous code re-read the chunk for each.
    let units = char_chunk_snapshot_range(ctx, chunk, start, end)?;
    let idx = mapper_find_units(ctx, arr, &units, ignore_case);
    if idx < 0 {
        return None;
    }
    let elem = match ctx.get_array_element(arr, idx as usize) {
        Value::Object(Some(o)) => o,
        _ => return None,
    };
    if units_equal_str(&units, &mapper_map_element_name(ctx, elem), ignore_case) {
        Some(elem)
    } else {
        None
    }
}

pub fn native_mapper_find_chunk(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let chunk = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let Some((_, start, end)) = char_chunk_parts(ctx, chunk) else {
        return Ok(Some(Value::Int(-1)));
    };
    Ok(Some(Value::Int(mapper_find_chunk_range(
        ctx, arr, chunk, start, end, false,
    ))))
}

pub fn native_mapper_find_chunk_range(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let chunk = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let start = args.get(2).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    let end = args.get(3).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    Ok(Some(Value::Int(mapper_find_chunk_range(
        ctx, arr, chunk, start, end, false,
    ))))
}

pub fn native_mapper_find_ignore_case(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let chunk = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let Some((_, start, end)) = char_chunk_parts(ctx, chunk) else {
        return Ok(Some(Value::Int(-1)));
    };
    Ok(Some(Value::Int(mapper_find_chunk_range(
        ctx, arr, chunk, start, end, true,
    ))))
}

pub fn native_mapper_find_ignore_case_range(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let chunk = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let start = args.get(2).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    let end = args.get(3).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    Ok(Some(Value::Int(mapper_find_chunk_range(
        ctx, arr, chunk, start, end, true,
    ))))
}

pub fn native_mapper_find_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let name = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => return Ok(Some(Value::Int(-1))),
    };
    Ok(Some(Value::Int(mapper_find_string(ctx, arr, &name))))}

pub fn native_mapper_exact_find_chunk(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let chunk = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let idx = native_mapper_find_chunk(ctx, args)?
        .and_then(|v| v.as_int())
        .unwrap_or(-1);
    if idx < 0 {
        return Ok(Some(Value::Object(None)));
    }
    let elem = ctx.get_array_element(arr, idx as usize);
    if let Value::Object(Some(o)) = elem {
        if char_chunk_equals_string(ctx, chunk, &mapper_map_element_name(ctx, o), false) {
            return Ok(Some(elem));
        }
    }
    Ok(Some(Value::Object(None)))
}

pub fn native_mapper_exact_find_ignore_case(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let chunk = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let idx = native_mapper_find_ignore_case(ctx, args)?
        .and_then(|v| v.as_int())
        .unwrap_or(-1);
    if idx < 0 {
        return Ok(Some(Value::Object(None)));
    }
    let elem = ctx.get_array_element(arr, idx as usize);
    if let Value::Object(Some(o)) = elem {
        if char_chunk_equals_string(ctx, chunk, &mapper_map_element_name(ctx, o), true) {
            return Ok(Some(elem));
        }
    }
    Ok(Some(Value::Object(None)))
}

pub fn native_mapper_exact_find_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let name = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };
    let idx = mapper_find_string(ctx, arr, &name);
    if idx < 0 {
        return Ok(Some(Value::Object(None)));
    }
    let elem = ctx.get_array_element(arr, idx as usize);
    if let Value::Object(Some(o)) = elem {
        if mapper_map_element_name(ctx, o) == name {
            return Ok(Some(elem));
        }
    }
    Ok(Some(Value::Object(None)))
}

pub fn native_mapper_internal_map_wildcard_wrapper(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let args_now = mapper_args_with_mapping_match_ready(ctx, args);
    let args: &[Value] = &args_now;
    let arr = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let nesting = args.get(2).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    let path = match args.get(3) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let mapping_data = match args.get(4) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };

    let Some((buff, start, original_end)) = char_chunk_parts(ctx, path) else {
        return Ok(None);
    };
    let full_len = original_end.saturating_sub(start);
    let mut search_end = original_end;
    let mut slash = usize::MAX;
    let mut idx = mapper_find_chunk_range(ctx, arr, path, start, search_end, false);
    let mut found: Option<(usize, usize)> = None;

    while idx >= 0 {
        let elem = match ctx.get_array_element(arr, idx as usize) {
            Value::Object(Some(o)) => o,
            _ => break,
        };
        let name = mapper_map_element_name(ctx, elem);
        let name_len = name.encode_utf16().count();
        if char_chunk_range_starts_with(ctx, buff, start, original_end, &name, 0, false)
            && (full_len == name_len
                || char_chunk_range_starts_with(
                    ctx,
                    buff,
                    start,
                    original_end,
                    "/",
                    name_len,
                    true,
                ))
        {
            found = Some((idx as usize, name_len));
            break;
        }

        slash = if slash == usize::MAX {
            char_chunk_nth_slash(ctx, buff, start, search_end, nesting + 1)
        } else {
            char_chunk_last_slash(ctx, buff, start, search_end)
        };
        if slash == search_end {
            break;
        }
        search_end = slash;
        idx = mapper_find_chunk_range(ctx, arr, path, start, search_end, false);
    }

    let Some((idx, wrapper_len)) = found else {
        return Ok(None);
    };
    let wrapper = match ctx.get_array_element(arr, idx) {
        Value::Object(Some(o)) => o,
        _ => return Ok(None),
    };
    let name_obj = match ctx.get_field_by_name(wrapper, "name") {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    };
    if let Value::Object(Some(wrapper_path)) = ctx.get_field_by_name(mapping_data, "wrapperPath") {
        message_bytes_set_string_object(ctx, wrapper_path, name_obj);
    }
    if full_len > wrapper_len {
        if let Value::Object(Some(path_info)) = ctx.get_field_by_name(mapping_data, "pathInfo") {
            message_bytes_set_chars(
                ctx,
                path_info,
                buff,
                start + wrapper_len,
                full_len - wrapper_len,
            );
        }
    }
    if let Value::Object(Some(request_path)) = ctx.get_field_by_name(mapping_data, "requestPath") {
        message_bytes_set_chars(ctx, request_path, buff, start, full_len);
    }
    let wrapper_object = match ctx.get_field_by_name(wrapper, "object") {
        Value::Object(o) => o,
        _ => None,
    };
    ctx.set_field_by_name(mapping_data, "wrapper", Value::Object(wrapper_object));
    ctx.set_field_by_name(
        mapping_data,
        "jspWildCard",
        Value::Int(
            ctx.get_field_by_name(wrapper, "jspWildCard")
                .as_int()
                .unwrap_or(0),
        ),
    );
    if let Some(path_match) = mapping_match_static(ctx, "PATH") {
        ctx.set_field_by_name(mapping_data, "matchType", Value::Object(Some(path_match)));
    }
    Ok(None)
}

pub fn mapper_wildcard_match(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    nesting: usize,
    path: ObjectRef,
) -> Option<(ObjectRef, usize)> {
    let (buff, start, original_end) = char_chunk_parts(ctx, path)?;
    let full_len = original_end.saturating_sub(start);
    let mut search_end = original_end;
    let mut slash = usize::MAX;
    let mut idx = mapper_find_chunk_range(ctx, arr, path, start, search_end, false);
    while idx >= 0 {
        let elem = match ctx.get_array_element(arr, idx as usize) {
            Value::Object(Some(o)) => o,
            _ => break,
        };
        let name = mapper_map_element_name(ctx, elem);
        let name_len = name.encode_utf16().count();
        if char_chunk_range_starts_with(ctx, buff, start, original_end, &name, 0, false)
            && (full_len == name_len
                || char_chunk_range_starts_with(
                    ctx,
                    buff,
                    start,
                    original_end,
                    "/",
                    name_len,
                    true,
                ))
        {
            return Some((elem, name_len));
        }
        slash = if slash == usize::MAX {
            char_chunk_nth_slash(ctx, buff, start, search_end, nesting + 1)
        } else {
            char_chunk_last_slash(ctx, buff, start, search_end)
        };
        if slash == search_end {
            break;
        }
        search_end = slash;
        idx = mapper_find_chunk_range(ctx, arr, path, start, search_end, false);
    }
    None
}

pub fn mapper_apply_wildcard_mapping(
    ctx: &mut dyn NativeContext,
    mapping_data: ObjectRef,
    path: ObjectRef,
    wrapper: ObjectRef,
    wrapper_len: usize,
) {
    let Some((buff, start, original_end)) = char_chunk_parts(ctx, path) else {
        return;
    };
    let full_len = original_end.saturating_sub(start);
    let name_obj = match ctx.get_field_by_name(wrapper, "name") {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    };
    if let Value::Object(Some(wrapper_path)) = ctx.get_field_by_name(mapping_data, "wrapperPath") {
        message_bytes_set_string_object(ctx, wrapper_path, name_obj);
    }
    if full_len > wrapper_len {
        if let Value::Object(Some(path_info)) = ctx.get_field_by_name(mapping_data, "pathInfo") {
            message_bytes_set_chars(
                ctx,
                path_info,
                buff,
                start + wrapper_len,
                full_len - wrapper_len,
            );
        }
    }
    if let Value::Object(Some(request_path)) = ctx.get_field_by_name(mapping_data, "requestPath") {
        message_bytes_set_chars(ctx, request_path, buff, start, full_len);
    }
    let wrapper_object = match ctx.get_field_by_name(wrapper, "object") {
        Value::Object(o) => o,
        _ => None,
    };
    ctx.set_field_by_name(mapping_data, "wrapper", Value::Object(wrapper_object));
    ctx.set_field_by_name(
        mapping_data,
        "jspWildCard",
        Value::Int(
            ctx.get_field_by_name(wrapper, "jspWildCard")
                .as_int()
                .unwrap_or(0),
        ),
    );
    if let Some(path_match) = mapping_match_static(ctx, "PATH") {
        ctx.set_field_by_name(mapping_data, "matchType", Value::Object(Some(path_match)));
    }
}

/// Publish a memo row's answer into `mapping_data`. `uri` is the CURRENT URI
/// chunk's `(buff, start, end)`: the caller has just proved its text is the
/// row's ([`mapper_fast_entry_serves`]), so the row's lengths apply to it, and
/// the `MessageBytes` views are cut from this request's buffer.
pub fn mapper_apply_fast_entry(
    ctx: &mut dyn NativeContext,
    entry: &MapperInternalFastEntry,
    mapping_data: ObjectRef,
    uri: (ObjectRef, usize, usize),
    views: [Value; 3],
) {
    if entry.no_context {
        ctx.set_field_by_name(mapping_data, "host", Value::Object(entry.host));
        return;
    }
    ctx.set_fields_by_name(
        mapping_data,
        &[
            ("host", Value::Object(entry.host)),
            ("context", Value::Object(entry.context)),
            ("contextSlashCount", Value::Int(entry.context_slash_count)),
        ],
    );

    // The caller matched this entry's text against the chunk's `[start, end)`
    // a moment ago and nothing has run Java since.
    let (buff, uri_start, uri_end) = uri;
    let wrapper_start = uri_start + entry.context_path_len;
    let wrapper_full_len = uri_end.saturating_sub(wrapper_start);
    // The three `MessageBytes` views hanging off `MappingData`.
    let (request_path, wrapper_path, path_info) = (views[0], views[1], views[2]);
    if entry.default_mapping {
        if let Value::Object(Some(request_path)) = request_path {
            message_bytes_set_chars(ctx, request_path, buff, wrapper_start, wrapper_full_len);
        }
        if let Value::Object(Some(wrapper_path)) = wrapper_path {
            message_bytes_set_chars(ctx, wrapper_path, buff, wrapper_start, wrapper_full_len);
        }
        ctx.set_fields_by_name(
            mapping_data,
            &[
                ("wrapper", Value::Object(entry.wrapper)),
                ("jspWildCard", Value::Int(0)),
                ("matchType", Value::Object(entry.path_match)),
            ],
        );
        return;
    }
    if let Value::Object(Some(wrapper_path)) = wrapper_path {
        message_bytes_set_string_object(ctx, wrapper_path, entry.wrapper_name);
    }
    if wrapper_full_len > entry.wrapper_len {
        if let Value::Object(Some(path_info)) = path_info {
            message_bytes_set_chars(
                ctx,
                path_info,
                buff,
                wrapper_start + entry.wrapper_len,
                wrapper_full_len - entry.wrapper_len,
            );
        }
    }
    if let Value::Object(Some(request_path)) = request_path {
        message_bytes_set_chars(ctx, request_path, buff, wrapper_start, wrapper_full_len);
    }
    ctx.set_fields_by_name(
        mapping_data,
        &[
            ("wrapper", Value::Object(entry.wrapper)),
            ("jspWildCard", Value::Int(0)),
            ("matchType", Value::Object(entry.path_match)),
        ],
    );
}

/// The component of `MappingData.contexts`. Defined by Tomcat's common (or
/// server) loader, so it is resolved from the `Mapper`'s own class, as
/// `Mapper`'s `anewarray` would resolve it.
pub const MAPPER_CONTEXT_CLASS: &str = "org/apache/catalina/Context";

/// The references [`native_mapper_internal_map`] still reads after it builds
/// `MappingData.contexts`.
pub struct MapperVersionedLocals {
    pub this: ObjectRef,
    pub uri_chunk: ObjectRef,
    pub mapping_data: ObjectRef,
    pub versions: ObjectRef,
    pub version: Option<ObjectRef>,
}

/// `mappingData.contexts = { versions[0].object, ..., versions[n-1].object }`,
/// as Tomcat's `internalMap` does for a context with more than one version.
///
/// The component lookup (which may load `Context` under the `Mapper`'s
/// loader) and the allocation are GC points: `locals` are pinned across them
/// and handed back current. The pins are released once, through the oldest,
/// after the last read (lesson ll). A lookup that fails is what `new
/// Context[n]` would throw, and propagates.
pub fn mapper_publish_context_versions(
    ctx: &mut dyn NativeContext,
    locals: MapperVersionedLocals,
    version_count: usize,
) -> Result<MapperVersionedLocals, MethodCallFailed> {
    let base = ctx.pin_native_root(locals.this);
    let uri_chunk_pin = ctx.pin_native_root(locals.uri_chunk);
    let mapping_data_pin = ctx.pin_native_root(locals.mapping_data);
    let versions_pin = ctx.pin_native_root(locals.versions);
    let version_pin = locals.version.map(|v| ctx.pin_native_root(v));
    let mapper_class = ctx.class_id_of_object(locals.this);
    let component = match ctx.class_id_by_name_near(MAPPER_CONTEXT_CLASS, mapper_class) {
        Some(id) => Ok(id),
        None => ctx.class_id_by_name_via_referencing_class(mapper_class, MAPPER_CONTEXT_CLASS),
    };
    let array = component.map(|component| ctx.new_ref_array(component, version_count));
    let now = MapperVersionedLocals {
        this: ctx.read_native_pin(base, locals.this),
        uri_chunk: ctx.read_native_pin(uri_chunk_pin, locals.uri_chunk),
        mapping_data: ctx.read_native_pin(mapping_data_pin, locals.mapping_data),
        versions: ctx.read_native_pin(versions_pin, locals.versions),
        version: match (locals.version, version_pin) {
            (Some(v), Some(pin)) => Some(ctx.read_native_pin(pin, v)),
            (other, _) => other,
        },
    };
    ctx.unpin_native_roots(base);
    let array = array?;
    // Nothing below allocates: `array` and `now` stay current.
    let len = version_count.min(ctx.array_length(now.versions));
    for i in 0..len {
        let object = match ctx.get_array_element(now.versions, i) {
            Value::Object(Some(version)) => match ctx.get_field_by_name(version, "object") {
                Value::Object(o) => o,
                _ => None,
            },
            _ => None,
        };
        ctx.set_array_element(array, i, Value::Object(object));
    }
    ctx.set_field_by_name(now.mapping_data, "contexts", Value::Object(Some(array)));
    Ok(now)
}

pub fn native_mapper_internal_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // gc-common w20-c: the slow path's `mapping_match_static` calls must not
    // run `MappingMatch.<clinit>` under the raw locals bound below.
    let args_now = mapper_args_with_mapping_match_ready(ctx, args);
    let args: &[Value] = &args_now;
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let host_chunk = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let uri_chunk = match args.get(2) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let version = match args.get(3) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let mapping_data = match args.get(4) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // The memo's validity stamp, read before anything below could collect (see
    // `MapperMemoKey`).
    let memo_epoch = ctx.gc_address_epoch();

    // `host` (the "already used" check) and the three `MessageBytes` views the
    // memo-hit path rewrites, in one read.
    let mut md = [Value::Object(None); 4];
    ctx.get_fields_by_name(
        mapping_data,
        &["host", "requestPath", "wrapperPath", "pathInfo"],
        &mut md,
    );
    if !matches!(md[0], Value::Object(None)) {
        return Err(RuntimeError::IllegalStateException {
            message: "MappingData already used".to_string(),
        }
        .into());
    }
    let views = [md[1], md[2], md[3]];

    let mut mapper_fields = [Value::Object(None); 2];
    ctx.get_fields_by_name(this, &["hosts", "defaultHost"], &mut mapper_fields);
    let hosts = match mapper_fields[0] {
        Value::Object(Some(o)) => o,
        _ => return Ok(None),
    };
    let default_host = match mapper_fields[1] {
        Value::Object(o) => o,
        _ => None,
    };
    let Some((host_buff, host_start, host_end)) = char_chunk_parts(ctx, host_chunk) else {
        return Ok(None);
    };
    let Some((uri_buff_for_cache, uri_start_for_cache, uri_end_for_cache)) =
        char_chunk_parts(ctx, uri_chunk)
    else {
        return Ok(None);
    };
    // `uri.isNull()` (`end > 0 ? false : !isSet`): Tomcat maps the host and
    // returns. A recycled chunk keeps its buffer, so `char_chunk_parts` alone
    // took it for an empty URI and went on to map the ROOT context (gc-common
    // w36-e). Such a call is neither served from nor written to the memo,
    // whose rows cannot tell a null URI from an empty one.
    let uri_is_null = uri_end_for_cache == 0 && !char_chunk_is_set(ctx, uri_chunk);
    // The bucket only; the row names its Mapper by address (see
    // `mapper_internal_fast_cache`).
    let this_key = ctx.identity_hash_code(this);
    let memo_vm = ctx.vm_identity();
    let probe = MapperMemoProbe {
        epoch: memo_epoch,
        mapper: this,
        hosts,
        default_host,
        version,
        host: (host_buff, host_start, host_end),
        uri: (uri_buff_for_cache, uri_start_for_cache, uri_end_for_cache),
    };
    let cache_hit = if uri_is_null {
        None
    } else {
        mapper_internal_fast_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(memo_vm, this_key))
            .cloned()
    };
    if let Some(entry) = cache_hit {
        if mapper_fast_entry_serves(ctx, &entry, &probe) {
            if mapper_dbg_enabled() {
                mapper_memo_record(true);
            }
            mapper_apply_fast_entry(ctx, &entry, mapping_data, probe.uri, views);
            return Ok(None);
        }
    }
    if mapper_dbg_enabled() {
        mapper_memo_record(false);
    }
    // The inputs a row written below is valid for, read before the slow path
    // moves the URI chunk's `start` (and before anything else could change
    // the text).
    let memo_key = MapperMemoKey {
        epoch: memo_epoch,
        mapper: this,
        hosts,
        default_host,
        version,
        host_units: char_array_range_units(ctx, host_buff, host_start, host_end),
        uri_units: char_array_range_units(
            ctx,
            uri_buff_for_cache,
            uri_start_for_cache,
            uri_end_for_cache,
        ),
    };
    let mut mapped_host =
        mapper_exact_find_chunk_range(ctx, hosts, host_chunk, host_start, host_end, true);
    if mapped_host.is_none() {
        let mut dot = None;
        for pos in host_start..host_end {
            if char_chunk_char_at(ctx, host_buff, pos) == b'.' as u16 {
                dot = Some(pos);
                break;
            }
        }
        if let Some(dot_pos) = dot {
            mapped_host =
                mapper_exact_find_chunk_range(ctx, hosts, host_chunk, dot_pos, host_end, true);
        }
    }
    if mapped_host.is_none() {
        // `defaultHost` was read with `hosts` above; nothing has run since.
        mapped_host = default_host;
    }
    let Some(mapped_host) = mapped_host else {
        return Ok(None);
    };
    let host_object = match ctx.get_field_by_name(mapped_host, "object") {
        Value::Object(o) => o,
        _ => None,
    };
    ctx.set_field_by_name(mapping_data, "host", Value::Object(host_object));
    if uri_is_null {
        // Can't map a context or wrapper without a URI.
        return Ok(None);
    }

    let Some((uri_buff, uri_start, uri_end)) = char_chunk_parts(ctx, uri_chunk) else {
        return Ok(None);
    };
    let context_list = match ctx.get_field_by_name(mapped_host, "contextList") {
        Value::Object(Some(o)) => o,
        _ => return Ok(None),
    };
    let contexts = match ctx.get_field_by_name(context_list, "contexts") {
        Value::Object(Some(o)) => o,
        _ => return Ok(None),
    };
    let contexts_len = ctx.array_length(contexts);
    if contexts_len == 0 {
        mapper_memo_store(
            memo_vm,
            this_key,
            MapperInternalFastEntry {
                key: memo_key,
                mapped_host,
                context_list: Some(context_list),
                selected_context: None,
                versions: None,
                selected_version: None,
                dispatch: [None; 5],
                context_path_len: 0,
                wrapper_len: 0,
                host: host_object,
                context: None,
                wrapper: None,
                wrapper_name: None,
                path_match: None,
                context_slash_count: 0,
                no_context: true,
                default_mapping: false,
            },
        );
        return Ok(None);
    }

    let mut search_end = uri_end;
    let mut slash = usize::MAX;
    let nesting = ctx
        .get_field_by_name(context_list, "nesting")
        .as_int()
        .unwrap_or(0)
        .max(0) as usize;
    let mut idx = mapper_find_chunk_range(ctx, contexts, uri_chunk, uri_start, search_end, false);
    let mut selected_context: Option<ObjectRef> = None;
    while idx >= 0 {
        let elem = match ctx.get_array_element(contexts, idx as usize) {
            Value::Object(Some(o)) => o,
            _ => break,
        };
        let name = mapper_map_element_name(ctx, elem);
        let name_len = name.encode_utf16().count();
        let uri_len = uri_end.saturating_sub(uri_start);
        if char_chunk_range_starts_with(ctx, uri_buff, uri_start, uri_end, &name, 0, false)
            && (uri_len == name_len
                || char_chunk_range_starts_with(
                    ctx, uri_buff, uri_start, uri_end, "/", name_len, true,
                ))
        {
            selected_context = Some(elem);
            break;
        }
        slash = if slash == usize::MAX {
            char_chunk_nth_slash(ctx, uri_buff, uri_start, search_end, nesting + 1)
        } else {
            char_chunk_last_slash(ctx, uri_buff, uri_start, search_end)
        };
        if slash == search_end {
            break;
        }
        search_end = slash;
        idx = mapper_find_chunk_range(ctx, contexts, uri_chunk, uri_start, search_end, false);
    }
    if selected_context.is_none() {
        if let Value::Object(Some(first)) = ctx.get_array_element(contexts, 0) {
            if mapper_map_element_name(ctx, first).is_empty() {
                selected_context = Some(first);
            }
        }
    }
    let Some(selected_context) = selected_context else {
        mapper_memo_store(
            memo_vm,
            this_key,
            MapperInternalFastEntry {
                key: memo_key,
                mapped_host,
                context_list: Some(context_list),
                selected_context: None,
                versions: None,
                selected_version: None,
                dispatch: [None; 5],
                context_path_len: 0,
                wrapper_len: 0,
                host: host_object,
                context: None,
                wrapper: None,
                wrapper_name: None,
                path_match: None,
                context_slash_count: 0,
                no_context: true,
                default_mapping: false,
            },
        );
        return Ok(None);
    };

    let versions = match ctx.get_field_by_name(selected_context, "versions") {
        Value::Object(Some(o)) => o,
        _ => return Ok(None),
    };
    let version_count = ctx.array_length(versions);
    if version_count == 0 {
        return Ok(None);
    }
    // Parallel deployment (`app##1`, `app##2` under one path): Tomcat's
    // `internalMap` publishes every version's `Context` in
    // `MappingData.contexts`, which `CoyoteAdapter.postParseRequest` walks to
    // send a request whose session belongs to an older version back to it.
    // The native never wrote it, so such a user landed on the newest version
    // and lost the session (gc-common w36-e,
    // `common-w34b-tomcat-mapper-native-never-sets-mapping-data-contexts-FIXED-20260926`).
    // Building the array is a GC point: the references read below come back
    // current, and the call is not memoised (a row would have to carry the
    // array).
    let memoisable = version_count <= 1;
    let (this, uri_chunk, mapping_data, versions, version) = if memoisable {
        (this, uri_chunk, mapping_data, versions, version)
    } else {
        let now = mapper_publish_context_versions(
            ctx,
            MapperVersionedLocals {
                this,
                uri_chunk,
                mapping_data,
                versions,
                version,
            },
            version_count,
        )?;
        (
            now.this,
            now.uri_chunk,
            now.mapping_data,
            now.versions,
            now.version,
        )
    };
    let selected_version = if let Some(version_obj) = version {
        let version_name = ctx.read_string(version_obj).unwrap_or_default();
        let idx = mapper_find_string(ctx, versions, &version_name);
        if idx >= 0 {
            match ctx.get_array_element(versions, idx as usize) {
                Value::Object(Some(o)) if mapper_map_element_name(ctx, o) == version_name => o,
                _ => match ctx.get_array_element(versions, version_count - 1) {
                    Value::Object(Some(o)) => o,
                    _ => return Ok(None),
                },
            }
        } else {
            match ctx.get_array_element(versions, version_count - 1) {
                Value::Object(Some(o)) => o,
                _ => return Ok(None),
            }
        }
    } else {
        match ctx.get_array_element(versions, version_count - 1) {
            Value::Object(Some(o)) => o,
            _ => return Ok(None),
        }
    };

    let context_object = match ctx.get_field_by_name(selected_version, "object") {
        Value::Object(o) => o,
        _ => None,
    };
    ctx.set_field_by_name(mapping_data, "context", Value::Object(context_object));
    ctx.set_field_by_name(
        mapping_data,
        "contextSlashCount",
        ctx.get_field_by_name(selected_version, "slashCount"),
    );
    // `paused` and the five dispatch tables, in one read; a memo row written
    // below records the tables it was decided against.
    let (paused, dispatch) = mapper_version_dispatch_state(ctx, selected_version);
    let [exact_wrappers, wildcard_wrappers, extension_wrappers, welcome_resources, default_wrapper] =
        dispatch;
    if !paused {
        if let Some(context_path) = match ctx.get_field_by_name(selected_version, "path") {
            Value::Object(Some(o)) => ctx.read_string(o),
            _ => None,
        } {
            let wrapper_start = uri_start + context_path.encode_utf16().count();
            if wrapper_start <= uri_end {
                ctx.set_field_by_name(uri_chunk, "start", Value::Int(wrapper_start as i32));
                // Tomcat's `internalMapWrapper` tries Rule 1 (exact match)
                // BEFORE Rule 2 (wildcard) and Rule 7 (default). The two
                // native short cuts below skipped it, so an exact servlet
                // mapping under a wildcard or default one (`/api/health`
                // beside `/api/*`) went to the wrong servlet (gc-common
                // w34-b). When an exact wrapper matches, Java decides.
                let exact_match = match exact_wrappers {
                    Some(arr) => mapper_exact_find_chunk_range(
                        ctx,
                        arr,
                        uri_chunk,
                        wrapper_start,
                        uri_end,
                        false,
                    )
                    .is_some(),
                    None => false,
                };
                // A wildcard match on a `jspWildCard` wrapper is Rule 2's
                // JSP special case (Bugzilla 27664 / 27704): Tomcat keeps
                // that wrapper, or clears it and checks the welcome files. It
                // never reaches Rule 7, so the default short cut must not run
                // either (it used to).
                let mut natives_may_finish = !exact_match;
                let wrapper_nesting = ctx
                    .get_field_by_name(selected_version, "nesting")
                    .as_int()
                    .unwrap_or(0)
                    .max(0) as usize;
                if let (true, Some(wildcard_wrappers)) = (natives_may_finish, wildcard_wrappers) {
                    if let Some((wrapper, wrapper_len)) =
                        mapper_wildcard_match(ctx, wildcard_wrappers, wrapper_nesting, uri_chunk)
                    {
                        let jsp_wildcard = ctx
                            .get_field_by_name(wrapper, "jspWildCard")
                            .as_int()
                            .unwrap_or(0)
                            != 0;
                        if jsp_wildcard {
                            natives_may_finish = false;
                        } else {
                            mapper_apply_wildcard_mapping(
                                ctx,
                                mapping_data,
                                uri_chunk,
                                wrapper,
                                wrapper_len,
                            );
                            let wrapper_name = match ctx.get_field_by_name(wrapper, "name") {
                                Value::Object(o) => o,
                                _ => None,
                            };
                            let wrapper_object = match ctx.get_field_by_name(wrapper, "object") {
                                Value::Object(o) => o,
                                _ => None,
                            };
                            let path_match = mapping_match_static(ctx, "PATH");
                            let context_slash_count = ctx
                                .get_field_by_name(selected_version, "slashCount")
                                .as_int()
                                .unwrap_or(0);
                            if memoisable {
                                mapper_memo_store(
                                    memo_vm,
                                    this_key,
                                    MapperInternalFastEntry {
                                        key: memo_key,
                                        mapped_host,
                                        context_list: Some(context_list),
                                        selected_context: Some(selected_context),
                                        versions: Some(versions),
                                        selected_version: Some(selected_version),
                                        dispatch,
                                        context_path_len: context_path.encode_utf16().count(),
                                        wrapper_len,
                                        host: host_object,
                                        context: context_object,
                                        wrapper: wrapper_object,
                                        wrapper_name,
                                        path_match,
                                        context_slash_count,
                                        no_context: false,
                                        default_mapping: false,
                                    },
                                );
                            }
                            ctx.set_field_by_name(uri_chunk, "start", Value::Int(uri_start as i32));
                            ctx.set_field_by_name(uri_chunk, "end", Value::Int(uri_end as i32));
                            return Ok(None);
                        }
                    }
                }
                let resources = match ctx.get_field_by_name(selected_version, "resources") {
                    Value::Object(Some(o)) => Some(o),
                    _ => None,
                };
                let extension_wrappers_empty = match extension_wrappers {
                    Some(o) => ctx.array_length(o) == 0,
                    None => true,
                };
                let welcome_resources_empty = match welcome_resources {
                    Some(o) => ctx.array_length(o) == 0,
                    None => true,
                };
                // `wrapper_start < uri_end`: a URI that IS the context path
                // (`/app`) is Tomcat's "no servlet path" case, which redirects
                // to `/app/` when the Context's
                // `mapperContextRootRedirectEnabled` is set (the default). Only
                // Java can ask the Context, so Java maps it (gc-common w34-b).
                if let (true, Some(default_wrapper)) = (
                    natives_may_finish
                        && wrapper_start < uri_end
                        && extension_wrappers_empty
                        && welcome_resources_empty
                        && resources.is_none(),
                    default_wrapper,
                ) {
                    let Some((buff, path_start, path_end)) = char_chunk_parts(ctx, uri_chunk)
                    else {
                        ctx.set_field_by_name(uri_chunk, "start", Value::Int(uri_start as i32));
                        ctx.set_field_by_name(uri_chunk, "end", Value::Int(uri_end as i32));
                        return Ok(None);
                    };
                    let path_len = path_end.saturating_sub(path_start);
                    let wrapper_object = match ctx.get_field_by_name(default_wrapper, "object") {
                        Value::Object(o) => o,
                        _ => None,
                    };
                    ctx.set_field_by_name(mapping_data, "wrapper", Value::Object(wrapper_object));
                    if let Value::Object(Some(request_path)) =
                        ctx.get_field_by_name(mapping_data, "requestPath")
                    {
                        message_bytes_set_chars(ctx, request_path, buff, path_start, path_len);
                    }
                    if let Value::Object(Some(wrapper_path)) =
                        ctx.get_field_by_name(mapping_data, "wrapperPath")
                    {
                        message_bytes_set_chars(ctx, wrapper_path, buff, path_start, path_len);
                    }
                    // Read once, here: the memo row below used to call it a
                    // second time INSIDE `insert`'s argument list, i.e. with the
                    // memo's mutex held around a lookup that can run
                    // `MappingMatch.<clinit>` (gc-common w19-a).
                    let default_path_match = mapping_match_static(ctx, "DEFAULT");
                    if let Some(default_match) = default_path_match {
                        ctx.set_field_by_name(
                            mapping_data,
                            "matchType",
                            Value::Object(Some(default_match)),
                        );
                    }
                    let context_slash_count = ctx
                        .get_field_by_name(selected_version, "slashCount")
                        .as_int()
                        .unwrap_or(0);
                    if memoisable {
                        mapper_memo_store(
                            memo_vm,
                            this_key,
                            MapperInternalFastEntry {
                                key: memo_key,
                                mapped_host,
                                context_list: Some(context_list),
                                selected_context: Some(selected_context),
                                versions: Some(versions),
                                selected_version: Some(selected_version),
                                dispatch,
                                context_path_len: context_path.encode_utf16().count(),
                                wrapper_len: 0,
                                host: host_object,
                                context: context_object,
                                wrapper: wrapper_object,
                                wrapper_name: None,
                                path_match: default_path_match,
                                context_slash_count,
                                no_context: false,
                                default_mapping: true,
                            },
                        );
                    }
                    ctx.set_field_by_name(uri_chunk, "start", Value::Int(uri_start as i32));
                    ctx.set_field_by_name(uri_chunk, "end", Value::Int(uri_end as i32));
                    return Ok(None);
                }
                ctx.set_field_by_name(uri_chunk, "start", Value::Int(uri_start as i32));
                ctx.set_field_by_name(uri_chunk, "end", Value::Int(uri_end as i32));
            }
        }
        let _ = ctx.invoke_virtual(
            this,
            "internalMapWrapper",
            "(Lorg/apache/catalina/mapper/Mapper$ContextVersion;Lorg/apache/tomcat/util/buf/CharChunk;Lorg/apache/catalina/mapper/MappingData;)V",
            &[
                Value::Object(Some(selected_version)),
                Value::Object(Some(uri_chunk)),
                Value::Object(Some(mapping_data)),
            ],
        )?;
    }
    Ok(None)
}

pub fn native_mapper_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this: ObjectRef = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let host_mb = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let uri_mb = match args.get(2) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let version = args.get(3).copied().unwrap_or(Value::Object(None));
    let mapping_data = match args.get(4) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // gc-common w34-b handoff (`handoff-w34b-mapper-map-raw-locals-across-to-chars`):
    // either `toChars()` below may allocate and collect, and this function
    // reads all five references after it. Pinned oldest-first and released
    // once, after the last read (lesson ll).
    let pin_base = ctx.pin_native_root(this);
    let pins = MapperMapPins {
        this: pin_base,
        host_mb: ctx.pin_native_root(host_mb),
        uri_mb: ctx.pin_native_root(uri_mb),
        mapping_data: ctx.pin_native_root(mapping_data),
        version: match version {
            Value::Object(Some(v)) => Some(ctx.pin_native_root(v)),
            _ => None,
        },
    };
    let result = native_mapper_map_body(ctx, &pins, this, host_mb, uri_mb, version, mapping_data);
    ctx.unpin_native_roots(pin_base);
    result
}

/// The pins `native_mapper_map` holds across its two `toChars()` conversions.
pub struct MapperMapPins {
    pub this: usize,
    pub host_mb: usize,
    pub uri_mb: usize,
    pub mapping_data: usize,
    pub version: Option<usize>,
}

impl MapperMapPins {
    /// The five references, current. Called only after a conversion
    /// reported that it allocated.
    pub fn reread(
        &self,
        ctx: &dyn NativeContext,
        (this, host_mb, uri_mb, version, mapping_data): (
            ObjectRef,
            ObjectRef,
            ObjectRef,
            Value,
            ObjectRef,
        ),
    ) -> (ObjectRef, ObjectRef, ObjectRef, Value, ObjectRef) {
        let version = match (version, self.version) {
            (Value::Object(Some(v)), Some(pin)) => Value::Object(Some(ctx.read_native_pin(pin, v))),
            (other, _) => other,
        };
        (
            ctx.read_native_pin(self.this, this),
            ctx.read_native_pin(self.host_mb, host_mb),
            ctx.read_native_pin(self.uri_mb, uri_mb),
            version,
            ctx.read_native_pin(self.mapping_data, mapping_data),
        )
    }
}

pub fn native_mapper_map_body(
    ctx: &mut dyn NativeContext,
    pins: &MapperMapPins,
    this: ObjectRef,
    host_mb: ObjectRef,
    uri_mb: ObjectRef,
    version: Value,
    mapping_data: ObjectRef,
) -> MethodCallResult {
    let mut host_fields = message_bytes_conversion_fields(ctx, host_mb);
    if host_fields[0].as_int().unwrap_or(0) == 0 {
        let default_host_name = match ctx.get_field_by_name(this, "defaultHostName") {
            Value::Object(Some(o)) => Some(o),
            _ => return Ok(None),
        };
        message_bytes_set_string_object(ctx, host_mb, default_host_name);
        host_fields = message_bytes_conversion_fields(ctx, host_mb);
    }
    let (host_chunk, host_moved) = message_bytes_to_chars_with(ctx, host_mb, host_fields)?;
    let (this, host_mb, uri_mb, version, mapping_data) = if host_moved {
        pins.reread(ctx, (this, host_mb, uri_mb, version, mapping_data))
    } else {
        (this, host_mb, uri_mb, version, mapping_data)
    };

    // Match Mapper.map()'s Java contract for a null URI. CoyoteAdapter
    // deliberately recycles decodedURI after an early protocol error, then
    // calls Mapper.map() only to identify the Host for error reporting. The
    // Java implementation sets MappingData.host and returns before context
    // / wrapper mapping. Mapping the null URI as an empty CharChunk instead
    // selects the ROOT context and can emit its welcome-file redirect,
    // overwriting the original protocol status (e.g. AJP's 403 secret
    // rejection) with 302.
    let uri_fields = message_bytes_conversion_fields(ctx, uri_mb);
    if uri_fields[0].as_int().unwrap_or(0) == 0 {
        let host_chunk = match ctx.get_field_by_name(host_mb, "charC") {
            Value::Object(Some(o)) => o,
            _ => return Ok(None),
        };
        let hosts = match ctx.get_field_by_name(this, "hosts") {
            Value::Object(Some(o)) => o,
            _ => return Ok(None),
        };
        let Some((host_buff, host_start, host_end)) = char_chunk_parts(ctx, host_chunk) else {
            return Ok(None);
        };
        let mut mapped_host =
            mapper_exact_find_chunk_range(ctx, hosts, host_chunk, host_start, host_end, true);
        if mapped_host.is_none() {
            let dot = (host_start..host_end)
                .find(|&pos| char_chunk_char_at(ctx, host_buff, pos) == b'.' as u16);
            if let Some(dot_pos) = dot {
                mapped_host =
                    mapper_exact_find_chunk_range(ctx, hosts, host_chunk, dot_pos, host_end, true);
            }
        }
        if mapped_host.is_none() {
            mapped_host = match ctx.get_field_by_name(this, "defaultHost") {
                Value::Object(Some(o)) => Some(o),
                _ => None,
            };
        }
        if let Some(mapped_host) = mapped_host {
            let host_object = match ctx.get_field_by_name(mapped_host, "object") {
                Value::Object(o) => o,
                _ => None,
            };
            ctx.set_field_by_name(mapping_data, "host", Value::Object(host_object));
        }
        return Ok(None);
    }
    let (uri_chunk, uri_moved) = message_bytes_to_chars_with(ctx, uri_mb, uri_fields)?;
    let (this, host_mb, uri_mb, version, mapping_data) = if uri_moved {
        pins.reread(ctx, (this, host_mb, uri_mb, version, mapping_data))
    } else {
        (this, host_mb, uri_mb, version, mapping_data)
    };

    // Either conversion may have allocated (or run Java), which can move the
    // other's chunk: re-read both then. Otherwise the references the two
    // conversions handed back are current and the reads are skipped.
    let (host_chunk, uri_chunk) = if host_moved || uri_moved {
        (
            match ctx.get_field_by_name(host_mb, "charC") {
                Value::Object(Some(o)) => Some(o),
                _ => None,
            },
            match ctx.get_field_by_name(uri_mb, "charC") {
                Value::Object(Some(o)) => Some(o),
                _ => None,
            },
        )    } else {
        (host_chunk, uri_chunk)    };
    let (Some(host_chunk), Some(uri_chunk)) = (host_chunk, uri_chunk) else {
        return Ok(None);
    };
    native_mapper_internal_map(
        ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(host_chunk)),
            Value::Object(Some(uri_chunk)),
            version,
            Value::Object(Some(mapping_data)),
        ],
    )
}

pub fn native_mapper_internal_map_exact_wrapper(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let args_now = mapper_args_with_mapping_match_ready(ctx, args);
    let args: &[Value] = &args_now;
    let arr = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let path = match args.get(2) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let mapping_data = match args.get(3) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };

    let Some((_, start, end)) = char_chunk_parts(ctx, path) else {
        return Ok(None);
    };
    let wrapper = if end == start {
        let idx = mapper_find_string(ctx, arr, "/");
        if idx < 0 {
            return Ok(None);
        }
        match ctx.get_array_element(arr, idx as usize) {
            Value::Object(Some(o)) if mapper_map_element_name(ctx, o) == "/" => o,
            _ => return Ok(None),
        }
    } else {
        let idx = mapper_find_chunk_range(ctx, arr, path, start, end, false);
        if idx < 0 {
            return Ok(None);
        }
        match ctx.get_array_element(arr, idx as usize) {
            Value::Object(Some(o))
                if char_chunk_equals_string(ctx, path, &mapper_map_element_name(ctx, o), false) =>
            {
                o            }
            _ => return Ok(None),
        }
    };

    let name_obj = match ctx.get_field_by_name(wrapper, "name") {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    };
    if let Value::Object(Some(request_path)) = ctx.get_field_by_name(mapping_data, "requestPath") {
        message_bytes_set_string_object(ctx, request_path, name_obj);
    }
    let wrapper_object = match ctx.get_field_by_name(wrapper, "object") {
        Value::Object(o) => o,
        _ => None,
    };
    ctx.set_field_by_name(mapping_data, "wrapper", Value::Object(wrapper_object));

    let is_context_root = end > start
        && end - start == 1
        && char_chunk_parts(ctx, path)
            .map(|(buff, s, _)| char_chunk_char_at(ctx, buff, s) == b'/' as u16)
            .unwrap_or(false);
    // gc-common w19-a: both string allocations and `mapping_match_static`
    // (which can run `MappingMatch.<clinit>`) can collect and move
    // `mapping_data` and the first string; every use after one re-reads its pin.
    let mapping_data_pin = ctx.pin_native_root(mapping_data);
    if is_context_root {
        let slash = ctx.create_string("/");
        let slash_pin = ctx.pin_native_root(slash);
        let empty = ctx.create_string("");
        let slash = ctx.read_native_pin(slash_pin, slash);
        let mapping_data = ctx.read_native_pin(mapping_data_pin, mapping_data);
        if let Value::Object(Some(path_info)) = ctx.get_field_by_name(mapping_data, "pathInfo") {
            message_bytes_set_string_object(ctx, path_info, Some(slash));
        }
        if let Value::Object(Some(wrapper_path)) =
            ctx.get_field_by_name(mapping_data, "wrapperPath")
        {
            message_bytes_set_string_object(ctx, wrapper_path, Some(empty));
        }
        if let Some(mm) = mapping_match_static(ctx, "CONTEXT_ROOT") {
            let mapping_data = ctx.read_native_pin(mapping_data_pin, mapping_data);
            ctx.set_field_by_name(mapping_data, "matchType", Value::Object(Some(mm)));
        }
    } else {
        if let Value::Object(Some(wrapper_path)) =
            ctx.get_field_by_name(mapping_data, "wrapperPath")
        {
            message_bytes_set_string_object(ctx, wrapper_path, name_obj);
        }
        if let Some(mm) = mapping_match_static(ctx, "EXACT") {
            let mapping_data = ctx.read_native_pin(mapping_data_pin, mapping_data);
            ctx.set_field_by_name(mapping_data, "matchType", Value::Object(Some(mm)));
        }
    }
    ctx.unpin_native_roots(mapping_data_pin);
    Ok(None)
}

pub fn native_output_stream_write_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // `OutputStream.write(byte[] b)` IS `write(b, 0, b.length)`, and reading
    // `b.length` off a null reference is an NPE before any byte moves. Falling
    // through to `Ok(None)` made the call a SILENT NO-OP: the caller writes,
    // gets no exception, closes the stream, and believes it holds the bytes.
    //
    // MEASURED with `L4TailSweep2.java` (`f.write((byte[]) null)`).
    // The first attempt at this fix went into `write([BII)V` -- a third
    // overload, which no row in that probe calls. `write(byte[])`,
    // `write(byte[],int,int)` and `write(int)` are three separate registered
    // slots, and a null check in one of them is worth nothing to the other
    // two: read the DESCRIPTOR the failing row dispatches on, not the method
    // name.
    let arr = match args.get(1) {
        Some(Value::Object(None)) => {
            return Err(
                cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into(),
            )
        }
        Some(Value::Object(Some(arr))) => *arr,
        _ => return Ok(None),
    };
    let len = ctx.array_length(arr) as i32;
    let res = ctx.invoke_virtual(
        this,
        "write",
        "([BII)V",
        &[Value::Object(Some(arr)), Value::Int(0), Value::Int(len)],
    )?;
    Ok(None)
}


pub fn os_env_bytes(s: &std::ffi::OsStr) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        s.as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        s.to_string_lossy().into_owned().into_bytes()
    }
}

pub fn native_process_environment_environ(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let mut cells: Vec<Vec<u8>> = Vec::new();
    for (key, value) in std::env::vars_os() {
        let key = os_env_bytes(&key);
        let value = os_env_bytes(&value);
        if key.iter().any(|b| *b == b'=' || *b == 0) || value.iter().any(|b| *b == 0) {
            continue;
        }
        cells.push(key);
        cells.push(value);
    }

    let outer = ctx.new_array(cratonvm_types::ArrayElementType::Reference, cells.len());
    let outer_root = ctx.pin_native_root(outer);
    for (idx, bytes) in cells.iter().enumerate() {
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
        if !ctx.write_byte_array_from(arr, 0, bytes) {
            for (i, &b) in bytes.iter().enumerate() {
                ctx.set_array_element(arr, i, Value::Int(b as i32));
            }
        }
        let outer_now = ctx.read_native_pin(outer_root, outer);
        ctx.set_array_element(outer_now, idx, Value::Object(Some(arr)));
    }
    let outer = ctx.read_native_pin(outer_root, outer);
    ctx.unpin_native_roots(outer_root);
    Ok(Some(Value::Object(Some(outer))))
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct RandomizedContextCacheKey {
    pub vm: usize,
    pub thread: i32,
    pub group: i32,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct RandomizedRandomCacheKey {
    pub vm: usize,
    pub context: i32,
    pub thread: i32,
}

pub fn native_randomness_get_random(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Registered `()Ljava/util/Random;` — reference-typed, so the by-name read
    // must not be returned raw: it answers `Value::Int(0)` for an unwritten
    // slot. See `docs/feature-designs/by-name-field-reads.md`.
    Ok(Some(field_read::ref_field(ctx, this, "random")))
}

#[inline]
pub fn xoroshiro_state(ctx: &dyn NativeContext, this: ObjectRef) -> (u64, u64) {
    let s0 = match ctx.get_field_by_name(this, "s0") {
        Value::Long(v) => v as u64,
        _ => 0,
    };
    let s1 = match ctx.get_field_by_name(this, "s1") {
        Value::Long(v) => v as u64,
        _ => 0,
    };
    (s0, s1)}

#[inline]
pub fn xoroshiro_next_long_raw(ctx: &mut dyn NativeContext, this: ObjectRef) -> i64 {
    let (s0, mut s1) = xoroshiro_state(ctx, this);
    let result = s0.wrapping_add(s1);
    s1 ^= s0;
    let new_s0 = s0.rotate_left(55) ^ s1 ^ s1.wrapping_shl(14);
    let new_s1 = s1.rotate_left(36);
    ctx.set_field_by_name(this, "s0", Value::Long(new_s0 as i64));
    ctx.set_field_by_name(this, "s1", Value::Long(new_s1 as i64));
    result as i64
}

#[inline]
pub fn xoroshiro_next_int_raw(ctx: &mut dyn NativeContext, this: ObjectRef) -> i32 {
    xoroshiro_next_long_raw(ctx, this) as i32
}

#[inline]
pub fn xoroshiro_next_bits_raw(ctx: &mut dyn NativeContext, this: ObjectRef, bits: i32) -> i32 {
    let shift = (32 - bits).clamp(0, 32) as u32;
    ((xoroshiro_next_int_raw(ctx, this) as u32) >> shift) as i32
}

pub fn native_xoroshiro128_plus_random_next_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(Value::Long(xoroshiro_next_long_raw(ctx, this))))
}

pub fn native_xoroshiro128_plus_random_next_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(Value::Int(xoroshiro_next_int_raw(ctx, this))))
}

pub fn native_xoroshiro128_plus_random_next_bits(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let bits = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
    Ok(Some(Value::Int(xoroshiro_next_bits_raw(ctx, this, bits))))
}

pub fn native_xoroshiro128_plus_random_next_int_bound(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let bound = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
    if bound <= 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: "bound must be positive".to_string(),
        }
        .into());
    }
    if (bound & bound.wrapping_neg()) == bound {
        let bits = xoroshiro_next_bits_raw(ctx, this, 31) as i64;
        return Ok(Some(Value::Int(((bound as i64 * bits) >> 31) as i32)));
    }
    loop {
        let bits = xoroshiro_next_bits_raw(ctx, this, 31);
        let value = bits % bound;
        if bits.wrapping_sub(value).wrapping_add(bound - 1) >= 0 {
            return Ok(Some(Value::Int(value)));
        }
    }
}

pub fn native_xoroshiro128_plus_random_next_float(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let bits = (xoroshiro_next_int_raw(ctx, this) as u32) >> 8;
    Ok(Some(Value::Float((bits as f32) * 5.960_464_5e-8_f32)))
}

pub fn native_xoroshiro128_plus_random_next_double(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let bits = (xoroshiro_next_long_raw(ctx, this) as u64) >> 11;
    Ok(Some(Value::Double(
        (bits as f64) * 1.110_223_024_625_156_5e-16_f64,
    )))
}

pub fn native_xoroshiro128_plus_random_next_boolean(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(Value::Int(
        if xoroshiro_next_long_raw(ctx, this) >= 0 {
            1
        } else {
            0
        },
    )))
}

pub fn native_xoroshiro128_plus_random_next_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let bytes = obj_arg(args, 1)?;
    let len = ctx.array_length(bytes);
    let mut index = 0;
    while index < len {
        let mut rnd = xoroshiro_next_int_raw(ctx, this) as i64 as u64;
        let chunk = std::cmp::min(len - index, 8);
        for _ in 0..chunk {
            ctx.set_array_element(bytes, index, Value::Int(rnd as u8 as i8 as i32));
            index += 1;
            rnd >>= 8;
        }
    }
    Ok(None)
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct RandomizedPerThreadCacheKey {
    pub vm: usize,
    pub context: i32,
    pub map: i32,
    pub thread: i32,
}

pub fn float_array_elem(ctx: &dyn NativeContext, arr: ObjectRef, index: usize) -> f32 {
    match ctx.get_array_element(arr, index) {
        Value::Float(v) => v,
        Value::Int(v) => f32::from_bits(v as u32),
        _ => 0.0,
    }
}

/// The JVM's preferred SIMD lane count for `float`, mirroring
/// `jdk.incubator.vector`'s `VectorSpecies.ofPreferred(float.class)`: 16
/// lanes (512-bit) with AVX-512F, 8 (256-bit) with AVX2, else 4 (128-bit).
/// Lucene's Panama vector code picks its species width the same way, and the
/// exact lane grouping changes the floating-point summation order (and thus
/// the bit-exact result) -- this has to track real hardware, not a fixed
/// constant, to stay correct across deployment targets.
#[inline]
pub fn panama_preferred_lanes_f32() -> usize {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx512f") {
            16
        } else if is_x86_feature_detected!("avx2") {
            8
        } else {
            4
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        4
    }
}

#[inline]
pub fn float_arg(args: &[Value], index: usize) -> f32 {
    match args.get(index) {
        Some(Value::Float(v)) => *v,
        Some(Value::Int(v)) => f32::from_bits(*v as u32),
        _ => 0.0,
    }
}

#[inline]
pub fn java_f32_min(a: f32, b: f32) -> f32 {
    if a.is_nan() || b.is_nan() {
        f32::NAN
    } else if a == 0.0 && b == 0.0 {
        if a.is_sign_negative() || b.is_sign_negative() {
            -0.0
        } else {
            0.0
        }
    } else if a < b {
        a
    } else {
        b
    }
}

#[inline]
pub fn java_f32_max(a: f32, b: f32) -> f32 {
    if a.is_nan() || b.is_nan() {
        f32::NAN
    } else if a == 0.0 && b == 0.0 {
        if a.is_sign_positive() || b.is_sign_positive() {
            0.0
        } else {
            -0.0
        }
    } else if a > b {
        a
    } else {
        b
    }
}

#[inline]
pub fn java_math_round_f32(v: f32) -> i32 {
    // The naive `floor(v + 0.5)` mis-rounds the largest float just below a
    // half-integer boundary (the same JDK-6430675 class of bug already fixed
    // for `java.lang.Math.round` in `lang_math::round_float`) -- reuse that
    // bit-exact implementation instead of a second, divergent copy.
    crate::lang_math::round_float(v)
}

#[inline]
pub fn set_float_array_elem(ctx: &dyn NativeContext, arr: ObjectRef, index: usize, value: f32) {
    ctx.set_array_element(arr, index, Value::Float(value));
}

#[inline]
pub fn set_int_array_elem(ctx: &dyn NativeContext, arr: ObjectRef, index: usize, value: i32) {
    ctx.set_array_element(arr, index, Value::Int(value));
}

#[inline]
pub fn byte_array_elem(ctx: &dyn NativeContext, arr: ObjectRef, index: usize) -> i32 {
    ctx.get_array_element(arr, index).as_int().unwrap_or(0) as i8 as i32
}

#[inline]
pub fn int_array_elem(ctx: &dyn NativeContext, arr: ObjectRef, index: usize) -> i32 {
    match ctx.get_array_element(arr, index) {
        Value::Int(v) => v,
        _ => 0,
    }
}

#[inline]
pub fn bool_array_elem(ctx: &dyn NativeContext, arr: ObjectRef, index: usize) -> bool {
    match ctx.get_array_element(arr, index) {
        Value::Int(v) => v != 0,
        _ => false,
    }
}

pub fn native_java_arrays_sort_long_range(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let arr = obj_arg(args, 0)?;
    let from = args.get(1).and_then(Value::as_int).unwrap_or(0);
    let to = args.get(2).and_then(Value::as_int).unwrap_or(0);
    let len = ctx.array_length(arr);
    // `Arrays.rangeCheck` order and texts: `fromIndex > toIndex` first, as
    // `IllegalArgumentException("fromIndex(f) > toIndex(t)")`, then
    // `new ArrayIndexOutOfBoundsException(fromIndex)` / `(toIndex)` (wave 100
    // L7: the IAE said "fromIndex > toIndex" and came after the negative test).
    if to < from {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("fromIndex({from}) > toIndex({to})"),
        }
        .into());
    }
    if from < 0 {
        return Err(lucene_aioobe(from));
    }
    let from = from as usize;
    let to = to as usize;
    if to > len {
        return Err(lucene_aioobe(to.min(i32::MAX as usize) as i32));
    }
    if to.saturating_sub(from) <= 1 {
        return Ok(None);
    }
    let mut values = Vec::with_capacity(to - from);
    for i in from..to {
        values.push(ctx.get_array_element(arr, i).as_long().unwrap_or(0));
    }
    values.sort();
    for (i, value) in values.into_iter().enumerate() {
        ctx.set_array_element(arr, from + i, Value::Long(value));
    }
    Ok(None)
}

pub fn java_string_hash_code_ascii(s: &str) -> i32 {
    let mut h = 0i32;
    for b in s.bytes() {
        h = h.wrapping_mul(31).wrapping_add(b as i32);
    }
    h
}

pub const LUCENE_RAM_USAGE_FALLBACK_DOCUMENT_BYTES: i64 = 1024;
pub const LUCENE_RAM_USAGE_MIN_DOCUMENT_BYTES: i64 = 1024;
pub const LUCENE_RAM_USAGE_DOCUMENT_BASE_BYTES: i64 = 376;
pub const LUCENE_RAM_USAGE_EXTRA_FIELD_BYTES: i64 = 256;

pub fn invoke_list_size(ctx: &mut dyn NativeContext, list: ObjectRef) -> Option<i32> {
    let pin = ctx.pin_native_root(list);
    let result = ctx.invoke_virtual(list, "size", "()I", &[]);
    ctx.unpin_native_roots(pin);
    match result {
        Ok(Some(Value::Int(size))) if size >= 0 => Some(size),
        _ => None,
    }
}

pub fn invoke_list_get(ctx: &mut dyn NativeContext, list: ObjectRef, index: i32) -> Option<ObjectRef> {
    let pin = ctx.pin_native_root(list);
    let result = ctx.invoke_virtual(list, "get", "(I)Ljava/lang/Object;", &[Value::Int(index)]);
    ctx.unpin_native_roots(pin);
    match result {
        Ok(Some(Value::Object(Some(field)))) => Some(field),
        _ => None,
    }
}

pub fn heap_byte_buffer_remaining(ctx: &dyn NativeContext, block: ObjectRef) -> i32 {
    let position = ctx
        .get_field_by_name(block, "position")
        .as_int()
        .unwrap_or(0);
    let limit = ctx.get_field_by_name(block, "limit").as_int().unwrap_or(0);
    limit.saturating_sub(position).max(0)
}

pub fn heap_byte_buffer_write_slice(
    ctx: &mut dyn NativeContext,
    block: ObjectRef,
    bytes: &[u8],
) -> usize {
    let position = ctx
        .get_field_by_name(block, "position")
        .as_int()
        .unwrap_or(0);
    let limit = ctx.get_field_by_name(block, "limit").as_int().unwrap_or(0);
    let offset = ctx.get_field_by_name(block, "offset").as_int().unwrap_or(0);
    let hb = match ctx.get_field_by_name(block, "hb") {
        Value::Object(Some(hb)) => hb,
        _ => return 0,
    };
    if position < 0 || limit < position {
        return 0;
    }
    let writable = (limit - position) as usize;
    let n = writable.min(bytes.len());
    if n == 0 {
        return 0;
    }
    let raw = offset as i64 + position as i64;
    if raw < 0 {
        return 0;
    }
    if !ctx.write_byte_array_from(hb, raw as usize, &bytes[..n]) {
        return 0;
    }
    ctx.set_field_by_name(block, "position", Value::Int(position + n as i32));
    n
}

pub fn array_deque_size_direct(
    ctx: &dyn NativeContext,
    deque: ObjectRef,
) -> Result<i32, MethodCallFailed> {
    let tail = ctx.get_field_by_name(deque, "tail").as_int().unwrap_or(0);
    let head = ctx.get_field_by_name(deque, "head").as_int().unwrap_or(0);
    let elements = match ctx.get_field_by_name(deque, "elements") {
        Value::Object(Some(elements)) => elements,
        _ => return Ok(0),
    };
    let len = ctx.array_length(elements) as i32;
    if len <= 0 {
        return Ok(0);
    }
    let mut size = tail - head;
    if size < 0 {
        size += len;
    }
    Ok(size)
}

pub fn native_jackson_stream_read_constraints_max_name_length(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    Ok(args.first().copied())
}

pub fn register_spring_codec_intrinsics(registry: &mut NativeMethodRegistry) {
    registry.register(
        "org/springframework/core/codec/CharSequenceEncoder",
        "calculateCapacity",
        "(Ljava/lang/CharSequence;Ljava/nio/charset/Charset;)I",
        native_spring_char_sequence_encoder_calculate_capacity, NativeKind::SyntheticStub,
    );

    registry.register(
        "org/springframework/core/annotation/PackagesAnnotationFilter",
        "matches",
        "(Ljava/lang/String;)Z",
        native_spring_packages_annotation_filter_matches, NativeKind::SyntheticStub,
    );
    registry.register(
        "org/springframework/core/annotation/AnnotationFilter",
        "matches",
        "(Ljava/lang/Class;)Z",
        native_spring_annotation_filter_matches_class, NativeKind::SyntheticStub,
    );
    registry.register(
        "org/springframework/core/annotation/AnnotationsScanner",
        "hasPlainJavaAnnotationsOnly",
        "(Ljava/lang/Class;)Z",
        native_spring_has_plain_java_annotations_only_class, NativeKind::SyntheticStub,
    );
    registry.register(
        "org/springframework/core/annotation/AnnotationsScanner",
        "hasPlainJavaAnnotationsOnly",
        "(Ljava/lang/Object;)Z",
        native_spring_has_plain_java_annotations_only_object, NativeKind::SyntheticStub,
    );
    registry.register(
        "org/junit/platform/commons/util/AnnotationUtils",
        "isInJavaLangAnnotationPackage",
        "(Ljava/lang/Class;)Z",
        native_junit_is_in_java_lang_annotation_package, NativeKind::SyntheticStub,
    );
    // `--compatible` only. Under `--real-jdk` the real `SpringExtension
    // .resolveParameter` bytecode runs: this is a 2026-07-08 speed shortcut
    // (`ParallelExecutionSpringExtensionTests`), not a missing capability, and
    // a native has no caller class to resolve Spring names through. Its tail
    // called `ParameterResolutionDelegate.resolveDependency` BY NAME, i.e. the
    // Application loader's copy, for a test class a
    // `CompileWithForkedClassLoader` fork defined -- the Application copy then
    // built an Application `NamedParameterDependencyDescriptor`, and the fork's
    // `ConstructorResolver.setCurrentInjectionPoint` could not cast it to the
    // FORK's `InjectionPoint` (`AotIntegrationTests`, 15 + 164 failures once
    // the bytecode name rule stopped accepting same-named classes). See
    // docs/feature-designs/native-dispatch-architecture.md
    // spring-jdkonly-bytecode-cast-name-rule-jit-class-resolution-FIXED-20260923.md.
    if !registry.compatibility_mode().is_jdk_only() {
        registry.register(
            "org/springframework/test/context/junit/jupiter/SpringExtension",
            "resolveParameter",
            "(Lorg/junit/jupiter/api/extension/ParameterContext;Lorg/junit/jupiter/api/extension/ExtensionContext;)Ljava/lang/Object;",
            native_spring_extension_resolve_parameter, NativeKind::SyntheticStub,
        );
    }
}

pub fn native_spring_char_sequence_encoder_calculate_capacity(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let seq = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };

    let len = if let Some(text) = ctx.read_string(seq) {
        text.encode_utf16().count() as i64
    } else {
        match ctx.invoke_virtual(seq, "length", "()I", &[])? {
            Some(Value::Int(n)) if n > 0 => n as i64,
            _ => 0,
        }
    };

    let capacity = len.saturating_mul(8).min(i32::MAX as i64) as i32;
    Ok(Some(Value::Int(capacity)))
}

pub fn spring_java_class_name(ctx: &mut dyn NativeContext, class_obj: ObjectRef) -> Option<String> {
    match native_class_get_name(ctx, &[Value::Object(Some(class_obj))]) {
        Ok(Some(Value::Object(Some(name)))) => ctx.read_string(name),
        _ => None,
    }
}

pub fn spring_packages_filter_matches_text(
    ctx: &mut dyn NativeContext,
    filter: ObjectRef,
    annotation_type: &str,
) -> bool {
    let prefixes = match ctx.get_field_by_name(filter, "prefixes") {
        Value::Object(Some(prefixes)) => prefixes,
        _ => return false,
    };
    for i in 0..ctx.array_length(prefixes) {
        if let Value::Object(Some(prefix)) = ctx.get_array_element(prefixes, i) {
            if let Some(prefix) = ctx.read_string(prefix) {
                if annotation_type.starts_with(&prefix) {
                    return true;
                }
            }
        }
    }
    false
}

pub fn native_spring_packages_annotation_filter_matches(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let annotation_type = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(
        spring_packages_filter_matches_text(ctx, this, &annotation_type) as i32,
    )))
}

pub fn native_spring_annotation_filter_matches_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_obj = obj_arg(args, 1)?;
    let Some(type_name) = spring_java_class_name(ctx, class_obj) else {
        return Ok(Some(Value::Int(0)));
    };

    if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(this))
        .as_deref()
        == Some("org/springframework/core/annotation/PackagesAnnotationFilter")
    {
        return Ok(Some(Value::Int(
            spring_packages_filter_matches_text(ctx, this, &type_name) as i32,
        )));
    }

    let type_name_obj = ctx.create_string(&type_name);
    ctx.invoke_virtual(
        this,
        "matches",
        "(Ljava/lang/String;)Z",
        &[Value::Object(Some(type_name_obj))],
    )
}

pub fn spring_has_plain_java_annotations_only_name(name: &str) -> bool {
    name.starts_with("java.") || name == "org.springframework.core.Ordered"
}

pub fn native_spring_has_plain_java_annotations_only_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let class_obj = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let matched = spring_java_class_name(ctx, class_obj)
        .map(|name| spring_has_plain_java_annotations_only_name(&name))
        .unwrap_or(false);
    Ok(Some(Value::Int(matched as i32)))
}

pub fn native_spring_has_plain_java_annotations_only_object(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let annotated = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let class_name = ctx.class_name_of_id(ctx.class_id_of_object(annotated));
    if class_name.as_deref() == Some("java/lang/Class") {
        return native_spring_has_plain_java_annotations_only_class(ctx, args);
    }
    if matches!(
        class_name.as_deref(),
        Some("java/lang/reflect/Method")
            | Some("java/lang/reflect/Constructor")
            | Some("java/lang/reflect/Field")
    ) {
        if let Ok(Some(Value::Object(Some(declaring)))) =
            ctx.invoke_virtual(annotated, "getDeclaringClass", "()Ljava/lang/Class;", &[])
        {
            return native_spring_has_plain_java_annotations_only_class(
                ctx,
                &[Value::Object(Some(declaring))],
            );
        }
    }
    Ok(Some(Value::Int(0)))
}

pub fn spring_extension_invoke_special_anchored_on_test_class(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    test_class: ObjectRef,
    args: &[Value],
) -> MethodCallResult {
    // Shared anchoring helper: invokes a static method on `class_name`,
    // preferring the copy already loaded by `test_class`'s own defining
    // loader when that loader is user-defined (see
    // spring_extension_get_application_context's call site for the full
    // AOT double-context-refresh rationale). Falls through unchanged to
    // the original loader-blind ctx.invoke_special by name in every other
    // case -- purely additive, no behavior change outside forked-loader
    // tests. Reused for every SpringExtension static call this native
    // family makes that needs to land on the SAME copy of SpringExtension
    // the test class's own machinery uses (getApplicationContext,
    // findProperlyScopedExtensionContext).
    if let Some(test_class_id) = crate::lang_class::mirror_class_id(ctx, test_class) {
        let loader_id = ctx.loader_id_of_class(test_class_id);
        if loader_id >= 3 {
            if let Some(fork_class_id) =
                ctx.class_id_defined_by_loader_exact(class_name, loader_id as u32)
            {
                return ctx.invoke_by_class_id(
                    fork_class_id,
                    class_name,
                    method_name,
                    descriptor,
                    args,
                );
            }
        }
    }
    ctx.invoke_special(class_name, method_name, descriptor, args)
}

pub fn spring_extension_get_application_context(
    ctx: &mut dyn NativeContext,
    extension_context: ObjectRef,
) -> MethodCallResult {
    const SPRING_EXTENSION: &str = "org/springframework/test/context/junit/jupiter/SpringExtension";
    const GET_APP_CTX_DESC: &str =
        "(Lorg/junit/jupiter/api/extension/ExtensionContext;)Lorg/springframework/context/ApplicationContext;";

    // SpringExtension.resolveParameter is intercepted by a single, global
    // native override (registered once, independent of which ClassId the
    // receiver instance actually is) -- so this trampoline has no bytecode
    // frame of its own and no referencing_class_id for the interpreter's
    // usual loader-aware CONSTANT_Class resolution to key off. Left alone,
    // ctx.invoke_special(SPRING_EXTENSION, ...) resolves the class purely
    // by name through the loader-blind global path (load_class_concurrent),
    // which prefers the Application-loader copy whenever the ordinary
    // delegation chain can also serve the class -- even when the CURRENT
    // test class was loaded by a CompileWithForkedClassLoader fork that
    // has its OWN, already-loaded copy of SpringExtension. Since
    // SpringExtension.getTestContextManager keys its JUnit Store lookup
    // on Namespace.create(SpringExtension.class), invoking the WRONG copy
    // silently constructs a second, independent TestContextManager (a
    // Store miss on a different Class-identity Namespace) with its own
    // un-customized ApplicationContext -- this is the AOT bean-override
    // double-context-refresh bug (see
    // CRATONVM-SPRING-GENUINE-BUGLIST's AOT cluster
    // sections). Anchor resolution on the CURRENT test class's own loader
    // instead via the shared helper above; falls through to the existing,
    // unchanged global behavior when there's no fork-loaded copy.
    //
    // gc-common w20-c: `getRequiredTestClass` is Java, and `extension_context`
    // is passed on after it: read it back from a pin.
    let extension_context_pin = ctx.pin_native_root(extension_context);
    let test_class = ctx.invoke_virtual(
        extension_context,
        "getRequiredTestClass",
        "()Ljava/lang/Class;",
        &[],
    );
    let extension_context = ctx.read_native_pin(extension_context_pin, extension_context);
    ctx.unpin_native_roots(extension_context_pin);
    if let Ok(Some(Value::Object(Some(test_class)))) = test_class {
        return spring_extension_invoke_special_anchored_on_test_class(
            ctx,
            SPRING_EXTENSION,
            "getApplicationContext",
            GET_APP_CTX_DESC,
            test_class,
            &[Value::Object(Some(extension_context))],
        );
    }

    ctx.invoke_special(
        SPRING_EXTENSION,
        "getApplicationContext",
        GET_APP_CTX_DESC,
        &[Value::Object(Some(extension_context))],
    )
}
fn native_spring_extension_resolve_parameter(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let parameter_context = obj_arg(args, 1)?;
    let extension_context = obj_arg(args, 2)?;

    // This native makes many re-entrant calls, each a GC point. gc-common
    // w20-c: every reference it uses after one is kept in a GC-updated handle
    // and read back at the use (only the reflective Parameter was; the
    // parameter context, extension context, executable, test class,
    // application context, override handler and bean factory were raw).
    let mut scope = NativeHandleScope::new(ctx);
    let parameter_context_handle = scope.root(parameter_context);
    let mut extension_context_handle = scope.root(extension_context);
    let parameter = match scope.invoke_virtual(
        parameter_context,
        "getParameter",
        "()Ljava/lang/reflect/Parameter;",
        &[],
    )? {
        Some(Value::Object(Some(parameter))) => parameter,
        _ => return Ok(Some(Value::Object(None))),
    };
    let parameter_handle = scope.root(parameter);
    let parameter_context = scope.get(&parameter_context_handle);
    let index = scope
        .invoke_virtual(parameter_context, "getIndex", "()I", &[])?
        .and_then(|v| v.as_int())
        .unwrap_or(0);
    let parameter_context = scope.get(&parameter_context_handle);
    let executable = match scope.invoke_virtual(
        parameter_context,
        "getDeclaringExecutable",
        "()Ljava/lang/reflect/Executable;",
        &[],
    )? {
        Some(Value::Object(Some(executable))) => executable,
        _ => return Ok(Some(Value::Object(None))),
    };
    let executable_handle = scope.root(executable);
    let extension_context = scope.get(&extension_context_handle);
    let test_class = match scope.invoke_virtual(
        extension_context,
        "getRequiredTestClass",
        "()Ljava/lang/Class;",
        &[],
    )? {
        Some(Value::Object(Some(test_class))) => test_class,
        _ => return Ok(Some(Value::Object(None))),
    };
    let mut test_class_handle = scope.root(test_class);

    let executable = scope.get(&executable_handle);
    if scope
        .class_name_arc_of_id(scope.class_id_of_object(executable))
        .as_deref()
        == Some("java/lang/reflect/Constructor")
    {
        if let Ok(Some(Value::Object(Some(declaring)))) =
            scope.invoke_virtual(executable, "getDeclaringClass", "()Ljava/lang/Class;", &[])
        {
            test_class_handle = scope.root(declaring);
            let extension_context = scope.get(&extension_context_handle);
            if let Ok(Some(Value::Object(Some(scoped)))) = spring_extension_invoke_special_anchored_on_test_class(
                &mut *scope,
                "org/springframework/test/context/junit/jupiter/SpringExtension",
                "findProperlyScopedExtensionContext",
                "(Ljava/lang/Class;Lorg/junit/jupiter/api/extension/ExtensionContext;)Lorg/junit/jupiter/api/extension/ExtensionContext;",
                declaring,
                &[
                    Value::Object(Some(declaring)),
                    Value::Object(Some(extension_context)),
                ],
            ) {
                extension_context_handle = scope.root(scoped);
            }
        }
    }

    // Real Spring source (spring-framework-recheck, verified 2026-07-23) still
    // has an isBeanOverride(Parameter) short-circuit ahead of the generic
    // ParameterResolutionDelegate fallback: a @BeanOverride-annotated (e.g.
    // @MockitoBean/@MockitoSpyBean) constructor parameter with a resolvable
    // BeanOverrideHandler.getBeanName() is looked up DIRECTLY by name via
    // applicationContext.getBean(name), bypassing ambiguous by-type
    // autowiring entirely. The prior "narrowed" comment here claiming this
    // was removed upstream was wrong (or based on a stale/different
    // checkout) -- without this branch, EVERY bean-override constructor
    // parameter whose override name doesn't happen to equal the parameter's
    // own name (or carry an explicit @Qualifier) falls through to
    // ParameterResolutionDelegate.resolveDependency and throws
    // NoUniqueBeanDefinitionException, since all sibling override beans
    // share the same declared type. Restore the shortcut by calling the
    // real (unmodified) BeanOverrideUtils.resolveHandlerForParameter, which
    // itself performs the isBeanOverride check internally (returns null for
    // non-override parameters), so this is purely additive.
    let extension_context = scope.get(&extension_context_handle);
    let application_context =
        spring_extension_get_application_context(&mut *scope, extension_context)?;
    let application_context_obj = match application_context {
        Some(Value::Object(Some(application_context))) => application_context,
        other => return Ok(other),
    };
    let application_context_handle = scope.root(application_context_obj);

    // Anchor this on the TEST CLASS's own loader, exactly like the
    // SpringExtension statics above -- a plain `ctx.invoke_special` by name is
    // loader-blind and prefers the Application-loader copy.
    //
    // That matters here more than anywhere else in this native, because
    // `resolveHandlerForParameter` does its own annotation scan:
    // `MergedAnnotations.from(parameter).stream(BeanOverride.class)`. The
    // `BeanOverride.class` literal is resolved against the copy of
    // `BeanOverrideUtils` that is executing, so the Application-loader copy
    // asks "is this parameter annotated with the APPLICATION loader's
    // @BeanOverride?" about a parameter belonging to a class the AOT
    // `CompileWithForkedClassLoader`/`TestCompiler` fork loaded -- whose
    // @MockitoBean is meta-annotated with the FORK's @BeanOverride. The
    // answer is no, for every parameter, so the whole by-name shortcut
    // silently never fired during AOT replay and every @BeanOverride
    // constructor parameter fell through to `ParameterResolutionDelegate
    // .resolveDependency`. Parameters whose override name happens to equal
    // the parameter name (`@MockitoBean ExampleService s0A`) or that carry an
    // explicit `@Qualifier` still resolved by accident; the rest threw
    // `NoUniqueBeanDefinitionException` ("expected single matching bean but
    // found 4: s0A,s0B,s0C,nonExistingBean"), since every sibling override
    // bean shares the declared type. That is "Family A" of
    // `AotIntegrationTests#endToEndTestsForBeanOverrides` -- 13 failures
    // across `MockitoBeanByNameLookupForConstructorParametersIntegrationTests`,
    // `MockitoSpyBeanByNameLookupForConstructorParametersIntegrationTests` and
    // `MockitoBeansByNameIntegrationTests`, all of which pass in non-AOT mode
    // because there is no fork loader there for the two copies to disagree
    // across.
    //
    // Confirmed on HotSpot with no CratonVM involvement: calling the
    // Application-loader copy of `resolveHandlerForParameter` on a
    // fork-loaded parameter returns null for EVERY parameter, while the fork's
    // own copy returns the handlers (`service0B` -> beanName `s0B`).
    let parameter = scope.get(&parameter_handle);
    let test_class = scope.get(&test_class_handle);
    if let Ok(Some(Value::Object(Some(handler)))) = spring_extension_invoke_special_anchored_on_test_class(
        &mut *scope,
        "org/springframework/test/context/bean/override/BeanOverrideUtils",
        "resolveHandlerForParameter",
        "(Ljava/lang/reflect/Parameter;Ljava/lang/Class;)Lorg/springframework/test/context/bean/override/BeanOverrideHandler;",
        test_class,
        &[
            Value::Object(Some(parameter)),
            Value::Object(Some(test_class)),
        ],
    ) {
        if let Ok(Some(Value::Object(Some(bean_name)))) =
            scope.invoke_virtual(handler, "getBeanName", "()Ljava/lang/String;", &[])
        {
            // `bean_name` is the last call's result, so current.
            let application_context_obj = scope.get(&application_context_handle);
            return scope.invoke_virtual(
                application_context_obj,
                "getBean",
                "(Ljava/lang/String;)Ljava/lang/Object;",
                &[Value::Object(Some(bean_name))],
            );
        }
    }

    let application_context_obj = scope.get(&application_context_handle);
    let bean_factory_result = scope.invoke_virtual(
        application_context_obj,
        "getAutowireCapableBeanFactory",
        "()Lorg/springframework/beans/factory/config/AutowireCapableBeanFactory;",
        &[],
    )?;
    // `bean_factory` is that call's result, so current.
    let bean_factory = match bean_factory_result {
        Some(Value::Object(Some(bean_factory))) => bean_factory,
        _ => return Ok(Some(Value::Object(None))),
    };
    let parameter = scope.get(&parameter_handle);
    let test_class = scope.get(&test_class_handle);
    scope.invoke_special(
        "org/springframework/beans/factory/annotation/ParameterResolutionDelegate",
        "resolveDependency",
        "(Ljava/lang/reflect/Parameter;ILjava/lang/Class;Lorg/springframework/beans/factory/config/AutowireCapableBeanFactory;)Ljava/lang/Object;",
        &[
            Value::Object(Some(parameter)),
            Value::Int(index),
            Value::Object(Some(test_class)),
            Value::Object(Some(bean_factory)),
        ],
    )
}

