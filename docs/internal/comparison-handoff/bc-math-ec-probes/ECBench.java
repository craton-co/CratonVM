import java.math.BigInteger;
import java.security.SecureRandom;
import org.bouncycastle.asn1.x9.X9ECParameters;
import org.bouncycastle.asn1.x9.ECNamedCurveTable;
import org.bouncycastle.crypto.ec.CustomNamedCurves;
import org.bouncycastle.math.ec.ECPoint;

public class ECBench {
    static SecureRandom RND = new SecureRandom();

    static void bench(String tag, X9ECParameters x9, int iters) {
        if (x9 == null) { System.out.println(tag + ": (null)"); return; }
        ECPoint g = x9.getG();
        int nbits = x9.getN().bitLength();
        // warm
        g.multiply(new BigInteger(nbits, RND)).normalize();
        long t0 = System.nanoTime();
        ECPoint acc = g;
        for (int i = 0; i < iters; i++) {
            BigInteger k = new BigInteger(nbits, RND);
            acc = g.multiply(k).normalize();
        }
        long t1 = System.nanoTime();
        double ms = (t1 - t0) / 1e6;
        System.out.printf("%-28s field=%-4s iters=%d total=%.1fms per=%.2fms%n",
            tag, x9.getCurve().getFieldSize() <= 0 ? "?" : "" + x9.getCurve().getFieldSize(),
            iters, ms, ms / iters);
    }

    public static void main(String[] a) {
        int iters = a.length > 0 ? Integer.parseInt(a[0]) : 20;
        System.out.println("=== ECBench iters=" + iters + " ===");
        // Fp generic (ECCurve.Fp, BigInteger-backed)
        bench("named/prime256v1(Fp gen)", ECNamedCurveTable.getByName("prime256v1"), iters);
        // Fp custom (SecP256R1, int[] Nat)
        bench("custom/secp256r1(Fp cust)", CustomNamedCurves.getByName("secp256r1"), iters);
        // F2m generic (ECCurve.F2m, LongArray)
        bench("named/sect233r1(F2m gen)", ECNamedCurveTable.getByName("sect233r1"), iters);
        // F2m custom
        bench("custom/sect233r1(F2m cust)", CustomNamedCurves.getByName("sect233r1"), iters);
        // larger F2m
        bench("named/sect571r1(F2m gen)", ECNamedCurveTable.getByName("sect571r1"), iters);
        System.out.println("=== done ===");
    }
}
