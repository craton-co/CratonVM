/**
 * Does the arith-LICM pre-header actually execute on every entry into a
 * hoisted loop?
 *
 * `shapeA` is the classic javac `for` shape (`goto COND` at the top, back edge
 * from the bottom condition to the body) — the loop header (= back-edge
 * target) has NO fall-through predecessor at all.
 *
 * `shapeB` is the shape `AttributesImpl.ensureCapacity` has: a loop whose
 * header is reached both by fall-through and by a forward `goto` from an
 * earlier branch — the forward `goto` skips the pre-header.
 */
public class LicmEntryProbe {

    // classic for-loop: invariant `base*3+11` recomputed in the body
    static int shapeA(int base, int n) {
        int sum = 0;
        for (int i = 0; i < n; i++) {
            sum += base * 3 + 11 + (i & 1);
        }
        return sum;
    }

    // ensureCapacity shape: two predecessors of the loop header, one of them a
    // forward goto that bypasses the fall-through path.
    static int shapeB(int n, boolean takeShortcut) {
        int max;
        if (takeShortcut) {
            max = 25;
        } else {
            max = 30;
        }
        while (max < n * 5) {
            max *= 2;
        }
        return max;
    }

    public static void main(String[] args) {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 200000;
        int badA = 0, badB = 0;
        for (int k = 0; k < iterations; k++) {
            int a = shapeA(7, 10);
            if (a != 325) {
                badA++;
                if (badA <= 3) {
                    System.out.println("shapeA WRONG k=" + k + " got=" + a + " want=325");
                }
            }
            // n = 61 -> n*5 = 305; from 25: 25,50,100,200,400 -> 400
            int b = shapeB(61, true);
            if (b != 400) {
                badB++;
                if (badB <= 3) {
                    System.out.println("shapeB WRONG k=" + k + " got=" + b + " want=400");
                }
            }
            // from 30: 30,60,120,240,480 -> 480
            int c = shapeB(61, false);
            if (c != 480) {
                badB++;
                if (badB <= 3) {
                    System.out.println("shapeB(false) WRONG k=" + k + " got=" + c + " want=480");
                }
            }
        }
        System.out.println("DONE badA=" + badA + " badB=" + badB);
        if (badA + badB > 0) {
            System.exit(1);
        }
    }
}
