// Repro for the JIT canonicalize_stack operand-clobber miscompile.
//
// Shape: a register-allocated (CalleeSaved) local pushed BELOW two
// frame-resident (spilled) int locals feeding an if_icmp / ifXX with a
// forward target. The branch handler pops the operands and THEN calls
// canonicalize_stack() on the remaining stack; the CalleeSaved slot is
// stored to canonical slot 0 = base_spill+0, which is exactly where the
// first popped operand still lives -> the compare reads the register
// local's bits instead of the operand.
//
// k1..k7 are hot (heavy mixing use) -> graph-coloring assigns them the 7
// Windows callee-saved GPRs. a..e are cold (2 reads each) -> spilled to
// frame slots. Each probe line P1..P8 builds [CalleeSaved, Frame(, Frame)]
// at the conditional.
//
//   javac CanonClobber.java
//   java CanonClobber 2000          (HotSpot reference)
//   cratonvm -c . CanonClobber 2000 (JIT: miscompiles before the fix)
//   cratonvm --nojit -c . CanonClobber 2000
public class CanonClobber {
    static int trial(int seed) {
        int k1 = seed + 1, k2 = seed + 2, k3 = seed + 3, k4 = seed + 4;
        int k5 = seed + 5, k6 = seed + 6, k7 = seed + 7;
        int a = seed & 7, b = (seed >> 1) & 7, c = (seed & 3) - 1;
        int d = seed | 1, e = seed ^ 3;

        // heat the k locals so they win registers
        k1 = k1 * 31 + k2; k2 = k2 * 31 + k3; k3 = k3 * 31 + k4;
        k4 = k4 * 31 + k5; k5 = k5 * 31 + k6; k6 = k6 * 31 + k7;
        k7 = k7 * 31 + k1;
        k1 = k1 * 31 + k7; k2 = k2 * 31 + k1; k3 = k3 * 31 + k2;
        k4 = k4 * 31 + k3; k5 = k5 * 31 + k4; k6 = k6 * 31 + k5;
        k7 = k7 * 31 + k6;

        int s;
        s  = k1 + (a < b ? 7 : 13);   // P1 if_icmpge: [CS, F, F]
        s += k2 + (b < a ? 7 : 13);   // P2 reversed operands
        s += k3 + (c > 0 ? 7 : 13);   // P3 ifle: [CS, F]
        s += k4 + (d > e ? 7 : 13);   // P4
        s += k5 + (e == d ? 7 : 13);  // P5 if_icmpne
        s += a + (k6 < k7 ? 7 : 13);  // P6 control: Frame below, reg operands
        s += k6 + (a < k7 ? 7 : 13);  // P7 mixed: val1 frame-resident
        s += k7 + (k1 < b ? 7 : 13);  // P8 mixed: val2 frame-resident

        // keep everything live to the end so all 12 interfere
        return s * 31 + (k1 ^ k2 ^ k3 ^ k4 ^ k5 ^ k6 ^ k7) + (a + b + c + d + e);
    }

    public static void main(String[] args) {
        int iters = Integer.parseInt(args[0]);
        int total = 0;
        for (int t = 0; t < iters; t++) {
            total = total * 31 + trial(t);
        }
        System.out.println("TOTAL " + total);
    }
}
