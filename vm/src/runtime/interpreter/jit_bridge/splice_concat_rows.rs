//! j27 wave 4, lane SPLICECAT (`CRATONVM_JIT_SPLICE_CONCAT_BRIDGE`, opt-in):
//! the inline resolver's admission of a spliced body's argument-taking
//! `StringConcatFactory` site as an `ir_indy_rows` row
//! (`cratonvm_jit::splice_concat_bridge` has the design).
//!
//! The row names the SAME site the callee's own compiled body calls: the one
//! `interned_jit_indy_bridge_site` interns for the instruction (keyed by the
//! callee's own method and bci). A javac concat site is built from the
//! constant pool alone (`invokedynamic::make_jit_string_concat_site_from_parts`:
//! recipe, constants, call type; no linkage, no Java run), so admitting it
//! here moves no class initialization or bootstrap side effect. Only a site
//! whose kind tag is `JIT_INDY_SITE_CONCAT` qualifies: a lambda, a linked
//! generic site or a guarded leaf keeps the resolver's existing answer.

use super::*;

/// The `ir_indy_rows` row `(callee pc, argument entries, b'L', site)` of the
/// spliced concat instruction `(callee method, callee descriptor, pc)` whose
/// call type is `descriptor`, or `None` (the caller's other admissions, or
/// its refusal, then apply). Arguments are counted one per value, as the
/// method-own door counts them (`count_param_slots`), which is what the
/// builder pops and the bridge's `arg_types` decode.
pub(super) fn splice_concat_row(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    class: &crate::classloading::Class,
    class_id: ClassId,
    cp_idx: u16,
    descriptor: &str,
    instruction: (&str, &str, usize),
) -> Option<(usize, usize, u8, usize)> {
    if !cratonvm_jit::splice_concat_bridge::splice_concat_bridge_enabled() {
        return None;
    }
    let (args, ret) = concat_row_shape(descriptor)?;
    let site = interned_jit_indy_bridge_site(shared, cm, class, class_id, cp_idx, Some(instruction));
    // SAFETY: `site` is 0 or a process-lived site from the intern table
    // (`make_jit_indy_bridge_site_from_parts` / the linked-site builder).
    let kind = unsafe { crate::runtime::invokedynamic::jit_indy_site_kind(site) };
    if kind != crate::runtime::invokedynamic::JIT_INDY_SITE_CONCAT {
        return None;
    }
    let (method, method_desc, pc) = instruction;
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] splice-concat-row ADMITTED {}.{}{} pc={pc} args={args} site={site:#x}",
            class.name, method, method_desc,
        );
    }
    Some((pc, args, ret, site))
}

/// `(argument entries, return tag)` of a concat call type a spliced row may
/// take: at least one argument (a `()L` site keeps the constant-word path)
/// and a reference result.
fn concat_row_shape(descriptor: &str) -> Option<(usize, u8)> {
    let args = crate::jit::count_param_slots(descriptor);
    let ret = crate::jit::return_type(descriptor);
    (args > 0 && ret == b'L').then_some((args, ret))
}

#[cfg(test)]
mod tests {
    use super::concat_row_shape;

    #[test]
    fn splicecat_concat_row_shapes_count_one_entry_per_value() {
        assert_eq!(concat_row_shape("(I)Ljava/lang/String;"), Some((1, b'L')));
        assert_eq!(
            concat_row_shape("(CFDCLjava/lang/Object;J)Ljava/lang/String;"),
            Some((6, b'L'))
        );
        assert_eq!(concat_row_shape("()Ljava/lang/String;"), None, "the constant-word path");
        assert_eq!(concat_row_shape("(I)I"), None, "never a concat call type");
    }
}
