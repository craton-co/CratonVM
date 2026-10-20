import java.security.NoSuchAlgorithmException;
import java.security.SecureRandom;
import java.util.Arrays;

/**
 * The half of the `java.util.Random` shadow retirement that is NOT obviously
 * safe.
 *
 * <p>`SecureRandom extends Random`, so retiring the `java/util/Random` natives
 * in real-JDK mode changes what runs during a `SecureRandom`'s SUPERCLASS
 * construction: `Random(long)` on a subclass receiver takes the
 * `setSeed(seed)` branch rather than assigning `this.seed` directly, and that
 * `setSeed` is `SecureRandom`'s own (still native here). `SecureRandom`'s
 * natives are registered separately and are not retired — but "the two
 * registrations still compose" is a claim, and this is the check.
 *
 * <p>Two properties matter and they pull in opposite directions:
 *
 * <ul>
 *   <li>SHA1PRNG seeded through `setSeed` MUST replay bit-for-bit — the JDK
 *       specifies it as a deterministic function of its seed, and this VM
 *       implements the real state machine for exactly that reason;
 *   <li>every other `SecureRandom` MUST NOT replay — a repeatable CSPRNG is a
 *       security defect, so those rows assert DIFFERENCE, not equality.
 * </ul>
 *
 * <p>Diff against real HotSpot. The deterministic rows must match exactly; the
 * non-deterministic rows print only a boolean so they are comparable at all.
 */
public class SecureRandomSpec {

    static String hex(byte[] b) {
        StringBuilder s = new StringBuilder();
        for (byte x : b) {
            s.append(String.format("%02x", x));
        }
        return s.toString();
    }

    public static void main(String[] args) throws Exception {
        // --- deterministic: SHA1PRNG replays from its seed --------------------
        try {
            SecureRandom a = SecureRandom.getInstance("SHA1PRNG");
            a.setSeed(42L);
            byte[] ba = new byte[16];
            a.nextBytes(ba);
            System.out.println("CK sha1prng.seed42.nextBytes = " + hex(ba));
            System.out.println("CK sha1prng.seed42.nextInt   = " + secondInt(42L));

            SecureRandom b = SecureRandom.getInstance("SHA1PRNG");
            b.setSeed(42L);
            byte[] bb = new byte[16];
            b.nextBytes(bb);
            System.out.println("CK sha1prng.replays          = " + Arrays.equals(ba, bb));

            SecureRandom c = SecureRandom.getInstance("SHA1PRNG");
            c.setSeed(43L);
            byte[] bc = new byte[16];
            c.nextBytes(bc);
            System.out.println("CK sha1prng.seed43.differs   = " + !Arrays.equals(ba, bc));
        } catch (NoSuchAlgorithmException e) {
            System.out.println("CK sha1prng UNAVAILABLE " + e);
        }

        // --- non-deterministic: a default SecureRandom must NOT replay --------
        SecureRandom d1 = new SecureRandom();
        SecureRandom d2 = new SecureRandom();
        byte[] x1 = new byte[16];
        byte[] x2 = new byte[16];
        d1.nextBytes(x1);
        d2.nextBytes(x2);
        System.out.println("CK default.distinct          = " + !Arrays.equals(x1, x2));

        // Seeded ctor is DISCARDED by the JDK too -- must still not replay.
        SecureRandom s1 = new SecureRandom(new byte[] {1, 2, 3, 4});
        SecureRandom s2 = new SecureRandom(new byte[] {1, 2, 3, 4});
        byte[] y1 = new byte[16];
        byte[] y2 = new byte[16];
        s1.nextBytes(y1);
        s2.nextBytes(y2);
        System.out.println("CK seededCtor.distinct      = " + !Arrays.equals(y1, y2));

        // --- the inherited java.util.Random surface on a SecureRandom --------
        // These dispatch through Random's methods on a SecureRandom receiver,
        // which is precisely the composition the retirement changes.
        SecureRandom r = new SecureRandom();
        int i = r.nextInt();
        int bounded = r.nextInt(100);
        long l = r.nextLong();
        double dv = r.nextDouble();
        float f = r.nextFloat();
        boolean bl = r.nextBoolean();
        System.out.println("CK inherited.nextInt(100).inRange = " + (bounded >= 0 && bounded < 100));
        System.out.println("CK inherited.nextDouble.inRange   = " + (dv >= 0.0 && dv < 1.0));
        System.out.println("CK inherited.nextFloat.inRange    = " + (f >= 0.0f && f < 1.0f));
        System.out.println("CK inherited.allCallable          = "
                + (i != 0 || l != 0 || bl || !bl));

        // A SecureRandom must not be an LCG: two draws from ONE instance differ.
        SecureRandom u = new SecureRandom();
        System.out.println("CK sameInstance.twoDrawsDiffer    = " + (u.nextLong() != u.nextLong()));
    }

    /** Second SHA1PRNG value from the same seed, as an int, for a stable row. */
    static int secondInt(long seed) throws NoSuchAlgorithmException {
        SecureRandom s = SecureRandom.getInstance("SHA1PRNG");
        s.setSeed(seed);
        s.nextBytes(new byte[16]);
        return s.nextInt();
    }
}
