import java.util.*;
public class HashMapArmedStressProbe {
    public static void main(String[] a) {
        HashMap<String,Integer> m = new HashMap<>();
        for (int i = 0; i < 3000; i++) m.put("k"+i, i);
        System.out.println("size=" + m.size());
        int got = 0; for (int i = 0; i < 3000; i++) { Integer v = m.get("k"+i); if (v != null && v == i) got++; }
        System.out.println("get-back=" + got);
        int it = 0; try { for (Map.Entry<String,Integer> e : m.entrySet()) it++; System.out.println("entrySet-iterated=" + it); }
        catch (Throwable t) { System.out.println("entrySet-iterate THREW " + t.getClass().getName() + " after " + it); }
        int ks = 0; try { for (String k : m.keySet()) ks++; System.out.println("keySet-iterated=" + ks); }
        catch (Throwable t) { System.out.println("keySet-iterate THREW " + t.getClass().getName() + " after " + ks); }
    }
}
