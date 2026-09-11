import java.math.BigInteger;

/** `BigInteger` primality and the layers underneath it, one row per layer.
 *
 *  Written to isolate `RJdkSecurity`'s `2^127-1 must be prime`, which is what
 *  `java/math/BigInteger` yielding to real bytecode used to produce. It is not
 *  one operation: the JDK routes `isProbablePrime` through `primeToCertainty`
 *  to `passesMillerRabin` / `passesLucasLehmer`, and those are built from `mod`,
 *  `modPow`, `shiftRight`, `subtract`, `bitLength` and `getLowestSetBit`. Armed
 *  or retired, every one of those yields too, so a wrong answer at the top could
 *  come from any of them. Each is asked directly, so the diff names the layer.
 *
 *  It worked: the first divergence was `(n-1) >> 1`, short by exactly 2^32,
 *  which is `shiftRightImplWorker` -- an `@IntrinsicCandidate` the VM natively
 *  overrides, running one iteration short of the JDK contract.
 *
 *  DETERMINISM, and this probe got it wrong once. Below 100 bits
 *  `isProbablePrime` is Miller-Rabin with bases drawn from an internal
 *  `Random`, and `certainty` only sets the round count -- `(certainty+1)/2`,
 *  capped at 50. A PRIME passes for every base, so it is safe at any certainty.
 *  A COMPOSITE is not: 91 has 16 strong liars among 90 bases, so
 *  `isProbablePrime(1)` on it is a coin flip, and it duly printed `true` here
 *  against HotSpot's `false`. That was this file's bug, not the VM's. Composites
 *  are therefore asked only at certainties whose round count puts a false
 *  positive around 1e-15, which is the ops page's "print no value the two VMs
 *  may choose independently" applied to a value that is USUALLY deterministic.
 */
public class L2PrimeProbe {
    static int rows = 0;

    static void row(String label, Object v) {
        System.out.println(label + " |" + v + "|");
        rows++;
    }

    static void ask(String tag, BigInteger n, boolean prime) {
        row(tag + " bitLength", n.bitLength());
        row(tag + " bitCount", n.bitCount());
        row(tag + " signum", n.signum());
        row(tag + " testBit0", n.testBit(0));
        row(tag + " lowestSetBit", n.getLowestSetBit());
        row(tag + " toString", n.toString());
        row(tag + " toString16", n.toString(16));
        BigInteger m1 = n.subtract(BigInteger.ONE);
        row(tag + " n-1", m1.toString());
        row(tag + " (n-1)>>1", m1.shiftRight(1).toString());
        row(tag + " (n-1)lowestSetBit", m1.getLowestSetBit());
        // the Miller-Rabin core, with FIXED bases so the row is deterministic
        for (int base : new int[] {2, 3, 5, 7, 61}) {
            BigInteger b = BigInteger.valueOf(base);
            row(tag + " modPow(" + base + ",n-1,n)", b.modPow(m1, n).toString());
            row(tag + " modPow(" + base + ",(n-1)/2,n)",
                b.modPow(m1.shiftRight(1), n).toString());
        }
        row(tag + " mod3", n.mod(BigInteger.valueOf(3)).toString());
        row(tag + " mod65537", n.mod(BigInteger.valueOf(65537)).toString());
        row(tag + " sq mod n", n.multiply(n).mod(n).toString());
        // See the class comment: a prime is base-independent, a composite is not.
        int[] certainties = prime ? new int[] {1, 5, 10, 20, 40, 100}
                                  : new int[] {40, 100};
        for (int c : certainties) {
            row(tag + " isProbablePrime(" + c + ")", n.isProbablePrime(c));
        }
        row(tag + " nextProbablePrime", n.nextProbablePrime().toString());
    }

    public static void main(String[] a) {
        // the exact value RJdkSecurity asserts on
        ask("m127", BigInteger.ONE.shiftLeft(127).subtract(BigInteger.ONE), true);
        // the neighbouring Mersenne exponents, prime and composite
        ask("m61", BigInteger.ONE.shiftLeft(61).subtract(BigInteger.ONE), true);
        ask("m89", BigInteger.ONE.shiftLeft(89).subtract(BigInteger.ONE), true);
        ask("m67composite", BigInteger.ONE.shiftLeft(67).subtract(BigInteger.ONE), false);
        // a small definite prime and a small composite, as controls
        ask("p97", BigInteger.valueOf(97), true);
        ask("c91", BigInteger.valueOf(91), false);
        // an RSA-shaped prime, since RJdkSecurity is a crypto vector
        ask("p1024ish", new BigInteger(
            "17976931348623159077293051907890247336179769789423065727343008115"
          + "77326758055009631327084773224075360211201138798713933576587897688"
          + "1440336881560650707"), true);
        System.out.println("rows " + rows);
        System.out.println("DONE L2PrimeProbe");
    }
}
