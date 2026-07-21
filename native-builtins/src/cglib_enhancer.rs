// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! CGLIB `Enhancer.createClass` / Spring `ConfigurationClassEnhancer.enhance`
//! native intercept.
//!
//! Spring Boot relies on CGLIB to subclass every `@Configuration`-annotated
//! class so calls between `@Bean` methods return shared bean instances rather
//! than fresh ones. CGLIB's `Enhancer.createClass()` exercises a large
//! bytecode-generation pipeline that is incomplete in this VM. The previous
//! pragmatic workaround (in `net_phase_e.rs`) simply returned the original
//! class unchanged, which breaks Spring code paths that rely on the post-
//! enhancement class being DIFFERENT from the original (`if_acmpeq` shortcut
//! at `ConfigurationClassPostProcessor.enhanceConfigurationClasses` pc 470,
//! `@Bean` method dispatch, and the `@Import` chain walker).
//!
//! This module ships a **minimum** bytecode emitter for the marker subclass.
//! For each `@Configuration` Class<?> passed in we:
//!
//!   1. Synthesise a class file whose `this_class` is
//!      `<OriginalName>$$SpringCGLIB$$<counter>` (Springs own naming
//!      policy tag, not cglibs default EnhancerByCGLIB -- see
//!      `build_enhancer_class`s doc comment).
//!   2. Set `super_class` to the original `@Configuration` class so the new
//!      class is a real subclass (`isAssignableFrom` works, `cast` works).
//!   3. List `org/springframework/context/annotation/ConfigurationClassEnhancer$EnhancedConfiguration`
//!      in the `interfaces` table so Spring's marker check passes.
//!   4. Emit a single no-arg `<init>()V` that calls `super.<init>()` — this is
//!      all that Spring's `enhance()` immediately needs in order to allocate
//!      the new class (it later sets the `BEAN_FACTORY_FIELD` reflectively).
//!   5. Feed the bytes through `define_class_full`, the same backend used by
//!      `Unsafe.defineClass` / `Lookup.defineClass` / `ClassLoader.defineClass1`.
//!
//! `@Bean`-method interception (shared-singleton semantics):
//!   * We add a `$$beanFactory` field and a `setBeanFactory` body that stores
//!     the factory into it (the `BeanFactoryAware` callback Spring already
//!     invokes on the marker interface).
//!   * For each instance, no-arg, reference-returning `@Bean` method we emit an
//!     override that mirrors Spring's `BeanMethodInterceptor`: if the call is
//!     the factory creating this bean (`SimpleInstantiationStrategy
//!     .getCurrentlyInvokedFactoryMethod()` names this method) it runs the real
//!     body via `super.<name>()`; otherwise (an inter-`@Bean`-method reference)
//!     it returns the shared instance via `beanFactory.getBean("<name>")`. Using
//!     the factory-method thread-local (not bean-creation state) is essential for
//!     scoped / lazy-init proxies, whose target is created under a *different*
//!     bean name (`scopedTarget.<name>`) — see `emit_bean_override`.
//!   * `@Bean` methods with parameters, primitive/void/array returns, or
//!     static/final/private modifiers are left un-overridden (they run the real
//!     body — the `proxyBeanMethods=false` trade-off), never regressing relative
//!     to the previous no-override enhancer.
//!
//! The intercept is registered AFTER `net_phase_e::register_phase_e_networking`
//! so it overrides the older bypass registration at the registry layer (last
//! writer wins — `native-api/src/registry.rs:1336-1338`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use cratonvm_native_api::{DefineClassFull, NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallResult, RuntimeError};
use cratonvm_types::Value;

/// Monotonic counter for generating unique enhancer subclass names. Mirrors
/// CGLIB's own `KeyFactory.generateName` counter.
static ENHANCER_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Per-superclass-name suffix counters for `build_enhancer_class`'s
/// `$$SpringCGLIB$$<n>` names -- Springs own `SpringNamingPolicy` keys
/// its counter by the base class name, so the first proxy generated for any
/// given `@Configuration` class always gets suffix `0`, no matter how many
/// *other* classes were enhanced earlier in the same JVM/session. Using the
/// single global `ENHANCER_COUNTER` here (shared with the unrelated
/// `$$SpringCGLIB$$LM*`/`$$SpringCGLIB$$RM*` lookup/replace-method enhancers)
/// made every class's suffix depend on unrelated enhancement activity
/// elsewhere in the same test run, breaking
/// `AnnotationConfigApplicationContextTests.refreshForAotRegisterHintsForCglibProxy`,
/// which hardcodes the literal expected name `...$$SpringCGLIB$$0` for the
/// first (and only) proxy of its `CglibConfiguration` class.
fn config_enhancer_counters() -> &'static Mutex<HashMap<String, u64>> {
    static COUNTERS: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    COUNTERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Next `$$SpringCGLIB$$<n>` suffix for `super_internal_name`, starting
/// at 0 for the first enhancement of any given class.
fn next_config_enhancer_counter(super_internal_name: &str) -> u64 {
    let mut counters = config_enhancer_counters()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let counter = counters.entry(super_internal_name.to_string()).or_insert(0);
    let value = *counter;
    *counter += 1;
    value
}

/// Cache of already-enhanced `@Configuration` classes, keyed by the ORIGINAL
/// (unenhanced) class's `ClassId`. Real CGLIB's `AbstractClassGenerator`
/// caches generated proxy classes per (superclass, callback-filter,
/// classloader) key and returns the SAME `Class` object on a repeat
/// `Enhancer.createClass()` for an identical configuration, rather than
/// generating a fresh numbered subclass every time. `cce_enhance` used to
/// regenerate + `defineClass` a brand-new `$$SpringCGLIB$$<n>` subclass
/// on every call — harmless for ordinary bean creation (each instance is
/// independent regardless of which identical-shape class it's an instance
/// of) but wrong whenever code depends on repeated `enhance()` calls for the
/// SAME `@Configuration` class returning the IDENTICAL `Class` object, e.g.
/// `AnnotationConfigApplicationContextTests.refreshForAotRegisterHintsForCglibProxy`
/// hardcodes the literal name `...$$SpringCGLIB$$0`, which only holds if
/// `CglibConfiguration` is enhanced exactly once per JVM session — other
/// test methods in the same class also register `CglibConfiguration` and
/// enhance it independently, bumping the counter before this test runs.
type CachedEnhancerClass = (cratonvm_types::ClassId, String, std::sync::Arc<Vec<u8>>);

fn config_enhancer_class_cache() -> &'static Mutex<HashMap<u32, CachedEnhancerClass>> {
    static CACHE: OnceLock<Mutex<HashMap<u32, CachedEnhancerClass>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Spring's marker interface for an enhanced `@Configuration` class.
const SPRING_MARKER_IFACE: &str =
    "org/springframework/context/annotation/ConfigurationClassEnhancer$EnhancedConfiguration";

// ---------------------------------------------------------------------------
// Class-file emitter primitives
// ---------------------------------------------------------------------------

/// Builds a JVMS class file in memory by appending constant pool entries
/// and method/attribute bytes. Indices returned by the `add_*` helpers
/// refer to the 1-based constant pool layout the class file format uses.
struct ClassWriter {
    /// Raw constant pool entries (each is its own tag-prefixed byte sequence).
    /// `cp[0]` is the JVMS "unused" sentinel — we pad it on construction.
    cp: Vec<Vec<u8>>,
}

impl ClassWriter {
    fn new() -> Self {
        // JVMS §4.4.1: constant_pool[0] is unused; valid indices start at 1.
        ClassWriter {
            cp: vec![Vec::new()],
        }
    }

    /// 1-based index of the next entry to be inserted.
    fn next_index(&self) -> u16 {
        self.cp.len() as u16
    }

    fn add_utf8(&mut self, s: &str) -> u16 {
        // CONSTANT_Utf8 (1): tag(1), length(u16), bytes
        let bytes = s.as_bytes();
        let mut entry = Vec::with_capacity(3 + bytes.len());
        entry.push(1u8);
        entry.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
        entry.extend_from_slice(bytes);
        let idx = self.next_index();
        self.cp.push(entry);
        idx
    }

    fn add_class(&mut self, internal_name: &str) -> u16 {
        // CONSTANT_Class (7): tag(1), name_index(u16)
        let name_idx = self.add_utf8(internal_name);
        let mut entry = Vec::with_capacity(3);
        entry.push(7u8);
        entry.extend_from_slice(&name_idx.to_be_bytes());
        let idx = self.next_index();
        self.cp.push(entry);
        idx
    }

    fn add_name_and_type(&mut self, name: &str, descriptor: &str) -> u16 {
        // CONSTANT_NameAndType (12): tag(1), name_index(u16), descriptor_index(u16)
        let name_idx = self.add_utf8(name);
        let desc_idx = self.add_utf8(descriptor);
        let mut entry = Vec::with_capacity(5);
        entry.push(12u8);
        entry.extend_from_slice(&name_idx.to_be_bytes());
        entry.extend_from_slice(&desc_idx.to_be_bytes());
        let idx = self.next_index();
        self.cp.push(entry);
        idx
    }

    fn add_methodref(&mut self, class_idx: u16, name: &str, descriptor: &str) -> u16 {
        // CONSTANT_Methodref (10): tag(1), class_index(u16), name_and_type_index(u16)
        let nt_idx = self.add_name_and_type(name, descriptor);
        let mut entry = Vec::with_capacity(5);
        entry.push(10u8);
        entry.extend_from_slice(&class_idx.to_be_bytes());
        entry.extend_from_slice(&nt_idx.to_be_bytes());
        let idx = self.next_index();
        self.cp.push(entry);
        idx
    }

    fn add_interface_methodref(&mut self, class_idx: u16, name: &str, descriptor: &str) -> u16 {
        // CONSTANT_InterfaceMethodref (11): tag(1), class_index, name_and_type_index
        let nt_idx = self.add_name_and_type(name, descriptor);
        let mut entry = Vec::with_capacity(5);
        entry.push(11u8);
        entry.extend_from_slice(&class_idx.to_be_bytes());
        entry.extend_from_slice(&nt_idx.to_be_bytes());
        let idx = self.next_index();
        self.cp.push(entry);
        idx
    }

    fn add_fieldref(&mut self, class_idx: u16, name: &str, descriptor: &str) -> u16 {
        // CONSTANT_Fieldref (9): tag(1), class_index, name_and_type_index
        let nt_idx = self.add_name_and_type(name, descriptor);
        let mut entry = Vec::with_capacity(5);
        entry.push(9u8);
        entry.extend_from_slice(&class_idx.to_be_bytes());
        entry.extend_from_slice(&nt_idx.to_be_bytes());
        let idx = self.next_index();
        self.cp.push(entry);
        idx
    }

    fn add_string(&mut self, s: &str) -> u16 {
        // CONSTANT_String (8): tag(1), string_index(u16) → Utf8
        let utf8_idx = self.add_utf8(s);
        let mut entry = Vec::with_capacity(3);
        entry.push(8u8);
        entry.extend_from_slice(&utf8_idx.to_be_bytes());
        let idx = self.next_index();
        self.cp.push(entry);
        idx
    }

    /// Assemble the final class file bytes.
    fn finish(
        &self,
        access_flags: u16,
        this_class_idx: u16,
        super_class_idx: u16,
        interfaces: &[u16],
        fields: &[Vec<u8>],
        methods: &[Vec<u8>],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        // magic + minor + major (class file version 52.0 = JDK 8 — universally
        // accepted by the loader; matches CGLIB's emit default).
        out.extend_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE]);
        out.extend_from_slice(&0u16.to_be_bytes()); // minor
        out.extend_from_slice(&52u16.to_be_bytes()); // major

        // constant_pool_count = real entries + 1 (the JVMS off-by-one)
        out.extend_from_slice(&(self.cp.len() as u16).to_be_bytes());
        for (i, entry) in self.cp.iter().enumerate() {
            if i == 0 {
                continue;
            }
            out.extend_from_slice(entry);
        }

        out.extend_from_slice(&access_flags.to_be_bytes());
        out.extend_from_slice(&this_class_idx.to_be_bytes());
        out.extend_from_slice(&super_class_idx.to_be_bytes());

        // interfaces_count + interfaces
        out.extend_from_slice(&(interfaces.len() as u16).to_be_bytes());
        for iface in interfaces {
            out.extend_from_slice(&iface.to_be_bytes());
        }

        // fields_count + raw field bytes
        out.extend_from_slice(&(fields.len() as u16).to_be_bytes());
        for f in fields {
            out.extend_from_slice(f);
        }

        // methods_count + raw method bytes
        out.extend_from_slice(&(methods.len() as u16).to_be_bytes());
        for m in methods {
            out.extend_from_slice(m);
        }

        // attributes_count = 0 (no SourceFile, no BootstrapMethods)
        out.extend_from_slice(&0u16.to_be_bytes());
        out
    }
}

/// Build the bytes for a minimal no-arg `<init>()V` that delegates to
/// `super.<init>()`. Returns the method_info bytes the class writer
/// concatenates into the `methods` table.
fn emit_default_ctor(
    name_idx: u16,
    descriptor_idx: u16,
    code_attr_name_idx: u16,
    super_init_methodref: u16,
) -> Vec<u8> {
    // method_info: access_flags(u16), name_index(u16), descriptor_index(u16),
    //              attributes_count(u16), attributes...
    let mut method = Vec::new();
    method.extend_from_slice(&0x0001u16.to_be_bytes()); // ACC_PUBLIC
    method.extend_from_slice(&name_idx.to_be_bytes()); // "<init>"
    method.extend_from_slice(&descriptor_idx.to_be_bytes()); // "()V"
    method.extend_from_slice(&1u16.to_be_bytes()); // attributes_count = 1 (Code)

    // Code attribute payload:
    //   max_stack(u16), max_locals(u16), code_length(u32), code(bytes),
    //   exception_table_length(u16), attributes_count(u16)
    //
    // Body bytecode:
    //   aload_0          (0x2A)
    //   invokespecial #super_init_methodref (0xB7, u16)
    //   return           (0xB1)
    let code: [u8; 5] = [
        0x2A,
        0xB7,
        (super_init_methodref >> 8) as u8,
        (super_init_methodref & 0xFF) as u8,
        0xB1,
    ];

    let mut code_attr = Vec::new();
    code_attr.extend_from_slice(&1u16.to_be_bytes()); // max_stack
    code_attr.extend_from_slice(&1u16.to_be_bytes()); // max_locals (this only)
    code_attr.extend_from_slice(&(code.len() as u32).to_be_bytes()); // code_length
    code_attr.extend_from_slice(&code);
    code_attr.extend_from_slice(&0u16.to_be_bytes()); // exception_table_length
    code_attr.extend_from_slice(&0u16.to_be_bytes()); // attributes_count (no StackMapTable; v52 lets us skip it)

    // attribute_info: name_index(u16), length(u32), info(bytes)
    method.extend_from_slice(&code_attr_name_idx.to_be_bytes());
    method.extend_from_slice(&(code_attr.len() as u32).to_be_bytes());
    method.extend_from_slice(&code_attr);
    method
}

/// Emit a constructor matching `desc` that delegates to the superclass's
/// same-descriptor constructor (`super(arg0, arg1, ...)`), mirroring what
/// real CGLIB emits for EVERY non-private superclass constructor -- not
/// just a no-arg one. Constructors are never inherited, so a superclass
/// with no no-arg constructor (any `@Configuration` class using
/// constructor injection, e.g. `AutowiredMixedCglibConfiguration
/// (Environment env)`) needs its own matching constructor on the
/// generated subclass; `emit_default_ctor`'s single hardcoded `()V`
/// version left such proxies with no usable constructor at all --
/// `Class.getConstructor(Environment.class)` failed with
/// `NoSuchMethodException`, surfacing during AOT processing as
/// `ConfigurationClassPostProcessor$...ProxyBeanRegistrationCodeFragments
/// .proxyInstantiationDescriptor`'s `IllegalStateException: No matching
/// constructor found on proxy class`.
fn emit_ctor_for_descriptor(
    cw: &mut ClassWriter,
    init_name_idx: u16,
    code_attr_name_idx: u16,
    super_class_idx: u16,
    desc: &str,
) -> Vec<u8> {
    let desc_idx = cw.add_utf8(desc);
    let super_init_ref = cw.add_methodref(super_class_idx, "<init>", desc);

    let params = parse_param_descriptors(desc);
    let mut code = vec![0x2Au8]; // aload_0
    let mut slot: u16 = 1;
    for p in &params {
        let load_op: u8 = match p.as_str() {
            "I" | "Z" | "B" | "C" | "S" => 0x15, // iload
            "J" => 0x16,                         // lload
            "F" => 0x17,                         // fload
            "D" => 0x18,                         // dload
            _ => 0x19,                           // aload (reference/array)
        };
        code.push(load_op);
        code.push(slot as u8);
        slot += if p == "J" || p == "D" { 2 } else { 1 };
    }
    code.push(0xB7); // invokespecial
    code.extend_from_slice(&super_init_ref.to_be_bytes());
    code.push(0xB1); // return

    // max_stack: `this` plus every argument, counting long/double as 2
    // stack words -- matches how they sit on the stack right before the
    // invokespecial consumes them all at once.
    let max_stack = 1 + params
        .iter()
        .map(|p| if p == "J" || p == "D" { 2 } else { 1 })
        .sum::<u16>();

    wrap_method(init_name_idx, desc_idx, code_attr_name_idx, &code, max_stack, slot)
}

/// Emit a no-op `public static CGLIB$SET_STATIC_CALLBACKS([Lorg/springframework
/// /cglib/proxy/Callback;)V` (and, separately, an identically-shaped
/// `CGLIB$SET_THREAD_CALLBACKS`). Real CGLIB emits both on every class it
/// generates; `Enhancer.isEnhanced()` / `registerCallbacks()` /
/// `registerStaticCallbacks()` do a plain `Class.getDeclaredMethod(name,
/// Callback[].class)` lookup for these EXACT names and throw
/// `IllegalArgumentException("... is not an enhanced class")`
/// (`Enhancer.setCallbacksHelper`) if either is missing -- observed via
/// `ConfigurationClassUtils.initializeConfigurationClass`, which the
/// generated bean-registration source calls unconditionally and which
/// itself calls `Enhancer.registerStaticCallbacks(configClass, CALLBACKS)`.
///
/// This native reimplementation never stores or reads a callback array --
/// the `@Bean` method overrides in `emit_bean_override` inline their
/// container-dispatch logic directly, with no indirection through
/// `Callback` objects -- so both setters are legitimately no-ops here;
/// their only job is to satisfy this reflective "is this class enhanced"
/// check.
fn emit_noop_callback_setter(cw: &mut ClassWriter, name_idx: u16, code_attr_name_idx: u16) -> Vec<u8> {
    let desc_idx = cw.add_utf8("([Lorg/springframework/cglib/proxy/Callback;)V");
    let code: [u8; 1] = [0xB1]; // return
    let mut method = Vec::new();
    method.extend_from_slice(&0x0009u16.to_be_bytes()); // ACC_PUBLIC | ACC_STATIC
    method.extend_from_slice(&name_idx.to_be_bytes());
    method.extend_from_slice(&desc_idx.to_be_bytes());
    method.extend_from_slice(&1u16.to_be_bytes()); // attributes_count = 1 (Code)

    let mut code_attr = Vec::new();
    code_attr.extend_from_slice(&0u16.to_be_bytes()); // max_stack
    code_attr.extend_from_slice(&1u16.to_be_bytes()); // max_locals (the Callback[] param, slot 0 -- static method, no `this`)
    code_attr.extend_from_slice(&(code.len() as u32).to_be_bytes());
    code_attr.extend_from_slice(&code);
    code_attr.extend_from_slice(&0u16.to_be_bytes()); // exception_table_length
    code_attr.extend_from_slice(&0u16.to_be_bytes()); // attributes_count

    method.extend_from_slice(&code_attr_name_idx.to_be_bytes());
    method.extend_from_slice(&(code_attr.len() as u32).to_be_bytes());
    method.extend_from_slice(&code_attr);
    method
}

/// Build the bytes for `setBeanFactory(BeanFactory bf)V` that STORES the
/// factory into the `$$beanFactory` field: `this.$$beanFactory = bf; return`.
/// The marker interface `EnhancedConfiguration` extends `BeanFactoryAware`,
/// whose `setBeanFactory` callback Spring invokes during context refresh —
/// this is the hook that makes the factory available to the `@Bean`-method
/// override interceptors (which read `$$beanFactory` to resolve inter-bean
/// references back through the factory, giving shared-singleton semantics).
fn emit_set_bean_factory(
    name_idx: u16,
    descriptor_idx: u16,
    code_attr_name_idx: u16,
    bf_field_ref: u16,
) -> Vec<u8> {
    let mut method = Vec::new();
    method.extend_from_slice(&0x0001u16.to_be_bytes()); // ACC_PUBLIC
    method.extend_from_slice(&name_idx.to_be_bytes()); // "setBeanFactory"
    method.extend_from_slice(&descriptor_idx.to_be_bytes()); // "(Lorg/springframework/beans/factory/BeanFactory;)V"
    method.extend_from_slice(&1u16.to_be_bytes()); // attributes_count = 1 (Code)

    // Body: aload_0; aload_1; putfield #bf_field_ref; return
    let code: [u8; 6] = [
        0x2A, // aload_0 (this)
        0x2B, // aload_1 (bf)
        0xB5, // putfield
        (bf_field_ref >> 8) as u8,
        (bf_field_ref & 0xFF) as u8,
        0xB1, // return
    ];

    let mut code_attr = Vec::new();
    code_attr.extend_from_slice(&2u16.to_be_bytes()); // max_stack
    code_attr.extend_from_slice(&2u16.to_be_bytes()); // max_locals (this + factory arg)
    code_attr.extend_from_slice(&(code.len() as u32).to_be_bytes()); // code_length
    code_attr.extend_from_slice(&code);
    code_attr.extend_from_slice(&0u16.to_be_bytes()); // exception_table_length
    code_attr.extend_from_slice(&0u16.to_be_bytes()); // attributes_count (no StackMapTable; v52 lets us skip)

    method.extend_from_slice(&code_attr_name_idx.to_be_bytes());
    method.extend_from_slice(&(code_attr.len() as u32).to_be_bytes());
    method.extend_from_slice(&code_attr);
    method
}

/// One `@Bean` method to override on the enhanced subclass.
struct BeanMethod {
    /// Method name (also the default bean name).
    name: String,
    /// Method descriptor, e.g. `()Lcom/example/Engine;`.
    descriptor: String,
    /// Internal name of the (reference) return type, for the result checkcast.
    return_internal: String,
    /// True when `return_internal` is (or transitively implements/extends)
    /// `org/springframework/beans/factory/FactoryBean`. An inter-bean
    /// reference to such a method must resolve the *raw* FactoryBean via
    /// `getBean("&<name>")`, not the product via `getBean("<name>")` — see
    /// `emit_bean_override`'s `beanname_lookup_idx` and SPR-6602.
    is_factory_bean: bool,
}

/// Emit a `@Bean`-method override mirroring Spring's `BeanMethodInterceptor`:
///
/// ```text
/// Method m = SimpleInstantiationStrategy.getCurrentlyInvokedFactoryMethod();
/// if (m != null && m.getName().equals("<name>"))             // the factory is
///     return super.<name>();                                  //   creating THIS bean
/// return (<Ret>) ((BeanFactory) this.$$beanFactory).getBean("<name>");  // inter-bean ref → shared
/// ```
///
/// This is the *exact* discriminator Spring uses
/// (`ConfigurationClassEnhancer$BeanMethodInterceptor.isCurrentlyInvokedFactoryMethod`):
/// `SimpleInstantiationStrategy.instantiate(..factoryMethod..)` sets a
/// thread-local to the `@Bean` factory method it is about to invoke reflectively,
/// so the *creating* call (which dispatches into this override) sees its own
/// method name and runs `super` (the real `@Bean` body — no re-entrancy, since
/// the super class is the original `@Configuration`). Any other call is an
/// inter-bean reference and routes through `getBean`, returning the
/// container-managed instance.
///
/// The previous heuristic keyed on `isSingletonCurrentlyInCreation("<name>")`,
/// which silently breaks whenever the bean *being created* has a different name
/// than the `@Bean` method — the case for **scoped proxies** (the real target is
/// registered as `scopedTarget.<name>`) and **lazy-init AOP proxies**. There the
/// creating call (`getBean("scopedTarget.<name>")` → factory method) found
/// `<name>` NOT in creation, fell through to `getBean("<name>")` which returns
/// the *proxy*, so the proxy's `TargetSource.getTarget()` resolved back to the
/// proxy itself → unbounded `proxy.method() → getTarget() → proxy.method()`
/// recursion (`StackOverflowError`). Keying on the factory-method thread-local
/// matches Spring and is name-agnostic, fixing both. No-arg reference-returning
/// methods only — so comparing the method name (params are always `()`) suffices.
///
/// The inter-bean `getBean("<name>")` call is further wrapped exactly like
/// Spring's `resolveBeanReference`: if `<name>` is already marked "currently
/// in creation" (an enclosing `getSingleton()` further up the call stack,
/// e.g. a sibling `@Bean` method invoked from this same class's own
/// `@PostConstruct` before the factory-method thread-local above is ever
/// set — SPR-8080), that flag is temporarily cleared for the duration of the
/// nested `getBean` call and unconditionally restored afterward (success or
/// exception) via a `finally`-shaped exception-table entry. Without this,
/// the nested lookup trips `BeanCurrentlyInCreationException` even though
/// real Spring resolves it fine.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
fn emit_bean_override(
    name_idx: u16,
    descriptor_idx: u16,
    code_attr_name_idx: u16,
    name_string_idx: u16,
    super_method_ref: u16,
    bf_field_ref: u16,
    beanfactory_cast_idx: u16,
    getbean_ref: u16,
    rettype_cast_idx: u16,
    get_factory_method_ref: u16,
    method_get_name_ref: u16,
    string_equals_ref: u16,
    // Configuration-bean-name-generator support (mirrors
    // BeanAnnotationHelper.determineBeanNameFor): when the beanFactory has a
    // ConfigurationBeanNameGenerator registered under
    // CONFIGURATION_BEAN_NAME_GENERATOR, the inter-bean `getBean` must resolve
    // the generator-derived name (e.g. FullyQualifiedConfigurationBeanNameGenerator
    // → "declaringClass.methodName"), not the plain method name.
    singleton_registry_cast_idx: u16,
    get_singleton_ref: u16,
    config_gen_iface_idx: u16,
    cfg_gen_name_string_idx: u16,
    // Default beanName used for the inter-bean `getBean` call (offset 31)
    // and its ConfigurationBeanNameGenerator-derived override (offset 60).
    // Equal to `name_string_idx`/`fq_name_string_idx` (the plain names) for
    // ordinary `@Bean` methods; "&"-prefixed for FactoryBean-typed ones so
    // the raw factory is resolved instead of its product (SPR-6602 et al).
    beanname_lookup_idx: u16,
    fq_beanname_lookup_idx: u16,
    // Mirrors Spring's `resolveBeanReference`: after resolving an inter-bean
    // reference, if a factory method is currently being invoked (local1
    // non-null — i.e. this call happened DURING another bean's creation, not
    // from arbitrary user code), register a dependency edge from the
    // resolved bean onto that outer bean via
    // `ConfigurableBeanFactory.registerDependentBean(beanName, outerBeanName)`.
    configurable_bf_cast_idx: u16,
    register_dependent_bean_ref: u16,
    // Mirrors Spring's `resolveBeanReference`'s reentrancy guard: the bean
    // being referenced may already be marked "currently in creation" by an
    // enclosing `getSingleton()` call further up the stack (e.g. a
    // `@PostConstruct` method on the *factory* `@Configuration` bean calling
    // one of its own sibling `@Bean` methods before the factory-method
    // thread-local is set — see `emit_bean_override`'s doc comment). Without
    // temporarily clearing that flag around the inter-bean `getBean` call,
    // the nested lookup trips `BeanCurrentlyInCreationException` even though
    // real Spring resolves it fine (SPR-8080).
    is_currently_in_creation_ref: u16,
    set_currently_in_creation_ref: u16,
) -> Vec<u8> {
    let b = |x: u16| -> [u8; 2] { x.to_be_bytes() };
    let mut code: Vec<u8> = Vec::new();
    // 0:  invokestatic SimpleInstantiationStrategy.getCurrentlyInvokedFactoryMethod()
    code.push(0xB8);
    code.extend_from_slice(&b(get_factory_method_ref));
    // 3:  astore_1   (local1 = currentlyInvoked Method, or null)
    code.push(0x4C);
    // 4:  aload_1
    code.push(0x2B);
    // 5:  ifnull → L_getbean (26); offset 21  (no factory method → inter-bean ref)
    code.push(0xC6);
    code.extend_from_slice(&b(21));
    // 8:  aload_1
    code.push(0x2B);
    // 9:  invokevirtual Method.getName()Ljava/lang/String;
    code.push(0xB6);
    code.extend_from_slice(&b(method_get_name_ref));
    // 12: ldc_w "<name>"
    code.push(0x13);
    code.extend_from_slice(&b(name_string_idx));
    // 15: invokevirtual String.equals(Object)Z
    code.push(0xB6);
    code.extend_from_slice(&b(string_equals_ref));
    // 18: ifeq → L_getbean (26); offset 8  (different method → inter-bean ref)
    code.push(0x99);
    code.extend_from_slice(&b(8));
    // 21: L_super: aload_0  (the factory IS creating this bean → real body)
    code.push(0x2A);
    // 22: invokespecial super.<name><desc>
    code.push(0xB7);
    code.extend_from_slice(&b(super_method_ref));
    // 25: areturn
    code.push(0xB0);
    // --- L_getbean (26): resolve the container bean name, then getBean(name). ---
    // String beanName = "<plain name>";
    // if (bf instanceof SingletonBeanRegistry sbr
    //         && sbr.getSingleton(CONFIGURATION_BEAN_NAME_GENERATOR)
    //                instanceof ConfigurationBeanNameGenerator) {
    //     beanName = "<fq name>";   // e.g. declaringClass.methodName
    // }
    // return (<Ret>) ((BeanFactory) bf).getBean(beanName);
    // 26: aload_0
    code.push(0x2A);
    // 27: getfield this.$$beanFactory     (Object bf)
    code.push(0xB4);
    code.extend_from_slice(&b(bf_field_ref));
    // 30: astore_2   (local2 = bf)
    code.push(0x4D);
    // 31: ldc_w "<name>"   (default beanName — plain, or "&name" for a
    //     FactoryBean-typed method so the raw factory is resolved instead
    //     of its product; see `beanname_lookup_idx`)
    code.push(0x13);
    code.extend_from_slice(&b(beanname_lookup_idx));
    // 34: astore_3   (local3 = beanName)
    code.push(0x4E);
    // 35: aload_2
    code.push(0x2C);
    // 36: instanceof SingletonBeanRegistry
    code.push(0xC1);
    code.extend_from_slice(&b(singleton_registry_cast_idx));
    // 39: ifeq → L_get (64); offset 25
    code.push(0x99);
    code.extend_from_slice(&b(25));
    // 42: aload_2
    code.push(0x2C);
    // 43: checkcast SingletonBeanRegistry
    code.push(0xC0);
    code.extend_from_slice(&b(singleton_registry_cast_idx));
    // 46: ldc_w CONFIGURATION_BEAN_NAME_GENERATOR
    code.push(0x13);
    code.extend_from_slice(&b(cfg_gen_name_string_idx));
    // 49: invokeinterface SingletonBeanRegistry.getSingleton(String)Object  count=2
    code.push(0xB9);
    code.extend_from_slice(&b(get_singleton_ref));
    code.push(0x02);
    code.push(0x00);
    // 54: instanceof ConfigurationBeanNameGenerator
    code.push(0xC1);
    code.extend_from_slice(&b(config_gen_iface_idx));
    // 57: ifeq → L_get (64); offset 7
    code.push(0x99);
    code.extend_from_slice(&b(7));
    // 60: ldc_w "<fq name>"   (plain, or "&"-prefixed for FactoryBean methods)
    code.push(0x13);
    code.extend_from_slice(&b(fq_beanname_lookup_idx));
    // 63: astore_3   (local3 = fq name)
    code.push(0x4E);
    // 64: L_get: aload_2   (bf)
    code.push(0x2C);
    // 65: checkcast ConfigurableBeanFactory   (used for isCurrentlyInCreation/
    //     setCurrentlyInCreation/getBean/registerDependentBean — all four
    //     live on this one interface, so a single cast covers everything
    //     below; ConfigurableBeanFactory extends BeanFactory).
    code.push(0xC0);
    code.extend_from_slice(&b(configurable_bf_cast_idx));
    // 68: astore 6   (local6 = cbf)
    code.push(0x3A);
    code.push(0x06);
    // 70: aload 6
    code.push(0x19);
    code.push(0x06);
    // 72: aload_3   (beanName)
    code.push(0x2D);
    // 73: invokeinterface ConfigurableBeanFactory.isCurrentlyInCreation(String)Z  count=2
    code.push(0xB9);
    code.extend_from_slice(&b(is_currently_in_creation_ref));
    code.push(0x02);
    code.push(0x00);
    // 78: istore 5   (local5 = alreadyInCreation)
    code.push(0x36);
    code.push(0x05);
    // 80: iload 5
    code.push(0x15);
    code.push(0x05);
    // 82: ifeq → TRY_START (94); offset 12  (not already in creation → skip
    //     the temporary clear)
    code.push(0x99);
    code.extend_from_slice(&b(12));
    // 85: aload 6
    code.push(0x19);
    code.push(0x06);
    // 87: aload_3   (beanName)
    code.push(0x2D);
    // 88: iconst_0
    code.push(0x03);
    // 89: invokeinterface ConfigurableBeanFactory.setCurrentlyInCreation(String,boolean)V  count=3
    code.push(0xB9);
    code.extend_from_slice(&b(set_currently_in_creation_ref));
    code.push(0x03);
    code.push(0x00);
    // --- TRY_START (94): the actual inter-bean getBean() call, protected by
    // the exception-table entry below so the "currently in creation" flag
    // gets restored (in the handler) even if getBean throws — mirrors
    // Spring's resolveBeanReference try/finally exactly. ---
    // 94: aload_2   (bf)
    code.push(0x2C);
    // 95: checkcast BeanFactory
    code.push(0xC0);
    code.extend_from_slice(&b(beanfactory_cast_idx));
    // 98: aload_3   (beanName)
    code.push(0x2D);
    // 99: invokeinterface BeanFactory.getBean(String)Object  count=2
    code.push(0xB9);
    code.extend_from_slice(&b(getbean_ref));
    code.push(0x02);
    code.push(0x00);
    // 104: checkcast <Ret>
    code.push(0xC0);
    code.extend_from_slice(&b(rettype_cast_idx));
    // 107: astore 4   (local4 = result)
    code.push(0x3A);
    code.push(0x04);
    // --- TRY_END (109, exclusive). Normal path continues below: mirrors
    // Spring's resolveBeanReference: only when a factory method IS currently
    // being invoked (local1 non-null), i.e. this getBean happened while
    // another @Bean method was under construction, not from arbitrary user
    // code, register a dependency edge. ---
    // 109: aload_1   (currentlyInvoked Method, or null)
    code.push(0x2B);
    // 110: ifnull → L_restore (125); offset 15  (no outer factory method → skip)
    code.push(0xC6);
    code.extend_from_slice(&b(15));
    // 113: aload 6   (cbf — already ConfigurableBeanFactory, no re-cast needed)
    code.push(0x19);
    code.push(0x06);
    // 115: aload_3   (beanName)
    code.push(0x2D);
    // 116: aload_1   (currentlyInvoked Method)
    code.push(0x2B);
    // 117: invokevirtual Method.getName()Ljava/lang/String;   (outerBeanName)
    code.push(0xB6);
    code.extend_from_slice(&b(method_get_name_ref));
    // 120: invokeinterface ConfigurableBeanFactory.registerDependentBean(String,String)V  count=3
    code.push(0xB9);
    code.extend_from_slice(&b(register_dependent_bean_ref));
    code.push(0x03);
    code.push(0x00);
    // 125: L_restore: iload 5   (alreadyInCreation)
    code.push(0x15);
    code.push(0x05);
    // 127: ifeq → L_return (139); offset 12  (wasn't already in creation → skip restore)
    code.push(0x99);
    code.extend_from_slice(&b(12));
    // 130: aload 6
    code.push(0x19);
    code.push(0x06);
    // 132: aload_3   (beanName)
    code.push(0x2D);
    // 133: iconst_1
    code.push(0x04);
    // 134: invokeinterface ConfigurableBeanFactory.setCurrentlyInCreation(String,boolean)V  count=3
    code.push(0xB9);
    code.extend_from_slice(&b(set_currently_in_creation_ref));
    code.push(0x03);
    code.push(0x00);
    // 139: L_return: aload 4
    code.push(0x19);
    code.push(0x04);
    // 141: areturn
    code.push(0xB0);
    // --- HANDLER (142): catches any Throwable from the TRY region (start_pc
    // 94, end_pc 109 exclusive — the getBean call only). Restores the
    // "currently in creation" flag exactly like the normal path, then
    // rethrows unchanged (a real `finally` never swallows). ---
    // 142: astore 7   (local7 = throwable)
    code.push(0x3A);
    code.push(0x07);
    // 144: iload 5
    code.push(0x15);
    code.push(0x05);
    // 146: ifeq → RETHROW (158); offset 12
    code.push(0x99);
    code.extend_from_slice(&b(12));
    // 149: aload 6
    code.push(0x19);
    code.push(0x06);
    // 151: aload_3   (beanName)
    code.push(0x2D);
    // 152: iconst_1
    code.push(0x04);
    // 153: invokeinterface ConfigurableBeanFactory.setCurrentlyInCreation(String,boolean)V  count=3
    code.push(0xB9);
    code.extend_from_slice(&b(set_currently_in_creation_ref));
    code.push(0x03);
    code.push(0x00);
    // 158: RETHROW: aload 7
    code.push(0x19);
    code.push(0x07);
    // 160: athrow
    code.push(0xBF);
    debug_assert_eq!(code.len(), 161);

    const TRY_START: u16 = 94;
    const TRY_END: u16 = 109;
    const HANDLER_PC: u16 = 142;

    let mut code_attr = Vec::new();
    code_attr.extend_from_slice(&3u16.to_be_bytes()); // max_stack (3-deep setCurrentlyInCreation/registerDependentBean calls)
    code_attr.extend_from_slice(&8u16.to_be_bytes()); // max_locals (this, method, bf, beanName, result, alreadyInCreation, cbf, throwable)
    code_attr.extend_from_slice(&(code.len() as u32).to_be_bytes());
    code_attr.extend_from_slice(&code);
    code_attr.extend_from_slice(&1u16.to_be_bytes()); // exception_table_length
    code_attr.extend_from_slice(&TRY_START.to_be_bytes());
    code_attr.extend_from_slice(&TRY_END.to_be_bytes());
    code_attr.extend_from_slice(&HANDLER_PC.to_be_bytes());
    code_attr.extend_from_slice(&0u16.to_be_bytes()); // catch_type 0 = any (finally semantics)
    code_attr.extend_from_slice(&0u16.to_be_bytes()); // attributes_count

    let mut method = Vec::new();
    method.extend_from_slice(&0x0001u16.to_be_bytes()); // ACC_PUBLIC
    method.extend_from_slice(&name_idx.to_be_bytes());
    method.extend_from_slice(&descriptor_idx.to_be_bytes());
    method.extend_from_slice(&1u16.to_be_bytes()); // attributes_count = 1 (Code)
    method.extend_from_slice(&code_attr_name_idx.to_be_bytes());
    method.extend_from_slice(&(code_attr.len() as u32).to_be_bytes());
    method.extend_from_slice(&code_attr);
    method
}

/// Emit a `$$beanFactory` field of type `Ljava/lang/Object;` (private synthetic).
fn emit_bean_factory_field(name_idx: u16, descriptor_idx: u16) -> Vec<u8> {
    let mut field = Vec::new();
    field.extend_from_slice(&(0x0002u16 | 0x1000u16).to_be_bytes()); // ACC_PRIVATE | ACC_SYNTHETIC
    field.extend_from_slice(&name_idx.to_be_bytes()); // "$$beanFactory"
    field.extend_from_slice(&descriptor_idx.to_be_bytes()); // "Ljava/lang/Object;"
    field.extend_from_slice(&0u16.to_be_bytes()); // attributes_count = 0
    field
}

/// Generate a fresh `<OriginalName>$$SpringCGLIB$$<counter>` class
/// file as a `Vec<u8>` plus the chosen internal-name. The new class
/// extends `super_internal_name` and implements
/// [`SPRING_MARKER_IFACE`].
fn build_enhancer_class(
    super_internal_name: &str,
    bean_methods: &[BeanMethod],
    ctor_descriptors: &[String],
) -> (String, Vec<u8>) {
    let counter = next_config_enhancer_counter(super_internal_name);
    // Real CGLIB's naming policy is overridden by Spring's own
    // `SpringNamingPolicy` (see `newEnhancer()` in
    // `ConfigurationClassEnhancer.java`): tag is "SpringCGLIB", not the
    // default "EnhancerByCGLIB", and the suffix is a PLAIN decimal counter
    // (not a hash/hex value). Generated bean-registration source references
    // the proxy class by this exact name (e.g. `new Foo$$SpringCGLIB$$0(...)`),
    // so a mismatched name here means "cannot find symbol" at compile time
    // even though a class WAS generated -- just under the wrong name.
    let new_name = format!("{super_internal_name}$$SpringCGLIB$${counter}");

    let mut cw = ClassWriter::new();

    // -- CONSTANT_Class entries: this, super, and the marker interface.
    let this_class_idx = cw.add_class(&new_name);
    let super_class_idx = cw.add_class(super_internal_name);
    let iface_idx = cw.add_class(SPRING_MARKER_IFACE);

    // -- Method machinery: <init> name + Code attribute name. Each
    // superclass constructor gets its own descriptor Utf8 + super methodref,
    // added inside `emit_ctor_for_descriptor` below.
    let init_name_idx = cw.add_utf8("<init>");
    let code_attr_name_idx = cw.add_utf8("Code");

    // -- `$$beanFactory` field (type Object) + its Fieldref on THIS class.
    let bf_field_name_idx = cw.add_utf8("$$beanFactory");
    let object_desc_idx = cw.add_utf8("Ljava/lang/Object;");
    let bf_field_ref = cw.add_fieldref(this_class_idx, "$$beanFactory", "Ljava/lang/Object;");

    // -- setBeanFactory(BeanFactory)V — required because the marker interface
    // `EnhancedConfiguration` extends `BeanFactoryAware`. Now stores into the
    // `$$beanFactory` field (was a no-op).
    let set_bf_name_idx = cw.add_utf8("setBeanFactory");
    let set_bf_desc_idx = cw.add_utf8("(Lorg/springframework/beans/factory/BeanFactory;)V");

    // Every non-private superclass constructor gets a matching delegating
    // constructor on the proxy (real CGLIB does the same for each one) --
    // see `emit_ctor_for_descriptor`'s doc comment for why a single
    // hardcoded `()V` ctor isn't enough.
    let ctor_descs: Vec<String> = if ctor_descriptors.is_empty() {
        vec!["()V".to_string()]
    } else {
        ctor_descriptors.to_vec()
    };
    let mut methods: Vec<Vec<u8>> = ctor_descs
        .iter()
        .map(|d| emit_ctor_for_descriptor(&mut cw, init_name_idx, code_attr_name_idx, super_class_idx, d))
        .collect();

    let set_bean_factory = emit_set_bean_factory(
        set_bf_name_idx,
        set_bf_desc_idx,
        code_attr_name_idx,
        bf_field_ref,
    );
    methods.push(set_bean_factory);

    let set_static_callbacks_name_idx = cw.add_utf8("CGLIB$SET_STATIC_CALLBACKS");
    methods.push(emit_noop_callback_setter(&mut cw, set_static_callbacks_name_idx, code_attr_name_idx));
    let set_thread_callbacks_name_idx = cw.add_utf8("CGLIB$SET_THREAD_CALLBACKS");
    methods.push(emit_noop_callback_setter(&mut cw, set_thread_callbacks_name_idx, code_attr_name_idx));

    let field = emit_bean_factory_field(bf_field_name_idx, object_desc_idx);

    if !bean_methods.is_empty() {
        // Shared constant-pool refs used by every @Bean override.
        let beanfactory_cast_idx = cw.add_class("org/springframework/beans/factory/BeanFactory");
        let getbean_ref = cw.add_interface_methodref(
            beanfactory_cast_idx,
            "getBean",
            "(Ljava/lang/String;)Ljava/lang/Object;",
        );
        // The factory-method thread-local that distinguishes "the container is
        // creating this bean" (→ super) from an inter-bean reference (→ getBean),
        // exactly like Spring's BeanMethodInterceptor.isCurrentlyInvokedFactoryMethod.
        let sis_cls =
            cw.add_class("org/springframework/beans/factory/support/SimpleInstantiationStrategy");
        let get_factory_method_ref = cw.add_methodref(
            sis_cls,
            "getCurrentlyInvokedFactoryMethod",
            "()Ljava/lang/reflect/Method;",
        );
        let method_cls = cw.add_class("java/lang/reflect/Method");
        let method_get_name_ref = cw.add_methodref(method_cls, "getName", "()Ljava/lang/String;");
        let string_cls = cw.add_class("java/lang/String");
        let string_equals_ref = cw.add_methodref(string_cls, "equals", "(Ljava/lang/Object;)Z");

        // ConfigurationBeanNameGenerator support: when the container has a
        // ConfigurationBeanNameGenerator registered under
        // CONFIGURATION_BEAN_NAME_GENERATOR, an inter-bean `getBean` must use the
        // generator-derived name (mirrors BeanAnnotationHelper.determineBeanNameFor).
        let singleton_registry_cast_idx =
            cw.add_class("org/springframework/beans/factory/config/SingletonBeanRegistry");
        let get_singleton_ref = cw.add_interface_methodref(
            singleton_registry_cast_idx,
            "getSingleton",
            "(Ljava/lang/String;)Ljava/lang/Object;",
        );
        // Spring's BeanMethodInterceptor.resolveBeanReference registers a bean
        // dependency edge (beanFactory.getDependentBeans(...)) whenever an
        // inter-bean reference resolves while another @Bean method is being
        // created — used to order singleton destruction correctly and
        // surfaced directly by ConfigurationClassPostProcessorTests'
        // enhancementIsPresent* assertions.
        let configurable_bf_cast_idx =
            cw.add_class("org/springframework/beans/factory/config/ConfigurableBeanFactory");
        let register_dependent_bean_ref = cw.add_interface_methodref(
            configurable_bf_cast_idx,
            "registerDependentBean",
            "(Ljava/lang/String;Ljava/lang/String;)V",
        );
        // SPR-8080 reentrancy guard (see `emit_bean_override`'s doc comment):
        // temporarily clear/restore "currently in creation" around the
        // inter-bean `getBean` call so a nested reference to a bean already
        // marked in-creation by an enclosing `getSingleton()` doesn't trip
        // `BeanCurrentlyInCreationException`.
        let is_currently_in_creation_ref = cw.add_interface_methodref(
            configurable_bf_cast_idx,
            "isCurrentlyInCreation",
            "(Ljava/lang/String;)Z",
        );
        let set_currently_in_creation_ref = cw.add_interface_methodref(
            configurable_bf_cast_idx,
            "setCurrentlyInCreation",
            "(Ljava/lang/String;Z)V",
        );
        let config_gen_iface_idx =
            cw.add_class("org/springframework/context/annotation/ConfigurationBeanNameGenerator");
        let cfg_gen_name_string_idx = cw.add_string(
            "org.springframework.context.annotation.internalConfigurationBeanNameGenerator",
        );
        // Dotted declaring-class name for the "declaringClass.methodName" default
        // used by FullyQualifiedConfigurationBeanNameGenerator.deriveBeanName.
        let super_dotted = super_internal_name.replace('/', ".");

        for bm in bean_methods {
            let name_idx = cw.add_utf8(&bm.name);
            let desc_idx = cw.add_utf8(&bm.descriptor);
            let name_string_idx = cw.add_string(&bm.name);
            let fq_name_string_idx = cw.add_string(&format!("{super_dotted}.{}", bm.name));
            // FactoryBean-typed @Bean methods: an inter-bean reference must
            // resolve the raw factory via getBean("&<name>"), not the
            // product via getBean("<name>") — see SPR-6602/11202/15275 and
            // `is_factory_bean_type`. The Method.getName() comparison
            // (name_string_idx, offset 12) always stays the plain method
            // name; only the default-beanName lookup constants change.
            let beanname_lookup_idx = if bm.is_factory_bean {
                cw.add_string(&format!("&{}", bm.name))
            } else {
                name_string_idx
            };
            let fq_beanname_lookup_idx = if bm.is_factory_bean {
                cw.add_string(&format!("&{super_dotted}.{}", bm.name))
            } else {
                fq_name_string_idx
            };
            let super_method_ref = cw.add_methodref(super_class_idx, &bm.name, &bm.descriptor);
            let rettype_cast_idx = cw.add_class(&bm.return_internal);
            let override_method = emit_bean_override(
                name_idx,
                desc_idx,
                code_attr_name_idx,
                name_string_idx,
                super_method_ref,
                bf_field_ref,
                beanfactory_cast_idx,
                getbean_ref,
                rettype_cast_idx,
                get_factory_method_ref,
                method_get_name_ref,
                string_equals_ref,
                singleton_registry_cast_idx,
                get_singleton_ref,
                config_gen_iface_idx,
                cfg_gen_name_string_idx,
                beanname_lookup_idx,
                fq_beanname_lookup_idx,
                configurable_bf_cast_idx,
                register_dependent_bean_ref,
                is_currently_in_creation_ref,
                set_currently_in_creation_ref,
            );
            methods.push(override_method);
        }
    }

    // access_flags = ACC_PUBLIC (0x0001) | ACC_SUPER (0x0020) — JLS-required
    // for any new class file. ACC_SYNTHETIC (0x1000) flags the generated
    // class so reflection sees it as synthetic, matching CGLIB's emit.
    let access_flags: u16 = 0x0001 | 0x0020 | 0x1000;

    let bytes = cw.finish(
        access_flags,
        this_class_idx,
        super_class_idx,
        &[iface_idx],
        &[field],
        &methods,
    );

    (new_name, bytes)
}

// ===========================================================================
// Method-injection (lookup-method / @Lookup) subclass generation — bug-B2.
//
// Spring's `<lookup-method>` / `@Lookup` declares an ABSTRACT bean class whose
// abstract methods must be implemented by a CGLIB subclass that returns a bean
// from the owning factory on each call. The previous instantiation shim refused
// to instantiate abstract classes (returned null → "Target object must not be
// null"). Here we emit a concrete subclass:
//   * `$$beanFactory` field (Object) holding the owning BeanFactory.
//   * default `<init>()V` → super.<init>().
//   * for each abstract method that has a lookup override: an override returning
//     `(Ret) bf.getBean(name|Ret.class[, boxedArgs])`.
//   * for each abstract method WITHOUT an override: a body that throws
//     `AbstractMethodError` (so the subclass is concrete/instantiable and calling
//     an un-declared lookup fails exactly as on CGLIB).
// The owning factory is stored into `$$beanFactory` by the instantiation shim
// right after `new_object` (no BeanFactoryAware callback needed here).
// ===========================================================================

/// One abstract method to materialise on the generated lookup subclass.
pub struct LookupMethodSpec {
    /// Method name.
    pub name: String,
    /// JVM descriptor, e.g. `(Ljava/lang/String;)Lorg/.../TestBean;`.
    pub descriptor: String,
    /// Internal name of the (reference) return type, e.g. `org/.../TestBean`.
    pub return_internal: String,
    /// `Some(name)` → resolve by bean name; `None` → resolve by return type.
    pub bean_name: Option<String>,
    /// `true` → emit a `getBean` override; `false` → emit a throwing stub.
    pub is_lookup: bool,
}

/// Shared constant-pool indices used by every emitted lookup override.
struct LookupCp {
    code_attr_name_idx: u16,
    bf_field_ref: u16,
    beanfactory_cast_idx: u16,
    object_class_idx: u16,
    getbean_name_ref: u16,
    getbean_name_args_ref: u16,
    getbean_type_ref: u16,
    getbean_type_args_ref: u16,
    valueof_int: u16,
    valueof_long: u16,
    valueof_float: u16,
    valueof_double: u16,
    valueof_bool: u16,
    valueof_byte: u16,
    valueof_char: u16,
    valueof_short: u16,
    ame_class_idx: u16,
    ame_init_ref: u16,
    // Generic-aware by-type lookup: getBeanProvider(ResolvableType.forMethodReturnType(m)).getObject()
    class_class_idx: u16,
    object_getclass_ref: u16,
    class_getsuperclass_ref: u16,
    class_getdeclaredmethod_ref: u16,
    resolvabletype_formethodreturntype_ref: u16,
    getbeanprovider_ref: u16,
    objectprovider_getobject_ref: u16,
    // NullBean → null unwrap for by-name lookups
    class_getname_ref: u16,
    string_equals_ref: u16,
    nullbean_fqn_string_idx: u16,
}

/// Push a small int constant (`iconst`/`bipush`/`sipush`).
fn push_int(code: &mut Vec<u8>, v: i32) {
    match v {
        -1..=5 => code.push((0x03 + v) as u8), // iconst_m1..iconst_5
        -128..=127 => {
            code.push(0x10); // bipush
            code.push(v as i8 as u8);
        }
        _ => {
            code.push(0x11); // sipush
            code.extend_from_slice(&(v as i16).to_be_bytes());
        }
    }
}

/// Split a method descriptor into its parameter descriptor strings.
fn parse_param_descriptors(desc: &str) -> Vec<String> {
    let inner = match (desc.find('('), desc.find(')')) {
        (Some(a), Some(b)) if b > a => &desc[a + 1..b],
        _ => return Vec::new(),
    };
    let bytes = inner.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < bytes.len() {
        let start = i;
        while i < bytes.len() && bytes[i] == b'[' {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        if bytes[i] == b'L' {
            while i < bytes.len() && bytes[i] != b';' {
                i += 1;
            }
            i += 1; // consume ';'
        } else {
            i += 1; // single-char primitive
        }
        out.push(inner[start..i.min(inner.len())].to_string());
    }
    out
}

/// Emit `load + box` for one parameter at `slot`, leaving a reference on the
/// operand stack (primitives boxed via `valueOf`).
fn emit_load_box(code: &mut Vec<u8>, cp: &LookupCp, p: &str, slot: u16) {
    let b = |x: u16| x.to_be_bytes();
    // load instruction (indexed forms work for any slot)
    let (load_op, valueof): (u8, Option<u16>) = match p {
        "I" => (0x15, Some(cp.valueof_int)),
        "Z" => (0x15, Some(cp.valueof_bool)),
        "B" => (0x15, Some(cp.valueof_byte)),
        "C" => (0x15, Some(cp.valueof_char)),
        "S" => (0x15, Some(cp.valueof_short)),
        "J" => (0x16, Some(cp.valueof_long)),
        "F" => (0x17, Some(cp.valueof_float)),
        "D" => (0x18, Some(cp.valueof_double)),
        _ => (0x19, None), // aload (reference / array)
    };
    code.push(load_op);
    code.push(slot as u8);
    if let Some(vref) = valueof {
        code.push(0xB8); // invokestatic Boxtype.valueOf
        code.extend_from_slice(&b(vref));
    }
}

/// Wrap a code body into a `method_info` (ACC_PUBLIC, single Code attribute).
fn wrap_method(
    name_idx: u16,
    desc_idx: u16,
    code_attr_name_idx: u16,
    code: &[u8],
    max_stack: u16,
    max_locals: u16,
) -> Vec<u8> {
    let mut code_attr = Vec::new();
    code_attr.extend_from_slice(&max_stack.to_be_bytes());
    code_attr.extend_from_slice(&max_locals.to_be_bytes());
    code_attr.extend_from_slice(&(code.len() as u32).to_be_bytes());
    code_attr.extend_from_slice(code);
    code_attr.extend_from_slice(&0u16.to_be_bytes()); // exception_table_length
    code_attr.extend_from_slice(&0u16.to_be_bytes()); // attributes_count
    let mut method = Vec::new();
    method.extend_from_slice(&0x0001u16.to_be_bytes()); // ACC_PUBLIC
    method.extend_from_slice(&name_idx.to_be_bytes());
    method.extend_from_slice(&desc_idx.to_be_bytes());
    method.extend_from_slice(&1u16.to_be_bytes()); // attributes_count = 1 (Code)
    method.extend_from_slice(&code_attr_name_idx.to_be_bytes());
    method.extend_from_slice(&(code_attr.len() as u32).to_be_bytes());
    method.extend_from_slice(&code_attr);
    method
}

/// Number of local-variable slots the params occupy (long/double take 2).
fn param_slots(params: &[String]) -> u16 {
    let mut n = 0u16;
    for p in params {
        n += if p == "J" || p == "D" { 2 } else { 1 };
    }
    n
}

/// Build the bytes for `build_lookup_subclass`'s `<OriginalName>$$SpringCGLIB$$N`.
/// Returns the chosen internal name and the class-file bytes.
pub fn build_lookup_subclass(
    super_internal_name: &str,
    methods: &[LookupMethodSpec],
) -> (String, Vec<u8>) {
    let counter = ENHANCER_COUNTER.fetch_add(1, Ordering::Relaxed);
    let new_name = format!("{super_internal_name}$$SpringCGLIB$$LM{counter:x}");
    let mut cw = ClassWriter::new();

    let this_class_idx = cw.add_class(&new_name);
    let super_class_idx = cw.add_class(super_internal_name);

    let init_name_idx = cw.add_utf8("<init>");
    let void_no_arg_desc_idx = cw.add_utf8("()V");
    let code_attr_name_idx = cw.add_utf8("Code");
    let super_init_methodref = cw.add_methodref(super_class_idx, "<init>", "()V");

    let bf_field_name_idx = cw.add_utf8("$$beanFactory");
    let object_desc_idx = cw.add_utf8("Ljava/lang/Object;");
    let bf_field_ref = cw.add_fieldref(this_class_idx, "$$beanFactory", "Ljava/lang/Object;");

    let beanfactory_cast_idx = cw.add_class("org/springframework/beans/factory/BeanFactory");
    let object_class_idx = cw.add_class("java/lang/Object");
    let getbean_name_ref = cw.add_interface_methodref(
        beanfactory_cast_idx,
        "getBean",
        "(Ljava/lang/String;)Ljava/lang/Object;",
    );
    let getbean_name_args_ref = cw.add_interface_methodref(
        beanfactory_cast_idx,
        "getBean",
        "(Ljava/lang/String;[Ljava/lang/Object;)Ljava/lang/Object;",
    );
    let getbean_type_ref = cw.add_interface_methodref(
        beanfactory_cast_idx,
        "getBean",
        "(Ljava/lang/Class;)Ljava/lang/Object;",
    );
    let getbean_type_args_ref = cw.add_interface_methodref(
        beanfactory_cast_idx,
        "getBean",
        "(Ljava/lang/Class;[Ljava/lang/Object;)Ljava/lang/Object;",
    );
    let integer_cls = cw.add_class("java/lang/Integer");
    let valueof_int = cw.add_methodref(integer_cls, "valueOf", "(I)Ljava/lang/Integer;");
    let long_cls = cw.add_class("java/lang/Long");
    let valueof_long = cw.add_methodref(long_cls, "valueOf", "(J)Ljava/lang/Long;");
    let float_cls = cw.add_class("java/lang/Float");
    let valueof_float = cw.add_methodref(float_cls, "valueOf", "(F)Ljava/lang/Float;");
    let double_cls = cw.add_class("java/lang/Double");
    let valueof_double = cw.add_methodref(double_cls, "valueOf", "(D)Ljava/lang/Double;");
    let bool_cls = cw.add_class("java/lang/Boolean");
    let valueof_bool = cw.add_methodref(bool_cls, "valueOf", "(Z)Ljava/lang/Boolean;");
    let byte_cls = cw.add_class("java/lang/Byte");
    let valueof_byte = cw.add_methodref(byte_cls, "valueOf", "(B)Ljava/lang/Byte;");
    let char_cls = cw.add_class("java/lang/Character");
    let valueof_char = cw.add_methodref(char_cls, "valueOf", "(C)Ljava/lang/Character;");
    let short_cls = cw.add_class("java/lang/Short");
    let valueof_short = cw.add_methodref(short_cls, "valueOf", "(S)Ljava/lang/Short;");
    let ame_class_idx = cw.add_class("java/lang/AbstractMethodError");
    let ame_init_ref = cw.add_methodref(ame_class_idx, "<init>", "()V");

    let class_class_idx = cw.add_class("java/lang/Class");
    let object_getclass_ref = cw.add_methodref(object_class_idx, "getClass", "()Ljava/lang/Class;");
    let class_getsuperclass_ref =
        cw.add_methodref(class_class_idx, "getSuperclass", "()Ljava/lang/Class;");
    let class_getdeclaredmethod_ref = cw.add_methodref(
        class_class_idx,
        "getDeclaredMethod",
        "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
    );
    let resolvabletype_cls = cw.add_class("org/springframework/core/ResolvableType");
    let resolvabletype_formethodreturntype_ref = cw.add_methodref(
        resolvabletype_cls,
        "forMethodReturnType",
        "(Ljava/lang/reflect/Method;)Lorg/springframework/core/ResolvableType;",
    );
    let getbeanprovider_ref = cw.add_interface_methodref(
        beanfactory_cast_idx,
        "getBeanProvider",
        "(Lorg/springframework/core/ResolvableType;)Lorg/springframework/beans/factory/ObjectProvider;",
    );
    let objectprovider_cls = cw.add_class("org/springframework/beans/factory/ObjectProvider");
    let objectprovider_getobject_ref =
        cw.add_interface_methodref(objectprovider_cls, "getObject", "()Ljava/lang/Object;");
    let class_getname_ref = cw.add_methodref(class_class_idx, "getName", "()Ljava/lang/String;");
    let string_cls = cw.add_class("java/lang/String");
    let string_equals_ref = cw.add_methodref(string_cls, "equals", "(Ljava/lang/Object;)Z");
    let nullbean_fqn_string_idx =
        cw.add_string("org.springframework.beans.factory.support.NullBean");

    let cp = LookupCp {
        code_attr_name_idx,
        bf_field_ref,
        beanfactory_cast_idx,
        object_class_idx,
        getbean_name_ref,
        getbean_name_args_ref,
        getbean_type_ref,
        getbean_type_args_ref,
        valueof_int,
        valueof_long,
        valueof_float,
        valueof_double,
        valueof_bool,
        valueof_byte,
        valueof_char,
        valueof_short,
        ame_class_idx,
        ame_init_ref,
        class_class_idx,
        object_getclass_ref,
        class_getsuperclass_ref,
        class_getdeclaredmethod_ref,
        resolvabletype_formethodreturntype_ref,
        getbeanprovider_ref,
        objectprovider_getobject_ref,
        class_getname_ref,
        string_equals_ref,
        nullbean_fqn_string_idx,
    };

    let ctor = emit_default_ctor(
        init_name_idx,
        void_no_arg_desc_idx,
        code_attr_name_idx,
        super_init_methodref,
    );
    let bf_field = emit_bean_factory_field(bf_field_name_idx, object_desc_idx);
    let mut method_bytes: Vec<Vec<u8>> = vec![ctor];

    let b = |x: u16| x.to_be_bytes();
    for m in methods {
        let name_idx = cw.add_utf8(&m.name);
        let desc_idx = cw.add_utf8(&m.descriptor);
        let params = parse_param_descriptors(&m.descriptor);
        let mlocals = 1 + param_slots(&params);

        if !m.is_lookup {
            // throwing stub: new AME; dup; invokespecial <init>; athrow
            let mut code = Vec::new();
            code.push(0xBB); // new
            code.extend_from_slice(&b(cp.ame_class_idx));
            code.push(0x59); // dup
            code.push(0xB7); // invokespecial
            code.extend_from_slice(&b(cp.ame_init_ref));
            code.push(0xBF); // athrow
            method_bytes.push(wrap_method(
                name_idx,
                desc_idx,
                code_attr_name_idx,
                &code,
                2,
                mlocals,
            ));
            continue;
        }

        let rettype_class_idx = cw.add_class(&m.return_internal);
        let has_args = !params.is_empty();
        let mut code: Vec<u8> = Vec::new();

        let emit_arg_array = |code: &mut Vec<u8>| {
            push_int(code, params.len() as i32);
            code.push(0xBD); // anewarray
            code.extend_from_slice(&b(cp.object_class_idx));
            let mut slot = 1u16;
            for (i, p) in params.iter().enumerate() {
                code.push(0x59); // dup
                push_int(code, i as i32);
                emit_load_box(code, &cp, p, slot);
                slot += if p == "J" || p == "D" { 2 } else { 1 };
                code.push(0x53); // aastore
            }
        };

        if let Some(bn) = m.bean_name.as_ref() {
            // ---- BY NAME: (Ret) bf.getBean(name[, args]); NullBean/null → null ----
            let name_string_idx = cw.add_string(bn);
            code.push(0x2A);
            code.push(0xB4);
            code.extend_from_slice(&b(cp.bf_field_ref));
            code.push(0xC0);
            code.extend_from_slice(&b(cp.beanfactory_cast_idx));
            code.push(0x13);
            code.extend_from_slice(&b(name_string_idx));
            if has_args {
                emit_arg_array(&mut code);
                code.push(0xB9);
                code.extend_from_slice(&b(cp.getbean_name_args_ref));
                code.push(0x03);
                code.push(0x00);
            } else {
                code.push(0xB9);
                code.extend_from_slice(&b(cp.getbean_name_ref));
                code.push(0x02);
                code.push(0x00);
            }
            // NullBean/null → null (branch offsets constant relative to suffix start)
            code.push(0x59); // dup
            code.extend_from_slice(&[0xC6, 0x00, 0x17]); // ifnull +23 (0xC6, NOT 0x99=ifeq)
            code.push(0x59); // dup
            code.push(0xB6);
            code.extend_from_slice(&b(cp.object_getclass_ref));
            code.push(0xB6);
            code.extend_from_slice(&b(cp.class_getname_ref));
            code.push(0x13);
            code.extend_from_slice(&b(cp.nullbean_fqn_string_idx));
            code.push(0xB6);
            code.extend_from_slice(&b(cp.string_equals_ref));
            code.extend_from_slice(&[0x9A, 0x00, 0x07]); // ifne +7
            code.push(0xC0);
            code.extend_from_slice(&b(rettype_class_idx));
            code.push(0xB0); // areturn
            code.push(0x57); // L: pop
            code.push(0x01); // aconst_null
            code.push(0xB0); // areturn
        } else if !has_args {
            // ---- BY TYPE, no args: generic-aware getBeanProvider(ResolvableType) ----
            let mname_string_idx = cw.add_string(&m.name);
            code.push(0x2A);
            code.push(0xB4);
            code.extend_from_slice(&b(cp.bf_field_ref));
            code.push(0xC0);
            code.extend_from_slice(&b(cp.beanfactory_cast_idx)); // [bf]
            code.push(0x2A); // aload_0
            code.push(0xB6);
            code.extend_from_slice(&b(cp.object_getclass_ref));
            code.push(0xB6);
            code.extend_from_slice(&b(cp.class_getsuperclass_ref));
            code.push(0x13);
            code.extend_from_slice(&b(mname_string_idx));
            code.push(0x03); // iconst_0
            code.push(0xBD);
            code.extend_from_slice(&b(cp.class_class_idx)); // anewarray Class
            code.push(0xB6);
            code.extend_from_slice(&b(cp.class_getdeclaredmethod_ref));
            code.push(0xB8);
            code.extend_from_slice(&b(cp.resolvabletype_formethodreturntype_ref));
            code.push(0xB9);
            code.extend_from_slice(&b(cp.getbeanprovider_ref));
            code.push(0x02);
            code.push(0x00);
            code.push(0xB9);
            code.extend_from_slice(&b(cp.objectprovider_getobject_ref));
            code.push(0x01);
            code.push(0x00);
            code.push(0xC0);
            code.extend_from_slice(&b(rettype_class_idx));
            code.push(0xB0);
        } else {
            // ---- BY TYPE, with args: (Ret) bf.getBean(Ret.class, args) ----
            code.push(0x2A);
            code.push(0xB4);
            code.extend_from_slice(&b(cp.bf_field_ref));
            code.push(0xC0);
            code.extend_from_slice(&b(cp.beanfactory_cast_idx));
            code.push(0x13);
            code.extend_from_slice(&b(rettype_class_idx));
            emit_arg_array(&mut code);
            code.push(0xB9);
            code.extend_from_slice(&b(cp.getbean_type_args_ref));
            code.push(0x03);
            code.push(0x00);
            code.push(0xC0);
            code.extend_from_slice(&b(rettype_class_idx));
            code.push(0xB0);
        }
        method_bytes.push(wrap_method(
            name_idx,
            desc_idx,
            code_attr_name_idx,
            &code,
            8,
            mlocals,
        ));
    }

    let access_flags: u16 = 0x0001 | 0x0020 | 0x1000; // PUBLIC | SUPER | SYNTHETIC
    let bytes = cw.finish(
        access_flags,
        this_class_idx,
        super_class_idx,
        &[],
        &[bf_field],
        &method_bytes,
    );
    (new_name, bytes)
}

// ===========================================================================
// Method-replacement (`<replaced-method>` / `ReplaceOverride`) subclass.
//
// Spring's `<replaced-method>` declares that a bean method should be replaced
// at runtime by a `MethodReplacer` bean. Real CGLIB generates a subclass whose
// override dispatches into `ReplaceOverrideMethodInterceptor`; that bytecode-
// generation pipeline is incomplete in this VM (see `cce_enhance`'s note), so
// we emit the equivalent subclass directly. For each replaced method we mirror
// `ReplaceOverrideMethodInterceptor.intercept` + `processReturnType`:
//
//   MethodReplacer mr = (MethodReplacer) bf.getBean("<replacer>", MethodReplacer.class);
//   Method m = <Super>.class.getDeclaredMethod("<name>", <paramTypes>);
//   Object r = mr.reimplement(this, m, new Object[]{ <boxed args> });
//   if (<primitive return> && r == null)
//       throw new IllegalStateException(
//           "Null return value from MethodReplacer does not match primitive return type for: " + m);
//   return <unbox/cast r>;
//
// The owning factory is stored into `$$beanFactory` by the instantiation shim
// right after `new_object`, exactly like the lookup-method subclass.
// ===========================================================================

/// One `<replaced-method>` / `ReplaceOverride` to materialise on the subclass.
pub struct ReplaceMethodSpec {
    /// Method name to override.
    pub name: String,
    /// JVM descriptor of the overridden method, e.g. `()Z` or `(Ljava/lang/String;)I`.
    pub descriptor: String,
    /// Bean name of the `MethodReplacer` to dispatch to.
    pub replacer_bean_name: String,
}

/// Per-wrapper constant-pool indices for boxing args / unboxing the replacer
/// result and the primitive `TYPE` field used to reconstruct the `Method`.
struct WrapperRefs {
    class_idx: u16,
    valueof_ref: u16,
    xvalue_ref: u16,
    type_field_ref: u16,
}

/// Build the wrapper machinery for one primitive descriptor char.
fn add_wrapper_refs(cw: &mut ClassWriter, ch: char) -> WrapperRefs {
    let (cls, valueof_desc, xvalue_name, xvalue_desc) = match ch {
        'I' => (
            "java/lang/Integer",
            "(I)Ljava/lang/Integer;",
            "intValue",
            "()I",
        ),
        'J' => ("java/lang/Long", "(J)Ljava/lang/Long;", "longValue", "()J"),
        'F' => (
            "java/lang/Float",
            "(F)Ljava/lang/Float;",
            "floatValue",
            "()F",
        ),
        'D' => (
            "java/lang/Double",
            "(D)Ljava/lang/Double;",
            "doubleValue",
            "()D",
        ),
        'Z' => (
            "java/lang/Boolean",
            "(Z)Ljava/lang/Boolean;",
            "booleanValue",
            "()Z",
        ),
        'B' => ("java/lang/Byte", "(B)Ljava/lang/Byte;", "byteValue", "()B"),
        'C' => (
            "java/lang/Character",
            "(C)Ljava/lang/Character;",
            "charValue",
            "()C",
        ),
        'S' => (
            "java/lang/Short",
            "(S)Ljava/lang/Short;",
            "shortValue",
            "()S",
        ),
        _ => (
            "java/lang/Integer",
            "(I)Ljava/lang/Integer;",
            "intValue",
            "()I",
        ),
    };
    let class_idx = cw.add_class(cls);
    let valueof_ref = cw.add_methodref(class_idx, "valueOf", valueof_desc);
    let xvalue_ref = cw.add_methodref(class_idx, xvalue_name, xvalue_desc);
    let type_field_ref = cw.add_fieldref(class_idx, "TYPE", "Ljava/lang/Class;");
    WrapperRefs {
        class_idx,
        valueof_ref,
        xvalue_ref,
        type_field_ref,
    }
}

/// Build a `<OriginalName>$$SpringCGLIB$$RMn` subclass overriding each replaced
/// method. Returns the chosen internal name and the class-file bytes.
pub fn build_replace_override_subclass(
    super_internal_name: &str,
    methods: &[ReplaceMethodSpec],
) -> (String, Vec<u8>) {
    let counter = ENHANCER_COUNTER.fetch_add(1, Ordering::Relaxed);
    let new_name = format!("{super_internal_name}$$SpringCGLIB$$RM{counter:x}");
    let mut cw = ClassWriter::new();

    let this_class_idx = cw.add_class(&new_name);
    let super_class_idx = cw.add_class(super_internal_name);

    let init_name_idx = cw.add_utf8("<init>");
    let void_no_arg_desc_idx = cw.add_utf8("()V");
    let code_attr_name_idx = cw.add_utf8("Code");
    let super_init_methodref = cw.add_methodref(super_class_idx, "<init>", "()V");

    let bf_field_name_idx = cw.add_utf8("$$beanFactory");
    let object_desc_idx = cw.add_utf8("Ljava/lang/Object;");
    let bf_field_ref = cw.add_fieldref(this_class_idx, "$$beanFactory", "Ljava/lang/Object;");

    // Shared invocation machinery.
    let beanfactory_cast_idx = cw.add_class("org/springframework/beans/factory/BeanFactory");
    let getbean_name_type_ref = cw.add_interface_methodref(
        beanfactory_cast_idx,
        "getBean",
        "(Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/Object;",
    );
    let methodreplacer_cls =
        cw.add_class("org/springframework/beans/factory/support/MethodReplacer");
    let reimplement_ref = cw.add_interface_methodref(
        methodreplacer_cls,
        "reimplement",
        "(Ljava/lang/Object;Ljava/lang/reflect/Method;[Ljava/lang/Object;)Ljava/lang/Object;",
    );
    let class_cls = cw.add_class("java/lang/Class");
    let getdeclaredmethod_ref = cw.add_methodref(
        class_cls,
        "getDeclaredMethod",
        "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
    );
    let object_cls = cw.add_class("java/lang/Object");
    let ise_cls = cw.add_class("java/lang/IllegalStateException");
    let ise_init_ref = cw.add_methodref(ise_cls, "<init>", "(Ljava/lang/String;)V");
    let sb_cls = cw.add_class("java/lang/StringBuilder");
    let sb_init_ref = cw.add_methodref(sb_cls, "<init>", "(Ljava/lang/String;)V");
    let sb_append_obj_ref = cw.add_methodref(
        sb_cls,
        "append",
        "(Ljava/lang/Object;)Ljava/lang/StringBuilder;",
    );
    let sb_tostring_ref = cw.add_methodref(sb_cls, "toString", "()Ljava/lang/String;");
    let ise_prefix_str_idx = cw.add_string(
        "Null return value from MethodReplacer does not match primitive return type for: ",
    );

    // Numeric primitive returns are unboxed via `((Number) result).xValue()` —
    // matching CGLIB's `CodeEmitter.unbox`, which casts to `Number` (NOT the
    // exact wrapper) so an `Integer` returned for a `byte`/`short`/`long`/`float`/
    // `double` method narrows correctly (the test sets `returnValue = 123`, an
    // Integer, for all numeric primitives). `boolean`/`char` cast to their exact
    // wrapper (`Boolean`/`Character`).
    let number_cls = cw.add_class("java/lang/Number");
    let num_byte_value = cw.add_methodref(number_cls, "byteValue", "()B");
    let num_short_value = cw.add_methodref(number_cls, "shortValue", "()S");
    let num_int_value = cw.add_methodref(number_cls, "intValue", "()I");
    let num_long_value = cw.add_methodref(number_cls, "longValue", "()J");
    let num_float_value = cw.add_methodref(number_cls, "floatValue", "()F");
    let num_double_value = cw.add_methodref(number_cls, "doubleValue", "()D");

    // Per-wrapper refs (boxing args / unboxing result / primitive TYPE).
    let wr_i = add_wrapper_refs(&mut cw, 'I');
    let wr_j = add_wrapper_refs(&mut cw, 'J');
    let wr_f = add_wrapper_refs(&mut cw, 'F');
    let wr_d = add_wrapper_refs(&mut cw, 'D');
    let wr_z = add_wrapper_refs(&mut cw, 'Z');
    let wr_b = add_wrapper_refs(&mut cw, 'B');
    let wr_c = add_wrapper_refs(&mut cw, 'C');
    let wr_s = add_wrapper_refs(&mut cw, 'S');
    let wrapper_for = |ch: char| -> &WrapperRefs {
        match ch {
            'I' => &wr_i,
            'J' => &wr_j,
            'F' => &wr_f,
            'D' => &wr_d,
            'Z' => &wr_z,
            'B' => &wr_b,
            'C' => &wr_c,
            'S' => &wr_s,
            _ => &wr_i,
        }
    };

    let ctor = emit_default_ctor(
        init_name_idx,
        void_no_arg_desc_idx,
        code_attr_name_idx,
        super_init_methodref,
    );
    let bf_field = emit_bean_factory_field(bf_field_name_idx, object_desc_idx);
    let mut method_bytes: Vec<Vec<u8>> = vec![ctor];

    let b = |x: u16| x.to_be_bytes();
    for m in methods {
        let name_idx = cw.add_utf8(&m.name);
        let desc_idx = cw.add_utf8(&m.descriptor);
        let name_str_idx = cw.add_string(&m.name);
        let replacer_str_idx = cw.add_string(&m.replacer_bean_name);

        let params = parse_param_descriptors(&m.descriptor);
        let ret = m.descriptor.split(')').nth(1).unwrap_or("V").to_string();
        let ret_char = ret.chars().next().unwrap_or('V');
        let primitive_ret = matches!(ret_char, 'I' | 'J' | 'F' | 'D' | 'Z' | 'B' | 'C' | 'S');

        // Per-param Class constant indices (for getDeclaredMethod's Class[]).
        let param_class_consts: Vec<Option<u16>> = params
            .iter()
            .map(|p| {
                if p.starts_with('L') || p.starts_with('[') {
                    let internal = if p.starts_with('L') && p.ends_with(';') {
                        p[1..p.len() - 1].to_string()
                    } else {
                        p.clone() // array descriptor is a valid CONSTANT_Class name
                    };
                    Some(cw.add_class(&internal))
                } else {
                    None // primitive — use Wrapper.TYPE at emit time
                }
            })
            .collect();
        // Return-type checkcast class index for reference returns.
        let ret_ref_class = if ret.starts_with('L') && ret.ends_with(';') {
            Some(cw.add_class(&ret[1..ret.len() - 1]))
        } else if ret.starts_with('[') {
            Some(cw.add_class(&ret))
        } else {
            None
        };

        let base = 1u16 + param_slots(&params);
        let mr_slot = base as u8;
        let method_slot = (base + 1) as u8;
        let args_slot = (base + 2) as u8;
        let result_slot = (base + 3) as u8;
        let max_locals = base + 4;

        let mut code: Vec<u8> = Vec::new();

        // (A) mr = (MethodReplacer) bf.getBean(replacer, MethodReplacer.class)
        code.push(0x2A); // aload_0
        code.push(0xB4); // getfield $$beanFactory
        code.extend_from_slice(&b(bf_field_ref));
        code.push(0xC0); // checkcast BeanFactory
        code.extend_from_slice(&b(beanfactory_cast_idx));
        code.push(0x13); // ldc_w "<replacer>"
        code.extend_from_slice(&b(replacer_str_idx));
        code.push(0x13); // ldc_w MethodReplacer.class
        code.extend_from_slice(&b(methodreplacer_cls));
        code.push(0xB9); // invokeinterface getBean(String,Class)
        code.extend_from_slice(&b(getbean_name_type_ref));
        code.push(0x03); // count = 3
        code.push(0x00);
        code.push(0xC0); // checkcast MethodReplacer
        code.extend_from_slice(&b(methodreplacer_cls));
        code.push(0x3A); // astore mr_slot
        code.push(mr_slot);

        // (B) method = Super.class.getDeclaredMethod(name, paramClasses)
        code.push(0x13); // ldc_w Super.class
        code.extend_from_slice(&b(super_class_idx));
        code.push(0x13); // ldc_w "<name>"
        code.extend_from_slice(&b(name_str_idx));
        push_int(&mut code, params.len() as i32);
        code.push(0xBD); // anewarray Class
        code.extend_from_slice(&b(class_cls));
        for (i, p) in params.iter().enumerate() {
            code.push(0x59); // dup
            push_int(&mut code, i as i32);
            match param_class_consts[i] {
                Some(cidx) => {
                    code.push(0x13); // ldc_w <Param>.class
                    code.extend_from_slice(&b(cidx));
                }
                None => {
                    // primitive: getstatic Wrapper.TYPE
                    let ch = p.chars().next().unwrap_or('I');
                    code.push(0xB2); // getstatic
                    code.extend_from_slice(&b(wrapper_for(ch).type_field_ref));
                }
            }
            code.push(0x53); // aastore
        }
        code.push(0xB6); // invokevirtual Class.getDeclaredMethod
        code.extend_from_slice(&b(getdeclaredmethod_ref));
        code.push(0x3A); // astore method_slot
        code.push(method_slot);

        // (C) args = new Object[]{ boxed params }
        push_int(&mut code, params.len() as i32);
        code.push(0xBD); // anewarray Object
        code.extend_from_slice(&b(object_cls));
        let mut slot = 1u16;
        for (i, p) in params.iter().enumerate() {
            code.push(0x59); // dup
            push_int(&mut code, i as i32);
            let ch = p.chars().next().unwrap_or('L');
            match ch {
                'J' => {
                    code.push(0x16); // lload
                    code.push(slot as u8);
                    code.push(0xB8); // invokestatic Long.valueOf
                    code.extend_from_slice(&b(wr_j.valueof_ref));
                    slot += 2;
                }
                'F' => {
                    code.push(0x17); // fload
                    code.push(slot as u8);
                    code.push(0xB8);
                    code.extend_from_slice(&b(wr_f.valueof_ref));
                    slot += 1;
                }
                'D' => {
                    code.push(0x18); // dload
                    code.push(slot as u8);
                    code.push(0xB8);
                    code.extend_from_slice(&b(wr_d.valueof_ref));
                    slot += 2;
                }
                'I' | 'Z' | 'B' | 'C' | 'S' => {
                    code.push(0x15); // iload
                    code.push(slot as u8);
                    code.push(0xB8);
                    code.extend_from_slice(&b(wrapper_for(ch).valueof_ref));
                    slot += 1;
                }
                _ => {
                    code.push(0x19); // aload (reference / array)
                    code.push(slot as u8);
                    slot += 1;
                }
            }
            code.push(0x53); // aastore
        }
        code.push(0x3A); // astore args_slot
        code.push(args_slot);

        // (D) result = mr.reimplement(this, method, args)
        code.push(0x19); // aload mr_slot
        code.push(mr_slot);
        code.push(0x2A); // aload_0 (this)
        code.push(0x19); // aload method_slot
        code.push(method_slot);
        code.push(0x19); // aload args_slot
        code.push(args_slot);
        code.push(0xB9); // invokeinterface reimplement(Object,Method,Object[])
        code.extend_from_slice(&b(reimplement_ref));
        code.push(0x04); // count = 4
        code.push(0x00);
        code.push(0x3A); // astore result_slot
        code.push(result_slot);

        // (E) processReturnType + return.
        if primitive_ret {
            // Build the throw block first to size the ifnonnull branch.
            let mut throw_block: Vec<u8> = Vec::new();
            throw_block.push(0xBB); // new IllegalStateException
            throw_block.extend_from_slice(&b(ise_cls));
            throw_block.push(0x59); // dup
            throw_block.push(0xBB); // new StringBuilder
            throw_block.extend_from_slice(&b(sb_cls));
            throw_block.push(0x59); // dup
            throw_block.push(0x13); // ldc_w prefix
            throw_block.extend_from_slice(&b(ise_prefix_str_idx));
            throw_block.push(0xB7); // invokespecial StringBuilder(String)
            throw_block.extend_from_slice(&b(sb_init_ref));
            throw_block.push(0x19); // aload method_slot
            throw_block.push(method_slot);
            throw_block.push(0xB6); // invokevirtual append(Object)
            throw_block.extend_from_slice(&b(sb_append_obj_ref));
            throw_block.push(0xB6); // invokevirtual toString()
            throw_block.extend_from_slice(&b(sb_tostring_ref));
            throw_block.push(0xB7); // invokespecial IllegalStateException(String)
            throw_block.extend_from_slice(&b(ise_init_ref));
            throw_block.push(0xBF); // athrow

            code.push(0x19); // aload result_slot
            code.push(result_slot);
            code.push(0xC7); // ifnonnull → L_ok (skip throw block)
            let off = (3 + throw_block.len()) as u16;
            code.extend_from_slice(&b(off));
            code.extend_from_slice(&throw_block);
            // L_ok: unbox and return. boolean/char cast to their exact wrapper;
            // numeric primitives cast to Number and narrow (CGLIB semantics).
            let (cast_idx, xvalue_ref, ret_op): (u16, u16, u8) = match ret_char {
                'Z' => (wr_z.class_idx, wr_z.xvalue_ref, 0xAC), // Boolean.booleanValue → ireturn
                'C' => (wr_c.class_idx, wr_c.xvalue_ref, 0xAC), // Character.charValue → ireturn
                'B' => (number_cls, num_byte_value, 0xAC),
                'S' => (number_cls, num_short_value, 0xAC),
                'I' => (number_cls, num_int_value, 0xAC),
                'J' => (number_cls, num_long_value, 0xAD), // lreturn
                'F' => (number_cls, num_float_value, 0xAE), // freturn
                'D' => (number_cls, num_double_value, 0xAF), // dreturn
                _ => (number_cls, num_int_value, 0xAC),
            };
            code.push(0x19); // aload result_slot
            code.push(result_slot);
            code.push(0xC0); // checkcast Number / Boolean / Character
            code.extend_from_slice(&b(cast_idx));
            code.push(0xB6); // invokevirtual xValue()
            code.extend_from_slice(&b(xvalue_ref));
            code.push(ret_op);
        } else if ret_char == 'V' {
            // void: ignore the result, just return.
            code.push(0xB1); // return
        } else {
            // reference return: null is permitted; checkcast and areturn.
            code.push(0x19); // aload result_slot
            code.push(result_slot);
            if let Some(rc) = ret_ref_class {
                code.push(0xC0); // checkcast Ret
                code.extend_from_slice(&b(rc));
            }
            code.push(0xB0); // areturn
        }

        method_bytes.push(wrap_method(
            name_idx,
            desc_idx,
            code_attr_name_idx,
            &code,
            8,
            max_locals,
        ));
    }

    let access_flags: u16 = 0x0001 | 0x0020 | 0x1000; // PUBLIC | SUPER | SYNTHETIC
    let bytes = cw.finish(
        access_flags,
        this_class_idx,
        super_class_idx,
        &[],
        &[bf_field],
        &method_bytes,
    );
    (new_name, bytes)
}

// ---------------------------------------------------------------------------
// Native intercept
// ---------------------------------------------------------------------------

/// Coerce a `Value` that may be a raw jlong handle into a proper
/// `Value::Object(ObjectRef)`. JNI / invoke bridges sometimes hand the
/// config Class through as `Value::Long(bits)`; if we forward that to the
/// caller the resulting `astore` / `if_acmpeq` retains an unrooted jlong
/// and Letsgo's verifier flags an access violation.
fn coerce_class_arg(v: Value) -> Value {
    match v {
        Value::Long(bits) => {
            if let Some(p) = cratonvm_types::jlong_bits_as_aligned_object_ptr(bits as u64) {
                Value::Object(Some(unsafe {
                    cratonvm_types::ObjectRef::from_raw(p as *mut u8)
                }))
            } else {
                Value::Object(None)
            }
        }
        other => other,
    }
}

const BEAN_ANNOTATION_DESC: &str = "Lorg/springframework/context/annotation/Bean;";

/// True if the method carries `@Bean` directly, or transitively through a
/// meta-annotation (a composed annotation that is itself annotated `@Bean`,
/// e.g. `@MyProxiedScope = @Bean @Scope(proxyMode=TARGET_CLASS)`). Mirrors
/// Spring's meta-annotation-aware `@Bean` lookup.
fn method_carries_bean(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
    name: &str,
    descriptor: &str,
) -> bool {
    let direct = ctx.method_annotations(class_id, name, descriptor);
    let mut visited = std::collections::HashSet::new();
    annotations_contain_bean(ctx, &direct, &mut visited, 0)
}

/// Recursive worker for [`method_carries_bean`]: returns true if `anns` (or any
/// of their annotation types, transitively) include `@Bean`. A visited-set and
/// a depth cap guard against the cyclic JDK meta-annotations
/// (`@Retention`/`@Target`/`@Documented` reference each other); `java`/`jdk`/
/// `kotlin` annotation packages are skipped outright — they never carry `@Bean`.
fn annotations_contain_bean(
    ctx: &mut dyn NativeContext,
    anns: &[cratonvm_native_api::AnnotationData],
    visited: &mut std::collections::HashSet<String>,
    depth: u32,
) -> bool {
    if anns
        .iter()
        .any(|a| a.type_descriptor == BEAN_ANNOTATION_DESC)
    {
        return true;
    }
    if depth >= 5 {
        return false;
    }
    // Clone the meta-annotation types first so we don't hold a borrow on `anns`
    // across the `&mut ctx` calls below.
    let metas: Vec<String> = anns.iter().map(|a| a.type_descriptor.clone()).collect();
    for desc in metas {
        if !(desc.starts_with('L') && desc.ends_with(';')) {
            continue;
        }
        let internal = &desc[1..desc.len() - 1];
        if internal.starts_with("java/")
            || internal.starts_with("jdk/")
            || internal.starts_with("kotlin/")
        {
            continue;
        }
        if !visited.insert(internal.to_string()) {
            continue;
        }
        if let Some(cid) = ctx.class_id_by_name(internal) {
            let meta_anns = ctx.class_annotations(cid);
            if annotations_contain_bean(ctx, &meta_anns, visited, depth + 1) {
                return true;
            }
        }
    }
    false
}

/// Resolve `internal_name` to a `ClassId`, force-loading it first if it
/// hasn't been touched yet (a `@Bean` method's return type may not be loaded
/// at enhancement time — only its descriptor string has been read so far).
fn resolve_or_load_class_id(
    ctx: &mut dyn NativeContext,
    internal_name: &str,
) -> Option<cratonvm_types::ClassId> {
    if let Some(id) = ctx.class_id_by_name(internal_name) {
        return Some(id);
    }
    let _ = ctx.load_class(internal_name);
    ctx.class_id_by_name(internal_name)
}

/// True when `return_internal` (a `@Bean` method's declared return type) IS
/// `org/springframework/beans/factory/FactoryBean`, or transitively
/// implements/extends it (directly, via a superclass such as
/// `AbstractFactoryBean`, or via an extended interface). Mirrors the check
/// real Spring's `ConfigurationClassEnhancer.BeanMethodInterceptor` uses (via
/// `factoryContainsBean(beanFactory, "&" + beanName)`) to decide whether an
/// inter-`@Bean`-method reference must resolve the raw factory bean rather
/// than its product — see SPR-6602 / SPR-11202 / SPR-15275.
fn is_factory_bean_type(ctx: &mut dyn NativeContext, return_internal: &str) -> bool {
    let factory_bean_id =
        match resolve_or_load_class_id(ctx, "org/springframework/beans/factory/FactoryBean") {
            Some(id) => id,
            None => return false,
        };
    let start = match resolve_or_load_class_id(ctx, return_internal) {
        Some(id) => id,
        None => return false,
    };
    let mut queue = std::collections::VecDeque::new();
    let mut seen = std::collections::HashSet::new();
    queue.push_back(start);
    while let Some(cur) = queue.pop_front() {
        if cur == factory_bean_id {
            return true;
        }
        if !seen.insert(cur) {
            continue;
        }
        for iface in ctx.class_interfaces(cur) {
            queue.push_back(iface);
        }
        if let Some(sup) = ctx.superclass_of(cur) {
            queue.push_back(sup);
        }
    }
    false
}

/// Internal package name (everything before the last `/`, empty for the
/// unnamed/default package) of a loaded class.
fn package_of(ctx: &dyn NativeContext, class_id: cratonvm_types::ClassId) -> String {
    let name = ctx.class_name_of_id(class_id).unwrap_or_default();
    match name.rfind('/') {
        Some(idx) => name[..idx].to_string(),
        None => String::new(),
    }
}

/// Walks `class_id` and its superclass chain (stopping at `Object`) so
/// `@Bean` methods **inherited** from a base `@Configuration` class are
/// overridden too, not just ones declared directly on the concrete class —
/// SPR-8756's "workaround" case (`protected @Bean` method in a base class in
/// a different package, inherited and called via a public wrapper).
/// Package-private (default-access) methods are only eligible when declared
/// in the SAME package as `class_id` itself: per JLS §8.4.8.1 a
/// package-private method is not inherited/overridable by a subclass in a
/// different package, so CGLIB has nothing legitimate to override there —
/// the call runs the real body directly (SPR-8756's "repro" case). Once a
/// (name, descriptor) signature has been considered at one level it is never
/// reconsidered further up the chain (the most-derived declaration wins, or
/// — for an ineligible package-private one — Java simply can't see the
/// ancestor's member at all).
fn scan_bean_methods(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
) -> Vec<BeanMethod> {
    const ACC_PUBLIC: u16 = 0x0001;
    const ACC_PRIVATE: u16 = 0x0002;
    const ACC_PROTECTED: u16 = 0x0004;
    const ACC_STATIC: u16 = 0x0008;
    const ACC_FINAL: u16 = 0x0010;
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let concrete_package = package_of(ctx, class_id);

    let mut cur = Some(class_id);
    while let Some(cid) = cur {
        for m in ctx.declared_methods(cid) {
            if m.name.starts_with('<') {
                continue;
            }
            if m.access_flags & (ACC_STATIC | ACC_PRIVATE | ACC_FINAL) != 0 {
                continue;
            }
            // No-arg only.
            if !m.descriptor.starts_with("()") {
                continue;
            }
            // Reference return only (Lxxx;) — parse the return-type internal name.
            let ret = match m.descriptor.split(')').nth(1) {
                Some(r) => r,
                None => continue,
            };
            if !(ret.starts_with('L') && ret.ends_with(';')) {
                continue;
            }
            // Guard against duplicate (name, descriptor) across the whole
            // chain — a more-derived class's declaration (or non-eligibility
            // decision) shadows any ancestor's.
            let key = (m.name.clone(), m.descriptor.clone());
            if seen.contains(&key) {
                continue;
            }
            let is_package_private =
                m.access_flags & (ACC_PUBLIC | ACC_PROTECTED | ACC_PRIVATE) == 0;
            if is_package_private && cid != class_id && package_of(ctx, cid) != concrete_package {
                seen.insert(key);
                continue;
            }
            // Must carry @Bean — directly OR via a meta-annotation (a composed
            // annotation such as `@MyProxiedScope` that is itself meta-annotated
            // `@Bean`). Spring's `@Bean` detection is meta-annotation aware, so the
            // enhancer must intercept those methods too; otherwise an inter-bean
            // reference to such a method runs the raw body and bypasses the
            // container (e.g. the scoped-proxy is never substituted).
            if !method_carries_bean(ctx, cid, &m.name, &m.descriptor) {
                seen.insert(key);
                continue;
            }
            seen.insert(key);
            let return_internal = ret[1..ret.len() - 1].to_string();
            let is_factory_bean = is_factory_bean_type(ctx, &return_internal);
            out.push(BeanMethod {
                name: m.name.clone(),
                descriptor: m.descriptor.clone(),
                return_internal,
                is_factory_bean,
            });
        }
        cur = ctx.superclass_of(cid);
    }
    out
}

/// `ConfigurationClassEnhancer.enhance(Class<?>, ClassLoader) → Class<?>`
/// native intercept. Replaces the previous "return original" bypass with a
/// real subclass produced by the minimal emitter above.
/// Mirror real CGLIB's `ReflectUtils.defineClass` hook: invoke the
/// currently-installed `ReflectUtils.generatedClassHandler` (a
/// `BiConsumer<String, byte[]>`, `org/springframework/cglib/core/ReflectUtils`)
/// with the new class's dotted name and raw class-file bytes, exactly like
/// real cglib does right before it defines the class.
///
/// Spring AOT processing installs this hook for the DURATION of
/// `processAheadOfTime` (`ApplicationContextAotGenerator
/// .withCglibClassHandler` -> `ReflectUtils.setGeneratedClassHandler
/// (CglibClassHandler::handleGeneratedClass)`) specifically to capture
/// generated CGLIB proxy bytecode into `GenerationContext.getGeneratedFiles()`
/// -- both so tests can assert on it directly
/// (`ApplicationContextAotGeneratorTests.isRegisteredCglibClass`) and, more
/// importantly, so the LATER `TestCompiler` compile step (and real runtime
/// AOT-generated source) can resolve the proxy class by name: the generated
/// bean-registration source references the proxy class directly (e.g.
/// `new Foo$$SpringCGLIB$$0(...)`), and without its .class bytes on the
/// compile classpath that reference fails with "cannot find symbol".
///
/// This native reimplementation bypasses the real `Enhancer`/`ReflectUtils`
/// bytecode-generation path entirely (see the module doc comment), so
/// without this explicit call the hook installed above never fires. No-op
/// (silently) if `ReflectUtils` isn't loaded or no handler is currently
/// installed -- both legitimate outside of AOT processing.
fn notify_generated_class_handler(
    ctx: &mut dyn NativeContext,
    loader_id: u32,
    new_name: &str,
    bytes: &[u8],
) {
    let Some(reflect_utils_cid) = ctx
        .class_id_by_name_and_loader("org/springframework/cglib/core/ReflectUtils", loader_id)
        .or_else(|| ctx.class_id_by_name("org/springframework/cglib/core/ReflectUtils"))
    else {
        return;
    };
    let Some(field_idx) =
        ctx.static_field_index_by_name(reflect_utils_cid, "generatedClassHandler")
    else {
        return;
    };
    let Value::Object(Some(handler)) = ctx.get_static_field(reflect_utils_cid, field_idx) else {
        return;
    };
    let dotted_name = new_name.replace('/', ".");
    let name_str = ctx.create_string(&dotted_name);
    let byte_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
    ctx.write_byte_array_from(byte_arr, 0, bytes);
    let _ = ctx.invoke_virtual(
        handler,
        "accept",
        "(Ljava/lang/Object;Ljava/lang/Object;)V",
        &[Value::Object(Some(name_str)), Value::Object(Some(byte_arr))],
    );
}

fn cce_enhance(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = receiver, args[1] = config Class, args[2] = ClassLoader.
    //
    // The receiver's OWN loader (not the config class's) is what scopes
    // ReflectUtils.generatedClassHandler correctly for
    // notify_generated_class_handler: under @CompileWithForkedClassLoader,
    // each test method gets a fresh child loader for INFRASTRUCTURE classes
    // (ConfigurationClassEnhancer, ReflectUtils, and this test's specific
    // installed handler), while the config class being enhanced is often
    // loaded by a SHARED/parent loader reused across many test methods.
    let receiver_loader_id = match args.first() {
        Some(Value::Object(Some(recv))) => {
            let recv_cid = ctx.class_id_of_object(*recv);
            ctx.loader_id_of_class(recv_cid) as u32
        }
        _ => 0,
    };
    let cls_val = match args.get(1).cloned() {
        Some(v) => coerce_class_arg(v),
        None => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "ConfigurationClassEnhancer.enhance: missing class arg".into(),
            }
            .into());
        }
    };

    // Resolve the input Class<?> mirror and its internal name.
    let cls_mirror = match cls_val {
        Value::Object(Some(m)) => m,
        _ => {
            eprintln!("[CCE] enhance: class arg was not an object — falling back to identity",);
            return Ok(Some(cls_val));
        }
    };

    let super_class_id = match crate::lang_class::mirror_class_id(ctx, cls_mirror) {
        Some(cid) => cid,
        None => {
            return Ok(Some(Value::Object(Some(cls_mirror))));
        }
    };

    // Real CGLIB caches the generated proxy class per superclass and
    // returns the SAME `Class` on a repeat `enhance()` call instead of
    // generating a fresh numbered subclass every time — see
    // `config_enhancer_class_cache`'s doc comment.
    let cached = config_enhancer_class_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&super_class_id.as_u32())
        .cloned();
    if let Some((cached_id, cached_name, cached_bytes)) = cached {
        notify_generated_class_handler(ctx, receiver_loader_id, &cached_name, &cached_bytes);
        let mirror = ctx.get_class_mirror(cached_id);
        return Ok(Some(Value::Object(Some(mirror))));
    }

    let super_name = match ctx.class_name_of_id(super_class_id) {
        Some(n) => n,
        None => {
            return Ok(Some(Value::Object(Some(cls_mirror))));
        }
    };

    // Scan the @Configuration class for @Bean methods to intercept, then build
    // the subclass bytes. Loader id 0 = application loader (same as every other
    // defineClass entry point in this codebase).
    let bean_methods = scan_bean_methods(ctx, super_class_id);
    // Real CGLIB emits one delegating constructor per non-private
    // superclass constructor -- see `emit_ctor_for_descriptor`'s doc
    // comment. Constructors are never inherited, so only `super_class_id`'s
    // OWN declared ones matter (no superclass-chain walk, unlike
    // `scan_bean_methods`).
    const ACC_PRIVATE: u16 = 0x0002;
    let ctor_descriptors: Vec<String> = ctx
        .declared_methods(super_class_id)
        .into_iter()
        .filter(|m| m.name == "<init>" && m.access_flags & ACC_PRIVATE == 0)
        .map(|m| m.descriptor)
        .collect();
    let (new_name, bytes) = build_enhancer_class(&super_name, &bean_methods, &ctor_descriptors);

    let opts = DefineClassFull {
        override_name: Some(new_name.clone()),
        skip_verification: true,
        ..Default::default()
    };

    let bytes_arc = std::sync::Arc::new(bytes);
    match ctx.define_class_full(&new_name, &bytes_arc, 0, opts) {
        Ok(cid) => {
            config_enhancer_class_cache()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(super_class_id.as_u32(), (cid, new_name.clone(), bytes_arc.clone()));
            let mirror = ctx.get_class_mirror(cid);
            eprintln!(
                "[CCE] enhance: defined {new_name} (super={super_name}, marker={SPRING_MARKER_IFACE}, intercepted @Bean methods={})",
                bean_methods.len(),
            );
            notify_generated_class_handler(ctx, receiver_loader_id, &new_name, &bytes_arc);
            Ok(Some(Value::Object(Some(mirror))))
        }
        Err(msg) => {
            eprintln!(
                "[CCE] enhance: define_class_full failed for {new_name}: {msg} — fallback to identity",
            );
            Ok(Some(Value::Object(Some(cls_mirror))))
        }
    }
}

/// Register the CGLIB / Spring `ConfigurationClassEnhancer.enhance` intercept.
/// Must be called AFTER `net_phase_e::register_phase_e_networking` so the
/// emitter implementation here wins over the older identity-bypass
/// registration. Registry semantics: re-register under the same triple
/// silently overwrites (`native-api/src/registry.rs:1336-1338`).
pub fn register_cglib_enhancer(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(
        "org/springframework/context/annotation/ConfigurationClassEnhancer",
        "enhance",
        "(Ljava/lang/Class;Ljava/lang/ClassLoader;)Ljava/lang/Class;",
        cce_enhance,
    );
    registry.set_category(__prev_cat);
}
