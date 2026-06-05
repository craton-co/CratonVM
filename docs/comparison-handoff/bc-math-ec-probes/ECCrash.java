import java.math.BigInteger;
import java.util.ArrayList;
import java.util.Enumeration;
import java.util.List;
import java.util.Random;
import org.bouncycastle.asn1.x9.X9ECParameters;
import org.bouncycastle.asn1.x9.ECNamedCurveTable;
import org.bouncycastle.crypto.ec.CustomNamedCurves;
import org.bouncycastle.math.ec.ECCurve;
import org.bouncycastle.math.ec.ECPoint;

// Deterministic reduce of the bc-math-ec silent rc=1 crash. Fixed-seed Random
// (not SecureRandom) + stable enumeration order => same crash point each run.
// Prints + flushes the curve/coord BEFORE operating, so the LAST line on disk
// names the failing curve when the process dies silently.
public class ECCrash {
    static Random RND = new Random(0xBCEC1234L);

    public static void main(String[] args) {
        int passes = args.length > 0 ? Integer.parseInt(args[0]) : 50;
        List names = new ArrayList();
        for (Enumeration e = ECNamedCurveTable.getNames(); e.hasMoreElements(); ) names.add(e.nextElement());
        for (Enumeration e = CustomNamedCurves.getNames(); e.hasMoreElements(); ) names.add(e.nextElement());
        int[] coords = ECCurve.getAllCoordinateSystems();

        for (int p = 0; p < passes; p++) {
            for (int ni = 0; ni < names.size(); ni++) {
                String name = (String) names.get(ni);
                for (int which = 0; which < 2; which++) {
                    X9ECParameters x9 = which == 0
                        ? ECNamedCurveTable.getByName(name)
                        : CustomNamedCurves.getByName(name);
                    if (x9 == null) continue;
                    ECCurve curve = x9.getCurve();
                    int nbits = x9.getN().bitLength();
                    for (int ci = 0; ci < coords.length; ci++) {
                        int coord = coords[ci];
                        ECCurve c;
                        ECPoint g;
                        if (curve.getCoordinateSystem() == coord) {
                            c = curve; g = x9.getG();
                        } else if (curve.supportsCoordinateSystem(coord)) {
                            c = curve.configure().setCoordinateSystem(coord).create();
                            g = c.importPoint(x9.getG());
                        } else {
                            continue;
                        }
                        System.out.println("p=" + p + " " + (which == 0 ? "named" : "custom")
                            + "/" + name + " coord=" + coord + " bits=" + nbits);
                        System.out.flush();
                        BigInteger k = new BigInteger(nbits, RND);
                        ECPoint q = g.multiply(k).normalize();
                        ECPoint r = q.add(g).twice().normalize();
                        if (r == null) System.out.println("?");
                    }
                }
            }
        }
        System.out.println("ALL DONE passes=" + passes);
        System.out.flush();
    }
}
