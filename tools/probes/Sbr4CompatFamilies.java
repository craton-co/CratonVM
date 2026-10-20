import java.lang.reflect.Modifier;
import java.security.MessageDigest;
import java.security.Provider;
import java.security.Security;
import java.security.Signature;
import java.util.*;
import java.util.concurrent.*;
import javax.crypto.Cipher;
import javax.crypto.Mac;
import javax.crypto.spec.SecretKeySpec;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLEngine;
import javax.net.ssl.SSLParameters;

/**
 * SBR round 4, lane compat: HotSpot-diffed probe for the families {@code --compatible} adopts
 * ({@code jdk-fork-join-pool}, {@code jdk-providers}) and the SSLEngine carrier.
 * Run under HotSpot, {@code --jdk-only} and {@code --compatible}; the three outputs must be
 * byte-identical (after the hash-normalisation below).
 * Sections: provider identity / class / services, provider consumers (Cipher, MessageDigest, Mac,
 * Signature, SSLContext), SSLEngine parameters, ForkJoinPool threads and tasks.
 */
public class Sbr4CompatFamilies {
    static String norm(Object v) {
        return String.valueOf(v).replaceAll("@[0-9a-f]+", "@X").replaceAll("-worker-[0-9]+", "-worker-N");
    }

    static void row(String name, Object v) {
        System.out.println(name + " |" + norm(v) + "|");
    }

    interface T {
        Object run() throws Throwable;
    }

    static void t(String name, T r) {
        try {
            row(name, r.run());
        } catch (Throwable e) {
            row(name, "THREW " + e.getClass().getName() + ": " + norm(e.getMessage()));
        }
    }

    public static void main(String[] args) throws Throwable {
        provider();
        consumers();
        engine();
        forkJoin();
    }

    static void provider() throws Throwable {
        System.out.println("== provider");
        Provider sun = Security.getProvider("SUN");
        t("sun.class", () -> sun.getClass().getName());
        t("sun.super", () -> sun.getClass().getSuperclass().getName());
        t("sun.info", () -> sun.getInfo());
        t("sun.versionStr", () -> sun.getVersionStr());
        t("sun.toString", () -> sun.toString());
        t("sun.same", () -> sun == Security.getProvider("SUN"));
        t("sun.equals", () -> sun.equals(Security.getProvider("SUN")));
        t("sun.hashStable", () -> sun.hashCode() == Security.getProvider("SUN").hashCode());
        t("sun.sha256.class", () -> sun.getService("MessageDigest", "SHA-256").getClassName());
        t("sun.sha256.provider", () -> sun.getService("MessageDigest", "SHA-256").getProvider() == sun);
        t("sun.service.count", () -> sun.getServices().size() > 20);
        t("sun.keys.some", () -> sun.keySet().stream().anyMatch(k -> k.toString().startsWith("MessageDigest.")));
        t("sun.get", () -> sun.get("MessageDigest.SHA-256"));
        t("sun.getProperty", () -> sun.getProperty("MessageDigest.SHA-256"));
        List<String> names = new ArrayList<>();
        List<String> classes = new ArrayList<>();
        for (Provider p : Security.getProviders()) {
            names.add(p.getName());
            classes.add(p.getClass().getSimpleName());
        }
        t("providers.names", () -> names);
        t("providers.classes", () -> classes);
        t("providers.same", () -> Security.getProviders()[0] == Security.getProviders()[0]);
        t("getProvider.missing", () -> Security.getProvider("NoSuchProvider"));
        t("getProvider.null", () -> Security.getProvider(null));
        Provider mine = new Provider("SbrCompat", "1.5", "probe provider") {};
        t("mine.name", () -> mine.getName());
        t("mine.version", () -> mine.getVersionStr());
        t("mine.empty", () -> mine.isEmpty());
        mine.put("MessageDigest.SBRC", "sun.security.provider.SHA2$SHA256");
        t("mine.service", () -> mine.getService("MessageDigest", "SBRC").getClassName());
        t("mine.notEqualsSun", () -> mine.equals(sun));
        t("addProvider", () -> Security.addProvider(mine));
        t("addProvider.again", () -> Security.addProvider(mine));
        t("added.same", () -> Security.getProvider("SbrCompat") == mine);
        t("added.position", () -> {
            List<String> n = new ArrayList<>();
            for (Provider p : Security.getProviders()) n.add(p.getName());
            return n.indexOf("SbrCompat") == n.size() - 1;
        });
        t("added.digest", () -> MessageDigest.getInstance("SBRC").getProvider() == mine);
        t("removeProvider", () -> { Security.removeProvider("SbrCompat"); return Security.getProvider("SbrCompat"); });
        t("insertAt", () -> Security.insertProviderAt(mine, 1));
        t("insertAt.first", () -> Security.getProviders()[0] == mine);
        t("insertAt.remove", () -> { Security.removeProvider("SbrCompat"); return Security.getProviders()[0].getName(); });
        t("getAlgorithms", () -> Security.getAlgorithms("MessageDigest").contains("SHA-256"));
        t("getProperty", () -> Security.getProperty("keystore.type"));
        t("setProperty", () -> { Security.setProperty("sbr.compat", "v"); return Security.getProperty("sbr.compat"); });
    }

    static void consumers() throws Throwable {
        System.out.println("== consumers");
        Provider sun = Security.getProvider("SUN");
        Provider jce = Security.getProvider("SunJCE");
        t("jce.class", () -> jce.getClass().getName());
        t("md.provider", () -> MessageDigest.getInstance("SHA-256").getProvider() == sun);
        t("md.provider.class", () -> MessageDigest.getInstance("SHA-256").getProvider().getClass().getName());
        t("md.digest", () -> HexFormat.of().formatHex(MessageDigest.getInstance("SHA-256").digest("abc".getBytes())));
        t("md.named", () -> MessageDigest.getInstance("SHA-1", "SUN").getAlgorithm());
        t("md.noProvider", () -> MessageDigest.getInstance("SHA-1", "NoSuch"));
        t("sig.provider", () -> Signature.getInstance("SHA256withRSA").getProvider().getName());
        t("sig.provider.class", () -> Signature.getInstance("SHA256withRSA").getProvider().getClass().getName());
        t("cipher.provider", () -> Cipher.getInstance("AES/CBC/PKCS5Padding").getProvider() == jce);
        t("cipher.provider.class", () -> Cipher.getInstance("AES").getProvider().getClass().getName());
        t("cipher.provider.name", () -> Cipher.getInstance("AES").getProvider().getName());
        t("cipher.byProvider", () -> Cipher.getInstance("AES", jce).getProvider() == jce);
        t("mac.provider", () -> Mac.getInstance("HmacSHA256").getProvider() == jce);
        t("mac.value", () -> {
            Mac m = Mac.getInstance("HmacSHA256");
            m.init(new SecretKeySpec("k".getBytes(), "HmacSHA256"));
            return HexFormat.of().formatHex(m.doFinal("m".getBytes()));
        });
        t("sslctx.provider", () -> SSLContext.getInstance("TLS").getProvider().getName());
        t("sslctx.provider.class", () -> SSLContext.getInstance("TLS").getProvider().getClass().getName());
        t("sslctx.providerEquals", () -> SSLContext.getInstance("TLS").getProvider()
                .equals(SSLContext.getDefault().getProvider()));
        t("sslctx.providerSame", () -> SSLContext.getInstance("TLS").getProvider() == Security.getProvider("SunJSSE"));
    }

    static void engine() throws Throwable {
        System.out.println("== engine");
        SSLEngine e = SSLContext.getDefault().createSSLEngine();
        t("engine.class", () -> e.getClass().getName());
        t("engine.fresh.constraints", () -> e.getSSLParameters().getAlgorithmConstraints() != null);
        t("engine.fresh.constraints.class", () -> e.getSSLParameters().getAlgorithmConstraints().getClass().getName());
        t("engine.fresh.alpn", () -> Arrays.toString(e.getSSLParameters().getApplicationProtocols()));
        t("engine.fresh.protocols", () -> e.getSSLParameters().getProtocols() != null);
        t("engine.fresh.endpoint", () -> e.getSSLParameters().getEndpointIdentificationAlgorithm());
        SSLParameters q = e.getSSLParameters();
        q.setApplicationProtocols(new String[] {"h2"});
        q.setEndpointIdentificationAlgorithm("HTTPS");
        e.setSSLParameters(q);
        t("engine.set.alpn", () -> Arrays.toString(e.getSSLParameters().getApplicationProtocols()));
        t("engine.set.endpoint", () -> e.getSSLParameters().getEndpointIdentificationAlgorithm());
        t("engine.set.constraints", () -> e.getSSLParameters().getAlgorithmConstraints() != null);
        t("engine.set.useClient", () -> { e.setUseClientMode(true); return e.getUseClientMode(); });
        t("engine.hsStatus", () -> e.getHandshakeStatus().toString());
        t("fresh.params.constraints", () -> new SSLParameters().getAlgorithmConstraints());
    }

    /** Wait for {@code f} without helping to run it (a poll, not a join), then read its result. */
    static <T> T await(ForkJoinTask<T> f) throws Exception {
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(30);
        while (!f.isDone() && System.nanoTime() < deadline) Thread.sleep(1);
        return f.get(1, TimeUnit.SECONDS);
    }

    static void forkJoin() throws Throwable {
        System.out.println("== forkjoin");
        ForkJoinPool common = ForkJoinPool.commonPool();
        t("common.class", () -> common.getClass().getName());
        t("common.same", () -> common == ForkJoinPool.commonPool());
        t("common.submit.thread", () -> {
            Thread w = await(common.submit(() -> Thread.currentThread()));
            return w.getName().startsWith("ForkJoinPool.commonPool-worker-") + "/" + w.isDaemon() + "/"
                    + w.getClass().getName() + "/" + (w != Thread.currentThread());
        });
        t("common.submit.group", () -> await(common.submit(() -> Thread.currentThread().getThreadGroup().getName())));
        t("common.execute.thread", () -> {
            CompletableFuture<String> f = new CompletableFuture<>();
            common.execute(() -> f.complete(Thread.currentThread().getClass().getName()));
            return f.get(10, TimeUnit.SECONDS);
        });
        // `invoke` from an external thread may be helped by the caller (HotSpot too), so the
        // thread of a task is asked of `submit` + a poll that cannot help.
        t("common.task.thread", () -> await(common.submit(new RecursiveTask<Boolean>() {
            protected Boolean compute() {
                return Thread.currentThread() instanceof ForkJoinWorkerThread;
            }
        })));
        t("common.task.inPool", () -> await(common.submit(new RecursiveTask<String>() {
            protected String compute() {
                return ForkJoinTask.inForkJoinPool() + "/" + (ForkJoinTask.getPool() == common);
            }
        })));
        t("common.invoke.result", () -> common.invoke(new RecursiveTask<Integer>() {
            protected Integer compute() {
                return 11;
            }
        }));
        t("main.inPool", () -> ForkJoinTask.inForkJoinPool());
        t("fork.join.sum", () -> {
            class Sum extends RecursiveTask<Long> {
                final int lo, hi;
                Sum(int lo, int hi) { this.lo = lo; this.hi = hi; }
                protected Long compute() {
                    if (hi - lo <= 100) { long s = 0; for (int i = lo; i < hi; i++) s += i; return s; }
                    int mid = (lo + hi) >>> 1;
                    Sum l = new Sum(lo, mid), r = new Sum(mid, hi);
                    l.fork();
                    return r.compute() + l.join();
                }
            }
            return common.invoke(new Sum(0, 100000));
        });
        t("task.state", () -> {
            ForkJoinTask<Integer> f = common.submit(() -> 5);
            f.get();
            return f.isDone() + "/" + f.isCompletedNormally() + "/" + f.getRawResult() + "/" + f.state();
        });
        t("task.exception", () -> {
            ForkJoinTask<Integer> f = common.submit(() -> { throw new IllegalStateException("boom"); });
            try {
                f.get();
                return "no throw";
            } catch (ExecutionException ee) {
                return ee.getCause() + "/" + f.isCompletedAbnormally();
            }
        });
        t("task.cancel", () -> {
            CountDownLatch gate = new CountDownLatch(1);
            ForkJoinTask<Integer> f = common.submit(() -> { gate.await(); return 1; });
            boolean c = f.cancel(true);
            gate.countDown();
            return c + "/" + f.isCancelled();
        });
        t("invokeAll", () -> {
            List<Callable<Integer>> l = List.of(() -> 1, () -> 2, () -> 3);
            int s = 0;
            for (Future<Integer> f : common.invokeAll(l)) s += f.get();
            return s;
        });
        t("invokeAny", () -> common.invokeAny(List.of(() -> 7)));
        t("parallelStream.sum", () -> java.util.stream.IntStream.range(0, 10000).parallel().sum());
        // HELD (`SBR4-COMPAT-families-HELD-20261005.md`): `--compatible` evaluates a parallel stream
        // pipeline on the caller (the native stream pipeline ignores `parallel()`); the only line of
        // this probe where `--compatible` still differs from HotSpot.
        t("held.parallelStream.threads", () -> {
            Set<Boolean> seen = ConcurrentHashMap.newKeySet();
            java.util.stream.IntStream.range(0, 2000).parallel()
                    .forEach(i -> seen.add(Thread.currentThread() instanceof ForkJoinWorkerThread));
            return seen.contains(true);
        });
        t("cf.async.thread", () -> CompletableFuture.supplyAsync(() -> Thread.currentThread().getName()
                .startsWith("ForkJoinPool.commonPool-worker-")).get());
        t("cf.async.daemon", () -> CompletableFuture.supplyAsync(() -> Thread.currentThread().isDaemon()).get());
        t("quiescence", () -> common.awaitQuiescence(5, TimeUnit.SECONDS));
        ForkJoinPool mine = new ForkJoinPool(3);
        t("mine.parallelism", () -> mine.getParallelism());
        t("mine.thread", () -> await(mine.submit(() -> Thread.currentThread().getName().startsWith("ForkJoinPool-"))));
        t("mine.thread.daemon", () -> await(mine.submit(() -> Thread.currentThread().isDaemon())));
        t("mine.getPool", () -> await(mine.submit(() -> ForkJoinTask.getPool() == mine)));
        t("mine.stream", () -> mine.submit(() -> java.util.stream.IntStream.range(0, 1000).parallel().sum()).get());
        mine.shutdown();
        t("mine.terminated", () -> mine.awaitTermination(10, TimeUnit.SECONDS));
        t("mine.isShutdown", () -> mine.isShutdown());
        t("mine.rejects", () -> { mine.submit(() -> 1); return "accepted"; });
    }
}
