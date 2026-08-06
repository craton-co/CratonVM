import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.HashSet;
import java.util.Iterator;
import java.util.List;
import java.util.Set;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
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
        check(view.getMap() != null, "getMap() is null");
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

    /** The blast radius: try-with-resources on a thread-per-task executor. */
    static void executorClose() throws Exception {
        List<Future<Integer>> fs = new ArrayList<>();
        try (ExecutorService es = Executors.newVirtualThreadPerTaskExecutor()) {
            for (int i = 0; i < 16; i++) {
                final int k = i;
                fs.add(es.submit(() -> k * k));
            }
            int sum = 0;
            for (Future<Integer> f : fs) {
                sum += f.get();
            }
            check(sum == 1240, "virtual-thread tasks summed to " + sum);
        }
        // Reaching here at all is the assertion: close() is shutdown() plus an
        // UNBOUNDED awaitTermination, so a view that never reports empty hangs
        // the thread forever rather than failing.
        ExecutorService es = Executors.newThreadPerTaskExecutor(Thread.ofPlatform().factory());
        for (int i = 0; i < 8; i++) {
            es.submit(() -> 1);
        }
        es.shutdown();
        check(es.awaitTermination(60, TimeUnit.SECONDS),
                "newThreadPerTaskExecutor did not reach TERMINATED");
    }

    public static void main(String[] args) throws Exception {
        identity();
        surface();
        mappedKeySetView();
        churn();
        concurrentAdds();
        executorClose();
        System.out.println("CK RChmKeySetView checks=" + checks);
        System.out.println("PASS RChmKeySetView");
    }
}
