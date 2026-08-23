import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashSet;
import java.util.LinkedList;
import java.util.List;
import java.util.Vector;

/**
 * How many elements does each reader see in a REAL
 * `ImmutableCollections$ListN` — the list `Arrays.stream(int[]).boxed()
 * .toList()` produces by falling through to JDK bytecode?
 *
 * Splitting readers by destination type separates "the source list is
 * malformed" from "one native that copies collections misreads it".
 */
public class ListNReadProbe {
    public static void main(String[] args) {
        int[] ints = {10445, 374, 279, 13180, 6437, 30};
        List<Integer> src = Arrays.stream(ints).boxed().toList();

        System.out.printf("CLASS        %s%n", src.getClass().getName());
        System.out.printf("size         %d%n", src.size());
        System.out.printf("get(5)       %d%n", src.get(5));
        System.out.printf("contains(30) %b%n", src.contains(30));
        System.out.printf("indexOf(30)  %d%n", src.indexOf(30));
        System.out.printf("stream.count %d%n", src.stream().count());
        System.out.printf("toArray      %d%n", src.toArray().length);
        System.out.printf("subList(0,6) %d%n", src.subList(0, 6).size());

        List<Integer> al = new ArrayList<>();
        System.out.printf("ArrayList.addAll   %b size=%d%n", al.addAll(src), al.size());
        List<Integer> ll = new LinkedList<>();
        System.out.printf("LinkedList.addAll  %b size=%d%n", ll.addAll(src), ll.size());
        Vector<Integer> vec = new Vector<>();
        System.out.printf("Vector.addAll      %b size=%d%n", vec.addAll(src), vec.size());
        HashSet<Integer> hs = new HashSet<>();
        System.out.printf("HashSet.addAll     %b size=%d%n", hs.addAll(src), hs.size());

        System.out.printf("new ArrayList<>()  size=%d%n", new ArrayList<>(src).size());
        System.out.printf("List.copyOf        size=%d%n", List.copyOf(src).size());

        // The same content through a plain ArrayList, as a control.
        List<Integer> ctrl = new ArrayList<>(List.of(10445, 374, 279, 13180, 6437, 30));
        List<Integer> al2 = new ArrayList<>();
        al2.addAll(ctrl);
        System.out.printf("CONTROL addAll     size=%d%n", al2.size());
    }
}
