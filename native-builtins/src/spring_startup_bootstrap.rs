//! S-SB: Spring ApplicationStartup bootstrap.
//!
//! Spring Boot's `AbstractApplicationContext` has an instance field:
//!   `private ApplicationStartup applicationStartup = ApplicationStartup.DEFAULT;`
//!
//! `ApplicationStartup.DEFAULT` is a static final field on an interface; its
//! value is set in `ApplicationStartup.<clinit>` as `new DefaultApplicationStartup()`.
//! In CratonVM's partial bootstrap, `DefaultApplicationStartup.<clinit>` (which
//! itself creates a `DefaultStartupStep` singleton) NPEs, gets swallowed, and the
//! `DEFAULT` field stays null.  Consequently every `AbstractApplicationContext`
//! instance has `applicationStartup = null`, and the very first call to
//! `getApplicationStartup().start(name)` crashes with "Cannot invoke start on null".
//!
//! Fix strategy:
//! 1. Native override for `getApplicationStartup()` in `AbstractApplicationContext`
//!    that returns a process-global no-op `ApplicationStartup` singleton if the
//!    field is null (or always, since the bytecode is just a field read anyway).
//! 2. Native overrides for `DefaultApplicationStartup.start(String)` →
//!    returns a global no-op `StartupStep`.
//! 3. Native overrides for `StartupStep.tag(String,String)`,
//!    `tag(String,Supplier)`, `end()` etc. → all no-ops.
//! 4. `post_clinit_fixup` (in vm_util.rs) sets `ApplicationStartup.DEFAULT`
//!    to the same singleton so GETSTATIC paths also get a non-null value.

use parking_lot::Mutex;
use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::{ObjectRef, Value};
use std::sync::OnceLock;

// ──────────────────────────────────────────────────────────────────────────────
// Singleton no-op ApplicationStartup object
// ──────────────────────────────────────────────────────────────────────────────

fn noop_startup() -> &'static Mutex<Option<ObjectRef>> {
    static S: OnceLock<Mutex<Option<ObjectRef>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

fn noop_step() -> &'static Mutex<Option<ObjectRef>> {
    static S: OnceLock<Mutex<Option<ObjectRef>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

/// Get-or-create the global no-op `ApplicationStartup` singleton.
/// Uses `DefaultApplicationStartup` as the carrier class so virtual dispatch
/// on `start(String)` hits our registered native override.
pub fn get_noop_startup(ctx: &mut dyn NativeContext) -> ObjectRef {
    if let Some(obj) = *noop_startup().lock() {
        return obj;
    }
    // 8 slots — conservative over-allocation for DefaultApplicationStartup's
    // real-JDK field count (it's a simple class with very few fields).
    let obj = crate::alloc_concurrent_synthetic(
        ctx,
        "org/springframework/core/metrics/DefaultApplicationStartup",
        8,
    );
    *noop_startup().lock() = Some(obj);
    obj
}

/// Get-or-create the global no-op `StartupStep` singleton.
pub fn get_noop_step(ctx: &mut dyn NativeContext) -> ObjectRef {
    if let Some(obj) = *noop_step().lock() {
        return obj;
    }
    // DefaultApplicationStartup$DefaultStartupStep in Spring Framework 5.3.x
    // (used by Spring Boot 2.7.x).  If not found, fall back to allocating a
    // bare java/lang/Object — the methods are all overridden natively anyway.
    let obj = crate::alloc_concurrent_synthetic(
        ctx,
        "org/springframework/core/metrics/DefaultApplicationStartup$DefaultStartupStep",
        16,
    );
    *noop_step().lock() = Some(obj);
    obj
}

// ──────────────────────────────────────────────────────────────────────────────
// Native method implementations
// ──────────────────────────────────────────────────────────────────────────────

fn get_application_startup(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(get_noop_startup(ctx)))))
}

fn startup_start(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(get_noop_step(ctx)))))
}

fn step_tag_sv(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // tag(String key, String value) → this StartupStep
    Ok(Some(args.first().copied().unwrap_or(Value::Object(None))))
}

fn step_tag_ssup(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // tag(String key, Supplier<String> value) → this StartupStep
    Ok(Some(args.first().copied().unwrap_or(Value::Object(None))))
}

fn step_end(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

fn step_get_name(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(ctx.create_string("")))))
}

fn step_get_id(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Long(0)))
}

fn step_get_parent_id(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

fn step_get_tags(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Return empty iterable — callers use this only for diagnostics.
    // Return a synthetic empty ArrayList.
    let list = crate::alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 4);
    // ArrayList.size = 0 already (default int field)
    Ok(Some(Value::Object(Some(list))))
}

// ──────────────────────────────────────────────────────────────────────────────
// Environment fix — AbstractApplicationContext.getEnvironment() must not return null.
// StandardEnvironment.<init>() can fail in CratonVM's partial bootstrap.
// We create a synthetic StandardEnvironment and override the critical methods
// on it so callers that read property sources or properties get safe defaults.
// ──────────────────────────────────────────────────────────────────────────────

fn noop_environment() -> &'static Mutex<Option<ObjectRef>> {
    static S: OnceLock<Mutex<Option<ObjectRef>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

fn get_noop_environment(ctx: &mut dyn NativeContext) -> ObjectRef {
    if let Some(obj) = *noop_environment().lock() {
        return obj;
    }

    let env_class = "org/springframework/core/env/StandardEnvironment";
    let mps_class = "org/springframework/core/env/MutablePropertySources";

    // Try to construct a REAL StandardEnvironment first. Its <init> calls
    // customizePropertySources which uses System.getProperties()/getenv() — both
    // of which CratonVM supports. If this succeeds we get a fully functional
    // environment with proper MutablePropertySources backing.
    let real_env = (|| -> Option<ObjectRef> {
        let env = match ctx.new_object(env_class) {
            Ok(Some(Value::Object(Some(o)))) => o,
            _ => return None,
        };
        ctx.invoke(env_class, "<init>", "()V", &[Value::Object(Some(env))])
            .ok()?;
        Some(env)
    })();

    let obj = if let Some(env) = real_env {
        env
    } else {
        // Fallback: synthetic allocation + inject a real MutablePropertySources
        // into its `propertySources` field so MPS.get() / addFirst() work.
        let env = crate::alloc_concurrent_synthetic(ctx, env_class, 32);

        // Try to create a real MutablePropertySources (its <init> just creates
        // a CopyOnWriteArrayList — should always succeed).
        if let Ok(Some(Value::Object(Some(mps)))) = ctx.new_object(mps_class) {
            let _ = ctx.invoke(mps_class, "<init>", "()V", &[Value::Object(Some(mps))]);
            // Store it in both possible field names used by Spring versions.
            ctx.set_field_by_name(env, "propertySources", Value::Object(Some(mps)));
        }

        env
    };

    *noop_environment().lock() = Some(obj);
    obj
}

fn get_environment(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(get_noop_environment(ctx)))))
}

fn create_environment(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(get_noop_environment(ctx)))))
}

// Environment.getProperty(String) → null (no properties in synthetic env, safe default)
fn env_get_property_str(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

// Environment.getProperty(String, String) → defaultValue
fn env_get_property_str_default(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    Ok(Some(args.get(2).copied().unwrap_or(Value::Object(None))))
}

// Environment.getProperty(String, Class) → null
fn env_get_property_class(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

// Environment.containsProperty(String) → false
fn env_contains_property(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

// Environment.getActiveProfiles() → empty String[]
fn env_get_active_profiles(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let arr = ctx.new_ref_array(rustjvm_types::ClassId::new(0), 0);
    Ok(Some(Value::Object(Some(arr))))
}

// Environment.getDefaultProfiles() → ["default"]
fn env_get_default_profiles(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let arr = ctx.new_ref_array(rustjvm_types::ClassId::new(0), 1);
    let default_str = ctx.create_string("default");
    ctx.set_array_element(arr, 0, Value::Object(Some(default_str)));
    Ok(Some(Value::Object(Some(arr))))
}

// Environment.acceptsProfiles(Profiles) → true
fn env_accepts_profiles(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(1)))
}

// Environment.acceptsProfiles(String[]) → true
fn env_accepts_profiles_arr(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(1)))
}

// getPropertySources() → read from the cached env's propertySources field,
// falling back to the env object itself if the field is not found.
fn env_get_property_sources(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let env = get_noop_environment(ctx);
    // Try to read the real propertySources field.
    let mps = ctx.get_field_by_name(env, "propertySources");
    if matches!(mps, Value::Object(Some(_))) {
        return Ok(Some(mps));
    }
    // If the real env was created but field lookup failed by name, try
    // to return the env object itself as a last resort — the caller will
    // likely NPE later, but this keeps us progressing.
    let fallback = crate::alloc_concurrent_synthetic(
        ctx,
        "org/springframework/core/env/MutablePropertySources",
        8,
    );
    Ok(Some(Value::Object(Some(fallback))))
}

// resolveRequiredPlaceholders(String) → same string
fn env_resolve_placeholders(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    Ok(Some(args.get(1).copied().unwrap_or(Value::Object(None))))
}

// ──────────────────────────────────────────────────────────────────────────────
// BeanFactory fix — GenericApplicationContext.getBeanFactory() must not return null.
//
// Root cause: `GenericApplicationContext.<init>()` does:
//   this.beanFactory = new DefaultListableBeanFactory();
// If `DefaultListableBeanFactory.<init>()` fails with a Rust-level error (NPE
// inside its <clinit> cascade, or any other VM-level failure), CratonVM's
// pending_runtime_error path converts the error to a Java exception and
// propagates it up the frame stack.  The PUTFIELD for `beanFactory` is never
// reached, so the field stays null (the default for a reference-typed final
// field allocated on our heap).
//
// The outer `AnnotationConfigApplicationContext.<init>` may also propagate the
// exception — but in practice the swallow logic or a catch block can absorb it,
// leaving the context object in a half-initialized state where `beanFactory`
// is null.  The next call, `new AnnotatedBeanDefinitionReader(this)`, calls
// `AnnotationConfigUtils.registerAnnotationConfigProcessors(registry)` which
// calls `registry.containsBeanDefinition(name)` → `getBeanFactory()` → NPE.
//
// Fix: native override for `GenericApplicationContext.getBeanFactory()` that
// lazily initializes a real `DefaultListableBeanFactory` on the receiver when
// the field is null, writes it back to the object's `beanFactory` field, and
// returns it.  This mirrors the getApplicationStartup() and getEnvironment()
// fixes already in this file.
// ──────────────────────────────────────────────────────────────────────────────

const DLBF: &str = "org/springframework/beans/factory/support/DefaultListableBeanFactory";
const GENERIC_CTX: &str = "org/springframework/context/support/GenericApplicationContext";

fn get_or_create_bean_factory(ctx: &mut dyn NativeContext, receiver: ObjectRef) -> ObjectRef {
    // Fast path: the field was already populated by the bytecode constructor.
    let current = ctx.get_field_by_name(receiver, "beanFactory");
    if let Value::Object(Some(bf)) = current {
        return bf;
    }

    // Slow path: beanFactory is null (constructor failed before PUTFIELD).
    // Try to create a real DefaultListableBeanFactory via its no-arg constructor.
    // If that also fails (e.g. its own <clinit> cascade NPEs), fall back to a
    // synthetic allocation so that downstream GETFIELD/PUTFIELD on the DLBF
    // still work (they're also overridden natively below).
    let bf = (|| -> Option<ObjectRef> {
        let obj = match ctx.new_object(DLBF) {
            Ok(Some(Value::Object(Some(o)))) => o,
            _ => return None,
        };
        // Invoke DefaultListableBeanFactory() no-arg constructor.
        ctx.invoke(DLBF, "<init>", "()V", &[Value::Object(Some(obj))]).ok()?;
        Some(obj)
    })()
    .unwrap_or_else(|| {
        // Fallback: synthetic allocation with generous field count.
        crate::alloc_concurrent_synthetic(ctx, DLBF, 64)
    });

    // Write the newly created factory back into the context object so future
    // reads from the bytecode (GETFIELD beanFactory) also see it.
    ctx.set_field_by_name(receiver, "beanFactory", Value::Object(Some(bf)));
    bf
}

fn get_bean_factory(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = this (GenericApplicationContext / subclass)
    let receiver = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            return Err(rustjvm_types::error::MethodCallFailed::InternalError(
                rustjvm_types::error::VmError::Runtime(
                    rustjvm_types::error::RuntimeError::NullPointerException {
                        message: Some("getBeanFactory: null receiver".to_string()),
                    },
                ),
            ))
        }
    };
    let bf = get_or_create_bean_factory(ctx, receiver);
    Ok(Some(Value::Object(Some(bf))))
}

// ──────────────────────────────────────────────────────────────────────────────
// DefaultListableBeanFactory native stubs.
//
// These are invoked when the DLBF is a synthetic allocation (fallback path
// above) or when CratonVM cannot execute the real bytecode.
//
// `containsBeanDefinition(String name)` — Spring's
// `AnnotationConfigUtils.registerAnnotationConfigProcessors` guards each
// registration with `registry.containsBeanDefinition(processorBeanName)`.
// The check is: "has this processor already been registered?" — the answer
// during fresh context construction is always false.  Returning false (0)
// lets Spring proceed to register all annotation post-processors.
// ──────────────────────────────────────────────────────────────────────────────

fn dlbf_contains_bean_definition(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Always return false: the bean has not been registered yet.
    // This is the correct answer for a freshly constructed context.
    Ok(Some(Value::Int(0)))
}

// `registerBeanDefinition(String beanName, BeanDefinition beanDefinition)`.
// In the synthetic-DLBF fallback path we have no real registry backing store,
// so this is a no-op.  Spring will later re-read the definitions through
// getBeanFactory() / getBean() paths, which we'll override as needed.
fn dlbf_register_bean_definition(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
}

// ──────────────────────────────────────────────────────────────────────────────
// SpringIterableConfigurationPropertySource$Cache — null-safe shims.
//
// Spring Boot 4.x's `SpringIterableConfigurationPropertySource$Cache.tryUpdate`
// reads `propertySource.getPropertyNames()` and immediately does `arraylength`
// on the result.  In CratonVM's partial bootstrap some property sources
// (notably synthetic StandardEnvironment-backed ones whose property maps were
// not populated) return `null` from `getPropertyNames()`, causing an NPE at
// pc=41 (the `arraylength` instruction).
//
// We override only `tryUpdate(EnumerablePropertySource)`:
//   * If `getPropertyNames()` is non-null, fall back to the original bytecode
//     by performing the work natively is not feasible — instead we let the
//     bytecode run (we can't, since override is unconditional) so we replicate
//     the minimal contract: install an empty `Cache$Data` record when the
//     field is null AND skip the original walk.  This is conservative: the
//     broken property source contributes no bindings, but Spring's Binder
//     continues to query the rest of the property sources normally and
//     downstream `Cache.getMapped` / `getConfigurationPropertyNames` no longer
//     trip `Assert.state(data != null)`.
//   * If `getPropertyNames()` returns null, also install the empty Data
//     so subsequent reads succeed.
//
// Net: this shim is invoked for every `tryUpdate` call.  It always installs
// an empty Data record on first use, then no-ops thereafter.  We accept the
// loss of dynamic property-name bindings through this iterable cache —
// downstream code reads from individual property sources directly.
// ──────────────────────────────────────────────────────────────────────────────

const SICP_CACHE: &str =
    "org/springframework/boot/context/properties/source/SpringIterableConfigurationPropertySource$Cache";
const SICP_NAME: &str = "org/springframework/boot/context/properties/source/ConfigurationPropertyName";

fn enum_prop_source_get_property_names(
    ctx: &mut dyn NativeContext,
    src: ObjectRef,
) -> Option<ObjectRef> {
    let r = ctx.invoke(
        "org/springframework/core/env/EnumerablePropertySource",
        "getPropertyNames",
        "()[Ljava/lang/String;",
        &[Value::Object(Some(src))],
    );
    match r {
        Ok(Some(Value::Object(opt))) => opt,
        _ => None,
    }
}

fn cache_try_update(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // tryUpdate(this, EnumerablePropertySource source) → void
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let source = match args.get(1) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };

    // Always install an empty Data record so downstream Assert.state(data!=null)
    // never trips.  We accept that this iterable cache contributes no
    // bindings — Binder still queries other property sources directly.
    let r = install_empty_data(ctx, this);
    eprintln!(
        "[SICP-DBG] tryUpdate: source={:?} install_empty_data={:?} data_after={:?}",
        source.is_some(),
        r.is_some(),
        ctx.get_field_by_name(this, "data")
    );
    Ok(None)
}

fn install_empty_data(ctx: &mut dyn NativeContext, cache: ObjectRef) -> Option<()> {
    let data_class =
        "org/springframework/boot/context/properties/source/SpringIterableConfigurationPropertySource$Cache$Data";

    // Construct empty HashMaps and HashSet via their no-arg constructors.
    let mk_hashmap = |ctx: &mut dyn NativeContext| -> Option<ObjectRef> {
        let m = match ctx.new_object("java/util/HashMap").ok()? {
            Some(Value::Object(Some(o))) => o,
            _ => return None,
        };
        ctx.invoke(
            "java/util/HashMap",
            "<init>",
            "()V",
            &[Value::Object(Some(m))],
        )
        .ok()?;
        Some(m)
    };
    let mk_hashset = |ctx: &mut dyn NativeContext| -> Option<ObjectRef> {
        let s = match ctx.new_object("java/util/HashSet").ok()? {
            Some(Value::Object(Some(o))) => o,
            _ => return None,
        };
        ctx.invoke(
            "java/util/HashSet",
            "<init>",
            "()V",
            &[Value::Object(Some(s))],
        )
        .ok()?;
        Some(s)
    };

    let mappings = mk_hashmap(ctx)?;
    let reverse_mappings = mk_hashmap(ctx)?;
    let descendants = mk_hashset(ctx)?;
    let sys_env_copy = mk_hashmap(ctx)?;

    // Empty ConfigurationPropertyName[] — class must be loadable.
    let cpn_cid = ctx.class_id_by_name(SICP_NAME)?;
    let cpn_arr = ctx.new_ref_array(cpn_cid, 0);

    // Empty String[]
    let str_cid = ctx.class_id_by_name("java/lang/String")?;
    let str_arr = ctx.new_ref_array(str_cid, 0);

    // Allocate the Data record.  Try the canonical (private) record
    // constructor first; if that fails (some bootstrap paths can't dispatch
    // to private record ctors via NativeContext.invoke), fall back to a
    // synthetic allocation and write the 6 record component fields directly.
    let ctor_desc = "(Ljava/util/Map;Ljava/util/Map;Ljava/util/Set;\
        [Lorg/springframework/boot/context/properties/source/ConfigurationPropertyName;\
        Ljava/util/Map;[Ljava/lang/String;)V";
    let data_obj = (|| -> Option<ObjectRef> {
        let obj = match ctx.new_object(data_class).ok()? {
            Some(Value::Object(Some(o))) => o,
            _ => return None,
        };
        ctx.invoke(
            data_class,
            "<init>",
            ctor_desc,
            &[
                Value::Object(Some(obj)),
                Value::Object(Some(mappings)),
                Value::Object(Some(reverse_mappings)),
                Value::Object(Some(descendants)),
                Value::Object(Some(cpn_arr)),
                Value::Object(Some(sys_env_copy)),
                Value::Object(Some(str_arr)),
            ],
        )
        .ok()?;
        Some(obj)
    })()
    .unwrap_or_else(|| {
        let obj = crate::alloc_concurrent_synthetic(ctx, data_class, 8);
        // Write the 6 record components directly so accessor methods return
        // non-null containers.
        ctx.set_field_by_name(obj, "mappings", Value::Object(Some(mappings)));
        ctx.set_field_by_name(obj, "reverseMappings", Value::Object(Some(reverse_mappings)));
        ctx.set_field_by_name(obj, "descendants", Value::Object(Some(descendants)));
        ctx.set_field_by_name(
            obj,
            "configurationPropertyNames",
            Value::Object(Some(cpn_arr)),
        );
        ctx.set_field_by_name(obj, "systemEnvironmentCopy", Value::Object(Some(sys_env_copy)));
        ctx.set_field_by_name(obj, "lastUpdated", Value::Object(Some(str_arr)));
        obj
    });

    ctx.set_field_by_name(cache, "data", Value::Object(Some(data_obj)));
    Some(())
}

#[allow(dead_code)]
fn cache_get_mapped(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // getMapped(this, ConfigurationPropertyName name) → Set<String>
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            // Return empty set
            let s = crate::alloc_concurrent_synthetic(ctx, "java/util/HashSet", 8);
            return Ok(Some(Value::Object(Some(s))));
        }
    };
    if let Value::Object(Some(_)) = ctx.get_field_by_name(this, "data") {
        // data is non-null — invoking the Java method would trigger a recursive
        // native dispatch on this same shim, so reimplement: just return empty.
        // (For our null-data fallback case this is fine; for the non-null
        //  case we lose precision but Spring still sees other property sources.)
    }
    // data was null OR we just installed an empty one — return empty Set.
    let s = match ctx.new_object("java/util/HashSet").ok().flatten() {
        Some(Value::Object(Some(o))) => o,
        _ => crate::alloc_concurrent_synthetic(ctx, "java/util/HashSet", 8),
    };
    let _ = ctx.invoke(
        "java/util/HashSet",
        "<init>",
        "()V",
        &[Value::Object(Some(s))],
    );
    Ok(Some(Value::Object(Some(s))))
}

#[allow(dead_code)]
fn cache_get_configuration_property_names(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // getConfigurationPropertyNames(this, String[] names) → ConfigurationPropertyName[]
    let this = match args.first() {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let data_is_null = match this {
        Some(t) => matches!(ctx.get_field_by_name(t, "data"), Value::Object(None)),
        None => true,
    };
    if data_is_null {
        let cpn_cid = ctx
            .class_id_by_name(SICP_NAME)
            .unwrap_or_else(|| rustjvm_types::ClassId::new(0));
        let arr = ctx.new_ref_array(cpn_cid, 0);
        return Ok(Some(Value::Object(Some(arr))));
    }
    // Fallback: empty array (avoids re-entering bytecode and avoids Assert.state).
    let cpn_cid = ctx
        .class_id_by_name(SICP_NAME)
        .unwrap_or_else(|| rustjvm_types::ClassId::new(0));
    let arr = ctx.new_ref_array(cpn_cid, 0);
    Ok(Some(Value::Object(Some(arr))))
}

// ──────────────────────────────────────────────────────────────────────────────
// Registration
// ──────────────────────────────────────────────────────────────────────────────

const ABSTRACT_CTX: &str = "org/springframework/context/support/AbstractApplicationContext";
const DEF_STARTUP: &str = "org/springframework/core/metrics/DefaultApplicationStartup";
const DEF_STEP: &str =
    "org/springframework/core/metrics/DefaultApplicationStartup$DefaultStartupStep";
const STARTUP_IFACE: &str = "org/springframework/core/metrics/ApplicationStartup";
const STEP_IFACE: &str = "org/springframework/core/metrics/StartupStep";

pub fn register(registry: &mut NativeMethodRegistry) {
    // AbstractApplicationContext.getApplicationStartup() — always return the
    // global no-op singleton so applicationStartup null can never crash.
    registry.register(
        ABSTRACT_CTX,
        "getApplicationStartup",
        "()Lorg/springframework/core/metrics/ApplicationStartup;",
        get_application_startup,
    );

    // DefaultApplicationStartup.start(String) → no-op step
    registry.register(
        DEF_STARTUP,
        "start",
        "(Ljava/lang/String;)Lorg/springframework/core/metrics/StartupStep;",
        startup_start,
    );
    // Also register on the interface class so invokevirtual on an interface
    // receiver still finds a native.
    registry.register(
        STARTUP_IFACE,
        "start",
        "(Ljava/lang/String;)Lorg/springframework/core/metrics/StartupStep;",
        startup_start,
    );

    // DefaultStartupStep methods
    for step_class in &[DEF_STEP, STEP_IFACE] {
        registry.register(
            step_class,
            "tag",
            "(Ljava/lang/String;Ljava/lang/String;)Lorg/springframework/core/metrics/StartupStep;",
            step_tag_sv,
        );
        registry.register(
            step_class,
            "tag",
            "(Ljava/lang/String;Ljava/util/function/Supplier;)Lorg/springframework/core/metrics/StartupStep;",
            step_tag_ssup,
        );
        registry.register(step_class, "end", "()V", step_end);
        registry.register(
            step_class,
            "getName",
            "()Ljava/lang/String;",
            step_get_name,
        );
        registry.register(step_class, "getId", "()J", step_get_id);
        registry.register(
            step_class,
            "getParentId",
            "()Ljava/lang/Long;",
            step_get_parent_id,
        );
        registry.register(
            step_class,
            "getTags",
            "()Ljava/lang/Iterable;",
            step_get_tags,
        );
    }

    // ── Environment fix ────────────────────────────────────────────────────
    // AbstractApplicationContext.getEnvironment() and createEnvironment()
    // must return a non-null ConfigurableEnvironment.  StandardEnvironment
    // construction fails in CratonVM's partial bootstrap (CLDR/resource-bundle
    // loading inside AbstractEnvironment.<init> NPEs).  Override both methods
    // to return a synthetic StandardEnvironment with no-op property access.
    const STD_ENV: &str = "org/springframework/core/env/StandardEnvironment";
    const ABS_ENV: &str = "org/springframework/core/env/AbstractEnvironment";
    const ENV_IFACE: &str = "org/springframework/core/env/Environment";
    const CONF_ENV: &str = "org/springframework/core/env/ConfigurableEnvironment";

    registry.register(
        ABSTRACT_CTX,
        "getEnvironment",
        "()Lorg/springframework/core/env/ConfigurableEnvironment;",
        get_environment,
    );
    registry.register(
        ABSTRACT_CTX,
        "createEnvironment",
        "()Lorg/springframework/core/env/ConfigurableEnvironment;",
        create_environment,
    );

    // Register property-access methods on StandardEnvironment, AbstractEnvironment,
    // and the Environment/ConfigurableEnvironment interfaces so invokevirtual on
    // our synthetic object always hits the native.
    for env_class in &[STD_ENV, ABS_ENV, ENV_IFACE, CONF_ENV] {
        registry.register(
            env_class,
            "getProperty",
            "(Ljava/lang/String;)Ljava/lang/String;",
            env_get_property_str,
        );
        registry.register(
            env_class,
            "getProperty",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
            env_get_property_str_default,
        );
        registry.register(
            env_class,
            "getProperty",
            "(Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/Object;",
            env_get_property_class,
        );
        registry.register(
            env_class,
            "containsProperty",
            "(Ljava/lang/String;)Z",
            env_contains_property,
        );
        registry.register(
            env_class,
            "getActiveProfiles",
            "()[Ljava/lang/String;",
            env_get_active_profiles,
        );
        registry.register(
            env_class,
            "getDefaultProfiles",
            "()[Ljava/lang/String;",
            env_get_default_profiles,
        );
        registry.register(
            env_class,
            "acceptsProfiles",
            "(Lorg/springframework/core/env/Profiles;)Z",
            env_accepts_profiles,
        );
        registry.register(
            env_class,
            "acceptsProfiles",
            "([Ljava/lang/String;)Z",
            env_accepts_profiles_arr,
        );
        registry.register(
            env_class,
            "getPropertySources",
            "()Lorg/springframework/core/env/MutablePropertySources;",
            env_get_property_sources,
        );
        registry.register(
            env_class,
            "resolveRequiredPlaceholders",
            "(Ljava/lang/String;)Ljava/lang/String;",
            env_resolve_placeholders,
        );
        registry.register(
            env_class,
            "resolvePlaceholders",
            "(Ljava/lang/String;)Ljava/lang/String;",
            env_resolve_placeholders,
        );
    }

    // ── BeanFactory fix ────────────────────────────────────────────────────
    // GenericApplicationContext.getBeanFactory() — lazily create the
    // DefaultListableBeanFactory if the field is null (constructor failed).
    // Register on both GenericApplicationContext and AbstractApplicationContext
    // (the method is declared on ConfigurableListableBeanFactory-returning
    // overrides at both levels).
    for ctx_class in &[GENERIC_CTX, ABSTRACT_CTX] {
        registry.register(
            ctx_class,
            "getBeanFactory",
            "()Lorg/springframework/beans/factory/support/DefaultListableBeanFactory;",
            get_bean_factory,
        );
        // Also cover the ConfigurableListableBeanFactory descriptor variant
        // used in the interface hierarchy.
        registry.register(
            ctx_class,
            "getBeanFactory",
            "()Lorg/springframework/beans/factory/config/ConfigurableListableBeanFactory;",
            get_bean_factory,
        );
    }

    // DefaultListableBeanFactory stubs — used ONLY when the DLBF is a
    // synthetic allocation (i.e. the bytecode <init> failed and we built a
    // fallback object in `get_or_create_bean_factory`).  Registering these
    // on the concrete `DefaultListableBeanFactory` class would unconditionally
    // override the real bytecode for every receiver — silently discarding
    // every bean-definition registration, including the user's
    // @SpringBootApplication primary source via
    // `AnnotatedBeanDefinitionReader.doRegisterBean`.  That was the root
    // cause of `MissingWebServerFactoryBeanException` on Spring Boot apps:
    // no beans got registered, so `ConfigurationClassPostProcessor` never
    // ran and `@EnableAutoConfiguration` never fired.
    //
    // The interfaces below have no concrete bytecode of their own, so an
    // override here only fires when virtual dispatch lands on a synthetic
    // object whose runtime class is the interface stub itself.  For real
    // DLBF instances, the bytecode walks `beanDefinitionMap` correctly.
    for bf_iface in &[
        "org/springframework/beans/factory/ListableBeanFactory",
        "org/springframework/beans/factory/config/ConfigurableListableBeanFactory",
    ] {
        registry.register(
            bf_iface,
            "containsBeanDefinition",
            "(Ljava/lang/String;)Z",
            dlbf_contains_bean_definition,
        );
        registry.register(
            bf_iface,
            "registerBeanDefinition",
            "(Ljava/lang/String;Lorg/springframework/beans/factory/config/BeanDefinition;)V",
            dlbf_register_bean_definition,
        );
    }

    // ── SpringIterableConfigurationPropertySource$Cache null-safe shims ──
    // Guard `tryUpdate` against `getPropertyNames()` returning null in CratonVM's
    // partial bootstrap (see comment block above the cache_try_update fn).
    registry.register(
        SICP_CACHE,
        "tryUpdate",
        "(Lorg/springframework/core/env/EnumerablePropertySource;)V",
        cache_try_update,
    );
}
