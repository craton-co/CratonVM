#[allow(unused_imports)]
use super::*;

impl Compiler {
    /// The inline `StringBuilder.toString()` body (JIT round 17 wave 1, lane
    /// strings, STR16-P1; `CRATONVM_JIT_SB_TOSTRING_INLINE`, default on).
    ///
    /// JDK 25 `StringBuilder.toString()` is `new String(this, null)`, whose
    /// LATIN1 arm is `value = Arrays.copyOfRange(asb.getValue(), 0,
    /// asb.length()); coder = LATIN1`. On entry RAX is the receiver, already
    /// null-checked and proven an exact `StringBuilder` by the region's
    /// class-id guard. The fast path builds exactly that answer:
    ///
    /// ```text
    ///   compact receiver ; coder == LATIN1 ; value != null ; value is a byte[]
    ///   ECX = count ; count <=u value.length ; count <=u max   (else decline)
    ///   String layout-replace guard                           (else decline)
    ///   R10 = thread (cached slot or TLS mirror; null -> decline)
    ///   R11 = align8(cursor)                 ; the byte[count]
    ///   R9  = R11 + DATA + align8(count)     ; the String
    ///   RAX = R9 + String total ; RAX >u end -> decline     ; one bump, both
    ///   byte[] header ; String header ; String body zeroed ; String.value = R11
    ///   copy count bytes value[0..count] -> R11 data (8 at a time, then bytes)
    ///   cursor = R9 + String total            ; commit LAST
    ///   RAX = R9
    /// ```
    ///
    /// Every decline precedes the first store, so a declined call leaves the
    /// TLAB exactly as it found it (the zero-elision invariant every other
    /// inline allocation relies on), and the ordinary call -- the JDK body
    /// under `--real-jdk` -- answers. The fast path makes no call, so it cannot
    /// collect, trap or deopt: it owes no safepoint map, and the header-before-
    /// commit argument of `emit_inline_tlab_new` covers both objects (the cursor
    /// store is the single publication point). The one reference store, `value`,
    /// writes a young pointer into a young, uncommitted String whose old value
    /// is the TLAB's zero.
    ///
    /// Round 17 wave 2, lane sbzgc: every backend, each with its own proof that
    /// the collector's barrier for that store is a no-op ([`SbToStringGc`]).
    /// Generational needs nothing at run time; ZGC checks the start-bit table's
    /// whole contract for BOTH objects before the first store and sets both
    /// start bits after the commit (the `new` / `newarray` sites' announce,
    /// call-free); G1 checks that the two objects share a region. Any failed
    /// check is one more decline edge, so no arm needs a helper call.
    ///
    /// Semantics kept: a FRESH String per call (never interned, never shared:
    /// `sb.toString() != sb.toString()`), `hash` 0 and `hashIsZero` false so
    /// `hashCode()` computes as for any new String; `count < value.length` copies
    /// only `count` bytes. An EMPTY builder declines to the call (round 17
    /// wave 4, lane sbempty): JDK 25's `toString()` opens with `if (length()
    /// == 0) return "";`, the literal, which a fresh String is not; under
    /// `CRATONVM_STRING_LITERAL_IDENTITY=0` it gets a fresh `byte[0]`, as
    /// `Arrays.copyOfRange` gives it.
    ///
    /// The UTF16 arm (round 17 wave 2, lane sbzgc,
    /// `CRATONVM_JIT_SB_TOSTRING_UTF16`, opt-in on top of the main switch):
    /// JDK 25's UTF16 arm compresses to LATIN1 when `maybeLatin1` is set and
    /// every char fits, and otherwise copies `length << 1` bytes with `coder =
    /// UTF16`. The builder layout carries no `maybeLatin1`, so the inline arm
    /// serves exactly the builders for which both JDK paths agree: it scans for
    /// one char above `0xFF` and, finding one, copies `2 * count` bytes into a
    /// fresh `byte[2 * count]` with `coder = UTF16`; an all-LATIN1 UTF16
    /// builder (including an empty one) declines, so the compression and the
    /// `-XX:-CompactStrings` answer stay the JDK's.
    ///
    /// Round 17 wave 6, lane sbzgc2 (SBZGC17-2): when the layout carries the
    /// `maybeLatin1` slot ([`crate::StringBuilderFieldLayout::with_maybe_latin1`])
    /// the arm reads it. An EXACT `false` (`--real-jdk`) skips the scan and
    /// copies as UTF16, the JDK's answer whatever the chars; a set flag with
    /// no char above `0xFF` is narrowed to a LATIN1 String inline under
    /// `CRATONVM_JIT_SB_TOSTRING_COMPRESS`, else declines. Under
    /// `CRATONVM_DBG_JITC` every path and counted decline bumps its
    /// [`crate::metrics::SB_TO_STRING_CENSUS`] row (SBZGC17-5).
    ///
    /// Registers: RAX, RCX, RDX, R8-R11 and flags; the region flushed the
    /// operand cache before the guard, and the decline edge re-reads the
    /// receiver from its slot.
    pub(in crate::x64) fn emit_sb_to_string_latin1_body(
        &mut self,
        b: &crate::StringBuilderFieldLayout,
        outer: &crate::StringFieldLayout,
        decline: &mut Vec<usize>,
    ) {
        let thread_source = self.jit_thread_slot_off != 0 || jit_thread_tls_disp() != 0;
        let gc_inputs = SbToStringGcInputs {
            zgc_registration: cratonvm_types::jit_tlab_registration_required(),
            zgc_start_bit_requested: super::objects::zgc_inline_start_bit_requested(),
            zgc_array_announce: crate::x64::zgc_inline_array_announce_enabled(),
            g1_table_live: g1_barrier_table_live(self.helpers.g1_barrier_addr),
            generational_bounds_live: region_bounds_are_live(self.helpers.region_bounds_addr),
        };
        let gc_name = sb_to_string_gc_name(&gc_inputs);
        let planned = sb_to_string_plan(outer).and_then(|p| {
            let gc = sb_to_string_gc_arm(gc_inputs)?;
            if !thread_source {
                Err("no thread source that is not a call")
            } else if self.deferred_alloc_blind_spill || self.deferred_alloc_shadow_push {
                Err("withheld allocation spill")
            } else if self.helpers.tlab_cursor_offset_in_thread
                == self.helpers.tlab_end_offset_in_thread
            {
                Err("no TLAB offsets")
            } else {
                Ok((p, gc))
            }
        });
        let (plan, gc) = match planned {
            Ok(pg) => pg,
            Err(why) => {
                if dbg_jitc_enabled() {
                    eprintln!(
                        "[cratonvm-jitc] sb-tostring-inline DECLINED {} gc={gc_name}: {why}",
                        self.method_label
                    );
                }
                decline.push(self.emit_jmp_rel32_patch());
                self.slot_mirror = None;
                return;
            }
        };
        let utf16_requested = crate::jit_sb_to_string_utf16_enabled();
        let utf16 = plan.utf16_coder.filter(|_| utf16_requested);
        // `maybeLatin1` (round 17 wave 6, lane sbzgc2, SBZGC17-2): the UTF16
        // arm reads it when the layout has its slot and it can use it --
        // `false` exact (`--real-jdk`) skips the scan, `true` with the
        // compress switch armed narrows an all-LATIN1 payload inline.
        let compress = utf16.is_some() && crate::jit_sb_to_string_compress_enabled();
        let skip_scan_on_false = b.maybe_latin1_false_is_exact;
        let maybe_latin1 = b
            .maybe_latin1_compact_offset
            .filter(|_| utf16.is_some() && (skip_scan_on_false || compress));
        // Run-time census (SBZGC17-5): only in code compiled while the
        // compile-time line below is armed; otherwise not one byte changes.
        let count = dbg_jitc_enabled();
        crate::metrics::note_sb_to_string(crate::metrics::SB_TS_SITES);
        if count {
            let utf16_word = match (utf16_requested, utf16.is_some()) {
                (false, _) => "off",
                (true, true) => "on",
                (true, false) => "refused(String.coder slot)",
            };
            let ml_word = match (b.maybe_latin1_compact_offset, maybe_latin1) {
                (None, _) => "no-slot",
                (Some(_), None) => "unused",
                (Some(_), Some(_)) if skip_scan_on_false => "exact",
                (Some(_), Some(_)) => "true-only",
            };
            eprintln!(
                "[cratonvm-jitc] sb-tostring-inline EMITTED {} gc={gc_name} zgc_start_bits={} g1_same_region={} utf16={utf16_word} maybe_latin1={ml_word} compress={compress} cid={} total={} value_off={} max={}",
                self.method_label,
                gc.zgc_start_bits,
                gc.g1_same_region,
                plan.string_class_id,
                plan.string_total,
                plan.value_off,
                plan.max_count
            );
        }
        let row = |r: usize| -> Option<&'static std::sync::atomic::AtomicU64> {
            if count {
                crate::metrics::SB_TO_STRING_CENSUS.get(r)
            } else {
                None
            }
        };
        if let Some(c) = row(crate::metrics::SB_TS_ENTERED) {
            // RAX (the receiver) survives; R8 is scratch until the tail.
            self.emit_lock_inc_counter_via_r8(c);
        }

        // ---- receiver screens (RAX = the builder) --------------------------
        self.emit_test_mem8_imm8(
            RAX,
            cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32, // Cast: small header constant
            cratonvm_types::GC_FLAG_COMPACT,
        );
        decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ legacy -> call
                                                       // ECX = coder.
        if b.coder_compact_is_byte {
            self.emit_movx_r64_mem_disp32(RCX, RAX, b.coder_compact_offset, 8, false);
        } else {
            self.emit_mov_r32_mem_disp32(RCX, RAX, b.coder_compact_offset);
        }
        // RDX = value; null -> call.
        self.emit_mov_r64_mem_disp32(RDX, RAX, b.value_compact_offset);
        self.emit_test_r64_r64(RDX);
        decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ
                                                       // CMP BYTE [RDX + kind], byte[] tag ; JNE -> call.
        self.emit_cmp_mem8_imm8(RDX, i32::from(plan.kind_d8), plan.byte_array_tag);
        decline.push(self.emit_jcc_rel32_patch(0x85));
        // LATIN1 (0) here; UTF16 to its own arm when armed, else -> call.
        self.buf.emit(&[0x85, 0xC9]); // TEST ECX,ECX
        let to_utf16 = if utf16.is_some() {
            Some(self.emit_jcc_rel32_patch(0x85)) // JNZ utf16 arm
        } else {
            decline.push(self.emit_jcc_rel32_patch(0x85)); // JNZ UTF16 -> call
            None
        };
        // ECX = count (zero-extends RCX).
        self.emit_mov_r32_mem_disp32(RCX, RAX, b.count_compact_offset);
        // count == 0 -> call (round 17 wave 4, lane sbempty,
        // `CRATONVM_STRING_LITERAL_IDENTITY`, default ON): JDK 25 answers the
        // `""` LITERAL for an empty builder, which this body cannot name (a
        // movable heap object, no baked address), so the call answers it --
        // the JDK body, or the native that now returns the literal too. The
        // UTF16 arm already declines an empty builder (its scan finds no char
        // above 0xFF).
        //
        // JIT round 19 proposal wave 5, lane concat5 (STRGAP4-2,
        // `CRATONVM_JIT_STRING_LITERAL_SLOTS`, default OFF): with this VM's
        // rooted `""` literal slot the empty builder is answered inline
        // instead (the arm after the LATIN1 tail below); without one, or with
        // the switch off, this is byte for byte the decline.
        let mut empty_arm: Option<(usize, usize)> = None;
        if empty_builder_declines_to_call() {
            self.buf.emit(&[0x85, 0xC9]); // TEST ECX,ECX
            let empty_slot = if super::op_concat::string_literal_slots_enabled() {
                self.direct_helpers.string_literal_slot(true)
            } else {
                None
            };
            let jz = self.emit_jcc_rel32_patch(0x84); // JZ -> call (or the "" arm)
            match empty_slot {
                Some(slot) => empty_arm = Some((jz, slot)),
                None => decline.push(jz),
            }
        }
        // CMP ECX,[RDX+len] ; JA -> call: unsigned, so a negative count is out.
        self.buf.emit(&[0x3B, 0x4A, plan.len_d8]);
        decline.push(self.emit_jcc_rel32_patch(0x87));
        // CMP ECX,max_count ; JA -> call.
        self.buf.emit(&[0x81, 0xF9]);
        self.buf.emit(&plan.max_count.to_le_bytes());
        decline.push(self.emit_jcc_rel32_patch(0x87));
        let latin1_arm = SbToStringArm {
            utf16_coder: None,
            narrow: false,
            taken_row: crate::metrics::SB_TS_TAKEN_LATIN1,
        };
        self.emit_sb_to_string_tail(&plan, gc, latin1_arm, count, decline, None);
        if let Some((jz, slot)) = empty_arm {
            // ---- "" arm (STRGAP4-2): RAX = the interned "" literal, read raw
            // from the VM's GC-maintained slot as an `ldc` slot is read; a
            // word still 0 (never, once published) declines. Joins the LATIN1
            // tail's fall-through with RAX = the String.
            let over = self.emit_jmp_rel32_patch();
            self.patch_rel32_to_here(jz);
            // Cast: the VM's slot address, baked.
            self.emit_mov_imm64_full(RAX, slot as i64);
            self.buf.emit(&[0x48, 0x8B, 0x00]); // MOV RAX,[RAX]
            self.emit_test_r64_r64(RAX);
            decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ -> call
            self.patch_rel32_to_here(over);
        }

        if let (Some(to_utf16), Some(coder)) = (to_utf16, utf16) {
            let mut done = vec![self.emit_jmp_rel32_patch()];
            // ---- UTF16 arm: ECX = coder, RDX = value (a byte[]) -------------
            self.patch_rel32_to_here(to_utf16);
            self.buf.emit(&[0x83, 0xF9, 0x01]); // CMP ECX,1 (UTF16 exactly)
            decline.push(self.emit_jcc_rel32_patch(0x85)); // JNE -> call
                                                           // ECX = count ; count <=u max/2 (so 2*count <=u max) ; JA -> call.
            self.emit_mov_r32_mem_disp32(RCX, RAX, b.count_compact_offset);
            if maybe_latin1.is_some() {
                // count == 0 -> call, explicitly: the scan below used to
                // decline an empty builder by finding no char, and neither
                // the skipped scan nor the compress may answer one (JDK 25
                // returns the `""` literal).
                self.buf.emit(&[0x85, 0xC9]); // TEST ECX,ECX
                decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ -> call
            }
            self.buf.emit(&[0x81, 0xF9]);
            self.buf.emit(&(plan.max_count / 2).to_le_bytes());
            decline.push(self.emit_jcc_rel32_patch(0x87));
            // R8D = 2*count ; CMP R8D,[RDX+len] ; JA -> call.
            self.buf.emit(&[0x44, 0x8D, 0x04, 0x09]); // LEA R8D,[RCX+RCX]
            self.buf.emit(&[0x44, 0x3B, 0x42, plan.len_d8]); // CMP R8D,[RDX+len]
            decline.push(self.emit_jcc_rel32_patch(0x87));
            // R9D = maybeLatin1 (RAX is still the compact receiver). JDK 25's
            // `String(AbstractStringBuilder, Void)`: `false` -> copy as UTF16
            // whatever the chars are; `true` -> compress when every char fits.
            let mut skip_scan = None;
            if let Some(ml_off) = maybe_latin1 {
                self.emit_movx_r64_mem_disp32(R9, RAX, ml_off, 8, false); // MOVZX R9D,BYTE
                if skip_scan_on_false {
                    self.buf.emit(&[0x45, 0x85, 0xC9]); // TEST R9D,R9D
                    skip_scan = Some(self.emit_jcc_rel32_patch(0x84)); // JZ: no scan
                }
            }
            // Scan for a char above 0xFF: its HIGH byte (odd index, x86 is
            // little-endian, as `StringUTF16`'s byte order is native) != 0.
            // None found -> the all-LATIN1 block below (the call, unless the
            // compress is armed and `maybeLatin1` is set).
            let all_latin1_inline = compress && maybe_latin1.is_some();
            self.buf.emit(&[0x31, 0xC0]); // XOR EAX,EAX (char index)
            let scan_top = self.buf.pos();
            self.buf.emit(&[0x39, 0xC8]); // CMP EAX,ECX
            let scan_end = self.emit_jcc_rel32_patch(0x83); // JAE: all LATIN1
                                                            // CMP BYTE [RDX+RAX*2+DATA+1],0 (the plan screened DATA+1 <= 127).
            self.buf.emit(&[0x80, 0x7C, 0x42, plan.data_d8 + 1, 0x00]);
            let found = self.emit_jcc_rel32_patch(0x85); // JNE found
            self.buf.emit(&[0xFF, 0xC0]); // INC EAX
            let back = self.emit_jmp_rel32_patch();
            let rel = scan_top as i32 - (back as i32 + 4); // Cast: rel32 displacement
            self.buf.try_patch_i32(back, rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
            let all_latin1_row = row(crate::metrics::SB_TS_UTF16_ALL_LATIN1);
            if !all_latin1_inline && all_latin1_row.is_none() {
                // The wave-2 shape, byte for byte: the scan's end IS a decline.
                decline.push(scan_end);
            } else {
                self.patch_rel32_to_here(scan_end);
                if all_latin1_inline {
                    // `maybeLatin1 == false` -> call (counted): the JDK copies
                    // such a builder as UTF16, and only an exact `false`
                    // (handled above, before the scan) may answer that inline.
                    self.buf.emit(&[0x45, 0x85, 0xC9]); // TEST R9D,R9D
                    self.emit_decline_jcc_counted(0x84, all_latin1_row, decline); // JZ
                                                                                  // ---- compress: ECX = count chars of RDX -> byte[count] --
                    let compressed = SbToStringArm {
                        utf16_coder: None,
                        narrow: true,
                        taken_row: crate::metrics::SB_TS_TAKEN_COMPRESSED,
                    };
                    self.emit_sb_to_string_tail(&plan, gc, compressed, count, decline, None);
                    done.push(self.emit_jmp_rel32_patch());
                } else if let Some(c) = all_latin1_row {
                    self.emit_lock_inc_counter_via_r8(c);
                    decline.push(self.emit_jmp_rel32_patch());
                }
            }
            // `maybeLatin1 == false`, exact: straight to the UTF16 copy.
            if let Some(skip) = skip_scan {
                self.patch_rel32_to_here(skip);
                if let Some(c) = row(crate::metrics::SB_TS_UTF16_SCAN_SKIPPED) {
                    self.emit_lock_inc_counter_via_r8(c);
                }
            }
            self.patch_rel32_to_here(found);
            // ECX = 2*count, the byte count (ECX still holds count on both
            // edges into here; R8 may be a counter address).
            self.buf.emit(&[0x01, 0xC9]); // ADD ECX,ECX
            let utf16_arm = SbToStringArm {
                utf16_coder: Some(coder),
                narrow: false,
                taken_row: crate::metrics::SB_TS_TAKEN_UTF16,
            };
            self.emit_sb_to_string_tail(&plan, gc, utf16_arm, count, decline, None);
            for d in done {
                self.patch_rel32_to_here(d);
            }
        }
        self.slot_mirror = None;
    }

    /// The allocation half of [`Self::emit_sb_to_string_latin1_body`], shared
    /// by its LATIN1 and UTF16 arms: `byte[ECX]` holding the first ECX bytes of
    /// the source array in RDX, plus the `String` over it, in ONE bump; every
    /// refusal before the first store pushes onto `decline`. `arm.utf16_coder`
    /// stores `coder = UTF16` (the LATIN1 arm leaves the zeroed `0`);
    /// `arm.narrow` (round 17 wave 6, lane sbzgc2) copies the low byte of each
    /// of ECX UTF16 chars instead of ECX bytes. `count` (`CRATONVM_DBG_JITC`)
    /// counts the TLAB-full and collector declines and `arm.taken_row` at run
    /// time; off, the tail is byte for byte its uncounted form. Leaves RAX =
    /// the String.
    ///
    /// `fill` (JIT round 19 proposal wave 4, lane strgap4, STRGAP19-3; `None`
    /// at every `toString()` site, which keeps their bytes unchanged) replaces
    /// the copy from RDX: it is emitted after both headers and `String.value`
    /// are stored, with R11 = the `byte[]` and R9 = the String, may clobber
    /// RAX, RCX, RDX, R8 and R10, and must write exactly the ECX bytes the
    /// array was sized for. R10 (the thread) is re-derived after it.
    pub(in crate::x64) fn emit_sb_to_string_tail(
        &mut self,
        plan: &SbToStringPlan,
        gc: SbToStringGc,
        arm: SbToStringArm,
        count: bool,
        decline: &mut Vec<usize>,
        fill: Option<&dyn Fn(&mut Compiler)>,
    ) {
        use crate::metrics as m;
        let row = |r: usize| -> Option<&'static std::sync::atomic::AtomicU64> {
            if count {
                m::SB_TO_STRING_CENSUS.get(r)
            } else {
                None
            }
        };
        let utf16_coder = arm.utf16_coder;
        // Cast: encoding immediates/displacements of small header constants.
        let cursor_off = self.helpers.tlab_cursor_offset_in_thread as i32;
        let end_off = self.helpers.tlab_end_offset_in_thread as i32;
        let class_id_off = self.helpers.class_id_offset_in_obj as i32;

        // ---- String layout-replace guard (RAX, R11 scratch) ----------------
        self.emit_mov_imm64_full(R11, plan.replace_guard.0 as i64); // Cast: baked address
        self.emit_mov_r32_mem_disp32(RAX, R11, 0);
        self.buf.emit_byte(0x3D); // CMP EAX, imm32
        self.buf.emit(&(plan.replace_guard.1 as i32).to_le_bytes()); // Cast: counter bits
        decline.push(self.emit_jcc_rel32_patch(0x85)); // JNE

        // ---- thread, then ONE bump for both objects ------------------------
        if self.jit_thread_slot_off != 0 {
            self.emit_load_local(RAX, self.jit_thread_slot_off);
        } else {
            // Cast: the mirror's TLS displacement, a small positive offset.
            self.emit_mov_rax_tls_disp32(jit_thread_tls_disp() as u32);
        }
        self.emit_mov_r64_r64(R10, RAX);
        self.emit_test_r64_r64(R10);
        decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ no thread -> call
                                                       // R11 = align8(cursor): the byte[].
        self.emit_mov_r64_mem_disp32(R11, R10, cursor_off);
        self.emit_add_r64_imm8(R11, 7);
        self.emit_and_r64_imm8(R11, -8);
        // R8 = align8(count).
        self.buf.emit(&[0x41, 0x89, 0xC8]); // MOV R8D,ECX
        self.emit_add_r64_imm8(R8, 7);
        self.emit_and_r64_imm8(R8, -8);
        // LEA R9,[R11+R8+DATA]: the String.
        self.buf.emit(&[0x4F, 0x8D, 0x4C, 0x03, plan.data_d8]);
        // RAX = R9 + total: the new cursor. CMP RAX,[R10+end] ; JA -> call.
        self.emit_lea_r64_mem_disp32(RAX, R9, plan.string_total);
        self.emit_cmp_r64_mem_disp32(RAX, R10, end_off);
        // JA -> call. A counted edge clobbers R8 (align8(count)) on the way
        // out only; the fall-through keeps it, though nothing below reads it.
        self.emit_decline_jcc_counted(0x87, row(m::SB_TS_TLAB_FULL), decline);

        // ---- per-collector pre-checks (RAX, R8 scratch; RAX re-derived at
        // the commit). Live: ECX, RDX, R9, R10, R11. ------------------------
        if gc.g1_same_region {
            self.emit_sb_g1_same_region_check(decline, row(m::SB_TS_G1_CROSS_REGION));
        }
        // zr z9/bo: one read of the start-bit form for the pre-check and
        // both stores (`CRATONVM_ZGC_JIT_SHORT_ANNOUNCE`).
        let short_bits = gc.zgc_start_bits && super::objects::zgc_jit_short_announce_enabled();
        if gc.zgc_start_bits {
            // Every condition of the start-bit table's contract, for both
            // objects, before anything is written; the bits follow the commit.
            self.emit_zgc_start_bit_precheck_form(
                cursor_off,
                end_off,
                &[R11, R9],
                decline,
                count,
                short_bits,
            );
        }
        // ---- no decline past this point: every store below is committed ----

        // byte[] header: class id 0, length, mark word, "no hash yet".
        self.emit_mov_dword_mem_disp32_imm32(R11, 0, 0);
        self.buf.emit(&[0x41, 0x89, 0x4B, plan.len_d8]); // MOV [R11+len],ECX
        self.emit_mov_dword_mem_disp32_imm32(
            R11,
            cratonvm_types::MARK_WORD_OFFSET as i32, // Cast: small header constant
            plan.byte_array_mark,
        );
        self.emit_mov_dword_mem_disp32_imm32(
            R11,
            cratonvm_types::IDENTITY_HASH_OFFSET as i32, // Cast: small header constant
            0,
        );
        // String header: class id and the compact mark word (NEUTRAL,
        // kind=Object, GC_FLAG_HEADER | GC_FLAG_COMPACT), as the inline `new`.
        self.emit_mov_dword_mem_disp32_imm32(R9, class_id_off, plan.string_class_id as i32); // Cast: ClassId fits in 32 bits
        let flags = cratonvm_types::GC_FLAG_COMPACT | cratonvm_types::GC_FLAG_HEADER;
        self.emit_mov_dword_mem_disp32_imm32(
            R9,
            cratonvm_types::MARK_WORD_OFFSET as i32, // Cast: small header constant
            i32::from(flags) << 24,
        );
        // String body: `coder` LATIN1 (0), `hash` 0, `hashIsZero` false, and
        // padding -- written, not assumed, as the mark word is.
        // Through `try_from`, not a cast: the header-offset inventory
        // (`header_offset_emission_site_inventory_matches_the_doc`) counts the
        // cast form. 8 today; an impossible failure zeroes nothing.
        let mut off =
            i32::try_from(cratonvm_types::COMPACT_HEADER_SIZE).unwrap_or(plan.string_total);
        while off < plan.string_total {
            self.emit_mov_dword_mem_disp32_imm32(R9, off, 0);
            off += 4;
        }
        // The UTF16 arm's `coder = UTF16` (1), over the zero just written.
        if let Some((coder_off, is_byte)) = utf16_coder {
            if is_byte {
                // MOV BYTE [R9+disp32],1: REX.B C6 /0, modrm 10 000 001.
                self.buf.emit(&[0x41, 0xC6, 0x81]);
                self.buf.emit(&coder_off.to_le_bytes());
                self.buf.emit_byte(1);
            } else {
                self.emit_mov_dword_mem_disp32_imm32(R9, coder_off, 1);
            }
        }
        // String.value = the byte[].
        self.emit_mov_mem_disp32_r64(R9, R11, plan.value_off);

        let d = plan.data_d8;
        if let Some(fill) = fill {
            fill(self);
            // The fill may use R10 as scratch: the thread again, for the
            // commit (proven non-null above; no decline past the stores).
            if self.jit_thread_slot_off != 0 {
                self.emit_load_local(RAX, self.jit_thread_slot_off);
            } else {
                // Cast: the mirror's TLS displacement, a small positive offset.
                self.emit_mov_rax_tls_disp32(jit_thread_tls_disp() as u32);
            }
            self.emit_mov_r64_r64(R10, RAX);
        } else if arm.narrow {
            // ---- compress: dst[i] = low byte of UTF16 char i, i < ECX ------
            // (round 17 wave 6, lane sbzgc2). The caller's scan proved every
            // high byte zero, so this is `StringUTF16.compress`'s answer; the
            // source reads stop at byte 2*count-2 < 2*count <=u value.length.
            self.buf.emit(&[0x45, 0x31, 0xC0]); // XOR R8D,R8D (i = 0)
            let narrow_top = self.buf.pos();
            self.buf.emit(&[0x41, 0x39, 0xC8]); // CMP R8D,ECX
            let narrowed = self.emit_jcc_rel32_patch(0x83); // JAE done
                                                            // MOVZX EAX,BYTE [RDX+R8*2+DATA]: REX.X 0F B6 /r, SIB scale 2.
            self.buf.emit(&[0x42, 0x0F, 0xB6, 0x44, 0x42, d]);
            self.buf.emit(&[0x43, 0x88, 0x44, 0x03, d]); // MOV [R11+R8+DATA],AL
            self.buf.emit(&[0x49, 0xFF, 0xC0]); // INC R8
            let back = self.emit_jmp_rel32_patch();
            let rel = narrow_top as i32 - (back as i32 + 4); // Cast: rel32 displacement
            self.buf.try_patch_i32(back, rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
            self.patch_rel32_to_here(narrowed);
        } else {
            self.emit_sb_to_string_byte_copy(d);
        }

        // ---- commit LAST, then answer the String ---------------------------
        // nio21 wave 10 L1 (`CRATONVM_ZGC_JIT_START_BIT_BEFORE_COMMIT`, default
        // ON): under ZGC both start bits are set BEFORE the commit (both
        // objects fully written; no poll since the pre-check), so a peer
        // frozen here owns two objects that are inside its published tail or
        // registered, never neither. The stores clobber RAX, so the new
        // cursor is derived after them. `=0`: commit, then the bits.
        let bits_first = gc.zgc_start_bits && super::objects::zgc_start_bit_before_commit_enabled();
        if bits_first {
            self.emit_zgc_start_bit_store_prechecked_form(cursor_off, R11, short_bits);
            self.emit_zgc_start_bit_store_prechecked_form(cursor_off, R9, short_bits);
        }
        self.emit_lea_r64_mem_disp32(RAX, R9, plan.string_total);
        self.emit_mov_mem_disp32_r64(R10, RAX, cursor_off);
        if gc.zgc_start_bits && !bits_first {
            // Announce both, as `emit_inline_tlab_newarray` / `_new` announce
            // one: after the commit, with no poll since the pre-check. The
            // copy's ECX / RDX are dead here; R9 / R10 / R11 survive.
            self.emit_zgc_start_bit_store_prechecked_form(cursor_off, R11, short_bits);
            self.emit_zgc_start_bit_store_prechecked_form(cursor_off, R9, short_bits);
        }
        if let Some(c) = row(arm.taken_row) {
            // R8 (the copy index, or the start-bit word index) is dead.
            self.emit_lock_inc_counter_via_r8(c);
        }
        self.emit_mov_r64_r64(RAX, R9);
        self.slot_mirror = None;
    }

    /// The byte copy of [`Self::emit_sb_to_string_tail`]: ECX bytes from the
    /// `byte[]` in RDX to the one in R11, 8 at a time, then bytes (`d` =
    /// `ARRAY_DATA_OFFSET` as a disp8). Clobbers RAX, R8 and the flags.
    pub(in crate::x64::op_invoke) fn emit_sb_to_string_byte_copy(&mut self, d: u8) {
        // ---- copy value[0..count) -> byte[] data ---------------------------
        self.buf.emit(&[0x45, 0x31, 0xC0]); // XOR R8D,R8D (i = 0)
        let qword_top = self.buf.pos();
        self.buf.emit(&[0x41, 0x8D, 0x40, 0x08]); // LEA EAX,[R8+8]
        self.buf.emit(&[0x39, 0xC8]); // CMP EAX,ECX
        let to_tail = self.emit_jcc_rel32_patch(0x87); // JA tail (i + 8 > count)
        self.buf.emit(&[0x4A, 0x8B, 0x44, 0x02, d]); // MOV RAX,[RDX+R8+DATA]
        self.buf.emit(&[0x4B, 0x89, 0x44, 0x03, d]); // MOV [R11+R8+DATA],RAX
        self.buf.emit(&[0x49, 0x83, 0xC0, 0x08]); // ADD R8,8
        let back = self.emit_jmp_rel32_patch();
        let rel = qword_top as i32 - (back as i32 + 4); // Cast: rel32 displacement
        self.buf.try_patch_i32(back, rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
        self.patch_rel32_to_here(to_tail);
        let byte_top = self.buf.pos();
        self.buf.emit(&[0x41, 0x39, 0xC8]); // CMP R8D,ECX
        let copied = self.emit_jcc_rel32_patch(0x83); // JAE done
        self.buf.emit(&[0x42, 0x0F, 0xB6, 0x44, 0x02, d]); // MOVZX EAX,BYTE [RDX+R8+DATA]
        self.buf.emit(&[0x43, 0x88, 0x44, 0x03, d]); // MOV [R11+R8+DATA],AL
        self.buf.emit(&[0x49, 0xFF, 0xC0]); // INC R8
        let back = self.emit_jmp_rel32_patch();
        let rel = byte_top as i32 - (back as i32 + 4); // Cast: rel32 displacement
        self.buf.try_patch_i32(back, rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
        self.patch_rel32_to_here(copied);
    }

    /// The G1 pre-check of the inline `toString()` (round 17 wave 2, lane
    /// sbzgc): `((R11 - base) ^ (R9 - base)) & region_mask == 0`, else a
    /// decline, against the published `JIT_G1_BARRIER` table -- the same
    /// arena-relative same-region test `emit_g1_barrier_filter` makes and
    /// `G1Collector::post_write_barrier_rset` opens with, so a passing pair
    /// (R9 = the String, R11 = the `byte[]` its `value` will name) is one whose
    /// store that barrier would have dismissed. An address outside the arena
    /// can only make the two differ (a wasted decline), never agree wrongly.
    /// Clobbers RAX, R8 and the flags; reads R9 and R11.
    ///
    /// Round 17 wave 6, lane sbzgc2 (SBZGC17-5): the decline is counted into
    /// `counter` when `Some`; `None` is byte for byte the uncounted check.
    pub(in crate::x64) fn emit_sb_g1_same_region_check(
        &mut self,
        decline: &mut Vec<usize>,
        counter: Option<&'static std::sync::atomic::AtomicU64>,
    ) {
        // Cast: the 'static `JIT_G1_BARRIER` table's address, baked.
        let table = self.helpers.g1_barrier_addr as i64;
        self.emit_mov_imm64_full(RAX, table);
        self.emit_mov_r64_mem_disp32(RAX, RAX, Self::G1B_ARENA_BASE);
        self.emit_mov_r64_r64(R8, R11);
        self.emit_sub_r64_r64(R8, RAX); // R8 = A - base
        self.buf.emit(&[0x48, 0xF7, 0xD8]); // NEG RAX
        self.buf.emit(&[0x4C, 0x01, 0xC8]); // ADD RAX,R9: RAX = S - base
        self.emit_xor_r64_r64(RAX, R8);
        self.emit_mov_imm64_full(R8, table);
        self.emit_and_r64_mem_disp32(RAX, R8, Self::G1B_REGION_MASK);
        self.emit_decline_jcc_counted(0x85, counter, decline); // JNZ -> call
    }
}

