// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.lang.ref.WeakReference;
import java.security.cert.CertificateException;
import java.security.cert.X509Certificate;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.function.IntUnaryOperator;
import java.util.logging.Handler;
import java.util.logging.LogRecord;
import java.util.logging.Logger;
import javax.net.ssl.SSLContext;
import javax.net.ssl.TrustManager;
import javax.net.ssl.X509TrustManager;

/**
 * gci i23/ep (2026-10-15): the undeploy shape of
 * {@code docs/known-issues/gc/common-w14d-proposal-ephemeron-native-side-tables.md}.
 *
 * <p>Each "webapp" is a fresh class loader that defines its own copies of
 * {@link Webapp} (a {@code java.util.logging.Handler}) and {@link WebappTm} (an
 * {@code X509TrustManager}). Installing it (a) gets the logger
 * {@code gciep23.app<id>}, adds the webapp's own handler to it and keeps the
 * logger in a static of the webapp class; (b) builds an {@code SSLContext} whose
 * only trust manager is a webapp {@code WebappTm}, kept in another static. So
 * the handler / trust manager reaches its class, its loader, the static, and the
 * logger / context that holds it: a cycle nothing else reaches once the webapp
 * is dropped ("undeploy" without {@code removeHandler}). HotSpot collects it and
 * unloads every dropped loader; on CratonVM the handler list and the trust
 * manager are values of weak-keyed native side tables, reported as strong roots
 * unless {@code CRATONVM_GEN_SIDE_ROW_EPHEMERON=1} (Generational) makes them
 * ephemerons of their key.
 *
 * <p>Soundness half: {@code N} more webapps stay installed across the same
 * collections; afterwards each live logger must still deliver one record to its
 * own handler (its handler list is reached only through the logger).
 *
 * <p>Deterministic stdout (HotSpot prints the same):
 * <pre>
 *   ep23 mode=M loaders=N unloaded=N delivered=D
 * </pre>
 * where {@code D = N} for modes {@code jul} and {@code both}, else 0. Stderr (not
 * compared): {@code [ep23] rounds=R cleared-per-round=... ms=T}.
 * Usage: {@code GciEp23LoaderUnloadProbe [loaders=32] [jul|tls|both]}.
 * Both nested classes must sit beside this one on the classpath (they are read
 * as resources).
 */
public final class GciEp23LoaderUnloadProbe {

    static final int INSTALL_JUL = 1;
    static final int INSTALL_TLS = 2;
    static final int LOG_ONE = 3;

    /** Defined once per webapp loader. {@code applyAsInt((id << 3) | op)}. */
    public static final class Webapp extends Handler implements IntUnaryOperator {
        static Logger log;
        static SSLContext ctx;
        static int delivered;

        public Webapp() {
        }

        @Override
        public void publish(LogRecord record) {
            delivered++;
        }

        @Override
        public void flush() {
        }

        @Override
        public void close() {
        }

        @Override
        public int applyAsInt(int arg) {
            int op = arg & 7;
            int id = arg >>> 3;
            try {
                if (op == INSTALL_JUL) {
                    Logger l = Logger.getLogger("gciep23.app" + id);
                    l.setUseParentHandlers(false);
                    l.addHandler(this);
                    log = l;
                    return 1;
                }
                if (op == INSTALL_TLS) {
                    SSLContext c = SSLContext.getInstance("TLS");
                    c.init(null, new TrustManager[] {new WebappTm(id)}, null);
                    ctx = c;
                    return 1;
                }
                if (op == LOG_ONE) {
                    log.info("ep23 record " + id);
                    return delivered;
                }
                return -1;
            } catch (Exception e) {
                System.err.println("[ep23] webapp " + id + " op " + op + " threw " + e);
                return -2;
            }
        }
    }

    /** Defined once per webapp loader; held only by its webapp's SSLContext. */
    public static final class WebappTm implements X509TrustManager {
        final int[] body;

        public WebappTm(int id) {
            body = new int[64];
            for (int i = 0; i < body.length; i++) {
                body[i] = id * 131 + i;
            }
        }

        @Override
        public void checkClientTrusted(X509Certificate[] chain, String authType)
                throws CertificateException {
            throw new CertificateException("ep23 client " + body[0]);
        }

        @Override
        public void checkServerTrusted(X509Certificate[] chain, String authType)
                throws CertificateException {
            throw new CertificateException("ep23 server " + body[0]);
        }

        @Override
        public X509Certificate[] getAcceptedIssuers() {
            return new X509Certificate[0];
        }
    }

    /** Defines the two webapp classes itself and delegates everything else. */
    static final class WebappLoader extends ClassLoader {
        private final Map<String, byte[]> own;

        WebappLoader(Map<String, byte[]> own) {
            super(GciEp23LoaderUnloadProbe.class.getClassLoader());
            this.own = own;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            byte[] bytes = own.get(name);
            if (bytes != null) {
                synchronized (this) {
                    Class<?> c = findLoadedClass(name);
                    if (c == null) {
                        c = defineClass(name, bytes, 0, bytes.length);
                    }
                    if (resolve) {
                        resolveClass(c);
                    }
                    return c;
                }
            }
            return super.loadClass(name, resolve);
        }
    }

    static volatile Object sink;

    static byte[] readClassBytes(String binaryName) throws Exception {
        String res = binaryName.replace('.', '/') + ".class";
        try (InputStream in = GciEp23LoaderUnloadProbe.class.getClassLoader().getResourceAsStream(res)) {
            if (in == null) {
                throw new IllegalStateException("class bytes not on the classpath: " + res);
            }
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            byte[] buf = new byte[4096];
            int n;
            while ((n = in.read(buf)) > 0) {
                out.write(buf, 0, n);
            }
            return out.toByteArray();
        }
    }

    /** A fresh webapp: its loader, its own classes, one installed instance. */
    static IntUnaryOperator deploy(Map<String, byte[]> own, String appName, int id, boolean jul,
            boolean tls) throws Exception {
        WebappLoader loader = new WebappLoader(own);
        Class<?> c = loader.loadClass(appName);
        if (c.getClassLoader() != loader) {
            throw new IllegalStateException("the webapp was not defined by its own loader");
        }
        IntUnaryOperator app = (IntUnaryOperator) c.getConstructor().newInstance();
        if (jul && app.applyAsInt((id << 3) | INSTALL_JUL) != 1) {
            throw new IllegalStateException("jul install failed for " + id);
        }
        if (tls && app.applyAsInt((id << 3) | INSTALL_TLS) != 1) {
            throw new IllegalStateException("tls install failed for " + id);
        }
        return app;
    }

    /** Deploys a webapp and drops it at once; answers only its loader, weakly. */
    static WeakReference<ClassLoader> deployAndUndeploy(Map<String, byte[]> own, String appName,
            int id, boolean jul, boolean tls) throws Exception {
        IntUnaryOperator app = deploy(own, appName, id, jul, tls);
        return new WeakReference<>(app.getClass().getClassLoader());
    }

    static void churn(int round) {
        Object[] keep = new Object[1024];
        for (int k = 0; k < 131072; k++) {
            long[] o = new long[16];
            o[0] = k + round;
            if ((k & 127) == 0) {
                keep[(k >>> 7) & 1023] = o;
            }
        }
        sink = keep;
    }

    public static void main(String[] args) throws Exception {
        int loaders = args.length > 0 ? Integer.parseInt(args[0]) : 32;
        String mode = args.length > 1 ? args[1] : "both";
        boolean jul = mode.equals("jul") || mode.equals("both");
        boolean tls = mode.equals("tls") || mode.equals("both");
        if (!jul && !tls) {
            throw new IllegalArgumentException("mode must be jul, tls or both: " + mode);
        }
        String appName = GciEp23LoaderUnloadProbe.class.getName() + "$Webapp";
        String tmName = GciEp23LoaderUnloadProbe.class.getName() + "$WebappTm";
        Map<String, byte[]> own = new HashMap<>();
        own.put(appName, readClassBytes(appName));
        own.put(tmName, readClassBytes(tmName));
        long t0 = System.nanoTime();

        IntUnaryOperator[] live = new IntUnaryOperator[loaders];
        List<WeakReference<ClassLoader>> dropped = new ArrayList<>();
        for (int i = 0; i < loaders; i++) {
            live[i] = deploy(own, appName, i, jul, tls);
            dropped.add(deployAndUndeploy(own, appName, loaders + i, jul, tls));
        }

        StringBuilder perRound = new StringBuilder();
        int cleared = 0;
        int rounds = 0;
        for (int r = 0; r < 10; r++) {
            churn(r);
            System.gc();
            rounds++;
            cleared = 0;
            for (WeakReference<ClassLoader> w : dropped) {
                if (w.refersTo(null)) {
                    cleared++;
                }
            }
            perRound.append(r == 0 ? "" : ",").append(cleared);
            if (cleared == loaders) {
                break;
            }
            Thread.sleep(20);
        }
        sink = null;

        int delivered = 0;
        if (jul) {
            for (int i = 0; i < loaders; i++) {
                int d = live[i].applyAsInt((i << 3) | LOG_ONE);
                if (d != 1) {
                    System.err.println("[ep23] live webapp " + i + " delivered " + d);
                }
                delivered += d;
            }
        }
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        System.out.println("ep23 mode=" + mode + " loaders=" + loaders + " unloaded=" + cleared
                + " delivered=" + delivered);
        System.err.println("[ep23] rounds=" + rounds + " cleared-per-round=" + perRound + " ms=" + ms);
    }
}
