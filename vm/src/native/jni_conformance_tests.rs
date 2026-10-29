//! JIT round 21 wave 20 (lane jniharness), row JNI6-1 of
//! `docs/known-issues/jit/jr21-r18-backlog-retriage-20261015.md` section 2.1:
//! the in-process JNI conformance harness.
//!
//! What it is. Every test here boots a `Vm` (no external JDK: the default
//! `--compatible` configuration with the synthetic library), defines two
//! fixture classes whose bytes are assembled below (so no `javac` and no
//! committed binaries), binds Rust `extern "C"` functions to their `native`
//! methods through `RegisterNatives`, and calls a Java method that calls one
//! of them. The native body then drives the JNI function table exactly as a C
//! library does: it loads slot N of the `JNIEnv` it was handed and calls it
//! (`Jni::slot`), never a Rust function of `jni.rs` by name. So the whole
//! production path runs: the interpreter's native dispatch, its implicit local
//! frame, the in-native transitions of the arm under test, every JNIEnv
//! function's door, and the rethrow of a pending exception at the native's
//! return. No C shim is needed: the table is plain function pointers, and the
//! bare-varargs slots are called through C-variadic function pointer types.
//!
//! What it asserts. HotSpot 25's documented answers: return values, the
//! pending-exception rules (class and message of what a failing function
//! leaves pending, `ExceptionCheck` / `ExceptionOccurred` / `ExceptionClear`),
//! reference kinds and local-reference capacity, string encodings (modified
//! UTF-8), array copy modes, and monitor ownership. A native collects every
//! divergence (`HarnessState::fail`) and the test reports them all at once.
//!
//! The arms. `CRATONVM_JNI_NATIVE_TRANSITIONS` and `CRATONVM_JNI_IN_NATIVE_LAZY`
//! are latched once per process (`jni_switches`' `OnceLock`), so
//! `flags::with_thread_overrides` cannot switch them; the harness sets this
//! thread's whole JNI package instead (`NativeCallTls::switches_override`,
//! test-only):
//!
//! * `Counted` -- the package off (`CRATONVM_JNI_NATIVE_TRANSITIONS=0`, the
//!   default): the native runs as a counted mutator;
//! * `Deposit` -- the package on, the lazy row off
//!   (`CRATONVM_JNI_NATIVE_TRANSITIONS=1 CRATONVM_JNI_IN_NATIVE_LAZY=0`): the
//!   native runs in native with a root deposit;
//! * `Lazy` -- the package on with the lazy row
//!   (`CRATONVM_JNI_NATIVE_TRANSITIONS=1`): a static native with a primitive
//!   argument enters the lazy in-native state (JNILAZY21, FF3-3).
//!
//! Each in-native arm checks, at the native's first line, that the call really
//! is in the state the arm names (a positive control: a harness that passes
//! because the arm never engaged proves nothing).
//!
//! The GC tests. `stress`: a native holds locals (strings, an `int[]`, an
//! object, an object array, a local inside an explicit `PushLocalFrame`, a
//! global and a weak global) across an up-call that allocates and calls
//! `System.gc()`, twice, and every handle still names the right object (all
//! three arms; Generational and G1). `cross`: a native in native waits while
//! ANOTHER thread forces a full collection, then reads its references back
//! (the two in-native arms: the counted arm would hold that pause for the
//! whole C call, the gcd-d2i shape kept by user decision).

use super::*;
use std::cell::RefCell;
use std::ffi::{c_void, CString};
use std::sync::mpsc;
use std::time::Duration;

use crate::config::{GcAlgorithm, VmConfig};
use crate::vm::Vm;

const BASE: &str = "jniharness/Base";
const FIXTURE: &str = "jniharness/Fixture";
/// HotSpot's `jni.h` release modes.
const JNI_COMMIT: JInt = 1;
const JNI_ABORT: JInt = 2;
/// How long a cross-thread step may take before the test reports it.
const WAIT: Duration = Duration::from_secs(120);

// ---------------------------------------------------------------------------
// The fixture classes, assembled here (class file version 52, straight-line
// code only, so no `StackMapTable` is needed)
// ---------------------------------------------------------------------------

const ACC_PUBLIC: u16 = 0x0001;
const ACC_STATIC: u16 = 0x0008;
const ACC_NATIVE: u16 = 0x0100;

const TAG_UTF8: u8 = 1;
const TAG_CLASS: u8 = 7;
const TAG_STRING: u8 = 8;
const TAG_FIELDREF: u8 = 9;
const TAG_METHODREF: u8 = 10;
const TAG_NAME_AND_TYPE: u8 = 12;

const OP_ICONST_1: u8 = 0x04;
const OP_ICONST_2: u8 = 0x05;
const OP_LDC_W: u8 = 0x13;
const OP_ILOAD_0: u8 = 0x1a;
const OP_ILOAD_1: u8 = 0x1b;
const OP_ILOAD_2: u8 = 0x1c;
const OP_LLOAD_0: u8 = 0x1e;
const OP_ALOAD_0: u8 = 0x2a;
const OP_POP: u8 = 0x57;
const OP_DUP: u8 = 0x59;
const OP_IADD: u8 = 0x60;
const OP_IRETURN: u8 = 0xac;
const OP_RETURN: u8 = 0xb1;
const OP_PUTFIELD: u8 = 0xb5;
const OP_INVOKESPECIAL: u8 = 0xb7;
const OP_INVOKESTATIC: u8 = 0xb8;
const OP_NEW: u8 = 0xbb;
const OP_NEWARRAY: u8 = 0xbc;
const OP_ATHROW: u8 = 0xbf;
/// `newarray`'s operand for `int`.
const T_INT: u8 = 10;

/// `opcode` followed by a big-endian constant-pool index.
fn op16(opcode: u8, index: u16) -> [u8; 3] {
    let [hi, lo] = index.to_be_bytes();
    [opcode, hi, lo]
}

struct Code {
    max_stack: u16,
    max_locals: u16,
    bytes: Vec<u8>,
}

/// A minimal class file writer: a constant pool (no de-duplication), fields
/// and methods, no attributes beyond `Code`.
struct ClassFile {
    pool: Vec<Vec<u8>>,
    this_class: u16,
    super_class: u16,
    fields: Vec<[u16; 3]>,
    methods: Vec<Vec<u8>>,
}

impl ClassFile {
    fn new(name: &str, super_name: &str) -> Self {
        let mut c = ClassFile {
            pool: Vec::new(),
            this_class: 0,
            super_class: 0,
            fields: Vec::new(),
            methods: Vec::new(),
        };
        c.this_class = c.class(name);
        c.super_class = c.class(super_name);
        c
    }

    fn add(&mut self, entry: Vec<u8>) -> u16 {
        self.pool.push(entry);
        u16::try_from(self.pool.len()).expect("a small constant pool")
    }

    fn utf8(&mut self, s: &str) -> u16 {
        let mut e = vec![TAG_UTF8];
        e.extend_from_slice(&u16::try_from(s.len()).expect("a short name").to_be_bytes());
        e.extend_from_slice(s.as_bytes());
        self.add(e)
    }

    fn class(&mut self, name: &str) -> u16 {
        let n = self.utf8(name);
        let mut e = vec![TAG_CLASS];
        e.extend_from_slice(&n.to_be_bytes());
        self.add(e)
    }

    fn string(&mut self, value: &str) -> u16 {
        let n = self.utf8(value);
        let mut e = vec![TAG_STRING];
        e.extend_from_slice(&n.to_be_bytes());
        self.add(e)
    }

    /// A `Fieldref` / `Methodref` (`tag`) of `owner.name:desc`.
    fn member(&mut self, tag: u8, owner: &str, name: &str, desc: &str) -> u16 {
        let c = self.class(owner);
        let n = self.utf8(name);
        let d = self.utf8(desc);
        let mut nat = vec![TAG_NAME_AND_TYPE];
        nat.extend_from_slice(&n.to_be_bytes());
        nat.extend_from_slice(&d.to_be_bytes());
        let nat = self.add(nat);
        let mut e = vec![tag];
        e.extend_from_slice(&c.to_be_bytes());
        e.extend_from_slice(&nat.to_be_bytes());
        self.add(e)
    }

    fn field(&mut self, access: u16, name: &str, desc: &str) {
        let n = self.utf8(name);
        let d = self.utf8(desc);
        self.fields.push([access, n, d]);
    }

    /// A method; `None` code for a `native` one.
    fn method(&mut self, access: u16, name: &str, desc: &str, code: Option<Code>) {
        let n = self.utf8(name);
        let d = self.utf8(desc);
        let mut m = Vec::new();
        m.extend_from_slice(&access.to_be_bytes());
        m.extend_from_slice(&n.to_be_bytes());
        m.extend_from_slice(&d.to_be_bytes());
        match code {
            None => m.extend_from_slice(&0u16.to_be_bytes()),
            Some(code) => {
                let attr = self.utf8("Code");
                let mut body = Vec::new();
                body.extend_from_slice(&code.max_stack.to_be_bytes());
                body.extend_from_slice(&code.max_locals.to_be_bytes());
                body.extend_from_slice(&(code.bytes.len() as u32).to_be_bytes());
                body.extend_from_slice(&code.bytes);
                body.extend_from_slice(&0u16.to_be_bytes()); // exception_table_length
                body.extend_from_slice(&0u16.to_be_bytes()); // attributes_count
                m.extend_from_slice(&1u16.to_be_bytes());
                m.extend_from_slice(&attr.to_be_bytes());
                m.extend_from_slice(&(body.len() as u32).to_be_bytes());
                m.extend_from_slice(&body);
            }
        }
        self.methods.push(m);
    }

    fn bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&0xCAFE_BABE_u32.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes()); // minor
        out.extend_from_slice(&52u16.to_be_bytes()); // major: Java 8
        out.extend_from_slice(&(self.pool.len() as u16 + 1).to_be_bytes());
        for entry in &self.pool {
            out.extend_from_slice(entry);
        }
        out.extend_from_slice(&0x0021u16.to_be_bytes()); // ACC_PUBLIC | ACC_SUPER
        out.extend_from_slice(&self.this_class.to_be_bytes());
        out.extend_from_slice(&self.super_class.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes()); // interfaces_count
        out.extend_from_slice(&(self.fields.len() as u16).to_be_bytes());
        for [access, name, desc] in &self.fields {
            out.extend_from_slice(&access.to_be_bytes());
            out.extend_from_slice(&name.to_be_bytes());
            out.extend_from_slice(&desc.to_be_bytes());
            out.extend_from_slice(&0u16.to_be_bytes()); // attributes_count
        }
        out.extend_from_slice(&(self.methods.len() as u16).to_be_bytes());
        for m in &self.methods {
            out.extend_from_slice(m);
        }
        out.extend_from_slice(&0u16.to_be_bytes()); // class attributes_count
        out
    }
}

/// ```text
/// public class jniharness.Base {
///     public int baseValue;
///     public Base(int v) { super(); baseValue = v; }
///     public int describe() { return 1; }
/// }
/// ```
fn base_class_bytes() -> Vec<u8> {
    let mut c = ClassFile::new(BASE, "java/lang/Object");
    c.field(ACC_PUBLIC, "baseValue", "I");
    let object_init = c.member(TAG_METHODREF, "java/lang/Object", "<init>", "()V");
    let base_value = c.member(TAG_FIELDREF, BASE, "baseValue", "I");
    let mut init = vec![OP_ALOAD_0];
    init.extend_from_slice(&op16(OP_INVOKESPECIAL, object_init));
    init.extend_from_slice(&[OP_ALOAD_0, OP_ILOAD_1]);
    init.extend_from_slice(&op16(OP_PUTFIELD, base_value));
    init.push(OP_RETURN);
    c.method(
        ACC_PUBLIC,
        "<init>",
        "(I)V",
        Some(Code {
            max_stack: 2,
            max_locals: 2,
            bytes: init,
        }),
    );
    c.method(
        ACC_PUBLIC,
        "describe",
        "()I",
        Some(Code {
            max_stack: 1,
            max_locals: 1,
            bytes: vec![OP_ICONST_1, OP_IRETURN],
        }),
    );
    c.bytes()
}

/// ```text
/// public class jniharness.Fixture extends jniharness.Base {
///     public int count; public long wide; public Object ref;
///     public static int hits;
///     public Fixture(int v) { super(v); count = v; }
///     public int describe() { return 2; }
///     public int add(int a, int b) { return a + b; }
///     public static int sum(int a, int b) { return a + b; }
///     public static void thrower() { throw new IllegalArgumentException("jni-harness boom"); }
///     public static void churn(int n) { 8 x new int[n]; System.gc(); }
///     static native int harness(long state);   // + runHarness(long) calling it
///     static native int stress(long state);    // + runStress(long)
///     static native int cross(long state);     // + runCross(long)
///     static native void throwFromNative();
/// }
/// ```
fn fixture_class_bytes() -> Vec<u8> {
    let mut c = ClassFile::new(FIXTURE, BASE);
    c.field(ACC_PUBLIC, "count", "I");
    c.field(ACC_PUBLIC, "wide", "J");
    c.field(ACC_PUBLIC, "ref", "Ljava/lang/Object;");
    c.field(ACC_PUBLIC | ACC_STATIC, "hits", "I");
    let base_init = c.member(TAG_METHODREF, BASE, "<init>", "(I)V");
    let count = c.member(TAG_FIELDREF, FIXTURE, "count", "I");
    let mut init = vec![OP_ALOAD_0, OP_ILOAD_1];
    init.extend_from_slice(&op16(OP_INVOKESPECIAL, base_init));
    init.extend_from_slice(&[OP_ALOAD_0, OP_ILOAD_1]);
    init.extend_from_slice(&op16(OP_PUTFIELD, count));
    init.push(OP_RETURN);
    c.method(
        ACC_PUBLIC,
        "<init>",
        "(I)V",
        Some(Code {
            max_stack: 2,
            max_locals: 2,
            bytes: init,
        }),
    );
    c.method(
        ACC_PUBLIC,
        "describe",
        "()I",
        Some(Code {
            max_stack: 1,
            max_locals: 1,
            bytes: vec![OP_ICONST_2, OP_IRETURN],
        }),
    );
    c.method(
        ACC_PUBLIC,
        "add",
        "(II)I",
        Some(Code {
            max_stack: 2,
            max_locals: 3,
            bytes: vec![OP_ILOAD_1, OP_ILOAD_2, OP_IADD, OP_IRETURN],
        }),
    );
    c.method(
        ACC_PUBLIC | ACC_STATIC,
        "sum",
        "(II)I",
        Some(Code {
            max_stack: 2,
            max_locals: 2,
            bytes: vec![OP_ILOAD_0, OP_ILOAD_1, OP_IADD, OP_IRETURN],
        }),
    );
    let iae = c.class("java/lang/IllegalArgumentException");
    let iae_init = c.member(
        TAG_METHODREF,
        "java/lang/IllegalArgumentException",
        "<init>",
        "(Ljava/lang/String;)V",
    );
    let boom = c.string("jni-harness boom");
    let mut thrower = op16(OP_NEW, iae).to_vec();
    thrower.push(OP_DUP);
    thrower.extend_from_slice(&op16(OP_LDC_W, boom));
    thrower.extend_from_slice(&op16(OP_INVOKESPECIAL, iae_init));
    thrower.push(OP_ATHROW);
    c.method(
        ACC_PUBLIC | ACC_STATIC,
        "thrower",
        "()V",
        Some(Code {
            max_stack: 3,
            max_locals: 0,
            bytes: thrower,
        }),
    );
    let gc = c.member(TAG_METHODREF, "java/lang/System", "gc", "()V");
    let mut churn = Vec::new();
    for _ in 0..8 {
        churn.extend_from_slice(&[OP_ILOAD_0, OP_NEWARRAY, T_INT, OP_POP]);
    }
    churn.extend_from_slice(&op16(OP_INVOKESTATIC, gc));
    churn.push(OP_RETURN);
    c.method(
        ACC_PUBLIC | ACC_STATIC,
        "churn",
        "(I)V",
        Some(Code {
            max_stack: 1,
            max_locals: 1,
            bytes: churn,
        }),
    );
    for native in ["harness", "stress", "cross"] {
        c.method(ACC_STATIC | ACC_NATIVE, native, "(J)I", None);
        let target = c.member(TAG_METHODREF, FIXTURE, native, "(J)I");
        let mut run = vec![OP_LLOAD_0];
        run.extend_from_slice(&op16(OP_INVOKESTATIC, target));
        run.push(OP_IRETURN);
        let mut runner = String::from("run");
        runner.push_str(&native[..1].to_ascii_uppercase());
        runner.push_str(&native[1..]);
        c.method(
            ACC_PUBLIC | ACC_STATIC,
            &runner,
            "(J)I",
            Some(Code {
                max_stack: 2,
                max_locals: 2,
                bytes: run,
            }),
        );
    }
    c.method(ACC_STATIC | ACC_NATIVE, "throwFromNative", "()V", None);
    c.bytes()
}

fn write_fixture_classes(dir: &std::path::Path) {
    let package = dir.join("jniharness");
    std::fs::create_dir_all(&package).expect("the fixture package directory");
    std::fs::write(package.join("Base.class"), base_class_bytes()).expect("Base.class");
    std::fs::write(package.join("Fixture.class"), fixture_class_bytes()).expect("Fixture.class");
}

// ---------------------------------------------------------------------------
// The function table, called as a C library calls it
// ---------------------------------------------------------------------------

/// A `JNIEnv*`. Every method loads its `jni.h` slot from the table the env
/// points at and calls it.
#[derive(Clone, Copy)]
struct Jni(JNIEnv);

/// Call slot `$slot` of `$env`'s table as `extern "C" fn(JNIEnv, $ty...) -> $ret`.
macro_rules! jni_call {
    ($env:expr, $slot:expr, fn($($ty:ty),*) -> $ret:ty, $($arg:expr),*) => {{
        let f = $env.slot::<extern "C" fn(JNIEnv $(, $ty)*) -> $ret>($slot);
        f($env.0 $(, $arg)*)
    }};
}

/// `JNINativeMethod` as `jni.h` declares it.
#[repr(C)]
struct NativeMethodEntry {
    name: *const c_char,
    signature: *const c_char,
    fn_ptr: *mut c_void,
}

fn cstr(s: &str) -> CString {
    CString::new(s).expect("no interior NUL in a harness string")
}

impl Jni {
    /// Slot `index` of the table, as the function type `F`.
    fn slot<F: Copy>(self, index: usize) -> F {
        assert!(index < JNI_FUNCTION_COUNT);
        assert_eq!(std::mem::size_of::<F>(), std::mem::size_of::<usize>());
        // SAFETY: a `JNIEnv` points at the table pointer (`get_jni_env`), the
        // table has `JNI_FUNCTION_COUNT` entries, and every caller names the
        // slot's `jni.h` signature as `F` (a function pointer, one word).
        unsafe {
            let table: *const usize = *self.0;
            let addr = *table.add(index);
            std::mem::transmute_copy::<usize, F>(&addr)
        }
    }

    fn get_version(self) -> JInt {
        jni_call!(self, 4, fn() -> JInt,)
    }
    fn find_class(self, name: &str) -> JClass {
        let n = cstr(name);
        jni_call!(self, 6, fn(*const c_char) -> JClass, n.as_ptr())
    }
    fn get_superclass(self, c: JClass) -> JClass {
        jni_call!(self, 10, fn(JClass) -> JClass, c)
    }
    fn is_assignable_from(self, a: JClass, b: JClass) -> JBoolean {
        jni_call!(self, 11, fn(JClass, JClass) -> JBoolean, a, b)
    }
    fn throw(self, t: JThrowable) -> JInt {
        jni_call!(self, 13, fn(JThrowable) -> JInt, t)
    }
    fn throw_new(self, c: JClass, msg: &str) -> JInt {
        let m = cstr(msg);
        jni_call!(self, 14, fn(JClass, *const c_char) -> JInt, c, m.as_ptr())
    }
    fn exception_occurred(self) -> JThrowable {
        jni_call!(self, 15, fn() -> JThrowable,)
    }
    fn exception_clear(self) {
        jni_call!(self, 17, fn() -> (),)
    }
    fn push_local_frame(self, capacity: JInt) -> JInt {
        jni_call!(self, 19, fn(JInt) -> JInt, capacity)
    }
    fn pop_local_frame(self, result: JObject) -> JObject {
        jni_call!(self, 20, fn(JObject) -> JObject, result)
    }
    fn new_global_ref(self, o: JObject) -> JObject {
        jni_call!(self, 21, fn(JObject) -> JObject, o)
    }
    fn delete_global_ref(self, o: JObject) {
        jni_call!(self, 22, fn(JObject) -> (), o)
    }
    fn delete_local_ref(self, o: JObject) {
        jni_call!(self, 23, fn(JObject) -> (), o)
    }
    fn is_same_object(self, a: JObject, b: JObject) -> JBoolean {
        jni_call!(self, 24, fn(JObject, JObject) -> JBoolean, a, b)
    }
    fn new_local_ref(self, o: JObject) -> JObject {
        jni_call!(self, 25, fn(JObject) -> JObject, o)
    }
    fn ensure_local_capacity(self, capacity: JInt) -> JInt {
        jni_call!(self, 26, fn(JInt) -> JInt, capacity)
    }
    fn alloc_object(self, c: JClass) -> JObject {
        jni_call!(self, 27, fn(JClass) -> JObject, c)
    }
    fn new_object_a(self, c: JClass, m: JMethodID, args: &[JValue]) -> JObject {
        jni_call!(
            self,
            30,
            fn(JClass, JMethodID, *const JValue) -> JObject,
            c,
            m,
            args.as_ptr()
        )
    }
    fn get_object_class(self, o: JObject) -> JClass {
        jni_call!(self, 31, fn(JObject) -> JClass, o)
    }
    fn is_instance_of(self, o: JObject, c: JClass) -> JBoolean {
        jni_call!(self, 32, fn(JObject, JClass) -> JBoolean, o, c)
    }
    fn get_method_id(self, c: JClass, name: &str, sig: &str) -> JMethodID {
        let (n, s) = (cstr(name), cstr(sig));
        jni_call!(
            self,
            33,
            fn(JClass, *const c_char, *const c_char) -> JMethodID,
            c,
            n.as_ptr(),
            s.as_ptr()
        )
    }
    fn call_object_method_a(self, o: JObject, m: JMethodID, args: &[JValue]) -> JObject {
        jni_call!(
            self,
            36,
            fn(JObject, JMethodID, *const JValue) -> JObject,
            o,
            m,
            args.as_ptr()
        )
    }
    fn call_int_method_a(self, o: JObject, m: JMethodID, args: &[JValue]) -> JInt {
        jni_call!(
            self,
            51,
            fn(JObject, JMethodID, *const JValue) -> JInt,
            o,
            m,
            args.as_ptr()
        )
    }
    fn call_nonvirtual_int_method_a(
        self,
        o: JObject,
        c: JClass,
        m: JMethodID,
        args: &[JValue],
    ) -> JInt {
        jni_call!(
            self,
            81,
            fn(JObject, JClass, JMethodID, *const JValue) -> JInt,
            o,
            c,
            m,
            args.as_ptr()
        )
    }
    fn get_field_id(self, c: JClass, name: &str, sig: &str) -> JFieldID {
        let (n, s) = (cstr(name), cstr(sig));
        jni_call!(
            self,
            94,
            fn(JClass, *const c_char, *const c_char) -> JFieldID,
            c,
            n.as_ptr(),
            s.as_ptr()
        )
    }
    fn get_object_field(self, o: JObject, f: JFieldID) -> JObject {
        jni_call!(self, 95, fn(JObject, JFieldID) -> JObject, o, f)
    }
    fn get_int_field(self, o: JObject, f: JFieldID) -> JInt {
        jni_call!(self, 100, fn(JObject, JFieldID) -> JInt, o, f)
    }
    fn get_long_field(self, o: JObject, f: JFieldID) -> JLong {
        jni_call!(self, 101, fn(JObject, JFieldID) -> JLong, o, f)
    }
    fn set_object_field(self, o: JObject, f: JFieldID, v: JObject) {
        jni_call!(self, 104, fn(JObject, JFieldID, JObject) -> (), o, f, v)
    }
    fn set_int_field(self, o: JObject, f: JFieldID, v: JInt) {
        jni_call!(self, 109, fn(JObject, JFieldID, JInt) -> (), o, f, v)
    }
    fn set_long_field(self, o: JObject, f: JFieldID, v: JLong) {
        jni_call!(self, 110, fn(JObject, JFieldID, JLong) -> (), o, f, v)
    }
    fn get_static_method_id(self, c: JClass, name: &str, sig: &str) -> JMethodID {
        let (n, s) = (cstr(name), cstr(sig));
        jni_call!(
            self,
            113,
            fn(JClass, *const c_char, *const c_char) -> JMethodID,
            c,
            n.as_ptr(),
            s.as_ptr()
        )
    }
    fn call_static_int_method_a(self, c: JClass, m: JMethodID, args: &[JValue]) -> JInt {
        jni_call!(
            self,
            131,
            fn(JClass, JMethodID, *const JValue) -> JInt,
            c,
            m,
            args.as_ptr()
        )
    }
    fn call_static_void_method_a(self, c: JClass, m: JMethodID, args: &[JValue]) {
        jni_call!(
            self,
            143,
            fn(JClass, JMethodID, *const JValue) -> (),
            c,
            m,
            args.as_ptr()
        )
    }
    fn get_static_field_id(self, c: JClass, name: &str, sig: &str) -> JFieldID {
        let (n, s) = (cstr(name), cstr(sig));
        jni_call!(
            self,
            144,
            fn(JClass, *const c_char, *const c_char) -> JFieldID,
            c,
            n.as_ptr(),
            s.as_ptr()
        )
    }
    fn get_static_int_field(self, c: JClass, f: JFieldID) -> JInt {
        jni_call!(self, 150, fn(JClass, JFieldID) -> JInt, c, f)
    }
    fn set_static_int_field(self, c: JClass, f: JFieldID, v: JInt) {
        jni_call!(self, 159, fn(JClass, JFieldID, JInt) -> (), c, f, v)
    }
    fn new_string(self, units: &[JChar]) -> JString {
        let len = JSize::try_from(units.len()).expect("a short string");
        jni_call!(
            self,
            163,
            fn(*const JChar, JSize) -> JString,
            units.as_ptr(),
            len
        )
    }
    fn get_string_length(self, s: JString) -> JSize {
        jni_call!(self, 164, fn(JString) -> JSize, s)
    }
    /// `NewStringUTF` of `bytes` (modified UTF-8, no NUL).
    fn new_string_utf_bytes(self, bytes: &[u8]) -> JString {
        let c = CString::new(bytes.to_vec()).expect("modified UTF-8 has no NUL byte");
        jni_call!(self, 167, fn(*const c_char) -> JString, c.as_ptr())
    }
    fn new_string_utf(self, text: &str) -> JString {
        self.new_string_utf_bytes(text.as_bytes())
    }
    fn get_string_utf_length(self, s: JString) -> JSize {
        jni_call!(self, 168, fn(JString) -> JSize, s)
    }
    /// `GetStringUTFChars` + `ReleaseStringUTFChars`: the bytes and `isCopy`.
    fn string_utf_chars(self, s: JString) -> Option<(Vec<u8>, JBoolean)> {
        let mut is_copy: JBoolean = 7;
        let p = jni_call!(
            self,
            169,
            fn(JString, *mut JBoolean) -> *const c_char,
            s,
            &mut is_copy
        );
        if p.is_null() {
            return None;
        }
        // SAFETY: `GetStringUTFChars` answered a NUL-terminated buffer that
        // stays valid until the release below.
        let bytes = unsafe { CStr::from_ptr(p) }.to_bytes().to_vec();
        jni_call!(self, 170, fn(JString, *const c_char) -> (), s, p);
        Some((bytes, is_copy))
    }
    fn get_array_length(self, a: JArray) -> JSize {
        jni_call!(self, 171, fn(JArray) -> JSize, a)
    }
    fn new_object_array(self, len: JSize, c: JClass, init: JObject) -> JArray {
        jni_call!(
            self,
            172,
            fn(JSize, JClass, JObject) -> JArray,
            len,
            c,
            init
        )
    }
    fn get_object_array_element(self, a: JArray, i: JSize) -> JObject {
        jni_call!(self, 173, fn(JArray, JSize) -> JObject, a, i)
    }
    fn set_object_array_element(self, a: JArray, i: JSize, v: JObject) {
        jni_call!(self, 174, fn(JArray, JSize, JObject) -> (), a, i, v)
    }
    fn new_int_array(self, len: JSize) -> JArray {
        jni_call!(self, 179, fn(JSize) -> JArray, len)
    }
    fn get_int_array_elements(self, a: JArray, is_copy: *mut JBoolean) -> *mut JInt {
        jni_call!(
            self,
            187,
            fn(JArray, *mut JBoolean) -> *mut JInt,
            a,
            is_copy
        )
    }
    fn release_int_array_elements(self, a: JArray, elems: *mut JInt, mode: JInt) {
        jni_call!(self, 195, fn(JArray, *mut JInt, JInt) -> (), a, elems, mode)
    }
    fn get_int_array_region(self, a: JArray, start: JSize, out: &mut [JInt]) {
        let len = JSize::try_from(out.len()).expect("a short region");
        jni_call!(
            self,
            203,
            fn(JArray, JSize, JSize, *mut JInt) -> (),
            a,
            start,
            len,
            out.as_mut_ptr()
        )
    }
    /// `GetIntArrayRegion` with a caller-chosen `len` into a buffer of at
    /// least `len.max(0)` elements (for the out-of-range rows).
    fn get_int_array_region_len(self, a: JArray, start: JSize, len: JSize, out: &mut [JInt]) {
        assert!(out.len() >= usize::try_from(len.max(0)).unwrap_or(0));
        jni_call!(
            self,
            203,
            fn(JArray, JSize, JSize, *mut JInt) -> (),
            a,
            start,
            len,
            out.as_mut_ptr()
        )
    }
    fn set_int_array_region(self, a: JArray, start: JSize, values: &[JInt]) {
        let len = JSize::try_from(values.len()).expect("a short region");
        jni_call!(
            self,
            211,
            fn(JArray, JSize, JSize, *const JInt) -> (),
            a,
            start,
            len,
            values.as_ptr()
        )
    }
    fn register_natives(self, c: JClass, methods: &[NativeMethodEntry]) -> JInt {
        let n = JInt::try_from(methods.len()).expect("a few natives");
        jni_call!(
            self,
            215,
            fn(JClass, *const NativeMethodEntry, JInt) -> JInt,
            c,
            methods.as_ptr(),
            n
        )
    }
    fn monitor_enter(self, o: JObject) -> JInt {
        jni_call!(self, 217, fn(JObject) -> JInt, o)
    }
    fn monitor_exit(self, o: JObject) -> JInt {
        jni_call!(self, 218, fn(JObject) -> JInt, o)
    }
    fn get_string_region(self, s: JString, start: JSize, len: JSize, out: &mut [JChar]) {
        assert!(out.len() >= usize::try_from(len.max(0)).unwrap_or(0));
        jni_call!(
            self,
            220,
            fn(JString, JSize, JSize, *mut JChar) -> (),
            s,
            start,
            len,
            out.as_mut_ptr()
        )
    }
    fn get_primitive_array_critical(self, a: JArray, is_copy: *mut JBoolean) -> *mut c_void {
        jni_call!(
            self,
            222,
            fn(JArray, *mut JBoolean) -> *mut c_void,
            a,
            is_copy
        )
    }
    fn release_primitive_array_critical(self, a: JArray, p: *mut c_void, mode: JInt) {
        jni_call!(self, 223, fn(JArray, *mut c_void, JInt) -> (), a, p, mode)
    }
    fn get_string_critical(self, s: JString, is_copy: *mut JBoolean) -> *const JChar {
        jni_call!(
            self,
            224,
            fn(JString, *mut JBoolean) -> *const JChar,
            s,
            is_copy
        )
    }
    fn release_string_critical(self, s: JString, p: *const JChar) {
        jni_call!(self, 225, fn(JString, *const JChar) -> (), s, p)
    }
    fn new_weak_global_ref(self, o: JObject) -> JObject {
        jni_call!(self, 226, fn(JObject) -> JObject, o)
    }
    fn delete_weak_global_ref(self, o: JObject) {
        jni_call!(self, 227, fn(JObject) -> (), o)
    }
    fn exception_check(self) -> JBoolean {
        jni_call!(self, 228, fn() -> JBoolean,)
    }
    fn get_object_ref_type(self, o: JObject) -> JInt {
        jni_call!(self, 232, fn(JObject) -> JInt, o)
    }

    // The bare-varargs slots, called as C calls them: through a C-variadic
    // function pointer (the trampolines exist on x86-64 only; elsewhere the
    // slots raise `UnsatisfiedLinkError`).
    #[cfg(target_arch = "x86_64")]
    fn new_object_1(self, c: JClass, m: JMethodID, a: JInt) -> JObject {
        let f = self.slot::<unsafe extern "C" fn(JNIEnv, JClass, JMethodID, ...) -> JObject>(28);
        // SAFETY: `NewObject`'s `jni.h` signature; one `jint` vararg.
        unsafe { f(self.0, c, m, a) }
    }
    #[cfg(target_arch = "x86_64")]
    fn call_int_method_2(self, o: JObject, m: JMethodID, a: JInt, b: JInt) -> JInt {
        let f = self.slot::<unsafe extern "C" fn(JNIEnv, JObject, JMethodID, ...) -> JInt>(49);
        // SAFETY: `CallIntMethod`'s `jni.h` signature; two `jint` varargs.
        unsafe { f(self.0, o, m, a, b) }
    }
    #[cfg(target_arch = "x86_64")]
    fn call_nonvirtual_int_method_0(self, o: JObject, c: JClass, m: JMethodID) -> JInt {
        let f =
            self.slot::<unsafe extern "C" fn(JNIEnv, JObject, JClass, JMethodID, ...) -> JInt>(79);
        // SAFETY: `CallNonvirtualIntMethod`'s `jni.h` signature; no varargs.
        unsafe { f(self.0, o, c, m) }
    }
    #[cfg(target_arch = "x86_64")]
    fn call_static_int_method_2(self, c: JClass, m: JMethodID, a: JInt, b: JInt) -> JInt {
        let f = self.slot::<unsafe extern "C" fn(JNIEnv, JClass, JMethodID, ...) -> JInt>(129);
        // SAFETY: `CallStaticIntMethod`'s `jni.h` signature; two `jint` varargs.
        unsafe { f(self.0, c, m, a, b) }
    }
}

// ---------------------------------------------------------------------------
// The arms and the state a native reports into
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Arm {
    /// `CRATONVM_JNI_NATIVE_TRANSITIONS=0` (the default).
    Counted,
    /// `CRATONVM_JNI_NATIVE_TRANSITIONS=1 CRATONVM_JNI_IN_NATIVE_LAZY=0`.
    Deposit,
    /// `CRATONVM_JNI_NATIVE_TRANSITIONS=1` (the lazy row on with it).
    Lazy,
}

const ALL_ARMS: [Arm; 3] = [Arm::Counted, Arm::Deposit, Arm::Lazy];

/// This thread's JNI package set to `arm` until the guard drops (an
/// assertion's unwind included).
struct ArmScope;

impl ArmScope {
    fn set(arm: Arm) -> Self {
        let native = arm != Arm::Counted;
        // The package rule as the latch resolves it with only
        // `CRATONVM_JNI_NATIVE_TRANSITIONS` set: indirect locals and the
        // foreign transitions on (their defaults), the package's riders on
        // with it, and the two switches outside the package at their defaults.
        let mut switches = jni_switches_from(Some(native), None, None, None, None);
        switches.bulk_array_copies = bulk_array_copies_from(None);
        switches.in_native_lazy = arm == Arm::Lazy;
        JNI_NATIVE_CALL.with(|t| t.switches_override.set(Some(switches)));
        ArmScope
    }
}

impl Drop for ArmScope {
    fn drop(&mut self) {
        let _ = JNI_NATIVE_CALL.try_with(|t| t.switches_override.set(None));
    }
}

/// The innermost in-native record of this thread: `(in_native, lazy)`, or
/// `None` when the call is counted (no record).
fn native_record_state() -> Option<(bool, bool)> {
    JNI_NATIVE_CALL
        .try_with(|t| {
            t.record
                .try_borrow()
                .ok()
                .and_then(|r| r.as_ref().map(|r| (r.in_native, r.lazy.is_some())))
        })
        .ok()
        .flatten()
}

/// What a native reports into; the test passes its address as the `long`
/// argument (no static: `per_vm_state_statics_ratchet`).
struct HarnessState {
    arm: Arm,
    shared: Arc<SharedVm>,
    failures: RefCell<Vec<String>>,
    facts: RefCell<Vec<String>>,
    /// `cross`: the native tells the collecting thread it is waiting...
    ready: Option<mpsc::Sender<()>>,
    /// ...and hears back once the collection ran (or why it did not).
    done: Option<mpsc::Receiver<Result<(), String>>>,
    /// `cross`: a global ref the test made before the call.
    global: JObject,
}

impl HarnessState {
    fn new(arm: Arm, shared: &Arc<SharedVm>) -> Self {
        HarnessState {
            arm,
            shared: Arc::clone(shared),
            failures: RefCell::new(Vec::new()),
            facts: RefCell::new(Vec::new()),
            ready: None,
            done: None,
            global: 0,
        }
    }

    fn fail(&self, what: String) {
        self.failures.borrow_mut().push(what);
    }

    fn fact(&self, what: String) {
        self.facts.borrow_mut().push(what);
    }

    fn gc_cycles(&self) -> u64 {
        self.shared
            .mem
            .gc_cycle_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Run a native's body against the state behind `state`, turning a panic into
/// a reported failure (an unwind out of an `extern "C"` function aborts).
fn run_native(state: JLong, body: impl FnOnce(&HarnessState)) -> JInt {
    // SAFETY: `state` is the address of the test's `HarnessState`, which
    // outlives the `Vm::invoke` this native runs inside, on this thread.
    let st = unsafe { &*(state as usize as *const HarnessState) };
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(st))) {
        Ok(()) => 0,
        Err(p) => {
            let text = p
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| (*s).to_string()))
                .unwrap_or_default();
            st.fail(format!("the native body panicked: {text}"));
            1
        }
    }
}

/// What a pending exception's `detailMessage` must be.
#[derive(Clone, Copy)]
enum Msg<'a> {
    Any,
    Null,
    Is(&'a str),
}

/// The checks a native runs.
struct Ck<'a> {
    st: &'a HarnessState,
    j: Jni,
}

impl Ck<'_> {
    fn ok(&self, cond: bool, what: &str) {
        if !cond {
            self.st.fail(what.to_string());
        }
    }

    fn eq<T: PartialEq + std::fmt::Debug>(&self, got: T, want: T, what: &str) {
        if got != want {
            self.st
                .fail(format!("{what}: got {got:?}, HotSpot answers {want:?}"));
        }
    }

    /// A non-zero handle / id, or a failure (`what` names it).
    fn some(&self, h: u64, what: &str) -> u64 {
        if h == 0 {
            self.st.fail(format!("{what}: NULL"));
            self.none_pending(what);
        }
        h
    }

    /// The text of a `jstring`, through `GetStringUTFChars`.
    fn text(&self, s: JString) -> Option<String> {
        self.j
            .string_utf_chars(s)
            .map(|(b, _)| String::from_utf8_lossy(&b).into_owned())
    }

    /// The class name of `h`'s object (diagnostics only).
    fn class_name_of(&self, h: JObject) -> String {
        let Some(o) = jobject_to_obj(h) else {
            return "<unresolvable>".to_string();
        };
        let cid = self.st.shared.mem.heap.class_id_of(o);
        self.st
            .shared
            .classes
            .class_manager
            .read()
            .get_class(cid)
            .map(|c| c.name.to_string())
            .unwrap_or_else(|| format!("<class id {}>", cid.as_u32()))
    }

    /// A throwable's `detailMessage`: `Ok(None)` for null. A class library
    /// whose `Throwable` names no `detailMessage` field (the fixture VM's
    /// synthetic layout keeps it in an unnamed slot) is asked `getMessage()`
    /// instead, which answers the same text for every throwable this battery
    /// raises (none overrides it but `NullPointerException`, whose JNI-raised
    /// instances have no extended message). A layout with neither (the opaque
    /// `_fN` bootstrap `Throwable`, whose four synthetic methods include no
    /// `getMessage`) is read the way the VM reads it for its own reports
    /// (`vm_exec::describe_throwable`: `backtrace`, then `detailMessage`).
    fn detail_message(&self, t: JThrowable) -> Result<Option<String>, String> {
        let j = self.j;
        let throwable = j.find_class("java/lang/Throwable");
        let fid = j.get_field_id(throwable, "detailMessage", "Ljava/lang/String;");
        if fid == 0 {
            j.exception_clear();
            let get = j.get_method_id(throwable, "getMessage", "()Ljava/lang/String;");
            if get == 0 {
                j.exception_clear();
                return self.described_message(t);
            }
            let m = j.call_object_method_a(t, get, &[]);
            if j.exception_check() == JNI_TRUE {
                j.exception_clear();
                return Err("Throwable.getMessage() threw".to_string());
            }
            return Ok(if m == 0 { None } else { self.text(m) });
        }
        let m = j.get_object_field(t, fid);
        Ok(if m == 0 { None } else { self.text(m) })
    }

    /// The message part of `vm_exec::describe_throwable` ("<class>: <msg>",
    /// or "<class>" alone for a null message). That reads the opaque layout's
    /// slot 1 (the real-JDK order, which the VM's own writers use: `ThrowNew`
    /// and every VM-minted error). The fixture's synthetic `Throwable` has a
    /// second writer: native-builtins' exception constructor shadows (a Java
    /// `new IllegalArgumentException(msg)`) store the message in slot 0, the
    /// synthetic-stub rule its `getMessage` native reads. So a null at slot 1
    /// is read again at slot 0.
    fn described_message(&self, t: JThrowable) -> Result<Option<String>, String> {
        let Some(o) = jobject_to_obj(t) else {
            return Err("an unresolvable throwable handle".to_string());
        };
        let name = self.class_name_of(t);
        let shared = &self.st.shared;
        let text = crate::vm::vm_exec::describe_throwable(shared, o);
        match text.strip_prefix(name.as_str()) {
            Some("") => {
                let slot0 = {
                    let cm = shared.classes.class_manager.read();
                    cm.get_loaded_class_id("java/lang/Throwable")
                        .and_then(|id| cm.get_class(id))
                        .filter(|c| c.fields.iter().all(|f| f.name.starts_with("_f")))
                        .map(|c| c.first_field_index)
                };
                Ok(slot0.and_then(|idx| match shared.mem.heap.get_field(o, idx) {
                    Value::Object(Some(m)) => {
                        crate::vm::read_java_string(&shared.mem.heap, m)
                    }
                    _ => None,
                }))
            }
            Some(rest) => rest
                .strip_prefix(": ")
                .map(|m| Some(m.to_string()))
                .ok_or_else(|| format!("describe_throwable answered {text:?}")),
            None => Err(format!("describe_throwable answered {text:?} for {name}")),
        }
    }

    /// `what` left `class` pending with `msg`; clears it.
    fn pending(&self, what: &str, class: &str, msg: Msg<'_>) {
        let j = self.j;
        if j.exception_check() != JNI_TRUE {
            self.st.fail(format!(
                "{what}: nothing pending; HotSpot leaves {class} pending"
            ));
            return;
        }
        let t = j.exception_occurred();
        j.exception_clear();
        if j.exception_check() != JNI_FALSE {
            self.st.fail(format!(
                "{what}: ExceptionCheck still true after ExceptionClear"
            ));
            j.exception_clear();
        }
        if t == 0 {
            self.st.fail(format!(
                "{what}: ExceptionOccurred answered NULL while ExceptionCheck answered true"
            ));
            return;
        }
        let expected = j.find_class(class);
        if expected == 0 {
            j.exception_clear();
            self.st
                .fail(format!("{what}: FindClass({class}) failed in the harness"));
            return;
        }
        if j.is_instance_of(t, expected) != JNI_TRUE {
            self.st.fail(format!(
                "{what}: pending {} is not a {class}",
                self.class_name_of(t)
            ));
            return;
        }
        let want = match msg {
            Msg::Any => return,
            Msg::Null => None,
            Msg::Is(m) => Some(m.to_string()),
        };
        match self.detail_message(t) {
            Ok(got) if got == want => {}
            Ok(got) => self.st.fail(format!(
                "{what}: {class} message {got:?}, HotSpot's is {want:?}"
            )),
            Err(e) => self.st.fail(format!("{what}: {e}")),
        }
    }

    /// Nothing is pending after `what`; reports and clears anything that is.
    fn none_pending(&self, what: &str) {
        let j = self.j;
        if j.exception_check() == JNI_FALSE {
            return;
        }
        let t = j.exception_occurred();
        j.exception_clear();
        let name = if t == 0 {
            "<no throwable>".to_string()
        } else {
            self.class_name_of(t)
        };
        let msg = if t == 0 {
            Ok(None)
        } else {
            self.detail_message(t)
        };
        self.st
            .fail(format!("{what}: unexpected pending {name} ({msg:?})"));
    }

    /// The arm's state at the native's entry (the positive control).
    fn arm_engaged(&self, at: &str) {
        let got = native_record_state();
        let want = match self.st.arm {
            Arm::Counted => None,
            Arm::Deposit => Some((true, false)),
            Arm::Lazy => Some((true, true)),
        };
        if got != want {
            self.st.fail(format!(
                "{at}: the call's in-native record is {got:?} ((in_native, lazy)), \
                 the {:?} arm needs {want:?}",
                self.st.arm
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// The natives
// ---------------------------------------------------------------------------

extern "C" fn harness_native(env: JNIEnv, _class: JClass, state: JLong) -> JInt {
    run_native(state, |st| {
        let ck = Ck { st, j: Jni(env) };
        ck.arm_engaged("harness entry");
        battery(&ck);
    })
}

extern "C" fn stress_native(env: JNIEnv, _class: JClass, state: JLong) -> JInt {
    run_native(state, |st| {
        let ck = Ck { st, j: Jni(env) };
        ck.arm_engaged("stress entry");
        stress_body(&ck);
    })
}

extern "C" fn cross_native(env: JNIEnv, _class: JClass, state: JLong) -> JInt {
    run_native(state, |st| {
        let ck = Ck { st, j: Jni(env) };
        ck.arm_engaged("cross entry");
        cross_body(&ck);
    })
}

/// `Fixture.throwFromNative()`: leaves an `IllegalArgumentException` pending
/// and returns; the dispatch must throw it into the Java caller.
extern "C" fn throw_from_native(env: JNIEnv, _class: JClass) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let j = Jni(env);
        let iae = j.find_class("java/lang/IllegalArgumentException");
        if iae != 0 {
            let _ = j.throw_new(iae, "jni-harness from native");
        }
    }));
}

// ---------------------------------------------------------------------------
// The battery
// ---------------------------------------------------------------------------

/// Class lookup, method ids and calls, object construction (the K1 / N1 rows
/// of `r15w6-jni6-review-findings`), fields, exceptions, references, strings,
/// arrays and monitors.
fn battery(ck: &Ck<'_>) {
    let j = ck.j;
    ck.eq(j.get_version(), JNI_VERSION_24, "GetVersion");
    ck.none_pending("battery entry");
    ck.eq(
        j.exception_occurred(),
        0,
        "ExceptionOccurred with nothing pending",
    );

    // -- Classes ------------------------------------------------------------
    let fixture = ck.some(j.find_class(FIXTURE), "FindClass(Fixture)");
    let base = ck.some(j.find_class(BASE), "FindClass(Base)");
    if fixture == 0 || base == 0 {
        return;
    }
    ck.eq(
        j.is_assignable_from(fixture, base),
        JNI_TRUE,
        "IsAssignableFrom(Fixture, Base)",
    );
    ck.eq(
        j.is_assignable_from(base, fixture),
        JNI_FALSE,
        "IsAssignableFrom(Base, Fixture)",
    );
    let sup = j.get_superclass(fixture);
    ck.eq(
        j.is_same_object(sup, base),
        JNI_TRUE,
        "GetSuperclass(Fixture) is Base",
    );
    ck.eq(
        j.get_object_ref_type(fixture),
        1,
        "GetObjectRefType(FindClass result) is local",
    );
    ck.eq(
        j.find_class("jniharness/Missing"),
        0,
        "FindClass of a missing class",
    );
    ck.pending(
        "FindClass(jniharness/Missing)",
        "java/lang/NoClassDefFoundError",
        Msg::Is("jniharness/Missing"),
    );

    // j27 wave 1 (lane JNINAT): RegisterNatives checks each entry as HotSpot's
    // `Method::register_native` does (`CRATONVM_JNI_REGISTERNATIVES_EXACT`).
    let register_one = |name: &str, sig: &str| {
        let (n, s) = (cstr(name), cstr(sig));
        let entry = [NativeMethodEntry {
            name: n.as_ptr(),
            signature: s.as_ptr(),
            fn_ptr: throw_from_native as *const () as *mut c_void,
        }];
        j.register_natives(fixture, &entry)
    };
    for (name, sig, what, msg) in [
        (
            "add",
            "(II)I",
            "RegisterNatives(declared, not native)",
            "Method 'int jniharness.Fixture.add(int, int)' is not declared as native",
        ),
        (
            "add",
            "(I)V",
            "RegisterNatives(known name, wrong signature)",
            "Method 'void jniharness.Fixture.add(int)' name or signature does not match",
        ),
        (
            "jniHarnessNoSuchNative",
            "()V",
            "RegisterNatives(a name no class declares)",
            "Method jniharness.Fixture.jniHarnessNoSuchNative()V not found",
        ),
    ] {
        ck.eq(register_one(name, sig), JNI_ERR, what);
        ck.pending(what, "java/lang/NoSuchMethodError", Msg::Is(msg));
    }
    ck.eq(
        register_one("throwFromNative", "()V"),
        JNI_OK,
        "RegisterNatives(declared native) again",
    );
    ck.none_pending("RegisterNatives(declared native) again");
    // j27 wave 8 (lane W8JNI): a NULL `fnPtr` for a declared native is
    // HotSpot's `clear_native_function` (JNI_OK, nothing pending; the binding
    // goes away), then the real binding is put back for the rows below.
    {
        let (n, s) = (cstr("throwFromNative"), cstr("()V"));
        let cleared = [NativeMethodEntry {
            name: n.as_ptr(),
            signature: s.as_ptr(),
            fn_ptr: std::ptr::null_mut(),
        }];
        ck.eq(
            j.register_natives(fixture, &cleared),
            JNI_OK,
            "RegisterNatives(declared native, NULL fnPtr)",
        );
        ck.none_pending("RegisterNatives(declared native, NULL fnPtr)");
    }
    ck.eq(
        register_one("throwFromNative", "()V"),
        JNI_OK,
        "RegisterNatives(declared native) after a NULL fnPtr",
    );

    // -- Objects, method ids, calls -----------------------------------------
    let ctor = ck.some(
        j.get_method_id(fixture, "<init>", "(I)V"),
        "GetMethodID(Fixture.<init>(I)V)",
    );
    let obj = ck.some(
        j.new_object_a(fixture, ctor, &[JValue { i: 7 }]),
        "NewObjectA(Fixture(7))",
    );
    ck.none_pending("NewObjectA");
    ck.eq(
        j.is_instance_of(obj, fixture),
        JNI_TRUE,
        "IsInstanceOf(obj, Fixture)",
    );
    ck.eq(
        j.is_instance_of(obj, base),
        JNI_TRUE,
        "IsInstanceOf(obj, Base)",
    );
    ck.eq(
        j.is_instance_of(0, fixture),
        JNI_TRUE,
        "IsInstanceOf(NULL, Fixture)",
    );
    ck.eq(
        j.is_same_object(j.get_object_class(obj), fixture),
        JNI_TRUE,
        "GetObjectClass(obj) is Fixture",
    );
    let count = ck.some(j.get_field_id(fixture, "count", "I"), "GetFieldID(count)");
    let base_value = ck.some(
        j.get_field_id(fixture, "baseValue", "I"),
        "GetFieldID(Fixture, inherited baseValue)",
    );
    ck.eq(j.get_int_field(obj, count), 7, "Fixture(7).count");
    ck.eq(j.get_int_field(obj, base_value), 7, "Fixture(7).baseValue");

    // K1: a superclass constructor id given to NewObject runs THAT
    // constructor on an instance of the named class.
    let base_ctor = ck.some(
        j.get_method_id(base, "<init>", "(I)V"),
        "GetMethodID(Base.<init>(I)V)",
    );
    let sub = ck.some(
        j.new_object_a(fixture, base_ctor, &[JValue { i: 9 }]),
        "NewObjectA(Fixture, Base.<init>(I)V)",
    );
    ck.none_pending("NewObjectA(Fixture, Base ctor)");
    ck.eq(
        j.is_instance_of(sub, fixture),
        JNI_TRUE,
        "K1: the result is a Fixture",
    );
    ck.eq(j.get_int_field(sub, base_value), 9, "K1: Base(int) ran");
    ck.eq(
        j.get_int_field(sub, count),
        0,
        "K1: Fixture(int) did not run",
    );

    let alloc = ck.some(j.alloc_object(fixture), "AllocObject(Fixture)");
    ck.eq(
        j.is_instance_of(alloc, fixture),
        JNI_TRUE,
        "AllocObject is a Fixture",
    );
    ck.eq(
        j.get_int_field(alloc, count),
        0,
        "AllocObject runs no constructor",
    );

    // Constructors are never inherited, a static/instance mismatch is a miss,
    // and a miss leaves NoSuchMethodError("[static ]<sig name>.<name><sig>").
    ck.eq(
        j.get_method_id(fixture, "<init>", "()V"),
        0,
        "GetMethodID(Fixture.<init>()V)",
    );
    ck.pending(
        "GetMethodID(Fixture, <init>, ()V)",
        "java/lang/NoSuchMethodError",
        Msg::Is("Ljniharness/Fixture;.<init>()V"),
    );
    ck.eq(
        j.get_method_id(fixture, "nope", "()V"),
        0,
        "GetMethodID(Fixture.nope)",
    );
    ck.pending(
        "GetMethodID(Fixture, nope, ()V)",
        "java/lang/NoSuchMethodError",
        Msg::Is("Ljniharness/Fixture;.nope()V"),
    );
    ck.eq(
        j.get_static_method_id(fixture, "add", "(II)I"),
        0,
        "GetStaticMethodID(instance add)",
    );
    ck.pending(
        "GetStaticMethodID(Fixture, add, (II)I)",
        "java/lang/NoSuchMethodError",
        Msg::Is("static Ljniharness/Fixture;.add(II)I"),
    );

    // N1: a virtual call through the superclass's id dispatches on the
    // receiver; a nonvirtual call runs exactly the named class's method.
    let describe_base = ck.some(
        j.get_method_id(base, "describe", "()I"),
        "GetMethodID(Base.describe)",
    );
    let describe_fix = ck.some(
        j.get_method_id(fixture, "describe", "()I"),
        "GetMethodID(Fixture.describe)",
    );
    ck.eq(
        j.call_int_method_a(obj, describe_base, &[]),
        2,
        "CallIntMethodA(Base.describe) is virtual",
    );
    ck.eq(
        j.call_nonvirtual_int_method_a(obj, base, describe_base, &[]),
        1,
        "CallNonvirtualIntMethodA(Base.describe)",
    );
    ck.eq(
        j.call_nonvirtual_int_method_a(obj, fixture, describe_fix, &[]),
        2,
        "CallNonvirtualIntMethodA(Fixture.describe)",
    );
    let add = ck.some(j.get_method_id(fixture, "add", "(II)I"), "GetMethodID(add)");
    ck.eq(
        j.call_int_method_a(obj, add, &[JValue { i: 40 }, JValue { i: 2 }]),
        42,
        "CallIntMethodA(add, 40, 2)",
    );
    let sum = ck.some(
        j.get_static_method_id(fixture, "sum", "(II)I"),
        "GetStaticMethodID(sum)",
    );
    ck.eq(
        j.call_static_int_method_a(fixture, sum, &[JValue { i: 3 }, JValue { i: 4 }]),
        7,
        "CallStaticIntMethodA(sum, 3, 4)",
    );
    ck.none_pending("the A-form calls");

    #[cfg(target_arch = "x86_64")]
    {
        ck.eq(
            j.call_int_method_2(obj, add, 40, 2),
            42,
            "CallIntMethod(add, 40, 2)",
        );
        ck.eq(
            j.call_static_int_method_2(fixture, sum, 3, 4),
            7,
            "CallStaticIntMethod(sum, 3, 4)",
        );
        ck.eq(
            j.call_nonvirtual_int_method_0(obj, base, describe_base),
            1,
            "CallNonvirtualIntMethod(Base.describe)",
        );
        let five = j.new_object_1(fixture, ctor, 5);
        ck.eq(
            j.get_int_field(five, count),
            5,
            "NewObject(Fixture, 5).count",
        );
        ck.none_pending("the varargs calls");
    }

    // -- Fields ---------------------------------------------------------------
    j.set_int_field(obj, count, 99);
    ck.eq(j.get_int_field(obj, count), 99, "SetIntField / GetIntField");
    let wide = ck.some(j.get_field_id(fixture, "wide", "J"), "GetFieldID(wide)");
    j.set_long_field(obj, wide, 0x1234_5678_9ABC_DEF0);
    ck.eq(
        j.get_long_field(obj, wide),
        0x1234_5678_9ABC_DEF0,
        "SetLongField / GetLongField",
    );
    let reff = ck.some(
        j.get_field_id(fixture, "ref", "Ljava/lang/Object;"),
        "GetFieldID(ref)",
    );
    let held = j.new_string_utf("jni-harness field");
    j.set_object_field(obj, reff, held);
    ck.eq(
        j.is_same_object(j.get_object_field(obj, reff), held),
        JNI_TRUE,
        "SetObjectField / GetObjectField",
    );
    j.set_object_field(obj, reff, 0);
    ck.eq(j.get_object_field(obj, reff), 0, "SetObjectField(NULL)");
    let hits = ck.some(
        j.get_static_field_id(fixture, "hits", "I"),
        "GetStaticFieldID(hits)",
    );
    j.set_static_int_field(fixture, hits, 31);
    ck.eq(
        j.get_static_int_field(fixture, hits),
        31,
        "SetStaticIntField / GetStaticIntField",
    );
    ck.none_pending("the field accessors");
    ck.eq(j.get_field_id(fixture, "nope", "I"), 0, "GetFieldID(nope)");
    ck.pending(
        "GetFieldID(Fixture, nope, I)",
        "java/lang/NoSuchFieldError",
        // HotSpot 25's `GetFieldID` names the class, the field and its
        // signature (`GetStaticFieldID` the field alone): gcf f3/j, read off
        // its own JNI table (`vm/src/native/jni.rs`, `jni_field_id`).
        Msg::Is("jniharness.Fixture.nope I"),
    );
    ck.eq(
        j.get_static_field_id(fixture, "count", "I"),
        0,
        "GetStaticFieldID(instance count)",
    );
    ck.pending(
        "GetStaticFieldID(Fixture, count, I)",
        "java/lang/NoSuchFieldError",
        Msg::Is("count"),
    );

    // -- Exceptions -------------------------------------------------------------
    let iae = ck.some(
        j.find_class("java/lang/IllegalArgumentException"),
        "FindClass(IllegalArgumentException)",
    );
    ck.eq(j.throw_new(iae, "jni-harness thrown"), JNI_OK, "ThrowNew");
    ck.eq(
        j.exception_check(),
        JNI_TRUE,
        "ExceptionCheck after ThrowNew",
    );
    let thrown = j.exception_occurred();
    ck.ok(thrown != 0, "ExceptionOccurred after ThrowNew is not NULL");
    j.exception_clear();
    ck.eq(
        j.exception_check(),
        JNI_FALSE,
        "ExceptionCheck after ExceptionClear",
    );
    ck.eq(
        j.exception_occurred(),
        0,
        "ExceptionOccurred after ExceptionClear",
    );
    if thrown != 0 {
        ck.eq(j.is_instance_of(thrown, iae), JNI_TRUE, "ThrowNew's class");
        ck.eq(
            ck.detail_message(thrown),
            Ok(Some("jni-harness thrown".to_string())),
            "ThrowNew's message",
        );
        ck.eq(j.throw(thrown), JNI_OK, "Throw");
        let again = j.exception_occurred();
        ck.eq(
            j.is_same_object(again, thrown),
            JNI_TRUE,
            "Throw re-pends the same throwable",
        );
        j.exception_clear();
    }
    let thrower = ck.some(
        j.get_static_method_id(fixture, "thrower", "()V"),
        "GetStaticMethodID(thrower)",
    );
    j.call_static_void_method_a(fixture, thrower, &[]);
    ck.pending(
        "CallStaticVoidMethodA(thrower)",
        "java/lang/IllegalArgumentException",
        Msg::Is("jni-harness boom"),
    );
    // A native's pending exception is thrown into its Java caller, and comes
    // back to this native as the up-call's pending exception.
    let from_native = ck.some(
        j.get_static_method_id(fixture, "throwFromNative", "()V"),
        "GetStaticMethodID(throwFromNative)",
    );
    j.call_static_void_method_a(fixture, from_native, &[]);
    ck.pending(
        "CallStaticVoidMethodA(throwFromNative)",
        "java/lang/IllegalArgumentException",
        Msg::Is("jni-harness from native"),
    );

    // -- References -------------------------------------------------------------
    ck.eq(j.ensure_local_capacity(0), JNI_OK, "EnsureLocalCapacity(0)");
    ck.eq(
        j.ensure_local_capacity(4096),
        JNI_OK,
        "EnsureLocalCapacity(4096)",
    );
    ck.eq(
        j.ensure_local_capacity(-1),
        JNI_ERR,
        "EnsureLocalCapacity(-1)",
    );
    // HotSpot's own reading of the pending state for a negative capacity is
    // not pinned here (see the lane page); whatever is pending is cleared.
    j.exception_clear();
    ck.eq(j.push_local_frame(-1), JNI_ERR, "PushLocalFrame(-1)");
    j.exception_clear();
    let s = j.new_string_utf("jni-harness ref");
    ck.eq(j.get_object_ref_type(s), 1, "GetObjectRefType(local)");
    ck.eq(j.is_same_object(0, 0), JNI_TRUE, "IsSameObject(NULL, NULL)");
    ck.eq(j.is_same_object(s, 0), JNI_FALSE, "IsSameObject(s, NULL)");
    let twin = j.new_string_utf("jni-harness ref");
    ck.eq(
        j.is_same_object(s, twin),
        JNI_FALSE,
        "NewStringUTF answers a new String each time",
    );
    let g = j.new_global_ref(s);
    ck.eq(j.get_object_ref_type(g), 2, "GetObjectRefType(global)");
    ck.eq(
        j.is_same_object(g, s),
        JNI_TRUE,
        "IsSameObject(global, local)",
    );
    j.delete_global_ref(g);
    let w = j.new_weak_global_ref(s);
    ck.eq(j.get_object_ref_type(w), 3, "GetObjectRefType(weak global)");
    ck.eq(
        j.is_same_object(w, s),
        JNI_TRUE,
        "IsSameObject(weak, local)",
    );
    j.delete_weak_global_ref(w);
    let l = j.new_local_ref(s);
    ck.eq(j.get_object_ref_type(l), 1, "GetObjectRefType(NewLocalRef)");
    ck.eq(
        j.is_same_object(l, s),
        JNI_TRUE,
        "IsSameObject(NewLocalRef, local)",
    );
    j.delete_local_ref(l);
    ck.eq(
        ck.text(s).as_deref(),
        Some("jni-harness ref"),
        "a local survives DeleteLocalRef of its copy",
    );

    ck.eq(j.push_local_frame(16), JNI_OK, "PushLocalFrame(16)");
    let inner = j.new_string_utf("jni-harness frame result");
    let promoted = j.pop_local_frame(inner);
    ck.ok(promoted != 0, "PopLocalFrame(result) answers a handle");
    ck.eq(
        j.get_object_ref_type(promoted),
        1,
        "PopLocalFrame's result is a local",
    );
    ck.eq(
        ck.text(promoted).as_deref(),
        Some("jni-harness frame result"),
        "PopLocalFrame's result",
    );
    // Local capacity is a hint: 4096 locals in a frame that asked for 16.
    ck.eq(
        j.push_local_frame(16),
        JNI_OK,
        "PushLocalFrame(16) for 4096 locals",
    );
    let mut minted = Vec::with_capacity(4096);
    for _ in 0..4096 {
        minted.push(j.new_local_ref(s));
    }
    ck.ok(
        minted.iter().all(|&h| h != 0),
        "4096 locals past the frame's capacity are all handed out",
    );
    let same = minted
        .iter()
        .step_by(97)
        .all(|&h| j.is_same_object(h, s) == JNI_TRUE);
    ck.ok(
        same,
        "4096 locals past the frame's capacity all name their object",
    );
    ck.eq(j.pop_local_frame(0), 0, "PopLocalFrame(NULL)");
    ck.none_pending("the reference rows");

    // -- Strings ----------------------------------------------------------------
    let plain = j.new_string_utf("jni-harness");
    ck.eq(j.get_string_length(plain), 11, "GetStringLength");
    ck.eq(j.get_string_utf_length(plain), 11, "GetStringUTFLength");
    ck.eq(
        j.string_utf_chars(plain),
        Some((b"jni-harness".to_vec(), JNI_TRUE)),
        "GetStringUTFChars (bytes, isCopy)",
    );
    let nul = j.new_string(&[0x41, 0, 0x42]);
    ck.eq(j.get_string_length(nul), 3, "GetStringLength(A NUL B)");
    ck.eq(
        j.get_string_utf_length(nul),
        4,
        "GetStringUTFLength(A NUL B): NUL is C0 80",
    );
    ck.eq(
        j.string_utf_chars(nul).map(|(b, _)| b),
        Some(vec![0x41, 0xC0, 0x80, 0x42]),
        "GetStringUTFChars(A NUL B)",
    );
    let emoji = j.new_string(&[0xD83D, 0xDE00]);
    ck.eq(
        j.get_string_utf_length(emoji),
        6,
        "GetStringUTFLength(U+1F600): a CESU-8 pair",
    );
    ck.eq(
        j.string_utf_chars(emoji).map(|(b, _)| b),
        Some(vec![0xED, 0xA0, 0xBD, 0xED, 0xB8, 0x80]),
        "GetStringUTFChars(U+1F600)",
    );
    let decoded = j.new_string_utf_bytes(&[0xED, 0xA0, 0xBD, 0xED, 0xB8, 0x80]);
    ck.eq(
        j.get_string_length(decoded),
        2,
        "NewStringUTF(CESU-8 pair) length",
    );
    let mut units = [0 as JChar; 2];
    j.get_string_region(decoded, 0, 2, &mut units);
    ck.eq(
        units,
        [0xD83D, 0xDE00],
        "GetStringRegion of NewStringUTF(CESU-8 pair)",
    );
    let mut spare = [0 as JChar; 8];
    j.get_string_region(decoded, 1, 5, &mut spare);
    ck.pending(
        "GetStringRegion(1, 5) of a 2-char string",
        "java/lang/StringIndexOutOfBoundsException",
        Msg::Null,
    );
    let mut is_copy: JBoolean = 7;
    let chars = j.get_string_critical(plain, &mut is_copy);
    if chars.is_null() {
        ck.st.fail("GetStringCritical answered NULL".to_string());
    } else {
        // SAFETY: a critical string region of `GetStringLength` (11) chars,
        // valid until the release.
        let got = unsafe { std::slice::from_raw_parts(chars, 11) }.to_vec();
        j.release_string_critical(plain, chars);
        let want: Vec<JChar> = "jni-harness".encode_utf16().collect();
        ck.eq(got, want, "GetStringCritical");
    }
    ck.none_pending("the string rows");

    // -- Arrays -----------------------------------------------------------------
    let ints = ck.some(j.new_int_array(4), "NewIntArray(4)");
    ck.eq(j.get_array_length(ints), 4, "GetArrayLength");
    j.set_int_array_region(ints, 0, &[1, 2, 3, 4]);
    let read = |j: Jni| {
        let mut out = [0 as JInt; 4];
        j.get_int_array_region(ints, 0, &mut out);
        out
    };
    ck.eq(read(j), [1, 2, 3, 4], "Set/GetIntArrayRegion");
    let mut copy_flag: JBoolean = 7;
    let elems = j.get_int_array_elements(ints, &mut copy_flag);
    if elems.is_null() {
        ck.st.fail("GetIntArrayElements answered NULL".to_string());
    } else {
        // HotSpot always hands out a copy of a non-empty array.
        ck.eq(copy_flag, JNI_TRUE, "GetIntArrayElements isCopy");
        // SAFETY: four elements, valid until the final release below.
        unsafe { *elems = 10 };
        j.release_int_array_elements(ints, elems, JNI_ABORT);
        ck.eq(
            read(j),
            [1, 2, 3, 4],
            "ReleaseIntArrayElements(JNI_ABORT) writes nothing back",
        );
        let elems = j.get_int_array_elements(ints, std::ptr::null_mut());
        if !elems.is_null() {
            // SAFETY: as above.
            unsafe { *elems.add(1) = 20 };
            j.release_int_array_elements(ints, elems, JNI_COMMIT);
            ck.eq(
                read(j),
                [1, 20, 3, 4],
                "ReleaseIntArrayElements(JNI_COMMIT) writes back",
            );
            // SAFETY: JNI_COMMIT does not free the buffer.
            unsafe { *elems.add(2) = 30 };
            j.release_int_array_elements(ints, elems, 0);
            ck.eq(
                read(j),
                [1, 20, 30, 4],
                "ReleaseIntArrayElements(0) writes back",
            );
        } else {
            ck.st
                .fail("GetIntArrayElements (second) answered NULL".to_string());
        }
    }
    // j27 wave 1 (lane JNINAT): an EMPTY array's elements are HotSpot's dummy
    // address, not a copy (`DEFINE_GETSCALARARRAYELEMENTS`), and every release
    // mode ignores it.
    let empty = ck.some(j.new_int_array(0), "NewIntArray(0)");
    let mut empty_copy: JBoolean = 7;
    let empty_elems = j.get_int_array_elements(empty, &mut empty_copy);
    ck.ok(
        !empty_elems.is_null(),
        "GetIntArrayElements(empty) is not NULL",
    );
    ck.eq(empty_copy, JNI_FALSE, "GetIntArrayElements(empty) isCopy");
    ck.none_pending("GetIntArrayElements(empty)");
    let again = j.get_int_array_elements(empty, std::ptr::null_mut());
    ck.eq(again, empty_elems, "GetIntArrayElements(empty) is one address");
    for mode in [JNI_COMMIT, JNI_ABORT, 0] {
        j.release_int_array_elements(empty, empty_elems, mode);
    }
    ck.none_pending("ReleaseIntArrayElements(empty)");
    // A release mode outside 0 / JNI_COMMIT / JNI_ABORT neither copies back
    // nor frees (`DEFINE_RELEASESCALARARRAYELEMENTS`); the buffer stays
    // outstanding and a later JNI_ABORT frees it.
    let before_odd = read(j);
    let odd = j.get_int_array_elements(ints, std::ptr::null_mut());
    if odd.is_null() {
        ck.st
            .fail("GetIntArrayElements (mode 5 row) answered NULL".to_string());
    } else {
        // SAFETY: four elements, valid until the JNI_ABORT release below.
        unsafe { *odd = 99 };
        j.release_int_array_elements(ints, odd, 5);
        ck.eq(
            read(j),
            before_odd,
            "ReleaseIntArrayElements(mode 5) writes nothing back",
        );
        // SAFETY: mode 5 freed nothing, so the buffer is still ours.
        ck.eq(
            unsafe { *odd },
            99,
            "ReleaseIntArrayElements(mode 5) frees nothing",
        );
        j.release_int_array_elements(ints, odd, JNI_ABORT);
    }
    ck.none_pending("ReleaseIntArrayElements(mode 5)");
    let crit = j.get_primitive_array_critical(ints, std::ptr::null_mut()) as *mut JInt;
    if crit.is_null() {
        ck.st
            .fail("GetPrimitiveArrayCritical answered NULL".to_string());
    } else {
        // SAFETY: a critical region of the four-element array.
        let last = unsafe { *crit.add(3) };
        ck.eq(last, 4, "GetPrimitiveArrayCritical reads the array");
        // SAFETY: as above.
        unsafe { *crit.add(3) = 40 };
        j.release_primitive_array_critical(ints, crit as *mut c_void, 0);
        ck.eq(
            read(j),
            [1, 20, 30, 40],
            "ReleasePrimitiveArrayCritical(0) writes back",
        );
    }
    ck.none_pending("the int[] rows");
    let mut big = [0 as JInt; 8];
    j.get_int_array_region_len(ints, 2, 5, &mut big);
    ck.pending(
        "GetIntArrayRegion(2, 5) of an int[4]",
        "java/lang/ArrayIndexOutOfBoundsException",
        Msg::Is("Array region 2..7 out of bounds for length 4"),
    );
    j.get_int_array_region_len(ints, 0, -1, &mut big);
    ck.pending(
        "GetIntArrayRegion(0, -1)",
        "java/lang/ArrayIndexOutOfBoundsException",
        Msg::Is("Length -1 is negative"),
    );
    ck.eq(j.new_int_array(-1), 0, "NewIntArray(-1)");
    ck.pending(
        "NewIntArray(-1)",
        "java/lang/NegativeArraySizeException",
        Msg::Is("-1"),
    );
    let objs = ck.some(
        j.new_object_array(3, fixture, obj),
        "NewObjectArray(3, Fixture, obj)",
    );
    ck.eq(j.get_array_length(objs), 3, "GetArrayLength(Fixture[3])");
    let filled =
        (0..3).all(|i| j.is_same_object(j.get_object_array_element(objs, i), obj) == JNI_TRUE);
    ck.ok(filled, "NewObjectArray fills every element with init");
    j.set_object_array_element(objs, 1, 0);
    ck.eq(
        j.get_object_array_element(objs, 1),
        0,
        "SetObjectArrayElement(NULL)",
    );
    ck.none_pending("the Fixture[] rows");
    ck.eq(
        j.get_object_array_element(objs, 3),
        0,
        "GetObjectArrayElement(3) of a Fixture[3]",
    );
    ck.pending(
        "GetObjectArrayElement(3)",
        "java/lang/ArrayIndexOutOfBoundsException",
        Msg::Is("Index 3 out of bounds for length 3"),
    );
    j.set_object_array_element(objs, 0, plain);
    ck.pending(
        "SetObjectArrayElement(Fixture[], String)",
        "java/lang/ArrayStoreException",
        Msg::Is("type mismatch: can not store java.lang.String to jniharness.Fixture[0]"),
    );
    ck.eq(
        j.is_same_object(j.get_object_array_element(objs, 0), obj),
        JNI_TRUE,
        "a refused store stores nothing",
    );

    // -- Monitors ---------------------------------------------------------------
    ck.eq(j.monitor_enter(obj), JNI_OK, "MonitorEnter");
    ck.eq(j.monitor_enter(obj), JNI_OK, "MonitorEnter (re-entry)");
    ck.eq(j.monitor_exit(obj), JNI_OK, "MonitorExit");
    ck.eq(j.monitor_exit(obj), JNI_OK, "MonitorExit (outermost)");
    ck.none_pending("balanced MonitorEnter / MonitorExit");
    ck.eq(
        j.monitor_exit(obj),
        JNI_ERR,
        "MonitorExit of a monitor this thread does not own",
    );
    ck.pending(
        "MonitorExit (not owner)",
        "java/lang/IllegalMonitorStateException",
        Msg::Is("current thread is not owner"),
    );
    ck.eq(j.monitor_enter(0), JNI_ERR, "MonitorEnter(NULL)");
    ck.pending(
        "MonitorEnter(NULL)",
        "java/lang/NullPointerException",
        Msg::Null,
    );
    ck.eq(j.monitor_exit(0), JNI_ERR, "MonitorExit(NULL)");
    ck.pending(
        "MonitorExit(NULL)",
        "java/lang/NullPointerException",
        Msg::Null,
    );

    ck.none_pending("battery end");
}

// ---------------------------------------------------------------------------
// The GC rows
// ---------------------------------------------------------------------------

/// The address `h` names now (diagnostics: how many referents moved).
fn handle_address(h: JObject) -> Option<usize> {
    jobject_to_obj(h).map(|o| o.as_ptr() as usize)
}

/// Locals of every kind a native commonly holds, and what each must read.
struct HeldLocals {
    texts: Vec<String>,
    strings: Vec<JString>,
    ints: JArray,
    obj: JObject,
    holder: JArray,
    count: JFieldID,
}

impl HeldLocals {
    fn mint(ck: &Ck<'_>, fixture: JClass) -> Option<Self> {
        let j = ck.j;
        let ctor = ck.some(
            j.get_method_id(fixture, "<init>", "(I)V"),
            "stress: GetMethodID(<init>)",
        );
        let count = ck.some(
            j.get_field_id(fixture, "count", "I"),
            "stress: GetFieldID(count)",
        );
        let string_class = ck.some(
            j.find_class("java/lang/String"),
            "stress: FindClass(String)",
        );
        if ctor == 0 || count == 0 || string_class == 0 {
            return None;
        }
        let texts: Vec<String> = (0..16).map(|i| format!("jni-harness held {i}")).collect();
        let strings: Vec<JString> = texts.iter().map(|t| j.new_string_utf(t)).collect();
        let ints = j.new_int_array(64);
        let values: Vec<JInt> = (0..64).map(|i| i * 3).collect();
        j.set_int_array_region(ints, 0, &values);
        let obj = j.new_object_a(fixture, ctor, &[JValue { i: 1234 }]);
        let holder = j.new_object_array(2, string_class, strings[0]);
        j.set_object_array_element(holder, 1, strings[1]);
        ck.none_pending("stress: minting the locals");
        Some(HeldLocals {
            texts,
            strings,
            ints,
            obj,
            holder,
            count,
        })
    }

    fn handles(&self) -> Vec<JObject> {
        let mut v = self.strings.clone();
        v.extend_from_slice(&[self.ints, self.obj, self.holder]);
        v
    }

    /// Every local still names its object.
    fn verify(&self, ck: &Ck<'_>, when: &str) {
        let j = ck.j;
        for (s, t) in self.strings.iter().zip(&self.texts) {
            ck.eq(
                ck.text(*s).as_deref(),
                Some(t.as_str()),
                &format!("{when}: a held String"),
            );
            ck.eq(
                j.get_object_ref_type(*s),
                1,
                &format!("{when}: a held String is still a local"),
            );
        }
        let mut out = [0 as JInt; 64];
        j.get_int_array_region(self.ints, 0, &mut out);
        ck.ok(
            out.iter().enumerate().all(|(i, &v)| v == (i as JInt) * 3),
            &format!("{when}: the held int[] keeps its elements"),
        );
        ck.eq(
            j.get_int_field(self.obj, self.count),
            1234,
            &format!("{when}: the held Fixture's count"),
        );
        ck.eq(
            j.is_same_object(j.get_object_array_element(self.holder, 0), self.strings[0]),
            JNI_TRUE,
            &format!("{when}: String[0] of the held array"),
        );
        ck.eq(
            j.is_same_object(j.get_object_array_element(self.holder, 1), self.strings[1]),
            JNI_TRUE,
            &format!("{when}: String[1] of the held array"),
        );
        ck.none_pending(&format!("{when}: reading the held locals"));
    }
}

/// Locals, an explicit frame's local, a global and a weak global, held across
/// two up-calls that allocate and run `System.gc()`.
fn stress_body(ck: &Ck<'_>) {
    let j = ck.j;
    let fixture = ck.some(j.find_class(FIXTURE), "stress: FindClass(Fixture)");
    let churn = ck.some(
        j.get_static_method_id(fixture, "churn", "(I)V"),
        "stress: GetStaticMethodID(churn)",
    );
    if fixture == 0 || churn == 0 {
        return;
    }
    let Some(held) = HeldLocals::mint(ck, fixture) else {
        return;
    };
    let global = j.new_global_ref(held.strings[3]);
    let weak = j.new_weak_global_ref(held.obj);
    ck.eq(j.push_local_frame(8), JNI_OK, "stress: PushLocalFrame(8)");
    let inner = j.new_string_utf("jni-harness inner frame");
    let mut watched = held.handles();
    watched.push(inner);
    let before: Vec<Option<usize>> = watched.iter().map(|&h| handle_address(h)).collect();
    for round in 0..2 {
        let when = format!("stress round {round}");
        let cycles = ck.st.gc_cycles();
        j.call_static_void_method_a(fixture, churn, &[JValue { i: 1 << 15 }]);
        ck.none_pending(&format!("{when}: churn"));
        // The positive control: a collection did run under the native.
        ck.ok(
            ck.st.gc_cycles() > cycles,
            &format!("{when}: System.gc() inside the up-call collected"),
        );
        held.verify(ck, &when);
        ck.eq(
            ck.text(inner).as_deref(),
            Some("jni-harness inner frame"),
            &format!("{when}: the explicit frame's local"),
        );
        ck.eq(
            ck.text(global).as_deref(),
            Some(held.texts[3].as_str()),
            &format!("{when}: the global ref"),
        );
        ck.eq(
            j.is_same_object(weak, held.obj),
            JNI_TRUE,
            &format!("{when}: the weak global ref of a held object"),
        );
    }
    let moved = watched
        .iter()
        .zip(&before)
        .filter(|(h, b)| handle_address(**h) != **b)
        .count();
    ck.st
        .fact(format!("stress-moved {moved}/{}", watched.len()));
    j.pop_local_frame(0);
    j.delete_global_ref(global);
    j.delete_weak_global_ref(weak);
    ck.none_pending("stress end");
}

/// The text of the global ref `cross` is handed (made before the call, so the
/// lazy arm's native needs no local to hold it).
const CROSS_TEXT: &str = "jni-harness cross global";

/// Wait in native while another thread collects, then read everything back.
/// The deposit arm holds locals of every kind; the lazy arm calls no JNIEnv
/// function before the wait (any local would end the lazy state for the rest
/// of the call), so it holds only the global ref the test made.
fn cross_body(ck: &Ck<'_>) {
    let j = ck.j;
    let (Some(ready), Some(done)) = (ck.st.ready.as_ref(), ck.st.done.as_ref()) else {
        ck.st.fail("cross: the test passed no channels".to_string());
        return;
    };
    let held = match ck.st.arm {
        Arm::Deposit => {
            let fixture = ck.some(j.find_class(FIXTURE), "cross: FindClass(Fixture)");
            if fixture == 0 {
                None
            } else {
                HeldLocals::mint(ck, fixture)
            }
        }
        Arm::Lazy => None,
        Arm::Counted => {
            ck.st
                .fail("cross: the counted arm would hold the pause".to_string());
            let _ = ready.send(());
            return;
        }
    };
    let want = if ck.st.arm == Arm::Lazy {
        Some((true, true))
    } else {
        Some((true, false))
    };
    ck.eq(
        native_record_state(),
        want,
        "cross: the call's (in_native, lazy) state while it waits",
    );
    let global = ck.st.global;
    let global_before = handle_address(global);
    let before: Vec<Option<usize>> = held
        .as_ref()
        .map(|h| h.handles().iter().map(|&x| handle_address(x)).collect())
        .unwrap_or_default();
    if ready.send(()).is_err() {
        ck.st
            .fail("cross: the collecting thread is gone".to_string());
        return;
    }
    match done.recv_timeout(WAIT) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => ck.st.fail(format!("cross: the collecting thread: {e}")),
        Err(_) => ck
            .st
            .fail("cross: no collection finished while the native was in native".to_string()),
    }
    ck.eq(
        ck.text(global).as_deref(),
        Some(CROSS_TEXT),
        "cross: the global ref held through the collection",
    );
    ck.st.fact(format!(
        "cross-global-moved {}",
        handle_address(global) != global_before
    ));
    if let Some(held) = &held {
        held.verify(ck, "cross");
        let moved = held
            .handles()
            .iter()
            .zip(&before)
            .filter(|(h, b)| handle_address(**h) != **b)
            .count();
        ck.st.fact(format!("cross-moved {moved}/{}", before.len()));
    }
    j.delete_global_ref(global);
    ck.none_pending("cross end");
}

// ---------------------------------------------------------------------------
// Booting, binding, invoking
// ---------------------------------------------------------------------------

/// A VM with the fixture classes on its class path and the natives bound.
/// `vm` drops before `_dir`.
struct Harness {
    vm: Vm,
    _dir: tempfile::TempDir,
}

/// The JNI thread-locals of a setup call on the test thread, cleared on drop.
struct SetupContext {
    frames_base: usize,
}

impl SetupContext {
    fn install(vm: &mut Vm) -> Self {
        let frames_base = local_frame_depth();
        set_jni_context_arc(Arc::clone(&vm.shared));
        set_jni_thread(vm.main_thread.as_mut() as *mut JvmThread);
        // A frame of its own, as a native dispatch opens one, so a local the
        // setup mints is an indirect handle (none escapes raw).
        push_local_frame(16);
        SetupContext { frames_base }
    }
}

impl Drop for SetupContext {
    fn drop(&mut self) {
        truncate_local_frames(self.frames_base);
        let _ = take_jni_pending_exception();
        clear_jni_thread();
        clear_jni_context();
    }
}

fn boot(gc_algorithm: GcAlgorithm) -> Harness {
    let dir = tempfile::tempdir().expect("a temp dir for the fixture classes");
    write_fixture_classes(dir.path());
    let config = VmConfig {
        gc_algorithm,
        ..VmConfig::default()
    }
    .with_classpath(vec![dir.path().to_string_lossy().into_owned()]);
    let mut vm = Vm::new(config);
    {
        let _ctx = SetupContext::install(&mut vm);
        let j = Jni(get_jni_env());
        let fixture = j.find_class(FIXTURE);
        assert_ne!(fixture, 0, "FindClass({FIXTURE}) from the class path");
        assert_eq!(
            j.exception_check(),
            JNI_FALSE,
            "FindClass({FIXTURE}) left nothing pending"
        );
        let names: Vec<(CString, CString, *mut c_void)> = vec![
            (
                cstr("harness"),
                cstr("(J)I"),
                harness_native as *const () as *mut c_void,
            ),
            (
                cstr("stress"),
                cstr("(J)I"),
                stress_native as *const () as *mut c_void,
            ),
            (
                cstr("cross"),
                cstr("(J)I"),
                cross_native as *const () as *mut c_void,
            ),
            (
                cstr("throwFromNative"),
                cstr("()V"),
                throw_from_native as *const () as *mut c_void,
            ),
        ];
        let methods: Vec<NativeMethodEntry> = names
            .iter()
            .map(|(n, s, f)| NativeMethodEntry {
                name: n.as_ptr(),
                signature: s.as_ptr(),
                fn_ptr: *f,
            })
            .collect();
        assert_eq!(
            j.register_natives(fixture, &methods),
            JNI_OK,
            "RegisterNatives"
        );
        assert_eq!(
            j.exception_check(),
            JNI_FALSE,
            "RegisterNatives left nothing pending"
        );
    }
    Harness { vm, _dir: dir }
}

/// `Fixture.<runner>(state)` under `st.arm`; the native's `int` answer.
fn invoke(h: &mut Harness, runner: &str, st: &HarnessState) -> Result<i32, String> {
    let _arm = ArmScope::set(st.arm);
    let ptr = st as *const HarnessState as usize as i64;
    match h.vm.invoke(FIXTURE, runner, "(J)I", &[Value::Long(ptr)]) {
        Ok(Some(Value::Int(v))) => Ok(v),
        other => Err(format!("{other:?}")),
    }
}

/// Report the facts; fail with every divergence the native collected.
fn assert_conformant(label: &str, st: &HarnessState, rc: Result<i32, String>) {
    for f in st.facts.borrow().iter() {
        eprintln!("[jni-harness] {label} fact {f}");
    }
    let mut failures = st.failures.borrow().clone();
    match rc {
        Ok(0) => {}
        Ok(n) => failures.push(format!("the native answered {n}")),
        Err(e) => failures.push(format!("the call into Java failed: {e}")),
    }
    assert!(
        failures.is_empty(),
        "{label}: {} JNI conformance failure(s):\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}

fn battery_on(arm: Arm) {
    let mut h = boot(GcAlgorithm::Generational);
    let st = HarnessState::new(arm, &h.vm.shared);
    let rc = invoke(&mut h, "runHarness", &st);
    assert_conformant(&format!("battery {arm:?}"), &st, rc);
}

#[test]
fn jni_conformance_battery_counted_arm() {
    battery_on(Arm::Counted);
}

#[test]
fn jni_conformance_battery_deposit_arm() {
    battery_on(Arm::Deposit);
}

#[test]
fn jni_conformance_battery_lazy_arm() {
    battery_on(Arm::Lazy);
}

fn stress_on(gc_algorithm: GcAlgorithm) {
    for arm in ALL_ARMS {
        let mut h = boot(gc_algorithm);
        let st = HarnessState::new(arm, &h.vm.shared);
        let rc = invoke(&mut h, "runStress", &st);
        assert_conformant(&format!("stress {gc_algorithm:?} {arm:?}"), &st, rc);
    }
}

#[test]
fn jni_locals_survive_a_collection_inside_an_upcall_generational() {
    stress_on(GcAlgorithm::Generational);
}

#[test]
fn jni_locals_survive_a_collection_inside_an_upcall_g1() {
    stress_on(GcAlgorithm::G1);
}

/// The collecting thread of `cross`: registered only once the native is in
/// native (a registered thread that never polls would hold any earlier pause),
/// one full collection, then dead before it answers.
fn spawn_collector(
    shared: Arc<SharedVm>,
    ready: mpsc::Receiver<()>,
    done: mpsc::Sender<Result<(), String>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let outcome = match ready.recv_timeout(WAIT) {
            Err(_) => Err("the native never reached its wait".to_string()),
            Ok(()) => {
                let registry = &shared.threads.thread_registry;
                let tid = registry.next_thread_id();
                let mut jt = Box::new(JvmThread::new(tid, "jni-harness-collector"));
                registry.register(tid, "jni-harness-collector", None);
                registry.set_gc_block_state(tid, jt.gc_block_state.clone());
                registry.set_root_snapshot(tid, jt.root_snapshot.clone());
                let load = || {
                    shared
                        .mem
                        .gc_cycle_count
                        .load(std::sync::atomic::Ordering::Relaxed)
                };
                let before = load();
                crate::runtime::interpreter::force_gc_for_vm(&shared, &mut jt);
                let after = load();
                registry.mark_dead(tid);
                drop(jt);
                if after > before {
                    Ok(())
                } else {
                    Err(format!("no collection ran ({before} -> {after} cycles)"))
                }
            }
        };
        let _ = done.send(outcome);
    })
}

fn cross_on(gc_algorithm: GcAlgorithm) {
    for arm in [Arm::Deposit, Arm::Lazy] {
        let mut h = boot(gc_algorithm);
        let (ready_tx, ready_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let collector = spawn_collector(Arc::clone(&h.vm.shared), ready_rx, done_tx);
        let mut st = HarnessState::new(arm, &h.vm.shared);
        st.ready = Some(ready_tx);
        st.done = Some(done_rx);
        // Made on this thread outside any native: the String is reachable
        // only through the global ref once the setup frame closes.
        st.global = {
            let _ctx = SetupContext::install(&mut h.vm);
            let j = Jni(get_jni_env());
            let s = j.new_string_utf(CROSS_TEXT);
            j.new_global_ref(s)
        };
        assert_ne!(st.global, 0, "NewGlobalRef for the cross row");
        let rc = invoke(&mut h, "runCross", &st);
        // Hang up first, so a collector still waiting for `ready` ends.
        st.ready = None;
        let joined = collector.join();
        assert!(joined.is_ok(), "the collecting thread panicked");
        assert_conformant(&format!("cross {gc_algorithm:?} {arm:?}"), &st, rc);
    }
}

#[test]
fn jni_locals_survive_a_collection_by_another_thread_while_in_native_generational() {
    cross_on(GcAlgorithm::Generational);
}

#[test]
fn jni_locals_survive_a_collection_by_another_thread_while_in_native_g1() {
    cross_on(GcAlgorithm::G1);
}

/// The fixture bytes are what the harness assumes: a quick structural check
/// that fails here, not as a confusing class-loading error in every row.
#[test]
fn jni_harness_fixture_classes_parse() {
    for (name, bytes) in [(BASE, base_class_bytes()), (FIXTURE, fixture_class_bytes())] {
        assert_eq!(&bytes[..4], &[0xCA, 0xFE, 0xBA, 0xBE], "{name}: magic");
        let parsed = cratonvm_reader::read_class(&bytes);
        assert!(
            parsed.is_ok(),
            "{name}: the class reader rejects the fixture: {:?}",
            parsed.err()
        );
    }
}
