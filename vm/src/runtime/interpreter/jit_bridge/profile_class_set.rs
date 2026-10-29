//! j27 wave 5, lane PROXY (`CRATONVM_JIT_IR_CHA_INTERFACE_CLASS_SET_SPLICE`,
//! opt-in): the profile-receiver resolver's answer for an interface site's
//! exact class set, carried to the optimizing tier's planner on the resolved
//! site itself (`cratonvm_jit::InlineSite::cha_class_set`; the guard's
//! argument is in `cratonvm_jit` `ir::cha_class_set`, the list's in
//! `crate::vm::realms::cha_class_set`).

use super::*;

/// After [`resolve_ir_profile_receiver_inline_site`] resolved `declaring`'s
/// body for a receiver of exactly `receiver` at the site `cp_class.name
/// descriptor`: when the switch is on and `cp_class` is an interface, every
/// loaded class whose exact objects select the same body there (the receiver
/// first). Each listed class is marked as copied into the open compile, as
/// the resolver marks its receiver: the dependency a redefinition or an
/// unload withdraws the artifact by. Empty for a set of fewer than two
/// classes, and with the switch off.
pub(super) fn profile_interface_class_set(
    shared: &SharedVm,
    requesting_class_id: ClassId,
    receiver: ClassId,
    cp_class: &str,
    name: &str,
    descriptor: &str,
    declaring: u32,
) -> Vec<u32> {
    if declaring == 0 || !cratonvm_jit::ir::ir_cha_interface_class_set_enabled() {
        return Vec::new();
    }
    let site_interface = {
        let cm = shared.classes.class_manager.read();
        jit_known_class(shared, &cm, cp_class, requesting_class_id)
        // `cm` drops here: the analysis takes the lock itself.
    };
    let Some(site_interface) = site_interface else {
        return Vec::new();
    };
    let set = shared.classes.cha_interface_class_set(
        site_interface,
        ClassId::new(declaring),
        receiver,
        name,
        descriptor,
    );
    if set.len() < 2 {
        return Vec::new();
    }
    {
        // Under the guard, as the exact-receiver resolvers mark theirs.
        let _cm = shared.classes.class_manager.read();
        for &class_id in &set {
            shared.jit.jit_cache.note_bytecode_copied(ClassId::new(class_id));
        }
    }
    set
}
