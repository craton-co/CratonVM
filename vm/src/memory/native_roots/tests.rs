    use super::*;
    use std::collections::HashSet;

    /// gch h4/wr: only an address inside the old generation's range is asked
    /// for its block (a young value, and every value off Generational, where
    /// the range is empty, is rooted as before).
    #[test]
    fn gch_wr_only_an_old_pending_value_is_asked_whether_its_block_is_allocated() {
        let (base, len) = (0x10_0000usize, 0x1_0000usize);
        assert!(inherited_value_is_old(base, base, len));
        assert!(inherited_value_is_old(base + len - 8, base, len));
        assert!(
            !inherited_value_is_old(base + len, base, len),
            "one past the end"
        );
        assert!(
            !inherited_value_is_old(base - 8, base, len),
            "below (young)"
        );
        assert!(
            !inherited_value_is_old(base, 0, 0),
            "no old generation (G1 / ZGC)"
        );
    }


    /// zt t26/zc5b: `CRATONVM_ZGC_SIDE_ROW_EPHEMERON` is default ON (`=0`
    /// kills) and engages ZGC only; the Generational switch does not reach
    /// ZGC, nor the ZGC one Generational or G1. A ZGC scan defers exactly
    /// when the inherited-`ThreadLocal` row's ZGC arm does: under the licence,
    /// outside a concurrent cycle's opening scan (`with_class_unload_marking`)
    /// and with no concurrent mark in flight (none on a fresh heap).
    #[cfg(feature = "zgc")]
/// zt t26/zc5b item 2 residual (`CRATONVM_ZGC_SIDE_ROW_DEFER_IN_CYCLE`,
    /// default ON): a ZGC concurrent cycle's opening scan defers the side-row
    /// values under the licence, the opening scan restarts the cycle's pin
    /// set, an in-flight scan files the UNION of its own pins and the cycle's
    /// earlier ones (a row deleted mid-cycle keeps its pin to the close), a
    /// scan outside a cycle drops the set, and a remap drops it too. `=0`
    /// roots in the cycle and keeps no set. Generational and G1 never keep one.
    #[cfg(feature = "zgc")]
/// The in-flight half of [`side_row_cycle_pins`], without a running mark:
    /// the union rule itself (the slot already holds the opening scan's pins).
    #[test]
    fn zt26_zc5b_an_in_flight_scan_files_the_union_of_the_cycles_pins() {
        let mut set = SideRowCyclePins::default();
        set.slot(SideRowSource::LogManager)
            .extend([(1, 10), (2, 20)]);
        let slot = set.slot(SideRowSource::LogManager);
        slot.extend([(2, 20), (3, 30)]);
        slot.sort_unstable();
        slot.dedup();
        assert_eq!(*slot, vec![(1, 10), (2, 20), (3, 30)]);
        assert!(set.slot(SideRowSource::Tls).is_empty());    }


    /// gci i23/ep: a rooting scan roots a side-row value it can vouch for (a
    /// young one, or an old one whose block is allocated) and drops an OLD
    /// address whose block is free (gch h4/wr's rule: a deferring major freed
    /// the value of a dead young key that a true-root major left in place).
    #[test]
    fn gci_i23ep_a_rooting_scan_drops_only_a_freed_old_side_row_value() {
        use crate::config::{GcAlgorithm, VmConfig};
        let shared = crate::vm::SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        });
        let live = shared
            .mem
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 0);
        let mut roots = Vec::new();
        push_rooted_side_row_values(&shared, vec![live], &mut roots);
        assert_eq!(roots, vec![live], "an allocated value is rooted");

        let (old_base, old_len) = shared.mem.heap.old_gen_info();
        if old_len >= 4096 {
            // The last word of a fresh old generation: inside its range, in no
            // allocated block.
            let freed_addr = old_base + old_len - 64;
            assert!(!shared.mem.heap.is_addr_live(freed_addr), "precondition");
            // SAFETY: an address only; never dereferenced (the filter asks the
            // heap's block map, which reads no object).
            let freed = unsafe { ObjectRef::from_raw(freed_addr as *mut u8) };
            let mut roots = Vec::new();
            push_rooted_side_row_values(&shared, vec![freed, live], &mut roots);
            assert_eq!(roots, vec![live], "the freed old value is not rooted");
        }
    }

    /// gc wave 9 lane F: the G1 inherited-value pin is default ON and is off
    /// when any of its three switches reads `=0` (the remark closes the pin
    /// only under the other two).
    #[test]
    fn gcf_w9_g1_itl_ephemeron_needs_all_three_remark_switches() {
        const KEYS: [&str; 3] = [
            "CRATONVM_G1_REMARK_ITL_EPHEMERON",
            "CRATONVM_G1_REMARK_ITL_SWEEP",
            "CRATONVM_G1_REMARK_UNLOAD_AFTER_KEEP",
        ];
        let unset: Vec<(&str, Option<&str>)> = KEYS.iter().map(|k| (*k, None)).collect();
        cratonvm_types::flags::with_thread_overrides(&unset, || {
            assert!(g1_itl_pending_ephemeron_enabled(), "default ON")
        });
        for off in KEYS {
            let edits: Vec<(&str, Option<&str>)> = KEYS
                .iter()
                .map(|k| (*k, if *k == off { Some("0") } else { None }))
                .collect();
            cratonvm_types::flags::with_thread_overrides(&edits, || {
                assert!(
                    !g1_itl_pending_ephemeron_enabled(),
                    "{off}=0 must turn it off"
                )
            });
        }
    }


    #[test]
    fn built_in_inventory_has_unique_named_scan_remap_pairs() {
        let mut names = HashSet::new();
        for source in VM_ROOT_SOURCES {
            assert!(!source.name.is_empty());
            assert!(
                names.insert(source.name),
                "duplicate built-in root source: {}",
                source.name
            );
            assert_ne!(source.scan as usize, 0);
            assert_ne!(source.remap as usize, 0);
        }
    }

    /// Every root source must supply BOTH halves as *distinct* functions. A row
    /// that accidentally names the same function twice would either scan on the
    /// remap path or remap on the scan path; the type system cannot catch it
    /// (both halves are `fn(&SharedVm, ..)`-shaped once coerced), so assert it.
    #[test]
    fn every_root_source_pairs_two_distinct_halves() {
        for source in VM_ROOT_SOURCES {
            assert_ne!(
                source.scan as usize, source.remap as usize,
                "root source {} names the same function as both scan and remap",
                source.name
            );
        }
    }

    /// Datagram socket rows are weak and need VM-scoped remap and prune callbacks.
    #[test]
    fn datagram_socket_side_tables_are_a_registered_root_source() {
        let source = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../native-builtins/src/lib_parts/essential_dur_parse.rs"
        ));
        let callbacks = source
            .split("fn scan_net_phase_e(")
            .nth(1)
            .expect("network root callbacks")
            .split("fn forget_net_phase_e(")
            .next()
            .unwrap();
        assert!(callbacks.contains("net_phase_e::gc_update_ds_refs(vm, map)"));
        assert!(callbacks.contains("net_phase_e::gc_sweep_net_socket_rows("));
        let registration = source
            .split("register_vm_scoped_root_callbacks(\n            \"native-builtins-net-phase-e\",") // literal-continuation-ok: source witness
            .nth(1)
            .expect("network callbacks registered for each VM");
        let registration = registration.split("},").next().unwrap();
        assert!(registration.contains("remap: remap_net_phase_e"));
        assert!(registration.contains("prune: prune_net_phase_e"));
    }

    /// gc-common w3-b: the `Class$Atomic` reflection slots have their own row
    /// (so they are loader-conditional and remapped exactly once), and the
    /// unconditional unsafe row no longer carries them.
    #[test]
    fn class_atomic_slots_are_their_own_root_source() {
        assert!(
            VM_ROOT_SOURCES
                .iter()
                .any(|s| s.name == "class-atomic-slots"),
            "the Class$Atomic reflection slots must be a VM root source"
        );
    }

    /// `defer_or_root` records the pin BEFORE it skips the root, and only for
    /// a user-loader owner on a licensed cycle whose marker follows the pin;
    /// every other combination roots the value.
    #[test]
    fn defer_or_root_pins_only_a_user_loader_owner_on_a_licensed_cycle() {
        use crate::config::VmConfig;
        let shared = std::sync::Arc::new(crate::vm::SharedVm::new(VmConfig::default()));
        let loader = shared
            .mem
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 0);
        let value = shared
            .mem
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 0);
        let value_addr = value.as_ptr() as usize;
        // gen r5w4/conc8: the deferral owner also requires a class the unload
        // removes with its loader; these ids are in no class store, so the
        // scan's screen (empty: nothing refused) stands in for it.
        let _screen = crate::memory::roots::DeferralScreen::enter(
            shared.vm_identity,
            rustc_hash::FxHashSet::default(),
        );
        // A class id no other test registers a loader pin for.
        let user_cid: u32 = 0x7ff0_3b01;
        let builtin_cid: u32 = 0x7ff0_3b02;
        // Through the authoritative side table (which also writes the
        // `loader_pin` row): `defer_or_root` uses a pin only when THIS VM's own
        // defining-loader row agrees (`roots::vm_loader_pin_addr`, w4-b).
        cratonvm_native_builtins::classloader::register_defining_loader(
            &shared.loader_identity,
            user_cid,
            loader,
        );
        let pinned = || {
            cratonvm_types::metadata_pin::roots_for_loader(loader.as_ptr() as usize)
                .is_some_and(|p| p.contains(&value_addr))
        };

        let mut roots = Vec::new();
        defer_or_root(&shared, false, Some(user_cid), value, &mut roots);
        assert_eq!(roots, vec![value], "no licence: rooted");
        assert!(!pinned());

        let mut roots = Vec::new();
        defer_or_root(&shared, true, None, value, &mut roots);
        assert_eq!(roots, vec![value], "no owner: rooted");

        let mut roots = Vec::new();
        defer_or_root(&shared, true, Some(builtin_cid), value, &mut roots);
        assert_eq!(
            roots,
            vec![value],
            "a built-in-loader owner has no loader_pin row: rooted"
        );
        assert!(!pinned());

        let mut roots = Vec::new();
        defer_or_root(&shared, true, Some(user_cid), value, &mut roots);
        if shared.mem.heap.metadata_pin_deferrable(value_addr) {
            assert!(
                roots.is_empty(),
                "a user-loader owner's value must not be a root"
            );
            assert!(
                pinned(),
                "...it must be pinned to the owner's loader instead"
            );
        } else {
            assert_eq!(
                roots,
                vec![value],
                "the marker would not follow the pin: rooted"
            );
            assert!(!pinned());
        }
        cratonvm_types::metadata_pin::replace_metadata_pins(shared.vm_identity, &[]);

        // gcd d1/c (`gengc-r5w4-conc8-native-root-deferrals-ignore-the-unload-classification`):
        // a class the unload never removes (the scan's screen names it: a
        // built-in-namespace class a user loader defined) is ROOTED although
        // its loader has a `loader_pin` row, so its value cannot be left
        // unmarked when that loader dies.
        {
            let _refused = crate::memory::roots::DeferralScreen::enter(
                shared.vm_identity,
                [user_cid].into_iter().collect(),
            );
            let mut roots = Vec::new();
            defer_or_root(&shared, true, Some(user_cid), value, &mut roots);
            assert_eq!(roots, vec![value], "a never-unloaded class's value: rooted");
            assert!(!pinned());
        }
        cratonvm_types::loader_pin::forget_vm_loader_pins(shared.vm_identity);
        cratonvm_types::metadata_pin::replace_metadata_pins(shared.vm_identity, &[]);
    }

    /// **AN `ObjectStreamClass` DESCRIPTOR DEFERS ONLY TO ITS OWN VM'S LOADER,
    /// UNDER ITS OWN VM'S LICENCE** (gc-common w5-b).
    ///
    /// The `osc-cache` row used to decide in `OscCache::scan_roots` from the
    /// process-global `metadata_weak_mode()` and the VM-less `loader_pin_addr`
    /// with no `metadata_pin_deferrable` check. Now it goes through
    /// `defer_or_root` under `roots::loader_metadata_licence`. Four arms: a VM
    /// with no user-loader class at the id roots even on a licensed cycle
    /// (another VM's loader at the same id must not capture it); the owning VM
    /// pins iff the heap says the marker follows the pin; an unlicensed cycle
    /// roots; the scoped licence is restored afterwards.
    #[test]
    fn osc_cache_descriptors_defer_only_to_their_own_vms_loader() {
        use crate::config::VmConfig;
        use crate::memory::roots::{loader_metadata_licence, NativeScanLicence};
        let vm_a = std::sync::Arc::new(crate::vm::SharedVm::new(VmConfig::default()));
        let vm_b = std::sync::Arc::new(crate::vm::SharedVm::new(VmConfig::default()));
        // gen r5w4/conc8: see `defer_or_root_pins_only_a_user_loader_owner_on_a_licensed_cycle`.
        let _screen = crate::memory::roots::DeferralScreen::enter(
            vm_a.vm_identity,
            rustc_hash::FxHashSet::default(),
        );
        // A class id no other test registers a loader pin for.
        let cid: u32 = 0x7ff0_5b01;
        let alloc = |vm: &crate::vm::SharedVm| {
            vm.mem
                .heap
                .alloc_object(crate::classloading::ClassId::new(0), 0)
        };
        let loader_a = alloc(&*vm_a);
        let desc_a = alloc(&*vm_a);
        let desc_b = alloc(&*vm_b);
        cratonvm_native_builtins::classloader::register_defining_loader(
            &vm_a.loader_identity,
            cid,
            loader_a,
        );
        vm_a.classes
            .osc_cache
            .insert_if_absent(cratonvm_types::ClassId::new(cid), desc_a);
        vm_b.classes
            .osc_cache
            .insert_if_absent(cratonvm_types::ClassId::new(cid), desc_b);
        let pinned = |desc: ObjectRef| {
            cratonvm_types::metadata_pin::roots_for_loader(loader_a.as_ptr() as usize)
                .is_some_and(|p| p.contains(&(desc.as_ptr() as usize)))
        };

        {
            let _licence = NativeScanLicence::enter(vm_b.vm_identity, true);
            assert!(loader_metadata_licence(&vm_b), "the scoped licence is read");
            let mut roots = Vec::new();
            scan_osc_cache(&vm_b, &mut roots);
            assert_eq!(
                roots,
                vec![desc_b],
                "B has no user-loader class at this id: rooted, never pinned to A's loader"
            );
            assert!(!pinned(desc_b));
        }

        {
            let _licence = NativeScanLicence::enter(vm_a.vm_identity, false);
            let mut roots = Vec::new();
            scan_osc_cache(&vm_a, &mut roots);
            assert_eq!(roots, vec![desc_a], "no licence: rooted");
            assert!(!pinned(desc_a));
        }

        {
            let _licence = NativeScanLicence::enter(vm_a.vm_identity, true);
            let mut roots = Vec::new();
            scan_osc_cache(&vm_a, &mut roots);
            if vm_a
                .mem
                .heap
                .metadata_pin_deferrable(desc_a.as_ptr() as usize)
            {
                assert!(
                    roots.is_empty(),
                    "A's own user-loader descriptor is not a root"
                );
                assert!(pinned(desc_a), "...it is pinned to A's loader instead");
            } else {
                assert_eq!(
                    roots,
                    vec![desc_a],
                    "the marker would not follow the pin (a young Generational \
                     descriptor): rooted"
                );
            }
            {
                let _inner = NativeScanLicence::enter(vm_a.vm_identity, false);
                assert!(!loader_metadata_licence(&vm_a));
            }
            assert!(
                loader_metadata_licence(&vm_a),
                "a nested scope restores the outer licence"
            );
        }

        cratonvm_types::loader_pin::forget_vm_loader_pins(vm_a.vm_identity);
        cratonvm_types::metadata_pin::replace_metadata_pins(vm_a.vm_identity, &[]);
    }

    /// **THE PROCESS-GLOBAL ROWS ARE A RATCHET, NOT A HABIT**
    /// (`common-b-process-global-root-sources-FIXED-20260923.md`, gc-common w4-b).
    ///
    /// The module doc says every source is VM-scoped or a documented exception
    /// whose scan half ignores the `SharedVm` (`_:`). Those exceptions hand VM
    /// B's collector VM A's addresses in a multi-VM process (`libcratonvm`
    /// embedders, this crate's parallel unit tests). They cannot all be fixed
    /// from this file -- each backing store lives in `native-builtins` /
    /// `native-io` / `native-api` -- but the set must only SHRINK. This freezes
    /// it exactly: a new `_:` scan fails here (key its store on `vm_identity`
    /// instead, the `logmanager` / `security-manager` pattern), and a fixed
    /// row must be removed from `FROZEN` so the ratchet tightens.
    #[test]
    fn process_global_root_sources_only_shrink() {
        // gc-common w8-a: 13 -> 5. `lambda-callsites`, `jboss-msc`,
        // `http-handlers`, `datagram-sockets`, `inet-addresses`,
        // `scheduled-pump`, `xnio-futures` and `panama-upcalls` are VM-scoped.
        // gc-common w9-b: 5 -> 3. `forkjoin` and `tls` are VM-scoped (the
        // `tls` row's `x509_manager` half since the w9 handoffs).
        // gc-common w9 handoffs: 3 -> 2. `server-ports` is VM-scoped.
        // gc-common w10-a / w10-b: 2 -> 0. `locale` and `nio` are VM-scoped;
        // every scan half now takes the collecting VM.
        const FROZEN: &[&str] = &[];
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/memory/native_roots.rs");
        let src = std::fs::read_to_string(path)
            .expect("native_roots.rs is readable")
            .replace("\r\n", "\n");
        // Every row's scan half, `_:` or not: the parser's own sanity check.
        let rows = src
            .lines()
            .filter(|l| {
                l.strip_prefix("fn scan_")
                    .and_then(|rest| rest.split_once('('))
                    .is_some_and(|(_, tail)| tail.contains("&crate::vm::SharedVm"))
            })
            .count();
        let mut found: Vec<String> = src
            .lines()
            .filter_map(|l| {
                let rest = l.strip_prefix("fn scan_")?;
                let (name, tail) = rest.split_once('(')?;
                tail.starts_with("_: &crate::vm::SharedVm")
                    .then(|| format!("scan_{name}"))
            })
            .collect();
        found.sort_unstable();
        assert!(
            rows >= 20,
            "the row parse found only {rows} scan halves -- the parser is broken, \
             failing rather than passing vacuously"
        );
        assert_eq!(
            found, FROZEN,
            "the set of root sources whose scan ignores the collecting VM changed. \
             A new one must key its store on `vm_identity`; a fixed one must be \
             removed from FROZEN"
        );
    }

    #[test]
    fn instrument_transformer_chain_is_a_registered_root_source() {
        assert!(
            VM_ROOT_SOURCES
                .iter()
                .any(|s| s.name == "instrument-transformers"),
            "the java.lang.instrument transformer chain must be a VM root source"
        );
    }

    #[test]
    fn classloader_value_rows_are_a_registered_root_source() {
        assert!(
            VM_ROOT_SOURCES
                .iter()
                .any(|s| s.name == "classloader-values"),
            "AbstractClassLoaderValue values are rooted only by this row \
             (gc-common w17-d)"
        );
    }

    #[test]
    fn type_variable_caches_are_a_registered_root_source() {
        assert!(
            VM_ROOT_SOURCES.iter().any(|s| s.name == "type-variables"),
            "the reflection TypeVariable caches are rooted only by this row \
             (gc-common w30-b)"
        );
    }

    #[test]
    fn defined_package_memo_is_a_registered_root_source() {
        assert!(
            VM_ROOT_SOURCES.iter().any(|s| s.name == "defined-packages"),
            "the defined-`Package` memo is rooted only by this row (gc-common w31-a)"
        );
    }

    #[test]
    fn layer_module_pins_are_a_registered_root_source() {
        assert!(
            VM_ROOT_SOURCES.iter().any(|s| s.name == "layer-modules"),
            "a user loader's weakly held Module is kept by this row only (zs s5/nd5)"
        );
    }
