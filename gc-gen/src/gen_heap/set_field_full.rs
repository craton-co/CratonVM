#[allow(unused_imports)]
use super::*;

impl GenerationalHeap {
    /// [`Self::set_field`] after its compact fast arm: the array refusal, the
    /// suspect-header and out-of-bounds guards, the unresolvable-compact guard
    /// and the legacy 16-byte cell store, unchanged since before gcf f2/p.
    pub fn set_field_full(
        &self,
        obj_ref: ObjectRef,
        header: &ObjectHeader,
        index: usize,
        value: Value,
    ) {
        // KC16 SIGSEGV audit: runtime bounds check.  Silently drop writes
        // that fall past the object's declared layout (layout mismatch
        // between synthetic and real JDK class shapes) rather than
        // overflowing into the neighboring object.  Still panic on
        // clearly-corrupt headers.
        // Symmetric with `get_field`: a 16-byte `Value` cell stamped over
        // packed element data corrupts every element sharing those bytes, and
        // past the first few indices lands outside the allocation. The bounds
        // test below admits it because an array mirrors its LENGTH into
        // `num_slots`. See `heap::refuse_array_receiver_field_access`.
        if header.kind() == ObjectKind::Array {
            // SAFETY: `obj_ref` is a live allocation base and `header` is its
            // header, resolved one line above.
            unsafe {
                cratonvm_gc_core::heap::refuse_array_receiver_field_access(
                    obj_ref.as_ptr() as usize,
                    header,
                    index,
                    "gen_heap::set_field",
                )
            };
            return;
        }
        let num_slots = header.num_slots() as usize;
        if num_slots > (1 << 24) {
            tracing::debug!(
                target: "cratonvm::gc::guard",
                obj = ?obj_ref.as_ptr(),
                index,
                num_slots,
                class_id = ?header.class_id,
                kind_byte = ObjectHeader::kind_tag(header.mark_word.load(Ordering::Relaxed)),
                gc_flags = format!("{:x}", header.gc_flags()),
                value = ?value,
                "gen_heap::set_field: suspect header (returning silently)",
            );
            // Rate-limited warn — do NOT panic. See matching comment in
            // `get_field` above.  Returning without writing keeps the
            // JVM alive so the Java side can recover or surface a real
            // exception.
            return;
        }
        if index >= num_slots {
            // DBG (CRATONVM_DBG_SWEEP_ZERO): if this OOB receiver was recently
            // ZEROED by the non-moving sweep, name its ORIGINAL class and the GC
            // that reclaimed it — turns "set_field on java/lang/Object[0]" into
            // "set_field on a SWEPT <UserClass>", naming the root-coverage gap.
            if let Some((cid, kind, cycle, reason, initiator, blocked)) =
                sweep_zero_lookup(obj_ref.as_ptr() as usize)
            {
                let orig_name = self
                    .diag_class_info(cid)
                    .map(|(n, _)| n)
                    .unwrap_or_else(|| "<unresolved>".to_string());
                eprintln!(
                    "[SWEEP-ZERO-HIT] set_field receiver {:p} was a SWEPT {} \
                     (orig class_id={} kind={}) reclaimed at cycle={} reason={} \
                     initiator_tid={} blocked={}",
                    obj_ref.as_ptr(),
                    orig_name,
                    cid,
                    kind,
                    cycle,
                    reason,
                    initiator,
                    blocked,
                );
            }
            // Out-of-bounds writes are dropped rather than corrupting the
            // neighboring object. The diagnostic distinguishes between two
            // distinct bug classes (matches the same split in `get_field`
            // above):
            //
            //   (A) `real_field_count > num_slots` — TRUE undersized layout.
            //       The class declares more instance fields than the
            //       allocation site asked for. This is the original smoking-
            //       gun case (PrintStream 1-field stub, commit cf1b478) and
            //       is still logged at `error!` level.
            //
            //   (B) `real_field_count <= num_slots` — CALLER-SIDE wrong slot.
            //       The class layout is correct; a writer used a slot index
            //       past the receiver's layout. Canonical instance: a
            //       JIT/interpreter path writes Class-mirror slots 96/97 on
            //       a 19-slot Class (`ignite.err`). Logged at `warn!` so the
            //       orchestrator's `grep -E "undersized"` still surfaces (A) —
            //       the real allocator bugs — while (B) is a separate triage
            //       workstream.
            //
            // Rate-limited exactly like `get_field`'s read-side twin
            // (`OOB_DIAG_CAP`, forced back on by `CRATONVM_DBG_OOBFIELD`).
            // gen r4/plumbing (2026-09-23): the read side was capped because
            // `resolve_class_info` allocates a `String` and the `warn!` formats
            // on EVERY out-of-bounds access, which dominated kafka's JUnit
            // discovery wall time; the WRITE side kept paying both per call,
            // for the case-(B) caller-side store the comment above calls a
            // separate triage workstream. The store is still dropped every
            // time — only the per-call diagnostics stop after the cap.
            static OOB_WRITE_DIAG_COUNT: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            const OOB_WRITE_DIAG_CAP: u64 = 512;
            let oob_dbg = gc_flags().dbg_oobfield;
            if oob_dbg
                || OOB_WRITE_DIAG_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    < OOB_WRITE_DIAG_CAP
            {
                let (class_name, real_fields) = match self.diag_class_info(header.class_id.as_u32())
                {
                    Some((name, n)) => (name, Some(n)),
                    None => ("<unresolved>".to_string(), None),
                };
                // CRATONVM_DBG_OOBFIELD=<substr>: dump a backtrace for OOB field
                // accesses whose class name contains <substr>, to localize the writer.
                if let Some(want) = gc_flags().dbg_oobfield_value.as_deref() {
                    if !want.is_empty() && class_name.contains(want) {
                        eprintln!(
                            "[OOBFIELD_ASRTAG_V1 WRITE] class={} index={} num_slots={} value={:?}\n{}",
                            class_name,
                            index,
                            num_slots,
                            value,
                            std::backtrace::Backtrace::force_capture()
                        );
                    }
                }
                let is_true_undersized = real_fields.is_some_and(|n| n > num_slots);
                if is_true_undersized {
                    tracing::error!(
                        target: "cratonvm::gc::guard",
                        obj = ?obj_ref.as_ptr(),
                        index,
                        num_slots,
                        class_id = ?header.class_id,
                        class_name = %class_name,
                        real_field_count = ?real_fields,
                        value = ?value,
                        "gen_heap::set_field: out-of-bounds field write dropped \
                         (undersized object layout — class declares more fields \
                         than the object was allocated with)",
                    );
                } else {
                    tracing::warn!(
                        target: "cratonvm::gc::guard",
                        obj = ?obj_ref.as_ptr(),
                        index,
                        num_slots,
                        class_id = ?header.class_id,
                        class_name = %class_name,
                        real_field_count = ?real_fields,
                        value = ?value,
                        "gen_heap::set_field: out-of-bounds field write dropped \
                         (caller used slot index past receiver's layout — \
                         class layout is correct; the bug is in the caller's \
                         slot computation)",
                    );
                }
            } // end rate-limited OOB-write diagnostics
              // RESID-DIAG (dohead residuals investigation, 20260718): see the
              // matching comment in get_field's OOB guard above. Outside the cap,
              // like the read side's, because it carries its own rate limit and
              // its power-of-two heartbeat must keep counting past the cap.
            if num_slots == 0 {
                // RATE-LIMIT (2026-07-21): see the matching READ guard above.
                static RESID_WRITE_DIAG_COUNT: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                let n = RESID_WRITE_DIAG_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if n < 5 || n.is_power_of_two() {
                    let class_name = self
                        .diag_class_info(header.class_id.as_u32())
                        .map(|(name, _)| name)
                        .unwrap_or_else(|| "<unresolved>".to_string());
                    if n < 5 {
                        eprintln!(
                            "[RESID-DIAG WRITE] class={class_name} index={index} num_slots={num_slots} obj={:p} value={value:?}\n{}",
                            obj_ref.as_ptr(),
                            std::backtrace::Backtrace::force_capture()
                        );
                    } else {
                        eprintln!(
                            "[RESID-DIAG WRITE] class={class_name} index={index} num_slots={num_slots} obj={:p} value={value:?} (occurrence #{n}, backtrace suppressed)",
                            obj_ref.as_ptr(),
                        );
                    }
                }
            }
            return;
        }
        debug_assert!(index < self.get_header(obj_ref).num_slots() as usize);
        // Compact layout: store the descriptor-derived tagless payload. The
        // write barrier fires for reference arms exactly as on the legacy path.
        // gcf f2/p: `set_field` resolves a compact receiver before calling
        // here, so this arm only answers `None` in practice (no registered
        // layout -- the guard below); it is kept so the full path stays
        // complete on its own.
        if let Some((off, storage)) = compact_field_slot(header, index) {
            self.set_compact_field_resolved(obj_ref, header, index, off, storage, value);
            return;
        }
        self.note_field_store_watches(header, obj_ref, index, value);
        // HIB-DCAST-LATEPHASE.1 (mutator side, write half). See the long note
        // on the matching guard in `get_field` above for why
        // `compact_field_slot`'s `None` is two different states and why only
        // the legacy one may fall through. This half is the more damaging of
        // the two: `write_slot` at `HEADER_SIZE + index * SLOT_SIZE` on a
        // compact-sized body does not merely read past the allocation, it
        // *writes* a 16-byte `Value` cell over whatever follows the object —
        // a neighbouring object's header, or unmapped memory. As in
        // `get_field`, the walkers' answer to this state is "skip, do not
        // guess"; the accessor's equivalent is this function's own
        // established one for an unserviceable write (suspect header,
        // out-of-bounds index): drop the store, record it loudly, do not
        // panic.
        if is_compact_object(header) {
            tracing::warn!(
                target: "cratonvm::gc::guard",
                obj = ?obj_ref.as_ptr(),
                index,
                num_slots,
                class_id = ?header.class_id,
                value = ?value,
                "gen_heap::set_field: compact receiver has no registered layout \
                 for its (class_id, field_count) — dropping the write rather \
                 than striding its packed compact body as legacy 16-byte cells \
                 (HIB-DCAST-LATEPHASE.1)",
            );
            return;
        }
        // SAFETY: Same as `get_field` — `index` is within `num_slots` so the
        // computed slot pointer is within the object's allocated region.
        // `write_slot` writes a `Value` at the computed address.
        unsafe {
            let ptr = slot_ptr(obj_ref, index);
            write_slot(ptr, value);
        }
        self.write_barrier(obj_ref, value);
    }

    /// Get the value of a volatile field.
    ///
    /// JLS §17.7 requires reads and writes of volatile long / double
    /// (and any volatile-declared field) to be atomic. The on-heap
    /// `Value` slot is 16 bytes (8-byte tag + 8-byte payload), wider
    /// than any stable Rust atomic primitive on x86-64, so a SeqCst
    /// fence pair gives the required happens-before ordering but does
    /// **not** by itself guarantee atomicity of the slot's read —
    /// a concurrent writer could leave the tag and payload words
    /// momentarily out of sync, surfacing as a torn long/double
    /// (e.g. high 32 bits from the old write, low 32 from the new).
    ///
    /// We close the atomicity hole with the striped-mutex pool in
    /// [`cratonvm_gc_core::collector::volatile_stripe_lock`]: paired
    /// `set_field_volatile` writers acquire the same stripe, so the
    /// 16-byte read here either observes a fully old or fully new
    /// `Value`. Striping keeps unrelated volatile fields from
    /// serializing across the heap (the previous design used a single
    /// heap-wide mutex which became a global bottleneck).
    pub fn get_field_volatile(&self, obj_ref: ObjectRef, index: usize) -> Value {
        let _guard = cratonvm_gc_core::collector::volatile_stripe_lock(obj_ref, index);
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        let val = self.get_field(obj_ref, index);
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        val
    }

    /// Set the value of a volatile field.
    ///
    /// Acquires the per-slot stripe lock from
    /// [`cratonvm_gc_core::collector::volatile_stripe_lock`] to make the 16-byte
    /// `Value` write appear atomic to a concurrent volatile reader.
    /// SeqCst fences provide the JMM happens-before edge. The write
    /// barrier still fires inside `set_field`. See
    /// [`Self::get_field_volatile`] for the full rationale.
    pub fn set_field_volatile(&self, obj_ref: ObjectRef, index: usize, value: Value) {
        let _guard = cratonvm_gc_core::collector::volatile_stripe_lock(obj_ref, index);
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        self.set_field(obj_ref, index, value); // barrier fires inside set_field
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    }

    // ----- T10.9.E descriptor-aware field access --------------------------

    /// Descriptor-aware get — normalizes the returned `Value` to the declared
    /// field type. See [`cratonvm_gc_core::heap::coerce_field_value_for_slot`].
    ///
    /// # G45: provenance
    ///
    /// These four inherent methods are the direct-`&GenerationalHeap`
    /// spelling of the four `GarbageCollector` defaults in `collector.rs`,
    /// which `GenerationalHeap`'s trait impl (`:17200`) does NOT override —
    /// so a VM run reaches the trait bodies and these are for callers
    /// holding the concrete heap. They coerced identically and reported
    /// identically badly (`class_id=-1 index=-1`), so they are repaired
    /// identically; letting the two spellings disagree about what the
    /// instrument says would be worse than either answer.
    ///
    /// Hot path unchanged: `site` is a three-word `Copy` value that
    /// `coerce_field_value_for_slot` passes only to the `#[cold]`
    /// reporter, and [`Self::class_id_of`] is one load from the object
    /// header that the neighbouring `get_field`/`set_field` dereferences
    /// in the same call.
    pub fn get_field_as(&self, obj_ref: ObjectRef, index: usize, desc_byte: u8) -> Value {
        let raw = self.get_field(obj_ref, index);
        cratonvm_gc_core::heap::coerce_field_value_for_slot(
            raw,
            desc_byte,
            cratonvm_gc_core::heap::FieldCoercionSite::read(Some(self.class_id_of(obj_ref)), index),
        )
    }

    /// Volatile descriptor-aware get. Provenance as in [`Self::get_field_as`].
    pub fn get_field_volatile_as(&self, obj_ref: ObjectRef, index: usize, desc_byte: u8) -> Value {
        let raw = self.get_field_volatile(obj_ref, index);
        cratonvm_gc_core::heap::coerce_field_value_for_slot(
            raw,
            desc_byte,
            cratonvm_gc_core::heap::FieldCoercionSite::read(Some(self.class_id_of(obj_ref)), index),
        )
    }

    /// Descriptor-aware set — normalizes the written `Value` to the declared
    /// field type before the underlying slot write.
    ///
    /// Reports `access="store"`: a coercing store destroyed something a
    /// writer meant, where a coercing read is usually a never-initialised
    /// slot repairing its own tag.
    pub fn set_field_as(&self, obj_ref: ObjectRef, index: usize, value: Value, desc_byte: u8) {
        let coerced = cratonvm_gc_core::heap::coerce_field_value_for_slot(
            value,
            desc_byte,
            cratonvm_gc_core::heap::FieldCoercionSite::store(Some(self.class_id_of(obj_ref)), index),
        );
        self.set_field(obj_ref, index, coerced);
    }

    /// Volatile descriptor-aware set. Provenance as in [`Self::set_field_as`].
    pub fn set_field_volatile_as(
        &self,
        obj_ref: ObjectRef,
        index: usize,
        value: Value,
        desc_byte: u8,
    ) {
        let coerced = cratonvm_gc_core::heap::coerce_field_value_for_slot(
            value,
            desc_byte,
            cratonvm_gc_core::heap::FieldCoercionSite::store(Some(self.class_id_of(obj_ref)), index),
        );
        self.set_field_volatile(obj_ref, index, coerced);
    }

    // ----- Array access ------------------------------------------------------

    /// Get the length of an array.
    pub fn array_length(&self, obj_ref: ObjectRef) -> usize {
        let header = self.get_header(obj_ref);
        if header.kind() != ObjectKind::Array {
            // Defensive hardening: native/bootstrap code can occasionally pass a
            // plain object into array helpers. Returning 0 keeps startup alive,
            // while diagnostics below pinpoint the exact caller and object shape.
            //
            // Rate-limit the diagnostic: long-running boots (e.g. Keycloak)
            // can hit this path tens of thousands of times. Emitting a full
            // `Backtrace::force_capture` each time produces hundreds of MB
            // of stderr and exhausts disk. Keep the first few one-line
            // warnings (with backtrace gated by `CRATONVM_GC_ARRAY_GUARD_BT`)
            // and silently return 0 thereafter.
            static GUARD_COUNT: AtomicUsize = AtomicUsize::new(0);
            const GUARD_LIMIT: usize = 5;
            let n = GUARD_COUNT.fetch_add(1, Ordering::Relaxed);
            if n < GUARD_LIMIT {
                // Format the entire message into one String and emit via a
                // single locked stderr write. Multi-segment `eprintln!`
                // formatting can interleave with other threads' stderr
                // writes on Windows and trigger the rare
                // "Имеющийся буфер не подходит" (os error 1784) panic on the
                // intermediate write of large {:?} pretty-printed values.
                //
                // CRITICAL: we are formatting fields of a header that we
                // already know is suspect (kind != Array). The raw bytes
                // backing `kind` and `element_type` may be garbage — using
                // `{:?}` on `#[repr(u8)]` enums whose byte value is outside
                // the declared variants is undefined behavior and has been
                // observed to provoke a `capacity overflow` panic deep in
                // `core::fmt` (the Display jump table writing an arbitrary
                // huge slice into the format buffer).
                //
                // Read the raw bytes directly via the object pointer rather
                // than via `&ObjectHeader` field access, so we never go
                // through the unsound enum-value path. The header layout is
                // `#[repr(C)]`: class_id(u32) @0, kind(u8) @4, elem(u8) @5.
                let obj_ptr = obj_ref.as_ptr();
                // SAFETY: `obj_ref` was previously dereferenced via
                // `get_header` above without faulting, so `obj_ptr` plus
                // `HEADER_SIZE` bytes is readable. We read individual bytes
                // and a 4-byte u32 within that range, which is well-defined
                // even when the byte values do not correspond to valid enum
                // discriminants.
                // SAFETY: `obj_ptr` was just dereferenced via `get_header` without
                // faulting, so the header's bytes (offsets 0..16) are readable; raw
                // byte/u32 reads are well-defined for any bit pattern.
                // Offsets come from the named header constants, not literals:
                // this diagnostic prints what a heap walker would have decoded,
                // so it must follow the header layout if it ever shifts (see
                // header-shrink.md).
                let (kind_byte, elem_byte, class_id_raw, stored_len) = unsafe {
                    let class_id_raw = (obj_ptr as *const u32).read_unaligned();
                    let kind_byte = cratonvm_types::kind_tag_at(obj_ptr);
                    let elem_byte = cratonvm_types::element_type_tag_at(obj_ptr);
                    let stored_len = (obj_ptr.add(cratonvm_types::ARRAY_LENGTH_OFFSET)
                        as *const u32)
                        .read_unaligned();
                    (kind_byte, elem_byte, class_id_raw, stored_len)
                };
                let msg = if gc_flags().gc_array_guard_bt {
                    let bt = Backtrace::force_capture();
                    format!(
                        "[GC-ARRAY-GUARD] array_length(non-array): kind_byte={} class_id={} elem_byte={} stored_len={} obj={:p} (#{}/{})\nbacktrace:\n{}\n",
                        kind_byte,
                        class_id_raw,
                        elem_byte,
                        stored_len,
                        obj_ptr,
                        n + 1,
                        GUARD_LIMIT,
                        bt,
                    )
                } else {
                    format!(
                        "[GC-ARRAY-GUARD] array_length(non-array): kind_byte={} class_id={} elem_byte={} stored_len={} obj={:p} (#{}/{}; set CRATONVM_GC_ARRAY_GUARD_BT=1 for backtrace)\n",
                        kind_byte,
                        class_id_raw,
                        elem_byte,
                        stored_len,
                        obj_ptr,
                        n + 1,
                        GUARD_LIMIT,
                    )
                };
                // Atomic write via locked stderr; ignore errors (best-effort
                // diagnostic). Avoids the eprintln panic on Windows when the
                // writer's internal buffer is unhappy.
                use std::io::Write;
                let stderr = std::io::stderr();
                let mut handle = stderr.lock();
                let _ = handle.write_all(msg.as_bytes());
                if n + 1 == GUARD_LIMIT {
                    let _ = handle.write_all(
                        b"[GC-ARRAY-GUARD] suppressing further array_length(non-array) warnings\n",
                    );
                }
            }
            return 0;
        }
        // Guard against a corrupt synthetic header whose length field is
        // garbage. The only sound ceiling is the JVM's own limit: a Java
        // array cannot exceed `Integer.MAX_VALUE` elements, so any larger
        // value is provably a corrupt header. An earlier `1 << 24` cap also
        // rejected *legitimate* arrays larger than 16M elements (e.g. a
        // 2^26-int vector), clobbering their length to 0 and raising a
        // spurious ArrayIndexOutOfBoundsException on every access.
        const MAX_REASONABLE_ARRAY_LEN: u32 = i32::MAX as u32; // JVM array ceiling
        let raw_len = header.array_length();
        if raw_len > MAX_REASONABLE_ARRAY_LEN {
            static SUSPECT_LEN_WARN_COUNT: AtomicU64 = AtomicU64::new(0);
            let n = SUSPECT_LEN_WARN_COUNT.fetch_add(1, Ordering::Relaxed);
            if n < 16 {
                // Format with raw byte values to avoid UB on Debug-formatting
                // a potentially-garbage `ArrayElementType` byte (same hazard
                // as the GC-ARRAY-GUARD path above).
                eprintln!(
                    "[GC-ARRAY-LEN-GUARD] suspect array_length={} (>1<<24) class_id={} elem_byte={} obj={:p} (n={})",
                    raw_len,
                    header.class_id.as_u32(),
                    header.element_type() as u8,
                    obj_ref.as_ptr(),
                    n,
                );
            }
            return 0;
        }
        raw_len as usize
    }

    /// Bulk-read a char[] array into a `Vec<u16>`.
    ///
    /// Much faster than per-element `get_array_element` for reading Java
    /// String backing arrays — copies the raw 2-byte-per-element data directly.
    pub fn read_char_array_bulk(&self, obj_ref: ObjectRef) -> Vec<u16> {
        let header = self.get_header(obj_ref);
        debug_assert_eq!(header.kind(), ObjectKind::Array);
        debug_assert_eq!(header.element_type(), ArrayElementType::Char);
        let len = header.array_length() as usize;
        let mut out = vec![0u16; len];
        // SAFETY: `obj_ref` is a valid Char array with `len` elements (verified by
        // header assertions above). Each char element is 2 bytes, so `HEADER_SIZE`
        // to `HEADER_SIZE + len * 2` is within the allocation. The destination
        // buffer `out` is freshly allocated with the same length. The regions do
        // not overlap because `out` is on the Rust heap, not in the GC arena.
        unsafe {
            let src = obj_ref.as_ptr().add(ARRAY_DATA_OFFSET);
            std::ptr::copy_nonoverlapping(src, out.as_mut_ptr() as *mut u8, len * 2);
        }
        out
    }

    /// Get a raw pointer to the start of the array data region.
    ///
    /// This is the address immediately after the object header. The caller is
    /// responsible for knowing the element type and bounds.
    pub fn array_data_ptr(&self, obj_ref: ObjectRef) -> *mut u8 {
        // SAFETY: `obj_ref` is a valid heap object whose allocation includes
        // `HEADER_SIZE` plus the data region, so advancing by `HEADER_SIZE`
        // yields a pointer within the allocation. Caller is responsible for
        // bounds and element-type correctness.
        unsafe { obj_ref.as_ptr().add(ARRAY_DATA_OFFSET) }
    }

    /// Get an array element at the given index.
    ///
    /// Returns `Err` with the index if out of bounds.
    pub fn get_array_element(&self, obj_ref: ObjectRef, index: usize) -> Result<Value, i32> {
        if gc_flags().dbg_deadref_store {
            self.note_deadref_recv(
                "get_array_element",
                obj_ref.as_ptr() as usize,
                index,
                Value::Object(None),
            );
        }
        let header = self.get_header(obj_ref);
        debug_assert_eq!(header.kind(), ObjectKind::Array);
        if index >= header.array_length() as usize {
            return Err(cratonvm_gc_core::heap::oob_index_code(index));
        }
        // SAFETY: Bounds check above guarantees `index < array_length`. The array
        // was allocated with sufficient data space for all elements after the header.
        // `read_prim_element` reads the correctly typed element at the given index.
        unsafe {
            let base = obj_ref.as_ptr().add(ARRAY_DATA_OFFSET);
            Ok(read_prim_element(base, index, header.element_type()))
        }
    }

    /// Like `get_array_element`, but auto-unboxes values stored by native
    /// collections. See `Heap::get_array_element_unboxing` for details.
    pub fn get_array_element_unboxing(
        &self,
        obj_ref: ObjectRef,
        index: usize,
    ) -> Result<Value, i32> {
        let header = self.get_header(obj_ref);
        debug_assert_eq!(header.kind(), ObjectKind::Array);
        if index >= header.array_length() as usize {
            return Err(cratonvm_gc_core::heap::oob_index_code(index));
        }
        // SAFETY: Bounds check above guarantees `index < array_length`. The array
        // data region is within the allocation.
        let value = unsafe {
            let base = obj_ref.as_ptr().add(ARRAY_DATA_OFFSET);
            read_prim_element(base, index, header.element_type())
        };
        if header.element_type() == ArrayElementType::Reference {
            if let Value::Object(Some(obj)) = value {
                // The stored word is treated as an `ObjectRef`, but a stale or
                // garbage non-zero element could point anywhere. Validate it
                // against the heap arenas (region + alignment + header sanity)
                // before dereferencing it as an `ObjectHeader`. If it does not
                // look like a live heap object, skip the unboxing and return
                // the value as-is rather than performing a wild read.
                if self.is_object_address(obj.as_ptr() as usize).is_some() {
                    // SAFETY: `is_object_address` confirmed `obj` points to a
                    // valid object header inside one of this heap's arenas.
                    let obj_header = unsafe { &*(obj.as_ptr() as *const ObjectHeader) };
                    if obj_header.class_id == AUTOBOX_CLASS_ID {
                        return Ok(self.get_field(obj, 0));
                    }
                }
            }
        }
        Ok(value)
    }

    /// Publish the semispace geometry for [`dead_young_ref_reason_global`].
    ///
    /// Called once per collection from `update_all_roots`, which is the one
    /// place that runs on every collection path and already holds the heap.
    pub fn publish_young_geometry(&self) {
        let (ilo, ihi) = self.young_inactive_semispace_range();
        let (alo, ahi) = {
            let from = self.lock_young_from();
            let base = from.base_ptr() as usize;
            (base, base + from.capacity())
        };
        YOUNG_GEO[0].store(alo, Ordering::Relaxed);
        YOUNG_GEO[1].store(ahi, Ordering::Relaxed);
        YOUNG_GEO[2].store(ilo, Ordering::Relaxed);
        YOUNG_GEO[3].store(ihi, Ordering::Relaxed);
    }

    /// Is `addr` a young reference that names no live object, and why?
    ///
    /// The two arms are independent, not a refinement of one another — see the
    /// note in [`Self::note_deadref_store`]. Together they are exact for the
    /// young generation: every young address that is not a live object base is
    /// either in the emptied semispace or reads back as zeroed bytes.
    ///
    /// `None` for an old-gen or off-heap address; this answers about young only.
    pub fn dead_young_ref_reason(&self, addr: usize) -> Option<&'static str> {
        if addr == 0 || addr % 8 != 0 {
            return None;
        }
        let (lo, hi) = self.young_inactive_semispace_range();
        if addr >= lo && addr < hi {
            return Some("INACTIVE-SEMISPACE");
        }
        // Only the ACTIVE semispace is left to ask about: the inactive one
        // answered above. gen r4w5/review5 (2026-09-24): and "inside it" is not
        // "mapped" -- the active semispace was last cycle's evacuated one, which
        // `uncommit_evacuated_young` (default ON) handed back to the OS and which
        // is re-committed only as far as the evacuation and the mutators' bumps
        // have reached. The header read used to trust containment alone
        // (`is_in_young_either`), so a stale reference into an uncommitted
        // granule faulted in the diagnostic that exists to name it. Screen both
        // ends of the header span through the arena's own commit map, under its
        // lock; an uncommitted span, or one that runs off the end, holds no
        // object.
        let readable = {
            let from = self.lock_young_from();
            let base = from.base_ptr() as usize;
            if !from.contains(addr as *const u8) {
                None
            } else {
                let off = addr - base;
                if off + HEADER_SIZE > from.capacity() {
                    return Some("HEADER-PAST-ACTIVE-SEMISPACE-END");
                }
                Some(from.is_readable_at(off) && from.is_readable_at(off + (HEADER_SIZE - 1)))
            }
        };
        match readable {
            None => None,
            Some(false) => Some("UNCOMMITTED-YOUNG"),
            // SAFETY: 8-aligned (checked at entry), inside the active young
            // semispace, and both the first and the last byte of the header
            // span are in committed granules (a 16-byte span crosses at most one
            // granule boundary), checked just above. No collection can decommit
            // young memory while a mutator is running this.
            Some(true) => unsafe { header_is_zero(addr) }.then_some("ZERO-HEADER-IN-YOUNG"),
        }
    }

    /// `CRATONVM_DBG_DEADREF_STORE`: report a reference store whose VALUE names
    /// the inactive young semispace.
    ///
    /// That semispace is the one the last moving cycle evacuated and emptied;
    /// no live object is ever in it (`young_inactive_semispace_range`). So a
    /// mutator storing an address from it is storing a reference to memory that
    /// is already dead — the producer half of the defect the `[heap-stale]`
    /// verifier can only find later, once a collection has zeroed the bytes or
    /// re-served them under another class.
    ///
    /// The two halves are not interchangeable. `[heap-stale]` names the
    /// REFERRER and the slot, which is where the bad value ended up;
    /// this names the CALLER, which is what has to be fixed. On BindableTests
    /// under GC stress the referrer half pointed at `java/lang/Module field[0]`
    /// for thousands of cycles and never once said who wrote it.
    ///
    /// Takes the to-space arena lock, so it is strictly a diagnostic path:
    /// the `gc_flags()` gate is a startup-static bool and the common case is
    /// one predicted-not-taken branch.
    ///
    /// (gen r4/plumbing, 2026-09-23: this paragraph block and a `#[cold]` used
    /// to sit on [`Self::note_deadref_recv`] — so the receiver arm was
    /// documented as the value arm, and this one had no documentation and no
    /// `#[cold]`. Both arms are now cold.)
    #[cold]
    pub fn note_deadref_store(&self, site: &str, holder: usize, slot: usize, value: usize) {
        if value == 0 {
            return;
        }
        let (lo, hi) = self.young_inactive_semispace_range();
        let Some(reason) = self.dead_young_ref_reason(value) else {
            return;
        };
        // Two ways a stored reference can already be dead, and the second one
        // is the one that took the longest to see.
        //
        //  * INACTIVE-SEMISPACE — the address is in the semispace the last
        //    moving cycle emptied. Unambiguous, and it is what caught
        //    `build_module`'s unpinned `layer`.
        //
        //  * ZERO-HEADER-IN-YOUNG — the address is in the ACTIVE semispace, so
        //    the arm above says nothing, but there is no object AT it: the
        //    header words are zero. Live young memory is never in that state at
        //    a reference store, because an allocation writes the header before
        //    anything can name the object. Zeroed young bytes that are not an
        //    object are dead by construction: the unwritten tail past the
        //    allocation frontier, a reclaimed free block, and — the case here —
        //    the interior of a retired TLAB's tail filler, whose data area
        //    `install_tail_filler` zeroes.
        //
        // The second arm is not a refinement of the first. A generational young
        // collection alternates its semispaces, so which arm a given dead
        // address trips depends only on the parity of the cycle it died in.
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        if n >= 24 {
            return;
        }
        // gen r4/plumbing (2026-09-23): this format string had lost its `\`
        // line continuations, so every report carried two runs of fourteen
        // literal spaces and a raw newline before `collection=` — the same
        // defect round 1 fixed in the `[GC] g1 humongous-eager:` line.
        eprintln!(
            "[deadref-store] #{n} {reason} {site}: holder=0x{holder:x} slot={slot} <- 0x{value:x} \
             (inactive semispace is [0x{lo:x},0x{hi:x})) — the value named no live object at the \
             moment it was stored, so no later collection lost it: this write is the defect. \
             collection={}\n{:?}",
            self.stats.minor_gc_count.load(Ordering::Relaxed),
            std::backtrace::Backtrace::force_capture(),
        );
    }

    /// `[deadref-recv]` — a field STORE whose RECEIVER names no live object.
    ///
    /// The twin of [`Self::note_deadref_store`], on the other operand. That one
    /// asks "is the value I am storing dead"; this one asks "is the object I am
    /// storing into dead", and the answers are independent: a native that
    /// captured a receiver before an allocation and stored through it
    /// afterwards writes a LIVE value into a DEAD address, so every arm of the
    /// value-side family reads zero while the field it meant to set stays at
    /// its JVM default.
    ///
    /// That shape is not hypothetical and it is not rare: the ten `ObjectRef`s
    /// the BindableTests family fixed on 2026-09-09 were all value-side, and
    /// the residual that survived them
    /// (`native_assertj_lightweight_comparable_assert`'s `objects` store) was
    /// receiver-side and therefore invisible to all of them. This arm is what
    /// makes the screen total.
    ///
    /// Same predicate, same rate limit, same backtrace as the value arm.
    #[cold]
    pub fn note_deadref_recv(&self, site: &str, holder: usize, slot: usize, value: Value) {
        if holder == 0 {
            return;
        }
        let (lo, hi) = self.young_inactive_semispace_range();
        let Some(reason) = self.dead_young_ref_reason(holder) else {
            return;
        };
        // Counted before the rate limit, so the number is the population and
        // not the printout. The value arm has no counter and reads as "some or
        // none"; this one has to be readable as a total, because its headline
        // claim — that a receiver-side store is now screened — is exactly the
        // kind of claim a silent instrument can appear to support while being
        // unwired. `deadref_recv_hits` is what
        // `a_store_through_a_vacated_receiver_is_reported` asserts on.
        DEADREF_RECV_HITS.fetch_add(1, Ordering::Relaxed);
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        if n >= 24 {
            return;
        }
        eprintln!(
            "[deadref-recv] #{n} {reason} {site}: holder=0x{holder:x} slot={slot} <- {value:?} \
             (inactive semispace is [0x{lo:x},0x{hi:x})) — the RECEIVER named no live object at \
             the moment of the access, so it went to a copy nothing else will touch: a write \
             there is lost (the field keeps its JVM default) and a read answers whatever the \
             pre-move copy held. No later collection lost anything. \
             collection={}\n{:?}",
            self.stats.minor_gc_count.load(Ordering::Relaxed),
            std::backtrace::Backtrace::force_capture(),
        );
    }

    /// Set an array element at the given index.
    ///
    /// Returns `Err` with the index if out of bounds.
    ///
    /// (gen r4/plumbing, 2026-09-23: these two lines used to be stranded on
    /// `publish_young_geometry`, ~200 lines up.)
    pub fn set_array_element(
        &self,
        obj_ref: ObjectRef,
        index: usize,
        value: Value,
    ) -> Result<(), i32> {
        // DIAGNOSTIC (bc math-ec): catch a fake `0x4` reference stored into a
        // reference array element via the heap set API (bytecode aastore,
        // native System.arraycopy / clone / reflection).
        if let Value::Object(Some(p)) = value {
            let a = p.as_ptr() as usize;
            if a != 0 && a < 0x1_0000 && gc_flags().dbg_badref {
                eprintln!("[BADREF:set_array_element] idx={} ptr=0x{:x}", index, a);
            }
        }
        if gc_flags().dbg_deadref_store {
            // The RECEIVER arm, as in `set_field`. An array is the commonest
            // stale receiver of all, because the natural way to build one —
            // allocate the array, then allocate each element — holds the
            // array's address across every element's allocation. The value
            // stored is a brand-new object and always live, so the value arm
            // below is silent on exactly the defect that matters here.
            self.note_deadref_recv("set_array_element", obj_ref.as_ptr() as usize, index, value);
            if let Value::Object(Some(p)) = value {
                self.note_deadref_store(
                    "set_array_element",
                    obj_ref.as_ptr() as usize,
                    index,
                    p.as_ptr() as usize,
                );
            }
        }
        // CRATONVM_DBG_STALE_OBJREF (cce0079): a stale VALUE stored into a
        // reference array is otherwise silent (only the ARRAY's header is
        // read below) — the wrong object then surfaces at an arbitrarily
        // later read as a CCE. With the quarantine active, read the value's
        // header too so the store site itself trips the loud forwarded-
        // header panic (producer-side attribution).
        if cratonvm_gc_core::stale_objref_debug::enabled() {
            if let Value::Object(Some(v)) = value {
                // Operand attribution — see the matching peek in `set_field`.
                // SAFETY: `v` is a non-null `ObjectRef` (matched `Object(Some(v))`), so reading its header word is
                // sound; diagnostic-only (stale-objref debug) path.
                let peek = unsafe { &*(v.as_ptr() as *const ObjectHeader) };
                if peek.is_forwarded() {
                    eprintln!(
                        "[storechk] set_array_element: STALE stored VALUE 0x{:x} \
                         (array 0x{:x} index {index}) thread={} — canary panic follows",
                        v.as_ptr() as usize,
                        obj_ref.as_ptr() as usize,
                        std::thread::current().name().unwrap_or("?"),
                    );
                }
                let _ = self.get_header(v);
            }
        }
        let header = self.get_header(obj_ref);
        debug_assert_eq!(header.kind(), ObjectKind::Array);
        // gcstress residual face-1 diagnostics (CRATONVM_DBG_CELLCORRUPT) —
        // the debug_assert above is a no-op in release: an array-element
        // write through a STALE array reference whose address is now occupied
        // by a plain object would write a raw 8-byte pointer into the middle
        // of that object's 16-byte Value cells (the observed {ptr, 0} corrupt
        // cells). Trap it with the holder identity + backtrace. The bounds
        // check below usually deflects such writes (a plain object has
        // array_length=0), so this logs the attempt either way.
        if cell_corrupt_diag_enabled() && header.kind() != ObjectKind::Array {
            let class_name = self
                .diag_class_info(header.class_id.as_u32())
                .map(|(n, _)| n)
                .unwrap_or_else(|| "<unresolved>".to_string());
            eprintln!(
                "[CELLCORRUPT:set_array_element-on-NON-ARRAY] obj=0x{:x} class_id={} \
                 class={class_name} kind=0x{:02x} num_slots={} array_len={} index={index} \
                 value={value:?}\n{}",
                obj_ref.as_ptr() as usize,
                header.class_id.as_u32(),
                ObjectHeader::kind_tag(header.mark_word.load(Ordering::Relaxed)),
                header.num_slots(),
                header.array_length(),
                std::backtrace::Backtrace::force_capture(),
            );
        }
        if index >= header.array_length() as usize {
            return Err(cratonvm_gc_core::heap::oob_index_code(index));
        }
        // SAFETY: Bounds check above guarantees `index < array_length`. The array
        // data region is within the allocation. `write_prim_element` writes at the
        // correct element offset. For reference arrays, autobox wrappers are
        // allocated on this heap and thus valid.
        unsafe {
            let base = obj_ref.as_ptr().add(ARRAY_DATA_OFFSET);
            // Compact ref arrays store a bare pointer, not a tagged `Value`.
            // (gengc-round1-core 2026-09-20: it is `ref_element_size()` wide,
            // which is 4 under compressed oops and 8 otherwise — the old "only
            // 8-byte pointers" wording predates narrow oops and states a width
            // the layout does not guarantee. `write_prim_element` and
            // `array_data_size` both size it through `element_byte_size`, so
            // the CODE was and is right.) If a non-Object value is written
            // (e.g. Value::Int from a native collection), auto-box it into a
            // 1-field wrapper object.
            if header.element_type() == ArrayElementType::Reference {
                match value {
                    Value::Object(_) => {
                        write_prim_element(base, index, header.element_type(), value);
                        // Write barrier: old-gen array storing young-gen ref.
                        // gen r4w3/cards3: element-precise card.
                        self.write_barrier_ref_array_element(obj_ref, index, value);
                    }
                    _ => {
                        // Fallible: `alloc_object` aborts the process when the
                        // heap is full, and a heap-full auto-box is an
                        // `OutOfMemoryError` (`ARRAY_STORE_OUT_OF_MEMORY`).
                        //
                        // gengc-round1-core (2026-09-20): `_full`, not
                        // `try_alloc_object`. The bare fallible form is
                        // YOUNG-ONLY — it never spills into old gen and never
                        // arms `note_young_spill_pressure` — so a heap whose
                        // young semi is full while old gen has gigabytes free
                        // turned a perfectly legal `aastore` into
                        // `OutOfMemoryError`, AND did not even flag the
                        // boundary GC that would have fixed it. `_full` walks
                        // the identical young → old-gen spill path
                        // `alloc_object` does and returns `None` only when BOTH
                        // generations are exhausted, which is the condition
                        // this arm's error actually means.
                        let Some(wrapper) = self.try_alloc_object_full(AUTOBOX_CLASS_ID, 1) else {
                            return Err(cratonvm_gc_core::heap::ARRAY_STORE_OUT_OF_MEMORY);
                        };
                        self.set_field(wrapper, 0, value);
                        // Arm the process-wide wrapper latch: `Heap::
                        // get_array_element` does NOT unbox, so a wrapper can
                        // be laundered out of an array and stored into a
                        // reference field, and the field read side has to be
                        // able to find it (`cratonvm_gc_core::autobox`).
                        cratonvm_gc_core::autobox::note_wrapper_created();
                        let wrapper_val = Value::Object(Some(wrapper));
                        write_prim_element(base, index, header.element_type(), wrapper_val);
                        // Write barrier: track the wrapper reference for gen GC
                        // (gen r4w3/cards3: element-precise card).
                        self.write_barrier_ref_array_element(
                            obj_ref,
                            index,
                            Value::Object(Some(wrapper)),
                        );
                    }
                }
            } else {
                write_prim_element(base, index, header.element_type(), value);
            }
        }
        Ok(())
    }

    // ----- Write barrier -----------------------------------------------------

    /// Write barrier — called after every reference store into a heap object.
    ///
    /// **Card marking only.** If source is in old gen and target is in young
    /// gen, marks the card table entry dirty (for minor GC).
    ///
    /// gengc-round1-core (2026-09-20): this doc used to promise a second
    /// function — "**SATB logging:** if concurrent marking is active, logs the
    /// *old* reference value to the SATB queue" — that the body has never
    /// performed and structurally cannot. It is handed `stored_value`, the
    /// NEW value, and is called AFTER the store, so the old value is already
    /// gone. The real pre-barrier is the separate [`Self::satb_barrier`],
    /// which the interpreter (`opcodes.rs`) and the JIT helper
    /// (`vm/src/jit/helpers.rs`) call themselves before every reference store.
    ///
    /// **So a caller that reaches this heap API directly — a native, a
    /// reflective store, `ctx.set_field` — must call `satb_barrier(old_value)`
    /// itself before overwriting a reference while concurrent marking is
    /// active.** `set_field` / `set_array_element` do the card mark for you
    /// (that is what their own docs mean by "no need to call `write_barrier`
    /// separately"); they do NOT do the SATB pre-barrier.
    ///
    /// Round-5 #11 (perf) — the previous implementation dereferenced the
    /// object header twice on the hot path: once to inspect
    /// `header.gc_flags` on the *source*, then a second time on the
    /// *target*. Each dereference is a cache-line load on a separate
    /// object and both are pure waste — the same information is already
    /// encoded in the address ranges of the old-gen arena. The card
    /// table caches those bounds (it must, in order to compute card
    /// indices), so we route the gen-check through it and skip both
    /// header reads entirely. The cost on the common path drops from
    /// "two L1/L2 misses + two flag tests" to "two integer range
    /// comparisons" — the same pattern that round-10 used for
    /// `post_write_barrier_rset` (per-thread region cache).
    #[inline]
    pub fn write_barrier(&self, obj: ObjectRef, stored_value: Value) {
        self.write_barrier_marking(obj, obj.as_ptr() as usize, stored_value);
    }

    /// gen r4w3/cards3 — the card barrier for a reference store into ELEMENT
    /// `index` of the reference array `array`.
    ///
    /// When the card table carries element-precise marks
    /// (`CardTable::precise_ref_array_marks`, `CRATONVM_GC_PRECISE_ARRAY_CARDS`,
    /// default on) this dirties the card holding the ELEMENT's slot, not the
    /// array's header card. The dirty-card scan then reads only the elements
    /// of that card (`scan_dirty_cards_inner`'s window arm) instead of the
    /// whole array — one store into a million-element array used to cost a
    /// million slot reads on every young pause
    /// (`docs/feature-designs/generative-gc-architecture.md`).
    ///
    /// Otherwise it is exactly [`Self::write_barrier`]. The two kinds of mark
    /// coexist safely in one table: a header-card mark still means "scan the
    /// whole object", which is what the collector's own re-marks
    /// (`deferred_dirty_cards`) and every other barrier caller rely on.
    #[inline]
    pub fn write_barrier_ref_array_element(
        &self,
        array: ObjectRef,
        index: usize,
        stored_value: Value,
    ) {
        let holder = array.as_ptr() as usize;
        let mark_addr = if self.card_table.precise_ref_array_marks() {
            // An in-bounds element (every caller has bounds-checked `index`)
            // lies inside the array, so this cannot overflow in practice;
            // fall back to the header card rather than wrap if it ever did.
            index
                .checked_mul(ref_element_size())
                .and_then(|o| o.checked_add(ARRAY_DATA_OFFSET))
                .and_then(|o| holder.checked_add(o))
                .unwrap_or(holder)
        } else {
            holder
        };
        self.write_barrier_marking(array, mark_addr, stored_value);
    }

    /// [`Self::write_barrier`]'s body, dirtying the card of `mark_addr` — the
    /// holder's header, or (gen r4w3/cards3) an element slot inside it.
    /// Whether the edge is cross-generational is still decided by the
    /// HOLDER's address.
    #[inline]
    pub fn write_barrier_marking(&self, obj: ObjectRef, mark_addr: usize, stored_value: Value) {
        // FAST PATH: only reference stores can produce a cross-gen edge.
        // Bail out BEFORE touching either object header for primitive/null
        // stores so the barrier cost on the common (primitive) path is
        // a single tag check.
        let target_ref = match stored_value {
            Value::Object(Some(r)) => r,
            _ => return,
        };

        // Observability, DELIBERATELY GATED (`cratonvm_gc_core::gc_metrics`): this barrier
        // runs on every reference store in the VM, so an unconditional
        // `fetch_add` here would be a process-global cache line pinged between
        // every storing mutator. `record_barrier_ref_store` compiles to one
        // relaxed load of an already-resolved, read-only-shared byte plus a
        // predicted not-taken branch unless `CRATONVM_GC_CARD_METRICS` is set.
        cratonvm_gc_core::gc_metrics::record_barrier_ref_store();

        let src_addr = obj.as_ptr() as usize;
        let dst_addr = target_ref.as_ptr() as usize;

        // Round-5 #11 fix: replace the two header dereferences with two
        // pure address-range comparisons against the cached old-gen
        // bounds. The card table already stores `base_addr` and
        // `region_size` for the old-gen arena (it has to — every dirty
        // entry is indexed by `(addr - base) / CARD_SIZE`). Reading
        // those two `usize` fields touches one already-hot cache line
        // (`self.card_table`) instead of two cold object headers.
        let old_base = self.card_table.base_addr();
        let old_end = old_base.wrapping_add(self.card_table.region_size());

        // Source must live in the old-gen arena. If it doesn't, this is
        // either a young→young store (no card needed) or a write into
        // GC-internal scratch space; either way, nothing to record.
        if src_addr < old_base || src_addr >= old_end {
            return;
        }

        // Target must live OUTSIDE the old-gen arena (i.e. in young) for
        // the edge to be cross-generational. Old→old refs are followed
        // by full-heap marking, not the card table.
        if dst_addr >= old_base && dst_addr < old_end {
            return;
        }

        // gc-genpause F5.2: one relaxed load and, at most, one release byte
        // store straight into the card map -- the same instruction sequence
        // the JIT's inline post barrier emits.
        //
        // This used to route through a per-thread buffer
        // (`thread_local_dirty_addr`): a TLS lookup, an `Arc` deref, a
        // `parking_lot::Mutex`, a linear table-id lookup and a `Vec::push`
        // that may reallocate, PER REFERENCE STORE, whose entries a safepoint
        // then folded into this very byte map. The indirection existed only
        // because the map was unreachable without the collector's mutex;
        // `CardTable::cards_addr` now caches its base, so the barrier can do
        // what the JIT has always done. It also never deduplicated -- a field
        // written in a loop buffered one entry per store, every one of them
        // naming the same card, which is exactly what `duplicate_card_marks`
        // counts.
        //
        // The buffered API is still present and still correct; nothing on the
        // mutator path calls it any more, so `flush_all` + `drain_pending` at
        // GC start are now walks over empty buffers.
        //
        // gen r4w3/cards3: `mark_addr` is `src_addr` except for an
        // element-precise reference-array store.
        self.card_table.mark_dirty_lockfree(mark_addr);
        // Same gate as `record_barrier_ref_store` above.
        //
        // CORRECTION (gengc-round4/cards, 2026-09-23): this note used to say
        // the counter sees "interpreter/native card marks ONLY" because the
        // JIT's inline post-write barrier stores `CARD_DIRTY` directly. By
        // default every compiled reference store into an old holder reaches
        // this function through `jit_write_barrier`, `jit_putfield_object` or
        // `jit_aastore`, so this counts every generational card mark in the
        // VM, compiled ones included. gcd d1/d: except under the opt-in
        // `CRATONVM_JIT_INLINE_CARD_MARK=1` (gen r4w4/cards4;
        // `Compiler::inline_card_mark_available()` is
        // `jit_inline_card_mark_enabled() && ..`, not the hard `false` this
        // note used to claim): that barrier reads the card inline and calls in
        // only for a CLEAN one, so a compiled store into an already-dirty card
        // is not counted here. (Nor did `duplicate_card_marks` ever "see
        // both" — it is fed by this path and the dead buffered drain only.)
        cratonvm_gc_core::gc_metrics::record_card_mark();
    }

    /// Card-table metadata for the x64 inline post-write barrier:
    /// `(card view address, old_base, old_end)`.
    ///
    /// gen r4w4/cards4 (2026-09-24): the first word is now
    /// [`CardTable::jit_card_view_addr`], the read-only [`cratonvm_gc_core::card_table::JitCardView`],
    /// NOT the raw card map. Compiled code reads card bytes through it and
    /// calls the collector's own barrier for every clean→dirty transition, so
    /// fetching this no longer disarms the scan bound or the summary map (it
    /// used to go through `CardTable::jit_cards_addr`, which does). That is
    /// what lets `build_helpers_for` fetch it in every generational run.
    /// `old_base`/`old_end` are informational; the emitted code loads the
    /// live values from the view.
    pub fn jit_card_table_info(&self) -> (usize, usize, usize) {
        let base = self.card_table.base_addr();
        (
            self.card_table.jit_card_view_addr(),
            base,
            base.wrapping_add(self.card_table.region_size()),
        )
    }

    /// gcj j2/jt2 — compiled reference-store sites emitted with no pre-store
    /// card check (`CRATONVM_JIT_POST_ONLY_CARD_MARK`), counted in this heap's
    /// own card view ([`CardTable::jit_post_only_card_sites`]).
    pub fn jit_post_only_card_sites(&self) -> u64 {
        self.card_table.jit_post_only_card_sites()
    }

    /// gcj j2/jt2 — the BUG-03 take-over's half of the post-only card mark:
    /// dirty the card of every word of a frozen peer's register file that lies
    /// in the card table's range ([`CardTable::complete_frozen_register_marks`],
    /// which carries the soundness argument). Called by the take-over at the
    /// end of a pause, after every card scan and clear, before the peer is
    /// resumed. Nothing is done, and `0` returned, while this heap's JIT has
    /// emitted no post-only site: then no compiled store relies on it.
    pub fn takeover_complete_post_only_card_marks(&self, words: &[usize]) -> usize {
        if self.card_table.jit_post_only_card_sites() == 0 {
            return 0;
        }
        self.card_table.complete_frozen_register_marks(words)
    }

    /// gen r4w4/cards4 — THIS heap's compiled-reference-store barrier plan, as
    /// `(pre_gate_addr, post_gate_addr, young_floor_addr, post_skip_mask)`,
    /// all zero when the plan was withheld (`CRATONVM_GC_JIT_REF_STORE_GATES=0`).
    ///
    /// The per-VM replacement for [`jit_ref_store_gate_addrs`] +
    /// [`jit_ref_store_post_skip_mask`]: the post gate and the mask come from
    /// this heap's own block, so nothing another heap in the process does can
    /// change them. The pre gate is the process-wide disjunction byte, which is
    /// the right answer for every heap (armed while any marker is).
    /// `young_floor_addr` is always 0: this collector's shape is the mask.
    pub fn jit_ref_store_plan(&self) -> (usize, usize, usize, u8) {
        let own = &self.jit_ref_store_gates;
        if own.published.load(Ordering::Acquire) == 0 {
            return (0, 0, 0, 0);
        }
        (
            // The process-wide pre byte, or its code-adjacent copy once
            // installed (t1 lane R, `install_jit_satb_pre_gate_cell`).
            jit_satb_pre_gate_addr(),
            &own.post_active as *const AtomicU8 as usize,
            0,
            own.post_skip_mask.load(Ordering::Acquire),
        )
    }

    /// SATB write barrier — called BEFORE a reference field is overwritten.
    ///
    /// If the SATB queue is accepting logs, logs the old reference value to it
    /// so the concurrent marker won't miss live objects.
    ///
    /// This should be called by the interpreter/JIT before every putfield/putstatic
    /// that overwrites a reference-typed field.
    ///
    /// gc-common w5-e (`common-g-proposal-one-satb-gate`, finished): the gate
    /// is the QUEUE, `SatbQueue::is_active()`, not the marker's phase. The
    /// queue is the gate that actually retains the value, and it owns the
    /// compiled-code pre-gate too (`satb.rs`, `jit_gate_armed`), so the old
    /// half-open window — phase says "marking", queue already closed, the
    /// store reads its old value and the log throws it away — cannot be
    /// expressed any more, and the `debug_assert!` that guarded the
    /// phase/queue order here is gone with it. The queue's accepting window
    /// covers every marking-active phase (it activates at initial mark before
    /// `ConcurrentMark`, and closes after the phase has left marking in
    /// `remark` and `abort_cycle`), so this can only log MORE than the phase
    /// gate did, never less.
    #[inline]
    pub fn satb_barrier(&self, old_value: Value) {
        // Fast path: one Acquire load of the queue's state.
        let Some(ref satb) = self.satb_queue else {
            return;
        };
        if !satb.is_active() {
            return;
        }

        // Only log reference values
        let old_ref = match old_value {
            Value::Object(Some(r)) => r,
            _ => return,
        };

        // gen r5w1/refs5 (opt-in `CRATONVM_GEN_SATB_OLD_ONLY`;
        // `gengc-r4w5-concmark5-satb-barrier-logs-young-old-values`): the
        // generational concurrent marker traces the OLD generation only, and
        // every consumer drops a non-old entry (`markable_old_object`). A young
        // old-value needs no logging: an old object reachable at the snapshot
        // only through a young one is an initial-mark root
        // (`collect_young_to_old_roots`), and a young value promoted mid-cycle
        // lies above the initial-mark snapshot. The card table spans exactly
        // the old generation's reservation (`write_barrier`'s own range test),
        // so this is one subtraction and one compare on two hot fields, paid
        // only while the queue is active. The flag read is the `OnceLock`
        // probe `gc_flags()` always is.
        if gc_flags().gen_satb_old_only {
            let addr = old_ref.as_ptr() as usize;
            if addr.wrapping_sub(self.card_table.base_addr()) >= self.card_table.region_size() {
                return;
            }
        }

        // Log the old reference to the per-thread SATB buffer. The buffer
        // auto-flushes into the global queue every ~256 entries, so the
        // hot write-barrier path takes no shared lock in the common case.
        cratonvm_gc_core::satb::satb_thread_local_log(satb, old_ref.as_ptr() as usize);
    }

    /// gcj j1/rf (`common-watched-referent-set-is-per-thread-not-per-heap-20261012`
    /// section 2) — this heap's key in the per-thread watched-referent table
    /// (`gc_quiescence::set_watched_referents_for` /
    /// `watched_referents_snapshot_for`): the base of its old generation's
    /// reservation, which the card table spans and which no other live heap
    /// in the process can share. Lock-free (the card table's copy, not the
    /// old generation's lock), so a reader already holding that lock may ask.
    /// The VM reads the same word through `VmHeap::jit_card_table_info`.
    #[inline]
    pub fn watched_set_key(&self) -> usize {
        crate::old_gen::watched_set_key_of(self.card_table.base_addr() as *const u8)
    }

    /// Enable concurrent GC support by attaching shared SATB and state.
    ///
    /// gc-common w5-e: the barrier's gate is the queue (`satb_barrier`), so
    /// the BARRIER has no reader for the phase tracker.
    ///
    /// gen r4w4/concmark4 (2026-09-24): the state is kept again, for the
    /// old-gen POLICY only (`major_trigger_decision`,
    /// `concurrent_cycle_due`): whether a cycle is open, and the adaptive
    /// start threshold's memory. It is the same instance the VM's cycle
    /// drivers build their markers on, so the policy sees the cycles they run.
    pub fn enable_concurrent_gc(
        &mut self,
        satb_queue: Arc<SatbQueue>,
        gc_state: Arc<ConcurrentGcState>,
    ) {
        self.satb_queue = Some(satb_queue);
        self.conc_cycle_state = Some(gc_state);
    }

    /// Round-5 fix (CRIT — UAF): expose the SATB queue handle so the
    /// VM can drain per-thread SATB buffers at safepoint entry. Returns
    /// `None` until `enable_concurrent_gc` has been called (i.e. until
    /// the concurrent old-gen collector is wired up).
    pub fn satb_queue_handle(&self) -> Option<Arc<SatbQueue>> {
        self.satb_queue.clone()
    }

    /// gc-common w36-d — the Generational arm of
    /// `VmHeap::take_resurrection_closure`: PRE-collection addresses of every
    /// object the last collection's finalizer-resurrection rounds marked that
    /// its strong closure had not (the resurrected finalizables included).
    /// Drained: a second call answers empty. Every collection clears it at
    /// entry, so a pause whose caller skipped the take cannot leak its set into
    /// the next pause.
    pub fn take_resurrection_closure(&self) -> Vec<usize> {
        std::mem::take(&mut *self.resurrection_closure.lock())
    }

    /// Append one resurrection round's depth-2 set (see
    /// [`Self::take_resurrection_closure`]). No lock is taken for an empty
    /// round, which is every round of a cycle without a dead finalizable.
    pub fn note_resurrection_closure(&self, addrs: &[usize]) {
        if !addrs.is_empty() {
            self.resurrection_closure.lock().extend_from_slice(addrs);
        }
    }

    /// gcj j3/rf3 (ec47, the legacy seed): the CURRENT addresses of the young
    /// resurrection set this pause has recorded so far (its young phase's
    /// drain; [`young_resurrection_members_of`] maps them through
    /// `pointer_map`), for [`OldGenFinalizerPass::young_resurrected`]. Read,
    /// not drained. Empty (no lock held past the check) on every pause without
    /// a dead young finalizable.
    pub fn young_resurrection_members(&self, pointer_map: &cratonvm_types::PointerMap) -> Vec<usize> {
        let closure = self.resurrection_closure.lock();
        if closure.is_empty() {
            return Vec::new();
        }
        young_resurrection_members_of(&closure, pointer_map)
    }

    /// Borrow the SATB queue without cloning the `Arc`. The JIT helper
    /// entry path (`VmHeap::flush_thread_satb`) checks `is_active()` on
    /// EVERY slow-path allocation; the refcount round trip of
    /// [`Self::satb_queue_handle`] is measurable there and buys nothing —
    /// the queue, once installed by `enable_concurrent_gc`, lives as long
    /// as the heap.
    #[inline]
    pub fn satb_queue_ref(&self) -> Option<&SatbQueue> {
        self.satb_queue.as_deref()
    }

    /// Get the old generation's base pointer and capacity (for creating a ConcurrentMarker).
    pub fn old_gen_info(&self) -> (usize, usize) {
        let og = self.old_gen.lock();
        (og.base_ptr() as usize, og.capacity())
    }

    /// gcj j2/og2 (C2 of `gcj-j1og-concurrent-cycle-and-old-gen-review-residuals`)
    /// — the span a concurrent cycle's mark bitmap is built over: the old
    /// generation's base and its GROWTH limit ([`OldGen::growth_max`]), not its
    /// capacity. The driver sizes the bitmap before the initial-mark pause
    /// takes the lock; a growth in that window (another thread's major:
    /// `grow_after_refusal`, `grow_to_min_free_ratio`) used to leave the grown
    /// span outside the bitmap, and the remark then refused the whole traced
    /// cycle (`bitmap_covers_generation`). The bitmap is `alloc_zeroed`, so
    /// the extra span costs address space until a mark lands in it. Equal to
    /// [`Self::old_gen_info`] on a generation built without headroom.
    /// `CRATONVM_GC_CONC_BITMAP_GROWTH_SPAN=0` restores the capacity span.
    ///
    /// gcj j3/x2 R5: the headroom is CAPPED ([`conc_mark_span_for`]): the
    /// capacity plus a quarter of it (at least
    /// [`CONC_MARK_SPAN_MIN_HEADROOM`]), never past the growth limit. Both the
    /// mark bitmap and the reference-skip bitmap are `alloc_zeroed` over this
    /// span at span / 64 bytes each, and on Windows a large zeroed allocation
    /// is commit-charged though never touched: under `-Xmx8g` the full growth
    /// limit charged ~256 MiB per concurrent cycle for a generation that may
    /// be a few tens of MiB (and ~55 MiB of it for the headroom alone where
    /// the capacity is already 4 GiB). A growth in the driver's window past
    /// the capped headroom is refused at remark, as every growth was before
    /// og2.
    pub fn old_gen_mark_span(&self) -> (usize, usize) {
        let og = self.old_gen.lock();
        let span = {
            conc_mark_span_for(og.capacity(), og.growth_max())
        };
        (og.base_ptr() as usize, span)
    }

    /// True when `addr` lies inside the old generation arena.
    ///
    /// Used by weak/soft/phantom reference processing after a **young** GC to
    /// decide whether a referent survived. A minor collection never touches the
    /// old generation, so every old-gen object is live for the purpose of
    /// reference clearing — the referent must NOT be cleared just because it was
    /// promoted out of the young space in an earlier cycle.
    ///
    /// Without this, `VmHeap::is_addr_live` returned `false` for the
    /// generational collector and `process_references_after_gc`'s
    /// `pointer_map.contains_key(addr) || is_addr_live(addr)` predicate cleared
    /// EVERY weak reference whose referent had been tenured to old gen. Visible
    /// symptom: a `WeakReference<ClassLoader>` (e.g. WildFly's
    /// `StandardResourceDescriptionResolver.bundleLoader`) read back null after
    /// the first young GC even though the classloader was strongly reachable,
    /// so `ResourceBundle.getBundle(.., null, ..)` threw `MissingResourceException`.
    pub fn is_old_gen_addr(&self, addr: usize) -> bool {
        self.old_gen.lock().contains(addr as *const u8)
    }

    /// True when `addr` is inside an **allocated** old-gen span.
    ///
    /// The strict counterpart of [`Self::is_old_gen_addr`], which is a bare
    /// range check. That range check is the right answer to "which generation
    /// is this address in?" (`metadata_pin_deferrable` / `mirror_pin_deferrable`
    /// ask exactly that) but the WRONG answer to "is the object still there?".
    ///
    /// `is_old_gen_addr`'s comment justifies the permissive form with "a minor
    /// collection never touches the old generation". That premise does not hold:
    /// [`Self::sweep_old_gen_non_moving`] runs a full old-gen mark-sweep that
    /// reclaims dead blocks IN PLACE, during a YOUNG collection, whenever a live
    /// JIT frame blocks the moving young collector — and it does not zero what
    /// it frees. Use this predicate for liveness; see
    /// `gc-old-gen-mark-accepts-unvalidated-addresses-FIXED.md`.
    pub fn is_live_old_gen_addr(&self, addr: usize) -> bool {
        self.old_gen.lock().is_allocated_addr(addr as *const u8)
    }

    /// True when `addr` is the start of a young from-space object that
    /// SURVIVED the collection that just ran (young-GC live-reclaim ROOT FIX,
    /// 2026-07-07 — RRWL/ThreadLocalMap$Entry IMSE/hang family).
    ///
    /// The NON-MOVING young sweep keeps survivors in place with NO
    /// `pointer_map` entry, so post-GC reference processing's survivor
    /// predicate (`pointer_map.contains_key(addr) || is_addr_live(addr)`)
    /// judged every live young Reference object and referent dead: nulled
    /// referents were never restored and live processor entries were pruned.
    /// The watched-referent identity-map mechanism covers survivors the
    /// sweep's disposition walk actually visits, but a desync-retained
    /// stretch (walk re-anchor) leaves its survivors without entries — this
    /// predicate closes that gap at the consumer side.
    ///
    /// Discriminator: the sweep ZEROES every span it reclaims before
    /// publishing it to the free list, so inside the current from-space
    /// "header non-zero" == "not reclaimed by the sweep that just ran".
    ///
    /// gen r4/plumbing (2026-09-23): BOTH header words, not the first one.
    /// Word 0 is `class_id | shape`, and for the commonest object in Java —
    /// `new Object()`, `ClassId(0)` with no fields — it is legitimately zero
    /// (see `GC_FLAG_HEADER`'s doc in `types/src/heap_types.rs`, which records
    /// the same trap biting the sweep itself). Reading word 0 alone therefore
    /// answered "reclaimed" for every live field-less `java.lang.Object`: a
    /// lock object, a sentinel, a `WeakReference<Object>` referent kept in
    /// place on a desync-retained stretch. Word 1 is the mark word, which
    /// carries `GC_FLAG_HEADER` from allocation on, and a reclaimed span is
    /// zero in both. Widening the test can only turn a "dead" into a "live"
    /// for an address whose 16 bytes are not all zero, i.e. it moves only in
    /// the over-retention direction this predicate already accepts.
    ///
    /// gen r5w3/obs7 (2026-09-26): the paragraph above describes the header
    /// layout of 2026-09-23. `ObjectHeader` now keeps the MARK word at offset
    /// 4 (`MARK_WORD_OFFSET`), inside word 0 (`class_id | mark << 32`), and
    /// `shape` / `aux` at 8 / 12. Word 0 of every live object therefore
    /// carries `GC_FLAG_HEADER` and is non-zero, so the first test alone
    /// decides for a live object; the second read (offset 8) now sees `shape`
    /// / `aux` (or a compact instance's first field), and is kept only as the
    /// same over-retention-direction widening for a span whose word 0 is
    /// zero. `gc/tests/gengc_r4_plumbing_observability.rs::a_live_field_less_object_is_a_young_survivor`
    /// pins the current layout.
    ///
    /// This is only sound where it is used — between `collect_garbage`
    /// returning and mutators resuming (the same STW window in which
    /// `process_references_after_gc` runs), when no allocation can have
    /// reused a reclaimed hole yet. After a MOVING collection, pre-GC
    /// addresses lie in the *other* semispace (survivors were evacuated and
    /// the arenas swapped), so this returns false for them and the
    /// pointer-map half of the predicate remains authoritative, exactly as
    /// before. A dead-but-retained object in a desync-skipped stretch reads
    /// as "live" for one cycle — pure over-retention, the sweep's documented
    /// safe direction.
    ///
    /// The "reclaimed span is zero" premise stopped holding with JIT round 23's
    /// lazy zero (`CRATONVM_GEN_IN_PLACE_LAZY_ZERO` +
    /// `CRATONVM_GEN_IN_PLACE_DEAD_MIRROR_ONLY` for the pinned in-place cycle,
    /// `CRATONVM_GEN_SWEEP_LAZY_ZERO` for the non-moving sweep, all default
    /// ON): a dead object's header is left in place and only the arena's
    /// lazy-dirty set says the span holds no object. So an address inside a
    /// lazy-dirty span is reclaimed, whatever its header reads. Without this
    /// test every dead watched referent of such a cycle read "survived" and
    /// its weak reference was never cleared
    /// (`gen-jit-young-pauses-keep-both-weak-referents-again`:
    /// `WeakClearYoungProbe` `inline=LIVE viaMethod=LIVE` on Generational with
    /// the JIT on).
    pub fn is_live_young_survivor(&self, addr: usize) -> bool {
        if addr & 0x7 != 0 {
            return false;
        }
        let from = self.lock_young_from();
        let base = from.base_ptr() as usize;
        let used_end = base + from.used();
        if addr < base || addr >= used_end {
            return false;
        }
        if from.lazy_dirty_contains(addr) {
            return false;
        }
        // SAFETY: bounds-checked 8-aligned address inside the mapped
        // from-space region; reading one u64 is valid.
        let word0 = unsafe { std::ptr::read(addr as *const u64) };
        if word0 != 0 {
            return true;
        }
        // The mark word, only when the whole 16-byte header lies below the
        // allocation frontier (an 8-aligned address 8 bytes short of it cannot
        // start an object anyway).
        if addr + HEADER_SIZE > used_end {
            return false;
        }
        // SAFETY: `addr + HEADER_SIZE <= used_end`, so the second header word
        // is inside the same mapped, allocated from-space prefix.
        let mark = unsafe { std::ptr::read((addr + 8) as *const u64) };
        mark != 0
    }

    /// gch h4/wr (`CRATONVM_GC_FULL_GC_TRUE_ROOTS_WATCHED_DEAD`, default ON since gci i1/og and jr20 wave 9): did
    /// the last non-moving true-root major leave the WATCHED young survivor at
    /// the pre-collection address `addr` out of its trace? Such an object is
    /// still in place (its header reads live to [`Self::is_live_young_survivor`])
    /// but nothing live reaches it, and its old referents may have been freed,
    /// so a weak table must not hand it back. Always `false` with the switch
    /// off, and between the start of a collection and its true-root major.
    pub fn true_root_watched_left_out(&self, addr: usize) -> bool {
        if self.true_root_watched_left_out_len.load(Ordering::Acquire) == 0 {
            return false;
        }
        self.true_root_watched_left_out
            .lock()
            .binary_search(&addr)
            .is_ok()
    }

    /// gch h4/wr: the current verdict, ascending (empty on almost every
    /// pause), for a caller that asks about many addresses in one window (the
    /// String table's sweep): one lock instead of one per address.
    pub fn true_root_watched_left_out_snapshot(&self) -> Vec<usize> {
        if self.true_root_watched_left_out_len.load(Ordering::Acquire) == 0 {
            return Vec::new();
        }
        self.true_root_watched_left_out.lock().clone()
    }

    /// gch h4/wr: how many watched young survivors the true-root majors of
    /// this heap judged dead, over the run (see
    /// [`Self::true_root_watched_left_out`]).
    pub fn true_root_watched_left_out_total(&self) -> u64 {
        self.true_root_watched_left_out_total
            .load(Ordering::Relaxed)
    }

    /// gch h4/wr: install one major's verdict (`addrs`, any order) and remove
    /// those addresses from the pointer map the collection returns: the young
    /// sweep gave every kept-in-place watched survivor an identity entry, which
    /// every post-collection consumer reads as "survived" before it asks the
    /// heap (`jni::sweep_weak_global_refs_with`, the reference restore).
    /// Returns how many it installed.
    pub fn install_true_root_watched_left_out(
        &self,
        mut addrs: Vec<usize>,
        pointer_map: &mut cratonvm_types::PointerMap,
    ) -> usize {
        for addr in &addrs {
            pointer_map.remove(addr);
        }
        addrs.sort_unstable();
        addrs.dedup();
        let n = addrs.len();
        self.true_root_watched_left_out_total
            .fetch_add(n as u64, Ordering::Relaxed);
        *self.true_root_watched_left_out.lock() = addrs;
        self.true_root_watched_left_out_len
            .store(n, Ordering::Release);
        n
    }

    /// gch h4/wr: a new collection starts; the last major's verdict expires.
    pub fn clear_true_root_watched_left_out(&self) {
        if self.true_root_watched_left_out_len.load(Ordering::Acquire) == 0 {
            return;
        }
        self.true_root_watched_left_out_len
            .store(0, Ordering::Release);
        self.true_root_watched_left_out.lock().clear();
    }

    /// H2-CID0 (2026-08-05) — which LIVE objects still hold `addr` in a
    /// reference slot?
    ///
    /// The answer to "the reference survived un-rewritten" is an object and a
    /// slot, and nothing reports it on the unperturbed path:
    /// `CRATONVM_DBG_STALE_OBJREF`'s attribution needs the quarantine (which
    /// changes the arena-reuse timing the defect lives in) and its thread-local
    /// half only walks frames, while the measured holder came out of a
    /// `getfield`.
    ///
    /// Walks old gen and the live young arena and asks each object's own slot
    /// ENUMERATOR — never a raw word comparison. A 16-byte legacy `Value` cell
    /// overwritten by a narrower variant keeps the previous `Object`'s pointer
    /// in its unused upper half, and this heap carries ~1 M such words per
    /// compaction, so a word scan would report holders that no decoder can
    /// produce.
    ///
    /// Returns `(holder_addr, holder_class_id, slot)` triples, capped. Only
    /// ever called from a terminal error path, so the O(heap) cost is paid
    /// exactly once and only after something has already gone wrong.
    pub fn live_holders_of(&self, addr: usize, cap: usize) -> Vec<(usize, u32, usize)> {
        let mut out: Vec<(usize, u32, usize)> = Vec::new();
        if addr == 0 {
            return out;
        }
        {
            let old_gen = self.old_gen.lock();
            for (obj_ptr, _size) in old_gen.walk_objects() {
                if out.len() >= cap {
                    return out;
                }
                // SAFETY: `walk_objects` yields valid old-gen object starts.
                let h = unsafe { &*(obj_ptr as *const ObjectHeader) };
                let cid = h.class_id.as_u32();
                // SAFETY: a walked base with a valid header and in-bounds body
                // — `for_each_ref_slot`'s contract.
                unsafe {
                    for_each_ref_slot(obj_ptr, h, |p, slot| {
                        if p as usize == addr && out.len() < cap {
                            out.push((obj_ptr as usize, cid, slot));
                        }
                    });
                }
            }
        }
        {
            let young = self.lock_young_from();
            let base = young.base_ptr() as usize;
            let used = young.used();
            let free = young.free_blocks_sorted();
            // gen r5w1/young5: on the shared `YoungGridCursor` (GAP fillers,
            // empty-object runs and hole crossings, the rules this walk
            // lacked: it stopped at the first retired-TLAB tail sentinel and
            // reported a prefix of from-space as the answer). An anomaly still
            // ends the walk: off the object grid, a partial answer beats a
            // wrong one.
            for step in YoungGridCursor::new(base, used, &free, &[]) {
                if out.len() >= cap {
                    break;
                }
                let GridStep::Object { off, .. } = step else {
                    break;
                };
                let obj_ptr = (base + off) as *mut u8;
                // SAFETY: the cursor parsed a plausible header at this
                // in-bounds offset of mapped from-space.
                let h = unsafe { &*(obj_ptr as *const ObjectHeader) };
                let cid = h.class_id.as_u32();
                // SAFETY: a walked young object with an in-bounds body.
                unsafe {
                    for_each_ref_slot(obj_ptr, h, |p, slot| {
                        if p as usize == addr && out.len() < cap {
                            out.push((obj_ptr as usize, cid, slot));
                        }
                    });
                }
            }
        }
        out
    }

    /// Is `addr` inside memory the collector has RECLAIMED — a free-list hole,
    /// the unallocated tail of a semispace, or the inactive semispace?
    ///
    /// H2-CID0 (2026-08-01). A reference that reads back as `java.lang.Object`
    /// is ambiguous on its face: `java.lang.Object` is `ClassId(0)`, and so is
    /// the ALL-ZERO header the young sweep writes over every span it reclaims —
    /// but an ordinary `new Object()` whose identity hash has not been minted
    /// also has an all-zero first header word, so no amount of staring at the
    /// header separates the two. That ambiguity is why the H2 `MVStore` cache
    /// investigation could only self-diagnose when `CRATONVM_DBG_SWEEP_ZERO`
    /// had been set BEFORE the run — i.e. never, in the report that matters.
    /// Both reclamation rings above are unconditional for that reason.
    ///
    /// Free-list membership is not ambiguous. A live object is never inside a
    /// free block, never past the allocation frontier, and never in the
    /// inactive semispace. So this answers the question outright, costs a
    /// copy and sort of the young free list per call, so a caller with many
    /// addresses in one allocation-free window uses `reclaimed_hole_probe`
    /// instead (gc-common w16-x), and needs no flag to have been set in
    /// advance.
    ///
    /// Returns `(what, span_base, span_size)`; `span_size` is 0 for the
    /// answers that are a region rather than a block.
    ///
    /// (gen r4/plumbing, 2026-09-23: everything above the previous paragraph
    /// used to open the doc of `live_holders_of`, the function in between.)
    pub fn reclaimed_hole_at(&self, addr: usize) -> Option<(&'static str, usize, usize)> {
        {
            let from = self.lock_young_from();
            let base = from.base_ptr() as usize;
            let used = from.used();
            let cap = from.capacity();
            if addr >= base && addr < base + cap {
                if addr >= base + used {
                    return Some((
                        "young from-space, past the allocation frontier",
                        base + used,
                        0,
                    ));
                }
                let off = addr - base;
                let blocks = from.free_blocks_sorted();
                let i = blocks.partition_point(|&(o, _)| o <= off);
                if i > 0 {
                    let (o, sz) = blocks[i - 1];
                    if off < o + sz {
                        return Some(("young from-space FREE BLOCK (reclaimed)", base + o, sz));
                    }
                }
                return None;
            }
        }
        {
            let to = self.young_to.lock();
            let base = to.base_ptr() as usize;
            if addr >= base && addr < base + to.capacity() {
                return Some(("young TO-space (the inactive semispace)", base, 0));
            }
        }
        {
            let old = self.old_gen.lock();
            if old.contains(addr as *const u8) && !old.is_allocated_addr(addr as *const u8) {
                return Some(("old-gen FREE BLOCK (reclaimed)", addr, 0));
            }
        }
        None
    }

    /// `[base, base + capacity)` of the INACTIVE young semispace — the arena
    /// the last moving cycle evacuated and zeroed.
    ///
    /// The same answer [`Self::reclaimed_hole_at`] gives from its `young_to`
    /// arm, hoisted so a caller with many addresses to test (the
    /// blocked-thread root audit) pays one lock rather than three per
    /// address. A live object is never in here.
    pub fn young_inactive_semispace_range(&self) -> (usize, usize) {
        let to = self.young_to.lock();
        let base = to.base_ptr() as usize;
        (base, base + to.capacity())
    }

    /// gci i13/vg (`CRATONVM_GC_VT_PARKED_GATHER_FILTER`): the young set the
    /// VM's split root gather tests parked continuations against
    /// (`VmHeap::parked_old_only_young_ranges`): BOTH semispaces, whole
    /// capacity (a word anywhere in either is young, live or not). `None`
    /// while a concurrent old mark is open (SATB armed), when the split is not
    /// offered at all. The section a `Some` answer lets the VM leave out of
    /// the root slice is staged with [`Self::stage_parked_old_only_roots`] and
    /// re-joins the roots of every old-space phase of the same collection
    /// ([`Self::take_parked_old_only_roots`]: the moving cycle's Phase 5 major
    /// and `run_non_moving_young_cycle`'s old sweep, the only two in a
    /// collection door). A young phase follows no root outside these ranges:
    /// the moving copy forwards only from-space objects and the non-moving
    /// mark only young ones, old -> young edges coming from the card table.
    pub fn parked_old_only_young_ranges(&self) -> Option<Vec<(usize, usize)>> {
        if self.satb_queue_ref().is_some_and(|q| q.is_active()) {
            return None;
        }
        let from = {
            let f = self.lock_young_from();
            let base = f.base_ptr() as usize;
            (base, base + f.capacity())
        };
        Some(vec![from, self.young_inactive_semispace_range()])
    }

    /// gci i13/vg: stage the pause's old-only root section (replacing any
    /// earlier one). See [`Self::parked_old_only_young_ranges`].
    pub fn stage_parked_old_only_roots(&self, old_only: &[ObjectRef]) {
        let mut staged = self.parked_old_only_roots.lock();
        staged.clear();
        staged.extend(old_only.iter().map(|r| r.as_ptr() as usize));
    }

    /// gci i13/vg: take the staged old-only root section (empty when none is
    /// staged) for a phase that marks or moves old space. Taken once: the
    /// one major of a collection consumes it.
    pub fn take_parked_old_only_roots(&self) -> Vec<usize> {
        std::mem::take(&mut *self.parked_old_only_roots.lock())
    }

    /// gcf f2/r: `(base, used, committed, capacity)` of the young from-space,
    /// the young to-space and the old generation, for the `-Xlog:gc+exit` heap
    /// summary (`unified_logging::serial_exit_heap`). The old generation's
    /// committed figure is `reported_old_committed`, the `Tenured Gen` pool's.
    /// Takes `young_from`, `young_to` and the old-generation lock one at a
    /// time, never nested. A reporting call, at VM exit.
    pub fn exit_summary_arenas(&self) -> [(usize, usize, usize, usize); 3] {
        let from = {
            let a = self.lock_young_from();
            (
                a.base_ptr() as usize,
                a.used(),
                a.committed_bytes(),
                a.capacity(),
            )
        };
        let to = {
            let a = self.young_to.lock();
            (
                a.base_ptr() as usize,
                a.used(),
                a.committed_bytes(),
                a.capacity(),
            )
        };
        let old = {
            let og = self.old_gen.lock();
            (
                og.base_ptr() as usize,
                og.used(),
                reported_old_committed(&og),
                og.capacity(),
            )
        };
        [from, to, old]    }

    /// The young addresses the last collection vacated: the inactive
    /// semispace, plus -- after a pinned in-place cycle, which vacates bytes
    /// INSIDE the active semispace -- that cycle's took-back spans (gen
    /// r5w5/pin9; closes
    /// `docs/feature-designs/generative-gc-architecture.md`).
    ///
    /// Two short locks, taken one after the other (`young_to`, then the span
    /// list), never nested. Meant for the pause that ran the collection, as
    /// the blocked-thread fold uses it; see [`YoungVacatedProbe`].
    pub fn young_vacated_probe(&self) -> YoungVacatedProbe {
        let inactive = self.young_inactive_semispace_range();
        let in_place = self.young_in_place_vacated.lock().clone();
        YoungVacatedProbe { inactive, in_place }
    }

    /// One-address form of [`Self::young_vacated_probe`].
    pub fn young_vacated_contains(&self, addr: usize) -> bool {
        let (lo, hi) = self.young_inactive_semispace_range();
        if addr >= lo && addr < hi {
            return true;
        }
        let spans = self.young_in_place_vacated.lock();
        let i = spans.partition_point(|&(a, _)| a <= addr);
        i > 0 && addr - spans[i - 1].0 < spans[i - 1].1
    }

    /// [`Self::reclaimed_hole_at`]'s two young arms, captured ONCE, for a
    /// caller that judges many addresses in a window in which nothing can
    /// allocate: the stop-the-world epilogue's weak side-table sweeps
    /// (`addr_keyed::InPlaceVerdict`). Answer with
    /// [`Self::is_reclaimed_hole_probed`].
    ///
    /// gc-common w16-x: `reclaimed_hole_at` rebuilds and sorts the whole young
    /// free list on every call (`Arena::free_blocks_sorted`). Once per dead row
    /// of every weak side table, that made a collection O(dead × free blocks),
    /// and a `System.gc()` after 20 000 dead `Properties` took seconds.
    ///
    /// Stale the moment anything allocates in young from-space. Never keep one
    /// past the pause that captured it.
    pub fn reclaimed_hole_probe(&self) -> YoungReclaimProbe {
        let (from_base, from_used, from_cap, from_free) = {
            let from = self.lock_young_from();
            (
                from.base_ptr() as usize,
                from.used(),
                from.capacity(),
                from.free_blocks_sorted(),
            )
        };
        // `young_from` is released before `young_to` is taken, the order
        // `reclaimed_hole_at` uses.
        let (to_lo, to_hi) = self.young_inactive_semispace_range();
        YoungReclaimProbe {
            from_base,
            from_used,
            from_cap,
            from_free,
            to_lo,
            to_hi,
        }
    }

    /// `self.reclaimed_hole_at(addr).is_some()`, with the two young arms
    /// answered from `probe` (see [`Self::reclaimed_hole_probe`]). The old-gen
    /// arm is asked live. It is already cheap, because `OldGen` caches its
    /// sorted free list.
    pub fn is_reclaimed_hole_probed(&self, probe: &YoungReclaimProbe, addr: usize) -> bool {
        if addr >= probe.from_base && addr < probe.from_base + probe.from_cap {
            if addr >= probe.from_base + probe.from_used {
                return true;
            }
            let off = addr - probe.from_base;
            let i = probe.from_free.partition_point(|&(o, _)| o <= off);
            return i > 0 && {
                let (o, sz) = probe.from_free[i - 1];
                off < o + sz
            };
        }
        if addr >= probe.to_lo && addr < probe.to_hi {
            return true;
        }
        let old = self.old_gen.lock();
        old.contains(addr as *const u8) && !old.is_allocated_addr(addr as *const u8)
    }

    /// Access the old generation directly (for concurrent sweep).
    /// Returns a lock guard.
    pub fn old_gen_lock(&self) -> parking_lot::MutexGuard<'_, OldGen> {
        self.old_gen.lock()
    }

    // ----- GC ----------------------------------------------------------------

    /// Record that a native-wrapper allocation had to spill into old gen
    /// because young was exhausted. See the `young_spill_pressure` field doc.
    ///
    /// The pressure flag (and therefore the boundary GC) is only armed once
    /// old-gen headroom drops below the worst-case promotion demand — the
    /// entire young semi (a boundary GC may need to evacuate ALL of young's
    /// live set into old) plus a young/8 margin. While old gen has more room
    /// than that, spilling is both safe and much cheaper than a full mark
    /// cycle (HashMapOnly 30M at -Xmx16g: 13.5s spilling vs 32.8s collecting
    /// on first exhaustion), so we stay on the spill path. Past the bound,
    /// waiting longer risks promotion failure (old too full to drain young)
    /// → the both-gens-full abort this machinery exists to avoid.
    ///
    /// Takes the old-gen lock — call this only from allocation SLOW paths
    /// (the spill arms / batch refill), never per-object.
    pub fn note_young_spill_pressure(&self) {
        let young_semi = self.young_semi_capacity();
        let advisable = {
            let og = self.old_gen.lock();
            og.used() + young_semi + young_semi / 8 >= og.capacity()
        };
        if advisable {
            self.young_spill_pressure
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Account for retained off-heap GPU VRAM and arm `young_spill_pressure`
    /// when external pacing policy thresholds are crossed.
    #[inline]
    pub fn note_external_gpu_retained(&self, delta_bytes: i64, wrapper_delta: i64) -> bool {
        self.external_gpu
            .apply_signed_delta(delta_bytes, wrapper_delta, &self.young_spill_pressure)
    }

    /// Native-wrapper young-exhaustion signal (see `young_spill_pressure`
    /// field doc). One relaxed load — cheap enough for a per-native-call
    /// check on the dispatch hot path.
    #[inline]
    pub fn young_spill_pressure(&self) -> bool {
        self.young_spill_pressure
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Clear the native-wrapper young-exhaustion signal (after the
    /// boundary GC ran, or after deciding the flag was stale).
    #[inline]
    pub fn clear_young_spill_pressure(&self) {
        self.young_spill_pressure
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    /// Lock `young_from`, republishing the trigger triple when the guard drops.
    ///
    /// The ONLY way this file takes that mutex. See
    /// [`Self::young_from_pub_used`] for why the choke point is the guard
    /// rather than the 83 assignments inside `arena.rs`.
    #[inline]
    pub fn lock_young_from(&self) -> YoungFromGuard<'_> {
        YoungFromGuard {
            guard: self.young_from.lock(),
            used: &self.young_from_pub_used,
            free: &self.young_from_pub_free,
            capacity: &self.young_from_pub_capacity,
        }
    }

    /// [`Self::lock_young_from`] without waiting: `None` when another thread
    /// (or this one, further up its stack) holds the mutex. The same guard, so
    /// a mutation made through it republishes the trigger triple on drop.
    ///
    /// gen r4w6/tlab6: for [`Self::return_tlab_tail`], which runs inside
    /// `Tlab::retire` — reachable from three dozen VM sites, some of them on a
    /// path that may already hold this lock or be racing a collection that
    /// does. A retire must never wait for (or deadlock on) the young arena; a
    /// busy lock just means the filler, as before.
    #[inline]
    pub fn try_lock_young_from(&self) -> Option<YoungFromGuard<'_>> {
        Some(YoungFromGuard {
            guard: self.young_from.try_lock()?,
            used: &self.young_from_pub_used,
            free: &self.young_from_pub_free,
            capacity: &self.young_from_pub_capacity,
        })
    }

    /// The trigger triple, without the lock: `(used, free_list_bytes, capacity)`.
    #[inline]
    pub fn young_from_published(&self) -> (usize, usize, usize) {
        (
            self.young_from_pub_used.load(Ordering::Relaxed),
            self.young_from_pub_free.load(Ordering::Relaxed),
            self.young_from_pub_capacity.load(Ordering::Relaxed),
        )
    }

    /// A young cycle refused to do anything — throttle the retry.
    ///
    /// gengc-round1-core (2026-09-20). See [`Self::young_cycle_skipped`] for
    /// the storm this closes. The floor is computed exactly the way
    /// [`Self::needs_gc_with_jit_allocation_frame`]'s post-collection floor is
    /// — current live occupancy plus a sixteenth of capacity, capped at the
    /// non-moving trigger — so collection is DEFERRED, never abandoned, and
    /// allocation failure remains the hard backstop (`alloc_object` spills to
    /// old gen, then GCs and retries).
    ///
    /// Storing directly rather than going through the `young_trigger_floor`
    /// epoch check is deliberate: that check only recomputes when
    /// `minor_gc_count` moves, and a refused cycle does not move it (it
    /// collected nothing, so counting it as a minor GC would corrupt both the
    /// statistic and `promotion_age_reachable`). The store is the one place
    /// that can raise the floor without a completed collection.
    ///
    /// Called with the `young_from` guard held, so `used`/`free_list_bytes`/
    /// `capacity` are the state the refusing cycle actually saw.
    pub fn note_skipped_young_cycle(&self, young_from: &Arena) {
        self.young_cycle_skipped.store(true, Ordering::Relaxed);
        let capacity = young_from.capacity();
        if capacity == 0 {
            return;
        }
        let live = young_from
            .used()
            .saturating_sub(young_from.free_list_bytes());
        let floor = live
            .saturating_add(capacity / 16)
            .min(capacity * NON_MOVING_YOUNG_GC_THRESHOLD_PERCENT / 100);
        // Monotone within one refusal streak: a second refusal at the same
        // occupancy must not lower a floor a first one already raised.
        let prev = self.young_trigger_floor.load(Ordering::Relaxed);
        if floor > prev {
            self.young_trigger_floor.store(floor, Ordering::Relaxed);
        }
    }

    /// Returns true when the young generation should be collected.
    pub fn needs_gc(&self) -> bool {
        self.needs_gc_with_jit_allocation_frame(false)
    }

    /// [`old_gen_is_wedged`] for this heap, with `young_capacity` (one
    /// semi-space, as the trigger already read it) standing in for the young
    /// half of `VmHeap::heap_capacity`. Takes the old-generation lock, so the
    /// trigger calls it only in the corner where the verdict matters, at most
    /// once per completed collection. gen r4w4/oom (2026-09-24).
    pub fn old_gen_wedged_against(&self, young_capacity: usize) -> bool {
        let (old_capacity, old_used) = {
            let old = self.old_gen.lock();
            (old.capacity(), old.used())
        };
        old_gen_is_wedged(
            old_capacity.saturating_sub(old_used),
            young_capacity.saturating_add(old_capacity),
        )
    }

    /// [`young_trigger_floor_judged`]'s question, for this heap: can the next
    /// young collection drain young at all? `None` when it can -- the old
    /// generation has room ([`Self::old_gen_wedged_against`] is `false`) AND
    /// the last cycle could promote (gcd d9/c, [`Self::young_drain_blocked`]
    /// is `false`) -- else the highest young occupancy a raised floor may use:
    /// `usize::MAX`, or [`Self::borrowed_young_trigger_cap`] when the old
    /// generation grew into the young budget. One old-generation lock.
    /// gcd d9/b, with gcd d9/c's cross-lane request 1 folded in: a sweep whose
    /// selective promotion a refusing helper window gated off cannot drain
    /// young however much room old gen has, the same corner as wedged (and
    /// `CRATONVM_GC_YOUNG_DRAIN_BLOCKED=0` takes that term out). Counts a
    /// stand-down owed to the blocked drain alone in `young_wedged_census[2]`.
    ///
    /// BUILD DEPENDENCY: `young_drain_blocked` is lane d9/c's (`02847aedd`).
    pub fn young_drain_stood_down_cap(&self, young_capacity: usize) -> Option<usize> {
        let (old_capacity, old_used) = {
            let old = self.old_gen.lock();
            (old.capacity(), old.used())
        };
        let wedged = old_gen_is_wedged(
            old_capacity.saturating_sub(old_used),
            young_capacity.saturating_add(old_capacity),
        );
        if !wedged {
            if !self.young_drain_blocked() {
                return None;
            }
            self.young_wedged_census[2].fetch_add(1, Ordering::Relaxed);
        }
        Some(
            self.borrowed_young_trigger_cap(old_capacity)
                .unwrap_or(usize::MAX),
        )
    }

    /// Count one post-collection floor sample's wedged verdict (gcd d9/b).
    #[inline]
    pub fn note_young_trigger_wedged(&self, verdict: WedgedTriggerVerdict) {
        let slot = match verdict {
            WedgedTriggerVerdict::Ordinary => return,
            WedgedTriggerVerdict::Raised => 0,
            WedgedTriggerVerdict::Deferred => 1,
        };
        self.young_wedged_census[slot].fetch_add(1, Ordering::Relaxed);
    }

    /// `[GC] young_trigger_wedged:` for the shutdown summary: whether the arm
    /// below the cap is on, how many post-collection floor samples a young
    /// generation that could not drain (old generation wedged, or gcd d9/c's
    /// blocked drain) raised to the non-moving trigger / deferred to
    /// allocation failure ([`young_trigger_floor_judged`]), and how many of
    /// the stand-downs were owed to the blocked drain alone. gcd d9/b.
    pub fn young_trigger_wedged_census_line(&self) -> String {
        format!(
            "[GC] young_trigger_wedged: ytw_raise_enabled={} ytw_raised={} ytw_deferred={} \
             ytw_drain_blocked={}",
            self.young_wedged_trigger,
            self.young_wedged_census[0].load(Ordering::Relaxed),
            self.young_wedged_census[1].load(Ordering::Relaxed),
            self.young_wedged_census[2].load(Ordering::Relaxed),
        )
    }

    /// Add `n` to slot `slot` ([`oome_ladder`]) of this heap's
    /// allocation-failure ladder census. Called by the VM's ladder; an
    /// out-of-range slot is ignored. gcd d9/b.
    #[inline]
    pub fn note_oome_ladder(&self, slot: usize, n: u64) {
        if let Some(c) = self.oome_ladder_census.get(slot) {
            c.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// The ladder census, slot by slot ([`oome_ladder`]). gcd d9/b.
    pub fn oome_ladder_census(&self) -> [u64; oome_ladder::LEN] {
        std::array::from_fn(|i| self.oome_ladder_census[i].load(Ordering::Relaxed))
    }

    /// `[GC] oome_ladder:` for the shutdown summary: the allocation-failure
    /// ladder's work per heap `OutOfMemoryError`. gcd d9/b, for
    /// `gcd-d4j-threads-oom-probe-crawls-on-a-full-old-gen`: how many
    /// collections each error cost and on which rung, so a census can tell
    /// the occupancy-trigger crawl (`ladder_threshold_wedged*`) from the
    /// forced tail (`ladder_forced*`) and the deciding majors
    /// (`ladder_majors*`, `ladder_second_majors`, `ladder_debt_payments`).
    pub fn oome_ladder_census_line(&self) -> String {
        use oome_ladder as l;
        let c = self.oome_ladder_census();
        let per_oome = |n: u64| {
            if c[l::OOMES] == 0 {
                0.0
            } else {
                n as f64 / c[l::OOMES] as f64
            }
        };
        format!(
            "[GC] oome_ladder: ladder_oomes={} ladder_forced={} ladder_forced_ms={} \
             ladder_forced_unproductive={} ladder_threshold_wedged={} \
             ladder_threshold_wedged_ms={} ladder_majors={} ladder_majors_ms={} \
             ladder_majors_lost_race={} ladder_deciding={} ladder_second_majors={} \
             ladder_debt_payments={} ladder_futile_young_verdicts={} \
             ladder_futile_young_skips={} ladder_forced_per_oome={:.1} \
             ladder_majors_per_oome={:.1} ladder_threshold_wedged_per_oome={:.1} \
             ladder_latched_grace={} ladder_latched_grace_ok={}",
            c[l::OOMES],
            c[l::FORCED],
            c[l::FORCED_US] / 1000,
            c[l::FORCED_UNPRODUCTIVE],
            c[l::THRESHOLD_WEDGED],
            c[l::THRESHOLD_WEDGED_US] / 1000,
            c[l::MAJORS],
            c[l::MAJORS_US] / 1000,
            c[l::MAJORS_LOST],
            c[l::DECIDING],
            c[l::SECOND_MAJORS],
            c[l::DEBT_PAYMENTS],
            c[l::FUTILE_VERDICTS],
            c[l::FUTILE_SKIPS],
            per_oome(c[l::FORCED]),
            per_oome(c[l::MAJORS]),
            per_oome(c[l::THRESHOLD_WEDGED]),
            c[l::LATCHED_GRACE],
            c[l::LATCHED_GRACE_OK],
        )
    }

    /// The JIT allocation/refill helper calls this while its compiled caller
    /// is still on the native stack.
    ///
    /// gen r5w6/sizer10: this doc used to say that root gathering "will detect
    /// it and select the non-moving young collector". That is no longer
    /// guaranteed. The blanket JIT fallback to the non-moving collector is
    /// opt-in (`CRATONVM_MOVING_YOUNG_NO_JIT`; see
    /// `refresh_moving_young_coverage_for_current_thread` in
    /// `vm/src/jit/conservative_roots.rs`). A collection this trigger starts
    /// from inside a JIT helper may therefore be MOVING, and it is then
    /// correct only if the calling site's oop map covers every live reference
    /// (e.g. a half-constructed object whose field array is being allocated).
    /// The trigger itself is unchanged by this note; see
    /// `gengc-r5w6-sizer10-refused-young-root-dangles-RETIRED-RULING-20261018.md`.
    pub fn needs_gc_for_jit_allocation(&self) -> bool {
        self.needs_gc_with_jit_allocation_frame(true)
    }

    pub fn needs_gc_with_jit_allocation_frame(&self, jit_allocation_frame: bool) -> bool {
        if self.external_gpu.should_trigger_gc() {
            return true;
        }
        // The three numbers, without the lock. See
        // [`Self::young_from_pub_used`] -- this predicate is on the allocation
        // path, and its former `young_from.lock()` was an atomic
        // read-modify-write on a line every allocating thread wants, taken to
        // read three words that only tune a heuristic.
        let (used, free_list_bytes, capacity) = if trigger_lockfree_enabled() {
            if trigger_verify_enabled() {
                // THE LOCK FIRST, THEN THE PUBLISHED READ, and the order is the
                // whole of the oracle's soundness.
                //
                // Reading the triple before acquiring lets a peer thread
                // allocate between the two reads, and the verifier then reports
                // its own race as a publish gap: `published` lags `actual` by
                // exactly one allocation, every time, with `free` and
                // `capacity` agreeing. Measured 2026-09-06 -- 0 divergences in
                // 4550 checks on a single-threaded probe, then 88 in 69922 on
                // multi-threaded H2, which is the signature of a racing reader
                // rather than of a missing publish.
                //
                // Acquired first, nothing can mutate the arena and nothing can
                // republish, so the published triple is the last unlock's state
                // and the arena is that same state. They must be equal, and a
                // divergence is what it claims to be: a mutation that reached
                // the arena without passing through `YoungFromGuard::drop`.
                let from = self.lock_young_from();
                let actual = (from.used(), from.free_list_bytes(), from.capacity());
                let published = self.young_from_published();
                TRIGGER_VERIFY_CHECKS.fetch_add(1, Ordering::Relaxed);
                if actual != published {
                    TRIGGER_VERIFY_DIVERGED.fetch_add(1, Ordering::Relaxed);
                    eprintln!(
                        "[gc-trigger-verify] published=(used={pu} free={pf} cap={pc}) actual=(used={au} free={af} cap={ac}) -- a mutation reached the arena without passing through YoungFromGuard::drop",
                        pu = published.0,
                        pf = published.1,
                        pc = published.2,
                        au = actual.0,
                        af = actual.1,
                        ac = actual.2,
                    );
                }
                actual
            } else {
                self.young_from_published()
            }
        } else {
            let from = self.lock_young_from();
            (from.used(), from.free_list_bytes(), from.capacity())
        };
        // DBG: CRATONVM_DBG_GC_STRESS=<bytes> forces a young GC every <bytes>
        // of allocation, so the non-moving sweep's corruption detection fires
        // right after the corrupting write (the corruptor's interpreted caller
        // is then on the mutator stack dumped by CRATONVM_DBG_CORRUPT_FRAMES).
        //
        // gen r4w2/youngmark (2026-09-23): after a NON-MOVING cycle `used` is
        // the bump high-water mark and never retreats, so `used >= t` stayed
        // true for the rest of the run and every interpreted `new` collected —
        // see `gc_stress_decision`.
        //
        // gen r4w3/young2 (2026-09-23): two more exits from the same storm.
        // (1) The re-arm now applies after a MOVING cycle too (see
        // `gc_stress_decision`: an interpreted `new` right after a 256 KiB TLAB
        // carve saw `used >= t` for any `t` below the carve). (2) While the
        // last attempted cycle REFUSED (`young_cycle_skipped`: to-space
        // undersized, object-start walk incomplete, T-3 tail) nothing about
        // the arena changed, so `used >= floor` would stay true and every
        // allocation would re-enter a pause that refuses again — the stress
        // arm bypassed the refusal floor `note_skipped_young_cycle` arms for
        // exactly this. The stress arm stands down and the normal trigger,
        // with that floor, decides until a cycle completes (the flag is
        // cleared at the top of every `collect_garbage_inner`).
        if let Some(t) =
            gc_stress_threshold().filter(|_| !self.young_cycle_skipped.load(Ordering::Relaxed))
        {
            let gc_count = self.stats.minor_gc_count.load(Ordering::Relaxed);
            let after_non_moving = self.young_stress_epochs[0].load(Ordering::Relaxed) == gc_count;
            let sample_now = self.young_stress_epochs[1].load(Ordering::Relaxed) != gc_count;
            // gen r4w4/oom (2026-09-24): the stress arm stands down in the
            // same wedged corner as the ordinary trigger (see
            // `young_trigger_floor_after_collection`), or a stress run on a
            // heap full of live data spins one cycle per TLAB carve exactly as
            // the unstressed heap used to. The threshold argument is moot for
            // the deferral verdict, so `usize::MAX` keeps the ordinary arm out
            // of it. Sampled once per epoch, like the stress floor it parks.
            if sample_now
                && young_trigger_floor_after_collection(
                    used.saturating_sub(free_list_bytes),
                    usize::MAX,
                    capacity,
                    // gcd d9/c: young cannot drain either (see `young_drain_blocked`).
                    || self.young_drain_blocked() || self.old_gen_wedged_against(capacity),
                ) == YOUNG_TRIGGER_DEFERRED
            {
                self.young_stress_epochs[1].store(gc_count, Ordering::Relaxed);
                self.young_stress_floor
                    .store(YOUNG_TRIGGER_DEFERRED, Ordering::Relaxed);
                return false;
            }
            let floor = self.young_stress_floor.load(Ordering::Relaxed);
            let (fires, sampled) = gc_stress_decision(
                used,
                free_list_bytes,
                t,
                after_non_moving,
                sample_now,
                floor,
            );
            if let Some(floor) = sampled {
                // Plain stores, as for `young_trigger_floor` below: racing
                // threads compute the same floor from the same arena.
                self.young_stress_epochs[1].store(gc_count, Ordering::Relaxed);
                self.young_stress_floor.store(floor, Ordering::Relaxed);
                // gen r4w6/tlab6: the carve count at the same epoch, for
                // `gc_stress_carve_bound`.
                self.young_stress_net_carved_at_epoch
                    .store(self.young_stress_net_carved(), Ordering::Relaxed);
            }
            // gen r4w6/tlab6: a verdict the carves alone produced stands only
            // past the carve allowance — see `gc_stress_carve_bound`. Two more
            // relaxed loads, and only once the per-epoch verdict has fired.
            if fires {
                let metric = if after_non_moving {
                    used.saturating_sub(free_list_bytes)
                } else {
                    used
                };
                let carved_since = self.young_stress_net_carved().saturating_sub(
                    self.young_stress_net_carved_at_epoch
                        .load(Ordering::Relaxed),
                );
                return gc_stress_carve_bound(fires, metric, floor, t, carved_since);
            }
            return fires;
        }
        // Trigger on LIVE occupancy, not the raw bump cursor. The non-moving,
        // JIT-frame-safe sweep (`sweep_young_non_moving`) reclaims dead objects
        // into the arena's free list WITHOUT retreating the cursor — it cannot
        // relocate survivors while conservative JIT roots are live — so `used()`
        // (== cursor, the high-water mark) stays pinned near capacity after such
        // a sweep even though most of that span is reusable free-list space that
        // `Arena::alloc` hands straight back out. Keying the trigger off the raw
        // cursor leaves `needs_gc` permanently true once the high-water mark
        // passes the threshold, so a young GC fires on essentially every
        // subsequent allocation: the bintrees18 / AllocLoop thrash (live set
        // fills young, ~150 no-progress sweeps reclaiming nothing, rc=124
        // timeout — `CRATONVM_DBG_SWEEP_EDGES` shows unmarked=0, edges=0 each
        // sweep). Subtracting the free-list bytes makes the metric reflect
        // genuinely-occupied space; the moving collector resets the cursor
        // itself, so this is a no-op there. The alloc-failure→GC-and-retry path
        // remains the hard backstop against fragmentation under-collection.
        let live = used.saturating_sub(free_list_bytes);
        // Cheney copying needs the unused half as worst-case survivor
        // headroom. The opt-out non-moving collector instead sweeps in place,
        // so it can safely use the active semi-space almost to capacity.
        // Only select the larger threshold when the next normal young cycle is
        // guaranteed to take that path. In particular, `--nojit` collections
        // remain moving even though `CRATONVM_MOVING_YOUNG` is unset.
        let non_moving_young = next_young_gc_is_guaranteed_non_moving(
            cratonvm_gc_core::gc_quiescence::is_active(),
            cratonvm_gc_core::gc_quiescence::unregistered_jit_frame_on_stack(),
            jit_allocation_frame,
            cratonvm_gc_core::gc_quiescence::moving_young_enabled(),
            gc_flags().dbg_force_moving,
        );
        // gch h2/ob: the gcg h1/ob sweep cap under the Serial-shaped trigger
        // is gone (vh1-ob `ob1_se_ofin_se_2`: a run whose cycles were 25 of 26
        // sweeps fired them at the default arm's 50 % and finalized 0/64, the
        // default's answer). Armed, a sweep fires at 90 % of the semi again,
        // HotSpot Serial's young cadence (`gcf-f5c-...`).
        let threshold = young_gc_trigger_bytes(
            capacity,
            self.young_gc_threshold.load(Ordering::Relaxed),
            non_moving_young,
            self.young_pause_goal_ms > 0, // gci i4/yg: per heap
        );
        // gcj yg3-residuals R3: under the Serial geometry every cycle, moving
        // or not, fires at eden on top of what the last cycle left young (the
        // pinned copy's survivor reserve below still applies).
        let threshold = match self.young_geometry {
            Some(g) => {
                let gc_count = self.stats.minor_gc_count.load(Ordering::Relaxed);
                if self.young_geometry_seen_gc.load(Ordering::Relaxed) != gc_count {
                    self.young_geometry_seen_gc
                        .store(gc_count, Ordering::Relaxed);
                    self.young_geometry_carry.store(live, Ordering::Relaxed);
                }
                geometry_young_trigger(
                    self.young_gc_threshold.load(Ordering::Relaxed),
                    self.young_geometry_carry.load(Ordering::Relaxed),
                    g.survivor,
                    g.semi_reserve.min(capacity),
                )
            }
            None => threshold,
        };
        // gcj j1/yg (`CRATONVM_GEN_PINNED_COPY_SURVIVOR_RESERVE`): a JIT-warm
        // cycle (the predicted non-moving one, which the pinned copy takes
        // when it can) leaves its survivor room free.
        let threshold = if non_moving_young && self.pinned_copy_survivor_reserve {
            threshold.min(capacity.saturating_sub(pinned_survivor_reserve(
                capacity,
                self.young_serial_survivor,
                self.pinned_survivor_estimate.load(Ordering::Relaxed),
            )))
        } else {
            threshold
        };
        young_trigger_debug(used, free_list_bytes, live, threshold, non_moving_young);
        // Anti-livelock floor. Sample `live` once per completed collection (the
        // first `needs_gc` after `minor_gc_count` moves): if a collection just
        // ran and left the live set AT OR ABOVE the trigger, collecting again
        // immediately cannot help — the non-moving sweep has no way to move that
        // data out of young — so require a further 1/16 of capacity of genuine
        // growth first. The floor is capped at the non-moving trigger
        // (`NON_MOVING_YOUNG_GC_THRESHOLD_PERCENT`), so collection is deferred,
        // never abandoned, and allocation failure remains the hard backstop
        // (`alloc_object` spills to old gen, then GCs and retries).
        let gc_count = self.stats.minor_gc_count.load(Ordering::Relaxed);
        // Plain load/store rather than a `swap`: this runs on the allocation
        // trigger path, and an unconditional read-modify-write on a shared
        // cacheline is exactly what that path should not pay. Two threads
        // racing here both compute the same floor from the same arena, so the
        // lost update is harmless.
        //
        // gen r4w4/oom (2026-09-24): when the floor's cap bites AND the old
        // generation cannot absorb anything, the floor is DEFERRED instead —
        // see `young_trigger_floor_after_collection`. The old-gen lock is taken
        // only in that corner, and at most once per completed collection.
        //
        // gcd d9/b: and below the cap, when the level the ordinary rule would
        // fire at is under the non-moving trigger (every sample on the default
        // moving 50 % trigger), the floor rises to that trigger while the old
        // generation is wedged -- see `young_trigger_floor_judged`. Still one
        // old-gen lock at most, once per completed collection.
        // `CRATONVM_GEN_WEDGED_YOUNG_TRIGGER=0` restores the rule above.
        //
        // gcd d9/c (cross-lane request 1): "wedged" here includes a last cycle
        // that could not promote (`young_drain_blocked`: a refusing helper
        // window gated the sweep's selective promotion off), whatever room old
        // gen has -- in both arms, the deferral at the cap and the raise below
        // it. `CRATONVM_GC_YOUNG_DRAIN_BLOCKED=0` takes that term out.
        if self.young_trigger_seen_gc_count.load(Ordering::Relaxed) != gc_count {
            self.young_trigger_seen_gc_count
                .store(gc_count, Ordering::Relaxed);
            let (floor, wedged) = young_trigger_floor_judged(
                live,
                threshold,
                capacity,
                self.young_wedged_trigger,
                || self.young_drain_stood_down_cap(capacity),
            );
            self.young_trigger_floor.store(floor, Ordering::Relaxed);
            // Two racing samplers may both count one epoch (same lost-update
            // tolerance as the floor store itself).
            self.note_young_trigger_wedged(wedged);
        }
        live >= threshold.max(self.young_trigger_floor.load(Ordering::Relaxed))
    }

    /// Total bytes currently allocated across young and old generations.
    pub fn allocated_bytes(&self) -> usize {
        self.lock_young_from().used() + self.old_gen.lock().used()
    }

    /// Live-bytes estimate for GC-productivity accounting: like
    /// [`allocated_bytes`], but young counts `used - free_list_bytes`
    /// instead of the raw bump cursor. The non-moving young sweep reclaims
    /// dead objects into the from-space free list WITHOUT retreating the
    /// cursor, so the raw `used()` stays pinned at its high-water mark
    /// forever after young first fills — measured that way, every sweep
    /// looks like it freed 0 bytes and the GC-overhead limit falsely
    /// declares a perfectly-productive collector "thrashing" (then stops
    /// collecting, wedging the heap into the old-gen-spill → abort path).
    /// Same live metric `needs_gc` already uses for its trigger.
    pub fn live_bytes_estimate(&self) -> usize {
        let from = self.lock_young_from();
        let young_live = from.used().saturating_sub(from.free_list_bytes());
        drop(from);
        young_live + self.old_gen.lock().used()
    }

    /// `(used, free-list bytes, largest free block, capacity)` for the young
    /// from-space — the numbers behind [`live_bytes_estimate`]'s young term,
    /// unaggregated. See [`cratonvm_gc_core::gpu_critical::VmHeap::young_occupancy`] for why
    /// the aggregate alone cannot answer the question it gets asked.
    pub fn young_from_occupancy(&self) -> (usize, usize, usize, usize) {
        let from = self.lock_young_from();
        (
            from.used(),
            from.free_list_bytes(),
            from.largest_free_block(),
            from.capacity(),
        )
    }

    /// Publish the denominators [`cratonvm_gc_core::gc_metrics::gc_metrics_report`]
    /// normalizes by: objects allocated since startup, bytes allocated, and the
    /// current live-byte estimate.
    ///
    /// gen r4/plumbing (2026-09-23): the first two are not what their names
    /// say on this backend. "Objects allocated" is `young_allocations +
    /// old_allocations`, whose young half is SLOW-path allocations only (TLAB
    /// bump allocations never reach `HeapStats`), and "bytes allocated" is
    /// [`Self::allocated_bytes`] — current occupancy, not a cumulative total.
    /// The honest cumulative figures live in the VM (`bytes_allocated_total`);
    /// see `docs/feature-designs/generative-gc-architecture.md`.
    ///
    /// Called at the top of every collection (so a report taken at any later
    /// point divides by numbers from the most recent pause) and again by the
    /// end-of-run GC summary. Takes the young and old locks, so it must be
    /// called with neither held — the top of `collect_garbage_inner` is before
    /// either is acquired.
    ///
    /// gen r5w5/sizer9 (step 2 of that page): the occupancy is no longer
    /// published as "bytes allocated" — the `allocated_bytes` gauge keeps
    /// whatever it held, as G1's publisher already does. The honest cumulative
    /// figure is the VM's `allocated_bytes_total`
    /// (`gc_metrics::record_vm_allocation_totals`), which the `[GC]
    /// cards/alloc:` ratios now divide by; the object count stays (rendered
    /// as `slow_path_objects=`, which is what it counts here).
    pub fn publish_gc_metrics_occupancy(&self) {
        let s = self.stats.snapshot();
        let kept_allocated_bytes = cratonvm_gc_core::gc_metrics::gc_metrics_raw().allocated_bytes;
        cratonvm_gc_core::gc_metrics::record_heap_occupancy(
            s.young_allocations.saturating_add(s.old_allocations),
            kept_allocated_bytes,
            self.live_bytes_estimate() as u64,
        );
    }

    /// DBG (bc math-ec `0x4`): scan the young from-space for the FIRST object
    /// reference field (or ref-array element) holding `Object(Some(p))` with
    /// `0 < p < 0x1000` — the `0x4` corruption signature, which we proved is
    /// written to a YOUNG object by the mutator (between GCs), NOT by the GC.
    /// Returns `(holder_addr, class_id, field_idx, payload, nbr_disc)` where
    /// `nbr_disc` is the discriminant word of the NEXT cell (field_idx+1) — for
    /// a misaligned-by-8 `Value::Object` write the seed cell reads payload `4`
    /// AND the next cell's disc reads the stray write's real (large) payload,
    /// confirming the misalignment mechanism. `field_idx` has bit 0x4000_0000
    /// set for a ref-array element. Locks `young_from`; call only OUTSIDE a GC.
    pub fn dbg_first_young_small_ref(&self) -> Option<(usize, u32, usize, usize, u64)> {
        let from = self.lock_young_from();
        let base = from.base_ptr();
        let used = from.used();
        let base_a = base as usize;
        let end_a = base_a + used;
        // HEADER-INDEPENDENT brute-force: the corruption scribbles garbage into
        // young object HEADERS too, so any object-walk (even re-syncing) can be
        // desynced past the victim. Instead scan every 8-aligned word for the
        // raw 16-byte `Object(Some(0<p<0x1000))` bit pattern (disc word == 4,
        // payload word in (0,0x1000)), then BACK-VALIDATE the hit sits at a real
        // field-cell offset of a plausible non-array object — which filters out
        // primitive-array `{4, small}` data (the dominant young bytes in EC).
        let mut a = base_a;
        while a + 16 <= end_a {
            // SAFETY: `a` is an 8-aligned address with `a + 16 <= end_a`, so this
            // word and the following one are fully inside the live young-gen arena.
            let disc = unsafe { std::ptr::read(a as *const u64) };
            if disc == 4 {
                // SAFETY: `a + 8` is in-bounds (loop guard ensures `a + 16 <= end_a`).
                let payload = unsafe { std::ptr::read((a + 8) as *const u64) };
                // Precise signature: the corruption payload is ALWAYS exactly 4
                // (== the Value::Object discriminant landing on a field payload).
                // Requiring ==4 (not just <0x1000) rejects transient
                // primitive-array `{4, n}` data (e.g. the startup 0x2c false
                // positive) and other small-but-not-4 values.
                if payload == 4 {
                    // Back-validate: for each candidate field index `fld`, the
                    // owning header would start at H = a - HEADER_SIZE - fld*16.
                    // Accept the first H that is in-arena, kind=Object,
                    // array_length==0, and num_slots in (fld, 1<<20].
                    let mut found: Option<(usize, u32, usize)> = None;
                    let max_fld = 256usize;
                    for fld in 0..max_fld {
                        let off = HEADER_SIZE + fld * SLOT_SIZE;
                        if a < base_a + off {
                            break;
                        }
                        let h_addr = a - off;
                        // SAFETY: `h_addr = a - off` is `>= base_a` (checked above) and
                        // `< a < end_a`, so a full `ObjectHeader` lies within the arena;
                        // fields are read defensively before trusting the contents.
                        let h = unsafe { &*(h_addr as *const ObjectHeader) };
                        if ObjectHeader::kind_tag(h.mark_word.load(Ordering::Relaxed)) == 0
                            && h.array_length() == 0
                            && (h.num_slots() as usize) > fld
                            && h.num_slots() <= (1 << 20)
                        {
                            // Filter: the candidate header's class must RESOLVE
                            // to a real class AND the corrupted field index `fld`
                            // must be a VALID field of that class (`fld < n`).
                            // This accepts a real victim whose num_slots differs
                            // from the resolved count (synthetic-vs-real layout,
                            // e.g. ECFieldElement$F2m) while rejecting bogus
                            // headers read out of neighbour bytes — notably
                            // cid=0 (java/lang/Object, 0 fields) which `is_some`
                            // alone wrongly accepted at fld[22].
                            let cid = h.class_id.as_u32();
                            let ok = cratonvm_gc_core::gc::resolve_class_info(cid)
                                .map(|(_, n)| fld < n)
                                .unwrap_or(false);
                            if ok {
                                found = Some((h_addr, cid, fld));
                                break;
                            }
                        }
                    }
                    if let Some((h_addr, cid, fld)) = found {
                        // The neighbour cell's first word is `[a + 16, a + 24)`,
                        // which the loop guard (`a + 16 <= end_a`) does NOT
                        // cover: on a hit in the last cell below the frontier
                        // the read went 8 bytes past `used`, possibly into an
                        // uncommitted granule (gen r4/plumbing, 2026-09-23).
                        // Report 0 for a neighbour that is not there.
                        let nbr = if a + 24 <= end_a {
                            // SAFETY: `a + 24 <= end_a`, so the 8-byte read at
                            // `a + 16` lies inside the allocated from-space prefix.
                            unsafe { std::ptr::read((a + 16) as *const u64) }
                        } else {
                            0
                        };
                        return Some((h_addr, cid, fld, payload as usize, nbr));
                    }
                }
            }
            a += 8;
        }
        None
    }

    /// Check if the old generation is above 75% capacity.
    ///
    /// gen r4w3/oldgen3 (2026-09-23): this was a third hand-written copy of
    /// the 75 % test (wave-2 `oldgen2` cross-lane request). It now asks the
    /// same policy the STW trigger does — `OldGen::major_trigger_verdict`, via
    /// the NON-counting `major_trigger_peek`, because this is polled far more
    /// often than a collection is decided (the concurrent cycle's trigger, and
    /// two allocation-path GC gates in the VM).
    ///
    /// Default (`CRATONVM_GC_OLD_TRIGGER_HYSTERESIS` off): `true` in exactly
    /// the cases the old expression was — the verdict's OFF arm is the bare
    /// occupancy test (`old_gen::tests::the_trigger_with_hysteresis_off_is_the_bare_occupancy_test`),
    /// and the one input on which the two differed, a ZERO-capacity old
    /// generation (`0 >= 0 * 75 / 100` is `true`; the verdict says `NotDue`),
    /// keeps the old answer explicitly. With the flag, the concurrent cycle
    /// and the VM's gates are held back by the same hysteresis as the STW
    /// trigger instead of re-opening a cycle the policy just declined.
    ///
    /// gen r4w4/concmark4 (2026-09-24): "would the STW old-gen trigger run
    /// now?" under the heap's old-gen policy (`OldGenPolicy`). `Legacy`
    /// (`CRATONVM_GC_NO_CONCURRENT_FIRST`): exactly the answer above.
    /// `ConcurrentFirst`: the policy's effective hysteresis, and `false` while
    /// an open concurrent cycle is one the STW collection would defer to — so
    /// the VM's spill-pressure gates (`vm_exec.rs`, `jit/helpers.rs`) agree
    /// with `major_trigger_decision`. It is no longer the concurrent cycle's
    /// trigger: that is [`Self::concurrent_cycle_due`].
    pub fn old_gen_needs_gc(&self) -> bool {
        let og = self.old_gen.lock();
        match OldGenPolicy::current() {
            OldGenPolicy::Legacy => {
                og.capacity() == 0
                    || og
                        .major_trigger_peek(false, gc_flags().old_trigger_hysteresis)
                        .runs()
            }
            OldGenPolicy::ConcurrentFirst => {
                let hysteresis = OldGenPolicy::ConcurrentFirst
                    .hysteresis(gc_flags().old_trigger_hysteresis_explicit);
                let verdict = og.major_trigger_peek(false, hysteresis);
                // The zero-capacity quirk above is kept on this arm too.
                og.capacity() == 0
                    || (verdict.runs() && !self.stw_defers_to_open_cycle(&og, verdict))
            }
        }
    }

    /// gen r4w4/concmark4 — with the concurrent-first policy, does a STW
    /// old-gen collection with this `verdict` defer to a concurrent cycle that
    /// is open right now? See `ConcurrentStartPolicy::stw_defers`.
    ///
    /// gcd d9/e: with `CRATONVM_GEN_CONC_PRECEDENCE` the ceiling is gone (the
    /// same predicate `major_trigger_decision_policy` uses, so the VM's spill
    /// gates keep agreeing with the pause).
    pub fn stw_defers_to_open_cycle(&self, og: &OldGen, verdict: MajorTrigger) -> bool {
        self.conc_cycle_state
            .as_deref()
            .is_some_and(ConcurrentGcState::cycle_in_progress)
            && ConcurrentStartPolicy::stw_defers_with(
                og.capacity(),
                og.used(),
                verdict,
                og.alloc_failures_since_last_collection(),
                crate::concurrent_mark::gen_conc_precedence_enabled(),
            )
    }

    /// gen r4w4/concmark4 (2026-09-24) — THE concurrent cycle's trigger: should
    /// the driver (`maybe_concurrent_gc_at`, after a young collection) open a
    /// concurrent old-gen cycle now?
    ///
    /// * `Legacy`: exactly the pre-2026-09-24 trigger, which was
    ///   [`Self::old_gen_needs_gc`] (the STW trigger's peek, 75 % floor).
    /// * `ConcurrentFirst`: the adaptive initiating occupancy
    ///   (`ConcurrentGcState::concurrent_start_due`), below the STW floor, so
    ///   the cycle normally runs before the STW collection is due. `false`
    ///   before `enable_concurrent_gc` (no driver can run then either).
    ///
    /// Takes the old-gen lock and then (leaf) the policy lock.
    ///
    /// gen r4w5/concmark5 (2026-09-24): with a concurrent-cycle SERVICE thread
    /// attached (`CRATONVM_GEN_CONC_SERVICE_THREAD`, `ConcurrentGcState::serve`),
    /// a due cycle asked about on any OTHER thread is handed to the service
    /// (`ConcurrentGcState::hand_off_to_service`) and this answers `false`, so
    /// the caller — a mutator in its young-collection epilogue, or the
    /// `System.gc()` door — goes back to work instead of running Phase 2 and
    /// the sweep inline. On the service thread itself, and with no service
    /// attached (the default), the answer is the policy's verdict, unchanged.
    pub fn concurrent_cycle_due(&self) -> bool {
        let due = self.concurrent_cycle_due_verdict();
        if due
            && self
                .conc_cycle_state
                .as_deref()
                .is_some_and(ConcurrentGcState::hand_off_to_service)
        {
            return false;
        }
        due
    }

    /// The concurrent start trigger's verdict (see [`Self::concurrent_cycle_due`]),
    /// without the service hand-off.
    pub fn concurrent_cycle_due_verdict(&self) -> bool {
        match OldGenPolicy::current() {
            OldGenPolicy::Legacy => {
                let due = self.old_gen_needs_gc();
                // gen r4w6/concsvc6: the trigger-to-start census on this arm
                // too (the concurrent-first arm records inside
                // `ConcurrentGcState::concurrent_start_due`).
                if let Some(state) = self.conc_cycle_state.as_deref() {
                    state.note_start_verdict(due);
                }
                due
            }
            OldGenPolicy::ConcurrentFirst => {
                let Some(state) = self.conc_cycle_state.as_deref() else {
                    return false;
                };
                let og = self.old_gen.lock();
                state.concurrent_start_due(
                    og.capacity(),
                    og.used(),
                    og.used_after_last_collection(),
                ) || self.shrink_cycle_due(&og)
            }
        }
    }

    /// gen r4w6 (orchestrator) -- the PERIODIC concurrent cycle, under
    /// `CRATONVM_GC_OLD_SHRINK_AFTER_CONCURRENT` only. The resize that flag
    /// enables runs after a concurrent cycle's sweep, but after a program
    /// drops most of its old data nothing opens a cycle: `used` still counts
    /// the dead bytes until a collection finds them, so occupancy never
    /// crosses the start threshold again, and the committed generation stayed
    /// at its peak (`GenR4W6OldShrinkProbe`: 2 of 8 runs shrank, by the luck
    /// of a cycle ending after the drop). HotSpot G1 answers the same shape
    /// with `G1PeriodicGCInterval` (a time interval; off by default there too).
    /// Here, counted in young pauses: due once [`SHRINK_CYCLE_PERIOD`] young
    /// collections have run with no old-gen collection of either kind, and the
    /// generation holds at least [`SHRINK_CYCLE_MIN_USED`]. Opening one
    /// restarts the count, so an idle old generation costs one concurrent
    /// cycle per period. Old-gen lock held by the caller.
    pub fn shrink_cycle_due(&self, og: &OldGen) -> bool {
        if !gc_flags().old_shrink_after_concurrent || !gc_flags().old_shrink {
            return false;
        }
        let collections = og.trigger_stats().collections;
        let minor = self.stats.minor_gc_count.load(Ordering::Relaxed);
        let seen = self.shrink_cycle_clock[0].swap(collections, Ordering::Relaxed);
        if seen != collections {
            // An old-gen collection ran since the last look: restart.
            self.shrink_cycle_clock[1].store(minor, Ordering::Relaxed);
            return false;
        }
        let since = self.shrink_cycle_clock[1].load(Ordering::Relaxed);
        if !periodic_shrink_cycle_due(minor.saturating_sub(since), og.used()) {
            return false;
        }
        // gen r5w5/conc9 (cross-lane, applied by the orchestrator): the clock
        // no longer restarts here, so an ask that loses the cycle to a caller
        // who then abandons it does not consume the verdict; the restart comes
        // from the completed collection bumping `collections` (above).
        true
    }

    /// gen r4w4/concmark4 — the shutdown `[GC] conc_policy:` line (printed by
    /// `VmHeap::print_gc_summary`): the policy, the start threshold now, the
    /// predicted per-cycle growth, and the cycle census — including the
    /// initial-mark takeover numbers wave 3 asked `obs` to print. `None`
    /// before `enable_concurrent_gc`. Keys are `concpol_`-prefixed.
    pub fn concurrent_policy_census_line(&self) -> Option<String> {
        let state = self.conc_cycle_state.as_deref()?;
        let (capacity, used) = {
            let og = self.old_gen.lock();
            (og.capacity(), og.used())
        };
        let policy = OldGenPolicy::current();
        let threshold = state.concurrent_start_threshold(capacity);
        let (growth, duration_us) = state.policy_estimates();
        let c = state.census();
        Some(format!(
            "[GC] conc_policy: concpol_policy={} concpol_start_threshold={threshold} \
             concpol_start_pct={} concpol_old_used={used} concpol_old_capacity={capacity} \
             concpol_predicted_growth={} concpol_cycle_us_ewma={} \
             concpol_cycles_completed={} concpol_stw_deferred={} concpol_stw_preempted={} \
             concpol_initial_marks={} concpol_initial_marks_with_takeover={} \
             concpol_frozen_threads={} concpol_frozen_objects_scanned={} \
             concpol_sweep_slices={} concpol_sweep_epoch_stops={} \
             concpol_overflow_rescan_passes={} concpol_satb_drained_in_mark={} \
             concpol_mark_local_spills={}",
            policy.name(),
            threshold * 100 / capacity.max(1),
            growth.map_or(-1, |g| i64::try_from(g).unwrap_or(i64::MAX)),
            duration_us.map_or(-1, |d| i64::try_from(d).unwrap_or(i64::MAX)),
            c.cycles_completed,
            c.stw_deferred,
            c.stw_preempted,
            c.initial_marks,
            c.initial_marks_with_takeover,
            c.frozen_threads,
            c.frozen_objects_scanned,
            c.sweep_slices,
            c.sweep_epoch_stops,
            c.overflow_rescan_passes,
            c.satb_drained_in_mark,
            c.mark_local_spills,
        ))
    }

    /// gcf f4/r (`gengc-r5w3-obs7-proposal-usage-threshold-on-old-generation-growth`):
    /// the `Tenured Gen` usage-threshold check for a direct old-generation
    /// allocation. The sample is the pool `jmx_memory_pools` reports, built from
    /// `og` alone (the caller holds the old-generation lock; the threshold
    /// table's own mutex is a leaf under it). The request is delivered by the
    /// VM's allocation slow path (`maybe_gc`), right after the allocation.
    #[inline]
    pub fn check_tenured_usage_on_growth(&self, og: &OldGen) {
        use cratonvm_gc_core::gc_metrics::{serial_pool_managers, JmxPoolUsage, JMX_POOL_TENURED};
        let thresholds = self.gc_notifications.pool_thresholds();
        if !thresholds.is_armed() {
            return;
        }
        // Widening: usize to u64 on every supported target.
        let b = |n: usize| n as u64;
        let used = og.used();
        let cap = og.capacity();
        let init = usize::try_from(og.sizing_stats().commit_floor).unwrap_or(usize::MAX);
        let sample = JmxPoolUsage {
            name: JMX_POOL_TENURED,
            init: Some(b(init.min(cap))),
            used: b(used),
            committed: b(reported_old_committed(og).max(used)),
            max: Some(b(cap)),
            collection_used: None,
            managers: serial_pool_managers(JMX_POOL_TENURED),
        };
        let _ = thresholds.check_usage_on_growth(&sample);
    }

    /// gen r4w5/concmark5 (2026-09-24) — THE CONCURRENT-CYCLE POLL SITE ON THE
    /// DIRECT OLD-GEN ALLOCATION PATH
    /// (`gengc-r4w4-concmark4-the-concurrent-cycle-is-polled-only-after-a-young-collection`).
    ///
    /// Called with the old-gen lock held, right after a humongous array or a
    /// young-spill object was placed in the old generation. That growth comes
    /// with no young collection, so no epilogue would ever ask
    /// [`Self::concurrent_cycle_due`] about it; with a service thread attached
    /// this wakes it when the generation has crossed the start threshold. Cost
    /// when nothing is attached: one relaxed load and a branch. With a service:
    /// three relaxed loads until `used` reaches the published threshold, then
    /// one policy-lock check per allocation until a request is pending.
    ///
    /// Concurrent-first only: the legacy trigger's verdict is
    /// `old_gen_needs_gc`, which takes this same lock; under the legacy policy
    /// the service's periodic check covers direct growth instead.
    ///
    /// Lock order: old-gen → policy (leaf) → service slot (leaf).
    #[inline]
    pub fn poll_concurrent_start_after_direct_old_alloc(&self, og: &OldGen) {
        // gcf f4/r: the old generation just grew outside a collection; fire
        // `Tenured Gen`'s usage threshold now, as HotSpot's low-memory detector
        // does after a slow-path allocation. One load when none is set.
        self.check_tenured_usage_on_growth(og);
        let Some(state) = self.conc_cycle_state.as_deref() else {
            return;
        };
        // gce e1/c (`gengc-r4w4-concmark4-...-polled-only-after-a-young-collection`,
        // item 1): with NO service attached, latch a start request that the
        // VM's allocation slow path runs inline
        // (`ConcurrentGcState::take_inline_start_request`). Opt-in
        // (`CRATONVM_GEN_CONC_INLINE_START`); off, this is the previous
        // early return. gcf f1/c: the flag is read once per VM
        // (`ConcurrentGcState::inline_start_enabled`); it used to be an env
        // lookup on every direct allocation above the start hint, under this
        // lock, with the flag off.
        if state.inline_growth_poll_armed(og.used()) {
            if state.inline_start_enabled()
                && OldGenPolicy::current() == OldGenPolicy::ConcurrentFirst
            {
                if state.concurrent_start_due(
                    og.capacity(),
                    og.used(),
                    og.used_after_last_collection(),
                ) {
                    let _ = state.latch_inline_start_request();
                }
            }
            return;
        }
        if !state.service_growth_poll_armed(og.used()) {
            return;
        }
        if OldGenPolicy::current() != OldGenPolicy::ConcurrentFirst {
            return;
        }
        if state.concurrent_start_due(og.capacity(), og.used(), og.used_after_last_collection()) {
            state.signal_old_gen_growth();
        }
    }

    /// gen r4w5/concmark5 — the shutdown `[GC] conc_driver:` line (the service
    /// thread and the remark hook; `ConcurrentGcState::driver_census_line`).
    /// `None` before `enable_concurrent_gc`.
    pub fn concurrent_driver_census_line(&self) -> Option<String> {
        self.conc_cycle_state
            .as_deref()
            .map(ConcurrentGcState::driver_census_line)
    }

    /// gen r4w5/concmark5 — the shutdown `[GC] major_cadence:` line: STW
    /// old-gen collections per 100 young decisions, back-to-back majors, and
    /// majors that freed under 5 % of the generation (see
    /// `concurrent_mark::MajorCadenceCensus`). Count-based, so it answers the
    /// hysteresis page's question without a wall-clock A/B. `None` before
    /// `enable_concurrent_gc`.
    pub fn major_cadence_census_line(&self) -> Option<String> {
        let state = self.conc_cycle_state.as_deref()?;
        let census = {
            let og = self.old_gen.lock();
            let t = og.trigger_stats();
            state.major_cadence(
                t.collections.saturating_sub(t.concurrent_collections),
                t.freed_bytes,
            )
        };
        Some(census.line())    }

    /// Enable/disable the per-collection `[GC] generational: …` line.
    ///
    /// This is what `--verbose:gc` turns on for this backend.
    /// `VmHeap::enable_gc_logging`'s Generational arm calls it, exactly as its
    /// G1 and ZGC neighbours call `G1Collector::enable_gc_logging` /
    /// `ZgcRealHeap::enable_gc_logging`. See [`Self::gc_log_enabled`] for why
    /// the state is per-heap rather than a process global, and
    /// `docs/feature-designs/generative-gc-architecture.md`
    /// for the defect it closes.
    ///
    /// Takes `&self`, not `&mut self`, because `VmHeap::enable_gc_logging`
    /// holds `&self` — so an arming call must not need a unique borrow. (An
    /// earlier draft of this comment justified it by saying the `VmHeap` arm
    /// holds the heap behind an `Arc` "like its neighbours do"; only the ZGC
    /// arm does. The signature is right, that reason was not.)
    pub fn set_verbose_gc(&self, on: bool) {
        self.gc_log_enabled
            .store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// Is the per-collection `--verbose:gc` line armed? See
    /// [`Self::set_verbose_gc`].
    pub fn verbose_gc_enabled(&self) -> bool {
        self.gc_log_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Accumulated pause accounting over this heap's completed collections:
    /// `(collections, total_pause_us, total_pause_ms_ceiled)`.
    ///
    /// `total_pause_ms_ceiled` is each pause rounded UP to a whole millisecond,
    /// then summed. Since gen r5w3/obs7 it is only the single-bean fallback's
    /// figure (`VmHeap::collection_time_ms`); the Serial beans'
    /// `GarbageCollectorMXBean.getCollectionTime()` is the floor of the
    /// microsecond sum, HotSpot's arithmetic (`gc_metrics::serial_collectors`,
    /// `gengc-r5w2-obs6-collection-time-ceils-every-pause`). See
    /// [`Self::pause_totals`] (the field) for why a refused cycle is excluded.
    pub fn gc_pause_totals(&self) -> (u64, u64, u64) {
        (
            self.pause_totals[0].load(Ordering::Relaxed),
            self.pause_totals[1].load(Ordering::Relaxed),
            self.pause_totals[2].load(Ordering::Relaxed),
        )
    }

    /// The part of [`Self::gc_pause_totals`] spent in collections that also
    /// reclaimed the old generation, same `(count, us, ms_ceiled)` shape.
    /// gen r4w2/obs; see the `old_pause_totals` field.
    pub fn gc_old_pause_totals(&self) -> (u64, u64, u64) {
        (
            self.old_pause_totals[0].load(Ordering::Relaxed),
            self.old_pause_totals[1].load(Ordering::Relaxed),
            self.old_pause_totals[2].load(Ordering::Relaxed),
        )
    }

    /// The `GarbageCollectorMXBean`s this heap reports: HotSpot Serial's
    /// `Copy` + `MarkSweepCompact`. gen r4w2/obs; see
    /// [`cratonvm_gc_core::gc_metrics::serial_collectors`].
    ///
    /// gcg g6/yg: a young pause that also ran an UNREQUESTED major books its
    /// young half under `Copy` too ([`serial_totals_with_escalations`]):
    /// HotSpot books every young collection that ran under `Copy` (vg4
    /// `yg4_beans_gen_*`: `minor=38 major=22` read `copy=16 msc=22`).
    pub fn jmx_collectors(&self) -> [cratonvm_gc_core::gc_metrics::JmxCollector; 2] {
        let esc = (
            self.escalated_young_totals[0].load(Ordering::Relaxed),
            self.escalated_young_totals[1].load(Ordering::Relaxed),
        );
        let (all, old) =
            serial_totals_with_escalations(self.gc_pause_totals(), self.gc_old_pause_totals(), esc);
        cratonvm_gc_core::gc_metrics::serial_collectors(all, old)
    }

    /// gcg g6/yg: record that this young cycle's copy is done and an
    /// unrequested major starts now. A requested major (`System.gc()`, the
    /// ladder) records nothing: HotSpot books that pause under the full
    /// collector only.
    pub fn note_escalation_mark(&self, major_requested: bool) {
        if !major_requested {
            self.escalation_mark.lock().mark = Some(std::time::Instant::now());
        }
    }

    /// gcg h1/ob: `major_trigger_decision` said an UNREQUESTED major runs;
    /// record the old generation's occupancy at that moment (the young half's
    /// end figure for the `-Xlog` split). Old-gen lock held by the caller; the
    /// escalation lock is a leaf.
    pub fn note_escalation_decision(&self, old_used: usize) {
        self.escalation_mark.lock().decided_old_used = Some(old_used);
    }

    /// gcg g6/yg: take (and clear) this heap's escalation mark, with the
    /// old-generation occupancy its decision recorded (gcg h1/ob). Clears the
    /// whole cycle state, a split nobody took included: the cycle entries call
    /// this to start fresh.
    pub fn take_escalation_mark(&self) -> Option<(std::time::Instant, Option<usize>)> {
        let mut s = self.escalation_mark.lock();
        let decided = s.decided_old_used.take();
        s.booked = None;
        s.mark.take().map(|at| (at, decided))
    }

    /// gcg h1/ob: take (and clear) the young half of the last collection, when
    /// that collection was a young pause that escalated into an unrequested
    /// major and the `Copy` bean booked its young half. The VM takes it once
    /// per collection, inside the pause (`gc_events::gc_event_seal`).
    pub fn take_escalation_split(&self) -> Option<EscalationSplit> {
        self.escalation_mark.lock().booked.take()
    }

    /// The to-space's capacity from the lock-free `region_bounds` mirror, or
    /// under its lock when the mirror is not published yet. gen r4w2/obs
    /// (factored out gen r5w2/obs6).
    pub fn young_to_capacity_published(&self) -> usize {
        let b = self.region_bounds[1].0.load(Ordering::Acquire);
        let e = self.region_bounds[1].1.load(Ordering::Acquire);
        match e.checked_sub(b) {
            Some(span) if b != 0 && span != 0 => span,
            _ => self.young_to.lock().capacity(),
        }
    }

    /// The ceiling a young semi-space may grow to: the young expansion in
    /// `collect_garbage_inner` grows one to `max_young_semi_size` capped by
    /// `(Xmx - old_capacity) / 2`, and never shrinks one below its current
    /// capacity. `None` when neither bound is set (the explicit-size test
    /// constructors). gen r4w2/obs (factored out gen r5w2/obs6).
    pub fn young_semi_max(&self, from_cap: usize, to_cap: usize, old_cap: usize) -> Option<usize> {
        if self.max_heap_bytes == usize::MAX && self.max_young_semi_size == usize::MAX {
            return None;
        }
        let budget = if self.max_heap_bytes == usize::MAX {
            usize::MAX
        } else {
            self.max_heap_bytes.saturating_sub(old_cap) / 2
        };
        Some(
            self.max_young_semi_size
                .min(budget)
                .max(from_cap)
                .max(to_cap),
        )
    }

    /// The heap's USABLE committed size, split `(young, old)` — HotSpot
    /// Serial's `CollectedHeap::capacity()`, i.e. `Runtime.totalMemory()`, the
    /// heap `MemoryUsage.getCommitted()` and the `(NNNM)` of an `-Xlog:gc`
    /// line. gen r5w2/obs6 (2026-09-26;
    /// `gengc-r4w3-hunter-runtime-memory-counts-the-copy-reserve`).
    ///
    /// # The copy reserve is not usable heap
    ///
    /// A copying young generation keeps its to-space empty between
    /// collections: nothing allocates there while mutators run (`refill_tlab`
    /// carves only the from-space). HotSpot's `DefNewGeneration::capacity()`
    /// is `eden + from` for exactly that reason, which is why Serial's
    /// `totalMemory()` / `maxMemory()` are below what it reserved. Until this
    /// wave the Generational answer counted both semi-spaces, so a program
    /// that sized a buffer from `maxMemory() - (totalMemory() - freeMemory())`
    /// was told about a whole semi-space (`-Xmx / 4` at the default split) it
    /// could never allocate.
    ///
    /// * **young** — the committed granules of ONE semi-space: the larger of
    ///   the two arenas' commits, clamped to the from-space capacity. Not the
    ///   from-space's own commit: the two swap roles every moving cycle, and
    ///   a to-space that only ever held survivors has committed far less, so
    ///   the from-space figure alone would drop after every flip and climb
    ///   back as allocation re-commits — a `totalMemory()` that moves with
    ///   the collection count, which `VmHeap::committed_bytes` forbids. The
    ///   larger of the two does not move at a flip, and is what the young
    ///   generation has brought into use for its usable half. It still falls
    ///   when a sweep hands whole free granules back to the OS
    ///   (`Arena::decommit_span`, in the modes that really decommit), as
    ///   HotSpot's `totalMemory()` falls when its heap shrinks.
    /// * **old** — `reported_old_committed`: the old generation's committed
    ///   granules on the reserving store, its touched prefix on the wholly
    ///   committed fallback, never below its used bytes.
    ///
    /// The `-Xms` consequence: the startup commit splits `-Xms` evenly across
    /// the two semi-spaces (`commit_initial_heap`), so the half committed into
    /// the to-space is committed memory but not usable heap, and a fresh
    /// `-Xms64m` heap reports `totalMemory() = 32m` (HotSpot Serial reports
    /// `-Xms` minus one survivor, ~62m; its copy reserve is a tenth of the
    /// young generation where ours is half of it). Since gen r5w4/defaults8
    /// the startup commit is split HotSpot's way by default
    /// (`xms_usable_first_split`; `CRATONVM_GEN_XMS_USABLE_FIRST=0` restores
    /// the even split) and the same heap reports ~64m.
    ///
    /// Takes `young_from`, then `young_to`, then the old-generation lock, one
    /// at a time (never nested). A reporting call: never on an allocation
    /// path, never with a collector lock held.
    pub fn usable_committed_parts(&self) -> (usize, usize) {
        if let Some(pools) = self.serial_pool_usage(None) {
            return (pools[0].committed + pools[1].committed, pools[2].committed);
        }
        let (from_committed, from_cap) = {
            let from = self.lock_young_from();
            (
                from.committed_bytes(),
                self.reported_semi_cap(from.capacity()),
            )
        };
        let to_committed = self.young_to.lock().committed_bytes();
        let young = from_committed.max(to_committed).min(from_cap);
        let old = reported_old_committed(&self.old_gen.lock());
        (young, old)
    }

    /// [`Self::usable_committed_parts`], summed: what `VmHeap::committed_bytes`
    /// answers for this heap. gen r5w2/obs6.
    pub fn usable_committed_bytes(&self) -> usize {
        let (young, old) = self.usable_committed_parts();
        young.saturating_add(old)
    }

    /// HotSpot Serial's `CollectedHeap::max_capacity()` — `Runtime.maxMemory()`
    /// and the heap `MemoryUsage.getMax()`. Fixed Serial reporting uses its
    /// construction-time capacity, excluding one aligned survivor reserve.
    /// The diagnostic legacy model uses the largest one semi-space may
    /// grow to plus the old generation's reservation, i.e. `-Xmx` less the
    /// copy reserve (`3/4 · -Xmx` at the default split). gen r5w2/obs6.
    ///
    /// `None` for a heap built from explicit arena sizes with no `-Xmx`
    /// budget (the test constructors); the VM then reports its configured
    /// `-Xmx`, as before. The Eden and Tenured pools' `max` sum to this value
    /// (the Survivor pool's `max` is undefined), as HotSpot's pools sum to
    /// its `max_capacity()`.
    ///
    /// Two relaxed loads, the to-space mirror and the old-generation lock
    /// once.
    pub fn max_usable_heap_bytes(&self) -> Option<usize> {
        let from_cap = self.reported_semi_cap(self.young_from_published().2);
        let to_cap = self.reported_semi_cap(self.young_to_capacity_published());
        let old_cap = self.old_gen.lock().capacity();
        if self.serial_pool_max_total > 0 {
            return Some(
                self.serial_pool_max_total
                    .max(old_cap.saturating_add(from_cap.max(to_cap))),
            );
        }
        self.young_semi_max(from_cap, to_cap, old_cap)
            .map(|semi| semi.saturating_add(old_cap))
    }

    pub fn serial_pool_maxima_for_reporting(&self) -> Option<[usize; 3]> {
        if !survivor_pool_reserve_enabled()
            || self.serial_pool_max_total == 0
        {
            return None;
        }
        // The fixed Serial projection is valid only while the logical heap
        // maximum still equals the construction-time pool budget. Capacity
        // growth can raise maxMemory above that budget; in that state use the
        // physical-capacity projection below so live bytes and pool maxima
        // describe the same heap.
        if self.max_usable_heap_bytes()? != self.serial_pool_max_total {
            return None;
        }
        let survivor = self
            .max_heap_bytes
            .checked_sub(self.serial_pool_max_total)?;
        let young = self
            .serial_pool_max_total
            .checked_sub(self.serial_tenured_pool_max)?;
        Some([
            young.checked_sub(survivor)?,
            survivor,
            self.serial_tenured_pool_max,
        ])
    }

    pub fn serial_pool_usage(
        &self,
        survivors: Option<usize>,
    ) -> Option<[crate::serial_pool_accounting::PoolUsage; 3]> {
        let maxima = self.serial_pool_maxima_for_reporting()?;
        for _ in 0..3 {
            let (young_live, from_committed, from_cap) = {
                let from = self.lock_young_from();
                (
                    from.used().saturating_sub(from.free_list_bytes()),
                    from.committed_bytes(),
                    from.capacity(),
                )
            };
            let to_committed = self.young_to.lock().committed_bytes();
            let (old_used, old_committed) = {
                let old = self.old_gen.lock();
                (old.used(), reported_old_committed(&old))
            };
            let retained = survivors.unwrap_or_else(|| {
                if self.last_collection_usage[2].load(Ordering::Relaxed) != 0 {
                    self.last_collection_usage[0].load(Ordering::Relaxed)
                } else {
                    0
                }
            });
            if let Some(pools) = crate::serial_pool_accounting::project(
                maxima,
                [young_live, old_used],
                [
                    from_committed.max(to_committed).min(from_cap),
                    old_committed,
                ],
                retained,
                self.serial_survivor_ratio,
                SERIAL_SPACE_ALIGNMENT,
            ) {
                *self.serial_pool_snapshot.lock() = Some(pools);
                return Some(pools);
            }
        }
        // Independent arena reads can straddle a collection. Never discard
        // live bytes to force that mixed observation into the fixed maxima.
        // Retain a previously coherent estimate, or the empty heap's birth
        // state if the first query itself raced. No arena lock is held here.
        self.serial_pool_snapshot_fallbacks
            .fetch_add(1, Ordering::Relaxed);
        let mut cached = self.serial_pool_snapshot.lock();
        if let Some(pools) = *cached {
            if pools.map(|p| p.max) == maxima {
                return Some(pools);
            }
        }
        let birth = crate::serial_pool_accounting::project(
            maxima,
            [0, 0],
            [0, 0],
            0,
            self.serial_survivor_ratio,
            SERIAL_SPACE_ALIGNMENT,
        )?;
        *cached = Some(birth);
        Some(birth)
    }

    pub fn serial_pool_snapshot_fallbacks(&self) -> u64 {
        self.serial_pool_snapshot_fallbacks.load(Ordering::Relaxed)
    }

    /// The heap `MemoryPoolMXBean`s this heap reports, in HotSpot Serial's
    /// model: `Eden Space` and `Survivor Space` (the young from-space, split
    /// at the last collection's survivors: see below) and `Tenured Gen` (the
    /// old generation). gen r4w2/obs; the pool model was reworked gen
    /// r5w2/obs6 so the pools, `Runtime` and the heap `MemoryUsage` tell one
    /// story. The mapping and its divergence from Serial are in `gc_metrics`'s
    /// "Per-generation JMX beans" block.
    /// The default fixed model accounts arena usage against the Serial pool
    /// budgets without dropping live bytes; pool maxima do not follow arena
    /// growth or survivor overflow. The physical-arena derivation below is
    /// retained for explicit-size heaps and the diagnostic `=0` kill switch.
    ///
    /// * The young usage is `used - free_list`, the live measure
    ///   [`Self::live_bytes_estimate`] uses, not the bump high-water: after a
    ///   non-moving young sweep the cursor has not moved and the reclaimed
    ///   bytes are on the free list (see [`Self::young_from_free_bytes`]).
    /// * gen r5w3/obs7: that usage is split. `Survivor Space.used` is what the
    ///   last completed collection left alive in young (its
    ///   `last_collection_usage[0]`, clamped to the young usage now), and
    ///   `Eden Space.used` the rest — what was allocated since. So
    ///   `Survivor Space` reads the survivors after every young collection
    ///   and `Eden Space` reads ~0 right after one, as on HotSpot Serial;
    ///   eden's collection usage is 0, survivor's the survivors. The
    ///   survivor's `committed` is its `used`, carved out of the young commit.
    ///   Until this wave the survivors were counted in `Eden Space` and
    ///   `Survivor Space` was `0/0/0`.
    /// * `committed` sums, over the three pools, to
    ///   [`Self::usable_committed_bytes`] — `VmHeap::committed_bytes`, i.e.
    ///   the heap `MemoryUsage`'s committed and `Runtime.totalMemory()` — and
    ///   `used` to [`Self::live_bytes_estimate`]: HotSpot's
    ///   `jmm_GetMemoryUsage` builds the heap usage by summing its pools.
    /// * `max` for eden is the ceiling a semi-space may grow to
    ///   (`young_semi_max`), for tenured its reservation; the two sum
    ///   to [`Self::max_usable_heap_bytes`] (`Runtime.maxMemory()`). Undefined
    ///   for eden when the heap has no bound (the explicit-size test
    ///   constructors).
    /// * `Survivor Space` has `init = 0` and an undefined `max`; before the
    ///   first collection it is `0/0/0`. The to-space is the copy reserve,
    ///   which is not usable heap and which HotSpot counts in no pool. Until
    ///   gen r5w2/obs6 this pool reported the to-space's capacity as
    ///   committed, so the pools summed to `-Xmx` while `totalMemory()` did
    ///   not. `max` is undefined rather than `0` so a caller computing
    ///   `used * 100 / max` gets 0, not an `ArithmeticException`.
    ///   gci i27/gs: under the default-ON `CRATONVM_GEN_SURVIVOR_POOL_RESERVE` the
    ///   survivor pool reports HotSpot Serial's reserve instead (max, init and
    ///   a committed of the young commit's reserve), carved out of eden; see
    ///   `survivor_reserve_split`.
    /// * `Tenured Gen`'s collection usage moves only on a collection that
    ///   reclaimed the old generation (gen r5w3/obs7): HotSpot Serial's young
    ///   collector manages the tenured pool as "not always affected", so a
    ///   young collection leaves its last collection usage alone. `0` until
    ///   the first such collection (HotSpot's `_after_gc_usage` starts at 0).
    /// * `Tenured Gen` `committed` is what the old generation has committed
    ///   (`reported_old_committed`), which grows from `-Xms` and falls when
    ///   a collection shrinks the free tail; its `init` is the `-Xms` prefix
    ///   the old generation committed at startup (its `commit_floor`). It
    ///   reported the reservation for both until gen r5w2/obs6
    ///   (`gengc-r5w1-oldgen5-tenured-pool-reports-the-reservation-as-committed`).
    /// * `init` for eden is the semi-space capacity the heap was built with,
    ///   clamped to `max` (gen r4w6/review6).
    ///
    /// Cost: three relaxed loads for eden's usage (the published trigger
    /// triple), two for the to-space span, and the `young_from`, `young_to`
    /// and old-generation locks once each, one at a time (the committed
    /// figures are granule counts the arenas keep under their locks).
    pub fn jmx_memory_pools(&self) -> [cratonvm_gc_core::gc_metrics::JmxPoolUsage; 3] {
        use cratonvm_gc_core::gc_metrics::{
            serial_pool_managers, JmxPoolUsage, JMX_POOL_EDEN, JMX_POOL_SURVIVOR, JMX_POOL_TENURED,
        };
        if let Some(pools) = self.serial_pool_usage(None) {
            let old_init = self.old_gen.lock().sizing_stats().commit_floor;
            let stamp = self.last_collection_usage[2].load(Ordering::Relaxed);
            let young_init = self.initial_young_semi_size;
            let survivor_init = serial_survivor_reserve_at(young_init, self.serial_survivor_ratio)
                .min(pools[1].max);
            return std::array::from_fn(|i| {
                let p = pools[i];
                let name = [JMX_POOL_EDEN, JMX_POOL_SURVIVOR, JMX_POOL_TENURED][i];
                let init = [
                    young_init.saturating_sub(survivor_init) as u64,
                    survivor_init as u64,
                    old_init,
                ][i]
                    .min(p.max as u64);
                JmxPoolUsage {
                    name,
                    init: Some(init),
                    used: p.used as u64,
                    committed: p.committed as u64,
                    max: Some(p.max as u64),
                    collection_used: (stamp != 0)
                        .then(|| self.serial_collection_used[i].load(Ordering::Relaxed) as u64),
                    managers: serial_pool_managers(name),
                }
            });
        }
        let (from_used, from_free, from_cap) = self.young_from_published();
        let from_cap = self.reported_semi_cap(from_cap);
        let young_live = from_used.saturating_sub(from_free);
        // gen r5w3/obs7 (2026-09-26;
        // `gengc-r5w2-obs6-proposal-survivor-pool-reports-the-last-cycles-survivors`):
        // the young generation's live bytes as the last completed collection
        // left them ARE that collection's survivors — at its end nothing but
        // survivors is in the from-space (a moving cycle copied them to its
        // base; a non-moving sweep left them in place and freed the rest).
        // They stay "survivors" until the next collection, as in HotSpot's
        // from-survivor space, and everything above them is what mutators
        // allocated since: Serial's eden. Clamped to the live bytes now (a
        // TLAB tail handed back to the free list can lower them).
        let recorded = self.last_collection_usage[2].load(Ordering::Relaxed) != 0;
        let survivor_used = if recorded {
            self.last_collection_usage[0]
                .load(Ordering::Relaxed)
                .min(young_live)
        } else {
            0
        };
        let eden_used = young_live - survivor_used;
        let to_cap = self.reported_semi_cap(self.young_to_capacity_published());
        let (young_committed, old_committed) = self.usable_committed_parts();
        let (old_used, old_cap, old_init) = {
            let og = self.old_gen.lock();
            (
                og.used(),
                og.capacity(),
                usize::try_from(og.sizing_stats().commit_floor).unwrap_or(usize::MAX),
            )
        };
        // gcj j3/og3 (B3 of `gcj-j2og2-free-ratio-growth-flip-blockers`,
        // `CRATONVM_GEN_TENURED_POOL_MAX_FIXED`, legacy reserve-kill arm): `Tenured Gen`'s max
        // moves toward HotSpot Serial's fixed tenured maximum (the growth
        // limit) as far as the pool identities allow. The pools' maxima must
        // still sum to `Runtime.maxMemory()` (`max_usable_heap_bytes`: the
        // young semi-space max plus the CAPACITY), and the young pools' max
        // may not fall below the semi-space the young arena already has (its
        // committed bytes live there). So the tenured max is the growth limit
        // capped at `maxMemory - max(from, to)`, never below the capacity, and
        // the young max is the remainder. j3a row `og3_b3_nums`: the first
        // version reported the growth limit and fed it to `young_semi_max`,
        // whose semi-space floor then broke the sum (245 vs 192 MiB at
        // `-Xmx256m`); while the young arena sits at its maximum the tenured
        // max IS the capacity, so HotSpot's fixed figure is out of reach without
        // a fixed `maxMemory` (residuals page P6).
        let semi_at_cap = self.young_semi_max(from_cap, to_cap, old_cap);
        let (semi_max, tenured_max) = match semi_at_cap {
            Some(semi) if true => {
                let floor = from_cap.max(to_cap);
                let total = self.serial_pool_max_total
                    .max(old_cap.saturating_add(floor));
                let t = self
                    .serial_tenured_pool_max
                    .min(total.saturating_sub(floor))
                    .max(old_cap);
                (Some(total - t), t)
            }
            _ => (semi_at_cap, old_cap),
        };
        // Widening: usize to u64 on every supported target.
        let b = |n: usize| n as u64;
        let after =
            |i: usize| recorded.then(|| b(self.last_collection_usage[i].load(Ordering::Relaxed)));
        // gen r4w6/review6: the construction semi-space size, clamped to `max`
        // because `new MemoryUsage(init, used, committed, max)` throws when
        // `init > max` (a budget that shrank below the initial size).
        let eden_init = {
            let init = b(self.initial_young_semi_size);
            Some(semi_max.map_or(init, |max| init.min(b(max))))
        };
        // `MemoryUsage` rejects `used > committed`; the young usage comes from
        // the published triple and the commit from the arena lock, so a racing
        // refill can put the two a granule apart.
        let young_total_committed = young_committed.max(young_live);
        // gen r5w3/obs7: the survivors' bytes are carved out of the young
        // commit, so the three pools still sum to `usable_committed_bytes`
        // (and `used` to `live_bytes_estimate`). `young_total_committed >=
        // young_live`, so eden keeps `used <= committed`.
        let survivor_committed = survivor_used;
        let eden_committed = young_total_committed - survivor_committed;
        // gci i27/gs (`gci-i14mp-...` item 8, `CRATONVM_GEN_SURVIVOR_POOL_RESERVE`,
        // default ON since 2026-10-17): HotSpot Serial's survivor reserve as the survivor
        // pool's max / committed, carved out of eden (sums unchanged).
        let reserve = match semi_max {
            // gci i45/sc: at the heap's `-XX:SurvivorRatio`.
            Some(m) if survivor_pool_reserve_enabled() => Some(survivor_reserve_split_at(
                m,
                young_total_committed,
                young_live,
                survivor_used,
                self.serial_survivor_ratio,
            )),
            _ => None,
        };
        let (eden_committed, survivor_committed) = match reserve {
            Some(s) => (s.eden_committed, s.survivor_committed),
            None => (eden_committed, survivor_committed),
        };
        let (eden_init, survivor_init) = match reserve {
            Some(s) => {
                let init = self.initial_young_semi_size;
                let surv =
                    serial_survivor_reserve_at(init, self.serial_survivor_ratio).min(s.survivor_max);
                (
                    Some(b(init.saturating_sub(surv).min(s.eden_max))),
                    Some(b(surv)),
                )
            }
            None => (eden_init, Some(0)),
        };
        [
            JmxPoolUsage {
                name: JMX_POOL_EDEN,
                init: eden_init,
                used: b(eden_used),
                committed: b(eden_committed),
                max: match reserve {
                    Some(s) => Some(b(s.eden_max)),
                    None => semi_max.map(|m| b(m.max(eden_committed))),
                },
                // gen r5w3/obs7: at the end of a collection everything young
                // is a survivor, so eden's collection usage is 0 — HotSpot
                // Serial's (eden is empty after every young collection).
                collection_used: recorded.then_some(0),
                managers: serial_pool_managers(JMX_POOL_EDEN),
            },
            JmxPoolUsage {
                name: JMX_POOL_SURVIVOR,
                init: survivor_init,
                // gen r5w3/obs7: the last collection's survivors (see
                // above); the to-space copy reserve is still in no pool.
                used: b(survivor_used),
                committed: b(survivor_committed),
                // gci i27/gs: HotSpot's `max_survivor_size()` under the
                // opt-in; undefined otherwise (as before).
                max: reserve.map(|s| b(s.survivor_max)),
                collection_used: after(0),
                managers: serial_pool_managers(JMX_POOL_SURVIVOR),
            },
            JmxPoolUsage {
                name: JMX_POOL_TENURED,
                // The `-Xms` prefix (`commit_floor`), clamped to `max` like
                // the young init.
                init: Some(b(old_init.min(tenured_max))),
                used: b(old_used),
                committed: b(old_committed.max(old_used)),
                max: Some(b(tenured_max)),
                collection_used: after(1),
                managers: serial_pool_managers(JMX_POOL_TENURED),
            },
        ]
    }

    /// This heap's GC-notification state: each JMX bean's last collection and
    /// the notifications waiting for delivery. gen r4w3/obs2; see
    /// [`cratonvm_gc_core::gc_metrics::GcNotificationQueue`].
    pub fn gc_notifications(&self) -> &cratonvm_gc_core::gc_metrics::GcNotificationQueue {
        &self.gc_notifications
    }

    /// The card table's self-maintained scan bound, for the `[GC]
    /// cards/scan_bound:` shutdown line: `(scan_bound_cards, num_cards,
    /// cards_skipped_by_scan_bound, raw_card_address_escaped)`. gen r4w2/obs.
    pub fn card_scan_bound_census(&self) -> (usize, usize, u64, bool) {
        (
            self.card_table.scan_bound_cards(),
            self.card_table.num_cards(),
            self.card_table.cards_skipped_by_scan_bound(),
            self.card_table.raw_card_address_escaped(),
        )
    }

    /// The tenuring threshold this collector applies: a young object is
    /// promoted once it has survived this many minor collections.
    ///
    /// Exposed for observability (the JFR `jdk.YoungGarbageCollection` event's
    /// `tenuringThreshold`, which the VM used to fill with G1's `15`).
    ///
    /// gen r4w6/young6: the value the NEXT young collection will use —
    /// [`PROMOTION_AGE`] by default, the adaptive threshold + 1 when adaptive
    /// tenuring is engaged. (Still this collector's "survivals before
    /// promotion" unit, which is HotSpot's threshold + 1; G1's accessor
    /// reports HotSpot's unit. Recorded in the young6 review, not changed:
    /// the JFR field's default value would move.)
    ///
    /// gen r5w2/obs6: this accessor keeps the collector's unit (every
    /// in-crate caller and test reasons in survivals); the HotSpot-unit value
    /// the JFR event now carries is [`Self::hotspot_tenuring_threshold`].
    pub fn tenuring_threshold(&self) -> u8 {
        self.effective_promotion_age()
    }

    /// The tenuring threshold in HotSpot's unit, `T`: an object whose age at
    /// the start of a young collection is `>= T` is promoted by it, i.e. on
    /// its `(T + 1)`-th survival — so [`Self::tenuring_threshold`] minus one
    /// (`2` at the default `PROMOTION_AGE` of 3). Under adaptive tenuring it
    /// is `TenuringState::threshold` itself, which is kept in HotSpot's unit.
    /// What `-Xlog:gc+age` prints as the "new threshold" and what JFR's
    /// `jdk.YoungGarbageCollection.tenuringThreshold` carries on HotSpot and,
    /// through `VmHeap::tenuring_threshold`, on G1 here. gen r5w2/obs6
    /// (2026-09-26; `gengc-r4w6-young6-tenuring-threshold-units-differ-across-backends`).
    pub fn hotspot_tenuring_threshold(&self) -> u8 {
        self.effective_promotion_age().saturating_sub(1)
    }

    /// `should_tenure`'s `promotion_age` for the next young collection, moving
    /// or non-moving (gen r4w6/young6). [`PROMOTION_AGE`] unless adaptive
    /// tenuring is engaged on this heap (`CRATONVM_GC_ADAPTIVE_TENURING`, or
    /// a `-XX:` tenuring value through [`Self::set_tenuring_config`]).
    pub fn effective_promotion_age(&self) -> u8 {
        self.tenuring
            .promotion_age(adaptive_tenuring_flag(), PROMOTION_AGE)
    }

    /// gc wave 9 (lane gcC): `should_tenure`'s `promotion_age` for the young
    /// cycle in progress: [`Self::effective_promotion_age`], or
    /// [`NON_AGING_PROMOTION_AGE`] when `CRATONVM_GEN_AGE_BY_SERIAL_EDEN` held
    /// this cycle's aging ([`Self::open_young_age_epoch`]). Identical to
    /// `effective_promotion_age` on an unarmed heap.
    pub fn cycle_promotion_age(&self) -> u8 {
        if self.young_age_this_cycle.load(Ordering::Relaxed) {
            self.effective_promotion_age()
        } else {
            NON_AGING_PROMOTION_AGE
        }
    }

    /// The young live bytes as the trigger reads them (`used` minus the
    /// free list): what a moving cycle left in from-space, or a sweep's
    /// survivors and unusable fragments, plus everything allocated since.
    pub fn young_live_for_age_epoch(&self) -> usize {
        let from = self.lock_young_from();
        from.used().saturating_sub(from.free_list_bytes())
    }

    /// gc wave 9 (lane gcC): decide whether the young cycle about to run ages
    /// its survivors ([`serial_eden_age_epoch`]). Called by
    /// [`Self::collect_garbage_inner`] before any arena lock is taken. A no-op
    /// on an unarmed heap. The live figure is re-based here as well as at the
    /// cycle's end, so a refused cycle's retry counts only what was allocated
    /// after the refusal; the eden a refused AGING cycle consumed is given back
    /// in [`Self::close_young_age_epoch`].
    pub fn open_young_age_epoch(&self) {
        // A heap pinned to the fixed promotion age (`pin_fixed_promotion_age`,
        // tests that assume the historical rule) ages once per cycle.
        if self.young_age_eden == 0 || self.tenuring.is_pinned_fixed() {
            return;
        }
        let live = self.young_live_for_age_epoch();
        let allocated = live.saturating_sub(self.young_age_epoch[1].load(Ordering::Relaxed));
        let (ages, credit) = serial_eden_age_epoch(
            self.young_age_epoch[0].load(Ordering::Relaxed),
            allocated,
            self.young_age_eden,
        );
        self.young_age_epoch[0].store(credit, Ordering::Relaxed);
        self.young_age_epoch[1].store(live, Ordering::Relaxed);
        self.young_age_this_cycle.store(ages, Ordering::Relaxed);
    }

    /// gc wave 9 (lane gcC): the young cycle ended; what is in young now is
    /// the base the next cycle's allocation is measured from. The cycle flag
    /// goes back to "ages", so any young collection that bypasses
    /// [`Self::open_young_age_epoch`] keeps the default rule.
    pub fn close_young_age_epoch(&self) {
        if self.young_age_eden == 0 {
            return;
        }
        let ages = self.young_age_this_cycle.load(Ordering::Relaxed);
        if self.young_cycle_skipped.load(Ordering::Relaxed) {
            // A refused cycle aged nothing: an aging verdict's eden goes back,
            // so the retry ages. Not counted either way.
            if ages {
                let credit = self.young_age_epoch[0].load(Ordering::Relaxed);
                self.young_age_epoch[0].store(
                    credit.saturating_add(self.young_age_eden),
                    Ordering::Relaxed,
                );
            }
        } else {
            self.young_age_epoch[if ages { 2 } else { 3 }].fetch_add(1, Ordering::Relaxed);
        }
        let live = self.young_live_for_age_epoch();
        self.young_age_epoch[1].store(live, Ordering::Relaxed);
        self.young_age_this_cycle.store(true, Ordering::Relaxed);
    }

    /// Apply the launcher's HotSpot tenuring flags (`-XX:MaxTenuringThreshold`,
    /// `-XX:InitialTenuringThreshold`, `-XX:TargetSurvivorRatio`,
    /// `-XX:+PrintTenuringDistribution`) to this heap. Call before the first
    /// collection; a default config changes nothing. gen r4w6/young6.
    pub fn set_tenuring_config(&self, cfg: &TenuringConfig) {
        if cfg.is_default() {
            return;
        }
        self.tenuring.configure(cfg);
    }

    /// gcg g3/x: the HotSpot free-ratio flags for the old generation's
    /// resize. A pair of `None` changes nothing.
    pub fn set_heap_free_ratios(&self, min: Option<u8>, max: Option<u8>) {
        if min.is_none() && max.is_none() {
            return;
        }
        self.old_gen.lock().set_heap_free_ratios(min, max);
    }

    /// gcg g1/y: pin this heap to the fixed [`PROMOTION_AGE`] -- the
    /// `CRATONVM_GC_ADAPTIVE_TENURING=0` arm for this heap alone, whatever
    /// the variable says (a typed `-XX:` tenuring value through
    /// [`Self::set_tenuring_config`] still engages adaptive tenuring, as it
    /// does under `=0`). Per heap: no process state. Tests that assume the
    /// historical age ask for it here, explicitly.
    pub fn pin_fixed_promotion_age(&self) {
        self.tenuring.pin_fixed();
    }

    /// [`Self::pin_fixed_promotion_age`], by value, for a constructor chain:
    /// `GenerationalHeap::with_sizes(a, b).with_fixed_promotion_age()`.
    #[must_use]
    pub fn with_fixed_promotion_age(self) -> Self {
        self.pin_fixed_promotion_age();
        self
    }

    /// gcf f4/r: `-Xlog:gc+age` wants the next young collection's age table
    /// (the VM sets this before each collection, `gc_events::gc_event_start`).
    pub fn set_tenuring_xlog(&self, on: bool) {
        self.tenuring.set_xlog(on);
    }

    /// gcf f4/r: the last young collection's `(desired_bytes, new_threshold,
    /// max_threshold, ages)` for the `gc,age` lines, taken once. `ages[a]` is
    /// the bytes of survivors of age `a` (index 0 unused, as HotSpot's table).
    pub fn take_tenuring_xlog(&self) -> Option<(u64, u8, u8, Vec<u64>)> {
        self.tenuring
            .take_xlog()
            .map(|(desired, next, max, ages)| (desired, next, max, ages.to_vec()))
    }

    /// `[adapted, raised, lowered, unchanged]` — how often the adaptive
    /// tenuring threshold was recomputed and which way it moved, over the
    /// heap's life (gen r4w6/young6). All zero unless adaptive tenuring is
    /// engaged.
    pub fn tenuring_census(&self) -> [u64; 4] {
        self.tenuring.census()
    }

    /// Record one timed collection into [`Self::pause_totals`]. Called by
    /// the two public entry points only, after `collect_garbage_inner`
    /// returns, with the pause they measured around it.
    pub fn note_collection_pause(&self, pause: std::time::Duration, refused: bool) {
        // Both public collection entries have released their arena guards.
        // Clear this pause's promotion prologue even when it did no old resize.
        self.old_gen.lock().note_collection_epilogue();
        if refused {
            return;
        }
        let us = u64::try_from(pause.as_micros()).unwrap_or(u64::MAX);
        // Round UP to whole milliseconds, and never below one: a collection
        // that completed must advance the JMX time (see the field doc).
        let ms_ceiled = us.div_ceil(1000).max(1);
        self.pause_totals[0].fetch_add(1, Ordering::Relaxed);
        self.pause_totals[1].fetch_add(us, Ordering::Relaxed);
        self.pause_totals[2].fetch_add(ms_ceiled, Ordering::Relaxed);
        // gen r4w2/obs: the full-collector share, and the post-collection pool
        // occupancy. `old_gen_reclaimed_last_cycle` is a thread-local set by
        // this same collecting thread inside `collect_garbage_inner` (cleared
        // on entry, set by either old-gen path), so it describes the cycle
        // just timed.
        let old_reclaimed = cratonvm_gc_core::gc_quiescence::old_gen_reclaimed_last_cycle();
        // gcg g6/yg: taken on every cycle, so a mark can never outlive its own.
        let escalation = self.take_escalation_mark();
        if old_reclaimed {
            self.old_pause_totals[0].fetch_add(1, Ordering::Relaxed);
            self.old_pause_totals[1].fetch_add(us, Ordering::Relaxed);
            self.old_pause_totals[2].fetch_add(ms_ceiled, Ordering::Relaxed);
            // gcg g6/yg: the young half of a pause whose major nobody asked
            // for -- from the pause start to the mark, the rest being the
            // major's (`mark.elapsed()` measures to now, just after the
            // collection returned).
            if let Some((mark, decided_old_used)) = escalation {
                {
                    let old_us = u64::try_from(mark.elapsed().as_micros()).unwrap_or(u64::MAX);
                    let young_us = us.saturating_sub(old_us);
                    self.escalated_young_totals[0].fetch_add(1, Ordering::Relaxed);
                    self.escalated_young_totals[1].fetch_add(young_us, Ordering::Relaxed);
                    // gcg h1/ob: the split the VM's `-Xlog` / JFR /
                    // notification reporting reads (`take_escalation_split`).
                    if let Some(old_used_at_mark) = decided_old_used {
                        self.escalation_mark.lock().booked = Some(EscalationSplit {
                            at: mark,
                            old_used_at_mark,
                        });
                    }
                }
            }
        }
        // No arena lock is held here (the entry points call this after
        // `collect_garbage_inner` has released every guard); the young figure
        // is the published triple, the old one takes the old-gen lock once
        // per full collection.
        //
        // `[0]` — the young live bytes at the end of this collection: every
        // one of them is a survivor, so it is both `Survivor Space`'s
        // collection usage and the survivor/eden split point
        // `jmx_memory_pools` uses until the next collection (gen r5w3/obs7).
        let (from_used, from_free, _) = self.young_from_published();
        let young_live = from_used.saturating_sub(from_free);
        self.last_collection_usage[0].store(young_live, Ordering::Relaxed);
        // gci i14/mp finding 3: the committed half of the same figures, as
        // `jmx_memory_pools` splits it (survivor committed = its used, eden the
        // rest of the young commit, tenured the old generation's). The young
        // and old locks once each, one at a time.
        let (young_committed, old_committed) = self.usable_committed_parts();
        let young_total = young_committed.max(young_live);
        // gci i27/gs (`CRATONVM_GEN_SURVIVOR_POOL_RESERVE`, default ON since 2026-10-17): the
        // same split `jmx_memory_pools` reports (eden's usage is 0 here).
        let semi_max = if survivor_pool_reserve_enabled() {
            let (_, _, from_cap) = self.young_from_published();
            let from_cap = self.reported_semi_cap(from_cap);
            let to_cap = self.reported_semi_cap(self.young_to_capacity_published());
            let old_cap = self.old_gen.lock().capacity();
            self.young_semi_max(from_cap, to_cap, old_cap)
        } else {
            None
        };
        let (eden_cc, survivor_cc) = match semi_max {
            Some(m) => {
                let s = survivor_reserve_split_at(
                    m,
                    young_total,
                    young_live,
                    young_live,
                    self.serial_survivor_ratio,
                );
                (s.eden_committed, s.survivor_committed)
            }
            None => (young_total - young_live, young_live),
        };
        self.last_collection_committed[0].store(eden_cc, Ordering::Relaxed);
        self.last_collection_committed[1].store(survivor_cc, Ordering::Relaxed);
        // `[1]` — `Tenured Gen`'s collection usage moves only when this
        // collection reclaimed the old generation, as HotSpot Serial's young
        // collector leaves the tenured pool's last collection usage alone
        // (gen r5w3/obs7; it moved on every collection).
        let mut stamp = COLLECTION_RECORDED;
        if old_reclaimed {
            let old_used = self.old_gen.lock().used();
            self.last_collection_usage[1].store(old_used, Ordering::Relaxed);
            self.last_collection_committed[2].store(old_committed.max(old_used), Ordering::Relaxed);
            stamp |= TENURED_COLLECTION_STAMPED;
        }
        if let Some(pools) = self.serial_pool_usage(Some(young_live)) {
            for i in 0..2 {
                self.serial_collection_used[i].store(pools[i].used, Ordering::Relaxed);
                self.last_collection_committed[i].store(pools[i].committed, Ordering::Relaxed);
            }
            if old_reclaimed {
                self.serial_collection_used[2].store(pools[2].used, Ordering::Relaxed);
                self.last_collection_committed[2].store(pools[2].committed, Ordering::Relaxed);
            }
        }
        self.last_collection_usage[2].fetch_or(stamp, Ordering::Relaxed);
    }

    /// gci i14/mp finding 3: `[eden, survivor, tenured]`
    /// `getCollectionUsage().getCommitted()` -- the committed at the end of the
    /// last collection that affected each pool; `None` before one (`Tenured
    /// Gen` until a collection reclaimed the old generation: HotSpot Serial's
    /// `Copy` does not manage it, so its last collection usage stays
    /// `(init, 0, 0, max)`). Pool order of [`Self::jmx_memory_pools`].
    pub fn jmx_collection_committed(&self) -> [Option<u64>; 3] {
        let stamp = self.last_collection_usage[2].load(Ordering::Relaxed);
        // Widening: usize to u64 on every supported target.
        let at = |i: usize| self.last_collection_committed[i].load(Ordering::Relaxed) as u64;
        let recorded = stamp & COLLECTION_RECORDED != 0;
        [
            recorded.then(|| at(0)),
            recorded.then(|| at(1)),
            (stamp & TENURED_COLLECTION_STAMPED != 0).then(|| at(2)),
        ]
    }

    /// Snapshot the pre-collection occupancy for the `--verbose:gc` line, or
    /// `None` when the flag is off.
    ///
    /// Called with no arena lock held, from the two public collection entry
    /// points, so the two `lock()`s here are uncontended and cost one
    /// collection rather than one per object. Taken BEFORE the pause clock
    /// starts, so they are not charged to the pause either.
    pub fn verbose_gc_begin(&self) -> Option<VerboseGcCycle> {
        if !self.verbose_gc_enabled() {
            return None;
        }
        Some(VerboseGcCycle {
            young_used: self.young_from_used(),
            old_used: self.old_gen_used(),
            young_free: self.young_from_free_bytes(),
        })
    }

    /// Emit the per-collection `--verbose:gc` line. No-op when
    /// [`Self::verbose_gc_begin`] returned `None`.
    ///
    /// # Why this lives at the entry points and not inside the cycle
    ///
    /// `collect_garbage_inner` has many exits — the non-moving divert returns
    /// straight out of `run_non_moving_young_cycle`, and two backstops refuse
    /// the cycle without doing anything. A line emitted from inside would have
    /// to be repeated on each of them, and the gap page's own acceptance test
    /// is that the number of printed lines equals the number of collections:
    /// "a per-collection emitter that misses a GC door is the same defect
    /// again". The two public entry points are the only doors into
    /// `collect_garbage_inner`, so emitting here cannot miss one by
    /// construction.
    ///
    /// It also means the measured `pause=` is the whole stop-the-world window
    /// the caller paid for, which is the number an operator comparing against
    /// HotSpot means — not the sum of the phases the collector chose to time.
    ///
    /// `eprintln!` rather than `tracing::`, for the reason `zgc.rs` already
    /// gives on its twin: `tracing::debug!` is compiled out of release builds
    /// and this line has to exist in the binary people reproduce with.
    ///
    /// `elapsed_us` is the pause the entry point measured around
    /// `collect_garbage_inner` — the same figure the pause-goal loop and
    /// [`Self::gc_pause_totals`] receive.
    pub fn verbose_gc_end(&self, cycle: Option<VerboseGcCycle>, elapsed_us: u128) {
        let Some(cycle) = cycle else { return };
        let young_after = self.young_from_used();
        let old_after = self.old_gen_used();
        let minor = self.stats.minor_gc_count.load(Ordering::Relaxed);
        // A refused cycle (to-space cannot cover from-space, or the
        // object-start walk could not complete) did not collect. Saying
        // "moving" about it would be a false report of exactly the kind
        // `moving_cycles_total` was renamed to stop.
        let refused = self.young_cycle_skipped.load(Ordering::Relaxed);
        // Ground truth for moving/non-moving, recorded by the branch that
        // actually decides (`gc_metrics::record_collector_decision`), so this
        // line cannot drift from the code the way a re-derivation would.
        let decision = cratonvm_gc_core::gc_metrics::last_collector_decision();
        let kind = if refused {
            "refused"
        } else if decision.is_some_and(|d| d.young_moving) {
            "moving"
        } else {
            "non-moving"
        };
        // Did an old-generation reclamation run in THIS cycle? Both old-gen
        // paths (the mark-compact `major_gc` and the in-place
        // `sweep_old_gen_non_moving`) set it, and every cycle clears it on
        // entry — and it is a thread-local set by this same collecting thread.
        let major = cratonvm_gc_core::gc_quiescence::old_gen_reclaimed_last_cycle();
        // On a non-moving cycle the interesting question is WHICH term of
        // `divert_non_moving` refused the copy — that is the one an operator
        // has to fix to get a moving cycle back, and it is the whole content
        // of `docs/gc-tuning.md`'s "[moving-young] fallback" section.
        let divert = match decision {
            // A refused cycle: `kind` already says `refused`, but not WHICH
            // refusal, and the two have different owners and different fixes
            // (an unparseable from-space layout vs. a live mutator's
            // un-retired TLAB tail). Rendered as `refusal=`, not `divert=`.
            Some(d) if refused && cratonvm_gc_core::gc_metrics::decision_reason::is_skipped(d.reason) => {
                Some(cratonvm_gc_core::gc_metrics::decision_reason::label(d.reason).to_string())
            }
            Some(d) if !d.young_moving && !refused => {
                let mut s = cratonvm_gc_core::gc_metrics::decision_reason::label(d.reason).to_string();
                if d.reason == cratonvm_gc_core::gc_metrics::decision_reason::NON_MOVING_COVERAGE_INCOMPLETE {
                    s.push('/');
                    s.push_str(cratonvm_gc_core::gc_quiescence::incomplete_reason::label(
                        d.incomplete_reason,
                    ));
                }
                // (The `/no-moving-young-optout` qualifier that stood here is
                // gone: the bare opt-out has its own reason code,
                // `nonmoving-young-compaction-disabled`, since gen r4w2,
                // 2026-09-23.)
                Some(s)            }
            _ => None,
        };
        eprintln!(
            "{}",
            render_verbose_gc_line(
                minor,
                kind,
                major,
                (cycle.young_used, young_after),
                (cycle.old_used, old_after),
                (cycle.young_free, self.young_from_free_bytes()),
                elapsed_us,
                divert.as_deref(),
            )
        );
    }

    /// Run a minor garbage collection cycle.
    ///
    /// Copies live young-gen objects to young to-space (or promotes to old gen
    /// if they have survived enough cycles). Scans dirty card table entries
    /// for old→young references.
    ///
    /// Returns GC statistics and a pointer remapping table.
    /// Like [`collect_garbage`] but keeps dead finalizable objects alive so
    /// their `finalize()` method can be invoked.  Returns the GC result and
    /// the *new* (post-GC) addresses of dead finalizable objects.
    ///
    /// The `_stw` parameter is type-level proof that the caller is in a
    /// stop-the-world phase — see [`cratonvm_gc_core::collector::StopTheWorldToken`].
    pub fn collect_garbage_with_finalizers(
        &self,
        _stw: &cratonvm_gc_core::collector::StopTheWorldToken,
        roots: &mut [ObjectRef],
        finalizer_addrs: &[usize],
        monitors: &dyn MonitorCleanup,
    ) -> (GcResult, Vec<usize>) {
        let _phase = cratonvm_jfr::phase::enter(cratonvm_jfr::phase::Category::GcPause);
        let goal = self.young_pause_goal_ms; // gci i4/yg: per heap
        // `--verbose:gc` before-image, taken before the pause clock starts.
        let verbose = self.verbose_gc_begin();
        // JVMTI `GarbageCollectionStart` / `GarbageCollectionFinish`.
        //
        // gen r4/plumbing (2026-09-23): the VM installs these hooks at boot
        // (`vm_init.rs`, `install_gc_start_hook`), and their only callers were
        // `gc::collect` / `gc::collect_with_finalizers` — the Cheney driver of
        // the vestigial semi-space `Heap`, which no `GcBackend` selects. So an
        // agent that enabled the two events received NEITHER on a generational
        // run, for any number of collections. These two public entry points
        // are the only doors into `collect_garbage_inner`, so firing here
        // cannot miss a collection. Both are one `Acquire` load when no hook
        // is installed.
        cratonvm_gc_core::gc::fire_gc_start();
        // ONE pause measurement, around the collection only, feeding the
        // pause-goal loop, the JMX pause total and the `--verbose:gc` line.
        // Until 2026-09-23 the goal loop's clock started before the verbose
        // before-image and stopped after the verbose `eprintln!`, so turning
        // `--verbose:gc` on lengthened the pauses the young-trigger feedback
        // judged — the instrument perturbed the tuning it reports on.
        let t0 = std::time::Instant::now();
        let mut out = self.collect_garbage_inner(roots, finalizer_addrs, monitors);
        // gc-common w36-d: the concurrent cycle's half of the finalizer
        // channel (see `ConcurrentGcState::publish_finalizer_candidates`).
        self.exchange_concurrent_finalizers(finalizer_addrs, &mut out);
        let pause = t0.elapsed();
        cratonvm_gc_core::gc::fire_gc_finish();
        let refused = self.young_cycle_skipped.load(Ordering::Relaxed);
        self.note_collection_pause(pause, refused);
        self.verbose_gc_end(verbose, pause.as_micros());
        // gengc-round1-core (2026-09-20): a REFUSED cycle is not a measured
        // pause. Feeding its ~0 ms through `next_young_trigger` takes the
        // `pause * 4 < goal` arm, which clears a pending halving trial — so a
        // refusal landing between the halving and the collection that judges
        // it silently disarms the revert rule that exists to stop the loop
        // halving the trigger against a live set the nursery cannot shrink.
        // gci i3/yg: through `feed_pause_goal_loop`, which also consumes the
        // VM's forced-cycle mark.
        self.feed_pause_goal_loop(pause, goal, out.0.stats.bytes_copied as u64, refused);
        self.external_gpu.on_gc_cycle_end();
        out
    }

    /// The pause-goal loop's door, shared by [`Self::collect_garbage`] and
    /// [`Self::collect_garbage_with_finalizers`] right after their single
    /// pause measurement: a refused cycle measured no pause and is never fed
    /// (see `collect_garbage_with_finalizers`), and `goal == 0` is the loop
    /// switched off.
    ///
    /// gci i3/yg: it first READS AND CLEARS the VM's forced-cycle mark
    /// ([`Self::mark_next_young_cycle_forced`]) on every call, so a mark never
    /// outlives the cycle it was set for; under
    /// `CRATONVM_GC_YOUNG_PAUSE_SKIP_FORCED` a forced cycle is counted
    /// (`adapt_calls`, `adapt_skipped_forced`) and not fed to the decision,
    /// leaving the threshold and any pending trial as they were. Off: the
    /// historical call, unchanged.
    pub fn feed_pause_goal_loop(
        &self,
        pause: std::time::Duration,
        goal: u64,
        bytes_copied: u64,
        refused: bool,
    ) {
        let forced = self.young_cycle_forced.swap(false, Ordering::Relaxed);
        // gci i22/ed: HotSpot Serial's post-collection resize under
        // `CRATONVM_GEN_SERIAL_EDEN_INITIAL`, whatever the pause goal (one
        // relaxed load when off). A refused cycle collected nothing.
        if !refused {
            self.serial_eden_follow_after_cycle();
        }
        if goal == 0 || refused {
            return;
        }
        if forced {
            let mut feedback = self.young_trigger_feedback.lock();
            if feedback.skip_forced {
                feedback.census.adapt_calls += 1;
                feedback.census.adapt_skipped_forced += 1;
                return;
            }
        }
        self.adapt_young_trigger_to_pause(
            u64::try_from(pause.as_millis()).unwrap_or(u64::MAX),
            goal,
            bytes_copied,
        );
    }

    /// gc-common w36-d — the stop-the-world end of the concurrent cycle's
    /// finalizer channel (`ConcurrentGcState::publish_finalizer_candidates`).
    /// A no-op until `enable_concurrent_gc`.
    ///
    /// 1. REPORT what the last remark retained: each retained address that is
    ///    still a candidate of THIS collection (so still a registered or
    ///    queued finalizable — one that has since been finalized and
    ///    deregistered is dropped, which keeps finalization once-only), at
    ///    its post-collection address, unless the collection already reported
    ///    it (an old-gen collection resurrects the same object itself).
    /// 2. PUBLISH this collection's candidates that now live in old gen, at
    ///    their post-collection addresses, for the next remark.
    ///
    /// Both halves are this pause's: the world is stopped and the remark runs
    /// in a later pause, so the channel cannot be read half-written.
    pub fn exchange_concurrent_finalizers(
        &self,
        finalizer_addrs: &[usize],
        out: &mut (GcResult, Vec<usize>),
    ) {
        let Some(state) = self.conc_cycle_state.as_deref() else {
            return;
        };
        let retained = state.take_remark_retained_finalizers();
        if !retained.is_empty() {
            let registered: FxHashSet<usize> = finalizer_addrs.iter().copied().collect();
            // gcj j3/og3 (C7): a set beside the list instead of a linear
            // `contains` per retained candidate (O(dead x retained) inside the
            // pause); the same entries in the same order.
            let mut present: FxHashSet<usize> = out.1.iter().copied().collect();
            for addr in retained {
                if !registered.contains(&addr) {
                    continue;
                }
                let now = out.0.pointer_map.get(&addr).copied().unwrap_or(addr);
                if present.insert(now) {
                    out.1.push(now);
                }
            }
        }
        if finalizer_addrs.is_empty() {
            state.publish_finalizer_candidates(Vec::new());
            return;
        }
        let old_gen = self.old_gen.lock();
        let old: Vec<usize> = finalizer_addrs
            .iter()
            .map(|a| out.0.pointer_map.get(a).copied().unwrap_or(*a))
            .filter(|&a| old_gen.contains(a as *const u8))
            .collect();
        drop(old_gen);
        state.publish_finalizer_candidates(old);
    }

    /// gci i17/fz (i16/fr's residual): [`Self::exchange_concurrent_finalizers`]
    /// publishes every resurrection candidate the stop-the-world door handed
    /// the collection, and with `CRATONVM_STW_UNDISCOVERED_REFS_STRONG` those
    /// include the VM probe's referents, which are not finalizable. The door
    /// takes them back here, inside the same pause, with their
    /// POST-collection addresses (`ConcurrentGcState::retract_finalizer_candidates`),
    /// so the next concurrent remark retains only real finalizable objects.
    /// A no-op until `enable_concurrent_gc`, and for an empty list.
    pub fn retract_concurrent_finalizer_candidates(&self, post_addrs: &[usize]) {
        if post_addrs.is_empty() {
            return;
        }
        if let Some(state) = self.conc_cycle_state.as_deref() {
            state.retract_finalizer_candidates(post_addrs);
        }
    }

    /// Feedback-size the young collection trigger against a pause goal.
    ///
    /// A moving young cycle's cost is dominated by how much it copies, and
    /// nothing currently ties that to a pause target: `young_semi` is
    /// `-Xmx / 4` and the trigger is a fixed percentage of it. Measured on the
    /// OAuth2 issuer-URI workload at `--Xmx 2g` — young semi 512 MB, trigger
    /// firing around 291 MB occupied — a single young collection copies ~994 000
    /// objects and takes 283-872 ms. Spring Security gives that exchange a
    /// 500 ms socket read budget and MockWebServer runs INSIDE the VM, so one
    /// such pause loses the response and the read times out.
    ///
    /// **Why feedback rather than a smaller semi-space.** Capping the initial
    /// young semi was tried before and measured a NET REGRESSION for large
    /// `-Xmx` workloads (see `with_capacity`'s note and
    /// `binarytrees-bt18-half-gap-20260730.md`): those workloads
    /// fall back to the non-moving sweep, and a smaller semi just fires that
    /// expensive fallback more often. Reacting to the pause we actually
    /// measured cannot repeat that mistake — a workload whose cycles are
    /// already inside the goal is never touched.
    ///
    /// **Default [`DEFAULT_YOUNG_PAUSE_GOAL_MS`]** since 2026-08-11;
    /// `CRATONVM_GC_YOUNG_PAUSE_MS=0` opts out. See `young_pause_goal_ms` for
    /// the throughput control behind the flip.
    ///
    /// The adapted value only reaches the collector through
    /// [`young_gc_trigger_bytes`], which until 2026-08-11 consulted it on the
    /// MOVING branch only — so on a JIT-heavy workload, where the trigger
    /// predicts "non-moving" at essentially every allocation, this whole
    /// feedback loop moved a number nothing read.
    ///
    /// **gen-gc-five (2026-09-02): a halving is a TRIAL, not a policy.** Once
    /// the refill-time trigger was made reachable on compiled code (the entry
    /// gate in `tlab_alloc_object_inner`), this loop was observed doing what
    /// `gen-gc-minor-pause-20260902` predicted: on `OldGenRsetProbe`, whose
    /// tenure cycles copy a fixed 1.2M-object live set, it halved the trigger
    /// 136 MB -> 67 -> 33 -> 19 MB across consecutive cycles without the pause
    /// moving at all, and the run took 88 collections where 14 would do. A
    /// moving pause is `a * survivors + b * allocated`; the nursery size only
    /// reaches the second term, and when the first dominates a smaller nursery
    /// buys nothing but more collections. So each halving is now checked
    /// against the pause it produces ([`next_young_trigger`]): one that did
    /// not cut the pause by a quarter is REVERTED, and the loop latches on the
    /// survivor volume it saw until that volume changes by a factor of two —
    /// which is the signal that the live set, not the nursery, was the pause.
    pub fn adapt_young_trigger_to_pause(&self, pause_ms: u64, goal_ms: u64, bytes_copied: u64) {
        // gcg h1/ob (`gcg-g4yg-young-sizing-and-xlog-review-findings` item 10):
        // the published capacity (equal to the arena's at every unlock, and no
        // young lock is held here), not a lock whose drop republishes the
        // trigger triple once per collection; the lock only before the first
        // publication.
        let capacity = match self.young_from_pub_capacity.load(Ordering::Relaxed) {
            0 => self.lock_young_from().capacity(),
            published => published,
        };
        if capacity == 0 {
            return;
        }
        // Never let feedback drive the trigger below this, or a workload with a
        // genuinely large live set would collect continuously and make things
        // worse than the pause it was trying to avoid.
        let floor = (capacity / 16).max(1);
        let ceiling = self.configured_young_trigger(capacity); // gcf f7/c
                                                               // gen r5w3/oldgen7: an old generation that grew into the young budget
                                                               // (`CRATONVM_GC_OLD_BORROW_YOUNG`) caps the loop's ceiling too, so an
                                                               // additive increase cannot hand the budget back. `None` — no clamp —
                                                               // on every heap that never grew.
        let ceiling = match self.borrowed_young_trigger_cap(self.old_gen_capacity()) {
            Some(cap) => ceiling.min(cap),
            None => ceiling,
        };
        // STW: this is the post-collection feedback step, so the read and the
        // store below cannot race a second writer.
        let current = self
            .young_gc_threshold
            .load(Ordering::Relaxed)
            .clamp(floor, ceiling.max(floor));
        let mut feedback = self.young_trigger_feedback.lock();
        // Count every measured pause the loop is handed (gen r4/alloc). Both
        // callers run this on the collecting thread straight after
        // `collect_garbage_inner`, which is the thread and the window
        // `OLD_GEN_RECLAIMED` is defined for.
        feedback.census.adapt_calls += 1;
        // gen r4w2/alloc2 (2026-09-23): a pause whose cycle ALSO reclaimed the
        // old generation (mark-compact on the moving path, the old-gen sweep on
        // the non-moving divert) is not evidence about the nursery. Nothing the
        // young trigger does can shorten an old-gen sweep or compaction, and
        // feeding that pause to `next_young_trigger` both halved the trigger
        // for it AND made it the baseline a pending trial is judged against —
        // so the next, much shorter, young-only pause "proved" the halving
        // helped and the loop halved again, defeating the gen-gc-five revert
        // latch. Such a pause now neither opens nor judges a trial and leaves
        // the threshold where it is: `fb.trial` stays pending for the next
        // young-only pause to judge. See
        // `docs/feature-designs/generative-gc-architecture.md`.
        if cratonvm_gc_core::gc_quiescence::old_gen_reclaimed_last_cycle() {
            feedback.census.adapt_on_major += 1;
            return;
        }
        let survivor_bytes = self.pause_goal_survivor_bytes(bytes_copied);
        let next = next_young_trigger(
            &mut feedback,
            current,
            floor,
            ceiling.max(floor),
            capacity,
            pause_ms,
            goal_ms,
            survivor_bytes,
        );
        self.young_gc_threshold.store(next, Ordering::Relaxed);
        if feedback.census.trigger_low_water == 0 || next < feedback.census.trigger_low_water {
            feedback.census.trigger_low_water = next;
        }
        drop(feedback);
        if gc_flags().dbg_gcpause && next != current {
            eprintln!(
                "[gcpause] young trigger {}KB -> {}KB (pause={pause_ms}ms goal={goal_ms}ms)",
                current / 1024,
                next / 1024
            );
        }
    }

    /// The survivor volume the pause-goal loop latches on for the collection
    /// that just completed: `bytes_copied` (to-space occupancy) after a moving
    /// cycle, and the young live set `sweep_young_non_moving` recorded in
    /// [`Self::non_moving_latch`] after a non-moving one.
    ///
    /// gen r4w3/young2 (2026-09-23). The non-moving `GcStats::bytes_copied`
    /// used to be `used_at_entry - swept`, which includes every byte on the
    /// free list at entry and so sits near `capacity - swept` on every cycle
    /// of a JIT-warm run: after one reverted halving the loop latched on it
    /// and `within_2x` never let it re-arm, whatever the live set did. Now
    /// `bytes_copied` says what was copied (promoted bytes, usually 0 — which
    /// as a latch input would re-arm on every jump from 0), so the latch reads
    /// the live set instead. The stamp makes the choice exact: it matches
    /// `minor_gc_count` only when the cycle that moved the count last was the
    /// non-moving sweep (a moving cycle bumps the count past it; a refused
    /// cycle never reaches the loop).
    pub fn pause_goal_survivor_bytes(&self, bytes_copied: u64) -> u64 {
        let count = self.stats.minor_gc_count.load(Ordering::Relaxed);
        if self.non_moving_latch[0].load(Ordering::Relaxed) == count {
            self.non_moving_latch[1].load(Ordering::Relaxed)
        } else {
            bytes_copied
        }
    }

    /// The young-trigger instrument: the live threshold, what set it, and what
    /// the pause-goal loop did to it. See [`YoungTriggerCensus`].
    ///
    /// Lock-free except for the feedback mutex, which only
    /// [`Self::adapt_young_trigger_to_pause`] takes (once per collection), and
    /// (gci i1/yg, only on a heap with the age clock armed) the age-clock
    /// mutex, which nothing holds while taking an arena lock, so
    /// this is safe to call at shutdown or from a report with any arena lock
    /// held. The capacity is the published one (`young_from_pub_capacity`),
    /// which equals the arena's at every unlock.
    pub fn young_trigger_census(&self) -> YoungTriggerCensus {
        let mut census = self.young_trigger_feedback.lock().census;
        census.trigger_bytes = self.young_gc_threshold.load(Ordering::Relaxed);
        census.semi_capacity = self.young_from_pub_capacity.load(Ordering::Relaxed);
        census.knob_percent = self.young_trigger_pct;
        census.pause_goal_ms = self.young_pause_goal_ms; // gci i4/yg: per heap
        // gci i22/ed: the CURRENT Serial eden / clock eden (the fixed ones on
        // a heap `CRATONVM_GEN_SERIAL_EDEN_INITIAL` did not arm).
        census.serial_eden_bytes = self.serial_eden_current(); // gcf f7/c
        census.age_clock_eden_bytes = self.age_clock_eden_now(); // gci i1/yg
        if self.young_age_clock_eden > 0 {
            let clock = self.age_clock.lock();
            census.age_clock_ticks = clock.ticks;
            census.age_clock_holds = clock.holds;
            census.age_clock_old_direct_bytes = clock.old_direct_total; // gci i3/yg
        }
        census
    }

    /// [`Self::young_trigger_census`] rendered as the `[GC] young_trigger:`
    /// shutdown line. Formatted here and printed by `VmHeap::print_gc_summary`
    /// (the same split `tlab::tlab_census_lines` uses).
    pub fn young_trigger_census_line(&self) -> String {
        let mut line = self.young_trigger_census().line();
        // gc wave 9 (lane gcC): the aging epoch's engagement, only when armed
        // (an unarmed heap's line is unchanged).
        if self.young_age_eden > 0 {
            line.push_str(&format!(
                " age_by_serial_eden={} age_epoch_aging_cycles={} age_epoch_held_cycles={}",
                self.young_age_eden,
                self.young_age_epoch[2].load(Ordering::Relaxed),
                self.young_age_epoch[3].load(Ordering::Relaxed),
            ));
        }
        // gci i22/ed: the initial-heap follow, only when armed
        // (`CRATONVM_GEN_SERIAL_EDEN_INITIAL`; an unarmed heap's line is
        // unchanged). `young_trigger_serial_eden=` above is the current eden.
        if self.young_serial_eden_now.load(Ordering::Relaxed) > 0 {
            let follow = *self.serial_eden_follow.lock();
            line.push_str(&format!(
                " young_trigger_serial_eden_max={} serial_eden_initial={} \
                 serial_young_gen={} serial_old_capacity={} serial_eden_grows={} \
                 serial_eden_shrinks={}",
                self.young_serial_eden,
                serial_eden_bytes_at(follow.young_initial, self.serial_survivor_ratio),
                follow.young_now,
                follow.old_capacity,
                follow.grows,
                follow.shrinks,
            ));
        }
        line
    }

    /// Run a minor (or, if old gen is full, full) garbage-collection cycle.
    ///
    /// The `_stw` parameter is type-level proof that the caller is in a
    /// stop-the-world phase — see [`cratonvm_gc_core::collector::StopTheWorldToken`].
    pub fn collect_garbage(
        &self,
        _stw: &cratonvm_gc_core::collector::StopTheWorldToken,
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
    ) -> GcResult {
        let _phase = cratonvm_jfr::phase::enter(cratonvm_jfr::phase::Category::GcPause);
        let goal = self.young_pause_goal_ms; // gci i4/yg: per heap
        // `--verbose:gc`, the JVMTI hooks and the single pause measurement —
        // see the twin in `collect_garbage_with_finalizers` for all three.
        let verbose = self.verbose_gc_begin();
        cratonvm_gc_core::gc::fire_gc_start();
        let t0 = std::time::Instant::now();
        let out = self.collect_garbage_inner(roots, &[], monitors).0;
        let pause = t0.elapsed();
        cratonvm_gc_core::gc::fire_gc_finish();
        let refused = self.young_cycle_skipped.load(Ordering::Relaxed);
        self.note_collection_pause(pause, refused);
        self.verbose_gc_end(verbose, pause.as_micros());
        // See the twin in `collect_garbage_with_finalizers`: a refused cycle
        // measured no pause, so it must not steer the pause-goal loop.
        self.feed_pause_goal_loop(pause, goal, out.stats.bytes_copied as u64, refused);
        self.external_gpu.on_gc_cycle_end();
        out
    }

}
