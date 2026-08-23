import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.stream.Collectors;
import java.util.stream.IntStream;
import java.util.stream.Stream;

/**
 * Copying a `Stream.toList()` result into another collection.
 *
 * `Stream.toList()` does not build an ArrayList: it hands the stream's
 * finished array straight to an immutable list without copying it
 * (`listFromTrustedArray`). Anything that then COPIES that list —
 * `ArrayList.addAll`, the copy constructor, `List.copyOf` — goes
 * through `toArray()`, which is a different method from the `size()`
 * and `iterator()` that a `toString()` exercises. A list that prints
 * all its elements can still hand out only some of them.
 */
public class StreamToListCopyProbe {

    static void report(String tag, List<Integer> src) {
        Object[] arr = src.toArray();
        Integer[] typed = src.toArray(new Integer[0]);
        List<Integer> viaAddAll = new ArrayList<>();
        viaAddAll.addAll(src);
        List<Integer> viaCtor = new ArrayList<>(src);
        int iterated = 0;
        for (Integer ignored : src) {
            iterated++;
        }
        java.util.List<Integer> viaCopyOf = List.copyOf(src);
        java.util.List<Integer> viaAsList = Arrays.asList(src.toArray(new Integer[0]));
        System.out.printf("%-16s %-46s size=%d iter=%d toArray=%d addAll=%d ctor=%d "
                + "copyOf=%d asList=%d%n",
                tag, src.getClass().getName(), src.size(), iterated,
                arr.length, viaAddAll.size(), viaCtor.size(), viaCopyOf.size(),
                viaAsList.size());
    }

    public static void main(String[] args) {
        int[] ints = {10445, 374, 279, 13180, 6437, 30};

        report("Stream.toList", Arrays.stream(ints).boxed().toList());
        report("List.of", List.of(10445, 374, 279, 13180, 6437, 30));
        report("collect(toList)", Arrays.stream(ints).boxed().collect(Collectors.toList()));
        report("ArrayList", new ArrayList<>(List.of(10445, 374, 279, 13180, 6437, 30)));
        report("Stream.of.toList", Stream.of(1, 2, 3, 4, 5, 6).toList());
        report("range.toList", IntStream.range(0, 6).boxed().toList());
        report("filtered.toList", IntStream.range(0, 12).filter(i -> i % 2 == 0).boxed().toList());
        report("mapped.toList", Stream.of(1, 2, 3, 4, 5, 6).map(x -> x * 2).toList());
        report("two-elem.toList", Stream.of(7, 8).toList());
        report("one-elem.toList", Stream.of(7).toList());
        report("empty.toList", Stream.<Integer>of().toList());
    }
}
