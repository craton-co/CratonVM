import java.math.BigInteger;
import org.bouncycastle.asn1.x9.X9ECParameters;
import org.bouncycastle.asn1.x9.ECNamedCurveTable;
import org.bouncycastle.crypto.ec.CustomNamedCurves;
import org.bouncycastle.math.ec.ECPoint;

// Deterministic EC multiply cross-check: prints affine coords of G*k for a
// fixed scalar on several curves, so CratonVM output can be diffed vs HotSpot.
public class ECVerify {
    static void show(String tag, X9ECParameters x9, BigInteger k) {
        if (x9 == null) { System.out.println(tag + " null"); return; }
        ECPoint p = x9.getG().multiply(k).normalize();
        System.out.println(tag + " x=" + p.getAffineXCoord().toBigInteger().toString(16)
            + " y=" + p.getAffineYCoord().toBigInteger().toString(16));
    }
    public static void main(String[] a) {
        BigInteger k = new BigInteger("123456789abcdef0fedcba98765432100123456789abcdef0", 16);
        show("named/prime256v1", ECNamedCurveTable.getByName("prime256v1"), k);
        show("named/secp384r1 ", ECNamedCurveTable.getByName("secp384r1"), k);
        show("named/brainpoolP256r1", ECNamedCurveTable.getByName("brainpoolP256r1"), k);
        // also exercise add/subtract heavily via Shamir-style combo
        X9ECParameters x9 = ECNamedCurveTable.getByName("prime256v1");
        ECPoint g = x9.getG();
        ECPoint acc = g.multiply(k).add(g.multiply(k.shiftRight(3))).subtract(g).normalize();
        System.out.println("combo x=" + acc.getAffineXCoord().toBigInteger().toString(16));
    }
}
