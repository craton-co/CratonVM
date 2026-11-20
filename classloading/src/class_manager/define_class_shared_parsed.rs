#[allow(unused_imports)]
use super::*;

impl ClassManager {
    /// [`Self::define_class_shared_with_options`] with the class file parsed
    /// already when `preparsed` is `Some` (it must be the parse of `bytes`;
    /// see [`Self::define_class_with_options_preparsed`]), parsed here
    /// otherwise.
    pub(in crate::class_manager) fn define_class_shared_parsed(
        &mut self,
        name: &str,
        bytes: SharedBytes,
        loader_id: ClassLoaderId,
        options: DefineClassOptions,
        preparsed: Option<cratonvm_reader::ClassFile>,
    ) -> Result<ClassId, VmError> {
        let _phase = cratonvm_jfr::phase::enter(cratonvm_jfr::phase::Category::ClassLoad);
        // Every class definition in the process funnels through here, so this
        // is the one place the count can be taken. See `define_census`.
        crate::define_census::note(name);
        // `CRATONVM_DBG_DEFINE_CENSUS` (interpreter round i4 wave 90, lane
        // L5): the inner phase rows below count only a define the VM's
        // `define_class_full` armed (a `defineClass*` native's); every other
        // define (class-path / jimage loads, nested defines) is one
        // `other-define` row. `false` / no clock without the census.
        let census_charged = crate::define_census::take_native_define();
        let _other_define_clock = crate::define_census::other_define_guard(census_charged);
        // What a successful define charges against `-XX:MaxMetaspaceSize`
        // (gc-common w6-f; see `metaspace_charged_bytes`). Taken here because
        // `bytes` is moved into the class record below.
        let metaspace_charge = bytes.len() as u64;
        // `CRATONVM_DBG_DEFINE=1` — one line per definition, naming the LOADER.
        //
        // The name filter was two hard-coded TestNG strings, which made the
        // switch useless for every later "who defined this class, and under
        // which loader?" question — and that is the question a duplicate
        // class-identity defect always reduces to. It now honours
        // `CRATONVM_DBG_DUPCLASS_FILTER` (the substring the sibling
        // `[DBG_DUPCLASS]` trace already keys on, so one filter drives both
        // halves of the investigation), and falls back to the historical pair
        // when no filter is set so an existing recipe still works.
        // `CRATONVM_DBG_DUPCLASS_BT=1` adds the Rust backtrace, which is what
        // names the delegation path that reached this definition.
        if loader_flags().dbg_define {
            let hit = match loader_flags().dbg_dupclass_filter.as_deref() {
                Some(f) => name.contains(f),
                None => name.contains("TestNGTestEngine") || name.contains("IsTestNGTestClass"),
            };
            if hit {
                eprintln!(
                    "[DEFINE-DBG] define_class name={} loader_id={:?} bytes_len={}",
                    name,
                    loader_id,
                    bytes.len()
                );
                if loader_flags().dbg_dupclass_bt {
                    eprintln!(
                        "[DEFINE-DBG-BT] {name}
{}",
                        std::backtrace::Backtrace::force_capture()
                    );
                }
            }
        }
        if loader_flags().dbg_fbcglib
            && (name.contains("RepositoryConfiguration") || name.contains("RawFactoryMethod"))
        {
            let haystack = String::from_utf8_lossy(&bytes);
            let has_factory_data = haystack.contains("CGLIB$FACTORY_DATA");
            eprintln!(
                "[FBCGLIB-DBG] define_class name={name} override_name={:?} loader_id={:?} bytes_len={} has_CGLIB$FACTORY_DATA_utf8={has_factory_data}",
                options.override_name,
                loader_id,
                bytes.len(),
            );
        }
        if loader_flags().dbg_obsreg
            && (name.contains("ObservationRegistry")
                || name.contains("RestClientObservationAutoConfigurationWithoutMetricsTests")
                || name.contains("TestObservationRegistry"))
        {
            eprintln!(
                "[OBSREG-DBG] define_class name={} loader_id={:?} bytes_len={}",
                name,
                loader_id,
                bytes.len()
            );
        }
        // WP2.3: Reject too-short / non-CAFEBABE bytes up-front with a
        // typed ClassFormatError. The reader will catch malformed
        // bytes too, but a stronger pre-check produces clearer error
        // messages and avoids leaking parser internals.
        if bytes.len() < 8 {
            // HotSpot `parse_stream`'s `guarantee_more(8)`: `Truncated class
            // file`, naming no class. It read `class file too short (N bytes;
            // need at least 8 for header)` before interpreter round i5 wave 109
            // (lane L5; `L5W109ClassFileParserTexts`, row `seven-bytes`).
            return Err(VmError::Linkage(LinkageError::ClassFormatError {
                class_name: name.to_string(),
                message: cratonvm_reader::class_reader_error::TRUNCATED_CLASS_FILE.to_string(),
            }));
        }
        if bytes[0..4] != [0xCA, 0xFE, 0xBA, 0xBE] {
            // HotSpot's `ClassFileParser` wording, the magic as an unsigned
            // decimal and the name in internal form; it names the class
            // itself, so the throwable's message is this text alone
            // (`exceptions::linkage_throwable`). Reached by a load-time
            // transformer's answer that is not a class file (interpreter round
            // i1 wave 43, lane L3; `L3W42TransformerBadBytes`, `load-garbage`:
            // `Incompatible magic value 16909060 in class file ...$Garbage`).
            let magic = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            let named = if name.is_empty() { "<Unknown>" } else { name };
            return Err(VmError::Linkage(LinkageError::ClassFormatError {
                class_name: name.to_string(),
                message: format!("Incompatible magic value {magic} in class file {named}"),
            }));
        }

        // Parse the class file -- unless the caller already did
        // (`CRATONVM_DEFINE_ONE_PARSE`): a parse of these same bytes, so the
        // error mapping below is the one it would have met too.
        // `CRATONVM_DBG_DEFINE_CENSUS` phase clock (interpreter round i4
        // wave 88, lane L5): parse + attribute decode.
        let parse_clock = crate::define_census::phase_start_if(census_charged);
        // `--enable-preview` is THIS VM's (`Self::preview_enabled`; interpreter
        // round i5 wave 101, lane L5). The own parse refuses a preview class
        // file where HotSpot's `verify_class_version` does, before the
        // constant pool; a caller's parse (the define natives' pre-pass,
        // `CRATONVM_DEFINE_ONE_PARSE`) used the plain entry point, which parses
        // a preview class file as a shape, so its version is checked here.
        let preview_enabled = self.preview_enabled;
        let parsed = match preparsed {
            Some(class_file) => match class_file.version.verify(preview_enabled) {
                Ok(()) => Ok(class_file),
                Err(rejection) => Err(
                    cratonvm_reader::class_reader_error::ClassReaderError::UnsupportedVersion {
                        major: class_file.version.major,
                        minor: class_file.version.minor,
                        rejection,
                    },
                ),
            },
            None => cratonvm_reader::read_class_shared_with_preview(bytes.clone(), preview_enabled),
        };
        let mut class_file = parsed.map_err(|e| {
            // A version rejection is `UnsupportedClassVersionError` on HotSpot,
            // not the bare `ClassFormatError` every other reader error maps to.
            // The reader cannot build the message itself: HotSpot's wording
            // embeds the class name, and `this_class` is not read until after
            // the constant pool — long after the version check. `name` is
            // already the internal (slash) form HotSpot prints.
            //
            // An empty `name` means the caller supplied none and the class
            // manager is about to derive it from `this_class` — which the
            // version check runs before. HotSpot has the same ordering problem
            // and prints a placeholder; measured on Adoptium 25.0.3.9 via
            // `ClassLoader.defineClass(null, bytes, 0, len)` of a 69.65535
            // class file:
            //
            //   Preview features are not enabled for <Unknown> (class file
            //   version 69.65535). Try running with '--enable-preview'
            //
            // `<Unknown>` is generic, not preview-specific: it appears in all
            // five of HotSpot's version messages, with and without the flag.
            // One knowing inaccuracy — HotSpot distinguishes a null name from
            // an explicitly empty one (`defineClass("", ...)` really does print
            // the doubled space), but `read_optional_internal_name` folds Java
            // `null` and `""` into the same Rust `""`, so CratonVM cannot tell
            // them apart here. Null is the reachable case (JNI DefineClass and
            // `ClassLoader.defineClass(null, ..)`); `""` is a caller passing a
            // deliberate empty name, and it gets null's message.
            let reported = if name.is_empty() { "<Unknown>" } else { name };
            if let Some(message) = e.unsupported_class_version_message(reported) {
                return VmError::Linkage(LinkageError::UnsupportedClassVersionError {
                    class_name: name.to_string(),
                    message,
                });
            }
            // HotSpot `ClassFileParser`'s wording where the reader knows it
            // (`Invalid method Code length 0 in class file p/X`, `Truncated
            // class file`, ...; interpreter round i5 wave 109, lane L5, page
            // `i108-L7-code-attribute-parse-refusals-use-the-readers-words`,
            // probe `L5W109ClassFileParserTexts`). Before it the reader's own
            // words followed the class name (`p/X: invalid class data: Code
            // attribute code_length 0 outside valid range 1..=65535`).
            if let Some(message) = e.hotspot_class_format_message(name) {
                return VmError::Linkage(LinkageError::ClassFormatError {
                    class_name: name.to_string(),
                    message,
                });
            }
            // HotSpot's own `ClassFormatError` text, already naming the class
            // (`Extra bytes at the end of class file p/X`): raise it without
            // the reader's `invalid class data:` lead (interpreter round i4
            // wave 98, lane L5).
            if let cratonvm_reader::class_reader_error::ClassReaderError::InvalidClassData {
                message,
            } = &e
            {
                if message.starts_with(cratonvm_reader::class_reader::EXTRA_BYTES_PREFIX) {
                    return VmError::Linkage(LinkageError::ClassFormatError {
                        class_name: name.to_string(),
                        message: message.clone(),
                    });
                }
            }
            VmError::Linkage(LinkageError::ClassFormatError {
                class_name: name.to_string(),
                message: e.to_string(),
            })
        })?;

        // Eagerly decode the class-level attribute table. Most class-level
        // attributes (`BootstrapMethods`, `Signature`, `RuntimeVisible[/Invisible]
        // Annotations`, `NestHost`, `NestMembers`, `Record`, `PermittedSubclasses`,
        // `InnerClasses`, `EnclosingMethod`, `Module`, `SourceFile`) are
        // consumed unconditionally below to populate the runtime `Class`
        // struct, so paying the decode cost up-front is strictly cheaper
        // than wrapping every consumer site in a fallible decode call. In
        // particular `BootstrapMethods` MUST be available before the class
        // becomes live, otherwise the first `invokedynamic` site that
        // resolves against the class would have to re-borrow the constant
        // pool from the reader (which we've already moved out of by then).
        // Class-level attribute counts are small (≤ ~10 in practice), so
        // the extra work is in the noise next to verification + linking.
        // A class attribute refusal HotSpot words itself (`Invalid SourceFile
        // attribute at constant pool index N`) in its words, naming
        // `this_class` (interpreter round i5 wave 110, lane L5).
        let class_attr_name = class_file.this_class.clone();
        force_decode_all(&mut class_file.attributes, &class_file.constant_pool).map_err(|e| {
            let e = e.naming_class(&class_attr_name);
            VmError::Linkage(LinkageError::ClassFormatError {
                class_name: name.to_string(),
                message: decode_refusal_hotspot_text(&e, name)
                    .unwrap_or_else(|| format!("attribute decode failed: {e}")),
            })
        })?;

        // Round-2 fix (CRIT): also force-decode method- and field-level
        // LazyAttributes. Without this, `ClassFileMethod::code()` /
        // `ClassFileField::constant_value_index()` return None for all
        // production-loaded classes (they only return Some on
        // `LazyAttribute::Decoded`), which silently disables the bytecode
        // verifier and breaks interp/JIT method-dispatch fallback.
        // The lazy-attribute win was a measurement mistake: methods/fields
        // are universally accessed at class-link time, so deferring decode
        // saves nothing. Decode eagerly at load.
        // A decode refusal HotSpot words itself (the exception table's
        // `Catch type in exception table has bad constant type`) is raised in
        // its words, naming `this_class` (interpreter round i5 wave 109, lane
        // L5).
        let decoded_class_name = class_file.this_class.clone();
        for method in class_file.methods.iter_mut() {
            force_decode_all(&mut method.attributes, &class_file.constant_pool).map_err(|e| {
                let e = e.naming_class(&decoded_class_name);
                VmError::Linkage(LinkageError::ClassFormatError {
                    class_name: name.to_string(),
                    message: decode_refusal_hotspot_text(&e, name).unwrap_or_else(|| {
                        format!("method '{}' attribute decode failed: {e}", method.name)
                    }),
                })
            })?;
        }
        for field in class_file.fields.iter_mut() {
            force_decode_all(&mut field.attributes, &class_file.constant_pool).map_err(|e| {
                let e = e.naming_class(&decoded_class_name);
                VmError::Linkage(LinkageError::ClassFormatError {
                    class_name: name.to_string(),
                    message: decode_refusal_hotspot_text(&e, name).unwrap_or_else(|| {
                        format!("field '{}' attribute decode failed: {e}", field.name)
                    }),
                })
            })?;
        }
        // zr z11/md: the decoded attributes hold views into `bytes`; re-home
        // them so they stop pinning the whole class file
        // (`CRATONVM_CLASS_CODE_COMPACT`, default ON).
        if loader_flags().class_code_compact {
            compact_member_attribute_views(&mut class_file, &bytes);
        }
        crate::define_census::phase_end(crate::define_census::DefinePhase::Parse, parse_clock);
        // `CRATONVM_DBG_DEFINE_CENSUS` (interpreter round i4 wave 94, lane L5):
        // the rest of this define, outside the named rows, as
        // `define-pre-checks` / `define-supertypes` / `define-build` /
        // `define-link-refusal`.
        let pre_checks_clock = crate::define_census::phase_start_if(census_charged);

        // WP2.3: Class-name match check. If the caller asked for class
        // `name` but the class file's `this_class` says something
        // else, the JVMS requires a `NoClassDefFoundError`
        // (§5.3.5#3.b). The `override_name` option (used by hidden
        // classes) bypasses this check because it deliberately mangles
        // the registered name.
        //
        // HotSpot's wording (`ClassFileParser`, measured on JDK 25, both
        // modes): the requested name, internal form, then the class file's:
        // `NoClassDefFoundError: p/A (wrong name: p/B)`. It read
        // `p/A (defineClass requested name p/A but class file declares p/B)`
        // before interpreter round i1 wave 37 (lane L5; probe
        // `L5/L5W37DefineWrongName`), which no caller parsing HotSpot's text
        // recognises.
        // HotSpot `ClassFileParser`'s access-flag refusals (interpreter round
        // i4 wave 84, lane L5): the class's own before the wrong-name check,
        // its fields' and methods' after it (below). `need_verify` is
        // HotSpot's `BytecodeVerificationRemote` scope: every loader but the
        // boot loader; a trusted generator (`skip_verification`) and an
        // instrumentation redefinition keep their own paths.
        //
        // `-Xverify:all` (t1 wave 13, lane I24W): `BytecodeVerificationLocal`
        // too, so HotSpot's `_need_verify` holds for boot classes and
        // `relax_format_check_for` relaxes no loader (probe
        // `T1W13I24WStructuralFormat.java`, `boot` rows under
        // `-Xverify:all -Xbootclasspath/a:`). `CRATONVM_VERIFY_ALL_CHECKS_BOOT_FORMAT=0`
        // keeps the boot and platform loaders relaxed.
        let strict_format = self.strict_verification && self.verify_all_checks_boot_format;
        let modifiers_need_verify = (loader_id != ClassLoaderId::Bootstrap || strict_format)
            && !options.skip_verification
            && !options.allow_redefine;
        // HotSpot's class-name checks (interpreter round i5 wave 106, lane L5;
        // probe `L5W106IllegalClassNames`): `verify_legal_class_name` on every
        // CONSTANT_Class while the pool is parsed (before the access flags),
        // then `this_class` must not be an array (after them). Its scope is
        // `_need_verify && !_relax_verify`: neither the boot nor the platform
        // loader (`relax_format_check_for`). Nothing checked these names, so a
        // user loader defined a class named `[Lfoo;`, `foo.Bar` or `a//B`.
        let names_need_verify = modifiers_need_verify
            && (loader_id != ClassLoaderId::Extension || strict_format);
        // `-Xverify:none` / `-noverify` (t1 wave 10, lane I24W,
        // `CRATONVM_NOVERIFY_RELAXES_FORMAT_CHECKS`; see
        // [`Self::set_noverify_relaxes_format_checks`]): HotSpot's
        // `_need_verify` is false for every loader, so the modifier and member
        // name legality checks go. Wave 13 (same lane,
        // `CRATONVM_NOVERIFY_RELAXES_STRUCTURAL_CHECKS`; see
        // [`Self::set_noverify_relaxes_structural_checks`]) relaxes the
        // structural `_need_verify` checks too; the ones HotSpot makes
        // regardless (or crashes without) keep the loader scope above.
        let (noverify_relaxed, structural_relaxed) = self.noverify_relaxation();
        let legal_modifiers_need_verify = modifiers_need_verify && !noverify_relaxed;
        let member_names_need_verify = names_need_verify && !noverify_relaxed;
        let structural_need_verify = modifiers_need_verify && !structural_relaxed;
        let class_names_need_verify = names_need_verify && !structural_relaxed;
        // The constant pool's reference checks (interpreter round i5 wave 110,
        // lane L5), made while HotSpot parses the pool, before the class
        // names' legality check of the same pass.
        if !options.allow_redefine && modifiers_need_verify {
            if let Some(refusal) = constant_pool_refusal(
                &class_file,
                name,
                member_names_need_verify,
                structural_need_verify,
            ) {
                return Err(VmError::Linkage(refusal));
            }
        }
        if !options.allow_redefine {
            if let Some(refusal) = class_name_refusal(&class_file, name, class_names_need_verify) {
                return Err(VmError::Linkage(refusal));
            }
        }
        if !options.allow_redefine {
            if let Some(refusal) =
                class_modifier_refusal(&class_file, name, legal_modifiers_need_verify)
            {
                return Err(VmError::Linkage(refusal));
            }
        }
        // A module / package constant in an ordinary class (wave 110, L5).
        if !options.allow_redefine && modifiers_need_verify {
            if let Some(refusal) = bad_constant_refusal(&class_file, name) {
                return Err(VmError::Linkage(refusal));
            }
        }
        if structural_need_verify
            && !options.allow_redefine
            && class_file.this_class.starts_with('[')
        {
            return Err(VmError::Linkage(LinkageError::ClassFormatError {
                class_name: name.to_string(),
                message: format!(
                    "Bad class name in class file {}",
                    if name.is_empty() { "<Unknown>" } else { name }
                ),
            }));
        }
        if options.override_name.is_none() && !name.is_empty() && &*class_file.this_class != name {
            return Err(VmError::Linkage(LinkageError::NoClassDefFoundError {
                class_name: format!("{name} (wrong name: {})", class_file.this_class),
            }));
        }

        // H5 (HIGH): privileged-package spoofing guard.
        //
        // A user-defined class loader must not be allowed to define a
        // class in a protected runtime package such as `java.*`. If it
        // could, the spoofed class would share its package *name* with
        // the genuine platform classes and (combined with same-loader
        // runtime-package identity, were that ever relaxed) could gain
        // access to package-private platform members; more concretely,
        // it lets untrusted code masquerade as core-library code. This
        // mirrors HotSpot's `ClassLoader.preDefineClass` /
        // `SystemDictionary` check, which throws
        // `SecurityException: Prohibited package name: java.*` for any
        // non-bootstrap loader defining a `java/` class.
        //
        // Exemptions:
        //   * The bootstrap loader (`ClassLoaderId::Bootstrap`) is the
        //     legitimate definer of all `java/`, `jdk/internal/`, and
        //     `sun/` classes.
        //   * Hidden classes / `override_name` defines are JDK-internal,
        //     trusted code-generation paths (`Lookup.defineHiddenClass`)
        //     that deliberately register under a mangled name and are
        //     gated by the trusted lookup that produced them; the real
        //     JVM permits them to live in restricted packages.
        //   * Privileged defines (`Unsafe.defineClass`, BUG-10) are the
        //     all-powerful JVM-internal path HotSpot itself routes around
        //     `preDefineClass`; ByteBuddy/CGLIB use it to inject an accessor
        //     such as `java.lang.ClassLoader$ByteBuddyAccessor$V1`. Any code
        //     that holds `Unsafe` already has full VM power, so exempting it
        //     does not widen the threat model.
        //   * The platform loader (`ClassLoaderId::Extension`): HotSpot's
        //     `preDefineClass` and `check_prohibited_package` both exempt it
        //     (it owns `java.sql`, `java.net.http`, ...) -- i4 wave 97.
        if loader_id != ClassLoaderId::Bootstrap
            && loader_id != ClassLoaderId::Extension
            && !options.hidden
            && options.override_name.is_none()
            && !options.privileged_define
        {
            let defined_name: &str = &class_file.this_class;
            if is_prohibited_package_name(defined_name) {
                return Err(VmError::Runtime(RuntimeError::SecurityException {
                    message: format!(
                        "Prohibited package name: {} \
                         (non-bootstrap loader {} cannot define a class in a \
                         protected platform package)",
                        defined_name.replace('/', "."),
                        loader_id
                    ),
                }));
            }
        }

        // Round 9 wave 6 (`vmhelp6`): the privileged exemption above must not
        // admit a FORGED `java/` class. HotSpot's `ClassFileParser` refuses a
        // `java/` class from any loader other than boot/platform whatever the
        // entry point, `Unsafe.defineClass` included. The exemption exists for
        // BRAND-NEW JDK-internal names (ByteBuddy's
        // `ClassLoader$ByteBuddyAccessor$V1` injection, `JavaLangAccess`
        // species classes), which CratonVM's native bridges route to
        // `Application` because they map a null loader to `0`. Two shapes are
        // refused here (see `privileged_define_forges_core_class`):
        //   * a `java/` define into a genuine user-defined loader
        //     (`UserDefined`) -- exactly HotSpot's rule, which no JDK-internal
        //     bridge relies on (they pass the null/boot loader);
        //   * a `java/` define that would SHADOW a class the bootstrap loader
        //     already defined (a second `java/lang/String`). A duplicate core
        //     name makes the tiers disagree -- the JIT's by-name loader-split
        //     fallback in `jit_typecheck_resolve` admits a subclass of the
        //     forgery as `instanceof String`, the interpreter refuses it -- and
        //     the name-keyed JDK layout census reads the fieldless forgery as
        //     the real `String`. See
        //     `jit-instanceof-helper-admits-a-duplicate-java-lang-class-the-interpreter-refuses-20260918.md`.
        // `allow_redefine` (instrumentation) keeps its own path.
        if options.privileged_define
            && loader_id != ClassLoaderId::Bootstrap
            && !options.hidden
            && options.override_name.is_none()
            && !options.allow_redefine
        {
            let defined_name: &str = &class_file.this_class;
            // The platform loader legitimately owns `java/` classes too
            // (`java/sql/*`), so a duplicate of one of those is the same forgery.
            // A fabricated compatibility STUB does not count: real bytes defined
            // over a stub upgrade it in place (see the stub-upgrade path below),
            // which is not a duplicate.
            let is_real = |id: ClassId| {
                self.class_store
                    .get(id)
                    .is_some_and(|c| !c.origin.is_compatibility_stub())
            };
            let bootstrap_has_name =
                loaded_classes_probe(&self.loaded_classes, ClassLoaderId::Bootstrap, defined_name)
                    .is_some_and(is_real)
                    || loaded_classes_probe(
                        &self.loaded_classes,
                        ClassLoaderId::Extension,
                        defined_name,
                    )
                    .is_some_and(is_real);
            // zt t25/df11b (define page §9d): the PLATFORM loader's privileged
            // define of a name it defined itself is HotSpot's duplicate-definition
            // `LinkageError` (the duplicate check below), not a forgery: a
            // second `Lookup.defineClass` of one `java.sql` class through a
            // platform lookup class answered `SecurityException`.
            // `CRATONVM_PLATFORM_REDEFINE_IS_DUPLICATE=0` keeps the refusal.
            let platform_redefines_own = loader_id == ClassLoaderId::Extension
                && loaded_classes_probe(
                    &self.loaded_classes,
                    ClassLoaderId::Extension,
                    defined_name,
                )
                .is_some_and(is_real);
            if !platform_redefines_own
                && privileged_define_forges_core_class(defined_name, loader_id, bootstrap_has_name)
            {
                return Err(VmError::Runtime(RuntimeError::SecurityException {
                    message: format!(
                        "Prohibited package name: {} \
                         (privileged define into loader {} may not forge or \
                         shadow a java.* class)",
                        defined_name.replace('/', "."),
                        loader_id
                    ),
                }));
            }
        }

        // The fields' and methods' `ClassFileParser` refusals (see above):
        // HotSpot parses them before the dictionary can see a duplicate.
        // The interfaces' duplicate check precedes them, the nest and sealed
        // attributes' follow them (interpreter round i5 wave 109, lane L5;
        // the duplicate check in `modifiers_need_verify`'s scope). The class
        // attribute checks are HotSpot's in every loader (the boot class path
        // included, probe `T1W13I24WStructuralFormat` `boot` row B18; t1 wave
        // 13, lane I24W); only a trusted generator (`skip_verification`)
        // skips them.
        if !options.allow_redefine {
            if structural_need_verify {
                if let Some(refusal) = duplicate_interface_refusal(&class_file) {
                    return Err(VmError::Linkage(refusal));
                }
            }
            if let Some(refusal) = member_refusal(
                &class_file,
                modifiers_need_verify,
                legal_modifiers_need_verify,
                member_names_need_verify,
                structural_need_verify,
                true,
            ) {
                return Err(VmError::Linkage(refusal));
            }
            if !options.skip_verification {
                if let Some(refusal) =
                    class_attribute_refusal(&class_file, member_names_need_verify)
                {
                    return Err(VmError::Linkage(refusal));
                }
            }
            if modifiers_need_verify && !structural_need_verify {
                widen_max_locals_to_arguments(&mut class_file);
            }
        }
        // Interpreter round i5 wave 101, lane L3: HotSpot's `InnerClasses`
        // checks under the same `_need_verify` scope (the verdict is the
        // reader's `inner_classes_verdict`): two `ClassFormatError`s, and a
        // circular or repeated-inner table IGNORED (applied below).
        if !options.allow_redefine {
            if let Some(refusal) = inner_class_modifier_refusal(
                &class_file.attributes,
                &class_file.constant_pool,
                class_file.version.major,
                &class_file.this_class,
                legal_modifiers_need_verify,
                structural_need_verify,
            ) {
                return Err(VmError::Linkage(refusal));
            }
        }
        // Interpreter round i7 wave 141, lane L5: an interface's superclass
        // must be `java/lang/Object` (every loader, as HotSpot's
        // `guarantee_property`; before the superclass is resolved).
        if !options.allow_redefine {
            if let Some(refusal) = interface_superclass_refusal(&class_file) {
                return Err(VmError::Linkage(refusal));
            }
        }
        // Under `-Xverify:none` HotSpot neither refuses nor ignores the table
        // (probe rows S16, S17, S48): it is kept as written.
        let inner_classes_ignored = if structural_need_verify {
            use cratonvm_reader::attribute::{inner_classes_verdict, InnerClassesVerdict as V};
            let verdict = class_file
                .attributes
                .iter()
                .find_map(|a| match a.as_decoded() {
                    Some(Attribute::InnerClasses(entries)) => {
                        Some(stored_inner_classes_verdict(entries, &class_file.constant_pool, class_file.version.major))
                    }
                    _ => None,
                });
            let refuse = |what: &str| {
                VmError::Linkage(LinkageError::ClassFormatError {
                    class_name: class_file.this_class.to_string(),
                    message: format!("{what} in class file {}", class_file.this_class),
                })
            };
            match verdict {
                Some(V::BothOuterAndInner) => {
                    return Err(refuse("Class is both outer and inner class"));
                }
                Some(V::DuplicateEntry) => {
                    return Err(refuse("Duplicate entry in InnerClasses attribute"));
                }
                Some(V::Ignore) => true,
                Some(V::Keep) | None => false,
            }
        } else {
            false
        };

        // WP2.3: Duplicate-define rejection. Check before we mutate
        // `loading_guard` so we don't poison the guard set on the
        // failure path. The `allow_redefine` flag (used by WP2.4
        // `redefineClasses`) bypasses this so the instrumentation
        // path can replace bytecode in place.
        // `class_file.this_class` is now `Arc<str>` (round 4 reader). To
        // keep the rest of this function — which previously used `String`
        // for `stored_name_preview` — working without per-line conversion,
        // we materialise into `String` here. The cost is one alloc on the
        // duplicate-define / hidden-collision probe path only; the hot
        // success path below builds a fresh `Arc<str>` from `class.name`
        // and never touches this `String` again.
        let stored_name_preview: String = options
            .override_name
            .clone()
            .unwrap_or_else(|| class_file.this_class.to_string());
        // Remember this class as a VM fallback copy once it is stored
        // (`CRATONVM_DEFINE_BUILTIN_CONSTRAINT_FALLBACK_PIN`, wave 133, lane
        // L5c). `false`, with no set write, unless the built-in refusal is on.
        let vm_fallback_copy = !options.allow_redefine
            && !options.hidden
            && self.records_vm_fallback_copy(loader_id, &options);
        if !options.allow_redefine && !options.hidden {
            // C34 audit fix (HIGH): zero-allocation probe via the
            // borrowed-key helper. The hot insert path below still
            // builds a real `Arc<str>` from `class.name` (the
            // pool-interned name); the duplicate-define probe now pays
            // zero extra allocation regardless of how many classes the
            // loader has defined.
            // zt t16/df6: a define of this name whose `addClass` upcall is in
            // flight holds the name (HotSpot's placeholder) although no
            // lookup answers it yet.
            let dup = loaded_classes_probe(&self.loaded_classes, loader_id, &stored_name_preview)
                .or_else(|| self.unbound_define_named(loader_id, &stored_name_preview));
            // Diagnostic for the "real cglib re-enhances an already-native-
            // CGLIB-generated class" family (see native-builtins/src/
            // cglib_enhancer.rs's `emit_public_static_field` doc comment):
            // confirms whether a same-name collision was actually detected
            // and rejected here (as opposed to real cglib silently
            // succeeding under a different name).
            if loader_flags().dbg_fbcglib && dup.is_some() {
                eprintln!(
                    "[FBCGLIB-DBG] duplicate-define rejected: name={stored_name_preview} loader_id={loader_id:?} existing={:?}",
                    dup
                );
            }
            if dup.is_some() {
                // `DuplicateClassDefinition`, NOT `IncompatibleClassChangeError`.
                //
                // HotSpot throws `java.lang.LinkageError` ITSELF for a duplicate
                // definition, and the variant beside it in `LinkageError`
                // already exists for exactly this, mapping to
                // `java/lang/LinkageError` with HotSpot's own wording (see
                // `vm/src/runtime/exceptions.rs`, whose comment names Tomcat's
                // loader lifecycle and ByteBuddy's injection strategies as the
                // callers that catch the base type).
                //
                // MEASURED in BOTH modes (`probes/ClassLoaderShadowSweep.java`):
                // defining the same bytes twice in one loader gave
                // `IncompatibleClassChangeError` against HotSpot's
                // `LinkageError`. ICCE *is* a LinkageError subclass, so
                // `catch (LinkageError)` was unaffected -- but code that
                // catches ICCE specifically caught ours and would not catch
                // HotSpot's, which is a recovery path running here that never
                // runs on the reference VM.
                //
                // The right variant was already present and simply not used
                // here; nothing needed to be added to the enum.
                return Err(VmError::Linkage(LinkageError::DuplicateClassDefinition {
                    class_name: stored_name_preview.to_string(),
                    loader: format!("{loader_id:?}"),
                }));
            }

            // JVMS §5.3.4 at define, HotSpot's `check_constraints` after its
            // dictionary check (interpreter round i1 wave 37, lane L5): a
            // constraint member resolution recorded pins this name for this
            // loader to another class, so defining a new one is the violation.
            // Every mode: the table is filled only where constraints are
            // recorded (`--real-jdk`, or `--compatible` with
            // `CRATONVM_COMPAT_LOADER_CONSTRAINTS`, the VM's
            // `ClassRealm::loader_constraints_on`), so a mode that records
            // none pays one `is_empty` test here. It was gated on `--real-jdk`,
            // which left `--compatible`'s recorded constraints unchecked at
            // define (interpreter round i1 wave 49, lane L5; probe
            // `L5W49CompatLoaderDoors`, `constraint accessor-first`). Probe
            // `L5W37LoaderConstraintPending`.
            //
            // A USER-DEFINED loader's define, and since interpreter round i6
            // wave 117 (lane L5, `CRATONVM_DEFINE_BUILTIN_CONSTRAINT_REFUSAL`,
            // default OFF since wave 118 until its census) a BUILT-IN loader's
            // too, as HotSpot refuses both:
            // except a define the VM makes for itself where HotSpot would define
            // nothing (`DefineClassOptions::vm_fallback`) and a compatibility
            // stub's upgrade (inside the refusal), which stay counted (the pin
            // after registration) and traced under `CRATONVM_DBG=access`.
            // Before, every built-in define was admitted: an application-loader
            // class defined after a user loader pinned the name to its own was
            // a second class of one name across a constraint.
            if matches!(loader_id, ClassLoaderId::UserDefined(_))
                || (self.define_builtin_constraint_refusal
                    && !options.vm_fallback
                    && !self.on_vm_fallback_thread())
            {
                if let Some(message) = self.loader_constraint_define_refusal(
                    &stored_name_preview,
                    loader_id,
                    class_file
                        .access_flags
                        .contains(ClassAccessFlags::INTERFACE),
                ) {
                    return Err(VmError::Linkage(LinkageError::LoaderConstraintViolation {
                        message,
                    }));
                }
            }

            // A synthetic stub for this exact name may already exist under a
            // built-in loader (`create_synthetic_stub` always registers under
            // `ClassLoaderId::Bootstrap` — see `is_enterprise_stub_prefix`).
            // This happens when code deliberately probes for a not-yet-generated
            // class via `ClassLoader.loadClass`/`Class.forName` expecting
            // `ClassNotFoundException` and then dynamically generates + defines
            // the real bytecode itself (e.g. SmallRye Config's `@ConfigMapping`
            // `<iface>$$CMImpl` runtime generation via
            // `MethodHandles.Lookup.defineClass`). The `reflective_probe` gate
            // only forces a proper CNFE for `Class.forName`-style existence
            // probes, not plain `ClassLoader.loadClass`, so the first lookup
            // fabricates a Bootstrap stub instead.
            //
            // If we minted a brand-new ClassId here instead, it would be
            // permanently shadowed: `get_loaded_class_id` (used by every
            // subsequent by-name resolution — Class.forName, loadClass,
            // MethodHandles.Lookup.findStatic/findConstructor, constant-pool
            // resolution, ...) always prefers the first-registered built-in
            // loader in delegation order, i.e. the empty Bootstrap stub, never
            // the real class just defined. Upgrade the existing stub in place
            // instead (same mechanism `load_class` uses when a stub's real
            // `.class` file later appears on the classpath), reusing its
            // ClassId so it becomes visible to every future lookup.
            // `CRATONVM_DEFINE_STUB_PROBE_BY_NAME` (wave 94, L5): no live class
            // of this name, no stub to upgrade -- without the per-user-loader
            // probes of `get_loaded_class_id`.
            let may_have_stub = !self.define_stub_probe_by_name
                || self.class_store.has_live_name(&stored_name_preview);
            if let Some(existing_id) = may_have_stub
                .then(|| self.get_loaded_class_id(&stored_name_preview))
                .flatten()
            {
                let is_stub = self
                    .class_store
                    .get(existing_id)
                    .map(|c| c.origin.is_compatibility_stub())
                    .unwrap_or(false);
                if is_stub {
                    self.upgrade_synthetic_class(
                        existing_id,
                        &stored_name_preview,
                        bytes,
                        loader_id,
                    )?;
                    self.charge_metaspace(existing_id, metaspace_charge);
                    return Ok(existing_id);
                }
            }
        }

        crate::define_census::phase_end(
            crate::define_census::DefinePhase::DefinePreChecks,
            pre_checks_clock,
        );
        let supertypes_clock = crate::define_census::phase_start_if(census_charged);

        // Mark this class as currently loading to detect circular hierarchies.
        // This guard is checked in load_class() before recursive calls.
        self.loading_guard.insert(name.to_string());

        // Recursively load the superclass (uses parent delegation too).
        // `class_file.super_class` is `Option<Arc<str>>`; `load_class` takes
        // `&str`, so deref through the Arc.
        //
        // Loader-faithful gate (CRATONVM_LOADER_AWARE_RESOLUTION): `load_class`
        // resolves a supertype via `get_loaded_class_id`, which returns ONE
        // class per name — the un-enhanced global copy. When a user loader that
        // defines its OWN per-loader copy of a supertype (e.g. Hibernate's
        // package-scoped `EnhancingClassLoader`, which enhances *every* in-package
        // class including the entity's superclass) defines a subclass, its super
        // link must point at the loader's enhanced copy, not the global one;
        // otherwise the subclass's vtable walk reaches the un-enhanced supertype
        // and a `$$_hibernate_read/write_<field>` accessor declared there is a
        // hard `NoSuchMethodError` (InheritedTest / MappedSuperclassTest crash on
        // `entity.anUnspecifiedObject`). Prefer a copy already defined by THIS
        // defining loader's namespace (populated first by
        // `preload_supertypes_via_loader`, JVMS §5.3.5 initiating-loader order),
        // falling back to the global `load_class`. Gated + only when the exact
        // copy exists → byte-identical gate-off / no same-loader copy.
        let loader_faithful = loader_aware_resolution() || options.force_loader_faithful_linking;
        let resolve_supertype = |this: &mut Self, internal: &str| -> Result<ClassId, VmError> {
            if loader_faithful {
                if let Some(id) = loaded_classes_probe(&this.loaded_classes, loader_id, internal) {
                    return Ok(id);
                }
                // JVMS 5.3.5: the defining loader is the INITIATING loader for
                // every supertype, so delegation applies. Without this the only
                // remaining option is `load_class`, which is loader-blind: for a
                // name that is also on the application classpath it happily
                // DEFINES A SECOND COPY in the application namespace and links
                // the subclass to that, which is how a Spring CGLIB proxy came
                // to extend a different copy of its own superclass than the
                // fork-loaded test class held.
                if let Some(id) =
                    loaded_class_via_parent_chain(
                        &this.loaded_classes,
                        &this.user_loader_parents,
                        loader_id,
                        internal,
                    )
                {
                    return Ok(id);
                }
                if crate::loaders::dbg_loader_chain() {
                    if let ClassLoaderId::UserDefined(ns) = loader_id {
                        let mut ancestors = [0u32; crate::loaders::MAX_USER_LOADER_DEPTH];
                        let n = this.user_loader_parents.ancestors(ns, &mut ancestors);
                        eprintln!(
                            "[loader-chain] supertype {internal} for ns {ns}: not in own ns, ancestors {:?} all missed -> global load",
                            &ancestors[..n]
                        );
                    }
                }
            }
            match this.load_class(internal) {
                // MATCHES THE VARIANT, not a message substring. The duplicate
                // raise site above used to build an
                // `IncompatibleClassChangeError` whose text contained "already
                // defined by", and this arm keyed on that text. Correcting the
                // raise site to `DuplicateClassDefinition` (HotSpot throws
                // `java.lang.LinkageError` itself) would have silently stopped
                // this arm from matching, turning a RECOVERED concurrent-
                // definition race into a hard failure -- so the two move
                // together. Keying on the variant instead of the wording is
                // also why the next wording change cannot break it.
                Err(VmError::Linkage(LinkageError::DuplicateClassDefinition {
                    class_name,
                    loader,
                })) => {
                    let message =
                        format!("duplicate class definition for {class_name} by {loader}");
                    // Benign concurrent-definition race: this recursive
                    // supertype/interface resolution (superclass or
                    // interfaces of the class currently being defined) lost
                    // a race against ANOTHER thread that independently
                    // defined the exact same (loader, name) pair in the
                    // meantime -- e.g. a background thread pool eagerly
                    // resolving a class (Spring Boot's
                    // OnClassCondition$ThreadedOutcomesResolver, or a
                    // Reactor Schedulers worker touching
                    // reactor/core/scheduler/NonBlocking) while the main
                    // thread recursively links a DIFFERENT class that also
                    // implements/extends it. Unlike a genuine duplicate
                    // `defineClass` call from application code, this path
                    // has no loader-level lock protecting it (that
                    // protection -- see native-builtins's
                    // `url_classloader_define_locks` -- only covers the
                    // top-level `URLClassLoader.findClass` entry point, not
                    // this internal recursive resolution). Prefer the
                    // winner's already-registered copy instead of failing
                    // this class's own definition outright.
                    match loaded_classes_probe(&this.loaded_classes, loader_id, internal) {
                        Some(id) => Ok(id),
                        None => {
                            let _ = &message;
                            Err(VmError::Linkage(LinkageError::DuplicateClassDefinition {
                                class_name,
                                loader,
                            }))
                        }
                    }
                }
                other => other,
            }
        };
        let validate_explicit_supertype =
            |this: &Self, id: ClassId, expected_name: &str| -> Result<ClassId, VmError> {
                match this.get_class(id) {
                    Some(class) if class.name.as_ref() == expected_name => Ok(id),
                    Some(class) => Err(VmError::Linkage(
                        LinkageError::IncompatibleClassChangeError {
                            message: format!(
                                "explicit supertype identity mismatch: class file names \
                                 {expected_name}, supplied ClassId {id:?} names {}",
                                class.name
                            ),
                        },
                    )),
                    None => Err(VmError::Linkage(LinkageError::NoClassDefFoundError {
                        class_name: expected_name.to_string(),
                    })),
                }
            };
        let superclass_id = match class_file.super_class {
            Some(ref super_name) => match options
                .superclass_id_override
                .map(|id| validate_explicit_supertype(self, id, super_name))
                .unwrap_or_else(|| resolve_supertype(self, super_name))
            {
                Ok(id) => Some(id),
                Err(e) => {
                    self.loading_guard.remove(name);
                    return Err(e);
                }
            },
            None => None, // java/lang/Object has no superclass
        };

        // SBR5 lane jfr: HotSpot's `JfrEventClassTransformer` adds the JFR
        // event schema (`eventConfiguration`, `startTime`, `duration` and the
        // five stub methods) to every concrete subclass of
        // `jdk.internal.event.Event` while it parses the class. Done here, with
        // the superclass resolved, so it covers indirect subclasses too; the
        // cached class bytes become the extended file, which is what the Java
        // instrumentation upcall expects to be handed. See
        // `jfr_event_schema` for the measured member list.
        let bytes =
            match self.extend_jfr_event_schema(superclass_id, loader_id, &bytes, &mut class_file) {
                Some(extended) => extended,
                None => bytes,
            };

        // No loader constraint for the superclass link (interpreter round i1
        // wave 37, lane L5): JVMS §5.3.4 constrains member descriptors and
        // overrides, and the subclass's loader having resolved the superclass
        // name is HotSpot's initiating-loader record, not a constraint. The
        // pin/impose pair that stood here was never read, grew the table by
        // two rows per define, and would have made the define-time check below
        // refuse a class on the say-so of a loader-blind supertype fallback.

        // Recursively load all interfaces. Each `iface_name` is `&Arc<str>`;
        // deref to `&str` for `load_class`. Same loader-faithful preference as
        // the superclass above (an enhanced subclass must link the loader's own
        // copy of an enhanced super-interface).
        let resolved_interfaces = if let Some(explicit) = options.interface_id_overrides.as_ref() {
            if explicit.len() != class_file.interfaces.len() {
                Err(VmError::Linkage(
                    LinkageError::IncompatibleClassChangeError {
                        message: format!(
                            "explicit interface identity count mismatch: class file has {}, \
                             caller supplied {}",
                            class_file.interfaces.len(),
                            explicit.len()
                        ),
                    },
                ))
            } else {
                explicit
                    .iter()
                    .zip(class_file.interfaces.iter())
                    .map(|(&id, name)| validate_explicit_supertype(self, id, name))
                    .collect::<Result<Vec<_>, _>>()
            }
        } else {
            class_file
                .interfaces
                .iter()
                .map(|iface_name| resolve_supertype(self, iface_name))
                .collect::<Result<Vec<_>, _>>()
        };
        let interface_ids: Vec<ClassId> = match resolved_interfaces {
            Ok(ids) => ids,
            Err(e) => {
                self.loading_guard.remove(name);
                return Err(e);
            }
        };

        // Loading guard no longer needed — class will be registered below
        self.loading_guard.remove(name);

        // JVMS §5.3.5 steps 3-4 and the class-file parser's supertype checks,
        // with HotSpot's messages: the direct superclass must be a class and
        // not final, and every direct superinterface an interface. They were
        // never asked here, so a class extending an interface or implementing
        // a class was defined, and a final superclass was left to the verifier
        // (a `VerifyError`, and nothing when verification is skipped).
        // `--real-jdk`, supertypes with real class bytes only (a compatibility
        // stub's flags are fabricated), not a redefinition. Interpreter round
        // i1 wave 29, lane L5; probe
        // `tools/probes/interp/L5/L5W29SupertypeShapeChecks.java`.
        // `--compatible` too with the `SupertypeShapeChecks` row (wave 50,
        // lane L5): the stub exclusion below keeps a fabricated supertype out.
        if self.aligned(cratonvm_types::HotSpotAlignment::SupertypeShapeChecks)
            && !options.allow_redefine
        {
            if let Some(message) = supertype_shape_violation(
                &class_file.this_class,
                superclass_id.and_then(|id| self.class_store.get(id)),
                interface_ids
                    .iter()
                    .filter_map(|&id| self.class_store.get(id)),
                |c| self.class_in_module_of_loader(c),
            ) {
                // The row-off route defined the class (wave 51, lane L5).
                self.note_alignment_divergence(
                    cratonvm_types::HotSpotAlignment::SupertypeShapeChecks,
                );
                return Err(VmError::Linkage(
                    LinkageError::IncompatibleClassChangeError { message },
                ));
            }
        }

        // Sealed supertypes (JEP 409), supertype access and the final-method
        // override check run once the runtime `Class` is built (it is the
        // accessor the access check needs): see `Self::supertype_link_refusal`
        // below (interpreter round i4 waves 84-85, lane L5).
        crate::define_census::phase_end(
            crate::define_census::DefinePhase::DefineSupertypes,
            supertypes_clock,
        );
        let build_clock = crate::define_census::phase_start_if(census_charged);

        // Compute field layout
        let (first_field_index, num_total_fields) =
            compute_field_layout(&class_file.fields, superclass_id, &self.class_store);

        // Wave 3-B (RE.4): some real-JDK classes (e.g. java.net.InetSocketAddress
        // = 1 instance field `holder`) have a much smaller declared field count
        // than the synthetic-mode field layout used by `native-builtins`. Native
        // `<init>` methods write to the synthetic indices; without padding the
        // object would lack slots for those writes (panic on set_field) or the
        // slots would never be allocated (so reads see uninitialised slots and
        // misreport as e.g. `port is not an int`). Pad with the larger of the
        // declared count and the synthetic stub layout.
        //
        // Except for the classes [`FLOOR_EXEMPT_CLASSES`] names, where the
        // floor describes the FABRICATED layout and nothing else: no native
        // writes a raw absolute slot past what the real class declares, so the
        // padding buys nothing and costs the whole object. See
        // `apply_synthetic_floor`.
        let num_total_fields = self.apply_synthetic_floor(name, superclass_id, num_total_fields);
        if field_trace_enabled()
            && (name.contains("DefaultHttpMessageConverters")
                || name.contains("AnsiOutputApplicationListener")
                || name.contains("AutoConfigurationPackages")
                || name.contains("ClearCachesApplicationListener")
                || name.contains("FileEncodingApplicationListener"))
        {
            let sup = superclass_id
                .and_then(|s| self.class_store.get(s))
                .map(|c| (c.name.to_string(), c.num_total_fields));
            eprintln!("[FIELD-TRACE] define {name}: first_field_index={first_field_index} num_total_fields={num_total_fields} (own_instance computed via compute_field_layout) super={sup:?}");
        }

        // Build the runtime Class
        let id = self.class_store.next_id();

        // Round 4 audit fix (HIGH): single-pass class-attribute extraction.
        //
        // The previous code ran ~10 independent `attributes.iter().find_map(...)`
        // walks (one each for SourceFile, BootstrapMethods, annotations,
        // Signature, NestHost, NestMembers, Record, PermittedSubclasses,
        // InnerClasses, EnclosingMethod, Module). For a typical class with N
        // class-level attributes, that's ~10 × N pattern-match operations and
        // ~10 × N `as_decoded()` calls on every class load. Folded into a
        // single iteration here.
        //
        // All class-level attributes were force-decoded immediately after
        // parsing, so `as_decoded()` returns `Some` for every entry.
        let cp = &class_file.constant_pool;
        let mut source_file: Option<String> = None;
        let mut bootstrap_methods = Vec::new();
        let mut annotations = Vec::new();
        // SBR wave 5 (reflmeta): see `ClassManager::class_type_annotations`.
        let mut class_type_annotations: Option<Vec<u8>> = None;
        let mut signature: Option<String> = None;
        // NestHost, NestMembers, Record, PermittedSubclasses, InnerClasses and
        // EnclosingMethod: one reader shared with `upgrade_synthetic_class`
        // (interpreter round i1 wave 27, lane L5), in this same single pass.
        let mut nesting = NestingAttributes::default();
        let mut module_name_from_attr: Option<String> = None;
        for attr in &class_file.attributes {
            match attr.as_decoded() {
                Some(Attribute::SourceFile(name)) => {
                    if source_file.is_none() {
                        source_file = Some(name.to_string());
                    }
                }
                Some(Attribute::BootstrapMethods(bms)) => {
                    if bootstrap_methods.is_empty() {
                        bootstrap_methods = bms.clone();
                    }
                }
                // Only `RuntimeVisibleAnnotations` (@Retention(RUNTIME)) are
                // exposed via reflection. `RuntimeInvisibleAnnotations` carry
                // @Retention(CLASS) types which JVMS requires NOT be visible
                // through Class.getAnnotation / isAnnotationPresent — see
                // gap-annotation-retention-policy.md.
                Some(Attribute::RuntimeVisibleAnnotations(anns)) => {
                    annotations.extend(anns.iter().cloned());
                }
                Some(a @ Attribute::RuntimeVisibleTypeAnnotations(_)) => {
                    if class_type_annotations.is_none() {
                        class_type_annotations =
                            cratonvm_reader::attribute::annotation_attribute_body(a);
                    }
                }
                Some(Attribute::Signature(s)) => {
                    if signature.is_none() {
                        signature = Some(s.to_string());
                    }
                }
                Some(Attribute::Module(m)) => {
                    if module_name_from_attr.is_none() {
                        module_name_from_attr = cp.get_utf8(m.name_index).map(|s| s.to_string());
                    }
                }
                Some(other) => nesting.absorb(other, cp),
                None => {}
            }
        }
        // JIT round 21 wave 26, lane nesting: `EnclosingMethod` as HotSpot
        // stores it (dropped without `InnerClasses`;
        // `CRATONVM_ENCLOSING_METHOD_NEEDS_INNER_CLASSES`).
        let NestingAttributes {
            nest_host,
            nest_members,
            record_components,
            has_record_attribute,
            permitted_subclasses,
            inner_classes,
            enclosing_method,
            has_inner_classes_attribute: _,
        } = nesting.hotspot_stored(true);
        // HotSpot ignores a circular / repeated-inner `InnerClasses` table
        // (`inner_classes_ignored`, above; interpreter round i5 wave 101);
        // zs s4/rp: a kept table's entries carry their flags as HotSpot
        // stores them.
        let inner_classes = if inner_classes_ignored {
            Vec::new()
        } else {
            hotspot_stored_inner_classes(inner_classes, class_file.version.major)
        };
        // See `ClassManager::empty_record_classes`.
        let empty_record = has_record_attribute && record_components.is_empty();

        // If this class IS a module-info declaration, register it in the module
        // registry (covers module-info.class files loaded lazily during class
        // resolution, supplementing the eager scan in new()).
        if name.ends_with("module-info") || name == "module-info" {
            if let Some(mut desc) = class_file.attributes.iter().find_map(|a| {
                a.as_decoded()
                    .and_then(|d| descriptor_from_module_attribute(d, &class_file.constant_pool))
            }) {
                // Mirror the eager scan: a lazily-loaded module-info that is NOT a
                // genuine platform module (java.*/jdk.*/…) is a class-path jar and
                // gets automatic-module access semantics. Without this, a lazy
                // re-register would overwrite the eager `automatic=true` entry with
                // a strict one and re-break readability (e.g. org.jboss.logging).
                //
                // …but NOT over an existing EXPLICIT registration. `vm_init`
                // re-registers every `--module-path` module with
                // `automatic = false` immediately after `ClassManager::new`
                // (whose app-class-path scan had stamped it `true`, because the
                // module roots are also put on the application search path).
                // `is_platform_module_name` is false for any app-named module,
                // so without this guard a later lazy load of that same
                // `module-info` silently downgrades a `--module-path` module
                // back to automatic — and `automatic` is now load-bearing:
                // `populate_boot_layer_modules` reads it (through
                // `NativeContext::module_is_class_path_only`) to decide what may
                // enter `ModuleLayer.boot()` and the system loader's
                // `ServicesCatalog`. A downgrade there would evict a genuine
                // module-path module from the boot layer, which is what
                // `regression-suite/src/RJdkModule.java` asserts against.
                // Nothing is lost for the case the paragraph above defends: a
                // `-cp` jar such as `org.jboss.logging` is never registered
                // explicit by anyone, so it still lands on `automatic = true`.
                // See docs/feature-designs/class-resolution-and-caching.md.
                let already_explicit = self
                    .module_registry
                    .get(&desc.name)
                    .is_some_and(|prev| !prev.automatic);
                desc.automatic = !already_explicit && !is_platform_module_name(&desc.name);
                let packages: Vec<String> = class_file
                    .attributes
                    .iter()
                    .find_map(|a| {
                        a.as_decoded().and_then(|d| {
                            packages_from_module_packages_attribute(d, &class_file.constant_pool)
                        })
                    })
                    .unwrap_or_default();
                self.module_registry.register(desc, packages);
                self.module_registry.build_readability_graph();
            }
        }

        // Determine this class's module membership from the package registry
        // (N4: unnamed-module baseline + named-module assignment).
        //
        // `named_module_for_package`, NOT `module_for_package`: a modular jar
        // reached through the CLASS path keeps its descriptor in the registry
        // (service discovery and labelling want it) but a real JVM puts its
        // classes in the UNNAMED module, and MEMBERSHIP is the surface where
        // that difference is observable — `--add-opens java.base/…=ALL-UNNAMED`
        // is qualified to the unnamed module, so a class wrongly labelled with
        // its jar's declared module name cannot be granted by it. Measured on
        // Tomcat's `catalina.jar` (which declares `org.apache.tomcat.catalina`);
        // see the note on `named_module_for_package`. `ClassManager::new`'s own
        // comment has said "the real JDK puts them in the unnamed module" since
        // the eager scan was written — this is the half of that sentence the
        // scan could not implement on its own.
        let pkg = package_of(&class_file.this_class);
        let module_name = if loader_flags().classpath_jar_unnamed_module {
            self.module_registry.named_module_for_package(pkg)
        } else {
            self.module_registry.module_for_package(pkg)
        }
        .map(|s| s.to_string())
        .or(module_name_from_attr);
        // A user-defined loader's class is never a member of a JDK platform
        // module: those modules are defined to the bootstrap / platform /
        // application loaders, and `ClassLoader.defineClass` puts a class of
        // any other loader in that loader's unnamed module (HotSpot:
        // `getModule().isNamed()` is false for a child-first loader's own
        // `javax.security.auth.x500.X500PrivateCredential`; CratonVM labelled it
        // `java.base` by its package). `--real-jdk` only; `--compatible` keeps
        // the package label (AGENTS.md; open on
        // `docs/feature-designs/class-resolution-and-caching.md`).
        // Access control already judged such a class this way
        // (`access_control.rs` `member_module_of`, wave 26). Interpreter round
        // i1 wave 28, lane L5; probe `L5W28JdkNamedModule`.
        // (`jdk.proxyN`, a dynamic proxy's own named module, IS defined to the
        // proxy's loader, and is kept.)
        let module_name = module_name.filter(|m| {
            let unnamed = matches!(loader_id, ClassLoaderId::UserDefined(_))
                && self.aligned(cratonvm_types::HotSpotAlignment::UserLoaderUnnamedModule)
                && is_platform_module_name(m)
                && !m.starts_with("jdk.proxy");
            if unnamed {
                // The row-off route kept the platform module's name (wave 51,
                // lane L5).
                self.note_alignment_divergence(
                    cratonvm_types::HotSpotAlignment::UserLoaderUnnamedModule,
                );
            }
            !unnamed
        });

        // NEW-8 + WP2.3: hidden classes register under a mangled name
        // (e.g. `Foo/0x1`) distinct from their class file's `this_class`.
        // All other callers get the stored name straight from the file.
        //
        // WP2.3: when `hidden = true`, the override name itself MUST be
        // unique. If a caller supplies an `override_name` that already
        // exists in this loader (collision from emitting two hidden
        // classes from the same template), we append a monotonic counter
        // suffix `/0x<n>` so each hidden class still gets a distinct
        // identity in the class store. Callers (e.g.
        // `Lookup.defineHiddenClass`) typically pre-mangle, but the
        // belt-and-suspenders check here means a probe that re-uses an
        // override_name across calls won't mysteriously fail with
        // duplicate-define on the second call.
        // Same boundary conversion as `stored_name_preview` above:
        // `class_file.this_class` is `Arc<str>` but the surrounding code
        // operates on `String`. Convert once here.
        let mut stored_name: String = options
            .override_name
            .clone()
            .unwrap_or_else(|| class_file.this_class.to_string());
        if options.hidden {
            // The probe of duplicates must consider the (loader, name)
            // composite key — two hidden classes with the same internal
            // name in different loaders are fine.
            //
            // C34 audit fix (HIGH): borrowed-key `raw_entry` probe — no
            // `Arc::from(&str)` per iteration. The dominant per-iteration
            // cost remains the `format!` building the suffixed name, but
            // the cumulative allocation tax across the loop (and across
            // every hidden-class define) is now zero.
            let mut probe_name = stored_name.clone();
            while loaded_classes_probe(&self.loaded_classes, loader_id, &probe_name).is_some() {
                self.hidden_name_counter = self.hidden_name_counter.wrapping_add(1);
                probe_name = format!("{}/0x{:x}", stored_name, self.hidden_name_counter);
            }
            stored_name = probe_name;
        }
        // Interpreter round i5 wave 102, lane L3
        // (`i102-L3-hidden-class-from-member-bytes-shape`): HotSpot's
        // `ClassFileParser` rewrites a hidden class's `this_class` Utf8 to the
        // mangled name, so each `InnerClasses` entry naming the class file's
        // `this_class` (javac shares that pool entry) names the hidden class
        // itself: its self entry still makes it a member class (modifiers,
        // simple binary name), and `getDeclaringClass0` then refuses it
        // because the outer class cannot list the mangled name.
        // JIT round 21 wave 21 (items 3-4 of `r21-review-w21-findings-FIXED-20261020.md`):
        // a hidden class's own class-file name, recorded for the self-name rule
        // ([`ClassManager::hidden_class_file_name`]) so it does not depend on
        // how the mint site spelled the stored name.
        let hidden_file_name: Option<Arc<str>> = options
            .hidden
            .then(|| Arc::clone(&class_file.this_class))
            .filter(|name| !name.is_empty());
        // Wave 22 (lane hidden-resid, `CRATONVM_HIDDEN_SELF_RECORDED_NAME`):
        // an ORDINARY define under another stored name keeps its class-file
        // name, for a later `mark_class_hidden`.
        let renamed_file_name: Option<Arc<str>> = (self.class_store.recorded_self_name()
            && !options.hidden
            && !class_file.this_class.is_empty()
            && options
                .override_name
                .as_deref()
                .is_some_and(|stored| stored != &*class_file.this_class))
        .then(|| Arc::clone(&class_file.this_class));
        let inner_classes = if options.hidden && *stored_name != *class_file.this_class {
            let this_name: &str = &class_file.this_class;
            inner_classes
                .into_iter()
                .map(|mut entry| {
                    if entry.inner_class == this_name {
                        entry.inner_class = ThinArcStr::from(&stored_name[..]);
                    }
                    if entry.outer_class == this_name {
                        entry.outer_class = ThinArcStr::from(&stored_name[..]);
                    }
                    entry
                })
                .collect()
        } else {
            inner_classes
        };

        // WP2.3: prefer caller-supplied CodeSource (from
        // `defineClass(... ProtectionDomain pd)`) over classpath
        // discovery. If neither is available (a class generated entirely
        // in memory — e.g. CGLIB / ByteBuddy / dynamic proxies) we
        // synthesize a stable `file:/runtime-defined/<class>.class` URL
        // so `Class.code_source` is *non-null* on every defined class.
        //
        // Why non-null matters: real-JDK `ClassLoader.preDefineClass`
        // calls `pd.getCodeSource()` on the resulting `ProtectionDomain`
        // unconditionally; if our `getProtectionDomain0()` native
        // returns null (which it does when `Class.code_source` is None)
        // any subsequent `pd.getCodeSource()` invocation NPEs with
        // "Cannot invoke getCodeSource on null" — exactly the pre-fix
        // behaviour observed by `apps/cglib_probe/cglib.trace.log`.
        //
        // A `file:/` URL satisfies the native's "non-bootstrap" filter
        // (which suppresses PD only for empty / `class:` URIs that
        // signal a true bootstrap class), so the native materialises a
        // real PD whose `getCodeSource()` returns a real `CodeSource`.
        //
        // The `runtime-defined` segment is intentionally chosen so it
        // is parseable as a `URL` by the JDK side and so policy files
        // can `grant codeBase "file:/runtime-defined/-"` to scope
        // permissions for emitted classes.
        let code_source_clock = crate::define_census::phase_start_if(census_charged);
        let skip_class_path_code_source = self.define_no_class_path_code_source
            && (options.hidden
                || options.user_loader_define
                || matches!(loader_id, ClassLoaderId::UserDefined(_)));
        let code_source = match options.code_source.clone() {
            Some(cs) => Some(cs),
            None if skip_class_path_code_source => Some(CodeSource::from_url(format!(
                "file:/runtime-defined/{}.class",
                stored_name
            ))),
            None => self.find_class_code_source_for_loader(name, loader_id).or_else(|| {
                // Fall back to a synthetic URL that the native
                // ProtectionDomain builder will accept. Use the
                // *stored* (potentially mangled) name so each hidden
                // class gets a distinct code base.
                Some(CodeSource::from_url(format!(
                    "file:/runtime-defined/{}.class",
                    stored_name
                )))
            }),
        };
        crate::define_census::phase_end(
            crate::define_census::DefinePhase::CodeSource,
            code_source_clock,
        );

        // WP2.3: nest-host attribution. The `Lookup.defineHiddenClass(...
        // NESTMATE ...)` path passes `options.nest_host_class_name =
        // Some(<lookup_class>)`. We apply it AFTER the class-file's
        // own NestHost attribute parsing so the explicit option wins.
        let declared_nest_host = options.nest_host_class_name.clone().or(nest_host);

        // W3-2 (JEP 371 / JVMS §5.4.4): a hidden class defined WITHOUT
        // `ClassOption.NESTMATE` is in a nest of its OWN — even when the
        // supplied bytes carry a `NestHost` attribute. See
        // `hidden_class_drops_class_file_nest_host` for the full reasoning.
        let nest_host = if hidden_class_drops_class_file_nest_host(
            options.hidden,
            options.nest_host_class_name.as_deref(),
        ) {
            None
        } else {
            declared_nest_host.clone()
        };

        // Real-JDK mode (contract §5): record where these bytes came from
        // BEFORE they are moved into the `Class` literal. Real bytes reached
        // this point, so no branch below can produce a `CompatibilityStub` —
        // this path is never a compatibility substitution.
        //
        // Origin classification deliberately reads the DECLARED nest host, not
        // the effective one: `ClassOrigin::GeneratedLambda { host }` is a
        // provenance label ("who asked for these bytes"), and the W3-2 drop
        // above is a nest-membership decision. Passing `declared_nest_host`
        // keeps every existing origin verdict byte-identical.
        let defined_origin = self.classify_defined_origin(
            &stored_name,
            loader_id,
            &options,
            module_name.as_deref(),
            code_source.as_ref().and_then(|cs| cs.url.as_deref()),
            declared_nest_host.as_deref(),
            &interface_ids,
        );

        let mut class = Class {
            id,
            loader_id,
            // Intern the class name through the global pool. If the reader
            // already interned this exact `this_class` string (the common
            // path), this is a hash-lookup + refcount bump — no allocation.
            name: cratonvm_types::intern_arc(&stored_name),
            source_file,
            version: class_file.version,
            // CDS-loaded classes are pre-verified — skip straight to Verified state.
            state: if self.cds_class_cache.contains_key(name) {
                ClassState::Verified
            } else {
                ClassState::Loaded
            },
            initializing_thread: None,
            constant_pool: class_file.constant_pool,
            // zs s4/rp: as HotSpot's ClassFileParser stores them.
            access_flags: ClassAccessFlagsBits::from_bits_retain(hotspot_stored_class_flags(
                class_file.access_flags.bits(),
                class_file.version.major,
            )),
            superclass: superclass_id,
            interfaces: interface_ids,
            fields: class_file.fields,
            methods: class_file.methods,
            first_field_index,
            num_total_fields,
            bootstrap_methods,
            signature,
            annotations,
            nest_host,
            nest_members,
            record_components,
            permitted_subclasses,
            inner_classes,
            enclosing_method,
            // NEW-8: `options.hidden` is true when called via
            // `Lookup.defineHiddenClass`. Setting it here keeps the
            // flag atomic with the class registration so
            // `find_class_by_name` cannot observe a brief non-hidden
            // window between insert and the subsequent
            // `set_class_hidden` call.
            hidden: options.hidden,
            module_name,
            // Installed together with the derived `is_synthetic_stub` by the
            // `set_origin` call below.
            origin: ClassOrigin::default(),
            has_finalizer: false, // computed below
            code_source,
            array_info: None,
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
            record_object_methods: std::sync::atomic::AtomicU8::new(0),
        };
        // Never a `CompatibilityStub`, so `is_synthetic_stub` stays `false`
        // exactly as the previous literal hard-coded it. Not yet stored: the
        // in-place provenance epoch stays put (wave 94).
        class.set_origin_of_unstored_class(defined_origin);

        // Compute has_finalizer: true if this class or any ancestor
        // overrides Object.finalize() (JLS §12.6), by HotSpot's rule: an empty
        // override is no finalizer and cancels an inherited one (zt t18/hunt2
        // and JIT round 21 wave 17; see `Self::computed_has_finalizer`).
        let parent_has = superclass_id
            .and_then(|sid| self.class_store.get(sid))
            .map_or(false, |parent| parent.has_finalizer);
        // zt t18/hunt2: HotSpot's rule, see `Self::computed_has_finalizer`.
        class.has_finalizer = self.computed_has_finalizer(&class, parent_has);
        crate::define_census::phase_end(crate::define_census::DefinePhase::DefineBuild, build_clock);
        let link_refusal_clock = crate::define_census::phase_start_if(census_charged);

        // HotSpot `ClassFileParser`'s `check_super_class_access`,
        // `check_super_interface_access` and `check_final_method_override`,
        // in that order (see `Self::supertype_link_refusal`).
        if !options.allow_redefine {
            if let Some(refusal) =
                self.supertype_link_refusal(&class, options.skip_verification)
            {
                return Err(VmError::Linkage(refusal));
            }
        }
        crate::define_census::phase_end(
            crate::define_census::DefinePhase::DefineLinkRefusal,
            link_refusal_clock,
        );

        // Audit-fix #1 (CRITICAL): run the JVMS §5.4.1 Pass 2 / Pass 3
        // verifier before the class is registered. Previously
        // `define_class_with_options` recorded `skip_verification` in a
        // side set but NEVER actually invoked the verifier, so every
        // caller of `Unsafe.defineClass` / `MethodHandles.Lookup
        // .defineClass` got a structurally valid but unverified class.
        //
        // Skip cases (each matches HotSpot's policy):
        //   - `options.skip_verification` — trusted runtime-generated
        //     classes (CGLIB, ByteBuddy, JDK Proxy, hidden classes) that
        //     emit bytecode our worklist verifier cannot model. The
        //     caller asserted trust by setting the flag.
        //   - CDS-cached bytes — already verified at archive-creation
        //     time; re-verifying would be redundant.
        //   - Synthetic stubs — created in-memory with empty bodies, no
        //     bytecode to verify (real-bytecode upgrade path runs
        //     verification on the replacement).
        //
        // On failure the class is dropped (never reaches the store /
        // loaded_classes map) and the caller receives a typed
        // `VmError::Linkage(LinkageError::VerifyError { .. })`.
        // User-defined loaders may legitimately hold an isolated copy of a
        // class that is also present in the application loader. Pass 3 tracks
        // names but its current hierarchy adapter cannot preserve both loader
        // identities through every pre-definition edge, producing false
        // areturn/checkcast VerifyErrors for otherwise valid forked bytecode.
        // Keep structural validation and defer that loader-sensitive Pass 3,
        // matching the link-time verifier policy in vm_util.
        //
        // `-Xverify:all` withdraws the deferral. The deferral trades a
        // type-state verdict for loader fidelity, and that trade is exactly
        // what a deployment asking for maximum scrutiny is refusing to make —
        // it is also how a `multianewarray` with more dimensions than its
        // descriptor has brackets reached the interpreter unchallenged from
        // every Spring/Tomcat/H2 application class (fixed in a01ccc442).
        let defer_loader_sensitive_pass3 = !self.strict_verification
            && loader_aware_resolution()
            && matches!(class.loader_id, ClassLoaderId::UserDefined(_));
        // `-Xverify:none` / `-noverify` (t1 wave 9, lane I24V,
        // `CRATONVM_NOVERIFY_SKIPS_DEFINE_PASS3`; see
        // [`Self::set_skip_remote_verification`]): HotSpot's
        // `BytecodeVerificationRemote` is off, so a non-boot class gets no
        // type-state verdict. It takes the deferred route below with the
        // verdict dropped: Pass 2 and the structural scan at define, the type
        // maps harvested, nothing pending for the link.
        let noverify_remote = self.skip_remote_verification
            && !self.strict_verification
            && loader_id != ClassLoaderId::Bootstrap
            && !options.allow_redefine;
        // TYPE MAPS (arch-2026-07-26/access-control-and-map-coverage): the
        // deferral above is about the verifier's *load decision*, not about
        // its type maps. Deferring the whole of Pass 3 also deferred the
        // per-pc oop maps and the per-method `safe_for_fast_path` flag, and
        // since every Spring / WildFly / H2 / Elasticsearch application class
        // is defined by a user-defined loader, that meant no maps for the
        // entire application workload — precisely the classes the maps exist
        // to describe. `collect_class_type_maps` runs the same walk with the
        // verdict discarded; see its doc comment for why the rows are sound
        // even when the assignability verdicts are not.
        let skip_all_verification = options.skip_verification
            || self.cds_class_cache.contains_key(name)
            || class.origin.is_compatibility_stub()
            || class.state == ClassState::Verified;
        // HotSpot's verifier loads both sides of a reference assignability
        // check through the verified class's loader, at link time. Record the
        // names here for the VM's link step to load
        // (`i47-verifier-assignability-checks-initiate-no-loads`). HotSpot
        // does not verify the bootstrap loader's classes
        // (`BytecodeVerificationLocal` is off).
        let verifier_loads = std::cell::RefCell::new(Vec::new());
        //
        // zt df4 §9c: a class of a platform or tool module of the runtime
        // image is the PLATFORM / application loader's on HotSpot (verified,
        // `BytecodeVerificationRemote`), though this VM defines it in the
        // boot namespace: its loads are recorded too, so that loader
        // initiates them (`CRATONVM_FIND_LOADED_BUILTIN_IMAGE_ROWS`, default
        // ON, `=0` kill).
        let record_verifier_loads = (loader_id != ClassLoaderId::Bootstrap
            || self.boot_tagged_image_class_of_builtin_loader(class.module_name.as_deref()))
            && !options.allow_redefine
            && self.aligned(cratonvm_types::HotSpotAlignment::VerifierLoadsAtLink);
        // `CRATONVM_DEFINE_TYPE_MAPS_AT_LINK`: set when the deferred branch
        // below leaves the type-map walk to the class's link.
        let mut type_maps_at_link = false;
        // Set with it when the link owes the structural scan too (below).
        let mut boot_structural_at_link = false;
        // `CRATONVM_VERIFY_USER_LOADER_CLASSES_AT_LINK`: set when the deferred
        // branch below leaves the Pass 3 verdict to the class's link.
        let mut user_pass3_at_link = false;
        let verify_clock = crate::define_census::phase_start_if(census_charged);
        if !skip_all_verification {
            let hierarchy = ClassStoreHierarchy {
                class_store: &self.class_store,
                loaded_classes: &self.loaded_classes,
                user_loader_parents: &self.user_loader_parents,
                // Audit fix (HIGH #1): the class being verified is not yet
                // in `class_store`; pass it explicitly so self-references
                // (its own name / id) resolve during verification.
                in_flight: Some(&class),
                in_flight_super_name: class_file.super_class.as_deref(),
                // CL-CLASSMANAGER fix: make type resolution loader-aware —
                // referenced names resolve through this class's defining
                // loader's delegation order, not a global first-match.
                requesting_loader: Some(class.loader_id),
                verifier_loads: record_verifier_loads.then_some(&verifier_loads),
                app_workarounds: &self.app_workarounds,
            };
            let route_clock = crate::define_census::phase_start_if(census_charged);
            // Pass 2's access-flag and `Code` presence rules are format
            // checks: the define's own format scope judges them (t1 wave 13,
            // lane I24W; see `verifier::verify_class_structure_with`).
            let format_checked = legal_modifiers_need_verify && structural_need_verify;
            // `CRATONVM_DEFINE_BOOT_MAPS_AT_LINK` (interpreter round i4 wave
            // 90, lane L5; see the field): a trusted bootstrap class with code
            // takes the deferred route's shape -- Pass 2 and the structural
            // scan here, the type-map walk at its link -- instead of the full
            // Pass 3, which HotSpot does not run for it.
            let skip_boot_verify = (self.skip_boot_verification
                || (!defer_loader_sensitive_pass3 && self.class_store.type_maps().drops_rows()))
                && !self.strict_verification
                && !options.hidden
                && !options.allow_redefine
                && crate::bytecode_verifier::class_is_bootstrap_trusted(&class);
            let boot_maps_at_link = !skip_boot_verify
                && self.define_boot_maps_at_link
                && self.type_maps_at_link
                && !self.strict_verification
                && !defer_loader_sensitive_pass3
                && !options.hidden
                && !options.allow_redefine
                && crate::bytecode_verifier::class_is_bootstrap_trusted(&class)
                && class.methods.iter().any(|m| m.code().is_some());
            if skip_boot_verify {
                if let Err(verify_err) =
                    crate::verifier::verify_class_structure_with(
                        &class,
                        &self.class_store,
                        format_checked,
                    )
                {
                    self.loading_guard.remove(name);
                    return Err(VmError::Linkage(verify_err));
                }
                if self.class_store.type_maps().drops_rows() {
                    self.class_store.type_maps().mark_skipped(class.id);
                }
                crate::define_census::phase_end(
                    crate::define_census::DefinePhase::VerifyDeferred,
                    route_clock,
                );
            } else if boot_maps_at_link {
                // Pass 2 here, as the full route runs it. The
                // hierarchy-independent half of Pass 3 (the structural scan)
                // moves to the link with the walk (Phase 5 fast boot, lane
                // p5-define): the scan and the walk each decoded every method,
                // so a linked class decoded each twice; the link now scans
                // first and the walk reuses the scan's decodes
                // (`verifier::publish_deferred_class_type_maps_scanned`). No
                // code of the class runs before its link, and a scan error
                // refuses the link with the same `VerifyError` the define
                // raised (HotSpot verifies none of these classes, so neither
                // point is HotSpot's; a class defined and never linked no
                // longer pays the scan, as HotSpot pays nothing).
                if let Err(verify_err) =
                    crate::verifier::verify_class_structure_with(
                        &class,
                        &self.class_store,
                        format_checked,
                    )
                {
                    self.loading_guard.remove(name);
                    return Err(VmError::Linkage(verify_err));
                }
                type_maps_at_link = true;
                boot_structural_at_link = true;
                crate::define_census::phase_end(
                    crate::define_census::DefinePhase::VerifyDeferred,
                    route_clock,
                );
            } else if defer_loader_sensitive_pass3 || noverify_remote {
                // SECURITY: the deferral exists because this adapter's
                // *assignability verdicts* can be wrong under two loaders that
                // hold same-named classes — it is a statement about the
                // hierarchy, not a licence to skip the structural envelope.
                // Before this call the deferred path made NO load decision at
                // all, so a class defined by a user-defined loader (i.e. every
                // Spring / WildFly / H2 application class) could carry an
                // out-of-range branch, a mid-instruction exception handler, an
                // under-declared `max_locals` or a local operand past the end
                // of the frame, and reach the interpreter unchallenged.
                //
                // `verify_class_structural_bytecode` is the hierarchy-INDEPENDENT
                // half of Pass 3 (JVMS §4.9.1): decode, branch/handler bounds
                // and instruction-boundary landing, local-index and `max_locals`
                // conformance. It cannot produce the loader-confusion false
                // rejections the deferral was introduced to avoid, because it
                // never asks the hierarchy a question.
                //
                // JIT round 20 (lane verif): `verify_deferred_class_bytecode`
                // runs that scan and then the JVMS §4.10.1.8 protected-receiver
                // and §4.10.1.9 invokespecial-target rules, which HotSpot
                // applies to every class a user-defined loader defines. They
                // ask the hierarchy only about the class's LINKED supertypes
                // and loaded member flags (and treat an unloaded receiver class
                // as assignable), so they add no loader-confusion refusal; see
                // the function's doc.
                // Interpreter round i5 wave 102, lane L7
                // (`CRATONVM_VERIFY_USER_LOADER_CLASSES_AT_LINK`, default on):
                // HotSpot gives the verdict when the class is LINKED, so a bad
                // class defined and never linked raises nothing. The define
                // keeps the structural scan; the linkage rules and the
                // type-state walk run at the link
                // ([`Self::verify_pending_user_pass3`]). A hidden class is
                // linked inside its define on HotSpot, and a redefinition is
                // never linked again, so both keep the define-time verdict.
                // `noverify_remote` alone (an application- or platform-loader
                // class under `-Xverify:none`, t1 wave 9 lane I24V) leaves
                // nothing for the link: its link skips the verifier outright.
                let pass3_at_link = defer_loader_sensitive_pass3 && {
                    let flags = loader_flags();
                    flags.verify_user_loader_classes
                        && flags.verify_user_loader_classes_at_link
                        && !options.hidden
                        && !options.allow_redefine
                };
                // Interpreter round i5 wave 103, lane L7
                // (`i102-L7-structural-scan-refuses-at-define`): the structural
                // scan moves to the link with the verdict. HotSpot's
                // `ClassFileParser` checks only the format at define (the
                // `Arguments can't fit into locals` refusal is
                // `member_modifier_refusal`'s); branch targets, handler
                // boundaries and local indices are its verifier's, at link. The
                // link's full walk runs the scan first
                // (`verify_method_structural_shared`, with HotSpot's texts) and
                // no code of an unlinked class runs; under `--noverify` the link
                // still runs the scan alone
                // ([`Self::verify_pending_user_structural`]).
                let define_verdict = if pass3_at_link {
                    Ok(())
                } else if noverify_remote {
                    // `-Xverify:none` (lane I24V): no verifier rule and no
                    // type-state verdict. An application / platform class
                    // keeps the Pass 2 it had at define before (its link
                    // skips Pass 2 under `--noverify`); both keep the
                    // structural envelope the decoded interpreter relies on.
                    let structure = if defer_loader_sensitive_pass3 {
                        Ok(())
                    } else {
                        crate::verifier::verify_class_structure_with(&class, &self.class_store, format_checked)
                    };
                    structure
                        .and_then(|()| crate::verifier::verify_class_structural_bytecode(&class))
                } else {
                    crate::verifier::verify_deferred_class_bytecode(&class, &hierarchy)
                        // Interpreter round i5 wave 101, lane L7: and the
                        // type-state verdict HotSpot gives every non-boot
                        // class, undecidable assignability answered
                        // "assignable" (`CRATONVM_VERIFY_USER_LOADER_CLASSES`,
                        // default on, `=0` off).
                        .and_then(|()| {
                            crate::verifier::verify_user_loader_class_bytecode(&class, &hierarchy)
                        })
                };
                if let Err(verify_err) = define_verdict {
                    self.loading_guard.remove(name);
                    return Err(VmError::Linkage(verify_err));
                }
                user_pass3_at_link = pass3_at_link;
                // Loader-sensitive class: harvest the maps, make no *type-state*
                // load decision. Publishing before registration is safe — the id
                // is already minted, the store is the only other thing keyed by
                // it, and nothing between here and `class_store.add` can fail.
                //
                // OBSERVABILITY: the type-state verdict was withheld, so the
                // class's maps carry `FastPathVeto::IncompleteWalk` for any
                // method whose walk did not finish and every consumer stays on
                // the conservative path. See `docs/security/verifier/coverage.md`
                // for what this deferral still leaves open.
                debug!(
                    class = %class.name,
                    loader = %loader_id,
                    "Pass 3 type-state verdict deferred (loader-sensitive); \
                     structural bytecode verification enforced",
                );
                // `CRATONVM_DEFINE_TYPE_MAPS_AT_LINK` (interpreter round i3
                // wave 81, lane L5): the walk below runs no check, so it can
                // run when the class is linked, before any of its code runs,
                // outside this write lock; a class defined and never linked
                // never pays it, as on HotSpot. Until then the store answers
                // `Unknown` for the class (conservative scan, no unchecked
                // fast path), which no code of an unlinked class reaches.
                //
                // Only a class with a method that HAS a `Code` attribute owes
                // the link anything: for one with none (no methods, or only
                // abstract / native ones) the walk records no row and no
                // verifier load, so it is published here exactly as with the
                // switch off and gets no pending row -- such a define pays
                // nothing for the switch, at define or at link (wave-81 host
                // A/B: `define-plain` read +20% with every class pending).
                // An application / platform class under `-Xverify:none` (lane
                // I24V) publishes at define, where its full Pass 3 published
                // before: its link is "trivial" under `--noverify`
                // (`vm_util::class_link_is_trivial`) and may be skipped.
                if self.type_maps_at_link
                    && defer_loader_sensitive_pass3
                    && class.methods.iter().any(|m| m.code().is_some())
                {
                    type_maps_at_link = true;
                } else {
                    crate::verifier::publish_deferred_class_type_maps(&class, &hierarchy);
                }
                crate::define_census::phase_end(
                    crate::define_census::DefinePhase::VerifyDeferred,
                    route_clock,
                );
            } else {
                // `verify_class_with_strictness` (Pass 2, then Pass 3), in two
                // calls so the census can time each (wave 90, lane L5).
                let structure_clock = crate::define_census::phase_start_if(census_charged);
                let structural = crate::verifier::verify_class_structure_with(&class, &self.class_store, format_checked);
                crate::define_census::phase_end(
                    crate::define_census::DefinePhase::VerifyStructure,
                    structure_clock,
                );
                let verified = structural.and_then(|()| {
                    let bytecode_clock = crate::define_census::phase_start_if(census_charged);
                    let bytecode = crate::verifier::verify_class_bytecode_with_strictness(
                        &class,
                        &hierarchy,
                        self.strict_verification,
                    );
                    crate::define_census::phase_end(
                        crate::define_census::DefinePhase::VerifyBytecode,
                        bytecode_clock,
                    );
                    bytecode
                });
                if let Err(verify_err) = verified {
                    // Verifier rejected the bytecode. Drop the guard set
                    // entry so retry attempts are not erroneously blocked.
                    self.loading_guard.remove(name);
                    return Err(VmError::Linkage(verify_err));
                }
                if options.hidden {
                    crate::define_census::phase_end(
                        crate::define_census::DefinePhase::VerifyFullHidden,
                        route_clock,
                    );
                }
                crate::define_census::phase_end(
                    crate::define_census::DefinePhase::VerifyFull,
                    route_clock,
                );
            }
        } else {
            // TYPE MAPS: record the *reason* there are no maps. Without this
            // the side table answers `Unknown` for a `--noverify` /
            // CDS / synthetic-stub / trusted-generated class, which a consumer
            // cannot distinguish from "not verified yet" — and "not yet" is a
            // state it might reasonably wait for. `Skipped` says: none are
            // coming, scan conservatively and stay off the unchecked fast
            // path. The marker never traps this id: `TypeMapStore::publish`
            // upgrades an unverified marker to real maps on a later publish,
            // and the redefine path replaces outright.
            // This VM's store, not the process one: see `TypeMapStore`.
            self.class_store.type_maps().mark_skipped(class.id);
        }
        crate::define_census::phase_end(crate::define_census::DefinePhase::Verify, verify_clock);
        let verifier_loads = verifier_loads.into_inner();
        if !verifier_loads.is_empty() {
            self.verifier_link_loads
                .insert(id, verifier_loads.into_boxed_slice());
        }
        if type_maps_at_link {
            self.type_maps_pending.insert(
                id,
                PendingTypeMaps {
                    record_loads: record_verifier_loads,
                    structural_scan: boot_structural_at_link,
                },
            );
        }
        if user_pass3_at_link {
            self.user_pass3_pending.insert(id);
        }

        debug!(
            class = %class.name,
            id = %id,
            loader = %loader_id,
            superclass = ?superclass_id,
            fields = num_total_fields,
            methods = class.methods.len(),
            has_finalizer = class.has_finalizer,
            "Class loaded",
        );

        // Register. Pre-check at the top of this function already
        // rejected duplicate-defines unless `allow_redefine` is set; if
        // the existing class is here, it's because we are doing an
        // in-place redefinition (WP2.4). In that case we drop the old
        // class_id before inserting the new one.
        //
        // Round 4 audit fix: the redundant `name_to_id` shadow map (keyed
        // by raw FNV-1a digest, loader-unaware, collision-unsafe) has
        // been removed — `loaded_classes` is now the single authoritative
        // index.
        //
        // T10.9.E: `class.name` is already `Arc<str>` — clone the arc
        // (refcount bump, no allocation) instead of `.to_string()`-ing it
        // into a fresh `String`. This is THE hot insert site — every class
        // define hits it once.
        let register_clock = crate::define_census::phase_start_if(census_charged);
        // `CRATONVM_DBG_DEFINE_CENSUS` (interpreter round i4 wave 93, lane L5):
        // `register-store` split into `store-index` (the name index, the
        // constraint pin, the user-loader set), `store-bytes` (the class-bytes
        // cache and the side tables) and `store-add` (`ClassStore::add`, the
        // metaspace charge).
        let store_clock = crate::define_census::phase_start_if(census_charged);
        let key = (loader_id, Arc::clone(&class.name));
        if options.allow_redefine {
            if let Some(old_id) = self.loaded_classes_remove(&key) {
                // The old id's instances outlive this re-key, so the name now
                // has two live ids even though the index only ever shows one.
                // `ANY_DUPLICATE_CLASS_NAME` (the field-retarget gate) is left
                // as it was, but the JIT's `instanceof` final-class miss must
                // stop answering inline: the helper admits the other id by
                // name. See `cratonvm_types::raise_duplicate_class_name_seen`.
                if old_id != id {
                    cratonvm_types::raise_duplicate_class_name_seen();
                }
                debug!(
                    class = %class.name,
                    old_id = %old_id,
                    new_id = %id,
                    "Class redefined (WP2.4 instrument)",
                );
            }
        }
        // zt t16/df6 (define page §1, corner 2): a fresh user-namespace class
        // whose caller makes the `addClass` upcall next is stored but not
        // bound by name until that upcall returned (`unbound_defines`).
        let bind_later = options.bind_name_after_add_class
            && !options.allow_redefine
            && !options.hidden
            && matches!(loader_id, ClassLoaderId::UserDefined(_));
        if bind_later {
            self.unbound_defines.insert(id, key);
        } else {
            self.loaded_classes_insert(key, id);
        }
        if vm_fallback_copy {
            self.vm_fallback_classes.insert(id);
        }
        // JVMS §5.3.4: a constraint that names (this name, this loader) and is
        // not pinned yet is pinned to the class just defined, so the other
        // side's later load is checked against it (the refusal above ran
        // first). Nothing while the table is empty (always, `--compatible`).
        // zt t16/df6: an unbound define pins when its name is bound.
        if !options.allow_redefine && !options.hidden && !bind_later {
            let constraints = self.loader_constraints.get_mut();
            if !constraints.is_empty() {
                let broken = constraints.pin_if_constrained(
                    &class.name,
                    loader_id.to_native_id(),
                    id.as_u32(),
                );
                // Only a built-in loader's VM-fallback or stub-upgrade define
                // reaches here with a broken constraint (or any built-in one
                // while `CRATONVM_DEFINE_BUILTIN_CONSTRAINT_REFUSAL=0`; default
                // ON since wave 134; the others were refused above):
                // counted in the table, admitted.
                if broken.is_some() && loader_flags().dbg_access {
                    eprintln!(
                        "[ACCESS-DBG] LOADER-CONSTRAINT (counted, admitted) built-in define of {} by {loader_id}",
                        class.name
                    );
                }
            }
        }
        // Round 5 audit fix (HIGH): mirror user-defined loader ids into
        // the `user_loaders` set so `find_class_by_name` can probe them
        // without re-walking every entry in `loaded_classes` per call.
        if matches!(loader_id, ClassLoaderId::UserDefined(_)) {
            self.user_loaders.insert(loader_id);
        }
        crate::define_census::phase_end(crate::define_census::DefinePhase::StoreIndex, store_clock);
        let store_clock = crate::define_census::phase_start_if(census_charged);
        // Route through the FIFO-tracking helper so the byte budget
        // (`class_bytes_cache_cap`, default 16 MiB) is enforced. Without
        // this, every classfile would stay resident forever — ~90 MB on
        // a medium Spring app.
        self.insert_class_bytes(id, bytes);
        match class_type_annotations {
            Some(body) => {
                self.class_type_annotations.insert(id, body.into());
            }
            None => {
                self.class_type_annotations.remove(&id);
            }
        }
        if empty_record {
            self.empty_record_classes.insert(id);
        } else {
            self.empty_record_classes.remove(&id);
        }
        // WP2.3: persist the per-class skip-verification flag in the side
        // table. The verifier consults `class_skip_bytecode_verification`
        // during link-time so trusted hidden / generated classes
        // (ByteBuddy, CGLIB, JDK Proxy) bypass Pass-3 type-checking
        // while ordinary classes still get verified.
        if options.skip_verification {
            self.skip_bytecode_verification.insert(id);
        }
        // JIT round 21 wave 14, lane hidden-static: the lock-free hidden-class
        // bitmap the compiled dispatch paths ask first ([`HiddenClassBits`]).
        // Before `class_store.add`, so no reader finds the class without it.
        if class.hidden || options.hidden {
            self.hidden_classes.note(id);
            // The lock-free prefilter word follows the recorded name (wave
            // 22, lane hidden-resid, `CRATONVM_JIT_HIDDEN_SELF_NAME_PREFILTER`).
            self.hidden_classes
                .record_self_name(id, hidden_file_name.as_deref());
        }
        // The recorded class-file name, on the class store (wave 21 for a
        // hidden define; wave 22, lane hidden-resid, for a renamed ordinary
        // one, read by a later `mark_class_hidden`). Ids are never reused,
        // so the row is new; the store drops it with the class.
        if let Some(file_name) = hidden_file_name.or(renamed_file_name) {
            self.class_store.set_recorded_file_name(id, Some(file_name));
        }
        // gci i11/cl (item 7): the NESTMATE hidden class's lookup class, for
        // the `-Xlog:class+load` source. Before `class_store.add`, under this
        // write guard, so the class-load hook (fired after the guard drops)
        // always finds it.
        match options.lookup_class_name.as_deref() {
            Some(lookup) if options.hidden && options.nest_host_class_name.is_some() => {
                self.hidden_lookup_class_names.insert(id, Arc::from(lookup));
            }
            _ => {
                self.hidden_lookup_class_names.remove(&id);
            }
        }
        crate::define_census::phase_end(crate::define_census::DefinePhase::StoreBytes, store_clock);
        let store_clock = crate::define_census::phase_start_if(census_charged);
        // T10.9.E: clone the `Arc<str>` (refcount bump) instead of
        // allocating a fresh `String` for the JVMTI hook.
        let class_name_for_hook = Arc::clone(&class.name);
        let class_id_for_hook = id.as_u32();
        self.class_store.add(class);
        if bind_later {
            // Out of the store's name index too until the name is bound, so
            // `class_defined_by_loader_exact`'s store fallback cannot answer
            // it either.
            // zt t30/dc: the `bind-withdraw-name` census row.
            let withdraw_clock = crate::define_census::phase_start_if(census_charged);
            self.class_store.withdraw_name(id);
            crate::define_census::phase_end(
                crate::define_census::DefinePhase::BindWithdrawName,
                withdraw_clock,
            );
        }
        self.charge_metaspace(id, metaspace_charge);
        crate::define_census::phase_end(crate::define_census::DefinePhase::StoreAdd, store_clock);
        crate::define_census::phase_end(
            crate::define_census::DefinePhase::RegisterStore,
            register_clock,
        );
        let vtable_clock = crate::define_census::phase_start_if(census_charged);
        // L4 gap 2/1 — the shadow-layout census. This class was defined from
        // real bytes; if CratonVM also carries a hand-numbered slot model for
        // its name, the two layouts are now both known and can be diffed once,
        // here, instead of guessed at per access.
        // `CRATONVM_DBG_DEFINE_CENSUS`: the `vtable-claims` row (wave 95, L5).
        let claims_clock = crate::define_census::phase_start_if(census_charged);
        self.report_shadow_layout(id);
        self.check_safe_positional_claims(id);
        crate::define_census::phase_end(crate::define_census::DefinePhase::VtableClaims, claims_clock);

        // T10.5 — Build this class's vtable descriptor layout, cache it on
        // `self.vtable_descriptors`, and fire the install hook so the VM
        // side can populate its `VtableManager`.
        //
        // The build is a standard single-inheritance vtable walk:
        //   1. Start from a clone of the superclass's vtable descriptor
        //      vec (empty for `java/lang/Object`).
        //   2. For each virtual method declared in THIS class, override
        //      any matching inherited slot in place; otherwise append a
        //      new slot.
        //
        // Virtual methods for vtable purposes are: non-static,
        // non-private, and not `<init>`/`<clinit>`. `private` methods are
        // statically dispatched (invokespecial), static methods live on
        // the Class object, and constructors are never virtual.
        let build_clock = crate::define_census::phase_start_if(census_charged);
        let (entries, overrides) = self.build_and_record_vtable_descriptors(id, superclass_id);
        self.vtable_descriptors.insert(id, entries.clone());
        crate::define_census::phase_end(crate::define_census::DefinePhase::VtableBuild, build_clock);
        let install_clock = crate::define_census::phase_start_if(census_charged);
        // C2 review P1: route through the VM-scoped dispatcher so the VM
        // receives a `ClassHandle` (VM identity + generation) instead of a
        // bare `u32` that aliases across VMs (the legacy `u32` hook was
        // deleted in interpreter round i1 wave 21).
        // The VM's hook times its own `install-*` sub-rows for a charged
        // define (wave 95, L5).
        let census_scope = crate::define_census::enter_charged_scope(census_charged);
        fire_vtable_install_hook_for(&self.metadata_realm, id, entries);
        crate::define_census::restore_charged_scope(census_scope);
        // T10.9.A — fire CHA override signals for each super-class slot
        // that this class replaced. No VM listener acts on them since
        // interpreter round i1 wave 22 (`vm/src/runtime/vtable.rs`,
        // `OVERRIDE_SIGNAL_CLEARS_SUPER_ENTRY`): the super's own vtable entry
        // stays the right answer for a receiver of the super's class, and
        // no cached dispatch entry (invoke cache, promoted resolution) was
        // ever reached by the signal.
        for (super_id, slot) in overrides {
            fire_vtable_override_hook_for(&self.metadata_realm, ClassId::new(super_id), slot);
        }
        crate::define_census::phase_end(
            crate::define_census::DefinePhase::VtableInstallHook,
            install_clock,
        );

        // T6.3.1 — Queue the JVMTI ClassLoad event (obsaudit D1, 2026-07-26:
        // it no longer fires synchronously here). Actual delivery happens
        // once the caller's L10 `class_manager` write guard is released — see
        // the DEFERRED FIRING notes on the hook registry near
        // `install_class_load_hook`, and `ClassManagerWriteGuard` in
        // `vm/src/vm/realms/class_realm.rs`. A hook may now safely call back
        // into `self` / `shared.classes` from its own thread.
        //
        // ClassPrepare is not queued here: a class is prepared when the VM
        // links it (`vm_util::link_claimed_class` posts it there, once per
        // class, on the linking thread). Until interpreter round i1 wave 17
        // it was queued here too, for every class defined, linked or not.
        //
        // zt t17/x13 (review of t16/df6): an unbound define posts its
        // ClassLoad when its name is bound (`bind_unbound_define`), and a
        // withdrawn one never does: HotSpot's `define_instance_class` posts
        // ClassLoad after the `addClass` upcall and `update_dictionary`, and a
        // define whose upcall threw posts none. Posted here, an agent's
        // `FindClass` / `GetLoadedClasses` from the event would miss a class
        // the event had just named.
        if !bind_later {
            let owner_domain = self.class_store.layout_domain();
            let hook_thread_id = current_thread_id(owner_domain);
            fire_class_load_hook(
                class_id_for_hook,
                &class_name_for_hook,
                hook_thread_id,
                owner_domain,
            );
        }
        crate::define_census::phase_end(crate::define_census::DefinePhase::RegisterVtable, vtable_clock);
        crate::define_census::phase_end(crate::define_census::DefinePhase::Register, register_clock);

        Ok(id)
    }

    /// T10.5 — build the vtable descriptor vec for `class_id`.
    ///
    /// Standard single-inheritance walk:
    /// - seed with a clone of the superclass's descriptor vec (empty for
    ///   `java/lang/Object`);
    /// - for each virtual method declared in this class, override the
    ///   matching inherited slot or append a new one.
    ///
    /// A method is considered "virtual" for vtable purposes when it is
    /// neither static, private, nor a constructor (`<init>`/`<clinit>`).
    /// Private methods use invokespecial (static dispatch), static
    /// methods live on the class itself, and constructors are never
    /// inherited.
    ///
    /// Returns an owned vec; caller is responsible for storing/forwarding.
    ///
    /// T10.9.A — also returns the list of `(super_class_id, slot)` pairs that
    /// were overridden by this class. The VM-side CHA listener uses this to
    /// invalidate super's cached dispatch entries.
    pub(in crate::class_manager) fn build_vtable_descriptors(
        &self,
        class_id: ClassId,
        superclass_id: Option<ClassId>,
    ) -> Vec<Option<VtableSlotDescriptor>> {
        self.build_vtable_descriptors_with_overrides(class_id, superclass_id)
            .0
    }

    /// [`Self::build_vtable_descriptors_with_overrides`] for a class being
    /// linked, plus the [`Self::shadowed_signature_vtables`] bookkeeping: under
    /// the loader-aware §5.4.5 policy a class joins the set when it inherits
    /// from a member or when one of its methods took a fresh slot for a
    /// signature the vtable already holds. Under the name-only policy
    /// (`CRATONVM_OVERRIDE_PACKAGE_BY_LOADER=0`, either mode) the set is never
    /// written.
    pub(in crate::class_manager) fn build_and_record_vtable_descriptors(
        &mut self,
        class_id: ClassId,
        superclass_id: Option<ClassId>,
    ) -> (Vec<Option<VtableSlotDescriptor>>, Vec<(u32, usize)>) {
        let mut shadows = false;
        let built = self.build_vtable_descriptors_shadowing(class_id, superclass_id, &mut shadows);
        if self.class_store.override_packages_by_loader()
            && (shadows
                || superclass_id.is_some_and(|s| self.shadowed_signature_vtables.contains(&s)))
        {
            self.shadowed_signature_vtables.insert(class_id);
        }
        built
    }

    /// Whether `class_id`'s vtable holds some signature in more than one slot
    /// (see [`Self::shadowed_signature_vtables`]): a lookup of the vtable by
    /// name and descriptor then answers the LOWEST such slot, which is not
    /// the slot of every call site naming that signature. Always `false`
    /// under the name-only policy (`CRATONVM_OVERRIDE_PACKAGE_BY_LOADER=0`).
    #[inline]
    pub fn vtable_shadows_a_signature(&self, class_id: ClassId) -> bool {
        !self.shadowed_signature_vtables.is_empty()
            && self.shadowed_signature_vtables.contains(&class_id)
    }

    /// Whether `class_id`'s vtable holds `(name, descriptor)` in more than one
    /// slot: the only signature a name-keyed vtable lookup can answer with
    /// the wrong slot. [`Self::vtable_shadows_a_signature`] answers for the
    /// whole class, so every virtual call on such a receiver paid the §5.4.6
    /// validation in the interpreter's vtable fast path; this narrows it to
    /// the duplicated signature (`r12w6-override-runtime-package-residuals`,
    /// item 3). A class with no recorded descriptors answers `true`.
    pub fn vtable_shadows_signature(
        &self,
        class_id: ClassId,
        name: &str,
        descriptor: &str,
    ) -> bool {
        if !self.vtable_shadows_a_signature(class_id) {
            return false;
        }
        if !self.shadow_validation_per_signature {
            return true;
        }
        self.vtable_descriptors.get(&class_id).is_none_or(|slots| {
            slots
                .iter()
                .flatten()
                .filter(|d| &*d.method_name == name && &*d.descriptor == descriptor)
                .nth(1)
                .is_some()
        })
    }

    /// Whether `class_id` is a hidden class (JEP 371) whose run-time package
    /// the interpreter's vtable fast path may read from its stored name with
    /// the `/0x<hex>` suffix cut, as `method_override::class_package` does
    /// (proposal P6-5). The stored name `p/Foo/0x1f` otherwise reads as package
    /// `p/Foo`, so every `invokevirtual p/Base.m` answered by a hidden class's
    /// own `m` paid the §5.4.6 validation although both are in `p`. `false`
    /// under `CRATONVM_VTABLE_HIDDEN_RECEIVER_PACKAGE=0`.
    #[inline]
    pub fn vtable_fast_path_hidden_receiver(&self, class_id: ClassId) -> bool {
        self.vtable_hidden_receiver_package
            && self
                .class_store
                .get(class_id)
                .is_some_and(|c| c.is_hidden())
    }

    /// Re-point every already-built descendant's cached vtable descriptors at
    /// `class_id`'s freshly-rebuilt slots.
    ///
    /// Called from [`Self::redefine_class`] step 6. `build_vtable_descriptors_
    /// with_overrides` seeds a subclass's vec by `extend_from_slice`-ing its
    /// superclass's, so each descendant owns a *copy* of the ancestor's slot
    /// (bytecode snapshot included). Redefinition keeps the layout identical
    /// but replaces the bodies, so those copies become stale the moment a class
    /// anywhere above them is redefined.
    ///
    /// The VM's `VtableManager::install_vtable` performs the equivalent sweep
    /// over its own installed tables, which is why in-flight dispatch normally
    /// recovers. It is not enough on its own: `retransformClasses` receives its
    /// argument array from a `HashSet` (Mockito's `InlineBytecodeGenerator`
    /// .triggerRetransformation), so the per-class redefines arrive in an order
    /// that varies run to run. When a superclass is redefined FIRST, the
    /// descendant's own redefine then re-runs `build_vtable_descriptors` off
    /// this stale copy and re-installs the ORIGINAL body over the VM-side
    /// table the sweep had just fixed — an agent's woven advice silently stops
    /// running for every call site that reaches the method through the vtable
    /// fast path. Keeping the loader-side copies in lockstep here makes the
    /// outcome order-independent, and also fixes a subclass linked *after* the
    /// redefine (which would otherwise inherit the pre-redefinition snapshot).
    ///
    /// O(total cached slots) on the cold JVMTI path only, matching the cost
    /// profile `install_vtable`'s sibling sweep already accepts.
    pub(in crate::class_manager) fn refresh_inherited_vtable_descriptors(
        &mut self,
        class_id: ClassId,
        new_entries: &[Option<VtableSlotDescriptor>],
    ) {
        let class_id_u32 = class_id.as_u32();
        let mut replacements: FxHashMap<(Arc<str>, Arc<str>), &VtableSlotDescriptor> =
            FxHashMap::with_capacity_and_hasher(new_entries.len(), Default::default());
        for descriptor in new_entries.iter().flatten() {
            if descriptor.declaring_class_id == class_id_u32 {
                replacements.insert(
                    (
                        Arc::clone(&descriptor.method_name),
                        Arc::clone(&descriptor.descriptor),
                    ),
                    descriptor,
                );
            }
        }
        if replacements.is_empty() {
            return;
        }
        let share = self.vtable_share_slots;
        for (owner, entries) in self.vtable_descriptors.iter_mut() {
            if *owner == class_id {
                continue;
            }
            for slot in entries.iter_mut() {
                let Some(existing) = slot.as_ref() else {
                    continue;
                };
                if existing.declaring_class_id != class_id_u32 {
                    continue;
                }
                let key = (
                    Arc::clone(&existing.method_name),
                    Arc::clone(&existing.descriptor),
                );
                match replacements.get(&key) {
                    Some(fresh) => {
                        *slot = if share {
                            Some((*fresh).clone())
                        } else {
                            Some(fresh.deep_copy())
                        }
                    }
                    // JEP 109 forbids adding/removing methods, so a slot
                    // attributed to this class must exist in its rebuilt vec.
                    // Fail closed rather than keep a snapshot we know is stale
                    // — `None` costs a slow-path resolution, a stale body is a
                    // wrong answer. Mirrors `VtableManager::install_vtable`.
                    None => *slot = None,
                }
            }
        }
    }

    /// The descriptor a vtable slot was introduced with: `slot`'s entry in
    /// the topmost ancestor (from `superclass_id` up) whose vtable still has
    /// that slot. A class's vtable extends its superclass's, so the class that
    /// first appended `slot` is the highest one long enough to hold it, and
    /// its entry there is its own declaration — the method a call through the
    /// slot resolved to. `None` when `superclass_id` has no built vtable
    /// holding `slot`.
    pub(in crate::class_manager) fn vtable_slot_root(
        &self,
        superclass_id: Option<ClassId>,
        slot: usize,
    ) -> Option<&VtableSlotDescriptor> {
        let mut root = None;
        let mut cur = superclass_id;
        let mut steps = 0usize;
        while let Some(id) = cur {
            steps += 1;
            if steps > crate::method_override::OVERRIDE_WALK_BUDGET {
                break;
            }
            match self.vtable_descriptors.get(&id).and_then(|v| v.get(slot)) {
                Some(Some(descriptor)) => root = Some(descriptor),
                _ => break,
            }
            cur = self.class_store.get(id).and_then(|c| c.superclass);
        }
        root
    }

    /// T10.9.A — same as `build_vtable_descriptors` but also returns the
    /// list of super-class slots this class overrode. Each pair is
    /// `(super_class_id_u32, slot_index)`. Used by the class-link path
    /// to fire CHA-invalidation signals on the super's cached vtable
    /// entries (so in-flight `invoke_cache` / JIT dispatch sees the
    /// override immediately).
    pub(in crate::class_manager) fn build_vtable_descriptors_with_overrides(
        &self,
        class_id: ClassId,
        superclass_id: Option<ClassId>,
    ) -> (Vec<Option<VtableSlotDescriptor>>, Vec<(u32, usize)>) {
        self.build_vtable_descriptors_shadowing(class_id, superclass_id, &mut false)
    }

    /// A copy of `slot`, as a vector holding it by inheritance keeps it:
    /// the shared block under `CRATONVM_VTABLE_SHARE_SLOTS` (the default),
    /// a private copy with it `=0`.
    #[inline]
    pub(in crate::class_manager) fn inherited_vtable_slot(
        &self,
        slot: &Option<VtableSlotDescriptor>,
    ) -> Option<VtableSlotDescriptor> {
        match slot {
            Some(d) if !self.vtable_share_slots => Some(d.deep_copy()),
            other => other.clone(),
        }
    }

    /// `parent`'s slots as a new class's inherited prefix, with room for
    /// `extra` more ([`Self::inherited_vtable_slot`] for each).
    pub(in crate::class_manager) fn inherit_vtable_slots(
        &self,
        parent: &[Option<VtableSlotDescriptor>],
        extra: usize,
    ) -> Vec<Option<VtableSlotDescriptor>> {
        let mut entries = Vec::with_capacity(parent.len() + extra);
        if self.vtable_share_slots {
            entries.extend_from_slice(parent);
        } else {
            entries.extend(parent.iter().map(|s| self.inherited_vtable_slot(s)));
        }
        entries
    }

    /// The body of [`Self::build_vtable_descriptors_with_overrides`]; also sets
    /// `*shadows` when one of this class's methods overrides no inherited slot
    /// of its signature and takes a fresh one, so that the vtable holds the
    /// signature twice.
    pub(in crate::class_manager) fn build_vtable_descriptors_shadowing(
        &self,
        class_id: ClassId,
        superclass_id: Option<ClassId>,
        shadows: &mut bool,
    ) -> (Vec<Option<VtableSlotDescriptor>>, Vec<(u32, usize)>) {
        // Seed from the superclass's already-built descriptor vec.
        // A zero-sized vec is the natural identity for `java/lang/Object`
        // and for classes whose superclass hasn't been processed yet
        // (which only happens in pathological re-entry paths; the normal
        // load order guarantees the super is built first).
        //
        // Round 5 audit fix (MED): borrow the parent slice first and
        // pre-count how many of THIS class's methods are virtual. If
        // the class has zero virtual methods (very common for
        // marker interfaces, all-static utility classes, and synthetic
        // shells) we clone the parent vec once with the exact capacity
        // it needs and skip the entire override-detection inner loop.
        // Pre-counting also lets us `Vec::with_capacity(parent + own)`
        // so the subsequent `entries.push` calls never reallocate
        // (the previous `cloned().unwrap_or_default()` allocated at
        // parent length and then re-grew on every fresh slot — three
        // reallocs on a class adding 5 methods to a 20-slot parent).
        let parent_slice: &[Option<VtableSlotDescriptor>] = match superclass_id {
            Some(sid) => self
                .vtable_descriptors
                .get(&sid)
                .map(|v| v.as_slice())
                .unwrap_or(&[]),
            None => &[],
        };

        let class = match self.class_store.get(class_id) {
            Some(c) => c,
            None => return (self.inherit_vtable_slots(parent_slice, 0), Vec::new()),
        };

        // Pre-count own virtual methods so we can size the entries vec
        // exactly. This walk is cheap (one pass over `class.methods`
        // with three flag checks per method) and pays for itself by
        // eliminating the Vec re-grows below.
        let own_virtual_count = class
            .methods
            .iter()
            .filter(|m| {
                !m.is_static()
                    && !m
                        .access_flags
                        .contains(cratonvm_reader::class_access_flags::MethodAccessFlags::PRIVATE)
                    && &*m.name != "<init>"
                    && &*m.name != "<clinit>"
            })
            .count();

        // Fast path: this class declares no virtual methods, so it
        // cannot override or extend the parent's vtable. Hand back a
        // tight clone of the parent slice with no override list.
        // Saves the `name_to_slot` map build + per-method match work.
        if own_virtual_count == 0 {
            return (self.inherit_vtable_slots(parent_slice, 0), Vec::new());
        }

        // Mutating path: allocate the destination vec with the exact
        // capacity it needs (`parent_len + own_virtual_count`) so
        // subsequent `push`es never realloc.
        let mut entries: Vec<Option<VtableSlotDescriptor>> =
            self.inherit_vtable_slots(parent_slice, own_virtual_count);

        // Maintain a (name, desc) -> slot index so we can detect overrides
        // without a linear scan for every method.
        //
        // T10.9.E: key is now `(Arc<str>, Arc<str>)` (was `(String, String)`)
        // — every key was previously `.clone()`-ed off the inherited
        // descriptor's String fields; now it's a refcount bump on the
        // Arc<str> from the source method. Also switched the inner map
        // from SipHash `HashMap` to `FxHashMap` because the key is fully
        // trusted (method-name strings interned by the class reader).
        //
        // One signature can own several inherited slots: a method that does
        // not override (§5.4.5) gets a fresh slot of its own. The map keeps
        // the FIRST slot; the later ones go to `repeated_slots`, which is
        // almost always empty, and a new method is checked against all of
        // them.
        let mut name_to_slot: FxHashMap<(Arc<str>, Arc<str>), usize> =
            FxHashMap::with_capacity_and_hasher(entries.len(), Default::default());
        let mut repeated_slots: Vec<usize> = Vec::new();
        for (slot, entry) in entries.iter().enumerate() {
            if let Some(e) = entry {
                let key = (Arc::clone(&e.method_name), Arc::clone(&e.descriptor));
                if name_to_slot.contains_key(&key) {
                    repeated_slots.push(slot);
                } else {
                    name_to_slot.insert(key, slot);
                }
            }
        }
        let class_id_u32 = class_id.as_u32();
        let super_u32 = superclass_id.map(|s| s.as_u32());

        // T10.9.A — record each super-class slot index that this class
        // overrides so the VM-side hook can fire the CHA invalidation.
        let mut overrides: Vec<(u32, usize)> = Vec::new();

        // Round 7 audit fix (HIGH #5): cache `code_attr.code.to_arc()`
        // by (class_id_u32, method_index). `to_arc()` materialises a
        // fresh `Arc<[u8]>` (alloc + memcpy) per call; without this
        // cache, every time the same method is touched by the vtable
        // build path it pays the alloc again. While the current loop
        // visits each `class.methods[i]` exactly once, the cache (a)
        // makes the intent explicit, (b) protects against future
        // re-entry from override / interface scaffolding being added
        // here, and (c) is essentially free on miss (one FxHashMap
        // insert). On Spring inheritance chains this saves the bulk of
        // the ~12k redundant Arc allocations attributed to vtable
        // installation.
        let mut method_code_cache: FxHashMap<(u32, u32), cratonvm_reader::ByteView> =
            FxHashMap::with_capacity_and_hasher(class.methods.len(), Default::default());
        // One shared source-file name for every snapshot of this class. zs
        // s2/fp2: interned, so it is the constant pool's own `SourceFile`
        // Utf8 string (the reader interns every Utf8) instead of a fresh
        // allocation per class.
        let source_file_arc: Option<Arc<str>> =
            class.source_file.as_deref().map(cratonvm_types::intern_arc);

        for (method_index, method) in class.methods.iter().enumerate() {
            // Skip non-virtual methods.
            if method.is_static() {
                continue;
            }
            if method
                .access_flags
                .contains(cratonvm_reader::class_access_flags::MethodAccessFlags::PRIVATE)
            {
                continue;
            }
            if &*method.name == "<init>" || &*method.name == "<clinit>" {
                continue;
            }

            // T10.9.E: `method.name` and `method.descriptor` are already
            // `Arc<str>` (see `reader/src/method.rs`). Clone the arcs
            // (refcount bumps) rather than allocating fresh Strings —
            // saves ~90k throwaway allocs per Spring Boot cold start.
            let key = (Arc::clone(&method.name), Arc::clone(&method.descriptor));

            // T10.9.A — snapshot the method's Code attribute + flags at
            // link time. `None` when the method has no Code (abstract)
            // or when it's native (no bytecode). Native methods still
            // populate the slot so name-based lookup succeeds; the
            // interpreter routes them through the native registry.
            let is_abstract = method
                .access_flags
                .contains(cratonvm_reader::class_access_flags::MethodAccessFlags::ABSTRACT);
            let is_native = method.is_native();
            let num_params_u16 = cratonvm_reader::count_param_slots(&method.descriptor) as u16;
            let dispatch: Option<Arc<VtableMethodSnapshot>> = if is_abstract {
                None
            } else if let Some(code_attr) = method.code() {
                // Round 7 audit fix (HIGH #5): dedupe `to_arc()` via
                // `method_code_cache` so the same method id never
                // allocates more than one `Arc<[u8]>` in this build.
                // First touch allocates + memcpys (one-time cost per
                // method); every subsequent touch is a refcount bump on
                // the cached Arc.
                let cache_key = (class_id_u32, method_index as u32);
                let code_arc = method_code_cache
                    .entry(cache_key)
                    .or_insert_with(|| code_attr.code.clone())
                    .clone();
                Some(Arc::new(VtableMethodSnapshot {
                    class_name: Arc::clone(&class.name),
                    source_file: source_file_arc.clone(),
                    // `code_attr.code: ByteView` — a refcount clone of the
                    // view, no copy (zr z11/md). The VM adapter pads its
                    // own free-standing copy for `Frame.code` / the JIT.
                    code: code_arc,
                    exception_table: code_attr.exception_table.clone(),
                    max_stack: code_attr.max_stack,
                    max_locals: code_attr.max_locals,
                    num_params: num_params_u16,
                    is_synchronized: method.is_synchronized(),
                    is_static: false,
                    is_native: false,
                }))
            } else if is_native {
                // Native methods don't carry bytecode; the snapshot is a
                // shell that identifies the method so the VM registry
                // lookup can find the Rust callback.
                Some(Arc::new(VtableMethodSnapshot {
                    class_name: Arc::clone(&class.name),
                    source_file: source_file_arc.clone(),
                    code: cratonvm_reader::ByteView::empty(),
                    exception_table: Vec::new(),
                    max_stack: 0,
                    max_locals: 0,
                    num_params: num_params_u16,
                    is_synchronized: method.is_synchronized(),
                    is_static: false,
                    is_native: true,
                }))
            } else {
                // Non-abstract, non-native method without a Code
                // attribute — this is a spec violation but we stay
                // defensive and leave `dispatch = None` so the slow
                // path kicks in.
                None
            };

            let new_entry = VtableSlotDescriptor::new(VtableSlotData {
                declaring_class_id: class_id_u32,
                method_index: method_index as u32,
                // T10.9.E: clone the `Arc<str>` from the source method
                // (refcount bump) instead of allocating a fresh `String`.
                method_name: Arc::clone(&method.name),
                descriptor: Arc::clone(&method.descriptor),
                dispatch,
            });

            if let Some(&first_slot) = name_to_slot.get(&key) {
                // JVMS §5.4.5, through the SAME predicate the VM's §5.4.6
                // selection uses (`method_override::can_override`), so the
                // vtable and the interpreter's selection cannot disagree. A
                // slot is overridden when this method can override the method
                // the slot was INTRODUCED for (its root — the resolved method
                // of a call through that slot), not merely its current
                // occupant: a public `A.m` occupied by a package-private `B.m`
                // from another package is still overridden by a public `C.m`.
                // Package-private cases are the only ones this decides
                // differently from name+descriptor matching; a
                // public/protected root is overridden by any non-private
                // instance method, as before.
                let mut candidate_buf: Vec<usize> = Vec::new();
                let candidates: &[usize] = if repeated_slots.is_empty() {
                    std::slice::from_ref(&first_slot)
                } else {
                    candidate_buf.push(first_slot);
                    candidate_buf.extend(repeated_slots.iter().copied().filter(|&s| {
                        entries[s]
                            .as_ref()
                            .is_some_and(|e| *e.method_name == *key.0 && *e.descriptor == *key.1)
                    }));
                    &candidate_buf
                };
                // The entry is moved into the last slot it overrides; only a
                // method overriding several slots (rare) pays a clone.
                let pending = Some(new_entry);
                let mut last_hit: Option<usize> = None;
                for &slot in candidates {
                    let root = self.vtable_slot_root(superclass_id, slot).and_then(|root| {
                        let a_id = ClassId::new(root.declaring_class_id);
                        self.class_store
                            .get(a_id)
                            .and_then(|a| a.methods.get(root.method_index as usize))
                            .map(|ma| (ma, a_id))
                    });
                    let is_true_override = match root {
                        Some((ma, a_id)) => {
                            let mut budget = crate::method_override::OVERRIDE_WALK_BUDGET;
                            crate::method_override::can_override(
                                &self.class_store,
                                method,
                                class_id,
                                ma,
                                a_id,
                                &mut budget,
                            )
                        }
                        // Root unknown: do not invent a non-override.
                        None => true,
                    };
                    if is_true_override {
                        // Override inherited slot in place — same slot index
                        // so that subclass dispatch remains index-stable
                        // across further inheritance. Record the super's slot
                        // for CHA invalidation.
                        if let Some(s) = super_u32 {
                            overrides.push((s, slot));
                        }
                        if let Some(prev) = last_hit.replace(slot) {
                            entries[prev] = pending.clone();
                        }
                    }
                }
                match last_hit {
                    Some(slot) => entries[slot] = pending,
                    // Not an override of anything inherited (cross-package
                    // package-private shadowing) — append a fresh,
                    // independent slot, same as an unrelated new signature.
                    // `name_to_slot` keeps the first slot; lookups by name
                    // find the inherited one first, as before.
                    None => {
                        *shadows = true;
                        entries.push(pending);
                    }
                }
            } else {
                // New method signature: append a fresh slot.
                let slot = entries.len();
                entries.push(Some(new_entry));
                name_to_slot.insert(key, slot);
            }
        }

        (entries, overrides)
    }

    /// T10.5 — read-only view of a class's vtable descriptor layout.
    ///
    /// Used by tests and by any caller that needs to inspect the
    /// descriptor vec without going through the VM's installed hook.
    /// Returns `None` if the class hasn't been processed by
    /// `define_class_with_options` yet.
    pub fn vtable_descriptors_of(
        &self,
        class_id: ClassId,
    ) -> Option<&[Option<VtableSlotDescriptor>]> {
        self.vtable_descriptors.get(&class_id).map(|v| v.as_slice())
    }

    /// zs s1/fp: the bytes this manager's per-class (and per-name) side
    /// tables hold, by capacity, one `(name, bytes)` row per table, followed
    /// by the class store's own index rows
    /// ([`ClassStore::footprint_census`]). It names the "unnamed" share of
    /// `unnamed-per-class-side-tables-CLOSED-20261023.md`: the
    /// power-of-two hash-table blocks no container walk reaches.
    ///
    /// Each row is the table's buckets (`hash_table_bytes`) plus the heap
    /// blocks its entries own where those are per-entry allocations the deep
    /// census does not already count: a nested map's own table, an
    /// `Arc<Atomic*>` block (16 bytes of counts plus the payload, rounded to
    /// 8), a boxed slice, an owned `String`'s bytes. The class-file bytes a
    /// cache entry points at (`class_bytes_cache_b`) and the vtable vectors
    /// (`vtable_descriptors`) are counted by the census itself and left out.
    /// Diagnostics only.
    pub fn footprint_census(&self) -> Vec<(&'static str, usize)> {
        use crate::footprint::hash_table_bytes as t;
        use std::mem::size_of;
        let arc_block = |payload: usize| 16 + payload.div_ceil(8) * 8;
        let strings = |it: &mut dyn Iterator<Item = usize>| it.sum::<usize>();
        let mut rows = vec![
            (
                "cm_loaded_classes",
                t(
                    self.loaded_classes.capacity(),
                    size_of::<((ClassLoaderId, Arc<str>), ClassId)>(),
                ),
            ),
            (
                "cm_class_bytes_cache_table",
                t(
                    self.class_bytes_cache.capacity(),
                    size_of::<(ClassId, SharedBytes)>(),
                ),
            ),
            (
                "cm_class_bytes_cache_fifo",
                self.class_bytes_cache_fifo.capacity() * size_of::<ClassId>(),
            ),
            (
                "cm_class_type_annotations",
                t(
                    self.class_type_annotations.capacity(),
                    size_of::<(ClassId, SharedBytes)>(),
                ),
            ),
            (
                "cm_empty_record_classes",
                t(self.empty_record_classes.capacity(), size_of::<ClassId>()),
            ),
            (
                "cm_class_bytes_base_digest",
                t(
                    self.class_bytes_base_digest.capacity(),
                    size_of::<(ClassId, (u32, u64))>(),
                ),
            ),
            (
                "cm_class_bytes_pinned",
                t(self.class_bytes_pinned.capacity(), size_of::<ClassId>()),
            ),
            (
                "cm_class_bytes_raw_agent_bases",
                t(self.class_bytes_raw_agent_bases.capacity(), size_of::<ClassId>()),
            ),
            (
                "cm_metaspace_charges",
                t(
                    self.metaspace_charges.capacity(),
                    size_of::<(ClassId, u64)>(),
                ),
            ),
            (
                "cm_vtable_descriptors_table",
                t(
                    self.vtable_descriptors.capacity(),
                    size_of::<(ClassId, Vec<Option<VtableSlotDescriptor>>)>(),
                ),
            ),
            (
                "cm_shadowed_signature_vtables",
                t(
                    self.shadowed_signature_vtables.capacity(),
                    size_of::<ClassId>(),
                ),
            ),
            (
                "cm_skip_bytecode_verification",
                t(
                    self.skip_bytecode_verification.capacity(),
                    size_of::<ClassId>(),
                ),
            ),
            (
                "cm_verifier_link_loads",
                t(
                    self.verifier_link_loads.capacity(),
                    size_of::<(ClassId, Box<[crate::vtype::VerifierLinkLoad]>)>(),
                ) + self
                    .verifier_link_loads
                    .values()
                    .map(|b| size_of_val(&**b))
                    .sum::<usize>(),
            ),
            (
                "cm_type_maps_pending",
                t(
                    self.type_maps_pending.capacity(),
                    size_of::<(ClassId, PendingTypeMaps)>(),
                ),
            ),
            (
                "cm_redefine_generations",
                self.redefine_generations.read().map_or(0, |g| {
                    t(g.capacity(), size_of::<(ClassId, Arc<AtomicU32>)>())
                        + g.len() * arc_block(size_of::<AtomicU32>())
                }),
            ),
            (
                "cm_redefinition_histories",
                t(
                    self.redefinition_histories.capacity(),
                    size_of::<(ClassId, crate::obsolete_code::RedefinitionHistory)>(),
                ),
            ),
            (
                "cm_name_definitions",
                t(
                    self.name_definitions.capacity(),
                    size_of::<(Arc<str>, FxHashMap<ClassId, u32>)>(),
                ) + self
                    .name_definitions
                    .values()
                    .map(|m| t(m.capacity(), size_of::<(ClassId, u32)>()))
                    .sum::<usize>(),
            ),
            (
                "cm_virtual_components",
                t(
                    self.virtual_components.capacity(),
                    size_of::<(String, ClassId)>(),
                ) + strings(&mut self.virtual_components.keys().map(|k| k.capacity())),
            ),
            ("cm_init_states", {
                let s = self.init_states.read();
                t(
                    s.capacity(),
                    size_of::<(ClassId, Arc<std::sync::atomic::AtomicU8>)>(),
                ) + s.len() * arc_block(1)
            }),
            (
                "cm_loading_guard",
                t(self.loading_guard.capacity(), size_of::<String>())
                    + strings(&mut self.loading_guard.iter().map(|k| k.capacity())),
            ),
            (
                "cm_synthetic_upgrade_absent",
                t(
                    self.synthetic_upgrade_absent.capacity(),
                    size_of::<String>(),
                ) + strings(&mut self.synthetic_upgrade_absent.iter().map(|k| k.capacity())),
            ),
            (
                "cm_origin_violations_seen",
                t(self.origin_violations_seen.capacity(), size_of::<String>())
                    + strings(&mut self.origin_violations_seen.iter().map(|k| k.capacity())),
            ),
            (
                "cm_origin_requesters",
                t(
                    self.origin_requesters.capacity(),
                    size_of::<(String, String)>(),
                ) + strings(
                    &mut self
                        .origin_requesters
                        .iter()
                        .map(|(k, v)| k.capacity() + v.capacity()),
                ),
            ),
            (
                "cm_pending_native_shadow_cedes",
                t(
                    self.pending_native_shadow_cedes.capacity(),
                    size_of::<String>(),
                ) + strings(
                    &mut self
                        .pending_native_shadow_cedes
                        .iter()
                        .map(|k| k.capacity()),
                ),
            ),
            (
                "cm_cds_class_cache",
                t(
                    self.cds_class_cache.capacity(),
                    size_of::<(String, Vec<u8>)>(),
                ) + strings(
                    &mut self
                        .cds_class_cache
                        .iter()
                        .map(|(k, v)| k.capacity() + v.capacity()),
                ),
            ),
        ];
        rows.extend(self.class_store.footprint_census());
        rows
    }

    /// Get a reference to a loaded class by its id.
    pub fn get_class(&self, id: ClassId) -> Option<&Class> {
        self.class_store.get(id)
    }

    /// Get a mutable reference to a loaded class by its id.
    pub fn get_class_mut(&mut self, id: ClassId) -> Option<&mut Class> {
        self.class_store.get_mut(id)
    }

    /// Diff CratonVM's fabricated slot model for this class against the layout
    /// the loaded image actually declares.
    ///
    /// `None` when there is nothing to say: the class has no model in
    /// [`synthetic_stub_field_model`], or it *is* a fabricated stub (diffing a
    /// model against itself answers nothing).
    ///
    /// This is the overlay detector's answer to the two gaps a per-access type
    /// check cannot reach — a read of a wrong-field slot, and a same-kind write
    /// into one. Both are invisible in the value's type tag and both are visible
    /// here. See [`crate::shadow_layout`].
    #[must_use]
    pub fn shadow_layout_diff(
        &self,
        id: ClassId,
    ) -> Option<crate::shadow_layout::ShadowLayoutDiff> {
        let name = self.class_store.get(id).map(|c| Arc::clone(&c.name))?;
        let model = self.synthetic_stub_fields_for(&name);
        crate::shadow_layout::diff_against_model(&self.class_store, id, &model)
    }

    /// Emit the one-per-class shadow-layout census line for a class that has
    /// just been defined from real bytes, under `CRATONVM_DBG=overlay`.
    ///
    /// Deliberately per class and not per access: the interesting output is the
    /// slot map, it never changes for a given image, and printing it once keeps
    /// it readable next to the access-site lines. `overlay-all` widens it from
    /// "only classes that actually disagree" to every class carrying a model, so
    /// the `Agrees` rows can be audited too — most of them are anonymous slots
    /// over real references, which this instrument cannot falsify (see the
    /// module docs).
    pub(in crate::class_manager) fn report_shadow_layout(&self, id: ClassId) {
        // Latched once: this runs on every class definition, and the default
        // path must not pay an env lookup per class.
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON
            .get_or_init(|| false)
        {
            return;
        }
        let Some(diff) = self.shadow_layout_diff(id) else {
            return;
        };
        let all = None::<std::ffi::OsString>.is_some();
        if diff.disagreement_count() == 0 && !all {
            return;
        }
        eprint!("{}", diff.render());
    }

    /// Re-check every `JDK-ONLY-LAYOUT: safe` positional claim about a class
    /// that has just been defined from real bytes.
    ///
    /// Wave-2 step 4. A `safe` verdict is an assertion about one specific JDK
    /// image, made once by a person reading `javap`, and nothing in the build
    /// re-checked it -- so a JDK upgrade that reordered a private field would
    /// not fail a test, it would silently corrupt an object.
    ///
    /// Deliberately NOT behind `CRATONVM_DBG_OVERLAY`, unlike the census next
    /// door. The census is a research instrument you switch on when you are
    /// already looking for something; this is a tripwire, and a tripwire that
    /// only fires while you are watching is not one. It costs a name compare
    /// against a six-row table per class definition, and only a class named in
    /// that table walks any fields at all.
    ///
    /// `debug_assert` on top of the log, so a broken claim fails the test suite
    /// rather than merely printing during it.
    pub(in crate::class_manager) fn check_safe_positional_claims(&self, id: ClassId) {
        let broken = crate::shadow_layout::check_positional_claims(&self.class_store, id);
        for message in &broken {
            tracing::error!(
                target: "cratonvm::layout",
                "JDK-ONLY-LAYOUT `safe` claim broken by this image: {message}"
            );
            eprintln!("[LAYOUT-CLAIM] BROKEN: {message}");
        }
        debug_assert!(
            broken.is_empty(),
            "a JDK-ONLY-LAYOUT `safe` claim does not hold for the loaded image:\n{}",
            broken.join("\n")
        );
    }

    /// Re-parent a loaded class.
    ///
    /// Use this instead of writing `class.superclass` through
    /// [`Self::get_class_mut`]: the store keeps a direct-subclass adjacency
    /// index (see [`ClassStore::descendants_of`]) that a raw field write
    /// desynchronises, which would make `recompute_subclass_layouts` blind to
    /// the class on the next superclass layout change.
    pub fn set_superclass(&mut self, id: ClassId, superclass: Option<ClassId>) {
        self.class_store.set_superclass(id, superclass);
    }

    /// Replace a loaded class's implemented-interface list.
    ///
    /// Use this instead of writing `class.interfaces` through
    /// [`Self::get_class_mut`]: the store keeps a direct-implementor adjacency
    /// index (see [`ClassStore::subtypes_of`]) that a raw field write
    /// desynchronises, which would make class-hierarchy analysis blind to
    /// every `invokeinterface` site whose implementors arrived this way — and
    /// blind SILENTLY, because a missing edge reads as "nothing to
    /// devirtualize here" rather than as an error.
    pub fn set_interfaces(&mut self, id: ClassId, interfaces: Vec<ClassId>) {
        self.class_store.set_interfaces(id, interfaces);
    }

    /// Atomically detach every class defined by `loader_id` from the live
    /// metadata graph.
    ///
    /// The underlying [`ClassStore`] leaves monotonic tombstones, so stale
    /// ClassIds fail closed and can never alias a later definition. Callers
    /// must additionally evict VM-owned ClassId caches (statics, mirrors,
    /// vtables and executable code); the returned identities are the exact
    /// invalidation set for that transaction.
    pub fn unload_user_loader(&mut self, loader_id: ClassLoaderId) -> Vec<UnloadedClass> {
        if !matches!(loader_id, ClassLoaderId::UserDefined(_)) {
            return Vec::new();
        }

        let ids: FxHashSet<ClassId> = self
            .class_store
            .iter()
            .filter(|class| class.loader_id == loader_id)
            .map(|class| class.id)
            .collect();
        self.unload_user_classes_inner(ids)
    }

    /// Atomically detach exactly the requested user-defined classes from the
    /// live metadata graph.
    ///
    /// A class-manager loader id is a resolution namespace, not necessarily a
    /// unique Java `ClassLoader` object: JDK dynamic proxies can share a
    /// synthetic namespace even when their defining loaders have different
    /// lifetimes. GC therefore supplies the ClassIds whose defining-loader
    /// entries were proven dead, and unloading any sibling merely because it
    /// shares that namespace would invalidate a still-executing class.
    ///
    /// A HIDDEN class is accepted whatever its namespace (gc-common w18-d):
    /// see [`Self::unloads_on_hint`].
    pub fn unload_user_classes(&mut self, class_ids: &[ClassId]) -> Vec<UnloadedClass> {
        let ids: FxHashSet<ClassId> = class_ids
            .iter()
            .copied()
            .filter(|id| self.unloads_on_hint(*id))
            .collect();
        self.unload_user_classes_inner(ids)
    }

    /// May an unload hint for `class_id` (a class the collector proved dead)
    /// remove it from the store?
    ///
    /// * A class of a USER namespace: yes, that is what the hints are for.
    /// * A HIDDEN class, in any namespace (gc-common w18-d): yes. A hidden
    ///   class has no name any loader can resolve, so nothing can reach it
    ///   but its mirror and its instances, and a hint for it means those are
    ///   dead: a non-strong one's mirror died
    ///   (`native-builtins::classloader::gc_reconcile_defining_loaders`), or
    ///   a strong one's defining loader did, which its mirror's `classLoader`
    ///   field would have kept alive. The JDK's `LambdaForm` classes and an
    ///   application's `Lookup.defineHiddenClass` results live in the
    ///   bootstrap and application namespaces.
    /// * Any other class of a built-in namespace: no. Its name still resolves
    ///   there, so it stays loaded (fail-closed behind its orphan marker).
    ///
    /// `vm::memory::gc::unload_dead_class_metadata` filters its hints with the
    /// same question before calling [`Self::unload_user_classes`].
    pub fn unloads_on_hint(&self, class_id: ClassId) -> bool {
        self.class_store.get(class_id).is_some_and(|class| {
            class.hidden || matches!(class.loader_id, ClassLoaderId::UserDefined(_))
        })
    }

    pub(in crate::class_manager) fn unload_user_classes_inner(&mut self, ids: FxHashSet<ClassId>) -> Vec<UnloadedClass> {
        if ids.is_empty() {
            return Vec::new();
        }

        // Remove both defining and initiating-name aliases that point into the
        // exact dead class set. Do not remove aliases for live siblings that
        // happen to share the same resolution namespace.
        //
        // gc-common w18-d: the removed aliases are released from the name
        // index one by one, exactly as `loaded_classes_remove` releases a
        // single key. This used to rebuild the whole index from every alias
        // in the VM (`rebuild_name_definitions`, one `Arc` clone and two hash
        // inserts per loaded class) on every unload. That was once per dead
        // loader; since non-strong hidden classes unload with their mirrors it
        // can be once per collection.
        let mut released: Vec<(Arc<str>, ClassId)> = Vec::new();
        self.loaded_classes.retain(|(_, name), id| {
            if ids.contains(id) {
                released.push((Arc::clone(name), *id));
                false
            } else {
                true
            }
        });
        for (name, id) in &released {
            release_name_definition(&mut self.name_definitions, name, *id);
        }
        // Removal here is by ClassId set, not by key, so there is no
        // `loaded_classes_remove` call to carry the definition-epoch bump.
        // Bump it here instead: unloading a loader is exactly the case where a
        // name stops resolving to the `ClassId` a cache memoized it against.
        bump_class_definition_epoch(self.class_store.layout_domain());
        bump_class_name_generation(self.class_store.layout_domain());
        // gc-common w18-d: per dead id, not a sweep of every loaded class's
        // entry in each table. Same result: each table is keyed by `ClassId`.
        {
            let mut redefine_generations = self
                .redefine_generations
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut init_states = self.init_states.write();
            for id in &ids {
                self.vtable_descriptors.remove(id);
                self.skip_bytecode_verification.remove(id);
                self.verifier_link_loads.remove(id);
                self.type_maps_pending.remove(id);
                self.user_pass3_pending.remove(id);
                redefine_generations.remove(id);
                init_states.remove(id);
                self.redefinition_histories.remove(id);
            }
        }
        // C2 review P1: unloading is an identity-invalidating event, so bump
        // the metadata generation of every class in the dead set. The tombstone
        // that `ClassStore::remove` leaves already makes a stale handle fail
        // (as `StaleMetadata::Unloaded`), but the generation is what survives
        // if a slot is ever revived, and it is deliberately NOT dropped from
        // the realm here for that reason — unlike `redefine_generations` above,
        // whose entry is only a cache-eviction gate for a live class. One u32
        // per unloaded class is the same order of residue as the tombstone slot
        // itself, which is likewise never reclaimed.
        self.metadata_realm.bump_generations(ids.iter().copied());

        for id in &ids {
            self.class_type_annotations.remove(id);
            self.hidden_lookup_class_names.remove(id);
            // The self-name prefilter word (wave 22, lane hidden-resid; item
            // 2 of `r21-review-w22-findings-FIXED-20261020.md`). The recorded
            // class-file name goes with `class_store.remove` below.
            self.hidden_classes.record_self_name(*id, None);
            self.empty_record_classes.remove(id);
            self.class_bytes_raw_agent_bases.remove(id);
            if let Some(bytes) = self.class_bytes_cache.remove(id) {
                // A pinned retransformation base is not in the FIFO's count
                // (interpreter round i1 wave 28, lane L3).
                if self.class_bytes_pinned.remove(id) {
                    self.class_bytes_pinned_size =
                        self.class_bytes_pinned_size.saturating_sub(bytes.len());
                } else {
                    self.class_bytes_cache_size =
                        self.class_bytes_cache_size.saturating_sub(bytes.len());
                }
            }
            self.pre_verified_redefinitions.get_mut().remove(id);
            // `-XX:MaxMetaspaceSize` accounting: give back exactly what the
            // define charged (gc-common w6-f).
            if let Some(charge) = self.metaspace_charges.remove(id) {
                self.metaspace_charged_bytes = self.metaspace_charged_bytes.saturating_sub(charge);
            }
            // The retransformation-base fingerprint. The FIFO never evicts it,
            // and until gc-common w9-e nothing else did either: every unloaded
            // class left its 16-byte row behind for the life of the VM. Ids
            // are not reused, so the row could only ever answer for a
            // tombstone (a leak, not a stale answer).
            self.class_bytes_base_digest.remove(id);
        }
        self.class_bytes_cache_fifo.retain(|id| !ids.contains(id));

        let mut unloaded = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(class) = self.class_store.remove(id) {
                unloaded.push(UnloadedClass {
                    id,
                    name: class.name,
                    loader_id: class.loader_id,
                });
                // No `fire_resolution_invalidate_hook` here (gc-common w8-e).
                // The symbolic and reflective resolution caches may hold an
                // unloaded class as key or as resolved target. The caller
                // evicts them for the whole set in one pass each, while it
                // still holds this manager's write lock
                // (`vm::memory::gc::unload_dead_class_metadata`). The hook
                // swept both caches once PER CLASS, in every live VM (it
                // carries no VM identity), inside the collection pause. See
                // `docs/feature-designs/class-resolution-and-caching.md`.
            }
        }
        // gc-common w18-d: only an unload that removed a USER-namespace class
        // can empty a user namespace, so only such an unload re-derives the
        // set (a walk of every class in the store). A batch of hidden classes
        // of built-in namespaces -- the per-collection case -- skips it.
        if unloaded
            .iter()
            .any(|class| matches!(class.loader_id, ClassLoaderId::UserDefined(_)))
        {
            self.user_loaders = self
                .class_store
                .iter()
                .filter_map(|class| {
                    matches!(class.loader_id, ClassLoaderId::UserDefined(_))
                        .then_some(class.loader_id)
                })
                .collect();
        }
        // Loader constraints (JVMS 5.3.4): the rows of loaders this unload
        // left with no class, and every pin naming an unloaded class
        // (gc-common w9-e, applied by the orchestrator). See
        // docs/feature-designs/class-resolution-and-caching.md.
        //
        // Only a USER namespace can be a dead loader (gc-common w18-d). A
        // built-in namespace is never in `user_loaders`, so an unloaded hidden
        // class of the application or bootstrap namespace used to name that
        // live loader dead here, and every constraint row it held was dropped.
        let dead_loaders: FxHashSet<u32> = unloaded
            .iter()
            .map(|class| class.loader_id)
            .filter(|loader| {
                matches!(loader, ClassLoaderId::UserDefined(_))
                    && !self.user_loaders.contains(loader)
            })
            .map(|loader| loader.to_native_id())
            .collect();
        let dead_classes: FxHashSet<u32> = unloaded.iter().map(|class| class.id.as_u32()).collect();
        self.loader_constraints
            .get_mut()
            .forget(&|loader| dead_loaders.contains(&loader), &|class| {
                dead_classes.contains(&class)
            });
        unloaded.sort_unstable_by_key(|class| class.id.as_u32());
        unloaded
    }

    /// Find a raw classpath resource by name.
    ///
    /// Searches all classpaths in order (bootstrap → extension → application).
    /// Returns the raw bytes of the first match, or `None` if not found.
    pub fn find_resource(&self, name: &str) -> Option<Vec<u8>> {
        self.bootstrap
            .class_path()
            .find_resource(name)
            .or_else(|| self.extension.class_path().find_resource(name))
            .or_else(|| self.application.class_path().find_resource(name))
    }

    /// Test only the application classpath for a resource, without reading or
    /// inflating it. This deliberately excludes bootstrap and extension
    /// modules so an identically named JDK resource cannot enable an
    /// application compatibility pack.
    pub fn application_contains_resource(&self, name: &str) -> bool {
        self.application.class_path().contains_resource(name)
    }

    /// Return a URL string for every classpath entry that contains a resource
    /// with the given name. Searches bootstrap, extension, and application
    /// classpaths in order and concatenates the results. Analog of
    /// `ClassLoader.getResources` — enumerates every match rather than
    /// stopping at the first.
    pub fn find_all_resource_urls(&self, name: &str) -> Vec<String> {
        let mut out = self.bootstrap.class_path().find_all_resource_urls(name);
        out.extend(self.extension.class_path().find_all_resource_urls(name));
        out.extend(self.application.class_path().find_all_resource_urls(name));
        out
    }

    /// The first `jrt:` URL of [`Self::find_all_resource_urls`] for `name`,
    /// asking only the class-path entries that can name one
    /// ([`ClassPath::first_runtime_image_resource_url`]), in the same
    /// bootstrap, extension, application order. Interpreter round i3 wave 73,
    /// lane L5 (`CRATONVM_PLATFORM_RESOURCE_JRT_WALK`).
    pub fn first_runtime_image_resource_url(&self, name: &str) -> Option<String> {
        self.bootstrap
            .class_path()
            .first_runtime_image_resource_url(name)
            .or_else(|| {
                self.extension
                    .class_path()
                    .first_runtime_image_resource_url(name)
            })
            .or_else(|| {
                self.application
                    .class_path()
                    .first_runtime_image_resource_url(name)
            })
    }

    /// [`Self::find_all_resource_urls`] restricted to ONE class-path segment.
    ///
    /// `segment` uses the same numbering [`Self::next_resource_url_from`]
    /// already documents — 0 bootstrap, 1 extension, 2 application — so the
    /// three segments concatenated in that order reproduce
    /// `find_all_resource_urls` exactly. Any other value yields nothing.
    ///
    /// Added for `ClassLoader.getDefinedPackage`, which is NON-DELEGATING: the
    /// application loader must not answer for `java.lang` merely because the
    /// boot image on segment 0 contains it. Before this existed the only probe
    /// available was the concatenation, so every built-in loader claimed every
    /// package in the process.
    pub fn find_resource_urls_in_segment(&self, name: &str, segment: usize) -> Vec<String> {
        match segment {
            0 => self.bootstrap.class_path().find_all_resource_urls(name),
            1 => self.extension.class_path().find_all_resource_urls(name),
            2 => self.application.class_path().find_all_resource_urls(name),
            _ => Vec::new(),
        }
    }

    /// `true` when [`Self::next_resource_url_from`] can serve `name`.
    pub fn resource_name_supports_incremental_scan(name: &str) -> bool {
        crate::class_path::ClassPath::name_supports_incremental_scan(name)
    }

    /// The incremental form of [`Self::find_all_resource_urls`]: the next URL
    /// at or after `(segment, index)`, plus the cursor to resume at.
    ///
    /// `segment` walks the same three class paths in the same order the
    /// whole-list version concatenates them — 0 bootstrap, 1 extension, 2
    /// application — so enumerating from `(0, 0)` to exhaustion yields exactly
    /// what `find_all_resource_urls` returns, in the same order. It exists so a
    /// caller that stops early stops the SCAN early; see
    /// `ClassPath::next_resource_url_from` for the measurement.
    ///
    /// A cursor is only meaningful for as long as the classpath is unchanged.
    /// Appending a root mid-enumeration shifts what an index names, exactly as
    /// it does for a JDK `Enumeration` held across a `URLClassLoader.addURL`.
    pub fn next_resource_url_from(
        &self,
        name: &str,
        segment: usize,
        index: usize,
    ) -> Option<(String, usize, usize)> {
        let paths = [
            self.bootstrap.class_path(),
            self.extension.class_path(),
            self.application.class_path(),
        ];
        let mut seg = segment;
        let mut idx = index;
        while seg < paths.len() {
            if let Some((url, next)) = paths[seg].next_resource_url_from(name, idx) {
                return Some((url, seg, next));
            }
            seg += 1;
            idx = 0;        }
        None
    }

    /// with the given name. Parallel to [`find_all_resource_urls`] but returns
    /// content rather than URLs — used by Rust-native resource enumeration
    /// paths (e.g. `ServiceLoader` provider discovery in
    /// `native-builtins/src/service_loader.rs`) that bypass the JDK's
    /// `URL.openStream` / `BufferedReader` chain. Searches bootstrap →
    /// extension → application and concatenates the results.
    pub fn find_all_resource_bytes(&self, name: &str) -> Vec<Vec<u8>> {
        let mut out = self.bootstrap.class_path().find_all_resource_bytes(name);
        out.extend(self.extension.class_path().find_all_resource_bytes(name));
        out.extend(self.application.class_path().find_all_resource_bytes(name));
        out
    }

    /// SBR5 `clload`: see `ClassPath::fast_define_ok`; asked of the application class path.
    pub fn application_fast_define_ok(&self, internal: &str) -> bool {
        self.application.class_path().fast_define_ok(internal)
    }

    /// SBR5 `clload`: see `ClassPath::fast_absent_ok`.
    pub fn application_fast_absent_ok(&self, internal: &str) -> bool {
        self.application.class_path().fast_absent_ok(internal)
    }

    /// The application class path as JDK 25's lazy `URLClassPath` over
    /// `java_class_path` ends with (`ClassPath::apply_url_class_path_model`;
    /// interpreter round i5 wave 108, lane L5). Call once, right after
    /// construction.
    pub fn apply_application_url_class_path_model(&mut self, java_class_path: &[String]) -> bool {
        self.application.apply_url_class_path_model(java_class_path)
    }

    /// `ClassPath::take_provider_circularity_error` of the application class
    /// path (wave 108, lane L5).
    pub fn application_take_provider_circularity_error(&self) -> bool {
        self.application.class_path().take_provider_circularity_error()
    }

    /// `ClassPath::provider_lookup_class_path` of the application class path
    /// (wave 108, lane L5).
    pub fn application_provider_lookup_class_path(&self) -> bool {
        self.application.class_path().provider_lookup_class_path()
    }

    /// The application class path's own parse of the archive at `path`
    /// (`ClassPath::outer_archive_for`; interpreter round i2 wave 69, lane
    /// L5, `CRATONVM_NESTED_JAR_CLASS_PATH_OUTER`), or `None`.
    pub fn application_outer_archive(
        &self,
        path: &std::path::Path,
    ) -> Option<crate::class_path::OuterArchive> {
        self.application.class_path().outer_archive_for(path)
    }

    /// The application class path's own answer for the entry `name` of the
    /// jar at `path` (`ClassPath::jar_entry_bytes_matching`; interpreter
    /// round i3 wave 71, lane L5, `CRATONVM_JAR_LOADER_CLASS_PATH_DIRECT`).
    pub fn application_jar_entry_bytes(
        &self,
        path: &std::path::Path,
        name: &str,
        crc32: u32,
        size: u64,
    ) -> cratonvm_native_api::ClassPathJarBytes {
        self.application
            .class_path()
            .jar_entry_bytes_matching(path, name, crc32, size)
    }

    /// Find the filesystem path of the classpath entry that holds a given
    /// class.  Used by `Class.getProtectionDomain()` to build a CodeSource
    /// with a real location URL.  Searches application → extension → bootstrap.
    pub fn find_class_source_path(&self, class_name: &str) -> Option<String> {
        self.application
            .class_path()
            .find_class_source_path(class_name)
            .or_else(|| {
                self.extension
                    .class_path()
                    .find_class_source_path(class_name)
            })
            .or_else(|| {
                self.bootstrap
                    .class_path()
                    .find_class_source_path(class_name)
            })
    }

    /// Build a real `CodeSource` for a given class by locating its origin
    /// classpath entry and, if that entry is a signed JAR, extracting the
    /// signer certificate blocks from `META-INF`.  Searches application →
    /// extension → bootstrap; returns `None` if the class is a synthetic
    /// stub or lives in a JMOD/jimage module.
    pub fn find_class_code_source(&self, class_name: &str) -> Option<CodeSource> {
        self.find_class_code_source_for_loader(class_name, ClassLoaderId::Application)
    }

    /// Loader-scoped variant of [`Self::find_class_code_source`]: bootstrap
    /// defines only search the bootstrap classpath, extension defines search
    /// extension → bootstrap, avoiding wasted application directory `stat()`
    /// probes on every JDK class.
    pub fn find_class_code_source_for_loader(
        &self,
        class_name: &str,
        loader_id: ClassLoaderId,
    ) -> Option<CodeSource> {
        let info = match loader_id {
            ClassLoaderId::Bootstrap => self.bootstrap.class_path().find_class_code_source_info(class_name),
            ClassLoaderId::Extension => self
                .extension
                .class_path()
                .find_class_code_source_info(class_name)
                .or_else(|| {
                    self.bootstrap
                        .class_path()
                        .find_class_code_source_info(class_name)
                }),
            ClassLoaderId::Application | ClassLoaderId::UserDefined(_) => self
                .application
                .class_path()
                .find_class_code_source_info(class_name)
                .or_else(|| {
                    self.extension
                        .class_path()
                        .find_class_code_source_info(class_name)
                })
                .or_else(|| {
                    self.bootstrap
                        .class_path()
                        .find_class_code_source_info(class_name)
                }),
        };
        let (url, certs) = info?;
        Some(CodeSource::new(Some(url), certs))
    }

    /// WP2.3 — query whether a class was registered with
    /// `DefineClassOptions::skip_verification = true`.
    ///
    /// Used by the VM's link-time verifier to bypass Pass-3 bytecode
    /// type-checking for trusted hidden / runtime-generated classes
    /// (JDK Proxy, ByteBuddy, CGLIB) that pass JVMS structural rules
    /// but emit synthesised stack frames that our verifier does not
    /// model.  Default `false` — every other class is verified.
    pub fn class_skip_bytecode_verification(&self, class_id: ClassId) -> bool {
        self.skip_bytecode_verification.contains(&class_id)
    }

    /// The names the link step of `class_id` still loads through its
    /// defining loader, as HotSpot's verifier does (empty when none). See
    /// the `verifier_link_loads` field.
    pub fn verifier_link_loads(&self, class_id: ClassId) -> &[crate::vtype::VerifierLinkLoad] {
        self.verifier_link_loads
            .get(&class_id)
            .map_or(&[], |names| names)
    }

    /// The link step of `class_id` made its verifier loads: forget them. A
    /// failed link keeps them, and the next attempt loads them again, as
    /// HotSpot verifies again.
    pub fn verifier_link_loads_done(&mut self, class_id: ClassId) {
        self.verifier_link_loads.remove(&class_id);
    }

    /// Override the env-read `CRATONVM_DEFINE_NO_CLASS_PATH_CODE_SOURCE`
    /// switch (the `define_no_class_path_code_source` field).
    pub fn set_define_no_class_path_code_source(&mut self, on: bool) {
        self.define_no_class_path_code_source = on;
    }

    /// Override the env-read `CRATONVM_DEFINE_TYPE_MAPS_AT_LINK` switch
    /// (the `type_maps_at_link` field). Call before the first define.
    pub fn set_type_maps_at_link(&mut self, on: bool) {
        self.type_maps_at_link = on;
    }

    /// Override the env-read `CRATONVM_DEFINE_BOOT_MAPS_AT_LINK` switch (the
    /// `define_boot_maps_at_link` field). Call before the first define.
    pub fn set_define_boot_maps_at_link(&mut self, on: bool) {
        self.define_boot_maps_at_link = on;
    }

    /// Enable or disable skipping Pass 3 bytecode verification and eager
    /// type-map walks for trusted bootstrap classes (matching HotSpot
    /// `-Xverify:remote`). Call before the first define.
    pub fn set_skip_boot_verification(&mut self, on: bool) {
        self.skip_boot_verification = on;
    }
    /// Does `class_id` still owe the type-map walk its define left to its
    /// link (`CRATONVM_DEFINE_TYPE_MAPS_AT_LINK`)?
    pub fn type_maps_pending(&self, class_id: ClassId) -> bool {
        !self.type_maps_pending.is_empty() && self.type_maps_pending.contains_key(&class_id)
    }

    /// `CRATONVM_DEFINE_TYPE_MAPS_AT_LINK` (interpreter round i3 wave 81,
    /// lane L5): run the type-map walk the define of `class_id` left to its
    /// link, exactly as the define's deferred branch runs it -- the same
    /// loader-aware hierarchy, the class answering its own name and its
    /// direct superclass edge -- and publish the maps (first writer wins, as
    /// at define). Returns the verifier loads the walk recorded, for
    /// [`Self::finish_pending_type_maps`]; `None` when nothing is pending.
    /// `Some(Err)` is the refusal of the structural scan a trusted bootstrap
    /// define left here too (Phase 5 fast boot, `PendingTypeMaps::
    /// structural_scan`): the link fails with it, nothing is published, and
    /// the class stays pending, so its next link attempt refuses again.
    /// Takes `&self`: the VM runs it under the class-manager READ lock.
    pub fn publish_pending_type_maps(
        &self,
        class_id: ClassId,
    ) -> Option<Result<Box<[crate::vtype::VerifierLinkLoad]>, LinkageError>> {
        let &PendingTypeMaps {
            record_loads,
            structural_scan,
        } = self.type_maps_pending.get(&class_id)?;
        let class = self.class_store.get(class_id)?;
        let super_name: Option<&str> = class
            .superclass
            .and_then(|super_id| self.class_store.get(super_id))
            .map(|super_class| &*super_class.name);
        // JIT round 23 wave 8f (lane W8-L1, proxy-define page;
        // `CRATONVM_LINK_TYPE_MAP_WALK_LOADS_ONLY`, opt-in `=1`; off walks as
        // before): the link's Pass 3 verdict walk ran first and already
        // published this class's verified maps (`verify_bytecode_walk`), so
        // this walk's maps would be discarded (first writer wins). Without
        // loads to record it is skipped outright; with them it walks for the
        // loads only, building no row ([`Self::type_map_walk_is_loads_only`]).
        let loads_only = self.type_map_walk_is_loads_only(class_id);
        // Phase 5 fast boot (lane p5-define): a scan the define left here
        // runs whatever the walk does (`PendingTypeMaps::structural_scan`).
        if loads_only && structural_scan {
            if let Err(e) = crate::verifier::verify_class_structural_bytecode(class) {
                return Some(Err(e));
            }
        }
        if loads_only && !record_loads {
            return Some(Ok(Box::default()));
        }
        let loads = std::cell::RefCell::new(Vec::new());
        {
            let hierarchy = ClassStoreHierarchy {
                class_store: &self.class_store,
                loaded_classes: &self.loaded_classes,
                user_loader_parents: &self.user_loader_parents,
                in_flight: Some(class),
                in_flight_super_name: super_name,
                requesting_loader: Some(class.loader_id),
                verifier_loads: record_loads.then_some(&loads),
                app_workarounds: &self.app_workarounds,
            };
            if loads_only {
                crate::verifier::walk_class_for_verifier_loads(class, &hierarchy);
            } else if structural_scan
                && !record_loads
                && self.class_store.type_maps().drops_rows()
                && class.version.major >= 51
            {
                let verify_structural = self.strict_verification
                    || !self.bootstrap.class_path().has_entries()
                    || !crate::bytecode_verifier::class_is_bootstrap_trusted(class);
                if let Err(e) =
                    crate::verifier::publish_deferred_class_type_maps_scanned_without_rows(
                        class, &hierarchy, verify_structural,
                    )
                {
                    return Some(Err(e));
                }
            } else if structural_scan {
                if let Err(e) =
                    crate::verifier::publish_deferred_class_type_maps_scanned(class, &hierarchy)
                {
                    return Some(Err(e));
                }
            } else {
                crate::verifier::publish_deferred_class_type_maps(class, &hierarchy);
            }
        }
        Some(Ok(loads.into_inner().into_boxed_slice()))
    }

    /// JIT round 23 wave 8f (`CRATONVM_LINK_TYPE_MAP_WALK_LOADS_ONLY`): would
    /// the link's type-map walk of `class_id` publish nothing, because this
    /// VM's store already holds VERIFIED maps for it (a skipped marker is
    /// upgraded by a publish, so it does not count)? Then only the walk's
    /// verifier loads matter. `false` with the switch off.
    ///
    /// Perf, **opt-in** (`=1`) by the n23-w8g A/B (pinned, 3 reps,
    /// `Zs9Nd8ProxyCost room 3000`, main-thread µs/define): default
    /// 1875/1661/2239 vs `=0` 1598/2109/2244 -- neutral on a noisy host,
    /// although the w8d profile put the walk at 5.5 % inclusive.
    pub fn type_map_walk_is_loads_only(&self, class_id: ClassId) -> bool {
        cratonvm_types::flags::runtime_var_os("CRATONVM_LINK_TYPE_MAP_WALK_LOADS_ONLY")
            .as_deref()
            != Some(std::ffi::OsStr::new("0"))
            && self
                .class_store
                .type_maps()
                .class_type_maps(class_id)
                .is_some_and(|maps| maps.verified())
    }

    /// zt df4 §9c: is a boot-namespace class of `module` a class HotSpot's
    /// platform or application loader defines (a platform or tool module of
    /// the runtime image, [`crate::module::ModuleRegistry::image_module_builtin_loader`])?
    /// `false` without a module, before the module tables are handed in, and
    /// under `CRATONVM_FIND_LOADED_BUILTIN_IMAGE_ROWS=0`.
    pub fn boot_tagged_image_class_of_builtin_loader(&self, module: Option<&str>) -> bool {
        module.is_some_and(|m| self.module_registry.image_module_builtin_loader(m).is_some())
    }

    /// Settle [`Self::publish_pending_type_maps`]: `class_id` owes no walk any
    /// more, and its link step loads the verifier loads the walk recorded
    /// ([`Self::verifier_link_loads`]), as a define-time walk would have left
    /// them.
    pub fn finish_pending_type_maps(
        &mut self,
        class_id: ClassId,
        loads: Box<[crate::vtype::VerifierLinkLoad]>,
    ) {
        if self.type_maps_pending.remove(&class_id).is_some() && !loads.is_empty() {
            self.verifier_link_loads.insert(class_id, loads);
        }
    }

    /// Does `class_id` still owe the Pass 3 verdict its define left to its
    /// link (`CRATONVM_VERIFY_USER_LOADER_CLASSES_AT_LINK`)?
    pub fn user_pass3_pending(&self, class_id: ClassId) -> bool {
        !self.user_pass3_pending.is_empty() && self.user_pass3_pending.contains(&class_id)
    }

    /// `CRATONVM_VERIFY_USER_LOADER_CLASSES_AT_LINK` (interpreter round i5
    /// wave 102, lane L7): give the class the verdict its define left to its
    /// link -- the linkage rules and the type-state walk
    /// ([`crate::verifier::verify_user_loader_class_bytecode`]) -- through
    /// the same loader-aware hierarchy the define would have used (the class
    /// answering its own name and its direct superclass edge), recording no
    /// verifier loads (the type-map walk records them). `Ok` when nothing is
    /// pending. Takes `&self`: the VM runs it under the class-manager READ
    /// lock; settle a pass with [`Self::finish_pending_user_pass3`].
    pub fn verify_pending_user_pass3(&self, class_id: ClassId) -> Result<(), LinkageError> {
        if !self.user_pass3_pending(class_id) {
            return Ok(());
        }
        let Some(class) = self.class_store.get(class_id) else {
            return Ok(());
        };
        let super_name: Option<&str> = class
            .superclass
            .and_then(|super_id| self.class_store.get(super_id))
            .map(|super_class| &*super_class.name);
        let hierarchy = ClassStoreHierarchy {
            class_store: &self.class_store,
            loaded_classes: &self.loaded_classes,
            user_loader_parents: &self.user_loader_parents,
            in_flight: Some(class),
            in_flight_super_name: super_name,
            requesting_loader: Some(class.loader_id),
            verifier_loads: None,
            app_workarounds: &self.app_workarounds,
        };
        // The full type-state walk applies the two linkage rules
        // `verify_deferred_class_bytecode` checks at define with the switch
        // off, in method order as HotSpot reports them, so it alone is the
        // verdict.
        crate::verifier::verify_user_loader_class_bytecode(class, &hierarchy)
    }

    /// `--noverify` (`-XX:-BytecodeVerificationRemote`) form of
    /// [`Self::verify_pending_user_pass3`]: HotSpot runs no verifier at all,
    /// but the interpreter relies on the structural scan (no branch into an
    /// instruction, no local past `max_locals`), which the define left to this
    /// link (interpreter round i5 wave 103, lane L7). `Ok` when nothing is
    /// pending; settle a pass with [`Self::finish_pending_user_pass3`].
    pub fn verify_pending_user_structural(&self, class_id: ClassId) -> Result<(), LinkageError> {
        if !self.user_pass3_pending(class_id) {
            return Ok(());
        }
        match self.class_store.get(class_id) {
            Some(class) => crate::verifier::verify_class_structural_bytecode(class),
            None => Ok(()),
        }
    }

    /// `CRATONVM_USER_PASS3_RECORDS_LINK_LOADS` (HotSpot parity, default ON
    /// since JIT round 23 wave 10, R23-W10-FLIP; `=0` restores the two-walk
    /// route): the two-walk route under-records the verifier loads HotSpot
    /// makes through the defining loader. `VLoad.java` on
    /// Temurin 25.0.3: `TckLoading`, `TckLang` and `RealFjp` match HotSpot
    /// exactly on one walk and miss loads on two (page
    /// `docs/feature-designs/class-resolution-and-caching.md`).
    /// As performance it lost the n23-w9c A/B 2 of 3 (`Zs9Nd8ProxyCost room
    /// 3000` µs/define 2026/2359/1916 vs `=0` 1645/1692/2223); parity
    /// overrides that. Pinned by `tests/verifier_loads_one_walk_diff.rs`;
    /// interpreter round i6 wave 119, lane L5,
    /// `i102-L5-proposal-one-walk-for-user-loader-pass3-and-type-maps`): does
    /// the link of `class_id` owe BOTH the Pass 3 verdict and the type-map walk,
    /// so that one walk can settle both
    /// ([`Self::verify_pending_user_pass3_one_walk`])?
    pub fn user_pass3_one_walk_due(&self, class_id: ClassId) -> bool {
        let flags = loader_flags();
        flags.user_pass3_records_link_loads
            && flags.verify_user_loader_classes
            && self.user_pass3_pending(class_id)
            && self.type_maps_pending(class_id)
    }

    /// The link's ONE walk (see [`Self::user_pass3_one_walk_due`]): the Pass 3
    /// verdict walk ([`Self::verify_pending_user_pass3`]'s, undecidable
    /// assignability answered "assignable"), recording the verifier loads the
    /// type-map walk would record -- the assignability loads and, at each
    /// method's handler table, the catch-type loads, in HotSpot's order -- and
    /// publishing the type maps (the walk's own `type_map_store`, first writer
    /// wins). `Ok(loads)` to settle with [`Self::finish_user_pass3_one_walk`];
    /// `Err` the verdict's refusal (nothing settled: the type-map row stays,
    /// as the verdict does). Takes `&self` (the class-manager READ lock).
    pub fn verify_pending_user_pass3_one_walk(
        &self,
        class_id: ClassId,
    ) -> Result<Box<[crate::vtype::VerifierLinkLoad]>, LinkageError> {
        let Some(class) = self.class_store.get(class_id) else {
            return Ok(Box::default());
        };
        let record_loads = self
            .type_maps_pending
            .get(&class_id)
            .is_some_and(|pending| pending.record_loads);
        let super_name: Option<&str> = class
            .superclass
            .and_then(|super_id| self.class_store.get(super_id))
            .map(|super_class| &*super_class.name);
        let loads = std::cell::RefCell::new(Vec::new());
        {
            let hierarchy = ClassStoreHierarchy {
                class_store: &self.class_store,
                loaded_classes: &self.loaded_classes,
                user_loader_parents: &self.user_loader_parents,
                in_flight: Some(class),
                in_flight_super_name: super_name,
                requesting_loader: Some(class.loader_id),
                verifier_loads: record_loads.then_some(&loads),
                app_workarounds: &self.app_workarounds,
            };
            crate::verifier::verify_user_loader_class_bytecode_recording(class, &hierarchy)?;        }
        Ok(loads.into_inner().into_boxed_slice())    }

    /// Settle [`Self::verify_pending_user_pass3_one_walk`]: `class_id` owes
    /// neither the verdict nor the type-map walk; its link step loads `loads`.
    pub fn finish_user_pass3_one_walk(
        &mut self,
        class_id: ClassId,
        loads: Box<[crate::vtype::VerifierLinkLoad]>,
    ) {
        self.user_pass3_pending.remove(&class_id);
        self.finish_pending_type_maps(class_id, loads);
    }

    /// Settle [`Self::verify_pending_user_pass3`]: `class_id` passed, its
    /// link owes no verdict any more.
    pub fn finish_pending_user_pass3(&mut self, class_id: ClassId) {
        self.user_pass3_pending.remove(&class_id);
    }

    // -------------------------------------------------------------------
    // C2 review P1 — generational metadata handles
    //
    // See `crate::metadata_handle` for the model and
    // `docs/feature-designs/class-resolution-and-caching.md` for the migration plan.
    // Everything here is additive: `ClassId` keeps working unchanged, and a
    // handle is what you mint when a reference is about to outlive the borrow
    // that produced it.
    // -------------------------------------------------------------------

    /// This manager's metadata realm — VM identity plus per-class generation.
    pub fn metadata_realm(&self) -> &MetadataRealm {
        &self.metadata_realm
    }

    /// The VM this manager's `ClassId`s are meaningful in, or
    /// [`UNBOUND_VM`](crate::metadata_handle::UNBOUND_VM) before binding.
    pub fn vm_id(&self) -> VmId {
        self.metadata_realm.vm()
    }

    /// Record which VM owns this manager.
    ///
    /// **Ordering:** call this immediately after the owning `SharedVm` has been
    /// assigned its `vm_identity` and *before* any handle is minted. Handles
    /// minted from an unbound manager carry
    /// [`UNBOUND_VM`](crate::metadata_handle::UNBOUND_VM) and are rejected with
    /// [`StaleMetadata::WrongVm`] once binding happens — the fail-closed
    /// direction, but still a rejection the VM would rather not have to
    /// explain.
    ///
    /// Takes `&self` (the realm's identity is an atomic) so it can be called
    /// through the `class_manager.read()` guard that `vm_init` already holds.
    ///
    /// Idempotent for the same [`VmId`]. Returns `Err(current)` if a
    /// *different* VM already claimed this manager: that means two `SharedVm`s
    /// share one `ClassManager`, under which every `ClassId` in the store is
    /// ambiguous, and the caller must fail rather than pick a winner.
    pub fn bind_vm_id(&self, vm: VmId) -> Result<(), VmId> {
        self.metadata_realm.bind_vm(vm)
    }

    /// Mint a handle for a **live** class. `None` if `class_id` names no loaded
    /// class in this manager (never loaded, or unloaded).
    ///
    /// The handle records the current VM identity and the class's current
    /// metadata generation, so it can be stored indefinitely and checked later
    /// with [`Self::resolve_class_handle`].
    pub fn class_handle(&self, class_id: ClassId) -> Option<ClassHandle> {
        if self.class_store.get(class_id).is_none() {
            return None;
        }
        self.metadata_realm.mint_class(class_id)
    }

    /// Check `handle` and return the `ClassId` it names.
    ///
    /// Every rejection is named — see [`StaleMetadata`]. This is the *only*
    /// sanctioned way to turn a stored handle back into an index.
    pub fn resolve_class_id(&self, handle: ClassHandle) -> Result<ClassId, StaleMetadata> {
        // `slot_count`, not `len`: unloaded classes leave tombstones, and an
        // index that lands on a tombstone must report `Unloaded` rather than
        // `OutOfRange`.
        let slots = self.class_store.slot_count() as u32;
        let live = self.class_store.get(handle.class_id_unchecked()).is_some();
        self.metadata_realm.resolve_class(handle, slots, live)
    }

    /// Check `handle` and borrow the class it names.
    pub fn resolve_class_handle(&self, handle: ClassHandle) -> Result<&Class, StaleMetadata> {
        let id = self.resolve_class_id(handle)?;
        // `resolve_class_id` already established that the slot is live; the
        // `ok_or` is for the borrow checker's benefit, not a second check.
        self.class_store
            .get(id)
            .ok_or(StaleMetadata::Unloaded { class: id })
    }

    /// Mint a handle for one method of a live class. `None` if the class is not
    /// loaded or `method_index` is past the end of its `methods` vec.
    pub fn method_handle(&self, class_id: ClassId, method_index: usize) -> Option<MethodHandle> {
        let class = self.class_store.get(class_id)?;
        if method_index >= class.methods.len() {
            return None;
        }
        let class_handle = self.metadata_realm.mint_class(class_id)?;
        Some(MethodHandle::from_parts(class_handle, method_index as u32))
    }

    /// Check `handle` and borrow the method it names.
    ///
    /// The class check runs first: a `RedefineClasses` replaces the whole
    /// `methods` vec, so a method index that is still *in range* after a
    /// redefine usually names a different method. That is precisely the silent
    /// failure the generation exists to catch.
    pub fn resolve_method_handle(
        &self,
        handle: MethodHandle,
    ) -> Result<&ClassFileMethod, StaleMetadata> {
        let class = self.resolve_class_handle(handle.class())?;
        let index = handle.index_unchecked();
        class
            .methods
            .get(index as usize)
            .ok_or(StaleMetadata::OutOfRange {
                kind: MetadataKind::Method,
                class: class.id,
                index,
                len: class.methods.len() as u32,
            })
    }

    /// Mint a handle for one declared field of a live class. `None` if the
    /// class is not loaded or `field_index` is past the end of its `fields`
    /// vec.
    ///
    /// `field_index` indexes the class's **own** `fields` vec, not the absolute
    /// instance-slot number — the latter shifts under
    /// `recompute_subclass_layouts` and is exactly what the generation guards.
    pub fn field_handle(&self, class_id: ClassId, field_index: usize) -> Option<FieldHandle> {
        let class = self.class_store.get(class_id)?;
        if field_index >= class.fields.len() {
            return None;
        }
        let class_handle = self.metadata_realm.mint_class(class_id)?;
        Some(FieldHandle::from_parts(class_handle, field_index as u32))
    }

    /// Check `handle` and borrow the field it names.
    pub fn resolve_field_handle(
        &self,
        handle: FieldHandle,
    ) -> Result<&ClassFileField, StaleMetadata> {
        let class = self.resolve_class_handle(handle.class())?;
        let index = handle.index_unchecked();
        class
            .fields
            .get(index as usize)
            .ok_or(StaleMetadata::OutOfRange {
                kind: MetadataKind::Field,
                class: class.id,
                index,
                len: class.fields.len() as u32,
            })
    }

    /// Current metadata generation of `class_id` — the value a freshly minted
    /// handle would carry. Zero for a class that has never been redefined,
    /// unloaded, or re-laid-out.
    ///
    /// Distinct from [`Self::class_redefine_generation`], which counts only
    /// JVMTI redefinitions and is what the per-thread invoke cache already
    /// gates on. This one additionally counts unloads and the synthetic-stub
    /// layout upgrade that renumbers fields.
    pub fn class_metadata_generation(&self, class_id: ClassId) -> u32 {
        self.metadata_realm.generation(class_id)
    }

    /// Advance `class_id`'s metadata generation, retiring every handle already
    /// minted for it.
    ///
    /// The class manager calls this itself on redefine, unload and layout
    /// upgrade. It is `pub` so a VM-side event that this crate cannot see (a
    /// mirror swap, an agent-driven metadata patch) can join the same
    /// invalidation protocol rather than inventing a parallel one.
    pub fn invalidate_class_metadata(&self, class_id: ClassId) -> u32 {
        self.metadata_realm.bump_generation(class_id)
    }

    // -------------------------------------------------------------------
    // WP2.4-B — JEP 109 / JVMTI RedefineClasses
    // -------------------------------------------------------------------

    /// WP2.4-B — current redefinition generation for a class. Starts
    /// at 0 for every freshly-defined class and increments by 1 on
    /// each successful [`Self::redefine_class`]. Caches that key
    /// entries by [`ClassId`] should snapshot this value at insert
    /// time and invalidate the entry on mismatch — that's cheaper than
    /// walking every cache at redefine time.
    pub fn class_redefine_generation(&self, class_id: ClassId) -> u32 {
        self.redefine_generations
            .read()
            .expect("redefine_generations poisoned")
            .get(&class_id)
            .map(|c| c.load(Ordering::Acquire))
            .unwrap_or(0)
    }

    /// The obsolete-method translations of `class_id`'s redefinitions, or
    /// `None` for a class never redefined (interpreter round i1 wave 19, lane
    /// L3; see [`crate::obsolete_code`]).
    pub fn redefinition_history(
        &self,
        class_id: ClassId,
    ) -> Option<&crate::obsolete_code::RedefinitionHistory> {
        self.redefinition_histories.get(&class_id)
    }

    /// Apply the stale-frame census to every redefined class's history
    /// (`RedefinitionHistory::prune`): no frame of the class the census can
    /// see is stamped below `floor_of(class)`; `None` leaves the history as
    /// it is (no census). Called by the VM under this manager's write lock,
    /// just before a redefinition (interpreter round i1 wave 23, lane L3; a
    /// floor per class since wave 24:
    /// `obsolete_frames::prune_histories_by_census`).
    pub fn prune_redefinition_histories(&mut self, floor_of: impl Fn(ClassId) -> Option<u64>) {
        for (&class_id, history) in self.redefinition_histories.iter_mut() {
            if let Some(floor) = floor_of(class_id) {
                history.prune(floor);
            }
        }
    }

    /// A handle to this manager's hidden-class id bitmap ([`HiddenClassBits`]),
    /// for a reader that must not take the class-manager lock. The VM's
    /// `ClassRealm` takes one at construction.
    pub fn hidden_class_bits(&self) -> HiddenClassBits {
        self.hidden_classes.clone()
    }

    /// Mark loaded class `id` hidden after its definition (the native
    /// `set_class_hidden` door): the `Class::hidden` flag and the lock-free
    /// [`HiddenClassBits`] together, so no hidden class is missing from the
    /// bitmap. No-op for an id this store does not hold.
    ///
    /// Under `CRATONVM_HIDDEN_SELF_RECORDED_NAME` (JIT round 21 wave 22, lane
    /// hidden-resid, default ON) a renamed ordinary define recorded its
    /// class-file name on the class store
    /// ([`ClassStore::recorded_file_name`]); once the class is hidden the
    /// rule ([`ClassStore::hidden_self_name`]) reads that `this_class`, not
    /// the stored name's `/0x<hex>` suffix (`unsafe_natives_ext.rs` mints
    /// `anonymous/0x..`, whose suffix names no class file). A class defined
    /// under its own name keeps the suffix fallback. `=0`: nothing was
    /// recorded, so the flag and the bit only, as before.
    pub fn mark_class_hidden(&mut self, id: ClassId) {
        let Some(class) = self.get_class_mut(id) else {
            return;
        };
        class.hidden = true;
        self.hidden_classes.note(id);
        if let Some(file_name) = self.class_store.recorded_file_name(id) {
            self.hidden_classes.record_self_name(id, Some(file_name));
        }
    }

    /// `CRATONVM_HIDDEN_SELF_RECORDED_NAME` for this VM (latched on the class
    /// store; JIT round 21 wave 22, lane hidden-resid): whether the self-name
    /// doors ask the recorded class-file name ([`ClassStore::hidden_self_name`])
    /// rather than the pure stored-suffix predicate.
    #[inline]
    pub fn hidden_self_recorded_name(&self) -> bool {
        self.class_store.recorded_self_name()
    }

    /// Pin [`Self::hidden_self_recorded_name`] for this VM (tests, embedders).
    pub fn set_hidden_self_recorded_name(&mut self, on: bool) {
        self.class_store.set_recorded_self_name(on);
    }

    /// The VM's handle on this manager's [`ClassInitBits`] (taken once at
    /// construction, like [`Self::hidden_class_bits`]).
    pub fn class_init_bits(&self) -> ClassInitBits {
        self.class_init_bits.clone()
    }

    /// The VM's handle on this manager's [`RedefinitionLog`] (taken once at
    /// construction, like [`Self::class_init_bits`]; nio21 wave 3 L1).
    pub fn redefinition_log(&self) -> Arc<RedefinitionLog> {
        Arc::clone(&self.redefinition_log)
    }

    /// Advance the process redefinition clock for a redefinition of
    /// `class_id` in THIS VM, recording the class in both the process ring
    /// and this VM's [`RedefinitionLog`]. Every advance a VM's redefinition
    /// makes goes through here (the two in [`Self::redefine_class`], the VM's
    /// own after its JIT flush); the free [`bump_class_redefinition_count_of`]
    /// records in the process ring only.
    pub fn bump_redefinition_count_of(&self, class_id: ClassId) {
        record_redefinition(class_id.as_u32(), Some(&*self.redefinition_log));
    }

    /// WP2.4-B — return a shared [`Arc<AtomicU32>`] handle to the
    /// generation counter for `class_id`. Callers (JIT cache, invoke
    /// cache) hold the handle and re-check on each cache hit; the
    /// counter outlives the [`ClassManager`] borrow because it's
    /// reference-counted.
    ///
    /// Lazily allocates the counter on first call. Calling this on a
    /// class that has never been redefined still returns a valid
    /// handle whose value is 0; the next `redefine_class` will bump
    /// it without re-allocating.
    ///
    /// WP2.4-F1 — takes `&self` (was `&mut self`) so the per-thread
    /// invoke-cache populate path can acquire a handle while only
    /// holding `class_manager.read()`.  The internal lock guards lazy
    /// insertion; the returned `Arc` outlives the lock guard.
    pub fn class_redefine_generation_handle(&self, class_id: ClassId) -> Arc<AtomicU32> {
        // Fast path: read-lock and clone if already present.  The hot
        // path (re-populate after a stale eviction) almost always hits
        // here because the counter was inserted on the very first
        // populate or by `redefine_class` itself.
        if let Some(existing) = self
            .redefine_generations
            .read()
            .expect("redefine_generations poisoned")
            .get(&class_id)
        {
            return Arc::clone(existing);
        }
        // Slow path: upgrade to write-lock and double-check before
        // inserting (a sibling thread may have raced us to insert).
        let mut guard = self
            .redefine_generations
            .write()
            .expect("redefine_generations poisoned");
        Arc::clone(
            guard
                .entry(class_id)
                .or_insert_with(|| Arc::new(AtomicU32::new(0))),
        )
    }

    /// Round 5 audit fix (HIGH): obtain the `Arc<AtomicU8>` cache of
    /// `class_id`'s initialization state for the warm-path fast check.
    ///
    /// Callers hold onto the returned `Arc` and consult it via a single
    /// atomic load on every entry to a class-init checkpoint
    /// (interpreter dispatch, JIT entry, reflection access). When the
    /// load returns [`CLASS_INIT_INITIALIZED`] the call can return
    /// immediately without acquiring the class manager `RwLock` — the
    /// previous code path took a `read()` lock + did a `get_class` +
    /// matched on the full `ClassState` enum on every single dispatch.
    ///
    /// Entries are created lazily on first call. Initial state is
    /// [`CLASS_INIT_UNINITIALIZED`]; the slow init path moves it to
    /// [`CLASS_INIT_INITIALIZED`] when init succeeds (via
    /// [`Self::set_class_init_state`]). [`CLASS_INIT_IN_PROGRESS`] is never
    /// stored: a claim is `Class::initializing_thread`, which a stub upgrade
    /// clears, and which the claim's finalize checks before it publishes
    /// (`vm_util::finalize_class_init`; interpreter round i2 wave 62, lane L5).
    pub fn class_init_state_handle(&self, class_id: ClassId) -> Arc<std::sync::atomic::AtomicU8> {
        // Round-9 classloading CRIT-1 fix (audit `round9-classloading-reader.md`):
        // probe the per-`Class` embedded `init_state` Arc FIRST — that path
        // takes no `init_states` lock at all and is the canonical home of
        // the AtomicU8 fast-path cache. The previous order probed the
        // side-table first, which made the embedded atomic dead code for
        // every real class (every class touched during init had a
        // side-table entry auto-populated by `set_class_init_state`, so
        // the side-table branch always won).
        //
        // The side-table is now SECONDARY: it backs only synthetic /
        // pre-registration classes for which no `Class` instance lives in
        // the `class_store`. Those are rare (a handful of early-boot
        // bootstrap stubs); the steady-state hot path skips the
        // `init_states` lock entirely.
        if let Some(class) = self.class_store.get(class_id) {
            return Arc::clone(&class.init_state);
        }
        // Fallback: synthetic class with no `Class` entry. Use the
        // side-table. Read-lock fast path; write-lock insert on miss.
        {
            let guard = self.init_states.read();
            if let Some(handle) = guard.get(&class_id) {
                return Arc::clone(handle);
            }
        }
        let mut guard = self.init_states.write();
        Arc::clone(guard.entry(class_id).or_insert_with(|| {
            Arc::new(std::sync::atomic::AtomicU8::new(CLASS_INIT_UNINITIALIZED))
        }))
    }

    /// Round 5 audit fix (HIGH): update the AtomicU8 init-state cache
    /// for `class_id`. Called by the slow path
    /// (`vm_util::ensure_class_initialized_shared`) immediately after
    /// the underlying `Class::state` transitions to
    /// [`ClassState::Initialized`] or
    /// [`ClassState::InitializationError`] (the latter is reported as
    /// UNINITIALIZED so the fast path falls through to the slow path,
    /// which reports the error). `Release` ordering pairs with
    /// `Acquire` reads on the fast path.
    ///
    /// Round-9 classloading CRIT-1 fix: write directly to the per-`Class`
    /// embedded atomic when the `Class` exists (the common case). Only
    /// fall back to the side-table for synthetic / pre-registration
    /// classes with no `Class` entry. This stops auto-populating the
    /// side-table for every real class — which was the bug that made
    /// the embedded `init_state: Arc<AtomicU8>` dead code on the hot
    /// `class_init_state_handle` fast path.
    pub fn set_class_init_state(&self, class_id: ClassId, state: u8) {
        if let Some(class) = self.class_store.get(class_id) {
            class
                .init_state
                .store(state, std::sync::atomic::Ordering::Release);
            return;
        }
        // Synthetic / pre-registration class: route through the side-table.
        let handle = self.class_init_state_handle(class_id);
        handle.store(state, std::sync::atomic::Ordering::Release);
    }

    /// WP2.4-B — JEP 109 + JVMTI `RedefineClasses` semantics. Replace
    /// the bytecode of `class_id` with `new_bytes`, leaving the class
    /// identity, vtable layout, field set, and existing instances
    /// untouched.
    ///
    /// # Constraints (must reject otherwise)
    ///
    /// 1. `new_bytes` parses as a valid class file and the parsed
    ///    `this_class` MUST match the existing class's name (the new
    ///    bytes can't be from a different class).
    /// 2. The new class's superclass name MUST match the existing
    ///    superclass name. Same for direct interfaces (counts AND
    ///    names AND order).
    /// 3. Field declarations MUST be identical as a set: same count,
    ///    identical name + descriptor + access flags (in any order;
    ///    Step 4c).
    /// 4. Method declarations MUST be identical as a set: same count,
    ///    identical name + descriptor + access flags, in any order
    ///    (Step 4d; the bodies are swapped by name + descriptor). Bodies
    ///    (Code attributes) and annotations may differ.
    ///
    /// `RedefineOptions::skip_structural_check = true` skips items
    /// 2-4 above (name match is always enforced).
    ///
    /// # On success
    ///
    /// * Each method's `Code` and `RuntimeVisibleAnnotations` (etc.)
    ///   attributes are replaced with the new ones. Non-`Code`
    ///   non-annotation attributes (e.g. `Exceptions`,
    ///   `MethodParameters`) are also replaced — they're metadata
    ///   that has no observable runtime effect on existing
    ///   in-flight frames.
    /// * The class's constant pool, bootstrap_methods, and
    ///   class-level annotations are replaced.
    /// * `class_bytes_cache` is updated to the new bytes.
    /// * The vtable layout is unchanged (same number of slots, same
    ///   order). Each slot's `dispatch` snapshot is rebuilt from the
    ///   new method bodies and re-installed via the same hooks
    ///   `define_class` uses.
    /// * The redefine generation counter for this class is bumped.
    /// * Every JIT-compiled body keyed on this class's id is evicted
    ///   via the installed [`JitInvalidateHook`].
    /// * The JVMTI `ClassFileLoadHook` event fires before parsing,
    ///   giving any registered agent the chance to substitute its
    ///   own transformed bytes.
    ///
    /// # On failure
    ///
    /// Returns [`LinkageError::UnsupportedClassRedefinitionError`]
    /// with a precise reason. The class state on the manager side is
    /// guaranteed to be unchanged on the failure path (the new bytes
    /// are validated end-to-end before any mutation begins).
    ///
    /// [`JitInvalidateHook`]: JitInvalidateHook
    pub fn redefine_class(
        &mut self,
        class_id: ClassId,
        new_bytes: Vec<u8>,
        options: RedefineOptions,
    ) -> Result<(), LinkageError> {
        self.redefine_class_typed(class_id, new_bytes, options)
            .map_err(RedefinitionRefused::into_linkage_error)
    }

    /// The `InnerClasses` and `EnclosingMethod` tables a redefinition of
    /// `class_id` installs, or `None` to keep the class's own
    /// (`CRATONVM_REDEFINE_SWAPS_INNER_CLASSES` off, or a hidden class). JIT
    /// round 21 wave 17, lane innerclasses; page
    /// `r21-review-w16-findings-FIXED-20261020.md` item 5.
    ///
    /// HotSpot's `VM_RedefineClasses::redefine_single_class` swaps the scratch
    /// class's `inner_classes` array into the redefined class, and that array
    /// carries the `EnclosingMethod` indices too. A changed `InnerClasses` is
    /// never refused: `compare_and_normalize_class_versions` checks only
    /// `NestHost`, `NestMembers`, `Record` and `PermittedSubclasses`
    /// ([`RedefinitionRefusal::ClassAttributeChanged`]'s text names exactly
    /// those four).
    ///
    /// The scratch class comes from `ClassFileParser`, so the define path's
    /// `InnerClasses` verdict applies under the same `_need_verify` scope
    /// (every loader but the boot loader). The two `ClassFormatError`s refuse
    /// the redefinition (`JVMTI_ERROR_INVALID_CLASS_FORMAT`), and an ignored
    /// table installs as empty. A hidden class is never redefinable on
    /// HotSpot; its trusted in-VM redefines keep their (renamed) table. Asked
    /// by [`Self::check_redefinition`] too, so a batch is refused before any
    /// class of it changes. Read per call: the path is cold (a JVMTI call).
    #[allow(clippy::type_complexity)]
    pub(in crate::class_manager) fn redefinition_inner_tables(
        &self,
        class_id: ClassId,
        class_name: &str,
        loader: ClassLoaderId,
        attributes: &[cratonvm_reader::attribute::LazyAttribute],
        cp: &ConstantPool,
        major: u16,
    ) -> Result<Option<(Vec<InnerClassEntry>, Option<EnclosingMethodInfo>)>, RedefinitionRefused>
    {
        if !true
            || self.class_store.get(class_id).is_some_and(|c| c.hidden)
        {
            return Ok(None);
        }
        use cratonvm_reader::attribute::{inner_classes_verdict, InnerClassesVerdict as V};
        if let Some(error) = inner_class_modifier_refusal(
            attributes,
            cp,
            major,
            class_name,
            loader != ClassLoaderId::Bootstrap,
            loader != ClassLoaderId::Bootstrap,
        ) {
            return Err(RedefinitionRefused::new(
                RedefinitionRefusal::InvalidClassFormat,
                class_name.to_string(),
                format!("{error}"),
            ));
        }
        let nesting = NestingAttributes::of(attributes, cp);
        let mut inner_classes = nesting.inner_classes;
        if loader != ClassLoaderId::Bootstrap {
            let verdict = attributes.iter().find_map(|a| match a.as_decoded() {
                Some(Attribute::InnerClasses(entries)) => Some(stored_inner_classes_verdict(entries, cp, major)),
                _ => None,
            });
            let refuse = |what: &str| {
                RedefinitionRefused::new(
                    RedefinitionRefusal::InvalidClassFormat,
                    class_name.to_string(),
                    format!("{what} in class file {class_name}"),
                )
            };
            match verdict {
                Some(V::BothOuterAndInner) => {
                    return Err(refuse("Class is both outer and inner class"));
                }
                Some(V::DuplicateEntry) => {
                    return Err(refuse("Duplicate entry in InnerClasses attribute"));
                }
                Some(V::Ignore) => inner_classes.clear(),
                Some(V::Keep) | None => {}
            }
        }
        // JIT r21 w25 merge review: the entries carry their flags as the
        // define path stores them (dev's zs s4/rp `hotspot_stored_inner_classes`);
        // HotSpot's scratch class is parsed by the same `ClassFileParser`.
        let inner_classes = hotspot_stored_inner_classes(inner_classes, major);
        Ok(Some((inner_classes, nesting.enclosing_method.map(|b| *b))))
    }

    /// Would [`Self::redefine_class`] accept `new_bytes` for `class_id`?
    /// Runs its checks — the header, the parse, the name, the structural
    /// comparison and the verification of the new bodies (Steps 0, 2-4 and
    /// 5b) — and changes nothing. The JVMTI `ClassFileLoadHook` (Step 1) is
    /// not among them: a native agent may substitute other bytes, so a caller
    /// that checks a batch first must still handle a refusal from the
    /// redefinition itself.
    ///
    /// JVMTI `RedefineClasses` checks every class of a call before it changes
    /// any (`VM_RedefineClasses::load_new_class_versions`), so a refusal of
    /// the last class leaves the first ones as they were; the
    /// `java.lang.instrument` natives use this for that (interpreter round i1
    /// wave 27, lane L3; verification since wave 28, `L3/L3W28VerifyBeforeInstall`).
    /// A check that verified leaves a note the redefinition of the same bytes
    /// takes instead of verifying again ([`Self::pre_verified_redefinitions`]).
    pub fn check_redefinition(
        &self,
        class_id: ClassId,
        new_bytes: &[u8],
        options: &RedefineOptions,
    ) -> Result<(), RedefinitionRefused> {
        Self::check_redefinition_header(class_id, new_bytes)?;
        let shape = self.redefinition_shape(class_id)?;
        let new_class_file = Self::parse_redefinition_bytes(&shape.name, new_bytes, self.preview_enabled)?;
        Self::check_redefinition_structure(&shape, &new_class_file, options)?;
        // JIT round 21 wave 17, lane innerclasses: the `InnerClasses` format
        // refusals the redefinition makes.
        self.redefinition_inner_tables(
            class_id,
            &shape.name,
            shape.loader,
            &new_class_file.attributes,
            &new_class_file.constant_pool,
            new_class_file.version.major,
        )?;
        self.verify_redefinition_ahead(class_id, &shape.name, new_class_file, new_bytes)
    }

}
