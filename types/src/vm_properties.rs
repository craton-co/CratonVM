// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The keys and flags of HotSpot's VM property list
//! (`Arguments::system_properties()`), in one table (interpreter round i4
//! wave 95, lane L1; stage 1 of
//! `docs/internal/proposals/interpreter/i61-L1-proposal-one-vm-property-list-20261025.md`).
//!
//! HotSpot keeps one list of VM properties, each with three flags: readable
//! (not internal), writeable (by a JVMTI agent in the OnLoad phase) and
//! internal. This VM had the key lists as private constants of the C JVMTI
//! table (`vm/src/jvmti/native_env.rs`) beside the literals of
//! `native-builtins/src/system_bootstrap.rs::native_vm_properties`; this
//! module is where both can read them (`native-builtins` cannot see `vm`).
//! The values stay with their owners (`vm::vm_init::vm_identity_properties`
//! and the `initial_*` functions) until the later stages.

/// One entry of the VM's own property list (besides the `-D` ones).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VmProperty {
    /// The property's key.
    pub key: &'static str,
    /// Writeable by `SetSystemProperty` in the OnLoad phase
    /// (`Arguments::init_system_properties`: the four path properties and
    /// `java.vm.info`). MEASURED: `java.vm.version` is not writeable,
    /// `java.library.path` is.
    pub writeable: bool,
    /// Set once the heap is made, so absent in the OnLoad phase
    /// (`java.vm.compressedOopsMode`).
    pub after_heap: bool,
}

/// The readable entries of HotSpot's VM property list besides the `-D` ones,
/// in its order, as JDK 25 lists them in the live phase (MEASURED:
/// `GetSystemProperties` on HotSpot 25.0.3 answers exactly these,
/// `sun.java.command` and `sun.java.launcher`, which the launcher passes as
/// `-D` properties, and the command line's `-D` properties;
/// `tools/probes/interp/L1/L1W61JvmtiAgentStartup.java`, its `props` row).
/// `user.dir`, `os.name`, `java.version` and the rest of
/// `System.getProperties()` come from the class library, not the VM.
pub const VM_PROPERTIES: [VmProperty; 13] = [
    entry("java.vm.specification.name", false),
    entry("java.vm.specification.vendor", false),
    entry("java.vm.specification.version", false),
    entry("java.vm.name", false),
    entry("java.vm.vendor", false),
    entry("java.vm.version", false),
    entry("java.vm.info", true),
    entry("jdk.debug", false),
    entry("java.home", true),
    entry("java.class.path", true),
    entry("java.library.path", true),
    entry("sun.boot.library.path", true),
    VmProperty {
        key: "java.vm.compressedOopsMode",
        writeable: false,
        after_heap: true,
    },
];

/// The fixed values of the VM's identity keys (interpreter round i6 wave 118,
/// lane L1; stage 2 of the i61-L1 property-list proposal): every
/// [`VM_PROPERTIES`] key whose value does not depend on the launch, in the
/// list's order. `java.vm.info` (the execution mode, `vm_init::vm_info_mode_list`)
/// and the four paths are the launch's. The one copy of these strings:
/// `vm::vm_init::vm_identity_properties` (the VM's property map, which
/// `System.getProperty` and JVMTI `GetSystemProperty` answer from) and
/// `native-builtins`' `SystemProps$Raw.vmProperties()` and management
/// beans read them here.
pub const VM_IDENTITY: [(&str, &str); 7] = [
    ("java.vm.specification.name", "Java Virtual Machine Specification"),
    ("java.vm.specification.vendor", "Oracle Corporation"),
    ("java.vm.specification.version", "25"),
    ("java.vm.name", "cratonvm"),
    ("java.vm.vendor", "cratonvm"),
    ("java.vm.version", "25.0.1+8"),
    ("jdk.debug", "release"),
];

/// The fixed value of identity key `key` ([`VM_IDENTITY`]), if it is one.
pub fn vm_identity_value(key: &str) -> Option<&'static str> {
    VM_IDENTITY.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

const fn entry(key: &'static str, writeable: bool) -> VmProperty {
    VmProperty {
        key,
        writeable,
        after_heap: false,
    }
}

/// The keys of [`VM_PROPERTIES`], in order.
pub fn vm_property_keys() -> impl Iterator<Item = &'static str> {
    VM_PROPERTIES.iter().map(|p| p.key)
}

/// Is `key` one of the VM's own properties that `SetSystemProperty` may
/// write in the OnLoad phase? (A non-internal `-D` property is writeable
/// too; the caller knows which keys came from the command line.)
pub fn is_writeable_vm_property(key: &str) -> bool {
    VM_PROPERTIES.iter().any(|p| p.writeable && p.key == key)
}

/// Is `key` a `-D` property HotSpot keeps internal (unreadable through
/// JVMTI): `-Xbootclasspath/a:`'s carrier and the module system's.
pub fn is_internal_property(key: &str) -> bool {
    key == "jdk.boot.class.path.append" || key.starts_with("jdk.module.")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The readable keys are HotSpot 25.0.3's measured live-phase set
    /// (`L1W61JvmtiAgentStartup`'s `props` row, less the launcher's `-D`
    /// properties), in its order.
    #[test]
    fn the_readable_keys_are_hotspots_measured_set() {
        let keys: Vec<&str> = vm_property_keys().collect();
        assert_eq!(
            keys,
            [
                "java.vm.specification.name",
                "java.vm.specification.vendor",
                "java.vm.specification.version",
                "java.vm.name",
                "java.vm.vendor",
                "java.vm.version",
                "java.vm.info",
                "jdk.debug",
                "java.home",
                "java.class.path",
                "java.library.path",
                "sun.boot.library.path",
                "java.vm.compressedOopsMode",
            ]
        );
        assert!(keys.iter().all(|k| !is_internal_property(k)));
    }

    /// Wave 118: every identity key is a readable list key, in the list's
    /// order, and none of them is writeable or launch-dependent.
    #[test]
    fn the_identity_values_cover_the_fixed_list_keys() {
        let keys: Vec<&str> = VM_IDENTITY.iter().map(|(k, _)| *k).collect();
        let fixed: Vec<&str> = vm_property_keys()
            .filter(|k| !is_writeable_vm_property(k) && *k != "java.vm.compressedOopsMode")
            .collect();
        assert_eq!(keys, fixed);
        assert_eq!(vm_identity_value("java.vm.name"), Some("cratonvm"));
        assert_eq!(vm_identity_value("java.vm.info"), None, "the launch's");
    }

    #[test]
    fn the_writeable_keys_are_the_paths_and_the_vm_info() {
        let writeable: Vec<&str> = VM_PROPERTIES
            .iter()
            .filter(|p| p.writeable)
            .map(|p| p.key)
            .collect();
        assert_eq!(
            writeable,
            [
                "java.vm.info",
                "java.home",
                "java.class.path",
                "java.library.path",
                "sun.boot.library.path",
            ]
        );
        assert!(!is_writeable_vm_property("java.vm.version"));
        assert!(is_internal_property("jdk.module.path"));
        assert!(!is_internal_property("jdk.debug"));
    }
}
