#[allow(unused_imports)]
use super::*;

impl<'a> NativeSystemAccess for NativeContextImpl<'a> {
    fn reduce_signal_usage(&self) -> bool {
        self.shared.config.reduce_signal_usage
    }

    // See the `NativeContext::refresh_root_snapshot` doc comment
    // (native-api/src/registry.rs) for the full rationale — this closes the
    // "pinned a long-lived batch, then drove long re-entrant/JIT-heavy
    // execution without ever blocking or self-initiating GC" gap by reusing
    // the existing blocking-path deposit mechanism without actually
    // blocking.
    fn refresh_root_snapshot(&mut self) {
        // cceres3 ROOT FIX (WildFly boot CCE long tail): this is a
        // NON-blocking republish — the caller (the native-collections stream
        // drain loops) keeps executing Java right after it. The old
        // `deposit_root_snapshot()` call was the raise=true variant: it set
        // `in_blocked_region` and nothing ever consumed it, so the identity
        // census EXCLUDED the running thread from every subsequent STW pause
        // (moving collections completed under its feet), its
        // `gc_block_state.fixup` accumulated unconsumed (observed live:
        // fixup_pending=44 across four raise=true deposits while running 43
        // frames deep in infinispan/management-model stream work), and every
        // frame ref it held or stored went stale — the poisoned-island
        // producer behind the WFLYCTL0079 / "Object cannot be cast to X"
        // family. The no-flag variant republishes pins/snapshot without
        // touching the flag — exactly what a still-running thread needs.
        self.deposit_root_snapshot_no_flag();
    }

    // rt22 (open): interpreter frames only; a compiled override of the method is invisible here.
    // See `rt22-compiled-callers-invisible-to-interpreter-frame-readers-20261102.md`.
    fn is_executing_instance_method(
        &self,
        receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
    ) -> bool {
        let receiver = self.shared.mem.heap.load_and_forward(receiver);
        self.thread.frames.iter().rev().any(|frame| {
            if frame.method_name() != method_name || frame.method_descriptor() != descriptor {
                return false;
            }
            matches!(
                frame.get_local(0),
                Value::Object(Some(this)) if self.shared.mem.heap.load_and_forward(this) == receiver
            )
        })
    }

    fn dbg_set_watch_cell(&mut self, addr: usize) {
        cratonvm_gc::heap::set_dynamic_watch(addr);
        crate::runtime::crash_handler::arm_generic_heap_watch(addr);
    }

    /// zt t9/pl: this VM's perf-data view (`PerfDataFile::attach_view`),
    /// for `Perf.attach0(0)`. Every collector: the counters are HotSpot's
    /// on each. `CRATONVM_PERF_ATTACH_SELF=0` keeps the zero-capacity answer.
    fn attach_own_perf_data(&mut self) -> Option<(usize, usize)> {
        self.shared
            .debug
            .perf_data_file
            .lock()
            .as_ref()
            .and_then(|file| file.attach_view())
    }

    /// zt t10/pl2: another VM's perf data, mapped read-only
    /// (`perf_data_file::attach_foreign`) and recorded per VM for
    /// `Perf.detach`. Every collector. `CRATONVM_PERF_ATTACH_FOREIGN=0`: the
    /// zero-capacity answer, as before.
    fn attach_foreign_perf_data(&mut self, lvmid: i32) -> Option<(usize, usize)> {
        if !true {
            return None;
        }
        let view = crate::runtime::perf_data_file::attach_foreign(lvmid)?;
        self.shared.debug.perf_foreign_views.lock().push(view);
        Some(view)
    }

    #[cfg(unix)]
    fn attach_foreign_perf_data_hotspot(
        &mut self,
        lvmid: i32,
    ) -> Option<Result<Option<(usize, usize)>, cratonvm_native_api::ForeignPerfAttachFailure>>
    {
        let on = cratonvm_types::flags::runtime_flag_default_on;
        if !on("CRATONVM_PERF_ATTACH_FOREIGN")
            || !crate::runtime::perf_data_file::attach_hotspot_lookup_enabled()
        {
            return None;
        }
        let attached = crate::runtime::perf_data_file::attach_foreign_hotspot(lvmid);
        if let Ok(Some(view)) = attached {
            self.shared.debug.perf_foreign_views.lock().push(view);
        }
        Some(attached)
    }

    /// zt t14/pl5: HotSpot's `IllegalArgumentException` for an id with no
    /// perf data (`perf_data_file::foreign_attach_missing_message`), under
    /// `CRATONVM_PERF_ATTACH_MISSING_THROWS` (default ON, `=0` kill: the
    /// zero-capacity answer, as before) and the foreign attach's own switch.
    fn foreign_perf_data_missing(&mut self, lvmid: i32) -> Option<&'static str> {
        let on = cratonvm_types::flags::runtime_flag_default_on;
        if !on("CRATONVM_PERF_ATTACH_FOREIGN") || !on("CRATONVM_PERF_ATTACH_MISSING_THROWS") {
            return None;
        }
        crate::runtime::perf_data_file::foreign_attach_missing_message(lvmid)
    }

    /// zt t10/pl2: unmap a recorded foreign view at `address`; this VM's
    /// own view and any other buffer are left alone.
    fn detach_perf_data(&mut self, address: usize) {
        let own = self
            .shared
            .debug
            .perf_data_file
            .lock()
            .as_ref()
            .and_then(|file| file.own_base());
        // zt t11/pl3 (t11/x8 P4): the decision is
        // `perf_data_file::take_foreign_view`, unit-tested there.
        let taken = crate::runtime::perf_data_file::take_foreign_view(
            &mut self.shared.debug.perf_foreign_views.lock(),
            own,
            address,
        );
        if let Some((base, len)) = taken {
            crate::runtime::perf_data_file::detach_foreign(base, len);
        }
    }

    fn note_varhandle_field_resolution(&mut self) {
        crate::jit::helpers::refresh_resolved_static_varhandles(&self.shared);
    }

    fn vm_identity(&self) -> usize {
        self.shared.vm_identity
    }

    /// This VM's bootstrap-append census (round 14 wave 2, MISC11-2). Round 14
    /// wave 3 (RV3-4): a VM that never appended answers with one `Acquire`
    /// load, no class-manager lock.
    fn is_bootstrap_appended_class(&self, internal: &str) -> bool {
        if !self
            .shared
            .classes
            .bootstrap_append_seen
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return false;
        }
        self.shared
            .classes
            .class_manager
            .read()
            .is_bootstrap_appended_class(internal)
    }
    fn boot_append_package_location(&self, package: &str) -> Option<String> {
        if !self
            .shared
            .classes
            .bootstrap_append_seen
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return None;
        }
        self.shared
            .classes
            .class_manager
            .read()
            .boot_append_package_location(package)
            .map(str::to_owned)
    }

    fn jvm_input_arguments(&self) -> Vec<String> {
        self.shared.config.jvm_args.clone()
    }

    fn assertion_directives(
        &self,
    ) -> Option<&cratonvm_native_api::assertion_directives::AssertionDirectives> {
        self.shared.config.assertion_directives.as_ref()
    }

    fn instance_field_descriptor_byte(&self, obj: ObjectRef, index: usize) -> Option<u8> {
        let (obj, validated) = self.shared.mem.heap.load_and_forward_checked(obj);
        if !validated {
            return None;
        }
        let class_id = self.shared.mem.heap.class_id_of_validated(obj);
        resolve_field_descriptor_byte_cached(self.shared, class_id, index)
    }

    fn boot_append_loaded_packages(&self) -> Vec<String> {
        if !self
            .shared
            .classes
            .bootstrap_append_seen
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Vec::new();
        }
        self.shared
            .classes
            .class_manager
            .read()
            .boot_append_loaded_packages()
    }

    /// This VM's creation instant (gc-common w17-e): the origin of its
    /// `RuntimeMXBean.getStartTime()` / `getUptime()` and `GcInfo` times, so a
    /// second VM in the process no longer reports the first VM's start.
    fn vm_start_instant(&self) -> Option<std::time::Instant> {
        Some(self.shared.debug.diagnostic_counters.start_instant())
    }

    /// This VM's `-verbose:gc` launch switch (gc-common w18-g).
    fn verbose_gc_at_launch(&self) -> bool {
        // gcg g5/yg: HotSpot's JMM answers `isVerbose()` from the `gc` log
        // level at info, so `-Xlog:gc` (and `-verbose:gc` mapped to it by
        // `CRATONVM_VERBOSE_GC_IS_XLOG_GC`) count as verbose too.
        self.shared.config.verbose_gc
            || crate::runtime::unified_logging::is_unified_logging_enabled(
                &[crate::runtime::unified_logging::LogTag::Gc],
                crate::runtime::unified_logging::LogLevel::Info,
            )
    }

    /// `MemoryMXBean.setVerbose` (gc-common w18-g): the per-pause lines and
    /// the backend's per-collection line, for this VM only.
    fn set_verbose_gc(&self, on: bool) {
        self.shared
            .mem
            .verbose_gc
            .store(on, std::sync::atomic::Ordering::Relaxed);
        self.shared.mem.heap.set_gc_logging(on);
    }

    fn record_printed_value(&mut self, value: Value) {
        self.thread.printed.push(value);
    }

    fn fast_unbox_primitive_wrapper(&self, obj: ObjectRef) -> Option<Option<Value>> {
        // An array's header carries its COMPONENT's class id: an `Integer[]`
        // is not an `Integer`, and must not reach the memo or the field read
        // (page `r11w13-rt-vm-exec-wrapper-unbox-array-alias-patch`).
        if self.shared.mem.heap.kind_of(obj) == cratonvm_types::ObjectKind::Array {
            return Some(None);
        }
        let class_id = self.shared.mem.heap.class_id_of(obj);
        let vm_key = self.shared.vm_identity;
        if PRIMITIVE_WRAPPER_CLASS_CACHE.with(|cache| cache.get() == Some((vm_key, class_id))) {
            return Some(read_wrapper_primitive_field(self.shared, obj));
        }
        if NON_PRIMITIVE_WRAPPER_CLASS_CACHE.with(|cache| cache.get() == Some((vm_key, class_id))) {
            return Some(None);
        }
        // `matches!` runs against a borrow of the name under the guard rather
        // than cloning the `Arc<str>` out — the clone/drop pair was itself a
        // contended atomic on shared class metadata.
        let recognized = self
            .shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|class| {
                matches!(
                    class.name.as_ref(),
                    "java/lang/Integer"
                        | "java/lang/Long"
                        | "java/lang/Boolean"
                        | "java/lang/Character"
                        | "java/lang/Byte"
                        | "java/lang/Short"
                        | "java/lang/Float"
                        | "java/lang/Double"
                )
            });
        match recognized {
            Some(true) => {
                PRIMITIVE_WRAPPER_CLASS_CACHE.with(|cache| cache.set(Some((vm_key, class_id))));
                Some(read_wrapper_primitive_field(self.shared, obj))
            }

            Some(false) => {
                NON_PRIMITIVE_WRAPPER_CLASS_CACHE.with(|cache| cache.set(Some((vm_key, class_id))));
                Some(None)
            }
            // Class not resolvable (early bootstrap, or a synthetic id with no
            // class-store entry). Answer "not a wrapper" as before, but do NOT
            // memoize it — unlike a resolved name, this can change once the
            // class is registered.
            None => Some(None),
        }
    }

    /// Fused `class_id_of_object` + `fast_unbox_primitive_wrapper` — see the
    /// trait declaration for why the pair is worth a door of its own.
    ///
    /// The saving is one `is_object_address` membership walk plus one
    /// forwarding barrier: `fast_unbox_primitive_wrapper` opens by reading the
    /// class id and the default implementation of this method would read it a
    /// second time through `class_id_of_object`. Here it is read once and both
    /// answers come out of it.
    ///
    /// `class_id_of_object`'s `CRATONVM_DBG_DEADRECV` probe is kept, and kept
    /// FIRST, because that is the whole point of it: it answers before the
    /// first dereference, so a receiver the collector already reclaimed is
    /// named rather than faulted on. A hit answers exactly what the two
    /// separate calls answer on a hit — `ClassId::new(0)` and "not a wrapper",
    /// since `class_id_of` would read `0` out of the reclaimed header and no
    /// class 0 is a wrapper.
    fn class_id_and_unboxed_primitive(&self, obj: ObjectRef) -> (ClassId, Option<Option<Value>>) {
        if deadrecv_check(&self.shared, self.thread, obj, "class_id_of_object") {
            return (ClassId::new(0), Some(None));
        }
        // Same array/component alias as `fast_unbox_primitive_wrapper`. The id
        // half keeps its old value: an array key never matches a wrapper,
        // because its primitive half is `None`.
        if self.shared.mem.heap.kind_of(obj) == cratonvm_types::ObjectKind::Array {
            return (self.shared.mem.heap.class_id_of(obj), Some(None));
        }
        let class_id = self.shared.mem.heap.class_id_of(obj);
        let vm_key = self.shared.vm_identity;
        if PRIMITIVE_WRAPPER_CLASS_CACHE.with(|cache| cache.get() == Some((vm_key, class_id))) {
            return (
                class_id,
                Some(read_wrapper_primitive_field(self.shared, obj)),
            );
        }
        if NON_PRIMITIVE_WRAPPER_CLASS_CACHE.with(|cache| cache.get() == Some((vm_key, class_id))) {
            return (class_id, Some(None));
        }
        // Cold: the class is not in either memo yet. Fall back to the
        // unfused door rather than growing a second copy of its
        // name-recognition ladder and its two memo installs.
        (class_id, self.fast_unbox_primitive_wrapper(obj))
    }

    fn copy_from_native_memory(&self, addr: i64, out: &mut [u8]) -> bool {
        // NIO-SERVER-SOCKET: `Unsafe.allocateMemory` returns synthetic arena
        // handles (base 0x10_0000_0000), not real pointers. A
        // `DirectByteBuffer.address()` from `Util.getTemporaryDirectBuffer`
        // reaching `Net.read0`/`SocketDispatcher` is such a handle — route it
        // through the off-heap store. Real OS pointers fall through to raw.
        //
        // Classified by the TAG BIT, not by `unsafe_arena_contains`. Two
        // reasons, in order of importance:
        //
        //  1. Correctness. `contains` is a *liveness* test, so a tagged handle
        //     whose arena had already been freed answered `false` and fell
        //     through to the raw branch below, where a synthetic handle
        //     (0x4000_0001_0000_0000-ish) is `copy_nonoverlapping`d as an OS
        //     pointer. That is a SIGSEGV on exactly the use-after-free the
        //     single-element `Unsafe.get*(long)` natives already refuse. The
        //     tag test is true for freed handles too, so the copy is declined
        //     and the caller raises the Java-visible error.
        //  2. Cost. `contains` is an `RwLock` read plus a `BTreeMap` range
        //     probe, and `unsafe_arena_copy_out` then repeats it. On the
        //     `DirectByteBuffer` per-element path (see `dbb_get_abs` in
        //     `native-io/src/direct_buffer.rs`) that is two locked map probes
        //     per byte moved.
        if cratonvm_native_builtins::unsafe_arena_addr_is_tagged(addr) {
            return cratonvm_native_builtins::unsafe_arena_copy_out(addr, out);
        }
        // Reject null / negative handles. Returning `false` (not performing the
        // copy) is how this layer signals failure; the native caller turns that
        // into the appropriate Java exception. The bounds of `out` itself are
        // the Java-side array bounds — the caller has already sliced the array,
        // so `out.len()` is the validated request size.
        if addr <= 0 {
            return false;
        }
        // Guard against an `addr + len` range that wraps past the end of the
        // address space (a forged length on a high `addr`). Such a copy would
        // read out-of-bounds / UB; refuse it instead. `addr > 0` here, so the
        // `as u64` cast is exact.
        if !native_range_is_in_bounds(addr as u64, out.len()) {
            return false;
        }
        // SAFETY: `addr` is a real, readable native pointer (not an arena
        // handle); the `[addr, addr + out.len())` range is non-wrapping
        // (checked above) and `out.len()` bytes are copied from it.
        unsafe {
            std::ptr::copy_nonoverlapping(addr as *const u8, out.as_mut_ptr(), out.len());
        }
        true
    }

    fn class_id_of_object_forwarded(&self, obj: ObjectRef) -> ClassId {
        // The first three lines of `get_field_by_name`, verbatim and for the
        // same reason: forward FIRST, then take the id from the forwarded
        // object, so a memoized slot index and the `get_field` that uses it
        // describe the same object.
        let (obj, obj_validated) = self.shared.mem.heap.load_and_forward_checked(obj);
        if obj_validated {
            self.shared.mem.heap.class_id_of_validated(obj)
        } else {
            ClassId::new(0)
        }
    }

    fn unsafe_byte_field_offsets(&self) -> bool {
        self.shared.classes.unsafe_byte_field_offsets
    }

    fn native_addr_is_arena_handle(&self, addr: i64) -> bool {
        // The same TAG-BIT test `copy_from_native_memory` / `copy_to_native_memory`
        // classify with, and deliberately not a liveness test: a freed handle is
        // still a handle, and reporting it as a raw pointer would tell the socket
        // census that a bounce-free transfer was available when it was not.
        cratonvm_native_builtins::unsafe_arena_addr_is_tagged(addr)
    }

    fn copy_to_native_memory(&mut self, addr: i64, data: &[u8]) -> bool {
        // Tag bit, not liveness — see `copy_from_native_memory` above for why.
        if cratonvm_native_builtins::unsafe_arena_addr_is_tagged(addr) {
            return cratonvm_native_builtins::unsafe_arena_copy_in(addr, data);
        }
        // Reject null / negative handles (failure is signalled by `false`; the
        // native caller raises the appropriate Java exception). `data` is the
        // Java-side source slice, already bounds-checked by the caller.
        if addr <= 0 {
            return false;
        }
        // Refuse a destination range `[addr, addr + len)` that would wrap past
        // the end of the address space (forged length on a high `addr`) — such
        // a write is out-of-bounds / UB. `addr > 0`, so `as u64` is exact.
        if !native_range_is_in_bounds(addr as u64, data.len()) {
            return false;
        }
        // DBG (bc math-ec): a raw copy whose destination aliases the MANAGED
        // HEAP is the prime suspect for the `0x4`-into-Fp-reference-field
        // corruption (e.g. Unsafe.copyMemory(heapSrc, off, null, heapAddr, n)
        // routed here with a heap dst). Catch it with the live Java stack so
        // the offending call site is pinned. is_heap_addr is region-membership
        // only (no header read) so it is safe on an arbitrary address.
        if heapcopy_dbg() && self.shared.mem.heap.is_heap_addr(addr as usize).is_some() {
            let n = data.len().min(16);
            eprintln!(
                "[heapcopy-WRITE] addr=0x{:x} len={} data={:02x?}",
                addr,
                data.len(),
                &data[..n],
            );
            for f in self.thread.frames.iter().rev().take(14) {
                eprintln!(
                    "[heapcopy-STK]   {}.{} pc={}",
                    f.class_name(),
                    f.method_name(),
                    f.pc,
                );
            }
        }
        // SAFETY: `addr` is a real, writable native pointer (not an arena
        // handle); `data.len()` bytes are copied to it.
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), addr as *mut u8, data.len());
        }
        true
    }

    fn record_printed_line(&mut self, text: String) {
        record_printed_line_bounded(&mut self.thread.printed_lines, text);
    }

    fn get_system_stream(&self, name: &str) -> Option<ObjectRef> {
        match name {
            "out" => *self.shared.system_out.read(),
            "err" => *self.shared.system_err.read(),
            "in" => *self.shared.system_in.read(),
            _ => None,
        }
    }

    fn cache_system_stdin(&mut self, stream: ObjectRef) {
        *self.shared.system_in.write() = Some(stream);
    }

    fn get_cached_module_mirror(&self, module_name: Option<&str>) -> Option<ObjectRef> {
        let key = module_name.unwrap_or("");
        self.shared.classes.module_mirrors.read().get(key).copied()
    }

    fn cache_module_mirror(&mut self, module_name: Option<&str>, module: ObjectRef) {
        let key = module_name.unwrap_or("").to_string();
        self.shared
            .classes
            .module_mirrors
            .write()
            .insert(key, module);
    }

    fn fixup_class_module_fields(&mut self) -> usize {
        super::vm_object::fixup_class_module_fields(self.shared)
    }

    fn record_layer_module_packages(
        &mut self,
        loader_ns: u32,
        packages: &[String],
        module: ObjectRef,
    ) {
        if packages.is_empty() {
            return;
        }
        // One global root per Module, shared by its packages. The root table's
        // lock is taken before, never under, the map's.
        //
        // zs s5/nd5: on ZGC a user loader's Module is held weakly and pinned
        // to its loader by the `layer-modules` root row
        // (`ClassRealm::layer_module_pins`), not rooted for good.
        let handle = match self.layer_module_loader_owned(loader_ns, module) {
            Some(handle) => handle,
            None => self.add_global_root(module),
        };
        if handle == 0 {
            return;
        }
        let classes = &self.shared.classes;
        let mut map = classes.layer_module_packages.write();
        for pkg in packages {
            map.entry(loader_ns).or_default().insert(pkg.as_str().into(), handle);
        }
        drop(map);
        classes
            .layer_module_packages_any
            .store(true, std::sync::atomic::Ordering::Release);
    }

    fn layer_module_of_package(&self, loader_ns: u32, package: &str) -> Option<ObjectRef> {
        // zs s7/nd6 (rv4 residual 1): resolve while the map's read guard is
        // held (map -> JNI), so a pruned row's handle is never resolved after
        // `prune_layer_module_pins` freed it (it drops the rows first).
        let module = {
            let map = self.shared.classes.layer_module_packages.read();
            let handle = *map.get(&loader_ns)?.get(package)?;
            self.resolve_global_root(handle)
        }?;
        // zs s5/nd5: the handle may be a weak one (`layer_module_loader_owned`):
        // a resolved weak referent handed to Java is kept alive first, as
        // `Reference.get()` does (a no-op outside a concurrent mark). On G1
        // and Generational too when they take the weak rows (the SATB
        // barrier: `VmHeap::referent_keep_alive`).
        if layer_module_loader_owned_here(self.shared) {
            self.shared
                .mem
                .heap
                .referent_keep_alive(Value::Object(Some(module)));
        }
        Some(module)
    }

    fn has_layer_module_packages(&self) -> bool {
        self.shared
            .classes
            .layer_module_packages_any
            .load(std::sync::atomic::Ordering::Acquire)
    }

    fn get_system_property(&self, key: &str) -> Option<String> {
        let normalized = normalize_system_property_key(key);
        // SBR5 sysprops: under `--real-jdk` the JDK's own `System.setProperty`
        // writes only the Java `System.props`; read it through (merged with the
        // store, latest writer wins) so every Rust reader sees a run-time write.
        cratonvm_native_builtins::sysprops_merge::resolve(self, normalized, || {
            self.shared
                .system_properties
                .read()
                .get(normalized)
                .cloned()
        })
    }

    fn list_system_properties(&self) -> Vec<(String, String)> {
        let store: Vec<(String, String)> = self
            .shared
            .system_properties
            .read()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        cratonvm_native_builtins::sysprops_merge::list(self, store)
    }

    fn is_command_line_system_property(&self, key: &str) -> bool {
        // Interpreter round i7 wave 139, lane L1b: the launcher's `-D` list.
        self.shared.config.system_properties.iter().any(|(k, _)| k == key)
    }

    fn prefers_real_bytecode(&self, class_name: &str) -> bool {
        crate::runtime::env_cache::real_bytecode_selector().prefers_real(class_name)
    }

    fn is_jdk_only(&self) -> bool {
        self.shared.config.is_jdk_only()
    }

    fn shutdown_hook_timeout_ms(&self) -> u64 {
        self.shared.config.shutdown_hook_timeout_ms
    }

    fn faithful_loader_locks(&self) -> bool {
        self.shared
            .natives
            .native_methods
            .compat_parallel_capable_loaders()
    }

    fn aligned(&self, a: cratonvm_types::HotSpotAlignment) -> bool {
        self.shared.natives.native_methods.aligned(a)
    }

    fn note_alignment_divergence(&self, a: cratonvm_types::HotSpotAlignment) {
        self.shared.classes.note_alignment_divergence(a);
    }

    fn jdk_url_class_loaders(&self) -> bool {
        self.shared.natives.native_methods.jdk_url_class_loaders()
    }

    fn builtin_loader_package_sync(&self) -> bool {
        self.shared
            .natives
            .native_methods
            .builtin_loader_package_sync()
    }

    fn resource_streams_open_url(&self) -> bool {
        self.shared
            .natives
            .native_methods
            .resource_streams_open_url()
    }

    fn defined_class_resources_defining_loader(&self) -> bool {
        self.shared
            .natives
            .native_methods
            .defined_class_resources_defining_loader()
    }

    fn jdk_built_packages(&self) -> bool {
        self.shared.natives.native_methods.jdk_built_packages()
    }
    fn reflect_class_access(&self) -> bool {
        self.shared.natives.native_methods.reflect_class_access()
    }
    fn reflect_invoke_plan(&self) -> bool {
        self.shared.natives.native_methods.reflect_invoke_plan()
    }
    fn reflect_door_cache_slot(
        &self,
    ) -> Option<&std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>> {
        Some(self.shared.natives.native_methods.reflect_door_cache_slot())
    }
    fn field_door_ident_memo(&self) -> bool {
        self.shared.natives.native_methods.field_door_ident_memo()
    }
    fn stack_trace_origin_record(&self) -> bool {
        self.shared
            .natives
            .native_methods
            .stack_trace_origin_record()
    }
    fn stack_trace_origin_record_slot(
        &self,
    ) -> Option<&std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>> {
        Some(
            self.shared
                .natives
                .native_methods
                .stack_trace_origin_record_slot(),
        )
    }
    fn stack_trace_fill_name_from_text(&self) -> bool {
        self.shared
            .natives
            .native_methods
            .stack_trace_fill_name_from_text()
    }
    fn compat_stack_trace_keeps_array(&self) -> bool {
        self.shared
            .natives
            .native_methods
            .compat_stack_trace_keeps_array()
    }
    fn stack_trace_fill_switches(&self) -> cratonvm_native_api::StackTraceFillSwitches {
        self.shared
            .natives
            .native_methods
            .stack_trace_fill_switches()
    }

    fn reflect_invoke_settled(&self) -> bool {
        self.shared.natives.native_methods.reflect_invoke_settled()
    }
    fn reflect_ctor_plan_per_vm(&self) -> bool {
        self.shared
            .natives
            .native_methods
            .reflect_ctor_plan_per_vm()
    }
    fn reflect_door_word_batch(&self) -> bool {
        self.shared.natives.native_methods.reflect_door_word_batch()
    }
    fn reflect_door_phase_stats(&self) -> bool {
        self.shared
            .natives
            .native_methods
            .reflect_door_phase_stats()
    }
    fn reflect_invoke_plan_facts(&self) -> bool {
        self.shared
            .natives
            .native_methods
            .reflect_invoke_plan_facts()
    }
    fn reflect_export_gate_disabled(&self) -> bool {
        self.shared
            .natives
            .native_methods
            .reflect_export_gate_disabled()
    }
    fn caller_sensitive_adapter(
        &self,
        class_id: ClassId,
        name: &str,
        descriptor: &str,
    ) -> Option<String> {
        let cm = self.shared.classes.class_manager.read_recursive();
        crate::runtime::stackwalker::caller_sensitive_adapter_descriptor(
            &cm.class_store,
            class_id,
            name,
            descriptor,
        )
    }
    fn inner_classes_agreement(&self) -> bool {
        self.shared.natives.native_methods.inner_classes_agreement()
    }
    fn boot_append_package_table(&self) -> bool {
        self.shared
            .natives
            .native_methods
            .boot_append_package_table()
    }

    fn note_nested_jar_walk(&self, skipped: bool) {
        let counter = if skipped {
            &self.shared.classes.nested_jar_walk_skipped
        } else {
            &self.shared.classes.nested_jar_walk_asked
        };
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn note_nested_jar_bytes(&self, served: bool) {
        let counter = if served {
            &self.shared.classes.nested_jar_bytes_served
        } else {
            &self.shared.classes.nested_jar_bytes_body
        };
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn note_nested_jar_row(&self, event: cratonvm_native_api::NestedJarRowEvent) {
        use cratonvm_native_api::NestedJarRowEvent as E;
        let slot = match event {
            E::Built => 0,
            E::Rebuilt => 1,
            E::Refused => 2,
            E::RefusalRetried => 3,
            E::RefusalKept => 4,
            E::ClassPathLent => 5,
            E::OuterHandleReused => 6,
            E::JarLoaderClassPathLent => 7,
            E::JarLoaderClassPathDirect => 8,
            E::JarLoaderClassPathDirectFallback => 9,
            E::PooledStatSkipped => 10,
        };
        self.shared.classes.nested_jar_row_events[slot]
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn note_jar_loader_bytes(&self, served: bool) {
        let counter = if served {
            &self.shared.classes.jar_loader_bytes_served
        } else {
            &self.shared.classes.jar_loader_bytes_body
        };
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn note_jvm_define_class(&self, bytes: usize) -> bool {
        // zt t16/pl7: `sun.cls.defineAppClasses` / `appClassBytes`, except
        // for the define a JNI `DefineClass` runs through `defineClass1`
        // (`jni_DefineClass` is no `jvm_define_class_common`).
        if cratonvm_types::perf_class_trace::take_define_uncounted() {
            return false;
        }
        let counters = &self.shared.debug.diagnostic_counters;
        let relaxed = std::sync::atomic::Ordering::Relaxed;
        counters.app_classes_defined.fetch_add(1, relaxed);
        // Widening: usize -> u64 (value preserved).
        counters.app_class_bytes.fetch_add(bytes as u64, relaxed);
        true
    }

    fn note_unsafe_define_class_call(&self) {
        // zt t16/pl7: `sun.cls.unsafeDefineClassCalls`.
        self.shared
            .debug
            .diagnostic_counters
            .unsafe_define_class_calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn class_trace_counters(
        &self,
    ) -> Option<std::sync::Arc<cratonvm_types::perf_class_trace::ClassTraceCounters>> {
        crate::runtime::perf_data_file::class_trace(self.shared)
    }

    fn archive_bytes_cache_slot(
        &self,
    ) -> Option<&std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>> {
        Some(
            self.shared
                .natives
                .native_methods
                .archive_bytes_cache_slot(),
        )
    }

    fn loader_identity_tables(&self) -> Option<&(dyn std::any::Any + Send + Sync)> {
        let tables: &(dyn std::any::Any + Send + Sync) = &self.shared.loader_identity;
        Some(tables)
    }

    fn class_hierarchy_memo_stamp(&self) -> Option<u64> {
        // JIT round 23 wave 6 (`CRATONVM_LOADER_LOAD_CLASS_OVERRIDE_MEMO`):
        // the edge epoch moves on set_superclass / set_interfaces / forward
        // edges / remove (unload); the in-place epoch on a stub upgrade; the
        // redefinition count on any redefinition. All three are monotone.
        let epochs = cratonvm_classloading::store_epochs(self.shared.jit.class_layout_domain);
        Some(
            epochs
                .hierarchy_edge_epoch()
                .wrapping_add(epochs.class_origin_in_place_epoch())
                .wrapping_add(cratonvm_classloading::class_redefinition_count()),
        )
    }

    fn preview_enabled(&self) -> bool {
        self.shared.config.enable_preview
    }

    fn finalization_enabled(&self) -> bool {
        self.shared.config.finalization_enabled
    }

    fn zip_source_slot_cache(
        &self,
    ) -> Option<&std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>> {
        Some(self.shared.natives.native_methods.zip_source_slot_cache())
    }

    // Interpreter round i6 wave 117, lane L4: the record `boolean`-component
    // memo, per VM (was a process-wide `static`).
    fn record_boolean_components_slot(
        &self,
    ) -> Option<&std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>> {
        Some(self.shared.natives.native_methods.record_boolean_components_slot())
    }

    // Interpreter round i7 wave 136, lane L4 (`CRATONVM_VH_REAL_VARFORM`): the
    // hidden form classes' ids and the shared forms' global roots, per VM.
    fn vh_real_varform_slot(
        &self,
    ) -> Option<&std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>> {
        Some(self.shared.natives.native_methods.vh_real_varform_slot())
    }

    // Interpreter round i6 wave 117, lane L4 (`CRATONVM_MH_REAL_DIRECT_HIDDEN_LATCH`):
    // the per-VM latch, conservative in every term (the JVMTI listener union
    // over every VM, any configured agent or debugger).
    fn hidden_frames_never_observed(&self) -> bool {
        let config = &self.shared.config;
        !self.shared.natives.native_methods.hidden_frames_observed()
            && !config.show_hidden_frames
            && config.jvmti_agent_options.is_empty()
            && config.jdwp_port.is_none()
            && config.jdwp_agent.is_none()
            && !crate::runtime::jvmti::any_listener_active()
    }

    // Interpreter round i7 wave 144, lane L1 (`CRATONVM_MH_FORM_FRAMES_UNDER_DEBUGGER`,
    // `VmConfig::mh_form_frames_under_debugger`): a JDWP session of this VM
    // is attached. Two loads; `false` in a build without the JDWP server.
    fn debugger_observes_frames(&self) -> bool {
        #[cfg(feature = "experimental-debug")]
        {
            self.shared.config.mh_form_frames_under_debugger
                && self.shared.debug.debugger_gates.session_attached()
        }
        #[cfg(not(feature = "experimental-debug"))]
        {
            false
        }
    }

    fn note_hidden_frames_observed(&self) {
        self.shared.natives.native_methods.note_hidden_frames_observed();
    }

    fn reference_layout_slot(
        &self,
    ) -> Option<&std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>> {
        Some(self.shared.natives.native_methods.reference_layout_slot())
    }

    fn process_tables_slot(
        &self,
    ) -> Option<&std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>> {
        Some(self.shared.natives.native_methods.process_tables_slot())
    }

    fn nested_jar_shared_outer(&self) -> bool {
        self.shared.natives.native_methods.nested_jar_shared_outer()
    }

    fn nested_jar_outer_index(&self) -> bool {
        self.shared.natives.native_methods.nested_jar_outer_index()
    }

    fn nested_jar_refusal_stamp(&self) -> bool {
        self.shared
            .natives
            .native_methods
            .nested_jar_refusal_stamp()
    }

    fn nested_jar_bytes_directory(&self) -> bool {
        self.shared
            .natives
            .native_methods
            .nested_jar_bytes_directory()
    }

    fn nested_jar_outer_handle_reuse(&self) -> bool {
        self.shared
            .natives
            .native_methods
            .nested_jar_outer_handle_reuse()
    }

    fn nested_jar_pooled_stat(&self) -> bool {
        self.shared.natives.native_methods.nested_jar_pooled_stat()
    }

    fn nested_jar_class_path_outer(&self) -> bool {
        self.shared
            .natives
            .native_methods
            .nested_jar_class_path_outer()
    }

    fn jar_loader_class_path_archive(&self) -> bool {
        self.shared
            .natives
            .native_methods
            .jar_loader_class_path_archive()
    }

    fn jar_loader_class_path_direct(&self) -> bool {
        self.shared
            .natives
            .native_methods
            .jar_loader_class_path_direct()
    }

    fn define_one_parse(&self) -> bool {
        self.shared.natives.native_methods.define_one_parse()
    }

    fn real_builtin_loaders(&self) -> bool {
        self.shared.natives.native_methods.real_builtin_loaders()
    }

    fn define_supertype_dictionary(&self) -> bool {
        self.shared
            .natives
            .native_methods
            .define_supertype_dictionary()
    }

    fn define_format_relaxation(&self) -> (bool, bool) {
        self.shared.classes.class_manager.read().noverify_relaxation()
    }

    fn define_records_supertype_initiation(&self) -> bool {
        self.shared
            .natives
            .native_methods
            .define_records_supertype_initiation()
    }

    fn loader_kind_memo(&self) -> bool {
        self.shared.natives.native_methods.loader_kind_memo()
    }

    fn clv_sub_fast_key(&self) -> bool {
        self.shared.natives.native_methods.clv_sub_fast_key()
    }

    fn loader_namespace_index(&self) -> bool {
        self.shared.natives.native_methods.loader_namespace_index()
    }

    fn platform_resource_jrt_walk(&self) -> bool {
        self.shared
            .natives
            .native_methods
            .platform_resource_jrt_walk()
    }

    fn inflater_reuse_buffers(&self) -> bool {
        self.shared.natives.native_methods.inflater_reuse_buffers()
    }

    fn inflater_input_window(&self) -> bool {
        self.shared.natives.native_methods.inflater_input_window()
    }

    fn inflater_raw_adler_zero(&self) -> bool {
        self.shared.natives.native_methods.inflater_raw_adler_zero()
    }

    fn inflater_sharded_table(&self) -> bool {
        self.shared.natives.native_methods.inflater_sharded_table()
    }

    fn jdk_feature_version(&self) -> Option<u32> {
        self.shared.natives.native_methods.jdk_feature()
    }

    fn arrays_mismatch_bulk(&self) -> bool {
        self.shared.natives.native_methods.arrays_mismatch_bulk()
    }

    fn compat_url_loader_defined_packages(&self) -> bool {
        self.shared
            .natives
            .native_methods
            .compat_url_loader_defined_packages()
    }

    fn string_literal_identity(&self) -> bool {
        self.shared.natives.native_methods.string_literal_identity()
    }

    fn impl_jars_es_loader_only(&self) -> bool {
        self.shared
            .natives
            .native_methods
            .impl_jars_es_loader_only()
    }

    fn image_app_module_loaders(&self) -> bool {
        self.shared
            .natives
            .native_methods
            .image_app_module_loaders()
    }

    fn per_name_loader_locks(&self) -> bool {
        self.shared.natives.native_methods.per_name_loader_locks()
    }

    fn delegates_to_url_parent(&self) -> bool {
        self.shared.natives.native_methods.delegates_to_url_parent()
    }

    fn resolver_absence_shape(&self) -> bool {
        self.shared.natives.native_methods.resolver_absence_shape()
    }

    fn linkage_throwable_of(
        &self,
        error: &cratonvm_types::error::VmError,
    ) -> Option<(&'static str, String)> {
        match error {
            cratonvm_types::error::VmError::Linkage(linkage) => {
                Some(crate::runtime::exceptions::linkage_throwable(linkage))
            }
            _ => None,
        }
    }

    fn url_app_handler_is_authoritative(&self) -> bool {
        self.shared
            .natives
            .native_methods
            .url_app_handler_is_authoritative()
    }

    fn messages_name_user_loaders(&self) -> bool {
        self.shared
            .natives
            .native_methods
            .messages_name_user_loaders()
    }

    fn initiating_records_on(&self) -> bool {
        self.shared.classes.initiating_records_on
    }

    fn loader_drive_under_loader_lock(&self) -> bool {
        self.shared.config.loader_drive_under_loader_lock
    }

    fn lambda_metafactory_reflective_to_jdk(&self) -> bool {
        self.shared.classes.lmf_reflective_to_jdk
    }

    fn mh_combinator_checks(&self) -> bool {
        self.shared.classes.mh_combinator_checks
    }

    fn hotspot_object_sizes(&self) -> bool {
        self.shared.natives.hotspot_object_sizes
    }

    fn hotspot_size_memo(&self, class_id: ClassId, tag: usize) -> Option<u64> {
        self.shared
            .natives
            .hotspot_size_memo
            .lock()
            .get(&class_id.as_u32())
            .filter(|&&(at, _)| at == tag)
            .map(|&(_, size)| size)
    }

    fn remember_hotspot_size(&self, class_id: ClassId, tag: usize, size: u64) {
        self.shared
            .natives
            .hotspot_size_memo
            .lock()
            .insert(class_id.as_u32(), (tag, size));
    }

    fn note_exact_var_handle_minted(&mut self, exact_slot: usize) {
        // `u32::MAX` is the "none minted" sentinel; no class has that many
        // fields.
        if let Ok(slot) = u32::try_from(exact_slot) {
            if slot != u32::MAX {
                // Round 15 wave 3 (lane vh3; VH2-2): the gate is stored FIRST,
                // so no compile from here on binds a thin `VarHandle` helper;
                // then the bodies that bound one before this mint are withdrawn
                // (`r15w2-vh2-thin-helpers-bound-before-first-exact-mint`).
                let before = self
                    .shared
                    .natives
                    .var_handle_exact_slot
                    .swap(slot, std::sync::atomic::Ordering::AcqRel);
                if before == u32::MAX {
                    let _ = crate::jit::helpers::retire_varhandle_thin_bound_bodies(self.shared);
                }
            }
        }
    }

    fn set_system_property(&mut self, key: &str, value: &str) -> Option<String> {
        let normalized = normalize_system_property_key(key).to_string();
        let old = self
            .shared
            .system_properties
            .write()
            .insert(normalized.clone(), value.to_string());
        cratonvm_native_builtins::sysprops_merge::note_store_write(&*self, &normalized, false);
        old
    }

    fn remove_system_property(&mut self, key: &str) -> Option<String> {
        let normalized = normalize_system_property_key(key).to_string();
        let old = self.shared.system_properties.write().remove(&normalized);
        cratonvm_native_builtins::sysprops_merge::note_store_write(&*self, &normalized, true);
        old
    }

    /// Mint a VM-generated class — contract §1 item 6's shapes, which are legal
    /// in every mode.
    ///
    /// Infallible on purpose: `ClassManager::ensure_generated_class` records no
    /// violation and refuses nothing, because a lambda body, a `$ProxyN` and
    /// its `Proxy$Instance` superclass, an array shape or a reflection accessor
    /// is a class a conforming JVM creates without a class file. This is NOT a
    /// way around the policy — see the warning on the trait declaration.
    #[track_caller]
    fn ensure_vm_internal_class(&mut self, name: &str, num_fields: usize) -> ClassId {
        // Same real-class preference as the fallible sibling: a name that
        // resolves to real bytes is not a fabrication at all.
        if let Ok(cid) = self.shared.load_class_concurrent(name) {
            return cid;
        }
        self.shared
            .classes
            .class_manager_write_settling()
            .ensure_generated_class(
                name,
                num_fields,
                cratonvm_classloading::ClassOrigin::VmInternal,
            )
    }

    /// Mint a compatibility stand-in, with the refusal `--real-jdk` requires.
    ///
    /// The infallible `ensure_synthetic_class` twin this used to sit beside was
    /// deleted on 2026-08-10 (Real-JDK wave 2, step 3). It recorded the
    /// violation and fabricated anyway, so:
    ///
    /// 1. this can return [`ClassIdentityError::AmbiguousName`] instead of a
    ///    silently-substituted stand-in, so a native that can fail gets to fail;
    /// 2. it goes through `ClassManager::try_ensure_synthetic_class`, which
    ///    **enforces** `--real-jdk`. Under the default `Compatible` mode the
    ///    behaviour is byte-for-byte what the twin did.
    ///
    /// The error is re-derived from the name index rather than pattern-matched
    /// out of the returned `VmError`: the classifier is the authority on
    /// *which* refusal this is, and matching on an error's shape would make
    /// this mapping quietly wrong the day a third refusal is added.
    #[track_caller]
    fn try_ensure_synthetic_class(
        &mut self,
        name: &str,
        num_fields: usize,
    ) -> Result<ClassId, cratonvm_native_api::ClassIdentityError> {
        use cratonvm_classloading::NameResolution;
        use cratonvm_native_api::ClassIdentityError;

        // Same real-class preference as the infallible spelling: a name that
        // resolves to real bytes is not a fabrication and cannot be ambiguous
        // (the loader answered with one class).
        //
        // Skipped once the class manager has recorded that the classpath has
        // no bytes for `name` (cleared on any classpath extension). The
        // manager's own `try_ensure_synthetic_class` below trusts that memo
        // and still answers an already-loaded class first, so the result is
        // the same; what the skip removes is a full classpath rescan on every
        // call. Under `--real-jdk` a refused name is never registered, so
        // without this every request re-scanned: 12,125 requests for
        // `cratonvm/stream/LazyOp` cost netty's
        // `Http2ConnectionRoundtripTest.stressTest` 1.22 million `statx`
        // misses and 18% of its CPU, much of it under the class manager's
        // write lock that every event-loop thread contends for.
        let known_absent = self
            .shared
            .classes
            .class_manager
            .read()
            .synthetic_class_bytes_known_absent(name);
        if !known_absent {
            if let Ok(cid) = self.shared.load_class_concurrent(name) {
                return Ok(cid);
            }
        }
        let mut manager = self.shared.classes.class_manager_write_settling();
        // Bound to a local so the `&mut` borrow of the guard ends before the
        // classifier's `&` borrow below, rather than relying on the match
        // scrutinee's borrow ending early.
        let fabricated = manager.try_ensure_synthetic_class(name, num_fields);
        match fabricated {
            Ok(id) => Ok(id),
            Err(err) => Err(match manager.classify_loaded_name(name) {
                NameResolution::Ambiguous { definitions } => ClassIdentityError::AmbiguousName {
                    name: name.to_string(),
                    definitions,
                },
                NameResolution::Absent | NameResolution::Unique(_) => ClassIdentityError::Refused {
                    name: name.to_string(),
                    reason: err.to_string(),
                },
            }),
        }
    }

    fn is_interface_class(&self, class_id: ClassId) -> bool {
        self.shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.is_interface())
            .unwrap_or(false)
    }

    fn interface_flag_and_superclass_of(&self, class_id: ClassId) -> (bool, Option<ClassId>) {
        if let Some(cached) = self.shared.classes.class_meta_words.get(class_id) {
            return cached;
        }
        let cm = self.shared.classes.class_manager.read();
        match cm.get_class(class_id) {
            Some(c) => {
                let res = (c.is_interface(), c.superclass);
                self.shared
                    .classes
                    .class_meta_words
                    .publish(class_id, res.0, res.1);
                res
            }
            None => (false, None),
        }
    }

    fn is_abstract_class(&self, class_id: ClassId) -> bool {
        self.shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.is_abstract())
            .unwrap_or(false)
    }

    /// T1.6.7 вЂ” `Thread.holdsLock(Object)` real implementation.
    fn current_thread_holds_lock(&self, obj: ObjectRef) -> bool {
        self.shared
            .threads
            .monitors
            .holds(obj, self.thread.thread_id)
    }

    /// JIT tier2 wave 5, lane nat21 (`NATIVE21-2`): the leaf variant, which
    /// reads the mark word and this thread's caches and takes no lock.
    fn current_thread_holds_lock_lock_free(&self, obj: ObjectRef) -> Option<bool> {
        self.shared
            .threads
            .monitors
            .holds_without_locking(obj, self.thread.thread_id)
    }

    fn jfr_begin_java_recording(&mut self) {
        let mut active = self.shared.debug.jfr_java_recording.lock();
        let mut recorder = self.shared.debug.flight_recorder.lock();
        // A recording that is STILL RUNNING is the one to keep: two concurrent
        // `RecordingStream`s (or a stream alongside a `Recording`) share it, and
        // beginning again must not replace it under the other one.
        //
        // A recording that has already been STOPPED must not be reused, and this
        // used to `return` on `active.is_some()` without looking. `Recording
        // ::start()` only transitions out of `New`, so the stopped recording
        // stayed stopped — and because the early return also skipped the
        // `jfr_java_recording_running` store, `Event.isEnabled()` answered
        // `false` for every recording after the first in a process.
        //
        // That is what made netty's `JfrEventsTest` fail 9 of its 10 tests while
        // the first one to run passed: each test opens its own `RecordingStream`,
        // and only the first got a live recording.
        let reusable = active.filter(|id| {
            recorder.get_recording(*id).map(|rec| rec.state)
                == Some(cratonvm_jfr::recording::RecordingState::Running)
        });
        if reusable.is_none() {
            // Release the finished recording's event ring rather than leaving it
            // in the map for the life of the process.
            if let Some(previous) = *active {
                recorder.discard_recording(previous);
            }
            let id = recorder.new_recording(cratonvm_jfr::RecordingSettings::new("jdk.jfr"));
            recorder.start_recording(id);
            *active = Some(id);
        }
        self.shared
            .debug
            .jfr_java_recording_running
            .store(true, std::sync::atomic::Ordering::Release);
    }

    fn jfr_end_java_recording(&mut self) {
        let id = *self.shared.debug.jfr_java_recording.lock();
        let Some(id) = id else {
            return;
        };
        self.shared
            .debug
            .jfr_java_recording_running
            .store(false, std::sync::atomic::Ordering::Release);
        self.shared.debug.flight_recorder.lock().stop_recording(id);
    }

    fn jfr_java_recording_active(&self) -> bool {
        self.shared
            .debug
            .jfr_java_recording_running
            .load(std::sync::atomic::Ordering::Acquire)
    }

    fn jfr_method_compiled(
        &self,
        class_id: ClassId,
        class_name: &str,
        method: &str,
        descriptor: &str,
    ) -> bool {
        self.shared
            .jit
            .jit_cache
            .read()
            .get(class_name, method, descriptor, class_id)
            .is_some()
    }

    fn jfr_collector_names(&self) -> (&'static str, &'static str) {
        match &self.shared.mem.heap {
            _ if self.shared.mem.heap.is_generational() => ("DefNew", "SerialOld"),
            _ if self.shared.mem.heap.is_g1() => ("G1New", "G1Old"),
            #[allow(unreachable_patterns)]
            _ => ("ZGC Minor", "ZGC Major"),
        }
    }

    /// zs s2/ob: ZGC only (`None` on Generational and G1, whose events keep
    /// their figures), under `CRATONVM_ZGC_JFR_HOTSPOT_CONFIGURATION`
    /// (default ON, `=0` kill).
    fn jfr_zgc_configuration(&self) -> Option<cratonvm_native_api::JfrZgcConfiguration> {
        if matches!(
            self.shared.mem.heap,
            _ if false
        ) || !true {
            return None;
        }
        let c = &self.shared.config;
        let bytes = |v: usize| i64::try_from(v).unwrap_or(i64::MAX);
        let initial = c.initial_heap_size.min(c.max_heap_size);
        let count = |v: Option<usize>| v.and_then(|n| u32::try_from(n).ok());
        Some(cratonvm_native_api::JfrZgcConfiguration {
            min_heap: bytes(crate::runtime::unified_logging::hotspot_min_heap_size(
                c.min_heap_size,
                initial,
            )),
            initial_heap: bytes(initial),
            max_heap: bytes(c.max_heap_size),
            parallel_gc_threads: count(c.zgc_parallel_gc_threads),
            conc_gc_threads: count(c.zgc_conc_gc_threads),
            dynamic_gc_threads: c.zgc_dynamic_gc_threads.unwrap_or(true),
            explicit_gc_disabled: c.explicit_gc == crate::config::ExplicitGcPolicy::Disabled,
            explicit_gc_concurrent: c.explicit_gc == crate::config::ExplicitGcPolicy::Concurrent,
        })
    }

    fn jfr_system_gc_on_caller(&self) -> Option<bool> {
        crate::runtime::interpreter::zgc_jfr_system_gc_on_caller(&self.shared)
    }

    fn jfr_vm_ticks(&self) -> i64 {
        if cratonvm_native_builtins_jfr::jfr_vm_events::any_recording() {
            cratonvm_native_builtins_jfr::jfr_vm_events::ticks_now().max(1)
        } else {
            0
        }
    }

    fn jfr_file_write_done(&mut self, stream: ObjectRef, bytes: i64, start_ticks: i64) {
        cratonvm_native_builtins_jfr::jfr_vm_events::file_write(self, stream, bytes, start_ticks);
    }

    fn jfr_emit_java_event(
        &mut self,
        event_name: &str,
        fields: &[(String, String, Value)],
        start_ns: u64,
        duration_ns: u64,
    ) {
        // Resolve the payload BEFORE taking the recorder lock: reading a
        // `String` field's characters goes back through `self`, and the
        // recorder guard would still be held.
        let mut declared: Vec<cratonvm_jfr::EventField> = Vec::with_capacity(fields.len());
        let mut values = cratonvm_jfr::EventFields::new();
        for (name, descriptor, value) in fields {
            // The descriptor decides the JFR field type; `Value` alone cannot
            // (CratonVM represents boolean/byte/char/short/int all as
            // `Value::Int`). A descriptor with no JFR counterpart — any
            // reference type other than `String`, or an array — is dropped
            // from BOTH the declaration and the payload, so the two stay the
            // same length and the event still carries every field the format
            // can describe.
            let (type_name, encoded) = match descriptor.as_str() {
                "Z" => (
                    "boolean",
                    cratonvm_jfr::EventValue::Boolean(value.as_int().unwrap_or(0) != 0),
                ),
                "B" | "C" | "S" | "I" => (
                    "int",
                    cratonvm_jfr::EventValue::Int(value.as_int().unwrap_or(0)),
                ),
                "J" => (
                    "long",
                    cratonvm_jfr::EventValue::Long(match value {
                        Value::Long(v) => *v,
                        other => other.as_int().unwrap_or(0) as i64,
                    }),
                ),
                "F" => (
                    "float",
                    cratonvm_jfr::EventValue::Float(match value {
                        Value::Float(v) => *v,
                        _ => 0.0,
                    }),
                ),
                "D" => (
                    "double",
                    cratonvm_jfr::EventValue::Double(match value {
                        Value::Double(v) => *v,
                        _ => 0.0,
                    }),
                ),
                "Ljava/lang/String;" => (
                    "string",
                    match value {
                        Value::Object(Some(text)) => match self.read_string(*text) {
                            Some(text) => cratonvm_jfr::EventValue::String(text.into()),
                            None => cratonvm_jfr::EventValue::Null,
                        },
                        _ => cratonvm_jfr::EventValue::Null,
                    },
                ),
                _ => continue,
            };
            declared.push(cratonvm_jfr::EventField::new(name, type_name, ""));
            values.push(encoded);
        }

        let thread_id = self.thread.thread_id.0;
        let mut recorder = self.shared.debug.flight_recorder.lock();
        // `register` is keyed by name and returns the existing id for a name it
        // already knows, so the first commit of an event name fixes its field
        // list for the rest of the process — which is what the chunk writer
        // needs, since one metadata declaration has to describe every instance.
        let type_id = recorder.type_registry.register(cratonvm_jfr::EventType {
            id: cratonvm_jfr::EventTypeId::INVALID,
            name: event_name.to_owned(),
            category: vec!["Java Application".to_owned()],
            description: "Event committed through the real-JDK jdk.jfr.Event bridge".to_owned(),
            fields: declared,
            has_thread: true,
            has_stacktrace: false,
            period: cratonvm_jfr::EventPeriod::BeginEnd,
            threshold: None,
        });
        if type_id.is_invalid() {
            return;
        }
        // A later commit of the same event name whose field count disagrees
        // with the registered declaration would be dropped by the writer; log
        // it here where the name is still in hand instead of leaving the drop
        // unexplained at dump time.
        if let Some(registered) = recorder.type_registry.get(type_id) {
            if registered.fields.len() != values.len() {
                tracing::debug!(
                    event = %event_name,
                    declared = registered.fields.len(),
                    committed = values.len(),
                    "jdk.jfr.Event commit disagrees with the field list registered for this \
                     event name; the event will be dropped from a dump"
                );
            }
        }
        recorder.record_event(cratonvm_jfr::EventInstance {
            type_id,
            start_time: start_ns,
            end_time: start_ns.saturating_add(duration_ns),
            thread_id,
            fields: values,
        });
    }

    fn jfr_configure_java_recording(
        &mut self,
        enabled_names: Option<&[String]>,
        thresholds: &[(String, u64)],
    ) {
        let id = *self.shared.debug.jfr_java_recording.lock();
        let Some(id) = id else {
            return;
        };
        let mut recorder = self.shared.debug.flight_recorder.lock();
        {
            let Some(recording) = recorder.get_recording_mut(id) else {
                return;
            };
            recording.settings.enabled_event_names =
                enabled_names.map(|names| names.iter().cloned().collect());
            recording.settings.event_thresholds_by_name = thresholds
                .iter()
                .map(|(name, nanos)| (name.clone(), *nanos))
                .collect();
        }
        // A14/A8 (2026-09-01): re-arm the `cratonvm.JitCompileDecision` producer
        // gate. That event is armed only when a RUNNING recording names it, and
        // the gate is otherwise refreshed by `refresh_running_ids` — which runs
        // on start, not on a settings change. Every Java-side settings edit
        // funnels through this function (`native-builtins/src/jfr.rs`), and
        // `Recording.enable(...)` changes no recording STATE, so without this
        // line `r.start()` followed by `r.enable(...)` leaves the producer
        // permanently dark while the reverse order works — an argument-order
        // dependence nothing would explain. The `recording` borrow is scoped
        // above so `recorder` is free to be re-borrowed here.
        cratonvm_jfr::jit_decision::sync_jit_decision_gate(&recorder);
    }

    fn jfr_set_java_output(&mut self, path: &str) {
        *self.shared.debug.jfr_java_output.lock() = Some(path.to_owned());
    }

    fn jfr_dump_java_recording(&mut self, path: &str) {
        let id = self.shared.debug.jfr_java_recording.lock().take();
        let Some(id) = id else {
            return;
        };
        self.shared
            .debug
            .jfr_java_recording_running
            .store(false, std::sync::atomic::Ordering::Release);
        let mut recorder = self.shared.debug.flight_recorder.lock();
        if let Err(error) = recorder.dump_recording(id, std::path::Path::new(path)) {
            tracing::warn!(path = %path, %error, "failed to dump Java JFR recording");
        }
        // The id was `take`n above, so nothing can reach this recording again;
        // drop it rather than leaving its event ring alive for the process.
        recorder.discard_recording(id);
    }

    fn emit_virtual_thread_pinned_jfr(&mut self, reason: &'static str) {
        // Interpreter round i5 wave 105, lane L6: `reason` is the event's
        // `blockingOperation`; `pinnedReason` and the start come from the
        // failed yield (`jdk_continuation::pinned_event_cause`).
        if cratonvm_native_builtins_jfr::jfr_vm_events::any_recording() {
            let (pinned_reason, start) =
                super::jdk_continuation::pinned_event_cause(self.shared, &*self.thread);
            cratonvm_native_builtins_jfr::jfr_vm_events::virtual_thread_pinned_with(
                self,
                reason,
                &pinned_reason,
                start,
            );
        }
        // Round-4: pin_reason is a JEP-491 enum-like literal — the underlying
        // `emit_virtual_thread_pinned_event` now requires `&'static str`.
        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        let carrier_id = self.thread.thread_id.0;
        // Virtual threads currently share their carrier's id; this is fine
        // for JFR classification — what matters is the `pin_reason` string.
        let vt_id = carrier_id;
        let mut jfr = self.shared.debug.flight_recorder.lock();
        cratonvm_jfr::builtin::emit_virtual_thread_pinned_event(
            &mut jfr,
            &self.thread.name,
            reason,
            carrier_id,
            vt_id,
            now_ns,
        );
    }

    /// T19_K2 вЂ” Register a native-spawned OS thread with the VM
    /// `ThreadRegistry`. The boxed `JoinHandle<()>` (raw pointer in
    /// `join_handle_ptr`) is taken back as `Box<JoinHandle<()>>` and
    /// transferred into the registry so
    /// `ThreadRegistry::wait_for_non_daemon_threads()` can `.join()`
    /// it during VM shutdown.
    ///
    /// Returns the `ThreadId.0` so the caller can later call
    /// `unregister_native_thread(id)` from inside the spawned thread's
    /// exit path.
    ///
    /// gcj j3/x2 R7: under `CRATONVM_NATIVE_THREAD_TERMINAL_BLOCK` the entry
    /// is registered STARTING (`stw_ready = false`): no census counts it until
    /// its own thread runs [`VmNativeThreadBlocker::publish_os_tid`]. `=0`:
    /// counted from here, as before.
    fn register_native_thread(&mut self, name: &str, daemon: bool, join_handle_ptr: usize) -> u64 {
        let tid = self.shared.threads.thread_registry.next_thread_id();
        {
            self.shared
                .threads
                .thread_registry
                .register_starting_with_daemon(tid, name, None, daemon);
        }
        // obsaudit D1: this runs on the newly-spawned OS thread itself (see
        // the raw `join_handle_ptr` claim below), so binding the JVMTI
        // thread-attribution TLS here correctly attributes every class this
        // `Thread.start()` worker loads to its own `jthread`.
        cratonvm_classloading::set_current_thread_id(
            self.shared.classes.class_layout_domain,
            tid.0,
        );
        // Claim the raw pointer for exactly-once consumption BEFORE
        // reconstructing the Box: if this exact pointer were ever passed twice
        // (a duplicated handle), the second `Box::from_raw` would double-free
        // the allocation. `claim_raw_join_handle` returns `false` on a repeat,
        // and we skip the reconstruction entirely.
        if join_handle_ptr != 0 && claim_raw_join_handle(join_handle_ptr) {
            // SAFETY: the caller built this via
            // `Box::into_raw(Box::new(join_handle))` immediately before
            // the call, and is contractually obliged to pass us the
            // exclusive ownership of that allocation. The claim above
            // guarantees this is the first (and only) reconstruction of
            // this pointer. We take it back and move the `JoinHandle<()>`
            // into the registry, where it lives until
            // `wait_for_non_daemon_threads` joins on it (or the registry is
            // dropped on VM teardown вЂ” in that case the handle is dropped,
            // which detaches the OS thread, matching HotSpot's behaviour for
            // daemon-on-shutdown teardown).
            let boxed: Box<std::thread::JoinHandle<()>> =
                unsafe { Box::from_raw(join_handle_ptr as *mut std::thread::JoinHandle<()>) };
            self.shared
                .threads
                .thread_registry
                .set_join_handle(tid, *boxed);
        }
        tid.0
    }

    fn native_thread_blocker(
        &self,
        thread_id: u64,
    ) -> Option<std::sync::Arc<dyn NativeThreadBlocker>> {
        if thread_id == 0 {
            return None;
        }
        Some(std::sync::Arc::new(VmNativeThreadBlocker {
            shared: self.shared.get_arc(),
            thread_id: ThreadId(thread_id),
        }))
    }

    /// T19_K2 вЂ” Mark a previously-registered native thread dead. Called
    /// from the spawned OS thread's exit path. Idempotent: passing an
    /// unknown id is a no-op (matches `ThreadRegistry::mark_dead`).
    fn unregister_native_thread(&mut self, thread_id: u64) {
        if thread_id == 0 {
            return;
        }
        let tid = crate::threading::jvm_thread::ThreadId(thread_id);
        self.shared.threads.thread_registry.mark_dead(tid);
        self.shared.threads.monitors.release_monitors_held_by(tid);
    }

    /// gcj j2/cm2 (`CRATONVM_NATIVE_THREAD_TERMINAL_BLOCK`; every backend since
    /// gcj j3/rt3 residual 5): the OS thread of a registered native thread could not be spawned
    /// (`xnio_io_thread::spawn_io_thread_with_ctx`,
    /// `vertx_eventloop::spawn_vertx_event_loop_inner`). Its entry was
    /// registered `stw_ready` and alive with no `JoinHandle`, so
    /// `entry_is_stw_live` would count it in every later census and every
    /// later pause would wait for a thread that does not exist. Unregister it
    /// now. `=0`: nothing, as before.
    fn abandon_native_thread(&mut self, thread_id: u64) {
        {
            self.unregister_native_thread(thread_id);
        }
    }

    /// T19_K2 вЂ” Attach a `Box<JoinHandle<()>>` to an already-registered
    /// native thread. Two-phase variant of `register_native_thread` for
    /// callers that need the `ThreadId` before spawning the OS thread.
    fn attach_join_handle_to_native_thread(
        &mut self,
        thread_id: u64,
        join_handle_ptr: usize,
    ) -> bool {
        if thread_id == 0 || join_handle_ptr == 0 {
            return false;
        }
        let tid = crate::threading::jvm_thread::ThreadId(thread_id);
        // Verify the thread is registered before consuming the pointer.
        // `is_alive` returns true on registration and false after
        // `mark_dead` вЂ” either way the entry exists.
        if self
            .shared
            .threads
            .thread_registry
            .thread_name(tid)
            .is_none()
        {
            return false;
        }
        // Claim the raw pointer for exactly-once consumption before
        // reconstructing the Box. A duplicated pointer (already consumed by a
        // prior `register_native_thread` / `attach_*` call) would otherwise be
        // `Box::from_raw`'d a second time and double-freed. On a repeat claim we
        // report failure without touching the (already-owned) allocation.
        if !claim_raw_join_handle(join_handle_ptr) {
            return false;
        }
        // SAFETY: caller built this via `Box::into_raw` and is
        // contractually obliged to pass us the exclusive ownership; the claim
        // above guarantees this is the only reconstruction of this pointer.
        let boxed: Box<std::thread::JoinHandle<()>> =
            unsafe { Box::from_raw(join_handle_ptr as *mut std::thread::JoinHandle<()>) };
        self.shared
            .threads
            .thread_registry
            .set_join_handle(tid, *boxed);
        true
    }

    /// T19_K4 вЂ” Attach a `java.lang.Thread` mirror to an
    /// already-registered native thread so look-ups by `ObjectRef`
    /// resolve and the registry's `alive_thread_objects()` /
    /// `Thread.enumerate()` enumerations include the carrier.
    ///
    /// `thread_id == 0` is rejected (id `0` is the reserved
    /// "main" sentinel and never has a synthetic mirror); unknown
    /// ids return `false`. The store is idempotent вЂ” re-attaching
    /// the same mirror is a no-op as far as the registry is
    /// concerned.
    fn set_native_thread_java_obj(&mut self, thread_id: u64, java_thread_obj: ObjectRef) -> bool {
        if thread_id == 0 {
            return false;
        }
        let tid = crate::threading::jvm_thread::ThreadId(thread_id);
        if self
            .shared
            .threads
            .thread_registry
            .thread_name(tid)
            .is_none()
        {
            return false;
        }
        let java_tid = read_java_thread_tid(self.shared, java_thread_obj).unwrap_or(0);
        self.shared
            .threads
            .thread_registry
            .set_java_thread_obj_with_tid(tid, java_thread_obj, java_tid);
        // SBR round 2 (lane `thread`): a Rust-owned carrier's mirror stands for
        // a live thread (see `populate_current_thread_object`).
        set_thread_state_fields(
            self.shared,
            java_thread_obj,
            Some(live_eetop(tid)),
            Some(THREAD_STATUS_RUNNABLE),
        );
        true
    }

    fn available_processor_count(&self) -> i32 {
        // Container-aware: prefer the cgroup-derived count the launcher stored
        // under `-XX:+UseContainerSupport`; otherwise fall back to the host
        // hardware thread count. (Disabling container support leaves the field
        // `None`, so the toggle is honored here transitively.)
        if let Some(n) = self.shared.config.container_effective_processors {
            return n.max(1) as i32;
        }
        std::thread::available_parallelism()
            .map(|n| n.get() as i32)
            .unwrap_or(1)
    }

    fn hotspot_vm_flags(&self) -> Vec<cratonvm_native_api::hotspot_flags::HotspotVmFlag> {
        // zt t24/x20: with the values a `setVMOption` wrote (`{management}`).
        crate::config::hotspot_flags::resolved_flags_for(self.shared)
    }

    fn set_manageable_hotspot_flag(&self, name: &str, value: u64) -> Option<Result<(), String>> {
        // zt t24/x20 (`CRATONVM_JMX_MANAGEABLE_HEAP_FREE_RATIOS`): HotSpot's
        // `WriteableFlags::set_flag` for the two manageable heap-free ratios,
        // in its order -- the range (`range(0, 100)`), then the constraint
        // (`MinHeapFreeRatio <= MaxHeapFreeRatio`, each against the other's
        // CURRENT value), with `print_flag_error_message_if_needed`'s texts
        // (the range printed without blanks, as it builds it). A value in
        // force is handed to the collector's sizing policy (Generational and
        // G1 read them; ZGC, like HotSpot's, does not).
        // zt t26/x21 (`CRATONVM_HOTSPOT_FLAG_TABLE_ROWS`): `SoftMaxHeapSize`
        // is manageable on every collector in HotSpot 25. Off ZGC (whose heap
        // owns its own row and door) the value is recorded for the readers;
        // `SoftMaxHeapSizeConstraintFunc` refuses one above `MaxHeapSize`,
        // with the text ZGC's door prints (row SM1). A ZGC heap answers
        // `None` here: its row is not writeable when its own door is off.
        if name == "SoftMaxHeapSize" {
            if !crate::config::hotspot_flags::hotspot_flag_table_rows_enabled()
                || self.shared.mem.heap.zgc_soft_max_flag().is_some()
            {
                return None;
            }
            let max = u64::try_from(self.shared.config.max_heap_size).unwrap_or(u64::MAX);
            if value > max {
                return Some(Err(
                    "SoftMaxHeapSize error: value violates its flag's constraint.".to_string(),
                ));
            }
            self.shared.mem.managed_flags.set_soft_max_heap_size(value);
            return Some(Ok(()));
        }
        // zt t28/mf (`CRATONVM_JMX_MANAGEABLE_EFFECT_FLAGS`): a modelled
        // manageable integer row (`HeapDumpGzipLevel`, and -- on every
        // collector since zt t31/x10 -- `FullGCHeapDumpLimit` /
        // `G1PeriodicGCInterval`). HotSpot's
        // `WriteableFlags::set_flag_from_jvalue` narrows the `long` to the
        // flag's type (`jint` / `juint` / `uintx`), then checks the range;
        // only `HeapDumpGzipLevel` has one (`range(0, 9)`).
        if crate::config::hotspot_flags::manageable_effect_flags_enabled()
            && crate::vm::realms::heap_realm::MANAGED_NUMERIC_FLAGS.contains(&name)
        {
            let rows = crate::config::hotspot_flags::resolved_flags(&self.shared.config);
            let row = rows
                .iter()
                .find(|row| row.name == name && row.is_manageable())?;
            // Casts: two's-complement narrowing, as the jvalue conversions.
            let text = match row.ty {
                "int" => {
                    let level = value as i64 as i32;
                    if !(0..=9).contains(&level) {
                        return Some(Err(format!(
                            "{name} error: must have value in range [0...9]"
                        )));
                    }
                    level.to_string()
                }
                "uint" => (value as u32).to_string(),
                "uintx" => value.to_string(),
                _ => return None,
            };
            if !self.shared.mem.managed_flags.set_number_text(name, text) {
                return None;
            }
            // zt t31/x10: a non-zero interval starts G1's periodic service.
            if name == "G1PeriodicGCInterval" {
                crate::runtime::interpreter::gc_and_alloc::note_g1_periodic_interval_written(
                    self.shared,
                    value,
                );
            }
            return Some(Ok(()));
        }
        let rows = crate::config::hotspot_flags::resolved_flags_for(self.shared);
        let current = |flag: &str| -> u64 {
            rows.iter()
                .find(|row| row.name == flag)
                .and_then(|row| row.value.parse::<u64>().ok())
                .unwrap_or(0)
        };
        let violates = match name {
            "MinHeapFreeRatio" => value > current("MaxHeapFreeRatio"),
            "MaxHeapFreeRatio" => value < current("MinHeapFreeRatio"),
            _ => return None,
        };
        if value > 100 {
            return Some(Err(format!(
                "{name} error: must have value in range [0...100]"
            )));
        }
        if violates {
            return Some(Err(format!(
                "{name} error: value violates its flag's constraint."
            )));
        }
        // Cast: `value <= 100` was checked above.
        let ratio = value as u8;
        self.shared.mem.managed_flags.set(name, ratio);
        if name == "MinHeapFreeRatio" {
            self.shared.mem.heap.set_heap_free_ratios(Some(ratio), None);
        } else {
            self.shared.mem.heap.set_heap_free_ratios(None, Some(ratio));
        }
        Some(Ok(()))
    }

    fn set_manageable_hotspot_text_flag(
        &self,
        name: &str,
        value: &str,
    ) -> Option<Result<(), String>> {
        // zt t27/lg1 (`CRATONVM_JMX_MANAGEABLE_EFFECT_FLAGS`): a modelled
        // manageable `bool` / `ccstr` row of this VM's table (the rows the
        // switch adds; `resolved_flags` decides which exist here: the
        // full-collection dumps on every collector since zt t31/x10's
        // `CRATONVM_GC_FULL_GC_HEAP_DUMPS`, on ZGC only under its kill). HotSpot's
        // `WriteableFlags::set_flag` has no range or constraint for any of
        // them; the value is in force for the next reader (the OOM dump,
        // the `SIGQUIT` histogram, the NPE text builders).
        if !crate::config::hotspot_flags::manageable_effect_flags_enabled() {
            return None;
        }
        let rows = crate::config::hotspot_flags::resolved_flags(&self.shared.config);
        let row = rows
            .iter()
            .find(|row| row.name == name && row.is_manageable())?;
        let managed = &self.shared.mem.managed_flags;
        match row.ty {
            "bool" => {
                let on = match value {
                    "true" => true,
                    "false" => false,
                    _ => return None,
                };
                managed.set_bool(name, on).then_some(Ok(()))
            }
            "ccstr" if name == "HeapDumpPath" => {
                managed.set_heap_dump_path(value);
                Some(Ok(()))
            }
            // zt t28/mf: `G1PeriodicGCSystemLoadThreshold` on ZGC (no effect
            // there, as on HotSpot), from `Flag.setDoubleValue`; HotSpot's
            // `range(0.0, (double)max_uintx)`.
            "double" => {
                let number = value.parse::<f64>().ok()?;
                if !(0.0..=u64::MAX as f64).contains(&number) {
                    return Some(Err(format!(
                        "{name} error: must have value in range [0.000...18446744073709551616.000]"
                    )));
                }
                managed
                    .set_number_text(name, crate::config::hotspot_flags::double_flag_text(number))
                    .then_some(Ok(()))
            }
            _ => None,
        }
    }

    fn max_heap_bytes(&self) -> i64 {
        // Report the configured `-Xmx`, which the launcher already sized from
        // the cgroup memory limit when running container-aware. Honest and
        // container-correct in place of the old hardcoded 256 MiB.
        //
        // gen r5w2/obs6 (2026-09-26): less what the heap can never allocate,
        // where the backend says so — on Generational the young copy reserve
        // (`VmHeap::max_usable_bytes`, HotSpot Serial's `max_capacity()`,
        // which is `-Xmx` less one survivor space). G1 and ZGC answer `None`
        // and keep `-Xmx`. Clamped to `-Xmx` so a heap built larger than the
        // configured budget can never report more than it.
        let xmx = self.shared.config.max_heap_size;
        let max = self
            .shared
            .mem
            .heap
            .max_usable_bytes()
            .map_or(xmx, |usable| usable.min(xmx));
        i64::try_from(max).unwrap_or(i64::MAX)
    }

    fn initial_heap_bytes(&self) -> i64 {
        // Report the configured `-Xms`, mirroring `max_heap_bytes`'s use of
        // the real configured value in place of a hardcoded placeholder.
        self.shared.config.initial_heap_size as i64
    }

    /// gci i42/mx: HotSpot's `[MinHeapSize, InitialHeapSize, MaxHeapSize]`,
    /// exactly as the `gc,init` block computes them (`vm_init.rs`, gcg g3/x):
    /// the configured `-Xmx` itself (without the copy reserve
    /// `max_heap_bytes` takes off), `-Xms` clamped to it, and
    /// `hotspot_min_heap_size`.
    fn hotspot_heap_sizes(&self) -> [i64; 3] {
        let config = &self.shared.config;
        gci_i42_mx_hotspot_heap_sizes(
            config.min_heap_size,
            config.initial_heap_size,
            config.max_heap_size,
        )
    }

    /// gci i42/mx: the explicit-GC flags `VmConfig::explicit_gc` folds
    /// ([`gci_i42_mx_explicit_gc_flags`]). gci i43/lf: with the concurrent
    /// flag's own value (`VmConfig::explicit_gc_invokes_concurrent`).
    fn explicit_gc_flags(&self) -> (bool, bool) {
        let config = &self.shared.config;
        gci_i42_mx_explicit_gc_flags(config.explicit_gc, config.explicit_gc_invokes_concurrent)
    }

    /// gci i43/lf: `(UseTLAB, MinTLABSize, TLABRefillWasteFraction)` from
    /// `VmConfig::tlab_flags` ([`gci_i43_lf_tlab_configuration`]).
    fn hotspot_tlab_configuration(&self) -> (bool, i64, i64) {
        gci_i43_lf_tlab_configuration(&self.shared.config.tlab_flags)
    }

    /// gci i12/ul F4: the classes HotSpot's `ClassLoadingMXBean` counts
    /// (InstanceKlasses: no array classes, no compatibility stubs), coherent
    /// with `unloaded_class_count` (`memory::gc::count_reported_unloads`). It
    /// was every live class in the store.
    fn loaded_class_count(&self) -> usize {
        self.shared
            .classes
            .class_manager
            .read()
            .loaded_instance_class_count()
    }

    /// The JMX non-heap `MemoryUsage` source (gc-common w3-f, 2026-09-23):
    /// the same measured class-metadata lower bound the serviceability heap
    /// summary reports, so the bean and `jcmd`-style summaries cannot
    /// disagree. One class-store walk under the class-manager read lock (plus
    /// the heap summary's young/old samples), on a JMX call — never a hot path.
    fn class_metadata_bytes(&self) -> Option<u64> {
        use crate::runtime::serviceability::VmDiagnosticState;
        Some(VmDiagnosticState::heap_summary(self.shared).metaspace_used)
    }

    /// gci i33/ms: the `Metaspace` / `Compressed Class Space` pools' source
    /// -- `vm_init::metaspace_stats`, one class-store walk under the
    /// class-manager read lock; `None` in the no-class-space shape
    /// (`CRATONVM_METASPACE_HOTSPOT_SPACES=0` / `..._HOTSPOT_FIGURE=0`), where
    /// the pools read [`Self::class_metadata_bytes`] as before.
    ///
    /// zt t1/ob: through `vm_init::class_metadata_walk_figures`, the walk
    /// ZGC's `GcInfo` edge samples take, so a pool bean and a `GcInfo` state
    /// the same `committed` and class-space figures for the same classes.
    fn metaspace_space_figures(&self) -> Option<[u64; 5]> {
        let f = crate::vm::vm_init::class_metadata_walk_figures(
            &self.shared.classes.class_manager.read(),
        );
        let (class_used, class_committed, class_reserved) = f.class_space?;
        Some([f.used, f.committed, class_used, class_committed, class_reserved])
    }

    /// gci i15/nh: this VM's `-XX:MaxMetaspaceSize` (the limit the define path
    /// enforces), the `Metaspace` pool's `max`.
    fn max_metaspace_bytes(&self) -> Option<u64> {
        self.shared
            .classes
            .class_manager
            .read()
            .max_metaspace_bytes()
    }

    /// gci i15/nh: the `CodeHeap` pools' source -- the JIT's live mapped code
    /// (`COMMITTED_JIT_CODE_BYTES`: method bodies, OSR trampolines and deopt
    /// stubs together, the quantity the code-cache cap bounds) and that cap.
    /// Process-wide, as HotSpot's code cache is; two relaxed loads.
    fn jit_code_cache_bytes(&self) -> Option<(u64, Option<u64>)> {
        // gci i16/nt: one reader, shared with the threshold check.
        Some(jmx_code_cache_bytes())
    }

    /// zt t3/ob: HotSpot's per-code-heap `init` on ZGC
    /// (`BackendGcBeans::hotspot_code_heap_init_bytes`). zt t30/nh: HotSpot's
    /// code cache layout on every collector ([`jmx_code_cache_layout`]).
    fn code_cache_layout(&self) -> Option<cratonvm_types::code_cache_layout::CodeCacheLayout> {
        jmx_code_cache_layout(self.shared)
    }

    fn unloaded_class_count(&self) -> u64 {
        self.shared
            .debug
            .diagnostic_counters
            .classes_unloaded
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    fn gc_collection_count(&self) -> u64 {
        self.shared.mem.heap.collection_count()
    }

    /// gci i1/wr (`r15w4-mh2-address-memo-epoch-misses-g1-cleanup-patch`, step
    /// 4): the address-memo epoch, which on G1 also moves at `cleanup`.
    fn gc_address_epoch(&self) -> u64 {
        // gci i2/wr: the Generational concurrent sweep frees old blocks between
        // pauses and books no collection, so its slice count moves the epoch too
        // (0 on G1 / ZGC: the Generational marker is its only writer).
        self.shared.mem.address_epoch()
    }

    /// Real accumulated pause time where the backend keeps one
    /// (`VmHeap::collection_time_ms`: Generational since gen r4/plumbing, G1
    /// since gc-common w5-f), the whole milliseconds of the microsecond sum as
    /// on HotSpot (gce e1/o). ZGC keeps no pause sum of its own, so it answers
    /// the GC event plumbing's backend-independent one
    /// (`HeapRealm::gc_pause_ns_total`, gc-common w6-f), rounded the same way.
    fn gc_collection_time_ms(&self) -> u64 {
        let mem = &self.shared.mem;
        mem.heap.collection_time_ms().unwrap_or_else(|| {
            cratonvm_gc::vm_heap::pause_sum_as_collection_time_ms(
                mem.gc_pause_ns_total
                    .load(std::sync::atomic::Ordering::Relaxed)
                    / 1_000,
            )
        })
    }

    /// gen r4w2/obs (2026-09-23): HotSpot Serial's two collector beans on the
    /// generational backend (`GenerationalHeap::jmx_collectors`).
    ///
    /// gc-common w7-f: HotSpot's own beans on G1 (`G1 Young Generation` /
    /// `G1 Old Generation` / `G1 Concurrent GC`) and ZGC (HotSpot 25's
    /// `ZGC Minor/Major Cycles/Pauses`, from `HeapRealm::backend_gc_beans`),
    /// counted by the common GC event plumbing
    /// (`cratonvm_gc::gc_metrics::BackendGcBeans`). Empty — the historical
    /// single `G1 Young Generation` bean in `jmx.rs` — only where no state
    /// exists.
    fn gc_collectors(&self) -> Vec<cratonvm_native_api::GcCollectorInfo> {
        let info =
            |c: &cratonvm_gc::gc_metrics::JmxCollector| cratonvm_native_api::GcCollectorInfo {
                name: c.name,
                count: c.count,
                time_ms: c.time_ms,
                pool_names: c.pools,
            };
        match self.shared.mem.heap.gen_jmx_collectors() {
            Some(collectors) => collectors.iter().map(info).collect(),
            None => self
                .shared
                .mem
                .backend_gc_beans()
                .map_or_else(Vec::new, |b| b.collectors().iter().map(info).collect()),
        }
    }

    /// gen r4w2/obs (2026-09-23): `Eden Space` / `Survivor Space` /
    /// `Tenured Gen` with real usage on the generational backend
    /// (`GenerationalHeap::jmx_memory_pools`).
    ///
    /// gc-common w7-f: `G1 Eden Space` / `G1 Survivor Space` / `G1 Old Gen`
    /// from one region census, and ZGC's `ZGC Young Generation` /
    /// `ZGC Old Generation` (`JmxPoolSampler::sample`),
    /// with the collection usage the last collection's seal stamped. Empty
    /// (the historical pools) only where no bean state exists.
    fn gc_memory_pools(&self) -> Vec<cratonvm_native_api::GcMemoryPoolInfo> {
        let bytes = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
        let opt = |v: Option<u64>| v.map_or(-1, bytes);
        // gci i14/mp finding 3: `collection_committed` is the committed at the
        // end of the last collection that affected the pool (`-1`: none yet).
        let info = |p: &cratonvm_gc::gc_metrics::JmxPoolUsage, collection_committed: Option<u64>| {
            cratonvm_native_api::GcMemoryPoolInfo {
                name: p.name,
                init: opt(p.init),
                used: bytes(p.used),
                committed: bytes(p.committed),
                max: opt(p.max),
                collection_used: opt(p.collection_used),
                collection_committed: opt(collection_committed),
                manager_names: p.managers,
            }
        };
        let heap = &self.shared.mem.heap;
        match heap {
            _ if self.shared.mem.heap.is_generational() => {
                let committed = self.shared.mem.heap.gen_jmx_collection_committed().unwrap();
                self.shared.mem.heap.gen_jmx_memory_pools().unwrap()
                    .iter()
                    .enumerate()
                    .map(|(i, p)| info(p, committed.get(i).copied().flatten()))
                    .collect()
            }
            _ => self
                .shared
                .mem
                .backend_gc_beans()
                .map_or_else(Vec::new, |b| {
                    let committed = b.collection_committed();
                    b.pools(&heap.backend_pool_samples())
                        .iter()
                        .enumerate()
                        .map(|(i, p)| info(p, committed.get(i).copied().flatten()))
                        .collect()
                }),
        }
    }

    /// zs s3/ob2: ZGC's non-heap `Metaspace` pool (`Metaspace Manager`),
    /// under `CRATONVM_ZGC_JMX_METASPACE_POOL` (default ON, `=0` kill; never
    /// on G1, whose bean state has the switch off, nor on Generational, which
    /// has no backend bean state). `used` is the measured class metadata
    /// `getNonHeapMemoryUsage()` and the `gc,exit` `Metaspace` line report
    /// (one class-store walk per loaded-class census); `init = 0`, `committed = used`,
    /// `max` = `-XX:MaxMetaspaceSize` or `-1` (HotSpot 25.0.3: `init = 0`,
    /// `max = -1`, `67108864` at `-XX:MaxMetaspaceSize=64m`).
    ///
    /// Not modelled, so not listed: HotSpot's three `CodeHeap '...'` pools
    /// (`CodeCacheManager`; the JIT keeps one unsegmented arena) and
    /// `Compressed Class Space` (no compressed class pointers here).
    ///
    /// zt t1/ob (the merge decision): only under
    /// `CRATONVM_ZGC_JMX_HOTSPOT_NON_HEAP_POOLS=0`. By default ZGC models no
    /// non-heap pool of its own (empty, no walk): `jmx.rs` then lists gci
    /// i15/nh's five HotSpot pools and two plain managers, as on every other
    /// backend and as HotSpot 25 ZGC lists them, with gci i33/ms's figures
    /// (the `Metaspace` committed by granule and a real `Compressed Class
    /// Space`), so the per-pool beans and `getNonHeapMemoryUsage()` (their
    /// sum) agree. The ZGC `GcInfo` edge samples (zs s3/ob2) stay on.
    fn non_heap_memory_pools(&self) -> Vec<cratonvm_native_api::GcMemoryPoolInfo> {
        let Some(b) = self.shared.mem.backend_gc_beans() else {
            return Vec::new();
        };
        if b.hotspot_non_heap_pools_enabled() {
            return Vec::new();
        }
        // The collection edges' memo (one walk per loaded-class census), so
        // a pool query and the next `GcInfo` agree.
        let Some(p) = crate::vm::vm_init::zgc_metaspace_pool_now(&self.shared, b) else {
            return Vec::new();
        };
        let bytes = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
        vec![cratonvm_native_api::GcMemoryPoolInfo {
            name: p.name,
            init: p.init.map_or(-1, bytes),
            used: bytes(p.used),
            committed: bytes(p.committed),
            max: p.max.map_or(-1, bytes),
            collection_used: -1,
            // A non-heap pool has no collection usage (gci i14/mp's field).
            collection_committed: -1,
            manager_names: p.managers,
        }]
    }

    /// zs s3/ob2: from now on ZGC's collection edges sample the class
    /// metadata for the `GcInfo`s' `Metaspace` key.
    ///
    /// zs s6/ob5: and the collections that ran before now get their
    /// `Metaspace` key (`CRATONVM_ZGC_METASPACE_FIRST_CYCLES`): the
    /// `GcInfoBuilder` being built names the pool.
    fn note_non_heap_pools_published(&self) {
        if let Some(b) = self.shared.mem.backend_gc_beans() {
            b.note_metaspace_published();
            crate::vm::vm_init::zgc_metaspace_backfill(&self.shared, true);
        }
    }

    /// gen r5w2/obs6 (2026-09-26): the heap's pool peaks as sampled at every
    /// collection edge (`GcNotificationQueue::pool_peak`, fed by
    /// `gc_events.rs::gc_bean_snapshot`; zs s2/ob: ZGC's edges through
    /// `BackendGcBeans::note_pool_peaks`, `CRATONVM_ZGC_POOL_PEAK_AT_GC`).
    ///
    /// gci i14/mp (`gci-i14mp-memory-pool-and-gc-mxbean-review-findings-FIXED-20261016`
    /// finding 1): on G1 / ZGC too (their bean state's queue; `None` here made
    /// a G1 pool's peak its usage at the query, so `G1 Eden Space`'s peak read
    /// ~0 after any collection and `G1 Old Gen`'s FELL when the old pool
    /// shrank). A query's own sample is recorded by
    /// [`Self::record_gc_pool_peak`] (finding 13: with the caller's sample).
    fn gc_pool_peak(&self, pool: &str) -> Option<(i64, i64)> {
        let bytes = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
        let (queue, _) = jmx_peak_queue(self.shared)?;
        queue
            .pool_peak(pool)
            .map(|(used, committed)| (bytes(used), bytes(committed)))
    }

    /// gci i14/mp findings 1 and 13: `getPeakUsage()` records the sample it
    /// took first -- HotSpot's `MemoryPool::get_peak_memory_usage` runs
    /// `record_peak_memory_usage` before it answers, so a peak seen by one
    /// query is kept by the next (the real JDK's `MemoryPoolImpl` has no field
    /// to keep it in) -- without taking a second sample of its own.
    fn record_gc_pool_peak(&self, pool: &str, used: i64, committed: i64) -> Option<(i64, i64)> {
        let (queue, names) = jmx_peak_queue(self.shared)?;
        // gci i15/nh: the non-heap pools' peaks live in the same queue (only
        // ever raised here: no collection edge samples them; zs s6/ob5: on
        // ZGC the `Metaspace` edges do, `CRATONVM_ZGC_METASPACE_PEAK_AT_GC`).
        let name: &'static str = names
            .iter()
            .copied()
            .chain(cratonvm_native_api::registry::JMX_ANY_NON_HEAP_POOL_NAMES.iter().copied())
            .find(|n| *n == pool)?;
        let unsigned = |v: i64| u64::try_from(v).unwrap_or(0);
        queue.note_pool_peak(name, unsigned(used), unsigned(committed));
        self.gc_pool_peak(name)
    }

    /// gen r5w2/obs6: `resetPeakUsage()` resets the collector-sampled peak to
    /// the pool's usage now (one pool sample). gci i14/mp: on G1 / ZGC too.
    fn reset_gc_pool_peak(&self, pool: &str) {
        let heap = &self.shared.mem.heap;
        let (queue, pools) = match heap {
            _ if self.shared.mem.heap.is_generational() => (
                self.shared.mem.heap.gen_gc_notifications().unwrap(),
                self.shared.mem.heap.gen_jmx_memory_pools().unwrap().to_vec(),
            ),
            _ => {
                let Some(b) = self.shared.mem.backend_gc_beans() else {
                    return;
                };
                (b.notifications(), b.pools(&heap.backend_pool_samples()))
            }
        };
        if let Some(p) = pools.iter().find(|p| p.name == pool) {
            queue.reset_pool_peak(p.name, p.used, p.committed);
        } else if let Some(name) = cratonvm_native_api::registry::JMX_ANY_NON_HEAP_POOL_NAMES
            .iter()
            .copied()
            .find(|n| *n == pool)
        {
            // gci i15/nh: a non-heap pool's peak is raised only by queries
            // (and, zs s6/ob5, ZGC's `Metaspace` collection edges), and every
            // `getPeakUsage()` records its own sample before it answers
            // (`record_gc_pool_peak`), so resetting to zero answers the usage
            // at the next query: HotSpot's "peak = usage now".
            queue.reset_pool_peak(name, 0, 0);
        }
    }

    /// gen r5w3/obs7 (2026-09-26): the generational backend checks HotSpot
    /// Serial's thresholds (`gc_metrics::serial_pool_threshold_support`:
    /// `Tenured Gen` both, eden and survivor the collection-usage one). gcd
    /// d10/o retirement (2026-10-04): G1 and ZGC check HotSpot 25's
    /// (`gc_metrics::backend_pool_threshold_support`: the old pool both, the
    /// young ones the collection-usage one), through their bean state; they
    /// answered `false` for every pool until then.
    fn gc_pool_threshold_support(&self, pool: &str) -> (bool, bool) {
        if self.shared.mem.heap.is_generational() {
            cratonvm_gc::gc_metrics::serial_pool_threshold_support(pool)
        } else if self.shared.mem.backend_gc_beans().is_some() {
            cratonvm_gc::gc_metrics::backend_pool_threshold_support(pool)
        } else {
            (false, false)
        }
    }

    /// gen r5w3/obs7: arm a generational pool's threshold
    /// (`GcNotificationQueue::pool_thresholds`). Setting a usage threshold
    /// also checks the pool's usage now, as HotSpot's `jmm_SetPoolThreshold`
    /// runs `LowMemoryDetector::detect_low_memory(pool)`; a crossing is
    /// handed to the delivery thread immediately, without waiting for an
    /// allocation or collection by the setting thread.
    ///
    /// gcd d10/o retirement: on G1 and ZGC too, against their bean state's
    /// pools and threshold table (`BackendGcBeans::notifications`), which the
    /// common GC event plumbing already checks at every collection's end.
    fn set_gc_pool_threshold(&self, pool: &str, collection: bool, threshold: i64) -> bool {
        use cratonvm_gc::gc_metrics::{
            backend_pool_threshold_support, serial_pool_threshold_support, PoolThresholdKind,
        };
        // gci i16/nt: a non-heap pool (`Metaspace`, the code heaps) arms the
        // same per-heap table under its `'static` name. Usage threshold only
        // (HotSpot builds them with the collection threshold unsupported, and
        // the JDK refuses that call before it gets here). Then sample at once,
        // as `jmm_SetPoolThreshold` runs `detect_low_memory(pool)`.
        if let Some(name) = cratonvm_native_api::registry::JMX_ANY_NON_HEAP_POOL_NAMES
            .iter()
            .copied()
            .find(|n| *n == pool)
        {
            if collection {
                return false;
            }
            let Some((queue, _)) = jmx_peak_queue(self.shared) else {
                return false;
            };
            let bytes = u64::try_from(threshold).unwrap_or(0);
            queue
                .pool_thresholds()
                .set(name, PoolThresholdKind::Usage, bytes);
            jmx_check_non_heap_thresholds(self.shared, NonHeapThresholdCheck::Armed);
            // zt t1/ob (merge regression): zs s9/ob6's hand-off, which the
            // heap arm below makes, was lost on this arm when the merge sent
            // ZGC's `Metaspace` here: a crossing found at the set waited for
            // the setting thread's next allocation door
            // (`Zs9Ob6ThresholdWhileAsleep` sleeps only: `notified=0`, HotSpot
            // `notified=1`). ZGC uses its diagnostic switch;
            // `BackendGcBeans::threshold_set_drain_enabled` enables G1.
            // gcj j1/ob: and on Generational, whose queue answers
            // `CRATONVM_GEN_THRESHOLD_SET_DRAIN` (false on every backend's).
            if queue.pool_thresholds().has_pending()
                && (queue.serial_threshold_set_drain_enabled()
                    || self
                        .shared
                        .mem
                        .backend_gc_beans()
                        .is_some_and(|b| b.threshold_set_drain_enabled()))
            {
                let _ = crate::runtime::interpreter::gc_and_alloc::hand_set_time_threshold_crossing_to_delivery_thread(
                    &self.shared,
                );
            }
            return true;
        }
        let heap = &self.shared.mem.heap;
        let backend = self.shared.mem.backend_gc_beans();
        // gcj j1/ob (`CRATONVM_GEN_THRESHOLD_SET_DRAIN`, read once per heap):
        // the Generational half of zs s9/ob6's set-time hand-off below.
        let gen_set_drain = match heap {
            _ if self.shared.mem.heap.is_generational() => {
                self.shared.mem.heap.gen_gc_notifications().unwrap().serial_threshold_set_drain_enabled()
            }
            _ => false,
        };
        let (pools, thresholds, support): (Vec<_>, _, fn(&str) -> (bool, bool)) = match heap {
            _ if self.shared.mem.heap.is_generational() => (
                self.shared.mem.heap.gen_jmx_memory_pools().unwrap().to_vec(),
                self.shared.mem.heap.gen_gc_notifications().unwrap().pool_thresholds(),
                serial_pool_threshold_support,
            ),
            _ => {
                let Some(b) = backend else {
                    return false;
                };
                // zs s6/ob5's ZGC `Metaspace` usage threshold is armed by the
                // non-heap path above (gci i16/nt, every backend); ZGC's
                // collection ends still check it (`gc_events.rs`,
                // `CRATONVM_ZGC_METASPACE_THRESHOLD`).
                (
                    b.pools(&heap.backend_pool_samples()),
                    b.notifications().pool_thresholds(),
                    backend_pool_threshold_support,
                )
            }
        };
        let Some(p) = pools.iter().find(|p| p.name == pool) else {
            return false;
        };
        let (usage_ok, collection_ok) = support(p.name);
        let kind = if collection {
            PoolThresholdKind::Collection
        } else {
            PoolThresholdKind::Usage
        };
        if !(if collection { collection_ok } else { usage_ok }) {
            return false;
        }
        // The Java side refuses a negative threshold before calling in.
        let bytes = u64::try_from(threshold).unwrap_or(0);
        thresholds.set(p.name, kind, bytes);
        if kind == PoolThresholdKind::Usage {
            thresholds.check(&pools, &[]);
            // gci i16/nt: a crossing found at the set is delivered at the next
            // drain point, not only after the next occupancy poll or
            // collection (the hint every queued notification uses).
            if thresholds.has_pending() {
                self.shared.mem.gc_barrier.note_ref_work_queued();
                // zs s9/ob6: and on ZGC (`CRATONVM_ZGC_THRESHOLD_SET_DRAIN`)
                // the delivery thread is asked now, so a thread that never
                // reaches an allocation door (asleep, or in compiled code) is
                // not what delivery waits for. gcj j1/ob: and on Generational.
                if gen_set_drain || backend.is_some_and(|b| b.threshold_set_drain_enabled()) {
                    let _ = crate::runtime::interpreter::gc_and_alloc::hand_set_time_threshold_crossing_to_delivery_thread(
                        &self.shared,
                    );
                }
            }
        }
        true
    }

    /// gen r4w3/obs2 (2026-09-23): per-bean notification enablement on the
    /// generational backend (`GenerationalHeap::gc_notifications`); gc-common
    /// w7-f: and on G1 / ZGC, through their bean state's queue. `false` (the
    /// trait default: no GC notifications) only where no state exists.
    fn set_gc_notification_enabled(&self, collector: &str, enabled: bool) -> bool {
        match self.shared.mem.heap.gen_gc_notifications() {
            Some(n) => n.set_enabled(collector, enabled),
            None => self
                .shared
                .mem
                .backend_gc_beans()
                .is_some_and(|b| b.notifications().set_enabled(collector, enabled)),
        }
    }

    /// gen r4w3/obs2 (2026-09-23): `getLastGcInfo()`'s data on the
    /// generational backend; gc-common w7-f: and on G1 / ZGC. `None` (the
    /// trait default) only where no bean state exists.
    fn gc_last_collection(
        &self,
        collector: &str,
    ) -> Option<cratonvm_native_api::GcNotificationInfo> {
        let rec = match self.shared.mem.heap.gen_gc_notifications() {
            Some(n) => n.last(collector),
            None => self
                .shared
                .mem
                .backend_gc_beans()
                .and_then(|b| b.notifications().last(collector)),
        }?;
        Some(gc_notification_info_of(&rec))
    }

    /// An EXPLICIT GC (`System.gc()`, `Runtime.gc()`, `MemoryMXBean.gc()`,
    /// `Bits.reserveMemory`'s `System.gc()`): honours this VM's
    /// `-XX:±DisableExplicitGC` / `-XX:±ExplicitGCInvokesConcurrent`
    /// (`VmConfig::explicit_gc`, gc-common w8-g). A VM-internal caller that
    /// needs a collection whatever those flags say must not use this.
    fn jvmti_vm_object_alloc(&mut self, obj: ObjectRef) -> ObjectRef {
        // Interpreter round i2 wave 63, lane L1: one relaxed load when no C
        // JVMTI env listens.
        #[cfg(feature = "experimental-debug")]
        if self
            .shared
            .debug
            .debugger_gates
            .native_event_armed(crate::debug::NativeEvent::VmObjectAlloc)
        {
            return crate::jvmti::native_env::post_vm_object_alloc(self.shared, self.thread, obj);
        }
        obj
    }

    /// gcj j1/jt: the same one relaxed load as [`Self::jvmti_vm_object_alloc`].
    fn jvmti_vm_object_alloc_armed(&self) -> bool {
        #[cfg(feature = "experimental-debug")]
        {
            let gates = &self.shared.debug.debugger_gates;
            gates.native_event_armed(crate::debug::NativeEvent::VmObjectAlloc)
        }
        #[cfg(not(feature = "experimental-debug"))]
        {
            false
        }
    }

    /// Interpreter round i9: the per-VM answer, read from the config.
    fn vm_jit_enabled(&self) -> bool {
        self.shared.config.jit_enabled
    }

    fn force_gc(&mut self) {
        // gci i23/pk (`gci-i19ob-jfr-gc-row-field-residuals-FIXED-20261017` item
        // 3): the request's `jdk.SystemGC` stack, walked here while the
        // caller's frames are; one relaxed load when no backend records.
        let jfr_stack = cratonvm_native_builtins_jfr::jfr_vm_more::system_gc_stack_trace(&mut *self);
        crate::runtime::interpreter::force_gc_from_native_with_jfr_stack(
            self.shared,
            self.thread,
            jfr_stack,
        );
        // zt t10/pl2: the in-process perf counters show this collection when
        // the call returns, as HotSpot's do (`CRATONVM_PERF_EVENT_REFRESH`).
        crate::runtime::perf_data_file::refresh_after_collection(self.shared);
    }

    /// A VM-internal collection (gc-common w8-g): the full collection and its
    /// drains whatever `VmConfig::explicit_gc` says.
    fn force_gc_for_vm(&mut self) {
        crate::runtime::interpreter::force_gc_for_vm(self.shared, self.thread);
    }

    /// gci i27/ob: JVMTI `ForceGarbageCollection` on its own door (cause
    /// `JvmtiEnv ForceGarbageCollection`).
    fn force_gc_for_jvmti(&mut self) {
        crate::runtime::interpreter::force_gc_for_jvmti(self.shared, self.thread);
    }

    /// gci i28/dg (`gci-i14mp-...` item 11b): the attach listener's command
    /// table as the `DiagnosticCommand` MBean lists it. Empty when this VM
    /// has no table (`-XX:+DisableAttachMechanism` without a dispatcher).
    fn dcmd_names(&mut self) -> Vec<String> {
        let table = self
            .shared
            .debug
            .jcmd_processor
            .lock()
            .as_ref()
            .map(|p| std::sync::Arc::clone(&p.attach_listener.commands));
        let Some(table) = table else {
            return Vec::new();
        };
        let names = crate::runtime::serviceability::dcmd_mbean_names(&table.read());
        names
    }

    /// gci i28/dg: one command's MBean description.
    fn dcmd_info(&mut self, name: &str) -> Option<cratonvm_native_api::registry::DcmdInfo> {
        let table = self
            .shared
            .debug
            .jcmd_processor
            .lock()
            .as_ref()
            .map(|p| std::sync::Arc::clone(&p.attach_listener.commands))?;
        let info = crate::runtime::serviceability::dcmd_mbean_info(&table.read(), name);
        info
    }

    /// gci i28/dg: `executeDiagnosticCommand`. The command runs on an OS
    /// thread of its own, which the VM does not know -- exactly the attach
    /// listener's thread, the one every command handler is written for (a
    /// `GC.run` there takes `run_dcmd_full_gc_on_attach_thread`'s attached
    /// carrier; on THIS Java thread it would take the latch path and wait for
    /// a mutator, i.e. for itself). Meanwhile this thread waits GC-safe in a
    /// blocking region that reads `RUNNABLE` under
    /// `CRATONVM_BLOCKED_IN_NATIVE_RUNNABLE` (HotSpot runs the jmm call in
    /// the VM; the thread is never `WAITING` there). It holds no Java
    /// reference across the wait (the line is a Rust `String`).
    fn dcmd_execute(&mut self, line: &str) -> Result<String, String> {
        let table = self
            .shared
            .debug
            .jcmd_processor
            .lock()
            .as_ref()
            .map(|p| std::sync::Arc::clone(&p.attach_listener.commands));
        let Some(table) = table else {
            return Err("Unknown diagnostic command".to_string());
        };
        let line = line.to_string();
        self.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
        // This worker can enter JDK bytecode while the native diagnostic
        // command is being resolved (for example, VM.flags from the
        // DiagnosticCommand MBean). The Rust thread default stack is too
        // small for the interpreter's inline-site resolver on that path and
        // overflows before the command handler runs. Match the VM's normal
        // Java-thread stack budget for this temporary native worker.
        let joined = std::thread::Builder::new()
            .name("DiagnosticCommand".to_string())
            .stack_size(8 * 1024 * 1024)
            .spawn(move || crate::runtime::serviceability::dcmd_mbean_execute(&table, &line))
            .map(|handle| handle.join());
        self.end_blocking_region();
        match joined {
            Ok(Ok(outcome)) => outcome,
            // The command panicked: HotSpot has no such state; report it as
            // the command's own failure text rather than unwinding into Java.
            Ok(Err(_)) => Ok("Diagnostic command failed\n".to_string()),
            Err(e) => Ok(format!("Diagnostic command could not start: {e}\n")),
        }
    }

    /// gci i47w/dh47: `HotSpotDiagnostic.dumpHeap0` (HotSpot's
    /// `jmm_DumpHeap0`). As [`Self::dcmd_execute`], the dump runs on an OS
    /// thread the VM does not know -- the thread `jcmd GC.heap_dump`'s
    /// handlers are written for (the live collection takes
    /// `run_dcmd_full_gc_on_attach_thread`'s attached carrier; the walk asks
    /// for its pause as an outsider) -- while this Java thread waits GC-safe
    /// in a blocking region. A scoped thread, so it borrows `shared` and
    /// `path`; nothing Java is held across the wait.
    fn dump_heap_for_mxbean(&mut self, path: &str, live: bool) -> Result<(), String> {
        let shared: &SharedVm = self.shared;
        self.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
        let joined = std::thread::scope(|s| {
            std::thread::Builder::new()
                .name("HeapDumper".to_string())
                .spawn_scoped(s, move || {
                    crate::vm::vm_init::heap_dump_for_mxbean(shared, path, live)
                })
                .map(|handle| handle.join())
        });
        self.end_blocking_region();
        match joined {
            Ok(Ok(outcome)) => outcome.map(|_| ()),
            Ok(Err(_)) => Err("heap dump failed".to_string()),
            Err(e) => Err(format!("heap dump could not start: {e}")),
        }
    }

    /// gc-common w4-d: `Runtime.runFinalization()` drains the VM's own
    /// finalizer queue (the JDK's is always empty here).
    fn run_finalization(&mut self) {
        crate::runtime::interpreter::run_pending_finalizers_for_runtime(self.shared, self.thread);
    }

    /// gc-common w5-d: `waitForReferenceProcessing()` answers from the VM's
    /// reference-delivery thread (full hand-off only; `false` otherwise).
    fn wait_for_reference_processing(&mut self) -> bool {
        crate::runtime::interpreter::wait_for_reference_processing_for_runtime(
            self.shared,
            self.thread,
        )
    }

    /// gc-common w6-d: see `queue_wake_up_delivery_serves` (`gc_and_alloc.rs`).
    fn reference_queue_wake_ups_delivered(&self) -> bool {
        crate::runtime::interpreter::queue_wake_up_delivery_serves(self.shared)
    }

    /// gc-common w5-d: register `obj` as a `Finalizer` row (NOT the
    /// `ReferenceType::Cleaner` wire 3 `Finalizer.register` used to pass) when
    /// its class has a finalizer. The same `has_finalizer` read the
    /// interpreter's `new` (`init_new_instance`) and JNI `NewObject` make.
    fn register_if_finalizable(&mut self, obj: ObjectRef) {
        // JIT round 21 wave 8, lane reflect: the body is shared with the
        // compiled `linkToSpecial` link; every constructor `MethodHandle` call
        // asks it now (native-builtins `link_to_special_constructor_registers`),
        // so it reads the published allocation recipe first, lock-free.
        crate::runtime::interpreter::register_if_finalizable_shared(self.shared, obj);
    }

    /// JIT round 21 wave 10, lane finalize: see
    /// `SharedVm::withdraw_unconstructed_finalizable_noted` (the unwinder's
    /// note is the proof; nothing is read unless the class has a finalizer).
    fn withdraw_unconstructed_finalizable(&mut self, obj: ObjectRef, epoch_before_call: u64) {
        crate::runtime::interpreter::withdraw_unconstructed_finalizable_at_door(
            self.shared,
            obj,
            epoch_before_call,
        );
    }

    fn withdraw_finalizable_before_constructor(&mut self, obj: ObjectRef, epoch: u64) {
        crate::runtime::interpreter::withdraw_finalizable_before_constructor(
            self.shared,
            obj,
            epoch,
        );
    }

    fn get_static_field(&self, class_id: ClassId, field_index: usize) -> Value {
        super::get_static_shared(self.shared, class_id, field_index)
    }

    fn set_static_field(&mut self, class_id: ClassId, field_index: usize, value: Value) {
        if matches!(value, Value::Object(_))
            && self
                .shared
                .classes
                .frozen
                .has_snapshots
                .load(std::sync::atomic::Ordering::Relaxed)
        {
            crate::runtime::frozen_pages::invalidate_class(self.shared, class_id.as_u32());
        }
        let value = forward_boundary_value(&self.shared.mem.heap, value);
        super::set_static_shared(self.shared, class_id, field_index, value);
    }

    fn static_field_hw_update(
        &mut self,
        class_id: ClassId,
        field_index: usize,
        f: &mut dyn FnMut(Value) -> Option<Value>,
    ) -> Option<(Value, bool)> {
        if self
            .shared
            .classes
            .frozen
            .has_snapshots
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            crate::runtime::frozen_pages::invalidate_class(self.shared, class_id.as_u32());
        }
        let heap = &self.shared.mem.heap;
        let mut forwarded = |current: Value| f(current).map(|v| forward_boundary_value(heap, v));
        hw_static_cell_update(self.shared, class_id, field_index, &mut forwarded)
    }

    // JIT round 21, lane atomics (STATICCAS21-2, `CRATONVM_JIT_UNSAFE_STATIC_RMW_INLINE`):
    // publish a whole `int` / `long` static's entry for the compiled `Unsafe`
    // RMW prefixes' static arm (`JitRealm::unsafe_inline.note_static_target`).
    // One relaxed load until a compile armed the static half, and once the
    // offset's entry is taken. Only while the natives' static atomic is
    // `hw_static_cell_update` (`CRATONVM_HW_STATIC_CAS`, the statics index):
    // the compiled locked instruction is atomic with that CAS on the same
    // payload word, not with the mirror-monitor fallback. The entry holds the
    // class's mirror as a weak word (ATOMICS21-5), read and published under
    // the `class_mirrors` read lock so the collector's re-key (under the
    // write lock) never misses it; a class with no mirror publishes nothing.
    fn note_unsafe_static_atomic_target(
        &self,
        offset: usize,
        class_id: ClassId,
        field_index: usize,
        desc: &dyn Fn() -> Option<u8>,
    ) {
        let shared = self.shared;
        if !shared.jit.unsafe_inline.static_publish_wanted(offset)
            || !shared.hw_static_cas
            || super::vm_object::statics_index_disabled()
        {
            return;
        }
        let wide = match desc() {
            Some(b'I') => false,
            Some(b'J') => true,
            _ => return,
        };
        let Some(base_cell) = shared
            .classes
            .statics_index
            .base_cell_addr(class_id, field_index)
        else {
            return;
        };
        let mirrors = shared.classes.class_mirrors.read();
        let Some(mirror) = mirrors.get(&class_id).copied() else {
            return;
        };
        shared.jit.unsafe_inline.note_static_target(
            offset,
            wide,
            mirror.as_ptr() as usize,
            base_cell,
            class_id.as_u32(),
            field_index,
        );
    }

    // j27 wave 2 (lane VHCOMP, `CRATONVM_JIT_VH_CONST_STATIC`): settle the
    // compiled static constant-`VarHandle` cells of every `static final`
    // holding `vh` (`crate::jit::vh_static_cells`). One relaxed load while no
    // registered cell waits.
    fn note_static_varhandle_target(
        &self,
        vh: ObjectRef,
        class_id: ClassId,
        field_index: usize,
        writable: bool,
        desc: &dyn Fn() -> Option<u8>,
    ) {
        crate::jit::vh_static_cells::note_target(
            self.shared,
            vh,
            class_id,
            field_index,
            writable,
            desc,
        );
    }

    // JIT round 21 (lane staticcas; correctness, unconditional): one hardware
    // CAS on the static's payload word (`compare_and_exchange_static_shared`).
    // The read-compare-write below is only for a static with no block yet or
    // an untyped cell (see that function); `static_cas_values_equal` is the
    // comparison both arms share.
    fn compare_and_exchange_static_field(
        &mut self,
        class_id: ClassId,
        field_index: usize,
        expected: Value,
        new_val: Value,
    ) -> (Value, bool) {
        if let Some(r) = super::compare_and_exchange_static_shared(
            self.shared,
            class_id,
            field_index,
            expected,
            new_val,
        ) {
            return r;
        }
        let cur = super::get_static_shared(self.shared, class_id, field_index);
        let ok = cratonvm_native_api::registry::static_cas_values_equal(cur, expected);
        if ok {
            self.set_static_field(class_id, field_index, new_val);
        }
        (cur, ok)
    }

    // Round 15 wave 2, lane jdkonly: `System.setOut(null)` reads back null.
    fn mark_system_stream_written(&self, field_name: &str) {
        if let Some(stream) =
            crate::runtime::interpreter::SystemStreamField::from_field_name(field_name)
        {
            stream.mark_written(self.shared);
        }
    }

    fn static_field_index_by_name(&self, class_id: ClassId, field_name: &str) -> Option<usize> {
        let cm = self.shared.classes.class_manager.read();
        let class = cm.get_class(class_id)?;
        let mut static_idx = 0usize;
        for f in &class.fields {
            if f.is_static() {
                if &*f.name == field_name {
                    return Some(static_idx);
                }
                static_idx += 1;
            }
        }
        None
    }

    fn fd_table(&self) -> &FileDescriptorTable {
        &self.shared.natives.fd_table
    }

    // -- Panama FFI (JEP 454) --

    fn allocate_native_memory(&mut self, size: usize, align: usize) -> Option<(i64, *mut u8)> {
        self.shared
            .natives
            .native_memory
            .lock()
            .allocate(size, align)
    }

    fn free_native_memory(&mut self, alloc_id: i64) {
        // Retire every published FFM fast-path verdict. A verdict says "this
        // carrier's scope was live and its block was there"; freeing a block is
        // exactly the event that can stop that being true, and the carrier goes
        // on pointing at the freed address. One relaxed increment per FREE,
        // never per access. See `cratonvm_native_builtins::ffm_fast`.
        cratonvm_native_builtins::ffm_fast::bump_epoch();
        self.shared.natives.native_memory.lock().free(alloc_id);
    }

    fn load_native_library(&mut self, path: &str) -> Result<i64, crate::error::MethodCallFailed> {
        // Resolve the library path: if `path` has no directory separator, search
        // `java.library.path` for the platform-specific library file name.
        let resolved = resolve_library_path(self.shared, path);

        // Safety: we trust the user-provided path. libloading handles platform differences.
        let lib = unsafe { libloading::Library::new(&resolved) }.map_err(|e| {
            crate::error::RuntimeError::IllegalStateException {
                message: format!("Failed to load library '{}': {}", resolved, e),
            }
        })?;

        // Call JNI_OnLoad(JavaVM*, void*) if exported by the library.
        // This allows the library to register its native methods via RegisterNatives.
        // Safety: JNI_OnLoad has a fixed, well-known signature.
        //
        // Windows: Apache `tcnative-*.dll` faults inside `JNI_OnLoad` /
        // `RegisterNatives` when paired with CratonVM. We keep the DLL loaded
        // (classpath / Tomcat may probe for its presence) but skip `JNI_OnLoad`
        // — Java entry points are satisfied by Rust stubs and `find_jni_native`
        // / `resolve_jni_native_in_libraries` blocks for
        // `org/apache/tomcat/jni/**`.
        //
        // Netty's `netty_tcnative_*` USED to be skipped by the same rule. It no
        // longer is: that blanket skip was written when `RegisterNatives` could
        // not serve the `FindClass` + `RegisterNatives` idiom every `JNI_OnLoad`
        // uses at all (see `native/jni.rs`'s index-215 note — `FindClass`
        // returned NULL, so the whole path was dead and any library that tried
        // it produced an `UnsatisfiedLinkError` at best). With that fixed, the
        // skip has one visible consequence and no remaining benefit:
        // `OpenSsl.isAvailable()` is permanently false, so netty's own suites
        // silently stop generating every `SslProvider.OPENSSL` /
        // `OPENSSL_REFCNT` parameter — `ParameterizedSslHandlerTest` enumerated
        // 7 of HotSpot's 63 tests, `SslErrorTest` 0 of 72 — and the
        // `SslContextBuilder`/`CloseNotify`/`OpenSslKeyMaterialManager` classes
        // fail the OpenSSL half outright. Measured against HotSpot 25 with the
        // same classpath; see the retired `ssl-suite-test-discovery-undercounts`
        // write-up.
        let basename_lc = std::path::Path::new(resolved.as_str())
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        // Conscrypt's extracted OpenJDK JNI library uses the same unsafe
        // RegisterNatives-on-load pattern as tcnative on CratonVM, on every
        // platform -- not just Windows. Its JNI_OnLoad calls FindClass on a
        // handful of bootstrap classes before doing anything else, and our
        // FindClass (and the RegisterNatives it feeds) aren't ABI-complete
        // enough to satisfy it; conscrypt's own init aborts the process the
        // moment one of those lookups fails (SIGABRT from inside
        // libconscrypt_openjdk_jni's `jniutil::init`, not a Rust panic).
        // Jetty only needs a provider that can advertise its ALPN processor
        // while it builds a connector; the actual TLS engine remains
        // CratonVM's own TLS surface (`t27_tls.rs`), and the Java entry
        // points conscrypt's Java-side classes call into are satisfied by
        // `register_conscrypt_native_bridges` in native-builtins/src/tls.rs.
        // Netty ships its binding as `netty_tcnative_<os>_<arch>` (and extracts
        // it under that name), Tomcat's APR binding as `tcnative-1`. Matching on
        // the `netty` prefix is what keeps the two apart — a bare `tcnative`
        // substring matches both.
        let is_netty_tcnative = basename_lc.contains("netty_tcnative");
        // Escape hatch for a host whose netty tcnative build does fault under
        // our JNI ABI: `CRATONVM_SYNTHETIC_NETTY_TCNATIVE=1` restores the
        // stub-only behaviour, and `OpenSsl.isAvailable()` goes back to `false`.
        let netty_tcnative_opt_out = cratonvm_types::flags::flags()
            .io
            .synthetic_netty_tcnative_forced;
        let skip_jni_onload_tcnative = (basename_lc.contains("tcnative")
            && (!is_netty_tcnative || netty_tcnative_opt_out))
            || basename_lc.contains("conscrypt_openjdk_jni");

        // Whether `JNI_OnLoad` actually ran to completion for a netty tcnative
        // library. Only that keeps `netty_tcnative_real` set (it is set for the
        // duration of `JNI_OnLoad` itself, see there) — the library being on
        // disk proves nothing, and dropping the stubs for a package whose real
        // entry points were never registered would turn a working stub surface
        // into `UnsatisfiedLinkError`s.
        let mut netty_tcnative_onload_ran = false;
        unsafe {
            type JniOnLoad = extern "C" fn(
                crate::native::jni::JavaVM,
                *mut std::ffi::c_void,
            ) -> crate::native::jni::JInt;
            if !skip_jni_onload_tcnative {
                if let Ok(sym) = lib.get::<JniOnLoad>(b"JNI_OnLoad\0") {
                    // Set TLS context so RegisterNatives (called from JNI_OnLoad) can
                    // resolve class names via the class manager.
                    //
                    // SECURITY: install via the RAII guard so the TLS pointers are
                    // cleared even if `JNI_OnLoad` (or a RegisterNatives up-call it
                    // makes) panics/unwinds — otherwise a dangling `*mut JvmThread`
                    // / `*mut SharedVm` would be left in TLS for a later JNI access
                    // to dereference (use-after-free).
                    //
                    // Safety: `self.thread` is the live `&mut JvmThread` borrowed
                    // for this call; it outlives `_jni_guard` per `set_jni_thread`.
                    let _jni_guard = JniContextGuard::install(self.shared, self.thread as *mut _);
                    let _jni_local_frame = JniImplicitFrameGuard::enter();
                    let _raw_jni_locals = crate::native::jni::RawLocalsDispatch::enter();
                    // gc JNI retirement (2026-10-10): the real package is live
                    // WHILE `JNI_OnLoad` runs, not only after it returns. Netty's
                    // `JNI_OnLoad` registers `NativeStaticallyReferencedJniMethods`,
                    // then `FindClass("io/netty/internal/tcnative/SSL")`, which
                    // initializes `SSL`, whose `<clinit>` calls those natives.
                    // With the flag still false the dispatch refused them
                    // (`skip_jni_incompatible_host_lib`), `<clinit>` threw
                    // `UnsatisfiedLinkError`, `FindClass` failed, `JNI_OnLoad`
                    // returned `JNI_ERR` and unregistered everything:
                    // `OpenSsl.isAvailable()` was false on every platform.
                    // Undone below if `JNI_OnLoad` fails.
                    if is_netty_tcnative {
                        self.shared
                            .natives
                            .netty_tcnative_real
                            .store(true, std::sync::atomic::Ordering::Release);
                        self.shared
                            .natives
                            .native_methods
                            .mute_netty_tcnative_stubs();
                    }
                    let jni_methods_before =
                        self.shared.natives.jni_native_methods.read().clone();
                    let in_native = crate::native::jni::JniNativeCall::enter(
                        crate::native::jni::raw_local_escapes(),
                    );
                    let version = sym(crate::native::jni::get_java_vm(), std::ptr::null_mut());
                    drop(in_native);
                    drop(_raw_jni_locals);
                    drop(_jni_local_frame);
                    drop(_jni_guard);
                    netty_tcnative_onload_ran = finish_jni_onload(
                        self.shared,
                        self.thread,
                        version,
                        &resolved,
                        is_netty_tcnative,
                        jni_methods_before,
                    )?;
                }
            }
        }
        if netty_tcnative_onload_ran {
            self.shared
                .natives
                .netty_tcnative_real
                .store(true, std::sync::atomic::Ordering::Release);
            // Retire the `SyntheticStub` stand-ins for this package in ONE
            // place — the registry's own lookup — rather than at the dispatch
            // sites. `vm_exec`'s general `is_native` arm is only one of several
            // routes into the registry: `try_stackless_invoke`'s
            // `resolve_step1_native` reaches it first for a plain zero-arg
            // static, which is exactly the shape of
            // `Library.initialize0()Z`. Suppressing only the `vm_exec` site
            // produced a half-real package that ran real `aprVersionString`
            // (1.7.5) and real `SSL.versionString` (BoringSSL) but a stubbed
            // `initialize0` returning `true` without calling `apr_initialize`
            // — so `tcn_global_pool` stayed NULL and the first real
            // `SSLContext.make` took a SIGSEGV inside `apr_pool_create_ex`
            // with a NULL parent pool.
            self.shared
                .natives
                .native_methods
                .mute_netty_tcnative_stubs();
        }

        let mut libs = self.shared.natives.native_libraries.lock();
        let index = libs.len() as i64;
        libs.push(lib);
        drop(libs);
        let mut loaders = self.shared.natives.native_library_loaders.lock();
        if loaders.len() <= index as usize {
            loaders.resize(index as usize + 1, Some(0));
        }
        loaders[index as usize] = Some(0);
        drop(loaders);
        // Clear any stale tombstone for this index. Live realms never recycle an
        // index, so this only matters when a previous VM's realm was dropped and
        // a new one landed at the same address — see `unloaded_native_libraries`.
        let key = (self.native_realm_key(), index);
        unloaded_native_libraries()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key);
        Ok(index)
    }

    fn set_native_library_loader(&mut self, lib_index: i64, loader_id: Option<u32>) {
        let Ok(idx) = usize::try_from(lib_index) else {
            return;
        };
        let mut loaders = self.shared.natives.native_library_loaders.lock();
        if let Some(slot) = loaders.get_mut(idx) {
            *slot = loader_id;
        }
    }

    /// Logical unload for `RawNativeLibraries.unload0`.
    ///
    /// Tombstones the index instead of dropping the `libloading::Library`:
    /// `native_libraries` is append-only and positional (see
    /// [`unloaded_native_libraries`]), and dropping the library would `dlclose`
    /// code that already-issued function pointers (bound FFM downcall stubs,
    /// cached `findEntry0` results) may still be executing from. After this call
    /// [`find_native_symbol`](NativeSystemAccess::find_native_symbol) refuses the
    /// index, which is what the Java-side contract observes.
    ///
    /// NOTE: JNI-convention resolution (`resolve_jni_native_in_libraries`)
    /// iterates the whole table, but since mcs21 it skips the tombstoned
    /// indices ([`unloaded_native_library_indices`]): a library unloaded
    /// through `RawNativeLibraries` no longer satisfies a `Java_*` symbol
    /// lookup. What stays: it still searches libraries that were only ever
    /// loaded through `RawNativeLibraries` (HotSpot searches the defining
    /// loader's `NativeLibraries` alone) -- filed as
    /// `mcs21-jni-lazy-link-searches-every-library-of-every-loader-20261102.md`.
    fn unload_native_library(&mut self, lib_index: i64) -> bool {
        if lib_index < 0 {
            return false;
        }
        let live = {
            let libs = self.shared.natives.native_libraries.lock();
            (lib_index as usize) < libs.len()
        };
        if !live {
            return false;
        }
        let key = (self.native_realm_key(), lib_index);
        unloaded_native_libraries()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key)
    }

    fn register_upcall(&mut self, entry: crate::native::ffi::UpcallEntry) -> usize {
        self.shared.natives.upcall_table.lock().register(entry)
    }

    fn get_upcall_info(&self, slot: usize) -> Option<(ObjectRef, Vec<i32>, i32)> {
        self.shared
            .natives
            .upcall_table
            .lock()
            .get(slot)
            .map(|e| (e.target, e.param_kinds.clone(), e.return_kind))
    }

    fn record_thread_sleep(&mut self, sleep_nanos: i64, actual_duration_nanos: u64) {
        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        let mut jfr = self.shared.debug.flight_recorder.lock();
        cratonvm_jfr::builtin::emit_thread_sleep_event(
            &mut jfr,
            sleep_nanos,
            self.thread.thread_id.0 as u64,
            now_ns.saturating_sub(actual_duration_nanos),
            actual_duration_nanos,
        );
    }

    fn record_file_read(&mut self, fd: i32, bytes_read: i64, eof: bool, duration_nanos: u64) {
        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        let path = format!("fd:{}", fd);
        let mut jfr = self.shared.debug.flight_recorder.lock();
        cratonvm_jfr::builtin::emit_file_read_event(
            &mut jfr,
            &path,
            bytes_read,
            eof,
            self.thread.thread_id.0 as u64,
            now_ns.saturating_sub(duration_nanos),
            duration_nanos,
        );
    }

    fn record_file_write(&mut self, fd: i32, bytes_written: i64, duration_nanos: u64) {
        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        let path = format!("fd:{}", fd);
        let mut jfr = self.shared.debug.flight_recorder.lock();
        cratonvm_jfr::builtin::emit_file_write_event(
            &mut jfr,
            &path,
            bytes_written,
            self.thread.thread_id.0 as u64,
            now_ns.saturating_sub(duration_nanos),
            duration_nanos,
        );
    }

    fn find_native_symbol(&self, lib_index: i64, name: &str) -> Option<usize> {
        // Indices retired by `unload_native_library` resolve nothing, exactly as
        // a dlclosed handle would. Snapshot them BEFORE taking the library lock
        // so the tombstone mutex is never nested inside it. The set is empty in
        // every run that never unloads, which is the overwhelming majority.
        let unloaded: Vec<i64> = {
            let realm = self.native_realm_key();
            let set = unloaded_native_libraries()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            set.iter()
                .filter(|(r, _)| *r == realm)
                .map(|(_, idx)| *idx)
                .collect()
        };
        if lib_index >= 0 && unloaded.contains(&lib_index) {
            return None;
        }
        let libs = self.shared.natives.native_libraries.lock();
        let c_name = std::ffi::CString::new(name).ok()?;

        if lib_index >= 0 && (lib_index as usize) < libs.len() {
            // Look up in a specific loaded library
            unsafe {
                libs[lib_index as usize]
                    .get::<*const ()>(c_name.as_bytes_with_nul())
                    .ok()
                    .map(|sym| *sym as usize)
            }
        } else {
            // Default/system lookup — try all loaded libraries, skipping any
            // that were unloaded (their symbols are no longer reachable through
            // this VM's library table).
            for (idx, lib) in libs.iter().enumerate() {
                if unloaded.contains(&(idx as i64)) {
                    continue;
                }
                if let Ok(sym) = unsafe { lib.get::<*const ()>(c_name.as_bytes_with_nul()) } {
                    return Some(*sym as usize);
                }
            }
            lookup_process_native_symbol(name)
        }
    }

    fn find_process_native_symbol(&self, name: &str) -> Option<Option<usize>> {
        // Round 14 wave 3 (lane ffm patch): the C runtime only, never the
        // program's libraries -- `Linker.defaultLookup()`'s search set on
        // HotSpot (`panama::default_lookup_process_symbol`).
        Some(lookup_process_native_symbol(name))
    }
}


// ---------------------------------------------------------------------------
// Annotation helpers
// ---------------------------------------------------------------------------

/// Extract annotation data from a list of attributes.
pub(in crate::vm) fn extract_annotations_from_attributes(
    attributes: &[cratonvm_reader::attribute::LazyAttribute],
    cp: &cratonvm_reader::constant_pool::ConstantPool,
) -> Vec<crate::native::registry::AnnotationData> {
    use cratonvm_reader::attribute::Attribute;
    let mut result = Vec::new();
    for lazy in attributes {
        let attr = match lazy.as_decoded() {
            Some(a) => a,
            None => continue,
        };
        match attr {
            // Reflection (Method/Field getAnnotation / isAnnotationPresent)
            // must only see @Retention(RUNTIME) annotations. CLASS-retained
            // annotations land in RuntimeInvisibleAnnotations and must stay
            // invisible to match HotSpot — gap-annotation-retention-policy.md.
            Attribute::RuntimeVisibleAnnotations(annotations) => {
                for ann in annotations {
                    if let Some(data) = convert_annotation(ann, cp) {
                        result.push(data);
                    }
                }
            }
            _ => {}
        }
    }
    result
}

/// Extract parameter annotation data from a list of attributes.
pub(in crate::vm) fn extract_parameter_annotations(
    attributes: &[cratonvm_reader::attribute::LazyAttribute],
    cp: &cratonvm_reader::constant_pool::ConstantPool,
) -> Vec<Vec<crate::native::registry::AnnotationData>> {
    use cratonvm_reader::attribute::Attribute;
    for lazy in attributes {
        let attr = match lazy.as_decoded() {
            Some(a) => a,
            None => continue,
        };
        match attr {
            Attribute::RuntimeVisibleParameterAnnotations(params)
            | Attribute::RuntimeInvisibleParameterAnnotations(params) => {
                return params
                    .iter()
                    .map(|anns| {
                        anns.iter()
                            .filter_map(|ann| convert_annotation(ann, cp))
                            .collect()
                    })
                    .collect();
            }
            _ => {}
        }
    }
    Vec::new()
}

/// Extract TYPE_USE annotations targeting the method return type
/// (`target_type` 0x14, METHOD_RETURN) with an empty `type_path` — i.e.
/// annotations placed directly on the top-level return type. Reads only the
/// `RuntimeVisibleTypeAnnotations` attribute (RUNTIME retention), matching
/// HotSpot's `getAnnotatedReturnType().getDeclaredAnnotations()`.
pub(in crate::vm) fn extract_return_type_annotations(
    attributes: &[cratonvm_reader::attribute::LazyAttribute],
    cp: &cratonvm_reader::constant_pool::ConstantPool,
) -> Vec<crate::native::registry::AnnotationData> {
    use cratonvm_reader::attribute::Attribute;
    const TARGET_METHOD_RETURN: u8 = 0x14;
    for lazy in attributes {
        let attr = match lazy.decoded_or_decode(cp) {
            Some(a) => a,
            None => continue,
        };
        if let Attribute::RuntimeVisibleTypeAnnotations(tas) = attr.as_ref() {
            return tas
                .iter()
                .filter(|ta| ta.target_type == TARGET_METHOD_RETURN && ta.type_path.is_empty())
                .filter_map(|ta| convert_annotation(&ta.annotation, cp))
                .collect();
        }
    }
    Vec::new()
}

/// Insert an `AnnotationData` into a `TypeArgAnnotations` forest at the
/// position described by `path` (a `type_path` restricted to TYPE_ARGUMENT
/// entries, per JVMS 4.7.20.2 — the caller has already filtered out any
/// ARRAY/INNER_TYPE/WILDCARD_BOUND entries). `path` must be non-empty; each
/// entry descends one nesting level (top-level index first), growing `tree`
/// (and each level's `children`) as needed so arbitrarily deep generics
/// (`ValueExtractor<ArgumentValue<@ExtractedValue ?>>`, two TYPE_ARGUMENT
/// steps) land at the right node, not just the single-level case
/// (`List<@NotBlank String>`) the extraction call sites used to hard-code.
pub(in crate::vm::vm_exec) fn insert_type_arg_annotation(
    tree: &mut Vec<crate::native::registry::TypeArgAnnotations>,
    path: &[cratonvm_reader::attribute::TypePathEntry],
    data: crate::native::registry::AnnotationData,
) {
    let idx = path[0].type_argument_index as usize;
    if tree.len() <= idx {
        tree.resize(idx + 1, Default::default());
    }
    if path.len() == 1 {
        tree[idx].anns.push(data);
    } else {
        insert_type_arg_annotation(&mut tree[idx].children, &path[1..], data);
    }
}

/// Extract TYPE_USE annotations targeting the method return type's TYPE
/// ARGUMENTS, at any nesting depth (`target_type` 0x14, METHOD_RETURN, with a
/// `type_path` made entirely of TYPE_ARGUMENT entries, `type_path_kind == 3`)
/// -- e.g. an annotation on `String` in `List<@NotBlank String> getNames()`,
/// or a nested generic like `ValueExtractor<Wrapper<@Foo ?>>`.
pub(in crate::vm) fn extract_return_type_argument_annotations(
    attributes: &[cratonvm_reader::attribute::LazyAttribute],
    cp: &cratonvm_reader::constant_pool::ConstantPool,
) -> Vec<crate::native::registry::TypeArgAnnotations> {
    use cratonvm_reader::attribute::Attribute;
    const TARGET_METHOD_RETURN: u8 = 0x14;
    const TYPE_PATH_KIND_TYPE_ARGUMENT: u8 = 3;
    for lazy in attributes {
        let attr = match lazy.decoded_or_decode(cp) {
            Some(a) => a,
            None => continue,
        };
        if let Attribute::RuntimeVisibleTypeAnnotations(tas) = attr.as_ref() {
            let mut out: Vec<crate::native::registry::TypeArgAnnotations> = Vec::new();
            for ta in tas {
                if ta.target_type != TARGET_METHOD_RETURN || ta.type_path.is_empty() {
                    continue;
                }
                if ta
                    .type_path
                    .iter()
                    .any(|e| e.type_path_kind != TYPE_PATH_KIND_TYPE_ARGUMENT)
                {
                    continue;
                }
                if let Some(data) = convert_annotation(&ta.annotation, cp) {
                    insert_type_arg_annotation(&mut out, &ta.type_path, data);
                }
            }
            return out;
        }
    }
    Vec::new()
}

/// Extract TYPE_USE annotations targeting the method formal parameters
/// (`target_type` 0x16, METHOD_FORMAL_PARAMETER) with an empty `type_path`.
/// The outer `Vec` is indexed by `formal_parameter_index` (sized to the
/// highest index seen); parameters without type annotations get an empty
/// inner `Vec`. Backs `Parameter.getAnnotatedType().getDeclaredAnnotations()`.
pub(in crate::vm) fn extract_parameter_type_annotations(
    attributes: &[cratonvm_reader::attribute::LazyAttribute],
    cp: &cratonvm_reader::constant_pool::ConstantPool,
) -> Vec<Vec<crate::native::registry::AnnotationData>> {
    use cratonvm_reader::attribute::Attribute;
    const TARGET_METHOD_FORMAL_PARAMETER: u8 = 0x16;
    for lazy in attributes {
        let attr = match lazy.decoded_or_decode(cp) {
            Some(a) => a,
            None => continue,
        };
        if let Attribute::RuntimeVisibleTypeAnnotations(tas) = attr.as_ref() {
            let mut out: Vec<Vec<crate::native::registry::AnnotationData>> = Vec::new();
            for ta in tas {
                if ta.target_type != TARGET_METHOD_FORMAL_PARAMETER || !ta.type_path.is_empty() {
                    continue;
                }
                // target_info for METHOD_FORMAL_PARAMETER is a single u1 index.
                let idx = match ta.target_info.first() {
                    Some(&i) => i as usize,
                    None => continue,
                };
                if let Some(data) = convert_annotation(&ta.annotation, cp) {
                    if out.len() <= idx {
                        out.resize(idx + 1, Vec::new());
                    }
                    out[idx].push(data);
                }
            }
            return out;
        }
    }
    Vec::new()
}

/// Extract TYPE_USE annotations targeting method formal parameters'
/// TYPE ARGUMENTS, at any nesting depth (`target_type` 0x16,
/// METHOD_FORMAL_PARAMETER, with a `type_path` made entirely of
/// TYPE_ARGUMENT entries, `type_path_kind == 3`) -- i.e. an annotation on a
/// generic type argument like the `@Valid` in `List<@Valid Person> persons`,
/// or a nested one like `Map<String, @Valid List<Person>>`, as opposed to
/// [`extract_parameter_type_annotations`] which only keeps the
/// empty-`type_path` (top-level parameter type) annotations.
///
/// Outer `Vec` indexed by `formal_parameter_index`; inner `Vec` indexed by
/// the top-level `type_argument_index` (0-based, JVMS 4.7.20.2), with each
/// entry's own `children` carrying the next nesting level. Backs
/// `AnnotatedParameterizedType.getAnnotatedActualTypeArguments()`
/// for method parameters.
pub(in crate::vm) fn extract_parameter_type_argument_annotations(
    attributes: &[cratonvm_reader::attribute::LazyAttribute],
    cp: &cratonvm_reader::constant_pool::ConstantPool,
) -> Vec<Vec<crate::native::registry::TypeArgAnnotations>> {
    use cratonvm_reader::attribute::Attribute;
    const TARGET_METHOD_FORMAL_PARAMETER: u8 = 0x16;
    const TYPE_PATH_KIND_TYPE_ARGUMENT: u8 = 3;
    for lazy in attributes {
        let attr = match lazy.decoded_or_decode(cp) {
            Some(a) => a,
            None => continue,
        };
        if let Attribute::RuntimeVisibleTypeAnnotations(tas) = attr.as_ref() {
            let mut out: Vec<Vec<crate::native::registry::TypeArgAnnotations>> = Vec::new();
            for ta in tas {
                if ta.target_type != TARGET_METHOD_FORMAL_PARAMETER || ta.type_path.is_empty() {
                    continue;
                }
                if ta
                    .type_path
                    .iter()
                    .any(|e| e.type_path_kind != TYPE_PATH_KIND_TYPE_ARGUMENT)
                {
                    continue;
                }
                // target_info for METHOD_FORMAL_PARAMETER is a single u1 index.
                let param_idx = match ta.target_info.first() {
                    Some(&i) => i as usize,
                    None => continue,
                };
                if let Some(data) = convert_annotation(&ta.annotation, cp) {
                    if out.len() <= param_idx {
                        out.resize(param_idx + 1, Vec::new());
                    }
                    insert_type_arg_annotation(&mut out[param_idx], &ta.type_path, data);
                }
            }
            return out;
        }
    }
    Vec::new()
}

/// Extract TYPE_USE annotations targeting a field's type (`target_type` 0x13,
/// FIELD) with an empty `type_path`. Backs
/// `Field.getAnnotatedType().getDeclaredAnnotations()`.
pub(in crate::vm) fn extract_field_type_annotations(
    attributes: &[cratonvm_reader::attribute::LazyAttribute],
    cp: &cratonvm_reader::constant_pool::ConstantPool,
) -> Vec<crate::native::registry::AnnotationData> {
    use cratonvm_reader::attribute::Attribute;
    const TARGET_FIELD: u8 = 0x13;
    for lazy in attributes {
        let attr = match lazy.decoded_or_decode(cp) {
            Some(a) => a,
            None => continue,
        };
        if let Attribute::RuntimeVisibleTypeAnnotations(tas) = attr.as_ref() {
            return tas
                .iter()
                .filter(|ta| ta.target_type == TARGET_FIELD && ta.type_path.is_empty())
                .filter_map(|ta| convert_annotation(&ta.annotation, cp))
                .collect();
        }
    }
    Vec::new()
}

/// Extract TYPE_USE annotations targeting a field type's TYPE ARGUMENTS, at
/// any nesting depth (`target_type` 0x13, FIELD, with a `type_path` made
/// entirely of TYPE_ARGUMENT entries, `type_path_kind == 3`) -- e.g. an
/// annotation on `String` in `List<@NotBlank String> names`.
pub(in crate::vm) fn extract_field_type_argument_annotations(
    attributes: &[cratonvm_reader::attribute::LazyAttribute],
    cp: &cratonvm_reader::constant_pool::ConstantPool,
) -> Vec<crate::native::registry::TypeArgAnnotations> {
    use cratonvm_reader::attribute::Attribute;
    const TARGET_FIELD: u8 = 0x13;
    const TYPE_PATH_KIND_TYPE_ARGUMENT: u8 = 3;
    for lazy in attributes {
        let attr = match lazy.decoded_or_decode(cp) {
            Some(a) => a,
            None => continue,
        };
        if let Attribute::RuntimeVisibleTypeAnnotations(tas) = attr.as_ref() {
            let mut out: Vec<crate::native::registry::TypeArgAnnotations> = Vec::new();
            for ta in tas {
                if ta.target_type != TARGET_FIELD || ta.type_path.is_empty() {
                    continue;
                }
                if ta
                    .type_path
                    .iter()
                    .any(|e| e.type_path_kind != TYPE_PATH_KIND_TYPE_ARGUMENT)
                {
                    continue;
                }
                if let Some(data) = convert_annotation(&ta.annotation, cp) {
                    insert_type_arg_annotation(&mut out, &ta.type_path, data);
                }
            }
            return out;
        }
    }
    Vec::new()
}

/// Extract the runtime-visible TYPE_USE annotations targeting one of a
/// class's declared supertypes (`target_type` 0x10, CLASS_EXTENDS) from its
/// original `.class` bytes, re-parsed on demand.
///
/// Unlike methods/fields (whose per-member attribute lists survive on the
/// loaded [`crate::classloading::class::Class`]), the class-level
/// `RuntimeVisibleTypeAnnotations` attribute is folded away during class
/// loading (JVMS §4.7 attributes not recognized by the `Class` struct's
/// field-by-field fold are dropped) — so there is nowhere to read it from
/// post-load. Re-parsing the cached original bytes (`class_bytes_cache`,
/// already retained for JVMTI retransformation) is the least invasive way to
/// recover it without adding a new field threaded through every `Class`
/// construction site in the codebase. This runs lazily, only when a
/// framework actually calls `Class.getAnnotatedSuperclass()` /
/// `getAnnotatedInterfaces()`, so the cost of a second parse pass is
/// immaterial. Returns an empty tree (not an error) if the bytes were
/// evicted from the FIFO-capped cache or fail to re-parse.
///
/// `supertype_index` is JVMS `CLASS_EXTENDS`'s `u2 supertype_index`: `0xFFFF`
/// selects the superclass, `0..n` selects the n-th entry of `interfaces`.
pub(in crate::vm) fn extract_class_extends_type_annotations(
    class_bytes: &[u8],
    supertype_index: u16,
) -> crate::native::registry::TypeArgAnnotations {
    use cratonvm_reader::attribute::Attribute;
    const TARGET_CLASS_EXTENDS: u8 = 0x10;
    const TYPE_PATH_KIND_TYPE_ARGUMENT: u8 = 3;

    let class_file = match cratonvm_reader::class_reader::read_class(class_bytes) {
        Ok(cf) => cf,
        Err(_) => return Default::default(),
    };
    let cp = &class_file.constant_pool;
    for lazy in &class_file.attributes {
        let attr = match lazy.decoded_or_decode(cp) {
            Some(a) => a,
            None => continue,
        };
        if let Attribute::RuntimeVisibleTypeAnnotations(tas) = attr.as_ref() {
            let mut tree = crate::native::registry::TypeArgAnnotations::default();
            for ta in tas {
                if ta.target_type != TARGET_CLASS_EXTENDS {
                    continue;
                }
                let idx = match ta.target_info.as_slice() {
                    [hi, lo] => u16::from_be_bytes([*hi, *lo]),
                    _ => continue,
                };
                if idx != supertype_index {
                    continue;
                }
                if ta
                    .type_path
                    .iter()
                    .any(|e| e.type_path_kind != TYPE_PATH_KIND_TYPE_ARGUMENT)
                {
                    continue;
                }
                let data = match convert_annotation(&ta.annotation, cp) {
                    Some(d) => d,
                    None => continue,
                };
                if ta.type_path.is_empty() {
                    tree.anns.push(data);
                } else {
                    insert_type_arg_annotation(&mut tree.children, &ta.type_path, data);
                }
            }
            return tree;
        }
    }
    Default::default()
}

/// Convert a reader Annotation to our AnnotationData.
pub(in crate::vm) fn convert_annotation(
    ann: &cratonvm_reader::attribute::Annotation,
    cp: &cratonvm_reader::constant_pool::ConstantPool,
) -> Option<crate::native::registry::AnnotationData> {
    use crate::native::registry::AnnotationData;
    let type_desc = cp.get_utf8(ann.type_index)?.to_string();
    let mut elements = Vec::new();
    for pair in &ann.element_value_pairs {
        let name = cp.get_utf8(pair.element_name_index)?.to_string();
        let value = convert_element_value(&pair.value, cp)?;
        elements.push((name, value));
    }
    Some(AnnotationData {
        type_descriptor: type_desc,
        elements,
    })
}

/// Convert a reader ElementValue to our AnnotationElementValue.
pub(in crate::vm) fn convert_element_value(
    ev: &cratonvm_reader::attribute::ElementValue,
    cp: &cratonvm_reader::constant_pool::ConstantPool,
) -> Option<crate::native::registry::AnnotationElementValue> {
    use crate::native::registry::AnnotationElementValue;
    use cratonvm_reader::attribute::ElementValue;
    use cratonvm_reader::constant_pool::ConstantPoolEntry;
    match ev {
        ElementValue::Const {
            tag,
            const_value_index,
        } => {
            let idx = *const_value_index;
            match tag {
                b'B' | b'C' | b'I' | b'S' | b'Z' => {
                    if let Some(ConstantPoolEntry::Integer(val)) = cp.get(idx) {
                        Some(AnnotationElementValue::Int(*val))
                    } else {
                        Some(AnnotationElementValue::Int(0))
                    }
                }
                b'J' => {
                    if let Some(ConstantPoolEntry::Long(val)) = cp.get(idx) {
                        Some(AnnotationElementValue::Long(val.get()))
                    } else {
                        Some(AnnotationElementValue::Long(0))
                    }
                }
                b'F' => {
                    if let Some(ConstantPoolEntry::Float(val)) = cp.get(idx) {
                        Some(AnnotationElementValue::Float(*val))
                    } else {
                        Some(AnnotationElementValue::Float(0.0))
                    }
                }
                b'D' => {
                    if let Some(ConstantPoolEntry::Double(val)) = cp.get(idx) {
                        Some(AnnotationElementValue::Double(val.get()))
                    } else {
                        Some(AnnotationElementValue::Double(0.0))
                    }
                }
                b's' => {
                    let val = cp.get_utf8(idx)?.to_string();
                    Some(AnnotationElementValue::StringVal(val))
                }
                _ => None,
            }
        }
        ElementValue::Enum {
            type_name_index,
            const_name_index,
        } => {
            let type_name = cp.get_utf8(*type_name_index)?.to_string();
            let const_name = cp.get_utf8(*const_name_index)?.to_string();
            Some(AnnotationElementValue::Enum(type_name, const_name))
        }
        ElementValue::Class { class_info_index } => {
            let desc = cp.get_utf8(*class_info_index)?.to_string();
            Some(AnnotationElementValue::Class(desc))
        }
        ElementValue::AnnotationValue(nested) => {
            let data = convert_annotation(nested, cp)?;
            Some(AnnotationElementValue::Annotation(data))
        }
        ElementValue::Array(values) => {
            let mut items = Vec::new();
            for v in values {
                items.push(convert_element_value(v, cp)?);
            }
            Some(AnnotationElementValue::Array(items))
        }
    }
}

// ---------------------------------------------------------------------------
// CRATONVM_DBG_DISPATCH_TALLY — which callees reach the slow resolvers?
// ---------------------------------------------------------------------------
//
// A CPU profile of a dispatch-bound workload is a flat tail of
// `slot_for_exact` / `find_method_recursive` / `invoke_or_native` samples. It
// proves the VM is dispatching and says nothing about *what*, which is the
// entire diagnosis: the `nioMemLZF:` residual in
// the retired `h2-jitban-longtail1` write-up read as "a long
// interpreter tail with no second hot spot to attack" for two revisions purely
// because nobody had the callee histogram.
//
// Recorded at the two general resolvers that an inline-cache HIT is supposed
// to bypass. A callee showing up here with a per-operation count in the tens
// of thousands is a call site the IC is not serving, and that is actionable in
// a way that a `slot_for_exact` percentage is not.
//
// Env-gated and `#[cold]`: the enabled path takes a global `Mutex` and formats
// a `String` per call, so it is a diagnosis tool, not something to leave on.
#[cold]
/// `CRATONVM_DBG_DEADRECV`: at `identity_hash_code`, ask whether the receiver is
/// an address this process already freed, BEFORE the first dereference, and
/// report it through `reclaim_guard` instead of faulting on it.
///
/// Two sources, because no single one covers both collectors: the reclamation
/// RINGS for the non-moving reclamations that feed them, and the published
/// young GEOMETRY for a moving young cycle, which feeds no ring at all. See
/// `deadrecv_check` for why the second arm exists and what it costs.
///
/// Opt-in and behaviour-changing: a hit returns 0, which `identity_hash_code`
/// otherwise never does (C28). That is deliberate, so one run names many
/// victims rather than dying at the first.
pub(in crate::vm::vm_exec) fn dbg_deadrecv() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| None::<std::ffi::OsString>.is_some())
}

/// `CRATONVM_DBG_DEADRECV`'s shared body: is `obj` an address this process has
/// already reclaimed, asked BEFORE anything dereferences it?
///
/// Every lookup is keyed on the ADDRESS and dereferences nothing, so they answer
/// whether or not the page is still mapped — which is the whole point, because
/// this defect's face is a fault on the read that every other consumer performs
/// first.
///
/// `true` means reported; the caller returns a benign value rather than
/// walking into the fault, so one run names MANY victims.
pub(crate) fn deadrecv_check(
    shared: &SharedVm,
    thread: &JvmThread,
    obj: ObjectRef,
    site: &'static str,
) -> bool {
    if !dbg_deadrecv() {
        return false;
    }
    let addr = obj.as_ptr() as usize;
    // RE-ALLOCATION SCREEN, and without it this probe reports mostly noise.
    //
    // Neither reclamation ring is pruned when the allocator hands a freed span
    // back out: `record_young_span_freed` appends one entry per coalesced span
    // and nothing ever removes it, so EVERY object later allocated inside that
    // span answers `young_freed_lookup` for the rest of the process. The rings
    // remember what was freed, not what is dead now.
    //
    // `is_object_address` is the opposite question and the one this guard
    // actually wants: the arena's object-start bitmap records "a base this
    // arena handed out and has NOT freed", so `Some` means the address was
    // RE-SERVED and is live whatever the ring remembers. It is address-keyed
    // and lock-free, and it screens through the commit bitmap before touching
    // anything, so it is safe on exactly the decommitted addresses this guard
    // exists to survive.
    //
    // What this trades away, stated rather than hidden: an ABA — a stale
    // reference to an address the allocator has since re-served for a
    // DIFFERENT object — now reads as live and is not reported. That case is
    // indistinguishable here without a per-address allocation epoch, and the
    // alternative was a guard whose every hit had to be re-litigated by hand.
    // `CRATONVM_DBG_VACATED_FRAMES` (`gc_quiescence::was_vacated`) is the
    // instrument that does track re-issue exactly; use it for the ABA face.
    //
    // Measured on `test_classes/gpu/GpuResidencyGc 0 1024 800` under
    // `--XX:UseGc Generational`, which is where the unscreened form was read as
    // evidence (`gpuresidencygc-generational-jit-reclaims-a-live-object-FIXED-20260908.md`):
    // 8 hits every run with the JIT on and 0 with `--nojit` — a split that is
    // fully explained by WHICH COLLECTOR RAN, since only the non-moving sweep
    // populates the young ring at all, and not by any reference being stale.
    // With the screen: 0 hits, 3 runs, and the probe passes.
    //
    // It also puts the guard's COST back where it belongs. Both lookups are
    // linear scans of their rings — the old-gen one is 2^20 entries — on every
    // `identity_hash_code` and `class_id_of_object`. Armed, that probe took
    // 163-251 s against 4 s unarmed; with this screen in front of them it is 4 s
    // armed, because a live address never reaches the scan. An instrument that
    // dilates its workload 40-60x is not measuring the same run.
    if shared.mem.heap.is_object_address(addr).is_some() {
        return false;
    }
    if cratonvm_gc::gen_heap::old_freed_lookup_covering(addr).is_some()
        || cratonvm_gc::gen_heap::young_freed_lookup(addr).is_some()
    {
        report_deadrecv(shared, thread, addr, site);
        return true;
    }
    // THE MOVING-YOUNG ARM, and without it this probe is silent on the one
    // collector it was written for.
    //
    // Both rings above are fed by a NON-MOVING reclamation: `old_freed_*` by
    // the old-gen sweep and mark-compact, `young_freed_*` by
    // `sweep_young_non_moving`'s `record_young_span_freed`. A MOVING young
    // cycle writes to neither — it evacuates the semispace and
    // `uncommit_evacuated_young` hands the span back to the OS — so on
    // `--XX:UseGc Generational` with moving cycles in the history, the rings
    // answer nothing about exactly the addresses that fault. That is the
    // Hazelcast `ServerTests` face: the crash handler names the span
    // (`site=unbumped-middle`, `and NOT re-committed since`) and this guard,
    // asked about the same address one instruction earlier, said "live".
    //
    // The question is address-keyed and dereferences NOTHING, which is what
    // lets it answer on a released granule: `is_object_address` screened above
    // through the commit bitmap and the arena's object-start bitmap, and
    // `young_geometry_span` is four relaxed loads and two range compares. A
    // live young reference always names an object start, so "inside the young
    // geometry and no object starts there" names no live object.
    //
    // Cheap enough to stay armed, which is the property the page asked for:
    // the `CRATONVM_DBG_VACATED_FRAMES` ledger that answers the same question
    // exactly costs a RECORD path running all process long and dilates this
    // workload about ninefold, past the window the crash needs.
    if let Some(span) = cratonvm_gc::gen_heap::young_geometry_span(addr) {
        tracing::error!(
            target: "cratonvm::gc::guard",
            obj = format!("{addr:#x}"),
            site = site,
            semispace = span,
            "receiver names a YOUNG address at which no object starts: a moving young cycle evacuated this span and nothing rewrote the reference. INACTIVE-SEMISPACE is the arena the last flip emptied; ACTIVE-SEMISPACE means the span was re-served, but not to an object base.",
        );
        report_deadrecv(shared, thread, addr, site);
        return true;
    }
    false
}

/// The shared report for both [`deadrecv_check`] arms: the reclaim guard's
/// verdict, then the Java stack that handed the dead receiver in.
///
/// The guard answers WHAT the address is and proves no live heap object holds it
/// — which leaves "a frame local, a register, or a native side table", and says
/// nothing about WHICH. The Java stack is the cheapest thing that narrows that:
/// the receiver was on some frame's operand stack one bytecode ago, so the
/// innermost frames bound where it came from.
pub(in crate::vm::vm_exec) fn report_deadrecv(shared: &SharedVm, thread: &JvmThread, addr: usize, site: &'static str) {
    crate::memory::reclaim_guard::report_reclaimed_receiver_forced(
        shared,
        addr,
        site,
        "java/lang/Object",
        0,
    );
    tracing::error!(
        target: "cratonvm::gc::guard",
        obj = format!("{addr:#x}"),
        site = site,
        java_frames = %deadrecv_java_frames(thread),
        "…and THIS is the Java stack that handed the dead receiver in. The innermost frame is the call site; the producer is the native, or the reference copy, that put this address on its operand stack.",
    );
}

/// Innermost Java frames, for [`report_deadrecv`].
///
/// Bounded at eight: this is an error path, and a full Hazelcast stack is
/// hundreds deep with nothing in the tail the top does not already say.
pub(in crate::vm::vm_exec) fn deadrecv_java_frames(thread: &JvmThread) -> String {
    thread
        .frames
        .iter()
        .rev()
        .take(8)
        .map(|f| {
            format!(
                "{}.{}{} pc={}",
                f.class_name(),
                f.method_name(),
                f.method_descriptor(),
                f.pc
            )
        })
        .collect::<Vec<_>>()
        .join(" <- ")
}

pub fn dbg_dispatch_tally(site: &str, class_name: &str, method_name: &str, descriptor: &str) {
    dispatch_tally::record(site, class_name, method_name, descriptor);
}

/// Print the `CRATONVM_DBG=dispatch-tally` rows at exit. Called from
/// `interp_census::report_at_exit`; prints nothing when the tally is unarmed.
pub fn dump_dispatch_tally_at_exit() {
    dispatch_tally::dump_at_exit();
}

// ---------------------------------------------------------------------------
// invoke_or_native and helpers
// ---------------------------------------------------------------------------

/// The native call of a by-name registry arm (`invoke_or_native`'s four,
/// `invoke_special_shared_impl`'s override): `safe_native_call`, holding the
/// monitor of the `ACC_SYNCHRONIZED` method the slot answers (JVMS §2.11.10)
/// while `native_sync_enabled`, as the interpreter's native doors do. These
/// doors have no inline cache, so the fact comes from the per-slot memo
/// (`interpreter::native_sync_for_id`): one relaxed load per call after the
/// first. Off, or with no id, the plain call.
///
/// `named` is the `(class, method, descriptor)` the arm found the native
/// under. Interpreter round i1 wave 40 (lane L1): while a debugger or an
/// agent wants method events, the call names that method to the funnel's
/// report ([`by_name_native_call_held`]), as `invoke_on_class_shared_inner`'s
/// registry arm does: a native reached through `Method.invoke` lands here
/// (`NativeContext::invoke_virtual` → `invoke_or_native`), and the funnel,
/// which names a native from its caller's invoke (`Method.invoke`), reported
/// nothing for it (`Runtime.availableProcessors()`, item 1 of
/// `docs/feature-designs/jvmti-delivery-threading.md`).
/// One relaxed load in a build with a JDWP server; nothing without one.
pub(in crate::vm::vm_exec) fn by_name_native_call(
    shared: &SharedVm,
    thread: &mut JvmThread,
    callback: NativeCallback,
    native_id: Option<cratonvm_native_api::NativeMethodId>,
    args: &[Value],
    named: (&str, &str, &str),
) -> MethodCallResult {
    let sync = native_id.and_then(|id| crate::runtime::interpreter::native_sync_for_id(shared, id));
    #[cfg(feature = "experimental-debug")]
    if shared.debug.debugger_gates.method_events_armed() {
        return by_name_native_call_held(shared, thread, callback, args, sync, named);
    }
    #[cfg(not(feature = "experimental-debug"))]
    let _ = named;
    crate::runtime::interpreter::safe_native_call_synchronized(shared, thread, callback, args, sync)
}

/// [`by_name_native_call`] while method events are wanted: the native named
/// for the funnel's report (`interpreter::with_held_native`, which holds it
/// only when the class `named` declares it `ACC_NATIVE`). The class is found
/// by name, loader-blind: the registry's natives are keyed by name too, and a
/// miss only leaves the call unnamed, as before.
#[cfg(feature = "experimental-debug")]
#[cold]
#[inline(never)]
pub(in crate::vm::vm_exec) fn by_name_native_call_held(
    shared: &SharedVm,
    thread: &mut JvmThread,
    callback: NativeCallback,
    args: &[Value],
    sync: Option<(bool, ClassId)>,
    (class_name, method_name, descriptor): (&str, &str, &str),
) -> MethodCallResult {
    let declaring = shared
        .classes
        .class_manager
        .read_recursive()
        .get_loaded_class_id(class_name);
    match declaring {
        Some(declaring) => crate::runtime::interpreter::with_held_native(
            shared,
            declaring,
            method_name,
            descriptor,
            callback as usize, // Cast: fn address
            || {
                crate::runtime::interpreter::safe_native_call_synchronized(
                    shared, thread, callback, args, sync,
                )
            },
        ),
        None => crate::runtime::interpreter::safe_native_call_synchronized(
            shared, thread, callback, args, sync,
        ),
    }
}

/// The slot id for [`by_name_native_call`]: the one the registry lookup
/// returned, or — on its cold descriptor-quirk path, which returns none — the
/// slot `resolve_id` names, looked up only while `native_sync_enabled`.
/// Registry only (no class-manager lock), so it may run under a `cm` guard.
pub(in crate::vm::vm_exec) fn by_name_native_id(
    shared: &SharedVm,
    native_id: Option<cratonvm_native_api::NativeMethodId>,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<cratonvm_native_api::NativeMethodId> {
    native_id.or_else(|| {
        crate::runtime::interpreter::native_sync_enabled(shared)
            .then(|| {
                shared
                    .natives
                    .native_methods
                    .resolve_id(class_name, method_name, descriptor)
            })
            .flatten()
    })
}

/// Whether [`invoke_static_or_native`] initializes the declaring class of a
/// registered static native before it runs (JVMS §5.5). Every mode since
/// interpreter round i1 wave 13: the `--compatible` census of
/// `docs/feature-designs/real-jdk-mode.md`
/// read zero `[static-native-init-census]` lines over the regression suite
/// with the JIT on and off. The interpreter's `execute_invokestatic` and the
/// JIT funnel tail (`jit_static_owner_init_before_native`) initialize in every
/// mode as well. Kept as the one policy point.
#[inline]
pub fn static_by_name_init_enforced(_shared: &SharedVm) -> bool {
    true
}

/// Whether `args[0]` is a receiver whose class is `java.lang.ClassLoader` or
/// a subclass of it. The by-name `ClassLoader` intercepts (`invoke_or_native`'s
/// `setDefaultAssertionStatus` and null-name `getResource*` arms, the
/// interpreter's `intercept_classloader_set_default_assertion_status`) match
/// a name and descriptor that any user type may also declare; they apply only
/// when this holds. A static call (no object `args[0]`) never matches. Takes
/// the class manager: ask it after the cheap name test.
pub fn receiver_is_class_loader(shared: &SharedVm, args: &[Value]) -> bool {
    match args.first() {
        Some(Value::Object(Some(receiver))) => {
            let cid = shared.mem.heap.class_id_of(*receiver);
            let cm = shared.classes.class_manager.read();
            cm.get_loaded_class_id("java/lang/ClassLoader")
                .is_some_and(|loader| cm.is_subclass_of(cid, loader))
        }
        _ => false,
    }
}

/// [`receiver_is_class_loader`], and the receiver's class INHERITS
/// `ClassLoader`'s `(method_name, descriptor)`: no class between it and
/// `java.lang.ClassLoader` declares the method, the built-in loaders
/// (`jdk/internal/loader/*`, whose lookups the VM's natives serve) aside.
/// The null-name `getResource*` arm of `invoke_or_native` asks it: a loader
/// that overrides the method (`URLClassLoader.getResourceAsStream`, a user
/// loader's `getResource`) runs its own body on HotSpot, null name or not,
/// and the arm used to run `ClassLoader`'s native in its place (interpreter
/// round i2 wave 54, lane L5; probe `L5W54ClassLoaderNullResourceName`,
/// `user-getResourceAsStream`). Cold: asked only for a null name.
pub fn receiver_inherits_class_loader_method(
    shared: &SharedVm,
    args: &[Value],
    method_name: &str,
    descriptor: &str,
) -> bool {
    let Some(Value::Object(Some(receiver))) = args.first() else {
        return false;
    };
    class_inherits_class_loader_method(
        shared,
        shared.mem.heap.class_id_of(*receiver),
        method_name,
        descriptor,
    )
}

/// Positive control of the retired `ClassLoader` null-resource-name intercepts
/// (`ClassRealm::classloader_null_name_intercepts`; interpreter round i2 wave
/// 55, lane L5): with `CRATONVM_DBG_CL_NULL_NAME=1`, one line per call that a
/// door would have sent to `ClassLoader`'s native and now leaves to ordinary
/// dispatch. Reached only with a null name argument and the intercepts
/// retired; `args` is `None` where the door has no `Value` receiver in hand.
#[cold]
#[inline(never)]
pub fn note_classloader_null_name_retired(
    shared: &SharedVm,
    args: Option<&[Value]>,
    door: &str,
    method_name: &str,
    descriptor: &str,
) {
    if !false {
        return;
    }
    let named = matches!(
        (method_name, descriptor),
        ("getResource", "(Ljava/lang/String;)Ljava/net/URL;")
            | (
                "getResources",
                "(Ljava/lang/String;)Ljava/util/Enumeration;"
            )
            | (
                "getResourceAsStream",
                "(Ljava/lang/String;)Ljava/io/InputStream;"
            )
    );
    let inherits = args.map_or(true, |args| {
        receiver_inherits_class_loader_method(shared, args, method_name, descriptor)
    });
    if named && inherits {
        eprintln!("[cl-null-name] intercept retired at {door}: {method_name}");
    }
}

/// [`receiver_inherits_class_loader_method`] for a receiver class already in
/// hand (the JIT's null-resource-name doors, `jit/helpers.rs`).
pub fn class_inherits_class_loader_method(
    shared: &SharedVm,
    class_id: ClassId,
    method_name: &str,
    descriptor: &str,
) -> bool {
    let cm = shared.classes.class_manager.read();
    let mut cid = Some(class_id);
    // Bounded like the other hierarchy walks of this file.
    for _ in 0..64 {
        let Some(class) = cid.and_then(|id| cm.get_class(id)) else {
            return false;
        };
        if &*class.name == "java/lang/ClassLoader" {
            return true;
        }
        if !class.name.starts_with("jdk/internal/loader/")
            && class.find_method(method_name, descriptor).is_some()
        {
            return false;
        }
        cid = class.superclass;
    }
    false
}

/// The class a by-name `invokestatic` of `class_name.method_name` through the
/// loaded symbolic owner `owner` must still initialize (JVMS §5.5): the
/// declaring class (`invokestatic_init_class`) when it is not initialized yet,
/// `None` when nothing is left to do. A reviewed VM-service receiver under
/// `--real-jdk` is never initialized, exactly as the interpreter's native
/// route leaves it (`dispatch_static::resolve_static_owner_fully`).
///
/// Steady state is the VM's initialized bitmap behind
/// `is_class_initialized_via_manager` (initialization is monotone, and an
/// initialized owner has initialized superclasses). Asked by the JIT's
/// `invokestatic` doors (`jit_static_init_pending`), by
/// [`init_static_native_owner`], and by the by-name static tails
/// ([`invoke_static_on_owner`], `invoke_static_shared_on_class`).
///
/// An owner that is never initialized itself — the symbolic owner of an
/// inherited static — pays a class-manager read and one member resolution
/// per call here, on the by-name routes only; with
/// `VmConfig::static_owner_supers_initialized` it pays one superclass walk
/// instead whenever the declaring superclass is already initialized
/// ([`inherited_static_declarer_initialized`]).
pub fn static_owner_init_pending(
    shared: &SharedVm,
    owner: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<ClassId> {
    if super::is_class_initialized_via_manager(shared, owner) {
        return None;
    }
    if shared.config.is_jdk_only()
        && cratonvm_native_api::no_image_receiver::is_reviewed_vm_service_receiver(class_name)
    {
        return None;
    }
    if shared.config.static_owner_supers_initialized
        && inherited_static_declarer_initialized(shared, owner, method_name, descriptor)
    {
        return None;
    }
    let init_class = crate::runtime::interpreter::invokestatic_init_class(
        shared,
        owner,
        method_name,
        descriptor,
    );
    (!super::is_class_initialized_via_manager(shared, init_class)).then_some(init_class)
}

/// Whether an `invokestatic` of `(method_name, descriptor)` through the
/// uninitialized class `owner` provably has nothing to initialize: `owner` is
/// a class (not an interface) that does not declare the method itself, and
/// the FIRST proper superclass declaring `(method_name, descriptor)` declares
/// it `static` and is initialized. That superclass is exactly what
/// `invokestatic_init_class` would answer — JVMS §5.4.3.3 looks in the
/// superclass chain before any interface, and the resolver's chain phase
/// returns the first non-abstract match, which a static method is — and an
/// initialized class answers `None` there too. Every other shape answers
/// `false`, and the caller takes the resolution, unchanged: the owner
/// declares it (the owner itself must be initialized), an interface owner,
/// a first match that is an instance method, no match on the chain (a
/// missing method, an interface static, a signature-polymorphic name), or a
/// declaring class not initialized yet.
///
/// One class-manager read and a name/descriptor scan per class of the chain,
/// in place of a `MemberResolver` resolution (a link-resolver probe, an
/// `adopt`, a store lookup) and the second initialized test. Behind
/// `VmConfig::static_owner_supers_initialized` (default off). Interpreter
/// round i3 wave 72, lane L3
/// (`i71-L3-proposal-an-inherited-static-owner-answers-from-its-superclasses-bits-20261104.md`).
#[inline(never)]
pub(in crate::vm::vm_exec) fn inherited_static_declarer_initialized(
    shared: &SharedVm,
    owner: ClassId,
    method_name: &str,
    descriptor: &str,
) -> bool {
    let cm = shared.classes.class_manager.read();
    let Some(owner_class) = cm.get_class(owner) else {
        return false;
    };
    if owner_class.is_interface() || owner_class.find_method(method_name, descriptor).is_some() {
        return false;
    }
    let mut next = owner_class.superclass;
    // Bounded like the other hierarchy walks of this file.
    for _ in 0..64 {
        let Some(class) = next.and_then(|id| cm.get_class(id)) else {
            return false;
        };
        if let Some(method) = class.find_method(method_name, descriptor) {
            return method.is_static() && super::is_class_initialized_fast(class);
        }
        next = class.superclass;
    }
    false
}

/// [`static_owner_init_pending`] for a by-name static route that may run a
/// REGISTERED native: the owner is `owner_override` (a loader-faithful owner
/// the caller dispatches on) or the by-name owner, and is never loaded here —
/// an owner no loader holds has nothing to initialize, as on the JIT's route.
/// A triple no native carries (`might_have_method_descriptor`) pays that one
/// prefilter: its bytecode callee is initialized by `invoke_or_native`'s own
/// tails.
pub(in crate::vm::vm_exec) fn static_native_init_pending(
    shared: &SharedVm,
    owner_override: Option<ClassId>,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<ClassId> {
    if !shared
        .natives
        .native_methods
        .might_have_method_descriptor(method_name, descriptor)
    {
        return None;
    }
    let owner = match owner_override {
        Some(owner) => owner,
        None => {
            let cm = shared.classes.class_manager.read();
            cm.get_loaded_class_id(class_name)?
        }
    };
    static_owner_init_pending(shared, owner, class_name, method_name, descriptor)
}

/// Run `f` with the references in `args` pinned — class loading and
/// `<clinit>` can collect, and `args` is a Rust copy no collector sees — and
/// write them back afterwards.
pub(in crate::vm::vm_exec) fn with_args_pinned<T>(
    shared: &SharedVm,
    thread: &mut JvmThread,
    args: &mut [Value],
    f: impl FnOnce(&SharedVm, &mut JvmThread) -> T,
) -> T {
    let pin_base = thread.native_pin_roots.len();
    let mut pinned: Vec<usize> = Vec::new();
    for (index, value) in args.iter().enumerate() {
        if let Value::Object(Some(obj)) = value {
            if let Some(obj) = shared.mem.heap.is_object_address(obj.as_ptr() as usize) {
                thread.native_pin_roots.push(obj);
                pinned.push(index);
            }
        }
    }
    let out = f(shared, thread);
    for (k, &index) in pinned.iter().enumerate() {
        if let (Some(&obj), Some(slot)) = (
            thread.native_pin_roots.get(pin_base + k),
            args.get_mut(index),
        ) {
            *slot = Value::Object(Some(obj));
        }
    }
    thread.native_pin_roots.truncate(pin_base);
    out
}

/// Initialize `init_class` with the references in `args` pinned across the
/// initializer ([`with_args_pinned`]) and written back afterwards.
pub(in crate::vm::vm_exec) fn initialize_pinning_args(
    shared: &SharedVm,
    thread: &mut JvmThread,
    init_class: ClassId,
    args: &mut [Value],
) -> Result<(), MethodCallFailed> {
    with_args_pinned(shared, thread, args, |shared, thread| {
        super::ensure_class_initialized_shared(shared, thread, init_class)
    })
}

/// The STATIC tail of the by-name routes, on the loaded symbolic owner
/// `owner`: initialize the class that DECLARES the method
/// ([`static_owner_init_pending`]) and dispatch on `owner`, exactly as the
/// instance tail [`invoke_by_class_id_shared`] dispatches. `C.m()` for a
/// static `m` declared in `P` runs `P.<clinit>` and never `C.<clinit>`
/// (JVMS §5.5, JLS §12.4.1), as `execute_invokestatic` does; the instance
/// tail initialized `C`. Interpreter round i1 wave 14, lane L5.
pub(in crate::vm::vm_exec) fn invoke_static_on_owner(
    shared: &SharedVm,
    thread: &mut JvmThread,
    owner: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    match static_owner_init_pending(shared, owner, class_name, method_name, descriptor) {
        Some(init_class) => {
            let mut fresh = args.to_vec();
            initialize_pinning_args(shared, thread, init_class, &mut fresh)?;
            invoke_static_on_class_shared(shared, thread, owner, method_name, descriptor, &fresh)
        }
        None => invoke_static_on_class_shared(shared, thread, owner, method_name, descriptor, args),
    }
}

/// A `REF_invokeStatic` lambda implementation on its loader-faithful owner
/// `impl_cid` (`lambda_impl_dispatch_override*`): initialize the class that
/// declares the method ([`static_owner_init_pending`], JVMS §5.5) and
/// dispatch on `impl_cid` with no retarget. The by-name arm next to each
/// override ([`invoke_static_shared`] / [`invoke_static_or_native`])
/// initializes the same class; the override arms used to initialize nothing.
/// `class_name` is the method handle's owner name. The override arms
/// (`lambda.rs` twice; the native-callback copy that was the third is gone
/// since wave 17) share this one helper. Interpreter round i1 wave 15, lane L5.
pub fn invoke_static_on_class_no_retarget(
    shared: &SharedVm,
    thread: &mut JvmThread,
    impl_cid: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    match static_owner_init_pending(shared, impl_cid, class_name, method_name, descriptor) {
        Some(init_class) => {
            let mut fresh = args.to_vec();
            initialize_pinning_args(shared, thread, init_class, &mut fresh)?;
            invoke_on_class_shared_no_retarget(
                shared,
                thread,
                impl_cid,
                method_name,
                descriptor,
                &fresh,
            )
        }
        None => invoke_on_class_shared_no_retarget(
            shared,
            thread,
            impl_cid,
            method_name,
            descriptor,
            args,
        ),
    }
}

/// [`invoke_shared`] for a STATIC call: load the symbolic owner (the
/// references in `args` pinned across the load, which can run a user loader),
/// then [`invoke_static_on_owner`] — never `<clinit>` of the owner itself
/// when the method is inherited. Also the lambda dispatcher's first
/// `REF_invokeStatic` call (`lambda.rs`).
pub fn invoke_static_shared(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    // The load can run a user loader: pin across it, and copy only when it
    // moved an argument (wave 15: this copied on every call with one).
    let (owner, fresh) = load_class_pinning_args(shared, thread, class_name, args);
    let owner = owner?;
    invoke_static_on_owner(
        shared,
        thread,
        owner,
        class_name,
        method_name,
        descriptor,
        fresh.as_deref().unwrap_or(args),
    )
}

/// [`invoke_static_on_owner`] (`owner`) or [`invoke_static_shared`] (`None`:
/// the owner loaded by name, the arguments pinned across the load) for a
/// `REF_invokeStatic` handle that remembers its settled owner
/// (`NativeInvokeAccess::invoke_static_settling`, interpreter round i1 wave
/// 20, lane L3). When the owner this call dispatches on is `settled`, the
/// call is the tail's `None` arm — dispatch with nothing to initialize —
/// without asking [`static_owner_init_pending`], which pays a class-manager
/// read and a member resolution per call for an inherited static (its owner
/// is never initialized itself). Otherwise it is the tail exactly, and the
/// owner is returned as settled once a successful call leaves nothing to
/// initialize for it: initialization is monotone and the owner's hierarchy
/// is fixed, so that answer holds for every later call on the same owner.
/// A call made while the declaring class is being initialized (by this
/// thread's own `<clinit>`) or has failed does not settle.
#[allow(clippy::too_many_arguments)]
pub fn invoke_static_settling(
    shared: &SharedVm,
    thread: &mut JvmThread,
    owner: Option<ClassId>,
    settled: Option<ClassId>,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> (MethodCallResult, Option<ClassId>) {
    invoke_static_settling_with_first_class(shared, thread, owner, settled, class_name,
        method_name, descriptor, args, None)
}

