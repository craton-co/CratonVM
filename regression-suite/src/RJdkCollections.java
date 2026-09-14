import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.Comparator;
import java.util.HashMap;
import java.util.HashSet;
import java.util.Iterator;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.ListIterator;
import java.util.Map;
import java.util.NoSuchElementException;
import java.util.Optional;
import java.util.OptionalInt;
import java.util.Set;
import java.util.TreeMap;
import java.util.stream.Collectors;
import java.util.stream.IntStream;
import java.util.stream.Stream;

/**
 * JDK-only corpus: the collections graph -- {@code ArrayList}, {@code HashMap},
 * iterators, streams, {@code Optional}.
 *
 * These are the classes CratonVM most often shadows with native shims, so under
 * {@code --jdk-only} they must execute real {@code java.util} bytecode with real
 * field layouts.
 *
 * Determinism: hash-ordered containers are NEVER printed in iteration order --
 * every emitted view is sorted or comes from an insertion-ordered container.
 */
public class RJdkCollections {
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    static void lists() {
        List<String> l = new ArrayList<>();
        for (int i = 0; i < 10; i++) {
            l.add("e" + i);
        }
        check(l.size() == 10, "size");
        check(l.get(3).equals("e3"), "get");
        check(l.indexOf("e7") == 7, "indexOf");
        check(l.contains("e9") && !l.contains("e10"), "contains");
        l.add(2, "ins");
        check(l.get(2).equals("ins") && l.size() == 11, "add(index)");
        check(l.remove(2).equals("ins") && l.size() == 10, "remove(index)");
        check(l.set(0, "z").equals("e0") && l.get(0).equals("z"), "set");
        l.set(0, "e0");

        // subList is a view: writes through, and structural change to the backing
        // list must invalidate it.
        List<String> view = l.subList(2, 5);
        check(view.size() == 3 && view.get(0).equals("e2"), "subList view");
        view.set(0, "V");
        check(l.get(2).equals("V"), "subList write-through");
        l.set(2, "e2");

        // ListIterator, forward and backward, with mutation.
        ListIterator<String> it = l.listIterator();
        int seen = 0;
        while (it.hasNext()) {
            it.next();
            seen++;
        }
        while (it.hasPrevious()) {
            it.previous();
            seen++;
        }
        check(seen == 20, "listIterator traversal: " + seen);

        // Iterator.remove during traversal.
        List<Integer> nums = new ArrayList<>(Arrays.asList(1, 2, 3, 4, 5, 6, 7, 8));
        Iterator<Integer> ni = nums.iterator();
        while (ni.hasNext()) {
            if (ni.next() % 2 == 0) {
                ni.remove();
            }
        }
        check(nums.equals(Arrays.asList(1, 3, 5, 7)), "Iterator.remove: " + nums);

        // toArray(T[]) with an undersized and an oversized array.
        String[] exact = l.toArray(new String[0]);
        check(exact.length == 10 && exact[9].equals("e9"), "toArray(new T[0])");
        String[] big = l.toArray(new String[12]);
        check(big.length == 12 && big[10] == null, "toArray(oversized) null terminator");

        // Immutable List.of rejects mutation.
        List<String> imm = List.of("a", "b", "c");
        boolean threw = false;
        try {
            imm.add("d");
        } catch (UnsupportedOperationException expected) {
            threw = true;
        }
        check(threw, "List.of must be immutable");

        Collections.sort(nums, Comparator.reverseOrder());
        check(nums.equals(Arrays.asList(7, 5, 3, 1)), "Collections.sort with comparator");
        System.out.println("CK RJdkCollections list=" + l);
    }

    static void maps() {
        Map<String, Integer> m = new HashMap<>();
        for (int i = 0; i < 24; i++) {
            m.put("k" + i, i * i);
        }
        check(m.size() == 24, "map size");
        check(m.get("k5") == 25, "map get");
        check(m.get("absent") == null, "map miss");
        check(m.containsKey("k23") && !m.containsKey("k24"), "containsKey");
        check(m.getOrDefault("absent", -1) == -1, "getOrDefault");
        check(m.putIfAbsent("k5", 999) == 25, "putIfAbsent existing");
        check(m.putIfAbsent("new", 1) == null && m.get("new") == 1, "putIfAbsent new");
        check(m.remove("new") == 1, "remove");
        m.merge("k1", 10, Integer::sum);
        check(m.get("k1") == 11, "merge");
        m.merge("k1", -10, Integer::sum);
        check(m.computeIfAbsent("ci", k -> k.length()) == 2, "computeIfAbsent");
        check(m.computeIfPresent("ci", (k, v) -> v + 1) == 3, "computeIfPresent");
        m.remove("ci");

        // Iteration order of a HashMap is unspecified -- sort before emitting.
        List<String> keys = new ArrayList<>(m.keySet());
        Collections.sort(keys);
        check(keys.size() == 24 && keys.get(0).equals("k0"), "sorted keySet");
        long sum = 0;
        for (Map.Entry<String, Integer> e : m.entrySet()) {
            sum += e.getValue();
        }
        check(sum == 4324, "entrySet value sum: " + sum);

        // entrySet is a view: setValue writes through.
        for (Map.Entry<String, Integer> e : m.entrySet()) {
            if (e.getKey().equals("k2")) {
                e.setValue(-1);
            }
        }
        check(m.get("k2") == -1, "entrySet setValue write-through");
        m.put("k2", 4);

        // LinkedHashMap keeps insertion order; TreeMap keeps sorted order.
        Map<String, Integer> lhm = new LinkedHashMap<>();
        lhm.put("z", 1);
        lhm.put("a", 2);
        lhm.put("m", 3);
        check(new ArrayList<>(lhm.keySet()).equals(Arrays.asList("z", "a", "m")),
                "LinkedHashMap order");
        TreeMap<String, Integer> tm = new TreeMap<>(lhm);
        check(new ArrayList<>(tm.keySet()).equals(Arrays.asList("a", "m", "z")), "TreeMap order");
        check(tm.firstKey().equals("a") && tm.lastKey().equals("z"), "TreeMap first/last");
        check(tm.headMap("m").size() == 1 && tm.tailMap("m").size() == 2, "TreeMap head/tail");

        Set<String> set = new HashSet<>(Arrays.asList("b", "a", "c", "a"));
        check(set.size() == 3, "HashSet dedup");
        List<String> sorted = new ArrayList<>(set);
        Collections.sort(sorted);
        System.out.println("CK RJdkCollections map=" + tm + " set=" + sorted + " sum=" + sum);
    }

    static void streams() {
        List<Integer> src = IntStream.rangeClosed(1, 20).boxed().collect(Collectors.toList());
        List<Integer> evens = src.stream().filter(i -> i % 2 == 0).collect(Collectors.toList());
        check(evens.size() == 10, "stream filter");
        int total = src.stream().mapToInt(Integer::intValue).sum();
        check(total == 210, "stream sum: " + total);
        String joined = src.stream().limit(5).map(String::valueOf)
                .collect(Collectors.joining(","));
        check(joined.equals("1,2,3,4,5"), "joining: " + joined);
        // Grouping produces a HashMap -- wrap it in a TreeMap before printing.
        Map<Boolean, List<Integer>> parts = src.stream()
                .collect(Collectors.partitioningBy(i -> i > 10));
        check(parts.get(Boolean.TRUE).size() == 10, "partitioningBy");
        Map<Integer, Long> byMod = src.stream()
                .collect(Collectors.groupingBy(i -> i % 3, Collectors.counting()));
        check(new TreeMap<>(byMod).toString().equals("{0=6, 1=7, 2=7}"), "groupingBy: " + byMod);
        check(src.stream().anyMatch(i -> i == 13), "anyMatch");
        check(src.stream().allMatch(i -> i > 0), "allMatch");
        check(src.stream().noneMatch(i -> i > 20), "noneMatch");
        check(src.stream().reduce(0, Integer::sum) == 210, "reduce");
        check(Stream.of("a", "bb", "ccc").flatMap(s -> s.chars().boxed()).count() == 6,
                "flatMap");
        List<Integer> sortedDesc = src.stream().sorted(Comparator.reverseOrder())
                .limit(3).collect(Collectors.toList());
        check(sortedDesc.equals(Arrays.asList(20, 19, 18)), "sorted: " + sortedDesc);
        // A sequential stream over a distinct/skip pipeline.
        check(Stream.of(1, 1, 2, 2, 3).distinct().skip(1).count() == 2, "distinct/skip");
        OptionalInt max = IntStream.of(3, 9, 4).max();
        check(max.isPresent() && max.getAsInt() == 9, "IntStream.max");
        System.out.println("CK RJdkCollections stream=" + new TreeMap<>(byMod) + " total=" + total);
    }

    /**
     * {@code AbstractPipeline.linkedOrConsumed} at the two sites W7-65 left open:
     * {@code close()} (its site 7, which sets UNCONDITIONALLY and does not check)
     * and {@code onClose(Runnable)} (its one check-only site, which throws and
     * does not set).
     *
     * Every expected value here was measured on HotSpot 25 and recorded in
     * docs/known-issues/jdk-only/W7-65-stream-reuse-throws.md -- two of them are
     * not what a reading of the JDK source alone predicts, which is why the
     * record says they were measured: {@code onClose} after a consume DOES throw
     * even though it never marks, and {@code close()} twice does NOT, even though
     * it always marks.
     *
     * The reds and the controls are labelled, because a flag set once too often
     * is the failure mode this whole area was declined twice for. Rows 1 and 2
     * fail on the pre-2026-08-12 behaviour (no-throw). Rows 3 and 4 are the
     * over-set controls: an implementation that marked-and-checked in
     * {@code close()}, or that marked in {@code onClose}, passes 1 and 2 and
     * fails these. Row 5 is the sibling-stream control -- it fails an
     * implementation that put the flag anywhere process-wide instead of on the
     * receiver.
     */
    static void streamReuse() {
        // 1. RED -- close() marks, so the next terminal must refuse.
        Stream<String> closedFirst = Stream.of("a", "b");
        closedFirst.close();
        boolean threw = false;
        try {
            closedFirst.count();
        } catch (IllegalStateException expected) {
            threw = true;
        }
        check(threw, "count() after close() must throw IllegalStateException");

        // 2. RED -- onClose() is the check-only site.
        Stream<String> consumed = Stream.of("a", "b");
        check(consumed.count() == 2, "the first terminal must still work");
        threw = false;
        try {
            consumed.onClose(() -> { });
        } catch (IllegalStateException expected) {
            threw = true;
        }
        check(threw, "onClose() after a terminal must throw IllegalStateException");

        // 3. CONTROL -- close() does not CHECK, so closing twice is legal.
        Stream<String> twice = Stream.of("a");
        threw = false;
        try {
            twice.close();
            twice.close();
        } catch (IllegalStateException unexpected) {
            threw = true;
        }
        check(!threw, "close() twice must not throw");

        // 4. CONTROL -- consume then close is the try-with-resources shape and
        //    must stay silent, which is the whole reason close() marks without
        //    checking.
        Stream<String> thenClosed = Stream.of("a", "b", "c");
        check(thenClosed.count() == 3, "terminal before close");
        threw = false;
        try {
            thenClosed.close();
        } catch (IllegalStateException unexpected) {
            threw = true;
        }
        check(!threw, "close() after a terminal must not throw");

        // 5. CONTROL -- the flag lives on the receiver. A fresh stream over the
        //    same source, taken after another was closed, must work; and a
        //    handler registered BEFORE the terminal must still run at close().
        List<String> src = Arrays.asList("x", "y");
        StringBuilder ran = new StringBuilder();
        Stream<String> fresh = src.stream().onClose(() -> ran.append("closed"));
        check(fresh.count() == 2, "a fresh stream after a closed sibling");
        fresh.close();
        check(ran.toString().equals("closed"),
                "a close handler registered before the terminal must still run");
        System.out.println("CK RJdkCollections streamReuse=" + ran);
    }

    static void optionals() {
        Optional<String> some = Optional.of("v");
        Optional<String> none = Optional.empty();
        check(some.isPresent() && !none.isPresent(), "isPresent");
        check(none.isEmpty() && !some.isEmpty(), "isEmpty");
        check(some.get().equals("v"), "get");
        check(none.orElse("d").equals("d"), "orElse");
        check(none.orElseGet(() -> "g").equals("g"), "orElseGet");
        check(some.map(String::toUpperCase).get().equals("V"), "map");
        check(some.filter(s -> s.equals("x")).isEmpty(), "filter");
        check(some.flatMap(s -> Optional.of(s + "!")).get().equals("v!"), "flatMap");
        check(Optional.ofNullable(null).isEmpty(), "ofNullable(null)");
        check(some.equals(Optional.of("v")), "Optional.equals");
        check(some.hashCode() == "v".hashCode(), "Optional.hashCode");
        boolean threw = false;
        try {
            none.get();
        } catch (NoSuchElementException expected) {
            threw = true;
        }
        check(threw, "Optional.empty().get() must throw NoSuchElementException");
        threw = false;
        try {
            none.orElseThrow(() -> new IllegalStateException("boom"));
        } catch (IllegalStateException expected) {
            threw = "boom".equals(expected.getMessage());
        }
        check(threw, "orElseThrow");
        StringBuilder sb = new StringBuilder();
        some.ifPresent(sb::append);
        none.ifPresent(sb::append);
        check(sb.toString().equals("v"), "ifPresent");
        System.out.println("CK RJdkCollections optional=" + some + "/" + none);
    }

    public static void main(String[] args) {
        lists();
        maps();
        streams();
        streamReuse();
        optionals();
        System.out.println("CK RJdkCollections checks=" + checks);
        System.out.println("PASS RJdkCollections (" + checks + " checks)");
    }
}
