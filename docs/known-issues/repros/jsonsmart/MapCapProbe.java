import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.HashSet;
import java.util.Map;
import java.util.Set;

// Discriminates the suspected cause of the JIT-vs-interpreter HashMap capacity
// divergence: the no-arg `<init>()V` is the only constructor the JIT's
// trivial-constructor elision can rewrite to java/lang/Object.<init>. If the
// no-arg form diverges and the (int) form does not, the elision is skipping a
// side-effecting native constructor.
public class MapCapProbe {
    static Map<String, Object> noArg() {
        Map<String, Object> m = new HashMap<>();
        fill(m);
        return m;
    }

    static Map<String, Object> intArg() {
        Map<String, Object> m = new HashMap<>(16);
        fill(m);
        return m;
    }

    static Map<String, Object> linked() {
        Map<String, Object> m = new LinkedHashMap<>();
        fill(m);
        return m;
    }

    static Set<String> hashSet() {
        Set<String> s = new HashSet<>();
        s.add("empty_str");
        s.add("empty_arr");
        s.add("empty_obj");
        return s;
    }

    static void fill(Map<String, Object> m) {
        m.put("empty_str", "");
        m.put("empty_arr", "");
        m.put("empty_obj", "");
    }

    static String order(Iterable<?> it) {
        StringBuilder sb = new StringBuilder();
        for (Object k : it) sb.append(k).append(' ');
        return sb.toString();
    }

    public static void main(String[] args) {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 3000;
        String[] refs = new String[4];
        int[] flips = new int[4];
        for (int i = 0; i < iterations; i++) {
            String[] cur = {
                order(noArg().keySet()),
                order(intArg().keySet()),
                order(linked().keySet()),
                order(hashSet()),
            };
            for (int j = 0; j < cur.length; j++) {
                if (refs[j] == null) refs[j] = cur[j];
                else if (!refs[j].equals(cur[j])) {
                    flips[j]++;
                    if (flips[j] == 1) System.out.println("FLIP[" + j + "] at iter=" + i
                            + " ref=[" + refs[j] + "] got=[" + cur[j] + "]");
                }
            }
        }
        System.out.println("refs: noArg=[" + refs[0] + "] intArg=[" + refs[1] + "] linked=[" + refs[2]
                + "] hashSet=[" + refs[3] + "]");
        System.out.println("DONE noArg=" + flips[0] + " intArg=" + flips[1] + " linked=" + flips[2]
                + " hashSet=" + flips[3]);
        int total = flips[0] + flips[1] + flips[2] + flips[3];
        System.out.println(total == 0 ? "PROBE_PASS" : "PROBE_FAIL");
    }
}
