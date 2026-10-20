import java.util.*;
import java.util.function.*;
import java.util.stream.*;

/**
 * SBR round 5, lane compat2: cost of the stream pipeline, native carrier (--compatible, row off)
 * against the JDK's own pipeline (row on / --jdk-only / HotSpot). Prints microseconds per
 * operation for each shape, best of the rounds after a warm-up. Run it several times and
 * compare CPU time (the host is loaded).
 */
public class Sbr5Compat2StreamsBench {
    static long sink;

    static double us(String name, int rounds, int iters, Supplier<Object> body) {
        double best = Double.MAX_VALUE;
        for (int r = 0; r < rounds; r++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < iters; i++) {
                Object o = body.get();
                sink += o == null ? 0 : o.hashCode();
            }
            double per = (System.nanoTime() - t0) / 1000.0 / iters;
            if (per < best) best = per;
        }
        System.out.printf("%-34s %10.2f us%n", name, best);
        return best;
    }

    public static void main(String[] args) {
        List<Integer> list = IntStream.range(0, 1000).boxed().collect(Collectors.toList());
        int[] ints = IntStream.range(0, 1000).toArray();
        List<String> words = list.stream().map(i -> "w" + (i % 37)).collect(Collectors.toList());
        int rounds = 7;
        us("list.stream.map.filter.sum", rounds, 400, () -> list.stream().map(i -> i * 3).filter(i -> (i & 1) == 0).mapToInt(i -> i).sum());
        us("list.stream.collect.toList", rounds, 400, () -> list.stream().map(i -> i + 1).collect(Collectors.toList()));
        us("list.stream.groupingBy.count", rounds, 200, () -> words.stream().collect(Collectors.groupingBy(w -> w, Collectors.counting())));
        us("list.stream.sorted.limit", rounds, 200, () -> list.stream().sorted(Comparator.reverseOrder()).limit(10).collect(Collectors.toList()));
        us("intstream.range.sum", rounds, 1000, () -> IntStream.range(0, 1000).sum());
        us("intstream.range.map.sum", rounds, 1000, () -> IntStream.range(0, 1000).map(i -> i * 2).sum());
        us("array.stream.boxed.toList", rounds, 400, () -> Arrays.stream(ints).boxed().toList());
        us("stream.of.map.collect", rounds, 20000, () -> Stream.of("a", "b", "c", "d").map(String::toUpperCase).collect(Collectors.joining(",")));
        us("list.forEach.stream.anyMatch", rounds, 2000, () -> list.stream().anyMatch(i -> i == 900));
        us("list.stream.reduce", rounds, 400, () -> list.stream().reduce(0, Integer::sum));
        us("optional.map.orElse", rounds, 200000, () -> Optional.of("x").map(String::length).orElse(0));
        us("map.entrySet.stream.sum", rounds, 400, () -> {
            Map<Integer, Integer> m = new HashMap<>();
            for (int i = 0; i < 200; i++) m.put(i, i);
            return m.entrySet().stream().mapToInt(Map.Entry::getValue).sum();
        });
        us("string.chars.filter.count", rounds, 2000, () -> "the quick brown fox jumps over the lazy dog".chars().filter(Character::isLetter).count());
        us("parallel.sum.100k", 3, 20, () -> IntStream.range(0, 100000).parallel().sum());
        System.out.println("sink " + (sink != 42));
    }
}
