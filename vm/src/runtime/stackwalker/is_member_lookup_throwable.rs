#[allow(unused_imports)]
use super::*;

/// The [`StandinFamily::MemberLookup`] rows: the looked-up `Class` methods
/// (each an entry), and `getConstructor0` (reached only as their hop).
pub(in crate::runtime::stackwalker) const MEMBER_LOOKUP_METHODS: &[StandinMethod] = &[
    StandinMethod {
        class: MEMBER_LOOKUP_CLASS,
        name: "getField",
        descriptor: "(Ljava/lang/String;)Ljava/lang/reflect/Field;",
        throws: &[NO_SUCH_FIELD_EXCEPTION],
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: MEMBER_LOOKUP_CLASS,
        name: "getDeclaredField",
        descriptor: "(Ljava/lang/String;)Ljava/lang/reflect/Field;",
        throws: &[NO_SUCH_FIELD_EXCEPTION],
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: MEMBER_LOOKUP_CLASS,
        name: "getMethod",
        descriptor: "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
        throws: &[NO_SUCH_METHOD_EXCEPTION],
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: MEMBER_LOOKUP_CLASS,
        name: "getDeclaredMethod",
        descriptor: "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
        throws: &[NO_SUCH_METHOD_EXCEPTION],
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: MEMBER_LOOKUP_CLASS,
        name: "getConstructor",
        descriptor: "([Ljava/lang/Class;)Ljava/lang/reflect/Constructor;",
        throws: &[NO_SUCH_METHOD_EXCEPTION],
        calls: Some(GET_CONSTRUCTOR0),
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: MEMBER_LOOKUP_CLASS,
        name: "getDeclaredConstructor",
        descriptor: "([Ljava/lang/Class;)Ljava/lang/reflect/Constructor;",
        throws: &[NO_SUCH_METHOD_EXCEPTION],
        calls: Some(GET_CONSTRUCTOR0),
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: MEMBER_LOOKUP_CLASS,
        name: GET_CONSTRUCTOR0.1,
        descriptor: GET_CONSTRUCTOR0.2,
        throws: &[NO_SUCH_METHOD_EXCEPTION],
        entry: false,
        ..StandinMethod::ROW
    },
    // `--compatible` serves the two casts too; each raises its
    // `ClassCastException` at one site.
    StandinMethod {
        class: MEMBER_LOOKUP_CLASS,
        name: "cast",
        descriptor: "(Ljava/lang/Object;)Ljava/lang/Object;",
        throws: &[CLASS_CAST_EXCEPTION],
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: MEMBER_LOOKUP_CLASS,
        name: "asSubclass",
        descriptor: "(Ljava/lang/Class;)Ljava/lang/Class;",
        throws: &[CLASS_CAST_EXCEPTION],
        ..StandinMethod::ROW
    },
];

/// Could a throwable of this class get [`member_lookup_standin_frames`]? The
/// lock-free first screen.
pub(crate) fn is_member_lookup_throwable(throwable_class: &str) -> bool {
    matches!(
        throwable_class,
        NO_SUCH_FIELD_EXCEPTION | NO_SUCH_METHOD_EXCEPTION | CLASS_CAST_EXCEPTION
    )
}

/// The frames HotSpot lists on top of the `NoSuchFieldException` /
/// `NoSuchMethodException` (`throwable_class`) a served `Class` member lookup
/// raised, when the innermost captured frame stands at the lookup's call,
/// OUTERMOST-first (append them), or `None`. See the section comment above.
/// `resolve_class(requester, name)` resolves a class name as the requester's
/// loader would.
pub(crate) fn member_lookup_standin_frames(
    store: &ClassStore,
    innermost: &BacktraceFrame,
    throwable_class: &str,
    resolve_class: &mut dyn FnMut(ClassId, &str) -> Option<ClassId>,
) -> Option<Vec<BacktraceFrame>> {
    use cratonvm_reader::instruction::Instruction;
    if !is_member_lookup_throwable(throwable_class) {
        return None;
    }
    let (caller_id, caller_name, caller_descriptor, bci) = standin_call_site(innermost)?;
    let caller_class = store.get(caller_id)?;
    let caller = caller_class.find_method(&caller_name, &caller_descriptor)?;
    let caller_code = caller.code()?;
    // Every row is an instance method of the final `java.lang.Class`.
    let Instruction::Invokevirtual(cp_index) = Instruction::decode(&caller_code.code, bci).ok()?.0
    else {
        return None;
    };
    let (owner, name, descriptor) = cp_method_ref(&caller_class.constant_pool, cp_index)?;
    if owner != MEMBER_LOOKUP_CLASS {
        return None;
    }
    let row = standin_rows(StandinFamily::MemberLookup).iter().find(|m| {
        m.entry
            && m.name == name
            && m.descriptor == descriptor
            && m.throws.contains(&throwable_class)
    })?;
    let class_id = if owner == &*caller_class.name {
        caller_id
    } else {
        resolve_class(caller_id, owner)?
    };
    let class = store.get(class_id)?;
    if &*class.name != MEMBER_LOOKUP_CLASS {
        return None;
    }
    let slot = |name: &str, descriptor: &str| {
        class
            .methods
            .iter()
            .enumerate()
            .find(|(_, m)| &*m.name == name && &*m.descriptor == descriptor)
    };
    let mut out = Vec::with_capacity(2);
    let (mut index, mut method) = slot(row.name, row.descriptor)?;
    if let Some((owner, next_name, next_descriptor)) = row.calls {
        let call_pc = only_call_to(
            &class.constant_pool,
            &method.code()?.code,
            owner,
            next_name,
            next_descriptor,
        )?;
        out.push(BacktraceFrame::Entry(standin_entry(
            class,
            class_id,
            method,
            index,
            Some(call_pc),
        )));
        (index, method) = slot(next_name, next_descriptor)?;
    }
    let sites = throwable_init_sites(&class.constant_pool, &method.code()?.code, throwable_class)?;
    let [site] = sites[..] else {
        return None;
    };
    out.push(BacktraceFrame::Entry(standin_entry(
        class,
        class_id,
        method,
        index,
        Some(site),
    )));
    true
        .then_some(out)
}

// ---------------------------------------------------------------------------
// A served raise: the JDK method a native stood for, at its throw
// (interpreter round i2 wave 53, lane L3)
// ---------------------------------------------------------------------------
//
// `docs/feature-designs/native-dispatch-architecture.md`.
// The families above each screen the innermost frame's call and the
// throwable's class to guess that a native raised it, and an NPE at an
// `invokevirtual` cannot be told that way from the invoke's own null-receiver
// NPE. Instead, the native-call funnel marks the raise: a native that returns
// an unbuilt `RuntimeError` notes, on the thread, the interpreted frame it was
// called under (`vm_exec::note_served_native_raise`), and the capture of the
// throwable that error becomes takes the mark (`JvmThread::served_native_raise`).
// With it, and only after every family above answered nothing, the method the
// innermost frame's call names is rebuilt from the real class bytes:
//
// * the method at its ONE construction of the throwable's exact class
//   ([`throwable_init_sites`]); or, with none,
// * the method at its ONE call of a method that constructs it, and that
//   callee at its ONE construction (`Class.getField(null)`:
//   `Objects.requireNonNull` at its `new NullPointerException`, `getField` at
//   its call of it).
//
// A JDK `native` method the call names lists its own frame, `(Native Method)`
// (the mark settles what [`native_leaf_frame`] must refuse for an instance
// call's NPE: the invoke's own null receiver). Two sites, two raising calls, a
// callee with two sites, an abstract method or a resource error
// (`StackOverflowError`, `OutOfMemoryError`, raised where the VM ran out, not
// at a JDK `throw`) answer nothing -- the caller stays on top, as before. The
// funnel marks every kind of native, not only `Bridge`s: the frames are the
// JDK method's own, whichever native served it. `VmConfig::served_raise_frames`
// (`CRATONVM_THROWABLE_SERVED_RAISE_FRAMES`) gates the mark.
//
// An IMPLICIT null dereference has no construction to find. A native that
// raises one says so in HotSpot's own words (`Cannot invoke "Object.getClass()"
// because "obj" is null`, `Field.checkAccess`'s), and the funnel keeps that
// action half on the mark ([`ServedNativeRaise::npe_action_of`]); the sites
// are then the method's `invoke*` instructions whose JEP 358 action renders
// exactly so ([`implicit_npe_invoke_sites`]), under the same ONE-site,
// ONE-hop rule. A native that knows its raise is at neither kind of site the
// rule would find (an inner accessor chain) builds its throwable itself, which
// the funnel does not mark. Of two or more constructions, the mark's message
// picks the ONE whose string literal it is
// ([`throwable_init_sites_with_literal`]: `StackWalker.getCallerClass`'s two
// `UnsupportedOperationException`s); none or two literals answer nothing.

/// The native-call funnel's mark of a raise (`vm_exec::note_served_native_raise`,
/// `JvmThread::served_native_raise`): the class of the throwable the error
/// becomes, the innermost interpreted frame's depth and `last_instr_pc` when
/// the native returned its unbuilt error, and the `Cannot invoke "..."`
/// action of an implicit-NPE message. Only a capture of that class takes it:
/// one of another class in between (the first build of the throwable's class
/// running loader Java that builds and catches its own) leaves it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ServedNativeRaise {
    pub(crate) throwable_class: &'static str,
    pub(crate) depth: usize,
    pub(crate) pc: usize,
    /// The marked frame's class and method name: a capture that lists that
    /// frame as an `Entry` (a cold frame the walk materialised as an entry)
    /// is recognised by them ([`ServedNativeRaise::stands_at`]).
    pub(crate) class_id: ClassId,
    pub(crate) method_name: Box<str>,
    pub(crate) npe_action: Option<Box<str>>,
    /// The error's message: picks among two or more constructions the ONE
    /// whose string literal it is ([`throwable_init_sites_with_literal`];
    /// `StackWalker.getCallerClass`'s two `UnsupportedOperationException`s).
    pub(crate) message: Option<Box<str>>,
    /// The class of the native's receiver (its first argument, an object
    /// that is not an array), under `VmConfig::served_raise_receiver`: a
    /// virtual or interface call selects the served method from it, as the
    /// invoke did (interpreter round i2 wave 54, lane L3; proposal
    /// `docs/feature-designs/real-jdk-mode.md`).
    pub(crate) receiver_class: Option<ClassId>,
    /// The JDK chain the native noted for this raise
    /// (`NativeExceptionAccess::note_served_raise_chain`; interpreter round i2
    /// wave 54, lane L3), its first hop the served method.
    pub(crate) chain: Option<Box<cratonvm_native_api::ServedRaiseChain>>,
    /// The JDK `native` method that raised, when a by-name door entered it
    /// with no bytecode call site naming it (`Method.invoke`'s target, a
    /// method handle's): its class and method slot
    /// (`vm_exec::note_door_native_callee`; interpreter round i2 wave 60,
    /// lane L3). HotSpot lists its `(Native Method)` frame on top
    /// ([`door_native_callee_frame`]).
    pub(crate) native_callee: Option<(ClassId, u32)>,
    /// The served class-initialization chain mark this mark displaced while
    /// that initialization's `<clinit>` still ran (interpreter round i5 wave
    /// 104, lane L3; `i103-L3-clinit-trigger-throwable-traces-lack-served-door-frames`
    /// residue (a)): a nested served initialization or a raise marked inside
    /// the `<clinit>` keeps the outer chain here, so the capture that takes
    /// this mark puts the outer one back, and the walk and capture rules list
    /// every standing chain (`vm_exec::standing_init_chain_mark`). The marks
    /// form a stack on the thread; no new field on `JvmThread`.
    pub(crate) outer: Option<Box<ServedNativeRaise>>,
}

impl ServedNativeRaise {
    /// The action half (`Cannot invoke "Owner.m(..)"`, through its closing
    /// quote) of a JEP 358 invoke message, or `None` for any other message.
    pub(crate) fn npe_action_of(message: &str) -> Option<Box<str>> {
        const PREFIX: &str = "Cannot invoke \"";
        let rest = message.strip_prefix(PREFIX)?;
        let end = PREFIX.len() + rest.find('"')? + 1;
        Some(Box::from(&message[..end]))
    }

    /// The mark's site hints, for [`served_raise_frames`].
    pub(crate) fn hints(&self) -> ServedRaiseHints<'_> {
        ServedRaiseHints {
            npe_action: self.npe_action.as_deref(),
            message: self.message.as_deref(),
            receiver_class: self.receiver_class,
            chain: self.chain.as_deref(),
        }
    }

    /// May the capture whose innermost frame is `innermost` believe the mark?
    /// An interpreted frame must stand at the marked instruction; a compiled
    /// or inlined one stands under the marked interpreted frame (its caller's
    /// depth and pc are what the capture checked); an entry must BE the marked
    /// frame -- its class and method (wave 53 hand-back: the capture
    /// lists a cold caller as an `Entry`, and refusing every entry lost the
    /// mark on every first call) -- so a stand-in or spliced entry, some
    /// family's frame, is still refused.
    pub(crate) fn stands_at(&self, innermost: &BacktraceFrame) -> bool {
        match innermost {
            BacktraceFrame::Method { bci, .. } => usize::try_from(*bci) == Ok(self.pc),
            BacktraceFrame::Compiled { .. } | BacktraceFrame::Inlined { .. } => true,
            // Not the bci: an OSR'd caller's interpreted frame keeps the pc it
            // entered compiled code at, while the entry names the call
            // (measured, `L3W53ServedRaiseFrames` `--compatible` with the JIT,
            // `field-null-again`). The rule decodes the entry's own bci.
            BacktraceFrame::Entry(entry) => {
                entry.class_id == Some(self.class_id) && *entry.method_name == *self.method_name
            }
        }
    }
}

/// `kind class.method@bci` of a captured frame, for the served-raise debug
/// lines (`CRATONVM_FRAME_TRACE`).
pub(crate) fn backtrace_frame_debug(frame: &BacktraceFrame) -> String {
    let kind = match frame {
        BacktraceFrame::Method { .. } => "interpreted",
        BacktraceFrame::Compiled { .. } => "compiled",
        BacktraceFrame::Inlined { .. } => "inlined",
        BacktraceFrame::Entry(_) => "entry",
    };
    let e = frame.to_entry();
    format!(
        "{kind} {}.{}@{}",
        e.class_name, e.method_name, e.byte_code_index
    )
}

/// What a [`ServedNativeRaise`] tells [`served_raise_frames`] about the site:
/// an implicit NPE's action, and the error's message.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ServedRaiseHints<'a> {
    pub(crate) npe_action: Option<&'a str>,
    pub(crate) message: Option<&'a str>,
    /// [`ServedNativeRaise::receiver_class`].
    pub(crate) receiver_class: Option<ClassId>,
    /// [`ServedNativeRaise::chain`].
    pub(crate) chain: Option<&'a cratonvm_native_api::ServedRaiseChain>,
}

/// The throwables a served raise never lists frames for: raised where the VM
/// ran out of a resource, not at a JDK method's `throw`.
pub(in crate::runtime::stackwalker) const SERVED_RAISE_EXCLUDED: [&str; 2] =
    ["java/lang/StackOverflowError", "java/lang/OutOfMemoryError"];

pub(in crate::runtime::stackwalker) const SERVED_RAISE_NPE: &str = "java/lang/NullPointerException";

pub(in crate::runtime::stackwalker) const SERVED_RAISE_CCE: &str = "java/lang/ClassCastException";

/// The pcs of the `invoke*` instructions of `code` (not `invokestatic`, not a
/// constructor call: neither dereferences a receiver) whose JEP 358 action
/// renders exactly `action`. `None` when `code` does not decode.
pub(in crate::runtime::stackwalker) fn implicit_npe_invoke_sites(
    cp: &cratonvm_reader::constant_pool::ConstantPool,
    code: &[u8],
    action: &str,
) -> Option<Vec<usize>> {
    use cratonvm_reader::instruction::Instruction;
    let mut sites = Vec::new();
    let mut pc = 0usize;
    while pc < code.len() {
        let (instruction, next) = Instruction::decode(code, pc).ok()?;
        let index = match instruction {
            Instruction::Invokevirtual(index)
            | Instruction::Invokespecial(index)
            | Instruction::Invokeinterface { index, .. } => Some(index),
            _ => None,
        };
        if let Some((owner, name, descriptor)) = index.and_then(|index| cp_method_ref(cp, index)) {
            if name != "<init>"
                && crate::runtime::exceptions::helpful_npe::action_invoke(owner, name, descriptor)
                    == action
            {
                sites.push(pc);
            }
        }
        if next <= pc {
            return None;
        }
        pc = next;
    }
    Some(sites)}

/// [`throwable_init_sites`], each with the ONE string literal (`ldc` /
/// `ldc_w` of a `String`) loaded between its `new` and its constructor call:
/// `None` for none or two (a message built at run time, a concatenation).
/// `None` overall when `code` does not decode.
pub(in crate::runtime::stackwalker) fn throwable_init_sites_with_literal<'a>(
    cp: &'a cratonvm_reader::constant_pool::ConstantPool,
    code: &[u8],
    throwable_class: &str,
) -> Option<Vec<(usize, Option<&'a str>)>> {
    use cratonvm_reader::constant_pool::ConstantPoolEntry;
    use cratonvm_reader::instruction::Instruction;
    let string_at = |index: u16| -> Option<&'a str> {
        match cp.get(index)? {
            ConstantPoolEntry::StringReference { string_index } => cp.get_utf8(*string_index),
            _ => None,
        }
    };
    // `(site, literal, literals seen)` per construction; the open ones, by
    // index into `sites`, innermost last.
    let mut sites: Vec<(usize, Option<&'a str>, u32)> = Vec::new();
    let mut open: Vec<usize> = Vec::new();
    let mut pc = 0usize;
    while pc < code.len() {
        let (instruction, next) = Instruction::decode(code, pc).ok()?;
        match instruction {
            Instruction::New(cp_index) if cp.get_class_name(cp_index) == Some(throwable_class) => {
                open.push(sites.len());
                sites.push((pc, None, 0));
            }
            Instruction::Ldc(index) => {
                if let (Some(&k), Some(text)) = (open.last(), string_at(u16::from(index))) {
                    sites[k].1 = Some(text);
                    sites[k].2 += 1;
                }
            }
            Instruction::LdcW(index) => {
                if let (Some(&k), Some(text)) = (open.last(), string_at(index)) {
                    sites[k].1 = Some(text);
                    sites[k].2 += 1;
                }
            }
            Instruction::Invokespecial(cp_index) if matches!(cp_method_ref(cp, cp_index), Some((o, n, _)) if o == throwable_class && n == "<init>") => {
                if let Some(k) = open.pop() {
                    sites[k].0 = pc;
                }
            }
            _ => {}
        }
        if next <= pc {
            return None;
        }
        pc = next;
    }
    Some(
        sites
            .into_iter()
            .map(|(site, literal, seen)| (site, literal.filter(|_| seen == 1)))
            .collect(),
    )
}

/// The raise sites of `code` for a served raise of `throwable_class`: its
/// implicit-NPE invokes when the mark carries an action, else its
/// constructions of the class -- of two or more, the ones whose string
/// literal is the error's `message`.
pub(in crate::runtime::stackwalker) fn served_raise_sites(
    cp: &cratonvm_reader::constant_pool::ConstantPool,
    code: &[u8],
    throwable_class: &str,
    hints: ServedRaiseHints<'_>,
) -> Option<Vec<usize>> {
    let message = hints.message;
    match hints.npe_action {
        Some(action) if throwable_class == SERVED_RAISE_NPE => {
            implicit_npe_invoke_sites(cp, code, action)
        }
        _ => {
            let sites = throwable_init_sites_with_literal(cp, code, throwable_class)?;
            if sites.len() < 2 {
                return Some(sites.into_iter().map(|(site, _)| site).collect());
            }
            let Some(message) = message else {
                return Some(sites.into_iter().map(|(site, _)| site).collect());
            };
            let by_message: Vec<usize> = sites
                .iter()
                .filter(|(_, literal)| *literal == Some(message))
                .map(|&(site, _)| site)
                .collect();
            // No literal says it: every site stays (two answer nothing).
            if by_message.is_empty() {
                Some(sites.into_iter().map(|(site, _)| site).collect())
            } else {
                Some(by_message)
            }
        }
    }
}

/// The method a call of `(name, descriptor)` named on class `owner_id` reaches
/// -- `owner_id` or its nearest superclass declaring it -- when it has code.
pub(in crate::runtime::stackwalker) fn served_raise_code_method<'s>(
    store: &'s ClassStore,
    owner_id: ClassId,
    name: &str,
    descriptor: &str,
) -> Option<(ClassId, &'s Class, usize, &'s ClassFileMethod)> {
    let found = virtual_selected_method(store, owner_id, name, descriptor)?;
    (!found.3.is_native() && !found.3.is_abstract() && found.3.code().is_some()).then_some(found)
}

/// The pc of the ONE call in `code` (a method of `class`, id `class_id`)
/// whose callee constructs `throwable_class`, with that callee and its ONE
/// construction site; `None` for no such call, two, a callee with two sites,
/// or a decode failure. A call whose owner is not loaded cannot have run and
/// is skipped.
#[allow(clippy::type_complexity)]
pub(in crate::runtime::stackwalker) fn served_raise_only_raising_call<'s>(
    store: &'s ClassStore,
    class_id: ClassId,
    class: &'s Class,
    code: &[u8],
    throwable_class: &str,
    hints: ServedRaiseHints<'_>,
    resolve_class: &mut dyn FnMut(ClassId, &str) -> Option<ClassId>,
) -> Option<(
    usize,
    (ClassId, &'s Class, usize, &'s ClassFileMethod, usize),
)> {
    use cratonvm_reader::instruction::Instruction;
    let mut found = None;
    let mut pc = 0usize;
    while pc < code.len() {
        let (instruction, next) = Instruction::decode(code, pc).ok()?;
        let index = match instruction {
            Instruction::Invokevirtual(index)
            | Instruction::Invokespecial(index)
            | Instruction::Invokestatic(index)
            | Instruction::Invokeinterface { index, .. } => Some(index),
            _ => None,
        };
        if let Some((owner, name, descriptor)) =
            index.and_then(|index| cp_method_ref(&class.constant_pool, index))
        {
            let owner_id = if owner == &*class.name {
                Some(class_id)
            } else {
                resolve_class(class_id, owner)
            };
            let callee =
                owner_id.and_then(|id| served_raise_code_method(store, id, name, descriptor));
            if let Some((callee_id, callee_class, slot, method)) = callee {
                let sites = served_raise_sites(
                    &callee_class.constant_pool,
                    &method.code()?.code,
                    throwable_class,
                    hints,
                )?;
                match sites[..] {
                    [] => {}
                    [site] if found.is_none() => {
                        found = Some((pc, (callee_id, callee_class, slot, method, site)));
                    }
                    _ => return None,
                }
            }
        }
        if next <= pc {
            return None;
        }
        pc = next;
    }
    found}

/// Is `sub` the class `sup`, or one of its subclasses or implementors (its
/// superclasses and, transitively, their interfaces)? Bounded; a class the
/// store does not hold ends its branch.
pub(in crate::runtime::stackwalker) fn class_is_subtype_of(store: &ClassStore, sub: ClassId, sup: ClassId) -> bool {
    let mut pending = vec![sub];
    let mut seen = 0usize;
    while let Some(id) = pending.pop() {
        if id == sup {
            return true;
        }
        seen += 1;
        if seen > 256 {
            return false;
        }
        if let Some(class) = store.get(id) {
            pending.extend(class.superclass);
            pending.extend(class.interfaces.iter().copied());
        }
    }
    false
}

/// The method a virtual or interface call of `(name, descriptor)` selects on
/// an instance of `receiver` (interpreter round i2 wave 54, lane L3; JVMS
/// 5.4.6): the class or its nearest superclass declaring it, else the ONE
/// non-abstract, non-static method of that name among the class's
/// superinterfaces (a default method; two candidates answer nothing). An
/// abstract selection answers nothing.
pub(in crate::runtime::stackwalker) fn receiver_selected_method<'s>(
    store: &'s ClassStore,
    receiver: ClassId,
    name: &str,
    descriptor: &str,
) -> Option<(ClassId, &'s Class, usize, &'s ClassFileMethod)> {
    if let Some(found) = virtual_selected_method(store, receiver, name, descriptor) {
        return (!found.3.is_abstract()).then_some(found);
    }
    let mut pending: Vec<ClassId> = Vec::new();
    let mut id = Some(receiver);
    for _ in 0..64 {
        let Some(class_id) = id else { break };
        let Some(class) = store.get(class_id) else {
            break;
        };
        pending.extend(class.interfaces.iter().copied());
        id = class.superclass;
    }
    let mut found: Option<(ClassId, &'s Class, usize, &'s ClassFileMethod)> = None;
    let mut visited: Vec<ClassId> = Vec::new();
    while let Some(iface) = pending.pop() {
        if visited.contains(&iface) || visited.len() > 256 {
            continue;
        }
        visited.push(iface);
        let Some(class) = store.get(iface) else {
            continue;
        };
        pending.extend(class.interfaces.iter().copied());
        let hit = class.methods.iter().enumerate().find(|(_, m)| {
            &*m.name == name && &*m.descriptor == descriptor && !m.is_abstract() && !m.is_static()
        });
        if let Some((slot, method)) = hit {
            if let Some((other, ..)) = found {
                if other != iface {
                    return None;
                }
            }
            found = Some((iface, class, slot, method));
        }
    }
    found
}

/// The pc of the LAST (`first`: the first) `invoke*` of `code` that calls a
/// method named `name` (with descriptor `descriptor`, any when `None`), or
/// `None`.
pub(in crate::runtime::stackwalker) fn named_call_site(
    cp: &cratonvm_reader::constant_pool::ConstantPool,
    code: &[u8],
    name: &str,
    descriptor: Option<&str>,
    first: bool,
) -> Option<usize> {
    use cratonvm_reader::instruction::Instruction;
    let mut last = None;
    let mut pc = 0usize;
    while pc < code.len() {
        let (instruction, next) = Instruction::decode(code, pc).ok()?;
        let index = match instruction {
            Instruction::Invokevirtual(index)
            | Instruction::Invokespecial(index)
            | Instruction::Invokestatic(index)
            | Instruction::Invokeinterface { index, .. } => Some(index),
            _ => None,
        };
        if let Some((_, callee, callee_descriptor)) =
            index.and_then(|index| cp_method_ref(cp, index))
        {
            if callee == name && descriptor.is_none_or(|d| d == callee_descriptor) {
                if first {
                    return Some(pc);
                }
                last = Some(pc);
            }
        }
        if next <= pc {
            return None;
        }
        pc = next;
    }
    last}

/// The frames of a [`cratonvm_native_api::ServedRaiseChain`] (interpreter
/// round i2 wave 54, lane L3), OUTERMOST first: `first` is the served method
/// (the chain's first hop), each further hop is looked up by name as the
/// previous hop's class would resolve it, and each hop is listed at its site
/// ([`cratonvm_native_api::ServedRaiseHopSite`]): its last call of the named
/// method, or, the innermost hop only, its one raise site. Any hop that
/// cannot be found or read answers nothing.
pub(in crate::runtime::stackwalker) fn served_raise_chain_frames<'s>(
    store: &'s ClassStore,
    first: (ClassId, &'s Class, usize, &'s ClassFileMethod),
    chain: &cratonvm_native_api::ServedRaiseChain,
    throwable_class: &str,
    hints: ServedRaiseHints<'_>,
    resolve_class: &mut dyn FnMut(ClassId, &str) -> Option<ClassId>,
) -> Option<Vec<BacktraceFrame>> {
    use cratonvm_native_api::ServedRaiseHopSite;
    let mut frames = Vec::with_capacity(chain.hops.len());
    let mut previous = first.0;
    for (i, hop) in chain.hops.iter().enumerate() {
        let (class_id, class, slot, method) = if i == 0 {
            first
        } else {
            let id = resolve_class(previous, hop.class)?;
            virtual_selected_method(store, id, hop.name, hop.descriptor)?
        };
        let innermost = i + 1 == chain.hops.len();
        // The served invoker HotSpot runs hidden frames for: not listed; the
        // next hop resolves from its class (wave 56, lane L3).
        if hop.site == ServedRaiseHopSite::Hidden {
            if i != 0 || innermost {
                return None;
            }
            previous = class_id;
            continue;
        }
        // A JDK `native` method raising from inside, the innermost hop only:
        // its own `(Native Method)` frame (`Array.newArray`).
        if method.is_native() && innermost && hop.site == ServedRaiseHopSite::Raise {
            frames.push(BacktraceFrame::Entry(standin_entry(
                class, class_id, method, slot, None,
            )));
            break;
        }
        if method.is_native() || method.is_abstract() {
            return None;
        }
        let code: &[u8] = &method.code()?.code;
        let site = match hop.site {
            ServedRaiseHopSite::FirstCall { name, descriptor } if !innermost => {
                named_call_site(&class.constant_pool, code, name, descriptor, true)?
            }
            ServedRaiseHopSite::LastCall { name, descriptor } if !innermost => {
                named_call_site(&class.constant_pool, code, name, descriptor, false)?
            }
            ServedRaiseHopSite::Raise if innermost => {
                match served_raise_sites(&class.constant_pool, code, throwable_class, hints)?[..] {
                    [site] => site,
                    _ => return None,
                }
            }
            // JIT round j27 wave 4, lane MHROWS2: a `checkcast` raise, the
            // reflection path's `NotANumber` rule (`argument_cast_frames`).
            ServedRaiseHopSite::LocalCheckcast { class_name }
                if innermost && throwable_class == SERVED_RAISE_CCE =>
            {
                last_checkcast_of_a_local_to(&class.constant_pool, code, class_name)?
            }
            _ => return None,
        };
        frames.push(BacktraceFrame::Entry(standin_entry(
            class,
            class_id,
            method,
            slot,
            Some(site),
        )));
        previous = class_id;    }
    Some(frames)
}

/// The frames HotSpot lists on top of a throwable of class `throwable_class`
/// that a native serving the call the innermost captured frame stands at
/// raised (the funnel's mark, `JvmThread::served_native_raise`),
/// OUTERMOST-first (append them), or `None`. See the section comment above.
/// `hints` are the mark's ([`ServedNativeRaise::hints`]).
/// `resolve_class(requester, name)` resolves a class name as the requester's
/// loader would.
pub(crate) fn served_raise_frames(
    store: &ClassStore,
    innermost: &BacktraceFrame,
    throwable_class: &str,
    hints: ServedRaiseHints<'_>,
    resolve_class: &mut dyn FnMut(ClassId, &str) -> Option<ClassId>,
) -> Option<Vec<BacktraceFrame>> {
    use cratonvm_reader::instruction::Instruction;
    if SERVED_RAISE_EXCLUDED.contains(&throwable_class) {
        return None;
    }
    let (caller_id, caller_name, caller_descriptor, bci) = standin_call_site(innermost)?;
    let caller_class = store.get(caller_id)?;
    let caller = caller_class.find_method(&caller_name, &caller_descriptor)?;
    // `dispatched`: the call selects by its receiver's class (wave 54).
    let (index, dispatched) = match Instruction::decode(&caller.code()?.code, bci).ok()?.0 {
        Instruction::Invokevirtual(index) | Instruction::Invokeinterface { index, .. } => {
            (index, true)
        }
        Instruction::Invokespecial(index) | Instruction::Invokestatic(index) => (index, false),
        _ => return None,
    };
    let (owner, name, descriptor) = cp_method_ref(&caller_class.constant_pool, index)?;
    if name == "<clinit>" {
        return None;
    }
    let owner_id = if owner == &*caller_class.name {
        caller_id
    } else {
        resolve_class(caller_id, owner)?
    };
    // Interpreter round i2 wave 54, lane L3: a virtual or interface call
    // selects from the receiver's class the mark kept, when that class is
    // the owner's subtype (a mark some inner by-name call left names another
    // receiver: the static selection answers then, as before).
    let from_receiver = hints
        .receiver_class
        .filter(|_| dispatched)
        .filter(|&receiver| class_is_subtype_of(store, receiver, owner_id))
        .and_then(|receiver| receiver_selected_method(store, receiver, name, descriptor));
    if crate::runtime::env_cache::frame_trace() && dispatched && hints.receiver_class.is_some() {
        eprintln!(
            "[SERVED_RAISE_RECEIVER] {owner}.{name}{descriptor} from receiver: {}",
            from_receiver
                .map(|found| format!("{}.{}", found.1.name, found.3.name))
                .unwrap_or_else(|| "declined".to_string())
        );
    }
    // Interpreter round i2 wave 56, lane L3: a chain whose first hop is a
    // served signature-polymorphic invoker (`ServedRaiseHopSite::Hidden`,
    // `MethodHandle.invokeExact` of a field handle's null base) names no
    // descriptor the call site carries: match it by class and invoker name
    // (`invokeExact` / `invoke`, or `invokeWithArguments(Object...)`, Java
    // HotSpot lists as its own frame at its `invokeExact` of the spreader),
    // and select the invoker's own declaration, which is not listed.
    const WITH_ARGUMENTS_ARRAY: &str = "([Ljava/lang/Object;)Ljava/lang/Object;";
    // Wave 59, lane L4: `invokeWithArguments(List)`, served too, is HotSpot's
    // frame at its call of the array overload, outside that one's.
    let with_list =
        name == "invokeWithArguments" && descriptor == "(Ljava/util/List;)Ljava/lang/Object;";
    let with_arguments =
        with_list || (name == "invokeWithArguments" && descriptor == WITH_ARGUMENTS_ARRAY);
    // Interpreter round i2 wave 59, lane L4: a `VarHandle` access mode is its
    // own served invoker (a static VarHandle's holder initialization,
    // `LazyInitializingVarHandle`'s frames below `VarHandleGuards`): matched
    // by its name, which the hop carries.
    if let Some(chain) = hints.chain.filter(|chain| {
        chain.hops.first().is_some_and(|hop| {
            hop.site == cratonvm_native_api::ServedRaiseHopSite::Hidden
                && hop.class == owner
                && (with_arguments
                    // Wave 62, lane L3: `invokeBasic`, the call a spun
                    // `LambdaForm$MH` (hidden, the marked frame the drop
                    // removed) makes of its target under `--real-jdk`.
                    || matches!(name, "invokeExact" | "invoke" | "invokeBasic")
                    || (owner == "java/lang/invoke/VarHandle" && hop.name == name))
        })
    }) {
        let invoker = if with_arguments { "invokeExact" } else { name };
        let first = store.get(owner_id).and_then(|owner_class| {
            owner_class
                .methods
                .iter()
                .enumerate()
                .find(|(_, m)| &*m.name == invoker)
                .map(|(slot, m)| (owner_id, owner_class, slot, m))
        })?;
        let mut frames =
            served_raise_chain_frames(store, first, chain, throwable_class, hints, resolve_class);
        if with_arguments {
            // `invokeWithArguments` itself, at its call of the spreader's
            // `invokeExact`, outermost.
            let at_call = |callee: &str, callee_descriptor: Option<&str>, descriptor: &str| {
                virtual_selected_method(store, owner_id, name, descriptor).and_then(
                    |(id, class, slot, method)| {
                        let code: &[u8] = &method.code()?.code;
                        let site = named_call_site(
                            &class.constant_pool,
                            code,
                            callee,
                            callee_descriptor,
                            false,
                        )?;
                        Some(BacktraceFrame::Entry(standin_entry(
                            class,
                            id,
                            method,
                            slot,
                            Some(site),
                        )))
                    },
                )
            };
            let outer = at_call("invokeExact", None, WITH_ARGUMENTS_ARRAY);
            let list = if with_list {
                at_call(name, Some(WITH_ARGUMENTS_ARRAY), descriptor).map(Some)
            } else {
                Some(None)
            };
            frames = match (list, outer, frames) {
                (Some(list), Some(outer), Some(mut inner)) => {
                    inner.insert(0, outer);
                    if let Some(list) = list {
                        inner.insert(0, list);
                    }
                    Some(inner)
                }
                _ => None,
            };
        }
        if crate::runtime::env_cache::frame_trace() {
            eprintln!(
                "[SERVED_RAISE_FRAMES] {owner}.{name}{descriptor} (hidden invoker) raised \
                 {throwable_class} (chain of {}): {} frame(s)",
                chain.hops.len(),
                frames.as_ref().map_or(0, Vec::len)
            );
        }
        return frames;
    }
    let found = match from_receiver {
        Some(found) => found,
        None => virtual_selected_method(store, owner_id, name, descriptor)?,
    };
    served_raise_method_frames(store, found, throwable_class, hints, resolve_class)
}

/// The hidden `VarHandleGuards.guard_<shape>` frame HotSpot runs between a
/// caller standing at an `invokevirtual VarHandle.<mode>` and the access
/// mode's implementation (`VarHandle<T>s$FieldStatic*.<mode>`, reached by the
/// guard's `MethodHandle.linkToStatic`), which this VM serves natively and
/// never pushes. The guard is the one whose descriptor is the call's,
/// erased (references to `Object`), between the leading `VarHandle` and the
/// trailing `VarHandle$AccessDescriptor`; a call type the class has no guard
/// for (HotSpot spins a `LambdaForm` invoker then) answers `None`, as does a
/// caller not at such a call or a `VarHandleGuards` not loaded. Interpreter
/// round i5 wave 102, lane L3 (`L3W100ClinitTriggerWalks` row
/// `VarHandle.get hidden`).
pub(crate) fn var_handle_guard_entry(
    store: &ClassStore,
    caller: &StackTraceEntry,
    resolve_class: &mut dyn FnMut(ClassId, &str) -> Option<ClassId>,
) -> Option<StackTraceEntry> {
    use cratonvm_reader::instruction::Instruction;
    let (caller_id, caller_class, _, caller_method) = entry_method(store, caller)?;
    let bci = usize::try_from(caller.byte_code_index).ok()?;
    let index = match Instruction::decode(&caller_method.code()?.code, bci).ok()?.0 {
        Instruction::Invokevirtual(index) => index,
        _ => return None,
    };
    let (owner, _, descriptor) = cp_method_ref(&caller_class.constant_pool, index)?;
    if owner != "java/lang/invoke/VarHandle" {
        return None;
    }
    let guard_descriptor = var_handle_guard_descriptor(descriptor)?;
    let guards_id = resolve_class(caller_id, "java/lang/invoke/VarHandleGuards")?;
    let guards = store.get(guards_id)?;
    let (slot, method) = guards.methods.iter().enumerate().find(|(_, m)| {
        m.name.starts_with("guard_") && &*m.descriptor == guard_descriptor.as_str()
    })?;
    let code: &[u8] = &method.code()?.code;
    let site = named_call_site(&guards.constant_pool, code, "linkToStatic", None, true)?;
    Some(standin_entry(guards, guards_id, method, slot, Some(site)))
}

/// The `VarHandleGuards` descriptor serving a `VarHandle` call of descriptor
/// `call` (see [`var_handle_guard_entry`]).
pub(in crate::runtime::stackwalker) fn var_handle_guard_descriptor(call: &str) -> Option<String> {
    fn erase(t: &str) -> &str {
        if t.starts_with('L') || t.starts_with('[') {
            "Ljava/lang/Object;"
        } else {
            t
        }
    }
    let params = descriptor_field_types(call)?;
    let (_, ret) = call.split_once(')')?;
    let mut out = String::from("(Ljava/lang/invoke/VarHandle;");
    for p in params {
        out.push_str(erase(p));
    }
    out.push_str("Ljava/lang/invoke/VarHandle$AccessDescriptor;)");
    out.push_str(erase(ret));
    Some(out)
}

/// The keyed body of [`served_raise_frames`] (interpreter round i2 wave 61,
/// lane L3): the frames of the served method `found` (its class id, class,
/// slot and method) raising `throwable_class`, OUTERMOST-first, or `None`.
pub(in crate::runtime::stackwalker) fn served_raise_method_frames<'s>(
    store: &'s ClassStore,
    found: (ClassId, &'s Class, usize, &'s ClassFileMethod),
    throwable_class: &str,
    hints: ServedRaiseHints<'_>,
    resolve_class: &mut dyn FnMut(ClassId, &str) -> Option<ClassId>,
) -> Option<Vec<BacktraceFrame>> {
    let (class_id, class, slot, method) = found;
    // Interpreter round i2 wave 54, lane L3: a chain the native noted, whose
    // first hop is this method, answers alone (a chain that cannot be
    // rebuilt answers nothing: its raise is at no site the one-hop rule
    // would find). A chain of another method is stale and ignored.
    let chain = hints.chain.filter(|chain| {
        chain.hops.first().is_some_and(|hop| {
            hop.class == &*class.name
                && hop.name == &*method.name
                && hop.descriptor == &*method.descriptor
        })
    });
    if let Some(chain) = chain {
        let frames = served_raise_chain_frames(
            store,
            (class_id, class, slot, method),
            chain,
            throwable_class,
            hints,
            resolve_class,
        );
        if crate::runtime::env_cache::frame_trace() {
            eprintln!(
                "[SERVED_RAISE_FRAMES] {}.{}{} raised {throwable_class} (chain of {}): {} frame(s)",
                class.name,
                method.name,
                method.descriptor,
                chain.hops.len(),
                frames.as_ref().map_or(0, Vec::len)
            );
        }
        return frames;
    }
    // A JDK `native` method raising from inside: its own frame, `(Native
    // Method)`, as HotSpot lists it (`Class.isAssignableFrom(null)`). The
    // mark is what tells this from the invoke's own receiver NPE, which
    // [`native_leaf_frame`] must refuse without it.
    if method.is_native() {
        let frame = BacktraceFrame::Entry(standin_entry(class, class_id, method, slot, None));
        if crate::runtime::env_cache::frame_trace() {
            eprintln!(
                "[SERVED_RAISE_FRAMES] {}.{}{} (native) raised {throwable_class}: 1 frame(s)",
                class.name, method.name, method.descriptor
            );
        }
        return Some(vec![frame]);
    }
    if method.is_abstract() {
        return None;
    }
    let code: &[u8] = &method.code()?.code;
    let sites = served_raise_sites(&class.constant_pool, code, throwable_class, hints)?;
    let frames = match sites[..] {
        [site] => vec![BacktraceFrame::Entry(standin_entry(
            class,
            class_id,
            method,
            slot,
            Some(site),
        ))],
        [] => {
            let (call_pc, (callee_id, callee_class, callee_slot, callee, callee_site)) =
                served_raise_only_raising_call(
                    store,
                    class_id,
                    class,
                    code,
                    throwable_class,
                    hints,
                    resolve_class,
                )?;
            vec![
                BacktraceFrame::Entry(standin_entry(class, class_id, method, slot, Some(call_pc))),
                BacktraceFrame::Entry(standin_entry(
                    callee_class,
                    callee_id,
                    callee,
                    callee_slot,
                    Some(callee_site),
                )),
            ]        }
        _ => return None,
    };
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[SERVED_RAISE_FRAMES] {}.{}{} raised {throwable_class}: {} frame(s)",
            class.name,
            method.name,
            method.descriptor,
            frames.len()
        );
    }
    Some(frames)
}

// ---------------------------------------------------------------------------
// Natives that call Java: `Class.forName0`
// ---------------------------------------------------------------------------
//
// Round 15 wave 1 (lane trace; TR6-1 of `jit-r14-trace6-proposals-RETIRED-20260930.md`). The
// rules above add frames at the INNERMOST end only. `Class.forName0` calls the
// class loader's Java `loadClass` (and, initializing, the class's `<clinit>`),
// and what that Java throws was captured with the native's frame missing in the
// MIDDLE. HotSpot 25, `Class.forName("nope")` from `Main.main`:
//
// ```text
// java.base/jdk.internal.loader.BuiltinClassLoader.loadClass(BuiltinClassLoader.java:...)
// java.base/jdk.internal.loader.ClassLoaders$AppClassLoader.loadClass(ClassLoaders.java:...)
// java.base/java.lang.ClassLoader.loadClass(ClassLoader.java:...)
// java.base/java.lang.Class.forName0(Native Method)
// java.base/java.lang.Class.forName(Class.java:...)
// java.base/java.lang.Class.forName(Class.java:...)
// Main.main(Main.java:3)
// ```
//
// (The application loader's three frames are missing here for another reason:
// the base `ClassLoader.loadClass` is a registered native, so that miss runs
// no Java at all and gets only the innermost form below --
// `r15w1-trace-served-app-loader-cnfe-has-no-loader-frames-FIXED-20260930.md`. A
// loader whose own `loadClass` is Java is the middle case.)
//
// `--real-jdk` ran both `forName` frames and lost `forName0`; `--compatible`
// serves `Class.forName` itself (a registered native) and lost all three. An
// adjacent pair (caller, callee) of an OUTERMOST-first capture gets the frames
// of the call the caller stands at when that call is an `invokestatic` of a
// census method ([`CALLBACK_NATIVE_ENTRIES`]) and the callee is a `loadClass`
// or `<clinit>` activation -- what the native runs, and the lock-free screen
// that keeps every other pair off the class-manager lock. The frames are
// rebuilt from the real class bytes: each Java hop at its ONE call of the next
// `Class.forName*` method (two calls answer nothing), down to
// `forName0 (Native Method)`.
//
// The same frames go on top of a capture whose INNERMOST frame stands at such
// a call ([`callback_native_leaf_frames`]): the throwables the native raises
// itself -- `ClassNotFoundException` from the VM, `NullPointerException` for a
// null name, `ExceptionInInitializerError` / `NoClassDefFoundError` of the
// initialization it runs. [`native_leaf_frame`] refuses the initialization
// family (an ordinary invoke raises it BEFORE entering) and cannot rebuild a
// served Java `forName`. `java.lang.Class` is initialized and linked long
// before any such call, so for these calls every throwable but
// `StackOverflowError` was raised inside, as on HotSpot.
// `CRATONVM_THROWABLE_CALLBACK_NATIVE_FRAMES=0` turns both off.
//
// Not in the census: `NativeMethodAccessorImpl.invoke0` /
// `NativeConstructorAccessorImpl.newInstance0` (JDK 25 reflects through
// `DirectMethodHandleAccessor`, and this VM serves `Method.invoke` itself:
// the reflection frames section), and `Class.forName(Module, String)` (its
// JDK body calls the loader in Java, not `forName0`).

pub(in crate::runtime::stackwalker) const CALLBACK_NATIVE_CLASS: &str = "java/lang/Class";

/// `(name, descriptor)` of the one native the census rebuilds down to.
pub(in crate::runtime::stackwalker) const CALLBACK_NATIVE_LEAF: (&str, &str) = (
    "forName0",
    "(Ljava/lang/String;ZLjava/lang/ClassLoader;Ljava/lang/Class;)Ljava/lang/Class;",
);

/// `(name, descriptor)` of the `java.lang.Class` calls a chain may start at:
/// the public `forName` overloads `--compatible` serves, and the native.
pub(in crate::runtime::stackwalker) const CALLBACK_NATIVE_ENTRIES: [(&str, &str); 3] = [
    ("forName", "(Ljava/lang/String;)Ljava/lang/Class;"),
    (
        "forName",
        "(Ljava/lang/String;ZLjava/lang/ClassLoader;)Ljava/lang/Class;",
    ),
    CALLBACK_NATIVE_LEAF,
];

/// The most hops a chain takes (JDK 25: `forName(String)` ->
/// `forName(String, Class)` -> `forName0`).
pub(in crate::runtime::stackwalker) const CALLBACK_CHAIN_MAX: usize = 4;

/// `CRATONVM_THROWABLE_CALLBACK_NATIVE_FRAMES` (default on); read only once a
/// chain was rebuilt.
pub(in crate::runtime::stackwalker) fn callback_native_frames_enabled() -> bool {
    true
}

/// `true` when `frame`, interpreted, certainly does not stand at an
/// `invokestatic` (every census call is one).
pub(in crate::runtime::stackwalker) fn certainly_not_at_invokestatic(frame: &BacktraceFrame) -> bool {
    match frame {
        BacktraceFrame::Method { method, bci, .. } => {
            usize::try_from(*bci)
                .ok()
                .and_then(|bci| method.code.get(bci).copied())
                != Some(0xb8)
        }
        _ => false,
    }
}

/// The census call `frame` stands at: `java.lang.Class` (id and class) and the
/// called `(name, descriptor)`, or `None` for any other instruction or call.
pub(in crate::runtime::stackwalker) fn callback_native_call_at<'s>(
    store: &'s ClassStore,
    frame: &BacktraceFrame,
    resolve_class: &mut dyn FnMut(ClassId, &str) -> Option<ClassId>,
) -> Option<(ClassId, &'s Class, (&'s str, &'s str))> {
    use cratonvm_reader::instruction::Instruction;
    let (caller_id, caller_name, caller_descriptor, bci) = standin_call_site(frame)?;
    let caller_class = store.get(caller_id)?;
    let caller = caller_class.find_method(&caller_name, &caller_descriptor)?;
    let code = caller.code()?;
    let Instruction::Invokestatic(cp_index) = Instruction::decode(&code.code, bci).ok()?.0 else {
        return None;
    };
    let (owner, name, descriptor) = cp_method_ref(&caller_class.constant_pool, cp_index)?;
    if owner != CALLBACK_NATIVE_CLASS
        || !CALLBACK_NATIVE_ENTRIES
            .iter()
            .any(|&(n, d)| n == name && d == descriptor)
    {
        return None;
    }
    let class_id = if owner == &*caller_class.name {
        caller_id
    } else {
        resolve_class(caller_id, owner)?
    };
    let class = store.get(class_id)?;
    (&*class.name == CALLBACK_NATIVE_CLASS).then_some((class_id, class, (name, descriptor)))
}

/// The frames of the census method `callee` (a `(name, descriptor)` of
/// `class`, `java.lang.Class`, id `class_id`), OUTERMOST-first: each Java hop
/// at its ONE `invokestatic` of a `java/lang/Class.forName*` method, then the
/// native leaf. `None` when a hop has no such call or two, when a hop is
/// missing, or when the chain ends at any native but [`CALLBACK_NATIVE_LEAF`].
pub(in crate::runtime::stackwalker) fn callback_native_chain<'s>(
    class_id: ClassId,
    class: &'s Class,
    callee: (&'s str, &'s str),
) -> Option<Vec<BacktraceFrame>> {
    use cratonvm_reader::instruction::Instruction;
    let (mut name, mut descriptor) = callee;
    let mut out = Vec::new();
    for _ in 0..CALLBACK_CHAIN_MAX {
        let (index, method) = class
            .methods
            .iter()
            .enumerate()
            .find(|(_, m)| &*m.name == name && &*m.descriptor == descriptor)?;
        if method.is_native() {
            if name != CALLBACK_NATIVE_LEAF.0 || descriptor != CALLBACK_NATIVE_LEAF.1 {
                return None;
            }
            out.push(BacktraceFrame::Entry(standin_entry(
                class, class_id, method, index, None,
            )));
            return Some(out);
        }
        let code = method.code()?;
        let bytes: &[u8] = &code.code;
        let mut next: Option<(usize, &'s str, &'s str)> = None;
        let mut pc = 0usize;
        while pc < bytes.len() {
            let (instruction, after) = Instruction::decode(bytes, pc).ok()?;
            if let Instruction::Invokestatic(cp_index) = instruction {
                if let Some((owner, n, d)) = cp_method_ref(&class.constant_pool, cp_index) {
                    if owner == CALLBACK_NATIVE_CLASS && n.starts_with("forName") {
                        if next.is_some() {
                            return None;
                        }
                        next = Some((pc, n, d));
                    }
                }
            }
            if after <= pc {
                return None;
            }
            pc = after;
        }
        let (call_pc, next_name, next_descriptor) = next?;
        out.push(BacktraceFrame::Entry(standin_entry(
            class,
            class_id,
            method,
            index,
            Some(call_pc),
        )));
        name = next_name;
        descriptor = next_descriptor;
    }
    None
}

/// Lock-free pre-screen of one caller/callee pair (adjacent frames of an
/// OUTERMOST-first capture) for [`callback_native_middle_frames`]: `false`
/// when the pair certainly gets no frames. The callee must be a `loadClass`
/// or `<clinit>` activation (round 15 wave 5, lane trace5: or a `findClass`
/// one), an interpreted caller must stand at an `invokestatic`.
pub(crate) fn may_need_callback_native_middle_frames(
    caller: &BacktraceFrame,
    callee: &BacktraceFrame,
) -> bool {
    backtrace_frame_identity(callee).is_some_and(|(_, name, _, _)| {
        name == "loadClass" || name == "<clinit>" || name == FIND_CLASS_NAME
    }) && (!certainly_not_at_invokestatic(caller)
        || (backtrace_frame_identity(callee).is_some_and(|(_, name, _, _)| name == "<clinit>")
            && !certainly_not_at_instance_invoke(caller)))
}

/// `true` when `frame`, interpreted, certainly does not stand at an
/// `invokevirtual` / `invokespecial` (an [`INITIALIZING_NATIVES`] call).
pub(in crate::runtime::stackwalker) fn certainly_not_at_instance_invoke(frame: &BacktraceFrame) -> bool {
    match frame {
        BacktraceFrame::Method { method, bci, .. } => !matches!(
            usize::try_from(*bci)
                .ok()
                .and_then(|bci| method.code.get(bci).copied()),
            Some(0xb6 | 0xb7)
        ),
        _ => false,
    }
}

/// `jdk.internal.misc.Unsafe` natives that run a class's `<clinit>` on the
/// calling thread: HotSpot lists the native's `(Native Method)` frame between
/// the Java caller standing at the call and the `<clinit>` activation
/// (`Unsafe.ensureClassInitialized` -> `ensureClassInitialized0`, under
/// `MethodHandleAccessorFactory.ensureClassInitialized` for `Method.invoke` /
/// `Constructor.newInstance` of a not-yet-initialized class,
/// `Lookup.ensureInitialized`, the `DirectMethodHandle` barrier; and
/// `allocateInstance`). Interpreter round i5 wave 100, lane L3
/// (`L3W100ClinitTriggerWalks` rows `Method.invoke` / `Constructor.newInstance`:
/// the walk listed `Unsafe::ensureClassInitialized` with no native frame above it).
pub(in crate::runtime::stackwalker) const INITIALIZING_NATIVES: [(&str, &str, &str); 2] = [
    (
        "jdk/internal/misc/Unsafe",
        "ensureClassInitialized0",
        "(Ljava/lang/Class;)V",
    ),
    (
        "jdk/internal/misc/Unsafe",
        "allocateInstance",
        "(Ljava/lang/Class;)Ljava/lang/Object;",
    ),
];

/// The `(Native Method)` frame of the [`INITIALIZING_NATIVES`] call `caller`
/// stands at, when `callee` is the `<clinit>` that native ran; `None`
/// otherwise. Read from the real class bytes: the native must be declared
/// `native` there.
pub(in crate::runtime::stackwalker) fn initializing_native_middle_frame(
    store: &ClassStore,
    caller: &BacktraceFrame,
    callee: &BacktraceFrame,
    resolve_class: &mut dyn FnMut(ClassId, &str) -> Option<ClassId>,
) -> Option<Vec<BacktraceFrame>> {
    use cratonvm_reader::instruction::Instruction;
    if backtrace_frame_identity(callee)?.1 != "<clinit>" {
        return None;
    }
    let (caller_id, caller_name, caller_descriptor, bci) = standin_call_site(caller)?;
    let caller_class = store.get(caller_id)?;
    let caller_method = caller_class.find_method(&caller_name, &caller_descriptor)?;
    let code = caller_method.code()?;
    let cp_index = match Instruction::decode(&code.code, bci).ok()?.0 {
        Instruction::Invokevirtual(index) | Instruction::Invokespecial(index) => index,
        _ => return None,
    };
    let (owner, name, descriptor) = cp_method_ref(&caller_class.constant_pool, cp_index)?;
    if !INITIALIZING_NATIVES
        .iter()
        .any(|&(o, n, d)| o == owner && n == name && d == descriptor)
    {
        return None;
    }
    let class_id = if owner == &*caller_class.name {
        caller_id
    } else {
        resolve_class(caller_id, owner)?
    };
    let class = store.get(class_id)?;
    let (index, method) = class
        .methods
        .iter()
        .enumerate()
        .find(|(_, m)| &*m.name == name && &*m.descriptor == descriptor)?;
    if !method.is_native() {
        return None;
    }
    Some(vec![BacktraceFrame::Entry(standin_entry(
        class, class_id, method, index, None,
    ))])
}

/// The frames HotSpot shows between `caller` and `callee` (adjacent frames of
/// an OUTERMOST-first capture) when `caller` stands at a census call that ran
/// `callee`, OUTERMOST-first, or `None`. See the section comment above.
/// `resolve_class(requester, name)` resolves a class name as the requester's
/// loader would.
pub(crate) fn callback_native_middle_frames(
    store: &ClassStore,
    caller: &BacktraceFrame,
    callee: &BacktraceFrame,
    resolve_class: &mut dyn FnMut(ClassId, &str) -> Option<ClassId>,
) -> Option<Vec<BacktraceFrame>> {
    if !may_need_callback_native_middle_frames(caller, callee) {
        return None;
    }
    // Interpreter round i5 wave 100, lane L3: an `Unsafe` initializing
    // native's own frame under the `<clinit>` it ran.
    if let Some(frames) = initializing_native_middle_frame(store, caller, callee, resolve_class) {
        return callback_native_frames_enabled().then_some(frames);
    }
    let (class_id, class, call) = callback_native_call_at(store, caller, resolve_class)?;
    let mut frames = callback_native_chain(class_id, class, call)?;
    // Round 15 wave 5 (lane trace5): a `findClass` the served base `loadClass`
    // ran under the native (HotSpot's `forName0` calls `loadClass(String)`):
    // the two `ClassLoader.loadClass` frames go above the chain, or nothing
    // does (the served-`findClass` section).
    if backtrace_frame_identity(callee).is_some_and(|(_, name, _, _)| name == FIND_CLASS_NAME) {
        if !is_find_class_frame(callee) {
            return None;
        }
        let finder = backtrace_frame_identity(callee)?.3?;
        frames.extend(served_find_class_loader_frames(store, &[finder], true)?);
        if !served_find_class_frames_enabled() {
            return None;
        }
    } else if served_load_class_callee(callee) == Some(true) {
        // An override of `loadClass(String, boolean)` the served
        // `loadClass(String)` dispatched to: `ClassLoader.loadClass(String)`
        // above the chain when it can be named, else the chain alone (the
        // answer before).
        let hop = backtrace_frame_identity(callee)
            .and_then(|(_, _, _, finder)| finder)
            .filter(|&finder| {
                store
                    .get(finder)
                    .is_some_and(|class| &*class.name != LOADER_BASE_CLASS)
            })
            .and_then(|finder| served_one_arg_load_class_frame(store, &[finder]));
        if let Some(frame) = hop.filter(|_| served_find_class_frames_enabled()) {
            frames.push(frame);
        }
    }
    callback_native_frames_enabled().then_some(frames)
}

/// The frames of the census call the INNERMOST captured frame stands at, for
/// a throwable of class `throwable_class` the native raised itself,
/// OUTERMOST-first (append them), or `None`. See the section comment above.
pub(crate) fn callback_native_leaf_frames(
    store: &ClassStore,
    innermost: &BacktraceFrame,
    throwable_class: &str,
    resolve_class: &mut dyn FnMut(ClassId, &str) -> Option<ClassId>,
) -> Option<Vec<BacktraceFrame>> {
    if throwable_class == "java/lang/StackOverflowError" || certainly_not_at_invokestatic(innermost)
    {
        return None;
    }
    let (class_id, class, call) = callback_native_call_at(store, innermost, resolve_class)?;
    let frames = callback_native_chain(class_id, class, call)?;
    callback_native_frames_enabled().then_some(frames)
}

// ---------------------------------------------------------------------------
// A served built-in loader's miss: the `loadClass` frames it never pushed
// ---------------------------------------------------------------------------
//
// Round 15 wave 2 (lane trace2;
// `r15w1-trace-served-app-loader-cnfe-has-no-loader-frames-FIXED-20260930.md`). The
// base `ClassLoader.loadClass(String)` is a registered native
// (`classloader_real.rs` `cl_real_load_class`), so the application and
// platform loaders search in Rust and raise the `ClassNotFoundException`
// themselves: no loader frame was ever pushed, whether the call came from
// user code's `loader.loadClass(name)`, from `Class.forName0` or from the VM.
// HotSpot 25 (`R15TraceAppLoaderFrames`, both rows):
//
// ```text
// java.base/jdk.internal.loader.BuiltinClassLoader.loadClass(BuiltinClassLoader.java:580)
// java.base/java.lang.ClassLoader.loadClass(ClassLoader.java:490)
// <caller>
// ```
//
// JDK 25's `ClassLoaders$AppClassLoader` no longer overrides
// `loadClass(String, boolean)` (JDK 17 / 21 did, for the security manager's
// package check, and call `super.loadClass`): the chain is found by virtual
// dispatch, not written down. The native tells the capture which loader missed
// (`JvmThread::served_loader_miss_hint`, set right before it builds the
// exception), and [`served_loader_miss_frames`] rebuilds, from the real class
// bytes: `ClassLoader.loadClass(String)` at its ONE virtual call of
// `loadClass(String, boolean)`; then that call dispatched from the loader's
// class, each overriding hop at its ONE `super.loadClass(String, boolean)`,
// down to the hop with ONE `new ClassNotFoundException` (`<init>` call) and no
// such super call. Any other shape answers nothing (fail-closed).
// `CRATONVM_THROWABLE_SERVED_LOADER_FRAMES=0` turns it off.

pub(in crate::runtime::stackwalker) const LOADER_BASE_CLASS: &str = "java/lang/ClassLoader";
pub(in crate::runtime::stackwalker) const LOAD_CLASS_NAME: &str = "loadClass";
pub(in crate::runtime::stackwalker) const LOAD_CLASS_ONE_ARG: &str = "(Ljava/lang/String;)Ljava/lang/Class;";
pub(in crate::runtime::stackwalker) const LOAD_CLASS_RESOLVE: &str = "(Ljava/lang/String;Z)Ljava/lang/Class;";
pub(in crate::runtime::stackwalker) const CLASS_NOT_FOUND_EXCEPTION: &str = "java/lang/ClassNotFoundException";

/// The most `loadClass(String, boolean)` hops a chain takes (JDK 17 / 21:
/// `AppClassLoader` -> `BuiltinClassLoader`).
pub(in crate::runtime::stackwalker) const LOADER_MISS_CHAIN_MAX: usize = 4;

/// The class a virtual call of `(name, descriptor)` on an instance of `from`
/// selects -- `from` or its nearest superclass declaring the method -- with
/// the method and its slot.
pub(in crate::runtime::stackwalker) fn virtual_selected_method<'s>(
    store: &'s ClassStore,
    from: ClassId,
    name: &str,
    descriptor: &str,
) -> Option<(ClassId, &'s Class, usize, &'s ClassFileMethod)> {
    let mut id = from;
    for _ in 0..64 {
        let class = store.get(id)?;
        let found = class
            .methods
            .iter()
            .enumerate()
            .find(|(_, m)| &*m.name == name && &*m.descriptor == descriptor);
        if let Some((index, method)) = found {
            return Some((id, class, index, method));        }
        id = class.superclass?;
    }
    None
}

/// Where one `loadClass` hop of [`served_loader_miss_frames`] leaves its body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::runtime::stackwalker) enum LoaderMissSite {
    /// Its ONE call of `loadClass(String, boolean)`, at `pc`; `special` for an
    /// `invokespecial` (`super.loadClass`).
    Call { pc: usize, special: bool },
    /// Its ONE `ClassNotFoundException.<init>` call, at `pc`, with no call of
    /// `loadClass(String, boolean)`.
    Raise { pc: usize },
}

/// The [`LoaderMissSite`] of `code` (a method of `class`), or `None` for any
/// other shape: two calls, two raises, both, neither, or a decode failure.
pub(in crate::runtime::stackwalker) fn loader_miss_site(class: &Class, code: &[u8]) -> Option<LoaderMissSite> {
    use cratonvm_reader::instruction::Instruction;
    let mut call: Option<(usize, bool)> = None;
    let mut calls = 0usize;
    let mut raise: Option<usize> = None;
    let mut raises = 0usize;
    let mut pc = 0usize;
    while pc < code.len() {
        let (instruction, next) = Instruction::decode(code, pc).ok()?;
        let (cp_index, special) = match instruction {
            Instruction::Invokevirtual(cp_index) => (Some(cp_index), false),
            Instruction::Invokespecial(cp_index) => (Some(cp_index), true),
            _ => (None, false),
        };
        if let Some((owner, name, descriptor)) =
            cp_index.and_then(|cp_index| cp_method_ref(&class.constant_pool, cp_index))
        {
            if name == LOAD_CLASS_NAME && descriptor == LOAD_CLASS_RESOLVE {
                call = Some((pc, special));
                calls += 1;
            } else if special && owner == CLASS_NOT_FOUND_EXCEPTION && name == "<init>" {
                raise = Some(pc);
                raises += 1;
            }
        }
        if next <= pc {
            return None;
        }
        pc = next;
    }
    match (calls, raises) {
        (1, 0) => call.map(|(pc, special)| LoaderMissSite::Call { pc, special }),
        (0, 1) => raise.map(|pc| LoaderMissSite::Raise { pc }),
        _ => None,
    }
}

/// The JDK frames of a `ClassNotFoundException` a served built-in loader's
/// `loadClass(String)` raised for an instance of `loader_class`
/// (`JvmThread::served_loader_miss_hint`), OUTERMOST-first (append them), or
/// `None`. See the section comment above. `throwable_class` must be exactly
/// `java/lang/ClassNotFoundException` (the class the native mints).
pub(crate) fn served_loader_miss_frames(
    store: &ClassStore,
    loader_class: ClassId,
    throwable_class: &str,
) -> Option<Vec<BacktraceFrame>> {
    if throwable_class != CLASS_NOT_FOUND_EXCEPTION {
        return None;
    }
    // The one-argument `loadClass` the call selected: the base class's own
    // (the native stands in for it only when nothing below overrides it).
    let (base_id, base, base_index, base_method) =
        virtual_selected_method(store, loader_class, LOAD_CLASS_NAME, LOAD_CLASS_ONE_ARG)?;
    if &*base.name != LOADER_BASE_CLASS {
        return None;
    }
    let base_code = base_method.code()?;
    let LoaderMissSite::Call { pc, special: false } = loader_miss_site(base, &base_code.code)?
    else {
        return None;
    };
    let mut out = vec![BacktraceFrame::Entry(standin_entry(
        base,
        base_id,
        base_method,
        base_index,
        Some(pc),
    ))];
    // `loadClass(name, false)` dispatched on the loader, then each
    // `super.loadClass` from the hop's direct superclass (JVMS 6.5
    // `invokespecial`, `ACC_SUPER`).
    let mut from = loader_class;
    for _ in 0..LOADER_MISS_CHAIN_MAX {
        let (id, class, index, method) =
            virtual_selected_method(store, from, LOAD_CLASS_NAME, LOAD_CLASS_RESOLVE)?;
        if method.is_native() {
            return None;
        }
        // The base `loadClass(String, boolean)` itself, which catches its
        // parent's miss and calls `findClass` (round 15 wave 3, lane trace3;
        // TR2W2-5): only a `URLClassLoader`'s own `findClass` answers.
        // Round 15 wave 5 (lane trace5): and only with no override hop above
        // it. A `URLClassLoader` subclass now leaves the hint too, and an
        // override of `loadClass(String, boolean)` that reaches the base by
        // `super.loadClass` ran as bytecode: its frame is already captured.
        if &*class.name == LOADER_BASE_CLASS {
            if out.len() != 1 {
                return None;
            }
            let base = (class, id, method, index);
            return url_loader_find_class_frames(store, loader_class, base, out);
        }
        let code = method.code()?;
        match loader_miss_site(class, &code.code)? {
            LoaderMissSite::Raise { pc } => {
                out.push(BacktraceFrame::Entry(standin_entry(
                    class,
                    id,
                    method,
                    index,
                    Some(pc),
                )));
                return true
                .then_some(out);
            }
            LoaderMissSite::Call { pc, special: true } => {
                out.push(BacktraceFrame::Entry(standin_entry(
                    class,
                    id,
                    method,
                    index,
                    Some(pc),
                )));
                from = class.superclass?;
            }
            // A virtual re-dispatch from inside an override: not a shape the
            // JDK's built-in loaders have.
            LoaderMissSite::Call { special: false, .. } => return None,
        }
    }
    None
}

// ---------------------------------------------------------------------------
// A `MethodHandle` door's `WrongMethodTypeException` (JIT round 16 wave 13,
// lane wmte)
// ---------------------------------------------------------------------------
//
// `invokeExact`, `invoke`, `invokeWithArguments` and `asType` are natives
// here, and they raise their `WrongMethodTypeException` themselves, so its
// trace started at the caller (`R16MhWmteFrames`: `WrongMethodTypeException |
// R16MhWmteFrames.main` for all three rows, both modes). HotSpot 25.0.3 raises
// each in JDK code (measured; module prefixes elided):
//
// ```text
// h.invokeExact((Object) "x")   Invokers.newWrongMethodTypeException(Invokers.java:522)
//                               Invokers.checkExactType(Invokers.java:531)
//                               <caller>
// (int) h.invoke("x")           MethodHandle.asTypeUncached(MethodHandle.java:915)
//                               MethodHandle.setAsTypeCache(MethodHandle.java:900)
//                               MethodHandle.asType(MethodHandle.java:873)
//                               Invokers.checkGenericType(Invokers.java:541)
//                               <caller>
// h.invokeWithArguments(1, 2)   the three asType frames, then
//                               MethodHandle.invokeWithArguments(MethodHandle.java:735)
//                               <caller>
// h.asType(t)                   the three asType frames, then <caller>
// ```
//
// (`LambdaForm` frames are hidden.) The door tells the capture which of the
// two it stands for (`JvmThread::mh_wmte_raise_hint`, set right before it
// builds the exception), and [`mh_wmte_raise_frames`] rebuilds them from the
// real class bytes: each at its first call of the next, the innermost at its
// `WrongMethodTypeException` constructor call (`throwable_init_sites`), with
// the line `Throwable.getStackTrace` reports there. The frames below `asType`
// follow the caller's own call instruction (the native door pushed none):
// `invoke` / `invokeExact` on a `MethodHandle` lists `checkGenericType`
// (an `asType` refusal under `invokeExact` is a generic invoker's),
// `invokeWithArguments` lists its own frame(s), anything else none. A core
// chain that cannot be named answers nothing (fail-closed); a frame below it
// that cannot be named is left out. `CRATONVM_MH_WMTE_JDK_FRAMES=0` (read by
// the door) leaves the caller on top, as before.

pub(in crate::runtime::stackwalker) const MH_WMTE_CLASS: &str = "java/lang/invoke/WrongMethodTypeException";
/// `java.lang.invoke.Invokers`, which the door's hint loads (not initializes)
/// so the capture, which only finds loaded classes, can name its frames.
pub(crate) const MH_WMTE_INVOKERS_CLASS: &str = "java/lang/invoke/Invokers";
pub(in crate::runtime::stackwalker) const MH_WMTE_HANDLE_CLASS: &str = "java/lang/invoke/MethodHandle";
pub(in crate::runtime::stackwalker) const MH_WMTE_AS_TYPE: (&str, &str) = (
    "asType",
    "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
);
const MH_WMTE_SET_AS_TYPE_CACHE: (&str, &str) = (
    "setAsTypeCache",
    "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
);
const MH_WMTE_AS_TYPE_UNCACHED: (&str, &str) = (
    "asTypeUncached",
    "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
);
const MH_WMTE_CHECK_EXACT_TYPE: (&str, &str) = (
    "checkExactType",
    "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodType;)V",
);
const MH_WMTE_NEW_WMTE: (&str, &str) = (
    "newWrongMethodTypeException",
    "(Ljava/lang/invoke/MethodType;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/WrongMethodTypeException;",
);
const MH_WMTE_CHECK_GENERIC_TYPE: (&str, &str) = (
    "checkGenericType",
    "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
);
const MH_WMTE_IWA_ARRAY: (&str, &str) = (
    "invokeWithArguments",
    "([Ljava/lang/Object;)Ljava/lang/Object;",
);
const MH_WMTE_IWA_LIST: (&str, &str) = (
    "invokeWithArguments",
    "(Ljava/util/List;)Ljava/lang/Object;",
);

/// `class.method`'s frame at its first call of `owner.callee`, or `None`.
pub(in crate::runtime::stackwalker) fn mh_wmte_frame_at_call(
    store: &ClassStore,
    class: &str,
    method: (&str, &str),
    owner: &str,
    callee: (&str, &str),
) -> Option<BacktraceFrame> {
    let (class_id, c, index, m) = named_method_in(store, class, method.0, method.1)?;
    let pc = first_call_to(&c.constant_pool, &m.code()?.code, owner, callee.0, callee.1)?;
    Some(BacktraceFrame::Entry(standin_entry(
        c,
        class_id,
        m,
        index,
        Some(pc),
    )))
}

/// `class.method`'s frame at its first `WrongMethodTypeException`
/// constructor call, or `None`.
pub(in crate::runtime::stackwalker) fn mh_wmte_frame_at_raise(
    store: &ClassStore,
    class: &str,
    method: (&str, &str),
) -> Option<BacktraceFrame> {
    let (class_id, c, index, m) = named_method_in(store, class, method.0, method.1)?;
    let pc = *throwable_init_sites(&c.constant_pool, &m.code()?.code, MH_WMTE_CLASS)?.first()?;
    Some(BacktraceFrame::Entry(standin_entry(
        c,
        class_id,
        m,
        index,
        Some(pc),
    )))
}

/// The JDK frames HotSpot lists below `asType` for the call `innermost`
/// stands at (OUTERMOST first; see the section comment): empty for any call
/// but a `MethodHandle` door's, or when one cannot be named.
pub(in crate::runtime::stackwalker) fn mh_wmte_as_type_caller_frames(
    store: &ClassStore,
    innermost: Option<&BacktraceFrame>,
) -> Vec<BacktraceFrame> {
    let Some((caller_id, caller_name, caller_descriptor, bci)) =
        innermost.and_then(standin_call_site)
    else {
        return Vec::new();
    };
    let Some(caller) = store.get(caller_id) else {
        return Vec::new();
    };
    let Some((callee, descriptor)) = caller
        .find_method(&caller_name, &caller_descriptor)
        .and_then(|method| method.code())
        .and_then(|code| invoke_target_at(&caller.constant_pool, &code.code, bci))
        .filter(|(owner, _, _)| *owner == MH_WMTE_HANDLE_CLASS)
        .map(|(_, name, descriptor)| (name, descriptor))
    else {
        return Vec::new();
    };
    let array = || {
        mh_wmte_frame_at_call(
            store,
            MH_WMTE_HANDLE_CLASS,
            MH_WMTE_IWA_ARRAY,
            MH_WMTE_HANDLE_CLASS,
            MH_WMTE_AS_TYPE,
        )
    };
    let frames = if callee == "invoke" || callee == "invokeExact" {
        mh_wmte_frame_at_call(
            store,
            MH_WMTE_INVOKERS_CLASS,
            MH_WMTE_CHECK_GENERIC_TYPE,
            MH_WMTE_HANDLE_CLASS,
            MH_WMTE_AS_TYPE,
        )
        .map(|frame| vec![frame])
    } else if (callee, descriptor) == MH_WMTE_IWA_ARRAY {
        array().map(|frame| vec![frame])
    } else if (callee, descriptor) == MH_WMTE_IWA_LIST {
        mh_wmte_frame_at_call(
            store,
            MH_WMTE_HANDLE_CLASS,
            MH_WMTE_IWA_LIST,
            MH_WMTE_HANDLE_CLASS,
            MH_WMTE_IWA_ARRAY,
        )
        .and_then(|list| Some(vec![list, array()?]))
    } else {
        None
    };
    frames.unwrap_or_default()
}

/// The JDK frames of a `WrongMethodTypeException` (`throwable_class`) a
/// `MethodHandle` door raised natively in place of `raise`'s JDK method
/// (`JvmThread::mh_wmte_raise_hint`), OUTERMOST-first (append them), or
/// `None`. `innermost` is the capture's innermost frame, the door's caller.
/// See the section comment above.
pub(crate) fn mh_wmte_raise_frames(
    store: &ClassStore,
    raise: cratonvm_native_api::MhWmteRaise,
    innermost: Option<&BacktraceFrame>,
    throwable_class: &str,
) -> Option<Vec<BacktraceFrame>> {
    use cratonvm_native_api::MhWmteRaise;
    if throwable_class != MH_WMTE_CLASS {
        return None;
    }
    match raise {
        MhWmteRaise::ExactType => Some(vec![
            mh_wmte_frame_at_call(
                store,
                MH_WMTE_INVOKERS_CLASS,
                MH_WMTE_CHECK_EXACT_TYPE,
                MH_WMTE_INVOKERS_CLASS,
                MH_WMTE_NEW_WMTE,
            )?,
            mh_wmte_frame_at_raise(store, MH_WMTE_INVOKERS_CLASS, MH_WMTE_NEW_WMTE)?,
        ]),
        MhWmteRaise::AsType => {
            let core = [
                mh_wmte_frame_at_call(
                    store,
                    MH_WMTE_HANDLE_CLASS,
                    MH_WMTE_AS_TYPE,
                    MH_WMTE_HANDLE_CLASS,
                    MH_WMTE_SET_AS_TYPE_CACHE,
                )?,
                mh_wmte_frame_at_call(
                    store,
                    MH_WMTE_HANDLE_CLASS,
                    MH_WMTE_SET_AS_TYPE_CACHE,
                    MH_WMTE_HANDLE_CLASS,
                    MH_WMTE_AS_TYPE_UNCACHED,
                )?,
                mh_wmte_frame_at_raise(store, MH_WMTE_HANDLE_CLASS, MH_WMTE_AS_TYPE_UNCACHED)?,
            ];
            let mut out = mh_wmte_as_type_caller_frames(store, innermost);
            out.extend(core);
            Some(out)
        }
    }
}

// Round 15 wave 3 (lane trace3; TR2W2-5 of `jit-r15w2-trace2-proposals-RETIRED-20261003.md`).
// A bare `java.net.URLClassLoader` inherits both `loadClass` methods, so
// HotSpot 25's miss of `new URLClassLoader(urls, parent).loadClass(name)` is:
//
// ```text
// java.base/java.net.URLClassLoader.findClass(URLClassLoader.java:...)
// java.base/java.lang.ClassLoader.loadClass(ClassLoader.java:...)   (its findClass call)
// java.base/java.lang.ClassLoader.loadClass(ClassLoader.java:490)
// <caller>
// ```
//
// The VM serves the base `loadClass` natively and, in `--compatible`, decides
// the miss in Rust (`classloader_real.rs`: the isolated arm, and the
// `URLClassLoader.findClass` native `ucl_real_find_class`), so none of those
// frames was pushed. The native sets the served-loader-miss hint for such a
// receiver too, and the chain above continues at the base
// `loadClass(String, boolean)`: that method at its ONE `findClass(String)`
// call, then `findClass` dispatched on the loader's class -- which must be
// `URLClassLoader`'s own, not an override the Rust never ran -- at its ONE
// `new ClassNotFoundException(String)` (the plain miss; the `(String,
// Throwable)` arm is the unreadable-resource one). Any other shape (a JDK whose
// `findClass` raises inside a `doPrivileged` action, two sites) answers nothing.
// `CRATONVM_THROWABLE_SERVED_URL_LOADER_FRAMES=0` turns this arm off (read once
// the chain was found); `CRATONVM_THROWABLE_SERVED_LOADER_FRAMES=0` all of it.

pub(in crate::runtime::stackwalker) const FIND_CLASS_NAME: &str = "findClass";
pub(in crate::runtime::stackwalker) const FIND_CLASS_DESCRIPTOR: &str = "(Ljava/lang/String;)Ljava/lang/Class;";
pub(in crate::runtime::stackwalker) const URL_CLASS_LOADER_CLASS: &str = "java/net/URLClassLoader";

/// The pc of the ONE `invoke*` in `code` that calls `owner.name descriptor`;
/// `None` for none, two, or a decode failure.
pub(in crate::runtime::stackwalker) fn only_call_to(
    cp: &cratonvm_reader::constant_pool::ConstantPool,
    code: &[u8],
    owner: &str,
    name: &str,
    descriptor: &str,
) -> Option<usize> {
    use cratonvm_reader::instruction::Instruction;
    let mut found = None;
    let mut pc = 0usize;
    while pc < code.len() {
        let (instruction, next) = Instruction::decode(code, pc).ok()?;
        let index = match instruction {
            Instruction::Invokevirtual(index)
            | Instruction::Invokespecial(index)
            | Instruction::Invokestatic(index)
            | Instruction::Invokeinterface { index, .. } => Some(index),
            _ => None,
        };
        if let Some(index) = index {
            if matches!(cp_method_ref(cp, index), Some((o, n, d)) if o == owner && n == name && d == descriptor)
            {
                if found.is_some() {
                    return None;
                }
                found = Some(pc);
            }
        }
        if next <= pc {
            return None;
        }
        pc = next;
    }
    found}

/// `out` (the frames down to `ClassLoader.loadClass(String)`) completed with
/// the base `loadClass(String, boolean)` hop `base` -- `(class, id, method,
/// slot)` -- at its `findClass` call and `URLClassLoader.findClass` at its plain
/// `ClassNotFoundException`, for a miss of a loader of class `loader_class`.
/// See the section comment above.
pub(in crate::runtime::stackwalker) fn url_loader_find_class_frames(
    store: &ClassStore,
    loader_class: ClassId,
    base: (&Class, ClassId, &ClassFileMethod, usize),
    mut out: Vec<BacktraceFrame>,
) -> Option<Vec<BacktraceFrame>> {
    let (base, base_id, base_method, base_index) = base;
    let base_code = base_method.code()?;
    let find_pc = only_call_to(
        &base.constant_pool,
        &base_code.code,
        LOADER_BASE_CLASS,
        FIND_CLASS_NAME,
        FIND_CLASS_DESCRIPTOR,
    )?;
    let (find_id, find_class, find_index, find_method) =
        virtual_selected_method(store, loader_class, FIND_CLASS_NAME, FIND_CLASS_DESCRIPTOR)?;
    // Round 15 wave 5 (lane trace5): or the base `ClassLoader.findClass` (its
    // one `throw new ClassNotFoundException(name)`), for a user loader with no
    // `findClass` of its own; the hint for such a loader is
    // `CRATONVM_THROWABLE_SERVED_LOADER_SUBCLASS_FRAMES`'s.
    let base_find = &*find_class.name == LOADER_BASE_CLASS;
    if (&*find_class.name != URL_CLASS_LOADER_CLASS && !base_find) || find_method.is_native() {
        return None;
    }
    let find_code = find_method.code()?;
    let raise_pc = only_call_to(
        &find_class.constant_pool,
        &find_code.code,
        CLASS_NOT_FOUND_EXCEPTION,
        "<init>",
        "(Ljava/lang/String;)V",
    )?;
    out.push(BacktraceFrame::Entry(standin_entry(
        base,
        base_id,
        base_method,
        base_index,
        Some(find_pc),
    )));
    out.push(BacktraceFrame::Entry(standin_entry(
        find_class,
        find_id,
        find_method,
        find_index,
        Some(raise_pc),
    )));
    Some(out)
}

/// Round 15 wave 5 (lane trace5): is the INNERMOST captured frame standing at
/// an `invokevirtual` / `invokespecial` / `invokestatic` of a `findClass`
/// method? Then a served-loader-miss hint was left by a `findClass` native
/// that Java code called directly (a `URLClassLoader` subclass calling the
/// protected `findClass` from its own method, Jasper's shape): HotSpot shows
/// `URLClassLoader.findClass` there and no `ClassLoader.loadClass` frame, so
/// [`served_loader_miss_frames`] must not answer. `false` when the call
/// cannot be decoded.
pub(crate) fn innermost_calls_find_class(store: &ClassStore, innermost: &BacktraceFrame) -> bool {
    let Some((caller_id, caller_name, caller_descriptor, bci)) = standin_call_site(innermost)
    else {
        return false;
    };
    let Some(caller_class) = store.get(caller_id) else {
        return false;
    };
    caller_class
        .find_method(&caller_name, &caller_descriptor)
        .and_then(|method| method.code())
        .and_then(|code| invoke_target_at(&caller_class.constant_pool, &code.code, bci))
        .is_some_and(|(_, name, _)| name == FIND_CLASS_NAME)
}

/// Round 15 wave 6 (lane trace6; TR5W5-4 of `jit-r15w5-trace5-proposals-RETIRED-20261003.md`,
/// item 3 of `r15w5-trace5-review-findings-CLOSED-20260930.md`): the
/// `java.net.URLClassLoader.findClass` frame HotSpot shows on top of a
/// `ClassNotFoundException` the VM's `findClass` NATIVE raised for a call Java
/// code made itself. `--compatible` serves `URLClassLoader.findClass` natively
/// (`ucl_real_find_class`) for a subclass receiver's virtual call (Jasper's
/// `JasperLoader` calling `findClass(name)`) and for `super.findClass(name)`;
/// the native pushes no frame, so the capture's top frame was the caller.
///
/// `innermost` must stand at an `invokevirtual` / `invokespecial` of
/// `findClass(String)` (the screen [`innermost_calls_find_class`] also
/// applies), and the method that call selects (JVMS 6.5: from `loader_class`,
/// the receiver's class the native's hint names, for `invokevirtual`; from the
/// caller's direct superclass for `invokespecial`) must be `URLClassLoader`'s
/// own, with ONE plain `new ClassNotFoundException(String)`: the frame goes at
/// that raise (the `url_loader_find_class_frames` pc). `throwable_class` must
/// be exactly `ClassNotFoundException`, the class the native mints. Any other
/// shape answers `None` (the capture keeps the caller on top, as before).
pub(crate) fn url_find_class_native_frame(
    store: &ClassStore,
    innermost: &BacktraceFrame,
    loader_class: ClassId,
    throwable_class: &str,
) -> Option<BacktraceFrame> {
    use cratonvm_reader::instruction::Instruction;
    if throwable_class != CLASS_NOT_FOUND_EXCEPTION {
        return None;
    }
    let (caller_id, caller_name, caller_descriptor, bci) = standin_call_site(innermost)?;
    let caller_class = store.get(caller_id)?;
    let caller_method = caller_class.find_method(&caller_name, &caller_descriptor)?;
    let caller_code = caller_method.code()?;
    let (index, special) = match Instruction::decode(&caller_code.code, bci).ok()?.0 {
        Instruction::Invokevirtual(index) => (index, false),
        Instruction::Invokespecial(index) => (index, true),
        _ => return None,
    };
    let (_, name, descriptor) = cp_method_ref(&caller_class.constant_pool, index)?;
    if name != FIND_CLASS_NAME || descriptor != FIND_CLASS_DESCRIPTOR {
        return None;
    }
    let from = if special {
        caller_class.superclass?
    } else {
        loader_class
    };
    let (find_id, find_class, find_index, find_method) =
        virtual_selected_method(store, from, FIND_CLASS_NAME, FIND_CLASS_DESCRIPTOR)?;
    if &*find_class.name != URL_CLASS_LOADER_CLASS || find_method.is_native() {
        return None;
    }
    let find_code = find_method.code()?;
    let raise_pc = only_call_to(
        &find_class.constant_pool,
        &find_code.code,
        CLASS_NOT_FOUND_EXCEPTION,
        "<init>",
        "(Ljava/lang/String;)V",
    )?;
    Some(BacktraceFrame::Entry(standin_entry(
        find_class,
        find_id,
        find_method,
        find_index,
        Some(raise_pc),
    )))
}

// ---------------------------------------------------------------------------
// A loader's `findClass` run by the served base `loadClass` (round 15 wave 5)
// ---------------------------------------------------------------------------
//
// Lane trace5; `r15w3-trace3-url-loader-find-class-middle-frames-FIXED-20260930.md`.
// The base `ClassLoader.loadClass(String)` / `(String, boolean)` are served
// natively (`classloader_real.rs`), and after the parent's miss they call the
// loader's `findClass` through `invoke_virtual`: `--real-jdk`'s isolated
// `URLClassLoader` arm, and every loader whose `findClass` is its own
// (`URLClassLoader`'s, or a user override: Hibernate's
// `AggregatedClassLoader`, plugin loaders) in both modes. That `findClass`
// is bytecode and throws with its own frame, so the capture was
// `<loader>.findClass | <caller>`. HotSpot 25 has
//
// ```text
// <loader>.findClass(...)
// java.base/java.lang.ClassLoader.loadClass(ClassLoader.java:557)   (its ONE findClass call)
// java.base/java.lang.ClassLoader.loadClass(ClassLoader.java:490)   (a loadClass(String) call)
// <caller>
// ```
//
// The rule judges an adjacent pair: the callee a `findClass(String)`
// activation of class `C`, the caller at an `invokevirtual` / `invokespecial`
// of `loadClass(String)` or `loadClass(String, boolean)` naming class `O`. The
// receiver is an instance of both, so `loadClass` selected from `C` and from
// `O` must be `java.lang.ClassLoader`'s own (the served native): a bytecode
// override would have pushed its own frame between them. Then `loadClass
// (String)` at its ONE `loadClass(String, boolean)` call (one-argument call
// only) and `loadClass(String, boolean)` at its ONE `findClass` call. Any
// other shape answers nothing. A `Class.forName` the VM serves reaches the
// same `findClass` through `forName0`: [`callback_native_middle_frames`] puts
// these frames above its chain. The one blind spot is a VM-registered native
// override of `loadClass` on a subclass of `C` that is not `C` itself (the
// receiver is in no frame); no such registration calls `findClass`.
//
// The same rule takes an override of `loadClass(String, boolean)` as the
// callee (Spring's `OverridingClassLoader`, test-isolation loaders): the served
// `loadClass(String)` dispatches to it (`classloader_real.rs`
// `cl_real_load_class`), so HotSpot's `ClassLoader.loadClass(String)` at its
// `loadClass(name, false)` call goes between, for a one-argument call only.
// `CRATONVM_THROWABLE_SERVED_FIND_CLASS_FRAMES=0` turns both off (read once a
// pair was rebuilt).

/// `CRATONVM_THROWABLE_SERVED_FIND_CLASS_FRAMES` (default on).
pub(in crate::runtime::stackwalker) fn served_find_class_frames_enabled() -> bool {
    true
}

/// Is `frame` a `findClass(String)` activation? String compares only; an
/// entry without a descriptor is taken by its name.
pub(in crate::runtime::stackwalker) fn is_find_class_frame(frame: &BacktraceFrame) -> bool {
    backtrace_frame_identity(frame).is_some_and(|(_, name, descriptor, _)| {
        name == FIND_CLASS_NAME && descriptor.is_none_or(|d| d == FIND_CLASS_DESCRIPTOR)
    })
}

/// What the served base `loadClass` ran, as the callee of a pair: the
/// loader's `findClass` (`Some(false)`), or an override of `loadClass(String,
/// boolean)` that the served `loadClass(String)` dispatched to (`Some(true)`:
/// HotSpot shows `ClassLoader.loadClass(String)` below it). String compares
/// only.
pub(in crate::runtime::stackwalker) fn served_load_class_callee(callee: &BacktraceFrame) -> Option<bool> {
    let (_, name, descriptor, _) = backtrace_frame_identity(callee)?;
    match (name, descriptor) {
        (FIND_CLASS_NAME, None | Some(FIND_CLASS_DESCRIPTOR)) => Some(false),
        (LOAD_CLASS_NAME, Some(LOAD_CLASS_RESOLVE)) => Some(true),
        _ => None,
    }
}

/// Lock-free pre-screen of one caller/callee pair (adjacent frames of an
/// OUTERMOST-first capture) for [`served_load_class_middle_frames`]: the
/// callee is a `findClass(String)` or `loadClass(String, boolean)`
/// activation and an interpreted caller stands at an `invokevirtual` /
/// `invokespecial`.
pub(crate) fn may_need_served_load_class_middle_frames(
    caller: &BacktraceFrame,
    callee: &BacktraceFrame,
) -> bool {
    served_load_class_callee(callee).is_some() && may_stand_at_instance_call(caller)
}

/// `false` when `frame`, interpreted, certainly does not stand at an
/// `invokevirtual` / `invokespecial`.
pub(in crate::runtime::stackwalker) fn may_stand_at_instance_call(frame: &BacktraceFrame) -> bool {
    match frame {
        BacktraceFrame::Method { method, bci, .. } => usize::try_from(*bci)
            .ok()
            .and_then(|bci| method.code.get(bci).copied())
            .is_some_and(|op| matches!(op, 0xb6 | 0xb7)),
        _ => true,
    }
}

/// The capture's one pre-screen of a pair for both middle rules,
/// [`may_need_callback_native_middle_frames`] or
/// [`may_need_served_load_class_middle_frames`], naming the callee once (a
/// compiled frame's label is split per call).
pub(crate) fn may_need_loader_middle_frames(
    caller: &BacktraceFrame,
    callee: &BacktraceFrame,
) -> bool {
    let Some((_, name, descriptor, _)) = backtrace_frame_identity(callee) else {
        return false;
    };
    loader_middle_pair_may(caller, name, descriptor)}

/// [`may_need_loader_middle_frames`] on the callee's method name and
/// descriptor.
pub(in crate::runtime::stackwalker) fn loader_middle_pair_may(caller: &BacktraceFrame, name: &str, descriptor: Option<&str>) -> bool {
    match (name, descriptor) {
        (LOAD_CLASS_NAME, Some(LOAD_CLASS_RESOLVE))
        | (FIND_CLASS_NAME, None | Some(FIND_CLASS_DESCRIPTOR)) => {
            !certainly_not_at_invokestatic(caller) || may_stand_at_instance_call(caller)
        }
        // Interpreter round i5 wave 100, lane L3: a `<clinit>` an `Unsafe`
        // initializing native ran stands above an instance call
        // (`INITIALIZING_NATIVES`).
        ("<clinit>", _) => {
            !certainly_not_at_invokestatic(caller) || !certainly_not_at_instance_invoke(caller)
        }
        (LOAD_CLASS_NAME | FIND_CLASS_NAME, _) => !certainly_not_at_invokestatic(caller),
        _ => false,
    }
}

/// Interpreter round i3 wave 73, lane L2 (`CRATONVM_THROWABLE_LOADER_MISS_MEMO=1`;
/// `i73-L2-proposal-memoise-the-delegation-miss-capture-20261106.md`): does the
/// pair (adjacent frames of an OUTERMOST-first capture) certainly get no frames
/// from EITHER loader-middle rule -- [`callback_native_middle_frames`] and
/// [`served_load_class_middle_frames`] -- for a reason the two frames and the
/// caller's own class bytes decide, with no class resolved by name?
///
/// The lock-free screen [`may_need_loader_middle_frames`] answers `true` for
/// every `loadClass(String, boolean)` callee whose caller may stand at an
/// instance call. That is every frame of a real-JDK delegation:
/// `ClassLoader.loadClass(String)` at its `loadClass(name, false)`, and
/// `ClassLoader.loadClass(String, boolean)` at `parent.loadClass(name,
/// false)`, above `BuiltinClassLoader.loadClass`'s `throw new
/// ClassNotFoundException(cn)`. Once the screen engages, the capture memo is
/// refused and the rules run under the class-manager lock -- on every parent
/// miss, although both rules answer nothing there: the served rule's
/// `loadClass(String, boolean)` hop exists only below a ONE-argument call,
/// and the callback rule needs an `invokestatic` of a `Class.forName` entry.
///
/// Decoded the way both rules decode (`standin_call_site`, the caller class's
/// method, the instruction at the frame's bci, its constant-pool method ref):
/// * an `invokestatic`: the served rule answers nothing (no instance call);
///   the callback rule answers nothing unless the call names one of
///   [`CALLBACK_NATIVE_ENTRIES`] on `java.lang.Class`;
/// * an `invokevirtual` / `invokespecial`: the callback rule answers nothing;
///   the served rule answers nothing unless the callee is one of its callees
///   ([`served_load_class_callee`]) and the call names `loadClass(String)`, or
///   names `loadClass(String, boolean)` with a `findClass` callee;
/// * any other instruction: neither answers.
///
/// A caller with no call site (`standin_call_site`) gets nothing from either.
/// `false` (not decided) when the caller's class or method or code is not in
/// `store`. The answer depends on the frames and on the caller class's bytes
/// only, so a capture memo keyed on the frames and the redefinition count may
/// remember a trace these rules left alone.
pub(crate) fn loader_middle_pair_decided_quiet(
    store: &ClassStore,
    caller: &BacktraceFrame,
    callee: &BacktraceFrame,
) -> bool {
    use cratonvm_reader::instruction::Instruction;
    let Some((caller_id, caller_name, caller_descriptor, bci)) = standin_call_site(caller) else {
        return true;
    };
    let Some(caller_class) = store.get(caller_id) else {
        return false;
    };
    let Some(method) = caller_class.find_method(&caller_name, &caller_descriptor) else {
        return false;
    };
    let Some(code) = method.code() else {
        return false;
    };
    let Ok((instruction, _)) = Instruction::decode(&code.code, bci) else {
        return false;
    };
    match instruction {
        Instruction::Invokestatic(cp_index) => {
            let census = cp_method_ref(&caller_class.constant_pool, cp_index).is_some_and(
                |(owner, name, descriptor)| {
                    owner == CALLBACK_NATIVE_CLASS
                        && CALLBACK_NATIVE_ENTRIES
                            .iter()
                            .any(|&(n, d)| n == name && d == descriptor)
                },
            );
            !census
        }
        Instruction::Invokevirtual(cp_index) | Instruction::Invokespecial(cp_index) => {
            // Wave 100, lane L3: an `Unsafe` initializing native under a
            // `<clinit>` gets its frame (`initializing_native_middle_frame`).
            let initializing = backtrace_frame_identity(callee)
                .is_some_and(|(_, name, _, _)| name == "<clinit>")
                && cp_method_ref(&caller_class.constant_pool, cp_index).is_some_and(
                    |(owner, name, descriptor)| {
                        INITIALIZING_NATIVES
                            .iter()
                            .any(|&(o, n, d)| o == owner && n == name && d == descriptor)
                    },
                );
            if initializing {
                return false;
            }
            let Some(override_hop) = served_load_class_callee(callee) else {
                return true;
            };
            match cp_method_ref(&caller_class.constant_pool, cp_index) {
                Some((_, LOAD_CLASS_NAME, LOAD_CLASS_ONE_ARG)) => false,
                Some((_, LOAD_CLASS_NAME, LOAD_CLASS_RESOLVE)) => override_hop,
                _ => true,
            }
        }
        _ => true,
    }
}

/// Every pair of the OUTERMOST-first `trace` that the lock-free screen
/// [`may_need_loader_middle_frames`] lets through is
/// [`loader_middle_pair_decided_quiet`]: the loader-middle rules would insert
/// nothing into `trace`, whatever is loaded.
pub(crate) fn loader_middle_pairs_decided_quiet(
    store: &ClassStore,
    trace: &[BacktraceFrame],
) -> bool {
    trace.windows(2).all(|pair| {
        !may_need_loader_middle_frames(&pair[0], &pair[1])
            || loader_middle_pair_decided_quiet(store, &pair[0], &pair[1])
    })
}

/// `java.lang.ClassLoader`'s own `loadClass` of `descriptor`, when a virtual
/// call of it selects that one from every class of `from` (not empty); `None`
/// for an override on the way, a `native` base, or an empty `from`.
pub(in crate::runtime::stackwalker) fn base_load_class_selected<'s>(
    store: &'s ClassStore,
    from: &[ClassId],
    descriptor: &str,
) -> Option<(ClassId, &'s Class, usize, &'s ClassFileMethod)> {
    let mut selected = None;
    for &id in from {
        let found = virtual_selected_method(store, id, LOAD_CLASS_NAME, descriptor)?;
        if &*found.1.name != LOADER_BASE_CLASS || found.3.is_native() {
            return None;
        }
        selected = Some(found);
    }
    selected
}

/// `java.lang.ClassLoader.loadClass(String)` at its ONE `loadClass(String,
/// boolean)` call, when a virtual call selects it from every class of `from`.
pub(in crate::runtime::stackwalker) fn served_one_arg_load_class_frame(store: &ClassStore, from: &[ClassId]) -> Option<BacktraceFrame> {
    let (id, class, index, method) = base_load_class_selected(store, from, LOAD_CLASS_ONE_ARG)?;
    let code = method.code()?;
    let LoaderMissSite::Call { pc, special: false } = loader_miss_site(class, &code.code)? else {
        return None;
    };
    Some(BacktraceFrame::Entry(standin_entry(
        class,
        id,
        method,
        index,
        Some(pc),
    )))
}

/// The base `ClassLoader.loadClass` frames between a call of `loadClass`
/// (`one_arg`: the one-argument form) and the `findClass` the served native
/// ran, OUTERMOST-first, for a receiver that is an instance of every class in
/// `from`; `None` unless both methods are `java.lang.ClassLoader`'s own as
/// selected from each class of `from`, and each has its ONE call of the next.
/// See the section comment above.
pub(in crate::runtime::stackwalker) fn served_find_class_loader_frames(
    store: &ClassStore,
    from: &[ClassId],
    one_arg: bool,
) -> Option<Vec<BacktraceFrame>> {
    let mut out = Vec::with_capacity(2);
    if one_arg {
        out.push(served_one_arg_load_class_frame(store, from)?);
    }
    let (id, class, index, method) = base_load_class_selected(store, from, LOAD_CLASS_RESOLVE)?;
    let code = method.code()?;
    let find_pc = only_call_to(
        &class.constant_pool,
        &code.code,
        LOADER_BASE_CLASS,
        FIND_CLASS_NAME,
        FIND_CLASS_DESCRIPTOR,
    )?;
    out.push(BacktraceFrame::Entry(standin_entry(
        class,
        id,
        method,
        index,
        Some(find_pc),
    )));
    Some(out)
}

/// The frames HotSpot shows between `caller` and `callee` (adjacent frames of
/// an OUTERMOST-first capture) when `caller` called the served base
/// `loadClass` and `callee` is the `findClass` it ran, OUTERMOST-first, or
/// `None`. See the section comment above. `resolve_class(requester, name)`
/// resolves a class name as the requester's loader would.
pub(crate) fn served_load_class_middle_frames(
    store: &ClassStore,
    caller: &BacktraceFrame,
    callee: &BacktraceFrame,
    resolve_class: &mut dyn FnMut(ClassId, &str) -> Option<ClassId>,
) -> Option<Vec<BacktraceFrame>> {
    use cratonvm_reader::instruction::Instruction;
    if !may_need_served_load_class_middle_frames(caller, callee) {
        return None;
    }
    let override_hop = served_load_class_callee(callee)?;
    let finder = backtrace_frame_identity(callee)?.3?;
    let (caller_id, caller_name, caller_descriptor, bci) = standin_call_site(caller)?;
    let caller_class = store.get(caller_id)?;
    let caller_method = caller_class.find_method(&caller_name, &caller_descriptor)?;
    let code = caller_method.code()?;
    let cp_index = match Instruction::decode(&code.code, bci).ok()?.0 {
        Instruction::Invokevirtual(index) | Instruction::Invokespecial(index) => index,
        _ => return None,
    };
    let (owner, name, descriptor) = cp_method_ref(&caller_class.constant_pool, cp_index)?;
    let one_arg = match (name, descriptor) {
        (LOAD_CLASS_NAME, LOAD_CLASS_ONE_ARG) => true,
        (LOAD_CLASS_NAME, LOAD_CLASS_RESOLVE) => false,
        _ => return None,
    };
    let owner_id = if owner == &*caller_class.name {
        caller_id
    } else {
        resolve_class(caller_id, owner)?
    };
    let frames = if override_hop {
        // The override itself is `finder`'s: never the base (served) one.
        if !one_arg || &*store.get(finder)?.name == LOADER_BASE_CLASS {
            return None;
        }
        vec![served_one_arg_load_class_frame(store, &[owner_id, finder])?]
    } else {
        served_find_class_loader_frames(store, &[owner_id, finder], one_arg)?
    };
    served_find_class_frames_enabled().then_some(frames)
}

/// Round 14 wave 4 (lane trace3; T4-3 of `jit-r14-trace4-proposals-RETIRED-20260930.md`,
/// `r13w13-trace3-vm-served-jdk-methods-residuals` item 6): the JDK frames of
/// the census method a PARKED thread is blocked in, OUTERMOST-first, for its
/// published stack (`innermost` is that stack's innermost entry). HotSpot's
/// `Thread.getStackTrace()` of a thread sleeping in a registered `sleep` ends
/// `Thread.sleepNanos0(Native Method)`, `Thread.sleepNanos`, `Thread.sleep`,
/// where the published stack ended at the caller.
///
/// The caller proves the thread is inside the call: it is blocked
/// `WAITING` / `TIMED_WAITING` now, and a blocked thread's published stack is
/// the one its blocking native deposited, standing at the invoke of what it
/// blocks in. A published entry carries no descriptor
/// ([`capture_frames_no_lines`]): the method is the ONE method of that name
/// whose code at the entry's bci is a call of a census method (two answer
/// nothing). Virtual threads are the caller's to refuse.
pub(crate) fn parked_thread_standin_frames(
    store: &ClassStore,
    innermost: &StackTraceEntry,
    resolve_class: &mut dyn FnMut(ClassId, &str) -> Option<ClassId>,
) -> Option<Vec<StackTraceEntry>> {
    let caller_id = innermost.class_id?;
    let bci = usize::try_from(innermost.byte_code_index).ok()?;
    let caller_class = store.get(caller_id)?;
    let is_census_call = |(_, name, descriptor): &(&str, &str, &str)| {
        standin_rows(StandinFamily::Census)
            .iter()
            .any(|m| m.entry && m.name == *name && m.descriptor == *descriptor)
    };
    let mut candidates = Vec::new();
    for method in caller_class
        .methods
        .iter()
        .filter(|m| *m.name == *innermost.method_name)
        .filter(|m| {
            innermost
                .method_descriptor
                .as_deref()
                .is_none_or(|d| *m.descriptor == *d)
        })
    {
        let Some(code) = method.code() else {
            continue;
        };
        let Some(target) = invoke_target_at(&caller_class.constant_pool, &code.code, bci) else {
            continue;
        };
        if is_census_call(&target) {
            candidates.push(target);
        }
    }
    // JIT round 25 wave 6, lane W6-F fix 5: a published entry has no
    // descriptor, and two overloads may both call a census method at the
    // bci -- `Object.wait(J)V` calls `wait0` at 55 where JDK 25
    // `wait(JI)V` calls `wait(J)V` -- which answered nothing, so about two
    // in three `Object.wait` deposits listed no `wait0(native)`. The thread
    // is parked INSIDE a native, so the one candidate whose chain is that
    // native alone (a direct call of the leaf) is the overload it is in.
    let mut chains = candidates.into_iter().filter_map(|target| {
        standin_chain(
            store,
            caller_id,
            target,
            StandinScreen::Parked,
            &mut *resolve_class,
        )
    });
    let first = chains.next()?;
    let frames = match chains.next() {
        None => first,
        Some(second) => {
            let mut direct = std::iter::once(first)
                .chain(std::iter::once(second))
                .chain(chains)
                .filter(|frames| frames.len() == 1);
            let only = direct.next()?;
            if direct.next().is_some() {
                return None;
            }
            only
        }
    };
    Some(frames.iter().map(BacktraceFrame::to_entry).collect())
}

/// Round 15 wave 2 (lane trace2; TR5-5): may a (bound) virtual thread's
/// published stack take `frames` ([`parked_thread_standin_frames`],
/// OUTERMOST-first)? Not a chain through a `LockSupport` park: its body
/// branches on `Thread.isVirtual()`, which a `ThreadBuilders$BoundVirtualThread`
/// answers `true` (HotSpot `-XX:-VMContinuations` runs `VirtualThreads.park`
/// there), while the chain is its platform `U.park` call. The `sleep` / `wait`
/// / `join` bodies test `instanceof VirtualThread`, which a bound thread fails,
/// so their platform chains are what ran.
pub(crate) fn parked_frames_fit_virtual_thread(frames: &[StackTraceEntry]) -> bool {
    !frames
        .iter()
        .any(|e| &*e.class_name == STANDIN_LOCK_SUPPORT_CLASS)
}

/// Interpreter round i1 wave 47, lane L1
/// (`docs/feature-designs/real-jdk-mode.md`):
/// the JDK frames a debugger lists for a thread blocked in (or suspended at
/// the return of) a registered native standing in for the method the invoke
/// at `pc` of `code` calls -- `code` is a method of class `caller_id` --
/// OUTERMOST-first: each Java level at its call of the next, with its method
/// slot and bytecode index, down to the `native` leaf (bytecode index -1).
/// HotSpot 25.0.3 lists `Thread.sleepNanos0` (native), `Thread.sleepNanos`,
/// `Thread.sleep` above a caller of `Thread.sleep`, and `Unsafe.park`,
/// `LockSupport.parkNanos` above a caller of `LockSupport.parkNanos`
/// (`L1W45JdiSuspendedInNative`), where the stand-in ran none of them.
///
/// The census rows ([`STANDIN_METHODS`]) and the park family
/// ([`STANDIN_PARK_METHODS`]); the chain is found in the real class bytes,
/// as [`native_standin_frames`] finds a throwable's. `None` when the invoke
/// calls no such method, or the chain does not reach a native leaf. Whether
/// the thread is inside that method is the caller's to decide (the running
/// native's callback, or a blocked thread's top invoke).
#[cfg(feature = "experimental-debug")]
pub(crate) fn debugger_standin_frames(
    store: &ClassStore,
    caller_id: ClassId,
    code: &[u8],
    pc: usize,
    resolve_class: &mut dyn FnMut(ClassId, &str) -> Option<ClassId>,
) -> Option<Vec<StackTraceEntry>> {
    let screen = StandinScreen::Debugger;
    let caller_class = store.get(caller_id)?;
    let callee = invoke_target_at(&caller_class.constant_pool, code, pc)?;
    let (_, name, descriptor) = callee;
    if !screen
        .rows()
        .any(|m| m.entry && m.name == name && m.descriptor == descriptor)
    {
        return None;
    }
    let frames = standin_chain(store, caller_id, callee, screen, resolve_class)?;
    Some(frames.iter().map(BacktraceFrame::to_entry).collect())
}

/// Round 14 wave 4 (lane trace3): the lines of a PUBLISHED trace's entries
/// (`trace`, OUTERMOST-first) whose method is an overload set. A published
/// entry carries no descriptor ([`capture_frames_no_lines`]), so
/// [`resolve_line_numbers_in_place`] leaves an overloaded method at
/// `UNKNOWN`: every JDK `Object.wait` / `Thread.join` / `Thread.sleep` frame
/// of a parked thread printed `(Object.java)` where HotSpot prints
/// `(Object.java:389)`. Entry `i` stands at a call of entry `i + 1`'s method,
/// so its method is the ONE overload whose code at its bci is an invoke of a
/// method of that name; the entry gets that overload's descriptor, slot and
/// line. Two candidates (or none) leave it as it was: fail-closed like the
/// resolver it follows. The innermost entry has no callee (unless leaf frames
/// were appended after it). Answers how many entries were resolved.
pub(crate) fn resolve_published_overloads_by_callee(
    store: &ClassStore,
    trace: &mut [StackTraceEntry],
) -> usize {
    use cratonvm_reader::instruction::Instruction;
    let mut resolved = 0;
    for i in 1..trace.len() {
        let (head, tail) = trace.split_at_mut(i);
        let entry = &mut head[i - 1];
        let callee_name: &str = &tail[0].method_name;
        if entry.line_number != LINE_NUMBER_UNKNOWN || entry.method_descriptor.is_some() {
            continue;
        }
        let (Some(class_id), Ok(bci)) = (entry.class_id, usize::try_from(entry.byte_code_index))
        else {
            continue;
        };
        let Some(class) = store.get(class_id) else {
            continue;
        };
        let mut only: Option<(usize, &ClassFileMethod)> = None;
        let mut ambiguous = false;
        for (index, method) in class.methods.iter().enumerate() {
            if *method.name != *entry.method_name {
                continue;
            }
            let Some(code) = method.code() else {
                continue;
            };
            let cp_index = match Instruction::decode(&code.code, bci).map(|(i, _)| i) {
                Ok(
                    Instruction::Invokevirtual(cp_index)
                    | Instruction::Invokespecial(cp_index)
                    | Instruction::Invokestatic(cp_index)
                    | Instruction::Invokeinterface {
                        index: cp_index, ..
                    },
                ) => cp_index,
                _ => continue,
            };
            let calls_callee = cp_method_ref(&class.constant_pool, cp_index)
                .is_some_and(|(_, name, _)| name == callee_name);
            if !calls_callee {
                continue;
            }
            if only.is_some() {
                ambiguous = true;
                break;
            }
            only = Some((index, method));
        }
        let Some((index, method)) = only.filter(|_| !ambiguous) else {
            continue;
        };
        entry.method_descriptor = Some(Arc::clone(&method.descriptor));
        entry.method_index = u32::try_from(index).ok();
        if let Some(line) = line_number_for_bci_in_method(method, bci) {
            entry.line_number = line;
            resolved += 1;
        }
    }
    resolved
}

// Round 14 wave 3 (lane trace;
// `r13w13-trace3-vm-served-jdk-methods-residuals-FIXED-20260929.md` item 3). A
// started platform thread whose class does not override `run()` enters
// `Thread.run()`, which the VM serves (a registered, force-listed `Bridge`) by
// calling `task.run()` itself, so the task's `run` was the OUTERMOST captured
// frame. HotSpot 25 has `java.base/java.lang.Thread.run(Thread.java:1474)`
// below it (the `@Hidden runWith` between them is never shown). This rebuilds
// that one frame from the real class bytes, at the call of `runWith` (JDK 21+)
// or of `target.run()` (JDK 17).

pub(in crate::runtime::stackwalker) const THREAD_CLASS: &str = "java/lang/Thread";

/// The class of `frame` when it is a `run()V` activation of a class other
/// than `java.lang.Thread`: the only shape the `Thread.run` stand-in applies
/// to. String compares only (every capture on a platform thread asks).
pub(in crate::runtime::stackwalker) fn thread_run_task_frame_class(frame: &BacktraceFrame) -> Option<ClassId> {
    thread_run_task_class_of(backtrace_frame_identity(frame)?)
}

/// `(class name, method name, descriptor, class id)`: what
/// [`backtrace_frame_identity`] answers for a frame.
pub(in crate::runtime::stackwalker) type FrameIdentity<'a> = (&'a str, &'a str, Option<&'a str>, Option<ClassId>);

/// [`thread_run_task_frame_class`] on a frame's identity.
pub(in crate::runtime::stackwalker) fn thread_run_task_class_of(identity: FrameIdentity<'_>) -> Option<ClassId> {
    let (class_name, method_name, descriptor, class_id) = identity;
    // A published entry (`capture_frames_no_lines`, another thread's stack)
    // carries no descriptor. Taken as `()V` (round 14 wave 3, lane trace2):
    // the bottom rule then also proves that `run()V` dispatched on the task
    // lands on this class, and a frame the served `Thread.run` called is that
    // `run()V`. (The middle rule needs the CALLER's descriptor, so it never
    // answers for such a stack.)
    let descriptor = descriptor.unwrap_or("()V");
    if method_name != "run" || descriptor != "()V" || class_name == THREAD_CLASS {
        return None;
    }
    class_id
}

/// `(class name, method name, descriptor, class id)` of `frame`, string
/// splits only. The descriptor is `None` only for an entry built without one.
pub(in crate::runtime::stackwalker) fn backtrace_frame_identity(
    frame: &BacktraceFrame,
) -> Option<(&str, &str, Option<&str>, Option<ClassId>)> {
    Some(match frame {
        BacktraceFrame::Method {
            method, class_id, ..
        } => (
            &*method.class_name,
            &*method.method_name,
            Some(&*method.method_descriptor),
            Some(*class_id),
        ),
        BacktraceFrame::Compiled {
            label,
            owner_class_id,
            ..
        } => {
            let (owner_and_method, descriptor) = label.rsplit_once(':')?;
            let (class_name, method_name) = owner_and_method.rsplit_once('.')?;
            (
                class_name,
                method_name,
                Some(descriptor),
                Some(ClassId::new(*owner_class_id)),
            )
        }
        BacktraceFrame::Inlined {
            label, class_id, ..
        } => {
            let (owner_and_method, descriptor) = label.rsplit_once(':')?;
            let (class_name, method_name) = owner_and_method.rsplit_once('.')?;
            (
                class_name,
                method_name,
                Some(descriptor),
                (*class_id != 0).then(|| ClassId::new(*class_id)),
            )
        }
        BacktraceFrame::Entry(entry) => (
            &*entry.class_name,
            &*entry.method_name,
            entry.method_descriptor.as_deref(),
            entry.class_id,
        ),
    })
}

/// `(class, method, descriptor)` of a `"class/Name.method:descriptor"` label,
/// exactly as `label.rsplit_once(':')` then `.rsplit_once('.')` split it, by
/// two backward byte scans (JIT round 16 wave 9, lane throwpath, TP16-9; the
/// TP16-1 argument: `:` and `.` are ASCII, so the last such byte is the last
/// such char and a char boundary). No emptiness refusal, as there.
pub(in crate::runtime::stackwalker) fn split_label_identity(label: &str) -> Option<(&str, &str, &str)> {
    let (dot, colon) = label_split_positions(label.as_bytes())?;
    Some((&label[..dot], &label[dot + 1..colon], &label[colon + 1..]))
}

/// [`backtrace_frame_identity`], a compiled or inlined frame's label split by
/// [`split_label_identity`]. The same answer for every frame (TP16-9).
pub(in crate::runtime::stackwalker) fn backtrace_frame_identity_scanned(frame: &BacktraceFrame) -> Option<FrameIdentity<'_>> {
    match frame {
        BacktraceFrame::Compiled {
            label,
            owner_class_id,
            ..
        } => {
            let (class_name, method_name, descriptor) = split_label_identity(label)?;
            Some((
                class_name,
                method_name,
                Some(descriptor),
                Some(ClassId::new(*owner_class_id)),
            ))
        }
        BacktraceFrame::Inlined {
            label, class_id, ..
        } => {
            let (class_name, method_name, descriptor) = split_label_identity(label)?;
            Some((
                class_name,
                method_name,
                Some(descriptor),
                (*class_id != 0).then(|| ClassId::new(*class_id)),
            ))
        }
        other => backtrace_frame_identity(other),
    }
}

/// [`lambda_body_named`] behind a one-byte screen: both of its needles hold a
/// `$`, so a name without one answers `false` without building either
/// substring searcher (TP16-9).
pub(in crate::runtime::stackwalker) fn lambda_body_named_screened(name: &str) -> bool {
    name.as_bytes().contains(&b'$') && lambda_body_named(name)
}

/// [`label_may_be_hidden`] with its `/0x` test as a byte-window scan instead
/// of a substring searcher built per call (TP16-9). The same answer.
pub(in crate::runtime::stackwalker) fn label_may_be_hidden_scanned(label: &str) -> bool {
    label.starts_with("java/")
        || label.starts_with("jdk/")
        || label.starts_with("sun/")
        || memchr::memchr_iter(b'/', label.as_bytes())
            .any(|i| label.as_bytes().get(i + 1..i + 3) == Some(b"0x"))
}

/// [`backtrace_frame_hidden_decided`] with [`label_may_be_hidden_scanned`].
pub(in crate::runtime::stackwalker) fn backtrace_frame_hidden_decided_scanned(frame: &BacktraceFrame) -> Option<bool> {
    match frame {
        BacktraceFrame::Compiled { label, .. } => {
            (!label_may_be_hidden_scanned(label)).then_some(false)
        }
        BacktraceFrame::Inlined {
            hidden: Some(hidden),
            ..
        } => Some(*hidden),
        BacktraceFrame::Inlined {
            label, class_id, ..
        } => (*class_id == 0 || !label_may_be_hidden_scanned(label)).then_some(false),
        other => backtrace_frame_hidden_decided(other),
    }
}

/// JIT round 16 wave 9 (lane throwpath, TP16-9): the answers of the
/// throwable capture's three per-frame pre-screens, from ONE pass that names
/// each frame once.
///
/// After the walk, `vm_exec`'s `capture_throwable_stack_trace` screened the
/// trace three times, every frame each time: `trace.windows(2)` for the
/// middle `Thread.run` rule ([`may_need_thread_run_middle_frame`]: the
/// callee's label split twice by `rsplit_once(char)`, then two substring
/// searchers for `lambda$` / `$anonfun$`), again for the loader / callback
/// middle rules ([`may_need_loader_middle_frames`]: the label split again),
/// and once more in the `--real-jdk` hidden drop (a `/0x` substring searcher
/// per compiled label). Every pair whose caller is compiled passes the
/// caller half of the first screen, so a stack of N compiled frames paid
/// about six label scans and three searcher constructions per frame per
/// throw -- the per-frame term of `deep-new` (24 compiled frames, 6.4x
/// HotSpot), and the work wave 7's memo arm DOUBLED (it ran both pair screens
/// again to decide `memo_screens_quiet`): by this reading, the +40% `deep-new`
/// showed with the memo on.
///
/// Each field is EXACTLY its screen's answer on the same trace (unit tests
/// `r16_throwpath_capture_screens_tests` compare them): `thread_run_middle` =
/// `trace.windows(2).any(may_need_thread_run_middle_frame)`, `loader_middle`
/// = the same over [`may_need_loader_middle_frames`], `hidden_quiet` = every
/// frame's [`backtrace_frame_hidden_decided`] is `Some(false)`, `run_with` =
/// `trace.iter().any(is_thread_run_with_frame)`. A caller may skip a rule
/// whose screen answers `false` only while the trace is the one screened.
/// `CRATONVM_THROWABLE_SCREEN_ONCE`, default ON since the w9b A/B (deep-24
/// throw 2855 -> 2354 ns, deep-new 5944 -> 4555 ns); `=0` is the kill switch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct CaptureScreens {
    pub(crate) thread_run_middle: bool,
    pub(crate) loader_middle: bool,
    pub(crate) hidden_quiet: bool,
    pub(crate) run_with: bool,
}

/// [`CaptureScreens`] of an OUTERMOST-first trace.
pub(crate) fn capture_screens(trace: &[BacktraceFrame]) -> CaptureScreens {
    let mut screens = CaptureScreens {
        thread_run_middle: false,
        loader_middle: false,
        hidden_quiet: true,
        run_with: false,
    };
    let mut caller: Option<&BacktraceFrame> = None;
    for frame in trace {
        let identity = backtrace_frame_identity_scanned(frame);
        if screens.hidden_quiet && backtrace_frame_hidden_decided_scanned(frame) != Some(false) {
            screens.hidden_quiet = false;
        }
        if !screens.run_with {
            screens.run_with = identity.is_some_and(|(class_name, method_name, _, _)| {
                class_name == THREAD_CLASS && method_name == "runWith"
            });
        }
        if let (Some(caller), Some(identity)) = (caller, identity) {
            if !screens.thread_run_middle {
                screens.thread_run_middle = thread_run_middle_caller_may(caller)
                    && (thread_run_task_class_of(identity).is_some()
                        || lambda_body_named_screened(identity.1));
            }
            if !screens.loader_middle {
                screens.loader_middle = loader_middle_pair_may(caller, identity.1, identity.2);
            }
        }
        caller = Some(frame);
    }
    screens
}

/// What a compiled or inlined frame's one-pass screen answers depend on,
/// besides the pair rules' caller: the variant, the label's identity
/// (address and length of the shared string) and the class-id field
/// (TF17-1). `None` for an interpreter or built frame, which is screened
/// without a label scan anyway.
pub(in crate::runtime::stackwalker) fn screen_frame_key(frame: &BacktraceFrame) -> Option<(u8, usize, usize, u32)> {
    match frame {
        BacktraceFrame::Compiled {
            label,
            owner_class_id,
            ..
        } => Some((1, label.as_ptr() as usize, label.len(), *owner_class_id)),
        // Wave 123 (L3): the stamp (wave 122) decides the hidden answer, so
        // it is part of the key: a stamped and an unstamped level of one
        // label (a single-pass and an optimizing-tier splice) never share a
        // remembered answer. Unstamped keeps tag 2, as before.
        BacktraceFrame::Inlined {
            label,
            class_id,
            hidden,
            ..
        } => {
            let tag = match hidden {
                None => 2,
                Some(false) => 3,
                Some(true) => 4,
            };
            Some((tag, label.as_ptr() as usize, label.len(), *class_id))
        }
        _ => None,
    }
}

/// JIT round 17 wave 1 (lane throwfill, TF17-1;
/// `CRATONVM_THROWABLE_FUSED_CAPTURE=1`): [`capture_screens`], with a
/// compiled or inlined frame's own answers -- its identity
/// ([`backtrace_frame_identity_scanned`], two label scans) and its hidden
/// decision ([`backtrace_frame_hidden_decided_scanned`], a scan of the whole
/// label) -- remembered for the last two labels seen. A compiled recursion or
/// two methods calling each other repeat them down the whole stack. Exactly
/// `capture_screens`' answer: both are pure functions of the frame's variant,
/// label text, class-id field and (an inlined level's, wave 123) hidden
/// stamp ([`screen_frame_key`]); two frames of one
/// trace with the same label address hold the same `Arc`, so the same text.
/// The pair rules still read each frame's own caller.
pub(crate) fn capture_screens_memoised(trace: &[BacktraceFrame]) -> CaptureScreens {
    type Remembered<'t> = ((u8, usize, usize, u32), Option<FrameIdentity<'t>>, bool);
    let mut screens = CaptureScreens {
        thread_run_middle: false,
        loader_middle: false,
        hidden_quiet: true,
        run_with: false,
    };
    let mut remembered: [Option<Remembered<'_>>; 2] = [None, None];
    let mut next_way = 0usize;
    let mut caller: Option<&BacktraceFrame> = None;
    for frame in trace {
        let key = screen_frame_key(frame);
        let hit = key.and_then(|key| {
            remembered
                .iter()
                .flatten()
                .find(|(k, _, _)| *k == key)
                .map(|&(_, identity, visible)| (identity, visible))
        });
        let (identity, visible) = match hit {
            Some(hit) => hit,
            None => {
                let identity = backtrace_frame_identity_scanned(frame);
                let visible = backtrace_frame_hidden_decided_scanned(frame) == Some(false);
                if let Some(key) = key {
                    remembered[next_way] = Some((key, identity, visible));
                    next_way ^= 1;
                }
                (identity, visible)
            }
        };
        if screens.hidden_quiet && !visible {
            screens.hidden_quiet = false;
        }
        if !screens.run_with {
            screens.run_with = identity.is_some_and(|(class_name, method_name, _, _)| {
                class_name == THREAD_CLASS && method_name == "runWith"
            });
        }
        if let (Some(caller), Some(identity)) = (caller, identity) {
            if !screens.thread_run_middle {
                screens.thread_run_middle = thread_run_middle_caller_may(caller)
                    && (thread_run_task_class_of(identity).is_some()
                        || lambda_body_named_screened(identity.1));
            }
            if !screens.loader_middle {
                screens.loader_middle = loader_middle_pair_may(caller, identity.1, identity.2);
            }
        }
        caller = Some(frame);
    }
    screens
}

/// Is `frame` `java.lang.Thread.runWith` (JDK 21+, `@Hidden`, the call
/// `Thread.run` and `ThreadBuilders$BoundVirtualThread.run` make to run a
/// task)? String compares only. Round 15 wave 3 hand-back (lane trace4): the
/// `--compatible` throwable capture leaves it out, as HotSpot does
/// (`vm_exec::drop_mode_hidden_frames`).
pub(crate) fn is_thread_run_with_frame(frame: &BacktraceFrame) -> bool {
    backtrace_frame_identity(frame).is_some_and(|(class_name, method_name, _, _)| {
        class_name == THREAD_CLASS && method_name == "runWith"
    })
}

/// Is `frame` a `java.lang.Thread.run` frame (a served row's, or one a rule
/// built)? String compares only. Round 15 wave 5 (lane trace5): the
/// `STTRACE_DBG_THREADRUN` census (TR4W3-7) counts them.
pub(crate) fn is_thread_run_frame(frame: &BacktraceFrame) -> bool {
    backtrace_frame_identity(frame).is_some_and(|(class_name, method_name, _, _)| {
        class_name == THREAD_CLASS && method_name == "run"
    })
}

/// Lock-free pre-screen of the OUTERMOST captured frame for
/// [`thread_run_standin_frame`].
pub(crate) fn may_be_thread_run_task_frame(outermost: &BacktraceFrame) -> bool {
    thread_run_task_frame_class(outermost).is_some()
}

/// Lock-free pre-screen of the OUTERMOST captured frame for
/// [`thread_run_standin_frame_for_lambda`] (round 14 wave 3, lane trace2): any
/// frame not named `main` may be the implementation method of a LAMBDA task.
/// A lambda proxy here has no class in the store and pushes no frame:
/// `task.run()` on it enters the implementation method directly
/// (`invokedynamic`'s `LambdaCallSite`), so that method is the outermost
/// frame. The `main` refusal keeps the main thread's throws off the
/// class-manager lock.
///
/// Round 14 wave 4 (lane trace3): tightened, still lock-free. Every throw on
/// a non-main platform thread whose outermost frame is not a task's `run()V`
/// (a JNI-attached thread, a VM-driven `ClassLoader.loadClass` whose
/// delegation throws `ClassNotFoundException`, a `<clinit>`) paid the
/// class-manager read lock and two field walks here to learn it is not a
/// lambda task. A `Runnable` proxy's implementation method takes the SAM's
/// parameters (none) after the captured ones, so it is one of:
/// - a method reference's target, bound or static: no parameters, `()...`
///   (a constructor reference's `<init>()V` too);
/// - a synthesized lambda body with captured parameters, which every
///   compiler that spins `invokedynamic` lambdas names as such: javac / ecj
///   / Kotlin `...lambda$...`, Scala `$anonfun$...`.
///
/// `<clinit>` is never an implementation method. A descriptor-less entry is
/// kept. `CRATONVM_THROWABLE_THREAD_RUN_LAMBDA_SCREEN=0` restores the wave-3
/// screen (any frame not named `main`); it is read only for a frame the new
/// screen refuses and the old one took.
pub(crate) fn may_be_thread_run_lambda_frame(outermost: &BacktraceFrame) -> bool {
    let Some((_, name, descriptor, _)) = backtrace_frame_identity(outermost) else {
        return false;
    };
    if name == "main" || name == "<clinit>" {
        return false;
    }
    lambda_implementation_shaped(name, descriptor)
}

/// See [`may_be_thread_run_lambda_frame`]: could `name` / `descriptor` be the
/// implementation method of a `Runnable` lambda proxy?
pub(in crate::runtime::stackwalker) fn lambda_implementation_shaped(name: &str, descriptor: Option<&str>) -> bool {
    descriptor.is_none_or(|d| d.starts_with("()")) || lambda_body_named(name)
}

/// Is `name` a synthesized lambda body's (javac / ecj / Kotlin `lambda$...`,
/// Scala `$anonfun$...`)? String compares only.
pub(in crate::runtime::stackwalker) fn lambda_body_named(name: &str) -> bool {
    name.contains("lambda$") || name.contains("$anonfun$")
}

/// Lock-free refusal for [`thread_run_standin_frame`] (round 14 wave 4, lane
/// trace3): `outermost` is a `run()V` declared by the running thread's OWN
/// class (`thread_class`, the `Thread` object's), so that class overrides
/// `run` and HotSpot's bottom frame is that override itself -- a
/// `ForkJoinWorkerThread`, any `Thread` subclass with its own `run`. Exact:
/// both the task rule and the lambda rule refuse such a thread class under
/// the class-manager lock anyway.
pub(crate) fn outermost_is_threads_own_run(
    outermost: &BacktraceFrame,
    thread_class: ClassId,
) -> bool {
    backtrace_frame_identity(outermost).is_some_and(|(_, name, descriptor, class_id)| {
        name == "run" && descriptor.unwrap_or("()V") == "()V" && class_id == Some(thread_class)
    })
}

/// Round 14 wave 6 (lane trace5; TR5-2 of `jit-r14-trace5-proposals-RETIRED-20260930.md`):
/// one thread's last bottom `Thread.run` decision
/// (`vm_exec::prepend_thread_run_standin_frame`), keyed by what it depends
/// on. The task (`holder.task` is final on JDK 21+; JDK 17's `target` is
/// cleared only by `Thread.exit()`, after `run` returned) and the `Thread`
/// object are fixed for a `JvmThread`, so the answer is a function of the
/// thread's class, the outermost frame's method and the class versions.
pub(crate) struct ThreadRunBottomMemo {
    /// `cratonvm_classloading::class_redefinition_count()` when made.
    pub(in crate::runtime::stackwalker) redefinitions: u64,
    /// The `Thread` object's class.
    pub(in crate::runtime::stackwalker) thread_class: ClassId,
    /// The outermost frame's class id, method name and descriptor. A frame
    /// without a class id (an inlined level that recorded none) is never
    /// memoised: its name alone does not say which class it is.
    pub(in crate::runtime::stackwalker) frame: (ClassId, Arc<str>, Option<Arc<str>>),
    pub(in crate::runtime::stackwalker) answer: Option<BacktraceFrame>,
    /// How many copies of `answer` go below the outermost frame (round 15
    /// wave 3, lane trace3): one per served `Thread.run` a `Thread` handed
    /// to another `Thread` as its task ran (`vm_exec::thread_run_bottom_frame`).
    /// The task chain is fixed for the thread, as the task is.
    pub(in crate::runtime::stackwalker) copies: usize,
}

impl ThreadRunBottomMemo {
    pub(crate) fn new(
        redefinitions: u64,
        thread_class: ClassId,
        outermost: &BacktraceFrame,
        answer: Option<BacktraceFrame>,
    ) -> Option<Self> {
        let (_, name, descriptor, class_id) = backtrace_frame_identity(outermost)?;
        let class_id = class_id?;
        Some(Self {
            redefinitions,
            thread_class,
            frame: (class_id, Arc::from(name), descriptor.map(Arc::from)),
            answer,
            copies: 1,
        })
    }

    /// The memo with `copies` copies of its answer (see the field).
    pub(crate) fn with_copies(mut self, copies: usize) -> Self {
        self.copies = copies.max(1);
        self
    }

    /// How many copies of the answer go in (see the field).
    pub(crate) fn copies(&self) -> usize {
        self.copies
    }

    /// The memoised answer when the key matches (`Some(None)`: "no frame").
    pub(crate) fn lookup(
        &self,
        redefinitions: u64,
        thread_class: ClassId,
        outermost: &BacktraceFrame,
    ) -> Option<Option<&BacktraceFrame>> {
        let (_, name, descriptor, class_id) = backtrace_frame_identity(outermost)?;
        let hit = self.redefinitions == redefinitions
            && self.thread_class == thread_class
            && Some(self.frame.0) == class_id
            && &*self.frame.1 == name
            && self.frame.2.as_deref() == descriptor;
        hit.then_some(self.answer.as_ref())
    }
}

/// The `java.lang.Thread.run()` frame HotSpot shows below `outermost` when
/// the thread's task is a LAMBDA proxy (round 14 wave 3, lane trace2): the
/// proxy's `run` is not a frame here (and hidden on HotSpot), so `outermost`
/// is the proxy's implementation method `impl_owner.impl_name impl_desc` (a
/// `lambda$...` body or a method reference's target) -- or, for a virtual
/// method reference, an override of it in a subclass. `sam` is the proxy's
/// `(name, erased descriptor)`: only a `Runnable`-shaped `run()V` answers.
/// Then the thread-class and `Thread.run`-site rules of
/// [`thread_run_standin_frame`] apply. Same switch.
pub(crate) fn thread_run_standin_frame_for_lambda(
    store: &ClassStore,
    outermost: &BacktraceFrame,
    thread_class: ClassId,
    sam: (&str, &str),
    impl_method: (&str, &str, &str),
) -> Option<BacktraceFrame> {
    if sam != ("run", "()V") {
        return None;
    }
    let (impl_owner, impl_name, impl_desc) = impl_method;
    let (class_name, method_name, descriptor, class_id) = backtrace_frame_identity(outermost)?;
    if method_name != impl_name || descriptor.is_some_and(|d| d != impl_desc) {
        return None;
    }
    if class_name != impl_owner {
        // A virtual method reference dispatched to a subclass's override.
        let mut id = class_id?;
        let mut reaches = false;
        for _ in 0..64 {
            let Some(class) = store.get(id) else {
                break;
            };
            if &*class.name == impl_owner {
                reaches = true;
                break;
            }
            match class.superclass {
                Some(superclass) => id = superclass,
                None => break,
            }
        }
        if !reaches {
            return None;
        }
    }
    if false {
        return None;
    }
    let (thread_id, thread) = thread_class_inheriting_run(store, thread_class)?;
    thread_run_site_entry(thread, thread_id).map(BacktraceFrame::Entry)
}

/// Is `thread_class` (a `Thread` object's class) a virtual thread's
/// (`java.lang.VirtualThread`, `BaseVirtualThread` or below)? Round 14 wave 4
/// (lane trace3): another thread's published stack has no `ThreadKind`.
pub(crate) fn is_virtual_thread_class(store: &ClassStore, thread_class: ClassId) -> bool {
    let mut id = thread_class;
    for _ in 0..64 {
        let Some(class) = store.get(id) else {
            return false;
        };
        if matches!(
            &*class.name,
            "java/lang/VirtualThread" | "java/lang/BaseVirtualThread"
        ) {
            return true;
        }
        match class.superclass {
            Some(superclass) => id = superclass,
            None => return false,
        }
    }
    false
}

/// `java.lang.Thread` (id and class) when `thread_class` reaches it without
/// overriding `run()V` and is not a virtual thread's class; `None` otherwise.
pub(in crate::runtime::stackwalker) fn thread_class_inheriting_run(
    store: &ClassStore,
    thread_class: ClassId,
) -> Option<(ClassId, &Class)> {
    let mut id = thread_class;
    for _ in 0..64 {
        let class = store.get(id)?;
        if &*class.name == THREAD_CLASS {
            return Some((id, class));
        }
        // Round 14 wave 3 (lane trace2): a virtual thread runs its task
        // through `VirtualThread.run(Runnable)`, never `Thread.run()`. The
        // throwable capture screens `ThreadKind::Virtual` first; a walk of
        // ANOTHER thread's published stack has only the object's class.
        if matches!(
            &*class.name,
            "java/lang/VirtualThread" | "java/lang/BaseVirtualThread"
        ) {
            return None;
        }
        if class.find_method("run", "()V").is_some() {
            return None;
        }
        id = class.superclass?;
    }
    None
}

/// Round 15 wave 3 (lane trace3;
/// `r15w2-trace2-thread-run-middle-frame-lambda-and-thread-tasks-FIXED-20260930.md`
/// shape 2): does `run()V` on an object of `class_id` enter `java.lang.Thread`'s
/// own `run` -- a platform `Thread` (subclass) that does not override it? For
/// a task that is itself such a `Thread` (`new Thread(new Thread(task))`), the
/// served `Thread.run` runs the inner thread's served `Thread.run`, which runs
/// ITS task: HotSpot 25 shows one `Thread.run` frame per thread in that chain
/// (`vm_exec::thread_run_bottom_frame` walks it).
pub(crate) fn thread_task_enters_thread_run(store: &ClassStore, class_id: ClassId) -> bool {
    thread_class_inheriting_run(store, class_id).is_some()
}

/// The `java.lang.Thread.run()` frame HotSpot shows below `outermost`, or
/// `None`. `thread_class` is the running thread's `Thread` object's class and
/// `task_class` the class of the `Runnable` the VM's `Thread.run` called (the
/// same `target` / `holder.task` field it reads). Answers only when all hold:
/// `outermost` is `X.run()V` with `X` not `Thread`; `thread_class` inherits
/// `run()V` from `java.lang.Thread` (an override is itself the bottom frame, as
/// on HotSpot); `run()V` dispatched on `task_class` lands on `X`; and
/// `Thread.run` has bytecode with exactly ONE call of `runWith` or of
/// `run()V`. `CRATONVM_THROWABLE_THREAD_RUN_FRAME=0` turns it off.
pub(crate) fn thread_run_standin_frame(
    store: &ClassStore,
    outermost: &BacktraceFrame,
    thread_class: ClassId,
    task_class: ClassId,
) -> Option<BacktraceFrame> {
    let frame_class = thread_run_task_frame_class(outermost)?;
    if false {
        return None;
    }
    // The thread's class must reach `java.lang.Thread` without overriding `run`.
    let (thread_id, thread) = thread_class_inheriting_run(store, thread_class)?;
    // `task.run()` must dispatch to the frame's class: the first class up the
    // task's superclass chain that declares `run()V`, or an interface default
    // the task implements.
    let mut id = task_class;
    let mut declaring = None;
    for _ in 0..64 {
        let class = store.get(id)?;
        if class.find_method("run", "()V").is_some() {
            declaring = Some(id);
            break;
        }
        match class.superclass {
            Some(superclass) => id = superclass,
            None => break,
        }
    }
    match declaring {
        Some(declaring) if declaring == frame_class => {}
        Some(_) => return None,
        None => {
            let frame_type = store.get(frame_class)?;
            if frame_type.find_method("run", "()V").is_none()
                || !store.get(task_class)?.is_subclass_of(frame_class, store)
            {
                return None;
            }
        }
    }
    thread_run_site_entry(thread, thread_id).map(BacktraceFrame::Entry)
}

/// `java.lang.Thread.run()V` (class `thread`, id `thread_id`) as a frame
/// standing at its ONLY call of `runWith` (JDK 21+) or of `run()V` (JDK 17's
/// `target.run()`), with that call's line; `None` for a native `run`, or a body
/// with no such call or two.
pub(in crate::runtime::stackwalker) fn thread_run_site_entry(thread: &Class, thread_id: ClassId) -> Option<StackTraceEntry> {
    use cratonvm_reader::instruction::Instruction;
    let (index, method) = thread
        .methods
        .iter()
        .enumerate()
        .find(|(_, m)| &*m.name == "run" && &*m.descriptor == "()V")?;
    if method.is_native() {
        return None;
    }
    let code = method.code()?;
    let bytes: &[u8] = &code.code;
    let mut site = None;
    let mut pc = 0usize;
    while pc < bytes.len() {
        let (instruction, next) = Instruction::decode(bytes, pc).ok()?;
        let cp_index = match instruction {
            Instruction::Invokevirtual(cp_index)
            | Instruction::Invokespecial(cp_index)
            | Instruction::Invokestatic(cp_index)
            | Instruction::Invokeinterface {
                index: cp_index, ..            } => Some(cp_index),
            _ => None,
        };
        if let Some((_, name, descriptor)) =
            cp_index.and_then(|cp_index| cp_method_ref(&thread.constant_pool, cp_index))
        {
            if name == "runWith" || (name == "run" && descriptor == "()V") {
                if site.is_some() {
                    return None;
                }
                site = Some(pc);
            }
        }
        if next <= pc {
            return None;
        }
        pc = next;
    }
    Some(standin_entry(thread, thread_id, method, index, Some(site?)))}

// Round 14 wave 3 (lane trace2; T14W3-3 of `jit-r14-trace3-proposals-RETIRED-20260930.md`,
// `r13w13-trace3-vm-served-jdk-methods-residuals-FIXED-20260929.md` item 3's middle
// case). `class W extends Thread { public void run() { ...; super.run(); } }`
// (JBoss `JBossThread`, framework thread wrappers) calls the VM-served
// `Thread.run`, which calls `task.run()` itself: the capture showed
// `task.run | W.run` where HotSpot 25 shows `task.run | Thread.run | W.run`.
// Also a direct `new Thread(r).run()`. The frame is inserted between a caller
// standing at an invoke that resolves to `java.lang.Thread.run()V` and its
// callee, when that callee is a `run()V` of a class other than `Thread`:
//
// - `invokespecial` (`super.run()`) is exact: the call entered `Thread.run`.
// - `invokevirtual` may have dispatched to the receiver's override instead.
//   An override's class extends `Thread`; a task's class (almost never)
//   does, so a callee whose class extends `Thread` gets no frame -- the rare
//   miss is a `Thread` handed to another `Thread` as its task.
//
// A bytecode-served `Thread.run` never reaches it (its own frame is then the
// callee, which the `run()V`-not-`Thread` screen refuses).
//
// Round 15 wave 3 (lane trace3;
// `r15w2-trace2-thread-run-middle-frame-lambda-and-thread-tasks-FIXED-20260930.md`
// shape 1, TR2W2-3 of `jit-r15w2-trace2-proposals-RETIRED-20261003.md`): a LAMBDA task's callee
// is its implementation method (the proxy pushes no frame), so `super.run()` /
// `t.run()` with `new Thread(() -> ...)` showed `lambda$x$0 | W.run` where
// HotSpot shows `lambda$x$0 | Thread.run | W.run`. A callee named as a
// synthesized lambda body (`lambda$` / `$anonfun$`: [`lambda_body_named`]) is
// admitted too, with the same call-site rule. Such a callee is never an
// override of `run` (an override is named `run`), so the `invokevirtual`
// override refusal does not apply to it; the one shape it gets wrong is a
// VM-served (frameless) `run` override of a `Thread` subclass whose task is a
// lambda, where HotSpot names that override instead. A method-reference task
// (`new Thread(this::work)`) keeps no middle frame: widening the lock-free
// screen to any no-argument callee would take the class-manager lock on
// almost every capture (TR2W2-2, the recorded served call, is exact there).
// `CRATONVM_THROWABLE_THREAD_RUN_LAMBDA_MIDDLE_FRAME=0` turns the lambda arm
// off; read only once a lambda pair's call site resolved to `Thread.run`.

/// Is `frame` a synthesized lambda body's activation, the callee a lambda
/// task leaves under the served `Thread.run`? String compares only.
pub(in crate::runtime::stackwalker) fn thread_run_lambda_callee(frame: &BacktraceFrame) -> bool {
    backtrace_frame_identity(frame).is_some_and(|(_, name, _, _)| lambda_body_named(name))
}

/// Lock-free name screen of a callee entry for [`thread_run_middle_frame`]
/// (`vm_exec::add_thread_run_walk_entries`, before wrapping the entries): a
/// task's `run`, or a lambda task's body (round 15 wave 3, lane trace3).
pub(crate) fn may_be_thread_run_middle_callee_name(name: &str) -> bool {
    name == "run" || lambda_body_named(name)
}

/// Lock-free pre-screen of one caller/callee pair for
/// [`thread_run_middle_frame`]: `false` when the pair certainly gets no frame.
/// An interpreted caller must stand at an `invokevirtual` / `invokespecial`.
pub(crate) fn may_need_thread_run_middle_frame(
    caller: &BacktraceFrame,
    callee: &BacktraceFrame,
) -> bool {
    thread_run_middle_caller_may(caller)
        && (thread_run_task_frame_class(callee).is_some() || thread_run_lambda_callee(callee))
}

/// The caller half of [`may_need_thread_run_middle_frame`].
pub(in crate::runtime::stackwalker) fn thread_run_middle_caller_may(caller: &BacktraceFrame) -> bool {
    match caller {
        BacktraceFrame::Method { method, bci, .. } => usize::try_from(*bci)
            .ok()
            .and_then(|bci| method.code.get(bci).copied())
            .is_some_and(|op| op == 0xb6 || op == 0xb7),
        _ => true,
    }
}

/// Does `class_id`'s superclass chain reach `java.lang.Thread`?
pub(in crate::runtime::stackwalker) fn class_extends_thread(store: &ClassStore, class_id: ClassId) -> bool {
    let mut id = class_id;
    for _ in 0..64 {
        let Some(class) = store.get(id) else {
            return false;
        };
        if &*class.name == THREAD_CLASS {
            return true;
        }
        match class.superclass {
            Some(superclass) => id = superclass,
            None => return false,
        }
    }
    false
}

/// The frame whose call the middle rule decodes: `(class id, class, method,
/// bci)`. Round 15 wave 3 (lane trace3): an entry of ANOTHER thread's
/// published stack carries no descriptor (nor a method slot:
/// [`capture_frames_no_lines`]), so the middle rule answered nothing there --
/// a thread dump of a `super.run()` worker (`JBossThread`) lacked the
/// `Thread.run` HotSpot shows. Such an entry names the ONE method of its name
/// whose code at its bci is an `invokevirtual` / `invokespecial` of `run()V`
/// (two answer nothing, as [`resolve_published_overloads_by_callee`] does).
/// `CRATONVM_THREAD_STACK_THREAD_RUN_MIDDLE_FRAMES=0` restores the refusal;
/// read only for such an entry that found its method.
pub(in crate::runtime::stackwalker) fn thread_run_caller_method<'s>(
    store: &'s ClassStore,
    caller: &BacktraceFrame,
) -> Option<(ClassId, &'s Class, &'s ClassFileMethod, usize)> {
    use cratonvm_reader::instruction::Instruction;
    if let Some((caller_id, caller_name, caller_descriptor, bci)) = standin_call_site(caller) {
        let caller_class = store.get(caller_id)?;
        let method = caller_class.find_method(&caller_name, &caller_descriptor)?;
        return Some((caller_id, caller_class, method, bci));
    }
    let BacktraceFrame::Entry(entry) = caller else {
        return None;
    };
    if entry.method_descriptor.is_some() {
        return None;
    }
    let caller_id = entry.class_id?;
    let bci = usize::try_from(entry.byte_code_index).ok()?;
    let caller_class = store.get(caller_id)?;
    let mut only = None;
    for method in caller_class
        .methods
        .iter()
        .filter(|m| *m.name == *entry.method_name)
    {
        let Some(code) = method.code() else {
            continue;
        };
        let cp_index = match Instruction::decode(&code.code, bci).map(|(i, _)| i) {
            Ok(Instruction::Invokevirtual(cp_index) | Instruction::Invokespecial(cp_index)) => {
                cp_index
            }
            _ => continue,
        };
        let calls_run = cp_method_ref(&caller_class.constant_pool, cp_index)
            .is_some_and(|(_, name, descriptor)| name == "run" && descriptor == "()V");
        if !calls_run {
            continue;
        }
        if only.is_some() {
            return None;
        }
        only = Some(method);
    }
    let method = only?;
    true
        .then_some((caller_id, caller_class, method, bci))
}

/// The `java.lang.Thread.run()` frame HotSpot shows between `caller` and
/// `callee` (adjacent frames of an OUTERMOST-first capture), or `None`. See
/// the section comment above. `resolve_class(requester, name)` resolves a
/// class name as the requester's loader would.
/// `CRATONVM_THROWABLE_THREAD_RUN_MIDDLE_FRAME=0` turns it off.
pub(crate) fn thread_run_middle_frame(
    store: &ClassStore,
    caller: &BacktraceFrame,
    callee: &BacktraceFrame,
    resolve_class: &mut dyn FnMut(ClassId, &str) -> Option<ClassId>,
) -> Option<StackTraceEntry> {
    use cratonvm_reader::instruction::Instruction;
    // `None`: a lambda task's body (round 15 wave 3, lane trace3).
    let callee_class = thread_run_task_frame_class(callee);
    if callee_class.is_none() && !thread_run_lambda_callee(callee) {
        return None;
    }
    let (caller_id, caller_class, caller_method, bci) = thread_run_caller_method(store, caller)?;
    let caller_code = caller_method.code()?;
    let bytes: &[u8] = &caller_code.code;
    let (cp_index, virtual_call) = match Instruction::decode(bytes, bci).ok()?.0 {
        Instruction::Invokevirtual(cp_index) => (cp_index, true),
        Instruction::Invokespecial(cp_index) => (cp_index, false),
        _ => return None,
    };
    let (owner, name, descriptor) = cp_method_ref(&caller_class.constant_pool, cp_index)?;
    if name != "run" || descriptor != "()V" {
        return None;
    }
    if false
    {
        return None;
    }
    // The call resolves to the first `run()V` up the owner's superclass
    // chain: it must be `java.lang.Thread`'s own.
    let mut id = resolve_class(caller_id, owner)?;
    let mut thread = None;
    for _ in 0..64 {
        let class = store.get(id)?;
        if class.find_method("run", "()V").is_some() {
            if &*class.name == THREAD_CLASS {
                thread = Some((id, class));
            }
            break;
        }
        id = class.superclass?;
    }
    let (thread_id, thread) = thread?;
    match callee_class {
        Some(callee_class) => {
            if virtual_call && class_extends_thread(store, callee_class) {
                return None;
            }
        }
        None => {
            if false {
                return None;
            }
        }
    }
    thread_run_site_entry(thread, thread_id)
}

/// Does the pair certainly get no frame from [`thread_run_middle_frame`], for
/// a reason the two frames and the caller's own bytes decide, with no class
/// resolved by name?
///
/// The lock-free screen [`may_need_thread_run_middle_frame`] lets through
/// every lambda body (or `run()V`) callee whose caller is compiled or stands
/// at a `invokevirtual` / `invokespecial` -- every task frame of a JUnit or
/// `ForkJoinPool` stack. The rule needs the caller to stand at a call of
/// `run()V` (decoded here exactly as the rule decodes it); any other call
/// leaves the pair alone however the names resolve. A `run()V` call is not
/// decided (its owner is resolved by name), nor is a caller whose class or
/// method is not in `store`, nor a published entry without a descriptor.
pub(crate) fn thread_run_middle_pair_decided_quiet(
    store: &ClassStore,
    caller: &BacktraceFrame,
    callee: &BacktraceFrame,
) -> bool {
    use cratonvm_reader::instruction::Instruction;
    if thread_run_task_frame_class(callee).is_none() && !thread_run_lambda_callee(callee) {
        return true;
    }
    let Some((caller_id, caller_name, caller_descriptor, bci)) = standin_call_site(caller) else {
        return !matches!(caller, BacktraceFrame::Entry(e) if e.method_descriptor.is_none());
    };
    let Some(caller_class) = store.get(caller_id) else {
        return false;
    };
    let Some(method) = caller_class.find_method(&caller_name, &caller_descriptor) else {
        return false;
    };
    let Some(code) = method.code() else {
        return true;
    };
    let Ok((Instruction::Invokevirtual(cp_index) | Instruction::Invokespecial(cp_index), _)) =
        Instruction::decode(&code.code, bci)
    else {
        return true;
    };
    !cp_method_ref(&caller_class.constant_pool, cp_index)
        .is_some_and(|(_, name, descriptor)| name == "run" && descriptor == "()V")
}

/// Every pair of the OUTERMOST-first `trace` that [`may_need_thread_run_middle_frame`]
/// lets through is [`thread_run_middle_pair_decided_quiet`]: the rule would
/// insert nothing into `trace`, whatever is loaded.
pub(crate) fn thread_run_middle_pairs_decided_quiet(
    store: &ClassStore,
    trace: &[BacktraceFrame],
) -> bool {
    trace.windows(2).all(|pair| {
        !may_need_thread_run_middle_frame(&pair[0], &pair[1])
            || thread_run_middle_pair_decided_quiet(store, &pair[0], &pair[1])
    })
}

