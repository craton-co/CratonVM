use super::*;
use crate::deopt::DespecRegistry;
use crate::ir::IrBuilder;

/// `static int f(int[] a, int n, int m) { int o = 0; for (int j = 0; j <
/// m; j++) { int s = j; for (int i = 0; i < n; i++) s = s * 3 + a[i]; o +=
/// s; } return o; }`, as javac lays it out: the outer header at 5, the
/// inner one at 18.
const NESTED: [u8; 54] = [
    0x03, // 0: iconst_0
    0x3e, // 1: istore_3
    0x03, // 2: iconst_0
    0x36, 0x04, // 3: istore 4
    0x15, 0x04, // 5: iload 4
    0x1c, // 7: iload_2
    0xa2, 0x00, 0x2c, // 8: if_icmpge 52
    0x15, 0x04, // 11: iload 4
    0x36, 0x05, // 13: istore 5
    0x03, // 15: iconst_0
    0x36, 0x06, // 16: istore 6
    0x15, 0x06, // 18: iload 6
    0x1b, // 20: iload_1
    0xa2, 0x00, 0x14, // 21: if_icmpge 41
    0x15, 0x05, // 24: iload 5
    0x06, // 26: iconst_3
    0x68, // 27: imul
    0x2a, // 28: aload_0
    0x15, 0x06, // 29: iload 6
    0x2e, // 31: iaload
    0x60, // 32: iadd
    0x36, 0x05, // 33: istore 5
    0x84, 0x06, 0x01, // 35: iinc 6, 1
    0xa7, 0xff, 0xec, // 38: goto 18
    0x1d, // 41: iload_3
    0x15, 0x05, // 42: iload 5
    0x60, // 44: iadd
    0x3e, // 45: istore_3
    0x84, 0x04, 0x01, // 46: iinc 4, 1
    0xa7, 0xff, 0xd4, // 49: goto 5
    0x1d, // 52: iload_3
    0xac, // 53: ireturn
];

const KEY: &str = "T.f:([III)I";
const INNER_HEADER: usize = 18;

/// Built and optimized as a method-entry compile with a de-spec registry
/// (so the predicates plant), then verified as `post-optimize` is.
fn optimized(
    overrides: &[(&str, Option<&str>)],
) -> Option<(Graph, crate::bailout::CompileResult<()>)> {
    cratonvm_types::flags::with_thread_overrides(overrides, || {
        let mut b = IrBuilder::new(3, 7);
        b.set_param_types(&[IrType::Ref, IrType::Int, IrType::Int]);
        let mut g = b.build(&NESTED, NESTED.len())?;
        let despec = DespecRegistry::new();
        optimize_with(
            &mut g,
            OptimizeContext {
                despec: Some(&despec),
                method_key: KEY,
                osr_route: false,
            },
        );
        let verdict =
            crate::ir_verify::verify_graph_at_phase(&g, crate::ir_verify::PHASE_POST_OPTIMIZE);
        Some((g, verdict))
    })
}

fn at_inner_header(g: &Graph) -> usize {
    g.safepoints
        .iter()
        .filter(|sp| !sp.post_call_only && sp.bci == INNER_HEADER)
        .count()
}

