import java.util.*;
import java.util.function.*;
import java.util.stream.*;

/** L3 — `AbstractPipeline.linkedOrConsumed`, across every stream shape.
 *
 *  `W7-65-stream-reuse-throws.md` fixed reuse for REFERENCE streams and left a
 *  named residual set open: primitive streams (§5.1) and short-layout streams
 *  (§5.6). It also says, of its own update, "Nothing here has been built or
 *  run." That was 18 days ago.
 *
 *  A stream is single-use. The JDK sets `linkedOrConsumed` when a stage is
 *  linked to OR consumed, and every later use of that stage throws
 *  `IllegalStateException: stream has already been operated upon or closed`.
 *  The rule is uniform across `Stream`, `IntStream`, `LongStream` and
 *  `DoubleStream`, so a VM that models it for one and not the others is
 *  inconsistent in a way that only shows on the shapes nobody probed.
 *
 *  THE MESSAGE IS PART OF THE OBSERVABLE. A bare "did it throw" row cannot
 *  tell `IllegalStateException` raised by the pipeline from one raised for some
 *  other reason, so every row prints the type AND the message.
 *
 *  Nothing here prints a stream's contents where order is unspecified; the
 *  sources are ordered and the terminals are counts or sorted.
 */
public class StreamReuseProbe {
    static int rows = 0;

    interface Val { Object call() throws Throwable; }

    static String esc(String s) {
        return s == null ? "null" : s.replace("\n", "\\n").replace("\r", "\\r");
    }

    static void p(String tag, Object v) {
        rows++;
        System.out.println(rows + " " + esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }

    static void tv(String tag, Val c) {
        try { p(tag, c.call()); }
        catch (Throwable e) {
            p(tag, "THREW " + e.getClass().getName() + " msg=" + esc(e.getMessage()));
        }
    }

    /** First use succeeds, second use must throw. Both halves are printed,
     *  because a row that only shows the second cannot tell a correct refusal
     *  from a stream that was already broken. */
    static <T> void reuse(String tag, Supplier<T> make, Function<T, Object> use) {
        T s = make.get();
        tv(tag + " first", () -> use.apply(s));
        tv(tag + " second", () -> use.apply(s));
    }

    public static void main(String[] args) {
        List<String> src = List.of("a", "b", "c");
        int[] ints = {1, 2, 3};
        long[] longs = {1L, 2L, 3L};
        double[] dbls = {1.0, 2.0, 3.0};

        // ---- REFERENCE streams: the shape W7-65 fixed, as the control.
        reuse("ref count", src::stream, s -> s.count());
        reuse("ref forEach", src::stream, s -> { s.forEach(x -> {}); return "ok"; });
        reuse("ref collect", src::stream, s -> s.collect(Collectors.toList()).size());
        reuse("ref iterator", src::stream, s -> { s.iterator(); return "ok"; });
        reuse("ref spliterator", src::stream, s -> { s.spliterator(); return "ok"; });
        reuse("ref toArray", src::stream, s -> s.toArray().length);
        reuse("ref findFirst", src::stream, s -> String.valueOf(s.findFirst().orElse(null)));
        reuse("ref anyMatch", src::stream, s -> s.anyMatch(x -> true));
        reuse("ref reduce", src::stream, s -> String.valueOf(s.reduce("", (a, b) -> a + b)));
        reuse("ref toList", src::stream, s -> s.toList().size());

        // LINKED, not consumed: an intermediate op also arms the flag.
        reuse("ref filter then filter", src::stream, s -> { s.filter(x -> true); return "ok"; });
        reuse("ref map then map", src::stream, s -> { s.map(x -> x); return "ok"; });
        reuse("ref sorted", src::stream, s -> { s.sorted(); return "ok"; });
        reuse("ref parallel", src::stream, s -> { s.parallel(); return "ok"; });
        reuse("ref sequential", src::stream, s -> { s.sequential(); return "ok"; });
        reuse("ref onClose", src::stream, s -> { s.onClose(() -> {}); return "ok"; });

        // A stage reused after a DIFFERENT stage consumed it.
        Stream<String> st = src.stream();
        Stream<String> mapped = st.map(x -> x);
        tv("ref parent after child linked", () -> st.count());
        tv("ref child still usable", () -> mapped.count());
        tv("ref child after own terminal", () -> mapped.count());

        // close() makes every later use throw too.
        Stream<String> cs = src.stream();
        cs.close();
        tv("ref count after close", () -> cs.count());
        tv("ref close twice", () -> { cs.close(); return "ok"; });

        // ---- PRIMITIVE streams: W7-65 §5.1, recorded open.
        reuse("int count", () -> Arrays.stream(ints), s -> s.count());
        reuse("int sum", () -> Arrays.stream(ints), s -> s.sum());
        reuse("int forEach", () -> Arrays.stream(ints), s -> { s.forEach(x -> {}); return "ok"; });
        reuse("int iterator", () -> Arrays.stream(ints), s -> { s.iterator(); return "ok"; });
        reuse("int toArray", () -> Arrays.stream(ints), s -> s.toArray().length);
        reuse("int filter", () -> Arrays.stream(ints), s -> { s.filter(x -> true); return "ok"; });
        reuse("int boxed", () -> Arrays.stream(ints), s -> { s.boxed(); return "ok"; });
        reuse("int asLongStream", () -> Arrays.stream(ints), s -> { s.asLongStream(); return "ok"; });
        reuse("int average", () -> Arrays.stream(ints), s -> s.average().getAsDouble());
        reuse("int max", () -> Arrays.stream(ints), s -> s.max().getAsInt());
        reuse("int summaryStatistics", () -> Arrays.stream(ints),
                s -> s.summaryStatistics().getSum());

        reuse("IntStream.range count", () -> IntStream.range(0, 3), s -> s.count());
        reuse("IntStream.of count", () -> IntStream.of(1, 2, 3), s -> s.count());
        reuse("mapToInt count", () -> src.stream().mapToInt(String::length), s -> s.count());

        reuse("long count", () -> Arrays.stream(longs), s -> s.count());
        reuse("long sum", () -> Arrays.stream(longs), s -> s.sum());
        reuse("long iterator", () -> Arrays.stream(longs), s -> { s.iterator(); return "ok"; });
        reuse("LongStream.range count", () -> LongStream.range(0, 3), s -> s.count());

        reuse("double count", () -> Arrays.stream(dbls), s -> s.count());
        reuse("double sum", () -> Arrays.stream(dbls), s -> s.sum());
        reuse("double iterator", () -> Arrays.stream(dbls), s -> { s.iterator(); return "ok"; });
        reuse("DoubleStream.of count", () -> DoubleStream.of(1.0, 2.0), s -> s.count());

        IntStream ic = IntStream.of(1, 2, 3);
        ic.close();
        tv("int count after close", () -> ic.count());

        // ---- SHORT-LAYOUT sources: W7-65 §5.6. An empty or one-element source
        // is where a VM that special-cases small collections stops modelling
        // the pipeline at all.
        reuse("empty list count", () -> List.<String>of().stream(), s -> s.count());
        reuse("one list count", () -> List.of("a").stream(), s -> s.count());
        reuse("Stream.empty count", Stream::<String>empty, s -> s.count());
        reuse("Stream.of one count", () -> Stream.of("a"), s -> s.count());
        reuse("IntStream.empty count", IntStream::empty, s -> s.count());
        reuse("empty int array count", () -> Arrays.stream(new int[0]), s -> s.count());
        reuse("singleton set count", () -> Collections.singleton("a").stream(), s -> s.count());
        reuse("emptySet count", () -> Collections.<String>emptySet().stream(), s -> s.count());

        // ---- Other sources, so the rule is not read off one collection.
        reuse("HashMap keySet count", () -> new HashMap<>(Map.of("a", 1)).keySet().stream(),
                s -> s.count());
        reuse("TreeMap values count", () -> new TreeMap<>(Map.of("a", 1)).values().stream(),
                s -> s.count());
        reuse("ArrayList count", () -> new ArrayList<>(src).stream(), s -> s.count());
        reuse("ArrayDeque count", () -> new ArrayDeque<>(src).stream(), s -> s.count());
        reuse("array stream count", () -> Arrays.stream(new String[] {"a", "b"}), s -> s.count());
        reuse("parallelStream count", () -> new ArrayList<>(src).parallelStream(), s -> s.count());

        // WHICH CARRIER each source hands back. A reuse row that does not
        // throw is either a synthetic stream whose flag nothing sets or a real
        // java.base pipeline that should have thrown on its own, and only the
        // class name tells those apart.
        p("cls list.stream", src.stream().getClass().getName());
        p("cls Arrays.stream(T[])", Arrays.stream(new String[] {"a"}).getClass().getName());
        p("cls parallelStream", new ArrayList<>(src).parallelStream().getClass().getName());
        p("cls singleton", Collections.singleton("a").stream().getClass().getName());
        p("cls emptySet", Collections.emptySet().stream().getClass().getName());
        p("cls ArrayDeque", new ArrayDeque<>(src).stream().getClass().getName());
        p("cls filter", src.stream().filter(x -> true).getClass().getName());
        p("cls map", src.stream().map(x -> x).getClass().getName());
        p("cls Arrays.stream(int[])", Arrays.stream(ints).getClass().getName());
        p("cls IntStream.of", IntStream.of(1).getClass().getName());
        p("cls IntStream.range", IntStream.range(0, 2).getClass().getName());
        p("cls mapToInt", src.stream().mapToInt(String::length).getClass().getName());
        p("cls LongStream.range", LongStream.range(0, 2).getClass().getName());
        p("cls HashMap keySet", new HashMap<>(Map.of("a", 1)).keySet().stream()
                .getClass().getName());
        p("cls empty list", List.<String>of().stream().getClass().getName());

        // CROSS-OP reuse: consume with one terminal, then try a DIFFERENT one.
        // If the second throws where a repeat of the first did not, the flag is
        // being SET and the first operation is simply not consulting it -- a
        // different defect from the flag never being set at all, and it decides
        // where the fix goes.
        for (Object[] row : new Object[][] {
                {"list", (Supplier<Stream<String>>) src::stream},
                {"singleton", (Supplier<Stream<String>>) () -> Collections.singleton("a").stream()},
                {"emptySet", (Supplier<Stream<String>>) () -> Collections.<String>emptySet().stream()},
                {"ArrayDeque", (Supplier<Stream<String>>) () -> new ArrayDeque<>(src).stream()},
                {"arraystream", (Supplier<Stream<String>>) () -> Arrays.stream(new String[] {"a", "b"})},
                {"parallel", (Supplier<Stream<String>>) () -> new ArrayList<>(src).parallelStream()},
        }) {
            String tag = (String) row[0];
            @SuppressWarnings("unchecked")
            Supplier<Stream<String>> mk = (Supplier<Stream<String>>) row[1];
            Stream<String> s1 = mk.get();
            tv(tag + " x count then iterator: count", () -> s1.count());
            tv(tag + " x count then iterator: iterator", () -> { s1.iterator(); return "ok"; });
            Stream<String> s2 = mk.get();
            tv(tag + " x count then filter: count", () -> s2.count());
            tv(tag + " x count then filter: filter", () -> { s2.filter(z -> true); return "ok"; });
            Stream<String> s3 = mk.get();
            tv(tag + " x iterator then count: iterator", () -> { s3.iterator(); return "ok"; });
            tv(tag + " x iterator then count: count", () -> s3.count());
        }

        System.out.println("DONE StreamReuseProbe");
    }
}
