// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A door-invisible compiled body for a reviewed `Intrinsic`'s own-body
//! fallback (JIT round 21, lane reflect4, REFLECT4-21-1;
//! opt-in `CRATONVM_JIT_BYTECODE_ONLY_KEY=1` since dev's own re-land of the
//! item, which marks the method re-entry-only under `CRATONVM_JIT_BYTECODE_ONLY_BODY`
//! (default ON), became the default form;
//! page `docs/known-issues/jit/jr21-reflect3-bytecode-only-fallback-bodies-always-interpret-20261014.md`).
//!
//! # The defect
//!
//! A registered `Intrinsic` that answers "not mine, run the body" calls
//! `NativeInvokeAccess::invoke_virtual_bytecode_only` or one of the
//! `invoke_special_bytecode_only*` doors, which call [`super::execute`] on the
//! method's declaring class. `execute` asks `registered_native_will_run` for
//! the triple before its `jit_cache` consult and SEALS the method
//! (`calls-native-shadowed-method`): its own bytecode is never compiled, and
//! every call interprets it. Hot instance: `BuiltinClassLoader.loadClassOrNull`
//! (`bcl_load_class_or_null`) for every image / module class `loadClass` under
//! `--jdk-only`.
//!
//! # Why the body is not published under the method's own key
//!
//! Every path that serves a compiled body by key relies on the registered
//! native having kept one from existing (the statically bound `jit_cache` arm
//! of `jit_invoke_dispatch_body` asks no native question of its own), and these
//! intrinsics are not their bodies: `bcl_load_class_or_null` runs a package
//! step after the body's answer, `bcl_find_class_in_module` adds the
//! `--compatible` class-path lookup. A body under the ordinary key would be
//! served to ordinary callers in place of the intrinsic.
//!
//! # What this does instead
//!
//! The body is compiled by the callee door
//! (`jit_bridge::try_jit_compile_bytecode_only_body`, the same pipeline and
//! gates as every by-name compile, minus the method's OWN native refusal) and
//! published under the same class name, method name and class id with the
//! descriptor [`bytecode_only_key_descriptor`] names (`<bytecode-only>` + the
//! descriptor). No descriptor starts with `<`, so no ordinary lookup can find
//! it; every whole-cache pass (redefinition flush, dependency invalidation,
//! class unload, sweep) still sees it, because those match by class, class id
//! and inlined classes. Only [`execute_bytecode_only`] looks the key up, and it
//! enters the body through the frameless one-shot door the lambda path uses
//! (`jit_bridge::execute_jit_call_oneshot`): a throw is routed through the
//! method's own handler table, a deopt resumes the stashed frame, and a decline
//! re-runs the method interpreted from the written-back arguments. `execute`
//! keeps its seal for every other entry.
//!
//! Admission (the miss path only): the triple's registered kind is
//! `Intrinsic` and its native runs (`registered_native_will_run`; a
//! body-compilable intrinsic is compiled by `execute` already), the method has
//! `Code`, is not `synchronized` (the one-shot door holds no monitor) and does
//! not invoke itself (a self call bound raw would bypass the native for that
//! call). The door counts calls under its own key and compiles at the tier
//! threshold and at its next three doublings, on this thread.

use super::*;

/// The prefix of the door-invisible descriptor. No JVM descriptor starts with
/// `<` (a method descriptor starts with `(`), so a key carrying it names no
/// method any ordinary lookup asks for.
pub(crate) const BYTECODE_ONLY_KEY_PREFIX: &str = "<bytecode-only>";

/// The descriptor a bytecode-only body of a method with `descriptor` is
/// published under (the publication side; one allocation per compile).
pub(crate) fn bytecode_only_key_descriptor(descriptor: &str) -> Arc<str> {
    Arc::from(format!("{BYTECODE_ONLY_KEY_PREFIX}{descriptor}"))
}

/// Capacity of [`with_bytecode_only_key`]'s stack buffer.
const KEY_BUF: usize = 256;

/// Run `f` with the door-invisible descriptor of `descriptor`, built in a
/// stack buffer (the per-call lookup allocates nothing); a descriptor too long
/// for the buffer takes the allocating form.
fn with_bytecode_only_key<R>(descriptor: &str, f: impl FnOnce(&str) -> R) -> R {
    let prefix = BYTECODE_ONLY_KEY_PREFIX.as_bytes();
    let total = prefix.len() + descriptor.len();
    if total <= KEY_BUF {
        let mut buf = [0u8; KEY_BUF];
        buf[..prefix.len()].copy_from_slice(prefix);
        buf[prefix.len()..total].copy_from_slice(descriptor.as_bytes());
        if let Ok(key) = std::str::from_utf8(&buf[..total]) {
            return f(key);
        }
    }
    f(&bytecode_only_key_descriptor(descriptor))
}

/// How many doublings of the tier threshold the door compiles at: at the
/// threshold and at 2x, 4x and 8x of it. Past that a method whose compile
/// keeps being refused stays interpreted, as before.
const MAX_COMPILE_DOUBLINGS: u32 = 3;

/// Does the `n`-th counted call of a method with no bytecode-only body ask
/// for its compile? `None`: not yet; `Some(false)`: the attempts are spent.
fn compile_attempt_due(n: u32, threshold: u32) -> Option<bool> {
    let t = threshold.max(1);
    if n < t {
        return None;
    }
    if n % t != 0 {
        return if n / t > (1 << MAX_COMPILE_DOUBLINGS) {
            Some(false)
        } else {
            None
        };
    }
    let multiple = n / t;
    if !multiple.is_power_of_two() {
        return None;
    }
    Some(multiple <= (1 << MAX_COMPILE_DOUBLINGS))
}

/// Does `v` arrive in the compiled ABI as the parameter tagged `tag` expects?
/// The one-shot door encodes each word by its value, so a word of another
/// representation is refused (the interpreted body runs instead). The rule of
/// the lambda one-shot's `value_fits_param_tag`.
fn value_fits_param_tag(v: &Value, tag: u8) -> bool {
    match tag {
        b'I' | b'Z' | b'B' | b'C' | b'S' => matches!(v, Value::Int(_)),
        b'J' => matches!(v, Value::Long(_)),
        b'F' => matches!(v, Value::Float(_)),
        b'D' => matches!(v, Value::Double(_)),
        b'L' | b'[' => matches!(v, Value::Object(_)),
        _ => false,
    }
}

/// Does `code` (declared by `class`) invoke `class.method_name descriptor`
/// itself, by any invoke kind? A compile could bind such a call raw to the
/// body, bypassing the intrinsic for that call, so such a method is refused.
/// An undecodable instruction answers `true` (refuse).
fn body_invokes_itself(
    class: &crate::classloading::Class,
    code: &[u8],
    method_name: &str,
    descriptor: &str,
) -> bool {
    let cp = &class.constant_pool;
    let mut pc = 0;
    while pc < code.len() {
        let (instruction, next_pc) = match Instruction::decode(code, pc) {
            Ok(decoded) => decoded,
            Err(_) => return true,
        };
        let cp_idx = match &instruction {
            Instruction::Invokevirtual(index)
            | Instruction::Invokespecial(index)
            | Instruction::Invokestatic(index) => Some(*index),
            Instruction::Invokeinterface { index, .. } => Some(*index),
            _ => None,
        };
        if let Some(cp_idx) = cp_idx {
            let names = match cp.get(cp_idx) {
                Some(ConstantPoolEntry::MethodReference {
                    class_index,
                    name_and_type_index,
                })
                | Some(ConstantPoolEntry::InterfaceMethodReference {
                    class_index,
                    name_and_type_index,
                }) => Some((
                    cp.get_class_name(*class_index),
                    cp.get_name_and_type(*name_and_type_index),
                )),
                _ => None,
            };
            match names {
                Some((Some(owner), Some((name, desc)))) => {
                    if name == method_name && desc == descriptor && owner == &*class.name {
                        return true;
                    }
                }
                // A reference this scan cannot read: refuse.
                _ => return true,
            }
        }
        if next_pc <= pc {
            return true;
        }
        pc = next_pc;
    }
    false
}

/// What [`try_bytecode_only_body`] decided.
enum DoorOutcome {
    /// The compiled body finished the call (a value, `void`, or a throw).
    Done(MethodCallResult),
    /// Run the method interpreted, from these arguments when the compiled
    /// attempt wrote relocated references back (`None`: the caller's own).
    Interpret(Option<Vec<Value>>),
}

/// `execute(shared, thread, class_id, method_name, descriptor, args)` for the
/// bytecode-only doors (`vm_exec::invoke_virtual_bytecode_only`,
/// `invoke_special_bytecode_only_shared_impl`): `class_id` is the method's
/// declaring class and `args` carry the receiver first for an instance method.
/// With a door-invisible body published (see the module doc) the call enters
/// it; otherwise it is `execute`, exactly as before.
pub(crate) fn execute_bytecode_only(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    match try_bytecode_only_body(shared, thread, class_id, method_name, descriptor, args) {
        DoorOutcome::Done(result) => result,
        DoorOutcome::Interpret(fresh) => execute(
            shared,
            thread,
            class_id,
            method_name,
            descriptor,
            fresh.as_deref().unwrap_or(args),
        ),
    }
}

fn try_bytecode_only_body(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> DoorOutcome {
    // Off: `execute`, with no lookup at all (one load of the flag snapshot).
    if !cratonvm_types::flags().jit.bytecode_only_body {
        crate::jit::helpers::note_bytecode_only_outcome(crate::jit::helpers::BCO_SWITCH_OFF);
        return DoorOutcome::Interpret(None);
    }
    // `execute`'s own no-compile arms: no JIT, a virtual thread (compiled code
    // carries none of the interpreter's unmount points), a JVMTI agent that
    // needs interpreted execution.
    if !shared.config.jit_enabled
        || crate::runtime::env_cache::disable_jit()
        || matches!(thread.kind, crate::threading::ThreadKind::Virtual)
        || super::jit_bridge::jvmti_requires_interpreter(shared)
    {
        crate::jit::helpers::note_bytecode_only_outcome(crate::jit::helpers::BCO_NO_JIT);
        return DoorOutcome::Interpret(None);
    }
    let Some(class_name) = shared
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .map(|c| Arc::clone(&c.name))
    else {
        return DoorOutcome::Interpret(None);
    };
    let body = with_bytecode_only_key(descriptor, |key| {
        shared
            .jit
            .jit_cache
            .read()
            .get(&class_name, method_name, key, class_id)
    });
    match body {
        Some(body) => enter_body(shared, thread, cratonvm_jit::RetainedCode::new(body), args),
        None => consider_compile(
            shared,
            thread,
            class_id,
            &class_name,
            method_name,
            descriptor,
            args,
        ),
    }
}

/// Enter the published bytecode-only `body` with `args` through the one-shot
/// door, under `execute`'s nested-Java acknowledgement (a native that runs Java
/// is not "in native" for a collection or a debugger while it does).
fn enter_body(
    shared: &SharedVm,
    thread: &mut JvmThread,
    body: cratonvm_jit::RetainedCode,
    args: &[Value],
) -> DoorOutcome {
    use crate::jit::helpers::{
        note_bytecode_only_outcome, BCO_BODY_NOT_ENTERABLE, BCO_ENTERED_DECLINED,
        BCO_ENTERED_RETURNED, BCO_ENTERED_THREW,
    };
    let Some(source) = body.compiled_source().cloned() else {
        note_bytecode_only_outcome(BCO_BODY_NOT_ENTERABLE);
        return DoorOutcome::Interpret(None);
    };
    let receiver = usize::from(!source.is_static);
    let n = args.len();
    let facts = source.descriptor_facts();
    if source.is_synchronized
        || n > super::jit_bridge::JIT_ABI_MAX_JAVA_ARGS
        || facts.param_tags_overflow
        || n != facts.param_tag_len as usize + receiver
        || (receiver == 1 && !matches!(args.first(), Some(Value::Object(Some(_)))))
        || !args[receiver..]
            .iter()
            .zip(&facts.param_tags[..facts.param_tag_len as usize])
            .all(|(v, &tag)| value_fits_param_tag(v, tag))
    {
        note_bytecode_only_outcome(BCO_BODY_NOT_ENTERABLE);
        return DoorOutcome::Interpret(None);
    }
    let mut words = [Value::Uninitialized; super::jit_bridge::JIT_ABI_MAX_JAVA_ARGS];
    words[..n].copy_from_slice(args);
    let outcome = {
        let _nested_java_ack = NestedJavaAck::enter(thread);
        super::jit_bridge::execute_jit_call_oneshot(shared, thread, &body, &source, &mut words[..n])
    };
    drop(body);
    match outcome {
        Ok(Some(value)) => {
            note_bytecode_only_outcome(BCO_ENTERED_RETURNED);
            DoorOutcome::Done(Ok(value))
        }
        Err(e) => {
            note_bytecode_only_outcome(BCO_ENTERED_THREW);
            DoorOutcome::Done(Err(e))
        }
        Ok(None) => {
            // Declined: the one-shot door wrote the references back at their
            // current addresses; the interpreted re-run starts from them.
            note_bytecode_only_outcome(BCO_ENTERED_DECLINED);
            DoorOutcome::Interpret(Some(words[..n].to_vec()))
        }
    }
}

/// The miss path: admit the method, count the call, and at a due count compile
/// its door-invisible body on this thread. This call runs interpreted either
/// way; the next one enters a published body.
fn consider_compile(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> DoorOutcome {
    use crate::jit::helpers::{
        note_bytecode_only_outcome, BCO_ATTEMPTS_SPENT, BCO_COMPILE_PUBLISHED, BCO_COMPILE_REFUSED,
        BCO_COUNTING, BCO_NOT_INTRINSIC, BCO_SHAPE_REFUSED,
    };
    // Only a reviewed `Intrinsic` whose native runs: anything else is
    // `execute`'s, unchanged (no registration: `execute` compiles the method
    // itself; a `Bridge` / `SyntheticStub` is `resolve_dispatch`'s to
    // arbitrate; a body-compilable intrinsic is compiled by `execute`).
    if shared
        .natives
        .native_methods
        .kind_of(class_name, method_name, descriptor)
        != Some(cratonvm_native_api::NativeKind::Intrinsic)
        || !registered_native_will_run(shared, class_name, method_name, descriptor)
    {
        note_bytecode_only_outcome(BCO_NOT_INTRINSIC);
        return DoorOutcome::Interpret(None);
    }
    let count = with_bytecode_only_key(descriptor, |key| {
        shared
            .jit
            .profile_store
            .increment_invocation(cratonvm_jit_api::invoc_key_parts(
                class_id.as_u32(),
                method_name,
                key,
            ))
    });
    match compile_attempt_due(count, crate::runtime::env_cache::jit_invocation_threshold()) {
        None => {
            note_bytecode_only_outcome(BCO_COUNTING);
            return DoorOutcome::Interpret(None);
        }
        Some(false) => {
            note_bytecode_only_outcome(BCO_ATTEMPTS_SPENT);
            return DoorOutcome::Interpret(None);
        }
        Some(true) => {}
    }
    // A `<clinit>` never takes a compile door (`execute`'s rule): it runs once.
    let admitted = method_name != "<clinit>" && {
        let cm = shared.classes.class_manager.read();
        cm.get_class(class_id).is_some_and(|class| {
            class
                .methods
                .iter()
                .find(|m| &*m.name == method_name && &*m.descriptor == descriptor)
                .is_some_and(|m| {
                    !m.is_synchronized()
                        && !m.is_abstract()
                        && m.code().is_some_and(|code| {
                            !body_invokes_itself(class, &code.code, method_name, descriptor)
                        })
                })
        })
    };
    if !admitted {
        note_bytecode_only_outcome(BCO_SHAPE_REFUSED);
        return DoorOutcome::Interpret(None);
    }
    // The compile can reach a safepoint (a class load, an initializer): the
    // reference arguments are pinned across it and read back after, as
    // `invoke_special_bytecode_only_shared_impl` pins its own.
    let pin_base = thread.native_pin_roots.len();
    for a in args {
        if let Value::Object(Some(o)) = a {
            thread.native_pin_roots.push(*o);
        }
    }
    let published = {
        let _nested_java_ack = NestedJavaAck::enter(thread);
        super::jit_bridge::try_jit_compile_bytecode_only_body(
            shared,
            class_id,
            class_name,
            method_name,
            descriptor,
        )
    };
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] bytecode-only body {} {class_name}.{method_name}{descriptor} (call {count})",
            if published.is_some() {
                "published"
            } else {
                "refused"
            },
        );
    }
    note_bytecode_only_outcome(if published.is_some() {
        BCO_COMPILE_PUBLISHED
    } else {
        BCO_COMPILE_REFUSED
    });
    // `RetainedCode`: this may be the last owner of a body withdrawn since.
    drop(published.map(cratonvm_jit::RetainedCode::new));
    let mut fresh: Option<Vec<Value>> = None;
    let mut k = 0;
    for (i, a) in args.iter().enumerate() {
        if let Value::Object(Some(o)) = a {
            if let Some(&pinned) = thread.native_pin_roots.get(pin_base + k) {
                if pinned != *o {
                    fresh.get_or_insert_with(|| args.to_vec())[i] = Value::Object(Some(pinned));
                }
            }
            k += 1;
        }
    }
    thread.native_pin_roots.truncate(pin_base);
    DoorOutcome::Interpret(fresh)
}

#[cfg(test)]
mod reflect4_bytecode_only_body_tests {
    use super::{
        compile_attempt_due, value_fits_param_tag, with_bytecode_only_key, BYTECODE_ONLY_KEY_PREFIX,
    };
    use crate::types::Value;

    #[test]
    fn the_door_invisible_descriptor_is_the_prefix_and_the_descriptor() {
        let desc = "(Ljava/lang/String;Z)Ljava/lang/Class;";
        let built = with_bytecode_only_key(desc, |k| k.to_string());
        assert_eq!(built, format!("{BYTECODE_ONLY_KEY_PREFIX}{desc}"));
        assert_eq!(&*super::bytecode_only_key_descriptor(desc), built.as_str());
        // No method descriptor can equal it: those start with '('.
        assert!(!built.starts_with('('));
        // A descriptor wider than the stack buffer takes the allocating form.
        let wide = format!("({})V", "Ljava/lang/Object;".repeat(20));
        let built = with_bytecode_only_key(&wide, |k| k.to_string());
        assert_eq!(built, format!("{BYTECODE_ONLY_KEY_PREFIX}{wide}"));
    }

    #[test]
    fn the_door_compiles_at_the_threshold_and_three_doublings_only() {
        assert_eq!(compile_attempt_due(0, 100), None);
        assert_eq!(compile_attempt_due(99, 100), None);
        assert_eq!(compile_attempt_due(100, 100), Some(true));
        assert_eq!(compile_attempt_due(150, 100), None);
        assert_eq!(compile_attempt_due(200, 100), Some(true));
        assert_eq!(compile_attempt_due(300, 100), None);
        assert_eq!(compile_attempt_due(400, 100), Some(true));
        assert_eq!(compile_attempt_due(800, 100), Some(true));
        assert_eq!(compile_attempt_due(1600, 100), Some(false));
        assert_eq!(compile_attempt_due(950, 100), Some(false));
        // A zero threshold is read as one.
        assert_eq!(compile_attempt_due(1, 0), Some(true));
    }

    #[test]
    fn only_the_verbatim_representation_fits_a_parameter() {
        assert!(value_fits_param_tag(&Value::Int(1), b'Z'));
        assert!(!value_fits_param_tag(&Value::Long(1), b'I'));
        assert!(value_fits_param_tag(&Value::Object(None), b'L'));
        assert!(!value_fits_param_tag(&Value::Int(0), b'L'));
        assert!(!value_fits_param_tag(&Value::Uninitialized, b'['));
    }
}
