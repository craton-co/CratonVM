import java.util.*;
public class MapProbe {
    static void t(String label, Map<String,String> m) {
        System.out.println(label + " put1=" + m.put("k","A") + " put2=" + m.put("k","B")
            + " get=" + m.get("k") + " remove=" + m.remove("k") + " remove2=" + m.remove("k"));
        Set<String> s = new LinkedHashSet<>();
        System.out.println(label + " (set) add1=" + s.add("x") + " addDup=" + s.add("x")
            + " remove=" + s.remove("x") + " removeAgain=" + s.remove("x"));
    }
    public static void main(String[] a) {
        t("HashMap      ", new HashMap<>());
        t("LinkedHashMap", new LinkedHashMap<>());
    }
}
