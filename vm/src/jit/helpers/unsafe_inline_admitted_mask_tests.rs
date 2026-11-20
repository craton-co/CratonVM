use super::*;

/// The VM's gate list and the JIT's bit order name the same triples in the
/// same order: a swapped pair would admit inline code for one accessor on
/// the strength of another's policy answer.
#[test]
fn the_gate_order_is_the_jit_bit_order() {
    let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let start = src
        .find("fn unsafe_inline_admitted_mask(")
        .expect("the mask function exists");
    let body = &src[start..start + src[start..].find("\n}\n").expect("closed item")];
    let names: Vec<&str> = body
        .split('&')
        .skip(1)
        .filter_map(|s| s.split(',').next())
        .map(str::trim)
        .filter(|s| s.starts_with("UNSAFE_") && s.ends_with("_GATE"))
        .collect();
    assert_eq!(
        names,
        [
            "UNSAFE_GET_BYTE_GATE",
            "UNSAFE_GET_SHORT_GATE",
            "UNSAFE_GET_CHAR_GATE",
            "UNSAFE_GET_INT_GATE",
            "UNSAFE_GET_LONG_GATE",
            "UNSAFE_PUT_BYTE_GATE",
            "UNSAFE_PUT_SHORT_GATE",
            "UNSAFE_PUT_CHAR_GATE",
            "UNSAFE_PUT_INT_GATE",
            "UNSAFE_PUT_LONG_GATE",
            "UNSAFE_GET_INT_VOLATILE_GATE",
            "UNSAFE_GET_LONG_VOLATILE_GATE",
            "UNSAFE_PUT_INT_VOLATILE_GATE",
            "UNSAFE_PUT_LONG_VOLATILE_GATE",
            "UNSAFE_CAS_INT_GATE",
            "UNSAFE_CAS_LONG_GATE",
        ],
        "unsafe_inline_admitted_mask's gate list moved; keep it in GATE_BITS order",
    );
    let gates: [&DirectHelperGate; 16] = [
        &UNSAFE_GET_BYTE_GATE,
        &UNSAFE_GET_SHORT_GATE,
        &UNSAFE_GET_CHAR_GATE,
        &UNSAFE_GET_INT_GATE,
        &UNSAFE_GET_LONG_GATE,
        &UNSAFE_PUT_BYTE_GATE,
        &UNSAFE_PUT_SHORT_GATE,
        &UNSAFE_PUT_CHAR_GATE,
        &UNSAFE_PUT_INT_GATE,
        &UNSAFE_PUT_LONG_GATE,
        &UNSAFE_GET_INT_VOLATILE_GATE,
        &UNSAFE_GET_LONG_VOLATILE_GATE,
        &UNSAFE_PUT_INT_VOLATILE_GATE,
        &UNSAFE_PUT_LONG_VOLATILE_GATE,
        &UNSAFE_CAS_INT_GATE,
        &UNSAFE_CAS_LONG_GATE,
    ];
    assert_eq!(
        gates.len(),
        cratonvm_jit::direct_helpers::UnsafeInlineAccess::GATE_BITS.len()
    );
    for (i, (method, desc)) in cratonvm_jit::direct_helpers::UnsafeInlineAccess::GATE_BITS
        .iter()
        .enumerate()
    {
        let (c, m, d) = gates[i].triple();
        assert_eq!(
            (c, m, d),
            ("jdk/internal/misc/Unsafe", *method, *desc),
            "bit {i}"
        );
    }
}

/// A default `--compatible` VM -- whose installed policy is `Permissive`
/// (`vm_init`), unless `CRATONVM_CAPABILITY_MODE` says otherwise --
/// admits every row.
#[test]
fn a_default_compatible_vm_admits_every_row() {
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    assert!(!crate::vm::dispatch_policy(&shared).is_jdk_only());
    // nio21 wave 4 L4: fourteen rows (the volatile int/long four added);
    // nio21 wave 5 L3: sixteen (the two CAS rows), unless the VM's array
    // CAS is back on its lock.
    let want = if crate::runtime::env_cache::unsafe_array_hw_cas_enabled() {
        0xFFFF        } else {
        0x3FFF        };
    assert_eq!(unsafe_inline_admitted_mask(&shared), want);    }