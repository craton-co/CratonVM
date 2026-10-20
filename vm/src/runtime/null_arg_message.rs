// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The helpful-NPE message of a null ARGUMENT, asked of the JDK's own
//! bytecode (JIT round 16, lane compatdrain, proposal C5W4-1 of
//! `docs/internal/jit-proposals/jit-r15w4-compat5-proposals-RETIRED-20261003.md`).
//!
//! # Why
//!
//! A native that stands for a JDK method and refuses a null argument has to
//! throw the NPE HotSpot throws when the JDK body dereferences that argument:
//! `Cannot invoke "java.util.Collection.toArray()" because "c" is null`. The
//! collection natives carry those lines as transcribed literals
//! (`NULL_C_TO_ARRAY`, `NULL_C_SIZE`, ...), each needing a HotSpot run to
//! confirm and each silently wrong when a JDK update renames a parameter or
//! reorders a body. HotSpot builds the line from the bytecode that trapped and
//! its `LocalVariableTable`; so can this VM, from the same bytecode, without
//! running it.
//!
//! # What the query answers
//!
//! [`null_arg_message_in`] reads the named method's bytecode and finds the
//! FIRST instruction that dereferences parameter `param` on the method's
//! straight-line prefix (bci 0 up to the first branch, switch, return or
//! throw), then renders it through [`helpful_npe`], exactly as the
//! interpreter would render that trap. Shapes it answers:
//!
//! * the parameter as the receiver of `invokeinterface` / `invokevirtual` /
//!   `invokespecial` (not `<init>`);
//! * the parameter as the operand of `arraylength`;
//! * the parameter passed on, as an argument, to a method of the SAME class
//!   called on `this` (or statically): the query follows that call, up to
//!   [`MAX_FOLLOW`] levels (`LinkedList.addAll(c)` -> `addAll(size, c)`).
//!
//! Everything else answers `None`, and the caller keeps its literal: a branch
//! before the dereference (the message would depend on the receiver's state:
//! `TreeSet.addAll`, `PriorityQueue(c)`), a dereference inside a `try` range,
//! the parameter handed to another class (`Objects.requireNonNull(c)` -- the
//! bare NPE -- or `HashMap.prepareArray(a)`), stored, compared, or consumed by
//! any other opcode first.
//!
//! The answer is "the message HotSpot prints IF execution reaches the
//! dereference"; whether it does (a preceding index check, a preceding call
//! that throws) is the caller's business, as it is today with the literals.

use crate::runtime::exceptions::helpful_npe::{self, CpRef, CpResolver, OperandOrigins};
use crate::vm::SharedVm;
use cratonvm_reader::attribute::{Attribute, CodeAttribute};
use cratonvm_reader::constant_pool::ConstantPoolEntry;
use cratonvm_reader::instruction::Instruction;
use cratonvm_reader::ConstantPool;

/// How many same-class calls the query follows the parameter through.
const MAX_FOLLOW: u32 = 4;

/// One method body the query reads.
pub(crate) struct MethodBody<'a> {
    /// The declaring class, internal form (`java/util/ArrayList`).
    pub class_name: &'a str,
    pub descriptor: &'a str,
    pub is_static: bool,
    pub cp: &'a ConstantPool,
    pub code: &'a CodeAttribute,
}

/// Where the query finds a method's body. The VM answers from its loaded
/// classes ([`LoadedClasses`]); the unit test from a parsed `java.base.jmod`.
pub(crate) trait MethodSource {
    /// Call `f` with the body of `class.name descriptor`, or answer `None`
    /// when the class or the method (or its `Code`) is not there.
    fn with_body(
        &self,
        class: &str,
        name: &str,
        descriptor: &str,
        f: &mut dyn FnMut(&MethodBody<'_>) -> Option<String>,
    ) -> Option<String>;
}

/// The JEP 358 message HotSpot prints when `class.name descriptor` is entered
/// with a null `param` (0-based, not counting `this`) and execution reaches
/// its first dereference. See the module doc for the shapes answered.
pub(crate) fn null_arg_message_in(
    src: &dyn MethodSource,
    class: &str,
    name: &str,
    descriptor: &str,
    param: usize,
) -> Option<String> {
    follow(src, class, name, descriptor, param, 0)
}

fn follow(
    src: &dyn MethodSource,
    class: &str,
    name: &str,
    descriptor: &str,
    param: usize,
    level: u32,
) -> Option<String> {
    if level > MAX_FOLLOW {
        return None;
    }
    src.with_body(class, name, descriptor, &mut |body| {
        scan(src, body, param, level)
    })
}

/// The descriptor tokens of a method's parameters (`I`, `J`, `[I`,
/// `Ljava/util/Collection;`).
fn param_tokens(descriptor: &str) -> Option<Vec<&str>> {
    let bytes = descriptor.as_bytes();
    if bytes.first() != Some(&b'(') {
        return None;
    }
    let mut out = Vec::new();
    let mut i = 1;
    while *bytes.get(i)? != b')' {
        let start = i;
        while *bytes.get(i)? == b'[' {
            i += 1;
        }
        if *bytes.get(i)? == b'L' {
            while *bytes.get(i)? != b';' {
                i += 1;
            }
        }
        i += 1;
        out.push(descriptor.get(start..i)?);
    }
    Some(out)
}

/// The local slot of reference parameter `param`, or `None` when there is
/// no such parameter or it is a primitive (nothing to dereference).
fn param_slot(descriptor: &str, is_static: bool, param: usize) -> Option<u16> {
    let tokens = param_tokens(descriptor)?;
    let token = tokens.get(param)?;
    if !matches!(token.as_bytes().first(), Some(b'L' | b'[')) {
        return None;
    }
    let mut slot: u16 = if is_static { 0 } else { 1 };
    for t in &tokens[..param] {
        slot = slot.checked_add(if *t == "J" || *t == "D" { 2 } else { 1 })?;
    }
    Some(slot)
}

/// Does `instr` end the straight-line prefix?
fn ends_prefix(instr: &Instruction) -> bool {
    instr.is_branch()
        || instr.is_return()
        || matches!(
            instr,
            Instruction::Athrow | Instruction::Jsr(_) | Instruction::JsrW(_) | Instruction::Ret(_)
        )
}

/// The query over one body: see the module doc.
fn scan(src: &dyn MethodSource, body: &MethodBody<'_>, param: usize, level: u32) -> Option<String> {
    let slot = param_slot(body.descriptor, body.is_static, param)?;
    // A dereference inside a protected range could be caught (whatever the
    // catch type: a handler's own code is not a first-step shape).
    let protected = |bci: usize| {
        body.code
            .exception_table
            .iter()
            .any(|e| usize::from(e.start_pc) <= bci && bci < usize::from(e.end_pc))
    };
    let code: &[u8] = &body.code.code;
    let mut prefix: Vec<(usize, Instruction)> = Vec::new();
    let mut bci = 0usize;
    loop {
        let (instr, next) = Instruction::decode(code, bci).ok()?;
        let last = ends_prefix(&instr) || next >= code.len();
        prefix.push((bci, instr));
        if last {
            break;
        }
        bci = next;
    }
    let end = prefix.last()?.0;
    let resolver = BodyResolver { body };
    let origins = OperandOrigins::run(code, end, &resolver)?;
    for (bci, instr) in &prefix {
        let bci = *bci;
        // Every prefix instruction must have a state; a gap means the
        // analysis stopped on something it does not model, and "not the
        // parameter" could then be wrong.
        origins.height_at(bci)?;
        let is_param = |depth: usize| origins.unwritten_local_at(code, bci, depth) == Some(slot);
        match instr {
            Instruction::Invokevirtual(idx)
            | Instruction::Invokespecial(idx)
            | Instruction::Invokestatic(idx)
            | Instruction::Invokeinterface { index: idx, .. } => {
                let CpRef::Method {
                    owner_internal,
                    name,
                    descriptor,
                } = resolver.method_ref(*idx)?
                else {
                    return None;
                };
                let n = param_tokens(&descriptor)?.len();
                let has_receiver = !matches!(instr, Instruction::Invokestatic(_));
                if has_receiver && is_param(n) {
                    if name == "<init>" || protected(bci) {
                        return None;
                    }
                    let action = helpful_npe::action_invoke(&owner_internal, &name, &descriptor);
                    let expr = helpful_npe::null_expr_for_invoke_receiver(code, bci, n, &resolver);
                    return Some(helpful_npe::combine(&action, expr.as_ref()));
                }
                let mut passed = None;
                for depth in 0..n {
                    if is_param(depth) {
                        if passed.is_some() {
                            return None;
                        }
                        passed = Some(n - 1 - depth);
                    }
                }
                if let Some(arg) = passed {
                    // Followed only into this class's own method, on `this`
                    // (or a static one): another class's body is not this
                    // method's question (`Objects.requireNonNull(c)` is the
                    // bare NPE; the caller's literal answers it).
                    if *owner_internal != *body.class_name || protected(bci) {
                        return None;
                    }
                    if has_receiver
                        && (body.is_static
                            || matches!(instr, Instruction::Invokeinterface { .. })
                            || origins.unwritten_local_at(code, bci, n) != Some(0))
                    {
                        return None;
                    }
                    return follow(src, &owner_internal, &name, &descriptor, arg, level + 1);
                }
            }
            Instruction::Invokedynamic(idx) => {
                let descriptor = resolver.invokedynamic_descriptor(*idx)?;
                let n = param_tokens(&descriptor)?.len();
                if (0..n).any(is_param) {
                    return None;
                }
            }
            Instruction::Arraylength => {
                if is_param(0) {
                    if protected(bci) {
                        return None;
                    }
                    let expr = helpful_npe::null_expr_at_depth(code, bci, 0, &resolver);
                    return Some(helpful_npe::combine(
                        &helpful_npe::action_array_length(),
                        expr.as_ref(),
                    ));
                }
            }
            // Every other opcode that consumes a reference operand: the
            // parameter used here first is not a shape the query answers.
            Instruction::Getfield(_)
            | Instruction::Putstatic(_)
            | Instruction::Astore(_)
            | Instruction::Areturn
            | Instruction::Athrow
            | Instruction::Monitorenter
            | Instruction::Monitorexit
            | Instruction::Ifnull(_)
            | Instruction::Ifnonnull(_)
            | Instruction::Instanceof(_) => {
                if is_param(0) {
                    return None;
                }
            }
            Instruction::Putfield(_) | Instruction::IfAcmpeq(_) | Instruction::IfAcmpne(_) => {
                if is_param(0) || is_param(1) {
                    return None;
                }
            }
            Instruction::Iaload
            | Instruction::Laload
            | Instruction::Faload
            | Instruction::Daload
            | Instruction::Aaload
            | Instruction::Baload
            | Instruction::Caload
            | Instruction::Saload => {
                if is_param(1) {
                    return None;
                }
            }
            Instruction::Iastore
            | Instruction::Lastore
            | Instruction::Fastore
            | Instruction::Dastore
            | Instruction::Bastore
            | Instruction::Castore
            | Instruction::Sastore => {
                if is_param(2) {
                    return None;
                }
            }
            Instruction::Aastore => {
                if is_param(0) || is_param(2) {
                    return None;
                }
            }
            _ => {}
        }
    }
    None
}

/// [`CpResolver`] over one [`MethodBody`]: the same answers
/// `interpreter::invoke::CpPoolResolver` gives over a loaded class.
struct BodyResolver<'b, 'a> {
    body: &'b MethodBody<'a>,
}

impl CpResolver for BodyResolver<'_, '_> {
    fn field_ref(&self, cp_index: u16) -> Option<CpRef> {
        let cp = self.body.cp;
        let Some(ConstantPoolEntry::FieldReference {
            class_index,
            name_and_type_index,
        }) = cp.get(cp_index)
        else {
            return None;
        };
        let owner = cp.get_class_name(*class_index)?.to_string();
        let (name, _) = cp.get_name_and_type(*name_and_type_index)?;
        Some(CpRef::Field {
            owner_internal: owner,
            name: name.to_string(),
        })
    }

    fn method_ref(&self, cp_index: u16) -> Option<CpRef> {
        let cp = self.body.cp;
        let (class_index, nat_index) = match cp.get(cp_index) {
            Some(ConstantPoolEntry::MethodReference {
                class_index,
                name_and_type_index,
            })
            | Some(ConstantPoolEntry::InterfaceMethodReference {
                class_index,
                name_and_type_index,
            }) => (*class_index, *name_and_type_index),
            _ => return None,
        };
        let owner = cp.get_class_name(class_index)?.to_string();
        let (name, desc) = cp.get_name_and_type(nat_index)?;
        Some(CpRef::Method {
            owner_internal: owner,
            name: name.to_string(),
            descriptor: desc.to_string(),
        })
    }

    fn local_name(&self, slot: u16, bci: usize) -> Option<String> {
        let bci = u16::try_from(bci).unwrap_or(u16::MAX);
        for attr in &self.body.code.attributes {
            if let Attribute::LocalVariableTable(entries) = attr {
                for e in entries {
                    let end = e.start_pc.saturating_add(e.length);
                    if e.index == slot && bci >= e.start_pc && bci < end {
                        if let Some(name) = self.body.cp.get_utf8(e.name_index) {
                            return Some(name.to_string());
                        }
                    }
                }
            }
        }
        None
    }

    fn is_static_method(&self) -> bool {
        self.body.is_static
    }

    fn method_descriptor(&self) -> Option<&str> {
        Some(self.body.descriptor)
    }

    fn field_is_category2(&self, cp_index: u16) -> Option<bool> {
        let cp = self.body.cp;
        let Some(ConstantPoolEntry::FieldReference {
            name_and_type_index,
            ..
        }) = cp.get(cp_index)
        else {
            return None;
        };
        let (_, desc) = cp.get_name_and_type(*name_and_type_index)?;
        Some(matches!(desc.as_bytes().first(), Some(b'J' | b'D')))
    }

    fn invokedynamic_descriptor(&self, cp_index: u16) -> Option<String> {
        let cp = self.body.cp;
        let Some(ConstantPoolEntry::InvokeDynamic {
            name_and_type_index,
            ..
        }) = cp.get(cp_index)
        else {
            return None;
        };
        let (_, desc) = cp.get_name_and_type(*name_and_type_index)?;
        Some(desc.to_string())
    }

    fn handler_pcs(&self) -> Vec<usize> {
        self.body
            .code
            .exception_table
            .iter()
            .map(|e| usize::from(e.handler_pc))
            .collect()
    }
}

/// [`MethodSource`] over the VM's loaded classes (by name, as
/// `jit_npe_message::trapping_code` falls back to). A class that is not
/// loaded, or loaded without the method, answers `None`.
pub(crate) struct LoadedClasses<'s> {
    pub shared: &'s SharedVm,
}

impl MethodSource for LoadedClasses<'_> {
    fn with_body(
        &self,
        class: &str,
        name: &str,
        descriptor: &str,
        f: &mut dyn FnMut(&MethodBody<'_>) -> Option<String>,
    ) -> Option<String> {
        let cm = self.shared.classes.class_manager.read_recursive();
        let id =
            crate::runtime::stackwalker::find_class_id_by_name_memoized(&cm.class_store, class)?;
        let loaded = cm.class_store.get(id)?;
        let method = loaded.find_method(name, descriptor)?;
        let code = method.code()?;
        f(&MethodBody {
            class_name: &loaded.name,
            descriptor,
            is_static: method.is_static(),
            cp: &loaded.constant_pool,
            code,
        })
    }
}

/// [`null_arg_message_in`] over the VM's loaded classes: what
/// `NativeContext::null_arg_message` answers. Not memoized: it runs only on a
/// null argument's error path, under a default-off switch, and re-reads at
/// most [`MAX_FOLLOW`] + 1 small bodies (a memo would be a new `static`, which
/// `vm/tests/per_vm_state_statics_ratchet.rs` counts).
pub(crate) fn null_arg_message(
    shared: &SharedVm,
    class: &str,
    name: &str,
    descriptor: &str,
    param: usize,
) -> Option<String> {
    null_arg_message_in(&LoadedClasses { shared }, class, name, descriptor, param)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn param_slots_skip_this_and_count_wide_parameters_twice() {
        assert_eq!(param_slot("(Ljava/util/Collection;)Z", false, 0), Some(1));
        assert_eq!(param_slot("(ILjava/util/Collection;)Z", false, 1), Some(2));
        assert_eq!(param_slot("(JLjava/lang/Object;)V", true, 1), Some(2));
        assert_eq!(param_slot("(D[I)V", false, 1), Some(3));
        // A primitive has nothing to dereference; past the end is no answer.
        assert_eq!(param_slot("(ILjava/util/Collection;)Z", false, 0), None);
        assert_eq!(param_slot("()V", false, 0), None);
        assert_eq!(
            param_tokens("([[Ljava/lang/String;JZ)V"),
            Some(vec!["[[Ljava/lang/String;", "J", "Z"])
        );
        assert_eq!(param_tokens("not a descriptor"), None);
    }

    /// `java.base`'s parsed classes, as a [`MethodSource`].
    struct JmodClasses {
        path: cratonvm_classloading::ClassPath,
        parsed: std::cell::RefCell<
            std::collections::HashMap<String, std::rc::Rc<cratonvm_reader::ClassFile>>,
        >,
    }

    impl JmodClasses {
        fn class(&self, name: &str) -> Option<std::rc::Rc<cratonvm_reader::ClassFile>> {
            if let Some(c) = self.parsed.borrow().get(name) {
                return Some(c.clone());
            }
            let bytes = self.path.find_class(name).ok()?;
            let mut class = cratonvm_reader::read_class_shared(bytes).ok()?;
            let cp = &class.constant_pool;
            for m in class.methods.iter_mut() {
                cratonvm_reader::force_decode_all(&mut m.attributes, cp).ok()?;
            }
            let class = std::rc::Rc::new(class);
            self.parsed
                .borrow_mut()
                .insert(name.to_string(), class.clone());
            Some(class)
        }
    }

    impl MethodSource for JmodClasses {
        fn with_body(
            &self,
            class: &str,
            name: &str,
            descriptor: &str,
            f: &mut dyn FnMut(&MethodBody<'_>) -> Option<String>,
        ) -> Option<String> {
            let parsed = self.class(class)?;
            let method = parsed.find_method(name, descriptor)?;
            let code = method.code()?;
            f(&MethodBody {
                class_name: &parsed.this_class,
                descriptor,
                is_static: method.is_static(),
                cp: &parsed.constant_pool,
                code,
            })
        }
    }

    fn java_base_jmod() -> Option<PathBuf> {
        let mut homes: Vec<PathBuf> = Vec::new();
        if let Ok(home) = cratonvm_types::flags::runtime_var("JAVA_HOME") {
            homes.push(PathBuf::from(home));
        }
        for candidate in [
            "C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot",
            "C:/Program Files/Java/jdk-25",
            "/data/toolchain/jdk-25",
        ] {
            homes.push(PathBuf::from(candidate));
        }
        homes
            .into_iter()
            .map(|h| h.join("jmods").join("java.base.jmod"))
            .find(|p| p.exists())
    }

    /// C5W4-1's first step: the query reproduces, from JDK 25's own bytecode,
    /// the lines HotSpot 25 printed for `R15Compat4NullArgMessages` rows
    /// 0-5, 8-9 and `R15Compat5NullArgMore` rows 4, 9 (transcribed in
    /// `native-collections` as `NULL_C_TO_ARRAY` / `NULL_C_SIZE` /
    /// `NULL_C_ITERATOR` / `NULL_A_LENGTH` / `NULL_M_COMPARATOR`; re-measured
    /// by `R16CompatdrainNullArgs`), and declines the bodies whose answer is
    /// not on a straight-line prefix or is not a dereference at all.
    #[test]
    fn the_query_reproduces_hotspots_null_argument_lines_from_jdk_bytecode() {
        let Some(jmod) = java_base_jmod() else {
            assert!(
                cratonvm_types::flags::runtime_var_os("CRATONVM_REQUIRE_E2E").is_none(),
                "no java.base.jmod (set JAVA_HOME)"
            );
            eprintln!("SKIP the_query_reproduces_hotspots_null_argument_lines: no java.base.jmod");
            return;
        };
        let src = JmodClasses {
            path: cratonvm_classloading::ClassPath::new(&[jmod.to_string_lossy().into_owned()]),
            parsed: Default::default(),
        };
        let to_array = "Cannot invoke \"java.util.Collection.toArray()\" because \"c\" is null";
        let size = "Cannot invoke \"java.util.Collection.size()\" because \"c\" is null";
        let iterator = "Cannot invoke \"java.util.Collection.iterator()\" because \"c\" is null";
        let length = "Cannot read the array length because \"a\" is null";
        let comparator = "Cannot invoke \"java.util.SortedMap.comparator()\" because \"m\" is null";
        let coll = "(Ljava/util/Collection;)Z";
        let ctor = "(Ljava/util/Collection;)V";
        let rows: [(&str, &str, &str, Option<&str>); 15] = [
            ("java/util/ArrayList", "addAll", coll, Some(to_array)),
            ("java/util/Vector", "addAll", coll, Some(to_array)),
            ("java/util/LinkedList", "addAll", coll, Some(to_array)),
            ("java/util/ArrayDeque", "addAll", coll, Some(size)),
            ("java/util/ArrayDeque", "<init>", ctor, Some(size)),
            ("java/util/LinkedList", "<init>", ctor, Some(to_array)),
            ("java/util/ArrayList", "<init>", ctor, Some(to_array)),
            ("java/util/HashSet", "<init>", ctor, Some(size)),
            (
                "java/util/AbstractCollection",
                "containsAll",
                "(Ljava/util/Collection;)Z",
                Some(iterator),
            ),
            (
                "java/util/ArrayList",
                "toArray",
                "([Ljava/lang/Object;)[Ljava/lang/Object;",
                Some(length),
            ),
            (
                "java/util/TreeMap",
                "<init>",
                "(Ljava/util/SortedMap;)V",
                Some(comparator),
            ),
            // `Objects.requireNonNull(c)` (through `batchRemove`): HotSpot's
            // bare NPE, which the query leaves to the caller.
            (
                "java/util/ArrayList",
                "removeAll",
                "(Ljava/util/Collection;)Z",
                None,
            ),
            (
                "java/util/ArrayDeque",
                "removeAll",
                "(Ljava/util/Collection;)Z",
                None,
            ),
            // A branch on the receiver's state comes first.
            ("java/util/TreeSet", "addAll", coll, None),
            // `a` goes to another class (`HashMap.prepareArray`).
            (
                "java/util/HashSet",
                "toArray",
                "([Ljava/lang/Object;)[Ljava/lang/Object;",
                None,
            ),
        ];
        for (class, name, desc, want) in rows {
            let got = null_arg_message_in(&src, class, name, desc, 0);
            assert_eq!(got.as_deref(), want, "{class}.{name}{desc}");
        }
        // `LinkedList.addAll(int, c)`: the second parameter.
        assert_eq!(
            null_arg_message_in(
                &src,
                "java/util/LinkedList",
                "addAll",
                "(ILjava/util/Collection;)Z",
                1
            )
            .as_deref(),
            Some(to_array)
        );
    }
}
