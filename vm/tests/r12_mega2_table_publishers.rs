// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Round 12 wave 3, lane mega2 (proposal W2-1 step 3 in
//! `docs/internal/jit-proposals/jit-r12-calls-proposals-RETIRED-20260928.md`): every arm of the
//! dispatch helpers that publishes a compiled body for a (call-site record,
//! receiver class) pair also publishes it into the VM's shared megamorphic
//! table, which the hashed stub now reads in machine code.
//!
//! A source witness: the publication is observable only as a missing helper
//! round trip at ANOTHER call site, which no unit test in this crate can
//! drive without a compiled Java caller. `R12MegaSharedSites.java` and
//! `R12MegaLambdaSites.java` are the behavioural probes.

fn body<'a>(src: &'a str, signature: &str) -> &'a str {
    let start = src
        .find(signature)
        .unwrap_or_else(|| panic!("`{signature}` exists"));
    let rest = &src[start..];
    let end = rest.find("\n}\n").map_or(rest.len(), |e| e + 3);
    &rest[..end]
}

#[test]
fn every_resolving_arm_publishes_into_the_shared_table() {
    let src = include_str!("../src/jit/helpers.rs").replace("\r\n", "\n");

    // The MIC helper's two resolving arms (step 1).
    let mic = body(&src, "\nunsafe fn jit_invoke_virtual_mic_body(");
    assert_eq!(
        mic.matches("publish_mega_dispatch_entry(").count(),
        2,
        "both resolving arms of jit_invoke_virtual_mic publish"
    );

    // The lambda-thunk install (step 3): under the SAM site's own slot.
    let lambda = body(&src, "\nunsafe fn install_lambda_inline_cache(");
    assert!(
        lambda.contains("install_mega_dispatch_way(") && lambda.contains("Some(pic),"),
        "the lambda thunk is published under its SAM site's selector"
    );
    assert!(
        lambda.contains("mega_table_publish_lambda_enabled()"),
        "the lambda publication has its kill switch"
    );

    // `jit_invoke_dispatch`'s virtual arm (step 3): no slot, so it only
    // publishes, and never an indy-trapping body.
    let dispatch = body(&src, "\nunsafe fn jit_invoke_dispatch_body(");
    assert!(
        dispatch.contains("install_mega_dispatch_way(") && dispatch.contains("None,"),
        "the blind virtual arm publishes into the shared table"
    );
    assert!(
        dispatch.contains("!_callee_pin.has_indy_trap"),
        "an indy-trapping body is never published for machine-code readers"
    );
    assert!(
        dispatch.contains("mega_table_publish_dispatch_enabled()"),
        "the dispatch publication has its kill switch"
    );

    // One installer, and it withdraws a publication made across a
    // redefinition.
    let install = body(&src, "\nunsafe fn install_mega_dispatch_way(");
    assert!(install.contains("redefine_epoch() != epoch_seen"));
    assert!(install.contains("table.retire_entry("));
}
