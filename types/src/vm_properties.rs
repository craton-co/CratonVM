// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! The keys and flags of HotSpot's VM property list
//! (`Arguments::system_properties()`), in one table (interpreter round i4
//! wave 95, lane L1; stage 1 of
//! `docs/feature-designs/real-jdk-mode.md`).
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
/// as JDK 25 lists them in the live phase. NOT in HotSpot's order (wave 135
/// measured it: [`HOTSPOT_LIST_ORDER`]); the set is (MEASURED:
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

/// [`VM_PROPERTIES`]' keys in the ORDER HotSpot 25's list holds them
/// (interpreter round i7 wave 135, lane L1b; stage 4 of the i61-L1
/// property-list proposal). MEASURED on HotSpot 25.0.3 (Windows, default
/// flags and `-Xint`): `SystemProps$Raw.vmProperties()` -- `JVM_GetProperties`,
/// the walk JVMTI `GetSystemProperties` makes too -- answers
/// `java.vm.specification.name java.vm.version java.vm.name jdk.debug
/// sun.boot.library.path java.library.path java.home java.class.path
/// [jdk.boot.class.path.append] java.vm.info java.vm.specification.vendor
/// java.vm.specification.version java.vm.vendor`, then the `-D` properties
/// in command-line order (a `-D` of a list key keeps the key's place), then
/// `sun.java.command`, `sun.java.launcher`, then the after-heap
/// `java.vm.compressedOopsMode` (`Arguments::init_system_properties`, then
/// `init_version_specific_system_properties`, then argument parsing, then
/// universe init). `VM_PROPERTIES` itself keeps this VM's historical order,
/// which its tests and `vm_init::vm_property_list` pin; the readers that
/// answer an ORDER sort by [`hotspot_list_sort_key`] under
/// `CRATONVM_VM_PROPERTY_LIST_ORDER` (default ON, `=0` off). Probe:
/// `L1bW135VmPropertyListOrder.java`.
pub const HOTSPOT_LIST_ORDER: [&str; 13] = [
    "java.vm.specification.name",
    "java.vm.version",
    "java.vm.name",
    "jdk.debug",
    "sun.boot.library.path",
    "java.library.path",
    "java.home",
    "java.class.path",
    "java.vm.info",
    "java.vm.specification.vendor",
    "java.vm.specification.version",
    "java.vm.vendor",
    "java.vm.compressedOopsMode",
];

/// Where `key` sorts in HotSpot's list ([`HOTSPOT_LIST_ORDER`]), for a
/// STABLE sort of a property vector: a list key set before the heap is
/// `(0, its place)`; any other key (a `-D` one, the launcher's, a platform
/// key a reader mixes in) is `(1, 0)`, so those keep their own order after
/// the list; an after-heap list key is `(2, its place)`, after everything.
pub fn hotspot_list_sort_key(key: &str) -> (u8, usize) {
    match HOTSPOT_LIST_ORDER.iter().position(|k| *k == key) {
        Some(at) if VM_PROPERTIES.iter().any(|p| p.key == key && p.after_heap) => (2, at),
        Some(at) => (0, at),
        None => (1, 0),
    }
}

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
/// zt t27/lg1: under `CRATONVM_VM_IDENTITY_HOTSPOT` (default ON)
/// `java.vm.name` is [`HOTSPOT_VM_NAME`].
pub fn vm_identity_value(key: &str) -> Option<&'static str> {
    if key == "java.vm.name" && vm_identity_hotspot_enabled() {
        return Some(HOTSPOT_VM_NAME);
    }
    VM_IDENTITY.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// The HotSpot collector family whose compressed-oops rules a VM follows
/// ([`hotspot_compressed_oops_mode`]). This VM's Generational collector is
/// HotSpot's Serial twin (`-XX:+UseSerialGC`), as JFR and `PrintFlagsFinal`
/// already model it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompressedOopsCollector {
    /// `-XX:+UseSerialGC` (this VM's Generational collector). Conservative
    /// heap alignment 2 MiB (the card table's), so compressed oops fit up to
    /// `32 GiB - 2 MiB`.
    Serial,
    /// `-XX:+UseG1GC`. Conservative heap alignment is G1's largest
    /// ergonomic region, 32 MiB, so compressed oops fit up to
    /// `32 GiB - 32 MiB`.
    G1,
    /// `-XX:+UseZGC`: no compressed oops, so no property.
    Z,
}

/// HotSpot 25's `java.vm.compressedOopsMode` for a heap of `max_heap` bytes
/// (`MaxHeapSize`) under `collector`, with `-XX:±UseCompressedOops` as
/// `use_compressed_oops` (HotSpot's default `true`); `None` when HotSpot sets
/// no such property (interpreter round i7 wave 136, lane L1b;
/// `docs/feature-designs/real-jdk-mode.md`).
///
/// `Arguments::set_use_compressed_oops` turns compressed oops off under ZGC,
/// under `-XX:-UseCompressedOops`, and for a heap above
/// `max_heap_for_compressed_oops()` (`32 GiB` less the collector's
/// conservative heap alignment); then `Universe::initialize_heap` never sets
/// the property. Otherwise `CompressedOops::initialize` places the heap at
/// `HeapBaseMinAddress` (2 GiB on x86_64) when it can: a heap ending below
/// 4 GiB is unscaled (`32-bit`), below 32 GiB zero-based, else the base is
/// aligned to 32 GiB (`Non-zero disjoint base`). MEASURED on HotSpot 25.0.3
/// (Windows x64, `System.getProperty`; probe
/// `L1bW136CompressedOopsModeProperty.java`):
/// G1 `-Xmx2g` `32-bit`, `-Xmx2049m` / `-Xmx30g` `Zero based`, `-Xmx30721m` /
/// `-Xmx32736m` disjoint, `-Xmx32737m` / `-Xmx40g` absent; Serial `-Xmx2047m`
/// `32-bit`, `-Xmx2049m` / `-Xmx30g` `Zero based`, `-Xmx30721m` / `-Xmx32766m`
/// disjoint, `-Xmx32767m` absent; ZGC and `-XX:-UseCompressedOops` absent.
/// HotSpot tests the typed `-Xmx`; `max_heap` may be the collector-aligned
/// one (`VmConfig::max_heap_size`): the limits are multiples of every
/// alignment the collector uses, so the answer is the same. A heap the OS
/// cannot place at the preferred address can fall to a weaker mode on
/// HotSpot; that is not modelled. Pure.
pub fn hotspot_compressed_oops_mode(
    max_heap: u64,
    collector: CompressedOopsCollector,
    use_compressed_oops: bool,
) -> Option<&'static str> {
    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;
    const HEAP_BASE_MIN_ADDRESS: u64 = 2 * GIB;
    const UNSCALED_OOP_HEAP_MAX: u64 = 4 * GIB;
    const OOP_ENCODING_HEAP_MAX: u64 = 32 * GIB;
    let alignment = match collector {
        CompressedOopsCollector::Z => return None,
        CompressedOopsCollector::Serial => 2 * MIB,
        CompressedOopsCollector::G1 => 32 * MIB,
    };
    if !use_compressed_oops || max_heap > OOP_ENCODING_HEAP_MAX - alignment {
        return None;
    }
    Some(if HEAP_BASE_MIN_ADDRESS + max_heap <= UNSCALED_OOP_HEAP_MAX {
        "32-bit"
    } else if HEAP_BASE_MIN_ADDRESS + max_heap <= OOP_ENCODING_HEAP_MAX {
        "Zero based"
    } else {
        "Non-zero disjoint base"
    })
}

/// HotSpot's `CompressedOops::mode_to_string` for a narrow-oop `base` and
/// `shift` that are in force (wave 136, lane L1b): what the property says
/// when this VM really runs with compressed oops (`-XX:+UseCompressedOops`
/// admitted, `cratonvm_types::narrow_oop`), the same test as JFR's
/// `jdk.GCHeapConfiguration.compressedOopsMode` and the `gc,init`
/// `Compressed Oops:` line. The disjoint test is a base aligned to
/// `4G << LogMinObjAlignmentInBytes` (32 GiB). Pure.
pub fn hotspot_narrow_oop_mode_name(base: u64, shift: usize) -> &'static str {
    const DISJOINT_ALIGN: u64 = 1 << 35;
    if base == 0 && shift == 0 {
        "32-bit"
    } else if base == 0 {
        "Zero based"
    } else if base % DISJOINT_ALIGN == 0 {
        "Non-zero disjoint base"
    } else {
        "Non-zero based"
    }
}

/// The last `-XX:±UseCompressedOops` of a VM option list (HotSpot's
/// `jvm_args`), or `true` (HotSpot's default) when there is none (wave 136,
/// lane L1b). Pure.
pub fn use_compressed_oops_from_jvm_args<'a>(args: impl IntoIterator<Item = &'a String>) -> bool {
    args.into_iter()
        .filter_map(|a| match a.as_str() {
            "-XX:+UseCompressedOops" => Some(true),
            "-XX:-UseCompressedOops" => Some(false),
            _ => None,
        })
        .last()
        .unwrap_or(true)
}

/// zt t27/lg1: HotSpot 25's `java.vm.name` on a 64-bit server build
/// (`VM_Version::vm_name()`), the VM every JDK 25 image this VM runs ships;
/// measured on the host (25.0.4, row `pl8_census_J`'s
/// `java.property.java.vm.name`) and locally (25.0.3).
pub const HOTSPOT_VM_NAME: &str = "OpenJDK 64-Bit Server VM";

/// `CRATONVM_VM_IDENTITY_HOTSPOT` (zt t27/lg1, page
/// `zgc-zt12pl4-perf-counters-what-is-left-*`; default ON, `=0` kill; both
/// modes, every view): the VM names itself as HotSpot 25 does for the image
/// it runs -- `java.vm.name` [`HOTSPOT_VM_NAME`], `java.vm.vendor` and
/// `java.vm.version` the image's own (`vm_init::vm_identity_properties`) --
/// in `System.getProperty`, JVMTI, the perf counters, `RuntimeMXBean`,
/// `jcmd VM.version`, JDWP and JFR, which all read the VM's property list.
/// `=0`: `cratonvm` / `cratonvm` / `25.0.1+8`, as before.
pub fn vm_identity_hotspot_enabled() -> bool {
    true
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
mod tests;
