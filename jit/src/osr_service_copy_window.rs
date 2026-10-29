//! j27 wave 8 (lane W8OSR): the OSR half of the service-copy window
//! (`CRATONVM_JIT_OSR_SERVICE_COPY_WINDOW`, opt-in; proposal
//! `docs/known-issues/jit/j27-osr-direct-call-service-copy-window-proposal-20261028.md`).
//!
//! dev's gkr window (`CRATONVM_JIT_SERVICE_COPY_WINDOW`, declared on dev, not
//! in this tree yet) lets the collector clear a refused direct call's service
//! copies of its reference arguments once the CALLEE stands where none of its
//! re-runs from entry can still happen. Its site admission,
//! `ir_lower::Lowerer::ir_direct_call_site_admits_service_copy_window`, passes
//! `self.graph.osr_compile` to `ir_direct_call_site_admits_owned_args`, so an
//! OSR body never admits a site. That term is the owned HAND-OFF's: a declined
//! owned callee becomes the caller's frameless deopt, answered by a re-run of
//! the caller from bci 0, which an OSR frame cannot give
//! (`osr_refuse_replay`). The window hands nothing off; the call keeps its
//! copies. The proposal asked two questions about the OSR body's readers of
//! the argument homes after the `CALL`. Both answers, by reading this tree:
//!
//! 1. **The post-call sentinel.** `i64::MIN` in RAX after a direct call means
//!    the callee-deopt service already ran: it either finished the callee (a
//!    value comes back, no sentinel) or left an exception pending. The cold
//!    side's exit is the exception tail (`push_call_exc_patch`), the same
//!    `call_exc_patches` row in both kinds of compile. It does not resume the
//!    interpreter AT the invoke from the invoke's own snapshot. In an OSR body
//!    the row becomes an `osr_exception_exit_bcis` exit, which the door routes
//!    with `route_osr_exception_out_of_artifact`: at a site no handler covers
//!    (the admission's `protected` term) there is no precise frame, so the
//!    exception propagates and no frame value is read.
//! 2. **Exception exits.** The pads that DO publish a frame (reason-9
//!    rethrow pads, call-site pads, compiled-handler arms) exist only at sites
//!    the admission already refuses (`armed_exits`, `protected`), and a
//!    rethrow pad clears the operand stack (`plan_rethrow_pad`), so it never
//!    names the call's arguments anyway. Guard exits after the call read the
//!    successors' states, which `argument_only_homes` already excludes in
//!    both kinds of compile.
//!
//! So the only reader left is the service's re-run of the callee, and that
//! reader is the one the window's GC half (`clear_closed_service_copies`)
//! judges from the CALLEE's code, whichever tier or entry the caller has.
//! The `osr_compile` term is therefore not load-bearing for the window, and
//! [`service_copy_window_osr_term`] drops it under the opt-in switch.
//!
//! Composition (at the next dev merge): in
//! `ir_direct_call_site_admits_service_copy_window` replace the argument
//! `self.graph.osr_compile` with
//! `crate::osr_service_copy_window::service_copy_window_osr_term(self.graph.osr_compile)`.
//! The owned hand-off (`ir_direct_call_args_owned_by_callee`) keeps the plain
//! `self.graph.osr_compile`. Nothing else changes: the GC half still needs
//! `CRATONVM_JIT_SERVICE_COPY_WINDOW` (dev, default ON) and its own walk
//! conditions, so this switch alone only widens which sites RECORD copies.
//! Opt-in because no host run has shown the GC walk clearing a copy in an
//! OSR body's frame (`[jitrootscan] service-copies cleared=` naming the OSR
//! artifact's label).

/// `CRATONVM_JIT_OSR_SERVICE_COPY_WINDOW` (opt-in, `=1` enables; j27 wave 8,
/// lane W8OSR): may an optimizing OSR body record service copies at a refused
/// direct call (see the module doc)? Read per call site, no static.
pub fn osr_service_copy_window_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_OSR_SERVICE_COPY_WINDOW")
}

/// The `osr_compile` argument the service-copy-window admission passes to
/// `ir_direct_call_site_admits_owned_args`: the compile's own answer, unless
/// the switch drops the term (an OSR compile then admits like a method-entry
/// one). The owned hand-off must keep passing the plain `osr_compile`.
pub fn service_copy_window_osr_term(osr_compile: bool) -> bool {
    osr_term_for(osr_compile, osr_service_copy_window_enabled())
}

/// The decision of [`service_copy_window_osr_term`], pure.
fn osr_term_for(osr_compile: bool, enabled: bool) -> bool {
    osr_compile && !enabled
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "CRATONVM_JIT_OSR_SERVICE_COPY_WINDOW";

    #[test]
    fn the_term_drops_only_for_an_osr_compile_with_the_switch() {
        assert!(osr_term_for(true, false), "off: an OSR compile still refuses");
        assert!(!osr_term_for(true, true), "on: the OSR term is dropped");
        assert!(!osr_term_for(false, false), "a method-entry compile never refuses on it");
        assert!(!osr_term_for(false, true));
    }

    #[test]
    fn the_switch_is_opt_in() {
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            assert!(service_copy_window_osr_term(true), "unset = off");
        });
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("1"))], || {
            assert!(!service_copy_window_osr_term(true));
            assert!(!service_copy_window_osr_term(false));
        });
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("0"))], || {
            assert!(service_copy_window_osr_term(true));
        });
    }
}
