#[allow(unused_imports)]
use super::*;

impl GarbageCollector for ZgcRealHeap {
    fn try_alloc_object(&self, class_id: ClassId, num_fields: usize) -> Option<ObjectRef> {
        ZgcRealHeap::try_alloc_object(self, class_id, num_fields)
    }

    fn try_alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> Option<ObjectRef> {
        ZgcRealHeap::try_alloc_array(self, class_id, element_type, length)
    }

    fn alloc_object(&self, class_id: ClassId, num_fields: usize) -> ObjectRef {
        let compact_body = cratonvm_types::compact_object_total_size(
            self.layout_domain(),
            class_id.as_u32(),
            num_fields,
        );
        let total = compact_body.unwrap_or_else(|| {
            num_fields
                .checked_mul(SLOT_SIZE)
                .and_then(|b| HEADER_SIZE.checked_add(b))
                .expect("object total size overflow")
        });
        let num_slots = u32::try_from(num_fields).expect("field count exceeds u32::MAX");
        // The header is written by the INITIALIZER, i.e. before the address
        // enters the object-start registry. See `alloc_raw`.
        let ptr = self
            .alloc_raw_tlab(total, |ptr| {
                let mut header = ObjectHeader::new(
                    class_id,
                    ObjectKind::Object,
                    ArrayElementType::Reference,
                    0,
                    num_slots,
                );
                if let Some(body) = compact_body {
                    header.set_compact_shape(num_slots, body);
                }
                // SAFETY: `ptr` is a fresh zeroed span of `total >= HEADER_SIZE`
                // bytes that the allocator has not yet published.
                unsafe { std::ptr::write(ptr as *mut ObjectHeader, header) };
            })
            .unwrap_or_else(|| {
                // `alloc_raw` has already exhausted everything a GC-free
                // recovery can reach: both ends' free lists, the un-bumped
                // middle, a forced `retire_all_tlabs`, both coalesce passes and
                // the unclaimed large-object reserve (`Arena::alloc`'s
                // "high-free-list-last-resort" and "eat into the UNCLAIMED
                // reserve" arms). There is no retry to add here. The only ways
                // out are upstream -- collect EARLIER (the trigger) or be able
                // to REPORT (the trait) -- so this site's remaining job is to
                // say which failure it was.
                //
                // zr z2/cp: ...or to let the VM report it. Under a converting
                // `catch_unwind` the hook raises a catchable OutOfMemoryError
                // and never returns; otherwise the abort below stands.
                self.offer_exhaustion_unwind(num_fields, "object");
                self.report_fatal_exhaustion("object", total);
                std::process::abort();
            });
        // SAFETY: the initializer above wrote a well-formed header at `ptr`.
        let obj = unsafe { ObjectRef::from_raw(ptr) };
        // Allocate BLACK while a concurrent cycle is marking. Must follow the
        // header write; see `allocate_black_if_marking`.
        self.allocate_black_if_marking(ptr);
        obj
    }

    fn alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> ObjectRef {
        if length > ZGC_REAL_MAX_ARRAY_LENGTH {
            // zr z2/cp: HotSpot's answer is an OutOfMemoryError (the VM
            // payload names the array limit); the assert stays the fallback.
            self.offer_exhaustion_unwind(length, "array");
        }
        assert!(
            length <= ZGC_REAL_MAX_ARRAY_LENGTH,
            "array length {length} exceeds maximum {ZGC_REAL_MAX_ARRAY_LENGTH}"
        );
        let data_size = array_data_size(length, element_type).expect("array data size overflow");
        let total = HEADER_SIZE
            .checked_add(data_size)
            .expect("array total size overflow");
        let len_u32 = u32::try_from(length).expect("array length exceeds u32::MAX");
        // The header is written by the INITIALIZER, i.e. before the address
        // enters the object-start registry. See `alloc_raw`.
        let ptr = self
            .alloc_raw_tlab(total, |ptr| {
                // SAFETY: fresh zeroed, unpublished span of `total >= HEADER_SIZE`.
                unsafe {
                    std::ptr::write(
                        ptr as *mut ObjectHeader,
                        ObjectHeader::new(
                            class_id,
                            ObjectKind::Array,
                            element_type,
                            len_u32,
                            len_u32, // mirror length into num_slots, like Heap/G1/gen_heap
                        ),
                    );
                }
            })
            .unwrap_or_else(|| {
                // See the matching note in `alloc_object`.
                self.offer_exhaustion_unwind(length, "array");
                self.report_fatal_exhaustion("array", total);
                std::process::abort();
            });
        // SAFETY: the initializer above wrote a well-formed header at `ptr`.
        let obj = unsafe { ObjectRef::from_raw(ptr) };
        // Allocate BLACK while a concurrent cycle is marking. Must follow the
        // header write; see `allocate_black_if_marking`.
        self.allocate_black_if_marking(ptr);
        obj
    }

    fn get_header(&self, obj: ObjectRef) -> &ObjectHeader {
        self.header(obj)
    }

    fn class_id_of(&self, obj: ObjectRef) -> ClassId {
        self.header(obj).class_id
    }

    fn kind_of(&self, obj: ObjectRef) -> ObjectKind {
        self.header(obj).kind()
    }

    fn element_type_of(&self, obj: ObjectRef) -> ArrayElementType {
        self.header(obj).element_type()
    }

    fn identity_hash_code(&self, obj: ObjectRef) -> i32 {
        let header = self.header(obj);
        match header.identity_hash(|| match self.next_hash() {
            0 => i32::MAX,
            h => h,
        }) {
            Ok(hash) => hash,
            Err(()) => cratonvm_gc_core::collector::displaced_identity_hash(obj.as_ptr() as usize),
        }
    }

    fn get_field(&self, obj: ObjectRef, index: usize) -> Value {
        let header = self.header(obj);
        if self.check_field_index(header, index, "get").is_none() {
            return Value::Object(None);
        }
        // READ side of the same watch, so the question survives `--nojit`.
        //
        // The JIT-side punned-cell detector lives in `jit_getfield_impl`, which
        // means turning the JIT off to ask "is COMPILED CODE the writer?" also
        // turns the detector off — the experiment blinds its own instrument.
        // This arm reads through the heap accessor the interpreter uses, so
        // `CRATONVM_DBG_WATCH_PUN=<class>:<slot>` reports the punned cell with
        // or without the JIT.
        //
        // Measured 2026-08-24: the STORE-side watch is live for this class
        // (6881 firings watching `SQLChar:2`) and fired ZERO times for slot 1
        // in a run that demonstrably produced the punned cell — so the write
        // does not go through `set_field` at all, and compiled code writing the
        // 16-byte cell directly is what is left to test.
        if punned_store_watch().is_some() {
            self.report_watched_punned_read(obj, index);
        }
        if let Some((offset, storage)) = zgc_compact_field_storage(header, index) {
            let ptr = unsafe { obj.as_ptr().add(offset) };
            // zt t18/zb1 (zr1br A2a): armed, a compact REFERENCE word is read
            // through the barrier -- the slot `resolve_ref_slot` names for this
            // shape. Unarmed (every default run) this is the read it was.
            let v = if storage.is_reference()
                && self.load_barrier_armed()
                && storage.size_runtime() == 8
                && !self.load_barrier_census_bypass()
            {
                self.load_barrier_ref_value(ptr as usize)
            } else {
                unsafe { cratonvm_types::read_compact_field(ptr, storage, Ordering::Relaxed) }
            };
            if !storage.is_reference() {
                return v;
            }
            // Un-box the wrapper `set_field` installs for a non-reference value
            // stored into a declared-REFERENCE slot. This heap's `set_field`
            // used to hand such a value to `write_compact_field`, whose
            // `FieldStorageKind::Reference` arm maps every non-`Object` value
            // to raw 0 — so the write was silently dropped to null while
            // `gen_heap` boxed it and the legacy 16-byte cell kept it verbatim.
            // ZGC has been the default since 2026-08-10, so the discarding
            // answer was the one most code actually got
            // (W7-84-primitive-in-reference-store.md).
            //
            // `autobox_payload` is the same validated read the reference-ARRAY
            // path here has always used; `cratonvm_gc_core::autobox` puts it behind the
            // process-wide latch so a run that never boxes never reaches it.
            // gc-common w4-e: that screen is now per slot
            // (`unbox_reference_field`), because the boot-time mirror overlays
            // arm the process-wide one in every run
            // (common-g-autobox-latch-always-armed).
            return cratonvm_gc_core::autobox::unbox_reference_field(
                v,
                header.class_id,
                index,
                |r| {
                    self.is_object_address(r.as_ptr() as usize)?;
                    // SAFETY: `is_object_address` confirmed `r` is a registered
                    // live allocation base, so its first HEADER_SIZE bytes are
                    // a header.
                    Some(unsafe { (*(r.as_ptr() as *const ObjectHeader)).class_id })
                },
                |r| <Self as GarbageCollector>::get_field(self, r, 0),
            );
        }
        // HIB-DCAST-LATEPHASE.1 (mutator side). `compact_object_field_storage`
        // answers `None` for TWO reasons and only the first licenses the
        // fall-through: (1) "this is a legacy object" — its contract, and the
        // 16-byte `Value` cell read below is correct; (2) "this IS a compact
        // object (`GC_FLAG_COMPACT`, set at allocation) but its
        // `(class_id, num_slots)` no longer resolves to a registered layout" —
        // a redefinition that changed the field count, a foreign layout
        // domain, or a desynced walk.
        //
        // Case (2) must NOT fall through. `alloc_object` sized this body with
        // `compact_object_body_size`, and `num_slots()` on a compact object is
        // the FIELD COUNT — so the `index < num_slots` screen above does not
        // bound `HEADER_SIZE + index * SLOT_SIZE`, and an entirely in-range
        // index still strides past the allocation. `object_field_size` already
        // treats this state as "this is not that object at all" and answers
        // `IMPLAUSIBLE_BODY_SIZE`; the hazard was recognised one layer down but
        // not at this call site.
        //
        // `is_compact_object(header)` — the per-object header bit, independent
        // of the registry — is what tells the two cases apart. It is the same
        // discriminator the GC walkers and this file's own
        // `ZCensusHeapView::reference_slots` already use. The walkers SKIP such
        // an object; an accessor cannot skip, so it serves nothing and says so.
        // Deliberately not a panic: the sibling accessors in `gen_heap.rs` /
        // `g1.rs` document why (a racing redefinition must not abort the JVM).
        if cratonvm_types::is_compact_object(header) {
            tracing::warn!(
                target: "cratonvm::gc::guard",
                obj = ?obj.as_ptr(),
                index,
                class_id = ?header.class_id,
                "zgc::get_field: compact receiver has no registered layout for \
                 its (class_id, field_count) — returning null rather than \
                 striding its packed compact body as legacy 16-byte cells \
                 (HIB-DCAST-LATEPHASE.1)",
            );
            return Value::Object(None);
        }
        // zt t18/zb1 (zr1br A2a): armed, a legacy cell whose tag says Object
        // has its PAYLOAD word read through the barrier -- the word
        // `resolve_ref_slot` names for this shape, behind the same tag gate
        // (a zero-filled cell is tag 0, `Int(0)`, and is not a reference).
        // A non-Object tag falls through to the checked read below. A
        // declared-reference cell's tag moves once, 0 -> Object, at its first
        // store; a reader that saw 0 and then reads the cell after that store
        // gets its payload unbarriered -- a plain pointer in plain-store mode,
        // and a race A2b's first coloring store has to close.
        if self.load_barrier_armed() {
            let cell = obj.as_ptr() as usize + HEADER_SIZE + index * SLOT_SIZE;
            // SAFETY: `index < num_slots` on a legacy body (checked above), so
            // `cell` starts a live 16-byte cell; its tag dword is 4-aligned.
            let tag = unsafe {
                (*((cell + cratonvm_types::FIELD_CELL_TAG_OFFSET)
                    as *const std::sync::atomic::AtomicU32))
                    .load(Ordering::Relaxed)
            };
            if tag == cratonvm_types::FIELD_CELL_TAG_OBJECT && !self.load_barrier_census_bypass() {
                return self
                    .load_barrier_ref_value(cell + cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET);
            }
            // zt t19/zb2 (A2b, zb1's risk 5.3 item 1): the tag read above saw
            // a non-Object cell, but a first reference store may land before
            // the checked read below re-reads the cell. A declared-reference
            // cell's tag moves once, `0 -> Object`, so an Object from that
            // read means the store landed: its payload is a word the barrier
            // must decode (a colored one on a colored-store heap), never a
            // pointer to hand out raw. Re-read it through the barrier.
            // SAFETY: as for the read below.
            let v = unsafe {
                cratonvm_gc_core::heap::read_value_cell_checked(
                    cell as *const Value,
                    "zgc::get_field",
                )
            };
            if matches!(v, Value::Object(Some(_))) && !self.load_barrier_census_bypass() {
                return self
                    .load_barrier_ref_value(cell + cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET);
            }
            return v;
        }
        // SAFETY: index validated < num_slots, so the slot is within bounds.
        //
        // Discriminant-screened and per-word atomic, matching the other three
        // legacy-cell readers. This site was the least guarded of the four — a
        // bare non-atomic `std::ptr::read::<Value>`, so it both tore against a
        // concurrent plain writer and transmuted a corrupt cell into a `Value`
        // that is UB to match on. See `heap::read_value_cell_checked`.
        unsafe {
            let ptr = obj.as_ptr().add(HEADER_SIZE + index * SLOT_SIZE);
            cratonvm_gc_core::heap::read_value_cell_checked(ptr as *const Value, "zgc::get_field")
        }
    }

    fn set_field(&self, obj: ObjectRef, index: usize, value: Value) {
        // ---- SATB, at the accessor -----------------------------------------
        //
        // Same argument as `set_array_element`'s: `putfield` and every native
        // `NativeContext::set_field` publish the overwritten reference at the
        // call site, but this accessor is also reachable directly (VM
        // internals, reflection helpers, `Unsafe`), and a reference lost there
        // during a concurrent cycle is a use-after-free rather than a missed
        // optimisation. Publishing here makes the coverage a property of the
        // ONE store path instead of a property of a call-site census that has
        // to stay complete forever.
        //
        // The one caller that must NOT publish has its own entry point --
        // see `set_field_no_satb`, and do not fold the two back together.
        //
        // Cost when nothing is marking: one relaxed load.
        if self.mark_active.load(Ordering::Relaxed) {
            // zt t32 (2f): under load-barrier marking only a plain word's
            // deletion is logged, read raw (`lbm_pre_barrier`).
            if self.load_barrier_marking() {
                self.lbm_pre_barrier(obj, index);
            } else if let Value::Object(Some(old)) = self.get_field(obj, index) {
                self.satb_pre_barrier(old.as_ptr() as usize);
            }
        }
        self.set_field_no_satb(obj, index, value);
    }

    fn get_field_volatile(&self, obj: ObjectRef, index: usize) -> Value {
        let _guard = cratonvm_gc_core::collector::volatile_stripe_lock(obj, index);
        std::sync::atomic::fence(Ordering::SeqCst);
        let v = self.get_field(obj, index);
        std::sync::atomic::fence(Ordering::SeqCst);
        v
    }

    fn set_field_volatile(&self, obj: ObjectRef, index: usize, value: Value) {
        let _guard = cratonvm_gc_core::collector::volatile_stripe_lock(obj, index);
        std::sync::atomic::fence(Ordering::SeqCst);
        self.set_field(obj, index, value);
        std::sync::atomic::fence(Ordering::SeqCst);
    }

    fn array_length(&self, obj: ObjectRef) -> usize {
        let header = self.header(obj);
        debug_assert_eq!(header.kind(), ObjectKind::Array, "not an array");
        header.array_length() as usize
    }

    fn get_array_element(&self, obj: ObjectRef, index: usize) -> Result<Value, i32> {
        let header = self.header(obj);
        if header.kind() != ObjectKind::Array {
            return Err(cratonvm_gc_core::heap::oob_index_code(index));
        }
        if index >= header.array_length() as usize {
            return Err(cratonvm_gc_core::heap::oob_index_code(index));
        }
        let element_type = header.element_type();
        // ---- THE LOAD BARRIER, on a real read path (Phase 4) -------------
        //
        // A reference element goes through `load_barrier_slot`, which forwards
        // a relocated offset, publishes to the marker, and self-heals the slot
        // — but ONLY once the good mask says slots are coloured, which nothing
        // in a default run does. Until then it is one relaxed load and a
        // compare, and the element is read exactly as before.
        //
        // This arm exists because `read_prim_element`'s own Reference branch
        // applies `plausible_heap_pointer` and degrades an implausible word to
        // `Object(None)` — sites 6 and 7 of
        // `gc/tests/zgc_colored_word_degradation.rs`. A coloured word IS
        // deliberately implausible, so a coloured reference array read through
        // that branch silently nulls every live element. Barriering first is
        // how this backend goes through the barrier rather than around it;
        // that arm is shared with Generational and G1 and must not be edited.
        // zr z1/br: the element address comes from the slot resolver. It was
        // `ARRAY_DATA_OFFSET + index * SLOT_SIZE` -- the 16-byte LEGACY CELL
        // stride, while a reference element is 8 bytes wide -- so every index
        // above 0 read and healed the wrong word, past the array's end for
        // `index >= len / 2`. The only test armed index 0.
        let armed_slot = if element_type == ArrayElementType::Reference
            && self.load_barrier_armed()
            && !self.load_barrier_census_bypass()
        {
            self.resolve_ref_slot(obj, index)
        } else {
            None
        };
        if let Some(slot) = armed_slot {
            let slot_addr = slot as usize;
            let val = match self.load_barrier_slot(slot_addr) {
                None => Value::Object(None),
                // SAFETY: the barrier returned a machine address it resolved
                // from a live slot; `from_raw` is the same construction the
                // unbarriered path performs.
                Some(addr) => Value::Object(Some(unsafe { ObjectRef::from_raw(addr as *mut u8) })),
            };
            // Latched: `autobox_payload`'s first act is `is_object_address`,
            // a registry probe, and it can only answer `Some` if a wrapper was
            // ever created — which is what sets the latch. See
            // `cratonvm_gc_core::autobox::array_read_may_hold_wrapper`; the FIELD read
            // paths have been latched since the latch was introduced and this
            // one was not.
            if cratonvm_gc_core::autobox::array_read_may_hold_wrapper() {
                if let Value::Object(Some(boxed)) = val {
                    if let Some(inner) = self.autobox_payload(boxed) {
                        return Ok(inner);
                    }
                }
            }
            return Ok(val);
        }
        // SAFETY: bounds check passed; data area starts at base + HEADER_SIZE.
        let val = unsafe {
            let base = obj.as_ptr().add(ARRAY_DATA_OFFSET);
            read_prim_element(base, index, element_type)
        };
        // Un-box the wrapper `set_array_element` installs for a non-Object
        // value stored into a reference array — see `autobox_payload`, and
        // `G1Collector::get_array_element` / `GenerationalHeap::
        // get_array_element_unboxing` for the same read. A Java-level
        // `Object[]` never holds an `AUTOBOX_CLASS_ID` object, so this cannot
        // change what an `aaload` observes.
        if element_type == ArrayElementType::Reference {
            // Latched: `autobox_payload`'s first act is `is_object_address`,
            // a registry probe, and it can only answer `Some` if a wrapper was
            // ever created — which is what sets the latch. See
            // `cratonvm_gc_core::autobox::array_read_may_hold_wrapper`; the FIELD read
            // paths have been latched since the latch was introduced and this
            // one was not.
            if cratonvm_gc_core::autobox::array_read_may_hold_wrapper() {
                if let Value::Object(Some(boxed)) = val {
                    if let Some(inner) = self.autobox_payload(boxed) {
                        return Ok(inner);
                    }
                }
            }
        }
        Ok(val)
    }

    fn set_array_element(&self, obj: ObjectRef, index: usize, value: Value) -> Result<(), i32> {
        self.audit_access_receiver(obj.as_ptr() as usize, index, "set_array_element");
        let header = self.header(obj);
        if header.kind() != ObjectKind::Array {
            return Err(cratonvm_gc_core::heap::oob_index_code(index));
        }
        if index >= header.array_length() as usize {
            return Err(cratonvm_gc_core::heap::oob_index_code(index));
        }
        let element_type = header.element_type();
        // ---- SATB, at the accessor rather than only at the call site ------
        //
        // `aastore` publishes the overwritten reference itself
        // (`opcodes.rs` -> `VmHeap::satb_barrier`), and so does every native
        // `NativeContext::set_field`. **`System.arraycopy` does not.** It
        // copies a reference array one element at a time through
        // `NativeContext::set_array_element`, which lands here with no
        // pre-barrier anywhere above it -- so a concurrent cycle that relied
        // on the call sites alone would lose every reference an arraycopy
        // overwrote, and the sweep would free objects that are still
        // referenced from the source array.
        //
        // Publishing here instead of hunting call sites is the
        // convert-the-idiom fix: this accessor is the ONE place every
        // reference-array store in the VM passes through, and a duplicate
        // publication from a call site that also barriers costs nothing
        // (`push_roots` dedups on the mark bit).
        //
        // Cost when nothing is marking: one relaxed load, on
        // reference-element stores only.
        if element_type == ArrayElementType::Reference && self.mark_active.load(Ordering::Relaxed) {
            // zt t32 (2f): see `set_field`.
            if self.load_barrier_marking() {
                self.lbm_pre_barrier(obj, index);
            } else if let Ok(Value::Object(Some(old))) = self.get_array_element(obj, index) {
                self.satb_pre_barrier(old.as_ptr() as usize);
            }
        }
        // SAFETY: bounds check passed; data area starts at base + HEADER_SIZE.
        unsafe {
            let base = obj.as_ptr().add(ARRAY_DATA_OFFSET);
            if element_type == ArrayElementType::Reference {
                // zt t19/zb2 (A2b): a colored-store heap writes the element as
                // the good-colored word (the slot `resolve_ref_slot` names for
                // an 8-byte reference array). Off: the store it was.
                let colored =
                    self.load_barrier_colored_stores() && cratonvm_types::ref_element_size() == 8;
                match value {
                    Value::Object(o) if colored => {
                        let word = self.colored_store_word(o.map_or(0, |r| r.as_ptr() as usize));
                        (*(base.add(index * 8) as *const AtomicU64)).store(word, Ordering::Relaxed);
                    }
                    Value::Object(o) => {
                        // JIT round 23 wave 2, lane L4: a coloured word while
                        // the load barrier is armed with a flipped good color
                        // (`armed_store_ref_element`); plain-store arming
                        // (zt A2a) keeps the plain store.
                        if !self.armed_store_ref_element(obj, index, o) {
                            write_prim_element(base, index, element_type, value);
                        }
                    }
                    other => {
                        // Auto-box non-Object values into a 1-field wrapper,
                        // matching `Heap::set_array_element`.
                        //
                        // FALLIBLE on purpose. `alloc_object` prints
                        // `FATAL: ZGC(real): … cannot be served` and calls
                        // `std::process::abort()`, so a heap-full auto-box
                        // killed the process where Java semantics call for an
                        // `OutOfMemoryError` the program can catch — and the
                        // store that triggered it is a native copying
                        // primitives into an `Object[]`, which is the LAST
                        // allocation before a heap fills, not the first. The
                        // element keeps its old value on this arm; nothing is
                        // half-written. See `ARRAY_STORE_OUT_OF_MEMORY`.
                        let Some(wrapper) =
                            self.try_alloc_object(cratonvm_gc_core::heap::AUTOBOX_CLASS_ID, 1)
                        else {
                            return Err(cratonvm_gc_core::heap::ARRAY_STORE_OUT_OF_MEMORY);
                        };
                        self.set_field(wrapper, 0, other);
                        // Arm the process-wide wrapper latch — see the matching
                        // note in `GenerationalHeap::set_array_element`.
                        cratonvm_gc_core::autobox::note_wrapper_created();
                        // Re-fetch base: alloc_object cannot move existing
                        // objects (non-moving heap), so `base` is still valid,
                        // but reads are clearer with the explicit comment.
                        if colored {
                            let word = self.colored_store_word(wrapper.as_ptr() as usize);
                            (*(base.add(index * 8) as *const AtomicU64))
                                .store(word, Ordering::Relaxed);
                        } else if !self.armed_store_ref_element(obj, index, Some(wrapper)) {
                            write_prim_element(
                                base,
                                index,
                                element_type,
                                Value::Object(Some(wrapper)),
                            );
                        }
                    }
                }
            } else {
                write_prim_element(base, index, element_type, value);
            }
        }
        // ---- CARD BARRIER, on reference-element stores --------------------
        //
        // The other half of the hole documented in `set_field_no_satb`. A
        // reference stored into an array of an old object needs a card for
        // exactly the reason a field does, and `aastore` reaches `write_barrier`
        // no more than `putfield` does. `System.arraycopy` is the case that
        // makes call-site coverage hopeless -- it copies a reference array one
        // element at a time straight through this accessor -- which is the same
        // argument the SATB publication above makes.
        //
        // Gated on the element type as well as on the arming bit: a primitive
        // element cannot be an old-to-young edge.
        // zr z2/g2: per element, so a large old array is carded per granule
        // under `CRATONVM_ZGC_GEN_ELEMENT_CARDS=1` (the object card otherwise,
        // exactly as `note_ref_store`). Same one relaxed load as before while
        // nothing is old -- the default (non-generational) path never gets
        // past it.
        if element_type == ArrayElementType::Reference
            && self.has_old_objects.load(Ordering::Relaxed)
        {
            let obj_addr = obj.as_ptr() as usize;
            let len = self.header_ref(obj.as_ptr()).array_length() as usize;
            let slot = obj_addr + ARRAY_DATA_OFFSET + index * cratonvm_types::ref_element_size();
            self.note_ref_store_element_slow(obj_addr, slot, len);
        }
        Ok(())
    }

    fn needs_gc(&self) -> bool {
        let a = self.allocated.load(Ordering::Relaxed);
        // DBG: `CRATONVM_DBG_GC_STRESS=<bytes>` — collect every `<bytes>` of
        // allocation, bypassing every occupancy predicate below.
        //
        // **This backend ignored the flag entirely until 2026-08-14**, which
        // made the tree's standard GC-stress lever inert on its DEFAULT
        // collector: `gen_heap` honours it, ZGC did not, so a repro command
        // copied from any handoff page silently ran an ordinary workload. It
        // is how a moving-collector defect that needs many cycles to surface
        // gets one cycle in a suite class and hides.
        //
        // Bypassing `gc_rearm` is the point and is safe because this is a
        // diagnostic: the floor exists to stop a cycle-per-allocation storm
        // against a live set parked above the threshold, and storming is
        // exactly what the operator asked for.
        if let Some(step) = cratonvm_gc_core::gc_flags().gc_stress_bytes {
            if step > 0 {
                let last = self.counters.gc_stress_mark.load(Ordering::Relaxed);
                if a.saturating_sub(last) >= step {
                    self.note_trigger(TRIGGER_BIT_STRESS, &self.counters.trigger_stress);
                    return true;
                }
            }
        }
        // ---- zt t21/zc2: THE CYCLE HAS NOT ENDED (opt-in) -------------------
        //
        // `CRATONVM_ZGC_CONCURRENT_SWEEP`: while a parked sweep has not
        // installed, the last cycle is still running (its concurrent sweep), so
        // no rule asks for the next one -- HotSpot's driver starts no cycle
        // inside another. The hook re-arms every gauge this function reads; an
        // allocation that fails meanwhile finishes the sweep (`alloc_raw`).
        // One relaxed load.
        if self.counters.csweep.pending() {
            return false;
        }
        // External GPU VRAM retained by live Java `GpuArray`/`GpuFuture` wrappers:
        // evaluated before `timer.interval_only()` and `conc_every_trigger` so an
        // idle Java heap holding large off-heap device buffers still triggers
        // cleanly at safepoints and `safe_native_call` boundaries without storming.
        if !self.conc_cycle_active.load(Ordering::Relaxed) && self.external_gpu.should_trigger_gc()
        {
            return true;
        }
        // ---- zt t10/tg3: `-XX:+ZCollectionIntervalOnly` (default ON, `=0`) -
        //
        // Every clause below stands down: the `Timer` tick and an allocation
        // that fails are the only starts (`set_collection_interval_only`).
        // One relaxed load.
        if self.counters.timer.interval_only() {
            return false;
        }
        // ---- zr z3/cs: EVERY TRIGGER OPENS A CONCURRENT CYCLE (opt-in) ----
        //
        // `CRATONVM_ZGC_CONC_EVERY_TRIGGER=1`. While a cycle is open or
        // requested, this gate answers: NO while the marker traces (or the
        // request waits for its opener), YES at the mark's fixed point, at the
        // urgent reserve, or when the request lapsed. Otherwise the clauses
        // below decide, and each early yes goes through
        // `conc_every_trigger_early`, which turns it into a request when the
        // heap can finish a mark. One relaxed load with the switch off.
        if self.conc_every_trigger_armed() {
            if let Some(answer) = self.conc_every_trigger_gate(a) {
                return answer;
            }
            // zt t26/zc9 (`CRATONVM_ZGC_CONCURRENT_RATE_START`, opt-in; in the
            // profile): no cycle open or requested -- ask for one at HotSpot's
            // allocation-rate point, before a clause below fires where the
            // heap can no longer finish a mark (`ct_no_headroom`: a
            // stop-the-world cycle). One relaxed load with the switch off.
            if self.conc_rate_start_request(a) {
                return false;
            }
        }
        // Two independent reasons to collect, behind one shared anti-storm
        // floor:
        //
        //   * `a >= gc_threshold` — the classic LIVE-BYTES trigger.
        //   * `headroom_low`      — the arena can no longer serve a
        //                           `zgc_headroom_margin` request. On a heap
        //                           that never compacts this is the constraint
        //                           that actually binds, and it can be reached
        //                           with live bytes far below the threshold:
        //                           measured at 66.8% on `ZipContentTests`
        //                           while an 8 KB allocation was failing.
        //
        // Both keep the `gc_rearm` floor, so neither can fire a cycle per
        // allocation against a live set parked above the threshold.
        //
        // ---- AND THAT SHARED FLOOR CAN SILENCE THE HEADROOM CLAUSE -------
        //
        // The two reasons do not have the same relationship to `gc_rearm`, and
        // pairing them with one `&&` is what makes the second unreachable in
        // exactly the shape it was written for.
        //
        // `gc_rearm` is `live + max(headroom/4, 64 KiB)`, recomputed from the
        // POST-SWEEP live figure. `a` is LIVE BYTES. `headroom_low` is about
        // ALLOCATABLE SPACE -- the arena's bump cursor and the shape of its
        // free list -- and on a non-compacting heap those two numbers come
        // apart without bound. A 2 GiB heap holding 10 MiB live after its last
        // cycle sets `gc_rearm` to ~507 MiB; the bump cursor then runs to
        // capacity over a fragmented free list, `alloc_raw` raises
        // `headroom_low`, and this clause still answers NO because 10 MiB of
        // live data is nowhere near 507 MiB. The collector then does not
        // collect until an allocation actually FAILS. What that costs depends
        // on WHICH entry point asks. The ordinary Java `new` / `newarray` goes
        // through `alloc_object_shared` / `gc_alloc_array`, whose ladder ends
        // in a catchable `OutOfMemoryError`; the infallible
        // `GarbageCollector::alloc_object` / `alloc_array` end in
        // `std::process::abort()` instead, and ~55 VM-internal call sites still
        // reach them (`handoff-p-alloc-object-must-be-able-to-fail.md`).
        //
        // MEASURED 2026-09-21, and it did not go the way this comment used to
        // claim: every exhaustion shape the orchestrator could construct --
        // gradual growth at -Xmx64m and -Xmx128m, the non-compacting arm, the
        // bypass-on arm, one over-sized request, and a fragmenting workload
        // that pushed 946 MB through a 48 MiB heap -- THREW, at ~63 MiB of a
        // 64 MiB heap. Near-full occupancy, no abort. So the "OOM with a
        // mostly-free heap" framing is NOT established for this clause, and
        // `trigger_headroom_starved` beside it exists to find out whether the
        // state even occurs. See `gap-p-headroom-flip-criteria.md`.
        //
        // The floor is NOT simply wrong for this clause, which is why the
        // bypass is opt-in rather than a straight removal. `headroom_low` is
        // cleared at the end of every collection and re-raised by the next
        // allocation that finds the arena short, so on a heap that is
        // genuinely full of LIVE data -- where a cycle frees nothing --
        // removing the floor is a collection per allocation, which is the
        // precise storm `gc_rearm` exists to prevent.
        //
        // `CRATONVM_ZGC_HEADROOM_BYPASSES_REARM=1` takes the narrow version:
        // the headroom clause may skip the floor, but only while the LAST
        // collection actually reclaimed something. "The previous cycle freed
        // nothing" is the evidence that collecting again is futile, and it
        // needs no tuning parameter and cannot be wrong about the direction --
        // the same argument `gen_force_major_next` makes one clause over.
        // Default OFF: it changes how often the default collector runs, and
        // this tree has shipped exactly that on an argument before and
        // reverted it. See
        // `docs/feature-designs/zgc-architecture.md`
        // for the measurement that would earn the flip.
        // ---- WARM-UP (HotSpot ZGC `rule_warmup`, r16 zgcarray ZA2) ------
        //
        // Before the third collection: collect at 10, 20, 30% occupancy. See
        // `ZGC_WARMUP_CYCLES` for what waiting for the 75% threshold cost.
        // Behind the same `gc_rearm` floor as the threshold clause, so a live
        // set already above the warm-up step cannot storm.
        //
        // zr z1/tr: the base is the SOFT maximum, as HotSpot's rule has it
        // (`-XX:SoftMaxHeapSize`, which defaults to the capacity, so the
        // default is unchanged), and the capacity is read without the arena
        // lock: `heap_capacity` used to take it, so before the third cycle
        // every poll that cleared the `gc_rearm` floor (which is `0` before the
        // first) serialised on the allocator's lock.
        {
            // zr zs s1/tp: warm-up cycles, not collections
            // (`warmup_cycles_done`).
            let done = self.warmup_cycles_done();
            // zr zs s1/tp: the floor is at most half a warm-up step of progress
            // (`warmup_rearm_floor`; `CRATONVM_ZGC_WARMUP_HOTSPOT_STEPS=0`
            // restores `gc_rearm`).
            if done < ZGC_WARMUP_CYCLES
                && self.counters.warmup_enabled.load(Ordering::Relaxed)
                && a >= (self.soft_max_capacity() / 10).saturating_mul(done + 1)
                && a >= self.warmup_rearm_floor()
            {
                self.note_trigger(TRIGGER_BIT_WARMUP, &self.counters.trigger_warmup);
                return self.conc_every_trigger_early(a);
            }
        }
        // ---- zr z1/tr: HOTSPOT'S DIRECTOR RULES (`director.rs`) -----------
        //
        // Allocation rate, proactive, high usage. Inert (one relaxed load of a
        // plain bool) unless `CRATONVM_ZGC_RATE_TRIGGER=1` (they decide) or
        // `CRATONVM_ZGC_TRIGGER_SHADOW=1` (they only record when they WOULD
        // have, for the shadow line). Between samples a poll is two relaxed
        // loads and a compare against a published byte threshold.
        //
        // zr zs s1/tp: behind the `gc_rearm` floor too
        // (`CRATONVM_ZGC_DIRECTOR_REARM_FLOOR`, default on, `=0` kill). This
        // comment used to say the rules need no floor because "the rate rule
        // is a rate and cannot re-fire without new allocation". Its BYTE
        // threshold can: after a cycle the first poll re-samples, and when the
        // post-collection free space is already below a rule's reserve (high
        // usage above 95% live, the rate rule wherever `max_rate x (cycle +
        // 0.1 s)` exceeds what the cycle left free) `evaluate` answers at once
        // and the next collection follows with no progress at all. A held yes
        // falls through to the clauses below; the floor is re-published by
        // every collection, so it delays, never suppresses.
        if let Some(rule) = self.counters.director.poll(a as u64) {
            // zs s2/rv: not the proactive rule. It fires on 10 % of soft max
            // of growth since the last cycle, or 5 minutes after it: it cannot
            // re-fire with zero progress, so the floor prevented no storm and
            // only moved HotSpot's own growth point to `gc_rearm`.
            let floored = rule != director::DirectorRule::Proactive
                && self.counters.director_rearm_floor.load(Ordering::Relaxed)
                && a < self.gc_rearm.load(Ordering::Relaxed);
            if !floored {
                let bit = match rule {
                    director::DirectorRule::AllocationRate => TRIGGER_BIT_RATE,
                    director::DirectorRule::Proactive => TRIGGER_BIT_PROACTIVE,
                    director::DirectorRule::HighUsage => TRIGGER_BIT_HIGH_USAGE,
                };
                if self.claim_trigger(bit) {
                    self.counters
                        .trigger_director
                        .fetch_add(1, Ordering::Relaxed);
                } else if bit == TRIGGER_BIT_PROACTIVE {
                    // zt t21/tg10: the bit was set already (by the tick's
                    // request, or an earlier poll): mark the co-claim.
                    self.note_proactive_poll_co_claim();
                }
                return self.conc_every_trigger_early(a);
            }
            if self.claim_trigger(TRIGGER_BIT_DIRECTOR_DEFERRED) {
                self.counters
                    .trigger_director_deferred
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        let rate_deciding = self.counters.director.is_deciding();
        let headroom_now = self.headroom_low.load(Ordering::Relaxed);
        let rearm = self.gc_rearm.load(Ordering::Relaxed);
        let bypass_enabled = self.headroom_bypasses_rearm.load(Ordering::Relaxed);
        let reclaimed_nothing = self.last_cycle_reclaimed_nothing.load(Ordering::Relaxed);
        let headroom_bypass = headroom_now && bypass_enabled && !reclaimed_nothing;
        // Under `CRATONVM_ZGC_RATE_TRIGGER=1` the live-bytes threshold stands
        // down (the director's rate and high-usage rules replace it); the
        // headroom clause stays as the contiguous-space backstop the rate rule
        // cannot see.
        // zt t1/tp: `pacing_threshold` is `gc_threshold` unless
        // `CRATONVM_ZGC_SOFT_MAX_PACES` (default ON) lowered it to the soft
        // maximum's.
        let over_threshold = !rate_deciding && a >= self.pacing_threshold();
        let floor_admits = a >= rearm && (over_threshold || headroom_now);
        // zr z6/tp: the near-full floor's engagement gauge -- an interval in
        // which the legacy `free / 4` floor would already have admitted this
        // collection and the near-full floor held it back. Once per interval
        // (the trigger mask), and a relaxed load plus a compare per poll only
        // while an occupancy clause is waiting on the floor.
        if !floor_admits
            && (over_threshold || headroom_now)
            && a >= self.counters.gc_rearm_legacy.load(Ordering::Relaxed)
            && self.claim_trigger(TRIGGER_BIT_PACE_DEFERRED)
        {
            self.counters
                .pacing
                .deferred
                .fetch_add(1, Ordering::Relaxed);
        }
        // ---- THE FLIP EVIDENCE, CHARGED ON BOTH ARMS ---------------------
        //
        // Read `ZgcCounters::trigger_headroom_starved` for why `trigger_headroom`
        // beside it cannot answer the question these three exist for. The short
        // version: that tally counts collections that happened; these count the
        // divergence between the two quantities `needs_gc` pairs with one `&&`,
        // which is what the flag is about.
        //
        // Cost: reached only when `headroom_low` is already true — i.e. after
        // the arena has gone short of contiguous space, which on a healthy run
        // never happens at all — and then a mask test and a not-taken branch
        // for every poll after the first in the interval.
        if headroom_now && !floor_admits {
            // The counterfactual, and the ONLY one of the three that is
            // readable in the DEFAULT (flag-off) arm. Charging it does not
            // depend on `bypass_enabled`: the question "would the bypass have
            // had anything to do here?" has to be answerable without turning
            // the bypass on, or the flip decision needs the very A/B it is
            // supposed to justify.
            if self.claim_trigger(TRIGGER_BIT_HEADROOM_BLOCKED) {
                self.counters
                    .trigger_headroom_starved
                    .fetch_add(1, Ordering::Relaxed);
                // HOW FAR below the floor, so "the two numbers come apart
                // without bound" is a measurement rather than an argument.
                // `fetch_max` and not a store: the interesting figure is the
                // worst interval in the run, and a later, smaller interval
                // must not overwrite it.
                self.counters
                    .trigger_headroom_starved_gap
                    .fetch_max(rearm.saturating_sub(a), Ordering::Relaxed);
            }
            // ...and, with the flag ON, why the bypass did not fire anyway.
            // Only `last_cycle_reclaimed_nothing` can produce this arm, since
            // `headroom_now && bypass_enabled && !reclaimed_nothing` is
            // `headroom_bypass`, which would have admitted.
            if bypass_enabled && reclaimed_nothing {
                self.note_trigger(
                    TRIGGER_BIT_HEADROOM_BYPASS_SUPPRESSED,
                    &self.counters.trigger_headroom_bypass_suppressed,
                );
            }
        }
        if floor_admits || headroom_bypass {
            // The engagement gauge for the `=1` arm: this collection is
            // happening ONLY because the floor was skipped. `!floor_admits` is
            // the whole of it — with it, the cycle would have run either way
            // and counting it would inflate the gauge with collections the
            // flag did not cause.
            if headroom_bypass && !floor_admits {
                self.note_trigger(
                    TRIGGER_BIT_HEADROOM_BYPASS,
                    &self.counters.trigger_headroom_bypass,
                );
            }
            // Both halves are recorded, and deliberately not as an
            // either/or: a cycle can satisfy the live-bytes threshold AND
            // be short of contiguous headroom, and which one is driving a
            // workload is the whole question when the collection COUNT is
            // what differs between two arms.
            //
            // Charged once per COLLECTION, not once per agreeing poll -- see
            // `ZgcCounters::trigger_seen` for the arithmetic this used to
            // report instead.
            if over_threshold {
                self.note_trigger(TRIGGER_BIT_THRESHOLD, &self.counters.trigger_threshold);
            }
            if headroom_now {
                self.note_trigger(TRIGGER_BIT_HEADROOM, &self.counters.trigger_headroom);
            }
            // zr z3/cs: the headroom clause too -- `headroom_low` is about
            // contiguous space, which the free-space test inside cannot see,
            // but an allocation that then fails collects through its own door
            // whatever this answers (`abandoned_stw`), which is HotSpot's
            // allocation-stall fallback.
            return self.conc_every_trigger_early(a);
        }

        // ---- F1: ENOUGH HAS BEEN ALLOCATED SINCE THE LAST CYCLE ----------
        //
        // The clause above is 75% of `-Xmx` and the one beside it is "the arena
        // cannot serve a request"; neither asks how much GARBAGE there is. At
        // `-Xmx2g` with 50 MiB live the first fires at 1.5 GiB of `allocated`,
        // so a cycle let ~1.45 GiB accumulate before running and then paid for
        // it: the registry, the mark bitmap and the sweep all cover the span
        // the bump cursor ran over. Pause work scaled with the heap FLAG, and
        // doubling `-Xmx` doubled every pause on a workload whose live set had
        // not changed.
        //
        // Two relaxed loads and a compare on the miss; `a` is already in hand.
        // See `zgc_alloc_trigger_percent` for the budget, the switch, and why
        // this clause needs no `gc_rearm` floor.
        let mut alloc_budget = self.alloc_trigger_bytes.load(Ordering::Relaxed);
        if alloc_budget == 0
            && !self.generational_enabled()
            && self.conc_every_trigger_armed()
            && self.pause_target_ms.load(Ordering::Relaxed) > 0
            && self.gc_count.load(Ordering::Relaxed) >= ZGC_WARMUP_CYCLES
            && self.heap_capacity() > 4 * 1024 * 1024 * 1024
        {
            alloc_budget = (self.heap_capacity() * 17 / 32).min(8704 * 1024 * 1024);
        }
        if alloc_budget > 0
            && a.saturating_sub(self.cycle_alloc_watermark.load(Ordering::Relaxed)) >= alloc_budget
        {
            // One charge per collection, on the same latch as the clauses
            // above. `alloc_trigger_fires` is the F1 engagement gauge on the
            // `zgc-pause` line and `trigger_alloc_budget` the reason tally on
            // the `zgc-trigger` line; they answer the same question for two
            // readers, so they move together -- which is why they share ONE
            // latch test rather than calling `note_trigger` twice, where the
            // first call would claim the bit and the second would see it taken
            // and leave its counter permanently at zero.
            if self.claim_trigger(TRIGGER_BIT_ALLOC_BUDGET) {
                self.counters
                    .alloc_trigger_fires
                    .fetch_add(1, Ordering::Relaxed);
                self.counters
                    .trigger_alloc_budget
                    .fetch_add(1, Ordering::Relaxed);
            }
            return self.conc_every_trigger_early(a);
        }

        // ---- G2d: THE NURSERY IS FULL ------------------------------------
        //
        // Both clauses above are about the WHOLE HEAP: `gc_threshold` is live
        // bytes and `headroom_low` is allocatable space. Neither ever asks "has
        // enough been allocated since the last collection to be worth a young
        // cycle?", so before this a young cycle happened only when a full
        // collection would have — i.e. young cycles were exactly as rare as the
        // collections they were meant to replace, and the generational split had
        // no opportunity to pay for itself. The 2026-08-17 measurement saw six
        // young cycles in a 600-round run for that reason.
        //
        // `allocated - watermark` is the nursery's size: `allocated` is already
        // incremented on the allocation path and already loaded above, so this
        // costs two relaxed loads and two compares on the miss.
        //
        // # Why this cannot storm, without consulting `gc_rearm`
        //
        // It deliberately does NOT go through the `gc_rearm` floor, because that
        // floor is a quarter of remaining headroom and is therefore much larger
        // than a nursery budget — ANDing them would make this clause
        // unreachable. It does not need the floor: the watermark is reset at the
        // end of every collection, so firing requires `budget` bytes of genuinely
        // NEW allocation each time. A live set parked above the threshold cannot
        // re-trigger it, which is the one thing `gc_rearm` exists to prevent.
        if !self.generational_enabled.load(Ordering::Relaxed) {
            return false;
        }
        let budget = self.gen_nursery_bytes.load(Ordering::Relaxed);
        if budget == 0 {
            return false; // the kill switch, see `zgc_gen_nursery_percent`
        }
        if a.saturating_sub(self.cycle_alloc_watermark.load(Ordering::Relaxed)) >= budget {
            // zt t20/gn7 (D7, `CRATONVM_ZGC_GEN_NURSERY_AFTER_WARMUP`, default
            // ON in the mode, `=0` kill): NOT BEFORE THE WARM-UP STEPS ARE
            // SPENT. HotSpot 25's director runs its warm-up majors at 10, 20
            // and 30 % of the soft maximum (`rule_major_warmup`, checked before
            // any minor rule) and its allocation-rate rule weighs free space
            // against the measured rate, so a fresh heap gets `Warmup` majors
            // first. This budget is a tenth of the heap of NEW allocation, so
            // after the first warm-up step it fired at ~10 % + live, before
            // the 20 % step, and with nothing old yet that cycle is
            // whole-heap: `Zt16Gn6JmxYoungPool` (gn6_JY_log) read `Warmup`,
            // two `Major Collection (Allocation Rate)`, `Warmup`, then the
            // minor, where HotSpot reads three `Warmup` majors and the minor.
            // The warm-up clause above owns these intervals; the threshold
            // and headroom clauses still serve a heap that fills.
            if self
                .counters
                .gen_nursery_after_warmup
                .load(Ordering::Relaxed)
                && self.counters.warmup_enabled.load(Ordering::Relaxed)
                && self.warmup_cycles_done() < ZGC_WARMUP_CYCLES
            {
                if !self
                    .counters
                    .gen_nursery_warmup_held_latch
                    .swap(true, Ordering::Relaxed)
                {
                    self.counters
                        .gen_nursery_warmup_held
                        .fetch_add(1, Ordering::Relaxed);
                }
                return false;
            }
            self.gen_nursery_triggered.store(true, Ordering::Relaxed);
            // Attribution only (the nursery has its own tally,
            // `gen_nursery_triggers`): the bit names the cause.
            self.claim_trigger(TRIGGER_BIT_NURSERY);
            return true;
        }
        false
    }

    fn collect_garbage(
        &self,
        _stw: &StopTheWorldToken,
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
    ) -> GcResult {
        // zt t34: whatever this pause does, the cycle it closes books no more
        // into the R6 plan's liveness once it ends.
        let _crl_liveness_end = self.counters.crl.live.pause_end_guard();
        // ---- THE JFR PHASE SPAN FOR THIS PAUSE ---------------------------
        //
        // `cratonvm_jfr::phase`'s whole-run reconciliation identity
        // (`docs/observability/phase-accounting.md` §10.9) charges each thread's
        // wall time to one of 16 categories and reports the unattributed
        // remainder. Until this line NO collector in the tree opened a
        // `Category::GcPause` or `Category::GcConcurrent` span -- not `g1.rs`,
        // not `gen_heap.rs`, not this one -- so every collection's time landed
        // in whatever executor span enclosed it (`JavaExecution` in a normal
        // run) and the identity was uniformly blind to GC. That is worse than
        // a missing column: `JavaExecution` is the denominator a throughput
        // claim is read against, so GC time was being counted as application
        // time.
        //
        // The span nests, and `close_span` charges SELF time with the child's
        // total handed back to the parent, so opening it here moves this
        // collection out of the enclosing executor category rather than adding
        // to the total -- the identity still balances.
        //
        // `GcPause` and not `GcConcurrent` for the whole function, deliberately,
        // and it is the same call `ZgcMetrics::phase_was_stw` makes: the entire
        // body runs inside one `StopTheWorldToken`. A concurrent mark that ran
        // before this pause is charged on ITS OWN threads, where it actually
        // spent the time; `finish_concurrent_mark` below only joins those
        // workers, and the joining IS pause time.
        //
        // Not a `ZgcPhaseGuard`: that guard also records into `ZgcMetrics` on
        // drop, and `record_metrics_for_cycle` already feeds the module from
        // `ZPhaseClock`'s laps. Two recorders over one cycle is the defect the
        // metrics adoption existed to remove.
        let _gc_phase = cratonvm_jfr::phase::enter(cratonvm_jfr::phase::Category::GcPause);
        // zs s3/ac: the COLLECTING thread's CPU across this whole collection
        // (`pause_thread_ms=` on `[GC] zgc-cpu:`), closed at the bottom of this
        // function, which has no early return. One clock pair per collection;
        // `CRATONVM_ZGC_MARK_CPU_ACCOUNTING=0` reads no clock. The cumulative
        // shares are snapshotted only under `--verbose:gc`, for the per-cycle
        // `[GC] zgc-pause-cpu:` deltas.
        let pause_cpu = mark::ZThreadCpuSpan::start(self.collection_cpu_accounting_on());
        let cpu_before = self
            .gc_log_enabled
            .load(Ordering::Relaxed)
            .then(|| self.collection_cpu_totals());
        // zr z4/un: no uncommitter pass runs while this collection does (the
        // slides write into free space without the allocator, and the
        // forwarding-word range changes here). A pass holds the gate for one
        // bounded batch at most; it only `try_lock`s, so it never waits on
        // this pause.
        let _uncommit_gate = self.counters.fp.uncommit_gate.lock();
        // zs s2/rf (`CRATONVM_ZGC_OVERSIZE_SCREEN_MEMO`): one oversize-screen
        // registry scan per over-long array per pause; emptied on exit.
        let _oversize_memo = OversizeScreenMemoGuard::arm(self);
        // zr z5/rv5: this collection's soft-precise probe, if the VM armed one
        // (`arm_soft_precise_probe`); taken here so it never reaches a later
        // collection, and last cycle's answer is dropped.
        let mut soft_precise_probe = self.counters.soft_precise_probe.lock().take();
        *self.counters.soft_precise_kept.lock() = None;
        // Pause clock for the `--verbose:gc` line at the end of this function.
        // Taken ONLY when logging is armed, so a quiet run pays one relaxed
        // load per collection and no clock read at all.
        //
        // ALSO taken when a pause TARGET is set, whether or not logging is on:
        // `refresh_pause_target_budget` sizes the next budget from this same
        // reading, and a control loop that only runs under `--verbose:gc`
        // would make the flag change the collector's behaviour rather than
        // just its output.
        //
        // zr z1/tr: and when the director samples (`CRATONVM_ZGC_RATE_TRIGGER`
        // / `CRATONVM_ZGC_TRIGGER_SHADOW`), whose rate rule budgets for the
        // cycle's own duration and is disabled until one has been timed.
        let gc_started = (self.gc_log_enabled.load(Ordering::Relaxed)
            || self.pause_target_ms.load(Ordering::Relaxed) != 0
            || self.counters.director.is_sampling())
        .then(std::time::Instant::now);
        // zt t21/zc2 (`CRATONVM_ZGC_CONCURRENT_SWEEP`, opt-in): a sweep the
        // previous cycle parked and no runner has taken yet is finished HERE,
        // before anything below reads `allocated`, touches the arena or reuses
        // the mark bits; its cost is this pause's. One relaxed load otherwise.
        let _ = self.finish_pending_sweep(ZSweepRunner::CycleStart);

        // OCCUPANCY ON ENTRY, for `ZgcMetrics::record_cycle`. One relaxed load,
        // taken here rather than reconstructed later as `live_bytes +
        // bytes_freed`: that sum is the swept span and omits whatever the
        // allocator holds but the sweep did not account (TLAB tails, the
        // large-object region's slack), so it would understate the "before"
        // figure -- and `record_cycle` derives the mutator allocation rate as
        // `before(this cycle) - live(previous cycle)`, where a systematic
        // understatement becomes a systematically low rate. `self.allocated` is
        // the gauge `collect_garbage` itself rewrites at the end of every
        // cycle, so the two readings are the same quantity one cycle apart.
        let bytes_in_use_before = self.allocated.load(Ordering::Relaxed);
        // zr z6/tp: the pacing census's inputs, read before this cycle resets
        // either: bytes allocated since the previous collection, and whether
        // that collection left the live set at or above the threshold.
        let pace_prev_live = self.cycle_alloc_watermark.load(Ordering::Relaxed);
        let pace_window = bytes_in_use_before.saturating_sub(pace_prev_live);
        let pace_near_full =
            self.gc_count.load(Ordering::Relaxed) > 0 && pace_prev_live >= self.gc_threshold;

        // This collection may free or move memory inside any thread's VM-TLAB
        // chunk, so no owned-chunk record survives it (`vm_tlab`'s
        // `OwnedVmTlabChunk`). Every mutator is stopped, so each sees the bump
        // before its next registration.
        vm_tlab::invalidate_owned_vm_tlab_chunks();

        // ---- CLOSE AN IN-FLIGHT CONCURRENT CYCLE -------------------------
        //
        // Before the TLAB retire, and therefore before ANYTHING in this
        // function touches the arena. `finish_concurrent_mark` is where the
        // mark workers are driven to a certified fixed point and then JOINED
        // (`ZMarkCoordinator::drop` joins, it does not detach), so putting it
        // first is what makes "no mark worker is running for the rest of this
        // collection" true rather than merely likely -- and the statements
        // below install filler headers, mutate the arena free list and
        // eventually zero dead objects.
        //
        // `Some(off_heap)` means the mark bits in this heap are the certified
        // transitive closure of the root set and the sweep may act on them:
        // the mark phase for THIS collection already happened, mostly with the
        // mutators running. `None` means either that no cycle was open or that
        // the handshake refused to certify one -- in both cases the mark loop
        // below runs from scratch, which is the same fail-closed fallback
        // `mark_with_controller_stw` has always had.
        // zs s4/ob3: also armed when the pause bean books this collection as
        // HotSpot's pauses (`CRATONVM_ZGC_PAUSE_PHASES`): the split is cut at
        // these laps. Arming the clock changes no consumer: every lap reader
        // is gated on `gc_started` (`ZCycleLaps::timed`), not on the clock.
        let mut clock =
            ZPhaseClock::new(gc_started.is_some() || self.counters.jfr.pause_split_wanted());
        // zr z4/jf: an untaken previous cycle's JFR facts never reach this one.
        self.counters.jfr.begin_cycle();
        // ARM THE EXTRA-ROOT FILTER, and only here. It answers "this object owns
        // no mirrors, metadata or overlay" from a snapshot of those tables' key
        // sets, which is a proof only while nothing can add to them -- true
        // inside this pause and false during a CONCURRENT phase, where a
        // mutator defining a class or building a native collection would
        // register an owner the snapshot has never seen. A stale "no" there
        // would drop a live edge, so the concurrent marker keeps asking the
        // real tables: the filter is armed for this pause and nothing else.
        //
        // ---- AND THIS LINE RUNS WITH READERS LIVE ------------------------
        //
        // *Corrected wave 3, from
        // `docs/feature-designs/zgc-architecture.md`,
        // which continues
        // `gap-a-root-filter-is-off-for-the-whole-concurrent-phase.md`.* The
        // previous wording said the filter is "disarmed everywhere but here",
        // which reads as "nothing outside this pause can be probing it". That
        // is not what the ORDER of these two statements does: `rebuild()` is
        // called BEFORE `finish_concurrent_mark`, and `finish_concurrent_mark`
        // is what drives the concurrent mark workers to a fixed point and joins
        // them. So with `CRATONVM_ZGC_CONC_START` set, this rebuild runs while
        // mark worker threads are alive and calling `extra_root_probe`. The
        // mutators are stopped -- the snapshot it takes is fresh -- but the
        // READERS are not stopped at all.
        //
        // That used to be unsound, and only an unasserted invariant in THIS
        // file kept it unreachable: `armed` happened to be `false` at every
        // rebuild, because the previous collection's
        // `counters.mark_root_filter.disarm()` is the last statement of
        // `collect_garbage`. Wave 1's
        // release-store-then-clear could not have bought what its comment
        // claimed -- a release store orders the storing thread's later writes
        // and is visible to an acquire LOAD, but a reader that has already
        // executed its `armed` load and is indexing the bitmaps sees the
        // clearing loop regardless. This filter's `false` is a PROOF, so half a
        // bloom is not a slower answer, it is a wrong one: a dropped live edge
        // and a reachable object swept.
        //
        // FIXED IN `mark_roots.rs` (wave 2): the snapshot is double-banked. A
        // rebuild fills the bank that is not published and publishes it with
        // one release store of a `state` word carrying both "is anything
        // published" and "which bank". A racing reader sees either the old
        // state and the old bank -- intact, because nothing writes to it -- or
        // the new state and the new bank, complete by release/acquire. There is
        // no third outcome, so this ordering is now legal rather than
        // accidentally safe, and it is no longer this file's job to preserve an
        // invariant that held up memory safety in another one.
        //
        // Arming the filter DURING the concurrent phase is default ON since zr
        // z7/cm (`CRATONVM_ZGC_MARK_ROOT_FILTER_CONC`, `=0` kill;
        // `conc_root_filter_armed`; this said "opt-in" until zs s2/mc2).
        // It rests on the per-table validity generations
        // (`types/src/{loader,mirror,metadata}_pin.rs`, `external_roots`) AND
        // the mark-end difference-set pass harvested just below
        // (`ZMarkRootFilter::late_owners_within`) -- because a generation check
        // protects proofs taken AFTER the mismatch and does nothing for the
        // ones already taken: a loader marked and scanned at T with the filter
        // answering "owns nothing", which then defines a class at T+1, has a
        // mirror the marker will never visit. Nothing re-claims it, because it
        // is already marked; `late_owners` roots that mirror directly. (This
        // paragraph said both halves were "not implemented" until zr z1/mk;
        // both landed in wave 4 of the 2026-09-20 round. The per-drain
        // `is_current` revalidation, stage 3c, is still not wired.)
        //
        // HARVEST BEFORE THE REBUILD (wave 4, stage 3a of
        // `handoff-r-wire-the-concurrent-root-filter.md`). `rebuild()` retires
        // the bank the concurrent phase's key sets live in, and
        // `late_owners()` is a difference against exactly those sets, so the
        // order is load-bearing: rebuild first and the repair set is computed
        // against a snapshot that no longer describes the proofs it has to
        // repair. There is one rebuild of slack in the bank rotation, so this
        // is belt and braces rather than a knife edge -- but the invariant is
        // written down here because it lives in one file and holds up another.
        // zt t1/mk (residual 10): the decision the cycle's mark start took.
        let filter_was_on = self.counters.conc_cycle_root_filter.load(Ordering::Relaxed);
        let late = if filter_was_on && self.conc_cycle_active.load(Ordering::Acquire) {
            Some(
                self.counters
                    .mark_root_filter
                    .late_owners_within(self.arena_base..self.arena_end),
            )
        } else {
            None
        };
        self.counters.mark_root_filter.rebuild();
        // zr z2/cc: a collection taken with concurrent marking CONFIGURED but
        // no cycle open is a missed window; name why, so a row reading "0
        // concurrent cycles" says which trigger collected first (the bits are
        // `trigger_seen`'s, still set here) and where the window was.
        let conc_trigger = self.conc_start_bytes.load(Ordering::Relaxed);
        if conc_trigger != 0
            && !self.conc_cycle_active.load(Ordering::Acquire)
            && self.gc_log_enabled.load(Ordering::Relaxed)
        {
            let a = self.allocated.load(Ordering::Relaxed);
            let (start, end) = self.conc_start_window(conc_trigger, a);
            eprintln!(
                "[GC] zgc-conc-miss: cycle={} allocated={a} window={start}..{end} \
                 threshold={} trigger_seen={:#x} nearest={} every_trigger={} requested={} \
                 gen_inert={}",
                self.gc_count.load(Ordering::Relaxed) + 1,
                self.gc_threshold,
                self.counters.trigger_seen.load(Ordering::Relaxed),
                zgc_conc_start_nearest() as u8,
                // zr z3/cs: under the switch the window is unused; `requested`
                // says whether a cycle request was pending (a lapsed or
                // refused one) when this stop-the-world collection ran.
                self.conc_every_trigger_armed() as u8,
                (self
                    .counters
                    .conc_trigger
                    .requested_at
                    .load(Ordering::Relaxed)
                    != 0) as u8,
                // t1 wave 2 lane Z: the window was withdrawn on this
                // generational heap (`gen_conc_window_inert`).
                (self.generational_enabled.load(Ordering::Relaxed) && self.gen_conc_window_inert())
                    as u8,
            );
        }
        // zr z3/cs: book this collection against the every-trigger cycle it
        // closes (finished / abandoned / lapsed) and reset the request. Before
        // `finish_concurrent_mark`, which clears the open flag and drops the
        // pool whose fixed-point flag this reads. No-op with the switch off.
        self.conc_every_trigger_account_collection();
        // zr z7b/cm: the far end of the adaptive window's rate sample --
        // `allocated` before the sweep resets it. See
        // `refresh_adaptive_conc_start`.
        self.counters
            .rate_alloc_at_entry
            .store(self.allocated.load(Ordering::Relaxed), Ordering::Relaxed);
        // zr z11/rv9: and the clock beside it (`CRATONVM_ZGC_CONC_RATE_EXCLUDE_PAUSE`).
        if self.conc_start_adaptive.load(Ordering::Relaxed) {
            self.counters
                .rate_nanos_at_entry
                .store(Self::monotonic_nanos(), Ordering::Relaxed);
        }
        // zt t1/mk (`CRATONVM_ZGC_FULL_REQUEST_FRESH_MARK`, default ON, `=0`
        // kill): a FULL request does not close an open concurrent cycle with
        // that cycle's mark set. The set is a snapshot of the heap at MARK
        // START (SATB), so every object that died during the phase -- and the
        // referent of every `Reference` registered during it, whose slot the
        // skip set did not hide -- survives it. HotSpot ZGC serves
        // `System.gc()` with a cycle that STARTS after the request (its
        // driver port satisfies a synchronous request only with a cycle
        // begun after it), so an object unreachable at the call is freed and
        // its weak references cleared by the time `System.gc()` returns.
        // Abandoning here leaves this collection exactly where every refused
        // certification already leaves it -- `finish_concurrent_mark` answers
        // `None` and the stop-the-world mark below starts from cleared bits --
        // so no new state is introduced. The request flag is the collecting
        // thread's own (`gc_quiescence::request_major_gc`, set by
        // `force_full_collection`'s doors). One TLS access per collection.
        //
        // zt t2/x1 (finding X1-16): CONSUMED here, on every collection under
        // the switch, cycle open or not. Only the generational heap consumed
        // it (`gen_heap`'s Phase 5), and `force_full_collection` withdraws it
        // only when it did NOT collect, so after a won `System.gc()` on ZGC the
        // flag stayed set on that thread for good: the t1 cut, which only
        // read it, then abandoned the concurrent cycle of EVERY later
        // collection that thread ran (an allocation-triggered one included).
        // Nothing else on ZGC reads it (`roots::conditional_loader_metadata`
        // answers `true` on ZGC without it; `native_roots` reads it for a
        // debug line), so taking it here changes no other ZGC decision. The
        // kill arm leaves it as the base did.
        //
        // zt t3/gn (`CRATONVM_ZGC_GEN_FULL_REQUEST_MAJOR`, default ON, `=0`
        // kill): on a GENERATIONAL heap the same request also decides the
        // cycle kind (the `force_major` block below). Read before the take
        // above it consumes the flag, and consumed here when that switch is
        // off, so a request never outlives the collection it made whole-heap.
        //
        // zt t6/tc (zt1tp item 7, `CRATONVM_ZGC_WARMUP_EXPLICIT_DOOR`, default
        // ON, `=0` kill): a collection the `System.gc()` / `jcmd GC.run` door
        // runs (its request is on this, the collecting, thread) is not a
        // warm-up step even when a poll had raised the warm-up bit: HotSpot
        // counts only `Warmup`-cause cycles (`ZStatCycle`'s warm-up count),
        // and its `-Xlog:gc` shows `System.gc()` first, then `Warmup` at 10 %.
        // Cleared before the mask is snapshotted for the cycle, so neither
        // `warmup_cycles_done` nor the director's warmth counts it; the next
        // poll raises the bit again if the step is still due. (An
        // allocation-failure collection still counts: that door leaves no
        // mark on the collecting thread the gc crate can read.)
        if cratonvm_gc_core::gc_quiescence::major_gc_requested()
            && zgc_flag_default("CRATONVM_ZGC_WARMUP_EXPLICIT_DOOR", true)
        {
            self.counters
                .trigger_seen
                .fetch_and(!TRIGGER_BIT_WARMUP, Ordering::Relaxed);
        }
        // zt t20/tg9: the opt-in allocation-stall door that also cleared the
        // bit here (zt t11/tg4) is retired: its engagement counter read
        // `seen=0` on every t14 row, switch on or off (the refill gate
        // collects at an occupancy door first, and the warm-up steps run at
        // the first polls after the fill), so the state it acted on never
        // occurred.
        // zt t9/tg2 (`zgc-zt7tg-timer-service-residuals` item 3,
        // `CRATONVM_ZGC_TIMER_NOT_WARMUP`, default ON, `=0` kill): a cycle
        // the director tick's `Timer` rule asked for reports `Timer` (first
        // in `hotspot_cause_for_mask`), so it is not a warm-up step either,
        // even when a poll had raised the warm-up bit before the tick's
        // request landed: HotSpot counts only `Warmup`-cause cycles. Cleared
        // here, before the snapshot, as the explicit door above does; the
        // next poll raises the bit again if the step is still due.
        if self.counters.trigger_seen.load(Ordering::Relaxed) & TRIGGER_BIT_TIMER != 0
            && zgc_flag_default("CRATONVM_ZGC_TIMER_NOT_WARMUP", true)
        {
            self.counters
                .trigger_seen
                .fetch_and(!TRIGGER_BIT_WARMUP, Ordering::Relaxed);
        }
        let gen_major_request = self.generational_enabled.load(Ordering::Relaxed)
            && self.counters.gen_full_request_major.load(Ordering::Relaxed)
            && cratonvm_gc_core::gc_quiescence::major_gc_requested();
        let full_request = self
            .counters
            .full_request_fresh_mark
            .load(Ordering::Relaxed)
            && cratonvm_gc_core::gc_quiescence::take_major_gc_request();
        if gen_major_request && !full_request {
            let _ = cratonvm_gc_core::gc_quiescence::take_major_gc_request();
        }
        // zt t18/zc1 (`CRATONVM_ZGC_SYSTEM_GC_CONCURRENT`): a cycle a
        // `System.gc()` request opened AFTER its call is the cycle that request
        // asked for, so its own collection closes it with its mark set rather
        // than abandoning it: the snapshot already postdates the call, which
        // is what the fresh-mark rule above exists to guarantee. Taken by every
        // collection (cycle open or not), so an ownership never outlives the
        // collection that closes the cycle -- another door's included, which
        // books it `lost`.
        let sysgc_owned = self.take_system_gc_cycle_owner();
        if sysgc_owned && self.conc_cycle_active.load(Ordering::Acquire) {
            let st = &self.counters.conc_trigger;
            // Under `CRATONVM_ZGC_FULL_REQUEST_FRESH_MARK=0` the request flag
            // is not taken above, so it is read here instead.
            if full_request || cratonvm_gc_core::gc_quiescence::major_gc_requested() {
                st.sysgc_served.fetch_add(1, Ordering::Relaxed);
            } else {
                st.sysgc_lost.fetch_add(1, Ordering::Relaxed);
            }
        }
        if full_request && self.conc_cycle_active.load(Ordering::Acquire) && !sysgc_owned {
            self.counters
                .full_request_conc_abandons
                .fetch_add(1, Ordering::Relaxed);
            if self.gc_log_enabled.load(Ordering::Relaxed) {
                eprintln!(
                    "[GC] zgc-conc-abandon: cycle={} cause=full-request abandons={}",
                    self.gc_count.load(Ordering::Relaxed) + 1,
                    self.full_request_conc_abandons(),
                );
            }
            self.abandon_concurrent_mark();
        }
        let concurrent_off_heap = self.finish_concurrent_mark(roots, late);
        let marked_concurrently = concurrent_off_heap.is_some();
        // zt t31/x10 (item 1): this collection's Mark Start occupancy, when
        // its mark ran concurrently; none for a stop-the-world mark.
        let mark_start_used = self
            .counters
            .conc_mark_start_used
            .swap(usize::MAX, Ordering::Relaxed);
        self.counters.last_mark_start_used.store(
            if marked_concurrently {
                mark_start_used
            } else {
                usize::MAX
            },
            Ordering::Relaxed,
        );
        // zt t24/hunt7: for the allocation ladder's clear-soft rung
        // (`last_collection_marked_concurrently`).
        self.counters
            .last_collection_marked_concurrently
            .store(marked_concurrently, Ordering::Relaxed);
        let markend_us = clock.lap();

        // ---- RETIRE EVERY TLAB -------------------------------------------
        // First statement after the concurrent-cycle handshake above, before
        // the registry snapshot below and
        // therefore before anything reads or walks a heap byte. This is the
        // module invariant `gc::zgc::tlab` states, honoured here for the three
        // things it actually buys on THIS heap:
        //
        //  1. Chunk tails come back. `retire_all_tlabs` hands every
        //     `[cursor, chunk_end)` remainder to the arena free list, so the
        //     coalescer at the end of the sweep sees them and the bytes are
        //     reusable. Skipping this leaks up to `threads * chunk` per cycle
        //     into spans the sweep can never visit (a filler is not in
        //     `registry`), which exhausts the heap in a few dozen cycles.
        //  2. Every un-handed-out span becomes parseable, so the arena bytes
        //     below `cursor` satisfy the walkability contract the rest of this
        //     crate assumes of them.
        //  3. It is the single point at which "no mutator owns a private slice
        //     of this heap" is true, which is what makes the mark snapshot on
        //     the next line a complete picture.
        //
        // Registry completeness is NOT one of the three: a TLAB-served object
        // enters `registry` at hand-out (`ZArenaTlabRegistry`'s
        // `registry_batch` note explains why batching was reverted), so it is
        // already there before this call.
        // AT A SAFEPOINT, and it matters which: a cell this cannot lock is
        // holding a chunk the collector cannot see, and `relocate_stw` must
        // then decline. Cleared first, so the flag describes THIS cycle.
        self.tlab_retire_incomplete.store(false, Ordering::Release);
        self.retire_all_tlabs_at_safepoint();

        // ---- GIVE THE UN-BUMPED MIDDLE BACK TO THE OS --------------------
        //
        // Here, and not where the space became free. Everything between the two
        // cursors belongs to neither end and can only be reached by moving a
        // cursor -- which commits what it passes over -- so releasing its whole
        // granules asks no liveness question at all.
        //
        // It is at the START of a collection rather than the end of the last
        // one because the last slide's forwarding records live in exactly that
        // span (`stamp_forwarding_words`), and a record has to stop being
        // believed BEFORE the page under it can go away.
        // `retire_forwarding_words` is that statement, and the ordering of the
        // two lines is the whole of it.
        //
        // The pair is what makes `-Xmx` a ceiling rather than a charge: with a
        // reserving backing store this is the only thing that returns the peak
        // to the OS, and a JVM that peaks and then idles used to hold its peak
        // for the life of the process.
        self.retire_forwarding_words();
        {
            let released = {
                let mut arena = self.arena.lock();
                // BOTH HALVES. The middle is the space no allocation ever
                // reached; the free lists are the space that WAS allocated and
                // has since been freed, and on a heap whose peak was large
                // objects that is all of it -- `probes/HeapGiveBack.java`
                // measured the middle alone returning 2 MiB of a 64 MiB peak.
                // zr z3/fp: the commitment the cycle found, before it gives
                // anything back -- the `zgc-footprint` line's peak.
                self.counters
                    .fp
                    .committed_peak
                    .fetch_max(arena.committed_bytes(), Ordering::Relaxed);
                // zr z4/jf X1 (applied by z4/un): time the give-back for
                // `jdk.ZUncommit`, counting the READABLE give-back's resets
                // too -- the default mode decommits nothing (`released` is 0)
                // and still returns the pages.
                let reset_before = arena.bytes_reset();
                let t = std::time::Instant::now();
                // zt t3/rl (`CRATONVM_ZGC_BITMAP_RELEASE_WITH_GRANULE`): the
                // side-table pages over what the give-back released go too,
                // with the world stopped and the arena lock held.
                let log = true;
                if log {
                    arena.set_release_log(true);
                }
                let r = arena.decommit_unused_above(0);
                if log {
                    let spans = arena.take_release_log();
                    self.release_bitmap_pages_over(&spans);
                }
                let ns = t.elapsed().as_nanos().min(u64::MAX as u128) as u64;
                let reset = arena.bytes_reset().saturating_sub(reset_before);
                (r, reset, ns)
            };
            let (released, reset, uncommit_ns) = released;
            if released + reset != 0 {
                // zr z4/jf: `jdk.ZUncommit` (this collection's facts).
                self.note_jfr_uncommit(released + reset, uncommit_ns);
            }
            if released != 0 {
                self.counters
                    .bytes_uncommitted
                    .fetch_add(released, Ordering::Relaxed);
                // RETRACT THE INLINE-LOAD BOUND. This is the half that was
                // missing, and it is a fault rather than a slowdown.
                //
                // `with_capacity` publishes `[arena_base, arena_end)` into
                // `JIT_READ_BOUNDS` slot 0 once (see `zgc_jit_read_bounds_
                // enabled`), and the JIT's guarded `getfield` emits a raw load
                // of the receiver's class-id word for any address inside that
                // range. The decommit above punches holes into the MIDDLE of
                // that same range, and `reservation::platform_decommit` is
                // `madvise(MADV_DONTNEED)` + `mprotect(PROT_NONE)` on Unix and
                // `VirtualFree(MEM_DECOMMIT)` on Windows -- a released granule
                // FAULTS on touch, it does not read back as zero (the paragraph
                // on `Arena::decommit_free_blocks` says so, and records that
                // believing otherwise is how two slides came to write into
                // released granules). Nothing re-narrowed the bound, so a
                // compiled load through a stale receiver that lands in a hole
                // was a SIGSEGV in compiled code with no owner.
                //
                // G1 has the same pair and gets it right: `uncommit_trailing_
                // free_regions_within` publishes the narrower bound BEFORE it
                // unmaps, and says that the order is the whole safety argument.
                // G1 can, because it only ever shrinks a PREFIX. These holes are
                // interior, so there is no narrower `[base, end)` to publish --
                // the only sound move is to withdraw the range entirely.
                //
                // A ONE-WAY LATCH, and cheap for exactly that reason: it costs
                // one relaxed load per collection that released anything, it
                // fires at most once per process, and after it every guarded
                // site falls through to the checked helper -- which is what an
                // unpublished collector already gets, and the fail-safe
                // direction. Clearing the table is also the ONLY mechanism that
                // can disarm an inline sequence in code that is already
                // compiled; `set_barrier_color` documents the same manoeuvre for
                // the same table.
                if zgc_jit_read_bounds_enabled()
                    && !self
                        .counters
                        .jit_read_bounds_retracted
                        .swap(true, Ordering::Relaxed)
                {
                    cratonvm_gc_core::jit_bounds::publish_jit_read_bounds(0, 0, 0);
                    tracing::info!(
                        target: "cratonvm::gc",
                        bytes = released,
                        "zgc: withdrew the JIT inline-load bound -- the arena now has decommitted holes inside it",
                    );
                }
                tracing::debug!(
                    target: "cratonvm::gc",
                    bytes = released,
                    "zgc: returned the un-bumped middle to the OS",
                );
            }
        }

        // ---- Mark phase --------------------------------------------------
        // Snapshot the registry of all live-or-dead allocations: marking reads
        // object bytes in place and does not allocate, so it needs no arena
        // lock and must not pin the registry either. The copy also
        // serves as the mark-phase validation oracle (ZGC-4): child pointers
        // pushed by `enumerate_references` come from raw field bytes, and a
        // corrupt/stale slot must be SKIPPED, not have a mark bit written
        // through it (a wild `header_mut` write inside an innocent object —
        // or outside the arena entirely — then cascades as the garbage
        // "object" is re-parsed for more children).
        //
        // `snapshot()` is the direct replacement for the old
        // `self.registry.lock().clone()` and keeps its two properties exactly:
        // it is FROZEN (so the oracle cannot drift under the mark loop) and its
        // membership is O(1). On the bitmap arm it copies one bit per 8 arena
        // bytes rather than 8+ bytes per live object, so it is also strictly
        // cheaper than the set clone at any occupancy above ~1.5%.
        let tlab_us = clock.lap();
        let registered: ZObjectStartsSnapshot = if zgc_bitmap_bounds_enabled() {
            // BOUNDED to the two ends the arena has actually bumped. See
            // `live_bitmap_bounds` for the measurement and for why the middle
            // is provably empty; `CRATONVM_ZGC_BITMAP_BOUNDS=0` restores the
            // whole-capacity copy.
            self.registry.snapshot_within(&self.live_bitmap_bounds())
        } else {
            self.registry.snapshot()
        };
        // zr z3/fp: the snapshot is the cycle's largest transient copy (1/64
        // of the bumped arena); its peak goes on the `zgc-footprint` line.
        self.counters
            .fp
            .snapshot_peak
            .fetch_max(registered.resident_bytes(), Ordering::Relaxed);
        // COUNTED, not collected. `bases()` here allocated one `usize` per
        // registered object -- 87 MB on the 10.8M-object arm, inside the pause,
        // which the 2026-08-17 anatomy measured as 13% of it. Every phase below
        // that needs the bases iterates the bitmap instead
        // (`for_each_base`); the only remaining `bases()` calls on this path are
        // behind the corpse-census flag and on the relocation path, both of
        // which are per-cycle and want a slice.
        let registered_count = registered.base_count();
        let snapshot_us = clock.lap();
        // zs s4/ob3: the end of this pause's `Pause Mark Start` record.
        let pause_mark_start_end = clock.at();

        // ---- PHASE G: IS THIS A YOUNG CYCLE? -----------------------------
        //
        // Decided here because the next statement is the mark-bit pass, and on a
        // young cycle that pass is where the old generation is pre-marked. See
        // the PHASE G block above `addr_is_young` for why a young cycle is the
        // same collection with one arm of one loop inverted.
        let gen_on = self.generational_enabled.load(Ordering::Relaxed);
        let promo_age = self.promotion_age();
        // FOUR reasons this collection must look at the whole heap.
        //
        //  * `marked_concurrently` -- the mark set `finish_concurrent_mark`
        //    handed over IS the whole-heap closure. It cannot be scoped after
        //    the fact, and re-marking would throw away the concurrent phase's
        //    entire product.
        //  * `hard_alloc_failure` -- an allocation actually failed after a
        //    collection. A young cycle retains the whole old generation
        //    unexamined, and here the evidence that that is not enough is
        //    already in.
        //  * `gen_force_major_next` -- a previous young cycle reclaimed nothing.
        //    See that field for why this is an escalation latch rather than a
        //    stricter trigger, and for the vacuous measurement the stricter
        //    trigger produced. **`headroom_low` is deliberately NOT here.**
        //  * the `minors_per_major` ceiling -- see `zgc_gen_minors_per_major`
        //    for why an unbounded run of young cycles is a leak.
        //
        // zr z2/g2: under `CRATONVM_ZGC_GEN_MAJOR_PROMOTED_PERCENT` the fourth
        // reason is old GROWTH instead of a minor count -- see
        // `gen_major_by_old_growth`.
        // zr z2/cp: the relocation selector's copy of the latch, taken here
        // because the re-arm block lowers the latch before the slide runs.
        let after_alloc_failure = self.hard_alloc_failure.load(Ordering::Relaxed);
        self.counters
            .cycle_after_alloc_failure
            .store(after_alloc_failure, Ordering::Relaxed);
        // zt t16/gn6 (item 12): the same copy for the `-Xlog` logger, which
        // reads it after the collection (the one above is consumed by the
        // relocation selector). Every cycle stores it; a latched failure
        // forces the cycle whole-heap, so `true` describes a major.
        self.counters
            .gen_last_major_alloc_stalled
            .store(after_alloc_failure, Ordering::Relaxed);
        // zt t3/gn: a FIFTH reason, the full request (`System.gc()`, `jcmd
        // GC.run`; `gen_major_request` above). HotSpot 25's generational ZGC
        // serves it with a major collection. Counted only where it decided
        // something: a heap that already has an old generation, on a cycle no
        // earlier reason had made whole-heap. Before `gen_major_ceiling_reached`
        // so a request does not also charge the old-growth counters.
        let request_major = gen_major_request
            && !marked_concurrently
            && !after_alloc_failure
            && self.has_old_objects.load(Ordering::Relaxed);
        if request_major {
            self.counters
                .gen_majors_by_request
                .fetch_add(1, Ordering::Relaxed);
        }
        // zt t11/gn3 (`CRATONVM_ZGC_GEN_MAJOR_RULE_CAUSES`, default ON in the
        // mode, `=0` kill): a SIXTH reason, a HotSpot major-only cause
        // (`Warmup`, `Proactive`; `gen_mask_is_major_rule`). The mask is the
        // one this cycle reports (`trigger_seen`, snapshotted and reset after
        // the slide), after the full-request and timer doors above took the
        // warm-up bit off. Counted like the request: only where it decided
        // something; before `gen_major_ceiling_reached` so it charges none of
        // that rule's counters.
        let rule_cause_major = gen_on
            && self.counters.gen_major_rule_causes.load(Ordering::Relaxed)
            && gen_mask_is_major_rule(self.counters.trigger_seen.load(Ordering::Relaxed));
        if rule_cause_major
            && !marked_concurrently
            && !after_alloc_failure
            && !gen_major_request
            && !self.gen_force_major_next.load(Ordering::Relaxed)
            && self.has_old_objects.load(Ordering::Relaxed)
        {
            self.counters
                .gen_majors_by_rule_cause
                .fetch_add(1, Ordering::Relaxed);
        }
        let force_major = marked_concurrently
            || after_alloc_failure
            || gen_major_request
            || self.gen_force_major_next.load(Ordering::Relaxed)
            || rule_cause_major
            || self.gen_major_ceiling_reached();
        // `has_old_objects` rather than just `gen_on`: with nothing promoted yet
        // a young cycle IS a full cycle, and counting it as a minor would burn
        // the `minors_per_major` budget on cycles that saved nothing.
        // ...and the young PAGE set has to be trustworthy. An allocation that
        // landed off the page grid leaves it incomplete, and an incomplete
        // young set does not under-collect for one cycle -- it retains whatever
        // is on the unrecorded page until a major, forever if majors are rare.
        // Fail closed: see `young_pages_are_complete`.
        // zr z3/rv3: and, with the old-start bitmap on, only a bitmap the
        // young cycle can USE -- its fallback is the unsound floor arm (see
        // `gen_young_refused_for_bitmap`). Last, so it counts only cycles that
        // would otherwise have been young.
        let young_cycle = gen_on
            && !force_major
            && self.has_old_objects.load(Ordering::Relaxed)
            && self.young_pages_are_complete()
            && !self.gen_young_refused_for_bitmap()
            && !self.stw_vm_reference_skip_used_this_cycle();
        // zt t32 (6a): for the R4 release and the slide's R4 decision.
        self.counters
            .rim_cycle_young
            .store(young_cycle, Ordering::Relaxed);
        // zt t7/cc (`CRATONVM_ZGC_GEN_YOUNG_SKIP_REFERENCES`): did the VM's
        // pre-collection pass leave the referents in place for this cycle?
        // Taken on EVERY collection so an arming never outlives its own.
        let young_refs_strong = self.take_young_refs_strong(young_cycle);
        // THE SWEEP FLOOR. On a young cycle the sweep visits only
        // `[gen_young_floor, cursor)` -- see that field for why bounding the
        // sweep by ADDRESS is the only way to bound it at all. `0` on a
        // whole-heap cycle, and `for_each_base_from(0, ..)` is no bound, so both
        // paths are one loop.
        let sweep_floor = if young_cycle {
            self.gen_young_floor.load(Ordering::Relaxed)
        } else {
            0
        };

        // Clear all mark bits first (objects may carry a stale bit from a
        // prior cycle's survivors).
        //
        // SKIPPED after a certified concurrent mark: those bits ARE this
        // cycle's answer, and clearing them here would discard the whole
        // closure and hand the sweep an empty live set.
        //
        // ON A YOUNG CYCLE THIS PASS IS ALSO THE OLD GENERATION'S MARK. Setting
        // the bit on an old object makes every phase downstream treat it as a
        // survivor without any of them being taught about generations: the mark
        // loop's `already visited` test skips it (so its fields are never
        // enumerated -- the saving), the reference processor's `is_marked_addr`
        // reads it as live, and the sweep clears the bit and retains it. Folded
        // into a pass that already runs, so a young cycle costs no extra walk of
        // the registry.
        //
        // THE BITMAP ARM SPLITS THE PASS IN TWO, and only a young cycle keeps
        // the walk. Clearing is `mark_clear_all` -- one linear store over
        // `capacity / 512` bytes -- so a WHOLE-HEAP cycle no longer walks the
        // registry here at all. That is a full pass over every registered
        // object, with a read-modify-write of a scattered 64-byte line each,
        // removed from every default (non-generational, non-concurrent)
        // collection. A young cycle still walks, because pre-marking the old
        // generation needs each object's `gc_age`, which lives nowhere but the
        // header; it no longer WRITES the header, only the bitmap.
        let mut gen_old_retained = 0usize;
        // zr z1/gn: a whole-heap generational cycle rebuilds the old-start
        // bitmap from its own sweep; clear it first (before `mark_clear_all`
        // resets the bounds it is cleared within).
        if gen_on && !young_cycle {
            self.begin_old_bits_rebuild();
        }
        let old_bits_young = young_cycle && self.old_bits_usable();
        if young_cycle && !old_bits_young && self.counters.gen_old_bits_on.load(Ordering::Relaxed) {
            self.counters
                .gen_old_bits_fallbacks
                .fetch_add(1, Ordering::Relaxed);
        }
        // The young-aged sweep needs the bitmap's complement, so it engages
        // only on a bitmap pre-marked cycle.
        let young_aged_sweep = old_bits_young
            && self
                .counters
                .gen_young_aged_sweep_on
                .load(Ordering::Relaxed);
        // zt t7/tg (GN-10): the clear + pre-mark streams, timed for
        // `[GC] zgc-gen-cost:` `*_stream_us` on a generational heap while the
        // pause clock runs (no clock read otherwise).
        let stream_started = (gen_on && gc_started.is_some()).then(std::time::Instant::now);
        if !marked_concurrently {
            // zt t34: a stop-the-world mark keeps no liveness for an R6 plan.
            self.crl_liveness_invalidate();
            let bulk_cleared = self.mark_clear_all();
            if young_cycle && bulk_cleared && old_bits_young {
                // `mark |= registered & old`: no header read. See
                // `premark_from_old_bits`.
                gen_old_retained = self.premark_from_old_bits(&registered);
                self.counters
                    .gen_old_bits_premarks
                    .fetch_add(1, Ordering::Relaxed);
                if self.counters.local.gen_old_bitmap_verify() {
                    self.verify_old_premark(&registered, promo_age);
                }
            } else if young_cycle {
                registered.for_each_base(|base| {
                    let header = self.header_mut(base as *mut u8);
                    if header.gc_age() >= promo_age {
                        self.mark_set(base);
                        gen_old_retained += 1;
                    } else if !bulk_cleared {
                        header.clear_gc_flags(GC_FLAG_MARKED);
                    }
                });
            } else if !bulk_cleared {
                registered.for_each_base(|base| {
                    self.header_mut(base as *mut u8)
                        .clear_gc_flags(GC_FLAG_MARKED);
                });
            }
        }
        let stream_us = stream_started.map_or(0, |t| t.elapsed().as_micros() as u64);
        // The old generation's contribution to the root set. Computed after the
        // pre-mark so the mark bits and `addr_is_young` agree, and before either
        // marker runs so both get the same roots.
        // zr z2/g2: timed for `[GC] zgc-gen-cost:` (only when the pause clock
        // runs).
        let mut remset_us = 0u64;
        let gen_extra_roots: Vec<usize> = if young_cycle {
            let t = gc_started.map(|_| std::time::Instant::now());
            let extra = self.young_extra_roots(promo_age);
            remset_us = t.map_or(0, |t| t.elapsed().as_micros() as u64);
            self.counters
                .gen_remembered_roots
                .fetch_add(extra.len(), Ordering::Relaxed);
            extra
        } else {
            Vec::new()
        };
        if young_cycle {
            self.counters
                .gen_old_retained
                .fetch_add(gen_old_retained, Ordering::Relaxed);
        }

        // INT-8: snapshot the referent-slot skip set — the currently
        // registered Weak/Soft/Phantom `Reference` object addresses. Their
        // slot 0 (the referent) must not be traced as a strong edge during
        // marking, or the referent always appears reachable through its own
        // Reference object and `process_references` below could never clear
        // it. Mirrors the G1 marker's `g1_set_reference_skip_set`; see
        // `enumerate_references`'s doc comment.
        let persistent_skip = self.counters.skipbits.persistent_enabled();
        let mut ref_skip_objs: FxHashSet<usize> = if persistent_skip {
            self.counters
                .ref_processor
                .lock()
                .soft_reference_object_addresses()
                .into_iter()
                .collect()
        } else {
            self.counters
                .ref_processor
                .lock()
                .reference_object_addresses()
                .into_iter()
                .collect()
        };
        // zt t22/zc3 (`CRATONVM_ZGC_CONCURRENT_REFS`): and the VM rows its
        // pre-collection pass hid instead of nulling (empty off the switch).
        self.extend_with_stw_vm_reference_skip(&mut ref_skip_objs);
        let skip_for = |addr: usize| -> Option<usize> {
            if self.mark_ref_skip_armed.load(Ordering::Acquire) {
                if let Some((cid, kind_code)) = self.weak_phantom_class_kind(addr, None) {
                    self.record_discovered_weak_phantom(addr, cid, kind_code);
                    return Some(0);
                }
            }
            if (persistent_skip && self.counters.skipbits.persistent_contains(addr))
                || (!ref_skip_objs.is_empty() && ref_skip_objs.contains(&addr))
            {
                Some(0)
            } else {
                None
            }
        };

        // ---- PARALLEL MARK (opt-in, Phase 3) -----------------------------
        //
        // `CRATONVM_ZGC_PARMARK` (unset = `auto` since zr z3/mt; `0` = serial)
        // runs the strong closure on the real mark
        // engine instead of the serial loop below, at this same safepoint. The
        // two must produce the IDENTICAL mark set — that is what
        // `parallel_mark_marks_the_same_objects_as_the_serial_loop` asserts,
        // and it is the only claim that matters here, because the sweep that
        // follows cannot tell which loop set the bits.
        //
        // ON by default since zr z3/mt (`Z_PARMARK_DEFAULT_AUTO`, the z2b host
        // rows: ~2.5x lower pause for +15-20 % CPU); `CRATONVM_ZGC_PARMARK=0`
        // is the serial kill switch. The pool is kept between collections
        // (`persistent_mark_pool`) and the context bound per cycle.
        //
        // SKIPPED ENTIRELY after a certified concurrent mark: this cycle's
        // strong closure is already computed and re-closed at this safepoint
        // by `finish_concurrent_mark`, so re-running it here would be a second
        // full trace of the same graph for an identical answer.
        // zr z5/rv5: an armed soft-precise probe needs the mark split in two
        // drains, which only the serial marker can do; a concurrently marked
        // cycle's mark is already complete, so the probe stands down there.
        if marked_concurrently {
            soft_precise_probe = None;
        }
        let parallel_workers = if marked_concurrently || soft_precise_probe.is_some() {
            0
        } else {
            self.parallel_mark_workers()
        };
        let mut work: Vec<usize> = Vec::new();
        let mut wild_skipped = concurrent_off_heap.unwrap_or(0);
        // What the serial marker did, when it ran (zr z1/mk): printed on the
        // `[GC] zgc-mark-serial:` line below.
        let mut serial_mark_stats: Option<ZSerialMarkStats> = None;
        // Cleared when the driver refuses to certify a complete mark set, which
        // routes this cycle through the single-threaded marker below.
        let mut parallel_ok = true;
        // zr z4/rv4: set by a failed parallel mark; see the `None` arm below.
        let mut parallel_rescan = false;
        // `>= 1`, not `> 1`: ONE worker still means the cycle is driven by
        // `zgc_concurrent`'s controller, which is Phase 3's first exit clause,
        // and it measures within 1.4% of the bespoke serial loop (95.7 ms vs
        // 94.4 ms on a 1M-object live set). Only `0` takes the hand-written
        // path, and that is the kill switch.
        if parallel_workers >= 1 {
            let mut root_addrs: Vec<u64> = roots.iter().map(|r| r.as_ptr() as u64).collect();
            // The remembered set's roots go to WHICHEVER marker runs, or a young
            // cycle is correct with one marker and a use-after-free with the
            // other. The CARD roots are young by construction (the card scan
            // filters on `addr_is_young`); the wholesale pin and overlay roots
            // of `young_extra_roots`' section (0) are not filtered, and an old
            // one is already pre-marked, so the engine's mark-bit dedup drops it
            // and its fields are NOT traced: its young children must come in
            // through its own cards, like any old object's (zt t3/gn: this
            // comment said every extra root was young; audit item GN-6 of
            // `t3-gn-report.md`).
            root_addrs.extend(gen_extra_roots.iter().map(|a| *a as u64));
            // OPEN THE CYCLE FIRST. `mark_parallel_stw`'s doc calls this "the
            // caller's contract" and this caller violated it until 2026-08-13.
            //
            // Without the snapshot, `visit_refs` has no skip set and traces
            // slot 0 of every `Reference` as a STRONG edge — so every weak,
            // soft, phantom and final referent is reachable through its own
            // `Reference` and can never be cleared. `WeakReference` and
            // `Cleaner` silently stop working; it is a leak, not a crash, and
            // the only signal is a one-shot warning nobody reads.
            //
            // It stayed hidden because parallel marking was opt-in and no test
            // drove `collect_garbage` with it on. Turning it on by default is
            // what surfaced it, via `real_weak_ref_cleared_when_referent_dies`
            // — the serial path builds its own `ref_skip_objs` a few lines
            // below, so the two marking paths disagreed about the one thing
            // that must not differ between them.
            // zr z2/cc: time the three pieces separately, so a row can tell the
            // engine's trace (`drive_us`) from the serial set-up around it
            // (`open_us`: skip set + bloom) -- the cycle's `mark_us` on
            // `[GC] zgc-pause:` also contains the bit clear and the old pre-mark.
            let par_t0 = std::time::Instant::now();
            // zs s2/mc2: the COLLECTING thread's CPU over the same span (skip
            // set, bloom, the driver's waits and handshakes) -- the share of
            // the mark that lands in `java_cpu_ms`, beside the pool's `cpu_us=`.
            let par_cpu = mark::ZThreadCpuSpan::start(
                self.counters
                    .mark_loop
                    .cpu_accounting
                    .load(Ordering::Relaxed),
            );
            let _skip = self.begin_concurrent_mark_cycle();
            let par_open_us = par_t0.elapsed().as_micros();
            let driven = self.mark_parallel_stw(&root_addrs, parallel_workers);
            let par_drive_us = par_t0.elapsed().as_micros().saturating_sub(par_open_us);
            self.end_concurrent_mark_cycle();
            let par_driver_cpu_ns = par_cpu.elapsed_ns();
            self.counters
                .mark_cpu_stw_driver_ns
                .fetch_add(par_driver_cpu_ns, Ordering::Relaxed);
            match driven {
                Some(stats) => {
                    self.counters
                        .parallel_mark_cycles
                        .fetch_add(1, Ordering::Relaxed);
                    // `off_head_children` is this loop's `wild_skipped` under
                    // another name — the engine's own doc says so.
                    wild_skipped = stats.off_heap_children as usize;
                    // zr z1/mk: the engine's own engagement counters, which no
                    // line printed. `docs/gc-tuning.md` sent readers to a
                    // `[GC] zgc-mark:` line for `ref_chunks` that did not
                    // exist; this is it, for the stop-the-world pool.
                    if self.gc_log_enabled.load(Ordering::Relaxed) {
                        eprintln!(
                            "[GC] zgc-mark-parallel: cycle={} workers={parallel_workers} \
                             open_us={par_open_us} drive_us={par_drive_us} roots={} \
                             marked={} scanned={} ref_chunks={} stripe_publishes={} \
                             idle_spills={} steals={}/{} termination_aborts={} off_heap={} \
                             cpu_us={} driver_cpu_us={} memo_hits={} memo_misses={} \
                             run_prefetches={} roots_marked={} array_splits={} \
                             array_tasks={}/{}",
                            self.gc_count.load(Ordering::Relaxed) + 1,
                            root_addrs.len(),
                            stats.objects_marked,
                            stats.objects_scanned,
                            stats.ref_chunks,
                            stats.stripe_publishes,
                            stats.idle_spills,
                            stats.steals_succeeded,
                            stats.steal_attempts,
                            stats.termination_aborts,
                            stats.off_heap_children,
                            stats.worker_cpu_ns / 1_000,
                            par_driver_cpu_ns / 1_000,
                            stats.layout_memo_hits,
                            stats.layout_memo_misses,
                            stats.run_prefetches,
                            // zs s5/mc3: `marked=` leaves out the root claims
                            // (residual 6 of the s2/mc2 page); the split arrays
                            // and their chunk tasks (pushed / run).
                            stats.roots_marked,
                            stats.array_splits,
                            stats.array_tasks,
                            stats.array_task_runs,
                        );
                    }
                    tracing::debug!(
                        target: "zgc",
                        workers = parallel_workers,
                        marked = stats.objects_marked,
                        scanned = stats.objects_scanned,
                        off_heap_children = stats.off_heap_children,
                        "zgc STW mark complete, driven by zgc_concurrent"
                    );
                }
                // FAIL CLOSED. The driver could not certify a complete mark
                // set, and a sweep against one is a use-after-free. Fall
                // through to the single-threaded marker below, which starts
                // from the same roots, seeded (zr z4/rv4) with the out-edges
                // of every base the driver already marked.
                // zr z4/rv4: NOT idempotent for a claim-based marker -- an
                // object the driver claimed but never scanned is skipped by
                // every serial arm, roots included. The fallback seeds the
                // out-edges of every marked base
                // (`parmark_fallback_rescan_seeds`).
                None => {
                    self.counters
                        .parallel_mark_fallbacks
                        .fetch_add(1, Ordering::Relaxed);
                    parallel_ok = false;
                    parallel_rescan = zgc_parmark_fallback_rescan();
                }
            }
        }
        if !marked_concurrently && (!parallel_ok || parallel_workers == 0) {
            // zr z5/rv5: under an armed soft-precise probe the conservative
            // compiled-frame words are traced in a SECOND drain (below), so
            // the first closure is "reachable from the precise roots".
            let conservative_words: rustc_hash::FxHashSet<usize> = if soft_precise_probe.is_some() {
                cratonvm_gc_core::gc_quiescence::pinned_jit_roots_snapshot()
                    .into_iter()
                    .collect()
            } else {
                rustc_hash::FxHashSet::default()
            };
            let mut deferred_roots: Vec<usize> = Vec::new();
            // Trace from roots. A work stack holds base addresses to visit.
            for (ri, r) in roots.iter().enumerate() {
                if dbg_zgc_wild() {
                    self.dbg_wild_edge(0, "root", ri, r.as_ptr() as u64);
                }
                let a = r.as_ptr() as usize;
                if conservative_words.contains(&a) {
                    deferred_roots.push(a);
                    continue;
                }
                work.push(a);
            }
            if dbg_zgc_wild() {
                for &a in gen_extra_roots.iter() {
                    self.dbg_wild_edge(0, "remembered", 0, a as u64);
                }
            }
            work.extend(gen_extra_roots.iter().copied());
            // ZGC-4: only registered allocation bases are objects. Roots are
            // pre-filtered by is_object_address, but CHILD pointers are raw
            // field bytes -- `mark_closure_serial` skips anything that is not a
            // current base and counts it. The pin and overlay edges are the
            // same owner-based propagation Generational's non-moving young
            // marker and old-gen BFS do (`gen_heap.rs`): a native side-table
            // entry is only reachable through the Java collection object that
            // owns it, so it is traced from a CONFIRMED-live owner, and
            // `native_roots.rs`'s `scan_collection_overlays` relies on this
            // loop running before it defers to us.
            if parallel_rescan {
                let n = self.parmark_fallback_rescan_seeds(&registered, &skip_for, &mut work);
                self.counters
                    .parallel_mark_fallback_rescans
                    .fetch_add(n, Ordering::Relaxed);
            }
            let stats = self.mark_closure_serial(&mut work, &registered, &skip_for, None);
            wild_skipped += stats.wild_skipped;
            serial_mark_stats = Some(stats);
            if let Some(candidates) = soft_precise_probe.as_ref() {
                // The precise closure is complete: record the candidates it
                // reached, then trace the conservative words into the same
                // bitmap (monotone, so the final mark set equals one drain's).
                let kept: Vec<usize> = candidates
                    .iter()
                    .copied()
                    .filter(|&c| registered.contains(c) && self.mark_is_set(c))
                    .collect();
                *self.counters.soft_precise_kept.lock() = Some(kept);
                self.counters
                    .soft_precise_probe_cycles
                    .fetch_add(1, Ordering::Relaxed);
                self.counters
                    .soft_precise_deferred_roots
                    .fetch_add(deferred_roots.len(), Ordering::Relaxed);
                work.clear();
                work.append(&mut deferred_roots);
                let second = self.mark_closure_serial(&mut work, &registered, &skip_for, None);
                wild_skipped += second.wild_skipped;
            }
            debug_assert!(deferred_roots.is_empty(), "every deferred root was traced");
        }
        if wild_skipped > 0 {
            tracing::warn!(
                target: "zgc",
                wild_skipped,
                "zgc mark: skipped non-registered child pointers (corrupt/stale ref slots)"
            );
        }

        // ---- Finalizer resurrection (see collect_garbage_with_finalizers):
        // mark dead-but-finalizable objects (and their subtrees) live so the
        // sweep keeps them for the finalizer thread. Runs after the main
        // closure so "unmarked" == dead, and before the sweep decides.
        // Everything from the snapshot to here is the mark: bit clearing plus
        // whichever marker ran. Zero on a concurrently-marked cycle, which is
        // the whole point of the phase.
        let mark_us = clock.lap();
        // zr z1/mk: what the serial marker did, so a probe can tell the
        // claim-on-push arm (objects/chunks/stack_peak non-zero) from the `=0`
        // arm (all zero) and read the work-stack allocation the pause paid.
        if let Some(st) = serial_mark_stats {
            if self.gc_log_enabled.load(Ordering::Relaxed) {
                eprintln!(
                    "[GC] zgc-mark-serial: cycle={} mark_us={mark_us} objects={} chunks={} \
                     stack_peak={} claim_on_push={} wild_skipped={} memo_hits={} \
                     memo_misses={} mark_first_skips={} cpu_us={}",
                    self.gc_count.load(Ordering::Relaxed) + 1,
                    st.objects,
                    st.chunks,
                    st.stack_peak,
                    zgc_mark_claim_on_push() as u8,
                    st.wild_skipped,
                    st.memo_hits,
                    st.memo_misses,
                    st.mark_first_skips,
                    st.cpu_ns / 1_000,
                );
            }
        }

        let fin_candidates = std::mem::take(&mut *self.counters.pending_finalizer_roots.lock());
        // CRATONVM_DBG_FINCAND: name the branch each finalizable candidate takes.
        // The three outcomes here are indistinguishable from outside — "never
        // finalized" looks identical whether the address was not registered,
        // or was registered and MARKED (something still roots it). See
        // `recyclertest-thread-not-collected-once-the-jit-warms-up`.
        let dbg_fincand = dbg_fincand();
        let (mut c_unreg, mut c_marked, mut c_dead) = (0usize, 0usize, 0usize);
        if !fin_candidates.is_empty() {
            let mut resurrected = Vec::new();
            // The depth-2 set: every object this pass's `mark_claim` wins, at
            // its pre-relocation address (the slide runs later).
            let mut closure: Vec<usize> = Vec::new();
            for addr in fin_candidates {
                if !registered.contains(addr) {
                    c_unreg += 1;
                    continue; // not a current allocation (already swept earlier)
                }
                if self.mark_is_set(addr) {
                    c_marked += 1;
                    continue; // survived normally — stays registered, not finalized
                }
                c_dead += 1;
                // zt t7/cc: an armed young cycle treats the finalizer
                // reference as strong, as HotSpot's minor does: the object
                // and its closure are kept (marked), NOT reported as
                // resurrected, so its finalizer row stays pending and the
                // next major finalizes it. No depth-2 set either: nothing
                // here is finalizer-reachable only (the weak referents were
                // left in place too).
                if young_refs_strong {
                    self.counters
                        .gen_young_fin_deferred
                        .fetch_add(1, Ordering::Relaxed);
                    work.push(addr);
                    let _ = self.mark_closure_serial(&mut work, &registered, &skip_for, None);
                    continue;
                }
                // Recorded PRE-slide; `collect_garbage` rewrites the list
                // through the pointer map once relocation has run, because the
                // contract with `collect_garbage_with_finalizers`'s caller is
                // POST-move addresses. This line used to say "non-moving:
                // address unchanged".
                resurrected.push(addr);
                // zt t32: under the diagnostic, how much of the finalizee's
                // reach (two levels) the mark had already marked.
                if dbg_fincand {
                    let (mut kids, mut kids_marked, mut gk, mut gk_marked) = (0, 0, 0, 0);
                    let mut level1: Vec<usize> = Vec::new();
                    self.enumerate_references(addr as *mut u8, &mut level1, None);
                    for &c in &level1 {
                        kids += 1;
                        kids_marked += self.mark_is_set(c) as usize;
                        let mut level2: Vec<usize> = Vec::new();
                        self.enumerate_references(c as *mut u8, &mut level2, None);
                        for &g in &level2 {
                            gk += 1;
                            gk_marked += self.mark_is_set(g) as usize;
                        }
                    }
                    eprintln!(
                        "[fincand] premarked fin=0x{addr:x} kids={kids_marked}/{kids} \
                         grandkids={gk_marked}/{gk} closure_before={}",
                        closure.len()
                    );
                }
                work.push(addr);
                // The same closure as the main mark (ZGC-4 wild-child skip,
                // pin and overlay edges included): a resurrected finalizable
                // object's own side-table entries must survive with it.
                // `closure` collects every object this pass marks.
                let _ =
                    self.mark_closure_serial(&mut work, &registered, &skip_for, Some(&mut closure));
            }
            if !resurrected.is_empty() {
                *self.counters.resurrected_finalizers.lock() = resurrected;
            }
            if !closure.is_empty() {
                self.counters.resurrection_closure.lock().extend(closure);
            }
        }
        if dbg_fincand && (c_unreg | c_marked | c_dead) != 0 {
            eprintln!(
                "[fincand] unregistered={c_unreg} marked_alive={c_marked} dead_resurrected={c_dead} \
                 closure={}",
                self.counters.resurrection_closure.lock().len()
            );
        }

        // ---- Reference processing ---------------------------------------
        // Run the shared `gc::reference` processor while the mark bits still
        // reflect this cycle's live set (sweep clears them below). This
        // clears/enqueues weak/soft/phantom/cleaner/finalizer references per
        // HotSpot ordering — without it, finalizers/cleaners and
        // `WeakReference` semantics silently broke under this backend. The
        // enqueue/finalize actions are surfaced for the runtime to drain; the
        // referent-null writes are applied in place here.
        let resurrect_us = clock.lap();

        let ref_result = self.process_references();
        // HIB-CV-24 (ZGC manifestation): capture any armed reconciliation
        // watch at this SAME pre-slide, mark-bitmap-exact point, before sweep
        // clears the mark bits and before relocation can slide an unrelated
        // survivor into a watched address's span. See `zgc_reconcile_watch_map`.
        self.zgc_capture_reconcile_watch();
        if !ref_result.to_enqueue.is_empty()
            || !ref_result.to_finalize.is_empty()
            || !ref_result.cleaner_actions.is_empty()
        {
            // The non-moving STW GC clears referents in place but does not run
            // finalizers or notify ReferenceQueues itself — that is the
            // runtime's job (cf. interpreter `process_references_after_gc`).
            // Surface the pending work so it is observable rather than silently
            // dropped when this backend is driven directly via the trait.
            tracing::debug!(
                target: "zgc",
                to_enqueue = ref_result.to_enqueue.len(),
                to_finalize = ref_result.to_finalize.len(),
                cleaner_actions = ref_result.cleaner_actions.len(),
                "zgc real: reference processing produced pending enqueue/finalize/cleaner work"
            );
        }

        // ---- INT-8 remark: resurrect policy-kept soft referents ----------
        // A soft referent the LRU policy chose to KEEP (not cleared, not
        // enqueued) was never traced by the main mark above — its Reference's
        // slot 0 was hidden by `ref_skip_objs`. If that referent is not ALSO
        // reachable some other way, the sweep below would reclaim it out
        // from under the still-live SoftReference. Mark it (and its
        // transitive closure) alive now, mirroring the interpreter's G1
        // remark step (`soft_survivor_referents`).
        {
            //
            // WITH the pin and overlay edges (zr z1/mk). This loop traced
            // `enumerate_references` alone until 2026-10-12, so a kept soft
            // referent's reference slots survived and its loader / mirror /
            // metadata pins and native collection overlay did not: a
            // policy-kept `SoftReference<HashMap>` whose entries live in an
            // overlay had them swept under a live map -- for a reference held
            // by the PRIVATE processor, i.e. in this file's tests (zr z2/rv:
            // `survivors` is empty in a real run; see the `ref_processor`
            // field). See
            // `mark_closure_serial`, which all three serial loops now share.
            let survivors = self.counters.ref_processor.lock().soft_survivor_referents();
            work.clear();
            work.extend(survivors.into_iter().filter(|&a| registered.contains(a)));
            let _ = self.mark_closure_serial(&mut work, &registered, &skip_for, None);
        }
        // zt t22/zc3 (`CRATONVM_ZGC_CONCURRENT_REFS`): the mark is final --
        // clear the hidden referent slots whose referent died, before the
        // sweep frees it or the slide re-issues its base. One lock when off.
        self.clear_dead_vm_referents_stw();

        // ---- Reference-slot census (diagnostic, default OFF) -------------
        // Placed HERE and nowhere else: this is the last point at which the
        // mark bits are final AND no object has been touched by the sweep. One
        // statement later the sweep clears every survivor's `GC_FLAG_MARKED`
        // and zero-fills every dead object, so a census run from inside or
        // after that loop would see an empty live set and a heap of zeroed
        // headers. It also holds no lock here — the registry guard was released
        // at the mark snapshot and the arena guard is not taken until the sweep
        // block below — which is what lets `for_each_live_object` take the
        // registry lock itself.
        //
        // Cost when disabled: one relaxed `AtomicBool` load and a not-taken
        // branch. `run_walk` re-checks the same gate and returns `None` before
        // touching this heap, so the guard is belt-and-braces; it is written
        // out anyway so the cost is visible at the callsite rather than one
        // module away.
        if self.counters.slot_census.is_enabled() {
            // The result is also logged by `run_walk` itself (target `zgc`,
            // with `legacy_share` and `verdict`), and folded into the census's
            // cumulative counters. Callers that want the per-walk gauge read it
            // back through `slot_census()`.
            let _ = self.counters.slot_census.run_walk(self);
        }

        // ---- EXTENT CENSUS (diagnostic, `CRATONVM_DBG_ZGC_CORPSE`) -------
        //
        // Every registered object's computed extent must end at or below the
        // next registered base. The sweep below depends on it twice over: it
        // zeroes `alloc_size(header)` bytes from a dead object's base and
        // hands the same span to `Arena::add_free_block`. An extent that is
        // too long therefore zeroes the header of the NEXT object and puts its
        // memory on the free list, so the allocator re-issues an address that
        // is still live and still registered -- which is the reopened
        // rewrite-pass page's failure exactly, with no compaction anywhere in
        // the story.
        //
        // Deliberately BEFORE the sweep, on the pre-sweep registry: anything
        // it reports is older than this cycle's zeroing, so the report cannot
        // be an echo of the damage it is looking for.
        if zgc_corpse_enabled() {
            // The one place on this path that still wants a slice, and it is
            // behind a diagnostic flag, so the 87 MB is the operator's choice.
            self.survey_registry_extents(&registered.bases(), "pre-sweep");
        }

        let refs_us = clock.lap();
        // zs s4/ob3: the end of this pause's `Pause Mark End` record; the
        // sweep, the slide and the rest of the pause are `Pause Relocate
        // Start` (`gc_metrics::ZgcPauseSplit`).
        self.counters
            .jfr
            .note_pause_split(marked_concurrently, pause_mark_start_end, clock.at());

        // ---- zr z1/gn: THE AGE-FILTERED NURSERY CENSUS (opt-in) ----------
        //
        // `measurement-nursery-survivor-census-20260921.md` names the one cheap
        // experiment that could still overturn proposal D: the census with
        // ALREADY-OLD objects excluded. Promotion here is a header label, so an
        // object promoted in an earlier young cycle still sits in the nursery's
        // address range and the plain census counts it a survivor on every
        // later cycle of the epoch. A real young space would have evacuated it.
        //
        // HERE, before the sweep, because that is the last point at which the
        // header still says what the object was when the cycle BEGAN (the
        // sweep ages survivors, and a just-promoted object reads exactly like
        // one that was old all along). "Survivor" is the mark bit, which is
        // final at this point (the census below reads the post-sweep registry
        // for the same answer). Same geometry as the plain census, with the
        // pre-sweep low cursor as `end`. A second instrument rather than a
        // change to the first, so the published series keeps its meaning.
        // zr z2/g2: both census walks are timed into `census_us` for
        // `[GC] zgc-gen-cost:`.
        let mut census_us = 0u64;
        if young_cycle && self.counters.gen_census_age_on.load(Ordering::Relaxed) {
            let t = gc_started.map(|_| std::time::Instant::now());
            let (arena_base, low_end) = {
                let arena = self.arena.lock();
                let b = arena.base_ptr() as usize;
                (b, b + arena.used_low_for_compaction())
            };
            let geo = generation::ZNurseryGeometry {
                base: arena_base,
                page_bytes: Self::Z_LOGICAL_PAGE_BYTES,
                floor: sweep_floor,
                end: low_end,
            };
            let census = &self.counters.gen_nursery_survivors_young;
            let mut obs = census.begin_cycle(geo);
            if obs.page_count() != 0 {
                registered.for_each_base_from(sweep_floor, |b| {
                    if !self.mark_is_set(b) {
                        return;
                    }
                    let h = self.header_ref(b as *mut u8);
                    if h.gc_age() >= promo_age {
                        return; // old before this cycle began
                    }
                    obs.note_survivor(b, self.survivor_size(b, h).unwrap_or(0));
                });
            }
            census.finish_cycle(obs);
            census_us += t.map_or(0, |t| t.elapsed().as_micros() as u64);
        }

        // ---- Sweep phase -------------------------------------------------
        //
        // DOES ANYTHING DOWNSTREAM ACTUALLY WANT THE DEAD ADDRESSES?
        //
        // Asked once, here, and not inside the loop. `MonitorCleanup::
        // wants_dead_addresses` is the shard survey `prune_dead` already runs
        // -- moved to before the sweep, because that is where the cost is. On
        // a workload with no inflated monitor and no held CAS lock (its own
        // comment: the common case) the slice would be consumed to remove
        // nothing, and building it is one `usize` per reclaimed object inside
        // the pause: 104 MB on the 13.0M-dead-object cycle this file's pause
        // anatomy is written against.
        //
        // The registry prune does NOT depend on this. It used to be driven
        // from the same slice, which is why the slice looked unconditional; it
        // now happens in `sweep_one`, where the base is already in hand.
        let want_dead = monitors.wants_dead_addresses();
        // zr z5/sw (`CRATONVM_ZGC_SWEEP_LIVE_PROPORTIONAL`, default ON): ask
        // for the cleanup's KEYS instead of handing it every dead base. One
        // inflated monitor anywhere makes `want_dead` true, and the list it
        // asks for is one `usize` per reclaimed object (plus a sizing pass
        // over both bitmaps, plus it switches the sweep's word step off) to
        // remove a handful of entries. Under a stop-the-world token the keys
        // cannot change between here and the prune. Not on a generational
        // cycle (out of this lane's scope: that path keeps the full list).
        let live_proportional = sweep::zgc_sweep_live_proportional_enabled();
        // t1 wave 8 lane ZT3GN (`CRATONVM_ZGC_GEN_PRUNE_KEYS`, default ON inside generational ZGC since 2026-10-27): and on
        // a generational cycle too -- see `young_evacuate::zgc_gen_prune_keys`.
        let gen_keys_ok = !gen_on || self.counters.young_evac.prune_keys.load(Ordering::Relaxed);
        let prune_keys: Option<Vec<usize>> = if want_dead && live_proportional && gen_keys_ok {
            monitors.prune_candidate_keys()
        } else {
            None
        };
        let want_dead = want_dead && prune_keys.is_none();
        // ---- G2e/G2f: THE TWO PER-DEAD-OBJECT COSTS -----------------------
        //
        // §3b measured `sweep` at 182 ms of a 309 ms mean pause and found the
        // generation split did not move it. It did not, because a bounded sweep
        // still pays both of these for every dead object it does visit, and both
        // are the reason a young cycle costs what a major costs:
        //
        //  * a memset of the object's WHOLE BODY -- O(reclaimed volume), i.e. a
        //    900 MB memset on a cycle that reclaims 900 MB, inside the pause;
        //  * a `push_block_routed` per object, and then a sort over all of them
        //    in `coalesce_free_list`.
        //
        // Both are removed in `sweep_one`. `zgc_sweep_header_zero` and
        // `zgc_sweep_dead_runs` carry the arguments; both are read here once
        // rather than per object.
        //
        // EVERY CYCLE, not just a young one. Both were scoped to `young_cycle`
        // when they landed on 2026-08-17, for one reason that has since expired:
        // they went in mid-gauntlet and a default run had to stay byte-for-byte
        // unchanged. Neither ARGUMENT was ever young-specific -- the body is
        // unreachable behind a zeroed header whatever the cycle kind, and the
        // walk is ascending on both paths.
        //
        // And §3d says the restriction pointed them away from the cost. A young
        // cycle's sweep is 6.3 ms of a 112 ms pause (5.6%); a WHOLE-HEAP cycle's
        // is 170 ms of 265 ms (64%), because it walks 13.0M dead objects against
        // a young cycle's 137k. The measured -30% was being applied to the small
        // one.
        //
        // THE ARENA GUARD IS NOT HELD FOR THE WALK ANY MORE. It used to wrap
        // the whole loop, which is what made the sweep unshardable. Only two
        // numbers were ever read from it (`base_ptr`, `high_cursor`), and both
        // are constant for the duration of a stop-the-world pause; the free
        // spans the walk produces are handed over afterwards, in one batch, in
        // the same ascending order `Arena::coalesce_free_list` requires. The
        // per-object `write_bytes` writes arena BYTES, which the guard never
        // protected -- it protects the free-list metadata.
        //
        // zr z9/rv8 (`CRATONVM_ZGC_LAZY_TLAB_STARTS` only; one relaxed load
        // otherwise): forget the lazy-starts record of every live buffer this
        // pause did not publish, before the sweep frees what lies below its
        // unknown cursor.
        self.lazy_note_collection();
        // zt t6/tc (GN-5, `CRATONVM_ZGC_GEN_SYSTEM_GC_TENURES`): a whole-heap
        // generational cycle run for a `System.gc()` door that tenures every
        // live young object (`gc_quiescence::request_system_gc_tenure`, read on
        // the collecting thread) promotes every survivor in its sweep. Lowered
        // after the sweep (below the spill pass).
        let tenure_all = gen_on
            && !young_cycle
            && cratonvm_gc_core::gc_quiescence::system_gc_tenure_requested()
            && zgc_flag_default("CRATONVM_ZGC_GEN_SYSTEM_GC_TENURES", true);
        if tenure_all {
            self.counters
                .gen_tenure_all_cycle
                .store(true, Ordering::Relaxed);
            self.counters
                .gen_tenure_all_sweeps
                .fetch_add(1, Ordering::Relaxed);
        }
        // zt t27/pc2 (`CRATONVM_ZGC_SWEEP_SIZE_MEMO`): EVERY collection takes
        // the last sweep's memo out, so one that does not use it (a deferred
        // sweep, a young cycle, the switch off) drops it rather than leaving
        // it for a later sweep it no longer describes. It is checked here,
        // before this cycle's `gc_count` increment, against the count it was
        // stamped for, and handed to the sweep below the deferral decision.
        let size_memo_taken = self.counters.size_memo.lock().take().filter(|m| {
            zgc_sweep_size_memo_enabled()
                && !gen_on
                && !young_cycle
                && sweep_floor == 0
                && m.valid_for(self.gc_count(), cratonvm_types::layout_replace_epoch())
        });
        let mut cfg = {
            let arena = self.arena.lock();
            ZSweepCfg {
                arena_base: arena.base_ptr() as usize,
                high_floor: arena.high_cursor(),
                arena_used: arena.used(),
                zero_header_only: self.gen_header_zero_only.load(Ordering::Relaxed),
                // zr z1/al, default ON since zr z8/zb; per cycle. The corpse
                // forensics need the zeroed headers, so they force it off.
                skip_dead_headers: sweep::zgc_sweep_skip_dead_headers_enabled()
                    && !zgc_corpse_enabled(),
                merge_dead_runs: self.gen_dead_runs_enabled.load(Ordering::Relaxed),
                // Can the mark bits be cleared in bulk after the walk, rather
                // than one survivor's header at a time inside it?
                bulk_clearable: self.mark_bits.is_some(),
                want_dead,
                // gc-common w29-b: no consumer. The dead objects' identity
                // hashes fed `cratonvm_types::identity_side_tables`, whose only
                // evictor (`securerandom`'s generator tables) is keyed by weak
                // lock keys now, freed per VM on every backend; the channel
                // (VM-less, ZGC-only) was removed. The sweep-side plumbing
                // goes with `handoff-w29b-retire-zgc-dead-hash-plumbing.md`.
                collect_dead_hashes: false,
                // zr z5/sw: the region step over mark-summary-empty blocks.
                region_skip: live_proportional && !gen_on,
                gen_on,
                promo_age,
                // zr z7/pc (`CRATONVM_ZGC_SWEEP_SIZE_HINTS` under
                // `CRATONVM_ZGC_RELOCATE_SIZE_TABLE`): record the survivors'
                // size cells only when a slide is expected after this sweep. A
                // wrong guess costs a buffer, never an answer: the slide checks
                // every hint against its own live set by address.
                record_live_cells: zgc_relocate_size_table_enabled()
                    && zgc_sweep_size_hints_enabled()
                    && self.relocation_requested()
                    && !young_cycle,
                // zt t26/pc1 (`CRATONVM_ZGC_SWEEP_RESERVE_HINTS`): last cycle's
                // hint count plus 1/16, never above this cycle's registered
                // count. The sweep applies it only under `record_live_cells`.
                reserve_live_hints: {
                    let last = self.counters.sweep_live_hints_last.load(Ordering::Relaxed);
                    last.saturating_add(last / 16).min(registered_count)
                },
                // zt t27/pc2: set below the deferral decision (a deferred
                // sweep runs outside this pause and takes no memo).
                size_memo: None,
                // zt t16/gn6 (GN-10): `minor_sweep_walk_us=`, under the same
                // condition as the rest of `[GC] zgc-gen-cost:`.
                time_walk: gen_on && young_cycle && gc_started.is_some(),
            }
        };

        // zs s3/cr (`CRATONVM_ZGC_EVAC_QUARANTINE`): the spans the last
        // evacuating slide withheld go back to the free list HERE, after this
        // cycle's mark and before its sweep, whichever sweep arm runs. A no-op
        // (one uncontended lock) when nothing is quarantined.
        self.release_evac_quarantine();
        // zt t23/zc5 (R1, `CRATONVM_DBG_ZGC_RELOC_SELECT_CENSUS=1`, diagnostic):
        // the relocation set chosen from this cycle's mark, HERE -- mark bits
        // final, nothing swept yet, ahead of both sweep arms -- for the
        // `[GC] zgc-rsel:` comparison with the slide's own selection. Reads
        // only; one flag read when off. See `reloc_select_census_begin`.
        self.reloc_select_census_begin(&registered, sweep_floor);
        // ---- zt t21/zc2: THE SWEEP LEAVES THE PAUSE (opt-in) ------------------
        //
        // `CRATONVM_ZGC_CONCURRENT_SWEEP`: a cycle the screen admits ends its
        // pause here -- HotSpot's Pause Mark End -- and parks the sweep; the
        // collecting thread runs it after the release, and its after-sweep hook
        // holds every consumer of the sweep's totals that follows below (see
        // `concurrent_sweep.rs` for the split and its invariants). Everything
        // from here to the end of this function then does not run for that
        // cycle; the compaction block in particular does not (a deferred cycle
        // never slides). Refused cycles, and every cycle with the switch off,
        // take the code below unchanged. One relaxed load when off.
        if self.counters.csweep.enabled() {
            match self.concurrent_sweep_refusal(
                &registered,
                &cfg,
                sweep_floor,
                young_cycle,
                want_dead,
                after_alloc_failure,
            ) {
                None => {
                    let mark_kind = if marked_concurrently {
                        "concurrent"
                    } else if parallel_workers >= 1 && parallel_ok {
                        "stw-parallel"
                    } else {
                        "stw-serial"
                    };
                    // zt t23/zc5 (R1): a deferred cycle never slides, so its
                    // census line has no slide side (`rsel_compared=0`).
                    self.reloc_select_census_emit();
                    // zt t32 (`CRATONVM_ZGC_CONCURRENT_REFERENT_CLEAR`): the
                    // staged referent clear runs after the release, behind
                    // the read gate. A no-op when nothing was staged.
                    self.arm_referent_read_gate();
                    // zt t32 (R6, 6b, `CRATONVM_ZGC_CONCURRENT_RELOCATE`): a
                    // deferred cycle never slides, but it may PLAN: the same
                    // request and refusals as the slide below (the refusal
                    // asked without retracting the cursor, which the parked
                    // sweep's install owns), then the plan's own admission.
                    let relocate_roots = (self.relocation_requested()
                        && !gen_on
                        && !young_cycle
                        && self.counters.crl.enabled()
                        && self.relocate_refusal_with(false).is_none()
                        && self.concurrent_relocate_admitted(young_cycle))
                    .then_some(&mut *roots);
                    self.finalize_discovered_weak_phantom();
                    return self.defer_sweep_and_close_pause(
                        concurrent_sweep::ZDeferredPause {
                            registered,
                            cfg,
                            registered_count,
                            prune_keys,
                            gc_started,
                            laps: ZCycleLaps {
                                timed: gc_started.is_some(),
                                marked_concurrently,
                                relocated: false,
                                markend_us,
                                tlab_us,
                                snapshot_us,
                                mark_us,
                                resurrect_us,
                                refs_us,
                                sweep_us: 0,
                                relocate_us: 0,
                            },
                            clock,
                            mark_kind,
                            bytes_in_use_before,
                            pace_near_full,
                            pace_window,
                            pause_cpu: &pause_cpu,
                            cpu_before,
                            relocate_roots,
                        },
                        monitors,
                    );
                }
                Some(reason) => self.note_concurrent_sweep_refusal(reason),
            }
        }
        // zt t32: this cycle sweeps in its pause, so a staged referent clear
        // runs here, before the sweep frees or the slide re-issues anything.
        self.run_staged_referent_clear();
        // zt t27/pc2: this sweep runs inside the pause; hand it the memo.
        cfg.size_memo = size_memo_taken;
        // THE BITMAP SWEEP, when the cycle's shape allows it: whole-heap (a
        // young cycle's floor makes the complement wrong -- it would reclaim
        // the dead below the floor that a minor deliberately leaves for the
        // next major), side mark bits, and the switch on. It returns the free
        // list ALREADY BUILT, so the whole rebuild-and-sort tail below is
        // skipped with it. See `sweep_bitmap`.
        //
        // zt t26/pc1: `CRATONVM_DBG_ZGC_RELOCATE_PHASES` also splits this
        // cycle's `sweep_us` lap -- the walk, the free-list install, and the
        // per-cycle passes after it -- on a `[zgc-sweep-phases]` line.
        let sweep_split_t0 = zgc_dbg_relocate_phases_enabled().then(std::time::Instant::now);
        let sweep_workers =
            if !gen_on && self.counters.csweep.enabled() && self.sweep_workers(gen_on) == 1 && true
            {
                self.conc_sweep_workers()
            } else {
                self.sweep_workers(gen_on)
            };
        //
        // `sweep_workers` REACHES THIS ARM NOW. Until 2026-09-04 the complement
        // sweep was serial and won the selection before the worker count was
        // read, so `CRATONVM_ZGC_PARSWEEP=<n>` parsed, clamped, reported, and
        // did nothing -- a switch that answers about a run that never sharded.
        // The complement chains `prev_end` across the whole address space, so
        // sharding it needed the seam join in `ZgcRealHeap::join_complement`
        // rather than a loop split.
        let bitmap_swept = (sweep_floor == 0 && self.bitmap_sweep_enabled())
            .then(|| self.sweep_bitmap(&registered, &cfg, sweep_workers))
            .flatten();
        // zr z1/gn: the below-floor slice of the young-aged sweep's accounting,
        // `(visited, bytes_freed, bytes_copied, promoted_bytes)`; zero on every
        // other arm.
        let mut young_aged_below = (0usize, 0usize, 0usize, 0usize);
        // zr z2/g2: the YOUNG complement sweep (default ON since zt t14/gn5). `young_install` is
        // `(nursery spans, new cursor offset, floor offset, low bytes pass (1)
        // freed)` for the installer below; `None` on every other arm.
        let mut young_install: Option<(Vec<(usize, usize)>, usize, usize, usize)> = None;
        // zt t16/gn5fix: the complement's dead runs (`ZYoungComplement::runs`).
        let mut young_runs = 0usize;
        // t1 wave 8 lane ZT3GN: the complement's below-floor windows.
        let mut young_below_windows: Vec<(usize, usize)> = Vec::new();
        let young_comp = if bitmap_swept.is_none()
            && young_cycle
            && self
                .counters
                .gen_young_complement_on
                .load(Ordering::Relaxed)
        {
            match self.sweep_young_complement(&registered, sweep_floor, &cfg, young_aged_sweep) {
                Ok(yc) => {
                    self.counters
                        .gen_young_complement_sweeps
                        .fetch_add(1, Ordering::Relaxed);
                    Some(yc)
                }
                Err(reason) => {
                    self.counters
                        .gen_young_complement_refusals
                        .fetch_add(1, Ordering::Relaxed);
                    self.counters
                        .gen_young_complement_refusal
                        .store(reason, Ordering::Relaxed);
                    None
                }
            }
        } else {
            None
        };
        let (mut swept_total, complement) = match bitmap_swept {
            Some((sh, spans, new_cursor)) => (sh, Some((spans, new_cursor))),
            None if young_comp.is_some() => {
                let yc = young_comp.expect("checked by the guard");
                if young_aged_sweep {
                    young_aged_below = yc.below;
                }
                young_runs = yc.runs;
                if zgc_dbg_relocate_phases_enabled() {
                    let (words, live_old, live_young, bulk, p1, p2) = yc.diag;
                    eprintln!(
                        "[zgc-young-complement] cycle={} floor_off={} cursor_off={} \
                         words={words} live_old={live_old} live_young={live_young} \
                         dead={} bulk_words={bulk} spans={} pass1_us={p1} pass2_us={p2}",
                        self.gc_count.load(Ordering::Relaxed) + 1,
                        yc.floor_off,
                        yc.new_cursor_off,
                        yc.sh.dead_count,
                        yc.spans.len(),
                    );
                }
                young_below_windows = yc.below_windows;
                young_install = Some((
                    yc.spans,
                    yc.new_cursor_off,
                    yc.floor_off,
                    yc.low_per_object_freed,
                ));
                (yc.sh, None)
            }
            None if young_aged_sweep => {
                let (sh, below) = self.sweep_young_aged(&registered, sweep_floor, &cfg);
                young_aged_below = below;
                (sh, None)
            }
            None if sweep_workers > 1 => (
                self.sweep_parallel(&registered, sweep_floor, &cfg, sweep_workers),
                None,
            ),
            None => (self.sweep_serial(&registered, sweep_floor, &cfg), None),
        };
        // THE SPILL SET, ALWAYS ON THIS THREAD. It belongs to no word range and
        // is not address-ordered, so it can neither be partitioned nor merged
        // into a run -- which is why the flush above it is load-bearing. Empty
        // on every healthy run.
        swept_total.flush();
        // zt t16/gn6 (GN-10): the young-aged walk's idle stretches, before the
        // spill pass adds per-object work to the same shard.
        let sweep_walk_ns = swept_total.walk_ns;
        // zr z7/pc: the bitmap arm's survivor size cells, for the slide below.
        let sweep_hints = ZSweepSizeHints {
            addrs: std::mem::take(&mut swept_total.live_addrs),
            cells: std::mem::take(&mut swept_total.live_cells),
        };
        // zt t26/pc1 (`CRATONVM_ZGC_SWEEP_RESERVE_HINTS`): next cycle's
        // reservation. Only a cycle that recorded hints speaks for the count.
        if cfg.record_live_cells {
            self.counters
                .sweep_live_hints_last
                .store(sweep_hints.addrs.len(), Ordering::Relaxed);
        }
        // The young-aged sweep owns every non-old base wherever it sits, so its
        // spill pass is unbounded too (an old spilled base is pre-marked and
        // survives `sweep_one`).
        let spill_floor = if young_aged_sweep { 0 } else { sweep_floor };
        registered.for_each_spilled_base(spill_floor, |base| {
            self.sweep_one(base, &cfg, &mut swept_total, Some(&registered))
        });
        swept_total.flush();
        // zt t27/pc2: the memo has served its one sweep; free it before the
        // slide builds this cycle's (below, after the relocation block).
        cfg.size_memo = None;
        let sweep_split_walked = sweep_split_t0.map(|_| std::time::Instant::now());
        if tenure_all {
            self.counters
                .gen_tenure_all_cycle
                .store(false, Ordering::Relaxed);
        }

        let bytes_copied = swept_total.bytes_copied;
        let objects_copied = swept_total.objects_copied;
        // `mut`: the complement sweep learns what it reclaimed from the ARENA
        // (how much the rebuilt free list gained, plus what the cursor gave
        // back), which is only knowable inside the lock below.
        let mut bytes_freed = swept_total.bytes_freed;
        let dead_count = swept_total.dead_count;
        let unsizable = swept_total.unsizable;
        let gen_promoted = swept_total.gen_promoted;
        let gen_promoted_bytes = swept_total.gen_promoted_bytes;
        // zt t5/gn (`CRATONVM_ZGC_GEN_HIGH_YOUNG_LIVE`): see
        // `gen_old_live_after_major`.
        let gen_young_high_bytes = swept_total.gen_young_high_bytes;
        // zt t6/tc (GN-18, `CRATONVM_ZGC_GEN_FLOOR_ARM_HIGH_OLD_LIVE`): idem.
        let gen_old_high_bytes = swept_total.gen_old_high_bytes;
        let swept = swept_total.swept;
        let zero_bytes_skipped = swept_total.zero_bytes_skipped;
        let dead_headers_skipped = swept_total.dead_headers_skipped;
        let dead_in_runs = swept_total.dead_in_runs;
        // zt t16/gn5fix: the young complement keeps its nursery spans in
        // `young_install`, not in the shard, while its dead count is in
        // `dead_in_runs`; without them `gen_dead_runs` read 0 against a
        // nonzero `gen_dead_objects` once the complement became the default
        // (`a_young_sweep_hands_the_free_list_one_span_per_run_of_dead_objects`).
        // Each complement run is a gap between survivors or the retracted
        // tail (it may also cover free space the nursery already had).
        let dead_runs = swept_total.spans.len() + young_runs;
        // The reused buffer, filled only when `want_dead` said so -- see
        // `dead_scratch`.
        let dbg_i0 = sweep_split_t0.map(|_| std::time::Instant::now());
        let mut dead_guard = self.counters.dead_scratch.lock();
        let dead: &mut Vec<usize> = &mut dead_guard;
        // TAKE THE SWEPT BUFFER instead of copying into the scratch.
        //
        // `clear` + `append` copied the whole list a THIRD time -- 1.33 % of
        // all busy samples on `CratonBench bintrees -Xmx512m`, inside the
        // pause, to move 96 MB from one buffer into another of the same size.
        // The sweep now sizes its list exactly (`sweep_bitmap_range` reserves
        // from a count of its own dead bits, `join_complement` from the shard
        // total), so the buffer arriving here is already the right shape and
        // the scratch's reuse is preserved by adopting it: the allocation the
        // scratch gives up is the one `swept_total` is dropped with.
        //
        // [`swap_is_worth_it`] is what keeps
        // `the_dead_scratch_is_cleared_and_its_capacity_reused` true -- see
        // there for why the test is about a SMALL cycle following a large one
        // and why an exact `>=` would be wrong.
        if swap_is_worth_it(swept_total.dead.capacity(), dead.capacity()) {
            std::mem::swap(dead, &mut swept_total.dead);
        } else {
            dead.clear();
            dead.append(&mut swept_total.dead);
        }
        // zr z5/sw: THE KEY PROBE, in place of the dead list. A key died this
        // cycle iff the cycle's snapshot registered it and the sweep has since
        // unregistered it -- exactly the set every sweep arm pushed into the
        // list (`retain_marked` / `remove` drop a base precisely when it is
        // reclaimed; an unsizable base is retained or re-inserted, and was
        // never listed). Decided HERE, after the sweep and BEFORE the slide:
        // the slide re-registers survivors at addresses that may be dead bases,
        // which is the collision `prune_dead`'s ordering comment describes.
        // Ascending, like the list it replaces.
        if let Some(keys) = &prune_keys {
            debug_assert!(
                dead.is_empty(),
                "want_dead was off, the sweep listed nothing"
            );
            dead.clear();
            dead.extend(
                keys.iter()
                    .copied()
                    .filter(|&k| registered.contains(k) && !self.registry.contains(k)),
            );
            dead.sort_unstable();
            dead.dedup();
            self.counters
                .sweep_prune_key_cycles
                .fetch_add(1, Ordering::Relaxed);
            self.counters
                .sweep_prune_keys_probed
                .fetch_add(keys.len(), Ordering::Relaxed);
            self.counters
                .sweep_prune_keys_dead
                .fetch_add(dead.len(), Ordering::Relaxed);
        }
        if swept_total.region_words_skipped != 0 {
            self.counters
                .sweep_region_words_skipped
                .fetch_add(swept_total.region_words_skipped, Ordering::Relaxed);
        }
        let mut dbg_i2: Option<std::time::Instant> = None;
        let dbg_i1 = sweep_split_t0.map(|_| std::time::Instant::now());
        {
            let mut arena = self.arena.lock();
            // ASCENDING, which the coalescer below depends on: adjacent dead
            // objects hand adjacent spans to `add_free_block`, and it can only
            // see two spans as adjacent if they arrive in order. The bitmap scan
            // IS that order within a shard, and `ZSweepShard::absorb` preserves
            // it across shards -- joining the seam where two shards' runs touch.
            // zr z2/g2: the young complement replaces the nursery's part of the
            // low list and keeps the part below the floor -- see
            // `install_young_complement`. Pass (1)'s low bytes were counted in
            // the shard AND are in the arena's growth, so they leave the
            // shard's figure here.
            if let Some((spans, new_cursor_off, floor_off, low_pass1)) = &young_install {
                bytes_freed = bytes_freed.saturating_sub(*low_pass1);
                let (freed, in_windows) = Self::install_young_complement(
                    &mut arena,
                    spans,
                    *floor_off,
                    *new_cursor_off,
                    &mut swept_total.spans,
                    &young_below_windows,
                );
                bytes_freed += freed;
                // ZT3GN: the windows' bytes that were free already were not
                // freed below the floor by this cycle.
                young_aged_below.1 = young_aged_below.1.saturating_sub(in_windows);
            }
            dbg_i2 = sweep_split_t0.map(|_| std::time::Instant::now());
            match &complement {
                // The bitmap sweep computed the whole LOW free list as the
                // complement of the live set: address-ordered and merged
                // already, so it is installed rather than accumulated, and
                // neither the coalescer below nor the tail retraction has
                // anything left to sort. `spans` still carries the
                // large-object end's individual holes, which route by offset.
                Some((spans, new_cursor)) => {
                    let was_free = arena.rebuild_low_free_list(spans);
                    let now_free: usize = spans.iter().map(|(_, len)| *len).sum();
                    // What this cycle FREED, as opposed to what is free: the
                    // complement includes the holes that were already on the
                    // list when the sweep started.
                    bytes_freed += now_free.saturating_sub(was_free);
                    bytes_freed += arena.retract_cursor_to(*new_cursor);
                    for (off, len) in swept_total.spans.drain(..) {
                        arena.add_free_block(off, len);
                    }
                }
                None => {
                    for (off, len) in swept_total.spans.drain(..) {
                        arena.add_free_block(off, len);
                    }
                }
            }

            // Coalesce the free list into maximal spans — same rationale as
            // gen_heap's post-sweep coalescer. The walk above returns ONE
            // object-sized hole per dead object, and `Arena::alloc`'s
            // small-tier scan is BUDGETED (16 entries): a workload whose
            // dead objects mix sizes (e.g. runs of 72-byte boxes burying
            // 296-byte byte[] holes) then misses reusable holes forever and
            // burns bump space until the arena exhausts — CopyChurn at
            // -Xmx256m OOM'd with most of the arena sitting unreachable on
            // the free list. Merging adjacent (and defensively overlapping)
            // holes rebuilds them into a few large spans that route to the
            // unbounded large tier, restoring reuse.
            //
            // The merge itself now lives on `Arena` (`coalesce_free_list`), so
            // this sweep and the last-resort merge `Arena::alloc` runs before
            // it returns `None` cannot drift apart.
            // (The young complement's list is merged by construction too.)
            if complement.is_none() && young_install.is_none() {
                arena.coalesce_free_list();
            }
            // The same two steps for the LARGE-OBJECT end. They are separate
            // calls, not a wider version of the two above, because a merge
            // that straddled the point where the two cursors meet would be
            // re-routed into one region by its offset alone — see
            // `Arena::coalesce_high`. Cheap: this list holds large objects, so
            // it is three orders of magnitude shorter than the low one.
            arena.coalesce_high();
            let reclaimed_high = arena.retract_high_cursor_into_free_head();
            if reclaimed_high != 0 {
                tracing::debug!(
                    target: "cratonvm::gc",
                    bytes = reclaimed_high,
                    high_cursor = arena.high_cursor(),
                    "zgc sweep: retracted the large-object cursor into a free head",
                );
            }
            // Un-bump a wholly-free tail. Coalescing above has made the topmost
            // span maximal, so this is one comparison — and it is the only
            // thing on a non-compacting heap that can restore a large
            // CONTIGUOUS region. Objects die young, so the top of the arena is
            // usually all garbage; without this the cursor is a one-way ratchet
            // and a 16 MB array becomes unservable forever once the process has
            // allocated its capacity, with 1.8 GB free and 15% live. See
            // `Arena::retract_cursor_into_free_tail`.
            // WHERE THE SWEPT BYTES ACTUALLY WENT.
            //
            // `bytes_freed` is what the sweep handed to `add_free_block`; this
            // is what the arena has to show for it, measured at the same
            // instant with the same lock held. A large `bytes_freed` beside a
            // small `free_bytes_total` is memory that was swept and then lost,
            // and it is the difference between "this heap fragments" and "this
            // heap leaks" -- which want opposite fixes.
            //
            // Under `--verbose:gc` only, next to the per-cycle line it
            // explains.
            if self.gc_log_enabled.load(Ordering::Relaxed) {
                eprintln!(
                    "[GC] zgc-reclaim: bytes_freed={} free_list_bytes={} \
                     largest_free_block={} cursor={} capacity={} committed={} \
                     registered={} reset_total={}",
                    bytes_freed,
                    arena.free_list_bytes(),
                    arena.largest_free_block(),
                    arena.used(),
                    arena.capacity(),
                    // WHAT THE PROCESS IS ACTUALLY HOLDING, beside what the
                    // heap is allowed to hold. Equal to `capacity` on the
                    // wholly-committed fallback store and well below it on a
                    // reserving one, which is the difference between "-Xmx is a
                    // ceiling" and "-Xmx is a charge".
                    arena.committed_bytes(),
                    registered_count,
                    // Given back while staying mapped
                    // (`CRATONVM_ZGC_READABLE_GIVE_BACK`); 0 when that is off.
                    arena.bytes_reset(),
                );
            }
            let reclaimed_tail = arena.retract_cursor_into_free_tail();
            if reclaimed_tail != 0 {
                tracing::debug!(
                    target: "cratonvm::gc",
                    bytes = reclaimed_tail,
                    cursor = arena.used(),
                    "zgc sweep: retracted the bump cursor into a free tail",
                );
            }
            // zr z3/fp (`CRATONVM_ZGC_UNCOMMIT_STAMP_AT_SWEEP`): the free list
            // and both cursors are final for this cycle, so every granule that
            // is unused now starts its `ZUncommitDelay` clock now, rather than
            // at the next collection's give-back. Stamps only; the release
            // stays in the safepoint window at the start of a collection.
            if self.counters.fp.stamp_at_sweep_on.load(Ordering::Relaxed) {
                let stamped = arena.stamp_idle_unused();
                self.counters
                    .fp
                    .idle_stamps
                    .fetch_add(stamped, Ordering::Relaxed);
            }
        }
        let sweep_split_installed = sweep_split_t0.map(|_| std::time::Instant::now());

        // ---- PHASE G bookkeeping, after the sweep and before the log -----
        //
        // THE LIVE FIGURE HAS TO INCLUDE WHAT THE SWEEP DID NOT VISIT. A young
        // cycle's `bytes_copied` counts only survivors at or above the floor, and
        // `allocated` drives both `gc_rearm` and the collection threshold: left
        // uncorrected the heap would read as nearly empty after every young
        // cycle, and the trigger would stop firing until an allocation failed.
        //
        // `objects_copied` is deliberately NOT corrected: it is reported as
        // "survivors this cycle examined", and a young cycle examined only the
        // nursery.
        let mut live_bytes = bytes_copied;
        if young_cycle && young_aged_sweep {
            // zr z1/gn: the young-aged sweep visits young objects BELOW the floor
            // too (already inside `gen_old_live_bytes`) and does not visit old
            // objects promoted inside the nursery (`gen_nursery_old_bytes`).
            let (below_visited, below_freed, below_copied, _below_promoted) = young_aged_below;
            let old_live = self
                .gen_old_live_bytes
                .load(Ordering::Relaxed)
                .saturating_sub(below_freed);
            self.gen_old_live_bytes.store(old_live, Ordering::Relaxed);
            let nursery_old = self.counters.gen_nursery_old_bytes.load(Ordering::Relaxed);
            live_bytes = bytes_copied
                .saturating_sub(below_copied)
                .saturating_add(old_live)
                .saturating_add(nursery_old);
            self.counters
                .gen_young_aged_sweeps
                .fetch_add(1, Ordering::Relaxed);
            self.counters
                .gen_below_floor_visited
                .fetch_add(below_visited, Ordering::Relaxed);
            // BYTES freed below the floor: what the floor-bounded sweep would
            // have left registered and unmarked.
            self.counters
                .gen_below_floor_freed_bytes
                .fetch_add(below_freed, Ordering::Relaxed);
            self.counters
                .gen_sweep_skipped
                .fetch_add(registered_count.saturating_sub(swept), Ordering::Relaxed);
        } else if young_cycle {
            live_bytes = live_bytes.saturating_add(self.gen_old_live_bytes.load(Ordering::Relaxed));
            self.counters
                .gen_sweep_skipped
                .fetch_add(registered_count.saturating_sub(swept), Ordering::Relaxed);
        }
        // The engagement counters for the two cost reductions, published
        // whether or not this cycle was young -- so a run whose young cycles all
        // ended up as majors reports zeroes rather than nothing.
        if zero_bytes_skipped != 0 {
            self.counters
                .gen_zero_bytes_skipped
                .fetch_add(zero_bytes_skipped, Ordering::Relaxed);
        }
        if cfg.skip_dead_headers {
            // The opt-in's engagement counter, and its per-cycle line while it
            // is on and the GC log is (`--verbose:gc`), so a row reads it
            // without a rebuild (zr z1/al). Gated on the log like the
            // `zgc-pause:` line: an unasked-for stderr line per cycle is the
            // pipe-deadlock shape recorded there.
            let total = self
                .counters
                .sweep_dead_headers_skipped
                .fetch_add(dead_headers_skipped, Ordering::Relaxed)
                + dead_headers_skipped;
            // zr z8/zb: and only when the switch was set explicitly -- it is
            // default ON now, and a default run must not gain a stderr line.
            if self.gc_log_enabled.load(Ordering::Relaxed)
                && sweep::zgc_sweep_skip_dead_headers_explicit()
            {
                eprintln!(
                    "[GC] zgc-sweep-skip-dead-headers: cycle_skipped={dead_headers_skipped} \
                     total_skipped={total} dead={dead_count} want_dead={}",
                    cfg.want_dead
                );
            }
        }
        if dead_in_runs != 0 {
            self.counters
                .gen_dead_runs
                .fetch_add(dead_runs, Ordering::Relaxed);
            self.counters
                .gen_dead_objects
                .fetch_add(dead_in_runs, Ordering::Relaxed);
        }
        if gen_promoted != 0 {
            self.counters
                .gen_promotions
                .fetch_add(gen_promoted, Ordering::Relaxed);
            // The BYTES beside the count, for the GC-overhead productivity
            // metric -- see `ZCounters::gen_promoted_bytes`. Inside the same
            // guard because the two are produced by the same sweep arm: a
            // cycle that promoted nothing has nothing to add to either.
            self.counters
                .gen_promoted_bytes
                .fetch_add(gen_promoted_bytes, Ordering::Relaxed);
            // zr z2/g2: the old-growth major rule's input; reset by every
            // whole-heap cycle below.
            self.counters
                .gen_promoted_since_major
                .fetch_add(gen_promoted_bytes, Ordering::Relaxed);
            // zr z1/gn: promotions at or above the floor become old objects the
            // young-aged sweep will not visit again; carry their bytes. (Below
            // the floor they are inside `gen_old_live_bytes` already.)
            if young_cycle && young_aged_sweep {
                self.counters.gen_nursery_old_bytes.fetch_add(
                    gen_promoted_bytes.saturating_sub(young_aged_below.3),
                    Ordering::Relaxed,
                );
            }
            // ARM THE BARRIER. Published after the sweep has set every one of
            // those ages and written every promotion card, so a store that sees
            // this bit set finds a table that already names the objects it has
            // to name.
            self.store_jit_gate(|g| &g.post_active, 1);
            self.has_old_objects.store(true, Ordering::Relaxed);
        }
        // zt t10/gn2: the young cycle's promotion volume for the `-Xlog` young
        // table (`gen_last_young_xlog_figures`); every young cycle overwrites it.
        if young_cycle {
            self.counters
                .gen_last_young_promoted_bytes
                .store(gen_promoted_bytes, Ordering::Relaxed);
            // zt t11/gn3 (x8 C8): the below-floor young survivors the old
            // figure still holds, and whether the old figure is this arm's.
            // Off the young-aged sweep `gen_old_live_bytes` holds neither this
            // cycle's promotions above the floor nor a correction for them, so
            // the young split is withheld there (`gen_last_young_xlog_figures`).
            let (_, _, below_copied, below_promoted) = young_aged_below;
            let below_young = if young_aged_sweep {
                below_copied.saturating_sub(below_promoted)
            } else {
                0
            };
            self.counters
                .gen_last_young_below_young_bytes
                .store(below_young, Ordering::Relaxed);
            self.counters
                .gen_last_young_xlog_exact
                .store(young_aged_sweep, Ordering::Relaxed);
            // zt t16/gn6 (item 11): ...and, unlike the bit above, lowered by
            // every whole-heap cycle (the `else` arm), so the JMX correction reads
            // `below_young` only while it still describes the heap.
            self.counters
                .gen_last_cycle_young_aged
                .store(young_aged_sweep, Ordering::Relaxed);
            // ---- zt t21/pl10 accounting (item 11): begin ----
            self.counters
                .gen_last_major_young_bytes
                .store(0, Ordering::Relaxed);
            // ---- zt t21/pl10 accounting: end ----
        } else {
            self.counters
                .gen_last_cycle_young_aged
                .store(false, Ordering::Relaxed);
            // ---- zt t21/pl10 accounting (item 11): begin ----
            // The whole-heap cycle's young-by-age survivors below the
            // large-object region (`gen_old_live_bytes` holds them).
            self.counters
                .gen_last_major_young_bytes
                .store(swept_total.gen_young_low_bytes, Ordering::Relaxed);
            // ---- zt t21/pl10 accounting: end ----
        }
        // zt t20/gn7: the warm-up hold's tally is once per interval.
        self.counters
            .gen_nursery_warmup_held_latch
            .store(false, Ordering::Relaxed);
        if self.gen_nursery_triggered.swap(false, Ordering::Relaxed) {
            self.counters
                .gen_nursery_triggers
                .fetch_add(1, Ordering::Relaxed);
            // HOW FAR PAST THE BUDGET THE NURSERY GOT. Measured from
            // `self.allocated` -- which this collection has NOT reset yet, the
            // reset being ~60 lines below -- against the same watermark and
            // budget the trigger compared, which is likewise still the old one.
            // Nobody allocates during the stop-the-world, so this is exactly the
            // figure `needs_gc` last saw. See `gen_nursery_overshoot_max` for why this is a gauge
            // on an open item rather than a defect.
            let budget = self.gen_nursery_bytes.load(Ordering::Relaxed);
            let grew = self
                .allocated
                .load(Ordering::Relaxed)
                .saturating_sub(self.cycle_alloc_watermark.load(Ordering::Relaxed));
            let over = grew.saturating_sub(budget);
            self.counters
                .gen_nursery_overshoot_max
                .fetch_max(over, Ordering::Relaxed);
        }
        if young_cycle {
            self.counters.young_cycles.fetch_add(1, Ordering::Relaxed);
            self.minors_since_major.fetch_add(1, Ordering::Relaxed);
            // ---- THE NURSERY SURVIVOR CENSUS ------------------------------
            //
            // Applies
            // `docs/feature-designs/zgc-architecture.md`.
            // One bit per 2 MiB logical page of nursery: did it hold a
            // survivor? That single number refutes or supports
            // `proposal-d-real-young-space.md` for the cost of one pass, BEFORE
            // a second allocator and a second TLAB path are written. The rule
            // is on `generation::ZNurserySurvivorCensus` and was fixed before
            // the run: >= 90% refutes, <= 60% supports.
            //
            // WHICH SURVIVORS, and why this is not the sweep's own loop. The
            // sweep lives in `gc/src/zgc/sweep.rs` and hands back no survivor
            // list -- `ZSweepShard` carries the DEAD bases, not the live ones.
            // What it does do is `registry.remove(base)` for every object it
            // reclaims, so at this point "still in `self.registry`" IS "was a
            // survivor of this cycle", answered by one bit test. `registered`
            // is the PRE-sweep snapshot, so walking it from `sweep_floor` and
            // filtering on the live registry enumerates exactly the nursery
            // survivors. The bitmap arm cannot reach here: it requires
            // `sweep_floor == 0`, and a young cycle's floor is where the
            // nursery starts.
            //
            // `floor` is the value `sweep_floor` TOOK for this cycle, not
            // `gen_young_floor` re-read -- otherwise the geometry and the sweep
            // would be describing different cycles. `end` is the arena's low
            // cursor, read after the sweep's retractions so it is the cursor a
            // mutator will actually allocate against.
            // `CRATONVM_ZGC_NURSERY_CENSUS` gates this O(nursery) diagnostic
            // walk: default OFF since zr z2/g2 (`=1` runs it), read per heap.
            if self.counters.gen_census_on.load(Ordering::Relaxed) {
                let census_t = gc_started.map(|_| std::time::Instant::now());
                let (arena_base, low_end) = {
                    let arena = self.arena.lock();
                    let b = arena.base_ptr() as usize;
                    (b, b + arena.used_low_for_compaction())
                };
                let geo = generation::ZNurseryGeometry {
                    base: arena_base,
                    page_bytes: Self::Z_LOGICAL_PAGE_BYTES,
                    floor: sweep_floor,
                    end: low_end,
                };
                let mut obs = self.gen_nursery_survivors.begin_cycle(geo);
                // Zero pages means either an empty nursery or a MALFORMED frame
                // (`ZNurseryGeometry::is_well_formed` -- a floor below the grid
                // base, i.e. an unset young floor). `finish_cycle` refuses to
                // count either as a cycle and tells the two apart for the
                // reader through `geometry_rejected`; counting an empty nursery
                // would drag the percentage toward "Supported", and clamping a
                // malformed frame -- which it did before wave 3 -- would
                // redefine the nursery as the whole arena and read as
                // "Refuted". An instrument must not drift in either direction
                // on its own. Either way there is nothing to walk, so skip it;
                // `finish_cycle` is still called, because `cycles_observed` is
                // the engagement counter and must fire whether or not this
                // cycle was evidence.
                if obs.page_count() != 0 {
                    registered.for_each_base_from(sweep_floor, |b| {
                        if !self.registry.contains(b) {
                            return;
                        }
                        // `note_survivor` ignores anything outside
                        // `[floor, end)` -- the large-object region above the
                        // low cursor is not nursery and must not be counted.
                        //
                        // IT DOES NOT IGNORE PROMOTED OBJECTS, AND THAT IS THE
                        // CENSUS'S BIAS. Promotion on this backend is a header
                        // label, not a copy into another space, so an object
                        // the collector has already called old still sits
                        // inside `[floor, end)` and is counted here as a
                        // nursery survivor. The probe behind the published
                        // `Refuted` recorded 2.05M promotions. Combined with a
                        // floor that moves only at a major, the per-page bit is
                        // monotone within a major epoch and the aggregate
                        // climbs for a reason that has nothing to do with the
                        // workload -- see `ZgcRealHeap::nursery_survivor_report`
                        // and `handoff-s-nursery-census-per-cycle.md`.
                        //
                        // THE FIX IS TWO LINES AND HALF OF IT IS NOT IN THIS
                        // FILE. `generation::ZNurseryCycleObserver::note_survivor`
                        // has to take the age; then this becomes
                        //
                        //     let h = self.header_ref(b as *mut u8);
                        //     let bytes = Self::alloc_size(h).unwrap_or(0);
                        //     obs.note_survivor(b, bytes, h.gc_age());
                        //
                        // with the observer counting only ages below
                        // `self.promotion_age()`. The header is already in hand
                        // here, so the call-site half costs nothing. Left
                        // undone deliberately rather than approximated by
                        // filtering here: silently changing what the census
                        // counts, mid-round, after its numbers were published
                        // and propagated, is the same offence as re-scoring the
                        // thresholds -- which lane S declined to do for exactly
                        // this reason. The per-cycle series now says HOW MUCH
                        // this costs; the correction lands with a re-run.
                        let bytes = self
                            .survivor_size(b, self.header_ref(b as *mut u8))
                            .unwrap_or(0);
                        obs.note_survivor(b, bytes);
                    });
                }
                self.gen_nursery_survivors.finish_cycle(obs);
                census_us += census_t.map_or(0, |t| t.elapsed().as_micros() as u64);
            }
            // ESCALATE ON EVIDENCE. A young cycle that freed nothing did not
            // answer whatever asked for it, and repeating it would be a
            // collection per allocation against an old generation it will not
            // examine. `bytes_freed` and not a ratio: "reclaimed literally
            // nothing" needs no tuning parameter and cannot be wrong about the
            // direction.
            //
            // zt t7/tg (GN-12): and, opt-in, a young cycle that freed under an
            // eighth of the nursery budget (`gen_escalate_after_minor`); the
            // weak-minor census beside it is what rows R1/R2 read to decide
            // that switch.
            //
            // zt t14/gn5 (`CRATONVM_ZGC_GEN_EMPTY_MINOR_NEEDS_PROMOTION`): not
            // when nothing was promoted since the last major -- see
            // `gen_empty_minor_escalation_skipped`.
            let escalate = self.note_gen_minor_outcome(bytes_freed)
                && !self.gen_empty_minor_escalation_skipped(
                    bytes_freed,
                    bytes_copied,
                    young_aged_sweep,
                );
            self.gen_force_major_next.store(escalate, Ordering::Relaxed);
        } else if gen_on {
            // zt t7/tg (GN-12): a whole-heap cycle ends a run of weak minors.
            self.counters.gen_weak_minor_run.store(0, Ordering::Relaxed);
            // A whole-heap cycle has just run, so whatever the latch was asking
            // for has happened.
            self.gen_force_major_next.store(false, Ordering::Relaxed);
            // Reset on EVERY non-young collection, including one that was
            // whole-heap because nothing had been promoted yet: the budget means
            // "young cycles since the whole heap was last examined", and any
            // full trace is such a time.
            self.minors_since_major.store(0, Ordering::Relaxed);
            // zr z2/g2: the old-growth rule's epoch restarts here, carrying
            // THIS cycle's promotions -- they are the first objects the next
            // major will examine for the first time.
            self.counters
                .gen_promoted_since_major
                .store(gen_promoted_bytes, Ordering::Relaxed);
            // zt t14/gn5: ...and remember the seed, which is live data, not
            // old garbage a later major could free.
            self.counters
                .gen_promoted_at_major
                .store(gen_promoted_bytes, Ordering::Relaxed);
            // AND THIS IS WHERE THE NURSERY BOUNDARY MOVES. A whole-heap cycle
            // has just swept everything, so every survivor below the cursor is
            // genuinely old and the bytes it just counted are the old live set.
            // Read with the arena lock, after the sweep and both cursor
            // retractions, so it is the cursor a mutator will actually allocate
            // against.
            let cursor = {
                let arena = self.arena.lock();
                arena.base_ptr() as usize + arena.used_low_for_compaction()
            };
            self.gen_young_floor.store(cursor, Ordering::Relaxed);
            self.gen_old_live_bytes.store(
                self.gen_old_live_after_major(
                    bytes_copied,
                    gen_young_high_bytes,
                    gen_old_high_bytes,
                ),
                Ordering::Relaxed,
            );
            // zr z1/gn: everything is below the new floor now.
            self.counters
                .gen_nursery_old_bytes
                .store(0, Ordering::Relaxed);
            // ...and this cycle's sweep has re-set every old survivor's bit.
            self.finish_old_bits_rebuild();
            // ...AND THE YOUNG PAGE SET STARTS AGAIN. This cycle examined every
            // page, so nothing below the new floor is owed a visit; the set
            // refills from the allocations that follow. Clearing it HERE rather
            // than at the next young cycle is what keeps it a record of "since
            // the last major" instead of an ever-growing union.
            self.clear_young_pages();
        }

        if unsizable != 0 {
            self.counters
                .local
                .unsizable_objects
                .fetch_add(unsizable, Ordering::Relaxed);
            if !self
                .counters
                .local
                .unsizable_warned
                .swap(true, Ordering::Relaxed)
            {
                tracing::warn!(
                    target: "cratonvm::gc::guard",
                    count = unsizable,
                    "zgc sweep: registered object(s) whose header could not be \
                     sized (compact layout unresolvable, or an implausible \
                     legacy num_slots) — retained rather than zeroed and \
                     free-listed. See ZgcRealHeap::alloc_size.",
                );
            }
        }
        // The registry prune used to be here, over the collected `dead` slice.
        // It now happens inside the sweep loop, where the base is already in
        // hand -- see the note at that site, which carries the in-place
        // argument this comment used to.
        // Memory hygiene for the forwarding table, on the same pass that
        // decided which addresses are live. See `prune_relocations`.
        self.prune_relocations();
        // zr z1/al residual (`CRATONVM_ZGC_TAIL_RETURN_CYCLE_CHECK`): the tails
        // this collection withheld for owners that could not retire are still
        // their owners' reservations, so they stay charged. Without this the
        // bytes a resumed owner bumps into its tail were charged nowhere.
        // `live_bytes` alone with the switch off.
        let republished = live_bytes.saturating_add(self.recharge_published_tails());
        self.allocated.store(republished, Ordering::Relaxed);
        // THE NURSERY'S ZERO. Reset on every collection, young or full, and from
        // the same figure `allocated` was just given -- so "bytes allocated since
        // the last collection" is exact rather than an estimate, and the trigger
        // above cannot storm.
        self.cycle_alloc_watermark
            .store(republished, Ordering::Relaxed);
        // Re-arm the trigger: require at least a quarter of the remaining
        // headroom (min 64 KiB) of NEW allocation before the next
        // threshold-triggered collection, so a live set parked above the
        // static threshold cannot re-fire a full STW cycle on every
        // allocation (see `gc_rearm`). Allocation-failure GCs are driven by
        // the fallible alloc paths and ignore this gate.
        //
        // zr z6/tp: while the live set itself is above the threshold, the
        // floor is `max(1 MiB, pct% of free)` instead (the threshold clause is
        // true on every poll there, so this floor alone paces the collector);
        // see `director::rearm_floor`. `gc_rearm_legacy` keeps the old floor
        // for the `pace_floor_deferred` gauge.
        let cap = self.heap_capacity();
        let legacy_floor = director::rearm_floor_legacy(cap, live_bytes);
        let near_full_pct = self.counters.near_full_progress_pct.load(Ordering::Relaxed);
        let mut floor = director::rearm_floor(cap, live_bytes, self.gc_threshold, near_full_pct);
        // zt t6/tc (zt1tp item 1, under
        // `CRATONVM_ZGC_SOFT_MAX_PACES`, default ON): below a `-XX:SoftMaxHeapSize` the
        // floor is a share of the SOFT headroom (`(soft - live) / 4`, 64 KiB
        // minimum), not of the capacity's, so the heap does not fill to
        // `live + (Xmx - live) / 4` past the soft threshold. Never raises the
        // floor; a live set at or above the soft maximum keeps the capacity's.
        if self.counters.soft_max_paces.load(Ordering::Relaxed) {
            let soft = self.soft_max_capacity();
            if soft < cap && live_bytes < soft {
                floor = floor.min(director::rearm_floor_legacy(soft, live_bytes));
            }
        }
        self.counters
            .gc_rearm_legacy
            .store(republished.saturating_add(legacy_floor), Ordering::Relaxed);
        self.gc_rearm
            .store(republished.saturating_add(floor), Ordering::Relaxed);
        // Lower the allocatable-space latch: the sweep above has just
        // free-listed every dead object and coalesced the result into maximal
        // spans, so whatever headroom this heap can have, it has now.
        //
        // Cleared unconditionally rather than recomputed. If headroom is STILL
        // below the margin the very next allocation re-arms it — and that path
        // is correct where a recompute would be fragile, because it re-measures
        // the arena the caller actually failed against instead of a snapshot
        // taken here. Re-arming cannot storm: `gc_rearm` was just set above
        // `allocated`, so the trigger waits for genuinely new allocation.
        self.headroom_low.store(false, Ordering::Relaxed);
        // ...and record whether lowering it was worth anything. This is the
        // ONLY thing that stops `CRATONVM_ZGC_HEADROOM_BYPASSES_REARM` looping:
        // the flag lets `headroom_low` re-raise a collection without waiting
        // for `gc_rearm`, and the line above guarantees it CAN be re-raised by
        // the very next allocation. A cycle that freed nothing is the evidence
        // that the next one would too. See
        // [`Self::last_cycle_reclaimed_nothing`].
        //
        // `bytes_freed` and not `bytes_copied`: the question is what came back,
        // not what survived. It already includes the complement sweep's arena
        // accounting and both cursor retractions, which is why this is here and
        // not beside the sweep.
        self.last_cycle_reclaimed_nothing
            .store(bytes_freed == 0, Ordering::Relaxed);
        // Disarm the native-allocation-pressure latch here, at the same point
        // `gc_rearm` is recomputed: the collection the latch asked for has now
        // happened, and the re-arm floor just published above is the only thing
        // that decides whether the next allocation may raise it again. Mirrors
        // G1's clear at the end of its cycle (`g1.rs:8392`).
        self.native_alloc_pressure.store(false, Ordering::Relaxed);
        self.external_gpu.on_gc_cycle_end();
        // Same point, same reasoning, for the hard-refusal latch. Note this is
        // NOT done in `clear_native_alloc_pressure`: that runs on the boundary's
        // "gates said no" path too, and lowering the hard bit there would throw
        // away the one signal this whole mechanism exists to carry.
        self.hard_alloc_failure.store(false, Ordering::Relaxed);
        // Re-arm the stress trigger against the post-sweep live figure.
        self.counters
            .gc_stress_mark
            .store(self.allocated.load(Ordering::Relaxed), Ordering::Relaxed);
        // ...and re-arm the trigger ATTRIBUTION, at the same point and for the
        // same reason: every reason that asked for this collection has now been
        // answered, so the next time one of them says yes it is asking for the
        // NEXT cycle and gets charged again. Placed here rather than at the top
        // of the function so a reason raised during the pause -- `headroom_low`
        // re-armed by an allocation the finalizer path makes, say -- belongs to
        // the cycle it will actually drive. See
        // `ZgcCounters::trigger_seen`.
        //
        // Snapshotted first: `zgc_trigger_shadow_enabled`'s line, emitted after
        // the slide, reports WHICH clause asked for this collection, and by
        // then the mask is back to zero. One relaxed load per collection.
        let trigger_mask_for_cycle = self.counters.trigger_seen.load(Ordering::Relaxed);
        self.counters.trigger_seen.store(0, Ordering::Relaxed);
        self.counters
            .last_cycle_trigger_mask
            .store(trigger_mask_for_cycle, Ordering::Relaxed);
        // zr zs s1/tp: a warm-up cycle advances the warm-up step count
        // (`warmup_cycles_done`); no other collection does.
        if trigger_mask_for_cycle & TRIGGER_BIT_WARMUP != 0 {
            self.counters
                .warmup_cycles_done
                .fetch_add(1, Ordering::Relaxed);
        }
        self.counters.pacing.record(
            pace_near_full,
            pace_kind_for_mask(trigger_mask_for_cycle),
            pace_window,
        );
        // zr z2/rv: the sweep has unregistered the dead; drop their huge-extent
        // slots so the interior probes' fallback stays a handful of loads.
        self.prune_huge_extents();
        // zs s10/sp: a collection that changed the picture re-admits the
        // free-list blocks the final-refusal rule set aside.
        self.recheck_sub_request_spans();
        let cycle = self.gc_count.fetch_add(1, Ordering::Relaxed) + 1;

        // Phase 2.2: one post-sweep fragmentation reading per collection.
        //
        // Here rather than inside the sweep's own arena scope above, and the
        // second `lock()` is deliberate: this reads the arena AFTER the sweep,
        // the high-end coalesce, and both cursor retractions, which is the
        // state a workload actually allocates against and therefore the only
        // one worth ratcheting on. The world is still stopped, so the two
        // scopes see identical bytes and the extra acquire is uncontended.
        {
            let arena = self.arena.lock();
            self.sample_frag_gauge(&arena, cycle);
        }

        // Per-collection `--verbose:gc` line. One `eprintln!` and no
        // `tracing` twin on purpose: the defect this closes is that a run with
        // JUST `--verbose:gc` (no `RUST_LOG`) produced zero output for the
        // whole run, and stderr is the only sink that is on in that case.
        //
        // Format: the `[GC]` prefix and the `zgc-real:` tag match this
        // backend's existing shutdown line in `vm_heap.rs::print_gc_summary`
        // (`[GC] zgc-real: collections=… occupancy=…/… bytes`), and the
        // key names are G1's `[GC-STAT]` keys verbatim (`pause_us=`,
        // `objects_copied=`, `bytes_copied=`, `bytes_freed=` — `g1.rs:6652`)
        // so the two backends' per-collection lines extract identically when
        // compared by hand. `objects_copied`/`bytes_copied` are the sweep's
        // SURVIVOR totals (G1's key names), not relocations: this line is
        // printed before the slide runs. What the slide copied is
        // `moved=` / `moved_bytes=` on `[GC] zgc-relocate:` (zr z9/sl; a host
        // reading of `bytes_copied=213510944` as "the slide moves the whole
        // live set" was this name).
        //
        // `gc::zgc::metrics::ZgcMetrics::format_cycle_line` renders the richer
        // OpenJDK-shaped line (before(%)->after(%), reclaimed, STW/concurrent
        // split, allocation stalls) for exactly this callsite, and as of
        // 2026-09-20 the module is **fed**: see `record_metrics_for_cycle`
        // below and the `metrics` field on `ZgcCounters`. It is not printed
        // HERE because it is a different format and every harness, grep and
        // archived comparison in this tree keys on the line below as it
        // stands; `CRATONVM_ZGC_METRICS=1` prints it alongside. Replacing this
        // line wholesale is a wave with a compatibility window -- what it is
        // NOT any more is blocked on the instrument being unreachable.
        // EVERY MARK BIT IS CLEAR FROM HERE, and on the bitmap arm that is a
        // statement about one linear store rather than about which objects the
        // sweep happened to visit.
        //
        // The header arm can only claim it for a cycle whose sweep was
        // UNBOUNDED: a young cycle skips everything below `sweep_floor`, and
        // those objects were pre-marked above, so their bits are still SET.
        // Claiming otherwise there would let the next mark start skip its
        // clearing walk and hand the sweep a mark set carrying a previous
        // cycle's bits, retaining whatever the old generation reached.
        //
        // The bitmap arm has no such gap, because `clear_all` does not care
        // what the sweep visited -- which is also why a young cycle on this arm
        // costs nothing extra to leave clean. Placed AFTER the sweep block (the
        // arena guard is released) and BEFORE compaction, which reads no mark
        // bit by design -- see `relocate_stw`'s note on why its live set is a
        // parameter.
        // RESIZE THE CONCURRENT WINDOW from what this run has now measured.
        // Once per collection, at the safepoint, so the allocation-path poll
        // stays two relaxed loads. A no-op unless `CRATONVM_ZGC_CONC_START=auto`.
        self.refresh_adaptive_conc_start(Self::monotonic_nanos());

        // THE SWEPT STRETCHES STOP COUNTING AS NOTED (nio21 wave 9 L3): the
        // sweep above is the last registry writer of this pause before the
        // slide, which only re-notes. See `forget_empty_registry_blocks` for
        // the conditions, including the frozen-peer decline.
        let _forgotten_blocks = self.forget_empty_registry_blocks();

        let bits_clear = if self.mark_clear_all() {
            true
        } else {
            sweep_floor == 0
        };
        // See `conc_bits_known_clear`; the debug build walks anyway and
        // `debug_assert`s this rather than trusting it.
        self.conc_bits_known_clear
            .store(bits_clear, Ordering::Release);

        let sweep_us = clock.lap();
        self.counters
            .last_sweep_us
            .store(sweep_us.min(u64::MAX as u128) as u64, Ordering::Relaxed);
        if let (Some(t0), Some(walked), Some(installed)) =
            (sweep_split_t0, sweep_split_walked, sweep_split_installed)
        {
            // zt t26/pc1: the split of the lap above (it also holds the
            // `refs` tail and the census set-up before `t0`: `pre_us`).
            eprintln!(
                "[zgc-sweep-phases] cycle={cycle} sweep_us={sweep_us} walk_us={} install_us={} \
                 post_us={} pre_us={} registered={registered_count} dead={dead_count} \
                 live_hints={} spans={dead_runs}",
                walked.duration_since(t0).as_micros(),
                installed.duration_since(walked).as_micros(),
                installed.elapsed().as_micros(),
                sweep_us.saturating_sub(t0.elapsed().as_micros()),
                sweep_hints.addrs.len(),
            );
            if let (Some(i0), Some(i1), Some(i2)) = (dbg_i0, dbg_i1, dbg_i2) {
                eprintln!(
                    "[zgc-install-laps] pre_us={} young_install_us={} rest_us={}",
                    i1.duration_since(i0).as_micros(),
                    i2.duration_since(i1).as_micros(),
                    installed.duration_since(i2).as_micros()
                );
            }
        }
        // RE-DERIVE THE ALLOCATION BUDGET from what this pause actually cost.
        // Before the logging block, and outside it, so the loop runs on a
        // quiet run too -- see `gc_started` above.
        //
        // FULL CYCLES ONLY. A young cycle's sweep skips everything below
        // `sweep_floor`, so its `bytes_freed` describes the nursery while its
        // `live_bytes` includes the old generation it never walked: the span
        // reconstructed from the pair is far larger than the span the pause
        // actually covered, and folding that in would teach the loop that a
        // pause is cheaper per byte than it is. A young pause and a full pause
        // are different cost functions; this models the one whose length is
        // the problem.
        //
        // AND THE SLIDE IS NOT IN THIS READING. The compaction block is ~150
        // lines below, inside the same `StopTheWorldToken`, and measured at
        // about twice this figure on `ZgcTlabStress 8 200`. Reading the clock
        // here therefore hands the loop roughly a third of the pause it is
        // modelling -- and because the `span` it divides by
        // (`live_bytes + bytes_freed`) includes the objects the slide then
        // moved, the ratio is not a correction factor for anything.
        // `CRATONVM_ZGC_PAUSE_TARGET_INCLUDES_RELOCATE=1` moves the reading to
        // after the slide (search for the second call to this function); the
        // two are mutually exclusive so a cycle never feeds the loop twice.
        // See `zgc_pause_target_includes_relocate` for why the correct
        // accounting is opt-in for a wave rather than simply switched on.
        if !zgc_pause_target_includes_relocate() {
            if let Some(started) = gc_started {
                if !young_cycle {
                    self.refresh_pause_target_budget(
                        started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
                        live_bytes,
                        bytes_freed,
                    );
                }
            }
        }
        // THE LOGGING BLOCK, and it needs its OWN gate rather than `gc_started`.
        //
        // `gc_started` is `Some` when logging is on **or a pause target is
        // set**, and that disjunction is deliberate: the budget loop above has
        // to run on a quiet run, or `--verbose:gc` would change the collector's
        // behaviour and not just its output. But the pause target defaults to
        // 200 ms (`zgc_pause_target_ms`), so `gc_started` is `Some` on EVERY
        // run, and these two `eprintln!`s were firing on every cycle of every
        // release run that never asked for them.
        //
        // Measured 2026-09-04: `vm/tests/class_loader_unload_regression.rs`
        // drives a probe that calls `System.gc()` ~180 times. Its stdout is
        // 124 bytes; its stderr was **101,808 bytes**, essentially all of it
        // these two lines. A Linux pipe holds 64 KiB, so a parent that captures
        // stderr and does not drain it until the child exits deadlocks — which
        // is exactly what that test did, and it read as "class-loader unloading
        // probe timed out". The probe itself answers `ok=true` in under 20 s
        // when its output goes anywhere with room for it.
        //
        // The test is fixed independently (it now drains while it waits — a
        // test must not depend on the child staying quiet). This is the other
        // half: a quiet run is quiet again, which is what the comment on the
        // budget loop above already assumed.
        if let Some(started) = gc_started.filter(|_| self.gc_log_enabled.load(Ordering::Relaxed)) {
            let pause_us = started.elapsed().as_micros();
            // `mark=` is the ONE field that says whether this collection's
            // mark phase was paid for inside the pause. Without it a
            // `pause_us` reading cannot be attributed: a short pause on a
            // concurrent cycle and a short pause because the live set happened
            // to be small are the same number, and this feature's whole claim
            // is about the first.
            let mark = if marked_concurrently {
                "concurrent"
            } else if parallel_workers >= 1 && parallel_ok {
                "stw-parallel"
            } else {
                "stw-serial"
            };
            // `gen=` is to Phase G what `mark=` is to Phase C: without it a
            // young cycle and a full one are the same line, and the whole claim
            // is about the difference. `young/N` names the objects the cycle
            // retained WITHOUT tracing -- the work it did not do -- and
            // `major/+N` the objects it promoted.
            let gen = if young_cycle {
                // `swept/registered` is the nursery: how much of the registry the
                // sweep actually walked. Equal numbers mean the floor is inert.
                format!("young/{gen_old_retained} swept={swept}/{registered_count}")
            } else if gen_on {
                format!("major/+{gen_promoted}")
            } else {
                "off".to_string()
            };
            eprintln!(
                "[GC] zgc-real: cycle={cycle} pause_us={pause_us} mark={mark} gen={gen} \
                 objects_copied={objects_copied} bytes_copied={bytes_copied} \
                 bytes_freed={bytes_freed} occupancy={bytes_copied}/{cap} bytes",
            );
            // THE ANATOMY. Without it, "the pause is 230 ms" cannot be acted
            // on: a pause that is mostly `mark_us` wants a longer concurrent
            // window, and a pause that is mostly `sweep_us` wants a concurrent
            // SWEEP and does not care about marking at all. `registered` is
            // here because every phase except `markend` scales with it.
            eprintln!(
                "[GC] zgc-pause: cycle={cycle} total_us={pause_us} \
                 markend_us={markend_us} tlab_us={tlab_us} snapshot_us={snapshot_us} \
                 mark_us={mark_us} resurrect_us={resurrect_us} refs_us={refs_us} \
                 sweep_us={sweep_us} registered={} dead={} \
                 alloc_trigger={}/{} \
                 pause_target={}ms/{}/unreachable={}",
                registered_count,
                dead_count,
                // ENGAGEMENT for F1: fires/budget. `0/<n>` means every cycle
                // still came from the 75%-of-`-Xmx` clause and the allocation
                // trigger measured nothing; `<n>/0` means the switch is off.
                self.counters.alloc_trigger_fires.load(Ordering::Relaxed),
                self.alloc_trigger_bytes.load(Ordering::Relaxed),
                // And the loop that SIZED that budget: the target, the span
                // in BYTES the controller currently believes a target-length
                // pause can walk, and the cycles where the target was not
                // achievable at all. A budget equal to capacity with
                // `unreachable` climbing is the loop saying it cannot
                // deliver, which looks identical to a budget equal to
                // capacity because pauses are comfortably short -- opposite
                // situations, same number, and this field is what separates
                // them.
                self.pause_target_ms.load(Ordering::Relaxed),
                self.pause_affordable_span.load(Ordering::Relaxed),
                self.counters
                    .pause_target_unreachable
                    .load(Ordering::Relaxed),
            );
        }

        // ---- COMPACTION (DEFAULT-ON since 2026-08-13) --------------------
        //
        // THE TWO PARAGRAPHS THAT USED TO OPEN THIS BLOCK SAID THE OPPOSITE,
        // and it was load-bearing. They called compaction opt-in behind
        // `CRATONVM_ZGC_RELOCATE=1` and said the non-moving path "is still what
        // every default run does" -- three months after the default moved. A
        // reader triaging a crash whose stack runs through `relocate_stw` would
        // conclude the slide could not have been running. `x64::
        // zgc_codegen_honours_read_barrier()` also answers `true`
        // unconditionally now, so `zgc_relocation_permitted`'s old blanket
        // refusal under the JIT no longer fires either: a default JIT run
        // compacts.
        //
        // By default the sweep is followed by a
        // stop-the-world slide that compacts the small-object end and returns
        // a NON-EMPTY pointer map. Three things make that safe to have here:
        //
        //  * the sub-flag, which is the only thing keeping it off a user's
        //    machine now that the `zgc` Cargo feature is default-ON;
        //  * `zgc_relocation_permitted` in `vm_init`, which refuses when the
        //    JIT is enabled AND `x64::zgc_codegen_honours_read_barrier()` is
        //    false -- i.e. when compiled code would load a reference field
        //    without routing through a barriering accessor. Since stage (a) of
        //    `zgc-jit-load-barrier.md` landed (2026-08-13) that predicate
        //    answers `true` unconditionally, so a default JIT run is permitted
        //    and the real per-cycle guard is the coverage proof below
        //    (`relocate_stw_admitted`), not this one;
        //  * the caller's `StopTheWorldToken`, already proven above.
        //
        // (That second bullet used to read "refuses whenever the JIT is
        // enabled", eight lines after this same comment records that a default
        // JIT run compacts. Both could not be true and the code supports the
        // first; see
        // `docs/feature-designs/zgc-architecture.md`.
        // It matters because that list is the safety argument a reviewer reads
        // before touching the slide, and one of its three legs was a refusal
        // that does not happen.)
        //
        // The `roots` slice is rewritten in place here, because a root that
        // still names a pre-slide address is a dangling pointer the moment
        // this function returns -- and unlike a field slot, nothing downstream
        // would rewrite it.
        self.finalize_discovered_weak_phantom();
        let mut pointer_map: cratonvm_types::PointerMap = cratonvm_types::PointerMap::default();
        // The `zgc-pause` line above is printed BEFORE this block, so without
        // this reading the slide's stop-the-world time is in no log line at
        // all. Measured (round 9 wave 5, `zgc5`) at about twice the logged
        // pause on `ZgcTlabStress 8 200`; see
        // `perf-zgc-relocate-stw-is-a-whole-heap-pass-outside-the-logged-pause`.
        // Only taken when the pause clock is, so a quiet run reads no clock.
        let relocate_started = gc_started.map(|_| std::time::Instant::now());
        // ASKED ONCE. `relocation_requested` reads `relocation_enabled` AND
        // `vm_heap::gpu_relocation_forbidden()`, and the second of those is a
        // live process-wide veto that a device can raise between two calls. The
        // metrics recorder below has to report whether the slide ran, not
        // whether it would be permitted a moment later, so both readers take
        // the same answer.
        // `CRATONVM_ZGC_GEN_YOUNG_RELOCATE=0` (zr z1/gn): no slide on a YOUNG
        // cycle. The slide's passes (live-set snapshot, page census, reference
        // rewrite) walk the WHOLE live set, so on a young cycle they are the
        // one O(heap) cost left. Default OFF since zr z2/g2 (`=1` restores the
        // young slide; the z1 rows measured -14 % CPU without it), read per
        // heap.
        let young_slide_ok = self.counters.gen_young_relocate_on.load(Ordering::Relaxed);
        // t1 wave 6 lane Y4 (`CRATONVM_ZGC_GEN_YOUNG_EVACUATE`, opt-in): a
        // young cycle may run the NURSERY-SCOPED slide instead (see
        // `young_evac_scope`); a refused scope runs no young slide unless
        // `young_slide_ok` also asks for the whole-live-set one.
        let young_evac_on = young_cycle && self.counters.young_evac.on.load(Ordering::Relaxed);
        // zt t32 (6a): no young slide while an R4 cycle is kept for a major.
        let relocated_this_cycle = self.relocation_requested()
            && !(young_cycle && !young_slide_ok && !young_evac_on)
            && !(young_cycle && self.counters.rim_keep_until_major.load(Ordering::Relaxed));
        let mut moved_this_cycle = 0usize;
        if relocated_this_cycle {
            // The POST-sweep registry: the sweep has already removed every
            // dead base from it, so what remains is exactly the live set --
            // and unlike the mark bits, it is still there. `registered` (the
            // pre-sweep snapshot) would include the objects just reclaimed.
            //
            // ASKED IN TWO STEPS (round 9 wave 6, `zgc6`): the refusals first,
            // so a refused cycle no longer materialises a live set it will not
            // read; then the snapshot, BOUNDED like the mark's
            // (`live_bitmap_bounds`, the same `CRATONVM_ZGC_BITMAP_BOUNDS`
            // switch): the never-bumped middle between the two cursors holds no
            // base, and the whole-capacity copy was 64 MB of bitmap per cycle
            // at a 4 GB heap before a single base was read. Same bases, same
            // ascending order.
            let refusal = self.relocate_refusal();
            // t1 wave 6 lane Y4: the scope, asked only past the cycle-level
            // refusals; a scoped cycle never takes the concurrent plan.
            let scope_clock = zgc_dbg_relocate_phases_enabled().then(std::time::Instant::now);
            let young_scope = (refusal.is_none() && young_evac_on)
                .then(|| self.young_evac_scope(old_bits_young && young_aged_sweep));
            if let (Some(t), Some(Ok(s))) = (scope_clock, young_scope.as_ref()) {
                eprintln!(
                    "[zgc-young-evac-scope] build_us={} live={} extra={} blocked={}",
                    t.elapsed().as_micros(),
                    s.live.len(),
                    s.extra.len(),
                    s.blocked_pages.len(),
                );
            }
            if let Some(Err(code)) = young_scope.as_ref() {
                self.counters.young_evac.note_refusal(*code);
            }
            // zt t32 (R6, `CRATONVM_ZGC_CONCURRENT_RELOCATE`): the plan in place
            // of the slide -- destinations allocated, the eager set copied, the
            // rest copied after the release. A refusal (counted) falls through
            // to the slide below.
            let planned_cycle = if refusal.is_none()
                && young_scope.is_none()
                && self.concurrent_relocate_admitted(young_cycle)
            {
                // zt t34: the live list only when the plan reads it (its
                // length is the census's live count).
                let n = std::cell::Cell::new(0usize);
                let live_now = || -> Vec<usize> {
                    let v = if zgc_bitmap_bounds_enabled() {
                        self.registry.bases_within(&self.live_bitmap_bounds())
                    } else {
                        self.registry.snapshot().bases()
                    };
                    n.set(v.len());
                    v
                };
                self.concurrent_relocate_start(&live_now)
                    .map(|(planned, map)| (planned, n.get(), map))
            } else {
                None
            };
            let (moved, reclaimed, map, live_len, refused) = match (refusal, planned_cycle) {
                (Some(reclaimed), _) => {
                    (0, reclaimed, cratonvm_types::PointerMap::default(), 0, true)
                }
                (None, Some((planned, live_len, map))) => (planned, 0, map, live_len, false),
                (None, None) if matches!(young_scope, Some(Ok(_))) => {
                    let Some(Ok(scope)) = young_scope.as_ref() else {
                        unreachable!("guarded by the arm")
                    };
                    self.counters.young_evac.note_scope(scope);
                    let (moved, reclaimed, map) =
                        self.relocate_stw_admitted_scoped(&scope.live, &sweep_hints, Some(scope));
                    self.counters.young_evac.note_moved(moved);
                    (moved, reclaimed, map, scope.live.len(), false)
                }
                (None, None) if young_scope.is_some() && !young_slide_ok => {
                    // Refused (counted above), and no whole-live-set young
                    // slide was asked for: this young cycle does not move.
                    (0, 0, cratonvm_types::PointerMap::default(), 0, true)
                }
                (None, None) => {
                    // zt t26/pc1 (`CRATONVM_ZGC_RELOCATE_LIVE_DIRECT`, default
                    // ON): the same list walked off the live bitmap and sized
                    // once, instead of a snapshot copy and a doubling `Vec`.
                    // `CRATONVM_DBG_ZGC_RELOCATE_PHASES` prices it (the slide's
                    // own phase line starts after it).
                    let list_t0 = zgc_dbg_relocate_phases_enabled().then(std::time::Instant::now);
                    let direct = true;
                    let live_now: Vec<usize> = if zgc_bitmap_bounds_enabled() {
                        if direct {
                            self.registry
                                .bases_within_summarised(&self.live_bitmap_bounds())
                        } else if direct {
                            self.registry.bases_within(&self.live_bitmap_bounds())
                        } else {
                            self.registry
                                .snapshot_within(&self.live_bitmap_bounds())
                                .bases()
                        }
                    } else {
                        self.registry.snapshot().bases()
                    };
                    if let Some(t0) = list_t0 {
                        eprintln!(
                            "[zgc-relocate-live-list] live={} direct={} us={}",
                            live_now.len(),
                            direct as u8,
                            t0.elapsed().as_micros()
                        );
                    }
                    let (moved, reclaimed, map) =
                        self.relocate_stw_admitted_hinted(&live_now, &sweep_hints);
                    (moved, reclaimed, map, live_now.len(), false)
                }
            };
            if let Some(started) =
                relocate_started.filter(|_| self.gc_log_enabled.load(Ordering::Relaxed))
            {
                // `pause_with_relocate_us` is the WHOLE stop-the-world so far,
                // from the same clock `zgc-pause`'s `total_us` read before this
                // block: the number a reader of that line actually wants
                // (round 9 wave 6, `zgc6`).
                //
                // `refused` / `skipped_jit` (round 9 wave 8, `zgc8`): a cycle
                // `relocate_refusal` declined prints `live=0 moved=0` exactly
                // like a slide that found nothing to move, and an A/B that
                // sums `moved` then reads a refused run as "the parallel
                // passes moved nothing" (zgc7's `par-w7b.tsv`, 2 of 4 rounds:
                // every cycle refused on `xt-helper-window-conservative-scan`,
                // which is timing, not the arm). `skipped_jit` is the running
                // count `zgc-relocation-skip-reason` breaks down at exit.
                let reloc_pool_census = self
                    .counters
                    .reloc_pool
                    .get()
                    .map(cratonvm_gc_core::evac_pool::EvacPool::census)
                    .unwrap_or_default();
                let qdead = &self.counters.evac_quarantine_dead_check;
                // zt t1/rl: the CR-E4 reading (`zgc-zs3cr-relocation-review-
                // residuals` item 1, the evacuating arm's cursor ratchet):
                // after this cycle's relocation, the low cursor, the bump
                // headroom and the largest low free block, in KiB. A ratchet
                // reads as `cursor_kb` climbing and headroom falling across
                // evacuating cycles while the largest free block grows.
                let (cursor_kib, headroom_kib, largest_low_kib) = {
                    let a = self.arena.lock();
                    (
                        a.used_low_for_compaction() / 1024,
                        a.low_bump_headroom() / 1024,
                        a.largest_low_free_block() / 1024,
                    )
                };
                eprintln!(
                    "[GC] zgc-relocate: cycle={cycle} relocate_us={} live={live_len} moved={moved} \
                     reclaimed={reclaimed} cost_gated={} pause_with_relocate_us={} \
                     refused={refused} skipped_jit={} targeted_overrun={} moved_bytes={} \
                     pins_by_base={} targets_pin_dropped={} evac={}/{}/{}/{}/{} \
                     quarantine={}/{}/{}/{}/{} evac_forced_blocks={} gate_sel={}/{} par={}/{}/{} \
                     pool={}/{}/{}/{} qdead={}/{}/{}/{}/{} cursor_kb={}/{}/{}",
                    started.elapsed().as_micros(),
                    self.counters.relocation_cost_gated.load(Ordering::Relaxed),
                    gc_started.map_or(0, |s| s.elapsed().as_micros()),
                    self.counters.relocation_skipped_jit.load(Ordering::Relaxed),
                    // Live bytes selected ABOVE `max_evacuation_bytes` because
                    // the page was targeted, cumulative. Zero unless
                    // `ZRelocationPolicy::budget_exempts_targets` fired; a
                    // persistently large value says to bound the targeted
                    // window, not to re-impose the budget. See the counter.
                    self.counters
                        .relocation_targeted_overrun_bytes
                        .load(Ordering::Relaxed),
                    // zr z9/sl: what this slide COPIED, both ends. The
                    // `zgc-real` line's `bytes_copied=` is the survivor total.
                    self.counters.slide_moved_bytes[0].load(Ordering::Relaxed),
                    // zs s2/sl2: cumulative engagement of the pin-by-base
                    // screen and of the target exemption's pin check.
                    self.counters
                        .slide_pins_dropped_by_base
                        .load(Ordering::Relaxed),
                    self.counters
                        .slide_targets_pin_dropped
                        .load(Ordering::Relaxed),
                    self.counters.slide_evacuate[0].load(Ordering::Relaxed),
                    self.counters.slide_evacuate[1].load(Ordering::Relaxed),
                    self.counters.slide_evacuate[2].load(Ordering::Relaxed),
                    self.counters.slide_evacuate[3].load(Ordering::Relaxed),
                    self.counters.slide_evacuate[4].load(Ordering::Relaxed),
                    // zs s3/cr: cumulative, see each counter's doc.
                    self.counters.evac_quarantine_stats[0].load(Ordering::Relaxed),
                    self.counters.evac_quarantine_stats[1].load(Ordering::Relaxed),
                    self.counters.evac_quarantine_stats[2].load(Ordering::Relaxed),
                    self.counters.evac_quarantine_stats[3].load(Ordering::Relaxed),
                    self.counters.evac_quarantine_stats[4].load(Ordering::Relaxed),
                    self.counters.evac_quarantine_stats[5].load(Ordering::Relaxed),
                    self.counters.selective_gate_on_selection[0].load(Ordering::Relaxed),
                    self.counters.selective_gate_on_selection[1].load(Ordering::Relaxed),
                    self.counters.reloc_par_passes[0].load(Ordering::Relaxed),
                    self.counters.reloc_par_passes[1].load(Ordering::Relaxed),
                    self.counters.reloc_par_passes[2].load(Ordering::Relaxed),
                    // zt t1/rl: THIS heap's relocation pool, `dispatches /
                    // inline_runs / max_dispatched / width` (0/0/0/0 until
                    // `CRATONVM_ZGC_RELOCATE_POOL=1` builds it).
                    reloc_pool_census.dispatches,
                    reloc_pool_census.inline_runs,
                    reloc_pool_census.max_dispatched,
                    reloc_pool_census.width(),
                    // zt t1/rl: the quarantine dead-at-release census.
                    qdead[0].load(Ordering::Relaxed),
                    qdead[1].load(Ordering::Relaxed),
                    qdead[2].load(Ordering::Relaxed),
                    qdead[3].load(Ordering::Relaxed),
                    qdead[4].load(Ordering::Relaxed),
                    cursor_kib,
                    headroom_kib,
                    largest_low_kib,
                );
                // zt t27/pc2: the three collector-CPU skips, cumulative --
                // slides declined because nothing could move
                // (`CRATONVM_ZGC_SLIDE_NO_MOVE_DECLINE`), TLAB carves the
                // futile latch answered / latches set
                // (`CRATONVM_ZGC_TLAB_CARVE_FUTILE_LATCH`), and survivors the
                // sweep sized from the last sweep's cells / sweeps that had a
                // memo (`CRATONVM_ZGC_SWEEP_SIZE_MEMO`). The engagement
                // census of each switch; its `=0` arm prints 0 there.
                eprintln!(
                    "[GC] zgc-cpu-skips: cycle={cycle} slide_nomove_declined={} \
                     slide_nomove_missed={} carve_futile_skips={} \
                     carve_futile_latches={} size_memo_hits={} size_memo_sweeps={}",
                    self.counters.slide_no_move_declined.load(Ordering::Relaxed),
                    self.counters.slide_no_move_missed.load(Ordering::Relaxed),
                    self.counters.carve_futile_census[0].load(Ordering::Relaxed),
                    self.counters.carve_futile_census[1].load(Ordering::Relaxed),
                    self.counters.size_memo_census[0].load(Ordering::Relaxed),
                    self.counters.size_memo_census[1].load(Ordering::Relaxed),
                );
            }
            // t1 wave 6 lane Y4: the scoped young slide's engagement,
            // cumulative, one line per young cycle that asked for it.
            if young_evac_on && self.gc_log_enabled.load(Ordering::Relaxed) {
                let r = self.gen_young_evacuate_report();
                eprintln!(
                    "[GC] zgc-gen-young-evac: cycle={cycle} young_evac_scopes={} \
                     young_evac_refusals={} young_evac_last_refusal={} \
                     young_evac_live_scoped={} young_evac_extra_rewritten={} \
                     young_evac_blocked_pages={} young_evac_moved={} \
                     young_evac_cycles_moved={} young_evac_floor_holds={} \
                     young_bulk_dead_words={} young_evac_pins_skipped={} \
                     young_evac_local_installs={} young_evac_deferred_zero_bytes={}",
                    r.scopes,
                    r.refusals,
                    r.last_refusal,
                    r.live_scoped,
                    r.extra_rewritten,
                    r.blocked_pages,
                    r.moved,
                    r.cycles_moved,
                    // t1 wave 8 lane ZT3GN: the floor-hold and bulk-dead
                    // engagement counters, cumulative.
                    self.counters.young_evac.floor_holds.load(Ordering::Relaxed),
                    self.counters
                        .young_evac
                        .bulk_dead_words
                        .load(Ordering::Relaxed),
                    // t1 wave 10 lane ZT3GN: the scoped pin screen, the
                    // nursery-local install and the deferred vacated zeroing.
                    self.counters
                        .young_evac
                        .pins_skipped
                        .load(Ordering::Relaxed),
                    self.counters
                        .young_evac
                        .local_installs
                        .load(Ordering::Relaxed),
                    self.counters
                        .young_evac
                        .deferred_zero_bytes
                        .load(Ordering::Relaxed),
                );
            }
            moved_this_cycle = moved;
            if moved > 0 {
                // zt t2/x1 (finding X1-4): the final-refusal rule's re-check
                // runs after the sweep, BEFORE this slide, so the clause its
                // doc names first ("a slide relocated the live set down") could
                // only fire one collection late. Ask again once the slide has
                // moved something (a no-op unless the opt-in rule is armed).
                self.recheck_sub_request_spans();
                self.counters
                    .compaction_cycles
                    .fetch_add(1, Ordering::Relaxed);
                self.counters
                    .objects_relocated
                    .fetch_add(moved, Ordering::Relaxed);
                for r in roots.iter_mut() {
                    if let Some(to) = map.get(&(r.as_ptr() as usize)) {
                        // SAFETY: `to` is an object base this slide just wrote,
                        // inside the arena, 8-byte aligned by construction.
                        *r = unsafe { ObjectRef::from_raw(*to as *mut u8) };
                    }
                }
                // ---- PHASE G: every card names a location that moved ----
                //
                // See `recard_relocated_old_objects` for the four-case argument
                // that the DESTINATIONS alone are enough. Object ages ride in
                // the headers and are unaffected by the slide; only a card whose
                // object moved has been invalidated in the direction that loses
                // an edge.
                if gen_on {
                    // ---- G2: THE SLIDE **IS** THE PROMOTION ---------------
                    //
                    // The first version of this dropped the floor and forced the
                    // next cycle to be a major, on the reasoning that a slide
                    // rewrites the low region so an address no longer says which
                    // generation an object is in. That is backwards: a slide
                    // rewrites the low region into exactly the shape a nursery
                    // wants.
                    //
                    // `compact_low_to` packs survivors from the first selected
                    // page upward and then drops the cursor to the end of the
                    // compacted region, so afterwards **every live object is
                    // below the cursor** -- the ones on unselected dense pages
                    // never moved and are below it too. Setting the floor there
                    // makes every survivor old and leaves the nursery EMPTY, so
                    // the next young cycle sweeps only what has been allocated
                    // since. That is a young space reclaimed by moving its
                    // survivors out, which is what G2 is.
                    //
                    // # And it carries NO floating garbage
                    //
                    // The registry at this point holds live objects only -- the
                    // sweep pruned the dead a few statements ago -- so declaring
                    // everything below the cursor old retains nothing that is
                    // not reachable. Compare the ordinary (non-relocating) floor
                    // advance, which happens at the end of a whole-heap cycle and
                    // has the same property for the same reason.
                    //
                    // # Why this needs no JIT load barrier
                    //
                    // Because the move is at a SAFEPOINT and the collector
                    // rewrites every slot itself. `zgc_relocation_permitted`
                    // already decided this configuration is allowed --  stage (a)
                    // of `zgc-jit-load-barrier.md` landed 2026-08-13 and
                    // `zgc_codegen_honours_read_barrier()` is true -- and
                    // `relocate_stw` refuses anyway while a JIT frame is on a
                    // stack. Promotion inherits both. A load barrier is what
                    // CONCURRENT relocation needs, not this.
                    let post_slide = {
                        let arena = self.arena.lock();
                        arena.base_ptr() as usize + arena.used_low_for_compaction()
                    };
                    // t1 wave 8 lane ZT3GN (`CRATONVM_ZGC_GEN_YOUNG_EVACUATE_FLOOR_HOLD`,
                    // default ON inside the opt-in scoped slide, `=0` kill): a
                    // SCOPED young slide does not advance the floor. Its cursor
                    // rarely drops to the packed survivors' end: a conservatively
                    // pinned survivor on the newest page (t1 wave 8 rows:
                    // `pinned_end_off == low_end_off` on most minors of
                    // `Zr1GnGenShape`) keeps it at the top, so the G2 advance put
                    // the floor at the top and handed the slide's vacated holes
                    // to the allocator BELOW it, where the next young cycle swept
                    // their objects one header at a time (pass (1)); repeated, it
                    // marched the floor to the arena's end. Held, the cycle's
                    // bookkeeping is a no-slide young cycle's: every survivor is
                    // still at or above the floor, so the stores below are
                    // skipped with it. (`reclaimed` is unused by the rule.)
                    let _ = reclaimed;
                    let floor_hold = young_cycle
                        && matches!(young_scope, Some(Ok(_)))
                        && self.counters.young_evac.floor_hold.load(Ordering::Relaxed);
                    if floor_hold {
                        self.counters
                            .young_evac
                            .floor_holds
                            .fetch_add(1, Ordering::Relaxed);
                    } else {
                        self.gen_young_floor.store(post_slide, Ordering::Relaxed);
                        // zr z1/gn: on a YOUNG cycle `bytes_copied` is only what the
                        // young sweep visited, so storing it dropped the whole old
                        // generation from the live estimate -- and with it from
                        // `allocated` after every later young cycle of the epoch.
                        // `live_bytes` is old + young on either kind of cycle.
                        // `CRATONVM_ZGC_GEN_SLIDE_OLD_LIVE=0` restores the old store.
                        let below_floor_live =
                            if young_cycle && self.counters.local.gen_slide_old_live() {
                                live_bytes
                            } else {
                                self.gen_old_live_after_major(
                                    bytes_copied,
                                    gen_young_high_bytes,
                                    gen_old_high_bytes,
                                )
                            };
                        self.gen_old_live_bytes
                            .store(below_floor_live, Ordering::Relaxed);
                        self.counters
                            .gen_nursery_old_bytes
                            .store(0, Ordering::Relaxed);
                    }
                    // The old-start bits move with the objects.
                    self.remap_old_bits_after_slide(&map, promo_age);
                    self.counters
                        .gen_promotions_by_slide
                        .fetch_add(moved, Ordering::Relaxed);
                    tracing::debug!(
                        target: "zgc",
                        moved,
                        floor = post_slide,
                        "zgc G2: the slide promoted its survivors; nursery empty"
                    );
                }
                if gen_on && self.has_old_objects.load(Ordering::Relaxed) {
                    let carded = self.recard_relocated_old_objects(&map, promo_age);
                    tracing::debug!(
                        target: "zgc",
                        carded,
                        moved,
                        "zgc: re-carded the old objects a slide moved"
                    );
                }
                tracing::debug!(
                    target: "zgc",
                    moved,
                    reclaimed,
                    "zgc STW compaction complete"
                );
            }
            pointer_map = map;
        }
        // zt t27/pc2 (`CRATONVM_ZGC_SWEEP_SIZE_MEMO`): keep this sweep's size
        // cells for the next sweep, minus every base this cycle's slide moved
        // away from (both ends; the map's keys), stamped for the `gc_count`
        // the next collection reads before its own increment (this cycle's
        // increment already ran, after the sweep). Only on the arm the next
        // sweep may use it on; see `ZSweepSizeMemo` for the argument.
        if cfg.record_live_cells && zgc_sweep_size_memo_enabled() && !gen_on && !young_cycle {
            let moved: Vec<usize> = if moved_this_cycle > 0 {
                pointer_map.keys().copied().collect()
            } else {
                Vec::new()
            };
            *self.counters.size_memo.lock() = ZSweepSizeMemo::from_hints(
                sweep_hints,
                moved,
                self.gc_count(),
                cratonvm_types::layout_replace_epoch(),
            );
        }
        // zt t23/zc5 (R1): this cycle's `[GC] zgc-rsel:` line, with the slide's
        // selection when the block above ran one. One relaxed swap when off.
        self.reloc_select_census_emit();
        // THE SLIDE'S DURATION, read outside the logging gate.
        //
        // `relocate_started` is `Some` exactly when `gc_started` is, and the
        // `zgc-relocate` line reads the same clock -- but only under
        // `gc_log_enabled`, so on a quiet run the number existed and was
        // discarded. Both the controller arm above and `ZgcMetrics` want it on
        // every run, and `Instant::elapsed` on an already-taken `Instant` is a
        // clock read, not a new measurement.
        let relocate_us = relocate_started.map_or(0u128, |s| s.elapsed().as_micros());
        // zr z2/g2: `[GC] zgc-gen-cost:` -- this cycle's laps, by kind. Only on
        // a generational heap and only when the pause clock ran.
        if gen_on {
            if let Some(started) = gc_started {
                let c = &self.counters.gen_cost;
                let y = young_cycle;
                c.add(y, GEN_COST_CYCLES, 1);
                c.add(y, GEN_COST_PAUSE_US, started.elapsed().as_micros() as u64);
                c.add(
                    y,
                    GEN_COST_PRE_US,
                    (markend_us + tlab_us + snapshot_us) as u64,
                );
                c.add(y, GEN_COST_MARK_US, mark_us as u64);
                c.add(y, GEN_COST_REMSET_US, remset_us);
                c.add(y, GEN_COST_REFS_US, (resurrect_us + refs_us) as u64);
                c.add(y, GEN_COST_SWEEP_US, sweep_us as u64);
                c.add(y, GEN_COST_CENSUS_US, census_us);
                c.add(y, GEN_COST_RELOCATE_US, relocate_us as u64);
                c.add(y, GEN_COST_SWEPT, swept as u64);
                c.add(y, GEN_COST_DEAD, dead_count as u64);
                c.add(y, GEN_COST_MOVED, moved_this_cycle as u64);
                c.add(y, GEN_COST_STREAM_US, stream_us);
                c.add(y, GEN_COST_SWEEP_WALK_US, sweep_walk_ns / 1000);
            }
        }
        // FEED THE INSTRUMENT. See `record_metrics_for_cycle` for the
        // phase-by-phase mapping and for why five `ZgcPhase`s deliberately stay
        // at `count=0` on this collector.
        self.record_metrics_for_cycle(
            ZCycleLaps {
                timed: gc_started.is_some(),
                marked_concurrently,
                relocated: relocated_this_cycle,
                markend_us,
                tlab_us,
                snapshot_us,
                mark_us,
                resurrect_us,
                refs_us,
                sweep_us,
                relocate_us,
            },
            bytes_in_use_before,
            live_bytes,
            cap,
            trigger_mask_for_cycle,
        );
        // RE-DERIVE THE ALLOCATION BUDGET, **after the slide**.
        //
        // The other arm of `CRATONVM_ZGC_PAUSE_TARGET_INCLUDES_RELOCATE`; the
        // pre-slide copy above runs when this one does not, so exactly one of
        // them feeds the loop per cycle. The controller sizes
        // `alloc_trigger_bytes`, i.e. how often this collector runs, so the
        // pause it is fed has to be the pause the mutators actually took --
        // and compaction is default-on and costs about twice the pre-slide
        // reading on `ZgcTlabStress 8 200`.
        //
        // `gc_started` is still live here: it is a `Copy` `Option<Instant>`
        // captured before the pause opened, and the block above only ever
        // *reads* it (`gc_started.map_or(0, |s| s.elapsed()..)` on the
        // `zgc-relocate` line). This reading is the same quantity that line
        // prints as `pause_with_relocate_us`, so the log and the controller
        // now agree by construction.
        //
        // FULL CYCLES ONLY, unchanged: the `!young_cycle` exclusion is about
        // `bytes_freed` describing the nursery while `live_bytes` includes an
        // old generation the cycle never walked, and moving the clock does not
        // touch that argument. It does mean a young cycle that relocated still
        // teaches the loop nothing -- which is the pre-existing behaviour, not
        // a regression introduced here; see
        // `gap-e-relocate-runs-outside-the-measured-pause.md` for why a young
        // pause wants its own cost function rather than a share of this one.
        if zgc_pause_target_includes_relocate() {
            if let Some(started) = gc_started {
                if !young_cycle {
                    self.refresh_pause_target_budget(
                        started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
                        live_bytes,
                        bytes_freed,
                    );
                }
            }
        }
        // ORDER IS LOAD-BEARING: prune the dead FIRST, then remap survivors.
        //
        // Compaction makes these two operations collide on the same address.
        // Survivors slide DOWN into space vacated by dead objects, so a dead
        // base X is very often a survivor L's new base. `remap_after_gc`
        // drains every shard and re-inserts, RETAINING an entry absent from
        // the map (correct: an in-place survivor is legitimately absent) — so
        // a dead object D's entry stays at X while L's entry is re-keyed to X,
        // and whichever insert lands second wins. Either L loses its monitor,
        // or L inherits D's, which is the exact thing `prune_dead`'s doc says
        // must not happen: "a NEW object reusing the address MUST get a fresh
        // monitor, not the dead object's".
        //
        // Pruning first removes D's entry while L's is still keyed at its OLD
        // address, so the remap moves L into a slot nobody else claims. It
        // also restores `prune_dead`'s documented precondition without any
        // filtering: at that moment every address in `dead` really is a freed
        // base, because no survivor has been re-keyed onto one yet — which is
        // what licenses its `Monitor::release_mark_ref`.
        //
        // A first attempt at this (2026-08-14, same day) kept the old order and
        // screened `dead` against the post-slide registry instead. That stopped
        // the collector freeing a live object's monitor, and it left D's stale
        // monitor sitting at X for L to inherit — trading a use-after-free for
        // a wrong-monitor bug. `io.netty.util.ResourceLeakDetectorTest` went
        // from FAIL to CRASH on it. Ordering fixes both; screening fixes one
        // and creates the other.
        // EMPTY WHEN `wants_dead_addresses` SAID SO, and that is not a
        // silently weakened prune: the same shard survey answered it, one
        // phase earlier, under the same stop-the-world token. `prune_dead`
        // early-returns on an empty slice, which is the identical outcome its
        // own survey reaches.
        // zr z5/sw: a slice the key probe built is `keys ∩ dead`, possibly
        // empty; `prune_dead_filtered` runs the per-pause bookkeeping that an
        // empty slice would otherwise skip.
        if prune_keys.is_some() {
            monitors.prune_dead_filtered(dead.as_slice());
        } else {
            monitors.prune_dead(dead.as_slice());
        }
        // zr z3/fp: `prune_dead` was the scratch's one reader. Record its peak,
        // and give back capacity a long run of small cycles has not used
        // (`CRATONVM_ZGC_FOOTPRINT_TRIM`).
        self.counters.fp.after_dead_scratch(dead);
        monitors.remap_after_gc(&pointer_map);

        // (gc-common w29-b: the identity-hash eviction call that stood here is
        // gone with its channel. The native side tables it served drop a dead
        // object's rows through the VM's weak lock-key sweep, which the common
        // stop-the-world epilogue runs for all three backends.)

        // THE REFERENCE PROCESSOR'S OWN TABLES MOVE TOO.
        //
        // `ref_processor` holds raw `usize` addresses -- `reference_obj`,
        // `referent` and `queue_addr` for every discovered soft/weak/phantom/
        // cleaner/finalizer `Reference`, plus the soft-ref address index and
        // the finalization queue. They are collector-side tables, not Java
        // slots, so the rewrite pass in `relocate_and_compact` cannot see them
        // and `pointer_map` never reached them.
        //
        // `process_references` said, until 2026-08-15:
        //
        //     (Non-moving: addresses are stable, so no `update_after_gc`
        //     relocation is needed.)
        //
        // True when it was written and false since compaction went default-on
        // on 2026-08-13 -- the same expired premise as the JNI critical pin,
        // the reference-processing guard and the monitor prune. Unlike those,
        // this one survives ACROSS cycles: the tables persist, and next
        // cycle's `pointer_map` does not contain last cycle's pre-move
        // addresses, so a stale entry is stale forever.
        //
        // What it costs, both directions, on the NEXT collection:
        //
        //  * `is_marked_addr(stale)` reads the zeroed span `compact_low_to`
        //    left behind, finds no mark bit, and `remove_collected` drops the
        //    entry -- a LIVE `WeakReference` silently leaves the processor and
        //    is never cleared or enqueued. No `Cleaner`, no queue delivery.
        //  * If a new object has since been allocated at that address and IS
        //    marked, `process_references` instead writes null into FIELD 0 of
        //    an unrelated live object. That is the `field index OOB index=0
        //    num_slots=0` signature when the new occupant has no slots.
        //
        // ZGC is the only backend that owns a live `ReferenceProcessor` (G1
        // constructs one only in its tests), which is why this presents as a
        // ZGC-only defect on the one netty class built entirely out of weak
        // references.
        //
        // Placed AFTER the monitor remap for the same reason that one is
        // ordered as it is, though the collision cannot arise here:
        // `remove_collected` already ran in `process_references`, BEFORE the
        // slide, so no dead entry is left for a survivor to inherit.
        if !pointer_map.is_empty() {
            self.counters
                .ref_processor
                .lock()
                .update_after_gc(&pointer_map);

            // SAME FAMILY: the resurrected-finalizer list is an OUTPUT, and
            // its contract says POST-move.
            //
            // `collect_garbage_with_finalizers` returns these addresses to the
            // runtime, which enqueues each one on the `FinalizerThread` --
            // "(their new addresses)", as the call site in
            // `vm/src/runtime/interpreter/gc_and_alloc.rs` puts it. G1 states
            // the contract outright: "the POST-copy addresses of objects Phase
            // 3.5 resurrected this collection".
            //
            // Here the list is filled during the remark, several phases BEFORE
            // the slide, and nothing rewrote it. Every resurrected object is by
            // definition live, so it is exactly the kind of object the slide
            // relocates -- and `finalize()` would then run against the vacated
            // address.
            let mut resurrected = self.counters.resurrected_finalizers.lock();
            for addr in resurrected.iter_mut() {
                if let Some(&new) = pointer_map.get(addr) {
                    *addr = new;
                }
            }
        }

        // Disarmed before any mutator runs again -- see the rebuild at the top.
        self.counters.mark_root_filter.disarm();
        // zs s3/ac: close the pause CPU span opened at the top.
        self.book_pause_cpu(cycle, &pause_cpu, cpu_before, gc_started);
        GcResult {
            stats: GcStats {
                objects_copied,
                bytes_copied,
                bytes_freed,
            },
            pointer_map,
        }
    }

    /// Card the written object when it lives in an old page --
    /// `zgc::remembered`, Phase 4.
    ///
    /// This arm said "Non-generational, non-concurrent: nothing to record"
    /// and was empty. It is the hook every reference store in the VM already
    /// reaches, so it is where a generational collector's card barrier
    /// belongs; see [`ZgcRealHeap::note_ref_store`] for why a card names an
    /// object rather than a slot, and for the cost while no page is old (one
    /// `is_empty` check).
    ///
    /// `stored_value` is deliberately not consulted. Filtering to
    /// "the value is a young reference" would card fewer objects, but it
    /// needs the value's page, which is another lock on the store path to
    /// avoid a re-scan that only costs anything at cycle time. Over-carding
    /// is safe -- a stale or unnecessary card makes a young cycle scan an
    /// object it did not need to -- while under-carding is a use-after-free.
    fn write_barrier(&self, obj: ObjectRef, _stored_value: Value) {
        self.note_ref_store(obj.as_ptr() as usize);
    }

    /// The SATB pre-barrier through the trait (zr z1/br).
    ///
    /// This heap used to keep the trait's EMPTY default, so
    /// `VmHeap::write_barrier_pre` and `VmHeap::write_barrier_keep_alive`
    /// were no-ops on the default collector while every other spelling of
    /// the same barrier (`VmHeap::satb_barrier`, the accessors) published.
    /// Today's three `write_barrier_pre` callers (interpreter `putfield`,
    /// `Vm::set_instance_field`, deopt materialisation) all store through
    /// `set_field` next, which publishes, so nothing was lost; but a caller
    /// that pairs the trait barrier with a raw store, and any first caller of
    /// `write_barrier_keep_alive` (none today), would have got nothing.
    /// While nothing marks this is the one relaxed load of
    /// [`Self::satb_pre_barrier`]; while marking, a duplicate publication is
    /// deduplicated by the mark bit.
    fn write_barrier_pre(&self, _slot: *mut ObjectRef, old: ObjectRef) {
        self.satb_pre_barrier(old.as_ptr() as usize);
    }

    fn allocated_bytes(&self) -> usize {
        self.allocated.load(Ordering::Relaxed)
    }
}
