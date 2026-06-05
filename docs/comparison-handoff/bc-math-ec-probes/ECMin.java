import java.math.BigInteger;
import java.util.Random;
import org.bouncycastle.asn1.x9.X9ECParameters;
import org.bouncycastle.crypto.ec.CustomNamedCurves;
import org.bouncycastle.math.ec.ECCurve;
import org.bouncycastle.math.ec.ECPoint;

// Minimal repro: custom F2m curve at a chosen coordinate system, one op kind.
// Logs to STDERR (survives a hard crash; CratonVM loses buffered stdout).
// args: [curveName] [coord] [op]  op = mul|add|twice|muladd|multwice|all
public class ECMin {
    public static void main(String[] a) throws Exception {
        String name = a.length > 0 ? a[0] : "sect193r2";
        int coord = a.length > 1 ? Integer.parseInt(a[1]) : 6;
        String op = a.length > 2 ? a[2] : "multwice";
        X9ECParameters x9 = CustomNamedCurves.getByName(name);
        if (x9 == null) { System.err.println(name + " not in CustomNamedCurves"); return; }
        ECCurve curve = x9.getCurve();
        ECCurve c = (curve.getCoordinateSystem() == coord)
            ? curve : curve.configure().setCoordinateSystem(coord).create();
        ECPoint g = (curve.getCoordinateSystem() == coord) ? x9.getG() : c.importPoint(x9.getG());
        int nbits = x9.getN().bitLength();
        Random rnd = new Random(0xBCEC1234L);
        System.err.println("curve=" + name + " coord=" + coord + " op=" + op + " bits=" + nbits
            + " class=" + c.getClass().getName());
        System.err.flush();
        for (int i = 0; i <= 5000; i++) {
            if ((i % 50) == 0) { System.err.println("i=" + i); System.err.flush(); }
            BigInteger k = new BigInteger(nbits, rnd);
            try {
                ECPoint q;
                if (op.equals("mul"))       { q = g.multiply(k).normalize(); }
                else if (op.equals("add"))  { q = g.add(g).normalize(); }
                else if (op.equals("twice")){ q = g.twice().normalize(); }
                else if (op.equals("muladd")){ q = g.multiply(k).normalize().add(g).normalize(); }
                else if (op.equals("multwice")){
                    ECPoint m = g.multiply(k).normalize();
                    q = m.twice().normalize();
                }
                else { ECPoint m = g.multiply(k).normalize(); q = m.add(g).twice().normalize(); }
                if (q == null) System.err.println("null at i=" + i);
            } catch (Throwable t) {
                System.err.println("CAUGHT at i=" + i + " k=" + k.toString(16) + ": " + t);
                t.printStackTrace();
                System.err.flush();
                throw t;
            }
        }
        System.err.println("SURVIVED 2000");
        System.err.flush();
    }
}
