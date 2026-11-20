use super::CompileInputs;

fn production() -> String {
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
    whole.to_string()
}

fn production_fn(sig: &str) -> String {
    let src = production();
    let start = src.find(sig);
    assert!(start.is_some(), "`{sig}` not found");
    let body = &src[start.unwrap_or(0)..];
    // `\u{7d}` is `}`: spelled so this line's braces balance for the
    // panic gates' line-by-line brace count (`scan_production_section`).
    body[..body.find("\n\u{7d}\n").map_or(body.len(), |e| e + 3)].to_string()
}

fn flat(s: &str) -> String {
    s.chars().filter(|c| !c.is_ascii_whitespace()).collect()
}

/// `read` opens the witness and hands back what the read produced; a read
/// that finds nothing builds nothing.
#[test]
fn read_pairs_the_witness_with_what_was_read() {
    let inputs = CompileInputs::read(|| Some(7_u32));
    assert_eq!(inputs.as_ref().map(|i| *i.get()), Some(7));
    assert!(CompileInputs::<u32>::read(|| None).is_none());
}

/// The one request builder takes `CompileInputs`, and no other
/// production line of this file builds a `CompileRequest`.
#[test]
fn every_compile_request_is_built_from_compile_inputs() {
    let src = production();
    assert_eq!(src.matches("CompileRequest::new(").count(), 1);
    let builder = flat(&production_fn("fn with_compile_request<R>("));
    assert!(builder.contains("inputs:&CompileInputs,"));
    assert!(builder.contains("letcached:&CachedBytecodeMethod=inputs.cached();"));
    assert!(builder.contains("CompileRequest::new(cached,helpers,"));
}

/// The witness is opened in one place only: `BytecodeReadWitness::open`,
/// which every `CompileInputs` constructor goes through.
#[test]
fn the_witness_is_opened_only_by_bytecode_read_witness() {
    let src = production();
    assert_eq!(
        src.matches("cratonvm_jit::open_compile_epoch_witness()")
            .count(),
        1
    );
    let open = flat(&production_fn("\nimpl BytecodeReadWitness {"));
    assert!(open.contains("Self(cratonvm_jit::open_compile_epoch_witness())"));
}

/// The two backends' entry points take `CompileInputs`, and each door
/// that holds bytecode it did not read itself adopts it.
#[test]
fn the_compile_entry_points_take_compile_inputs() {
    let optimizing = flat(&production_fn("fn compile_optimizing_artifact("));
    assert!(optimizing.contains("inputs:&CompileInputs,"));
    let osr = flat(&production_fn("fn compile_osr_body("));
    assert!(osr.contains("inputs:&CompileInputs<OsrBytecode>,"));
    let upgrade = flat(&production_fn("fn try_jit_upgrade_with_gate_uncontained("));
    let adopt = upgrade.find("letinputs=CompileInputs::adopt(shared,Arc::clone(cached))?;");
    let compile = upgrade.find("compile_optimizing_artifact(shared,&inputs,false)");
    assert!(adopt.is_some() && adopt < compile);
    let build = flat(&production_fn("fn build_osr_optimizing_artifact("));
    let adopt = build.find("letSome(inputs)=CompileInputs::adopt(shared,Arc::clone(cached))else");
    let stamps = build.find("letstamps=OsrBuildStamps::read(shared,class_id);");
    let compile = build.find("compile_optimizing_artifact(shared,&inputs,true)");
    assert!(adopt.is_some() && adopt < stamps && stamps < compile);
    let door = flat(&production_fn("fn compile_osr_artifact("));
    assert!(door.contains(
        "OsrCodeSource::Frame{code,max_locals}=>{letSome(frame_inputs)=CompileInputs::adopt_frame_code("
    ));
}

/// The direct-bind callee compile opens its witness before it looks the
/// method up, and builds its request from the inputs it pairs with it.
#[test]
fn the_direct_bind_callee_compile_reads_inside_its_witness() {
    let body = flat(&production_fn("fn compile_optimizing_artifact("));
    let witness = body.find("letcallee_witness=BytecodeReadWitness::open();");
    let lookup = body.find("callee_class_id,declaring_class_arc,method,declaring_id,");
    let inputs = body.find("letcallee_inputs=callee_witness.inputs(");
    let request = body.find("c_cp.with_compile_request(&callee_inputs,");
    assert!(witness.is_some() && witness < lookup && lookup < inputs && inputs < request);
}

/// `OsrBytecode` is what `compile_osr_body` reads its code and locals from.
#[test]
fn the_osr_body_reads_its_code_from_the_inputs() {
    let body = flat(&production_fn("fn compile_osr_body("));
    assert!(body.contains("letcode:&[u8]=&inputs.get().code;"));
    assert!(body.contains("letmax_locals:usize=inputs.get().max_locals;"));
}
