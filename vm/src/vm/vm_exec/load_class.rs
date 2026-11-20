#[allow(unused_imports)]
use super::*;

impl<'a> NativeClassAccess for NativeContextImpl<'a> {
    fn load_class(&mut self, name: &str) -> MethodCallResult {
        self.load_class_driving(name, true)
    }

    fn load_class_for_loader_receiver(&mut self, name: &str) -> MethodCallResult {
        let drive_after_miss = !self.shared.classes.load_class_asks_receiver_only;
        self.load_class_driving(name, drive_after_miss)
    }

    fn stack_trace_source_file(&self, class_id: ClassId, captured: Option<&str>)
        -> Option<std::sync::Arc<str>> {
        self.shared.classes.stack_trace_source_file(class_id, captured)
    }

    fn retain_stack_trace_source_file(&self, class_id: ClassId, text: &str, string: ObjectRef) {
        self.shared.classes.retain_stack_trace_source_file(class_id, text, string);
    }

    fn class_source_file(&self, class_id: ClassId) -> Option<String> {
        let cm = self.shared.classes.class_manager.read();
        let file = cm
            .class_store()
            .get(class_id)
            .and_then(|c| c.source_file.as_deref().map(str::to_string));
        drop(cm);
        file
    }

    fn class_name_of_id(&self, class_id: ClassId) -> Option<String> {
        self.shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.name.to_string())
    }

    /// The whole point of the override: `Class::name` is already an
    /// `Arc<str>`, so the answer is a refcount bump rather than the fresh
    /// `String` the default (and `class_name_of_id`) allocates.
    fn class_name_arc_of_id(&self, class_id: ClassId) -> Option<std::sync::Arc<str>> {
        self.shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| std::sync::Arc::clone(&c.name))
    }

    fn class_id_of_object(&self, obj: ObjectRef) -> ClassId {
        // DBG (`CRATONVM_DBG_DEADRECV`): the widest of the three sites — every
        // "what class is this" read lands here, so a victim that reaches
        // neither of the other two is still named. `ClassId::new(0)` on a hit
        // is what a reclaimed header already reads as.
        if deadrecv_check(&self.shared, self.thread, obj, "class_id_of_object") {
            return ClassId::new(0);
        }
        let obj = self.shared.mem.heap.load_and_forward(obj);
        self.shared.mem.heap.class_id_of(obj)
    }

    fn would_fabricate_synthetic_stub(&self, name: &str) -> bool {
        self.shared
            .classes
            .class_manager
            .read()
            .would_fabricate_synthetic_stub(name)
    }

    fn is_class_synthetic_stub(&self, class_name: &str) -> bool {
        match self.shared.load_class_concurrent(class_name) {
            Ok(class_id) => self
                .shared
                .classes
                .class_manager
                .read()
                .get_class(class_id)
                .is_some_and(|c| c.origin.is_compatibility_stub()),
            Err(_) => false,
        }
    }

    /// JIT round 21 wave 14, lane innerclasses: the class's recorded
    /// provenance (`ClassOrigin::has_real_bytes`), read without loading.
    fn class_has_real_bytes(&self, class_id: ClassId) -> bool {
        self.shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .is_some_and(|c| c.origin.has_real_bytes())
    }

    fn loader_id_of_class(&self, class_id: ClassId) -> i32 {
        use cratonvm_types::ClassLoaderId;
        let cm = self.shared.classes.class_manager.read();
        match cm.get_loader_id(class_id) {
            // The canonical encode lives on `ClassLoaderId` next to its two
            // decoders so the pair cannot drift again — hand-written copies of
            // this table at the `define_class_*` / `class_id_*_loader` sites
            // below are exactly what produced the CGLIB `@Configuration`
            // `UserDefined(2)`-vs-`Application` mistagging.
            Some(id) => id.to_native_id() as i32,
            None => ClassLoaderId::Application.to_native_id() as i32,
        }
    }

    fn loader_id_of_class_definite(&self, class_id: ClassId) -> Option<i32> {
        // JIT round 16 wave 13 (lane wmte; MH16-1): the same encoding, without
        // `loader_id_of_class`'s "application" default for an unknown loader.
        let cm = self.shared.classes.class_manager.read();
        cm.get_loader_id(class_id)
            .map(|id| id.to_native_id() as i32)
    }

    fn method_exists(&self, class_name: &str, method_name: &str, descriptor: &str) -> bool {
        let cm = self.shared.classes.class_manager.read();
        let class_id = match cm.get_loaded_class_id(class_name) {
            Some(id) => id,
            None => return false,
        };
        // Walk the class hierarchy looking for the method
        let mut current = Some(class_id);
        while let Some(cid) = current {
            if let Some(class) = cm.class_store.get(cid) {
                if class.find_method(method_name, descriptor).is_some() {
                    return true;
                }
                // Also check if it's a synthetic stub (native-only class) вЂ” methods
                // are registered in the native registry, not in the class file
                if class.origin.is_compatibility_stub() {
                    return true; // assume native methods exist
                }
                current = class.superclass;
            } else {
                break;
            }
        }
        // Also check native method registry
        self.shared
            .natives
            .native_methods
            .find(class_name, method_name, descriptor)
            .is_some()
    }

    fn has_app_workaround(
        &self,
        kind: cratonvm_native_api::AppWorkaroundKind,
        class_name: &str,
    ) -> bool {
        self.shared
            .natives
            .native_methods
            .has_app_workaround(kind, class_name, "", "")
    }

    fn class_declares_method(&self, class_id: ClassId, name: &str, descriptor: &str) -> bool {
        // Declared-only check: inspect this exact class, NOT its superclasses.
        //
        // Exclude compiler-generated bridge methods (ACC_BRIDGE, 0x0040).
        // A bridge is not a genuine override in the OOP sense CratonVM's
        // callers care about ("does this class provide its OWN real
        // implementation") -- it is javac's forwarding stub for visibility
        // (e.g. `StringBuilder.substring(int)` forwarding to its
        // package-private superclass `AbstractStringBuilder`'s real
        // implementation) or covariant-return erasure. Counting it as a
        // "declared override" broke
        // `native_mockito_mock_method_advice_is_overridden`'s ancestor walk:
        // for a Mockito inline mock of `StringBuilder`, it saw
        // `StringBuilder`'s bridge `substring(int)` between the mock's own
        // class and `AbstractStringBuilder` (the reflected Method's real
        // declaring class) and concluded "overridden -- do not intercept
        // here", silently skipping Mockito's advice and returning the real
        // (empty-buffer) computation instead of the stubbed answer. Real
        // JDK reflection call sites never see bridges as "the" declared
        // method for this kind of check (ByteBuddy's own `MethodGraph`
        // merges a bridge into its bridged target), so excluding them here
        // matches that semantics. The other two callers (constructor checks,
        // a `ClassLoader.findResources` override probe) are unaffected:
        // constructors can never be bridges, and `findResources`'s fixed,
        // non-generic signature is never bridge-erased in practice.
        let cm = self.shared.classes.class_manager.read();
        match cm.class_store.get(class_id) {
            Some(class) => class
                .find_method(name, descriptor)
                .is_some_and(|m| !m.is_bridge()),
            None => false,
        }
    }

    /// Round 15 wave 4, lane jdkonly6 (JO5-5): the one method's flags, read
    /// in place (the trait default copies every method's metadata).
    fn declared_method_access_flags(
        &self,
        class_id: ClassId,
        name: &str,
        descriptor: &str,
    ) -> Option<u16> {
        let cm = self.shared.classes.class_manager.read();
        cm.class_store
            .get(class_id)?
            .find_method(name, descriptor)
            .map(|m| m.access_flags.bits())
    }

    fn ensure_class_initialized(&mut self, name: &str) -> Result<ClassId, MethodCallFailed> {
        // Same loader fidelity as `load_class` -- this is the other name-based
        // entry point natives reach for. See `resolve_class_loader_faithful`.
        let class_id = self.resolve_class_loader_faithful(name)?;
        super::ensure_class_initialized_shared(self.shared, self.thread, class_id)?;
        Ok(class_id)
    }

    fn ensure_class_initialized_with_class_id(
        &mut self,
        class_id: ClassId,
    ) -> Result<(), MethodCallFailed> {
        super::ensure_class_initialized_shared(self.shared, self.thread, class_id)
    }

    fn link_class_for_reflection(&mut self, class_id: ClassId) -> Result<(), MethodCallFailed> {
        super::link_class_for_reflection(self.shared, self.thread, class_id)
    }

    fn check_class_path_package_unlinked(
        &mut self,
        class_id: ClassId,
    ) -> Result<(), MethodCallFailed> {
        super::check_class_path_package_unlinked(self.shared, self.thread, class_id)
    }

    fn check_class_path_package_at_door(
        &mut self,
        class_id: ClassId,
    ) -> Result<(), MethodCallFailed> {
        super::check_class_path_package_unlinked_at_door(self.shared, self.thread, class_id)
    }

    fn class_path_package_refused(&self, class_id: ClassId) -> bool {
        super::class_path_package_refused(self.shared, class_id)
    }

    fn claim_builtin_loader_class_record(&self, class_id: ClassId) -> bool {
        super::vm_util::claim_builtin_loader_class_record(self.shared, class_id)
    }

    fn register_lambda_proxy(
        &mut self,
        functional_interface: &str,
        sam_method_name: &str,
        sam_descriptor: &str,
        impl_class: &str,
        impl_member: &str,
        impl_descriptor: &str,
        impl_ref_kind: u8,
        instantiated_descriptor: &str,
        capture_types: &str,
        serializable: bool,
    ) -> u32 {
        use crate::classloading::resolution::{LambdaCallSite, MethodHandle};
        use std::sync::Arc;

        // Reflective `LambdaMetafactory` (e.g. log4j2's `ServiceLoaderUtil`)
        // builds the same lambda metadata the `invokedynamic` opcode would —
        // route it through the identical `lambda_proxies` table so the
        // interpreter's SAM dispatch handles instances uniformly.
        let kind =
            MethodHandleKind::from_tag(impl_ref_kind).unwrap_or(MethodHandleKind::InvokeStatic);
        let proxy_class_id = self.shared.alloc_lambda_proxy_id();
        let call_site = LambdaCallSite {
            functional_interface_id: None,
            functional_interface: Arc::from(functional_interface),
            sam_method_name: Arc::from(sam_method_name),
            sam_descriptor: Arc::from(sam_descriptor),
            impl_handle: MethodHandle {
                kind,
                class_name: Arc::from(impl_class),
                member_name: Arc::from(impl_member),
                descriptor: Arc::from(impl_descriptor),
            },
            instantiated_descriptor: Arc::from(instantiated_descriptor),
            capture_types: capture_types.chars().collect(),
            proxy_class_id,
            serializable_flag: serializable,
        };
        let registered = {
            let mut proxies = self.shared.classes.lambda_proxies.write();
            if crate::runtime::invokedynamic::lambda_proxy_table_has_room(proxies.len()) {
                proxies.insert(proxy_class_id, Arc::new(call_site));
                true
            } else {
                false
            }
        };
        if !registered {
            return 0;
        }
        // gci i11/cl (`gch-h4fl-xlog-class-load-residuals` item 6): the
        // `-Xlog:class+load` line of the hidden class HotSpot spins here,
        // written after the table guard is dropped. No host is recorded on
        // this path, so `lambda_proxy_host` names the implementation's owner.
        crate::runtime::unified_logging::log_lambda_proxy_class_load(self.shared, proxy_class_id);
        proxy_class_id.as_u32()
    }

    fn register_lambda_proxy_markers(&mut self, proxy_class_id: u32, marker_interfaces: &[String]) {
        if proxy_class_id == 0 || marker_interfaces.is_empty() {
            return;
        }
        let names: Vec<std::sync::Arc<str>> = marker_interfaces
            .iter()
            .map(|n| std::sync::Arc::from(n.as_str()))
            .collect();
        crate::runtime::invokedynamic::record_lambda_proxy_markers(
            self.shared.vm_identity,
            ClassId::new(proxy_class_id),
            &names,
        );
    }

    fn register_lambda_proxy_bridges(&mut self, proxy_class_id: u32, bridges: &[String]) {
        if proxy_class_id == 0 || bridges.is_empty() {
            return;
        }
        let descs: Vec<std::sync::Arc<str>> = bridges
            .iter()
            .map(|d| cratonvm_types::intern_arc(d))
            .collect();
        crate::runtime::invokedynamic::record_lambda_proxy_bridges(
            self.shared.vm_identity,
            ClassId::new(proxy_class_id),
            &descs,
        );
    }

    fn register_lambda_proxy_host(&mut self, proxy_class_id: u32, host: ClassId) {
        crate::runtime::invokedynamic::record_reflective_lambda_proxy_host(
            self.shared,
            ClassId::new(proxy_class_id),
            host,
        );
    }

    fn lambda_functional_interface(&self, class_id: ClassId) -> Option<String> {
        self.shared
            .classes
            .lambda_proxies
            .read()
            .get(&class_id)
            .map(|cs| cs.functional_interface.to_string())
    }

    fn lambda_functional_interface_id(&self, class_id: ClassId) -> Option<ClassId> {
        self.shared
            .classes
            .lambda_proxies
            .read()
            .get(&class_id)
            .and_then(|cs| cs.functional_interface_id)
    }

    fn lambda_proxy_host_id(&self, class_id: ClassId) -> Option<ClassId> {
        // Interpreter round i7 wave 142, lane L3: the recorded defining class
        // itself (`lambda_proxy_host` names it; a name is ambiguous once two
        // loaders define it).
        let host_id = self
            .shared
            .classes
            .lambda_proxy_hosts
            .read()
            .get(&class_id)
            .copied()?;
        self.shared
            .classes
            .class_manager
            .read()
            .get_class(host_id)
            .is_some()
            .then_some(host_id)
    }

    fn lambda_proxy_host(&self, class_id: ClassId) -> Option<String> {
        // Prefer the recorded *defining* class (where the lambda / method-ref's
        // invokedynamic appears) — this is what HotSpot names the proxy after and
        // reports as the nest host, even for a cross-class method reference whose
        // implementation method lives in a different class.
        if let Some(host_id) = self
            .shared
            .classes
            .lambda_proxy_hosts
            .read()
            .get(&class_id)
            .copied()
        {
            if let Some(name) = self
                .shared
                .classes
                .class_manager
                .read()
                .get_class(host_id)
                .map(|c| c.name.to_string())
            {
                return Some(name);
            }
        }
        // Fallback (lambda proxies created off the indy bootstrap path, e.g. in
        // tests): the implementation method handle's owner — correct for genuine
        // lambdas and same-class method references.
        self.shared
            .classes
            .lambda_proxies
            .read()
            .get(&class_id)
            .map(|cs| cs.impl_handle.class_name.to_string())
    }

    fn lambda_proxy_record(
        &self,
        class_id: ClassId,
    ) -> Option<cratonvm_native_api::LambdaProxyRecord> {
        // Interpreter round i6 wave 118, lane L3: reflection's reader of the
        // one record the JVMTI and JNI readers use.
        let facts = crate::runtime::lambda_proxy::lambda_proxy_facts(self.shared, class_id)?;
        let methods = facts
            .methods
            .iter()
            .map(|m| cratonvm_native_api::MethodMetadata {
                name: m.name.to_string(),
                descriptor: m.descriptor.to_string(),
                access_flags: m.access_flags,
                declaring_class_id: class_id,
                // The serialization-hostile pair declares
                // `throws NotSerializableException`
                // (`generateSerializationHostileMethods`).
                exceptions: if matches!(&*m.name, "writeObject" | "readObject") {
                    vec!["java/io/NotSerializableException".to_string()]
                } else {
                    Vec::new()
                },
                signature: None,
            })
            .collect();
        Some(cratonvm_native_api::LambdaProxyRecord {
            interfaces: facts.interfaces,
            methods,
        })
    }

    fn lambda_proxy_serial_metadata(
        &self,
        class_id: ClassId,
    ) -> Option<cratonvm_native_api::LambdaSerialMetadata> {
        // Take the call site out from under the lock before reading the
        // marker and bridge tables (their own locks).
        let cs = self
            .shared
            .classes
            .lambda_proxies
            .read()
            .get(&class_id)
            .cloned()?;
        let vm_identity = self.shared.vm_identity;
        // `altMetafactory` markers and bridges: reflection lists them
        // (`getInterfaces`, `getDeclaredMethods`) and deserialization replays
        // them, as HotSpot's `$deserializeLambda$` re-runs the same bootstrap.
        let marker_interfaces: Vec<String> =
            crate::runtime::invokedynamic::lambda_proxy_marker_interfaces(vm_identity, class_id)
                .map(|markers| markers.iter().map(|m| m.name.to_string()).collect())
                .unwrap_or_default();
        let bridges: Vec<String> =
            crate::runtime::invokedynamic::lambda_proxy_bridge_descriptors(vm_identity, class_id)
                .map(|descs| descs.iter().map(|d| d.to_string()).collect())
                .unwrap_or_default();
        Some(cratonvm_native_api::LambdaSerialMetadata {
            functional_interface: cs.functional_interface.to_string(),
            sam_method_name: cs.sam_method_name.to_string(),
            sam_descriptor: cs.sam_descriptor.to_string(),
            impl_class: cs.impl_handle.class_name.to_string(),
            impl_member: cs.impl_handle.member_name.to_string(),
            impl_descriptor: cs.impl_handle.descriptor.to_string(),
            impl_ref_kind: cs.impl_handle.kind.as_tag(),
            instantiated_descriptor: cs.instantiated_descriptor.to_string(),
            capture_types: cs.capture_types.iter().collect(),
            marker_interfaces,
            bridges,
        })
    }

    fn lambda_proxy_serializability(
        &self,
        class_id: ClassId,
    ) -> cratonvm_native_api::LambdaSerializability {
        self.shared.lambda_proxy_serializability(class_id)
    }

    fn is_subclass(&self, child: ClassId, parent: ClassId) -> bool {
        // JIT round 20 wave 17, lane pRefl (`R20PNatAssignable`): through the
        // published secondary-supers closure (`class_is_subtype`: one atomic
        // load and a binary search once `child`'s closure is published, else
        // the same locked `is_subclass_of`, which publishes it) -- the relation
        // the `checkcast` / `instanceof` opcodes and the JNI type checks already
        // ask without the lock. Every reflective native (`Class.isInstance`,
        // `isAssignableFrom`, `cast`, `asSubclass`) took the class-manager read
        // lock here per call. Same answer, positive and negative.
        if crate::runtime::interpreter::class_is_subtype(self.shared, child, parent) {
            return true;
        }
        // Synthetic lambda proxy ClassIds (>= 0x8000_0000) are not in the
        // class manager, so `is_subclass_of` always reports false. Reflection
        // callers like CGLIB's `CallbackInfo.determineType` then conclude that
        // the lambda doesn't implement its functional interface and throw
        // `IllegalStateException("Unknown callback type ...")`. Route through
        // `lambda_proxy_satisfies` so reflective `isAssignableFrom`,
        // `isInstance`, and friends agree with the interpreter's
        // checkcast/instanceof view.
        if child.as_u32() >= 0x8000_0000 {
            return crate::runtime::interpreter::lambda_proxy_satisfies_public(
                self.shared,
                child,
                parent,
            );
        }
        false
    }

    fn class_assignable_to_name(&self, class_id: ClassId, target_class_name: &str) -> Option<bool> {
        // The SAME walk `typecheck::aastore_element_assignable` reaches for when
        // its ClassId-comparing checks have run out — see the boundary's doc for
        // why a reflective caller must not ask a narrower question than the
        // bytecode does.
        Some(
            self.shared
                .classes
                .class_manager
                .read()
                .is_assignable_to_name(class_id, target_class_name),
        )
    }

    /// nio21 wave 6 L1: this VM's own redefinition log, while the thread's
    /// invoke cache is bound to it (`CRATONVM_INVOKE_CACHE_PER_VM_REDEFINITIONS`),
    /// exactly as the obsolete-frame passes ask it
    /// (`obsolete_frames::vm_redefined_classes_between`); unbound, `None`
    /// sends the caller to the process ring. See the trait.
    fn redefined_classes_between(
        &self,
        after: u64,
        upto: u64,
        visit: &mut dyn FnMut(ClassId),
    ) -> Option<bool> {
        if !self.thread.invoke_cache.has_redefinition_log() {
            return None;
        }
        Some(
            self.shared
                .classes
                .redefinition_log
                .for_each_between(after, upto, |c| visit(c)),
        )
    }

    fn array_value_casts_to_desc(
        &self,
        value: cratonvm_types::ObjectRef,
        target_desc: &str,
    ) -> Option<bool> {
        // `checkcast`'s own pair, reached through the same `pub use
        // typecheck::*` re-export as `aastore_element_assignable` below, so
        // this answers exactly as the opcode and `jit_checkcast` do (one
        // strict array rule in every mode since i10-L2).
        let value = self.shared.mem.heap.load_and_forward(value);
        let src = crate::runtime::interpreter::array_descriptor_of(self.shared, value)?;
        if crate::runtime::interpreter::array_is_assignable_to(self.shared, &src, target_desc) {
            return Some(true);
        }
        // That predicate resolves the innermost component classes by NAME,
        // with no loader. When two loaders define one of those names, it may
        // have compared the wrong class, and a refusal would be a false
        // `ClassCastException` on a correct store. The callers throw only on
        // `Some(false)`, so answer "cannot tell" for that case instead.
        let ambiguous = |desc: &str| {
            desc.trim_start_matches('[')
                .strip_prefix('L')
                .and_then(|c| c.strip_suffix(';'))
                .is_some_and(|name| {
                    matches!(
                        self.classify_class_name(name),
                        cratonvm_native_api::NameLookup::Ambiguous { .. }
                    )
                })
        };
        if ambiguous(&src) || ambiguous(target_desc) {
            return None;
        }
        Some(false)
    }

    fn synthetic_implements_declared(&self, class_id: ClassId, target_class_name: &str) -> bool {
        // Via the `pub use typecheck::*` re-export in `interpreter.rs`, exactly
        // as `aastore_element_assignable` below reaches its predicate: one
        // implementation, three doors (the `checkcast`/`instanceof` opcodes, the
        // JIT's `jit_checkcast`/`jit_instanceof` helpers, and reflection).
        crate::runtime::interpreter::synthetic_implements_public(
            self.shared,
            class_id,
            target_class_name,
        )
    }

    fn superclass_of(&self, class_id: ClassId) -> Option<ClassId> {
        self.shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .and_then(|c| c.superclass)
    }


    fn aastore_element_assignable(
        &self,
        array: cratonvm_types::ObjectRef,
        value: cratonvm_types::ObjectRef,
    ) -> Option<bool> {
        // Via the `pub use typecheck::*` re-export in `interpreter.rs`, which
        // caps each item at its own declared visibility — so this reaches the
        // `pub(crate)` predicate without widening the module.
        let array = self.shared.mem.heap.load_and_forward(array);
        let value = self.shared.mem.heap.load_and_forward(value);
        Some(crate::runtime::interpreter::aastore_element_assignable(
            self.shared,
            array,
            value,
        ))
    }

    fn array_instance_of_array_class(
        &self,
        array: cratonvm_types::ObjectRef,
        target_array_class: ClassId,
    ) -> Option<bool> {
        let array = self.shared.mem.heap.load_and_forward(array);
        // The bytecode's own identity verdict (`instanceof` / `checkcast`
        // under `--real-jdk`); `None` for a target with no recorded component.
        crate::runtime::interpreter::array_cast_resolved_verdict(
            self.shared,
            array,
            target_array_class,
        )
    }

    fn array_class_assignable_to_array_class(
        &self,
        src_array_class: ClassId,
        target_array_class: ClassId,
    ) -> Option<bool> {
        crate::runtime::interpreter::array_class_cast_resolved_verdict(
            self.shared,
            src_array_class,
            target_array_class,
        )
    }

    fn class_id_by_name(&self, name: &str) -> Option<ClassId> {
        self.shared
            .classes
            .class_manager
            .read()
            .find_unique_class_by_name(name)
    }

    /// JIT r23 wave 3c, lane L5: the per-VM thread-local cache
    /// (`object_class_id`, keyed by `vm_identity`), not a name lookup under the
    /// class-manager lock per call. `java/lang/Object` is defined once, by the
    /// boot loader, so the bootstrap lookup is the unique-name answer.
    fn object_class_id(&self) -> Option<ClassId> {
        object_class_id(self.shared)
    }

    /// The real answer behind [`Self::class_id_by_name`]'s `None`.
    ///
    /// `find_unique_class_by_name` above already fails closed on an ambiguous
    /// name — it returns `None` rather than picking one of several same-named
    /// classes — but `None` is also what an absent name returns, and the two
    /// demand opposite actions from a native that would load or fabricate on a
    /// miss. `classify_loaded_name` is the same index read (O(1) against the
    /// per-name definition count) with the two cases kept apart, so this costs
    /// a native nothing until it asks.
    ///
    /// The variant mapping is one-to-one on purpose: `native-api` cannot
    /// depend on `classloading`, so `NameLookup` mirrors `NameResolution`, and
    /// this method is the single place the two meet. A future variant added on
    /// either side must fail to compile here rather than silently fold into a
    /// neighbouring case.
    fn classify_class_name(&self, name: &str) -> cratonvm_native_api::NameLookup {
        use cratonvm_classloading::NameResolution;
        use cratonvm_native_api::NameLookup;

        match self
            .shared
            .classes
            .class_manager
            .read()
            .classify_loaded_name(name)
        {
            NameResolution::Absent => NameLookup::Absent,
            NameResolution::Unique(id) => NameLookup::Unique(id),
            NameResolution::Ambiguous { definitions } => NameLookup::Ambiguous { definitions },
        }
    }

    /// Real initialization state, for `Unsafe.shouldBeInitialized` and the
    /// reflective / handle doors. Reuses the helper the interpreter itself
    /// uses, so this is the VM's initialized-bitmap hit plus (on a clear bit)
    /// one class-manager read — it does NOT run `<clinit>`.
    fn is_class_initialized(&self, class_id: ClassId) -> bool {
        super::vm_util::is_class_initialized_via_manager(&self.shared, class_id)
    }

    fn class_id_by_name_near(&self, name: &str, near: ClassId) -> Option<ClassId> {
        self.shared
            .classes
            .class_manager
            .read()
            .find_class_by_name_for_class(name, near)
    }

    // JIT round 17 wave 6, lane mh6: the MH doors' constant-pool view of a name
    // (the referencing loader's initiated/delegated classes), never loading.
    fn class_id_seen_by_class_without_loading(
        &self,
        name: &str,
        referencing: ClassId,
    ) -> Option<ClassId> {
        let cm = self.shared.classes.class_manager.read();
        crate::runtime::interpreter::class_resolved_without_loading(
            &self.shared,
            &cm,
            referencing,
            name,
        )
    }

    // JIT round 17 wave 6, lane mh6 (`CRATONVM_MH_SITE_ACCESS_CHECK`): the
    // constant-pool door's access verdict for a class an MH call site names.
    fn class_access_denial_for_name(&self, referencing: ClassId, name: &str) -> Option<String> {
        if !crate::runtime::interpreter::constants::class_constant_access_enforced(self.shared) {
            return None;
        }
        let denial = {
            let cm = self.shared.classes.class_manager.read();
            crate::runtime::interpreter::constants::class_constant_access_denial_in(
                self.shared,
                &cm,
                referencing,
                name,
                None,
            )
        };
        // Interpreter round i5 wave 100, lane L4 (probe `L4W100UnnamedModuleHashTexts`):
        // the doors' copy names an unnamed-module accessor as HotSpot does.
        denial.map(|message| {
            super::access_denial_with_unnamed_module_hash(self.shared, referencing, message)
        })
    }

    fn class_id_by_name_delegated(&self, name: &str) -> Option<ClassId> {
        self.shared
            .classes
            .class_manager
            .read()
            .resolve_fast_path_class_id(name)
    }

    fn class_id_by_name_via_referencing_class(
        &mut self,
        referencing_class_id: ClassId,
        name: &str,
    ) -> Result<ClassId, cratonvm_types::error::MethodCallFailed> {
        // `resolve_class_or_array_loader_aware`, not the plain variant: an
        // array descriptor arriving here (`Class.forName("[Lp/X;")`,
        // `Array.newInstance`, generic-signature parsing) must be keyed on its
        // component's defining loader per JVMS §5.3.3, and the plain resolver
        // explicitly declines `[` names.
        crate::runtime::interpreter::resolve_class_or_array_loader_aware(
            self.shared,
            self.thread,
            referencing_class_id,
            name,
        )
    }

    fn is_record_class(&self, class_id: ClassId) -> bool {
        let cm = self.shared.classes.class_manager.read();
        // A `Record` attribute with no component is a record too
        // (`ClassManager::empty_record_classes`; i4 wave 84, lane L3).
        cm.get_class(class_id)
            .is_some_and(|c| c.is_record() || cm.empty_record_classes.contains(&class_id))
    }

    fn object_method_fast_path(&self, class_id: ClassId) -> (u8, usize, usize) {
        /// Bit 3 of the returned mask — identity `hashCode`/`equals`.
        const IDENTITY_SEMANTICS: u8 = 1 << 3;
        /// Bit 4 — `java.lang.String`.
        const IS_STRING: u8 = 1 << 4;
        /// Bit 5 — exactly `java.util.ArrayList`.
        const IS_ARRAY_LIST: u8 = 1 << 5;
        /// Bit 6 — one of the eight boxed primitives, `.1` its `value` slot.
        const BOXED_PRIMITIVE: u8 = 1 << 6;
        /// Bit 7 — with bit 6: `java.lang.Boolean` (hash 1231/1237).
        const BOXED_BOOLEAN: u8 = 1 << 7;
        /// Superclass hops walked looking for `java.lang.Enum`. An enum
        /// constant with a class body adds exactly one (its anonymous
        /// subclass); the small bound keeps a corrupt hierarchy from looping.
        const MAX_SUPER_HOPS: usize = 8;

        let cm = self.shared.classes.class_manager.read();
        let Some(class) = cm.get_class(class_id) else {
            return (0, 0, 0);
        };
        let mut bits =
            class.generated_record_object_methods() & !crate::classloading::RECORD_OBJ_COMPUTED;
        if bits == 0 {
            // Not a record with generated bodies. Three other shapes are worth
            // a native fast path — all common as record components, and each
            // otherwise costing a full name-based `invoke_virtual` per hash:
            //
            //   * `java.lang.String` — the caller uses the compact string
            //     reader instead.
            //   * exactly `java.util.ArrayList` — the caller walks the backing
            //     array (`List.hashCode`/`equals` are contract-fixed).
            //     Exactly, not a subclass: a subclass may override either, and
            //     `java.util.Vector` has a different field layout entirely.
            //   * an enum — JLS §8.9 declares both `Enum.hashCode` and
            //     `Enum.equals` final and identity-based, so no subclass can
            //     change them (Hibernate's `MutationKind`).
            match &*class.name {
                "java/lang/String" => bits |= IS_STRING,
                "java/util/ArrayList" => bits |= IS_ARRAY_LIST,
                // Interpreter round i7 wave 135, lane L3
                // (`i134-R2-record-layout-lookup-is-35x-hotspot`): the eight
                // boxed primitives. Each class is final and its `hashCode` /
                // `equals` are fixed by the JDK (the wrapper hash of `value`;
                // `equals` is "same class, same `value`", `Float`/`Double` by
                // `floatToIntBits`/`doubleToLongBits`), so a caller may answer
                // them from the `value` field, whose slot is `.1` here. Only
                // when the class declares an instance `value` of the
                // primitive's own descriptor; otherwise no bit. Read by the
                // record intrinsic under `CRATONVM_RECORD_BOXED_COMPONENTS`
                // only (default ON since interpreter round i7 wave 144).
                "java/lang/Integer" | "java/lang/Long" | "java/lang/Short"
                | "java/lang/Byte" | "java/lang/Character" | "java/lang/Boolean"
                | "java/lang/Float" | "java/lang/Double" => {
                    let descriptor = match &*class.name {
                        "java/lang/Integer" => "I",
                        "java/lang/Long" => "J",
                        "java/lang/Short" => "S",
                        "java/lang/Byte" => "B",
                        "java/lang/Character" => "C",
                        "java/lang/Boolean" => "Z",
                        "java/lang/Float" => "F",
                        _ => "D",
                    };
                    if let Some((slot, field)) =
                        class.find_own_field_by_descriptor("value", descriptor)
                    {
                        if !field.is_static() {
                            let mut boxed = BOXED_PRIMITIVE;
                            if descriptor == "Z" {
                                boxed |= BOXED_BOOLEAN;
                            }
                            return (boxed, slot, 0);
                        }
                    }
                }
                _ => {
                    let mut superclass = class.superclass;
                    for _ in 0..MAX_SUPER_HOPS {
                        let Some(id) = superclass else { break };
                        let Some(parent) = cm.get_class(id) else {
                            break;
                        };
                        match &*parent.name {
                            "java/lang/Enum" => {
                                bits |= IDENTITY_SEMANTICS;
                                break;
                            }
                            "java/lang/Object" => break,
                            _ => superclass = parent.superclass,
                        }
                    }
                }
            }
        }
        (bits, class.first_field_index, class.record_components.len())
    }

    fn record_components(&self, class_id: ClassId) -> Vec<(String, String)> {
        self.shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| {
                c.record_components
                    .iter()
                    .map(|rc| (rc.name.clone(), rc.descriptor.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn record_component_annotation_bytes(
        &self,
        class_id: ClassId,
        index: usize,
    ) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
        // Interpreter round i2 wave 58, lane L3.
        self.shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .and_then(|c| c.record_components.get(index))
            .map(|rc| {
                (
                    rc.annotations.as_deref().map(<[u8]>::to_vec),
                    rc.type_annotations.as_deref().map(<[u8]>::to_vec),
                )
            })
            .unwrap_or_default()
    }

    fn record_component_signature(&self, class_id: ClassId, index: usize) -> Option<String> {
        // Interpreter round i5 wave 101, lane L3: the component's own
        // Signature, never the same-named field's.
        self.shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .and_then(|c| c.record_components.get(index))
            .and_then(|rc| rc.signature.as_deref().map(str::to_string))
    }

    fn is_sealed_class(&self, class_id: ClassId) -> bool {
        self.shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.is_sealed())
            .unwrap_or(false)
    }

    fn permitted_subclasses(&self, class_id: ClassId) -> Vec<String> {
        self.shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.permitted_subclasses.clone())
            .unwrap_or_default()
    }

    // -- T13: java/lang/Class metadata methods --

    fn class_file_version(&self, class_id: ClassId) -> u16 {
        self.shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.version.major)
            .unwrap_or(65)
    }

    fn class_file_minor_version(&self, class_id: ClassId) -> u16 {
        self.shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.version.minor)
            .unwrap_or(0)
    }

    fn inner_classes(&self, class_id: ClassId) -> Vec<(String, String, String, u16)> {
        self.shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| {
                c.inner_classes
                    .iter()
                    .map(|ic| {
                        (
                            ic.inner_class.to_string(),
                            ic.outer_class.to_string(),
                            ic.inner_name.to_string(),
                            ic.access_flags,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn enclosing_method(&self, class_id: ClassId) -> Option<(String, String, String)> {
        self.shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .and_then(|c| {
                c.enclosing_method.as_ref().map(|em| {
                    (
                        em.class_name.clone(),
                        em.method_name.clone(),
                        em.method_descriptor.clone(),
                    )
                })
            })
    }

    fn enclosing_method_class_name(&self, class_id: ClassId) -> Option<String> {
        // One clone, and a recursive read: the `--compatible` name bridges
        // ask this on every call, possibly under a frame that holds a read
        // (JIT round 21 wave 22, lane hidden-resid; item 4 of
        // `r21-review-w22-findings-FIXED-20261020.md`).
        self.shared
            .classes
            .class_manager
            .read_recursive()
            .get_class(class_id)
            .and_then(|c| c.enclosing_method.as_ref().map(|em| em.class_name.clone()))
    }

    fn declaring_class(&self, class_id: ClassId) -> Option<ClassId> {
        // JIT round 21 wave 16, lane innerclasses
        // (`CRATONVM_DECLARING_CLASS_ANY_OUTER_ROW`, read once per VM; page
        // `r21-review-w15-native-findings-FIXED-20261020.md` item 9): HotSpot's
        // `find_inner_classes_attr` takes the FIRST self row, and
        // `compute_enclosing_class` makes it a member link whenever its outer
        // class is set, with or without a simple name. `=0` restores the old
        // rule (first self row with both). Read before the class-manager lock.
        let any_outer_row =
            cratonvm_native_builtins::lang_class::declaring_class_any_outer_row_enabled(self);
        let cm = self.shared.classes.class_manager.read();
        let class = cm.get_class(class_id)?;
        let outer = cratonvm_native_builtins::lang_class::declaring_member_row_outer(
            class.inner_classes.iter().map(|ic| {
                (
                    ic.inner_class.as_str(),
                    ic.outer_class.as_str(),
                    ic.inner_name.as_str(),
                )
            }),
            &class.name,
            any_outer_row,
        )?;
        // Resolve outer class name to ClassId
        cm.find_class_by_name_for_class(outer, class_id)
    }

    fn nest_host_name(&self, class_id: ClassId) -> Option<String> {
        self.shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .and_then(|c| c.nest_host.clone())
    }

    fn nest_member_names(&self, class_id: ClassId) -> Vec<String> {
        self.shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.nest_members.clone())
            .unwrap_or_default()
    }

    fn class_num_total_fields(&self, class_id: ClassId) -> usize {
        self.shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.num_total_fields)
            .unwrap_or(0)
    }

    fn declared_fields(&self, class_id: ClassId) -> Vec<FieldMetadata> {
        self.declared_fields_matching(class_id, None).0
    }

    // Interpreter round i5 wave 102, lane L4 (`CRATONVM_MHN_RESOLVE_NAMED_FIELDS`):
    // metadata for the fields of one name only; the slot numbering still
    // counts every field, so the matches carry the slots `declared_fields` gives.
    fn declared_fields_named(&self, class_id: ClassId, name: &str) -> (Vec<FieldMetadata>, bool) {
        self.declared_fields_matching(class_id, Some(name))
    }

    fn declared_methods(&self, class_id: ClassId) -> Vec<MethodMetadata> {
        self.declared_methods_matching(class_id, None).0
    }

    // Interpreter round i5 wave 100, lane L4 (`CRATONVM_MHN_RESOLVE_NAMED_METHODS`):
    // metadata for the methods of one name only, built from the method table
    // without materialising the others.
    fn declared_methods_named(&self, class_id: ClassId, name: &str) -> (Vec<MethodMetadata>, bool) {
        self.declared_methods_matching(class_id, Some(name))
    }

    fn class_interfaces(&self, class_id: ClassId) -> Vec<ClassId> {
        let cm = self.shared.classes.class_manager.read();
        cm.get_class(class_id)
            .map(|c| c.interfaces.clone())
            .unwrap_or_default()
    }

    fn class_access_flags(&self, class_id: ClassId) -> u16 {
        let cm = self.shared.classes.class_manager.read();
        cm.get_class(class_id)
            .map(|c| c.access_flags.bits())
            .unwrap_or(0)
    }

    fn primitive_class_mirror(&mut self, name: &str) -> ObjectRef {
        super::get_or_create_primitive_mirror(self.shared, name)
    }

    fn class_annotations(&self, class_id: ClassId) -> Vec<crate::native::registry::AnnotationData> {
        let cm = self.shared.classes.class_manager.read();
        let class = match cm.get_class(class_id) {
            Some(c) => c,
            None => return Vec::new(),
        };
        let mut result = Vec::new();
        for ann in &class.annotations {
            if let Some(data) = convert_annotation(ann, &class.constant_pool) {
                result.push(data);
            }
        }
        result
    }

    fn method_annotations(
        &self,
        class_id: ClassId,
        method_name: &str,
        method_desc: &str,
    ) -> Vec<crate::native::registry::AnnotationData> {
        let cm = self.shared.classes.class_manager.read();
        let class = match cm.get_class(class_id) {
            Some(c) => c,
            None => return Vec::new(),
        };
        for m in &class.methods {
            if &*m.name == method_name && &*m.descriptor == method_desc {
                return extract_annotations_from_attributes(&m.attributes, &class.constant_pool);
            }
        }
        Vec::new()
    }

    fn field_annotations(
        &self,
        class_id: ClassId,
        field_name: &str,
    ) -> Vec<crate::native::registry::AnnotationData> {
        let cm = self.shared.classes.class_manager.read();
        let class = match cm.get_class(class_id) {
            Some(c) => c,
            None => return Vec::new(),
        };
        for f in &class.fields {
            if &*f.name == field_name {
                return extract_annotations_from_attributes(&f.attributes, &class.constant_pool);
            }
        }
        Vec::new()
    }

    fn class_signature(&self, class_id: ClassId) -> Option<String> {
        let cm = self.shared.classes.class_manager.read();
        let class = cm.get_class(class_id)?;
        class.signature.clone()
    }

    fn method_signature(
        &self,
        class_id: ClassId,
        method_name: &str,
        method_desc: &str,
    ) -> Option<String> {
        let cm = self.shared.classes.class_manager.read();
        let class = cm.get_class(class_id)?;
        for m in &class.methods {
            if &*m.name == method_name && &*m.descriptor == method_desc {
                for attr in &m.attributes {
                    if let Some(cratonvm_reader::attribute::Attribute::Signature(s)) =
                        attr.as_decoded()
                    {
                        // `s: &Arc<str>` (round 4 reader). Caller wants
                        // an owned `String`; materialise once.
                        return Some(s.to_string());
                    }
                }
                return None;
            }
        }
        None
    }

    fn method_parameters(
        &self,
        class_id: ClassId,
        method_name: &str,
        method_desc: &str,
    ) -> Vec<(String, u16)> {
        let cm = self.shared.classes.class_manager.read();
        let class = match cm.get_class(class_id) {
            Some(c) => c,
            None => return Vec::new(),
        };
        for m in &class.methods {
            if &*m.name == method_name && &*m.descriptor == method_desc {
                for attr in &m.attributes {
                    if let Some(cratonvm_reader::attribute::Attribute::MethodParameters(params)) =
                        attr.as_decoded()
                    {
                        // JVMS 4.7.24: name_index == 0 means an anonymous /
                        // synthetic parameter вЂ” surface it as an empty
                        // string so the caller can fall back to "argN".
                        return params
                            .iter()
                            .map(|p| {
                                let name = if p.name_index == 0 {
                                    String::new()
                                } else {
                                    class
                                        .constant_pool
                                        .get_utf8(p.name_index)
                                        .unwrap_or("")
                                        .to_string()
                                };
                                (name, p.access_flags)
                            })
                            .collect();
                    }
                }
                return Vec::new();
            }
        }
        Vec::new()
    }

    fn method_parameters_attribute(
        &self,
        class_id: ClassId,
        method_name: &str,
        method_desc: &str,
    ) -> Option<Vec<(cratonvm_native_api::MethodParameterName, u16)>> {
        use cratonvm_native_api::MethodParameterName as N;
        // Interpreter round i5 wave 101, lane L3: as `JVM_GetMethodParameters`
        // reads it -- every entry (a zero-entry attribute is not "absent"),
        // with the `name_index` checks it makes before building the array.
        let cm = self.shared.classes.class_manager.read();
        let class = cm.get_class(class_id)?;
        let m = class
            .methods
            .iter()
            .find(|m| &*m.name == method_name && &*m.descriptor == method_desc)?;
        let params = m.attributes.iter().find_map(|attr| match attr.as_decoded() {
            Some(cratonvm_reader::attribute::Attribute::MethodParameters(params)) => Some(params),
            _ => None,
        })?;
        let cp = &class.constant_pool;
        Some(
            params
                .iter()
                .map(|p| {
                    let name = if p.name_index == 0 {
                        N::Unnamed
                    } else if usize::from(p.name_index) >= cp.len() {
                        N::OutOfBounds
                    } else if let Some(wide) = cp.get_utf8_wide(p.name_index) {
                        N::Name(String::from_utf16_lossy(wide))
                    } else {
                        match cp.get_utf8(p.name_index) {
                            Some(s) => N::Name(s.to_string()),
                            None => N::NotUtf8,
                        }
                    };
                    (name, p.access_flags)
                })
                .collect(),
        )
    }

    fn field_signature(&self, class_id: ClassId, field_name: &str) -> Option<String> {
        let cm = self.shared.classes.class_manager.read();
        let class = cm.get_class(class_id)?;
        for f in &class.fields {
            if &*f.name == field_name {
                for attr in &f.attributes {
                    if let Some(cratonvm_reader::attribute::Attribute::Signature(s)) =
                        attr.as_decoded()
                    {
                        // `s: &Arc<str>` (round 4 reader). Caller wants
                        // an owned `String`; materialise once.
                        return Some(s.to_string());
                    }
                }
                return None;
            }
        }
        None
    }

    fn method_parameter_annotations(
        &self,
        class_id: ClassId,
        method_name: &str,
        method_desc: &str,
    ) -> Vec<Vec<crate::native::registry::AnnotationData>> {
        let cm = self.shared.classes.class_manager.read();
        let class = match cm.get_class(class_id) {
            Some(c) => c,
            None => return Vec::new(),
        };
        for m in &class.methods {
            if &*m.name == method_name && &*m.descriptor == method_desc {
                return extract_parameter_annotations(&m.attributes, &class.constant_pool);
            }
        }
        Vec::new()
    }

    fn method_return_type_annotations(
        &self,
        class_id: ClassId,
        method_name: &str,
        method_desc: &str,
    ) -> Vec<crate::native::registry::AnnotationData> {
        let cm = self.shared.classes.class_manager.read();
        let class = match cm.get_class(class_id) {
            Some(c) => c,
            None => return Vec::new(),
        };
        for m in &class.methods {
            if &*m.name == method_name && &*m.descriptor == method_desc {
                return extract_return_type_annotations(&m.attributes, &class.constant_pool);
            }
        }
        Vec::new()
    }

    fn method_return_type_argument_annotations(
        &self,
        class_id: ClassId,
        method_name: &str,
        method_desc: &str,
    ) -> Vec<crate::native::registry::TypeArgAnnotations> {
        let cm = self.shared.classes.class_manager.read();
        let class = match cm.get_class(class_id) {
            Some(c) => c,
            None => return Vec::new(),
        };
        for m in &class.methods {
            if &*m.name == method_name && &*m.descriptor == method_desc {
                return extract_return_type_argument_annotations(
                    &m.attributes,
                    &class.constant_pool,
                );
            }
        }
        Vec::new()
    }

    fn method_parameter_type_annotations(
        &self,
        class_id: ClassId,
        method_name: &str,
        method_desc: &str,
    ) -> Vec<Vec<crate::native::registry::AnnotationData>> {
        let cm = self.shared.classes.class_manager.read();
        let class = match cm.get_class(class_id) {
            Some(c) => c,
            None => return Vec::new(),
        };
        for m in &class.methods {
            if &*m.name == method_name && &*m.descriptor == method_desc {
                return extract_parameter_type_annotations(&m.attributes, &class.constant_pool);
            }
        }
        Vec::new()
    }

    fn field_type_annotations(
        &self,
        class_id: ClassId,
        field_name: &str,
    ) -> Vec<crate::native::registry::AnnotationData> {
        let cm = self.shared.classes.class_manager.read();
        let class = match cm.get_class(class_id) {
            Some(c) => c,
            None => return Vec::new(),
        };
        for f in &class.fields {
            if &*f.name == field_name {
                return extract_field_type_annotations(&f.attributes, &class.constant_pool);
            }
        }
        Vec::new()
    }

    fn field_type_argument_annotations(
        &self,
        class_id: ClassId,
        field_name: &str,
    ) -> Vec<crate::native::registry::TypeArgAnnotations> {
        let cm = self.shared.classes.class_manager.read();
        let class = match cm.get_class(class_id) {
            Some(c) => c,
            None => return Vec::new(),
        };
        for f in &class.fields {
            if &*f.name == field_name {
                return extract_field_type_argument_annotations(
                    &f.attributes,
                    &class.constant_pool,
                );
            }
        }
        Vec::new()
    }

    fn method_parameter_type_argument_annotations(
        &self,
        class_id: ClassId,
        method_name: &str,
        method_desc: &str,
    ) -> Vec<Vec<crate::native::registry::TypeArgAnnotations>> {
        let cm = self.shared.classes.class_manager.read();
        let class = match cm.get_class(class_id) {
            Some(c) => c,
            None => return Vec::new(),
        };
        for m in &class.methods {
            if &*m.name == method_name && &*m.descriptor == method_desc {
                return extract_parameter_type_argument_annotations(
                    &m.attributes,
                    &class.constant_pool,
                );
            }
        }
        Vec::new()
    }

    /// `Class.getAnnotatedSuperclass()` / `Class.getAnnotatedInterfaces()`
    /// (and their `getAnnotatedActualTypeArguments()` chains) — see
    /// `extract_class_extends_type_annotations` for why this re-parses the
    /// cached original bytes rather than reading from `Class`.
    fn class_extends_type_annotations(
        &self,
        class_id: ClassId,
        supertype_index: u16,
    ) -> crate::native::registry::TypeArgAnnotations {
        let bytes = {
            let cm = self.shared.classes.class_manager.read();
            match cm.defined_class_bytes(class_id) {
                Some(b) => b,
                None => return Default::default(),
            }
        };
        extract_class_extends_type_annotations(&bytes, supertype_index)
    }

    fn method_annotation_default(
        &self,
        class_id: ClassId,
        method_name: &str,
        method_desc: &str,
    ) -> Option<crate::native::registry::AnnotationElementValue> {
        let cm = self.shared.classes.class_manager.read();
        let class = match cm.get_class(class_id) {
            Some(c) => c,
            None => return None,
        };
        for m in &class.methods {
            if &*m.name == method_name && &*m.descriptor == method_desc {
                for attr in &m.attributes {
                    if let Some(cratonvm_reader::attribute::Attribute::AnnotationDefault(ev)) =
                        attr.as_decoded()
                    {
                        return convert_element_value(ev, &class.constant_pool);
                    }
                }
                return None;
            }
        }
        None
    }

    /// SBR wave 5 (reflmeta): `Class.getRawAnnotations`'s bytes -- the kept
    /// (visible-only) class annotations written back index for index.
    fn raw_annotations(&self, class_id: ClassId) -> Vec<u8> {
        let cm = self.shared.classes.class_manager.read();
        cm.get_class(class_id)
            .and_then(|c| {
                crate::runtime::instrument_reconstitute::class_annotations_body(&c.annotations)
            })
            .unwrap_or_default()
    }

    /// SBR wave 5 (reflmeta): `Class.getRawTypeAnnotations`'s bytes, the body
    /// `ClassManager::class_type_annotations` keeps at define time (`Class`
    /// itself does not keep class-level type annotations, and the class-bytes
    /// cache is FIFO-evicted: reading it dropped a class's type-parameter
    /// annotations once enough later classes had loaded).
    fn raw_type_annotations(&self, class_id: ClassId) -> Vec<u8> {
        let cm = self.shared.classes.class_manager.read();
        cm.class_type_annotations
            .get(&class_id)
            .map(|b| b.as_ref().to_vec())
            .unwrap_or_default()
    }

    fn member_raw_annotation_attributes(
        &self,
        class_id: ClassId,
        member_name: &str,
        member_desc: &str,
        is_method: bool,
        attrs: &[cratonvm_native_api::RawAnnotationAttr],
    ) -> Vec<Option<Vec<u8>>> {
        use cratonvm_native_api::RawAnnotationAttr as A;
        let mut out = vec![None; attrs.len()];
        let cm = self.shared.classes.class_manager.read();
        let Some(class) = cm.get_class(class_id) else {
            return out;
        };
        let attributes = if is_method {
            class
                .methods
                .iter()
                .find(|m| &*m.name == member_name && &*m.descriptor == member_desc)
                .map(|m| &m.attributes)
        } else {
            class
                .fields
                .iter()
                .find(|f| &*f.name == member_name && &*f.descriptor == member_desc)
                .map(|f| &f.attributes)
        };
        let Some(attributes) = attributes else {
            return out;
        };
        for (slot, attr) in attrs.iter().enumerate() {
            let name = match attr {
                A::Annotations => "RuntimeVisibleAnnotations",
                A::ParameterAnnotations => "RuntimeVisibleParameterAnnotations",
                A::AnnotationDefault => "AnnotationDefault",
                A::TypeAnnotations => "RuntimeVisibleTypeAnnotations",
            };
            out[slot] = attributes.iter().find_map(|a| {
                crate::runtime::instrument_reconstitute::annotation_attribute_body(a, name)
            });
        }
        out
    }

    fn reflect_constant_pool_size(&self, class_id: ClassId) -> Option<usize> {
        {
            let cm = self.shared.classes.class_manager.read();
            if let Some(c) = cm.get_class(class_id) {
                return Some(c.constant_pool.len());
            }
        }
        // Interpreter round i5 wave 107, lane L3
        // (`i106-L3-lambda-proxy-classes-have-no-constant-pool`,
        // `L3W106LambdaProxyConstantPool`): a lambda proxy has no class file;
        // its pool is the modelled spun class's (`lambda_proxy_spun_class`,
        // measured against HotSpot 25 for JVMTI `GetConstantPool`), so the
        // reflective view and the debugger's agree. Read with the class
        // manager's lock released (the model takes it).
        crate::runtime::lambda_proxy::lambda_proxy_spun_class_of(self.shared, class_id)
            .map(|spun| spun.pool.len() + 1)
    }

    fn reflect_constant_pool_entry(
        &self,
        class_id: ClassId,
        index: i32,
    ) -> cratonvm_native_api::ReflectConstant {
        use cratonvm_native_api::ReflectConstant as R;
        use cratonvm_reader::constant_pool::ConstantPoolEntry as E;
        let Ok(index) = u16::try_from(index) else {
            return R::Invalid;
        };
        let cm = self.shared.classes.class_manager.read();
        let Some(class) = cm.get_class(class_id) else {
            // A lambda proxy: its modelled spun class's pool (wave 107, lane
            // L3; see `reflect_constant_pool_size`).
            drop(cm);
            return crate::runtime::lambda_proxy::lambda_proxy_spun_class_of(self.shared, class_id)
                .map(|spun| lambda_proxy_reflect_constant(&spun.pool, index))
                .unwrap_or(R::Invalid);
        };
        let cp = &class.constant_pool;
        let units = |i: u16| -> Option<Vec<u16>> {
            match cp.get_utf8_wide(i) {
                Some(w) => Some(w.to_vec()),
                None => cp.get_utf8(i).map(|s| s.encode_utf16().collect()),
            }
        };
        match cp.get(index) {
            None | Some(E::Tombstone) => R::Invalid,
            Some(E::Utf8(_)) => units(index).map(R::Utf8).unwrap_or(R::Invalid),
            Some(E::Integer(v)) => R::Integer(*v),
            Some(E::Float(v)) => R::Float(*v),
            Some(E::Long(v)) => R::Long(v.get()),
            Some(E::Double(v)) => R::Double(v.get()),
            Some(E::StringReference { string_index }) => {
                units(*string_index).map(R::String).unwrap_or(R::Invalid)
            }
            Some(E::ClassReference { name_index }) => cp
                .get_utf8(*name_index)
                .map(|s| R::Class(s.to_string()))
                .unwrap_or(R::Invalid),
            // Interpreter round i5 wave 106, lane L3: the member, name-and-type
            // and dynamic entries carry what `ConstantPool.getClassRefIndexAt0`,
            // `getNameAndTypeRefIndexAt0`, `getMemberRefInfoAt0` and
            // `getNameAndTypeRefInfoAt0` answer (`L3W106ConstantPoolRefs`).
            Some(
                E::FieldReference {
                    class_index,
                    name_and_type_index,
                }
                | E::MethodReference {
                    class_index,
                    name_and_type_index,
                }
                | E::InterfaceMethodReference {
                    class_index,
                    name_and_type_index,
                },
            ) => {
                let tag = match cp.get(index) {
                    Some(E::FieldReference { .. }) => 9,
                    Some(E::MethodReference { .. }) => 10,
                    _ => 11,
                };
                let class_name = match cp.get(*class_index) {
                    Some(E::ClassReference { name_index }) => cp.get_utf8(*name_index),
                    _ => None,
                };
                let name_and_type = match cp.get(*name_and_type_index) {
                    Some(E::NameAndType {
                        name_index,
                        descriptor_index,
                    }) => cp.get_utf8(*name_index).zip(cp.get_utf8(*descriptor_index)),
                    _ => None,
                };
                match (class_name, name_and_type) {
                    (Some(class_name), Some((name, descriptor))) => R::MemberRef {
                        tag,
                        class_index: *class_index,
                        name_and_type_index: *name_and_type_index,
                        class_name: class_name.to_string(),
                        name: name.to_string(),
                        descriptor: descriptor.to_string(),
                    },
                    _ => R::Other(tag),
                }
            }
            Some(E::NameAndType {
                name_index,
                descriptor_index,
            }) => match (cp.get_utf8(*name_index), cp.get_utf8(*descriptor_index)) {
                (Some(name), Some(descriptor)) => R::NameAndType {
                    name: name.to_string(),
                    descriptor: descriptor.to_string(),
                },
                _ => R::Other(12),
            },
            Some(E::MethodHandle { .. }) => R::Other(15),
            Some(E::MethodType { .. }) => R::Other(16),
            Some(E::Dynamic {
                name_and_type_index,
                ..
            }) => R::DynamicRef {
                tag: 17,
                name_and_type_index: *name_and_type_index,
            },
            Some(E::InvokeDynamic {
                name_and_type_index,
                ..
            }) => R::DynamicRef {
                tag: 18,
                name_and_type_index: *name_and_type_index,
            },
            Some(E::Module { .. }) => R::Other(19),
            Some(E::Package { .. }) => R::Other(20),
        }
    }

    fn method_exceptions(
        &self,
        class_id: ClassId,
        method_name: &str,
        method_desc: &str,
    ) -> Vec<String> {
        let cm = self.shared.classes.class_manager.read();
        let class = match cm.get_class(class_id) {
            Some(c) => c,
            None => return Vec::new(),
        };
        for m in &class.methods {
            if &*m.name == method_name && &*m.descriptor == method_desc {
                for attr in &m.attributes {
                    if let Some(cratonvm_reader::attribute::Attribute::Exceptions {
                        exception_indices,
                    }) = attr.as_decoded()
                    {
                        return exception_indices
                            .iter()
                            .filter_map(|idx| {
                                class
                                    .constant_pool
                                    .get_class_name(*idx)
                                    .map(|s| s.to_string())
                            })
                            .collect();
                    }
                }
                return Vec::new();
            }
        }
        Vec::new()
    }

    fn module_name_of_class(&self, class_id: ClassId) -> Option<String> {
        self.shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .and_then(|c| c.module_name.clone())
    }

    /// Interpreter round i1 wave 45, lane L5: see the trait. The layer module
    /// is `access_control::layer_module_of`'s answer (the loader's
    /// package-to-module map `defineModule0` fills, `--real-jdk`), the one
    /// the VM's access control and `Class.getModule()` already use; every
    /// other class keeps `module_name_of_class`'s answer.
    fn module_access(
        &self,
        accessor: ClassId,
        target: ClassId,
        question: cratonvm_native_api::ModuleQuestion,
    ) -> Option<bool> {
        let cm = self.shared.classes.class_manager.read();
        let (Some(accessor), Some(target)) = (cm.get_class(accessor), cm.get_class(target)) else {
            return None;
        };
        crate::classloading::access_control::module_access(
            accessor,
            target,
            &cm.module_registry,
            question,
        )
    }

    fn module_identity_of_class(
        &self,
        class_id: ClassId,
    ) -> cratonvm_native_api::ClassModuleIdentity {
        use cratonvm_native_api::ClassModuleIdentity as Ident;
        let cm = self.shared.classes.class_manager.read();
        let Some(class) = cm.get_class(class_id) else {
            return Ident::Unnamed;
        };
        if let Some((loader_ns, name)) =
            crate::classloading::access_control::layer_module_of(class, &cm.module_registry)
        {
            return Ident::Layer {
                loader_ns,
                name: name.to_string(),
                version: cm
                    .module_registry
                    .layer_module_version(loader_ns, name)
                    .map(str::to_string),
            };
        }
        match class.module_name.as_deref() {
            Some(name) if !name.is_empty() => Ident::Named(name.to_string()),
            _ => Ident::Unnamed,
        }
    }

    /// `module m[@v]` / `unnamed module` under HotSpot's
    /// `ModuleEntry::should_show_version()`
    /// (`access_control::message_module_version`). Interpreter round i4 wave
    /// 99, lane L5.
    fn module_clause_of_class(&self, class_id: ClassId) -> String {
        let identity = self.module_identity_of_class(class_id);
        let Some(name) = identity.name() else {
            return identity.describe();
        };
        let cm = self.shared.classes.class_manager.read();
        let version = cm.get_class(class_id).and_then(|class| {
            crate::classloading::access_control::message_module_version(class, &cm.module_registry)
        });
        format!(
            "module {}",
            crate::classloading::access_control::module_with_version(name, version)
        )
    }

    fn reads_module(&self, reader: &str, provider: &str) -> bool {
        let cm = self.shared.classes.class_manager.read();
        if cm.module_registry.is_empty() {
            return true;
        }
        cm.module_registry.reads(reader, provider)
    }

    fn is_package_exported_unqualified(&self, module_name: &str, pkg: &str) -> bool {
        let cm = self.shared.classes.class_manager.read();
        if cm.module_registry.is_empty() {
            return true;
        }
        cm.module_registry
            .is_package_exported_unqualified(module_name, pkg)
    }

    fn is_package_exported_to(&self, module_name: &str, pkg: &str, to_module: &str) -> bool {
        let cm = self.shared.classes.class_manager.read();
        if cm.module_registry.is_empty() {
            return true;
        }
        cm.module_registry
            .is_package_exported_to(module_name, pkg, to_module)
    }

    fn is_package_open_unqualified(&self, module_name: &str, pkg: &str) -> bool {
        let cm = self.shared.classes.class_manager.read();
        if cm.module_registry.is_empty() {
            return true;
        }
        cm.module_registry
            .is_package_open_unqualified(module_name, pkg)
    }

    fn is_package_open_to(&self, module_name: &str, pkg: &str, to_module: &str) -> bool {
        let cm = self.shared.classes.class_manager.read();
        if cm.module_registry.is_empty() {
            return true;
        }
        cm.module_registry
            .is_package_open_to(module_name, pkg, to_module)
    }

    fn is_package_exported_or_open_to_layer_module(
        &self,
        module_name: &str,
        pkg: &str,
        layer_module: &str,
        layer_loader_ns: Option<u32>,
        open: bool,
    ) -> Option<bool> {
        let cm = self.shared.classes.class_manager.read();
        if cm.module_registry.is_empty() {
            return None;
        }
        Some(if open {
            cm.module_registry.is_package_open_to_layer_module(
                module_name,
                pkg,
                layer_module,
                layer_loader_ns,
            )
        } else {
            cm.module_registry.is_package_exported_to_layer_module(
                module_name,
                pkg,
                layer_module,
                layer_loader_ns,
            )
        })
    }

    fn module_add_reads(&mut self, reader: &str, provider: &str) {
        self.shared
            .classes
            .class_manager
            .write()
            .module_registry
            .add_reads(reader, provider);
    }

    fn module_add_exports(&mut self, module_name: &str, pkg: &str, target: &str) {
        self.shared
            .classes
            .class_manager
            .write()
            .module_registry
            .add_exports(module_name, pkg, target);
    }

    fn module_add_exports_to_layer_module(
        &mut self,
        module_name: &str,
        pkg: &str,
        target: &str,
        target_loader_ns: u32,
    ) {
        self.shared
            .classes
            .class_manager
            .write()
            .module_registry
            .add_exports_to_layer_module(module_name, pkg, target, target_loader_ns);
    }

    fn module_add_opens_to_layer_module(
        &mut self,
        module_name: &str,
        pkg: &str,
        target: &str,
        target_loader_ns: u32,
    ) {
        self.shared
            .classes
            .class_manager
            .write()
            .module_registry
            .add_opens_to_layer_module(module_name, pkg, target, target_loader_ns);
    }

    fn module_add_opens(&mut self, module_name: &str, pkg: &str, target: &str) {
        self.shared
            .classes
            .class_manager
            .write()
            .module_registry
            .add_opens(module_name, pkg, target);
    }

    fn layer_module_define(
        &mut self,
        loader_ns: u32,
        name: &str,
        is_open: bool,
        packages: &[String],
    ) {
        self.shared
            .classes
            .class_manager
            .write()
            .module_registry
            .define_layer_module(loader_ns, name, is_open, packages);
    }

    fn layer_module_add_read(
        &mut self,
        loader_ns: u32,
        name: &str,
        target: cratonvm_native_api::LayerModuleEdgeTarget<'_>,
    ) {
        use crate::classloading::module::LayerExportTarget;
        use cratonvm_native_api::LayerModuleEdgeTarget as Edge;
        let target = match target {
            Edge::Everyone => LayerExportTarget::Everyone,
            Edge::AllUnnamed => LayerExportTarget::AllUnnamed,
            Edge::Named(m) => LayerExportTarget::Named(m.to_string()),
            Edge::Layer { loader_ns, name } => LayerExportTarget::Layer {
                loader_ns,
                name: name.to_string(),
            },
            Edge::Unnamed { loader_ns } => LayerExportTarget::Unnamed { loader_ns },
        };
        self.shared
            .classes
            .class_manager
            .write()
            .module_registry
            .add_layer_read(loader_ns, name, target);
    }

    fn layer_module_set_version(&mut self, loader_ns: u32, name: &str, version: &str) {
        self.shared
            .classes
            .class_manager
            .write()
            .module_registry
            .set_layer_module_version(loader_ns, name, version);
    }

    fn layer_module_add_export(
        &mut self,
        loader_ns: u32,
        name: &str,
        pkg: &str,
        target: cratonvm_native_api::LayerModuleEdgeTarget<'_>,
    ) {
        use crate::classloading::module::LayerExportTarget;
        use cratonvm_native_api::LayerModuleEdgeTarget as Edge;
        let target = match target {
            Edge::Everyone => LayerExportTarget::Everyone,
            Edge::AllUnnamed => LayerExportTarget::AllUnnamed,
            Edge::Named(m) => LayerExportTarget::Named(m.to_string()),
            Edge::Layer { loader_ns, name } => LayerExportTarget::Layer {
                loader_ns,
                name: name.to_string(),
            },
            Edge::Unnamed { loader_ns } => LayerExportTarget::Unnamed { loader_ns },
        };
        self.shared
            .classes
            .class_manager
            .write()
            .module_registry
            .add_layer_export(loader_ns, name, pkg, target);
    }

    fn module_packages(&self, module_name: &str) -> Vec<String> {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .packages_of(module_name)
    }

    fn module_uses(&self, module_name: &str) -> Vec<String> {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .get(module_name)
            .map(|d| d.uses.clone())
            .unwrap_or_default()
    }

    fn module_is_open(&self, module_name: &str) -> bool {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .get(module_name)
            .is_some_and(|d| d.is_open)
    }

    fn module_is_registered(&self, module_name: &str) -> bool {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .get(module_name)
            .is_some()
    }

    fn module_names(&self) -> Vec<String> {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .module_names()
    }

    fn module_is_class_path_only(&self, module_name: &str) -> bool {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .is_class_path_only(module_name)
    }

    fn module_outside_boot_configuration(&self, module_name: &str) -> bool {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .is_outside_boot_configuration(module_name)
    }

    fn boot_configuration_module_names(&self) -> Vec<String> {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .boot_configuration_modules()
    }

    fn module_patch_paths(&self, module_name: &str) -> Vec<String> {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .patch_paths(module_name)
            .to_vec()
    }

    fn module_exports(&self, module_name: &str) -> Vec<(String, Vec<String>)> {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .get(module_name)
            .map(|d| {
                d.exports
                    .iter()
                    .map(|e| (e.package_name.clone(), e.to_modules.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn module_opens(&self, module_name: &str) -> Vec<(String, Vec<String>)> {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .get(module_name)
            .map(|d| {
                d.opens
                    .iter()
                    .map(|o| (o.package_name.clone(), o.to_modules.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn module_dynamic_exports(&self, module_name: &str) -> Vec<(String, Option<String>)> {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .dynamic_exports_of(module_name)
    }

    fn module_dynamic_opens(&self, module_name: &str) -> Vec<(String, Option<String>)> {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .dynamic_opens_of(module_name)
    }

    fn module_dynamic_reads(&self, reader: &str) -> Vec<String> {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .dynamic_reads_of(reader)
    }

    fn module_requires(&self, module_name: &str) -> Vec<(String, bool, bool)> {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .get(module_name)
            .map(|d| {
                d.requires
                    .iter()
                    .map(|r| (r.module_name.clone(), r.is_transitive, r.is_static))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn module_provides(&self, module_name: &str) -> Vec<(String, Vec<String>)> {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .get(module_name)
            .map(|d| {
                d.provides
                    .iter()
                    .map(|p| (p.service.clone(), p.with.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn module_is_automatic(&self, module_name: &str) -> bool {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .get(module_name)
            .is_some_and(|d| d.automatic)
    }

    fn module_is_module_path_automatic(&self, module_name: &str) -> bool {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .is_automatic_module(module_name)
    }

    fn module_version(&self, module_name: &str) -> Option<String> {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .get(module_name)
            .and_then(|d| d.version.clone())
    }

    fn module_recorded_hash_names(&self, module_name: &str) -> Option<Vec<String>> {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .recorded_hash_names(module_name)
            .map(<[String]>::to_vec)
    }

    fn module_hashed_in_java_base(&self, module_name: &str) -> Option<bool> {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .recorded_hash_names("java.base")
            .map(|names| module_name == "java.base" || names.iter().any(|n| n == module_name))
    }

    fn module_main_class(&self, module_name: &str) -> Option<String> {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .get(module_name)
            .and_then(|d| d.main_class.clone())
    }

    fn module_requires_full(
        &self,
        module_name: &str,
    ) -> Vec<(String, bool, bool, bool, bool, Option<String>)> {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .get(module_name)
            .map(|d| {
                d.requires
                    .iter()
                    .map(|r| {
                        (
                            r.module_name.clone(),
                            r.is_transitive,
                            r.is_static,
                            r.is_synthetic,
                            r.is_mandated,
                            r.compiled_version.clone(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn module_exports_full(&self, module_name: &str) -> Vec<(String, Vec<String>, bool, bool)> {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .get(module_name)
            .map(|d| {
                d.exports
                    .iter()
                    .map(|e| {
                        (
                            e.package_name.clone(),
                            e.to_modules.clone(),
                            e.is_synthetic,
                            e.is_mandated,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn module_opens_full(&self, module_name: &str) -> Vec<(String, Vec<String>, bool, bool)> {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .get(module_name)
            .map(|d| {
                d.opens
                    .iter()
                    .map(|o| {
                        (
                            o.package_name.clone(),
                            o.to_modules.clone(),
                            o.is_synthetic,
                            o.is_mandated,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn all_module_names(&self) -> Vec<String> {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .module_names()
    }

    fn module_for_package(&self, pkg: &str) -> Option<String> {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .module_for_package(pkg)
            .map(|s| s.to_string())
    }

    fn named_module_for_package(&self, pkg: &str) -> Option<String> {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .named_module_for_package(pkg)
            .map(|s| s.to_string())
    }

    fn runtime_image_defines_class(&self, internal: &str) -> Option<bool> {
        self.shared
            .classes
            .class_manager
            .read()
            .runtime_image_defines_class(internal)
    }

    fn any_loaded_class_in_package(&self, package_slash: &str) -> bool {
        self.shared
            .classes
            .class_manager
            .read()
            .any_loaded_class_in_package(package_slash)
    }

    fn any_loaded_class_in_package_for_loader(&self, package_slash: &str, loader_id: u32) -> bool {
        self.shared
            .classes
            .class_manager
            .read()
            .any_loaded_class_in_package_for_loader(package_slash, loader_id)
    }

    fn loaded_package_representatives(&self, loader_id: u32) -> Vec<(String, ClassId)> {
        self.shared
            .classes
            .class_manager
            .read()
            .loaded_package_representatives(loader_id)
    }

    fn loaded_package_representative(&self, loader_id: u32, package_slash: &str) -> Option<ClassId> {
        self.shared
            .classes
            .class_manager
            .read()
            .loaded_package_representative(loader_id, package_slash)
    }

    fn set_class_hidden(&mut self, class_id: ClassId) {
        // The flag and the lock-free hidden-class bitmap together (JIT round
        // 21 wave 14, lane hidden-static; `ClassManager::mark_class_hidden`).
        self.shared
            .classes
            .class_manager_write()
            .mark_class_hidden(class_id);
    }

    fn is_class_hidden(&self, class_id: ClassId) -> bool {
        let cm = self.shared.classes.class_manager.read();
        cm.get_class(class_id)
            .map(|c| c.is_hidden())
            .unwrap_or(false)
    }

    fn copy_nest_info(&mut self, source_class: ClassId, target_class: ClassId) {
        // NEW-8: hidden classes created with the NESTMATE ClassOption
        // inherit the lookup class's nest host and nest members. We copy
        // the relevant fields onto the target so that member access
        // checks see the hidden class as a legitimate nestmate.
        let mut cm = self.shared.classes.class_manager_write();
        // Grab the nest info from the source class first (release the
        // immutable borrow before we take a mutable one).
        let (nest_host, nest_members) = match cm.get_class(source_class) {
            Some(src) => {
                // If source is itself a nest member, it has a nest_host
                // pointing at the host. If it IS the host (or has no
                // nest info), use source's own name so the hidden class
                // becomes a member of source's nest.
                let host = src
                    .nest_host
                    .clone()
                    .unwrap_or_else(|| src.name.to_string());
                (host, src.nest_members.clone())
            }
            None => return,
        };
        if let Some(target) = cm.get_class_mut(target_class) {
            target.nest_host = Some(nest_host);
            target.nest_members = nest_members;
        }
    }

    fn initialize_class(&mut self, class_id: ClassId) -> Result<(), MethodCallFailed> {
        // NEW-8: force the class's <clinit> to run now. The interpreter's
        // `ensure_class_initialized_shared` handles thread-safe init and
        // skips classes that are already initialized.
        //
        // HIB-CV-26 fix (2026-07-16): pass the result straight through
        // instead of collapsing it into a `String`. `<clinit>` failures
        // already come back from `ensure_class_initialized_shared` with
        // the correct two-layer identity: a Java exception raised by a
        // static initializer is already wrapped as a catchable
        // `ExceptionInInitializerError`/`NoClassDefFoundError`
        // (`MethodCallFailed::ExceptionThrown`) per JVMS §5.5, and only a
        // genuine VM bug is `MethodCallFailed::InternalError`. Flattening
        // both into a string here (as the old code did) forced every
        // caller of `initialize_class` — including `Class.forName` — to
        // re-wrap ordinary `<clinit>` exceptions as an unrecoverable
        // `VmError::Internal`, aborting the whole VM instead of letting
        // Java code catch them.
        crate::vm::vm_util::ensure_class_initialized_shared(&self.shared, self.thread, class_id)
    }

    fn initialize_class_served(
        &mut self,
        class_id: ClassId,
        chain: cratonvm_native_api::ServedRaiseChain,
    ) -> Result<(), MethodCallFailed> {
        if crate::vm::vm_util::is_class_initialized_via_manager(&self.shared, class_id) {
            return Ok(());
        }
        initialize_class_served_cold(self.shared, self.thread, class_id, chain)
    }

    fn ensure_class_initialized_served(
        &mut self,
        name: &str,
        chain: &dyn Fn() -> cratonvm_native_api::ServedRaiseChain,
    ) -> Result<ClassId, MethodCallFailed> {
        // `ensure_class_initialized`'s resolution; its initialization served
        // (interpreter round i2 wave 59, lane L4).
        let class_id = self.resolve_class_loader_faithful(name)?;
        if !crate::vm::vm_util::is_class_initialized_via_manager(&self.shared, class_id) {
            initialize_class_served_cold(self.shared, self.thread, class_id, chain())?;
        }
        Ok(class_id)
    }

    fn in_clinit(&self) -> bool {
        crate::vm::vm_util::in_clinit_shared()
    }

    fn service_providers_from_modules(&self, service_class: &str) -> Vec<String> {
        self.shared
            .classes
            .class_manager
            .read()
            .module_registry
            .service_providers(service_class)
    }

    fn check_deep_reflection_access(
        &self,
        accessor_class_id: ClassId,
        target_class_id: ClassId,
    ) -> Result<(), String> {
        let cm = self.shared.classes.class_manager.read();
        // No modules registered в†’ classpath-only mode, allow.
        if cm.module_registry.is_empty() {
            return Ok(());
        }
        let accessor = match cm.get_class(accessor_class_id) {
            Some(c) => c,
            None => return Ok(()),
        };
        let target = match cm.get_class(target_class_id) {
            Some(c) => c,
            None => return Ok(()),
        };
        // By the modules' identities: a layer module's class as the target
        // (wave 44) or the accessor (wave 45), else the name map -- one
        // answer shared with `reflective_export_to_accessor` since wave 46
        // (lane L5).
        crate::classloading::access_control::reflective_module_access(
            accessor,
            target,
            &cm.module_registry,
            crate::classloading::access_control::ReflectiveModuleQuestion::Open,
        )
    }

    fn reflective_export_to_accessor(
        &self,
        accessor_class_id: ClassId,
        target_class_id: ClassId,
    ) -> bool {
        let cm = self.shared.classes.class_manager.read();
        // No modules registered -> classpath-only mode. `check_deep_reflection_
        // access` short-circuits to allow there, so this widening query is
        // never reached; answer consistently anyway.
        if cm.module_registry.is_empty() {
            return true;
        }
        let (Some(accessor), Some(target)) = (
            cm.get_class(accessor_class_id),
            cm.get_class(target_class_id),
        ) else {
            return false;
        };
        // By the modules' identities (see `check_deep_reflection_access`).
        crate::classloading::access_control::reflective_module_access(
            accessor,
            target,
            &cm.module_registry,
            crate::classloading::access_control::ReflectiveModuleQuestion::Export,
        )
        .is_ok()
    }

    /// Interpreter round i3 wave 74, lane L3: [`Self::reflective_export_to_accessor`]
    /// is `true` for every accessor (an empty registry, or an unnamed-module
    /// target in no layer module). An unknown class answers `false`, so the
    /// caller asks the full question.
    fn reflective_export_to_every_accessor(&self, target_class_id: ClassId) -> bool {
        let cm = self.shared.classes.class_manager.read();
        if cm.module_registry.is_empty() {
            return true;
        }
        let Some(target) = cm.get_class(target_class_id) else {
            return false;
        };
        crate::classloading::access_control::reflective_export_reaches_every_accessor(
            target,
            &cm.module_registry,
        )
    }

    fn find_resource(&self, name: &str) -> Option<Vec<u8>> {
        self.shared.classes.class_manager.read().find_resource(name)
    }

    fn class_bytes(&self, class_id: ClassId) -> Option<Vec<u8>> {
        let cm = self.shared.classes.class_manager.read();
        // The cache, else a fingerprint-checked re-read (zr z11/md).
        cm.defined_class_bytes(class_id).map(|bytes| bytes.to_vec())
    }

    fn class_bytes_match_base(&self, class_id: ClassId, bytes: &[u8]) -> Option<bool> {
        let cm = self.shared.classes.class_manager.read();
        cm.class_bytes_match_base(class_id, bytes)
    }

    /// Interpreter round i1 wave 29, lane L3: see the trait and
    /// `runtime::instrument_reconstitute`.
    fn normalize_retransformation_bytes(&self, class_id: ClassId, bytes: Vec<u8>) -> Vec<u8> {
        let cm = self.shared.classes.class_manager.read();
        crate::runtime::instrument_reconstitute::normalize_source_flags(&cm, class_id, bytes)
    }

    fn reconstituted_class_bytes(&self, class_id: ClassId) -> Option<Vec<u8>> {
        let cm = self.shared.classes.class_manager.read();
        crate::runtime::instrument_reconstitute::reconstitute_logged(&cm, class_id)
    }

    fn find_all_resource_urls(&self, name: &str) -> Vec<String> {
        self.shared
            .classes
            .class_manager
            .read()
            .find_all_resource_urls(name)
    }

    fn first_runtime_image_resource_url(&self, name: &str) -> Option<String> {
        self.shared
            .classes
            .class_manager
            .read()
            .first_runtime_image_resource_url(name)
    }

    fn find_resource_urls_in_segment(&self, name: &str, segment: u8) -> Vec<String> {
        self.shared
            .classes
            .class_manager
            .read()
            .find_resource_urls_in_segment(name, segment as usize)
    }

    fn module_path_resource_url(&self, module: &str, name: &str) -> Option<Option<String>> {
        self.shared
            .classes
            .class_manager
            .read()
            .module_path_resource_url(module, name)
    }

    fn module_path_resource_bytes(&self, module: &str, name: &str) -> Option<Option<Vec<u8>>> {
        self.shared
            .classes
            .class_manager
            .read()
            .module_path_resource_bytes(module, name)
    }

    fn next_resource_url(
        &self,
        name: &str,
        segment: u32,
        index: u32,
    ) -> Option<(String, u32, u32)> {
        let (url, seg, idx) = self
            .shared
            .classes
            .class_manager
            .read()
            .next_resource_url_from(name, segment as usize, index as usize)?;
        Some((url, seg as u32, idx as u32))
    }

    fn resource_name_supports_incremental_scan(&self, name: &str) -> bool {
        cratonvm_classloading::ClassManager::resource_name_supports_incremental_scan(name)
    }

    fn find_all_resource_bytes(&self, name: &str) -> Vec<Vec<u8>> {
        self.shared
            .classes
            .class_manager
            .read()
            .find_all_resource_bytes(name)
    }

    fn find_class_source_path(&self, class_name: &str) -> Option<String> {
        self.shared
            .classes
            .class_manager
            .read()
            .find_class_source_path(class_name)
    }

    fn class_path_fast_define_ok(&self, internal: &str) -> bool {
        self.shared
            .classes
            .class_manager
            .read()
            .application_fast_define_ok(internal)
    }

    fn class_path_fast_absent_ok(&self, internal: &str) -> bool {
        self.shared
            .classes
            .class_manager
            .read()
            .application_fast_absent_ok(internal)
    }

    fn class_path_take_provider_circularity_error(&self) -> bool {
        self.shared
            .classes
            .class_manager
            .read()
            .application_take_provider_circularity_error()
    }

    fn class_path_provider_lookup_sensitive(&self) -> bool {
        self.shared
            .classes
            .class_manager
            .read()
            .application_provider_lookup_class_path()
    }

    fn class_path_outer_archive(
        &self,
        path: &std::path::Path,
    ) -> Option<Box<dyn std::any::Any + Send + Sync>> {
        let archive = self
            .shared
            .classes
            .class_manager
            .read()
            .application_outer_archive(path)?;
        Some(Box::new(archive))
    }

    fn class_path_jar_entry_bytes(
        &self,
        path: &std::path::Path,
        name: &str,
        crc32: u32,
        size: u64,
    ) -> cratonvm_native_api::ClassPathJarBytes {
        self.shared
            .classes
            .class_manager
            .read()
            .application_jar_entry_bytes(path, name, crc32, size)
    }

    fn class_code_base(&self, class_id: ClassId) -> Option<String> {
        let cm = self.shared.classes.class_manager.read();
        let cls = cm.class_store.get(class_id)?;
        cls.code_source.as_ref()?.url.clone()
    }

    fn class_code_source_cert_digests(&self, class_id: ClassId) -> Vec<String> {
        let cm = self.shared.classes.class_manager.read();
        match cm.class_store.get(class_id) {
            Some(cls) => cls
                .code_source
                .as_ref()
                .map(|cs| cs.certificate_sha256.clone())
                .unwrap_or_default(),
            None => Vec::new(),
        }
    }

    fn class_code_source_certs(&self, class_id: ClassId) -> Vec<Vec<u8>> {
        let cm = self.shared.classes.class_manager.read();
        match cm.class_store.get(class_id) {
            Some(cls) => cls
                .code_source
                .as_ref()
                .map(|cs| cs.certificates.clone())
                .unwrap_or_default(),
            None => Vec::new(),
        }
    }

    fn class_id_from_mirror(&self, mirror: ObjectRef) -> Option<ClassId> {
        super::class_id_from_mirror(self.shared, mirror)
    }

    fn cached_class_mirror(&self, class_id: ClassId) -> Option<ObjectRef> {
        let ptr = self.shared.classes.getclass_mirror_words.get(class_id);
        if ptr != 0 {
            return Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) });
        }
        let _ = self.shared.classes.getclass_mirror_words.base_addr();
        let guard = self.shared.classes.class_mirrors.read();
        let mirror = guard.get(&class_id).copied()?;
        self.shared
            .classes
            .getclass_mirror_words
            .publish(class_id, mirror);
        Some(mirror)
    }

    fn note_class_mirror_id(&self, mirror: ObjectRef, class_id: ClassId, kind: &dyn Fn() -> u8) {
        // JIT round 20 wave 19, lane zAssign: the compiled
        // `Class.isAssignableFrom` / `isInstance` arms' mirror hints.
        crate::jit::helpers::class_mirror_id_hint_fill(self.shared, mirror, class_id, kind);
    }

    fn list_application_class_names(&self) -> Vec<String> {
        self.shared
            .classes
            .class_manager
            .read()
            .list_application_class_names()
    }

    fn register_dynamic_classpath(&mut self, paths: &[String]) {
        self.shared
            .classes
            .class_manager
            .write()
            .extend_application_classpath(paths);
    }

    fn unregister_dynamic_classpath(&mut self, paths: &[String]) -> usize {
        self.shared
            .classes
            .class_manager
            .write()
            .retract_application_classpath(paths)
    }

    fn register_bootstrap_classpath(&mut self, paths: &[String]) {
        self.shared
            .classes
            .class_manager
            .write()
            .extend_bootstrap_classpath(paths);
    }

    fn reset_thread_jmx_contention_stats(&mut self) {
        self.shared
            .threads
            .thread_registry
            .reset_jmx_contention_stats();
    }

    fn define_class_from_bytes(&mut self, name: &str, bytes: &[u8]) -> Option<ClassId> {
        use cratonvm_types::ClassLoaderId;
        // gci i12/ev: the define touches `shared` only; its guard drop posts
        // a C `ClassLoad` on this thread's own handle (`define_on_own_handle`).
        let shared = self.shared;
        define_on_own_handle(shared, self.thread, || {
            let mut cm = shared.classes.class_manager_write();
            let next_id_before = cm.class_store.next_id();
            match cm.define_class(name, bytes, ClassLoaderId::Application) {
                Ok(cid) => {
                    // Release the ClassManager write lock before invalidating:
                    // nothing below needs it.
                    drop(cm);
                    // j27 wave 8 (lane W8CHA): the finalizable-define listener
                    // too, as `define_class_full` and JNI `DefineClass` run it.
                    shared.note_finalizable_define(cid);
                    // j27 wave 2 (lane REDEFINE): the class-hierarchy listener.
                    shared.note_java_loader_subtype_define(cid);
                    // A fresh id cannot have replaced anything a body depends on
                    // (see `define_withdraws_by_name`, round 12 wave 5).
                    if !crate::vm::realms::jit_realm::define_withdraws_by_name(
                        cid,
                        next_id_before,
                        false,
                    ) {
                        return Some(cid);
                    }
                    // Invalidate JIT-compiled methods that inlined from this class (Session 31)
                    let evicted = shared.jit.jit_cache.write().invalidate_for_class(name);
                    if evicted > 0 {
                        tracing::debug!(
                            "JIT: invalidated {evicted} method(s) due to class reload: {name}"
                        );
                    }
                    // A define over an already-loaded name replaces that class's
                    // bytecode, and with it every tiering verdict about the old one.
                    // r10 lane `tier2`: the profile store is keyed by (class_id, method,
                    // descriptor) and `class_id` is STABLE across a redefine, so without this the
                    // new bytecode's execution blends into branch ratios and receiver tables
                    // gathered from the OLD bytecode. The unload path in `memory/gc.rs` has always
                    // invalidated both; redefine invalidated only the tiering verdicts.
                    shared.jit.profile_store.invalidate_class(cid.as_u32());
                    shared.jit.tiered_manager.on_class_redefined(cid, name);
                    Some(cid)
                }
                Err(e) => {
                    tracing::debug!("defineClass failed for {name}: {e:?}");
                    None
                }
            }
        })
    }

    fn define_hidden_class_from_bytes(
        &mut self,
        stored_name: &str,
        bytes: &[u8],
    ) -> Result<ClassId, String> {
        use cratonvm_classloading::DefineClassOptions;
        use cratonvm_types::ClassLoaderId;
        // gci i12/ev: as `define_class_from_bytes` (`define_on_own_handle`).
        let shared = self.shared;
        let res = define_on_own_handle(shared, self.thread, || {
            let mut cm = shared.classes.class_manager_write();
            let next_id_before = cm.class_store.next_id();
            let options = DefineClassOptions {
                override_name: Some(stored_name.to_string()),
                hidden: true,
                ..Default::default()
            };
            match cm.define_class_with_options(stored_name, bytes, ClassLoaderId::Application, options)
            {
                Ok(cid) => {
                    // A hidden define always mints a fresh id under a unique
                    // stored name (see `define_withdraws_by_name`).
                    if !crate::vm::realms::jit_realm::define_withdraws_by_name(
                        cid,
                        next_id_before,
                        false,
                    ) {
                        return Ok(cid);
                    }
                    // Hidden classes cannot be inlined from (they may be
                    // unloaded independently), but we still invalidate the
                    // JIT cache defensively.
                    let evicted = shared
                        .jit
                        .jit_cache
                        .write()
                        .invalidate_for_class(stored_name);
                    if evicted > 0 {
                        tracing::debug!(
                            "JIT: invalidated {evicted} method(s) due to hidden class define: {stored_name}"
                        );
                    }
                    Ok(cid)
                }
                Err(e) => Err(format!("{e:?}")),
            }
        });
        if let Ok(cid) = res {
            crate::runtime::frozen_pages::note_class_defined(shared, cid);
        }
        res
    }

    fn define_class_with_loader(
        &mut self,
        name: &str,
        bytes: &[u8],
        loader_id: u32,
    ) -> Option<ClassId> {
        use cratonvm_types::ClassLoaderId;
        // Inverse of `loader_id_of_class`'s encoding, under the native-API
        // boundary's default-sentinel convention (`0` == "unspecified, use
        // the application namespace"). See
        // `ClassLoaderId::from_native_id_or_default`.
        let cl_id = ClassLoaderId::from_native_id_or_default(loader_id);
        // gci i12/ev: as `define_class_from_bytes` (`define_on_own_handle`).
        let shared = self.shared;
        define_on_own_handle(shared, self.thread, || {
            let mut cm = shared.classes.class_manager_write();
            let next_id_before = cm.class_store.next_id();
            match cm.define_class(name, bytes, cl_id) {
                Ok(cid) => {
                    drop(cm);
                    // j27 wave 8 (lane W8CHA): see `define_class_from_bytes`.
                    shared.note_finalizable_define(cid);
                    // j27 wave 2 (lane REDEFINE): the class-hierarchy listener.
                    shared.note_java_loader_subtype_define(cid);
                    // Round 12 wave 5: see `define_withdraws_by_name`.
                    if !crate::vm::realms::jit_realm::define_withdraws_by_name(
                        cid,
                        next_id_before,
                        false,
                    ) {
                        return Some(cid);
                    }
                    let evicted = shared.jit.jit_cache.write().invalidate_for_class(name);
                    if evicted > 0 {
                        tracing::debug!(
                            "JIT: invalidated {evicted} method(s) due to class reload: {name}"
                        );
                    }
                    // A define over an already-loaded name replaces that class's
                    // bytecode, and with it every tiering verdict about the old one.
                    // r10 lane `tier2`: the profile store is keyed by (class_id, method,
                    // descriptor) and `class_id` is STABLE across a redefine, so without this the
                    // new bytecode's execution blends into branch ratios and receiver tables
                    // gathered from the OLD bytecode. The unload path in `memory/gc.rs` has always
                    // invalidated both; redefine invalidated only the tiering verdicts.
                    shared.jit.profile_store.invalidate_class(cid.as_u32());
                    shared.jit.tiered_manager.on_class_redefined(cid, name);
                    Some(cid)
                }
                Err(e) => {
                    tracing::debug!("defineClass (loader {loader_id}) failed for {name}: {e:?}");
                    None
                }
            }
        })
    }

    fn class_id_by_name_and_loader(&self, name: &str, loader_id: u32) -> Option<ClassId> {
        use cratonvm_types::ClassLoaderId;
        // Same decode as `define_class_full` — see
        // `ClassLoaderId::from_native_id_or_default`.
        let cl_id = ClassLoaderId::from_native_id_or_default(loader_id);
        let cm = self.shared.classes.class_manager.read();
        cm.find_class_by_name_in_loader(name, cl_id)
    }

    fn class_id_defined_by_loader_exact(&self, name: &str, loader_id: u32) -> Option<ClassId> {
        use cratonvm_types::ClassLoaderId;
        // Same decode as `define_class_full` — see
        // `ClassLoaderId::from_native_id_or_default`.
        let cl_id = ClassLoaderId::from_native_id_or_default(loader_id);
        let cm = self.shared.classes.class_manager.read();
        cm.class_defined_by_loader_exact(name, cl_id)
    }

    fn bootstrap_class_id_exact(&self, name: &str) -> Option<ClassId> {
        let cm = self.shared.classes.class_manager.read();
        cm.class_defined_by_loader_exact(name, cratonvm_types::ClassLoaderId::Bootstrap)
    }

    /// zt t16/df6: `ClassManager::bind_unbound_define` under the write guard.
    /// zt t30/dc: `CRATONVM_DBG_DEFINE_CENSUS` times the lock, the body and
    /// the guard's release as `bind-cm-acquire` / `-body` / `-release`.
    fn bind_unbound_define(&mut self, class_id: ClassId) -> Option<bool> {
        use cratonvm_classloading::define_census::{phase_end, phase_start, DefinePhase};
        let acquire_clock = phase_start();
        let mut cm = self.shared.classes.class_manager_write();
        phase_end(DefinePhase::BindCmAcquire, acquire_clock);
        let body_clock = phase_start();
        let bound = cm.bind_unbound_define(class_id);
        phase_end(DefinePhase::BindCmBody, body_clock);
        let release_clock = phase_start();
        drop(cm);
        phase_end(DefinePhase::BindCmRelease, release_clock);
        bound
    }

    /// zt t16/df6: `ClassManager::withdraw_unbound_define` under the write
    /// guard.
    fn withdraw_unbound_define(&mut self, class_id: ClassId) -> bool {
        self.shared
            .classes
            .class_manager_write()
            .withdraw_unbound_define(class_id)
    }

    /// zt t16/df6: `ClassManager::unbound_define_of` under the read guard
    /// (same loader decode as `define_class_full`).
    fn unbound_define_of(&self, name: &str, loader_id: u32) -> Option<ClassId> {
        use cratonvm_types::ClassLoaderId;
        let cl_id = ClassLoaderId::from_native_id_or_default(loader_id);
        self.shared
            .classes
            .class_manager
            .read()
            .unbound_define_of(name, cl_id)
    }

    fn array_class_id_for_loader(&mut self, name: &str, loader_id: u32) -> Option<ClassId> {
        use cratonvm_types::ClassLoaderId;
        let cl_id = ClassLoaderId::from_native_id_or_default(loader_id);
        // Read-lock probe first (interpreter round i1 wave 48, lane L5): an
        // existing array class is filed under its component's defining
        // loader, so under a user loader's own key it is exactly the array
        // over the class that loader defined, and under the bootstrap key the
        // answer for a built-in requester (`load_array_class_for_loader`'s
        // own pre-probe). `Object.getClass()` of such an array asks this per
        // call in both modes now (`lang_class::strict_array_class_id`), and
        // took the class-manager WRITE lock every time.
        let probe_key = match cl_id {
            ClassLoaderId::UserDefined(_) => cl_id,
            _ => ClassLoaderId::Bootstrap,
        };
        if let Some(id) = self
            .shared
            .classes
            .class_manager
            .read()
            .loaded_class_under_exact_key(name, probe_key)
        {
            return Some(id);
        }
        self.shared
            .classes
            .class_manager_write_settling()
            .load_array_class_for_loader(name, cl_id)
            .ok()
    }

    fn register_virtual_array_component(&mut self, internal_name: &str, class_id: ClassId) {
        self.shared
            .classes
            .class_manager
            .write()
            .register_virtual_component(internal_name, class_id);
    }

    fn define_class_full(
        &mut self,
        name: &str,
        bytes: &[u8],
        loader_id: u32,
        opts: cratonvm_native_api::DefineClassFull,
    ) -> Result<ClassId, String> {
        // WP2.3: single backend for all four entry points
        // (Unsafe.defineClass, jdk.internal.misc.Unsafe.defineClass,
        // MethodHandles.Lookup.defineClass, ClassLoader.defineClass1/2).
        use cratonvm_classloading::{CodeSource, DefineClassOptions};
        use cratonvm_types::ClassLoaderId;
        // `CRATONVM_DEFINE_ONE_PARSE` (interpreter round i3 wave 72, lane L5):
        // the caller's parse of these bytes, moved out of `opts` (a clone of
        // the `Arc` would keep `define_preparsed_class_file` from taking it).
        let mut opts = opts;
        // `CRATONVM_DBG_DEFINE_CENSUS` phase clocks (interpreter round i4
        // wave 88, lane L5).
        use cratonvm_classloading::define_census::{phase_end, phase_start, DefinePhase};
        let pre_clock = phase_start();
        let preparsed = opts.preparsed.take();
        // Must be the inverse of `loader_id_of_class`'s encoding
        // (Bootstrap=0, Extension=1, Application=2, UserDefined(id)=id),
        // modulo the boundary's documented `0` == "unspecified, use the
        // application namespace" sentinel. Both directions now live on
        // `ClassLoaderId` (`to_native_id` / `from_native_id_or_default`)
        // precisely so they cannot drift again: this site used to
        // special-case only `0` and fall through everything else --
        // including `2` -- to `UserDefined(loader_id)`. A caller that
        // round-trips a REAL loader id via `loader_id_of_class` (e.g. the
        // CGLIB `@Configuration` enhancer fetching its superclass's own
        // loader so the generated subclass is defined by the SAME loader)
        // got `UserDefined(2)` back for the Application loader instead of
        // `Application` -- a different `ClassLoaderId` that
        // `same_runtime_package`'s `loader_id` equality check correctly
        // treats as a DIFFERENT runtime package from the superclass's
        // `Application` tag. That silently defeated package-private
        // method-override detection for the whole generated class: every
        // package-private `@Bean` override was treated as an unrelated,
        // independent method in a fresh vtable slot rather than a true
        // override of the inherited slot, so virtual dispatch to it from
        // ordinary (non-invokespecial) call sites kept resolving to the
        // superclass's original method body forever, breaking `@Bean`
        // inter-method singleton-sharing (calling one `@Bean` method from
        // another on `this` re-ran the real factory body instead of
        // returning the container's cached instance).
        // `bootstrap_namespace`: a bootstrap-lookup `Lookup.defineClass`
        // (`--real-jdk` `defineClass0`), which `loader_id`'s `0` sentinel
        // cannot name (interpreter round i1 wave 37, lane L5).
        let cl_id = if opts.bootstrap_namespace {
            ClassLoaderId::Bootstrap
        } else {
            ClassLoaderId::from_native_id_or_default(loader_id)
        };
        // JIT round 20 rev13c: the defining loader's HotSpot label, for the
        // define-time supertype access refusal's message (`ClassManager`
        // `supertype_access_violation`). One table probe once labelled.
        if matches!(cl_id, ClassLoaderId::UserDefined(_)) {
            if let Some(loader) = cratonvm_native_builtins::classloader::user_loader_object(
                &*self,
                cl_id.to_native_id(),
            ) {
                crate::runtime::resolve::loader_constraints::note_loader_label(
                    self.shared,
                    cl_id,
                    loader,
                );
            }
        }
        // JVMS §5.3.5, `--real-jdk`: a user-defined loader that INITIATED this
        // name (the VM asked it and it answered another loader's class) may not
        // define it itself; HotSpot's `check_constraints` refuses it with the
        // duplicate-definition `LinkageError` (interpreter round i1 wave 38,
        // lane L5; `runtime::resolve::initiating_records`, probe
        // `L5W37InitiatingLoaderRecord`). The text travels as a
        // `LoaderConstraintViolation`, the variant the define natives turn into
        // `java.lang.LinkageError` with the message verbatim. One read guard
        // for a named user-loader define; nothing in the other mode.
        if !opts.hidden && !opts.allow_redefine {
            let effective_name = opts.override_name.as_deref().unwrap_or(name);
            if let Some(initiated) = crate::runtime::resolve::initiating_records::define_refusal(
                self.shared,
                cl_id,
                effective_name,
            ) {
                let dotted = effective_name.replace('/', ".");
                // zt df4 §9c: a built-in loader's define names its loader by
                // its own id (the native id the define carries is `0` for
                // the application loader).
                let message_ns = match cl_id {
                    ClassLoaderId::Application | ClassLoaderId::Extension => cl_id.to_native_id(),
                    _ => loader_id,
                };
                let message =
                    cratonvm_native_builtins::classloader_real::initiated_duplicate_definition_message(
                        self,
                        message_ns,
                        effective_name,
                        initiated,
                    )
                    .unwrap_or_else(|| {
                        format!("loader {cl_id} attempted duplicate class definition for {dotted}.")
                    });
                return Err(format!(
                    "{:?}",
                    VmError::Linkage(LinkageError::LoaderConstraintViolation { message })
                ));
            }
        }
        let code_source =
            if opts.code_source_url.is_none() && opts.code_source_certificates.is_empty() {
                None
            } else {
                Some(CodeSource::new(
                    opts.code_source_url.clone(),
                    opts.code_source_certificates.clone(),
                ))
            };
        let define_opts = DefineClassOptions {
            override_name: opts.override_name.clone(),
            hidden: opts.hidden,
            skip_verification: opts.skip_verification,
            code_source,
            allow_redefine: opts.allow_redefine,
            nest_host_class_name: opts.nest_host_class_name.clone(),
            lookup_class_name: opts.lookup_class_name.clone(),
            privileged_define: opts.privileged_define,
            force_loader_faithful_linking: opts.force_loader_faithful_linking,
            superclass_id_override: opts.superclass_id_override,
            interface_id_overrides: opts.interface_id_overrides.clone(),
            user_loader_define: opts.user_loader_define,
            // zt t16/df6: never with `initialize`: the initializer below runs
            // before the caller's upcall and may resolve the class by name.
            bind_name_after_add_class: opts.bind_name_after_add_class && !opts.initialize,
            ..Default::default()
        };
        let defining_loader = opts.defining_loader;

        // `java.lang.instrument` transform-on-load, native half.
        //
        // This is the single backend for every "define a class from raw bytes"
        // entry point — `ClassLoader.defineClass1/2/0`, `Unsafe.defineClass`,
        // `MethodHandles.Lookup.defineClass`/`defineHiddenClass` — which is
        // where a user-defined loader's classes come from. The load-time hook in
        // `runtime::instrument` only covers the built-in delegation chain, so
        // without this a `-javaagent:` would be offered the JDK and the
        // classpath but not a single webapp / Spring / OSGi class.
        //
        // The chain runs here, before the define, with no class-manager lock
        // held. Two exclusions, both deliberate: HIDDEN classes have no binding
        // name, `Instrumentation.isModifiableClass` reports them unmodifiable,
        // and the JDK does not offer them to transformers either; and a
        // REDEFINE already ran the chain in `native_redefine_classes0`, so
        // running it again here would weave the same class twice.
        let transformed;
        // The retransformation base when a retransform-capable transformer
        // changed the class file (`run_load_time_transform_chain`): recorded as
        // the class's cached bytes after the define below.
        let mut retransform_base: Option<Vec<u8>> = None;
        // Whether a transformer changed the class file: the class path cannot
        // give such a base back, so it is pinned out of the class-bytes FIFO's
        // reach below (interpreter round i1 wave 28, lane L3).
        let mut load_transformed = false;
        let bytes = if !opts.hidden
            && !opts.allow_redefine
            && !name.is_empty()
            && crate::runtime::instrument::load_hook_armed(self.shared)
        {
            let (chain_bytes, base) = crate::runtime::instrument::run_load_time_transform_chain(
                self,
                name,
                cl_id,
                opts.protection_domain,
                bytes,
            );
            load_transformed = base.is_some() || chain_bytes[..] != bytes[..];
            transformed = chain_bytes;
            retransform_base = base;
            &transformed[..]
        } else {
            bytes
        };
        // JIT round 20 lane superAcc: explicit supertype identities (the
        // defining loader's `loadClass` answers, or a lookup's) name the
        // supertypes of the bytes the caller passed. A transformer that changed
        // a supertype NAME makes them stale: the define resolves the new
        // names itself, as before. Asked only for a transformed define that
        // carries identities (a re-parse of the transformed bytes).
        let define_opts = if load_transformed
            && (define_opts.superclass_id_override.is_some()
                || define_opts.interface_id_overrides.is_some())
        {
            let mut define_opts = define_opts;
            let stale = match cratonvm_reader::read_class(bytes) {
                Ok(class_file) => {
                    let cm = self.shared.classes.class_manager.read();
                    let names = |id: ClassId, expected: &str| {
                        cm.get_class(id).is_some_and(|c| &*c.name == expected)
                    };
                    let super_stale = define_opts.superclass_id_override.is_some_and(|id| {
                        !class_file
                            .super_class
                            .as_deref()
                            .is_some_and(|expected| names(id, expected))
                    });
                    let ifaces_stale =
                        define_opts
                            .interface_id_overrides
                            .as_ref()
                            .is_some_and(|ids| {
                                ids.len() != class_file.interfaces.len()
                                    || ids
                                        .iter()
                                        .zip(class_file.interfaces.iter())
                                        .any(|(&id, expected)| !names(id, &**expected))
                            });
                    super_stale || ifaces_stale
                }
                // Malformed transformed bytes: the define reports them.
                Err(_) => false,
            };
            if stale {
                define_opts.superclass_id_override = None;
                define_opts.interface_id_overrides = None;
            }
            define_opts
        } else {
            define_opts
        };

        // `-XX:MaxMetaspaceSize` (gc-common w6-f,
        // `docs/feature-designs/real-jdk-mode.md`),
        // in HotSpot's order: a define that would not fit runs one full
        // collection first — class unloading runs inside it and gives a dead
        // loader's charge back (`ClassManager::unload_user_classes`) — and only
        // if it still does not fit is it refused with
        // `OutOfMemoryError: Metaspace`. Every "define from raw bytes" entry
        // point comes through here, which is where a leaking loader's classes
        // (webapp redeploys, CGLIB / ByteBuddy / proxy generators) come from;
        // the built-in loaders' class-path loads are charged but never
        // refused. No limit (the default) → one `Option` check under a
        // class-manager READ lock (taken with no other lock held; the define
        // below takes the write lock anyway), so `--compatible` without the
        // flag is unchanged.
        // The refusal is the `Debug` text of a `VmError::Runtime` OOME, the
        // shape the define natives' error typing recovers.
        //
        // gci i33/ms: the predicate is HotSpot's -- the model's COMMITTED
        // metaspace of every live class, boot classes included, plus this
        // class, against the limit (`vm_init::metaspace_limit_would_exceed`,
        // summed incrementally); `CRATONVM_METASPACE_LIMIT_HOTSPOT_CHECK=0`
        // keeps the non-boot class-file bytes, which refused later than
        // HotSpot. Still one `Option` check without a limit.
        let charge = bytes.len() as u64;
        let over_limit = |shared: &SharedVm| {
            crate::vm::vm_init::metaspace_limit_would_exceed(
                &shared.classes.class_manager.read(),
                cl_id,
                charge,
            )
        };
        // Two collections, like HotSpot's "last ditch" metaspace GC: on ZGC the
        // first one's class unloading gives the charge back only after this
        // define has re-checked (measured, w6 orchestrator: every refusal was
        // followed by 7,506 successful defines with no further collection), so
        // one collection refused a define that fits. Only on the over-limit
        // path; no cost otherwise. Not `force_gc_from_native`: that is
        // `System.gc()`, whose finalizer / cleaner / notification drains would
        // run application code (or wait up to the finalizer timeout) on this
        // thread while it holds the loader's class-loading lock (gc-common
        // w7-g).
        if over_limit(self.shared) {
            crate::runtime::interpreter::force_gc_for_class_metadata(self.shared, self.thread);
            if over_limit(self.shared) {
                crate::runtime::interpreter::force_gc_for_class_metadata(self.shared, self.thread);
            }
            if over_limit(self.shared) {
                let oome = VmError::Runtime(RuntimeError::OutOfMemoryError {
                    message: "Metaspace".to_string(),
                });
                tracing::debug!("defineClass (loader {loader_id}) refused for {name}: {oome}");
                return Err(format!("{oome:?}"));
            }
        }

        // The caller's parse stands for the bytes only while no transformer
        // changed them (and only for these very bytes).
        let preparsed = if load_transformed {
            None
        } else {
            preparsed
                .and_then(define_preparsed_class_file)
                .filter(|(source, _)| source[..] == bytes[..])
        };
        phase_end(DefinePhase::VmPreDefine, pre_clock);
        let define_clock = phase_start();
        // gci i12/ev (`gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`
        // item 1c): a C agent's `ClassLoad` of this class is posted when the
        // write guard below drops (`class_load_adapter`), with no handle of
        // this thread. The define touches only `shared`, so while a C env
        // listens for `ClassLoad` it runs with this thread's JNI binding
        // installed from `self.thread` (`post_on_own_thread`), which the
        // callback's context keeps. One load when no C env listens. A refusal
        // is returned raw and named (lane superAcc's unnamed-module hash,
        // which needs `self`) after the closure.
        let shared = self.shared;
        let define = || -> Result<(ClassId, ClassId), VmError> {
            // `cm-write-acquire` / `cm-write-release` (interpreter round i4
            // wave 94, lane L5): the lock and the guard's release work, the
            // part of `class-manager-define` the inner rows do not see.
            let acquire_clock = phase_start();
            let mut cm = shared.classes.class_manager_write();
            phase_end(DefinePhase::CmWriteAcquire, acquire_clock);
            let next_id_before = cm.class_store.next_id();
            // The census's inner rows count this define (wave 90, lane L5):
            // the class manager takes the arm at its define entry. Cleared
            // after, in case an early refusal never reached it.
            cratonvm_classloading::define_census::arm_native_define();
            let defined = match preparsed {
                Some((source, class_file)) => cm.define_class_with_options_preparsed(
                    name,
                    source,
                    cl_id,
                    define_opts,
                    class_file,
                ),
                None => cm.define_class_with_options(name, bytes, cl_id, define_opts),
            };
            let _ = cratonvm_classloading::define_census::take_native_define();
            // JIT round 20 lane superAcc: a define-time module refusal names
            // the definer's unnamed module by its identity hash, as HotSpot's
            // `verify_class_access_msg` does; the class manager cannot read
            // the module object, so the hash is appended after the closure,
            // with the class-manager guard released.
            let cid = match defined {
                Ok(cid) => cid,
                Err(e) => {
                    drop(cm);
                    return Err(e);
                }
            };
            // Record the defining loader before the write guard drops: every
            // lookup that can see the new class takes the class-manager read
            // lock, so none sees it without its loader (see
            // `DefineClassFull::defining_loader`).
            if let Some(loader) = defining_loader {
                cratonvm_native_builtins::classloader::register_defining_loader(
                    &shared.loader_identity,
                    cid.as_u32(),
                    loader,
                );
            }
            // The retransformation base: the file before the first
            // retransform-capable transformer changed it, else the file as
            // defined. Pinned when a transformer changed anything
            // (`ClassManager::insert_class_bytes_pinned`); an untransformed
            // file stays in the FIFO, where the class-path fallback and the
            // defining loader's resource can stand in for it.
            match retransform_base {
                Some(base) => cm.insert_raw_agent_base(cid, base),
                None if load_transformed => cm.insert_class_bytes_pinned(cid, bytes.to_vec()),
                None => {}
            }
            // Wave 45, lane L3: a woven class's registered natives cede to its
            // woven bytecode, as at a class-path load
            // (`instrument::pre_transform_for_load`).
            if load_transformed
                && shared
                    .natives
                    .native_methods
                    .class_may_have_natives(name)
            {
                cm.cede_native_shadows_to_woven_bytecode(cid);
            }
            let release_clock = phase_start();
            drop(cm);
            phase_end(DefinePhase::CmWriteRelease, release_clock);
            Ok((cid, next_id_before))
        };
        #[cfg(feature = "experimental-debug")]
        let defined = if shared.debug.debugger_gates.native_class_load_armed() {
            crate::runtime::interpreter::post_on_own_thread(self.thread, define)
        } else {
            define()
        };
        #[cfg(not(feature = "experimental-debug"))]
        let defined = define();
        let (cid, next_id_before) = match defined {
            Ok(defined) => defined,
            Err(e) => {
                return Err(format!(
                    "{:?}",
                    self.define_refusal_with_unnamed_module_hash(e, cl_id)
                ));
            }
        };
        phase_end(DefinePhase::ClassManagerDefine, define_clock);
        let post_clock = phase_start();

        // Review rev-w13 (JIT round 21 wave 14, item 1 of
        // `docs/feature-designs/real-jdk-mode.md`): a
        // finalizable class a Java loader defines bars its ancestors'
        // throw-before-init constructors from compiling, as the
        // `load_class_concurrent_for` listener does for the VM's own loads
        // (`SharedVm::note_finalizable_define`; no lock is held here).
        self.shared.note_finalizable_define(cid);
        // JIT round j27 wave 2 (lane REDEFINE,
        // `CRATONVM_JIT_JAVA_DEFINE_SUBTYPE_WITHDRAW`): and the class-hierarchy
        // listener (`SharedVm::note_subtype_define`). Not for a hidden class: a
        // lambda proxy per define would withdraw the bodies bound to its
        // functional interface (guarded binds, so a missing withdrawal costs
        // performance only); see the page
        // `j27-java-loader-defines-skip-the-new-subtype-listener-FIXED-20261029.md`.
        if !opts.hidden {
            self.shared.note_java_loader_subtype_define(cid);
        }

        // Round 12 wave 5: only a define that can have replaced a class (a stub
        // upgraded in place, an `allow_redefine` re-key) withdraws by name; a
        // fresh id sits beside another loader's same-named class. See
        // `define_withdraws_by_name`.
        if crate::vm::realms::jit_realm::define_withdraws_by_name(
            cid,
            next_id_before,
            opts.allow_redefine,
        ) {
            // Invalidate JIT for any class with the same name (handles redefine).
            let evicted = self.shared.jit.jit_cache.write().invalidate_for_class(name);
            if evicted > 0 {
                tracing::debug!("JIT: invalidated {evicted} method(s) due to defineClass: {name}");
            }
            // A define over an already-loaded name replaces that class's bytecode,
            // and with it every tiering verdict about the old one.
            // r10 lane `tier2`: the profile store is keyed by (class_id, method,
            // descriptor) and `class_id` is STABLE across a redefine, so without this the
            // new bytecode's execution blends into branch ratios and receiver tables
            // gathered from the OLD bytecode. The unload path in `memory/gc.rs` has always
            // invalidated both; redefine invalidated only the tiering verdicts.
            self.shared.jit.profile_store.invalidate_class(cid.as_u32());
            self.shared.jit.tiered_manager.on_class_redefined(cid, name);
        }
        phase_end(DefinePhase::PostWithdraw, post_clock);
        let init_clock = phase_start();

        // gci i16/nt: HotSpot's metaspace low-memory check on growth
        // (`jmx_check_non_heap_thresholds`). One load unless a pool threshold
        // is armed; no lock held here, no Java run.
        jmx_check_non_heap_thresholds(
            self.shared,
            NonHeapThresholdCheck::Defined {
                class_file_bytes: charge,
            },
        );

        if opts.initialize {
            // The census's `init-*` / `link-*` rows count this initialization
            // (wave 90, lane L5): `initialize_class_shared` takes the arm at
            // its entry; cleared after, for an initialization that never got
            // there (already initialized, or claimed by another thread).
            cratonvm_classloading::define_census::arm_define_init();
            let initialized = self.initialize_class(cid);
            let _ = cratonvm_classloading::define_census::take_define_init();
            // Best-effort init; failures bubble back as Err.
            if let Err(e) = initialized {
                // `MethodCallFailed::ExceptionThrown`'s Display is the raw
                // heap address, so this used to read
                //   initialize after define failed for <name>: exception
                //   thrown: ref(0x7b5b041c3720)
                // which names neither the exception class nor its message. The
                // caller turns this string into a `ClassFormatError` message,
                // so the pointer is what an operator, a probe diff and a
                // regression vector all end up holding. `describe_throwable`
                // is the same reader the uncaught-exception path uses.
                let cause = match &e {
                    cratonvm_types::error::MethodCallFailed::ExceptionThrown(exc) => {
                        // `initialize_class` already wraps a `<clinit>` failure
                        // in `ExceptionInInitializerError` per JVMS §5.5 (see
                        // vm_util.rs). Describing THAT object here would hand
                        // the caller "ExceptionInInitializerError" (no message
                        // — the EIIE ctor sets `cause`, not `detailMessage`) as
                        // the "cause" half of this function's own
                        // "initialize after define failed for X: <cause>"
                        // string. The two native-builtins call sites that
                        // parse this string (`lang_system::
                        // exception_in_initializer_error`) build a FRESH
                        // EIIE around whatever they parse out of `<cause>` —
                        // so describing the outer EIIE here double-wraps:
                        // ExceptionInInitializerError(cause=
                        // ExceptionInInitializerError()) instead of
                        // ExceptionInInitializerError(cause=<original
                        // RuntimeException>) that HotSpot produces. Describe
                        // the EIIE's OWN cause instead, so the string names
                        // the true root exception and the reconstruction one
                        // layer up wraps it exactly once, matching HotSpot.
                        let root = unwrap_exception_in_initializer_error(&self.shared, *exc);
                        describe_throwable(&self.shared, root)
                    }
                    other => other.to_string(),
                };
                return Err(format!(
                    "initialize after define failed for {name}: {cause}"
                ));
            }
        }
        if opts.initialize {
            phase_end(DefinePhase::PostInitialize, init_clock);
        }
        phase_end(DefinePhase::VmPostDefine, post_clock);
        Ok(cid)
    }

    fn redefine_class(&mut self, class_id: ClassId, new_bytes: &[u8]) -> Result<(), String> {
        // WP2.4-F1 вЂ” JEP 109 redefine path: route to
        // `class_manager::redefine_class` (Agent 2.4-B) which performs
        // the in-place method-body swap, refreshes the vtable, bumps
        // the per-class `redefine_generations` counter, and fires the
        // JIT invalidate hook. Earlier versions of this binding called
        // `define_class_with_options(allow_redefine: true)`, which
        // minted a fresh ClassId for the redefined class and left the
        // ORIGINAL ClassId's `Class.methods` table untouched вЂ” so the
        // already-loaded `Target` instance kept dispatching to the
        // pre-transform bytecode and the per-thread invoke cache
        // (keyed on the original ClassId) never observed a generation
        // bump.  The route below makes the in-place swap semantics
        // observable end-to-end.
        self.redefine_class_with(class_id, new_bytes, false)
            .map_err(|refused| format!("{:?}", refused.into_linkage_error()))
    }

    /// `Instrumentation.retransformClasses` path. Same in-place swap as
    /// `redefine_class`, but preserves the class's ORIGINAL cached bytes so the
    /// NEXT retransform re-runs the transformer chain from the original rather
    /// than the already-woven bytes (otherwise `mockStatic(X)` followed by
    /// `mock(X)` double-instruments X and the instance mock fails).
    fn retransform_class(&mut self, class_id: ClassId, new_bytes: &[u8]) -> Result<(), String> {
        self.redefine_class_with(class_id, new_bytes, true)
            .map_err(|refused| format!("{:?}", refused.into_linkage_error()))
    }

    /// Interpreter round i1 wave 27, lane L3: `ClassManager::check_redefinition`
    /// under the read lock, the refusal mapped to libinstrument's throwable.
    fn check_class_redefinition(
        &self,
        class_id: ClassId,
        new_bytes: &[u8],
    ) -> Result<(), cratonvm_native_api::InstrumentRefusal> {
        let options = cratonvm_classloading::RedefineOptions::default();
        self.shared
            .classes
            .class_manager
            .read()
            .check_redefinition(class_id, new_bytes, &options)
            .map_err(instrument_refusal_of)
    }

    /// Interpreter round i1 wave 27, lane L3: see the trait. A redefinition
    /// (not a retransformation) whose class file a retransform-capable
    /// transformer changed records the file as it was before that change as
    /// the class's retransformation base, as JVMTI caches it
    /// (`VM_RedefineClasses::redefine_single_class`); `redefine_class_with`
    /// recorded the transformed file, so the next retransform wove it twice
    /// (`L3/L3W27RedefineKeepsTheBase`).
    fn redefine_class_as_instrument(
        &mut self,
        class_id: ClassId,
        new_bytes: &[u8],
        retransform: bool,
        retransform_base: Option<&[u8]>,
    ) -> Result<(), cratonvm_native_api::InstrumentRefusal> {
        self.redefine_class_with(class_id, new_bytes, retransform)
            .map_err(instrument_refusal_of)?;
        match (retransform, retransform_base) {
            (false, Some(base)) => {
                // Pinned, as `redefine_class_typed` pinned the file it installed
                // (interpreter round i1 wave 28, lane L3).
                self.shared
                    .classes
                    .class_manager_write()
                    .insert_raw_agent_base(class_id, base.to_vec());
            }
            (true, Some(base)) => {
                // The base this retransform started from, kept as JVMTI keeps
                // it once a retransformation changed the class: the live class
                // is woven now, so a base the FIFO evicts later could only be
                // reconstituted from the woven class (interpreter round i1
                // wave 29, lane L3; `L3/L3W29ReconstitutedBase`, row
                // `generated-again`). A base already pinned is the same file.
                let mut cm = self.shared.classes.class_manager_write();
                if !cm.is_class_bytes_pinned(class_id) {
                    cm.insert_raw_agent_base(class_id, base.to_vec());
                }
            }
            (_, None) => {}
        }
        Ok(())
    }

    /// Interpreter round i1 wave 25, lane L6: see the trait. A nested call
    /// (a transformer that retransforms) joins the open batch.
    fn begin_redefinition_batch(&mut self) -> bool {
        if self.thread.redefinition_batch.is_some() {
            return false;
        }
        self.thread.redefinition_batch =
            Some(self.shared.jit.jit_cache.bodies_withdrawn_by_redefinition());
        true
    }

    /// Interpreter round i1 wave 25, lane L6: the loop-exit handshake every
    /// redefinition of the batch deferred (`redefine_class_with`), once, when
    /// any of them withdrew a compiled body.
    fn end_redefinition_batch(&mut self, opened: bool) {
        if !opened {
            return;
        }
        let Some(before) = self.thread.redefinition_batch.take() else {
            return;
        };
        if self.shared.jit.jit_cache.bodies_withdrawn_by_redefinition() != before {
            crate::runtime::interpreter::request_withdrawn_body_exits(
                self.shared,
                self.thread.thread_id,
                before,
            );
        }
    }

    /// Interpreter round i1 wave 37, lane L3: see the trait; the call-wide
    /// redefinition fence (`obsolete_frames::raise_install_fence`).
    ///
    /// Interpreter round i1 wave 40, lane L2 follow-up: the fence holds only
    /// interpreter loops, so the call also holds this VM's code-cache
    /// publications (`JitCache::hold_publications`) from before its first
    /// install to after its last: no body compiled from a class the call
    /// already installed runs beside a compiled body of one it has not
    /// (`L3W38RedefineTwoClassesSamePool.java`).
    fn begin_redefinition_install(&mut self, classes: &[(ClassId, &[u8])]) -> bool {
        let held = crate::runtime::interpreter::obsolete_frames::raise_install_fence(
            self.shared,
            self.thread.thread_id,
            classes,
        );
        if held {
            self.shared.jit.jit_cache.hold_publications();
            // Interpreter round i1 wave 41, lane L2: the compiled half of the
            // fence -- every compiled dependent of every class of the call is
            // withdrawn, and its running frames sent to the interpreter (where
            // the fence holds them), before the first class is installed.
            // Armed first, as `redefine_class_with` arms it: an unarmed cache
            // patches nothing.
            self.shared
                .jit
                .jit_cache
                .arm_not_entrant(cratonvm_jit::NotEntrantHook {
                    // Cast: a helper's address, baked into generated code.
                    helper: crate::jit::helpers::jit_not_entrant_entry as *const () as usize,
                    // Cast: this VM's address, the helpers' `vm_ptr` word.
                    vm_ptr: self.shared as *const crate::vm::SharedVm as usize,
                });
            crate::runtime::interpreter::withdraw_compiled_dependents_ahead_of_install(
                self.shared,
                self.thread.thread_id,
                classes,
            );
        }
        held
    }

    /// Interpreter round i1 wave 37, lane L3: see the trait. Wave 40 (lane L2
    /// follow-up): releases the publication hold after the fence is down.
    fn end_redefinition_install(&mut self, held: bool) {
        if held {
            crate::runtime::interpreter::obsolete_frames::lower_install_fence(
                self.shared,
                self.thread.thread_id,
            );
            self.shared.jit.jit_cache.release_publications();
        }
    }

    fn list_loaded_class_ids(&self) -> Vec<ClassId> {
        let cm = self.shared.classes.class_manager.read();
        // zt t18/df7 (x13 item 21a): an unbound or withdrawn define is in no
        // dictionary, so no listing names it (HotSpot's never `loaded`).
        cm.class_store.iter_named().map(|c| c.id).collect()
    }

    fn list_lambda_proxy_class_ids(&self) -> Vec<ClassId> {
        let mut ids: Vec<ClassId> = self
            .shared
            .classes
            .lambda_proxies
            .read()
            .keys()
            .copied()
            .collect();
        ids.sort_unstable_by_key(|id| id.as_u32());
        ids
    }

    fn list_initiated_class_ids(&self, loader_id: u32) -> Vec<ClassId> {
        use cratonvm_types::ClassLoaderId;
        // The exact decode: `0` is the bootstrap loader. The sole caller,
        // `Instrumentation.getInitiatedClasses`
        // (`instrument::native_get_initiated_classes0`), maps its argument to
        // the loader's namespace since interpreter round i1 wave 41 (lane
        // L3); it used to pass `0` for every loader, decoded here as the
        // application namespace.
        let cl_id = ClassLoaderId::from_native_id(loader_id);
        // The classes it initiated as another loader's (JVMS §5.3; a user
        // loader under `--real-jdk`, the only one the VM records), as
        // HotSpot's per-loader dictionary lists them. Copied out before the
        // class manager is read: no lock is held inside the other.
        let recorded: Vec<ClassId> = if matches!(cl_id, ClassLoaderId::UserDefined(_)) {
            let records = self.shared.classes.initiating_records.read();
            records
                .get(&cl_id)
                .map(|initiated| initiated.values().copied().collect())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        // The JDK classes it initiated by resolution, when a start-up agent
        // armed the record (wave 46, lane L5; see the scan below). Copied out
        // before the class manager is read, as above.
        let armed = self
            .shared
            .classes
            .jdk_names_initiated_armed
            .load(std::sync::atomic::Ordering::Relaxed);
        let jdk_recorded: Vec<ClassId> = if armed && cl_id != ClassLoaderId::Bootstrap {
            self.shared
                .classes
                .jdk_names_initiated
                .read()
                .get(&cl_id)
                .map(|set| set.iter().copied().collect())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        // With the resolved class-entry table on (interpreter round i1 wave 47,
        // lane L5; `ClassRealm::resolved_entry_table`), the classes of other
        // loaders the loader's classes' entries resolved to -- through the
        // class door or as a method reference's owner -- ARE its resolution
        // rows of HotSpot's dictionary, and replace the constant-pool scan
        // below. The loader's classes are collected under a class-manager
        // guard dropped before the table's is taken.
        let entry_table = self.shared.classes.resolved_entry_table;
        let table_rows: Vec<ClassId> = if entry_table && cl_id != ClassLoaderId::Bootstrap {
            let referencing: Vec<ClassId> = {
                let cm = self.shared.classes.class_manager.read();
                cm.class_store
                    .iter()
                    .filter(|c| c.loader_id == cl_id && !c.name.starts_with('['))
                    .map(|c| c.id)
                    .collect()
            };
            crate::runtime::resolve::initiating_records::resolved_entries_of(
                self.shared,
                &referencing,
            )
        } else {
            Vec::new()
        };
        // HotSpot's list (measured, JDK 25; interpreter round i1 wave 42, lane
        // L3, `L3W42InitiatedArrays.java`) is the
        // loader's dictionary plus the eight primitive array classes, and,
        // for each class in it, the array classes of that class that exist
        // (`Gen`, `Gen[]`; `Object`, `Object[]`, `Object[][]`), and every
        // existing array of a primitive type (`[[I`) for every loader. HotSpot
        // creates the eight at start-up; here they are made on demand, so
        // they are made first. A hidden class is in no dictionary.
        //
        // `Object[]` and `String[]` exist on HotSpot from start-up too
        // (`Universe::genesis` makes `Object[]`; the launcher resolves
        // `main(String[])`), and the bootstrap loader lists them. CratonVM
        // allocates `main`'s arguments with the element class alone
        // (`vm-cli` `run`), so `[Ljava/lang/String;` was made only when
        // Java code first named it -- after the list, in
        // `L3W42InitiatedArrays`' `boot` row (`string[]=false`; interpreter
        // round i1 wave 43, lane L5).
        const PRIMITIVE_ARRAYS: [&str; 10] = [
            "[Z",
            "[C",
            "[F",
            "[D",
            "[B",
            "[S",
            "[I",
            "[J",
            "[Ljava/lang/Object;",
            "[Ljava/lang/String;",
        ];
        {
            let missing = {
                let cm = self.shared.classes.class_manager.read();
                PRIMITIVE_ARRAYS.iter().any(|name| {
                    cm.loaded_class_under_exact_key(name, ClassLoaderId::Bootstrap)
                        .is_none()
                })
            };
            if missing {
                let mut cm = self.shared.classes.class_manager_write_settling();
                for name in PRIMITIVE_ARRAYS {
                    let _ = cm.load_array_class_for_loader(name, ClassLoaderId::Bootstrap);
                }
            }
        }
        let cm = self.shared.classes.class_manager.read();
        let mut ids: Vec<ClassId> = Vec::new();
        let mut in_dictionary: std::collections::HashSet<ClassId> =
            std::collections::HashSet::new();
        for c in cm.class_store.iter_named() {
            if c.loader_id != cl_id || c.hidden || c.name.starts_with('[') {
                continue;
            }
            if in_dictionary.insert(c.id) {
                ids.push(c.id);
            }
        }
        // The direct supertypes of the classes it defined: JVMS §5.3.5 step 3
        // resolves them with the defining loader as the initiating loader, so
        // a user loader that delegated `java/lang/Object` initiated it
        // (HotSpot lists `Object` for the probe's loader). Collected from the
        // defined classes only, before the recorded ones are added.
        let mut supertypes: Vec<ClassId> = Vec::new();
        for &id in &ids {
            if let Some(c) = cm.class_store.get(id) {
                supertypes.extend(c.superclass);
                supertypes.extend(c.interfaces.iter().copied());
            }
        }
        // The JDK classes its classes resolved by name (a constant, `new`,
        // a member's owner): HotSpot's resolution files each in the
        // resolving class's loader's dictionary, so the application loader
        // lists `java.lang.String`, a user loader a JDK class its class
        // named. The VM records no initiating loader for a JDK name (that
        // would be a write on every JDK name's resolution), so the list asks
        // the constant pools of the classes the loader defined, hidden ones
        // included, for the JDK classes (bootstrap- or platform-defined, and
        // loaded) their `CONSTANT_Class` entries name -- an array entry by
        // its element class. It over-lists an entry never resolved whose
        // class another loader has loaded (HotSpot lists that one only when
        // its verifier loaded it through the loader). Interpreter round i1
        // wave 44, lane L5; probe `L5W44InitiatedJdkClasses`.
        //
        // Since wave 46 (lane L5), when a start-up agent armed the record
        // (`ClassRealm::jdk_names_initiated`, written by the `CONSTANT_Class`
        // resolution door and a user loader's `Class.forName`), the record
        // answers for those resolutions, and the scan asks only the class
        // entries a field or method reference names: a member's owner, which
        // the invoke and field doors resolve by the receiver or by name
        // without that door. A bare class constant never resolved (an `ldc`
        // in a method never run) is no longer listed.
        let trace_initiated =
            armed && false;
        if trace_initiated {
            eprintln!(
                "[RETRANSFORM] getInitiatedClasses({cl_id:?}): {} recorded JDK classes, {} \
                 resolved class-entry rows (table {})",
                jdk_recorded.len(),
                table_rows.len(),
                if entry_table { "on" } else { "off" }
            );
            for &class_id in jdk_recorded.iter().chain(&table_rows) {
                if let Some(c) = cm.class_store.get(class_id) {
                    eprintln!(
                        "[RETRANSFORM] getInitiatedClasses({cl_id:?}): recorded {}",
                        c.name
                    );
                }
            }
        }
        let resolver = crate::runtime::resolve::MemberResolver::new(self.shared);
        let mut jdk_named: Vec<ClassId> = jdk_recorded;
        jdk_named.extend(table_rows);
        if cl_id != ClassLoaderId::Bootstrap && !entry_table {
            use cratonvm_reader::constant_pool::ConstantPoolEntry;
            let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
            let mut owners: std::collections::HashSet<u16> = std::collections::HashSet::new();
            for c in cm.class_store.iter_named() {
                if c.loader_id != cl_id || c.name.starts_with('[') {
                    continue;
                }
                let cp = &c.constant_pool;
                if armed {
                    owners.clear();
                    for index in 1..cp.len() {
                        let Ok(index) = u16::try_from(index) else {
                            break;
                        };
                        match cp.get(index) {
                            Some(
                                ConstantPoolEntry::FieldReference { class_index, .. }
                                | ConstantPoolEntry::MethodReference { class_index, .. }
                                | ConstantPoolEntry::InterfaceMethodReference { class_index, .. },
                            ) => {
                                owners.insert(*class_index);
                            }
                            _ => {}
                        }
                    }
                }
                for index in 1..cp.len() {
                    // A bare class entry counts when the record saw its
                    // resolution, or when its `ldc` record holds the class
                    // (JVMS §5.4.3's record of the entry, whichever path
                    // filled it; wave 46 follow-up: `L5W44InitiatedJdkClasses`
                    // lost `java.lang.String`, a bare `ldc` the class-door
                    // record did not hold on the host).
                    if armed
                        && !u16::try_from(index).is_ok_and(|index| {
                            owners.contains(&index)
                                || resolver
                                    .probe_constant(resolver.scope(c.id), index)
                                    .into_hit()
                                    .is_some()
                        })
                    {
                        continue;
                    }
                    if trace_initiated {
                        if let Some(name) =
                            u16::try_from(index).ok().and_then(|i| cp.get_class_name(i))
                        {
                            eprintln!(
                                "[RETRANSFORM] getInitiatedClasses({cl_id:?}): {} #{index} {name} scanned",
                                c.name
                            );
                        }
                    }
                    let Some(name) = u16::try_from(index)
                        .ok()
                        .and_then(|index| cp.get_class_name(index))
                    else {
                        continue;
                    };
                    let dims = name.bytes().take_while(|&b| b == b'[').count();
                    let leaf = if dims == 0 {
                        name
                    } else {
                        match name[dims..]
                            .strip_prefix('L')
                            .and_then(|rest| rest.strip_suffix(';'))
                        {
                            Some(leaf) => leaf,
                            None => continue,
                        }
                    };
                    if !seen.insert(leaf) {
                        continue;
                    }
                    // A name the loader defines itself resolves to its own
                    // class, which is listed already.
                    if cm.loaded_class_under_exact_key(leaf, cl_id).is_some() {
                        continue;
                    }
                    let jdk = cm
                        .loaded_class_under_exact_key(leaf, ClassLoaderId::Bootstrap)
                        .or_else(|| {
                            (cl_id != ClassLoaderId::Extension)
                                .then(|| {
                                    cm.loaded_class_under_exact_key(leaf, ClassLoaderId::Extension)
                                })
                                .flatten()
                        });
                    jdk_named.extend(jdk);
                }
            }
        }
        for class_id in recorded.into_iter().chain(supertypes).chain(jdk_named) {
            let listable = cm
                .class_store
                .get(class_id)
                .is_some_and(|c| !c.hidden && !c.name.starts_with('['));
            if listable && in_dictionary.insert(class_id) {
                ids.push(class_id);
            }
        }
        // Names an array whose leaf cannot be found (the orchestrator's trace
        // of the wave-42 `boot string[]` row).
        let trace_arrays = false;
        // The array classes: an array whose leaf is primitive for every
        // loader, and an array of a reference leaf when the leaf is listed.
        for c in cm.class_store.iter() {
            let dims = c.name.bytes().take_while(|&b| b == b'[').count();
            if dims == 0 {
                continue;
            }
            let leaf_descriptor = &c.name[dims..];
            let listed = if leaf_descriptor.len() == 1 {
                true
            } else {
                // Walk the component chain down to the leaf class.
                let mut current = c;
                let mut leaf = None;
                while let Some(info) = current.array_info.as_ref() {
                    match cm.class_store.get(info.component_class_id) {
                        Some(component) if component.name.starts_with('[') => {
                            current = component;
                        }
                        Some(component) => {
                            leaf = Some(component.id);
                            break;
                        }
                        None => break,
                    }
                }
                // An array minted without `array_info` (the one cause the
                // code shows for the host's wave-42 run listing no `String[]`
                // for the bootstrap loader) names its leaf only by
                // its descriptor: the class of that name under the array's
                // own loader key, else under the application loader's (an
                // application class's array is filed under the bootstrap
                // loader).
                if leaf.is_none() && current.array_info.is_none() {
                    if let Some(leaf_name) = leaf_descriptor
                        .strip_prefix('L')
                        .and_then(|rest| rest.strip_suffix(';'))
                    {
                        let leaf_name = leaf_name.replace('.', "/");
                        leaf = cm
                            .loaded_class_under_exact_key(&leaf_name, c.loader_id)
                            .or_else(|| {
                                cm.loaded_class_under_exact_key(
                                    &leaf_name,
                                    ClassLoaderId::Application,
                                )
                            });
                    }
                }
                if leaf.is_none() && trace_arrays {
                    eprintln!(
                        "[RETRANSFORM] getInitiatedClasses({cl_id:?}): array {} ({:?}) names \
                         no leaf class (array_info {})",
                        c.name,
                        c.id,
                        if c.array_info.is_some() {
                            "present"
                        } else {
                            "absent"
                        }
                    );
                }
                if trace_arrays && c.loader_id == cl_id {
                    if let Some(found) = leaf.filter(|found| !in_dictionary.contains(found)) {
                        eprintln!(
                            "[RETRANSFORM] getInitiatedClasses({cl_id:?}): array {} ({:?}) \
                             has leaf {found:?}, which is not in the list",
                            c.name, c.id
                        );
                    }
                }
                leaf.is_some_and(|leaf| in_dictionary.contains(&leaf))
            };
            if listed {
                ids.push(c.id);
            }
        }
        ids
    }

    fn note_initiating_load(
        &mut self,
        loader_ns: u32,
        internal_name: &str,
        class_id: ClassId,
    ) -> Result<(), MethodCallFailed> {
        // The constraint table's own check (`--real-jdk`; one read guard and
        // an `is_empty` test while nothing is constrained), its refusal
        // materialised as the Java `LinkageError` the native throws; then the
        // initiating record (interpreter round i1 wave 38, lane L5).
        let loader = cratonvm_types::ClassLoaderId::UserDefined(loader_ns);
        match crate::runtime::resolve::loader_constraints::check_initiating_load(
            self.shared,
            loader,
            internal_name,
            class_id,
        ) {
            Ok(()) => {
                crate::runtime::resolve::initiating_records::record_initiating_load(
                    self.shared,
                    loader,
                    internal_name,
                    class_id,
                );
                // `Class.forName(jdkName, init, loader)` files the JDK class in
                // the loader's dictionary as well (wave 46, lane L5).
                if self
                    .shared
                    .classes
                    .jdk_names_initiated_armed
                    .load(std::sync::atomic::Ordering::Relaxed)
                {
                    crate::runtime::resolve::initiating_records::record_jdk_name_initiated(
                        self.shared,
                        loader,
                        class_id,
                    );
                }
                Ok(())
            }
            Err(MethodCallFailed::InternalError(VmError::Linkage(error))) => Err(
                crate::runtime::exceptions::throw_linkage_error(self.shared, self.thread, error),
            ),
            Err(other) => Err(other),
        }
    }

    fn initiated_class_for_loader(&self, loader_ns: u32, internal_name: &str) -> Option<ClassId> {
        crate::runtime::resolve::initiating_records::initiated_class_for_find_loaded(
            self.shared,
            cratonvm_types::ClassLoaderId::UserDefined(loader_ns),
            internal_name,
        )
    }

    fn jdk_names_initiated_armed(&self) -> bool {
        self.shared
            .classes
            .jdk_names_initiated_armed
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    fn note_jdk_class_initiated(&self, loader_native_id: u32, class_id: ClassId) {
        if !self.jdk_names_initiated_armed() {
            return;
        }
        let loader = cratonvm_types::ClassLoaderId::from_native_id(loader_native_id);
        crate::runtime::resolve::initiating_records::record_jdk_name_initiated(
            self.shared,
            loader,
            class_id,
        );
    }

    fn note_reflected_members(&self, class_id: ClassId, kind: u8, public_only: bool) {
        use crate::runtime::resolve::initiating_records::{
            note_reflected_members, ReflectedMembers,
        };
        let members = match kind {
            0 => ReflectedMembers::Methods,
            1 => ReflectedMembers::Constructors,
            _ => ReflectedMembers::Fields,
        };
        note_reflected_members(self.shared, class_id, members, public_only);
    }

    fn note_agent_start(&self, agent_class_id: Option<ClassId>) {
        crate::runtime::resolve::initiating_records::note_agent_start(self.shared, agent_class_id);
    }

    fn builtin_find_loaded_initiated_only(&self) -> bool {
        self.shared.classes.builtin_find_loaded_initiated_only
    }

    fn builtin_loader_initiated(&self, loader_native_id: u32, class_id: ClassId) -> bool {
        crate::runtime::resolve::initiating_records::builtin_loader_initiated(
            self.shared,
            cratonvm_types::ClassLoaderId::from_native_id(loader_native_id),
            class_id,
        )
    }

    fn direct_subtypes_of_loaders(
        &self,
        class_id: ClassId,
        loader_native_ids: &[u32],
        limit: usize,
    ) -> Vec<ClassId> {
        let cm = self.shared.classes.class_manager.read_recursive();
        // JIT r23 wave 12: the store remembers misses, so a repeated ask
        // reads only the subtypes added since (`CRATONVM_SUBTYPE_LOADER_MISS_MEMO`).
        cm.class_store
            .direct_subtypes_of_loaders(class_id, loader_native_ids, limit)
    }

    fn builtin_image_class_has_direct_supertype(
        &self,
        class_id: ClassId,
        loader_native_id: u32,
    ) -> Option<bool> {
        if !self.shared.classes.builtin_image_class_rows {
            return None;
        }
        let cm = self.shared.classes.class_manager.read_recursive();
        // JIT r24 (proxy-define residual page): through the store's miss memo,
        // keyed by the loader; a JDK root's direct subtypes are every class
        // defined since, and this is asked on every fresh loader.
        let found = cm.class_store.direct_subtype_any_since(
            class_id,
            u64::from(loader_native_id),
            |c| {
                c.loader_id == cratonvm_types::ClassLoaderId::Bootstrap
                    && c.module_name
                        .as_deref()
                        .and_then(|m| cm.module_registry.image_module_builtin_loader(m))
                        .is_some_and(|l| l.to_native_id() == loader_native_id)
            },
        );
        Some(found)
    }

    fn note_builtin_find_loaded_initiated(&self, outcome: u8) {
        let counter = match outcome {
            0 => &self.shared.classes.builtin_find_loaded_withheld,
            1 => &self.shared.classes.builtin_find_loaded_served_by_row,
            _ => &self.shared.classes.builtin_find_loaded_served_by_supertype,
        };
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn allocate_loader_id(&mut self) -> u32 {
        use std::sync::atomic::Ordering;
        // User-defined loader namespace ids MUST start at 3: the `ClassLoaderId`
        // i32 encoding (see `loader_id_of_class` / `define_class_full`) reserves
        // 0=Bootstrap, 1=Extension, 2=Application. Starting the counter at 1
        // handed the first two user loaders ids 1 and 2, which alias
        // `UserDefined(1)`↔Extension and `UserDefined(2)`↔Application — so a
        // user-loader namespace was indistinguishable from a built-in one. That
        // broke loader-faithful resolution for bytecode-enhanced Hibernate
        // entities: the `EnhancingClassLoader` got namespace 2, its enhanced
        // entity was stored as `UserDefined(2)`, and `inherit_lookup_loader`'s
        // `raw < 3` guard then re-homed the ByteBuddy instantiator into the
        // Application namespace, so `new Country` resolved the *un-enhanced* copy
        // → `ClassCastException`/`PersistentAttributeInterceptable`. Starting at 3
        // guarantees every allocated namespace is a genuine `UserDefined` id.
        // The boundary itself is `ClassLoaderId::NATIVE_FIRST_USER_DEFINED`,
        // which the codec (`to_native_id` / `from_native_id*`) is defined
        // against — the counter (`SharedVm::next_loader_id`) is seeded from there.
        // Per VM since interpreter round i5 wave 105, lane L5 (was a process
        // `static`): no table keyed by a bare namespace id spans VMs any more.
        self.shared.next_loader_id.fetch_add(1, Ordering::Relaxed)
    }
}

