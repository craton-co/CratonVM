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
use cratonvm_types::{ObjectRef, Value};

/// Monotonic counter for generating unique enhancer subclass names. Mirrors
/// CGLIB's own `KeyFactory.generateName` counter.
static ENHANCER_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Per-(defining-classloader, superclass-name) suffix counters for
/// `build_enhancer_class`'s `$$SpringCGLIB$$<n>` names -- Springs own
/// `SpringNamingPolicy` keys its counter by the base class name, so the
/// first proxy generated for any given `@Configuration` class always gets
/// suffix `0`, no matter how many *other* classes were enhanced earlier in
/// the same JVM/session. Using the single global `ENHANCER_COUNTER` here
/// (shared with the unrelated `$$SpringCGLIB$$LM*`/`$$SpringCGLIB$$RM*`
/// lookup/replace-method enhancers) made every class's suffix depend on
/// unrelated enhancement activity elsewhere in the same test run, breaking
/// `AnnotationConfigApplicationContextTests.refreshForAotRegisterHintsForCglibProxy`,
/// which hardcodes the literal expected name `...$$SpringCGLIB$$0` for the
/// first (and only) proxy of its `CglibConfiguration` class.
///
/// Keying by class name ALONE is still wrong for suites like
/// `ApplicationContextAotGeneratorTests`, where several `@Test` methods
/// each enhance their OWN fixture class that happens to share the simple
/// name `CglibConfiguration` (`@CompileWithForkedClassLoader` gives each
/// test method a fresh child loader, so these are genuinely distinct
/// `ClassId`s / distinct `Class` tokens, not repeat enhancements of one
/// class) — under the pure name-keyed counter, the second and third such
/// test methods observed in the SAME `KRun` batch inherited the first
/// method's already-incremented counter and got suffix `1`/`2` instead of
/// the `0` every one of them independently expects. Real CGLIB's own
/// generated-name/cache state (`AbstractClassGenerator`) lives in a
/// per-`ClassLoader` map, so a fresh loader always starts a fresh count —
/// mirror that here by keying on `(defining_loader_id, super_internal_name)`
/// instead of the name alone.
fn config_enhancer_counters() -> &'static Mutex<HashMap<(u32, String), u64>> {
    static COUNTERS: OnceLock<Mutex<HashMap<(u32, String), u64>>> = OnceLock::new();
    COUNTERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Next `$$SpringCGLIB$$<n>` suffix for `super_internal_name` as loaded by
/// `super_loader_id`, starting at 0 for the first enhancement of any given
/// (loader, class) pair.
fn next_config_enhancer_counter(super_loader_id: u32, super_internal_name: &str) -> u64 {
    let mut counters = config_enhancer_counters()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let counter = counters
        .entry((super_loader_id, super_internal_name.to_string()))
        .or_insert(0);
    let value = *counter;
    *counter += 1;
    value
}

/// Cache of already-enhanced `@Configuration` classes, keyed by
/// `(defining_loader_id, super_internal_name)` — see
/// `config_enhancer_counters`'s doc comment for why a bare `ClassId` is
/// unsafe as a cache key: this VM recycles `ClassId` numbers once their
/// class/loader is garbage-collected, so two UNRELATED classes loaded at
/// different times (e.g. `CglibConfiguration` freshly defined by two
/// different test methods' own short-lived `@CompileWithForkedClassLoader`
/// loaders) can end up with the SAME numeric `ClassId`, silently aliasing
/// this cache onto a stale, already-invalid entry from an earlier test.
/// Confirmed as the actual root cause of two previously-unexplained
/// `ApplicationContextAotGeneratorTests$ConfigurationClassCglibProxy`
/// failures (`processAheadOfTimeUsesCglibClassForFactoryMethod`'s
/// "not an enhanced class", `processAheadOfTimeWhenHasCglibProxyUseProxy`'s
/// "Hello1" double-incremented counter) that both passed 100% reliably in
/// isolation but failed 100% deterministically as part of the full class
/// run — i.e. cross-test contamination via this exact stale-`ClassId`-reuse
/// mechanism, not flakiness. Real CGLIB's own `AbstractClassGenerator`
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

fn config_enhancer_class_cache() -> &'static Mutex<HashMap<(u32, String), CachedEnhancerClass>> {
    static CACHE: OnceLock<Mutex<HashMap<(u32, String), CachedEnhancerClass>>> = OnceLock::new();
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

/// Build a minimal, valid, otherwise-empty class file for one of real
/// CGLIB's two `$$SpringCGLIB$$FastClass$$<n>` reflection-avoidance helper
/// classes (real cglib emits one for the enhancer class itself and one for
/// its own generated `Factory`-dispatch machinery, per enhanced
/// `@Configuration` class). This native reimplementation has no use for
/// FastClass at all — `emit_bean_override`'s inter-bean dispatch is inlined
/// directly into the enhancer bytecode, never indirects through a
/// `Callback`/`FastClass` lookup table — so unlike `build_enhancer_class`
/// this is a pure `extends java/lang/Object` placeholder with a single
/// default constructor and nothing else; nothing ever loads or invokes it.
///
/// Its ONLY job is to exist: `ApplicationContextAotGeneratorTests
/// $ConfigurationClassCglibProxy.processAheadOfTimeWhenHasCglibProxyWriteProxyAndGenerateReflectionHints`
/// asserts both FastClass names are present (byte-for-byte, via
/// `TestGenerationContext.getGeneratedFiles()`) AND have an
/// `INVOKE_DECLARED_CONSTRUCTORS` reflection hint registered — both of
/// which `notify_generated_class_handler`'s target, real Spring's
/// `CglibClassHandler.handleGeneratedClass`, already does unconditionally
/// for ANY name+bytes pair handed to it (`generatedFiles.addFile(...)` +
/// `runtimeHints.reflection().registerType(...)`), so simply calling it for
/// these two placeholder names/bytes is sufficient — no separate hint-
/// registration code needed here.
fn build_fastclass_placeholder(name: &str) -> Vec<u8> {
    let mut cw = ClassWriter::new();
    let this_class_idx = cw.add_class(name);
    let super_class_idx = cw.add_class("java/lang/Object");
    let init_name_idx = cw.add_utf8("<init>");
    let init_desc_idx = cw.add_utf8("()V");
    let code_attr_name_idx = cw.add_utf8("Code");
    let super_init_ref = cw.add_methodref(super_class_idx, "<init>", "()V");
    let ctor = emit_default_ctor(
        init_name_idx,
        init_desc_idx,
        code_attr_name_idx,
        super_init_ref,
    );
    const ACC_PUBLIC: u16 = 0x0001;
    const ACC_SUPER: u16 = 0x0020;
    cw.finish(
        ACC_PUBLIC | ACC_SUPER,
        this_class_idx,
        super_class_idx,
        &[],
        &[],
        &[ctor],
    )
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

    wrap_method(
        init_name_idx,
        desc_idx,
        code_attr_name_idx,
        &code,
        max_stack,
        slot,
    )
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
fn emit_noop_callback_setter(
    cw: &mut ClassWriter,
    name_idx: u16,
    code_attr_name_idx: u16,
) -> Vec<u8> {
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
/// Java's `Class.getSimpleName()` for a nested-class internal name: the
/// portion after the last `$` (or `/` if there's no `$`) — matches what
/// `resolveBeanReference`'s error message uses for `beanMethod
/// .getDeclaringClass().getSimpleName()`, computed at codegen time since the
/// declaring class is always statically known here (avoids a runtime
/// `getClass()` round-trip just to reformat a name we already have).
fn simple_name_of_internal(internal: &str) -> String {
    let after_slash = internal.rsplit('/').next().unwrap_or(internal);
    after_slash
        .rsplit('$')
        .next()
        .unwrap_or(after_slash)
        .to_string()
}

/// Shared constant-pool refs for `emit_bean_override`'s type-mismatch
/// handling (real Spring's `resolveBeanReference`: an inter-bean `getBean()`
/// result that isn't assignable to the declared return type throws a
/// descriptive `IllegalStateException` instead of a bare `ClassCastException`).
/// Computed once per generated class, reused by every `@Bean` override.
struct MismatchRefs {
    stringbuilder_cls: u16,
    sb_init_ref: u16,
    sb_append_string_ref: u16,
    sb_tostring_ref: u16,
    object_equals_ref: u16,
    object_getclass_ref: u16,
    class_getname_ref: u16,
    illegalstate_cls: u16,
    illegalstate_init_ref: u16,
    nsbde_cls: u16,
    get_merged_bd_ref: u16,
    get_resource_desc_ref: u16,
    lit_bean_method_prefix: u16,
    lit_called_as: u16,
    lit_overridden_by: u16,
    lit_close_bracket_dot: u16,
    lit_overriding_bean: u16,
}

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
    // FactoryBean-enhancement (SPR-6602/11202/15275): `Some((methodref,
    // exposed_type_string_idx))` when this method's return type IS (or
    // extends/implements) `FactoryBean` — splices an extra invokestatic
    // call into `cratonvm/internal/ConfigEnhancerSupport
    // .enhanceFactoryBeanReference(Object rawFactory, Object beanFactory,
    // String beanName, String exposedTypeInternalName)Object` right after
    // the raw inter-bean `getBean("&name")` call and before the
    // `checkcast <Ret>`, wrapping the raw factory in a delegating proxy
    // whose `getObject()` resolves the container's cached product instead
    // of computing a fresh one — see `enhance_factory_bean_reference`'s
    // doc comment for the full rationale. `None` for ordinary (non-
    // FactoryBean) `@Bean` methods, producing byte-identical output to
    // before this parameter existed.
    fb_ref: Option<(u16, u16)>,
    // Type-mismatch handling (real Spring's `resolveBeanReference`): when the
    // inter-bean `getBean()` result isn't assignable to the declared return
    // type, throw a descriptive `IllegalStateException` instead of letting a
    // bare `checkcast` raise `ClassCastException`. The two string indices are
    // per-method (this method's own declaring-class simple name + method
    // name, and its declared return type's dotted name — both known at
    // codegen time); `mismatch` bundles the refs shared across every
    // `@Bean` override in the class.
    decl_method_string_idx: u16,
    rettype_dotted_string_idx: u16,
    mismatch: &MismatchRefs,
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
    // 104: [FactoryBean-typed methods only] wrap the raw factory returned by
    // getBean("&name") in a delegating proxy before the checkcast below, so
    // a caller that immediately does `.getObject()` on the result (a direct
    // call, not itself routed through this override) sees the container's
    // cached product rather than a freshly-computed one — see `fb_ref`'s
    // doc comment / `enhance_factory_bean_reference`. Left inside the
    // SPR-8080 try-region deliberately: if the wrapping call throws, the
    // "currently in creation" flag must still be restored by the handler
    // below, exactly like the plain getBean call it replaces.
    //   aload_2 (bf, as Object); aload_3 (beanName); ldc_w <exposedType>;
    //   invokestatic ConfigEnhancerSupport.enhanceFactoryBeanReference
    if let Some((enhance_fb_ref_methodref, exposed_type_string_idx)) = fb_ref {
        code.push(0x2C); // aload_2
        code.push(0x2D); // aload_3
        code.push(0x13); // ldc_w
        code.extend_from_slice(&b(exposed_type_string_idx));
        code.push(0xB8); // invokestatic
        code.extend_from_slice(&b(enhance_fb_ref_methodref));
    }
    // --- Type-mismatch check (real Spring's resolveBeanReference): replaces
    // a bare `checkcast <Ret>` with `instanceof` + a descriptive
    // IllegalStateException on mismatch, treating a `beanInstance
    // .equals(null)` NullBean marker as a plain null (assignable to
    // anything). All of this stays inside the SPR-8080 try region below
    // (same as the checkcast it replaces) so a thrown IllegalStateException
    // still restores "currently in creation" via the outer handler. ---
    // 0: dup
    code.push(0x59);
    // 1: instanceof <Ret>
    code.push(0xC1);
    code.extend_from_slice(&b(rettype_cast_idx));
    // 4: ifne → L_STORE (120); offset 116
    code.push(0x9A);
    code.extend_from_slice(&b(116));
    // 7: dup
    code.push(0x59);
    // 8: aconst_null
    code.push(0x01);
    // 9: invokevirtual Object.equals(Object)Z
    code.push(0xB6);
    code.extend_from_slice(&b(mismatch.object_equals_ref));
    // 12: ifeq → L_THROW (20); offset 8
    code.push(0x99);
    code.extend_from_slice(&b(8));
    // 15: pop   (drop the NullBean marker)
    code.push(0x57);
    // 16: aconst_null
    code.push(0x01);
    // 17: goto → L_STORE (120); offset 103
    code.push(0xA7);
    code.extend_from_slice(&b(103));
    // --- L_THROW (20): genuine mismatch — build and throw the message. ---
    // 20: astore 7   (local7 = mismatchInstance; safe here — only reused as
    //     scratch in this normal-flow region; the SPR-8080 handler's own
    //     `astore 7` only executes on an actual exception, at which point
    //     this value is irrelevant.)
    code.push(0x3A);
    code.push(0x07);
    // 22: new StringBuilder
    code.push(0xBB);
    code.extend_from_slice(&b(mismatch.stringbuilder_cls));
    // 25: dup
    code.push(0x59);
    // 26: invokespecial StringBuilder.<init>()V
    code.push(0xB7);
    code.extend_from_slice(&b(mismatch.sb_init_ref));
    // 29: ldc_w "@Bean method "
    code.push(0x13);
    code.extend_from_slice(&b(mismatch.lit_bean_method_prefix));
    // 32: invokevirtual StringBuilder.append(String)StringBuilder
    code.push(0xB6);
    code.extend_from_slice(&b(mismatch.sb_append_string_ref));
    // 35: ldc_w "<DeclaringSimpleName>.<methodName>"
    code.push(0x13);
    code.extend_from_slice(&b(decl_method_string_idx));
    // 38: invokevirtual append
    code.push(0xB6);
    code.extend_from_slice(&b(mismatch.sb_append_string_ref));
    // 41: ldc_w " called as bean reference for type ["
    code.push(0x13);
    code.extend_from_slice(&b(mismatch.lit_called_as));
    // 44: invokevirtual append
    code.push(0xB6);
    code.extend_from_slice(&b(mismatch.sb_append_string_ref));
    // 47: ldc_w "<declared return type, dotted>"
    code.push(0x13);
    code.extend_from_slice(&b(rettype_dotted_string_idx));
    // 50: invokevirtual append
    code.push(0xB6);
    code.extend_from_slice(&b(mismatch.sb_append_string_ref));
    // 53: ldc_w "] but overridden by non-compatible bean instance of type ["
    code.push(0x13);
    code.extend_from_slice(&b(mismatch.lit_overridden_by));
    // 56: invokevirtual append
    code.push(0xB6);
    code.extend_from_slice(&b(mismatch.sb_append_string_ref));
    // 59: aload 7   (mismatchInstance)
    code.push(0x19);
    code.push(0x07);
    // 61: invokevirtual Object.getClass()Class
    code.push(0xB6);
    code.extend_from_slice(&b(mismatch.object_getclass_ref));
    // 64: invokevirtual Class.getName()String
    code.push(0xB6);
    code.extend_from_slice(&b(mismatch.class_getname_ref));
    // 67: invokevirtual append
    code.push(0xB6);
    code.extend_from_slice(&b(mismatch.sb_append_string_ref));
    // 70: ldc_w "]."
    code.push(0x13);
    code.extend_from_slice(&b(mismatch.lit_close_bracket_dot));
    // 73: invokevirtual append
    code.push(0xB6);
    code.extend_from_slice(&b(mismatch.sb_append_string_ref));
    // 76: dup   (save a copy of the StringBuilder ref into local7 — survives
    //     an exception from the nested try2 below, where the operand stack
    //     is cleared)
    code.push(0x59);
    // 77: astore 7
    code.push(0x3A);
    code.push(0x07);
    // --- TRY2_START (79): optional " Overriding bean of same name declared
    // in: <resource>" suffix — mirrors resolveBeanReference's own
    // try/catch(NoSuchBeanDefinitionException), silently skipped on failure. ---
    // 79: ldc_w " Overriding bean of same name declared in: "
    code.push(0x13);
    code.extend_from_slice(&b(mismatch.lit_overriding_bean));
    // 82: invokevirtual append
    code.push(0xB6);
    code.extend_from_slice(&b(mismatch.sb_append_string_ref));
    // 85: aload 6   (cbf)
    code.push(0x19);
    code.push(0x06);
    // 87: aload_3   (beanName)
    code.push(0x2D);
    // 88: invokeinterface ConfigurableBeanFactory.getMergedBeanDefinition(String)BeanDefinition  count=2
    code.push(0xB9);
    code.extend_from_slice(&b(mismatch.get_merged_bd_ref));
    code.push(0x02);
    code.push(0x00);
    // 93: invokeinterface BeanDefinition.getResourceDescription()String  count=1
    code.push(0xB9);
    code.extend_from_slice(&b(mismatch.get_resource_desc_ref));
    code.push(0x01);
    code.push(0x00);
    // 98: invokevirtual append
    code.push(0xB6);
    code.extend_from_slice(&b(mismatch.sb_append_string_ref));
    // --- TRY2_END (101, exclusive) ---
    // 101: goto → L_BUILD_DONE (105); offset 4
    code.push(0xA7);
    code.extend_from_slice(&b(4));
    // --- CATCH2 (104): NoSuchBeanDefinitionException — discard, keep the
    // pre-suffix StringBuilder (stack cleared by the JVM on exception entry,
    // so reload the saved copy from local7). ---
    // 104: pop
    code.push(0x57);
    // --- L_BUILD_DONE (105) ---
    // 105: invokevirtual StringBuilder.toString()String
    code.push(0xB6);
    code.extend_from_slice(&b(mismatch.sb_tostring_ref));
    // 108: astore 7   (local7 = msg)
    code.push(0x3A);
    code.push(0x07);
    // 110: new IllegalStateException
    code.push(0xBB);
    code.extend_from_slice(&b(mismatch.illegalstate_cls));
    // 113: dup
    code.push(0x59);
    // 114: aload 7   (msg)
    code.push(0x19);
    code.push(0x07);
    // 116: invokespecial IllegalStateException.<init>(String)V
    code.push(0xB7);
    code.extend_from_slice(&b(mismatch.illegalstate_init_ref));
    // 119: athrow
    code.push(0xBF);
    // --- L_STORE (120): shared by both the instanceof-true path and the
    // NullBean→null path (checkcast on a null reference is always legal). ---
    // 120: checkcast <Ret>
    code.push(0xC0);
    code.extend_from_slice(&b(rettype_cast_idx));
    // 123: astore 4   (local4 = result)
    code.push(0x3A);
    code.push(0x04);
    // --- TRY_END (exclusive). Normal path continues below: mirrors
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
    // Every branch offset in this method is RELATIVE and has both its
    // instruction and its target strictly before or strictly after the
    // `fb_ref` insertion point (pc 104) — so the 8-byte splice never needs
    // any of them recomputed, only the three ABSOLUTE exception-table
    // values below.
    let fb_ref_extra_bytes: u16 = if fb_ref.is_some() { 8 } else { 0 };
    // The mismatch-handling block (125 bytes) replaced a 5-byte
    // checkcast+astore4, so everything from the old try_end/handler_pc
    // onward shifted forward by 120 bytes — see that block's own comment for
    // why every OTHER branch offset in the method needed no changes (it's a
    // self-contained splice whose only exit is the same fallthrough point
    // the old code had, `checkcast <Ret>; astore 4`).
    let mismatch_block_growth: u16 = 120;
    debug_assert_eq!(
        code.len(),
        161 + fb_ref_extra_bytes as usize + mismatch_block_growth as usize
    );

    let try_start: u16 = 94;
    let try_end: u16 = 109 + fb_ref_extra_bytes + mismatch_block_growth;
    let handler_pc: u16 = 142 + fb_ref_extra_bytes + mismatch_block_growth;
    // Inner try2/catch2 — NoSuchBeanDefinitionException around the
    // getMergedBeanDefinition()/getResourceDescription() suffix, positioned
    // at the FIXED offsets 79/101/104 relative to the mismatch block's own
    // start (104, or 112 with `fb_ref`), unaffected by fb_ref_extra_bytes
    // since the block's internal layout is identical either way.
    let mismatch_block_start: u16 = 104 + fb_ref_extra_bytes;
    let try2_start: u16 = mismatch_block_start + 79;
    let try2_end: u16 = mismatch_block_start + 101;
    let handler_pc2: u16 = mismatch_block_start + 104;

    let mut code_attr = Vec::new();
    // max_stack: the `fb_ref` splice pushes bf/beanName/exposedType (3
    // items) on top of the raw factory already on the stack from
    // `getBean`, briefly reaching a 4-deep stack before `invokestatic`
    // consumes them — one more than the 3-deep
    // setCurrentlyInCreation/registerDependentBean calls that otherwise
    // dominate. The mismatch-handling block's own peak (dup/instanceof,
    // Object.equals, and the cbf/beanName pair feeding
    // getMergedBeanDefinition) stays at or below 3, so it never raises this.
    let max_stack: u16 = if fb_ref.is_some() { 4 } else { 3 };
    code_attr.extend_from_slice(&max_stack.to_be_bytes());
    code_attr.extend_from_slice(&8u16.to_be_bytes()); // max_locals (this, method, bf, beanName, result, alreadyInCreation, cbf, throwable)
    code_attr.extend_from_slice(&(code.len() as u32).to_be_bytes());
    code_attr.extend_from_slice(&code);
    code_attr.extend_from_slice(&2u16.to_be_bytes()); // exception_table_length
                                                      // Entry 1 (checked first): inner NoSuchBeanDefinitionException handler.
    code_attr.extend_from_slice(&try2_start.to_be_bytes());
    code_attr.extend_from_slice(&try2_end.to_be_bytes());
    code_attr.extend_from_slice(&handler_pc2.to_be_bytes());
    code_attr.extend_from_slice(&mismatch.nsbde_cls.to_be_bytes());
    // Entry 2: outer SPR-8080 any-Throwable finally handler.
    code_attr.extend_from_slice(&try_start.to_be_bytes());
    code_attr.extend_from_slice(&try_end.to_be_bytes());
    code_attr.extend_from_slice(&handler_pc.to_be_bytes());
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

/// Emit a `public static Object CGLIB$FACTORY_DATA;` field (no
/// `ConstantValue` needed — reference-typed static fields default to
/// `null` at class initialization).
///
/// Real cglib's `Enhancer.wrapCachedClass` — invoked from
/// `AbstractClassGenerator.create()`, on EVERY class it hands back,
/// fresh or not — reads and writes this exact field via reflection
/// (`klass.getField("CGLIB$FACTORY_DATA")`) to attach its own
/// `EnhancerFactoryData` bookkeeping. This reimplementation never uses
/// that bookkeeping itself, but Spring's `CglibAopProxy` (for
/// `proxyTargetClass=true` general AOP proxying) treats ANY class whose
/// name contains `"$$"` as "already a CGLIB proxy" (`ClassUtils
/// .isCglibProxyClass`) and, when asked to further proxy one of OUR
/// generated `@Configuration` enhancer classes, calls `getSuperclass()`
/// to find the "real" target and re-enhances THAT with real cglib —
/// computing the EXACT SAME `<Original>$$SpringCGLIB$$0` name we
/// already used, purely by coincidence of both sides independently
/// implementing Spring's own naming convention for "first proxy of this
/// class". Real cglib's own naming has no way to know our class was
/// never registered through ITS bookkeeping (we bypass cglib's Java
/// machinery entirely), so it doesn't detect the clash and tries to
/// `defineClass` under our already-taken name; this VM's classloader
/// correctly rejects that as already-defined (matching what a genuine
/// same-name race would do on any JVM), and cglib's own defineClass
/// failure-recovery path then loads OUR existing class as if it were
/// its own freshly-generated one — at which point `wrapCachedClass`
/// needs this field to exist, or it throws `NoSuchFieldException:
/// CGLIB$FACTORY_DATA` wrapped in `AopConfigException: Could not
/// generate CGLIB subclass... final class or non-visible class`
/// (`ConfigurationClassPostProcessorTests.genericsBasedInjectionWith*`).
///
/// `CGLIB$FACTORY_DATA` alone was not sufficient: once it stopped
/// throwing, the SAME real-cglib recovery path went on to read/write
/// further bookkeeping fields real cglib-generated classes always carry —
/// `CGLIB$CALLBACK_FILTER` (confirmed empirically to be the very next one
/// `NoSuchFieldException`'d), and by the same reasoning likely
/// `CGLIB$THREAD_CALLBACKS`/`CGLIB$STATIC_CALLBACKS`/`CGLIB$BOUND` too
/// (`Enhancer.setThreadCallbacks`/`setCallbacks`/`isEnhanced`'s usual
/// reflective targets) — see `build_enhancer_class`'s call site for the
/// full field list this reimplementation now proactively emits.
fn emit_public_static_field(name_idx: u16, descriptor_idx: u16) -> Vec<u8> {
    let mut field = Vec::new();
    field.extend_from_slice(&(0x0001u16 | 0x0008u16).to_be_bytes()); // ACC_PUBLIC | ACC_STATIC
    field.extend_from_slice(&name_idx.to_be_bytes());
    field.extend_from_slice(&descriptor_idx.to_be_bytes());
    field.extend_from_slice(&0u16.to_be_bytes()); // attributes_count = 0
    field
}

/// Emit harmless no-op/null-returning stub bodies for all seven
/// `org/springframework/cglib/proxy/Factory` interface methods.
///
/// Confirmed empirically (`ConfigurationClassPostProcessorTests
/// .genericsBasedInjectionWith*`): once the `CGLIB$*` bookkeeping fields
/// stopped `NoSuchFieldException`ing, the SAME real-cglib
/// defineClass-failure-recovery path (see `emit_public_static_field`'s
/// doc comment) went on to `(Factory) klass_instance` our loaded-as-if-
/// its-own class — `ClassCastException` since we never declared this
/// marker interface. Real cglib-generated classes ALWAYS implement it
/// (with real callback-array-backed bodies); this reimplementation has no
/// callback array to back them with (the `@Bean` overrides dispatch
/// directly, see the module doc comment), so every method is a safe
/// null-returning/no-op stub — nothing in this VM's own dispatch ever
/// calls them, they exist purely so an incidental `instanceof`/cast from
/// OTHER real cglib machinery that mistakes this class for its own
/// doesn't blow up.
fn emit_cglib_factory_interface_methods(
    cw: &mut ClassWriter,
    code_attr_name_idx: u16,
) -> Vec<Vec<u8>> {
    let callback_desc = "Lorg/springframework/cglib/proxy/Callback;";
    let callback_arr_desc = "[Lorg/springframework/cglib/proxy/Callback;";
    let object_desc = "Ljava/lang/Object;";

    let mut methods = Vec::new();

    // Object newInstance(Callback)
    {
        let name_idx = cw.add_utf8("newInstance");
        let desc_idx = cw.add_utf8(&format!("({callback_desc}){object_desc}"));
        methods.push(wrap_method(
            name_idx,
            desc_idx,
            code_attr_name_idx,
            &[0x01, 0xB0],
            1,
            2,
        ));
    }
    // Object newInstance(Callback[])
    {
        let name_idx = cw.add_utf8("newInstance");
        let desc_idx = cw.add_utf8(&format!("({callback_arr_desc}){object_desc}"));
        methods.push(wrap_method(
            name_idx,
            desc_idx,
            code_attr_name_idx,
            &[0x01, 0xB0],
            1,
            2,
        ));
    }
    // Object newInstance(Class[], Object[], Callback[])
    {
        let name_idx = cw.add_utf8("newInstance");
        let desc_idx = cw.add_utf8(&format!(
            "([Ljava/lang/Class;[Ljava/lang/Object;{callback_arr_desc}){object_desc}"
        ));
        methods.push(wrap_method(
            name_idx,
            desc_idx,
            code_attr_name_idx,
            &[0x01, 0xB0],
            1,
            4,
        ));
    }
    // Callback getCallback(int)
    {
        let name_idx = cw.add_utf8("getCallback");
        let desc_idx = cw.add_utf8(&format!("(I){callback_desc}"));
        methods.push(wrap_method(
            name_idx,
            desc_idx,
            code_attr_name_idx,
            &[0x01, 0xB0],
            1,
            2,
        ));
    }
    // void setCallback(int, Callback)
    {
        let name_idx = cw.add_utf8("setCallback");
        let desc_idx = cw.add_utf8(&format!("(I{callback_desc})V"));
        methods.push(wrap_method(
            name_idx,
            desc_idx,
            code_attr_name_idx,
            &[0xB1],
            0,
            3,
        ));
    }
    // Callback[] getCallbacks()
    {
        let name_idx = cw.add_utf8("getCallbacks");
        let desc_idx = cw.add_utf8(&format!("(){callback_arr_desc}"));
        methods.push(wrap_method(
            name_idx,
            desc_idx,
            code_attr_name_idx,
            &[0x01, 0xB0],
            1,
            1,
        ));
    }
    // void setCallbacks(Callback[])
    {
        let name_idx = cw.add_utf8("setCallbacks");
        let desc_idx = cw.add_utf8(&format!("({callback_arr_desc})V"));
        methods.push(wrap_method(
            name_idx,
            desc_idx,
            code_attr_name_idx,
            &[0xB1],
            0,
            2,
        ));
    }

    methods
}

/// Generate a fresh `<OriginalName>$$SpringCGLIB$$<counter>` class
/// file as a `Vec<u8>` plus the chosen internal-name. The new class
/// extends `super_internal_name` and implements
/// [`SPRING_MARKER_IFACE`].
/// Minimal label-based bytecode assembler: computes JVM branch/goto offsets
/// automatically instead of requiring them hand-derived. `emit_bean_override`
/// (the no-arg case) hand-derives its offsets because it's a small, stable,
/// heavily-commented method; `emit_bean_override_with_args` below has
/// substantially more branches (the `useArgs` decision), where hand-derived
/// offsets become error-prone to get right and to keep right under edits.
struct Asm {
    code: Vec<u8>,
    fixups: Vec<(usize, String)>,
    labels: std::collections::HashMap<String, u16>,
}

impl Asm {
    fn new() -> Self {
        Asm {
            code: Vec::new(),
            fixups: Vec::new(),
            labels: std::collections::HashMap::new(),
        }
    }
    fn pos(&self) -> u16 {
        self.code.len() as u16
    }
    fn mark(&mut self, label: &str) {
        let p = self.pos();
        self.labels.insert(label.to_string(), p);
    }
    fn u8_(&mut self, v: u8) {
        self.code.push(v);
    }
    fn u16be(&mut self, v: u16) {
        self.code.extend_from_slice(&v.to_be_bytes());
    }
    /// Emit a branch opcode (`ifnull`/`ifeq`/`ifne`/`goto`/...) with a
    /// placeholder 2-byte offset, resolved against `label` at `finish()`.
    fn branch(&mut self, opcode: u8, label: &str) {
        self.code.push(opcode);
        let at = self.code.len();
        self.code.extend_from_slice(&[0, 0]);
        self.fixups.push((at, label.to_string()));
    }
    fn finish(mut self) -> Vec<u8> {
        for (at, label) in &self.fixups {
            let opcode_addr = *at as i32 - 1;
            let target =
                *self.labels.get(label).unwrap_or_else(|| {
                    panic!("emit_bean_override_with_args: unresolved label {label}")
                }) as i32;
            let offset = (target - opcode_addr) as i16;
            let bytes = offset.to_be_bytes();
            self.code[*at] = bytes[0];
            self.code[*at + 1] = bytes[1];
        }
        self.code
    }
}

/// `Integer`/`Long`/.../`Boolean.valueOf` methodrefs for boxing primitive
/// `@Bean`-method parameters into the `Object[]` array passed to
/// `BeanFactory.getBean(String, Object...)` — see `emit_load_box2`.
struct ValueOfRefs {
    valueof_int: u16,
    valueof_long: u16,
    valueof_float: u16,
    valueof_double: u16,
    valueof_bool: u16,
    valueof_byte: u16,
    valueof_char: u16,
    valueof_short: u16,
}

/// Emit `load + box` for one parameter at `slot`, leaving a reference on the
/// operand stack (primitives boxed via `valueOf`). Standalone twin of
/// `emit_load_box` (which takes `&LookupCp`) so `emit_bean_override_with_args`
/// doesn't need to construct an entire unrelated `LookupCp` just for its 8
/// `valueOf` refs.
fn emit_load_box2(code: &mut Vec<u8>, v: &ValueOfRefs, p: &str, slot: u16) {
    let b = |x: u16| x.to_be_bytes();
    let (load_op, valueof): (u8, Option<u16>) = match p {
        "I" => (0x15, Some(v.valueof_int)),
        "Z" => (0x15, Some(v.valueof_bool)),
        "B" => (0x15, Some(v.valueof_byte)),
        "C" => (0x15, Some(v.valueof_char)),
        "S" => (0x15, Some(v.valueof_short)),
        "J" => (0x16, Some(v.valueof_long)),
        "F" => (0x17, Some(v.valueof_float)),
        "D" => (0x18, Some(v.valueof_double)),
        _ => (0x19, None), // aload (reference / array)
    };
    code.push(load_op);
    code.push(slot as u8);
    if let Some(vref) = valueof {
        code.push(0xB8); // invokestatic Boxtype.valueOf
        code.extend_from_slice(&b(vref));
    }
}

/// Like `emit_bean_override`, but for `@Bean` methods that take one or more
/// parameters. `emit_bean_override` alone can't handle these: its local-slot
/// layout hardcodes `this` at 0 and everything else at 1..=7, leaving no room
/// for real parameters (which the JVM requires at 1..=N for an instance
/// method matching its own descriptor). Implements Spring's
/// `resolveBeanReference` `useArgs` decision for the inter-bean-reference
/// path: a non-singleton bean always resolves via `getBean(name, args)`; a
/// singleton bean resolves via `getBean(name, args)` ONLY if every
/// reference-typed arg is non-null, else falls back to plain `getBean(name)`
/// so the container's own creation/autowiring supplies the real argument
/// instead of a stubbed null one — see `nullArgumentThroughBeanMethodCall`.
/// FactoryBean-enhancement (`fb_ref` in `emit_bean_override`) is intentionally
/// NOT supported here: no test exercises a FactoryBean-typed `@Bean` method
/// that also takes parameters, and real Spring's own `enhanceFactoryBean`
/// path assumes a no-arg getter shape.
#[allow(clippy::too_many_arguments)]
fn emit_bean_override_with_args(
    name_idx: u16,
    descriptor_idx: u16,
    code_attr_name_idx: u16,
    name_string_idx: u16,
    params: &[String],
    super_method_ref: u16,
    bf_field_ref: u16,
    beanfactory_cast_idx: u16,
    getbean_ref: u16,
    getbean_name_args_ref: u16,
    is_singleton_ref: u16,
    rettype_cast_idx: u16,
    get_factory_method_ref: u16,
    method_get_name_ref: u16,
    string_equals_ref: u16,
    singleton_registry_cast_idx: u16,
    get_singleton_ref: u16,
    config_gen_iface_idx: u16,
    cfg_gen_name_string_idx: u16,
    beanname_lookup_idx: u16,
    fq_beanname_lookup_idx: u16,
    configurable_bf_cast_idx: u16,
    register_dependent_bean_ref: u16,
    is_currently_in_creation_ref: u16,
    set_currently_in_creation_ref: u16,
    object_class_idx: u16,
    valueof: &ValueOfRefs,
    decl_method_string_idx: u16,
    rettype_dotted_string_idx: u16,
    mismatch: &MismatchRefs,
) -> Vec<u8> {
    let is_wide = |p: &str| p == "J" || p == "D";
    let w: u16 = params.iter().map(|p| if is_wide(p) { 2 } else { 1 }).sum();
    let l_method = 1 + w;
    let l_bf = 2 + w;
    let l_beanname = 3 + w;
    let l_result = 4 + w;
    let l_already = 5 + w;
    let l_cbf = 6 + w;
    let l_scratch = 7 + w;
    let max_locals = 8 + w;

    let mut a = Asm::new();

    // currentlyInvoked = SimpleInstantiationStrategy.getCurrentlyInvokedFactoryMethod()
    a.u8_(0xB8);
    a.u16be(get_factory_method_ref);
    a.u8_(0x3A);
    a.u8_(l_method as u8); // astore l_method
    a.u8_(0x19);
    a.u8_(l_method as u8); // aload l_method
    a.branch(0xC6, "L_GETBEAN"); // ifnull
    a.u8_(0x19);
    a.u8_(l_method as u8); // aload l_method
    a.u8_(0xB6);
    a.u16be(method_get_name_ref); // invokevirtual Method.getName()
    a.u8_(0x13);
    a.u16be(name_string_idx); // ldc_w "<name>"
    a.u8_(0xB6);
    a.u16be(string_equals_ref); // invokevirtual String.equals
    a.branch(0x99, "L_GETBEAN"); // ifeq

    a.mark("L_SUPER");
    a.u8_(0x2A); // aload_0
    {
        let mut s = 1u16;
        for p in params {
            let op: u8 = match p.as_str() {
                "J" => 0x16,
                "F" => 0x17,
                "D" => 0x18,
                "I" | "Z" | "B" | "C" | "S" => 0x15,
                _ => 0x19,
            };
            a.u8_(op);
            a.u8_(s as u8);
            s += if is_wide(p) { 2 } else { 1 };
        }
    }
    a.u8_(0xB7);
    a.u16be(super_method_ref); // invokespecial super.<name><desc>
    a.u8_(0xB0); // areturn

    a.mark("L_GETBEAN");
    a.u8_(0x2A); // aload_0
    a.u8_(0xB4);
    a.u16be(bf_field_ref); // getfield $$beanFactory
    a.u8_(0x3A);
    a.u8_(l_bf as u8); // astore l_bf
    a.u8_(0x13);
    a.u16be(beanname_lookup_idx); // ldc_w <default beanName>
    a.u8_(0x3A);
    a.u8_(l_beanname as u8); // astore l_beanname
    a.u8_(0x19);
    a.u8_(l_bf as u8); // aload l_bf
    a.u8_(0xC1);
    a.u16be(singleton_registry_cast_idx); // instanceof SingletonBeanRegistry
    a.branch(0x99, "L_GET"); // ifeq
    a.u8_(0x19);
    a.u8_(l_bf as u8); // aload l_bf
    a.u8_(0xC0);
    a.u16be(singleton_registry_cast_idx); // checkcast SingletonBeanRegistry
    a.u8_(0x13);
    a.u16be(cfg_gen_name_string_idx); // ldc_w CONFIGURATION_BEAN_NAME_GENERATOR
    a.u8_(0xB9);
    a.u16be(get_singleton_ref); // invokeinterface getSingleton(String)Object count=2
    a.u8_(0x02);
    a.u8_(0x00);
    a.u8_(0xC1);
    a.u16be(config_gen_iface_idx); // instanceof ConfigurationBeanNameGenerator
    a.branch(0x99, "L_GET"); // ifeq
    a.u8_(0x13);
    a.u16be(fq_beanname_lookup_idx); // ldc_w <fq name>
    a.u8_(0x3A);
    a.u8_(l_beanname as u8); // astore l_beanname

    a.mark("L_GET");
    a.u8_(0x19);
    a.u8_(l_bf as u8); // aload l_bf
    a.u8_(0xC0);
    a.u16be(configurable_bf_cast_idx); // checkcast ConfigurableBeanFactory
    a.u8_(0x3A);
    a.u8_(l_cbf as u8); // astore l_cbf
    a.u8_(0x19);
    a.u8_(l_cbf as u8); // aload l_cbf
    a.u8_(0x19);
    a.u8_(l_beanname as u8); // aload l_beanname
    a.u8_(0xB9);
    a.u16be(is_currently_in_creation_ref); // invokeinterface isCurrentlyInCreation(String)Z count=2
    a.u8_(0x02);
    a.u8_(0x00);
    a.u8_(0x36);
    a.u8_(l_already as u8); // istore l_already
    a.u8_(0x15);
    a.u8_(l_already as u8); // iload l_already
    a.branch(0x99, "TRY_START"); // ifeq
    a.u8_(0x19);
    a.u8_(l_cbf as u8); // aload l_cbf
    a.u8_(0x19);
    a.u8_(l_beanname as u8); // aload l_beanname
    a.u8_(0x03); // iconst_0
    a.u8_(0xB9);
    a.u16be(set_currently_in_creation_ref); // invokeinterface setCurrentlyInCreation(String,boolean)V count=3
    a.u8_(0x03);
    a.u8_(0x00);

    a.mark("TRY_START");
    // useArgs decision: !isSingleton -> always use args; isSingleton -> use
    // args only if every reference-typed param is non-null (mirrors real
    // Spring's resolveBeanReference exactly).
    a.u8_(0x19);
    a.u8_(l_bf as u8); // aload l_bf
    a.u8_(0xC0);
    a.u16be(beanfactory_cast_idx); // checkcast BeanFactory
    a.u8_(0x19);
    a.u8_(l_beanname as u8); // aload l_beanname
    a.u8_(0xB9);
    a.u16be(is_singleton_ref); // invokeinterface isSingleton(String)Z count=2
    a.u8_(0x02);
    a.u8_(0x00);
    a.branch(0x99, "L_WITH_ARGS"); // ifeq (not singleton -> use args)
    {
        let mut s = 1u16;
        for p in params {
            let is_ref = !matches!(p.as_str(), "I" | "Z" | "B" | "C" | "S" | "J" | "F" | "D");
            if is_ref {
                a.u8_(0x19);
                a.u8_(s as u8); // aload s
                a.branch(0xC6, "L_NOARGS"); // ifnull
            }
            s += if is_wide(p) { 2 } else { 1 };
        }
    }

    a.mark("L_WITH_ARGS");
    a.u8_(0x19);
    a.u8_(l_bf as u8); // aload l_bf
    a.u8_(0xC0);
    a.u16be(beanfactory_cast_idx); // checkcast BeanFactory
    a.u8_(0x19);
    a.u8_(l_beanname as u8); // aload l_beanname
    push_int(&mut a.code, params.len() as i32);
    a.u8_(0xBD);
    a.u16be(object_class_idx); // anewarray Object
    {
        let mut s = 1u16;
        for (i, p) in params.iter().enumerate() {
            a.u8_(0x59); // dup
            push_int(&mut a.code, i as i32);
            emit_load_box2(&mut a.code, valueof, p, s);
            a.u8_(0x53); // aastore
            s += if is_wide(p) { 2 } else { 1 };
        }
    }
    a.u8_(0xB9);
    a.u16be(getbean_name_args_ref); // invokeinterface getBean(String,Object[])Object count=3
    a.u8_(0x03);
    a.u8_(0x00);
    a.branch(0xA7, "L_GETBEAN_DONE"); // goto

    a.mark("L_NOARGS");
    a.u8_(0x19);
    a.u8_(l_bf as u8); // aload l_bf
    a.u8_(0xC0);
    a.u16be(beanfactory_cast_idx); // checkcast BeanFactory
    a.u8_(0x19);
    a.u8_(l_beanname as u8); // aload l_beanname
    a.u8_(0xB9);
    a.u16be(getbean_ref); // invokeinterface getBean(String)Object count=2
    a.u8_(0x02);
    a.u8_(0x00);

    a.mark("L_GETBEAN_DONE");
    // --- Type-mismatch check (same shape as emit_bean_override's, adapted
    // to this method's shifted local slots). ---
    a.u8_(0x59); // dup
    a.u8_(0xC1);
    a.u16be(rettype_cast_idx); // instanceof <Ret>
    a.branch(0x9A, "L_STORE"); // ifne
    a.u8_(0x59); // dup
    a.u8_(0x01); // aconst_null
    a.u8_(0xB6);
    a.u16be(mismatch.object_equals_ref); // invokevirtual Object.equals
    a.branch(0x99, "L_THROW"); // ifeq
    a.u8_(0x57); // pop
    a.u8_(0x01); // aconst_null
    a.branch(0xA7, "L_STORE"); // goto

    a.mark("L_THROW");
    a.u8_(0x3A);
    a.u8_(l_scratch as u8); // astore l_scratch
    a.u8_(0xBB);
    a.u16be(mismatch.stringbuilder_cls); // new StringBuilder
    a.u8_(0x59); // dup
    a.u8_(0xB7);
    a.u16be(mismatch.sb_init_ref); // invokespecial <init>()V
    a.u8_(0x13);
    a.u16be(mismatch.lit_bean_method_prefix); // ldc_w "@Bean method "
    a.u8_(0xB6);
    a.u16be(mismatch.sb_append_string_ref);
    a.u8_(0x13);
    a.u16be(decl_method_string_idx); // ldc_w "Decl.method"
    a.u8_(0xB6);
    a.u16be(mismatch.sb_append_string_ref);
    a.u8_(0x13);
    a.u16be(mismatch.lit_called_as); // ldc_w " called as bean reference for type ["
    a.u8_(0xB6);
    a.u16be(mismatch.sb_append_string_ref);
    a.u8_(0x13);
    a.u16be(rettype_dotted_string_idx); // ldc_w "<rettype dotted>"
    a.u8_(0xB6);
    a.u16be(mismatch.sb_append_string_ref);
    a.u8_(0x13);
    a.u16be(mismatch.lit_overridden_by); // ldc_w "] but overridden by non-compatible bean instance of type ["
    a.u8_(0xB6);
    a.u16be(mismatch.sb_append_string_ref);
    a.u8_(0x19);
    a.u8_(l_scratch as u8); // aload l_scratch
    a.u8_(0xB6);
    a.u16be(mismatch.object_getclass_ref); // invokevirtual getClass
    a.u8_(0xB6);
    a.u16be(mismatch.class_getname_ref); // invokevirtual getName
    a.u8_(0xB6);
    a.u16be(mismatch.sb_append_string_ref);
    a.u8_(0x13);
    a.u16be(mismatch.lit_close_bracket_dot); // ldc_w "]."
    a.u8_(0xB6);
    a.u16be(mismatch.sb_append_string_ref);
    a.u8_(0x59); // dup
    a.u8_(0x3A);
    a.u8_(l_scratch as u8); // astore l_scratch

    a.mark("TRY2_START");
    a.u8_(0x13);
    a.u16be(mismatch.lit_overriding_bean); // ldc_w " Overriding bean of same name declared in: "
    a.u8_(0xB6);
    a.u16be(mismatch.sb_append_string_ref);
    a.u8_(0x19);
    a.u8_(l_cbf as u8); // aload l_cbf
    a.u8_(0x19);
    a.u8_(l_beanname as u8); // aload l_beanname
    a.u8_(0xB9);
    a.u16be(mismatch.get_merged_bd_ref); // invokeinterface getMergedBeanDefinition count=2
    a.u8_(0x02);
    a.u8_(0x00);
    a.u8_(0xB9);
    a.u16be(mismatch.get_resource_desc_ref); // invokeinterface getResourceDescription count=1
    a.u8_(0x01);
    a.u8_(0x00);
    a.u8_(0xB6);
    a.u16be(mismatch.sb_append_string_ref);
    a.mark("TRY2_END");
    a.branch(0xA7, "L_BUILD_DONE"); // goto
    a.mark("CATCH2");
    a.u8_(0x57); // pop
    a.mark("L_BUILD_DONE");
    a.u8_(0xB6);
    a.u16be(mismatch.sb_tostring_ref); // invokevirtual toString
    a.u8_(0x3A);
    a.u8_(l_scratch as u8); // astore l_scratch
    a.u8_(0xBB);
    a.u16be(mismatch.illegalstate_cls); // new IllegalStateException
    a.u8_(0x59); // dup
    a.u8_(0x19);
    a.u8_(l_scratch as u8); // aload l_scratch
    a.u8_(0xB7);
    a.u16be(mismatch.illegalstate_init_ref); // invokespecial <init>(String)V
    a.u8_(0xBF); // athrow

    a.mark("L_STORE");
    a.u8_(0xC0);
    a.u16be(rettype_cast_idx); // checkcast <Ret>
    a.u8_(0x3A);
    a.u8_(l_result as u8); // astore l_result
    let try_start = *a.labels.get("TRY_START").unwrap();
    let try_end = a.pos();

    // Normal path: dependency registration + SPR-8080 restore (outside the
    // try region — matches `emit_bean_override`'s own scoping choice).
    a.u8_(0x19);
    a.u8_(l_method as u8); // aload l_method
    a.branch(0xC6, "L_RESTORE"); // ifnull
    a.u8_(0x19);
    a.u8_(l_cbf as u8); // aload l_cbf
    a.u8_(0x19);
    a.u8_(l_beanname as u8); // aload l_beanname
    a.u8_(0x19);
    a.u8_(l_method as u8); // aload l_method
    a.u8_(0xB6);
    a.u16be(method_get_name_ref); // invokevirtual Method.getName()
    a.u8_(0xB9);
    a.u16be(register_dependent_bean_ref); // invokeinterface registerDependentBean(String,String)V count=3
    a.u8_(0x03);
    a.u8_(0x00);

    a.mark("L_RESTORE");
    a.u8_(0x15);
    a.u8_(l_already as u8); // iload l_already
    a.branch(0x99, "L_RETURN"); // ifeq
    a.u8_(0x19);
    a.u8_(l_cbf as u8); // aload l_cbf
    a.u8_(0x19);
    a.u8_(l_beanname as u8); // aload l_beanname
    a.u8_(0x04); // iconst_1
    a.u8_(0xB9);
    a.u16be(set_currently_in_creation_ref); // invokeinterface setCurrentlyInCreation(String,boolean)V count=3
    a.u8_(0x03);
    a.u8_(0x00);

    a.mark("L_RETURN");
    a.u8_(0x19);
    a.u8_(l_result as u8); // aload l_result
    a.u8_(0xB0); // areturn

    let handler_pc = a.pos();
    a.u8_(0x3A);
    a.u8_(l_scratch as u8); // astore l_scratch  (caught throwable)
    a.u8_(0x15);
    a.u8_(l_already as u8); // iload l_already
    a.branch(0x99, "RETHROW"); // ifeq
    a.u8_(0x19);
    a.u8_(l_cbf as u8); // aload l_cbf
    a.u8_(0x19);
    a.u8_(l_beanname as u8); // aload l_beanname
    a.u8_(0x04); // iconst_1
    a.u8_(0xB9);
    a.u16be(set_currently_in_creation_ref); // invokeinterface setCurrentlyInCreation(String,boolean)V count=3
    a.u8_(0x03);
    a.u8_(0x00);
    a.mark("RETHROW");
    a.u8_(0x19);
    a.u8_(l_scratch as u8); // aload l_scratch
    a.u8_(0xBF); // athrow

    let try2_start = *a.labels.get("TRY2_START").unwrap();
    let try2_end = *a.labels.get("TRY2_END").unwrap();
    let handler_pc2 = *a.labels.get("CATCH2").unwrap();
    let nsbde_cls = mismatch.nsbde_cls;

    let code = a.finish();

    let mut code_attr = Vec::new();
    // max_stack: the Object[]-building loop briefly reaches array+index+
    // boxed-value (3) on top of bf/beanName already consumed by that point;
    // the deepest actual point is bf+beanName+array (3) before the loop, or
    // array+index+box (3) during it. 6 is generous headroom.
    let max_stack: u16 = 6;
    code_attr.extend_from_slice(&max_stack.to_be_bytes());
    code_attr.extend_from_slice(&max_locals.to_be_bytes());
    code_attr.extend_from_slice(&(code.len() as u32).to_be_bytes());
    code_attr.extend_from_slice(&code);
    code_attr.extend_from_slice(&2u16.to_be_bytes()); // exception_table_length
                                                      // Entry 1 (checked first): inner NoSuchBeanDefinitionException handler.
    code_attr.extend_from_slice(&try2_start.to_be_bytes());
    code_attr.extend_from_slice(&try2_end.to_be_bytes());
    code_attr.extend_from_slice(&handler_pc2.to_be_bytes());
    code_attr.extend_from_slice(&nsbde_cls.to_be_bytes());
    // Entry 2: outer SPR-8080 any-Throwable finally handler.
    code_attr.extend_from_slice(&try_start.to_be_bytes());
    code_attr.extend_from_slice(&try_end.to_be_bytes());
    code_attr.extend_from_slice(&handler_pc.to_be_bytes());
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

fn build_enhancer_class(
    super_loader_id: u32,
    super_internal_name: &str,
    bean_methods: &[BeanMethod],
    ctor_descriptors: &[String],
) -> (String, Vec<u8>) {
    let counter = next_config_enhancer_counter(super_loader_id, super_internal_name);
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

    // -- CONSTANT_Class entries: this, super, and the marker interfaces.
    let this_class_idx = cw.add_class(&new_name);
    let super_class_idx = cw.add_class(super_internal_name);
    let iface_idx = cw.add_class(SPRING_MARKER_IFACE);
    // `org/springframework/cglib/proxy/Factory` — see
    // `emit_cglib_factory_interface_methods`'s doc comment: the SAME
    // real-cglib recovery path that needed the `CGLIB$*` bookkeeping
    // fields also does `(Factory) klass_instance` somewhere in
    // `Enhancer`'s post-generation bookkeeping, unconditionally (real
    // cglib-generated classes ALWAYS implement this marker), so it must
    // be implemented here too, not just field-compatible.
    let factory_iface_idx = cw.add_class("org/springframework/cglib/proxy/Factory");

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
        .map(|d| {
            emit_ctor_for_descriptor(
                &mut cw,
                init_name_idx,
                code_attr_name_idx,
                super_class_idx,
                d,
            )
        })
        .collect();

    let set_bean_factory = emit_set_bean_factory(
        set_bf_name_idx,
        set_bf_desc_idx,
        code_attr_name_idx,
        bf_field_ref,
    );
    methods.push(set_bean_factory);

    let set_static_callbacks_name_idx = cw.add_utf8("CGLIB$SET_STATIC_CALLBACKS");
    methods.push(emit_noop_callback_setter(
        &mut cw,
        set_static_callbacks_name_idx,
        code_attr_name_idx,
    ));
    let set_thread_callbacks_name_idx = cw.add_utf8("CGLIB$SET_THREAD_CALLBACKS");
    methods.push(emit_noop_callback_setter(
        &mut cw,
        set_thread_callbacks_name_idx,
        code_attr_name_idx,
    ));
    methods.extend(emit_cglib_factory_interface_methods(
        &mut cw,
        code_attr_name_idx,
    ));

    let field = emit_bean_factory_field(bf_field_name_idx, object_desc_idx);
    // Real-cglib-bookkeeping fields — see `emit_public_static_field`'s doc
    // comment. All `Ljava/lang/Object;`-typed except `CGLIB$BOUND`, which
    // real cglib declares `boolean` (`Field.getBoolean`/`setBoolean` would
    // throw `IllegalArgumentException` on a mismatched declared type).
    let factory_data_name_idx = cw.add_utf8("CGLIB$FACTORY_DATA");
    let factory_data_field = emit_public_static_field(factory_data_name_idx, object_desc_idx);
    let callback_filter_name_idx = cw.add_utf8("CGLIB$CALLBACK_FILTER");
    let callback_filter_field = emit_public_static_field(callback_filter_name_idx, object_desc_idx);
    let thread_callbacks_name_idx = cw.add_utf8("CGLIB$THREAD_CALLBACKS");
    let thread_callbacks_field =
        emit_public_static_field(thread_callbacks_name_idx, object_desc_idx);
    let static_callbacks_name_idx = cw.add_utf8("CGLIB$STATIC_CALLBACKS");
    let static_callbacks_field =
        emit_public_static_field(static_callbacks_name_idx, object_desc_idx);
    let bound_name_idx = cw.add_utf8("CGLIB$BOUND");
    let bool_desc_idx = cw.add_utf8("Z");
    let bound_field = emit_public_static_field(bound_name_idx, bool_desc_idx);

    if !bean_methods.is_empty() {
        // Shared constant-pool refs used by every @Bean override.
        let beanfactory_cast_idx = cw.add_class("org/springframework/beans/factory/BeanFactory");
        let getbean_ref = cw.add_interface_methodref(
            beanfactory_cast_idx,
            "getBean",
            "(Ljava/lang/String;)Ljava/lang/Object;",
        );
        // Parameterized @Bean methods only (emit_bean_override_with_args):
        // the explicit-args getBean overload and the isSingleton() check
        // driving Spring's `useArgs` decision.
        let getbean_name_args_ref = cw.add_interface_methodref(
            beanfactory_cast_idx,
            "getBean",
            "(Ljava/lang/String;[Ljava/lang/Object;)Ljava/lang/Object;",
        );
        let is_singleton_ref = cw.add_interface_methodref(
            beanfactory_cast_idx,
            "isSingleton",
            "(Ljava/lang/String;)Z",
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

        // FactoryBean-enhancement (SPR-6602/11202/15275) invokestatic target —
        // see `emit_bean_override`'s `fb_ref` doc comment and
        // `enhance_factory_bean_reference`. Added unconditionally alongside
        // the other shared refs above (harmless unused constant-pool entries
        // if no `@Bean` method in this class happens to be FactoryBean-typed).
        let config_enhancer_support_cls = cw.add_class("cratonvm/internal/ConfigEnhancerSupport");
        let enhance_fb_ref_methodref = cw.add_methodref(
            config_enhancer_support_cls,
            "enhanceFactoryBeanReference",
            "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/String;Ljava/lang/String;)Ljava/lang/Object;",
        );

        // Shared refs for `emit_bean_override`'s type-mismatch handling —
        // see `MismatchRefs`'s doc comment.
        let mismatch_object_cls = cw.add_class("java/lang/Object");
        let mismatch_class_cls = cw.add_class("java/lang/Class");
        let mismatch_sb_cls = cw.add_class("java/lang/StringBuilder");
        let mismatch_ise_cls = cw.add_class("java/lang/IllegalStateException");
        let mismatch_beandef_cls =
            cw.add_class("org/springframework/beans/factory/config/BeanDefinition");
        let mismatch_refs = MismatchRefs {
            stringbuilder_cls: mismatch_sb_cls,
            sb_init_ref: cw.add_methodref(mismatch_sb_cls, "<init>", "()V"),
            sb_append_string_ref: cw.add_methodref(
                mismatch_sb_cls,
                "append",
                "(Ljava/lang/String;)Ljava/lang/StringBuilder;",
            ),
            sb_tostring_ref: cw.add_methodref(mismatch_sb_cls, "toString", "()Ljava/lang/String;"),
            object_equals_ref: cw.add_methodref(
                mismatch_object_cls,
                "equals",
                "(Ljava/lang/Object;)Z",
            ),
            object_getclass_ref: cw.add_methodref(
                mismatch_object_cls,
                "getClass",
                "()Ljava/lang/Class;",
            ),
            class_getname_ref: cw.add_methodref(
                mismatch_class_cls,
                "getName",
                "()Ljava/lang/String;",
            ),
            illegalstate_cls: mismatch_ise_cls,
            illegalstate_init_ref: cw.add_methodref(
                mismatch_ise_cls,
                "<init>",
                "(Ljava/lang/String;)V",
            ),
            nsbde_cls: cw
                .add_class("org/springframework/beans/factory/NoSuchBeanDefinitionException"),
            get_merged_bd_ref: cw.add_interface_methodref(
                configurable_bf_cast_idx,
                "getMergedBeanDefinition",
                "(Ljava/lang/String;)Lorg/springframework/beans/factory/config/BeanDefinition;",
            ),
            get_resource_desc_ref: cw.add_interface_methodref(
                mismatch_beandef_cls,
                "getResourceDescription",
                "()Ljava/lang/String;",
            ),
            lit_bean_method_prefix: cw.add_string("@Bean method "),
            lit_called_as: cw.add_string(" called as bean reference for type ["),
            lit_overridden_by: cw
                .add_string("] but overridden by non-compatible bean instance of type ["),
            lit_close_bracket_dot: cw.add_string("]."),
            lit_overriding_bean: cw.add_string(" Overriding bean of same name declared in: "),
        };

        // Shared boxing refs for parameterized @Bean methods
        // (emit_bean_override_with_args) — see `ValueOfRefs`.
        let integer_cls = cw.add_class("java/lang/Integer");
        let long_cls = cw.add_class("java/lang/Long");
        let float_cls = cw.add_class("java/lang/Float");
        let double_cls = cw.add_class("java/lang/Double");
        let boolean_cls = cw.add_class("java/lang/Boolean");
        let byte_cls = cw.add_class("java/lang/Byte");
        let char_cls = cw.add_class("java/lang/Character");
        let short_cls = cw.add_class("java/lang/Short");
        let valueof_refs = ValueOfRefs {
            valueof_int: cw.add_methodref(integer_cls, "valueOf", "(I)Ljava/lang/Integer;"),
            valueof_long: cw.add_methodref(long_cls, "valueOf", "(J)Ljava/lang/Long;"),
            valueof_float: cw.add_methodref(float_cls, "valueOf", "(F)Ljava/lang/Float;"),
            valueof_double: cw.add_methodref(double_cls, "valueOf", "(D)Ljava/lang/Double;"),
            valueof_bool: cw.add_methodref(boolean_cls, "valueOf", "(Z)Ljava/lang/Boolean;"),
            valueof_byte: cw.add_methodref(byte_cls, "valueOf", "(B)Ljava/lang/Byte;"),
            valueof_char: cw.add_methodref(char_cls, "valueOf", "(C)Ljava/lang/Character;"),
            valueof_short: cw.add_methodref(short_cls, "valueOf", "(S)Ljava/lang/Short;"),
        };

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
            // Precomputed at codegen time (both pieces are statically known
            // here) for the type-mismatch message — see `MismatchRefs`.
            let decl_method_string_idx = cw.add_string(&format!(
                "{}.{}",
                simple_name_of_internal(super_internal_name),
                bm.name
            ));
            let rettype_dotted_string_idx = cw.add_string(&bm.return_internal.replace('/', "."));
            let fb_ref = if bm.is_factory_bean {
                let exposed_type_string_idx = cw.add_string(&bm.return_internal);
                Some((enhance_fb_ref_methodref, exposed_type_string_idx))
            } else {
                None
            };
            let bean_method_params = parse_param_descriptors(&bm.descriptor);
            let override_method = if bean_method_params.is_empty() {
                emit_bean_override(
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
                    fb_ref,
                    decl_method_string_idx,
                    rettype_dotted_string_idx,
                    &mismatch_refs,
                )
            } else {
                emit_bean_override_with_args(
                    name_idx,
                    desc_idx,
                    code_attr_name_idx,
                    name_string_idx,
                    &bean_method_params,
                    super_method_ref,
                    bf_field_ref,
                    beanfactory_cast_idx,
                    getbean_ref,
                    getbean_name_args_ref,
                    is_singleton_ref,
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
                    mismatch_object_cls,
                    &valueof_refs,
                    decl_method_string_idx,
                    rettype_dotted_string_idx,
                    &mismatch_refs,
                )
            };
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
        &[iface_idx, factory_iface_idx],
        &[
            field,
            factory_data_field,
            callback_filter_field,
            thread_callbacks_field,
            static_callbacks_field,
            bound_field,
        ],
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
    /// Internal name of the class whose OWN `declared_methods` this abstract
    /// method actually appears in — NOT necessarily this subclass's immediate
    /// superclass, since another enhancer (e.g. `ConfigurationClassEnhancer`)
    /// may already have wrapped the original user class before this lookup
    /// subclass wraps THAT in turn. Baked into the generated bytecode as a
    /// `Class` constant so the by-type/no-args branch can look the `Method`
    /// up reflectively without a `getClass().getSuperclass()` walk that only
    /// ever finds one level up. See beanLookupFromSameConfigurationClass.
    pub declaring_internal: String,
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
            // Resolves the Method via a baked-in Class constant for
            // `m.declaring_internal` instead of `this.getClass()
            // .getSuperclass()` — the latter only ever finds one level up,
            // which is wrong once another enhancer (e.g.
            // ConfigurationClassEnhancer) already wraps the original user
            // class before this lookup subclass wraps THAT in turn.
            let mname_string_idx = cw.add_string(&m.name);
            let declaring_class_idx = cw.add_class(&m.declaring_internal);
            code.push(0x2A);
            code.push(0xB4);
            code.extend_from_slice(&b(cp.bf_field_ref));
            code.push(0xC0);
            code.extend_from_slice(&b(cp.beanfactory_cast_idx)); // [bf]
            code.push(0x13); // ldc_w <declaring class>
            code.extend_from_slice(&b(declaring_class_idx));
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
    /// Internal name of the class that actually DECLARES this method --
    /// not necessarily the enhanced subclass's immediate superclass. A
    /// replaced method inherited from a grandparent (or higher) class,
    /// e.g. `MethodReplaceCandidate.replaceMe` inherited two levels down
    /// through `SerializableMethodReplacerCandidate`, must reflectively
    /// resolve via ITS OWN declaring class: `Class.getDeclaredMethod` only
    /// finds methods declared directly on the class it's called on, so
    /// always querying the immediate superclass threw
    /// `NoSuchMethodException` for anything declared further up
    /// (XmlBeanFactoryTests.serializableMethodReplacerAndSuperclass).
    pub declaring_internal: String,
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
/// `own_bean_factory_field`: pass `false` when `super_internal_name` is
/// ITSELF an already-generated lookup-override subclass (see
/// `try_build_method_injection`'s chaining in spring_startup_bootstrap.rs)
/// that already declares a `$$beanFactory` field. Declaring a SECOND field
/// of the same name on this subclass would shadow rather than reuse it:
/// the generated methods below reference the field via `this_class_idx`,
/// so they'd read/write the new (never-initialised) shadow copy while
/// `set_field_by_name`'s hierarchy walk resolves whichever copy comes
/// first -- observed as `this.$$beanFactory` staying null
/// (XmlBeanFactoryTests.replaceMethodOverrideWithSetterInjection, a bean
/// needing BOTH a lookup-override-satisfied abstract method AND a
/// replaced concrete method). When `false`, skip declaring the field and
/// reference the inherited one instead -- a `getfield`/`putfield` naming
/// a subclass still resolves to the superclass's field per JVMS 5.4.3.2
/// field resolution.
pub fn build_replace_override_subclass(
    super_internal_name: &str,
    methods: &[ReplaceMethodSpec],
    own_bean_factory_field: bool,
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
    let bf_field_ref = cw.add_fieldref(
        if own_bean_factory_field {
            this_class_idx
        } else {
            super_class_idx
        },
        "$$beanFactory",
        "Ljava/lang/Object;",
    );

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
        // Resolve the Method reflectively against the class that actually
        // declares it (see the field doc on `declaring_internal`), not
        // always the enhanced subclass's immediate superclass.
        let declaring_class_idx = cw.add_class(&m.declaring_internal);

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

        // (B) method = DeclaringClass.class.getDeclaredMethod(name, paramClasses)
        code.push(0x13); // ldc_w DeclaringClass.class
        code.extend_from_slice(&b(declaring_class_idx));
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
    let fields: &[Vec<u8>] = if own_bean_factory_field {
        &[bf_field]
    } else {
        &[]
    };
    let bytes = cw.finish(
        access_flags,
        this_class_idx,
        super_class_idx,
        &[],
        fields,
        &method_bytes,
    );
    (new_name, bytes)
}

/// Sibling of [`build_replace_override_subclass`] for the case where the
/// bean's declared class is itself an INTERFACE (e.g. `EchoService` in
/// `XmlBeanFactoryTests.replaceNonOverloadedInterfaceMethodWithoutSpecifyingExplicitArgTypes`).
/// An interface has no `<init>` and cannot be `extends`ed — real CGLIB's
/// `Enhancer` documents exactly this: "if the 'superclass' is in fact an
/// interface, turn it into an implemented interface". So this generates
/// `class $$SpringCGLIB$$RMI<N> extends Object implements <iface>` instead
/// of `extends <iface>`.
///
/// `methods` are the interface's abstract (or default) methods matched to a
/// configured `<replaced-method>` — same delegating-to-`MethodReplacer` body
/// as the class-based generator. `uncovered` are the interface's OTHER
/// abstract methods with no matching override: since the generated class
/// must be concrete (every abstract interface method needs SOME method
/// body to be instantiable), each gets a throwing `AbstractMethodError` stub
/// — the same fallback `build_lookup_subclass` already uses for an abstract
/// method with no matching `<lookup-method>`. A default (non-abstract)
/// interface method that isn't in `methods` needs no stub at all: normal
/// interface-default-method inheritance already provides its body.
pub fn build_replace_override_subclass_for_interface(
    iface_internal_name: &str,
    methods: &[ReplaceMethodSpec],
    uncovered: &[(String, String)],
) -> (String, Vec<u8>) {
    let counter = ENHANCER_COUNTER.fetch_add(1, Ordering::Relaxed);
    let new_name = format!("{iface_internal_name}$$SpringCGLIB$$RMI{counter:x}");
    let mut cw = ClassWriter::new();

    let this_class_idx = cw.add_class(&new_name);
    let object_super_idx = cw.add_class("java/lang/Object");
    let iface_idx = cw.add_class(iface_internal_name);

    let init_name_idx = cw.add_utf8("<init>");
    let void_no_arg_desc_idx = cw.add_utf8("()V");
    let code_attr_name_idx = cw.add_utf8("Code");
    let super_init_methodref = cw.add_methodref(object_super_idx, "<init>", "()V");

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
    let ame_class_idx = cw.add_class("java/lang/AbstractMethodError");
    let ame_init_ref = cw.add_methodref(ame_class_idx, "<init>", "()V");

    let number_cls = cw.add_class("java/lang/Number");
    let num_byte_value = cw.add_methodref(number_cls, "byteValue", "()B");
    let num_short_value = cw.add_methodref(number_cls, "shortValue", "()S");
    let num_int_value = cw.add_methodref(number_cls, "intValue", "()I");
    let num_long_value = cw.add_methodref(number_cls, "longValue", "()J");
    let num_float_value = cw.add_methodref(number_cls, "floatValue", "()F");
    let num_double_value = cw.add_methodref(number_cls, "doubleValue", "()D");

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
        let declaring_class_idx = cw.add_class(&m.declaring_internal);

        let params = parse_param_descriptors(&m.descriptor);
        let ret = m.descriptor.split(')').nth(1).unwrap_or("V").to_string();
        let ret_char = ret.chars().next().unwrap_or('V');
        let primitive_ret = matches!(ret_char, 'I' | 'J' | 'F' | 'D' | 'Z' | 'B' | 'C' | 'S');

        let param_class_consts: Vec<Option<u16>> = params
            .iter()
            .map(|p| {
                if p.starts_with('L') || p.starts_with('[') {
                    let internal = if p.starts_with('L') && p.ends_with(';') {
                        p[1..p.len() - 1].to_string()
                    } else {
                        p.clone()
                    };
                    Some(cw.add_class(&internal))
                } else {
                    None
                }
            })
            .collect();
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
        code.push(0x03);
        code.push(0x00);
        code.push(0xC0); // checkcast MethodReplacer
        code.extend_from_slice(&b(methodreplacer_cls));
        code.push(0x3A); // astore mr_slot
        code.push(mr_slot);

        // (B) method = DeclaringClass.class.getDeclaredMethod(name, paramClasses)
        code.push(0x13);
        code.extend_from_slice(&b(declaring_class_idx));
        code.push(0x13);
        code.extend_from_slice(&b(name_str_idx));
        push_int(&mut code, params.len() as i32);
        code.push(0xBD);
        code.extend_from_slice(&b(class_cls));
        for (i, p) in params.iter().enumerate() {
            code.push(0x59);
            push_int(&mut code, i as i32);
            match param_class_consts[i] {
                Some(cidx) => {
                    code.push(0x13);
                    code.extend_from_slice(&b(cidx));
                }
                None => {
                    let ch = p.chars().next().unwrap_or('I');
                    code.push(0xB2);
                    code.extend_from_slice(&b(wrapper_for(ch).type_field_ref));
                }
            }
            code.push(0x53);
        }
        code.push(0xB6);
        code.extend_from_slice(&b(getdeclaredmethod_ref));
        code.push(0x3A);
        code.push(method_slot);

        // (C) args = new Object[]{ boxed params }
        push_int(&mut code, params.len() as i32);
        code.push(0xBD);
        code.extend_from_slice(&b(object_cls));
        let mut slot = 1u16;
        for (i, p) in params.iter().enumerate() {
            code.push(0x59);
            push_int(&mut code, i as i32);
            let ch = p.chars().next().unwrap_or('L');
            match ch {
                'J' => {
                    code.push(0x16);
                    code.push(slot as u8);
                    code.push(0xB8);
                    code.extend_from_slice(&b(wr_j.valueof_ref));
                    slot += 2;
                }
                'F' => {
                    code.push(0x17);
                    code.push(slot as u8);
                    code.push(0xB8);
                    code.extend_from_slice(&b(wr_f.valueof_ref));
                    slot += 1;
                }
                'D' => {
                    code.push(0x18);
                    code.push(slot as u8);
                    code.push(0xB8);
                    code.extend_from_slice(&b(wr_d.valueof_ref));
                    slot += 2;
                }
                'I' | 'Z' | 'B' | 'C' | 'S' => {
                    code.push(0x15);
                    code.push(slot as u8);
                    code.push(0xB8);
                    code.extend_from_slice(&b(wrapper_for(ch).valueof_ref));
                    slot += 1;
                }
                _ => {
                    code.push(0x19);
                    code.push(slot as u8);
                    slot += 1;
                }
            }
            code.push(0x53);
        }
        code.push(0x3A);
        code.push(args_slot);

        // (D) result = mr.reimplement(this, method, args)
        code.push(0x19);
        code.push(mr_slot);
        code.push(0x2A);
        code.push(0x19);
        code.push(method_slot);
        code.push(0x19);
        code.push(args_slot);
        code.push(0xB9);
        code.extend_from_slice(&b(reimplement_ref));
        code.push(0x04);
        code.push(0x00);
        code.push(0x3A);
        code.push(result_slot);

        // (E) processReturnType + return.
        if primitive_ret {
            let mut throw_block: Vec<u8> = Vec::new();
            throw_block.push(0xBB);
            throw_block.extend_from_slice(&b(ise_cls));
            throw_block.push(0x59);
            throw_block.push(0xBB);
            throw_block.extend_from_slice(&b(sb_cls));
            throw_block.push(0x59);
            throw_block.push(0x13);
            throw_block.extend_from_slice(&b(ise_prefix_str_idx));
            throw_block.push(0xB7);
            throw_block.extend_from_slice(&b(sb_init_ref));
            throw_block.push(0x19);
            throw_block.push(method_slot);
            throw_block.push(0xB6);
            throw_block.extend_from_slice(&b(sb_append_obj_ref));
            throw_block.push(0xB6);
            throw_block.extend_from_slice(&b(sb_tostring_ref));
            throw_block.push(0xB7);
            throw_block.extend_from_slice(&b(ise_init_ref));
            throw_block.push(0xBF);

            code.push(0x19);
            code.push(result_slot);
            code.push(0xC7);
            let off = (3 + throw_block.len()) as u16;
            code.extend_from_slice(&b(off));
            code.extend_from_slice(&throw_block);
            let (cast_idx, xvalue_ref, ret_op): (u16, u16, u8) = match ret_char {
                'Z' => (wr_z.class_idx, wr_z.xvalue_ref, 0xAC),
                'C' => (wr_c.class_idx, wr_c.xvalue_ref, 0xAC),
                'B' => (number_cls, num_byte_value, 0xAC),
                'S' => (number_cls, num_short_value, 0xAC),
                'I' => (number_cls, num_int_value, 0xAC),
                'J' => (number_cls, num_long_value, 0xAD),
                'F' => (number_cls, num_float_value, 0xAE),
                'D' => (number_cls, num_double_value, 0xAF),
                _ => (number_cls, num_int_value, 0xAC),
            };
            code.push(0x19);
            code.push(result_slot);
            code.push(0xC0);
            code.extend_from_slice(&b(cast_idx));
            code.push(0xB6);
            code.extend_from_slice(&b(xvalue_ref));
            code.push(ret_op);
        } else if ret_char == 'V' {
            code.push(0xB1);
        } else {
            code.push(0x19);
            code.push(result_slot);
            if let Some(rc) = ret_ref_class {
                code.push(0xC0);
                code.extend_from_slice(&b(rc));
            }
            code.push(0xB0);
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

    // Every OTHER abstract interface method (no matching <replaced-method>)
    // needs SOME body for the class to be concrete/instantiable — a throwing
    // AbstractMethodError stub, same fallback build_lookup_subclass uses.
    for (name, descriptor) in uncovered {
        let name_idx = cw.add_utf8(name);
        let desc_idx = cw.add_utf8(descriptor);
        let params = parse_param_descriptors(descriptor);
        let mlocals = 1 + param_slots(&params);
        let mut code: Vec<u8> = Vec::new();
        code.push(0xBB); // new AbstractMethodError
        code.extend_from_slice(&b(ame_class_idx));
        code.push(0x59); // dup
        code.push(0xB7); // invokespecial <init>
        code.extend_from_slice(&b(ame_init_ref));
        code.push(0xBF); // athrow
        method_bytes.push(wrap_method(
            name_idx,
            desc_idx,
            code_attr_name_idx,
            &code,
            2,
            mlocals,
        ));
    }

    let access_flags: u16 = 0x0001 | 0x0020 | 0x1000; // PUBLIC | SUPER | SYNTHETIC
    let fields: &[Vec<u8>] = &[bf_field];
    let bytes = cw.finish(
        access_flags,
        this_class_idx,
        object_super_idx,
        &[iface_idx],
        fields,
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

/// `SpringNamingPolicy.getClassName(prefix, source, key, names)` — real cglib's
/// name chooser, wrapped so it also avoids names THIS VM already defined.
///
/// Real cglib guarantees uniqueness with `AbstractClassGenerator$ClassLoaderData
/// .reservedClassNames`: every generator it runs reserves the name it took, so
/// the next one picks the following counter value. `cce_enhance` bypasses cglib
/// entirely, so its `<Config>$$SpringCGLIB$$0` is invisible to that set — and
/// the next real-cglib generation for the SAME prefix in the same loader picks
/// `$$0` too. `CglibAopProxy` is exactly that case: it strips the
/// `$$SpringCGLIB$$` suffix (`ClassUtils.getUserClass`) and proxies the raw
/// `@Configuration` class, so its prefix is identical to the enhancer's.
///
/// The second definition then took over the name, and every symbolic
/// field/method ref naming that class resolved to the AOP proxy instead of the
/// enhanced config class:
/// `NoSuchFieldError: ...$Config$$SpringCGLIB$$0.$$beanFactory` out of the
/// enhanced class's own `setBeanFactory`
/// (`BeanMethodPolymorphismTests.beanMethodDetectedOnSuperClass` and
/// `NestedConfigurationClassTests.twoLevelsDeepWithInheritanceAndScopedProxy`,
/// both of which pass in isolation and fail only after a test that AOP-proxies
/// a `@Configuration` bean has run).
///
/// The real method's prefix massaging (`_java.util.…` escaping, the
/// `org.springframework.cglib.empty.Object` default, the `$$FastClass$$` tag)
/// is intricate and covered by `SpringNamingPolicyTests`, so delegate to the
/// real bytecode for the candidate and only advance past names that are
/// already taken. Names cglib itself reserved are already skipped by its own
/// predicate, so this loop is a no-op unless a native generator got there
/// first.
/// The loader namespace real CGLIB is about to define the class it is naming
/// into, or `None` when that cannot be established.
///
/// `AbstractClassGenerator.generate` publishes itself on the static `CURRENT`
/// ThreadLocal *before* it calls `generateClassName`, so
/// `getCurrent().getClassLoader()` is exactly the `ClassLoader` its
/// `ReflectUtils.defineClass` a few lines later will target — the same object
/// `create()` used to pick the `ClassLoaderData` whose reserved-name predicate
/// produced the candidate.
///
/// `AbstractClassGenerator` is resolved from the naming policy's OWN class
/// rather than by name: under `@CompileWithForkedClassLoader` every non-JDK
/// name is defined twice, and the global by-name lookup then answers `None`
/// (deliberately — see `ClassManager::get_loaded_class_id`) or the wrong copy.
/// Both classes live in `org.springframework.cglib.core`, so whichever copy of
/// cglib is running, the referencing lookup lands on its own.
fn current_generator_loader_namespace(
    ctx: &mut dyn NativeContext,
    naming_policy: ObjectRef,
) -> Option<u32> {
    const ACG: &str = "org/springframework/cglib/core/AbstractClassGenerator";
    let policy_cid = ctx.class_id_of_object(naming_policy);
    let acg_cid = ctx
        .class_id_by_name_via_referencing_class(policy_cid, ACG)
        .ok()?;
    let current = ctx
        .invoke_by_class_id(
            acg_cid,
            ACG,
            "getCurrent",
            "()Lorg/springframework/cglib/core/AbstractClassGenerator;",
            &[],
        )
        .ok()??;
    let Value::Object(Some(generator)) = current else {
        return None;
    };
    // `getClassLoader()` runs bytecode (`Enhancer` answers it from its
    // superclass), so the generator has to survive a GC across the call.
    let pin = ctx.pin_native_root(generator);
    let generator = ctx.read_native_pin(pin, generator);
    let namespace = match ctx.invoke_virtual(
        generator,
        "getClassLoader",
        "()Ljava/lang/ClassLoader;",
        &[],
    ) {
        Ok(Some(Value::Object(cl))) => Some(loader_namespace_for_object(ctx, cl)),
        _ => None,
    };
    ctx.unpin_native_roots(pin);
    namespace
}

fn native_spring_naming_policy_get_class_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Object(None)));
    };
    let candidate = ctx.invoke_virtual_bytecode_only(
        this,
        "getClassName",
        "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/Object;Lorg/springframework/cglib/core/Predicate;)Ljava/lang/String;",
        &args[1..],
    )?;
    let Some(Value::Object(Some(name_ref))) = candidate else {
        return Ok(candidate);
    };
    let Some(name) = ctx.read_string(name_ref) else {
        return Ok(candidate);
    };
    // Split `<base><n>` where `<n>` is the trailing decimal counter cglib
    // appends. Anything else (no trailing digits) is left alone.
    let digits = name.len() - name.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return Ok(candidate);
    }
    let (base, num) = name.split_at(name.len() - digits);
    let Ok(mut counter) = num.parse::<u64>() else {
        return Ok(candidate);
    };
    // Only advance past a name the loader CGLIB is about to define into has
    // already defined ITSELF. This used to ask `class_id_by_name`, i.e. "is
    // this name taken ANYWHERE in the VM", and the honest answer to that is
    // `true` for names that are perfectly free in the target loader.
    //
    // Spring AOT is the case that breaks: `CglibAopProxy` builds a proxy class
    // in the forked test loader at build time, then builds the same proxy again
    // in the compiled artifacts' child `DynamicClassLoader` at run time. On
    // HotSpot the second generation keeps `$$0` — nothing in the CHILD holds
    // that name — and `AbstractClassGenerator`'s `attemptLoad` then resolves it
    // through delegation to the build-time class, so the proxy is reused and no
    // second definition happens at all. Bumping it to `$$1` makes that load
    // miss, and the fresh proxy lands in the child loader, a different runtime
    // package from its superclass — where a package-private override is not an
    // override (JVMS 5.4.5) and `invokevirtual` keeps reaching the original
    // body. That was the whole of
    // `AotIntegrationTests.endToEndTestsForBeanOverrides`'s divergence:
    // `MockitoSpyBeanAndSpringAopProxyIntegrationTests$DateService.getDate` is
    // package-private, so the spy's stub was silently bypassed.
    //
    // The collision this shim exists for is unaffected: `cce_enhance` defines
    // its `$$SpringCGLIB$$<n>` into a namespace, and a name it minted is
    // therefore defined BY that namespace's loader, which is exactly what
    // `class_id_defined_by_loader_exact` reports.
    let Some(target_loader) = current_generator_loader_namespace(ctx, this) else {
        // No generator on the stack (`SpringNamingPolicyTests` calls
        // `getClassName` directly) — there is no target loader to test against,
        // so leave cglib's own answer alone, as HotSpot does.
        return Ok(candidate);
    };
    let mut chosen = name.clone();
    // Bounded: a runaway here would be worse than a collision.
    for _ in 0..1024 {
        if ctx
            .class_id_defined_by_loader_exact(&chosen.replace('.', "/"), target_loader)
            .is_none()
        {
            break;
        }
        counter += 1;
        chosen = format!("{base}{counter}");
    }
    if chosen == name {
        return Ok(candidate);
    }
    if crate::nbflags().dbg_ccecache {
        eprintln!("[CCECACHE-DBG] naming policy: {name} already defined -> {chosen}");
    }
    let out = ctx.create_string(&chosen);
    Ok(Some(Value::Object(Some(out))))
}

/// Spring's marker interface for a class loader that can define classes on
/// behalf of a caller (`publicDefineClass`) and can name the loader an
/// enhanced class should really land in (`getOriginalClassLoader`).
const SMART_CLASS_LOADER_IFACE: &str = "org/springframework/core/SmartClassLoader";

/// True when `class_id` — or any class/interface above it — is Spring's
/// `SmartClassLoader`.
fn implements_smart_class_loader(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
) -> bool {
    let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut stack = vec![class_id];
    while let Some(cid) = stack.pop() {
        if !seen.insert(cid.as_u32()) {
            continue;
        }
        if ctx.class_name_arc_of_id(cid).as_deref() == Some(SMART_CLASS_LOADER_IFACE) {
            return true;
        }
        for iface in ctx.class_interfaces(cid) {
            stack.push(iface);
        }
        if let Some(sup) = ctx.superclass_of(cid) {
            stack.push(sup);
        }
    }
    false
}

/// Port of `ConfigurationClassEnhancer.reliesOnPackageVisibility`: a config
/// class that is package-private, or that declares a package-private
/// constructor or `@Bean` method, can only be subclassed from its OWN loader's
/// runtime package — so the enhanced subclass must be defined there no matter
/// which loader the caller asked for.
fn relies_on_package_visibility(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
) -> bool {
    const ACC_PUBLIC: u16 = 0x0001;
    const ACC_PROTECTED: u16 = 0x0004;
    const VISIBLE: u16 = ACC_PUBLIC | ACC_PROTECTED;

    if ctx.class_access_flags(class_id) & VISIBLE == 0 {
        return true;
    }
    let methods = ctx.declared_methods(class_id);
    if methods
        .iter()
        .any(|m| m.name == "<init>" && m.access_flags & VISIBLE == 0)
    {
        return true;
    }
    // Only a package-private/private method can force the fallback, so the
    // (expensive) `@Bean` annotation walk is worth doing for those alone.
    let hidden: Vec<(String, String)> = methods
        .iter()
        .filter(|m| m.name != "<init>" && m.name != "<clinit>" && m.access_flags & VISIBLE == 0)
        .map(|m| (m.name.clone(), m.descriptor.clone()))
        .collect();
    hidden
        .into_iter()
        .any(|(name, descriptor)| method_carries_bean(ctx, class_id, &name, &descriptor))
}

/// CratonVM namespace id for a `ClassLoader` object, normalised so it can be
/// compared against `NativeContext::loader_id_of_class`.
///
/// `loader_namespace_id` answers `0` ("the global/application namespace") for
/// every BUILT-IN loader, while `loader_id_of_class` reports the application
/// loader as `2` — the two conventions have to be reconciled before an
/// equality test means anything. `None` is the bootstrap loader (`0` in both).
fn loader_namespace_for_object(ctx: &mut dyn NativeContext, cl: Option<ObjectRef>) -> u32 {
    match cl {
        Some(obj) => match crate::classloader::loader_namespace_id(ctx, obj) {
            0 => 2,
            id => id,
        },
        None => 0,
    }
}

/// Decide which loader namespace the enhanced subclass belongs in, mirroring
/// the arbitration `ConfigurationClassEnhancer.enhance` performs before it
/// hands the job to CGLIB.
///
/// Spring passes the loader it WANTS the proxy in; whether the proxy actually
/// lands there depends on three further questions, and this native replacement
/// for `enhance` has to answer all three itself (measured against HotSpot with
/// `probes/CceProbe.java`, which prints the real answer for each of the four
/// loaders `ConfigurationClassEnhancerTests` uses):
///
///  1. **Is it a different loader at all?** `classLoader == configClass
///     .getClassLoader()` is the common case and short-circuits everything.
///  2. **Does the loader name a different original?** A `SmartClassLoader`
///     answers `getOriginalClassLoader()`; Spring's own `OverridingClassLoader`
///     and the tests' `CustomSmartClassLoader` return their PARENT, which is
///     where the proxy then goes.
///  3. **Would the proxy see the same superclass there?** Package visibility
///     (`reliesOnPackageVisibility`) rules out a foreign loader outright, and
///     even for a public config class a loader that defines its OWN copy of the
///     superclass (exactly what `OverridingClassLoader` does) must not be used:
///     real CGLIB's define fails there and `createClass`'s fallback re-runs the
///     generation against the config class's own loader. Asking the loader to
///     `loadClass` the superclass name is the same question the JVM asks when
///     it resolves `super_class` at define time.
///
/// Returns the namespace id to define into. Nothing else has to be recorded:
/// `Class.getClassLoader()` already resolves a user-defined namespace id back
/// to its loader object (`native_class_get_class_loader`'s `loader_type >= 3`
/// arm), so defining into the namespace IS the attribution.
fn resolve_define_loader(
    ctx: &mut dyn NativeContext,
    super_class_id: cratonvm_types::ClassId,
    super_name: &str,
    super_loader_id: u32,
    requested: Value,
) -> u32 {
    let requested_obj = match requested {
        Value::Object(Some(obj)) => obj,
        _ => return super_loader_id,
    };
    let pin = ctx.pin_native_root(requested_obj);
    let mut cl = Some(ctx.read_native_pin(pin, requested_obj));
    let mut id = loader_namespace_for_object(ctx, cl);

    // (2) A SmartClassLoader gets to redirect to its "original" loader.
    if id != super_loader_id {
        if let Some(obj) = cl {
            let obj = ctx.read_native_pin(pin, obj);
            let cl_cid = ctx.class_id_of_object(obj);
            if implements_smart_class_loader(ctx, cl_cid) {
                let obj = ctx.read_native_pin(pin, obj);
                if let Ok(Some(Value::Object(orig))) = ctx.invoke_virtual(
                    obj,
                    "getOriginalClassLoader",
                    "()Ljava/lang/ClassLoader;",
                    &[],
                ) {
                    cl = orig;
                    id = loader_namespace_for_object(ctx, cl);
                }
            }
        }
    }

    // (3a) Package visibility pins the proxy to the config class's loader.
    if id != super_loader_id && relies_on_package_visibility(ctx, super_class_id) {
        ctx.unpin_native_roots(pin);
        return super_loader_id;
    }

    // (3b) A loader that would resolve the superclass to a DIFFERENT class
    // cannot host the proxy.
    if id != super_loader_id {
        if let Some(obj) = cl {
            let obj = ctx.read_native_pin(pin, obj);
            let dotted = super_name.replace('/', ".");
            let name_obj = ctx.create_string(&dotted);
            let obj = ctx.read_native_pin(pin, obj);
            let resolved = ctx.invoke_virtual(
                obj,
                "loadClass",
                "(Ljava/lang/String;)Ljava/lang/Class;",
                &[Value::Object(Some(name_obj))],
            );
            let same = match resolved {
                Ok(Some(Value::Object(Some(mirror)))) => {
                    crate::lang_class::mirror_class_id(ctx, mirror) == Some(super_class_id)
                }
                _ => false,
            };
            ctx.unpin_native_roots(pin);
            return if same { id } else { super_loader_id };
        }
    }

    ctx.unpin_native_roots(pin);
    // A null "original" loader (bootstrap) is not a namespace this enhancer
    // can define into — keep the config class's own loader.
    if cl.is_some() {
        id
    } else {
        super_loader_id
    }
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

    let super_name = match ctx.class_name_of_id(super_class_id) {
        Some(n) => n,
        None => {
            return Ok(Some(Value::Object(Some(cls_mirror))));
        }
    };
    let super_loader_id = ctx.loader_id_of_class(super_class_id) as u32;

    // `enhance(configClass, classLoader)`'s third argument is not advisory:
    // a public config class asked for in a foreign loader really is defined
    // THERE, and `enhancedClass.getClassLoader()` is asserted on
    // (`ConfigurationClassEnhancerTests.withPublicClass`). Everything this
    // native has to weigh before agreeing to that is in
    // `resolve_define_loader`; the answer then keys the name counter, the
    // class cache and the define itself, all of which are per-namespace.
    let requested_loader = args.get(2).cloned().unwrap_or(Value::Object(None));
    let mirror_pin = ctx.pin_native_root(cls_mirror);
    let define_loader_id = resolve_define_loader(
        ctx,
        super_class_id,
        &super_name,
        super_loader_id,
        requested_loader,
    );
    let cls_mirror = ctx.read_native_pin(mirror_pin, cls_mirror);
    ctx.unpin_native_roots(mirror_pin);
    let cache_key = (define_loader_id, super_name.clone());

    // Real CGLIB caches the generated proxy class per superclass and
    // returns the SAME `Class` on a repeat `enhance()` call instead of
    // generating a fresh numbered subclass every time — see
    // `config_enhancer_class_cache`'s doc comment (key shape: keyed by
    // `(defining_loader_id, super_internal_name)`, NOT the recyclable
    // `ClassId` alone).
    let cached = config_enhancer_class_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&cache_key)
        .cloned();
    if crate::nbflags().dbg_ccecache {
        eprintln!(
            "[CCECACHE-DBG] enhance called: super_class_id={:?} cache_key={:?} receiver_loader_id={} hit={}",
            super_class_id,
            cache_key,
            receiver_loader_id,
            cached.is_some(),
        );
    }
    if let Some((cached_id, cached_name, cached_bytes)) = cached {
        notify_generated_class_handler(ctx, receiver_loader_id, &cached_name, &cached_bytes);
        let mirror = ctx.get_class_mirror(cached_id);
        return Ok(Some(Value::Object(Some(mirror))));
    }

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
    // Real CGLIB's `Enhancer.filterConstructors` (via `VisibilityPredicate`)
    // drops every non-visible declared constructor and throws
    // `IllegalArgumentException("No visible constructors in " + sc)` when
    // NONE remain (SpringApplicationTests.sourcesMustBeAccessible: a
    // `@Configuration` class with only a `private` constructor). This
    // fast-path bytecode generator skips real CGLIB entirely, so it must
    // replicate that same guard itself — without it, `build_enhancer_class`
    // silently emitted a proxy with zero delegating constructors and
    // `SpringApplication.run()` never threw.
    //
    // The test expects `BeanDefinitionStoreException` with this
    // `IllegalArgumentException` as its ROOT cause, not the bare
    // `IllegalArgumentException` itself — construct + wrap it explicitly
    // (`crate::spring_startup_bootstrap::wrap_as_bean_definition_store_exception`,
    // the same helper `m5_abstract_bean_factory_resolve_bean_class_with_name`
    // uses for an analogous "we replaced the real bytecode, so we must
    // replicate its catch-and-wrap too" case) rather than a bare
    // `RuntimeError::IllegalArgumentException`, which would propagate
    // unwrapped.
    if ctor_descriptors.is_empty() {
        let message = format!("No visible constructors in class {super_name}");
        let message_str = ctx.create_string(&message);
        let cause = match ctx.new_object_initialized(
            "java/lang/IllegalArgumentException",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(message_str))],
        ) {
            Ok(Some(Value::Object(Some(cause)))) => cause,
            _ => {
                return Err(RuntimeError::IllegalArgumentException { message }.into());
            }
        };
        return Err(
            crate::spring_startup_bootstrap::wrap_as_bean_definition_store_exception(
                ctx,
                None,
                &super_name,
                cause,
            ),
        );
    }
    // The generated `$$SpringCGLIB$$<n>` suffix is numbered per
    // `(super_loader_id, super_internal_name)` by
    // `next_config_enhancer_counter`, so the FIRST enhancement of a given
    // `@Configuration` class under a given loader is always `$$0` — which
    // several tests hardcode. Define it into THAT loader, not unconditionally
    // into the application loader (0): real cglib defines its subclass into the
    // superclass's own loader, and a per-loader counter is only collision-free
    // inside a per-loader namespace.
    //
    // Under `@CompileWithForkedClassLoader` the same `@Configuration` class is
    // re-loaded by a fresh child loader for each test method, so the second,
    // genuinely distinct `CglibConfiguration` correctly missed the cache,
    // correctly drew `$$0` from its own fresh counter — and then collided with
    // the FIRST loader's `$$0` in the application loader's single flat
    // namespace (`IncompatibleClassChangeError: ... already defined by
    // application loader`). The old code answered that collision by returning
    // the ORIGINAL, UN-enhanced class, which surfaced downstream as
    // `IllegalArgumentException: class ...CglibConfiguration is not an enhanced
    // class` from `Enhancer.registerStaticCallbacks` during AOT replay, and as
    // a `@Bean` body running twice ("Hello1" instead of "Hello0") when the
    // un-enhanced config class was instantiated directly. Those are exactly the
    // two long-standing `ApplicationContextAotGeneratorTests
    // $ConfigurationClassCglibProxy` cross-test residuals: both pass in
    // isolation (one loader, no collision) and failed 100% deterministically in
    // the full 40-method class run.
    let mut attempt = 0u32;
    let (mut new_name, mut bytes) = build_enhancer_class(
        define_loader_id,
        &super_name,
        &bean_methods,
        &ctor_descriptors,
    );

    let (cid, new_name, bytes_arc) = loop {
        let opts = DefineClassFull {
            override_name: Some(new_name.clone()),
            skip_verification: true,
            ..Default::default()
        };
        let bytes_arc = std::sync::Arc::new(std::mem::take(&mut bytes));
        match ctx.define_class_full(&new_name, &bytes_arc, define_loader_id, opts) {
            Ok(cid) => break (cid, new_name, bytes_arc),
            // Belt and braces for any remaining way the name can already be
            // taken in the target namespace (a loader that delegates the name to
            // a parent; a cache entry the class registry outlived; …): burn the
            // counter and try the next suffix rather than hand back an
            // un-enhanced class, which is always wrong. A differently numbered
            // but genuinely enhanced class still satisfies every behavioural
            // assertion — only assertions over generated *source text* care
            // about the number, and the per-loader define above is what keeps
            // those at `$$0`.
            Err(msg) if attempt < 16 && msg.contains("already defined") => {
                attempt += 1;
                let (retry_name, retry_bytes) = build_enhancer_class(
                    define_loader_id,
                    &super_name,
                    &bean_methods,
                    &ctor_descriptors,
                );
                eprintln!(
                    "[CCE] enhance: {new_name} already defined in loader {define_loader_id} — retrying as {retry_name}",
                );
                new_name = retry_name;
                bytes = retry_bytes;
            }
            Err(msg) => {
                eprintln!(
                    "[CCE] enhance: define_class_full failed for {new_name}: {msg} — fallback to identity",
                );
                return Ok(Some(Value::Object(Some(cls_mirror))));
            }
        }
    };

    config_enhancer_class_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(cache_key, (cid, new_name.clone(), bytes_arc.clone()));
    let mirror = ctx.get_class_mirror(cid);
    eprintln!(
        "[CCE] enhance: defined {new_name} (super={super_name}, loader={define_loader_id}, marker={SPRING_MARKER_IFACE}, intercepted @Bean methods={})",
        bean_methods.len(),
    );
    notify_generated_class_handler(ctx, receiver_loader_id, &new_name, &bytes_arc);
    // See `build_fastclass_placeholder`'s doc comment: real cglib always emits
    // two FastClass helper classes alongside the enhancer itself; our dispatch
    // never needs them, but AOT's `isRegisteredCglibClass` test asserts their
    // presence (bytes + reflection hint) regardless, and
    // `notify_generated_class_handler` is a no-op outside AOT processing (no
    // handler installed), so this is safe to call unconditionally.
    for suffix in ["FastClass$$0", "FastClass$$1"] {
        let fastclass_name = format!("{super_name}$$SpringCGLIB$${suffix}");
        let fastclass_bytes = build_fastclass_placeholder(&fastclass_name);
        notify_generated_class_handler(ctx, receiver_loader_id, &fastclass_name, &fastclass_bytes);
    }
    Ok(Some(Value::Object(Some(mirror))))
}

// ===========================================================================
// FactoryBean-enhancement (SPR-6602/11202/15275) proxy generator.
//
// Real Spring's `ConfigurationClassEnhancer$BeanMethodInterceptor
// .resolveBeanReference`, after resolving an inter-`@Bean`-method
// reference that turns out to be `FactoryBean`-typed, calls
// `enhanceFactoryBean(rawFactory, exposedType, beanFactory, beanName)`
// before handing the result back to the caller. Without this, a raw
// `factory.getObject()` call made directly on that reference (either from
// generated proxy bytecode elsewhere, or — the common case — from the
// user's OWN `@Configuration` class body, e.g. `foo().getObject()`)
// bypasses the container's `factoryBeanObjectCache` entirely and computes
// a fresh, non-singleton-matching product every time.
//
// `enhanceFactoryBean` picks one of three representations based on the
// raw factory's *actual runtime* finality and the `@Bean` method's
// *declared* return type (`exposedType`):
//   * `exposedType` is final or `getObject()` is final, and `exposedType`
//     is NOT an interface: no proxy is possible at all — return the raw
//     factory unchanged (matches real Spring's own fallback).
//   * final class/method, `exposedType` IS an interface: a real JDK
//     dynamic proxy implementing just that one interface, whose
//     `InvocationHandler` intercepts only `getObject()`.
//   * otherwise: a field-copying CGLIB-style subclass of the factory's own
//     concrete class, overriding only `getObject()`.
// In both proxy cases every OTHER method (`isSingleton`'s interface
// default, `getObjectType`, custom methods, `equals`/`hashCode`/
// `toString`) delegates straight through to the raw factory's own real
// dispatch — exactly like real Spring's CGLIB callback / interface-proxy
// `InvocationHandler`, which intercept `getObject` alone.
// ===========================================================================

/// Cache of built subclass-wrapper class names, keyed by the raw factory's
/// concrete `ClassId`. Built once per concrete class (the wrapper has no
/// per-instance state baked into its bytecode — `$$fbBeanFactory`/
/// `$$fbBeanName` are plain fields set post-allocation), reused for every
/// subsequent wrap of an instance of that same class.
fn fb_subclass_cache() -> &'static Mutex<HashMap<u32, String>> {
    static CACHE: OnceLock<Mutex<HashMap<u32, String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Synthetic `InvocationHandler` class backing the JDK-interface-proxy
/// FactoryBean wrapper (`build_or_get_factory_bean_interface_proxy`).
/// Fields: 0 = raw factory (Object), 1 = beanFactory (Object), 2 = bean
/// name (String, Object-typed field). Dispatched to via `fb_handler_invoke`,
/// registered as a plain native on `(FB_HANDLER_CLASS, "invoke", ...)` —
/// picked up automatically by the generic proxy-dispatch path
/// (`invoke_or_native` in `vm/src/vm/vm_exec.rs`, which resolves the
/// handler's declaring class BY NAME and checks the native registry
/// before falling back to real bytecode), so no VM-side special-casing is
/// needed — the same mechanism every other native-registered synthetic
/// class in this codebase already relies on.
const FB_HANDLER_CLASS: &str = "cratonvm/internal/FactoryBeanEnhancerHandler";
const FB_HANDLER_FIELD_RAW_FACTORY: usize = 0;
const FB_HANDLER_FIELD_BEAN_FACTORY: usize = 1;
const FB_HANDLER_FIELD_BEAN_NAME: usize = 2;

/// Collect the distinct `getObject`-named, 0-arg method signatures declared
/// anywhere in `class_id`'s hierarchy (most-derived class first; a
/// covariant-return override and the compiler-synthesized erasure bridge
/// are both collected, deduplicated by descriptor), plus whether the most
/// specific one (preferring a non-bridge, i.e. non-`()Ljava/lang/Object;`,
/// descriptor when one exists) is `final`. Mirrors real Spring's
/// `clazz.getMethod("getObject").getModifiers()` check, which reflection's
/// own bridge-suppression logic resolves to the covariant override when
/// both exist.
fn factory_bean_getobject_signatures(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
) -> (Vec<(String, String)>, bool) {
    const ACC_FINAL: u16 = 0x0010;
    let mut out: Vec<(String, String, u16)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut cur = Some(class_id);
    while let Some(cid) = cur {
        for m in ctx.declared_methods(cid) {
            if m.name != "getObject" || !m.descriptor.starts_with("()") {
                continue;
            }
            if seen.insert(m.descriptor.clone()) {
                out.push((m.name.clone(), m.descriptor.clone(), m.access_flags));
            }
        }
        cur = ctx.superclass_of(cid);
    }
    let method_final = out
        .iter()
        .find(|(_, d, _)| d.as_str() != "()Ljava/lang/Object;")
        .or_else(|| out.first())
        .map(|(_, _, af)| af & ACC_FINAL != 0)
        .unwrap_or(false);
    (
        out.into_iter().map(|(n, d, _)| (n, d)).collect(),
        method_final,
    )
}

/// Field-copying CGLIB-style subclass wrapper for a non-final FactoryBean:
/// extends the raw factory's own concrete class, overriding every declared
/// `getObject()`-named 0-arg signature (the real covariant method and any
/// generics bridge) to delegate to `beanFactory.getBean(beanName)` instead
/// of running the real body — mirrors real Spring's
/// `createCglibProxyForFactoryBean`.
///
/// Real CGLIB (via Objenesis) instantiates the proxy WITHOUT calling any
/// constructor, then copies every declared instance field (through the
/// whole superclass chain) from the original instance into the proxy by
/// reflection, so the inherited real methods it does NOT override observe
/// identical state. This reimplementation does the same via
/// `ctx.allocate_instance` (no `<init>` call at all — sidesteps arbitrary
/// or absent constructor descriptors and inner-class outer-instance
/// capture entirely, same trick as real CGLIB's Objenesis path) plus a
/// native field-copy loop keyed by each field's already-known heap slot
/// index (`FieldMetadata::slot_index`, identical on both objects since the
/// wrapper only APPENDS its own two new fields after the inherited ones).
fn build_factory_bean_subclass_wrapper(
    ctx: &mut dyn NativeContext,
    concrete_cid: cratonvm_types::ClassId,
    raw_factory: cratonvm_types::ObjectRef,
    bean_factory: cratonvm_types::ObjectRef,
    bean_name: &str,
) -> Option<cratonvm_types::ObjectRef> {
    let cached = fb_subclass_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&concrete_cid.as_u32())
        .cloned();
    let wrapper_name = match cached {
        Some(name) => name,
        None => {
            let concrete_name = ctx.class_name_of_id(concrete_cid)?;
            let (signatures, _) = factory_bean_getobject_signatures(ctx, concrete_cid);
            if signatures.is_empty() {
                return None;
            }
            let counter = ENHANCER_COUNTER.fetch_add(1, Ordering::Relaxed);
            let new_name = format!("{concrete_name}$$SpringCGLIB$$FB{counter:x}");

            let mut cw = ClassWriter::new();
            let this_class_idx = cw.add_class(&new_name);
            let super_class_idx = cw.add_class(&concrete_name);
            let code_attr_name_idx = cw.add_utf8("Code");

            let bf_field_name_idx = cw.add_utf8("$$fbBeanFactory");
            let object_desc_idx = cw.add_utf8("Ljava/lang/Object;");
            let bf_field_ref =
                cw.add_fieldref(this_class_idx, "$$fbBeanFactory", "Ljava/lang/Object;");
            let bn_field_name_idx = cw.add_utf8("$$fbBeanName");
            let bn_field_ref =
                cw.add_fieldref(this_class_idx, "$$fbBeanName", "Ljava/lang/Object;");

            let beanfactory_cast_idx =
                cw.add_class("org/springframework/beans/factory/BeanFactory");
            let getbean_ref = cw.add_interface_methodref(
                beanfactory_cast_idx,
                "getBean",
                "(Ljava/lang/String;)Ljava/lang/Object;",
            );
            let string_cls = cw.add_class("java/lang/String");

            let b = |x: u16| -> [u8; 2] { x.to_be_bytes() };
            let mut methods = Vec::new();
            for (name, descriptor) in &signatures {
                let name_idx = cw.add_utf8(name);
                let desc_idx = cw.add_utf8(descriptor);
                let ret = descriptor.split(')').nth(1).unwrap_or("Ljava/lang/Object;");
                let ret_internal = if ret.starts_with('L') && ret.ends_with(';') {
                    ret[1..ret.len() - 1].to_string()
                } else {
                    // A non-reference getObject() return can't happen for a
                    // real FactoryBean (its type parameter always erases to
                    // a reference), but fall back to Object rather than
                    // emit an invalid checkcast target if it ever does.
                    "java/lang/Object".to_string()
                };
                let ret_class_idx = cw.add_class(&ret_internal);

                let mut code: Vec<u8> = Vec::new();
                code.push(0x2A); // aload_0
                code.push(0xB4); // getfield $$fbBeanFactory
                code.extend_from_slice(&b(bf_field_ref));
                code.push(0xC0); // checkcast BeanFactory
                code.extend_from_slice(&b(beanfactory_cast_idx));
                code.push(0x2A); // aload_0
                code.push(0xB4); // getfield $$fbBeanName
                code.extend_from_slice(&b(bn_field_ref));
                code.push(0xC0); // checkcast String
                code.extend_from_slice(&b(string_cls));
                code.push(0xB9); // invokeinterface BeanFactory.getBean(String)Object
                code.extend_from_slice(&b(getbean_ref));
                code.push(0x02);
                code.push(0x00);
                code.push(0xC0); // checkcast <Ret>
                code.extend_from_slice(&b(ret_class_idx));
                code.push(0xB0); // areturn

                methods.push(wrap_method(
                    name_idx,
                    desc_idx,
                    code_attr_name_idx,
                    &code,
                    2,
                    1,
                ));
            }

            let bf_field = emit_bean_factory_field(bf_field_name_idx, object_desc_idx);
            let bn_field = {
                let mut field = Vec::new();
                field.extend_from_slice(&(0x0002u16 | 0x1000u16).to_be_bytes()); // ACC_PRIVATE | ACC_SYNTHETIC
                field.extend_from_slice(&bn_field_name_idx.to_be_bytes());
                field.extend_from_slice(&object_desc_idx.to_be_bytes());
                field.extend_from_slice(&0u16.to_be_bytes());
                field
            };

            let access_flags: u16 = 0x0001 | 0x0020 | 0x1000; // PUBLIC | SUPER | SYNTHETIC
            let bytes = cw.finish(
                access_flags,
                this_class_idx,
                super_class_idx,
                &[],
                &[bf_field, bn_field],
                &methods,
            );

            let opts = DefineClassFull {
                override_name: Some(new_name.clone()),
                skip_verification: true,
                ..Default::default()
            };
            // Define into the CONCRETE class's own loader, like `cce_enhance`
            // above (see its `super_loader_id` comment and
            // `configproxy-cglib-loaderid-fixed-20260727.md`):
            // a generated subclass has to sit in its superclass's runtime
            // package `(defining loader, package name)` or `same_runtime_package`
            // refuses every package-private override it declares. Hardcoding
            // the application loader only worked because the decode collapsed
            // `Application` and `UserDefined(2)`; for a genuinely fork-loaded
            // `FactoryBean` it put the wrapper in the wrong namespace outright.
            // No-op for the common app-loaded case (id 2 -> `Application`).
            let concrete_loader_id = ctx.loader_id_of_class(concrete_cid).max(0) as u32;
            match ctx.define_class_full(
                &new_name,
                &std::sync::Arc::new(bytes),
                concrete_loader_id,
                opts,
            ) {
                Ok(_cid) => {
                    fb_subclass_cache()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(concrete_cid.as_u32(), new_name.clone());
                    new_name
                }
                Err(msg) => {
                    eprintln!(
                        "[CCE] enhanceFactoryBeanReference: define_class_full failed for {new_name}: {msg}"
                    );
                    return None;
                }
            }
        }
    };

    // `allocate_instance`/`create_string` below can both allocate and
    // trigger GC, which may relocate `raw_factory`/`bean_factory` (moving-GC
    // safety — mirrors `wrap_annotation_in_real_proxy`'s own pin/read/unpin
    // idiom): pin both up front and re-read them via the pin after every
    // subsequent allocating call, never trusting the plain `ObjectRef`
    // variable across one.
    let pin_base = ctx.pin_native_root(raw_factory);
    ctx.pin_native_root(bean_factory);

    let new_obj = ctx.allocate_instance(&wrapper_name)?;
    let new_obj_pin = ctx.pin_native_root(new_obj);
    let raw_factory = ctx.read_native_pin(pin_base, raw_factory);

    // Every instance field the concrete class (and its ancestors) declares
    // lives at the SAME heap slot index on both objects — the wrapper
    // subclass only ever APPENDS its own two new fields after them.
    // `get_field`/`set_field` are plain reads/writes (no allocation), so
    // `new_obj`/`raw_factory` stay valid for the whole loop.
    let mut cur = Some(concrete_cid);
    while let Some(cid) = cur {
        for f in ctx.declared_fields(cid) {
            if f.is_static {
                continue;
            }
            let val = ctx.get_field(raw_factory, f.slot_index);
            ctx.set_field(new_obj, f.slot_index, val);
        }
        cur = ctx.superclass_of(cid);
    }
    let bean_factory = ctx.read_native_pin(pin_base + 1, bean_factory);
    ctx.set_field_by_name(
        new_obj,
        "$$fbBeanFactory",
        Value::Object(Some(bean_factory)),
    );

    let name_str = ctx.create_string(bean_name);
    let new_obj = ctx.read_native_pin(new_obj_pin, new_obj);
    ctx.set_field_by_name(new_obj, "$$fbBeanName", Value::Object(Some(name_str)));
    ctx.unpin_native_roots(pin_base);
    Some(new_obj)
}

/// Real JDK dynamic-proxy FactoryBean wrapper for a final class / final
/// `getObject()`, used whenever `exposedType` is an interface (mirrors real
/// Spring's `createInterfaceProxyForFactoryBean`). Implements just the one
/// `exposed_type_cid` interface (not every interface the raw factory
/// happens to implement) via the same real-`$ProxyN`-class machinery the
/// rest of this VM's `Proxy.newProxyInstance`/real-annotation-proxy support
/// already uses (`define_or_get_proxy_class`) — see
/// `wrap_annotation_in_real_proxy` for the precedent this mirrors.
fn build_or_get_factory_bean_interface_proxy(
    ctx: &mut dyn NativeContext,
    exposed_type_cid: cratonvm_types::ClassId,
    raw_factory: cratonvm_types::ObjectRef,
    bean_factory: cratonvm_types::ObjectRef,
    bean_name: &str,
) -> Option<cratonvm_types::ObjectRef> {
    let proxy_cid = match crate::define_or_get_proxy_class(ctx, 0, &[exposed_type_cid]) {
        crate::ProxyClassOutcome::Real(cid) => cid,
        _ => return None,
    };
    let exposed_mirror = ctx.get_class_mirror(exposed_type_cid);

    // Every step below except the final field/array writes can allocate and
    // trigger a moving GC (`ensure_synthetic_class`/`alloc_object`/
    // `create_string`/`new_ref_array`) — pin every live `ObjectRef` up front
    // and re-read it via its pin after each such call, never trusting a
    // plain variable across one. Mirrors `wrap_annotation_in_real_proxy`'s
    // own pin/read/unpin idiom.
    let pin_base = ctx.pin_native_root(raw_factory);
    ctx.pin_native_root(bean_factory);
    ctx.pin_native_root(exposed_mirror);

    // Fallible since 2026-08-10 (JDK-only wave 2, step 3), and the refusal is
    // ABSORBED here rather than propagated, deliberately: this function's
    // failure channel is `None`, which it already answers when
    // `define_or_get_proxy_class` cannot build a real proxy, and the caller's
    // response is to leave the factory bean unwrapped. A refused
    // `FB_HANDLER_CLASS` gets that same answer instead of a fabrication. The
    // violation is recorded upstream by `admit_compatibility_class` either way,
    // so strict mode still reports the request; what changes is that it stops
    // continuing in the state the contract forbids.
    let handler_cid = match ctx.try_ensure_synthetic_class(FB_HANDLER_CLASS, 3) {
        Ok(cid) => cid,
        Err(_) => {
            ctx.unpin_native_roots(pin_base);
            return None;
        }
    };
    let handler = ctx.alloc_object(handler_cid, 3);
    let handler_pin = ctx.pin_native_root(handler);
    let raw_factory = ctx.read_native_pin(pin_base, raw_factory);
    let bean_factory = ctx.read_native_pin(pin_base + 1, bean_factory);
    ctx.set_field(
        handler,
        FB_HANDLER_FIELD_RAW_FACTORY,
        Value::Object(Some(raw_factory)),
    );
    ctx.set_field(
        handler,
        FB_HANDLER_FIELD_BEAN_FACTORY,
        Value::Object(Some(bean_factory)),
    );

    let name_str = ctx.create_string(bean_name);
    let handler = ctx.read_native_pin(handler_pin, handler);
    ctx.set_field(
        handler,
        FB_HANDLER_FIELD_BEAN_NAME,
        Value::Object(Some(name_str)),
    );

    let n = ctx.class_num_total_fields(proxy_cid).max(3);
    let real = ctx.alloc_object(proxy_cid, n);
    let real_pin = ctx.pin_native_root(real);
    let handler = ctx.read_native_pin(handler_pin, handler);
    ctx.set_field(real, 0, Value::Object(Some(handler)));

    let iface_arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 1);
    let real = ctx.read_native_pin(real_pin, real);
    let exposed_mirror = ctx.read_native_pin(pin_base + 2, exposed_mirror);
    ctx.set_array_element(iface_arr, 0, Value::Object(Some(exposed_mirror)));
    ctx.set_field(real, 1, Value::Object(Some(iface_arr)));
    ctx.set_field(real, 2, Value::Int(0));
    ctx.unpin_native_roots(pin_base);
    Some(real)
}

/// `cratonvm/internal/FactoryBeanEnhancerHandler.invoke(Object, Method,
/// Object[])Object` — the `InvocationHandler.invoke` body for the
/// JDK-interface-proxy FactoryBean wrapper. `getObject()` (any 0-arg
/// descriptor — FactoryBean's only such member) delegates to
/// `beanFactory.getBean(beanName)`, resolving the container's CACHED
/// product. Every other method delegates straight to the raw factory
/// instance via real virtual dispatch.
fn fb_handler_invoke(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(handler))) = args.first().copied() else {
        return Ok(Some(Value::Object(None)));
    };
    let method_obj = match args.get(2) {
        Some(Value::Object(Some(m))) => Some(*m),
        _ => None,
    };
    let method_name = match method_obj {
        Some(m) => match crate::lang_class::method_name_value(ctx, m) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        },
        None => String::new(),
    };
    let raw_factory = match ctx.get_field(handler, FB_HANDLER_FIELD_RAW_FACTORY) {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(Value::Object(None))),
    };

    if method_name == "getObject" {
        let bean_factory = match ctx.get_field(handler, FB_HANDLER_FIELD_BEAN_FACTORY) {
            Value::Object(Some(o)) => o,
            _ => return Ok(Some(Value::Object(Some(raw_factory)))),
        };
        let bean_name_obj = match ctx.get_field(handler, FB_HANDLER_FIELD_BEAN_NAME) {
            Value::Object(Some(o)) => o,
            _ => return Ok(Some(Value::Object(Some(raw_factory)))),
        };
        return ctx.invoke_virtual(
            bean_factory,
            "getBean",
            "(Ljava/lang/String;)Ljava/lang/Object;",
            &[Value::Object(Some(bean_name_obj))],
        );
    }

    // Every other call (isSingleton's interface default, getObjectType,
    // custom methods, equals/hashCode/toString) delegates straight to the
    // raw factory's own real dispatch — mirrors real Spring's CGLIB
    // callback / interface-proxy handler, which intercept getObject alone.
    // The descriptor MUST come from the mirror's `parameterTypes`/`returnType`
    // (via `method_descriptor_for_invoke`), never from the `signature` field:
    // `Method.signature` holds the *generic* signature and is null for every
    // non-generic method, so the old read fell through to a hardcoded
    // `()Ljava/lang/Object;` default and made `isSingleton()Z` /
    // `getObjectType()Ljava/lang/Class;` dispatch as `()Ljava/lang/Object;` —
    // `NoSuchMethodError: <rawFactory>.isSingleton()Ljava/lang/Object;` out of
    // the proxy body (Spr15275 `withAbstractFactoryBean`,
    // `withAbstractFactoryBeanForInterface`, `withFinalFactoryBean`).
    let descriptor = match method_obj {
        Some(m) => {
            let d = crate::lang_class::method_descriptor_for_invoke(&*ctx, m);
            if d.is_empty() {
                "()Ljava/lang/Object;".to_string()
            } else {
                d
            }
        }
        None => "()Ljava/lang/Object;".to_string(),
    };
    let call_args: Vec<Value> = match args.get(3) {
        Some(Value::Object(Some(arr))) => {
            let len = ctx.array_length(*arr);
            (0..len).map(|i| ctx.get_array_element(*arr, i)).collect()
        }
        _ => Vec::new(),
    };
    let result = ctx.invoke_virtual(raw_factory, &method_name, &descriptor, &call_args)?;

    // `InvocationHandler.invoke` is declared to return `Object`, and the JDK's
    // generated proxy body immediately `checkcast`s that result to the boxed
    // wrapper before unboxing it (`checkcast java/lang/Boolean; invokevirtual
    // booleanValue()Z` for an `isSingleton()Z` proxy method). Handing back the
    // raw primitive `Value` the delegated call produced aborted the VM with
    // "checkcast: not an object reference (got Int(1)) at
    // jdk/proxy2/$Proxy23.isSingleton()Z" — box it to match the contract, and
    // map a `void` delegate to `null`.
    let ret_desc = descriptor
        .rfind(')')
        .map(|i| descriptor[i + 1..].to_string())
        .unwrap_or_default();
    let boxed = match ret_desc.as_str() {
        "V" => Some(Value::Object(None)),
        "Z" | "B" | "C" | "S" | "I" | "J" | "F" | "D" => match result {
            Some(v @ (Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_))) => {
                Some(crate::lang_class::box_value(ctx, v, &ret_desc))
            }
            other => other,
        },
        _ => result,
    };
    Ok(boxed)
}

/// `cratonvm/internal/ConfigEnhancerSupport.enhanceFactoryBeanReference`
/// invokestatic target spliced into `emit_bean_override`'s generated
/// bytecode (see `fb_ref`'s doc comment there). Mirrors real Spring's
/// `ConfigurationClassEnhancer$BeanMethodInterceptor.enhanceFactoryBean`:
/// picks a subclass wrapper, an interface-proxy wrapper, or the identity
/// fallback based on the raw factory's actual runtime finality and
/// whether the `@Bean` method's declared return type (`exposedType`) is an
/// interface.
fn enhance_factory_bean_reference(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let raw_factory = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(args.first().cloned()),
    };
    let bean_factory = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(Some(raw_factory)))),
    };
    // `emit_bean_override` passes the SAME `beanName` local it just used for
    // the raw-factory `getBean("&name")` lookup — for a FactoryBean-typed
    // method that is always "&"-prefixed (see `beanname_lookup_idx`/
    // `fq_beanname_lookup_idx` in `build_enhancer_class`). Both builders
    // below need the PLAIN name instead: they call `beanFactory.getBean
    // (beanName)` (no "&") to resolve the container's cached PRODUCT, not
    // the raw factory again — passing the "&"-prefixed name through
    // unchanged made `getObject()` return the raw factory a second time,
    // which then failed its checkcast to the product type at the original
    // call site (e.g. `BarFactory cannot be cast to Bar`).
    let bean_name = match args.get(2) {
        Some(Value::Object(Some(s))) => {
            let raw = ctx.read_string(*s).unwrap_or_default();
            raw.strip_prefix('&').unwrap_or(&raw).to_string()
        }
        _ => return Ok(Some(Value::Object(Some(raw_factory)))),
    };
    let exposed_type_internal = match args.get(3) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(Some(raw_factory)))),
    };

    const ACC_FINAL: u16 = 0x0010;
    let concrete_cid = ctx.class_id_of_object(raw_factory);
    let class_final = ctx.class_access_flags(concrete_cid) & ACC_FINAL != 0;
    let (sigs, method_final) = factory_bean_getobject_signatures(ctx, concrete_cid);
    let needs_interface_proxy = class_final || method_final;
    if crate::nbflags().dbg_fbref {
        eprintln!(
            "[FBREF] enter bean_name={bean_name} exposed_type={exposed_type_internal} concrete={:?} class_final={class_final} method_final={method_final} sigs={:?} needs_interface_proxy={needs_interface_proxy}",
            ctx.class_name_of_id(concrete_cid),
            sigs,
        );
    }

    // `resolve_or_load_class_id` can force-load (and run the `<clinit>` of)
    // `exposed_type_internal`, an arbitrary allocation/GC opportunity — pin
    // both live objects across it so neither builder below is ever handed a
    // stale post-GC `ObjectRef`. `build_factory_bean_subclass_wrapper` /
    // `build_or_get_factory_bean_interface_proxy` additionally pin/refresh
    // internally around their OWN allocating calls; re-pinning an
    // already-current reference here is cheap and just belt-and-suspenders.
    let pin_base = ctx.pin_native_root(raw_factory);
    ctx.pin_native_root(bean_factory);

    if needs_interface_proxy {
        let exposed_id = resolve_or_load_class_id(ctx, &exposed_type_internal);
        let raw_factory = ctx.read_native_pin(pin_base, raw_factory);
        let bean_factory = ctx.read_native_pin(pin_base + 1, bean_factory);
        ctx.unpin_native_roots(pin_base);
        if let Some(exposed_cid) = exposed_id {
            if ctx.is_interface_class(exposed_cid) {
                if let Some(wrapped) = build_or_get_factory_bean_interface_proxy(
                    ctx,
                    exposed_cid,
                    raw_factory,
                    bean_factory,
                    &bean_name,
                ) {
                    return Ok(Some(Value::Object(Some(wrapped))));
                }
            }
        }
        // Final class/method with a non-interface (or unresolvable) exposed
        // type: no proxy possible — mirrors real Spring's own fallback.
        if crate::nbflags().dbg_fbref {
            eprintln!("[FBREF] identity fallback (no interface proxy possible)");
        }
        return Ok(Some(Value::Object(Some(raw_factory))));
    }

    let raw_factory = ctx.read_native_pin(pin_base, raw_factory);
    let bean_factory = ctx.read_native_pin(pin_base + 1, bean_factory);
    ctx.unpin_native_roots(pin_base);
    let subclass_result = build_factory_bean_subclass_wrapper(
        ctx,
        concrete_cid,
        raw_factory,
        bean_factory,
        &bean_name,
    );
    if crate::nbflags().dbg_fbref {
        let class_name = subclass_result.map(|o| ctx.class_name_of_id(ctx.class_id_of_object(o)));
        eprintln!(
            "[FBREF] subclass wrapper: present={} class={:?}",
            subclass_result.is_some(),
            class_name
        );
    }
    match subclass_result {
        Some(wrapped) => Ok(Some(Value::Object(Some(wrapped)))),
        None => Ok(Some(Value::Object(Some(raw_factory)))),
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
    // See `native_spring_naming_policy_get_class_name`: keeps real cglib's
    // generated names from colliding with the ones `cce_enhance` mints outside
    // cglib's own reserved-name bookkeeping.
    registry.register(
        "org/springframework/cglib/core/SpringNamingPolicy",
        "getClassName",
        "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/Object;Lorg/springframework/cglib/core/Predicate;)Ljava/lang/String;",
        native_spring_naming_policy_get_class_name,
    );
    // FactoryBean-enhancement (SPR-6602/11202/15275) — see the module doc
    // comment above `fb_subclass_cache`. `enhanceFactoryBeanReference` is
    // the invokestatic target `emit_bean_override` splices into a
    // FactoryBean-typed `@Bean` method's generated override;
    // `FB_HANDLER_CLASS`'s `invoke` backs the interface-proxy
    // representation's `InvocationHandler` and is picked up automatically
    // by the generic proxy-dispatch path (`invoke_or_native`), no VM-side
    // wiring needed.
    registry.register(
        "cratonvm/internal/ConfigEnhancerSupport",
        "enhanceFactoryBeanReference",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/String;Ljava/lang/String;)Ljava/lang/Object;",
        enhance_factory_bean_reference,
    );
    registry.register(
        FB_HANDLER_CLASS,
        "invoke",
        "(Ljava/lang/Object;Ljava/lang/reflect/Method;[Ljava/lang/Object;)Ljava/lang/Object;",
        fb_handler_invoke,
    );
    registry.set_category(__prev_cat);
}

#[cfg(test)]
mod fb_ref_bytecode_tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    /// Structural regression guard for `emit_bean_override`'s `fb_ref`
    /// byte-splice: build one FactoryBean-typed and one plain `@Bean`
    /// method on the same enhancer class, parse the result with the real
    /// class-file reader (`cratonvm-reader`, no VM/NativeContext needed —
    /// this is pure byte-emission logic), and check the FactoryBean method's
    /// Code attribute is exactly 8 bytes longer, its `max_stack` one deeper,
    /// and each of its two exception-table entries shifted by exactly the
    /// amount the splice implies: the inner NoSuchBeanDefinitionException
    /// handler wholesale (+8 on all three PCs, it lives after the splice
    /// point), the outer SPR-8080 `finally` only on its end/handler PCs (its
    /// TRY_START precedes the splice).
    ///
    /// Everything is asserted as a DELTA against the plain method. Absolute
    /// offsets were pinned here once and broke the moment an unrelated change
    /// to `emit_bean_override`'s body moved them (the mismatch block added
    /// 120 bytes and a second handler) — while the splice itself, which is all
    /// this test exists to guard, was still correct.
    #[test]
    fn fb_ref_splice_shifts_exception_table_by_exactly_8_bytes() {
        let bean_methods = vec![
            BeanMethod {
                name: "plainBean".to_string(),
                descriptor: "()Lcom/example/Plain;".to_string(),
                return_internal: "com/example/Plain".to_string(),
                is_factory_bean: false,
            },
            BeanMethod {
                name: "factoryBean".to_string(),
                descriptor: "()Lcom/example/MyFactoryBean;".to_string(),
                return_internal: "com/example/MyFactoryBean".to_string(),
                is_factory_bean: true,
            },
        ];
        // Dedicated loader id so parallel test runs never share a
        // `next_config_enhancer_counter` bucket with another test.
        let (_, bytes) = build_enhancer_class(999_001, "com/example/MyConfig", &bean_methods, &[]);
        let class_file = cratonvm_reader::read_class(&bytes).expect("generated class must parse");

        let mut plain = class_file
            .find_method("plainBean", "()Lcom/example/Plain;")
            .expect("plainBean method present")
            .clone();
        let mut factory = class_file
            .find_method("factoryBean", "()Lcom/example/MyFactoryBean;")
            .expect("factoryBean method present")
            .clone();
        for a in plain.attributes.iter_mut() {
            a.decode(&class_file.constant_pool)
                .expect("decode plainBean Code");
        }
        for a in factory.attributes.iter_mut() {
            a.decode(&class_file.constant_pool)
                .expect("decode factoryBean Code");
        }
        let plain_code = plain.code().expect("plainBean has a Code attribute");
        let factory_code = factory.code().expect("factoryBean has a Code attribute");

        // Everything asserted here is RELATIVE to the plain method. The
        // absolute byte offsets this test used to pin (code length 161,
        // exception-table PCs 94/109/142) describe `emit_bean_override`'s body
        // at the time the splice landed, so any later, unrelated edit to that
        // body breaks them while the splice itself is still correct — which is
        // exactly what happened. The splice contract is a delta, so test the
        // delta.
        assert_eq!(
            factory_code.max_stack,
            plain_code.max_stack + 1,
            "fb_ref splice briefly needs one more stack slot"
        );
        assert_eq!(plain_code.max_locals, factory_code.max_locals);

        assert_eq!(
            factory_code.code.len(),
            plain_code.code.len() + 8,
            "fb_ref splice must add exactly 8 bytes"
        );

        // `emit_bean_override` emits TWO handlers, and the splice moves them
        // differently — assert each against its plain-method counterpart.
        assert_eq!(plain_code.exception_table.len(), 2);
        assert_eq!(factory_code.exception_table.len(), 2);

        // Entry 0 — the inner NoSuchBeanDefinitionException handler around the
        // mismatch block. The whole block sits AFTER the splice point, so all
        // three of its PCs move by the full +8.
        let pe0 = &plain_code.exception_table[0];
        let fe0 = &factory_code.exception_table[0];
        assert_eq!(
            fe0.start_pc,
            pe0.start_pc + 8,
            "inner try2 starts after the splice point — shifts wholesale"
        );
        assert_eq!(fe0.end_pc, pe0.end_pc + 8);
        assert_eq!(fe0.handler_pc, pe0.handler_pc + 8);
        assert_eq!(fe0.catch_type, pe0.catch_type);

        // Entry 1 — the outer SPR-8080 any-Throwable `finally`. Its TRY_START
        // is before the splice point, so only its end/handler move.
        let pe1 = &plain_code.exception_table[1];
        let fe1 = &factory_code.exception_table[1];
        assert_eq!(
            fe1.start_pc, pe1.start_pc,
            "TRY_START is before the splice point — unaffected"
        );
        assert_eq!(fe1.end_pc, pe1.end_pc + 8);
        assert_eq!(fe1.handler_pc, pe1.handler_pc + 8);
        assert_eq!(
            fe1.catch_type, 0,
            "outer handler keeps `finally` (catch_type 0) semantics"
        );
    }

    /// Regression guard for the real-cglib-bookkeeping fields
    /// (`emit_public_static_field`'s call site): every generated enhancer
    /// class must declare all five, PUBLIC, with the exact names/types
    /// real cglib's `Enhancer.wrapCachedClass`/`isEnhanced`/callback
    /// machinery reflects on when it mistakes one of our classes for its
    /// own (see `emit_cglib_factory_data_field`'s doc comment) — or the
    /// `ConfigurationClassPostProcessorTests.genericsBasedInjectionWith*`
    /// class of bug resurfaces one field at a time.
    #[test]
    fn enhancer_class_declares_all_cglib_bookkeeping_fields() {
        let (_, bytes) = build_enhancer_class(999_002, "com/example/BookkeepingConfig", &[], &[]);
        let class_file = cratonvm_reader::read_class(&bytes).expect("generated class must parse");

        let expect_field = |name: &str, descriptor: &str| {
            let f = class_file
                .find_field(name)
                .unwrap_or_else(|| panic!("missing field {name}"));
            assert_eq!(&*f.descriptor, descriptor, "{name} has wrong descriptor");
            assert!(
                f.access_flags
                    .contains(cratonvm_reader::class_access_flags::FieldAccessFlags::PUBLIC)
                    && f.is_static(),
                "{name} must be public static"
            );
        };
        expect_field("CGLIB$FACTORY_DATA", "Ljava/lang/Object;");
        expect_field("CGLIB$CALLBACK_FILTER", "Ljava/lang/Object;");
        expect_field("CGLIB$THREAD_CALLBACKS", "Ljava/lang/Object;");
        expect_field("CGLIB$STATIC_CALLBACKS", "Ljava/lang/Object;");
        expect_field("CGLIB$BOUND", "Z");

        assert!(
            class_file
                .interfaces
                .iter()
                .any(|i| &**i == "org/springframework/cglib/proxy/Factory"),
            "generated class must implement org/springframework/cglib/proxy/Factory \
             (real cglib's defineClass-failure recovery path casts our class to it)"
        );
        let callback_desc = "Lorg/springframework/cglib/proxy/Callback;";
        let callback_arr_desc = "[Lorg/springframework/cglib/proxy/Callback;";
        for (name, desc) in [
            (
                "newInstance",
                format!("({callback_desc})Ljava/lang/Object;"),
            ),
            (
                "newInstance",
                format!("({callback_arr_desc})Ljava/lang/Object;"),
            ),
            (
                "newInstance",
                format!(
                    "([Ljava/lang/Class;[Ljava/lang/Object;{callback_arr_desc})Ljava/lang/Object;"
                ),
            ),
            ("getCallback", format!("(I){callback_desc}")),
            ("setCallback", format!("(I{callback_desc})V")),
            ("getCallbacks", format!("(){callback_arr_desc}")),
            ("setCallbacks", format!("({callback_arr_desc})V")),
        ] {
            assert!(
                class_file.find_method(name, &desc).is_some(),
                "missing Factory method {name}{desc}"
            );
        }
    }
}
