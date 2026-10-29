// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 9 (lane W9EXC): a null `MethodHandle` at a COMPILED caller's
//! `invokeExact` / `invoke` raises HotSpot's compiled-code answer.
//!
//! HotSpot 25.0.3 (`tools/probes/jit/j27/J27NpeHiddenRows.java` row
//! `mh-null-handle`, page
//! `docs/internal/jit/j27-mh-null-handle-invokeexact-npe-is-the-interpreter-answer-FIXED-20261029.md`):
//! the interpreter null-checks the receiver at the call
//! (`Cannot invoke "java.lang.invoke.MethodHandle.invokeExact(String)" because "<local1>" is null`,
//! top frame the caller); a compiled caller (default `java`, C1 only) links the
//! site to the invoker, whose `Invokers.checkExactType(mh, type)` /
//! `checkGenericType(mh, type)` dereferences the handle first
//! (`Cannot invoke "java.lang.invoke.MethodHandle.type()" because "mh" is null`,
//! top frame `Invokers.checkExactType`, then the caller).
//!
//! CratonVM's compiled code traps at the call. [`note_null_invoker_level`]
//! adds the check method to the trapping frame's inline chain, at its call of
//! `type()` / `asType(..)`, as if the body had inlined it (which is what
//! HotSpot's compiled code did). The JEP 358 message and the trace are then
//! built from that level by the ordinary compiled-NPE path
//! (`jit_npe_message`, `stackwalker::push_compiled_frames`). The interpreter's
//! own throw (`--nojit`, `java -Xint`) is untouched.
//!
//! `CRATONVM_JIT_MH_NULL_INVOKER_FRAME` (default ON, `=0` keeps the call-site
//! answer). Fails closed (no change) whenever a step cannot be named: the
//! trapping opcode is not such a call, `Invokers` is not loaded, its method or
//! call is missing.

use crate::jit::conservative_roots::{ActiveCompiledFrame, InlinedLevel};
use crate::vm::SharedVm;
use cratonvm_types::ClassId;
use std::sync::Arc;

const METHOD_HANDLE: &str = "java/lang/invoke/MethodHandle";
const INVOKERS: &str = "java/lang/invoke/Invokers";

/// The `Invokers` check HotSpot's compiled invoker runs first for a call of
/// `owner.name`, as `(method, descriptor, the MethodHandle method it calls on
/// the handle)`. `None` for any call that is not a `MethodHandle` invoker.
pub(crate) fn null_invoker_check(
    owner: &str,
    name: &str,
) -> Option<(&'static str, &'static str, &'static str)> {
    if owner != METHOD_HANDLE {
        return None;
    }
    match name {
        "invokeExact" => Some((
            "checkExactType",
            "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodType;)V",
            "type",
        )),
        "invoke" => Some((
            "checkGenericType",
            "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
            "asType",
        )),
        _ => None,
    }
}

/// The inline-chain label of `Invokers.<method><descriptor>`, in the
/// `"class/Name.method:descriptor"` shape every level carries.
fn invokers_label(method: &str, descriptor: &str) -> Arc<str> {
    Arc::from(format!("{INVOKERS}.{method}:{descriptor}"))
}

fn enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_MH_NULL_INVOKER_FRAME")
}

/// The pc of the first `invokevirtual owner.name` in `code`.
fn first_virtual_call(
    cp: &cratonvm_reader::constant_pool::ConstantPool,
    code: &[u8],
    owner: &str,
    name: &str,
) -> Option<u32> {
    use cratonvm_reader::instruction::Instruction;
    let mut pc = 0usize;
    while pc < code.len() {
        let (instruction, next) = Instruction::decode(code, pc).ok()?;
        if let Instruction::Invokevirtual(index) = instruction {
            if crate::runtime::stackwalker::cp_method_ref(cp, index)
                .is_some_and(|(o, n, _)| o == owner && n == name)
            {
                return u32::try_from(pc).ok();
            }
        }
        pc = next;
    }
    None
}

/// The `MethodHandle` method an `invokevirtual` at `bci` of the method
/// `label` (`"class/Name.method:descriptor"`, class `class_id`) calls.
fn invoker_called_at(
    store: &crate::classloading::ClassStore,
    class_id: u32,
    label: &str,
    bci: u32,
) -> Option<(&'static str, &'static str, &'static str)> {
    use cratonvm_reader::instruction::Instruction;
    let (owner_and_method, descriptor) = label.rsplit_once(':')?;
    let (_, method_name) = owner_and_method.rsplit_once('.')?;
    let class = store.get(ClassId::new(class_id))?;
    let code = &class.find_method(method_name, descriptor)?.code()?.code;
    let Instruction::Invokevirtual(index) = Instruction::decode(code, bci as usize).ok()?.0 else {
        return None;
    };
    let (owner, name, _) = crate::runtime::stackwalker::cp_method_ref(&class.constant_pool, index)?;
    null_invoker_check(owner, name)
}

/// The level standing for the invoker's check, for a trap at `label`'s
/// `bci`, or `None`.
fn check_level(shared: &SharedVm, class_id: u32, label: &str, bci: u32) -> Option<InlinedLevel> {
    let cm = shared.classes.class_manager.read_recursive();
    let store = &cm.class_store;
    let (check, check_descriptor, callee) = invoker_called_at(store, class_id, label, bci)?;
    let invokers_id = cm.get_loaded_class_id(INVOKERS)?;
    let invokers = store.get(invokers_id)?;
    let code = &invokers.find_method(check, check_descriptor)?.code()?.code;
    let call = first_virtual_call(&invokers.constant_pool, code, METHOD_HANDLE, callee)?;
    Some(InlinedLevel {
        label: invokers_label(check, check_descriptor),
        bci: call,
        class_id: invokers_id.as_u32(),
        hidden: Some(false),
    })
}

/// Make the innermost frame of `snapshot` stand in the invoker's check when
/// it trapped on a null `MethodHandle` at an `invokeExact` / `invoke` (see the
/// module docs). Answers whether it did.
pub(crate) fn note_null_invoker_level(
    shared: &SharedVm,
    snapshot: Option<&mut Vec<ActiveCompiledFrame>>,
) -> bool {
    let Some(frame) = snapshot.and_then(|s| s.last_mut()) else {
        return false;
    };
    if !enabled() {
        return false;
    }
    let (class_id, label, bci) = match frame.chain().first() {
        Some(level) => (level.class_id, Arc::clone(&level.label), level.bci),
        None => match u32::try_from(frame.bci) {
            Ok(bci) if !frame.chain_poisoned || frame.trap_site_exact => {
                (frame.owner_class_id, Arc::clone(&frame.label), bci)
            }
            _ => return false,
        },
    };
    let Some(level) = check_level(shared, class_id, &label, bci) else {
        return false;
    };
    let mut chain = frame.chain().to_vec();
    chain.insert(0, level);
    frame.inline_chain = chain;
    frame.inline_chain_shared = None;
    frame.trap_site_exact = true;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invoke_exact_and_invoke_name_their_checks() {
        assert_eq!(
            null_invoker_check(METHOD_HANDLE, "invokeExact").map(|c| (c.0, c.2)),
            Some(("checkExactType", "type"))
        );
        assert_eq!(
            null_invoker_check(METHOD_HANDLE, "invoke").map(|c| (c.0, c.2)),
            Some(("checkGenericType", "asType"))
        );
        // Not an invoker: the call-site answer stands.
        assert_eq!(null_invoker_check(METHOD_HANDLE, "invokeWithArguments"), None);
        assert_eq!(null_invoker_check(METHOD_HANDLE, "type"), None);
        assert_eq!(null_invoker_check("java/lang/invoke/VarHandle", "invokeExact"), None);
    }

    #[test]
    fn the_level_label_is_a_compiled_frame_label() {
        let (method, descriptor, _) = null_invoker_check(METHOD_HANDLE, "invokeExact").unwrap();
        assert_eq!(
            &*invokers_label(method, descriptor),
            "java/lang/invoke/Invokers.checkExactType:(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodType;)V"
        );
    }
}
