impl DeoptVerifier {
    /// A verifier with no data registered: structural lane only.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register the bytecode limits of one method (the root method or any
    /// inlined callee whose scopes appear in the metadata).
    pub fn with_method(mut self, limits: MethodFrameLimits) -> Self {
        self.methods.insert(limits.method_key.clone(), limits);
        self
    }

    /// Register the oop map covering the safepoint at `native_offset`.
    pub fn with_oop_map(mut self, native_offset: u32, coverage: OopCoverage) -> Self {
        self.oop_coverage.insert(native_offset, coverage);
        self
    }

    /// Register IR node ids the optimizer removed. A slot that names one of
    /// these without carrying a rebuild recipe is rejected.
    pub fn with_removed_nodes(mut self, nodes: impl IntoIterator<Item = u32>) -> Self {
        self.removed_nodes.extend(nodes);
        self
    }

    /// Register removed node ids the emitter *can* rebuild (the scalar
    /// replacement map's keys). These are the removed nodes a
    /// `VirtualObject` slot is allowed to name.
    pub fn with_materializable_nodes(mut self, nodes: impl IntoIterator<Item = u32>) -> Self {
        self.materializable_nodes.extend(nodes);
        self
    }

    /// Require every deopt point carrying reference slots to have a registered
    /// oop map. Off by default, because a backend that has not wired precise
    /// maps yet publishes none at all and its frames are handled
    /// conservatively — flagging every point there would be noise, not a
    /// finding.
    ///
    /// No production caller today (JIT round 22, lane throws, re-checked):
    /// both backends leave it `false` because their maps are conservative,
    /// and the single-pass backend registers no oop map at all (its
    /// `native_offset` coordinates differ from its safepoint maps'; see
    /// `x64::driver`'s "oop-map agreement -- not registered" note). Turn it on
    /// for a backend whose maps are complete AND anchored, together with
    /// proposal 1 of `docs/feature-designs/deopt-osr.md`.
    pub fn requiring_oop_map(mut self, require: bool) -> Self {
        self.require_oop_map = require;
        self
    }

    /// Verify `points`, returning every violation found (never short-circuits:
    /// the second violation is usually the one that explains the first).
    ///
    /// Never panics and never mutates — same contract as
    /// `ir_verify::verify_graph`, and for the same reason: this is the
    /// component that must survive input it was not designed for.
    pub fn violations(&self, points: &[DeoptimizationPoint]) -> Vec<DeoptMetadataError> {
        let mut out = Vec::new();

        for w in points.windows(2) {
            if w[0].native_offset > w[1].native_offset {
                out.push(DeoptMetadataError::DeoptPointsUnsorted {
                    first: w[0].native_offset,
                    second: w[1].native_offset,
                });
            }
        }

        for point in points {
            self.check_point(point, &mut out);
        }
        out
    }

    /// [`Self::violations`], as a compile step: `Ok(())` or a [`Bailout`]
    /// listing what was wrong.
    ///
    /// A failure means the artifact must NOT be installed. Bailing is always
    /// semantically valid — the method takes the interpreter or the
    /// single-pass backend — whereas installing code whose deopt cannot be
    /// reconstructed is not.
    pub fn verify(&self, points: &[DeoptimizationPoint]) -> CompileResult<()> {
        let errors = self.violations(points);
        if errors.is_empty() {
            return Ok(());
        }
        Err(deopt_metadata_bailout(&errors))
    }

    // ── per-point ────────────────────────────────────────────────────

    fn check_point(&self, point: &DeoptimizationPoint, out: &mut Vec<DeoptMetadataError>) {
        // A `PendingException` frame deliberately carries the *throwing* bci and
        // a post-pop operand stack — it is not a resume point (see
        // `DeoptReason::PendingException`), so the point-vs-frame bci identity
        // still holds and is checked, but nothing below assumes resumability.
        if point.bci != point.frame_state.bci {
            out.push(DeoptMetadataError::PointBciMismatch {
                native_offset: point.native_offset,
                point_bci: point.bci,
                frame_bci: point.frame_state.bci,
            });
        }

        // ── exception-state agreement ────────────────────────────────
        //
        // `reason` and `semantics.rethrow_exception` answer the same question
        // for two different consumers: `reason` is the CHARGING key (the deopt
        // log, the de-speculation lookups, `recommend_action`), while every
        // routing and resume decision reads `semantics` — both trampoline
        // entries, `ordinary_stash_frame`, `OsrEntryPlan::resume_after_exit`,
        // `osr_exit_policy` and the OSR exception-exit transfer. This lane is
        // what keeps the charging key and the routing flag from parting
        // company. If they disagree, one of them treats
        // an exceptional frame as a resume point and executes past a call that
        // never returned — which is the `finally`-not-run / leaked-`athrow`-bci
        // defect shape, restated in metadata. Both directions are checked: a
        // `PendingException` point that forgot the flag, and a flagged point
        // whose reason will route it into `LAST_DEOPT`.
        let pending = point.reason == DeoptReason::PendingException;
        if pending != point.semantics.rethrow_exception {
            out.push(DeoptMetadataError::ResumeSemanticsMismatch {
                native_offset: point.native_offset,
                reason: point.reason,
                rethrow: point.semantics.rethrow_exception,
            });
        }

        let coverage = self.oop_coverage.get(&point.native_offset);
        let mut scope: Option<&FrameState> = Some(&point.frame_state);
        let mut depth = 0usize;
        let mut chain_ids = ChainVirtualIds::default();
        while let Some(state) = scope {
            // Bounded exactly like the other walk in this module
            // (`frame_state_is_resumable`):
            // a chain this deep is a metadata defect, and checking only its
            // first `MAX_SCOPE_CHAIN` scopes would report "clean" for a frame
            // whose outer scopes were never looked at.
            if depth >= MAX_SCOPE_CHAIN {
                out.push(DeoptMetadataError::ScopeChainTooDeep {
                    native_offset: point.native_offset,
                    cap: MAX_SCOPE_CHAIN,
                });
                break;
            }
            self.check_scope(point, state, depth, coverage, &mut chain_ids, out);
            scope = state.caller.as_deref();
            depth += 1;
        }
        // Virtual-object ids are CHAIN-wide (round 13 wave 5, lane resume): a
        // chain resolves every scope with one `emitted` set, so a reference
        // may name an object another scope defines, and the VM materialises
        // the whole chain as one graph. A flat frame is one scope: unchanged.
        for (at, id) in chain_ids.referenced {
            if !chain_ids.defined.contains(&id) {
                out.push(DeoptMetadataError::UndefinedVirtualObjectRef { at, id });
            }
        }
    }

    /// The JVM value category a machine-location descriptor claims for the word
    /// it names, or `None` for a descriptor that names no machine word.
    ///
    /// Two descriptors of the *same* word must agree on this, which is what
    /// [`DeoptMetadataError::SlotTypeConflict`] enforces. `"ref"` is also what
    /// the oop-map agreement lanes join on.
    fn slot_category(v: &FrameValue) -> Option<(i32, &'static str)> {
        match v {
            FrameValue::StackSlot(off) => Some((*off, "int")),
            FrameValue::StackSlotRef(off) => Some((*off, "ref")),
            FrameValue::StackSlotLong(off) => Some((*off, "long")),
            FrameValue::StackSlotFloat(off) => Some((*off, "float")),
            FrameValue::StackSlotDouble(off) => Some((*off, "double")),
            _ => None,
        }
    }

    fn check_scope(
        &self,
        point: &DeoptimizationPoint,
        state: &FrameState,
        depth: usize,
        coverage: Option<&OopCoverage>,
        chain_ids: &mut ChainVirtualIds,
        out: &mut Vec<DeoptMetadataError>,
    ) {
        // ── scope lane ───────────────────────────────────────────────
        if !self.methods.is_empty() {
            match self.methods.get(&state.method_key) {
                None => out.push(DeoptMetadataError::UnknownMethod {
                    native_offset: point.native_offset,
                    bci: state.bci,
                    method_key: state.method_key.clone(),
                    scope_depth: depth,
                }),
                Some(limits) => {
                    if state.bci >= limits.code_len {
                        out.push(DeoptMetadataError::BciOutOfRange {
                            native_offset: point.native_offset,
                            bci: state.bci,
                            method_key: state.method_key.clone(),
                            scope_depth: depth,
                            code_len: limits.code_len,
                        });
                    }
                    if state.locals.len() > limits.max_locals as usize {
                        out.push(DeoptMetadataError::LocalCountMismatch {
                            native_offset: point.native_offset,
                            bci: state.bci,
                            method_key: state.method_key.clone(),
                            scope_depth: depth,
                            found: state.locals.len(),
                            max_locals: limits.max_locals,
                        });
                    }
                    if state.stack.len() > limits.max_stack as usize {
                        out.push(DeoptMetadataError::StackCountMismatch {
                            native_offset: point.native_offset,
                            bci: state.bci,
                            method_key: state.method_key.clone(),
                            scope_depth: depth,
                            found: state.stack.len(),
                            max_stack: limits.max_stack,
                        });
                    }
                }
            }
        }

        // ── slot lanes ───────────────────────────────────────────────
        //
        // Virtual-object ids are CHAIN-wide (round 13 wave 5, lane resume; they
        // were taken as scope-local, which no producer ever emitted: a chain
        // resolves all its scopes with one `emitted` set, and the VM
        // materialises the chain as one graph). So the definitions seen in the
        // scopes already walked seed this one (a second definition of an id is
        // a duplicate wherever it sits), and references are validated by the
        // caller once the whole chain is walked, because a `VirtualObjectRef`
        // may legally precede its definition — the materializer allocates all
        // shells before wiring any field.
        let mut ctx = ScopeCheck {
            defined: std::mem::take(&mut chain_ids.defined),
            ..ScopeCheck::default()
        };

        let base = |kind: SlotKind, index: usize| SlotRef {
            native_offset: point.native_offset,
            bci: state.bci,
            method_key: state.method_key.clone(),
            scope_depth: depth,
            kind,
            index,
        };

        for (i, v) in state.locals.iter().enumerate() {
            self.check_value(v, &base(SlotKind::Local, i), coverage, &mut ctx, out);
        }
        for (i, v) in state.stack.iter().enumerate() {
            self.check_value(v, &base(SlotKind::Stack, i), coverage, &mut ctx, out);
        }

        // ── monitor lane ─────────────────────────────────────────────
        //
        // "Balanced" here means the list can be replayed as `monitorenter`
        // pairs on resume: every entry names a re-lockable object, carries a
        // depth of at least one (a depth-0 entry is a lock nobody holds), and
        // no object appears twice (a re-entrant lock is ONE entry with depth 2 —
        // two entries would make the resume enter it twice and leave the
        // interpreter one `monitorexit` short at method end) -- except the one
        // split a nested-lock elision publishes: a held entry, then one re-take
        // entry (see the check below).
        for (i, m) in state.monitors.iter().enumerate() {
            let at = base(SlotKind::Monitor, i);
            if m.lock_depth == 0 {
                out.push(DeoptMetadataError::UnbalancedMonitor {
                    at: at.clone(),
                    detail: "lock_depth is 0, so the resume would record a lock nobody holds"
                        .to_string(),
                });
            }
            // Exactly one shape may name an object twice (round 11 wave 15,
            // lane `ir`): the levels the compiled code holds, then the levels
            // a nested-lock elision removed -- `[{o, n, relock: false},
            // {o, k, relock: true}]` (`ir_optimize::elide_nested_monitors`,
            // `ir_lower::Lowerer::resolve_monitors`). The resume skips the
            // first and enters the second `k` times, so the thread holds `o`
            // `n + k` times, which is what the interpreter frame releases.
            // Anything else (two held parts, a re-take before the held part,
            // three entries) still cannot be replayed.
            let mut earlier = state.monitors[..i].iter().filter(|p| p.object == m.object);
            let first = earlier.next();
            let split_retake =
                m.relock && first.is_some_and(|held| !held.relock) && earlier.next().is_none();
            if first.is_some() && !split_retake {
                out.push(DeoptMetadataError::UnbalancedMonitor {
                    at: at.clone(),
                    detail: format!(
                        "object {:?} is already held by an earlier entry — re-entrancy must be \
                         one entry with lock_depth > 1, or a held entry followed by one \
                         re-take entry",
                        m.object
                    ),
                });
            }
            if value_blocks_resume(&m.object) || matches!(m.object, FrameValue::Undefined) {
                out.push(DeoptMetadataError::UnbalancedMonitor {
                    at: at.clone(),
                    detail: format!(
                        "monitor object {:?} cannot be reconstructed, so the resume cannot \
                         unlock it",
                        m.object
                    ),
                });
            }
            // A materialized object's lock CANNOT already be held, so its entry
            // must ask the resume to acquire it.
            //
            // `relock == false` means "the compiled code took this lock through
            // the monitor helper and the thread still holds it, so acquiring it
            // again would leave it one level too deep after the interpreter's
            // `monitorexit`". That statement is impossible about a
            // `VirtualObject`/`VirtualObjectRef`: the object does not exist
            // until the resume materializes it, so no thread can be holding its
            // monitor, and the elision of its monitor ops is *why* the entry is
            // recorded at all. Both producers agree today — the single-pass
            // Phase C lane hardcodes `relock: true`
            // (`x64/deopt_stubs.rs::build_frame_state_at`), and `ir_lower`'s
            // `resolve_monitors` derives `relock = !live_monitor_operands
            // .contains(&obj)`, which cannot contain a scalar-replaced object
            // because a live monitor op on it is exactly what stops it being
            // scalar-replaced.
            //
            // So this is a tripwire on an agreement between two files, not a
            // case anything is expected to hit; it is worth a lane because the
            // failure is silent and late. The resume materializes the shell,
            // skips the `monitorenter`, and the interpreter frame then runs the
            // `monitorexit` the bytecode pairs with the enter — releasing a
            // monitor this thread never acquired. Reported as an
            // `UnbalancedMonitor` rather than a new variant because it is
            // precisely that: a list that cannot be replayed as
            // enter/exit pairs.
            if !m.relock
                && matches!(
                    m.object,
                    FrameValue::VirtualObject(_) | FrameValue::VirtualObjectRef(_)
                )
            {
                out.push(DeoptMetadataError::UnbalancedMonitor {
                    at: at.clone(),
                    detail: format!(
                        "monitor object {:?} is materialized by the resume, so its lock cannot \
                         already be held, but `relock` is false — the resume would skip the \
                         `monitorenter` and the interpreter frame would still run the matching \
                         `monitorexit`",
                        m.object
                    ),
                });
            }
            // A monitor entry replays as `monitorenter` on the object it names,
            // and the interpreter's method-exit path emits one `monitorexit`
            // per entry. So the entry has to name something lockable. A
            // primitive descriptor here is not a near-miss: the resume would
            // build a `Value::Int`, the exit would try to unlock it, and the
            // real monitor stays held — a hang in whatever thread asks for it
            // next, arbitrarily far from the deopt that caused it. An explicit
            // null is the same story with an NPE at the enter instead.
            if let Some(detail) = monitor_object_defect(&m.object) {
                out.push(DeoptMetadataError::MonitorObjectNotAReference {
                    at: at.clone(),
                    detail,
                });
            }
            self.check_value(&m.object, &at, coverage, &mut ctx, out);
        }

        // Handed to the chain walk (`check_point`), which validates the
        // references against every scope's definitions.
        chain_ids.referenced.append(&mut ctx.referenced);
        chain_ids.defined = std::mem::take(&mut ctx.defined);

        if ctx.needs_oop_map && coverage.is_none() && self.require_oop_map {
            out.push(DeoptMetadataError::MissingOopMap {
                native_offset: point.native_offset,
                bci: state.bci,
            });
        }
    }

    /// Check one `FrameValue`, recursing through virtual-object fields.
    fn check_value(
        &self,
        v: &FrameValue,
        at: &SlotRef,
        coverage: Option<&OopCoverage>,
        ctx: &mut ScopeCheck,
        out: &mut Vec<DeoptMetadataError>,
    ) {
        // ── frame-slot well-formedness, for every `StackSlot*` variant ──
        //
        // Two properties that hold for *all five* typed frame-slot variants
        // and that nothing checked before, so they are done once here rather
        // than five times in the match below.
        if let Some((off, kind)) = Self::slot_category(v) {
            // (1) The offset must address this frame. Every producer builds
            // these as `-spill_off` from a strictly positive `[rbp - spill]`
            // offset (`x64::frame_value_for_slot`, `x64::sr_field_values`,
            // `ir_lower::typed_stack_slot`), so `off >= 0` cannot come from a
            // correct emitter: `0` is `[rbp]`, the saved caller frame pointer
            // — the one word `ir_lower::poison_slot` exists to keep the
            // compiler from ever naming — and anything above it is the
            // caller's frame or the return address. Reconstructing a local
            // from there is a plausible-looking wrong value, which is worse
            // than a refusal.
            if off >= 0 {
                out.push(DeoptMetadataError::FrameSlotOutsideFrame {
                    at: at.clone(),
                    offset: off,
                });
            }
            // (2) One machine word, one value. Two slots may legitimately
            // *share* a word (`aload_0` leaves local 0 and stack[0] naming the
            // same spill), but then they agree on its type. Disagreeing is an
            // internal contradiction that needs no external data to spot, and
            // it is precisely how an `int` ends up reconstructed into a slot
            // the interpreter reads as a reference.
            // Cloned out of the map before the match so the lookup borrow ends
            // here rather than spanning the `insert` in the `None` arm.
            let prior = ctx.word_types.get(&off).cloned();
            match prior {
                Some((first, first_kind)) if first_kind != kind => {
                    out.push(DeoptMetadataError::SlotTypeConflict {
                        at: at.clone(),
                        first,
                        offset: off,
                        first_kind,
                        second_kind: kind,
                    });
                }
                Some(_) => {}
                None => {
                    ctx.word_types.insert(off, (at.clone(), kind));
                }
            }
        }

        match v {
            // ── register-file bounds ─────────────────────────────────
            FrameValue::Register(r) | FrameValue::RegisterLong(r) => {
                self.check_gpr_index(*r, out);
                self.check_primitive_not_in_oop_map(
                    at,
                    coverage,
                    "gpr",
                    *r as i32,
                    if matches!(v, FrameValue::Register(_)) {
                        "int"
                    } else {
                        "long"
                    },
                    |cov| cov.covers_register(*r),
                    out,
                );
            }
            FrameValue::XmmFloat(n) | FrameValue::XmmDouble(n) => {
                if *n as usize >= 16 {
                    out.push(DeoptMetadataError::RegisterFileIndexOutOfRange {
                        bank: "xmm",
                        index: *n,
                    });
                }
            }

            // ── oop-map agreement ────────────────────────────────────
            FrameValue::RegisterRef(r) => {
                ctx.needs_oop_map = true;
                self.check_gpr_index(*r, out);
                if let Some(cov) = coverage {
                    if cov.moving_young_coverage_complete && !cov.covers_register(*r) {
                        out.push(DeoptMetadataError::ReferenceRegisterNotInOopMap {
                            at: at.clone(),
                            reg: *r,
                        });
                    }
                }
            }
            FrameValue::StackSlotRef(off) => {
                ctx.needs_oop_map = true;
                // Deopt spells the word as `*(rbp + off)` with `off < 0`; the
                // oop map spells the same word as the positive `[rbp - off]`.
                let positive = -*off;
                if i16::try_from(positive).is_err() {
                    out.push(DeoptMetadataError::UnencodableRefOffset {
                        at: at.clone(),
                        frame_offset: positive,
                    });
                } else if let Some(cov) = coverage {
                    if cov.moving_young_coverage_complete && !cov.covers_frame_slot(positive) {
                        out.push(DeoptMetadataError::ReferenceNotInOopMap {
                            at: at.clone(),
                            frame_offset: positive,
                        });
                    }
                }
            }
            // The converse oop-map direction, for the four *primitive* frame
            // slots. See `check_primitive_not_in_oop_map`.
            FrameValue::StackSlot(off)
            | FrameValue::StackSlotLong(off)
            | FrameValue::StackSlotFloat(off)
            | FrameValue::StackSlotDouble(off) => {
                let positive = -*off;
                let kind = Self::slot_category(v).map_or("primitive", |(_, k)| k);
                self.check_primitive_not_in_oop_map(
                    at,
                    coverage,
                    "frame",
                    positive,
                    kind,
                    |cov| cov.covers_frame_slot(positive),
                    out,
                );
            }
            FrameValue::Object(addr) if *addr != 0 => {
                out.push(DeoptMetadataError::BakedObjectAddress {
                    at: at.clone(),
                    address: *addr,
                });
            }

            // ── virtual objects ──────────────────────────────────────
            FrameValue::VirtualObject(state) => {
                if !ctx.defined.insert(state.id) {
                    out.push(DeoptMetadataError::DuplicateVirtualObjectDefinition {
                        at: at.clone(),
                        id: state.id,
                    });
                }
                if state.num_fields != state.field_values.len() {
                    out.push(DeoptMetadataError::VirtualObjectFieldCountMismatch {
                        at: at.clone(),
                        id: state.id,
                        declared: state.num_fields,
                        found: state.field_values.len(),
                    });
                }
                // The id IS the IR node of the eliminated allocation. Naming a
                // removed node is only legal when that node is also registered
                // as materializable — otherwise the recipe was built from a
                // stale graph and rebuilds a slot from nothing.
                let node = state.id as u32;
                if self.removed_nodes.contains(&node) && !self.materializable_nodes.contains(&node)
                {
                    out.push(DeoptMetadataError::SlotNamesRemovedNode {
                        at: at.clone(),
                        node,
                    });
                }
                for (i, fv) in state.field_values.iter().enumerate() {
                    let field_at = at.field(state.id, i);
                    self.check_value(fv, &field_at, coverage, ctx, out);
                }
            }
            FrameValue::VirtualObjectRef(id) => ctx.referenced.push((at.clone(), *id)),

            // ── eliminated vs. undefined ─────────────────────────────
            //
            // `MaterializationRequired` is NOT a violation: it is the honest
            // marker for "this value was deleted and I cannot rebuild it", and
            // it already makes the frame unresumable, so the method takes the
            // safe re-run. What WOULD be a violation is the same situation
            // spelled `Undefined` — and that one is undetectable here by
            // construction, which is exactly why producers must be fixed to
            // emit this variant. See `docs/jit/deopt-metadata.md`.
            FrameValue::MaterializationRequired(_) => {}

            _ => {}
        }
    }

    fn check_gpr_index(&self, r: u8, out: &mut Vec<DeoptMetadataError>) {
        if r as usize >= 16 {
            out.push(DeoptMetadataError::RegisterFileIndexOutOfRange {
                bank: "gpr",
                index: r,
            });
        }
    }

    /// The **converse** of the oop-map agreement rule: a location the deopt map
    /// reads as a primitive must not be one the oop map lists as holding a live
    /// reference.
    ///
    /// `docs/jit/deopt-metadata.md` §3 states the forward direction (a
    /// reference the deopt map names must be one the GC updates) and then says
    /// the converse — an oop-map slot the deopt map does not name — is *not* an
    /// error, because the GC may legitimately track a spilled temporary the
    /// interpreter frame does not resume from. That remains true, and this is
    /// not that case: here the deopt map **does** name the word, and names it
    /// as an `int`/`long`/`float`/`double`. One machine word at one safepoint
    /// holds one value, so the two maps cannot both be right, and either
    /// reading is unsound — the resume hands the interpreter a truncated `int`
    /// where a live reference belongs (and drops the oop from the frame's root
    /// set), or the collector relocates against a word that is not a pointer.
    ///
    /// Sound on the IR backend by construction rather than by luck: `plan_slots`
    /// keeps disjoint `Ref` and `Prim` free lists, so "a colour is `Ref` or
    /// `Prim` from its first assignment and never changes class"
    /// (`jit/src/ir_lower.rs`, *Reference / primitive separation*). This lane is
    /// the tripwire on that invariant, and a real check for any backend wired in
    /// later whose deopt map and oop map are built from different sources — which
    /// `jit/src/x64.rs` is (`local_oop_masks`/`stack_oop_marks` vs. the register
    /// allocator's live sets).
    ///
    /// Gated on `moving_young_coverage_complete` for the same reason the forward
    /// lane is: a map that does not claim completeness is a conservative
    /// over-approximation the collector already refuses to move against, so a
    /// disagreement there is not evidence of a defect.
    #[allow(clippy::too_many_arguments)]
    fn check_primitive_not_in_oop_map(
        &self,
        at: &SlotRef,
        coverage: Option<&OopCoverage>,
        bank: &'static str,
        location: i32,
        kind: &'static str,
        covered: impl Fn(&OopCoverage) -> bool,
        out: &mut Vec<DeoptMetadataError>,
    ) {
        let Some(cov) = coverage else { return };
        if !cov.moving_young_coverage_complete {
            return;
        }
        if covered(cov) {
            out.push(DeoptMetadataError::PrimitiveSlotCoveredByOopMap {
                at: at.clone(),
                bank,
                location,
                kind,
            });
        }
    }
}

/// Per-scope accumulators shared by every [`DeoptVerifier::check_value`] call
/// for one scope.
///
/// Grouped into a struct rather than passed as five `&mut` parameters because
/// the set grew past what a reader can keep straight at a call site, and every
/// member has the same lifetime: one scope of one deopt point.
#[derive(Default)]
struct ScopeCheck {
    /// Virtual-object ids *defined* (by a `VirtualObject`) in this scope.
    defined: FxHashSet<usize>,
    /// `VirtualObjectRef` edges, validated after the whole scope is walked (a
    /// reference may legally precede its definition).
    referenced: Vec<(SlotRef, usize)>,
    /// Any reference-typed slot was seen, so this scope needs an oop map.
    needs_oop_map: bool,
    /// Frame word (`*(rbp + off)`) → the first descriptor that named it and the
    /// JVM category that descriptor claimed. Drives
    /// [`DeoptMetadataError::SlotTypeConflict`].
    word_types: FxHashMap<i32, (SlotRef, &'static str)>,
}

/// The virtual-object ids of a whole inlined chain (round 13 wave 5, lane
/// resume): ids are chain-wide, so the per-scope [`ScopeCheck`] hands its
/// definitions and references on, and `DeoptVerifier::check_point`
/// validates the references once every scope was walked.
#[derive(Default)]
struct ChainVirtualIds {
    defined: FxHashSet<usize>,
    referenced: Vec<(SlotRef, usize)>,
}

/// Why `object` cannot serve as a held monitor, or `None` if it can.
///
/// A monitor entry is replayed as a `monitorenter` on resume and balanced by
/// one `monitorexit` at method exit, so the recorded value has to be something
/// the interpreter can lock. Two shapes cannot be:
///
/// * a **primitive** descriptor — the resume builds a `Value::Int`/`Long`/…,
///   the exit tries to unlock it, and the real monitor is never released. The
///   symptom is a hang in an unrelated thread, arbitrarily later.
/// * an **explicit null** (`Object(0)`) — `monitorenter` on null throws, so no
///   correct emitter records one; a null here means the emitter lost the
///   object, not that the program locked nothing.
///
/// `Undefined`, `Unsupported` and `MaterializationRequired` are already
/// reported by the surrounding [`DeoptMetadataError::UnbalancedMonitor`] arm,
/// so they are deliberately not repeated here.
fn monitor_object_defect(object: &FrameValue) -> Option<String> {
    match object {
        FrameValue::Object(0) => Some(
            "the recorded object is null, and `monitorenter` on null throws rather than locking"
                .to_string(),
        ),
        FrameValue::Object(_)
        | FrameValue::StackSlotRef(_)
        | FrameValue::RegisterRef(_)
        | FrameValue::VirtualObject(_)
        | FrameValue::VirtualObjectRef(_)
        | FrameValue::Undefined
        | FrameValue::Unsupported
        | FrameValue::MaterializationRequired(_) => None,
        other => Some(format!(
            "{other:?} is a primitive descriptor, so the resume would `monitorenter` a \
             non-reference and the matching `monitorexit` would never release the real lock"
        )),
    }
}

/// Render a violation list as a compilation [`Bailout`].
///
/// Uses [`BailoutReason::DeoptMetadata`] (its own category in `bailout.rs`,
/// so these are countable separately from IR-verification rejections) with the
/// context `phase=install`, because that is the only point at which this runs:
/// over emitted metadata, before the artifact becomes a `CompiledMethod`.
fn deopt_metadata_bailout(errors: &[DeoptMetadataError]) -> Bailout {
    let shown = errors.len().min(MAX_REPORTED_DEOPT_VIOLATIONS);
    let mut msg = format!("{} deopt-metadata violation(s): ", errors.len());
    let rendered: Vec<String> = errors[..shown].iter().map(|e| e.to_string()).collect();
    msg.push_str(&rendered.join("; "));
    if errors.len() > shown {
        msg.push_str(&format!(" … and {} more", errors.len() - shown));
    }
    Bailout::with_context(
        BailoutReason::DeoptMetadata(msg),
        "phase=install".to_string(),
    )
}

/// Convenience wrapper: verify `points` with a verifier built from `methods`
/// and `oop_maps`.
///
/// The shape an install site wants — one call, `CompileResult<()>`, bail on
/// failure — without having to know the builder API.
pub fn verify_deopt_metadata(
    points: &[DeoptimizationPoint],
    methods: impl IntoIterator<Item = MethodFrameLimits>,
    oop_maps: impl IntoIterator<Item = (u32, OopCoverage)>,
) -> CompileResult<()> {
    let mut verifier = DeoptVerifier::new();
    for m in methods {
        verifier = verifier.with_method(m);
    }
    for (off, cov) in oop_maps {
        verifier = verifier.with_oop_map(off, cov);
    }
    verifier.verify(points)
}
