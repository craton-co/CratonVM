import java.util.*;
import java.util.concurrent.ConcurrentHashMap;

/**
 * Keys with IDENTICAL hashCode land in one bin on any table size, so the sort
 * that reorders CratonVM's segmented storage cannot separate them: whatever
 * order they come out in IS the chain order. That makes this probe a direct
 * read of whether the bucket chain is built by prepending or appending.
 *
 * "Aa"/"BB" both hash to 2112; the four-character products all hash to 2031744.
 */
public class ChainOrderProbe {
    static void run(String label, String[] keys) {
        ConcurrentHashMap<String, Integer> m = new ConcurrentHashMap<>();
        for (int i = 0; i < keys.length; i++) m.put(keys[i], i);
        StringBuilder sb = new StringBuilder();
        for (String k : m.keySet()) sb.append(k).append(' ');
        System.out.println(label + " hashes=" + keys[0].hashCode()
            + " inserted=" + String.join(" ", keys) + " | iterated=" + sb.toString().trim());
    }

    public static void main(String[] a) {
        run("pair  ", new String[]{"Aa", "BB"});
        run("pairR ", new String[]{"BB", "Aa"});
        run("quad  ", new String[]{"AaAa", "AaBB", "BBAa", "BBBB"});
        run("quadR ", new String[]{"BBBB", "BBAa", "AaBB", "AaAa"});
        // and the same through HashMap for comparison
        HashMap<String, Integer> h = new HashMap<>();
        for (String k : new String[]{"AaAa", "AaBB", "BBAa", "BBBB"}) h.put(k, 0);
        System.out.println("hashmap quad iterated=" + h.keySet());
        System.out.println("CHAIN_END");
    }
}
