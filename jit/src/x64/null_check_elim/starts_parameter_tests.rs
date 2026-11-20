use super::{
    array_receiver_local, array_receiver_local_with_starts, bytecode_analysis,
    preceding_aload_nonnull_local, preceding_aload_nonnull_local_with_starts,
};

/// One method body carrying BOTH documented operand/opcode collisions, so
/// the equivalence below is asserted over bytes where a wrong answer is
/// actually reachable rather than over a straight-line decode that any
/// implementation would get right.
///
///   0: aload_0
///   1: getfield #42      `[0xB4, 0x00, 0x2A]` — the trailing constant-pool
///                        byte is numerically `aload_0`. This is the ES
///                        RestClient / `HttpRequestWrapper.getParams()`
///                        shape the SOUNDNESS FIX on
///                        `preceding_aload_nonnull_local` documents.
///   4: ifnonnull +3      the site that asks the question.
///   7: aload_1
///   8: sipush 3          `[0x11, 0x00, 0x03]` — the trailing byte is
///                        numerically `iconst_0`, the collision
///                        `array_receiver`'s SOUNDNESS FIX documents.
///  11: aaload            the other site that asks the question.
///  12: areturn
fn collision_body() -> Vec<u8> {
    #[rustfmt::skip]
    let code: Vec<u8> = vec![
        0x2a,               // 0: aload_0
        0xb4, 0x00, 0x2a,   // 1: getfield #42
        0xc7, 0x00, 0x03,   // 4: ifnonnull +3
        0x2b,               // 7: aload_1
        0x11, 0x00, 0x03,   // 8: sipush 3
        0x32,               // 11: aaload
        0xb0,               // 12: areturn
    ];
    code
}

/// The parameterised form and the convenience wrapper are the same
/// function, at every pc.
///
/// Asserted over the whole body (and a little past its end) rather than at
/// the two interesting pcs, because the failure mode this guards against is
/// an off-by-one introduced while threading `starts` — which shows up at
/// some unremarkable pc, not at the one the author was thinking about.
#[test]
fn with_starts_and_wrapper_agree_at_every_pc() {
    let code = collision_body();
    let starts = bytecode_analysis::instruction_starts(&code, code.len());

    // The map the hoisted callers must reproduce. Spelled out so that a
    // change to `instruction_starts` that silently re-shapes it fails here
    // and not as a miscompile.
    assert_eq!(
        starts,
        vec![
            true,  // 0  aload_0
            true,  // 1  getfield
            false, // 2  cp index high
            false, // 3  cp index low (0x2A, NOT an aload_0)
            true,  // 4  ifnonnull
            false, // 5  branch offset high
            false, // 6  branch offset low
            true,  // 7  aload_1
            true,  // 8  sipush
            false, // 9  imm high
            false, // 10 imm low (0x03, NOT an iconst_0)
            true,  // 11 aaload
            true,  // 12 areturn
        ]
    );

    // Past the end too: both spellings must agree on out-of-range pcs, not
    // merely on in-range ones.
    for pc in 0..code.len() + 4 {
        assert_eq!(
            preceding_aload_nonnull_local_with_starts(&code, pc, &starts),
            preceding_aload_nonnull_local(&code, pc),
            "preceding_aload_nonnull_local disagrees with its _with_starts \
             form at pc {pc}"
        );
        assert_eq!(
            array_receiver_local_with_starts(&code, pc, &starts),
            array_receiver_local(&code, pc),
            "array_receiver_local disagrees with its _with_starts form at \
             pc {pc}"
        );
    }

    // Agreement on `None` everywhere would also pass the loop above, so
    // pin the two answers that carry the soundness claims.
    assert_eq!(
        preceding_aload_nonnull_local_with_starts(&code, 4, &starts),
        None,
        "the `ifnonnull` at 4 follows `getfield #42`, whose trailing 0x2A \
         is an operand byte and not an `aload_0`"
    );
    assert_eq!(
        array_receiver_local_with_starts(&code, 11, &starts),
        Some(1),
        "the `aaload` at 11 takes its receiver from the `aload_1` at 7, \
         across a 3-byte `sipush` whose trailing 0x03 is an operand byte \
         and not an `iconst_0`"
    );
}

/// A `starts` map built for a DIFFERENT, shorter code array must not be
/// able to index out of range.
///
/// This is the failure mode that makes the bound checks non-negotiable
/// rather than defensive: these helpers run on a compile thread, the
/// module denies `clippy::panic`, and an out-of-range `starts[pc]` would
/// take the process down rather than decline an optimisation. Threading a
/// map as a parameter is what makes a mismatched length expressible at
/// all, so the test arrives with the parameter.
///
/// The required degradation is one-directional: a short map may only turn
/// `Some` into `None` — keep the runtime null check — never the reverse,
/// and never a panic.
#[test]
fn a_map_from_a_shorter_code_array_cannot_index_out_of_bounds() {
    let code = collision_body();
    // A map for the first three bytes only: `[aload_0, getfield-opcode,
    // cp-index-high]`, length 3 against a 13-byte body.
    let short = bytecode_analysis::instruction_starts(&code[..3], 3);
    assert_eq!(short.len(), 3);

    for pc in 0..code.len() + 4 {
        // The assertion is that these two calls RETURN. Every pc from 3
        // upward indexes past `short`.
        let prec = preceding_aload_nonnull_local_with_starts(&code, pc, &short);
        let recv = array_receiver_local_with_starts(&code, pc, &short);

        if pc >= short.len() + 2 {
            assert_eq!(
                prec, None,
                "pc {pc} is beyond the short map's reach; the only sound \
                 answer is `None`"
            );
        }
        if pc >= short.len() {
            assert_eq!(
                recv, None,
                "pc {pc} is beyond the short map's reach; the only sound \
                 answer is `None`"
            );
        }
    }

    // And the degradation really is a degradation: the same pc that the
    // correct map proves is refused under the short one.
    let full = bytecode_analysis::instruction_starts(&code, code.len());
    assert_eq!(array_receiver_local_with_starts(&code, 11, &full), Some(1));
    assert_eq!(array_receiver_local_with_starts(&code, 11, &short), None);
}
