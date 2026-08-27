import java.lang.reflect.*;
import java.io.*;
import java.util.*;
import java.util.concurrent.*;

/** Method, Constructor, LinkedHashMap, PriorityQueue, ByteArrayOutputStream,
 *  CompletableFuture, ThreadPoolExecutor and ConcurrentHashMap$KeySetView ---
 *  ~130 more rows off the bridge-kind retirement surface.
 *
 *  Concurrency is made DETERMINISTIC, not sampled: every executor is
 *  single-threaded, every future is joined before it is read, and nothing
 *  prints a thread name, a timing or a queue order that depends on scheduling.
 *  A probe that measures a race is worse than no probe. */
public class ConcurrentReflectSweep {
    static String esc(String s) {
        StringBuilder b = new StringBuilder(s.length());
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x20 || c > 0x7e) b.append(String.format("\\u%04x", (int) c));
            else b.append(c);
        }
        return b.toString();
    }
    static void p(String tag, Object v) {
        System.out.println(esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }
    static void t(String tag, ThrowingRun r) {
        try { r.run(); p(tag, "no-throw"); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }
    interface ThrowingRun { void run() throws Exception; }

    public static class Fix {
        public int pub; private String priv = "p";
        public Fix() {} public Fix(int a) { pub = a; } private Fix(String s) { priv = s; }
        public int add(int a, int b) { return a + b; }
        public static String stat(String s) { return "S" + s; }
        protected void prot() {}
        public int varargs(int... xs) { return xs.length; }
        public <T> T generic(T t) { return t; }
        public void thrower() throws IOException { throw new IOException("boom"); }
        public String toString() { return "Fix(" + pub + "," + priv + ")"; }
    }

    static void methodsAndCtors() throws Exception {
        Method add = Fix.class.getMethod("add", int.class, int.class);
        p("m getName", add.getName());
        p("m getReturnType", add.getReturnType().getName());
        p("m getParameterTypes", Arrays.toString(add.getParameterTypes()));
        p("m getParameterCount", add.getParameterCount());
        p("m getModifiers", Modifier.toString(add.getModifiers()));
        p("m getDeclaringClass", add.getDeclaringClass().getSimpleName());
        p("m isVarArgs", add.isVarArgs());
        p("m isSynthetic", add.isSynthetic());
        p("m isBridge", add.isBridge());
        p("m getExceptionTypes", Arrays.toString(add.getExceptionTypes()));
        p("m toString", add.toString());
        p("m toGenericString", add.toGenericString());
        p("m invoke", add.invoke(new Fix(), 2, 3));
        p("m equals same", add.equals(Fix.class.getMethod("add", int.class, int.class)));
        p("m hashCode equal", add.hashCode() == Fix.class.getMethod("add", int.class, int.class).hashCode());
        Method st = Fix.class.getMethod("stat", String.class);
        p("m static invoke null recv", st.invoke(null, "x"));
        Method va = Fix.class.getMethod("varargs", int[].class);
        p("m isVarArgs true", va.isVarArgs());
        p("m varargs invoke", va.invoke(new Fix(), (Object) new int[]{1, 2, 3}));
        Method gen = Fix.class.getMethod("generic", Object.class);
        p("m generic toGenericString", gen.toGenericString());
        Method thr = Fix.class.getMethod("thrower");
        try { thr.invoke(new Fix()); p("m thrower", "no-throw"); }
        catch (InvocationTargetException e) {
            p("m thrower wraps", e.getClass().getSimpleName() + "->" + e.getCause().getClass().getName()); }
        t("m invoke wrong recv", () -> add.invoke("str", 1, 2));
        t("m invoke null recv on instance", () -> add.invoke(null, 1, 2));
        t("m getMethod private", () -> Fix.class.getMethod("prot"));
        Method prot = Fix.class.getDeclaredMethod("prot");
        p("m getDeclaredMethod protected", prot.getName());
        t("m invoke inaccessible", () -> Fix.class.getDeclaredMethod("prot").invoke(new Fix()));

        Constructor<Fix> c0 = Fix.class.getConstructor();
        Constructor<Fix> c1 = Fix.class.getConstructor(int.class);
        p("c newInstance()", c0.newInstance());
        p("c newInstance(int)", c1.newInstance(9));
        p("c getParameterCount", c1.getParameterCount());
        p("c getModifiers", Modifier.toString(c1.getModifiers()));
        p("c toString", c1.toString());
        p("c declaredCount", Fix.class.getDeclaredConstructors().length);
        t("c getConstructor(String) is private", () -> Fix.class.getConstructor(String.class));
        Constructor<Fix> cp = Fix.class.getDeclaredConstructor(String.class);
        t("c private newInstance without setAccessible", () -> cp.newInstance("v"));
        cp.setAccessible(true);
        p("c private newInstance after setAccessible", cp.newInstance("v"));
        t("c newInstance wrong arity", () -> c1.newInstance());
        t("c newInstance wrong type", () -> c1.newInstance("s"));
    }

    static void maps() {
        LinkedHashMap<String, Integer> lm = new LinkedHashMap<>();
        for (String k : new String[]{"c", "a", "b"}) lm.put(k, k.charAt(0) - 'a');
        p("lhm preserves insertion order", lm);
        p("lhm keySet order", lm.keySet());
        p("lhm values order", lm.values());
        lm.put("a", 99);
        p("lhm re-put keeps position", lm);
        p("lhm remove", lm.remove("a") + " -> " + lm);
        LinkedHashMap<String, Integer> acc = new LinkedHashMap<>(16, 0.75f, true);
        acc.put("x", 1); acc.put("y", 2); acc.put("z", 3);
        acc.get("x");
        p("lhm access-order after get", acc.keySet());
        p("lhm entrySet order", lm.entrySet());
        t("lhm keySet add", () -> lm.keySet().add("q"));

        PriorityQueue<Integer> pq = new PriorityQueue<>(Arrays.asList(5, 1, 4, 2));
        p("pq peek is min", pq.peek());
        p("pq size", pq.size());
        List<Integer> drained = new ArrayList<>();
        while (!pq.isEmpty()) drained.add(pq.poll());
        p("pq drains sorted", drained);
        PriorityQueue<String> pr = new PriorityQueue<>(Comparator.reverseOrder());
        pr.addAll(Arrays.asList("a", "c", "b"));
        p("pq comparator peek", pr.peek());
        p("pq poll order", pr.poll() + pr.poll() + pr.poll());
        p("pq poll on empty", pr.poll());
        t("pq element on empty", () -> new PriorityQueue<String>().element());
        t("pq add null", () -> new PriorityQueue<String>().add(null));

        ConcurrentHashMap<String, Integer> chm = new ConcurrentHashMap<>();
        chm.put("a", 1);
        Set<String> view = chm.keySet();
        p("chm keySet view", new TreeSet<>(view));
        t("chm keySet() add is UOE", () -> view.add("z"));
        ConcurrentHashMap.KeySetView<String, Boolean> ks = ConcurrentHashMap.newKeySet();
        p("chm newKeySet add is LEGAL", ks.add("k"));
        p("chm newKeySet contents", new TreeSet<>(ks));
        p("chm newKeySet dup add", ks.add("k"));
        ConcurrentHashMap.KeySetView<String, Integer> kv = chm.keySet(7);
        p("chm keySet(default) add", kv.add("n"));
        p("chm map saw the default", chm.get("n"));
    }

    static void streamsAndFutures() throws Exception {
        ByteArrayOutputStream bo = new ByteArrayOutputStream();
        bo.write('a'); bo.write(new byte[]{'b', 'c'}); bo.write(new byte[]{'x', 'y', 'z'}, 1, 2);
        p("baos size", bo.size());
        p("baos toString", bo.toString("UTF-8"));
        p("baos toByteArray", Arrays.toString(bo.toByteArray()));
        ByteArrayOutputStream bo2 = new ByteArrayOutputStream();
        bo.writeTo(bo2);
        p("baos writeTo", bo2.toString("UTF-8"));
        bo.reset();
        p("baos after reset", bo.size());
        t("baos write oob", () -> bo.write(new byte[2], 0, 9));

        CompletableFuture<String> cf = CompletableFuture.completedFuture("v");
        p("cf isDone/get", cf.isDone() + "/" + cf.get());
        p("cf thenApply", cf.thenApply(s -> s + "!").get());
        p("cf thenCompose", cf.thenCompose(s -> CompletableFuture.completedFuture(s + "?")).get());
        p("cf thenCombine", cf.thenCombine(CompletableFuture.completedFuture("w"), (a, b) -> a + b).get());
        CompletableFuture<String> man = new CompletableFuture<>();
        p("cf incomplete isDone", man.isDone());
        man.complete("done");
        p("cf after complete", man.get() + "/" + man.isDone());
        p("cf second complete returns false", man.complete("again"));
        CompletableFuture<String> failed = new CompletableFuture<>();
        failed.completeExceptionally(new IllegalStateException("bad"));
        p("cf isCompletedExceptionally", failed.isCompletedExceptionally());
        t("cf get on failed", () -> failed.get());
        p("cf exceptionally", failed.exceptionally(e -> "recovered").get());
        p("cf getNow default", new CompletableFuture<String>().getNow("dflt"));
        CompletableFuture<String> cancelled = new CompletableFuture<>();
        p("cf cancel", cancelled.cancel(true) + "/" + cancelled.isCancelled());
        t("cf get on cancelled", () -> cancelled.get());

        ExecutorService ex = Executors.newSingleThreadExecutor();
        try {
            Future<Integer> f = ex.submit(() -> 21 * 2);
            p("executor submit get", f.get());
            p("executor future isDone", f.isDone());
            List<Future<Integer>> all = ex.invokeAll(Arrays.asList(() -> 1, () -> 2, () -> 3));
            List<Integer> vals = new ArrayList<>();
            for (Future<Integer> q : all) vals.add(q.get());
            p("executor invokeAll in order", vals);
            Future<?> r = ex.submit(() -> { throw new IllegalStateException("task"); });
            try { r.get(); p("executor failing task", "no-throw"); }
            catch (ExecutionException e) { p("executor wraps", e.getCause().getClass().getName()); }
            p("executor isShutdown before", ex.isShutdown());
        } finally {
            ex.shutdown();
            p("executor awaitTermination", ex.awaitTermination(10, TimeUnit.SECONDS));
            p("executor isShutdown/isTerminated", ex.isShutdown() + "/" + ex.isTerminated());
            t("executor submit after shutdown", () -> ex.submit(() -> 1));
        }
    }

    public static void main(String[] a) throws Exception {
        methodsAndCtors();
        maps();
        streamsAndFutures();
        System.out.println("DONE ConcurrentReflectSweep");
    }
}
