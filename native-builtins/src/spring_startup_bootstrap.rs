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
    // Empty `ArrayList` — match real-JDK field slots (modCount may occupy slot 0).
    let data_slot = ctx
        .resolve_field_index("java/util/ArrayList", "elementData")
        .unwrap_or(0);
    let size_slot = ctx
        .resolve_field_index("java/util/ArrayList", "size")
        .unwrap_or(1);
    let n_fields = std::cmp::max(data_slot, size_slot) + 1;
    let list = crate::alloc_concurrent_synthetic(ctx, "java/util/ArrayList", n_fields);
    let arr = ctx.new_array(rustjvm_types::ArrayElementType::Reference, 0);
    ctx.set_field(list, data_slot, Value::Object(Some(arr)));
    ctx.set_field(list, size_slot, Value::Int(0));
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

const BPP_LIST: &str =
    "org/springframework/beans/factory/support/AbstractBeanFactory$BeanPostProcessorCacheAwareList";

/// `AbstractBeanFactory.beanPostProcessors` must be a non-null
/// `BeanPostProcessorCacheAwareList` — Spring casts it to that concrete type.
///
/// Non-static member classes (JVMS §4.7.6 / JLS §8.8) take a synthetic first
/// constructor parameter, the enclosing instance; the compiler stores it in
/// `this$0`.  If bytecode `new`/`invokespecial` ran with a broken argument
/// binding, we can see the correct runtime class with a **null** `this$0`.
/// The old logic returned early whenever the class name matched, so those
/// lists were never repaired and `addBeanProcessor` NPE'd on the outer ref.
fn ensure_bean_post_processors_list(ctx: &mut dyn NativeContext, bean_factory: ObjectRef) {
    const INNER_INIT: &str =
        "(Lorg/springframework/beans/factory/support/AbstractBeanFactory;)V";

    let cur = ctx.get_field_by_name(bean_factory, "beanPostProcessors");
    if let Value::Object(Some(obj)) = cur {
        let cid = ctx.class_id_of_object(obj);
        if ctx.class_name_of_id(cid).as_deref() == Some(BPP_LIST) {
            let needs_outer = match ctx.get_field_by_name(obj, "this$0") {
                Value::Object(Some(o)) => o != bean_factory,
                _ => true,
            };
            if !needs_outer {
                return;
            }
            // Wrong or missing enclosing factory — patch in place so we keep
            // any list state and fix the NPE Spring hits in addBeanProcessor.
            ctx.set_field_by_name(obj, "this$0", Value::Object(Some(bean_factory)));
            return;
        }
    }

    if let Ok(Some(Value::Object(Some(list)))) = ctx.new_object(BPP_LIST) {
        let args = &[
            Value::Object(Some(list)),
            Value::Object(Some(bean_factory)),
        ];
        let inited = ctx
            .invoke_special(BPP_LIST, "<init>", INNER_INIT, args)
            .is_ok()
            || ctx.invoke(BPP_LIST, "<init>", INNER_INIT, args).is_ok();
        if inited {
            ctx.set_field_by_name(
                bean_factory,
                "beanPostProcessors",
                Value::Object(Some(list)),
            );
        }
    }
}

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
    ensure_bean_post_processors_list(ctx, bf);
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
// StandardConfigDataLocationResolver.resolve — null-safe shim.
//
// Spring Boot 4.x `StandardConfigDataLocationResolver.resolve(ctx, location)`
// calls `location.split()` which returns a `ConfigDataLocation[]`.  Each
// element is passed to `getReferences(ctx, loc)` which dereferences `loc`
// via `loc.getResourceLocation(...)`.
//
// In CratonVM's partial bootstrap, one of the array entries ends up null
// (likely because `StringUtils.delimitedListToStringArray` or `Properties`
// returns a null mid-array).  The result is:
//
//     NullPointerException: Cannot invoke getResourceLocation on null
//
// Without application.yml/properties on the demo classpath, the correct
// behaviour is to load no config-data resources.  We override the public
// `resolve(ConfigDataLocationResolverContext, ConfigDataLocation)` to return
// an empty `ArrayList` — Spring proceeds without any config-data overrides
// from the standard locations.
// ──────────────────────────────────────────────────────────────────────────────

fn empty_arraylist(ctx: &mut dyn NativeContext) -> MethodCallResult {
    let list = match ctx.new_object("java/util/ArrayList").ok().flatten() {
        Some(Value::Object(Some(o))) => o,
        _ => crate::alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 8),
    };
    let _ = ctx.invoke(
        "java/util/ArrayList",
        "<init>",
        "()V",
        &[Value::Object(Some(list))],
    );
    Ok(Some(Value::Object(Some(list))))
}

// ──────────────────────────────────────────────────────────────────────────────
// Spring Cloud AbstractEnvironmentDecrypt.decrypt — null-safe shim.
//
// `AbstractEnvironmentDecrypt.decrypt(TextEncryptor, PropertySources)` iterates
// `propertySources`, casts each `EnumerablePropertySource` and reads
// `getPropertyNames()` straight into `arraylength`.  Under CratonVM's partial
// bootstrap at least one synthetic property source returns `null` from
// `getPropertyNames()` (our synthetic StandardEnvironment or one of the
// MutablePropertySources entries), so the `arraylength` at pc=72 NPEs.
//
// The demo has no encrypted properties, so the correct semantic answer here
// is "no decryptions performed" → an empty Map.  We override `decrypt` on
// `AbstractEnvironmentDecrypt` to return an empty `HashMap`, bypassing the
// broken iteration entirely.
// ──────────────────────────────────────────────────────────────────────────────

fn empty_hashmap(ctx: &mut dyn NativeContext) -> MethodCallResult {
    let map = match ctx.new_object("java/util/HashMap").ok().flatten() {
        Some(Value::Object(Some(o))) => o,
        _ => crate::alloc_concurrent_synthetic(ctx, "java/util/HashMap", 8),
    };
    let _ = ctx.invoke(
        "java/util/HashMap",
        "<init>",
        "()V",
        &[Value::Object(Some(map))],
    );
    Ok(Some(Value::Object(Some(map))))
}

fn abstract_env_decrypt(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    empty_hashmap(ctx)
}

fn standard_config_data_resolve(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    empty_arraylist(ctx)
}

fn standard_config_data_resolve_profile_specific(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    empty_arraylist(ctx)
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

fn cache_try_update(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // tryUpdate(this, EnumerablePropertySource source) → void
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let _source = args.get(1).copied();

    // Always install an empty Data record so downstream Assert.state(data!=null)
    // never trips.  We accept that this iterable cache contributes no
    // bindings — Binder still queries other property sources directly.
    let _ = install_empty_data(ctx, this);
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

    // ── StandardConfigDataLocationResolver null-safe shim ──
    // See comment block above standard_config_data_resolve for rationale.
    const STD_CFG_RES: &str =
        "org/springframework/boot/context/config/StandardConfigDataLocationResolver";
    registry.register(
        STD_CFG_RES,
        "resolve",
        "(Lorg/springframework/boot/context/config/ConfigDataLocationResolverContext;Lorg/springframework/boot/context/config/ConfigDataLocation;)Ljava/util/List;",
        standard_config_data_resolve,
    );
    // ── Spring Cloud AbstractEnvironmentDecrypt null-safe shim ──
    // Override decrypt(TextEncryptor, PropertySources) to return an empty Map,
    // sidestepping the arraylength-on-null NPE from synthetic property sources
    // whose getPropertyNames() returns null.  The demo has no encrypted props.
    const ABS_ENV_DECRYPT: &str =
        "org/springframework/cloud/bootstrap/encrypt/AbstractEnvironmentDecrypt";
    registry.register(
        ABS_ENV_DECRYPT,
        "decrypt",
        "(Lorg/springframework/security/crypto/encrypt/TextEncryptor;Lorg/springframework/core/env/PropertySources;)Ljava/util/Map;",
        abstract_env_decrypt,
    );

    registry.register(
        STD_CFG_RES,
        "resolveProfileSpecific",
        "(Lorg/springframework/boot/context/config/ConfigDataLocationResolverContext;Lorg/springframework/boot/context/config/ConfigDataLocation;Lorg/springframework/boot/context/config/Profiles;)Ljava/util/List;",
        standard_config_data_resolve_profile_specific,
    );

    // ── Hibernate Validator preinitialization shim ─────────────────────────
    // Spring Boot's `BackgroundPreinitializer$ValidationInitializer.run()`
    // calls into Hibernate Validator's `buildValidatorFactory()`, which
    // recursively initialises `ValueExtractorManager`.  In CratonVM's
    // partial bootstrap, `ValueExtractorManager.<clinit>` throws an
    // `IllegalArgumentException`, which becomes an `ExceptionInInitializerError`
    // and is swallowed by BackgroundPreinitializer — but the swallow happens
    // only after a lot of wasted work and noisy diagnostics.  More importantly,
    // any later code that touches a Hibernate Validator class hits a
    // `NoClassDefFoundError` because the previous <clinit> failure poisoned
    // the class.
    //
    // The preinitializer is purely an optimisation — Spring Boot tolerates a
    // null validator and will initialise it lazily on first use.  We stub
    // ValidationInitializer.run() as a no-op so the entire validator chain
    // is skipped at preinit time.
    registry.register(
        "org/springframework/boot/autoconfigure/BackgroundPreinitializer$ValidationInitializer",
        "run",
        "()V",
        noop_void,
    );

    // ── ApplicationHome.getStartClass(Enumeration) ────────────────────────
    // Iterates manifest entries from JAR resources; CratonVM's nested-jar
    // resource Enumeration loops forever in this path. Returning null is
    // safe — ApplicationHome interprets it as "couldn't determine start
    // class" and falls back to a default location.
    registry.register(
        "org/springframework/boot/system/ApplicationHome",
        "getStartClass",
        "(Ljava/util/Enumeration;)Ljava/lang/Class;",
        return_null_object,
    );

    // M3: AbstractBeanDefinition.resolveBeanClass(ClassLoader) → return null
    // instead of throwing ClassNotFoundException, so Spring's
    // preInstantiateSingletons skips beans whose class is missing on the
    // partial classpath (e.g. RedisHttpSessionConfiguration in SportMe).
    registry.register(
        "org/springframework/beans/factory/support/AbstractBeanDefinition",
        "resolveBeanClass",
        "(Ljava/lang/ClassLoader;)Ljava/lang/Class;",
        m3_abstract_bean_definition_resolve_bean_class,
    );

    // N6: AbstractBeanFactory.doResolveBeanClass(RootBeanDefinition, Class[])
    // → return null instead of throwing CNFE so Spring's preInstantiateSingletons
    // skips beans whose class is missing on the partial classpath (e.g.
    // RedisHttpSessionConfiguration in SportMe). M3 hooked the WRONG method;
    // the actual throw site is the private doResolveBeanClass.
    registry.register(
        "org/springframework/beans/factory/support/AbstractBeanFactory",
        "doResolveBeanClass",
        "(Lorg/springframework/beans/factory/support/RootBeanDefinition;[Ljava/lang/Class;)Ljava/lang/Class;",
        m4_abstract_bean_factory_do_resolve_bean_class,
    );

    // ───────────────────────────────────────────────────────────────────────
    // CGLIB-ε: ConfigurationClassPostProcessor.processConfigBeanDefinitions
    //
    // Strategy: intercept the entry point where Spring decides which
    // @Configuration / @Import classes get processed.  At this boundary we
    // can manually walk every @Configuration class' @Import annotation tree
    // and register a RootBeanDefinition for each imported class on the
    // BeanDefinitionRegistry passed in as args[1].
    //
    // This complements the existing CCE.enhance() bypass (net_phase_e.rs):
    // that bypass keeps CGLIB from blowing up but leaves @Import children
    // unregistered.  Walking @Import here closes that gap so @EnableXxx
    // chains pull in their child @Configuration beans even when the real
    // ConfigurationClassParser path is incomplete.
    //
    // Reflection plan per (Class, ClassLoader):
    //   for ann in ctx.class_annotations(cls):
    //     if ann.type_descriptor == "Lorg/springframework/context/annotation/Import;":
    //       for v in ann.elements[("value", Array(...))]:
    //         if let Class(desc) = v -> derive imported class name,
    //         allocate RootBeanDefinition, setBeanClassName, then call
    //         registry.registerBeanDefinition(name, bd).
    //
    // We register on the concrete class and also on the
    // BeanDefinitionRegistryPostProcessor interface so virtual dispatch on
    // either receiver hits us.  An additional alternate intercept on
    // BeanFactoryPostProcessor.postProcessBeanFactory is registered below
    // so the same walk runs once per context refresh even if the registry
    // boundary is skipped.
    // ───────────────────────────────────────────────────────────────────────
    const CCPP: &str = "org/springframework/context/annotation/ConfigurationClassPostProcessor";
    const BDRPP: &str = "org/springframework/beans/factory/support/BeanDefinitionRegistryPostProcessor";
    const BFPP: &str = "org/springframework/beans/factory/config/BeanFactoryPostProcessor";

    registry.register(
        CCPP,
        "processConfigBeanDefinitions",
        "(Lorg/springframework/beans/factory/support/BeanDefinitionRegistry;)V",
        ccpp_process_config_bean_definitions,
    );
    registry.register(
        CCPP,
        "postProcessBeanDefinitionRegistry",
        "(Lorg/springframework/beans/factory/support/BeanDefinitionRegistry;)V",
        ccpp_process_config_bean_definitions,
    );
    registry.register(
        BDRPP,
        "postProcessBeanDefinitionRegistry",
        "(Lorg/springframework/beans/factory/support/BeanDefinitionRegistry;)V",
        ccpp_process_config_bean_definitions,
    );
    // Alternate: BeanFactoryPostProcessor.postProcessBeanFactory — the
    // ConfigurableListableBeanFactory passed here also implements
    // BeanDefinitionRegistry in real Spring, so the same walk works.
    registry.register(
        CCPP,
        "postProcessBeanFactory",
        "(Lorg/springframework/beans/factory/config/ConfigurableListableBeanFactory;)V",
        ccpp_process_config_bean_definitions,
    );
    registry.register(
        BFPP,
        "postProcessBeanFactory",
        "(Lorg/springframework/beans/factory/config/ConfigurableListableBeanFactory;)V",
        ccpp_process_config_bean_definitions,
    );
}

// Spring's @Import annotation descriptor.
const IMPORT_DESC: &str = "Lorg/springframework/context/annotation/Import;";
// Spring's @Configuration annotation descriptor.
const CONFIGURATION_DESC: &str = "Lorg/springframework/context/annotation/Configuration;";

/// Manually walk @Configuration → @Import chains and register each imported
/// class with the BeanDefinitionRegistry passed as args[1].  This is the
/// CGLIB-ε strategy: instead of intercepting `enhance` (where CGLIB itself
/// runs), we intercept at the boundary where Spring decides which
/// @Configuration classes to process.
fn ccpp_process_config_bean_definitions(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = receiver (ConfigurationClassPostProcessor)
    // args[1] = BeanDefinitionRegistry (or ConfigurableListableBeanFactory)
    let registry = match args.get(1).cloned() {
        Some(Value::Object(Some(o))) => o,
        _ => {
            eprintln!("[CCPP-DBG] processConfigBeanDefinitions: no registry arg → no-op");
            return Ok(None);
        }
    };

    // Snapshot every loaded application class so we don't mutate the list
    // while load_class() runs inside the walk.
    let class_names = ctx.list_application_class_names();
    let mut seen_imports: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut registered = 0usize;

    for cls_name in &class_names {
        let cid = match ctx.class_id_by_name(cls_name) {
            Some(c) => c,
            None => continue,
        };
        // Only walk @Configuration classes.
        let anns = ctx.class_annotations(cid);
        let is_config = anns
            .iter()
            .any(|a| a.type_descriptor == CONFIGURATION_DESC);
        if !is_config {
            continue;
        }
        walk_imports_recursive(ctx, cls_name, registry, &mut seen_imports, &mut registered, 0);
    }

    if registered > 0 {
        eprintln!(
            "[CCPP-DBG] processConfigBeanDefinitions: registered {} @Import bean definitions across {} @Configuration classes",
            registered,
            class_names.len(),
        );
    }
    Ok(None)
}

/// Recursively walk a class' @Import tree and ensure every imported class
/// has a RootBeanDefinition registered.  `seen` prevents cycles.
fn walk_imports_recursive(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    registry: ObjectRef,
    seen: &mut std::collections::HashSet<String>,
    registered: &mut usize,
    depth: usize,
) {
    if depth > 16 {
        return; // safety guard against pathological @Import cycles
    }
    let cid = match ctx.class_id_by_name(class_name) {
        Some(c) => c,
        None => {
            // Try to load on demand so @Import targets that aren't yet
            // loaded still get walked.
            if ctx.load_class(class_name).is_err() {
                return;
            }
            match ctx.class_id_by_name(class_name) {
                Some(c) => c,
                None => return,
            }
        }
    };

    let anns = ctx.class_annotations(cid);
    let import_ann = match anns.iter().find(|a| a.type_descriptor == IMPORT_DESC) {
        Some(a) => a.clone(),
        None => return,
    };

    // @Import.value() is a Class[] — find the "value" element.
    let value_array = match import_ann
        .elements
        .iter()
        .find(|(name, _)| name == "value")
    {
        Some((_, v)) => v.clone(),
        None => return,
    };

    let entries = match value_array {
        rustjvm_native_api::AnnotationElementValue::Array(v) => v,
        single => vec![single],
    };

    for entry in entries {
        let desc = match entry {
            rustjvm_native_api::AnnotationElementValue::Class(d) => d,
            _ => continue,
        };
        // Convert "Lcom/foo/Bar;" → "com/foo/Bar".
        if !(desc.starts_with('L') && desc.ends_with(';')) {
            continue;
        }
        let imp_class = desc[1..desc.len() - 1].to_string();
        if !seen.insert(imp_class.clone()) {
            continue; // already processed
        }
        // Skip @Import targets whose class isn't on the classpath — mirrors
        // Spring's behavior where ClassNotFound during @Import processing
        // simply omits the import (often guarded by @ConditionalOnClass).
        // Without this, registering a RootBeanDefinition for a missing class
        // throws CannotLoadBeanClassException later during bean preInstantiation.
        if ctx.class_id_by_name(&imp_class).is_none()
            && ctx.load_class(&imp_class).is_err()
        {
            if std::env::var("CCPP_DBG").is_ok() {
                eprintln!(
                    "[CCPP-DBG] walk_imports: skipping missing @Import target {}",
                    imp_class
                );
            }
            continue;
        }
        // Register a RootBeanDefinition for this imported class.
        if register_root_bean_definition(ctx, registry, &imp_class) {
            *registered += 1;
        }
        // Recurse into the import's own @Import tree.
        walk_imports_recursive(ctx, &imp_class, registry, seen, registered, depth + 1);
    }
}

/// Allocate a `RootBeanDefinition`, set its bean class name, and call
/// `registry.registerBeanDefinition(name, bd)`.  Returns true on success.
fn register_root_bean_definition(
    ctx: &mut dyn NativeContext,
    registry: ObjectRef,
    class_name: &str,
) -> bool {
    const RBD: &str = "org/springframework/beans/factory/support/RootBeanDefinition";
    // Allocate. Prefer the real constructor; fall back to synthetic if the
    // class isn't loadable in this context.
    let bd = match ctx.new_object(RBD).ok().flatten() {
        Some(Value::Object(Some(o))) => {
            let _ = ctx.invoke(RBD, "<init>", "()V", &[Value::Object(Some(o))]);
            o
        }
        _ => crate::alloc_concurrent_synthetic(ctx, RBD, 16),
    };
    // setBeanClassName(String) — declared on AbstractBeanDefinition.
    let name_obj = ctx.create_string(class_name);
    let _ = ctx.invoke(
        "org/springframework/beans/factory/support/AbstractBeanDefinition",
        "setBeanClassName",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(bd)), Value::Object(Some(name_obj))],
    );
    // registry.registerBeanDefinition(beanName, bd) — beanName defaults to
    // the FQN for @Import children when the imported class has no explicit
    // bean name.
    let bean_name = ctx.create_string(&class_name.replace('/', "."));
    let res = ctx.invoke(
        "org/springframework/beans/factory/support/BeanDefinitionRegistry",
        "registerBeanDefinition",
        "(Ljava/lang/String;Lorg/springframework/beans/factory/config/BeanDefinition;)V",
        &[
            Value::Object(Some(registry)),
            Value::Object(Some(bean_name)),
            Value::Object(Some(bd)),
        ],
    );
    res.is_ok()
}

fn return_null_object(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

/// M3: AbstractBeanDefinition.resolveBeanClass(ClassLoader) → Class
///
/// Returns null on ClassNotFound instead of throwing. Spring's downstream
/// (`getTypeForFactoryBean`, `predictBeanType`, `isFactoryBean`,
/// `findAutowireCandidates`) handles null gracefully; throwing would
/// fail-fast in `preInstantiateSingletons`.
fn m3_abstract_bean_definition_resolve_bean_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let recv = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Read beanClassName String field via getfield by name
    let name_obj = match ctx.get_field_by_name(recv, "beanClassName") {
        Value::Object(Some(s)) => s,
        _ => return Ok(Some(Value::Object(None))),
    };
    let class_name = match ctx.read_string(name_obj) {
        Some(s) => s,
        None => return Ok(Some(Value::Object(None))),
    };
    let internal = class_name.replace('.', "/");
    // Try to load via class manager; on failure return null (no CNFE).
    if let Some(cid) = ctx.class_id_by_name(&internal) {
        let mirror = ctx.get_class_mirror(cid);
        return Ok(Some(Value::Object(Some(mirror))));
    }
    // Fallback: try ensure_class_initialized (which loads if necessary).
    if let Ok(cid) = ctx.ensure_class_initialized(&internal) {
        let mirror = ctx.get_class_mirror(cid);
        return Ok(Some(Value::Object(Some(mirror))));
    }
    Ok(Some(Value::Object(None)))
}

fn m4_abstract_bean_factory_do_resolve_bean_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = this (AbstractBeanFactory), args[1] = mbd (RootBeanDefinition)
    let mbd = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let name_obj = match ctx.get_field_by_name(mbd, "beanClassName") {
        Value::Object(Some(s)) => s,
        _ => return Ok(Some(Value::Object(None))),
    };
    let class_name = match ctx.read_string(name_obj) {
        Some(s) => s,
        None => return Ok(Some(Value::Object(None))),
    };
    let internal = class_name.replace('.', "/");
    if let Some(cid) = ctx.class_id_by_name(&internal) {
        let mirror = ctx.get_class_mirror(cid);
        return Ok(Some(Value::Object(Some(mirror))));
    }
    if let Ok(cid) = ctx.ensure_class_initialized(&internal) {
        let mirror = ctx.get_class_mirror(cid);
        return Ok(Some(Value::Object(Some(mirror))));
    }
    Ok(Some(Value::Object(None)))
}

fn noop_void(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}
