#[allow(unused_imports)]
use super::*;

impl ClassManager {
    /// Step 5b of [`Self::redefine_class`] without installing anything
    /// (interpreter round i1 wave 28, lane L3): the new bodies are verified in
    /// a scratch copy of the live class ([`redefinition_scratch_class`]) under
    /// the policy the redefinition applies ([`verify_redefined_class`]), and
    /// the type maps the verifier publishes land in a throwaway store
    /// ([`ScratchTypeMaps`]), so the live class's maps keep describing its
    /// live bodies whatever the call decides.
    ///
    /// The scratch class carries the NEW constant pool alone; the redefinition
    /// installs it with the old constants appended for obsolete frames
    /// (`merge_for_obsolete_code_compacting`), which the new bodies never
    /// index and the verifier never walks as a table.
    pub(in crate::class_manager) fn verify_redefinition_ahead(
        &self,
        class_id: ClassId,
        class_name: &str,
        new_class_file: cratonvm_reader::ClassFile,
        new_bytes: &[u8],
    ) -> Result<(), RedefinitionRefused> {
        let Some(live) = self.class_store.get(class_id) else {
            // `redefinition_shape` found it; the redefinition reports this.
            return Ok(());
        };
        // The redefinition's own skip cases (see `verify_skip` there).
        if live.origin.is_compatibility_stub()
            || self.skip_bytecode_verification.contains(&class_id)
        {
            return Ok(());
        }
        let generation = self.class_redefine_generation(class_id);
        let requesting_loader = Some(live.loader_id);
        let scratch = redefinition_scratch_class(live, new_class_file);
        let verdict = {
            // As the redefinition's: the class is resident, not in flight.
            let hierarchy = ClassStoreHierarchy {
                class_store: &self.class_store,
                loaded_classes: &self.loaded_classes,
                user_loader_parents: &self.user_loader_parents,
                in_flight: None,
                in_flight_super_name: None,
                requesting_loader,
                verifier_loads: None,
                app_workarounds: &self.app_workarounds,
            };
            let scratch_maps = ScratchTypeMaps {
                inner: &hierarchy,
                store: crate::type_maps::TypeMapStore::new(),
            };
            verify_redefined_class(&scratch, &self.class_store, &scratch_maps)
        };
        match verdict {
            Ok(()) => {
                let mut verified = self.pre_verified_redefinitions.lock();
                // Bounded: an entry a refused call left behind is dead weight
                // until the class is checked or redefined again.
                if verified.len() >= 4096 {
                    verified.clear();
                }
                verified.insert(class_id, (class_bytes_digest(new_bytes), generation));
                Ok(())
            }
            Err(verify_err) => {
                debug!(
                    class = %class_name,
                    error = ?verify_err,
                    "WP2.4-B redefine check: bytecode verification failed; nothing installed",
                );
                dump_rejected_redefine_bytes(class_name, new_bytes);
                Err(RedefinitionRefused::new(
                    RedefinitionRefusal::FailsVerification,
                    class_name,
                    format!("new bytes failed bytecode verification: {verify_err}"),
                ))
            }
        }
    }

    /// Did [`Self::check_redefinition`] verify exactly `bytes` for `class_id`
    /// at its current redefinition generation? Takes the note either way.
    pub(in crate::class_manager) fn take_pre_verified_redefinition(&self, class_id: ClassId, bytes: &[u8]) -> bool {
        let note = self.pre_verified_redefinitions.lock().remove(&class_id);
        note == Some((
            class_bytes_digest(bytes),
            self.class_redefine_generation(class_id),
        ))
    }

    /// Is a verification by [`Self::check_redefinition`] waiting for
    /// `class_id`'s redefinition to take it?
    pub fn redefinition_pre_verified(&self, class_id: ClassId) -> bool {
        self.pre_verified_redefinitions
            .lock()
            .contains_key(&class_id)
    }

    /// Step 0 of [`Self::redefine_class`]: header sanity (cheap pre-checks).
    pub(in crate::class_manager) fn check_redefinition_header(
        class_id: ClassId,
        new_bytes: &[u8],
    ) -> Result<(), RedefinitionRefused> {
        if new_bytes.len() < 8 {
            return Err(RedefinitionRefused::new(
                RedefinitionRefusal::InvalidClassFormat,
                class_id.as_u32().to_string(),
                format!(
                    "new_bytes too short ({} bytes; need >= 8 for header)",
                    new_bytes.len()
                ),
            ));
        }
        if new_bytes[0..4] != [0xCA, 0xFE, 0xBA, 0xBE] {
            return Err(RedefinitionRefused::new(
                RedefinitionRefusal::InvalidClassFormat,
                class_id.as_u32().to_string(),
                "bad magic in new_bytes; expected CAFEBABE",
            ));
        }
        Ok(())
    }

    /// Snapshot the existing class identity. We need the name + loader_id for
    /// the cache update at the end of [`Self::redefine_class`], plus the
    /// structural snapshot (super/interfaces/fields/methods) for the
    /// constraint checks. All of this is cloned into owned data so the borrow
    /// ends before the JVMTI hook (which may re-enter the loader) is called.
    ///
    /// TODO(T10.9.E): the `to_string()` calls in this block could be swapped
    /// for `Arc::clone(&name)` style refcount bumps. Skipped here because this
    /// is the cold JVMTI `redefine_class` path (called once per agent
    /// retransform, not per class-load) — the String snapshots are used
    /// downstream as owned, comparable values for structural-equivalence
    /// checks, so converting them also requires touching the comparator.
    pub(in crate::class_manager) fn redefinition_shape(
        &self,
        class_id: ClassId,
    ) -> Result<RedefinitionShape, RedefinitionRefused> {
        let cls = self.class_store.get(class_id).ok_or_else(|| {
            RedefinitionRefused::new(
                RedefinitionRefusal::InvalidClass,
                class_id.as_u32().to_string(),
                "class id not loaded",
            )
        })?;
        // Resolve super name from the super's ClassId (or empty
        // string for java/lang/Object which has no super).
        let super_name = match cls.superclass {
            Some(sid) => self
                .class_store
                .get(sid)
                .map(|s| s.name.to_string())
                .unwrap_or_default(),
            None => String::new(),
        };
        // Direct interfaces — internal names in declaration order.
        let iface_names: Vec<String> = cls
            .interfaces
            .iter()
            .filter_map(|iid| self.class_store.get(*iid).map(|i| i.name.to_string()))
            .collect();
        // Field signatures: (name, descriptor, access_flag bits).
        let field_sigs: Vec<(String, String, u16)> = cls
            .fields
            .iter()
            .map(|f| {
                (
                    f.name.to_string(),
                    f.descriptor.to_string(),
                    f.access_flags.bits(),
                )
            })
            .collect();
        // Method signatures: (name, descriptor, access_flag bits).
        let method_sigs: Vec<(String, String, u16)> = cls
            .methods
            .iter()
            .map(|m| {
                (
                    m.name.to_string(),
                    m.descriptor.to_string(),
                    m.access_flags.bits(),
                )
            })
            .collect();
        // A compatibility stub's flags are minted, not read from a class
        // file, and a hidden class is never offered for redefinition by
        // `isModifiableClass`: compare the modifiers of neither.
        let class_flags = if cls.origin.is_compatibility_stub() || cls.hidden {
            None
        } else {
            Some(cls.access_flags.bits())
        };
        // The same classes' nest, record and sealed attributes (a stub's are
        // derived from its name, a hidden class's nest host may be its
        // definer's lookup class): wave 45, lane L3.
        let nesting = if cls.origin.is_compatibility_stub() || cls.hidden {
            None
        } else {
            Some(RedefinitionNesting {
                nest_host: cls.nest_host.clone(),
                nest_members: cls.nest_members.clone(),
                record_components: cls
                    .record_components
                    .iter()
                    .map(|rc| (rc.name.clone(), rc.descriptor.clone()))
                    .collect(),
                permitted_subclasses: cls.permitted_subclasses.clone(),
            })
        };
        Ok(RedefinitionShape {
            name: cls.name.to_string(),
            loader: cls.loader_id,
            super_name,
            iface_names,
            field_sigs,
            method_sigs,
            class_flags,
            nesting,
        })
    }

    /// Step 2 of [`Self::redefine_class`]: parse the new bytes and decode the
    /// attribute tables the rest of the redefinition reads.
    pub(in crate::class_manager) fn parse_redefinition_bytes(
        existing_name: &str,
        bytes: &[u8],
        preview_enabled: bool,
    ) -> Result<cratonvm_reader::ClassFile, RedefinitionRefused> {
        // This VM's `--enable-preview` (interpreter round i5 wave 101, lane L5).
        let mut new_class_file = cratonvm_reader::read_class_with_preview(bytes, preview_enabled).map_err(|e| {
            RedefinitionRefused::new(
                RedefinitionRefusal::InvalidClassFormat,
                existing_name,
                format!("new_bytes failed to parse: {e}"),
            )
        })?;

        // Same rationale as `define_class_with_options`: every class-level
        // attribute we look at below (`BootstrapMethods`, `RuntimeVisible
        // [/Invisible]Annotations`, `SourceFile`) feeds the runtime `Class`,
        // so eager decode of the class-level table is the cheapest path
        // and keeps the rest of this function index/match-style identical
        // to the pre-lazy code.
        force_decode_all(
            &mut new_class_file.attributes,
            &new_class_file.constant_pool,
        )
        .map_err(|e| {
            RedefinitionRefused::new(
                RedefinitionRefusal::InvalidClassFormat,
                existing_name,
                format!("attribute decode failed: {e}"),
            )
        })?;

        // Eagerly decode each method's attribute table too — mirrors
        // `define_class_with_options`. The Step-5 verifier
        // (`verify_code_attribute_presence`) and the post-swap interpreter
        // both read the `Code` attribute via `method.code()`, which only
        // returns `Some` for an *already-decoded* attribute. Without this,
        // every method's `Code` stays a lazy `Raw` attribute, so the verifier
        // sees a concrete method with no Code and rejects the redefine with
        // "non-abstract non-native method must have Code attribute" (observed
        // on `Foo.<init>` in the wp2_4b_redefine suite). Field attributes stay
        // lazy — nothing on this path reads them.
        for m in new_class_file.methods.iter_mut() {
            force_decode_all(&mut m.attributes, &new_class_file.constant_pool).map_err(|e| {
                RedefinitionRefused::new(
                    RedefinitionRefusal::InvalidClassFormat,
                    existing_name,
                    format!("method attribute decode failed: {e}"),
                )
            })?;
        }
        Ok(new_class_file)
    }

    /// Steps 3 and 4 of [`Self::redefine_class`]: the name match (always
    /// enforced) and the structural-equivalence checks (skippable), in
    /// HotSpot's order (`compare_and_normalize_class_versions`): hierarchy,
    /// the nest / record / sealed attributes (wave 45), class modifiers,
    /// fields, methods.
    pub(in crate::class_manager) fn check_redefinition_structure(
        shape: &RedefinitionShape,
        new_class_file: &cratonvm_reader::ClassFile,
        options: &RedefineOptions,
    ) -> Result<(), RedefinitionRefused> {
        let existing_name = &shape.name;
        let refuse = |kind: RedefinitionRefusal, message: String| {
            RedefinitionRefused::new(kind, existing_name.as_str(), message)
        };
        // ---- Step 3: name match (always enforced) ----
        // `new_class_file.this_class` is `Arc<str>` (round 4 reader),
        // `existing_name` is `String`. Compare via `&str` to avoid an
        // intermediate allocation.
        if &*new_class_file.this_class != existing_name.as_str() {
            return Err(refuse(
                RedefinitionRefusal::NamesDontMatch,
                format!(
                    "new bytes declare this_class = {}, expected {}",
                    new_class_file.this_class, existing_name,
                ),
            ));
        }

        // ---- Step 4: structural-equivalence checks (skippable) ----
        if options.skip_structural_check {
            return Ok(());
        }
        // 4a — superclass name match. `super_class` is now
        // `Option<Arc<str>>`; materialise into the `String` baseline
        // the existing comparison + format expects (this path runs
        // only on JVMTI redefine, not bootstrap).
        let existing_super_name = &shape.super_name;
        let new_super_name: String = new_class_file
            .super_class
            .as_ref()
            .map(|s| s.to_string())
            .unwrap_or_default();
        if &new_super_name != existing_super_name {
            return Err(refuse(
                RedefinitionRefusal::HierarchyChanged,
                format!(
                    "superclass changed: was {} now {}",
                    if existing_super_name.is_empty() {
                        "<none>"
                    } else {
                        existing_super_name.as_str()
                    },
                    if new_super_name.is_empty() {
                        "<none>"
                    } else {
                        new_super_name.as_str()
                    },
                ),
            ));
        }
        // 4b — interface list (counts, order, names).
        let existing_iface_names = &shape.iface_names;
        if new_class_file.interfaces.len() != existing_iface_names.len() {
            return Err(refuse(
                RedefinitionRefusal::HierarchyChanged,
                format!(
                    "interface count changed: was {} now {}",
                    existing_iface_names.len(),
                    new_class_file.interfaces.len(),
                ),
            ));
        }
        for (i, (old, new)) in existing_iface_names
            .iter()
            .zip(new_class_file.interfaces.iter())
            .enumerate()
        {
            // `old: &String`, `new: &Arc<str>` — compare via &str.
            if old.as_str() != &**new {
                return Err(refuse(
                    RedefinitionRefusal::HierarchyChanged,
                    format!("interface[{i}] changed: was {old} now {new}"),
                ));
            }
        }
        // 4b'' — the NestHost, NestMembers, Record and PermittedSubclasses
        // attributes (interpreter round i1 wave 45, lane L3). HotSpot refuses
        // a change to any of them with CLASS_ATTRIBUTE_CHANGED, after the
        // hierarchy and before the class modifiers, fields and methods
        // (measured: a donor that drops the NestHost and also adds a method,
        // a field or `final` is refused for the attribute; one that also adds
        // an interface, for the hierarchy). This check was missing: a nest
        // member redefined with bytes naming no nest host was installed, and
        // a record given one component more was refused as a schema change
        // (`L3W45RedefineShapes.java`).
        if let Some(old_nesting) = &shape.nesting {
            let nesting =
                NestingAttributes::of(&new_class_file.attributes, &new_class_file.constant_pool);
            let new_nesting = RedefinitionNesting {
                nest_host: nesting.nest_host,
                nest_members: nesting.nest_members,
                record_components: nesting
                    .record_components
                    .into_iter()
                    .map(|rc| (rc.name, rc.descriptor))
                    .collect(),
                permitted_subclasses: nesting.permitted_subclasses,
            };
            if let Some(message) = old_nesting.changed_by(&new_nesting) {
                return Err(refuse(RedefinitionRefusal::ClassAttributeChanged, message));
            }
        }
        // 4b' — class modifiers (interpreter round i1 wave 27, lane L3). JVMTI
        // forbids changing them; HotSpot compares the recognised class
        // modifiers (`JVM_RECOGNIZED_CLASS_MODIFIERS`: public, final, super,
        // interface, abstract, synthetic, annotation, enum) and refuses with
        // CLASS_MODIFIERS_CHANGED. This check was missing: a redefinition
        // that made a class final was installed with the old flags kept.
        const RECOGNIZED_CLASS_MODIFIERS: u16 = 0x0001 // public
            | 0x0010 // final
            | 0x0020 // super
            | 0x0200 // interface
            | 0x0400 // abstract
            | 0x1000 // synthetic
            | 0x2000 // annotation
            | 0x4000; // enum
        if let Some(old_flags) = shape.class_flags {
            let old_flags = old_flags & RECOGNIZED_CLASS_MODIFIERS;
            // zs s6/rv4: the new bytes' flags as `define` STORES them
            // (`hotspot_stored_class_flags`, zs s4/rp), which is what the old
            // side is: a pre-50 interface gains `ACC_ABSTRACT` on both sides,
            // as HotSpot parses both. Masking the raw flags alone refused the
            // redefinition of an old interface whose file omits it with
            // `CLASS_MODIFIERS_CHANGED`.
            let new_flags = hotspot_stored_class_flags(
                new_class_file.access_flags.bits(),
                new_class_file.version.major,
            ) & RECOGNIZED_CLASS_MODIFIERS;
            if old_flags != new_flags {
                return Err(refuse(
                    RedefinitionRefusal::ClassModifiersChanged,
                    format!(
                        "class modifiers changed: was flags={old_flags:#x} now \
                         flags={new_flags:#x}",
                    ),
                ));
            }
        }
        // 4c — field set (counts + name+desc+modifiers as a SET). Like
        // methods (4d), JVMTI matches fields by name+descriptor and is
        // order-insensitive; fields are not swapped by redefine (the live
        // instance layout is retained), so this is a pure equivalence
        // check. Field (name, desc) pairs are unique within a class per
        // JVMS §4.5.
        let existing_field_sigs = &shape.field_sigs;
        if new_class_file.fields.len() != existing_field_sigs.len() {
            return Err(refuse(
                RedefinitionRefusal::SchemaChanged,
                format!(
                    "field count changed: was {} now {}",
                    existing_field_sigs.len(),
                    new_class_file.fields.len(),
                ),
            ));
        }
        let existing_field_map: FxHashMap<(&str, &str), u16> = existing_field_sigs
            .iter()
            .map(|(n, d, f)| ((n.as_str(), d.as_str()), *f))
            .collect();
        for new_field in &new_class_file.fields {
            let new_name: &str = &new_field.name;
            let new_desc: &str = &new_field.descriptor;
            let new_flags = new_field.access_flags.bits();
            match existing_field_map.get(&(new_name, new_desc)) {
                None => {
                    return Err(refuse(
                        RedefinitionRefusal::SchemaChanged,
                        format!(
                            "field set changed: {new_name}:{new_desc} not present in the \
                             loaded class (JEP 109 forbids add/remove/rename)",
                        ),
                    ));
                }
                Some(&old_flags) if old_flags != new_flags => {
                    return Err(refuse(
                        RedefinitionRefusal::SchemaChanged,
                        format!(
                            "field {new_name}:{new_desc} modifiers changed: was \
                             flags={old_flags:#x} now flags={new_flags:#x}",
                        ),
                    ));
                }
                Some(_) => {}
            }
        }
        // 4d — method declarations (counts + name+desc+modifiers as a
        // SET). JVMTI RedefineClasses/RetransformClasses matches methods
        // by name+descriptor, NOT by position: a transformer is free to
        // re-emit the method table in a different order (ByteBuddy's
        // inline mock maker re-derives the class from its original bytes
        // and routinely reorders constructors/methods). HotSpot accepts
        // this; an order-sensitive check here spuriously rejects the
        // redefine, the woven advice is never installed, and Mockito
        // inline mocks of concrete classes silently fail to intercept.
        // We therefore validate that the (name, descriptor, flags) MULTISET
        // is identical and defer the actual body swap to a name+desc match
        // that preserves the existing method order (see Step 5 below).
        let existing_method_sigs = &shape.method_sigs;
        if new_class_file.methods.len() != existing_method_sigs.len() {
            // HotSpot names the direction: more methods is an add, fewer a
            // delete.
            let kind = if new_class_file.methods.len() > existing_method_sigs.len() {
                RedefinitionRefusal::MethodAdded
            } else {
                RedefinitionRefusal::MethodDeleted
            };
            return Err(refuse(
                kind,
                format!(
                    "method count changed: was {} now {} (JEP 109 forbids add/remove)",
                    existing_method_sigs.len(),
                    new_class_file.methods.len(),
                ),
            ));
        }
        // Map existing (name, desc) -> flags. Method (name, desc) pairs are
        // unique within a class per JVMS §4.5/§4.6, so a plain map is a
        // faithful key set.
        let existing_method_map: FxHashMap<(&str, &str), u16> = existing_method_sigs
            .iter()
            .map(|(n, d, f)| ((n.as_str(), d.as_str()), *f))
            .collect();
        for new_method in &new_class_file.methods {
            let new_name: &str = &new_method.name;
            let new_desc: &str = &new_method.descriptor;
            let new_flags = new_method.access_flags.bits();
            match existing_method_map.get(&(new_name, new_desc)) {
                None => {
                    // Same count, one method renamed: an add and a delete.
                    // Which one HotSpot names first depends on its symbol
                    // order; the add is the one this loop found.
                    return Err(refuse(
                        RedefinitionRefusal::MethodAdded,
                        format!(
                            "method set changed: {new_name}{new_desc} not present in the \
                             loaded class (JEP 109 forbids add/remove/rename)",
                        ),
                    ));
                }
                Some(&old_flags) if old_flags != new_flags => {
                    return Err(refuse(
                        RedefinitionRefusal::MethodModifiersChanged,
                        format!(
                            "method {new_name}{new_desc} modifiers changed: was \
                             flags={old_flags:#x} now flags={new_flags:#x}",
                        ),
                    ));
                }
                Some(_) => {}
            }
        }
        // Counts are equal and every new (name, desc) maps to a distinct
        // existing key (new keys are themselves unique), so the match is a
        // bijection — no existing method is left unmatched.
        Ok(())
    }

    /// [`Self::redefine_class`] with the refusal typed
    /// ([`RedefinitionRefused`]), for callers that must report it as HotSpot
    /// does (the `java.lang.instrument` natives; interpreter round i1 wave 27,
    /// lane L3).
    pub fn redefine_class_typed(
        &mut self,
        class_id: ClassId,
        new_bytes: Vec<u8>,
        options: RedefineOptions,
    ) -> Result<(), RedefinitionRefused> {
        // ---- Step 0: header sanity (cheap pre-checks) ----
        Self::check_redefinition_header(class_id, &new_bytes)?;

        // Snapshot the existing class identity (see `redefinition_shape`).
        let shape = self.redefinition_shape(class_id)?;
        let existing_name = shape.name.clone();
        let existing_loader = shape.loader;

        // ---- Step 1: fire ClassFileLoadHook (pre-parse) ----
        // The agent sees the OLD bytes and the NEW bytes and may
        // return its own transformed buffer. We grab the old bytes
        // from `class_bytes_cache`; if not present (synthetic stub or
        // old code path that never recorded them) we pass an empty
        // slice — JVMTI agents tolerate that.
        let old_bytes_opt = self.defined_class_bytes(class_id);
        let old_bytes_slice: &[u8] = old_bytes_opt.as_deref().unwrap_or(&[]);
        let class_id_u32 = class_id.as_u32();
        let effective_new_bytes: Vec<u8> = match fire_class_file_load_hook(
            class_id_u32,
            &existing_name,
            old_bytes_slice,
            &new_bytes,
        ) {
            Some(transformed) => transformed,
            None => new_bytes,
        };
        // `check_redefinition` already verified these exact bytes against the
        // class as it is now (interpreter round i1 wave 28, lane L3): Step 5b
        // does not verify them again. Bytes a native `ClassFileLoadHook`
        // substituted, or a class redefined since, miss the note.
        let pre_verified = self.take_pre_verified_redefinition(class_id, &effective_new_bytes);

        // ---- Step 2: parse new bytes ----
        let new_class_file = Self::parse_redefinition_bytes(
            &existing_name,
            &effective_new_bytes,
            self.preview_enabled,
        )?;

        // ---- Steps 3 and 4: name match and structural equivalence ----
        Self::check_redefinition_structure(&shape, &new_class_file, &options)?;
        let existing_method_sigs = shape.method_sigs;

        // ---- Step 5: in-place swap ----
        // At this point every constraint has cleared. Take the new
        // class file apart and write its mutable parts onto the
        // existing Class. Hold the &mut borrow only as long as needed,
        // then drop it before firing hooks (which can re-enter the
        // loader).
        //
        // Caches keyed on this class's `(class, cp index)` pairs compare
        // their fill-time snapshot of this counter; advancing it BEFORE the
        // constant pool is replaced means no fill that straddles the swap
        // is kept (see `CLASS_REDEFINITIONS`). Advanced again after Step 9.
        record_redefinition(class_id.as_u32(), Some(&*self.redefinition_log));
        // A frame a lock-free door builds from here to the second advance may
        // run either body; the history records the window so the frame's
        // stamp is not taken for proof that it runs the old one (interpreter
        // round i1 wave 23, lane L3; `RedefinitionHistory::record_window`).
        let redefinition_began = class_redefinition_count();
        if options.log_diff {
            // Best-effort diagnostic — list any methods whose code
            // attribute bytes differ. Cheap because we already have
            // both the old and the new method vecs in scope.
            // (Using only the existing/new method indices; the new
            // methods will be moved in below so we capture before that.)
            if let Some(existing) = self.class_store.get(class_id) {
                for (i, m) in existing.methods.iter().enumerate() {
                    if i >= new_class_file.methods.len() {
                        break;
                    }
                    // `c.code: ByteView` — deref to `&[u8]` for the
                    // equality compare. Empty fallback is the empty
                    // slice with no allocation.
                    let old_code: &[u8] = m.code().map(|c| &c.code[..]).unwrap_or(&[]);
                    let new_code: &[u8] = new_class_file.methods[i]
                        .code()
                        .map(|c| &c.code[..])
                        .unwrap_or(&[]);
                    if old_code != new_code {
                        debug!(
                            class = %existing_name,
                            method = %m.name,
                            descriptor = %m.descriptor,
                            old_len = old_code.len(),
                            new_len = new_code.len(),
                            "WP2.4-B redefine: method body changed",
                        );
                    }
                }
            }
        }

        // Reorder the incoming method bodies to match the EXISTING method
        // order before the swap. The structural check (Step 4d) matches
        // methods by (name, descriptor) as a set and tolerates reordering,
        // but the live `Class.methods` vec is index-stable: vtable slots, the
        // JIT's per-method compilation records, and the per-thread resolution
        // caches all key off a method's position in this vec. Installing the
        // transformer's (possibly reordered) vec verbatim would silently remap
        // those indices and corrupt dispatch. Instead we rebuild the vec in
        // the existing order, taking each existing method's replacement body
        // by name+descriptor. `skip_structural_check` redefines (trusted
        // hidden/proxy classes) keep the verbatim order — they have no stable
        // vtable contract to honour.
        let new_methods = if options.skip_structural_check {
            new_class_file.methods
        } else {
            // Owned keys so `incoming` does not borrow `new_class_file.methods`
            // (we move that vec below). This is the cold JVMTI redefine path.
            let mut incoming: FxHashMap<(String, String), usize> = FxHashMap::default();
            for (idx, m) in new_class_file.methods.iter().enumerate() {
                incoming.insert((m.name.to_string(), m.descriptor.to_string()), idx);
            }
            // Resolve each existing method (in order) to its index in the
            // incoming vec. Step 4d already proved this is a bijection, so
            // every lookup succeeds; fall back defensively to identity order
            // if some invariant is violated rather than panicking.
            let order: Option<Vec<usize>> = existing_method_sigs
                .iter()
                .map(|(n, d, _)| incoming.get(&(n.clone(), d.clone())).copied())
                .collect();
            match order {
                Some(order) => {
                    // Permute `new_class_file.methods` into `order`. Move each
                    // method out exactly once via `Option::take`.
                    let mut slots: Vec<Option<_>> =
                        new_class_file.methods.into_iter().map(Some).collect();
                    order
                        .into_iter()
                        .map(|idx| slots[idx].take().expect("bijection guarantees one take"))
                        .collect()
                }
                None => new_class_file.methods,
            }
        };
        let new_constant_pool = new_class_file.constant_pool;
        let new_attributes = new_class_file.attributes;

        // JIT round 21 wave 17, lane innerclasses
        // (`CRATONVM_REDEFINE_SWAPS_INNER_CLASSES`, correctness, default ON,
        // `=0` keeps the old tables; page `r21-review-w16-findings-FIXED-20261020.md`
        // item 5). HotSpot's `VM_RedefineClasses::redefine_single_class` swaps
        // the scratch class's `inner_classes` array into the redefined class,
        // and that array carries the `EnclosingMethod` indices too. It never
        // refuses a changed `InnerClasses`: `compare_and_normalize_class_versions`
        // checks only `NestHost`, `NestMembers`, `Record` and
        // `PermittedSubclasses` (the `ClassAttributeChanged` text names exactly
        // those four). The scratch class is parsed by `ClassFileParser`, so the
        // define path's `InnerClasses` verdict applies under the same
        // `_need_verify` scope (every loader but the boot loader): the two
        // `ClassFormatError`s refuse the redefinition (JVMTI
        // `INVALID_CLASS_FORMAT`), and an ignored table installs as empty. A
        // hidden class is never redefinable on HotSpot; its trusted redefines
        // keep their (renamed) table.
        let new_inner_tables = self.redefinition_inner_tables(
            class_id,
            &existing_name,
            existing_loader,
            &new_attributes,
            &new_constant_pool,
            new_class_file.version.major,
        )?;

        // Recompute class-level annotations from the new attributes.
        // All entries are already decoded (see `force_decode_all` above),
        // so `as_decoded()` returns `Some` everywhere.
        let mut new_annotations = Vec::new();
        let mut new_type_annotations: Option<Vec<u8>> = None;
        for attr in &new_attributes {
            match attr.as_decoded() {
                // Reflection-visible only: skip RuntimeInvisibleAnnotations
                // (@Retention(CLASS)) — see gap-annotation-retention-policy.md.
                Some(Attribute::RuntimeVisibleAnnotations(anns)) => {
                    new_annotations.extend(anns.iter().cloned());
                }
                Some(a @ Attribute::RuntimeVisibleTypeAnnotations(_)) => {
                    if new_type_annotations.is_none() {
                        new_type_annotations =
                            cratonvm_reader::attribute::annotation_attribute_body(a);
                    }
                }
                _ => {}
            }
        }
        // SBR wave 5 (reflmeta): the redefinition's class-level type
        // annotations replace the old ones (`class_type_annotations`).
        match new_type_annotations {
            Some(body) => {
                self.class_type_annotations.insert(class_id, body.into());
            }
            None => {
                self.class_type_annotations.remove(&class_id);
            }
        }
        // Recompute bootstrap methods (invokedynamic call sites depend
        // on these; they may legitimately change with a body redefine).
        let new_bootstrap_methods = new_attributes
            .iter()
            .find_map(|a| match a.as_decoded() {
                Some(Attribute::BootstrapMethods(bms)) => Some(bms.clone()),
                _ => None,
            })
            .unwrap_or_default();
        // Source file may have changed if the compiler regenerated it.
        let new_source_file = new_attributes.iter().find_map(|a| match a.as_decoded() {
            Some(Attribute::SourceFile(name)) => Some(name.to_string()),
            _ => None,
        });

        // Obsolete methods (JEP 109: "active frames continue to run the
        // bytecodes of the original method"). Those bytecodes index the pool
        // being replaced, so the installed pool is the new one, index for
        // index, with every old constant it lacks appended; the translation
        // is recorded once the redefinition has succeeded (below) and the VM
        // rewrites a running old body through it
        // (`vm::runtime::interpreter::obsolete_frames`). A merge that does
        // not fit a `u2` pool count installs the new pool alone, as before
        // (interpreter round i1 wave 19, lane L3).
        // Old constants no frame the kept history can translate may name are
        // left out once they would bloat the pool (interpreter round i1 wave
        // 24, lane L3; `merge_for_obsolete_code_compacting`).
        let new_pool_len = new_constant_pool.len();
        let history = self.redefinition_histories.get(&class_id);
        let merged = self.class_store.get(class_id).and_then(|cls| {
            crate::obsolete_code::merge_for_obsolete_code_compacting(
                &cls.constant_pool,
                &cls.bootstrap_methods,
                &new_constant_pool,
                &new_bootstrap_methods,
                &crate::obsolete_code::ldc_operands_of(&cls.methods),
                history,
            )
        });
        // The line tables the redefinition replaces, for the frames still
        // running an old body (interpreter round i1 wave 21, lane L4).
        let replaced_lines = self
            .class_store
            .get(class_id)
            .map(|cls| crate::obsolete_code::replaced_line_tables(&cls.methods, &new_methods))
            .unwrap_or_default();
        let (new_constant_pool, new_bootstrap_methods, obsolete_translation) = match merged {
            Some(merged) => (
                merged.constant_pool,
                merged.bootstrap_methods,
                Some(merged.translation),
            ),
            None => (new_constant_pool, new_bootstrap_methods, None),
        };

        // The loader_id, id, name, superclass, interfaces, fields,
        // first_field_index, num_total_fields, hidden, module_name,
        // is_synthetic_stub, has_finalizer, code_source, nest_host,
        // nest_members, record_components, permitted_subclasses,
        // version, state, and
        // initializing_thread are all PRESERVED — JEP 109 forbids
        // changing any of them. (inner_classes and enclosing_method are
        // swapped after verification since JIT round 21 wave 17,
        // `CRATONVM_REDEFINE_SWAPS_INNER_CLASSES`.)
        //
        // Round 4 audit fix (CRIT): the swap is now performed as
        // snapshot-mutate-verify-rollback. Previously the redefine path
        // wrote `new_methods`/`new_constant_pool` straight onto the
        // live `Class` with **no call to `verifier::verify_class` or
        // `bytecode_verifier::verify_bytecode`** — so any malformed
        // bytecode delivered via JVMTI `RedefineClasses` /
        // `Instrumentation.redefineClasses` bypassed the verifier
        // entirely and crashed the interpreter mid-method. We now:
        //   1. snapshot the prior `methods` / `constant_pool` /
        //      `bootstrap_methods` / `annotations` / `source_file`,
        //   2. install the new ones in place,
        //   3. invoke `verifier::verify_class` (same Pass-2 + Pass-3
        //      entry point as `define_class_with_options`), and
        //   4. on failure, restore the snapshot and return the
        //      existing `UnsupportedClassRedefinitionError` variant.
        //
        // The structural-equivalence check above already guarantees
        // identical fields/methods/super/interfaces, so the verifier
        // never sees a mismatch between the live `Class`'s shape and
        // the bytecode it is verifying — only the bodies + CP have
        // changed, which is exactly what we want to verify.
        //
        // Skip cases mirror `define_class_with_options`:
        //   - synthetic stubs have no bytecode to verify (and the
        //     redefine path shouldn't hit them in practice, but be safe);
        //   - hidden classes flagged `skip_bytecode_verification` at
        //     define time keep the bypass on redefine (JVMTI agents
        //     transforming a hidden class shouldn't suddenly trip the
        //     verifier the original define skipped).
        let verify_skip = self
            .class_store
            .get(class_id)
            .map(|c| c.origin.is_compatibility_stub())
            .unwrap_or(false)
            || self.skip_bytecode_verification.contains(&class_id)
            || pre_verified;
        if pre_verified {
            debug!(
                class = %existing_name,
                "WP2.4-B redefine: verified by its check; not verified again",
            );
        }

        // Round 5 audit fix (HIGH): in debug builds, snapshot every
        // invariant field of `Class` so that a `debug_assert!` block
        // after the swap can verify the redefine touched ONLY the
        // documented mutable set (`methods`, `constant_pool`,
        // `bootstrap_methods`, `annotations`, `source_file`). If any
        // other field changes — e.g. a future commit accidentally
        // mutates `superclass` from this path — the assertion fires
        // and aborts, preventing silent type-confusion bugs from
        // shipping to release builds.
        //
        // The destructuring pattern in `RedefineInvariantSnapshot::from_class`
        // explicitly OMITS the trailing `..` so adding a new field to
        // `Class` is a hard compile error there until the author
        // classifies it as either an invariant (add to the snapshot
        // struct + the `assert_eq` walk) or a mutable redefine field
        // (add to the rollback tuple + the documented set + the
        // post-swap mutation block).  This trip-wire fires in release
        // builds because `from_class` is always compiled (only the
        // runtime `assert_eq` walk is debug-gated below).
        //
        // Round 7 audit fix (CRIT #1): always build the snapshot so the
        // exhaustive destructuring trip-wire fires in release builds
        // too. The actual `assert_eq` runtime check below remains gated
        // behind `cfg(debug_assertions)`.
        let invariant_snapshot: Option<RedefineInvariantSnapshot> = self
            .class_store
            .get(class_id)
            .map(RedefineInvariantSnapshot::from_class);

        // Snapshot + swap. The snapshot is only retained for rollback
        // when we're going to run the verifier; otherwise we drop the
        // old vecs immediately.
        let rollback_snapshot = {
            let cls = self.class_store.get_mut(class_id).ok_or_else(|| {
                RedefinitionRefused::new(
                    RedefinitionRefusal::InvalidClass,
                    existing_name.clone(),
                    "class id vanished during redefine (race)",
                )
            })?;
            // Using `std::mem::take` for the vecs (cheap; leaves an
            // empty Vec behind that we immediately overwrite) and
            // `clone` for source_file (Option<String>, allocation-light).
            let snapshot = if verify_skip {
                None
            } else {
                Some((
                    std::mem::take(&mut cls.methods),
                    std::mem::replace(
                        &mut cls.constant_pool,
                        ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
                    ),
                    std::mem::take(&mut cls.bootstrap_methods),
                    std::mem::take(&mut cls.annotations),
                    cls.source_file.clone(),
                ))
            };
            // Install the new bodies / CP / metadata. The Class's shape
            // (super, interfaces, fields, declared methods sigs) is
            // unchanged thanks to the structural-equivalence check above.
            cls.methods = new_methods;
            cls.constant_pool = new_constant_pool;
            cls.bootstrap_methods = new_bootstrap_methods;
            cls.annotations = new_annotations;
            // HotSpot replaces the attribute even when it is absent in the
            // new version. Keeping the old name here also prevents trace
            // decoding from clearing the Class mirror's source-file cache.
            cls.source_file = new_source_file;
            snapshot
        };

        // Round 5 audit fix (HIGH): verify that ONLY the documented
        // mutable fields changed. The mutable set is:
        //   methods, constant_pool, bootstrap_methods, annotations, source_file
        // Everything else is a JEP 109 invariant and a mutation here
        // would silently corrupt downstream caches (vtable, JIT,
        // resolution cache) that key off identity-stable fields.
        //
        // Round 7 audit fix (CRIT #1): `invariant_snapshot` is always
        // built (above) so the destructuring trip-wire fires in release.
        // Only the runtime `assert_eq` walk is debug-gated here.
        #[cfg(debug_assertions)]
        if let (Some(before), Some(after)) = (
            invariant_snapshot.as_ref(),
            self.class_store
                .get(class_id)
                .map(RedefineInvariantSnapshot::from_class),
        ) {
            before.assert_eq(&after, &existing_name);
        }
        // In release builds the snapshot is built for its trip-wire
        // side-effect (compile-time enumeration of every Class field)
        // but otherwise unused. Suppress the dead-store warning.
        #[cfg(not(debug_assertions))]
        let _ = invariant_snapshot;

        // ---- Step 5b: verify the freshly-installed bytecode ----
        // Run the same Pass-2 (structural) + Pass-3 (bytecode type
        // checking) verifier that `define_class_with_options` runs at
        // class-define time. The verifier consults the live class store
        // through `ClassStoreHierarchy` so super/interface lookups walk
        // the already-loaded class graph (which is unchanged by this
        // redefine).
        if let Some((
            old_methods,
            old_constant_pool,
            old_bootstrap_methods,
            old_annotations,
            old_source_file,
        )) = rollback_snapshot
        {
            // Run the verifier in an inner scope so the immutable
            // borrows it takes on `self.class_store` and
            // `self.loaded_classes` drop before we reach the rollback
            // path that needs `self.class_store.get_mut(...)`.
            let verify_result: Result<(), LinkageError> = {
                let hierarchy = ClassStoreHierarchy {
                    class_store: &self.class_store,
                    loaded_classes: &self.loaded_classes,
                    user_loader_parents: &self.user_loader_parents,
                    // Redefine verifies a class already resident in the
                    // store (in-place mutation), so no in-flight class.
                    in_flight: None,
                    in_flight_super_name: None,
                    // CL-CLASSMANAGER fix: the redefined class is resident
                    // at `class_id`; resolve referenced names through its
                    // own defining loader's delegation order so two loaders
                    // defining the same name do not cross-resolve.
                    requesting_loader: self.class_store.get(class_id).map(|c| c.loader_id),
                    verifier_loads: None,
                    app_workarounds: &self.app_workarounds,
                };
                // SAFETY-shape: the class we just mutated is at
                // `class_id` in `self.class_store`. `get` returns `Some`
                // here because the mutate block above succeeded and the
                // store is not concurrently modified (we hold
                // `&mut self`).
                match self.class_store.get(class_id) {
                    // The policy `check_redefinition` applies too
                    // (`verify_redefined_class`).
                    Some(cls) => verify_redefined_class(cls, &self.class_store, &hierarchy),
                    None => Err(LinkageError::VerifyError {
                        class_name: existing_name.clone(),
                        method_name: String::new(),
                        message: "class id vanished during redefine verification".to_string(),
                    }),
                }
            };
            if let Err(verify_err) = verify_result {
                // Roll back: restore the snapshot so the live Class
                // returns to its pre-redefine state. The vtable
                // re-install, generation bump, JIT invalidation, and
                // resolution-cache invalidation all happen below this
                // point, so no observable side effect needs to be
                // unwound here.
                if let Some(cls) = self.class_store.get_mut(class_id) {
                    cls.methods = old_methods;
                    cls.constant_pool = old_constant_pool;
                    cls.bootstrap_methods = old_bootstrap_methods;
                    cls.annotations = old_annotations;
                    cls.source_file = old_source_file;
                }
                debug!(
                    class = %existing_name,
                    error = ?verify_err,
                    "WP2.4-B redefine: bytecode verification failed; rolled back",
                );
                // The rejected bytes are the ONLY copy of what the agent
                // produced — the rollback below throws them away, and the
                // transformer is not deterministic enough to re-derive them by
                // hand. `CRATONVM_DBG_REDEFINE_DUMP=<dir>` keeps them so a
                // rejection can be disassembled instead of guessed at.
                dump_rejected_redefine_bytes(&existing_name, &effective_new_bytes);
                return Err(RedefinitionRefused::new(
                    RedefinitionRefusal::FailsVerification,
                    existing_name.clone(),
                    format!("new bytes failed bytecode verification: {verify_err}"),
                ));
            }
        }
        // Verification succeeded (or was legitimately skipped). The
        // snapshot vecs (if any) are dropped here; the new ones remain
        // installed.

        // JIT round 21 wave 17, lane innerclasses
        // (`CRATONVM_REDEFINE_SWAPS_INNER_CLASSES`): the new `InnerClasses`
        // and `EnclosingMethod` tables, installed once the redefinition can
        // no longer be refused, so the rollback above never has to restore
        // them. The VM then drops the natives' answers built from the old
        // tables (`lang_class::forget_redefined_inner_class_answers`).
        if let Some((inner_classes, enclosing_method)) = new_inner_tables {
            if let Some(cls) = self.class_store.get_mut(class_id) {
                cls.inner_classes = inner_classes;
                cls.enclosing_method = enclosing_method.map(Box::new);
            }
        }

        // ---- Step 5c: re-publish this class's type maps ----
        // The maps published when this class was DEFINED describe the OLD
        // method bodies, at `Class::methods` indices this redefine may have
        // reshuffled. `type_maps::publish_class_type_maps` is first-writer-
        // wins, so the verify above (when it ran) could not overwrite them —
        // the class's bytecode and its oop maps would silently diverge, and a
        // stale oop map is a wrong oop map, i.e. heap corruption rather than a
        // pessimisation. `refresh_class_type_maps` is the *replacing* install
        // and is what keeps the two in lockstep.
        //
        // Unconditional on a successful redefine, including the paths that
        // skipped verification (trusted generated bytecode, `--noverify`):
        // there, replacing stale maps with an honest "walk did not complete"
        // set is still strictly better than leaving the old class's rows
        // visible.
        {
            let hierarchy = ClassStoreHierarchy {
                class_store: &self.class_store,
                loaded_classes: &self.loaded_classes,
                user_loader_parents: &self.user_loader_parents,
                in_flight: None,
                in_flight_super_name: None,
                requesting_loader: self.class_store.get(class_id).map(|c| c.loader_id),
                verifier_loads: None,
                app_workarounds: &self.app_workarounds,
            };
            if let Some(cls) = self.class_store.get(class_id) {
                crate::verifier::refresh_class_type_maps(cls, &hierarchy);
            }
        }

        // Forget the borrow — the rest of this function is hooks +
        // bookkeeping that may re-enter the manager.
        let _ = existing_loader; // silence unused warning if no read below

        // Update the cached class bytes so subsequent
        // `getResourceAsStream` lookups + later redefines see the new
        // bytes as their "old bytes".  Route through the FIFO helper:
        // a redefine bumps the entry to the most-recently-used end of
        // the deque so it stays in cache.
        //
        // SKIPPED for `retransformClasses` (`preserve_original_bytes`): the
        // cached bytes are the retransform BASE — each retransform must re-run
        // the transformer chain from the ORIGINAL bytes, not the previously
        // woven ones, or a second retransform (e.g. mockStatic+mock of the
        // same class) double-instruments it. Matches HotSpot, which keeps the
        // original `cached_class_file` across retransformations.
        if options.preserve_original_bytes {
            // Touch `effective_new_bytes` so the move-out below is balanced and
            // no unused-variable lint fires when the cache update is skipped.
            let _ = &effective_new_bytes;
        } else {
            // Pinned: a redefinition's class file is not on the class path, so
            // an eviction would make the class un-retransformable (interpreter
            // round i1 wave 28, lane L3; `class_bytes_pinned`).
            self.insert_class_bytes_pinned(class_id, effective_new_bytes);
        }

        // ---- Step 6: rebuild + re-install vtable descriptor snapshots ----
        //
        // The vtable LAYOUT is unchanged (same method count, same
        // order), but each slot's `dispatch` snapshot was built from
        // the OLD code attributes. Rebuild the descriptor list from
        // the current Class state and re-fire the install hook so the
        // VM's VtableManager picks up the new code/exception tables.
        //
        // We deliberately do NOT recompute the override list here:
        // the inheritance shape didn't change, so no super-class slots
        // were freshly overridden by the redefine.
        let class_super_id = self.class_store.get(class_id).and_then(|c| c.superclass);
        let new_entries = self.build_vtable_descriptors(class_id, class_super_id);
        self.vtable_descriptors
            .insert(class_id, new_entries.clone());
        // A subclass's descriptor vec is seeded by COPYING its superclass's
        // (`build_vtable_descriptors_with_overrides`'s `extend_from_slice`), so
        // replacing only this class's own vec leaves every already-linked
        // descendant holding pre-redefinition snapshots. The VM-side
        // `VtableManager::install_vtable` sweeps its own tables for exactly
        // those inherited entries, but nothing refreshed the class-loader-side
        // copies that the NEXT `build_vtable_descriptors` reads from — so a
        // descendant redefined later in the same `retransformClasses` batch
        // rebuilt from the stale parent copy and re-installed the ORIGINAL
        // body, silently undoing the sweep. See
        // `refresh_inherited_vtable_descriptors`.
        self.refresh_inherited_vtable_descriptors(class_id, &new_entries);
        // The VM-scoped dispatcher, like the link-time install: the legacy
        // `fn(u32, ..)` hook names no VM, so a redefinition in one VM of a
        // multi-VM process re-installed into whichever manager that hook
        // reaches (interpreter round i1 wave 19).
        fire_vtable_install_hook_for(&self.metadata_realm, class_id, new_entries);

        // ---- Step 7: bump generation counter ----
        // WP2.4-F1: use the shared handle accessor so the per-thread
        // invoke-cache (which holds an `Arc<AtomicU32>` clone of the
        // *same* counter from populate time) sees this bump on its next
        // hit and auto-evicts the stale entry.
        let counter = self.class_redefine_generation_handle(class_id);
        // `Release` ordering: any reader that sees the new generation
        // is guaranteed to also see the new methods we wrote above
        // (strictly we already serialized the writes via &mut, but
        // the explicit Release pairs cleanly with cross-thread
        // Acquire reads of the counter).
        let new_gen = counter.fetch_add(1, Ordering::Release) + 1;
        // C2 review P1: bump the metadata realm's generation in the same step.
        // Every `ClassHandle` / `MethodHandle` / `FieldHandle` minted against
        // the pre-redefine class is now stale, and must fail with a named
        // `StaleMetadata::StaleGeneration` rather than resolving into the
        // freshly-replaced `methods` vec at an index that means something else.
        // Separate from the counter above on purpose — see
        // `class_metadata_generation` for why the two are not yet unified.
        self.metadata_realm.bump_generation(class_id);
        // Arm the global fast-path flag so the interpreter's native/intrinsic
        // shadowing guards start consulting per-class redefine generations.
        ANY_CLASS_REDEFINED.store(true, Ordering::Release);

        // ---- Step 8: invalidate JIT caches keyed on class_id ----
        fire_jit_invalidate_hook(class_id_u32, self.class_store.layout_domain());

        // ---- Step 9: invalidate the shared ResolutionCache ----
        // Round 4 audit fix (CRIT): the per-VM `ResolutionCache` caches
        // resolved fields/methods/call-sites/condy values keyed by
        // (referring-class, cp-index). The InvokeCache already auto-
        // evicts via the RedefineGate generation bumped in step 7, but
        // ResolutionCache has no gate — so without this hook every
        // cached cp-index from the redefined class continues returning
        // the resolution made against the OLD constant pool. The
        // VM-side adapter (installed at SharedVm::new) takes the
        // resolution_cache write lock and drops every entry whose key
        // refers to this class OR whose resolved declaring class IS
        // this class. See `ResolutionCache::invalidate_class`.
        fire_resolution_invalidate_hook(
            class_id_u32,
            self.class_store.layout_domain(),
            ResolutionInvalidation::ConstantPoolReplaced,
        );
        // After the sweep: a cache that filled from an unswept row between
        // the two advances holds a snapshot the counter has now left.
        record_redefinition(class_id.as_u32(), Some(&*self.redefinition_log));

        // The obsolete-method translation (see the merge above), stamped with
        // the count every frame built before this redefinition completed
        // reads below. Recorded last, after verification succeeded: a
        // rolled-back redefinition leaves no step behind.
        self.redefinition_histories
            .entry(class_id)
            .or_default()
            .record_window(
                redefinition_began,
                class_redefinition_count(),
                obsolete_translation,
                replaced_lines,
                Some(new_pool_len),
            );

        debug!(
            class = %existing_name,
            class_id = class_id_u32,
            generation = new_gen,
            "WP2.4-B class redefined in place",
        );

        Ok(())
    }

    /// SBR5 lane jfr: is `superclass_id` `jdk.internal.event.Event` or one of
    /// its subclasses? Walks the loaded superclass chain; the depth cap only
    /// guards a corrupt (cyclic) chain.
    pub(in crate::class_manager) fn superclass_chain_reaches_jfr_event(&self, superclass_id: ClassId) -> bool {
        let mut current = Some(superclass_id);
        let mut depth = 0u32;
        while let Some(id) = current {
            let Some(class) = self.class_store.get(id) else {
                return false;
            };
            if &*class.name == "jdk/internal/event/Event" {
                return true;
            }
            current = class.superclass;
            depth += 1;
            if depth > 64 {
                return false;            }        }
        false
    }

    /// SBR5 lane jfr: give a concrete `jdk.internal.event.Event` subclass the
    /// members HotSpot's `JfrEventClassTransformer` adds at class definition
    /// (see [`crate::jfr_event_schema`]). Returns the extended class bytes
    /// (to be cached as the class's bytes) and rewrites `class_file`'s field
    /// and method tables to match; `None` leaves everything untouched, which is
    /// every class that is not such an event class.
    pub(in crate::class_manager) fn extend_jfr_event_schema(
        &self,
        superclass_id: Option<ClassId>,
        loader_id: ClassLoaderId,
        bytes: &SharedBytes,
        class_file: &mut cratonvm_reader::ClassFile,
    ) -> Option<SharedBytes> {
        let superclass_id = superclass_id?;
        if class_file
            .access_flags
            .intersects(ClassAccessFlags::INTERFACE | ClassAccessFlags::ABSTRACT)
        {
            return None;
        }
        if !self.superclass_chain_reaches_jfr_event(superclass_id) {
            return None;
        }
        // JDK 17/21 keep the JFR "mirror" events in `jdk.jfr.events` (module
        // jdk.jfr, the boot loader); their instrumentation names the typed
        // `EventConfiguration` field. Only a java.base event (`jdk/internal/event`)
        // cannot name a jdk.jfr type and takes the `Object`-typed field.
        let untyped = loader_id == ClassLoaderId::Bootstrap
            && !self.jfr_typed_event_configuration
            && !class_file.this_class.starts_with("jdk/jfr/");
        let extended = crate::jfr_event_schema::extend_event_class(bytes.as_ref(), untyped)?;
        let extended = SharedBytes::from(extended);
        let mut parsed = cratonvm_reader::read_class_shared(extended.clone()).ok()?;
        force_decode_all(&mut parsed.attributes, &parsed.constant_pool).ok()?;
        for method in parsed.methods.iter_mut() {
            force_decode_all(&mut method.attributes, &parsed.constant_pool).ok()?;
        }
        for field in parsed.fields.iter_mut() {
            force_decode_all(&mut field.attributes, &parsed.constant_pool).ok()?;
        }
        class_file.constant_pool = parsed.constant_pool;
        class_file.fields = parsed.fields;
        class_file.methods = parsed.methods;
        Some(extended)
    }

    /// List all class names available on the application classpath.
    ///
    /// Returns binary class names (e.g. `com/example/MyClass`).
    pub fn list_application_class_names(&self) -> Vec<String> {
        self.application.class_path().list_class_names()
    }

    /// Insert raw class bytes into [`Self::class_bytes_cache`], evicting
    /// older entries (FIFO) once the cumulative byte budget exceeds
    /// [`Self::class_bytes_cache_cap`].
    ///
    /// Re-inserts of the same `ClassId` (e.g. JVMTI redefine) update the
    /// size accounting and move the entry to the tail of the FIFO so
    /// it is the *last* candidate for eviction — agents that redefine
    /// hot classes keep them in cache.
    pub fn insert_class_bytes(&mut self, class_id: ClassId, bytes: impl Into<SharedBytes>) {
        let bytes = bytes.into();
        let new_size = bytes.len();
        // Record the retransformation base's fingerprint BEFORE the FIFO can
        // evict the bytes themselves. This map is never evicted; see the field
        // docs on `class_bytes_base_digest`.
        self.class_bytes_base_digest
            .insert(class_id, class_bytes_digest(&bytes));
        // If we already had an entry for this class, subtract its size
        // and remove it from the FIFO (or the pinned set) before re-appending.
        self.forget_cached_class_bytes(class_id);
        self.class_bytes_cache_size = self.class_bytes_cache_size.saturating_add(new_size);
        self.class_bytes_cache.insert(class_id, bytes);
        self.class_bytes_cache_fifo.push_back(class_id);

        // Evict oldest entries until we fit under the cap. We always
        // keep at least the most-recently-inserted entry, so the cap
        // is *soft* — a single class larger than the cap stays cached.
        while self.class_bytes_cache_size > self.class_bytes_cache_cap
            && self.class_bytes_cache_fifo.len() > 1
        {
            let victim = match self.class_bytes_cache_fifo.pop_front() {
                Some(v) => v,
                None => break,
            };
            if let Some(b) = self.class_bytes_cache.remove(&victim) {
                self.class_bytes_cache_size = self.class_bytes_cache_size.saturating_sub(b.len());
            }
        }
    }

    /// Set the soft byte cap for [`Self::class_bytes_cache`]. Triggers
    /// immediate FIFO eviction if the new cap is smaller than the
    /// current cache size. Use `usize::MAX` to disable eviction.
    pub fn set_class_bytes_cache_cap(&mut self, cap: usize) {
        self.class_bytes_cache_cap = cap;
        while self.class_bytes_cache_size > self.class_bytes_cache_cap
            && self.class_bytes_cache_fifo.len() > 1
        {
            let victim = match self.class_bytes_cache_fifo.pop_front() {
                Some(v) => v,
                None => break,
            };
            if let Some(b) = self.class_bytes_cache.remove(&victim) {
                self.class_bytes_cache_size = self.class_bytes_cache_size.saturating_sub(b.len());
            }
        }
    }

    /// Record `bytes` as `class_id`'s retransformation base where the FIFO
    /// cannot evict it: a class file the class path cannot give back (one a
    /// load-time transformer changed, or one `redefineClasses` installed). See
    /// [`Self::class_bytes_pinned`]. Replaces any entry the class had, pinned
    /// or not, and moves the fingerprint with it as
    /// [`Self::insert_class_bytes`] does.
    ///
    /// Bounded per VM by [`Self::class_bytes_pinned_cap`]: an insert that
    /// would pass it goes to the FIFO instead (evictable, as every base was
    /// before interpreter round i1 wave 28), with one warning per VM, so a
    /// later `retransformClasses` that finds the base evicted is explained.
    pub fn insert_class_bytes_pinned(&mut self, class_id: ClassId, bytes: impl Into<SharedBytes>) {
        let bytes = bytes.into();
        let len = bytes.len();
        let held_now = if self.class_bytes_pinned.contains(&class_id) {
            self.class_bytes_cache.get(&class_id).map_or(0, |b| b.len())
        } else {
            0
        };
        let pinned_after = self
            .class_bytes_pinned_size
            .saturating_sub(held_now)
            .saturating_add(len);
        if pinned_after > self.class_bytes_pinned_cap {
            if !self.class_bytes_pinned_overflowed {
                self.class_bytes_pinned_overflowed = true;
                tracing::warn!(
                    "class-bytes cache: the pinned retransformation bases passed their \
                     {} byte cap; further bases (class files an agent transformed or \
                     redefined) are evictable, and `retransformClasses` skips a class \
                     whose base was evicted",
                    self.class_bytes_pinned_cap
                );
            }
            self.insert_class_bytes(class_id, bytes);
            return;
        }
        self.class_bytes_base_digest
            .insert(class_id, class_bytes_digest(&bytes));
        self.forget_cached_class_bytes(class_id);
        self.class_bytes_pinned_size = self.class_bytes_pinned_size.saturating_add(len);
        self.class_bytes_cache.insert(class_id, bytes);
        self.class_bytes_pinned.insert(class_id);
    }

    /// Drop `class_id`'s [`Self::class_bytes_cache`] entry with its
    /// accounting: out of the pinned set, or out of the FIFO (a linear scan —
    /// the deque is small relative to total bytes, and a re-insert is rare
    /// next to a first load). The fingerprint stays.
    pub(in crate::class_manager) fn forget_cached_class_bytes(&mut self, class_id: ClassId) {
        self.class_bytes_raw_agent_bases.remove(&class_id);
        let Some(prev) = self.class_bytes_cache.remove(&class_id) else {
            return;
        };
        if self.class_bytes_pinned.remove(&class_id) {
            self.class_bytes_pinned_size = self.class_bytes_pinned_size.saturating_sub(prev.len());
            return;
        }
        self.class_bytes_cache_size = self.class_bytes_cache_size.saturating_sub(prev.len());
        if let Some(pos) = self
            .class_bytes_cache_fifo
            .iter()
            .position(|id| *id == class_id)
        {
            self.class_bytes_cache_fifo.remove(pos);
        }
    }

    /// zr z11/md (`CRATONVM_BOOT_CLASS_BYTES_REFETCH`, default ON): drop the
    /// cache entry [`Self::insert_class_bytes`] just made for a class the boot
    /// loader defined from `found`, the bytes its own class path served.
    /// The fingerprint stays, so [`Self::defined_class_bytes`] can give the
    /// file back by re-reading it. Only an unpinned entry that IS `found`
    /// goes (a define that installed other bytes -- a transformer's -- keeps
    /// its entry). The entry is the FIFO's newest, so this is O(1).
    pub(in crate::class_manager) fn release_refetchable_class_bytes(&mut self, class_id: ClassId, found: &SharedBytes) {
        if self.class_bytes_pinned.contains(&class_id) {
            return;
        }
        let same = self.class_bytes_cache.get(&class_id).is_some_and(|cached| {
            cached.as_ref().as_ptr() == found.as_ref().as_ptr() && cached.len() == found.len()
        });
        if !same {
            return;
        }
        if let Some(prev) = self.class_bytes_cache.remove(&class_id) {
            self.class_bytes_cache_size = self.class_bytes_cache_size.saturating_sub(prev.len());
        }
        if self.class_bytes_cache_fifo.back() == Some(&class_id) {
            self.class_bytes_cache_fifo.pop_back();
        } else if let Some(pos) = self
            .class_bytes_cache_fifo
            .iter()
            .rposition(|id| *id == class_id)
        {
            self.class_bytes_cache_fifo.remove(pos);
        }
        self.class_bytes_refetched_released = self.class_bytes_refetched_released.saturating_add(1);
    }

    /// The class file `class_id`'s current definition came from, for the
    /// readers that need it after the define (JVMTI / `retransformClasses`
    /// bases, `Class.getAnnotatedSuperclass`, a defined class's own `.class`
    /// resource, the CDS dump, the debugger). The class-bytes cache when it
    /// holds the file; otherwise a re-read through the delegation chain
    /// (bootstrap -> extension -> application) that answers only when the
    /// bytes fingerprint-match what the class was defined from
    /// ([`Self::class_bytes_match_base`]). `None` when neither has it.
    ///
    /// Before zr z11/md every reader read the cache alone, so a class whose
    /// entry the 16 MiB FIFO evicted silently lost its answer; the re-read is
    /// what lets `CRATONVM_BOOT_CLASS_BYTES_REFETCH` keep no copy of a boot
    /// image class's file at all.
    pub fn defined_class_bytes(&self, class_id: ClassId) -> Option<SharedBytes> {
        if let Some(cached) = self.class_bytes_cache.get(&class_id) {
            return Some(cached.clone());
        }
        let class = self.class_store.get(class_id)?;
        if class.hidden || class.name.starts_with('[') || class.origin.is_compatibility_stub() {
            return None;
        }
        let (bytes, _) = self.find_class_bytes_delegated(&class.name).ok()?;
        (self.class_bytes_match_base(class_id, &bytes) == Some(true)).then_some(bytes)
    }

    /// How many class files [`Self::release_refetchable_class_bytes`] let go
    /// (zr z11/md; the positive control of `CRATONVM_BOOT_CLASS_BYTES_REFETCH`).
    pub fn class_bytes_refetch_released(&self) -> u64 {
        self.class_bytes_refetched_released
    }

    /// JVMTI saved bytes from before a capable transformer changed the file.
    pub fn insert_raw_agent_base(&mut self, id: ClassId, bytes: impl Into<SharedBytes>) {
        self.insert_class_bytes_pinned(id, bytes);
        self.class_bytes_raw_agent_bases.insert(id);
    }

    pub fn has_raw_agent_base(&self, id: ClassId) -> bool {
        self.class_bytes_raw_agent_bases.contains(&id)
    }

    /// Is this class's cached file pinned against eviction?
    pub fn is_class_bytes_pinned(&self, class_id: ClassId) -> bool {
        self.class_bytes_pinned.contains(&class_id)
    }

    /// Has an insert passed [`Self::class_bytes_pinned_cap`] (so a base that
    /// should be pinned may sit in the FIFO)? The VM then reconstitutes no
    /// class file from a live class (interpreter round i1 wave 29, lane L3;
    /// `vm/src/runtime/instrument_reconstitute.rs`).
    pub fn class_bytes_pinned_overflowed(&self) -> bool {
        self.class_bytes_pinned_overflowed
    }

    /// Bytes held by the pinned entries of [`Self::class_bytes_cache`].
    pub fn class_bytes_pinned_size(&self) -> usize {
        self.class_bytes_pinned_size
    }

    /// Set the bound on the pinned retransformation bases. Entries already
    /// pinned stay; only later pinned inserts are held to the new bound.
    pub fn set_class_bytes_pinned_cap(&mut self, cap: usize) {
        self.class_bytes_pinned_cap = cap;
    }

    /// Do `bytes` fingerprint-match the class file `class_id`'s current
    /// definition came from?
    ///
    /// `None` means no fingerprint was recorded (a synthetic stub, or a class
    /// defined before this bookkeeping existed) and the caller must decide for
    /// itself. `Some(false)` means the caller is holding a **different class
    /// file that happens to carry the same name** — see the field docs on
    /// [`Self::class_bytes_base_digest`] for how a retransform reaches that
    /// state and why installing the result would be a silently wrong class
    /// definition.
    pub fn class_bytes_match_base(&self, class_id: ClassId, bytes: &[u8]) -> Option<bool> {
        self.class_bytes_base_digest
            .get(&class_id)
            .map(|recorded| *recorded == class_bytes_digest(bytes))
    }

    /// Current total bytes held by [`Self::class_bytes_cache`].
    pub fn class_bytes_cache_size(&self) -> usize {
        self.class_bytes_cache_size
    }

    /// Current soft cap for [`Self::class_bytes_cache`].
    pub fn class_bytes_cache_cap(&self) -> usize {
        self.class_bytes_cache_cap
    }

    /// Bytes of class metadata currently charged against
    /// `-XX:MaxMetaspaceSize` (gc-common w6-f,
    /// `docs/feature-designs/class-resolution-and-caching.md`).
    ///
    /// What is charged, and why it is not the heap summary's `metaspace_used`:
    ///
    /// * the CLASS-FILE length of every class defined from real bytes (a
    ///   define, or a stub upgraded by one) — O(1) at define time, where the
    ///   heap summary's measured figure is a walk of the whole store;
    /// * by every loader EXCEPT the bootstrap loader. HotSpot maps the JDK's
    ///   own classes from the CDS archive, outside the space
    ///   `MaxMetaspaceSize` bounds; charging them here would make a limit that
    ///   fits on HotSpot fail here first;
    /// * given back, exactly, when the class is unloaded.
    ///
    /// Both choices make the figure a conservative UNDER-estimate of
    /// HotSpot's metaspace use (a class's metadata is typically larger than
    /// its class file there), so a `-XX:MaxMetaspaceSize` that a build passes
    /// routinely and HotSpot never hits is not hit here either; a leak still
    /// reaches it, only a little later.
    pub fn metaspace_charged_bytes(&self) -> u64 {
        self.metaspace_charged_bytes
    }

    /// `-XX:MaxMetaspaceSize`, or `None` when unbounded (the default).
    pub fn max_metaspace_bytes(&self) -> Option<u64> {
        self.max_metaspace_bytes
    }

    /// Install (or with `None`, remove) this VM's `-XX:MaxMetaspaceSize`.
    /// Enforced by the VM's define path, which collects before it refuses
    /// (`vm_exec.rs::define_class_full`); nothing here refuses a define.
    pub fn set_max_metaspace_bytes(&mut self, limit: Option<u64>) {
        self.max_metaspace_bytes = limit;
    }

    /// gci i33/ms: the VM's running [`MetaspaceArenaTally`] for the
    /// `-XX:MaxMetaspaceSize` check (the VM fills it; nothing here reads it).
    pub fn metaspace_arena_tally(&self) -> &parking_lot::Mutex<MetaspaceArenaTally> {
        &self.metaspace_arena_tally
    }

    /// Would charging `additional` more bytes exceed `-XX:MaxMetaspaceSize`?
    /// Always `false` without a limit.
    pub fn metaspace_would_exceed(&self, additional: u64) -> bool {
        metaspace_limit_exceeded(
            self.max_metaspace_bytes,
            self.metaspace_charged_bytes,
            additional,
        )
    }

    /// Charge `bytes` for class `id` just defined from real bytes, replacing
    /// any earlier charge for the same id. A bootstrap-defined class is
    /// charged nothing (see [`Self::metaspace_charged_bytes`]).
    pub(in crate::class_manager) fn charge_metaspace(&mut self, id: ClassId, bytes: u64) {
        let charge = match self.class_store.get(id).map(|c| c.loader_id) {
            Some(ClassLoaderId::Bootstrap) | None => 0,
            Some(_) => bytes,
        };
        let prior = if charge == 0 {
            self.metaspace_charges.remove(&id)
        } else {
            self.metaspace_charges.insert(id, charge)
        };
        self.metaspace_charged_bytes = self
            .metaspace_charged_bytes
            .saturating_sub(prior.unwrap_or(0))
            .saturating_add(charge);
    }

    /// Dynamically extend the application classpath at runtime.
    ///
    /// Called by `URLClassLoader` when new URLs are registered. Each path is
    /// added as a directory or JAR entry to the application class finder.
    pub fn extend_application_classpath(&mut self, paths: &[String]) {
        for path in paths {
            self.application.add_path(path);
        }
        // A new classpath entry may now contain a class previously memoized
        // as absent — re-arm the synthetic-stub upgrade scan. See
        // [`Self::synthetic_upgrade_absent`].
        if !paths.is_empty() {
            self.synthetic_upgrade_absent.clear();
        }
    }

    /// Retract paths previously added by [`Self::extend_application_classpath`].
    ///
    /// Backs `URLClassLoader.close()`: a closed loader must stop serving classes
    /// and resources it has not already loaded. Classes already defined stay
    /// defined — HotSpot's `close()` shuts the loader's `URLClassPath`, it does
    /// not unload anything — so only the search roots go away.
    ///
    /// Returns the number of classpath entries actually removed; see
    /// [`ClassPath::remove_path`] for why that can legitimately be zero.
    pub fn retract_application_classpath(&mut self, paths: &[String]) -> usize {
        let mut removed = 0;
        for path in paths {
            removed += self.application.remove_path(path);
        }
        if removed > 0 {
            // A name that resolved only through a retracted root must now be
            // re-resolved (and can legitimately come back absent).
            self.synthetic_upgrade_absent.clear();
        }
        removed
    }

    /// Append paths to the BOOTSTRAP class search path so the classes they
    /// contain are loaded by the bootstrap loader (`ClassLoaderId::Bootstrap`,
    /// i.e. a `null` `Class.getClassLoader()`). Drives
    /// `Instrumentation.appendToBootstrapClassLoaderSearch`: Mockito's inline
    /// mock maker injects `MockMethodDispatcher` here and then asserts it is
    /// loaded by the bootstrap loader (so redefined JDK classes can reach it).
    pub fn extend_bootstrap_classpath(&mut self, paths: &[String]) {
        // Round 15 wave 5 (lane proxy8): file the packages of the appended
        // classes that no named module claims on the store, where §5.4.5
        // overriding tells them apart from a same-named class-path package
        // (`ClassStore::is_split_boot_package`). A `java/*` package is never
        // filed: no application-loader class can define one on HotSpot, and
        // the only ones CratonVM records `Application` there are the built-in
        // lookup collapse (`BoundMethodHandle` species). Read per append (rare),
        // so no memo static. `CRATONVM_OVERRIDE_SPLIT_BOOT_PACKAGE=0` files none.
        let split_packages =
            true;
        for path in paths {
            self.bootstrap.add_path(path);
            let names = bootstrap_appended_class_names(path);
            if split_packages {
                let mut packages: FxHashSet<String> = FxHashSet::default();
                for name in &names {
                    let package = package_of(name);
                    if name != "module-info"
                        && package != "java"
                        && !package.starts_with("java/")
                        && !packages.contains(package)
                        && self
                            .module_registry
                            .named_module_for_package(package)
                            .is_none()
                    {
                        packages.insert(package.to_string());
                    }
                }
                self.class_store.note_split_boot_packages(packages);
            }
            for name in &names {
                let package = package_of(name);
                if package.is_empty() || name == "module-info" {
                    continue;
                }
                self.bootstrap_append_packages
                    .entry(package.to_string())
                    .or_insert_with(|| (path.clone(), Vec::new()))
                    .1
                    .push(name.clone());
            }
            self.bootstrap_appended_classes.extend(names);
        }
        if !self.bootstrap_appended_classes.is_empty() {
            self.bootstrap_append_seen.store(true, Ordering::Release);
        }
        // See [`Self::extend_application_classpath`]: a new bootstrap entry can
        // satisfy a name previously memoized as absent.
        if !paths.is_empty() {
            self.synthetic_upgrade_absent.clear();
        }
    }

    /// Find a class by name. Searches all loaders in priority order
    /// (bootstrap → extension → application), then any user-defined
    /// loaders that have been observed in `loaded_classes`.
    ///
    /// Returns `None` if the class hasn't been loaded by any loader.
    ///
    /// **Loader-blind — prefer [`Self::find_class_by_name_for_loader`].**
    /// This method has no notion of *which* loader is asking, so when two
    /// or more distinct user-defined loaders each define their own class
    /// under the same name it cannot know which copy the caller means (see
    /// the context.groovy note below — it now reports a miss rather than
    /// guessing, but "miss when it could have answered correctly given the
    /// requester" is itself the residual unsoundness). New call sites that
    /// have a requesting/initiating loader in hand should call
    /// `find_class_by_name_for_loader(name, requesting_loader)` instead,
    /// which checks that loader's own namespace first and never guesses
    /// across unrelated user-defined loaders. `#[deprecated]` below is
    /// advisory only (no `deny(warnings)` anywhere in the workspace, so this
    /// cannot break the centrally-run build) — it exists to surface the ~50
    /// remaining external call sites for follow-up migration. See
    /// `loader-identity.md` for the current per-file tally.
    ///
    /// **Round 4 audit fix (HIGH):** the prior fallback scanned every
    /// entry in `loaded_classes` linearly for each key (O(n · keys)).
    /// With the (`ClassLoaderId`, `Arc<str>`) keying we already have,
    /// the user-defined-loader extension is collected upfront and then
    /// probed by exact key — O(loaders · keys) hash lookups instead of
    /// O(entries · keys). On a Spring app with ~15k loaded classes that
    /// turns every miss from ~15k string compares into a handful of
    /// hash probes.
    #[deprecated(note = "loader-blind; use find_class_by_name_for_loader")]
    pub fn find_class_by_name(&self, name: &str) -> Option<ClassId> {
        let slash = if name.contains('.') && !name.contains('/') {
            name.replace('.', "/")
        } else {
            name.to_string()
        };
        let dot = slash.replace('/', ".");
        let keys = if slash == dot {
            vec![slash]
        } else {
            vec![slash, dot]
        };

        // Round 9 audit fix (HIGH #6): use the canonical
        // `BUILTIN_LOADER_DELEGATION_CHAIN` constant instead of the
        // (now-deleted) inline `BUILTIN_LOADERS` array. Parent-delegation
        // order is encoded in the constant: Bootstrap → Extension →
        // Application.

        for key in &keys {
            // C34 audit fix (HIGH): zero-allocation borrowed-key probe.
            // Previously Round 8 audit fix (HIGH #6) routed through
            // `intern_arc` to share the pool Arc with the original
            // registration — that avoided the byte comparison but still
            // paid a global-pool RwLock read per probe. The hashbrown
            // `raw_entry` path here pays neither an allocation nor a
            // pool lock; the bucket walk does a direct `&str` comparison.
            for loader_id in BUILTIN_LOADER_DELEGATION_CHAIN {
                if let Some(id) = loaded_classes_probe(&self.loaded_classes, *loader_id, key) {
                    if let Some(class) = self.get_class(id) {
                        if class.hidden {
                            continue;
                        }
                    }
                    return Some(id);
                }
            }
        }

        // Round 5 audit fix (HIGH): user-defined loaders are now tracked
        // incrementally in `self.user_loaders` (insert on every
        // `loaded_classes` insert with a `ClassLoaderId::UserDefined(_)`
        // key). The previous Round-4 implementation re-collected the set
        // by walking every entry in `loaded_classes` on each call — that
        // walk was the same O(entries) we set out to remove in Round 4,
        // just hidden one indirection deeper. The new path is
        // O(user_loaders) (typically ≤10).
        //
        // Early-out: most apps never define a user loader, in which case
        // the set is empty and we skip the inner loop entirely.
        //
        // context.groovy fix: same unsound "first match across an unordered
        // hash-set of loaders" issue as `get_loaded_class_id` above (see its
        // doc comment for the full Groovy `GroovyClassLoader$InnerLoader` /
        // same-named-closure rationale) -- when two or more DIFFERENT user
        // loaders each define their own distinct class under this name, this
        // context-free lookup cannot know which one the caller means, so it
        // must report a miss (`None`) rather than guess and silently return
        // the wrong loader's copy.
        if !self.user_loaders.is_empty() {
            for key in &keys {
                // C34 audit fix (HIGH): zero-allocation borrowed-key probe
                // (same rationale as the builtin loaders loop above).
                let mut found: Option<ClassId> = None;
                let mut ambiguous = false;
                for loader_id in &self.user_loaders {
                    if let Some(id) = loaded_classes_probe(&self.loaded_classes, *loader_id, key) {
                        if let Some(class) = self.get_class(id) {
                            if class.hidden {
                                continue;
                            }
                        }
                        match found {
                            Some(prev) if prev != id => {
                                ambiguous = true;
                                break;
                            }
                            _ => found = Some(id),
                        }
                    }
                }
                if ambiguous {
                    continue;
                }
                if let Some(id) = found {
                    if loader_flags().dbg_dupclass
                        && loader_flags()
                            .dbg_dupclass_filter
                            .as_deref()
                            .is_none_or(|f| name.contains(f))
                    {
                        eprintln!(
                            "[BLINDPICK] find_class_by_name({name:?}) -> {id:?} (loader={:?}) via user-loader scan",
                            self.class_store.get(id).map(|c| c.loader_id),
                        );
                        if loader_flags().dbg_dupclass_bt {
                            let bt = std::backtrace::Backtrace::force_capture();
                            eprintln!("[BLINDPICK-BT] {name}\n{bt}");                        }                    }
                    return Some(id);                }            }        }
        None
    }

    /// Loader-aware class lookup — JVMS §5.3/§5.4.3 delegation semantics
    /// keyed on `(requesting_loader, name)` identity rather than on `name`
    /// alone. Built-in loaders use strict parent-first delegation and never
    /// search a child namespace. User-defined loaders prefer their own exact
    /// definition, then the simplified built-in parent chain.
    ///
    /// This is the sound replacement for [`Self::find_class_by_name`]:
    /// unlike that method (and unlike [`Self::find_class_by_name_in_loader`],
    /// whose fallback now forwards here — see its doc comment), this
    /// function never scans `self.user_loaders` for an unrelated
    /// user-defined loader's same-named class. Two isolating loaders that
    /// each define their own copy of `X` must never collapse to whichever
    /// one this function happens to see; if `requesting_loader` and the
    /// built-in chain both miss, the answer is `None`, full stop.
    ///
    /// **Known limitation:** `ClassManager` does not track user-defined
    /// loader *parentage* — a user loader's `getParent()` is a Java-level
    /// field (`java.lang.ClassLoader.parent`) that this crate never
    /// observes (see `loaders.rs`'s `BUILTIN_LOADER_DELEGATION_CHAIN` doc
    /// comment: "user-defined loaders have their parent chains modelled on
    /// the Java side"). So when `requesting_loader` is itself a
    /// `ClassLoaderId::UserDefined` loader whose Java-level parent is
    /// ANOTHER user-defined loader (rather than the built-in chain), this
    /// function cannot walk that link — it degrades to "requesting loader's
    /// own namespace, then the built-in chain," which is a strict subset of
    /// full JVMS delegation for that case. Callers that need the true
    /// parent chain for such a loader must drive its `loadClass` directly
    /// at the bytecode/interpreter layer (the way
    /// `native-builtins::lang_class::native_class_get_declared_classes`
    /// falls through to `ClassLoader.loadClass` via
    /// `native-builtins::classloader::defining_loader_for` when this kind
    /// of lookup misses) rather than expecting this crate to resolve it.
    /// See `loader-identity.md`.
    pub fn find_class_by_name_for_loader(
        &self,
        name: &str,
        requesting_loader: ClassLoaderId,
    ) -> Option<ClassId> {
        // JIT round 23 wave 7 (proxy-define page; `CRATONVM_CLASS_BY_NAME_NO_ALLOC`,
        // default ON, `=0` kills): the same keys in the same order -- the
        // slash form, then the dot form only when the slash form has a `/`
        // -- without the two `String`s and the `Vec` per call. Callers
        // include every `getDeclaredConstructors` (`note_reflected_members`)
        // and the JIT's site-owner resolution.
        if cratonvm_types::flags().loader.class_by_name_no_alloc {
            return self.find_class_by_name_for_loader_borrowed(name, requesting_loader);
        }
        self.find_class_by_name_for_loader_owned(name, requesting_loader)
    }

    /// [`Self::find_class_by_name_for_loader`] under
    /// `CRATONVM_CLASS_BY_NAME_NO_ALLOC=1` (opt-in since the n23-w7 A/B): no allocation for a
    /// slash-form name found under its slash form.
    pub(in crate::class_manager) fn find_class_by_name_for_loader_borrowed(
        &self,
        name: &str,
        requesting_loader: ClassLoaderId,
    ) -> Option<ClassId> {
        let slash: std::borrow::Cow<'_, str> = if name.contains('.') && !name.contains('/') {
            std::borrow::Cow::Owned(name.replace('.', "/"))
        } else {
            std::borrow::Cow::Borrowed(name)
        };
        // A hidden class answers nothing by name: the next key is tried.
        let visible = |key: &str| {
            loaded_class_for_requesting_loader(
                &self.loaded_classes,
                &self.user_loader_parents,
                requesting_loader,
                key,
                true,
            )
            .filter(|&id| !self.get_class(id).is_some_and(|class| class.hidden))
        };
        if let Some(id) = visible(&slash) {
            return Some(id);
        }
        // The dot form is a second, different key only when the slash form
        // has a `/` (the owned path's `slash == dot` test).
        if slash.contains('/') {
            return visible(&slash.replace('/', "."));        }
        None
    }

    /// [`Self::find_class_by_name_for_loader`] with
    /// the default (`CRATONVM_CLASS_BY_NAME_NO_ALLOC` unset): the original key list.
    pub(in crate::class_manager) fn find_class_by_name_for_loader_owned(
        &self,
        name: &str,
        requesting_loader: ClassLoaderId,
    ) -> Option<ClassId> {
        let slash = if name.contains('.') && !name.contains('/') {
            name.replace('.', "/")
        } else {
            name.to_string()
        };
        let dot = slash.replace('/', ".");
        let keys = if slash == dot {
            vec![slash]
        } else {
            vec![slash, dot]
        };

        for key in &keys {
            if let Some(id) = loaded_class_for_requesting_loader(
                &self.loaded_classes,
                &self.user_loader_parents,
                requesting_loader,
                key,
                true,
            ) {
                if let Some(class) = self.get_class(id) {
                    if class.hidden {
                        continue;
                    }
                }
                return Some(id);
            }
        }
        None
    }

    /// `Some(referencing_class_id)` when `referencing_class_id` is a HIDDEN
    /// class and `name` -- a name its own constant pool carries -- is its own
    /// class-FILE name, while `CRATONVM_HIDDEN_SELF_CP_BY_ID` is on
    /// ([`HiddenClassBits::self_cp_by_id`]); `None` otherwise (JIT round 21
    /// wave 20, lane hidden-self-resolve;
    /// `docs/feature-designs/class-resolution-and-caching.md`).
    ///
    /// JVMS 5.4.3.1 and `Lookup::defineHiddenClass`: in a hidden class,
    /// `this_class` and every other `CONSTANT_Class` naming the class's own
    /// name resolve to the hidden class itself, never through the loader. A
    /// hidden class is stored as `<class-file name>/0x<hex>` and is in no
    /// namespace, so a by-name lookup of that name answers nothing until an
    /// ordinary class of the name is loaded, and that third class from then
    /// on (`R21Fz5HiddenOsr1`: its private `step` / `priv` were then judged
    /// against the ordinary class's, an `IllegalAccessError` in every mode).
    ///
    /// Cost for an ordinary referencing class: one load of the lock-free
    /// hidden-class bitmap. The switch is read only for a hidden one.
    #[inline]
    pub fn hidden_self_cp_answer(
        &self,
        name: &str,
        referencing_class_id: ClassId,
    ) -> Option<ClassId> {
        (self.is_hidden_self_name(referencing_class_id, name)
            && self.hidden_classes.self_cp_by_id())
        .then_some(referencing_class_id)
    }

    /// The class-file name (`this_class`) hidden class `id` was defined
    /// from, recorded at define time; `None` for an ordinary class, bytes
    /// with an empty `this_class`, or a class marked hidden after a define
    /// under its own name (JIT round 21 wave 21). Since wave 22 (lane
    /// hidden-resid) read from the class store, which drops it at unload.
    #[inline]
    pub fn hidden_class_file_name(&self, id: ClassId) -> Option<&str> {
        self.get_class(id)
            .filter(|class| class.hidden)
            .and_then(|_| self.class_store.recorded_file_name(id))
    }

    /// [`hidden_self_name_rule`] for class `id` of this manager: is `name`,
    /// from `id`'s own constant pool, the HIDDEN class `id`'s own class-file
    /// name? The bitmap first (one load for an ordinary class, whatever its
    /// id), then the class and its recorded class-file name. Not switched:
    /// the switches belong to the doors that act on the answer.
    #[inline]
    pub fn is_hidden_self_name(&self, id: ClassId, name: &str) -> bool {
        if self.hidden_classes.contains(id) == Some(false) {
            return false;
        }
        self.get_class(id)
            .is_some_and(|class| self.class_store.hidden_self_name(class, name))
    }

    /// [`Self::find_class_by_name_for_class`] for a name read from the
    /// constant pool of `referencing_class_id`: a hidden class's own
    /// class-file name answers the hidden class itself
    /// ([`Self::hidden_self_cp_answer`]), every other name the loader lookup.
    /// For constant-pool resolution only -- `Class.forName` from a hidden
    /// class does NOT find the hidden class (it is in no namespace).
    #[inline]
    pub fn find_cp_class_for_class(
        &self,
        name: &str,
        referencing_class_id: ClassId,
    ) -> Option<ClassId> {
        self.hidden_self_cp_answer(name, referencing_class_id)
            .or_else(|| self.find_class_by_name_for_class(name, referencing_class_id))
    }

    /// Resolve a loaded class through the defining loader of
    /// `requesting_class_id`.
    ///
    /// This is the preferred convenience API for constant-pool, exception,
    /// reflection and JIT metadata paths that already carry the class whose
    /// symbolic reference is being resolved.
    #[inline]
    pub fn find_class_by_name_for_class(
        &self,
        name: &str,
        requesting_class_id: ClassId,
    ) -> Option<ClassId> {
        let loader = self.get_loader_id(requesting_class_id)?;
        self.find_class_by_name_for_loader(name, loader)
    }

    /// Resolve an exact bootstrap definition. A bootstrap lookup never
    /// delegates down to extension, application or user namespaces.
    pub fn find_bootstrap_class_by_name(&self, name: &str) -> Option<ClassId> {
        let slash = if name.contains('.') && !name.contains('/') {
            name.replace('.', "/")
        } else {
            name.to_string()
        };
        let dot = slash.replace('/', ".");
        for key in [&slash, &dot] {
            if let Some(id) =
                loaded_classes_probe(&self.loaded_classes, ClassLoaderId::Bootstrap, key)
            {
                if self.get_class(id).is_some_and(|class| !class.hidden) {
                    return Some(id);                }            }        }
        None
    }

    /// Context-free lookup that succeeds only when one loader has defined the
    /// requested name. This is safe for diagnostics and legacy metadata that
    /// genuinely carry no initiating loader; runtime resolution should use
    /// [`Self::find_class_by_name_for_class`] instead.
    pub fn find_unique_class_by_name(&self, name: &str) -> Option<ClassId> {
        // An already-internal name with no separator to translate is the
        // overwhelmingly common case and both probe keys collapse onto it, so
        // take it without allocating either `String`.
        if !name.contains('.') && !name.contains('/') {
            return self.unique_visible_definition(name);
        }
        // i6-L2: an internal name (`java/lang/String`) is its own slash key,
        // so probe it without the two `String`s below; they are built only
        // when the probe misses and the dotted spelling still has to be tried.
        // Same keys, same order as the general path.
        if !name.contains('.') {
            if let Some(id) = self.unique_visible_definition(name) {
                return Some(id);
            }
            return self.unique_visible_definition(&name.replace('/', "."));
        }
        let slash = if name.contains('/') {
            name.to_string()
        } else {
            name.replace('.', "/")
        };
        let dot = slash.replace('/', ".");
        for key in [&slash, &dot] {
            if let Some(id) = self.unique_visible_definition(key) {
                return Some(id);
            }
        }
        None
    }

    /// Classify a class name against the definition index, keeping the three
    /// "no answer" outcomes apart.
    ///
    /// C2 review P1 (classloading identity): every context-free lookup in this
    /// file collapses *absent* and *ambiguous* onto `None`
    /// ([`Self::find_unique_class_by_name`], [`Self::get_loaded_class_id`],
    /// [`Self::find_class_by_name`]). Returning `None` for ambiguity is the
    /// correct **fail-closed** answer — picking one of several same-named
    /// classes is type confusion — but it is only half the contract: a caller
    /// that reads `None` as "nobody has this class" goes on to *load a second
    /// copy*, which is precisely how the Spring AOT `argument type mismatch`
    /// family and the Groovy `$_run_closureN` collapse were produced. The
    /// collapse is unrecoverable at the call site because the two cases demand
    /// opposite actions:
    ///
    ///   * `Absent`    — go and load it (there is nothing to conflict with).
    ///   * `Ambiguous` — do NOT load; re-ask with an initiating loader via
    ///     [`Self::find_class_by_name_for_loader`] /
    ///     [`Self::find_class_by_name_for_class`], or fail.
    ///   * `Unique`    — use it.
    ///
    /// O(1) against [`Self::name_definitions`]. Accepts either separator form
    /// (`p/X` or `p.X`), matching [`Self::find_unique_class_by_name`].
    pub fn classify_loaded_name(&self, name: &str) -> NameResolution {
        // An already-internal name with no separator to translate is the
        // overwhelmingly common case and both probe keys collapse onto it, so
        // take it without allocating either `String`.
        if !name.contains('.') && !name.contains('/') {
            return self.classify_exact_name(name);
        }
        let slash = if name.contains('/') {
            name.to_string()
        } else {
            name.replace('.', "/")
        };
        let dot = slash.replace('/', ".");
        let slash_answer = self.classify_exact_name(&slash);
        if !matches!(slash_answer, NameResolution::Absent) {
            return slash_answer;
        }
        self.classify_exact_name(&dot)
    }

    /// [`Self::classify_loaded_name`] without separator normalisation.
    pub(in crate::class_manager) fn classify_exact_name(&self, name: &str) -> NameResolution {
        let Some(ids) = self.name_definitions.get(name) else {
            return NameResolution::Absent;
        };
        let mut distinct = ids.keys();
        let Some(&id) = distinct.next() else {
            // An empty id set is retired by `release_name_definition`, so this
            // is unreachable in practice; treat it as absent rather than
            // asserting on a shape the index promises not to produce.
            return NameResolution::Absent;
        };
        if distinct.next().is_some() {
            return NameResolution::Ambiguous {
                definitions: ids.len(),
            };
        }
        // A hidden class is not reachable by name at all (JVMS §5.4.3.1 — a
        // hidden class is never recorded in the loader's name table), so a lone
        // hidden definition reads as absent, not as a usable unique answer.
        if self.get_class(id).is_some_and(|class| !class.hidden) {
            NameResolution::Unique(id)
        } else {
            NameResolution::Absent
        }
    }

    /// The one `ClassId` defined under `name`, or `None` when zero or more
    /// than one distinct class carries it (ambiguity is information loss, not
    /// permission to pick the first entry) or when the single match is hidden.
    ///
    /// O(1) against [`Self::name_definitions`]; see that field for why the
    /// linear scan this replaced mattered so much.
    pub(in crate::class_manager) fn unique_visible_definition(&self, name: &str) -> Option<ClassId> {
        match self.classify_exact_name(name) {
            NameResolution::Unique(id) => Some(id),
            NameResolution::Absent | NameResolution::Ambiguous { .. } => None,
        }
    }

    /// Sole writer of `loaded_classes` inserts -- keeps `name_definitions` in
    /// step. Returns the displaced `ClassId`, exactly like `HashMap::insert`.
    pub(in crate::class_manager) fn loaded_classes_insert(
        &mut self,
        key: (ClassLoaderId, Arc<str>),
        id: ClassId,
    ) -> Option<ClassId> {
        if let Ok(filter) = Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
            if !filter.is_empty() && key.1.contains(filter.as_str()) {
                eprintln!(
                    "[DBG_DEFINE] insert name={} loader={:?} id={}",
                    key.1,
                    key.0,
                    id.as_u32()
                );
                if matches!(key.0, ClassLoaderId::Application) {
                    eprintln!(
                        "[DBG_DEFINE_BT] {}",
                        std::backtrace::Backtrace::force_capture()
                    );
                }
            }
        }
        let name = Arc::clone(&key.1);
        // One name comparison on the class-DEFINITION path (cold: at most a
        // few thousand times per process) buys `is_annotation_proxy_class` a
        // one-relaxed-load answer on the virtual-invoke path (hot: millions of
        // times per second). See `StoreEpochs::annotation_proxy_defined`:
        // this store's latch, not a process one (interpreter round i1 wave
        // 25, lane L5).
        if is_vm_annotation_carrier_name(&name) {
            store_epochs(self.class_store.layout_domain())
                .annotation_proxy_defined
                .store(true, Ordering::Relaxed);
        }
        let displaced = self.loaded_classes.insert(key, id);
        bump_class_definition_epoch(self.class_store.layout_domain());
        // Read `name_definitions` BEFORE it is updated below: the question is
        // whether this insert can change an answer that already existed.
        if displaced.is_some_and(|old| old != id)
            || name_insert_changes_an_existing_answer(&self.name_definitions, &name, id)
        {
            bump_class_name_generation(self.class_store.layout_domain());
        }
        if let Some(old) = displaced {
            release_name_definition(&mut self.name_definitions, &name, old);
        }
        // `distinct` is the number of *different* `ClassId`s this name now
        // resolves to. Two loaders mapping one name to the same id keep it at
        // 1 (the inner map is keyed by id, refcounted by loader), so this is
        // exactly the loader-split condition `any_duplicate_class_name`
        // reports. Reading it here costs nothing: the `entry` walk that
        // maintains the index is already being done.
        let distinct = {
            let per_name = self.name_definitions.entry(Arc::clone(&name)).or_default();
            *per_name.entry(id).or_insert(0) += 1;
            per_name.len()
        };
        if distinct > 1 {
            note_duplicate_class_name(&name);
        }
        displaced
    }

    /// Sole writer of `loaded_classes` removals -- keeps `name_definitions` in
    /// step.
    pub(in crate::class_manager) fn loaded_classes_remove(&mut self, key: &(ClassLoaderId, Arc<str>)) -> Option<ClassId> {
        let removed = self.loaded_classes.remove(key);
        if removed.is_some() {
            bump_class_definition_epoch(self.class_store.layout_domain());
            bump_class_name_generation(self.class_store.layout_domain());
        }
        if let Some(old) = removed {
            release_name_definition(&mut self.name_definitions, &key.1, old);
        }
        removed
    }

    /// zt t16/df6: the unbound define holding `(loader_id, name)`
    /// ([`Self::unbound_defines`]), if any. One `is_empty` test while no
    /// define door has an upcall in flight.
    pub(in crate::class_manager) fn unbound_define_named(&self, loader_id: ClassLoaderId, name: &str) -> Option<ClassId> {
        if self.unbound_defines.is_empty() {
            return None;
        }
        self.unbound_defines
            .iter()
            .find(|(_, (l, n))| *l == loader_id && &**n == name)
            .map(|(&id, _)| id)
    }

    /// zt t16/df6 (define page D6): the define of `name` into `loader_id`
    /// that still waits for its `addClass` upcall ([`Self::unbound_defines`]),
    /// if any. A define door asks before its own define, and waits for the
    /// answer to turn `None`, as HotSpot's second definer waits on the first
    /// one's placeholder. An EMPTY `name` answers any unbound define into
    /// `loader_id` (a nameless door asks that first, so it reads the bytes'
    /// `this_class` only while one is). One `is_empty` test when none is.
    pub fn unbound_define_of(&self, name: &str, loader_id: ClassLoaderId) -> Option<ClassId> {
        if self.unbound_defines.is_empty() {
            return None;
        }
        if name.is_empty() {
            return self
                .unbound_defines
                .iter()
                .find(|(_, (l, _))| *l == loader_id)
                .map(|(&id, _)| id);
        }
        self.unbound_define_named(loader_id, name)
    }

    /// zt t16/df6: is `class_id` an unbound define ([`Self::unbound_defines`])?
    pub fn is_unbound_define(&self, class_id: ClassId) -> bool {
        !self.unbound_defines.is_empty() && self.unbound_defines.contains_key(&class_id)
    }

    /// zt t16/df6 (define page §1, corner 2): the define door's `addClass`
    /// upcall for `class_id` returned, so its name is bound now, as HotSpot's
    /// `update_dictionary` runs after the upcall: `loaded_classes`, the store's
    /// name index and the JVMS §5.3.4 constraint pin the define skipped.
    ///
    /// `None`: `class_id` was not an unbound define (its name was bound at the
    /// define, or this was called twice); nothing to do. `Some(true)`: bound.
    /// `Some(false)`: the key was taken meanwhile by another class (no define
    /// can take it: the duplicate check counts unbound defines; only a path
    /// that inserts without that check could), so the class stays nameless,
    /// as after [`Self::withdraw_unbound_define`], and the caller refuses its
    /// define as a duplicate definition.
    pub fn bind_unbound_define(&mut self, class_id: ClassId) -> Option<bool> {
        let (loader_id, name) = self.unbound_defines.remove(&class_id)?;
        if self.class_store.get(class_id).is_none()
            || loaded_classes_probe(&self.loaded_classes, loader_id, &name).is_some()
        {
            return Some(false);
        }
        self.loaded_classes_insert((loader_id, Arc::clone(&name)), class_id);
        // zt t30/dc: the `bind-restore-name` census row (its only caller is
        // a define door's settle, so the census flag alone scopes it).
        let restore_clock = crate::define_census::phase_start();
        self.class_store.restore_name(class_id);
        crate::define_census::phase_end(
            crate::define_census::DefinePhase::BindRestoreName,
            restore_clock,
        );
        let constraints = self.loader_constraints.get_mut();
        if !constraints.is_empty() {
            let broken =
                constraints.pin_if_constrained(&name, loader_id.to_native_id(), class_id.as_u32());
            if broken.is_some() && loader_flags().dbg_access {
                eprintln!(
                    "[ACCESS-DBG] LOADER-CONSTRAINT (counted, admitted) bound define of {name} by {loader_id}"
                );
            }
        }
        // zt t17/x13: the ClassLoad event the define withheld, now that a
        // lookup can answer the class (queued; delivered when the caller's
        // write guard drops, as the define's own would have been).
        let owner_domain = self.class_store.layout_domain();
        fire_class_load_hook(
            class_id.as_u32(),
            &name,
            current_thread_id(owner_domain),
            owner_domain,
        );
        Some(true)
    }

    /// zt t16/df6 (define page §1, corner 2): the define door's `addClass`
    /// upcall for `class_id` threw, so the define fails and leaves no class
    /// under its name, as HotSpot's (`define_instance_class` throws before
    /// `update_dictionary`). The name was never bound, so nothing is
    /// unpublished: the row goes, and the class stays in the store with no
    /// name binding (no lookup can answer it; it unloads with its loader).
    /// A later define of the name, with these bytes or others, defines a
    /// fresh class. `false` when `class_id` was not an unbound define.
    ///
    /// zt t21/df9 (define page §9): HotSpot's `check_constraints` ran BEFORE
    /// the upcall, and its `LoaderConstraintTable::check_or_update` set a
    /// pending constraint on (name, loader) to this class then; the failed
    /// upcall leaves that pin in place, naming a class no dictionary holds
    /// (never `loaded`). So the constraint stays pinned here too, to the
    /// withdrawn class, and a later define of the name by any loader of the
    /// constraint is refused with HotSpot's text for a pin to a class that is
    /// not loaded ([`Self::loader_constraint_define_refusal`]). Only a
    /// constraint that names (name, loader) is touched (`pin_if_constrained`:
    /// no row is grown). `CRATONVM_ZGC_DEFINE_WITHDRAW_PINS_CONSTRAINT=0`:
    /// nothing is pinned (the pre-t21 answer: the retry defines).
    pub fn withdraw_unbound_define(&mut self, class_id: ClassId) -> bool {
        let Some((loader_id, name)) = self.unbound_defines.remove(&class_id) else {
            return false;
        };
        let constraints = self.loader_constraints.get_mut();
        if !constraints.is_empty()
            && self.class_store.get(class_id).is_some()
        {
            let broken =
                constraints.pin_if_constrained(&name, loader_id.to_native_id(), class_id.as_u32());
            if loader_flags().dbg_access {
                eprintln!(
                    "[ACCESS-DBG] LOADER-CONSTRAINT withdrawn define of {name} by {loader_id} keeps its pin (broken={})",
                    broken.is_some()
                );
            }
        }
        true
    }

    // `rebuild_name_definitions` (recompute the whole index from every alias)
    // was deleted in gc-common w18-d: the unload path, its only caller, now
    // releases exactly the aliases it removes (`release_name_definition`, as
    // `loaded_classes_remove` does). Its duplicate-name scan added nothing: a
    // removal can only shrink a name's id set, and the latch it raised is
    // sticky and already raised by the insert that made the duplicate.

    /// Find a class by name within a specific loader's namespace, with delegation
    /// fallback to the standard loader chain (Bootstrap → Extension → Application).
    /// Exact-key lookup: the `ClassId` recorded under *exactly* `(loader_id,
    /// name)`, with **no** parent-delegation / global fallback.
    ///
    /// Unlike [`Self::find_class_by_name_in_loader`] (which falls back to the
    /// built-in delegation chain on a miss, making it loader-blind for names the
    /// loader did not itself define), this answers only "has `loader_id` already
    /// defined this exact name?". Used by loader-faithful `CONSTANT_Class`
    /// resolution to detect a class a user-defined loader has *itself* defined
    /// (e.g. an isolated copy) before falling through to invoking its
    /// `loadClass`.
    pub fn class_defined_by_loader_exact(
        &self,
        name: &str,
        loader_id: ClassLoaderId,
    ) -> Option<ClassId> {
        loaded_classes_probe(&self.loaded_classes, loader_id, name).or_else(|| {
            // The per-loader index is the normal O(1) path.  A few
            // re-entrant defineClass paths can expose a fully usable Class
            // before that index has been populated; retain exact defining
            // loader semantics on that cold path instead of collapsing to an
            // unrelated application-loader copy.
            self.class_store.find_id_by_loader_and_name(name, loader_id)
        })
    }

    /// **Behavior change (loader-identity consolidation):** the parent-chain
    /// fallback used to be the loader-blind `find_class_by_name`, which — on
    /// a miss in the built-in chain — additionally scanned every OTHER
    /// user-defined loader's namespace and returned an unambiguous same-named
    /// match if it found exactly one. That was a guess: it could hand back a
    /// completely unrelated user loader's class just because `loader_id`
    /// itself and the built-ins didn't have it. The fallback now forwards to
    /// [`Self::find_class_by_name_for_loader`], which stops at the built-in
    /// chain and never guesses across unrelated user loaders. Practical
    /// effect: callers only see a difference in the case that WAS unsound
    /// (some other, unrelated user loader happened to have a same-named
    /// class); the own-namespace and built-in-chain paths are unchanged.
    pub fn find_class_by_name_in_loader(
        &self,
        name: &str,
        loader_id: ClassLoaderId,
    ) -> Option<ClassId> {
        // C34 audit fix (HIGH): zero-allocation borrowed-key probe.
        // Previously Round 8 audit fix (HIGH #6) used `intern_arc` so the
        // probe Arc shared the global pool with the original
        // registration — saving the byte comparison but still paying a
        // global RwLock read. The hashbrown `raw_entry` path here pays
        // neither.
        if let Some(id) = loaded_classes_probe(&self.loaded_classes, loader_id, name) {
            return Some(id);
        }
        // Delegate to parent chain — see the doc comment above.
        self.find_class_by_name_for_loader(name, loader_id)
    }

    /// Whether the user namespace `loader` still defines a class, as of the
    /// last unload (gc-common w11-e). Right after
    /// [`Self::unload_user_classes`], `false` for a namespace named by one of
    /// the unloaded classes means that namespace has no class left, so a
    /// VM-side table keyed by it can drop its rows. Answers from the
    /// `user_loaders` set, which an unload rebuilds from the surviving
    /// classes; between unloads it may also hold a namespace that only
    /// initiated a load.
    ///
    /// `true` for every built-in loader (gc-common w18-d; it was `false`). A
    /// built-in namespace is permanent, so an unload never empties it, and
    /// since hidden classes of built-in namespaces unload
    /// ([`Self::unloads_on_hint`]) the question is asked about them:
    /// `unload_dead_class_metadata` drops the whole initiating-resolution memo
    /// of every namespace this answers `false` for, so the old answer flushed
    /// the application loader's memo each time one of its hidden classes died.
    pub fn user_namespace_has_classes(&self, loader: ClassLoaderId) -> bool {
        !matches!(loader, ClassLoaderId::UserDefined(_)) || self.user_loaders.contains(&loader)
    }

    /// Get the loader identity for a loaded class.
    pub fn get_loader_id(&self, class_id: ClassId) -> Option<ClassLoaderId> {
        self.class_store.get(class_id).map(|c| c.loader_id)
    }

    /// Convenience: check if `child_id` is a subclass of (or implements) `parent_id`.
    pub fn is_subclass_of(&self, child_id: ClassId, parent_id: ClassId) -> bool {
        self.class_store
            .get(child_id)
            .is_some_and(|child| child.is_subclass_of(parent_id, &self.class_store))
    }

    /// Loader-identity-blind fallback for [`is_subclass_of`] -- see
    /// `Class::is_subclass_of_by_name`'s doc comment for the full rationale
    /// (exception-handler `catch_type` resolution needing to match a
    /// same-named-but-different-`ClassId` exception class across loaders).
    pub fn is_subclass_of_by_name(&self, child_id: ClassId, target_name: &str) -> bool {
        self.class_store
            .get(child_id)
            .is_some_and(|child| child.is_subclass_of_by_name(target_name, &self.class_store))
    }

    /// Loader-identity-blind assignability for JIT `checkcast`/`instanceof` —
    /// see `Class::is_assignable_to_name`'s doc comment for the rationale
    /// (same-named class defined by two loaders resolving to different
    /// `ClassId`s through the flat name-only lookup).
    pub fn is_assignable_to_name(&self, child_id: ClassId, target_name: &str) -> bool {
        self.class_store
            .get(child_id)
            .is_some_and(|child| child.is_assignable_to_name(target_name, &self.class_store))
    }

    /// Get a reference to the underlying class store.
    pub fn class_store(&self) -> &ClassStore {
        &self.class_store
    }

    /// The number of classes currently loaded.
    pub fn loaded_count(&self) -> usize {
        self.class_store.len()
    }

    /// Is any currently-loaded class a member of `package_slash` (e.g.
    /// `"java/sql"`, `""` for the default package)?
    ///
    /// This is the DEFINITION question `ClassLoader.getDefinedPackage` asks,
    /// and it is not the same as the visibility question
    /// `find_all_resource_urls` answers: a package whose class files sit in the
    /// boot image but none of whose classes has been loaded is visible and NOT
    /// defined, exactly as on HotSpot, where a loader defines a package when it
    /// defines a class in it.
    ///
    /// Immediate members only — `java/sql` does not answer for
    /// `java/sql/rowset/Foo`, matching the JDK's flat package namespace.
    ///
    /// O(loaded classes). Called only from the `getDefinedPackage` /
    /// `getDefinedPackages` family, which is not a hot path; deliberately NOT
    /// memoised, because classes keep loading and a memo would freeze the
    /// answer a package had before its first class arrived.
    /// [`Self::any_loaded_class_in_package`], narrowed to the classes ONE
    /// loader defined.
    ///
    /// `loader_native_id` is `ClassLoaderId::to_native_id()`'s flat value, the
    /// same numbering `NativeContext::loader_id_of_class` reports and
    /// `classloader::loader_namespace_id` hands out — so a user-defined
    /// loader's own id (>= 3) selects exactly the classes IT defined, and
    /// nothing a parent or the application loader defined.
    ///
    /// This is the question `ClassLoader.getDefinedPackage` asks of a CUSTOM
    /// loader. The unscoped form cannot answer it: `com.example.app` has a
    /// loaded class in every run, and answering `true` for a loader that
    /// defined none of them is the "any package I can see" error one arm over.
    pub fn any_loaded_class_in_package_for_loader(
        &self,
        package_slash: &str,
        loader_native_id: u32,
    ) -> bool {
        // The package index (`CRATONVM_LOADER_PACKAGE_INDEX`): the package's
        // own classes, filtered by their CURRENT loader as the walk does.
        if let Some(ids) = self.class_store.package_member_ids(package_slash) {
            return ids.iter().any(|&i| {
                self.class_store
                    .get(ClassId::new(i))
                    .is_some_and(|c| c.loader_id.to_native_id() == loader_native_id)
            });
        }
        self.class_store.iter().any(|c| {
            c.loader_id.to_native_id() == loader_native_id
                && match c.name.rsplit_once('/') {
                    Some((pkg, _)) => pkg == package_slash,
                    None => package_slash.is_empty(),
                }
        })
    }

    /// One loaded class per distinct package among the classes `loader_native_id`
    /// defined from real bytes (the boot image or the class path; never a
    /// hidden class, an array, a generated class or a compatibility stub), as
    /// `(package in internal form, class id)`. `""` is the unnamed package.
    /// What HotSpot's built-in loaders list in their `packages` map for the
    /// classes they defined (interpreter round i2 wave 55, lane L5:
    /// `lang_class::sync_builtin_loader_packages`). Linear in the class store;
    /// asked only by the package-listing doors.
    pub fn loaded_package_representatives(&self, loader_native_id: u32) -> Vec<(String, ClassId)> {
        let mut seen: FxHashSet<&str> = FxHashSet::default();
        let mut out = Vec::new();
        for c in self.class_store.iter() {
            if c.loader_id.to_native_id() != loader_native_id
                || c.hidden
                || c.name.starts_with('[')
                || !matches!(
                    c.origin,
                    ClassOrigin::BootImage { .. } | ClassOrigin::ApplicationClassPath { .. }
                )
            {
                continue;
            }
            let pkg = c.name.rsplit_once('/').map_or("", |(p, _)| p);
            if seen.insert(pkg) {
                out.push((pkg.to_string(), c.id));
            }
        }
        out
    }

    /// [`Self::loaded_package_representatives`]'s class for the one package
    /// `package_slash`: the first class in store order that the listing would
    /// keep for it (so the same class), with no allocation and a stop at the
    /// first match. Still linear in the class store on a miss. Interpreter
    /// round i4 wave 93, lane L5.
    pub fn loaded_package_representative(
        &self,
        loader_native_id: u32,
        package_slash: &str,
    ) -> Option<ClassId> {
        let keep = |c: &Class| {
            c.loader_id.to_native_id() == loader_native_id
                && !c.hidden
                && !c.name.starts_with('[')
                && matches!(
                    c.origin,
                    ClassOrigin::BootImage { .. } | ClassOrigin::ApplicationClassPath { .. }
                )
                && c.name.rsplit_once('/').map_or("", |(p, _)| p) == package_slash
        };
        // `CRATONVM_LOADER_PACKAGE_INDEX` (interpreter round i4 wave 94, lane
        // L5): the package's own classes, in id (= store) order, so the same
        // first match as the walk; a package with no class is one probe.
        if let Some(ids) = self.class_store.package_member_ids(package_slash) {
            return ids
                .iter()
                .filter_map(|&i| self.class_store.get(ClassId::new(i)))
                .find(|c| keep(*c))
                .map(|c| c.id);
        }
        self.class_store.iter().find(|c| keep(*c)).map(|c| c.id)
    }

    pub fn any_loaded_class_in_package(&self, package_slash: &str) -> bool {
        // The package index (`CRATONVM_LOADER_PACKAGE_INDEX`) holds exactly the
        // live classes whose name the walk below would match.
        if let Some(ids) = self.class_store.package_member_ids(package_slash) {
            return !ids.is_empty();
        }
        self.class_store.iter().any(|c| {
            // Array classes (`[Ljava/lang/String;`) are members of no package
            // for this purpose; the leading `[` keeps them from matching a
            // real name anyway.
            match c.name.rsplit_once('/') {
                Some((pkg, _)) => pkg == package_slash,
                None => package_slash.is_empty(),
            }
        })
    }

    /// Register a class name → id mapping for a given loader.
    ///
    /// This is primarily used by test code that manually inserts classes
    /// into the `ClassStore` and needs `find_class_by_name` to work.
    pub fn register_class_name(&mut self, loader_id: ClassLoaderId, name: &str, id: ClassId) {
        // T10.9.E: intern the name through the global pool so that
        // subsequent define-class flows (which clone `class.name` for the
        // hot insert) share the same `Arc<str>` allocation — keeping the
        // key dedup story identical regardless of whether the class is
        // registered via this side-door or via `define_class_with_options`.
        //
        // Round 4 audit fix: previously also wrote into the redundant
        // collision-unsafe `name_to_id` shadow map; that map is gone now
        // and `loaded_classes` is the only index.
        let name_arc = cratonvm_types::intern_arc(name);
        self.loaded_classes_insert((loader_id, name_arc), id);
        // Round 5 audit fix (HIGH): keep `user_loaders` in sync — see
        // `define_class_with_options` for the rationale (avoid the
        // O(entries) walk in `find_class_by_name`).
        if matches!(loader_id, ClassLoaderId::UserDefined(_)) {
            self.user_loaders.insert(loader_id);
        }
    }

    /// Create a synthetic stub class for a JDK class that has no .class file.
    ///
    /// The stub has no methods (all handled by native registry) and minimal
    /// fields. Special cases add static fields for well-known classes like
    /// `java/lang/System` (needs `in`, `out`, `err`).
    pub(in crate::class_manager) fn create_synthetic_stub(&mut self, name: &str) -> Result<ClassId, VmError> {
        // Guard against re-entry: check again if already loaded.
        // T10.9.E: probe with `Arc::<str>::from(name)` — same allocation
        // cost as the prior `name.to_string()`.
        if let Some(&id) = self
            .loaded_classes
            .get(&(ClassLoaderId::Bootstrap, Arc::<str>::from(name)))
        {
            return Ok(id);
        }

        // Real-JDK choke point #1 (contract §5). This is the single funnel for
        // the whole `load_class` fabrication chain — the enterprise prefixes
        // (`org/jboss`, `io/quarkus`, `io/smallrye`, …), the
        // `ProcessHandle`/`ProcessHandle$Info` native-backed exemption, and the
        // residual "JDK-namespace name with no bytes anywhere" case all arrive
        // here and nowhere else. Placing the gate at the callee rather than at
        // each of `load_class`'s arms means a future arm cannot route around it.
        //
        // `ClassNotFoundException` is the right refusal: the three sibling arms
        // of the same `load_class` match (`$$`-generated names, `package-info`,
        // the reflective-probe gate) already return exactly this error, so every
        // downstream caller is written to handle it, and constant-pool
        // resolution converts it to `NoClassDefFoundError` with no extra work.
        let stub_reason = if is_native_backed_jdk_stub(name) {
            NATIVE_BACKED_STUB_REASON
        } else if self.is_enterprise_stub_prefix(name) {
            ENTERPRISE_PREFIX_STUB_REASON
        } else {
            MISSING_CLASS_FILE_STUB_REASON
        };
        self.admit_compatibility_class(name, stub_reason)?;

        // Load superclass with correct hierarchy for known JDK classes.
        // Without this, exception catch handlers can't match subclasses
        // (e.g., `catch (RuntimeException e)` won't catch ArithmeticException).
        let superclass_id = if name == "java/lang/Object" {
            None
        } else {
            let parent = jdk_superclass(name);
            Some(self.load_class(parent)?)
        };

        // Create static fields for known classes
        let fields = self.synthetic_stub_fields_for(name);
        let num_static = fields
            .iter()
            .filter(|f| f.access_flags.contains(FieldAccessFlags::STATIC))
            .count();
        let num_instance = fields.len() - num_static;

        let parent_fields = match superclass_id {
            Some(super_id) => self
                .class_store
                .get(super_id)
                .map_or(0, |c| c.num_total_fields),
            None => 0,
        };

        // Load interface classes AFTER the stub is registered (to avoid re-entrant cycles)
        let iface_names = jdk_interfaces(name).to_vec();

        let id = self.class_store.next_id();
        // S-trinity #1: the `$`-name-as-interface heuristic misclassifies
        // concrete inner classes. Carve out the JDK loader chain
        // (`ClassLoaders$AppClassLoader` etc.) which is concrete; without
        // this exception, `alloc_classloader` produces objects whose
        // class chain has the INTERFACE bit set, and downstream
        // `(ClassLoader) priv.run()` checkcasts fail.
        let is_concrete_dollar_class = matches!(
            name,
            "jdk/internal/loader/ClassLoaders$AppClassLoader"
                | "jdk/internal/loader/ClassLoaders$PlatformClassLoader"
                | "java/util/Collections$SynchronizedObject"
                | "java/util/Collections$SynchronizedCollection"
                | "java/util/Collections$SynchronizedSet"
                | "java/util/Collections$SynchronizedMap"
                | "java/util/Collections$SingletonList"
                | "java/util/Collections$SingletonSet"
                | "java/util/Collections$SingletonMap"
                | "java/util/Collections$EmptyList"
                | "java/util/Collections$EmptySet"
                | "java/util/Collections$EmptyMap"
                | "java/util/Collections$EmptyIterator"
                | "java/util/Collections$EmptyListIterator"
                | "java/util/Collections$EmptyEnumeration"
                | "java/util/ArrayList$Itr"
                | "java/util/ArrayList$ListItr"
                // `Arrays.asList(T...)`'s fixed-size view. Concrete, and
                // instantiated: `native_arrays_as_list` allocates one per call
                // and the accessors read its `a` slot. Left to the `$`
                // heuristic it was fabricated as an ABSTRACT INTERFACE, so the
                // one field it needs could never be stored — every
                // `Arrays.asList(...)` answered `size() == 0`, which is what
                // `KNOWN_SYNTHETIC_JDK_GAPS` pinned as a missing
                // `Arrays.asList`.
                | "java/util/Arrays$ArrayList"
                | "java/util/function/Function$Identity"
        );
        // LETSGO_S1: Curated list of well-known JDK interfaces whose names
        // aren't matched by the `$` / `*able` / heuristic. Without this,
        // synthetic stubs for `java.util.{Set, Map, List, Collection,
        // Queue, Deque, Iterator, Map$Entry, ...}` end up as concrete
        // classes and `instanceof` walks via the `interfaces` edge fail
        // when no concrete bytecode `Set`/`Map`/etc. was loaded ahead of
        // the dependent class. (LinkedHashSet → HashSet → AbstractSet
        // chain is fine without this, but Spring boot's `Set.class`
        // reflection probes still need it.)
        let is_known_jdk_interface = matches!(
            name,
            "java/util/Collection"
                | "java/util/Set"
                | "java/util/SortedSet"
                | "java/util/NavigableSet"
                | "java/util/List"
                | "java/util/Map"
                | "java/util/SortedMap"
                | "java/util/NavigableMap"
                | "java/util/Queue"
                | "java/util/Deque"
                | "java/util/Iterator"
                | "java/util/ListIterator"
                | "java/util/Enumeration"
                | "java/util/Spliterator"
                | "java/util/RandomAccess"
                | "java/util/concurrent/ConcurrentMap"
                | "java/util/concurrent/BlockingQueue"
                | "java/util/concurrent/BlockingDeque"
                | "java/util/concurrent/TransferQueue"
                | "java/lang/ProcessHandle"
                | "java/lang/ProcessHandle$Info"
        );
        let access_flags = if jdk_superclass(name) == "java/lang/Enum" {
            // ACC_ENUM, derived from the superclass row rather than from a
            // second list of enum names — one fact, one place. It must be
            // tested BEFORE the `$`/`able` heuristics: `HttpClient$Version`
            // contains a `$` and would otherwise be fabricated as an INTERFACE.
            //
            // `native-builtins`' `class_is_declared_enum` requires BOTH
            // `ACC_ENUM` and a `java/lang/Enum` parent, and it gates
            // `Class.getEnumConstants`, which `EnumSet.allOf` reads through.
            // Without the flag a synthetic enum publishes its constants and
            // answers `name()`, `ordinal()` and `values()` correctly while
            // `X.class.isEnum()` is false and `getEnumConstants()` is null.
            //
            // That was true of EVERY synthetic JDK enum, including
            // `PosixFilePermission`, which has been the model for the others
            // since it landed — measured 2026-09-02 with
            // `apps/probes/SyntheticEnumSurface` and `TuDiag`:
            //
            //     TimeUnit             isEnum=false getEnumConstants=null allOf=0
            //     DayOfWeek            isEnum=false getEnumConstants=null allOf=0
            //     PosixFilePermission  isEnum=false getEnumConstants=null allOf=0
            //     a user-defined enum  isEnum=true  getEnumConstants=2    allOf=2
            //
            // A real enum's class file carries `ACC_ENUM` and `ACC_FINAL`, and
            // `SUPER` is set on every modern class file.
            ClassAccessFlags::PUBLIC
                | ClassAccessFlags::SUPER
                | ClassAccessFlags::FINAL
                | ClassAccessFlags::ENUM
        } else if (name.contains("$") && !is_concrete_dollar_class)
            || name.ends_with("able")
            || is_known_jdk_interface
        {
            // Likely an interface (Serializable, Comparable, Iterable, etc.)
            ClassAccessFlags::PUBLIC | ClassAccessFlags::INTERFACE | ClassAccessFlags::ABSTRACT
        } else {
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER
        };

        let mut class = Class {
            id,
            loader_id: ClassLoaderId::Bootstrap,
            name: cratonvm_types::intern_arc(name),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded,
            initializing_thread: None,
            constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
            access_flags,
            superclass: superclass_id,
            interfaces: vec![], // populated after registration to avoid cycles
            fields,
            methods: synthetic_stub_ctor_methods(name),
            first_field_index: parent_fields,
            num_total_fields: parent_fields + num_instance,
            bootstrap_methods: vec![],
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Self::synthetic_inner_classes(name),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            // Set together with `is_synthetic_stub` by `set_origin` below.
            origin: ClassOrigin::default(),
            has_finalizer: false, // synthetic stubs don't override finalize()
            signature: None,
            code_source: None,
            array_info: None,
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
            record_object_methods: std::sync::atomic::AtomicU8::new(0),
        };
        // This is *the* forbidden origin: a stand-in for a class whose real
        // bytes were never found. `set_origin` raises the derived
        // `is_synthetic_stub` to `true`, matching the previous literal.
        // Not yet stored: the in-place provenance epoch stays put (wave 94).
        class.set_origin_of_unstored_class(ClassOrigin::compatibility_stub(stub_reason));

        debug!(
            class = %class.name,
            id = %id,
            superclass = ?superclass_id,
            methods = class.methods.len(),
            method_sigs = ?class
                .methods
                .iter()
                .map(|m| format!("{}{}", m.name, m.descriptor))
                .collect::<Vec<_>>(),
            "Synthetic stub class created",
        );

        // Round 4 audit fix: `name_to_id` shadow map removed
        // (loader-unaware + FNV-1a collision-unsafe). `loaded_classes`
        // is now the single name→id index.
        //
        // T10.9.E: clone the existing `Arc<str>` (refcount bump) instead
        // of allocating a fresh `String` for the map key.
        //
        // Round 7 audit fix (CRIT #2): the key hardcodes
        // `ClassLoaderId::Bootstrap` because synthetic JDK stubs are
        // bootstrap-loaded by construction. Assert that the `Class`
        // being inserted agrees, so a future contributor who extends
        // this path to user-defined loaders has to update both sides at
        // once instead of silently inserting a mis-keyed entry that
        // would shadow the real class in `loaded_classes`.
        debug_assert_eq!(
            class.loader_id,
            ClassLoaderId::Bootstrap,
            "synthetic class creation paths must use Bootstrap loader (got {:?} for {})",
            class.loader_id,
            class.name,
        );
        let key = (ClassLoaderId::Bootstrap, Arc::clone(&class.name));
        self.loaded_classes_insert(key, id);
        self.class_store.add(class);

        // Deferred interface resolution: now that this class is registered,
        // loading interface classes won't re-create it
        if !iface_names.is_empty() {
            let mut iface_ids = Vec::new();
            for iface_name in iface_names {
                if let Ok(iface_id) = self.load_class(iface_name) {
                    iface_ids.push(iface_id);
                }
            }
            // Through `set_interfaces`, not a raw `class.interfaces =` write:
            // the store's direct-implementor index has to see these edges, or
            // class-hierarchy analysis is blind to every `invokeinterface`
            // site these stubs can receive.
            if !iface_ids.is_empty() {
                self.class_store.set_interfaces(id, iface_ids);
            }
        }

        Ok(id)
    }

    /// The defining loader JVMS §5.3.3 assigns to an array class, given the
    /// `ClassId` of its **immediate component** (`None` for a primitive
    /// component).
    ///
    /// > If the component type is a `reference` type, the Java Virtual Machine
    /// > marks C to have the defining loader of the component type as its
    /// > defining loader. Otherwise, the Java Virtual Machine marks C to have
    /// > the bootstrap class loader as its defining loader.
    /// >
    /// > — JVMS SE 21 §5.3.3, step 2
    ///
    /// **Deliberate narrowing.** A component defined by one of the three
    /// *built-in* loaders is reported as `Bootstrap` rather than as
    /// `Extension`/`Application`. The built-in loaders form a single
    /// parent-first delegation chain (`BUILTIN_LOADER_DELEGATION_CHAIN`) that
    /// is probed in order by every lookup in this file, so they can never hold
    /// two distinct classes under one name — the isolation that §5.3.3 exists
    /// to protect is not at risk there. Widening this to the exact built-in
    /// loader would move the vast majority of array classes out of
    /// [`Self::find_bootstrap_class_by_name`]'s and
    /// `find_class_by_name_for_loader(_, Bootstrap)`'s reach (both stop at the
    /// requester and never delegate *down*), which is a behaviour change on the
    /// hottest lookup in the VM for no isolation benefit. Only a
    /// `UserDefined` component loader — the case that genuinely produces two
    /// same-named classes — yields a non-bootstrap array class.
    /// See `array-class-defining-loader.md`.
    pub(in crate::class_manager) fn array_defining_loader(&self, component_id: Option<ClassId>) -> ClassLoaderId {
        match component_id
            .and_then(|id| self.class_store.get(id))
            .map(|component| component.loader_id)
        {
            Some(loader @ ClassLoaderId::UserDefined(_)) => loader,
            _ => ClassLoaderId::Bootstrap,
        }
    }

    /// The `ClassId` filed under *exactly* `(loader_id, name)` — one hash
    /// probe, no delegation, no linear-scan fallback, no separator
    /// normalisation.
    ///
    /// This is [`Self::class_defined_by_loader_exact`] without its cold
    /// `ClassStore` scan, for callers that need an O(1) *read-lock* fast path
    /// and are content to treat a miss as "ask the slow path". The array-class
    /// resolver in `vm/src/runtime/interpreter.rs` is the motivating caller: it
    /// must not use a delegating probe (that is the collapse JVMS §5.3.3
    /// isolation exists to prevent) and must not pay an O(number of classes)
    /// scan on every array reference from a user-loader-defined class.
    pub fn loaded_class_under_exact_key(
        &self,
        name: &str,
        loader_id: ClassLoaderId,
    ) -> Option<ClassId> {
        loaded_classes_probe(&self.loaded_classes, loader_id, name)
    }

    /// Record a virtual class (a lambda proxy: not in the `ClassStore`) under its internal
    /// name, so array classes over it can be synthesised ([`Self::load_array_class_for_loader`]).
    pub fn register_virtual_component(&mut self, internal_name: &str, id: ClassId) {
        self.virtual_components
            .insert(internal_name.to_string(), id);
    }

    /// Load (synthesising on demand) the array class `name` **as
    /// `requesting_loader` would see it** — JVMS §5.3.3.
    ///
    /// This is the loader-faithful entry point the flat
    /// [`Self::load_class`] cannot be: `load_class` takes no loader, so its
    /// `[` arm can only ask as the bootstrap loader does. Two isolating
    /// loaders that each define their own `p/X` must get two *different*
    /// `[Lp/X;` classes, because each one's component is a different class;
    /// collapsing them is type confusion (the verifier and
    /// `Class.getComponentType()` both read array-class identity).
    ///
    /// The array's map key is derived from the resolved component, never from
    /// `requesting_loader` itself, so it is **canonical**: any two callers that
    /// resolve the same component `ClassId` land on the same array class, and
    /// callers that resolve different components land on different array
    /// classes. `requesting_loader` only chooses *which* component is resolved.
    ///
    /// Behaviour for a built-in `requesting_loader` is unchanged from the
    /// pre-existing bootstrap-only synthesis: such a loader can only reach the
    /// built-in delegation chain, whose components map back to `Bootstrap` via
    /// [`Self::array_defining_loader`].
    pub fn load_array_class_for_loader(
        &mut self,
        name: &str,
        requesting_loader: ClassLoaderId,
    ) -> Result<ClassId, VmError> {
        if !name.starts_with('[') {
            return Err(VmError::ClassFile(ClassFileError::InvalidClassFile {
                class_name: name.to_string(),
                message: format!("load_array_class_for_loader called with non-array name {name}"),
            }));
        }
        // Hot-path pre-probe, valid ONLY for a built-in requester. Such a
        // requester cannot reach a user-defined namespace, so if the bootstrap
        // key already holds this array name it is by construction the answer
        // this call would compute — identical to the pre-2026-08-01 behaviour,
        // at the same cost (one hash probe). A user-defined requester always
        // pays the full component derivation, because for it the bootstrap
        // entry may be *another* loader's array class.
        if !matches!(requesting_loader, ClassLoaderId::UserDefined(_)) {
            if let Some(id) =
                loaded_classes_probe(&self.loaded_classes, ClassLoaderId::Bootstrap, name)
            {
                return Ok(id);
            }
        }
        self.synthesize_array_class_for_loader(name, requesting_loader)
    }

    /// RKC16N.3 — Synthesise a reference- or primitive-array class without
    /// any classpath I/O.
    ///
    /// Per JVMS §5.3.3 an array class is created by the VM directly from its
    /// component type — no `.class` file is ever consulted — and is marked as
    /// defined by *the defining loader of that component type* (bootstrap for
    /// a primitive component). The synthesised `Class`:
    /// * has the original descriptor as its name (e.g. `[Ljava/util/HashMap;`,
    ///   `[I`, `[[Ljava/lang/Object;`),
    /// * has `superclass = java/lang/Object`,
    /// * implements `Cloneable` and `java.io.Serializable` (JLS §10.7),
    /// * is *not* marked `is_synthetic_stub` (it is a fully-formed array class,
    ///   not a stand-in for missing bytecode),
    /// * for reference-array types, recursively resolves the component class
    ///   so that `[[Ljava/util/HashMap;` triggers loading of
    ///   `[Ljava/util/HashMap;` and `java/util/HashMap`, and
    /// * records that component in `Class::array_info`, which is what makes the
    ///   array class's identity checkable rather than merely name-shaped.
    ///
    /// The result is cached in `loaded_classes` under
    /// `(component's defining loader, name)`, so two calls that resolve the
    /// same component return the same `ClassId`.
    pub(in crate::class_manager) fn synthesize_array_class_for_loader(
        &mut self,
        name: &str,
        requesting_loader: ClassLoaderId,
    ) -> Result<ClassId, VmError> {
        debug_assert!(
            name.starts_with('['),
            "synthesize_array_class called with non-array name {name}"
        );

        // Resolve the component class FIRST: the array's map key is derived
        // from it (JVMS §5.3.3), so there is nothing to probe the cache with
        // until it is known. We strip exactly one leading `[` and dispatch on
        // the next character:
        //   `[`  → another array; recurse with the SAME requesting loader, so
        //          `[[Lp/X;` inherits `p/X`'s loader through `[Lp/X;`.
        //   `L…;` → reference component, e.g. `Ljava/util/HashMap;`. Strip the
        //           leading `L` and trailing `;` and resolve the named class.
        //   else → primitive component (`I`, `J`, `Z`, `B`, `S`, `C`, `F`, `D`).
        //          Primitive component classes have no `Class<?>` mirror in the
        //          ClassStore yet (they are surfaced lazily by the VM's
        //          `Class.getPrimitiveClass`), so we leave them unresolved
        //          here — and §5.3.3 makes such an array bootstrap-defined
        //          anyway. Anything that needs the component class (e.g.
        //          `java.lang.Class.getComponentType()`) re-derives it from
        //          the array name.
        let rest = &name[1..];
        if rest.is_empty() {
            return Err(VmError::ClassFile(ClassFileError::InvalidClassFile {
                class_name: name.to_string(),
                message: "array descriptor with empty component".to_string(),
            }));
        }
        let component_id: Option<ClassId> = match rest.as_bytes()[0] {
            b'[' => {
                // Multi-dim array — synthesise the inner array first. The
                // inner array is itself a reference type, so §5.3.3's rule
                // recurses through it and `[[Lp/X;` ends up defined by the
                // same loader as `p/X`.
                Some(self.load_array_class_for_loader(rest, requesting_loader)?)
            }
            b'L' => {
                // Reference component: must end with ';'.
                if !rest.ends_with(';') || rest.len() < 3 {
                    return Err(VmError::ClassFile(ClassFileError::InvalidClassFile {
                        class_name: name.to_string(),
                        message: format!("malformed reference-array descriptor: {name}"),
                    }));
                }
                let component_name = rest[1..rest.len() - 1].to_string();
                // §5.3.3 step 1: "the algorithm of this section is applied
                // recursively **using L**". Ask the requesting loader first;
                // only when it has no answer at all does this fall back to the
                // flat, loader-blind `load_class`. Bubbling the error up
                // matches the JVMS rule that resolving an array class resolves
                // its element type first.
                match self.find_class_by_name_for_loader(&component_name, requesting_loader) {
                    Some(id) => Some(id),
                    // A virtual (lambda proxy) component has no class file and no store
                    // entry: its registered id is the component.
                    None => match self.virtual_components.get(component_name.as_str()) {
                        Some(id) => Some(*id),
                        None => Some(self.load_class(&component_name)?),
                    },
                }
            }
            b'Z' | b'B' | b'C' | b'S' | b'I' | b'J' | b'F' | b'D' => {
                // Primitive-array — nothing to recursively load.
                if rest.len() != 1 {
                    return Err(VmError::ClassFile(ClassFileError::InvalidClassFile {
                        class_name: name.to_string(),
                        message: format!("malformed primitive-array descriptor: {name}"),
                    }));
                }
                None
            }
            _ => {
                return Err(VmError::ClassFile(ClassFileError::InvalidClassFile {
                    class_name: name.to_string(),
                    message: format!("unrecognised array component tag in {name}"),
                }));
            }
        };

        // JVMS §5.3.3 step 2 — the defining loader, and therefore the map key.
        let array_loader = self.array_defining_loader(component_id);

        // Cache hit on the EXACT key. This is the only probe that is safe:
        // a name-only / delegating probe is precisely the collapse this
        // function exists to remove.
        if let Some(id) = loaded_classes_probe(&self.loaded_classes, array_loader, name) {
            // ...and the cached entry must still be an array over the component
            // we just resolved. A loader id is never recycled, but a *class* id
            // under a live loader is: `unload_user_classes` (the GC-driven
            // path) can retire `p/X` without retiring `[Lp/X;`, and that same
            // loader may then define a fresh `p/X`. Returning the cached array
            // there would hand back a class whose component is a tombstone —
            // aliasing two distinct `p/X`es onto one array class, which is the
            // exact confusion this function exists to prevent. Fall through and
            // re-synthesise instead; `loaded_classes_insert` displaces the
            // stale key and `ClassStore` keeps the dead array as an unreachable
            // tombstone, so no live reference is invalidated.
            let cached_component = self
                .class_store
                .get(id)
                .and_then(|class| class.array_info.as_ref())
                .map(|info| info.component_class_id);
            // `None` means a primitive-component array (no component to drift)
            // or an array class minted before `array_info` was populated; both
            // keep the pre-existing behaviour of trusting the cache.
            if cached_component.is_none() || cached_component == component_id {
                return Ok(id);
            }
        }

        // Migration: an array class filed under `(Bootstrap, name)` by the
        // legacy loader-blind path (or before its component was re-homed into
        // a user loader by `upgrade_synthetic_class`) is only *this* array
        // class if it was built over the very same component. `array_info`
        // records that component, so the question is answerable exactly rather
        // than by re-deriving the component by name — which is the ambiguous
        // lookup being removed here.
        //
        //   * same component  → re-key in place (identity must be preserved:
        //     `p/X[].class == p/X[].class`), mirroring the
        //     `upgrade_synthetic_class` re-home.
        //   * different component, or an array class that predates
        //     `array_info` and so cannot prove sameness → leave the bootstrap
        //     entry untouched and mint a distinct array class under the
        //     correct key. Aliasing two arrays over different components is
        //     the type confusion; a second `ClassId` is merely a duplicate,
        //     and the two are genuinely different classes in the case that
        //     matters.
        if array_loader != ClassLoaderId::Bootstrap {
            if let Some(existing) =
                loaded_classes_probe(&self.loaded_classes, ClassLoaderId::Bootstrap, name)
            {
                let same_component = self
                    .class_store
                    .get(existing)
                    .and_then(|class| class.array_info.as_ref())
                    .map(|info| info.component_class_id)
                    == component_id;
                if same_component {
                    let key_name = self
                        .class_store
                        .get(existing)
                        .map(|class| Arc::clone(&class.name))
                        .unwrap_or_else(|| cratonvm_types::intern_arc(name));
                    if let Some(class) = self.class_store.get_mut(existing) {
                        class.loader_id = array_loader;
                    }
                    // The defining loader is an input of `can_override`'s
                    // run-time package test, so a memo stamped with the
                    // hierarchy-edge epoch (the selection memo under
                    // `CRATONVM_INTERP_SELECT_MEMO_EDGE_EPOCH`) must see an
                    // in-place loader change (interpreter round i4 wave 94,
                    // lane L7). The insert below moves the definition epoch.
                    bump_hierarchy_edge_epoch(self.class_store.layout_domain());
                    // Only retire the stale alias when it still names THIS
                    // class — same guard as `upgrade_synthetic_class`.
                    let stale_alias_is_ours = loaded_classes_probe(
                        &self.loaded_classes,
                        ClassLoaderId::Bootstrap,
                        &key_name,
                    ) == Some(existing);
                    self.loaded_classes_insert((array_loader, Arc::clone(&key_name)), existing);
                    self.user_loaders.insert(array_loader);
                    if stale_alias_is_ours {
                        self.loaded_classes_remove(&(ClassLoaderId::Bootstrap, key_name));
                    }
                    // gci i12/ul F2: re-homed into a user namespace.
                    self.note_pending_user_array(existing);
                    return Ok(existing);
                }
            }
        }

        // Resolve `java/lang/Object` and the JLS §10.7 array interfaces.
        // These calls go through the normal `load_class` path (no array
        // recursion because none of these names start with `[`), so they
        // hit either the real classpath or `create_synthetic_stub` exactly
        // as they would for any other JDK class.
        let object_id = self.load_class("java/lang/Object")?;
        let mut iface_ids = Vec::with_capacity(2);
        if let Ok(id) = self.load_class("java/lang/Cloneable") {
            iface_ids.push(id);
        }
        if let Ok(id) = self.load_class("java/io/Serializable") {
            iface_ids.push(id);
        }

        // Belt-and-braces: the `load_class` calls above can re-enter this
        // function for the same `name` (a synthetic-stub fabrication for
        // `java/lang/Object` runs arbitrary registration code). Re-probe the
        // exact key before minting a second `ClassId` for it.
        if let Some(id) = loaded_classes_probe(&self.loaded_classes, array_loader, name) {
            return Ok(id);
        }

        let id = self.class_store.next_id();
        // An array `Class` is `final`, `public`, and has the `ACC_ABSTRACT`
        // bit cleared — same surface flags `java.lang.Class` reports for
        // `int[].class.getModifiers()`. We mark it `SUPER` for parity with
        // ordinary loaded classes; `FINAL` reflects that you cannot subclass
        // an array type.
        let access_flags =
            ClassAccessFlags::PUBLIC | ClassAccessFlags::FINAL | ClassAccessFlags::SUPER;

        // `ArrayInfo` inputs: total `[` count and the innermost non-array type.
        let array_dimension = name.bytes().take_while(|&b| b == b'[').count();
        let leaf_descriptor = &name[array_dimension..];
        let leaf_is_reference = leaf_descriptor.starts_with('L') && leaf_descriptor.ends_with(';');
        let leaf_component_name = if leaf_is_reference {
            cratonvm_types::intern_arc(&leaf_descriptor[1..leaf_descriptor.len() - 1])
        } else {
            cratonvm_types::intern_arc(leaf_descriptor)
        };

        // JDK-ONLY-NOTE (W4-2): an array class's module is its COMPONENT
        // type's module. This was a hardcoded `Some("java.base")` for every
        // array class regardless of component type, so
        // `MyAppClass[].class.getModule()` answered `java.base` where HotSpot
        // answers the unnamed module. Measured on Temurin 25.0.3:
        //
        //   int[].class.getModule()        module java.base
        //   String[].class.getModule()     module java.base
        //   ArrProbe[].class.getModule()   unnamed module @691a7f8f
        //
        // `Class.getModule()`'s javadoc says it directly: "If this class
        // represents an array type then this method returns the Module for
        // the element type." A primitive component has no `ClassId` in the
        // store (`component_id == None`) and its element type's module is
        // java.base, which is the one case the old hardcode got right. A
        // multi-dimensional array reaches here with `component_id` naming the
        // inner ARRAY class, whose own module was computed by this same rule
        // one recursion down, so `[[Lp/X;` inherits `p/X`'s module through
        // `[Lp/X;` exactly as it already inherits its defining loader.
        //
        // `None` is the unnamed module (see `Class::module_name`), so a
        // classpath component type produces an array class in the unnamed
        // module rather than one falsely claiming java.base.
        let array_module_name: Option<String> = match component_id {
            Some(component) => self
                .class_store
                .get(component)
                .and_then(|component_class| component_class.module_name.clone()),
            None => Some("java.base".to_string()),
        };

        let class = Class {
            id,
            loader_id: array_loader,
            name: cratonvm_types::intern_arc(name),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Initialized,
            initializing_thread: None,
            constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
            access_flags,
            superclass: Some(object_id),
            interfaces: iface_ids,
            fields: vec![],
            methods: vec![],
            // Array objects carry no Java-level instance fields — their
            // length and elements live in the array header maintained by
            // the GC, not in field slots.
            first_field_index: self
                .class_store
                .get(object_id)
                .map_or(0, |c| c.num_total_fields),
            num_total_fields: self
                .class_store
                .get(object_id)
                .map_or(0, |c| c.num_total_fields),
            bootstrap_methods: vec![],
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: array_module_name,
            // Crucially: an array class is NOT a synthetic stub — it is a
            // fully-formed array class produced by the VM itself.
            // Marking it stub would (a) emit a misleading log line and
            // (b) make `load_class` try to "upgrade" it from a non-existent
            // .class file on the next call.
            //
            // Contract §5: array classes get `VmArray`, never
            // `CompatibilityStub`, in **both** modes. JVMS §5.3.3 says the VM
            // creates them without consulting any class file, so calling an
            // array a compatibility substitution would make the
            // zero-compatibility-class acceptance criterion unsatisfiable by
            // construction. Set directly (not via `set_origin`) because the
            // derived bool is written in the same literal, one line down.
            origin: ClassOrigin::VmArray,
            has_finalizer: false,
            signature: None,
            code_source: None,
            // RKC16N.3 / C2 review P1: array-class metadata IS populated now,
            // and it has a consumer — the migration branch above reads
            // `component_class_id` to decide whether a pre-existing
            // bootstrap-keyed array class is the same class as this one. That
            // question cannot be answered by re-deriving the component from the
            // array *name*, because a name is exactly what does not identify a
            // class. `None` for a primitive component: `[I`'s component has no
            // `ClassId` in the store, and §5.3.3 makes such an array bootstrap-
            // defined regardless, so there is nothing to witness.
            array_info: component_id.map(|component_class_id| ArrayInfo {
                component_class_id,
                // Widening guard: an array descriptor is capped at 255
                // dimensions by JVMS §4.4.1, and the reader rejects deeper
                // ones, so this cannot truncate for a well-formed name.
                array_dimension: array_dimension.min(u8::MAX as usize) as u8,
                leaf_component_name,
            }),
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
            record_object_methods: std::sync::atomic::AtomicU8::new(0),
        };

        debug!(
            class = %class.name,
            id = %id,
            superclass = ?class.superclass,
            "Synthesised array class (RKC16N.3)",
        );

        // Round 4 audit fix: `name_to_id` shadow map removed (loader-
        // unaware + FNV-1a collision-unsafe). `loaded_classes` is now
        // the single name→id index.
        //
        // T10.9.E: clone the existing `Arc<str>` (refcount bump) instead
        // of allocating a fresh `String` for the map key.
        //
        // C2 review P1 (2026-08-01) — this used to `debug_assert_eq!` that
        // `class.loader_id == Bootstrap`, citing "Round 7 audit fix (CRIT #2):
        // per JVMS §5.3.3, array classes are *created* by the bootstrap class
        // loader regardless of the component type's defining loader."
        //
        // That reading of §5.3.3 was **wrong**, and the assertion pinned the
        // bug in place. §5.3.3 step 2 says, verbatim:
        //
        //   "If the component type is a reference type, the Java Virtual
        //    Machine marks C to have the defining loader of the component type
        //    as its defining loader. Otherwise, the Java Virtual Machine marks
        //    C to have the bootstrap class loader as its defining loader."
        //
        // What §5.3.3 actually says about the bootstrap loader is that no
        // *class file* is consulted — the array class is created by the VM, not
        // found by a loader. That is a statement about class **creation**, not
        // about the **defining loader**, and the old comment conflated the two.
        //
        // The invariant the assertion was protecting — that `Class::loader_id`
        // and the `loaded_classes` key never disagree — is real, and is
        // preserved: both now come from the single `array_loader` binding.
        debug_assert_eq!(
            class.loader_id, array_loader,
            "array class {} must be keyed under its own defining loader",
            class.name,
        );
        let key = (array_loader, Arc::clone(&class.name));
        self.loaded_classes_insert(key, id);
        // Keep the context-free probe (`get_loaded_class_id`,
        // `find_class_by_name`) able to reach a user-loader-defined array
        // class, exactly as `upgrade_synthetic_class` does for a re-homed
        // class. `user_loaders` is a set of loader ids; inserting a built-in
        // id would make the built-in chain get scanned twice, so gate it.
        if matches!(array_loader, ClassLoaderId::UserDefined(_)) {
            self.user_loaders.insert(array_loader);
        }
        self.class_store.add(class);
        // gci i12/ul F2: the VM gives a user namespace's array class its
        // element's defining-loader row (`take_pending_user_arrays`).
        if matches!(array_loader, ClassLoaderId::UserDefined(_)) {
            self.note_pending_user_array(id);
        }

        Ok(id)
    }

    /// gci i12/ul F2 (`gci-i12ul-class-unloading-and-side-tables-on-g1-review-findings-RETIRED-UNFIXED-20261018`):
    /// queue an array class synthesised in (or re-homed into) a USER namespace.
    /// It has no defining-loader row of its own -- those come from
    /// `defineClass` -- so the VM drains this queue
    /// ([`Self::take_pending_user_arrays`]) at every root scan and registers the
    /// element's loader for it: its instances then keep that loader alive, its
    /// mirror is deferred to it, and it unloads with it. Per VM (a field of
    /// this manager), bounded by the drain.
    pub(in crate::class_manager) fn note_pending_user_array(&self, id: ClassId) {
        self.pending_user_arrays
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(id);
    }

    /// gci i12/ul F2: take every user-namespace array class queued since the
    /// last call (see [`Self::note_pending_user_array`]). Needs only a shared
    /// borrow, so a root scan drains it under the manager's READ lock.
    pub fn take_pending_user_arrays(&self) -> Vec<ClassId> {
        std::mem::take(
            &mut *self
                .pending_user_arrays
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    /// gci i12/ul F4: the classes `ClassLoadingMXBean.getLoadedClassCount`
    /// counts, as HotSpot counts them (`ClassLoadingService`, InstanceKlasses):
    /// every live class but the synthesised ARRAY classes and the
    /// compatibility stubs (no HotSpot class stands behind a stub). Hidden
    /// classes count, as on HotSpot. One walk of the store: a JMX call, never a
    /// hot path. [`Self::loaded_count`] (every live class) is unchanged.
    pub fn loaded_instance_class_count(&self) -> usize {
        self.class_store
            .iter()
            .filter(|class| counts_as_loaded_instance_class(&class.name, &class.origin))
            .count()
    }

    /// Upgrade a synthetic stub class to a real class loaded from bytecode.
    ///
    /// Reuses the existing ClassId so that existing references (field indices,
    /// type checks, etc.) remain valid. The class's methods, fields, constant
    /// pool, and other metadata are replaced with those from the real class file.
    ///
    /// The `first_field_index` and `num_total_fields` are recomputed, but we
    /// preserve `max(old, new)` for `num_total_fields` so that objects already
    /// allocated with the synthetic layout don't underflow.
    pub(in crate::class_manager) fn upgrade_synthetic_class(
        &mut self,
        id: ClassId,
        name: &str,
        bytes: SharedBytes,
        loader_id: ClassLoaderId,
    ) -> Result<(), VmError> {
        use cratonvm_reader::attribute::Attribute;

        let mut class_file = cratonvm_reader::read_class_shared(bytes.clone()).map_err(|e| {
            VmError::ClassFile(ClassFileError::InvalidClassFile {
                class_name: name.to_string(),
                message: e.to_string(),
            })
        })?;

        // Same rationale as `define_class_with_options`: each class-level
        // attribute consumed below populates the runtime `Class` directly,
        // so eager decode is the cheapest path and lets the rest of this
        // upgrade path use plain `Attribute` pattern matching.
        force_decode_all(&mut class_file.attributes, &class_file.constant_pool).map_err(|e| {
            VmError::ClassFile(ClassFileError::InvalidClassFile {
                class_name: name.to_string(),
                message: format!("attribute decode failed: {e}"),
            })
        })?;
        // Method- and field-level attributes too, as `define_class_with_options`
        // does ("Round-2 fix (CRIT)"): `ClassFileMethod::code()` answers only a
        // DECODED `Code` attribute. Without this every upgraded class's
        // methods read as having no `Code`, so linking one that is verified
        // failed with `VerifyError: <init>: non-abstract non-native method
        // must have Code attribute` (interpreter round i2 wave 61, lane L5;
        // probe `L5W61StubUpgradeInit`, row `first-other-thread`).
        for method in class_file.methods.iter_mut() {
            force_decode_all(&mut method.attributes, &class_file.constant_pool).map_err(|e| {
                VmError::ClassFile(ClassFileError::InvalidClassFile {
                    class_name: name.to_string(),
                    message: format!("method '{}' attribute decode failed: {e}", method.name),
                })
            })?;
        }
        for field in class_file.fields.iter_mut() {
            force_decode_all(&mut field.attributes, &class_file.constant_pool).map_err(|e| {
                VmError::ClassFile(ClassFileError::InvalidClassFile {
                    class_name: name.to_string(),
                    message: format!("field '{}' attribute decode failed: {e}", field.name),
                })
            })?;
        }
        // zr z11/md: as the define path (`CRATONVM_CLASS_CODE_COMPACT`).
        if loader_flags().class_code_compact {
            compact_member_attribute_views(&mut class_file, &bytes);
        }

        // Load superclass (may already be loaded). `super_class` is now
        // `Option<Arc<str>>`; deref for the `&str` parameter.
        //
        // W7-26 — these two were `.ok()` and `filter_map(… .ok())`, and both
        // laundered a supertype resolution failure into a WRONG LAYOUT rather
        // than into a failed upgrade:
        //
        //   * `superclass_id = None` says "this class has no superclass" when
        //     its own class file says it has one, and `compute_field_layout`
        //     below reads exactly that bit — so every inherited field slot
        //     collapses and `first_field_index` becomes 0. That is the
        //     slot-index species (`docs/architecture/natives-over-real-jdk-classes.md`
        //     §5: a slot index against a real layout is heap corruption), not a
        //     missing diagnostic.
        //   * `filter_map` silently SHORTENS the interface list, so
        //     `instanceof` answers false for a type the class file declares and
        //     the itable is built one entry short.
        //
        // Neither has a fallback to fall through to, and
        // `define_class_with_options` — the sibling that defines the same
        // class from the same bytes — propagates both (`return Err(e)` on its
        // `resolve_supertype` and on `resolved_interfaces`). This function was
        // the outlier. All three callers already handle the `Err`: two log it
        // at `debug` and keep the un-upgraded stub, which is strictly better
        // than installing a stub whose layout claims a hierarchy it does not
        // have, and the third (`define_class_with_options`) propagates.
        //
        // The `loading_guard` entry MUST be removed on every exit or the name
        // is permanently seen as circular by `load_class`; that is why these
        // are written as explicit `match`es rather than `?`.
        self.loading_guard.insert(name.to_string());
        let superclass_id = match class_file.super_class {
            Some(ref super_name) => match self.load_class(&**super_name) {
                Ok(id) => Some(id),
                Err(e) => {
                    self.loading_guard.remove(name);
                    return Err(e);
                }
            },
            None => None,        };

        // Load interfaces. `iface_name: &Arc<str>` derefs to `&str`.
        let mut interface_ids: Vec<ClassId> = Vec::with_capacity(class_file.interfaces.len());
        for iface_name in class_file.interfaces.iter() {
            match self.load_class(iface_name) {
                Ok(id) => interface_ids.push(id),
                Err(e) => {
                    self.loading_guard.remove(name);
                    return Err(e);
                }
            }
        }
        self.loading_guard.remove(name);

        // Compute field layout from real class file
        let (first_field_index, num_total_fields) =
            compute_field_layout(&class_file.fields, superclass_id, &self.class_store);

        // Wave 3-B (RE.4): pad to the synthetic stub field count when defined
        // (mirrors `define_class_with_options`, through the same helper so the
        // two cannot disagree about the floor or about who is exempt from it).
        // Required for classes that are upgraded from a synthetic stub but
        // whose real bytecode field count is smaller than the synthetic-mode
        // layout used by native helpers.
        let num_total_fields = self.apply_synthetic_floor(name, superclass_id, num_total_fields);

        // Extract source file. Inlined here (rather than
        // `class_file.source_file()`) so we pattern-match through
        // `LazyAttribute::as_decoded()`. Every entry was already decoded by
        // the `force_decode_all` call above, so `as_decoded()` returns
        // `Some` for every attribute and the `_ => None` arm only fires for
        // attributes of a different kind.
        // `Attribute::SourceFile(Arc<str>)` (round 4 reader) — materialise
        // into the `Option<String>` shape `class.source_file` expects.
        let source_file = class_file
            .attributes
            .iter()
            .find_map(|a| match a.as_decoded() {
                Some(Attribute::SourceFile(name)) => Some(name.to_string()),
                _ => None,
            });

        // Extract bootstrap methods
        let bootstrap_methods = class_file
            .attributes
            .iter()
            .find_map(|a| match a.as_decoded() {
                Some(Attribute::BootstrapMethods(bms)) => Some(bms.clone()),
                _ => None,
            })
            .unwrap_or_default();

        // Extract annotations (reflection-visible only: skip
        // RuntimeInvisibleAnnotations / @Retention(CLASS) — see
        // gap-annotation-retention-policy.md).
        let mut annotations = Vec::new();
        for attr in &class_file.attributes {
            match attr.as_decoded() {
                Some(Attribute::RuntimeVisibleAnnotations(anns)) => {
                    annotations.extend(anns.iter().cloned());
                }
                _ => {}
            }
        }

        // Extract Signature — `Attribute::Signature(Arc<str>)`; convert
        // to the `Option<String>` shape `class.signature` expects.
        let signature = class_file
            .attributes
            .iter()
            .find_map(|a| match a.as_decoded() {
                Some(Attribute::Signature(s)) => Some(s.to_string()),
                _ => None,
            });

        // NestHost, NestMembers, Record, PermittedSubclasses, InnerClasses,
        // EnclosingMethod: the class file's, not the stub's literal
        // (`create_synthetic_stub`: no nest, no record, a name-guessed
        // InnerClasses). Before interpreter round i1 wave 27 (lane L5) an
        // upgraded `Outer$Inner` kept `nest_host: None`, so its private
        // accesses to `Outer` failed `are_nestmates` (a false entry in the
        // `--compatible` member-access census), and an upgraded record or
        // sealed class answered `isRecord()` / `isSealed()` false. A stub is
        // never hidden, so the class file's `NestHost` stands as the define
        // path's does for an ordinary class.
        let nesting = NestingAttributes::of(&class_file.attributes, &class_file.constant_pool);

        // Preserve the larger num_total_fields so existing objects don't break
        let (old_first_field_index, old_num_total) = self
            .class_store
            .get(id)
            .map(|c| (c.first_field_index, c.num_total_fields))
            .unwrap_or((0, 0));
        let final_num_total = num_total_fields.max(old_num_total);

        // C2 review P1 (classloading identity): the defining loader recorded on
        // the `Class` is about to change (see `class.loader_id = loader_id`
        // below), and `loaded_classes` is keyed on `(defining loader, name)`.
        // Capture the key this class is currently filed under so the map can be
        // re-keyed in the same transaction — without that, the two disagree and
        // a `(built-in loader, name)` entry keeps naming a class whose defining
        // loader is a user-defined one.
        let previous_loader_id = self.class_store.get(id).map(|c| c.loader_id);
        let registered_name: Option<Arc<str>> =
            self.class_store.get(id).map(|c| Arc::clone(&c.name));

        // Contract §5: "The in-place 'upgrade a stub to real' path must call
        // `set_origin`." Real bytes have just replaced the fabricated skeleton,
        // so the class stops being a compatibility stub and becomes whatever
        // the loader that supplied those bytes makes it. Computed out here
        // because both inputs need `&self` while the update below holds
        // `&mut self.class_store`.
        //
        // `source` is the classpath entry the bytes were actually found on,
        // which is what the census wants to report; fall back to the class name
        // when the finder cannot name an entry (CDS cache hit, in-memory jar).
        let upgrade_source: Arc<str> = self
            .find_class_source_path(name)
            .map(Arc::<str>::from)
            .unwrap_or_else(|| Arc::<str>::from(name));
        let upgrade_module: Option<Arc<str>> = self
            .module_registry
            .module_for_package(package_of(name))
            .map(Arc::<str>::from);
        let upgraded_origin = match loader_id {
            ClassLoaderId::Bootstrap => ClassOrigin::BootImage {
                module: upgrade_module,
                source: upgrade_source,
            },
            ClassLoaderId::Extension | ClassLoaderId::Application => {
                ClassOrigin::ApplicationClassPath {
                    source: upgrade_source,
                }
            }
            ClassLoaderId::UserDefined(_) => ClassOrigin::UserDefined {
                loader: loader_id,
                source: Some(upgrade_source),
            },
        };

        // Update the class in-place.
        //
        // The superclass goes through `ClassStore::set_superclass` rather than
        // the `get_mut` block below: the store keeps a direct-subclass
        // adjacency index (see `ClassStore::descendants_of`), and a stub minted
        // under one superclass whose real bytecode names another must move its
        // edge, not just its field. A raw `class.superclass = ...` here would
        // leave `recompute_subclass_layouts` blind to this class.
        self.class_store.set_superclass(id, superclass_id);
        // The interface list goes through `ClassStore::set_interfaces` for
        // exactly the same reason on the other index: a stub minted with no
        // interfaces (which is the common case — the deferred-resolution paths
        // above only populate a curated few) whose real bytecode names several
        // must create those edges, not just overwrite the field. A raw
        // `class.interfaces = ...` here would leave class-hierarchy analysis
        // blind to every `invokeinterface` site this class can receive, which
        // is most of them: a real-JDK boot upgrades thousands of stubs through
        // this path.
        self.class_store.set_interfaces(id, interface_ids);
        // See `ClassManager::empty_record_classes` (i4 wave 84, lane L3).
        if nesting.has_record_attribute && nesting.record_components.is_empty() {
            self.empty_record_classes.insert(id);
        } else {
            self.empty_record_classes.remove(&id);
        }
        // The stub's finalizer bit, for the subclass propagation below (JIT
        // round 21 wave 22, lane hidden-resid).
        let old_has_finalizer = self.class_store.get(id).is_some_and(|c| c.has_finalizer);
        if let Some(class) = self.class_store.get_mut(id) {
            class.source_file = source_file;
            class.version = class_file.version;
            class.constant_pool = class_file.constant_pool;
            class.access_flags =
                ClassAccessFlagsBits::from_bits_retain(hotspot_stored_class_flags(
                    class_file.access_flags.bits(),
                    class_file.version.major,
                ));
            class.fields = class_file.fields;
            class.methods = class_file.methods;
            class.first_field_index = first_field_index;
            class.num_total_fields = final_num_total;
            class.bootstrap_methods = bootstrap_methods;
            class.annotations = annotations;
            class.signature = signature;
            class.nest_host = nesting.nest_host;
            class.nest_members = nesting.nest_members;
            class.record_components = nesting.record_components;
            class.permitted_subclasses = nesting.permitted_subclasses;
            class.inner_classes =
                hotspot_stored_inner_classes(nesting.inner_classes, class_file.version.major);
            class.enclosing_method = nesting.enclosing_method;
            // JVMS 5.3 runtime-package identity bug fix: the synthetic
            // stub this class started as was minted with SOME default
            // `loader_id` (e.g. `ClassLoaderId::Bootstrap`, see
            // `create_synthetic_stub`), which is frequently NOT the
            // loader that actually supplied the real bytecode we just
            // parsed -- `loader_id` (this fn's parameter) came straight
            // out of `find_class_bytes_delegated`'s parent-delegation
            // search (bootstrap -> extension -> application) and is
            // authoritative for where these bytes were actually found.
            // Every other field below is refreshed from the real class
            // file on upgrade; `loader_id` was the one exception, left
            // permanently wrong (e.g. stuck at `Bootstrap` for an
            // ordinary Application-classpath class). That divergence
            // was invisible until a same-runtime-package check
            // (JVMS 5.3: defining loader + package name) started
            // comparing `loader_id` across classes that are, from
            // Java's perspective, defined by the very same
            // `ClassLoader` object -- see `check_class_access` /
            // `same_runtime_package` in `access_control.rs`.
            class.loader_id = loader_id;
            // `set_origin` clears the derived `is_synthetic_stub` in the same
            // write as it installs the real provenance, so the two can never
            // disagree here. A stale `true` is precisely what makes real bytes
            // keep losing to a fabricated stand-in.
            class.set_origin(upgraded_origin);
            // Reset *both* initialization representations so verification and
            // the real `<clinit>` run after a stub-to-bytecode upgrade.  The
            // dispatch fast path reads `init_state` before inspecting
            // `ClassState`; leaving the stub's Initialized value there makes
            // it return early even though this real class has never executed
            // its initializer.  That leaked stale static slots through
            // `Collections.emptyList()` during Spring Boot bootstrap.
            class.state = ClassState::Loaded;
            // A thread linking or initializing the STUB loses its claim here:
            // its finalize sees the claim gone and publishes nothing, and its
            // caller starts over on the real class (`vm_util::finalize_class_init`,
            // interpreter round i2 wave 62, lane L5). Its waiters are released
            // by it or by the next claim, which takes over their entry.
            class.initializing_thread = None;
            let previous_init_state = class.init_state.swap(
                CLASS_INIT_UNINITIALIZED,
                std::sync::atomic::Ordering::AcqRel,
            );
            // Every lock-free memo of "initialized" must forget it too: the
            // VM's bitmap bit and each thread's memo entry (see
            // `ClassInitBits`). Only a stub that was not UNINITIALIZED can be
            // remembered anywhere; an upgrade before first use (most of them)
            // retires nothing.
            if previous_init_state != CLASS_INIT_UNINITIALIZED {
                self.class_init_bits.note_init_reset(id.as_u32());
            }

            // Compute has_finalizer (the parent half is applied below, by the
            // same rule: wave 17 / zt t18/hunt2, `Self::computed_has_finalizer`;
            // fields read directly, `class` borrows the store mutably).
            class.has_finalizer = self.finalization_enabled
                && class_has_finalizer_by_hotspot_rule(
                    class,
                    false,
                    self.empty_finalizer_not_finalizable && self.finalizer_empty_is_none,
                );
        }
        // C2 review P1 (classloading identity): re-key `loaded_classes` to match
        // the defining loader just installed above.
        //
        // A synthetic stub is always minted under `ClassLoaderId::Bootstrap`
        // (`create_synthetic_stub` asserts it; array classes are NOT stubs and
        // since the §5.3.3 fix are keyed under their component's defining
        // loader — see `synthesize_array_class_for_loader`), so a stub is filed
        // in `loaded_classes` under `(Bootstrap, name)`. This
        // path then re-homes the `Class` to whichever loader actually supplied
        // the real bytes — including, via `define_class_with_options`'s
        // "upgrade the stub instead of minting a shadowed second ClassId" arm,
        // a `ClassLoaderId::UserDefined`. Every other field was refreshed; the
        // map key was not, leaving two defects:
        //
        //   * `loaded_classes` and `ClassStore` disagreed about the defining
        //     loader, so `class_defined_by_loader_exact(name, user_loader)`
        //     missed its O(1) index and only answered through the cold
        //     linear-scan fallback.
        //   * far worse, `(Bootstrap, name) -> id` kept resolving for
        //     bootstrap/extension/application-initiated lookups
        //     (`get_loaded_class_id`, `find_class_by_name_for_loader`,
        //     `find_bootstrap_class_by_name`), i.e. a built-in loader
        //     delegating DOWN into a user-defined loader's namespace. That is
        //     the isolation break every other lookup in this file is written to
        //     prevent (`built_in_lookup_is_parent_first_and_never_delegates_down`):
        //     JDK code and an unrelated loader would share one `ClassId` for a
        //     class only one of them defined, and a second user loader defining
        //     the same name would then be silently outranked by the stale
        //     built-in alias instead of reading as ambiguous.
        //
        // Deliberately narrow: only a re-home INTO a user-defined loader is
        // corrected here. Built-in -> built-in re-homing (the ordinary "stub
        // for a JDK class, real bytes later found on the application
        // classpath" case) is left exactly as it was. The built-in chain is
        // walked parent-first at lookup time, so `(Bootstrap, name)` there is a
        // legitimate initiating-loader record for a name the bootstrap loader
        // really did initiate; and *adding* the `(Application, name)` defining
        // key would start making a subsequent `define_class` for that pair
        // report a duplicate-define `LinkageError` where it currently mints a
        // second copy. That is arguably the JVMS-correct outcome, but it is a
        // behaviour change on the hottest path in the VM and is out of scope
        // here — see `classloading-identity-audit.md`.
        if let (Some(previous_loader_id), Some(registered_name)) =
            (previous_loader_id, registered_name)
        {
            let rehomed_to_user_loader = previous_loader_id != loader_id
                && matches!(loader_id, ClassLoaderId::UserDefined(_));
            if rehomed_to_user_loader {
                // Only retire the old key when it still names THIS class: a
                // concurrent define may already have reassigned it, and
                // removing someone else's entry would be a second, worse bug.
                let stale_alias_is_ours = loaded_classes_probe(
                    &self.loaded_classes,
                    previous_loader_id,
                    &registered_name,
                ) == Some(id);
                let stale_alias_is_builtin =
                    !matches!(previous_loader_id, ClassLoaderId::UserDefined(_));
                self.loaded_classes_insert((loader_id, Arc::clone(&registered_name)), id);
                self.user_loaders.insert(loader_id);
                if stale_alias_is_ours && stale_alias_is_builtin {
                    self.loaded_classes_remove(&(previous_loader_id, registered_name));
                }
            }
        }

        // Check parent's has_finalizer (needs separate borrow). zt t18/hunt2:
        // the whole HotSpot rule (`Self::computed_has_finalizer`), which can
        // also turn the own-declaration answer above OFF (an empty `finalize()`,
        // `--finalization=disabled`).
        let parent_has = superclass_id
            .and_then(|sid| self.class_store.get(sid))
            .map_or(false, |parent| parent.has_finalizer);
        let has_finalizer = self
            .class_store
            .get(id)
            .map(|class| self.computed_has_finalizer(class, parent_has));
        if let Some(has_finalizer) = has_finalizer {
            if let Some(class) = self.class_store.get_mut(id) {
                class.has_finalizer = has_finalizer;
            }
        }
        // A subclass loaded while this class was a stub inherited the stub's
        // bit (a stub declares no `finalize`); HotSpot has no stubs, so its
        // subclass is finalizable exactly when the real parent is
        // (`CRATONVM_STUB_UPGRADE_FINALIZER_TO_SUBCLASSES`, default ON, `=0`
        // leaves the subclasses' bits as they were; JIT round 21 wave 22, lane
        // hidden-resid; item 8 of `r21-review-w18-findings-FIXED-20261020.md`).
        //
        // JIT round 21 wave 23 (lane finvm): every class whose bit moved -- the
        // upgraded class and each subclass -- is queued for the VM
        // (`stub_upgrade_finalizer_flips`), which runs its define-time
        // finalizer listener and drops its allocation recipe after the write
        // guard releases (`ClassManagerWriteGuard::drop`).
        let new_has_finalizer = self.class_store.get(id).is_some_and(|c| c.has_finalizer);
        if self.stub_upgrade_finalizer_to_subclasses && new_has_finalizer != old_has_finalizer {
            self.stub_upgrade_finalizer_flips.push((id, new_has_finalizer));
            for cid in self.propagate_finalizer_to_subclasses(id) {
                let has = self.class_store.get(cid).is_some_and(|c| c.has_finalizer);
                self.stub_upgrade_finalizer_flips.push((cid, has));
            }
        }

        // Propagate the layout change to already-loaded subclasses.
        //
        // A synthetic stub for an Abstract* JDK class (e.g.
        // `java/util/AbstractList`) is created with the minimal field
        // layout from `synthetic_stub_fields` — usually ZERO instance
        // fields. A subclass loaded from real bytecode *while the parent
        // is still a stub* (e.g. Scala's
        // `JavaCollectionWrappers$SeqWrapper extends java.util.AbstractList`)
        // computes its own `first_field_index` / `num_total_fields` from
        // that too-small parent count.
        //
        // When the real `AbstractList` bytecode is later loaded, this
        // upgrade path grows the parent's `num_total_fields` (real
        // `AbstractList` has the `modCount` field). Without propagation
        // the already-loaded subclass keeps its stale layout: its
        // `num_total_fields` undercounts the true field total and its
        // own fields overlap the parent's. `getfield`/`putfield` then
        // resolve a field index that exceeds the object's allocated slot
        // count (the GC guard reports
        // "out-of-bounds field read ... undersized object layout").
        //
        // Fix: if the upgrade changed this class's layout, recompute the
        // field layout of every transitive subclass. `ClassId`s are
        // assigned in load order and a subclass is always loaded *after*
        // its superclass, so a single forward pass by ascending id
        // visits every class after its (already-recomputed) parent —
        // a valid topological order for the superclass relation.
        if old_first_field_index != first_field_index || old_num_total != final_num_total {
            self.recompute_subclass_layouts(id);
        }
        // Compact ref-field layout: the upgrade replaced this class's field
        // descriptors (stub -> real bytecode), so its offset table / oop-map
        // must be rebuilt even when the field *count* is unchanged.
        self.class_store.register_compact_layout_if_enabled(id);

        // Unlike JVMTI `redefine_class` (JEP 109 forbids field-layout
        // changes there), this upgrade path CAN change instance field
        // count/order/offsets outright: the stub's placeholder descriptors
        // are replaced by the real class file's. A method already
        // JIT-compiled against the stub's layout has the stub's field
        // offsets (`compact_field_off`, or the legacy `field_index *
        // SLOT_SIZE` cell offset) baked directly into its machine code as
        // immediates; nothing else here evicted that code, so it went on
        // reading/writing the WRONG byte offset of any object allocated
        // under the new (post-upgrade) layout — a stale-offset getfield
        // could silently return whatever raw bytes sat at the old offset
        // (e.g. a small int) where a reference was expected, corrupting
        // anything computed from that value. Found while investigating the
        // guarded-inline-getfield SIGSEGV cluster
        // (docs/known-issues/elasticsearch-suite/ES-HANG-20260709-*); that
        // specific SIGSEGV's actual root cause turned out to be a different,
        // already-fixed bug (the fabricated-(0,false)-compact-slot issue,
        // see `be7102344` / the WildFly Host Controller fix), but this
        // invalidation gap is real and independent of it — nothing else in
        // the VM ever evicted JIT code after a layout-changing synthetic-
        // stub upgrade. Mirror `redefine_class`'s own Step 8 and fire the
        // same hook.
        fire_jit_invalidate_hook(id.as_u32(), self.class_store.layout_domain());

        // C2 review P1: this is the *other* identity-invalidating event, and
        // it is strictly worse than a JEP 109 redefine — the stub's methods,
        // constant pool AND instance-field layout are all replaced, so a
        // memoized `(class_id, method_index)` or `(class_id, field_index)`
        // pair now names something else. `redefine_generations` is NOT bumped
        // on this path (it counts JVMTI redefinitions only), which is exactly
        // why the realm keeps its own counter.
        self.metadata_realm.bump_generation(id);

        // The upgrade replaced the constant pool and may have shifted
        // field indices for this class and its subclasses; drop any
        // cached `(referring-class, cp-index) -> ResolvedField` entries
        // resolved against the stale layout. Mirrors the invalidation
        // `redefine_class` does for in-place bytecode replacement.
        fire_resolution_invalidate_hook(
            id.as_u32(),
            self.class_store.layout_domain(),
            ResolutionInvalidation::ConstantPoolReplaced,
        );

        // NO vtable rebuild here, unlike `redefine_class` step 6 — and that is
        // sound, not an oversight. Investigated 2026-08-01 off the back of the
        // JEP 109 sibling bug (`refresh_inherited_vtable_descriptors`); the two
        // properties that make it safe are pinned by
        // `synthetic_stub_has_no_vtable_and_no_dispatchable_body`:
        //
        //   1. A compatibility stub NEVER has a vtable. Both mint paths
        //      (`create_synthetic_stub` and `fabricate_class`) push straight
        //      into `class_store` without calling `build_vtable_descriptors` or
        //      firing the install hook, so `execute_invokevirtual_vtable_fast`
        //      always answers `CacheMiss` for a stub receiver. There is no
        //      stub-derived vtable entry this upgrade could leave stale.
        //   2. A stub's methods are all `ACC_NATIVE` with no attributes
        //      (`synthetic_stub_ctor_methods`), so they carry no Code — no
        //      `Arc<CachedBytecodeMethod>` can be built from a stub body, and
        //      therefore no stale *bytecode* snapshot can be sitting in a
        //      thread's invoke cache or the promoted-invoke map either. That
        //      matters because this path deliberately does NOT bump
        //      `redefine_generations`, so `RedefineGate` would never evict one.
        //
        // What the missing rebuild costs is dispatch COVERAGE, never
        // correctness: the upgraded class still has no vtable afterwards, and a
        // subclass linked while its parent was a stub seeded its own descriptor
        // vec from the parent's absent one. Both simply miss `lookup_slot` and
        // fall to the (correct) slow path. Not worth a broad dispatch change on
        // spec — a `CRATONVM_DBG_SYNUPGRADE` census over a Spring Boot JUnit
        // class saw this branch's guard evaluated 6277 times and the upgrade
        // itself taken ZERO times: every stub in the process is a VM-internal
        // shape (`cratonvm/internal/*`, `java/util/HashMap$KeyItr`,
        // `java/lang/annotation/AnnotationProxy`, `Proxy$Instance`, …) whose
        // real bytes exist nowhere, so 3190 of 3205 stub-hits short-circuit on
        // the known-absent memo and the remaining 15 scan and find nothing.

        // Cache the class bytes (FIFO-bounded helper).
        self.insert_class_bytes(id, bytes);

        // L4 — the other path by which a modelled class acquires a real layout:
        // a stub that was already minted is replaced in place by the real bytes.
        // The ClassId is deliberately reused, so every native holding a
        // positional index for the old model now addresses the new one.
        self.report_shadow_layout(id);
        self.check_safe_positional_claims(id);

        Ok(())
    }

    /// Take the `(class, new has_finalizer)` rows stub upgrades queued since
    /// the last take (parents before children), leaving none. The VM's
    /// write-guard release takes them (`ClassManagerWriteGuard::drop`) and,
    /// with no class-manager lock held, runs the define-time finalizer
    /// listener for each class that became finalizable and drops each moved
    /// class's published allocation recipe. Empty, and allocation-free, on
    /// every release whose section upgraded nothing. JIT round 21 wave 23,
    /// lane finvm (`CRATONVM_STUB_UPGRADE_FINALIZER_TO_SUBCLASSES`; item 8 of
    /// `r21-review-w18-findings-FIXED-20261020.md`).
    pub fn take_stub_upgrade_finalizer_flips(&mut self) -> Vec<(ClassId, bool)> {
        if self.stub_upgrade_finalizer_flips.is_empty() {
            return Vec::new();
        }
        std::mem::take(&mut self.stub_upgrade_finalizer_flips)
    }

    /// Whether [`Self::take_stub_upgrade_finalizer_flips`] would answer a row:
    /// the release asks first, so a store no VM owns yet keeps its rows for
    /// the first release after the VM registers.
    pub fn has_stub_upgrade_finalizer_flips(&self) -> bool {
        !self.stub_upgrade_finalizer_flips.is_empty()
    }

    /// Recompute `first_field_index` / `num_total_fields` for every class
    /// whose superclass chain passes through `changed_id`, after that
    /// class's instance-field layout changed (see `upgrade_synthetic_class`).
    ///
    /// Iterates the `ClassStore` in ascending `ClassId` order. Because a
    /// subclass is always loaded — and therefore assigned a `ClassId` —
    /// *after* its superclass, this ordering guarantees each class is
    /// visited only after its parent has already been recomputed, so a
    /// single pass propagates the change down arbitrarily deep
    /// inheritance chains.
    ///
    /// As in the load/upgrade paths, `num_total_fields` is only ever
    /// grown (`max(old, new)`): objects already allocated against the
    /// previous layout must not be left with too few slots.
    /// Recompute `has_finalizer` of every already-loaded transitive subclass
    /// of `changed_id`, whose own bit a stub upgrade just changed, by the
    /// same rule the define uses ([`class_has_finalizer_by_hotspot_rule`]),
    /// parents before children (`descendants_of` is breadth-first, as
    /// [`Self::recompute_subclass_layouts`] relies on). A subclass whose bit
    /// changed has its compiled code withdrawn (an allocation recipe bakes
    /// the bit in); the ids that changed are returned. JIT round 21 wave 22, lane
    /// hidden-resid (`CRATONVM_STUB_UPGRADE_FINALIZER_TO_SUBCLASSES`; item 8
    /// of `r21-review-w18-findings-FIXED-20261020.md`).
    pub(in crate::class_manager) fn propagate_finalizer_to_subclasses(&mut self, changed_id: ClassId) -> Vec<ClassId> {
        let empty_is_none = self.empty_finalizer_is_none();
        let finalization_enabled = self.finalization_enabled;
        let mut changed: Vec<ClassId> = Vec::new();
        for cid in self.class_store.descendants_of(changed_id) {
            let parent_has = self
                .class_store
                .get(cid)
                .and_then(|c| c.superclass)
                .and_then(|sid| self.class_store.get(sid))
                .is_some_and(|parent| parent.has_finalizer);
            if let Some(class) = self.class_store.get_mut(cid) {
                let has = finalization_enabled
                    && class_has_finalizer_by_hotspot_rule(class, parent_has, empty_is_none);
                if has != class.has_finalizer {
                    class.has_finalizer = has;
                    changed.push(cid);
                }
            }
        }
        for cid in &changed {
            fire_jit_invalidate_hook(cid.as_u32(), self.class_store.layout_domain());
        }
        changed
    }

    pub(in crate::class_manager) fn recompute_subclass_layouts(&mut self, changed_id: ClassId) {
        // Descendants whose `first_field_index` actually shifts: their
        // previously-resolved `(referring-class, cp-index) -> ResolvedField`
        // cache entries are baked against the stale offset and must be evicted
        // (see the post-loop invalidation below).
        let mut changed_descendants: Vec<u32> = Vec::new();
        // `descendants_of` walks the store's direct-subclass adjacency index
        // breadth-first, so it yields exactly the transitive subclasses of
        // `changed_id`, parents before children — the topological order the
        // recompute below needs, since each entry reads its parent's
        // already-updated `num_total_fields`.
        //
        // This replaces a scan of `0..class_store.len()` that probed every
        // class with `is_subclass_of`. That scan was wrong twice over:
        //
        //   * `len()` is the LIVE class count, not the id upper bound —
        //     `slot_count()` is (see its doc). After any class unload the
        //     tombstone makes `len() < slot_count()`, so the loop stopped
        //     short and silently skipped the highest-id subclasses. Those are
        //     the most recently loaded ones, i.e. precisely the application
        //     classes that extend a JDK stub. A missed descendant keeps a
        //     `first_field_index` baked against the pre-upgrade parent and its
        //     own fields then overlap the parent's — the "out-of-bounds field
        //     read ... undersized object layout" this function exists to
        //     prevent.
        //   * it cost `O(classes x hierarchy depth)` per upgrade, and runs
        //     once per synthetic-stub upgrade whose layout shifted.
        for cid in self.class_store.descendants_of(changed_id) {
            let Some(super_id) = self.class_store.get(cid).and_then(|c| c.superclass) else {
                continue;
            };
            let parent_total = self
                .class_store
                .get(super_id)
                .map_or(0, |sc| sc.num_total_fields);
            let own_instance_fields = match self.class_store.get(cid) {
                Some(c) => c
                    .fields
                    .iter()
                    .filter(|f| !f.access_flags.contains(FieldAccessFlags::STATIC))
                    .count(),
                None => continue,
            };
            let new_first = parent_total;
            let new_total = parent_total + own_instance_fields;
            if field_trace_enabled() {
                if let Some(class) = self.class_store.get(cid) {
                    if class.name.contains("DefaultHttpMessageConverters")
                        || class.name.contains("AnsiOutputApplicationListener")
                        || class.name.contains("AutoConfigurationPackages")
                    {
                        eprintln!("[FIELD-TRACE] recompute {}: old(ffi={},ntf={}) -> new(ffi={},ntf={}) changed_id={changed_id:?}",
                            class.name, class.first_field_index, class.num_total_fields, new_first, new_total);
                    }
                }
            }
            if let Some(class) = self.class_store.get_mut(cid) {
                let old_first = class.first_field_index;
                let old_total = class.num_total_fields;
                class.first_field_index = new_first;
                // Grow-only: never shrink below the count an existing
                // object was allocated with.
                class.num_total_fields = class.num_total_fields.max(new_total);
                if old_first != class.first_field_index || old_total != class.num_total_fields {
                    changed_descendants.push(cid.as_u32());
                }
            }
        }

        // A descendant whose field layout shifted may hold previously-resolved
        // `(referring-class, cp-index) -> ResolvedField` cache entries baked
        // against the STALE `first_field_index`. The parent invalidation in
        // `upgrade_synthetic_class` (`fire_resolution_invalidate_hook(id, ..)`) does
        // NOT evict them — `invalidate_class` keys on `key_class` /
        // `declaring_class_id`, both of which are the descendant itself, not the
        // parent. Without this, the descendant's getfield/putfield keeps reading
        // the old slot, which is out of bounds on the new layout — the
        // BC `:core:test` gradle-worker `EncodedStream$EncodedInput.delegate`
        // "slot 1 vs num_slots 1" hang. Evict them now (also drives JIT
        // recompilation of any method that baked the stale offset).
        for cid in changed_descendants {
            // Layout only: the descendant's constant pool is unchanged, so its
            // recorded failures and permanent constants stay (wave 24).
            fire_resolution_invalidate_hook(
                cid,
                self.class_store.layout_domain(),
                ResolutionInvalidation::FieldLayoutOnly,
            );
            // C2 review P1: the descendant's `first_field_index` just moved,
            // so every field handle minted against it is stale. Same reasoning
            // as the parent bump in `upgrade_synthetic_class`, applied to each
            // class the shift propagated to.
            self.metadata_realm.bump_generation(ClassId::new(cid));
            // Compact ref-field layout: the descendant's field offsets / oop-map
            // shifted with its parent's growth — rebuild + re-register it.
            self.class_store
                .register_compact_layout_if_enabled(ClassId::new(cid));
            // Same reasoning as `upgrade_synthetic_class`: this descendant's
            // own field offsets just shifted, so any already-JIT-compiled
            // method with one of them baked in (compact or legacy cell
            // offset) is now stale. See the fire_jit_invalidate_hook call
            // in `upgrade_synthetic_class` for the full explanation.
            fire_jit_invalidate_hook(cid, self.class_store.layout_domain());
        }
    }
}
/// Return the correct superclass for well-known JDK classes.
///
/// This is critical for exception handling: without the correct hierarchy,
/// `catch (RuntimeException e)` won't match `ArithmeticException` because
/// `is_subclass_of` checks require a correct parent chain.
/// Public lookup for verifier fallback — walks JDK superclass hierarchy
/// when classes aren't loaded yet.
pub fn jdk_superclass_lookup(name: &str) -> &'static str {
    jdk_superclass(name)
}

/// Return the least common superclass known to the static hierarchy table.
///
/// This is a verifier fallback for pre-Java-7 classfiles that have no
/// StackMapTable. Their types are inferred by merging control-flow frames, but
/// sibling exception classes are often not loaded yet when a merge happens. The
/// dynamic store remains authoritative when it can answer; this helper keeps
/// the fallback from unnecessarily widening known sibling classes to Object.
fn jdk_superclass_with_hints<'a>(
    name: &'a str,
    hints: &[cratonvm_native_api::AppWorkaroundEntry],
) -> &'a str {
    if let Some(entry) = hints.iter().find(|e| {
        e.kind == cratonvm_native_api::AppWorkaroundKind::VerifierSuperclassHint
            && e.class_pattern == name
    }) {
        return entry.aux;
    }
    jdk_superclass(name)
}

pub fn static_common_superclass_lookup(a: &str, b: &str) -> String {
    static_common_superclass_lookup_with_hints(a, b, &[])
}

pub fn static_common_superclass_lookup_with_hints(
    a: &str,
    b: &str,
    hints: &[cratonvm_native_api::AppWorkaroundEntry],
) -> String {
    if a == b {
        return a.to_string();
    }

    let mut a_chain = Vec::with_capacity(8);
    let mut current = a;
    for _ in 0..64 {
        a_chain.push(current);
        if current == "java/lang/Object" {
            break;
        }
        let next = jdk_superclass(current);
        if next == current {
            break;
        }
        current = next;
    }

    let mut current = b;
    for _ in 0..64 {
        if a_chain.iter().any(|ancestor| *ancestor == current) {
            return current.to_string();
        }
        if current == "java/lang/Object" {
            break;
        }
        let next = jdk_superclass_with_hints(current, hints);
        if next == current {
            break;
        }
        current = next;
    }
    "java/lang/Object".to_string()
}

/// Verifier fallback: is `child` a subclass of `parent` according to the
/// static JDK superclass table?
///
/// Walks the `jdk_superclass` chain from `child` upward. Every chain
/// terminates at `java/lang/Object` (the table's `_` default), so the walk
/// always finishes. This is spec-correct for bytecode verification: the JDK
/// class hierarchy is fixed, and the walk only reports `true` for a genuine
/// ancestor — it does NOT blanket-accept unrelated reference types.
///
/// Interfaces are intentionally not modelled here (the verifier handles
/// interface targets via the `is_interface` / `is_known_jdk_interface`
/// relaxation), so this only proves class-to-superclass relationships.
pub(in crate::class_manager) fn jdk_name_is_subclass_with_hints(
    child: &str,
    parent: &str,
    hints: &[cratonvm_native_api::AppWorkaroundEntry],
) -> bool {
    if child == parent {
        return true;
    }
    let mut current = child;
    // The deepest JDK hierarchy chains are well under 32 links; the bound
    // is a belt-and-braces guard against a malformed table entry.
    for _ in 0..64 {
        if current == "java/lang/Object" {
            return parent == "java/lang/Object";
        }
        let next = jdk_superclass_with_hints(current, hints);
        if next == parent {
            return true;
        }
        if next == current {
            // No progress (only possible for `Object`, handled above) —
            // stop to avoid an infinite loop.
            return false;
        }
        current = next;
    }
    false
}

/// The throwable classes `bootstrap_core_classes` pre-loads (its "Tier 3").
///
/// Hoisted out of that function so a test can iterate it. Every name here is
/// one this VM can hand to running bytecode, so every name here must have a
/// `jdk_superclass` arm that reaches `java/lang/Throwable` -- see
/// `throwable_like_inherits_its_chain`, which asserts exactly that over this
/// list. A name added below with no arm gets a blanket `java/lang/Object`
/// parent, which costs it the three-slot throwable model AND its compact
/// layout, and nothing else would have said so.
pub(in crate::class_manager) const BOOTSTRAP_EXCEPTION_CLASSES: &[&str] = &[
    "java/lang/NullPointerException",
    "java/lang/ArithmeticException",
    "java/lang/ArrayIndexOutOfBoundsException",
    "java/lang/IndexOutOfBoundsException",
    "java/lang/StringIndexOutOfBoundsException",
    "java/lang/ClassCastException",
    "java/lang/IllegalArgumentException",
    "java/lang/IllegalStateException",
    "java/lang/UnsupportedOperationException",
    "java/lang/ClassNotFoundException",
    "java/lang/NoSuchMethodException",
    "java/lang/NoSuchFieldException",
    "java/lang/NoSuchMethodError",
    "java/lang/NoSuchFieldError",
    "java/lang/AbstractMethodError",
    "java/lang/IncompatibleClassChangeError",
    "java/lang/IllegalAccessError",
    "java/lang/InstantiationError",
    "java/lang/StackOverflowError",
    "java/lang/OutOfMemoryError",
    "java/lang/ExceptionInInitializerError",
    "java/lang/LinkageError",
    "java/lang/VerifyError",
    "java/lang/SecurityException",
    "java/lang/NegativeArraySizeException",
    "java/lang/ArrayStoreException",
    "java/lang/IllegalMonitorStateException",
    "java/lang/InterruptedException",
    "java/lang/CloneNotSupportedException",
    "java/lang/NumberFormatException",
    "java/io/IOException",
    "java/io/FileNotFoundException",
    "java/io/UnsupportedEncodingException",
    "java/io/EOFException",
    "java/util/NoSuchElementException",
    "java/util/ConcurrentModificationException",
    "java/lang/reflect/InvocationTargetException",
];

/// Does `name`'s fabricated superclass chain reach `java/lang/Throwable`?
///
/// `synthetic_stub_fields` asks this to decide whether a throwable-shaped stub
/// should declare the three-slot model itself or INHERIT it. A name
/// `jdk_superclass` knows gets a real fabricated parent chain — the stub mint
/// path loads `jdk_superclass(name)` and sizes the class to
/// `parent.num_total_fields + own` — so re-declaring the three at every level
/// double-counts, and an exception hierarchy is deep enough for that to matter.
/// A name it does not know gets a blanket `java/lang/Object` parent and must
/// declare them.
///
/// `java/lang/Throwable` itself answers FALSE: nothing above it carries the
/// model, and its own arm is what declares it.
///
/// The walk is bounded rather than trusting `jdk_superclass` to terminate. It
/// does terminate today — every arm either names a strictly higher class or
/// falls through to `java/lang/Object` — but this function's whole job is to
/// decide a field count, and a cycle introduced later should cost a wrong
/// count, not a hang inside class definition.
pub(in crate::class_manager) fn jdk_chain_reaches_throwable(name: &str) -> bool {
    let mut cur = name;
    for _ in 0..64 {
        let parent = jdk_superclass(cur);
        if parent == "java/lang/Throwable" {
            return true;
        }
        if parent == cur || parent == "java/lang/Object" {
            return false;
        }
        cur = parent;
    }
    false
}

