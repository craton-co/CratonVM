// AUDIT 2026-07-11: fixture for the `lcmp`/`fcmpl`/`fcmpg`/`dcmpl`/
// `dcmpg` (0x94-0x98) admission change. Each method below is the
// idiomatic, real-javac-compiled 3-way compare-and-branch pattern —
// exactly the shape every Java `compareTo`-style method reduces to,
// since Java has no source-level operator that produces a raw -1/0/1
// comparison value without a branch.
//
// `compareLongs` compiles to `lload; lload; lcmp; if<cond>` (twice).
// `compareFloats` compiles to `fload; fload; fcmpg; ifge` (for `a < b`,
// so a NaN operand — which fcmpg reports as `+1` — does NOT take the
// branch) and `fload; fload; fcmpl; ifle` (for `a > b`, mirrored) —
// javac's standard choice of which cmp variant to pair with which
// relational operator, per JLS §15.20.1. `compareDoubles` is the same
// shape with `dcmpg`/`dcmpl`.
//
// Before jit-cuda's 0x94-0x98 admission fix, `analyzer::analyze` on any
// of these rejected immediately at the first `*cmp*` opcode with
// `Reason::Compare`. After the fix, `classify` admits `lcmp`/`fcmpl`/
// `fcmpg`/`dcmpl`/`dcmpg` unconditionally (the PTX lowering is bit-exact
// for every input), so each method below is now analyzer-`Eligible` —
// but `lower_method` still refuses it, one layer later, at the `if<cond>`
// that immediately follows every `*cmp*` here: general `if*` branches
// outside the canonical counted-loop guard position are unchanged and
// still reject. See `jit-cuda/src/lowering.rs`'s
// `compare_branch_fusion_*_is_eligible_then_rejected_at_lowering` tests
// for the pinned before/after transition.
public class CompareBranchFusion {
    public static int compareLongs(long a, long b) {
        if (a < b) {
            return -1;
        }
        if (a > b) {
            return 1;
        }
        return 0;
    }

    public static int compareFloats(float a, float b) {
        if (a < b) {
            return -1;
        }
        if (a > b) {
            return 1;
        }
        return 0;
    }

    public static int compareDoubles(double a, double b) {
        if (a < b) {
            return -1;
        }
        if (a > b) {
            return 1;
        }
        return 0;
    }
}
