//! gci i47w judge jw1 (item A of
//! `gci-i23pk-park-permit-residuals-windows-and-stand-ins-FIXED-20261017`): on
//! Windows an elapsed or negative deadline returns before the interrupt
//! test and before the permit is taken, as HotSpot 25's os_windows.cpp
//! `Parker::park` does; the Linux order (permit first) is unchanged.
const SRC: &str = concat!(
    include_str!("../vm_exec/mod.rs"),
    "\n",
    include_str!("../vm_exec/method.rs"),
    "\n",
    include_str!("../vm_exec/report_pad_access.rs"),
    "\n",
    include_str!("../vm_exec/diagnose_sync_on_value_based_class.rs"),
    "\n",
    include_str!("../vm_exec/thread_interrupted_field_slot_of.rs"),
    "\n",
    include_str!("../vm_exec/clear_breakpoints_of_redefined_class.rs"),
    "\n",
    include_str!("../vm_exec/load_class.rs"),
    "\n",
    include_str!("../vm_exec/native_new_initializes_class_first.rs"),
    "\n",
    include_str!("../vm_exec/new_object.rs"),
    "\n",
    include_str!("../vm_exec/gen_referent_read_keep_alive.rs"),
    "\n",
    include_str!("../vm_exec/append_native_standin_frames.rs"),
    "\n",
    include_str!("../vm_exec/unloaded_native_libraries.rs"),
    "\n",
    include_str!("../vm_exec/extract_annotations_from_attributes.rs"),
    "\n",
    include_str!("../vm_exec/invoke_static_settling_with_first.rs"),
    "\n",
    include_str!("../vm_exec/invoke_special_shared.rs"),
    "\n",
    include_str!("../vm_exec/proxy_resolve_declaring_method.rs"),
    "\n",
    include_str!("../vm_exec/invoke_on_class_shared_inner.rs"),
    "\n",
    include_str!("../vm_exec/jni_table_native_call.rs")
);

fn park_body() -> &'static str {
    let head = "fn park_with_state_inner(";
    let start = SRC.find(head).expect("park_with_state_until");
    let len = SRC[start..]
        .find("\n    \u{7d}\n")
        .expect("end of park_with_state_until");
    &SRC[start..start + len]
}

#[test]
fn gci_i47w_jw1_windows_at_once_park_keeps_the_permit_before_the_interrupt_test() {
    let park = park_body();
    let win = park
        .find("&& reported_timeout == Some(std::time::Duration::ZERO)")
        .expect("the Windows at-once return");
    let gate = park[..win]
        .rfind("#[cfg(windows)]")
        .expect("cfg(windows) gate");
    let interrupted = park
        .find("// JDK spec: if interrupted, park returns immediately")
        .expect("interrupt test");
    let permit = park.find("let had_permit = ").expect("permit take");
    assert!(gate < win && win < interrupted && interrupted < permit);
    let arm = &park[win..interrupted];
    assert!(arm.contains("!vm_scheduled_virtual(self.thread)"));
    assert!(arm.contains("self.commit_thread_park_event(span);"));
    assert!(arm.contains("return;"));
    assert!(!arm.contains("take_permit"), "the permit stays");
}
