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
//! Limitations:
//!   * We do NOT override `@Bean` methods. Inter-`@Bean`-method calls inside
//!     a `@Configuration` class will return fresh instances rather than shared
//!     beans (same trade-off as `@Configuration(proxyBeanMethods = false)`).
//!   * We do NOT add the `$$beanFactory` field — Spring's reflective set on
//!     this field will throw `NoSuchFieldException`. Callers that need the
//!     field should be updated separately (a follow-up emitter pass).
//!
//! The intercept is registered AFTER `net_phase_e::register_phase_e_networking`
//! so it overrides the older bypass registration at the registry layer (last
//! writer wins — `native-api/src/registry.rs:1336-1338`).

use std::sync::atomic::{AtomicU64, Ordering};

use rustjvm_native_api::{DefineClassFull, NativeContext, NativeMethodRegistry};
use rustjvm_types::error::{MethodCallResult, RuntimeError};
use rustjvm_types::Value;

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

    /// Assemble the final class file bytes.
    fn finish(
        &self,
        access_flags: u16,
        this_class_idx: u16,
        super_class_idx: u16,
        interfaces: &[u16],
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

        // fields_count = 0
        out.extend_from_slice(&0u16.to_be_bytes());

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

/// Build the bytes for a `setBeanFactory(BeanFactory)V` body that is a no-op
/// (`return`). The marker interface `EnhancedConfiguration` extends
/// `BeanFactoryAware`, which declares `setBeanFactory(BeanFactory) throws
/// BeansException`. Real CGLIB enhancement stores the factory into a
/// `$$beanFactory` field; the no-op variant is enough for context refresh
/// to advance past the `BeanFactoryAware` callback. Subsequent code that
/// reflectively reads `$$beanFactory` will see no such field and fall back
/// to other lookup paths — same trade-off as our prior identity-bypass
/// enhancer, which never had the field either.
fn emit_set_bean_factory(
    name_idx: u16,
    descriptor_idx: u16,
    code_attr_name_idx: u16,
) -> Vec<u8> {
    let mut method = Vec::new();
    method.extend_from_slice(&0x0001u16.to_be_bytes()); // ACC_PUBLIC
    method.extend_from_slice(&name_idx.to_be_bytes()); // "setBeanFactory"
    method.extend_from_slice(&descriptor_idx.to_be_bytes()); // "(Lorg/springframework/beans/factory/BeanFactory;)V"
    method.extend_from_slice(&1u16.to_be_bytes()); // attributes_count = 1 (Code)

    // Body bytecode: just `return` (0xB1). max_stack=0, max_locals=2 (this + arg).
    let code: [u8; 1] = [0xB1];

    let mut code_attr = Vec::new();
    code_attr.extend_from_slice(&0u16.to_be_bytes()); // max_stack
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

/// Generate a fresh `<OriginalName>$$EnhancerByCGLIB$$<counter>` class
/// file as a `Vec<u8>` plus the chosen internal-name. The new class
/// extends `super_internal_name` and implements
/// [`SPRING_MARKER_IFACE`].
fn build_enhancer_class(super_internal_name: &str) -> (String, Vec<u8>) {
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

    // -- setBeanFactory(BeanFactory)V — required because the marker interface
    // `EnhancedConfiguration` extends `BeanFactoryAware`, whose abstract
    // `setBeanFactory(BeanFactory)V` must be implemented or invocation throws
    // NoSuchMethodError when Spring's `BeanFactoryAware` post-processor casts
    // and invokes it.
    let set_bf_name_idx = cw.add_utf8("setBeanFactory");
    let set_bf_desc_idx =
        cw.add_utf8("(Lorg/springframework/beans/factory/BeanFactory;)V");

    let ctor = emit_default_ctor(
        init_name_idx,
        void_no_arg_desc_idx,
        code_attr_name_idx,
        super_init_methodref,
    );
    let set_bean_factory =
        emit_set_bean_factory(set_bf_name_idx, set_bf_desc_idx, code_attr_name_idx);

    // access_flags = ACC_PUBLIC (0x0001) | ACC_SUPER (0x0020) — JLS-required
    // for any new class file. ACC_SYNTHETIC (0x1000) flags the generated
    // class so reflection sees it as synthetic, matching CGLIB's emit.
    let access_flags: u16 = 0x0001 | 0x0020 | 0x1000;

    let bytes = cw.finish(
        access_flags,
        this_class_idx,
        super_class_idx,
        &[iface_idx],
        &[ctor, set_bean_factory],
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
            if let Some(p) = rustjvm_types::jlong_bits_as_aligned_object_ptr(bits as u64) {
                Value::Object(Some(unsafe { rustjvm_types::ObjectRef::from_raw(p as *mut u8) }))
            } else {
                Value::Object(None)
            }
        }
        other => other,
    }
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
            eprintln!("[CCE] enhance: mirror has no ClassId — fallback to identity");
            return Ok(Some(Value::Object(Some(cls_mirror))));
        }
    };

    let super_name = match ctx.class_name_of_id(super_class_id) {
        Some(n) => n,
        None => {
            eprintln!("[CCE] enhance: ClassId has no name — fallback to identity");
            return Ok(Some(Value::Object(Some(cls_mirror))));
        }
    };

    // Build the class bytes. Loader id 0 = application loader (same as
    // every other defineClass entry point in this codebase).
    let (new_name, bytes) = build_enhancer_class(&super_name);

    let opts = DefineClassFull {
        override_name: Some(new_name.clone()),
        skip_verification: true,
        ..Default::default()
    };

    match ctx.define_class_full(&new_name, &bytes, 0, opts) {
        Ok(cid) => {
            let mirror = ctx.get_class_mirror(cid);
            eprintln!(
                "[CCE] enhance: defined {new_name} (super={super_name}, marker={SPRING_MARKER_IFACE})",
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
    registry.register(
        "org/springframework/context/annotation/ConfigurationClassEnhancer",
        "enhance",
        "(Ljava/lang/Class;Ljava/lang/ClassLoader;)Ljava/lang/Class;",
        cce_enhance,
    );
}
