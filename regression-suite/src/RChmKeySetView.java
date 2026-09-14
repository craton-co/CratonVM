import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.HashSet;
import java.util.Iterator;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.ThreadFactory;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * Regression: ConcurrentHashMap.newKeySet() returned a plain HashSet.
 *
 * Two defects in one. The returned object was a `java.util.HashSet`, so
 * `(ConcurrentHashMap.KeySetView<K, ?>) set` threw ClassCastException and
 * `getMappedValue()` was a NoSuchMethodError. And a HashSet's add/remove/size
 * run the HashMap natives, which take no lock — so the JDK's canonical
 * "give me a concurrent set" was answered with a structure that corrupts under
 * concurrent mutation. The size it kept could even go negative, which a real
 * `ConcurrentHashMap.size()` cannot do (it clamps `sumCount() < 0` to 0).
 *
 * That is an unbounded hang, not a cosmetic wrong answer:
 * `ThreadPerTaskExecutor.tryTerminate()` only advances SHUTDOWN → TERMINATED
 * when its `newKeySet()` of live threads reports empty, and
 * `ExecutorService.close()` is `shutdown()` then an UNBOUNDED
 * `awaitTermination`. So `Executors.newVirtualThreadPerTaskExecutor()` in a
 * try-with-resources hung forever on the closing brace.
 *
 * `churn` is the load-bearing case: every add is followed by its own remove, so
 * the correct final size is 0 on any interleaving. A single-threaded size check
 * passed throughout the bug's life and would not have caught it.
 */
public class RChmKeySetView {
    static int checks = 0;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    /** Balanced churn: `size` must land on 0, and iteration must agree with it. */
    static void churn() throws Exception {
        final int threads = 8;
        final int iters = 500;
        for (int round = 0; round < 3; round++) {
            final Set<Object> s = ConcurrentHashMap.newKeySet();
            Thread[] th = new Thread[threads];
            for (int t = 0; t < threads; t++) {
                th[t] = new Thread(() -> {
                    for (int i = 0; i < iters; i++) {
                        Object o = new Object();
                        s.add(o);
                        s.remove(o);
                    }
                });
                th[t].start();
            }
            for (Thread t : th) {
                t.join();
            }
            int iterated = 0;
            for (Object o : s) {
                iterated++;
            }
            check(s.size() == 0, "churn round " + round + ": size=" + s.size() + " (expected 0)");
            check(s.isEmpty(), "churn round " + round + ": isEmpty=false at size " + s.size());
            check(iterated == 0, "churn round " + round + ": iterated " + iterated + " elements");
        }
    }

    /** Distinct concurrent adds: nothing lost, nothing invented. */
    static void concurrentAdds() throws Exception {
        final int threads = 8;
        final int iters = 500;
        final Set<String> s = ConcurrentHashMap.newKeySet();
        Thread[] th = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            final int id = t;
            th[t] = new Thread(() -> {
                for (int i = 0; i < iters; i++) {
                    s.add(id + ":" + i);
                }
            });
            th[t].start();
        }
        for (Thread t : th) {
            t.join();
        }
        check(s.size() == threads * iters, "concurrent adds: size=" + s.size());
        int iterated = 0;
        for (String x : s) {
            iterated++;
        }
        check(iterated == threads * iters, "concurrent adds: iterated " + iterated);
        for (int t = 0; t < threads; t++) {
            for (int i = 0; i < iters; i++) {
                check(s.contains(t + ":" + i), "concurrent adds: lost " + t + ":" + i);
            }
        }
    }

    /** The class the JDK promises, not a stand-in. */
    static void identity() {
        Object o = ConcurrentHashMap.newKeySet();
        check(o instanceof Set, "newKeySet() is not a Set: " + o.getClass().getName());
        ConcurrentHashMap.KeySetView<String, Boolean> view =
                (ConcurrentHashMap.KeySetView<String, Boolean>) o;
        check(Boolean.TRUE.equals(view.getMappedValue()),
                "getMappedValue()=" + view.getMappedValue());
        // `!= null` alone is satisfied by a fresh map minted per call. The
        // three lines below already prove the add reaches SOME map the getter
        // returns; what they cannot see is that it is ONE map, held by the
        // view, and empty before the add. Both halves are read from the same
        // object at run time -- no host constant.
        ConcurrentHashMap<String, Boolean> backing = view.getMap();
        check(backing != null && backing.isEmpty() && backing == view.getMap(),
                "getMap() must be one stable, initially empty backing map, got " + backing);
        view.add("a");
        check(view.getMap().containsKey("a"), "add() did not reach the backing map");
        check(Boolean.TRUE.equals(view.getMap().get("a")),
                "add() stored " + view.getMap().get("a") + ", not the mapped value");
    }

    /** The whole Set surface, so no method is left walking an empty `table`. */
    static void surface() {
        Set<String> s = ConcurrentHashMap.newKeySet();
        check(s.size() == 0 && s.isEmpty(), "fresh view is not empty");
        check(s.add("a"), "add of a new element returned false");
        check(!s.add("a"), "add of a duplicate returned true");
        check(s.contains("a") && s.size() == 1, "add did not take");
        check(s.remove("a") && !s.remove("a"), "remove result is wrong");

        s.addAll(Arrays.asList("a", "b", "c"));
        check(s.size() == 3, "addAll size=" + s.size());
        check(s.containsAll(Arrays.asList("a", "c")), "containsAll");

        List<String> seen = new ArrayList<>();
        for (String x : s) {
            seen.add(x);
        }
        Collections.sort(seen);
        check(seen.equals(Arrays.asList("a", "b", "c")), "iterator yielded " + seen);
        check(s.toArray().length == 3, "toArray length");
        check(s.toArray(new String[0]).length == 3, "toArray(T[]) length");

        Set<String> ref = new HashSet<>(Arrays.asList("a", "b", "c"));
        check(s.hashCode() == ref.hashCode(), "hashCode " + s.hashCode() + " vs " + ref.hashCode());
        check(s.equals(ref) && ref.equals(s), "equals is not symmetric with HashSet");
        check(new HashSet<>(s).equals(ref), "copy constructor lost elements");

        AtomicInteger n = new AtomicInteger();
        s.forEach(x -> n.incrementAndGet());
        check(n.get() == 3, "forEach visited " + n.get());
        check(s.stream().count() == 3, "stream().count()=" + s.stream().count());
        check(s.spliterator().estimateSize() == 3, "spliterator estimateSize");

        Iterator<String> it = s.iterator();
        while (it.hasNext()) {
            if (it.next().equals("b")) {
                it.remove();
            }
        }
        check(s.size() == 2 && !s.contains("b"), "Iterator.remove did not delete");
        check(s.removeIf(x -> x.equals("c")) && s.size() == 1, "removeIf");
        s.addAll(Arrays.asList("b", "c"));
        check(s.removeAll(Arrays.asList("a", "c")) && s.size() == 1, "removeAll");
        s.addAll(Arrays.asList("a", "c", "d"));
        check(s.retainAll(Arrays.asList("a", "b")) && s.size() == 2, "retainAll");
        s.clear();
        check(s.isEmpty() && s.size() == 0, "clear");

        Set<String> sized = ConcurrentHashMap.newKeySet(64);
        sized.add("x");
        check(sized.size() == 1 && sized.contains("x"), "newKeySet(int)");

        boolean npe = false;
        try {
            s.add(null);
        } catch (NullPointerException e) {
            npe = true;
        }
        check(npe, "add(null) did not throw NullPointerException");
    }

    /** `keySet(V)` is a LIVE, add-able view of the map it came from. */
    static void mappedKeySetView() {
        ConcurrentHashMap<String, Integer> m = new ConcurrentHashMap<>();
        m.put("p", 1);
        ConcurrentHashMap.KeySetView<String, Integer> view = m.keySet(9);
        check(view.size() == 1 && view.contains("p"), "keySet(v) does not see existing keys");
        view.add("r");
        check(Integer.valueOf(9).equals(m.get("r")), "keySet(v).add did not write through");
        view.remove("p");
        check(!m.containsKey("p"), "keySet(v).remove did not write through");
        m.put("q", 2);
        check(view.contains("q"), "keySet(v) is a detached snapshot, not a live view");
    }

    /**
     * The blast radius: closing a thread-per-task executor.
     *
     * Reached REFLECTIVELY, and closed through an `AutoCloseable` cast rather
     * than try-with-resources, because this tree is also compiled at
     * `--release 17` (regression-suite/run.sh, RELEASES) and all three of
     * `Executors.newVirtualThreadPerTaskExecutor`, `newThreadPerTaskExecutor`
     * and `ExecutorService extends AutoCloseable` are Java 19/21 API. A cast
     * between two interface types is legal at every source level and reaches
     * exactly the `close()` that try-with-resources would emit, so what is
     * exercised is unchanged. A RUNTIME older than 21 has no such executor to
     * exercise at all; that is said on the CK line instead of being counted as
     * a check that did not run.
     */
    static void executorClose() throws Exception {
        Method newVirtual;
        Method newPlatform;
        try {
            newVirtual = Executors.class.getMethod("newVirtualThreadPerTaskExecutor");
            newPlatform = Executors.class.getMethod("newThreadPerTaskExecutor", ThreadFactory.class);
        } catch (NoSuchMethodException e) {
            // Absent on a real JDK 17 runtime; absent on a Java 21+ runtime it
            // is the defect, not a level, so do not let it pass in silence.
            check(Runtime.version().feature() < 21,
                    "Java " + Runtime.version().feature() + " lacks " + e.getMessage());
            System.out.println("CK thread-per-task=absent");
            return;
        }
        System.out.println("CK thread-per-task=present");

        List<Future<Integer>> fs = new ArrayList<>();
        ExecutorService virt = (ExecutorService) newVirtual.invoke(null);
        try {
            for (int i = 0; i < 16; i++) {
                final int k = i;
                fs.add(virt.submit(() -> k * k));
            }
            int sum = 0;
            for (Future<Integer> f : fs) {
                sum += f.get();
            }
            check(sum == 1240, "virtual-thread tasks summed to " + sum);
        } finally {
            // Returning from this at all is the assertion: `close()` is
            // `shutdown()` plus an UNBOUNDED `awaitTermination`, and
            // `ThreadPerTaskExecutor.tryTerminate()` only advances
            // SHUTDOWN -> TERMINATED once its `newKeySet()` of live threads
            // reports empty. A view that never does hangs here forever rather
            // than failing.
            ((AutoCloseable) virt).close();
        }

        ExecutorService plat =
                (ExecutorService) newPlatform.invoke(null, Executors.defaultThreadFactory());
        for (int i = 0; i < 8; i++) {
            plat.submit(() -> 1);
        }
        plat.shutdown();
        check(plat.awaitTermination(60, TimeUnit.SECONDS),
                "newThreadPerTaskExecutor did not reach TERMINATED");
    }

    /**
     * `keySet()` with no argument: a live, READ-ONLY view of the same map.
     *
     * It used to be a `java.util.HashSet` carrying a resync-on-read backing.
     * That got reads and `remove` right, and three things wrong: the cast
     * threw, `add` silently mutated a private snapshot instead of throwing,
     * and `retainAll` did not write through at all while `removeAll` returned
     * `false` after removing.
     */
    static void plainKeySetView() {
        ConcurrentHashMap<String, Integer> m = new ConcurrentHashMap<>();
        m.put("a", 1);
        m.put("b", 2);
        m.put("c", 3);
        Set<String> ks = m.keySet();

        ConcurrentHashMap.KeySetView<String, Integer> view =
                (ConcurrentHashMap.KeySetView<String, Integer>) ks;
        check(view.getMappedValue() == null,
                "keySet() has a mapped value: " + view.getMappedValue());
        check(view.getMap() == m, "keySet().getMap() is not the source map");

        // Live, not a snapshot.
        m.put("d", 4);
        check(ks.size() == 4 && ks.contains("d"), "keySet() did not see a later put");
        m.remove("d");
        check(ks.size() == 3 && !ks.contains("d"), "keySet() did not see a later remove");

        // Read-only: no mapped value means `add` cannot work.
        boolean threw = false;
        try {
            ks.add("nope");
        } catch (UnsupportedOperationException e) {
            threw = true;
        }
        check(threw, "keySet().add did not throw UnsupportedOperationException");
        check(!m.containsKey("nope"), "keySet().add mutated the map");
        threw = false;
        try {
            ks.addAll(Arrays.asList("p", "q"));
        } catch (UnsupportedOperationException e) {
            threw = true;
        }
        check(threw, "keySet().addAll did not throw UnsupportedOperationException");

        // Every removal path writes through to the map.
        check(ks.remove("c") && !m.containsKey("c"), "keySet().remove did not write through");
        m.put("c", 3);
        for (Iterator<String> it = ks.iterator(); it.hasNext(); ) {
            if (it.next().equals("c")) {
                it.remove();
            }
        }
        check(!m.containsKey("c"), "keySet().iterator().remove did not write through");
        m.put("e", 5);
        check(ks.removeIf(x -> x.equals("e")) && !m.containsKey("e"),
                "keySet().removeIf did not write through");
        m.put("f", 6);
        m.put("g", 7);
        List<String> drop = new ArrayList<>();
        drop.add("f");
        drop.add("g");
        check(ks.removeAll(drop) && !m.containsKey("f") && !m.containsKey("g"),
                "keySet().removeAll did not write through (or reported false having removed)");
        m.put("h", 8);
        List<String> keep = new ArrayList<>();
        keep.add("a");
        keep.add("b");
        check(ks.retainAll(keep) && !m.containsKey("h") && m.containsKey("a"),
                "keySet().retainAll did not write through");
        ks.clear();
        check(m.isEmpty() && ks.isEmpty(), "keySet().clear did not write through");

        // The Spring `SimpleAliasRegistry.getAliases` shape: iteration order is
        // HotSpot's flat-table bucket order, and all four readers agree on it.
        ConcurrentHashMap<String, String> am = new ConcurrentHashMap<>(16);
        am.put("myalias", "x");
        am.put("youralias", "y");
        am.put("thirdalias", "z");
        List<String> viaKeySet = new ArrayList<>(am.keySet());
        List<String> viaForEach = new ArrayList<>();
        am.forEach((k, v) -> viaForEach.add(k));
        List<String> viaIterator = new ArrayList<>();
        for (String k : am.keySet()) {
            viaIterator.add(k);
        }
        check(viaKeySet.equals(viaForEach) && viaKeySet.equals(viaIterator),
                "keySet()/forEach/iterator disagree on order: " + viaKeySet + " " + viaForEach
                        + " " + viaIterator);
        System.out.println("CK keySet order=" + viaKeySet);

        // The `CachedIntrospectionResults.clearClassLoader` shape.
        ConcurrentHashMap<String, String> cache = new ConcurrentHashMap<>();
        for (int i = 0; i < 10; i++) {
            cache.put("k" + i, i % 2 == 0 ? "keep" : "drop");
        }
        cache.keySet().removeIf(k -> "drop".equals(cache.get(k)));
        check(cache.size() == 5, "keySet().removeIf evicted " + (10 - cache.size()) + " of 5");

        // Reached polymorphically, the static receiver type must not decide the class.
        Map<String, Integer> asMap = m;
        m.put("z", 1);
        check(asMap.keySet().contains("z") && asMap.keySet().size() == 1,
                "((Map) chm).keySet() disagrees with chm.keySet()");
    }

    public static void main(String[] args) throws Exception {
        identity();
        surface();
        mappedKeySetView();
        plainKeySetView();
        churn();
        concurrentAdds();
        executorClose();
        System.out.println("CK RChmKeySetView checks=" + checks);
        System.out.println("PASS RChmKeySetView");
    }
}
