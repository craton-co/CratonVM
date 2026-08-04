import java.math.BigDecimal;
import java.math.RoundingMode;
import java.util.HashMap;
import java.util.Map;

/**
 * Reproduce, without Hibernate or H2, the value computation that
 * org.hibernate.orm.test.batch.BatchTest.doBatchInsertUpdate performs per row:
 *
 *   dp.setX( new BigDecimal( i * 0.1d ).setScale( 19, RoundingMode.DOWN ) );
 *   dp.setY( BigDecimal.valueOf( Math.cos( dp.getX().doubleValue() ) )
 *                .setScale( 19, RoundingMode.DOWN ) );
 *
 * BatchTest fails with a unique-index violation on DataPoint(xval,yval) under
 * JIT and never under --nojit. The constraint can only fire if two rows carry
 * the SAME (x,y) pair, so before blaming the JDBC batch path, check whether the
 * pair the loop computes is even correct: a JIT defect in the double local
 * (i * 0.1d), in the int->double conversion, or in the BigDecimal round-trip
 * would produce a duplicate (x,y) with no batching involved at all.
 *
 * The values are recomputed from scratch in a helper each iteration so the
 * result cannot come from a cached object, and every value is checked against
 * the reference the same expression produces on a freshly-parsed constant path.
 *
 * Prints DUP / MISMATCH lines and exits 1 if anything diverges; exits 0 silent
 * (apart from the summary) when the loop is faithful.
 */
public class BatchXyProbe {

    /** The exact per-row computation from BatchTest, via the same accessors. */
    static final class Point {
        private BigDecimal x;
        private BigDecimal y;

        void setX(BigDecimal x) {
            this.x = x;
        }

        BigDecimal getX() {
            return x;
        }

        void setY(BigDecimal y) {
            this.y = y;
        }

        BigDecimal getY() {
            return y;
        }
    }

    static Point makePoint(int i, int scale) {
        Point dp = new Point();
        dp.setX(new BigDecimal(i * 0.1d).setScale(scale, RoundingMode.DOWN));
        dp.setY(BigDecimal.valueOf(Math.cos(dp.getX().doubleValue())).setScale(scale, RoundingMode.DOWN));
        return dp;
    }

    /**
     * The same value, computed without the object or the accessors, so a
     * divergence tells us whether the defect is in the arithmetic or in the
     * field round-trip.
     */
    static String flat(int i, int scale) {
        double xd = i * 0.1d;
        BigDecimal x = new BigDecimal(xd).setScale(scale, RoundingMode.DOWN);
        BigDecimal y = BigDecimal.valueOf(Math.cos(x.doubleValue())).setScale(scale, RoundingMode.DOWN);
        return x + "|" + y;
    }

    public static void main(String[] args) {
        final int nEntities = args.length > 0 ? Integer.parseInt(args[0]) : 50;
        final int reps = args.length > 1 ? Integer.parseInt(args[1]) : 200000;
        final int scale = 19;

        // Reference table, built once while everything is still interpreted.
        String[] expected = new String[nEntities];
        for (int i = 0; i < nEntities; i++) {
            expected[i] = flat(i, scale);
        }
        // The premise the unique index rests on: x is unique per row by
        // construction. If this fails the test itself is wrong, not the VM.
        Map<String, Integer> premise = new HashMap<>();
        for (int i = 0; i < nEntities; i++) {
            Integer prev = premise.put(expected[i], i);
            if (prev != null) {
                System.out.println("PREMISE-BROKEN i=" + i + " duplicates i=" + prev + " " + expected[i]);
                System.exit(2);
            }
        }

        long bad = 0;
        long checked = 0;
        for (int rep = 0; rep < reps; rep++) {
            Map<String, Integer> seen = new HashMap<>();
            for (int i = 0; i < nEntities; i++) {
                Point dp = makePoint(i, scale);
                String key = dp.getX() + "|" + dp.getY();
                checked++;
                if (!key.equals(expected[i])) {
                    bad++;
                    if (bad <= 20) {
                        System.out.println("MISMATCH rep=" + rep + " i=" + i
                                + " got=" + key + " want=" + expected[i]);
                    }
                }
                Integer prev = seen.put(key, i);
                if (prev != null) {
                    bad++;
                    if (bad <= 20) {
                        System.out.println("DUP rep=" + rep + " i=" + i
                                + " collides with i=" + prev + " value=" + key);
                    }
                }
                // Same values again through the flat path, so a divergence
                // between the two localizes the defect.
                String f = flat(i, scale);
                checked++;
                if (!f.equals(expected[i])) {
                    bad++;
                    if (bad <= 20) {
                        System.out.println("FLAT-MISMATCH rep=" + rep + " i=" + i
                                + " got=" + f + " want=" + expected[i]);
                    }
                }
            }
        }
        System.out.println("BatchXyProbe nEntities=" + nEntities + " reps=" + reps
                + " checked=" + checked + " bad=" + bad);
        if (bad != 0) {
            System.exit(1);
        }
    }
}
