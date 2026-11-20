#[allow(unused_imports)]
use super::*;

impl<'a> NativeInvokeAccess for NativeContextImpl<'a> {
    fn invoke(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        let mut fwd_buf: Vec<Value> = Vec::new();
        let args = forward_boundary_args(&self.shared.mem.heap, args, &mut fwd_buf);
        // JNI `FindClass` (see `native_caller_loader_anchor`): resolve the
        // owner through the calling class's user-defined loader first.
        let mut preresolved: Option<ClassId> = None;
        let mut args_now: Option<Vec<Value>> = None;
        if let Some(anchor) = self.native_caller_loader_anchor(class_name) {
            let pin_base = self.thread.native_pin_roots.len();
            let pins = pin_native_object_values(self.thread, args);
            let resolved = crate::runtime::interpreter::resolve_class_or_array_loader_aware(
                self.shared,
                self.thread,
                anchor,
                class_name,
            );
            let fresh = reread_native_object_values(self.thread, args, &pins);
            self.thread.native_pin_roots.truncate(pin_base);
            if let Ok(cid) = resolved {
                preresolved = Some(cid);
                args_now = Some(fresh);
            }
        }
        // `invoke_shared`, classifying an inherited static under
        // `native_invoke_static_tail_enforced` (and its census).
        let before = served_raise_mark_addr(self.thread);
        // The stack-walker residual-costs page, item 1
        // (`VmConfig::native_invoke_memo`): the native->Java callback memo's
        // `NativeContext::invoke` half. A hit is the ordinary tail's own
        // `interpreter::execute` on the declaring class it reached for this
        // class name, first-argument class, name and descriptor; a miss arms
        // the witness and fills only if the dispatch reached that tail. A
        // loader-anchored name (`native_caller_loader_anchor`) is not keyed by
        // its name alone, so it keeps the full door.
        let memo_key = (preresolved.is_none()
            && crate::runtime::env_cache::native_callback_memo()
            && (crate::runtime::native_callee_memo::SURVIVES_REDEFINITION
                || !crate::classloading::any_class_redefined()))
        .then(|| crate::runtime::native_callee_memo::static_first_arg_key(self.shared, args))
        .flatten();
        let result = match memo_key {
            Some(first_key) => self.invoke_memoized(class_name, method_name, descriptor, args, first_key),
            None => native_context_invoke(
                self.shared,
                self.thread,
                class_name,
                method_name,
                descriptor,
                args_now.as_deref().unwrap_or(args),
                preresolved,
            ),
        };
        let owner = preresolved.map_or(DoorOwner::Name(class_name), DoorOwner::Id);
        door_result_noted(
            self.shared,
            self.thread,
            before,
            owner,
            method_name,
            descriptor,
            result,
        )
    }

    fn invoke_by_class_id(
        &mut self,
        class_id: ClassId,
        _class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        let mut fwd_buf: Vec<Value> = Vec::new();
        let args = forward_boundary_args(&self.shared.mem.heap, args, &mut fwd_buf);
        let before = served_raise_mark_addr(self.thread);
        let result = invoke_by_class_id_shared(
            self.shared,
            self.thread,
            class_id,
            method_name,
            descriptor,
            args,
        );
        door_result_noted(
            self.shared,
            self.thread,
            before,
            DoorOwner::Id(class_id),
            method_name,
            descriptor,
            result,
        )
    }

    /// The by-name static tail ([`invoke_static_shared`]): the same load and
    /// dispatch as [`Self::invoke`]'s `invoke_shared`, but the class
    /// initialized is the method's declaring class (JVMS §5.5). In every
    /// mode: a `REF_invokeStatic` handle's first call initializes what
    /// `DirectMethodHandle.ensureInitialized` names, the declaring class
    /// (interpreter round i1 wave 16, lane L3).
    fn invoke_static(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        let mut fwd_buf: Vec<Value> = Vec::new();
        let args = forward_boundary_args(&self.shared.mem.heap, args, &mut fwd_buf);
        let before = served_raise_mark_addr(self.thread);
        // Interpreter round i2 wave 63, lane L3 (`VmConfig::served_raise_door_key`).
        let door = door_in_flight_enter(
            self.shared,
            self.thread,
            DoorOwner::Name(class_name),
            class_name,
            method_name,
            descriptor,
        );
        let result = invoke_static_shared(
            self.shared,
            self.thread,
            class_name,
            method_name,
            descriptor,
            args,
        );
        door_in_flight_leave(self.thread, door);
        door_result_noted(
            self.shared,
            self.thread,
            before,
            DoorOwner::Name(class_name),
            method_name,
            descriptor,
            result,
        )
    }

    /// [`Self::invoke_static`] on a resolved owner: [`invoke_static_on_owner`],
    /// the same dispatch as [`Self::invoke_by_class_id`] with the declaring
    /// class initialized instead of `class_id`.
    fn invoke_static_by_class_id(
        &mut self,
        class_id: ClassId,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        let mut fwd_buf: Vec<Value> = Vec::new();
        let args = forward_boundary_args(&self.shared.mem.heap, args, &mut fwd_buf);
        let before = served_raise_mark_addr(self.thread);
        // Interpreter round i2 wave 63, lane L3 (`VmConfig::served_raise_door_key`).
        let door = door_in_flight_enter(
            self.shared,
            self.thread,
            DoorOwner::Id(class_id),
            class_name,
            method_name,
            descriptor,
        );
        let result = invoke_static_on_owner(
            self.shared,
            self.thread,
            class_id,
            class_name,
            method_name,
            descriptor,
            args,
        );
        door_in_flight_leave(self.thread, door);
        door_result_noted(
            self.shared,
            self.thread,
            before,
            DoorOwner::Id(class_id),
            method_name,
            descriptor,
            result,
        )
    }

    /// [`Self::invoke_static`] / [`Self::invoke_static_by_class_id`] for a
    /// handle that remembers its settled owner: [`invoke_static_settling`].
    fn invoke_static_settling(
        &mut self,
        owner: Option<ClassId>,
        settled: Option<ClassId>,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> (MethodCallResult, Option<ClassId>) {
        self.invoke_static_settling_door(
            owner,
            settled,
            None,
            None,
            class_name,
            method_name,
            descriptor,
            args,
        )
    }

    fn invoke_static_settling_minted(
        &mut self,
        owner: Option<ClassId>,
        settled: Option<ClassId>,
        first_class: ClassId,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> (MethodCallResult, Option<ClassId>) {
        self.invoke_static_settling_door(owner, settled, None, Some(first_class),
            class_name, method_name, descriptor, args)
    }

    /// [`Self::invoke_static_settling`] with the handle record's door-key
    /// slot: interpreter round i2 wave 65, lane L3
    /// (`VmConfig::served_raise_settled_door_key`; switch off: the slot is
    /// never read or filled).
    #[allow(clippy::too_many_arguments)]
    fn invoke_static_settling_keyed(
        &mut self,
        owner: Option<ClassId>,
        settled: Option<ClassId>,
        door_key: &cratonvm_native_api::SettledDoorKey,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> (MethodCallResult, Option<ClassId>) {
        let door_key = self
            .shared
            .config
            .served_raise_settled_door_key
            .then_some(door_key);
        self.invoke_static_settling_door(
            owner,
            settled,
            door_key,
            None,
            class_name,
            method_name,
            descriptor,
            args,
        )
    }

    /// Interpreter round i2 wave 66, lane L3
    /// (`VmConfig::served_raise_carrier_door_key`): the carrier handle keeps
    /// a door key only while the VM would read and fill it.
    fn carrier_settled_door_key(&self) -> bool {
        self.shared.config.served_raise_carrier_door_key
            && self.shared.config.served_raise_settled_door_key
    }

    fn invoke_special(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        // WP2.9 -- invokespecial semantics: invoke the *resolved* method on
        // `class_name` with no virtual dispatch and no iface/abstract retarget
        // to the receiver's concrete class. Required for `Lookup.findSpecial`
        // private-to-private calls and default-method super-call patterns.
        let mut fwd_buf: Vec<Value> = Vec::new();
        let args = forward_boundary_args(&self.shared.mem.heap, args, &mut fwd_buf);
        let before = served_raise_mark_addr(self.thread);
        let result = invoke_special_shared(
            self.shared,
            self.thread,
            class_name,
            method_name,
            descriptor,
            args,
        );
        door_result_noted(
            self.shared,
            self.thread,
            before,
            DoorOwner::Name(class_name),
            method_name,
            descriptor,
            result,
        )
    }

    fn invoke_special_by_class_id(
        &mut self,
        class_id: ClassId,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        // Same primitive the JIT's invokespecial resolution uses
        // (`invoke_special_shared_on_class`) - reused here for reflective
        // `Method.invoke` dispatch of private / cross-package package-private
        // instance methods, which has the identical loader-identity
        // requirement. See the trait method's doc comment.
        let mut fwd_buf: Vec<Value> = Vec::new();
        let args = forward_boundary_args(&self.shared.mem.heap, args, &mut fwd_buf);
        let before = served_raise_mark_addr(self.thread);
        let result = invoke_special_shared_on_class(
            self.shared,
            self.thread,
            class_id,
            class_name,
            method_name,
            descriptor,
            args,
        );
        door_result_noted(
            self.shared,
            self.thread,
            before,
            DoorOwner::Id(class_id),
            method_name,
            descriptor,
            result,
        )
    }

    fn invoke_special_bytecode_only(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        let mut fwd_buf: Vec<Value> = Vec::new();
        let args = forward_boundary_args(&self.shared.mem.heap, args, &mut fwd_buf);
        invoke_special_bytecode_only_shared(
            self.shared,
            self.thread,
            class_name,
            method_name,
            descriptor,
            args,
        )
    }

    fn invoke_special_bytecode_only_by_class_id(
        &mut self,
        class_id: ClassId,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        let mut fwd_buf: Vec<Value> = Vec::new();
        let args = forward_boundary_args(&self.shared.mem.heap, args, &mut fwd_buf);
        invoke_special_bytecode_only_shared_on_class(
            self.shared,
            self.thread,
            class_id,
            class_name,
            method_name,
            descriptor,
            args,
        )
    }

    fn invoke_special_bytecode_only_hinted(
        &mut self,
        class_id: ClassId,
        method_hint: &mut Option<u32>,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        let mut fwd_buf: Vec<Value> = Vec::new();
        let args = forward_boundary_args(&self.shared.mem.heap, args, &mut fwd_buf);
        invoke_special_bytecode_only_shared_hinted(
            self.shared,
            self.thread,
            class_id,
            method_hint,
            class_name,
            method_name,
            descriptor,
            args,
        )
    }

    // -- LinkResolver wiring (round-9 HIGH #7) -------------------------
    //
    // Probe / insert into the per-VM `LinkResolver` cache for Java-side
    // reflection natives. Mirrors the JNI `GetMethodID` / `GetFieldID`
    // wiring (round-8 CRIT #2) so Spring / Hibernate / ByteBuddy
    // identical-triple probes through `Class.getDeclaredMethod` etc.
    // hit the same cache as their JNI counterparts.

    fn link_resolver_get_method(
        &self,
        class_id: ClassId,
        name: &str,
        descriptor: &str,
    ) -> Option<(ClassId, u32)> {
        use cratonvm_classloading::resolution::ResolvedMember;
        match self
            .shared
            .classes
            .link_resolver
            .get(class_id, name, descriptor)?
        {
            ResolvedMember::Method {
                declaring_class_id,
                index,
            } => Some((declaring_class_id, index)),
            // A cached `NotFound` is a legitimate hit — return `None` so
            // the caller short-circuits without re-walking the hierarchy.
            // The caller distinguishes "cache cold miss" from "cache hit:
            // not found" by checking `link_resolver_get_method` once and
            // proceeding to the metadata walk only if `None` came back
            // (the walk re-confirms NotFound and re-inserts; the cost is
            // a single redundant cold-miss walk in the rare case the
            // class genuinely lacks the member).
            ResolvedMember::NotFound => None,
            // Method probe matched a Field entry — should never happen
            // for properly-keyed lookups but degrade safely.
            ResolvedMember::Field { .. } => None,
        }
    }

    fn link_resolver_insert_method(
        &self,
        class_id: ClassId,
        name: &str,
        descriptor: &str,
        declaring: ClassId,
        index: u32,
    ) {
        use cratonvm_classloading::resolution::ResolvedMember;
        self.shared.classes.link_resolver.insert(
            class_id,
            cratonvm_types::intern_arc(name),
            cratonvm_types::intern_arc(descriptor),
            ResolvedMember::Method {
                declaring_class_id: declaring,
                index,
            },
        );
    }

    fn link_resolver_get_field(
        &self,
        class_id: ClassId,
        name: &str,
        descriptor: &str,
    ) -> Option<(ClassId, u32, bool)> {
        use cratonvm_classloading::resolution::ResolvedMember;
        match self
            .shared
            .classes
            .link_resolver
            .get(class_id, name, descriptor)?
        {
            ResolvedMember::Field {
                declaring_class_id,
                absolute_index,
                is_static,
            } => Some((declaring_class_id, absolute_index, is_static)),
            ResolvedMember::NotFound => None,
            ResolvedMember::Method { .. } => None,
        }
    }

    fn link_resolver_insert_field(
        &self,
        class_id: ClassId,
        name: &str,
        descriptor: &str,
        declaring: ClassId,
        absolute_index: u32,
        is_static: bool,
    ) {
        use cratonvm_classloading::resolution::ResolvedMember;
        self.shared.classes.link_resolver.insert(
            class_id,
            cratonvm_types::intern_arc(name),
            cratonvm_types::intern_arc(descriptor),
            ResolvedMember::Field {
                declaring_class_id: declaring,
                absolute_index,
                is_static,
            },
        );
    }

    fn invoke_virtual(
        &mut self,
        receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        // Arguments as well as the receiver: see `forward_boundary_args`. This
        // is the entry point the H2 natives use, and the one whose unforwarded
        // `session` argument is the MVStore-writer page's headline verdict.
        let mut fwd_buf: Vec<Value> = Vec::new();
        let args = forward_boundary_args(&self.shared.mem.heap, args, &mut fwd_buf);
        let mut receiver = self.shared.mem.heap.load_and_forward(receiver);
        let mut receiver_class_id = self.shared.mem.heap.class_id_of(receiver);
        if let Some(recovered) = recover_stale_lambda_receiver_from_native_pins(
            self.shared,
            &*self.thread,
            receiver_class_id,
            method_name,
            descriptor,
            args,
        ) {
            receiver = recovered;
            receiver_class_id = self.shared.mem.heap.class_id_of(receiver);
        }
        // Check if the receiver is a lambda proxy.
        let call_site = self.shared.classes.lambda_call_site_for(receiver_class_id);
        if crate::runtime::env_cache::invoke_virtual_entry_trace()
            && method_name == "aotContributedInitializerStartsManagementContext"
        {
            eprintln!(
                "[INVOKE-VIRTUAL-ENTRY-TRACE] method={} receiver_class_id={:?} is_lambda_proxy={}",
                method_name,
                receiver_class_id,
                call_site.is_some()
            );
        }

        // A serializable lambda proxy's spun `writeReplace()` (interpreter
        // round i2 wave 61, lane L4): its `SerializedLambda`, the answer
        // `Method.invoke` already gave. A method handle on the member (the
        // JDK's reflective accessor, `CRATONVM_JDK_REFLECTIVE_INVOKE`) calls
        // it virtually, and the NoSuchMethodError rescue below answered the
        // receiver itself. One branch for any other receiver.
        let serial_hook = matches!(&call_site, Some(lcs) if lcs.serializable_flag)
            && args.is_empty()
            && method_name == "writeReplace";
        if serial_hook {
            if let Some(result) =
                lambda_proxy_write_replace_call(self, receiver, method_name, descriptor)
            {
                return result;
            }
        }
        // Interpreter round i4 wave 86 (lane L4): an ACCIDENTALLY serializable
        // proxy's hostile `writeObject` / `readObject` throw
        // `NotSerializableException("Non-serializable lambda")`. Reached here
        // when `ObjectStreamClass.invokeWriteObject`'s `Method.invoke` runs as
        // the JDK's reflective accessor (a method handle calling the member
        // virtually); the serialization hooks' neutral result below answered a
        // no-op, so the stream wrote the lambda (`L4W86LambdaSerializationShapes`
        // row `accidental: write=`).
        if call_site.is_some()
            && !call_site.as_ref().is_some_and(|lcs| lcs.serializable_flag)
            && args.len() == 1
            && (method_name == "writeObject" || method_name == "readObject")
        {
            if let Some(failure) =
                cratonvm_native_builtins::lang_class::lambda_proxy_hostile_serialization_call(
                    self,
                    receiver,
                    method_name,
                    descriptor,
                )
            {
                return Err(failure);
            }
        }

        // Keep the receiver and arguments rooted across the dispatch decision:
        // the selected lambda body can allocate immediately after this block.
        // Only a lambda-proxy receiver runs anything in between (the
        // SAM-compatibility predicate); an ordinary receiver neither pins nor
        // allocates the handle vector (wave 15).
        let is_lambda_proxy = call_site.is_some();
        let sam_compat_pin_base = self.thread.native_pin_roots.len();
        let arg_pins: Vec<Option<usize>> = if is_lambda_proxy {
            self.thread.native_pin_roots.push(receiver);
            args.iter()
                .map(|a| match a {
                    Value::Object(Some(o)) => {
                        let idx = self.thread.native_pin_roots.len();
                        self.thread.native_pin_roots.push(*o);
                        Some(idx)
                    }
                    _ => None,
                })
                .collect()
        } else {
            Vec::new()
        };

        if let Some(lcs) = call_site.filter(|lcs| {
            // A lambda only implements its exact SAM descriptor (plus any
            // `FLAG_BRIDGES` bridge). Same-named defaults must run their
            // bytecode, even when a null argument is assignable to both the
            // default and SAM parameter types. The one shared predicate.
            crate::runtime::interpreter::lambda_accepts_descriptor(
                self.shared,
                receiver_class_id,
                lcs,
                method_name,
                descriptor,
            )
        }) {
            // Re-read receiver and args through the pins before the selected
            // lambda body can allocate and move them.
            receiver = self.thread.native_pin_roots[sam_compat_pin_base];
            let refreshed_args: Vec<Value> = args
                .iter()
                .zip(arg_pins.iter())
                .map(|(orig, pin)| match pin {
                    Some(idx) => Value::Object(Some(self.thread.native_pin_roots[*idx])),
                    None => *orig,
                })
                .collect();
            self.thread.native_pin_roots.truncate(sam_compat_pin_base);

            // Every implementation kind is served by the interpreter's
            // dispatcher, `try_lambda_dispatch` — the one copy bytecode, the
            // JIT and `invoke_on_class_shared_inner` use (wave 16 stage 1 took
            // `REF_invokeSpecial` and the field kinds; wave 17 the rest, and
            // deleted this function's own per-kind copy). Nothing above has
            // collected since the re-read, so `receiver` and `refreshed_args`
            // are current.
            native_callback_lambda_via_interpreter(
                self.shared,
                self.thread,
                receiver,
                receiver_class_id,
                &lcs,
                method_name,
                descriptor,
                &refreshed_args,
            )
        } else {
            if crate::runtime::env_cache::invoke_virtual_entry_trace()
                && method_name == "aotContributedInitializerStartsManagementContext"
            {
                eprintln!(
                    "[INVOKE-VIRTUAL-ENTRY-TRACE] method={} entered NOT-LAMBDA else branch",
                    method_name
                );
            }
            // Not a lambda-dispatch call after all (the receiver wasn't a
            // recognized proxy, or the `.filter()` predicate above rejected
            // it) -- release the pins from the GC-safety block above. Refresh
            // `receiver` through its pin in case the SAM-compatibility check
            // ran (and triggered a GC) before the predicate rejected the
            // match; `args` itself is not rebuilt here for the (rare, only
            // reachable when a lambda receiver's SAM-shaped overload check
            // fails) non-lambda fallback dispatch on a lambda-proxy receiver
            // -- see the GC-safety comment above this block's `if`.
            if is_lambda_proxy {
                receiver = self.thread.native_pin_roots[sam_compat_pin_base];
            }
            self.thread.native_pin_roots.truncate(sam_compat_pin_base);

            // THE RESOLVED-HANDLE FAST PATH for a native->Java callback.
            //
            // Everything from here to `invoke_or_native` resolves the callee
            // BY NAME, on every call: a class-manager read plus a `String`
            // allocation for the receiver's class name, a `find_with_kind`
            // triple hash against ~3 100 native slots, the cold
            // descriptor-quirk rewrite on the miss, two more class-manager
            // reads inside `invoke_or_native`, and then the whole by-name
            // resolution again in `invoke_on_class_shared_inner`. This is
            // ~2 500 stubs' calling convention, and
            // `completablefuture-composition-is-20x-and-5-percent-compiled-CLOSED-20260902.md`
            // §3 measured removing it at ONE call site at 1.154x of a whole
            // benchmark.
            //
            // A hit means this exact (VM, receiver class, method, descriptor)
            // last reached the ordinary bytecode tail at the bottom of
            // `invoke_on_class_shared_inner`, on this declaring class, and
            // nothing that could change that has happened since — see
            // `native_callee_memo`'s invalidation table. The replay is
            // `interpreter::execute` on that class, which is exactly what the
            // tail does and exactly what `invoke_virtual_bytecode_only` (the
            // hand-written form of this fix) does.
            //
            // Two things are refused before the probe. An ARRAY receiver
            // stores its COMPONENT class id in the header, so its id would
            // redeem a plain receiver's entry and run the component's
            // override (the `KC26 array.clone()` bug, restated below). A
            // REDEFINED process (an agent's woven bytecode competing with
            // registered natives) used to be the third, for the rest of the
            // process; since interpreter round i1 wave 18 the memo's stamp
            // carries the redefinition count instead
            // (`native_callee_memo::SURVIVES_REDEFINITION`, kill switch). A
            // lambda-proxy id cannot appear: those come
            // from the reserved `LAMBDA_PROXY_ID_BASE` range and `ClassId`s
            // are never reused, so an id that was an ordinary class when the
            // memo was filled is still one.
            let memo_eligible = crate::runtime::env_cache::native_callback_memo()
                && self.shared.mem.heap.kind_of(receiver) != cratonvm_types::ObjectKind::Array
                && (crate::runtime::native_callee_memo::SURVIVES_REDEFINITION
                    || !crate::classloading::any_class_redefined());
            if memo_eligible {
                if let Some(declaring) = crate::runtime::native_callee_memo::lookup(
                    self.shared,
                    receiver_class_id,
                    method_name,
                    descriptor,
                ) {
                    // JIT round 23 wave 2e (proxy proposals item 1): on the
                    // stack, not the process heap, for the common arities.
                    let mut full_args: InvokeVirtualArgs = smallvec::SmallVec::new();
                    full_args.push(Value::Object(Some(receiver)));
                    full_args.extend_from_slice(args);
                    return crate::runtime::interpreter::execute(
                        self.shared,
                        self.thread,
                        declaring,
                        method_name,
                        descriptor,
                        &full_args,
                    );
                }
            }

            // Not a lambda proxy SAM call вЂ” normal virtual dispatch.
            // If the receiver IS a lambda proxy but calling a non-SAM method
            // (e.g. andThen), dispatch on the functional interface class.
            //
            // KC26 array.clone() bug: array objects store their COMPONENT class
            // id (e.g. `OptionCategory`) in the header — NOT the array class
            // id. Calling `get_class(receiver_class_id).name` for an array
            // receiver therefore returns the component class name. Routing
            // dispatch through the component then resolves `clone()` to the
            // *component's* override (`Enum.clone()` for enum arrays — which
            // is the JDK's deliberate CNSE-thrower) instead of `Object.clone`
            // (the array-cloning native). Per JVMS §4.4.1, every array class's
            // method table is `Object`'s — short-circuit array receivers to
            // `java/lang/Object` here, matching the parallel logic in
            // `invoke_or_native` and `try_stackless_invoke`.
            // `resolved_from_receiver` tracks whether `class_name` is just the
            // receiver's own class name (the plain-object case). When it is, the
            // final dispatch keys on `receiver_class_id` DIRECTLY rather than
            // re-resolving the name — otherwise a class a custom loader defined
            // (load-time weaving, webapp isolation) collapses to the same-named
            // class some other loader registered first, so a reflective
            // `Method.invoke` on the loader-private instance runs the wrong
            // body (JVMS §5.3). Arrays (dispatch on Object) and lambda-proxy
            // non-SAM calls (dispatch on the functional interface) must keep the
            // name-based path, so the flag is false there.
            let mut resolved_from_receiver = false;
            // JIT round 23 wave 2e (proxy proposals item 1): the class name is
            // the class's own `Arc<str>` (a refcount bump), not a fresh
            // `String` per callback; every use below reads it as `&str`.
            let class_name: InvokeVirtualClassName =
                if self.shared.mem.heap.kind_of(receiver) == cratonvm_types::ObjectKind::Array {
                    InvokeVirtualClassName::Static("java/lang/Object")
                } else {
                    // Only a lambda-proxy receiver has an entry: an ordinary
                    // one skips the `lambda_proxies` read lock (wave 15).
                    let lambda_iface = if is_lambda_proxy {
                        let proxies = self.shared.classes.lambda_proxies.read();
                        proxies
                            .get(&receiver_class_id)
                            .map(|lcs| lcs.functional_interface.clone())
                    } else {
                        None
                    };
                    match lambda_iface {
                        Some(iface) => InvokeVirtualClassName::Shared(iface),
                        None => {
                            let by_id = self
                                .shared
                                .classes
                                .class_manager
                                .read()
                                .get_class(receiver_class_id)
                                .map(|c| c.name.clone());
                            match by_id {
                                Some(n) => {
                                    resolved_from_receiver = true;
                                    InvokeVirtualClassName::Shared(n)
                                }
                                None => InvokeVirtualClassName::Owned(format!(
                                    "<unknown class {}>",
                                    receiver_class_id
                                )),
                            }
                        }
                    }
                };
            let class_name: &str = &class_name;

            // Check for java.lang.reflect.Proxy dynamic proxy dispatch.
            // When Java code calls any method on a Proxy$Instance object, we
            // intercept and forward to the InvocationHandler.invoke().
            // Round 13 wave 4 (lane proxy2): a generated `$ProxyN` that declares
            // the method runs its body, as both interpreter and compiled callers
            // do (`proxy_call_takes_ordinary_dispatch` reads the switches).
            // An array is never a proxy: its header carries the COMPONENT's id
            // (SBR2 W9 SMALL2, `invoke.rs` twin).
            if (class_name == "java/lang/reflect/Proxy$Instance"
                || (self.shared.mem.heap.kind_of(receiver) != cratonvm_types::ObjectKind::Array
                    && crate::runtime::interpreter::class_chain_reaches_proxy_instance(
                        self.shared,
                        receiver_class_id,
                    )))
                && !crate::runtime::interpreter::proxy_call_takes_ordinary_dispatch(
                    self.shared,
                    &[Value::Object(Some(receiver))],
                    method_name,
                    descriptor,
                )
            {
                return proxy_invoke_handler(self, receiver, method_name, descriptor, args);
            }

            // Check for annotation proxy dispatch.
            // When Java code calls annotation.value(), annotation.path(), etc.,
            // look up the element by method name in the proxy's stored elements.
            if class_name == "java/lang/annotation/AnnotationProxy" {
                let result = annotation_proxy_invoke(self, receiver, method_name, args)?;
                // Unbox primitive-returning members so `invoke_virtual`'s
                // contract matches bytecode methods (which yield a raw
                // primitive Value for a primitive return descriptor). The
                // proxy stores members as BOXED wrappers; returning them boxed
                // makes a re-entrant caller like `Method.invoke`'s `box_value`
                // DOUBLE-box — it stores the wrapper reference in a fresh
                // wrapper's value slot, so a later `intValue()` reads the
                // pointer (a positive int) instead of the real value. That is
                // exactly how ByteBuddy's `JavaDispatcher`-driven read of
                // `@Advice.OnMethodEnter.skipOnIndex()` (declared `default -1`)
                // saw a bogus `>= 0` index, throwing "void is not an array
                // type but an index for a relocation is defined" and failing
                // Hibernate's BytecodeProvider service-load.
                if let Some(value) = result {
                    let ret = descriptor
                        .rsplit(')')
                        .next()
                        .unwrap_or("L")
                        .chars()
                        .next()
                        .unwrap_or('L');
                    let unboxed = match ret {
                        'I' | 'Z' | 'B' | 'C' | 'S' | 'J' | 'F' | 'D' => {
                            if let Value::Object(Some(obj)) = value {
                                self.shared.mem.heap.get_field(obj, 0)
                            } else {
                                value
                            }
                        }
                        _ => value,
                    };
                    return Ok(Some(unboxed));
                }
                return Ok(None);
            }

            // `Class.forName(name, ..., loader)` invokes `loadClass(String)`
            // through this NativeContext path.  For a subclass that merely
            // inherits ClassLoader's implementation, virtual resolution must
            // execute CratonVM's base ClassLoader native (which performs
            // parent-first delegation and loader-local lookup), not the
            // real-JDK bytecode/global-resolution fallback. Preserve genuine
            // subclass overrides by checking the method's actual declarer.
            //
            // Spring's `DynamicClassLoader` is such an inheriting subclass;
            // its generated classes deliberately live in the forked parent.
            // The normal receiver resolver can retain the inherited JDK body
            // before the base native gate sees it, collapsing this lookup to
            // the global same-named class. Route this known inheriting loader
            // directly through the base native to preserve parent-first
            // fork-loader identity.
            if self.shared.natives.native_methods.has_app_workaround(
                cratonvm_native_api::AppWorkaroundKind::InheritBaseClassLoaderLoadClass,
                &class_name,
                method_name,
                descriptor,
            ) {
                let mut full_args: InvokeVirtualArgs = smallvec::SmallVec::new();
                full_args.push(Value::Object(Some(receiver)));
                full_args.extend_from_slice(args);
                return self.invoke_or_native(
                    "java/lang/ClassLoader",
                    method_name,
                    descriptor,
                    &full_args,
                );
            }
            if method_name == "loadClass"
                && descriptor == "(Ljava/lang/String;)Ljava/lang/Class;"
                && resolved_from_receiver
            {
                // JIT round 23 wave 2 page review (`prx23`): the receiver's
                // OWN method list was the whole test, so a loader whose
                // application superclass overrides `loadClass(String)` (B
                // extends A extends ClassLoader, A overrides) ran the base
                // native and skipped A's override. HotSpot selects A's body.
                // `load_class_uses_base_native` reads the chain.
                let use_base_loader_native = {
                    let cm = self.shared.classes.class_manager.read();
                    load_class_uses_base_native(
                        std::iter::successors(cm.get_class(receiver_class_id), |class| {
                            class.superclass.and_then(|sid| cm.get_class(sid))
                        })
                        .take(LOAD_CLASS_CHAIN_LIMIT)
                        .map(|class| LoadClassLink {
                            is_class_loader: class.name.as_ref() == "java/lang/ClassLoader",
                            declares: class.methods.iter().any(|method| {
                                method.name.as_ref() == method_name
                                    && method.descriptor.as_ref() == descriptor
                            }),
                            application_bytes: matches!(
                                class.origin,
                                cratonvm_classloading::ClassOrigin::ApplicationClassPath { .. }
                                    | cratonvm_classloading::ClassOrigin::UserDefined { .. }
                            ),
                        }),
                    )
                };
                if use_base_loader_native {
                    let mut full_args: InvokeVirtualArgs = smallvec::SmallVec::new();
                    full_args.push(Value::Object(Some(receiver)));
                    full_args.extend_from_slice(args);
                    return self.invoke_or_native(
                        "java/lang/ClassLoader",
                        method_name,
                        descriptor,
                        &full_args,
                    );
                }
            }

            // Prepend receiver to args.
            let mut full_args: InvokeVirtualArgs = smallvec::SmallVec::new();
            full_args.push(Value::Object(Some(receiver)));
            full_args.extend_from_slice(args);

            // `invoke_on_class_shared` dispatches on `receiver_class_id`
            // directly rather than re-resolving `class_name` through the
            // global class map — it preserves native-override precedence
            // while resolving the actual receiver hierarchy, which matters
            // for a method-local anonymous class (the global map's
            // name->id lookup can collapse it onto an unrelated same-named
            // class registered by another loader, landing on an inherited
            // Object member instead of the receiver's concrete override —
            // notably `toString()` reached from a native call such as
            // String.format's `%s`).
            //
            // That path does more locking than `invoke_or_native` and is
            // unsafe to make the default for every `resolved_from_receiver`
            // call: routing ALL ordinary virtual dispatch through it
            // (rather than gating it to cases that actually need exact-
            // class resolution) reintroduced a startup hang — Spring
            // Boot's `BackgroundPreinitializingApplicationListener` runs
            // Hibernate Validator's reflection-heavy constraint-helper
            // warmup concurrently with the main thread's own bean/class
            // initialization, and making this heavier path the hot path
            // for every virtual call from both threads deadlocked them
            // (see embedded-tomcat-loopback-self-connect-silent-hang-FIXED.md).
            // Keep it scoped to the two cases that actually need it: the
            // original loader-identity divergence this mechanism was built
            // for, and the specific anonymous-`toString()` shape the
            // regression test (`vm/tests/string_format_throwing_tostring.rs`)
            // covers.
            let needs_exact_class_dispatch = resolved_from_receiver
                && (method_name == "toString" && descriptor == "()Ljava/lang/String;"
                    || self
                        .shared
                        .classes
                        .class_manager
                        .read()
                        .get_loaded_class_id(&class_name)
                        != Some(receiver_class_id));
            // Name first, then the (now cached) gate: the name compare fails
            // on its length for every other method, so the trace costs one
            // `usize` compare on the path every native->Java callback takes.
            // `CRATONVM_JIT_HOT_LOOKUP_CACHE=0` restores the original order and
            // the uncached read.
            if if crate::runtime::env_cache::hot_lookup_cache() {
                method_name == "aotContributedInitializerStartsManagementContext"
                    && crate::runtime::env_cache::needs_exact_trace()
            } else {
                None::<std::ffi::OsString>.is_some()
                    && method_name == "aotContributedInitializerStartsManagementContext"
            } {
                let global_id = self
                    .shared
                    .classes
                    .class_manager
                    .read()
                    .get_loaded_class_id(&class_name);
                eprintln!(
                    "[NEEDS-EXACT-TRACE] method={} class_name={} resolved_from_receiver={} receiver_class_id={:?} global_lookup_id={:?} needs_exact_class_dispatch={}",
                    method_name, class_name, resolved_from_receiver, receiver_class_id, global_id, needs_exact_class_dispatch
                );
            }
            // Arm the memo's witness across the dispatch. It is NOT a
            // prediction: the fill below happens only if the dispatch
            // actually arrives at `invoke_on_class_shared_inner`'s ordinary
            // bytecode tail, which every special arm returns before reaching.
            // Both routes below end at that same tail with the same declaring
            // class, so both are memoisable.
            //
            // `arms_poly_call_site` is the one thing the tail cannot see: a
            // signature-polymorphic `invoke`/`invokeExact` needs
            // `invoke_or_native` to publish the CALL-SITE descriptor before
            // dispatching, and the fast path does not call it. Refused here
            // rather than at the tail because this is where the class name
            // that predicate reads is in hand.
            let memo_frame_depth = self.thread.frames.len();
            let memo_save = (memo_eligible
                && resolved_from_receiver
                && !arms_poly_call_site(&class_name, method_name))
            .then(|| {
                crate::runtime::native_callee_memo::arm(
                    receiver_class_id,
                    method_name,
                    descriptor,
                    memo_frame_depth,
                )
            });
            // Wave 60, lane L3: the receiver's class, read before the call (a
            // collection may move the receiver); an array selects from
            // `Object`.
            let door = if self.shared.mem.heap.kind_of(receiver) == ObjectKind::Object {
                DoorOwner::Id(receiver_class_id)
            } else {
                DoorOwner::Name("java/lang/Object")
            };
            let before = served_raise_mark_addr(self.thread);
            let result = if needs_exact_class_dispatch {
                invoke_on_class_shared(
                    self.shared,
                    self.thread,
                    receiver_class_id,
                    method_name,
                    descriptor,
                    &full_args,
                )
            } else {
                self.invoke_or_native(&class_name, method_name, descriptor, &full_args)
            };
            let result = door_result_noted(
                self.shared,
                self.thread,
                before,
                door,
                method_name,
                descriptor,
                result,
            );
            if let Some(save) = memo_save {
                if let Some(declaring) = crate::runtime::native_callee_memo::disarm(save) {
                    crate::runtime::native_callee_memo::fill(
                        self.shared,
                        receiver_class_id,
                        method_name,
                        descriptor,
                        declaring,
                    );
                }
            }
            result
        }
    }

    fn virtual_selection_failure(
        &mut self,
        receiver: ObjectRef,
        owner: ClassId,
        method_name: &str,
        descriptor: &str,
    ) -> Option<MethodCallFailed> {
        let receiver_class = self.class_id_of_object(receiver);
        reflective_selection_failure(
            self.shared,
            self.thread,
            receiver_class,
            owner,
            method_name,
            descriptor,
        )
    }

    fn invoke_virtual_declared(
        &mut self,
        declared_class: &str,
        receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        self.invoke_virtual_declared_selecting(
            declared_class,
            None,
            receiver,
            method_name,
            descriptor,
            args,
            true,
        )
    }

    // Interpreter round i6 wave 118, lane L4 (`CRATONVM_MH_VIRTUAL_BY_OWNER_ID`;
    // the `VIRTUAL` half of `i63-L4-proposal-virtual-door-dispatches-by-owner-id`):
    // the §5.4.6 selection finds the declared class on the receiver's chain by
    // the handle's owner id. `declared_class` stays the NoSuchMethodError
    // retry's name.
    fn invoke_virtual_declared_by_class_id(
        &mut self,
        owner: ClassId,
        declared_class: &str,
        receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        self.invoke_virtual_declared_selecting(
            declared_class,
            Some(owner),
            receiver,
            method_name,
            descriptor,
            args,
            true,
        )
    }

    fn invoke_virtual_exact(
        &mut self,
        declared_class: &str,
        receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        // Interpreter round i3 wave 77, lane L3 (`CRATONVM_REFLECT_INVOKE_PLAN_FACTS`):
        // the caller established the receiver is a plain object of the
        // declared class itself, for which `declared_dispatch_selection`
        // answers `None` at its name compare -- skip it.
        self.invoke_virtual_declared_selecting(
            declared_class,
            None,
            receiver,
            method_name,
            descriptor,
            args,
            false,
        )
    }

    fn invoke_virtual_bytecode_only(
        &mut self,
        receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        // Call straight into `interpreter::execute` — the actual "just run
        // this bytecode, no native check" primitive that
        // `invoke_on_class_shared_inner` itself falls back to once it has
        // decided native doesn't apply. `invoke_on_class_shared` is NOT
        // sufficient here: besides its primary `check_override` gate (which
        // IS skipped for an ordinary concrete method like
        // `ThreadPoolExecutor.execute`/`submit`/`shutdown`), it has a
        // SECOND, unconditional native-registry check for any non-interface
        // declaring class (`override_cb` in `invoke_on_class_shared_inner`,
        // vm_exec.rs) that re-finds this exact native regardless of the
        // first gate — routing through `invoke_on_class_shared` reintroduced
        // infinite recursion (confirmed via a depth-counter probe: `execute`
        // called itself on the same receiver until the native stack
        // overflowed) instead of actually reaching bytecode.
        let mut fwd_buf: Vec<Value> = Vec::new();
        let args = forward_boundary_args(&self.shared.mem.heap, args, &mut fwd_buf);
        let receiver = self.shared.mem.heap.load_and_forward(receiver);
        let class_id = self.shared.mem.heap.class_id_of(receiver);
        let declaring_class_id = {
            let cm = self.shared.classes.class_manager.read();
            crate::classloading::find_method_recursive(
                class_id,
                method_name,
                descriptor,
                &cm.class_store,
            )
            .map(|(_, declaring_id)| declaring_id)
            .unwrap_or(class_id)
        };
        let mut full_args = Vec::with_capacity(1 + args.len());
        full_args.push(Value::Object(Some(receiver)));
        full_args.extend_from_slice(args);
        // JIT round 21 lane reflect4 (REFLECT4-21-1,
        // `CRATONVM_JIT_BYTECODE_ONLY_KEY`): `execute`, or the door-invisible
        // compiled body of a reviewed `Intrinsic`'s own JDK body.
        crate::runtime::interpreter::bytecode_only_body::execute_bytecode_only(
            self.shared,
            self.thread,
            declaring_class_id,
            method_name,
            descriptor,
            &full_args,
        )
    }
}

/// JVMS §5.5 / `new`: the class is initialized BEFORE the instance exists.
/// `new_object_initialized{,_with_class_id}` allocated the object (and, for a
/// class with a non-trivial `finalize`, registered it for finalization through
/// `init_new_instance`) and only then ran `<clinit>`: when that threw, an
/// instance of the now-erroneous class was already registered and its
/// `finalize()` later ran on an object whose constructor never ran; HotSpot
/// (`InterpreterRuntime::_new`, `Reflection::invoke_constructor`, JNI
/// `NewObject`) initializes first and allocates nothing. Round 15 wave 4,
/// lane jdkonly5 (R5 of `r15w4-jdkonly4-builder-review-findings-CLOSED-20260930.md`).
///
/// The steady state (class already initialized) is the manager's atomic
/// probe and never reads the switch. `CRATONVM_NATIVE_NEW_INIT_FIRST=0`
/// (`VmConfig::native_new_init_first`, read once per VM) restores
/// allocate-then-initialize; the callers keep their own
/// `ensure_class_initialized_shared` after the allocation, which is then the
/// fast no-op when this ran.
pub(in crate::vm::vm_exec) fn native_new_initializes_class_first(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
) -> Result<(), MethodCallFailed> {
    if super::is_class_initialized_via_manager(shared, class_id)
    {
        return Ok(());
    }
    super::ensure_class_initialized_shared(shared, thread, class_id)
}

/// JVMS §5.5 for the constructor-less native allocations
/// (`NativeHeapAccess::instantiate`: `Unsafe.allocateInstance`, the
/// serialization constructor; JNI `AllocObject` has its own twin in
/// `native/jni.rs`): HotSpot's `InstanceKlass::allocate_instance(java_class)`
/// runs `initialize` before it allocates, so a `<clinit>` side effect is seen
/// by the first allocation and a failing `<clinit>` is the answer
/// (`ExceptionInInitializerError`, then `NoClassDefFoundError`). These paths
/// never initialized at all. Round 15 wave 4, lane jdkonly6
/// (`r15w4-jdkonly5-native-allocation-skips-class-init-FIXED-20260930.md`).
///
/// The steady state (class already initialized) is the manager's atomic
/// probe and never reads the switch. `CRATONVM_NATIVE_ALLOC_INITIALIZES=0`
/// (`VmConfig::native_alloc_initializes`, read once per VM) restores
/// allocation without initialization.
pub(in crate::vm::vm_exec) fn native_alloc_initializes_class_first(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
) -> Result<(), MethodCallFailed> {
    if super::is_class_initialized_via_manager(shared, class_id)
    {
        return Ok(());
    }
    super::ensure_class_initialized_shared(shared, thread, class_id)
}

impl NativeContextImpl<'_> {
    /// [`NativeInvokeAccess::invoke_virtual_declared`]'s body; `select`:
    /// ask `declared_dispatch_selection` (the declared door), or not (the
    /// exact door, `NativeInvokeAccess::invoke_virtual_exact`: a receiver of
    /// the declared class itself, for which it answers `None`). Interpreter
    /// round i3 wave 77, lane L3.
    pub(in crate::vm::vm_exec) fn invoke_virtual_declared_selecting(
        &mut self,
        declared_class: &str,
        declared_id: Option<ClassId>,
        receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
        select: bool,
    ) -> MethodCallResult {
        // gen r4w4/rooting2 (residual item 5): the first dispatch can run Java
        // before it fails with the NoSuchMethodError the retry below keys on
        // (a `<clinit>`, a user loader resolving the receiver's class), and the
        // retry used to rebuild its argument vector from the caller's
        // pre-dispatch copies. Pin the receiver and every object argument for
        // the duration, and rebuild from the pins. The pins are pushed in
        // order onto this thread's stack (`pin_native_root` is a push that
        // returns the index), so the k-th object argument sits at
        // `pin_base + 1 + k` and no handle vector is needed on this path.
        let pin_base = self.pin_native_root(receiver);
        for v in args {
            if let Value::Object(Some(o)) = v {
                self.pin_native_root(*o);
            }
        }
        // Round 13 wave 1 (lane mhffm): the receiver-class walk below runs the
        // first same-signature method from the receiver down, whether or not
        // it overrides the declared one (§5.4.5: a package-private method is
        // overridden only from its own run-time package). Where §5.4.6
        // selection against the declared method picks another class, run that
        // class's method exactly (`r12w8-hunter5-mh-and-reflection-virtual-
        // dispatch-ignore-the-override-rule`).
        // Interpreter round i3 wave 77, lane L3: `invoke_virtual_exact` skips the
        // question for a receiver of the declared class itself (`None` there).
        let selection = if select {
            declared_dispatch_selection(
                self.shared,
                declared_class,
                declared_id,
                receiver,
                method_name,
                descriptor,
            )
        } else {
            None
        };
        let result = match selection {
            Some(selected) => {
                let mut fwd_buf: Vec<Value> = Vec::new();
                let args = forward_boundary_args(&self.shared.mem.heap, args, &mut fwd_buf);
                let mut full_args = Vec::with_capacity(1 + args.len());
                full_args.push(Value::Object(Some(
                    self.shared.mem.heap.load_and_forward(receiver),
                )));
                full_args.extend_from_slice(args);
                let before = served_raise_mark_addr(self.thread);
                let result = invoke_on_class_shared_no_retarget(
                    self.shared,
                    self.thread,
                    selected,
                    method_name,
                    descriptor,
                    &full_args,
                );
                door_result_noted(
                    self.shared,
                    self.thread,
                    before,
                    DoorOwner::Id(selected),
                    method_name,
                    descriptor,
                    result,
                )
            }
            None => self.invoke_virtual(receiver, method_name, descriptor, args),
        };
        let out = match &result {
            Err(MethodCallFailed::InternalError(VmError::Linkage(
                LinkageError::NoSuchMethodError {
                    class_name,
                    method_name: nsme_method,
                    method_descriptor,
                    ..
                },
            ))) if class_name == "java/lang/Object"
                && declared_class != "java/lang/Object"
                && nsme_method == method_name
                && method_descriptor == descriptor
                && !is_object_member(method_name, descriptor) =>
            {
                let mut full_args = Vec::with_capacity(1 + args.len());
                let receiver_now = self.read_native_pin(pin_base, receiver);
                full_args.push(Value::Object(Some(receiver_now)));
                let mut handle = pin_base + 1;
                for v in args {
                    match v {
                        Value::Object(Some(o)) => {
                            full_args.push(Value::Object(Some(self.read_native_pin(handle, *o))));
                            handle += 1;
                        }
                        other => full_args.push(*other),
                    }
                }
                self.invoke_or_native(declared_class, method_name, descriptor, &full_args)
            }
            _ => result,
        };
        self.unpin_native_roots(pin_base);
        out
    }
}

/// Interpreter round i3 wave 83, lane L3 (`CRATONVM_REFLECT_CTOR_LEAN_ALLOC`,
/// `VmConfig::reflect_ctor_lean_alloc`, default off; the census page
/// `interpreter-i81-L3-proposal-name-the-reflective-calls-gap-and-reentry-KEPT-OFF-20261019.md`,
/// "Progress (wave 83)"): the constructor door of a class already
/// initialized. The host census read a reflective no-arg construction's
/// `call_door` (the arguments' mark to `execute`'s entry) at ~270 ns against
/// ~67 for a static `Method.invoke`; the difference is this function's
/// allocation work. The same steps and order as
/// `new_object_initialized_with_class_id`, with:
///
/// * the defaults, the finalizer registration and the reference discovery
///   from the per-class allocation recipe (`jit_alloc_class_cache`, the one
///   the interpreter's `new` and the JIT's allocation helpers read:
///   `jit::helpers::jit_post_alloc_init`) instead of `init_new_instance`'s
///   class-manager read and superclass walk with a store per field;
/// * the `<init>` argument vector inline (`SmallVec`, no heap allocation up
///   to eight words) instead of a `Vec` per call.
///
/// The class-initialization tests stay (both are the bitmap's one load for
/// an initialized class); the caller takes this only when the bitmap already
/// says initialized. The allocation is the same fallible no-GC
/// `try_alloc_object_full` with the same external charge.
impl NativeContextImpl<'_> {
    #[inline(never)]
    pub(in crate::vm::vm_exec) fn new_object_initialized_lean(
        &mut self,
        class_id: ClassId,
        init_desc: &str,
        init_args: &[Value],
    ) -> MethodCallResult {
        let pin_base = self.thread.native_pin_roots.len();
        let arg_pins = pin_native_object_values(self.thread, init_args);
        let result = (|| {
            native_new_initializes_class_first(self.shared, self.thread, class_id)?;
            let num_fields = self
                .shared
                .classes
                .class_manager
                .read()
                .get_class(class_id)
                .map(|c| c.num_total_fields)
                .unwrap_or(0);
            let obj_ref = self
                .shared
                .mem
                .heap
                .try_alloc_object_full(class_id, num_fields)
                .ok_or_else(|| {
                    MethodCallFailed::InternalError(VmError::Runtime(
                        RuntimeError::OutOfMemoryError {
                            message: format!(
                                "Java heap space (new_object_initialized_with_class_id {} fields)",
                                num_fields
                            ),
                        },
                    ))
                })?;
            // Defaults + JLS §12.6 finalizer registration + the allocation's
            // reference discovery, from the recipe (its legacy fallback is
            // the locked walk `init_new_instance` makes).
            crate::jit::helpers::jit_post_alloc_init(self.shared, obj_ref, class_id);
            self.note_native_object_alloc(obj_ref);
            reflect_door_gate_stamp(
                self.shared,
                cratonvm_native_builtins::ReflectDoorGatePoint::DoorAllocated,
            );

            let obj_pin = self.thread.native_pin_roots.len();
            self.thread.native_pin_roots.push(obj_ref);
            super::ensure_class_initialized_shared(self.shared, self.thread, class_id)?;
            let obj_ref = self
                .thread
                .native_pin_roots
                .get(obj_pin)
                .copied()
                .unwrap_or(obj_ref);
            let mut full: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
            full.push(Value::Object(Some(obj_ref)));
            for (value, handle) in init_args.iter().zip(arg_pins.iter()) {
                full.push(match (value, handle) {
                    (Value::Object(Some(fallback)), Some(handle)) => Value::Object(Some(
                        self.thread
                            .native_pin_roots
                            .get(*handle)
                            .copied()
                            .unwrap_or(*fallback),
                    )),
                    _ => *value,
                });
            }

            let init_name: &str = "<init>";
            // JIT round 21 wave 10, lane finalize: `obj_ref` is current here.
            let epoch_before_init = self.shared.mem.address_epoch();
            let init_result = match crate::runtime::native_callee_memo::lookup(
                self.shared,
                class_id,
                init_name,
                init_desc,
            ) {
                Some(declaring) => crate::runtime::interpreter::execute(
                    self.shared,
                    self.thread,
                    declaring,
                    init_name,
                    init_desc,
                    &full,
                ),
                None => {
                    let memo_save = crate::runtime::native_callee_memo::arm(
                        class_id,
                        init_name,
                        init_desc,
                        self.thread.frames.len(),
                    );
                    let result = invoke_on_class_shared(
                        self.shared,
                        self.thread,
                        class_id,
                        init_name,
                        init_desc,
                        &full,
                    );
                    if let Some(declaring) = crate::runtime::native_callee_memo::disarm(memo_save) {
                        crate::runtime::native_callee_memo::fill(
                            self.shared,
                            class_id,
                            init_name,
                            init_desc,
                            declaring,
                        );
                    }
                    result
                }
            };

            let forwarded = self
                .thread
                .native_pin_roots
                .get(obj_pin)
                .copied()
                .unwrap_or(obj_ref);
            // JIT round 21 wave 10, lane finalize
            // (`CRATONVM_FINALIZER_REGISTER_AT_OBJECT_INIT`): the registration
            // made at the allocation above goes when the constructor threw
            // before `Object.<init>` returned (the unwinder's proof).
            if init_result.is_err() {
                crate::runtime::interpreter::withdraw_unconstructed_finalizable_at_door(
                    self.shared,
                    obj_ref,
                    epoch_before_init,
                );
            }
            init_result?;
            Ok(Some(Value::Object(Some(forwarded))))
        })();
        self.thread.native_pin_roots.truncate(pin_base);
        result
    }
}

