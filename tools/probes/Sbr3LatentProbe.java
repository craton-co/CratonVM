import java.security.Provider;
import java.security.Security;
import java.util.*;
import java.util.concurrent.*;
import javax.crypto.Cipher;
import javax.crypto.spec.GCMParameterSpec;
import javax.crypto.spec.SecretKeySpec;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLEngine;
import javax.net.ssl.SSLParameters;

/**
 * SBR3 lane `latent`: HotSpot-diffed probe for the latent carrier items.
 * Run under HotSpot, `--jdk-only` and `--compatible`; the three outputs must be identical.
 * Sections: Provider, SSLParameters ALPN, Thread.Builder, Cipher, ForkJoinPool.
 */
public class Sbr3LatentProbe {
    static void line(String s) { System.out.println(s); }

    static void guarded(String name, Callable<Object> c) {
        try {
            line(name + " = " + c.call());
        } catch (Throwable t) {
            line(name + " threw " + t.getClass().getName() + ": " + t.getMessage());
        }
    }

    public static void main(String[] args) throws Exception {
        provider();
        alpn();
        threadBuilder();
        cipher();
        forkJoin();
    }

    static void provider() throws Exception {
        line("== provider");
        Provider sun = Security.getProvider("SUN");
        guarded("sun.name", () -> sun.getName());
        guarded("sun.class", () -> sun.getClass().getName());
        guarded("sun.versionStr", () -> sun.getVersionStr());
        guarded("sun.info", () -> sun.getInfo());
        guarded("sun.toString", () -> sun.toString());
        guarded("sun.sameInstance", () -> sun == Security.getProvider("SUN"));
        guarded("sun.equals", () -> sun.equals(Security.getProvider("SUN")));
        guarded("sun.hashStable", () -> sun.hashCode() == sun.hashCode());
        guarded("sun.isEmpty", () -> sun.isEmpty());
        guarded("sun.hasServices", () -> sun.getServices().size() > 10);
        guarded("sun.sha256", () -> sun.getService("MessageDigest", "SHA-256").getClassName());
        List<String> names = new ArrayList<>();
        for (Provider p : Security.getProviders()) names.add(p.getName());
        guarded("providers", () -> names);
        // a user-constructed provider runs the real constructor
        Provider mine = new Provider("SbrLatent", "1.5", "probe provider") {};
        guarded("mine.name", () -> mine.getName());
        guarded("mine.version", () -> mine.getVersionStr());
        guarded("mine.info", () -> mine.getInfo());
        guarded("mine.empty", () -> mine.isEmpty());
        mine.put("MessageDigest.SBRX", "sun.security.provider.SHA2$SHA256");
        guarded("mine.get", () -> mine.get("MessageDigest.SBRX"));
        guarded("mine.service", () -> mine.getService("MessageDigest", "SBRX") != null);
        guarded("mine.notEqualsSun", () -> mine.equals(sun));
        guarded("sslctx.provider", () -> SSLContext.getInstance("TLS").getProvider().getName());
        guarded("sslctx.providerEquals", () -> SSLContext.getInstance("TLS").getProvider()
                .equals(SSLContext.getDefault().getProvider()));
    }

    static void alpn() throws Exception {
        line("== alpn");
        SSLParameters p = new SSLParameters();
        guarded("fresh.alpn", () -> Arrays.toString(p.getApplicationProtocols()));
        guarded("fresh.alpnLen", () -> p.getApplicationProtocols().length);
        guarded("fresh.constraints", () -> p.getAlgorithmConstraints());
        guarded("fresh.retrans", () -> p.getEnableRetransmissions());
        p.setApplicationProtocols(new String[] {"h2", "http/1.1"});
        guarded("set.alpn", () -> Arrays.toString(p.getApplicationProtocols()));
        guarded("set.constraints", () -> p.getAlgorithmConstraints());
        guarded("set.nullArg", () -> { p.setApplicationProtocols(null); return "no throw"; });
        guarded("set.emptyElem", () -> { p.setApplicationProtocols(new String[] {""}); return "no throw"; });
        guarded("after.alpn", () -> Arrays.toString(p.getApplicationProtocols()));
        SSLEngine e = SSLContext.getDefault().createSSLEngine();
        guarded("engine.fresh.alpn", () -> Arrays.toString(e.getSSLParameters().getApplicationProtocols()));
        SSLParameters q = e.getSSLParameters();
        q.setApplicationProtocols(new String[] {"h2"});
        e.setSSLParameters(q);
        guarded("engine.set.alpn", () -> Arrays.toString(e.getSSLParameters().getApplicationProtocols()));
        guarded("engine.set.constraints", () -> e.getSSLParameters().getAlgorithmConstraints() != null);
        guarded("engine.set.protocols", () -> e.getSSLParameters().getProtocols() != null);
    }

    static void threadBuilder() throws Exception {
        line("== thread builder");
        guarded("startVirtualThread", () -> {
            Thread t = Thread.startVirtualThread(() -> {});
            t.join();
            return t.isVirtual() + "/" + t.isDaemon() + "/[" + t.getName() + "]/" + t.getPriority()
                    + "/" + t.getState();
        });
        guarded("ofVirtual.name", () -> {
            Thread t = Thread.ofVirtual().name("v-probe").unstarted(() -> {});
            return t.isVirtual() + "/" + t.getName() + "/" + t.getState();
        });
        guarded("ofVirtual.counter", () -> {
            Thread.Builder b = Thread.ofVirtual().name("vc-", 5);
            return b.unstarted(() -> {}).getName() + "," + b.unstarted(() -> {}).getName();
        });
        guarded("ofPlatform.unstarted", () -> {
            Thread t = Thread.ofPlatform().name("p-probe").daemon(true).priority(7).unstarted(() -> {});
            return t.isVirtual() + "/" + t.getName() + "/" + t.isDaemon() + "/" + t.getPriority()
                    + "/" + t.getState();
        });
        guarded("ofPlatform.start", () -> {
            final String[] seen = new String[1];
            Thread t = Thread.ofPlatform().name("p-run").start(() -> seen[0] = Thread.currentThread().getName());
            t.join();
            return seen[0] + "/" + t.getState();
        });
        guarded("ofVirtual.start", () -> {
            final boolean[] seen = new boolean[1];
            Thread t = Thread.ofVirtual().start(() -> seen[0] = Thread.currentThread().isVirtual());
            t.join();
            return seen[0] + "/" + t.getState();
        });
        guarded("factory", () -> {
            ThreadFactory f = Thread.ofPlatform().name("fac-", 0).daemon(true).factory();
            Thread a = f.newThread(() -> {});
            Thread b = f.newThread(() -> {});
            return a.getName() + "," + b.getName() + "," + a.isDaemon() + "," + a.isVirtual();
        });
        guarded("virtualFactory", () -> {
            ThreadFactory f = Thread.ofVirtual().name("vf-", 1).factory();
            Thread a = f.newThread(() -> {});
            return a.getName() + "," + a.isVirtual() + "," + a.getState();
        });
        guarded("executor", () -> {
            try (ExecutorService ex = Executors.newVirtualThreadPerTaskExecutor()) {
                return ex.submit(() -> Thread.currentThread().isVirtual()).get();
            }
        });
    }

    static void cipher() throws Exception {
        line("== cipher");
        byte[] key = new byte[16];
        for (int i = 0; i < 16; i++) key[i] = (byte) i;
        SecretKeySpec ks = new SecretKeySpec(key, "AES");
        guarded("cbc", () -> {
            Cipher c = Cipher.getInstance("AES/CBC/PKCS5Padding");
            c.init(Cipher.ENCRYPT_MODE, ks, new javax.crypto.spec.IvParameterSpec(new byte[16]));
            byte[] ct = c.doFinal("hello cipher probe".getBytes());
            Cipher d = Cipher.getInstance("AES/CBC/PKCS5Padding");
            d.init(Cipher.DECRYPT_MODE, ks, new javax.crypto.spec.IvParameterSpec(new byte[16]));
            return HexFormat.of().formatHex(ct) + "/" + new String(d.doFinal(ct)) + "/" + c.getAlgorithm()
                    + "/" + c.getBlockSize() + "/" + c.getOutputSize(5) + "/" + c.getProvider().getName()
                    + "/" + c.getProvider().getClass().getName();
        });
        guarded("gcm", () -> {
            Cipher c = Cipher.getInstance("AES/GCM/NoPadding");
            c.init(Cipher.ENCRYPT_MODE, ks, new GCMParameterSpec(128, new byte[12]));
            c.updateAAD(new byte[] {1, 2, 3});
            byte[] ct = c.doFinal("gcm probe".getBytes());
            Cipher d = Cipher.getInstance("AES/GCM/NoPadding");
            d.init(Cipher.DECRYPT_MODE, ks, new GCMParameterSpec(128, new byte[12]));
            d.updateAAD(new byte[] {1, 2, 3});
            return HexFormat.of().formatHex(ct) + "/" + new String(d.doFinal(ct)) + "/" + c.getIV().length;
        });
        guarded("class", () -> Cipher.getInstance("AES").getClass().getName());
        guarded("maxKey", () -> Cipher.getMaxAllowedKeyLength("AES"));
        guarded("noSuchAlg", () -> Cipher.getInstance("NoSuchAlg/CBC/PKCS5Padding"));
        guarded("noSuchPad", () -> Cipher.getInstance("AES/CBC/NoSuchPadding"));
        guarded("uninit", () -> Cipher.getInstance("AES").doFinal(new byte[16]));
        guarded("badKey", () -> {
            Cipher c = Cipher.getInstance("AES");
            c.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(new byte[5], "AES"));
            return "no throw";
        });
        guarded("toString", () -> Cipher.getInstance("AES").toString().startsWith("Cipher."));
    }

    static void forkJoin() throws Exception {
        line("== forkjoin");
        ForkJoinPool common = ForkJoinPool.commonPool();
        guarded("common.class", () -> common.getClass().getName());
        guarded("common.same", () -> common == ForkJoinPool.commonPool());
        guarded("common.parallelismPositive", () -> common.getParallelism() > 0);
        guarded("common.parallelismEq", () -> common.getParallelism() == ForkJoinPool.getCommonPoolParallelism());
        guarded("common.factory", () -> common.getFactory().getClass().getName());
        guarded("common.submitCallable", () -> common.submit(() -> 6 * 7).get());
        guarded("common.invoke", () -> common.invoke(new RecursiveTask<Integer>() {
            protected Integer compute() { return 11; }
        }));
        guarded("common.execute", () -> {
            CountDownLatch l = new CountDownLatch(1);
            common.execute(l::countDown);
            return l.await(10, TimeUnit.SECONDS);
        });
        guarded("common.workerName", () -> {
            String n = common.submit(() -> Thread.currentThread().getName()).get();
            return n.startsWith("ForkJoinPool.commonPool-worker-");
        });
        guarded("common.invokeAll", () -> {
            List<Callable<Integer>> l = List.of(() -> 1, () -> 2, () -> 3);
            int s = 0;
            for (Future<Integer> f : common.invokeAll(l)) s += f.get();
            return s;
        });
        guarded("common.awaitQuiescence", () -> common.awaitQuiescence(5, TimeUnit.SECONDS));
        guarded("common.isShutdown", () -> common.isShutdown());
        guarded("common.shutdownNoop", () -> { common.shutdown(); return common.isShutdown(); });
        ForkJoinPool mine = new ForkJoinPool(3);
        guarded("mine.parallelism", () -> mine.getParallelism());
        guarded("mine.invoke", () -> mine.invoke(new RecursiveTask<Integer>() {
            protected Integer compute() { return 5; }
        }));
        guarded("mine.submit", () -> mine.submit(() -> "ok").get());
        guarded("mine.parallelStream", () -> mine.submit(() ->
                java.util.stream.IntStream.range(0, 1000).parallel().sum()).get());
        mine.shutdown();
        guarded("mine.terminated", () -> mine.awaitTermination(10, TimeUnit.SECONDS));
        guarded("mine.isShutdown", () -> mine.isShutdown());
        guarded("parallelStream", () -> java.util.stream.IntStream.range(0, 10000).parallel().sum());
        guarded("completableFuture", () -> CompletableFuture.supplyAsync(() -> 9).thenApply(x -> x + 1).get());
    }
}
