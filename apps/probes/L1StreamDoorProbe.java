import java.util.*;
import java.util.stream.*;

/** L1 §5 — does a dispatch door reach the `java/util/stream/*` INTERFACE
 *  registrations? Exercises the interface surface broadly so the registry dump
 *  taken from this run answers precondition 4 per triple. Prints shapes only.
 */
public final class L1StreamDoorProbe {
    static int rows = 0;
    static void t(String tag, Object v) { rows++; System.out.println(tag + " |" + v + "|"); }

    public static void main(String[] a) {
        List<String> src = new ArrayList<>(List.of("delta", "alpha", "charlie", "bravo", "alpha"));

        t("count", src.stream().count());
        t("filter+toList", src.stream().filter(s -> s.length() > 4).collect(Collectors.toList()));
        t("map", src.stream().map(String::toUpperCase).collect(Collectors.toList()));
        t("sorted", src.stream().sorted().collect(Collectors.toList()));
        t("distinct", src.stream().distinct().collect(Collectors.toList()));
        t("limit", src.stream().limit(2).collect(Collectors.toList()));
        t("skip", src.stream().skip(3).collect(Collectors.toList()));
        t("anyMatch", src.stream().anyMatch(s -> s.startsWith("a")));
        t("allMatch", src.stream().allMatch(s -> s.length() > 3));
        t("noneMatch", src.stream().noneMatch(String::isEmpty));
        t("findFirst", src.stream().findFirst().orElse("none"));
        t("min", src.stream().min(Comparator.naturalOrder()).orElse("none"));
        t("max", src.stream().max(Comparator.naturalOrder()).orElse("none"));
        t("reduce", src.stream().reduce("", (x, y) -> x + y.charAt(0)));
        t("joining", src.stream().collect(Collectors.joining(",", "[", "]")));
        t("groupingBy", new TreeMap<>(src.stream().collect(Collectors.groupingBy(String::length))));
        t("partitioningBy", new TreeMap<>(src.stream().collect(Collectors.partitioningBy(s -> s.length() > 4))));
        t("toSet-size", src.stream().collect(Collectors.toSet()).size());
        t("flatMap", src.stream().flatMap(s -> Stream.of(s.charAt(0))).collect(Collectors.toList()));
        t("peek", src.stream().peek(s -> {}).count());
        t("toArray", Arrays.toString(src.stream().toArray()));
        t("iterate", Stream.iterate(1, x -> x * 2).limit(5).collect(Collectors.toList()));
        t("generate", Stream.generate(() -> "z").limit(3).collect(Collectors.toList()));
        t("concat", Stream.concat(Stream.of("p"), Stream.of("q")).collect(Collectors.toList()));
        t("empty", Stream.empty().count());
        t("ofOne", Stream.of("solo").collect(Collectors.toList()));
        t("mapToInt-sum", src.stream().mapToInt(String::length).sum());
        t("mapToLong-sum", src.stream().mapToLong(String::length).sum());
        t("mapToDouble-sum", src.stream().mapToDouble(String::length).sum());
        t("boxed", IntStream.range(0, 4).boxed().collect(Collectors.toList()));
        t("intRange", IntStream.range(0, 5).sum());
        t("intRangeClosed", IntStream.rangeClosed(1, 4).sum());
        t("intFilter", IntStream.of(1, 2, 3, 4).filter(x -> x % 2 == 0).sum());
        t("intMap", IntStream.of(1, 2, 3).map(x -> x + 1).sum());
        t("intMax", IntStream.of(3, 9, 1).max().getAsInt());
        t("intMin", IntStream.of(3, 9, 1).min().getAsInt());
        t("intAvg", IntStream.of(2, 4).average().getAsDouble());
        t("intCount", IntStream.of(1, 2, 3).count());
        t("intToArray", Arrays.toString(IntStream.of(5, 6).toArray()));
        t("intAsLong", IntStream.of(1, 2).asLongStream().sum());
        t("intMapToObj", IntStream.of(1, 2).mapToObj(Integer::toString).collect(Collectors.toList()));
        t("longRange", LongStream.range(0, 5).sum());
        t("longOf", LongStream.of(7L, 8L).max().getAsLong());
        t("longMap", LongStream.of(1L, 2L).map(x -> x * 3).sum());
        t("longBoxed", LongStream.of(1L).boxed().collect(Collectors.toList()));
        t("dblOf", DoubleStream.of(1.5, 2.5).sum());
        t("dblMax", DoubleStream.of(1.5, 2.5).max().getAsDouble());
        t("dblMap", DoubleStream.of(1.0).map(x -> x + 1).sum());
        t("dblBoxed", DoubleStream.of(2.0).boxed().collect(Collectors.toList()));
        t("collSstream", src.stream().getClass() != null);
        t("parallelFlag", src.stream().isParallel());
        t("spliteratorNotNull", src.stream().spliterator() != null);
        t("iteratorHasNext", src.stream().iterator().hasNext());
        t("seq", src.stream().sequential().count());
        t("unordered", src.stream().unordered().count());
        t("onClose", src.stream().onClose(() -> {}).count());
        t("summaryStats", IntStream.of(1, 2, 3).summaryStatistics().getSum());
        t("counting", src.stream().collect(Collectors.counting()));
        t("toMap-size", src.stream().distinct().collect(Collectors.toMap(s -> s, String::length)).size());
        System.out.println("rows " + rows);
        System.out.println("DONE L1StreamDoorProbe");
    }
}
