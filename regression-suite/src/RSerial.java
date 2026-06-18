import java.io.*;
import java.util.*;

/**
 * Regression: java.io serialization round-trip — the synthetic OOS/OIS path.
 * Exercises primitive + reference + array fields, nested objects, a cyclic
 * self-reference (back-reference handle), and serialized collections.
 */
public class RSerial {
    static int checks = 0;
    static void check(boolean c, String m) { checks++; if (!c) throw new AssertionError(m); }

    static class Node implements Serializable {
        int id; long big; double d; String name;
        int[] data; List<String> tags; Node self; Node next;
        Node(int id) { this.id = id; }
    }

    @SuppressWarnings("unchecked")
    static <T> T roundtrip(T o) throws Exception {
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        try (ObjectOutputStream oos = new ObjectOutputStream(bos)) { oos.writeObject(o); }
        try (ObjectInputStream ois = new ObjectInputStream(new ByteArrayInputStream(bos.toByteArray()))) {
            return (T) ois.readObject();
        }
    }

    public static void main(String[] a) throws Exception {
        Node n = new Node(7);
        n.big = 9_000_000_000L; n.d = 3.5; n.name = "root";
        n.data = new int[] { 10, 20, 30 };
        n.tags = new ArrayList<>(Arrays.asList("x", "y"));
        n.self = n;                         // cyclic self-reference
        n.next = new Node(8); n.next.name = "child";

        Node r = roundtrip(n);
        check(r.id == 7 && r.big == 9_000_000_000L && r.d == 3.5, "primitive fields");
        check("root".equals(r.name), "String field");
        check(Arrays.equals(r.data, new int[] { 10, 20, 30 }), "int[] field");
        check(r.tags.equals(Arrays.asList("x", "y")), "List field");
        check(r.self == r, "self-reference resolves to same instance");
        check(r.next != null && r.next.id == 8 && "child".equals(r.next.name), "nested object");

        // ---- serialized collections ----
        ArrayList<Integer> list = new ArrayList<>(Arrays.asList(1, 2, 3, 4, 5));
        check(roundtrip(list).equals(list), "ArrayList round-trip");
        HashMap<String, Integer> map = new HashMap<>();
        map.put("a", 1); map.put("b", 2);
        check(roundtrip(map).equals(map), "HashMap round-trip");
        check(roundtrip("a plain string").equals("a plain string"), "String round-trip");
        check(roundtrip(Integer.valueOf(123)).equals(123), "boxed Integer round-trip");
        check(Arrays.equals(roundtrip(new long[] { 1L, 2L, 3L }), new long[] { 1L, 2L, 3L }), "long[] round-trip");

        System.out.println("PASS RSerial (" + checks + " checks)");
    }
}
