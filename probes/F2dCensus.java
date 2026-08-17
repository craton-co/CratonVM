/**
 * Self-contained census of f2d over NaN float bit patterns.
 *
 * The expected widening is arithmetic, not an oracle: a float NaN widens to the
 * double whose sign is the float's, whose exponent is all ones, whose top 23
 * mantissa bits are the float's 23, and whose quiet bit is set. So this probe
 * needs no HotSpot run to be meaningful — it checks the VM against the format.
 */
public final class F2dCensus {

    static long expect(int fbits) {
        long sign = ((long) (fbits >>> 31)) << 63;
        long mant = ((long) (fbits & 0x007fffff)) << 29;
        return sign | 0x7ff0000000000000L | mant | 0x0008000000000000L;
    }

    public static void main(String[] args) {
        int bad = 0;
        int total = 0;
        int firstBad = 0;
        StringBuilder samples = new StringBuilder();
        long state = 0x12345678L;
        // Every NaN pattern is exponent 0xFF with a non-zero mantissa.
        for (int i = 0; i < 200000; i++) {
            state ^= state << 13;
            state ^= state >>> 7;
            state ^= state << 17;
            int mant = (int) (state & 0x007fffff);
            if (mant == 0) {
                continue;
            }
            int sign = ((state >>> 40) & 1) != 0 ? 0x80000000 : 0;
            int fbits = sign | 0x7f800000 | mant;
            float f = Float.intBitsToFloat(fbits);
            long got = Double.doubleToRawLongBits((double) f);
            long want = expect(fbits);
            total++;
            if (got != want) {
                bad++;
                if (bad == 1) {
                    firstBad = fbits;
                }
                if (bad <= 6) {
                    samples.append("   in=").append(Integer.toHexString(fbits))
                           .append(" got=").append(Long.toHexString(got))
                           .append(" want=").append(Long.toHexString(want)).append('\n');
                }
            }
        }
        System.out.println("f2d NaN census: " + bad + " / " + total + " wrong");
        System.out.print(samples);

        if (bad > 0) {
            // Characterise: is it the sign bit, or a mantissa range?
            int negBad = 0;
            int negTotal = 0;
            int posBad = 0;
            int posTotal = 0;
            for (int m = 1; m < 0x800000; m += 0x1000) {
                for (int s = 0; s < 2; s++) {
                    int fbits = (s == 1 ? 0x80000000 : 0) | 0x7f800000 | m;
                    long got = Double.doubleToRawLongBits((double) Float.intBitsToFloat(fbits));
                    boolean ok = got == expect(fbits);
                    if (s == 1) {
                        negTotal++;
                        if (!ok) {
                            negBad++;
                        }
                    } else {
                        posTotal++;
                        if (!ok) {
                            posBad++;
                        }
                    }
                }
            }
            System.out.println("by sign: positive " + posBad + "/" + posTotal
                    + ", negative " + negBad + "/" + negTotal);
            System.out.println("first bad pattern = " + Integer.toHexString(firstBad));
        }
    }
}
