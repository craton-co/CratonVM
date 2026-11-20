#[allow(unused_imports)]
use super::*;

/// The [`receiver_overrides_load_class`] memo's key parts for `descriptor`:
/// the context's hierarchy stamp and the overload index, or `None` (switch
/// off, no stamp, or an overload the memo does not keep).
pub fn load_class_override_memo_key(ctx: &dyn NativeContext, descriptor: &str) -> Option<(u64, u8)> {
    if !crate::vmflags().loader.load_class_override_memo {
        return None;
    }
    let overload = match descriptor {
        "(Ljava/lang/String;)Ljava/lang/Class;" => 0,
        "(Ljava/lang/String;Z)Ljava/lang/Class;" => 1,
        _ => return None,
    };
    Some((ctx.class_hierarchy_memo_stamp()?, overload))
}

/// The uncached walk behind [`receiver_overrides_load_class`].
pub fn receiver_overrides_load_class_walk(
    ctx: &mut dyn NativeContext,
    receiver: cratonvm_types::ClassId,
    descriptor: &str,
) -> bool {
    let mut cid = Some(receiver);
    let mut found_override = false;
    while let Some(id) = cid {
        let name = match ctx.class_name_of_id(id) {
            Some(n) => n,
            None => break,
        };
        // URLClassLoader-family loaders (notably Spring Boot's
        // `LaunchedURLClassLoader`) have their class resolution substituted by
        // CratonVM's classpath scanner — their `loadClass` bytecode depends on
        // `URLClassPath` / nested-JAR plumbing CratonVM does not run, and they
        // are handled by the existing `Class.forName` / base-delegation rescues.
        // Leave them on the base path: do NOT route them through their override.
        if name == "java/net/URLClassLoader" {
            // Plain URLClassLoader-family receivers stay on CratonVM's base
            // path, but a subclass override found below the URLClassLoader
            // superclass must still win. BeanShell's BshClassLoader extends
            // URLClassLoader and overrides loadClass(String, boolean); masking
            // that override makes ClassManagerImpl.classForName reuse a stale
            // globally-loaded MyMessenger instead of reaching findClass.
            return found_override;
        }
        if is_builtin_loader_class_in(&*ctx, &name) {
            // Reached the builtin base.
            break;
        }
        if !found_override
            && ctx
                .declared_methods(id)
                .iter()
                .any(|m| m.name == "loadClass" && m.descriptor == descriptor)
        {
            found_override = true;        }
        cid = ctx.superclass_of(id);
    }
    found_override}

/// See [`receiver_overrides_load_class`]. The one-argument public overload is
/// itself virtual and can be overridden independently of the protected
/// `(String, boolean)` form. `ModifiedClassPathClassLoader` does exactly that
/// to reject packages excluded by Spring Boot's `@ClassPathExclusions`.
pub fn receiver_overrides_load_class_single(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> bool {
    receiver_overrides_load_class(ctx, this, "(Ljava/lang/String;)Ljava/lang/Class;")
}

/// See [`receiver_overrides_load_class`].
pub fn receiver_overrides_load_class_resolve(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> bool {
    receiver_overrides_load_class(ctx, this, "(Ljava/lang/String;Z)Ljava/lang/Class;")
}

// A public `loadClass(String)` override commonly delegates with
// `super.loadClass(name)`. CratonVM serves that base JDK method natively, so
// the nested invokespecial reaches the same callback as the outer virtual
// call. Remember the active loader identity per native thread and let that
// nested call take base delegation; otherwise ModifiedClassPathClassLoader's
// `return super.loadClass(name)` recursively re-enters its own override.
//
// gc-common w29-e (`common-w28b-remaining-identity-hash-keyed-side-tables`,
// rank 20): each entry is the receiver's native PIN HANDLE and the receiver as
// pushed (the `read_native_pin` fallback), matched by the pin's CURRENT
// address -- the w26-a pattern (`native-collections`' `reentry_guard_enter`).
// The entries used to be identity hashes. A hash follows the loader across a
// move, but two LIVE loaders can share one (every heap mints it from one
// 32-bit counter that wraps), so a colliding loader's `loadClass` reached from
// inside the first one's override read as the nested `super.loadClass`
// delegation and skipped its own override (`ModifiedClassPathClassLoader`'s
// package exclusions, say). A pin names exactly one object, and the outer
// frame that pushed an entry -- and so its pin -- is alive whenever an inner
// frame looks.
thread_local! {
    pub static SINGLE_LOAD_CLASS_OVERRIDE_IN_FLIGHT: RefCell<Vec<(usize, ObjectRef)>> =
        const { RefCell::new(Vec::new()) };
}

/// Push `this` (CURRENT) onto [`SINGLE_LOAD_CLASS_OVERRIDE_IN_FLIGHT`], or
/// answer `None` when an entry already names it (the nested delegation). On
/// `Some` the caller owns the returned pin and MUST pass it to
/// [`single_load_class_guard_leave`] on every path.
pub fn single_load_class_guard_enter(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<usize> {
    let pin = ctx.pin_native_root(this);
    let reentered = SINGLE_LOAD_CLASS_OVERRIDE_IN_FLIGHT.with(|active| {
        let mut active = active.borrow_mut();
        let ctx: &dyn NativeContext = &*ctx;
        let held = active
            .iter()
            .any(|&(h, pushed)| ctx.read_native_pin(h, pushed) == this);
        if !held {
            active.push((pin, this));
        }
        held
    });
    if reentered {
        // The newest pin: releasing it releases nothing older.
        ctx.unpin_native_roots(pin);
        return None;
    }
    Some(pin)
}

/// Pop the entry [`single_load_class_guard_enter`] pushed under `pin` and
/// release the pin (the newest this frame holds: the guarded call's own pins
/// are balanced inside it).
pub fn single_load_class_guard_leave(ctx: &mut dyn NativeContext, pin: usize) {
    SINGLE_LOAD_CLASS_OVERRIDE_IN_FLIGHT.with(|active| {
        let mut active = active.borrow_mut();
        let found = active.iter().rposition(|&(h, _)| h == pin);
        debug_assert!(found.is_some(), "single loadClass guard entry missing");
        if let Some(i) = found {
            active.remove(i);
        }
    });
    ctx.unpin_native_roots(pin);
}

/// Invoke a genuine public `loadClass(String)` override once, or return
/// `None` when the receiver has no override or this is its nested
/// `super.loadClass(name)` delegation.
pub fn invoke_single_load_class_override(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    name_obj: ObjectRef,
) -> Option<MethodCallResult> {
    if !receiver_overrides_load_class_single(ctx, this) {
        return None;
    }
    let pin = single_load_class_guard_enter(ctx, this)?;
    if ctx.is_executing_instance_method(this, "loadClass", "(Ljava/lang/String;)Ljava/lang/Class;")
    {
        single_load_class_guard_leave(ctx, pin);
        return None;
    }
    let result = ctx.invoke_virtual(
        this,
        "loadClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        &[Value::Object(Some(name_obj))],
    );
    single_load_class_guard_leave(ctx, pin);
    Some(result)
}

/// True if `this` is a USER-DEFINED `ClassLoader` (a non-builtin subclass), as
/// opposed to a built-in bootstrap/extension/application/URL loader. Used to
/// decide whether `findLoadedClass`/`findLoadedClass0` must be loader-scoped
/// (a user loader only "knows" classes in its own namespace) versus the global
/// lookup that is correct for the built-in loaders.
/// True iff `this` is a bare `java.net.URLClassLoader` instance (not a user
/// subclass — a subclass already gets its own namespace via
/// `is_user_defined_loader`'s `!is_builtin_loader_class` check).
///
/// `loader_namespace_id`/`peek_loader_namespace_id` are the ONLY callers —
/// `is_builtin_loader_class` intentionally still lists
/// `"java/net/URLClassLoader"` for its other ~20 call sites (isolation
/// checks, resource/service-loader resolution, …), where the bare class is
/// correctly treated as "not a distinguished user loader implementation".
/// But `new URLClassLoader(urls)` is a completely ordinary, unlimited-arity
/// application pattern for building an ISOLATED loader (e.g. Spring Boot's
/// `ApplicationHomeTests` constructs one per test method, each defining its
/// own unrelated `com.example.Source`); routing every such instance through
/// `loader_namespace_id`'s built-in shortcut (id `0`, the shared Application
/// namespace) made a SECOND bare `URLClassLoader` instance's class-define
/// collide with the first's as `IncompatibleClassChangeError: already
/// defined by application loader` — the two loaders are unrelated objects
/// with disjoint URLs, not aliases of the single real Application loader.
pub fn is_bare_url_class_loader(ctx: &mut dyn NativeContext, this: ObjectRef) -> bool {
    if let Some(kind) = memoized_loader_kind(ctx, this) {
        return kind.bare_url;
    }
    ctx.class_name_arc_of_id(ctx.class_id_of_object(this))
        .as_deref()
        == Some("java/net/URLClassLoader")
}

pub fn is_user_defined_loader(ctx: &mut dyn NativeContext, this: ObjectRef) -> bool {
    if let Some(kind) = memoized_loader_kind(ctx, this) {
        return kind.user_defined;
    }
    let cid = ctx.class_id_of_object(this);
    // This predicate is reached from real ClassLoader bytecode before the
    // receiver's dynamic type has otherwise been constrained. Treating every
    // non-builtin class as a user loader made ordinary objects (notably String)
    // reach the synthetic CL_LOADER_ID slot probe, producing an OOB field read
    // during DoHead's reflective class loading. Require actual ClassLoader
    // inheritance before considering the built-in-name exclusion.
    let Some(class_loader) = ctx.class_id_by_name("java/lang/ClassLoader") else {
        return false;
    };
    if cid != class_loader && !ctx.is_subclass(cid, class_loader) {
        return false;
    }
    match ctx.class_name_of_id(cid) {
        Some(name) => !is_builtin_loader_class_in(&*ctx, &name),
        None => false,
    }
}

/// What [`is_user_defined_loader`] and [`is_bare_url_class_loader`] answer
/// for an instance of one `ClassLoader` subclass (`CRATONVM_LOADER_KIND_MEMO`,
/// interpreter round i3 wave 81, lane L5). Both are functions of the class
/// alone: its name (fixed for a class id) and whether it extends
/// `java.lang.ClassLoader` (fixed once true: only a compatibility stub's
/// upgrade re-parents a class, and the memo keeps no `false` for that).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoaderKind {
    pub user_defined: bool,
    pub bare_url: bool,
}

/// `CRATONVM_LOADER_KIND_MEMO`'s per-VM table: a [`LoaderKind`] per class id
/// of a `ClassLoader` subclass, filled on a receiver's first ask. A class id
/// is never reused (`ClassStore` keeps a tombstone), so a row is never stale;
/// a non-loader class (an ordinary object reaching the predicates) gets no
/// row. Cleared at [`Self::CAP`] rows (a cleared row costs one more
/// computation, the behaviour without the switch).
#[derive(Default)]
pub struct LoaderKinds {
    pub by_class: rustc_hash::FxHashMap<u32, LoaderKind>,
}

impl LoaderKinds {
    /// Rows kept before the table is cleared.
    pub const CAP: usize = 1 << 14;

    /// The memoized kind of class `class_id`, if any.
    pub fn get(&self, class_id: u32) -> Option<LoaderKind> {
        self.by_class.get(&class_id).copied()
    }

    /// Remember `kind` for class `class_id`.
    pub fn insert(&mut self, class_id: u32, kind: LoaderKind) {
        if self.by_class.len() >= Self::CAP && !self.by_class.contains_key(&class_id) {
            self.by_class.clear();
        }
        self.by_class.insert(class_id, kind);
    }

    /// Rows held.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.by_class.len()
    }
}

/// The verdicts of [`is_user_defined_loader`] and [`is_bare_url_class_loader`]
/// for class `cid`, computed exactly as they compute them, or `None` when the
/// pair must not be memoized: `java.lang.ClassLoader` or the class's name is
/// not known yet, or the class is not a `ClassLoader` subclass (a `false` that
/// a stub upgrade could turn into a `true`).
pub fn compute_loader_kind(
    ctx: &mut dyn NativeContext,
    cid: cratonvm_types::ClassId,
) -> Option<LoaderKind> {
    let class_loader = ctx.class_id_by_name("java/lang/ClassLoader")?;
    if cid != class_loader && !ctx.is_subclass(cid, class_loader) {
        return None;
    }
    let name = ctx.class_name_arc_of_id(cid)?;
    Some(LoaderKind {
        user_defined: !is_builtin_loader_class_in(&*ctx, &name),
        bare_url: &*name == "java/net/URLClassLoader",
    })
}

/// `CRATONVM_LOADER_KIND_MEMO` (interpreter round i3 wave 81, lane L5): the
/// receiver's [`LoaderKind`] from this VM's memo, filled on a miss; `None`
/// with the switch off, in a context with no per-VM slot, or for a class
/// [`compute_loader_kind`] does not memoize -- the caller then computes the
/// verdict as before. The table's lock is never held across a `ctx` call.
pub fn memoized_loader_kind(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<LoaderKind> {
    if !ctx.loader_kind_memo() {
        return None;
    }
    let cid = ctx.class_id_of_object(this);
    let hit = archive_bytes_caches(&*ctx)?
        .loader_kinds
        .read()
        .get(cid.as_u32());
    if hit.is_some() {
        return hit;
    }
    let kind = compute_loader_kind(ctx, cid)?;
    if let Some(caches) = archive_bytes_caches(&*ctx) {
        caches.loader_kinds.write().insert(cid.as_u32(), kind);
    }
    Some(kind)
}

/// One user loader's namespace answer in [`LoaderNamespaceIndex`]
/// (`CRATONVM_LOADER_NAMESPACE_INDEX`, interpreter round i3 wave 83, lane L5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NamespaceRow {
    /// The namespace id [`peek_loader_namespace_id`] / [`loader_namespace_id`]
    /// answered.
    pub ns: u32,
    /// The id came from the loader's [`LoaderMeta`] (a constructor native
    /// recorded it), not from the object-keyed namespace store: the two
    /// paths of [`loader_namespace_id_at`] record the parent link differently,
    /// and a hit replays the path that produced the row.
    pub from_meta: bool,
    /// Replaying the path's parent-link tail can change nothing any more:
    /// the link is recorded (a constructor-id loader), or the whole parent
    /// chain is recorded and ends at a non-zero built-in loader (an allocated
    /// id; a `0` may still be upgraded, so it is replayed every time, as
    /// before). A settled hit is the answer alone.
    pub parent_settled: bool,
}

/// `CRATONVM_LOADER_NAMESPACE_INDEX`'s per-VM table (interpreter round i3
/// wave 83, lane L5).
///
/// `by_loader` maps a user loader's ADDRESS to the namespace row the scans
/// over the loader-meta and namespace tables ([`LoaderIdentityTables`]) answered for
/// it. An address names one object only between two collections: an object
/// moves or dies only in a collection, and a dead loader's address is reused
/// only after one. So every row belongs to the GC epoch it was filed in (the
/// VM's `gc_collection_count` plus the loader reconcile generation, see
/// [`loader_address_epoch`]: G1's `cleanup` frees regions without booking a
/// collection; i4 wave 84), and the first ask or filing in a later epoch
/// clears the rows: the index is never re-keyed (the risk wave 81 left this
/// direction for) because it never outlives the addresses it holds. A miss,
/// a cleared row and a refused filing each cost the scans, the behaviour
/// without the switch.
///
/// `real_layout` holds the class ids [`cl_has_synthetic_layout`] found on
/// the real `java.lang.ClassLoader` layout (a witness field resolves). That
/// verdict is a function of the class (a class id is never reused, and a
/// class with the real fields keeps them; only a stub's upgrade changes a
/// layout, from synthetic to real), so it outlives epochs; a synthetic
/// verdict is never memoized.
///
/// Both halves are cleared at [`Self::CAP`] rows.
#[derive(Default)]
pub struct LoaderNamespaceIndex {
    /// The GC epoch `by_loader`'s rows belong to.
    pub epoch: u64,
    // addr-keyed: valid for ONE GC epoch only -- cleared on the first ask or filing after the VM's collection count moves (no object moves or dies between collections).
    pub by_loader: rustc_hash::FxHashMap<usize, NamespaceRow>,
    pub real_layout: rustc_hash::FxHashSet<u32>,
}

impl LoaderNamespaceIndex {
    /// Rows kept (per half) before that half is cleared.
    pub const CAP: usize = 1 << 14;

    /// The row of the loader at `addr`, if one was filed in `epoch`.
    pub fn get(&self, epoch: u64, addr: usize) -> Option<NamespaceRow> {
        if self.epoch != epoch {
            return None;
        }
        self.by_loader.get(&addr).copied()
    }

    /// File `row` for the loader at `addr` in `epoch`, dropping every row of
    /// an earlier epoch first.
    pub fn insert(&mut self, epoch: u64, addr: usize, row: NamespaceRow) {
        if self.epoch != epoch {
            self.by_loader.clear();
            self.epoch = epoch;
        }
        if self.by_loader.len() >= Self::CAP && !self.by_loader.contains_key(&addr) {
            self.by_loader.clear();
        }
        self.by_loader.insert(addr, row);
    }

    /// Did class `class_id` show the real `ClassLoader` layout?
    pub fn is_real_layout(&self, class_id: u32) -> bool {
        self.real_layout.contains(&class_id)
    }

    /// Remember that class `class_id` shows the real `ClassLoader` layout.
    pub fn note_real_layout(&mut self, class_id: u32) {
        if self.real_layout.len() >= Self::CAP && !self.real_layout.contains(&class_id) {
            self.real_layout.clear();
        }
        self.real_layout.insert(class_id);
    }

    /// Rows held for the current epoch.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.by_loader.len()
    }
}

/// [`LoaderIdentityTables::reconcile_generation`] moves on every
/// [`gc_reconcile_defining_loaders`] pass of its VM (interpreter round i4 wave
/// 84, lane L5).
///
/// `CRATONVM_LOADER_NAMESPACE_INDEX` keyed its rows on the VM's
/// `gc_collection_count` alone, on the rule "an address names one loader
/// between two collections". That count is not the event that frees a
/// loader's address on every backend: G1's `cleanup` (the STW end of a
/// concurrent mark cycle) returns whole Old regions to the free list without
/// booking a collection (`G1Collector::address_epoch`'s doc,
/// `docs/feature-designs/concurrent-gc-maturation.md`),
/// so a loader allocated into a reclaimed region before the next evacuation
/// pause could sit at a dead loader's address in the SAME count epoch and be
/// served the dead loader's namespace row. The reconcile pass is exactly the
/// point at which the object-keyed loader tables themselves learn that a
/// loader died or moved (G1 runs it at the final remark, before `cleanup`
/// frees anything; Generational and ZGC run it after every collection), so
/// the index is valid only while neither counter moves.
///
/// It was one process-wide counter, moved by every VM's reconcile ("a per-VM
/// counter would put a map lookup under a lock"); interpreter round i5 wave
/// 101, lane L5 made it a field of the VM's own tables, which the context
/// hands over without a lock, so another VM's collection no longer costs this
/// VM's index a refill.
///
/// The epoch an address-keyed loader row belongs to: the VM's collection
/// count plus its reconcile generation. Both are non-decreasing, so the sum
/// moves whenever either does; only equality of two reads means anything.
pub fn loader_address_epoch(ctx: &dyn NativeContext) -> u64 {
    // The address epoch, not the collection count (a statistic): G1's cleanup
    // and a Generational concurrent sweep slice re-use addresses without
    // booking a collection (`NativeContext::gc_address_epoch`, gc wave 9 lane I).
    ctx.gc_address_epoch().wrapping_add(
        loader_tables(ctx)
            .reconcile_generation
            .load(Ordering::Acquire),
    )
}

/// `CRATONVM_LOADER_NAMESPACE_INDEX`: the VM's current loader address epoch
/// ([`loader_address_epoch`]) when the switch is on and the context has a
/// per-VM slot; `None` otherwise (the caller then runs the scans, as before).
pub fn namespace_index_epoch(ctx: &dyn NativeContext) -> Option<u64> {
    if !ctx.loader_namespace_index() || archive_bytes_caches(ctx).is_none() {
        return None;
    }
    Some(loader_address_epoch(ctx))
}

/// The namespace row filed for `loader` in `epoch`, if any.
pub fn indexed_namespace(
    ctx: &dyn NativeContext,
    epoch: u64,
    loader: ObjectRef,
) -> Option<NamespaceRow> {
    archive_bytes_caches(ctx)?
        .loader_namespaces
        .read()
        .get(epoch, loader.as_ptr() as usize)
}

thread_local! {
    /// [`peek_loader_namespace_id`]'s last index hit on this thread:
    /// `(vm identity, loader address, loader address epoch, namespace id)`.
    pub static NAMESPACE_LAST_HIT: std::cell::Cell<(usize, usize, u64, u32)> =
        const { std::cell::Cell::new((0, 0, 0, 0)) };
}

/// `CRATONVM_LOADER_NAMESPACE_LAST_HIT` (JIT round 23 wave 2, proxy-define
/// page; default ON, `=0` kill): [`peek_loader_namespace_id`] answers a repeat
/// of this thread's last namespace-index hit without the index's lock and
/// table lookups. One proxy define asks the same loader 4-6 times in a row
/// (`findLoadedClass` per supertype, `initiated_class_mirror`); the index
/// lookups were ~10 % of the define arm. Memoised in a `MemoSlot` (1 off,
/// 2 on), re-derived on a test override.
pub fn namespace_last_hit_enabled() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on = true;
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    }
}

/// The namespace id [`note_namespace_last_hit`] recorded for `loader` in
/// `epoch` on this VM, if that is the thread's last hit. Exactly as valid as
/// the index row it copies: the row is keyed by `(epoch, loader address)` and a
/// loader's namespace id never changes, and the epoch (collection count plus
/// loader reconcile generation, [`loader_address_epoch`]) moves before a dead
/// loader's address can name another loader -- a define changes no loader's
/// id, an unload frees no address without moving it.
pub fn namespace_last_hit(ctx: &dyn NativeContext, epoch: u64, loader: ObjectRef) -> Option<u32> {
    let (vm, address, at, ns) = NAMESPACE_LAST_HIT.with(std::cell::Cell::get);
    (address != 0
        && address == loader.as_ptr() as usize
        && at == epoch
        && vm == ctx.vm_identity())
    .then_some(ns)
}

/// Record an index hit for [`namespace_last_hit`].
pub fn note_namespace_last_hit(ctx: &dyn NativeContext, epoch: u64, loader: ObjectRef, ns: u32) {
    let row = (ctx.vm_identity(), loader.as_ptr() as usize, epoch, ns);
    NAMESPACE_LAST_HIT.with(|c| c.set(row));
}

/// File `row` for `loader`, but only if no collection or loader reconcile ran
/// since `epoch` was read (the answer was computed from addresses of that
/// epoch). The index's lock is taken here alone, never across a `ctx` call.
pub fn file_namespace(ctx: &dyn NativeContext, epoch: u64, loader: ObjectRef, row: NamespaceRow) {
    if loader_address_epoch(ctx) != epoch {
        return;
    }
    if let Some(caches) = archive_bytes_caches(ctx) {
        caches
            .loader_namespaces
            .write()
            .insert(epoch, loader.as_ptr() as usize, row);
    }
}

/// Is a constructor-id loader's parent-link tail settled: the link is
/// recorded (`record_parent_link` then returns at once), or the id is a
/// built-in one (it records nothing)?
pub fn meta_parent_settled(ctx: &dyn NativeContext, ns: u32) -> bool {
    ns < 3 || loader_tables(ctx).user_loader_parents().parent_known(ns)
}

/// Is an allocated-id loader's parent-link tail settled: its whole recorded
/// chain ends at a non-zero built-in loader, so recomputing every level's
/// parent namespace re-registers the same links (a loader's parent is fixed
/// at construction; a user loader's id and the platform / application
/// namespaces never change)? A chain ending in `0` is not settled: that `0`
/// may be an application parent not yet identified, which a later replay
/// upgrades, as the scans' path does.
pub fn store_parent_settled(ctx: &dyn NativeContext, ns: u32) -> bool {
    loader_tables(ctx)
        .user_loader_parents()
        .builtin_parent(ns)
        .is_some_and(|terminal| terminal != 0)
}

/// Loader-object → CratonVM loader-namespace-id side table for real-JDK mode.
///
/// In synthetic-JDK mode a user loader's namespace id lives in the synthetic
/// `CL_LOADER_ID` field slot (populated by `ClassLoader.<init>`/`defineClass`,
/// and mirrored into [`LoaderMeta::loader_id`] since L1). In real-JDK mode
/// that slot is a genuine `java.lang.ClassLoader` field and cannot be
/// repurposed, so the id is keyed on the loader OBJECT instead.
///
/// Distinct from the L1 loader-meta table ([`LoaderIdentityTables`]), which it sits directly beside:
/// this table holds ids `loader_namespace_id_at` allocated *lazily*, on first
/// request, for loaders whose constructor never ran through one of our
/// natives. `LoaderMeta` holds what the constructor natives themselves
/// recorded. Both are object-keyed and both are pruned/remapped by the same
/// [`gc_reconcile_defining_loaders`] pass, for the same reason.
///
/// # GC contract — both halves are load-bearing
///
/// The entries are `(ObjectRef, u32)`. The `ObjectRef` is a raw heap address, so
/// this table has a two-part GC contract and **neither half may be dropped**:
///
/// * it is deliberately **NOT** a GC root (see `gc_scan_loader_singleton_roots`,
///   which roots only the app/platform singletons) — rooting it would pin every
///   user loader forever and defeat loader unloading;
/// * therefore [`gc_reconcile_defining_loaders`] MUST prune entries whose loader
///   died this cycle and remap the survivors that moved. That is the *only*
///   thing standing between a dead loader's address being reused and a brand-new
///   loader silently inheriting the dead one's namespace id — and, through
///   `UserLoaderParents::register`, its parent link too.
///
/// The doc comment this replaced still described the table's *previous*,
/// identity-hash-keyed form ("holds only `i32 → u32` … no GC rooting"), which
/// had stopped being true; a reader who believed it would conclude there was
/// nothing for the collector to do here. Pinned by
/// `loader_namespace_store_is_pruned_and_remapped_by_gc_reconcile`.
///
/// # The table is per VM
///
/// Each row was `(vm_identity, loader, namespace id)` in one process table
/// (gc-common w9-a); since interpreter round i5 wave 101, lane L5 the table is
/// [`LoaderIdentityTables::loader_namespaces`], the VM's own, and a row is
/// `(loader, namespace id)`. The address means something only in the heap
/// that allocated it, so every lookup goes to the caller's VM's table; the GC
/// pass judges and remaps only that table, VM creation touches no other VM's
/// rows, and the table dies with its VM. Pinned by
/// `w9a_loader_namespace_and_meta_rows_belong_to_their_vm`.
///
/// Keyed by the loader OBJECT, not its identity hash: identity hashes can
/// collide across distinct loader instances (address-derived hashes recur
/// after a collection reuses the region), and the old hash-keyed map never
/// pruned dead loaders — a fresh per-compile loader (Spring TestCompiler's
/// DynamicClassLoader) could inherit a dead sibling's namespace id and
/// resolve THAT loader's same-named generated classes. Entries are
/// remapped/pruned post-GC by [`gc_reconcile_defining_loaders`].
///
/// Reverse of `loader_namespace_id`: given a namespace id already allocated
/// via the object-keyed side table (real-JDK mode's path — a `UserDefined`
/// id from `class_manager`'s per-class `loader_id`, e.g. as returned by
/// `NativeContext::loader_id_of_class`), find the live `ClassLoader` object
/// that owns it. `None` for built-in namespaces (0/1/2) or a namespace this
/// process never allocated via the object-keyed store.
///
/// Since L1 an id assigned by a `ClassLoader` constructor native (rather than
/// by `loader_namespace_id_at`'s own allocation) IS tracked here too:
/// `remember_namespace_object` mirrors it in on first lookup. The doc used to
/// name that gap — "one only ever set through the synthetic-JDK
/// `CL_LOADER_ID` field slot, which this store doesn't track" — and it is now
/// closed in both modes.
///
/// Exists because several call sites need to *actively drive* a specific
/// loader's own `loadClass()` (JVMS §5.4.3 initiating-loader semantics) once
/// they already know a class's numeric namespace id but not the loader
/// object itself — `defining_loader_for` (a separate, narrowly-populated
/// side table keyed by `class_id`, written only by explicit
/// `register_defining_loader` calls) is NOT a reliable source for this: a
/// class defined via `ucl_try_define_local_class`'s isolated-loader native
/// path IS correctly assigned a real `UserDefined` namespace id (this store
/// IS populated for it, since `loader_namespace_id` is exactly what
/// assigned that id), independent of whether `register_defining_loader`
/// also happened to run for it.
///
/// (The VM-less, test-only form `loader_object_for_namespace_id` went with
/// the process table, interpreter round i5 wave 101, lane L5: there is no
/// table of every VM's rows left to search.)
///
/// The namespace reverse lookup in the VM's own table: the loader object it
/// returns is always one of that VM's own heap objects.
pub fn loader_object_for_namespace_id_in(
    tables: &LoaderIdentityTables,
    ns_id: u32,
) -> Option<ObjectRef> {
    if ns_id < 3 {
        return None;
    }
    tables
        .loader_namespaces
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .loader_of(tables.rows_generation(), ns_id)
}

/// The `ClassLoader` object of user-defined loader namespace `ns_id` in the
/// caller's VM ([`loader_object_for_namespace_id_in`]); `None` for a built-in
/// namespace or one no loader holds. For the load-time transformer chain,
/// which names a `defineClass` caller's loader by its `ClassLoaderId` and
/// hands transformers (and, interpreter round i2 wave 53, C agents'
/// `ClassFileLoadHook`) the loader object.
pub fn user_loader_object(ctx: &dyn NativeContext, ns_id: u32) -> Option<ObjectRef> {
    loader_object_for_namespace_id_in(loader_tables(ctx), ns_id)
}

/// Stable CratonVM loader-namespace id for a `ClassLoader` instance, allocating
/// one on first request. Built-in loaders map to `0` (the Application / global
/// namespace — they ARE the global store). A user-defined loader uses the id
/// its constructor native recorded ([`LoaderMeta::loader_id`], mirrored into
/// the synthetic `CL_LOADER_ID` slot in synthetic-JDK mode) when it has one,
/// and otherwise gets one allocated here and keyed on the loader object. Used
/// by `defineClass` to give a user loader its own namespace so an
/// override-first redefinition of an already-loaded class does not collide
/// with the original definer.
pub fn loader_namespace_id(ctx: &mut dyn NativeContext, loader: ObjectRef) -> u32 {
    loader_namespace_id_at(ctx, loader, 0)
}

/// `loader_namespace_id` with the parent-walk recursion depth threaded through.
/// The depth cap only guards against a malformed `parent` cycle — real chains
/// are 2-3 deep.
pub fn loader_namespace_id_at(ctx: &mut dyn NativeContext, loader: ObjectRef, depth: usize) -> u32 {
    // `CRATONVM_LOADER_NAMESPACE_INDEX` (interpreter round i3 wave 83, lane
    // L5): a user loader already answered in this GC epoch replays only its
    // path's parent-link tail (nothing once settled); everything else runs
    // the scans and files the user loader's answer.
    let epoch = namespace_index_epoch(&*ctx);
    if let Some(epoch) = epoch {
        if let Some(row) = indexed_namespace(&*ctx, epoch, loader) {
            return replay_namespace_row(ctx, epoch, loader, row, depth);
        }
    }
    let (ns, row) = loader_namespace_id_scan(ctx, loader, depth);
    if let (Some(epoch), Some(row)) = (epoch, row) {
        file_namespace(&*ctx, epoch, loader, row);
    }
    ns
}

/// An indexed loader's [`loader_namespace_id_at`] tail: exactly what the
/// scans' path that produced `row` does after its lookup -- for a
/// constructor id, mirror it into the object-keyed store and record the
/// parent link once; for an allocated id, recompute the parent's namespace
/// and register the link -- skipped once [`NamespaceRow::parent_settled`].
pub fn replay_namespace_row(
    ctx: &mut dyn NativeContext,
    epoch: u64,
    loader: ObjectRef,
    row: NamespaceRow,
    depth: usize,
) -> u32 {
    if row.parent_settled {
        return row.ns;
    }
    let settled = if row.from_meta {
        remember_namespace_object(loader_tables(ctx), loader, row.ns);
        record_parent_link(ctx, loader, row.ns, depth);
        meta_parent_settled(ctx, row.ns)
    } else {
        let parent_ns = parent_namespace_id(ctx, loader, depth);
        loader_tables(ctx).user_loader_parents().register(row.ns, parent_ns);
        store_parent_settled(ctx, row.ns)
    };
    if settled {
        file_namespace(
            &*ctx,
            epoch,
            loader,
            NamespaceRow {
                parent_settled: true,
                ..row
            },
        );
    }
    row.ns
}

/// [`loader_namespace_id_at`] by the scans (the whole computation without
/// `CRATONVM_LOADER_NAMESPACE_INDEX`), with the row the index may file for a
/// user loader (`None` for a built-in one).
pub fn loader_namespace_id_scan(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    depth: usize,
) -> (u32, Option<NamespaceRow>) {
    if !is_user_defined_loader(ctx, loader) && !is_bare_url_class_loader(ctx, loader) {
        // Which built-in loader `loader` actually is matters to a caller
        // reached through `parent_namespace_id`: a blanket `0` here does not
        // just mean "no id", it means "this loader's PARENT delegates to the
        // WHOLE built-in chain (Bootstrap, Extension, Application)" — see
        // `loaded_class_for_requesting_loader`'s built-in-chain fallback.
        // Collapsing the platform loader into that generic `0` let a
        // `ModifiedClassPathClassLoader` (parent = platform, specifically to
        // EXCLUDE Application from delegation) fall back to probing
        // Application anyway, silently resolving a same-named class through
        // the wrong loader — observed as the `PropertySource`/
        // `EnumerablePropertySource` cross-loader `ClassCastException`
        // family under `@ClassPathExclusions`. Identity check mirrors
        // `parent_is_platform` above: the singleton reference is the fast
        // path, the class name is the real-JDK fallback (the JDK can
        // manufacture another `PlatformClassLoader` object before our
        // singleton is observed).
        let is_platform = platform_loader_of(loader_tables(ctx))
            .is_some_and(|p| p.as_ptr() == loader.as_ptr())
            || ctx
                .class_name_of_id(ctx.class_id_of_object(loader))
                .is_some_and(|name| name == "jdk/internal/loader/ClassLoaders$PlatformClassLoader");
        if is_platform {
            return (cratonvm_types::ClassLoaderId::NATIVE_EXTENSION, None);
        }
        if app_loader_of(loader_tables(ctx)).is_some_and(|p| p.as_ptr() == loader.as_ptr()) {
            return (cratonvm_types::ClassLoaderId::NATIVE_APPLICATION, None);
        }
        return (0, None);
    }
    if let Some(v) = loader_id_of(ctx, loader) {
        // A loader whose id came from a `ClassLoader` constructor native has
        // it in the L1 side table (and, in synthetic-JDK mode, in the field
        // slot as well), so this path never reaches the allocation below —
        // the parent link has to be recorded here too or the chain is
        // invisible for those loaders.
        //
        // Mirror it into the object-keyed store as well, or
        // `loader_object_for_namespace_id` cannot answer for this loader and
        // the "drive that loader's own loadClass" call sites lose their
        // receiver. Before L1 real-JDK mode got this for free: the
        // constructor's slot-6 write was coerced away, so the code below ran
        // and pushed the entry. Now that the id survives, push it here —
        // which also closes the same hole synthetic-JDK mode has always had
        // (`loader_object_for_namespace_id`'s doc used to name it).
        // Insert BEFORE `record_parent_link`, which recurses into this
        // function against a non-reentrant `Mutex`.
        remember_namespace_object(loader_tables(ctx), loader, v);
        record_parent_link(ctx, loader, v, depth);
        let row = namespace_row_for(ctx, loader, v, true);
        return (v, row);
    }
    // Resolve the parent's namespace BEFORE taking the store lock: doing it
    // afterwards re-enters this function (the parent may not have an id yet)
    // against a non-reentrant `std::sync::Mutex`.
    let parent_ns = parent_namespace_id(ctx, loader, depth);
    let found = {
        let tables = loader_tables(ctx);
        tables
            .loader_namespaces
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .namespace_of(tables.rows_generation(), loader)
    };
    if let Some(id) = found {
        loader_tables(ctx).user_loader_parents().register(id, parent_ns);
        let row = namespace_row_for(ctx, loader, id, false);
        return (id, row);
    }
    // The table is reached through `ctx` now (interpreter round i5 wave 101,
    // lane L5), so its lock cannot be held across `allocate_loader_id`
    // (`&mut ctx`): allocate first, then insert unless a racing thread filed
    // the loader meanwhile, whose id then wins (a spent id costs nothing; ids
    // come from a counter).
    let fresh = ctx.allocate_loader_id();
    let id = {
        let tables = loader_tables(ctx);
        let mut map = tables
            .loader_namespaces
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let existing = map.namespace_of(tables.rows_generation(), loader);
        match existing {
            Some(id) => id,
            None => {
                map.push((loader, fresh));
                fresh
            }
        }
    };
    loader_tables(ctx).user_loader_parents().register(id, parent_ns);
    let row = namespace_row_for(ctx, loader, id, false);
    (id, row)
}

/// The [`NamespaceRow`] `CRATONVM_LOADER_NAMESPACE_INDEX` may file for a user
/// loader the scans answered `ns` for; `None` with the switch off (nothing is
/// computed) or for a loader on the synthetic layout, whose namespace may
/// still come from its `CL_LOADER_ID` slot when a later native writes it.
pub fn namespace_row_for(
    ctx: &dyn NativeContext,
    loader: ObjectRef,
    ns: u32,
    from_meta: bool,
) -> Option<NamespaceRow> {
    if !ctx.loader_namespace_index() || cl_has_synthetic_layout(ctx, loader) {
        return None;
    }
    let parent_settled = if from_meta {
        meta_parent_settled(ctx, ns)
    } else {
        store_parent_settled(ctx, ns)
    };
    Some(NamespaceRow {
        ns,
        from_meta,
        parent_settled,
    })
}

/// Record `loader → id` in the object-keyed namespace store if it is not
/// already there, so [`loader_object_for_namespace_id_in`] can invert an id that
/// was assigned by a `ClassLoader` constructor native rather than by
/// [`loader_namespace_id_at`]'s own allocation. Takes the lock briefly and
/// recurses into nothing. `tables` are the loader's own VM's
/// (`loader_tables(ctx)`).
pub fn remember_namespace_object(tables: &LoaderIdentityTables, loader: ObjectRef, id: u32) {
    if id < 3 {
        return;
    }
    let mut map = tables
        .loader_namespaces
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if map.namespace_of(tables.rows_generation(), loader).is_some() {
        return;
    }
    map.push((loader, id));
}

/// Namespace id of `loader`'s delegation parent, allocating one for the parent
/// if it does not have one yet. `0` for a built-in (or absent) parent.
///
/// Allocating eagerly matters: the child is normally the first of the pair to
/// define a class, so a lazy "record it when the parent gets an id" scheme
/// would leave the link permanently unset for exactly the case that needs it —
/// Spring's `TestCompiler` `DynamicClassLoader` over a
/// `@CompileWithForkedClassLoader` fork loader. Allocating an id for a loader
/// that never defines a class costs nothing: ids come from a counter, and the
/// class manager's `user_loaders` set is still only populated at define time.
pub fn parent_namespace_id(ctx: &mut dyn NativeContext, loader: ObjectRef, depth: usize) -> u32 {
    if depth >= cratonvm_classloading::MAX_USER_LOADER_DEPTH {
        return 0;
    }
    match classloader_parent(ctx, loader) {
        Some(parent) if parent.as_ptr() != loader.as_ptr() => {
            loader_namespace_id_at(ctx, parent, depth + 1)
        }
        _ => 0,
    }
}

/// Record `loader`'s parent link when its namespace id was already known.
/// A link once recorded — including the `0` that means "delegates to the
/// built-in chain" — is never recomputed, so this stays a single map probe on
/// the hot path.
pub fn record_parent_link(ctx: &mut dyn NativeContext, loader: ObjectRef, id: u32, depth: usize) {
    if id < 3 || loader_tables(ctx).user_loader_parents().parent_known(id) {
        return;
    }
    let parent_ns = parent_namespace_id(ctx, loader, depth);
    loader_tables(ctx).user_loader_parents().register(id, parent_ns);
}

/// Read-only probe of a user loader's namespace id (no allocation). `None` when
/// the loader is built-in, or has not yet been assigned one (it has defined no
/// class under a distinct namespace).
pub fn peek_loader_namespace_id(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
) -> Option<u32> {
    // `CRATONVM_LOADER_NAMESPACE_INDEX` (interpreter round i3 wave 83, lane
    // L5): an answer filed in this GC epoch, else the scans (a found id is
    // filed; "no id yet" is not, so the allocation that follows is seen).
    let epoch = namespace_index_epoch(&*ctx);
    if let Some(epoch) = epoch {
        let last_hit = namespace_last_hit_enabled();
        if last_hit {
            if let Some(ns) = namespace_last_hit(&*ctx, epoch, loader) {
                return Some(ns);
            }
        }
        if let Some(row) = indexed_namespace(&*ctx, epoch, loader) {
            if last_hit {
                note_namespace_last_hit(&*ctx, epoch, loader, row.ns);
            }
            return Some(row.ns);
        }
    }
    if !is_user_defined_loader(ctx, loader) && !is_bare_url_class_loader(ctx, loader) {
        return None;
    }
    if let Some(v) = loader_id_of(ctx, loader) {
        if let Some(epoch) = epoch {
            if let Some(row) = namespace_row_for(&*ctx, loader, v, true) {
                file_namespace(
                    &*ctx,
                    epoch,
                    loader,
                    NamespaceRow {
                        parent_settled: false,
                        ..row
                    },
                );
            }
        }
        return Some(v);
    }
    let id = {
        let tables = loader_tables(ctx);
        tables
            .loader_namespaces
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .namespace_of(tables.rows_generation(), loader)
    }?;
    if let Some(epoch) = epoch {
        if let Some(row) = namespace_row_for(&*ctx, loader, id, false) {
            file_namespace(
                &*ctx,
                epoch,
                loader,
                NamespaceRow {
                    parent_settled: false,
                    ..row
                },
            );
        }
    }
    Some(id)
}

/// True for a JDK dynamic-proxy class's internal name (`jdk/proxyN/$ProxyM` on
/// JDK 9+, `com/sun/proxy/$ProxyM` on the legacy layout). Generated proxies are
/// never real classpath classes, so a built-in loader can only ever "find" one
/// because it leaked into CratonVM's flat global store — see the use in
/// [`find_loaded_class_for_loader`].
///
/// By generated-proxy SHAPE (`$Proxy<digits>` simple name) in any package: a
/// proxy for a non-public interface is defined in THAT interface's package
/// (`p/$Proxy3`, as `ProxyBuilder` does), which the older
/// `jdk/proxy*` / `com/sun/proxy*` prefix test missed, so the sibling-loader
/// leak guard did not cover it (round 13 wave 7, lane predicates).
/// `CRATONVM_PROXY_USER_SUBCLASS_ORDINARY=0` restores the prefix test.
pub fn is_generated_proxy_name(name: &str) -> bool {
    if !crate::reflect_annotations::proxy_user_subclass_is_ordinary() {
        return (name.starts_with("jdk/proxy") || name.starts_with("com/sun/proxy"))
            && name.contains("$Proxy");
    }
    crate::reflect_annotations::proxy_name_has_generated_shape(name)
}

/// True when `cid` is a generated proxy class that loader `this` must NOT be
/// allowed to resolve: a proxy is visible only to its defining loader and that
/// loader's delegation descendants (JVMS §5.3). Stops CratonVM's flat global
/// store from leaking one loader's proxy to an unrelated/sibling loader through
/// the various name-keyed resolution natives. ClassUtilsTests.isCacheSafe.
pub fn proxy_hidden_from(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    internal: &str,
    cid: cratonvm_types::ClassId,
) -> bool {
    if !is_generated_proxy_name(internal) {
        return false;
    }
    match defining_loader_for(loader_tables(ctx), cid.as_u32()) {
        Some(def) => !loader_can_see_defining(ctx, this, def),
        None => false,
    }
}

/// Shared `findLoadedClass` logic (JVMS §5.3): returns the Class mirror for
/// `internal_name` only if `this` loader is recorded as having loaded it —
/// NEVER a class some OTHER loader happens to have loaded. Does NOT trigger
/// loading.
///
/// For a built-in loader the global loaded-class set is the right answer —
/// EXCEPT for a bare `java.net.URLClassLoader`, which is a JDK class but a
/// user-defined loader and takes the user-defined path below for every
/// non-proxy name (W7-82 / W7-87; see the long comment in
/// `find_loaded_class_for_loader_inner`). For a
/// user-defined loader, a class counts as "loaded by this loader" if either:
///   1. it lives in this loader's own namespace (a distinct copy this loader
///      defined — the override-first redefinition case), or
///   2. the globally-known class of that name records THIS loader as its
///      defining loader (the common case: e.g. ByteBuddy's `ByteArrayClassLoader`
///      defines under the Application namespace but registers itself as definer).
/// Otherwise it is not visible to this loader as "already loaded" → `None`,
/// which lets the loader's `loadClass` override proceed to `findClass`/define.
/// The `ClassId` of a class named `internal_name` that **this exact loader
/// object** is recorded as the defining loader of.
///
/// This is the strongest identity statement the VM can make about "who defined
/// it", and it is deliberately narrower than a namespace-id match. CratonVM
/// keys its class store on `(loader_id, name)`, and a `loader_id` is a
/// synthetic NAMESPACE number, not a loader: two distinct `ClassLoader` objects
/// can end up sharing one. That is not hypothetical — it is the measured shape
/// behind the Tomcat webapp stop/start family, where the ~14th
/// `WebappClassLoader` in a process started colliding with an earlier, already
/// finished one over `org/apache/catalina/loader/JdbcLeakPrevention`. On
/// HotSpot each of those loaders defines its own copy and none of them
/// conflicts.
///
/// So the two questions must not be confused:
///
/// * *"does this NAMESPACE hold the name"* — `class_id_defined_by_loader_exact`,
///   which is what the store can answer cheaply and what the duplicate-define
///   probe in the class manager uses; and
/// * *"did THIS OBJECT define it"* — this function, which is the one JVMS
///   §5.3.5 turns on and the only one that may raise a `LinkageError`.
///
/// **A `None` here is never proof of the negative.** The record is an
/// `ObjectRef` and a moving collection can leave a stale pointer, so a genuine
/// same-loader define can read back as "not recorded". Every caller must treat
/// `None` as "cannot tell" and take the permissive branch: a missed
/// `LinkageError` is the pre-existing behaviour, a spurious one is a new way to
/// break a workload.
pub fn class_defined_by_this_loader_object(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    internal_name: &str,
) -> Option<cratonvm_types::ClassId> {
    // gc-common w10-f: one index look-up instead of a walk over every row of
    // every VM. Three phases, so no class-manager read (`class_name_arc_of_id`)
    // runs under the process-wide store mutex:
    //   1. under the lock, answer from the memoized names, or snapshot the ids
    //      whose names were never read;
    //   2. read those names with no lock held;
    //   3. under the lock again, publish them for the next probe.
    // A loader defining `n` classes therefore reads each name once in total,
    // not once per later miss.
    let key = this.as_ptr() as usize;
    let unread: Vec<u32> = {
        // Phase 1 only reads (wave 143: a shared read under
        // `CRATONVM_DEFINING_LOADER_SHARED_READS`); phase 3 publishes under
        // the exclusive side, as before.
        let table = defining_table_read(loader_tables(ctx));
        let entry = table.by_loader.get(&key)?;
        if let Some(&cid) = entry.by_name.get(internal_name) {
            return Some(cratonvm_types::ClassId::new(cid));
        }
        entry.ids[entry.named..].to_vec()
    };
    if unread.is_empty() {
        return None;
    }
    let read: Vec<(u32, Option<Arc<str>>)> = unread
        .into_iter()
        .map(|cid| {
            (
                cid,
                ctx.class_name_arc_of_id(cratonvm_types::ClassId::new(cid)),
            )
        })
        .collect();
    let hit = read
        .iter()
        .find(|(_, name)| name.as_deref() == Some(internal_name))
        .map(|&(cid, _)| cratonvm_types::ClassId::new(cid));
    if let Some(entry) = loader_tables(ctx)
        .defining
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .by_loader
        .get_mut(&key)
    {
        entry.publish_names(&read);
    }
    hit
}

pub fn find_loaded_class_for_loader(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    internal_name: &str,
) -> Option<ObjectRef> {
    let __obsreg_dbg = crate::vmflags().loader.dbg_obsreg
        && (internal_name.contains("ObservationRegistry")
            || internal_name.contains("SecurityFilterAutoConfigurationEarlyInitializationTests")
            || internal_name.contains("PathRequestTests")
            || internal_name.contains("ManagementWebSecurityAutoConfigurationTests"));
    let __is_user_defined = is_user_defined_loader(ctx, this);
    if __obsreg_dbg {
        eprintln!(
            "[OBSREG-DBG] find_loaded_class_for_loader(this={:?}, name={}) is_user_defined={}",
            this, internal_name, __is_user_defined
        );
    }
    let __result =
        find_loaded_class_for_loader_inner(ctx, this, internal_name, __is_user_defined, false);
    if __obsreg_dbg {
        let cid = __result.map(|m| ctx.class_id_of_object(m));
        eprintln!(
            "[OBSREG-DBG] find_loaded_class_for_loader(this={:?}, name={}) -> {:?} (class_id={:?})",
            this, internal_name, __result, cid
        );
    }
    __result
}

/// zt df4 §9c: [`find_loaded_class_for_loader`] for the Java-facing
/// `findLoadedClass` natives (`ClassLoader.findLoadedClass`,
/// `findLoadedClass0`). HotSpot's `JVM_FindLoadedClass` never loads: a
/// runtime-image class of a module defined to the built-in loader that the
/// VM has not materialised yet is not "loaded", so the answer is null
/// (`CRATONVM_FIND_LOADED_BUILTIN_NO_MATERIALIZE`, default ON, `=0` kill:
/// the lazy define answers it, as before). The JDK's
/// `BuiltinClassLoader.loadClassOrNull` then reaches
/// `findClassInModuleOrNull`, whose reviewed `Intrinsic`
/// (`classloader_real::bcl_find_class_in_module_or_null`) materialises the
/// VM's class instead of defining a second copy from the module's bytes.
/// The VM's own callers keep [`find_loaded_class_for_loader`].
pub fn find_loaded_class_for_loader_java(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    internal_name: &str,
) -> Option<ObjectRef> {
    let is_user_defined = is_user_defined_loader(ctx, this);
    find_loaded_class_for_loader_inner(ctx, this, internal_name, is_user_defined, true)
}

/// `CRATONVM_FIND_LOADED_BUILTIN_NO_MATERIALIZE` (see
/// [`find_loaded_class_for_loader_java`]). Read only on the lazy-define arm
/// (a name the class table does not hold) and by the intrinsic.
pub fn find_loaded_no_materialize_enabled() -> bool {
    true
}

/// `CRATONVM_FIND_LOADED_BUILTIN_BEHIND_USER` -- correctness, default ON,
/// `=0` restores the old `None` (JIT round 18 wave 11, lane mhresid): a
/// built-in loader's `findLoadedClass` reports its own definition of a name
/// even when a user-defined loader defined a class of that name too
/// ([`builtin_own_definition`]). Read only when the global probe missed.
///
/// Round 18 first hung this on a second arm that answered when the global
/// lookup returned the USER loader's copy. That arm could not fire once
/// `class_id_by_name` failed closed on an ambiguous name: a built-in copy
/// beside the user copy makes the name ambiguous, so the lookup never returns
/// the user copy while a built-in one exists. The switch now gates the arm that
/// does answer.
pub fn find_loaded_builtin_behind_user_enabled() -> bool {
    true
}

pub fn find_loaded_class_for_loader_inner(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    internal_name: &str,
    is_user_defined: bool,
    java_facing: bool,
) -> Option<ObjectRef> {
    // W7-82 / W7-87 — the bare-`URLClassLoader` carve-out, both halves.
    //
    // `is_user_defined_loader` answers "is the loader's CLASS a JDK loader
    // class", and `is_builtin_loader_class` lists `java/net/URLClassLoader`
    // among them. But `URLClassLoader` is the one entry on that list with a
    // PUBLIC constructor — `java/lang/ClassLoader` is abstract and
    // `java/security/SecureClassLoader`'s constructors are protected, so an
    // instance of either is necessarily a user subclass carrying a user class
    // name. A bare `new URLClassLoader(urls, parent)` is therefore a JDK class
    // but a genuinely USER-DEFINED loader, and the namespace allocator already
    // says so: `loader_namespace_id_at` and `peek_loader_namespace_id` both
    // spell their guard `!is_user_defined_loader(..) &&
    // !is_bare_url_class_loader(..)`, so a bare `URLClassLoader` DEFINES into
    // its own namespace id (>= 3).
    //
    // This function was the one site left out of that carve-out, and the two
    // halves then disagreed in BOTH directions.
    //
    // W7-82 closed the first. The built-in branch's "a built-in loader never
    // counts as having loaded a class a user-defined loader defined" clause
    // (`loader_id_of_class(cid) > 2 -> None`) hid namespace-3 classes from the
    // very loader that had defined them, so the cache probe went blind, every
    // later lookup re-drove `define_class_full`, and a second
    // `Class.forName(name, true, loader)` surfaced as `ClassFormatError: ...
    // already defined by user-defined(3) loader` where HotSpot returns the
    // cached class.
    //
    // W7-87 closes the second — this branch's OTHER half. The GLOBAL FALLBACK
    // below also answered a bare `URLClassLoader` with any APPLICATION-namespace
    // class of that name: one it never defined and was never asked to load.
    // Measured against HotSpot 25: `new URLClassLoader(urls, null)
    // .loadClass("SomeAppClass")` returned the application loader's class where
    // HotSpot raises `ClassNotFoundException`, and `findLoadedClass` reported it
    // where HotSpot reports null. `new URLClassLoader(urls, null)` is THE
    // isolating-loader idiom, so the fallback silently defeated the isolation
    // the loader was constructed for — and `ucl_try_define_local_class`'s own
    // doc comment already named the rule it was breaking ("would let a
    // `URLClassLoader(urls, null)` resolve application classes its own (failed)
    // URL search should have hidden from it"). A `URLClassLoader` SUBCLASS was
    // correct throughout, on both arms: one line of `extends` decided it.
    //
    // So a bare `URLClassLoader` now takes the USER-DEFINED branch below
    // outright — exactly the predicate the namespace allocator uses. That
    // branch's steps 1 and 2 ARE W7-82's two additive probes, verbatim, so
    // nothing that half fixed is given up; what changes is that a miss now ends
    // in `None` instead of the global fallback. Unlike W7-82 this direction is
    // a NARROWING, with real blast radius — see
    // W7-87-urlclassloader-namespace-asymmetry.md.
    //
    // ONE case stays on the built-in branch: a GENERATED PROXY name. That arm is
    // already loader-identity- and delegation-aware (`proxy_hidden_from` ->
    // `loader_can_see_defining`), so it is not a leak, and
    // `classloader_real::load_class_visible_to` short-circuits proxy resolution
    // to this function BEFORE parent delegation runs — dropping it here would
    // turn a proxy a bare `URLClassLoader` can legitimately see through its
    // parent into a `ClassNotFoundException`. One axis at a time; proxy
    // visibility is not this lane's.
    let takes_builtin_branch = !is_user_defined
        && (is_generated_proxy_name(internal_name) || !is_bare_url_class_loader(ctx, this));
    if takes_builtin_branch {
        // SBR2 wave 9 (modload): a class the VM has not materialised yet, in a
        // module defined to THIS built-in loader (see
        // `lazy_define_for_builtin_loader`). Asked only for a name the class
        // table does not hold at all, so the visibility rules below are
        // untouched for every class that is known.
        let Some(known_cid) = ctx.class_id_by_name(internal_name).or_else(|| {
            find_loaded_builtin_behind_user_enabled()
                .then(|| builtin_own_definition(ctx, this, internal_name))
                .flatten()
        }) else {
            if java_facing && find_loaded_no_materialize_enabled() {
                return None;
            }
            return lazy_define_for_builtin_loader(ctx, this, internal_name);
        };
        return Some(known_cid).and_then(|cid| {
            // A generated proxy is checked via `proxy_hidden_from` — loader-identity
            // and delegation aware — REGARDLESS of `loader_id_of_class(cid)`. Proxy
            // classes get a fresh per-generation internal loader/module id (e.g. the
            // `jdk/proxy1/…` numbering) even when their defining loader is a
            // BUILT-IN loader (the application loader itself), so `loader_id_of_class`
            // is > 2 for a proxy the app loader legitimately just defined. Running the
            // blanket "> 2 -> hide" check first (as before) hid every generated proxy
            // from `findLoadedClass`/`Class.forName(name, false, loader)` even when
            // `this` WAS the recorded defining loader: a fresh `Class.forName` on a
            // proxy class the app loader had itself just created via
            // `Proxy.newProxyInstance` reported it as not found, throwing
            // ClassNotFoundException. That broke AspectJ's reflection-based pointcut
            // matching, which resolves a scratch composite-interface proxy class by
            // name via `Class.forName` to inspect its superclass: the lookup failure
            // surfaced as `ReflectionWorldException: can't determine superclass of
            // missing type jdk.proxyN.$ProxyM`, silently dropping the advisor
            // (Spring's `AspectJExpressionPointcut` catches the exception and falls
            // back to a "never matches" verdict) — reproduced by Spring's
            // `AspectJAutoProxyCreatorTests` (an `@Around` advice on an inherited
            // default interface method never fired).
            if is_generated_proxy_name(internal_name) {
                if proxy_hidden_from(ctx, this, internal_name, cid) {
                    return None;
                }
                return Some(ctx.get_class_mirror(cid));
            }
            // JVMS §5.3: a built-in loader (bootstrap/platform/app) never counts
            // as having loaded a class that a *user-defined* loader defined.
            // CratonVM's flat global store would otherwise let the app loader
            // report a child loader's class as "already loaded" — and since
            // `loadClass` delegates parent-first, a sibling custom loader then
            // resolves it too (`ClassUtilsTests.isCacheSafe`).
            //
            // Actual user-loader namespace hits are not visible to built-in
            // loaders. Application-namespace classes that merely record a
            // user-defined defining loader still keep their app-loader
            // visibility below.
            //
            // The three built-in loaders are NOT mutually visible either: they
            // form a strict ancestor chain (Bootstrap -> Extension/Platform ->
            // Application), and `findLoadedClass` must only report a class
            // defined by `this` or one of `this`'s OWN ancestors — never a
            // descendant's. A blanket `> 2` here treated Bootstrap, Extension,
            // and Application as one undifferentiated group: asking the
            // PLATFORM loader whether it has an application class "loaded"
            // (as happens on every `super.loadClass` delegation from a loader
            // parented to platform — e.g. a `ModifiedClassPathClassLoader`,
            // Spring's `@ClassPathExclusions` isolation) found the app
            // loader's pre-existing copy and returned it, so the isolated
            // loader's `loadClass` never reached its own `findClass` to define
            // a fresh one — a same-named class split across two loaders,
            // observed as the `PropertySource`/`EnumerablePropertySource`
            // family's `ClassCastException` under `@ClassPathExclusions`.
            // `builtin_loader_ordinal(this) == None` (bootstrap has no `this`
            // object in practice, or the singleton could not be identified)
            // keeps the old permissive bound as a safe fallback.
            let candidate_ordinal = ctx.loader_id_of_class(cid);
            if candidate_ordinal > 2 {
                // A user loader's copy is never a built-in loader's loaded class.
                // The built-in copy behind it (JIT round 18,
                // `docs/feature-designs/native-dispatch-architecture.md`) is
                // answered by `builtin_own_definition` above: with both copies
                // defined the global lookup is ambiguous and never lands here.
                return None;
            }
            // zt t21/df9: the platform loader's Java-defined copy of a name
            // the application loader defined too (their own namespaces now,
            // `lang_system::define_loader_namespace`); the global lookup
            // answers the platform one first.
            if candidate_ordinal == cratonvm_types::ClassLoaderId::NATIVE_EXTENSION as i32 {
                if let Some(own) = builtin_own_definition_first(ctx, this, internal_name, cid) {
                    return Some(ctx.get_class_mirror(own));
                }
            }
            if let Some(this_ordinal) = builtin_loader_ordinal(ctx, this) {
                if candidate_ordinal > this_ordinal as i32 {
                    return None;
                }
            }
            if let Some(def) = defining_loader_for(loader_tables(ctx), cid.as_u32()) {
                if !loader_can_see_defining(ctx, this, def) {
                    return None;
                }
            }
            Some(ctx.get_class_mirror(cid))
        });
    }
    // 1. Own-namespace copy — EXACT (no global / parent-delegation fallback).
    // `findLoadedClass` must report ONLY a class THIS user loader has itself
    // defined; the fallback-prone `class_id_by_name_and_loader` would otherwise
    // hand back some OTHER loader's copy of a name this loader has not defined.
    // That broke classloader isolation once a child loader already held a
    // namespace (≥1 defined class): an as-yet-undefined but eligible class
    // (e.g. an annotation interface the child overrides) resolved to the parent/
    // app copy instead of the child defining its own, so
    // `annotation.getClass().getClassLoader()` reported the app loader
    // (MergedAnnotationClassLoaderTests.synthesizedUsesCorrectClassLoader).
    // `define_class` records `(loader, name)` in `loaded_classes` regardless of
    // the loader-aware-resolution gate, so the exact probe still finds the
    // loader's own copy on later lookups (no duplicate definition); a genuine
    // miss correctly falls through so the loader's own `loadClass` runs.
    if let Some(id) = peek_loader_namespace_id(ctx, this) {
        if let Some(cid) = ctx.class_id_defined_by_loader_exact(internal_name, id) {
            return Some(ctx.get_class_mirror(cid));
        }
    }
    // Real-JDK ClassLoader layouts do not reliably expose the synthetic
    // namespace id used by the class manager. `defineClass` also records the
    // exact defining loader object per ClassId; consult that authoritative
    // relation so a parent fork loader can recover its own already-defined
    // class before delegating to a global same-named copy.
    if let Some(cid) = class_defined_by_this_loader_object(ctx, this, internal_name) {
        return Some(ctx.get_class_mirror(cid));
    }
    // 2. A globally-known class THIS loader is the defining loader of.
    if let Some(cid) = ctx.class_id_by_name(internal_name) {
        if let Some(def) = defining_loader_for(loader_tables(ctx), cid.as_u32()) {
            if def.as_ptr() == this.as_ptr() {
                return Some(ctx.get_class_mirror(cid));
            }
        }
    }
    None
}

pub fn cl_load_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            // HotSpot: a bare NPE (`ConcurrentHashMap.putIfAbsent` on a null key), no message.
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: None,
            }
            .into());
        }
    };

    // `ClassLoader.loadClass(String)` is spec'd as `return loadClass(name, false)`.
    // If the receiver's actual class overrides the public single-argument or protected
    // `loadClass(String,boolean)` with its own bytecode (e.g. Spring's
    // OverridingClassLoader, which redefines eligible classes under itself
    // BEFORE parent delegation, or rejects filtered names), dispatch the virtual
    // `loadClass(name, false)` so that override actually runs. Reimplementing
    // base parent-first delegation here would resolve the class through the
    // global/app class store and ignore the user loader entirely (its custom
    // ordering and defining-loader identity would be lost).
    //
    // `super.loadClass(name, resolve)` from such an override is an invokespecial
    // that lands on the base native `cl_load_class_resolve`
    // (→ `cl_load_class_base_delegation`), so there is no recursion back here.
    if let Some(result) = invoke_single_load_class_override(ctx, this, name_obj) {
        return result;
    }
    if receiver_overrides_load_class_resolve(ctx, this) {
        // gci i18/vp: a tail call (`continuation_pin::tail_call`), exactly the
        // JDK's `return loadClass(name, false)`: the `Class` answer is this
        // native's, unchanged, and nothing runs after it. The override is
        // application code (a loader's own lock, a fetch) that may park,
        // which unmounts a virtual thread on HotSpot. (The single-argument
        // override arm above is NOT a tail call: its in-flight guard is popped
        // after the call.)
        return cratonvm_native_api::continuation_pin::tail_call(|| {
            ctx.invoke_virtual(
                this,
                "loadClass",
                "(Ljava/lang/String;Z)Ljava/lang/Class;",
                &[Value::Object(Some(name_obj)), Value::Int(0)],
            )
        });
    }

    cl_load_class_base_delegation(ctx, this, name_obj)
}

/// Canonical `ClassLoader.loadClass(String)` entry point for interpreter
/// dispatches that must enforce the public null-name contract.
pub fn cl_load_class_essential(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    cl_load_class(ctx, args)
}

pub fn classloader_parent(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
) -> Option<ObjectRef> {
    // The real named `parent` field is populated by name in exactly ONE
    // place (the bootstrap app loader's own construction, see
    // `alloc_classloader`) — every ordinary `ClassLoader`/`URLClassLoader`
    // constructor native (`cl_init_parent`, `cl_init_name_parent`,
    // `ucl_setup`, ...) writes only the numeric `CL_PARENT_REF` slot. For
    // those (the overwhelming majority of real-JDK-mode loaders), a
    // by-name read of "parent" returns a genuinely-null Java field — NOT
    // evidence that the loader has no parent — so it must fall through to
    // the slot, not be trusted as the final answer. Treating that null as
    // definitive made every `URLClassLoader` constructed with a non-null
    // parent (e.g. Spring Boot's `PropertiesLauncher.wrapWithCustomClassLoader`
    // wrapping a `LaunchedClassLoader`) look parentless to
    // `cl_load_class_base_delegation`, which then skipped real parent-first
    // delegation entirely and went straight to the (parentless) global/own-URL
    // fallback — silently losing the parent's classpath.
    // SBR2-LOADERS: a JDK-built `BuiltinClassLoader` carries TWO `parent`
    // fields; the by-name read above finds the derived one, which for the
    // platform loader is the `BootClassLoader` object while
    // `ClassLoader.parent` (what `getParent()` answers) is null. With the
    // built-in loaders adopted, ask the declaring class.
    if builtin_loaders_adopted(loader_tables(&*ctx)) && !cl_has_synthetic_layout(ctx, loader) {
        if let Some(index) = ctx.resolve_field_index("java/lang/ClassLoader", "parent") {
            return match ctx.get_field(loader, index) {
                Value::Object(Some(parent)) => Some(parent),
                _ => None,
            };
        }
    }
    if let Value::Object(Some(parent)) = ctx.get_field_by_name(loader, "parent") {
        return Some(parent);
    }
    // L1: the slot fallback is for OUR layout only. On the real layout slot 1
    // is `java.lang.ClassLoader.name` — a String — so this read handed the
    // platform loader's own name back as its PARENT, and every caller that
    // walks the chain (`builtin_loader_reachable`, `parent_namespace_id`,
    // Tomcat's `while (j.getParent() != null)`) then treated a String as a
    // ClassLoader. On a real layout a null by-name `parent` is the truth:
    // nothing but real bytecode and the by-name writes above ever sets it.
    if !cl_has_synthetic_layout(ctx, loader) {
        return None;
    }
    match ctx.get_field(loader, CL_PARENT_REF) {
        Value::Object(Some(parent)) => Some(parent),
        _ => None,
    }
}

/// True if `loader` can see a class whose defining loader is `defining` — i.e.
/// `defining` is `loader` itself or one of its delegation ancestors (parent
/// chain). JVMS §5.3: a class defined by loader D is visible to L only if L
/// (transitively) delegates to D. CratonVM keeps a single flat global class
/// store, so without this check a loader can resolve an unrelated *sibling*
/// loader's class by name — e.g. `Proxy.getProxyClass(childLoader1, …)` is
/// globally registered as `jdk.proxy1.$Proxy0`, so `childLoader2.loadClass`
/// found it too (HotSpot throws ClassNotFoundException). That made
/// `ClassUtils.isCacheSafe(composite, siblingLoader)` wrongly true via its
/// `isLoadable` fallback. ClassUtilsTests.isCacheSafe.
/// Whether an APPLICATION-tier built-in loader appears in `loader`'s parent
/// chain, including `loader` itself. A `false` answer means the chain never
/// reaches a loader that can see the application classpath, so per JVMS 5.3
/// only bootstrap/platform (JDK module) classes are resolvable through
/// delegation and CratonVM's flat global store -- which conflates every
/// loaded class, including ones only the application loader can see -- must
/// not stand in for delegation here.
///
/// The platform loader does NOT count, even though it is "built-in" (not
/// user-defined): it only sees JDK platform modules, never application
/// classes, so treating it the same as the application loader wrongly let a
/// loader parented ONLY as `UserLoader -> PlatformClassLoader -> bootstrap`
/// (e.g. Spring's `CompileWithForkedClassLoaderClassLoader`, whose whole
/// point is to skip the application loader and mint its OWN fresh copies of
/// non-JDK classes) "see" an application class that was merely already
/// loaded elsewhere in the process. That produced a real, reproducing bug:
/// `SpringFactoriesEnvironmentPostProcessorsFactory` resolved through the
/// flat store to the ORIGINAL application-loader copy instead of the forked
/// loader calling its own `findClass` override to mint an isolated copy —
/// so a `DeferredLogFactory` instance captured against the app-loader
/// `Class` object failed an `ArgumentResolver` type match against a
/// factory's constructor parameter resolved via the forked loader, leaving
/// the parameter null (`NullPointerException` in
/// `CloudFoundryVcapEnvironmentPostProcessor.<init>`, "logFactory" null).
pub fn builtin_loader_reachable(ctx: &mut dyn NativeContext, loader: ObjectRef) -> bool {
    let mut cur = Some(loader);
    for _ in 0..256 {
        let Some(l) = cur else { break };
        if !is_user_defined_loader(ctx, l) && !is_platform_class_loader(ctx, l) {
            return true;
        }
        cur = classloader_parent(ctx, l);
    }
    false
}

pub fn loader_can_see_defining(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    defining: ObjectRef,
) -> bool {
    let mut cur = Some(loader);
    // Bounded walk up the parent chain (defensive cap against cycles).
    for _ in 0..256 {
        let Some(loader) = cur else { break };
        if loader.as_ptr() == defining.as_ptr() {
            return true;
        }
        cur = classloader_parent(ctx, loader);
    }
    false
}

/// Resolve `internal` through CratonVM's global class store, but enforce loader
/// isolation: if the resolved class was defined by a *user-defined* loader that
/// `this` cannot see ([`loader_can_see_defining`]), return `None` so the caller
/// falls through to `findClass` / "not found" instead of leaking another
/// loader's class. Only user-defined defining loaders are recorded in the
/// defining-loader registry (built-in app/platform/bootstrap loaders are not),
/// so the common case — app/JDK classes with no registered defining loader —
/// is unchanged and still resolves permissively.
/// The class mirror for `cid`, unless loader isolation hides it from `this`.
///
/// Enforces isolation for both custom and built-in requesters. The app loader
/// must not report a child loader's class as already globally available; that
/// reverse leak lets sibling BeanShell interpreters reuse the first generated
/// `MyMessenger` instead of defining their own.
pub fn cid_visible_mirror(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    cid: cratonvm_types::ClassId,
) -> Option<ObjectRef> {
    // A class whose defining loader was confirmed collected is gone from
    // every OTHER loader's perspective (real unloading semantics) -- checked
    // BEFORE the live-registry lookup so a pruned entry never falls through
    // to "no restriction, visible to all" (see `is_defining_loader_orphaned`).
    if is_defining_loader_orphaned(loader_tables(ctx), cid.as_u32()) {
        return None;
    }
    if let Some(def) = defining_loader_for(loader_tables(ctx), cid.as_u32()) {
        if !loader_can_see_defining(ctx, this, def) {
            return None;
        }
    }
    Some(ctx.get_class_mirror(cid))
}

/// Real-JDK-mode-analogue diagnostic (see `classloader_real::load_class_visible_to`
/// / `no_class_def_found_error`, fixed 2026-07-17): `ensure_class_initialized`
/// bottoms out in the same `ClassManager::load_class` that propagates a
/// recursive supertype/interface load failure UNCHANGED, so a `ClassNotFound`
/// naming something other than `internal` means the requested class file
/// exists but a *dependency* is missing -- JVMS §5.3/§5.4:
/// `NoClassDefFoundError`, not a bare CNFE on the requested class. Surfaced
/// as an immediate `Err` (rather than folded into the `Ok(None)` "not
/// found" case) so the caller throws it instead of silently returning a
/// null `Class` (this native's genuine-miss contract -- see step 7 of
/// `cl_load_class_base_delegation`).
pub fn resolve_global_if_visible(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    internal: &str,
) -> Result<Option<ObjectRef>, cratonvm_types::error::MethodCallFailed> {
    // gen r4w3/rooting: `ensure_class_initialized` runs `<clinit>` (GC
    // point); re-read `this` before the visibility check uses it.
    let this_pin = ctx.pin_native_root(this);
    let initialized = ctx.ensure_class_initialized(internal);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    match initialized {
        Ok(cid) => Ok(cid_visible_mirror(ctx, this, cid)),
        Err(cratonvm_types::error::MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::ClassFile(
                cratonvm_types::error::ClassFileError::ClassNotFound { class_name },
            ),
        )) if class_name != internal
            && cratonvm_classloading::array_descriptor_element_class(internal)
                != Some(class_name.as_str()) =>
        {
            tracing::debug!(
                requested = internal,
                missing_dependency = %class_name,
                "resolve_global_if_visible: requested class exists but a \
                 dependency failed to resolve -- NoClassDefFoundError"
            );
            Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                crate::classloader_real::no_class_def_found_error(ctx, &class_name)?,
            ))
        }
        Err(e) => {
            tracing::debug!(
                requested = internal,
                error = %e,
                "resolve_global_if_visible: ensure_class_initialized failed"
            );
            Ok(None)
        }
    }
}

/// Base-class `ClassLoader.loadClass` parent-first delegation, reimplemented in
/// Rust (CratonVM keeps no JDK bytecode for `ClassLoader.loadClass`).
///
/// Reached when the receiver does NOT override `loadClass(String,boolean)` — it
/// inherits the base behavior — and also via `super.loadClass(name, resolve)`
/// (invokespecial → the base native `cl_load_class_resolve`) from a subclass
/// override that wants the standard parent-first path as its fallback.
pub fn cl_load_class_base_delegation(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    name_obj: ObjectRef,
) -> MethodCallResult {
    // zt pl4 item 1: a Java `loadClass`, not the VM's upcall (no `CLASS_LOAD`).
    let _java_load = cratonvm_types::perf_class_trace::JavaInitiatedLoad::enter();
    let dotted = ctx.read_string(name_obj).unwrap_or_default();
    let internal = dotted.replace('.', "/");
    let __obsreg_dbg =
        crate::vmflags().loader.dbg_obsreg && internal.contains("ObservationRegistry");
    if __obsreg_dbg {
        let parent = classloader_parent(ctx, this);
        let this_cls = ctx.class_name_of_id(ctx.class_id_of_object(this));
        let parent_cls = parent.map(|p| ctx.class_name_of_id(ctx.class_id_of_object(p)));
        eprintln!(
            "[OBSREG-DBG] cl_load_class_base_delegation ENTER this={:?} this_class={:?} parent={:?} parent_class={:?} name={}",
            this, this_cls, parent, parent_cls, internal
        );
    }
    let __result = cl_load_class_base_delegation_inner(ctx, this, name_obj, &internal);
    if __obsreg_dbg {
        let cid = match &__result {
            Ok(Some(Value::Object(Some(m)))) => Some(ctx.class_id_of_object(*m)),
            _ => None,
        };
        eprintln!(
            "[OBSREG-DBG] cl_load_class_base_delegation EXIT this={:?} name={} -> {:?} (class_id={:?})",
            this, internal, __result, cid
        );
    }
    __result
}

/// GC-SAFETY wrapper. The delegation body below dispatches arbitrary Java
/// several times — the parent's `loadClass`, the receiver's `findClass`
/// override, `ensure_class_initialized`'s `<clinit>`, and exception
/// construction — and keeps using `this` and `name_obj` afterwards. Both are
/// bare Rust locals: `safe_native_call` pins the native's ARGS and the
/// collector remaps those pins, but nothing rewrites these copies. Hibernate
/// reaches exactly this path through `ClassLoaderServiceImpl.classForName` ->
/// `AggregatedClassLoader` (which is `super(null)` and overrides `findClass`
/// to iterate scoped child loaders), which is where `CRATONVM_DBG_STALE_OBJREF`
/// caught a stale deref. Root both for the whole body and re-read them after
/// every dispatch. See
/// map-resize-unpinned-chain-cursors-nojit-segv-20260731-FIXED.md.
pub fn cl_load_class_base_delegation_inner(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    name_obj: ObjectRef,
    internal: &str,
) -> MethodCallResult {
    let this_pin = ctx.pin_native_root(this);
    let name_pin = ctx.pin_native_root(name_obj);
    let result =
        cl_load_class_base_delegation_rooted(ctx, this, this_pin, name_obj, name_pin, internal);
    ctx.unpin_native_roots(this_pin);
    result
}

pub fn cl_load_class_base_delegation_rooted(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    this_pin: usize,
    name_obj: ObjectRef,
    name_pin: usize,
    internal: &str,
) -> MethodCallResult {
    // gen r4w3/rooting: function-scoped `mut` bindings so every post-GC-point
    // re-read below reaches all later uses instead of dying at a block brace.
    let mut this = this;
    let mut name_obj = name_obj;
    let internal = internal.to_string();
    // HIB-CV-24 / SBR-14 -- honor a supplied child/isolated `ClassLoader`.
    //
    // CratonVM stands in for `ClassLoader.loadClass` with this native (it keeps no
    // JDK bytecode for it). The steps below resolve a class through CratonVM's
    // flat global store (`ensure_class_initialized`) BEFORE reaching the
    // `findClass` override (step 4). For a custom loader whose parent is the
    // bootstrap loader (e.g. Hibernate's `AggregatedClassLoader`, which is
    // `super(null)` and overrides `findClass` to iterate scoped child loaders),
    // that global pre-resolution acts like the application loader and bypasses the
    // supplied loader entirely (JVMS §5.3: a bootstrap parent cannot load an
    // application class, so `findClass` MUST run). When such a loader overrides
    // `findClass` and the requested class is NOT a bootstrap/platform class, defer
    // every global short-circuit to AFTER `findClass`. Only for a NULL parent — a
    // non-null (app/platform) parent keeps JVMS parent-first (it legitimately
    // loads the class; `findClass` is not called). Built-in loaders and bootstrap
    // classes keep the permissive global path (CratonVM has no separate bootstrap
    // classpath). Opt-out: `CRATONVM_CL_BOOTSTRAP_SCOPED=0`.
    let parent = classloader_parent(ctx, this);
    let parent_is_null = parent.is_none();
    // The platform loader has the same visibility as bootstrap for application
    // classes: it can load JDK modules, never a test/application class.  In
    // real-JDK mode its Java fields cannot carry CratonVM's synthetic loader
    // type marker, so the old `parent_type` fallback below mistook it for the
    // app loader and leaked a global app class before a child `findClass` got
    // a chance to define its own copy.  Compare the singleton identity instead
    // of inspecting real JDK object fields.
    let parent_is_platform = parent.is_some_and(|candidate| {
        // The JDK can manufacture another PlatformClassLoader object before
        // our native singleton is observed, so identity is a fast path only;
        // the object's actual runtime class is the authoritative fallback.
        ctx.class_name_of_id(ctx.class_id_of_object(candidate))
            .is_some_and(|name| name == "jdk/internal/loader/ClassLoaders$PlatformClassLoader")
            || platform_loader_of(loader_tables(&*ctx))
                .is_some_and(|platform| platform.as_ptr() == candidate.as_ptr())
    });
    let receiver_has_find_class_override = receiver_overrides_find_class(ctx, this);
    let defer_to_find_class = cl_bootstrap_scoped()
        && (parent_is_null || parent_is_platform)
        && !is_bootstrap_class_name(&internal)
        // A jar appended via Instrumentation.appendToBootstrapClassLoaderSearch
        // belongs to the bootstrap loader: parent delegation must serve it
        // BEFORE any findClass override defines a per-loader copy (Mockito
        // asserts its injected MockMethodDispatcher has a null loader).
        && !ctx.is_bootstrap_appended_class(&internal)
        && receiver_has_find_class_override;
    // JVMS 5.3-faithful scoping of the flat-store fallback (same gate as the
    // defer logic above): CratonVM's global store stands in for "the app
    // classpath, reachable through the parent chain". A loader whose REAL
    // parent chain never passes through a built-in loader (e.g.
    // `new ClassLoader(null) {}`, or a loader parented to such) can only see
    // bootstrap classes on HotSpot; answering an application class from the
    // flat store bypasses the loader's own fallback logic (seen:
    // ThrowawayClassLoader.loadClassFromResource never ran because
    // super.loadClass resolved the probe class globally, failing its
    // stream-closing contract test). Loaders with a findClass override keep
    // their step-6 rescue below, so only override-less chains change.
    let scoped_user_chain = cl_bootstrap_scoped()
        && !is_bootstrap_class_name(&internal)
        && !ctx.is_bootstrap_appended_class(&internal)
        && !builtin_loader_reachable(ctx, this);
    // JVM spec §5.3.2 — parent-first delegation:
    // 1. Check if this loader already loaded the class (findLoadedClass)
    let loader_type = loader_type_of(ctx, this).unwrap_or(LOADER_APP);

    // A user-defined loader must always return a class it has already
    // defined before delegating to its parent. In real-JDK mode the
    // synthetic loader-type slot is unavailable, so the legacy branch below
    // can misclassify it as an application loader and skip this check; that
    // leaks a same-named global class through a forked parent loader.
    if is_user_defined_loader(ctx, this) {
        if let Some(mirror) = find_loaded_class_for_loader(ctx, this, &internal) {
            return Ok(Some(Value::Object(Some(mirror))));
        }
        // The read-only lookup above intentionally avoids allocating a
        // namespace.  A real-JDK ClassLoader may not expose the synthetic
        // id field even though `defineClass` has already registered classes
        // through `loader_namespace_id`; ask that same authoritative mapping
        // before falling through to the parent/global store.
        let loader_id = loader_namespace_id(ctx, this);
        if let Some(cid) = ctx.class_id_by_name_and_loader(&internal, loader_id) {
            if let Some(mirror) = cid_visible_mirror(ctx, this, cid) {
                return Ok(Some(Value::Object(Some(mirror))));
            }
        }
    }

    // For synthetic-mode custom loaders, check own namespace first.
    if loader_type == LOADER_CUSTOM {
        let loader_id = loader_id_of(ctx, this);
        if let Some(lid) = loader_id {
            if let Some(cid) = ctx.class_id_by_name_and_loader(&internal, lid) {
                if let Some(mirror) = cid_visible_mirror(ctx, this, cid) {
                    return Ok(Some(Value::Object(Some(mirror))));
                }
            }
        }
    }

    // A URLClassLoader parented only by bootstrap/platform is intentionally
    // isolated from application entries. Spring Boot's
    // ModifiedClassPathClassLoader uses this topology to remove selected JARs
    // from a test's classpath. Do this before the flat global-store fallback:
    // otherwise `super.loadClass` from its override returns the application
    // loader's same-named class, and Spring's ASM metadata resolves annotation
    // types through a different loader than the condition class itself.
    //
    // Keep bootstrap-appended classes on their normal parent-delegation path;
    // Mockito's injected dispatcher must remain bootstrap-defined.
    // In real-JDK mode an inherited URLClassLoader relationship is not always
    // visible through `object_extends` while dispatching a subclass override.
    // Its constructor URLs are retained independently and are the authoritative
    // signal that this loader has a private URL search path.
    let has_private_url_path = !loader_constructor_url_paths(ctx, this).is_empty();
    if (url_classloader_isolated_from_app(ctx, this)
        || ((parent_is_null || parent_is_platform) && has_private_url_path))
        && !is_bootstrap_class_name(&internal)
        && !ctx.is_bootstrap_appended_class(&internal)
    {
        if let Some(result) = ucl_try_define_local_class(ctx, this, &internal) {
            return result;
        }
        let exception = crate::jboss_module_loader::alloc_single_message_exception(
            ctx,
            "java/lang/ClassNotFoundException",
            1,
            &internal,
        );
        return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
            exception?,
        ));
    }

    // 2. Delegate to parent loader first (recursive parent-first delegation)
    if let Some(parent) = parent {
        // Recursively delegate to parent by calling its loadClass
        let parent_type = loader_type_of(ctx, parent).unwrap_or(LOADER_APP);
        let parent_lid = loader_id_of(ctx, parent);
        // Check parent's namespace for custom loaders. Apply loader-isolation:
        // a sibling custom loader's class (e.g. a generated proxy that leaked
        // into the app-loader namespace but whose registered defining loader is
        // an unrelated child) must not be handed to `this`. ClassUtilsTests
        // .isCacheSafe via `isLoadable`.
        if let Some(pid) = parent_lid {
            if let Some(cid) = ctx.class_id_by_name_and_loader(&internal, pid) {
                if let Some(mirror) = cid_visible_mirror(ctx, this, cid) {
                    return Ok(Some(Value::Object(Some(mirror))));
                }
            }
        }
        // A user-defined parent has its own delegation and `findClass`
        // behavior. In real-JDK mode its internal loader-type fields are not
        // available to this native, so treating it like a built-in parent and
        // consulting the flat global store first can return an unrelated
        // same-named application class. Invoke the parent's actual loadClass
        // before any global fallback, exactly as parent-first delegation
        // requires (notably DynamicClassLoader -> forked test loader).
        if is_user_defined_loader(ctx, parent) {
            let delegated = ctx.invoke_virtual(
                parent,
                "loadClass",
                "(Ljava/lang/String;)Ljava/lang/Class;",
                &[Value::Object(Some(name_obj))],
            );
            // The dispatch above ran arbitrary Java: everything captured
            // before it is a pre-move address on the fall-through path.
            // gen r4w3/rooting: ASSIGN (not shadow) so the refresh reaches
            // steps 2-6 below, outside this block.
            this = ctx.read_native_pin(this_pin, this);
            name_obj = ctx.read_native_pin(name_pin, name_obj);
            match delegated {
                Ok(Some(Value::Object(Some(mirror)))) => {
                    return Ok(Some(Value::Object(Some(mirror))));
                }
                // W7-26 R1 -- the synthetic-mode twin of the narrowing applied to
                // `classloader_real.rs`'s step 0. JDK 25 `ClassLoader.loadClass`
                // wraps its parent delegation in exactly one `catch
                // (ClassNotFoundException)`; the bare `_ =>` also caught a
                // `LinkageError` and every `RuntimeException` the parent raised
                // and reported the class as merely absent, turning a diagnosable
                // failure into a wrong answer. `absorb_class_absent` keeps the
                // fall-through for the two class-absent shapes only, tested by
                // `ClassId` hierarchy rather than by name.
                //
                // The two `read_native_pin` refreshes above this `match` are
                // GC-correctness, not style: the `invoke_virtual` ran arbitrary
                // Java and every address captured before it is a pre-move one on
                // the fall-through path.
                Ok(_) => {}
                Err(failed) => {
                    crate::classloader_real::absorb_class_absent(&*ctx, failed)?;
                }
            }
        }
        // For built-in parent loaders (bootstrap/platform/app), use standard delegation
        if parent_type != LOADER_CUSTOM && !defer_to_find_class && !scoped_user_chain {
            // Standard delegation handles bootstrap → extension → app
            if let Some(mirror) = resolve_global_if_visible(ctx, this, &internal)? {
                return Ok(Some(Value::Object(Some(mirror))));
            }
            // gen r4w3/rooting: `<clinit>` may have run; refresh.
            this = ctx.read_native_pin(this_pin, this);
        }
    } else if !defer_to_find_class && !scoped_user_chain {
        // No parent (or null parent) → delegate directly to bootstrap loader
        // Bootstrap delegation: use the standard class loading chain
        if let Some(mirror) = resolve_global_if_visible(ctx, this, &internal)? {
            return Ok(Some(Value::Object(Some(mirror))));
        }
        // gen r4w3/rooting: `<clinit>` may have run; refresh.
        this = ctx.read_native_pin(this_pin, this);
    }

    // 3. Parent couldn't find it — fall back to standard loading
    //    (this covers bootstrap → extension → application delegation).
    //    Skipped when deferring to a custom `findClass` override (HIB-CV-24) so
    //    the supplied loader runs before CratonVM's global store answers.
    if !defer_to_find_class && !scoped_user_chain {
        if let Some(mirror) = resolve_global_if_visible(ctx, this, &internal)? {
            return Ok(Some(Value::Object(Some(mirror))));
        }
        // gen r4w3/rooting: `<clinit>` may have run; refresh.
        this = ctx.read_native_pin(this_pin, this);
    }

    // URLClassLoader searches its recorded URLs after parent delegation. Its
    // entries deliberately are not appended to the process-wide application
    // path, because that would make one temporary loader's classes and
    // resources visible to another.
    // Some real-JDK subclasses do not expose their inherited
    // URLClassLoader identity through `object_extends` during native
    // dispatch. The helper is a no-op for receivers without recorded URLs,
    // so probe it directly rather than dropping their isolated path.
    if let Some(result) = ucl_try_define_local_class(ctx, this, &internal) {
        return result;
    }
    // gen r4w3/rooting: the local URL search may fetch / define (GC-capable)
    // even on a miss; refresh both locals before `findClass`.
    this = ctx.read_native_pin(this_pin, this);
    name_obj = ctx.read_native_pin(name_pin, name_obj);

    // 4. Custom-classloader extension point. The JVM `ClassLoader.loadClass`
    //    contract is: after parent delegation fails, call `findClass(name)`.
    //    `findClass` is the documented override hook — application code
    //    (Eclipse Equinox OSGi, custom loaders) subclasses `ClassLoader`
    //    and overrides `findClass` to load from a custom source. Since this
    //    Rust native stands in for `ClassLoader.loadClass` (CratonVM keeps
    //    no JDK bytecode for it), the native must perform the virtual
    //    `findClass` dispatch itself so the user override actually runs.
    //
    //    Guarded by `receiver_overrides_find_class` so this only fires for
    //    genuine non-builtin subclasses — a built-in loader has no override
    //    and the callback would recurse back into `cl_find_class`.
    //
    //    Note: unlike the original `return`, we fall through on failure so
    //    the IMPL-JARS fallback (step 5) can still fire when `findClass`
    //    throws ClassNotFoundException (e.g. EmbeddedImplClassLoader with
    //    empty jarMetas).
    if receiver_overrides_find_class(ctx, this) {
        // The base `ClassLoader.findClass` itself is registered as a native.
        // A regular virtual call can therefore re-enter that native through
        // the inherited declaration and bypass this known subclass override.
        // The predicate above proves a real bytecode implementation exists on
        // the receiver hierarchy; select that implementation explicitly.
        let result = ctx.invoke_virtual_bytecode_only(
            this,
            "findClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[Value::Object(Some(name_obj))],
        );
        // `findClass` is the loader's own Java override (Hibernate's
        // AggregatedClassLoader iterates its scoped child loaders here) —
        // refresh before the fall-through arms reuse either local.
        // gen r4w3/rooting: ASSIGN so the refreshed `this` reaches steps 5/6
        // (`impl_jars_load_class`) below, outside this block.
        this = ctx.read_native_pin(this_pin, this);
        match result {
            Ok(Some(Value::Object(Some(_)))) => return result,
            // A miss from URLClassLoader's own native URL/HTTP search is
            // authoritative -- propagate it (e.g. ClassNotFoundException)
            // rather than falling through to step 6's global-store fallback,
            // which would let a null-parent URLClassLoader resolve
            // application classes its own (failed) URL search should have
            // hidden from it. See docs/feature-designs/native-dispatch-architecture.md.
            _ if defer_to_find_class && find_class_is_urlclassloader_native(ctx, this) => {
                return result
            }
            // findClass threw (ClassNotFoundException) or returned null — fall through.
            _ => {}
        }
    }

    // 5. IMPL-JARS fallback: ES EmbeddedImplClassLoader stores provider
    //    classes and all their inner/helper classes as individual ZIP entries
    //    under IMPL-JARS/<module>/<jar_dir>/<classfile> inside the outer
    //    module JAR. When neither the flat classpath nor findClass can locate
    //    the class, try scanning those entries directly. Only for that loader
    //    (`impl_jars_fallback_applies`; interpreter round i2 wave 54, lane L5).
    if crate::service_loader::impl_jars_fallback_applies(ctx, this) {
        if let Ok(Some(mirror)) = impl_jars_load_class(ctx, Some(this), &internal) {
            return Ok(Some(Value::Object(Some(mirror))));
        }
    }

    // 6. Deferred-resolution last resort (HIB-CV-24). When `defer_to_find_class`
    //    skipped the global short-circuits above and the loader's own `findClass`
    //    did not produce the class, CratonVM's flat store is still the only source
    //    of application classes — resolve here so a findClass-overriding loader
    //    whose override legitimately misses (delegating the actual load elsewhere)
    //    does not spuriously fail a class the runtime can provide.
    if defer_to_find_class {
        if let Ok(cid) = ctx.ensure_class_initialized(&internal) {
            let mirror = ctx.get_class_mirror(cid);
            return Ok(Some(Value::Object(Some(mirror))));
        }
    }

    // 7. Not found and no user override — class genuinely missing.
    Ok(Some(Value::Object(None)))
}

pub fn cl_load_class_resolve(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Base `ClassLoader.loadClass(String,boolean)` — boolean resolve arg is
    // ignored (we always resolve). This is the native for the base class only;
    // a subclass override of this method runs its own bytecode (it shadows the
    // inherited native), so reaching here means the receiver uses base
    // parent-first delegation. Must call the base delegation DIRECTLY (not
    // `cl_load_class`) so that `super.loadClass(name, resolve)` from a subclass
    // override does not bounce back into the override-dispatch and recurse.
    let this = obj_arg(args, 0)?;
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            // HotSpot: a bare NPE (`ConcurrentHashMap.putIfAbsent` on a null key), no message.
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: None,
            }
            .into());
        }
    };
    cl_load_class_base_delegation(ctx, this, name_obj)
}

pub fn cl_find_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Base `ClassLoader.findClass(String)`. CratonVM reuses the parent-first
    // delegation as a permissive base findClass (covers the IMPL-JARS fallback).
    // Routed to the base delegation directly so it never triggers the
    // `loadClass(String,boolean)` override-dispatch (which would be wrong for
    // findClass and could recurse via a subclass `super.findClass`).
    let this = obj_arg(args, 0)?;
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    cl_load_class_base_delegation(ctx, this, name_obj)
}

pub fn cl_find_class_module(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // module-aware variant — module arg (index 1) ignored, class name at index 2
    let this = obj_arg(args, 0)?;
    let module_name = args.get(1).copied().unwrap_or(Value::Object(None));
    let name_obj = match args.get(2) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let dotted = ctx.read_string(name_obj).unwrap_or_default();
    let internal = dotted.replace('.', "/");

    // The base overload is native, but a custom class loader can override the
    // module-aware variant. Elasticsearch's EmbeddedImplClassLoader does so
    // for its IMPL-JARS, therefore this inherited-method path must preserve
    // virtual dispatch before falling back to the flat classpath.
    // gen r4w3/rooting: the `findClass` upcall, the IMPL-JARS define and
    // `ensure_class_initialized` (`<clinit>`) are all GC points and `this` is
    // used after each; keep it rooted and re-read.
    let this_pin = ctx.pin_native_root(this);
    if receiver_overrides_find_class(ctx, this) {
        let result = ctx.invoke_virtual_bytecode_only(
            this,
            "findClass",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/Class;",
            &[module_name, Value::Object(Some(name_obj))],
        );
        if matches!(result, Ok(Some(Value::Object(Some(_))))) {
            ctx.unpin_native_roots(this_pin);
            return result;
        }
    }
    let this = ctx.read_native_pin(this_pin, this);

    // A module-aware lookup can be the first request for an embedded
    // implementation dependency, so share loadClass/findClass(String)'s
    // IMPL-JARS fallback here as well (Elasticsearch's loader only,
    // `impl_jars_fallback_applies`).
    if crate::service_loader::impl_jars_fallback_applies(ctx, this) {
        if let Ok(Some(mirror)) = impl_jars_load_class(ctx, Some(this), &internal) {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(Value::Object(Some(mirror))));
        }
    }

    let initialized = ctx.ensure_class_initialized(&internal);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    match initialized {
        Ok(cid) => {
            // `BuiltinClassLoader` (the app/platform loader) calls this 2-arg
            // `findClass(module, name)` during its module/classpath search. The
            // global resolve above ignores loader identity, so a generated proxy
            // defined by an unrelated child loader would leak through here — this
            // is the path that defeated the `findLoadedClass` guard in
            // ClassUtilsTests.isCacheSafe. Hide cross-loader proxies.
            if proxy_hidden_from(ctx, this, &internal, cid) {
                return Ok(Some(Value::Object(None)));
            }
            let mirror = ctx.get_class_mirror(cid);
            Ok(Some(Value::Object(Some(mirror))))
        }
        Err(_) => Ok(Some(Value::Object(None))),
    }
}

// T19_H12_LOADCLASS_MODULE — `ClassLoader.loadClass(Module, String)Class`.
//
// JDK 25 (post-JEP 261) introduces this package-private overload to support
// `Class.forName(Module, String)`. JDK's bytecode for the public
// `Class.forName(Module, String)` resolves `module.getClassLoader()` and
// dispatches a virtual `loadClass(Module, String)` against the result.
// Without this native, the dispatch falls through to whatever the
// receiver's actual class is — which on our synthetic Module objects
// can be a `HashSet` (because the Module's slot 2 holds the packages set
// when JDK bytecode reads it as the `loader` field). Registering a
// real native at the receiver's actual class isn't reachable; we
// register at `java/lang/ClassLoader` so the method is at least
// resolvable when the receiver IS a real ClassLoader.
//
// Per spec: returns null if the class is not visible to the module.
pub fn cl_load_class_module(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = this (ClassLoader)
    // args[1] = Module
    // args[2] = String name
    let _this = obj_arg(args, 0)?;
    match args.get(1) {
        Some(Value::Object(Some(_))) => {}
        _ => return Ok(Some(Value::Object(None))),
    };
    let name_obj = match args.get(2) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let dotted = ctx.read_string(name_obj).unwrap_or_default();
    if dotted.is_empty() {
        return Ok(Some(Value::Object(None)));
    }
    // Hardening: reject names with control bytes or path separators.
    if dotted.bytes().any(|b| b < 0x20 || b == 0x7F)
        || dotted.contains('/')
        || dotted.contains('\\')
    {
        return Ok(Some(Value::Object(None)));
    }
    let internal = dotted.replace('.', "/");
    match ctx.ensure_class_initialized(&internal) {
        Ok(cid) => {
            if ctx.is_class_hidden(cid) {
                return Ok(Some(Value::Object(None)));
            }
            let mirror = ctx.get_class_mirror(cid);
            Ok(Some(Value::Object(Some(mirror))))
        }
        // Spec: null on miss, not CNFE.
        Err(_) => Ok(Some(Value::Object(None))),
    }
}

/// Java class file magic number.
pub const CLASS_FILE_MAGIC: [u8; 4] = [0xCA, 0xFE, 0xBA, 0xBE];

// ---------------------------------------------------------------------------
// cglib SEGV guard — Round-17 (this agent)
//
// cglib's proxy generator (used by Spring AOP / Hibernate persistence)
// emits bytecode at runtime via ASM, then hands it to one of:
//   * `ClassLoader.defineClass(String,byte[],int,int[,ProtectionDomain])`
//   * `ClassLoader.defineClass1/2/0` (JDK-internal natives)
//   * `sun.misc.Unsafe.defineClass(...)` (legacy / fallback path)
//
// The generated bytecode references our synthetic JDK classes whose
// field/method layouts don't match what cglib's ASM emitter assumes.
// When `define_class_full` accepts those bytes and the resulting class
// is later verified / linked, the layout mismatch triggers a native
// SEGV (rc=139) inside the interpreter — not a clean Java exception.
//
// Strategy: short-circuit BEFORE handing the bytes to `define_class_full`.
// If the class name (taken from the explicit argument when present, or
// scanned from the bytecode this_class entry as a fallback) looks like
// a cglib proxy, return `null` from the native and let the Java caller
// observe an NPE / LinkageError — which IS recoverable, unlike a SEGV.
//
// Pattern matching is STRICT: only actual cglib-generated proxy classes
// trigger the short-circuit. Frameworks like Quarkus/ASM use similar
// naming conventions in unrelated libraries, so we require the full
// `$$EnhancerByCGLIB$$` token (not just a `ByCGLIB$$` substring) or a
// hit under the `net/sf/cglib/proxy/` subpackage.
// ---------------------------------------------------------------------------

/// Returns true when `name` (in JVM internal form, slashes not dots,
/// may be empty) looks like a cglib-generated proxy.
///
/// STRICT matching: we require the canonical cglib enhancer marker
/// `$$EnhancerByCGLIB$$` (with BOTH leading and trailing `$$` delimiters)
/// or a prefix under `net/sf/cglib/proxy/`. Looser substring matches such
/// as `ByCGLIB$$` previously misfired on Quarkus/ASM-emitted classes whose
/// constant pool contained a similar fragment, routing them through the
/// placeholder path — which returns `java/lang/Object` and caused
/// downstream layout-mismatch SEGVs on keycloak startup.
///
/// The `$$EnhancerByCGLIB$$` token is a literal cglib code-generation
/// marker; no legitimate non-cglib class name contains it. The
/// `net/sf/cglib/proxy/` prefix is the cglib library's own package.
/// Either match is sufficient to identify a cglib-generated proxy.
pub fn is_cglib_proxy_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    name.contains("$$EnhancerByCGLIB$$") || name.starts_with("net/sf/cglib/proxy/")
}

/// Returns a `Class` mirror to use as a stand-in when we short-circuit a
/// cglib proxy define. We prefer `java/lang/Object` (loaded by every VM
/// boot) so callers get back a non-null mirror; if that lookup fails we
/// fall through to `Value::Object(None)` and let the Java caller take an
/// NPE. Either outcome is preferable to the native SEGV.
///
/// Safety: this function NEVER dereferences a raw pointer. The mirror is
/// only constructed when `class_id_by_name` returns `Some(cid)` for
/// `java/lang/Object` — a class loaded by every VM boot. On the null
/// path the caller receives `Value::Object(None)` and surfaces an NPE,
/// which is a recoverable Java-level outcome rather than a native crash.
pub fn cglib_placeholder_mirror(ctx: &mut dyn NativeContext) -> Value {
    if let Some(cid) = ctx.class_id_by_name("java/lang/Object") {
        return Value::Object(Some(ctx.get_class_mirror(cid)));
    }
    Value::Object(None)
}

/// Decide whether to short-circuit a `defineClass*` call for cglib.
/// Returns `Some(placeholder_value)` to short-circuit, or `None` to let
/// the normal path proceed.
///
/// `name` is the (possibly empty) explicit name in JVM internal form.
/// `bytes` is the raw class file slice (currently unused — the bytecode
/// sniff fallback was removed because it produced false positives /
/// SIGILL on malformed buffers; see git history).
///
/// This short-circuit is ALWAYS ON (no env gate). The name match is
/// strict enough (`$$EnhancerByCGLIB$$` literal token or
/// `net/sf/cglib/proxy/` package prefix) that false positives on
/// legitimate Quarkus/Keycloak classes are not possible.
pub fn cglib_guard_value(ctx: &mut dyn NativeContext, name: &str, _bytes: &[u8]) -> Option<Value> {
    // Strict-name match only. We do NOT sniff bytecode — the previous
    // `sniff_class_file_this_name` fallback was a defensive class-file
    // parser, but bytecode parsing on adversarial / truncated buffers
    // had a history of OOB reads and SIGILL (notably keycloak startup).
    // If the caller did not pass a `name`, we let the normal define
    // path handle it; if those bytes are a real cglib proxy CratonVM
    // will SEGV (the original problem) but at least we cannot regress
    // unrelated apps by mis-classifying their bytecode.
    if is_cglib_proxy_name(name) {
        tracing::warn!("[cglib-shim] short-circuiting defineClass for {name} (SEGV avoidance)");
        return Some(cglib_placeholder_mirror(ctx));
    }
    None
}

pub fn cl_define_class_basic(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    use cratonvm_types::error::{LinkageError, RuntimeError};

    // defineClass(String name, byte[] b, int off, int len)
    // args: [this, name, byte_array, offset, length]
    //
    // WP2.3: routes through `define_class_full` so all defineClass
    // entry points share the same backend (name-mismatch check,
    // dup-define rejection, hidden-flag handling, PD attribution).
    let this = obj_arg(args, 0)?;

    // Extract class name (may be null — use name from class file)
    let name_str = match args.get(1) {
        Some(Value::Object(Some(name_obj))) => {
            let dotted = ctx.read_string(*name_obj).unwrap_or_default();
            dotted.replace('.', "/")
        }
        _ => String::new(),
    };

    // Extract byte array, offset, length
    let byte_array = match args.get(2) {
        Some(Value::Object(Some(arr))) => *arr,
        _ => {
            tracing::warn!("ClassLoader.defineClass({name_str}): null bytecode array");
            return Err(RuntimeError::NullPointerException {
                message: Some("ClassLoader.defineClass: bytecode array must not be null".into()),
            }
            .into());
        }
    };

    let array_len = ctx.array_length(byte_array);

    // Safe integer handling: reject negative offset/length (i32 → usize)
    let offset = match args.get(3) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        Some(Value::Int(_)) => return Err(RuntimeError::aioobe_index_only(-1).into()),
        _ => 0,
    };
    let length = match args.get(4) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        Some(Value::Int(_)) => return Err(RuntimeError::aioobe_index_only(-1).into()),
        _ => array_len,
    };

    // Bounds validation: ensure offset + length doesn't exceed array.
    // Use checked_add so a pathological (offset=usize::MAX, length=N)
    // pair cannot wrap around into an in-range value.
    if offset
        .checked_add(length)
        .map_or(true, |end| end > array_len)
    {
        tracing::warn!(
            "[define_class] bounds violation: offset={offset} length={length} \
             array_len={array_len} (name={name_str})"
        );
        return Err(RuntimeError::aioobe_index_only(-1).into());
    }

    // Read bytes from the array.
    //
    // Defensive: wrap the copy loop in `catch_unwind` so a panic inside
    // `get_array_element` (e.g. cglib emitting a large bytecode buffer
    // that hits a stale array layout) does NOT propagate to SIGABRT.
    // `AssertUnwindSafe` is required because `ctx` is `&mut`; the loop
    // performs read-only access on a separate array object so unwinding
    // does not leave shared state observably partial.
    let class_bytes_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        read_class_bytes_bulk(ctx, byte_array, offset, length)
    }));
    let class_bytes = match class_bytes_result {
        Ok(b) => b,
        Err(_) => {
            tracing::error!(
                "[define_class] panic while reading byte array for {name_str}; aborting"
            );
            return Err(cratonvm_types::error::LinkageError::ClassFormatError {
                class_name: name_str.clone(),
                message: "defineClass: panic while reading bytecode array".into(),
            }
            .into());
        }
    };

    // Pre-validate the class file header so that obviously-bad bytes
    // never reach `define_class_full` (cheap CAFEBABE magic check), with
    // HotSpot's `ClassFileParser` text (`classfile_parser_header_refusal`).
    if class_bytes.len() < 8 || class_bytes[0..4] != CLASS_FILE_MAGIC {
        tracing::warn!("[define_class] invalid magic for {name_str}; rejecting");
        return Err(classfile_parser_header_refusal(
            ctx,
            &name_str,
            &class_bytes,
        ));
    }

    // cglib SEGV guard: short-circuit proxy classes BEFORE handing the
    // bytes to `define_class_full`. See `cglib_guard_value` for details.
    if let Some(v) = cglib_guard_value(ctx, &name_str, &class_bytes) {
        return Ok(Some(v));
    }

    // The caller's ProtectionDomain, decoded through the ONE reader that
    // understands both PD shapes. Six copies of an inline decode used to
    // stand here, and all six read `CodeSource.location` with
    // `read_string` -- which fails on a real `java.net.URL`, a different
    // concrete class -- so every real-JDK-constructed CodeSource silently
    // lost its URL and the defined class came back carrying the
    // synthesised `file:/runtime-defined/<name>.class` instead of the
    // caller's. See `extract_pd_code_source_url`.
    // gen r4w3/rooting: `define_class_full` below resolves supertypes (GC
    // point) and `this` is used afterwards to record the defining loader;
    // root it here and re-read after each step that can collect.
    let this_pin = ctx.pin_native_root(this);
    let pd_url = match args.get(5) {
        Some(Value::Object(Some(pd))) => extract_pd_code_source_url(ctx, *pd),
        _ => None,
    };
    let this = ctx.read_native_pin(this_pin, this);

    // Define via the shared backend. Empty name = use class file's
    // own this_class. Loader id 0 = application loader.
    let loader_id = get_or_assign_loader_id(ctx, this);
    let mut opts = cratonvm_native_api::DefineClassFull {
        code_source_url: pd_url,
        ..Default::default()
    };
    // zs s8/nd7: the mirror object before the define (`this` is pinned and
    // re-read below; nothing else raw is used after this point).
    let reservation = match crate::lang_system::reserve_mirror_for_define(
        ctx,
        &crate::lang_system::define_name_hint(&name_str, &class_bytes),
        &mut opts,
    ) {
        Ok(r) => r,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    // Wrap the backend call in `catch_unwind` so a panic inside
    // `define_class_full` (e.g. malformed bytecode that defeats the
    // verifier's bounds-checks) returns null instead of SIGABRT.
    let define_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ctx.define_class_full(&name_str, &class_bytes, loader_id, opts)
    }));
    let define_result = match define_result {
        Ok(r) => r,
        Err(_) => {
            tracing::error!(
                "[define_class] panic inside define_class_full for {name_str}; aborting"
            );
            // zs s9/nd8 (rv6 residual 5): release `this_pin` and, above it,
            // the mirror reservation, as the `Err(msg)` arm does.
            ctx.unpin_native_roots(this_pin);
            return Err(cratonvm_types::error::LinkageError::ClassFormatError {
                class_name: name_str.clone(),
                message: "defineClass: panic inside backend (likely malformed bytecode)".into(),
            }
            .into());
        }
    };
    match define_result {
        Ok(cid) => {
            // Record the exact defining ClassLoader instance so
            // `Class.getClassLoader()` returns THIS loader rather than the
            // app-loader fallback. The public `defineClass(...)` overloads
            // (this native + its PD / ByteBuffer delegators) must do this just
            // like the JDK-internal `defineClass1` does — otherwise a class a
            // custom loader defines (e.g. Spring's OverridingClassLoader
            // redefining an eligible class under itself) would report the wrong
            // loader and classloader-isolation patterns silently break.
            // gen r4w3/rooting: re-read after `define_class_full`.
            let this = ctx.read_native_pin(this_pin, this);
            crate::classloader::register_defining_loader(loader_tables(ctx), cid.as_u32(), this);
            let count = loader_classes_loaded_of(ctx, this).unwrap_or(0);
            loader_set_classes_loaded(ctx, this, count + 1);
            // zs s8/nd7: the reservation sits above `this_pin` (released
            // with it on the failure arms); read it back right before the
            // mint.
            let reserved = crate::lang_system::take_mirror_reservation(ctx, reservation);
            ctx.unpin_native_roots(this_pin);
            let mirror = crate::lang_system::defined_class_mirror_reserved(ctx, cid, reserved);
            Ok(Some(Value::Object(Some(mirror))))
        }
        Err(msg) => {
            ctx.unpin_native_roots(this_pin);
            if msg.starts_with("Runtime(OutOfMemoryError") {
                // gc-common w6-f: `-XX:MaxMetaspaceSize` refused the define
                // (`vm_exec.rs::define_class_full`) -- throw, as HotSpot does,
                // instead of returning null.
                return Err(RuntimeError::OutOfMemoryError {
                    message: "Metaspace".into(),
                }
                .into());
            }
            tracing::warn!("ClassLoader.defineClass({name_str}) failed: {msg}");
            Ok(Some(Value::Object(None)))
        }
    }
}

pub fn cl_define_class_pd(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // defineClass(String name, byte[] b, int off, int len, ProtectionDomain pd)
    // Same backend as cl_define_class_basic — the basic variant already
    // reads the optional PD argument at index 5 when present.
    cl_define_class_basic(ctx, args)
}

/// `ClassLoader.defineClass(String name, ByteBuffer b, ProtectionDomain pd)`
/// for the synthetic JDK (a real JDK runs its own bytecode body, which beats
/// this registration). Interpreter round i5 wave 101, lane L5: this handed its
/// `[this, name, b, pd]` straight to [`cl_define_class_basic`], which read the
/// `ByteBuffer` OBJECT as the `byte[]` (its array length, its "elements") and
/// the `ProtectionDomain` slot as the offset, so a buffer define read garbage.
/// Now it does what JDK 25's body does: the `remaining()` bytes from
/// `position()`; a heap buffer with an array and a direct buffer keep their
/// position, a read-only heap buffer (no accessible array) has it advanced by
/// `b.get(tb)`; a null buffer is the body's `b.remaining()` NPE.
pub fn cl_define_class_bb(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use cratonvm_types::error::RuntimeError;
    let this = obj_arg(args, 0)?;
    let object_at = |i: usize| match args.get(i) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let (name, pd) = (object_at(1), object_at(3));
    let Some(bb) = object_at(2) else {
        return Err(RuntimeError::NullPointerException {
            message: Some(
                "Cannot invoke \"java.nio.ByteBuffer.remaining()\" because \"<parameter2>\" is null"
                    .into(),
            ),
        }
        .into());
    };
    // Every reference crosses Java calls below: pinned, read back after each.
    let base = ctx.pin_native_root(this);
    let name_pin = name.map(|n| (n, ctx.pin_native_root(n)));
    let bb_pin = ctx.pin_native_root(bb);
    let pd_pin = pd.map(|p| (p, ctx.pin_native_root(p)));
    let result = define_class_from_buffer(ctx, (this, base), name_pin, (bb, bb_pin), pd_pin);
    ctx.unpin_native_roots(base);
    result
}

/// [`cl_define_class_bb`] with its references pinned: each `(object, pin)`.
pub fn define_class_from_buffer(
    ctx: &mut dyn NativeContext,
    this: (ObjectRef, usize),
    name: Option<(ObjectRef, usize)>,
    bb: (ObjectRef, usize),
    pd: Option<(ObjectRef, usize)>,
) -> MethodCallResult {
    // Every answer comes from the buffer's own Java methods, never from its
    // fields or a derived view (interpreter round i5 wave 105, lane L5): the
    // wave-101 body read heap-array and direct buffers through `duplicate()`,
    // and a synthetic-JDK view without resolvable storage then aborted the VM
    // from inside `get(byte[])` (`internal error: ByteBuffer missing backing
    // storage`). Same order of calls as JDK 25's body.
    fn int_answer(v: Option<Value>) -> i32 {
        match v {
            Some(Value::Int(n)) => n,
            _ => 0,
        }
    }
    let now = ctx.read_native_pin(bb.1, bb.0);
    let len = int_answer(ctx.invoke_virtual(now, "remaining", "()I", &[])?).max(0) as usize;
    let now = ctx.read_native_pin(bb.1, bb.0);
    let direct = int_answer(ctx.invoke_virtual(now, "isDirect", "()Z", &[])?) != 0;
    if !direct {
        let now = ctx.read_native_pin(bb.1, bb.0);
        let has_array = int_answer(ctx.invoke_virtual(now, "hasArray", "()Z", &[])?) != 0;
        if has_array {
            // `defineClass(name, b.array(), b.position() + b.arrayOffset(),
            // len, pd)`: the buffer's own array, no copy, position kept.
            // `array()` last, so no GC point lies between it and the define.
            let now = ctx.read_native_pin(bb.1, bb.0);
            let pos = int_answer(ctx.invoke_virtual(now, "position", "()I", &[])?);
            let now = ctx.read_native_pin(bb.1, bb.0);
            let off = int_answer(ctx.invoke_virtual(now, "arrayOffset", "()I", &[])?);
            let now = ctx.read_native_pin(bb.1, bb.0);
            if let Some(Value::Object(Some(arr))) =
                ctx.invoke_virtual(now, "array", "()[B", &[])?
            {
                let define_args = [
                    Value::Object(Some(ctx.read_native_pin(this.1, this.0))),
                    Value::Object(name.map(|(n, pin)| ctx.read_native_pin(pin, n))),
                    Value::Object(Some(arr)),
                    Value::Int(pos.saturating_add(off)),
                    Value::Int(len as i32),
                    Value::Object(pd.map(|(p, pin)| ctx.read_native_pin(pin, p))),
                ];
                return cl_define_class_basic(ctx, &define_args);
            }
            // `hasArray()` true but no array came back: copy through `get`
            // below, as for a buffer without one.
        }
    }
    // A heap buffer without an accessible array (read-only): `b.get(tb)`,
    // which advances the position to the limit. A direct buffer: the JDK's
    // `defineClass2` reads at `position()` and leaves it there, so the
    // position is put back after the copy.
    let saved_pos = if direct {
        let now = ctx.read_native_pin(bb.1, bb.0);
        Some(int_answer(ctx.invoke_virtual(now, "position", "()I", &[])?))
    } else {
        None
    };
    let tb = ctx.new_array(cratonvm_types::ArrayElementType::Byte, len);
    let tb_pin = ctx.pin_native_root(tb);
    let now = ctx.read_native_pin(bb.1, bb.0);
    let tb_now = ctx.read_native_pin(tb_pin, tb);
    ctx.invoke_virtual(
        now,
        "get",
        "([B)Ljava/nio/ByteBuffer;",
        &[Value::Object(Some(tb_now))],
    )?;
    if let Some(pos) = saved_pos {
        let now = ctx.read_native_pin(bb.1, bb.0);
        ctx.invoke_virtual(now, "position", "(I)Ljava/nio/Buffer;", &[Value::Int(pos)])?;
    }
    let define_args = [
        Value::Object(Some(ctx.read_native_pin(this.1, this.0))),
        Value::Object(name.map(|(n, pin)| ctx.read_native_pin(pin, n))),
        Value::Object(Some(ctx.read_native_pin(tb_pin, tb))),
        Value::Int(0),
        Value::Int(len as i32),
        Value::Object(pd.map(|(p, pin)| ctx.read_native_pin(pin, p))),
    ];
    cl_define_class_basic(ctx, &define_args)
}

// ---------------------------------------------------------------------------
// WP2.3-C — JDK-internal `defineClass1` / `defineClass2` / `defineClass0`.
//
// These are the natives that the public `ClassLoader.defineClass(...)`
// overloads call into via the JDK's pure-Java wrapper.  CGLIB / direct
// user code typically goes through one of these three entry points.
// All three converge on `define_class_full` so PD attribution,
// hidden-flag handling, name-mismatch detection, and dup-define
// rejection are unified.
//
// Argument layout (for a static native, no `this` slot — these are
// `static` in the JDK source even though the public `defineClass`
// methods are instance methods that pass `this` as arg 0):
//
//   defineClass1(ClassLoader loader,
//                String name, byte[] b, int off, int len,
//                ProtectionDomain pd, String source) -> Class
//   defineClass2(ClassLoader loader,
//                String name, ByteBuffer bb, int off, int len,
//                ProtectionDomain pd, String source) -> Class
//   defineClass0(ClassLoader loader, Class<?> lookup,
//                String name, byte[] b, int off, int len,
//                ProtectionDomain pd, boolean initialize, int flags,
//                Object classData) -> Class
//
// The first arg is always the class loader — call it `loader` here.
// We allow that arg to be null (bootstrap) and fall back to the app
// loader id (0 → use ClassManager default).
// ---------------------------------------------------------------------------

/// Read a UTF-8 String from arg slot `idx`. Returns the empty string
/// on null. Treats binary-name dots as JVM internal slashes.
///
/// `pub`: also used by `shared_secrets_bridge::jla_define_class`.
pub fn read_optional_internal_name(
    ctx: &dyn NativeContext,
    args: &[Value],
    idx: usize,
) -> String {
    match args.get(idx) {
        Some(Value::Object(Some(o))) => {
            let dotted = ctx.read_string(*o).unwrap_or_default();
            dotted.replace('.', "/")
        }
        _ => String::new(),
    }
}

/// Read an `int` from arg slot `idx`. Negative values are folded into
/// `None` so callers can validate them as a JVM `IndexOutOfBoundsException`.
pub fn read_nonneg_int(args: &[Value], idx: usize) -> Option<usize> {
    match args.get(idx) {
        Some(Value::Int(v)) if *v >= 0 => Some(*v as usize),
        Some(Value::Int(_)) => None,
        _ => Some(0),
    }
}

/// The class-file bytes of a `byte[]` window: ONE bulk copy (`read_byte_array_into`,
/// a `memcpy` of the payload on the VM) instead of a `get_array_element` per byte.
/// The per-byte loop paid the array bounds + address checks and a flag read per
/// element, ~6.3 million of them for 1 500 classes (SBR5 `clload`). The caller has
/// already bounds-checked `[off, off+len)`; a short copy leaves the tail zero, the
/// value the per-element loop gave a non-`int` element.
pub fn read_class_bytes_bulk(
    ctx: &dyn NativeContext,
    array: ObjectRef,
    off: usize,
    len: usize,
) -> Vec<u8> {
    let mut out = vec![0u8; len];
    let _ = ctx.read_byte_array_into(array, off, &mut out);
    out
}

/// Read a `[B` (byte array) into a `Vec<u8>` honoring `[off, off+len)`.
/// Returns `Err(message)` if bounds are invalid (will surface as
/// `IndexOutOfBoundsException` to Java).
///
/// Defensive: rejects any access whose `offset+length` would overflow or
/// exceed the array length BEFORE the copy loop runs. Wraps the copy
/// loop itself in `catch_unwind` so a panic inside `get_array_element`
/// (e.g. due to a corrupt array object on the cglib path) returns an
/// `Err` instead of unwinding to SIGABRT. cglib emits 10-50 KB bytecode
/// buffers, so OOB-style SEGVs were observed before this hardening.
pub fn read_byte_array_slice(
    ctx: &dyn NativeContext,
    array: ObjectRef,
    off: usize,
    len: usize,
) -> Result<Vec<u8>, String> {
    let cap = ctx.array_length(array);
    // Strict overflow-safe bound: off+len must fit in `cap`.
    match off.checked_add(len) {
        Some(end) if end <= cap => {}
        Some(end) => {
            return Err(format!(
                "offset+length ({end}) > array length {cap} (off={off}, len={len})"
            ));
        }
        None => {
            return Err(format!("offset+length overflow (off={off}, len={len})"));
        }
    }
    let ctx_ref: &dyn NativeContext = ctx;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        read_class_bytes_bulk(ctx_ref, array, off, len)
    }));
    match result {
        Ok(out) => Ok(out),
        Err(_) => {
            tracing::error!(
                "[define_class] panic while reading byte array \
                 (off={off}, len={len}, cap={cap}) — aborting"
            );
            Err("panic while reading byte array".to_string())
        }
    }
}

/// `java.net.URL.toString()` for a URL we must not (or cannot) call bytecode
/// on — the CodeSource-location readers below and in `lookup_define` run on
/// the class-definition path, where `invoke_virtual("toExternalForm")` is not
/// available.
///
/// Measured (`java UrlProbe`, OpenJDK 25.0.3). `URLStreamHandler.toExternalForm`
/// is `protocol + ":" + ["//" + authority] + file + ["#" + ref]`, where `file`
/// is already `path + "?" + query`:
///
/// | spec                                   | toString                               | `protocol:file` alone |
/// |----------------------------------------|----------------------------------------|-----------------------|
/// | `file:/C:/repo/lib/foo.jar`            | `file:/C:/repo/lib/foo.jar`            | same                  |
/// | `file:///C:/repo/lib/foo.jar`          | `file:/C:/repo/lib/foo.jar`            | same                  |
/// | `jar:file:/C:/repo/lib/foo.jar!/`      | `jar:file:/C:/repo/lib/foo.jar!/`      | same                  |
/// | `jar:file:/o/lib/bar.jar!/com/x/Y.class`| `jar:file:/o/lib/bar.jar!/com/x/Y.class`| same                 |
/// | `file://server/share/x.jar`            | `file://server/share/x.jar`            | `file:/share/x.jar`   |
/// | `http://example.com:8080/a/b?q=1#frag` | `http://example.com:8080/a/b?q=1#frag` | `http:/a/b?q=1`       |
/// | `https://user@host/p`                  | `https://user@host/p`                  | `https:/p`            |
///
/// So `protocol + ":" + file` — what this module used to inline — is right for
/// exactly the classpath shapes and wrong for everything with an authority or
/// a fragment. The `authority` field, not `host`, is the one that round-trips:
/// `https://user@host/p` has `host == "host"` but `authority == "user@host"`.
///
/// Layout: real `java.net.URL` declares `protocol`(0) `host`(1) `port`(2)
/// `file`(3) `query`(4) `authority`(5) `path`(6) `userInfo`(7) `ref`(8)
/// (`javap -p java.net.URL`, JDK 25), and this crate's 13-slot synthetic URL
/// mirrors those indices — hence the by-name-then-slot reads. The LEGACY
/// 6-slot synthetic instead cached the whole spec in slots 0 and 5; a
/// protocol that reads back containing `':'` is that shape, and is returned
/// verbatim rather than being re-prefixed.
pub fn url_to_external_form(ctx: &dyn NativeContext, url: ObjectRef) -> Option<String> {
    // The location may already be a String rather than a URL.
    if let Some(s) = ctx.read_string(url) {
        return Some(s);
    }
    let read = |name: &str, slot: usize| -> Option<String> {
        match ctx.get_field_by_name(url, name) {
            Value::Object(Some(s)) => ctx.read_string(s),
            // A failed by-name read is never proof the field is null, so the
            // numeric fallback has to be tried regardless. This comment used
            // to say the reason was that an ABSENT field answers `Int(0)`;
            // that is the `MockNativeContext` behaviour, not production's.
            // Production (`vm_exec.rs::get_field_by_name`, and the trait
            // contract in `native-api/src/registry.rs`) answers
            // `Value::Object(None)` for a field it cannot resolve — which is
            // BYTE-FOR-BYTE the same answer as a present reference field that
            // happens to be null. The correct rationale is therefore the
            // stronger one: "by-name miss" and "field is genuinely null" are
            // indistinguishable from the value, so no `Object(None)` result
            // may be read as a layout verdict.
            _ => match ctx.get_field(url, slot) {
                Value::Object(Some(s)) => ctx.read_string(s),
                _ => None,
            },
        }
    };
    // NOT `?`: a legacy 6-slot URL leaves slot 0 null and caches the whole
    // spec in slot 5 only, so "no protocol" is a shape to handle, not a
    // failure. (`jboss_module_loader`'s `classpath:/…` resource URLs are
    // exactly that shape.)
    let protocol = read("protocol", 0).unwrap_or_default();
    if protocol.contains(':') {
        // Legacy 6-slot synthetic URL with the full spec cached in slot 0.
        return Some(protocol);
    }
    if protocol.is_empty() {
        // Legacy 6-slot synthetic URL with the full spec cached in slot 5.
        return match ctx.get_field(url, 5) {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        };
    }
    let mut out = String::with_capacity(protocol.len() + 32);
    out.push_str(&protocol);
    out.push(':');
    // `authority` is only at slot 5 on the real/13-slot layouts; the legacy
    // shape was already returned above, so the numeric read is safe here.
    match read("authority", 5) {
        Some(auth) if !auth.is_empty() => {
            out.push_str("//");
            out.push_str(&auth);
        }
        _ => {
            // No `authority` field (older synthetic): rebuild it from
            // host[:port], the way `net_uri_inet::url_external_form` does.
            let host = read("host", 1).unwrap_or_default();
            if !host.is_empty() {
                out.push_str("//");
                out.push_str(&host);
                let port = match ctx.get_field_by_name(url, "port") {
                    Value::Int(p) => p,
                    _ => match ctx.get_field(url, 2) {
                        Value::Int(p) => p,
                        _ => -1,
                    },
                };
                if port >= 0 {
                    out.push(':');
                    out.push_str(&port.to_string());
                }
            }
        }
    }
    if let Some(file) = read("file", 3) {
        out.push_str(&file);
    }
    if let Some(r) = read("ref", 8) {
        out.push('#');
        out.push_str(&r);
    }
    Some(out)
}

/// Decode an optional `ProtectionDomain` arg into a `code_source_url`
/// string suitable for `DefineClassFull::code_source_url`.
///
/// The synthetic PD layout (3 fields) carries the CodeSource at slot
/// 0 (= `PD_CODE_SOURCE_REF`) and the CodeSource carries either a
/// `URL` ObjectRef or a String at slot 0 (= `CS_LOCATION_REF`). For
/// a real-JDK-shape PD, we additionally probe `getCodeSource()` /
/// `getLocation()` by name as a belt-and-suspenders fallback.
pub fn extract_pd_code_source_url(ctx: &dyn NativeContext, pd: ObjectRef) -> Option<String> {
    // Synthetic PD path: field 0 -> CodeSource; field 0 of CS -> URL or String.
    if let Value::Object(Some(cs)) = ctx.get_field(pd, PD_CODE_SOURCE_REF) {
        if let Some(s) = ctx.read_string(cs) {
            return Some(s);
        }
        if let Value::Object(Some(loc)) = ctx.get_field(cs, CS_LOCATION_REF) {
            // W7-7 had to gate a raw slot-5 read here on a class-side
            // `authority` witness, because slot 5 is the full-spec cache on the
            // legacy synthetic URL and the `authority` FIELD on a real one —
            // and `read_string` succeeds on both. `url_to_external_form` makes
            // that distinction once, for every caller, and additionally returns
            // the real URL's full external form instead of nothing.
            if let Some(s) = url_to_external_form(ctx, loc) {
                return Some(s);
            }
        }
    }
    // Real-JDK PD path: field 0 may not match. Try by-name.
    if let Value::Object(Some(cs)) = ctx.get_field_by_name(pd, "codesource") {
        if let Value::Object(Some(loc)) = ctx.get_field_by_name(cs, "location") {
            // Real `CodeSource.location` is typed `java.net.URL`, not
            // `String` — `read_string` correctly fails on it (it's a
            // different concrete class), which silently dropped every
            // real-JDK-constructed CodeSource's URL here (e.g.
            // `URLClassLoader.defineClass(name, Resource)`'s
            // `new CodeSource(url, signers)`, the path
            // `ModifiedClassPathClassLoader`/`@ClassPathOverrides` uses to
            // load an overridden jar's classes — see
            // `NoSuchMethodFailureAnalyzerTests`). Reconstruct the URL string
            // from its own real fields. This used to inline
            // `protocol + ":" + file`, which is `toString()` only while the
            // authority and ref are both absent — see `url_to_external_form`
            // for the measured table and the two shapes it got wrong.
            if let Some(s) = url_to_external_form(ctx, loc) {
                return Some(s);
            }
        }
    }
    None
}

// W7-13: `read_byte_buffer_slice` lived here — a SECOND `defineClass2`
// ByteBuffer decoder, and the one that actually ran, because this module's
// `defineClass2` registration shadows `lang_system`'s in synthetic-JDK mode.
// It hardcoded slot 0 as the backing `byte[]` (on a real `HeapByteBuffer`
// slot 0 is `Buffer.mark`, an int, so every real heap buffer fell through to
// the direct arm and died on "no native address") and it CLAMPED an
// out-of-range `(off, len)` with `min`/`saturating_add` instead of rejecting
// it. Both defects were already fixed in
// `lang_system::read_byte_buffer_define_class_slice`; the fix was inert
// wherever the shadow won. `cl_define_class2` now calls that decoder, so
// there is exactly one.

/// Bind the loader-id used to register the new class. We look up the
/// loader's recorded namespace id if it has one (lazily allocating a fresh
/// id), otherwise fall through to id 0 (= application loader). A null
/// loader is treated as the bootstrap class loader, which the backend
/// also models as id 0 in this VM.
/// Whether a HIDDEN class defined through a lookup on `lookup_mirror` (or,
/// with no lookup class, for `loader`) belongs in the BOOTSTRAP namespace
/// ([`cratonvm_native_api::DefineClassFull::bootstrap_namespace`]): the lookup
/// class is one the VM tags bootstrap (a `java.base` class, or another JDK
/// class the VM defined under the bootstrap key), or there is no lookup class
/// and the loader is the null one. HotSpot's `JVM_LookupDefineClass` defines a
/// hidden class in its lookup class's loader, module and runtime package; the
/// `0` loader id every built-in loader collapses to here is the APPLICATION
/// namespace, which put `java.lang.String$$StringConcat` (`StringConcatFactory`,
/// a full-power lookup on `String`) in another runtime package than the
/// package-private `StringConcatHelper$StringConcatBase` it extends, and
/// its spin failed with `IllegalAccessError`. Both modes. Interpreter round i1
/// wave 44, lane L5 (probe `L4W43CopyWithViews.java`,
/// `concat-*`).
pub fn hidden_define_in_bootstrap_namespace(
    ctx: &mut dyn NativeContext,
    lookup_mirror: Option<ObjectRef>,
    loader: Value,
) -> bool {
    let bootstrap = match lookup_mirror.and_then(|m| crate::lang_class::mirror_class_id(ctx, m)) {
        Some(cid) => {
            ctx.loader_id_of_class(cid) == cratonvm_types::ClassLoaderId::NATIVE_BOOTSTRAP as i32
        }
        None => matches!(loader, Value::Object(None)),
    };
    if bootstrap && cratonvm_types::flags().loader.dbg_define {
        eprintln!("[DEFINE-DBG] hidden define in the bootstrap namespace (bootstrap lookup class)");
    }
    bootstrap
}

pub fn loader_id_for(ctx: &mut dyn NativeContext, loader: Value) -> u32 {
    if let Value::Object(Some(cl)) = loader {
        return get_or_assign_loader_id(ctx, cl);
    }
    0
}

/// HotSpot's `ClassFileParser` refusal of a class-file header too short or
/// with the wrong magic, for the VM's define doors that stand in for
/// `JVM_DefineClass*` (`ClassLoader.defineClass` stand-ins, `defineClass0/1/2`
/// through [`define_class_via_full`], `Unsafe.defineClass`): fewer than 8
/// bytes is `ClassFormatError: Truncated class file`; a wrong magic is
/// `ClassFormatError: Incompatible magic value <u32> in class file <name>`,
/// the name in internal form, `<Unknown>` for none
/// (`lang_system::jdk_only_classfile_header_error`, measured on JDK 25 by
/// `L5W42DefineClassErrors`). These doors said `<door>: not a valid class
/// file (bad magic)` before interpreter round i4 wave 95 (lane L5;
/// `i42-L5-proposal-one-classfileparser-shaped-error-for-every-define-door`,
/// probe `L5W95DefineDoorTexts`). Both modes: HotSpot's text is the target of
/// each. Callers ask only for a header that fails the check.
pub fn classfile_parser_header_refusal(
    ctx: &mut dyn NativeContext,
    internal_name: &str,
    bytes: &[u8],
) -> cratonvm_types::error::MethodCallFailed {
    crate::lang_system::jdk_only_classfile_header_error(ctx, internal_name, bytes).unwrap_or_else(
        || {
            cratonvm_types::error::LinkageError::ClassFormatError {
                class_name: internal_name.to_string(),
                message: "Truncated class file".to_string(),
            }
            .into()
        },
    )
}

/// Common backend used by all three `defineClassN` natives. Returns
/// the resulting Class mirror as a `Value::Object(Some(...))` or an
/// exception via the `MethodCallResult` channel.
///
/// `pub`: also reused by `shared_secrets_bridge::jla_define_class`
/// (`JavaLangAccess.defineClass`, the `System$1` bridge that
/// `jdk.internal.reflect.ClassDefiner` calls into) so both entry points
/// share the same magic-check / panic-guard / PD-attribution behavior.
///
/// `loader`: the `ClassLoader` object passed to the `defineClassN` native
/// (may be `Value::Object(None)` for the bootstrap loader). Recorded via
/// `register_defining_loader` on success so this class's true defining
/// loader is known to every `defining_loader_for` consumer (`getClassLoader`,
/// GC loader-pinning, `inherit_lookup_loader`'s namespace lookup, ...) —
/// previously only `Lookup.defineClass` (`lookup_define.rs`) recorded this,
/// so any class defined the ordinary way (`ClassLoader.defineClass`, e.g.
/// Groovy's `GroovyClassLoader` compiling a script class) had no recorded
/// defining loader. That left `inherit_lookup_loader`'s legacy fallback
/// (`loader_id_of_class`, which collapses small `UserDefined(n)` ids into
/// the builtin-loader id range) as the only source of truth for a CGLIB
/// proxy generated against such a class, mis-routing the proxy into the
/// Application namespace and CNFE-failing the `Class.forName(name, true,
/// loader)` CGLIB issues right after — see
/// `CRATONVM-SPRING-GENUINE-BUGLIST`'s Groovy cluster
/// entry (`GroovyAspectTests`/`GroovyAspectIntegrationTests` residuals).
///
/// A header too short or with the wrong magic is refused with HotSpot's
/// `ClassFileParser` text ([`classfile_parser_header_refusal`]).
pub fn define_class_via_full(
    ctx: &mut dyn NativeContext,
    name: &str,
    bytes: Vec<u8>,
    loader_id: u32,
    opts: cratonvm_native_api::DefineClassFull,
    initialize: bool,
    class_data: Option<Value>,
    loader: Value,
) -> MethodCallResult {
    use cratonvm_types::error::LinkageError;

    // Pre-validate the class file header: at least 8 bytes (magic +
    // minor + major) and CAFEBABE magic must be present, otherwise the
    // backend parser may dereference garbage past the buffer end. HotSpot's
    // `JVM_DefineClass*` / `JVM_LookupDefineClass` refuse such bytes in
    // `ClassFileParser` with a `ClassFormatError`; this door threw
    // `IllegalArgumentException: defineClass: not a valid class file (bad
    // magic)` before interpreter round i4 wave 95 (lane L5).
    if bytes.len() < 8 || bytes[0..4] != CLASS_FILE_MAGIC {
        tracing::warn!("[define_class] invalid magic for {name}; rejecting");
        return Err(classfile_parser_header_refusal(ctx, name, &bytes));
    }

    // Wrap the backend call in `catch_unwind` so a panic inside
    // `define_class_full` (verifier OOB, ASM-emitted bytecode that
    // defeats our class file parser, etc.) returns a clean
    // ClassFormatError instead of unwinding to SIGABRT.
    // GC: a reference held in a Rust local across an allocating or Java-re-entering
    // call goes stale under a moving collector, and under the Generational
    // non-moving young sweep an unrooted object is ZEROED in place. Pin and
    // re-read. `safe_native_call_impl` truncates `native_pin_roots` when the native
    // returns, so an unmatched pin costs nothing on an error path. See
    // `internal/audits/wide-tranche-triage-20260907.md`.
    // `define_class_full` defines a class and `get_class_mirror` below
    // allocates the mirror, so BOTH incoming references — the `classData`
    // object and the loader — are stale by the time they are stored and
    // registered. The audit reported `loader`; reading found `class_data` too.
    let class_data_pin = match class_data {
        Some(Value::Object(Some(o))) => Some((ctx.pin_native_root(o), o)),
        _ => None,
    };
    let loader_pin = match loader {
        Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
        _ => None,
    };
    // gc-common w18-d: read before `opts` moves into the backend.
    let non_strong_hidden = opts.unloads_with_its_mirror();
    let hidden = opts.hidden;
    // zs s8/nd7: the mirror object before the define (the class data and the
    // loader are pinned above; `opts`' domain is re-read across it).
    let mut opts = opts;
    // zs s9/nd8 (rv6 residual 5): the lowest pin this body took, released on
    // the failure arms (the reservation sits above it).
    let pin_floor = [class_data_pin.map(|(p, _)| p), loader_pin.map(|(p, _)| p)]
        .into_iter()
        .flatten()
        .min();
    let release_pins = |ctx: &mut dyn NativeContext| {
        if let Some(floor) = pin_floor {
            ctx.unpin_native_roots(floor);
        }
    };
    // zt t6/nd4 (zt1nd page item 1): the one allocation `addClass` below can
    // make (the `classes` list's growth) happens HERE, before anything is
    // published, so its `OutOfMemoryError` leaves nothing defined, as
    // HotSpot's `define_instance_class` (addClass before the dictionary
    // update) does. ZGC only; `CRATONVM_ZGC_ADDCLASS_PRESIZE=0` kills it.
    // The presize and the reservation are the census's `mirror-reserve` row.
    let reserve_clock = cratonvm_classloading::define_census::phase_start();
    if !hidden
        && addclass_presize_enabled(ctx)
        && ctx.method_exists(CL_CLASS, "addClass", ADD_CLASS_DESCRIPTOR)
    {
        if let Some((p, o)) = loader_pin {
            // zt t16/df6 (define page §2): the presize runs Java (a GC point),
            // and `opts`' domain came in raw: pin it across, re-read it, and
            // release this pin alone (the topmost) before the reservation,
            // which pins the domain itself.
            let domain_pin = opts
                .protection_domain
                .map(|pd| (ctx.pin_native_root(pd), pd));
            let loader_obj = ctx.read_native_pin(p, o);
            // zt t9/df: malformed bytes keep their `ClassFormatError`.
            let presized = presize_loader_classes(ctx, loader_obj);
            if let Some((dp, pd)) = domain_pin {
                opts.protection_domain = Some(ctx.read_native_pin(dp, pd));
                ctx.unpin_native_roots(dp);
            }
            if let Err(e) = refusal_yields_to_format_error(ctx, presized, &bytes, "") {
                // zt t7/x4b: a refused define is timed in its row too.
                cratonvm_classloading::define_census::phase_end(
                    cratonvm_classloading::define_census::DefinePhase::MirrorReserve,
                    reserve_clock,
                );
                release_pins(ctx);
                return Err(e);
            }
        }
    }
    let reservation = crate::lang_system::reserve_mirror_for_define(
        ctx,
        &crate::lang_system::define_name_hint(name, &bytes),
        &mut opts,
    );
    // zt t9/df: malformed bytes keep their `ClassFormatError`.
    let reservation = match refusal_yields_to_format_error(ctx, reservation, &bytes, "") {
        Ok(r) => r,
        Err(e) => {
            // zt t7/x4b: as above.
            cratonvm_classloading::define_census::phase_end(
                cratonvm_classloading::define_census::DefinePhase::MirrorReserve,
                reserve_clock,
            );
            release_pins(ctx);
            return Err(e);
        }
    };
    cratonvm_classloading::define_census::phase_end(
        cratonvm_classloading::define_census::DefinePhase::MirrorReserve,
        reserve_clock,
    );
    let define_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ctx.define_class_full(name, &bytes, loader_id, opts)
    }));
    let define_result = match define_result {
        Ok(r) => r,
        Err(_) => {
            tracing::error!("[define_class] panic inside define_class_full for {name}; aborting");
            let _ = crate::lang_system::take_mirror_reservation(ctx, reservation);
            release_pins(ctx);
            return Err(cratonvm_types::error::LinkageError::ClassFormatError {
                class_name: name.to_string(),
                message: "defineClass: panic inside backend (likely malformed bytecode)".into(),
            }
            .into());
        }
    };
    match define_result {
        Ok(cid) => {
            // Record the true defining loader (see the doc comment above)
            // so later `defining_loader_for(cid)` consumers — including
            // `inherit_lookup_loader`'s namespace lookup for a subsequent
            // CGLIB/`Lookup.defineClass` proxy of this exact class — see the
            // real loader instead of falling back to the legacy
            // `loader_id_of_class` path, which can collapse a small
            // `UserDefined(n)` id into the builtin-loader range.
            //
            // Gated on `is_user_defined_loader` to preserve the existing
            // invariant that `defining_loader_store` only ever holds genuine
            // custom-`ClassLoader` instances, never the built-in bootstrap/
            // platform/application loader — registering the latter would add
            // an entry for nearly every class defined during a run (the vast
            // majority go through the system loader) for no behavioral
            // benefit (every consumer either wants the true custom loader or
            // already has its own built-in-loader fallback).
            //
            // MUST run before `get_class_mirror` below: `class_mirror_impl`
            // consults `defining_loader_for(cid)` at mirror-creation time —
            // the ONLY time it is ever consulted for this mirror, since the
            // mirror is cached from then on — to decide whether to record a
            // `mirror_pin` row. `get_class_mirror` before this registration
            // means `defining_loader_for` still answers `None` when the
            // mirror is minted, so no row is EVER recorded (there is no
            // retroactive backfill) and `roots.rs` step 6 falls back to
            // unconditionally rooting the mirror forever — permanently
            // defeating unloading for every class defined this way
            // (`ClassLoader.defineClass`: Jasper JSP compilation, Groovy
            // script classes, CGLIB proxies, ...). Confirmed via
            // `CRATONVM_DBG_MIRRORPIN`: `TestDefaultInstanceManager`'s
            // evicted JSP class mirror traced `pinned=false => ROOTED`
            // every cycle, so its annotation-cache entry could never be
            // reclaimed.
            if loader_aware_resolution() {
                if let Value::Object(Some(loader_obj)) = loader {
                    let loader_obj = match loader_pin {
                        Some((p, o)) => ctx.read_native_pin(p, o),
                        None => loader_obj,
                    };
                    if is_user_defined_loader(ctx, loader_obj) {
                        register_defining_loader(loader_tables(ctx), cid.as_u32(), loader_obj);
                    }
                }
            }
            let reserved = crate::lang_system::take_mirror_reservation(ctx, reservation);
            let mirror = crate::lang_system::defined_class_mirror_reserved(ctx, cid, reserved);
            // classData (defineClass0 path) goes into the mirror's own
            // `Class.classData` field, read back through its pin.
            if let Some((p, o)) = class_data_pin {
                let data = ctx.read_native_pin(p, o);
                attach_class_data(ctx, mirror, data);
            }
            // Eager-init request: run <clinit> now (defineClass0 path
            // when `initialize == true`). `ctx.initialize_class` can
            // allocate and trigger a moving GC, so `mirror` (a raw
            // `ObjectRef` captured above and returned again below) must be
            // rooted across the call — same Family-1 stale-ObjectRef
            // pattern as the sibling `lk_ensure_initialized` fix. See
            // wildfly-parallel-boot-stale-objectref-residual.md.
            let mirror_pin = ctx.pin_native_root(mirror);
            // Interpreter round i2 wave 63, lane L1: HotSpot records every
            // class a loader object defines (not a hidden one) in that
            // loader's `classes` list, `ClassLoader.addClass`
            // (`SystemDictionary::define_instance_class`), before the class
            // is initialized; JVMTI's heap walks reach the classes through
            // it, and its failure (an `OutOfMemoryError`) fails the define.
            if !hidden {
                if let Some((p, o)) = loader_pin {
                    let loader_obj = ctx.read_native_pin(p, o);
                    let mirror = ctx.read_native_pin(mirror_pin, mirror);
                    // zt t16/df6 (define page §2): a failed upcall leaves an
                    // orphan a retry of these bytes adopts (the duplicate arm
                    // below), as at the `defineClass0/1/2` doors.
                    let added = record_with_loader(ctx, loader_obj, mirror).map(|()| mirror);
                    note_add_class_orphan_on_failure(ctx, &added, cid, &bytes);
                    added?;
                }
            }
            if initialize {
                // `initialize = true` is a CONTRACT, not a hint. HotSpot's
                // `JVM_LookupDefineClass` links AND initializes before handing the
                // class back, and an initialization failure reaches the caller:
                // the original `Error`, or `ExceptionInInitializerError` wrapping
                // anything else (JVMS 5.5).
                //
                // This used to `tracing::warn!` and fall through, returning the
                // mirror as though `<clinit>` had succeeded. The caller then held
                // an UNINITIALIZED class and the real failure surfaced later,
                // somewhere with no visible connection to the define — the
                // silent-wrong-result shape this file's own error contract
                // (`cl_unsafe_define_class`'s note) argues against.
                //
                // Propagating `MethodCallFailed` unchanged is deliberate: it
                // carries the exception the initializer actually raised, so the
                // caller sees what HotSpot would have thrown rather than a
                // `ClassFormatError` about the define step. The sibling body in
                // `lang_system.rs` wraps instead, and that wrapping is a separate
                // deviation with three consumers keyed on its message text — see
                // `docs/feature-designs/class-resolution-and-caching.md`.
                //
                // `safe_native_call_impl` truncates `native_pin_roots` when the
                // native returns, so the unmatched `mirror_pin` above costs
                // nothing on this path — the same reasoning the pin's own comment
                // records for the other error arms.
                if let Err(e) = ctx.initialize_class(cid) {
                    tracing::warn!(
                        "defineClass0 initialize: <clinit> for {name} failed; propagating"
                    );
                    return Err(e);
                }
            }
            let mirror = ctx.read_native_pin(mirror_pin, mirror);
            // Release from the LOWEST pin this body took (the class data's,
            // then the loader's): releasing from `mirror_pin` left both on a
            // normal return, which the `[pin-balance] LEAK` census named for
            // `System$1.defineClass` (interpreter round i2 wave 62, lane L4).
            let floor = [class_data_pin.map(|(p, _)| p), loader_pin.map(|(p, _)| p)]
                .into_iter()
                .flatten()
                .fold(mirror_pin, usize::min);
            ctx.unpin_native_roots(floor);
            // gc-common w18-d: last, after `<clinit>` and with nothing left
            // to allocate: from here the class lives as long as its mirror
            // and its instances, and the mirror is this native's return value.
            if non_strong_hidden {
                register_non_strong_hidden_class(ctx, cid);
            }
            Ok(Some(Value::Object(Some(mirror))))
        }
        Err(msg) => {
            let _ = crate::lang_system::take_mirror_reservation(ctx, reservation);
            // zt t16/df6 (define page §2): the retry of a define whose
            // `addClass` upcall failed adopts the class it left behind (ZGC,
            // `CRATONVM_ZGC_DEFINE_ADD_CLASS_RETRY`; `None` unless it is an
            // orphan of these bytes in this loader), and runs the `<clinit>`
            // the failed define never reached when `initialize` asks. This
            // door installs no domain object, so the adoption writes none.
            if !hidden
                && (msg.contains("DuplicateClassDefinition") || msg.contains("already defined"))
            {
                if let Some((p, o)) = loader_pin {
                    let loader_obj = ctx.read_native_pin(p, o);
                    if let Some(adopted) =
                        adopt_add_class_orphan(ctx, loader_obj, name, loader_id, &bytes, None)
                    {
                        let answer = match adopted {
                            Ok(mirror) if initialize => match ctx.class_id_from_mirror(mirror) {
                                Some(adopted_id) => ctx
                                    .initialize_class(adopted_id)
                                    .map(|()| ctx.get_class_mirror(adopted_id)),
                                None => Ok(mirror),
                            },
                            other => other,
                        };
                        release_pins(ctx);
                        return answer.map(|mirror| Some(Value::Object(Some(mirror))));
                    }
                }
            }
            release_pins(ctx);
            tracing::warn!("ClassLoader.defineClass({name}) failed: {msg}");
            Err(LinkageError::ClassFormatError {
                class_name: name.to_string(),
                message: format!("defineClass: {msg}"),
            }
            .into())
        }
    }
}

/// `loader.addClass(mirror)`, the JDK's record of the classes a loader
/// defined (interpreter round i2 wave 63, lane L1); nothing on a class
/// library without the method.
///
/// zt t9/df (`zgc-zt8x5-wave-t7-review-residuals-CLOSED-20261019.md` item 2): the
/// call is the exact `java.lang.ClassLoader.addClass(Class)`
/// ([`call_loader_add_class`]), as the real-JDK doors make it since t8/x5, so a
/// user loader's own `addClass(Class)` no longer intercepts the synthetic /
/// `JavaLangAccess` backend's upcall either. `CRATONVM_DEFINE_ADD_CLASS_EXACT=0`
/// restores the virtual call here too (every collector, as that switch is).
pub fn record_with_loader(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    mirror: ObjectRef,
) -> Result<(), MethodCallFailed> {
    let class_id = ctx.class_id_from_mirror(mirror);
    let loader_pin = ctx.pin_native_root(loader);
    let mirror_pin = ctx.pin_native_root(mirror);
    let mut result = if ctx.method_exists(CL_CLASS, "addClass", ADD_CLASS_DESCRIPTOR) {
        // Claimed as in `invoke_add_class` (interpreter round i7 wave 137,
        // lane L5): the VM's own record of a built-in loader's class skips it
        // then. Package publication is independent of this claim.
        let claimed = class_id.is_none_or(|cid| ctx.claim_builtin_loader_class_record(cid));
        if claimed {
            let loader = ctx.read_native_pin(loader_pin, loader);
            let mirror = ctx.read_native_pin(mirror_pin, mirror);
            call_loader_add_class(ctx, loader, mirror).map(|_| ())
        } else {
            Ok(())
        }
    } else {
        Ok(())
    };
    if result.is_ok() {
        if let Some(class_id) = class_id {
            let loader = ctx.read_native_pin(loader_pin, loader);
            result = sync_package_for_defined_class(ctx, loader, class_id);
        }
    }
    ctx.unpin_native_roots(mirror_pin);
    ctx.unpin_native_roots(loader_pin);
    result
}

/// Publish the package for a just-defined class after either the Java define
/// door or the VM's own class-path define path. This is independent of the
/// `addClass` claim, so a class already listed by its define door still gets
/// the package-map bookkeeping.
pub fn sync_package_for_defined_class(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    class_id: cratonvm_types::ClassId,
) -> Result<(), MethodCallFailed> {
    if !ctx.builtin_loader_package_sync() {
        return Ok(());
    }
    if !loader_is_builtin(ctx, loader) {
        return Ok(());
    }
    let Some(class_name) = ctx.class_name_of_id(class_id) else {
        return Err(RuntimeError::InternalError {
            message: format!("missing class name for package publication: {class_id:?}"),
        }
        .into());
    };
    let package = class_name.rsplit_once('/').map_or("", |(pkg, _)| pkg);
    let dotted = package.replace('/', ".");
    let loader_pin = ctx.pin_native_root(loader);
    let loader = ctx.read_native_pin(loader_pin, loader);
    let synced = crate::lang_class::sync_builtin_loader_package_for_class(
        ctx, loader, &dotted, class_id,
    );
    ctx.unpin_native_roots(loader_pin);
    if !synced? {
        return Err(RuntimeError::InternalError {
            message: format!("could not publish package {dotted:?} to built-in loader"),
        }
        .into());
    }
    Ok(())
}

/// `ClassLoader.addClass(Class)`'s descriptor.
pub const ADD_CLASS_DESCRIPTOR: &str = "(Ljava/lang/Class;)V";

/// The `loader.addClass(mirror)` call of every define door
/// ([`record_with_loader`], [`invoke_add_class`]).
///
/// Interpreter round i7 wave 135, lane L5
/// (`interpreter-i135-L5-add-class-upcall-runs-a-user-loaders-own-add-class-FIXED-20261021.md`):
/// HotSpot calls `java.lang.ClassLoader.addClass` ITSELF
/// (`Universe::loader_addClass_method()`, `JavaCalls::call` of that resolved
/// method: no virtual selection). The method is package-private in
/// `java.lang`, so no loader class can override it (JVMS §5.4.5), but a user
/// loader may declare its own `void addClass(Class)` -- and the plain
/// `invoke_virtual` door runs the first same-signature method from the
/// receiver's class down, override or not, so it ran THAT method instead
/// (the class was missing from `ClassLoader.classes`, and the user method ran
/// once per define). The call names `java/lang/ClassLoader` exactly
/// (`invoke_special`, as zt t8/x5 made it on the ZGC round; wave 135 had
/// `invoke_virtual_declared`, whose §5.4.6 selection gives the same method). `CRATONVM_DEFINE_ADD_CLASS_EXACT=0` (HotSpot parity,
/// default ON; [`define_add_class_exact_enabled`]) restores the receiver walk.
pub fn call_loader_add_class(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    mirror: ObjectRef,
) -> MethodCallResult {
    let args = [Value::Object(Some(mirror))];
    if define_add_class_exact_enabled() {
        if let Some(answer) = call_loader_add_class_hinted(ctx, loader, mirror) {
            return answer;
        }
        // zt t8/x5 (the same fix, reached in parallel on the ZGC round): the
        // exact call, `invoke_special` on `java/lang/ClassLoader` -- the
        // resolved method itself, with no selection at all.
        ctx.invoke_special(
            CL_CLASS,
            "addClass",
            ADD_CLASS_DESCRIPTOR,
            &[Value::Object(Some(loader)), Value::Object(Some(mirror))],
        )
    } else {
        ctx.invoke_virtual(loader, "addClass", ADD_CLASS_DESCRIPTOR, &args)
    }
}

/// `CRATONVM_DEFINE_ADD_CLASS_EXACT` (HotSpot parity, default ON, `=0` off;
/// interpreter round i7 wave 135, lane L5). Read per define, as
/// [`define_add_class_enabled`] is. Two halves:
/// * [`call_loader_add_class`]: the upcall runs `ClassLoader.addClass` itself;
/// * `unsafe_natives::native_unsafe_define_class`: `Unsafe.defineClass0` into
///   a BUILT-IN loader other than the platform loader (the application
///   loader) makes the upcall too, as HotSpot's `JVM_DefineClass` ->
///   `define_instance_class` does for every non-null loader; that door pinned
///   (and so recorded) only a user loader or the platform loader.
pub fn define_add_class_exact_enabled() -> bool {
    true
}

/// `CRATONVM_DEFINE_ADD_CLASS_HINTED` (performance, default OFF, read once
/// per VM into [`LoaderIdentityTables::define_add_class_hinted`];
/// interpreter round i7 wave 135, lane L5, page
/// `i71-orch-proposal-class-path-jar-loading-is-eight-times-hotspot`): the
/// define's `addClass` upcall enters `java.lang.ClassLoader.addClass`'s
/// bytecode directly by its declared-method index, kept per VM in
/// [`LoaderIdentityTables::add_class_hint`] -- HotSpot's own shape
/// (`JavaCalls::call` of the resolved `Universe::loader_addClass_method()`).
/// The declared door ([`call_loader_add_class`]) re-resolves by name and asks
/// the §5.4.6 selection question on every define for an answer that is
/// always `ClassLoader.addClass`.
///
/// `None` (the caller takes the declared door): the switch is off, the
/// context keeps no tables, the loader is on the synthetic layout (whose
/// `ClassLoader` may answer `addClass` with a registered native, which this
/// bytecode-only entry would skip), or `java/lang/ClassLoader` has no id.
/// The VM checks the hint against the class's methods on every call, so a
/// stale or foreign word costs one re-resolution by name, never a wrong
/// method.
pub fn call_loader_add_class_hinted(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    mirror: ObjectRef,
) -> Option<MethodCallResult> {
    let (on, word) = {
        let tables = ctx
            .loader_identity_tables()
            .and_then(|any| any.downcast_ref::<LoaderIdentityTables>())?;
        let on = *tables.define_add_class_hinted.get_or_init(|| {
            cratonvm_types::flags::runtime_flag_on("CRATONVM_DEFINE_ADD_CLASS_HINTED")
        });
        (on, tables.add_class_hint.load(Ordering::Relaxed))
    };
    if !on || cl_has_synthetic_layout(ctx, loader) {
        return None;
    }
    let cid = ctx.class_id_by_name(CL_CLASS)?;
    let mut hint = (word != 0 && (word >> 32) as u32 == cid.as_u32())
        .then(|| (word as u32).wrapping_sub(1));
    let answer = ctx.invoke_special_bytecode_only_hinted(
        cid,
        &mut hint,
        CL_CLASS,
        "addClass",
        ADD_CLASS_DESCRIPTOR,
        &[Value::Object(Some(loader)), Value::Object(Some(mirror))],
    );
    if let Some(tables) = ctx
        .loader_identity_tables()
        .and_then(|any| any.downcast_ref::<LoaderIdentityTables>())
    {
        let next = hint.map_or(0, |i| (u64::from(cid.as_u32()) << 32) | u64::from(i.wrapping_add(1)));
        tables.add_class_hint.store(next, Ordering::Relaxed);
    }
    Some(answer)
}

/// zt t7/cl: `CRATONVM_DEFINE_ADD_CLASS` (default ON, `=0` kill). Memoised in
/// a `MemoSlot`, which does not publish while an override is live and is reset
/// when one is installed or dropped, so a test's thread override reaches it.
///
/// HotSpot's `SystemDictionary::define_instance_class` calls the Java upcall
/// `ClassLoader.addClass(Class)` for every class a non-boot loader defines
/// (a hidden class is never defined through it), so the loader's private
/// `classes` list holds every class it defined, in definition order: what a
/// heap walk, JVMTI `FollowReferences` and a `privateLookupIn` getter on
/// `ClassLoader.classes` see. The real-JDK define doors
/// (`lang_system::native_classloader_define_class0/1/2`, which serve
/// `ClassLoader.defineClass*`, `Lookup.defineClass` and JNI `DefineClass` in
/// BOTH `--real-jdk` and `--compatible`; `Unsafe.defineClass0`; and
/// `--compatible`'s own `Lookup.defineClass` door) never made the upcall, so
/// the list stayed empty in both modes. Only the synthetic-JDK / JLA backend
/// ([`define_class_via_full`]) made it. Every collector: it moves both modes'
/// Java-visible behaviour to HotSpot's; G1 and Generational take it alike.
/// `=0` restores the doors that skip the upcall.
pub fn define_add_class_enabled() -> bool {
    // JIT round 23 wave 2 (proxy-define page): read twice per define; the
    // `MemoSlot` (1 off, 2 on) is re-derived when a test installs an override,
    // so the kill arm below still sees its own setting.
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on = true;
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    }
}

/// zt t7/cl: the loader's `java.lang.ClassLoader.classes` list, or `None`
/// (no such field, or null: a VM-made loader object whose constructor never
/// ran). The field is read through the slot `java/lang/ClassLoader` declares
/// it in, so a subclass that declares its own `classes` field does not shadow
/// it; only when no such declared slot resolves is it read by name.
///
/// zt t7/cl2: the declared slot no longer waits for the real-layout witness
/// (`cl_has_synthetic_layout`). A field `java/lang/ClassLoader` itself
/// declares sits at the same index in every subclass instance, so the
/// witness added nothing. It also sent x4's shadowing fixture, whose
/// `ClassLoader` declares `classes` but no witness field, back to the by-name
/// read: the shadowing x4 fixed in `array_list_is_full` (merge `4cc40787d2`).
/// The hazard that witness guards elsewhere (`classloader_parent`) is a
/// HARD-CODED synthetic slot number, which this read never uses.
pub fn loader_classes_list(ctx: &dyn NativeContext, loader: ObjectRef) -> Option<ObjectRef> {
    let declared = ctx.resolve_field_index(CL_CLASS, "classes");
    let value = match declared {
        Some(index) => ctx.get_field(loader, index),
        None => ctx.get_field_by_name(loader, "classes"),
    };
    match value {
        Value::Object(Some(list)) => Some(list),
        _ => None,
    }
}

/// zt t7/cl: `loader.addClass(mirror)` after a define door published a
/// non-hidden class into `loader` (both references current at entry), as
/// HotSpot's `define_instance_class` does for every non-boot loader. Returns
/// the mirror re-read after the upcall (it runs Java, a GC point). Nothing is
/// done with the switch off ([`define_add_class_enabled`]), on a class
/// library without the method, or for a loader without a `classes` list (a
/// VM-made loader object: `addClass` would throw `NullPointerException`
/// where HotSpot, whose loaders always ran their constructor, cannot).
///
/// The upcall's failure (its list growth's `OutOfMemoryError`) is the
/// define's, as on HotSpot. On ZGC the growth normally ran before the define
/// ([`presize_for_add_class`]), so the upcall allocates nothing.
pub fn add_defined_class_to_loader(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    mirror: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    if !define_add_class_enabled()
        || !ctx.method_exists(CL_CLASS, "addClass", ADD_CLASS_DESCRIPTOR)
    {
        return Ok(mirror);
    }
    invoke_add_class(ctx, loader, mirror)
}

/// [`add_defined_class_to_loader`] past its switch and method gates.
///
/// Interpreter round i7 wave 137, lane L5
/// (`interpreter-i137-L5-vm-defined-application-classes-are-missing-from-the-loaders-classes-list-FIXED-20261021.md`):
/// the class is CLAIMED first (`NativeContext::claim_builtin_loader_class_record`),
/// so a built-in loader's class this door records is not recorded again by
/// the VM's own record of the classes it defined for that loader
/// (`vm_util::record_in_builtin_loader`): HotSpot lists each class once.
pub fn invoke_add_class(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    mirror: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    let class_id = ctx.class_id_from_mirror(mirror);
    let loader_pin = ctx.pin_native_root(loader);
    let mirror_pin = ctx.pin_native_root(mirror);
    let mut result = Ok(mirror);
    let loader = ctx.read_native_pin(loader_pin, loader);
    if loader_classes_list(ctx, loader).is_some() {
        let claimed = class_id.is_none_or(|cid| ctx.claim_builtin_loader_class_record(cid));
        if claimed {
            let loader = ctx.read_native_pin(loader_pin, loader);
            let mirror = ctx.read_native_pin(mirror_pin, mirror);
            result = invoke_add_class_claimed(ctx, loader, mirror);
        }
    }
    if result.is_ok() {
        if let Some(class_id) = class_id {
            let loader = ctx.read_native_pin(loader_pin, loader);
            result = result.and_then(|mirror| {
                sync_package_for_defined_class(ctx, loader, class_id).map(|()| mirror)
            });
        }
    }
    if let Ok(_) = result {
        let current_mirror = ctx.read_native_pin(mirror_pin, mirror);
        ctx.unpin_native_roots(mirror_pin);
        ctx.unpin_native_roots(loader_pin);
        Ok(current_mirror)
    } else {
        ctx.unpin_native_roots(mirror_pin);
        ctx.unpin_native_roots(loader_pin);
        result
    }
}

/// Interpreter round i7 wave 137, lane L5: the VM's record of a class it
/// defined ITSELF for a built-in loader (`vm_util::record_in_builtin_loader`,
/// which claimed `class_id` already): `loader.addClass(mirror)`, as HotSpot's
/// `define_instance_class` makes it for every class the application (or
/// platform) loader defines. The VM loads an application class-path class
/// from its own class path -- the `--real-jdk` `loadClassOrNull` /
/// `findClassOnClassPathOrNull` intrinsics, the `--compatible` class-path
/// natives, a constant-pool resolution -- so the JDK's
/// `BuiltinClassLoader.defineClass` -> `defineClass1` door, which records
/// the class, never runs, and the class was missing from the loader's
/// `ClassLoader.classes` in both modes (`L5W135LoaderClassesList`,
/// `app-own-class` row).
///
/// `Ok(Some(true))`: the upcall ran. `Ok(Some(false))`: nothing to record,
/// ever -- the switch or the method is off, the class's loader is the boot
/// loader (a boot module's image class) or not a built-in one, or it has no
/// `classes` list; the VM keeps its claim. `Ok(None)`: not now -- the class
/// has no loader object yet (a class initialized before the application
/// loader exists), or its built-in loader has no `classes` list yet; the VM
/// keeps it pending and asks again once the application loader exists
/// (wave 139). The
/// upcall's own failure is returned (the VM drops its claim too).
pub fn record_vm_defined_class_in_builtin_loader(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
) -> Result<Option<bool>, MethodCallFailed> {
    let mut add_class =
        define_add_class_enabled() && ctx.method_exists(CL_CLASS, "addClass", ADD_CLASS_DESCRIPTOR);
    if !add_class && !ctx.builtin_loader_package_sync() {
        return Ok(Some(false));
    }
    let mirror = ctx.get_class_mirror(class_id);
    // The loader the mirror records (the mirror allocator fills it for an
    // adopted built-in loader); else `getClassLoader`'s answer, but only once
    // the application loader exists: this record must never be the call that
    // makes a built-in loader (a class linked during boot, before the JDK's
    // `ClassLoaders` adopted them, is recorded by a later door instead).
    let loader = match ctx.get_field_by_name(mirror, "classLoader") {
        Value::Object(Some(loader)) => loader,
        _ if app_loader_of(loader_tables(ctx)).is_some() => {
            let mirror_pin = ctx.pin_native_root(mirror);
            let loader = crate::lang_class::native_class_get_class_loader(
                ctx,
                &[Value::Object(Some(mirror))],
            );
            ctx.unpin_native_roots(mirror_pin);
            match loader? {
                Some(Value::Object(Some(loader))) => loader,
                // The boot loader's class (a boot module's image class).
                _ => return Ok(Some(false)),
            }
        }
        _ => return Ok(None),
    };
    // Re-read: `getClassLoader` may allocate (the mirror is not used again
    // before here otherwise).
    let mirror = ctx.get_class_mirror(class_id);
    if !loader_is_builtin(ctx, loader) {
        return Ok(Some(false));
    }
    // A built-in loader object without its `classes` list yet (a loader the
    // VM made before the JDK's adopted one ran its constructor): "not now"
    // while the application loader does not exist, so the VM keeps the class
    // pending and asks again once it does; after that, never (interpreter
    // round i7 wave 139, lane L5; L3's review item (c) of wave 137: the
    // answer was "never" even during boot).
    if add_class && loader_classes_list(ctx, loader).is_none() {
        if app_loader_of(loader_tables(ctx)).is_none() {
            return Ok(None);
        }
        add_class = false;
    }
    // Both operations can run Java and move the loader. Keep it rooted across
    // addClass, then publish the package using the refreshed reference.
    let loader_pin = ctx.pin_native_root(loader);
    let mut result = if add_class {
        invoke_add_class_claimed(ctx, loader, mirror).map(|_| ())
    } else {
        Ok(())
    };
    if result.is_ok() && ctx.builtin_loader_package_sync() {
        let package_result = if let Some(class_name) = ctx.class_name_of_id(class_id) {
            let package = class_name.rsplit_once('/').map_or("", |(pkg, _)| pkg);
            let dotted = package.replace('/', ".");
            let loader = ctx.read_native_pin(loader_pin, loader);
            crate::lang_class::sync_builtin_loader_package_for_class(
                ctx, loader, &dotted, class_id,
            )
            .and_then(|synced| {
                if synced {
                    Ok(())
                } else {
                    Err(RuntimeError::InternalError {
                        message: format!("could not publish package {dotted:?} to built-in loader"),
                    }
                    .into())
                }
            })
        } else {
            Err(RuntimeError::InternalError {
                message: format!("missing class name for package publication: {class_id:?}"),
            }
            .into())
        };
        result = result.and(package_result);
    }
    ctx.unpin_native_roots(loader_pin);
    // Package publication failure must fail this define, like HotSpot's
    // postDefineClass allocation, and must never replay a successful addClass.
    result.map(|_| Some(true))
}

/// [`invoke_add_class`] past its claim: the upcall, with the mirror pinned
/// across it. Returns the mirror re-read after the upcall.
pub fn invoke_add_class_claimed(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    mirror: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    let mirror_pin = ctx.pin_native_root(mirror);
    // zt t8/x5: HotSpot's upcall is `JavaCalls::call` on the one
    // `Universe::loader_addClass_method()` -- NOT a virtual dispatch
    // ([`call_loader_add_class`]; probe `Zt8X5AddClassShadow`, and the i7
    // wave-135 lane-L5 probe `L5W135AddClassExact`).
    let result = call_loader_add_class(ctx, loader, mirror).map(|_| ());
    let mirror = ctx.read_native_pin(mirror_pin, mirror);
    ctx.unpin_native_roots(mirror_pin);
    result.map(|()| mirror)
}

/// zt t9/df: `CRATONVM_ZGC_DEFINE_ADD_CLASS_MORE_DOORS` (default ON on ZGC,
/// `=0` kill; read per define). The `addClass` upcall at the definers t7/cl
/// left out (`zgc-zt8x5-wave-t7-review-residuals-CLOSED-20261019.md` item 3,
/// `zgc-zt7cl-define-path-and-class-name-residuals-SUPERSEDED-20261019.md` item 3):
/// `--compatible`'s `Proxy.newProxyInstance` / `getProxyClass` bridges, which
/// define the `$ProxyN` class themselves, `--compatible`'s `Lookup.defineClass`
/// door from a lookup class of a built-in loader, and `Unsafe.defineClass`
/// into the application loader. HotSpot records each of these classes in its
/// loader's `classes` list (probe `Zt8X5AddClassDefiners`). ZGC only by the
/// zt round's rule (G1 and Generational unchanged); nothing about it depends
/// on the collector. Also off with `CRATONVM_DEFINE_ADD_CLASS=0`.
///
/// zt t10/df2: `Zt8X5AddClassDefiners`' `proxy` / `lookup-app` rows never
/// reach these doors over a real JDK, in either mode. The JDK's
/// `Proxy.newProxyInstance` and `Lookup.defineClass` have `Code`, which
/// `vm_exec::resolve_dispatch` runs over a `Bridge`; they define through
/// `JavaLangAccess` -> `defineClass1` / `defineClass0`, whose upcall
/// [`define_add_class_enabled`] serves. Measured: t9a's `--compatible` row
/// printed `in=true` with this switch `=0`, and the t8b base (bridges with no
/// upcall) printed `in=true` too. What this switch decides over a real JDK:
/// `Unsafe.defineClass` into the application loader (a native door in both
/// modes; `Zt9DfDefinePath` `unsafe-app`) and, by reading, the
/// `Proxy.getProxyClass` bridge `force_native_over_real_jdk_bytecode` forces
/// under `--compatible` (row `Zt10Df2AddClassDoors` decides). The lookup arm serves
/// the synthetic JDK only. `CRATONVM_DBG_DEFINE_ADD_CLASS_DOOR=1` names the
/// door of each upcall ([`trace_add_class_door`]).
///
/// g1gen/c7 (the G1 / Generational half, `g1-and-generational-items-carried-
/// from-the-zgc-zt-round` item 7): on every collector. Nothing about it
/// depended on the collector; the zt round gated it on ZGC by its scope rule
/// alone.
pub fn define_add_class_more_doors_enabled(ctx: &dyn NativeContext) -> bool {
    let _ = ctx;
    true
}

/// zt t9/df: `CRATONVM_ZGC_DEFINE_INIT_AFTER_ADD_CLASS` (default ON on ZGC,
/// `=0` kill; read per define): `defineClass0` with `initialize` and no class
/// data runs `<clinit>` after its `addClass` upcall (as with class data),
/// not inside the define before it. HotSpot's `JVM_LookupDefineClass`
/// initializes after `define_instance_class` (which made the upcall)
/// returned, so an initializer that reads its own loader's `classes` list
/// finds itself there. The failure of the deferred initializer is the
/// `ExceptionInInitializerError` (or `Error`) the VM's own initialization
/// raised, not one rebuilt from the define's message text. Also off with
/// `CRATONVM_DEFINE_ADD_CLASS=0` (no upcall to wait for). ZGC only by the zt
/// round's rule.
///
/// g1gen/c7 (the G1 / Generational half, `g1-and-generational-items-carried-
/// from-the-zgc-zt-round` item 7): on every collector. Nothing about it
/// depended on the collector; the zt round gated it on ZGC by its scope rule
/// alone.
pub fn define_init_after_add_class_enabled(ctx: &dyn NativeContext) -> bool {
    let _ = ctx;
    define_add_class_enabled()
}

/// zt t9/df: the built-in loader object a class tagged `loader_tag`
/// (`NativeContext::loader_id_of_class`: `NATIVE_EXTENSION` = 1, the
/// platform loader; `NATIVE_APPLICATION` = 2, the application loader) was
/// defined into, for the `addClass` upcall of
/// [`define_add_class_more_doors_enabled`]'s doors; `None` for any other tag
/// or when the VM has not built that loader yet. The singleton is GC-rooted
/// and remapped (`gc_scan_loader_singleton_roots`), so the answer is current;
/// [`add_defined_class_to_loader`] still skips an object without a `classes`
/// list. Allocates nothing.
pub fn builtin_loader_for_add_class(
    ctx: &dyn NativeContext,
    loader_tag: i32,
) -> Option<ObjectRef> {
    let tables = loader_tables(ctx);
    if loader_tag == cratonvm_types::ClassLoaderId::NATIVE_APPLICATION as i32 {
        app_loader_of(tables)
    } else if loader_tag == cratonvm_types::ClassLoaderId::NATIVE_EXTENSION as i32 {
        platform_loader_of(tables)
    } else {
        None
    }
}

/// zt t9/df: `CRATONVM_ZGC_DEFINE_ADD_CLASS_RETRY` (default ON on ZGC, `=0`
/// kill; read per use). A define door publishes the class before its
/// `addClass` upcall; HotSpot's `define_instance_class` makes the upcall
/// first, so when the upcall fails (its list growth's `OutOfMemoryError`) the
/// class is not defined there and the program's retry defines it. Here the
/// class stays published and the retry met `LinkageError: duplicate class
/// definition` (`zgc-zt8x5-wave-t7-review-residuals-CLOSED-20261019.md` item 1;
/// `zgc-zt7cl-define-path-and-class-name-residuals-SUPERSEDED-20261019.md` item 1, the
/// racing `addClass` that takes the presized slot). With the switch on, the
/// failed upcall records the class ([`note_add_class_orphan_on_failure`])
/// and a retry of the SAME bytes by the same loader adopts it
/// ([`adopt_add_class_orphan`]): the upcall runs again, and the define
/// answers the class, as HotSpot's retry answers its fresh one. ZGC only by
/// the zt round's rule.
///
/// g1gen/c7 (the G1 / Generational half, `g1-and-generational-items-carried-
/// from-the-zgc-zt-round` item 7): on every collector. Nothing about it
/// depended on the collector; the zt round gated it on ZGC by its scope rule
/// alone.
pub fn define_add_class_retry_enabled(ctx: &dyn NativeContext) -> bool {
    let _ = ctx;
    true
}

/// zt t9/df: `CRATONVM_DBG_DEFINE_ADD_CLASS_RETRY=1` prints one stderr line
/// per orphan noted (`[define] add-class orphan noted: class_id=<n>`) and per
/// adoption (`[define] add-class orphan adopted: <name> class_id=<n> ok=<b>`),
/// the positive control of the `Zt6Nd4AddClassBoundary` rows.
pub fn dbg_define_add_class_retry() -> bool {
    false
}

/// zt t9/df: the digest an orphan row keeps of its defining bytes (std's
/// SipHash with its fixed keys: the same bytes give the same digest in every
/// run). Only a retry of the bytes that were defined adopts the class.
pub fn define_bytes_digest(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// zt t9/df: after a define door's `addClass` upcall (`added` is its
/// result), record `class_id` as an orphan when the upcall failed: the class
/// was published and its define is about to throw. Allocates nothing, runs
/// no Java. See [`define_add_class_retry_enabled`].
pub fn note_add_class_orphan_on_failure(
    ctx: &dyn NativeContext,
    added: &Result<ObjectRef, MethodCallFailed>,
    class_id: cratonvm_types::ClassId,
    bytes: &[u8],
) {
    if added.is_ok() || !define_add_class_retry_enabled(ctx) {
        return;
    }
    let digest = define_bytes_digest(bytes);
    loader_tables(ctx)
        .add_class_orphans
        .lock()
        .insert(class_id.as_u32(), digest);
    if dbg_define_add_class_retry() {
        eprintln!("[define] add-class orphan noted: class_id={}", class_id.as_u32());
    }
}

/// zt t16/df6 (`docs/feature-designs/zgc-architecture.md`
/// §1, corner 2 and D6): `CRATONVM_ZGC_DEFINE_BIND_AFTER_ADD_CLASS` (default
/// ON on ZGC, `=0` kill; read per define). HotSpot's
/// `SystemDictionary::define_instance_class` makes the `ClassLoader.addClass`
/// upcall BEFORE `update_dictionary`: while it runs, the name is held by the
/// define's placeholder and answered by no lookup, and an upcall that throws
/// leaves no class under the name, so the program's retry defines afresh --
/// with the same bytes or with OTHER bytes. CratonVM's doors published first,
/// so a failed upcall left the class behind: t9's orphan adoption serves a
/// retry of the SAME bytes, but a retry with other bytes met
/// `LinkageError: duplicate class definition` (corner 2). With the switch on,
/// a define door of a USER-DEFINED loader asks the class manager to keep the
/// name unbound (`DefineClassFull::bind_name_after_add_class`; the class is
/// stored, its mirror minted, its defining-loader row written, but no
/// by-name lookup answers it: [`register_defining_loader_unbound`]) and
/// settles it after the upcall ([`settle_unbound_define`]): bound when the
/// upcall returned, withdrawn when it threw. A second define of the name in
/// the meantime waits for the first to settle ([`wait_for_unbound_define`],
/// D6). Defines into a built-in loader's namespace (a class-path load can
/// define those names without the define's duplicate check), hidden classes,
/// redefinitions and defines that initialize keep binding at the define, and
/// the orphan adoption stays their fallback. Also off with
/// `CRATONVM_DEFINE_ADD_CLASS=0` (no upcall to wait for). ZGC only by the zt
/// round's rule. `loader_id` is the door's namespace (encoded as for
/// `define_class_full`): only a user-defined namespace asks, so a
/// built-in loader's define pays nothing for the switch (not even the
/// class-manager lock the binding takes).
///
/// g1gen/c7 (the G1 / Generational half, `g1-and-generational-items-carried-
/// from-the-zgc-zt-round` item 7): on every collector. Nothing about it
/// depended on the collector; the zt round gated it on ZGC by its scope rule
/// alone.
pub fn define_bind_after_add_class_enabled(ctx: &dyn NativeContext, loader_id: u32) -> bool {
    let _ = ctx;
    matches!(
        cratonvm_types::ClassLoaderId::from_native_id_or_default(loader_id),
        cratonvm_types::ClassLoaderId::UserDefined(_)
    ) && define_add_class_enabled()
}

/// zt t16/df6: settle a define door's late name binding after its `addClass`
/// upcall (`added` is the upcall's result; `requested` is whether the door
/// set `DefineClassFull::bind_name_after_add_class` and registered the
/// defining loader with [`register_defining_loader_unbound`]). Answers the
/// define's result and whether the class was WITHDRAWN (the door then notes
/// no orphan: no lookup can reach the class and a retry defines afresh).
///
/// * upcall returned: the class manager binds the name
///   (`NativeContext::bind_unbound_define`; `None` when the define bound it
///   itself, e.g. a stub upgraded in place), and the class joins its
///   loader's by-name index. If the name was taken meanwhile (`Some(false)`:
///   only an insert that skips the duplicate check could), the define is a
///   duplicate definition and the class stays nameless.
/// * upcall threw: the class manager withdraws the name
///   (`NativeContext::withdraw_unbound_define`), so the define leaves no
///   class, as HotSpot's. A define the class manager did not defer
///   (`false`: a built-in namespace) is indexed as before and keeps the
///   orphan path.
///
/// Not requested: `added` as given, nothing withdrawn, no lock taken. A GC
/// point only through the class manager's write lock; `added`'s mirror is
/// pinned across it.
pub fn settle_unbound_define(
    ctx: &mut dyn NativeContext,
    added: Result<ObjectRef, MethodCallFailed>,
    class_id: cratonvm_types::ClassId,
    requested: bool,
) -> (Result<ObjectRef, MethodCallFailed>, bool) {
    if !requested {
        return (added, false);
    }
    match added {
        Ok(mirror) => {
            let pin = ctx.pin_native_root(mirror);
            let bound = ctx.bind_unbound_define(class_id);
            let mirror = ctx.read_native_pin(pin, mirror);
            ctx.unpin_native_roots(pin);
            if bound == Some(false) {
                let class_name = ctx.class_name_of_id(class_id).unwrap_or_default();
                if dbg_define_add_class_retry() {
                    eprintln!(
                        "[define] add-class unbound define refused: {class_name} class_id={} (name taken)",
                        class_id.as_u32()
                    );
                }
                let refusal: MethodCallFailed =
                    cratonvm_types::error::LinkageError::DuplicateClassDefinition {
                        class_name,
                        loader: "<unknown>".to_string(),
                    }
                    .into();
                return (Err(refusal), false);
            }
            // zt t30/dc: the `bind-index` census row.
            let index_clock = cratonvm_classloading::define_census::phase_start();
            index_defining_loader(loader_tables(ctx), class_id.as_u32());
            cratonvm_classloading::define_census::phase_end(
                cratonvm_classloading::define_census::DefinePhase::BindIndex,
                index_clock,
            );
            if bound == Some(true) && dbg_define_add_class_retry() {
                eprintln!(
                    "[define] add-class unbound define bound: class_id={}",
                    class_id.as_u32()
                );
            }
            (Ok(mirror), false)
        }
        Err(e) => {
            let withdrawn = ctx.withdraw_unbound_define(class_id);
            if !withdrawn {
                index_defining_loader(loader_tables(ctx), class_id.as_u32());
            }
            if withdrawn && dbg_define_add_class_retry() {
                eprintln!(
                    "[define] add-class unbound define withdrawn: class_id={}",
                    class_id.as_u32()
                );
            }
            (Err(e), withdrawn)
        }
    }
}

/// zt t16/df6 (define page D6): the most `Thread.yield()` rounds a define
/// waits for another define of its name to settle (a valve: a define that
/// never settles must not hang this one).
pub const UNBOUND_DEFINE_WAIT_ROUNDS: u32 = 1 << 20;

/// zt t16/df6 (define page D6): before a define door defines `name` (empty:
/// the bytes' `this_class`) into `loader_id`, wait until no other define of
/// that name is between its define and its `addClass` upcall's end -- HotSpot's
/// second definer waits on the first one's placeholder, then defines when the
/// first failed (here: the name was withdrawn) or meets the duplicate
/// definition when it succeeded. Before, a retry racing a failing upcall met
/// `LinkageError` (the window between the publish and the orphan note).
///
/// Each round runs `Thread.yield()` through the VM, so the waiting thread
/// passes safepoints while the other thread's upcall (which can collect)
/// runs, as the `Proxy` cache's wait does. The caller must hold no raw
/// reference across this call (every door calls it with only pinned
/// references live). Off ([`define_bind_after_add_class_enabled`]) or with no
/// unbound define into `loader_id`: one class-manager read, no wait, no parse.
pub fn wait_for_unbound_define(
    ctx: &mut dyn NativeContext,
    name: &str,
    bytes: &[u8],
    loader_id: u32,
) -> Result<(), MethodCallFailed> {
    if !define_bind_after_add_class_enabled(ctx, loader_id)
        || ctx.unbound_define_of("", loader_id).is_none()
    {
        return Ok(());
    }
    let owned;
    let name: &str = if name.is_empty() {
        match extract_this_class_name(bytes) {
            Some(n) => {
                owned = n;
                &owned
            }
            None => return Ok(()),
        }
    } else {
        name
    };
    let mut rounds = 0u32;
    while rounds < UNBOUND_DEFINE_WAIT_ROUNDS {
        let Some(holder) = ctx.unbound_define_of(name, loader_id) else {
            break;
        };
        // zt t18/df7 (x13 review item 21c): the define holding the name is
        // THIS thread's own, re-entered inside its `addClass` upcall (only
        // agent or debugger code can run there). Only this thread can settle
        // it, so waiting would spin the whole valve. No wait: the define then
        // meets the duplicate definition (`classify_duplicate_define`'s
        // unbound arm), the refusal a define of a name already defined gets
        // (HotSpot's placeholder owner would block on itself instead).
        if unbound_define_held_here(&*ctx, holder) {
            if dbg_define_add_class_retry() {
                eprintln!("[define] add-class unbound define is this thread's own: {name}");
            }
            break;
        }
        rounds += 1;
        ctx.invoke_static("java/lang/Thread", "yield", "()V", &[])?;
    }
    if rounds > 0 && dbg_define_add_class_retry() {
        eprintln!("[define] add-class unbound define waited: {name} rounds={rounds}");
    }
    Ok(())
}

thread_local! {
    /// zt t18/df7 (x13 review items 21b, 21c): the unbound defines this
    /// thread holds between its define and the end of its door
    /// ([`hold_unbound_define_across`]), as `(vm_identity, class id)` --
    /// HotSpot's placeholder records its owner thread. Per thread, transient
    /// (one entry per define door on the stack): not compatibility state.
    pub static UNBOUND_DEFINES_HELD: std::cell::RefCell<Vec<(usize, u32)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// zt t18/df7 (x13 item 21c): is the unbound define `class_id` held by a
/// define door running on THIS thread ([`hold_unbound_define_across`])?
pub fn unbound_define_held_here(
    ctx: &dyn NativeContext,
    class_id: cratonvm_types::ClassId,
) -> bool {
    let key = (ctx.vm_identity(), class_id.as_u32());
    UNBOUND_DEFINES_HELD.with(|held| held.borrow().contains(&key))
}

/// zt t18/df7 (x13 review items 21b, 21c): run `door` -- a define door's
/// steps from its define to its answer, [`settle_unbound_define`] among them
/// -- with `class_id`'s unbound define marked as this thread's own
/// ([`unbound_define_held_here`]). `requested`: the door asked for the late
/// binding (`DefineClassFull::bind_name_after_add_class`); otherwise `door`
/// runs as it is, with nothing marked and nothing caught.
///
/// 21b: a panic inside `door` before its settle (caught later by the native
/// dispatcher, so the thread lives on) would leave the class manager's
/// unbound row forever: every later define of the name would wait out
/// [`UNBOUND_DEFINE_WAIT_ROUNDS`] and then meet the duplicate definition. The
/// unwind is caught here, the row withdrawn (a no-op after the settle: the
/// row is gone and `withdraw_unbound_define` answers `false`), and the panic
/// resumed with its own payload. A withdrawn class is left as a failed
/// upcall leaves it: nameless, its defining-loader row unindexed.
pub fn hold_unbound_define_across<T>(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
    requested: bool,
    door: impl FnOnce(&mut dyn NativeContext) -> T,
) -> T {
    if !requested {
        return door(ctx);
    }
    // zt t30/dc: the `bind-hold` census row, the push and the pop (the
    // door's own steps are its other rows).
    let push_clock = cratonvm_classloading::define_census::phase_start();
    let key = (ctx.vm_identity(), class_id.as_u32());
    UNBOUND_DEFINES_HELD.with(|held| held.borrow_mut().push(key));
    let push_ns = push_clock.map_or(0, |t| t.elapsed().as_nanos() as u64);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| door(&mut *ctx)));
    let pop_clock = push_clock.map(|_| std::time::Instant::now());
    UNBOUND_DEFINES_HELD.with(|held| {
        let mut held = held.borrow_mut();
        if let Some(at) = held.iter().rposition(|&k| k == key) {
            held.remove(at);
        }
    });
    if let Some(t) = pop_clock {
        cratonvm_classloading::define_census::phase_charge_ns(
            cratonvm_classloading::define_census::DefinePhase::BindHold,
            push_ns + t.elapsed().as_nanos() as u64,
        );
    }
    match outcome {
        Ok(answer) => answer,
        Err(payload) => {
            let withdrawn = ctx.withdraw_unbound_define(class_id);
            if withdrawn && dbg_define_add_class_retry() {
                eprintln!(
                    "[define] add-class unbound define withdrawn on unwind: class_id={}",
                    class_id.as_u32()
                );
            }
            std::panic::resume_unwind(payload)
        }
    }
}

/// zt t18/df7 (x13 review item 21d): a define door's define of `name` (empty:
/// the bytes' `this_class`) into `loader_id` failed as a duplicate definition
/// (`duplicate`: the door's own reading of the refusal). That duplicate may
/// be another define of the name whose `addClass` upcall is still in flight
/// (it began after this door's [`wait_for_unbound_define`], so the class
/// manager's duplicate check counted its unbound row): wait for it to settle
/// as the first wait does, and answer whether the name is FREE now (that
/// define was withdrawn: its upcall threw), in which case the door defines
/// once more -- HotSpot's second definer waits on the placeholder and then
/// defines its own bytes when the first one failed. `false`: the name is
/// bound (the duplicate stands, as HotSpot's), still held (this thread's own
/// define, or the valve ran out), or the late binding is off.
///
/// A GC point while it waits (`Thread.yield()`): the caller holds only pinned
/// references, and re-reads them (the protection domain in its options).
pub fn redefine_after_unbound_define_settles(
    ctx: &mut dyn NativeContext,
    name: &str,
    bytes: &[u8],
    loader_id: u32,
    duplicate: bool,
) -> Result<bool, MethodCallFailed> {
    if !duplicate || !define_bind_after_add_class_enabled(ctx, loader_id) {
        return Ok(false);
    }
    let owned;
    let name: &str = if name.is_empty() {
        match extract_this_class_name(bytes) {
            Some(n) => {
                owned = n;
                &owned
            }
            None => return Ok(false),
        }
    } else {
        name
    };
    wait_for_unbound_define(ctx, name, bytes, loader_id)?;
    if ctx.unbound_define_of(name, loader_id).is_some() {
        return Ok(false);
    }
    let free = ctx
        .class_id_defined_by_loader_exact(name, loader_id)
        .is_none();
    if free && dbg_define_add_class_retry() {
        eprintln!("[define] add-class unbound define settled withdrawn: {name} (define again)");
    }
    Ok(free)
}

/// zt t9/df: a define door's duplicate-definition arm (the loader object
/// itself defined `internal_name`), before it raises `LinkageError`: when the
/// existing class is an orphan of a failed `addClass` upcall
/// ([`note_add_class_orphan_on_failure`]) and `bytes` are the bytes it was
/// defined from, make the upcall now and answer the class (`Some(Ok(mirror))`)
/// -- what HotSpot's retry of a define whose upcall failed answers, a defined
/// class in its loader's list. `Some(Err)`: the upcall failed again (the
/// class stays an orphan). `None`: not an orphan, or other bytes; the caller
/// raises its `LinkageError` as before.
///
/// The row is claimed (removed) before the upcall, so of two threads retrying
/// together one adopts and the other meets the duplicate definition, as two
/// HotSpot defines of one name in one loader do. `loader` is current at entry;
/// `loader_id` is the door's namespace. A GC point (the upcall runs Java).
///
/// zt t11/df3: an empty `internal_name` (a NAMELESS define:
/// `defineClass(null, ..)`, `Unsafe.defineClass(null, ..)`) is the bytes'
/// own `this_class`, as the define named the class. Before, the look-up below
/// asked for `""`, found nothing, and a nameless retry of an orphan met the
/// duplicate definition. Bytes without a readable `this_class` adopt nothing.
///
/// zt t14/df5 (define page §1, corner 3): `domain` is the RETRY's
/// `ProtectionDomain` argument for a door that installs one in the mirror
/// (`defineClass0/1/2`: `Some(pd)`, current at entry); `None` for a door that
/// installs none (`Unsafe.defineClass`). With
/// [`define_orphan_retry_domain_enabled`] the adoption writes it into the
/// mirror's `protectionDomain` before the upcall: HotSpot's retry is a fresh
/// define with the retry's domain, and the orphan's mirror was never handed to
/// Java (its define threw at the upcall; since t12 `findLoadedClass` hides
/// it), so the first define's domain was visible only through this adoption.
pub fn adopt_add_class_orphan(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    internal_name: &str,
    loader_id: u32,
    bytes: &[u8],
    domain: Option<Option<ObjectRef>>,
) -> Option<Result<ObjectRef, MethodCallFailed>> {
    if !define_add_class_retry_enabled(ctx)
        || loader_tables(ctx).add_class_orphans.lock().is_empty()
    {
        return None;
    }
    let internal_name: std::borrow::Cow<'_, str> = if internal_name.is_empty() {
        std::borrow::Cow::Owned(extract_this_class_name(bytes)?)
    } else {
        std::borrow::Cow::Borrowed(internal_name)
    };
    let internal_name: &str = &internal_name;
    let loader_pin = ctx.pin_native_root(loader);
    // Pinned above the loader: `unpin_native_roots(loader_pin)` releases both.
    let domain_pin = match domain {
        Some(Some(pd)) => Some((ctx.pin_native_root(pd), pd)),
        _ => None,
    };
    // zt t11/df3 (x8 C5 / hand-off D1): the namespace fallback is for a
    // built-in loader (no defining-loader row), and a namespace is not a
    // loader: the application and platform loaders share namespace 0, so an
    // `Unsafe.defineClass` retry into one of them (whose duplicate arm is not
    // gated on `SameLoaderObject`) could adopt the other's orphan. The
    // fallback's class must also name THIS loader object in its mirror's
    // `classLoader` (every door writes it after the define, before the
    // upcall); otherwise nothing is adopted and the caller's `LinkageError`
    // stands. A null `classLoader` is accepted for the application loader
    // only: a platform-loader define always writes its loader there
    // (`unsafe_natives::unsafe_define_mirror_with_platform`), so a null one is
    // never the platform loader's class.
    let class_id = match class_defined_by_this_loader_object(ctx, loader, internal_name) {
        Some(cid) => Some(cid),
        None => ctx
            .class_id_defined_by_loader_exact(internal_name, loader_id)
            .filter(|cid| {
                let mirror = ctx.get_class_mirror(*cid);
                let loader_now = ctx.read_native_pin(loader_pin, loader);
                match ctx.get_field_by_name(mirror, "classLoader") {
                    Value::Object(Some(o)) => o == loader_now,
                    Value::Object(None) => is_application_class_loader(ctx, loader_now),
                    _ => false,
                }
            }),
    };
    let digest = define_bytes_digest(bytes);
    let claimed = class_id.filter(|cid| {
        let mut orphans = loader_tables(&*ctx).add_class_orphans.lock();
        match orphans.get(&cid.as_u32()) {
            Some(&kept) if kept == digest => {
                orphans.remove(&cid.as_u32());
                true
            }
            _ => false,
        }
    });
    let Some(class_id) = claimed else {
        ctx.unpin_native_roots(loader_pin);
        return None;
    };
    let mirror = ctx.get_class_mirror(class_id);
    if domain.is_some() && define_orphan_retry_domain_enabled() {
        let pd = domain_pin.map(|(pin, pd)| ctx.read_native_pin(pin, pd));
        ctx.set_field_by_name(mirror, "protectionDomain", Value::Object(pd));
    }
    let loader = ctx.read_native_pin(loader_pin, loader);
    let added = add_adopted_orphan_to_loader(ctx, loader, mirror);
    ctx.unpin_native_roots(loader_pin);
    if added.is_err() {
        // Still an orphan: a later retry may adopt it.
        loader_tables(ctx)
            .add_class_orphans
            .lock()
            .insert(class_id.as_u32(), digest);
    }
    if dbg_define_add_class_retry() {
        eprintln!(
            "[define] add-class orphan adopted: {internal_name} class_id={} ok={}",
            class_id.as_u32(),
            added.is_ok()
        );
    }
    Some(added)
}

/// zt t11/df3: [`adopt_add_class_orphan`] for a NAMELESS define door
/// (`lang_system`'s `defineClass1` / `defineClass2` with a null name). Those
/// doors classify a duplicate by the caller's name
/// (`lang_system::classify_duplicate_define`), which answers "not a duplicate"
/// for an empty one, so their adoption arm was never reached and a nameless
/// retry of an orphan met the duplicate definition's `LinkageError` (HotSpot
/// defines it). `msg` is the define's refusal text: only a duplicate
/// definition is offered to the adoption, which then names the class by the
/// bytes' `this_class` and adopts it only when THIS loader object defined it
/// from these bytes. `None` otherwise (the caller's error stands). zt t12/x9:
/// with the retry switch on (ZGC) it takes the orphan map's lock once for the
/// empty-map check; it parses the bytes (and allocates the name) only when an
/// orphan is pending.
pub fn adopt_nameless_add_class_orphan(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    loader_id: u32,
    bytes: &[u8],
    msg: &str,
    domain: Option<Option<ObjectRef>>,
) -> Option<Result<ObjectRef, MethodCallFailed>> {
    if !(msg.contains("DuplicateClassDefinition") || msg.contains("already defined")) {
        return None;
    }
    adopt_add_class_orphan(ctx, loader, "", loader_id, bytes, domain)
}

/// zt t31 (define page residual, row `df6_a1_orphan`): the upcall of an
/// orphan's adoption. The failed upcall that made the orphan had claimed the
/// class's one built-in loader record (`invoke_add_class`, interpreter i137
/// L5), so the adoption's [`add_defined_class_to_loader`] found it claimed and
/// skipped the upcall: the adopted class was missing from the application
/// loader's `classes` (HotSpot lists it; its retry is the class's only
/// define). The adoption makes the upcall under that claim, which it takes
/// first in case nothing held it. `CRATONVM_ZGC_DEFINE_ORPHAN_ADOPT_RECORDS`
/// (default ON, `=0`: through the claim, as before).
pub fn add_adopted_orphan_to_loader(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    mirror: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    if !define_add_class_enabled()
        || !ctx.method_exists(CL_CLASS, "addClass", ADD_CLASS_DESCRIPTOR)
        || loader_classes_list(ctx, loader).is_none()
    {
        return Ok(mirror);
    }
    if let Some(cid) = ctx.class_id_from_mirror(mirror) {
        let _ = ctx.claim_builtin_loader_class_record(cid);
    }
    let class_id = ctx.class_id_from_mirror(mirror);
    let loader_pin = ctx.pin_native_root(loader);
    let result = invoke_add_class_claimed(ctx, loader, mirror).and_then(|mirror| {
        if let Some(class_id) = class_id {
            let loader = ctx.read_native_pin(loader_pin, loader);
            sync_package_for_defined_class(ctx, loader, class_id).map(|()| mirror)
        } else {
            Ok(mirror)
        }
    });
    ctx.unpin_native_roots(loader_pin);
    result
}



/// zt t14/df5 (define page §1, corner 3): `CRATONVM_ZGC_DEFINE_ORPHAN_RETRY_DOMAIN`
/// (default ON, `=0` kill; read per adoption, which is OOM-rare). ZGC only
/// through [`define_add_class_retry_enabled`], which gates every adoption. An
/// adopted orphan's mirror takes the retry's `ProtectionDomain`
/// ([`adopt_add_class_orphan`]'s `domain`); `=0` keeps the first define's.
pub fn define_orphan_retry_domain_enabled() -> bool {
    true
}

