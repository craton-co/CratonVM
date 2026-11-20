/// The text of the production function starting at `sig`, up to the next
/// top-level item.
fn production_fn(sig: &str) -> String {
    let whole = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let start = whole.find(sig);
    assert!(start.is_some(), "`{sig}` not found");
    let body = &whole[start.unwrap_or(0)..];
    let end = body[1..]
        .find("\nfn ")
        .into_iter()
        .chain(body[1..].find("\nunsafe fn "))
        .chain(body[1..].find("\npub "))
        .chain(body[1..].find("\n#[cfg(test)]"))
        .min()
        .map_or(body.len(), |e| e + 1);
    body[..end].to_string()
}

/// `CRATONVM_JIT_SPECIAL_BC_SYNC`: the `invokespecial` door admits a
/// `synchronized` template only through the switch, and serves one only
/// through the monitored install (never the unlocked one), as the virtual
/// twin does. A source witness: the door needs a live VM, a compiled caller
/// and a final class to be reached at all.
#[test]
fn the_special_door_admits_a_synchronized_template_only_monitored() {
    let resolve = production_fn("\npub fn resolve_special_bytecode_callee(");
    assert!(resolve.contains("build_bytecode_callee_cached("));
    assert!(resolve.contains("special_bytecode_callee_sync_enabled()"));
    assert!(
        !resolve.contains("build_lambda_impl_cached("),
        "off, the switch must leave exactly `build_lambda_impl_cached`'s answer"
    );
    let door = production_fn("\npub unsafe fn try_jit_special_bytecode_callee(");
    let sync_arm = door.find("if cached.is_synchronized");
    // Wave 138 (interpreter i7, L1): the compiled caller's form of the
    // unlocked install.
    let unlocked = door.find("install_and_run_cached_frame_from_compiled(");
    assert!(sync_arm.is_some() && unlocked.is_some());
    assert!(
        sync_arm < unlocked,
        "a synchronized template must leave before the unlocked install"
    );
    let serve = production_fn("\npub fn special_bytecode_callee_monitored(");
    assert!(serve.contains("install_and_run_cached_frame_monitored("));
    assert!(!serve.contains("install_and_run_cached_frame("));
}
