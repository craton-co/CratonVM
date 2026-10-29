// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 5 (lane UNROLLRES; item 3 of
//! `docs/known-issues/jit/j27-loopcg-counted-loop-poll-and-residency-proposal-20261028.md`):
//! the page form of the IR back-edge poll, `CRATONVM_JIT_IR_PAGE_POLL=1`
//! (opt-in).
//!
//! # Shape
//!
//! The single-pass page form (`x64::safepoint::emit_page_form_poll`,
//! `crate::poll_page`), ported to `Lowerer::emit_safepoint_poll`:
//!
//! ```text
//!   MOV  R11, <seg>:[cell_disp32]     ; this OS thread's poll cell
//! site:
//!   TEST [R11 + pair_base], EAX        ; faults on the stop page during a
//!                                      ; pause, on the armed page while armed
//! ```
//!
//! Outlined (the default for back edges, `CRATONVM_JIT_IR_BACK_EDGE_POLL_OUTLINE`),
//! that is the whole hot path: no flag test, no `Jcc`. The fault's recovery pc
//! is the outlined slow block (`emit_outlined_polls`), which ends in its usual
//! `JMP` back to the instruction after the `TEST`. Inline, a `JMP rel32` over
//! the slow path takes the place of the byte poll's `JZ`, and the recovery pc
//! is the first byte of the slow path.
//!
//! # Why the slow path is unchanged
//!
//! The fault leaves every register but R11 (the poll's scratch in every form)
//! as it was before the poll. The `TEST` never completes, so the flags are left
//! as they were too, and nothing in the slow path reads them before writing
//! them. So the recovery pc runs exactly the code the byte poll's taken branch
//! reaches: the W6-B spills, the map, the slow-path call with the flag byte as
//! argument 0 (the helper decides what the visit was for, as it does for the
//! single-pass page form), the reloads.
//!
//! # What refuses it (the byte poll then, byte for byte)
//!
//! * A poll with a mode exit: the forced-exit patch (`exit_poll_sites`)
//!   rewrites a `Jcc` this form does not have.
//! * The stress countdown (`CRATONVM_DBG_JIT_DEOPT_A_LOT` /
//!   `CRATONVM_DBG_GC_AT_BACK_EDGE`): it jumps to the poll's `Jcc` with ZF
//!   clear.
//! * The owed-cycle latch (`CRATONVM_GEN_CONC_OWED_POLL`): a second byte test.
//! * The method-entry poll (not a loop poll; left as it is).
//! * No pair in disp32 reach, `CRATONVM_JIT_PAGE_POLL=0`, or no implicit-null
//!   table to register the site in (`poll_page::emit_enabled`).
//!
//! # Registration
//!
//! The `(site, slow)` offsets go into the implicit-null table when the body
//! becomes a `CompiledMethod` ([`register_page_poll_sites`]). A declined site
//! refuses the body, because an unregistered site would turn a pause into a
//! crash; the buffer's drop retires whatever was registered. This backend
//! never rewinds its buffer (see `lower_inner_attempt`), so no site can be
//! left at a stale offset.

use super::Lowerer;
use crate::CompiledMethod;

/// `CRATONVM_JIT_IR_PAGE_POLL=1` (perf, **opt-in**; j27 wave 5, lane
/// UNROLLRES): emit the page form of an IR back-edge poll. Opt-in because
/// it was written without a build, and its fault path cannot be exercised by
/// a unit test (no VM maps a pair there). Read per poll; no `static`.
pub(super) fn ir_page_poll_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_PAGE_POLL")
}

/// What a poll is, for the page form's admission.
#[derive(Clone, Copy, Debug)]
pub(super) struct PagePollAsk {
    pub(super) back_edge: bool,
    pub(super) mode_exit: bool,
    pub(super) stress: bool,
    pub(super) owed_latch: bool,
    pub(super) pair_base: usize,
}

/// May this poll take the page form, the switches aside? Pure.
pub(super) fn shape_admits(ask: PagePollAsk) -> bool {
    ask.back_edge
        && !ask.mode_exit
        && !ask.stress
        && !ask.owed_latch
        && crate::poll_page::base_fits_disp32(ask.pair_base)
}

/// Offset of the faulting `TEST` inside [`page_poll_bytes`].
pub(super) const SITE_OFFSET: usize = 9;

/// The page form's two instructions: `MOV R11, <seg>:[cell_disp]` (9 bytes),
/// then `TEST [R11 + pair_base], EAX` (7 bytes) at [`SITE_OFFSET`]. The same
/// encoding as `x64::safepoint::emit_page_form_poll`.
pub(super) fn page_poll_bytes(seg_prefix: u8, cell_disp: u32, pair_base: u32) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[0] = seg_prefix;
    out[1..5].copy_from_slice(&[0x4C, 0x8B, 0x1C, 0x25]); // MOV R11, <seg>:[disp32]
    out[5..9].copy_from_slice(&cell_disp.to_le_bytes());
    // TEST r/m32, r32 (0x85), REX.B (R11 base), ModRM mod=10 reg=EAX rm=R11.
    out[9..12].copy_from_slice(&[0x41, 0x85, 0x83]);
    out[12..16].copy_from_slice(&pair_base.to_le_bytes());
    out
}

/// The opcode the inline arm skips its slow path with: the byte poll's
/// `JZ rel32`, or for the page form (whose fault is the only way in) a
/// `JMP rel32`. Both are followed by the same rel32 field.
pub(super) fn inline_skip_opcode(page_form: bool) -> &'static [u8] {
    if page_form {
        &[0xE9]
    } else {
        &[0x0F, 0x84]
    }
}

/// Register every `(site, slow)` offset of `cm` in the implicit-null table.
/// `Err` names why the body must be refused.
pub(super) fn register_page_poll_sites(
    cm: &CompiledMethod,
    sites: &[(usize, usize)],
) -> Result<(), &'static str> {
    if sites.is_empty() {
        return Ok(());
    }
    let base = cm.code_bytes().as_ptr() as usize;
    if !crate::implicit_null::note_registration_base(cm.entry as usize, base) {
        return Err("ir-page-poll-entry-not-base");
    }
    for &(site, slow) in sites {
        if !crate::implicit_null::register(base + site, base + slow) {
            crate::implicit_null::unregister_range(base, cm.code_bytes().len());
            return Err("ir-page-poll-declined");
        }
    }
    Ok(())
}

impl Lowerer<'_> {
    /// Emit the page form's two instructions when this back-edge poll takes
    /// it; the site's buffer offset, or `None` (nothing emitted).
    pub(super) fn emit_page_poll_prefix(&mut self, back_edge: bool, mode_exit: bool) -> Option<usize> {
        let ask = PagePollAsk {
            back_edge,
            mode_exit,
            stress: self.poll_stress_period != 0,
            owed_latch: self.direct_helpers.gen_owed_poll_flag_addr != 0,
            pair_base: self.direct_helpers.page_poll_base,
        };
        if !ir_page_poll_enabled() || !shape_admits(ask) || !crate::poll_page::emit_enabled() {
            return None;
        }
        // Cast: the probe answers a 32-bit segment displacement, and
        // `base_fits_disp32` admitted the pair below 2 GiB.
        let bytes = page_poll_bytes(
            crate::x64::inline_rbp_tls_segment_prefix(),
            crate::poll_page::cell_tls_disp() as u32,
            ask.pair_base as u32,
        );
        let start = self.buf.pos();
        self.buf.emit(&bytes);
        Some(start + SITE_OFFSET)
    }

    /// The outlined arm's branch to its slow block: the byte poll's
    /// `JNZ rel32`, or nothing for the page form (its fault is the branch).
    /// Returns the key `outlined_polls` / `outlined_poll_spills` file it
    /// under: the `JNZ`'s rel32 field, or the page form's site.
    pub(super) fn emit_outlined_poll_branch(
        &mut self,
        page_site: Option<usize>,
        mode_exit: bool,
    ) -> usize {
        if let Some(site) = page_site {
            self.page_polls_outlined.push(site);
            return site;
        }
        self.buf.emit(&[0x0F, 0x85]); // JNZ .slow (outlined, after the body)
        let slow_patch = self.buf.pos();
        if mode_exit {
            self.note_exit_poll_site(slow_patch);
        }
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        slow_patch
    }

    /// `emit_outlined_polls`, at the start of the slow block filed under
    /// `slow_patch`: patch the byte poll's `JNZ` here, or record the page
    /// form's recovery pc.
    pub(super) fn bind_outlined_poll_slow(&mut self, slow_patch: usize) {
        let here = self.buf.pos();
        if let Some(i) = self.page_polls_outlined.iter().position(|&s| s == slow_patch) {
            self.page_polls_outlined.swap_remove(i);
            self.page_poll_sites.push((slow_patch, here));
            return;
        }
        let rel = here as i32 - (slow_patch as i32 + 4);
        Self::patch_or_bail(&mut self.buf, slow_patch, rel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask() -> PagePollAsk {
        PagePollAsk {
            back_edge: true,
            mode_exit: false,
            stress: false,
            owed_latch: false,
            pair_base: 0x1000_0000,
        }
    }

    #[test]
    fn j27_page_poll_admits_a_plain_back_edge_only() {
        assert!(shape_admits(ask()));
        let refused = [
            PagePollAsk { back_edge: false, ..ask() },
            PagePollAsk { mode_exit: true, ..ask() },
            PagePollAsk { stress: true, ..ask() },
            PagePollAsk { owed_latch: true, ..ask() },
            PagePollAsk { pair_base: 0, ..ask() },
            PagePollAsk { pair_base: 0x1000_0010, ..ask() },
            PagePollAsk { pair_base: 0x7FFF_F000, ..ask() },
        ];
        for a in refused {
            assert!(!shape_admits(a), "{a:?} must take the byte poll");
        }
    }

    #[test]
    fn j27_page_poll_bytes_are_the_single_pass_encoding() {
        let bytes = page_poll_bytes(0x64, 0x0000_0A28, 0x1000_0000);
        assert_eq!(
            bytes,
            [
                0x64, 0x4C, 0x8B, 0x1C, 0x25, 0x28, 0x0A, 0x00, 0x00, // MOV R11, fs:[0xA28]
                0x41, 0x85, 0x83, 0x00, 0x00, 0x00, 0x10, // TEST [R11 + 0x10000000], EAX
            ]
        );
        assert_eq!(&bytes[SITE_OFFSET..SITE_OFFSET + 3], &[0x41, 0x85, 0x83]);
    }

    #[test]
    fn j27_page_poll_inline_skip_is_jz_off_and_jmp_on() {
        assert_eq!(inline_skip_opcode(false), &[0x0F, 0x84]);
        assert_eq!(inline_skip_opcode(true), &[0xE9]);
    }

    #[test]
    fn j27_page_poll_switch_is_opt_in() {
        let read = |v: Option<&str>| {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_JIT_IR_PAGE_POLL", v)],
                ir_page_poll_enabled,
            )
        };
        assert!(!read(None), "unset is off");
        assert!(read(Some("1")), "=1 is on");
    }
}
