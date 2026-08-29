import java.math.BigInteger;

/** L8 tail — `java.math.BigInteger`, the largest single class left in the
 *  unowned `--jdk-only` surface: 24 bridge-with-code rows, every one of them a
 *  native shadowing real bytecode.
 *
 *  Arithmetic is the easiest surface in the campaign to sweep EXHAUSTIVELY,
 *  because it is pure: no clock, no locale, no file system, no allocation order
 *  a caller can see. So this probe does not sample. It takes a corpus chosen to
 *  put a value on both sides of every representation boundary CratonVM's
 *  implementation could plausibly have — the `int` and `long` limits, the
 *  32-bit word boundary the JDK's `mag[]` is built from, the sign-magnitude
 *  discontinuity at zero, a value whose two's-complement `toByteArray` needs a
 *  leading pad byte and one whose does not — and runs every ordered pair
 *  through every binary operation.
 *
 *  WHY EVERY ROW PRINTS A DECIMAL STRING. `toString()` is not in the 24 and is
 *  not shadowed, so it is the closest thing to a neutral rendering available.
 *  It is asked directly in §1 anyway: if it were wrong, every row below would
 *  differ at once, and §1 is what tells the two cases apart.
 *
 *  DETERMINISM. The one operation here that is randomised internally is
 *  `isProbablePrime`, whose Miller-Rabin witnesses come from an internal
 *  `Random`. A definite prime passes for every witness and a small composite is
 *  caught by trial division, so the corpus uses only values whose answer does
 *  not depend on the draw — and the whole probe is run twice on HotSpot to say
 *  so rather than to assume it.
 */
public class BigIntegerSweep {

    static int rows = 0;

    interface F {
        Object get() throws Throwable;
    }

    static final char[] HEX = "0123456789abcdef".toCharArray();

    static String esc(String s) {
        if (s == null) {
            return "null";
        }
        char[] out = new char[s.length() * 6];
        int n = 0;
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x20 || c > 0x7e) {
                out[n++] = '\\';
                out[n++] = 'u';
                out[n++] = HEX[(c >> 12) & 0xf];
                out[n++] = HEX[(c >> 8) & 0xf];
                out[n++] = HEX[(c >> 4) & 0xf];
                out[n++] = HEX[c & 0xf];
            } else {
                out[n++] = c;
            }
        }
        return new String(out, 0, n);
    }

    static void p(String tag, F f) {
        rows++;
        String v;
        try {
            v = String.valueOf(f.get());
        } catch (Throwable e) {
            v = "THREW " + e.getClass().getName() + ": " + e.getMessage();
        }
        System.out.print(esc(tag));
        System.out.print(" |");
        System.out.print(esc(v));
        System.out.println("|");
    }

    static void sect(String name, Runnable r) {
        try {
            r.run();
        } catch (Throwable e) {
            System.out.println("SECTION-ABORTED " + name + " " + e.getClass().getName());
        }
    }

    static String hex(byte[] b) {
        if (b == null) {
            return "null";
        }
        char[] out = new char[b.length * 2];
        for (int i = 0; i < b.length; i++) {
            out[i * 2] = HEX[(b[i] >> 4) & 0xf];
            out[i * 2 + 1] = HEX[b[i] & 0xf];
        }
        return new String(out);
    }

    // ------------------------------------------------------------- the corpus

    /** Named so a differing row says WHICH boundary it sits on. */
    static final String[] NAMES = {
        "0", "1", "-1", "2", "-2", "7", "-7",
        "int.max", "int.max+1", "int.min", "int.min-1",
        "word", "word-1", "word+1",
        "long.max", "long.max+1", "long.min", "long.min-1",
        "2^127-1", "-2^127", "pad-needed", "no-pad", "big-prime", "big-composite",
    };

    static final BigInteger[] VALUES = {
        BigInteger.ZERO,
        BigInteger.ONE,
        BigInteger.ONE.negate(),
        BigInteger.TWO,
        BigInteger.TWO.negate(),
        BigInteger.valueOf(7),
        BigInteger.valueOf(-7),
        BigInteger.valueOf(Integer.MAX_VALUE),
        BigInteger.valueOf(Integer.MAX_VALUE).add(BigInteger.ONE),
        BigInteger.valueOf(Integer.MIN_VALUE),
        BigInteger.valueOf(Integer.MIN_VALUE).subtract(BigInteger.ONE),
        // The 32-bit word boundary the JDK's magnitude array is built from.
        BigInteger.ONE.shiftLeft(32),
        BigInteger.ONE.shiftLeft(32).subtract(BigInteger.ONE),
        BigInteger.ONE.shiftLeft(32).add(BigInteger.ONE),
        BigInteger.valueOf(Long.MAX_VALUE),
        BigInteger.valueOf(Long.MAX_VALUE).add(BigInteger.ONE),
        BigInteger.valueOf(Long.MIN_VALUE),
        BigInteger.valueOf(Long.MIN_VALUE).subtract(BigInteger.ONE),
        BigInteger.ONE.shiftLeft(127).subtract(BigInteger.ONE),
        BigInteger.ONE.shiftLeft(127).negate(),
        // 0x80 leading: two's complement needs a zero pad byte in front.
        new BigInteger("128"),
        // 0x7f leading: it does not.
        new BigInteger("127"),
        new BigInteger("170141183460469231731687303715884105727"),  // 2^127-1, prime
        new BigInteger("170141183460469231731687303715884105725"),  // ... minus 2
    };

    // ------------------------------------------------- 1. the neutral rendering

    /** Asked FIRST and on its own, so that a `toString` defect is one section
     *  rather than an indictment of every section. */
    static void rendering() {
        for (int i = 0; i < VALUES.length; i++) {
            String t = "[" + NAMES[i] + "]";
            BigInteger v = VALUES[i];
            p(t + " toString", () -> v.toString());
            p(t + " toString(16)", () -> v.toString(16));
            p(t + " toString(2)", () -> v.toString(2));
            p(t + " signum", () -> v.signum());
            p(t + " equals self", () -> v.equals(new BigInteger(v.toString())));
            p(t + " hashCode agrees", () -> v.hashCode() == new BigInteger(v.toString()).hashCode());
            p(t + " compareTo self", () -> v.compareTo(new BigInteger(v.toString())));
            p(t + " negate", () -> v.negate().toString());
            p(t + " abs", () -> v.abs().toString());
        }
    }

    // ------------------------------------------------------ 2. the unary rows

    static void unary() {
        for (int i = 0; i < VALUES.length; i++) {
            String t = "[" + NAMES[i] + "]";
            BigInteger v = VALUES[i];
            p(t + " bitCount", () -> v.bitCount());
            p(t + " bitLength", () -> v.bitLength());
            p(t + " not", () -> v.not().toString());
            p(t + " not twice is identity", () -> v.not().not().equals(v));
            p(t + " toByteArray", () -> hex(v.toByteArray()));
            p(t + " toByteArray length", () -> v.toByteArray().length);
            // The round trip is the pair of rows that catches a pad-byte error
            // the hex row alone could be read past.
            p(t + " toByteArray round trip", () -> new BigInteger(v.toByteArray()).toString());
            p(t + " intValueExact", () -> v.intValueExact());
            p(t + " longValueExact", () -> v.longValueExact());
            p(t + " intValue", () -> v.intValue());
            p(t + " longValue", () -> v.longValue());
            p(t + " getLowestSetBit", () -> v.getLowestSetBit());
        }
    }

    // ----------------------------------------------------- 3. the binary rows

    static void binary() {
        for (int i = 0; i < VALUES.length; i++) {
            for (int j = 0; j < VALUES.length; j++) {
                String t = "[" + NAMES[i] + " ? " + NAMES[j] + "]";
                BigInteger a = VALUES[i];
                BigInteger b = VALUES[j];
                p(t + " add", () -> a.add(b).toString());
                p(t + " subtract", () -> a.subtract(b).toString());
                p(t + " multiply", () -> a.multiply(b).toString());
                p(t + " divide", () -> a.divide(b).toString());
                p(t + " remainder", () -> a.remainder(b).toString());
                p(t + " mod", () -> a.mod(b).toString());
                p(t + " gcd", () -> a.gcd(b).toString());
                p(t + " and", () -> a.and(b).toString());
                p(t + " or", () -> a.or(b).toString());
                p(t + " xor", () -> a.xor(b).toString());
                p(t + " compareTo", () -> sgn(a.compareTo(b)));
                p(t + " equals", () -> a.equals(b));
            }
        }
    }

    static int sgn(int v) {
        return v < 0 ? -1 : (v > 0 ? 1 : 0);
    }

    // ------------------------------------------------- 4. identities that bind

    /** Rows a wrong implementation can fail even when every value above happens
     *  to look plausible: the algebra the operations owe each other. */
    static void identities() {
        for (int i = 0; i < VALUES.length; i++) {
            for (int j = 0; j < VALUES.length; j++) {
                String t = "[" + NAMES[i] + " ? " + NAMES[j] + "]";
                BigInteger a = VALUES[i];
                BigInteger b = VALUES[j];
                p(t + " divide*b + remainder == a", () -> {
                    if (b.signum() == 0) {
                        return "n/a";
                    }
                    return a.divide(b).multiply(b).add(a.remainder(b)).equals(a);
                });
                p(t + " remainder sign follows a", () -> {
                    if (b.signum() == 0) {
                        return "n/a";
                    }
                    BigInteger r = a.remainder(b);
                    return r.signum() == 0 || r.signum() == a.signum();
                });
                p(t + " mod is non-negative", () -> {
                    if (b.signum() <= 0) {
                        return "n/a";
                    }
                    return a.mod(b).signum() >= 0;
                });
                p(t + " and/or/xor cover", () -> a.and(b).xor(a.or(b)).equals(a.xor(b)));
                p(t + " add then subtract", () -> a.add(b).subtract(b).equals(a));
            }
        }
    }

    // --------------------------------------------------------- 5. the shifts

    static final int[] SHIFTS = {0, 1, 2, 7, 8, 31, 32, 33, 63, 64, 65, 127, 128, -1, -8, -32, -64};

    static void shifts() {
        for (int i = 0; i < VALUES.length; i++) {
            BigInteger v = VALUES[i];
            for (int s : SHIFTS) {
                String t = "[" + NAMES[i] + " <<" + s + "]";
                p(t + " shiftLeft", () -> v.shiftLeft(s).toString());
                p(t + " shiftRight", () -> v.shiftRight(s).toString());
                // `shiftLeft(-n)` is specified as `shiftRight(n)` and vice
                // versa, which is the row an implementation using a raw
                // unsigned count gets wrong.
                p(t + " shiftLeft(-s)==shiftRight(s)",
                    () -> v.shiftLeft(-s).equals(v.shiftRight(s)));
            }
            for (int bit : new int[] {0, 1, 7, 31, 32, 33, 63, 64, 126, 127, 128}) {
                p("[" + NAMES[i] + "] testBit " + bit, () -> v.testBit(bit));
                p("[" + NAMES[i] + "] setBit " + bit, () -> v.setBit(bit).toString());
                p("[" + NAMES[i] + "] clearBit " + bit, () -> v.clearBit(bit).toString());
                p("[" + NAMES[i] + "] flipBit " + bit, () -> v.flipBit(bit).toString());
            }
            p("[" + NAMES[i] + "] testBit -1", () -> v.testBit(-1));
        }
    }

    // ------------------------------------------------- 6. modular arithmetic

    static final int[] MOD_IDX = {1, 3, 5, 7, 11, 14, 18, 22};

    static void modular() {
        for (int ii : MOD_IDX) {
            for (int jj : MOD_IDX) {
                BigInteger a = VALUES[ii];
                BigInteger m = VALUES[jj];
                String t = "[" + NAMES[ii] + " mod " + NAMES[jj] + "]";
                p(t + " modInverse", () -> a.modInverse(m).toString());
                p(t + " modInverse checks out", () -> {
                    if (m.signum() <= 0) {
                        return "n/a";
                    }
                    return a.modInverse(m).multiply(a).mod(m).equals(BigInteger.ONE);
                });
                for (int kk : new int[] {0, 1, 3, 5}) {
                    BigInteger e = VALUES[kk];
                    p(t + " modPow " + NAMES[kk], () -> a.modPow(e, m).toString());
                }
                // A NEGATIVE exponent is legal exactly when the base is
                // invertible, and is `modInverse` composed with `modPow`.
                p(t + " modPow -3", () -> a.modPow(BigInteger.valueOf(-3), m).toString());
            }
        }
        for (int ii : MOD_IDX) {
            BigInteger v = VALUES[ii];
            p("[" + NAMES[ii] + "] isProbablePrime(20)", () -> v.isProbablePrime(20));
            p("[" + NAMES[ii] + "] isProbablePrime(1)", () -> v.isProbablePrime(1));
            p("[" + NAMES[ii] + "] isProbablePrime(0)", () -> v.isProbablePrime(0));
            p("[" + NAMES[ii] + "] isProbablePrime(-1)", () -> v.isProbablePrime(-1));
            p("[" + NAMES[ii] + "] nextProbablePrime", () -> v.nextProbablePrime().toString());
        }
    }

    // ------------------------------------------------- 7. the constructors

    static void construction() {
        String[] specs = {
            "0", "-0", "1", "-1", "127", "128", "255", "256", "-128", "-129",
            "4294967295", "4294967296", "9223372036854775807", "-9223372036854775808",
            "170141183460469231731687303715884105727",
        };
        for (String s : specs) {
            p("[new BigInteger(\"" + s + "\")] value", () -> new BigInteger(s).toString());
            p("[new BigInteger(\"" + s + "\")] bytes", () -> hex(new BigInteger(s).toByteArray()));
            p("[new BigInteger(\"" + s + "\",16)] value", () -> new BigInteger(s, 16).toString());
        }
        byte[][] arrays = {
            {}, {0}, {1}, {(byte) 0xff}, {(byte) 0x80}, {0, (byte) 0x80},
            {(byte) 0xff, (byte) 0xff}, {0, 0, 0, 1}, {(byte) 0x80, 0, 0, 0},
        };
        for (byte[] a : arrays) {
            String t = "[new BigInteger(" + hex(a) + ")]";
            p(t + " value", () -> new BigInteger(a).toString());
            p(t + " round trip", () -> hex(new BigInteger(a).toByteArray()));
            for (int sig : new int[] {-1, 0, 1, 2, -2}) {
                p(t + " signum " + sig + " value", () -> new BigInteger(sig, a).toString());
            }
        }
        // The refusals the two constructors owe.
        p("new BigInteger(\"\")", () -> new BigInteger("").toString());
        p("new BigInteger(\"-\")", () -> new BigInteger("-").toString());
        p("new BigInteger(\"+\")", () -> new BigInteger("+").toString());
        p("new BigInteger(\" 1\")", () -> new BigInteger(" 1").toString());
        p("new BigInteger(\"1 \")", () -> new BigInteger("1 ").toString());
        p("new BigInteger(\"0x1\")", () -> new BigInteger("0x1").toString());
        p("new BigInteger(\"--1\")", () -> new BigInteger("--1").toString());
        p("new BigInteger(\"1\", 1)", () -> new BigInteger("1", 1).toString());
        p("new BigInteger(\"1\", 37)", () -> new BigInteger("1", 37).toString());
        p("new BigInteger(\"z\", 36)", () -> new BigInteger("z", 36).toString());
        p("new BigInteger((byte[]) null)", () -> new BigInteger((byte[]) null).toString());
        p("new BigInteger((String) null)", () -> new BigInteger((String) null).toString());
        p("valueOf round trips", () -> {
            String out = "";
            for (long l : new long[] {0, 1, -1, Long.MAX_VALUE, Long.MIN_VALUE}) {
                out = out + BigInteger.valueOf(l) + ",";
            }
            return out;
        });
    }

    // ------------------------------------------------------ 8. the refusals

    static void refusals() {
        BigInteger a = new BigInteger("170141183460469231731687303715884105727");
        p("divide by zero", () -> a.divide(BigInteger.ZERO).toString());
        p("remainder by zero", () -> a.remainder(BigInteger.ZERO).toString());
        p("mod by zero", () -> a.mod(BigInteger.ZERO).toString());
        p("mod by negative", () -> a.mod(BigInteger.valueOf(-7)).toString());
        p("divideAndRemainder by zero", () -> a.divideAndRemainder(BigInteger.ZERO)[0].toString());
        p("modInverse by zero", () -> a.modInverse(BigInteger.ZERO).toString());
        p("modInverse by negative", () -> a.modInverse(BigInteger.valueOf(-7)).toString());
        p("modInverse of a non-unit", () -> BigInteger.TWO.modInverse(BigInteger.valueOf(4)).toString());
        p("modPow by zero modulus", () -> a.modPow(BigInteger.TWO, BigInteger.ZERO).toString());
        p("modPow negative exponent, non-invertible",
            () -> BigInteger.TWO.modPow(BigInteger.valueOf(-1), BigInteger.valueOf(4)).toString());
        p("intValueExact overflow", () -> a.intValueExact());
        p("longValueExact overflow", () -> a.longValueExact());
        p("shortValueExact overflow", () -> a.shortValueExact());
        p("byteValueExact overflow", () -> a.byteValueExact());
        // A NEGATIVE modulus. `modPow`'s own body says "a negative modulus is
        // outside the BigInteger spec, which requires m > 0" and then computes
        // against |m| anyway; the sweep above could not see that, because every
        // modulus in MOD_IDX is positive. Asked here for all three modular
        // operations, so the one that answers where its siblings refuse stands
        // out on its own row.
        p("modPow by negative modulus",
            () -> a.modPow(BigInteger.TWO, BigInteger.valueOf(-7)).toString());
        p("modPow negative exp by negative modulus",
            () -> a.modPow(BigInteger.valueOf(-3), BigInteger.valueOf(-7)).toString());
        p("modPow by minus one", () -> a.modPow(BigInteger.TWO, BigInteger.ONE.negate()).toString());
        p("modInverse by negative modulus (again)",
            () -> a.modInverse(BigInteger.valueOf(-7)).toString());
        p("mod by negative (again)", () -> a.mod(BigInteger.valueOf(-7)).toString());
        p("modPow by one", () -> a.modPow(BigInteger.TWO, BigInteger.ONE).toString());
        p("modPow zero exponent zero modulus",
            () -> a.modPow(BigInteger.ZERO, BigInteger.ZERO).toString());

        // THE NULL-ARGUMENT MESSAGES, and why they are worth a row each.
        //
        // HotSpot's helpful NPE (JEP 358, on by default since 15) names the
        // field or method the real bytecode was about to touch and the
        // PARAMETER it came from: `Cannot read field "signum" because "val" is
        // null`. CratonVM computes those correctly for every general shape --
        // a null local, a null field, an array length, an unbox, a monitor
        // enter -- measured by `apps/probes/HelpfulNpeProbe.java`. What flattens
        // them here is the SHADOW: a native intercepts before the bytecode that
        // would have produced the message, and the shared `obj_arg` helper has
        // only a generic one to give.
        //
        // The discriminator is in this list. `andNot`, `min`, `max`,
        // `compareTo` and `divideAndRemainder` have no native, so real bytecode
        // runs and the message is right; every method beside them that is
        // shadowed answers `null object argument`. That is the whole diagnosis
        // on one screen, which is why the unshadowed ones are asked too.
        p("add null", () -> a.add(null).toString());
        p("subtract null", () -> a.subtract(null).toString());
        p("multiply null", () -> a.multiply(null).toString());
        p("divide null", () -> a.divide(null).toString());
        p("remainder null", () -> a.remainder(null).toString());
        p("mod null", () -> a.mod(null).toString());
        p("gcd null", () -> a.gcd(null).toString());
        p("and null", () -> a.and(null).toString());
        p("or null", () -> a.or(null).toString());
        p("xor null", () -> a.xor(null).toString());
        p("modInverse null", () -> a.modInverse(null).toString());
        p("modPow null exponent", () -> a.modPow(null, BigInteger.TEN).toString());
        p("modPow null modulus", () -> a.modPow(BigInteger.TWO, null).toString());
        p("andNot null (unshadowed control)", () -> a.andNot(null).toString());
        p("min null (unshadowed control)", () -> a.min(null).toString());
        p("max null (unshadowed control)", () -> a.max(null).toString());
        p("compareTo null (unshadowed control)", () -> a.compareTo(null));
        p("divideAndRemainder null (unshadowed control)",
            () -> a.divideAndRemainder(null)[0].toString());
        p("equals null is false, not a throw", () -> a.equals(null));
        p("testBit huge", () -> a.testBit(Integer.MAX_VALUE));
        p("shiftLeft Integer.MIN_VALUE", () -> a.shiftLeft(Integer.MIN_VALUE).toString());
        p("pow negative", () -> a.pow(-1).toString());
        p("sqrt of a negative", () -> BigInteger.valueOf(-4).sqrt().toString());
    }

    public static void main(String[] args) {
        sect("rendering", BigIntegerSweep::rendering);
        sect("unary", BigIntegerSweep::unary);
        sect("binary", BigIntegerSweep::binary);
        sect("identities", BigIntegerSweep::identities);
        sect("shifts", BigIntegerSweep::shifts);
        sect("modular", BigIntegerSweep::modular);
        sect("construction", BigIntegerSweep::construction);
        sect("refusals", BigIntegerSweep::refusals);
        System.out.println("rows " + rows);
        System.out.println("DONE BigIntegerSweep");
    }
}
