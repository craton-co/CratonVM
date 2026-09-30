// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Method invocation: resolution, dispatch, and the native bridge.
//!
//! Moved verbatim out of `interpreter.rs`'s `Helper: Method invocation`
//! section. Lint levels declared at the parent module level (including
//! its no-panic `deny` gate, where it has one) are inherited here.

use super::*;

pub(super) fn execute_invoke(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    is_special: bool,
    pc: usize,
) -> Result<CachedCallResult, MethodCallFailed> {
    execute_invoke_kind(shared, thread, frame_idx, cp_index, is_special, false, pc)
}

/// JEP 358 — a [`CpResolver`] backed by the live constant pool of
/// `current_class_id`. Acquires `class_manager.read_recursive()` per query; the
/// whole helpful-NPE analysis runs once per thrown NPE (cold path), so the
/// per-call lock cost is irrelevant.
///
/// [`CpResolver`]: crate::runtime::exceptions::helpful_npe::CpResolver
pub(super) struct CpPoolResolver<'a> {
    pub(super) shared: &'a SharedVm,
    pub(super) class_id: ClassId,
    /// Method identity needed to locate the `LocalVariableTable` for real
    /// local-name resolution (increment 2). When these are empty the
    /// resolver still works — `local_name` just falls back to `<localN>`.
    pub(super) method_name: &'a str,
    pub(super) method_descriptor: &'a str,
}

impl crate::runtime::exceptions::helpful_npe::CpResolver for CpPoolResolver<'_> {
    fn field_ref(&self, cp_index: u16) -> Option<crate::runtime::exceptions::helpful_npe::CpRef> {
        use crate::runtime::exceptions::helpful_npe::CpRef;
        let cm = self.shared.classes.class_manager.read_recursive();
        let class = cm.get_class(self.class_id)?;
        if let Some(ConstantPoolEntry::FieldReference {
            class_index,
            name_and_type_index,
        }) = class.constant_pool.get(cp_index)
        {
            let owner = class
                .constant_pool
                .get_class_name(*class_index)?
                .to_string();
            let (name, _) = class
                .constant_pool
                .get_name_and_type(*name_and_type_index)?;
            Some(CpRef::Field {
                owner_internal: owner,
                name: name.to_string(),
            })
        } else {
            None
        }
    }

    fn method_ref(&self, cp_index: u16) -> Option<crate::runtime::exceptions::helpful_npe::CpRef> {
        use crate::runtime::exceptions::helpful_npe::CpRef;
        let cm = self.shared.classes.class_manager.read_recursive();
        let class = cm.get_class(self.class_id)?;
        let (class_index, nat_index) = match class.constant_pool.get(cp_index) {
            Some(ConstantPoolEntry::MethodReference {
                class_index,
                name_and_type_index,
            })
            | Some(ConstantPoolEntry::InterfaceMethodReference {
                class_index,
                name_and_type_index,
            }) => (*class_index, *name_and_type_index),
            _ => return None,
        };
        let owner = class.constant_pool.get_class_name(class_index)?.to_string();
        let (name, desc) = class.constant_pool.get_name_and_type(nat_index)?;
        Some(CpRef::Method {
            owner_internal: owner,
            name: name.to_string(),
            descriptor: desc.to_string(),
        })
    }

    /// JEP 358 step 4 — real `LocalVariableTable` name resolution. Look up
    /// the source name of local slot `slot` live at byte-offset `bci` by
    /// scanning the trapping method's `Code.LocalVariableTable` attribute
    /// (`[start_pc, start_pc+length)` is the live range, JVMS §4.7.13). The
    /// name is a Utf8 CP entry referenced by `name_index`. Returns `None`
    /// when the method has no LVT (the common stripped-debug-info case),
    /// which makes the analysis fall back to the synthetic `<localN>` /
    /// `this` spelling.
    fn local_name(&self, slot: u16, bci: usize) -> Option<String> {
        use cratonvm_reader::attribute::Attribute;
        let cm = self.shared.classes.class_manager.read_recursive();
        let class = cm.get_class(self.class_id)?;
        let method = class.find_method(self.method_name, self.method_descriptor)?;
        let code = method.code()?;
        // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
        let bci_u16 = bci.min(u16::MAX as usize) as u16;
        for attr in &code.attributes {
            if let Attribute::LocalVariableTable(entries) = attr {
                for e in entries {
                    // Live range is `[start_pc, start_pc + length)`. A slot is
                    // matched only when the trapping bci falls inside it, so a
                    // re-used slot resolves to the correct variable name.
                    let end = e.start_pc.saturating_add(e.length);
                    if e.index == slot && bci_u16 >= e.start_pc && bci_u16 < end {
                        if let Some(name) = class.constant_pool.get_utf8(e.name_index) {
                            return Some(name.to_string());
                        }
                    }
                }
            }
        }
        None
    }

    /// Whether the trapping method is `static` — drives slot-0 naming
    /// (`<local0>` in a static method vs `this` in an instance method). Looked
    /// up from the method's access flags; a method we can't resolve defaults to
    /// instance (legacy `this` spelling), matching the trait default.
    fn method_descriptor(&self) -> Option<&str> {
        (!self.method_descriptor.is_empty()).then_some(self.method_descriptor)
    }

    fn is_static_method(&self) -> bool {
        let cm = self.shared.classes.class_manager.read_recursive();
        let Some(class) = cm.get_class(self.class_id) else {
            return false;
        };
        match class.find_method(self.method_name, self.method_descriptor) {
            Some(method) => method.is_static(),
            None => false,
        }
    }

    /// Whether the field ref at `cp_index` is a `long` / `double` (interpreter
    /// round i1 wave 22, lane L7: the whole-method analysis sizes the
    /// category-dependent shuffles with it).
    fn field_is_category2(&self, cp_index: u16) -> Option<bool> {
        let cm = self.shared.classes.class_manager.read_recursive();
        let class = cm.get_class(self.class_id)?;
        let Some(ConstantPoolEntry::FieldReference {
            name_and_type_index,
            ..
        }) = class.constant_pool.get(cp_index)
        else {
            return None;
        };
        let (_, desc) = class
            .constant_pool
            .get_name_and_type(*name_and_type_index)?;
        Some(matches!(desc.as_bytes().first(), Some(b'J' | b'D')))
    }

    /// The call-site descriptor of the `invokedynamic` at `cp_index` (wave 22):
    /// what sizes a string concatenation or a lambda capture in the analysis.
    fn invokedynamic_descriptor(&self, cp_index: u16) -> Option<String> {
        let cm = self.shared.classes.class_manager.read_recursive();
        let class = cm.get_class(self.class_id)?;
        let Some(ConstantPoolEntry::InvokeDynamic {
            name_and_type_index,
            ..
        }) = class.constant_pool.get(cp_index)
        else {
            return None;
        };
        let (_, desc) = class
            .constant_pool
            .get_name_and_type(*name_and_type_index)?;
        Some(desc.to_string())
    }

    /// The trapping method's exception-handler bcis (wave 22), read from the
    /// declaring class's current `Code` attribute, as `local_name` reads the
    /// `LocalVariableTable`. (A frame running a body a redefinition replaced
    /// is analysed against the current body's handlers; its message loses at
    /// most its `because` clause.)
    fn handler_pcs(&self) -> Vec<usize> {
        let cm = self.shared.classes.class_manager.read_recursive();
        let Some(class) = cm.get_class(self.class_id) else {
            return Vec::new();
        };
        let Some(code) = class
            .find_method(self.method_name, self.method_descriptor)
            .and_then(|m| m.code())
        else {
            return Vec::new();
        };
        code.exception_table
            .iter()
            .map(|e| usize::from(e.handler_pc))
            .collect()
    }
}

/// JEP 358 — synthesize the HotSpot-style extended message for a null-receiver
/// `invoke*` at the current bci. Always returns at least the action half
/// (`Cannot invoke "Owner.name(params)"`); appends `because "<expr>" is null`
/// when the bounded backward analysis can name the null expression.
pub(super) fn helpful_npe_invoke_message(
    shared: &SharedVm,
    thread: &JvmThread,
    frame_idx: usize,
    owner_internal: &str,
    method_name: &str,
    method_descriptor: &str,
    num_params: usize,
) -> String {
    use crate::runtime::exceptions::helpful_npe;
    // `-XX:-ShowCodeDetailsInExceptionMessages`: HotSpot's `getMessage()` is
    // null. The throw site needs a `String`, so hand back the empty marker that
    // `throw_runtime_error` maps to `None` (see its NPE arm).
    if crate::runtime::env_cache::helpful_npe_suppressed() {
        return String::new();
    }
    let action = helpful_npe::action_invoke(owner_internal, method_name, method_descriptor);
    let frame = &thread.frames[frame_idx];
    // `last_instr_pc` is set by the dispatch loop to the bci of the opcode
    // currently executing (here, the trapping invoke) — `pc` may already be
    // advanced. This is the deopt-independent trapping bci the design doc
    // relies on for the interpreter path.
    let invoke_bci = frame.last_instr_pc;
    let code = Arc::clone(&frame.code);
    let m_name = frame.method_name_arc();
    let m_desc = frame.method_descriptor_arc();
    let resolver = CpPoolResolver {
        shared,
        class_id: frame.class_id,
        method_name: &m_name,
        method_descriptor: &m_desc,
    };
    let expr = helpful_npe::null_expr_for_invoke_receiver(&code, invoke_bci, num_params, &resolver);
    helpful_npe::combine(&action, expr.as_ref())
}

/// JEP 358 increment 2 — synthesize the HotSpot-style extended message for a
/// non-invoke null-deref opcode (`getfield`/`putfield`, `arraylength`, the
/// array load/store family, `monitorenter`/`monitorexit`, `athrow`). The
/// caller supplies the already-built action half (e.g.
/// `Cannot read field "x"`) and the operand's depth below the top of the
/// operand stack as it stood just before the trapping opcode. Appends
/// `because "<expr>" is null` when the bounded backward analysis can name the
/// null operand; otherwise emits the action half alone (HotSpot omits the
/// `because` clause rather than fabricating one).
///
/// Gated by `env_cache::helpful_npe_opcodes()` at the call sites; the trapping
/// bci is `last_instr_pc`, always known in the interpreter (deopt-independent).
pub(super) fn helpful_npe_opcode_message(
    shared: &SharedVm,
    thread: &JvmThread,
    frame_idx: usize,
    action: &str,
    depth_below_top: usize,
) -> String {
    let frame = &thread.frames[frame_idx];
    helpful_npe_opcode_message_parts(
        shared,
        frame.class_id,
        &frame.code,
        frame.method_name_arc_ref(),
        frame.method_descriptor_arc_ref(),
        frame.last_instr_pc,
        action,
        depth_below_top,
    )
}

/// `thread`-free core of [`helpful_npe_opcode_message`]: builds the message
/// from the trapping frame's already-extracted parts. Kept separate so the
/// opcode call sites can invoke it from inside a `pop_object_ref_ctx_with`
/// closure (which holds a `&mut` borrow of the frame's operand stack and so
/// cannot also borrow `thread`). The bytecode analysis only reads `shared`
/// (for CP / LVT lookups) and the supplied `code` slice, never `thread`.
#[allow(clippy::too_many_arguments)]
pub(super) fn helpful_npe_opcode_message_parts(
    shared: &SharedVm,
    class_id: ClassId,
    code: &[u8],
    method_name: &str,
    method_descriptor: &str,
    trap_bci: usize,
    action: &str,
    depth_below_top: usize,
) -> String {
    use crate::runtime::exceptions::helpful_npe;
    // See `helpful_npe_invoke_message`: an explicit opt-out yields the empty
    // marker, which `throw_runtime_error` turns into a null `getMessage()`.
    if crate::runtime::env_cache::helpful_npe_suppressed() {
        return String::new();
    }
    let resolver = CpPoolResolver {
        shared,
        class_id,
        method_name,
        method_descriptor,
    };
    let expr = helpful_npe::null_expr_at_depth(code, trap_bci, depth_below_top, &resolver);
    helpful_npe::combine_opt(action, expr.as_ref())
}

/// Does an `invokevirtual` (0xb6) site name a **private** method?
///
/// JVMS 5.4.6 selects a private method as the one the constant pool resolved,
/// with no override lookup at all. javac has emitted `invokevirtual` for a call
/// to a private instance method since Java 11 (JEP 181 nestmates), where it
/// used to emit `invokespecial` — the opcode changed, the semantics did not.
///
/// Every compiled dispatcher resolves an `invoke_kind == 0` site by walking up
/// from the RECEIVER's class, so on such a site it finds the most-derived
/// same-named private method and calls THAT. The shape is ordinary — a class
/// whose constructor calls its own `private void init()`, subclassed by a class
/// that does the same — and `io/vertx/core/net/TCPSSLOptions`,
/// `ClientOptionsBase` and `HttpClientOptions` are three such levels in one
/// chain. The interpreter has pinned these correctly since
/// [`resolved_private_invokevirtual_target`] below; the compile doors had no
/// equivalent, and each classifies invoke sites for itself.
///
/// This is the COMPILE-TIME form of that question, answered while the caller
/// already holds the class-manager read guard. A `true` means the site must be
/// classified as a direct, non-dispatching bind (`invoke_kind == 1`) rather
/// than as virtual dispatch.
///
/// The rule itself lives in
/// [`crate::classloading::invokevirtual_private_declaring_class`], beside its
/// `invokespecial` twin; this is the name-and-loader-resolving wrapper the
/// interpreter side wants. [`crate::runtime::interpreter::jit_bridge`]'s three
/// compile doors call the same rule for the declaring class NAME.
pub(crate) fn invokevirtual_site_targets_private(
    cm: &crate::classloading::ClassManager,
    current_class_id: ClassId,
    target_class: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    let Some(cp_class_id) = cm.find_class_by_name_for_class(target_class, current_class_id) else {
        return false;
    };
    crate::classloading::invokevirtual_private_declaring_class(
        cp_class_id,
        method_name,
        descriptor,
        cm.class_store(),
    )
    .is_some()
}

/// Does an `invokevirtual` (0xb6) site resolve to a method **no subclass can
/// override**? Returns the class that declares it.
///
/// A `final` method, or any method of a `final` class, has exactly one possible
/// target at every site that names it: the verifier guarantees the receiver is
/// an instance of the constant pool's class, and `final` forbids the override
/// that would make selection differ from resolution. So the site is statically
/// bound in the same sense `invokestatic` is — and, unlike class-hierarchy
/// speculation, it needs NO invalidation dependency, because no class that
/// could ever be loaded may override it. That is the whole reason this rule is
/// separate from a guarded monomorphic bind rather than a weaker case of it.
///
/// Measured on netty's `ByteBuf` accessor chain, which is what motivated it. A
/// single `getByte(int)` on a pooled buffer runs
///
/// ```text
/// getByte -> checkIndex -> checkIndex(int,int) -> ensureAccessible
///                                              -> checkIndex0 -> capacity
///         -> _getByte -> idx
/// ```
///
/// and `CRATONVM_DBG_JITC=1` reported EVERY link as `ir-direct-call MISSED …
/// static=false special=false`: nine generic dispatches for one byte, against
/// the two instructions HotSpot inlines it to. Five of those links —
/// `checkIndex(int)`, `checkIndex(int,int)`, `checkIndex0`, `ensureAccessible`
/// and `PooledByteBuf.idx` — are declared `final`, and were being dispatched
/// virtually only because no door had ever asked.
///
/// Deliberately NARROWER than the letter of the rule in two places:
///
///  * `native` targets are excluded. A registered native has no compiled body
///    to bind to, and the three doors already route natives through the
///    native-shadow and thin-helper machinery; admitting them here would put a
///    second classification in front of that one for no gain.
///  * `abstract` and `static` are excluded as impossible-by-construction rather
///    than trusted not to occur (a `final abstract` method is illegal, and
///    `invokevirtual` never names a `static` one) — a malformed classfile must
///    fall back to dispatch, not bind.
///
/// Returns the DECLARING class, name and id, which the caller must substitute
/// for the constant pool's class name before any direct bind: the CP entry
/// commonly names a subclass (`PooledHeapByteBuf.checkIndex`) while the body
/// lives on the ancestor that declares it (`AbstractByteBuf`), and binding
/// under the subclass name would key the compiled callee under a method that
/// class does not declare. The id is what the direct binder uses: the
/// declaring class need not be accessible to, nor findable by name from, the
/// caller (`StringBuilder.length()` is `AbstractStringBuilder`'s, a
/// package-private class; i12-L2).
///
/// The constant-pool class is resolved through `jit_known_class`, like every
/// compile-time class lookup: `current_class_id`'s loader, and JVMS §5.4.4
/// access with it as the accessor. A class the caller may not access keeps
/// its site on dispatch, whose resolution raises the `IllegalAccessError`.
///
/// `CRATONVM_JIT_FINAL_DEVIRT=0` turns this off; the counter is
/// [`cratonvm_jit::FINAL_INVOKEVIRTUAL_PINNED`].
pub(crate) fn invokevirtual_site_final_owner(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    current_class_id: ClassId,
    target_class: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<(String, ClassId)> {
    if !crate::runtime::env_cache::jit_final_devirt() {
        return None;
    }
    let cp_class_id =
        super::jit_bridge::jit_known_class(shared, cm, target_class, current_class_id)?;
    let store = cm.class_store();
    // The selection rule itself lives beside its two siblings in
    // `classloading` — see `invokevirtual_final_declaring_class`. What stays
    // here is this door's POLICY: the kill switch above, the native screen
    // below, and the engagement counter.
    let declaring_id = crate::classloading::invokevirtual_final_declaring_class(
        cp_class_id,
        method_name,
        descriptor,
        store,
    )?;
    let owner = store.get(declaring_id).map(|c| c.name.to_string())?;
    if final_devirt_native_shadow(
        shared,
        cm,
        cp_class_id,
        declaring_id,
        method_name,
        descriptor,
    ) {
        cratonvm_jit::FINAL_DEVIRT_NATIVE_SHADOW_REFUSED
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] final-devirt REFUSED {target_class}.{method_name}{descriptor} \
                 (declared on {owner}): a registered native shadows it for some receiver"
            );
        }
        return None;
    }
    cratonvm_jit::FINAL_INVOKEVIRTUAL_PINNED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Some((owner, declaring_id))
}

/// Would a registered native shadow the body this `final` site is about to be
/// bound to, for some receiver the site can see?
///
/// `final` is a promise about BYTECODE: no subclass may declare another body.
/// It says nothing about CratonVM's native registry, which shadows a JDK method
/// by registering on a class name — very often a SUBCLASS of the one that
/// declares the body. `invoke_or_native` honours that by walking the RECEIVER's
/// superclass chain; a devirtualised site has thrown the receiver away and
/// calls the classfile body directly. So the interpreter and compiled code
/// answer the same call differently, and only after tier-up.
///
/// Measured 2026-09-05, `probes/ChanStateCensus3.java` and
/// `probes/CloseDevirtProbe.java`, both in netty's own shape (a small
/// delegating method: `AbstractNioChannel.isOpen()` is `return ch.isOpen()`,
/// `NioDatagramChannel.doClose()` is `javaChannel().close()`), against
/// `sun.nio.ch.SocketChannelImpl` / `DatagramChannelImpl` receivers whose
/// `isOpen`/`close` natives are registered on
/// `java/nio/channels/{Socket,ServerSocket,Datagram}Channel`:
///
///  * `isOpen()Z` — declared `public final` on
///    `java/nio/channels/spi/AbstractInterruptibleChannel` as `return !closed`.
///    Compiled callers read the raw field and answered OPEN on a closed
///    channel from the tier-up call onward: 397,739 of 400,000, first wrong at
///    call 2,261.
///  * `close()V` — declared `public final` on the same class, opening with
///    `synchronized (closeLock)`. CratonVM's datagram factory never runs the
///    JDK constructor that assigns `closeLock`, so the compiled body threw
///    `NullPointerException` and left the socket open: 3,487 of 4,000 closes,
///    first at call 512, against 0 of 20,000 on HotSpot.
///
/// Together those are the netty
/// `channeloutboundbuffer-close-ordering-three-classes` page: netty's
/// `AbstractChannel.close()` runs `doClose0()` and then, in the same `finally`,
/// `outboundBuffer.close(cause)`, which throws
/// `IllegalStateException: close() must be invoked after the channel is closed.`
/// on a channel still reading open. `DnsNameResolverTest` logged it 384 times
/// with the JIT on and **0** times under `--nojit`.
///
/// # The question this asks
///
/// Not "does the declaring class have a native" — that is the screen the
/// guarded-inline resolver already learned was too weak
/// (`resolve_inline_site_from`'s `native-shadow-on-receiver-chain`, 2026-09-04)
/// — but "is a native registered on ANY class a receiver at this site could
/// have". A devirtualised site has no receiver, so the walk runs the other way:
/// ask the registry which classes own a native for this `(name, descriptor)`,
/// and refuse if any of them is the declaring class, the constant-pool class,
/// or a subclass of the declaring class.
///
/// # The one case it cannot see
///
/// A native-carrying subclass that is **not loaded yet** when the site
/// compiles. It is skipped deliberately: an unloaded class cannot be a
/// receiver, and treating every unloaded owner as a hazard would refuse every
/// `close()V` / `equals` / `hashCode` site in the tree, since those names are
/// registered on dozens of classes most runs never load. The residual is the
/// same one the guarded-inline screen carries — that one keys on the receiver
/// it has SEEN — and it is named here rather than left implicit.
fn final_devirt_native_shadow(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    cp_class_id: ClassId,
    declaring_id: ClassId,
    method_name: &str,
    descriptor: &str,
) -> bool {
    if !crate::runtime::env_cache::jit_final_devirt_native_screen() {
        return false;
    }
    let registry = &shared.natives.native_methods;
    let store = cm.class_store();
    // The exact-name half, which is also the WHOLE answer whenever the class is
    // `final`: no subclass can exist, so the constant-pool class and the
    // declaring class are the only receivers there are.
    for id in [cp_class_id, declaring_id] {
        if let Some(class) = store.get(id) {
            if registry
                .find(&class.name, method_name, descriptor)
                .is_some()
            {
                return true;
            }
        }
    }
    // The subclass half. `owner_classes_for_method` is the inverted index; the
    // list is normally empty (no native anywhere for this name/descriptor) and
    // is at most a handful of names when it is not.
    let owners = registry.owner_classes_for_method(method_name, descriptor);
    owners.iter().any(|owner| {
        cm.get_loaded_class_id(owner)
            .is_some_and(|owner_id| is_subclass_of(store, owner_id, declaring_id))
    })
}

/// Is `candidate` `declaring_id` itself, or a subclass of it?
///
/// The superclass chain only, matching `invoke_or_native`'s own walk: a native
/// registered on an INTERFACE never shadows a `final` class method, because
/// native resolution for a virtual call never walks interfaces.
fn is_subclass_of(
    store: &crate::classloading::ClassStore,
    candidate: ClassId,
    declaring_id: ClassId,
) -> bool {
    let mut cur = Some(candidate);
    while let Some(id) = cur {
        if id == declaring_id {
            return true;
        }
        cur = store.get(id).and_then(|c| c.superclass);
    }
    false
}

/// Resolve a Java 11+ private-method call encoded as `invokevirtual`.
///
/// Private methods are not virtual dispatch targets even when modern classfiles
/// encode the call with opcode 0xb6. Dispatch must stay pinned to the resolved
/// constant-pool target; otherwise a subclass/private-static helper with the
/// same name and descriptor can be selected by receiver-class lookup.
pub(super) fn resolved_private_invokevirtual_target(
    shared: &SharedVm,
    current_class_id: ClassId,
    method_class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> Option<(ClassId, Arc<str>)> {
    // A private method is only ever invocable, per JVM access control, from
    // within the exact class that declares it — so a private-via-invokevirtual
    // call's CP-resolved owner name always names the CALLER's own class.
    // Resolve directly against `current_class_id` in that case: it's precise
    // by construction, with no name lookup involved and therefore no risk of
    // a loader-blind name lookup returning a DIFFERENT loaded copy of a
    // same-named class. Two classloaders each defining their own
    // `org/h2/mvstore/RootReference` is exactly this shape: `lookup_loader_
    // initiated` can miss (this call is the class resolving ITSELF by name,
    // not a delegated import), and its `get_loaded_class_id` fallback is a
    // single-slot "first loaded wins" map that silently returns the OTHER
    // loader's copy — pinning a private call's dispatch to the wrong
    // class's bytecode/constant pool while the receiver stays the caller's
    // own (correct-loader) object. See
    // bug-h2-suite-residual-fail-triage-FIXED.md
    // (TestUpgrade's `RootReference.tryUpdate`/`hasChangesSince` residual).
    //
    // The self-reference test and the method lookup share one `class_manager`
    // guard on that (common) path. The other path drops it first, because
    // `lookup_loader_initiated` takes its own locks.
    // The RESOLVED method (JVMS §5.4.3.3: the first declaration on the
    // superclass chain, whatever its flags) decides, not
    // `find_method_recursive`, whose first phase steps past an abstract
    // declaration to a concrete ancestor. `A { private m }`, `abstract B
    // extends A { abstract m }`, `C extends B` calling `invokevirtual C.m`
    // resolves to B's abstract `m` and dispatches virtually; pinning A's
    // private `m` ran A.m for every receiver (javac output; interpreter round
    // i1 wave 29, lane L4, probe
    // `tools/probes/interp/L4/L4W29PrivateAncestorShadow.java`).
    let private_target_in = |cm: &crate::classloading::ClassManager,
                             target_class_id: ClassId|
     -> Option<(ClassId, Arc<str>)> {
        let store = &cm.class_store;
        let declaring_id = crate::runtime::resolve::selection::resolve_declaring(
            store,
            target_class_id,
            method_name,
            method_descriptor,
        )?;
        let method = store
            .get(declaring_id)?
            .find_method(method_name, method_descriptor)?;
        if !method.access_flags.contains(MethodAccessFlags::PRIVATE) {
            return None;
        }
        let declaring_name = store
            .get(declaring_id)
            .map(|c| Arc::clone(&c.name))
            .unwrap_or_else(|| Arc::from(method_class_name));
        Some((declaring_id, declaring_name))
    };
    {
        let cm = shared.classes.class_manager.read();
        let self_match = cm
            .get_class(current_class_id)
            .map(|c| {
                super::constants::is_self_class_reference(&c.name, c.is_hidden(), method_class_name)
            })
            .unwrap_or(false);
        if self_match {
            return private_target_in(&*cm, current_class_id);
        }
    }
    let target_class_id = lookup_loader_initiated(shared, current_class_id, method_class_name)
        .or_else(|| {
            shared
                .classes
                .class_manager
                .read()
                .get_loaded_class_id(method_class_name)
        })?;
    let cm = shared.classes.class_manager.read();
    private_target_in(&*cm, target_class_id)
}

/// May the stale-`java.lang.Thread`-mirror recovery consult the former-mirror
/// address table for a receiver with this class id and this header?
///
/// **`class_id == 0` on its own is not the question**, and getting that wrong
/// is what made the recovery substitute a thread mirror for a live `long[]`.
/// Three unrelated things wear `ClassId(0)` (see `memory::reclaim_guard`):
///
/// * a span the collector reclaimed and zeroed — the case the recovery exists
///   for, and the only one where the former-address table's identity argument
///   holds;
/// * a genuine `new Object()` — since H1, one with a non-zero identity hash;
/// * **every primitive array.** An array header carries its COMPONENT class id
///   (JVMS §4.4.1) and `long[]`/`int[]`/`byte[]` have none, so `Newarray`
///   stamps `ClassId::new(0)`.
///
/// Only the first has an all-zero header: an array has a non-zero `kind`,
/// `element_type` and `shape`, and a live object has a non-zero identity hash.
/// Same 16 bytes, and the same test, as `execute_invoke_kind`'s own
/// stale-pointer detector further down.
#[inline]
pub(super) fn stale_mirror_recovery_applies(
    class_id: ClassId,
    header: &[u8; cratonvm_types::HEADER_SIZE],
) -> bool {
    class_id == ClassId::new(0) && *header == [0u8; cratonvm_types::HEADER_SIZE]
}

/// Can a call site whose constant pool names `cp_class_name` be holding a
/// `java.lang.Thread` mirror at all?
///
/// The second half of the stale-mirror recovery's gate, and the half that does
/// not depend on the header. Two names are refused outright:
///
/// * **an array type.** `[J` has no relationship to `java.lang.Thread` in
///   either direction, so a mirror there is wrong by construction — no heap
///   state can make it right;
/// * **bare `java/lang/Object`.** Every mirror is assignable to it, so it
///   carries no evidence that the receiver was ever a mirror. Admitting it is
///   exactly what would leave the `new Object()` window open: a zero-field
///   `Object` has an all-zero header, so the header test cannot separate it
///   from a reclaimed span, and an `Object`-typed call site cannot either.
///
/// Everything else is admitted only if the recovered mirror really is an
/// instance of the named type. The check is by NAME and walks supers *and*
/// interfaces ([`Class::is_assignable_to_name`]), so a `Runnable.run()` site on
/// a `Thread` still recovers, and a site typed `MyThread` refuses a plain
/// `java.lang.Thread` mirror — correctly, because a vacated address identified
/// ONE thread's mirror and if that mirror is not of the site's type the
/// substitution was going to be wrong anyway.
///
/// This narrows the recovery. The case it exists for —
/// `Thread.currentThread().getThreadGroup()` in Tomcat's
/// `TaskThreadFactory.<init>`, see
/// `gc-blocked-thread-frame-stale-thread-mirror-RESOLVED.md`
/// — names `java/lang/Thread` and is unaffected. An `Object`-typed use of a
/// stale mirror now reads the zeroed object instead of being repaired; that
/// degrades a `toString`, where admitting it risks corrupting a live object's
/// identity.
fn mirror_is_plausible_at_call_site(
    shared: &SharedVm,
    cp_class_name: &str,
    mirror: ObjectRef,
) -> bool {
    if !call_site_type_can_hold_a_thread_mirror(cp_class_name) {
        return false;
    }
    let mirror_cid = shared.mem.heap.class_id_of(mirror);
    let cm = shared.classes.class_manager.read();
    cm.get_class(mirror_cid)
        .is_some_and(|c| c.is_assignable_to_name(cp_class_name, &cm.class_store))
}

/// The name-only half of [`mirror_is_plausible_at_call_site`] — the two
/// call-site types that can never be evidence of a thread mirror, whatever the
/// heap says.
#[inline]
pub(super) fn call_site_type_can_hold_a_thread_mirror(cp_class_name: &str) -> bool {
    !cp_class_name.starts_with('[') && cp_class_name != "java/lang/Object"
}

/// [`stale_mirror_recovery_applies`] against a live receiver, reading its
/// header only after confirming the address is inside a heap region.
fn receiver_is_a_reclaimed_span(shared: &SharedVm, recv: ObjectRef) -> bool {
    if shared
        .mem
        .heap
        .is_heap_addr(recv.as_ptr() as usize)
        .is_none()
    {
        return false;
    }
    // SAFETY: `is_heap_addr` just confirmed the address is inside a heap
    // region, and every heap object begins with a readable header at least
    // 16 bytes long.
    let header: [u8; cratonvm_types::HEADER_SIZE] =
        unsafe { std::ptr::read(recv.as_ptr() as *const [u8; cratonvm_types::HEADER_SIZE]) };
    stale_mirror_recovery_applies(shared.mem.heap.class_id_of(recv), &header)
}

// ---------------------------------------------------------------------------
// Round 13 wave 4 (lane proxy2): one dynamic-proxy dispatch model.
// ---------------------------------------------------------------------------

/// Whether a call `execute_invoke_kind`'s `is_proxy_dispatch` would intercept
/// (a non-special call on a receiver whose class reaches a proxy super) takes
/// the ordinary dispatch instead.
///
/// The intercept called the `InvocationHandler` from Rust
/// (`vm::proxy_invoke_handler_shared`) while a compiled caller dispatches
/// into the generated `$ProxyN` body, so any defect of the body, its
/// `<clinit>` or its constant pool was JIT-only by construction (round 13
/// wave 3's Spring `IllegalAccessError`), and the two tiers disagreed on
/// observable facts: the `Method` the handler receives (a separately cached
/// synthesized object against the body's `<clinit>` `getMethod` result),
/// argument boxing, and when `<clinit>` runs. So a call the receiver's own
/// class DECLARES runs that body in the interpreter too. Both tiers then
/// share `native_builtins`' `native_proxy_dispatch_invoke`. Running the body
/// rather than intercepting compiled calls is also the cheaper model: the
/// interpreted (cold) call pays a few bytecodes and the native door, while
/// hot callers keep their compiled, inlinable body. Kept on the intercept:
/// a class that does not declare the method (the synthetic `Proxy$Instance`
/// shim has no bodies) and, only under `CRATONVM_PROXY_ANNOTATION_BODY=0`, a
/// proxy whose handler is the VM's own `AnnotationProxy` (annotation member
/// data, not a user handler; round 13 wave 6 runs its body by default).
/// `CRATONVM_PROXY_ONE_DISPATCH_MODEL=0` restores the intercept.
///
/// `Object`'s `wait` / `notify` / `notifyAll` are `final`: no proxy class
/// declares them and the JDK never hands them to the handler, but the
/// intercept did. They take the ordinary dispatch to `Object`'s natives
/// (`CRATONVM_PROXY_FINAL_OBJECT_METHODS=0` restores the intercept).
/// `getClass` stays on the intercept, which already answers it as `Object`
/// does.
pub(crate) fn proxy_call_takes_ordinary_dispatch(
    shared: &SharedVm,
    args: &[Value],
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    use cratonvm_native_builtins::reflect_annotations as proxy_natives;
    let Some(Value::Object(Some(receiver))) = args.first().copied() else {
        return false;
    };
    if shared.mem.heap.kind_of(receiver) != cratonvm_types::ObjectKind::Object {
        return false;
    }
    let receiver_class_id = shared.mem.heap.class_id_of(receiver);
    if matches!(
        (method_name, method_descriptor),
        ("wait", "()V")
            | ("wait", "(J)V")
            | ("wait", "(JI)V")
            | ("notify", "()V")
            | ("notifyAll", "()V")
    ) {
        // Not for an instance of the synthetic shim itself: it has no class
        // file, and its by-name dispatch is not worth trusting for a
        // fallback-only shape.
        if !proxy_natives::proxy_final_object_methods_bypass_handler() {
            return false;
        }
        let cm = shared.classes.class_manager.read();
        return cm
            .get_class(receiver_class_id)
            .is_some_and(|class| &*class.name != "java/lang/reflect/Proxy$Instance");
    }
    let cm = shared.classes.class_manager.read();
    proxy_receiver_runs_its_body_in(
        shared,
        &cm,
        receiver,
        receiver_class_id,
        method_name,
        method_descriptor,
    )
}

/// The body half of [`proxy_call_takes_ordinary_dispatch`], under a
/// class-manager read guard the caller already holds (it takes no lock of
/// its own, so the interpreter's vtable fast path can ask it under its guard
/// without the nested-read self-deadlock that block documents): whether the
/// proxy `receiver` of class `receiver_class_id` runs its own generated body
/// for `(method_name, method_descriptor)` -- its class declares the method
/// (and, under `CRATONVM_PROXY_ANNOTATION_BODY=0`, its handler is not the
/// VM's `AnnotationProxy`) -- while `CRATONVM_PROXY_ONE_DISPATCH_MODEL` is on.
pub(super) fn proxy_receiver_runs_its_body_in(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    receiver: ObjectRef,
    receiver_class_id: ClassId,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    use cratonvm_native_builtins::reflect_annotations as proxy_natives;
    if !proxy_natives::proxy_interpreter_runs_generated_body() {
        return false;
    }
    // Slot 0 is the handler in both proxy layouts (`Proxy.h`, and the
    // synthetic shim's first slot).
    let handler_class_id = match shared.mem.heap.get_field(receiver, 0) {
        Value::Object(Some(handler)) => Some(shared.mem.heap.class_id_of(handler)),
        _ => None,
    };
    let Some(receiver_class) = cm.get_class(receiver_class_id) else {
        return false;
    };
    let declares = receiver_class
        .find_method(method_name, method_descriptor)
        .is_some();
    // Round 13 wave 5 (lane proxy3): the proxy predicates are name-based
    // (`class_name_is_proxy_super`), so a user class that extends
    // `java.lang.reflect.Proxy` itself (legal: `protected Proxy(h)`) was
    // treated as a generated proxy, and every call it does not declare --
    // an inherited method, `toString` -- went to its handler. HotSpot runs
    // such a class like any other (`Proxy.isProxyClass` is false for it).
    // Only a generated `$ProxyN` (origin `GeneratedProxy`) and the synthetic
    // shim keep the intercept for what they do not declare.
    // `CRATONVM_PROXY_USER_SUBCLASS_ORDINARY=0` restores the old answer.
    // The same answer as the cast / `aastore` / lambda screens
    // (`typecheck.rs`, round 13 wave 6).
    let user_subclass = proxy_natives::proxy_user_subclass_is_ordinary()
        && !super::typecheck::class_is_generated_proxy_or_shim_in(receiver_class);
    // Round 13 wave 6 (lane proxy4): an annotation proxy (handler = the VM's
    // `AnnotationProxy`) runs its body too, through
    // `native_proxy_dispatch_invoke`'s `AnnotationProxy` arms -- the route a
    // compiled caller and, since wave 5, a cached interpreted site already
    // took. Keeping only the uncached interpreted call on the intercept left
    // two renderings of one annotation live in one run.
    // `CRATONVM_PROXY_ANNOTATION_BODY=0` keeps it on the intercept.
    let annotation_handler = !proxy_natives::proxy_annotation_handler_runs_body()
        && handler_class_id
            .and_then(|id| cm.get_class(id))
            .is_some_and(|class| &*class.name == "java/lang/annotation/AnnotationProxy");
    (declares || user_subclass) && !annotation_handler
}

/// Variant of `execute_invoke` that knows whether the source bytecode was
/// `invokeinterface`. Only invokeinterface call sites pass `is_interface=true`;
/// invokevirtual / invokespecial pass `false`. The flag gates γ's CP-resolved-
/// interface stash so the default-method rescue at the NSME emit site never
/// fires for non-interface dispatch (which could otherwise re-route to a
/// stale receiver class on Java-exception unwinding through unrelated invokes).
pub(super) fn execute_invoke_kind(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    is_special: bool,
    is_interface: bool,
    pc: usize,
) -> Result<CachedCallResult, MethodCallFailed> {
    dbg_invoke_stats_record(3);
    let current_class_id = thread.frames[frame_idx].class_id;

    // Before the constant pool is read: an owner failure below is recorded
    // only while no redefinition may have replaced that pool.
    let fill_as_of = crate::classloading::resolution::ResolutionCache::fill_snapshot();
    let (method_class_name, method_name, method_descriptor, num_params) =
        resolve_method_ref(shared, current_class_id, cp_index).map_err(|e| {
            settle_method_resolution_failure(
                shared,
                thread,
                current_class_id,
                cp_index,
                e,
                fill_as_of,
            )
        })?;
    let method_owner_name = Arc::clone(&method_class_name);

    // JVMS §6.5 / §5.4.3.3: the method reference, and first its CLASS, is
    // resolved before the receiver is looked at, and a failed class entry
    // fails every later resolution through it (§5.4.3). This path resolves
    // the owner only symbolically (below), which is enough to dispatch a
    // non-null receiver, but a NULL receiver of an owner that was never
    // loaded threw `NullPointerException` where HotSpot throws the
    // resolution error, and nothing was recorded (interpreter round i1 wave
    // 25, lane L5b; probe `L5W25OwnerFailureRecordInvoke`, `virtual` rows).
    // So: rethrow a recorded owner failure (one relaxed load when the VM has
    // none), and for a null receiver whose owner is not known in the caller's
    // namespace, resolve it through the caller's loader, recording a failure.
    // Both run while the arguments are still on the operand stack, so a
    // `loadClass` upcall's collection sees them. A non-null receiver keeps
    // the receiver-driven path unchanged.
    if !method_class_name.starts_with('[') {
        if let Some(recorded) =
            recorded_member_owner_failure(shared, thread, current_class_id, cp_index)
        {
            return Err(recorded);
        }
        let receiver_is_null = {
            let stack = &thread.frames[frame_idx].stack;
            stack.len() > num_params && matches!(stack.peek_at(num_params), Value::Object(None))
        };
        let owner_unknown = receiver_is_null && {
            let cm = shared.classes.class_manager.read();
            owner_known_to_caller(shared, &cm, current_class_id, &method_class_name).is_none()
        };
        if owner_unknown {
            resolve_class_loader_aware(shared, thread, current_class_id, &method_class_name)
                .map_err(|e| {
                    crate::runtime::exceptions::convert_class_not_found_for(
                        shared,
                        thread,
                        Some(current_class_id),
                        &method_class_name,
                        e,
                    )
                })
                .map_err(|e| {
                    record_member_owner_failure_as_of(
                        shared,
                        current_class_id,
                        cp_index,
                        e,
                        fill_as_of,
                    )
                })?;
            // JVMS §5.4.3: a failure another thread recorded for the entry
            // meanwhile is the outcome (`--jdk-only`; wave 39, lane L5).
            if let Some(recorded) = recorded_member_owner_failure_after_success(
                shared,
                thread,
                current_class_id,
                cp_index,
            ) {
                return Err(recorded);
            }
            // The owner is loaded now, so the JVMS §5.4.4 checks the first
            // resolution could not run (it did not cache its answer) run
            // here, before the null receiver is looked at — as
            // `execute_invokestatic` re-resolves after loading its owner
            // (interpreter round i1 wave 26, lane L5). Where they are
            // enforced only: `--compatible` keeps its NPE here unchanged.
            if member_access_enforced(shared) {
                let _ = resolve_method_ref(shared, current_class_id, cp_index).map_err(|e| {
                    settle_method_resolution_failure(
                        shared,
                        thread,
                        current_class_id,
                        cp_index,
                        e,
                        fill_as_of,
                    )
                })?;
            }
        }
    }

    // The constant-pool owner, looked up in the caller's loader namespace
    // without loading anything, answers two questions in one
    // `class_manager` read: the JVMS §6.5 static-flag check below, and the
    // resolved reference §5.4.6 selection needs further down. `None` (owner
    // not loaded there, or an array owner) leaves both on their lenient paths.
    //
    // JVMS §6.5: `invokevirtual` / `invokespecial` / `invokeinterface` of a
    // STATIC method is an `IncompatibleClassChangeError`. It is a linkage
    // error, so it precedes the receiver's null check and pops nothing. The
    // site is never cached: this function returns before any populate step,
    // so the error is raised on every execution, as on HotSpot.
    let (cp_owner_resolved, static_mismatch, cp_owner_unknown): (
        Option<crate::runtime::resolve::selection::ResolvedRef>,
        Option<String>,
        bool,
    ) = if method_class_name.starts_with('[') {
        (None, None, false)
    } else {
        let cm = shared.classes.class_manager.read();
        match owner_known_to_caller(shared, &cm, current_class_id, &method_class_name) {
            Some(owner) => (
                // Selection is for virtual dispatch only.
                (!is_special)
                    .then(|| {
                        crate::runtime::resolve::selection::resolved_ref_for_owner(
                            &cm.class_store,
                            owner,
                            &method_name,
                            &method_descriptor,
                        )
                    })
                    .flatten(),
                // The reference's kind against its class first (wave 39):
                // HotSpot checks it while resolving, before the static flag.
                methodref_kind_mismatch(
                    shared,
                    &cm,
                    current_class_id,
                    cp_index,
                    owner,
                    if is_special {
                        MethodrefUse::Special
                    } else if is_interface {
                        MethodrefUse::Interface
                    } else {
                        MethodrefUse::Virtual
                    },
                    &method_name,
                    &method_descriptor,
                )
                .or_else(|| {
                    crate::runtime::resolve::selection::static_flag_mismatch(
                        &cm.class_store,
                        owner,
                        &method_name,
                        &method_descriptor,
                        false,
                    )
                    .map(|message| interface_method_static_wording(&cm, owner, message))
                }),
                false,
            ),
            None => (None, None, true),
        }
    };
    if let Some(message) = static_mismatch {
        return Err(crate::error::LinkageError::IncompatibleClassChangeError { message }.into());
    }
    // JVMS §6.5 / §5.4.3.3 for a NON-null receiver: the owner is resolved
    // before the receiver decides anything (interpreter round i1 wave 27, lane
    // L5; `i25-L5-invokevirtual-does-not-resolve-an-unloaded-owner-before-the-
    // null-check` (a)). Only when the lookup just made found no owner in the
    // caller's namespace (the null-receiver arm above has resolved its own
    // case already), and out of line: a warm site never gets here.
    if cp_owner_unknown {
        resolve_owner_unknown_to_caller(
            shared,
            thread,
            current_class_id,
            cp_index,
            &method_class_name,
            fill_as_of,
        )?;
    }

    // PGO-01 (pgo-01-call-site-evidence-gap.md):
    // call-site evidence for invokespecial. invokevirtual/invokeinterface are
    // NOT recorded here — they are covered by the receiver-type profile
    // instead (see MethodProfile's doc comment on `receivers` vs
    // `call_sites`), and double-recording both would double-count a single
    // call-site execution against two different evidence sources. Recorded
    // once, here, right after resolution succeeds — this function is only
    // reached on a genuine miss from the fast cached dispatch
    // (execute_invokevirtual_cached), so by this point the invokespecial
    // instruction is definitely executing, not merely being probed. NOT
    // placed at every downstream successful-dispatch return point, of which
    // this function has many (native, JIT, lambda-proxy, default-method
    // rescue…) — see MethodProfile::call_sites's doc comment for the explicit
    // concurrency contract: this is a heuristic hotness signal for the
    // inliner, not a correctness input, so the small over/under-count this
    // early-return placement can produce on a resolution error is acceptable.
    if is_special && crate::jit::profile::is_receiver_profiling_enabled()
        && !thread.frames[frame_idx].runs_obsolete_method()
    {
        let (cid, mn, md) = method_key_parts(&thread.frames[frame_idx]);
        shared
            .jit
            .profile_store
            .record_call_site_borrowed(cid, mn, md, pc, thread.frames[frame_idx].code.len());
    }

    if crate::runtime::env_cache::dbg_loader_trace()
        && method_owner_name.contains("RootReference")
        && &*method_name == "<init>"
    {
        let cm = shared.classes.class_manager.read();
        let cur_loader = cm.get_loader_id(current_class_id);
        drop(cm);
        eprintln!(
            "[EIK-ENTRY] cp_index={cp_index} is_special={is_special} current_class_id={current_class_id:?} cur_loader={cur_loader:?} method={method_owner_name}.{method_name}{method_descriptor}"
        );
    }

    if crate::runtime::env_cache::dbg_hang_sample() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CALL_COUNT: AtomicU64 = AtomicU64::new(0);
        let n = CALL_COUNT.fetch_add(1, Ordering::Relaxed);
        if n % 200_000 == 0 {
            eprintln!(
                "[HANG_SAMPLE_V1] call#{n} {}.{}{}",
                &*method_class_name, &*method_name, &*method_descriptor
            );
        }
    }

    // KAFKA-DEFAULT-RESCUE: snapshot the CP-resolved class id (if loaded)
    // before any downstream code can move `method_class_name`. The default-
    // method rescue at the NSME emit site in `invoke_on_class_shared_inner`
    // reads this via a thread-local stashed below.
    //
    // Option B+C gating: only stash for `invokeinterface` AND only when the
    // CP-resolved class is actually a Java interface. Non-interface invokes
    // (virtual / special / static) must not arm the rescue — its
    // `find_method_recursive` walk would otherwise pick the wrong declaring
    // class for default methods that conflict with the receiver's class
    // hierarchy. The slot is also left unset when the resolved class isn't
    // an interface so a malformed CP entry can't poison nested dispatch.
    let cp_resolved_class_id: Option<ClassId> = if is_interface {
        // An interface Methodref is resolved by the current frame's initiating
        // loader too. The cache-preparation probe must not create a global
        // duplicate before the actual invokeinterface dispatch has a chance to
        // use its loader-local constant-pool identity.
        let loaded =
            resolve_class_loader_aware(shared, thread, current_class_id, &method_class_name).ok();
        loaded.and_then(|cid| {
            let cm = shared.classes.class_manager.read();
            cm.get_class(cid).filter(|c| c.is_interface()).map(|_| cid)
        })
    } else {
        None
    };

    let total_args = num_params + 1;

    // PERF (2026-07-21): see `pop_coerced_invoke_args_virtual` — same
    // non-allocating `nth_param_tag_byte` swap for `split_method_descriptor`.
    // Pop slots as raw CompactValue and decode with the parameter descriptor
    // so a category-2 long whose NaN-box bit pattern collides with a tagged
    // sub-tag survives bit-exact. The prior `pop()` → `to_value()` decoded
    // such a long as `Value::Int`, which `coerce_invoke_arg_for_descriptor`
    // then widened — corrupting `J` args to invokevirtual/special callees.
    // Mirrors `pop_coerced_invoke_args_virtual`. See
    // bc-ec-mod-mododdinverse-investigation.md.
    //
    // Popped straight into their final slots, last parameter first: one `Vec`
    // per call rather than a staging `Vec` of raw slots plus this one. The
    // decode of each slot is unchanged, and `ParamTags::get` is random-access.
    let mut args = vec![Value::Uninitialized; total_args];
    // ONE forward scan, hoisted out of this per-argument loop.
    let param_tags = ParamTags::of(&method_descriptor);
    for i in (0..num_params).rev() {
        let (cv, kind) = thread.frames[frame_idx].stack.pop_with_kind()?;
        let pd_byte = param_tags.get(&method_descriptor, i);
        let v = decode_arg_kind_aware(cv, kind, pd_byte);
        args[i + 1] = coerce_invoke_arg_for_descriptor(pd_byte, v);
    }
    let recv_val = thread.frames[frame_idx]
        .stack
        .pop_with_kind()?
        .0
        .decode_by_descriptor(b'L');
    if crate::runtime::env_cache::dbg_jetty2() && &*method_name == "getClasspath" {
        eprintln!(
            "[jetty2-eik] execute_invoke_kind {}.{}{} receiver={:?}",
            &*method_class_name, &*method_name, &*method_descriptor, recv_val
        );
    }
    args[0] = coerce_invoke_arg_for_descriptor(b'L', recv_val);

    // Apply the same forwarding read barrier used by getfield to every
    // reference copied from the operand stack. A moving collection can leave
    // an old from-space address in a frame slot; once the invoke pops that
    // slot it is no longer visible to the frame-root remapper. Dispatch then
    // dereferences the stale receiver (or a stale object argument) while
    // resolving/invoking the callee. Refresh while the forwarding header is
    // still available, before any class lookup or native call can touch it.
    // The receiver exactly as the operand stack handed it over, before the
    // barrier above may have rewritten it. Kept so the invariant check further
    // down can say WHICH of the two is the bad one: a receiver that was already
    // wrong on the stack is a root-remap gap, whereas one that only became
    // wrong here means `load_and_forward` redirected a live reference through a
    // forwarding word it should not have trusted. Nothing but a `Copy` of an
    // already-materialised `Value`; no allocation, no branch on the hot path.
    let recv_before_refresh = match args.first() {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    for value in &mut args {
        if let Value::Object(Some(obj)) = value {
            let before = *obj;
            // The word the barrier is about to decide on, read once here so a
            // later report can quote it. The barrier itself re-loads it; a
            // disagreement between the two is itself the finding.
            // SAFETY: `before` is a reference popped from the operand stack and
            // is about to be dereferenced by `load_and_forward` anyway;
            // `MARK_WORD_OFFSET` is inside the header of any heap object.
            let mark = if shared
                .mem
                .heap
                .is_heap_addr(before.as_ptr() as usize)
                .is_some()
            {
                unsafe {
                    std::ptr::read(
                        before.as_ptr().add(cratonvm_types::MARK_WORD_OFFSET) as *const u32
                    ) as u64
                }
            } else {
                0
            };
            *obj = shared.mem.heap.load_and_forward(*obj);
            if obj.as_ptr() != before.as_ptr() {
                crate::memory::reclaim_guard::note_barrier_rewrite(
                    before.as_ptr() as usize,
                    mark,
                    obj.as_ptr() as usize,
                );
            }
        }
    }
    // JVMS §6.5 `invokespecial`: a null `objectref` raises
    // NullPointerException before the callee frame exists. The receiver-driven
    // null check further down lives in the arm only `invokevirtual` /
    // `invokeinterface` reach, so a special call — javac emits one for
    // `other.privateMethod()` inside the declaring class, e.g.
    // `Class.isDirectSubType`'s `c.getInterfaces(false)` — pushed a frame with
    // `this == null` and ran the body. The cached `Bytecode` arm already
    // defers a null receiver here precisely so that this path owns the NPE.
    // `<init>` is excluded: the verifier guarantees an uninitialized, non-null
    // receiver for it, and constructor dispatch keeps its existing handling.
    if is_special
        && method_name.as_ref() != "<init>"
        && matches!(args.first(), Some(Value::Object(None)))
    {
        let npe_msg = helpful_npe_invoke_message(
            shared,
            thread,
            frame_idx,
            &method_class_name,
            &method_name,
            &method_descriptor,
            num_params,
        );
        return Err(RuntimeError::NullPointerException {
            message: Some(npe_msg),
        }
        .into());
    }
    let args_root_guard = InvokeArgsRootGuard::new(thread, &args);
    if crate::runtime::env_cache::dbg_loader_trace()
        && method_class_name.contains("RootReference")
        && method_name.as_ref() == "tryUpdate"
    {
        let describe = |v: &Value| -> String {
            match v {
                Value::Object(Some(obj)) => {
                    let cid = shared.mem.heap.class_id_of(*obj);
                    let cn = shared
                        .classes
                        .class_manager
                        .read()
                        .get_class(cid)
                        .map(|c| c.name.to_string())
                        .unwrap_or_default();
                    format!("addr={:?} cid={:?} loader_class={}", obj, cid, cn)
                }
                Value::Object(None) => "null".to_string(),
                other => format!("{:?}", other),
            }
        };
        eprintln!(
            "[TRYUPDATE-TRACE/slow] caller_class_id={:?} cp_index={} receiver(args[0])={} updated(args[1])={}",
            current_class_id,
            cp_index,
            describe(&args[0]),
            args.get(1).map(describe).unwrap_or_default(),
        );
    }
    if crate::runtime::env_cache::dbg_loader_trace() && &*method_name == "compareAndSetRoot" {
        let describe = |v: &Value| -> String {
            match v {
                Value::Object(Some(obj)) => {
                    let cid = shared.mem.heap.class_id_of(*obj);
                    let cn = shared
                        .classes
                        .class_manager
                        .read()
                        .get_class(cid)
                        .map(|c| c.name.to_string())
                        .unwrap_or_default();
                    format!("addr={:?} cid={:?} loader_class={}", obj, cid, cn)
                }
                Value::Object(None) => "null".to_string(),
                other => format!("{:?}", other),
            }
        };
        eprintln!(
            "[CASROOT-TRACE/slow] caller_class_id={:?} cp_index={} receiver_map(args[0])={} expected(args[1])={} updated(args[2])={}",
            current_class_id,
            cp_index,
            describe(&args[0]),
            args.get(1).map(describe).unwrap_or_default(),
            args.get(2).map(describe).unwrap_or_default(),
        );
    }
    if crate::runtime::env_cache::dbg_loader_trace()
        && method_class_name.contains("Page")
        && method_name.as_ref() == "<init>"
    {
        let describe = |v: &Value| -> String {
            match v {
                Value::Object(Some(obj)) => {
                    let cid = shared.mem.heap.class_id_of(*obj);
                    let cn = shared
                        .classes
                        .class_manager
                        .read()
                        .get_class(cid)
                        .map(|c| c.name.to_string())
                        .unwrap_or_default();
                    format!("addr={:?} cid={:?} loader_class={}", obj, cid, cn)
                }
                Value::Object(None) => "null".to_string(),
                other => format!("{:?}", other),
            }
        };
        eprintln!(
            "[PAGEINIT-TRACE/slow] caller_class_id={:?} cp_index={} ctor_desc={} new_page(args[0])={} map_arg(args[1])={}",
            current_class_id,
            cp_index,
            method_descriptor,
            describe(&args[0]),
            args.get(1).map(describe).unwrap_or_default(),
        );
    }
    // Register the freshly constructed receiver with the software watchpoint
    // ONLY when its class is in the watch filter. Registering every
    // constructed object (the original shape) makes each heap field write take
    // the watch mutex and scan the registry, and buries the interesting lines
    // under millions of unrelated ones — the flag was effectively unusable on
    // anything larger than the H2 repro it was written for.
    //
    // `dbg_field_watch` first: unarmed, the class filter still ran its
    // default `contains("Page") || contains("RootReference")` scans on every
    // slow-path invoke only for `watch` to ignore the answer. This block also
    // used to appear twice, verbatim.
    if let Value::Object(Some(o)) = &args[0] {
        if crate::runtime::env_cache::dbg_field_watch()
            && crate::runtime::env_cache::field_watch_class_matches(&method_class_name)
        {
            cratonvm_types::field_watch::watch(*o);
        }
    }
    if crate::runtime::env_cache::dbg_loader_trace()
        && method_class_name.contains("RootReference")
        && method_name.as_ref() == "<init>"
    {
        let describe = |v: &Value| -> String {
            match v {
                Value::Object(Some(obj)) => {
                    let cid = shared.mem.heap.class_id_of(*obj);
                    let cn = shared
                        .classes
                        .class_manager
                        .read()
                        .get_class(cid)
                        .map(|c| c.name.to_string())
                        .unwrap_or_default();
                    format!("addr={:?} cid={:?} loader_class={}", obj, cid, cn)
                }
                Value::Object(None) => "null".to_string(),
                other => format!("{:?}", other),
            }
        };
        eprintln!(
            "[ROOTREFINIT-TRACE/slow] caller_class_id={:?} cp_index={} ctor_desc={} this(args[0])={} a1={} a2={} a3={}",
            current_class_id,
            cp_index,
            method_descriptor,
            describe(&args[0]),
            args.get(1).map(describe).unwrap_or_default(),
            args.get(2).map(describe).unwrap_or_default(),
            args.get(3).map(describe).unwrap_or_default(),
        );
    }
    // Spring's loader-fork test infrastructure can expose two physical copies
    // of this private enum while representing one logical annotation operation.
    // Preserve the enum member identity by its declaring binary name and enum
    // constant name only for that loader-aware bridge; ordinary cross-loader
    // Enum.equals/identity semantics remain unchanged.
    //
    // This is NOT redundant with the registered `MergedAnnotation$Adapt.isIn`
    // native override + `force_native_over_real_jdk_bytecode` gate elsewhere
    // in this file: empirically (2026-07-19 merge with origin/dev, which
    // shipped that native override independently), removing this inline
    // bridge and relying on the native override alone reintroduced the
    // WebFluxManagementChildContextConfigurationIntegrationTests hang (stuck
    // within seconds of the first sub-test, host load LOW at the time — not
    // a contention artifact). Keep both: this bridge covers whatever
    // dispatch path reaches `isIn` without going through the native-override
    // gate for this specific loader-forked scenario.
    if crate::runtime::env_cache::loader_aware_resolution()
        && method_class_name.as_ref()
            == "org/springframework/core/annotation/MergedAnnotation$Adapt"
        && method_name.as_ref() == "isIn"
        && method_descriptor.as_ref()
            == "([Lorg/springframework/core/annotation/MergedAnnotation$Adapt;)Z"
    {
        if let (Some(Value::Object(Some(receiver))), Some(Value::Object(Some(array)))) =
            (args.first(), args.get(1))
        {
            let receiver_name = match shared.mem.heap.get_field(*receiver, 0) {
                Value::Object(Some(name)) => read_java_string(&shared.mem.heap, name),
                _ => None,
            };
            let receiver_ordinal = shared.mem.heap.get_field(*receiver, 1);
            // One `class_manager` read for the whole scan (it took one per
            // element), and the class names compared as the shared `Arc<str>`
            // rather than copied into a `String` each. Nothing below runs Java
            // code or re-enters the class manager while it is held.
            let cm = shared.classes.class_manager.read();
            let receiver_class_name = cm
                .get_class(shared.mem.heap.class_id_of(*receiver))
                .map(|class| class.name.clone());
            for i in 0..shared.mem.heap.array_length(*array) {
                let Ok(Value::Object(Some(candidate))) =
                    shared.mem.heap.get_array_element(*array, i)
                else {
                    continue;
                };
                let candidate_name = match shared.mem.heap.get_field(candidate, 0) {
                    Value::Object(Some(name)) => read_java_string(&shared.mem.heap, name),
                    _ => None,
                };
                let candidate_ordinal = shared.mem.heap.get_field(candidate, 1);
                let candidate_class_name = cm
                    .get_class(shared.mem.heap.class_id_of(candidate))
                    .map(|class| class.name.clone());
                let same_name = candidate_name
                    .as_deref()
                    .zip(receiver_name.as_deref())
                    .is_some_and(|(candidate, receiver)| candidate == receiver);
                let same_ordinal = matches!(
                    (candidate_ordinal, receiver_ordinal),
                    (Value::Int(candidate), Value::Int(receiver)) if candidate == receiver
                );
                if candidate_class_name == receiver_class_name && (same_name || same_ordinal) {
                    thread.frames[frame_idx].stack.push(Value::Int(1))?;
                    return Ok(CachedCallResult::Handled);
                }
            }
        }
    }
    // GC-stale `java.lang.Thread`-mirror receiver recovery.
    //
    // A moving / promoting young GC can relocate a thread's
    // `java.lang.Thread` mirror while a *stale copy* of its old address still
    // sits in a running or blocked frame's operand stack / local — the
    // frame/operand remap-coverage gap documented in
    // `gc-blocked-thread-frame-stale-thread-mirror-RESOLVED.md`. The
    // registry and the per-thread `java_thread_obj` field are remapped, but
    // the frame copy is not, so an invoke whose receiver is that copy (the
    // classic `Thread.currentThread().getThreadGroup()` in
    // `TaskThreadFactory.<init>`) dispatches on a zeroed object and real-JDK
    // `Thread.getThreadGroup()` reads a null `holder` and NPEs (the Tomcat
    // `TestDigestAuthenticator` family). When the stale receiver's address is
    // a recorded former mirror address, recover that thread's live mirror.
    //
    // Precise / no false substitutions: the former-address table is populated
    // *only* by GC mirror relocations, and we consult it *only* when the
    // receiver header is genuinely all-zero. A vacated address uniquely
    // identified one thread's mirror, so identity is preserved, and we
    // additionally verify the recovered mirror is itself live before
    // substituting.
    //
    // "Genuinely all-zero" is a **16-byte header comparison, not
    // `class_id == 0`**, and the difference is this whole recovery's
    // correctness. `class_id == 0` is worn by three unrelated things (see
    // `memory::reclaim_guard`), and one of them is not stale at all:
    //
    //   **every primitive array.** `Instruction::Newarray` allocates with
    //   `ClassId::new(0)` because an array header carries its COMPONENT class
    //   id (JVMS §4.4.1) and `long[]`/`int[]`/`byte[]` have none.
    //
    // So the `class_id == 0` form matched a perfectly live `long[]` whose
    // address happened to appear in `former_mirror_addrs` — the young allocator
    // re-serves vacated addresses constantly — and replaced the receiver with a
    // `java.lang.Thread`. Measured on `org.h2.test.db.TestTempTables`: 5
    // occurrences in ~50 `--nojit` runs, every one of them
    // `java/util/Arrays.copyOf(long[], int)`'s `original.clone()` dispatching
    // into a thread mirror, surfacing as
    // `CloneNotSupportedException` from a `java/lang/Thread.clone` frame (the
    // mirror's inherited `Thread.clone` body) and costing two sessions on a
    // hunt for a GC bug that was not there. The substituted class was
    // `java/lang/Thread`, `jdk/internal/misc/InnocuousThread` and
    // `org/h2/mvstore/FileStore$BackgroundWriterThread` on different runs —
    // whichever mirror had previously occupied the address.
    // See `bug-h2-testtemptables-clonenotsupportedexception-thread-clone-frame-FIXED.md`.
    //
    // The all-zero test is exactly what the recovery was written for — the
    // Tomcat `TestDigestAuthenticator` case dispatches on a *zeroed* object —
    // and it is the same test the stale-pointer detector further down this
    // function already applies. An array cannot pass it: it carries a non-zero
    // `kind`, `element_type` and `shape`.
    //
    // The header test alone still leaves one address-collision window, and the
    // second filter below closes it. On the 16-byte header a bare
    // `new Object()` IS all-zero — `class_id` 0, `shape` 0 (no fields),
    // `ObjectKind::Object` and `ArrayElementType::Reference` both discriminant
    // 0, and the identity hash is minted lazily into the mark word rather than
    // stamped at allocation. So a zero-field `Object` sitting on a vacated
    // mirror address would still be swapped for a thread. That is far narrower
    // than "every primitive array", but it is the same defect, and it is closed
    // here by asking a question the header cannot answer: **is the recovered
    // mirror something this CALL SITE could legitimately be holding?**
    if !is_special {
        let recovered: Option<ObjectRef> = if let Value::Object(Some(recv)) = &args[0] {
            let recv = *recv;
            if receiver_is_a_reclaimed_span(shared, recv) {
                shared
                    .threads
                    .thread_registry
                    .recover_stale_mirror(recv.as_ptr() as usize)
                    .filter(|live| {
                        live.as_ptr() != recv.as_ptr()
                            && shared.mem.heap.class_id_of(*live) != ClassId::new(0)
                            && mirror_is_plausible_at_call_site(shared, &method_class_name, *live)
                    })
            } else {
                None
            }
        } else {
            None
        };
        if let Some(live) = recovered {
            args_root_guard.replace_object_arg(&args, 0, live);
            args[0] = Value::Object(Some(live));
        }
    }

    // CRATONVM_DBG_JETTY — trace every invoke into the Jetty launcher
    // package. The boot-test target (`java -jar start.jar --list-config`)
    // NPEs at `Main.start(Main.java:397)` on `args.getClasspath()`; this
    // logs the receiver value and the full argument list for every
    // `org/eclipse/jetty/start/` dispatch so the orchestrator's next run
    // shows exactly which call passes a null `StartArgs` (or whether the
    // `start(StartArgs)` overload is being confused with the no-arg
    // `start()` that reads the null `jsvcStartArgs` field).
    if crate::runtime::env_cache::dbg_jetty()
        && method_class_name.starts_with("org/eclipse/jetty/start/")
    {
        let recv_desc = match args.first() {
            Some(Value::Object(Some(r))) => {
                let cid = shared.mem.heap.class_id_of(*r);
                let cn = shared
                    .classes
                    .class_manager
                    .read()
                    .get_class(cid)
                    .map(|c| c.name.to_string())
                    .unwrap_or_else(|| format!("<cid {}>", cid.as_u32()));
                format!("Object({cn}@{:p})", r.as_ptr())
            }
            Some(Value::Object(None)) => "NULL".to_string(),
            other => format!("{other:?}"),
        };
        let arg_tail: Vec<String> = args
            .iter()
            .skip(1)
            .map(|v| match v {
                Value::Object(Some(r)) => {
                    let cid = shared.mem.heap.class_id_of(*r);
                    let cn = shared
                        .classes
                        .class_manager
                        .read()
                        .get_class(cid)
                        .map(|c| c.name.to_string())
                        .unwrap_or_else(|| format!("<cid {}>", cid.as_u32()));
                    format!("Object({cn})")
                }
                Value::Object(None) => "NULL".to_string(),
                other => format!("{other:?}"),
            })
            .collect();
        eprintln!(
            "[cratonvm-jetty] invoke{} {}.{}{} receiver={} args={:?}",
            if is_special { "special" } else { "virtual" },
            &*method_class_name,
            &*method_name,
            &*method_descriptor,
            recv_desc,
            arg_tail,
        );
    }

    // A lambda proxy cannot be the receiver of a verifier-valid private
    // instance call: its receiver must have the private method's declaring
    // class. Check the cheap proxy table first so the private-only resolution
    // walk below is not paid for every uncached SAM invocation.
    // `is_lambda_proxy_class` answers an ordinary receiver (every id below the
    // proxy id base) with one compare; this was a `lambda_proxies` read lock
    // and a hash probe on every slow-path virtual call.
    let is_lambda_proxy_receiver = if !is_special {
        match args.first() {
            Some(Value::Object(Some(obj_ref))) => shared
                .classes
                .is_lambda_proxy_class(shared.mem.heap.class_id_of(*obj_ref)),
            _ => false,
        }
    } else {
        false
    };

    let private_virtual_target = if !is_special
        && !is_lambda_proxy_receiver
        && matches!(args.first(), Some(Value::Object(Some(_))))
    {
        resolved_private_invokevirtual_target(
            shared,
            current_class_id,
            &method_class_name,
            &method_name,
            &method_descriptor,
        )
    } else {
        None
    };
    let effectively_special = is_special || private_virtual_target.is_some();
    // `--jdk-only`: an `invokeinterface` pinned to a PRIVATE interface method
    // is not cached. Its entry would be the unguarded special one, and a hit
    // skips the receiver-implements-the-interface check below, so a receiver
    // that does not implement the interface ran the private method once a
    // good receiver had filled the entry (`L2W37InterfaceSelectionHot`,
    // `private-recv-cold`; interpreter round i1 wave 37, lane L2). javac emits
    // the shape for a default method calling its interface's private helper,
    // which the JDK has in three interfaces; those sites take the slow path.
    let private_interface_pin_uncached =
        is_interface && !is_special && private_virtual_target.is_some() && shared.config.is_jdk_only();

    // Check for lambda proxy dispatch
    if !effectively_special {
        if let Value::Object(Some(obj_ref)) = &args[0] {
            let obj_class_id = shared.mem.heap.class_id_of(*obj_ref);
            if let Some(result) = try_lambda_dispatch(
                shared,
                thread,
                *obj_ref,
                obj_class_id,
                &method_name,
                &method_descriptor,
                &args[1..],
            )? {
                if let Some(value) = result {
                    let ret = crate::jit::return_type(&method_descriptor);
                    let value = coerce_value_for_return(value, ret);
                    // T18.K4 — tag-exact push for J/D lambda return values.
                    push_invoke_return_value(&mut thread.frames[frame_idx].stack, value)?;
                    // Hypothesis (b): lambda dispatch may have transitively
                    // run a native through `safe_native_call`; clear the
                    // pending-return field now that the value lives on the
                    // operand stack so it doesn't outlive the call site.
                    crate::vm::native_return_pushed_to_stack(shared, thread);
                }
                return Ok(CachedCallResult::Handled);
            }
        }
    }

    // Capture receiver class_id for virtual cache population.
    // Arrays are redirected to java/lang/Object, so skip caching for them
    // to avoid polluting the inline cache with the wrong target.
    let receiver_class_id = if !effectively_special {
        match &args[0] {
            Value::Object(Some(obj_ref)) => {
                if shared.mem.heap.kind_of(*obj_ref) == cratonvm_types::ObjectKind::Array {
                    None // Don't cache array dispatches — component class_id would conflict
                } else {
                    Some(shared.mem.heap.class_id_of(*obj_ref))
                }
            }
            _ => None,
        }
    } else {
        None
    };

    // JVMS §6.5 `invokeinterface`: "if the class of objectref does not
    // implement the resolved interface, invokeinterface throws an
    // IncompatibleClassChangeError" — even when the receiver happens to have a
    // same-named public method. Checked here, on the slow path, because every
    // cache entry a later hit can use is receiver-guarded and filled only
    // after this check passed for that receiver class. `receiver_does_not_
    // implement` answers only for a provable case (real class bytes on both
    // sides and throughout the receiver's hierarchy, no loader-split copy of
    // the interface), so compatibility shapes keep today's duck typing.
    if is_interface && !is_special {
        // A private interface method (javac 11+ emits `invokeinterface` for
        // one) makes the site effectively special and clears
        // `receiver_class_id`, but the receiver must still implement the
        // interface (interpreter round i1 wave 30,
        // `i29-L4-invokespecial-and-invokeinterface-selection-diverge-from-hotspot`
        // item 3): read its class here.
        let receiver_for_check = receiver_class_id.or_else(|| match &args[0] {
            Value::Object(Some(obj_ref))
                if private_virtual_target.is_some()
                    && shared.mem.heap.kind_of(*obj_ref) != cratonvm_types::ObjectKind::Array =>
            {
                Some(shared.mem.heap.class_id_of(*obj_ref))
            }
            _ => None,
        });
        if let (Some(receiver_id), Some(iface_id)) = (receiver_for_check, cp_resolved_class_id) {
            let message = if receiver_id != ClassId::new(0) {
                let cm = shared.classes.class_manager.read();
                crate::runtime::resolve::selection::receiver_does_not_implement(
                    &cm,
                    receiver_id,
                    iface_id,
                )
            } else if shared.config.is_jdk_only() {
                // Class id 0 is also a plain `java.lang.Object`, which the
                // dispatch below would send to the interface by name and run
                // its default method (interpreter round i1 wave 38, lane L6;
                // `L6W38InterfaceReceiverHot`, `object` rows).
                match &args[0] {
                    Value::Object(Some(obj_ref)) => {
                        let slots = shared.mem.heap.num_fields(*obj_ref);
                        let cm = shared.classes.class_manager.read();
                        crate::runtime::resolve::selection::object_receiver_does_not_implement(
                            &cm, slots, iface_id,
                        )
                    }
                    _ => None,
                }
            } else {
                None
            };
            if let Some(message) = message {
                return Err(crate::error::LinkageError::IncompatibleClassChangeError {
                    message,
                }
                .into());
            }
        }
        // An ARRAY receiver implements only `Cloneable` and `Serializable`:
        // it has no receiver class above (its header holds the component's),
        // so a default method of the interface ran on it (interpreter round
        // i1 wave 39, lane L2; `--jdk-only`, as the compiled twin in
        // `jit::helpers`). Array dispatches are never cached, so every call
        // of one reaches here.
        if receiver_for_check.is_none() && shared.config.is_jdk_only() {
            if let (Value::Object(Some(obj_ref)), Some(iface_id)) = (&args[0], cp_resolved_class_id)
            {
                if shared.mem.heap.kind_of(*obj_ref) == cratonvm_types::ObjectKind::Array {
                    let message = crate::runtime::interpreter::array_descriptor_of(shared, *obj_ref)
                        .and_then(|descriptor| {
                            let cm = shared.classes.class_manager.read();
                            crate::runtime::resolve::selection::array_receiver_does_not_implement(
                                &cm,
                                &descriptor,
                                iface_id,
                            )
                        });
                    if let Some(message) = message {
                        return Err(crate::error::LinkageError::IncompatibleClassChangeError {
                            message,
                        }
                        .into());
                    }
                }
            }
        }
    }

    // An array-typed call site whose receiver is not an array is a provable
    // invariant violation: the verifier guarantees the operand is `[J`, and
    // nothing in a well-formed heap turns a `[J` into a plain object. Report it
    // where both facts are still in hand — the dispatch below no longer looks at
    // the receiver's header at such a site, so without this the corruption would
    // pass through silently and surface later as a `ClassCastException` on the
    // `checkcast` that follows `clone()`.
    //
    // Costs one byte compare per non-special invoke on a name the caller has
    // already resolved; the body is reached only when the invariant is broken.
    // ... and an array-typed call site whose receiver is NULL owes the same
    // JEP 358 message every other `invokevirtual` owes. The receiver-driven
    // `match` below has a `Value::Object(None)` arm that builds it; the
    // array-typed branch is chosen BEFORE that match is reached, so a null
    // array receiver used to skip the null check entirely and fall through to
    // dispatch. `Object.clone()` is on `force_native_over_real_jdk_bytecode`'s
    // list, so it reached `native_object_clone`, whose own null arm can only
    // say `clone on null` — it is inside a native and has no bytecode context
    // to name the expression from. MEASURED on JDK 25.0.3+9, BOTH modes:
    //
    //   static String[] sa;  sa.clone()
    //     HotSpot   Cannot invoke "[Ljava.lang.String;.clone()"
    //                 because "NpeCloneProbe.sa" is null
    //     was       clone on null
    //
    // `arraylength` and `aaload` on the same null field were already right, so
    // this was the one null-deref opcode family on an array that was not.
    if !is_special && method_class_name.starts_with('[') {
        if matches!(args.first(), Some(Value::Object(None))) {
            let npe_msg = helpful_npe_invoke_message(
                shared,
                thread,
                frame_idx,
                &method_class_name,
                &method_name,
                &method_descriptor,
                num_params,
            );
            return Err(RuntimeError::NullPointerException {
                message: Some(npe_msg),
            }
            .into());
        }
        if let Some(Value::Object(Some(recv))) = args.first().copied() {
            if shared.mem.heap.kind_of(recv) != cratonvm_types::ObjectKind::Array {
                crate::memory::reclaim_guard::report_impossible_dispatch_terminal(
                    shared,
                    thread,
                    recv,
                    "array-typed call site, non-array receiver",
                    &format!("{method_class_name}.{method_name}{method_descriptor}"),
                    recv_before_refresh,
                );
            }
        }
    }

    // Determine the class to invoke on.
    // invoke_class: Arc<str> — cheap clone, derefs to &str for all downstream calls.
    let invoke_class: Arc<str> = if is_special {
        // JVMS §6.5 super-call redirect — see `invokespecial_owner_class_name`.
        // A no-op for constructors, private-method calls, and any call whose
        // CP-referenced class is not a genuine superclass of this frame's
        // own class; `method_class_name` passes straight through those.
        match invokespecial_owner_class_name(
            shared,
            current_class_id,
            cp_index,
            &method_class_name,
            &method_name,
            &method_descriptor,
        ) {
            Ok(owner) => owner,
            Err((exception_class, message)) => {
                // The calling method is never compiled (interpreter round i1
                // wave 36): a compiled `invokespecial` binds by the lenient
                // walk and would run the body this selection refuses
                // (`L4W35SpecialSelectionHot`). Bail-listing closes every
                // compile door (`compile_gate::admit`).
                {
                    let frame = &thread.frames[frame_idx];
                    crate::runtime::interpreter::jit_bridge::jit_verdicts(shared).mark_bail_listed(
                        frame.class_id,
                        frame.class_name(),
                        frame.method_name(),
                        frame.method_descriptor(),
                    );
                }
                return Err(
                    match crate::runtime::exceptions::create_exception_object(
                        shared,
                        thread,
                        exception_class,
                        Some(&message),
                    ) {
                        Ok(exc) => MethodCallFailed::ExceptionThrown(exc),
                        Err(e) => e,
                    },
                );
            }
        }
    } else if let Some((_declaring_id, declaring_name)) = &private_virtual_target {
        Arc::clone(declaring_name)
    } else if method_class_name.starts_with('[') {
        // The call SITE names an array type, so JVMS §4.4.1 settles the target
        // statically: an array class declares no methods of its own and its
        // method table is `java.lang.Object`'s. There is no subclass of `[J`
        // that could override `clone()`, so the receiver's header has nothing
        // to contribute and must not be consulted.
        //
        // The receiver-driven branch below reaches the same answer for a
        // receiver whose header says `kind == Array`, and that is the whole
        // 2026-07-31 fix. What it cannot do is answer correctly for a receiver
        // whose header does NOT say array — a block reclaimed while still
        // referenced and then re-served now describes whatever occupies it, and
        // `class_id_of` then picks that occupant's `clone()` body. That is how
        // `java/util/Arrays.copyOf(long[], int)`'s `original.clone()` — an
        // `invokevirtual "[J".clone:()Ljava/lang/Object;` — reached
        // `java.lang.Thread.clone`, whose body is `throw new
        // CloneNotSupportedException()` and nothing else, in
        // `bug-h2-testtemptables-clonenotsupportedexception-thread-clone-frame`.
        // Deciding from the call site instead of from the header removes that
        // route by construction, for every array-typed call site and every
        // reason a header might lie.
        //
        // A non-Object member at an array-typed call site cannot come from real
        // javac output; keep the CP name for it so the S111r8 synthetic-shape
        // rescue below (and `try_stackless_invoke`'s `[`-prefix rewrite) behave
        // exactly as before.
        if crate::vm::is_object_member(&method_name, &method_descriptor) {
            Arc::from("java/lang/Object")
        } else {
            method_class_name.clone()
        }
    } else {
        match &args[0] {
            Value::Object(Some(obj_ref)) => {
                // Arrays store the component class_id in their header, but
                // method dispatch must go through java.lang.Object (JVMS §4.4.1).
                // Check heap kind first to avoid misrouting clone()/toString()/etc.
                if shared.mem.heap.kind_of(*obj_ref) == cratonvm_types::ObjectKind::Array {
                    // S111r8: an Object[] array (cid=0 component class)
                    // being dispatched for a non-Object method like
                    // iterator()/hasNext()/size() typically means a
                    // synthetic native return-shape leaked into a
                    // typed-collection caller (e.g. HashSet.iterator
                    // bytecode read its `map` field which our synthetic
                    // HashSet stores as an Object[] backing array rather
                    // than a real HashMap). Object's vtable can't service
                    // these calls; falling back to the CP-resolved
                    // interface class lets the slow path's
                    // `check_override` list and the receiver-driven
                    // fallback in `invoke_on_class_shared_inner`
                    // recover. Object members (equals/hashCode/toString/
                    // clone/etc.) still dispatch via Object per
                    // JVMS §4.4.1.
                    if !crate::vm::is_object_member(&method_name, &method_descriptor) {
                        method_class_name.clone()
                    } else {
                        Arc::from("java/lang/Object")
                    }
                } else {
                    let cid = shared.mem.heap.class_id_of(*obj_ref);

                    // Stale pointer detection: if the header reads as all-zeros
                    // (class_id=0, kind=Object), the pointer likely targets
                    // zeroed-out GC from-space memory. Fall back to the constant
                    // pool method_ref class so dispatch has a chance to succeed.
                    if cid == ClassId::new(0) {
                        // H1: Stale-pointer detection. A `cid=0`+`fields=0`
                        // object — `new Object()` — used to produce an
                        // all-zero first 16 bytes that this detector could
                        // not distinguish from genuine stale memory. That
                        // has been fixed twice: first by minting an eager
                        // identity hash in `init_object_header`, which the
                        // 2026-08-06/07 header shrink undid when the hash
                        // moved into the mark word and went lazy; then by
                        // `GC_FLAG_HEADER` (2026-09-08), which puts the
                        // distinction in the header itself rather than in a
                        // value that has to be minted.
                        //
                        // The second fix is the durable one, and not only for
                        // this detector: an all-zero header is also
                        // unparseable by the young non-moving sweep's linear
                        // walk, which is what
                        // `h2-testvaluememory-system-gc-retained-every-empty-object-FIXED-20260908`
                        // is about.
                        //
                        // The detector still fires the warn! when the
                        // header is genuinely all-zero — a true
                        // stale-pointer regression — and falls back to
                        // the CP method-ref class so dispatch has a
                        // chance to succeed instead of NPE'ing.
                        // SAFETY: obj_ref is a live ObjectRef; its pointer is a valid heap address with at least a 16-byte readable header.
                        let header_bytes: [u8; 16] =
                            unsafe { std::ptr::read(obj_ref.as_ptr() as *const [u8; 16]) };
                        if header_bytes == [0u8; 16] {
                            // BUG-03 probe (gated CRATONVM_DBG_BUG03): when a stale
                            // (all-zero) invokevirtual receiver is seen, compare it to
                            // THIS thread's java_thread_obj field and the registry's
                            // remapped mirror — pinpoints whether the staleness is in
                            // the operand-stack copy, the per-thread field, or the
                            // registry (the moving-GC concurrent-spawn reclamation).
                            if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_BUG03").is_some()
                            {
                                let field = thread
                                    .java_thread_obj
                                    .map(|o| o.as_ptr() as usize)
                                    .unwrap_or(0);
                                let reg = shared
                                    .threads
                                    .thread_registry
                                    .java_thread_obj(thread.thread_id)
                                    .map(|o| o.as_ptr() as usize)
                                    .unwrap_or(0);
                                eprintln!(
                                    "[BUG03] stale recv={:p} on tid={} method={}.{} | java_thread_obj-field=0x{:x} registry-mirror=0x{:x} (field==recv:{} reg==recv:{})",
                                    obj_ref.as_ptr(), thread.thread_id.0, &*method_class_name, &*method_name,
                                    field, reg, field == obj_ref.as_ptr() as usize, reg == obj_ref.as_ptr() as usize,
                                );
                            }
                            // "Zeroed-a-live-object" detector consumer
                            // (CRATONVM_DBG_SWEEP_ZERO): this receiver lost its
                            // header to the non-moving young sweep. Recover its
                            // ORIGINAL class from the sweep ring so the
                            // root-coverage gap is NAMED (e.g. a reclaimed
                            // java/util/concurrent/ForkJoinTask whose only live
                            // ref was a register/native-stack root the sweep
                            // couldn't see — `CRATONVM_DBG_SWEEP_EDGES` silent).
                            if let Some((cid, kind, cycle, reason, initiator, blocked)) =
                                // Cast: object/code pointer to integer address
                                cratonvm_gc::gen_heap::sweep_zero_lookup(
                                        obj_ref.as_ptr() as usize
                                    )
                            {
                                // try_read (not read): this is a debug-only leaf
                                // path; never risk a re-entrant class_manager
                                // deadlock — fall back to the raw class_id.
                                let orig = shared
                                    .classes
                                    .class_manager
                                    .try_read()
                                    .and_then(|cm| {
                                        cm.class_store
                                            .get(cratonvm_types::ClassId::new(cid))
                                            .map(|c| c.name.to_string())
                                    })
                                    .unwrap_or_else(|| format!("class_id={cid}"));
                                // GC context (CRATONVM_DBG_MTROOTS): names the GC
                                // that reclaimed the live object so the
                                // initiator-vs-blocked-mutator root-coverage gap is
                                // pinned (reason 0 = unknown / gate off).
                                let reason_s = match reason {
                                    1 => "System.gc",
                                    2 => "alloc-young(maybe_gc)",
                                    3 => "forced-alloc(maybe_gc_forced)",
                                    _ => "unknown",
                                };
                                // Holder thread state (CRATONVM_DBG_MTROOTS): the
                                // detector fires ON the thread that holds the
                                // reclaimed ref. Log its id / blocked-flag / kind
                                // + call stack so we can see whether the holder
                                // was EXCLUDED from the STW (counted blocked while
                                // actually running) — the multi-thread root gap.
                                let holder_blocked = thread
                                    .gc_block_state
                                    .in_blocked_region
                                    .load(std::sync::atomic::Ordering::Acquire);
                                let mut stk = String::new();
                                {
                                    use std::fmt::Write as _;
                                    for f in thread.frames.iter().rev().take(8) {
                                        let _ = write!(
                                            stk,
                                            "\n[sweep-zero]     at {}.{}",
                                            f.class_name(),
                                            f.method_name()
                                        );
                                    }
                                }
                                eprintln!(
                                    "[sweep-zero] RECLAIMED-LIVE receiver ptr={:p}: original \
                                     class={} (class_id={} kind=0x{:02x}), zeroed by non-moving \
                                     sweep cycle {}; invoked as {}.{} — the live ref was a \
                                     register/native-stack root the marker missed \
                                     [gc reason={} initiator_tid={} blocked_threads={}] \
                                     [holder tid={} in_blocked={} kind={:?}]{}",
                                    obj_ref.as_ptr(),
                                    orig,
                                    cid,
                                    kind,
                                    cycle,
                                    &*method_class_name,
                                    &*method_name,
                                    reason_s,
                                    initiator,
                                    blocked,
                                    thread.thread_id.0,
                                    holder_blocked,
                                    thread.kind,
                                    stk,
                                );
                                // A2 forensic breadcrumb (CRATONVM_DBG_A2): was this
                                // address EVER header-written by an allocator, with
                                // what class/size? Distinguishes never-allocated /
                                // allocated-then-clobbered / mid-object (double-
                                // allocation or free-list overlap) for the reclaimed
                                // victim — the observed victims had ALREADY all-zero
                                // headers at sweep time, which the sweep record alone
                                // cannot explain.
                                let victim_addr = obj_ref.as_ptr() as usize;
                                let hist = cratonvm_gc::a2dbg::history_at(victim_addr, 12);
                                if hist.is_empty() {
                                    eprintln!(
                                        "[sweep-zero]   [A2] NO event touches {victim_addr:#x} (never header-written here, ring wrapped, or CRATONVM_DBG_A2 off)",
                                    );
                                } else {
                                    for r in hist {
                                        if r.kind == 0xFF {
                                            eprintln!(
                                                "[sweep-zero]   [A2] seq={} FREE @{:#x}",
                                                r.seq, r.addr,
                                            );
                                        } else {
                                            eprintln!(
                                                "[sweep-zero]   [A2] seq={} ALLOC @{:#x} class_id={} kind={} et={} alen={} ns={} size={}{}",
                                                r.seq, r.addr, r.class_id, r.kind, r.element_type,
                                                r.array_length, r.num_slots, r.size,
                                                if r.addr != victim_addr { " (covering)" } else { "" },
                                            );
                                        }
                                    }
                                }
                            }
                            // A bare `new Object()` is no longer all-zero, so
                            // the `java/lang/Object` demotion this block used to
                            // carry is GONE.
                            //
                            // The history is worth keeping, because it is the
                            // same defect twice. `ObjectHeader::new` leaves
                            // `MARK_NEUTRAL`, `ObjectKind::Object` and
                            // `ArrayElementType::Reference` at `0`, and a
                            // no-field `Object` has `class_id = 0` and
                            // `shape = 0` — so every one of the 16 bytes this
                            // detector reads was zero for a healthy, freshly
                            // allocated `java.lang.Object`. An eager identity
                            // hash used to hide that; the 2026-08-06/07 header
                            // shrink folded the hash into the mark word and made
                            // it lazy, and the false positive came back at a
                            // 100% rate — six lines of Java, all four
                            // collectors. This site's answer was to demote the
                            // warn to debug for `Object`-declared call sites,
                            // with the trade stated explicitly: a genuinely
                            // stale receiver there logs at debug instead.
                            //
                            // `GC_FLAG_HEADER` (2026-09-08) removed the premise
                            // instead. A published header is never sixteen zero
                            // bytes now, so `header_bytes == [0u8; 16]` means
                            // what this detector always wanted it to mean, and
                            // the trade is no longer worth making: by the old
                            // comment's own reasoning it was only worth it
                            // "against a 100% false-positive rate here". Zero
                            // "Stale pointer detected" lines across the 92-vector
                            // regression suite on all three collectors and the
                            // H2 corpus after the change.
                            //
                            // `java/lang/ClassLoader` KEEPS its demotion — it
                            // was never about the all-zero-by-design shape.
                            // WildFly / JBoss Modules hits this path on
                            // `ClassLoader`-typed invokevirtual sites when a
                            // receiver has genuinely lost its header but CP
                            // resolution already says `java/lang/ClassLoader`;
                            // the CP fallback succeeds and the WARN was mostly
                            // noise.
                            if method_class_name.as_ref() == "java/lang/ClassLoader" {
                                tracing::debug!(
                                    "Stale pointer detected in invokevirtual receiver \
                                     (ptr={:p}, all-zero header) — falling back to CP class {}",
                                    obj_ref.as_ptr(),
                                    &*method_class_name,
                                );
                            } else {
                                tracing::warn!(
                                    "Stale pointer detected in invokevirtual receiver \
                                     (ptr={:p}, all-zero header) — falling back to CP class {}",
                                    obj_ref.as_ptr(),
                                    &*method_class_name,
                                );
                            }
                            // T-DBG: opt-in deep diagnostic. Dump the Java call
                            // stack and the per-frame locals/operand slots that
                            // reference this stale address so we can see HOW
                            // the bad pointer arrived in the receiver slot.
                            if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_STALE_RECV")
                                .is_some()
                            {
                                // Cast: object/code pointer to integer address
                                let stale_addr = obj_ref.as_ptr() as usize;
                                // Extend CRATONVM_DBG_STALE_RECV with the A2
                                // allocation breadcrumb regardless of whether
                                // the non-moving sweep_zero ring has a match
                                // (it never will under the default MOVING
                                // young collector -- sweep_zero only records
                                // the non-moving-sweep code path, which the
                                // moving collector never runs). Found during
                                // the 2026-07-16 hib-aqs-livelock
                                // investigation: the moving-collector case
                                // needed its own always-on history dump to
                                // rule out a double-free/overlap explanation
                                // (it was in fact a stale native-caller
                                // reference surviving a legitimate
                                // relocation -- see
                                // initialize_real_thread_pool_executor).
                                if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_A2")
                                    .is_some()
                                {
                                    let hist = cratonvm_gc::a2dbg::history_at(stale_addr, 16);
                                    if hist.is_empty() {
                                        eprintln!(
                                            "[stale-recv] [A2] NO event touches {stale_addr:#x}"
                                        );
                                    } else {
                                        for r in hist {
                                            if r.kind == 0xFF {
                                                eprintln!(
                                                    "[stale-recv] [A2] seq={} FREE @{:#x}",
                                                    r.seq, r.addr
                                                );
                                            } else {
                                                eprintln!("[stale-recv] [A2] seq={} ALLOC @{:#x} class_id={} kind={} et={} alen={} ns={} size={}", r.seq, r.addr, r.class_id, r.kind, r.element_type, r.array_length, r.num_slots, r.size);
                                            }
                                        }
                                    }
                                }
                                eprintln!(
                                    "[stale-recv] ptr=0x{:x} method={}.{}{} — Java frames:",
                                    stale_addr,
                                    &*method_class_name,
                                    &*method_name,
                                    &*method_descriptor,
                                );
                                {
                                    let blocked_flag = thread
                                        .gc_block_state
                                        .in_blocked_region
                                        .load(std::sync::atomic::Ordering::Acquire);
                                    eprintln!(
                                        "[stale-recv] holder tid={} blocked={} epoch={} kind={:?}",
                                        thread.thread_id.0,
                                        blocked_flag,
                                        shared.mem.heap.collection_count(),
                                        thread.kind,
                                    );
                                    for (e, moved_to, mlen, as_dest) in
                                        crate::memory::gc::gcpart_probe(stale_addr)
                                    {
                                        eprintln!(
                                            "[stale-recv] [gcpart] epoch={e} map_len={mlen} moved_to={moved_to:x?} appears_as_dest={as_dest}"
                                        );
                                    }
                                    for (ago, site) in push_prov_find(stale_addr) {
                                        eprintln!(
                                            "[stale-recv] [pushprov] pushed {ago} invoke-returns ago at {site}"
                                        );
                                    }
                                    for (ago, desc) in deposit_gap_find(stale_addr) {
                                        eprintln!(
                                            "[stale-recv] [deposit-gap] {ago} entries ago: {desc}"
                                        );
                                    }
                                    for (age, site, tag, s, l) in
                                        cratonvm_gc::zero_forensics::probe(stale_addr)
                                    {
                                        eprintln!(
                                            "[stale-recv] [zeroed] age={age} site={} tag={tag} range=0x{s:x}+0x{l:x}",
                                            if site == 1 { "sweep-span" } else { "fromspace-reset" },
                                        );
                                    }
                                    if remap_trace_on() {
                                        eprintln!(
                                            "[stale-recv] [trace]\n    {}",
                                            remap_trace_dump()
                                        );
                                        for (ago, cb, site) in nret_find(stale_addr) {
                                            eprintln!(
                                                "[stale-recv] [nret] returned {} native-returns ago by {} at {}",
                                                ago,
                                                cratonvm_native_api::native_ring::name_of(cb)
                                                    .unwrap_or_else(|| format!("<cb@{cb:#x}>")),
                                                site,
                                            );
                                        }
                                        for (ago, parent, fidx) in getfield_ring_find(stale_addr) {
                                            let cur = shared
                                                .mem
                                                .heap
                                                .is_object_address(parent)
                                                .map(|p| {
                                                    format!(
                                                        "{:?}",
                                                        shared.mem.heap.get_field(p, fidx)
                                                    )
                                                })
                                                .unwrap_or_else(|| "<parent-not-obj>".into());
                                            eprintln!(
                                                "[stale-recv] [getfield] pushed {} getfields ago from parent=0x{parent:x} fld[{fidx}] — parent's field NOW = {cur}",
                                                ago,
                                            );
                                        }
                                    }
                                }
                                for (fi, f) in thread.frames.iter().enumerate().rev().take(30) {
                                    eprintln!(
                                        "  [{}] {}.{}{} pc={}",
                                        fi,
                                        f.class_name(),
                                        f.method_name(),
                                        f.method_descriptor(),
                                        f.pc,
                                    );
                                    if let Some(loc) = f.dbg_locate_addr(stale_addr) {
                                        eprintln!("      LOCATE {loc}");
                                    }
                                    if fi + 3 >= thread.frames.len() {
                                        eprintln!("      RAWSTACK{}", f.dbg_stack_dump());
                                        eprintln!("      POPPED{}", f.stack.dbg_dump_popped(5));
                                    }
                                    for li in 0..f.locals_len() {
                                        // Cast: numeric/representation conversion
                                        let v = f.get_local(li as u16);
                                        if let Value::Object(Some(o)) = v {
                                            // Cast: object/code pointer to integer address
                                            let addr = o.as_ptr() as usize;
                                            let marker =
                                                if addr == stale_addr { "STALE" } else { "" };
                                            // Probe the header of every Object
                                            // local: a non-zero hash means it
                                            // looks live, all-zero means it
                                            // shares the stale fate.
                                            // SAFETY: `addr` is the heap address of a live Object local; reading its 16-byte header is valid.
                                            let bytes: [u8; 16] =
                                                unsafe { std::ptr::read(addr as *const [u8; 16]) };
                                            let all_zero = bytes == [0u8; 16];
                                            eprintln!(
                                                "      LOCAL[{}] -> 0x{:x} all_zero_header={} {}",
                                                li, addr, all_zero, marker,
                                            );
                                        }
                                    }
                                    for si in 0..f.stack.len() {
                                        let v = f.stack.get_value(si);
                                        if let Value::Object(Some(o)) = v {
                                            // Cast: object/code pointer to integer address
                                            let addr = o.as_ptr() as usize;
                                            let marker =
                                                if addr == stale_addr { "STALE" } else { "" };
                                            // SAFETY: `addr` is the heap address of a live Object stack slot; reading its 16-byte header is valid.
                                            let bytes: [u8; 16] =
                                                unsafe { std::ptr::read(addr as *const [u8; 16]) };
                                            let all_zero = bytes == [0u8; 16];
                                            eprintln!(
                                                "      STACK[{}] -> 0x{:x} all_zero_header={} {}",
                                                si, addr, all_zero, marker,
                                            );
                                        }
                                    }
                                }
                            }
                            method_class_name.clone()
                        } else {
                            // S111r8: cid=0 with non-zero header means a
                            // synthetic alloc lost its class_id (e.g.
                            // `alloc_object(ClassId::new(0), …)` from a
                            // native fallback). The previous code returned
                            // bare `java/lang/Object`, which then sent
                            // `Set.iterator()` / `Map.keySet()` /
                            // `Iterator.hasNext()` invokes through
                            // Object's vtable and surfaced as
                            // `NoSuchMethodError Object.iterator()`.
                            //
                            // The CP method-ref class (e.g.
                            // `java/util/Set`) already resolved at link
                            // time and is the correct dispatch class for
                            // any non-Object method. Use it as the
                            // fallback so the slow path can locate the
                            // registered native (`HashSet.iterator`,
                            // `HashMap.keySet`, etc.) even though the
                            // receiver header is corrupt. Object members
                            // (equals/hashCode/toString/getClass/wait/
                            // notify/notifyAll/clone/finalize) still
                            // dispatch on Object so subclass overrides
                            // through the slow path's Object-fallback
                            // logic still apply.
                            if crate::vm::is_object_member(&method_name, &method_descriptor) {
                                Arc::from("java/lang/Object")
                            } else {
                                method_class_name.clone()
                            }
                        }
                    } else {
                        // If receiver is a lambda proxy calling a non-SAM method
                        // (e.g. Function.andThen), dispatch on the functional
                        // interface class so the native default method is found.
                        let lambda_iface = {
                            let proxies = shared.classes.lambda_proxies.read();
                            proxies
                                .get(&cid)
                                .map(|lcs| lcs.functional_interface.clone())
                        };
                        if let Some(iface) = lambda_iface {
                            iface
                        } else {
                            // S111r12 — receiver's runtime class is an interface
                            // (e.g. `java/lang/Comparable`) but the CP method-ref
                            // class is a concrete/abstract class with the actual
                            // method declared (`java/lang/ClassLoader.loadClass`).
                            // This pattern surfaces when a native-allocated
                            // ClassLoader instance lost its concrete class_id
                            // somewhere in the boot chain and `class_id_of`
                            // returns a stub interface cid instead. Routing
                            // dispatch through the CP class lets the slow path
                            // find the registered native or bytecode method.
                            // Mirrors the S111r8 cid=0 → CP-class fallback.
                            // Guard: only fires when the receiver's class is an
                            // interface AND the CP class is NOT that same
                            // interface (avoid changing well-formed
                            // `Iterator.hasNext()` etc. dispatches).
                            //
                            // S-trinity #2 — symmetric extension: receiver's
                            // runtime class is plain `java/lang/Object` (e.g.
                            // a value just returned from
                            // `PrivilegedAction.run()` whose declared return
                            // is `Object`, or a synthetic native return that
                            // landed without subclass info), and the CP
                            // method-ref class is `java/lang/ClassLoader` (or
                            // any concrete class declaring the method). Treat
                            // it the same as the interface case so the
                            // `(ClassLoader) priv.run()` chain in
                            // `LoaderUtil.getClassLoader` and
                            // `Logger.getMessageLogger` can dispatch the
                            // subsequent `loadClass` instead of NSME'ing on
                            // `Object.loadClass`.
                            let cm_read = shared.classes.class_manager.read();
                            let recv_class = cm_read.get_class(cid);
                            let recv_is_iface =
                                recv_class.map(|c| c.is_interface()).unwrap_or(false);
                            let recv_name_opt = recv_class.map(|c| Arc::from(&*c.name));
                            drop(cm_read);
                            let recv_is_bare_object = recv_name_opt
                                .as_ref()
                                .map(|n: &Arc<str>| &**n == "java/lang/Object")
                                .unwrap_or(false);
                            let cp_is_not_object = &*method_class_name != "java/lang/Object";
                            if (recv_is_iface || recv_is_bare_object)
                                && cp_is_not_object
                                && !crate::vm::is_object_member(&method_name, &method_descriptor)
                                && recv_name_opt
                                    .as_ref()
                                    .map(|n: &Arc<str>| &**n != &*method_class_name)
                                    .unwrap_or(true)
                            {
                                method_class_name.clone()
                            } else {
                                recv_name_opt.unwrap_or(method_class_name)
                            }
                        }
                    }
                }
            }
            Value::Object(None) => {
                if crate::runtime::env_cache::dbg_jetty2() {
                    let cm = shared.classes.class_manager.read();
                    eprintln!(
                        "[jetty2] NULL-RECEIVER invoke {}.{}{} — Java stack:",
                        &*method_class_name, &*method_name, &*method_descriptor
                    );
                    for (i, f) in thread.frames.iter().enumerate().rev().take(20) {
                        let cn = cm
                            .get_class(f.class_id)
                            .map(|c| c.name.to_string())
                            .unwrap_or_default();
                        eprintln!(
                            "  [{i}] {}.{}{} pc={}",
                            cn,
                            f.method_name(),
                            f.method_descriptor(),
                            f.pc
                        );
                    }
                }
                // Every mode throws the JEP 358 NPE here, as HotSpot does. The
                // `Compatible`-mode null-receiver shims that used to answer
                // instead (`Unsafe` dispatched on the constant-pool class,
                // `Class` / `URL` / `File` accessors answering null, 0 or
                // false) fired zero times in the wave-9 census and were
                // deleted; see
                // docs/internal/fixed-bugs/interpreter-L3-null-receiver-shims-answer-instead-of-npe-FIXED-20260925.md.
                {
                    // T19.H11 — diagnostic eprintln removed; the JmxProperties
                    // boot path NPE was traced to
                    // `DefaultLoggerFinder.isSystem(Module m)` with m=null
                    // (m.getClassLoader() NPEs). Fix lives in
                    // `native-builtins/src/lib.rs` as a native override that
                    // treats null module as `isSystem=true`.
                    if crate::runtime::env_cache::npe_invoke_dbg() {
                        eprintln!(
                            "[NPE-DBG] invokevirtual null receiver: {}.{}",
                            method_class_name, method_name
                        );
                    }
                    // NOTE: `null.length()` MUST throw NullPointerException
                    // per the JVM spec — `invokevirtual` null-checks the
                    // receiver before dispatch. An earlier hack here returned
                    // 0 for a "Liberty install-root" path, but that silently
                    // corrupted every other caller: e.g. `new
                    // StringTokenizer((String) null, ...)` does `str.length()`
                    // in its constructor, so the hack produced an empty
                    // tokenizer instead of an NPE, which made OSGi
                    // `Version`'s `nextToken()` throw NoSuchElementException
                    // and surface as `IllegalArgumentException: invalid
                    // version "null"` (Felix framework bootstrap). The hack
                    // is removed so the spec-compliant NPE below fires.
                    if crate::runtime::env_cache::modstatic_dbg() && &*method_name == "set" {
                        let cm = shared.classes.class_manager.read();
                        eprintln!("MODSTATIC: NPE 'set on null' frames:");
                        for (i, f) in thread.frames.iter().enumerate().rev().take(12) {
                            let cn = cm
                                .get_class(f.class_id)
                                .map(|c| c.name.to_string())
                                .unwrap_or_default();
                            eprintln!("  [{i}] {}.{} pc={}", cn, f.method_name(), f.pc);
                        }
                    }
                    // CRATONVM_DBG_JETTY — dump the full Java call stack when a
                    // null-receiver invokevirtual fires on a Jetty launcher
                    // method (the boot-test NPE site). This shows which frame /
                    // pc loaded the null receiver — e.g. `Main.start` doing
                    // `aload_1` of a null `StartArgs` param.
                    if crate::runtime::env_cache::dbg_jetty()
                        && method_class_name.starts_with("org/eclipse/jetty/start/")
                    {
                        let cm = shared.classes.class_manager.read();
                        eprintln!(
                            "[cratonvm-jetty] NULL-RECEIVER invoke {}.{}{} — Java stack:",
                            &*method_class_name, &*method_name, &*method_descriptor
                        );
                        for (i, f) in thread.frames.iter().enumerate().rev().take(20) {
                            let cn = cm
                                .get_class(f.class_id)
                                .map(|c| c.name.to_string())
                                .unwrap_or_default();
                            eprintln!(
                                "  [{i}] {}.{}{} pc={}",
                                cn,
                                f.method_name(),
                                f.method_descriptor(),
                                f.pc
                            );
                        }
                    }
                    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_NPE_STACK").is_some() {
                        let cm = shared.classes.class_manager.read();
                        eprintln!(
                            "[CRATONVM_DBG_NPE_STACK] NPE invoke {}.{}{} — stack:",
                            &*method_class_name, &*method_name, &*method_descriptor
                        );
                        for (i, f) in thread.frames.iter().enumerate().rev().take(30) {
                            let cn = cm
                                .get_class(f.class_id)
                                .map(|c| c.name.to_string())
                                .unwrap_or_default();
                            let loader = cm.get_loader_id(f.class_id);
                            eprintln!(
                                "  [{i}] {}.{}{} pc={} class_id={:?} loader={:?}",
                                cn,
                                f.method_name(),
                                f.method_descriptor(),
                                f.pc,
                                f.class_id,
                                loader
                            );
                        }
                    }
                    // JEP 358: build the HotSpot-style extended NPE message —
                    // `Cannot invoke "Owner.name(params)" because "<expr>" is
                    // null`. The action half is always present; the `because`
                    // clause is added when the bounded backward bytecode
                    // analysis can name the null receiver expression
                    // (aload local/param, getfield, getstatic, aaload). The
                    // trapping bci is `last_instr_pc`, always known here in the
                    // interpreter, so this is deopt-independent.
                    let npe_msg = helpful_npe_invoke_message(
                        shared,
                        thread,
                        frame_idx,
                        &method_class_name,
                        &method_name,
                        &method_descriptor,
                        num_params,
                    );
                    return Err(RuntimeError::NullPointerException {
                        message: Some(npe_msg),
                    }
                    .into());
                }
            }
            _ => method_class_name,
        }
    };

    // Dynamic proxy dispatch: forward interface method calls on
    // `Proxy$Instance` (and any class extending it — WP2.5-A generated
    // `$ProxyN` classes) to the InvocationHandler.invoke(). Handle Object
    // methods specially. Fast path stays a literal compare; slow path
    // walks the receiver's superclass chain — only fires when
    // `invoke_class != "Proxy$Instance"` and we have an actual receiver
    // to inspect, so the cost is zero on every non-proxy dispatch.
    // Only a non-special call can be a proxy dispatch, so the chain walk (a
    // `class_manager` read per slow-path invoke) is skipped for special calls.
    //
    // Round 13 wave 4 (lane proxy2): a call the receiver's generated class
    // declares runs that class's body, as a compiled caller's does, and
    // `Object`'s final `wait`/`notify`/`notifyAll` run `Object`'s; see
    // `proxy_call_takes_ordinary_dispatch`.
    let is_proxy_dispatch = !is_special
        && (&*invoke_class == "java/lang/reflect/Proxy$Instance"
            || matches!(
                args.first(),
                Some(Value::Object(Some(receiver)))
                    if class_chain_reaches_proxy_instance(
                        shared,
                        shared.mem.heap.class_id_of(*receiver),
                    )
            ))
        && !proxy_call_takes_ordinary_dispatch(shared, &args, &method_name, &method_descriptor);
    if is_proxy_dispatch {
        // Handle getClass() directly — return the proxy's class mirror
        if &*method_name == "getClass" {
            if let Value::Object(Some(proxy_ref)) = &args[0] {
                let class_id = shared.mem.heap.class_id_of(*proxy_ref);
                // `args` is pinned (`args_root_guard`) and nothing below reads
                // it, so the mirror may collect: on a full heap its first use
                // throws `OutOfMemoryError` (gen r5w1/oom5).
                let mirror = super::constants::class_mirror_or_oom(
                    shared,
                    thread,
                    class_id,
                    "proxy-getClass",
                )?;
                thread.frames[frame_idx]
                    .stack
                    .push(Value::Object(Some(mirror)))?;
                return Ok(CachedCallResult::Handled);
            }
        }
        if let Value::Object(Some(proxy_ref)) = &args[0] {
            let result = crate::vm::proxy_invoke_handler_shared(
                shared,
                thread,
                *proxy_ref,
                &method_name,
                &method_descriptor,
                &args[1..],
            )?;
            if let Some(value) = result {
                // G24-1. This used to be five copies of one `if let
                // Value::Object(Some(obj)) = value { get_field(obj, 0) } else
                // { value }`, one per return-descriptor group, and it was the
                // whole of the return coercion on the live proxy path. Two
                // things were wrong with it and neither was the duplication:
                // `Value::Object(None)` fell into the `else` arm and was pushed
                // as the primitive return value where HotSpot throws
                // `NullPointerException` (8 measured rows), and slot 0 was read
                // off WHATEVER object arrived with no wrapper-class check at
                // all, so 18 more measured rows silently succeeded — three of
                // them by reinterpreting an `int` payload as float bits.
                //
                // Both now live in `vm::proxy_coerce_handler_return`, applied
                // inside `proxy_invoke_handler_shared` at the points where the
                // USER's handler result comes back. That placement is
                // deliberate: the AnnotationProxy arm of that function returns
                // earlier and keeps the lenient unbox, so annotation member
                // data — which is the VM's own bookkeeping, not a value any
                // Java code chose — cannot be refused by the strict contract.
                //
                // So by the time a value reaches this line it has already been
                // coerced, by one arm or the other, and is a raw JVM value for
                // a primitive return. The lenient helper is still called rather
                // than dropped, because it is a no-op on an already-raw value
                // and this is the documented boundary between the shared
                // dispatch hook and the interpreter's operand stack.
                let unboxed = crate::vm::proxy_unbox_primitive_return(
                    shared,
                    &method_descriptor,
                    Ok(Some(value)),
                )?
                .unwrap_or(value);
                let ret = crate::jit::return_type(&method_descriptor);
                let pushed = coerce_value_for_return(unboxed, ret);
                // T18.K4 — tag-exact push for J/D proxy return values.
                push_invoke_return_value(&mut thread.frames[frame_idx].stack, pushed)?;
            }
            return Ok(CachedCallResult::Handled);
        }
    }

    // H2-CID0, CLONE face (2026-08-03). `java.lang.Thread.clone()` is
    // `new CloneNotSupportedException / dup / invokespecial / athrow` and
    // nothing else, so a dispatch that lands there is never something the
    // program asked for: no library clones a Thread. Every observed arrival is
    // a receiver whose header does not say what the caller's reference should
    // point at — `Arrays.copyOf(long[], int)` doing `original.clone()` on a
    // `long[]`, reaching `Thread.clone` instead of the array clone.
    //
    // Two different defects produce it, and this reporter is what tells them
    // apart. Array receivers carry their COMPONENT class id in the header, and
    // dispatching on that without an array check routes `someArray.m()` into
    // the component class's body — the defect fixed 2026-07-31 (see
    // `bug-h2-testtemptables-clonenotsupportedexception-thread-clone-frame-FIXED.md`).
    // The other is this family: the receiver's block was reclaimed while still
    // referenced and re-served, so the header now describes whatever occupies
    // it. Only the second leaves a record in the reclamation rings, and
    // `report_reclaimed_receiver` asks them.
    //
    // Costs nothing on a healthy run: the whole check is two string compares
    // that fail, and it is only reached at a dispatch terminal. See
    // `bug-h2-classid0-stale-address-family-FIXED.md`,
    // whose "what to try next" asked for exactly this — the two
    // `CloneNotSupportedException` occurrences it recorded produced no verdict
    // because nothing on the clone path consulted the heap.
    if &*invoke_class == "java/lang/Thread"
        && &*method_name == "clone"
        && &*method_descriptor == "()Ljava/lang/Object;"
    {
        if let Some(Value::Object(Some(recv))) = args.first().copied() {
            crate::memory::reclaim_guard::report_impossible_dispatch_terminal(
                shared,
                thread,
                recv,
                "Thread.clone dispatch",
                "java/lang/Thread.clone()Ljava/lang/Object;",
                None,
            );
        }
    }

    // Annotation proxy dispatch: method calls on annotation proxies
    //
    // S111r18 — gate the dispatch on `kind == Object`. A reference array
    // whose component class is `AnnotationProxy` (e.g. `Annotation[]` for
    // a repeatable annotation or `excludeFilters` on `@ComponentScan`)
    // shares the same `class_id_of` value because our heap stores the
    // component class id on the array header. Without this guard, every
    // method call on such an array (Object.getClass / Object.toString /
    // Array.getLength via reflection) gets routed through
    // `annotation_proxy_invoke_shared`, which reads element-value slots
    // out of array memory — surfacing as `getClass() returns null` or
    // wrong-component types in Spring's `MergedAnnotation.adaptForAttribute`
    // and breaking the `excludeFilters` array iteration that builds the
    // `MergedAnnotation[]`.
    if &*invoke_class == "java/lang/annotation/AnnotationProxy"
        && !is_special
        && matches!(
            args.first(),
            Some(Value::Object(Some(r))) if shared.mem.heap.kind_of(*r) == cratonvm_types::ObjectKind::Object
        )
    {
        if let Value::Object(Some(ann_ref)) = &args[0] {
            let invoke_args: &[Value] = if args.len() >= 1 { &args[1..] } else { &[] };
            let result = crate::vm::annotation_proxy_invoke_shared(
                shared,
                thread,
                *ann_ref,
                &method_name,
                invoke_args,
            )?;
            if let Some(value) = result {
                // Unbox the result if the method returns a primitive type.
                // The return token follows the `)` that CLOSES the parameter
                // list; the last `)` can sit inside a class name (`()LA)I;`).
                let ret_char = descriptor_return_ref(&method_descriptor)
                    .chars()
                    .next()
                    .unwrap_or('L');
                let unboxed = match ret_char {
                    'I' | 'Z' | 'B' | 'C' | 'S' | 'J' | 'F' | 'D' => {
                        if let Value::Object(Some(obj)) = value {
                            shared.mem.heap.get_field(obj, 0)
                        } else {
                            value
                        }
                    }
                    _ => value,
                };
                let ret = crate::jit::return_type(&method_descriptor);
                let pushed = coerce_value_for_return(unboxed, ret);
                // T18.K4 — tag-exact push for J/D annotation-proxy return values.
                push_invoke_return_value(&mut thread.frames[frame_idx].stack, pushed)?;
            }
            return Ok(CachedCallResult::Handled);
        }
    }

    // KAFKA-DEFAULT-RESCUE: stash the CP-resolved interface (or class) id
    // for the in-flight invoke. The default-method rescue at the NSME emit
    // site in `invoke_on_class_shared_inner` consults this slot when the
    // hierarchy walk on the receiver-derived dispatch class can't find the
    // method but the CP-resolved interface declares a default body.
    // Wrapped in an RAII guard so nested invokes restore the parent's slot.
    let _cp_iface_guard = crate::vm::PendingCpIfaceGuard::new(cp_resolved_class_id);

    if let Some(res) = intercept_force_registered_native(
        shared,
        thread,
        frame_idx,
        &invoke_class,
        method_name.as_ref(),
        method_descriptor.as_ref(),
        &args,
    ) {
        // A force-native virtual call used to return before the ordinary
        // stackless-dispatch epilogue below could warm `invoke_cache`.  That
        // made every subsequent call to common real-JDK overrides (notably
        // String's compact-string operations) re-enter full method
        // resolution, even though `populate_virtual_invoke_cache` already
        // has a redefine-aware `VirtualNative` representation for exactly
        // this dispatch.  Keep the private-target exclusion from the normal
        // virtual epilogue because such a call has a distinct resolution
        // identity.
        if !is_special && private_virtual_target.is_none() {
            if let Some(rcv_cid) = receiver_class_id {
                populate_virtual_invoke_cache(
                    thread,
                    shared,
                    current_class_id,
                    cp_index,
                    rcv_cid,
                    &args[0],
                    pc,
                );
            }
        }
        return res;
    }

    if let Some(res) = intercept_jython_pymodule_findattr(
        shared,
        thread,
        frame_idx,
        method_name.as_ref(),
        method_descriptor.as_ref(),
        receiver_class_id,
        &args,
    ) {
        return res;
    }

    if let Some(res) = intercept_jython_pyjavatype_findattr_ex(
        shared,
        thread,
        frame_idx,
        method_name.as_ref(),
        method_descriptor.as_ref(),
        receiver_class_id,
        is_special,
        &args,
    ) {
        return res;
    }

    // Inherited URLClassLoader methods invoked through a subclass-owned
    // constant-pool entry evade the static-class native gate.  Intercept the
    // resolved base methods so real-JDK URLClassPath shims never discard local
    // resources or custom URLStreamHandler-backed URLs.
    if let Some(res) = intercept_urlclassloader_subclass_native_method(
        shared,
        thread,
        frame_idx,
        method_name.as_ref(),
        method_descriptor.as_ref(),
        receiver_class_id,
        is_special,
        &args,
    ) {
        return res;
    }

    if let Some(res) = intercept_classloader_subclass_resource_native(
        shared,
        thread,
        frame_idx,
        method_name.as_ref(),
        method_descriptor.as_ref(),
        receiver_class_id,
        is_special,
        &args,
    ) {
        return res;
    }

    // Loader-isolation dispatch override (gated on CRATONVM_LOADER_AWARE_RESOLUTION).
    //
    // `invoke_class` is the receiver's class NAME, and the dispatch below
    // re-resolves that name via `get_loaded_class_id` — which returns ONE class
    // per name. Under a user loader two classes can share a name: a per-loader
    // ENHANCED copy (e.g. a Hibernate bytecode-enhanced entity — its
    // `$$_hibernate_*` methods, dirty-tracking, and field-access interception
    // overrides) AND the un-enhanced copy on the global classpath. Name
    // re-resolution silently picks the global copy, so a virtual/interface call
    // on an enhanced receiver would run the UN-enhanced method: `setX()` skips
    // dirty tracking (`[]` instead of `["x"]`), `$$_hibernate_*` is missing
    // (spurious NoSuchMethodError), etc. For a virtual/interface call the
    // receiver's OWN runtime `class_id` is the authoritative dispatch target, so
    // hand it to `try_stackless_invoke` as a dispatch-class override when it
    // diverges from the name-resolved class. This keeps the normal stackless
    // frame-push path (NO extra recursion), unlike routing through the recursive
    // `invoke_on_class_shared`. Gated + divergence-only → byte-identical in the
    // default (gate-off) / single-class-per-name case.
    // An invokeinterface default is part of the interface identity, not merely
    // its binary name. A forked receiver can implement a loader-local copy of
    // an interface while the calling class's constant-pool lookup finds the
    // application copy. Running that application's default method mixes its
    // static constants with the forked receiver (Spring's MergedAnnotation
    // Adapt enum is identity-sensitive). Prefer the receiver loader's exact
    // interface when it is a real superinterface of the receiver.
    //
    // BUT only when the receiver is actually going to fall through to that
    // interface default in the first place. If the receiver's OWN class
    // hierarchy already declares a concrete (class-level, non-interface)
    // override of this exact name+descriptor, that override must win —
    // redirecting `class_id` straight to the interface's per-loader copy
    // skips past the override and runs the interface default instead. Found
    // via `SpringBootContextLoaderAotTests` (`@CompileWithForkedClassLoader`):
    // `DelegatingSmartContextLoader` (loaded by the forked test classloader)
    // overrides `AotContextLoader.loadContextForAotProcessing(MergedContextConfiguration,
    // RuntimeHints)`, but this override redirected dispatch to the forked
    // loader's own copy of `AotContextLoader` — whose default body just calls
    // the 1-arg `loadContextForAotProcessing(MergedContextConfiguration)`
    // default, which unconditionally throws
    // `UnsupportedOperationException("Invoke loadContextForAotProcessing(...)
    // instead")`. Use `find_method_recursive` on the receiver's OWN class_id
    // (loader-accurate, unlike a name-based lookup) to check for a real
    // override before applying the redirect.
    let loader_interface_override =
        if is_interface && !is_special && crate::runtime::env_cache::loader_aware_resolution() {
            receiver_class_id.and_then(|receiver_id| {
                let cm = shared.classes.class_manager.read();
                let receiver_has_class_override = crate::classloading::find_method_recursive(
                    receiver_id,
                    &method_name,
                    &method_descriptor,
                    &cm.class_store,
                )
                .is_some_and(|(_, declaring_id)| {
                    !cm.get_class(declaring_id)
                        .is_some_and(|class| class.is_interface())
                });
                if receiver_has_class_override {
                    return None;
                }
                let receiver_loader = cm.get_loader_id(receiver_id)?;
                let exact =
                    cm.class_defined_by_loader_exact(&method_owner_name, receiver_loader)?;
                (Some(exact) != cm.get_loaded_class_id(&method_owner_name)
                    && cm
                        .get_class(exact)
                        .is_some_and(|class| class.is_interface()))
                .then_some(exact)
            })
        } else {
            None
        };
    let receiver_interface_dispatch = (is_interface && !is_special)
        .then_some(receiver_class_id)
        .flatten()
        .filter(|class_id| *class_id != ClassId::new(0))
        // `is_lambda_proxy_receiver` asked the same question of the same object.
        .filter(|_| !is_lambda_proxy_receiver);
    let dispatch_override: Option<ClassId> = if let Some((declaring_id, _)) =
        &private_virtual_target
    {
        Some(*declaring_id)
    } else if let Some(receiver_id) = receiver_interface_dispatch {
        // find_method_recursive performs the JVMS maximally-specific
        // default-method selection only when it starts at the runtime
        // receiver. Starting at the CP owner returns that interface's own
        // default immediately and bypasses a covariant bridge declared by a
        // subinterface implemented by the receiver.
        Some(receiver_id)
    } else if let Some(interface_id) = loader_interface_override {
        Some(interface_id)
    } else if !is_special && crate::runtime::env_cache::loader_aware_resolution() {
        // Lambda-proxy receiver invoking a non-SAM (default) interface
        // method: `invoke_class` was set to the functional interface NAME
        // (see the lambda_iface branch above), and the name re-resolution
        // below collapses to ONE copy per name. The proxy call site carries
        // the loader-resolved interface id captured at bootstrap time
        // (forked-classloader tests re-define the whole framework, so the
        // app copy and the fork copy both exist); dispatch on that id so the
        // default method executes in the DEFINING loader context and its own
        // constant-pool resolutions stay inside that loader (Spring AOT
        // `ArgumentCodeGenerator.and()` chain, 2026-07-15).
        let lambda_iface_override = receiver_class_id.and_then(|rcv_cid| {
            let iface_id = shared
                .classes
                .lambda_proxies
                .read()
                .get(&rcv_cid)
                .and_then(|lcs| lcs.functional_interface_id)?;
            let cm = shared.classes.class_manager.read();
            let iface_matches_invoke = cm
                .get_class(iface_id)
                .map(|c| &*c.name == &*invoke_class)
                .unwrap_or(false);
            (iface_matches_invoke && cm.get_loaded_class_id(&invoke_class) != Some(iface_id))
                .then_some(iface_id)
        });
        lambda_iface_override.or_else(|| {
            receiver_class_id.filter(|rcv_cid| {
                *rcv_cid != ClassId::new(0) && {
                    let cm = shared.classes.class_manager.read();
                    cm.get_loaded_class_id(&invoke_class) != Some(*rcv_cid)
                        && cm
                            .get_class(*rcv_cid)
                            .map(|c| &*c.name == &*invoke_class)
                            .unwrap_or(false)
                }
            })
        })
    } else if is_special && should_use_loader_initiated_resolution(shared, current_class_id) {
        // invokespecial owner is the CP-resolved class NAME (`method_class_name`),
        // which `get_loaded_class_id` collapses to ONE copy per name. Super and
        // private calls from inside a loader-private enhanced class must reach
        // that same loader's owner copy. For self-targets, the receiver is
        // more precise than the caller: a global harness class can execute
        // `new C; invokespecial C.<init>` where `new` correctly allocated a
        // loader-private enhanced C. Running the global C constructor against
        // that receiver writes the wrong layout slots and leaves enhanced fields
        // null.
        //
        // NOT restricted to `<init>`: invokespecial also dispatches private
        // instance methods and (from inside the declaring class) any method
        // called on `this`. A hidden class calling its OWN private method —
        // `invokespecial <ITSELF>.priv()`, same shape as the constructor case
        // — hits the identical owner-name mismatch (see
        // `is_self_class_reference`'s doc comment below) and must resolve the
        // same way. `is_self_class_reference` plus `find_method` already scope
        // this correctly: a real `super.foo()` call names a DIFFERENT class in
        // `invoke_class` (the superclass), which never equals the receiver's
        // own class name, so this override naturally stays inert for super
        // calls and only fires for genuine self-targets.
        let receiver_self_ctor = match args.first() {
            Some(Value::Object(Some(recv))) => {
                let recv_cid = shared.mem.heap.class_id_of(*recv);
                if recv_cid != ClassId::new(0) {
                    let cm = shared.classes.class_manager.read();
                    // `is_self_class_reference`, not raw `==`: a hidden
                    // class's own `<init>` names its owner via the
                    // CP's class-FILE name (`invoke_class`), while the
                    // receiver's stored name carries the VM-appended
                    // `"/0x<counter>"` suffix (JEP 371) — see
                    // `is_self_class_reference`'s doc comment. Exact
                    // equality can never match a hidden class's own
                    // constructor, which is why `invokespecial
                    // <ITSELF>.<init>` on a hidden class previously
                    // fell through to the name-based lookup below and
                    // raised an uncatchable `ClassNotFound` (see
                    // hidden-class-self-reference-door-census-20260912.md
                    // §5) — every door that first needs a constructed
                    // instance failed the same way.
                    let recv_matches_owner = cm
                        .get_class(recv_cid)
                        .map(|c| {
                            is_self_class_reference(&c.name, c.is_hidden(), &invoke_class)
                                && c.find_method(&method_name, &method_descriptor).is_some()
                        })
                        .unwrap_or(false);
                    if recv_matches_owner {
                        Some(recv_cid)
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            _ => None,
        };
        receiver_self_ctor.or_else(|| {
            lookup_loader_initiated(shared, current_class_id, &invoke_class).filter(|owner_cid| {
                *owner_cid != ClassId::new(0)
                    && shared
                        .classes
                        .class_manager
                        .read()
                        .get_loaded_class_id(&invoke_class)
                        != Some(*owner_cid)
            })
        })
    } else {
        None
    };

    // `is_special` first and the flag memoized: this ran a two-probe flag
    // lookup on EVERY slow-path invoke, special or not.
    fn dbg_invspecial() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| {
            cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_INVSPECIAL").is_some()
        })
    }
    if is_special && dbg_invspecial() {
        let cm = shared.classes.class_manager.read();
        let cur_loader = cm.get_loader_id(current_class_id);
        if matches!(
            cur_loader,
            Some(cratonvm_types::ClassLoaderId::UserDefined(_))
        ) {
            let invoke_class_resolved = cm.get_loaded_class_id(&invoke_class);
            drop(cm);
            eprintln!(
                "[INVSPECIAL] method={}.{}{} current_class_id={:?} cur_loader={:?} invoke_class={} dispatch_override={:?} invoke_class_resolved(global)={:?}",
                method_owner_name, method_name, method_descriptor, current_class_id, cur_loader, invoke_class, dispatch_override, invoke_class_resolved
            );
        }
    }
    if crate::runtime::env_cache::dbg_loader_trace()
        && (invoke_class.contains("RootReference") || &*method_name == "compareAndSetRoot")
    {
        let cm = shared.classes.class_manager.read();
        let invoke_class_resolved = cm.get_loaded_class_id(&invoke_class);
        let cur_loader = cm.get_loader_id(current_class_id);
        let recv_loader = receiver_class_id.and_then(|c| cm.get_loader_id(c));
        drop(cm);
        eprintln!(
            "[LOADER-TRACE] invoke_kind method={}.{}{} is_special={} invoke_class={} invoke_class_resolved={:?} receiver_class_id={:?} recv_loader={:?} current_class_id={:?} cur_loader={:?} dispatch_override={:?}",
            method_owner_name, method_name, method_descriptor, is_special, invoke_class, invoke_class_resolved, receiver_class_id, recv_loader, current_class_id, cur_loader, dispatch_override
        );
    }
    // JVMS §5.4.6 selection input for `try_stackless_invoke`'s step 4: the
    // receiver's own class and what this site resolved. Only a genuinely
    // virtual dispatch (not special, not a pinned private target) on a real
    // receiver has one.
    let selection_input = if effectively_special {
        None
    } else {
        receiver_class_id
            .filter(|id| *id != ClassId::new(0))
            .and_then(|receiver_id| {
                // An interface site's identity matters only for wording an
                // error; prefer the loader-aware interface id when the
                // caller-namespace lookup above found nothing.
                let resolved = cp_owner_resolved.or_else(|| {
                    cp_resolved_class_id.map(|iface_id| {
                        crate::runtime::resolve::selection::ResolvedRef::Interface(Some(iface_id))
                    })
                });
                resolved.map(|resolved| (receiver_id, resolved))
            })
    };
    // Class/interface resolution above may have triggered a moving collection
    // while `args` lived only in its Rust Vec. Re-read the remapped pin slots.
    args_root_guard.refresh(&mut args);
    // `--jdk-only` (interpreter round i1 wave 37, lane L4): an invoke-exact
    // `VarHandle` refuses a call site that is not its access mode type. Every
    // interpreted `VarHandle` access reaches this point (no site cache holds
    // one); a VM that minted no exact handle pays the relaxed load only.
    if let Some(exact_slot) = var_handle_exact_gate(shared) {
        if !is_special
            && crate::vm::vm_exec::is_var_handle_signature_polymorphic_receiver(
                &method_owner_name,
            )
        {
            if let Some(Value::Object(Some(vh))) = args.first().copied() {
                if var_handle_receiver_is_exact(shared, vh, exact_slot) {
                    if let Some(refusal) = var_handle_exact_refusal(
                        shared,
                        thread,
                        vh,
                        &method_name,
                        &method_descriptor,
                    ) {
                        return Err(refusal);
                    }
                }
            }
        }
    }
    // Try stackless frame push for bytecode methods (avoids Rust stack recursion)
    // For virtual/special calls, do NOT walk the native hierarchy — subclass
    // bytecode overrides must take priority over parent native overrides.
    match try_stackless_invoke(
        shared,
        thread,
        frame_idx,
        &invoke_class,
        &method_name,
        &method_descriptor,
        &args,
        false,
        is_special,
        dispatch_override,
        selection_input,
    )? {
        CachedCallResult::FramePushed => {
            args_root_guard.refresh(&mut args);
            if is_special || (private_virtual_target.is_some() && !private_interface_pin_uncached) {
                populate_invoke_cache(
                    thread,
                    shared,
                    current_class_id,
                    cp_index,
                    is_special,
                    false,
                    pc,
                    None,
                );
            } else if private_virtual_target.is_none() && loader_interface_override.is_none() {
                if let Some(rcv_cid) = receiver_class_id {
                    populate_virtual_invoke_cache(
                        thread,
                        shared,
                        current_class_id,
                        cp_index,
                        rcv_cid,
                        &args[0],
                        pc,
                    );
                }
            }
            return Ok(CachedCallResult::FramePushed);
        }
        CachedCallResult::Handled => {
            args_root_guard.refresh(&mut args);
            if is_special || (private_virtual_target.is_some() && !private_interface_pin_uncached) {
                populate_invoke_cache(
                    thread,
                    shared,
                    current_class_id,
                    cp_index,
                    is_special,
                    false,
                    pc,
                    None,
                );
            } else if private_virtual_target.is_none() && loader_interface_override.is_none() {
                if let Some(rcv_cid) = receiver_class_id {
                    populate_virtual_invoke_cache(
                        thread,
                        shared,
                        current_class_id,
                        cp_index,
                        rcv_cid,
                        &args[0],
                        pc,
                    );
                }
            }
            return Ok(CachedCallResult::Handled);
        }
        CachedCallResult::CacheMiss => {
            args_root_guard.refresh(&mut args);
            // Exotic case — fall through to recursive dispatch
        }
    }

    // Fallback: recursive dispatch for exotic cases (signature-polymorphic, JNI, proxy, etc.)
    // Honor the loader-isolation dispatch override here too: a divergent
    // receiver whose method the stackless path couldn't handle (synthetic stub /
    // exotic → CacheMiss) must still dispatch on the receiver's own class_id,
    // not the wrong name-resolved copy. Rare, so the recursive path is fine.
    //
    // `dispatch_override` alone under-detects this: it's computed by comparing
    // `get_loaded_class_id(&invoke_class)` against the receiver's class_id at
    // THIS point in time, but `invoke_shared`'s own `load_class_concurrent`
    // (name-based) can independently resolve the SAME name string to a
    // DIFFERENT ClassId than `get_loaded_class_id` just did — observed with
    // `@ClassPathOverrides`'s `ModifiedClassPathClassLoader`, where an old
    // override jar's class (e.g. Spring's `MimeType`, missing a method added
    // in later versions) and the main classpath's same-named class are BOTH
    // loaded, and the two name-resolution call sites picked different
    // copies. That let `mimeType.isMoreSpecific(null)` — a genuinely absent
    // method on the receiver's OWN class — silently dispatch to the OTHER
    // same-named class's bytecode instead of raising `NoSuchMethodError`
    // (`NoSuchMethodFailureAnalyzerTests`). For an ordinary virtual call, the
    // receiver's own (already-loaded, already-initialized) class_id is
    // JVMS-authoritative regardless of what any name lookup returns, so
    // prefer it whenever available instead of falling through to the
    // loader-blind by-name path. Excludes:
    //  - the stale-pointer sentinel (`ClassId::new(0)`, see the cid==0
    //    handling above), which intentionally keeps using the CP method-ref
    //    class name for its own recovery path;
    //  - lambda-proxy receivers, whose synthetic class_id is NOT a normal
    //    entry in `class_manager` (it has no real method table of its own
    //    for `invoke_on_class_shared`'s `find_method_recursive` to walk).
    //    `invoke_on_class_shared_inner` DOES also re-check
    //    `shared.classes.lambda_proxies` on the receiver up front and redirect to
    //    `try_lambda_dispatch`, but only when passed the RECEIVER's own
    //    class_id — routing a lambda receiver's SAM method call (e.g.
    //    `Consumer.accept`) through this branch at all, instead of the
    //    by-name `invoke_shared` fallback the lambda dispatch machinery
    //    already handles correctly, regressed
    //    `ProcessInfoTests.memoryInfoIsAvailable`'s `allSatisfy(lambda)`
    //    with `NoSuchMethodError: <unknown class N>.accept(...)`.
    // `receiver_class_id` is `Some` only for a non-special, non-array receiver:
    // the object `is_lambda_proxy_receiver` already looked up.
    let is_lambda_receiver = receiver_class_id.is_some() && is_lambda_proxy_receiver;
    let result = if let Some(rcv_cid) = dispatch_override
        .or_else(|| receiver_class_id.filter(|c| *c != ClassId::new(0) && !is_lambda_receiver))
    {
        // Dispatching on the receiver's own class: SELECT against what this
        // site resolved (JVMS §5.4.6), the input step 4 above used, instead of
        // taking the first same-signature declaration on the receiver's chain
        // (a private or cross-package package-private method there would hide
        // the override).
        match selection_input.filter(|&(receiver_id, _)| receiver_id == rcv_cid) {
            Some((_, resolved)) => crate::vm::invoke_on_receiver_class_selecting(
                shared,
                thread,
                rcv_cid,
                resolved,
                &method_name,
                &method_descriptor,
                &args,
            )?,
            None => crate::vm::invoke_on_class_shared(
                shared,
                thread,
                rcv_cid,
                &method_name,
                &method_descriptor,
                &args,
            )?,
        }
    } else {
        invoke_shared(
            shared,
            thread,
            &invoke_class,
            &method_name,
            &method_descriptor,
            &args,
        )?
    };

    if let Some(value) = result {
        let ret = crate::jit::return_type(&method_descriptor);
        if ret != b'V' {
            let value = coerce_value_for_return(value, ret);
            // T18.K4 — tag-exact push for J/D fallback invoke return values.
            push_invoke_return_value(&mut thread.frames[frame_idx].stack, value)?;
            // Hypothesis (b): a `safe_native_call` reached via `invoke_shared`
            // (recursive slow path) sets `thread.native_pending_return` on object
            // returns but the clear-after-push helper is only invoked in the
            // stackless dispatch arms. Once the value lives on the operand stack
            // (or a local), the stale pinned reference can outlive a subsequent
            // minor GC and re-surface in `update_root_snapshot`. Clear it here so
            // every slow-path consumer mirrors the stackless invariant.
            crate::vm::native_return_pushed_to_stack(shared, thread);
        }
    }

    // Populate cache for future fast-path hits
    if is_special || (private_virtual_target.is_some() && !private_interface_pin_uncached) {
        populate_invoke_cache(
            thread,
            shared,
            current_class_id,
            cp_index,
            is_special,
            false,
            pc,
            None,
        );
    } else if private_virtual_target.is_none() {
        if let Some(rcv_cid) = receiver_class_id {
            populate_virtual_invoke_cache(
                thread,
                shared,
                current_class_id,
                cp_index,
                rcv_cid,
                &args[0],
                pc,
            );
        }
    }

    Ok(CachedCallResult::Handled)
}

/// Split a method descriptor into (param_types, return_type), where each type
/// is a single descriptor token such as "I", "J", "Ljava/lang/Integer;", or
/// "[Ljava/lang/String;".
pub fn split_method_descriptor(descriptor: &str) -> (Vec<String>, String) {
    let (params, ret) = split_method_descriptor_ref(descriptor);
    (
        params.into_iter().map(str::to_string).collect(),
        ret.to_string(),
    )
}

/// The return-type token of a method descriptor, without parsing (or
/// allocating for) the parameter list.
///
/// The return type is everything after the `)` that CLOSES the parameter
/// list. That is not always the first `)`: a class name may contain one
/// (JVMS 4.2.2), and for `(LA)B;)Z` the first-`)` scan answered `B;)Z`
/// (round 11 wave 5). The walk (`descriptor_param_list_end`) touches only the
/// parameter bytes and allocates nothing, which is what two of the lambda
/// dispatcher's `split_method_descriptor` calls were replaced by this for.
/// Returns `""` for a descriptor whose parameter list does not close
/// (malformed), which every consumer already treats as "not `V`, not a
/// match".
#[inline]
pub fn descriptor_return_ref(descriptor: &str) -> &str {
    match cratonvm_jit_api::descriptor_param_list_end(descriptor) {
        Some(close) => descriptor.get(close + 1..).unwrap_or(""),
        None => "",
    }
}

/// Borrowing twin of [`split_method_descriptor`]: the parameter tokens and the
/// return token as slices of `descriptor`, so a caller that only reads them
/// pays one `Vec` allocation instead of one `String` per parameter plus one for
/// the return type.
///
/// Use this on any per-call path. The lambda dispatcher reached
/// `split_method_descriptor` four to six times per lambda INVOCATION — twice in
/// `try_lambda_dispatch` purely to read a return-type character, twice more in
/// `coerce_lambda_args`, again in `checkcast_lambda_instantiated_args` — and
/// `mi_malloc`/`mi_free`/`__memmove` were ~36% of a lambda-only `perf` profile
/// before those sites moved here.
///
/// A malformed descriptor yields `(vec![], "")` rather than a panic.
pub fn split_method_descriptor_ref(descriptor: &str) -> (Vec<&str>, &str) {
    let bytes = descriptor.as_bytes();
    // A descriptor that does not open with `(` is MALFORMED, and this function
    // used to PANIC on it rather than reject it: `i` starts at 1 to skip the
    // `(`, the two loops are bounded by `bytes.len()`, but the tail slice
    // `&descriptor[i..]` is not -- so an EMPTY descriptor reached
    // `&""[1..]` and panicked with "start byte index 1 is out of bounds for
    // string of length 0".
    //
    // That panic crosses the native boundary. Measured 2026-09-10: with
    // `CRATONVM_ENFORCE_NATIVE_SHADOW` armed on core reflection,
    // `MethodHandles.Lookup.unreflect(Method)` arrives here with an empty
    // descriptor; the panic is logged as a "Native method panic caught" and
    // then aborts the VM with `internal error`, which killed lane 3's
    // instrument at row 126 of 245 and made the whole arm unscorable. A
    // malformed descriptor must not be able to take the VM down.
    //
    // `descriptor_return_ref` directly above already states the house rule for
    // this input -- it "returns `""` for a descriptor with no `')'`
    // (malformed), which every consumer already treats as not-`V`, not-a-match"
    // -- so the hardening existed and had been applied to one of the two
    // neighbouring parsers. This is the other one.
    if bytes.first() != Some(&b'(') {
        return (Vec::new(), "");
    }
    // Pre-size from the `(...)` span: one token is at least one byte, and no
    // real descriptor holds more than a handful. Without this the per-call Vec
    // reallocated through `RawVec::grow_one` on the lambda path.
    let mut params: Vec<&str> = Vec::with_capacity(8);
    let mut i = 1; // skip '('
    while i < bytes.len() && bytes[i] != b')' {
        let start = i;
        // Consume array dims.
        while i < bytes.len() && bytes[i] == b'[' {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        match bytes[i] {
            b'L' => {
                while i < bytes.len() && bytes[i] != b';' {
                    i += 1;
                }
                // An unterminated class name (`(Ljava/lang/String`) ran `i`
                // past the end here, and the slice below panicked. Same house
                // rule as the missing-`(` case above: malformed, not fatal.
                if i >= bytes.len() {
                    return (Vec::new(), "");
                }
                i += 1; // consume ';'
            }
            // A non-ASCII byte cannot start a primitive tag, and slicing one
            // byte past it could split a UTF-8 sequence and panic.
            b if !b.is_ascii() => return (Vec::new(), ""),
            _ => {
                i += 1; // single-char primitive
            }
        }
        params.push(&descriptor[start..i]);
    }
    // Skip ')'
    if i < bytes.len() && bytes[i] == b')' {
        i += 1;
    }
    (params, &descriptor[i..])
}

/// Non-allocating equivalent of `split_method_descriptor(d).0[n].as_bytes().first()`:
/// the FIRST byte of the n-th parameter's descriptor token (`b'['` for arrays —
/// byte-identical to the old closures, and `decode_by_descriptor` treats `b'['`
/// as a reference). The warm call-dispatch arms only ever need this tag byte (to
/// pick the category-2 long/double pop path), so they previously paid a per-call
/// `Vec<String>` + per-param `String` allocation in `split_method_descriptor`
/// purely to read one byte each. Returns `b'L'` when `n` is out of range (the
/// arms' existing default). Tokenization mirrors `split_method_descriptor`.
pub(super) fn nth_param_tag_byte(descriptor: &str, n: usize) -> u8 {
    let bytes = descriptor.as_bytes();
    let mut i = 1; // skip '('
    let mut idx = 0;
    while i < bytes.len() && bytes[i] != b')' {
        let tag = bytes[i]; // first byte of this token ('[' for arrays)
        while i < bytes.len() && bytes[i] == b'[' {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        match bytes[i] {
            b'L' => {
                while i < bytes.len() && bytes[i] != b';' {
                    i += 1;
                }
                i += 1; // consume ';'
            }
            _ => {
                i += 1; // single-char primitive
            }
        }
        if idx == n {
            return tag;
        }
        idx += 1;
    }
    b'L'
}

/// Every parameter tag byte of a descriptor, collected in ONE forward scan.
///
/// [`nth_param_tag_byte`] answers for a single index and rescans from `(` each
/// time. Every caller in the tree is a per-ARGUMENT loop, so the descriptor was
/// being re-tokenised once per argument: popping N args cost O(N^2) scanning,
/// re-derived on every call, for a descriptor that is fixed per call site.
///
/// Measured before writing this: against HotSpot's interpreter CratonVM runs
/// `iadd` at 4.1x but pays ~32ns per extra argument against HotSpot's ~0.94ns,
/// a 34x gap that is far above its own baseline. `args8` cost 479.6ns against
/// `args0`'s 221.0ns on the same run.
///
/// This is the same shape the 2026-08-18 interpreter audit kept finding, and
/// the fix already exists one variant away: `CachedInvokeTarget::Intrinsic`
/// carries `param_descs`, "split ONCE at IC-fill time ... without re-parsing
/// the descriptor string". The bytecode variants never got it. Doing it per
/// call rather than per IC fill keeps the change inside the dispatch arms —
/// `CachedBytecodeMethod` cannot take a new field without touching its 38
/// struct literals across four crates, none of which has a `..` tail.
///
/// `INLINE` matches the dispatch arms' own `MAX_INLINE_ARGS`. A descriptor with
/// more parameters than that falls back to the per-index scan, so behaviour is
/// unchanged for the rare wide case rather than capped.
pub(super) struct ParamTags {
    tags: [u8; Self::INLINE],
    /// Number of entries in `tags` that were filled by the single scan.
    len: usize,
    /// `true` when the descriptor has more parameters than `tags` can hold, so
    /// `get` must fall back rather than answer `b'L'` for a real parameter.
    overflow: bool,
    /// Kill switch: `CRATONVM_JIT_NO_PARAM_TAG_SCAN=1` skips the scan entirely
    /// and sends every `get` back through the per-index rescan, reproducing the
    /// pre-change behaviour EXACTLY. It exists so the speedup can be measured
    /// on one binary — a cross-binary comparison is not an A/B.
    bypass: bool,
}

fn param_tag_scan_disabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_PARAM_TAG_SCAN").is_some()
    })
}

impl ParamTags {
    // 8, not 16. Measured: at 16 the struct cost ~11ns of fixed setup on EVERY
    // call (zero-arg calls regressed in 7 of 8 paired rounds) against ~7.6ns
    // saved per argument — break-even at ~1.5 args, which is a pessimisation for
    // the 0-2 arg calls that dominate real Java. Eight covers essentially every
    // method; wider ones take the fallback and are unchanged.
    const INLINE: usize = 8;

    /// The tags for a resolved method, read from the per-method memo on its
    /// `CachedBytecodeMethod` rather than rescanned for this call.
    ///
    /// This is the constructor the invoke paths want. `CRATONVM_JIT_NO_
    /// DESCRIPTOR_FACTS=1` routes it back through [`Self::of`], restoring the
    /// per-call scan exactly, so the memo can be priced on one binary.
    #[inline]
    pub(super) fn for_method(cached: &cratonvm_jit_api::CachedBytecodeMethod) -> Self {
        if cratonvm_jit_api::descriptor_facts_disabled() {
            return Self::of(&cached.method_descriptor);
        }
        Self::from_facts(cached.descriptor_facts())
    }

    /// Adopt a [`cratonvm_jit_api::DescriptorFacts`] that was tokenised once
    /// per *method* and cached on the `CachedBytecodeMethod`, instead of
    /// rescanning the descriptor for this one call.
    ///
    /// The two tokenisations are the same algorithm — `DescriptorFacts::of`
    /// is [`Self::of`]'s parameter walk, moved to where it can be memoized —
    /// so this is a relocation of work, not a change of answer.
    /// `param_tags_match_descriptor_facts` pins them against each other.
    ///
    /// `CRATONVM_JIT_NO_PARAM_TAG_SCAN` still bypasses to the per-index
    /// rescan, so the kill switch means the same thing on both constructors.
    ///
    /// Prefer [`Self::for_method`] at call sites that hold the whole
    /// `CachedBytecodeMethod`: it also honours
    /// `CRATONVM_JIT_NO_DESCRIPTOR_FACTS`, which this constructor cannot,
    /// having no descriptor string to fall back to.
    #[inline]
    pub(super) fn from_facts(facts: &cratonvm_jit_api::DescriptorFacts) -> Self {
        if param_tag_scan_disabled() {
            return Self {
                tags: [b'L'; Self::INLINE],
                len: 0,
                overflow: false,
                bypass: true,
            };
        }
        debug_assert_eq!(
            Self::INLINE,
            cratonvm_jit_api::DescriptorFacts::INLINE_PARAMS,
            "ParamTags and DescriptorFacts must agree on the inline width, or \
             the overflow fallback engages at two different arities"
        );
        Self {
            tags: facts.param_tags,
            // Widening: bounded by INLINE_PARAMS (8) by the producer's loop.
            len: facts.param_tag_len as usize,
            overflow: facts.param_tags_overflow,
            bypass: false,
        }
    }

    /// Tokenise `descriptor` once. Tokenisation mirrors [`nth_param_tag_byte`]
    /// exactly, including its `b'['`-for-arrays tag and its `b'L'` answer for
    /// an out-of-range index; `param_tags_match_nth_param_tag_byte` pins that.
    ///
    /// Prefer [`Self::from_facts`] wherever a `CachedBytecodeMethod` is in
    /// hand: this constructor rescans the descriptor on every call, and the
    /// invoke path called it per invoke. It remains for the resolution paths
    /// that hold only a descriptor string.
    #[inline]
    pub(super) fn of(descriptor: &str) -> Self {
        if param_tag_scan_disabled() {
            return Self {
                tags: [b'L'; Self::INLINE],
                len: 0,
                overflow: false,
                bypass: true,
            };
        }
        let bytes = descriptor.as_bytes();
        let mut tags = [b'L'; Self::INLINE];
        let mut len = 0usize;
        let mut overflow = false;
        let mut i = 1; // skip '('
        while i < bytes.len() && bytes[i] != b')' {
            let tag = bytes[i]; // first byte of this token ('[' for arrays)
            while i < bytes.len() && bytes[i] == b'[' {
                i += 1;
            }
            if i >= bytes.len() {
                break;
            }
            match bytes[i] {
                b'L' => {
                    while i < bytes.len() && bytes[i] != b';' {
                        i += 1;
                    }
                    i += 1; // consume ';'
                }
                _ => {
                    i += 1; // single-char primitive
                }
            }
            if len < Self::INLINE {
                tags[len] = tag;
                len += 1;
            } else {
                overflow = true;
            }
        }
        Self {
            tags,
            len,
            overflow,
            bypass: false,
        }
    }

    /// The n-th parameter's tag byte. Identical to
    /// `nth_param_tag_byte(descriptor, n)` for every `n`.
    #[inline]
    pub(super) fn get(&self, descriptor: &str, n: usize) -> u8 {
        if self.bypass {
            nth_param_tag_byte(descriptor, n)
        } else if n < self.len {
            self.tags[n]
        } else if self.overflow {
            nth_param_tag_byte(descriptor, n)
        } else {
            b'L'
        }
    }

    /// The tag for argument slot `i` of a NON-STATIC call, where slot 0 is the
    /// receiver and carries `b'L'`. Spelled out here because every virtual arm
    /// had written the same `if i == 0 { b'L' } else { ...(i - 1) }` closure.
    #[inline]
    pub(super) fn get_with_receiver(&self, descriptor: &str, i: usize) -> u8 {
        if i == 0 {
            b'L'
        } else {
            self.get(descriptor, i - 1)
        }
    }
}

/// Unbox a boxed primitive wrapper object into its primitive `Value`.
/// Returns the original value unchanged if it's not a recognized wrapper.
pub(super) fn unbox_wrapper(shared: &SharedVm, prim_char: char, v: Value) -> Value {
    match (prim_char, v) {
        ('I' | 'B' | 'S' | 'C' | 'Z', Value::Object(Some(b))) => shared.mem.heap.get_field(b, 0),
        // Lambda metafactory adaptation permits unboxing followed by primitive
        // widening.  An Integer supplied to a `long` implementation method
        // must therefore become Value::Long, rather than carrying the raw
        // compact Int tag into an lload/putfield J path.
        ('J' | 'F' | 'D', Value::Object(Some(b))) => {
            widen_unboxed_primitive(prim_char, shared.mem.heap.get_field(b, 0))
        }
        (_, other) => other,
    }
}

/// Apply the primitive-widening portion of lambda unboxing without changing
/// the source wrapper's value. This is intentionally limited to conversions
/// permitted after unboxing by the Java language specification.
pub(super) fn widen_unboxed_primitive(target: char, value: Value) -> Value {
    match (target, value) {
        ('J', Value::Int(value)) => Value::Long(value as i64),
        ('F', Value::Int(value)) => Value::Float(value as f32),
        ('F', Value::Long(value)) => Value::Float(value as f32),
        ('D', Value::Int(value)) => Value::Double(value as f64),
        ('D', Value::Long(value)) => Value::Double(value as f64),
        ('D', Value::Float(value)) => Value::Double(value as f64),
        (_, value) => value,
    }
}

/// Normalize a value crossing into `aastore`.
///
/// The verifier guarantees an object reference at this opcode, but native and
/// reflective bridges can expose an unboxed primitive despite an `Object`
/// return descriptor. The common fast and decoded handlers both use this
/// allocation-only recovery helper, so package selection cannot change
/// wrapper identity or layout. Ordinary Java boxing still goes through
/// `valueOf` and retains its cache semantics.
///
/// Fallible (gc-common w5-c, `handoff-w4c-infallible-callers-residue.md` §2):
/// the wrapper is allocated with the collection-free `try_alloc_object_full`,
/// and a refusal is the `OutOfMemoryError` HotSpot would throw, reported on the
/// `aastore` handler's own channel. This was the infallible
/// `VmHeap::alloc_object`, which on exhaustion aborts the process on
/// Generational and ZGC and spends G1's emergency reserve
/// (`common-f-g1-fatal-abort-from-infallible-alloc-object`). No collection
/// here: the handlers call this with the array and index still on the operand
/// stack and the popped value in a Rust local, and the recovery it performs is
/// for bridge-produced primitives, not for a path a program allocates through.
pub(super) fn normalize_aastore_value(
    shared: &SharedVm,
    value: Value,
) -> Result<Value, RuntimeError> {
    let (class_name, payload) = match value {
        Value::Int(_) => ("java/lang/Integer", value),
        Value::Long(_) => ("java/lang/Long", value),
        Value::Float(_) => ("java/lang/Float", value),
        Value::Double(_) => ("java/lang/Double", value),
        _ => return Ok(value),
    };
    let class_id = shared
        .classes
        .class_manager
        .write()
        .load_class(class_name)
        .unwrap_or(ClassId::new(0));
    let wrapper = shared
        .mem
        .heap
        .try_alloc_object_full(class_id, 1)
        .ok_or_else(|| RuntimeError::OutOfMemoryError {
            // The parenthesised site detail never reaches Java
            // (`RuntimeError::as_java_throwable`).
            message: format!("Java heap space (aastore box of {class_name})"),
        })?;
    shared.mem.heap.set_field(wrapper, 0, payload);
    Ok(Value::Object(Some(wrapper)))
}

/// Box a primitive `Value` by invoking the wrapper's `valueOf(prim)`.
pub(super) fn box_primitive(
    shared: &SharedVm,
    thread: &mut JvmThread,
    prim_char: char,
    v: Value,
) -> Result<Value, MethodCallFailed> {
    let (cls, desc) = match prim_char {
        'Z' => ("java/lang/Boolean", "(Z)Ljava/lang/Boolean;"),
        'B' => ("java/lang/Byte", "(B)Ljava/lang/Byte;"),
        'S' => ("java/lang/Short", "(S)Ljava/lang/Short;"),
        'C' => ("java/lang/Character", "(C)Ljava/lang/Character;"),
        'I' => ("java/lang/Integer", "(I)Ljava/lang/Integer;"),
        'J' => ("java/lang/Long", "(J)Ljava/lang/Long;"),
        'F' => ("java/lang/Float", "(F)Ljava/lang/Float;"),
        'D' => ("java/lang/Double", "(D)Ljava/lang/Double;"),
        _ => return Ok(v),
    };
    // If already an Object, nothing to do.
    if matches!(v, Value::Object(_)) {
        return Ok(v);
    }
    let r = invoke_shared(shared, thread, cls, "valueOf", desc, &[v])?;
    Ok(r.unwrap_or(Value::Object(None)))
}

/// Returns true if the descriptor token is a primitive ("I","J",...).
pub(super) fn is_primitive_desc(token: &str) -> bool {
    matches!(token, "I" | "J" | "F" | "D" | "B" | "S" | "Z" | "C")
}

/// Returns true if the descriptor token is a reference (L... or [...).
pub(super) fn is_reference_desc(token: &str) -> bool {
    token.starts_with('L') || token.starts_with('[')
}

/// Coerce a single argument between the SAM's view (`sam_tok`) and the
/// impl's view (`impl_tok`). If SAM has reference but impl has primitive,
/// unbox. If SAM has primitive but impl has reference, box.
pub(super) fn coerce_arg(
    shared: &SharedVm,
    thread: &mut JvmThread,
    sam_tok: &str,
    impl_tok: &str,
    v: Value,
) -> Result<Value, MethodCallFailed> {
    // LOAD-BEARING BEYOND THIS FUNCTION: `coerce_lambda_args` skips its whole
    // body — descriptor walks, pin pushes, the `checkcast` replay — for a
    // non-capturing lambda whose three descriptors are identical, and that
    // shortcut is only equivalent because equal tokens coerce to the identity
    // HERE. If this arm ever has to do work, drop that fast path with it.
    if sam_tok == impl_tok {
        return Ok(v);
    }
    // SAM = reference (e.g. Object), impl = primitive — unbox.
    if is_reference_desc(sam_tok) && is_primitive_desc(impl_tok) {
        // A `null` cannot be unboxed: the class `LambdaMetafactory` spins
        // raises a message-less `NullPointerException` there (interpreter
        // round i1 wave 23, lane L4; `Function<Integer,Integer> f = Math::abs;
        // f.apply(null)`). It used to pass the null on as the primitive
        // argument. Probe `tools/probes/interp/L4/L4W23LambdaConversions.java`.
        if matches!(v, Value::Object(None)) {
            return Err(RuntimeError::NullPointerException { message: None }.into());
        }
        let ch = impl_tok.chars().next().ok_or_else(|| {
            MethodCallFailed::InternalError(VmError::Internal {
                message: "empty primitive descriptor token in coerce_arg".to_string(),
            })
        })?;
        return Ok(unbox_wrapper(shared, ch, v));
    }
    // SAM = primitive, impl = reference — box.
    if is_primitive_desc(sam_tok) && is_reference_desc(impl_tok) {
        let ch = sam_tok.chars().next().ok_or_else(|| {
            MethodCallFailed::InternalError(VmError::Internal {
                message: "empty primitive descriptor token in coerce_arg".to_string(),
            })
        })?;
        return box_primitive(shared, thread, ch, v);
    }
    // Primitive widening (e.g. I -> J) — best-effort.
    if is_primitive_desc(sam_tok) && is_primitive_desc(impl_tok) {
        return Ok(widen_primitive(sam_tok, impl_tok, v));
    }
    Ok(v)
}

// ---------------------------------------------------------------------------
// Lambda / invokedynamic dispatch
// ---------------------------------------------------------------------------
//
// Getting from a call site's SAM descriptor to the implementation method,
// and making the arguments fit: `interpreter/lambda.rs`.

/// How a cached-native dispatch learns its return tag.
///
/// The tag decides one thing — whether the native's result is pushed onto the
/// caller's operand stack — and it is needed only when the native actually
/// returned a value, which is why [`RetTag::Scan`] stays lazy.
#[derive(Clone, Copy)]
pub(super) enum RetTag<'a> {
    /// Read off the call site's cached `DescriptorFacts`; nothing to compute.
    Known(u8),
    /// Scan `descriptor` for the byte after `')'`, as this path always did.
    Scan(&'a str),
}

impl RetTag<'_> {
    #[inline]
    fn resolve(self) -> u8 {
        match self {
            RetTag::Known(tag) => tag,
            RetTag::Scan(descriptor) => crate::jit::return_type(descriptor),
        }
    }
}

/// [`invoke_cached_native_callback`] for the two inline-cache `Native` arms,
/// which hold the resolved [`NativeMethodId`] and can therefore ask whether the
/// slot claims **leaf** — see
/// [`NativeMethodRegistry::set_leaf`](cratonvm_native_api::NativeMethodRegistry::set_leaf).
///
/// A leaf goes through `safe_native_call_leaf`, which is the same funnel minus
/// the pinning, GC probes, thread-state transitions and unwind bookkeeping that
/// a body doing one field read cannot need. Measured on
/// `probes/NativeShapeProbe.java` under `--nojit`: `AtomicInteger.get()` cost
/// 922 ns against a 167 ns empty-loop control, i.e. ~755 ns for a `return
/// value;`, and essentially all of it was the funnel.
///
/// The leafness question is one bounds-checked index into the registry's slot
/// table on an id the cache already resolved — no hashing, no string compare,
/// nothing this path was not already holding. Answering `false` for an
/// unrecognised id keeps an unexpected handle on the full funnel.
///
/// `sync` is the entry's `ACC_SYNCHRONIZED` monitor fact
/// (`CachedInvokeTarget::Native::sync`): `Some` takes the full funnel under
/// the monitor ([`invoke_cached_native_callback_synchronized`]), never the
/// leaf shortcut. `None` — every other native — costs one not-taken branch.
#[inline]
#[allow(clippy::too_many_arguments)]
pub(super) fn invoke_cached_native_callback_leaf_aware(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    callback: cratonvm_native_api::NativeCallback,
    native_id: cratonvm_native_api::NativeMethodId,
    args: &[Value],
    ret: RetTag<'_>,
    sync: Option<(bool, ClassId)>,
) -> Result<(), MethodCallFailed> {
    if let Some(sync) = sync {
        return invoke_cached_native_callback_synchronized(
            shared, thread, frame_idx, callback, args, ret, sync,
        );
    }
    if !shared.natives.native_methods.is_leaf_id(native_id) {
        return invoke_cached_native_callback_impl(
            shared, thread, frame_idx, callback, args, ret, false,
        );
    }
    super::site_cache::site_stats::bump(super::site_cache::site_stats::NATFACTS_LEAF);
    // The native ring is deliberately not entered. It exists so a watchdog can
    // name the native a hung thread is inside; a leaf cannot block, so it can
    // never be the answer to that question, and `record_enter`/`record_exit`
    // are two of the calls this path exists to remove.
    let result = crate::vm::safe_native_call_leaf(shared, thread, callback, args)?;
    if let Some(value) = result {
        let ret = ret.resolve();
        if ret != b'V' {
            let value = coerce_value_for_return(value, ret);
            push_invoke_return_value(&mut thread.frames[frame_idx].stack, value)?;
            crate::vm::native_return_pushed_to_stack(shared, thread);
        }
    }
    Ok(())
}

#[inline]
pub(super) fn invoke_cached_native_callback_impl(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    callback: cratonvm_native_api::NativeCallback,
    args: &[Value],
    ret: RetTag<'_>,
    objects_prevalidated: bool,
) -> Result<(), MethodCallFailed> {
    // Widening: small integer index -> usize (non-negative, fits in pointer width)
    let _ring_idx = cratonvm_native_api::native_ring::record_enter(callback as usize);
    let result = if objects_prevalidated {
        crate::vm::safe_native_call_prevalidated_objects(shared, thread, callback, args)
    } else {
        crate::vm::safe_native_call(shared, thread, callback, args)
    };
    cratonvm_native_api::native_ring::record_exit(_ring_idx);
    let result = result?;
    if let Some(value) = result {
        let ret = ret.resolve();
        // A void method must not leave anything on the caller's operand stack,
        // even if its native happens to return `Some(_)` (many natives return
        // the receiver / a status for convenience). The slow path
        // (`execute_invoke_kind`) already drops the value for `V`; the cached
        // VirtualNative fast path historically pushed it unconditionally,
        // leaking one operand per call. With an overloaded void method whose
        // first call primes this cache (e.g. `java/util/zip/Checksum.update`),
        // the leak accumulates until the caller frame's operand stack overflows
        // its `max_stack` — kafka-clients `Crc32CTest.testUpdate` panicked at
        // `value_stack.rs` "len 24 index 24". Only push for non-void returns.
        if ret != b'V' {
            let value = coerce_value_for_return(value, ret);
            push_invoke_return_value(&mut thread.frames[frame_idx].stack, value)?;
            crate::vm::native_return_pushed_to_stack(shared, thread);
        }
    }
    Ok(())
}

/// [`invoke_cached_native_callback_impl`] for an inline-cache entry whose
/// native answers an `ACC_SYNCHRONIZED` method (`sync` is the entry's
/// `(is_static, declaring class)` fact): the call holds the method's monitor
/// through [`safe_native_call_synchronized`], exactly as the slow path's native
/// doors do, so such a site is cached instead of taking the slow path — and a
/// fill — on every call (interpreter round i1 wave 11, lane L5).
pub(super) fn invoke_cached_native_callback_synchronized(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    callback: cratonvm_native_api::NativeCallback,
    args: &[Value],
    ret: RetTag<'_>,
    sync: (bool, ClassId),
) -> Result<(), MethodCallFailed> {
    let ring_idx = cratonvm_native_api::native_ring::record_enter(callback as usize);
    let result = safe_native_call_synchronized(shared, thread, callback, args, Some(sync));
    cratonvm_native_api::native_ring::record_exit(ring_idx);
    if let Some(value) = result? {
        // Same void rule as `invoke_cached_native_callback_impl`.
        let ret = ret.resolve();
        if ret != b'V' {
            let value = coerce_value_for_return(value, ret);
            push_invoke_return_value(&mut thread.frames[frame_idx].stack, value)?;
            crate::vm::native_return_pushed_to_stack(shared, thread);
        }
    }
    Ok(())
}

/// Invoke a cached native callback with [`safe_native_call`] (pins jobject
/// args / return values across safepoint GC) and push any result.
#[inline]
pub(super) fn invoke_cached_native_callback(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    callback: cratonvm_native_api::NativeCallback,
    args: &[Value],
    method_descriptor: &str,
) -> Result<(), MethodCallFailed> {
    invoke_cached_native_callback_impl(
        shared,
        thread,
        frame_idx,
        callback,
        args,
        RetTag::Scan(method_descriptor),
        false,
    )
}

#[inline]
pub(super) fn invoke_cached_native_callback_prevalidated(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    callback: cratonvm_native_api::NativeCallback,
    args: &[Value],
    method_descriptor: &str,
) -> Result<(), MethodCallFailed> {
    invoke_cached_native_callback_impl(
        shared,
        thread,
        frame_idx,
        callback,
        args,
        RetTag::Scan(method_descriptor),
        true,
    )
}

/// CRATONVM_DBG_SOE — one-shot diagnostic: when the frame-depth ceiling is
/// hit, dump the newest Java frames so the recursion CYCLE is visible. The
/// Throwable stack capture keeps only a handful of frames, which hides which
/// methods actually recurse (e.g. the H2 GROUP BY StackOverflowError).
pub(super) fn dump_stack_on_soe(thread: &JvmThread) {
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_SOE").is_none() {
        return;
    }
    use std::sync::atomic::{AtomicBool, Ordering};
    static DUMPED: AtomicBool = AtomicBool::new(false);
    if DUMPED.swap(true, Ordering::Relaxed) {
        return;
    }
    eprintln!(
        "[DBG_SOE] stack depth {} — newest 150 frames:",
        thread.frames.len()
    );
    for (i, f) in thread.frames.iter().rev().take(150).enumerate() {
        eprintln!(
            "[DBG_SOE]   #{i} {}.{}{} pc={}",
            f.class_name(),
            f.method_name(),
            f.method_descriptor(),
            f.pc
        );
    }
}

// ---------------------------------------------------------------------------
// Native-over-bytecode override policy
// ---------------------------------------------------------------------------
//
// Which methods a registered Rust native takes over from real bytecode, the
// superclass walk that gives an abstract-class native its reach, and the
// redefinition rules that make an override yield: `interpreter/native_override.rs`.

/// Every constant registry triple [`try_stackless_invoke`] can dispatch from an
/// arm that holds a `NativeCallback` but **no `NativeMethodId`**, and therefore
/// never reaches `NativeMethodRegistry::record_invocation`.
///
/// This is the sweep `G33-1` §8 asked for, run over this file. It is a **third
/// bypass family**, distinct from the interpreter's intrinsic table (§2
/// mechanism 1) and the JIT's thin direct-call helpers (§2 mechanism 2), and it
/// is arm-independent: none of it depends on the JIT or on
/// `CRATONVM_DISABLE_INTRINSICS`, so the two-part exact-census recipe in §4 does
/// **not** make these rows exact.
///
/// The gap is already acknowledged in code — the census increment further down
/// this function says a `None` id "means the callback came from one of the
/// exotic arms, which resolve other triples and are the wave-2 census gap noted
/// at step 1". What was missing is any way for a *reader of the dump* to learn
/// that. These marks supply it.
///
/// Three arms are deliberately absent because their triple is not constant and
/// cannot be enumerated here; see the record for the nomination:
///
///  * the superclass walk (`find(&parent.name, method_name, descriptor)`),
///    whose class comes from a runtime hierarchy;
///  * the three `sun/security/ssl/*Impl` → `javax/net/ssl/*` aliases, whose
///    method and descriptor come from the call site;
///  * `surefire_lazy_launcher_discover_native`, whose whole triple is
///    discovered from the runtime receiver.
///
/// A triple not registered in this VM does not resolve and is not marked.
const UNCOUNTED_STACKLESS_NATIVES: [(&str, &str, &str); 14] = [
    // The `JarFile` invokespecial constructor bridge — four registered
    // descriptor shapes, dispatched and returned `Handled` before the census
    // increment below is ever reached.
    ("java/util/jar/JarFile", "<init>", "(Ljava/io/File;)V"),
    ("java/util/jar/JarFile", "<init>", "(Ljava/io/File;Z)V"),
    ("java/util/jar/JarFile", "<init>", "(Ljava/io/File;ZI)V"),
    (
        "java/util/jar/JarFile",
        "<init>",
        "(Ljava/io/File;ZILjava/lang/Runtime$Version;)V",
    ),
    // The `super.close()` bridge: the call site names `JarFile` or `ZipFile`,
    // the dispatch always resolves `ZipFile.close`.
    ("java/util/zip/ZipFile", "close", "()V"),
    // Reflection. `NCS_METHOD_INVOKE` / `NCS_CONSTRUCTOR_NEW_INSTANCE` memoize
    // the callback per registry generation and return `Handled` directly, so
    // every reflective call through these two reads as zero.
    (
        "java/lang/reflect/Method",
        "invoke",
        "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/lang/reflect/Constructor",
        "newInstance",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    // Panama: the receiver-gated `DowncallHandle` arms.
    (
        "java/lang/foreign/DowncallHandle",
        "type",
        "()Ljava/lang/invoke/MethodType;",
    ),
    (
        "java/lang/foreign/DowncallHandle",
        "invoke",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/lang/foreign/DowncallHandle",
        "invokeExact",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/lang/foreign/DowncallHandle",
        "invokeBasic",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    // The signature-polymorphic `MethodHandle` bridge, registered under the
    // erased `Object[]` descriptor while the call site carries a concrete one.
    // This is the same species as `G33-1` §8 N4's `vm_exec.rs` finding, seen
    // from the interpreter's stackless path.
    (
        "java/lang/invoke/MethodHandle",
        "invoke",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/lang/invoke/MethodHandle",
        "invokeExact",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/lang/invoke/MethodHandle",
        "invokeBasic",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
    ),
];

/// Declare every [`UNCOUNTED_STACKLESS_NATIVES`] slot's `invocations` count
/// incomplete, once per (VM, registry generation).
///
/// # Where this is called from, and why not per dispatch
///
/// From the points in [`try_stackless_invoke`] where an uncounted native is
/// about to be dispatched, all of which are already committed to a
/// `safe_native_call` — so the steady-state cost is three relaxed loads and a
/// predictable branch on a path whose next act costs ~141 ns, and nothing at all
/// on the ordinary counted path.
///
/// It marks the whole list rather than the one triple that fired, deliberately.
/// The bit's claim is that a bypassing path **exists** for the slot, which is a
/// property of this function's shape and is statically true for all fourteen
/// however the call arrived; and the alternative — recovering the triple that
/// produced the callback — would mean either a `resolve_id` per dispatch or a
/// reverse lookup from a callback address, on the interpreter's hottest
/// function.
///
/// Making these rows *exact* instead is a separate, real option: the arms take
/// the full `safe_native_call` funnel, against which `G33-1` §5's measured
/// +9.2 ns is the same ~6% the counter already costs everywhere else it sits.
/// It is not taken here because it means rewriting eleven `find` calls in
/// `try_stackless_invoke` into `resolve_id` + `callback_of`, and this lane could
/// neither build nor measure. It is nominated in the record instead.
///
/// The latch and its race are the same shape as the JIT side's — see
/// `jit::helpers::mark_direct_call_helper_natives_incomplete`. Repeats are
/// no-ops; a VM that was never marked can never be skipped.
#[cold]
fn mark_stackless_exotic_natives_incomplete(shared: &SharedVm) {
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
    static MARKED_ANY: AtomicBool = AtomicBool::new(false);
    static MARKED_VM: AtomicUsize = AtomicUsize::new(0);
    static MARKED_GENERATION: AtomicU32 = AtomicU32::new(0);

    let registry = &shared.natives.native_methods;
    let generation = registry.generation();
    if MARKED_ANY.load(Ordering::Relaxed)
        && MARKED_VM.load(Ordering::Relaxed) == shared.vm_identity
        && MARKED_GENERATION.load(Ordering::Relaxed) == generation
    {
        return;
    }
    mark_stackless_exotic_natives_incomplete_in(registry);
    MARKED_VM.store(shared.vm_identity, Ordering::Relaxed);
    MARKED_GENERATION.store(generation, Ordering::Relaxed);
    MARKED_ANY.store(true, Ordering::Relaxed);
}

/// The registry half of [`mark_stackless_exotic_natives_incomplete`], split out
/// so the marking can be driven against a registry built in a test rather than
/// only through a live `SharedVm` and a real reflective call.
///
/// Returns how many of [`UNCOUNTED_STACKLESS_NATIVES`] resolved in this
/// registry. A triple that does not resolve is not an error — a VM that never
/// registered the Panama or `MethodHandle` bridges simply has nothing to
/// declare about them.
fn mark_stackless_exotic_natives_incomplete_in(
    registry: &cratonvm_native_api::NativeMethodRegistry,
) -> usize {
    let mut marked = 0usize;
    for &(class_name, method_name, descriptor) in UNCOUNTED_STACKLESS_NATIVES.iter() {
        if let Some(id) = registry.resolve_id(class_name, method_name, descriptor) {
            registry.mark_invocations_incomplete(id);
            marked += 1;
        }
    }
    marked
}

/// JVMS §2.11.10 for natives: a registered native that answers an
/// `ACC_SYNCHRONIZED` Java method runs holding that method's monitor (the JVM
/// takes it around a native method too), on the slow invoke path and on the
/// inline-cache hit alike: a cached `Native` / `VirtualNative` entry carries
/// the monitor fact (`sync`, since interpreter round i1 wave 11) and its hit
/// arm takes it; no `Intrinsic` entry is built for such a method (that arm
/// carries none).
///
/// **On in every mode** (`--jdk-only` since interpreter round i1 wave 9,
/// `--compatible` since wave 10). `CRATONVM_NATIVE_SYNC=0`
/// (`CRATONVM_JIT=-native-sync`) disarms it and `1` re-arms it.
///
/// The deadlock the switch was held back for — a `Bridge` that shadows a
/// synchronized JDK method's BYTECODE and blocks on another thread while
/// holding the monitor — was censused under `--compatible` (wave 9: the core
/// suite armed is identical to unarmed, jdk-only corpus 44/44); see
/// `docs/internal/fixed-bugs/interpreter-L3-acc-synchronized-skipped-on-native-dispatch-FIXED-20260925.md`.
/// `shared` stays in the signature so the policy remains per VM (AGENTS.md:
/// no process global for compatibility state) should a mode need its own
/// default again.
#[inline]
pub(crate) fn native_sync_enabled(shared: &SharedVm) -> bool {
    let _ = shared;
    native_sync_override().unwrap_or(true)
}

/// `CRATONVM_NATIVE_SYNC`, read once: `Some(true)` for `1`/`true`,
/// `Some(false)` for `0`/`false`, `None` (the default, on) when unset or
/// anything else.
fn native_sync_override() -> Option<bool> {
    static OVERRIDE: std::sync::OnceLock<Option<bool>> = std::sync::OnceLock::new();
    *OVERRIDE.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_NATIVE_SYNC")
            .ok()
            .and_then(|v| {
                if v == "1" || v.eq_ignore_ascii_case("true") {
                    Some(true)
                } else if v == "0" || v.eq_ignore_ascii_case("false") {
                    Some(false)
                } else {
                    None
                }
            })
    })
}

/// The monitor a native answering `class_name.method_name descriptor` must
/// hold under [`native_sync_enabled`]: `(is_static, declaring class)` of the
/// Java method it answers, when that method is `ACC_SYNCHRONIZED`. `None`
/// (two bool loads) when the switch is off — after naming the method on stderr
/// if the door census is armed (see the census note inside). Takes a
/// `class_manager` read, so it must not be called with a guard held.
fn synchronized_native_target(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    dispatch_class_override: Option<ClassId>,
) -> Option<(bool, ClassId)> {
    let armed = native_sync_enabled(shared);
    // With the door census armed (`CRATONVM_DBG_FIELD_SITE`) and the switch
    // disarmed (`CRATONVM_NATIVE_SYNC=0`), name each synchronized method a
    // native answers WITHOUT its monitor, once per triple. Unarmed, this is a
    // second bool load on the native slow path only.
    if !armed && !super::site_cache::site_stats::on() {
        return None;
    }
    let cm = shared.classes.class_manager.read();
    let start = dispatch_class_override.or_else(|| cm.get_loaded_class_id(class_name))?;
    let (is_synchronized, is_static, declaring_id) =
        resolved_method_sync_facts(shared, &cm, start, method_name, descriptor)?;
    if !armed {
        if is_synchronized {
            note_unmonitored_synchronized_native(
                cm.class_store()
                    .get(declaring_id)
                    .map(|c| &*c.name)
                    .unwrap_or(class_name),
                method_name,
                descriptor,
            );
        }
        return None;
    }
    is_synchronized.then_some((is_static, declaring_id))
}

/// The monitor for a native answering a method whose flags the caller already
/// resolved (`try_stackless_invoke`'s `ACC_NATIVE` arm and step 6): the
/// [`synchronized_native_target`] answer without its lookup, with the same
/// census when the switch is off. `class_name` only labels the census line.
fn resolved_native_sync(
    shared: &SharedVm,
    is_synchronized: bool,
    is_static: bool,
    declaring_id: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<(bool, ClassId)> {
    if !is_synchronized {
        return None;
    }
    if native_sync_enabled(shared) {
        return Some((is_static, declaring_id));
    }
    if super::site_cache::site_stats::on() {
        note_unmonitored_synchronized_native(class_name, method_name, descriptor);
    }
    None
}

/// Print, once per method, that a Rust native answered an `ACC_SYNCHRONIZED`
/// method without taking its monitor (`CRATONVM_NATIVE_SYNC` off). Diagnostic
/// only, reached only while the door census is armed; the set is bounded by
/// the number of distinct synchronized natives a run calls.
fn note_unmonitored_synchronized_native(class_name: &str, method_name: &str, descriptor: &str) {
    static SEEN: std::sync::OnceLock<parking_lot::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    let key = format!("{class_name}.{method_name}{descriptor}");
    let fresh = SEEN
        .get_or_init(Default::default)
        .lock()
        .insert(key.clone());
    if fresh {
        eprintln!(
            "[native-sync-census] synchronized method answered by a native without its monitor: {key}"
        );
    }
}

/// `(is_synchronized, is_static, declaring class)` of the method `start`
/// resolves `method_name`/`descriptor` to. Resolves through `MemberResolver`,
/// the one entry point `runtime::resolve::guard` enforces, rather than a raw
/// hierarchy walk; `cm` is the caller's guard. `None` when it does not resolve.
pub(super) fn resolved_method_sync_facts(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    start: ClassId,
    method_name: &str,
    descriptor: &str,
) -> Option<(bool, bool, ClassId)> {
    let resolver = crate::runtime::resolve::MemberResolver::new(shared);
    let found = resolver
        .declared_method(cm, resolver.scope(start), method_name, descriptor)
        .ok()?;
    let (declaring_id, method_index) = resolver.adopt(found).ok()?;
    let method = cm
        .class_store()
        .get(declaring_id)?
        .methods
        .get(method_index as usize)?;
    Some((method.is_synchronized(), method.is_static(), declaring_id))
}

/// `safe_native_call`, holding the monitor of the synchronized method the
/// native answers when `sync` is `Some((is_static, declaring class))`: the
/// receiver (`args[0]`), or the declaring class's mirror for a static method.
/// The monitor is pinned across the call, so the release sees the object's
/// current address, and it is released on `Ok` and `Err` alike. A null or
/// missing receiver is left to the native, exactly as without the monitor.
pub(crate) fn safe_native_call_synchronized(
    shared: &SharedVm,
    thread: &mut JvmThread,
    callback: cratonvm_native_api::NativeCallback,
    args: &[Value],
    sync: Option<(bool, ClassId)>,
) -> MethodCallResult {
    let Some((is_static, declaring_id)) = sync else {
        return crate::vm::safe_native_call(shared, thread, callback, args);
    };
    let mut call_args = args.to_vec();
    let obj = if is_static {
        // The mirror fetch can allocate: pin the arguments across it. On a
        // full heap its first use collects and throws `OutOfMemoryError`
        // (gen r5w1/oom5); the pins drop on that return too.
        let pins = InvokeArgsRootGuard::new(thread, &call_args);
        let mirror = super::constants::class_mirror_or_oom(
            shared,
            thread,
            declaring_id,
            "static-synchronized-monitor",
        )?;
        pins.refresh(&mut call_args);
        drop(pins);
        mirror
    } else {
        match call_args.first() {
            Some(Value::Object(Some(receiver))) => *receiver,
            _ => return crate::vm::safe_native_call(shared, thread, callback, args),
        }
    };
    // Blocks on contention; pins and remaps `call_args` across the wait. The
    // guard releases on `Ok`, `Err` and unwind alike.
    let _monitor =
        crate::vm::vm_exec::SynchronizedMethodGuard::enter(shared, thread, obj, &mut call_args);
    crate::vm::safe_native_call(shared, thread, callback, &call_args)
}

/// `safe_native_call` for a registered native FORCED in front of the bytecode
/// of `class_name.method_name descriptor` (`intercept_force_registered_native`):
/// under [`native_sync_enabled`] it holds that method's monitor when the method
/// is `ACC_SYNCHRONIZED`, exactly as `try_stackless_invoke`'s native arms do.
/// Off, one bool load and the plain call.
///
/// The cached twin (`intercept_force_registered_native_cached`) runs from a
/// `Bytecode` / `VirtualBytecode` entry, which the fills cache for a
/// synchronized method while armed (only `Intrinsic` entries are refused), so
/// it takes the same monitor from the entry's own flags.
pub(super) fn safe_forced_native_call(
    shared: &SharedVm,
    thread: &mut JvmThread,
    callback: cratonvm_native_api::NativeCallback,
    args: &[Value],
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> MethodCallResult {
    let sync = synchronized_native_target(shared, class_name, method_name, descriptor, None);
    safe_native_call_synchronized(shared, thread, callback, args, sync)
}

/// Per-VM memo of the [`synchronized_native_target`] answer for each native
/// REGISTRATION, indexed by its dense `NativeMethodId`: does the slot's own
/// `(class, method, descriptor)` resolve to an `ACC_SYNCHRONIZED` method, and
/// which monitor does it need. It serves the per-CALL by-name doors
/// (`vm_exec::invoke_or_native`'s registry arms, `invoke_special_shared`'s
/// native override, the JIT native site cache's fill), which have no inline
/// cache to keep the lookup off their hot path: after the first call per slot
/// the answer is one relaxed load.
///
/// Lives on `SharedVm::natives` (per VM; AGENTS.md: no process global). The
/// table is sized lazily to the registry's slot count on first use (the
/// registry is immutable once the VM is shared); an id past the end — a slot
/// registered afterwards, which only a test can do — is answered uncached.
///
/// Word layout: bits 63..34 a 30-bit tag, bits 33..32 the kind, bits 31..0
/// the declaring class for a static method's mirror. Two kinds of answer,
/// tagged by what can change them (i11-L5, wave 12):
///
/// * **Found** (kind `1` not synchronized, `2` synchronized instance, `3`
///   synchronized static): the slot's class name resolved and the method was
///   found on it or on a superCLASS. Only a re-pointing of the name (a second
///   definition, a removal, an unload — `class_name_generation`) or an
///   in-place replacement of a class (`resolution_epoch`) can change that, so
///   the tag is the low 30 bits of their SUM. A class definition elsewhere in
///   the process does not refill it.
/// * **Unresolved** (kind `0`, bit 0 set; the fact is "no monitor"): the class
///   is not loaded, the method is not found, or it was found only on a
///   superINTERFACE of a class (interface methods are never synchronized, but
///   a superclass defined later can supersede the answer). A later definition
///   can change these, so the tag is the low 30 bits of
///   `class_definition_epoch() + resolution_epoch()`, the pre-wave-12 rule.
///
/// Every counter is monotone, so each sum strictly increases when either of
/// its terms moves: a false hit would need exactly 2^30 bumps between fill
/// and read. The word `0` is "never filled".
#[derive(Default)]
pub struct NativeSyncFacts {
    slots: std::sync::OnceLock<Box<[std::sync::atomic::AtomicU64]>>,
    /// Refills (every slot, every cause). Counted unconditionally: the refill
    /// path already takes a `class_manager` read. Reported under the door
    /// census (`CRATONVM_DBG_FIELD_SITE`) and read by tests.
    refills: std::sync::atomic::AtomicU64,
}

/// The counters a [`NativeSyncFacts`] word is tagged with, snapshotted
/// together BEFORE a resolution so a definition racing the fill leaves the
/// word stale rather than wrongly current.
#[derive(Clone, Copy)]
struct NativeSyncEpochs {
    /// `class_name_generation() + resolution_epoch()`: a found answer's tag.
    found: u64,
    /// `class_definition_epoch() + resolution_epoch()`: an unresolved one's.
    unresolved: u64,
}

/// The counters are the VM's own ([`cratonvm_classloading::StoreEpochs`],
/// interpreter round i1 wave 20, lane L2): the memo is per VM, so another
/// VM's class loading no longer forces its refills.
impl NativeSyncEpochs {
    #[inline]
    fn found_now(epochs: &cratonvm_classloading::StoreEpochs) -> u64 {
        epochs
            .class_name_generation()
            .wrapping_add(epochs.resolution_epoch())
    }

    #[inline]
    fn unresolved_now(epochs: &cratonvm_classloading::StoreEpochs) -> u64 {
        epochs
            .class_definition_epoch()
            .wrapping_add(epochs.resolution_epoch())
    }

    fn now(epochs: &cratonvm_classloading::StoreEpochs) -> Self {
        Self {
            found: Self::found_now(epochs),
            unresolved: Self::unresolved_now(epochs),
        }
    }
}

/// What a [`NativeSyncFacts`] refill learned: the fact, and whether it may be
/// tagged with the found-answer counters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeSyncAnswer {
    Found(Option<(bool, ClassId)>),
    Unresolved,
}

impl NativeSyncFacts {
    const EPOCH_SHIFT: u32 = 34;
    const EPOCH_MASK: u64 = (1 << 30) - 1;
    const KIND_UNRESOLVED: u64 = 0;
    const KIND_PLAIN: u64 = 1;
    const KIND_SYNC_INSTANCE: u64 = 2;
    const KIND_SYNC_STATIC: u64 = 3;
    /// Bit 0 of an unresolved word, so the word is never `0` (never filled).
    const UNRESOLVED_MARK: u64 = 1;

    /// Refills this VM's memo has done, every slot and cause.
    pub fn refills(&self) -> u64 {
        self.refills.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn slot(
        &self,
        registry: &cratonvm_native_api::NativeMethodRegistry,
        id: cratonvm_native_api::NativeMethodId,
    ) -> Option<&std::sync::atomic::AtomicU64> {
        self.slots
            .get_or_init(|| {
                (0..registry.len())
                    .map(|_| std::sync::atomic::AtomicU64::new(0))
                    .collect()
            })
            .get(id.index())
    }

    /// The fact a word holds, when its tag still matches the counters its
    /// kind is tagged with (`found_now` / `unresolved_now`, read only for the
    /// kind the word holds). `None`: never filled, or stale.
    #[inline]
    fn decode_current(
        word: u64,
        epochs: &cratonvm_classloading::StoreEpochs,
    ) -> Option<Option<(bool, ClassId)>> {
        Self::decode(
            word,
            || NativeSyncEpochs::found_now(epochs),
            || NativeSyncEpochs::unresolved_now(epochs),
        )
    }

    fn decode(
        word: u64,
        found_now: impl FnOnce() -> u64,
        unresolved_now: impl FnOnce() -> u64,
    ) -> Option<Option<(bool, ClassId)>> {
        if word == 0 {
            return None;
        }
        let tag = word >> Self::EPOCH_SHIFT;
        let kind = (word >> 32) & 0b11;
        let current = if kind == Self::KIND_UNRESOLVED {
            unresolved_now()
        } else {
            found_now()
        };
        if tag != (current & Self::EPOCH_MASK) {
            return None;
        }
        if kind == Self::KIND_UNRESOLVED || kind == Self::KIND_PLAIN {
            Some(None)
        } else if kind == Self::KIND_SYNC_INSTANCE {
            Some(Some((false, ClassId::new(word as u32))))
        } else {
            Some(Some((true, ClassId::new(word as u32))))
        }
    }

    fn encode(answer: NativeSyncAnswer, epochs: NativeSyncEpochs) -> u64 {
        let tag = |epoch: u64| (epoch & Self::EPOCH_MASK) << Self::EPOCH_SHIFT;
        match answer {
            // Kind bits `KIND_UNRESOLVED` (zero).
            NativeSyncAnswer::Unresolved => tag(epochs.unresolved) | Self::UNRESOLVED_MARK,
            NativeSyncAnswer::Found(None) => tag(epochs.found) | (Self::KIND_PLAIN << 32),
            NativeSyncAnswer::Found(Some((false, declaring))) => {
                tag(epochs.found) | (Self::KIND_SYNC_INSTANCE << 32) | declaring.as_u32() as u64
            }
            NativeSyncAnswer::Found(Some((true, declaring))) => {
                tag(epochs.found) | (Self::KIND_SYNC_STATIC << 32) | declaring.as_u32() as u64
            }
        }
    }

    /// Count a refill; under the door census, report the running total at
    /// every power of two (a steady workload should stop printing early).
    #[cold]
    fn note_refill(&self) {
        let n = self
            .refills
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        if n.is_power_of_two() && super::site_cache::site_stats::on() {
            eprintln!("[native-sync] memo refills={n}");
        }
    }
}

/// The [`NativeSyncFacts`] answer for `start.method_name descriptor` (the
/// caller resolved `start` from the slot's class name; `None` = not loaded).
/// `Found` only when the answer cannot be changed by a later class
/// definition: see the word layout on [`NativeSyncFacts`].
fn native_sync_answer(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    start: Option<ClassId>,
    method_name: &str,
    descriptor: &str,
) -> NativeSyncAnswer {
    let Some(start) = start else {
        return NativeSyncAnswer::Unresolved;
    };
    let Some((is_synchronized, is_static, declaring_id)) =
        resolved_method_sync_facts(shared, cm, start, method_name, descriptor)
    else {
        return NativeSyncAnswer::Unresolved;
    };
    let from_superinterface = declaring_id != start
        && cm
            .class_store()
            .get(declaring_id)
            .is_some_and(|c| c.is_interface());
    if from_superinterface {
        return NativeSyncAnswer::Unresolved;
    }
    NativeSyncAnswer::Found(is_synchronized.then_some((is_static, declaring_id)))
}

/// The monitor the native registered in slot `id` must hold under
/// [`native_sync_enabled`]: `(is_static, declaring class)` when the slot's
/// own triple resolves to an `ACC_SYNCHRONIZED` method, `None` otherwise or
/// when the switch is off. Memoised per slot in [`NativeSyncFacts`]; a refill
/// takes a `class_manager` read, so the caller must hold no class-manager
/// guard.
pub(crate) fn native_sync_for_id(
    shared: &SharedVm,
    id: cratonvm_native_api::NativeMethodId,
) -> Option<(bool, ClassId)> {
    if !native_sync_enabled(shared) {
        return None;
    }
    let registry = &shared.natives.native_methods;
    let memo = &shared.natives.native_sync_facts;
    let slot = memo.slot(registry, id);
    // No slot (an id past the table) reads as "never filled".
    let word = slot.map_or(0, |s| s.load(std::sync::atomic::Ordering::Relaxed));
    let vm_epochs = cratonvm_classloading::store_epochs(shared.jit.class_layout_domain);
    if let Some(fact) = NativeSyncFacts::decode_current(word, vm_epochs) {
        return fact;
    }
    // Snapshot BEFORE resolving, so a definition racing the fill leaves the
    // word stale rather than wrongly current.
    let epochs = NativeSyncEpochs::now(vm_epochs);
    let (class_name, method_name, descriptor) = registry.triple_of(id)?;
    let answer = {
        let cm = shared.classes.class_manager.read();
        let start = cm.get_loaded_class_id(class_name);
        native_sync_answer(shared, &cm, start, method_name, descriptor)
    };
    if let Some(slot) = slot {
        memo.note_refill();
        slot.store(
            NativeSyncFacts::encode(answer, epochs),
            std::sync::atomic::Ordering::Relaxed,
        );
    }
    match answer {
        NativeSyncAnswer::Found(fact) => fact,
        NativeSyncAnswer::Unresolved => None,
    }
}

/// Stackless invoke: resolve a method and either call native (Handled) or push
/// a bytecode frame (FramePushed).  Returns `CacheMiss` for exotic cases that
/// cannot be handled stacklessly (signature-polymorphic, JNI, etc.), in which
/// case the caller should fall back to the recursive `invoke_shared` path.
pub(super) fn try_stackless_invoke(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
    walk_native_hierarchy: bool,
    is_special: bool,
    // Loader-isolation dispatch override: when `Some`, the bytecode-method
    // lookup uses THIS class_id instead of re-resolving `class_name` (which can
    // pick the wrong same-named per-loader copy). Only the divergent
    // virtual-dispatch caller passes it; `None` everywhere else preserves the
    // legacy name-based resolution byte-for-byte.
    dispatch_class_override: Option<ClassId>,
    // JVMS §5.4.6 selection input for a virtual/interface call: the
    // receiver's runtime class and what the call site resolved. Applied at
    // step 4 only when the class being searched IS that receiver class.
    // `None` for invokestatic / invokespecial.
    selection: Option<(ClassId, crate::runtime::resolve::selection::ResolvedRef)>,
) -> Result<CachedCallResult, MethodCallFailed> {
    use crate::runtime::frame::padded_bytecode;
    use crate::vm::{coerce_value_for_return, native_return_pushed_to_stack, safe_native_call};

    // The denominator of the per-invoke lookup count — see
    // `cratonvm_native_api::registry::lookup_census`. This is the every-invoke
    // entry point, so it is what a "one lookup per invoke" restructuring would
    // be dividing the registry probes by.
    cratonvm_native_api::registry::lookup_census::probe(
        cratonvm_native_api::registry::lookup_census::INVOKE_STACKLESS,
    );

    // The receiver, for the two redefine-immunity gates below. Only the ZIP arm
    // of the immunity looks at it (see `zip_immunity_waived_for_receiver`), and
    // only after a redefinition has actually happened, so this costs a slice
    // index on every invoke and nothing else.
    let receiver_for_immunity = match args.first() {
        Some(Value::Object(Some(obj))) => Some(*obj),
        _ => None,
    };

    // `CRATONVM_DBG_DEADREF_STORE`: were the arguments ALREADY dead on entry?
    //
    // `[deadref-arg]` fires where the arguments are laid into the callee's
    // locals, which is the last statement of a long prologue and cannot say
    // whether the collection that killed them ran inside that prologue or
    // before this function was ever called. Asking the same question at entry
    // splits it: a hit HERE means the caller handed down a slice it had already
    // held across a collection, and the fix belongs upstream; a hit only at the
    // frame build means the prologue is what needs bracketing.
    crate::runtime::frame::note_dead_arg_pub(args, "try_stackless_invoke ENTRY");

    // Memo for the constant `DowncallHandle.type()` triple resolved in the
    // receiver-class-gated arm below (native-dispatch-memoization §3, B4).
    // ONE STATIC, ONE TRIPLE: the single `.callback` below passes three
    // literals, and no other arm of this function touches this cell. In
    // particular the `.or_else` chain's lookups — whose class name is rewritten
    // from `[`-prefixed receivers and therefore varies at runtime — are NOT
    // memoizable by this mechanism and are deliberately left re-resolving.
    static NCS_DOWNCALL_HANDLE_TYPE: cratonvm_native_api::NativeCallSite =
        cratonvm_native_api::NativeCallSite::new();

    // T15: Array types (`[LFoo;`, `[I`, etc.) inherit their method
    // dispatch from `java.lang.Object` (JVMS §4.4.1).  Treat any invoke
    // on an array-typed receiver as if the receiver were `java.lang.Object`
    // — otherwise lookups for `clone()` on `[LFoo;` dead-end in CacheMiss
    // and fall through to a path that throws CloneNotSupportedException.
    let class_name = if class_name.starts_with('[') {
        "java/lang/Object"
    } else {
        class_name
    };

    // H2-CID0, CLONE face -- the `java.lang.Enum` half.
    //
    // `Enum.clone()` is `protected final Object clone() { throw new
    // CloneNotSupportedException(); }` and nothing else, so a dispatch that
    // lands there is never something the program asked for: javac will not
    // compile a call to it. Arriving here means virtual dispatch was driven by
    // a receiver whose class is not the one the call site named -- either an
    // array dispatched through its COMPONENT class id (`jit/helpers.rs` names
    // this exact shape for enum-typed arrays) or the stale/reclaimed-ObjectRef
    // family seen through `clone()` instead of through a `checkcast`.
    //
    // `java.lang.Thread.clone()` has the identical property and is reported
    // here too. It also has a reporter in `execute_invoke_kind`, and that used
    // to be the only one — which left the route that matters UNCOVERED: a
    // JIT-compiled call site reaches `invoke_or_native` ->
    // `invoke_on_class_shared_inner` -> here, and never passes through
    // `execute_invoke_kind` at all. The H2 suite sweep that reopened
    // `bug-h2-testtemptables-clonenotsupportedexception-thread-clone-frame`
    // ran on a binary that already carried the `execute_invoke_kind` reporter
    // and quoted no verdict with the trace, and the suite runner's default mode
    // is JIT-on. A duplicate verdict on the interpreter route costs one extra
    // rate-limited log line; a missing one on the JIT route costs the session.
    // The `site` string distinguishes them.
    //
    // This closes what the retired
    // `bug-h2-testmultithread-concurrent-update-timeout` write-up asked for:
    // `TestMultiThread.testConcurrentUpdate` failed 2 runs in 10 with
    // `General error: "java.lang.CloneNotSupportedException"` on `COMMIT`
    // (H2's `Page.copy()` -> `Page.clone()`), and neither occurrence produced a
    // verdict because nothing on the clone path asked the heap. The old-gen
    // reclamation ring answers even after the allocator has re-served the
    // block, which is the case where the receiver reads back as a VALID object
    // of an unrelated class -- exactly how a `Page` becomes something whose
    // `clone()` throws.
    //
    // Cost on a healthy run: one `&str` comparison per stackless invoke, which
    // fails on length for every method whose name is not five bytes.
    if method_name == "clone" && matches!(class_name, "java/lang/Enum" | "java/lang/Thread") {
        if let Some(Value::Object(Some(recv))) = args.first().copied() {
            crate::memory::reclaim_guard::report_impossible_dispatch_terminal(
                shared,
                thread,
                recv,
                "impossible clone (stackless invoke)",
                &format!("{class_name}.{method_name}{descriptor}"),
                None,
            );
        }
    }

    // Cache the descriptor's return-type byte once for the write-side
    // coercion applied to every native callback's pushed return value.
    // Same class of bug as the read-side `getfield` coercion: a primitive
    // smuggled through the heap as an Object pointer (e.g. `String.charAt`
    // returning a char-array element via `get_array_element`) must be
    // reinterpreted as the descriptor's primitive type before reaching
    // the caller's operand stack — otherwise the next `pop_int` blows up
    // with `expected int on stack, got ref(...)`.
    let ret_type = crate::jit::return_type(descriptor);

    // A nested-archive subclass calls `super(file)` with invokespecial. Mockito
    // can redefine JarFile, so the ordinary redefine guard would otherwise run
    // the real JDK constructor. Its ZipFile state is not present on CratonVM's
    // compact native JarFile representation; retain the bridge for precisely
    // the registered File constructor shapes. Ordinary mock calls remain
    // redefine-aware.
    //
    // `--compatible` only in effect: under `--jdk-only` these four triples are
    // retired (`RETIRED_SHADOW_JARFILE_TRIPLES`), so `find` misses and the real
    // constructor runs. That miss is the point -- this arm calling `find`
    // directly is what kept every armed-dial run building native `JarFile`s,
    // the half-retirement behind wave 4's `+34`.
    if is_special
        && class_name == "java/util/jar/JarFile"
        && method_name == "<init>"
        && matches!(
            descriptor,
            "(Ljava/io/File;)V"
                | "(Ljava/io/File;Z)V"
                | "(Ljava/io/File;ZI)V"
                | "(Ljava/io/File;ZILjava/lang/Runtime$Version;)V"
        )
    {
        if let Some(callback) =
            shared
                .natives
                .native_methods
                .find("java/util/jar/JarFile", method_name, descriptor)
        {
            // §4 census: this arm holds a callback and no id, and returns
            // without reaching the increment below.
            mark_stackless_exotic_natives_incomplete(shared);
            safe_native_call(shared, thread, callback, args)?;
            return Ok(CachedCallResult::Handled);
        }
    }

    // A subclass `super.close()` is an invokespecial whose constant-pool
    // owner is JarFile even though the concrete implementation is inherited
    // from ZipFile. Mockito can redefine JarFile for ordinary mock calls; the
    // general redefine guard correctly yields to that advice, but must not
    // make this statically-bound superclass call fall into ZipFile's real
    // bytecode (its `res` field is absent on CratonVM-native JarFiles).
    // Limit this bypass to the exact invokespecial close shape. Virtual mock
    // calls still take the normal redefine-aware dispatch path.
    //
    // Under `--jdk-only` no `ZipFile.close` native is registered at all (the
    // row is retired and `jar_manifest.rs` registers its Intrinsic in
    // `--compatible` only), so `find` misses and `super.close()` runs the real
    // body over the real `res` a real `JarFile` constructor set.
    if is_special
        && matches!(
            class_name,
            "java/util/jar/JarFile" | "java/util/zip/ZipFile"
        )
        && method_name == "close"
        && descriptor == "()V"
    {
        if let Some(callback) =
            shared
                .natives
                .native_methods
                .find("java/util/zip/ZipFile", method_name, descriptor)
        {
            // §4 census: uncounted arm, same as the constructor bridge above.
            mark_stackless_exotic_natives_incomplete(shared);
            safe_native_call(shared, thread, callback, args)?;
            return Ok(CachedCallResult::Handled);
        }
    }

    // Registered natives that must beat real-JDK bytecode on the declaring
    // class (URL.getHost DNS loop, ClassLoader assertion lock NPE, etc.) are
    // NOT re-checked here: both callers (`execute_invoke_kind`,
    // `execute_invokestatic`) run `intercept_force_registered_native` on this
    // exact `(class_name, method_name, descriptor, args)`, unconditionally,
    // before calling in, and proceed only when it declined. The second evaluation of
    // its ~55-branch name gauntlet was pure repetition on every slow-path
    // invoke. A new caller must keep that contract.

    // WP2.2 / Surefire: `try_stackless_invoke` does `native_methods.find(class_name, …)`
    // first, then — when `walk_native_hierarchy` is false (invokevirtual fast path) —
    // skips the superclass walk if the **receiver's class** already declares bytecode
    // for the method. `java.lang.reflect.Method` has real JDK bytecode for `invoke`,
    // so `has_own_bytecode` is true, the `or_else` returns None, and we never consult
    // `java/lang/reflect/Method` in the registry. Force the registered
    // `native_method_invoke` (same triple as essentials) so reflection works.
    // Scoped to call sites that actually resolve on java/lang/reflect/Method
    // (mirrors `native_override_for_cached_reflect_invoke`). Matching on
    // name+descriptor ALONE hijacked every `invoke(Object,Object[])Object`
    // in the wild — e.g. Gradle's `ServiceMethod.invoke` implementations,
    // whose receivers then hit `native_method_invoke` and died with
    // "Method.invoke: no declaring class" (ProjectBuilder bootstrap,
    // Spring Boot buildSrc suite).
    if class_name == "java/lang/reflect/Method"
        && method_name == "invoke"
        && descriptor == "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;"
    {
        // ONE STATIC, ONE TRIPLE — see the discipline note in
        // `dispatch_virtual.rs`. A `NativeCallSite` is keyed on the registry
        // generation alone and does not re-verify the triple on a warm hit, so
        // a cell must be unreachable with a second triple. The `if` above has
        // already proven all three components by exact equality, so this cell
        // sees exactly one. It is its own cell rather than a share of
        // `dispatch_virtual`'s identically-keyed `NCS_METHOD_INVOKE`, matching
        // that module's rule that no cell is reachable from more than one call.
        static NCS_METHOD_INVOKE: cratonvm_native_api::NativeCallSite =
            cratonvm_native_api::NativeCallSite::new();
        if let Some(callback) = NCS_METHOD_INVOKE.callback(
            &shared.natives.native_methods,
            "java/lang/reflect/Method",
            method_name,
            descriptor,
        ) {
            // §4 census: `NativeCallSite` hands back a callback, never an id,
            // and this arm returns `Handled` without reaching the increment
            // below — so every reflective `Method.invoke` reads as zero.
            mark_stackless_exotic_natives_incomplete(shared);
            let result = safe_native_call(shared, thread, callback, args)?;
            if let Some(value) = result.filter(|_| ret_type != b'V') {
                push_invoke_return_value(
                    &mut thread.frames[frame_idx].stack,
                    coerce_value_for_return(value, ret_type),
                )?;
                native_return_pushed_to_stack(shared, thread);
            }
            return Ok(CachedCallResult::Handled);
        }
    }
    // Same class-scoping as the Method.invoke block above: an unscoped match
    // hijacked every `newInstance(Object[])Object` (e.g. Objenesis
    // ObjectInstantiator implementations).
    if class_name == "java/lang/reflect/Constructor"
        && method_name == "newInstance"
        && descriptor == "([Ljava/lang/Object;)Ljava/lang/Object;"
    {
        // ONE STATIC, ONE TRIPLE — as above; the guard proves the triple.
        static NCS_CONSTRUCTOR_NEW_INSTANCE: cratonvm_native_api::NativeCallSite =
            cratonvm_native_api::NativeCallSite::new();
        if let Some(callback) = NCS_CONSTRUCTOR_NEW_INSTANCE.callback(
            &shared.natives.native_methods,
            "java/lang/reflect/Constructor",
            method_name,
            descriptor,
        ) {
            // §4 census: uncounted arm, same shape as `Method.invoke` above.
            mark_stackless_exotic_natives_incomplete(shared);
            let result = safe_native_call(shared, thread, callback, args)?;
            if let Some(value) = result.filter(|_| ret_type != b'V') {
                push_invoke_return_value(
                    &mut thread.frames[frame_idx].stack,
                    coerce_value_for_return(value, ret_type),
                )?;
                native_return_pushed_to_stack(shared, thread);
            }
            return Ok(CachedCallResult::Handled);
        }
    }

    // 1. Check native override first (same priority as invoke_or_native).
    //    Walk the superclass chain if:
    //    - method is NOT <init> (constructors are NOT inherited)
    //    - For static calls: walk when the constant pool names a subclass that
    //      does not declare the method itself (an inherited static); a
    //      declaration on the named class hides every ancestor's
    //    - For virtual/special calls: ONLY walk if the target class does NOT have
    //      its own bytecode for the method. If it does, the bytecode override takes
    //      priority (e.g. URI.toString() must NOT be short-circuited by
    //      Object.toString() native). If it doesn't (e.g. RunnerClassLoader.getParent()),
    //      walking finds the parent's native (ClassLoader.getParent native).
    //
    // JDK-only §7: the PRIMARY lookup in the chain below (the one keyed on
    // `class_name` itself) goes through `resolve_step1_native`. The receiver-
    // gated probes, the SSL impl->API aliases, the `DowncallHandle` adapters
    // and the superclass walk still call bare `find`: each of those resolves a
    // DIFFERENT triple from `(class_name, method_name, descriptor)`, so a
    // kind/id looked up on `class_name` would describe the wrong slot. Wave 2
    // routes them by giving each its own resolved triple.
    let mut step1_native_id: Option<cratonvm_native_api::NativeMethodId> = None;
    let mut step1_refusal: Option<cratonvm_types::error::JdkOnlyViolation> = None;
    // The superclass walk's registration slot, read only by the
    // synchronized-method memo below (`native_sync_for_id`) and never counted
    // for the §4 census (the walk's triple is not enumerable; see
    // `UNCOUNTED_STACKLESS_NATIVES`). Set only where the slot's memo and
    // `synchronized_native_target`'s per-call lookup name the same method —
    // see the walk.
    let mut walk_native_id: Option<cratonvm_native_api::NativeMethodId> = None;
    // The receiver's runtime class for a virtual/interface call (`None` for
    // invokestatic / invokespecial), for the walk's retired-row mask (round 14
    // wave 2, lane shadow).
    let walk_receiver: Option<ClassId> = selection.as_ref().map(|(receiver, _)| *receiver);
    let native_cb = match args.first() {
        Some(Value::Object(Some(obj)))
            if method_name == "type" && descriptor == "()Ljava/lang/invoke/MethodType;" =>
        {
            let is_downcall = shared
                .classes
                .class_manager
                .read()
                .get_class(shared.mem.heap.class_id_of(*obj))
                .map(|class| class.name.as_ref() == "java/lang/foreign/DowncallHandle")
                .unwrap_or(false);
            if is_downcall {
                // Fully-constant triple (native-dispatch-memoization §3, B4).
                // Only ever reached with this one triple, so the per-call-site
                // memo cannot be asked to serve a different one.
                NCS_DOWNCALL_HANDLE_TYPE.callback(
                    &shared.natives.native_methods,
                    "java/lang/foreign/DowncallHandle",
                    "type",
                    "()Ljava/lang/invoke/MethodType;",
                )
            } else {
                surefire_lazy_launcher_discover_native(shared, method_name, descriptor, *obj)
            }
        }
        Some(Value::Object(Some(obj)))
            if matches!(method_name, "invoke" | "invokeExact" | "invokeBasic") =>
        {
            let is_downcall = shared
                .classes
                .class_manager
                .read()
                .get_class(shared.mem.heap.class_id_of(*obj))
                .map(|class| class.name.as_ref() == "java/lang/foreign/DowncallHandle")
                .unwrap_or(false);
            if is_downcall {
                shared.natives.native_methods.find(
                    "java/lang/foreign/DowncallHandle",
                    method_name,
                    "([Ljava/lang/Object;)Ljava/lang/Object;",
                )
            } else {
                surefire_lazy_launcher_discover_native(shared, method_name, descriptor, *obj)
            }
        }
        Some(Value::Object(Some(obj))) => {
            surefire_lazy_launcher_discover_native(shared, method_name, descriptor, *obj)
        }
        _ => None,
    }
    .or_else(|| {
        resolve_step1_native(
            shared,
            class_name,
            method_name,
            descriptor,
            dispatch_class_override,
            &mut step1_native_id,
            &mut step1_refusal,
        )
        .or_else(|| {
            class_name
                .starts_with("sun/security/ssl/SSLContextImpl")
                .then(|| {
                    shared.natives.native_methods.find(
                        "javax/net/ssl/SSLContext",
                        method_name,
                        descriptor,
                    )
                })
                .flatten()
        })
        .or_else(|| {
            class_name
                .starts_with("sun/security/ssl/SSLSocketFactoryImpl")
                .then(|| {
                    shared.natives.native_methods.find(
                        "javax/net/ssl/SSLSocketFactory",
                        method_name,
                        descriptor,
                    )
                })
                .flatten()
        })
        .or_else(|| {
            class_name
                .starts_with("sun/security/ssl/SSLSocketImpl")
                .then(|| {
                    shared.natives.native_methods.find(
                        "javax/net/ssl/SSLSocket",
                        method_name,
                        descriptor,
                    )
                })
                .flatten()
        })
    })
    .or_else(|| {
        if method_name == "<init>" {
            return None;
        }
        // Loader-precise start: prefer the caller's own already-resolved
        // dispatch class (computed by the caller via loader-aware
        // resolution for invokespecial self/super calls and the private-
        // invokevirtual fast path) over the flat, loader-blind
        // `get_loaded_class_id` name lookup below, which collapses to
        // whichever same-named class loaded FIRST process-wide. Two
        // classloaders each defining their own class named `class_name`
        // (e.g. `Upgrade.loadH2`'s old H2 driver vs. the current H2 build,
        // both declaring `org/h2/command/Parser`) otherwise walk the
        // WRONG class's hierarchy here: this native-override pre-check is
        // an independent lookup from the bytecode-resolution one further
        // below that already honors `dispatch_class_override`, so without
        // this it could force a same-named ANCESTOR's native override
        // (e.g. `org/h2/command/ParserBase.read()V`) onto a receiver whose
        // own, unrelated class of the same name declares that method
        // itself and doesn't extend that ancestor at all. See
        // bug-h2-suite-residual-fail-triage-FIXED.md
        // (TestUpgrade's `ParserBase.getSyntaxError`/`Token.start()` NPE).
        let start_cid = |cm: &crate::classloading::ClassManager| {
            dispatch_class_override.or_else(|| cm.get_loaded_class_id(class_name))
        };
        // Skip the hierarchy walk if the class declares the method itself.
        // For a virtual call its own body overrides any ancestor's native;
        // for a static call its own declaration IS the resolved method
        // (JVMS §5.4.3.3 stops at the first declaring class, and static
        // methods hide rather than override), so an ancestor's native for the
        // same signature is not the method being invoked. That second case
        // used to walk anyway (`walk_native_hierarchy`), and ran the
        // ancestor's native in place of the owner's own static body.
        //
        // One `class_manager` guard for this check and the ancestor walk
        // below; they used to take two back to back.
        let cm = shared.classes.class_manager.read();
        // For the static walk the hiding rule is applied only to a class
        // with real class bytes: a compatibility stub's declarations are
        // fabricated, and its statics keep reaching an ancestor's native
        // exactly as before.
        let (start_declares, start_has_real_bytes) = start_cid(&cm)
            .and_then(|cid| cm.get_class(cid))
            .map(|cls| {
                (
                    cls.find_method(method_name, descriptor).is_some(),
                    cls.origin.has_real_bytes(),
                )
            })
            .unwrap_or((false, false));
        if start_declares && (!walk_native_hierarchy || start_has_real_bytes) {
            return None;
        }
        let mut cid = start_cid(&cm)?;
        // Where the walk began, for the retired-row mask below.
        let walk_start = cid;
        loop {
            let parent_id = cm.get_class(cid)?.superclass?;
            let parent = cm.get_class(parent_id)?;
            let has_bytecode = parent.find_method(method_name, descriptor).is_some();
            // JVMTI redefine guard, per ancestor. Mockito's inline mock
            // maker mocks a CONCRETE class (e.g. `java.net.HttpURLConnection`)
            // by redefining that class directly and weaving advice into its
            // methods, then instantiating a trivial marker SUBCLASS (which
            // declares only a couple of identity/interceptor-plumbing
            // methods; it does NOT override every mockable method the way
            // an interface mock's generated subclass does). So the RECEIVER
            // here is that marker subclass, which itself was never redefined
            // Only the ANCESTOR (`HttpURLConnection`) was. The top-level
            // `native_shadow_suppressed_by_redefine(shared, class_name)`
            // check below only inspects the receiver's own class and misses
            // this entirely, so a native registered on the redefined
            // ancestor kept winning over its now-woven bytecode: every call
            // on the mock (including Mockito's own stubbing/verification
            // calls) silently bypassed the mock's advice and ran the real
            // native instead. Skip an ancestor's native the same way the
            // receiver-class check does.
            let parent_redefined = native_shadow_suppressed_in(&cm, &parent.name)
                && !redefine_immune_forced_native_for_receiver(
                    shared,
                    &parent.name,
                    method_name,
                    descriptor,
                    receiver_for_immunity,
                );
            if !parent_redefined {
                // `resolve_id` + `callback_of` is `find` (same prefilter,
                // exact slot and quirk fallback) that also names the slot.
                let registry = &shared.natives.native_methods;
                if let Some((id, cb)) = registry
                    .resolve_id(&parent.name, method_name, descriptor)
                    .and_then(|id| registry.callback_of(id).map(|cb| (id, cb)))
                    // `--jdk-only`: a `Bridge` above a retired row is masked
                    // and the walk goes on (round 14 wave 2, lane shadow;
                    // `retired_row_masks_ancestor_bridge`).
                    // The enforcement dial too, which this walk never asked
                    // (the virtual populate's walk does); inert unless
                    // `CRATONVM_ENFORCE_NATIVE_SHADOW` covers `parent`. `cm` is
                    // held, and the dial's probe takes it `read_recursive()`.
                    .filter(|(id, _)| {
                        let kind = registry
                            .kind_of_id(*id)
                            .unwrap_or(cratonvm_native_api::NativeKind::Bridge);
                        !retired_row_masks_ancestor_bridge(
                            shared,
                            &cm,
                            (walk_start, walk_receiver),
                            parent_id,
                            method_name,
                            descriptor,
                            kind,
                        ) && !crate::runtime::interpreter::jdk_only_dial_yields_to_bytecode(
                            shared,
                            &parent.name,
                            method_name,
                            descriptor,
                            kind,
                        )
                    })
                {
                    // The slot's synchronized-method memo resolves from
                    // `parent`, `synchronized_native_target` from the start
                    // class. They name the same method when the start class
                    // does not declare it (the classes between it and
                    // `parent` do not, or the walk would have stopped) and
                    // `parent`'s name finds `parent` itself.
                    if !start_declares && cm.get_loaded_class_id(&parent.name) == Some(parent_id) {
                        walk_native_id = Some(id);
                    }
                    return Some(cb);
                }
            }
            // S107 collection-toString fix: if this parent has bytecode and no
            // same-parent native override, bytecode wins over deeper native
            // ancestors (e.g. AbstractCollection.toString over Object.toString).
            if has_bytecode {
                return None;
            }
            cid = parent_id;
        }
    });
    // JVMTI redefine guard: when an agent (e.g. a Mockito inline mock) has
    // redefined this class, its woven bytecode is authoritative. Drop any
    // native override so dispatch falls through to the (instrumented)
    // bytecode and the advice runs. Fast-pathed on `any_class_redefined`.
    // Reflection-metadata natives stay authoritative (see
    // `redefine_immune_reflection_native`).
    let native_cb = if native_shadow_suppressed_by_redefine(shared, class_name)
        && !redefine_immune_forced_native_for_receiver(
            shared,
            class_name,
            method_name,
            descriptor,
            receiver_for_immunity,
        ) {
        None
    } else {
        native_cb
    };
    let native_cb = if synthetic_stub_should_yield_to_real_bytecode(
        shared,
        class_name,
        method_name,
        descriptor,
    ) {
        None
    } else {
        native_cb
    };
    // (`ThreadPoolExecutor.execute(Runnable)` receiver-shape check deleted
    // 2026-08-06. `native_es_execute` is now tagged `SyntheticStub` and
    // `java/util/concurrent/ThreadPoolExecutor` is on the real-protected-stub
    // allow-list, so the `synthetic_stub_should_yield_to_real_bytecode` guard
    // immediately above already drops this native shadow whenever the real
    // `execute()` body is loaded — for every receiver, with no field probe.
    // See `force_native_over_real_jdk_bytecode` in `native_override.rs`.)
    // A `JdkOnly` refusal recorded by `resolve_step1_native` is authoritative:
    // §1.3 forbids silently substituting some other implementation for a
    // refused stub, so this must surface as an error rather than fall through
    // to the bytecode path. Never set under `Compatible`.
    if let Some(violation) = step1_refusal {
        return Err(MethodCallFailed::InternalError(VmError::JdkOnly(violation)));
    }
    // The three guards above (redefine suppression, synthetic-stub yield,
    // ThreadPoolExecutor receiver shape) can veto step 1's callback, and the
    // `.or_else` arms below can supply a DIFFERENT one for a different triple.
    // Either way the step-1 slot handle no longer describes what is about to
    // run, so drop it: an invocation counted against the wrong slot is worse
    // than an uncounted one.
    if native_cb.is_none() {
        step1_native_id = None;
        walk_native_id = None;
    }
    // Real-JDK Linker can adapt a void downcall through a generic
    // MethodHandle. Its field 0 retains the actual DowncallHandle. Preserve
    // the call-site arguments but substitute that target for native dispatch.
    let mut downcall_adapter_args: Option<Vec<Value>> = None;
    let native_cb = native_cb.or_else(|| {
        // This is an adapter for MethodHandle itself, not a general fallback
        // for any method named invoke*.  In particular, JUnit's executable
        // invocation path reaches methods with those names on ordinary
        // zero-field objects; treating those objects as Linker adapters reads
        // a non-existent slot 0 and leaves the interpreter retrying the call.
        if class_name != "java/lang/invoke/MethodHandle"
            || !matches!(method_name, "invoke" | "invokeExact" | "invokeBasic")
        {
            return None;
        }
        let adapter = match args.first() {
            Some(Value::Object(Some(adapter))) => *adapter,
            _ => return None,
        };
        // This is a narrow adaptation for a real-JDK MethodHandle wrapper
        // around our synthetic DowncallHandle. `invoke` is an ordinary method
        // name too (notably JUnit's InterceptingExecutableInvoker.invoke), so
        // probing field 0 before establishing that the receiver is actually a
        // MethodHandle subclass turns every unrelated zero-field receiver into
        // an OOB heap-field read. Apart from the diagnostic flood, returning a
        // benign null from that probe can strand the caller in a retry loop.
        //
        // Use the runtime receiver hierarchy rather than `class_name`: the
        // invoked method can be resolved on an inherited MethodHandle owner
        // while the adapter itself is a concrete JDK subclass.
        let is_method_handle_adapter = {
            let cm = shared.classes.class_manager.read();
            let adapter_class = shared.mem.heap.class_id_of(adapter);
            cm.get_loaded_class_id("java/lang/invoke/MethodHandle")
                .map(|method_handle_class| {
                    adapter_class == method_handle_class
                        || cm.is_subclass_of(adapter_class, method_handle_class)
                })
                .unwrap_or(false)
        };
        if !is_method_handle_adapter {
            return None;
        }
        let target = match shared.mem.heap.get_field(adapter, 0) {
            Value::Object(Some(target)) => target,
            _ => return None,
        };
        let is_downcall = shared
            .classes
            .class_manager
            .read()
            .get_class(shared.mem.heap.class_id_of(target))
            .map(|class| class.name.as_ref() == "java/lang/foreign/DowncallHandle")
            .unwrap_or(false);
        if !is_downcall {
            return None;
        }
        let callback = shared.natives.native_methods.find(
            "java/lang/foreign/DowncallHandle",
            method_name,
            "([Ljava/lang/Object;)Ljava/lang/Object;",
        )?;
        let mut routed = args.to_vec();
        routed[0] = Value::Object(Some(target));
        downcall_adapter_args = Some(routed);
        Some(callback)
    });
    if crate::runtime::env_cache::bd_debug()
        && (method_name == "intValue"
            || (class_name.contains("BigDecimal")
                && (method_name == "<init>" || method_name == "intValue")))
    {
        eprintln!(
            "[try_stackless_invoke] class_name={} method={} desc={} native_cb={} walk_native={}",
            class_name,
            method_name,
            descriptor,
            native_cb.is_some(),
            walk_native_hierarchy
        );
    }
    if method_name == "<init>"
        && crate::runtime::env_cache::dbg_sttrace()
        && (class_name.contains("Exception")
            || class_name.contains("Throwable")
            || class_name.contains("Error"))
    {
        eprintln!("STTRACE_DBG_TSI class={class_name} desc={descriptor} native_cb={} walk={walk_native_hierarchy}",
                  native_cb.is_some());
    }
    // invoke/invokeExact are signature-polymorphic. Their native bridge is
    // registered under the erased Object[] descriptor, while a real call site
    // carries its concrete descriptor. Route those concrete signatures through
    // the bridge so synthetic combinators (notably guardWithTest around a
    // foreign downcall) retain and dispatch their target.
    let native_cb = native_cb.or_else(|| {
        if class_name == "java/lang/invoke/MethodHandle"
            && matches!(method_name, "invoke" | "invokeExact" | "invokeBasic")
        {
            shared.natives.native_methods.find(
                "java/lang/invoke/MethodHandle",
                method_name,
                "([Ljava/lang/Object;)Ljava/lang/Object;",
            )
        } else {
            None
        }
    });
    // THE door for `MethodHandle.invoke`: the interpreter's stackless path,
    // not `vm_exec`'s signature-polymorphic block, which the ClassId-resolved
    // route reaches and this one does not. Measured by arming there first and
    // watching the consuming native print `None` for all sixteen rows.
    //
    // `invoke` is the entry whose collect-or-passthrough answer depends on the
    // type the CALLER WROTE — `mh.invoke((String[]) null)` passes the null
    // through, `mh.invoke((Object) null)` collects it — and `descriptor` here
    // is exactly that type. `invokeExact` needs the same channel for a
    // different reason: its whole rule is a comparison AGAINST the call site.
    // `vm_exec::arms_poly_call_site` owns which names arm and why. See
    // `cratonvm_native_api::poly_call_site`.
    if crate::vm::vm_exec::arms_poly_call_site(class_name, method_name) {
        cratonvm_native_api::poly_call_site::arm(descriptor);
    }
    if crate::runtime::env_cache::dbg_mh_stack()
        && matches!(method_name, "invoke" | "invokeExact" | "invokeBasic")
    {
        eprintln!(
            "[MH_STACK] class={class_name} method={method_name} desc={descriptor} native_cb={} args={}",
            native_cb.is_some(),
            args.len()
        );
    }
    if crate::runtime::env_cache::dbg_mh_adapter()
        && method_name == "invokeExact"
        && descriptor == "(Ljava/lang/foreign/MemorySegment;Ljava/lang/foreign/MemorySegment;IILjava/lang/foreign/MemorySegment;)V"
    {
        if let Some(Value::Object(Some(receiver))) = args.first() {
            let target = match shared.mem.heap.get_field(*receiver, 0) {
                Value::Object(Some(target)) => Some(target),
                _ => None,
            };
            let target_class = target.and_then(|target| {
                shared
                    .classes.class_manager
                    .read()
                    .get_class(shared.mem.heap.class_id_of(target))
                    .map(|class| class.name.to_string())
            });
            eprintln!(
                "[MH_ADAPTER] class={class_name} target={} f0={:?} f1={:?} f2={:?} f3={:?} f4={:?} f16={:?} f17={:?} f18={:?} f19={:?} f20={:?} arg1={:?}",
                target_class.as_deref().unwrap_or("<unknown>"),
                shared.mem.heap.get_field(*receiver, 0),
                shared.mem.heap.get_field(*receiver, 1),
                shared.mem.heap.get_field(*receiver, 2),
                shared.mem.heap.get_field(*receiver, 3),
                shared.mem.heap.get_field(*receiver, 4),
                shared.mem.heap.get_field(*receiver, 16),
                shared.mem.heap.get_field(*receiver, 17),
                shared.mem.heap.get_field(*receiver, 18),
                shared.mem.heap.get_field(*receiver, 19),
                shared.mem.heap.get_field(*receiver, 20),
                args.get(1),
            );
        }
    }
    if let Some(callback) = native_cb {
        // §4 census, at the point of actual dispatch: one relaxed increment on
        // a handle already in hand — no hashing, no allocation. `None` here
        // means the callback came from one of the exotic arms, which resolve
        // other triples and are the wave-2 census gap noted at step 1.
        if let Some(id) = step1_native_id {
            shared.natives.native_methods.record_invocation(id);
        } else {
            // `None` is the gap, and this is where it is declared rather than
            // merely commented. The dispatch below is about to run a native
            // that nothing will count; mark the enumerable triples that can
            // reach here so the census reports them as floors. See
            // [`UNCOUNTED_STACKLESS_NATIVES`] — including which two arms are
            // NOT enumerable and remain silent.
            mark_stackless_exotic_natives_incomplete(shared);
        }
        let call_args = downcall_adapter_args.as_deref().unwrap_or(args);
        // Step 1 answers before method resolution, so the synchronized flag of
        // the method this native stands for is looked up here. A step-1 slot
        // (`resolve_step1_native`'s own `(class_name, method, descriptor)`
        // registration) with no loader override resolves from the same class
        // either way, so it reads the per-slot memo: one relaxed load instead
        // of a `class_manager` read and a resolver lookup on every uncached
        // (e.g. megamorphic) native call, now that the monitor is on in every
        // mode. The census (switch disarmed) keeps the lookup. A slot the
        // superclass walk found reads the memo too (wave 11), under the
        // conditions the walk states where it records `walk_native_id`.
        let memo_id = match (step1_native_id, walk_native_id) {
            (Some(id), _) if dispatch_class_override.is_none() => Some(id),
            (None, Some(id)) => Some(id),
            _ => None,
        };
        let sync = match memo_id {
            Some(id) if native_sync_enabled(shared) => native_sync_for_id(shared, id),
            _ => synchronized_native_target(
                shared,
                class_name,
                method_name,
                descriptor,
                dispatch_class_override,
            ),
        };
        let result = safe_native_call_synchronized(shared, thread, callback, call_args, sync)?;
        if let Some(value) = result {
            // Signature-polymorphic MethodHandle natives are registered with an
            // erased Object return and therefore box primitive results. The
            // current bytecode descriptor is concrete, so use the common
            // call-site adapter before the typed return-slot coercion. This must
            // cover every primitive: Netty uses invokeExact(Thread)Z here, while
            // Panama exercises the float path that originally motivated it.
            let value = if class_name == "java/lang/invoke/MethodHandle"
                && matches!(method_name, "invoke" | "invokeExact" | "invokeBasic")
            {
                crate::vm::unbox_poly_return_checked(
                    shared,
                    thread,
                    Some(value),
                    descriptor,
                    method_name,
                )?
                .unwrap_or(Value::Object(None))
            } else {
                value
            };
            if crate::runtime::env_cache::dbg_stackless() {
                eprintln!(
                    "[STACKLESS_RET] {}.{}{} -> {:?}",
                    class_name, method_name, descriptor, value
                );
            }
            if ret_type != b'V' {
                // T18.K4 — tag-exact push for J/D native-override return values.
                // `coerce_value_for_return` may widen/narrow the type-erased
                // native result; we then push via the category-2 aware path so
                // J/D retain their bits across the operand-stack boundary.
                push_invoke_return_value(
                    &mut thread.frames[frame_idx].stack,
                    coerce_value_for_return(value, ret_type),
                )?;
                native_return_pushed_to_stack(shared, thread);
            }
        }
        if crate::runtime::env_cache::resume_pc_dbg() && method_name == "enhance" {
            let f = &thread.frames[frame_idx];
            let pc = f.pc;
            let b = f.code.get(pc).copied().unwrap_or(0);
            tracing::error!(
                target: "cratonvm_vm::dbg_resume_pc",
                "[CRATONVM_DBG_RESUME_PC] after stackless native enhance: caller {}.{}\n\
                 caller_pc={} next_bytecode=0x{:02x}",
                f.class_name(),
                f.method_name(),
                pc,
                b
            );
        }
        return Ok(CachedCallResult::Handled);
    }

    // 2. Look up class — must already be loaded for stackless path.
    //    A dispatch override (the receiver's own runtime class_id, supplied only
    //    when it diverges from the name-resolved copy under loader isolation)
    //    takes precedence so the ENHANCED per-loader copy's methods dispatch
    //    instead of the un-enhanced global same-named class.
    if crate::runtime::env_cache::dbg_loader_trace()
        && class_name.contains("RootReference")
        && method_name == "<init>"
    {
        let recv_cid = match args.first() {
            Some(Value::Object(Some(recv))) => Some(shared.mem.heap.class_id_of(*recv)),
            _ => None,
        };
        let name_resolved = shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id(class_name);
        eprintln!(
            "[TSI-CTOR-TRACE] class_name={class_name} descriptor={descriptor} is_special={is_special} dispatch_class_override={dispatch_class_override:?} name_resolved={name_resolved:?} receiver_actual_cid={recv_cid:?}"
        );
    }
    // Steps 2-5 share one `class_manager` guard; the owner lookup, the stub
    // check and the method lookup used to take three back to back.
    let cm = shared.classes.class_manager.read();
    let class_id = match dispatch_class_override.or_else(|| cm.get_loaded_class_id(class_name)) {
        Some(id) => id,
        None => return Ok(CachedCallResult::CacheMiss),
    };

    // 3. Synthetic stubs need the recursive path
    if cm
        .class_store
        .get(class_id)
        .is_some_and(|class| class.origin.is_compatibility_stub())
    {
        return Ok(CachedCallResult::CacheMiss);
    }

    // 4. Find method in class hierarchy.
    //
    // A virtual/interface dispatch on the receiver's own class SELECTS per
    // JVMS §5.4.6 (`runtime::resolve::selection`): a private or static
    // subclass method and a package-private non-override are not overrides,
    // an abstract selected method is an `AbstractMethodError`, and two
    // concrete maximally-specific defaults are an
    // `IncompatibleClassChangeError`. Every other shape — invokestatic,
    // invokespecial, a loader-split name that resolved to a different copy,
    // a hierarchy with a compatibility class in it — keeps the lenient walk.
    let strict = match selection {
        Some((receiver_id, resolved)) if receiver_id == class_id => Some((
            resolved,
            crate::runtime::resolve::selection::select(
                &cm.class_store,
                receiver_id,
                resolved,
                method_name,
                descriptor,
            ),
        )),
        _ => None,
    };
    let found = match strict {
        Some((_, crate::runtime::resolve::selection::Selection::Selected(declaring))) => cm
            .class_store
            .get(declaring)
            .and_then(|class| class.find_method(method_name, descriptor))
            .map(|m| (m, declaring)),
        Some((resolved, crate::runtime::resolve::selection::Selection::AbstractMethod(sel))) => {
            let message = crate::runtime::resolve::selection::abstract_method_message(
                &cm.class_store,
                class_id,
                resolved,
                sel,
                method_name,
                descriptor,
            );
            drop(cm);
            return match crate::runtime::exceptions::create_exception_object(
                shared,
                thread,
                "java/lang/AbstractMethodError",
                Some(&message),
            ) {
                Ok(exc) => Err(MethodCallFailed::ExceptionThrown(exc)),
                Err(e) => Err(e),
            };
        }
        Some((_, crate::runtime::resolve::selection::Selection::NotPublic(selected))) => {
            // `invokeinterface` selected a package-private or protected
            // method: HotSpot's `IllegalAccessError` (interpreter round i1
            // wave 30).
            let message = crate::runtime::resolve::selection::not_public_message(
                &cm.class_store,
                selected,
                method_name,
                descriptor,
            );
            drop(cm);
            return Err(crate::error::LinkageError::IllegalAccessError { message }.into());
        }
        Some((_, crate::runtime::resolve::selection::Selection::ConflictingDefaults(a, b))) => {
            let message = crate::runtime::resolve::selection::conflicting_defaults_message(
                &cm.class_store,
                class_id,
                a,
                b,
                method_name,
                descriptor,
            );
            drop(cm);
            return Err(
                crate::error::LinkageError::IncompatibleClassChangeError { message }.into(),
            );
        }
        Some((_, crate::runtime::resolve::selection::Selection::Lenient)) | None => {
            crate::classloading::find_method_recursive(
                class_id,
                method_name,
                descriptor,
                &cm.class_store,
            )
        }
    };
    let (method, declaring_id) = match found {
        Some(r) => r,
        None => {
            drop(cm);
            return Ok(CachedCallResult::CacheMiss);
        }
    };

    let is_native = method.is_native();
    let is_synchronized = method.is_synchronized();
    let is_static = method.is_static();

    if is_native {
        // Native method — look up in registry by declaring class.
        //
        // THE canonical §7 site. Of every native-dispatch point in the
        // interpreter this is the only one holding both a resolved `&Class`
        // and the resolved `&Method` at the same time, so it is the one that
        // calls `resolve_dispatch` itself rather than the name-only wave-1
        // adapter. The class-manager read guard is deliberately still held: it
        // is what makes the `&Class` borrow available, and dropping it first is
        // exactly why the old code had to allocate `declaring_name`.
        let class = match cm.get_class(declaring_id) {
            Some(class) => class,
            None => {
                drop(cm);
                return Ok(CachedCallResult::CacheMiss);
            }
        };
        let registry = &shared.natives.native_methods;
        // `resolve_id` is the same single triple hash `find` paid, and its slot
        // handle turns the §4 census increment into one relaxed add. The
        // documented identity `resolve_id(..).and_then(callback_of) == find(..)`
        // (descriptor-quirk fallback included) is what keeps this byte-for-byte.
        let native_id = registry.resolve_id(&class.name, method_name, descriptor);
        let native = native_id.and_then(|id| {
            registry.callback_of(id).map(|cb| {
                (
                    cb,
                    registry
                        .kind_of_id(id)
                        .unwrap_or(cratonvm_native_api::NativeKind::Bridge),
                )
            })
        });
        let decision =
            crate::vm::resolve_dispatch(crate::vm::dispatch_policy(shared), class, method, native);
        // A `SyntheticStub` refusal is a real policy stop (§1.3) and must not
        // fall through to some other implementation.
        //
        // DEVIATION, forced by preserving behaviour: `Reject(MissingNative)` is
        // NOT surfaced as an error in either mode. An `ACC_NATIVE` method with
        // no entry in the Rust registry is the ordinary case for a JNI-bound
        // native — `RegisterNatives` pointers and dlsym-resolved symbols are
        // resolved further down the recursive `invoke_shared` path, which this
        // `CacheMiss` is the door to. Erroring here would refuse every
        // legitimate JNI bridge, which §1.5 explicitly permits. The structured
        // `MissingNative` belongs at the END of that chain (wave 2), not at the
        // first of its three lookups.
        let mut refusal: Option<cratonvm_types::error::JdkOnlyViolation> = None;
        let callback = match decision {
            crate::vm::DispatchDecision::Reject(
                violation @ cratonvm_types::error::JdkOnlyViolation::SyntheticNativeInvocation {
                    ..
                },
            ) => {
                refusal = Some(violation);
                None
            }
            crate::vm::DispatchDecision::Reject(_) => None,
            other => other.native_callback(),
        };
        drop(cm);
        if let Some(violation) = refusal {
            return Err(MethodCallFailed::InternalError(VmError::JdkOnly(violation)));
        }
        if let Some(callback) = callback {
            // §4 census: relaxed increment on the handle already resolved above.
            if let Some(id) = native_id {
                shared.natives.native_methods.record_invocation(id);
            }
            // JVMS §2.11.10: a synchronized native runs holding its monitor
            // (per mode, see `native_sync_enabled`).
            let sync = resolved_native_sync(
                shared,
                is_synchronized,
                is_static,
                declaring_id,
                class_name,
                method_name,
                descriptor,
            );
            let result = safe_native_call_synchronized(shared, thread, callback, args, sync)?;
            if let Some(value) = result.filter(|_| ret_type != b'V') {
                // T18.K4 — tag-exact push for J/D native-bytecode method return values.
                push_invoke_return_value(
                    &mut thread.frames[frame_idx].stack,
                    coerce_value_for_return(value, ret_type),
                )?;
                native_return_pushed_to_stack(shared, thread);
            }
            return Ok(CachedCallResult::Handled);
        }
        // JNI or other exotic native — fall back
        return Ok(CachedCallResult::CacheMiss);
    }

    // 5. Bytecode method — the four facts the frame needs, not a clone of the
    // whole `CodeAttribute` (its nested attribute `Vec`s included). `code` is a
    // `ByteView`, so this is a refcount bump; it is padded after the guard is
    // dropped, through the per-method memo (see step 10).
    let (code_view, exception_table, max_stack, max_locals) = match method.code() {
        Some(c) => (
            c.code.clone(),
            Arc::<[_]>::from(c.exception_table.as_slice()),
            c.max_stack,
            c.max_locals,
        ),
        None => {
            drop(cm);
            return Ok(CachedCallResult::CacheMiss);
        }
    };

    let class = match cm.get_class(declaring_id) {
        Some(c) => c,
        None => {
            drop(cm);
            return Ok(CachedCallResult::CacheMiss);
        }
    };
    let source_file: Option<Arc<str>> = class.source_file.as_deref().map(Arc::from);
    let class_name_arc: Arc<str> = Arc::from(&*class.name);
    let declaring_is_interface = class.is_interface();
    drop(cm);

    // 6. Check for native override on bytecode method (same as invoke_on_class_shared).
    //
    // EXCEPTION — interface DEFAULT methods (declaring class is an interface,
    // instance method with code): natives registered on interface names
    // (`java/util/Collection`, `java/util/List`, …) are bridges for synthetic
    // receivers with no real class hierarchy — and synthetic receivers never
    // reach this step (synthetic stubs bail at step 3; abstract methods at
    // step 5). A real-bytecode receiver that resolved a default method must
    // run that bytecode, matching `populate_virtual_invoke_cache` which caches
    // `VirtualBytecode` for this exact site. Without this guard the first,
    // uncached call at each site dispatched the interface bridge while every
    // later (cached) call ran the bytecode — e.g. `Collection.stream()` on a
    // custom `AbstractList` (Hibernate's `JoinedList`) materialised an EMPTY
    // stream exactly once per call site (OrderProbe: count#1=0, count#2=3),
    // collapsing `AbstractEntityPersister`'s property closure to length 0.
    // Deliberate interface-default overrides (e.g. `Iterator.remove`) belong
    // in `force_native_over_real_jdk_bytecode`, which fires before this path.
    // Static interface methods keep the native check (mirrors invokestatic
    // promotion in `populate_invoke_cache`, which keys on the CP class).
    // JVMTI redefine guard: a class redefined in place by an agent runs its
    // woven bytecode (so the mock advice fires) rather than the native shadow.
    // Reflection-metadata natives stay authoritative (see
    // `redefine_immune_reflection_native`) — a Mockito inline mock of
    // `java.lang.reflect.Method` must not disable annotation reflection.
    let force_interface_default_native = declaring_is_interface
        && !is_static
        && should_force_registered_native_over_bytecode_for_receiver(
            shared,
            &class_name_arc,
            method_name,
            descriptor,
            receiver_for_immunity,
        );
    // This is the gate that actually decides for `java/util/zip/ZipFile.close`
    // and `.getName` on a Mockito inline mock. Measured 2026-09-10: with the
    // other five converted and this one left receiver-blind,
    // `CRATONVM_DBG_ZIPIMMUNE=off` intercepted and stubbed the mock perfectly
    // while the receiver-aware waiver recorded zero invocations — the
    // `[zipimmune]` trace showed the `java/util/zip/ZipFile` consultation
    // arriving with no waiver line beside it, i.e. from here.
    if (!(declaring_is_interface && !is_static) || force_interface_default_native)
        && (!native_shadow_suppressed_by_redefine(shared, &class_name_arc)
            || redefine_immune_forced_native_for_receiver(
                shared,
                &class_name_arc,
                method_name,
                descriptor,
                receiver_for_immunity,
            ))
    {
        // `find` -> `resolve_id` + `callback_of`: one hash either way
        // (`resolve_id(..).and_then(callback_of)` is documented to equal
        // `find(..)`), but the slot handle also yields the `NativeKind` §7
        // needs and makes the §4 census increment a relaxed add.
        let registry = &shared.natives.native_methods;
        let native_id = registry.resolve_id(&class_name_arc, method_name, descriptor);
        if let Some((id, callback)) =
            native_id.and_then(|id| registry.callback_of(id).map(|cb| (id, cb)))
        {
            // (The `ThreadPoolExecutor.execute(Runnable)` receiver-shape check
            // that used to sit here — a SEPARATE, independent double-check
            // running even after real bytecode was resolved at step 4/5 — was
            // deleted 2026-08-06. The `synthetic_stub_should_yield_to_real_bytecode`
            // term below now answers it for every receiver: `native_es_execute`
            // is a `SyntheticStub` and `ThreadPoolExecutor` is allow-listed.)
            //
            // This guard IS this site's compatibility verdict, so it is handed
            // to §7 as `compat_native_wins` verbatim — same predicate, same
            // short-circuit.
            //
            // `synthetic_stub_should_yield_to_real_bytecode` deliberately keeps
            // its own `kind_of` lookup rather than reusing `kind_of_id` above:
            // the two disagree on the descriptor-quirk cold path (`kind_of`
            // misses and reports "not a stub"), and reusing the slot's true
            // kind would silently change which natives yield.
            let compat_native_wins = !synthetic_stub_should_yield_to_real_bytecode(
                shared,
                &class_name_arc,
                method_name,
                descriptor,
            );
            let kind = registry
                .kind_of_id(id)
                .unwrap_or(cratonvm_native_api::NativeKind::Bridge);
            match crate::vm::resolve_native_dispatch_wave1(
                crate::vm::DispatchDoor::StacklessForce,
                crate::vm::dispatch_policy(shared),
                &class_name_arc,
                method_name,
                descriptor,
                Some((callback, kind)),
                compat_native_wins,
                // Step 5 already produced this method's `Code` attribute, so
                // §7 step 3 has a definite answer here: there IS bytecode, and
                // under `JdkOnly` a plain `Bridge` must not shadow it.
                true,
            ) {
                Some(crate::vm::DispatchDecision::Reject(violation)) => {
                    return Err(MethodCallFailed::InternalError(VmError::JdkOnly(violation)));
                }
                Some(decision) => {
                    if let Some(callback) = decision.native_callback() {
                        // §4 census: relaxed add on the handle already in hand.
                        registry.record_invocation(id);
                        let sync = resolved_native_sync(
                            shared,
                            is_synchronized,
                            is_static,
                            declaring_id,
                            class_name,
                            method_name,
                            descriptor,
                        );
                        let result =
                            safe_native_call_synchronized(shared, thread, callback, args, sync)?;
                        if let Some(value) = result.filter(|_| ret_type != b'V') {
                            // T18.K4 — tag-exact push for J/D native-override (on bytecode method) return values.
                            push_invoke_return_value(
                                &mut thread.frames[frame_idx].stack,
                                coerce_value_for_return(value, ret_type),
                            )?;
                            native_return_pushed_to_stack(shared, thread);
                        }
                        return Ok(CachedCallResult::Handled);
                    }
                }
                // Compatible: one of the two guards vetoed the native, exactly
                // as before. JdkOnly: that, or §7 step 3 preferring the real
                // bytecode this method already has.
                None => {}
            }
        }
    }

    // 7. Handle synchronized: acquire monitor before pushing frame. The guard
    // pins it until the frame takes it (`transfer_to_frame` below) and
    // releases it on every exit in between (the SOE bail, an unwind).
    let mut synchronized_args: Option<Vec<Value>> = None;
    let mut monitor_guard = if is_synchronized {
        let sync_args = synchronized_args.get_or_insert_with(|| args.to_vec());
        let obj = if is_static {
            // JVMS §2.11.10: a `static synchronized` method's monitor is the
            // `Class` object — the SAME object `ldc class`, `synchronized(X.class)`,
            // and `X.class.wait()/notify()` use. Locking a synthetic per-class
            // lock here desyncs from those, so e.g. a static-sync method calling
            // `X.class.notifyAll()` would throw IllegalMonitorStateException.
            //
            // Fetching the mirror allocates the first time. The arguments are
            // pinned across it and read back, so this site does not depend on
            // that allocation never reaching a collection: `sync_args` is a
            // copy the collector does not scan, and the `invokestatic`
            // caller's own `Vec` is not pinned either — a moving collection
            // there would leave the callee's locals, and the pins
            // `monitor_enter_synchronized_method` takes next, holding
            // from-space addresses.
            // gen r5w1/oom5: on a full heap the first use collects and throws
            // `OutOfMemoryError`; the pins drop on that return too.
            let pins = InvokeArgsRootGuard::new(thread, sync_args);
            let mirror = super::constants::class_mirror_or_oom(
                shared,
                thread,
                declaring_id,
                "static-synchronized-monitor",
            )?;
            pins.refresh(sync_args);
            drop(pins);
            mirror
        } else {
            match sync_args.first() {
                Some(Value::Object(Some(obj_ref))) => *obj_ref,
                _ => {
                    return Err(MethodCallFailed::InternalError(VmError::Internal {
                        message: "synchronized instance method called with null or missing this"
                            .to_string(),
                    }));
                }
            }
        };
        Some(crate::vm::vm_exec::SynchronizedMethodGuard::enter(
            shared, thread, obj, sync_args,
        ))
    } else {
        None
    };
    let args = synchronized_args.as_deref().unwrap_or(args);

    // 8. (Deleted 2026-09-23: self-recursive tail-call elimination.) HotSpot
    // eliminates no frame at any tier, so the in-place `reset_for_tail_call`
    // rewrite that stood here made infinite tail recursion loop instead of
    // throwing `StackOverflowError`, dropped frames from stack traces and
    // `StackWalker`, and — because only this cache-miss path could reach it —
    // did so for the first call of a site only. Every call now pushes a frame
    // and the `max_stack_depth` check below raises SOE, as on HotSpot. See
    // docs/internal/fixed-bugs/interpreter-L4-self-recursive-tail-call-elimination-diverges-from-hotspot-FIXED-20260923.md.

    // 9. Stack overflow check
    if thread.frames.at_frame_limit(0, shared.config.max_stack_depth) {
        dump_stack_on_soe(thread);
        // `monitor_guard` releases the monitor (and retracts the JMX publish)
        // as this bail drops it.
        drop(monitor_guard);
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::StackOverflowError,
        )));
    }

    // 10. Push bytecode frame
    // T10.7 — replenish the per-thread pool from the shared pool if empty.
    thread.refill_pools_from_shared(
        &shared.mem.operand_stack_pool,
        &shared.mem.tag_pool,
        // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
        max_locals as usize,
        // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
        (max_stack as usize).max(16) + 8,
    );
    // The per-method padded-code memo, not a fresh pad per call: a fresh
    // `Arc` per call cost an allocation and a copy of the body, and defeated
    // every cache keyed on the code pointer (quickening, local liveness) for
    // the sites that take this path on every call.
    let code = crate::runtime::frame::padded_bytecode_for_method(
        declaring_id,
        method_name,
        descriptor,
        &code_view,
    );
    let mut frame = Frame::new_pooled(
        declaring_id,
        class_name_arc,
        Arc::from(method_name),
        Arc::from(descriptor),
        source_file,
        code,
        exception_table,
        max_stack,
        max_locals,
        args,
        &mut thread.locals_pool,
        &mut thread.stacks_pool,
    );
    frame.monitor_on_exit = monitor_guard.as_mut().and_then(|g| g.transfer_to_frame());
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[FRAME_PUSH/stackless] depth={} {}.{}{}",
            thread.frames.len(),
            frame.class_name(),
            frame.method_name(),
            frame.method_descriptor()
        );
    }
    push_frame_and_fire_entry(shared.vm_identity, thread, frame);

    Ok(CachedCallResult::FramePushed)
}

// ---------------------------------------------------------------------------
// invokevirtual / invokeinterface
// ---------------------------------------------------------------------------
//
// The three cached dispatch tiers and the native-shadow consult every one
// of them has to pass: `interpreter/dispatch_virtual.rs`.

/// Resolve the metadata for a constant-pool method reference.
///
/// Besides the symbolic names and parameter count, this stores the exact
/// native callback/category for the symbolic owner. The registry hash is paid
/// once per resolved CP entry, not once per call site that consumes it.
///
/// **This is the resolution core, not the public entry point.** New callers
/// outside `crate::runtime::interpreter` go through
/// [`crate::runtime::resolve::MemberResolver::method_ref`], which tags the
/// answer with the VM that produced it and converts failures into the
/// structured [`crate::runtime::resolve::ResolveError`]. It is `pub(crate)`
/// only so that `MemberResolver` can delegate here; `runtime::resolve::guard`
/// fails the build if anything else names it.
pub(crate) fn resolve_method_metadata(
    shared: &SharedVm,
    current_class_id: ClassId,
    cp_index: u16,
) -> Result<ResolvedMethod, MethodCallFailed> {
    hotpath_counts::bump(&hotpath_counts::RESOLVE_METHOD_REF_CALLS);
    // Check cache first. Cloning is Arc bumps plus two copied function-pointer
    // metadata fields; there is no string allocation or registry hash.
    if let Some(cached) = shared
        .classes
        .resolution_cache
        .read()
        .get_method(current_class_id, cp_index)
    {
        return Ok(cached.clone());
    }

    // read_recursive() instead of read() — resolve_method_ref can be called
    // from ctx.invoke_virtual within a native, which may itself be dispatched
    // by an interpreter frame that already holds class_manager.read() on this
    // thread. parking_lot's write-preferring policy blocks new read() calls
    // when a writer is queued, so a recursive plain read() → deadlock;
    // read_recursive() succeeds immediately for an existing read-holder.
    //
    // The fill below lands after `cm` is dropped; a redefinition of this
    // class in between must not leave the old pool's member cached
    // (`ResolutionCache::fill_snapshot`, interpreter round i1 wave 22).
    let fill_as_of = crate::classloading::resolution::ResolutionCache::fill_snapshot();
    let cm = shared.classes.class_manager.read_recursive();
    let class = cm
        .get_class(current_class_id)
        .ok_or_else(|| VmError::Internal {
            message: "current class not found".to_string(),
        })?;

    let (class_idx, nat_idx) = match class.constant_pool.get(cp_index) {
        Some(ConstantPoolEntry::MethodReference {
            class_index,
            name_and_type_index,
        })
        | Some(ConstantPoolEntry::InterfaceMethodReference {
            class_index,
            name_and_type_index,
        }) => (*class_index, *name_and_type_index),
        _ => {
            return Err(VmError::Internal {
                message: format!("invalid method ref at cp#{cp_index}"),
            }
            .into());
        }
    };

    let class_name: Arc<str> = Arc::from(
        class
            .constant_pool
            .get_class_name(class_idx)
            .ok_or_else(|| VmError::Internal {
                message: format!("invalid class ref at cp#{class_idx}"),
            })?,
    );
    let (method_name_str, method_descriptor_str) = class
        .constant_pool
        .get_name_and_type(nat_idx)
        .ok_or_else(|| VmError::Internal {
        message: format!("invalid name_and_type at cp#{nat_idx}"),
    })?;

    let method_name: Arc<str> = Arc::from(method_name_str);
    let method_descriptor: Arc<str> = Arc::from(method_descriptor_str);
    let num_params = count_method_params(&method_descriptor);

    // Module access check (JPMS §5.4.4): verify accessor can reach the target
    // class's module.
    //
    // C2 P0 — routed through `runtime::resolve::MemberResolver`, the one
    // access-control entry point in `vm/src/runtime/`. `MemberFlags::OwnerOnly`
    // + `AccessPolicy::ModuleOnly` is the exact shape of what this call has
    // always been: at this point the hierarchy walk has not happened, so there
    // is no declaring method and no method flags to check — only the owner
    // class the constant pool names. That is also why the member half of JVMS
    // §5.4.4 is not enforced on this path; see
    // `classloading::access_control`'s module docs. Unchanged behaviour,
    // named policy.
    //
    // The owner is looked up in the CALLER's loader namespace first (the
    // loader-blind global name table can hand back another loader's copy),
    // and when it is not loaded at all the check cannot run — so the answer
    // is then NOT cached: a cached entry would serve every later execution of
    // the site without ever re-running the check, making the verdict depend
    // on whether unrelated code happened to load the owner first. The next
    // resolution (the owner is loaded by then — `invokestatic` loads it, and
    // `execute_invokestatic` re-checks after loading) caches normally.
    let owner_id = if class_name.starts_with('[') {
        None
    } else {
        cm.find_class_by_name_for_class(&class_name, current_class_id)
            .or_else(|| cm.get_loaded_class_id(&class_name))
    };
    // The owner the wave-26 checks below judge: the class the name denotes in
    // the CALLER's namespace (itself, a direct supertype, then the loader's
    // own), never the loader-blind fallback's answer (lane L5b).
    let owner_in_namespace = if class_name.starts_with('[') {
        None
    } else {
        owner_in_referencing_namespace(shared, &cm, current_class_id, &class_name)
    };
    if let Some(target_id) = owner_id {
        let resolver = crate::runtime::resolve::MemberResolver::new(shared);
        let _grant = resolver.check_member_access(
            &cm,
            resolver.scope(current_class_id),
            resolver.scope(target_id),
            crate::runtime::resolve::MemberFlags::OwnerOnly,
            None,
            crate::runtime::resolve::AccessPolicy::ModuleOnly,
        )?;
    }
    // An array owner (`[I.clone()`) has no module of its own to check.
    //
    // Under `--jdk-only`, an owner that only the loader-blind fallback knows is
    // not a resolution yet (interpreter round i1 wave 27, lane L5): the
    // receiver-call slow path resolves it through the caller's loader first
    // (`resolve_owner_unknown_to_caller`), and a cached entry here would let
    // `execute_invokevirtual_vtable_fast` dispatch the next execution on its
    // receiver, past a JVMS §5.4.3 record that resolution left
    // (`L5W27ReceiverOwnerUnresolvable`, `take#1`). Once the owner is known in
    // the caller's namespace (its own definition, a recorded parent, the
    // initiating memo) the entry is cached as before. `--compatible` too since
    // wave 29 (census-decided, `i25-L5-invokevirtual-...`).
    let mut cacheable = class_name.starts_with('[') || owner_in_namespace.is_some();
    // A reference whose tag disagrees with its class's kind (wave 39,
    // `methodref_kind_mismatch`) is not cached: a cached entry would let the
    // receiver doors (`execute_invokevirtual_vtable_fast`) dispatch the next
    // execution past the `IncompatibleClassChangeError` the slow door raises.
    if cacheable
        && owner_in_namespace.is_some_and(|owner| {
            methodref_tag_disagrees(shared, &cm, current_class_id, cp_index, owner).is_some()
        })
    {
        cacheable = false;
    }

    // JVMS §5.4.4 beyond JPMS (interpreter round i1 wave 26, lane L5): the
    // class the reference names, then the method it resolves to
    // (`field_access::member_owner_access_refusal` /
    // `method_member_access_refusal`), enforced under `--jdk-only` and counted
    // under `--compatible`. On this resolution miss only — a cached answer
    // passed it when filled — and only against the owner found in the
    // CALLER's namespace: the loader-blind fallback above can answer another
    // loader's copy, whose runtime package is not the reference's. An owner
    // not loaded yet is checked by the resolution that follows its loading
    // (`execute_invokestatic` resolves again; this answer is not cached).
    let refusal_against = |target_id: ClassId| {
        member_owner_access_refusal(shared, &cm, current_class_id, &class_name, target_id)
            .map(|message| ("method owner class", message))
            .or_else(|| {
                method_member_access_refusal(
                    shared,
                    &cm,
                    current_class_id,
                    target_id,
                    &method_name,
                    &method_descriptor,
                )
                .map(|message| ("method", message))
            })
    };
    let refusal = owner_in_namespace.and_then(|id| refusal_against(id));
    // An owner only the loader-blind fallback knows (interpreter round i1 wave
    // 27, lane L5): the checks run against it too, but a refusal is neither
    // enforced (it may be another loader's copy, whose runtime package is not
    // the reference's: the wave-26 false denial) nor CACHED — a cached answer
    // never runs the checks again, so an access HotSpot refuses would be
    // admitted for good once cached. Counted (`member_access_unverified`). A
    // later resolution that finds the owner in the caller's namespace decides.
    let unverified = if owner_in_namespace.is_none() && !class_name.starts_with('[') {
        owner_id.and_then(|id| refusal_against(id))
    } else {
        None
    };
    // JVMS §5.3.4 against the class that DECLARES the resolved method
    // (interpreter round i1 wave 31): a `LinkageError` under `--jdk-only`,
    // counted under `--compatible`. On this resolution miss only.
    let loader_constraint = owner_in_namespace
        .and_then(|owner| {
            crate::runtime::resolve::selection::resolve_declaring(
                &cm.class_store,
                owner,
                &method_name,
                &method_descriptor,
            )
        })
        .and_then(|declaring| {
            loader_constraint_violation(
                shared,
                &cm,
                "method",
                current_class_id,
                declaring,
                &method_name,
                &method_descriptor,
            )
        });

    // JVMS §5.4.3.4 step 3 (interpreter round i1 wave 32): an interface
    // method reference reaches `java.lang.Object` only for a public instance
    // method, so `invokeinterface I.clone()` is a `NoSuchMethodError`, as in
    // HotSpot, in both modes (a genuine bug: `Object.clone` was dispatched).
    // Judged against the owner in the caller's namespace; an owner not loaded
    // yet is judged by the resolution after its loading.
    let hidden_object_method = owner_in_namespace.is_some_and(|owner| {
        crate::runtime::resolve::selection::interface_ref_names_a_non_public_object_method(
            &cm.class_store,
            owner,
            &method_name,
            &method_descriptor,
        )
    });

    // Drop the read lock before acquiring write lock
    drop(cm);

    if hidden_object_method {
        return Err(crate::error::LinkageError::NoSuchMethodError {
            class_name: class_name.to_string(),
            method_name: method_name.to_string(),
            method_descriptor: method_descriptor.to_string(),
        }
        .into());
    }

    if let Some(message) = loader_constraint {
        loader_constraint_census(shared, message)?;
    }

    if let Some((what, message)) = refusal {
        if let Some(message) = member_access_census(shared, what, message) {
            return Err(crate::error::LinkageError::IllegalAccessError { message }.into());
        }
    }
    if let Some((what, message)) = unverified {
        member_access_unverified_census(shared, what, &message);
        cacheable = false;
    }

    let (native_target, native_kind) = shared
        .natives
        .native_methods
        .find_with_kind(&class_name, &method_name, &method_descriptor)
        .map(|(target, kind)| (Some(target), Some(kind)))
        .unwrap_or((None, None));
    let resolved = ResolvedMethod {
        declaring_class_id: current_class_id,
        class_name,
        method_name,
        method_descriptor,
        num_params: num_params as u16, // Widening: parameter count conversion
        native_target,
        native_kind,
    };
    if cacheable {
        shared.classes.resolution_cache.write().put_method_as_of(
            current_class_id,
            cp_index,
            resolved.clone(),
            fill_as_of,
        );
    }

    Ok(resolved)
}

/// The class a receiver call's `Methodref` owner `name` denotes in the
/// caller's namespace, known without loading: the question
/// `execute_invoke_kind`'s two owner arms (the null-receiver one and the
/// `cp_owner_unknown` one) ask before deciding to resolve the owner through
/// the caller's loader.
///
/// `--jdk-only`: [`owner_in_referencing_namespace`], the owner
/// `resolve_method_metadata` judges access against, so the two agree: an
/// owner that only `find_class_by_name_for_class`'s guess knows for a
/// child-first user loader is resolved through that loader first, and the
/// resolution that follows is cached (interpreter round i1 wave 29, lane L5;
/// `L5/L5W29ChildFirstAgentLoader`). `--compatible`: the plain lookup, as
/// before. Slow path only.
fn owner_known_to_caller(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    current_class_id: ClassId,
    name: &str,
) -> Option<ClassId> {
    owner_in_referencing_namespace(shared, cm, current_class_id, name)
}

/// The receiver-call half of JVMS §5.4.3.3's "resolve the owner first"
/// (`execute_invoke_kind`, interpreter round i1 wave 27, lane L5): the
/// `Methodref` names an owner that the caller's namespace does not know
/// (`find_class_by_name_for_class` missed, and neither the loader's own
/// definition nor its initiating memo has it), and the receiver is about to
/// decide the target anyway.
///
/// * `--jdk-only`: the owner is resolved through the caller's loader
///   (`resolve_class_loader_aware`) while the arguments are still on the
///   operand stack; a failure is converted and RECORDED against the class
///   entry, so this execution and every later one through the entry throw it,
///   as HotSpot does before looking at the receiver. After a success the
///   JVMS §5.4.4 checks run (`resolve_method_ref` again, now with the owner
///   known), as the null-receiver arm does.
/// * `--compatible`: unchanged behaviour; the occurrence is counted
///   (`ClassRealm::receiver_owner_unresolved`, in the `CRATONVM_DBG=access`
///   exit total, and traced there): the census the `--compatible` stage needs.
///
/// VM-service and VM-minted stand-in receivers (no class file in any JDK
/// image; `dispatch_static.rs` exempts the same names from owner resolution)
/// are left to their bridges.
#[cold]
#[inline(never)]
fn resolve_owner_unknown_to_caller(
    shared: &SharedVm,
    thread: &mut JvmThread,
    current_class_id: ClassId,
    cp_index: u16,
    method_class_name: &str,
    fill_as_of: u64,
) -> Result<(), MethodCallFailed> {
    if cratonvm_native_api::no_image_receiver::is_reviewed_vm_service_receiver(method_class_name)
        || cratonvm_native_api::no_image_receiver::receiver_declared_by_no_supported_image(
            method_class_name,
        )
    {
        return Ok(());
    }
    // A hidden class naming itself (stored as `<name>/0x<hex>`, never in a
    // name table, so the lookup above cannot find it): the caller IS the owner.
    let names_itself = shared
        .classes
        .class_manager
        .read()
        .get_class(current_class_id)
        .is_some_and(|c| is_self_class_reference(&c.name, c.is_hidden(), method_class_name));
    // Known to the loader through its own definition or its memoised
    // `loadClass` answer: resolved already, nothing to count.
    if names_itself || lookup_loader_initiated(shared, current_class_id, method_class_name).is_some()
    {
        return Ok(());
    }
    let n = shared
        .classes
        .receiver_owner_unresolved
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .saturating_add(1);
    // Every mode since wave 29 (census-decided: 45 occurrences over the
    // `--compatible` suites, the Spring Boot fat jar and TestTomcat, every
    // one an owner that resolves).
    let enforced = true;
    if cratonvm_types::flags().loader.dbg_access {
        let caller = shared
            .classes
            .class_manager
            .read()
            .get_class(current_class_id)
            .map(|c| c.name.to_string())
            .unwrap_or_default();
        eprintln!(
            "[ACCESS-DBG] OWNER {} #{n} receiver call from {caller} names {method_class_name}, unknown to its loader",
            if enforced {
                "RESOLVED"
            } else {
                "UNRESOLVED (compatible, census)"
            }
        );
    }
    if !enforced {
        return Ok(());
    }
    let owner = resolve_class_loader_aware(shared, thread, current_class_id, method_class_name)
        .map_err(|e| {
            crate::runtime::exceptions::convert_class_not_found_for(
                shared,
                thread,
                Some(current_class_id),
                method_class_name,
                e,
            )
        })
        .map_err(|e| {
            record_member_owner_failure_as_of(shared, current_class_id, cp_index, e, fill_as_of)
        })?;
    // JVMS §5.4.3: a failure another thread recorded for the entry meanwhile
    // is the outcome (`--jdk-only`; wave 39, lane L5).
    if let Some(recorded) =
        recorded_member_owner_failure_after_success(shared, thread, current_class_id, cp_index)
    {
        return Err(recorded);
    }
    // The loader was asked and refused, and CratonVM's global fallback
    // answered: that answer is this loader's resolution of the name, so it is
    // memoised as the loader's own `loadClass` answer would have been (a
    // `drive_defining_loader_load` success memoises itself). Without it the
    // owner stays unknown to the caller (`owner_known_to_caller`) and every
    // slow-path execution of the site would ask the loader again, uncached
    // (interpreter round i1 wave 29, lane L5). Not for a JDK-global name,
    // whose memo `drive_loader_for_global_name` owns.
    if !is_global_resolution_namespace(method_class_name)
        && lookup_loader_initiated(shared, current_class_id, method_class_name).is_none()
    {
        let loader = shared
            .classes
            .class_manager
            .read()
            .get_loader_id(current_class_id);
        if let Some(loader @ cratonvm_types::ClassLoaderId::UserDefined(_)) = loader {
            cache_loader_initiated(shared, loader, method_class_name, owner);
        }
    }
    resolve_method_ref(shared, current_class_id, cp_index)
        .map(|_| ())
        .map_err(|e| {
            settle_method_resolution_failure(
                shared,
                thread,
                current_class_id,
                cp_index,
                e,
                fill_as_of,
            )
        })
}

/// A [`resolve_method_ref`] failure of the site `(class_id, cp_index)` of a
/// caller that holds its `JvmThread`, settled as JVMS §5.4.3 has it (the
/// threadless core can neither build nor record a throwable), where the
/// JVMS §5.4.4 checks are enforced ([`member_access_enforced`]; `--compatible`
/// passes every failure through unchanged):
///
/// * an `IllegalAccessError` whose reference's CLASS entry already holds a
///   recorded failure rethrows the record — that entry failed first;
/// * an enforced owner-class refusal ([`member_owner_access_refusal`], asked
///   again here: this is the cold failure path) becomes the Java
///   `IllegalAccessError`, recorded against that class entry, so a `new`, an
///   `ldc` or another member reference through it rethrows it, as HotSpot's
///   `klass_at_impl` records it.
///
/// Everything else, including a refusal of the MEMBER (which HotSpot does not
/// record: member resolution is re-run and refuses the same way), passes
/// unchanged. `as_of` is the `ResolutionCache::fill_snapshot` taken before the
/// constant pool was read (interpreter round i1 wave 26, lane L5).
#[cold]
pub(super) fn settle_method_resolution_failure(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    cp_index: u16,
    failure: MethodCallFailed,
    as_of: u64,
) -> MethodCallFailed {
    let MethodCallFailed::InternalError(VmError::Linkage(
        crate::error::LinkageError::IllegalAccessError { message },
    )) = &failure
    else {
        return failure;
    };
    // `--compatible` enforces none of the new refusals; its pre-existing JPMS
    // `IllegalAccessError` keeps its old path unchanged.
    if !member_access_enforced(shared) {
        return failure;
    }
    if let Some(recorded) = recorded_member_owner_failure(shared, thread, class_id, cp_index) {
        return recorded;
    }
    let owner_refusal = {
        let cm = shared.classes.class_manager.read_recursive();
        let owner_name = cm.get_class(class_id).and_then(|class| {
            let class_index = match class.constant_pool.get(cp_index)? {
                ConstantPoolEntry::MethodReference { class_index, .. }
                | ConstantPoolEntry::InterfaceMethodReference { class_index, .. } => *class_index,
                _ => return None,
            };
            class
                .constant_pool
                .get_class_name(class_index)
                .map(str::to_string)
        });
        owner_name.and_then(|name| {
            let owner = owner_in_referencing_namespace(shared, &cm, class_id, &name)?;
            member_owner_access_refusal(shared, &cm, class_id, &name, owner)
        })
    };
    if owner_refusal.as_deref() != Some(message.as_str()) {
        return failure;
    }
    let error = match crate::runtime::exceptions::create_exception_object(
        shared,
        thread,
        "java/lang/IllegalAccessError",
        Some(message.as_str()),
    ) {
        Ok(obj) => MethodCallFailed::ExceptionThrown(obj),
        Err(e) => e,
    };
    record_member_owner_failure_as_of(shared, class_id, cp_index, error, as_of)
}

/// Compatibility view for the interpreter paths that only need symbolic
/// method data. The underlying cache entry still carries its native target.
/// `--jdk-only` (interpreter round i1 wave 37, lane L4; item 3 of
/// `interpreter-L4-varhandle-views-exact-behavior-and-access-mode-queries-FIXED-20261010`): the
/// `WrongMethodTypeException` an invoke-exact `VarHandle` raises when the
/// descriptor `call_site` the calling instruction names for `method_name` is
/// not exactly its access mode type
/// (`lang_invoke::varhandle_exact_call_site_refusal`), or `None`.
///
/// For the doors that hold the REAL call-site descriptor: `execute_invoke_kind`
/// (every interpreted `VarHandle` access) and the JIT's site-cached native
/// dispatch. Each asks only for a receiver whose `exact` field is set
/// ([`var_handle_receiver_is_exact`]), once this VM has minted an exact handle
/// ([`var_handle_exact_gate`]). `CRATONVM_DBG_MH_STACK=1` prints one
/// `[MH_STACK] exact VarHandle` line per judged access (the positive control).
#[cold]
#[inline(never)]
pub(crate) fn var_handle_exact_refusal(
    shared: &SharedVm,
    thread: &mut JvmThread,
    vh: ObjectRef,
    method_name: &str,
    call_site: &str,
) -> Option<MethodCallFailed> {
    let mut ctx = crate::vm::NativeContextImpl { shared, thread };
    let refusal = cratonvm_native_builtins::lang_invoke::varhandle_exact_call_site_refusal(
        &mut ctx,
        vh,
        method_name,
        call_site,
    );
    if crate::runtime::env_cache::dbg_mh_stack() {
        eprintln!(
            "[MH_STACK] exact VarHandle {method_name}{call_site}: {}",
            if refusal.is_some() {
                "refused"
            } else {
                "admitted"
            }
        );
    }
    refusal
}

/// The slot of `java.lang.invoke.VarHandle.exact` once this VM has minted an
/// invoke-exact `VarHandle` (`NativeRealm::var_handle_exact_slot`), else
/// `None`: the one relaxed load every door that judges exact handles pays
/// (interpreter round i1 wave 37, lane L4).
#[inline]
pub(crate) fn var_handle_exact_gate(shared: &SharedVm) -> Option<usize> {
    let slot = shared
        .natives
        .var_handle_exact_slot
        .load(std::sync::atomic::Ordering::Relaxed);
    (slot != u32::MAX).then_some(slot as usize)
}

/// Is the `VarHandle` receiver `vh`'s real `exact` field (at `exact_slot`,
/// from [`var_handle_exact_gate`]) set? Every `VarHandle` under `--jdk-only`
/// has the real layout (`vform`, `exact`, then a subclass's own fields); a
/// synthetic-layout one holds a `String` there and reads as not exact.
#[inline]
pub(crate) fn var_handle_receiver_is_exact(
    shared: &SharedVm,
    vh: ObjectRef,
    exact_slot: usize,
) -> bool {
    matches!(shared.mem.heap.get_field(vh, exact_slot), Value::Int(v) if v != 0)
}

/// `Some(owner is an interface)` when the method reference at `cp_index`
/// names `owner` with the other tag (a `Methodref` of an interface, an
/// `InterfaceMethodref` of a class), under `--jdk-only` and when both the
/// caller and the owner have real class bytes; else `None`. The shared half of
/// [`methodref_kind_mismatch`] and of `resolve_method_metadata`'s decision
/// not to cache such a reference.
fn methodref_tag_disagrees(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    current_class_id: ClassId,
    cp_index: u16,
    owner: ClassId,
) -> Option<bool> {
    if !shared.config.is_jdk_only() {
        return None;
    }
    let caller = cm.get_class(current_class_id)?;
    if !caller.origin.has_real_bytes() {
        return None;
    }
    let interface_tag = match caller.constant_pool.get(cp_index)? {
        ConstantPoolEntry::MethodReference { .. } => false,
        ConstantPoolEntry::InterfaceMethodReference { .. } => true,
        _ => return None,
    };
    let owner_class = cm.class_store.get(owner)?;
    if !owner_class.origin.has_real_bytes() {
        return None;
    }
    let owner_is_interface = owner_class.is_interface();
    (owner_is_interface != interface_tag).then_some(owner_is_interface)
}

/// The invoke instruction [`methodref_kind_mismatch`] judges for.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum MethodrefUse {
    Static,
    Special,
    Virtual,
    Interface,
}

/// JVMS 5.4.3.3 / 5.4.3.4 (interpreter round i1 wave 39, lane L4): resolving a
/// `CONSTANT_Methodref` whose class is an INTERFACE, or a
/// `CONSTANT_InterfaceMethodref` whose class is a CLASS, is an
/// `IncompatibleClassChangeError`. HotSpot's message depends on the
/// instruction (measured, probe `tools/probes/interp/L4/L4W39MethodrefKindMismatch.java`):
///
/// * `invokevirtual`, `Methodref` of an interface: `Found interface I, but
///   class was expected`;
/// * `invokeinterface`, `InterfaceMethodref` of a class: `Found class C, but
///   interface was expected`;
/// * otherwise (`invokestatic`, `invokespecial`), the tag check after the
///   method lookup: `Method 'void I.s()' must be InterfaceMethodref constant`
///   / `Method 'void C.cs()' must be Methodref constant`. Only when the
///   method resolves: a missing one is the lookup's error first.
///
/// `None` when the kinds agree, and whenever the caller or the owner has no
/// real class bytes (a class this VM spun, or a stub's flags, is not
/// evidence). `--jdk-only` only; `--compatible` keeps linking such a
/// reference (by design, unchanged). Slow path only: every door that
/// reaches it returns before any cache is filled.
/// HotSpot words an instance invoke of a STATIC method by the resolution
/// routine: an interface method reference (`resolve_interface_method`, used
/// by `invokeinterface` and by `invokespecial` of an `InterfaceMethodref`)
/// says `Expected instance not static method '<m>'`, a class method
/// reference `Expecting non-static method '<m>'` (the wording
/// `selection::static_flag_mismatch` returns). Measured on HotSpot 25,
/// `tools/probes/interp/L4/L4W43InterfaceMemberKinds.java` rows
/// `i-iface-static`, `i-special-static`, `c-iface-static` against
/// `k-virtual-static`, `k-special-static`. Interpreter round i1 wave 43,
/// lane L4.
fn interface_method_static_wording(
    cm: &crate::classloading::ClassManager,
    owner: ClassId,
    message: String,
) -> String {
    const CLASS_WORDING: &str = "Expecting non-static method ";
    let owner_is_interface = cm.class_store.get(owner).is_some_and(|c| c.is_interface());
    if owner_is_interface {
        if let Some(rest) = message.strip_prefix(CLASS_WORDING) {
            return format!("Expected instance not static method {rest}");
        }
    }
    message
}

pub(super) fn methodref_kind_mismatch(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    current_class_id: ClassId,
    cp_index: u16,
    owner: ClassId,
    use_: MethodrefUse,
    method_name: &str,
    method_descriptor: &str,
) -> Option<String> {
    if method_name == "<init>" || method_name == "<clinit>" {
        return None;
    }
    let owner_is_interface = methodref_tag_disagrees(shared, cm, current_class_id, cp_index, owner)?;
    let store = &cm.class_store;
    let owner_class = store.get(owner)?;
    use crate::runtime::resolve::selection as sel;
    let owner_name = sel::external_class_name(owner_class);
    match (owner_is_interface, use_) {
        (true, MethodrefUse::Virtual) => {
            return Some(format!("Found interface {owner_name}, but class was expected"));
        }
        (false, MethodrefUse::Interface) => {
            return Some(format!("Found class {owner_name}, but interface was expected"));
        }
        _ => {}
    }
    let declaring = sel::resolve_declaring(store, owner, method_name, method_descriptor)?;
    let holder = store.get(declaring)?.name.clone();
    Some(format!(
        "Method '{}' must be {} constant",
        sel::external_method_name(&holder, method_name, method_descriptor),
        if owner_is_interface {
            "InterfaceMethodref"
        } else {
            "Methodref"
        }
    ))
}

pub(super) fn resolve_method_ref(
    shared: &SharedVm,
    current_class_id: ClassId,
    cp_index: u16,
) -> Result<(Arc<str>, Arc<str>, Arc<str>, usize), MethodCallFailed> {
    let resolved = resolve_method_metadata(shared, current_class_id, cp_index)?;
    Ok((
        resolved.class_name,
        resolved.method_name,
        resolved.method_descriptor,
        resolved.num_params as usize,
    ))
}

/// JVMS §6.5 `invokespecial` — apply the super-call "selection" redirect
/// (see `classloading::invokespecial_selection_start`'s doc comment for the
/// full rule) to the class named by an `invokespecial` constant-pool
/// reference, so dispatch starts the method search at the CALLING class's
/// own direct superclass rather than blindly at the CP-referenced ancestor
/// whenever that redirect applies. `resolve_method_ref` (which produced
/// `method_class_name`) returns the bare CP text and has no notion of the
/// calling class, so this is a separate, deliberately narrow lookup.
///
/// A class between the caller and the CP-referenced ancestor may override
/// the method (a compiler-generated bridge, or an ordinary override) —
/// resolving from the CP-referenced class directly walks straight past it.
/// Shared by both the interpreter's own `invokespecial` dispatch (here) and
/// the JIT compiler's call-site resolution (`try_jit_compile_callee`'s
/// `invoke_resolver` closures in this file), so the two execution modes
/// never disagree on the target.
///
/// Returns `method_class_name` unchanged whenever the redirect does not
/// apply (constructors, interface references, non-superclass references, a
/// caller class file without `ACC_SUPER`, or any resolution miss) — always
/// safe to substitute directly for `method_class_name` at the `is_special`
/// call site.
///
/// Under `--jdk-only` the answer is JVMS §6.5's selection
/// (`selection::select_special`, interpreter round i1 wave 32): a static
/// declaration on the way is skipped, and an abstract selected method or
/// conflicting defaults are the `Err` (the exception class and HotSpot's
/// message), which the caller raises or, on a cache fill, declines to cache.
pub(super) fn invokespecial_owner_class_name(
    shared: &SharedVm,
    current_class_id: ClassId,
    cp_index: u16,
    method_class_name: &Arc<str>,
    method_name: &str,
    method_descriptor: &str,
) -> Result<Arc<str>, (&'static str, String)> {
    if method_name == "<init>" {
        return Ok(Arc::clone(method_class_name));
    }
    let cm = shared.classes.class_manager.read();
    let Some(class) = cm.get_class(current_class_id) else {
        return Ok(Arc::clone(method_class_name));
    };
    let is_interface_ref = matches!(
        class.constant_pool.get(cp_index),
        Some(ConstantPoolEntry::InterfaceMethodReference { .. })
    );
    let Some(cp_class_id) = cm.find_class_by_name_for_class(method_class_name, current_class_id)
    else {
        return Ok(Arc::clone(method_class_name));
    };
    let store = cm.class_store();
    if shared.config.is_jdk_only() {
        use crate::runtime::resolve::selection::{select_special, SpecialSelection};
        match select_special(
            store,
            current_class_id,
            cp_class_id,
            is_interface_ref,
            method_name,
            method_descriptor,
        ) {
            SpecialSelection::Unchanged => {}
            SpecialSelection::Owner(id) => {
                return Ok(store
                    .get(id)
                    .map(|c| Arc::clone(&c.name))
                    .unwrap_or_else(|| Arc::clone(method_class_name)));
            }
            SpecialSelection::AbstractMethod(message) => {
                return Err(("java/lang/AbstractMethodError", message));
            }
            SpecialSelection::IncompatibleClassChange(message) => {
                return Err(("java/lang/IncompatibleClassChangeError", message));
            }
        }
    }
    let start = crate::classloading::invokespecial_selection_start(
        current_class_id,
        cp_class_id,
        is_interface_ref,
        method_name,
        store,
    );
    if start == cp_class_id {
        return Ok(Arc::clone(method_class_name));
    }
    Ok(match store.get(start) {
        Some(c) => Arc::from(&*c.name),
        None => Arc::clone(method_class_name),
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Malformed descriptors are rejected, never sliced out of bounds. An
    /// unterminated class name ran the index one past the end and the slice
    /// panicked; a non-ASCII tag byte could split a UTF-8 sequence.
    #[test]
    fn split_method_descriptor_ref_rejects_malformed_input_without_panicking() {
        for bad in [
            "(Ljava/lang/String",
            "(L",
            "(IL",
            "([Ljava/lang/Object",
            "(\u{e9})V",
            "(I\u{e9}",
        ] {
            assert_eq!(
                split_method_descriptor_ref(bad),
                (Vec::new(), ""),
                "descriptor {bad:?}"
            );
        }
        // Well-formed input is unchanged.
        assert_eq!(
            split_method_descriptor_ref("(I[JLjava/lang/String;)V"),
            (vec!["I", "[J", "Ljava/lang/String;"], "V")
        );
        assert_eq!(split_method_descriptor_ref("()V"), (Vec::new(), "V"));
    }

    /// Round 11 wave 5 (lane rt): the return token follows the `)` that closes
    /// the parameter list. A class name may contain `)`, and the first-`)` scan
    /// answered `B;)Z` for the first descriptor below.
    #[test]
    fn descriptor_return_ref_is_not_fooled_by_a_paren_in_a_class_name() {
        assert_eq!(descriptor_return_ref("(LA)B;)Z"), "Z");
        assert_eq!(descriptor_return_ref("()LA)I;"), "LA)I;");
        assert_eq!(
            descriptor_return_ref("(I[J)Ljava/lang/String;"),
            "Ljava/lang/String;"
        );
        assert_eq!(descriptor_return_ref("()V"), "V");
        assert_eq!(descriptor_return_ref("(I"), "");
        assert_eq!(descriptor_return_ref(""), "");
    }

    // -----------------------------------------------------------------------
    // §4 census — `try_stackless_invoke`'s exotic arms declare themselves
    // uncounted
    // (docs/internal/jdk-only/G37-1-marking-the-bypasses-20260817.md)
    // -----------------------------------------------------------------------

    fn census_probe_native(
        _ctx: &mut dyn cratonvm_native_api::NativeContext,
        _args: &[Value],
    ) -> cratonvm_types::error::MethodCallResult {
        Ok(None)
    }

    /// The signature-polymorphic rows must stay on the **erased** descriptor.
    ///
    /// `MethodHandle.invoke*` and `DowncallHandle.invoke*` are registered under
    /// `([Ljava/lang/Object;)Ljava/lang/Object;` while a real call site carries
    /// its concrete signature — that mismatch is the whole reason those arms
    /// exist, and it is also the reason the census row that loses the call is
    /// the erased one. "Tidying" these rows to concrete descriptors would leave
    /// the table resolving nothing and the marks silently absent, which reads
    /// identically to a fixed instrument.
    ///
    /// Duplicate-free for the same reason the JIT-side list is: a duplicate
    /// would make the count assertions below pass over one triple twice.
    #[test]
    fn the_signature_polymorphic_rows_use_the_erased_descriptor() {
        const ERASED: &str = "([Ljava/lang/Object;)Ljava/lang/Object;";
        for (class, method, descriptor) in UNCOUNTED_STACKLESS_NATIVES {
            if matches!(
                class,
                "java/lang/invoke/MethodHandle" | "java/lang/foreign/DowncallHandle"
            ) && matches!(method, "invoke" | "invokeExact" | "invokeBasic")
            {
                assert_eq!(
                    descriptor, ERASED,
                    "{class}.{method} is dispatched through the erased bridge; a \
                     concrete descriptor here resolves nothing and marks nothing"
                );
            }
        }

        let mut seen: Vec<(&str, &str, &str)> = Vec::new();
        for row in UNCOUNTED_STACKLESS_NATIVES {
            assert!(
                !seen.contains(&row),
                "UNCOUNTED_STACKLESS_NATIVES lists {row:?} twice"
            );
            seen.push(row);
        }
    }

    /// Marking must turn exactly the exotic-arm rows into floors, leave a
    /// counted row exact, and leave the tallies alone.
    ///
    /// The counted control here is deliberately the shape this function's
    /// ordinary path takes: a native reached through `resolve_step1_native`,
    /// which holds the id and calls `record_invocation`. Those rows are exact
    /// and must keep saying so — the point of the bit is to separate them from
    /// the eleven arms that are not, not to blanket the census in doubt.
    #[test]
    fn marking_the_stackless_exotic_arms_turns_their_rows_into_floors() {
        let mut registry = cratonvm_native_api::NativeMethodRegistry::new();
        registry.with_category(cratonvm_native_api::NativeKind::Bridge, |r| {
            for (class, method, descriptor) in UNCOUNTED_STACKLESS_NATIVES {
                r.register(class, method, descriptor, census_probe_native);
            }
            r.register(
                "java/lang/System",
                "identityHashCode",
                "(Ljava/lang/Object;)I",
                census_probe_native,
            );
        });

        let counted = registry
            .resolve_id(
                "java/lang/System",
                "identityHashCode",
                "(Ljava/lang/Object;)I",
            )
            .expect("control registered");
        let method_invoke = registry
            .resolve_id(
                "java/lang/reflect/Method",
                "invoke",
                "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
            )
            .expect("registered");

        assert_eq!(registry.slots_with_incomplete_invocations(), 0);
        registry.record_invocation(counted);

        let marked = mark_stackless_exotic_natives_incomplete_in(&registry);
        assert_eq!(
            marked,
            UNCOUNTED_STACKLESS_NATIVES.len(),
            "every listed triple was registered above, so every one must resolve"
        );

        assert_eq!(
            registry.invocations_complete(method_invoke),
            Some(false),
            "reflective Method.invoke is dispatched from an arm that holds no \
             NativeMethodId, so its zero proves nothing"
        );
        assert_eq!(
            registry.invocations_complete(counted),
            Some(true),
            "the ordinary step-1 path counts, and must keep claiming to"
        );
        assert_eq!(
            registry.invocations_of_id(counted),
            Some(1),
            "marking other slots must not disturb a counted tally"
        );
        assert_eq!(
            registry.slots_with_incomplete_invocations(),
            UNCOUNTED_STACKLESS_NATIVES.len()
        );

        // Idempotent: the marker is called from five dispatch points and the
        // latch is an optimisation, not a correctness requirement.
        assert_eq!(
            mark_stackless_exotic_natives_incomplete_in(&registry),
            UNCOUNTED_STACKLESS_NATIVES.len()
        );
        assert_eq!(
            registry.slots_with_incomplete_invocations(),
            UNCOUNTED_STACKLESS_NATIVES.len()
        );
    }

    /// A registry without the Panama / `MethodHandle` bridges must be marked
    /// with nothing rather than panic. This runs on the interpreter's hottest
    /// function; an unregistered triple is an ordinary state, not an error.
    #[test]
    fn marking_an_empty_registry_marks_nothing_and_does_not_panic() {
        let registry = cratonvm_native_api::NativeMethodRegistry::new();
        assert_eq!(mark_stackless_exotic_natives_incomplete_in(&registry), 0);
        assert_eq!(registry.slots_with_incomplete_invocations(), 0);
    }
}

#[cfg(test)]
mod native_sync_facts_tests {
    use super::{ClassId, NativeSyncAnswer, NativeSyncEpochs, NativeSyncFacts};

    /// Each kind of word is checked against its own counter only (i11-L5,
    /// wave 12): a found answer survives a move of the definition-epoch sum,
    /// an unresolved one survives a move of the name-generation sum, and each
    /// goes stale when its own counter moves. `0` is never a hit.
    #[test]
    fn each_answer_kind_is_tagged_by_its_own_counter() {
        let epochs = NativeSyncEpochs {
            found: 7,
            unresolved: 40,
        };
        let declaring = ClassId::new(123);
        let cases = [
            (NativeSyncAnswer::Found(None), None),
            (
                NativeSyncAnswer::Found(Some((false, declaring))),
                Some((false, declaring)),
            ),
            (
                NativeSyncAnswer::Found(Some((true, declaring))),
                Some((true, declaring)),
            ),
        ];
        for (answer, fact) in cases {
            let word = NativeSyncFacts::encode(answer, epochs);
            assert_ne!(word, 0);
            assert_eq!(NativeSyncFacts::decode(word, || 7, || 41), Some(fact));
            assert_eq!(NativeSyncFacts::decode(word, || 8, || 40), None);
            // The tag is 30 bits: the counter compares modulo 2^30.
            assert_eq!(
                NativeSyncFacts::decode(word, || 7 + (1 << 30), || 0),
                Some(fact)
            );
        }
        let unresolved = NativeSyncFacts::encode(NativeSyncAnswer::Unresolved, epochs);
        assert_ne!(unresolved, 0, "an unresolved word is not 'never filled'");
        assert_eq!(NativeSyncFacts::decode(unresolved, || 8, || 40), Some(None));
        assert_eq!(NativeSyncFacts::decode(unresolved, || 7, || 41), None);
        // An unresolved answer filled at counter 0 is still distinguishable
        // from the empty word.
        let at_zero = NativeSyncFacts::encode(
            NativeSyncAnswer::Unresolved,
            NativeSyncEpochs {
                found: 0,
                unresolved: 0,
            },
        );
        assert_eq!(NativeSyncFacts::decode(at_zero, || 0, || 0), Some(None));
        assert_eq!(NativeSyncFacts::decode(0, || 0, || 0), None);
    }
}

#[cfg(test)]
mod param_tags_tests {
    use super::{nth_param_tag_byte, ParamTags};

    /// The whole safety argument for replacing the per-argument
    /// `nth_param_tag_byte` scan with one `ParamTags::of`: the two must answer
    /// identically for EVERY index, including out-of-range ones, or a
    /// category-2 `long`/`double` argument gets popped down the category-1
    /// path and its high bits are silently dropped. That is the exact failure
    /// `nth_param_tag_byte`'s own call sites were written to prevent (BC
    /// safegcd `0xFFFC_…` accumulators), and it is silent — a wrong tag
    /// produces a plausible number, not a crash.
    ///
    /// Indices are probed past the parameter count on purpose: the dispatch
    /// arms index by argument slot, which for a wide (category-2) descriptor
    /// runs past the parameter count.
    #[test]
    fn param_tags_match_nth_param_tag_byte() {
        let mut descriptors: Vec<String> = vec![
            "()V".to_string(),
            "()I".to_string(),
            "(I)I".to_string(),
            "(J)J".to_string(),
            "(D)D".to_string(),
            "(F)V".to_string(),
            "(Z)Z".to_string(),
            "(B)B".to_string(),
            "(S)S".to_string(),
            "(C)C".to_string(),
            "(Ljava/lang/String;)V".to_string(),
            "([I)V".to_string(),
            "([[Ljava/lang/Object;)V".to_string(),
            "(IJDLjava/lang/String;[BF)Ljava/lang/Object;".to_string(),
            "(Ljava/lang/String;Ljava/lang/String;)Z".to_string(),
            "([Ljava/lang/String;[[JI)V".to_string(),
            // Degenerate/malformed shapes the scanner must not disagree on.
            "(".to_string(),
            "()".to_string(),
            "(L".to_string(),
            "([".to_string(),
            "(Ljava/lang/String".to_string(),
        ];

        // Exactly at, one below and one above the inline capacity, so the
        // overflow fallback is exercised rather than assumed.
        for n in [15usize, 16, 17, 40] {
            descriptors.push(format!("({})V", "I".repeat(n)));
            descriptors.push(format!("({})V", "J".repeat(n)));
            descriptors.push(format!("({})V", "Ljava/lang/String;".repeat(n)));
            descriptors.push(format!("({})V", "[I".repeat(n)));
        }

        for d in &descriptors {
            let tags = ParamTags::of(d);
            for n in 0..64 {
                assert_eq!(
                    tags.get(d, n),
                    nth_param_tag_byte(d, n),
                    "descriptor {d:?} index {n}"
                );
            }
        }

        // The memoized tokenisation must answer identically to the per-call
        // one, on every descriptor above including the malformed and the
        // overflowing. `DescriptorFacts::of` is this scan moved to where it
        // can be cached on `CachedBytecodeMethod`; if the two ever drift, the
        // invoke path decodes an argument against the wrong tag, which is a
        // silent wrong value rather than a crash — exactly the failure mode
        // `nth_param_tag_byte`'s own call sites exist to prevent.
        for d in &descriptors {
            let facts = cratonvm_jit_api::DescriptorFacts::of(d);
            let from_facts = ParamTags::from_facts(&facts);
            let scanned = ParamTags::of(d);
            for n in 0..64 {
                assert_eq!(
                    from_facts.get(d, n),
                    scanned.get(d, n),
                    "DescriptorFacts disagrees with ParamTags::of for {d:?} index {n}"
                );
            }
            // And the return tag against the scan it replaced.
            assert_eq!(
                facts.ret_tag,
                crate::jit::return_type(d),
                "DescriptorFacts::ret_tag disagrees with jit::return_type for {d:?}"
            );
            // The claim the cached-native arms rest on, stated directly:
            // when the tag array is complete, reading `param_tags[i]` is the
            // same byte `pop_coerced_invoke_args_*` would have obtained by
            // scanning the descriptor it re-resolved. `native_site_facts_usable`
            // admits exactly this shape.
            if !facts.param_tags_overflow {
                for i in 0..facts.param_tag_len as usize {
                    assert_eq!(
                        facts.param_tags[i],
                        scanned.get(d, i),
                        "cached-native facts tag {i} disagrees with the scan for {d:?}"
                    );
                }
            }
        }
    }

    /// `native_site_facts_usable` must refuse exactly the two shapes the tag
    /// array cannot describe, and admit everything else.
    ///
    /// It is the whole guard between the facts-driven argument pop and the
    /// general helper, and both of its refusals are silent-wrong-answer shapes
    /// rather than crashes: an overflowing descriptor has no tags past the
    /// eighth parameter, and a `num_params` that disagrees with the tokenised
    /// count means the entry and the descriptor describe different methods.
    #[test]
    fn cached_native_facts_are_refused_for_the_shapes_they_cannot_describe() {
        use crate::runtime::interpreter::native_site_facts_usable;
        let ok = cratonvm_jit_api::DescriptorFacts::of("(IJLjava/lang/String;)V");
        assert!(native_site_facts_usable(&ok, 3, false));
        assert!(native_site_facts_usable(&ok, 3, true));
        // A count that disagrees with the tokenised one.
        assert!(!native_site_facts_usable(&ok, 2, false));
        assert!(!native_site_facts_usable(&ok, 4, false));
        // More parameters than the inline tag array holds.
        let over = cratonvm_jit_api::DescriptorFacts::of("(IIIIIIIII)V");
        assert!(over.param_tags_overflow);
        assert!(!native_site_facts_usable(&over, 9, false));
        // Zero parameters is the commonest cached-native shape of all.
        let none = cratonvm_jit_api::DescriptorFacts::of("()I");
        assert!(native_site_facts_usable(&none, 0, true));
    }

    /// Slot 0 of a non-static call is the receiver and must answer `b'L'`
    /// whatever the descriptor says, with parameter `k` at slot `k + 1`.
    #[test]
    fn get_with_receiver_offsets_by_one() {
        let d = "(JLjava/lang/String;I)V";
        let tags = ParamTags::of(d);
        assert_eq!(tags.get_with_receiver(d, 0), b'L');
        for k in 0..8 {
            assert_eq!(
                tags.get_with_receiver(d, k + 1),
                nth_param_tag_byte(d, k),
                "slot {} vs param {k}",
                k + 1
            );
        }
    }
}
