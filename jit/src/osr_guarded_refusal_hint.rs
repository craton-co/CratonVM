//! j27 wave 3 (lane SMALLCALL): what the `osr-splice-guarded-refused` debug
//! line (`plan_osr_door_guarded_splices`, `CRATONVM_DBG_JITC=1`) adds after
//! the verdict.
//!
//! The single-pass OSR door's guarded pass reports every `None` from the VM's
//! receiver resolver as `CalleeUnresolved { speculated_class_id: Some(..) }`.
//! That reads as "the receiver class or its method could not be found", but
//! the commonest cause is the resolver REFUSING the body it found: this pass
//! hands it no direct binder, so a body that carries a call
//! (`ByteBuffer.position(I)ByteBuffer` is `{ super.position(p); return this; }`,
//! and `Buffer.position(I)` builds its exception message through an
//! `invokedynamic` concat) is refused as "neither spliced nor direct-bound".
//! The small-JDK-method page read `CalleeUnresolved` for
//! `CharBuffer.position` / `limit` as a lookup failure. The resolver prints the
//! real reason on its own `inline-resolve REFUSED ... route=receiver=<id>` line;
//! the hint points there. Debug output only: no plan changes. Since j27 wave
//! 4 (lane OSRSUPER) the hint also names the opt-in that binds the
//! `super.m()` case (`crate::osr_guarded_super_bind`).

use crate::InlineRefusal;

/// The suffix of the refusal line for `refusal` (empty for every other
/// refusal, and for an admitted plan).
pub(crate) fn osr_guarded_refusal_hint(refusal: Option<&InlineRefusal>) -> &'static str {
    match refusal {
        Some(InlineRefusal::CalleeUnresolved {
            speculated_class_id: Some(_),
        }) => {
            " (the receiver resolver returned no body; its reason is the \
             `inline-resolve REFUSED ... route=receiver=<id>` line above, \
             typically a call-carrying body this pass cannot direct-bind; \
             `CRATONVM_JIT_OSR_GUARDED_SUPER_BIND=1` binds a `super.m()` call, \
             see its `osr-splice-guarded-super-bind` line)"
        }
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::osr_guarded_refusal_hint;
    use crate::InlineRefusal;

    #[test]
    fn a_receiver_resolver_refusal_points_at_the_resolver_line() {
        let r = InlineRefusal::CalleeUnresolved {
            speculated_class_id: Some(419),
        };
        let hint = osr_guarded_refusal_hint(Some(&r));
        assert!(hint.contains("inline-resolve REFUSED"), "{hint}");
        assert!(hint.contains("route=receiver="), "{hint}");
        // j27 wave 4 (lane OSRSUPER): and names the opt-in that lifts it.
        assert!(hint.contains("CRATONVM_JIT_OSR_GUARDED_SUPER_BIND=1"), "{hint}");
    }

    #[test]
    fn other_verdicts_get_no_hint() {
        // A constant-pool miss (`speculated_class_id: None`) is a real lookup
        // failure, and an admitted plan has no refusal at all.
        let cp = InlineRefusal::CalleeUnresolved {
            speculated_class_id: None,
        };
        assert_eq!(osr_guarded_refusal_hint(Some(&cp)), "");
        assert_eq!(osr_guarded_refusal_hint(Some(&InlineRefusal::ForceInterpreted)), "");
        assert_eq!(osr_guarded_refusal_hint(None), "");
    }
}
