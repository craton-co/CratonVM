import java.security.MessageDigest;
import java.security.MessageDigestSpi;
import java.security.Provider;
import java.security.Security;

/**
 * H13-2 section 2 (`ca8f03069`): "a bare SPI whose Delegate needs the Provider
 * had no wrapper shape, so every standard-shaped MessageDigest provider was
 * unreachable". ProviderLookupProbe cannot see this -- every one of its rows
 * goes through SUN or an unregistered name, so it never registers a provider
 * written to the documented JCA contract.
 *
 * This one does exactly that, and nothing else: a Provider subclass whose only
 * service is a MessageDigest implemented as a bare MessageDigestSpi, reached
 * through all three getInstance overloads. Deterministic output; no timing, no
 * paths, no ephemeral values.
 */
public class CustomProviderSpiProbe {

    public static final class CountingDigestSpi extends MessageDigestSpi {
        private int len;
        @Override protected void engineUpdate(byte b) { len++; }
        @Override protected void engineUpdate(byte[] b, int off, int n) { len += n; }
        @Override protected byte[] engineDigest() { byte[] r = { (byte) len }; len = 0; return r; }
        @Override protected void engineReset() { len = 0; }
        @Override protected int engineGetDigestLength() { return 1; }
    }

    public static final class L7Provider extends Provider {
        public L7Provider() {
            super("L7P", "1.0", "H13-2 section 2 probe provider");
            put("MessageDigest.L7DIGEST", CountingDigestSpi.class.getName());
        }
    }


    /** Extends the ENGINE, not the SPI: the shape the record says already
     *  resolves, because it satisfies is_subclass and never needs a Delegate. */
    public static final class SubclassDigest extends MessageDigest {
        private int len;
        public SubclassDigest() { super("L7SUB"); }
        @Override protected void engineUpdate(byte b) { len++; }
        @Override protected void engineUpdate(byte[] b, int off, int n) { len += n; }
        @Override protected byte[] engineDigest() { byte[] x = { (byte) len }; len = 0; return x; }
        @Override protected void engineReset() { len = 0; }
    }


    /** putService() is protected, so the call must live inside the subclass. */
    public static final class L7QProvider extends Provider {
        public L7QProvider() {
            super("L7Q", "1.0", "putService shape");
            putService(new Provider.Service(
                    this, "MessageDigest", "L7SVC", CountingDigestSpi.class.getName(), null, null));
        }
    }

    public static final class L7RProvider extends Provider {
        public L7RProvider() {
            super("L7R", "1.0", "engine subclass shape");
            put("MessageDigest.L7SUB", SubclassDigest.class.getName());
        }
    }

    interface Call { Object run() throws Exception; }

    static void t(String label, Call c) {
        String out;
        try {
            Object v = c.run();
            out = (v instanceof MessageDigest)
                    ? "OK provider=" + ((MessageDigest) v).getProvider().getName()
                    : "OK " + v;
        } catch (Throwable e) {
            out = e.getClass().getName() + ": " + e.getMessage();
        }
        System.out.println(label + " => " + out);
    }

    public static void main(String[] args) throws Exception {
        Provider p = new L7Provider();
        System.out.println("addProvider position>0 => " + (Security.addProvider(p) > 0));
        System.out.println("getProvider(L7P) name  => "
                + (Security.getProvider("L7P") == null ? "null" : Security.getProvider("L7P").getName()));
        System.out.println("getService present     => " + (p.getService("MessageDigest", "L7DIGEST") != null));

        t("MessageDigest.getInstance(L7DIGEST)",            () -> MessageDigest.getInstance("L7DIGEST"));
        t("MessageDigest.getInstance(L7DIGEST, L7P)",       () -> MessageDigest.getInstance("L7DIGEST", "L7P"));
        t("MessageDigest.getInstance(L7DIGEST, provider)",  () -> MessageDigest.getInstance("L7DIGEST", p));
        t("MessageDigest.getInstance(L7DIGEST, SUN)",       () -> MessageDigest.getInstance("L7DIGEST", "SUN"));

        // The SPI must actually be reached, not merely resolved: a wrapper that
        // resolves and then delegates nowhere passes every row above.
        // Caught, not thrown: a VM that refuses the lookup must still reach the
        // discriminator rows below. An uncaught throw here would leave them
        // UNTESTED while a line-diff reported them as merely "missing".
        try {
            MessageDigest md = MessageDigest.getInstance("L7DIGEST");
            md.update(new byte[] { 1, 2, 3, 4, 5 });
            byte[] d = md.digest();
            System.out.println("digest length          => " + d.length);
            System.out.println("digest byte counts 5   => " + (d.length == 1 && d[0] == 5));
            System.out.println("getDigestLength        => " + md.getDigestLength());
            System.out.println("getAlgorithm           => " + md.getAlgorithm());
        } catch (Throwable e) {
            System.out.println("digest length          => " + e.getClass().getName());
            System.out.println("digest byte counts 5   => unreached");
            System.out.println("getDigestLength        => unreached");
            System.out.println("getAlgorithm           => unreached");
        }

        // ---- discriminator: legacy put() string mapping vs putService() ----
        // The two registration shapes reach the provider chain by different
        // routes. Testing only one cannot say which half a refusal lives in.
        L7QProvider q = new L7QProvider();
        Security.addProvider(q);
        System.out.println("L7Q getService present => " + (q.getService("MessageDigest", "L7SVC") != null));
        t("MessageDigest.getInstance(L7SVC)",      () -> MessageDigest.getInstance("L7SVC"));
        t("MessageDigest.getInstance(L7SVC, L7Q)", () -> MessageDigest.getInstance("L7SVC", "L7Q"));

        // And the engine-subclass shape, which the record says already works
        // (BouncyCastle's route) -- the control for both of the above.
        L7RProvider r = new L7RProvider();
        Security.addProvider(r);
        t("MessageDigest.getInstance(L7SUB, L7R)", () -> MessageDigest.getInstance("L7SUB", "L7R"));

        System.out.println("RESULT done");
    }
}
