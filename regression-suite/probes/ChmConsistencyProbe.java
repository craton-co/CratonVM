import java.util.*;
import java.util.concurrent.*;
public class ChmSeq {
    public static void main(String[] a) {
        ConcurrentHashMap<String,String> m1 = new ConcurrentHashMap<>();
        for (int i = 0; i < 4; i++) m1.put("k"+i, "v"+i);
        System.out.println("back-to-back puts   size=" + m1.size() + " keys=" + new TreeSet<>(m1.keySet()));

        ConcurrentHashMap<String,String> m2 = new ConcurrentHashMap<>();
        for (int i = 0; i < 4; i++) { m2.put("k"+i, "v"+i); m2.size(); }
        System.out.println("puts + size()       size=" + m2.size() + " keys=" + new TreeSet<>(m2.keySet()));

        ConcurrentHashMap<String,String> m3 = new ConcurrentHashMap<>();
        for (int i = 0; i < 4; i++) { m3.put("k"+i, "v"+i); Math.abs(1); }
        System.out.println("puts + Math.abs(1)  size=" + m3.size() + " keys=" + new TreeSet<>(m3.keySet()));

        ConcurrentHashMap<Integer,String> m4 = new ConcurrentHashMap<>();
        for (int i = 0; i < 4; i++) m4.put(i, "v"+i);
        System.out.println("Integer keys        size=" + m4.size());

        // door dependence
        ConcurrentHashMap<String,String> m5 = new ConcurrentHashMap<>();
        m5.putIfAbsent("only","x");
        Map<String,String> asMap = m5;
        System.out.println("via CHM  containsKey=" + m5.containsKey("only") + " keySet=" + m5.keySet() + " es=" + m5.entrySet().size());
        System.out.println("via Map  containsKey=" + asMap.containsKey("only") + " keySet=" + asMap.keySet() + " es=" + asMap.entrySet().size());
    }
}
