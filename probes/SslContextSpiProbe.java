import java.lang.reflect.Method;
import java.security.Provider;
import java.security.Security;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLEngine;
import javax.net.ssl.SSLParameters;

/**
 * The whole `javax.net.ssl.SSLContext` surface CratonVM registers natives on,
 * printed for a context built around a THIRD PARTY's `SSLContextSpi` and for a
 * context CratonVM built itself.
 *
 * Both arms matter. The defect this probes
 * (`sslcontext-natives-ignore-a-third-party-spi`) is that the natives answered
 * for an object somebody else built; the fix must move the BC arm to HotSpot's
 * answers and leave the CratonVM arm exactly where it was, because the netty
 * TLS classes exercise that one exclusively. A probe that printed only the BC
 * arm could be satisfied by delegating everything, which is the fix that would
 * break them.
 *
 * Run the same command on HotSpot and on CratonVM and diff. Every line is
 * `KEY = value`, one per line, so `diff` is the whole reading protocol.
 *
 *   java @/tmp/bc18.args SslContextSpiProbe
 *   <cratonvm> --java-home <jdk> @/tmp/bc18.args SslContextSpiProbe
 *
 * `/tmp/bc18.args` must NOT carry the `*-jdk15on-1.70` jars — `bctls-jdk18on`
 * resolves `NISTObjectIdentifiers` from 1.70 and dies in `TlsUtils.<clinit>` on
 * both VMs otherwise. See the page's Repro section.
 */
public final class SslContextSpiProbe {

    public static void main(String[] args) throws Exception {
        // bcprov FIRST: BouncyCastle's JSSE builds its `JcaTlsCrypto` through
        // `SecureRandom.getInstance("DEFAULT")`, an algorithm only the bcprov
        // provider registers. Without it `init` fails with "unable to create
        // JcaTlsCrypto: DEFAULT SecureRandom not available" and every method
        // after it answers "SSLContext has not been initialized" — on HotSpot
        // too. The two VMs would then AGREE, for a reason that has nothing to
        // do with either of them.
        add("org.bouncycastle.jce.provider.BouncyCastleProvider");
        Provider bc = add("org.bouncycastle.jsse.provider.BouncyCastleJsseProvider");

        if (bc != null) {
            report("bc", SSLContext.getInstance("TLSv1.3", bc));
        }
        // The control arm: whatever `getInstance(String)` alone produces. On
        // CratonVM that is the synthetic context the natives own.
        report("own", SSLContext.getInstance("TLSv1.3"));
    }

    private static Provider add(String className) {
        try {
            Class<?> c = Class.forName(className);
            Provider p = (Provider) c.getDeclaredConstructor().newInstance();
            Security.addProvider(p);
            return p;
        } catch (Throwable t) {
            System.out.println("PROVIDER_LOAD_FAILED " + className + " = " + t);
            return null;
        }
    }

    private static void report(String tag, SSLContext ctx) {
        line(tag, "contextClass", ctx.getClass().getName());
        line(tag, "contextSpiClass", spiClassOf(ctx));
        one(tag, "getProtocol", () -> ctx.getProtocol());
        one(tag, "getProvider", () -> String.valueOf(ctx.getProvider()));
        // `init` first: the factory getters below are contracted to fail on an
        // un-initialised context, so an un-init'd probe would print a refusal
        // and call it a divergence.
        one(tag, "init", () -> {
            try {
                ctx.init(null, null, null);
                return "ok";
            } catch (Exception e) {
                return e.getClass().getName() + ": " + e.getMessage();
            }
        });
        one(tag, "createSSLEngine", () -> classOf(ctx.createSSLEngine()));
        one(tag, "createSSLEngine(h,p)", () -> classOf(ctx.createSSLEngine("example.test", 443)));
        // `toString()` alone is what caught the un-initialised engine: it threw
        // `NPE: Cannot read field "conSession" because "this.conContext" is null`
        // on an engine BC never built.
        one(tag, "engineToString", () -> {
            SSLEngine e = ctx.createSSLEngine();
            String s = String.valueOf(e);
            // Do not print the identity hash — it differs run to run.
            return s.replaceAll("@[0-9a-fA-F]+", "@X");
        });
        one(tag, "getSocketFactory", () -> classOf(ctx.getSocketFactory()));
        one(tag, "getServerSocketFactory", () -> classOf(ctx.getServerSocketFactory()));
        one(tag, "getClientSessionContext", () -> classOf(ctx.getClientSessionContext()));
        one(tag, "getServerSessionContext", () -> classOf(ctx.getServerSessionContext()));
        one(tag, "defaultProtocols", () -> protos(ctx.getDefaultSSLParameters()));
        one(tag, "supportedProtocols", () -> protos(ctx.getSupportedSSLParameters()));
    }

    private static String spiClassOf(SSLContext ctx) {
        try {
            java.lang.reflect.Field f = SSLContext.class.getDeclaredField("contextSpi");
            f.setAccessible(true);
            return classOf(f.get(ctx));
        } catch (Throwable t) {
            return "<unreadable: " + t.getClass().getName() + ">";
        }
    }

    private static String protos(SSLParameters p) {
        String[] a = p.getProtocols();
        return a == null ? "null" : String.join(",", a);
    }

    private static String classOf(Object o) {
        return o == null ? "null" : o.getClass().getName();
    }

    private interface Probe {
        String get() throws Exception;
    }

    private static void one(String tag, String key, Probe p) {
        String v;
        try {
            v = p.get();
        } catch (Throwable t) {
            v = "THREW " + t.getClass().getName() + ": " + t.getMessage();
        }
        line(tag, key, v);
    }

    private static void line(String tag, String key, String value) {
        System.out.println(tag + "." + key + " = " + value);
        System.out.flush();
    }

    static {
        // Keep `Method` imported-and-used so an unused-import warning cannot
        // hide a real one.
        Method[] unused = SSLContext.class.getMethods();
        if (unused.length == 0) {
            throw new IllegalStateException("SSLContext has no methods");
        }
    }
}
