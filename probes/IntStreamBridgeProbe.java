import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

/**
 * The two stream conversions GPULlama3's tokenizer wraps around a
 * correct token list:
 *
 *   encodeImpl:    List<Integer> -> .stream().mapToInt(i -> i).toArray()
 *   encodeAsList:  int[]         -> Arrays.stream(..).boxed().toList()
 *
 * The BPE loop that feeds them is already known to produce the same six
 * ids on both VMs, and the application still ends up with one. So one
 * of these two is dropping elements.
 */
public class IntStreamBridgeProbe {
    static void check(String tag, List<Integer> src) {
        int[] arr = src.stream().mapToInt(i -> i).toArray();
        List<Integer> back = Arrays.stream(arr).boxed().toList();
        System.out.printf("%-18s in=%d toArray=%d boxed=%d %s%n",
                tag, src.size(), arr.length, back.size(), Arrays.toString(arr));
    }

    public static void main(String[] args) {
        check("List.of", List.of(10445, 374, 279, 13180, 6437, 30));
        check("ArrayList", new ArrayList<>(List.of(10445, 374, 279, 13180, 6437, 30)));

        List<Integer> grown = new ArrayList<>();
        grown.addAll(List.of(10445, 374, 279, 13180, 6437));
        grown.addAll(List.of(30));
        check("addAll-twice", grown);

        List<Integer> big = new ArrayList<>();
        for (int i = 0; i < 40; i++) {
            big.add(i * 7 + 1);
        }
        check("forty", big);

        // The exact composition encodeAsList performs.
        int[] once = grown.stream().mapToInt(i -> i).toArray();
        List<Integer> twice = Arrays.stream(once).boxed().toList();
        System.out.printf("COMPOSED n=%d %s%n", twice.size(), twice);

        // mapToInt on a stream that has already been mapped.
        int[] viaMap = grown.stream().map(x -> x).mapToInt(Integer::intValue).toArray();
        System.out.printf("VIA_MAP n=%d%n", viaMap.length);

        // sum/count as an independent read of the same pipeline.
        System.out.printf("COUNT %d SUM %d%n",
                grown.stream().mapToInt(i -> i).count(),
                grown.stream().mapToInt(i -> i).sum());
    }
}
