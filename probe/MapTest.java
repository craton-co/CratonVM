import java.util.*;

public class MapTest {
    public static void main(String[] args) {
        // Replicate _renameProperties map ops: iterate entrySet, it.remove() the "y" entry,
        // then get("y") / containsKey using a DIFFERENT String instance with same content.
        LinkedHashMap<String,Integer> map = new LinkedHashMap<>();
        map.put(new String("y"), 1);
        map.put(new String("setY"), 2);
        map.put(new String("other"), 3);

        Iterator<Map.Entry<String,Integer>> it = map.entrySet().iterator();
        while (it.hasNext()) {
            Map.Entry<String,Integer> e = it.next();
            if (e.getKey().equals("y")) it.remove();
        }
        // different String object, same content
        String lookup = new StringBuilder().append('y').toString();
        System.out.println("size=" + map.size() + " (expect 2)");
        System.out.println("containsKey(y)=" + map.containsKey(lookup) + " (expect false)");
        System.out.println("get(y)=" + map.get(lookup) + " (expect null)");

        // Also test put-after-remove (loop2 path when old==null)
        map.put(lookup, 99);
        System.out.println("after re-put size=" + map.size() + " (expect 3)");

        // Direct removal test
        LinkedHashMap<String,Integer> m2 = new LinkedHashMap<>();
        m2.put("y", 1);
        boolean rem = m2.entrySet().removeIf(en -> en.getKey().equals("y"));
        System.out.println("removeIf removed=" + rem + " m2.get(y)=" + m2.get("y") + " (expect true/null)");

        // hashCode of the key strings
        String k1 = new String("y"); String k2 = new StringBuilder().append('y').toString();
        System.out.println("k1.hashCode=" + k1.hashCode() + " k2.hashCode=" + k2.hashCode()
            + " equals=" + k1.equals(k2) + " == " + (k1==k2));
    }
}
