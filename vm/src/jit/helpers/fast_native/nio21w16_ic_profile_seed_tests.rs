fn body_of(src: &str, sig: &str) -> String {
    let start = src.find(sig).expect("the function exists");
    let rest = &src[start + 1..];
    rest[..rest.find("\n\x7d\n").map_or(rest.len(), |e| e + 3)].to_string()
}

/// The seed note runs after the bound-site route (whose slots are never
/// armed) and before every virtual door, under a recording panic guard.
#[test]
fn the_seed_note_is_asked_before_the_virtual_doors() {
    let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let body = body_of(&src, "\npub unsafe extern \"C\" fn jit_invoke_virtual_mic(");
    let bound = body
        .find("jit_invoke_bound_site_mic(")
        .expect("the bound-site route");
    let note = body
        .find("note_ic_profile_seed(vm_ptr, seed, args_ptr)")
        .expect("the seed note");
    assert!(bound < note);
    assert!(body.contains("contain(\"ic_profile_seed\", OnPanic::RecordFor { vm_ptr }"));
    for door in [
        "try_ffm_direct_door(",
        "try_lean_native_door(",
        "jit_invoke_virtual_mic_body(",
    ] {
        assert!(note < body.find(door).unwrap(), "the note precedes {door}");
    }
}

/// The recompile is asked only after the profile row was written and the
/// one-per-method claim succeeded.
#[test]
fn the_recompile_follows_the_record_and_the_claim() {
    let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let body = body_of(&src, "\npub unsafe fn note_ic_profile_seed(");
    let record = body.find("seed_ic_observed_receiver(").expect("the record");
    let claim = body.find("claim_ic_seed_recompile(").expect("the claim");
    let ask = body
        .find("request_ic_seed_recompile(")
        .expect("the request");
    assert!(record < claim && claim < ask);
}

/// j27 HANGHUNT (`CRATONVM_JIT_IR_IC_SEED_CLAIM_ON_ENQUEUE`): a filled row
/// still asks under the switch, and a refused retry hands the claim back,
/// after the request answered.
#[test]
fn j27_hanghunt_a_refused_seed_retry_releases_its_claim() {
    let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let body = body_of(&src, "\npub unsafe fn note_ic_profile_seed(");
    assert!(body.contains("ic_seed_asks_recompile(seeded, claim_on_enqueue)"));
    let ask = body
        .find("request_ic_seed_recompile(")
        .expect("the request");
    let kept = body
        .find("ic_seed_claim_kept(answer, claim_on_enqueue)")
        .expect("the claim verdict");
    let release = body.find("release_ic_seed_recompile(").expect("the release");
    assert!(ask < kept && kept < release);
}
