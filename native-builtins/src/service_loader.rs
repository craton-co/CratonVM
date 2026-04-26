//! RA.8 — real `java.util.ServiceLoader` implementation.
//!
//! Walks the classpath via the system ClassLoader's
//! `getResources("META-INF/services/<interface>")`, reads each provider
//! descriptor line-by-line, instantiates the listed classes, and
//! returns an iterator over them. This replaces the prior stub that
//! always returned an empty iterator and therefore blocked every JDK
//! feature that depends on service-provider lookup (Charset providers,
//! java.util.spi.* services, etc.).

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::{MethodCallFailed, MethodCallResult, VmError};
use rustjvm_types::Value;

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
        Err(_) => ctx.alloc_object(rustjvm_types::ClassId::new(0), 2),
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
    sl: rustjvm_types::ObjectRef,
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

    for fqn in providers {
        let name = ctx.create_string(&fqn);
        let class = ctx
            .invoke(
                "java/lang/Class",
                "forName",
                "(Ljava/lang/String;)Ljava/lang/Class;",
                &[Value::Object(Some(name))],
            )
            .ok()
            .and_then(|v| v);
        let class = match class {
            Some(Value::Object(Some(c))) => c,
            _ => continue,
        };
        // newInstance via Class.getDeclaredConstructor() + Constructor.newInstance().
        let empty_types = ctx.new_ref_array(
            ctx.class_id_by_name("java/lang/Class")
                .unwrap_or(rustjvm_types::ClassId::new(0)),
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
            _ => continue,
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
                .unwrap_or(rustjvm_types::ClassId::new(0)),
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
            _ => continue,
        };
        ctx.invoke(
            al_cls,
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(list)), Value::Object(Some(inst))],
        )?;
    }

    let it = ctx.invoke(
        al_cls,
        "iterator",
        "()Ljava/util/Iterator;",
        &[Value::Object(Some(list))],
    )?;
    Ok(it)
}

fn native_sl_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Defer to iterator() + StreamSupport.stream.
    let it = native_sl_iterator(ctx, args)?;
    let iter_obj = match it {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let spliterator = ctx.invoke(
        "java/util/Spliterators",
        "spliteratorUnknownSize",
        "(Ljava/util/Iterator;I)Ljava/util/Spliterator;",
        &[Value::Object(Some(iter_obj)), Value::Int(0)],
    )?;
    let sp_obj = match spliterator {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let stream = ctx.invoke(
        "java/util/stream/StreamSupport",
        "stream",
        "(Ljava/util/Spliterator;Z)Ljava/util/stream/Stream;",
        &[Value::Object(Some(sp_obj)), Value::Int(0)],
    )?;
    Ok(stream)
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
    r.register(sl, "findFirst", "()Ljava/util/Optional;", native_sl_find_first);
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
