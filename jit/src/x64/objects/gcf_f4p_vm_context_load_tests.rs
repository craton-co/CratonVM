// gch h2/fj (d1a item 1): every site converted; the one left is the
// loader's own load in `emit_load_vm_context`.
const BASELINE: usize = 1;

fn raw_loads(src: &str) -> usize {
    src.lines()
        .filter(|l| {
            l.contains(concat!("emit_load_", "local("))
                && l.contains(concat!("self.heap_", "local_offset"))
        })
        .count()
}

#[test]
fn gcf_f4p_raw_vm_context_loads_do_not_grow() {
    let n = raw_loads(include_str!("../deopt_stubs.rs"))
        + raw_loads(include_str!("../frames.rs"))
        + raw_loads(include_str!("../inlining.rs"))
        + raw_loads(include_str!("../inlining/spliced_sites.rs"))
        + raw_loads(include_str!("../objects.rs"))
        + raw_loads(include_str!("../op_array.rs"))
        + raw_loads(include_str!("../op_field.rs"))
        + raw_loads(concat!(include_str!("../op_invoke/mod.rs"), "\n", include_str!("../op_invoke/sp_tailcall_keeps_vm_helper_frame.rs"), "\n", include_str!("../op_invoke/sp_byte_array_view_body.rs"), "\n", include_str!("../op_invoke/walk_invoke_instance.rs"), "\n", include_str!("../op_invoke/arraylist_access_prefix.rs"), "\n", include_str!("../op_invoke/indy_site_is_bridged.rs"), "\n", include_str!("../op_invoke/sb_to_string_latin1_body.rs")))
        + raw_loads(include_str!("../op_object.rs"));
    assert!(
        n <= BASELINE,
        "{n} raw VM-context loads (baseline {BASELINE}): \
         load the VM context through `emit_load_vm_context`, which refuses an unreserved slot"
    );
}
