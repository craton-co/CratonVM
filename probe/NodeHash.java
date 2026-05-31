import java.util.*;
import java.lang.reflect.*;

public class NodeHash {
    public static void main(String[] args) throws Exception {
        HashMap<String,Integer> m = new HashMap<>();
        m.put("a",1); m.put("b",2); m.put("c",3);

        Field tableF = HashMap.class.getDeclaredField("table");
        tableF.setAccessible(true);
        Object[] table = (Object[]) tableF.get(m);
        System.out.println("table len = " + (table==null?"null":table.length));
        Class<?> nodeC = Class.forName("java.util.HashMap$Node");
        Field hashF = nodeC.getDeclaredField("hash");  hashF.setAccessible(true);
        Field keyF  = nodeC.getDeclaredField("key");   keyF.setAccessible(true);
        Field nextF = nodeC.getDeclaredField("next");   nextF.setAccessible(true);

        // spread hash function as in HashMap.hash
        for (Object bucket : table) {
            Object node = bucket;
            while (node != null) {
                Object key = keyF.get(node);
                int storedHash = hashF.getInt(node);
                int h = key.hashCode();
                int spread = h ^ (h >>> 16);
                System.out.println("key=" + key + " key.hashCode=" + h
                    + " spread=" + spread + " storedHash=" + storedHash
                    + " MATCH=" + (spread == storedHash));
                node = nextF.get(node);
            }
        }

        // Now: iterator remove "b", then inspect table again
        Iterator<Map.Entry<String,Integer>> it = m.entrySet().iterator();
        while (it.hasNext()) { if (it.next().getKey().equals("b")) it.remove(); }
        System.out.println("after it.remove: size=" + m.size());
    }
}
