//! Exit-time censuses of the compiled-code native funnel (JIT round 19 wave 2,
//! lane funnel2; `docs/feature-designs/jit-ir-optimizations.md`).
//!
//! * `CRATONVM_DBG_NATIVE_SITE_DOOR_CENSUS` -- the SBR5 lean native door
//!   (`jit::helpers::try_lean_native_door`): how many compiled calls it saw,
//!   served, missed (empty slot vs a slot another site holds) and declined,
//!   and what a served crossing costs in its three spans. The round-17 split
//!   census (`CRATONVM_DBG_NATIVE_DOOR_SPLIT`) times only the site-cache door
//!   behind it, so since SBR5 it no longer sees a warm plain crossing.
//! * `CRATONVM_DBG_FUNNEL_INTRINSIC_CENSUS` (FUNNEL17-4) -- every registered
//!   `Intrinsic` / `Bridge` that owns its slot and whose LOADED class declares
//!   a concrete (non-native, non-abstract) body for the same triple: the
//!   candidates to un-register so the JIT compiles and inlines the JDK body
//!   instead of crossing into Rust. Each row gives the body's size, its invoke
//!   count (a body with none is a pure field/compare/loop body) and how often
//!   this run dispatched the native by name or id (a lower bound; see
//!   `NativeCensusEntry::invocations`).
//!
//! Both print nothing unless armed. Called from `vm-cli`'s shutdown path
//! (`finish_jdk_only`), which every exit arm with a VM reaches.

use crate::SharedVm;

/// Rows printed by the FUNNEL17-4 census (the rest are counted only).
const INTRINSIC_CENSUS_ROWS: usize = 120;

/// One FUNNEL17-4 row.
struct BodyRow {
    class: String,
    name: String,
    descriptor: String,
    kind: &'static str,
    invocations: u64,
    code_len: usize,
    invokes: usize,
    has_handlers: bool,
}

/// `(code length, invoke instructions)` of `code`, walked with the JIT's own
/// instruction-length decoder.
fn body_shape(code: &[u8]) -> (usize, usize) {
    let mut pc = 0usize;
    let mut invokes = 0usize;
    while pc < code.len() {
        // invokevirtual, invokespecial, invokestatic, invokeinterface,
        // invokedynamic.
        if (0xb6..=0xba).contains(&code[pc]) {
            invokes += 1;
        }
        pc += cratonvm_jit::bytecode_insn_len(code, pc).max(1);
    }
    (code.len(), invokes)
}

impl SharedVm {
    /// Print the armed native-funnel censuses (see the module doc). Cheap and
    /// silent when neither switch is on.
    pub fn report_native_funnel_censuses_at_exit(&self) {
        if let Some(line) = self.natives.native_methods.native_site_door_census_line() {
            eprintln!("{line}");
        }
        if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_FUNNEL_INTRINSIC_CENSUS") {
            self.report_funnel_intrinsic_census();
        }
    }

    fn report_funnel_intrinsic_census(&self) {
        use cratonvm_native_api::NativeKind;
        let rows = self.natives.native_methods.census();
        // The System.exit path may find the class manager held; say so rather
        // than print an empty census that reads as "no candidates".
        let Some(cm) = self.classes.class_manager.try_read_untracked() else {
            eprintln!("[funnel-intrinsic-census] class-manager lock unavailable at exit; no rows");
            return;
        };
        let mut considered = 0usize;
        let mut not_loaded = 0usize;
        let mut native_in_jdk = 0usize;
        let mut out: Vec<BodyRow> = Vec::new();
        for row in &rows {
            let kind = match row.kind {
                NativeKind::Intrinsic => "intrinsic",
                NativeKind::Bridge => "bridge",
                NativeKind::SyntheticStub => continue,
            };
            if !row.owns_slot {
                continue;
            }
            considered += 1;
            let Some(class) = cm
                .get_loaded_class_id(&row.class)
                .and_then(|id| cm.get_class(id))
            else {
                not_loaded += 1;
                continue;
            };
            let Some(method) = class.find_method(&row.name, &row.descriptor) else {
                continue;
            };
            if method.is_native() || method.is_abstract() {
                native_in_jdk += 1;
                continue;
            }
            let code = method.attributes.iter().find_map(|a| {
                match a.decoded_or_decode(&class.constant_pool)?.into_owned() {
                    cratonvm_reader::attribute::Attribute::Code(code) => Some(code),
                    _ => None,
                }
            });
            let Some(code) = code else {
                continue;
            };
            let (code_len, invokes) = body_shape(&code.code);
            out.push(BodyRow {
                class: row.class.clone(),
                name: row.name.clone(),
                descriptor: row.descriptor.clone(),
                kind,
                invocations: row.invocations,
                code_len,
                invokes,
                has_handlers: !code.exception_table.is_empty(),
            });
        }
        drop(cm);
        // Most dispatched first; among equals, the smallest body first (the
        // cheapest un-registration).
        out.sort_by(|a, b| {
            b.invocations
                .cmp(&a.invocations)
                .then(a.code_len.cmp(&b.code_len))
                .then_with(|| (&a.class, &a.name).cmp(&(&b.class, &b.name)))
        });
        let leaf_bodies = out.iter().filter(|r| r.invokes == 0).count();
        eprintln!(
            "[funnel-intrinsic-census] mode={} owning intrinsic/bridge rows={considered} \
             class-not-loaded={not_loaded} native-in-jdk={native_in_jdk} \
             with-jdk-body={} (no-invoke bodies={leaf_bodies}); rows by dispatches, then size:",
            self.compatibility_mode().as_str(),
            out.len(),
        );
        for r in out.iter().take(INTRINSIC_CENSUS_ROWS) {
            eprintln!(
                "[funnel-intrinsic-census]   {}.{}{} kind={} dispatches={} code_len={} invokes={}{}",
                r.class,
                r.name,
                r.descriptor,
                r.kind,
                r.invocations,
                r.code_len,
                r.invokes,
                if r.has_handlers { " handlers" } else { "" },
            );
        }
        if out.len() > INTRINSIC_CENSUS_ROWS {
            eprintln!(
                "[funnel-intrinsic-census]   ... {} more rows not printed",
                out.len() - INTRINSIC_CENSUS_ROWS
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::body_shape;

    #[test]
    fn body_shape_counts_invokes_and_steps_whole_instructions() {
        // aload_0; getfield #2; invokevirtual #3; ireturn
        let code = [0x2a, 0xb4, 0x00, 0x02, 0xb6, 0x00, 0x03, 0xac];
        assert_eq!(body_shape(&code), (8, 1));
        // The getfield operand byte 0xb6 must not count as an invoke.
        let code = [0x2a, 0xb4, 0xb6, 0xb6, 0xac];
        assert_eq!(body_shape(&code), (5, 0));
    }
}
