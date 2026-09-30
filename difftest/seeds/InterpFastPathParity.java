// difftest: strict
//
// Fast-path / decoded-path parity for the opcode families the raw-bytecode
// interpreter arm gained in the 2026-08-18 interpreter audit.
//
// The interpreter has two implementations of the same opcodes (see the
// `interp-decoded` note in `difftest/src/main.rs`): the raw-byte fast path,
// and the decoded `Instruction` handler that `-Xverify:none` selects for all
// of them at once. Until this audit the fast path had no arm for the long
// shifts, `fneg`/`dneg`, `frem`/`drem`, eight of the twelve primitive
// conversions, the four float/double comparisons, six of the seven operand
// stack shuffles, `ifnull`/`ifnonnull` or `if_acmpeq`/`if_acmpne` — so those
// opcodes had exactly one implementation and this axis had nothing to compare.
// Now they have two, and every value below has to come out the same in
// `jit-on`, `nojit` and `interp-decoded`, and match HotSpot in all three.
//
// The interesting inputs are the ones where the two paths could plausibly
// disagree: NaN (which comparison opcode's `l`/`g` suffix answers), the
// saturating float→integer narrowings (JVMS §2.8.3), shift distances above the
// operand width, signed zero, and a category-2 value moving through a shuffle.
public class InterpFastPathParity {

    static final class Box {
        int v;
        long lv;
        double dv;
        Box(int v) { this.v = v; }
    }

    public static void main(String[] args) {
        longShifts();
        floatDoubleNegRem();
        conversions();
        comparisons();
        shuffles();
        referenceBranches();
        backEdgeShapes();
    }

    // lshl / lshr / lushr — distance masked to the low 6 bits.
    private static void longShifts() {
        long v = 0x0123_4567_89AB_CDEFL;
        long n = -1L;
        System.out.println("lshl: " + (v << 1) + " " + (v << 63) + " " + (v << 64) + " " + (v << 65));
        System.out.println("lshr: " + (v >> 4) + " " + (n >> 4) + " " + (n >> 64) + " " + (n >> 63));
        System.out.println("lushr: " + (v >>> 4) + " " + (n >>> 4) + " " + (n >>> 64) + " " + (n >>> 63));
        // The 64-bit mixer shape that made the missing arms worth closing.
        long h = 0x9E3779B97F4A7C15L;
        h ^= h >>> 33;
        h *= 0xFF51AFD7ED558CCDL;
        h ^= h >>> 33;
        System.out.println("mix: " + h);
    }

    // fneg / dneg / frem / drem.
    private static void floatDoubleNegRem() {
        float fz = 0.0f;
        double dz = 0.0d;
        float fn = Float.NaN;
        double dn = Double.NaN;
        System.out.println("fneg: " + (-fz) + " " + Float.floatToRawIntBits(-fz) + " " + (-3.5f));
        System.out.println("dneg: " + (-dz) + " " + Double.doubleToRawLongBits(-dz) + " " + (-3.5d));
        System.out.println("fneg nan: " + Float.isNaN(-fn));
        System.out.println("dneg nan: " + Double.isNaN(-dn));
        System.out.println("frem: " + (7.5f % 2.0f) + " " + (-7.5f % 2.0f) + " " + (7.5f % -2.0f));
        System.out.println("drem: " + (7.5d % 2.0d) + " " + (-7.5d % 2.0d) + " " + (7.5d % -2.0d));
        System.out.println("frem inf: " + (Float.POSITIVE_INFINITY % 2.0f) + " " + (2.0f % 0.0f));
        System.out.println("drem inf: " + (Double.POSITIVE_INFINITY % 2.0d) + " " + (2.0d % 0.0d));
    }

    // l2f l2d f2i f2l f2d d2i d2l d2f — including the §2.8.3 saturations.
    private static void conversions() {
        long big = 9007199254740993L; // 2^53 + 1, not representable as a double
        long lmax = Long.MAX_VALUE;
        System.out.println("l2f: " + ((float) big) + " " + ((float) lmax));
        System.out.println("l2d: " + ((double) big) + " " + ((double) lmax));

        float[] fs = { 3.9f, -3.9f, Float.NaN, Float.POSITIVE_INFINITY, Float.NEGATIVE_INFINITY, 1e30f };
        for (int i = 0; i < fs.length; i++) {
            System.out.println("f2i/f2l/f2d " + i + ": " + ((int) fs[i]) + " " + ((long) fs[i]) + " " + ((double) fs[i]));
        }
        double[] ds = { 3.9d, -3.9d, Double.NaN, Double.POSITIVE_INFINITY, Double.NEGATIVE_INFINITY, 1e300d };
        for (int i = 0; i < ds.length; i++) {
            System.out.println("d2i/d2l/d2f " + i + ": " + ((int) ds[i]) + " " + ((long) ds[i]) + " " + ((float) ds[i]));
        }
    }

    // fcmpl / fcmpg / dcmpl / dcmpg. `<` and `>` against a NaN operand pick
    // different suffixes, so both answers are observed here.
    private static void comparisons() {
        float[] fs = { 1.0f, 2.0f, Float.NaN, -0.0f, 0.0f };
        for (int i = 0; i < fs.length; i++) {
            for (int j = 0; j < fs.length; j++) {
                float a = fs[i];
                float b = fs[j];
                System.out.println("fcmp " + i + "," + j + ": " + (a < b) + (a > b) + (a <= b) + (a >= b) + (a == b));
            }
        }
        double[] ds = { 1.0d, 2.0d, Double.NaN, -0.0d, 0.0d };
        for (int i = 0; i < ds.length; i++) {
            for (int j = 0; j < ds.length; j++) {
                double a = ds[i];
                double b = ds[j];
                System.out.println("dcmp " + i + "," + j + ": " + (a < b) + (a > b) + (a <= b) + (a >= b) + (a == b));
            }
        }
    }

    // pop2 / dup_x1 / dup_x2 / dup2 / dup2_x1 / dup2_x2, via the source idioms
    // javac lowers to them. A category-2 value moving through a shuffle is the
    // case a bit-copy gets right and a `Value` round-trip can lose.
    private static void shuffles() {
        int[] ia = new int[4];
        long[] la = new long[4];
        double[] da = new double[4];

        // dup2 (arrayref, index) around a compound array assignment.
        for (int i = 0; i < 4; i++) {
            ia[i] = i;
            ia[i] += 10;
            la[i] = i;
            la[i] += 100L;
            da[i] = i;
            da[i] += 0.5d;
        }
        // dup2_x2 — a long/double compound array assignment stores a cat-2
        // value back under a two-slot (arrayref, index) pair.
        la[1] *= 3L;
        da[1] *= 2.5d;
        // dup_x1 / dup_x2 — chained assignment through fields and arrays.
        Box p = new Box(0);
        Box q = new Box(0);
        p.v = q.v = 7;
        ia[0] = ia[1] = 21;
        // dup2_x1 — a cat-2 value stored through a chained assignment.
        long lv;
        la[0] = lv = 42L;
        // dup2_x1 proper — a compound assignment to a cat-2 INSTANCE field
        // whose result is used leaves (objectref, value-hi, value-lo) on the
        // stack; dup2_x2 is the same shape over an (arrayref, index) pair.
        Box r = new Box(0);
        long fieldLong = (r.lv += 5L);
        double fieldDouble = (r.dv += 1.5d);
        long elemLong = (la[2] += 7L);
        double elemDouble = (da[2] += 2.5d);
        System.out.println("cat2 chains: " + fieldLong + " " + fieldDouble
                + " " + elemLong + " " + elemDouble);

        System.out.println("ia: " + ia[0] + " " + ia[1] + " " + ia[2] + " " + ia[3]);
        System.out.println("la: " + la[0] + " " + la[1] + " " + la[2] + " " + la[3]);
        System.out.println("da: " + da[0] + " " + da[1] + " " + da[2] + " " + da[3]);
        System.out.println("boxes: " + p.v + " " + q.v + " lv=" + lv);

        // pop2 — a cat-2 expression statement whose value is discarded.
        popTwo();
    }

    private static long counter = 0;

    private static long bump() {
        counter += 1;
        return counter;
    }

    private static void popTwo() {
        bump(); // long result discarded -> pop2
        bump();
        System.out.println("counter: " + counter);
    }

    // ifnull / ifnonnull / if_acmpeq / if_acmpne as plain (forward) branches.
    private static void referenceBranches() {
        Box a = new Box(1);
        Box b = new Box(1);
        Box n = null;
        StringBuilder sb = new StringBuilder();
        sb.append(a == null).append(a != null);
        sb.append(n == null).append(n != null);
        sb.append(a == b).append(a != b);
        sb.append(a == a).append(a != a);
        sb.append(a == n).append(n == a);
        System.out.println("refbranches: " + sb);
    }

    // The loop shapes whose back edge is a reference branch rather than a
    // `goto` or an int comparison: `do { … } while (p != null)` and
    // `do { … } while (o != sentinel)`. Before this audit neither incremented
    // `Frame::backward_count`, so neither could reach the OSR threshold nor
    // earn loop-work tier-up credit. The values printed are the same either
    // way — what changes is whether the loop is visible to tier-up — so this
    // seed's job is to prove the new arms did not change the ANSWER.
    private static void backEdgeShapes() {
        Box[] chain = new Box[512];
        for (int i = 0; i < chain.length; i++) {
            chain[i] = new Box(i);
        }
        Object sentinel = chain[chain.length - 1];

        // Back edge: ifnonnull.
        int idx = 0;
        long sum = 0;
        Box cur = chain[0];
        do {
            sum += cur.v;
            idx++;
            cur = idx < chain.length ? chain[idx] : null;
        } while (cur != null);

        // Back edge: if_acmpne.
        int j = 0;
        long sum2 = 0;
        Object o;
        do {
            o = chain[j];
            sum2 += ((Box) o).v;
            j++;
        } while (o != sentinel);

        // Back edge: ifnull.
        int k = 0;
        long sum3 = 0;
        Box seek = null;
        do {
            seek = (k == chain.length - 1) ? chain[k] : null;
            sum3 += k;
            k++;
        } while (seek == null);

        System.out.println("backedges: " + sum + " " + sum2 + " " + sum3);
    }
}
