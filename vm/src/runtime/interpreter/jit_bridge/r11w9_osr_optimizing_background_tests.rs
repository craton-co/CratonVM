use super::{
    osr_optimizing_body_ready, osr_optimizing_refusal_key, remember_osr_optimizing_artifact,
    OsrBuildStamps,
};
use crate::config::VmConfig;
use crate::vm::SharedVm;
use cratonvm_types::ClassId;
use std::sync::Arc;

const CLASS: &str = "r11w9/Loop";
const METHOD: &str = "run";
const DESC: &str = "()V";

/// Metadata only; the `ret` body never runs.
fn body(entries: &[u32], sentinel_free: bool) -> Arc<cratonvm_jit::CompiledMethod> {
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).unwrap();
    buf.emit(&[0xC3]); // ret
    let mut cm = cratonvm_jit::CompiledMethod::new(buf);
    cm.ir_osr_entries = entries.iter().map(|&b| (b, 0, 0)).collect();
    cm.ir_osr_sentinel_free = sentinel_free;
    Arc::new(cm)
}

fn ready(shared: &SharedVm, cid: ClassId, code: &Arc<[u8]>, pc: usize) -> bool {
    osr_optimizing_body_ready(shared, cid, CLASS, METHOD, DESC, code, pc)
}

/// What the builder does with an accepted body: keep it under `pc`.
fn keep(
    shared: &SharedVm,
    cid: ClassId,
    pc: usize,
    artifact: &Arc<cratonvm_jit::CompiledMethod>,
    code: &Arc<[u8]>,
) {
    let key = osr_optimizing_refusal_key(shared.vm_identity, cid, METHOD, DESC, pc);
    let pins: Arc<[cratonvm_jit::RetainedCode]> =
        Vec::<cratonvm_jit::RetainedCode>::new().into();
    let stamps = OsrBuildStamps::read(shared, cid);
    // `exits_admitted: false`: these bodies are admitted (or not) by
    // `ir_osr_sentinel_free` alone, as before tier proposal 27.
    remember_osr_optimizing_artifact(shared, key, artifact, &pins, stamps, code, false);
}

#[test]
fn the_background_arm_sees_only_an_enterable_cached_optimizing_body() {
    // Each kill switch makes the arm answer `false` by design.
    if !super::osr_optimizing_tier_enabled()
        || super::osr_optimizing_cache_disabled()
    {
        return;
    }
    let shared = SharedVm::new(VmConfig::default());
    let cid = ClassId::new(4242);
    // `iconst_0; istore_1; goto 2`, padded like `Frame::code`.
    let code: Arc<[u8]> = Arc::from(&[0x03u8, 0x3c, 0xa7, 0x00, 0x00, 0x00, 0x00][..]);

    assert!(!ready(&shared, cid, &code, 2), "nothing is cached yet");

    keep(&shared, cid, 2, &body(&[2], true), &code);
    assert!(
        ready(&shared, cid, &code, 2),
        "an accepted body with a stub at this pc"
    );

    let other: Arc<[u8]> = Arc::from(&[0xb1u8, 0x00, 0x00][..]);
    assert!(
        !ready(&shared, cid, &other, 2),
        "a frame running other bytecode must not be handed this body"
    );

    // Cached under a pc it has no stub for: `try_osr` would not enter it,
    // so the arm must not send the frame there.
    keep(&shared, cid, 9, &body(&[2], true), &code);
    assert!(!ready(&shared, cid, &code, 9), "no entry stub at this pc");

    // A body that can return the sentinel is not the door's to enter.
    keep(&shared, cid, 5, &body(&[5], false), &code);
    assert!(!ready(&shared, cid, &code, 5), "not sentinel-free");

    // A denied method keeps the arm's `osr_method_denied` accounting.
    let method = crate::jit::tiered::MethodKey::with_class_id(cid, CLASS, METHOD, DESC);
    shared.jit.tiered_manager.mark_osr_denied(method);
    assert!(
        !ready(&shared, cid, &code, 2),
        "OSR is denied for the method"
    );
}

/// A pc-blind verdict is kept once per method: every header of one method
/// shares the key, and no real pc (nor another method) does.
#[test]
fn a_method_wide_refusal_key_covers_every_header_of_its_method_only() {
    use super::osr_optimizing_method_wide_key as wide;
    let cid = ClassId::new(7);
    let at = |vm: usize, desc: &str, pc: usize| {
        osr_optimizing_refusal_key(vm, cid, METHOD, desc, pc)
    };
    assert_eq!(wide(at(1, DESC, 2)), wide(at(1, DESC, 40)));
    assert_ne!(wide(at(1, DESC, 2)), at(1, DESC, 2));
    assert_ne!(wide(at(1, DESC, 2)), wide(at(1, "(I)V", 2)));
    assert_ne!(wide(at(1, DESC, 2)), wide(at(2, DESC, 2)));
}

/// The worker asks the optimizing route BEFORE it builds the single-pass
/// body, and only the optimizing route's own gate can skip that build. A
/// text pin, because `background_compile_task` needs a live VM, a loaded
/// class and a backend.
#[test]
fn the_background_osr_task_asks_the_optimizing_route_first() {
    let whole = concat!(
        include_str!("../jit_bridge/mod.rs"),
        "\n",
        include_str!("../jit_bridge/jit_verdicts.rs"),
        "\n",
        include_str!("../jit_bridge/osr_continuation.rs"),
        "\n",
        include_str!("../jit_bridge/compile_osr_body.rs"),
        "\n",
        include_str!("../jit_bridge/osr_exception_exit.rs"),
        "\n",
        include_str!("../jit_bridge/try_osr.rs"),
        "\n",
        include_str!("../jit_bridge/native_shadow_arms.rs"),
        "\n",
        include_str!("../jit_bridge/compile_optimizing_artifact.rs"),
        "\n",
        include_str!("../jit_bridge/never_run_static_exit_pcs.rs"),
        "\n",
        include_str!("../jit_bridge/try_jit_compile_callee_slow.rs"),
        "\n",
        include_str!("../jit_bridge/byte_array_field_declarer_admitted.rs"),
        "\n",
        include_str!("../jit_bridge/folded_handle_member_row.rs"),
        "\n",
        include_str!("../jit_bridge/callee_never_taken_rows.rs"),
        "\n",
        include_str!("../jit_bridge/mint_ldc_slots_at_offer.rs")
    )
    .replace("\r\n", "\n");
    let src = whole.as_str();
    let task = &src[src
        .find("\npub(in crate::runtime::interpreter) fn background_compile_task(")
        .expect("background_compile_task exists")..];
    // Whitespace stripped, so the pin survives any wrapping `cargo fmt`
    // chooses (the reason `jit/src/tests.rs`'s `code_only` exists).
    let flat: String = task.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let optimizing = flat
        .find("background_osr_optimizing_body(&shared,&task.method_key,osr_bciasusize)")
        .expect("the OSR task asks the optimizing route");
    let single_pass = flat
        .find("compile_osr_artifact(")
        .expect("the OSR task still builds the single-pass body");
    assert!(
        optimizing < single_pass,
        "the optimizing route must be asked before the single-pass compile"
    );
}
