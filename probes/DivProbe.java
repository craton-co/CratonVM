/**
 * Division/remainder semantics under the optimizing tier: every case the
 * lowerer special-cases (zero divisor, MIN/-1 overflow, negative operands) on
 * both widths, plus the *speculative* shape this session fixed — a division
 * that only one arm of a branch reaches, whose other arm leaves the divisor at
 * zero. Prints a checksum so CratonVM and HotSpot can be diffed exactly.
 */
public final class DivProbe {
    static int idiv(int a, int b) {
        return a / b;
    }

    static int irem(int a, int b) {
        return a % b;
    }

    static long ldiv(long a, long b) {
        return a / b;
    }

    static long lrem(long a, long b) {
        return a % b;
    }

    /** The `MemoryEstimator.estimateMemory` shape: the divisor is only non-zero
     *  on the arm that actually performs the division. */
    static long speculative(long flag, int counter, long sum, long delta) {
        int c = counter;
        if (flag == 0 || c-- == 0) {
            if (flag == 0) {
                if (++c == 256) {
                    flag = 1L << 24;
                }
                return (sum * c + delta + (c >> 1)) / c;
            }
            return sum + delta + c;
        }
        return sum - delta;
    }

    static int specInt(int flag, int counter, int sum) {
        int c = counter;
        if (flag == 0 || c-- == 0) {
            if (flag == 0) {
                ++c;
                return (sum * c + (c >> 1)) % c;
            }
            return sum + c;
        }
        return sum - c;
    }

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 300_000;
        long acc = 0;
        int thrown = 0;
        int[] ia = {0, 1, -1, 7, -7, 256, Integer.MIN_VALUE, Integer.MAX_VALUE};
        long[] la = {0L, 1L, -1L, 7L, -7L, 1L << 40, Long.MIN_VALUE, Long.MAX_VALUE};
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < ia.length; i++) {
                for (int j = 0; j < ia.length; j++) {
                    try {
                        acc = acc * 31 + idiv(ia[i], ia[j]);
                    } catch (ArithmeticException e) {
                        thrown++;
                    }
                    try {
                        acc = acc * 31 + irem(ia[i], ia[j]);
                    } catch (ArithmeticException e) {
                        thrown++;
                    }
                    try {
                        acc = acc * 31 + ldiv(la[i], la[j]);
                    } catch (ArithmeticException e) {
                        thrown++;
                    }
                    try {
                        acc = acc * 31 + lrem(la[i], la[j]);
                    } catch (ArithmeticException e) {
                        thrown++;
                    }
                }
            }
            for (int f = 0; f < 2; f++) {
                for (int c = 0; c < 4; c++) {
                    try {
                        acc = acc * 31 + speculative(f == 0 ? 0L : (1L << 24), c, 24766, 9026);
                    } catch (ArithmeticException e) {
                        thrown++;
                    }
                    try {
                        acc = acc * 31 + specInt(f, c, 24766);
                    } catch (ArithmeticException e) {
                        thrown++;
                    }
                }
            }
        }
        System.out.println("acc=" + acc + " thrown=" + thrown);
    }
}
