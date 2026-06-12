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
//!      `<OriginalName>$$EnhancerByCGLIB$$<counter>`.
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
//!     it returns the shared instance via `beanFactory.getBean("<name>")`.
//!   * `@Bean` methods with parameters, primitive/void/array returns, or
//!     static/final/private modifiers are left un-overridden (they run the real
//!     body — the `proxyBeanMethods=false` trade-off), never regressing relative
//!     to the previous no-override enhancer.
//!
//! The intercept is registered AFTER `net_phase_e::register_phase_e_networking`
//! so it overrides the older bypass registration at the registry layer (last
//! writer wins — `native-api/src/registry.rs:1336-1338`).

use std::sync::atomic::{AtomicU64, Ordering};

use cratonvm_native_api::{DefineClassFull, NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallResult, RuntimeError};
use cratonvm_types::Value;

/// Monotonic counter for generating unique enhancer subclass names. Mirrors
/// CGLIB's own `KeyFactory.generateName` counter.
static ENHANCER_COUNTER: AtomicU64 = AtomicU64::new(0);

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
        ClassWriter { cp: vec![Vec::new()] }
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
    let code: [u8; 5] = [0x2A, 0xB7, (super_init_methodref >> 8) as u8,
                         (super_init_methodref & 0xFF) as u8, 0xB1];

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
}

/// Emit a `@Bean`-method override that gives shared-singleton semantics:
///
/// ```text
/// Object bf = this.$$beanFactory;
/// if (!((BeanFactory) bf).isSingleton("<name>"))             // prototype/scoped
///     return super.<name>();                                  //   → fresh each call
/// if (((DefaultSingletonBeanRegistry) bf).isSingletonCurrentlyInCreation("<name>"))
///     return super.<name>();                                  // we ARE creating it
/// return (<Ret>) ((BeanFactory) bf).getBean("<name>");        // inter-bean ref → shared
/// ```
///
/// Equivalent in outcome to Spring's `BeanMethodInterceptor` but keyed on the
/// factory's creation state rather than the `getCurrentlyInvokedFactoryMethod`
/// thread-local: a singleton is marked "currently in creation" *before* its
/// factory method runs, so the creating call takes `super` (the real `@Bean`
/// body — no re-entrancy, since the super class is the original
/// `@Configuration`) and only later inter-bean references reach `getBean`,
/// returning the cached singleton. Non-singleton (e.g. `@Scope("prototype")`)
/// beans always take `super`, so each reference creates a fresh instance and
/// there is no `getBean` recursion. No-arg reference-returning methods only.
#[allow(clippy::too_many_arguments)]
fn emit_bean_override(
    name_idx: u16,
    descriptor_idx: u16,
    code_attr_name_idx: u16,
    name_string_idx: u16,
    super_method_ref: u16,
    bf_field_ref: u16,
    beanfactory_cast_idx: u16,
    issingleton_ref: u16,
    dsbr_cast_idx: u16,
    in_creation_ref: u16,
    getbean_ref: u16,
    rettype_cast_idx: u16,
) -> Vec<u8> {
    let b = |x: u16| -> [u8; 2] { x.to_be_bytes() };
    let mut code: Vec<u8> = Vec::new();
    // 0:  aload_0
    code.push(0x2A);
    // 1:  getfield this.$$beanFactory
    code.push(0xB4);
    code.extend_from_slice(&b(bf_field_ref));
    // 4:  astore_1  (local1 = bf)
    code.push(0x3C);
    // 5:  aload_1
    code.push(0x2B);
    // 6:  checkcast BeanFactory
    code.push(0xC0);
    code.extend_from_slice(&b(beanfactory_cast_idx));
    // 9:  ldc_w "<name>"
    code.push(0x13);
    code.extend_from_slice(&b(name_string_idx));
    // 12: invokeinterface BeanFactory.isSingleton(String)Z  count=2
    code.push(0xB9);
    code.extend_from_slice(&b(issingleton_ref));
    code.push(0x02);
    code.push(0x00);
    // 17: ifeq → L_super (49); offset 32  (not a singleton → fresh via super)
    code.push(0x99);
    code.extend_from_slice(&b(32));
    // 20: aload_1
    code.push(0x2B);
    // 21: checkcast DefaultSingletonBeanRegistry
    code.push(0xC0);
    code.extend_from_slice(&b(dsbr_cast_idx));
    // 24: ldc_w "<name>"
    code.push(0x13);
    code.extend_from_slice(&b(name_string_idx));
    // 27: invokevirtual isSingletonCurrentlyInCreation(String)Z
    code.push(0xB6);
    code.extend_from_slice(&b(in_creation_ref));
    // 30: ifne → L_super (49); offset 19  (we are creating it → super)
    code.push(0x9A);
    code.extend_from_slice(&b(19));
    // 33: aload_1
    code.push(0x2B);
    // 34: checkcast BeanFactory
    code.push(0xC0);
    code.extend_from_slice(&b(beanfactory_cast_idx));
    // 37: ldc_w "<name>"
    code.push(0x13);
    code.extend_from_slice(&b(name_string_idx));
    // 40: invokeinterface BeanFactory.getBean(String)Object  count=2
    code.push(0xB9);
    code.extend_from_slice(&b(getbean_ref));
    code.push(0x02);
    code.push(0x00);
    // 45: checkcast <Ret>
    code.push(0xC0);
    code.extend_from_slice(&b(rettype_cast_idx));
    // 48: areturn
    code.push(0xB0);
    // 49: L_super: aload_0
    code.push(0x2A);
    // 50: invokespecial super.<name><desc>
    code.push(0xB7);
    code.extend_from_slice(&b(super_method_ref));
    // 53: areturn
    code.push(0xB0);
    debug_assert_eq!(code.len(), 54);

    let mut code_attr = Vec::new();
    code_attr.extend_from_slice(&2u16.to_be_bytes()); // max_stack
    code_attr.extend_from_slice(&2u16.to_be_bytes()); // max_locals (this + bf)
    code_attr.extend_from_slice(&(code.len() as u32).to_be_bytes());
    code_attr.extend_from_slice(&code);
    code_attr.extend_from_slice(&0u16.to_be_bytes()); // exception_table_length
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

/// Generate a fresh `<OriginalName>$$EnhancerByCGLIB$$<counter>` class
/// file as a `Vec<u8>` plus the chosen internal-name. The new class
/// extends `super_internal_name` and implements
/// [`SPRING_MARKER_IFACE`].
fn build_enhancer_class(
    super_internal_name: &str,
    bean_methods: &[BeanMethod],
) -> (String, Vec<u8>) {
    let counter = ENHANCER_COUNTER.fetch_add(1, Ordering::Relaxed);
    let new_name = format!("{super_internal_name}$$EnhancerByCGLIB$${counter:x}");

    let mut cw = ClassWriter::new();

    // -- CONSTANT_Class entries: this, super, and the marker interface.
    let this_class_idx = cw.add_class(&new_name);
    let super_class_idx = cw.add_class(super_internal_name);
    let iface_idx = cw.add_class(SPRING_MARKER_IFACE);

    // -- Method machinery: <init>, ()V, Code, and the super-ctor methodref.
    let init_name_idx = cw.add_utf8("<init>");
    let void_no_arg_desc_idx = cw.add_utf8("()V");
    let code_attr_name_idx = cw.add_utf8("Code");
    let super_init_methodref = cw.add_methodref(super_class_idx, "<init>", "()V");

    // -- `$$beanFactory` field (type Object) + its Fieldref on THIS class.
    let bf_field_name_idx = cw.add_utf8("$$beanFactory");
    let object_desc_idx = cw.add_utf8("Ljava/lang/Object;");
    let bf_field_ref = cw.add_fieldref(this_class_idx, "$$beanFactory", "Ljava/lang/Object;");

    // -- setBeanFactory(BeanFactory)V — required because the marker interface
    // `EnhancedConfiguration` extends `BeanFactoryAware`. Now stores into the
    // `$$beanFactory` field (was a no-op).
    let set_bf_name_idx = cw.add_utf8("setBeanFactory");
    let set_bf_desc_idx =
        cw.add_utf8("(Lorg/springframework/beans/factory/BeanFactory;)V");

    let ctor = emit_default_ctor(
        init_name_idx,
        void_no_arg_desc_idx,
        code_attr_name_idx,
        super_init_methodref,
    );
    let set_bean_factory = emit_set_bean_factory(
        set_bf_name_idx,
        set_bf_desc_idx,
        code_attr_name_idx,
        bf_field_ref,
    );

    let field = emit_bean_factory_field(bf_field_name_idx, object_desc_idx);

    let mut methods: Vec<Vec<u8>> = vec![ctor, set_bean_factory];

    if !bean_methods.is_empty() {
        // Shared constant-pool refs used by every @Bean override.
        let beanfactory_cast_idx =
            cw.add_class("org/springframework/beans/factory/BeanFactory");
        let issingleton_ref = cw.add_interface_methodref(
            beanfactory_cast_idx,
            "isSingleton",
            "(Ljava/lang/String;)Z",
        );
        let getbean_ref = cw.add_interface_methodref(
            beanfactory_cast_idx,
            "getBean",
            "(Ljava/lang/String;)Ljava/lang/Object;",
        );
        let dsbr_cast_idx = cw
            .add_class("org/springframework/beans/factory/support/DefaultSingletonBeanRegistry");
        let in_creation_ref = cw.add_methodref(
            dsbr_cast_idx,
            "isSingletonCurrentlyInCreation",
            "(Ljava/lang/String;)Z",
        );

        for bm in bean_methods {
            let name_idx = cw.add_utf8(&bm.name);
            let desc_idx = cw.add_utf8(&bm.descriptor);
            let name_string_idx = cw.add_string(&bm.name);
            let super_method_ref =
                cw.add_methodref(super_class_idx, &bm.name, &bm.descriptor);
            let rettype_cast_idx = cw.add_class(&bm.return_internal);
            let override_method = emit_bean_override(
                name_idx,
                desc_idx,
                code_attr_name_idx,
                name_string_idx,
                super_method_ref,
                bf_field_ref,
                beanfactory_cast_idx,
                issingleton_ref,
                dsbr_cast_idx,
                in_creation_ref,
                getbean_ref,
                rettype_cast_idx,
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
                Value::Object(Some(unsafe { cratonvm_types::ObjectRef::from_raw(p as *mut u8) }))
            } else {
                Value::Object(None)
            }
        }
        other => other,
    }
}

const BEAN_ANNOTATION_DESC: &str = "Lorg/springframework/context/annotation/Bean;";

/// Scan the `@Configuration` class for `@Bean` factory methods that the
/// enhancer can safely override with shared-singleton interception. We only
/// take **instance, no-arg, reference-returning, non-final** methods — the
/// case the override bytecode handles (S03's `engine`/`car1`/`car2`/`widget`).
/// `@Bean` methods with parameters, primitive/void/array returns, or
/// static/final/private modifiers are left un-overridden (they run the real
/// body directly, i.e. the `proxyBeanMethods=false` trade-off), which never
/// regresses behaviour relative to the previous no-override enhancer.
fn scan_bean_methods(ctx: &mut dyn NativeContext, class_id: cratonvm_types::ClassId) -> Vec<BeanMethod> {
    const ACC_STATIC: u16 = 0x0008;
    const ACC_PRIVATE: u16 = 0x0002;
    const ACC_FINAL: u16 = 0x0010;
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for m in ctx.declared_methods(class_id) {
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
        let return_internal = ret[1..ret.len() - 1].to_string();
        // Must carry @Bean.
        let has_bean = ctx
            .method_annotations(class_id, &m.name, &m.descriptor)
            .iter()
            .any(|a| a.type_descriptor == BEAN_ANNOTATION_DESC);
        if !has_bean {
            continue;
        }
        // Guard against duplicate (name, descriptor) — a class can't declare
        // two, but be defensive since the override is keyed on name.
        if !seen.insert((m.name.clone(), m.descriptor.clone())) {
            continue;
        }
        out.push(BeanMethod {
            name: m.name.clone(),
            descriptor: m.descriptor.clone(),
            return_internal,
        });
    }
    out
}

/// `ConfigurationClassEnhancer.enhance(Class<?>, ClassLoader) → Class<?>`
/// native intercept. Replaces the previous "return original" bypass with a
/// real subclass produced by the minimal emitter above.
fn cce_enhance(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = receiver, args[1] = config Class, args[2] = ClassLoader.
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
            eprintln!(
                "[CCE] enhance: class arg was not an object — falling back to identity",
            );
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

    // Scan the @Configuration class for @Bean methods to intercept, then build
    // the subclass bytes. Loader id 0 = application loader (same as every other
    // defineClass entry point in this codebase).
    let bean_methods = scan_bean_methods(ctx, super_class_id);
    let (new_name, bytes) = build_enhancer_class(&super_name, &bean_methods);

    let opts = DefineClassFull {
        override_name: Some(new_name.clone()),
        skip_verification: true,
        ..Default::default()
    };

    match ctx.define_class_full(&new_name, &bytes, 0, opts) {
        Ok(cid) => {
            let mirror = ctx.get_class_mirror(cid);
            eprintln!(
                "[CCE] enhance: defined {new_name} (super={super_name}, marker={SPRING_MARKER_IFACE}, intercepted @Bean methods={})",
                bean_methods.len(),
            );
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
