// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JVMTI capabilities negotiation.

use super::JvmtiError;

/// The set of JVMTI capabilities that an agent can request.
///
/// Each field corresponds to a single capability flag as defined in the
/// JVMTI specification. Agents request capabilities at startup and the
/// VM grants them based on what it can support.
#[derive(Debug, Clone, Default)]
pub struct JvmtiCapabilities {
    pub can_tag_objects: bool,
    pub can_generate_field_modification_events: bool,
    pub can_generate_field_access_events: bool,
    pub can_get_bytecodes: bool,
    pub can_get_synthetic_attribute: bool,
    pub can_get_owned_monitor_info: bool,
    pub can_get_current_contended_monitor: bool,
    pub can_get_monitor_info: bool,
    pub can_pop_frame: bool,
    pub can_redefine_classes: bool,
    pub can_signal_thread: bool,
    pub can_get_source_file_name: bool,
    pub can_get_line_numbers: bool,
    pub can_get_source_debug_extension: bool,
    pub can_access_local_variables: bool,
    pub can_maintain_original_method_order: bool,
    pub can_generate_single_step_events: bool,
    pub can_generate_exception_events: bool,
    pub can_generate_frame_pop_events: bool,
    pub can_generate_breakpoint_events: bool,
    pub can_suspend: bool,
    pub can_generate_method_entry_events: bool,
    pub can_generate_method_exit_events: bool,
    pub can_generate_all_class_hook_events: bool,
    pub can_generate_compiled_method_load_events: bool,
    pub can_generate_monitor_events: bool,
    pub can_generate_vm_object_alloc_events: bool,
    pub can_generate_garbage_collection_events: bool,
    pub can_get_current_thread_cpu_time: bool,
    pub can_force_early_return: bool,
}

impl JvmtiCapabilities {
    /// Returns capabilities representing what this VM implementation can
    /// potentially support for a real, `-agentpath:`-attached native agent.
    ///
    /// obsaudit D14 (2026-07-26): this used to set every field `true`,
    /// including a dozen `can_generate_*_events` capabilities for event
    /// kinds this env's `EventManager` would then never actually fire —
    /// `AddCapabilities` would succeed, `SetEventNotificationMode` would
    /// succeed, and the agent would simply never see the event, with
    /// nothing telling it why. That is the same failure shape D2 documents
    /// for `can_access_local_variables` on the *other* (synthetic-facing)
    /// `JvmtiCapabilities` in `runtime/jvmti.rs`.
    ///
    /// `runtime::jvmti::JvmtiEventManager`'s bridge (see
    /// `install_real_agent_env_bridge` in `runtime/jvmti.rs`) now forwards 9
    /// event kinds to this env: VMInit, VMDeath, ThreadStart, ThreadEnd,
    /// ClassLoad, ClassPrepare, GarbageCollectionStart/Finish, ObjectFree.
    /// None of those correspond to a capability flag below (VM/thread/class
    /// lifecycle and GC events are unconditionally delivered once an agent
    /// is attached — the JVMTI spec does not gate them behind
    /// `can_generate_*_events`). The capabilities that *are* gated behind a
    /// `can_generate_*_events` flag — method entry/exit, single-step,
    /// breakpoint, frame pop, field access/modification, class-file-load
    /// hook, compiled-method-load, VM-object-alloc, and monitor events — are
    /// per-bytecode/per-invocation or per-contended-lock hot paths the
    /// bridge deliberately does not cover (see the bridge's own doc comment
    /// for why), so they are `false` here: an honest "not available" is
    /// strictly better than a silent "granted, delivers nothing".
    pub fn potential() -> Self {
        Self {
            can_tag_objects: true,
            can_generate_field_modification_events: false,
            can_generate_field_access_events: false,
            can_get_bytecodes: true,
            can_get_synthetic_attribute: true,
            can_get_owned_monitor_info: true,
            can_get_current_contended_monitor: true,
            can_get_monitor_info: true,
            can_pop_frame: true,
            can_redefine_classes: true,
            can_signal_thread: true,
            can_get_source_file_name: true,
            can_get_line_numbers: true,
            can_get_source_debug_extension: true,
            can_access_local_variables: true,
            can_maintain_original_method_order: true,
            can_generate_single_step_events: false,
            can_generate_exception_events: false,
            can_generate_frame_pop_events: false,
            can_generate_breakpoint_events: false,
            can_suspend: true,
            can_generate_method_entry_events: false,
            can_generate_method_exit_events: false,
            can_generate_all_class_hook_events: false,
            can_generate_compiled_method_load_events: false,
            can_generate_monitor_events: false,
            can_generate_vm_object_alloc_events: false,
            can_generate_garbage_collection_events: true,
            can_get_current_thread_cpu_time: true,
            can_force_early_return: true,
        }
    }

    /// Attempt to add (grant) the requested capabilities.
    ///
    /// Each requested capability (set to `true`) is checked against the
    /// potential capabilities. If a capability is requested but not
    /// potentially available, an error is returned. Otherwise the
    /// capability is enabled on `self`.
    pub fn add_capabilities(&mut self, requested: &JvmtiCapabilities) -> Result<(), JvmtiError> {
        let potential = Self::potential();
        macro_rules! check_and_set {
            ($field:ident) => {
                if requested.$field {
                    if !potential.$field {
                        return Err(JvmtiError::InvalidCapability);
                    }
                    self.$field = true;
                }
            };
        }

        check_and_set!(can_tag_objects);
        check_and_set!(can_generate_field_modification_events);
        check_and_set!(can_generate_field_access_events);
        check_and_set!(can_get_bytecodes);
        check_and_set!(can_get_synthetic_attribute);
        check_and_set!(can_get_owned_monitor_info);
        check_and_set!(can_get_current_contended_monitor);
        check_and_set!(can_get_monitor_info);
        check_and_set!(can_pop_frame);
        check_and_set!(can_redefine_classes);
        check_and_set!(can_signal_thread);
        check_and_set!(can_get_source_file_name);
        check_and_set!(can_get_line_numbers);
        check_and_set!(can_get_source_debug_extension);
        check_and_set!(can_access_local_variables);
        check_and_set!(can_maintain_original_method_order);
        check_and_set!(can_generate_single_step_events);
        check_and_set!(can_generate_exception_events);
        check_and_set!(can_generate_frame_pop_events);
        check_and_set!(can_generate_breakpoint_events);
        check_and_set!(can_suspend);
        check_and_set!(can_generate_method_entry_events);
        check_and_set!(can_generate_method_exit_events);
        check_and_set!(can_generate_all_class_hook_events);
        check_and_set!(can_generate_compiled_method_load_events);
        check_and_set!(can_generate_monitor_events);
        check_and_set!(can_generate_vm_object_alloc_events);
        check_and_set!(can_generate_garbage_collection_events);
        check_and_set!(can_get_current_thread_cpu_time);
        check_and_set!(can_force_early_return);

        Ok(())
    }

    /// Check whether a named capability is currently enabled.
    pub fn has_capability(&self, cap: &str) -> bool {
        match cap {
            "can_tag_objects" => self.can_tag_objects,
            "can_generate_field_modification_events" => self.can_generate_field_modification_events,
            "can_generate_field_access_events" => self.can_generate_field_access_events,
            "can_get_bytecodes" => self.can_get_bytecodes,
            "can_get_synthetic_attribute" => self.can_get_synthetic_attribute,
            "can_get_owned_monitor_info" => self.can_get_owned_monitor_info,
            "can_get_current_contended_monitor" => self.can_get_current_contended_monitor,
            "can_get_monitor_info" => self.can_get_monitor_info,
            "can_pop_frame" => self.can_pop_frame,
            "can_redefine_classes" => self.can_redefine_classes,
            "can_signal_thread" => self.can_signal_thread,
            "can_get_source_file_name" => self.can_get_source_file_name,
            "can_get_line_numbers" => self.can_get_line_numbers,
            "can_get_source_debug_extension" => self.can_get_source_debug_extension,
            "can_access_local_variables" => self.can_access_local_variables,
            "can_maintain_original_method_order" => self.can_maintain_original_method_order,
            "can_generate_single_step_events" => self.can_generate_single_step_events,
            "can_generate_exception_events" => self.can_generate_exception_events,
            "can_generate_frame_pop_events" => self.can_generate_frame_pop_events,
            "can_generate_breakpoint_events" => self.can_generate_breakpoint_events,
            "can_suspend" => self.can_suspend,
            "can_generate_method_entry_events" => self.can_generate_method_entry_events,
            "can_generate_method_exit_events" => self.can_generate_method_exit_events,
            "can_generate_all_class_hook_events" => self.can_generate_all_class_hook_events,
            "can_generate_compiled_method_load_events" => {
                self.can_generate_compiled_method_load_events
            }
            "can_generate_monitor_events" => self.can_generate_monitor_events,
            "can_generate_vm_object_alloc_events" => self.can_generate_vm_object_alloc_events,
            "can_generate_garbage_collection_events" => self.can_generate_garbage_collection_events,
            "can_get_current_thread_cpu_time" => self.can_get_current_thread_cpu_time,
            "can_force_early_return" => self.can_force_early_return,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_capabilities_all_false() {
        let caps = JvmtiCapabilities::default();
        assert!(!caps.can_tag_objects);
        assert!(!caps.can_get_bytecodes);
        assert!(!caps.can_generate_breakpoint_events);
        assert!(!caps.can_generate_garbage_collection_events);
        assert!(!caps.can_redefine_classes);
    }

    #[test]
    fn test_potential_capabilities() {
        let pot = JvmtiCapabilities::potential();
        assert!(pot.can_tag_objects);
        assert!(pot.can_get_bytecodes);
        assert!(pot.can_generate_garbage_collection_events);
        assert!(pot.can_pop_frame);
        assert!(pot.can_redefine_classes);
        assert!(pot.can_force_early_return);
    }

    /// obsaudit D14: capabilities whose events the real-agent bridge does
    /// not (and, for hot-path reasons, should not) forward must stay
    /// unavailable, or `AddCapabilities` would grant something that then
    /// silently never fires. See the doc comment on `potential()`.
    #[test]
    fn test_potential_capabilities_excludes_unbridged_events() {
        let pot = JvmtiCapabilities::potential();
        assert!(!pot.can_generate_method_entry_events);
        assert!(!pot.can_generate_method_exit_events);
        assert!(!pot.can_generate_single_step_events);
        assert!(!pot.can_generate_breakpoint_events);
        assert!(!pot.can_generate_frame_pop_events);
        assert!(!pot.can_generate_field_access_events);
        assert!(!pot.can_generate_field_modification_events);
        assert!(!pot.can_generate_all_class_hook_events);
        assert!(!pot.can_generate_compiled_method_load_events);
        assert!(!pot.can_generate_vm_object_alloc_events);
        assert!(!pot.can_generate_monitor_events);
        assert!(!pot.can_generate_exception_events);
    }

    #[test]
    fn test_add_capabilities_success() {
        let mut caps = JvmtiCapabilities::default();
        let mut requested = JvmtiCapabilities::default();
        requested.can_tag_objects = true;
        requested.can_generate_garbage_collection_events = true;

        assert!(caps.add_capabilities(&requested).is_ok());
        assert!(caps.can_tag_objects);
        assert!(caps.can_generate_garbage_collection_events);
        assert!(!caps.can_get_bytecodes); // not requested
    }

    /// obsaudit D14: requesting a capability whose events are not bridged
    /// to a real agent must fail, not silently succeed.
    #[test]
    fn test_add_capabilities_rejects_unbridged_events() {
        let mut caps = JvmtiCapabilities::default();
        let mut requested = JvmtiCapabilities::default();
        requested.can_generate_breakpoint_events = true;

        assert_eq!(
            caps.add_capabilities(&requested),
            Err(JvmtiError::InvalidCapability)
        );
        assert!(!caps.can_generate_breakpoint_events);
    }

    #[test]
    fn test_add_capabilities_idempotent() {
        let mut caps = JvmtiCapabilities::default();
        let mut requested = JvmtiCapabilities::default();
        requested.can_redefine_classes = true;

        // First request should succeed
        assert!(caps.add_capabilities(&requested).is_ok());
        assert!(caps.can_redefine_classes);

        // Second request (already enabled) should also succeed
        assert!(caps.add_capabilities(&requested).is_ok());
        assert!(caps.can_redefine_classes);
    }

    #[test]
    fn test_has_capability_by_name() {
        let mut caps = JvmtiCapabilities::default();
        assert!(!caps.has_capability("can_tag_objects"));

        caps.can_tag_objects = true;
        assert!(caps.has_capability("can_tag_objects"));

        // Unknown capability name.
        assert!(!caps.has_capability("can_fly"));
    }
}
