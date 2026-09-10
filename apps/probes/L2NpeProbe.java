import java.math.BigInteger;

/** Does a RETIRED `BigInteger` row produce HotSpot's helpful NPE?
 *
 *  The 2026-08-29 L8 batch fixed thirteen of these by hand: a shadowing native
 *  intercepts before the bytecode that would have produced the message, and the
 *  shared `obj_arg` helper had only a generic string, so the natives were given
 *  the real body's first-dereference message as a constant. Retiring the row
 *  removes the native, so the message should now come from the bytecode itself.
 *
 *  The control is the half of the family that was NEVER shadowed -- `andNot`,
 *  `min`, `max`, `compareTo`, `divideAndRemainder`. They take the same argument
 *  type on the same class and have always run real bytecode, so if they carry
 *  the helpful message and the retired rows do not, the difference is about the
 *  retirement and not about this VM's NPE machinery. That control was already
 *  sitting in the L8 probe's output; it is asked here explicitly.
 */
public class L2NpeProbe {
    static int rows = 0;

    static void ask(String tag, Runnable r) {
        try {
            r.run();
            System.out.println(tag + " |NO THROW|");
        } catch (Throwable t) {
            System.out.println(tag + " |" + t.getClass().getName() + ": " + t.getMessage() + "|");
        }
        rows++;
    }

    public static void main(String[] a) {
        BigInteger one = BigInteger.ONE;
        BigInteger big = BigInteger.ONE.shiftLeft(127);

        // RETIRED in this build (survivor: an older math_bignum intrinsic)
        ask("retired add", () -> one.add(null));
        ask("retired subtract", () -> one.subtract(null));
        ask("retired multiply", () -> one.multiply(null));
        // RETIRED, no survivor
        ask("retired divide", () -> one.divide(null));
        ask("retired remainder", () -> one.remainder(null));
        ask("retired mod", () -> one.mod(null));
        ask("retired gcd", () -> one.gcd(null));
        ask("retired and", () -> one.and(null));
        ask("retired or", () -> one.or(null));
        ask("retired xor", () -> one.xor(null));
        ask("retired modPow", () -> one.modPow(null, one));
        ask("retired modInverse", () -> one.modInverse(null));

        // CONTROL: never shadowed, always real bytecode
        ask("control andNot", () -> one.andNot(null));
        ask("control min", () -> one.min(null));
        ask("control max", () -> one.max(null));
        ask("control compareTo", () -> one.compareTo(null));
        ask("control divideAndRemainder", () -> one.divideAndRemainder(null));

        // CONTROL 2: a null deref in ordinary app bytecode on this same run
        ask("control app field", () -> {
            int[] z = null;
            System.out.println(z.length);
        });
        ask("control app invoke", () -> {
            String s = null;
            System.out.println(s.length());
        });

        // shape: does the retired path even return the right VALUE?
        ask("retired add returns", () -> {
            BigInteger r = big.add(BigInteger.ONE);
            if (r == null) throw new IllegalStateException("add returned null");
            if (!r.toString().equals("170141183460469231731687303715884105729"))
                throw new IllegalStateException("add wrong: " + r);
        });

        System.out.println("rows " + rows);
        System.out.println("DONE L2NpeProbe");
    }
}
