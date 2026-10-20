// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP1.6 — synthetic `LambdaMetafactory` proxy metadata.
//!
//! The bulk of lambda proxy creation (bootstrap + allocation) lives in
//! `runtime/invokedynamic.rs` and the SAM dispatch in
//! `runtime/interpreter/lambda.rs` (`try_lambda_dispatch`).
//!
//! What is left here is small and, as of 2026-09-24, has NO production
//! caller: `ImplKind` (a classification of the implementation handle's
//! `MethodHandleKind`), `describe_call_site` and `capture_count`. Their only
//! readers are this file's tests and the linkage smoke check in
//! `vm/tests/wp2_5_proxy.rs`; `try_lambda_dispatch` matches on
//! `MethodHandleKind` directly. The diagnostics counters this module once
//! held were deleted in round 10 (note below). A caller that wants the
//! classification should use `ImplKind` rather than a second copy of it.

use crate::classloading::resolution::{LambdaCallSite, MethodHandle, MethodHandleKind};
use crate::vm::SharedVm;
use cratonvm_types::ClassId;
use std::sync::Arc;

/// Category of the implementation target captured by `LambdaCallSite`.
///
/// This is what the SAM call-site ends up delegating to. Consumers use
/// this to decide how many pre-arguments the proxy will need and how to
/// route the `invokeSpecial`/`invokeVirtual`/etc. dispatch.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ImplKind {
    /// `REF_invokeStatic` — a static method reference. Captures are passed
    /// in as leading parameters.
    Static,
    /// `REF_invokeVirtual` / `REF_invokeInterface` — instance bind via
    /// `this` (captured as the first parameter of the indy factory).
    Virtual,
    /// `REF_invokeSpecial` — non-virtual instance dispatch (e.g. private
    /// helpers, `super.foo()` references).
    Special,
    /// `REF_newInvokeSpecial` — constructor reference (`Foo::new`).
    Constructor,
    /// `REF_getField` / `REF_getStatic` — field-getter reference.
    Getter,
    /// `REF_putField` / `REF_putStatic` — field-setter reference.
    Setter,
}

impl ImplKind {
    /// Classify the bsm's implementation MethodHandle.
    pub fn from_method_handle(mh: &MethodHandle) -> Self {
        match mh.kind {
            MethodHandleKind::InvokeStatic => ImplKind::Static,
            MethodHandleKind::InvokeVirtual | MethodHandleKind::InvokeInterface => {
                ImplKind::Virtual
            }
            MethodHandleKind::InvokeSpecial => ImplKind::Special,
            MethodHandleKind::NewInvokeSpecial => ImplKind::Constructor,
            MethodHandleKind::GetField | MethodHandleKind::GetStatic => ImplKind::Getter,
            MethodHandleKind::PutField | MethodHandleKind::PutStatic => ImplKind::Setter,
        }
    }

    /// `true` iff this impl kind requires a non-static dispatch — i.e. the
    /// first argument to the impl will be the `this` of the captured
    /// instance.
    pub fn is_instance(self) -> bool {
        matches!(
            self,
            ImplKind::Virtual | ImplKind::Special | ImplKind::Getter | ImplKind::Setter
        )
    }
}

// (The four lambda-proxy lifecycle counters -- `note_bootstrap`,
// `note_dispatch`, `bootstrap_count`, `dispatch_count` -- and the two statics
// behind them were DELETED in round 10 rather than wired.
//
// The question they were meant to answer is "were the bootstrap and dispatch
// paths both exercised in this run?". Neither recorder had a caller anywhere
// in production: `note_bootstrap`'s doc said it was "called once per distinct
// (class, cp_index) pair by `invokedynamic.rs`" and no such call existed, so
// both counters read zero on every run that has ever existed. The only
// references were this file's own `counters_are_monotonic` test -- which
// proved the `fetch_add` works, not that anything calls it -- and a linkage
// smoke check in `vm/tests/wp2_5_proxy.rs`.
//
// That question is answered elsewhere now, by instruments with real feeders:
// `interpreter::lambda`'s `LAMBDA_*` counters for the dispatch half, and the
// `indy`/`lambda` rows of the JIT method-stats dump for the bootstrap half.
// See docs/internal/retired/r10-diagread-orphan-instrument-debt-register-20260921-RETIRED-20260922.md.)

/// Describe a `LambdaCallSite` in a short human-readable form for log
/// output and test assertions. Format:
/// `<functional_interface>.<sam_method><sam_descriptor>` –> `<impl>`.
pub fn describe_call_site(lcs: &LambdaCallSite) -> String {
    format!(
        "{}.{}{} -> {}.{}{}",
        lcs.functional_interface,
        lcs.sam_method_name,
        lcs.sam_descriptor,
        lcs.impl_handle.class_name,
        lcs.impl_handle.member_name,
        lcs.impl_handle.descriptor,
    )
}

/// Return the number of capture arguments the lambda factory takes. This
/// is the length of the indy factory's parameter list.
pub fn capture_count(lcs: &LambdaCallSite) -> usize {
    lcs.capture_types.len()
}

// ---------------------------------------------------------------------------
// One record of a lambda proxy's class facts (interpreter round i2 wave 56,
// lane L1;
// `docs/internal/fixed-bugs/interpreter-i55-L1-proposal-lambda-proxies-answer-the-tool-interfaces-from-one-record-DONE-20261224.md`)
//
// The VM mints a lambda's class outside the class store, so every tool
// interface that names classes answers one from its `LambdaCallSite`. These
// are the facts of the hidden class HotSpot's `InnerClassLambdaMetafactory`
// spins (JDK 25), MEASURED with `L1W55JvmtiLambdaClass` and
// `L1W56JvmtiLambdaMethods`, built on demand (never at link time: the indy
// path is unchanged and nothing is kept per proxy) by [`lambda_proxy_facts`].
// Readers: the C JVMTI table (`jvmti::native_env`).
// ---------------------------------------------------------------------------

/// `ACC_SYNTHETIC | ACC_FINAL | ACC_SUPER`: a lambda class's modifiers
/// (measured `0x1030`).
pub const LAMBDA_PROXY_CLASS_MODIFIERS: u16 = 0x1030;

/// `ACC_PUBLIC`, the functional method's flags.
const ACC_PUBLIC: u16 = 0x0001;
/// `ACC_PRIVATE`, the constructor's flags.
const ACC_PRIVATE: u16 = 0x0002;
/// `ACC_PRIVATE | ACC_FINAL`: `writeReplace`, `writeObject`, `readObject`.
const ACC_PRIVATE_FINAL: u16 = 0x0012;
/// `ACC_PUBLIC | ACC_BRIDGE`: an `altMetafactory` bridge.
const ACC_PUBLIC_BRIDGE: u16 = 0x0041;

/// One method the spun lambda class declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LambdaProxyMethod {
    /// Its name (`<init>`, the functional method's, `writeReplace`, ...).
    pub name: Arc<str>,
    /// Its descriptor (the constructor's takes the captured values).
    pub descriptor: Arc<str>,
    /// Its access flags as the spun class file has them: never
    /// `ACC_SYNTHETIC` (measured: `IsMethodSynthetic` false for each).
    pub access_flags: u16,
    /// Its argument slots, `this` included; also its `max_locals` (the spun
    /// code stores no local; measured).
    pub args_size: u16,
}

/// What the spun class adds for serialization
/// (`InnerClassLambdaMetafactory::generateInnerClass`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LambdaProxySerial {
    /// Neither method.
    None,
    /// `FLAG_SERIALIZABLE`: `writeReplace()`.
    Friendly,
    /// Not flagged, but the functional interface or a marker extends
    /// `Serializable`: `writeObject` and `readObject`, which throw.
    Hostile,
}

/// HotSpot's facts of lambda proxy class `id`'s spun class, read from its
/// `LambdaCallSite` and the side tables the bootstrap fills.
#[derive(Debug, Clone)]
pub struct LambdaProxyFacts {
    /// The host class (the class whose call site minted it), by id when the
    /// VM has it; its defining loader is the proxy's.
    pub host_id: Option<ClassId>,
    /// The host's internal name.
    pub host_name: String,
    /// The direct superinterfaces in `InnerClassLambdaMetafactory`'s order:
    /// the functional interface, the `altMetafactory` markers, then
    /// `java.io.Serializable` for a serializable lambda none of them already
    /// extends. One the class store does not know is left out.
    pub interfaces: Vec<ClassId>,
    /// The declared methods, in the spun class file's order.
    pub methods: Vec<LambdaProxyMethod>,
    /// The declared fields, `(name, descriptor)`, in the spun class file's
    /// order: one `private final arg$N` per captured value, typed as its
    /// constructor parameter (the i54-L1 retirement; MEASURED,
    /// `L1K54JvmtiLambdaProxyLocalsAndFields`). Capture `i` is the proxy
    /// object's field slot `i`, where the lambda dispatch reads it.
    pub fields: Vec<(String, String)>,
}

impl LambdaProxyFacts {
    /// `GetClassSignature`'s answer for proxy `id`: the host's name,
    /// `$$Lambda`, then `.` and the 16-digit hex suffix (the id here, as
    /// `Class.getName` spells it with `/`).
    pub fn signature(&self, id: ClassId) -> String {
        format!("L{}$$Lambda.0x{:016x};", self.host_name, id.as_u32())
    }
}

/// The facts of lambda proxy `id`, or `None` when `id` is no lambda proxy
/// of `shared`. Cold: a tool's question, never the indy path.
#[cold]
#[inline(never)]
pub fn lambda_proxy_facts(shared: &SharedVm, id: ClassId) -> Option<LambdaProxyFacts> {
    let site = shared.classes.lambda_proxies.read().get(&id).cloned()?;
    let (host_id, host_name) = crate::runtime::interpreter::lambda_proxy_host(shared, id)?;
    let host_id = host_id.or_else(|| class_id_named(shared, &host_name));
    let markers =
        crate::runtime::invokedynamic::lambda_proxy_marker_interfaces(shared.vm_identity, id);
    let mut interfaces: Vec<ClassId> = Vec::new();
    let functional = site
        .functional_interface_id
        .or_else(|| class_id_named(shared, &site.functional_interface));
    interfaces.extend(functional);
    for marker in markers.iter().flat_map(|m| m.iter()) {
        let marker_id = marker
            .class_id
            .or_else(|| class_id_named(shared, &marker.name));
        if let Some(marker_id) = marker_id.filter(|m| !interfaces.contains(m)) {
            interfaces.push(marker_id);
        }
    }
    let cm = shared.classes.class_manager.read();
    // `is_assignable_to_name` walks the interface DAG (`is_subclass_of_by_name`
    // walks superclasses only, so an interface extending `Serializable` read
    // as not serializable: wave 55's copy appended a second `Serializable`).
    let any_serializable = interfaces
        .iter()
        .any(|&i| cm.is_assignable_to_name(i, "java/io/Serializable"));
    let serializable = cm.resolve_fast_path_class_id("java/io/Serializable");
    drop(cm);
    let serial = if site.serializable_flag {
        if let (false, Some(serializable)) = (any_serializable, serializable) {
            interfaces.push(serializable);
        }
        LambdaProxySerial::Friendly
    } else if any_serializable {
        LambdaProxySerial::Hostile
    } else {
        LambdaProxySerial::None
    };
    let bridges =
        crate::runtime::invokedynamic::lambda_proxy_bridge_descriptors(shared.vm_identity, id);
    let methods = lambda_proxy_methods(&site, bridges.as_deref().unwrap_or_default(), serial);
    Some(LambdaProxyFacts {
        host_id,
        host_name,
        interfaces,
        methods,
        fields: lambda_proxy_fields(&site),
    })
}

/// `ACC_PRIVATE | ACC_FINAL`: an `arg$N` capture field's modifiers
/// (measured `0x12`).
pub const LAMBDA_PROXY_FIELD_MODIFIERS: u16 = 0x0012;

/// The `arg$N` fields `InnerClassLambdaMetafactory` declares on the class it
/// spins for `site`, `(name, descriptor)`, one per captured value in order
/// ([`LambdaProxyFacts::fields`]).
pub fn lambda_proxy_fields(site: &LambdaCallSite) -> Vec<(String, String)> {
    capture_descriptors(site)
        .into_iter()
        .enumerate()
        .map(|(i, descriptor)| (format!("arg${}", i + 1), descriptor))
        .collect()
}

/// A loaded class's id by internal name, when the class store has one.
fn class_id_named(shared: &SharedVm, name: &str) -> Option<ClassId> {
    shared
        .classes
        .class_manager
        .read()
        .resolve_fast_path_class_id(name)
}

/// The methods `InnerClassLambdaMetafactory` declares on the class it spins
/// for `site`, in its order: the private constructor taking the captured
/// values, the public functional method (the SAM's erased descriptor), one
/// public bridge per `FLAG_BRIDGES` descriptor, then `writeReplace` or the
/// hostile `writeObject` / `readObject` pair. (A non-capturing class gets a
/// `<clinit>` only under `jdk.internal.lambda.disableEagerInitialization`,
/// which the VM does not model.)
pub fn lambda_proxy_methods(
    site: &LambdaCallSite,
    bridges: &[Arc<str>],
    serial: LambdaProxySerial,
) -> Vec<LambdaProxyMethod> {
    let captures = capture_descriptors(site);
    let init = format!("({})V", captures.concat());
    let capture_slots: usize = captures.iter().map(|d| descriptor_slots(d)).sum();
    let mut methods = vec![LambdaProxyMethod {
        name: Arc::from("<init>"),
        descriptor: Arc::from(init),
        access_flags: ACC_PRIVATE,
        args_size: slots_with_this(capture_slots),
    }];
    let forwarding = |descriptor: &Arc<str>, access_flags: u16| LambdaProxyMethod {
        name: Arc::clone(&site.sam_method_name),
        descriptor: Arc::clone(descriptor),
        access_flags,
        args_size: slots_with_this(
            param_descriptors(descriptor)
                .iter()
                .map(|d| descriptor_slots(d))
                .sum(),
        ),
    };
    methods.push(forwarding(&site.sam_descriptor, ACC_PUBLIC));
    methods.extend(bridges.iter().map(|d| forwarding(d, ACC_PUBLIC_BRIDGE)));
    let private_final = |name: &str, descriptor: &str, args_size: u16| LambdaProxyMethod {
        name: Arc::from(name),
        descriptor: Arc::from(descriptor),
        access_flags: ACC_PRIVATE_FINAL,
        args_size,
    };
    match serial {
        LambdaProxySerial::None => {}
        LambdaProxySerial::Friendly => {
            methods.push(private_final("writeReplace", "()Ljava/lang/Object;", 1));
        }
        LambdaProxySerial::Hostile => {
            methods.push(private_final(
                "writeObject",
                "(Ljava/io/ObjectOutputStream;)V",
                2,
            ));
            methods.push(private_final(
                "readObject",
                "(Ljava/io/ObjectInputStream;)V",
                2,
            ));
        }
    }
    methods
}

/// `this` plus `slots` argument slots, as a `u16` (a descriptor has at most
/// 255 slots).
fn slots_with_this(slots: usize) -> u16 {
    u16::try_from(slots.saturating_add(1)).unwrap_or(u16::MAX)
}

/// The declared type of each capture, as the spun class types its `arg$N`
/// fields and its constructor's parameters: the factory type's parameters,
/// which are the implementation method's leading parameters, after its
/// receiver for an instance method (`REF_invokeVirtual`, `REF_invokeSpecial`,
/// `REF_invokeInterface`: the bound receiver is the first capture, typed by
/// the method's class) -- the rule `Class.getDeclaredFields` of a proxy uses
/// (`native-builtins` `lang_class::capture_declared_descriptors`). A capture
/// whose derived descriptor disagrees with its recorded type character is
/// spelled by that character (`Ljava/lang/Object;` for a reference).
fn capture_descriptors(site: &LambdaCallSite) -> Vec<String> {
    use crate::classloading::resolution::MethodHandleKind as K;
    let handle = &site.impl_handle;
    let mut declared: Vec<String> = Vec::new();
    if matches!(
        handle.kind,
        K::InvokeVirtual | K::InvokeSpecial | K::InvokeInterface
    ) {
        declared.push(format!("L{};", handle.class_name));
    }
    if matches!(
        handle.kind,
        K::InvokeVirtual
            | K::InvokeSpecial
            | K::InvokeInterface
            | K::InvokeStatic
            | K::NewInvokeSpecial
    ) {
        declared.extend(param_descriptors(&handle.descriptor));
    }
    site.capture_types
        .iter()
        .enumerate()
        .map(|(i, &ty)| {
            let is_ref_char = matches!(ty, 'L' | '[');
            let agrees = declared.get(i).filter(|d| {
                let is_ref = d.starts_with('L') || d.starts_with('[');
                if is_ref_char {
                    is_ref
                } else {
                    d.len() == 1 && d.starts_with(ty)
                }
            });
            match agrees {
                Some(d) => d.clone(),
                None if is_ref_char => "Ljava/lang/Object;".to_string(),
                None => ty.to_string(),
            }
        })
        .collect()
}

/// The parameter descriptors of method descriptor `desc`, one token each;
/// empty for a malformed one.
fn param_descriptors(desc: &str) -> Vec<String> {
    let Some(params) = desc.strip_prefix('(').and_then(|d| d.split(')').next()) else {
        return Vec::new();
    };
    let bytes = params.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        while bytes.get(i) == Some(&b'[') {
            i += 1;
        }
        match bytes.get(i) {
            Some(b'L') => match params[i..].find(';') {
                Some(end) => i += end + 1,
                None => return Vec::new(),
            },
            Some(_) => i += 1,
            None => return Vec::new(),
        }
        out.push(params[start..i].to_string());
    }
    out
}

/// The local-variable slots a value of descriptor `d` takes.
fn descriptor_slots(d: &str) -> usize {
    if d == "J" || d == "D" {
        2
    } else {
        1
    }
}

// ---------------------------------------------------------------------------
// The spun class of a proxy (interpreter round i2 wave 57, lane L1: its code;
// the i54-L1 retirement: its constant pool,
// `docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-does-not-know-the-vms-lambda-proxy-classes-FIXED-20261007.md`)
//
// The VM runs a proxy's call without the class `InnerClassLambdaMetafactory`
// spins, but JVMTI's `GetMethodLocation`, `GetBytecodes` and
// `GetConstantPool` (and JDWP's `Method.Bytecodes`) answer it. This is that
// class's code and constant pool as JDK 25's generator builds them through
// the ClassFile API (`generateInnerClass`, `generateConstructor`,
// `forwardingMethod` with `TypeConvertingMethodAdapter.convertType`,
// `generateSerializationFriendlyMethods`,
// `generateSerializationHostileMethods`): an entry is made the first time
// the generator asks for it (equal entries are shared), in the order of its
// calls -- the class's name, each field's name and descriptor, then per
// method its name, its descriptor and the entries its instructions need --
// and, as the class is written, its superclass and interfaces, then the
// attribute names (`Code`, `Exceptions`, `BootstrapMethods`). HotSpot then
// appends the hidden class's own name (`Host$$Lambda+0x...`) and points the
// class entry at it. MEASURED, HotSpot 25.0.3: every entry and every byte of
// ten shapes (`tools/probes/interp/L1/L1K54JvmtiLambdaProxyConstantPool.java`).
// ---------------------------------------------------------------------------

/// What the code model needs to know of the classes a site names, from the
/// class store: the access flags of a method (`class`, `name`, `descriptor`)
/// and whether a class is an interface. `None`: not known.
pub trait LambdaCodeLookup {
    fn method_flags(&self, class: &str, name: &str, descriptor: &str) -> Option<u16>;
    fn is_interface(&self, class: &str) -> Option<bool>;
}

/// `ACC_PROTECTED`.
const ACC_PROTECTED: u16 = 0x0004;
/// `ACC_PRIVATE` of a member (the constructor's flags reuse [`ACC_PRIVATE`]).
const ACC_PRIVATE_MEMBER: u16 = 0x0002;

/// The JVM type kind a descriptor loads as (`TypeKind.from(...).asLoadable()`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Int,
    Long,
    Float,
    Double,
    Ref,
    Void,
}

fn kind_of(d: &str) -> Kind {
    match d.as_bytes().first() {
        Some(b'J') => Kind::Long,
        Some(b'F') => Kind::Float,
        Some(b'D') => Kind::Double,
        Some(b'L' | b'[') => Kind::Ref,
        Some(b'V') => Kind::Void,
        _ => Kind::Int,
    }
}

fn is_primitive(d: &str) -> bool {
    matches!(d, "Z" | "B" | "C" | "S" | "I" | "J" | "F" | "D")
}

/// The primitive a box class's descriptor wraps
/// (`TypeConvertingMethodAdapter.primitiveTypeKindFromClass`).
fn unboxed(d: &str) -> Option<&'static str> {
    Some(match d {
        "Ljava/lang/Integer;" => "I",
        "Ljava/lang/Long;" => "J",
        "Ljava/lang/Boolean;" => "Z",
        "Ljava/lang/Short;" => "S",
        "Ljava/lang/Byte;" => "B",
        "Ljava/lang/Character;" => "C",
        "Ljava/lang/Float;" => "F",
        "Ljava/lang/Double;" => "D",
        _ => return None,
    })
}

/// The box class of primitive descriptor `p` (`Wrapper`).
fn box_class(p: &str) -> &'static str {
    match p {
        "Z" => "java/lang/Boolean",
        "B" => "java/lang/Byte",
        "S" => "java/lang/Short",
        "C" => "java/lang/Character",
        "J" => "java/lang/Long",
        "F" => "java/lang/Float",
        "D" => "java/lang/Double",
        _ => "java/lang/Integer",
    }
}

/// The return descriptor of method descriptor `desc`.
fn return_descriptor(desc: &str) -> &str {
    desc.rsplit_once(')').map_or("V", |(_, r)| r)
}

/// The class name a `ClassEntry` of field descriptor `d` holds: the
/// internal name of an object type, the descriptor of an array type.
fn class_name_of(d: &str) -> &str {
    d.strip_prefix('L')
        .and_then(|n| n.strip_suffix(';'))
        .unwrap_or(d)
}

/// One constant-pool entry of a spun class (none of its entries is a `long`
/// or a `double`, so each takes one index).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PoolEntry {
    Utf8(String),
    Integer(i32),
    Class(u16),
    String(u16),
    Fieldref(u16, u16),
    Methodref(u16, u16),
    InterfaceMethodref(u16, u16),
    NameAndType(u16, u16),
    MethodHandle(u8, u16),
    /// `CONSTANT_Dynamic`: the bootstrap method's index, the name and type.
    Dynamic(u16, u16),
}

impl PoolEntry {
    /// The entry in class-file form (JVMS 4.4).
    fn write(&self, out: &mut Vec<u8>) {
        let two = |out: &mut Vec<u8>, tag: u8, a: u16, b: u16| {
            out.push(tag);
            out.extend_from_slice(&a.to_be_bytes());
            out.extend_from_slice(&b.to_be_bytes());
        };
        match self {
            PoolEntry::Utf8(text) => {
                out.push(1);
                let bytes = modified_utf8(text);
                out.extend_from_slice(
                    &u16::try_from(bytes.len()).unwrap_or(u16::MAX).to_be_bytes(),
                );
                out.extend_from_slice(&bytes);
            }
            PoolEntry::Integer(v) => {
                out.push(3);
                out.extend_from_slice(&v.to_be_bytes());
            }
            PoolEntry::Class(name) => {
                out.push(7);
                out.extend_from_slice(&name.to_be_bytes());
            }
            PoolEntry::String(text) => {
                out.push(8);
                out.extend_from_slice(&text.to_be_bytes());
            }
            PoolEntry::Fieldref(c, nt) => two(out, 9, *c, *nt),
            PoolEntry::Methodref(c, nt) => two(out, 10, *c, *nt),
            PoolEntry::InterfaceMethodref(c, nt) => two(out, 11, *c, *nt),
            PoolEntry::NameAndType(n, t) => two(out, 12, *n, *t),
            PoolEntry::MethodHandle(kind, reference) => {
                out.push(15);
                out.push(*kind);
                out.extend_from_slice(&reference.to_be_bytes());
            }
            PoolEntry::Dynamic(bsm, nt) => two(out, 17, *bsm, *nt),
        }
    }
}

/// A spun class's constant pool as the ClassFile API's `SplitConstantPool`
/// grows it: an entry is appended when first asked for, an equal one is
/// shared.
#[derive(Default)]
struct Pool {
    entries: Vec<PoolEntry>,
    index: std::collections::HashMap<PoolEntry, u16>,
    /// `BootstrapMethods` rows were made (a `CONSTANT_Dynamic`).
    bootstraps: bool,
}

impl Pool {
    fn put(&mut self, entry: PoolEntry) -> u16 {
        if let Some(&at) = self.index.get(&entry) {
            return at;
        }
        self.entries.push(entry.clone());
        let at = u16::try_from(self.entries.len()).unwrap_or(u16::MAX);
        self.index.insert(entry, at);
        at
    }

    fn utf8(&mut self, text: &str) -> u16 {
        self.put(PoolEntry::Utf8(text.to_string()))
    }

    fn class(&mut self, name: &str) -> u16 {
        let name = self.utf8(name);
        self.put(PoolEntry::Class(name))
    }

    fn string(&mut self, text: &str) -> u16 {
        let text = self.utf8(text);
        self.put(PoolEntry::String(text))
    }

    fn name_and_type(&mut self, name: &str, descriptor: &str) -> u16 {
        let name = self.utf8(name);
        let descriptor = self.utf8(descriptor);
        self.put(PoolEntry::NameAndType(name, descriptor))
    }

    /// A method reference: the owner's class entry first, then the name and
    /// type (`ConstantPoolBuilder.methodRefEntry(ClassDesc, String,
    /// MethodTypeDesc)`, and the order a foreign entry is cloned in).
    fn method(&mut self, owner: &str, name: &str, descriptor: &str, interface: bool) -> u16 {
        let owner = self.class(owner);
        let nt = self.name_and_type(name, descriptor);
        self.put(if interface {
            PoolEntry::InterfaceMethodref(owner, nt)
        } else {
            PoolEntry::Methodref(owner, nt)
        })
    }

    fn field(&mut self, owner: u16, name: &str, descriptor: &str) -> u16 {
        let nt = self.name_and_type(name, descriptor);
        self.put(PoolEntry::Fieldref(owner, nt))
    }
}

/// An instruction stream over a spun class's pool.
struct Code<'p> {
    bytes: Vec<u8>,
    pool: &'p mut Pool,
}

impl Code<'_> {
    fn op(&mut self, op: u8) -> &mut Self {
        self.bytes.push(op);
        self
    }

    /// An instruction with a two-byte constant-pool operand.
    fn indexed(&mut self, op: u8, index: u16) -> &mut Self {
        self.bytes.push(op);
        self.bytes.extend_from_slice(&index.to_be_bytes());
        self
    }

    /// `ldc` of entry `index`, `ldc_w` past 255.
    fn ldc(&mut self, index: u16) -> &mut Self {
        match u8::try_from(index) {
            Ok(short) => self.bytes.extend_from_slice(&[0x12, short]),
            Err(_) => {
                self.indexed(0x13, index);
            }
        }
        self
    }

    /// `checkcast`, `new` or `anewarray` of the class of descriptor or name
    /// `class`.
    fn class_op(&mut self, op: u8, class: &str) -> &mut Self {
        let index = self.pool.class(class_name_of(class));
        self.indexed(op, index)
    }

    /// `invoke*` of a method.
    fn invoke(
        &mut self,
        op: u8,
        owner: &str,
        name: &str,
        descriptor: &str,
        interface: bool,
    ) -> &mut Self {
        let index = self.pool.method(owner, name, descriptor, interface);
        if op == 0xb9 {
            // `invokeinterface`: the index, the argument slots with the
            // receiver, and a zero.
            let count: usize = 1 + param_descriptors(descriptor)
                .iter()
                .map(|d| descriptor_slots(d))
                .sum::<usize>();
            self.indexed(op, index);
            self.bytes
                .extend_from_slice(&[u8::try_from(count).unwrap_or(u8::MAX), 0]);
            return self;
        }
        self.indexed(op, index)
    }

    /// `CodeBuilder.loadConstant(int)`: `iconst_<n>`, `bipush` or `sipush`.
    fn int_constant(&mut self, value: usize) -> &mut Self {
        match value {
            0..=5 => self.bytes.push(0x03 + value as u8), // Cast: 0..=5
            6..=127 => self.bytes.extend_from_slice(&[0x10, value as u8]), // Cast: below 128
            _ => {
                let v = u16::try_from(value).unwrap_or(u16::MAX).to_be_bytes();
                self.bytes.extend_from_slice(&[0x11, v[0], v[1]]);
            }
        }
        self
    }

    /// `CodeBuilder.loadLocal(kind, slot)`.
    fn load(&mut self, kind: Kind, slot: usize) -> &mut Self {
        let (short, long) = match kind {
            Kind::Long => (0x1e, 0x16),
            Kind::Float => (0x22, 0x17),
            Kind::Double => (0x26, 0x18),
            Kind::Ref => (0x2a, 0x19),
            Kind::Int | Kind::Void => (0x1a, 0x15),
        };
        match slot {
            0..=3 => self.bytes.push(short + slot as u8), // Cast: 0..=3
            4..=255 => self.bytes.extend_from_slice(&[long, slot as u8]), // Cast: below 256
            _ => {
                let s = u16::try_from(slot).unwrap_or(u16::MAX).to_be_bytes();
                self.bytes.extend_from_slice(&[0xc4, long, s[0], s[1]]);
            }
        }
        self
    }

    /// `TypeConvertingMethodAdapter.widen`: one conversion, or none.
    fn widen(&mut self, from: Kind, to: Kind) -> &mut Self {
        let op = match (from, to) {
            (Kind::Int, Kind::Long) => 0x85,
            (Kind::Int, Kind::Float) => 0x86,
            (Kind::Int, Kind::Double) => 0x87,
            (Kind::Long, Kind::Int) => 0x88,
            (Kind::Long, Kind::Float) => 0x89,
            (Kind::Long, Kind::Double) => 0x8a,
            (Kind::Float, Kind::Int) => 0x8b,
            (Kind::Float, Kind::Long) => 0x8c,
            (Kind::Float, Kind::Double) => 0x8d,
            (Kind::Double, Kind::Int) => 0x8e,
            (Kind::Double, Kind::Long) => 0x8f,
            (Kind::Double, Kind::Float) => 0x90,
            _ => return self,
        };
        self.op(op)
    }

    /// `TypeConvertingMethodAdapter.box`: `Wrapper.valueOf` of primitive `p`.
    fn box_(&mut self, p: &str) -> &mut Self {
        let owner = box_class(p);
        self.invoke(0xb8, owner, "valueOf", &format!("({p})L{owner};"), false)
    }

    /// `TypeConvertingMethodAdapter.unbox`: to primitive `p`, through
    /// `Number` but for a `boolean` and a `char`.
    fn unbox(&mut self, p: &str) -> &mut Self {
        let (owner, name) = match p {
            "Z" => ("java/lang/Boolean", "booleanValue"),
            "C" => ("java/lang/Character", "charValue"),
            "B" => ("java/lang/Number", "byteValue"),
            "S" => ("java/lang/Number", "shortValue"),
            "J" => ("java/lang/Number", "longValue"),
            "F" => ("java/lang/Number", "floatValue"),
            "D" => ("java/lang/Number", "doubleValue"),
            _ => ("java/lang/Number", "intValue"),
        };
        self.invoke(0xb6, owner, name, &format!("(){p}"), false)
    }

    /// `TypeConvertingMethodAdapter.cast`: `checkcast` unless to `Object`.
    fn cast(&mut self, to: &str) -> &mut Self {
        if to != "Ljava/lang/Object;" {
            self.class_op(0xc0, to);
        }
        self
    }

    /// `TypeConvertingMethodAdapter.convertType(arg, target, functional)`.
    fn convert(&mut self, arg: &str, target: &str, functional: &str) -> &mut Self {
        if (arg == target && arg == functional) || arg == "V" || target == "V" {
            return self;
        }
        if is_primitive(arg) {
            if is_primitive(target) {
                return self.widen(kind_of(arg), kind_of(target));
            }
            match unboxed(target) {
                // Widen to the box's primitive, then box.
                Some(p) => self.widen(kind_of(arg), kind_of(p)).box_(p),
                // Box and cast.
                None => self.box_(arg).cast(target),
            };
            return self;
        }
        let src = if arg == functional || is_primitive(functional) {
            arg
        } else {
            self.cast(functional);
            functional
        };
        if is_primitive(target) {
            match unboxed(src) {
                Some(p) if p != "C" && p != "Z" => {
                    self.unbox(target);
                }
                Some(p) => {
                    self.unbox(p).widen(kind_of(p), kind_of(target));
                }
                None => {
                    match target {
                        "C" => self.cast("Ljava/lang/Character;"),
                        "Z" => self.cast("Ljava/lang/Boolean;"),
                        _ => self.cast("Ljava/lang/Number;"),
                    };
                    self.unbox(target);
                }
            }
        } else if src != target {
            self.cast(target);
        }
        self
    }

    /// The return instruction of a value of kind `kind`.
    fn ret(&mut self, kind: Kind) -> &mut Self {
        self.op(match kind {
            Kind::Int => 0xac,
            Kind::Long => 0xad,
            Kind::Float => 0xae,
            Kind::Double => 0xaf,
            Kind::Ref => 0xb0,
            Kind::Void => 0xb1,
        })
    }
}

/// The spun class of a proxy ([`lambda_proxy_spun_class`]).
#[derive(Debug, Clone)]
pub struct SpunClass {
    /// The constant pool, entry 1 first, as HotSpot holds it (the hidden
    /// name appended, the class entry pointing at it).
    pub pool: Vec<PoolEntry>,
    /// Each method's code, in the methods' order.
    pub codes: Vec<Vec<u8>>,
    /// Each forwarding method's invoking instruction (the location of its
    /// frame while it runs the implementation); `None` for the others.
    pub invoke_at: Vec<Option<usize>>,
}

impl SpunClass {
    /// `GetConstantPool`'s answer: `constant_pool_count` and the entries in
    /// class-file form.
    pub fn constant_pool(&self) -> (u16, Vec<u8>) {
        let mut bytes = Vec::new();
        for entry in &self.pool {
            entry.write(&mut bytes);
        }
        (
            u16::try_from(self.pool.len() + 1).unwrap_or(u16::MAX),
            bytes,
        )
    }
}

/// The class `InnerClassLambdaMetafactory` spins for `site` in host class
/// `host` -- its `methods` ([`lambda_proxy_methods`]) and its direct
/// `interfaces` (internal names, in its order) -- as HotSpot holds it once
/// defined as hidden class `proxy`; `None` when the model cannot tell (an
/// implementation the class store does not know enough of).
pub fn lambda_proxy_spun_class(
    site: &LambdaCallSite,
    host: &str,
    methods: &[LambdaProxyMethod],
    interfaces: &[&str],
    proxy: ClassId,
    lookup: &dyn LambdaCodeLookup,
) -> Option<SpunClass> {
    let captures = capture_descriptors(site);
    let mut pool = Pool::default();
    let this = pool.class(&format!("{host}$$Lambda"));
    for (i, capture) in captures.iter().enumerate() {
        pool.utf8(&format!("arg${}", i + 1));
        pool.utf8(capture);
    }
    let mut codes = Vec::with_capacity(methods.len());
    let mut invoke_at = Vec::with_capacity(methods.len());
    let mut hostile = false;
    for method in methods {
        pool.utf8(&method.name);
        pool.utf8(&method.descriptor);
        let mut code = Code {
            bytes: Vec::new(),
            pool: &mut pool,
        };
        let mut at = None;
        match (&*method.name, method.access_flags) {
            ("<init>", _) => {
                code.op(0x2a)
                    .invoke(0xb7, "java/lang/Object", "<init>", "()V", false);
                let mut slot = 1;
                for (i, capture) in captures.iter().enumerate() {
                    code.op(0x2a).load(kind_of(capture), slot);
                    let field = code.pool.field(this, &format!("arg${}", i + 1), capture);
                    code.indexed(0xb5, field);
                    slot += descriptor_slots(capture);
                }
                code.op(0xb1);
            }
            ("writeReplace", ACC_PRIVATE_FINAL) => {
                let handle = &site.impl_handle;
                code.class_op(0xbb, "java/lang/invoke/SerializedLambda")
                    .op(0x59);
                let constants = [
                    code.pool.class(host),
                    code.pool.string(&site.functional_interface),
                    code.pool.string(&site.sam_method_name),
                    code.pool.string(&site.sam_descriptor),
                ];
                for index in constants {
                    code.ldc(index);
                }
                let kind = code
                    .pool
                    .put(PoolEntry::Integer(i32::from(handle.kind.as_tag())));
                code.ldc(kind);
                let constants = [
                    code.pool.string(&handle.class_name),
                    code.pool.string(&handle.member_name),
                    code.pool.string(&handle.descriptor),
                    code.pool.string(&site.instantiated_descriptor),
                ];
                for index in constants {
                    code.ldc(index);
                }
                code.int_constant(captures.len())
                    .class_op(0xbd, "java/lang/Object");
                for (i, capture) in captures.iter().enumerate() {
                    code.op(0x59).int_constant(i).op(0x2a);
                    let field = code.pool.field(this, &format!("arg${}", i + 1), capture);
                    code.indexed(0xb4, field);
                    if is_primitive(capture) {
                        code.box_(capture);
                    }
                    code.op(0x53);
                }
                code.invoke(
                    0xb7,
                    "java/lang/invoke/SerializedLambda",
                    "<init>",
                    "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;ILjava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;[Ljava/lang/Object;)V",
                    false,
                )
                .op(0xb0);
            }
            ("writeObject" | "readObject", ACC_PRIVATE_FINAL) => {
                hostile = true;
                // The `Exceptions` attribute's class, asked for before the code.
                code.pool.class("java/io/NotSerializableException");
                code.class_op(0xbb, "java/io/NotSerializableException")
                    .op(0x59);
                let message = code.pool.string("Non-serializable lambda");
                code.ldc(message)
                    .invoke(
                        0xb7,
                        "java/io/NotSerializableException",
                        "<init>",
                        "(Ljava/lang/String;)V",
                        false,
                    )
                    .op(0xbf);
            }
            _ => {
                at = Some(forwarding_code(
                    &mut code,
                    site,
                    host,
                    this,
                    &captures,
                    &method.descriptor,
                    lookup,
                )?);
            }
        }
        codes.push(code.bytes);
        invoke_at.push(at);
    }
    // As the class is written: its superclass and interfaces, then the
    // attribute names.
    pool.class("java/lang/Object");
    for interface in interfaces {
        pool.class(interface);
    }
    pool.utf8("Code");
    if hostile {
        pool.utf8("Exceptions");
    }
    if pool.bootstraps {
        pool.utf8("BootstrapMethods");
    }
    // HotSpot's hidden class: its own name appended, the class entry
    // pointing at it.
    pool.entries.push(PoolEntry::Utf8(format!(
        "{host}$$Lambda+0x{:016x}",
        proxy.as_u32()
    )));
    let hidden = u16::try_from(pool.entries.len()).ok()?;
    if let Some(entry) = pool.entries.get_mut(usize::from(this) - 1) {
        *entry = PoolEntry::Class(hidden);
    }
    Some(SpunClass {
        pool: pool.entries,
        codes,
        invoke_at,
    })
}

/// [`lambda_proxy_spun_class`] of lambda proxy `id` in `shared`, with the
/// class store as its lookup; `None` when `id` is no proxy or the model
/// cannot tell. Cold: a tool's question.
#[cold]
#[inline(never)]
pub fn lambda_proxy_spun_class_of(shared: &SharedVm, id: ClassId) -> Option<SpunClass> {
    let site = shared.classes.lambda_proxies.read().get(&id).cloned()?;
    let facts = lambda_proxy_facts(shared, id)?;
    let interfaces: Vec<String> = {
        let cm = shared.classes.class_manager.read();
        facts
            .interfaces
            .iter()
            .filter_map(|&i| cm.get_class(i).map(|c| c.name.to_string()))
            .collect()
    };
    let interfaces: Vec<&str> = interfaces.iter().map(String::as_str).collect();
    lambda_proxy_spun_class(
        &site,
        &facts.host_name,
        &facts.methods,
        &interfaces,
        id,
        &StoreLookup(shared),
    )
}

/// The location of a frame of method `index` of lambda proxy `id` in
/// `shared` while it runs the implementation: its invoking instruction in
/// the spun code (interpreter round i2 wave 60, lane L1; MEASURED,
/// `L1W60JvmtiLambdaFrames`). `None` for any other method, or when the
/// model cannot tell. Cold: a tool's question.
#[cold]
#[inline(never)]
pub fn lambda_proxy_invoke_location(shared: &SharedVm, id: ClassId, index: usize) -> Option<usize> {
    lambda_proxy_spun_class_of(shared, id)?
        .invoke_at
        .get(index)
        .copied()
        .flatten()
}

/// What a debugger's listing shows of a lambda proxy method's frame: the
/// method's name and descriptor and the frame's location, the index of the
/// forwarding instruction in the modelled spun code (the i54-L1 retirement).
/// Built once per proxy method ([`lambda_proxy_row`]) and kept by the VM
/// (`DebugRealm::lambda_proxy_rows`), so that a listing made under the
/// debug-state lock never reads the class store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LambdaProxyRow {
    pub name: Arc<str>,
    pub descriptor: Arc<str>,
    pub location: i64,
}

/// [`LambdaProxyRow`] of method `index` of lambda proxy `id`; `None` when
/// `id` is no proxy or `index` no method of it. Reads the class store. Cold.
#[cold]
#[inline(never)]
pub fn lambda_proxy_row(shared: &SharedVm, id: ClassId, index: usize) -> Option<LambdaProxyRow> {
    let facts = lambda_proxy_facts(shared, id)?;
    let method = facts.methods.get(index)?;
    let location = lambda_proxy_invoke_location(shared, id, index)
        .and_then(|at| i64::try_from(at).ok())
        .unwrap_or(0);
    Some(LambdaProxyRow {
        name: Arc::clone(&method.name),
        descriptor: Arc::clone(&method.descriptor),
        location,
    })
}

/// The index, among the methods [`lambda_proxy_methods`] lists for lambda
/// proxy `id`, of the forwarding method a call with `descriptor` enters:
/// the functional method (1) for its own descriptor, else the
/// `FLAG_BRIDGES` bridge with that descriptor (interpreter round i2 wave
/// 60, lane L1). The functional method when none matches. Cold: asked only
/// while a C JVMTI env lists a proxy's frames.
#[cold]
#[inline(never)]
pub fn lambda_proxy_forwarding_index(
    shared: &SharedVm,
    id: ClassId,
    site: &LambdaCallSite,
    descriptor: &str,
) -> u16 {
    if *site.sam_descriptor == *descriptor {
        return 1;
    }
    crate::runtime::invokedynamic::lambda_proxy_bridge_descriptors(shared.vm_identity, id)
        .and_then(|bridges| bridges.iter().position(|b| **b == *descriptor))
        .and_then(|at| u16::try_from(at + 2).ok())
        .unwrap_or(1)
}

/// The spun code of method `index` of lambda proxy `id` in `shared`
/// ([`lambda_proxy_spun_class_of`]); `None` when `id` is no proxy, `index`
/// no method of it, or the model cannot tell. Cold: a tool's question.
#[cold]
#[inline(never)]
pub fn lambda_proxy_code(shared: &SharedVm, id: ClassId, index: usize) -> Option<Vec<u8>> {
    lambda_proxy_spun_class_of(shared, id)?
        .codes
        .into_iter()
        .nth(index)
}

/// [`LambdaCodeLookup`] over a VM's class store (classes by internal name,
/// as `lambda_proxy_facts` finds the host's).
struct StoreLookup<'a>(&'a SharedVm);

impl LambdaCodeLookup for StoreLookup<'_> {
    fn method_flags(&self, class: &str, name: &str, descriptor: &str) -> Option<u16> {
        let id = class_id_named(self.0, class)?;
        let cm = self.0.classes.class_manager.read();
        cm.get_class(id)?
            .methods
            .iter()
            .find(|m| &*m.name == name && &*m.descriptor == descriptor)
            .map(|m| m.access_flags.bits())
    }

    fn is_interface(&self, class: &str) -> Option<bool> {
        let id = class_id_named(self.0, class)?;
        let cm = self.0.classes.class_manager.read();
        Some(cm.get_class(id)?.access_flags.bits() & 0x0200 != 0)
    }
}

/// `MethodHandles.classData`'s descriptor (`ConstantDescs.BSM_CLASS_DATA`).
const CLASS_DATA_DESCRIPTOR: &str =
    "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/Object;";

/// `InnerClassLambdaMetafactory.forwardingMethod(methodType)`: the
/// functional method's code, and each bridge's, appended to `code` (`this`
/// the spun class's entry); answers the offset, from where it started, of
/// the instruction that invokes the implementation (wave 60: the location of
/// a proxy's frame).
fn forwarding_code(
    code: &mut Code<'_>,
    site: &LambdaCallSite,
    host: &str,
    this: u16,
    captures: &[String],
    method_type: &str,
    lookup: &dyn LambdaCodeLookup,
) -> Option<usize> {
    use crate::classloading::resolution::MethodHandleKind as K;
    let handle = &site.impl_handle;
    let impl_class = &*handle.class_name;
    let flags = || lookup.method_flags(impl_class, &handle.member_name, &handle.descriptor);
    // `AbstractValidatingLambdaMetafactory`'s normalisation of the kind,
    // then `useImplMethodHandle`.
    let interface = || match handle.kind {
        K::InvokeInterface => Some(true),
        _ => lookup.is_interface(impl_class),
    };
    let (kind, instance) = match handle.kind {
        K::InvokeVirtual | K::InvokeInterface => (
            if interface()? {
                K::InvokeInterface
            } else {
                K::InvokeVirtual
            },
            true,
        ),
        K::InvokeSpecial => {
            if impl_class == host && flags()? & ACC_PRIVATE_MEMBER != 0 {
                (
                    if interface()? {
                        K::InvokeInterface
                    } else {
                        K::InvokeVirtual
                    },
                    true,
                )
            } else {
                (K::InvokeSpecial, true)
            }
        }
        K::InvokeStatic | K::NewInvokeSpecial => (handle.kind, false),
        _ => return None,
    };
    let package = |name: &str| name.rsplit_once('/').map_or("", |(p, _)| p).to_string();
    let protected_elsewhere = kind != K::NewInvokeSpecial
        && package(host) != package(impl_class)
        && flags().is_some_and(|f| f & ACC_PROTECTED != 0);
    let use_handle = protected_elsewhere || kind == K::InvokeSpecial;
    // `implMethodType`: the receiver first for an instance method; a
    // constructor returns its class.
    let mut impl_params = Vec::new();
    if instance {
        impl_params.push(format!("L{impl_class};"));
    }
    impl_params.extend(param_descriptors(&handle.descriptor));
    let impl_return = if kind == K::NewInvokeSpecial {
        format!("L{impl_class};")
    } else {
        return_descriptor(&handle.descriptor).to_string()
    };
    let sam_params = param_descriptors(method_type);
    let dynamic_params = param_descriptors(&site.instantiated_descriptor);
    let start = code.bytes.len();
    if kind == K::NewInvokeSpecial {
        code.class_op(0xbb, impl_class).op(0x59);
    }
    if use_handle {
        // `ldc` of the class data: a `CONSTANT_Dynamic` whose bootstrap is
        // `MethodHandles.classData`, named `_`, of type `MethodHandle`.
        let reference = code.pool.method(
            "java/lang/invoke/MethodHandles",
            "classData",
            CLASS_DATA_DESCRIPTOR,
            false,
        );
        code.pool.put(PoolEntry::MethodHandle(6, reference));
        code.pool.bootstraps = true;
        let nt = code
            .pool
            .name_and_type("_", "Ljava/lang/invoke/MethodHandle;");
        let dynamic = code.pool.put(PoolEntry::Dynamic(0, nt));
        code.ldc(dynamic);
    }
    for (i, capture) in captures.iter().enumerate() {
        code.op(0x2a);
        let field = code.pool.field(this, &format!("arg${}", i + 1), capture);
        code.indexed(0xb4, field);
    }
    let mut slot = 1;
    for (i, arg) in sam_params.iter().enumerate() {
        code.load(kind_of(arg), slot);
        slot += descriptor_slots(arg);
        let target = impl_params.get(captures.len() + i)?;
        let functional = dynamic_params.get(i)?;
        code.convert(arg, target, functional);
    }
    let invoke_at = code.bytes.len() - start;
    if use_handle {
        let mut exact = String::from("(");
        if kind != K::InvokeStatic {
            exact.push_str(&format!("L{impl_class};"));
        }
        exact.push_str(handle.descriptor.strip_prefix('(')?);
        code.invoke(
            0xb6,
            "java/lang/invoke/MethodHandle",
            "invokeExact",
            &exact,
            false,
        );
    } else {
        let op = match kind {
            K::InvokeStatic => 0xb8,
            K::InvokeVirtual => 0xb6,
            K::InvokeSpecial | K::NewInvokeSpecial => 0xb7,
            _ => 0xb9,
        };
        let interface = op == 0xb9 || lookup.is_interface(impl_class) == Some(true);
        code.invoke(
            op,
            impl_class,
            &handle.member_name,
            &handle.descriptor,
            interface,
        );
    }
    let sam_return = return_descriptor(method_type);
    code.convert(&impl_return, sam_return, sam_return)
        .ret(kind_of(sam_return));
    Some(invoke_at)
}

/// `text` in a `CONSTANT_Utf8`'s modified UTF-8 (NUL as `C0 80`, a
/// supplementary character as its two surrogates; no terminator).
fn modified_utf8(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len());
    let mut units = [0u16; 2];
    for c in text.chars() {
        if c != '\0' && c.is_ascii() {
            bytes.push(c as u8); // Cast: an ASCII character is one byte
            continue;
        }
        for unit in c.encode_utf16(&mut units).iter() {
            let u = u32::from(*unit);
            if u != 0 && u < 0x80 {
                bytes.push(u as u8); // Cast: below 0x80
            } else if u < 0x800 {
                // Truncation: each byte takes the bits its mask selects.
                bytes.push((0xC0 | (u >> 6)) as u8);
                bytes.push((0x80 | (u & 0x3F)) as u8);
            } else {
                // Truncation: as above.
                bytes.push((0xE0 | (u >> 12)) as u8);
                bytes.push((0x80 | ((u >> 6) & 0x3F)) as u8);
                bytes.push((0x80 | (u & 0x3F)) as u8);
            }
        }
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classloading::resolution::{MethodHandle, MethodHandleKind};

    fn mk_handle(kind: MethodHandleKind) -> MethodHandle {
        MethodHandle {
            kind,
            class_name: std::sync::Arc::from("com/example/Foo"),
            member_name: std::sync::Arc::from("bar"),
            descriptor: std::sync::Arc::from("()V"),
        }
    }

    #[test]
    fn impl_kind_classification_covers_every_reference_kind() {
        assert_eq!(
            ImplKind::from_method_handle(&mk_handle(MethodHandleKind::InvokeStatic)),
            ImplKind::Static
        );
        assert_eq!(
            ImplKind::from_method_handle(&mk_handle(MethodHandleKind::InvokeVirtual)),
            ImplKind::Virtual
        );
        assert_eq!(
            ImplKind::from_method_handle(&mk_handle(MethodHandleKind::InvokeInterface)),
            ImplKind::Virtual
        );
        assert_eq!(
            ImplKind::from_method_handle(&mk_handle(MethodHandleKind::InvokeSpecial)),
            ImplKind::Special
        );
        assert_eq!(
            ImplKind::from_method_handle(&mk_handle(MethodHandleKind::NewInvokeSpecial)),
            ImplKind::Constructor
        );
        assert_eq!(
            ImplKind::from_method_handle(&mk_handle(MethodHandleKind::GetField)),
            ImplKind::Getter
        );
        assert_eq!(
            ImplKind::from_method_handle(&mk_handle(MethodHandleKind::PutStatic)),
            ImplKind::Setter
        );
    }

    /// A lookup that knows nothing but what a test says.
    struct NoLookup(Option<u16>);

    impl LambdaCodeLookup for NoLookup {
        fn method_flags(&self, _: &str, _: &str, _: &str) -> Option<u16> {
            self.0
        }

        fn is_interface(&self, _: &str) -> Option<bool> {
            Some(false)
        }
    }

    /// A call site of host `K54Cp` whose implementation is static
    /// `K54Cp.<member><descriptor>`.
    fn k54_site(
        interface: &str,
        sam: &str,
        sam_desc: &str,
        inst: &str,
        member: &str,
        descriptor: &str,
        captures: Vec<char>,
        serial: bool,
    ) -> LambdaCallSite {
        LambdaCallSite {
            functional_interface_id: None,
            functional_interface: Arc::from(interface),
            sam_method_name: Arc::from(sam),
            sam_descriptor: Arc::from(sam_desc),
            impl_handle: MethodHandle {
                kind: MethodHandleKind::InvokeStatic,
                class_name: Arc::from("K54Cp"),
                member_name: Arc::from(member),
                descriptor: Arc::from(descriptor),
            },
            instantiated_descriptor: Arc::from(inst),
            capture_types: captures,
            proxy_class_id: ClassId::new(0x8000_0002),
            serializable_flag: serial,
        }
    }

    /// The spun class of `site` with `interfaces`, with no class store.
    fn spun(site: &LambdaCallSite, serial: LambdaProxySerial, interfaces: &[&str]) -> SpunClass {
        lambda_proxy_spun_class(
            site,
            "K54Cp",
            &lambda_proxy_methods(site, &[], serial),
            interfaces,
            ClassId::new(0x8000_0002),
            &NoLookup(None),
        )
        .expect("modelled")
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn utf8(s: &str) -> PoolEntry {
        PoolEntry::Utf8(s.to_string())
    }

    const HIDDEN: &str = "K54Cp$$Lambda+0x0000000080000002";

    /// The i54-L1 retirement: the spun class's constant pool and code, entry
    /// for entry and byte for byte as HotSpot 25.0.3 holds them (MEASURED
    /// with `GetConstantPool` / `GetBytecodes`,
    /// `tools/probes/interp/L1/L1K54JvmtiLambdaProxyConstantPool.java`; the
    /// hidden suffix is the VM's own id). Wave 57 measured the lengths and
    /// first opcodes and left every pool operand 0.
    #[test]
    fn a_lambda_proxys_spun_class_is_the_metafactorys() {
        use PoolEntry::*;
        // `Runnable plain = K54Cp::hit;`
        let plain = k54_site(
            "java/lang/Runnable",
            "run",
            "()V",
            "()V",
            "hit",
            "()V",
            vec![],
            false,
        );
        let class = spun(&plain, LambdaProxySerial::None, &["java/lang/Runnable"]);
        assert_eq!(
            class.pool,
            vec![
                utf8("K54Cp$$Lambda"),
                Class(18),
                utf8("<init>"),
                utf8("()V"),
                utf8("java/lang/Object"),
                Class(5),
                NameAndType(3, 4),
                Methodref(6, 7),
                utf8("run"),
                utf8("K54Cp"),
                Class(10),
                utf8("hit"),
                NameAndType(12, 4),
                Methodref(11, 13),
                utf8("java/lang/Runnable"),
                Class(15),
                utf8("Code"),
                utf8(HIDDEN),
            ]
        );
        assert_eq!(
            class.codes.iter().map(|c| hex(c)).collect::<Vec<_>>(),
            vec!["2ab70008b1", "b8000eb1"]
        );
        assert_eq!(class.invoke_at, vec![None, Some(0)]);
        assert_eq!(class.constant_pool().0, 19);
        // `Supplier<String> capturing = () -> text + number;` (a `String`
        // and a `long`).
        let capturing = k54_site(
            "java/util/function/Supplier",
            "get",
            "()Ljava/lang/Object;",
            "()Ljava/lang/String;",
            "lambda$main$0",
            "(Ljava/lang/String;J)Ljava/lang/String;",
            vec!['L', 'J'],
            false,
        );
        let class = spun(
            &capturing,
            LambdaProxySerial::None,
            &["java/util/function/Supplier"],
        );
        assert_eq!(
            class.pool,
            vec![
                utf8("K54Cp$$Lambda"),
                Class(29),
                utf8("arg$1"),
                utf8("Ljava/lang/String;"),
                utf8("arg$2"),
                utf8("J"),
                utf8("<init>"),
                utf8("(Ljava/lang/String;J)V"),
                utf8("java/lang/Object"),
                Class(9),
                utf8("()V"),
                NameAndType(7, 11),
                Methodref(10, 12),
                NameAndType(3, 4),
                Fieldref(2, 14),
                NameAndType(5, 6),
                Fieldref(2, 16),
                utf8("get"),
                utf8("()Ljava/lang/Object;"),
                utf8("K54Cp"),
                Class(20),
                utf8("lambda$main$0"),
                utf8("(Ljava/lang/String;J)Ljava/lang/String;"),
                NameAndType(22, 23),
                Methodref(21, 24),
                utf8("java/util/function/Supplier"),
                Class(26),
                utf8("Code"),
                utf8(HIDDEN),
            ]
        );
        assert_eq!(
            class.codes.iter().map(|c| hex(c)).collect::<Vec<_>>(),
            vec!["2ab7000d2a2bb5000f2a20b50011b1", "2ab4000f2ab40011b80019b0"]
        );
        // `Function<Integer, String> unboxing = K54Cp::twice;` over
        // `twice(int)`: `checkcast Integer; invokevirtual Number.intValue`.
        let unboxing = k54_site(
            "java/util/function/Function",
            "apply",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            "(Ljava/lang/Integer;)Ljava/lang/String;",
            "twice",
            "(I)Ljava/lang/String;",
            vec![],
            false,
        );
        let class = spun(
            &unboxing,
            LambdaProxySerial::None,
            &["java/util/function/Function"],
        );
        assert_eq!(
            class.codes[1],
            vec![0x2b, 0xc0, 0, 12, 0xb6, 0, 18, 0xb8, 0, 24, 0xb0]
        );
        assert_eq!(class.pool[12], utf8("java/lang/Number"));
        assert_eq!(class.pool[14], utf8("intValue"));
        assert_eq!(class.pool.len(), 28);
        assert_eq!(class.invoke_at[1], Some(7));
        // `(Runnable & Serializable) K54Cp::hit`: `writeReplace`.
        let serializable = k54_site(
            "java/lang/Runnable",
            "run",
            "()V",
            "()V",
            "hit",
            "()V",
            vec![],
            true,
        );
        let class = spun(
            &serializable,
            LambdaProxySerial::Friendly,
            &["java/lang/Runnable", "java/io/Serializable"],
        );
        assert_eq!(
            hex(&class.codes[2]),
            "bb001259120b1214121512161217121812191216121603bd0006b7001cb0"
        );
        assert_eq!(
            &class.pool[19..24],
            &[String(19), String(9), String(4), Integer(6), String(10)]
        );
        assert_eq!(
            &class.pool[28..],
            &[
                Class(19),
                utf8("java/io/Serializable"),
                Class(30),
                utf8("Code"),
                utf8(HIDDEN)
            ]
        );
        assert_eq!(class.invoke_at, vec![None, Some(0), None]);
        // The hostile pair: the `Exceptions` attribute's class before the
        // code, its name after `Code` (by the generator's rules, not
        // measured: javac makes no such lambda).
        let class = spun(&plain, LambdaProxySerial::Hostile, &["java/lang/Runnable"]);
        let codes: Vec<std::string::String> = class.codes.iter().map(|c| hex(c)).collect();
        assert_eq!(codes[2], codes[3]);
        assert_eq!(codes[2].len(), 20);
        assert_eq!(class.pool[class.pool.len() - 2], utf8("Exceptions"));
    }

    /// Interpreter round i2 wave 56, lane L1: the methods of the class
    /// `InnerClassLambdaMetafactory` spins, in its order and with the facts
    /// HotSpot 25 answers for them (measured, `L1W56JvmtiLambdaMethods`).
    #[test]
    fn a_lambda_proxys_methods_are_the_spun_classs() {
        let site = |kind: MethodHandleKind, owner: &str, descriptor: &str, captures: Vec<char>| {
            LambdaCallSite {
                functional_interface_id: None,
                functional_interface: Arc::from("java/util/function/Function"),
                sam_method_name: Arc::from("apply"),
                sam_descriptor: Arc::from("(Ljava/lang/Object;)Ljava/lang/Object;"),
                impl_handle: MethodHandle {
                    kind,
                    class_name: Arc::from(owner),
                    member_name: Arc::from("m"),
                    descriptor: Arc::from(descriptor),
                },
                instantiated_descriptor: Arc::from("(Ljava/lang/Integer;)Ljava/lang/String;"),
                capture_types: captures,
                proxy_class_id: ClassId::new(0x8000_0001),
                serializable_flag: false,
            }
        };
        let shape = |methods: Vec<LambdaProxyMethod>| -> Vec<(String, u16, u16)> {
            methods
                .into_iter()
                .map(|m| {
                    (
                        format!("{}{}", m.name, m.descriptor),
                        m.access_flags,
                        m.args_size,
                    )
                })
                .collect()
        };
        // A static body capturing a String and a long (measured: `<init>`
        // 0x2 with 4 slots, `apply` 0x1 with 2).
        let capturing = site(
            MethodHandleKind::InvokeStatic,
            "p/Host",
            "(Ljava/lang/String;JLjava/lang/Integer;)Ljava/lang/String;",
            vec!['L', 'J'],
        );
        assert_eq!(
            shape(lambda_proxy_methods(
                &capturing,
                &[],
                LambdaProxySerial::None
            )),
            vec![
                ("<init>(Ljava/lang/String;J)V".to_string(), 0x2, 4),
                (
                    "apply(Ljava/lang/Object;)Ljava/lang/Object;".to_string(),
                    0x1,
                    2
                ),
            ]
        );
        // A bound receiver is typed by the method's class; a bridge and the
        // serialization methods follow the functional method.
        let bound = site(
            MethodHandleKind::InvokeVirtual,
            "p/Recv",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            vec!['L'],
        );
        let bridges: Vec<Arc<str>> = vec![Arc::from("(Ljava/lang/String;)Ljava/lang/Object;")];
        assert_eq!(
            shape(lambda_proxy_methods(
                &bound,
                &bridges,
                LambdaProxySerial::Friendly
            )),
            vec![
                ("<init>(Lp/Recv;)V".to_string(), 0x2, 2),
                (
                    "apply(Ljava/lang/Object;)Ljava/lang/Object;".to_string(),
                    0x1,
                    2
                ),
                (
                    "apply(Ljava/lang/String;)Ljava/lang/Object;".to_string(),
                    0x41,
                    2
                ),
                ("writeReplace()Ljava/lang/Object;".to_string(), 0x12, 1),
            ]
        );
        // A capture whose type disagrees with the body's parameter is spelled
        // by its kind; the hostile pair for an unflagged serializable one.
        let odd = site(
            MethodHandleKind::InvokeStatic,
            "p/Host",
            "(I)Ljava/lang/Object;",
            vec!['L', 'D'],
        );
        assert_eq!(
            shape(lambda_proxy_methods(&odd, &[], LambdaProxySerial::Hostile)),
            vec![
                ("<init>(Ljava/lang/Object;D)V".to_string(), 0x2, 4),
                (
                    "apply(Ljava/lang/Object;)Ljava/lang/Object;".to_string(),
                    0x1,
                    2
                ),
                (
                    "writeObject(Ljava/io/ObjectOutputStream;)V".to_string(),
                    0x12,
                    2
                ),
                (
                    "readObject(Ljava/io/ObjectInputStream;)V".to_string(),
                    0x12,
                    2
                ),
            ]
        );
    }

    #[test]
    fn is_instance_predicate_matches_spec() {
        assert!(ImplKind::Virtual.is_instance());
        assert!(ImplKind::Special.is_instance());
        assert!(ImplKind::Getter.is_instance());
        assert!(ImplKind::Setter.is_instance());
        assert!(!ImplKind::Static.is_instance());
        assert!(!ImplKind::Constructor.is_instance());
    }

    /// Guards the JDK's serializability rule for lambda proxies
    /// (`SharedVm::lambda_proxy_serializability`).
    ///
    /// Before 2026-08-01 every lambda proxy was treated as `Serializable`:
    /// `getDeclaredMethods()` reported a synthetic `writeReplace()` on all of
    /// them and the `instanceof` fast path answered `true` for
    /// `java.io.Serializable` unconditionally, so a plain
    /// `Supplier<String> s = () -> "x"` looked serializable where real HotSpot
    /// throws ClassCastException on `(Serializable) s`.
    ///
    /// This asserts the two arms a bare fixture can decide without a loaded
    /// class hierarchy: the recorded `FLAG_SERIALIZABLE` alone must drive
    /// `ByFlag` vs `NotSerializable`, and a `ClassId` that is not a registered
    /// proxy at all must come back `NotSerializable` rather than defaulting to
    /// serializable. (`ByInheritance` needs a real `Serializable`-extending
    /// interface loaded, so the `LambdaSerProbe` differential probe against
    /// jdk-25 covers that arm.)
    ///
    /// NB: the sibling lambda-proxy tests in `vm/src/vm.rs` live behind
    /// `#[cfg(all(test, feature = "synthetic-jdk"))]` and do NOT run by
    /// default -- this one is deliberately here so it actually executes.
    #[test]
    fn lambda_proxy_serializability_follows_the_recorded_flag() {
        use crate::classloading::resolution::LambdaCallSite;
        use crate::config::VmConfig;
        use crate::vm::SharedVm;
        use cratonvm_native_api::LambdaSerializability;
        use cratonvm_types::ClassId;
        use std::sync::Arc;

        let shared = Arc::new(SharedVm::new(VmConfig::default()));

        let register = |flag: bool| {
            let id = shared.alloc_lambda_proxy_id();
            shared.classes.lambda_proxies.write().insert(
                id,
                std::sync::Arc::new(LambdaCallSite {
                    functional_interface_id: None,
                    // `Runnable` does not extend `Serializable`, so the
                    // inheritance half of the rule is false either way and the
                    // recorded flag is the only thing under test.
                    functional_interface: std::sync::Arc::from("java/lang/Runnable"),
                    sam_method_name: std::sync::Arc::from("run"),
                    sam_descriptor: std::sync::Arc::from("()V"),
                    impl_handle: mk_handle(MethodHandleKind::InvokeStatic),
                    instantiated_descriptor: std::sync::Arc::from("()V"),
                    capture_types: vec![],
                    proxy_class_id: id,
                    serializable_flag: flag,
                }),
            );
            id
        };

        let plain = register(false);
        let ser = register(true);

        assert_eq!(
            shared.lambda_proxy_serializability(plain),
            LambdaSerializability::NotSerializable,
            "a plain metafactory lambda over a non-Serializable interface must not \
             be serializable"
        );
        assert_eq!(
            shared.lambda_proxy_serializability(ser),
            LambdaSerializability::ByFlag,
            "FLAG_SERIALIZABLE must make the lambda serializable AND earn it the \
             added java.io.Serializable marker interface"
        );
        assert_eq!(
            shared.lambda_proxy_serializability(ClassId::new(1)),
            LambdaSerializability::NotSerializable,
            "a ClassId that is not a registered lambda proxy must not be reported \
             serializable"
        );
    }
}
