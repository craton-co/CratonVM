#[allow(unused_imports)]
use super::*;

impl<'a> NativeExceptionAccess for NativeContextImpl<'a> {
    /// zs s5/nd5: the `OutOfMemoryError` a caught `NativeAllocOom` stands for
    /// (`native_oom::NativeAllocOom::into_runtime_error`'s message). ZGC heap
    /// only (the round's scope): elsewhere `None`, so a caller resumes the
    /// unwind and nothing changes for Generational / G1.
    fn native_alloc_oom_failure(
        &self,
        payload: &(dyn std::any::Any + Send),
    ) -> Option<MethodCallFailed> {
        if !super::vm_object::heap_is_zgc(self.shared) {
            return None;
        }
        let oom = payload.downcast_ref::<crate::runtime::native_oom::NativeAllocOom>()?;
        Some(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::OutOfMemoryError {
                message: oom.java_message(),
            },
        )))
    }

    fn capture_stack_trace(&mut self) -> Vec<StackTraceEntry> {
        self.capture_current_stack_trace()
    }

    fn capture_stack_walk_trace(&mut self) -> Vec<StackTraceEntry> {
        self.capture_stack_walk_trace_with(false)
    }

    fn stack_walk_lean_capture(&self) -> bool {
        crate::runtime::stackwalker::stackwalk_lean_capture_enabled()
    }

    fn capture_stack_walk_trace_deferred(&mut self) -> Vec<StackTraceEntry> {
        self.capture_stack_walk_trace_with(true)
    }

    fn resolve_deferred_stack_walk_line(&self, entry: &mut StackTraceEntry) {
        if !matches!(
            entry.line_number,
            cratonvm_native_api::LINE_NUMBER_DEFERRED
                | cratonvm_native_api::LINE_NUMBER_DEFERRED_AT_BCI_ZERO
        ) {
            return;
        }
        let cm = self.shared.classes.class_manager.read();
        crate::runtime::stackwalker::resolve_deferred_walk_line(&cm, entry);
    }

    fn capture_stack_walk_trace_tail(&mut self, want: usize) -> Option<Vec<StackTraceEntry>> {
        // JIT round 21 wave 3, lane walk (WALK21-9 / WALK21-10): only for a
        // thread with nothing `capture_stack_walk_trace_with` splices in (a
        // reflective call's frames, a served `Thread.run`, a served-init
        // chain mark), and a window none of its pair rules can touch.
        if !self.stack_walk_capture_is_splice_free() {
            return None;
        }
        let tail = {
            let cm = self.shared.classes.class_manager.read();
            crate::runtime::stackwalker::capture_walk_tail(&cm.class_store, &self.thread.frames, want)?
        };
        crate::runtime::stackwalker::walk_tail_needs_no_pair_rule(&tail).then_some(tail)
    }

    fn stack_walk_anchor(&self) -> Option<(usize, u32)> {
        // WALK21-10: taken only while nothing would be spliced into the
        // walk's capture, so the anchored capture needs no splice either.
        if !self.stack_walk_capture_is_splice_free() {
            return None;
        }
        let chain = crate::jit::conservative_roots::current_thread_jit_depth();
        Some((self.thread.frames.len(), u32::try_from(chain).ok()?))
    }

    fn capture_stack_walk_trace_anchored(
        &mut self,
        anchor: (usize, u32),
    ) -> Option<Vec<StackTraceEntry>> {
        // The frames below the anchor are suspended under the walk's own
        // native call, so they cannot have gone (review rev-vmjit item 4,
        // `r21-review-vmjit-findings-20261019.md`); a stack shallower than the
        // anchor is refused rather than read.
        if self.thread.frames.len() < anchor.0
            || crate::jit::conservative_roots::current_thread_jit_depth() < anchor.1 as usize
        {
            return None;
        }
        let cm = self.shared.classes.class_manager.read();
        let mut trace = crate::runtime::stackwalker::capture_stack_walk_trace_below(
            &cm,
            &self.thread.frames,
            anchor.0,
            anchor.1,
        );
        // The pair rules of `capture_stack_walk_trace_with`, as it applies
        // them (the splices were empty when the anchor was taken).
        let virtual_thread = matches!(self.thread.kind, crate::threading::ThreadKind::Virtual);
        add_thread_run_walk_entries(
            self.shared,
            &cm,
            self.thread.java_thread_obj,
            &mut trace,
            virtual_thread,
        );
        insert_callback_native_walk_entries(&cm, &mut trace);
        Some(trace)
    }

    fn enter_reflective_call(&mut self, kind: cratonvm_native_api::ReflectiveCallKind) -> usize {
        // Interpreter round i1 wave 49, lane L3: a `doPrivileged` bridge's
        // frame only while the VM lists it (`CRATONVM_STANDIN_PRIVILEGED_FRAMES`);
        // otherwise the mark is the current length and the leave a no-op.
        if kind.is_privileged() && false {
            return self.thread.reflective_calls.len();
        }
        // Interpreter round i1 wave 43, lane L3: the call's JDK frames name its
        // accessor class, which the native never loads; load it once per
        // thread so a capture can name it (HotSpot loads it for the call).
        let bit = crate::runtime::stackwalker::reflective_kind_bit(kind);
        if self.thread.reflective_accessors_loaded & bit == 0 {
            self.thread.reflective_accessors_loaded |= bit;
            if let Some(accessor) = crate::runtime::stackwalker::reflective_accessor_class(kind) {
                let _ = self.shared.load_class_concurrent(accessor);
            }
            // JIT round 20, lane walkcompat: the stream head a served
            // `forEach` loop's frames name.
            if let Some(extra) = crate::runtime::stackwalker::stack_walk_extra_class(kind) {
                let _ = self.shared.load_class_concurrent(extra);
            }
        }
        // Wave 45, lane L3: name the call's JDK frames now (once per thread
        // and redefinition count), so the trace this thread PUBLISHES for
        // other threads -- which may not take the class-manager lock -- can
        // list them. `try_read`: a busy lock leaves them for a later call.
        let shared = self.shared;
        crate::runtime::stackwalker::name_reflective_frames_if_stale(
            &mut self.thread.reflective_frames_named,
            kind,
            || {
                let cm = shared.classes.class_manager.try_read()?;
                Some(crate::runtime::stackwalker::reflective_entries(
                    &cm.class_store,
                    kind,
                ))
            },
        );
        // JIT round 20 wave 7, lane revVM
        // (`jr20-published-traces-omit-served-stackwalker-frames-FIXED-20261012.md`):
        // a served walk has no slot in the thread's memo; its frames go to the
        // VM's, which the published trace reads too. A memo probe per entry.
        if kind.is_stack_walk() {
            shared
                .threads
                .throwable_layout_cache
                .stack_walk_named
                .name_if_stale(kind, || {
                    let cm = shared.classes.class_manager.try_read()?;
                    Some(crate::runtime::stackwalker::reflective_entries(
                        &cm.class_store,
                        kind,
                    ))
                });
        }
        let mark = self.thread.reflective_calls.len();
        self.thread
            .reflective_calls
            .push(crate::runtime::stackwalker::ReflectiveCallRow {
                interp_depth: u32::try_from(self.thread.frames.len()).unwrap_or(u32::MAX),
                jit_depth:
                    u32::try_from(crate::jit::conservative_roots::current_thread_jit_depth())
                        .unwrap_or(u32::MAX),
                kind,
                check: None,
                raised: None,
                door_save: None,
                adapter_pin: None,
            });
        mark
    }

    fn note_reflective_raise_target(&mut self, target: ObjectRef) {
        // Interpreter round i1 wave 48, lane L3: the wrapped throwable's
        // class, for a wrapper whose constructor pushes no frame
        // (`attach_reflective_raise`). Outside a reflective call: nothing.
        let Some(call) = self.thread.reflective_calls.last_mut() else {
            return;
        };
        call.raised = Some(self.shared.mem.heap.class_id_of(target));
    }

    fn note_reflective_target(&mut self, target: Option<(ClassId, &str, &str)>) {
        // Interpreter round i2 wave 62, lane L3 (wave 62's retired row-key switch):
        // the method the call's door enters, resolved now (a static JDK
        // method only: the native screens the call), withdrawn after the
        // door. Outside a reflective call: nothing.
        //
        // Interpreter round i2 wave 64, lane L3 (proposal
        // `i63-L3-proposal-the-reflective-door-notes-the-door-record`): the
        // note is the thread's door record (`JvmThread::served_door_in_flight`,
        // the carrier every static JDK door writes under
        // `VmConfig::served_raise_door_key`), the outer record saved on the
        // row (`ReflectiveCallRow::door_save`) and restored by the `None`
        // note. Wave 65: taken under `served_raise_door_key` alone (wave 62's
        // `CRATONVM_THROWABLE_SERVED_RAISE_ROW_KEY`, a strict subset of it
        // since wave 64, is retired).
        let Some(index) = self.thread.reflective_calls.len().checked_sub(1) else {
            return;
        };
        match target {
            Some((class_id, name, descriptor)) => {
                let outer = door_in_flight_note(
                    self.shared,
                    self.thread,
                    DoorOwner::Id(class_id),
                    name,
                    descriptor,
                );
                if let Some(call) = self.thread.reflective_calls.get_mut(index) {
                    // A second note without a `None` between keeps the first
                    // save: the record outside the call.
                    if call.door_save.is_none() {
                        call.door_save = Some(outer);
                    }
                }
            }
            None => {
                let save = self
                    .thread
                    .reflective_calls
                    .get_mut(index)
                    .and_then(|call| call.door_save.take());
                door_in_flight_leave(self.thread, save);
            }
        }
    }

    fn leave_reflective_call(&mut self, mark: usize) {
        // Wave 64, lane L3: a call left with its door still noted (no `None`
        // note: an early return) gives the record outside it back.
        let save = self
            .thread
            .reflective_calls
            .get(mark..)
            .and_then(outermost_door_save);
        door_in_flight_leave(self.thread, save);
        self.thread.reflective_calls.truncate(mark);
    }

    fn note_invoke_with_arguments_adapter(&mut self, pin: usize, noted: bool) {
        // Interpreter round i7 wave 136, lane L4 (page
        // `gap-hidden-walks-lack-hotspots-spun-lambda-form-frames`, residuals
        // 1-2): the served call's adapter, on ITS row only. A row is this
        // call's when it is a served `invokeWithArguments` row with nothing
        // pushed above it yet (the lanes note before they run the target) and
        // no adapter noted (an outer served call that reached a variable-arity
        // collector's unmarked call natively already noted its own).
        let interp_depth = u32::try_from(self.thread.frames.len()).unwrap_or(u32::MAX);
        let Ok(slot) = u32::try_from(pin) else {
            return;
        };
        if !noted {
            // Withdraw only this call's own note (an unmarked call must not
            // clear an outer call's). Interpreter round i7 wave 137, lane L3
            // (cross-review): searched on every row, not only the innermost,
            // so a row some callee left open above this call (an unbalanced
            // mark) cannot keep a slot that is about to be unpinned and
            // reused by another pin. An outer call's slot is never this one:
            // its pin is still held, and pins are a stack.
            for row in self.thread.reflective_calls.iter_mut().rev() {
                if row.adapter_pin == Some(slot) {
                    row.adapter_pin = None;
                    break;
                }
            }
            return;
        }
        let Some(row) = self.thread.reflective_calls.last_mut() else {
            return;
        };
        if !row.kind.is_invoke_with_arguments() {
            return;
        }
        let jit_depth = u32::try_from(crate::jit::conservative_roots::current_thread_jit_depth())
            .unwrap_or(u32::MAX);
        if row.adapter_pin.is_none() && row.interp_depth == interp_depth && row.jit_depth == jit_depth
        {
            row.adapter_pin = Some(slot);
        }
    }

    fn invoke_with_arguments_adapters(&self) -> Vec<Option<ObjectRef>> {
        // Wave 136, lane L4: read through the pin, so the reference is where
        // the object is now; a slot past the pin stack (an imbalance) answers
        // `None` rather than a stale address.
        let pins = &self.thread.native_pin_roots;
        self.thread
            .reflective_calls
            .iter()
            .filter(|row| row.kind.is_invoke_with_arguments())
            .map(|row| {
                row.adapter_pin
                    .and_then(|slot| pins.get(usize::try_from(slot).ok()?).copied())
            })
            .collect()
    }

    fn rebase_compiled_stack_budget(&mut self, stack_bounds: Option<(usize, usize)>) -> usize {
        // Interpreter round i3 wave 79, lane L3 (`L3W79NestedReflectDepth`).
        crate::jit::helpers::rebase_self_call_stack_floor(stack_bounds)
    }

    fn restore_compiled_stack_budget(&mut self, saved: usize) {
        crate::jit::helpers::restore_self_call_stack_floor(saved);
    }

    fn enter_served_thread_run(&mut self) -> usize {
        // Round 15 wave 3 (lane trace4; TR2W2-2): where the call stands, as a
        // reflective call's row (`stackwalker::ServedThreadRunCalls`).
        let interp_depth = u32::try_from(self.thread.frames.len()).unwrap_or(u32::MAX);
        let mark = self.thread.served_thread_run.enter(|| {
            let jit_depth =
                u32::try_from(crate::jit::conservative_roots::current_thread_jit_depth())
                    .unwrap_or(u32::MAX);
            (interp_depth, jit_depth)
        });
        // Round 15 wave 6 (lane trace6; TR4W3-2): name the `Thread.run` frame
        // now (once per thread and redefinition count), so the trace this
        // thread PUBLISHES -- which may not take the class-manager lock -- can
        // list its rows. `try_read` never waits: a busy lock names nothing.
        if !self.thread.served_thread_run.is_empty() {
            let shared = self.shared;
            self.thread.served_thread_run.name_if_stale(|| {
                shared
                    .classes
                    .class_manager
                    .try_read()
                    .map(|cm| crate::runtime::stackwalker::served_thread_run_entry(&cm.class_store))
            });
        }
        mark
    }

    fn leave_served_thread_run(&mut self, mark: usize) {
        self.thread.served_thread_run.leave(mark);
    }

    fn leave_served_thread_run_yielded(&mut self, mark: usize) {
        // Round 15 wave 6 (lane trace6; the yield half of TR4W3-5).
        self.thread.served_thread_run.leave_yielded(mark);
    }

    fn note_reflective_check(&mut self, check: cratonvm_native_api::ReflectiveCheck) {
        // Interpreter round i1 wave 46, lane L3: on the innermost call, which
        // the native serving the check is inside (none: not a reflective
        // call's check, nothing to note).
        let Some(call) = self.thread.reflective_calls.last_mut() else {
            return;
        };
        call.check = Some(check);
        if matches!(
            check,
            cratonvm_native_api::ReflectiveCheck::NullToPrimitive(_)
                | cratonvm_native_api::ReflectiveCheck::ArgumentCast(
                    cratonvm_native_api::ReflectiveArgumentCast::NotANumber(_)
                        | cratonvm_native_api::ReflectiveArgumentCast::Unconvertible(_)
                )
        ) {
            // The cause's top frame names `ValueConversions.unbox<Wrapper>`,
            // which the native never runs: load the class (HotSpot has it
            // loaded for the call) so the capture can name it. An error path.
            let _ = self
                .shared
                .load_class_concurrent(crate::runtime::stackwalker::VALUE_CONVERSIONS_CLASS);
        }
        if check == cratonvm_native_api::ReflectiveCheck::AbstractClass {
            // Wave 55, lane L3: the frame names the JDK's
            // `InstantiationExceptionConstructorAccessorImpl`, which HotSpot
            // loads for the call and the native never runs. An error path.
            let _ = self.shared.load_class_concurrent(
                crate::runtime::stackwalker::INSTANTIATION_EXCEPTION_ACCESSOR,
            );
        }
        if check == cratonvm_native_api::ReflectiveCheck::ClassInit {
            // Wave 58, lane L3: the frames name the JDK's accessor factory,
            // which HotSpot loads to build the accessor and `--compatible`'s
            // native never runs. Before the initialization, on its cold path.
            for class in crate::runtime::stackwalker::CLASS_INIT_CHAIN_CLASSES {
                let _ = self.shared.load_class_concurrent(class);
            }
        }
    }

    fn capture_stack_trace_tail(&mut self, want: usize) -> (Vec<StackTraceEntry>, bool) {
        let mode = caller_tail_mode();
        if mode == CALLER_TAIL_OFF || crate::runtime::env_cache::dbg_sttrace() {
            return (self.capture_current_stack_trace(), true);
        }
        let (tail, whole) = {
            let cm = self.shared.classes.class_manager.read();
            crate::runtime::stackwalker::capture_tail_trace(
                &cm.class_store,
                &self.thread.frames,
                want,
            )
        };
        if mode == CALLER_TAIL_VERIFY {
            let full = self.capture_current_stack_trace();
            let expect = &full[full.len().saturating_sub(want)..];
            let same = tail.len() == expect.len()
                && tail.iter().zip(expect).all(|(a, b)| {
                    a.class_name == b.class_name
                        && a.method_name == b.method_name
                        && a.class_id == b.class_id
                });
            if !same {
                caller_tail_log(&format!(
                    "MISMATCH getCallerClass want={want} whole={whole} tail={:?} full-tail={:?}",
                    tail.iter()
                        .map(|e| (e.class_name.as_ref(), e.method_name.as_ref(), e.class_id))
                        .collect::<Vec<_>>(),
                    expect
                        .iter()
                        .map(|e| (e.class_name.as_ref(), e.method_name.as_ref(), e.class_id))
                        .collect::<Vec<_>>(),
                ));
                eprintln!(
                    "[CALLER-TAIL-MISMATCH] want={want} whole={whole} tail={:?} full-tail={:?}",
                    tail.iter()
                        .map(|e| (e.class_name.as_ref(), e.method_name.as_ref(), e.class_id))
                        .collect::<Vec<_>>(),
                    expect
                        .iter()
                        .map(|e| (e.class_name.as_ref(), e.method_name.as_ref(), e.class_id))
                        .collect::<Vec<_>>(),
                );
                return (full, true);
            }
            if CALLER_TAIL_VERIFIED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 512 == 0 {
                caller_tail_log("verified capture_stack_trace_tail (every 512th)");
            }
        }
        (tail, whole)
    }

    fn class_cast_message(&self, from: ClassId, from_display: &str, to: ClassId) -> Option<String> {
        // Interpreter round i1 wave 47, lane L3: the interpreter's own
        // `checkcast` wording, for a native standing in for JDK code that
        // runs one (a constructor argument's `(Number) x`). An array's
        // `from` is its component's id, whose name is not `from_display`:
        // `klass_origin_by_id` then declines it and the name path resolves
        // the array's clause from its component, as for the opcode.
        let to_display = {
            let cm = self.shared.classes.class_manager.read();
            // Wave 24, lane names: an array target by its BOTTOM class's
            // hidden flag (`CRATONVM_HIDDEN_NAME_TEXT_BY_CLASS`).
            cm.get_class(to).map(|c| {
                crate::runtime::exceptions::external_name_by_bottom_class_or(
                    &c.name,
                    crate::runtime::exceptions::bottom_class_hidden_in(&cm, to, &c.name),
                    |name| crate::runtime::exceptions::hotspot_external_name_of_class(name, c.hidden),
                )
            })
        }?;
        crate::runtime::exceptions::hotspot_class_cast_message(
            self.shared,
            &*self.thread,
            from_display,
            &to_display,
            Some((from, to)),
        )
    }

    fn max_java_stack_trace_depth(&self) -> usize {
        // Interpreter round i4 wave 84, lane L2: per VM, not HotSpot's
        // default the trait answers.
        self.shared.config.max_java_stack_trace_depth
    }

    fn hidden_stack_entries(&self, entries: &[StackTraceEntry]) -> Vec<bool> {
        hidden_entries_view(&self.shared, entries, self.shared.config.show_hidden_frames)
    }

    fn hidden_member_entries(&self, entries: &[StackTraceEntry]) -> Vec<bool> {
        // Interpreter round i4 wave 92, lane L2: the method's own hiddenness,
        // as the listing answers it without `-XX:+ShowHiddenFrames`.
        hidden_entries_view(&self.shared, entries, false)
    }

    fn show_hidden_frames_flag(&self) -> bool {
        self.shared.config.show_hidden_frames
    }

    fn caller_sensitive_stack_entries(&self, entries: &[StackTraceEntry]) -> Vec<bool> {
        // Interpreter round i2 wave 55, lane L3: a `DROP_METHOD_INFO` walk's
        // ` caller sensitive` tag. Both modes (HotSpot's answer); the class
        // manager is read once per walk, and only when a frame can carry it.
        // Interpreter round i4 wave 96, lane L3: whether one can is the
        // defining loader's answer (boot or platform, as HotSpot's
        // `ClassFileParser`), read under the lock, not a name screen.
        use crate::runtime::stackwalker::stack_entry_is_caller_sensitive;
        if !entries.iter().any(|e| e.class_id.is_some()) {
            return vec![false; entries.len()];
        }
        let cm = self.shared.classes.class_manager.read();
        entries
            .iter()
            .map(|e| stack_entry_is_caller_sensitive(&cm.class_store, e))
            .collect()
    }

    fn lists_hidden_standin_frames(&self) -> bool {
        true
    }

    fn caller_class_finder_rule(&self) -> bool {
        self.shared.config.caller_class_finder_rule
    }

    fn reflection_caller_class_rule(&self) -> bool {
        self.shared.config.reflection_caller_class_rule
    }

    fn reflection_cs_caller_rule(&self) -> bool {
        self.shared.config.reflection_cs_caller_rule
    }

    fn caller_lambda_proxy_insertions(
        &mut self,
        frames: &[StackTraceEntry],
        served: &dyn Fn(&str, &str) -> bool,
    ) -> Vec<(usize, Vec<StackTraceEntry>)> {
        caller_lambda_proxy_insertions(&self.shared, frames, served, false)
    }

    fn served_lambda_proxy_caller(
        &mut self,
        served: &dyn Fn(&str, &str) -> bool,
    ) -> Option<(ClassId, Option<ClassId>)> {
        let (tail, _) = self.capture_stack_trace_tail(1);
        let top = tail.last()?;
        let proxy =
            caller_lambda_proxy_insertions(&self.shared, std::slice::from_ref(top), served, true)
                .into_iter()
                .find(|(at, _)| *at == 0)
                .and_then(|(_, entries)| entries.first().and_then(|e| e.class_id))?;
        let host = crate::runtime::interpreter::lambda_proxy_host(&self.shared, proxy)
            .and_then(|(host, _)| host);
        Some((proxy, host))
    }

    fn public_method_jdk_walk(&self) -> bool {
        self.shared.config.public_method_jdk_walk
    }

    fn reflection_caller_class_fast(&self) -> Option<ClassId> {
        // Interpreter round i2 wave 60, lane L3: the rule's answer when the
        // asking frame's caller is a frame the security walk never skips and
        // nothing the walkers' capture adds (a reflective call's or a served
        // `Thread.run`'s frames) can stand between them. Two frames, one read
        // guard, no capture.
        if !self.thread.reflective_calls.is_empty() || !self.thread.served_thread_run.is_empty() {
            // Interpreter round i6 wave 121, lane L3
            // (`VmConfig::reflection_caller_fast_under_records`): a call's JDK
            // frames splice in right below the first slot it pushed
            // (`stackwalker::splice_frames_reporting`, anchored at its
            // interpreter depth and JIT chain length). Two interpreter frames
            // pushed since the deepest call put the asker and its caller both
            // above every splice, so the two innermost slots are still theirs.
            // Fewer (the asker is the call's first frame, or a native asks
            // inside the call with nothing pushed) take the whole rule.
            let deepest = self
                .thread
                .reflective_calls
                .iter()
                .map(|row| row.interp_depth)
                .chain(self.thread.served_thread_run.deepest_interp_depth())
                .max()
                .unwrap_or(0) as usize;
            if self.thread.frames.len() < deepest.saturating_add(2) {
                return None;
            }
        }
        let (ids, _) = crate::runtime::stackwalker::frame_class_ids_tail(&self.thread.frames, 2);
        let [asker, caller] = ids[..] else {
            return None;
        };
        // Interpreter round i4 wave 94, lane L3
        // (`VmConfig::caller_lambda_proxy_frames`): an interpreted caller
        // standing at an `invokeinterface` may have reached the asker through
        // a lambda proxy, whose frame HotSpot answers; the whole rule decides
        // (`caller_lambda_proxy_insertions`). A compiled caller is answered
        // here as before.
        {
            let frames: &[_] = &self.thread.frames;
            if let [.., caller_frame, asker_frame] = frames {
                if asker_frame.class_id == asker
                    && caller_frame.class_id == caller
                    && caller_frame.code.get(caller_frame.last_instr_pc) == Some(&0xb9)
                {
                    return None;
                }
            }
        }
        let cm = self.shared.classes.class_manager.read();
        let asker_class = cm.get_class(asker)?;
        // Interpreter round i5 wave 106, lane L3 (`L3W106CallerClassAsker`):
        // an asker no boot or platform loader defined cannot be
        // `@CallerSensitive`, and HotSpot throws `InternalError` for it; the
        // whole rule decides.
        if !crate::runtime::stackwalker::class_may_be_caller_sensitive(asker_class) {
            return None;
        }
        let asker_name = &asker_class.name;
        let caller_name = &cm.get_class(caller)?.name;
        crate::runtime::stackwalker::caller_frame_never_skipped(asker_name, caller_name)
            .then_some(caller)
    }

    fn sfi_lazy_element(&self) -> bool {
        self.shared.config.sfi_lazy_element
    }

    fn compatible_jdk_walk_frames(&self) -> bool {
        self.shared.config.compatible_jdk_walk_frames
    }

    fn sfi_walk_lines(&self) -> bool {
        self.shared.config.sfi_walk_lines
    }

    fn compatible_reflect_type_impls(&self) -> bool {
        self.shared.config.compatible_reflect_type_impls
    }

    fn walk_carrier_retired(&self) -> bool {
        self.shared.natives.native_methods.walk_carrier_retired()
    }

    fn jdk_type_annotations(&self) -> bool {
        self.shared.natives.native_methods.jdk_type_annotations()
    }

    fn jdk_declaration_annotations(&self) -> bool {
        self.shared
            .natives
            .native_methods
            .jdk_declaration_annotations()
    }

    fn reflection_loads_signature_types(&self) -> bool {
        self.shared.config.reflection_loads_signature_types
    }

    fn note_served_raise_chain(&mut self, chain: cratonvm_native_api::ServedRaiseChain) -> bool {
        // Interpreter round i2 wave 54, lane L3: the next marked raise of the
        // chain's throwable class takes it (`note_served_native_raise`). The
        // hops' classes are loaded now -- HotSpot has every one of them
        // loaded for the call that raises -- so the capture, which cannot
        // load, can read their bytes. An error path.
        if !(self.shared.config.served_raise_frames && self.shared.config.served_raise_chain) {
            return false;
        }
        // Wave 56, lane L3: a chain starting at a served invoker's hidden
        // frames (a field handle's null base) is its own switch.
        if false
            && chain
                .hops
                .first()
                .is_some_and(|hop| hop.site == cratonvm_native_api::ServedRaiseHopSite::Hidden)
        {
            return false;
        }
        for hop in &chain.hops {
            // Through the transformer chain: an agent sees the load, as it
            // would HotSpot's (which loads these when it builds the accessor).
            if self
                .shared
                .load_class_transformed(self.thread, hop.class)
                .is_err()
            {
                if crate::runtime::env_cache::frame_trace() {
                    eprintln!("[SERVED_RAISE_CHAIN] refused: {} did not load", hop.class);
                }
                return false;
            }
        }
        // j27 wave 4 (lane MHROWS2): tagged with the call it is noted at,
        // after the loads above (their frames are gone again).
        self.thread.served_raise_chain = Some(Box::new(NotedServedRaiseChain {
            chain: Box::new(chain),
            site: ServedRaiseCallSite::of(&self.thread.frames),
        }));
        true
    }

    fn mark_served_raise(&mut self, chain: cratonvm_native_api::ServedRaiseChain) -> bool {
        // 2026-10-07 (item 3 of the i47-L5 page): the mark the funnel sets for
        // an unbuilt error, set before a native BUILDS its throwable, as
        // `initialize_class_served_cold` sets it for a class initialization's
        // failure; the capture of that throwable takes it while the native's
        // caller still stands at the call. Same switches; the hops' classes
        // are loaded first, as `note_served_raise_chain` loads them.
        if !(self.shared.config.served_raise_frames && self.shared.config.served_raise_chain)
            || !chain.hops.iter().all(|hop| {
                self.shared
                    .load_class_transformed(self.thread, hop.class)
                    .is_ok()
            })
        {
            if crate::runtime::env_cache::frame_trace() {
                eprintln!(
                    "[SERVED_RAISE_MARK] refused for {}: switches off or a hop did not load",
                    chain.throwable_class
                );
            }
            return false;
        }
        let depth = self.thread.frames.len();
        // Interpreter round i5 wave 104, lane L3: over a standing served
        // class-init chain's mark, kept underneath (`ServedNativeRaise::outer`).
        let outer = self
            .thread
            .served_native_raise
            .take()
            .and_then(|old| standing_init_chain_mark(&self.thread.frames, old));
        let mark =
            self.thread
                .frames
                .last()
                .map(|frame| crate::runtime::stackwalker::ServedNativeRaise {
                    throwable_class: chain.throwable_class,
                    depth,
                    pc: frame.last_instr_pc,
                    class_id: frame.class_id,
                    method_name: Box::from(frame.method_name()),
                    npe_action: None,
                    message: None,
                    receiver_class: None,
                    chain: Some(Box::new(chain)),
                    native_callee: None,
                    outer,
                });
        let marked = mark.is_some();
        if crate::runtime::env_cache::frame_trace() {
            eprintln!(
                "[SERVED_RAISE_MARK] set before the build: depth={depth} marked={marked} hops={}",
                mark.as_ref()
                    .and_then(|m| m.chain.as_ref())
                    .map_or(0, |c| c.hops.len())
            );
        }
        self.thread.served_native_raise = mark.map(Box::new);
        marked
    }

    fn withdraw_served_raise(&mut self) {
        // Interpreter round i5 wave 104, lane L3: the mark this native set
        // (at its own depth) gives way to the standing class-init chain mark
        // it was set over; a standing chain mark left after the capture took
        // this one (`take_served_raise_mark`) stays; anything else goes, as
        // before.
        let depth = self.thread.frames.len();
        self.thread.served_native_raise = match self.thread.served_native_raise.take() {
            Some(mut mark) if mark.depth == depth => mark
                .outer
                .take()
                .and_then(|outer| standing_init_chain_mark(&self.thread.frames, outer)),
            Some(mark) => standing_init_chain_mark(&self.thread.frames, mark),
            None => None,
        };
    }

    fn hidden_standin_frames(
        &mut self,
        frames: &[StackTraceEntry],
    ) -> Vec<(usize, Vec<StackTraceEntry>)> {
        hidden_standin_insertions(&self.shared, frames)
    }

    fn spun_lambda_form_frame(
        &self,
        class_id: ClassId,
        name: &str,
        descriptor: &str,
    ) -> Option<StackTraceEntry> {
        // Interpreter round i6 wave 115, lane L3 (`CRATONVM_STANDIN_SPUN_IWA_FRAMES`).
        let cm = self.shared.classes.class_manager.read();
        crate::runtime::stackwalker::spun_lambda_form_entry(
            &cm.class_store,
            class_id,
            name,
            descriptor,
        )
    }

    fn spun_lambda_form_frame_at(
        &self,
        class_id: ClassId,
        name: &str,
        descriptor: &str,
        ordinal: usize,
        count: usize,
    ) -> Option<StackTraceEntry> {
        // Interpreter round i7 wave 139, lane L3.
        let cm = self.shared.classes.class_manager.read();
        crate::runtime::stackwalker::spun_lambda_form_entry_at(
            &cm.class_store,
            class_id,
            name,
            descriptor,
            ordinal,
            count,
        )
    }

    fn method_handle_invoke_exact_site(&self, caller: &StackTraceEntry) -> Option<String> {
        // Interpreter round i6 wave 121, lane L3 (`CRATONVM_STANDIN_SPUN_LINKER_FRAMES`).
        let cm = self.shared.classes.class_manager.read();
        crate::runtime::stackwalker::method_handle_invoke_exact_site(&cm.class_store, caller)
    }

    fn method_handle_generic_invoke_site(&self, caller: &StackTraceEntry) -> Option<String> {
        // Interpreter round i7 wave 142, lane L3 (`CRATONVM_STANDIN_SPUN_LINKER_FRAMES`).
        let cm = self.shared.classes.class_manager.read();
        crate::runtime::stackwalker::method_handle_generic_invoke_site(&cm.class_store, caller)
    }

    fn capture_throwable_stack_trace(&mut self, throwable: ObjectRef) -> usize {
        // `CRATONVM_DBG_DIRECT_THROWABLE_BUILD` (R23W1L3-3): one per capture.
        use crate::vm::realms::thread_realm::direct_throwable_event::CAPTURES;
        self.shared.threads.throwable_layout_cache.note_direct_throwable(CAPTURES);
        // nio21 wave 21 L1
        // (`r21-implicit-catch-rare-badtrace-under-moving-collectors-FIXED-20261010.md`):
        // the capture stores the trace under the address it was HANDED. A
        // collection inside it would leave the row at the old address -- this
        // throwable without a trace, or, where a dead throwable's row survived
        // at the new one, with THAT trace. The throwable is pinned across the
        // capture and the pin read back: moved, it is captured again at its
        // new address (the hints the first capture took are not replayed; a
        // capture that collects is the rare case this repairs).
        // `CRATONVM_THROWABLE_RECAPTURE_IF_MOVED=0` stores once, as before.
        if !crate::runtime::exceptions::throwable_recapture_if_moved_enabled() {
            return self.capture_throwable_stack_trace_once(throwable);
        }
        let pin = self.thread.native_pin_roots.len();
        self.thread.native_pin_roots.push(throwable);
        let mut captured_at = throwable;
        let mut depth = self.capture_throwable_stack_trace_once(throwable);
        for _ in 0..2 {
            let now = self.thread.native_pin_roots[pin];
            if now == captured_at {
                break;
            }
            depth = self.capture_throwable_stack_trace_once(now);
            captured_at = now;
        }
        self.thread.native_pin_roots.truncate(pin);
        depth
    }

    fn cached_stack_trace_element_class_id(&self) -> Option<ClassId> {
        use std::sync::atomic::Ordering;

        let class_id = self
            .shared
            .threads
            .throwable_layout_cache
            .stack_trace_element_class_id
            .load(Ordering::Relaxed);
        (class_id != crate::vm::realms::thread_realm::UNRESOLVED_STACK_TRACE_ELEMENT_CLASS_ID)
            .then(|| ClassId::new(class_id))
    }

    fn cache_stack_trace_element_class_id(&self, class_id: ClassId) {
        self.shared
            .threads
            .throwable_layout_cache
            .stack_trace_element_class_id
            .store(class_id.as_u32(), std::sync::atomic::Ordering::Relaxed);
    }

    fn throwable_suppressed_sentinel(&self) -> Option<Value> {
        use std::sync::atomic::Ordering;

        let cache = &self.shared.threads.throwable_layout_cache;
        let raw_class_id = cache.throwable_class_id.load(Ordering::Relaxed);
        let class_id = if raw_class_id
            == crate::vm::realms::thread_realm::UNRESOLVED_STACK_TRACE_ELEMENT_CLASS_ID
        {
            let resolved = self.class_id_by_name("java/lang/Throwable")?;
            cache
                .throwable_class_id
                .store(resolved.as_u32(), Ordering::Relaxed);
            resolved
        } else {
            ClassId::new(raw_class_id)
        };
        let raw_index = cache
            .suppressed_sentinel_static_index
            .load(Ordering::Relaxed);
        let index =
            if raw_index == crate::vm::realms::thread_realm::UNRESOLVED_THROWABLE_FIELD_INDEX {
                let resolved = self.static_field_index_by_name(class_id, "SUPPRESSED_SENTINEL")?;
                cache
                    .suppressed_sentinel_static_index
                    .store(resolved, Ordering::Relaxed);
                resolved
            } else {
                raw_index
            };
        Some(self.get_static_field(class_id, index))
    }

    fn throwable_stack_trace_depth(&self, throwable: ObjectRef) -> Option<usize> {
        self.shared.throwable_stack_trace_len_for(throwable)
    }

    fn innermost_frame_class_exact(&self) -> bool {
        crate::runtime::stackwalker::innermost_frame_class_exact(&self.thread.frames)
    }

    fn clone_throwable_backtrace(&mut self, original: ObjectRef, clone: ObjectRef) {
        self.shared.clone_throwable_backtrace(original, clone);
    }

    // Interpreter round i3 wave 82, lane L2:
    // `CRATONVM_THROWABLE_BACKTRACE_IN_OBJECT`, this VM's answer.
    fn throwable_backtrace_in_object(&self) -> bool {
        self.shared
            .threads
            .throwable_layout_cache
            .backtrace_in_object_enabled()
    }

    fn throwable_stack_trace_entry(
        &self,
        throwable: ObjectRef,
        index: usize,
    ) -> Option<StackTraceEntry> {
        self.shared
            .throwable_stack_trace_entry_for(throwable, index)
    }

    // Round 15 wave 7 (lane throw2, TH1-5): `exceptions::create_compiled_implicit_npe`.
    fn deferred_npe_message(&mut self, throwable: ObjectRef) -> Option<Option<String>> {
        crate::runtime::exceptions::deferred_npe_message(self.shared, throwable)
    }

    // JIT round 16 (lane compatdrain, C5W4-1): `runtime::null_arg_message`.
    fn null_arg_message(
        &self,
        class: &str,
        method: &str,
        descriptor: &str,
        param: usize,
    ) -> Option<String> {
        crate::runtime::null_arg_message::null_arg_message(
            self.shared,
            class,
            method,
            descriptor,
            param,
        )
    }

    fn replace_throwable_stack_trace(
        &mut self,
        throwable: ObjectRef,
        entries: Vec<StackTraceEntry>,
    ) -> bool {
        let frames = entries
            .into_iter()
            .map(crate::runtime::stackwalker::BacktraceFrame::Entry)
            .collect();
        self.shared.store_throwable_backtrace(throwable, frames);
        true
    }

    fn get_throwable_stack_trace(&self, throwable: ObjectRef) -> Option<Vec<StackTraceEntry>> {
        let r = self.shared.throwable_stack_trace_for(throwable);
        if crate::runtime::env_cache::dbg_sttrace() {
            eprintln!(
                "STTRACE_DBG_LOOKUP obj={:?} hit={} len={}",
                throwable.as_ptr(),
                r.is_some(),
                r.as_ref().map_or(0, Vec::len),
            );
        }
        r
    }

    fn frame_class_ids_tail(&self, want: usize) -> (Vec<ClassId>, bool) {
        if caller_tail_mode() == CALLER_TAIL_OFF {
            return (self.frame_class_ids(), true);
        }
        let (tail, whole) =
            crate::runtime::stackwalker::frame_class_ids_tail(&self.thread.frames, want);
        if caller_tail_mode() == CALLER_TAIL_VERIFY {
            let full = self.frame_class_ids();
            let expect = &full[..full.len().min(want)];
            if tail.as_slice() != expect {
                caller_tail_log(&format!(
                    "MISMATCH frame_class_ids want={want} whole={whole} tail={tail:?} full-head={expect:?}"
                ));
                eprintln!(
                    "[CALLER-TAIL-MISMATCH] frame_class_ids want={want} whole={whole} tail={tail:?} full-head={expect:?}"
                );
                return (full, true);
            }
            if CALLER_TAIL_VERIFIED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 512 == 0 {
                caller_tail_log("verified frame_class_ids_tail (every 512th)");
            }
        }
        (tail, whole)
    }

    fn frame_class_ids(&self) -> Vec<ClassId> {
        // Innermost-first, matching `capture_stack_trace`'s `.iter().rev()`
        // convention (see its own callers, e.g. `resolve_caller_class_id`).
        //
        // This used to map `self.thread.frames` directly, which SKIPS every
        // JIT-compiled frame — a compiled method pushes no interpreter `Frame`.
        // Caller attribution then blamed the next frame down. See
        // `stackwalker::frame_class_ids_with_compiled` for the two directions
        // that gets wrong and the witness for each.
        crate::runtime::stackwalker::frame_class_ids_with_compiled(&self.thread.frames)
    }
}

impl NativeContextImpl<'_> {
    /// One capture of [`NativeExceptionAccess::capture_throwable_stack_trace`]
    /// (the body it had before nio21 wave 21 L1), storing the trace under
    /// `throwable`'s address as handed.
    pub(in crate::vm::vm_exec) fn capture_throwable_stack_trace_once(&mut self, throwable: ObjectRef) -> usize {
        // HotSpot's `fill_in_stack_trace` skip, applied HERE rather than at the
        // `fillInStackTrace` native, because this is the one entry point every
        // "record this throwable's origin" path goes through — the twelve
        // `native_exc_init_*` bodies via `capture_throwable_trace`, and
        // `Throwable.fillInStackTrace(int)` itself. Doing it at one of those
        // instead would leave the other reporting the filling machinery as the
        // throw site. See `stackwalker::trim_throwable_fill_frames`.
        //
        // Decided before the entries are built, so the frames it removes are
        // never materialised -- see `capture_throwable_trace_without_store`.
        // Source lines stay unresolved: `SharedVm::throwable_stack_trace_for`
        // resolves them only if Java ever reads the trace.
        // The class-manager lock is still taken only when a frame's name
        // matched, as before.
        let throwable_class = self.class_id_of_object(throwable);
        // JIT round 20 (lane throw): a throwable of another class than the one
        // an armed trap snapshot is building walks its own compiled frames
        // (`stackwalker::suspend_trap_capture_for_foreign`); the guard puts
        // the snapshot back when this capture returns.
        let _foreign_capture =
            crate::runtime::stackwalker::suspend_trap_capture_for_foreign(throwable_class);
        // Round 14 wave 6 (lane trace5; TR5-3): a served `Thread.join(long)`'s
        // hint, taken by EVERY capture so it never outlives the throw it was
        // set for.
        let timed_join = self.thread.served_timed_join_hint.take();
        // Round 15 wave 2 (lane trace2): a served built-in loader's miss, taken
        // the same way.
        let loader_miss = self.thread.served_loader_miss_hint.take();
        // `-XX:-StackTraceInThrowable` (interpreter round i4 wave 85, lane
        // L2): HotSpot's `fill_in_stack_trace` returns at once, so every
        // throwable reads an empty trace. An EMPTY row is stored rather than
        // none, so the `backtrace` marker the caller writes and `depth == 0`
        // still name a row (the `--compatible` reader and the trap splice
        // treat "no row" as "not ours"). Every capture door ends here: the
        // native `fillInStackTrace(int)`, the `native_exc_init_*` bodies, the
        // direct implicit build and the JIT's direct `new Throwable`.
        if !self.shared.config.stack_trace_in_throwable {
            let _ = (timed_join, loader_miss);
            self.shared
                .store_throwable_backtrace(throwable, Vec::new().into_boxed_slice());
            return 0;
        }
        // Interpreter round i2 wave 53, lane L3: the native-call funnel's mark
        // of a raise (`note_served_native_raise`), taken the same way and
        // believed only while the interpreted frame it names still stands at
        // the instruction it was called from.
        if crate::runtime::env_cache::frame_trace() {
            if let Some(mark) = self.thread.served_native_raise.as_ref() {
                let cname = self
                    .shared
                    .classes
                    .class_manager
                    .read_recursive()
                    .get_class(throwable_class)
                    .map(|c| c.name.to_string())
                    .unwrap_or_default();
                let at = mark
                    .depth
                    .checked_sub(1)
                    .and_then(|index| self.thread.frames.get(index))
                    .map(|f| (f.method_name().to_string(), f.last_instr_pc));
                eprintln!(
                    "[SERVED_RAISE_MARK] capture of {cname}: mark {} depth={} pc={} frames.len={} frame@depth={:?}",
                    mark.throwable_class,
                    mark.depth,
                    mark.pc,
                    self.thread.frames.len(),
                    at
                );
            }
        }
        let served_raise = if self
            .thread
            .served_native_raise
            .as_ref()
            .is_some_and(|mark| {
                self.shared
                    .classes
                    .class_manager
                    .read_recursive()
                    .get_class(throwable_class)
                    .is_some_and(|c| &*c.name == mark.throwable_class)
            }) {
            // Interpreter round i2 wave 62, lane L3
            // (`VmConfig::served_raise_own_construction`): a capture of the
            // mark's class made deeper, in Java the native ran (a `<clinit>`
            // catching another class's failed initialization), leaves the
            // mark standing for the raise it was set for.
            if served_raise_mark_left_for_its_own_raise(
                    self.shared,
                    self.thread,
                    throwable_class,
                )
            {
                None
            } else {
                // Wave 104, lane L3: a class-init chain mark this one was set
                // over stands again (`ServedNativeRaise::outer`).
                take_served_raise_mark(self.thread)
            }
            .filter(|mark| {
                mark.depth
                    .checked_sub(1)
                    .and_then(|index| self.thread.frames.get(index))
                    .is_some_and(|frame| frame.last_instr_pc == mark.pc)
            })
        } else {
            None
        };
        // Interpreter round i2 wave 60, lane L3: a door's native callee is
        // listed only when nothing but the throwable's own construction runs
        // above the marked frame (`stackwalker::door_native_callee_frame`).
        let mut served_raise = served_raise;
        if let Some(mark) = served_raise
            .as_deref_mut()
            .filter(|mark| mark.native_callee.is_some())
        {
            let frames = &self.thread.frames;
            // Wave 62, lane L3: under `served_raise_own_construction` the
            // constructors must be the throwable's own (its class or a
            // superclass), not any `<init>`.
            let constructing = {
                only_construction_above(self.shared, frames, mark.depth, throwable_class)
            };
            if !constructing {
                mark.native_callee = None;
            }
        }
        // Interpreter round i2 wave 62, lane L3 (the row key, retired in wave 65)
        // and wave 63 (`VmConfig::served_raise_door_key`): with no door callee
        // on a mark, the method the innermost door is entering
        // (`JvmThread::served_door_in_flight`), when nothing but this
        // throwable's construction stands above the door (`door_key_believed`).
        // Wave 64: the served `Method.invoke`'s door writes the same record
        // (`ReflectiveCallRow::door_save`), so the innermost door keys and
        // there is no second carrier to choose against. With the switch off
        // no record is ever set.
        let unkeyed_mark = served_raise
            .as_deref()
            .is_none_or(|mark| mark.native_callee.is_none());
        let door_record = if unkeyed_mark {
            self.thread.served_door_in_flight
        } else {
            None
        };
        let door_key = door_record
            .and_then(|door| served_door_key(self.shared, self.thread, &door, throwable_class));
        // JIT round 16 wave 13 (lane wmte): a `MethodHandle` door's
        // `WrongMethodTypeException` hint, taken only by a capture of that
        // class (another throwable built in between leaves it; the door
        // withdraws it after the build).
        let mh_wmte = if self.thread.mh_wmte_raise_hint.is_some()
            && self
                .shared
                .classes
                .class_manager
                .read_recursive()
                .get_class(throwable_class)
                .is_some_and(|c| &*c.name == "java/lang/invoke/WrongMethodTypeException")
        {
            self.thread.mh_wmte_raise_hint.take()
        } else {
            None
        };
        // Interpreter round i1 wave 48, lane L1: a virtual-thread sleep's
        // hint, taken by the `InterruptedException` capture it was set for
        // (wave 48 follow-up: a capture of another throwable in between, say
        // while that class is first loaded and initialized, leaves it).
        let virtual_sleep = if self.thread.virtual_sleep_hint.is_some()
            && self
                .shared
                .classes
                .class_manager
                .read_recursive()
                .get_class(throwable_class)
                .is_some_and(|c| &*c.name == "java/lang/InterruptedException")
        {
            self.thread.virtual_sleep_hint.take()
        } else {
            if self.thread.virtual_sleep_hint.is_some() && crate::runtime::env_cache::frame_trace()
            {
                eprintln!("[VIRTUAL_SLEEP_FRAMES] hint kept across another throwable's capture");
            }
            None
        };
        // Interpreter round i5 wave 103, lane L3
        // (`VmConfig::served_init_chain_capture_frames`): a capture made inside
        // a `<clinit>` a served door runs under a class-init chain, while the
        // mark still stands (not taken above), lists the chain's JDK frames
        // below the `<clinit>` (`insert_served_init_chain_capture_frames`). A
        // thread with no mark pays the one `Option` test.
        // Wave 104, lane L3: every standing chain of the thread's mark stack
        // (`ServedNativeRaise::outer`), and also for a capture that took a
        // raise marked inside the `<clinit>` (taking it put the chain's mark
        // back): HotSpot lists the chain below that raise's frames too.
        let init_chain_standing = self.thread.served_native_raise.is_some()
            && served_init_chain_stands(
                &self.thread.frames,
                self.thread.served_native_raise.as_deref(),
            );
        // Interpreter round i5 wave 105, lane L3
        // (`i103-L3-clinit-trigger-throwable-traces-lack-served-door-frames`
        // residue): under `-XX:+ShowHiddenFrames` a throwable keeps hidden
        // frames, so a static VarHandle's chain lists the hidden
        // `VarHandleGuards.guard_*` frame as the walk does. Its class is loaded
        // here, before any class-manager guard is taken (as the walk does).
        let init_chain_guard = init_chain_standing
            && self.shared.config.show_hidden_frames
            && mark_stack_has_var_handle_chain(self.thread.served_native_raise.as_deref());
        if init_chain_guard {
            let _ = self
                .shared
                .load_class_concurrent("java/lang/invoke/VarHandleGuards");
        }
        let shared = self.shared;
        // Interpreter round i1 wave 43, lane L3: the JDK frames of the
        // reflective calls this thread is inside, as the thread last named
        // them, or named now under a class-manager read guard taken for that
        // alone (the capture itself takes none). A thread outside any
        // reflective call pays this one branch.
        let splices = if self.thread.reflective_calls.is_empty() {
            Vec::new()
        } else {
            let thread = &mut *self.thread;
            let mut splices = crate::runtime::stackwalker::reflective_splices(
                &mut thread.reflective_frames_named,
                &thread.reflective_calls,
                &mut |kind: cratonvm_native_api::ReflectiveCallKind| {
                    let cm = shared.classes.class_manager.read();
                    let entries =
                        crate::runtime::stackwalker::reflective_entries(&cm.class_store, kind);
                    drop(cm);
                    entries
                },
            );
            attach_reflective_raise(shared, thread, throwable_class, &mut splices);
            splices
        };
        // Round 15 wave 3 (lane trace4; TR2W2-2): the served `Thread.run`
        // calls this thread is inside, merged after the reflective raise
        // (`stackwalker::ServedThreadRunCalls`). A thread with them takes no
        // rule-built `Thread.run` frame below (`served_rows`).
        let mut splices = splices;
        // Round 15 wave 5 (lane trace5; TR4W3-7): under `CRATONVM_DBG_STTRACE`
        // only, the reflective splices alone, for the rules' side answer below.
        let rules_side_splices = (crate::runtime::env_cache::dbg_sttrace()
            && !self.thread.served_thread_run.is_empty())
        .then(|| splices.clone());
        let served_rows = if self.thread.served_thread_run.is_empty() {
            false
        } else {
            let rows = self.thread.served_thread_run.splices(|| {
                let cm = shared.classes.class_manager.read();
                let entry = crate::runtime::stackwalker::served_thread_run_entry(&cm.class_store);
                drop(cm);
                entry
            });
            let any = !rows.is_empty();
            crate::runtime::stackwalker::merge_served_thread_run_splices(&mut splices, rows);
            any
        };
        let frames = &self.thread.frames;
        let mut cm = None;
        // HotSpot's `MaxJavaStackTraceDepth` (per-VM): the capture keeps the
        // innermost entries, counted after the fill-frame trim above, and walks
        // only as many frames as that needs.
        let max_depth = shared.config.max_java_stack_trace_depth;
        // JIT round 16 wave 7 (lane throwpath, TP16-6;
        // `CRATONVM_THROWABLE_CAPTURE_MEMO=1`): the capture memo
        // (`stackwalker::ThrowableCaptureMemo`). Only for a capture whose answer
        // is a function of the frames and this key: no served hint, no
        // splice, no debug census; the screens below decide the rest.
        let thread_obj_addr = self
            .thread
            .java_thread_obj
            .map_or(0, |o| o.as_ptr() as usize);
        // Interpreter round i3 wave 73, lane L2
        // (`CRATONVM_THROWABLE_LOADER_MISS_MEMO=1`): a served loader's miss is
        // keyed (`ThrowableCaptureKey::loader_miss`) instead of refused. Read
        // only for a hinted capture.
        let memo_key = (shared.threads.throwable_layout_cache.capture_memo_enabled()
            && self.thread.throwable_capture_memo.attempt()
            && timed_join.is_none()
            && (loader_miss.is_none()
                || shared.threads.throwable_layout_cache.loader_miss_memo_enabled())
            && served_raise.is_none()
            // Wave 103, lane L3: nor one under a standing class-init chain.
            && !init_chain_standing
            // Wave 63, lane L3: a door-keyed capture's answer is the door's,
            // not the frames' (a handle's door opens no splice).
            && door_key.is_none()
            && mh_wmte.is_none()
            && virtual_sleep.is_none()
            // JIT round 20 wave 12 (lane qMisc, L4W15-2;
            // `CRATONVM_THROWABLE_CAPTURE_MEMO_SPLICES=1`): a capture under
            // reflective splices is keyed by them
            // (`ThrowableCaptureMemo::with_splices`) instead of refused.
            // Served `Thread.run` rows still refuse (`served_rows`).
            && (splices.is_empty()
                || shared.threads.throwable_layout_cache.capture_memo_splices_enabled())
            && !served_rows
            && rules_side_splices.is_none()
            && !crate::runtime::env_cache::dbg_sttrace()
            && frames.len() <= crate::runtime::stackwalker::ThrowableCaptureMemo::MAX_FRAMES)
            .then(|| crate::runtime::stackwalker::ThrowableCaptureKey {
                throwable_class,
                redefinitions: cratonvm_classloading::class_redefinition_count(),
                max_depth,
                thread_obj: thread_obj_addr,
                loader_miss,
            });
        let mut memo_compiled = None;
        let mut memo_raw = crate::jit::conservative_roots::RawWalkKey::default();
        let layout_cache = &shared.threads.throwable_layout_cache;
        // JIT round 16 wave 11 (lane throw2, TP16-12;
        // `CRATONVM_THROWABLE_TRIM_IS_A_MEMO=1`): the fill-frame trim's
        // `is_a` answers, remembered per thread
        // (`stackwalker::ThrowableIsAMemo`), so a repeated throw does not take
        // the class-manager lock for them.
        let mut is_a_memo = layout_cache
            .trim_is_a_memo_enabled()
            .then_some(&mut self.thread.throwable_is_a_memo);
        let is_a_redefinitions = is_a_memo
            .as_ref()
            .map_or(0, |_| cratonvm_classloading::class_redefinition_count());
        // JIT round 16 wave 15 (lane throwfill;
        // `CRATONVM_THROWABLE_WALK_SCOPED_LOOKUPS=1`): the compiled-frame walk
        // with its per-activation lookups scoped to the walk
        // (`conservative_roots::WalkLookups`). The same frames.
        let walk_scoped = layout_cache.walk_scoped_lookups_enabled();
        // JIT round 17 wave 1 (lane throwfill, TF17-1;
        // `CRATONVM_THROWABLE_FUSED_CAPTURE=1`): the survivors built by
        // consuming the walk (labels moved, not cloned), the label screen once
        // per distinct label, the one-pass screens remembered per label, on a
        // walk-scoped walk. The same trace (`capture_throwable_backtrace_fused`).
        let fused = layout_cache.fused_capture_enabled();
        // JIT round 19 wave 1 (lane throwfill, THROWS18-1;
        // `CRATONVM_THROWABLE_FUSED_REWRITTEN=1`): the fused door also lays out
        // a walk the OSR / call dedupe rewrites (dedupe first, its overrides
        // laid out with the walk). The same trace.
        let fused_rewritten = fused && layout_cache.fused_rewritten_enabled();
        // TF17-0 (`CRATONVM_DBG_THROWFILL_PHASES=1`): where a capture's time
        // goes, per phase, printed per 1024 captures. Off, nothing is timed.
        let phases = layout_cache.throwfill_phases_enabled();
        // JIT round 17 wave 2 (lane walk, TF17-5;
        // `CRATONVM_THROWABLE_BOUNDED_WALK=1`): a capped capture walks only the
        // innermost `max_depth + slack` compiled activations of each chain
        // entry and proves the cut lies outside the kept frames
        // (`stackwalker::capture_throwable_backtrace_bounded`). The same trace.
        let bounded_walk = max_depth != 0 && layout_cache.bounded_walk_enabled();
        let phase_start = phases.then(std::time::Instant::now);
        let mut phase_walked: Option<std::time::Instant> = None;
        // Retained as `BacktraceFrame`s: one refcount per cached interpreter
        // frame instead of four (see `stackwalker::BacktraceFrame`).
        let mut trace = if let Some(key) = memo_key.as_ref() {
            use crate::vm::realms::thread_realm::{CAPTURE_MEMO_HIT, CAPTURE_MEMO_MISS};
            // 2026-10-07: first without the walk -- a memo whose recorded
            // walk reads what this stack reads answers before any code range
            // is resolved (`ThrowableCaptureMemo::lookup_raw`).
            if !crate::runtime::stackwalker::trap_capture_armed() {
                if let Some(hit) = self
                    .thread
                    .throwable_capture_memo
                    .lookup_raw_spliced(key, frames, &splices)
                {
                    self.thread.throwable_capture_memo.note_outcome(true);
                    layout_cache
                        .note_capture_memo(crate::vm::realms::thread_realm::CAPTURE_MEMO_RAW_HIT);
                    if !splices.is_empty() {
                        layout_cache.note_capture_memo(
                            crate::vm::realms::thread_realm::CAPTURE_MEMO_SPLICED_HIT,
                        );
                    }
                    layout_cache.note_capture_memo(CAPTURE_MEMO_HIT);
                    let depth = hit.len();
                    shared.retain_captured_backtrace_shared(throwable, hit);
                    return depth;
                }
            } else if let Some(hit) =
                crate::runtime::stackwalker::lookup_memo_on_armed_trap_capture_spliced(
                    &mut self.thread.throwable_capture_memo,
                    key,
                    frames,
                    &splices,
                )
            {
                // nio21 wave 15 L4: a compiled implicit exception's capture
                // compares the armed trap snapshot in place instead of
                // cloning it first (the walk-compared hit below, without the
                // clone; a snapshot records no raw key to alias).
                self.thread.throwable_capture_memo.note_outcome(true);
                if !splices.is_empty() {
                    layout_cache.note_capture_memo(
                        crate::vm::realms::thread_realm::CAPTURE_MEMO_SPLICED_HIT,
                    );
                }
                layout_cache.note_capture_memo(CAPTURE_MEMO_HIT);
                let depth = hit.len();
                shared.retain_captured_backtrace_shared(throwable, hit);
                return depth;
            }
            let jit = crate::runtime::stackwalker::throwable_capture_compiled_frames_recorded(
                frames.len(),
                walk_scoped || fused,
                max_depth,
                &mut memo_raw,
            );
            if let Some((way, hit)) = self
                .thread
                .throwable_capture_memo
                .lookup_spliced(key, frames, &jit, &splices)
            {
                self.thread.throwable_capture_memo.note_outcome(true);
                self.thread
                    .throwable_capture_memo
                    .alias(way, std::mem::take(&mut memo_raw));
                if !splices.is_empty() {
                    layout_cache.note_capture_memo(
                        crate::vm::realms::thread_realm::CAPTURE_MEMO_SPLICED_HIT,
                    );
                }
                layout_cache.note_capture_memo(CAPTURE_MEMO_HIT);
                let depth = hit.len();
                shared.retain_captured_backtrace_shared(throwable, hit);
                return depth;
            }
            self.thread.throwable_capture_memo.note_outcome(false);
            layout_cache.note_capture_memo(CAPTURE_MEMO_MISS);
            let mut is_a = |holder: Option<ClassId>, holder_name: &str| {
                throwable_capture_is_a_memoised(
                    shared,
                    &mut cm,
                    is_a_memo.as_deref_mut(),
                    is_a_redefinitions,
                    throwable_class,
                    holder,
                    holder_name,
                )
            };
            // JIT round 19 wave 2 (lane throws2, THROWS19-8;
            // `CRATONVM_THROWABLE_MEMO_MISS_FUSED=1`): with the fused door on,
            // the miss is laid out by it and the memo keeps a copy of the walk
            // (cheap under `CRATONVM_THROWABLE_SHARED_INLINE_CHAINS=1`). The
            // same trace and the same memo.
            let (trace, walk) = if fused && crate::runtime::stackwalker::memo_miss_fused_enabled() {
                crate::runtime::stackwalker::capture_throwable_backtrace_fused_returning_walk(
                    frames,
                    jit,
                    max_depth,
                    fused_rewritten,
                    &mut is_a,
                    &splices,
                )
            } else {
                // JIT round 16 wave 9 (lane throwpath): the walk comes back
                // from the capture instead of being cloned before it.
                crate::runtime::stackwalker::capture_throwable_backtrace_returning_walk(
                    frames, jit, max_depth, &mut is_a, &splices,
                )
            };
            memo_compiled = Some(walk);
            trace
        } else if bounded_walk {
            // JIT round 17 wave 2 (lane walk, TF17-5): the compiled walk
            // bounded to the cap, verified, redone whole on a miss. Its walk
            // is timed with its layout by the phase census.
            let mut is_a = |holder: Option<ClassId>, holder_name: &str| {
                throwable_capture_is_a_memoised(
                    shared,
                    &mut cm,
                    is_a_memo.as_deref_mut(),
                    is_a_redefinitions,
                    throwable_class,
                    holder,
                    holder_name,
                )
            };
            crate::runtime::stackwalker::capture_throwable_backtrace_bounded(
                frames,
                max_depth,
                walk_scoped || fused,
                fused,
                &mut is_a,
                &splices,
            )
        } else if walk_scoped || fused || phases {
            // The walk taken apart from the layout: with neither switch on
            // (only the phase census), the same walk and the same capture as
            // `capture_throwable_backtrace_without_store`.
            let jit = crate::runtime::stackwalker::throwable_capture_compiled_frames_with(
                frames.len(),
                walk_scoped || fused,
                max_depth,
            );
            if phases {
                phase_walked = Some(std::time::Instant::now());
                // JIT round 18 wave 10 (lane throws): the walk's shape for the
                // same census (compiled activations, inlined levels, and the
                // fused door's refusal of this walk when the door is on).
                let inlined = jit.iter().map(|f| f.chain().len()).sum::<usize>();
                let refusal = if fused {
                    crate::runtime::stackwalker::fused_capture_refusal(frames, &jit, &splices)
                } else {
                    None
                };
                // JIT round 19 wave 1 (lane throwfill, THROWS18-1): a rewritten
                // walk the fused door lays out itself is no refusal; it is
                // counted in its own column (`fused-rewritten=`).
                let refusal = match refusal {
                    Some(crate::runtime::stackwalker::FusedRefusal::Rewritten)
                        if fused_rewritten =>
                    {
                        layout_cache.note_throwfill_fused_rewritten();
                        None
                    }
                    other => other.map(|r| r as usize),
                };
                layout_cache.note_throwfill_shape(jit.len(), inlined, refusal);
            }
            let mut is_a = |holder: Option<ClassId>, holder_name: &str| {
                throwable_capture_is_a_memoised(
                    shared,
                    &mut cm,
                    is_a_memo.as_deref_mut(),
                    is_a_redefinitions,
                    throwable_class,
                    holder,
                    holder_name,
                )
            };
            if fused {
                crate::runtime::stackwalker::capture_throwable_backtrace_fused_with(
                    frames,
                    jit,
                    max_depth,
                    fused_rewritten,
                    &mut is_a,
                    &splices,
                )
            } else {
                crate::runtime::stackwalker::capture_throwable_backtrace_from_walk(
                    frames, jit, max_depth, &mut is_a, &splices,
                )
            }
        } else {
            crate::runtime::stackwalker::capture_throwable_backtrace_without_store(
                frames,
                max_depth,
                &mut |holder: Option<ClassId>, holder_name: &str| {
                    throwable_capture_is_a_memoised(
                        shared,
                        &mut cm,
                        is_a_memo.as_deref_mut(),
                        is_a_redefinitions,
                        throwable_class,
                        holder,
                        holder_name,
                    )
                },
                &splices,
            )
        };
        // JIT round 16 wave 9 (lane throwpath, TP16-9;
        // `CRATONVM_THROWABLE_SCREEN_ONCE=1`): the three per-frame pre-screens
        // below, decided in one pass that names each frame once
        // (`stackwalker::CaptureScreens`; each answer exactly its screen's).
        // `None` -- the switch off, or the trace changed since -- leaves every
        // rule to its own screen.
        let phase_laid_out = phases.then(std::time::Instant::now);
        // Interpreter round i2 wave 60, lane L3: the marked frame as captured,
        // before the hidden drop below may remove it (`append_native_standin_frames`).
        let hidden_marked = served_raise
            .as_deref()
            .and_then(|mark| trace.last().filter(|frame| mark.stands_at(frame)).cloned());
        let mut screens = layout_cache.screen_once_enabled().then(|| {
            if fused {
                // TF17-1: the same answers, a repeated label screened once.
                crate::runtime::stackwalker::capture_screens_memoised(&trace)
            } else {
                crate::runtime::stackwalker::capture_screens(&trace)
            }
        });
        // Interpreter round i3 wave 73, lane L2
        // (`CRATONVM_THROWABLE_LOADER_MISS_MEMO=1`): a loader-middle screen
        // whose every engaged pair the decoded rule proves gets no frames (a
        // real-JDK delegation: `loadClass(String, boolean)` calls above
        // `BuiltinClassLoader.loadClass`'s throw) is quiet: the memo below is
        // made and the rules are not run. Read only once the screen engaged.
        if let Some(s) = screens.as_mut().filter(|s| s.loader_middle) {
            if layout_cache.loader_miss_memo_enabled() {
                let guard = cm.get_or_insert_with(|| shared.classes.class_manager.read());
                if crate::runtime::stackwalker::loader_middle_pairs_decided_quiet(
                    &guard.class_store,
                    &trace,
                ) {
                    s.loader_middle = false;
                }
            }
        }
        // The same for the `Thread.run` middle-frame screen: a lambda body
        // under a caller that is not at a `run()V` call (every task frame of a
        // JUnit or ForkJoinPool stack) gets no frame whatever is loaded, so
        // the memo is made and the rule is not run.
        if let Some(s) = screens.as_mut().filter(|s| s.thread_run_middle) {
            let guard = cm.get_or_insert_with(|| shared.classes.class_manager.read());
            if crate::runtime::stackwalker::thread_run_middle_pairs_decided_quiet(
                &guard.class_store,
                &trace,
            ) {
                s.thread_run_middle = false;
            }
        }
        // TP16-6: a memo is made only when none of the class-lookup screens
        // engages (their answers can depend on what is loaded).
        let mut memo_screens_quiet = memo_compiled.is_some()
            && match screens {
                Some(s) => !s.thread_run_middle && !s.loader_middle,
                None => !trace.windows(2).any(|pair| {
                    crate::runtime::stackwalker::may_need_thread_run_middle_frame(
                        &pair[0], &pair[1],
                    ) || crate::runtime::stackwalker::may_need_loader_middle_frames(
                        &pair[0], &pair[1],
                    )
                }),
            };
        // Round 14 wave 3 (lane trace): the outermost frame as captured, for
        // `prepend_thread_run_standin_frame` below (a string screen; cloned
        // only for a `run()V` frame, or -- lane trace2 -- any frame not named
        // `main`, which may be a lambda task's implementation method).
        let thread_run_outermost = trace
            .first()
            .filter(|f| {
                crate::runtime::stackwalker::may_be_thread_run_task_frame(f)
                    || crate::runtime::stackwalker::may_be_thread_run_lambda_frame(f)
            })
            .cloned();
        // Round 14 wave 3 (lane trace2): `Thread.run` between a `super.run()`
        // (or a direct `thread.run()`) and the task it ran. Before the hidden
        // drop (a lambda task's `run` is hidden); HotSpot counts the frame
        // against `MaxJavaStackTraceDepth`, so the capture is re-capped.
        let platform_thread = !matches!(self.thread.kind, crate::threading::ThreadKind::Virtual);
        // Round 15 wave 2 (lane trace2): virtual threads too (see the callee).
        // Round 15 wave 3 (lane trace4): not under served rows (they listed
        // every `Thread.run`).
        // TP16-9: a rule whose one-pass screen answered `false` would have
        // answered `false` from its own screen; an insertion changes the
        // trace, so the screens are dropped after one.
        if !served_rows
            && screens.is_none_or(|s| s.thread_run_middle)
            && insert_thread_run_middle_frames(shared, &mut cm, &mut trace, !platform_thread)
        {
            crate::runtime::stackwalker::cap_throwable_trace(&mut trace, max_depth);
            screens = None;
        }
        // Round 15 wave 1 (lane trace; TR6-1): `Class.forName0 (Native
        // Method)` below the loader / `<clinit>` frames it ran.
        if screens.is_none_or(|s| s.loader_middle)
            && insert_callback_native_middle_frames(shared, &mut cm, &mut trace)
        {
            crate::runtime::stackwalker::cap_throwable_trace(&mut trace, max_depth);
            screens = None;
        }
        // HotSpot leaves hidden frames out (`Method::is_hidden`, interpreter
        // round i1 wave 34): `--real-jdk`, and `--compatible` under
        // `VmConfig::compatible_hides_hidden_frames` (wave 48), before `depth`
        // is taken. Round 15 wave 3 hand-back (lane trace4): under
        // `--compatible` with that switch off, still the one `@Hidden` JDK
        // frame the served `Thread.run` rows exposed. Both decided in
        // `drop_mode_hidden_frames`.
        {
            let captured = trace.len();
            // TP16-9: `drop_mode_hidden_frames` drops nothing when its own
            // screen finds every frame decided visible (`--real-jdk`) or no
            // `Thread.runWith` frame (the `--compatible` arm).
            let hidden_quiet = screens.is_some_and(|s| {
                if shared.config.hides_hidden_frames() {
                    s.hidden_quiet
                } else {
                    !s.run_with
                }
            });
            let dropped = if hidden_quiet {
                0
            } else {
                drop_mode_hidden_frames(shared, &mut cm, &mut trace)
            };
            // HotSpot's depth counts the frames it RECORDS: a hidden frame
            // does not use up `MaxJavaStackTraceDepth` (measured,
            // `L6W37HiddenFramesDepthCap`; wave 37). A capture that was cut
            // at the cap and then lost hidden frames is redone whole, filtered,
            // and cut again. Only a trace at the cap that has hidden frames
            // pays for it.
            if dropped > 0 && max_depth != 0 && captured >= max_depth {
                memo_screens_quiet = false;
                // Interpreter round i6 wave 123, lane L3
                // (`CRATONVM_HIDDEN_METHOD_BITS`, default off): the redo in
                // growing windows, not whole
                // (`stackwalker::recapture_past_hidden_cut`). The same trace.
                // Wave 124 (L9 review): the switch is asked before the
                // class-manager guard is taken, so the switch-off arm takes
                // no lock the whole redo did not (a VM whose store has the
                // bits but whose thread does not see the variable only loses
                // the windows, never the answer).
                let bits_switch =
                    true;
                let windowed = bits_switch && {
                    let guard = cm.get_or_insert_with(|| shared.classes.class_manager.read());
                    crate::runtime::stackwalker::store_keeps_hidden_method_bits(
                        &guard.class_store,
                        throwable_class,
                    )
                };
                // `window` raw frames (`0`: all); the processed trace, uncut,
                // and how many raw frames the walk gave.
                let mut redo = |window: usize| {
                    let mut whole =
                        crate::runtime::stackwalker::capture_throwable_backtrace_without_store(
                            frames,
                            window,
                            &mut |holder: Option<ClassId>, holder_name: &str| {
                                throwable_capture_is_a(
                                    shared,
                                    &mut cm,
                                    throwable_class,
                                    holder,
                                    holder_name,
                                )
                            },
                            &splices,
                        );
                    let raw = whole.len();
                    if !served_rows {
                        insert_thread_run_middle_frames(
                            shared,
                            &mut cm,
                            &mut whole,
                            !platform_thread,
                        );
                    }
                    insert_callback_native_middle_frames(shared, &mut cm, &mut whole);
                    drop_mode_hidden_frames(shared, &mut cm, &mut whole);
                    (whole, raw)
                };
                trace = if windowed {
                    crate::runtime::stackwalker::recapture_past_hidden_cut(
                        max_depth,
                        captured,
                        captured - dropped,
                        &mut redo,
                    )
                } else {
                    let (mut whole, _) = redo(0);
                    crate::runtime::stackwalker::cap_throwable_trace(&mut whole, max_depth);
                    whole
                };
            }
        }
        let phase_ruled = phases.then(std::time::Instant::now);
        // Round 13 wave 13 (lane trace3): the JDK frames of a method the VM
        // served without pushing them (`Thread.sleepNanos0(Native Method)`...).
        // Round 14 wave 5 (lane trace4): on virtual threads too (their own rows).
        // TP16-6 (wave 9 form): a memo only when it appended nothing for a
        // reason the innermost frame and the throwable class decide -- the
        // decoded answer for a compiled innermost frame, not the lock-free
        // screen that refused every compiled throw.
        let memo_screens_quiet_before_standins = memo_screens_quiet;
        memo_screens_quiet &= append_native_standin_frames(
            shared,
            &mut cm,
            throwable_class,
            !platform_thread,
            timed_join,
            virtual_sleep,
            served_raise.as_deref(),
            hidden_marked.as_ref(),
            // Wave 63, lane L3: the door record's key (no hints); since wave
            // 64 the only door carrier, `Method.invoke`'s door included.
            door_key,
            &mut trace,
            max_depth,
            // JIT round 16 wave 11 (lane throw2, TP16-10).
            layout_cache
                .standin_site_memo_enabled()
                .then_some(&mut self.thread.throwable_standin_site_memo),
        );
        // Round 15 wave 2 (lane trace2): above those (a served `forName`'s
        // `forName0`), the loader frames of a served built-in loader's miss.
        append_served_loader_miss_frames(
            shared,
            &mut cm,
            throwable_class,
            loader_miss,
            &mut trace,
            max_depth,
        );
        // JIT round 16 wave 13 (lane wmte): a `MethodHandle` door's
        // `WrongMethodTypeException`, on top of its caller.
        append_mh_wmte_raise_frames(
            shared,
            &mut cm,
            throwable_class,
            mh_wmte,
            &mut trace,
            max_depth,
        );
        // Interpreter round i5 wave 103, lane L3: the served class-init
        // chain's JDK frames below a standing `<clinit>` (see above).
        // Wave 104, lane L3: innermost chain first, then each outer one.
        if init_chain_standing {
            let mut next = self.thread.served_native_raise.as_deref();
            while let Some(mark) = next {
                if let Some(clinit_class) = served_init_chain_clinit(frames, mark) {
                    insert_served_init_chain_capture_frames(
                        shared,
                        &mut cm,
                        mark,
                        clinit_class,
                        &mut trace,
                        max_depth,
                        init_chain_guard,
                    );
                }
                next = mark.outer.as_deref();
            }
        }
        // Interpreter round i5 wave 104 hand-back (`L3W56FieldHandleNullBase`
        // row `getter-with-arguments`): inside a served `invokeWithArguments`
        // call, the call's splice and a served-raise rule above may both list
        // `MethodHandle.invokeWithArguments`, next to each other; HotSpot has
        // the one frame (`stackwalker::dedupe_adjacent_invoke_with_arguments`).
        if self
            .thread
            .reflective_calls
            .iter()
            .any(|row| row.kind.is_invoke_with_arguments())
            && crate::runtime::stackwalker::dedupe_adjacent_invoke_with_arguments(&mut trace)
        {
            memo_screens_quiet = false;
        }
        if platform_thread && !served_rows {
            // Round 14 wave 3 (lane trace): `Thread.run` below a started
            // thread's task (round 15 wave 3, lane trace4: a thread with
            // served rows has it from its bottom row).
            let thread_obj = self.thread.java_thread_obj;
            prepend_thread_run_standin_frame(
                shared,
                &mut cm,
                thread_obj,
                &mut self.thread.thread_run_bottom_memo,
                thread_run_outermost.as_ref(),
                &mut trace,
                max_depth,
            );
        }
        // Round 15 wave 5 (lane trace5; TR4W3-7 of
        // `jit-r15w3-trace4-proposals-RETIRED-20261003.md`): the census TR4W3-3 needs before the
        // `Thread.run` rules retire. For a capture under served rows, the
        // `Thread.run` frames the rows listed and the ones the rules would
        // have built on the same frames (a side capture; debug only).
        if let Some(side_splices) = rules_side_splices.filter(|_| served_rows) {
            use crate::runtime::stackwalker::{
                is_thread_run_frame, may_be_thread_run_lambda_frame, may_be_thread_run_task_frame,
            };
            let mut side = crate::runtime::stackwalker::capture_throwable_backtrace_without_store(
                frames,
                max_depth,
                &mut |holder: Option<ClassId>, holder_name: &str| {
                    throwable_capture_is_a(shared, &mut cm, throwable_class, holder, holder_name)
                },
                &side_splices,
            );
            let side_outermost = side
                .first()
                .filter(|f| may_be_thread_run_task_frame(f) || may_be_thread_run_lambda_frame(f))
                .cloned();
            insert_thread_run_middle_frames(shared, &mut cm, &mut side, !platform_thread);
            if platform_thread {
                let mut side_memo: Option<crate::runtime::stackwalker::ThreadRunBottomMemo> = None;
                prepend_thread_run_standin_frame(
                    shared,
                    &mut cm,
                    self.thread.java_thread_obj,
                    &mut side_memo,
                    side_outermost.as_ref(),
                    &mut side,
                    max_depth,
                );
            }
            let listed = trace.iter().filter(|f| is_thread_run_frame(f)).count();
            let rules = side.iter().filter(|f| is_thread_run_frame(f)).count();
            eprintln!(
                "STTRACE_DBG_THREADRUN rows={} listed={listed} rules={rules} agree={}",
                self.thread.served_thread_run.len(),
                listed == rules
            );
        }
        drop(cm);
        // Interpreter round i4 wave 92, lane L2: under `-XX:+ShowHiddenFrames`
        // the hidden JDK frames HotSpot lists and this VM never pushed (a
        // lambda proxy's SAM frame, a served `MethodHandle` call's
        // `LambdaForm` frames). No class-manager guard is held here. A trace
        // that got some is not memoised (the memo's screens never saw them).
        if shared.config.show_hidden_frames
            && insert_show_hidden_standin_frames(shared, &mut trace, max_depth)
        {
            memo_screens_quiet = false;
        }
        if crate::runtime::env_cache::dbg_sttrace() {
            eprintln!(
                "STTRACE_DBG_CAP frames={} depth={}",
                self.thread.frames.len(),
                trace.len()
            );
            for (i, frame) in trace.iter().enumerate() {
                let e = frame.to_entry();
                eprintln!("  STTRACE_DBG_CAP[{i}] {}.{}", e.class_name, e.method_name);
            }
        }
        let depth = trace.len();
        // nio21 wave 15 L4: one shared allocation for the stored trace and
        // the memo made from it (`ThrowableCaptureMemo::new_shared`).
        let trace: std::sync::Arc<[crate::runtime::stackwalker::BacktraceFrame]> =
            std::sync::Arc::from(trace);
        // TP16-6: remember this capture for the next one on the same stack.
        let memo_walk_present = memo_compiled.is_some();
        if let (true, Some(key), Some(compiled)) = (memo_screens_quiet, memo_key, memo_compiled) {
            let made = crate::runtime::stackwalker::ThrowableCaptureMemo::new_shared(
                key,
                &self.thread.frames,
                compiled,
                memo_raw,
                std::sync::Arc::clone(&trace),
            )
            // JIT round 20 wave 12 (lane qMisc, L4W15-2): keyed by the
            // reflective splices it laid out (none: unchanged).
            .map(|memo| memo.with_splices(&splices));
            let was_made = made.is_some();
            if let Some(memo) = made {
                self.thread.throwable_capture_memo.remember(memo);
            }
            self.shared
                .threads
                .throwable_layout_cache
                .note_capture_memo(if was_made {
                    crate::vm::realms::thread_realm::CAPTURE_MEMO_MADE
                } else {
                    crate::vm::realms::thread_realm::CAPTURE_MEMO_REFUSED
                });
        } else if memo_key.is_some() {
            self.shared
                .threads
                .throwable_layout_cache
                .note_capture_memo(crate::vm::realms::thread_realm::CAPTURE_MEMO_REFUSED);
            if self
                .shared
                .threads
                .throwable_layout_cache
                .capture_memo_refusal_report_due()
            {
                eprintln!(
                    "[capture-memo] refused: screens_quiet={memo_screens_quiet_before_standins} \
                     standins_quiet={} walk={} trace=[{}]",
                    memo_screens_quiet || !memo_screens_quiet_before_standins,
                    memo_walk_present,
                    trace
                        .iter()
                        .map(|f| {
                            let e = f.to_entry();
                            format!("{}.{}", e.class_name, e.method_name)
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                );
            }
        }
        let phase_stored_from = phases.then(std::time::Instant::now);
        // Interpreter round i3 wave 82, lane L2: into a throwable-owned holder
        // under `CRATONVM_THROWABLE_BACKTRACE_IN_OBJECT=1`, else the registry.
        self.shared
            .retain_captured_backtrace_shared(throwable, trace);
        // TF17-0: the phase census. A memo-path capture has no separate walk
        // mark; its walk is counted with its layout.
        if let (Some(start), Some(laid_out), Some(ruled), Some(stored_from)) =
            (phase_start, phase_laid_out, phase_ruled, phase_stored_from)
        {
            let walked = phase_walked.unwrap_or(start);
            let ns = |from: std::time::Instant, to: std::time::Instant| {
                u64::try_from(to.saturating_duration_since(from).as_nanos()).unwrap_or(u64::MAX)
            };
            let end = std::time::Instant::now();
            layout_cache.note_throwfill_phases(
                [
                    ns(start, walked),
                    ns(walked, laid_out),
                    ns(laid_out, ruled),
                    ns(ruled, stored_from),
                    ns(stored_from, end),
                ],
                depth,
            );
        }
        depth
    }
}

impl<'a> NativeGpuAccess for NativeContextImpl<'a> {
    /// Phase 5: override the GPU dispatch escape hatch. Delegates
    /// to `crate::runtime::offload::dispatch_method_from_native`
    /// when the gpu-offload feature is on; otherwise returns None
    /// (the trait's default).
    fn gpu_dispatch_method(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        java_args: &[Value],
    ) -> Option<u64> {
        #[cfg(feature = "gpu-offload")]
        {
            Some(crate::runtime::offload::dispatch_method_from_native(
                self.shared,
                class_name,
                method_name,
                descriptor,
                java_args,
            ))
        }
        #[cfg(not(feature = "gpu-offload"))]
        {
            let _ = (class_name, method_name, descriptor, java_args);
            None
        }
    }

    /// GpuStream affinity — override the stream-mint escape hatch.
    /// Delegates to `OffloadCache::stream_create` on the per-VM
    /// default-ordinal cache when the gpu-offload feature is on;
    /// otherwise returns `None` (the trait's default).
    fn gpu_stream_create(&mut self) -> Option<u64> {
        #[cfg(feature = "gpu-offload")]
        {
            self.shared
                .offload_registry
                .get_or_create(self.shared.config.gpu_device_ordinal, &self.shared.config)
                .stream_create()
        }
        #[cfg(not(feature = "gpu-offload"))]
        {
            None
        }
    }

    /// GpuStream affinity — override the stream-release escape hatch.
    /// Delegates to `OffloadCache::stream_release`; a no-op when the
    /// gpu-offload feature is off (the trait's default already
    /// covers that, but the cache lookup itself is guarded the same
    /// way every other GPU override in this impl is).
    fn gpu_stream_release(&mut self, handle: u64) {
        #[cfg(feature = "gpu-offload")]
        {
            self.shared
                .offload_registry
                .get_or_create(self.shared.config.gpu_device_ordinal, &self.shared.config)
                .stream_release(handle);
        }
        #[cfg(not(feature = "gpu-offload"))]
        {
            let _ = handle;
        }
    }

    fn gpu_stream_synchronize(&mut self, handle: u64) -> Result<(), String> {
        #[cfg(feature = "gpu-offload")]
        {
            self.shared
                .offload_registry
                .get_or_create(self.shared.config.gpu_device_ordinal, &self.shared.config)
                .stream_synchronize(handle)
        }
        #[cfg(not(feature = "gpu-offload"))]
        {
            let _ = handle;
            Ok(())
        }
    }

    fn gpu_promote_ephemeral_array(
        &mut self,
        future_handle: u64,
        new_array_handle: u64,
    ) -> Option<(cratonvm_native_api::registry::DeviceArrayElem, usize)> {
        #[cfg(feature = "gpu-offload")]
        {
            crate::runtime::offload::promote_submission_device_array(
                self.shared,
                future_handle,
                new_array_handle,
            )
        }
        #[cfg(not(feature = "gpu-offload"))]
        {
            let _ = (future_handle, new_array_handle);
            None
        }
    }

    /// CUDA graphs — start recording launches on `stream_handle` instead
    /// of issuing them. `false` on a driverless VM or a refused capture.
    fn gpu_graph_begin_capture(&mut self, stream_handle: u64) -> bool {
        #[cfg(feature = "gpu-offload")]
        {
            self.shared
                .offload_registry
                .get_or_create(self.shared.config.gpu_device_ordinal, &self.shared.config)
                .graph_begin_capture(stream_handle)
        }
        #[cfg(not(feature = "gpu-offload"))]
        {
            let _ = stream_handle;
            false
        }
    }

    /// CUDA graphs — stop recording and instantiate. `0` for every
    /// failure; zero is never a valid handle.
    fn gpu_graph_end_capture(&mut self, stream_handle: u64) -> u64 {
        #[cfg(feature = "gpu-offload")]
        {
            self.shared
                .offload_registry
                .get_or_create(self.shared.config.gpu_device_ordinal, &self.shared.config)
                .graph_end_capture(stream_handle)
        }
        #[cfg(not(feature = "gpu-offload"))]
        {
            let _ = stream_handle;
            0
        }
    }

    /// CUDA graphs — one `cuGraphLaunch` of everything the graph recorded.
    fn gpu_graph_replay(&mut self, stream_handle: u64, graph_handle: u64) -> u64 {
        #[cfg(feature = "gpu-offload")]
        {
            self.shared
                .offload_registry
                .get_or_create(self.shared.config.gpu_device_ordinal, &self.shared.config)
                .graph_replay(stream_handle, graph_handle)
        }
        #[cfg(not(feature = "gpu-offload"))]
        {
            let _ = (stream_handle, graph_handle);
            0
        }
    }

    /// CUDA graphs — open an argument-update pass over a captured graph.
    fn gpu_graph_begin_replay(&mut self, stream_handle: u64, graph_handle: u64) -> bool {
        #[cfg(feature = "gpu-offload")]
        {
            self.shared
                .offload_registry
                .get_or_create(self.shared.config.gpu_device_ordinal, &self.shared.config)
                .graph_begin_replay(stream_handle, graph_handle)
        }
        #[cfg(not(feature = "gpu-offload"))]
        {
            let _ = (stream_handle, graph_handle);
            false
        }
    }

    /// CUDA graphs — close the pass and submit the graph once.
    fn gpu_graph_end_replay(&mut self, stream_handle: u64) -> u64 {
        #[cfg(feature = "gpu-offload")]
        {
            self.shared
                .offload_registry
                .get_or_create(self.shared.config.gpu_device_ordinal, &self.shared.config)
                .graph_end_replay(stream_handle)
        }
        #[cfg(not(feature = "gpu-offload"))]
        {
            let _ = stream_handle;
            0
        }
    }

    /// CUDA graphs — node count, or `-1` for an unknown handle.
    fn gpu_graph_node_count(&self, graph_handle: u64) -> i32 {
        #[cfg(feature = "gpu-offload")]
        {
            self.shared
                .offload_registry
                .get_or_create(self.shared.config.gpu_device_ordinal, &self.shared.config)
                .graph_node_count(graph_handle)
        }
        #[cfg(not(feature = "gpu-offload"))]
        {
            let _ = graph_handle;
            -1
        }
    }

    /// CUDA graphs — free a graph. Idempotent, like every other release.
    fn gpu_graph_release(&mut self, graph_handle: u64) {
        #[cfg(feature = "gpu-offload")]
        {
            self.shared
                .offload_registry
                .get_or_create(self.shared.config.gpu_device_ordinal, &self.shared.config)
                .graph_release(graph_handle);
        }
        #[cfg(not(feature = "gpu-offload"))]
        {
            let _ = graph_handle;
        }
    }

    /// GpuStream affinity — stream-affine sibling of
    /// `gpu_dispatch_method`. Delegates to
    /// `crate::runtime::offload::dispatch_method_from_native_on_stream`
    /// (same class/method resolution + marshalling as
    /// `gpu_dispatch_method`, plus the `stream_handle` resolution
    /// step) when the gpu-offload feature is on; otherwise returns
    /// `None` (the trait's default, which itself falls back to
    /// `gpu_dispatch_method`).
    fn gpu_dispatch_method_on_stream(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        java_args: &[Value],
        stream_handle: Option<u64>,
    ) -> Option<u64> {
        #[cfg(feature = "gpu-offload")]
        {
            Some(
                crate::runtime::offload::dispatch_method_from_native_on_stream(
                    self.shared,
                    class_name,
                    method_name,
                    descriptor,
                    java_args,
                    stream_handle,
                ),
            )
        }
        #[cfg(not(feature = "gpu-offload"))]
        {
            let _ = (
                class_name,
                method_name,
                descriptor,
                java_args,
                stream_handle,
            );
            None
        }
    }

    /// Phase 6 #4 (2026-07-11: switched to the non-blocking poll) —
    /// query the real GPU submission registry for `handle`'s completion
    /// state.
    ///
    /// Previously a pure `sub.status.lock()` peek: it read whatever
    /// `SubmissionStatus` the submission already carried but never
    /// asked the device anything, so it could never observe a
    /// completion that some other caller (`futureSynchronize`/`get()`)
    /// hadn't already finalized — `futureIsDone` stayed `false` forever
    /// for a submission nobody was blocking on. Now delegates to
    /// `runtime::offload::poll_submission_status`, which finalizes the
    /// submission inline (bounded work only — by the time it does, the
    /// device event has already fired) the first time it observes the
    /// device-side work as done. `Native.futureIsDone`/`futureStatus`
    /// (`native-builtins/src/craton_gpu.rs`) are therefore now
    /// genuinely non-blocking-but-live rather than only ever reporting
    /// stale `Running`.
    fn gpu_future_status(&self, handle: u64) -> Option<i32> {
        #[cfg(feature = "gpu-offload")]
        {
            let outcome = crate::runtime::offload::poll_submission_status(self.shared, handle)?;
            Some(match outcome {
                crate::runtime::offload::PollOutcome::Running => 0,
                crate::runtime::offload::PollOutcome::Completed => 1,
                crate::runtime::offload::PollOutcome::Failed => 2,
            })
        }
        #[cfg(not(feature = "gpu-offload"))]
        {
            let _ = handle;
            None
        }
    }

    /// 2026-07-11 — `NativeContext::gpu_future_take_result` override.
    /// See the trait doc comment (`native-api/src/registry.rs`) for the
    /// full contract; this is the read half that hands back a completed
    /// submission's `SerializedResult` (translated into the
    /// `native-api`-side `GpuFutureResult` transport enum, since
    /// `native-api` cannot depend on this crate's `offload` types).
    ///
    /// Reuses `poll_submission_status` — the same non-blocking,
    /// finalize-only-if-the-device-already-reported-done path
    /// `gpu_future_status` now uses — so a caller that already observed
    /// `isDone() == true` gets the result with no wait, and a caller
    /// that hasn't gets `None` rather than an implicit block.
    fn gpu_future_take_result(
        &self,
        handle: u64,
    ) -> Option<cratonvm_native_api::registry::GpuFutureResult> {
        #[cfg(feature = "gpu-offload")]
        {
            use crate::runtime::offload::{PollOutcome, SerializedResult, SubmissionStatus};

            // Only a terminal `Completed` outcome carries a result;
            // `Running` and `Failed` (and an unknown handle, which
            // `poll_submission_status` already reports as `None`) don't.
            // This never blocks: `poll_submission_status` only ever
            // finalizes inline when the device has already reported the
            // work done, never by waiting for it to.
            if crate::runtime::offload::poll_submission_status(self.shared, handle)?
                != PollOutcome::Completed
            {
                return None;
            }
            let sub = crate::runtime::offload::lookup_submission(handle)?;
            let status = sub.status.lock();
            match &*status {
                SubmissionStatus::Completed { result } => Some(match result {
                    SerializedResult::Void => cratonvm_native_api::registry::GpuFutureResult::Void,
                    SerializedResult::ScalarI32(v) => {
                        cratonvm_native_api::registry::GpuFutureResult::ScalarI32(*v)
                    }
                    SerializedResult::ScalarI64(v) => {
                        cratonvm_native_api::registry::GpuFutureResult::ScalarI64(*v)
                    }
                    SerializedResult::ScalarF32(v) => {
                        cratonvm_native_api::registry::GpuFutureResult::ScalarF32(*v)
                    }
                    SerializedResult::ScalarF64(v) => {
                        cratonvm_native_api::registry::GpuFutureResult::ScalarF64(*v)
                    }
                    SerializedResult::DeviceArray {
                        array_handle,
                        elem,
                        len,
                    } => cratonvm_native_api::registry::GpuFutureResult::DeviceArray {
                        array_handle: *array_handle,
                        elem: *elem,
                        len: *len,
                    },
                    // `finalize_submission` never actually constructs one
                    // of these today: array outputs are copied back to
                    // the caller's own Java array via `MarshalWriteback`
                    // writeback entries (drained by `finalize_submission`
                    // itself), not stamped into `SerializedResult` — its
                    // `scalar_result` local is only ever set by the
                    // single Part-E scalar writeback kind, so a
                    // `Completed` status here only ever holds `Void` or a
                    // `Scalar*` variant in practice. These arms are
                    // therefore unreachable today; map to `Void`
                    // defensively rather than panic if that ever changes.
                    SerializedResult::PrimitiveArrayI32 { .. }
                    | SerializedResult::PrimitiveArrayI64 { .. }
                    | SerializedResult::PrimitiveArrayF32 { .. }
                    | SerializedResult::PrimitiveArrayF64 { .. } => {
                        cratonvm_native_api::registry::GpuFutureResult::Void
                    }
                }),
                // `poll_submission_status` just reported `Completed`
                // under its own lock acquisition; this re-locks and
                // should see the same terminal state (a submission never
                // regresses out of a terminal status). Stay defensive
                // rather than assume that invariant instead of checking it.
                _ => None,
            }
        }
        #[cfg(not(feature = "gpu-offload"))]
        {
            let _ = handle;
            None
        }
    }

    /// Phase 8 #1: evict the device-buffer cache entry for a
    /// GpuArray handle. Called from `Native.releaseArray` when Java
    /// drops the wrapper.
    fn gpu_release_array_cache(&mut self, handle: u64) {
        #[cfg(feature = "gpu-offload")]
        {
            crate::runtime::offload::device_cache::release(handle);
        }
        #[cfg(not(feature = "gpu-offload"))]
        {
            let _ = handle;
        }
    }

    /// Phase 10 #1: wipe the explicit-submit input-residency
    /// cache. Called from `Native.releaseExecutor` so device
    /// buffers cached for plain JVM primitive arrays are freed when
    /// the Java `GpuExecutor` is closed.
    fn gpu_release_submission(&mut self, handle: u64) {
        #[cfg(feature = "gpu-offload")]
        {
            // Kill switch: the drain is new and default-on, and its
            // absence is what the old behaviour was. See
            // `CRATONVM_GPU_NO_SUBMISSION_DRAIN`.
            use std::sync::OnceLock;
            static ENABLED: OnceLock<bool> = OnceLock::new();
            let on = *ENABLED.get_or_init(|| {
                None::<std::ffi::OsString>.is_none()
            });
            if on {
                crate::runtime::offload::release_submission(handle);
            }
        }
        #[cfg(not(feature = "gpu-offload"))]
        {
            let _ = handle;
        }
    }

    fn gpu_clear_input_cache(&mut self) {
        #[cfg(feature = "gpu-offload")]
        {
            crate::runtime::offload::input_cache::clear_all(self.shared.vm_identity);
        }
    }

    /// Phase 9 #1: materialise dirty device-side bytes into a
    /// host buffer. The caller (the `Native.arrayToHost` shim)
    /// stamps the returned bytes into the resident store before
    /// rebuilding the Java array. None means "no download
    /// needed" — either the entry is unknown or already-clean.
    /// Phase 7 #2 counterpart of `gpu_array_download_if_dirty`: write
    /// host bytes through a resident array's existing device pointer.
    fn gpu_array_upload_bytes(&self, handle: u64, bytes: &[u8]) -> Option<bool> {
        #[cfg(feature = "gpu-offload")]
        {
            crate::runtime::offload::device_cache::upload_from_bytes(handle, bytes)
        }
        #[cfg(not(feature = "gpu-offload"))]
        {
            let _ = (handle, bytes);
            None
        }
    }

    fn gpu_array_download_if_dirty(&self, handle: u64) -> Option<Vec<u8>> {
        #[cfg(feature = "gpu-offload")]
        {
            crate::runtime::offload::device_cache::download_into_bytes_if_dirty(handle)
        }
        #[cfg(not(feature = "gpu-offload"))]
        {
            let _ = handle;
            None
        }
    }

    /// Phase 6 #5 + Phase 9 #2 push 2: resolve a lambda proxy back to
    /// its target method + captured values for GPU dispatch.
    ///
    /// Non-static targets (`InvokeVirtual` / `InvokeSpecial`) are
    /// admitted when the captured-value list begins with a non-null
    /// receiver — the receiver becomes `java_args[0]` to
    /// [`dispatch_method_from_native`], which extracts the analyzer's
    /// recorded `this_field_cps` from the receiver before processing
    /// remaining captures as kernel args.
    ///
    /// Still rejected:
    /// - `InvokeInterface` (no fixture today)
    /// - `NewInvokeSpecial` (heap construction on the device — no GPU
    ///   semantics)
    /// - `GetField` / `PutField` / `GetStatic` / `PutStatic` method
    ///   handles (no GPU semantics for arbitrary field access)
    fn gpu_resolve_lambda_target(
        &self,
        callable: ObjectRef,
    ) -> Option<(String, String, String, Vec<Value>)> {
        #[cfg(feature = "gpu-offload")]
        {
            use crate::classloading::resolution::MethodHandleKind;
            let cid = self.shared.mem.heap.class_id_of(callable);
            let proxies = self.shared.classes.lambda_proxies.read();
            let lcs = proxies.get(&cid)?;
            // Phase 9 #2 — admit InvokeStatic, InvokeVirtual, and
            // InvokeSpecial. The analyzer's non-static relaxation
            // means a virtual/special call's target can be admitted
            // when its body uses the `aload_0; getfield <field>`
            // shape; the marshaller extracts those fields from the
            // receiver. Other handle kinds remain CPU-only.
            let kind_admitted = matches!(
                lcs.impl_handle.kind,
                MethodHandleKind::InvokeStatic
                    | MethodHandleKind::InvokeVirtual
                    | MethodHandleKind::InvokeSpecial
            );
            if !kind_admitted {
                // See the sibling site in `runtime/offload.rs`: `debug!` is
                // compiled out in release, so this decision was
                // unreachable by any RUST_LOG directive.
                tracing::info!(
                    target: "gpu.offload",
                    handle_kind = ?lcs.impl_handle.kind,
                    target_class = %lcs.impl_handle.class_name,
                    target_member = %lcs.impl_handle.member_name,
                    "lambda target rejected: handle kind has no GPU lowering",
                );
                return None;
            }
            // Merge bridge: `impl_handle` fields are `Arc<str>` in the
            // current reader; the GPU lambda-resolver tuple is typed
            // `String`. Convert explicitly.
            let class_name = lcs.impl_handle.class_name.to_string();
            let member_name = lcs.impl_handle.member_name.to_string();
            let descriptor = lcs.impl_handle.descriptor.to_string();
            let n_captures = lcs.capture_types.len();
            drop(proxies);
            let mut captures = Vec::with_capacity(n_captures);
            for i in 0..n_captures {
                captures.push(self.shared.mem.heap.get_field(callable, i));
            }
            Some((class_name, member_name, descriptor, captures))
        }
        #[cfg(not(feature = "gpu-offload"))]
        {
            let _ = callable;
            None
        }
    }

    /// Phase 6 #4 + Phase 7 #1: block on the GPU submission and
    /// run its deferred writebacks. If the submission still carries
    /// a FinalizeState (the kernel may still be running on the GPU),
    /// this is the call that:
    ///   1. waits on the recorded event,
    ///   2. drains all the device-to-host writebacks,
    ///   3. drops the SafepointToken (releases GC).
    /// Idempotent — subsequent calls find FinalizeState already
    /// taken and return the cached terminal status.
    fn gpu_future_synchronize(&self, handle: u64) -> Option<Result<(), String>> {
        #[cfg(feature = "gpu-offload")]
        {
            let sub = crate::runtime::offload::lookup_submission(handle)?;
            Some(crate::runtime::offload::finalize_submission(
                self.shared,
                &sub,
            ))
        }
        #[cfg(not(feature = "gpu-offload"))]
        {
            let _ = handle;
            None
        }
    }

    /// The failure category recorded when the submission failed.
    ///
    /// Read straight off the submission — whichever code path failed
    /// stamped the category at that moment, so nothing here has to infer
    /// it from the message.
    #[allow(clippy::too_many_arguments)]
    fn gpu_dispatch_gemm(
        &mut self,
        half: bool,
        a_handle: u64,
        b_handle: u64,
        c_handle: u64,
        m: i32,
        n: i32,
        k: i32,
        trans_a: bool,
        trans_b: bool,
        stream_handle: Option<u64>,
    ) -> Option<u64> {
        #[cfg(feature = "gpu-offload")]
        {
            use crate::runtime::kernels::GemmKind;
            let kind = if half { GemmKind::F16 } else { GemmKind::F32 };
            Some(crate::runtime::offload::dispatch_gemm(
                self.shared,
                kind,
                a_handle,
                b_handle,
                c_handle,
                m,
                n,
                k,
                trans_a,
                trans_b,
                stream_handle,
            ))
        }
        #[cfg(not(feature = "gpu-offload"))]
        {
            let _ = (half, a_handle, b_handle, c_handle, m, n, k);
            let _ = (trans_a, trans_b, stream_handle);
            None
        }
    }

    fn gpu_future_error_kind(
        &self,
        handle: u64,
    ) -> Option<cratonvm_native_api::registry::GpuErrorKind> {
        #[cfg(feature = "gpu-offload")]
        {
            crate::runtime::offload::submission_error_kind(handle)
        }
        #[cfg(not(feature = "gpu-offload"))]
        {
            let _ = handle;
            None
        }
    }

    /// Timed wait on a GPU submission, run entirely on this side of the
    /// native boundary.
    fn gpu_future_await(&self, handle: u64, timeout_nanos: u64) -> i32 {
        #[cfg(feature = "gpu-offload")]
        {
            crate::runtime::offload::await_submission(self.shared, handle, timeout_nanos)
        }
        #[cfg(not(feature = "gpu-offload"))]
        {
            let _ = (handle, timeout_nanos);
            cratonvm_native_api::registry::GPU_AWAIT_UNSUPPORTED
        }
    }

    /// GPU device enumeration: delegate to the real CUDA bridge probe.
    ///
    /// `cuda_bridge::probe()` reports the primary device (ordinal 0) or
    /// `Err(DeviceError::NoDriver)` when no driver is present or the
    /// bridge was built in stub mode. We translate `Ok(caps)` into a
    /// single-entry list and any `Err` into an empty list — so the
    /// `Native.deviceCount` shim returns a truthful `0` on a driverless
    /// host rather than fabricating a device.
    fn gpu_device_info(&self) -> Vec<(String, u32, u32, u64)> {
        #[cfg(feature = "gpu-offload")]
        {
            match cuda_bridge::probe() {
                Ok(caps) => vec![(
                    caps.name,
                    caps.compute_major,
                    caps.compute_minor,
                    caps.total_global_mem,
                )],
                Err(_) => Vec::new(),
            }
        }
        #[cfg(not(feature = "gpu-offload"))]
        {
            Vec::new()
        }
    }

    fn gpu_chain_submission(
        &self,
        pred_handle: u64,
        class_name: &str,
        method_name: &str,
        method_desc: &str,
        args: &[Value],
    ) -> Result<u64, String> {
        #[cfg(feature = "gpu-offload")]
        {
            crate::runtime::offload::chain_submission_from_native(
                self.shared,
                pred_handle,
                class_name,
                method_name,
                method_desc,
                args,
            )
        }
        #[cfg(not(feature = "gpu-offload"))]
        {
            let _ = (pred_handle, class_name, method_name, method_desc, args);
            Err("gpu-offload feature is disabled".to_string())
        }
    }

    fn gpu_release_ephemeral_array_after_enqueue(&self, array_handle: u64) {
        #[cfg(feature = "gpu-offload")]
        {
            if array_handle != 0 {
                crate::runtime::offload::release_ephemeral_array_after_enqueue(array_handle);
            }
        }
        #[cfg(not(feature = "gpu-offload"))]
        {
            let _ = array_handle;
        }
    }

    fn gpu_submission_stream_handle(&self, submission_handle: u64) -> Option<u64> {
        #[cfg(feature = "gpu-offload")]
        {
            if submission_handle == 0 {
                None
            } else {
                crate::runtime::offload::submission_stream_handle(submission_handle)
            }
        }
        #[cfg(not(feature = "gpu-offload"))]
        {
            let _ = submission_handle;
            None
        }
    }

    fn gpu_submission_retained_vram(&self, submission_handle: u64) -> u64 {
        #[cfg(feature = "gpu-offload")]
        {
            if submission_handle == 0 {
                0
            } else {
                crate::runtime::offload::submission_retained_vram(submission_handle)
            }
        }
        #[cfg(not(feature = "gpu-offload"))]
        {
            let _ = submission_handle;
            0
        }
    }

    fn gpu_note_retained_vram(&mut self, delta_bytes: i64, wrapper_delta: i64) -> bool {
        self.shared
            .mem
            .heap
            .note_external_gpu_retained(delta_bytes, wrapper_delta)
    }
}

/// Native-library indices retired by `NativeContext::unload_native_library`
/// (`RawNativeLibraries.unload0`), keyed by `(NativeRealm address, index)`.
///
/// `SharedVm.natives.native_libraries` is an append-only `Vec<Library>`: the
/// index IS the handle (`find_native_symbol` indexes it positionally and
/// `RawNativeLibraries.load0` hands out `index + 1` as the opaque Java-side
/// handle), so an element can never be removed without renumbering every live
/// handle. Unload is therefore recorded out-of-band as a tombstone, and
/// `find_native_symbol` refuses tombstoned indices — the library stays mapped
/// (see the `unload_native_library` contract: pointers handed out earlier may
/// still be live) but is no longer reachable through its handle.
///
/// Keying by the realm's address rather than the bare index keeps concurrently
/// live VMs in one process from seeing each other's tombstones. An address can
/// be reused by a *later* VM, so `load_native_library` clears the tombstone for
/// every index it hands out; since indices are assigned densely from 0, any
/// index a new realm can observe has been cleared by the load that created it.
/// The set is bounded by the number of unloaded libraries (typically zero).
pub(in crate::vm::vm_exec) fn unloaded_native_libraries() -> &'static std::sync::Mutex<std::collections::HashSet<(usize, i64)>>
{
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static UNLOADED: OnceLock<Mutex<HashSet<(usize, i64)>>> = OnceLock::new();
    UNLOADED.get_or_init(|| Mutex::new(HashSet::new()))
}

/// mcs21 review (lazy JNI link): the indices of `natives`' libraries that
/// `RawNativeLibraries.unload0` retired ([`unloaded_native_libraries`]), for
/// `jni::resolve_jni_native_in_libraries`, which used to iterate the whole
/// table and so bound a `Java_*` symbol out of a library the Java side had
/// already unloaded (the NOTE on `unload_native_library` recorded it as "not
/// changed here"). Snapshotted by the caller BEFORE it takes the library lock,
/// as `find_native_symbol` does, so the tombstone mutex is never nested inside
/// it. Keyed by the realm's address exactly as
/// [`NativeContextImpl::native_realm_key`]. Empty in every run that never
/// unloads.
pub(crate) fn unloaded_native_library_indices(
    natives: &crate::vm::realms::NativeRealm,
) -> Vec<usize> {
    let realm = std::ptr::from_ref(natives) as usize;
    let set = unloaded_native_libraries()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    set.iter()
        .filter(|(r, _)| *r == realm)
        .filter_map(|(_, idx)| usize::try_from(*idx).ok())
        .collect()
}


impl<'a> NativeContextImpl<'a> {
    /// Identity of this VM's [`NativeRealm`](crate::vm::realms::native_realm::NativeRealm)
    /// for [`unloaded_native_libraries`] keying. The realm is owned by the
    /// `SharedVm` and never moves while the VM is alive, so its address is a
    /// stable per-VM key for the lifetime of any handle derived from it.
    pub(in crate::vm::vm_exec) fn native_realm_key(&self) -> usize {
        std::ptr::from_ref(&self.shared.natives) as usize
    }
}

/// The most lines a thread's `printed_lines` keeps
/// ([`record_printed_line_bounded`]).
pub(crate) const PRINTED_LINES_CAP: usize = 1 << 16;

/// Append `text` to a thread's `printed_lines`, keeping at most
/// [`PRINTED_LINES_CAP`] lines: on reaching the cap the oldest half is
/// dropped (amortised O(1) per line).
///
/// The `--compatible` `println` / `print` stand-ins record every line here
/// before writing it; the only readers are the in-process TCK harness
/// (`runtime/tck.rs`, after `main` returns) and tests, all of which print far
/// fewer lines than the cap. Unbounded, a long-running `--compatible` process
/// that logs through `System.out` kept a copy of every line it printed
/// (`docs/feature-designs/real-jdk-mode.md`).
/// `CRATONVM_PRINTED_LINES_BOUND=0` restores the unbounded buffer; the switch
/// is read only when a buffer reaches the cap, never per line.
pub(crate) fn record_printed_line_bounded(lines: &mut Vec<String>, text: String) {
    if lines.len() >= PRINTED_LINES_CAP
        && true
    {
        lines.drain(..PRINTED_LINES_CAP / 2);
    }
    lines.push(text);
}

