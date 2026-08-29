// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A minimal `.class` file writer for the verifier corpus.
//!
//! Every corpus case is a real byte array with a real `CAFEBABE` header, a real
//! constant pool and a real `Code` attribute — parsed back by
//! [`cratonvm_reader::read_class`] before it reaches the verifier. Building the
//! bytes in Rust (rather than shipping compiled fixtures) is deliberate:
//!
//!  * `javac` cannot emit most of what a verifier corpus needs. There is no way
//!    to ask it for a branch into the middle of an instruction, a handler whose
//!    `handler_pc` is out of range, or a `new` whose operand points at a `Utf8`
//!    constant. Those shapes only exist in hand-written bytes.
//!  * the corpus stays runnable with `cargo test` alone — no JDK on the build
//!    host, no committed binaries whose provenance nobody can check.
//!
//! The builder performs **no validation of its own**. That is the point: it
//! must be able to emit malformed class files, otherwise it could not produce
//! the negative half of the corpus. Whatever it emits is what the reader and
//! the verifier get.

#![allow(dead_code)]

/// Constant-pool tags (JVMS §4.4, Table 4.4-A).
mod tag {
    pub const UTF8: u8 = 1;
    pub const INTEGER: u8 = 3;
    pub const LONG: u8 = 5;
    pub const CLASS: u8 = 7;
    pub const STRING: u8 = 8;
    pub const FIELDREF: u8 = 9;
    pub const METHODREF: u8 = 10;
    pub const NAME_AND_TYPE: u8 = 12;
}

/// One entry of a method's exception table (JVMS §4.7.3).
#[derive(Debug, Clone, Copy)]
pub struct Handler {
    pub start_pc: u16,
    pub end_pc: u16,
    pub handler_pc: u16,
    /// Constant-pool index of the caught class, or `0` for catch-all.
    pub catch_type: u16,
}

impl Handler {
    /// A catch-all (`finally`) handler.
    pub fn catch_all(start_pc: u16, end_pc: u16, handler_pc: u16) -> Self {
        Handler {
            start_pc,
            end_pc,
            handler_pc,
            catch_type: 0,
        }
    }
}

/// A method to emit. Build with [`MethodSpec::new`] and the `with_*` setters.
#[derive(Debug, Clone)]
pub struct MethodSpec {
    pub access_flags: u16,
    pub name: String,
    pub descriptor: String,
    pub max_stack: u16,
    pub max_locals: u16,
    pub code: Vec<u8>,
    pub handlers: Vec<Handler>,
    /// Raw `StackMapTable` attribute payload — `number_of_entries` followed by
    /// the encoded frames (JVMS §4.7.4). `None` emits no `StackMapTable`.
    pub stack_map_table: Option<Vec<u8>>,
}

pub const ACC_PUBLIC: u16 = 0x0001;
pub const ACC_STATIC: u16 = 0x0008;

impl MethodSpec {
    pub fn new(
        name: &str,
        descriptor: &str,
        max_stack: u16,
        max_locals: u16,
        code: Vec<u8>,
    ) -> Self {
        MethodSpec {
            access_flags: ACC_PUBLIC | ACC_STATIC,
            name: name.to_string(),
            descriptor: descriptor.to_string(),
            max_stack,
            max_locals,
            code,
            handlers: Vec::new(),
            stack_map_table: None,
        }
    }

    /// An instance method (clears `ACC_STATIC`, so local 0 is `this`).
    pub fn instance(mut self) -> Self {
        self.access_flags &= !ACC_STATIC;
        self
    }

    pub fn with_handlers(mut self, handlers: Vec<Handler>) -> Self {
        self.handlers = handlers;
        self
    }

    /// Attach a `StackMapTable` built from `frames`, each already encoded.
    pub fn with_stack_map(mut self, frames: Vec<Vec<u8>>) -> Self {
        let mut payload = Vec::new();
        payload.extend_from_slice(&(frames.len() as u16).to_be_bytes());
        for frame in frames {
            payload.extend_from_slice(&frame);
        }
        self.stack_map_table = Some(payload);
        self
    }
}

/// A `same_frame` entry (JVMS §4.7.4): identical locals, empty stack.
/// `offset_delta` must be in `0..=63`.
pub fn same_frame(offset_delta: u8) -> Vec<u8> {
    assert!(
        offset_delta <= 63,
        "same_frame offset_delta must fit 0..=63"
    );
    vec![offset_delta]
}

/// Accumulates a constant pool and method table, then emits the class bytes.
pub struct ClassBuilder {
    minor: u16,
    major: u16,
    /// Encoded pool entries. `pool[i]` is constant-pool index `i + 1`.
    pool: Vec<Vec<u8>>,
    access_flags: u16,
    this_class: u16,
    super_class: u16,
    methods: Vec<Vec<u8>>,
}

impl ClassBuilder {
    /// Start a `public super` class named `name` extending `java/lang/Object`.
    pub fn new(name: &str, major: u16) -> Self {
        let mut b = ClassBuilder {
            minor: 0,
            major,
            pool: Vec::new(),
            access_flags: 0x0021, // ACC_PUBLIC | ACC_SUPER
            this_class: 0,
            super_class: 0,
            methods: Vec::new(),
        };
        let this_class = b.class(name);
        let super_class = b.class("java/lang/Object");
        b.this_class = this_class;
        b.super_class = super_class;
        b
    }

    fn add(&mut self, bytes: Vec<u8>) -> u16 {
        self.pool.push(bytes);
        // Pool indices are 1-based.
        self.pool.len() as u16
    }

    pub fn utf8(&mut self, s: &str) -> u16 {
        let mut e = vec![tag::UTF8];
        e.extend_from_slice(&(s.len() as u16).to_be_bytes());
        e.extend_from_slice(s.as_bytes());
        self.add(e)
    }

    pub fn class(&mut self, name: &str) -> u16 {
        let name_index = self.utf8(name);
        let mut e = vec![tag::CLASS];
        e.extend_from_slice(&name_index.to_be_bytes());
        self.add(e)
    }

    pub fn integer(&mut self, value: i32) -> u16 {
        let mut e = vec![tag::INTEGER];
        e.extend_from_slice(&value.to_be_bytes());
        self.add(e)
    }

    pub fn string(&mut self, value: &str) -> u16 {
        let utf8_index = self.utf8(value);
        let mut e = vec![tag::STRING];
        e.extend_from_slice(&utf8_index.to_be_bytes());
        self.add(e)
    }

    /// A `CONSTANT_Long`, which occupies **two** pool slots (JVMS §4.4.5).
    pub fn long(&mut self, value: i64) -> u16 {
        let mut e = vec![tag::LONG];
        e.extend_from_slice(&value.to_be_bytes());
        let index = self.add(e);
        // The unusable second slot is NOT written to the file — the reader
        // accounts for it when it advances its own index (JVMS §4.4.5). Reserve
        // it here with a zero-byte entry so our index arithmetic and the
        // emitted `constant_pool_count` both stay correct.
        self.add(Vec::new());
        index
    }

    pub fn name_and_type(&mut self, name: &str, descriptor: &str) -> u16 {
        let n = self.utf8(name);
        let d = self.utf8(descriptor);
        let mut e = vec![tag::NAME_AND_TYPE];
        e.extend_from_slice(&n.to_be_bytes());
        e.extend_from_slice(&d.to_be_bytes());
        self.add(e)
    }

    pub fn method_ref(&mut self, owner: &str, name: &str, descriptor: &str) -> u16 {
        let c = self.class(owner);
        let nt = self.name_and_type(name, descriptor);
        let mut e = vec![tag::METHODREF];
        e.extend_from_slice(&c.to_be_bytes());
        e.extend_from_slice(&nt.to_be_bytes());
        self.add(e)
    }

    pub fn field_ref(&mut self, owner: &str, name: &str, descriptor: &str) -> u16 {
        let c = self.class(owner);
        let nt = self.name_and_type(name, descriptor);
        let mut e = vec![tag::FIELDREF];
        e.extend_from_slice(&c.to_be_bytes());
        e.extend_from_slice(&nt.to_be_bytes());
        self.add(e)
    }

    /// The index the next `add`-ing helper will return. Useful when a method
    /// body must reference a constant that is created after the code bytes.
    pub fn next_index(&self) -> u16 {
        self.pool.len() as u16 + 1
    }

    pub fn add_method(&mut self, spec: MethodSpec) {
        let name_index = self.utf8(&spec.name);
        let descriptor_index = self.utf8(&spec.descriptor);
        let code_attr_name = self.utf8("Code");
        let smt_name = if spec.stack_map_table.is_some() {
            Some(self.utf8("StackMapTable"))
        } else {
            None
        };

        // Code attribute body (everything after attribute_length).
        let mut body = Vec::new();
        body.extend_from_slice(&spec.max_stack.to_be_bytes());
        body.extend_from_slice(&spec.max_locals.to_be_bytes());
        body.extend_from_slice(&(spec.code.len() as u32).to_be_bytes());
        body.extend_from_slice(&spec.code);
        body.extend_from_slice(&(spec.handlers.len() as u16).to_be_bytes());
        for h in &spec.handlers {
            body.extend_from_slice(&h.start_pc.to_be_bytes());
            body.extend_from_slice(&h.end_pc.to_be_bytes());
            body.extend_from_slice(&h.handler_pc.to_be_bytes());
            body.extend_from_slice(&h.catch_type.to_be_bytes());
        }
        match (&spec.stack_map_table, smt_name) {
            (Some(payload), Some(name)) => {
                body.extend_from_slice(&1u16.to_be_bytes()); // attributes_count
                body.extend_from_slice(&name.to_be_bytes());
                body.extend_from_slice(&(payload.len() as u32).to_be_bytes());
                body.extend_from_slice(payload);
            }
            _ => body.extend_from_slice(&0u16.to_be_bytes()), // attributes_count
        }

        let mut method_info = Vec::new();
        method_info.extend_from_slice(&spec.access_flags.to_be_bytes());
        method_info.extend_from_slice(&name_index.to_be_bytes());
        method_info.extend_from_slice(&descriptor_index.to_be_bytes());
        method_info.extend_from_slice(&1u16.to_be_bytes()); // attributes_count
        method_info.extend_from_slice(&code_attr_name.to_be_bytes());
        method_info.extend_from_slice(&(body.len() as u32).to_be_bytes());
        method_info.extend_from_slice(&body);

        self.methods.push(method_info);
    }

    pub fn build(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&0xCAFE_BABE_u32.to_be_bytes());
        out.extend_from_slice(&self.minor.to_be_bytes());
        out.extend_from_slice(&self.major.to_be_bytes());
        // constant_pool_count is the number of entries PLUS ONE (JVMS §4.1).
        out.extend_from_slice(&(self.pool.len() as u16 + 1).to_be_bytes());
        for entry in &self.pool {
            out.extend_from_slice(entry);
        }
        out.extend_from_slice(&self.access_flags.to_be_bytes());
        out.extend_from_slice(&self.this_class.to_be_bytes());
        out.extend_from_slice(&self.super_class.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes()); // interfaces_count
        out.extend_from_slice(&0u16.to_be_bytes()); // fields_count
        out.extend_from_slice(&(self.methods.len() as u16).to_be_bytes());
        for m in &self.methods {
            out.extend_from_slice(m);
        }
        out.extend_from_slice(&0u16.to_be_bytes()); // class attributes_count
        out
    }
}

// ---------------------------------------------------------------------------
// Opcodes used by the corpus (JVMS §6.5)
// ---------------------------------------------------------------------------

pub mod op {
    pub const ACONST_NULL: u8 = 0x01;
    pub const ICONST_0: u8 = 0x03;
    pub const LCONST_0: u8 = 0x09;
    pub const SIPUSH: u8 = 0x11;
    pub const ILOAD_0: u8 = 0x1a;
    pub const LLOAD_0: u8 = 0x1e;
    pub const ALOAD_0: u8 = 0x2a;
    pub const ISTORE_0: u8 = 0x3b;
    pub const ISTORE_1: u8 = 0x3c;
    pub const ISTORE_2: u8 = 0x3d;
    pub const ISTORE_3: u8 = 0x3e;
    pub const LSTORE_0: u8 = 0x3f;
    pub const ASTORE_1: u8 = 0x4c;
    pub const ASTORE_2: u8 = 0x4d;
    pub const POP: u8 = 0x57;
    pub const POP2: u8 = 0x58;
    pub const DUP: u8 = 0x59;
    pub const IFEQ: u8 = 0x99;
    pub const GOTO: u8 = 0xa7;
    pub const ATHROW: u8 = 0xbf;
    pub const RETURN: u8 = 0xb1;
    pub const INVOKESPECIAL: u8 = 0xb7;
    pub const INVOKESTATIC: u8 = 0xb8;
    pub const NEW: u8 = 0xbb;
}

/// `opcode` followed by a big-endian `u16` operand.
pub fn u16_op(opcode: u8, operand: u16) -> [u8; 3] {
    let [hi, lo] = operand.to_be_bytes();
    [opcode, hi, lo]
}

/// `opcode` followed by a big-endian `i16` branch offset.
pub fn branch(opcode: u8, offset: i16) -> [u8; 3] {
    let [hi, lo] = offset.to_be_bytes();
    [opcode, hi, lo]
}
