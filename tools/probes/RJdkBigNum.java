import java.math.BigDecimal;
import java.math.BigInteger;
import java.math.RoundingMode;

public class RJdkBigNum {
    static int checks = 0;

    static void guarded(String label, java.util.function.Supplier<Object> s) {
        checks++;
        String out;
        try {
            Object o = s.get();
            out = (o == null) ? "<null>" : String.valueOf(o);
        } catch (Throwable t) {
            out = "!! " + t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(label + " = " + out);
    }

    static BigDecimal bd(long u, int s) {
        return new BigDecimal(BigInteger.valueOf(u), s);
    }

    public static void main(String[] args) {
        final int MIN = Integer.MIN_VALUE;
        final int MAX = Integer.MAX_VALUE;

        // --- multiply: the product scale is checkScale(scale1 + scale2), and
        //     the RECEIVER's zeroness is what exempts. G10-1 section 5.1.
        guarded("mul.1", () -> bd(1, MAX).multiply(bd(1, MAX)));
        guarded("mul.2", () -> bd(1, MAX).multiply(bd(1, 1)));
        guarded("mul.3", () -> bd(1, MAX).multiply(bd(0, MAX)));
        guarded("mul.4", () -> bd(0, MAX).multiply(bd(1, MAX)).scale());
        guarded("mul.5", () -> bd(1, MIN).multiply(bd(1, MIN)));
        guarded("mul.6", () -> bd(1, MIN).multiply(bd(1, -1)));
        guarded("mul.7", () -> bd(0, MIN).multiply(bd(1, MIN)).scale());
        guarded("mul.8", () -> bd(1, MAX).multiply(bd(1, MIN)).scale());
        guarded("mul.9", () -> bd(1, 1073741824).multiply(bd(1, 1073741824)));

        // --- doubleValue / floatValue must not render the scale. Every one of
        //     these answers in 0 ms on HotSpot. G10-1 section 5.2.
        int[] ds = { MIN, MIN + 1, -400, -309, -308, -1, 0, 1, 308, 309, 323, 324, 325, MAX };
        long[] us = { 1L, -1L, 0L, 15L, -15L, 9007199254740993L };
        for (long u : us) {
            for (int s : ds) {
                guarded("dv." + u + "." + s, () -> bd(u, s).doubleValue());
                guarded("fv." + u + "." + s, () -> bd(u, s).floatValue());
            }
        }
        guarded("dv.max", () -> bd(17976931348623157L, -292).doubleValue());
        guarded("dv.inf", () -> bd(17976931348623159L, -292).doubleValue());

        // --- setScale over every RoundingMode at the scales that overflow when
        //     negated. Deliberately EXCLUDES 715827883, which HotSpot spends
        //     ~90 s on before OutOfMemoryError. G10-1 section 4.1 / section 11.4.
        for (RoundingMode rm : RoundingMode.values()) {
            for (int sc : new int[] { MIN, MIN + 1, -715827884, -715827883, -3, -1, 0, 2, MAX - 1, MAX }) {
                guarded("ss." + rm + "." + sc, () -> new BigDecimal("1.5").setScale(sc, rm));
                guarded("ss0." + rm + "." + sc, () -> BigDecimal.ZERO.setScale(sc, rm));
            }
        }
        for (int m : new int[] { -1, 0, 7, 8, 99, MIN, MAX }) {
            guarded("ssm." + m, () -> new BigDecimal("1.5").setScale(0, m));
            guarded("ssm0." + m, () -> bd(0, 1).setScale(1, m));
        }

        // --- the rounding table, all 8 modes.
        String[] halves = { "2.5", "-2.5", "1.5", "-1.5", "0.5", "-0.5", "2.4", "-2.4", "2.6", "-2.6", "0.0" };
        for (RoundingMode rm : RoundingMode.values()) {
            for (String v : halves) {
                guarded("rt." + rm + "." + v, () -> new BigDecimal(v).setScale(0, rm));
            }
        }

        // --- toBigInteger / intValue / longValue / toPlainString at the extremes.
        guarded("tbi.1", () -> bd(1, MIN).toBigInteger());
        guarded("tbi.2", () -> bd(1, MIN + 1).toBigInteger());
        guarded("tbi.3", () -> bd(1, MAX).toBigInteger());
        guarded("tbi.4", () -> bd(0, MIN).toBigInteger());
        guarded("iv.1", () -> bd(1, MIN).intValue());
        guarded("lv.1", () -> bd(1, MIN).longValue());
        guarded("tps.1", () -> bd(1, MIN).toPlainString());
        guarded("tps.2", () -> bd(1, MAX).toPlainString());

        // --- toString's three roads. toPlainString and toEngineeringString
        //     differ from toString and from each other, on purpose.
        long[][] tv = { {0,0},{0,1},{0,-1},{0,5},{0,-5},{1,0},{1,6},{1,7},{1,-1},{1,-6},
                        {123,5},{123,-5},{-123,-5},{10,3},{100,3},{1000,3},{12,-20} };
        for (long[] r : tv) {
            final long u = r[0]; final int s = (int) r[1];
            guarded("ts." + u + "." + s, () -> bd(u, s).toString());
            guarded("tp." + u + "." + s, () -> bd(u, s).toPlainString());
            guarded("te." + u + "." + s, () -> bd(u, s).toEngineeringString());
        }
        guarded("neg.1", () -> bd(1, MIN).negate().scale());
        guarded("prec.1", () -> bd(1, MIN).precision());
        guarded("bdnull", () -> new BigDecimal((BigInteger) null));

        // --- BigInteger identity: null and wrong-typed arguments.
        //     G10-1 section 5.5.
        guarded("bi.eq.null", () -> BigInteger.ONE.equals(null));
        guarded("bi.eq.str", () -> BigInteger.ONE.equals("1"));
        guarded("bi.eq.int", () -> BigInteger.ONE.equals(Integer.valueOf(1)));
        guarded("bi.eq.self", () -> BigInteger.ONE.equals(BigInteger.valueOf(1)));
        guarded("bi.cmp.null", () -> BigInteger.ONE.compareTo(null));
        guarded("bi.max.null", () -> BigInteger.ONE.max(null));

        // --- BigInteger shifts: a negative distance flips direction and is
        //     then read UNSIGNED. G10-1 section 3.1.
        String[] sv = { "0", "1", "-1", "-2", "3", "-3" };
        int[] sh = { 0, 1, 31, 32, 33, -1, -31, -32, -33, MIN, MIN + 1, MAX };
        for (String v : sv) {
            for (int n : sh) {
                guarded("shl." + v + "." + n, () -> summarize(new BigInteger(v).shiftLeft(n)));
                guarded("shr." + v + "." + n, () -> summarize(new BigInteger(v).shiftRight(n)));
            }
        }

        // --- mod / remainder / divide / modPow / modInverse refusals.
        guarded("bi.divz", () -> BigInteger.ONE.divide(BigInteger.ZERO));
        guarded("bi.remz", () -> BigInteger.ONE.remainder(BigInteger.ZERO));
        guarded("bi.modz", () -> BigInteger.ONE.mod(BigInteger.ZERO));
        guarded("bi.modneg", () -> new BigInteger("7").mod(new BigInteger("-3")));
        guarded("bi.mod.sign", () -> new BigInteger("-7").mod(new BigInteger("3")));
        guarded("bi.rem.sign", () -> new BigInteger("-7").remainder(new BigInteger("3")));
        guarded("bi.mp.0", () -> new BigInteger("3").modPow(new BigInteger("2"), BigInteger.ZERO));
        guarded("bi.mp.neg", () -> new BigInteger("3").modPow(new BigInteger("-1"), new BigInteger("7")));
        guarded("bi.mp.ninv", () -> new BigInteger("2").modPow(new BigInteger("-1"), new BigInteger("4")));
        guarded("bi.mi.0", () -> new BigInteger("3").modInverse(BigInteger.ZERO));
        guarded("bi.mi.ninv", () -> new BigInteger("2").modInverse(new BigInteger("4")));
        guarded("bi.pow.neg", () -> BigInteger.TWO.pow(-1));
        guarded("bi.pow.zero", () -> BigInteger.ZERO.pow(0));
        guarded("bi.pow.one", () -> BigInteger.ONE.pow(MAX));
        guarded("bi.sqrt.neg", () -> new BigInteger("-4").sqrt());
        guarded("bi.tb.neg", () -> BigInteger.ONE.testBit(-1));
        guarded("bi.tb.max", () -> BigInteger.ONE.testBit(MAX));
        guarded("bi.tb.max.neg", () -> BigInteger.valueOf(-1).testBit(MAX));

        System.out.println("RJdkBigNum checks=" + checks);
    }

    static String summarize(BigInteger b) {
        String s = b.toString();
        if (s.length() > 60) {
            return "<signum=" + b.signum() + " bitLength=" + b.bitLength() + ">";
        }
        return s;
    }
}
