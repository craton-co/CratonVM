import java.lang.reflect.Field;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.math.BigInteger;
import java.security.KeyPair;
import java.security.KeyPairGenerator;
import java.security.Signature;
import java.security.spec.ECGenParameterSpec;
import java.util.Arrays;
import java.util.Random;
import javax.crypto.KeyAgreement;

/**
 * SBR wave 5, lane bigint (TLS-T1): the RSA/DH/EC arithmetic core that HotSpot
 * intrinsifies, and that the JDK's own JSSE handshake bytecode sits on.
 *
 * <pre>
 *   java --add-opens java.base/java.math=ALL-UNNAMED \
 *        --add-opens java.base/sun.security.util.math.intpoly=ALL-UNNAMED \
 *        SbrW5Bigint fuzz [hot]     # bit-identity: diff the output against HotSpot 25
 *   java SbrW5Bigint perf [rounds] # timing; interleave arms externally
 * </pre>
 *
 * {@code fuzz} calls every {@code @IntrinsicCandidate} kernel DIRECTLY by
 * reflection (the only instrument that asks the kernel and nothing else):
 * {@code BigInteger.implMultiplyToLen / implSquareToLen / implMulAdd / mulAdd /
 * implMontgomeryMultiply / implMontgomerySquare / shiftLeftImplWorker /
 * shiftRightImplWorker}, {@code MontgomeryIntegerPolynomialP256.mult / square}
 * and {@code IntegerPolynomial.conditionalAssign}, over random operands at
 * lengths 1..128 plus the edges (zero, one, all-ones limbs, odd and even
 * moduli, reused / short / null product buffers, out-of-range lengths), and
 * folds every result array, return value and exception (class + message) into
 * a per-kernel FNV-1a hash. It then does the same through the public API
 * (multiply, square, modPow with odd and even moduli, shifts, modInverse).
 * First-run hashes are printed; with {@code hot} the whole battery is re-run
 * 30 times and any hash that moves counts as drift (cold == warm).
 */
public class SbrW5Bigint {
    static long h;
    static void mix(long v) { for (int i = 0; i < 8; i++) { h ^= (v >>> (8 * i)) & 0xFF; h *= 0x100000001b3L; } }
    static void mix(int[] a) { if (a == null) { mix(0xDEADL); return; } mix(a.length); for (int v : a) mix(v); }
    static void mix(long[] a) { if (a == null) { mix(0xDEADL); return; } mix(a.length); for (long v : a) mix(v); }
    static void mix(String s) { if (s == null) { mix(0xBEEFL); return; } mix(s.length()); for (int i = 0; i < s.length(); i++) mix(s.charAt(i)); }

    static Method find(Class<?> c, String name) {
        for (Method m : c.getDeclaredMethods()) {
            if (m.getName().equals(name)) { m.setAccessible(true); return m; }
        }
        throw new RuntimeException("absent " + c.getName() + "." + name);
    }

    static StringBuilder excLog = new StringBuilder();

    /** Invoke, folding result + every array argument + exception into h. */
    static void call(Object recv, Method m, Object... args) {
        try {
            Object r = m.invoke(recv, args);
            if (r instanceof int[]) mix((int[]) r);
            else if (r instanceof Integer) mix((Integer) r);
            else mix(r == null ? 0x77L : 0x78L);
        } catch (InvocationTargetException ite) {
            Throwable c = ite.getCause();
            mix(c.getClass().getName());
            mix(c.getMessage());
            if (excLog.length() < 4000) excLog.append("  exc ").append(m.getName()).append(' ').append(c.getClass().getName()).append(": ").append(c.getMessage()).append('\n');
        } catch (IllegalAccessException e) {
            throw new RuntimeException(e);
        }
        for (Object o : args) {
            if (o instanceof int[]) mix((int[]) o);
            else if (o instanceof long[]) mix((long[]) o);
        }
    }

    static int[] rnd(Random r, int n, int kind) {
        int[] a = new int[n];
        for (int i = 0; i < n; i++) {
            switch (kind) {
                case 0: a[i] = r.nextInt(); break;          // random
                case 1: a[i] = 0; break;                    // zero
                case 2: a[i] = -1; break;                   // all ones
                case 3: a[i] = (i == n - 1) ? 1 : 0; break; // one
                default: a[i] = (r.nextInt(4) == 0) ? -1 : (r.nextInt(3) == 0 ? 0 : r.nextInt());
            }
        }
        return a;
    }

    static long inv64(int[] mod) {
        long n0 = (mod[mod.length - 1] & 0xFFFFFFFFL) + ((mod.length > 1 ? mod[mod.length - 2] & 0xFFFFFFFFL : 0L) << 32);
        long t = n0;
        for (int i = 0; i < 5; i++) t *= 2 - n0 * t;
        return -t;
    }

    static Method MTL, SQ, IMA, MA, MM, MS, SL, SR, PMULT, PSQ, CASSIGN;
    static Object P256;

    static String[] NAMES = {"implMultiplyToLen", "implSquareToLen", "implMulAdd", "mulAdd",
        "implMontgomeryMultiply", "implMontgomerySquare", "shiftLeftImplWorker", "shiftRightImplWorker",
        "P256.mult", "P256.square", "conditionalAssign", "api.multiply", "api.square", "api.modPowOdd",
        "api.modPowEven", "api.shift", "api.modInverse", "api.rsaLike"};

    static long[] battery() {
        long[] out = new long[NAMES.length];
        Random r = new Random(0x5B5B5B5BL);
        int[] kinds = {0, 1, 2, 3, 4};

        // 0 implMultiplyToLen(int[] x, int xlen, int[] y, int ylen, int[] z)
        h = 0xcbf29ce484222325L;
        for (int xl = 1; xl <= 128; xl += (xl < 16 ? 1 : 7)) {
            for (int k : kinds) {
                int yl = 1 + r.nextInt(128);
                int[] x = rnd(r, xl, k), y = rnd(r, yl, (k + 1) % 5);
                int[] z = new int[xl + yl + r.nextInt(3)];
                Arrays.fill(z, 0x5A5A5A5A);
                call(null, MTL, x, xl, y, yl, z);
            }
        }
        call(null, MTL, new int[2], 3, new int[2], 2, new int[8]);   // xlen past x
        call(null, MTL, new int[2], 2, new int[2], 2, new int[3]);   // z too short
        call(null, MTL, new int[2], 0, new int[2], 2, new int[4]);   // zero xlen
        out[0] = h;

        // 1 implSquareToLen(int[] x, int len, int[] z, int zlen)
        h = 0xcbf29ce484222325L;
        for (int l = 1; l <= 128; l++) {
            for (int k : kinds) {
                int[] x = rnd(r, l, k);
                int[] z = new int[2 * l];
                Arrays.fill(z, 0x13579BDF);
                call(null, SQ, x, l, z, 2 * l);
            }
        }
        call(null, SQ, new int[2], 0, new int[4], 4);
        call(null, SQ, new int[2], 3, new int[6], 6);
        call(null, SQ, new int[2], 2, new int[3], 4);
        out[1] = h;

        // 2,3 implMulAdd / mulAdd(int[] out, int[] in, int offset, int len, int k)
        for (int which = 0; which < 2; which++) {
            h = 0xcbf29ce484222325L;
            Method m = which == 0 ? IMA : MA;
            for (int l = 1; l <= 128; l += (l < 16 ? 1 : 5)) {
                for (int k : kinds) {
                    int[] in = rnd(r, l, k);
                    int[] o = rnd(r, l + 1 + r.nextInt(l + 2), (k + 2) % 5);
                    int off = r.nextInt(o.length - l);
                    int kk = k == 2 ? -1 : (k == 1 ? 0 : r.nextInt());
                    call(null, m, o, in, off, l, kk);
                }
            }
            call(null, m, new int[3], new int[3], 2, 3, 5);   // past the end
            call(null, m, new int[3], new int[3], 0, 0, 5);   // len 0
            out[2 + which] = h;
        }

        // 4,5 implMontgomeryMultiply / implMontgomerySquare
        for (int which = 0; which < 2; which++) {
            h = 0xcbf29ce484222325L;
            for (int l = 1; l <= 64; l++) {
                for (int k : kinds) {
                    int[] mod = rnd(r, l, 0);
                    mod[l - 1] |= 1;                          // odd modulus
                    if (k == 2) Arrays.fill(mod, -1);          // all-ones odd modulus
                    if (k == 3) mod[0] |= 0x80000000;          // top bit set
                    if (k == 4 && l > 1) mod[0] = 1;           // small top word
                    if (mod[0] == 0) mod[0] = 1 + r.nextInt(1000);
                    long iv = inv64(mod);
                    int[] a = rnd(r, l, k == 3 ? 0 : k), b = rnd(r, l, (k + 3) % 5);
                    // Keep a, b < mod (the domain oddModPow calls these in):
                    // outside it montReduce's final `while (n >= mod) n -= mod`
                    // runs up to 2^32 times per call, and HotSpot's own
                    // intrinsic stops reducing early, so only the Java body
                    // defines an answer there.
                    int top = (int) ((mod[0] & 0xFFFFFFFFL) >>> 1);
                    a[0] = (int) ((a[0] & 0xFFFFFFFFL) % ((top & 0xFFFFFFFFL) + 1));
                    b[0] = (int) ((b[0] & 0xFFFFFFFFL) % ((top & 0xFFFFFFFFL) + 1));
                    int pk = r.nextInt(3);
                    int[] prod = pk == 0 ? null : (pk == 1 ? new int[2 * l] : new int[2 * l + 1 + r.nextInt(3)]);
                    if (prod != null) Arrays.fill(prod, 0x2468ACE0);
                    if (which == 0) call(null, MM, a, b, mod, l, iv, prod);
                    else call(null, MS, a, mod, l, iv, prod);
                }
            }
            // even modulus + arbitrary inv, short product, bad lengths
            int[] ev = {0x12345678, 0x9abcdef0};
            if (which == 0) {
                call(null, MM, new int[] {5, 6}, new int[] {7, 8}, ev, 2, 0x1234L, null);
                call(null, MM, new int[2], new int[2], ev, 0, 1L, null);
                call(null, MM, new int[2], new int[2], ev, 3, 1L, null);
                call(null, MM, new int[] {1, 2}, new int[] {3, 4}, new int[] {9, 11}, 2, inv64(new int[] {9, 11}), new int[2]);
            } else {
                call(null, MS, new int[] {5, 6}, ev, 2, 0x1234L, null);
                call(null, MS, new int[2], ev, 0, 1L, null);
                call(null, MS, new int[2], ev, 3, 1L, null);
                call(null, MS, new int[] {1, 2}, new int[] {9, 11}, 2, inv64(new int[] {9, 11}), new int[2]);
            }
            out[4 + which] = h;
        }

        // 6,7 shiftLeftImplWorker / shiftRightImplWorker(newArr, oldArr, newIdx, shiftCount, numIter)
        for (int which = 0; which < 2; which++) {
            h = 0xcbf29ce484222325L;
            Method m = which == 0 ? SL : SR;
            for (int l = 2; l <= 128; l += (l < 16 ? 1 : 9)) {
                for (int sc = 1; sc < 32; sc += 3) {
                    int[] old = rnd(r, l, r.nextInt(5));
                    int newIdx = r.nextInt(2);
                    int numIter = l - 1;
                    int[] na = new int[l + newIdx];
                    Arrays.fill(na, 0x0F0F0F0F);
                    call(null, m, na, old, newIdx, sc, numIter);
                }
            }
            out[6 + which] = h;
        }

        // 8,9 MontgomeryIntegerPolynomialP256.mult(long[] a, long[] b, long[] r) / square(long[] a, long[] r)
        long[] edge = {0L, 1L, 0xFFFFFFFFFFFFFL, 0x10000000000000L, -1L, Long.MIN_VALUE, Long.MAX_VALUE,
            0x000fffffffffffffL, 0x00000fffffffffffL, 0x0000001000000000L, 0x0000ffffffff0000L};
        long[][] pModLimbs = {{0x000fffffffffffffL, 0x00000fffffffffffL, 0L, 0x0000001000000000L, 0x0000ffffffff0000L}};
        for (int which = 0; which < 2; which++) {
            h = 0xcbf29ce484222325L;
            for (int i = 0; i < 3000; i++) {
                long[] a = new long[5], b = new long[5], res = new long[5];
                int mode = i % 6;
                for (int j = 0; j < 5; j++) {
                    switch (mode) {
                        case 0: a[j] = r.nextLong() & 0xFFFFFFFFFFFFFL; b[j] = r.nextLong() & 0xFFFFFFFFFFFFFL; break;
                        case 1: a[j] = r.nextLong(); b[j] = r.nextLong(); break;       // any 64-bit limb
                        case 2: a[j] = edge[r.nextInt(edge.length)]; b[j] = edge[r.nextInt(edge.length)]; break;
                        case 3: a[j] = pModLimbs[0][j]; b[j] = r.nextLong() & 0xFFFFFFFFFFFFFL; break;
                        case 4: a[j] = (r.nextLong() & 0x1FFFFFFFFFFFFFL) - (1L << 52); b[j] = r.nextLong() & 0x3FFFFFFFFFFFFFL; break;
                        default: a[j] = r.nextLong() >> r.nextInt(64); b[j] = r.nextLong() >>> r.nextInt(64);
                    }
                }
                Arrays.fill(res, 0x1111L);
                if (which == 0) call(P256, PMULT, a, b, res); else call(P256, PSQ, a, res);
            }
            // aliasing: r == a
            long[] al = {1, 2, 3, 4, 5};
            if (which == 0) call(P256, PMULT, al, al, al); else call(P256, PSQ, al, al);
            // short arrays
            if (which == 0) call(P256, PMULT, new long[4], new long[5], new long[5]); else call(P256, PSQ, new long[4], new long[5]);
            if (which == 0) call(P256, PMULT, new long[5], new long[5], new long[4]); else call(P256, PSQ, new long[5], new long[3]);
            out[8 + which] = h;
        }

        // 10 IntegerPolynomial.conditionalAssign(int set, long[] a, long[] b)
        h = 0xcbf29ce484222325L;
        for (int i = 0; i < 500; i++) {
            int n = 1 + r.nextInt(20);
            long[] a = new long[n], b = new long[n];
            for (int j = 0; j < n; j++) { a[j] = r.nextLong(); b[j] = r.nextLong(); }
            int set = i % 3 == 2 ? r.nextInt() : (i & 1);
            call(null, CASSIGN, set, a, b);
        }
        call(null, CASSIGN, 1, new long[3], new long[2]);
        call(null, CASSIGN, 1, new long[2], new long[3]);
        out[10] = h;

        // 11.. public API
        h = 0xcbf29ce484222325L;
        for (int bits = 1; bits <= 128 * 32; bits += (bits < 256 ? 13 : 257)) {
            BigInteger x = new BigInteger(bits, r), y = new BigInteger(1 + r.nextInt(4096), r);
            if (r.nextBoolean()) x = x.negate();
            mix(x.multiply(y).toByteArray().length); mix(Arrays.hashCode(x.multiply(y).toByteArray()));
        }
        out[11] = h;
        h = 0xcbf29ce484222325L;
        for (int bits = 1; bits <= 128 * 32; bits += (bits < 256 ? 11 : 263)) {
            BigInteger x = new BigInteger(bits, r);
            mix(Arrays.hashCode(x.multiply(x).toByteArray()));
            mix(Arrays.hashCode(x.pow(3).toByteArray()));
        }
        out[12] = h;
        h = 0xcbf29ce484222325L;
        for (int bits = 2; bits <= 2048; bits += (bits < 128 ? 17 : 191)) {
            BigInteger m = new BigInteger(bits, r).setBit(0).setBit(bits - 1);
            BigInteger b = new BigInteger(bits + 8, r), e = new BigInteger(1 + r.nextInt(bits), r);
            mix(Arrays.hashCode(b.modPow(e, m).toByteArray()));
            mix(Arrays.hashCode(b.negate().modPow(e, m).toByteArray()));
        }
        out[13] = h;
        h = 0xcbf29ce484222325L;
        for (int bits = 2; bits <= 1024; bits += (bits < 128 ? 19 : 173)) {
            BigInteger m = new BigInteger(bits, r).clearBit(0).setBit(bits - 1);
            BigInteger b = new BigInteger(bits + 8, r), e = new BigInteger(1 + r.nextInt(bits), r);
            mix(Arrays.hashCode(b.modPow(e, m).toByteArray()));
        }
        out[14] = h;
        h = 0xcbf29ce484222325L;
        for (int i = 0; i < 400; i++) {
            BigInteger x = new BigInteger(1 + r.nextInt(4000), r);
            if (r.nextBoolean()) x = x.negate();
            int s = r.nextInt(300);
            mix(Arrays.hashCode(x.shiftLeft(s).toByteArray()));
            mix(Arrays.hashCode(x.shiftRight(s).toByteArray()));
        }
        out[15] = h;
        h = 0xcbf29ce484222325L;
        for (int bits = 8; bits <= 2048; bits += 97) {
            BigInteger m = new BigInteger(bits, r).setBit(0).setBit(bits - 1);
            BigInteger x = new BigInteger(bits - 1, r).setBit(0);
            try { mix(Arrays.hashCode(x.modInverse(m).toByteArray())); } catch (ArithmeticException e) { mix(e.getMessage()); }
        }
        out[16] = h;
        // RSA-shaped: CRT pieces of a fixed-seed "key" (p, q probable primes)
        h = 0xcbf29ce484222325L;
        {
            Random kr = new Random(42);
            BigInteger p = BigInteger.probablePrime(1024, kr), q = BigInteger.probablePrime(1024, kr);
            BigInteger n = p.multiply(q), e = BigInteger.valueOf(65537);
            BigInteger phi = p.subtract(BigInteger.ONE).multiply(q.subtract(BigInteger.ONE));
            BigInteger d = e.modInverse(phi);
            BigInteger msg = new BigInteger(2040, kr);
            BigInteger sig = msg.modPow(d, n);
            mix(Arrays.hashCode(sig.toByteArray()));
            mix(sig.modPow(e, n).equals(msg) ? 1 : 0);
            BigInteger dp = d.mod(p.subtract(BigInteger.ONE)), dq = d.mod(q.subtract(BigInteger.ONE));
            BigInteger s1 = msg.modPow(dp, p), s2 = msg.modPow(dq, q);
            mix(Arrays.hashCode(s1.toByteArray())); mix(Arrays.hashCode(s2.toByteArray()));
        }
        out[17] = h;
        return out;
    }

    static void setup() throws Exception {
        Class<BigInteger> bi = BigInteger.class;
        MTL = find(bi, "implMultiplyToLen");
        SQ = find(bi, "implSquareToLen");
        IMA = find(bi, "implMulAdd");
        MA = find(bi, "mulAdd");
        MM = find(bi, "implMontgomeryMultiply");
        MS = find(bi, "implMontgomerySquare");
        SL = find(bi, "shiftLeftImplWorker");
        SR = find(bi, "shiftRightImplWorker");
        Class<?> mp = Class.forName("sun.security.util.math.intpoly.MontgomeryIntegerPolynomialP256");
        Field one = mp.getDeclaredField("ONE");
        P256 = one.get(null);
        PMULT = find(mp, "mult");
        PSQ = find(mp, "square");
        CASSIGN = find(Class.forName("sun.security.util.math.intpoly.IntegerPolynomial"), "conditionalAssign");
    }

    static void fuzz(boolean hot) throws Exception {
        setup();
        long[] first = battery();
        for (int i = 0; i < NAMES.length; i++) System.out.printf("%-24s %016x%n", NAMES[i], first[i]);
        System.out.print(excLog);
        if (hot) {
            int drift = 0;
            for (int it = 0; it < 30; it++) {
                long[] again = battery();
                for (int i = 0; i < NAMES.length; i++) {
                    if (again[i] != first[i]) {
                        drift++;
                        if (drift <= 10) System.out.printf("DRIFT it=%d %s %016x%n", it, NAMES[i], again[i]);
                    }
                }
            }
            System.out.println("drift " + drift);
        }
        System.out.println("DONE SbrW5Bigint fuzz");
    }

    // ------------------------------------------------------------------ perf
    static volatile long sink;

    static void perf(int rounds) throws Exception {
        Random r = new Random(7);
        BigInteger m2048 = new BigInteger(2048, r).setBit(2047).setBit(0);
        BigInteger e2048 = new BigInteger(2048, r), b2048 = new BigInteger(2040, r);
        BigInteger m1024 = new BigInteger(1024, r).setBit(1023).setBit(0);
        BigInteger e1024 = new BigInteger(1024, r), b1024 = new BigInteger(1020, r);

        KeyPairGenerator rk = KeyPairGenerator.getInstance("RSA");
        rk.initialize(2048);
        KeyPair rsa = rk.generateKeyPair();
        KeyPairGenerator ek = KeyPairGenerator.getInstance("EC");
        ek.initialize(new ECGenParameterSpec("secp256r1"));
        KeyPair ec1 = ek.generateKeyPair(), ec2 = ek.generateKeyPair();
        byte[] msg = new byte[256];
        for (int i = 0; i < msg.length; i++) msg[i] = (byte) (i * 31);

        String[] ops = {"modPow1024", "modPow2048", "rsaSign", "rsaVerify", "ecdh", "ecdsaSign", "ecdsaVerify"};
        int[] iters = {20, 4, 8, 60, 20, 20, 20};
        double[] best = new double[ops.length];
        Arrays.fill(best, 1e18);
        byte[] rsaSig = null, ecSig = null;
        for (int round = 0; round <= rounds; round++) {
            StringBuilder line = new StringBuilder("round " + round);
            for (int op = 0; op < ops.length; op++) {
                int n = iters[op];
                long t0 = System.nanoTime();
                for (int i = 0; i < n; i++) {
                    switch (op) {
                        case 0: sink += b1024.modPow(e1024, m1024).intValue(); break;
                        case 1: sink += b2048.modPow(e2048, m2048).intValue(); break;
                        case 2: {
                            Signature s = Signature.getInstance("SHA256withRSA");
                            s.initSign(rsa.getPrivate()); s.update(msg); rsaSig = s.sign(); sink += rsaSig[i & 63]; break;
                        }
                        case 3: {
                            Signature s = Signature.getInstance("SHA256withRSA");
                            s.initVerify(rsa.getPublic()); s.update(msg); sink += s.verify(rsaSig) ? 1 : 1000; break;
                        }
                        case 4: {
                            KeyAgreement ka = KeyAgreement.getInstance("ECDH");
                            ka.init(ec1.getPrivate()); ka.doPhase(ec2.getPublic(), true); sink += ka.generateSecret()[i & 31]; break;
                        }
                        case 5: {
                            Signature s = Signature.getInstance("SHA256withECDSA");
                            s.initSign(ec1.getPrivate()); s.update(msg); ecSig = s.sign(); sink += ecSig[i & 15]; break;
                        }
                        default: {
                            Signature s = Signature.getInstance("SHA256withECDSA");
                            s.initVerify(ec1.getPublic()); s.update(msg); sink += s.verify(ecSig) ? 1 : 1000; break;
                        }
                    }
                }
                double us = (System.nanoTime() - t0) / 1e3 / n;
                if (round > 0) best[op] = Math.min(best[op], us);
                line.append(String.format(" %s=%.0f", ops[op], us));
            }
            System.out.println(line + " us/op");
        }
        StringBuilder b = new StringBuilder("BEST");
        for (int op = 0; op < ops.length; op++) b.append(String.format(" %s=%.0f", ops[op], best[op]));
        System.out.println(b + " us/op  sink=" + (sink != 0));
        System.out.println("check modPow2048=" + Integer.toHexString(b2048.modPow(e2048, m2048).hashCode()));
    }

    /** Print P256 `mult` on out-of-range limbs as Rust test literals (decimal). */
    static void limbs() throws Exception {
        setup();
        long m = Long.MIN_VALUE, x = Long.MAX_VALUE, t = 1L << 52;
        long[][][] cases = {
            {{-1, -1, -1, -1, -1}, {-1, -1, -1, -1, -1}},
            {{m, m, m, m, m}, {1, 2, 3, 4, 5}},
            {{t, t, t, t, t}, {t - 1, t - 1, t - 1, t - 1, t - 1}},
            {{0x000fffffffffffffL, 0x00000fffffffffffL, 0L, 0x0000001000000000L, 0x0000ffffffff0000L},
             {0x000fffffffffffffL, 0x00000fffffffffffL, 0L, 0x0000001000000000L, 0x0000ffffffff0000L}},
            {{-1, 0, -1, 0, -1}, {x, 12345, x, -98765, 7}},
        };
        for (long[][] c : cases) {
            long[] r = new long[5];
            PMULT.invoke(P256, c[0], c[1], r);
            System.out.println("(&" + Arrays.toString(c[0]) + ", &" + Arrays.toString(c[1]) + ", &" + Arrays.toString(r) + "),");
        }
    }

    public static void main(String[] a) throws Exception {
        String mode = a.length > 0 ? a[0] : "fuzz";
        if (mode.equals("limbs")) { limbs(); return; }
        if (mode.equals("perf")) perf(a.length > 1 ? Integer.parseInt(a[1]) : 5);
        else fuzz(a.length > 1 && a[1].equals("hot"));
    }
}
