import java.security.Provider;
import java.security.Security;
import javax.crypto.Mac;
import javax.crypto.spec.SecretKeySpec;

/**
 * E25-R11 TASK 1: the seventeenth public Mac method,
 * getInstance(String, Provider), and the ownership check added to the
 * (String, String) overload -- which "checked provider EXISTENCE and never
 * OWNERSHIP".
 */
public class MacSurfaceProbe {
    interface Call { Object run() throws Exception; }

    static void t(String label, Call c) {
        String out;
        try {
            Object v = c.run();
            out = (v instanceof Mac)
                    ? "OK alg=" + ((Mac) v).getAlgorithm()
                        + " provider=" + ((Mac) v).getProvider().getName()
                        + " len=" + ((Mac) v).getMacLength()
                    : "OK " + v;
        } catch (Throwable e) {
            out = e.getClass().getName() + ": " + e.getMessage();
        }
        System.out.println(label + " => " + out);
    }

    public static void main(String[] a) throws Exception {
        Provider sunJce = Security.getProvider("SunJCE");
        System.out.println("SunJCE present => " + (sunJce != null));

        // The 17th method: getInstance(String, Provider).
        t("Mac.getInstance(HmacSHA256, providerObj)", () -> Mac.getInstance("HmacSHA256", sunJce));
        t("Mac.getInstance(HmacSHA1, providerObj)", () -> Mac.getInstance("HmacSHA1", sunJce));
        t("Mac.getInstance(NoSuchMac, providerObj)", () -> Mac.getInstance("NoSuchMac", sunJce));

        // OWNERSHIP, not existence: SUN exists but owns no Mac algorithm, so
        // this must be NoSuchAlgorithmException naming SUN -- not a success and
        // not NoSuchProviderException.
        t("Mac.getInstance(HmacSHA256, \"SUN\")", () -> Mac.getInstance("HmacSHA256", "SUN"));
        t("Mac.getInstance(HmacSHA256, \"SunJCE\")", () -> Mac.getInstance("HmacSHA256", "SunJCE"));
        t("Mac.getInstance(HmacSHA256, \"Ghost\")", () -> Mac.getInstance("HmacSHA256", "Ghost"));
        t("Mac.getInstance(HmacSHA256)", () -> Mac.getInstance("HmacSHA256"));

        // The SPI must actually compute, not merely resolve.
        Mac m = Mac.getInstance("HmacSHA256");
        m.init(new SecretKeySpec(new byte[] { 1, 2, 3, 4 }, "HmacSHA256"));
        m.update(new byte[] { 9, 9, 9 });
        byte[] d = m.doFinal();
        StringBuilder sb = new StringBuilder();
        for (byte b : d) sb.append(String.format("%02x", b));
        System.out.println("HmacSHA256 digest = " + sb);
        System.out.println("RESULT done");
    }
}
