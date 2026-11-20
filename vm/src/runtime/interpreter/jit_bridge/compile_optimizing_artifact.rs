#[allow(unused_imports)]
use super::*;

impl CpResolvers<'_> {
    /// Is `cp_idx` the operand of an `opcode` instruction (`invokevirtual`
    /// `0xb6`, `invokespecial` `0xb7`, `invokestatic` `0xb8` or
    /// `invokeinterface` `0xb9`) in this method's bytecode?
    /// [`Self::invoke`] is handed only the index; one walk of the code per
    /// compile answers every site.
    pub(in crate::runtime::interpreter::jit_bridge) fn names_an_invoke_of(
        &self,
        cp_idx: u16,
        opcode: u8,
    ) -> bool {
        const INVOKEVIRTUAL: u8 = 0xb6;
        const INVOKEINTERFACE: u8 = 0xb9;
        let operands = self.invoke_operands.get_or_init(|| {
            let code = self.code;
            let mut found: Vec<(u16, u8)> = Vec::new();
            let mut pc = 0usize;
            while pc < code.len() {
                let op = code[pc];
                // JIT round 20 wave 7 (lane huntDisp): all four method
                // invokes, for `invoke_site_linkage_error`.
                if (INVOKEVIRTUAL..=INVOKEINTERFACE).contains(&op) && pc + 2 < code.len() {
                    found.push((u16::from_be_bytes([code[pc + 1], code[pc + 2]]), op));
                }
                // Same walk, and the same `max(1)`, as `code_has_new_of`.
                pc += cratonvm_jit::bytecode_insn_len(code, pc).max(1);
            }
            found.sort_unstable();
            found.dedup();
            found.into_boxed_slice()
        });
        operands.binary_search(&(cp_idx, opcode)).is_ok()
    }

    pub(in crate::runtime::interpreter::jit_bridge) fn class_name(
        &self,
        cp_idx: u16,
    ) -> Option<String> {
        let cm = self.shared.classes.class_manager.read();
        let class = cm.get_class(self.class_id)?;
        class
            .constant_pool
            .get_class_name(cp_idx)
            .map(|s| s.to_string())
    }

    /// Round 15 wave 4 (lane fieldowner, CH3-1): `cp_idx`'s field site as
    /// [`jit_resolve_field_site`] answers it, once per compile when `memo` is
    /// given (every field ask below derives from this).
    pub(in crate::runtime::interpreter::jit_bridge) fn resolved_field(
        &self,
        memo: Option<&JitFieldSiteMemo>,
        cp_idx: u16,
    ) -> Option<ResolvedField> {
        if let Some(memo) = memo {
            if let Some(answer) = memo.rows.borrow().get(&cp_idx) {
                return answer.clone();
            }
        }
        let answer = jit_resolve_field_site(self.shared, self.class_id, cp_idx).ok();
        if let Some(memo) = memo {
            memo.rows.borrow_mut().insert(cp_idx, answer.clone());
        }
        answer
    }

    /// `(field index, descriptor tag, compact slot)` of a `getfield`/`putfield`.
    pub(in crate::runtime::interpreter::jit_bridge) fn instance_field(
        &self,
        memo: Option<&JitFieldSiteMemo>,
        cp_idx: u16,
    ) -> Option<(usize, u8, Option<(u32, bool)>)> {
        let field = self.resolved_field(memo, cp_idx)?;
        if !jit_field_kind_agrees(&field, false)
            || method_puts_final_field_illegally(
                self.shared,
                self.class_id,
                self.method_name,
                self.code,
                cp_idx,
                &field,
            )
        {
            return None;
        }
        // SBR4 satb: a method that `getfield`s `Reference.referent` answers the
        // marker tag for the field, which the compiler reads as "no field row
        // for this pc": the load stays on the checked `jit_getfield` helper,
        // which carries the SATB keep-alive (see `jit_load_of_referent`).
        if field.field_index == 0
            && jit_load_of_referent(self.shared, Some(0xb4), &field)
            && code_has_getfield_of(self.code, cp_idx)
        {
            return Some((field.field_index, REFERENT_SITE_TAG, None));
        }
        // Get the field descriptor from the constant pool
        let cm = self.shared.classes.class_manager.read();
        let class = cm.get_class(self.class_id)?;
        let nat_idx = match class.constant_pool.get(cp_idx) {
            Some(ConstantPoolEntry::FieldReference {
                name_and_type_index,
                ..
            }) => *name_and_type_index,
            // Unreachable in practice: `resolved_field` already declined a
            // non-`Fieldref` entry (`invalid field ref at cp#N`). The old
            // mislabelled refusal record here was dead (r15w4 fieldowner F3).
            _ => return None,
        };
        let (_, descriptor) = class.constant_pool.get_name_and_type(nat_idx)?;
        let type_tag = *descriptor.as_bytes().first()?;
        if !jit_field_tag_agrees(&field, type_tag, cp_idx) {
            return None;
        }
        // `None` ⇒ no genuine compact slot for this field (class has no
        // registered `CompactLayout`, or the index falls outside it).
        // Fabricating a `(0, false)` placeholder here poisoned the JIT's
        // compact-offset inline getfield/putfield with a garbage offset: for
        // a reference field it emitted a 32-bit sign-extended load of half a
        // `Value` cell, producing a bogus non-null receiver that SIGSEGVed in
        // the invoke inline cache (WildFly Host Controller `host=foo:add()`,
        // docs/known-issues/wildfly-domain-hostcontroller-sigsegv-*).
        Some((
            field.field_index,
            type_tag,
            cratonvm_types::compact_field_disp(
                field.declaring_class_id.as_u32(),
                field.field_index,
            )
            .map(|(o, r)| (o as u32, r)),
        ))
    }

    /// Round 15 wave 3 (lane chain3, CHN15-2 producer): `(field index,
    /// declaring class id)` of a `getfield` / `putfield`'s resolved field, for
    /// the IR alias model (`CompileRequest::cp_field_declaring_class_resolver`).
    /// The same resolution and kind check as [`Self::instance_field`]
    /// ([`Self::resolved_field`]); the JIT keeps the row only where the
    /// index equals that resolver's.
    pub(in crate::runtime::interpreter::jit_bridge) fn instance_field_declaring_class(
        &self,
        memo: Option<&JitFieldSiteMemo>,
        cp_idx: u16,
    ) -> Option<(usize, u32)> {
        let field = self.resolved_field(memo, cp_idx)?;
        if !jit_field_kind_agrees(&field, false) {
            return None;
        }
        let decl = jit_field_decl_identity(self.shared, self.class_id, cp_idx, &field)?;
        Some((field.field_index, decl))
    }

    /// `(declaring class id, field index, descriptor tag, volatile)` of a
    /// `getstatic`/`putstatic`.
    pub(in crate::runtime::interpreter::jit_bridge) fn static_field(
        &self,
        memo: Option<&JitFieldSiteMemo>,
        cp_idx: u16,
    ) -> Option<(u32, usize, u8, bool)> {
        let field = self.resolved_field(memo, cp_idx)?;
        if !jit_field_kind_agrees(&field, true)
            || method_puts_final_field_illegally(
                self.shared,
                self.class_id,
                self.method_name,
                self.code,
                cp_idx,
                &field,
            )
        {
            return None;
        }
        let cm = self.shared.classes.class_manager.read();
        let class = cm.get_class(self.class_id)?;
        let nat_idx = match class.constant_pool.get(cp_idx) {
            Some(ConstantPoolEntry::FieldReference {
                name_and_type_index,
                ..
            }) => *name_and_type_index,
            _ => return None,
        };
        let (_, descriptor) = class.constant_pool.get_name_and_type(nat_idx)?;
        let type_tag = *descriptor.as_bytes().first()?;
        if !jit_field_tag_agrees(&field, type_tag, cp_idx) {
            return None;
        }
        Some((
            field.declaring_class_id.as_u32(),
            field.field_index,
            type_tag,
            field.is_volatile,
        ))
    }

    /// Interpreter round i4 wave 92, lane L2
    /// (`CompileRequest::cp_field_deferred_resolver`): the descriptor tag of
    /// the `Fieldref` at `cp_idx`, read from this pool alone
    /// ([`jit_deferred_field_tag`], which the OSR door asks too since wave
    /// 93). `None` for an entry that is not a `Fieldref` (the refusal stays).
    pub(in crate::runtime::interpreter::jit_bridge) fn deferred_field_tag(
        &self,
        cp_idx: u16,
    ) -> Option<u8> {
        let tag = jit_deferred_field_tag(self.shared, self.class_id, cp_idx);
        // JIT round 21, lane tcshape: the site compiles deferred; remembered
        // for `note_deferred_field_waiters`.
        if tag.is_some() {
            if let Ok(mut seen) = self.deferred_fields_seen.try_borrow_mut() {
                if !seen.contains(&cp_idx) {
                    seen.push(cp_idx);
                }
            }
        }
        tag
    }

    /// JIT round 17 wave 2 (lane irfeed; `r17-monitor2-static-final-feed-needs-
    /// a-final-bit-20261005.md`): may the JIT trust the REFERENCE a
    /// `getstatic` site reads as a constant once the declaring class is
    /// initialised? `true` only for an `ACC_STATIC | ACC_FINAL` field of
    /// reference type (`L` / `[`) declared by a class the application loader
    /// or a user-defined loader defined from a class file of version 53+
    /// (Java 9+, where `putstatic` of a final outside `<clinit>` is an
    /// `IllegalAccessError`; an older class file may write it from any of its
    /// own methods). Bootstrap and platform classes are never trusted: the
    /// JDK rewrites some of its static finals at boot (`System.in/out/err`
    /// first among them). For `CompileRequest::cp_static_field_final_resolver`;
    /// the JIT also requires its own initialised-class answer
    /// (`resolve_static_base`) before it trusts anything.
    pub(in crate::runtime::interpreter::jit_bridge) fn static_field_is_trusted_final(
        &self,
        memo: Option<&JitFieldSiteMemo>,
        cp_idx: u16,
    ) -> bool {
        use cratonvm_types::ClassLoaderId;
        let Some(field) = self.resolved_field(memo, cp_idx) else {
            return false;
        };
        if !(field.is_static && field.is_final && field.is_reference)
            || !matches!(field.desc_byte, b'L' | b'[')
        {
            return false;
        }
        let cm = self.shared.classes.class_manager.read();
        matches!(
            cm.get_loader_id(field.declaring_class_id),
            Some(ClassLoaderId::Application | ClassLoaderId::UserDefined(_))
        ) && cm
            .get_class(field.declaring_class_id)
            .is_some_and(|c| c.version.major >= 53)
    }

    /// `ACC_VOLATILE` of a `getfield`/`putfield` site, the instance twin of the
    /// static resolver's fourth element. Without it the JIT compiled a volatile
    /// store with no StoreLoad fence and let the optimizing tier hoist a
    /// volatile read (`volatile-instance-fields-are-plain-accesses-in-compiled-
    /// code`). An unresolvable (or refused) site answers `false`;
    /// `instance_field` refuses it anyway.
    pub(in crate::runtime::interpreter::jit_bridge) fn field_is_volatile(
        &self,
        memo: Option<&JitFieldSiteMemo>,
        cp_idx: u16,
    ) -> bool {
        self.resolved_field(memo, cp_idx)
            .is_some_and(|field| field.is_volatile)
    }

    /// nio21 wave 2 (lane L3): `ACC_FINAL` of a `getfield` / `putfield`
    /// site's resolved INSTANCE field, for
    /// `CompileRequest::cp_field_final_resolver`. The same resolution and kind
    /// check as [`Self::instance_field`]; a volatile field answers `false`
    /// (JVMS 4.5 forbids the pair, and the memo never forwards a volatile
    /// access anyway). An unresolvable site answers `false`: no forwarding.
    /// Not a trust statement about the VALUE (no constant folding happens on
    /// it); see the "Final fields" section of `cratonvm_jit::ir`.
    pub(in crate::runtime::interpreter::jit_bridge) fn instance_field_is_final(
        &self,
        memo: Option<&JitFieldSiteMemo>,
        cp_idx: u16,
    ) -> bool {
        self.resolved_field(memo, cp_idx).is_some_and(|field| {
            field.is_final && !field.is_volatile && jit_field_kind_agrees(&field, false)
        })
    }

    /// JIT round 20 (lane memory): `CompileRequest::cp_field_byte_array_resolver`,
    /// the compiling method's own twin of the splice proof in
    /// `resolve_inline_site_from` (`InlineSite::byte_array_field_pcs`): the
    /// pool row is `hb:[B` ([`byte_array_field_row`]) and the resolved field
    /// is an array-typed instance field of the bootstrap `java/nio/ByteBuffer`
    /// ([`byte_array_field_declarer_admitted`]). The pool is read and its
    /// class-manager guard dropped before the resolution, which takes its own.
    pub(in crate::runtime::interpreter::jit_bridge) fn instance_field_is_byte_array(
        &self,
        memo: Option<&JitFieldSiteMemo>,
        cp_idx: u16,
    ) -> bool {
        let row = {
            let cm = self.shared.classes.class_manager.read();
            let Some(class) = cm.get_class(self.class_id) else {
                return false;
            };
            let Some(ConstantPoolEntry::FieldReference {
                name_and_type_index,
                ..
            }) = class.constant_pool.get(cp_idx)
            else {
                return false;
            };
            class
                .constant_pool
                .get_name_and_type(*name_and_type_index)
                .is_some_and(|(name, desc)| byte_array_field_row(name, desc))
        };
        row && self.resolved_field(memo, cp_idx).is_some_and(|field| {
            field.desc_byte == b'['
                && !field.is_static
                && jit_field_kind_agrees(&field, false)
                && byte_array_field_declarer_admitted(self.shared, &field)
        })
    }

    /// `(class, name, descriptor)` of a `Methodref`/`InterfaceMethodref`.
    pub(in crate::runtime::interpreter::jit_bridge) fn invoke(
        &self,
        cp_idx: u16,
    ) -> Option<(String, String, String)> {
        let (class_idx, answer) = {
            let cm = self.shared.classes.class_manager.read();
            let class = cm.get_class(self.class_id)?;
            let (class_idx, nat_idx, is_iface) = match class.constant_pool.get(cp_idx) {
                Some(ConstantPoolEntry::MethodReference {
                    class_index,
                    name_and_type_index,
                    ..
                }) => (*class_index, *name_and_type_index, false),
                Some(ConstantPoolEntry::InterfaceMethodReference {
                    class_index,
                    name_and_type_index,
                    ..
                }) => (*class_index, *name_and_type_index, true),
                _ => return None,
            };
            let target_class = class.constant_pool.get_class_name(class_idx)?;
            let (method_name, descriptor) = class.constant_pool.get_name_and_type(nat_idx)?;
            // `--real-jdk`: an `invokespecial` whose JVMS §6.5 selection is an
            // error (`AbstractMethodError`, conflicting defaults) is left to
            // the interpreter, which raises it — the method is not compiled,
            // at any door that builds its request here (method entry, the
            // callee doors, the optimizing OSR door; a declined site bails
            // the whole compile, as a §5.4.4 refusal below does). The owner
            // resolver cannot refuse a site, and binding the lenient walk ran
            // the concrete ancestor's body where HotSpot throws (interpreter
            // round i1 wave 37, lane L2). The first question of a compile
            // walks the bytecode once; a `--compatible` VM and a constructor
            // never ask.
            // Interpreter round i7 wave 136, lane L2: the interface-sender
            // receiver refusal (`CRATONVM_INTERFACE_SPECIAL_RECEIVER_CHECK`)
            // is its own switch, not a selection check, and the interpreter
            // makes that check whatever `selection_checks` says; an
            // interface's method is asked with selection checks off too
            // (`CRATONVM_SELECTION_CHECKS_EVERY_MODE=0` under `--compatible`
            // compiled the unchecked site here while every other door, which
            // asks unconditionally, refused it).
            if (self.shared.classes.selection_checks
                || (class.is_interface()
                    && super::invoke::interface_special_receiver_check_enabled(self.shared)))
                && method_name != "<init>"
                && self.names_an_invoke_of(cp_idx, 0xb7)
            {
                if let Some(why) = special_site_bind_refusal(
                    self.shared,
                    &cm,
                    self.class_id,
                    target_class,
                    is_iface,
                    method_name,
                    descriptor,
                    false,
                ) {
                    // Bail-listed as well: the backend's `invoke_resolve`
                    // resolver bail is not a permanent verdict by itself, and
                    // the answer is a function of the loaded hierarchy, so
                    // every warmup-gate retry would re-run the scan to reach
                    // it again. Wave 36's interpreter-side answer is the same.
                    let verdicts = jit_verdicts(self.shared);
                    verdicts.mark_bail_listed(
                        self.class_id,
                        &class.name,
                        self.method_name,
                        self.method_descriptor,
                    );
                    verdicts.record_compile_refusal(
                        self.class_id,
                        &class.name,
                        self.method_name,
                        self.method_descriptor,
                        why,
                    );
                    dbg_special_site_refusal(
                        "compile-request",
                        &class.name,
                        self.method_name,
                        self.method_descriptor,
                        (target_class, method_name, descriptor),
                        why,
                    );
                    return None;
                }
            }
            // `--real-jdk`: an `invokeinterface` of a PRIVATE interface method
            // is pinned as a statically bound call (`invokespecial_owner`'s
            // `0xb9` arm), and no compiled route checks that the receiver
            // implements the interface (JVMS §6.5's
            // `IncompatibleClassChangeError`); the interpreter does. Such a
            // method stays interpreted (interpreter round i1 wave 37, lane L2;
            // `L2W37InterfaceSelectionHot`). javac emits the shape for a
            // default method calling its interface's private helper, which
            // the JDK has in three interfaces.
            if is_iface
                && self.shared.classes.selection_checks
                && self.names_an_invoke_of(cp_idx, 0xb9)
                && private_interface_method_pinned(
                    self.shared,
                    &cm,
                    self.class_id,
                    target_class,
                    method_name,
                    descriptor,
                )
                // Interpreter round i6 wave 132, lane L6
                // (`CRATONVM_JIT_PRIVATE_IFACE_THIS_PIN`, default OFF): every
                // such site's receiver is the method's unstored `this`, which
                // implements the interface, so the pin needs no check.
                && !private_iface_this_pin_proven(
                    self.shared,
                    &cm,
                    self.class_id,
                    self.method_name,
                    self.method_descriptor,
                    Some(self.code),
                    target_class,
                    cp_idx,
                    None,
                    descriptor,
                )
            {
                let why = "invokeinterface-private-receiver-unchecked";
                let verdicts = jit_verdicts(self.shared);
                verdicts.mark_bail_listed(
                    self.class_id,
                    &class.name,
                    self.method_name,
                    self.method_descriptor,
                );
                verdicts.record_compile_refusal(
                    self.class_id,
                    &class.name,
                    self.method_name,
                    self.method_descriptor,
                    why,
                );
                dbg_special_site_refusal(
                    "compile-request",
                    &class.name,
                    self.method_name,
                    self.method_descriptor,
                    (target_class, method_name, descriptor),
                    why,
                );
                return None;
            }
            // JIT round 20 wave 7 (lane huntDisp): a site the interpreter
            // answers with a JVMS §6.5 / §5.4.3.3-4 linkage error on EVERY
            // execution ([`invoke_site_linkage_error`]) is left to it, as a
            // §6.5 selection error above is: no compiled route checks the
            // static flag or the reference's tag, and the dispatch helpers
            // ran the method by name (`R20HuntDispLink`). Only broken
            // bytecode reaches the refusal. Correctness, unconditional.
            if [0xb6u8, 0xb7, 0xb8, 0xb9].into_iter().any(|op| {
                self.names_an_invoke_of(cp_idx, op)
                    && invoke_site_linkage_error(
                        self.shared,
                        &cm,
                        self.class_id,
                        cp_idx,
                        target_class,
                        method_name,
                        descriptor,
                        op,
                    )
            }) {
                let why = "invoke-site-linkage-error";
                let verdicts = jit_verdicts(self.shared);
                verdicts.mark_bail_listed(
                    self.class_id,
                    &class.name,
                    self.method_name,
                    self.method_descriptor,
                );
                verdicts.record_compile_refusal(
                    self.class_id,
                    &class.name,
                    self.method_name,
                    self.method_descriptor,
                    why,
                );
                dbg_special_site_refusal(
                    "compile-request",
                    &class.name,
                    self.method_name,
                    self.method_descriptor,
                    (target_class, method_name, descriptor),
                    why,
                );
                return None;
            }
            (
                class_idx,
                (
                    target_class.to_string(),
                    method_name.to_string(),
                    descriptor.to_string(),
                ),
            )
        };
        // JVMS §5.4.3: a site whose CLASS entry holds a recorded resolution
        // failure is left to the interpreter, which rethrows the record; the
        // compiler would otherwise bind the call by name to an owner some
        // other entry loaded since (interpreter round i1 wave 26, lane L5;
        // `i25-L5-invokevirtual-does-not-resolve-an-unloaded-owner-...` (b)).
        // One relaxed load in a VM that never recorded a failure. Asked with
        // the class-manager guard dropped (the probe reads the resolution
        // cache).
        if resolution_failure_is_recorded(self.shared, self.class_id, class_idx) {
            return None;
        }
        // JVMS §5.4.4, where it is enforced (`--real-jdk`): a reference the
        // interpreter refuses with `IllegalAccessError` must not be bound by
        // name either. The resolution core answers from its cache for every
        // site the interpreter already resolved, and refuses (without
        // caching) exactly what the interpreter refuses.
        //
        // A site no door has resolved yet whose record would be observable
        // ([`first_call_cell_for`]) is only PEEKED at: the backend guards it
        // so it resolves at its first execution (interpreter round i1 wave 50,
        // lane L1), as HotSpot's compilers leave it to an uncommon trap.
        if member_access_enforced(self.shared) {
            let resolver = crate::runtime::resolve::MemberResolver::new(self.shared);
            let scope = resolver.scope(self.class_id);
            // Wave 51 (lane L1; stage 1 of
            // `i50-L1-proposal-compile-doors-resolve-nothing`): behind its
            // switch every site is peeked, so compiling a method resolves
            // none of its sites; `method_ref_peek` answers a cached site from
            // the cache, as `method_ref` does.
            let resolved = if self.first_call(cp_idx) != 0 {
                resolver.method_ref_peek(scope, cp_idx)
            } else if door_peeks_unresolved_sites_enabled(self.shared) {
                if false
                    && matches!(
                        resolver.probe_method_ref(scope, cp_idx),
                        crate::runtime::resolve::CacheProbe::Miss
                    )
                {
                    eprintln!(
                        "[first-call] door peeked an unresolved site: class {} cp #{cp_idx}",
                        self.class_id.as_u32()
                    );
                }
                resolver.method_ref_peek(scope, cp_idx)
            } else {
                // An answer resting on a `NestHost` claim no door has loaded
                // the host of is not final (JIT round 20 wave 16, lane nest):
                // the threadless core admits it uncached, so nothing is bound
                // by name and the site's first execution settles the claim.
                let unsettled_reads = || {
                    self.shared
                        .classes
                        .nest_host_unsettled_reads
                        .load(std::sync::atomic::Ordering::Relaxed)
                };
                let before = unsettled_reads();
                let resolved = resolver.method_ref(scope, cp_idx);
                if unsettled_reads() != before {
                    return None;
                }
                // t1 wave 3, lane P (residual 2 of the t1-P page): an answer
                // `method_ref` did not cache may rest on an "unverified" owner
                // (another loader's class the caller's loader has not
                // answered for), whose refusal it does not enforce; the peek
                // refuses that shape, so ask it before binding by name.
                if resolved.is_ok()
                    && matches!(
                        resolver.probe_method_ref(scope, cp_idx),
                        crate::runtime::resolve::CacheProbe::Miss
                    )
                {
                    resolver.method_ref_peek(scope, cp_idx).and(resolved)
                } else {
                    resolved
                }
            };
            if resolved.is_err_and(|e| {
                e.kind() == crate::runtime::resolve::ResolveErrorKind::IllegalAccess
            }) {
                return None;
            }
        }
        Some(answer)
    }

    /// [`first_call_cell_for`] for the invoke at `cp_idx` of this pool: the
    /// armed first-call cell of a site the compile must leave to its first
    /// execution, or `0`. What `CompileRequest::cp_invoke_first_call` asks.
    pub(in crate::runtime::interpreter::jit_bridge) fn first_call(&self, cp_idx: u16) -> usize {
        let cell = first_call_cell_for(self.shared, self.class_id, cp_idx);
        if cell != 0 {
            if let Ok(mut seen) = self.first_calls_seen.try_borrow_mut() {
                if !seen.contains(&cell) {
                    seen.push(cell);
                }
            }
        }
        cell
    }

    /// JVMS §6.5 `invokespecial` super-call redirect — see `try_compile`'s doc
    /// comment on `cp_invokespecial_owner_resolver`. `self.class_id` is the
    /// class whose bytecode is being compiled, i.e. the CALLING class for every
    /// invoke site — exactly the identity the redirect rule needs. `None` (no
    /// override) whenever the redirect does not apply, which is the
    /// overwhelming majority of `invokespecial` sites (constructors, private
    /// methods, ordinary direct-superclass supers).
    pub(in crate::runtime::interpreter::jit_bridge) fn invokespecial_owner(
        &self,
        cp_idx: u16,
        opcode: u8,
    ) -> Option<(String, u32)> {
        let class_id = self.class_id;
        let cm = self.shared.classes.class_manager.read();
        let class = cm.get_class(class_id)?;
        let (class_idx, nat_idx, is_iface) = match class.constant_pool.get(cp_idx) {
            Some(ConstantPoolEntry::MethodReference {
                class_index,
                name_and_type_index,
                ..
            }) => (*class_index, *name_and_type_index, false),
            Some(ConstantPoolEntry::InterfaceMethodReference {
                class_index,
                name_and_type_index,
                ..
            }) => (*class_index, *name_and_type_index, true),
            _ => return None,
        };
        let target_class = class.constant_pool.get_class_name(class_idx)?;
        let (method_name, descriptor) = class.constant_pool.get_name_and_type(nat_idx)?;
        // JVMS 5.4.6 -- an `invokevirtual` (0xb6) that resolves to a PRIVATE
        // method selects exactly that method: no override lookup, no walk up
        // from the receiver. javac emits 0xb6 for a call to a private instance
        // method from Java 11 on (JEP 181 nestmates), where it used to emit
        // `invokespecial` -- so this is now the ordinary encoding of
        // `this.somePrivateHelper()`, including the
        // constructor-calls-its-own-`init()` shape that
        // `io/vertx/core/net/TCPSSLOptions`, `ClientOptionsBase` and
        // `HttpClientOptions` all use, one per level of the same chain.
        //
        // Answering here reclassifies the site as a DIRECT bind at the
        // declaring class, which is what stops the compiled dispatchers
        // resolving it from the receiver and landing on the most-derived
        // same-named private method.
        //
        // Access control makes the answer precise rather than a guess: a
        // private method is invocable only from the class that declares it, so
        // the constant pool's owner name is this compiling class itself and no
        // loader-blind name lookup is in play.
        //
        // Every substituted owner is recorded with its id
        // (`SubstitutedOwners`): it is a DECLARING class or a selection
        // start, not the class the constant pool names, so the door's direct
        // binder neither access-checks it against this class nor looks it up
        // again by name (i12-L2). The constant-pool class itself is resolved
        // through `jit_known_class` on every arm, which is the §5.4.4 check.
        // The id also rides to run time in the site's
        // `JitInvokeInfo::owner_class_id`, so the dispatch helper does not
        // resolve the substituted name in this class's loader either
        // (interpreter round i1 wave 14, L2).
        let substituted = |id: ClassId, name: &str| -> (String, u32) {
            self.substituted_owners
                .note(class_id, name, method_name, descriptor, id);
            (name.to_string(), id.as_u32())
        };
        if opcode == 0xb6 {
            let cp_class_id = jit_known_class(self.shared, &cm, target_class, class_id)?;
            let store = cm.class_store();
            if let Some(declaring_id) = crate::classloading::invokevirtual_private_declaring_class(
                cp_class_id,
                method_name,
                descriptor,
                store,
            ) {
                return store
                    .get(declaring_id)
                    .map(|c| substituted(declaring_id, &*c.name));
            }
            // Not private, but possibly unoverridable anyway — a `final`
            // method, or any method of a `final` class, has exactly one
            // possible target at this site for the same reason a private one
            // does. Same conclusion (statically bound; substitute the
            // DECLARING class, which for this rule is often NOT the class the
            // constant pool names), reached by a different argument. Both
            // rules live outside this file — the private one in
            // `classloading::invokevirtual_private_declaring_class`, this one
            // in `invoke::invokevirtual_site_final_owner`.
            return super::invoke::invokevirtual_site_final_owner(
                self.shared,
                &cm,
                class_id,
                target_class,
                method_name,
                descriptor,
            )
            .map(|(owner, owner_id)| substituted(owner_id, &owner));
        }
        // JVMS §6.5 `invokeinterface`, selection step 1: an `0xb9` site whose
        // resolved method is a PRIVATE interface method (a default method
        // calling its interface's own private helper) selects exactly that
        // method. Answering with the declaring interface reclassifies the site
        // as a direct bind, exactly as the `0xb6` arm above does for a private
        // `invokevirtual`; without it the MIC/PIC helper resolved the call from
        // the RECEIVER and ran its unrelated public namesake. `None` for every
        // non-private `0xb9` site, which stays ordinary interface dispatch.
        if opcode == 0xb9 {
            let cp_class_id = jit_known_class(self.shared, &cm, target_class, class_id)?;
            let store = cm.class_store();
            return crate::classloading::invokevirtual_private_declaring_class(
                cp_class_id,
                method_name,
                descriptor,
                store,
            )
            .and_then(|declaring_id| {
                store
                    .get(declaring_id)
                    .map(|c| substituted(declaring_id, &*c.name))
            });
        }
        if opcode != 0xb7 {
            return None;
        }
        // A grandparent named by a super call is found on the caller's chain
        // when the loader has not initiated it (`special_site_cp_class`).
        let cp_class_id = special_site_cp_class(self.shared, &cm, class_id, target_class)?;
        let store = cm.class_store();
        // `--real-jdk`: the interpreter's JVMS §6.5 selection
        // (`invokespecial_owner_class_name`, interpreter round i1 wave 32), so
        // a static declaration on the way is skipped here too. A selection
        // that ends in an error never reaches here: `Self::invoke` declines
        // the site first, and the compile bails (interpreter round i1 wave
        // 37, lane L2); the lenient start below is only a fallback.
        if self.shared.classes.selection_checks && method_name != "<init>" {
            use crate::runtime::resolve::selection::{select_special, SpecialSelection};
            if let SpecialSelection::Owner(id) = select_special(
                store,
                class_id,
                cp_class_id,
                is_iface,
                method_name,
                descriptor,
            ) {
                if id == cp_class_id {
                    return None;
                }
                return store.get(id).map(|c| substituted(id, &*c.name));
            }
        }
        let start = crate::classloading::invokespecial_selection_start(
            class_id,
            cp_class_id,
            is_iface,
            method_name,
            store,
        );
        if start == cp_class_id {
            return None;
        }
        store.get(start).map(|c| substituted(start, &*c.name))
    }

    /// invokedynamic-uncommon-trap fix: an invokedynamic CP index's target
    /// descriptor (no bootstrap/CallSite resolution — the codegen only needs
    /// the site's arg/return stack effect), plus its BRIDGE SITE (`0` when the
    /// bootstrap is one the bridge cannot serve). Folded into one resolver
    /// because `indy_info`'s fifth element is filled in the same loop.
    ///
    /// The `cratonvm_jit` resolver is handed only the CP index, so the
    /// instruction is recovered from this method's bytecode: exact when one
    /// `invokedynamic` of the method uses `cp_idx` (the normal case: javac
    /// shares an entry only between identical method references), unnamed
    /// otherwise (see [`interned_jit_indy_bridge_site`]).
    pub(in crate::runtime::interpreter::jit_bridge) fn indy(
        &self,
        cp_idx: u16,
    ) -> Option<(String, usize)> {
        self.indy_for(cp_idx, sole_indy_bci_of(self.code, cp_idx))
    }

    /// JIT round 16 wave 10 (lane indyir; `CRATONVM_JIT_INDY_SITE_BY_PC`,
    /// default OFF): [`Self::indy`] for the `invokedynamic` AT `bci`, which a
    /// door that knows the instruction asks
    /// (`CompileRequest::cp_invokedynamic_site_resolver`). With the switch on,
    /// an entry several instructions share is answered per instruction -- the
    /// key the interpreter links it under -- instead of being left unnamed
    /// (which refuses a linked or lambda bridge, so such a method traps at
    /// every such site in both tiers: `R16IndyLinked` `twice`, two
    /// instructions of one entry, ran ~3.4 us per call). With it off, or for a
    /// `bci` that is not this entry's `invokedynamic`, exactly [`Self::indy`].
    pub(in crate::runtime::interpreter::jit_bridge) fn indy_at(
        &self,
        cp_idx: u16,
        bci: usize,
    ) -> Option<(String, usize)> {
        if crate::runtime::invokedynamic::jit_indy_site_by_pc_enabled()
            && indy_operand_at(self.code, bci) == Some(cp_idx)
        {
            return self.indy_for(cp_idx, Some(bci));
        }
        self.indy(cp_idx)
    }

    /// JIT round 16 wave 10 (lane indyir, IN16-8;
    /// `CRATONVM_JIT_INDY_CONSTANT_INVOKE`, default OFF): the method the
    /// `invokedynamic` at `bci` may be compiled AS by the optimizing tier --
    /// `(class, name, descriptor, invoke_kind, class id)` -- or `None` to keep
    /// the bridge.
    ///
    /// The VM half of the answer is
    /// `invokedynamic::jit_indy_constant_direct_target` (a bound direct target
    /// of a `ConstantCallSite`, learnt by the bridge). This half makes the
    /// plain call the jit crate then plans indistinguishable from the bridge's:
    ///
    /// * the bridge site is the one the bridge would use (the same instruction
    ///   key, [`Self::indy_at`]);
    /// * the target's NAME, resolved from this class, is the target class
    ///   itself: every by-name question the plan asks (the inline resolver,
    ///   the direct binder, the cycle sets) then names the right class --
    ///   or, JIT round 21 lane indylate2 (`CRATONVM_JIT_INDY_ADMIT_BY_ID`),
    ///   the name is not known from this class at all and the target is
    ///   admitted by the id the handle names ([`Self::indy_target_admission`]);
    /// * the caller may call it directly: a `public` method, or one of this
    ///   class's own (a handle may expose a private method of another class,
    ///   which a plain call from here must not reach);
    /// * not `synchronized`, `native` or `abstract`: those keep the bridge,
    ///   whose dispatch handles them, rather than meeting the plan's
    ///   synchronized-direct and native rows with a constant-pool index that
    ///   names no `Methodref`.
    pub(in crate::runtime::interpreter::jit_bridge) fn indy_direct(
        &self,
        cp_idx: u16,
        bci: usize,
    ) -> Option<(String, String, String, u8, u32)> {
        if !crate::runtime::invokedynamic::jit_indy_constant_invoke_enabled()
            || indy_operand_at(self.code, bci) != Some(cp_idx)
        {
            return None;
        }
        let (_, site) = self.indy_at(cp_idx, bci)?;
        if site == 0 {
            return None;
        }
        let target =
            crate::runtime::invokedynamic::jit_indy_constant_direct_target(self.shared, site)?;
        let admission = self.indy_target_admission(
            &target.class,
            &target.name,
            &target.desc,
            target.invoke_kind,
            target.class_id,
        );
        let admitted = admission.is_ok();
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[indy-constant] {}{} bci={bci} cp#{cp_idx}: {} {}.{}{} kind={} ({})",
                self.method_name,
                self.method_descriptor,
                if admitted {
                    "compiled as a call of"
                } else {
                    "refused (bridge kept) for"
                },
                target.class,
                target.name,
                target.desc,
                target.invoke_kind,
                match admission {
                    Ok(reason) | Err(reason) => reason,
                },
            );
        }
        admitted.then(|| {
            (
                target.class,
                target.name,
                target.desc,
                target.invoke_kind,
                target.class_id,
            )
        })
    }

    /// JIT round 20 wave 11 (lane pIndy, SPLICE19-1;
    /// `CRATONVM_JIT_INDY_MUTABLE_GUARD`, default ON): for the
    /// `invokedynamic` at `bci` whose call site is a `MutableCallSite` /
    /// `VolatileCallSite`, the method the optimizing tier may compile it as
    /// BEHIND A GUARD on the call site's target -- the call, as
    /// [`Self::indy_direct`] answers it, and `(bridge site, call-site slot,
    /// target slot, CallSite.target field slot)` -- or `None` to keep the
    /// bridge. The VM half is `invokedynamic::jit_indy_mutable_guard_target`
    /// (the target the bridge bound last); this half applies
    /// [`Self::indy_direct`]'s caller-side terms to it
    /// ([`Self::indy_target_admission`]), so the planned call is one this
    /// class may make plainly.
    pub(in crate::runtime::interpreter::jit_bridge) fn indy_mutable_guard(
        &self,
        cp_idx: u16,
        bci: usize,
    ) -> Option<cratonvm_jit::compile_request::IndyMutableGuardAnswer> {
        if !crate::runtime::invokedynamic::jit_indy_mutable_guard_enabled()
            || indy_operand_at(self.code, bci) != Some(cp_idx)
        {
            return None;
        }
        let (_, site) = self.indy_at(cp_idx, bci)?;
        if site == 0 {
            return None;
        }
        let (target, call_site_slot, target_slot, field_index, planned_cell, field_row) =
            crate::runtime::invokedynamic::jit_indy_mutable_guard_target(self.shared, site)?;
        let admission = self.indy_target_admission(
            &target.class,
            &target.name,
            &target.desc,
            target.invoke_kind,
            target.class_id,
        );
        let admitted = admission.is_ok();
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[indy-mutable] {}{} bci={bci} cp#{cp_idx}: {} {}.{}{} kind={} ({})",
                self.method_name,
                self.method_descriptor,
                if admitted {
                    "guarded call planned of"
                } else {
                    "refused (bridge kept) for"
                },
                target.class,
                target.name,
                target.desc,
                target.invoke_kind,
                match admission {
                    Ok(reason) | Err(reason) => reason,
                },
            );
        }
        // INDY21 closure (`CRATONVM_JIT_INDY_MUTABLE_RETARGET_RECOMPILE`): the
        // cell this compile plans against is recorded only for a plan the
        // admission accepted (a refused target builds no guard, so a retarget
        // away from its cell must not arm the site).
        if admitted {
            crate::runtime::invokedynamic::jit_indy_mutable_note_planned(site, planned_cell);
        }
        // JIT round 21 wave 1 (lane indy, INDY21-1,
        // `CRATONVM_JIT_INDY_MUTABLE_BIMORPHIC`): the second target the guard
        // may test before the bridge, `(0, 0)` with the switch off. After the
        // planned cell was recorded just above.
        let second = if admitted {
            crate::runtime::invokedynamic::jit_indy_mutable_second_target(self.shared, site)
        } else {
            (0, 0)
        };
        if second != (0, 0) && crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[indy-mutable] {}{} bci={bci} cp#{cp_idx}: bimorphic, second target cell {:#x}",
                self.method_name, self.method_descriptor, second.1,
            );
        }
        admitted.then(|| {
            (
                (
                    target.class,
                    target.name,
                    target.desc,
                    target.invoke_kind,
                    target.class_id,
                ),
                (site, call_site_slot, target_slot, field_index),
                second,
                // mcs21 (MCS21-2): `CallSite.target`'s compact row (`0`: none).
                field_row,
            )
        })
    }

    /// JIT round 21 wave 17 (lane records, IN16-6;
    /// `CRATONVM_JIT_IR_INDY_RECORD_FOLD`, perf, default ON, `=0` keeps the
    /// bridge): the field-wise fold the optimizing tier may build for the
    /// `invokedynamic` at `bci`, and the class id of `java/util/Objects`, or
    /// `None` to keep the bridge (`IrBuilder::try_indy_record_fold`).
    ///
    /// Answered only for a record class's OWN javac-generated `hashCode()I`
    /// / `equals(Ljava/lang/Object;)Z` body
    /// (`Class::generated_record_object_methods`: a `final` record, the body
    /// exactly `aload_0; [aload_1;] invokedynamic; ireturn`, the bootstrap
    /// `ObjectMethods.bootstrap` with javac's canonical getter list) -- so the
    /// fold's `isInstance` is the exact class test, its getters are this
    /// class's own fields, and the method has no exception table. The site
    /// must be bridged (its bootstrap resolved, [`Self::indy_at`]); each getter
    /// (`REF_getField`) is resolved as the `getfield` of its `Fieldref` is
    /// ([`jit_resolve_field_site`]), and must be an instance field this class
    /// declares.
    pub(in crate::runtime::interpreter::jit_bridge) fn indy_record_fold(
        &self,
        cp_idx: u16,
        bci: usize,
    ) -> Option<cratonvm_jit::compile_request::IndyRecordFoldAnswer> {
        use crate::classloading::{RECORD_OBJ_EQUALS, RECORD_OBJ_HASH_CODE};
        if indy_operand_at(self.code, bci) != Some(cp_idx) {
            return None;
        }
        let equals = match (self.method_name, self.method_descriptor) {
            ("hashCode", "()I") => false,
            ("equals", "(Ljava/lang/Object;)Z") => true,
            _ => return None,
        };
        // javac's body: the `invokedynamic` right after the `aload`s.
        let indy_bci = if equals { 2 } else { 1 };
        if bci != indy_bci {
            return None;
        }
        let (_, site) = self.indy_at(cp_idx, bci)?;
        if site == 0 {
            return None;
        }
        let (getter_fields, objects_class_id) = {
            let cm = self.shared.classes.class_manager.read();
            let class = cm.get_class(self.class_id)?;
            let bit = if equals {
                RECORD_OBJ_EQUALS
            } else {
                RECORD_OBJ_HASH_CODE
            };
            if class.generated_record_object_methods() & bit == 0 {
                return None;
            }
            let bsm_index = match class.constant_pool.get(cp_idx)? {
                ConstantPoolEntry::InvokeDynamic {
                    bootstrap_method_attr_index,
                    ..
                } => usize::from(*bootstrap_method_attr_index),
                _ => return None,
            };
            let bsm = class.bootstrap_methods.get(bsm_index)?;
            let mut fields: Vec<u16> = Vec::new();
            for &arg in bsm.bootstrap_arguments.get(2..)? {
                match class.constant_pool.get(arg)? {
                    ConstantPoolEntry::MethodHandle {
                        reference_kind: 1,
                        reference_index,
                    } => fields.push(*reference_index),
                    _ => return None,
                }
            }
            let objects = cm.find_bootstrap_class_by_name("java/util/Objects")?;
            (fields, objects.as_u32())
        };
        let mut components: Vec<(u32, u8)> = Vec::with_capacity(getter_fields.len());
        for field_cp in getter_fields {
            let field = jit_resolve_field_site(self.shared, self.class_id, field_cp).ok()?;
            if !jit_field_kind_agrees(&field, false) || field.declaring_class_id != self.class_id {
                return None;
            }
            let tag = jit_deferred_field_tag(self.shared, self.class_id, field_cp)?;
            if !jit_field_tag_agrees(&field, tag, field_cp) {
                return None;
            }
            components.push((u32::try_from(field.field_index).ok()?, tag));
        }
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[indy-record-fold] {}{} bci={bci} cp#{cp_idx}: {} over {} component(s) \
                 {:?}",
                self.method_name,
                self.method_descriptor,
                if equals { "equals" } else { "hashCode" },
                components.len(),
                components
                    .iter()
                    .map(|&(index, tag)| format!("{index}:{}", tag as char))
                    .collect::<Vec<_>>(),
            );
        }
        let string_components =
            record_fold_string_components(self.shared, self.class_id, &components);
        Some((
            cratonvm_jit::ir::IndyRecordFold {
                equals,
                record_class_id: self.class_id.as_u32(),
                components,
                string_components,
                // Filled per compile by the JIT (`record_fold_admit_string_hash`).
                string_hash_slot: None,
            },
            objects_class_id,
        ))
    }

    /// JIT round 20 wave 11 (lane pIndy): the caller-side admission of a
    /// planned target, for both [`Self::indy_direct`] and
    /// [`Self::indy_mutable_guard`]: the class is the target class (by name or
    /// by id, [`indy_target_class_admission`]); the method is `public` or this
    /// class's own; not `synchronized`, `native` or `abstract`; static exactly
    /// when the call is.
    ///
    /// JIT round 21 lane indylate2
    /// (`jr21-indylate2-indy-target-refused-for-a-child-loader-caller-20261014.md`):
    /// `Ok` names the admit reason (`"by-name"` / `"by-id"`) and `Err` the
    /// refusal, for the `[indy-constant]` / `[indy-mutable]` debug lines.
    pub(in crate::runtime::interpreter::jit_bridge) fn indy_target_admission(
        &self,
        class: &str,
        name: &str,
        desc: &str,
        invoke_kind: u8,
        class_id: u32,
    ) -> Result<&'static str, &'static str> {
        use cratonvm_reader::class_access_flags::MethodAccessFlags;
        if class_id == 0 {
            return Err("no class id");
        }
        let cm = self.shared.classes.class_manager.read();
        let id = ClassId::new(class_id);
        let by_name = jit_known_class_at(self.shared, &cm, class, self.class_id, None);
        // The by-id terms, asked only where they decide (`by_name` is `None`):
        // a target of a built-in loader (never unloaded, so the id the
        // compiled call bakes as its class context stays that class), not
        // hidden (a hidden class has no name to resolve the call by), named
        // `class`, whose name resolved from ITSELF is itself -- the question
        // every by-name lookup of the planned call asks from that class
        // context (`IrIndyDirectPlan::class_context_at`, and the jit crate's
        // `indy_context_*` resolvers).
        let by_id_terms = || {
            cm.get_loader_id(id).is_some_and(|l| l.is_builtin())
                && cm
                    .get_class(id)
                    .is_some_and(|c| !c.hidden && &*c.name == class)
                && jit_known_class_at(self.shared, &cm, class, id, None) == Some(id)
        };
        let reason = indy_target_class_admission(
            by_name.map(|found| found.as_u32()),
            class_id,
            jit_indy_admit_by_id_enabled(),
            by_id_terms,
        )?;
        let owner = cm.get_class(id).ok_or("target class not loaded")?;
        let method = owner
            .methods
            .iter()
            .find(|m| &*m.name == name && &*m.descriptor == desc)
            .ok_or("target method not found")?;
        let flags = method.access_flags;
        if flags.intersects(
            MethodAccessFlags::SYNCHRONIZED
                | MethodAccessFlags::NATIVE
                | MethodAccessFlags::ABSTRACT,
        ) {
            return Err("synchronized, native or abstract target");
        }
        if !(id == self.class_id || flags.contains(MethodAccessFlags::PUBLIC)) {
            return Err("non-public target of another class");
        }
        if method.is_static() != (invoke_kind == 3) {
            return Err("static-ness differs from the call kind");
        }
        Ok(reason)
    }

    /// JIT round 21 wave 19 (lane mhbind2; MHBIND21-1,
    /// `CRATONVM_JIT_MH_CONSTANT_COMPILE_BOUND`): for a site
    /// [`Self::mh_constant`] answered with `invoke_kind 4`, the `(argL1 slot,
    /// bound value's exact class id)` of the REAL `bindTo` result the `static
    /// final` field of `field_cp` holds -- the row of that handle, by the same
    /// reads (the field's address in this collection epoch, or the field's own
    /// row), or `None` (the jit crate then drops the site). No heap object is
    /// dereferenced.
    pub(in crate::runtime::interpreter::jit_bridge) fn mh_constant_bound(
        &self,
        invoke_cp: u16,
        _bci: usize,
        field_cp: u16,
    ) -> Option<(u32, u32)> {
        // j27 wave 6 (lane T1LAM2): a bound STATIC row (`invoke_kind 6`) too,
        // under `CRATONVM_JIT_MH_CONSTANT_COMPILE_BOUND_STATIC`.
        let (bound_on, bound_static_on) = (
            mh_constant_compile_bound_enabled(),
            mh_constant_compile_bound_static_enabled(),
        );
        let kind_ok = |t: &MhConstantTarget| {
            mh_constant_bound_kind_admitted(t.invoke_kind, bound_on, bound_static_on)
        };
        if !mh_constant_compile_enabled(self.shared) || !(bound_on || bound_static_on) {
            return None;
        }
        let (_, _, site) = self.invoke(invoke_cp)?;
        let field = self.resolved_field(None, field_cp)?;
        if !field.is_static || !field.is_final || !field.is_reference || field.is_volatile {
            return None;
        }
        // JIT round j27 (lane MHINDY,
        // `j27-mh-constant-bound-and-filter-doors-read-the-wrong-epoch-20261028.md`):
        // the epoch the fills key the address rows by (`mem.address_epoch()`,
        // as `Self::mh_constant` reads it), not the heap's collection count --
        // the two differ once G1 recycles a region or a concurrent sweep
        // begins, and every address row then missed.
        let realm = &self.shared.mem;
        let epoch = realm.address_epoch();
        let value =
            crate::vm::get_static_shared(self.shared, field.declaring_class_id, field.field_index);
        if realm.address_epoch() != epoch {
            return None;
        }
        let cratonvm_types::Value::Object(Some(handle)) = value else {
            return None;
        };
        let holder = field.declaring_class_id.as_u32();
        // The field's own row first, as `mh_constant` reads it (its witness
        // was checked there; a row and an address row that disagree refused
        // the site there), then the address row of this epoch.
        mh_constant_field_row_of(self.shared, holder, field.field_index, &site)
            .filter(kind_ok)
            .or_else(|| {
                mh_constant_target_of(self.shared, handle.as_ptr() as u64, epoch, &site)
                    .filter(kind_ok)
            })
            .and_then(|t| t.bound)
    }

    /// JIT round 21 wave 20 (lane mhfilter; MHBIND2-1,
    /// `CRATONVM_JIT_MH_CONSTANT_COMPILE_FILTER`): for a site
    /// [`Self::mh_constant`] answered with `invoke_kind 5`, the PRE call of the
    /// REAL filter species the `static final` field of `field_cp` holds -- the
    /// row of that handle, by [`Self::mh_constant_bound`]'s reads -- or `None`
    /// (the jit crate then drops the site). No heap object is dereferenced.
    ///
    /// JIT round j27 (lane MHINDY; page
    /// `jr21-mhfilter-pre-call-frame-reports-the-getstatic-line-20261014.md`):
    /// asked once per site, with the site's `getstatic` bci; `None` when that
    /// bci and the invoke's `bci` name different source lines of this method.
    /// The PRE call is keyed by the `getstatic` pc, so a trace taken while it
    /// runs names that pc's line, where HotSpot names the `invokeExact`'s.
    pub(in crate::runtime::interpreter::jit_bridge) fn mh_constant_filter(
        &self,
        invoke_cp: u16,
        bci: usize,
        field_cp: u16,
        getstatic_bci: usize,
    ) -> Option<cratonvm_jit::compile_request::MhConstantFilterRow> {
        if !mh_constant_compile_enabled(self.shared) || !mh_constant_compile_filter_enabled() {
            return None;
        }
        if !self.mh_constant_filter_window_is_one_line(getstatic_bci, bci) {
            return None;
        }
        let (_, _, site) = self.invoke(invoke_cp)?;
        let field = self.resolved_field(None, field_cp)?;
        if !field.is_static || !field.is_final || !field.is_reference || field.is_volatile {
            return None;
        }
        // JIT round j27 (lane MHINDY): the fills' epoch, as in
        // `Self::mh_constant_bound`.
        let realm = &self.shared.mem;
        let epoch = realm.address_epoch();
        let value =
            crate::vm::get_static_shared(self.shared, field.declaring_class_id, field.field_index);
        if realm.address_epoch() != epoch {
            return None;
        }
        let cratonvm_types::Value::Object(Some(handle)) = value else {
            return None;
        };
        let holder = field.declaring_class_id.as_u32();
        mh_constant_field_row_of(self.shared, holder, field.field_index, &site)
            .filter(|t| t.invoke_kind == 5)
            .or_else(|| {
                mh_constant_target_of(self.shared, handle.as_ptr() as u64, epoch, &site)
                    .filter(|t| t.invoke_kind == 5)
            })
            .and_then(|t| t.filter.map(|f| f.row()))
    }

    /// Converting REAL `asType` handle (`invoke_kind 7`) row resolver.
    pub(in crate::runtime::interpreter::jit_bridge) fn mh_constant_convert(
        &self,
        invoke_cp: u16,
        _bci: usize,
        field_cp: u16,
        _getstatic_bci: usize,
    ) -> Option<cratonvm_jit::compile_request::MhConstantConvertRow> {
        if !mh_constant_compile_enabled(self.shared) || !mh_constant_compile_converting_enabled() {
            return None;
        }
        let (_, _, site) = self.invoke(invoke_cp)?;
        let field = self.resolved_field(None, field_cp)?;
        if !field.is_static || !field.is_final || !field.is_reference || field.is_volatile {
            return None;
        }
        let realm = &self.shared.mem;
        let epoch = realm.address_epoch();
        let value =
            crate::vm::get_static_shared(self.shared, field.declaring_class_id, field.field_index);
        if realm.address_epoch() != epoch {
            return None;
        }
        let cratonvm_types::Value::Object(Some(handle)) = value else {
            return None;
        };
        let holder = field.declaring_class_id.as_u32();
        mh_constant_field_row_of(self.shared, holder, field.field_index, &site)
            .filter(|t| t.invoke_kind == 7)
            .or_else(|| {
                mh_constant_target_of(self.shared, handle.as_ptr() as u64, epoch, &site)
                    .filter(|t| t.invoke_kind == 7)
            })
            .and_then(|t| t.convert.map(|c| *c))
    }

    /// JIT round j27 (lane MHINDY): do bcis `getstatic_bci` and `bci` of the
    /// compiled method map to one `LineNumberTable` line (JVMS §4.7.12: the
    /// entry with the largest `start_pc <= bci`)? Two absent lines agree (a
    /// trace prints no line for either). A method this class does not hold
    /// answers `false`.
    pub(in crate::runtime::interpreter::jit_bridge) fn mh_constant_filter_window_is_one_line(
        &self,
        getstatic_bci: usize,
        bci: usize,
    ) -> bool {
        let cm = self.shared.classes.class_manager.read();
        let Some(class) = cm.get_class(self.class_id) else {
            return false;
        };
        let Some(method) = class
            .methods
            .iter()
            .find(|m| *m.name == *self.method_name && *m.descriptor == *self.method_descriptor)
        else {
            return false;
        };
        mh_filter_line_at(method, getstatic_bci) == mh_filter_line_at(method, bci)
    }

    /// JIT round 21 wave 20 (lane mhfilter; MHBIND2-1): the `invoke_kind 5`
    /// arm of [`Self::mh_constant`]'s member check, for a row `t` whose member
    /// already resolved from this class to `id` and passed the shared flag and
    /// access rules: the MAIN member by the rule of its kind on its own type
    /// (static; or an instance method, not private, of a class, with its
    /// receiver's class), the site the text shape the JDK gives the species
    /// (`mh_filter_row_fits`), and the PRE member by the same rules
    /// ([`Self::mh_constant_filter_pre_admitted`]).
    pub(in crate::runtime::interpreter::jit_bridge) fn mh_constant_filter_admitted(
        &self,
        cm: &crate::classloading::ClassManager,
        t: &MhConstantTarget,
        id: ClassId,
        site: &str,
    ) -> bool {
        use cratonvm_reader::class_access_flags::MethodAccessFlags;
        let Some(f) = t.filter.as_deref() else {
            return false;
        };
        let Some(class) = cm.get_class(id) else {
            return false;
        };
        let Some(method) = class
            .methods
            .iter()
            .find(|m| *m.name == *t.name && *m.descriptor == *t.desc)
        else {
            return false;
        };
        let main_ok = match f.main_kind {
            3 => {
                method.is_static()
                    && mh_constant_descriptor_agrees(self.shared, cm, self.class_id, id, &t.desc)
            }
            0 => {
                let own_type = format!("(L{};{}", t.class, t.desc.get(1..).unwrap_or(""));
                !method.is_static()
                    && !method.access_flags.contains(MethodAccessFlags::PRIVATE)
                    && !class.is_interface()
                    && mh_constant_descriptor_agrees(self.shared, cm, self.class_id, id, &own_type)
            }
            _ => false,
        };
        main_ok
            && cratonvm_jit::compile_request::mh_filter_row_fits(site, &t.class, &t.desc, &f.row())
            && self.mh_constant_filter_pre_admitted(cm, f)
    }

    /// JIT round 21 wave 20 (lane mhfilter; MHBIND2-1): may this class call a
    /// filter species' PRE member `f` as a plain call, by the rules
    /// [`Self::mh_constant`] asks of a row's own member: its name resolves
    /// from this class to the recorded class id, it is not synchronized,
    /// native or abstract, `public` or this class's own (or the nonpublic
    /// rule), static-ness per its kind (an instance one neither private nor
    /// of an interface), and every class its own type names is one class
    /// from both loaders.
    pub(in crate::runtime::interpreter::jit_bridge) fn mh_constant_filter_pre_admitted(
        &self,
        cm: &crate::classloading::ClassManager,
        f: &MhConstantFilter,
    ) -> bool {
        use cratonvm_reader::class_access_flags::MethodAccessFlags;
        jit_known_class_at(self.shared, cm, &f.pre_class, self.class_id, None)
            .filter(|id| id.as_u32() == f.pre_class_id)
            .and_then(|id| {
                let class = cm.get_class(id)?;
                let method = class
                    .methods
                    .iter()
                    .find(|m| *m.name == *f.pre_name && *m.descriptor == *f.pre_desc)?;
                let flags = method.access_flags;
                let callable = !flags.intersects(
                    MethodAccessFlags::SYNCHRONIZED
                        | MethodAccessFlags::NATIVE
                        | MethodAccessFlags::ABSTRACT,
                ) && (id == self.class_id
                    || flags.contains(MethodAccessFlags::PUBLIC)
                    || mh_constant_nonpublic_member_callable(cm, self.class_id, class, flags))
                    && match f.pre_kind {
                        3 => {
                            method.is_static()
                                && mh_constant_descriptor_agrees(
                                    self.shared,
                                    cm,
                                    self.class_id,
                                    id,
                                    &f.pre_desc,
                                )
                        }
                        0 => {
                            !method.is_static()
                                && !flags.contains(MethodAccessFlags::PRIVATE)
                                && !class.is_interface()
                                && mh_constant_descriptor_agrees(
                                    self.shared,
                                    cm,
                                    self.class_id,
                                    id,
                                    &format!(
                                        "(L{};{}",
                                        f.pre_class,
                                        f.pre_desc.get(1..).unwrap_or("")
                                    ),
                                )
                        }
                        _ => false,
                    };
                callable.then_some(())
            })
            .is_some()
    }

    /// JIT round 16 wave 17 (lane mh7; MH16-8 at compile time,
    /// `CRATONVM_JIT_MH_CONSTANT_COMPILE`, default ON): the static method the
    /// `invokevirtual MethodHandle.invokeExact` / `invoke` at `bci` (constant
    /// pool `invoke_cp`) may be compiled AS by the optimizing tier --
    /// `(class, name, descriptor, 3, class id)` -- when its receiver is the
    /// value of the `getstatic` of `field_cp` (the jit crate proves that from
    /// straight-line code, `mh_constant_receiver_getstatic`), or `None` to keep
    /// the call.
    ///
    /// The handle is a COMPILE-TIME constant only when:
    ///
    /// * the field is a resolved `static final` reference of a class whose
    ///   initialization has COMPLETED, so nothing writes it again (the JVMS
    ///   §6.5 `putstatic` rule; HotSpot trusts the same fields);
    /// * the handle it holds now is one the dispatch helpers' constant bind
    ///   admitted for a call site of this descriptor, as an exact direct
    ///   STATIC handle ([`note_mh_constant_target`]): the address read here
    ///   and the recorded one are equal in one collection epoch, read before
    ///   and after the field (an equal address in an equal epoch is the same
    ///   object, the rule every address-keyed memo of the handle doors uses).
    ///   No heap object is dereferenced, so a background compile may ask.
    ///
    /// The member must then be one this class may call as a plain
    /// `invokestatic`, as [`Self::indy_direct`] asks of a constant call site's
    /// target: its name, resolved from this class, is that class; it is
    /// static, `public` or this class's own, and not `synchronized`, `native`
    /// or `abstract`. The admission already refused `java/`, `jdk/` and `sun/`
    /// members (caller-sensitive methods see their caller).
    ///
    /// JIT round 17 wave 1 (lane mhconst): the field's own row answers first
    /// when `CRATONVM_JIT_MH_CONSTANT_FIELD_ROWS` is on (MH7-2,
    /// [`mh_constant_field_row_of`]); a miss is remembered for a recompile when
    /// `CRATONVM_JIT_MH_CONSTANT_RECOMPILE` is on (MH7-1,
    /// [`note_mh_constant_waiter`]); and the member's descriptor must name the
    /// same classes from both loaders ([`mh_constant_descriptor_agrees`]).
    ///
    /// JIT round 17 wave 3 (lane mh4; MHCONST17-2, opt-in
    /// [`mh_constant_virtual_enabled`]): an admitted unbound VIRTUAL handle is
    /// answered as `(class, name, descriptor, 0, class id)` -- a non-static,
    /// non-private member of a class (not an interface) whose descriptor is
    /// the site's less its leading receiver, the site's classes one class
    /// from both loaders. A field row of an older collection epoch answers
    /// only through its witness ([`mh_constant_field_row_witness_holds`]).
    pub(in crate::runtime::interpreter::jit_bridge) fn mh_constant(
        &self,
        invoke_cp: u16,
        bci: usize,
        field_cp: u16,
    ) -> Option<(String, String, String, u8, u32)> {
        use cratonvm_reader::class_access_flags::MethodAccessFlags;
        if !mh_constant_compile_enabled(self.shared) {
            return None;
        }
        let (holder_class, door, site) = self.invoke(invoke_cp)?;
        let door_admitted = door == "invokeExact"
            || (door == "invoke"
                && cratonvm_native_builtins::lang_invoke::mh_constant_bind_invoke_enabled());
        if holder_class != "java/lang/invoke/MethodHandle" || !door_admitted {
            return None;
        }
        // JIT r23 wave 4b, lane L2 (L2W4B-1, `CRATONVM_JIT_MH_CONSTANT_CONDY`):
        // the receiver walk also hands over an `ldc` of a condy; its handle's
        // member was decoded when the entry resolved (`note_mh_condy_member`).
        // A condy entry never reaches the field path below.
        let receiver_is_condy = {
            let cm = self.shared.classes.class_manager.read();
            cm.get_class(self.class_id).is_some_and(|c| {
                matches!(
                    c.constant_pool.get(field_cp),
                    Some(ConstantPoolEntry::Dynamic { .. })
                )
            })
        };
        if receiver_is_condy {
            if !mh_constant_condy_enabled() {
                return None;
            }
            let member = self
                .shared
                .jit
                .bridge
                .mh_condy_members
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&(self.class_id.as_u32(), field_cp))
                .cloned()?;
            let answer = {
                let cm = self.shared.classes.class_manager.read();
                mh_condy_answer(&cm, &member, &site).filter(|(_, _, desc, kind, class_id)| {
                    // The site's classes are one class from the caller's and
                    // the member's loaders, the field-held bind's rule.
                    let checked = if *kind == 3 {
                        desc.as_str()
                    } else {
                        site.as_str()
                    };
                    mh_constant_descriptor_agrees(
                        self.shared,
                        &cm,
                        self.class_id,
                        ClassId::new(*class_id),
                        checked,
                    )
                })
            };
            if crate::jit::helpers::mh_constant_bind_dbg() {
                eprintln!(
                    "[mh-bind] compile {}{} bci={bci} MethodHandle.{door}{site} condy cp#{field_cp} -> {}",
                    self.method_name,
                    self.method_descriptor,
                    match &answer {
                        Some((c, n, d, k, _)) => format!("{c}.{n}{d} kind {k}"),
                        None => format!("refused {}.{}{} ref_kind {}", member.class, member.name, member.desc, member.ref_kind),
                    }
                );
            }
            return answer;
        }
        let field = self.resolved_field(None, field_cp)?;
        if !field.is_static || !field.is_final || !field.is_reference || field.is_volatile {
            return None;
        }
        if !crate::vm::is_class_initialized_via_manager(self.shared, field.declaring_class_id) {
            return None;
        }
        let realm = &self.shared.mem;
        let epoch = realm.address_epoch();
        let value =
            crate::vm::get_static_shared(self.shared, field.declaring_class_id, field.field_index);
        if realm.address_epoch() != epoch {
            return None;
        }
        let cratonvm_types::Value::Object(Some(handle)) = value else {
            return None;
        };
        let holder = field.declaring_class_id.as_u32();
        // JIT round 22 (lane memory, FFM22-1): a bound FFM downcall handle in
        // this field gets its compiled-call row now, or this method waits for
        // the bound row's first fill of it. Never changes this door's answer.
        if door == "invokeExact" {
            ffm_const_note_compile(
                self.shared,
                holder,
                field.field_index,
                handle.as_ptr() as u64,
                epoch,
                &site,
                || {
                    let cm = self.shared.classes.class_manager.read();
                    cm.get_class(self.class_id).map(|c| LateBindWaiter {
                        class_id: self.class_id.as_u32(),
                        class_name: Arc::<str>::from(&*c.name),
                        method_name: Arc::from(self.method_name),
                        descriptor: Arc::from(self.method_descriptor),
                    })
                },
            );
        }
        let by_address = mh_constant_target_of(self.shared, handle.as_ptr() as u64, epoch, &site);
        // JIT round 17 wave 1 (lane mhconst; MH7-2, opt-in
        // `CRATONVM_JIT_MH_CONSTANT_FIELD_ROWS`): the field's own row answers
        // across collections. A row of the address memo for the handle the
        // field holds NOW, in this epoch, must name the same member: a field
        // `Unsafe` rewrote to another admitted handle is refused, not trusted.
        let field_entry =
            mh_constant_field_row_entry_of(self.shared, holder, field.field_index, &site);
        // JIT round 17 wave 3 (lane mh4;
        // `r17-mh-constant-field-row-trusts-a-rewritten-field-across-a-collection-FIXED-20261005.md`,
        // `CRATONVM_JIT_MH_CONSTANT_FIELD_ROW_WITNESS`, default ON): a row of an
        // older epoch answers only when its witness names, in this epoch, the
        // very address the field holds now. A witness naming another object
        // (or cleared) means the field was rewritten: the row goes. A row no
        // mutator witnessed is not used across a collection (the address memo
        // still answers, and the next fill witnesses it).
        let mut stale_row = false;
        let field_row = match field_entry {
            Some((row, weak)) if row.epoch != epoch => {
                match mh_constant_field_row_witness_holds(
                    self.shared,
                    weak,
                    epoch,
                    handle.as_ptr() as u64,
                ) {
                    Some(true) => Some(row),
                    Some(false) => {
                        mh_constant_field_row_dropped(
                            self.shared,
                            holder,
                            field.field_index,
                            &site,
                            &row,
                        );
                        stale_row = true;
                        None
                    }
                    None => {
                        stale_row = true;
                        None
                    }
                }
            }
            other => other.map(|(row, _)| row),
        };
        let disagrees = matches!(
            (&field_row, &by_address),
            (Some(row), Some(now)) if !mh_constant_same_member(row, now)
        )
            // JIT round 17 wave 2 (lane mh3;
            // `CRATONVM_JIT_MH_CONSTANT_RETIRE_ON_REWRITE`): a row written in
            // THIS epoch was written behind an address compare of this epoch,
            // and that handle's memo row lives as long as the row (no
            // collection evicts it, a violation drops both). No memo row for
            // the handle the field holds now means the field was rewritten
            // (`Unsafe.putObject`) to a handle nothing admitted: the row no
            // longer names what the field holds. Across a collection the row
            // answers only through its witness (round 17 wave 3, above).
            || (by_address.is_none()
                && field_row.as_ref().is_some_and(|row| row.epoch == epoch)
                && mh_constant_retire_on_rewrite_enabled());
        let from_field = field_row.is_some() && !disagrees;
        let target = if disagrees {
            None
        } else if field_row.is_some() {
            field_row
        } else {
            by_address
        };
        let admitted = target.as_ref().is_some_and(|t| {
            let cm = self.shared.classes.class_manager.read();
            jit_known_class_at(self.shared, &cm, &t.class, self.class_id, None)
                .filter(|id| id.as_u32() == t.class_id)
                .and_then(|id| {
                    let class = cm.get_class(id)?;
                    let method = class
                        .methods
                        .iter()
                        .find(|m| *m.name == *t.name && *m.descriptor == *t.desc)?;
                    let flags = method.access_flags;
                    // JIT round 19 wave 1 (lane mhiface; MHRESID18-4): an
                    // interface member is called through `invokeinterface`,
                    // so an abstract one is admitted (kind 2 only).
                    let refused_flags = if t.invoke_kind == 2
                        || (t.invoke_kind == 7
                            && t.convert.as_deref().is_some_and(|c| c.main_kind == 2))
                    {
                        MethodAccessFlags::SYNCHRONIZED | MethodAccessFlags::NATIVE
                    } else {
                        MethodAccessFlags::SYNCHRONIZED
                            | MethodAccessFlags::NATIVE
                            | MethodAccessFlags::ABSTRACT
                    };
                    let callable = !flags.intersects(refused_flags)
                        && (id == self.class_id
                        || flags.contains(MethodAccessFlags::PUBLIC)
                        // JIT round 17 wave 3 (lane mh4; MHCONST17-2):
                        // a virtual member is admitted by the same rule
                        // (a private one never reaches here: the
                        // admission refuses it).
                        || mh_constant_nonpublic_member_callable(&cm, self.class_id, class, flags))
                        && match t.invoke_kind {
                            3 => {
                                method.is_static()
                                    && *t.desc == *site
                                    && mh_constant_descriptor_agrees(
                                        self.shared,
                                        &cm,
                                        self.class_id,
                                        id,
                                        &t.desc,
                                    )
                            }
                            // An unbound virtual handle's member, called with
                            // the site's first argument as its receiver: the
                            // site is the member's descriptor after one
                            // reference, and every class the SITE names (the
                            // receiver's too) is one class from both loaders.
                            0 => {
                                mh_constant_virtual_enabled()
                                    && !method.is_static()
                                    && !flags.contains(MethodAccessFlags::PRIVATE)
                                    && !class.is_interface()
                                    && mh_constant_virtual_member_desc(&site).as_deref()
                                        == Some(&*t.desc)
                                    && mh_constant_descriptor_agrees(
                                        self.shared,
                                        &cm,
                                        self.class_id,
                                        id,
                                        &site,
                                    )
                            }
                            // JIT round 19 wave 1 (lane mhiface; MHRESID18-4,
                            // `CRATONVM_JIT_MH_CONSTANT_INTERFACE`): an unbound
                            // interface handle's member, by the virtual rule,
                            // declared by an interface. The builder guards the
                            // receiver with an `instanceof` of it.
                            2 => {
                                mh_constant_interface_enabled()
                                    && !method.is_static()
                                    && !flags.contains(MethodAccessFlags::PRIVATE)
                                    && class.is_interface()
                                    && mh_constant_virtual_member_desc(&site).as_deref()
                                        == Some(&*t.desc)
                                    && mh_constant_descriptor_agrees(
                                        self.shared,
                                        &cm,
                                        self.class_id,
                                        id,
                                        &site,
                                    )
                            }
                            // JIT round 21 wave 19 (lane mhbind2; MHBIND21-1,
                            // `CRATONVM_JIT_MH_CONSTANT_COMPILE_BOUND`): a REAL
                            // `bindTo` result of a virtual member, called on
                            // its bound value: the site IS the member's
                            // descriptor (the receiver is no site argument),
                            // the member an instance method of a class, and
                            // every class the site names one class from both
                            // loaders. The builder reads the receiver off the
                            // handle (`mh_constant_bound`).
                            4 => {
                                mh_constant_compile_bound_enabled()
                                    && t.bound.is_some()
                                    && !method.is_static()
                                    && !flags.contains(MethodAccessFlags::PRIVATE)
                                    && !class.is_interface()
                                    && *t.desc == *site
                                    && mh_constant_descriptor_agrees(
                                        self.shared,
                                        &cm,
                                        self.class_id,
                                        id,
                                        &site,
                                    )
                            }
                            // j27 wave 6 (lane T1LAM2,
                            // `CRATONVM_JIT_MH_CONSTANT_COMPILE_BOUND_STATIC`):
                            // a REAL `bindTo` result of a STATIC member, called
                            // with its bound value first: the member is static,
                            // its descriptor the site's with the leading
                            // reference in front (the bound handle's own type
                            // is the site, else `invokeExact` is the door's
                            // `WrongMethodTypeException`), and every class the
                            // member names one class from both loaders. The
                            // builder reads the value off the handle
                            // (`mh_constant_bound`).
                            6 => {
                                mh_constant_compile_bound_static_enabled()
                                    && t.bound.is_some()
                                    && method.is_static()
                                    && mh_constant_bound_static_site_matches(&t.desc, &site)
                                    && mh_constant_descriptor_agrees(
                                        self.shared,
                                        &cm,
                                        self.class_id,
                                        id,
                                        &t.desc,
                                    )
                            }
                            // JIT round 21 wave 20 (lane mhfilter; MHBIND2-1,
                            // `CRATONVM_JIT_MH_CONSTANT_COMPILE_FILTER`): a REAL
                            // filter species, two plain calls. The row's own
                            // member (the MAIN call) by the rule of its kind on
                            // its own type, the site the shape the JDK gives the
                            // species (`mh_filter_row_fits`), and the PRE member
                            // by the same rules
                            // ([`Self::mh_constant_filter_pre_admitted`]).
                            5 => {
                                mh_constant_compile_filter_enabled()
                                    && self.mh_constant_filter_admitted(&cm, t, id, &site)
                            }
                            7 => {
                                mh_constant_compile_converting_enabled()
                                    && t.convert.as_deref().is_some_and(|c| {
                                        let kind_ok = match c.main_kind {
                                            3 => method.is_static(),
                                            0 => {
                                                !method.is_static()
                                                    && !flags.contains(MethodAccessFlags::PRIVATE)
                                                    && !class.is_interface()
                                            }
                                            2 => {
                                                !method.is_static()
                                                    && !flags.contains(MethodAccessFlags::PRIVATE)
                                                    && class.is_interface()
                                            }
                                            _ => false,
                                        };
                                        kind_ok
                                            && mh_constant_descriptor_agrees(
                                                self.shared,
                                                &cm,
                                                self.class_id,
                                                id,
                                                &t.desc,
                                            )
                                    })
                            }
                            _ => false,
                        };
                    callable.then_some(())
                })
                .is_some()
        });
        // JIT round 17 wave 2 (lane mh3;
        // `CRATONVM_JIT_MH_CONSTANT_RETIRE_ON_REWRITE`): the body this site
        // compiles into depends on the handle staying what it was admitted
        // as. The dependency is recorded first and the row it came from read
        // again after: a rewrite found in between dropped the row (the site is
        // refused) or finds the dependency (the body is withdrawn). A site
        // whose dependency cannot be recorded is refused.
        let admitted = admitted
            && (!mh_constant_retire_on_rewrite_enabled()
                || target.as_ref().is_some_and(|t| {
                    let class_name = {
                        let cm = self.shared.classes.class_manager.read();
                        cm.get_class(self.class_id)
                            .map(|c| Arc::<str>::from(&*c.name))
                    };
                    let recorded = class_name.is_some_and(|name| {
                        note_mh_constant_compiled(
                            self.shared,
                            (t.class_id, &*t.name, &*t.desc),
                            self.class_id.as_u32(),
                            &name,
                        )
                    });
                    let still = if from_field {
                        mh_constant_field_row_of(self.shared, holder, field.field_index, &site)
                    } else {
                        mh_constant_target_of(self.shared, handle.as_ptr() as u64, epoch, &site)
                    };
                    recorded && still.as_ref() == Some(t)
                }));
        // MH7-2: an address-memo admission becomes the field's row (the field
        // was just read holding that very handle, in its epoch).
        if let Some(t) = target.as_ref().filter(|_| admitted && !from_field) {
            note_mh_constant_field_row(self.shared, holder, field.field_index, &site, t);
        }
        // MH7-1 (opt-in `CRATONVM_JIT_MH_CONSTANT_RECOMPILE`): no admission
        // yet; the constant bind's next admission of this field's handle asks
        // a recompile of this method (`mh_constant_waiters_served`).
        let waiting = target.is_none() && !disagrees && mh_constant_recompile_enabled(self.shared);
        if waiting {
            let class_name = {
                let cm = self.shared.classes.class_manager.read();
                cm.get_class(self.class_id)
                    .map(|c| Arc::<str>::from(&*c.name))
            };
            if let Some(class_name) = class_name {
                note_mh_constant_waiter(
                    self.shared,
                    holder,
                    field.field_index,
                    &site,
                    LateBindWaiter {
                        class_id: self.class_id.as_u32(),
                        class_name,
                        method_name: Arc::from(self.method_name),
                        descriptor: Arc::from(self.method_descriptor),
                    },
                );
            }
        }
        if crate::jit::helpers::mh_constant_bind_dbg() {
            eprintln!(
                "[mh-bind] compile {}{} bci={bci} MethodHandle.{door}{site} field cp#{field_cp} -> {}",
                self.method_name,
                self.method_descriptor,
                match (&target, admitted) {
                    (Some(t), true) if from_field => {
                        format!(
                            "{}.{}{} {} (field row)",
                            t.class,
                            t.name,
                            t.desc,
                            mh_constant_kind_word(t.invoke_kind)
                        )
                    }
                    (Some(t), true) => format!(
                        "{}.{}{} {}",
                        t.class,
                        t.name,
                        t.desc,
                        mh_constant_kind_word(t.invoke_kind)
                    ),
                    (Some(t), false) => format!("refused {}.{}{}", t.class, t.name, t.desc),
                    (None, _) if disagrees => "field row disagrees with the handle".to_string(),
                    (None, _) if waiting && stale_row => {
                        "field row not witnessed in this epoch (waiting)".to_string()
                    }
                    (None, _) if waiting => {
                        "no admitted handle in this epoch (waiting)".to_string()
                    }
                    (None, _) if stale_row => {
                        "field row not witnessed in this epoch".to_string()
                    }
                    (None, _) => "no admitted handle in this epoch".to_string(),
                },
            );
        }
        let t = target.filter(|_| admitted)?;
        Some((
            t.class.into(),
            t.name.into(),
            t.desc.into(),
            t.invoke_kind,
            t.class_id,
        ))
    }

    /// [`Self::indy`] for the instruction at `bci` (`None`: unnamed, see
    /// [`interned_jit_indy_bridge_site`]).
    pub(in crate::runtime::interpreter::jit_bridge) fn indy_for(
        &self,
        cp_idx: u16,
        bci: Option<usize>,
    ) -> Option<(String, usize)> {
        let cm = self.shared.classes.class_manager.read();
        let class = cm.get_class(self.class_id)?;
        match class.constant_pool.get(cp_idx)? {
            ConstantPoolEntry::InvokeDynamic {
                name_and_type_index,
                ..
            } => class
                .constant_pool
                .get_name_and_type(*name_and_type_index)
                .map(|(_name, descriptor)| {
                    let instruction =
                        bci.map(|bci| (self.method_name, self.method_descriptor, bci));
                    (
                        descriptor.to_string(),
                        interned_jit_indy_bridge_site(
                            self.shared,
                            &cm,
                            class,
                            self.class_id,
                            cp_idx,
                            instruction,
                        ),
                    )
                }),
            _ => None,
        }
    }

    /// `new`/`anewarray`: `Resolved` when the target class is already loaded,
    /// `Deferred` (holder class id + CP index) when it is not.
    ///
    /// Also `Deferred` when this method `new`s the class and its
    /// initialization is still pending (wave 4, L7): a resolved row compiles
    /// to an allocation with no class-init check. See
    /// [`jit_new_target_init_pending`]. The bytecode scan runs only for such a
    /// class, i.e. almost never, and so does the link attempt of
    /// [`jit_new_target_pending_after_link`] (after the read lock is released).
    pub(in crate::runtime::interpreter::jit_bridge) fn new_site(
        &self,
        cp_idx: u16,
    ) -> Option<cratonvm_jit::JitNewSite> {
        let site = {
            let cm = self.shared.classes.class_manager.read();
            match resolve_jit_new_site(self.shared, &cm, self.class_id, cp_idx) {
                Some(cratonvm_jit::JitNewSite::Resolved { class_id, .. })
                    if jit_new_target_init_pending(self.shared, &cm, ClassId::new(class_id))
                        && code_has_new_of(self.code, cp_idx) =>
                {
                    Err(ClassId::new(class_id))
                }
                other => Ok(other),
            }
        };
        match site {
            Ok(other) => other,
            Err(target_id) if !jit_new_target_pending_after_link(self.shared, target_id) => {
                let cm = self.shared.classes.class_manager.read();
                resolve_jit_new_site(self.shared, &cm, self.class_id, cp_idx)
            }
            Err(target_id) => {
                // JIT round 19 wave 2, lane eager2 (EAGER19-1): the census of
                // the sites the uninitialized-`new` trap is for: a LOADED
                // class whose initialization is still owed. Each costs the
                // optimizing tier this method (single-pass fall-through and a
                // blind `<init>` dispatch) unless `CRATONVM_JIT_UNINIT_NEW_TRAP`
                // plants a trap there.
                if crate::runtime::env_cache::dbg_jitc() {
                    let cm = self.shared.classes.class_manager.read();
                    eprintln!(
                        "[cratonvm-jitc] uninit-new CENSUS {}.{}{} cp={cp_idx} class={}: loaded, \
                         initialization owed ({})",
                        cm.get_class(self.class_id)
                            .map(|c| c.name.to_string())
                            .unwrap_or_default(),
                        self.method_name,
                        self.method_descriptor,
                        cm.get_class(target_id)
                            .map(|c| c.name.to_string())
                            .unwrap_or_default(),
                        if jit_new_target_erroneous(&cm, target_id) {
                            "erroneous: no trap"
                        } else {
                            "trap candidate"
                        },
                    );
                }
                Some(cratonvm_jit::JitNewSite::Deferred {
                    holder_class_id: self.class_id.as_u32(),
                    cp_idx,
                })
            }
        }
    }

    /// Elidable-`<init>` for `new` scalar replacement.
    pub(in crate::runtime::interpreter::jit_bridge) fn elidable_init(&self, cp_idx: u16) -> bool {
        resolve_jit_elidable_init_loading(self.shared, self.class_id, cp_idx)
    }

    /// The class id of an invoke* site's declared (`Methodref`) class, for the
    /// CRC32/CRC32C `update` call-site intrinsics' receiver class-id guard.
    pub(in crate::runtime::interpreter::jit_bridge) fn invoke_class_id(
        &self,
        cp_idx: u16,
    ) -> Option<u32> {
        let cm = self.shared.classes.class_manager.read();
        let class = cm.get_class(self.class_id)?;
        let class_idx = match class.constant_pool.get(cp_idx) {
            Some(ConstantPoolEntry::MethodReference { class_index, .. }) => *class_index,
            Some(ConstantPoolEntry::InterfaceMethodReference { class_index, .. }) => *class_index,
            _ => return None,
        };
        let target_class = class.constant_pool.get_class_name(class_idx)?;
        Some(
            jit_known_class_at(
                self.shared,
                &cm,
                target_class,
                self.class_id,
                Some(class_idx),
            )?
            .as_u32(),
        )
    }

    /// Review #80: the class that DECLARES an invoke's resolved method, for the
    /// `Atomic*` intrinsics' subclass sites.
    pub(in crate::runtime::interpreter::jit_bridge) fn invoke_declaring_class(
        &self,
        cp_idx: u16,
    ) -> Option<String> {
        cp_method_ref_declaring_class_name(self.shared, self.class_id, cp_idx)
    }

    /// The id of the class [`Self::invoke_declaring_class`] names, for the
    /// recursion-cycle checks of an inherited-name call site (wave 17, L3).
    pub(in crate::runtime::interpreter::jit_bridge) fn invoke_declaring_class_id(
        &self,
        cp_idx: u16,
    ) -> Option<u32> {
        cp_method_ref_declaring_class_id(self.shared, self.class_id, cp_idx)
    }

    /// inc 35: an `ldc2_w` constant as `(bits, is_double)`, so the IR builder
    /// lowers a `double` to `dconst` and a `long` to `lconst`.
    pub(in crate::runtime::interpreter::jit_bridge) fn ldc2w(
        &self,
        cp_idx: u16,
    ) -> Option<(i64, bool)> {
        let cm = self.shared.classes.class_manager.read();
        let class = cm.get_class(self.class_id)?;
        let val = match class.constant_pool.get(cp_idx)? {
            ConstantPoolEntry::Long(v) => Some((v.get(), false)),
            ConstantPoolEntry::Double(v) => Some((v.to_bits() as i64, true)), // Cast: JIT ABI -- float bits to i64
            _ => None,
        };
        if let Some(door) = self.ldc2w_trace {
            if crate::runtime::env_cache::dbg_jit_ldc() {
                eprintln!(
                    "[cratonvm-ldc2w] {door} idx={} -> {:?} (f64 {})",
                    cp_idx,
                    val,
                    // Cast: integer word reinterpreted as float/double bit pattern
                    val.map(|(v, _)| f64::from_bits(v as u64))
                        .unwrap_or(f64::NAN)
                );
            }
        }
        val
    }

    /// RBC.2 — `ldc`/`ldc_w` int/float/String/Class constants. Without it every
    /// method containing an `ldc` failed codegen at the 0x12/0x13 arm and
    /// stayed interpreted forever (the BC-suite 34-64x interpreter gap). A
    /// `MethodHandle`/`MethodType`/condy `ldc` still answers `None`, a
    /// permanent compile bail.
    /// Round 14 wave 3 (lane calls, I7-4): `String.hashCode()` of the literal
    /// `ldc #cp_idx` pushes, for the optimizing tier's hash fold. Exactly the
    /// literals `jit_ldc_constant_for` offers as a `String` site (a lossless
    /// UTF-8 form); `None` for anything else.
    pub(in crate::runtime::interpreter::jit_bridge) fn ldc_string_hash(
        &self,
        cp_idx: u16,
    ) -> Option<i32> {
        let cm = self.shared.classes.class_manager.read();
        let class = cm.get_class(self.class_id)?;
        match class.constant_pool.get(cp_idx)? {
            ConstantPoolEntry::StringReference { string_index }
                if class.constant_pool.get_utf8_wide(*string_index).is_none() =>
            {
                class
                    .constant_pool
                    .get_utf8(*string_index)
                    .map(crate::vm::java_string_hash)
            }
            _ => None,
        }
    }

    /// Round 14 wave 4 (lane calls2, C14W3-4): the UTF-16 units of the literal
    /// `ldc #cp_idx` pushes, for the optimizing tier's literal query fold.
    /// Exactly the literals [`Self::ldc_string_hash`] answers.
    pub(in crate::runtime::interpreter::jit_bridge) fn ldc_string_utf16(
        &self,
        cp_idx: u16,
    ) -> Option<Vec<u16>> {
        let cm = self.shared.classes.class_manager.read();
        let class = cm.get_class(self.class_id)?;
        match class.constant_pool.get(cp_idx)? {
            ConstantPoolEntry::StringReference { string_index }
                if class.constant_pool.get_utf8_wide(*string_index).is_none() =>
            {
                class
                    .constant_pool
                    .get_utf8(*string_index)
                    .map(|s| s.encode_utf16().collect())
            }
            _ => None,
        }
    }

    pub(in crate::runtime::interpreter::jit_bridge) fn ldc(
        &self,
        cp_idx: u16,
    ) -> Option<cratonvm_jit::JitLdcConstant> {
        // The class-manager guard ends before the slot lookup (`with_ldc_slot`).
        let c = {
            let cm = self.shared.classes.class_manager.read();
            jit_ldc_constant_for(&cm, self.class_id, cp_idx)
        };
        with_ldc_slot(self.shared, c)
    }

    /// A [`crate::jit::CompileRequest`] whose every `cp_*` resolver is this
    /// pool's, and every other input `CompileRequest::new`'s default (interpreter
    /// round i1 wave 4, L7). The doors used to spell the same thirteen one-line
    /// closures out each; a door now fills only the fields that are its own,
    /// with struct-update syntax over the request it is handed (the type is
    /// `Copy`): `CompileRequest { callee_compiler: …, ..cp_request }`.
    ///
    /// Handed to `f` rather than returned because the closures the request
    /// borrows live in this frame. `cp_elidable_init_resolver` is always set; a
    /// door that gates scalar-`new` replacement off passes `None` over it.
    ///
    /// The method comes as [`CompileInputs`] (interpreter round i1 wave 43,
    /// lane L2): its bytecode was read inside the compile's epoch witness, so
    /// no door can build a request from bytecode it read before.
    pub(in crate::runtime::interpreter::jit_bridge) fn with_compile_request<R>(
        &self,
        inputs: &CompileInputs,
        helpers: &cratonvm_jit::JitRuntimeHelpers,
        f: impl FnOnce(crate::jit::CompileRequest<'_>) -> R,
    ) -> R {
        ensure_ir_collection_layouts(self.shared);
        let cached: &CachedBytecodeMethod = inputs.cached();
        let class_name = |cp_idx: u16| self.class_name(cp_idx);
        // Round 15 wave 4 (lane fieldowner, CH3-1): one resolution per field
        // site behind the four field asks (`JitFieldSiteMemo`).
        let field_memo_rows = JitFieldSiteMemo::default();
        let field_memo = true.then_some(&field_memo_rows);
        let instance_field = |cp_idx: u16| self.instance_field(field_memo, cp_idx);
        let static_field = |cp_idx: u16| self.static_field(field_memo, cp_idx);
        // Interpreter round i4 wave 92, lane L2: a field site the two
        // resolvers above decline compiles as a deferred site
        // (`CompileRequest::cp_field_deferred_resolver`), behind its switch.
        let deferred_field_tag = |cp_idx: u16| self.deferred_field_tag(cp_idx);
        let deferred_fields_on = jit_deferred_field_sites_enabled();
        // JIT round 17 wave 2 (lane irfeed): the trusted `static final` bit.
        let static_field_final =
            |cp_idx: u16| self.static_field_is_trusted_final(field_memo, cp_idx);
        let field_is_volatile = |cp_idx: u16| self.field_is_volatile(field_memo, cp_idx);
        // nio21 wave 2 (lane L3): the final bit for the IR memo.
        let field_is_final = |cp_idx: u16| self.instance_field_is_final(field_memo, cp_idx);
        // JIT round 20 (lane memory): the `ByteBuffer.hb` fact for the
        // method's own reads.
        let field_is_byte_array =
            |cp_idx: u16| self.instance_field_is_byte_array(field_memo, cp_idx);
        let instance_field_decl =
            |cp_idx: u16| self.instance_field_declaring_class(field_memo, cp_idx);
        let invoke = |cp_idx: u16| self.invoke(cp_idx);
        let first_call = |cp_idx: u16| self.first_call(cp_idx);
        let invokespecial_owner =
            |cp_idx: u16, opcode: u8| self.invokespecial_owner(cp_idx, opcode);
        let indy = |cp_idx: u16| self.indy(cp_idx);
        // JIT round 16 wave 10 (lane indyir): the same, per instruction, and
        // the constant-site plain call (IN16-8). Both answer exactly as before
        // with their switches off.
        let indy_at = |cp_idx: u16, bci: usize| self.indy_at(cp_idx, bci);
        let indy_direct = |cp_idx: u16, bci: usize| self.indy_direct(cp_idx, bci);
        // JIT round 20 wave 11 (lane pIndy, SPLICE19-1): a mutable call site's
        // bound target, as a guarded call.
        let indy_mutable_guard = |cp_idx: u16, bci: usize| self.indy_mutable_guard(cp_idx, bci);
        // JIT round 21 wave 17 (lane records, IN16-6): a record's generated
        // `hashCode` / `equals` site, as a field-wise fold.
        let indy_record_fold = |cp_idx: u16, bci: usize| self.indy_record_fold(cp_idx, bci);
        // JIT round 20 wave 11 (lane pIndy, W10L3-3 / W11L3-3): a bridge
        // site's published lambda singleton slot. Reads only the site.
        let indy_constant_word =
            |site: usize| crate::runtime::invokedynamic::jit_indy_constant_word_slot(site);
        // JIT round 20 wave 12 (lane qLamNew, W10L3-2 / W11L3-2): a linked
        // capturing site's proxy class and capture count. Tables only.
        let indy_capture_alloc = |site: usize| {
            crate::runtime::invokedynamic::jit_indy_capture_alloc_class(self.shared, site)
        };
        // t1 wave 11, lane JLAM (LAMBDATRAP21-4): a non-capturing lambda
        // bridge site, published or not. Reads only the site.
        let indy_constant_lambda_site =
            |site: usize| crate::runtime::invokedynamic::jit_indy_site_is_constant_lambda(site);
        // JIT round 21 (lane lambda2, TLOCAL21-1 / LAMBDATRAP21-3): a linked
        // lambda site's static impl, for the SAM call on its value. Tables
        // and the impl's bytecode template only.
        // JIT round 21 wave 20 (lane sbfuse, STRINTR21-4): a fused
        // `StringBuilder` chain's concat site, interned per VM. Pure data.
        let sb_concat_site =
            |recipe: &[u16], constants: &[Vec<u16>], descriptor: &str, deopt_on_overflow: bool| {
                interned_jit_sb_chain_concat_site(
                    self.shared,
                    recipe,
                    constants,
                    descriptor,
                    deopt_on_overflow,
                )
            };
        let indy_lambda_impl = |site: usize| {
            let proxy =
                crate::runtime::invokedynamic::jit_indy_lambda_proxy_class(self.shared, site)?;
            super::lambda::lambda_proxy_static_impl_call(self.shared, ClassId::new(proxy))
        };
        // JIT round 16 wave 17 (lane mh7): a constant handle's member.
        let mh_constant =
            |invoke_cp: u16, bci: usize, field_cp: u16| self.mh_constant(invoke_cp, bci, field_cp);
        // JIT round 21 wave 19 (lane mhbind2; MHBIND21-1).
        let mh_constant_bound = |invoke_cp: u16, bci: usize, field_cp: u16| {
            self.mh_constant_bound(invoke_cp, bci, field_cp)
        };
        // JIT round 21 wave 20 (lane mhfilter; MHBIND2-1).
        let mh_constant_filter =
            |invoke_cp: u16, bci: usize, field_cp: u16, getstatic_bci: usize| {
                self.mh_constant_filter(invoke_cp, bci, field_cp, getstatic_bci)
            };
        let mh_constant_convert =
            |invoke_cp: u16, bci: usize, field_cp: u16, getstatic_bci: usize| {
                self.mh_constant_convert(invoke_cp, bci, field_cp, getstatic_bci)
            };
        let new_site = |cp_idx: u16| self.new_site(cp_idx);
        // JIT round 20 wave 12 (lane qMisc, NIO20-L3-1): does `getClass()`
        // report this loaded class exactly for its own instances? The name
        // under a short class-manager read (released before the predicate);
        // the predicate reads only native-builtins' static remap tables.
        let getclass_exact_fold = |class_id: u32| -> bool {
            let name = {
                let cm = self.shared.classes.class_manager.read();
                cm.get_class(ClassId::new(class_id))
                    .map(|c| c.name.to_string())
            };
            name.is_some_and(|n| cratonvm_native_builtins::getclass_reports_only_itself(&n))
        };
        // JIT round 21 wave 4, lane ic (IC21-2): a fresh `invokevirtual`
        // site's compile-time shared-table bind; asked by the planners only
        // under `CRATONVM_JIT_MEGA_VTABLE_COLUMN_BAKED`. Short class-manager
        // and vtable reads, one after the other (no guard is held here).
        let mega_site_bind = |info: &cratonvm_jit::JitInvokeInfo,
                              pic: &cratonvm_jit::JitPICSlot| {
            crate::jit::helpers::bake_mega_virtual_site(self.shared, info, pic)
        };
        let elidable_init = |cp_idx: u16| -> bool { self.elidable_init(cp_idx) };
        let invoke_class_id = |cp_idx: u16| self.invoke_class_id(cp_idx);
        let invoke_declaring_class = |cp_idx: u16| self.invoke_declaring_class(cp_idx);
        let invoke_declaring_class_id = |cp_idx: u16| self.invoke_declaring_class_id(cp_idx);
        let ldc2w = |cp_idx: u16| self.ldc2w(cp_idx);
        let ldc = |cp_idx: u16| self.ldc(cp_idx);
        let ldc_string_hash = |cp_idx: u16| self.ldc_string_hash(cp_idx);
        // Pure data filed on a mutator thread when the declaring class finished
        // initialising; see `ClassRealm::static_varhandle_fields`. No heap read.
        let static_final_prim = |class_id: u32, field_index: u32, type_tag: u8| {
            resolve_static_final_prim_for_jit(self.shared, class_id, field_index, type_tag)
        };
        let static_varhandle = |class_id: u32, field_index: u32| {
            self.shared
                .classes
                .static_varhandle_fields
                .read()
                .get(&(class_id, field_index))
                .copied()
        };
        let ldc_string_utf16 = |cp_idx: u16| self.ldc_string_utf16(cp_idx);
        // JIT r20 wave 11 (lane pIR): pure data filed on a mutator thread when
        // the declaring class finished initialising; see
        // `ClassRealm::static_final_array_lengths`. No heap read.
        let static_final_array_length = |class_id: u32, field_index: u32| {
            self.shared
                .classes
                .static_final_array_lengths
                .read()
                .get(&(class_id, field_index))
                .copied()
        };
        // `ClassRealm::static_final_ref_classes`, the same kind of pure data.
        let static_final_ref_class = |class_id: u32, field_index: u32| {
            self.shared
                .classes
                .static_final_ref_classes
                .read()
                .get(&(class_id, field_index))
                .copied()
        };
        // Round 13 wave 10 (lane sync6): a `static synchronized` method's
        // monitor slot (no class-manager guard is held here, the lookup's
        // contract). The background worker only finds a slot a mutator minted.
        let self_lock_mirror_slot = if cached.is_synchronized
            && cached.is_static
            && crate::runtime::env_cache::jit_self_locking_sync_static()
        {
            crate::jit::helpers::class_mirror_slot_for(self.shared, cached.declaring_class_id)
        } else {
            0
        };
        let live_branch_counters = || {
            self.shared.jit.profile_store.branch_counters_borrowed(
                cached.declaring_class_id.as_u32(),
                &cached.method_name,
                &cached.method_descriptor,
                cached_method_code_len(cached),
            )
        };
        // `CRATONVM_FROZEN_PAGES`: the snapshot as it stands; `None` with the
        // switch off.
        // With the switch on and no snapshot yet, an empty one: the build
        // then records which roots it is waiting for.
        let frozen_facts = crate::runtime::frozen_pages::enabled().then(|| {
            self.shared
                .classes
                .frozen
                .facts
                .read()
                .clone()
                .unwrap_or_default()
        });
        // Stage 2: the receiver each class's methods are nearly always called on.
        let frozen_receivers = crate::runtime::frozen_pages::dominant_receivers(self.shared);
        let const_folds_snapshot = self
            .shared
            .jit
            .bridge
            .const_folds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let class_is_final_resolver = |cid: u32| jit_class_is_final_for_typecheck(self.shared, cid);
        let class_is_cha_leaf_resolver =
            |cid: u32| jit_class_is_cha_leaf_for_typecheck(self.shared, cid);
        let compiled = f(crate::jit::CompileRequest {
            class_is_final_resolver: Some(&class_is_final_resolver),
            class_is_cha_leaf_resolver: Some(&class_is_cha_leaf_resolver),
            branch_counter_resolver: Some(&live_branch_counters),
            frozen_facts: frozen_facts.as_ref(),
            frozen_state: Some(&*self.shared.classes.frozen.facts_state),
            frozen_receivers: frozen_receivers.as_deref(),
            cp_class_name_resolver: Some(&class_name),
            cp_field_resolver: Some(&instance_field),
            cp_static_field_resolver: Some(&static_field),
            cp_field_deferred_resolver: deferred_fields_on
                .then_some(&deferred_field_tag as &dyn Fn(u16) -> Option<u8>),
            cp_static_field_final_resolver: Some(&static_field_final),
            static_final_array_length_resolver: Some(&static_final_array_length),
            static_final_ref_class_resolver: Some(&static_final_ref_class),
            cp_field_volatile_resolver: Some(&field_is_volatile),
            cp_field_final_resolver: Some(&field_is_final),
            cp_field_byte_array_resolver: Some(&field_is_byte_array),
            cp_field_declaring_class_resolver: Some(&instance_field_decl),
            cp_invoke_resolver: Some(&invoke),
            // The sites `invoke` only peeked at, guarded by both tiers so they
            // resolve at their first execution (interpreter round i1 wave 50,
            // lane L1).
            cp_invoke_first_call: Some(&first_call),
            static_varhandle_resolver: Some(&static_varhandle),
            static_final_prim_resolver: Some(&static_final_prim),
            cp_invokespecial_owner_resolver: Some(&invokespecial_owner),
            cp_new_resolver: Some(&new_site),
            cp_ldc_resolver: Some(&ldc),
            cp_ldc_string_hash_resolver: Some(&ldc_string_hash),
            cp_ldc_string_utf16_resolver: Some(&ldc_string_utf16),
            getclass_exact_fold_resolver: Some(&getclass_exact_fold),
            mega_site_binder: Some(&mega_site_bind),
            cp_ldc2w_resolver: Some(&ldc2w),
            cp_invoke_class_id_resolver: Some(&invoke_class_id),
            cp_elidable_init_resolver: Some(&elidable_init),
            // invokedynamic-uncommon-trap fix: an invokedynamic CP index's
            // target descriptor, so the codegen can lower the instruction to an
            // uncommon-trap deopt instead of bailing the whole method.
            cp_invokedynamic_descriptor_resolver: Some(&indy),
            cp_invokedynamic_site_resolver: Some(&indy_at),
            cp_indy_direct_resolver: Some(&indy_direct),
            cp_indy_mutable_guard_resolver: Some(&indy_mutable_guard),
            cp_indy_record_fold_resolver: Some(&indy_record_fold),
            indy_constant_word_resolver: Some(&indy_constant_word),
            indy_capture_alloc_resolver: Some(&indy_capture_alloc),
            indy_constant_lambda_site_resolver: Some(&indy_constant_lambda_site),
            indy_lambda_impl_resolver: Some(&indy_lambda_impl),
            sb_concat_site_resolver: Some(&sb_concat_site),
            cp_mh_constant_resolver: Some(&mh_constant),
            cp_mh_constant_bound_resolver: Some(&mh_constant_bound),
            cp_mh_constant_filter_resolver: Some(&mh_constant_filter),
            cp_mh_constant_convert_resolver: Some(&mh_constant_convert),
            cp_invoke_declaring_class_resolver: Some(&invoke_declaring_class),
            cp_invoke_declaring_class_id_resolver: Some(&invoke_declaring_class_id),
            // A debugger may read this VM's locals: no mode exit may resume a
            // frame that shows a dead but assigned local as `0` (interpreter
            // round i1 wave 19, lane L1). Every door built here gets it.
            debugger_observes_locals: crate::runtime::jvmti::debugger_observes_locals(self.shared),
            // t1 wave 2, lane M: no elided lock of a non-escaping object while
            // a monitor observer may exist (JDWP, or a C agent's owned-monitor
            // / locals capability), as HotSpot's escape barrier lists it.
            monitor_observer_at_compile: crate::runtime::jvmti::monitor_observer_may_exist(
                self.shared,
            ),
            // t1 wave 3, lane M: and no scalar replacement while an object
            // observer may exist (adds a C agent's `can_tag_objects`), so local
            // reads and heap walks see every allocation.
            scalar_observer_at_compile: crate::runtime::jvmti::object_observer_may_exist(
                self.shared,
            ),
            // JIT round 21 wave 24 (lane flipprep, flip review item 2): a C
            // JVMTI agent of this VM may hold `can_access_local_variables`;
            // the optimizing tier then keeps every fresh-allocation volatile
            // store fenced. Asked only under the opt-in elision switch, so
            // the default request is unchanged and takes no JVMTI lock.
            // Without `experimental-debug` there is no C JVMTI plane.
            jvmti_locals_accessible: crate::jit::fresh_volatile_store_unfenced_enabled() && {
                #[cfg(feature = "experimental-debug")]
                {
                    crate::jvmti::native_env::locals_capability_may_be_held(self.shared)
                }
                #[cfg(not(feature = "experimental-debug"))]
                {
                    false
                }
            },
            // An exception client armed now: no compiled local throw edge
            // (round nio21 wave 2, lane L2). One armed after the read is the
            // JVMTI plane's (`local_throw_edges_need_the_interpreter`).
            // Interpreter round i4 wave 92 (lane L1): nor in a VM that serves
            // JVMTI `Exception` from compiled code
            // (`CRATONVM_JVMTI_EXCEPTION_FROM_COMPILED`), whose listener armed
            // later does not hold the VM off compiled code.
            // Wave 93 (lane L1): nor in a VM that serves JDWP `Exception`
            // from compiled code under the same switch.
            exception_event_flag_addrs: Some(crate::runtime::jvmti::exception_event_union_addrs()),
            exception_events_armed_at_compile:
                crate::runtime::interpreter::compiled_catch_events_armed(self.shared)
                    || crate::runtime::jvmti::exceptions_post_from_compiled_code(
                        self.shared.vm_identity,
                    )
                    || crate::runtime::interpreter::jdwp_exceptions_post_from_compiled_code(
                        self.shared,
                    ),
            // JIT round 23 wave 4 (lane L1): the "armed now" half alone, for
            // the spliced-callee-handler gate (`exception_events_armed_at_compile`
            // above is `true` for every compile of a VM that serves the events
            // from compiled code, i.e. every production compile).
            exception_listener_armed_at_compile:
                crate::runtime::interpreter::compiled_catch_events_armed(self.shared),
            self_lock_mirror_slot,
            caller_mirror_slot: {
                let ignored = is_security_walk_ignored_for_jit(
                    self.shared,
                    cached.declaring_class_id,
                    &cached.class_name,
                    &cached.method_name,
                    false,
                );
                if ignored {
                    0
                } else {
                    crate::jit::helpers::class_mirror_slot_for(
                        self.shared,
                        cached.declaring_class_id,
                    )
                }
            },
            object_class_id: self
                .shared
                .classes
                .class_manager
                .try_read()
                .and_then(|cm| cm.find_bootstrap_class_by_name("java/lang/Object"))
                .map(|id| id.as_u32()),
            const_folds: Some(&const_folds_snapshot),
            const_folds_refresher: Some(&|| {
                self.shared
                    .jit
                    .bridge
                    .const_folds
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
            }),
            caller_security_walk_ignored: is_security_walk_ignored_for_jit(
                self.shared,
                cached.declaring_class_id,
                &cached.class_name,
                &cached.method_name,
                false,
            ),
            // This VM's compile verdicts (interpreter round i1 wave 11): the
            // bail list `admit` asks and the compile records into, and the
            // recursion-cycle sets the bind ladders ask; and its deferred-`new`
            // memo (wave 10). Required inputs of the constructor (waves 17 and
            // 18, `jit_compile_realm`), so no request can miss them.
            ..crate::jit::CompileRequest::new(cached, helpers, jit_compile_realm(self.shared))
        });
        // gci i18/dk (`gci-i14mp-memory-pool-and-gc-mxbean-review-findings-FIXED-20261016`
        // item 7, the code-heap half i16/nt left NARROWED): HotSpot checks a
        // code heap's usage threshold at code-cache allocation
        // (`CodeCache::allocate` -> `MemoryService::track_code_cache_memory_usage`).
        // Every compile request is built here
        // (`every_compile_request_is_built_from_compile_inputs`), and the
        // compile `f` ran has committed its code by now, so this is the
        // VM's one per-VM point after a code allocation. Reporting only: one
        // `match` and one load unless a pool threshold is armed; armed, the
        // code-heap sample is two relaxed loads and the threshold table's own
        // lock, a leaf (`PoolThresholds` takes nothing under it) -- no
        // class-manager or JIT-cache lock, no Java, whatever the caller holds.
        crate::vm::vm_exec::jmx_check_non_heap_thresholds(
            self.shared,
            crate::vm::vm_exec::NonHeapThresholdCheck::CodeAllocated,
        );
        compiled
    }
}

/// Build an OPTIMIZING-TIER artifact for `cached`, and nothing else.
///
/// The input assembly — every constant-pool resolver, the invoke plans, the
/// `new`-site resolutions, the inline sites, the PGO profile, the helper table
/// — with **no admission policy and no side effects on the method's tiering
/// state**. That separation is the whole point of this function existing.
///
/// It was extracted from [`try_jit_upgrade_with_gate`], which is the
/// METHOD-ENTRY door: it asks a policy question first, and on a refusal it
/// calls `mark_jit_bail_listed`. That combination makes it unusable as a
/// question. Another door that called it to ask "could the optimizing tier
/// take this?" got two wrong answers at once — a refusal for a reason that
/// belongs to the method-entry door (any native-shadowed callee, i.e. nearly
/// every method containing a `println`), and a bail-list entry that
/// `compile_gate::admit` then honours at EVERY door, switching off compiles
/// that were working. The OSR door tried exactly that on 2026-09-04.
///
/// So: policy stays with the door that owns it, assembly lives here, and a
/// caller takes the second without triggering the first. `try_jit_upgrade_with_gate`
/// keeps its gates and calls this; the OSR door calls this and applies its own.
///
/// `None` when the backend declines the method — a compile refusal, which is
/// never a statement about whether the method may be compiled again.
///
/// The artifact comes back UNPUBLISHED together with the owners of every
/// callee entry its code CALLs directly ([`PinnedArtifact`]); the caller keeps
/// them until the artifact is published (or, for a body that is never
/// published, for as long as it may run).
///
/// `osr_door` is `true` only for the optimizing OSR door
/// (`build_osr_optimizing_artifact`): its body runs inside the OSR'd frame's
/// activation, so its unconditional back-edge polls may leave for the
/// interpreter when an agent needs it (`CompileRequest::osr_mode_exit_polls`,
/// interpreter round i1 wave 15, lane L2). The method-entry door passes
/// `false`, which compiles exactly what it did.
pub(in crate::runtime::interpreter) fn compile_optimizing_artifact(
    shared: &SharedVm,
    inputs: &CompileInputs,
    osr_door: bool,
) -> Option<PinnedArtifact> {
    // JIT round 18 wave 9 (lane eagercold): a top-level optimizing compile
    // starts its own eager-chain budget, as `try_jit_compile_callee_slow`'s
    // does; see [`optimizing_door_chain_budget_enabled`].
    if optimizing_door_chain_budget_enabled() {
        eager_callee_chain_enter_top_level();
    }
    // Read inside the compile's epoch witness by the door (interpreter round
    // i1 wave 43, lane L2; see `CompileInputs`).
    let cached: &Arc<CachedBytecodeMethod> = inputs.cached();
    let class_id = cached.declaring_class_id;
    ensure_ir_collection_layouts(shared);
    // JIT round 21 wave 11 (lane finalize): a finalizable class's constructor
    // that can throw before its `super(..)` stays interpreted
    // (`exception_dispatch::finalizer_bars_compiling`).
    if super::finalizer_bars_compiling(
        shared,
        class_id,
        &cached.method_name,
        &cached.method_descriptor,
    ) {
        return None;
    }
    // JIT round 21 wave 1 (lane indy, INDY21-3): before the compile reads any
    // class's initialization state (`note_static_init_waiters`).
    let static_init_seq0 = static_init_log_seq(shared);
    // The constant-pool resolvers, over one `CpResolvers` shared with the other
    // two compile doors (see the struct).
    let cp = CpResolvers {
        shared,
        class_id,
        substituted_owners: SubstitutedOwners::default(),
        method_name: &cached.method_name,
        method_descriptor: &cached.method_descriptor,
        code: &cached.code,
        ldc2w_trace: Some("upgrade"),
        invoke_operands: Default::default(),
        first_calls_seen: Default::default(),
        deferred_fields_seen: Default::default(),
    };
    // The `cp_*` resolvers themselves come from `cp.with_compile_request`
    // at the request below.
    // PGO-02: receiver class-id -> class-name resolver for a guarded
    // speculative virtual/interface inline plan's SpeculatedReceiver
    // invalidation dependency (plan_inline's fail-closed rule — see
    // docs/feature-designs/profile-guided-inlining.md). `None` (id not
    // loaded, or unloaded between profiling and compiling) refuses
    // that one speculation rather than recording an unmatchable
    // name-less dependency.
    let class_id_namer = |cid: u32| -> Option<String> {
        let cm = shared.classes.class_manager.read();
        cm.get_class(cratonvm_types::ClassId::new(cid))
            .map(|c| c.name.to_string())
    };
    // PGO-02 R0: the BODY a receiver of exactly `cid` dispatches to at a site
    // declared `(cp_class, name, desc)` — what a guard admitting that class may
    // splice. See `resolve_receiver_inline_site` for why the constant-pool
    // callee is the wrong body here.
    let receiver_inline_resolver = |cid: u32, cp_class: &str, name: &str, desc: &str| {
        resolve_receiver_inline_site(
            shared,
            cached.declaring_class_id,
            cid,
            cp_class,
            name,
            desc,
            // No direct-bind resolver on the guarded-virtual path: the
            // closure that owns it is declared further down this function, and
            // a spliced body reached through a receiver guard is planned before
            // it exists. The consequence is a REFUSAL, never a downgrade — a
            // call-carrying body with nothing to bind to is not admitted at all
            // (see the admission rule in `resolve_inline_site_from`).
            None,
        )
    };
    // CHA (NOTES-deopt2.md M5): the single loaded concrete implementor of this
    // site, which is what lets an UNPROFILED compile devirtualize at all.
    let unique_concrete_resolver = |cp_class: &str, name: &str, desc: &str| {
        resolve_unique_concrete_bind(shared, cached.declaring_class_id, cp_class, name, desc)
    };
    // activate-ir-optimizer: elidable-`<init>` resolver for `new` scalar
    // replacement. Now default-ON (soaked: bt10/14/16/18 == HotSpot, POJO probes
    // == HotSpot, 802 jit + 20 differential tests green). `CRATONVM_JIT_SCALAR_NEW=0`
    // is the opt-out safety net — when off, `None` is passed and the IR builder
    // bails on `new`, restoring the single-pass backend for allocation methods.
    let scalar_new_on = true;

    // Callee compiler: given (class_name, method_name, descriptor), try to JIT-compile
    // the callee and return (entry_ptr, needs_context). Used for cross-method direct calls.
    // FIX (jdk-only-string-indexof-int-int-miscompiles-in-the-single-pass-tier):
    // every callee address this compile binds stays pinned until the compile
    // publishes. See the wrapper below for why the bind, and not publication,
    // is where the keep-alive has to start.
    let bind_pins: std::cell::RefCell<Vec<std::sync::Arc<cratonvm_jit::CompiledMethod>>> =
        std::cell::RefCell::new(Vec::new());
    //
    // `holder` is the class whose constant pool names `callee_class`: this
    // method's class for its own sites, a spliced body's declaring class for a
    // call inside that body (`InlineDirectBind`, i11-L0).
    let callee_compiler_bound = |holder: ClassId,
                                 callee_class: &str,
                                 callee_method: &str,
                                 callee_desc: &str|
     -> Option<(usize, bool)> {
        // FJP blocklist + native shadow: see `direct_bind_name_refusal`.
        if let Some(refusal) =
            direct_bind_name_refusal(shared, holder, callee_class, callee_method, callee_desc)
        {
            cratonvm_jit::note_direct_callee_bind_refusal(refusal);
            return None;
        }
        // Interpreter round i1 wave 43, lane L2 (`CompileInputs`): the witness
        // of an in-place callee compile opens before the method is looked up
        // (its `MethodInfo` is cloned under the guard just below, and its
        // bytecode copied from that clone further down). Nested inside the
        // caller's compile, whose older epochs it keeps.
        let callee_witness = BytecodeReadWitness::open();
        // Look up the callee class and method first, so exception table,
        // native shadows, and JIT cache are checked against the resolved callee.
        let (
            callee_class_id,
            declaring_class_arc,
            method,
            declaring_id,
            static_callee_class_initialized,
        ) = {
            let cm = shared.classes.class_manager.read();
            // A substituted owner by its recorded id (i12-L2).
            let Some(bind_class) = direct_bind_callee_class(
                shared,
                &cm,
                &cp.substituted_owners,
                holder,
                callee_class,
                callee_method,
                callee_desc,
            ) else {
                drop(cm);
                // JIT round 19, lane eager19 (TP16-22): the background door's
                // `callee-class-not-found` waiter, for this door's method-entry
                // compiles (`late_bind_upgrade_door_enabled`). Wave 8 (lane
                // osr8, INTERP719-2): and an OSR body's, as an optimizing
                // OSR waiter (`CRATONVM_JIT_LATE_BIND_OSR_OPTIMIZING`).
                note_upgrade_door_late_bind_waiter(
                    shared,
                    osr_door.then_some(inputs.osr_entry_pc).flatten(),
                    osr_door,
                    cratonvm_jit::DirectBindRefusal::CalleeClassNotFound,
                    callee_class,
                    callee_method,
                    callee_desc,
                    cached,
                );
                return None;
            };
            let callee_cid = bind_class.id;
            let store = cm.class_store();
            let (m, d_id) = crate::classloading::find_method_recursive(
                callee_cid,
                callee_method,
                callee_desc,
                store,
            )?;
            if !bind_class.admits(m) {
                return None;
            }
            // The jit-invokestatic-clinit-gap verdict (JVMS §5.5: a raw CALL
            // bypasses every door that initializes the class; initialization
            // is monotone, so one compile-time check holds for the site),
            // taken here so the JIT-cache hit path is gated too: a body
            // compiled while its class's `<clinit>` was still running on
            // another thread is exactly such a hit.
            let initialized = !m.is_static()
                || store
                    .get(d_id)
                    .map(crate::vm::is_class_initialized_fast)
                    .unwrap_or(false);
            // This door publishes under the class that DECLARES the method
            // (`callee_cached` below), so the cache is probed by it first: by
            // the site's name alone, an inherited `Sub.f` -> `A.f` missed a
            // published `A.f` and every caller compile recompiled it
            // (interpreter round i1 wave 13, L3).
            let declaring_name = store.get(d_id)?.name.clone();
            (callee_cid, declaring_name, m.clone(), d_id, initialized)
        };
        // Interpreter round i1 wave 39 (lane L1, cross-lane): no raw CALL into,
        // and no in-place compile of, a method a debugger's breakpoint sits in
        // (`interpreter::breakpoint_bars_compiling`); the site keeps dispatch,
        // whose door refuses the method while the breakpoint stays.
        if super::breakpoint_bars_compiling(shared, declaring_id, callee_method, callee_desc) {
            return None;
        }
        // JIT round 21 wave 11 (lane finalize): nor a finalizable class's
        // constructor that can throw before its `super(..)`.
        if super::finalizer_bars_compiling(shared, declaring_id, callee_method, callee_desc) {
            return None;
        }

        // Synchronized, exception table / no body, uninitialized static
        // declaring class — all BEFORE the cache probe below, so a cache hit
        // cannot bind what they refuse. See `direct_bind_method_refusal`.
        // Round 13 wave 6 (lane sync3): a synchronized instance callee may
        // still bind its published SELF-LOCKING body, and nothing else
        // (`self_locking_body_bind_candidate`; round 13 wave 10, lane sync6:
        // a static one too, once its class is initialized).
        let self_locking_only =
            match direct_bind_method_refusal(&method, static_callee_class_initialized) {
                Some(cratonvm_jit::DirectBindRefusal::Synchronized)
                    if self_locking_body_bind_candidate(
                        &method,
                        static_callee_class_initialized,
                    ) =>
                {
                    true
                }
                Some(refusal) => {
                    cratonvm_jit::note_direct_callee_bind_refusal(refusal);
                    // JIT round 19 (TP16-22 / INTERP719-2): a "not yet"
                    // refusal (an uninitialized static callee's class) leaves
                    // a late-bind waiter; `note_late_bind_waiter` keeps only
                    // the refusals that may lift. j27 wave 4 (lane DISPROWS,
                    // C2W2-6): asked here, where the clinit-gap verdict is
                    // taken; the re-check further down that used to ask it
                    // could no longer refuse (initialization is monotone).
                    note_upgrade_door_late_bind_waiter(
                        shared,
                        osr_door.then_some(inputs.osr_entry_pc).flatten(),
                        osr_door,
                        refusal,
                        callee_class,
                        callee_method,
                        callee_desc,
                        cached,
                    );
                    return None;
                }
                None => false,
            };

        // Check JIT cache first: under the declaring class, then under the
        // site's (`try_jit_compile_callee_slow` publishes by the name it was
        // asked for).
        {
            let jit_cache = shared.jit.jit_cache.read();
            let mut published = jit_cache.get(
                &declaring_class_arc,
                callee_method,
                callee_desc,
                declaring_id,
            );
            if published.is_none() && declaring_id != callee_class_id {
                published =
                    jit_cache.get(callee_class, callee_method, callee_desc, callee_class_id);
            }
            if self_locking_only && !published.as_deref().is_some_and(is_self_locking_body) {
                // JIT round 19 wave 4, lane syncdoor4 (SYNCDOOR4-1): no body
                // at all yet -- compile the callee's self-locking body in
                // place and bind it (`self_lock_eager_bind_enabled`). The
                // cache's `read()` is lock-free, so the compile may publish.
                let eager = self_lock_eager_bind_enabled() && published.is_none();
                let bound = if eager {
                    compile_unpublished_self_locking_callee(
                        shared,
                        "upgrade",
                        holder,
                        callee_class,
                        declaring_id,
                        &declaring_class_arc,
                        callee_method,
                        callee_desc,
                        method.is_static(),
                    )
                } else {
                    None
                };
                if let Some(compiled) = bound {
                    {
                        bind_pins
                            .borrow_mut()
                            .push(std::sync::Arc::clone(&compiled));
                    }
                    // Cast: object/code pointer to integer address
                    return Some((compiled.entry_ptr() as usize, compiled.needs_context()));
                }
                // SYNCDOOR4-1: a method-entry compile left on the helper
                // waits for the callee's self-locking body (TP16-21's waiter,
                // effective under `CRATONVM_JIT_LATE_BIND_SELF_LOCK`), as the
                // background door's refusal already does.
                if self_lock_eager_bind_enabled() && !osr_door {
                    note_self_lock_late_bind_waiter(
                        shared,
                        &declaring_class_arc,
                        callee_method,
                        callee_desc,
                        cached,
                    );
                }
                cratonvm_jit::note_direct_callee_bind_refusal(
                    cratonvm_jit::DirectBindRefusal::Synchronized,
                );
                return None;
            }
            if let Some(compiled) = published {
                // jit-invokedynamic-groovy-regression fix: never bake a
                // direct machine-code CALL to an artifact containing an
                // unconditional invokedynamic trap — its sentinel +
                // stashed frame would bail through the compiled CALLER's
                // epilogue, past the only point (a dispatch helper) that
                // can resume the callee precisely. Returning None keeps
                // the site on `jit_invoke_dispatch`, whose
                // `try_resume_trapped_callee` resolves the trap in place.
                if compiled.has_indy_trap {
                    cratonvm_jit::note_direct_callee_bind_refusal(
                        cratonvm_jit::DirectBindRefusal::IndyTrap,
                    );
                    return None;
                }
                // Round 13 wave 8 (lane codecache): hold THIS body, so a
                // supersede plus retirement before `callee_compiler_in`'s
                // by-address re-pin cannot leave the baked entry unpinned.
                {
                    bind_pins
                        .borrow_mut()
                        .push(std::sync::Arc::clone(&compiled));
                }
                // Cast: object/code pointer to integer address
                return Some((compiled.entry_ptr() as usize, compiled.needs_context()));
                // Cast: JIT entry point to address
            }
        }

        let code_attr = method.code()?;

        let cm = shared.classes.class_manager.read();
        let store = cm.class_store();

        // jit-invokestatic-clinit-gap (2026-07-17; JVMS §5.5): a raw CALL to a
        // static callee bypasses every door that initializes its class, so it
        // is baked only once that class is initialized. The verdict is taken
        // once, before the cache probe (`direct_bind_method_refusal`, with its
        // late-bind waiter); initialization is monotone, so it holds here.
        // j27 wave 4 (lane DISPROWS, C2W2-6): the re-check that stood here
        // could no longer refuse and is gone.

        let declaring_class_name = store.get(declaring_id).map(|c| &*c.name)?;
        let source_file = store
            .get(declaring_id)
            .and_then(|c| c.source_file.as_deref())
            .map(Arc::from);
        let num_params = count_method_params(callee_desc);

        let callee_inputs = callee_witness.inputs(Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: declaring_id,
                class_name: Arc::from(declaring_class_name),
                method_name: Arc::from(callee_method),
                method_descriptor: Arc::from(callee_desc),
                source_file,
                code: crate::runtime::frame::padded_bytecode(&code_attr.code),
                exception_table: Arc::from(code_attr.exception_table.as_slice()),
                max_stack: code_attr.max_stack,
                max_locals: code_attr.max_locals,
                num_params: num_params as u16, // Widening: parameter count conversion
                is_synchronized: method.is_synchronized(),
                is_static: method.is_static(),
            },
        )));
        drop(cm);
        let callee_cached: &Arc<CachedBytecodeMethod> = callee_inputs.cached();

        // S-HIB.1 twin — apply the static skip list to recursive callee
        // compilations from THIS closure too. Before this check, the
        // closure honored only the FJP blocklist + native-shadow gate, so
        // a threshold compile could silently callee-compile methods every
        // other path bans: complex `<init>`/`<clinit>` bodies (observed:
        // `java/util/regex/Pattern.<init>` attempted during the
        // `Pattern.compile` upgrade), `is_known_miscompile` entries, and
        // anything in `CRATONVM_JIT_BISECT_SKIP` — which also made
        // skip-based bisection silently unsound for any method reachable
        // as a direct callee. Mirrors `try_jit_compile_callee`.
        {
            // The static JIT ban list was deleted 2026-07-31 (see
            // docs/feature-designs/native-dispatch-architecture.md).
            // Nothing is statically skipped now; `CRATONVM_JIT_DENY` is the single
            // remaining force-interpret lever, applied in `jit::try_compile`.
            // GPU-offload JIT admission gate — see offload_jit_gate.
            #[cfg(feature = "gpu-offload")]
            if crate::runtime::offload_jit_gate::caller_blocks_jit_by_name(
                shared,
                callee_cached.declaring_class_id,
                callee_method,
                &callee_cached.method_descriptor,
            ) {
                return None;
            }
        }

        // Build resolvers for the callee's constant pool
        let callee_cid = declaring_id;
        // The callee's constant-pool resolvers (see `CpResolvers`). This door
        // never printed its `ldc2_w` answers.
        let c_cp = CpResolvers {
            shared,
            class_id: callee_cid,
            substituted_owners: SubstitutedOwners::default(),
            method_name: &callee_cached.method_name,
            method_descriptor: &callee_cached.method_descriptor,
            code: &callee_cached.code,
            ldc2w_trace: None,
            invoke_operands: Default::default(),
            first_calls_seen: Default::default(),
            deferred_fields_seen: Default::default(),
        };
        // The `cp_*` resolvers themselves come from `c_cp.with_compile_request`
        // at the request below (interpreter round i1 wave 5, L7).
        //
        // The callee's own binder first probes published bodies. Missing static
        // callees may use the existing bounded eager chain: initialized holders,
        // full binding checks, a compile-stack cycle check, depth and fan-out
        // limits. Special calls keep the lookup-only route. Every bound body
        // stays pinned in `c_bind_pins` until the `put` below.
        let c_lookup_on = true;
        let c_bind_pins: std::cell::RefCell<Vec<std::sync::Arc<cratonvm_jit::CompiledMethod>>> =
            std::cell::RefCell::new(Vec::new());
        // `holder` is the class whose constant pool names the callee: this
        // callee's class, or a body spliced into it (`InlineDirectBind`).
        let c_direct_lookup_in = |holder: ClassId,
                                  site_class: &str,
                                  site_method: &str,
                                  site_desc: &str|
         -> Option<(usize, bool)> {
            let body = published_direct_callee_bind(
                shared,
                &c_cp.substituted_owners,
                holder,
                site_class,
                site_method,
                site_desc,
                // JIT round 19 wave 2, lane eager2 (EAGER19-5): the in-place
                // callee is the method whose body holds the site.
                Some(callee_cached.as_ref()),
            )
            .or_else(|| {
                compile_unpublished_static_callee_bind(
                    shared,
                    &c_cp.substituted_owners,
                    holder,
                    site_class,
                    site_method,
                    site_desc,
                    Some(callee_cached.as_ref()),
                )
            })?;
            // Cast: JIT entry point to address
            let bound = (body.entry_ptr() as usize, body.needs_context());
            c_bind_pins.borrow_mut().push(body);
            Some(bound)
        };
        let c_direct_lookup =
            |site_class: &str, site_method: &str, site_desc: &str| -> Option<(usize, bool)> {
                c_direct_lookup_in(callee_cid, site_class, site_method, site_desc)
            };
        // The single-pass inline resolver, as the by-name door builds it
        // (`try_jit_compile_callee_slow`): a spliced body's calls get the same
        // bounded binder, and the substituted owners are this compile's.
        let c_inline_resolver = |site_class: &str,
                                 site_method: &str,
                                 site_desc: &str|
         -> Option<cratonvm_jit::InlineSite> {
            resolve_inline_site(
                shared,
                callee_cid,
                site_class,
                site_method,
                site_desc,
                Some(&c_direct_lookup_in as InlineDirectBind<'_>),
                Some(&c_cp.substituted_owners),
            )
        };
        // The optimizing splices' direct binder (below): the same lookup, under
        // the same gate the main door's IR resolvers ask.
        let c_ir_direct_bind: Option<InlineDirectBind<'_>> =
            if c_lookup_on && cratonvm_jit::ir_splice_direct_call_enabled() {
                Some(&c_direct_lookup_in as InlineDirectBind<'_>)
            } else {
                None
            };
        // PGO-02: receiver class-id -> class-name resolver for a guarded
        // speculative virtual/interface inline plan's SpeculatedReceiver
        // invalidation dependency (plan_inline's fail-closed rule — see
        // docs/feature-designs/profile-guided-inlining.md). `None` (id not
        // loaded, or unloaded between profiling and compiling) refuses
        // that one speculation rather than recording an unmatchable
        // name-less dependency.
        let c_class_id_namer = |cid: u32| -> Option<String> {
            let cm = shared.classes.class_manager.read();
            cm.get_class(cratonvm_types::ClassId::new(cid))
                .map(|c| c.name.to_string())
        };
        // PGO-02 R0: the BODY a receiver of exactly `cid` dispatches to at a
        // site declared `(cp_class, name, desc)`. The guard admits a runtime
        // class, so this — not the constant-pool callee — is what may be
        // spliced behind it. See `resolve_receiver_inline_site`.
        let c_receiver_inline_resolver = |cid: u32, cp_class: &str, name: &str, desc: &str| {
            resolve_receiver_inline_site(shared, callee_cid, cid, cp_class, name, desc, None)
        };
        // CHA (NOTES-deopt2.md M5), resolved in the CALLEE's loader: this door
        // compiles `callee_cid`'s method, so its sites' constant-pool names
        // belong to that class, not to whatever asked for the compile.
        let c_unique_concrete_resolver = |cp_class: &str, name: &str, desc: &str| {
            resolve_unique_concrete_bind(shared, callee_cid, cp_class, name, desc)
        };
        // The optimizing tier's own inline resolver for this callee compile.
        // Its direct-bind resolver is `c_ir_direct_bind`, with the same bounded
        // static-callee compilation as the single-pass resolver. With
        // `CRATONVM_JIT_INPLACE_CALLEE_LOOKUP_BINDER=0` it is `None` again and
        // the calls a body spliced here leaves behind keep the dispatch helper.
        let c_ir_inline_resolver = |callee_class: &str, callee_method: &str, callee_desc: &str| {
            resolve_ir_inline_site(
                shared,
                callee_cid,
                callee_class,
                callee_method,
                callee_desc,
                c_ir_direct_bind,
            )
        };
        // Round 12 wave 5 (lane iropt4, proposal W4-2): the exact-receiver
        // twin of the resolver above, with the same direct-bind resolver. The
        // builder proves every site it splices
        // (`IrBuilder::exact_receiver_proof`).
        let c_ir_exact_receiver_resolver =
            |callee_class: &str, callee_method: &str, callee_desc: &str| {
                resolve_ir_exact_receiver_inline_site(
                    shared,
                    callee_cid,
                    callee_class,
                    callee_method,
                    callee_desc,
                    c_ir_direct_bind,
                )
            };
        // Round 13 wave 12 (lane guardsplice, W7-1): the profile-selected
        // twin, with the same direct-bind resolver.
        let c_ir_receiver_inline_resolver = |receiver_class_id: u32,
                                             callee_class: &str,
                                             callee_method: &str,
                                             callee_desc: &str|
         -> Option<cratonvm_jit::InlineSite> {
            resolve_ir_profile_receiver_inline_site(
                shared,
                callee_cid,
                receiver_class_id,
                callee_class,
                callee_method,
                callee_desc,
                c_ir_direct_bind,
            )
        };
        // Elidable-`<init>` resolver for `new` scalar replacement, default-ON
        // (opt-out: CRATONVM_JIT_SCALAR_NEW=0).
        let c_scalar_new_on = true;
        // The tier the supersede door would give this callee (round 11 wave 10,
        // lane `synccall`, page
        // `r11w9-tier-optimizing-door-callee-compile-tier-policy-patch`): an
        // unseen callee compiled here gets no hotness check, so it must not
        // also skip the C2 gate's `new` term. For every other refusal the IR
        // pipeline would decline anyway and the body is the same single-pass
        // one, minus a wasted IR attempt. `code` is the padded bytecode (+2
        // zero tail), hence the `saturating_sub(2)` `background_compile_task`
        // uses for its own call.
        let c_optimize = cratonvm_jit::c2_upgrade_would_engage(
            &callee_cached.code,
            callee_cached.code.len().saturating_sub(2),
            callee_desc,
            crate::runtime::env_cache::jit_ir_long(),
            true,
            crate::runtime::env_cache::jit_ir_call_virtual(),
        );

        // Compile the callee with no recursive COMPILE: its binder and inline
        // resolvers only look up published bodies (`c_direct_lookup`, above).
        let c_pgo_profile = {
            let profile_key = crate::jit::profile::MethodKey {
                class_id: callee_cached.declaring_class_id.as_u32(),
                method_name: callee_cached.method_name.clone(),
                descriptor: callee_cached.method_descriptor.clone(),
            };
            shared.jit.profile_store.get_profile(&profile_key)
        };
        let c_helpers = crate::jit::helpers::build_helpers_for(shared);
        let c_string_layout_resolver = || resolve_string_field_layout(shared);
        let self_call_identity =
            self_call_identity_stable(shared, callee_cached.declaring_class_id);
        // JDK-ONLY-WAVE2 §4. Answers "would the generic tail admit this
        // triple's registered native?" — which since 2026-09-22 is a wider
        // question than "is it a reviewed `NativeKind::Intrinsic`?", because a
        // refused bind never stopped the call: the site keeps its `invoke_info`
        // and `vm_exec::invoke_or_native` ran the same `Bridge` anyway. Still
        // fail-closed for a `SyntheticStub`, for anything unregistered and for
        // an armed `CRATONVM_ENFORCE_NATIVE_SHADOW`. The one predicate is
        // `helpers::direct_native_bind_admitted`, shared with the callee-side
        // gate; this closure used to be a third hand-written copy of a question
        // that had already been hoisted there.
        // Cheap: only the strict arm of `direct_native_helper` calls it, and only
        // for a triple whose helper cell is already non-zero.
        let direct_bind_admission = |class: &str, method: &str, descriptor: &str| -> bool {
            crate::jit::helpers::direct_native_bind_admitted(shared, class, method, descriptor)
        };

        // Round 9 wave 7 (`mutrec7`): see `cycle_edge_target_admitted`.
        let c_class_is_final_resolver = |cid: u32| jit_class_is_final_for_typecheck(shared, cid);
        // JIT r20 wave 11 (lane pIR): see `jit_class_is_subclass_for_catch`.
        let c_class_is_subclass_resolver =
            |thrown: u32, catch_id: u32| jit_class_is_subclass_for_catch(shared, thrown, catch_id);
        let c_cycle_edge_admission =
            |caller_class_id: u32, callee_class: &str, callee_method: &str, callee_desc: &str| {
                cycle_edge_target_admitted(
                    shared,
                    ClassId::new(caller_class_id),
                    callee_class,
                    callee_method,
                    callee_desc,
                )
            };
        // Round 15 wave 2 (lane calls2, CR15-4): this compile publishes an
        // acceptance verdict nobody reads. It used to REPLACE the enclosing
        // compile's pending one (when run from that compile's single-pass
        // fall-through, after its gate) and stay in the slot for the next
        // reader on this thread. Now the enclosing verdict is handed back and
        // this one discarded (`optimizing_door_own_verdict_enabled`).
        let c_outer_verdict =
            optimizing_door_own_verdict_enabled().then(NestedVerdictHandOff::take_for_this_compile);
        // JIT round 21 lane indylate2 (`CRATONVM_JIT_INDY_ADMIT_BY_ID`,
        // re-landed t1 wave 8 lane RL-REFL): the direct binder and the
        // optimizing inliner asked in the class context of an indy-planned
        // site (its target class), whose name this class's loader may not
        // answer (`CompileRequest::indy_context_callee_compiler`).
        let c_indy_context_callee =
            |context: u32, site_class: &str, site_method: &str, site_desc: &str| {
                c_direct_lookup_in(ClassId::new(context), site_class, site_method, site_desc)
            };
        let c_indy_context_ir_inline =
            |context: u32, site_class: &str, site_method: &str, site_desc: &str| {
                resolve_ir_inline_site(
                    shared,
                    ClassId::new(context),
                    site_class,
                    site_method,
                    site_desc,
                    c_ir_direct_bind,
                )
            };
        let c_indy_by_id = jit_indy_admit_by_id_enabled();
        // Every `cp_*` resolver (and `cached` / `helpers`) comes from
        // `cp_request`; the fields below are this door's own.
        // JIT round 21 lane indylate2 (`CRATONVM_JIT_INDY_ADMIT_BY_ID`): the
        // direct binder and the optimizing inliner asked in the class context
        // of an indy-planned site (its target class), whose name this class's
        // loader may not answer (`CompileRequest::indy_context_callee_compiler`).
        let c_indy_context_callee =
            |context: u32, site_class: &str, site_method: &str, site_desc: &str| {
                c_direct_lookup_in(ClassId::new(context), site_class, site_method, site_desc)
            };
        let c_indy_context_ir_inline =
            |context: u32, site_class: &str, site_method: &str, site_desc: &str| {
                resolve_ir_inline_site(
                    shared,
                    ClassId::new(context),
                    site_class,
                    site_method,
                    site_desc,
                    c_ir_direct_bind,
                )
            };
        let c_indy_by_id = jit_indy_admit_by_id_enabled();
        let compiled = c_cp.with_compile_request(&callee_inputs, &c_helpers, |cp_request| {
            crate::jit::try_compile_request(&crate::jit::CompileRequest {
                // Lookup-only (CR15-1, above); `None` with the switch off.
                callee_compiler: if c_lookup_on {
                    Some(&c_direct_lookup)
                } else {
                    None
                },
                profile: c_pgo_profile.as_ref(),
                // The single-pass splices (CR15-1); `None` with the switch off.
                inline_resolver: if c_lookup_on {
                    Some(&c_inline_resolver)
                } else {
                    None
                },
                // String call-site intrinsics (length/charAt/hashCode/equals/…):
                // resolve java/lang/String's value/coder/hash field layout so the
                // JIT inlines these accessors instead of crossing the VM→native
                // boundary per call (bug-03). `resolve_string_field_layout`
                // returns None → intrinsics bail to dispatch when String isn't
                // loaded yet.
                string_layout_resolver: Some(&c_string_layout_resolver),
                cp_elidable_init_resolver: if c_scalar_new_on {
                    cp_request.cp_elidable_init_resolver
                } else {
                    None
                },
                // C2 only when the supersede door's gate would take it — see
                // `c_optimize` above.
                optimize: c_optimize,
                // Gap B: int-only invokestatic → Op::Call. Now default-ON
                // (inc 23, soaked: bt10/14/16/18 == HotSpot + IrCall/IrCallGc
                // probes == HotSpot, ON==OFF). `CRATONVM_JIT_IR_CALL=0` is the
                // opt-out — restores single-pass dispatch for invokestatic.
                ir_emit_calls: crate::runtime::env_cache::jit_ir_call(),
                // inc 24/29: invokespecial → Op::Call. Now default-ON;
                // `CRATONVM_JIT_IR_CALL_SPECIAL=0` opts out.
                ir_emit_special_calls: true,
                // inc 25/29: long methods → IR path. Now default-ON; `CRATONVM_JIT_IR_LONG=0` opts out.
                ir_emit_long: crate::runtime::env_cache::jit_ir_long(),
                // inc 26 + inline-cache lowering: invokevirtual/invokeinterface
                // → Op::Call with MIC/PIC fast paths. Default-ON now that the IR
                // backend has parity with single-pass dispatch;
                // `CRATONVM_JIT_IR_CALL_VIRTUAL=0` opts out.
                ir_emit_virtual_calls: crate::runtime::env_cache::jit_ir_call_virtual(),
                // inc 30 + Slices A/B/C: double/float XMM value tier. Now
                // default-ON — the tier is opcode-complete (frem/drem, FP arrays,
                // FP-slot deopt resume all landed) and validated == HotSpot
                // (bt10/14/16/18 checksums + FP E2E probes). `CRATONVM_JIT_IR_FP=0`
                // is the opt-out (restores the int/long/ref-only IR path).
                ir_emit_fp: true,
                class_id_name_resolver: if crate::runtime::env_cache::jit_guarded_virtual_inline() {
                    Some(&c_class_id_namer)
                } else {
                    None
                },
                receiver_inline_resolver: if crate::runtime::env_cache::jit_guarded_virtual_inline()
                {
                    Some(&c_receiver_inline_resolver)
                } else {
                    None
                },
                // CHA has its own lever (`CRATONVM_JIT_CHA=0`) AND rides the guard
                // it feeds. The conjunction is the point: `jit_cha` makes "is
                // class-hierarchy analysis the cause" a one-variable bisect that
                // leaves the receiver-profile devirtualization in place, while
                // `jit_guarded_virtual_inline` stays load-bearing because CHA's
                // verdict is lowered as a receiver class-id compare — with no
                // namer the plan would be refused for want of an invalidation
                // dependency anyway. See `env_cache::jit_cha`.
                unique_concrete_resolver: if crate::runtime::env_cache::jit_cha()
                    && crate::runtime::env_cache::jit_guarded_virtual_inline()
                {
                    Some(&c_unique_concrete_resolver)
                } else {
                    None
                },
                // IR-tier inlining. Behind its own gate; `None` splices nothing.
                ir_inline_resolver: if cratonvm_jit::ir_inline_enabled() {
                    Some(&c_ir_inline_resolver)
                } else {
                    None
                },
                // JIT round 21 lane indylate2: see `c_indy_context_callee`.
                indy_context_callee_compiler: if c_lookup_on && c_indy_by_id {
                    Some(&c_indy_context_callee)
                } else {
                    None
                },
                indy_context_ir_inline_resolver: if cratonvm_jit::ir_inline_enabled()
                    && c_indy_by_id
                {
                    Some(&c_indy_context_ir_inline)
                } else {
                    None
                },
                // Round 12 wave 5 (lane iropt4): exact-receiver splices, as at
                // the main door below.
                ir_exact_receiver_resolver: if cratonvm_jit::ir_inline_enabled()
                    && cratonvm_jit::ir::ir_exact_receiver_splice_enabled()
                {
                    Some(&c_ir_exact_receiver_resolver)
                } else {
                    None
                },
                // Round 13 wave 12 (lane guardsplice, W7-1): profile-selected
                // guarded splices, as at the main door below.
                ir_receiver_inline_resolver: if cratonvm_jit::ir_inline_enabled()
                    && cratonvm_jit::ir::ir_profile_guarded_splice_enabled()
                {
                    Some(&c_ir_receiver_inline_resolver)
                } else {
                    None
                },
                // Per-VM Real-JDK policy (JDK-ONLY-WAVE2 §2). Was a process-global
                // latch the JIT read for itself, so a `Compatible` VM sharing a
                // process with a `JdkOnly` one lost the thin direct-call helpers.
                jdk_only: crate::vm::dispatch_policy(shared).is_jdk_only(),
                // JDK-ONLY-WAVE2 §4: the registry's own `NativeKind`, in place of
                // the JIT's seven hard-coded triples, as the §1.4 verdict on
                // whether a thin direct-call helper may shadow real bytecode.
                direct_bind_admission: Some(&direct_bind_admission),
                // This VM's per-bci de-spec registry.
                despec: Some(&shared.jit.despec_registry),
                // M7 — this VM's runtime de-speculation verdicts.
                runtime_despec: Some(&shared.jit.runtime_despec),
                self_call_identity_stable: self_call_identity,
                direct_helpers: &crate::jit::helpers::direct_helper_table_for(shared),
                cycle_edge_admission: Some(&c_cycle_edge_admission),
                // Round 9 wave 8 (`arraylist8`): final-class verdicts for the
                // inline `instanceof` miss. See `jit_class_is_final_for_typecheck`.
                class_is_final_resolver: Some(&c_class_is_final_resolver),
                class_is_subclass_resolver: Some(&c_class_is_subclass_resolver),
                ..cp_request
            })
        });
        if c_outer_verdict.is_some() {
            // This compile's own verdict: no reader (see `c_outer_verdict`).
            let _ = cratonvm_jit::ir_evidence::take_last_verdict();
        }
        drop(c_outer_verdict);
        let mut compiled = compiled?;
        let entry = compiled.entry_ptr() as usize; // Cast: JIT entry point to address
        let needs_ctx = compiled.needs_context();
        // jit-invokedynamic-groovy-regression fix — see the matching gate
        // at the JIT-cache-hit return above. The artifact is still cached
        // (below) for helper/interpreter dispatch, but never handed back
        // for a baked direct machine-code CALL.
        let indy_trap = compiled.has_indy_trap;
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] callee-compile {}.{}{} entry={:p} len={}",
                callee_cached.class_name,
                callee_cached.method_name,
                callee_cached.method_descriptor,
                compiled.entry_ptr(),
                compiled.code_bytes().len()
            );
        }
        crate::jit::disasm::maybe_dump_annotated(
            if compiled.used_ir_backend {
                "callee/ir"
            } else {
                "callee/sp"
            },
            &callee_cached.class_name,
            &callee_cached.method_name,
            &callee_cached.method_descriptor,
            compiled.entry_ptr(),
            compiled.code_bytes(),
            compiled.osr_pc_to_native.as_deref(),
            compiled.osr_local_assignments.as_deref(),
        );

        // Store in JIT cache
        stamp_compilation_epoch(
            shared,
            &callee_cached.class_name,
            &callee_cached.method_name,
            &callee_cached.method_descriptor,
            &mut compiled,
        );
        // Round 13 wave 3 (lane replay2; R13-1): the bytecode this body was
        // compiled from, for a trap after its class is redefined.
        stamp_compiled_source_of(&compiled, Arc::clone(callee_cached));
        // Stamp the wrapped-entry requirement before the body is shared.
        // See `CompiledMethod::requires_wrapped_entry`: publication is the
        // last point that still knows this is an `ACC_SYNCHRONIZED` method,
        // and every unwrapped consumer downstream holds only a raw entry
        // pointer.
        compiled.requires_wrapped_entry = callee_cached.is_synchronized;
        // Round 15 wave 2 (lane calls2): tier proposal R12-4 for this door too.
        // The cache probe above missed, but an optimizing body of the callee
        // (the background worker's) may have been published while this one
        // compiled; `put` keeps it rather than replace it with a single-pass
        // body. An optimizing body here is unaffected (`put` asks
        // `used_ir_backend`). `CRATONVM_JIT_KEEP_OPTIMIZING_BODY=0` restores
        // the replace.
        compiled.keep_live_optimizing_body =
            crate::jit::tiered::keep_live_optimizing_body_enabled();
        let published_here = {
            let mut jit_cache = shared.jit.jit_cache.write();
            // VH3-1: see `compile_osr_body`.
            if !crate::jit::helpers::varhandle_thin_bound_artifact_is_stale(shared, &compiled) {
                jit_cache.put(
                    callee_cached.class_name.clone(),
                    callee_cached.method_name.clone(),
                    callee_cached.method_descriptor.clone(),
                    callee_cached.declaring_class_id,
                    compiled,
                );
            }
            // THIS body, if `put` kept it: a refused publish (install epoch,
            // code-cache cap) or a concurrent winner is not ours to report.
            jit_cache
                .get(
                    &callee_cached.class_name,
                    &callee_cached.method_name,
                    &callee_cached.method_descriptor,
                    callee_cached.declaring_class_id,
                )
                // Cast: JIT entry point to address, compared with `entry`.
                .filter(|body| body.entry_ptr() as usize == entry)
        };
        // Round 11 wave 18, P1: the tiered manager never scheduled this body,
        // so without this it kept the method at `Interpreter`
        // (`r11-orch-callee-compiled-bodies-are-never-promoted`). After the
        // `jit_cache` guard is gone: this takes the manager's mutex.
        if let Some(body) = &published_here {
            note_callee_publish_to_tiered_manager(
                shared,
                callee_cached.declaring_class_id,
                &callee_cached.class_name,
                &callee_cached.method_name,
                &callee_cached.method_descriptor,
                body,
            );
        }
        let kept = published_here.is_some();
        drop(published_here);

        if indy_trap {
            return None;
        }
        // Round 13 wave 8 (lane codecache,
        // `r13w8-codecache-baked-entry-of-a-refused-callee-FIXED-20260928.md`): bind
        // only an entry the cache KEPT. `put` refuses (install epoch,
        // dependencies, unpinnable callees, a redefined baked callee) by
        // dropping the body, which unmaps it at once, so `entry` then names
        // freed memory -- and the caller's own buffer, mapped moments later by
        // this same compile, is routinely placed over it. The pin in
        // `callee_compiler_in` misses such an entry, but the binding was still
        // returned and baked; the optimizing OSR door, which never publishes,
        // then CALLed it.
        if !kept {
            return None;
        }
        Some((entry, needs_ctx))
    };
    // The pinning wrapper. `callee_compiler_bound` decides WHETHER to bind and to what;
    // this holds the answer alive. A raw entry address is only as meaningful
    // as the artifact that owns it, and between this bind and
    // `prepare_for_publication` there is a whole compilation during which that
    // artifact can be superseded, retired, freed and its buffer re-handed to
    // an unrelated method. `pin_jit_entry` upgrades the owner's `Weak` here,
    // while it is still the callee, so the address cannot change meaning
    // underneath the code being emitted against it.
    let callee_compiler_in = |holder: ClassId,
                              callee_class: &str,
                              callee_method: &str,
                              callee_desc: &str|
     -> Option<(usize, bool)> {
        let bound = callee_compiler_bound(holder, callee_class, callee_method, callee_desc);
        if let Some((entry, _)) = bound {
            let Some(owner) = cratonvm_jit::pin_jit_entry(entry) else {
                // A binding is usable only while its entry owner can be
                // upgraded. Returning the raw address after a pin miss lets a
                // recycled code buffer be baked into a direct CALL.
                return None;
            };
            bind_pins.borrow_mut().push(owner);
        }
        bound
    };
    // What the jit crate asks for this method's OWN call sites, whose names
    // come from this class's constant pool.
    let callee_compiler =
        |callee_class: &str, callee_method: &str, callee_desc: &str| -> Option<(usize, bool)> {
            callee_compiler_in(class_id, callee_class, callee_method, callee_desc)
        };
    // Round 11 wave 10 (lane `synccall`, page
    // `r11w9-sync-caller-held-monitor-direct-call-patch`): the `static
    // synchronized` callee this compile may CALL directly while it holds the
    // class monitor itself (`CompileRequest::sync_direct_lookup`). Since
    // round 11 wave 17 a callee with no published body is compiled by the
    // lookup (`sync_direct_compile_unpublished_callee`), since a caller
    // compiled once never gets a "next compile" to bind it in. Its refusals
    // are already counted once for the site, as
    // `DirectBindRefusal::Synchronized`, by `callee_compiler` above.
    // Round 11 wave 13 (lane `spsync`): the body is `sync_direct_target`,
    // shared with the single-pass compile door; this closure only pins.
    let sync_direct_lookup =
        |cp_idx: u16| -> Option<cratonvm_jit::compile_request::SyncDirectTarget> {
            // Round 14 wave 3 (lane sync, SS-2): a refused body still answers
            // a static callee's monitor half (`entry == 0`, nothing to pin).
            let Some((target, pin)) = sync_direct_target(shared, class_id, &cp, cp_idx) else {
                return sync_direct_monitor_answer(shared, class_id, &cp, cp_idx);
            };
            // Pinned with the other bound callees until the caller publishes.
            bind_pins.borrow_mut().push(pin);
            Some(target)
        };

    let pgo_profile = {
        let profile_key = crate::jit::profile::MethodKey {
            class_id: cached.declaring_class_id.as_u32(),
            method_name: cached.method_name.clone(),
            descriptor: cached.method_descriptor.clone(),
        };
        shared.jit.profile_store.get_profile(&profile_key)
    };
    let helpers = crate::jit::helpers::build_helpers_for(shared);

    // The optimizing tier's own inline resolver — see `resolve_ir_inline_site`
    // for how its admission set differs in both directions.
    //
    // `callee_compiler` as the direct-bind resolver, which is the SAME one this
    // compile hands `try_compile` for its own call sites and the same one the
    // single-pass `inline_resolver` hands its spliced bodies (in
    // `try_jit_compile_callee_slow`; this door's copy went with
    // `CRATONVM_JIT_MAIN_INLINE`, JIT round 14 wave 2). Until
    // 2026-09-09 this argument was absent, and the note where it should have
    // been said "`IrBuilder` has no direct-call lowering inside a relocated
    // body to bake an entry into". That was true; it no longer is
    // (`ir_direct_calls` is keyed by combined-buffer pc and
    // `append_ir_inline_site` fills it), and while it was true every
    // statically-bound call an optimizing splice left behind lowered to
    // `jit_invoke_dispatch` and resolved its callee BY NAME on every
    // execution — the measured 3.5x loss the sibling's own comment describes,
    // paid on the tier that is supposed to be the fast one.
    //
    // Using the same resolver as the sibling matters beyond symmetry: it bounds
    // its own recursion (depth, cycle, fan-out), which is why the sibling's
    // comment gives reuse as the reason not to write a lookup by hand here.
    let ir_inline_resolver = |callee_class: &str,
                              callee_method: &str,
                              callee_desc: &str|
     -> Option<cratonvm_jit::InlineSite> {
        resolve_ir_inline_site(
            shared,
            cached.declaring_class_id,
            callee_class,
            callee_method,
            callee_desc,
            if cratonvm_jit::ir_splice_direct_call_enabled() {
                Some(&callee_compiler_in as InlineDirectBind<'_>)
            } else {
                None
            },
        )
    };
    // Round 12 wave 4 (lane iropt3, W3-1): the exact-receiver twin of the
    // resolver above, with the same direct-bind resolver. See
    // `resolve_ir_exact_receiver_inline_site`.
    let ir_exact_receiver_resolver = |callee_class: &str,
                                      callee_method: &str,
                                      callee_desc: &str|
     -> Option<(cratonvm_jit::InlineSite, u32)> {
        resolve_ir_exact_receiver_inline_site(
            shared,
            cached.declaring_class_id,
            callee_class,
            callee_method,
            callee_desc,
            if cratonvm_jit::ir_splice_direct_call_enabled() {
                Some(&callee_compiler_in as InlineDirectBind<'_>)
            } else {
                None
            },
        )
    };
    // Round 13 wave 12 (lane guardsplice, W7-1): the profile-selected twin,
    // with the same direct-bind resolver. See
    // `resolve_ir_profile_receiver_inline_site`.
    let ir_receiver_inline_resolver = |receiver_class_id: u32,
                                       callee_class: &str,
                                       callee_method: &str,
                                       callee_desc: &str|
     -> Option<cratonvm_jit::InlineSite> {
        resolve_ir_profile_receiver_inline_site(
            shared,
            cached.declaring_class_id,
            receiver_class_id,
            callee_class,
            callee_method,
            callee_desc,
            if cratonvm_jit::ir_splice_direct_call_enabled() {
                Some(&callee_compiler_in as InlineDirectBind<'_>)
            } else {
                None
            },
        )
    };
    let string_layout_resolver = || resolve_string_field_layout(shared);
    let self_call_identity = self_call_identity_stable(shared, cached.declaring_class_id);
    // JDK-ONLY-WAVE2 §4. Answers "would the generic tail admit this
    // triple's registered native?" — which since 2026-09-22 is a wider
    // question than "is it a reviewed `NativeKind::Intrinsic`?", because a
    // refused bind never stopped the call: the site keeps its `invoke_info`
    // and `vm_exec::invoke_or_native` ran the same `Bridge` anyway. Still
    // fail-closed for a `SyntheticStub`, for anything unregistered and for
    // an armed `CRATONVM_ENFORCE_NATIVE_SHADOW`. The one predicate is
    // `helpers::direct_native_bind_admitted`, shared with the callee-side
    // gate; this closure used to be a third hand-written copy of a question
    // that had already been hoisted there.
    // Cheap: only the strict arm of `direct_native_helper` calls it, and only
    // for a triple whose helper cell is already non-zero.
    let direct_bind_admission = |class: &str, method: &str, descriptor: &str| -> bool {
        crate::jit::helpers::direct_native_bind_admitted(shared, class, method, descriptor)
    };

    // Round 9 wave 7 (`mutrec7`): see `cycle_edge_target_admitted`.
    let class_is_final_resolver = |cid: u32| jit_class_is_final_for_typecheck(shared, cid);
    // JIT r20 wave 11 (lane pIR): see `jit_class_is_subclass_for_catch`.
    let class_is_subclass_resolver =
        |thrown: u32, catch_id: u32| jit_class_is_subclass_for_catch(shared, thrown, catch_id);
    let cycle_edge_admission =
        |caller_class_id: u32, callee_class: &str, callee_method: &str, callee_desc: &str| {
            cycle_edge_target_admitted(
                shared,
                ClassId::new(caller_class_id),
                callee_class,
                callee_method,
                callee_desc,
            )
        };
    // Round 15 wave 2 (lane calls2, CR15-4): this compile's verdict comes back
    // as a value (`PinnedArtifact::verdict`). A pending one is an enclosing
    // compile's (handed back below) or, at the top level, stale (discarded):
    // see `NestedVerdictHandOff`.
    let own_verdict_on = optimizing_door_own_verdict_enabled();
    let outer_verdict = own_verdict_on.then(NestedVerdictHandOff::take_for_this_compile);
    // Every `cp_*` resolver comes from `cp_request` (`CpResolvers::
    // with_compile_request`); the fields below are this door's own.
    // JIT round 19 wave 2, lane eager2 (EAGER19-1): method-entry compiles
    // only; an OSR body's trap would ask a method-entry recompile its looping
    // frame never enters (EAGER19-4).
    let uninit_new_trap =
        |cp_idx: u16, pc: usize| uninit_new_trap_admitted(shared, cached, cp_idx, pc);
    // JIT round 19 wave 3, lane eager3 (EAGER2-3): the class-initialized
    // guard, method-entry compiles only for the same reason.
    let uninit_new_guard =
        |cp_idx: u16, pc: usize| uninit_new_guard_admitted(shared, cached, cp_idx, pc);
    // Interpreter round i4 wave 94, lane L2 (`CRATONVM_JIT_IR_FIELD_SITE_TRAP`):
    // the unresolved-field trap, method-entry compiles only for the same
    // reason as the two above.
    let field_site_trap =
        |cp_idx: u16, pc: usize| field_site_trap_admitted(shared, cached, cp_idx, pc, "upgrade");
    // JIT round 21 lane indylate2 (`CRATONVM_JIT_INDY_ADMIT_BY_ID`): the
    // direct binder and the optimizing inliner asked in the class context of
    // an indy-planned site (its target class), whose name this class's loader
    // may not answer (`CompileRequest::indy_context_callee_compiler`).
    let indy_context_callee =
        |context: u32, callee_class: &str, callee_method: &str, callee_desc: &str| {
            callee_compiler_in(
                ClassId::new(context),
                callee_class,
                callee_method,
                callee_desc,
            )
        };
    let indy_context_ir_inline =
        |context: u32, callee_class: &str, callee_method: &str, callee_desc: &str| {
            resolve_ir_inline_site(
                shared,
                ClassId::new(context),
                callee_class,
                callee_method,
                callee_desc,
                if cratonvm_jit::ir_splice_direct_call_enabled() {
                    Some(&callee_compiler_in as InlineDirectBind<'_>)
                } else {
                    None
                },
            )
        };
    let indy_by_id = jit_indy_admit_by_id_enabled();
    // t1 wave 10 (lane LCAP2, `CRATONVM_JIT_IR_CONTEXT_ABI_OF_INSTALLED`,
    // default OFF): the entry ABI of the method-entry body this compile will
    // supersede (`CompileRequest::installed_body_needs_context`). Not for the
    // OSR door: an OSR body supersedes nothing.
    let installed_body_needs_context =
        if !osr_door && cratonvm_jit::ir_context_abi_of_installed_enabled() {
            let current = shared.jit.jit_cache.read().get(
                &cached.class_name,
                &cached.method_name,
                &cached.method_descriptor,
                cached.declaring_class_id,
            );
            let abi = current.as_ref().map(|body| body.needs_context());
            drop(current.map(cratonvm_jit::RetainedCode::new));
            abi
        } else {
            None
        };
    let compiled = cp.with_compile_request(inputs, &helpers, |cp_request| {
        crate::jit::try_compile_request(&crate::jit::CompileRequest {
            installed_body_needs_context,
            callee_compiler: Some(&callee_compiler),
            cp_uninit_new_trap_resolver: if osr_door {
                None
            } else {
                Some(&uninit_new_trap)
            },
            cp_field_trap_resolver: if osr_door {
                None
            } else {
                Some(&field_site_trap)
            },
            cp_uninit_new_guard_resolver: if osr_door {
                None
            } else {
                Some(&uninit_new_guard)
            },
            profile: pgo_profile.as_ref(),
            // No single-pass splices on this (optimizing) door: the IR splices
            // (`ir_inline_resolver`) replaced them. The opt-in that wired them
            // here, `CRATONVM_JIT_MAIN_INLINE` (default off since 2026-06 over a
            // `try_emit_inline_body` miscompile, a Spring boot enum CCE), was
            // deleted in JIT round 14 wave 2.
            inline_resolver: None,
            // String call-site intrinsics (length/charAt/hashCode/equals/…): resolve
            // java/lang/String's value/coder/hash field layout so the JIT inlines
            // these accessors instead of crossing the VM→native boundary per call
            // (bug-03). Mirrors the already-wired `try_jit_compile_callee_slow` path.
            string_layout_resolver: Some(&string_layout_resolver),
            cp_elidable_init_resolver: if scalar_new_on {
                cp_request.cp_elidable_init_resolver
            } else {
                None
            },
            // Inline mutator compile path is the optimized (C2-equivalent) tier.
            optimize: true,
            // Gap B: int-only invokestatic → Op::Call. Now default-ON (inc 23);
            // `CRATONVM_JIT_IR_CALL=0` is the opt-out (single-pass dispatch).
            ir_emit_calls: crate::runtime::env_cache::jit_ir_call(),
            // inc 24/29: invokespecial → Op::Call. Now default-ON; `CRATONVM_JIT_IR_CALL_SPECIAL=0` opts out.
            ir_emit_special_calls: true,
            // inc 25/29: long methods → IR path. Now default-ON; `CRATONVM_JIT_IR_LONG=0` opts out.
            ir_emit_long: crate::runtime::env_cache::jit_ir_long(),
            // inc 26: invokevirtual/invokeinterface → Op::Call (dynamic dispatch via
            // the helper), gated default-OFF (its own soak). `=1` opts in.
            ir_emit_virtual_calls: crate::runtime::env_cache::jit_ir_call_virtual(),
            // inc 30 + Slices A/B/C: double/float XMM value tier. Now default-ON
            // (opcode-complete + validated == HotSpot). `CRATONVM_JIT_IR_FP=0` opts out.
            ir_emit_fp: true,
            class_id_name_resolver: if crate::runtime::env_cache::jit_guarded_virtual_inline() {
                Some(&class_id_namer)
            } else {
                None
            },
            receiver_inline_resolver: if crate::runtime::env_cache::jit_guarded_virtual_inline() {
                Some(&receiver_inline_resolver)
            } else {
                None
            },
            // CHA has its own lever (`CRATONVM_JIT_CHA=0`) AND rides the guard it
            // feeds. The conjunction is the point: `jit_cha` makes "is
            // class-hierarchy analysis the cause" a one-variable bisect that leaves
            // the receiver-profile devirtualization in place, while
            // `jit_guarded_virtual_inline` stays load-bearing because CHA's verdict
            // is lowered as a receiver class-id compare — with no namer the plan
            // would be refused for want of an invalidation dependency anyway. See
            // `env_cache::jit_cha`.
            unique_concrete_resolver: if crate::runtime::env_cache::jit_cha()
                && crate::runtime::env_cache::jit_guarded_virtual_inline()
            {
                Some(&unique_concrete_resolver)
            } else {
                None
            },
            // IR-tier inlining. Behind its own gate; `None` splices nothing.
            ir_inline_resolver: if cratonvm_jit::ir_inline_enabled() {
                Some(&ir_inline_resolver)
            } else {
                None
            },
            // JIT round 21 lane indylate2: see `indy_context_callee`.
            indy_context_callee_compiler: if indy_by_id {
                Some(&indy_context_callee)
            } else {
                None
            },
            indy_context_ir_inline_resolver: if cratonvm_jit::ir_inline_enabled() && indy_by_id {
                Some(&indy_context_ir_inline)
            } else {
                None
            },
            // Round 12 wave 4 (lane iropt3): exact-receiver splices. Same gate
            // as the plain IR inliner, plus its own kill switch.
            ir_exact_receiver_resolver: if cratonvm_jit::ir_inline_enabled()
                && cratonvm_jit::ir::ir_exact_receiver_splice_enabled()
            {
                Some(&ir_exact_receiver_resolver)
            } else {
                None
            },
            // Round 13 wave 12 (lane guardsplice, W7-1): profile-selected
            // guarded splices. Same gate as the plain IR inliner, plus the
            // guarded splice's two kill switches.
            ir_receiver_inline_resolver: if cratonvm_jit::ir_inline_enabled()
                && cratonvm_jit::ir::ir_profile_guarded_splice_enabled()
            {
                Some(&ir_receiver_inline_resolver)
            } else {
                None
            },
            // Per-VM Real-JDK policy (JDK-ONLY-WAVE2 §2). Was a process-global
            // latch the JIT read for itself, so a `Compatible` VM sharing a
            // process with a `JdkOnly` one lost the thin direct-call helpers.
            jdk_only: crate::vm::dispatch_policy(shared).is_jdk_only(),
            // JDK-ONLY-WAVE2 §4: the registry's own `NativeKind`, in place of
            // the JIT's seven hard-coded triples, as the §1.4 verdict on
            // whether a thin direct-call helper may shadow real bytecode.
            direct_bind_admission: Some(&direct_bind_admission),
            // This VM's per-bci de-spec registry.
            despec: Some(&shared.jit.despec_registry),
            // M7 — this VM's runtime de-speculation verdicts.
            runtime_despec: Some(&shared.jit.runtime_despec),
            self_call_identity_stable: self_call_identity,
            direct_helpers: &crate::jit::helpers::direct_helper_table_for(shared),
            cycle_edge_admission: Some(&cycle_edge_admission),
            // Round 9 wave 8 (`arraylist8`): final-class verdicts for the inline
            // `instanceof` miss. See `jit_class_is_final_for_typecheck`.
            class_is_final_resolver: Some(&class_is_final_resolver),
            class_is_subclass_resolver: Some(&class_is_subclass_resolver),
            // Round 11 wave 10 (lane `synccall`): caller-held synchronized
            // direct CALLs. See `sync_direct_lookup` above.
            sync_direct_lookup: Some(&sync_direct_lookup),
            // Interpreter round i1 wave 15 (lane L2): see the doc above.
            osr_mode_exit_polls: osr_door,
            // Wave 38 (lane L2): the optimizing OSR route's backend refusal is
            // memoised by `build_osr_optimizing_artifact`, not bail-listed.
            osr_optimizing_route: osr_door,
            osr_requested_bci: inputs.osr_entry_pc.filter(|_| osr_door),
            ..cp_request
        })
    });
    // Taken on the declined path too, so no verdict is left in the slot.
    let verdict = if own_verdict_on {
        cratonvm_jit::ir_evidence::take_last_verdict()
    } else {
        None
    };
    drop(outer_verdict);
    let compiled = compiled?;
    // The pins leave WITH the artifact. They used to be dropped here, while
    // the body was still unpublished, so a bound callee could be superseded,
    // retired and its buffer recycled before the caller's `put` — refused by
    // `prepare_for_publication` under strict callee roots (a whole optimizing
    // compile thrown away), or published as a CALL into code nothing roots
    // with strict mode off. The optimizing OSR door never publishes through
    // `JitCache` at all, so nothing re-validated its baked calls.
    // As `RetainedCode`, so whichever holder releases the last pin does it
    // through the retirement queue rather than by a bare unmap.
    let callee_pins = bind_pins
        .borrow_mut()
        .drain(..)
        .map(cratonvm_jit::RetainedCode::new)
        .collect();
    // al16: a method-entry body that left a site to its first execution is
    // recompiled once that site has run (the OSR door has its own rule,
    // `AcceptedOsrOptimizing::first_call_cells_at_build`).
    if !osr_door {
        note_first_call_waiters(shared, &cp, cached);
        // JIT round 20 wave 17 (lane pStr3): and once a class it reads
        // statics of through the helper finishes initializing.
        note_static_init_waiters(shared, &compiled, cached, static_init_seq0);
    }
    Some(PinnedArtifact {
        compiled,
        callee_pins,
        verdict,
    })
}

/// Round 15 wave 2 (lane calls2, CR15-1 of `jit-r15-calls-proposals-RETIRED-20261003.md`,
/// `r15w1-calls-optimizing-door-callee-compile-publishes-stripped-bodies`):
/// the LOOKUP-ONLY direct binder of [`compile_optimizing_artifact`]'s in-place
/// callee compile. Answers the body already published for the statically
/// bound `callee_class.callee_method callee_desc`, named by a site of
/// `holder`, and NEVER compiles one, so a binder handed to that nested compile
/// (and to the inline resolvers it runs) cannot drive a further compile.
///
/// The refusal gates are the mutator door's `callee_compiler`, in its order:
/// FJP blocklist and native shadow ([`direct_bind_name_refusal`]), the
/// loader-faithful class ([`direct_bind_callee_class`]) and member access,
/// `synchronized` (only a published self-locking body), exception table / no
/// body, JVMS §5.5 static init ([`direct_bind_method_refusal`]), a debugger's
/// breakpoint, and an unconditional invokedynamic trap. The cache is probed by
/// the declaring class, then by the site's class, as both doors do. The caller
/// keeps the returned body alive until the compile that bakes its entry
/// publishes.
pub(in crate::runtime::interpreter::jit_bridge) fn published_direct_callee_bind(
    shared: &SharedVm,
    substituted: &SubstitutedOwners,
    holder: ClassId,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
    // JIT round 19 wave 2, lane eager2 (EAGER19-5): the method whose body
    // holds the site (the in-place callee being compiled), named on the trace
    // and remembered as a late-bind waiter on a refusal that may lift, under
    // [`optimizing_door_chain_align_enabled`]. `None` remembers nothing.
    waiting: Option<&CachedBytecodeMethod>,
) -> Option<std::sync::Arc<cratonvm_jit::CompiledMethod>> {
    // EAGER19-5: the background door's `dc_no!` for this binder (trace +
    // waiter), only under the switch; the tally is unconditional as before.
    let align = waiting.is_some() && optimizing_door_chain_align_enabled();
    let refused = |why: &str, reason: Option<cratonvm_jit::DirectBindRefusal>| {
        if let Some(reason) = reason {
            cratonvm_jit::note_direct_callee_bind_refusal(reason);
        }
        if !align {
            return;
        }
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] lookup-direct-call DECLINED {callee_class}.{callee_method}{callee_desc}: {why}"
            );
        }
        if let (Some(reason), Some(waiting)) = (reason, waiting) {
            note_late_bind_waiter(
                shared,
                reason,
                callee_class,
                callee_method,
                callee_desc,
                waiting,
            );
        }
    };
    if let Some(refusal) =
        direct_bind_name_refusal(shared, holder, callee_class, callee_method, callee_desc)
    {
        refused(direct_bind_refusal_name(refusal), Some(refusal));
        return None;
    }
    // `Err` carries the refusal, named and recorded after the class-manager
    // guard is dropped.
    let resolved: Result<_, (&str, Option<cratonvm_jit::DirectBindRefusal>)> = (|| {
        let cm = shared.classes.class_manager.read();
        let Some(bind_class) = direct_bind_callee_class(
            shared,
            &cm,
            substituted,
            holder,
            callee_class,
            callee_method,
            callee_desc,
        ) else {
            // Not tallied before EAGER19-5 either (the `?` this replaces).
            return Err((
                "callee-class-not-found",
                align.then_some(cratonvm_jit::DirectBindRefusal::CalleeClassNotFound),
            ));
        };
        let store = cm.class_store();
        let Some((method, declaring_id)) = crate::classloading::find_method_recursive(
            bind_class.id,
            callee_method,
            callee_desc,
            store,
        ) else {
            return Err(("callee-method-not-found", None));
        };
        if !bind_class.admits(method) {
            return Err(("substituted-owner-member-access", None));
        }
        let initialized = store
            .get(declaring_id)
            .map(crate::vm::is_class_initialized_fast)
            .unwrap_or(false);
        let self_locking_only = match direct_bind_method_refusal(method, initialized) {
            Some(cratonvm_jit::DirectBindRefusal::Synchronized)
                if self_locking_body_bind_candidate(method, initialized) =>
            {
                true
            }
            Some(refusal) => return Err((direct_bind_refusal_name(refusal), Some(refusal))),
            None => false,
        };
        let Some(declaring) = store.get(declaring_id) else {
            return Err(("callee-class-not-found", None));
        };
        Ok((
            bind_class.id,
            declaring_id,
            declaring.name.clone(),
            self_locking_only,
        ))
    })();
    let (callee_class_id, declaring_id, declaring_name, self_locking_only) = match resolved {
        Ok(found) => found,
        Err((why, reason)) => {
            refused(why, reason);
            return None;
        }
    };
    if super::breakpoint_bars_compiling(shared, declaring_id, callee_method, callee_desc) {
        refused("debugger-breakpoint", None);
        return None;
    }
    // JIT round 21 wave 11 (lane finalize): no bind of a finalizable class's
    // constructor that can throw before its `super(..)` (a body of it could
    // exist only from before the switch; none is built since).
    if super::finalizer_bars_compiling(shared, declaring_id, callee_method, callee_desc) {
        refused("finalizable-ctor-may-throw-before-init", None);
        return None;
    }
    let published = {
        let jit_cache = shared.jit.jit_cache.read();
        let mut published =
            jit_cache.get(&declaring_name, callee_method, callee_desc, declaring_id);
        if published.is_none() && declaring_id != callee_class_id {
            published = jit_cache.get(callee_class, callee_method, callee_desc, callee_class_id);
        }
        published
    };
    let Some(body) = published else {
        refused(
            "callee-not-yet-compiled",
            Some(cratonvm_jit::DirectBindRefusal::CalleeNotYetCompiled),
        );
        return None;
    };
    if self_locking_only && !is_self_locking_body(&body) {
        refused(
            direct_bind_refusal_name(cratonvm_jit::DirectBindRefusal::Synchronized),
            Some(cratonvm_jit::DirectBindRefusal::Synchronized),
        );
        return None;
    }
    if body.has_indy_trap {
        refused("indy-trap", Some(cratonvm_jit::DirectBindRefusal::IndyTrap));
        return None;
    }
    Some(body)
}

/// Give the optimizing door's nested static calls the bounded eager chain
/// already used by the ordinary callee door. A lookup-only miss otherwise
/// freezes dispatch into the caller, even after the leaf has compiled.
///
/// This plans a call only after the same native, loader, access, breakpoint
/// and initialized-class checks as a published bind. It runs no Java code and
/// retains no class-manager or code-cache lock across a nested compile.
pub(in crate::runtime::interpreter::jit_bridge) fn compile_unpublished_static_callee_bind(
    shared: &SharedVm,
    substituted: &SubstitutedOwners,
    holder: ClassId,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
    // EAGER19-5: handed through to the final [`published_direct_callee_bind`].
    waiting: Option<&CachedBytecodeMethod>,
) -> Option<std::sync::Arc<cratonvm_jit::CompiledMethod>> {
    if !crate::runtime::env_cache::jit_eager_callee_chain() {
        eager_chain_census_note_refused(shared, EAGER_DOOR_OPTIMIZING_STATIC, EAGER_RULE_CHAIN_OFF);
        return None;
    }
    if cratonvm_jit::jit_active_compile_depth() > MAX_EAGER_CALLEE_CHAIN_DEPTH {
        eager_chain_census_note_refused(shared, EAGER_DOOR_OPTIMIZING_STATIC, EAGER_RULE_DEPTH);
        return None;
    }
    if direct_bind_name_refusal(shared, holder, callee_class, callee_method, callee_desc).is_some()
    {
        return None;
    }
    let (callee_id, declaring_id, declaring_name) = {
        let cm = shared.classes.class_manager.read();
        let bind_class = direct_bind_callee_class(
            shared,
            &cm,
            substituted,
            holder,
            callee_class,
            callee_method,
            callee_desc,
        )?;
        let store = cm.class_store();
        let resolver = crate::runtime::resolve::MemberResolver::new(shared);
        let (declaring_id, method_index) = resolver
            .declared_method(
                &cm,
                resolver.scope(bind_class.id),
                callee_method,
                callee_desc,
            )
            .and_then(|method| resolver.adopt(method))
            .ok()?;
        let declaring = store.get(declaring_id)?;
        let method = declaring.methods.get(method_index as usize)?;
        if !method.is_static()
            || !bind_class.admits(method)
            || direct_bind_method_refusal(method, crate::vm::is_class_initialized_fast(declaring))
                .is_some()
        {
            return None;
        }
        (bind_class.id, declaring_id, declaring.name.clone())
    };
    // JIT round 18 wave 9 (lane eagercold): never compile a callee that has
    // never run. See [`eager_static_callee_needs_run`].
    if eager_static_callee_needs_run()
        && !eager_static_callee_has_run(shared, declaring_id, callee_method, callee_desc)
    {
        // A never-invoked static target is intentionally not compiled from its
        // caller. If it later runs and becomes bindable, let the bounded,
        // opt-in cold-callee waiter rebuild this optimizing caller once; a
        // spliced copy cannot be repaired by rebinding the target alone.
        if let Some(waiting) = waiting {
            note_cold_callee_late_bind_waiter(
                shared,
                callee_class,
                callee_method,
                callee_desc,
                waiting,
                true,
            );
        }
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] ir-direct-call eager-compile SKIPPED \
                 {declaring_name}.{callee_method}{callee_desc}: never invoked"
            );
        }
        eager_chain_census_note_refused(shared, EAGER_DOOR_OPTIMIZING_STATIC, EAGER_RULE_NEVER_RUN);
        return None;
    }
    // JIT round 19 wave 2, lane eager2 (EAGER19-5): the ordinary chain's
    // warmth gate (`CRATONVM_JIT_EAGER_CHAIN_MIN_INVOCATIONS`, with its
    // EAGERCOLD18-1 floor), on top of the has-run gate above, so a callee
    // with 1..floor-1 calls is refused by both doors alike. Not a late-bind
    // waiter, as at the ordinary chain (a cold callee is not "not yet").
    if optimizing_door_chain_align_enabled()
        && !eager_callee_chain_is_warm(shared, declaring_id, callee_method, callee_desc)
    {
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] ir-direct-call eager-compile SKIPPED \
                 {declaring_name}.{callee_method}{callee_desc}: eager-callee-chain-cold"
            );
        }
        // CF-W8-2 (`CRATONVM_JIT_LATE_BIND_COLD_CALLEE`): this door compiles
        // for the optimizing tier only, so its caller may wait, once.
        if let Some(waiting) = waiting {
            note_cold_callee_late_bind_waiter(
                shared,
                callee_class,
                callee_method,
                callee_desc,
                waiting,
                true,
            );
        }
        cratonvm_jit::note_direct_callee_bind_refusal(
            cratonvm_jit::DirectBindRefusal::EagerChainCold,
        );
        eager_chain_census_note_refused(shared, EAGER_DOOR_OPTIMIZING_STATIC, EAGER_RULE_COLD);
        return None;
    }
    // One `if` before JIT round 19 wave 3 (lane eager3); split so the census
    // names the rule, in the same order.
    if super::breakpoint_bars_compiling(shared, declaring_id, callee_method, callee_desc) {
        eager_chain_census_note_refused(
            shared,
            EAGER_DOOR_OPTIMIZING_STATIC,
            EAGER_RULE_BREAKPOINT,
        );
        return None;
    }
    if cratonvm_jit::jit_active_compile_contains(&declaring_name, callee_method, callee_desc) {
        eager_chain_census_note_refused(shared, EAGER_DOOR_OPTIMIZING_STATIC, EAGER_RULE_CYCLE);
        return None;
    }
    if !eager_callee_chain_try_spend() {
        eager_chain_census_note_refused(shared, EAGER_DOOR_OPTIMIZING_STATIC, EAGER_RULE_BUDGET);
        return None;
    }
    // JIT round 19, lane eager19 (EAGERCOLD18-2): a thin-profile callee at C1.
    let optimize =
        eager_callee_compile_optimize(shared, declaring_id, callee_method, callee_desc, true);
    // JIT round 19 wave 2, lane eager2 (EAGER19-2): see [`eager_callee_compile_key`].
    let (compile_id, compile_class) =
        eager_callee_compile_key(callee_id, callee_class, declaring_id, &declaring_name);
    let eager = try_jit_compile_callee_for_class(
        shared,
        compile_id,
        compile_class,
        callee_method,
        callee_desc,
        optimize,
    );
    eager_chain_census_note_compiled(
        shared,
        EAGER_DOOR_OPTIMIZING_STATIC,
        declaring_id,
        callee_method,
        callee_desc,
        eager.as_ref().map(|(body, _, _)| body),
    );
    if let Some((body, _, _)) = eager {
        note_callee_publish_to_tiered_manager(
            shared,
            compile_id,
            compile_class,
            callee_method,
            callee_desc,
            &body,
        );
    }
    // Reapply the full bind gates after compilation or a racing publication;
    // the returned owner, rather than a raw entry, is pinned by the caller.
    published_direct_callee_bind(
        shared,
        substituted,
        holder,
        callee_class,
        callee_method,
        callee_desc,
        waiting,
    )
}

/// `CRATONVM_JIT_EAGER_STATIC_CALLEE_NEEDS_RUN` (JIT round 18 wave 9, lane
/// eagercold; **default ON**, `0` restores d7d3a79658's unconditional compile):
/// [`compile_unpublished_static_callee_bind`] compiles only a static callee
/// that has run at least once ([`eager_static_callee_has_run`]).
///
/// Why: d7d3a79658 gave the optimizing door's nested static calls the eager
/// callee chain. An optimizing compile of a method whose callees have never
/// run (an OSR compile of `main`'s set-up loop, say) then compiled them, and
/// each of those compiles runs the ordinary door's own chain, so a whole call
/// tree was compiled before any of it ran. Every class such a tree
/// instantiates is still uninitialized, so each constructor splice is refused
/// (`ctor-class-not-initialized-or-finalizable`, [`resolve_inline_site_from`])
/// and the `new` stays a `jit_new_object` plus a blind dispatch for the life
/// of the body: `R16AllocPremium alloc-dead` went from 1.50 to ~500 ns/op
/// (page `r18-eager-static-callee-chain-compiles-uninitialized-constructors-blind-FIXED-20261006.md`).
/// A callee that has never run is one HotSpot's C2 would not compile either
/// (it has no profile; C2 inlines it or leaves a call). The site keeps the
/// dispatch helper, which is what it had before d7d3a79658; the callee is
/// compiled by its own tiering once it runs. A callee that HAS run keeps
/// d7d3a79658's bounded compile. The ordinary door's chain (734dd4035a,
/// `CompileOrderProbe`) is not touched. Read per eager static-callee compile
/// (cold: a compile-time miss).
pub(in crate::runtime::interpreter::jit_bridge) fn eager_static_callee_needs_run() -> bool {
    true
}

/// `CRATONVM_JIT_OPTIMIZING_DOOR_CHAIN_ALIGN` (JIT round 19 wave 2, lane
/// eager2, EAGER19-5 of `jit-r19-eager19-proposals.md`; **default on**): the
/// optimizing door's in-place callee binder behaves like the ordinary chain's
/// (`direct_callee_lookup_bound` in [`try_jit_compile_callee_slow`]):
///
/// * [`compile_unpublished_static_callee_bind`] asks the same warmth gate
///   ([`eager_callee_chain_is_warm`], `MIN_INVOCATIONS` with the EAGERCOLD18-1
///   floor) besides its own has-run gate, so a callee with 1..30 calls is no
///   longer compiled by one door and refused by the other;
/// * [`published_direct_callee_bind`] names each refusal on the
///   `CRATONVM_DBG_JITC` trace (`lookup-direct-call DECLINED`) and remembers
///   the in-place callee as a late-bind waiter on a refusal that may lift
///   (callee class not found, static callee's class not initialized:
///   [`late_bind_refusal_may_lift`]), as the background door's `dc_no!` does.
///   That callee is a METHOD-ENTRY compile, so the late-bind recompile
///   (which compiles the method entry) is the right repair whichever door
///   the outer compile was.
///
/// Read per eager static-callee compile and per lookup-only bind (cold).
pub(in crate::runtime::interpreter::jit_bridge) fn optimizing_door_chain_align_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_OPTIMIZING_DOOR_CHAIN_BUDGET` (JIT round 18 wave 9, lane
/// eagercold; **default OFF** until measured): does a TOP-LEVEL
/// [`compile_optimizing_artifact`] (the method-entry upgrade and the optimizing
/// OSR door) start a fresh eager-chain fan-out budget
/// ([`eager_callee_chain_enter_top_level`])? Without it the budget its nested
/// [`compile_unpublished_static_callee_bind`] compiles spend is whatever the
/// last `try_jit_compile_callee_slow` / sync-direct top-level compile on the
/// same thread left: anywhere from the full 96 to none, depending on which
/// compile that thread ran before -- the "per top-level compile" bound is
/// neither per compile nor deterministic for this door (page
/// `r18-optimizing-door-eager-chain-budget-is-inherited-from-the-previous-compile-CLOSED-20261006.md`).
/// Read once per optimizing compile.
pub(in crate::runtime::interpreter::jit_bridge) fn optimizing_door_chain_budget_enabled() -> bool {
    false
}

/// `CRATONVM_JIT_EAGER_CALLEE_THIN_PROFILE_C1` (JIT round 19, lane eager19,
/// EAGERCOLD18-2 of `docs/feature-designs/jit-ir-optimizations.md`; **default on**): see
/// [`eager_callee_compile_optimize`]. Read per eager callee compile (cold).
pub(in crate::runtime::interpreter::jit_bridge) fn eager_callee_thin_profile_c1_enabled() -> bool {
    true
}

/// The backend an eager-chain callee compile uses: `caller_optimize` (the
/// caller's own tier for the ordinary chain, always C2 for the optimizing
/// door's nested static callee, d7d3a79658), except that under
/// [`eager_callee_thin_profile_c1_enabled`] a callee whose profile-store
/// invocation count is below the tiered policy's `c2_min_invocations` (the
/// fewest calls any C2 door accepts; 1 000 by default) is compiled single-pass.
///
/// Why: a C2 body compiled for a callee that has barely run has no profile to
/// speculate on, and nothing re-offers a C2 body (the tiered manager records
/// it as C2 through `note_callee_publish_to_tiered_manager`). A C1 body is
/// recorded as C1, so the manager's ordinary C1 -> C2 upgrade compiles the
/// callee again once it is hot, with a profile and its classes initialized.
/// The callers that baked a direct CALL to the C1 body are then forwarded by
/// the C2 supersede's not-entrant stub (`supersede_retire_superseded`, hm5,
/// default ON) -- but only when the C2 body is published under the SAME key as
/// the C1 one; see the book `jit-r19-eager19-proposals.md` (EAGER19-2) for the
/// site-class-key case. Pair with `CRATONVM_JIT_SUPERSEDE_FORWARD_EVERY_DOOR`.
pub(in crate::runtime::interpreter::jit_bridge) fn eager_callee_compile_optimize(
    shared: &SharedVm,
    declaring_id: ClassId,
    callee_method: &str,
    callee_desc: &str,
    caller_optimize: bool,
) -> bool {
    if !caller_optimize || !eager_callee_thin_profile_c1_enabled() {
        return caller_optimize;
    }
    let bar = shared.jit.tiered_manager.policy().c2_min_invocations;
    let key = cratonvm_jit_api::invoc_key_parts(declaring_id.as_u32(), callee_method, callee_desc);
    let thick = shared.jit.profile_store.invocation_count(key) >= bar;
    if !thick && crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] eager-callee THIN-PROFILE single-pass {callee_method}{callee_desc} \
             (count < c2_min_invocations {bar})"
        );
    }
    thick
}

/// `CRATONVM_JIT_EAGER_CALLEE_DECLARING_KEY` (JIT round 19 wave 2, lane
/// eager2, EAGER19-2 of `jit-r19-eager19-proposals.md`; **default on**): see
/// [`eager_callee_compile_key`]. Read per eager callee compile (cold).
pub(in crate::runtime::interpreter::jit_bridge) fn eager_callee_declaring_key_enabled() -> bool {
    true
}

/// The class an eager-chain callee compile is asked for and reported to the
/// tiered manager under: the SITE's class `(site_id, site_class)` as before,
/// or, under [`eager_callee_declaring_key_enabled`], the class that DECLARES
/// the resolved method `(declaring_id, declaring_class)`.
///
/// Why: the by-name door publishes under the class it is asked for, and the
/// interpreter counts and the tiered manager tiers the DECLARING method. For
/// an inherited callee (`invokevirtual Sub.f` resolving to a final `A.f`,
/// `invokestatic Sub.s` to `A.s`) the eager body lived under `Sub.f`, a key
/// the manager never upgrades: its own C2 body landed under `A.f`, nothing
/// superseded the `Sub.f` body, and hm5 never forwarded the callers that baked
/// it (they stayed on that body for good, at C1 under
/// `CRATONVM_JIT_EAGER_CALLEE_THIN_PROFILE_C1`); the manager also compiled
/// `A.f` a second time at its own threshold. Both binders probe the declaring
/// key first and the site's second, so the bind reads the body either way,
/// and the site-keyed read-back of the slow door is untouched for every other
/// door. The declaring id is loader-faithful (it is the class
/// `find_method_recursive` walked to from the site's own class).
pub(in crate::runtime::interpreter::jit_bridge) fn eager_callee_compile_key<'k>(
    site_id: ClassId,
    site_class: &'k str,
    declaring_id: ClassId,
    declaring_class: &'k str,
) -> (ClassId, &'k str) {
    if declaring_id != site_id && declaring_id.as_u32() != 0 && eager_callee_declaring_key_enabled()
    {
        (declaring_id, declaring_class)
    } else {
        (site_id, site_class)
    }
}

/// Has the method `declaring_id.callee_method callee_desc` been invoked (or
/// credited loop work) at least once, by the profile store's invocation
/// counter (`ProfileStore::invocation_count`, keyed by
/// `cratonvm_jit_api::invoc_key_parts` over the DECLARING class, the key every
/// counting door uses)? Read-only: asking never counts.
pub(in crate::runtime::interpreter::jit_bridge) fn eager_static_callee_has_run(
    shared: &SharedVm,
    declaring_id: ClassId,
    callee_method: &str,
    callee_desc: &str,
) -> bool {
    let key = cratonvm_jit_api::invoc_key_parts(declaring_id.as_u32(), callee_method, callee_desc);
    shared.jit.profile_store.invocation_count(key) > 0
}

/// `CRATONVM_JIT_IR_SPLICE_NEVER_RUN_STATIC_EXIT` (nio21 wave 6, lane L4) --
/// **default ON**, `=0` restores the refusal. In an optimizing-tier splice, a
/// static call the direct binder left without an entry ONLY because its
/// callee has never run (`eager_static_callee_needs_run`: the eager chain
/// never compiles a method nothing has invoked) becomes the precise splice
/// exit an unloaded callee already gets (`unloaded_static_call_pcs`:
/// `IrBuilder`'s invoke arm plants an always-failing guard resumed through the
/// chain snapshot at that call, `DeoptReason::ClassLoading`, which recompiles).
/// Before, such a row made the whole body unbindable
/// (`ir-splice-unbindable-surviving-call`).
///
/// The measured shape is `String$CaseInsensitiveComparator.compare(String,
/// String)`: four static arms by coder, of which a Latin-1 workload runs one;
/// `StringUTF16.compareToCI`, `compareToCI_Latin1` and
/// `StringLatin1.compareToCI_UTF16` never run, are never compiled, and kept
/// the 68-byte body out of every caller (`TreeMap.put`'s comparator splice).
/// `String.regionMatches(ZILString;II)Z` has the same four arms. HotSpot's C2
/// plants an uncommon trap at a call its profile never reached; this is that,
/// with the callee's invocation counter as the profile.
///
/// Only for a callee the VM would otherwise compile and bind: loaded, static,
/// plain bytecode, no registered native over it (whose calls the counter may
/// not see), and only when the spliced body itself or the method being
/// compiled has run, so a counter that is not counting cannot turn every arm
/// into a trap. A trap that fires runs
/// the callee in the interpreter, which counts it; the recompile binds it.
/// Read per resolution.
pub(in crate::runtime::interpreter::jit_bridge) fn splice_never_run_static_exit_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_SPLICE_UNBINDABLE_NESTED_TO_CALL` (nio21 wave 6, lane L4)
/// -- **default ON**, `=0` keeps every nested body. In IR mode, a statically
/// bound nested body the planner will refuse for an unbindable call of its
/// own (`cratonvm_jit::ir_splice_body_has_unbindable_call`) is dropped before
/// the enclosing body's binds, so its call is direct-bound like any other.
/// Before, the binder skipped the nested pc, the planner refused the nested
/// body, and the pc it left -- statically bound, no entry -- refused the
/// enclosing body as well: `StringLatin1.compareToCI` (its
/// `Character.toLowerCase` natives) kept
/// `CaseInsensitiveComparator.compare(String,String)` out of every caller. The
/// nested body was never going to be spliced, so only a call changes: an
/// unbound one becomes a bound one. Read per resolution.
pub(in crate::runtime::interpreter::jit_bridge) fn splice_unbindable_nested_to_call_enabled() -> bool
{
    true
}

fn resolve_static_final_prim_for_jit(
    vm: &SharedVm,
    class_id: u32,
    field_index: u32,
    type_tag: u8,
) -> Option<i64> {
    if matches!(type_tag, b'L' | b'[') {
        return None;
    }
    let cid = ClassId::new(class_id);
    if !crate::vm::is_class_initialized_via_manager(vm, cid) {
        return None;
    }
    {
        let cm = vm.classes.class_manager.read();
        let class = cm.get_class(cid)?;
        if !crate::jit::helpers::final_array_holder_admitted(
            cm.get_loader_id(cid),
            &class.origin,
            &class.name,
            class.version.major,
        ) {
            return None;
        }
        let field = class
            .fields
            .iter()
            .filter(|f| f.is_static())
            .nth(field_index as usize)?;
        if !field.is_final()
            || field.is_volatile()
            || field.descriptor.as_bytes().first().copied() != Some(type_tag)
        {
            return None;
        }
    }
    match (
        type_tag,
        crate::vm::get_static_shared(vm, cid, field_index as usize),
    ) {
        (b'Z', Value::Int(v)) => Some(i64::from(v != 0)),
        (b'B', Value::Int(v)) => Some(i64::from(v as i8)),
        (b'S', Value::Int(v)) => Some(i64::from(v as i16)),
        (b'C', Value::Int(v)) => Some(i64::from(v as u16)),
        (b'I', Value::Int(v)) => Some(i64::from(v)),
        (b'J', Value::Long(v)) => Some(v),
        (b'F', Value::Float(v)) => Some(i64::from(v.to_bits())),
        (b'D', Value::Double(v)) => Some(v.to_bits() as i64),
        _ => None,
    }
}
