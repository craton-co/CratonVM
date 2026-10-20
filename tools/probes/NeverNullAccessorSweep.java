// The half-shim hunt: JDK accessors SPECIFIED never to return null, on the
// classes CratonVM allocates a synthetic carrier for.
//
// Generalised from the defect that killed all of Groovy (18 corpus classes):
// `ModuleFinder.ofSystem().findAll()` built a `ModuleDescriptor` with only its
// `name` field set, so `packages()`/`exports()`/`opens()` answered NULL. A
// registered native hid that in compatible mode; under `--jdk-only` the native
// steps aside, the real JDK bytecode runs `return packages;`, and the null
// reaches the caller -- two frames later, as somebody else's
// NoClassDefFoundError.
//
// So every row here asks one question of a fabricated carrier: does an accessor
// the JDK guarantees is non-null actually answer non-null? A row that reads
// `null` is a defect; a row that differs BETWEEN MODES is that defect in its
// most dangerous form, because the default build cannot see it.
//
// Hygiene: prints `nonnull`/`NULL`/an exception class -- never the value, since
// most of these are identity- or environment-dependent.
import java.net.InetAddress;
import java.nio.charset.Charset;
import java.util.ArrayList;
import java.util.List;
import java.util.Optional;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.ThreadPoolExecutor;
import java.util.concurrent.TimeUnit;

public class NeverNullAccessorSweep {
    interface Body { Object run() throws Throwable; }

    /** Prints `nonnull` / `NULL` rather than the value. */
    static void nn(String tag, Body b) {
        String v;
        try {
            Object o = b.run();
            v = (o == null) ? "NULL" : "nonnull";
        } catch (Throwable e) {
            v = "throws " + e.getClass().getName();
        }
        System.out.println(tag + " = " + v);
    }

    /** For rows whose VALUE is specified and stable. */
    static void t(String tag, Body b) {
        String v;
        try { v = String.valueOf(b.run()); }
        catch (Throwable e) { v = "throws " + e.getClass().getName(); }
        System.out.println(tag + " = " + v);
    }

    public static void main(String[] a) {
        modules();
        optionals();
        futures();
        threadsAndPools();
        charsetsAndNet();
        collectionViews();
        System.out.println("DONE");
    }

    // ---- the family the Groovy defect was in -------------------------------

    static void modules() {
        nn("mod.base.descriptor", () -> Object.class.getModule().getDescriptor());
        nn("mod.base.packages", () -> Object.class.getModule().getPackages());
        nn("mod.base.layer", () -> Object.class.getModule().getLayer());
        nn("mod.desc.packages", () -> Object.class.getModule().getDescriptor().packages());
        nn("mod.desc.exports", () -> Object.class.getModule().getDescriptor().exports());
        nn("mod.desc.opens", () -> Object.class.getModule().getDescriptor().opens());
        nn("mod.desc.requires", () -> Object.class.getModule().getDescriptor().requires());
        nn("mod.desc.provides", () -> Object.class.getModule().getDescriptor().provides());
        nn("mod.desc.uses", () -> Object.class.getModule().getDescriptor().uses());
        nn("mod.desc.modifiers", () -> Object.class.getModule().getDescriptor().modifiers());
        nn("layer.boot.modules", () -> ModuleLayer.boot().modules());
        nn("layer.boot.parents", () -> ModuleLayer.boot().parents());
        nn("layer.boot.cfg", () -> ModuleLayer.boot().configuration());
        nn("layer.cfg.modules", () -> ModuleLayer.boot().configuration().modules());
        // The finder path the Groovy defect was ON.
        nn("finder.findAll", () -> java.lang.module.ModuleFinder.ofSystem().findAll());
        nn("finder.find.desc.packages", () -> java.lang.module.ModuleFinder.ofSystem()
            .find("java.base").orElseThrow().descriptor().packages());
        nn("finder.find.desc.exports", () -> java.lang.module.ModuleFinder.ofSystem()
            .find("java.base").orElseThrow().descriptor().exports());
        nn("finder.find.desc.opens", () -> java.lang.module.ModuleFinder.ofSystem()
            .find("java.base").orElseThrow().descriptor().opens());
        nn("finder.find.desc.requires", () -> java.lang.module.ModuleFinder.ofSystem()
            .find("java.base").orElseThrow().descriptor().requires());
        // Every descriptor the finder hands back must answer, not just java.base.
        t("finder.allDescriptorsAnswer", () -> {
            int bad = 0;
            for (var r : java.lang.module.ModuleFinder.ofSystem().findAll()) {
                var d = r.descriptor();
                if (d == null || d.packages() == null || d.exports() == null
                        || d.opens() == null || d.requires() == null) bad++;
            }
            return "badDescriptors=" + bad;
        });
    }

    // ---- Optional: a one-field carrier ------------------------------------

    static void optionals() {
        nn("opt.empty", () -> Optional.empty());
        t("opt.empty.isPresent", () -> Optional.empty().isPresent());
        t("opt.of.get", () -> Optional.of("x").get());
        t("opt.ofNullable.null", () -> Optional.ofNullable(null).isPresent());
        t("opt.map", () -> Optional.of("x").map(s -> s + "y").orElse("Z"));
        t("opt.emptyGet", () -> Optional.empty().get());
        t("opt.stream.count", () -> Optional.of("x").stream().count());
    }

    // ---- CompletableFuture: a two/three-field carrier ---------------------

    static void futures() {
        t("cf.completed.get", () -> CompletableFuture.completedFuture("v").get());
        t("cf.completed.isDone", () -> CompletableFuture.completedFuture("v").isDone());
        t("cf.thenApply", () -> CompletableFuture.completedFuture(2)
            .thenApply(x -> x * 3).get());
        t("cf.failed.isCompletedExceptionally", () -> {
            CompletableFuture<String> f = new CompletableFuture<>();
            f.completeExceptionally(new IllegalStateException("x"));
            return f.isCompletedExceptionally();
        });
        t("cf.allOf", () -> {
            CompletableFuture.allOf(CompletableFuture.completedFuture(1),
                                    CompletableFuture.completedFuture(2)).get();
            return "joined";
        });
        nn("cf.newIncomplete", () -> new CompletableFuture<String>());
    }

    // ---- Thread / pools ----------------------------------------------------

    static void threadsAndPools() {
        nn("thread.current", () -> Thread.currentThread());
        nn("thread.current.name", () -> Thread.currentThread().getName());
        nn("thread.current.group", () -> Thread.currentThread().getThreadGroup());
        nn("thread.current.state", () -> Thread.currentThread().getState());
        nn("thread.current.ccl", () -> Thread.currentThread().getContextClassLoader());
        t("thread.current.isAlive", () -> Thread.currentThread().isAlive());
        t("pool.submitGet", () -> {
            ExecutorService es = Executors.newFixedThreadPool(2);
            try { return es.submit(() -> 41 + 1).get(); } finally { es.shutdown(); }
        });
        t("pool.tpeQueueNonNull", () -> {
            ThreadPoolExecutor tpe = (ThreadPoolExecutor) Executors.newFixedThreadPool(1);
            try { return tpe.getQueue() != null; } finally { tpe.shutdown(); }
        });
        t("pool.awaitTermination", () -> {
            ExecutorService es = Executors.newFixedThreadPool(1);
            es.shutdown();
            return es.awaitTermination(5, TimeUnit.SECONDS);
        });
    }

    // ---- Charset / InetAddress --------------------------------------------

    static void charsetsAndNet() {
        nn("charset.default", () -> Charset.defaultCharset());
        nn("charset.forName", () -> Charset.forName("UTF-8"));
        nn("charset.aliases", () -> Charset.forName("UTF-8").aliases());
        t("charset.name", () -> Charset.forName("UTF-8").name());
        t("charset.canEncode", () -> Charset.forName("UTF-8").canEncode());
        nn("inet.loopback", () -> InetAddress.getLoopbackAddress());
        nn("inet.loopback.address", () -> InetAddress.getLoopbackAddress().getAddress());
        t("inet.loopback.hostAddress", () -> InetAddress.getLoopbackAddress().getHostAddress());
        t("inet.byAddressIsLoopback", () -> InetAddress.getByAddress(
            new byte[] { 127, 0, 0, 1 }).isLoopbackAddress());
    }

    // ---- collection views / spliterators ----------------------------------

    static void collectionViews() {
        nn("list.spliterator", () -> new ArrayList<>(List.of(1, 2)).spliterator());
        t("list.spliterator.size", () -> new ArrayList<>(List.of(1, 2)).spliterator()
            .estimateSize());
        nn("list.stream", () -> List.of(1, 2).stream());
        t("list.stream.count", () -> List.of(1, 2, 3).stream().count());
        nn("map.entrySet", () -> java.util.Map.of("a", 1).entrySet());
        nn("map.keySet", () -> java.util.Map.of("a", 1).keySet());
        nn("map.values", () -> java.util.Map.of("a", 1).values());
        t("map.entry.getKey", () -> java.util.Map.of("a", 1).entrySet()
            .iterator().next().getKey());
        nn("set.iterator", () -> java.util.Set.of("a").iterator());
        t("enumset.size", () -> java.util.EnumSet.allOf(TimeUnit.class).size());
        nn("enumset.iterator", () -> java.util.EnumSet.allOf(TimeUnit.class).iterator());
    }
}
