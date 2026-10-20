// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.function.IntUnaryOperator;
import java.util.logging.Filter;
import java.util.logging.Handler;
import java.util.logging.Level;
import java.util.logging.LogRecord;
import java.util.logging.Logger;

/**
 * gci i24/lk (2026-10-15): {@code docs/known-issues/gc/gci-i24lk-jul-name-keyed-rows-pin-webapp-loaders-20261015.md}
 * and {@code gci-i24lk-jul-log-and-logp-publish-twice-20261015.md}.
 *
 * <p>Each "webapp" is a fresh class loader that defines its own {@link App} (a
 * {@code java.util.logging.Handler}) and {@link AppFilter} (a {@code Filter}).
 * Installing it gets the logger {@code gcilk24.<d|k><id>}, turns its parent
 * handlers off, adds the webapp's handler and sets the webapp's filter.
 *
 * <ul>
 *   <li>{@code drop}: a dropped webapp keeps NO reference to its logger. On
 *       HotSpot the logger is held weakly by {@code LogManager}, so it, its
 *       handler, its filter and the loader all go: every dropped loader
 *       unloads. No cycle is involved, so no ephemeron is needed.</li>
 *   <li>{@code cycle}: a dropped webapp keeps its logger in a static of its
 *       own class (handler -> class -> loader -> static -> logger -> handler),
 *       the i23/ep shape. HotSpot unloads every dropped loader.</li>
 *   <li>{@code dd}: no loaders. One logger with one counting handler in the
 *       application loader; one call each of {@code log(Level, String)},
 *       {@code logp(Level, String, String, String)} and {@code info(String)}.
 *       HotSpot publishes each record once.</li>
 * </ul>
 *
 * <p>Soundness half (drop / cycle): {@code N} webapps stay installed, each
 * keeping its logger in a static; afterwards each logs one record through each
 * of the three methods, and its handler must see exactly one per call.
 *
 * <p>Deterministic stdout (HotSpot prints the same):
 * <pre>
 *   lk24 mode=drop|cycle loaders=N unloaded=N log=N logp=N info=N
 *   lk24 mode=dd log=1 logp=1 info=1
 * </pre>
 * Stderr (not compared): {@code [lk24] rounds=R cleared-per-round=... ms=T} and
 * one line per live webapp whose counts are not 1.
 * Usage: {@code GciLk24JulRowsProbe [loaders=32] [drop|cycle|dd]}. Both nested
 * webapp classes must sit beside this one on the classpath (they are read as
 * resources).
 */
public final class GciLk24JulRowsProbe {

    static final int INSTALL_KEEP = 1;
    static final int INSTALL_DROP = 2;
    static final int LOG_LOG = 3;
    static final int LOG_LOGP = 4;
    static final int LOG_INFO = 5;

    /** Defined once per webapp loader. {@code applyAsInt((id << 3) | op)}. */
    public static final class App extends Handler implements IntUnaryOperator {
        static Logger log;
        static int delivered;

        public App() {
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
                if (op == INSTALL_KEEP || op == INSTALL_DROP) {
                    String name = (op == INSTALL_KEEP ? "gcilk24.k" : "gcilk24.d") + id;
                    Logger l = Logger.getLogger(name);
                    l.setUseParentHandlers(false);
                    l.addHandler(this);
                    l.setFilter(new AppFilter(id));
                    if (op == INSTALL_KEEP) {
                        log = l;
                    }
                    return 1;
                }
                int before = delivered;
                if (op == LOG_LOG) {
                    log.log(Level.INFO, "lk24 log " + id);
                } else if (op == LOG_LOGP) {
                    log.logp(Level.INFO, "Lk24", "logp", "lk24 logp " + id);
                } else if (op == LOG_INFO) {
                    log.info("lk24 info " + id);
                } else {
                    return -1;
                }
                return delivered - before;
            } catch (Exception e) {
                System.err.println("[lk24] webapp " + id + " op " + op + " threw " + e);
                return -2;
            }
        }
    }

    /** Defined once per webapp loader; held only by its webapp's logger. */
    public static final class AppFilter implements Filter {
        final int[] body;

        public AppFilter(int id) {
            body = new int[64];
            for (int i = 0; i < body.length; i++) {
                body[i] = id * 131 + i;
            }
        }

        @Override
        public boolean isLoggable(LogRecord record) {
            return body.length == 64;
        }
    }

    /** The {@code dd} mode's handler: counts every record it is handed. */
    static final class Counter extends Handler {
        int published;

        @Override
        public void publish(LogRecord record) {
            published++;
        }

        @Override
        public void flush() {
        }

        @Override
        public void close() {
        }
    }

    /** Defines the two webapp classes itself and delegates everything else. */
    static final class WebappLoader extends ClassLoader {
        private final Map<String, byte[]> own;

        WebappLoader(Map<String, byte[]> own) {
            super(GciLk24JulRowsProbe.class.getClassLoader());
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
    static Logger ddLogger;

    static byte[] readClassBytes(String binaryName) throws Exception {
        String res = binaryName.replace('.', '/') + ".class";
        try (InputStream in = GciLk24JulRowsProbe.class.getClassLoader().getResourceAsStream(res)) {
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
    static IntUnaryOperator deploy(Map<String, byte[]> own, String appName, int id, int install)
            throws Exception {
        WebappLoader loader = new WebappLoader(own);
        Class<?> c = loader.loadClass(appName);
        if (c.getClassLoader() != loader) {
            throw new IllegalStateException("the webapp was not defined by its own loader");
        }
        IntUnaryOperator app = (IntUnaryOperator) c.getConstructor().newInstance();
        if (app.applyAsInt((id << 3) | install) != 1) {
            throw new IllegalStateException("install failed for " + id);
        }
        return app;
    }

    /** Deploys a webapp and drops it at once; answers only its loader, weakly. */
    static WeakReference<ClassLoader> deployAndUndeploy(Map<String, byte[]> own, String appName,
            int id, int install) throws Exception {
        IntUnaryOperator app = deploy(own, appName, id, install);
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

    static void doubleDelivery() {
        Logger l = Logger.getLogger("gcilk24.dd");
        ddLogger = l;
        l.setUseParentHandlers(false);
        Counter counter = new Counter();
        l.addHandler(counter);
        int before = counter.published;
        l.log(Level.INFO, "lk24 dd log");
        int log = counter.published - before;
        before = counter.published;
        l.logp(Level.INFO, "Lk24", "dd", "lk24 dd logp");
        int logp = counter.published - before;
        before = counter.published;
        l.info("lk24 dd info");
        int info = counter.published - before;
        System.out.println("lk24 mode=dd log=" + log + " logp=" + logp + " info=" + info);
    }

    public static void main(String[] args) throws Exception {
        int loaders = args.length > 0 ? Integer.parseInt(args[0]) : 32;
        String mode = args.length > 1 ? args[1] : "drop";
        if (mode.equals("dd")) {
            doubleDelivery();
            return;
        }
        boolean cycle = mode.equals("cycle");
        if (!cycle && !mode.equals("drop")) {
            throw new IllegalArgumentException("mode must be drop, cycle or dd: " + mode);
        }
        String appName = GciLk24JulRowsProbe.class.getName() + "$App";
        String filterName = GciLk24JulRowsProbe.class.getName() + "$AppFilter";
        Map<String, byte[]> own = new HashMap<>();
        own.put(appName, readClassBytes(appName));
        own.put(filterName, readClassBytes(filterName));
        long t0 = System.nanoTime();

        IntUnaryOperator[] live = new IntUnaryOperator[loaders];
        List<WeakReference<ClassLoader>> dropped = new ArrayList<>();
        int dropInstall = cycle ? INSTALL_KEEP : INSTALL_DROP;
        for (int i = 0; i < loaders; i++) {
            live[i] = deploy(own, appName, i, INSTALL_KEEP);
            // A dropped webapp's id is offset so its logger name never matches a live one.
            dropped.add(deployAndUndeploy(own, appName, loaders + i, dropInstall));
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

        int[] totals = new int[3];
        int[] ops = {LOG_LOG, LOG_LOGP, LOG_INFO};
        for (int i = 0; i < loaders; i++) {
            for (int k = 0; k < ops.length; k++) {
                int d = live[i].applyAsInt((i << 3) | ops[k]);
                if (d != 1) {
                    System.err.println("[lk24] live webapp " + i + " op " + ops[k] + " delivered " + d);
                }
                totals[k] += d;
            }
        }
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        System.out.println("lk24 mode=" + mode + " loaders=" + loaders + " unloaded=" + cleared
                + " log=" + totals[0] + " logp=" + totals[1] + " info=" + totals[2]);
        System.err.println("[lk24] rounds=" + rounds + " cleared-per-round=" + perRound + " ms=" + ms);
    }
}
