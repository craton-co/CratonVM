#[allow(unused_imports)]
use super::*;

impl NativeMethodRegistry {
    /// Declare that this slot is dispatched through a path that does **not**
    /// call [`record_invocation`](Self::record_invocation), so its count is a
    /// floor rather than a total.
    ///
    /// Call it **once, cold, at bind time** — where a call site is wired to a
    /// pre-resolved callback — not per dispatch. The whole point of the bit is
    /// that the bypassing paths cannot afford a per-call atomic; paying one to
    /// announce that you are not paying one would be absurd.
    ///
    /// # The sites that owe this call
    ///
    /// Measured 2026-08-17 and recorded in
    /// `docs/feature-designs/native-dispatch-architecture.md`.
    /// None of them calls this yet — this method is the landing point the
    /// nominations in that record are written against, and it is deliberately
    /// on the registry rather than in each caller so there is one contract:
    ///
    /// * `vm/src/runtime/interpreter/dispatch_static.rs`, where
    ///   `populate_invoke_cache` installs `CachedInvokeTarget::Intrinsic`, and
    ///   the matching site in `dispatch_virtual.rs`.
    /// * `vm/src/jit/helpers.rs`, where `jit::try_compile` binds a call site to
    ///   `jit_hashmap_put_direct` / `jit_hashmap_get_direct` /
    ///   `jit_concurrent_hashmap_get_direct` /
    ///   `jit_string_latin1_to_lower_direct`.
    /// * `vm/src/vm/vm_exec.rs`'s `invoke_or_native` arms, which resolve with
    ///   `find_with_kind` and hold no id — that one needs a `resolve_id` first
    ///   and is the least attractive of the three.
    ///
    /// # Sticky by design
    ///
    /// Never cleared, including by re-registration of the triple. A
    /// re-registration updates the slot in place, but a call site already bound
    /// to the *previous* callback keeps calling it, so the count keeps being
    /// short. Clearing the bit would make a stale claim of completeness — the
    /// one direction this instrument must not err in.
    ///
    /// An out-of-range `id` is ignored, exactly as in `record_invocation`.
    #[inline]
    pub fn mark_invocations_incomplete(&self, id: NativeMethodId) {
        if let Some(flag) = self.slot_invocations_incomplete.get(id.index()) {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Whether [`invocations_of_id`](Self::invocations_of_id) for this slot is
    /// a total (`Some(true)`) or a floor (`Some(false)`); `None` if the handle
    /// does not belong to this registry. O(1).
    ///
    /// `Some(true)` means only that no dispatch path has *declared* itself a
    /// bypass. It is a claim carried by the code, not a proof — read
    /// [`record_invocation`](Self::record_invocation)'s bypass list before
    /// treating it as one.
    #[inline]
    pub fn invocations_complete(&self, id: NativeMethodId) -> Option<bool> {
        self.slot_invocations_incomplete
            .get(id.index())
            .map(|f| !f.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// How many slots have been declared incomplete
    /// ([`mark_invocations_incomplete`](Self::mark_invocations_incomplete)).
    ///
    /// One number a dump header can print so a reader is told the column is a
    /// floor *before* reading the column, rather than after quoting it. Cold:
    /// one relaxed load per slot, run once at report time, the same shape and
    /// the same justification as
    /// [`invocations_of_kind`](Self::invocations_of_kind).
    pub fn slots_with_incomplete_invocations(&self) -> usize {
        self.slot_invocations_incomplete
            .iter()
            .filter(|f| f.load(std::sync::atomic::Ordering::Relaxed))
            .count()
    }

    /// Total dispatches recorded for slots currently classified `kind`
    /// (`docs/book/src/user-guide/real-jdk-mode.md` §4). The CI gate reads this with
    /// `NativeKind::SyntheticStub` and asserts zero.
    ///
    /// # What a zero from this gate does and does not prove
    ///
    /// It is a sum of [`record_invocation`](Self::record_invocation) counters,
    /// so it inherits that method's blind spots exactly: a synthetic stub
    /// dispatched only through the interpreter's intrinsic table or a JIT thin
    /// direct-call helper contributes **nothing** here. The assertion is
    /// therefore sound in one direction only — non-zero is proof a stub ran,
    /// zero is not proof none did.
    ///
    /// **G33-1's reason the gate had not been wrong yet has EXPIRED.** That
    /// record said "neither bypass family currently serves a `SyntheticStub`
    /// (both tables are `java.base` intrinsics and collection fast paths, all
    /// `Bridge` or `Intrinsic`)". A **third** family has landed since —
    /// `vm/src/runtime/interpreter/invoke.rs`'s `UNCOUNTED_STACKLESS_NATIVES`,
    /// the stackless-invoke path that returns `Handled` before the census
    /// increment — and **four of its fourteen triples are registered
    /// `SyntheticStub`**:
    ///
    /// ```text
    /// java/lang/foreign/DowncallHandle  type        ()Ljava/lang/invoke/MethodType;
    /// java/lang/foreign/DowncallHandle  invoke      ([Ljava/lang/Object;)Ljava/lang/Object;
    /// java/lang/foreign/DowncallHandle  invokeExact ([Ljava/lang/Object;)Ljava/lang/Object;
    /// java/lang/foreign/DowncallHandle  invokeBasic ([Ljava/lang/Object;)Ljava/lang/Object;
    /// ```
    ///
    /// MEASURED — `--dump-native-registry`, `9ae371468`, default (`compatible`)
    /// mode: all four `kind = synthetic-stub`, `owns_slot = true`, registered
    /// at `native-builtins/src/phases_late/foreign_ffm.rs:4247…4265`. So a
    /// Panama downcall taken through the stackless path runs a `SyntheticStub`
    /// and contributes **nothing** to this total: the gate reads zero and says
    /// PASS. That is the quiet failure, not the loud one.
    ///
    /// Under `--real-jdk` those four are not registered at all (same dump,
    /// `mode: jdk-only`: `synthetic-stub` count **0**, total 10,691), so the
    /// jdk-only gate is safe **today, by mode** — a fact about what
    /// `foreign_ffm.rs` currently registers, not a property of this method.
    ///
    /// **Do not read a zero from this method on its own.** Use
    /// [`invocations_of_kind_checked`](Self::invocations_of_kind_checked),
    /// which returns the same total alongside
    /// [`incomplete_slots_of_kind`](Self::incomplete_slots_of_kind) so a gate
    /// can tell a *conclusive* zero from an *uninformative* one and go red
    /// rather than quiet. See
    /// `docs/feature-designs/native-dispatch-architecture.md`
    /// and
    /// `docs/feature-designs/native-dispatch-architecture.md`.
    ///
    /// **Derived, not maintained.** Three per-kind global counters would make
    /// this O(1), but at the price of a SECOND contended atomic RMW on every
    /// native dispatch: `record_invocation`'s per-slot `fetch_add` is spread
    /// across ~3,100 independent cache lines, whereas a per-kind global is one
    /// line every thread in the VM would ping-pong. This scan is ~3,100 relaxed
    /// loads, run once, cold, at report time. The hot path wins.
    ///
    /// **Consequence of deriving it, by design:** the kind comes from the
    /// slot's *current* classification, and a re-registration of the same triple
    /// under a different kind rewrites that slot in place while its counter
    /// keeps accumulating. So a triple registered as `SyntheticStub`, invoked,
    /// then re-registered as `Bridge` reports its pre-promotion invocations
    /// under `Bridge`. That is the honest reading of "which kind is this slot
    /// now", it errs toward under-reporting stubs only in the direction where a
    /// stub was *fixed*, and `census()` still shows the supersession via
    /// `overwrote`.
    pub fn invocations_of_kind(&self, kind: NativeKind) -> u64 {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.kind == kind)
            .map(|(index, _)| self.invocation_total(index).unwrap_or(0))
            .sum()
    }

    /// How many slots currently classified `kind` have declared their
    /// invocation count a floor
    /// ([`mark_invocations_incomplete`](Self::mark_invocations_incomplete)).
    ///
    /// [`slots_with_incomplete_invocations`](Self::slots_with_incomplete_invocations)
    /// narrowed to one kind, and it is the number that makes
    /// [`invocations_of_kind`](Self::invocations_of_kind) readable: **a zero
    /// total means "none ran" only when this is also zero.** Non-zero here
    /// means at least one slot of that kind is wired to a dispatch path that
    /// does not tick the counter, so the total is a floor and a zero total
    /// carries no information at all.
    ///
    /// Same cost and same justification as `invocations_of_kind`: one relaxed
    /// load per slot, cold, at report time.
    pub fn incomplete_slots_of_kind(&self, kind: NativeKind) -> usize {
        self.slots
            .iter()
            .zip(self.slot_invocations_incomplete.iter())
            .filter(|(slot, _)| slot.kind == kind)
            .filter(|(_, flag)| flag.load(std::sync::atomic::Ordering::Relaxed))
            .count()
    }

    /// [`invocations_of_kind`](Self::invocations_of_kind) paired with the one
    /// number that says whether it may be believed.
    ///
    /// This is the form a CI gate must read. The bare total is sound in one
    /// direction only — non-zero proves a stub ran, zero does not prove none
    /// did — and a gate that asserts `== 0` on it inherits that asymmetry
    /// silently: when the instrument goes blind the assertion goes **quiet**,
    /// which is the worst thing a CI check can do. [`KindInvocations::is_conclusive_zero`]
    /// is the predicate that distinguishes "measured none" from "cannot tell",
    /// and [`KindInvocations::is_clean`] is the assertion itself — it refuses
    /// both a non-zero total and an unmeasurable one.
    pub fn invocations_of_kind_checked(&self, kind: NativeKind) -> KindInvocations {
        KindInvocations {
            kind,
            total: self.invocations_of_kind(kind),
            incomplete_slots: self.incomplete_slots_of_kind(kind),
        }
    }

    /// The `(class, method, descriptor)` triple that currently owns `id`.
    /// Diagnostics (native-ring dumps, tracing), not a dispatch input: nothing
    /// chooses a native by it. One reader decides whether a cached entry is
    /// dropped (the interpreter's owner-scoped redefine eviction, WK-2, round
    /// 15 wave 4 lane walk2): it names the class that registered a cached
    /// slot, which is this triple's class for as long as the slot lives.
    #[inline]
    pub fn triple_of(&self, id: NativeMethodId) -> Option<(&str, &str, &str)> {
        let slot = self.slots.get(id.index())?;
        self.registrations.get(slot.reg_index as usize)
    }

    /// [`find`](Self::find) with a precomputed digest. See
    /// [`resolve_id_by_key`](Self::resolve_id_by_key) for the verification
    /// contract.
    #[inline]
    pub fn find_by_key(
        &self,
        key: NativeMethodKey,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<NativeCallback> {
        let id = self.resolve_id_by_key(key, class_name, method_name, descriptor)?;
        self.callback_of(id)
    }

    /// [`find_with_kind`](Self::find_with_kind) with a precomputed digest.
    ///
    /// Unlike `find_with_kind` this reports the slot's true kind on the
    /// descriptor-quirk path too; `find_with_kind`'s `Bridge` fallback there is
    /// preserved only for the existing callers that depend on it.
    #[inline]
    pub fn find_with_kind_by_key(
        &self,
        key: NativeMethodKey,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<(NativeCallback, NativeKind)> {
        let id = self.resolve_id_by_key(key, class_name, method_name, descriptor)?;
        let slot = self.slots.get(id.index())?;
        Some((slot.callback, slot.kind))
    }

    /// Reset the [`lookup_census`] so a measurement covers only what follows.
    /// Cold; exists so a caller can exclude VM boot from the ratio.
    pub fn reset_lookup_census() {
        lookup_census::reset();
    }

    /// Whether a native MAY be registered for some method of `class_name`:
    /// `false` proves none is (the class-name prefilter below, one hash and a
    /// set probe). Interpreter round i1 wave 45, lane L3: a class an agent's
    /// load-time transformer rewrote cedes its registered natives to its woven
    /// bytecode, and the load path asks this first so a class the registry
    /// never names pays nothing (`instrument::pre_transform_for_load`).
    #[inline]
    pub fn class_may_have_natives(&self, class_name: &str) -> bool {
        self.class_prefilter(class_name).is_some()
    }

    /// The class-name prefilter: `Some(prefix_state)` when this class MIGHT
    /// register a native, `None` when it provably registers none.
    ///
    /// One-sided by construction, in the safe direction. `classes_with_natives`
    /// holds the digest of every registered class name, so a registered class
    /// can never be absent; an unregistered class whose digest collides with a
    /// registered one merely falls through to the full path and misses there.
    ///
    /// Returning the prefix state rather than a bool is what lets the caller
    /// finish the digest without re-walking the class name — see
    /// [`native_class_hash`].
    #[inline]
    pub(in crate::registry) fn class_prefilter(&self, class_name: &str) -> Option<(u64, u64)> {
        // nio21 wave 17 L1: the same state, from the per-thread memo when this
        // thread last hashed the same name (`native_class_hash_memoized`).
        let class_state = if self.class_hash_memo {
            native_class_hash_memoized(class_name)
        } else {
            native_class_hash(class_name)
        };
        self.classes_with_natives
            .contains(&class_state)
            .then_some(class_state)
    }

    /// Slot lookup by exact triple (no descriptor rewriting).
    #[inline]
    pub(in crate::registry) fn slot_for_exact(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<&NativeSlot> {
        // Prefilter on the class name alone before finishing the digest: see
        // `native_class_hash`. Exact for a miss, may false-positive into the
        // full path.
        let class_state = self.class_prefilter(class_name)?;
        let key = native_method_hash_from(class_state, method_name, descriptor);
        let idx = self.slot_index_for_key(key, class_name, method_name, descriptor)?;
        self.slots.get(idx as usize)
    }

    /// Retire this registry's `io/netty/internal/tcnative/**` stand-ins for
    /// this VM.
    ///
    /// Called from the library-load path just before Netty's real
    /// `netty_tcnative` `JNI_OnLoad` runs (that `JNI_OnLoad` initializes
    /// `io.netty.internal.tcnative.SSL`, whose `<clinit>` calls the natives
    /// it has just registered), and undone by
    /// [`unmute_netty_tcnative_stubs`](Self::unmute_netty_tcnative_stubs) if
    /// it fails. See the [`netty_tcnative_muted`](Self) field doc for why the
    /// decision cannot live at a dispatch site.
    pub fn mute_netty_tcnative_stubs(&self) {
        self.netty_tcnative_muted
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Bring the `io/netty/internal/tcnative/**` stand-ins back: the real
    /// library's `JNI_OnLoad` failed (it unregisters what it registered), so
    /// the stubs are again the only thing behind the package.
    pub fn unmute_netty_tcnative_stubs(&self) {
        self.netty_tcnative_muted
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether [`mute_netty_tcnative_stubs`](Self::mute_netty_tcnative_stubs)
    /// has been called on this registry.
    pub fn netty_tcnative_stubs_muted(&self) -> bool {
        self.netty_tcnative_muted
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Slot index for an exact triple, given the class prefix state the caller
    /// already obtained from [`class_prefilter`](Self::class_prefilter).
    ///
    /// Exists so an entry point can prefilter ONCE and still reach both the
    /// exact lookup and the quirk fallback. `find` and `find_with_kind` used to
    /// call `slot_for_exact` (which prefilters) and then, on a miss, a second
    /// prefilter to decide whether the quirk path was worth entering — hashing
    /// the class name twice for every miss on a class that DOES register
    /// natives, which is the common miss on a collection-heavy workload.
    #[inline]
    pub(in crate::registry) fn slot_index_from_state(
        &self,
        class_state: (u64, u64),
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<u32> {
        let key = native_method_hash_from(class_state, method_name, descriptor);
        self.slot_index_for_key(key, class_name, method_name, descriptor)
    }

    /// The one place a 128-bit digest is turned into a slot index — and the one
    /// place the full name is verified.
    ///
    /// A digest hit is treated as a *candidate*: we fetch the triple the
    /// candidate slot was registered under and compare all three strings. A
    /// mismatch is reported as a **miss**, not as a callback.
    ///
    /// This is not defensive theater. `classloading/src/class_manager.rs` (the
    /// `loaded_classes` field doc, "Round 4 audit fix (CRIT)") records a
    /// shipped defect of exactly this shape: a `name_to_id: FxHashMap<u64,
    /// ClassId>` keyed by a raw FNV-1a digest with no name verification, where
    /// any collision returned the wrong `ClassId` and caused silent type
    /// confusion downstream. Three `str` comparisons of already-hot cache lines
    /// are cheaper than the byte-at-a-time hash that produced the key, so this
    /// is bought at essentially no cost.
    #[inline]
    pub(in crate::registry) fn slot_index_for_key(
        &self,
        key: (u64, u64),
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<u32> {
        let idx = *self.slot_by_key.get(&key)?;
        let slot = self.slots.get(idx as usize)?;
        let (c, m, d) = self.registrations.get(slot.reg_index as usize)?;
        if c == class_name && m == method_name && d == descriptor {
            // Retired stand-ins (see `netty_tcnative_muted`). This is the one
            // place every resolution route — `slot_for_exact`, the
            // descriptor-quirk rewrite, and the precomputed-digest
            // `resolve_id_by_key` — turns a digest into a slot, so a single
            // check here cannot be routed around. It sits on the confirmed-hit
            // edge, past the `slot_by_key` probe, so a miss (the overwhelmingly
            // common case) never even loads the flag.
            if self
                .netty_tcnative_muted
                .load(std::sync::atomic::Ordering::Relaxed)
                && class_name.starts_with(NETTY_TCNATIVE_PACKAGE)
            {
                return None;
            }
            Some(idx)
        } else {
            None
        }
    }

    /// Look up a native method implementation (zero allocation on the
    /// fast path; zero allocation on a miss with a clean descriptor).
    #[inline]
    pub fn find(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<NativeCallback> {
        lookup_census::probe(lookup_census::FIND);
        // One prefilter for both the exact lookup and the quirk fallback: a
        // class that registers nothing cannot be rescued by a descriptor
        // rewrite either, since every variant is looked up under the SAME class
        // name and that class has no key in `slot_by_key` under any descriptor.
        let class_state = self.class_prefilter(class_name)?;
        if let Some(idx) =
            self.slot_index_from_state(class_state, class_name, method_name, descriptor)
        {
            return self.slots.get(idx as usize).map(|slot| slot.callback);
        }

        // AUDIT 2026-05-17 (Fix 4): the compatibility-variants path was
        // previously building a `Vec<String>` on every miss, even for
        // perfectly-formed descriptors that needed no rewriting (which
        // is the common case — a real miss is usually "this native is
        // not implemented", not "the descriptor needed a fixup"). Now
        // we short-circuit: only walk the variant logic when the
        // descriptor *actually* has a quirk worth rewriting. Clean
        // descriptors return `None` with zero allocation.
        let quirked = Self::find_with_descriptor_quirks(self, class_name, method_name, descriptor);
        if quirked.is_none() {
            lookup_census::note_miss(class_name, method_name, descriptor);
        }
        quirked
    }

    /// Cold path of `find`: try compatibility-rewritten descriptor
    /// variants. Returns `None` if the descriptor is already clean
    /// (no whitespace, no NUL, no `\r\n`, and any `L…` return type
    /// already correctly terminated with `;`).
    #[inline]
    pub(in crate::registry) fn find_with_descriptor_quirks(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<NativeCallback> {
        let id = self.resolve_id_with_descriptor_quirks(class_name, method_name, descriptor)?;
        self.callback_of(id)
    }

    /// Handle-returning form of [`find_with_descriptor_quirks`]. The quirk
    /// rewrite is the only part of resolution that is not a pure function of
    /// the exact triple, so it has to produce a slot index too — otherwise a
    /// call site that memoizes a `NativeMethodId` would silently lose the
    /// compatibility rewrites that `find` performs.
    #[cold]
    #[inline(never)]
    pub(in crate::registry) fn resolve_id_with_descriptor_quirks(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<NativeMethodId> {
        lookup_census::probe(lookup_census::QUIRKS);
        // Cheap precheck: if the descriptor has none of the quirks the
        // rewrites target, there are no variants to try — bail before
        // touching the allocator.
        //
        // One backward byte pass answers both questions. `descriptor.rfind(')')`
        // went through `core::str::pattern::CharSearcher`, whose UTF-8 reverse
        // search showed up as its own 0.73% line in a flat profile of
        // `AdaptiveByteBufAllocatorTest` — for a byte that is ASCII by
        // definition in a JVM descriptor (JVMS 4.3.3).
        let bytes = descriptor.as_bytes();
        let mut has_whitespace_or_nul = false;
        let mut rparen = None;
        for (i, &b) in bytes.iter().enumerate() {
            if b.is_ascii_whitespace() || b == b'\0' {
                has_whitespace_or_nul = true;
            }
            if b == b')' {
                rparen = Some(i);
            }
        }
        let object_return_quirk = match rparen {
            Some(i) => {
                let ret = &bytes[i + 1..];
                ret.first() == Some(&b'L') && ret.last() != Some(&b';')
            }
            None => false,
        };
        if !has_whitespace_or_nul && !object_return_quirk {
            return None;
        }

        // At most 3 candidates (trimmed, no_newlines, return-type fixup).
        // Stash them inline in a small fixed-size array — no Vec growth.
        let mut variants: [Option<String>; 3] = [None, None, None];
        let mut n_variants = 0usize;

        let trimmed = descriptor.trim_matches(|c: char| c.is_ascii_whitespace() || c == '\0');
        if trimmed != descriptor {
            variants[n_variants] = Some(trimmed.to_string());
            n_variants += 1;
        }

        let no_newlines = trimmed.replace(['\r', '\n'], "");
        if no_newlines != descriptor
            && !variants
                .iter()
                .take(n_variants)
                .any(|v| v.as_deref() == Some(no_newlines.as_str()))
        {
            variants[n_variants] = Some(no_newlines.clone());
            n_variants += 1;
        }

        for base in [trimmed, no_newlines.as_str()] {
            if let Some(rparen) = base.rfind(')') {
                let (args_part, ret_part) = base.split_at(rparen + 1);
                if ret_part.starts_with('L') {
                    let candidate = if !ret_part.ends_with(';') {
                        Some(format!("{args_part}{ret_part};"))
                    } else {
                        ret_part
                            .strip_suffix(';')
                            .map(|stripped| format!("{args_part}{stripped}"))
                    };
                    if let Some(cand) = candidate {
                        if cand != descriptor
                            && n_variants < variants.len()
                            && !variants
                                .iter()
                                .take(n_variants)
                                .any(|v| v.as_deref() == Some(cand.as_str()))
                        {
                            variants[n_variants] = Some(cand);
                            n_variants += 1;
                        }
                    }
                }
            }
        }

        for candidate_desc in variants
            .iter()
            .take(n_variants)
            .filter_map(|s| s.as_deref())
        {
            let k = native_method_hash(class_name, method_name, candidate_desc);
            // Verify against the REWRITTEN descriptor — that is the triple the
            // native was registered under, and the one the digest encodes.
            if let Some(idx) = self.slot_index_for_key(k, class_name, method_name, candidate_desc) {
                return Some(NativeMethodId::from_u32(idx));
            }
        }
        None
    }

    /// NEW-14: copy every native method currently registered under
    /// `from_class` to also be reachable under `to_class`.
    ///
    /// Used by the JDBC registration path to make every
    /// `PreparedStatement` method also dispatch when invoked on a
    /// `CallableStatement` instance (the JDK's `CallableStatement`
    /// interface extends `PreparedStatement`, so every PS method is
    /// valid on a CS instance — but native dispatch is keyed by class
    /// name, not Java inheritance, so we have to populate both keys
    /// explicitly). The function:
    ///
    ///   1. Walks the `registrations` log (the per-`register()`
    ///      append-only list of `(class, method, descriptor)` triples)
    ///      to find every entry whose class equals `from_class`.
    ///   2. For each match, re-registers the same callback under
    ///      `to_class` with the same method name and descriptor.
    ///
    /// Idempotent: calling twice produces the same final state. Any
    /// existing registration on `to_class` is overwritten (matching
    /// the behavior of `register` itself, which re-registers silently
    /// when the triple is identical).
    ///
    /// Deliberately NOT `#[track_caller]` (2026-07-31, Real-JDK §4 provenance).
    /// Propagating the caller here would attribute every aliased row to the
    /// JDBC registrar that asked for the alias, which is a lie: nobody wrote
    /// those `CallableStatement` registrations, this loop synthesized them. Left
    /// off, `Location::caller()` inside `register()` resolves to the
    /// `self.register(...)` line below, so a census row reading
    /// `native-api/src/registry.rs:<alias_class>` is self-describing —
    /// "this native exists because of an alias, delete the source registration
    /// instead".
    pub fn alias_class(&mut self, from_class: &str, to_class: &str) {
        // Collect first to avoid mutating while iterating, and to drop
        // the immutable borrow on `self.registrations` before we call
        // `self.register()` below. Two passes (rather than one closure that
        // both walks `registrations` and probes the slot table) so the
        // `registrations` borrow is provably released before the whole-`self`
        // lookup — a rare boot-time path, so the extra `Vec` is free.
        let matched: Vec<(String, String)> = self
            .registrations
            .iter()
            .filter(|(class, _, _)| *class == from_class)
            .map(|(_, method, descriptor)| (method.to_string(), descriptor.to_string()))
            .collect();
        let entries: Vec<(String, String, NativeCallback, NativeKind)> = matched
            .into_iter()
            .filter_map(|(method, descriptor)| {
                let slot = self.slot_for_exact(from_class, &method, &descriptor)?;
                Some((method, descriptor, slot.callback, slot.kind))
            })
            .collect();
        for (method_name, descriptor, callback, kind) in entries {
            self.register(to_class, &method_name, &descriptor, callback, kind);
        }
    }

    /// The number of registered native methods (distinct triples — a
    /// re-registration of the same triple does not increase this).
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Returns true if no native methods are registered.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Fallback search: find any registered native callback whose
    /// `(method_name, descriptor)` matches, regardless of class.
    ///
    /// Used by the slow-path dispatcher recovery for the case where a
    /// synthetic native-allocated object (e.g. our `Pattern` instances
    /// allocated with `ClassId::new(0)`) ends up routed through
    /// `java/lang/Object` because the heap reports `class_id_of` as 0
    /// (the literal Object class id). A scoped native like
    /// `java/util/regex/Pattern.matcher(Ljava/lang/CharSequence;)Ljava/util/regex/Matcher;`
    /// is uniquely identified by `(method_name, descriptor)` because
    /// `Object` has no such method — so this scan recovers the
    /// correct callback without needing the CP method-ref class.
    ///
    /// Returns the callback of the FIRST class to register this
    /// `(method_name, descriptor)` pair. The lookup is O(1): it consults
    /// the `by_method_desc` index built incrementally at registration
    /// time, not an O(N) scan. The index is populated with
    /// `entry().or_insert()`, so if several classes register the same
    /// `(method, descriptor)` the earliest registration is the one kept
    /// and returned here — the "first match" wording above is therefore
    /// a real guarantee, not an artifact of iteration order.
    ///
    /// Call sites should still gate this on the slow recovery path (NSME
    /// about to be raised), not the hot dispatch.
    pub fn find_by_method_descriptor(
        &self,
        method_name: &str,
        descriptor: &str,
    ) -> Option<NativeCallback> {
        // AUDIT 2026-05-17 (Fix 5): O(1) lookup via the class-agnostic
        // `by_method_desc` index built at registration time. The index
        // is keyed by `native_method_hash("", method, descriptor)` so the
        // class portion is masked out, and first-registration wins on
        // key collision (see `register`).
        let key = native_method_hash("", method_name, descriptor);
        self.by_method_desc.get(&key).copied()
    }

    /// Cheap conservative prefilter for hot call paths that only need to know
    /// whether a class-qualified native lookup could possibly succeed.
    ///
    /// A false result is definitive for clean descriptors: no registered native
    /// has this `(method_name, descriptor)` pair on any class, so callers may
    /// skip class-specific `find()` probes and superclass walks. Descriptors
    /// that would trigger the compatibility rewrite path return true even when
    /// the exact index misses, preserving `find()` semantics.
    #[inline]
    pub fn might_have_method_descriptor(&self, method_name: &str, descriptor: &str) -> bool {
        let key = native_method_hash("", method_name, descriptor);
        if self.by_method_desc.contains_key(&key) {
            return true;
        }
        let has_whitespace_or_nul = descriptor
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b == b'\0');
        let object_return_quirk = match descriptor.rfind(')') {
            Some(rparen) => {
                let ret = &descriptor[rparen + 1..];
                ret.starts_with('L') && !ret.ends_with(';')
            }
            None => false,
        };
        has_whitespace_or_nul || object_return_quirk
    }
}
impl Default for NativeMethodRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for NativeMethodRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeMethodRegistry")
            .field("count", &self.slots.len())
            .finish()
    }
}


/// The rows `CRATONVM_INTRINSIC_BODY_COMPILES_ROWS` stands for when it is
/// UNSET (or set to a truthy word: `1`, `true`, `on`, `yes`, which is what
/// `CRATONVM_JIT=intrinsic-body-compiles-rows` writes).
///
/// Interpreter round i3 wave 81, lane L4: `finder-mint`, the one row that won
/// under the JIT (host, wave 80: `L4W77FinderMintBench` `find-getter` 15.5 ->
/// 3.1 us, every probe SAME); `method-type-intern` and `ste-to-string` lost
/// and stay off. THE FLIP IS THIS ONE LINE: `""` restores the wave-80 default
/// (no row). `=0`, `=none` or `=` (empty) turns every row off per run; the
/// INVENTORY row's `off_word` is `Some("0")`.
///
/// Interpreter round i4 wave 84 (orchestrator triage): `method-finder-mint`
/// joins the default (Windows, Temurin 25.0.3, `L4W81MethodFinderBench
/// --real-jdk`, 3 interleaved reps, medians: find-virtual 16.8 -> 3.9 us,
/// find-static 41.8 -> 3.0, find-special 15.9 -> 3.3, find-constructor 14.4
/// -> 5.1, find-virtual-ref 87.0 -> 3.2, unreflect-method 24.0 -> 3.5;
/// checksums equal). Removing it from this line restores the wave-83 default.
pub const INTRINSIC_BODY_COMPILES_ROWS_DEFAULT: &str = "finder-mint,method-finder-mint";

/// `CRATONVM_INTRINSIC_BODY_COMPILES_ROWS` as read: `None` (unset) is
/// [`INTRINSIC_BODY_COMPILES_ROWS_DEFAULT`]; a set value is parsed by
/// [`parse_intrinsic_body_compiles_rows`], so an empty value lists no row.
pub(in crate::registry) fn read_intrinsic_body_compiles_rows(value: Option<&str>) -> Vec<String> {
    parse_intrinsic_body_compiles_rows(value.unwrap_or(INTRINSIC_BODY_COMPILES_ROWS_DEFAULT))
}

/// The row tokens of `CRATONVM_INTRINSIC_BODY_COMPILES_ROWS` (interpreter
/// round i3 wave 80, lane L4): comma-separated, trimmed, ASCII-lowercased;
/// empty items and the off words (`0`, `false`, `off`, `no`, `none`) dropped.
/// Wave 81: a truthy word (`1`, `true`, `on`, `yes`) stands for the default
/// rows ([`INTRINSIC_BODY_COMPILES_ROWS_DEFAULT`]), so the group token's
/// "on" (`=1`) means "the default set", never a row named `1`.
pub(in crate::registry) fn parse_intrinsic_body_compiles_rows(value: &str) -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();
    for item in value.split(',') {
        let row = item.trim().to_ascii_lowercase();
        if row.is_empty() || matches!(row.as_str(), "0" | "false" | "off" | "no" | "none") {
            continue;
        }
        if matches!(row.as_str(), "1" | "true" | "on" | "yes") {
            for default_row in INTRINSIC_BODY_COMPILES_ROWS_DEFAULT.split(',') {
                let default_row = default_row.trim().to_ascii_lowercase();
                if !default_row.is_empty() && !rows.contains(&default_row) {
                    rows.push(default_row);
                }
            }
            continue;
        }
        if !rows.contains(&row) {
            rows.push(row);
        }
    }
    rows
}

