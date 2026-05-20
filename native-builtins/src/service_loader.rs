//! RA.8 — real `java.util.ServiceLoader` implementation.
//!
//! Walks the classpath via the system ClassLoader's
//! `getResources("META-INF/services/<interface>")`, reads each provider
//! descriptor line-by-line, instantiates the listed classes, and
//! returns an iterator over them. This replaces the prior stub that
//! always returned an empty iterator and therefore blocked every JDK
//! feature that depends on service-provider lookup (Charset providers,
//! java.util.spi.* services, etc.).

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, VmError};
use cratonvm_types::Value;

/// `ServiceLoader.load(Class)` — use the thread context class loader.
fn native_sl_load_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let service = match args.first() {
        Some(Value::Object(Some(o))) => Value::Object(Some(*o)),
        _ => {
            return Err(MethodCallFailed::InternalError(VmError::Internal {
                message: "ServiceLoader.load(null)".to_string(),
            }));
        }
    };
    // Fetch the thread context class loader via Thread.currentThread().getContextClassLoader().
    let tcl = ctx
        .invoke(
            "java/lang/Thread",
            "currentThread",
            "()Ljava/lang/Thread;",
            &[],
        )
        .ok()
        .and_then(|v| v);
    let loader = match tcl {
        Some(Value::Object(Some(t))) => {
            ctx.invoke(
                "java/lang/Thread",
                "getContextClassLoader",
                "()Ljava/lang/ClassLoader;",
                &[Value::Object(Some(t))],
            )
            .ok()
            .and_then(|v| v)
            .unwrap_or(Value::Object(None))
        }
        _ => Value::Object(None),
    };
    build_service_loader(ctx, service, loader)
}

/// `ServiceLoader.load(Class, ClassLoader)` — pass through caller's loader.
fn native_sl_load_class_loader(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let service = match args.first() {
        Some(Value::Object(Some(o))) => Value::Object(Some(*o)),
        _ => {
            return Err(MethodCallFailed::InternalError(VmError::Internal {
                message: "ServiceLoader.load(null, ...)".to_string(),
            }));
        }
    };
    let loader = args.get(1).copied().unwrap_or(Value::Object(None));
    build_service_loader(ctx, service, loader)
}

fn build_service_loader(
    ctx: &mut dyn NativeContext,
    service: Value,
    loader: Value,
) -> MethodCallResult {
    let obj = match ctx.ensure_class_initialized("java/util/ServiceLoader") {
        Ok(cid) => {
            let real = ctx.class_num_total_fields(cid);
            ctx.alloc_object(cid, real.max(2))
        }
        Err(_) => ctx.alloc_object(cratonvm_types::ClassId::new(0), 2),
    };
    // Synthetic slots: [0]=serviceClass, [1]=loader.
    ctx.set_field(obj, 0, service);
    ctx.set_field(obj, 1, loader);
    // Dual-write for real-JDK ServiceLoader field names.
    ctx.set_field_by_name(obj, "service", service);
    ctx.set_field_by_name(obj, "loader", loader);
    Ok(Some(Value::Object(Some(obj))))
}

/// Read provider FQNs for `sl.service` from every
/// `META-INF/services/<fqcn>` resource visible on the classpath.
/// Each line in each resource file contributes one provider name.
///
/// WP1.8-narrow (session 94): the original implementation drove the
/// JDK chain `ClassLoader.getResources(String) -> Enumeration<URL> ->
/// URL.openStream() -> InputStreamReader.<init> -> BufferedReader.
/// <init>(Reader) -> readLine()`. That chain depends on synthetic-stub
/// surface (`URL.openStream`, `BufferedReader` constructor + readLine)
/// that is incomplete in the open-sourced revision and surfaces as
/// `NoSuchMethodError` at bytecode resolution before the proper natives
/// are reached. The WP7.1 commit (`245e996`) confirms this and works
/// around the gap in `jdbc.rs::collect_driver_providers` by walking the
/// classpath directly.
///
/// **Hybrid resolution (session 94 merge):** the WP1.8-narrow approach
/// (parsing `find_all_resource_urls` URL strings inline) and the
/// Wave 8 closure approach (a layered `find_all_resource_bytes` helper
/// across `ClassPath` / `ClassManager` / `NativeContext` / `Vm`) target
/// the same goal. The hybrid keeps WP1.8-narrow's structure and tests
/// but routes byte fetching through the layered helper — the URL-string
/// detour and the inline `read_resource_bytes` jar-cracker disappear,
/// leaving classpath-flavour handling owned by `class_path.rs` where
/// every classpath consumer (jdbc, here, anywhere else later) sees a
/// uniform implementation. Functionally identical to the prior
/// implementation when the JDK chain works (lex-sorted, dedup'd FQNs
/// from every classpath match); strictly more robust when it doesn't.
fn discover_providers(
    ctx: &mut dyn NativeContext,
    sl: cratonvm_types::ObjectRef,
) -> Result<Vec<String>, MethodCallFailed> {
    let service_class = match ctx.get_field_by_name(sl, "service") {
        Value::Object(Some(c)) => c,
        _ => match ctx.get_field(sl, 0) {
            Value::Object(Some(c)) => c,
            _ => {
                return Err(MethodCallFailed::InternalError(VmError::Internal {
                    message: "ServiceLoader: service class is null".to_string(),
                }));
            }
        },
    };
    let service_name_val = ctx.invoke(
        "java/lang/Class",
        "getName",
        "()Ljava/lang/String;",
        &[Value::Object(Some(service_class))],
    )?;
    let service_name = match service_name_val {
        Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    if service_name.is_empty() {
        return Err(MethodCallFailed::InternalError(VmError::Internal {
            message: "ServiceLoader: service.getName() returned null".to_string(),
        }));
    }
    let resource = format!("META-INF/services/{}", service_name);

    // Fetch every classpath match's bytes in one call. The layered
    // `find_all_resource_bytes` helper walks Directory / JarFile /
    // NestedJar / JmodFile / JImageFile entries; classpath-flavour
    // handling lives in `class_path.rs` so we do not need a local
    // jar-cracker here.
    let mut providers: Vec<String> = Vec::new();
    let descriptors = ctx.find_all_resource_bytes(&resource);
    for bytes in &descriptors {
        parse_provider_lines(bytes, &mut providers);
    }

    // Test mocks may stub `find_resource` without populating the
    // bytes list (the trait's default `find_all_resource_bytes` impl
    // returns empty); honour the single-resource fallback so a
    // fixture pointing at one descriptor still walks.
    if descriptors.is_empty() {
        if let Some(bytes) = ctx.find_resource(&resource) {
            parse_provider_lines(&bytes, &mut providers);
        }
    }

    providers.sort();
    providers.dedup();
    if matches!(
        std::env::var("CRATONVM_DIAG_SERVICELOADER").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    ) {
        eprintln!(
            "[SL-DBG] ServiceLoader.iterator service={} descriptors={} providers={} ({:?})",
            service_name,
            descriptors.len(),
            providers.len(),
            providers
        );
    }
    Ok(providers)
}

/// Tokenize a `META-INF/services/<spi>` descriptor. Each non-comment,
/// non-blank line is a provider FQN; everything after `#` on a line is
/// a comment. Mirrors `is_valid_provider_name`'s validation so callers
/// that reuse the parsed list don't need to re-filter.
fn parse_provider_lines(bytes: &[u8], out: &mut Vec<String>) {
    let text = match std::str::from_utf8(bytes) {
        Ok(t) => t,
        Err(_) => return,
    };
    for raw in text.lines() {
        let token = raw.split('#').next().unwrap_or("").trim();
        if !token.is_empty() && is_valid_provider_name(token) {
            out.push(token.to_string());
        }
    }
}

fn is_valid_provider_name(s: &str) -> bool {
    // JDK ServiceLoader spec: each line must be a fully-qualified
    // binary class name containing only Java identifier chars, `.`, and `$`.
    if s.is_empty() {
        return false;
    }
    s.chars().all(|c| {
        c.is_alphanumeric() || c == '.' || c == '_' || c == '$'
    })
}

/// `ServiceLoader.iterator()` — scan META-INF/services and return an
/// Iterator<Object> over instantiated providers.
fn native_sl_iterator(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let sl = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let providers = discover_providers(ctx, sl)?;

    // Build an ArrayList and populate with Class.forName(provider).newInstance().
    let al_cls = "java/util/ArrayList";
    let al_cid = ctx
        .ensure_class_initialized(al_cls)
        .map_err(|_| MethodCallFailed::InternalError(VmError::Internal {
            message: "ArrayList: not loaded".to_string(),
        }))?;
    let list = ctx.alloc_object(al_cid, ctx.class_num_total_fields(al_cid).max(4));
    ctx.invoke(al_cls, "<init>", "()V", &[Value::Object(Some(list))])?;

    let diag = matches!(
        std::env::var("CRATONVM_DIAG_SERVICELOADER").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    );
    if diag {
        eprintln!("[SL-DBG] iterator() entering loop with {} providers", providers.len());
    }
    for fqn in providers {
        if diag {
            eprintln!("[SL-DBG]   instantiate provider={fqn}");
        }
        let name = ctx.create_string(&fqn);
        let class_res = ctx.invoke(
            "java/lang/Class",
            "forName",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[Value::Object(Some(name))],
        );
        let class_opt = match &class_res {
            Ok(v) => *v,
            Err(e) => {
                if diag {
                    eprintln!("[SL-DBG]   forName({fqn}) raised: {e:?}");
                }
                None
            }
        };
        let class = match class_opt {
            Some(Value::Object(Some(c))) => c,
            other => {
                if diag {
                    eprintln!(
                        "[SL-DBG]   skip (forName returned {:?}): {fqn}",
                        other
                    );
                }
                continue;
            }
        };
        // newInstance via Class.getDeclaredConstructor() + Constructor.newInstance().
        let empty_types = ctx.new_ref_array(
            ctx.class_id_by_name("java/lang/Class")
                .unwrap_or(cratonvm_types::ClassId::new(0)),
            0,
        );
        let ctor = ctx
            .invoke(
                "java/lang/Class",
                "getDeclaredConstructor",
                "([Ljava/lang/Class;)Ljava/lang/reflect/Constructor;",
                &[Value::Object(Some(class)), Value::Object(Some(empty_types))],
            )
            .ok()
            .and_then(|v| v);
        let ctor = match ctor {
            Some(Value::Object(Some(c))) => c,
            _ => {
                if diag {
                    eprintln!("[SL-DBG]   skip (no zero-arg ctor): {fqn}");
                }
                continue;
            }
        };
        // setAccessible(true)
        let _ = ctx.invoke(
            "java/lang/reflect/AccessibleObject",
            "setAccessible",
            "(Z)V",
            &[Value::Object(Some(ctor)), Value::Int(1)],
        );
        let empty_args = ctx.new_ref_array(
            ctx.class_id_by_name("java/lang/Object")
                .unwrap_or(cratonvm_types::ClassId::new(0)),
            0,
        );
        let inst = ctx
            .invoke(
                "java/lang/reflect/Constructor",
                "newInstance",
                "([Ljava/lang/Object;)Ljava/lang/Object;",
                &[Value::Object(Some(ctor)), Value::Object(Some(empty_args))],
            )
            .ok()
            .and_then(|v| v);
        let inst = match inst {
            Some(Value::Object(Some(o))) => o,
            _ => {
                if diag {
                    eprintln!("[SL-DBG]   skip (newInstance returned null): {fqn}");
                }
                continue;
            }
        };
        ctx.invoke(
            al_cls,
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(list)), Value::Object(Some(inst))],
        )?;
    }

    if diag {
        let size = ctx
            .invoke(al_cls, "size", "()I", &[Value::Object(Some(list))])
            .ok()
            .and_then(|v| v);
        eprintln!(
            "[SL-DBG] iterator() final list size={:?}",
            size
        );
    }
    let it = ctx.invoke(
        al_cls,
        "iterator",
        "()Ljava/util/Iterator;",
        &[Value::Object(Some(list))],
    )?;
    Ok(it)
}

/// `ServiceLoader.stream()` — return a `Stream<Provider<S>>`-equivalent.
///
/// Root-cause note (session 95): the prior implementation routed through
/// `Spliterators.spliteratorUnknownSize(iterator(), 0)` followed by
/// `StreamSupport.stream(spliterator, false)`. In the open-sourced
/// non-synthetic-JDK build, `Spliterators.spliteratorUnknownSize` has NO
/// native registration (`register_p69_spliterator` only runs under
/// `register_synthetic_overrides`), so the call fell through to real-JDK
/// bytecode and produced a real `Spliterators$IteratorSpliterator`. The
/// `native_stream_support_stream_from_spliterator` override then read
/// field 0 of that real spliterator, found it was not our synthetic
/// backing `Object[]`, and fell into `drain_spliterator_to_stream` —
/// which is a give-up stub that always yields an EMPTY stream. Net
/// effect: `ServiceLoader.load(X).stream().count()` returned 0 even
/// though `iterator()` produced the providers correctly.
///
/// Fix: drain the iterator directly into an `Object[]` and build a
/// synthetic `java/util/stream/Stream` (field 0 = array) — the layout
/// every `Stream.*` native (`count`, `map`, `filter`, `toList`,
/// `forEach`, ...) already understands. No JDK Spliterator middleman.
fn native_sl_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let it = native_sl_iterator(ctx, args)?;
    let iter_obj = match it {
        Some(Value::Object(Some(o))) => o,
        // Null iterator → empty stream (not null) so downstream
        // `.count()` / `.filter()` natives have a valid receiver.
        _ => return alloc_synthetic_stream(ctx, &[]),
    };
    // Drain the iterator into a Vec<Value>.
    let mut collected: Vec<Value> = Vec::new();
    const SAFETY_CAP: usize = 1_000_000;
    loop {
        let has_next = ctx.invoke_virtual(iter_obj, "hasNext", "()Z", &[]);
        if !matches!(has_next, Ok(Some(Value::Int(1)))) {
            break;
        }
        let next = ctx.invoke_virtual(iter_obj, "next", "()Ljava/lang/Object;", &[]);
        match next {
            Ok(Some(v)) => collected.push(v),
            _ => break,
        }
        if collected.len() >= SAFETY_CAP {
            break;
        }
    }
    if matches!(
        std::env::var("CRATONVM_DIAG_SERVICELOADER").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    ) {
        eprintln!(
            "[SL-DBG] stream() drained {} providers into synthetic stream",
            collected.len()
        );
    }
    alloc_synthetic_stream(ctx, &collected)
}

fn native_sl_find_first(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let it = native_sl_iterator(ctx, args)?;
    let iter_obj = match it {
        Some(Value::Object(Some(o))) => o,
        _ => {
            return empty_optional(ctx);
        }
    };
    let has_next = ctx
        .invoke(
            "java/util/Iterator",
            "hasNext",
            "()Z",
            &[Value::Object(Some(iter_obj))],
        )?;
    if matches!(has_next, Some(Value::Int(0)) | None) {
        return empty_optional(ctx);
    }
    let first = ctx.invoke(
        "java/util/Iterator",
        "next",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(iter_obj))],
    )?;
    let first_obj = match first {
        Some(Value::Object(Some(o))) => o,
        _ => return empty_optional(ctx),
    };
    let opt = ctx.invoke(
        "java/util/Optional",
        "of",
        "(Ljava/lang/Object;)Ljava/util/Optional;",
        &[Value::Object(Some(first_obj))],
    )?;
    Ok(opt)
}

fn empty_optional(ctx: &mut dyn NativeContext) -> MethodCallResult {
    let empty = ctx.invoke(
        "java/util/Optional",
        "empty",
        "()Ljava/util/Optional;",
        &[],
    )?;
    Ok(empty)
}

/// `ServiceLoader.spliterator()` — the JDK inherits `Iterable.spliterator()`'s
/// default body (`Spliterators.spliteratorUnknownSize(iterator(), 0)`), but
/// the default-method dispatch through our interpreter has historically not
/// propagated elements through to the resulting stream (the empty-stream
/// surfaces as Elasticsearch's `AssertionError: available names are []` from
/// `CliToolProvider.load` even though our native `ServiceLoader.iterator()`
/// returns 13 providers). Provide a direct native that builds a synthetic
/// 3-field Spliterator (array, pos, fence) backed by the freshly-discovered
/// providers so callers like `sl.spliterator().stream().filter(...)` see the
/// real provider list.
fn native_sl_spliterator(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Drive the existing iterator() native to produce an Iterator over the
    // instantiated providers, then drain it into an Object[]. The whole
    // approach mirrors `Spliterators.spliteratorUnknownSize` but is wired
    // directly onto `ServiceLoader` so the call site doesn't depend on the
    // `Iterable.spliterator()` default-method machinery.
    let iter_val = native_sl_iterator(ctx, args)?;
    let iter = match iter_val {
        Some(Value::Object(Some(o))) => o,
        _ => {
            let empty = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
            let cid = ctx.ensure_class_initialized("java/util/Spliterator")
                .unwrap_or(cratonvm_types::ClassId::new(0));
            let n = ctx.class_num_total_fields(cid).max(3);
            let obj = ctx.alloc_object(cid, n);
            ctx.set_field(obj, 0, Value::Object(Some(empty)));
            ctx.set_field(obj, 1, Value::Int(0));
            ctx.set_field(obj, 2, Value::Int(0));
            return Ok(Some(Value::Object(Some(obj))));
        }
    };
    // Drain the iterator into a Vec<Value>.
    let mut collected: Vec<Value> = Vec::new();
    const SAFETY_CAP: usize = 1_000_000;
    loop {
        let has_next = ctx.invoke_virtual(iter, "hasNext", "()Z", &[]);
        if !matches!(has_next, Ok(Some(Value::Int(1)))) {
            break;
        }
        let next = ctx.invoke_virtual(iter, "next", "()Ljava/lang/Object;", &[]);
        let val = match next {
            Ok(Some(v)) => v,
            _ => break,
        };
        collected.push(val);
        if collected.len() >= SAFETY_CAP {
            break;
        }
    }
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, collected.len());
    for (i, v) in collected.iter().enumerate() {
        ctx.set_array_element(arr, i, *v);
    }
    let cid = ctx.ensure_class_initialized("java/util/Spliterator")
        .unwrap_or(cratonvm_types::ClassId::new(0));
    let n = ctx.class_num_total_fields(cid).max(3);
    let obj = ctx.alloc_object(cid, n);
    ctx.set_field(obj, 0, Value::Object(Some(arr)));
    ctx.set_field(obj, 1, Value::Int(0));
    ctx.set_field(obj, 2, Value::Int(collected.len() as i32));
    Ok(Some(Value::Object(Some(obj))))
}

/// Late-binding override for `StreamSupport.stream(Spliterator, boolean)`.
///
/// Background — the prior `phases_late.rs::register_p69_spliterator`
/// registration of the same triple is, for reasons specific to this binary
/// (very large source file, incremental-compile interaction) not making it
/// into the final `cratonvm.exe`: stress-checking the binary with `grep -ao`
/// over the literal string `"[STREAM-SUPPORT-DBG]"` shows it absent, and
/// runtime traces of `ServiceLoader.load(...).spliterator().stream()
/// .filter(...)` from Elasticsearch's `CliToolProvider.load` never trigger
/// the registered native — `Stream.filter` runs on real-JDK ReferencePipeline
/// bytecode against an empty stream, surfacing as
///   `AssertionError: CliToolProvider [server] not found, available names are []`
/// even though our native `ServiceLoader.iterator()` had just yielded
/// 13 providers.
///
/// Re-registering here (from a small, less-noisy translation unit) ensures
/// the closure is the LAST writer to the `NativeMethodRegistry` HashMap for
/// the `(StreamSupport, stream, (Spliterator,Z)Stream)` triple. The body
/// drains the supplied synthetic spliterator's backing Object[] into a fresh
/// synthetic Stream so the downstream `Stream.filter` / `Stream.toList`
/// natives see the real provider list.
fn native_stream_support_stream_from_spliterator(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let spliterator = match args.first() {
        Some(Value::Object(Some(s))) => *s,
        _ => {
            // Null spliterator → empty stream.
            return alloc_synthetic_stream(ctx, &[]);
        }
    };
    // Read field 0 of the spliterator. Our `Spliterators.spliteratorUnknownSize`
    // and `ServiceLoader.spliterator()` natives both place a fully-materialised
    // Object[] in field 0 — read it directly. For real-JDK Spliterator subclasses
    // whose field 0 isn't an array, fall back to draining via
    // `forEachRemaining(Consumer)`.
    let field0 = ctx.get_field(spliterator, 0);
    let arr = match field0 {
        Value::Object(Some(a))
            if ctx.heap_kind_of(a) == cratonvm_types::ObjectKind::Array => a,
        _ => return drain_spliterator_to_stream(ctx, spliterator),
    };
    let pos = match ctx.get_field(spliterator, 1) {
        Value::Int(v) => v as usize,
        _ => 0,
    };
    let fence = match ctx.get_field(spliterator, 2) {
        Value::Int(v) => v as usize,
        _ => ctx.array_length(arr),
    };
    // Snapshot the slice [pos, fence) into a fresh array so the resulting
    // Stream's lifetime is independent of the spliterator's cursor.
    let n = fence.saturating_sub(pos);
    let snapshot = ctx.new_array(cratonvm_types::ArrayElementType::Reference, n);
    for i in 0..n {
        let v = ctx.get_array_element(arr, pos + i);
        ctx.set_array_element(snapshot, i, v);
    }
    let cid = ctx.ensure_class_initialized("java/util/stream/Stream")
        .unwrap_or(cratonvm_types::ClassId::new(0));
    let nfields = ctx.class_num_total_fields(cid).max(1);
    let stream = ctx.alloc_object(cid, nfields);
    ctx.set_field(stream, 0, Value::Object(Some(snapshot)));
    Ok(Some(Value::Object(Some(stream))))
}

fn alloc_synthetic_stream(
    ctx: &mut dyn NativeContext,
    elems: &[Value],
) -> MethodCallResult {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, elems.len());
    for (i, v) in elems.iter().enumerate() {
        ctx.set_array_element(arr, i, *v);
    }
    let cid = ctx.ensure_class_initialized("java/util/stream/Stream")
        .unwrap_or(cratonvm_types::ClassId::new(0));
    let nfields = ctx.class_num_total_fields(cid).max(1);
    let stream = ctx.alloc_object(cid, nfields);
    ctx.set_field(stream, 0, Value::Object(Some(arr)));
    Ok(Some(Value::Object(Some(stream))))
}

fn drain_spliterator_to_stream(
    ctx: &mut dyn NativeContext,
    spliterator: cratonvm_types::ObjectRef,
) -> MethodCallResult {
    // Best-effort drain via `tryAdvance(Consumer)` — bounded.
    let mut collected: Vec<Value> = Vec::new();
    const SAFETY_CAP: usize = 1_000_000;
    // We can't pass a closure to JDK code; instead, repeatedly call
    // `tryAdvance` and rely on the side-effect of advancing the spliterator's
    // cursor while a no-op consumer absorbs the element. Since we don't have a
    // way to inject a side-channel consumer here, fall through to an empty
    // stream rather than risk infinite-looping a misbehaving spliterator.
    let _ = (spliterator, &mut collected, SAFETY_CAP);
    alloc_synthetic_stream(ctx, &[])
}

pub fn register_service_loader_natives(r: &mut NativeMethodRegistry) {
    let sl = "java/util/ServiceLoader";
    r.register(
        sl,
        "load",
        "(Ljava/lang/Class;)Ljava/util/ServiceLoader;",
        native_sl_load_class,
    );
    r.register(
        sl,
        "load",
        "(Ljava/lang/Class;Ljava/lang/ClassLoader;)Ljava/util/ServiceLoader;",
        native_sl_load_class_loader,
    );
    r.register(
        sl,
        "loadInstalled",
        "(Ljava/lang/Class;)Ljava/util/ServiceLoader;",
        native_sl_load_class,
    );
    r.register(sl, "iterator", "()Ljava/util/Iterator;", native_sl_iterator);
    r.register(sl, "stream", "()Ljava/util/stream/Stream;", native_sl_stream);
    r.register(sl, "spliterator", "()Ljava/util/Spliterator;", native_sl_spliterator);
    r.register(sl, "findFirst", "()Ljava/util/Optional;", native_sl_find_first);

    // Re-register `StreamSupport.stream(Spliterator, boolean)` — see the
    // header comment on `native_stream_support_stream_from_spliterator`. This
    // must run LAST to win the registration race against the prior phase69
    // registration that doesn't survive linking in this binary.
    r.register(
        "java/util/stream/StreamSupport",
        "stream",
        "(Ljava/util/Spliterator;Z)Ljava/util/stream/Stream;",
        native_stream_support_stream_from_spliterator,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_name_validation_accepts_fqn() {
        assert!(is_valid_provider_name("com.acme.Foo"));
        assert!(is_valid_provider_name("a.b$Inner"));
        assert!(is_valid_provider_name("Foo_1"));
    }

    #[test]
    fn provider_name_validation_rejects_junk() {
        assert!(!is_valid_provider_name(""));
        assert!(!is_valid_provider_name("foo bar"));
        assert!(!is_valid_provider_name("foo;bar"));
        assert!(!is_valid_provider_name("!foo"));
    }

    #[test]
    fn comment_stripping_drops_hash_suffix() {
        let raw = "com.acme.Foo  # comment";
        let token = raw.split('#').next().unwrap().trim();
        assert_eq!(token, "com.acme.Foo");
    }

    /// WP1.8-narrow: the descriptor parser strips comments and blanks,
    /// rejects malformed lines, and returns valid FQNs unchanged. This
    /// is the load-bearing parser the iterator now uses (replacing the
    /// JDK BufferedReader chain that the open-sourced revision can't
    /// drive end-to-end).
    #[test]
    fn parse_provider_lines_handles_comments_blanks_and_junk() {
        let body = b"# header comment\n  com.acme.A  \n\ncom.acme.B # trailing\nfoo bar\n!bad\nGood$Inner\n";
        let mut out = Vec::new();
        parse_provider_lines(body, &mut out);
        assert_eq!(out, vec!["com.acme.A", "com.acme.B", "Good$Inner"]);
    }

    /// WP1.8-narrow: invalid UTF-8 bytes do not panic the parser; they
    /// are silently dropped. ServiceLoader descriptors are spec'd as
    /// UTF-8 but a malformed JAR shouldn't kill discovery for the rest
    /// of the classpath.
    #[test]
    fn parse_provider_lines_silently_drops_non_utf8() {
        let body: &[u8] = &[0xff, 0xfe, b'\n'];
        let mut out = Vec::new();
        parse_provider_lines(body, &mut out);
        assert!(out.is_empty());
    }
}
