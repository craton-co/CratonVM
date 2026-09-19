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

    static final class LhmOuter implements Serializable {
        private static final long serialVersionUID = 1L;
        final int limit;
        final LinkedHashMap<String, String> map;

        LhmOuter(int limit) {
            this.limit = limit;
            this.map = new LinkedHashMap<String, String>() {
                private static final long serialVersionUID = 1L;
                @Override
                protected boolean removeEldestEntry(Map.Entry<String, String> eldest) {
                    if (LhmOuter.this.limit < 0) {
                        throw new AssertionError("outer reference lost");
                    }
                    return size() > LhmOuter.this.limit;
                }
            };
        }
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
        LhmOuter lhm = new LhmOuter(100);
        lhm.map.put("a", "1");
        lhm.map.put("b", "2");
        LhmOuter lhmRoundTrip = roundtrip(lhm);
        // HashMap.readObject must not invoke removeEldestEntry before this
        // anonymous subclass's synthetic this$0 field is restored; the live
        // put after deserialize proves the outer reference is usable again.
        lhmRoundTrip.map.put("c", "3");
        check(lhmRoundTrip.limit == 100 && lhmRoundTrip.map.size() == 3
            && "1".equals(lhmRoundTrip.map.get("a")), "anonymous LinkedHashMap this$0 restored");
        check(roundtrip("a plain string").equals("a plain string"), "String round-trip");
        check(roundtrip(Integer.valueOf(123)).equals(123), "boxed Integer round-trip");
        check(Arrays.equals(roundtrip(new long[] { 1L, 2L, 3L }), new long[] { 1L, 2L, 3L }), "long[] round-trip");

        // ---- DataOutputStream.written visible to a SUBCLASS via getfield ----
        // (HIB-CV-25b) The inherited protected `written` counter must be the
        // SAME field the native writer maintains. A subclass that reads
        // `this.written` (as jboss-classfilewriter's ByteArrayDataOutputStream
        // does to record back-patch positions) previously saw a stale 0 — the
        // natives wrote a hardcoded wrong slot — corrupting generated class-file
        // magic and breaking all Weld client-proxy generation.
        WrittenSpy spy = new WrittenSpy(new ByteArrayOutputStream());
        check(spy.peek() == 0, "DOS.written initial");
        spy.writeInt(0xCAFEBABE);
        check(spy.size() == 4, "DOS.size after writeInt");
        check(spy.peek() == 4, "DOS.written subclass-visible after writeInt");
        spy.writeShort(0x41);
        spy.writeByte(7);
        check(spy.size() == spy.peek(), "DOS.size == subclass written");
        check(spy.peek() == 7, "DOS.written subclass-visible after mixed writes");
        // back-patch at the recorded position must not clobber the leading int
        byte[] bytes = ((ByteArrayOutputStream) spy.sink()).toByteArray();
        check((bytes[0] & 0xFF) == 0xCA && (bytes[1] & 0xFF) == 0xFE
            && (bytes[2] & 0xFF) == 0xBA && (bytes[3] & 0xFF) == 0xBE, "DOS leading int intact");

        // ---- StringWriter inherited fields visible to a SUBCLASS (HIB-CV-25b
        //      sibling). The JDK sets Writer.lock == StringWriter.buf (a
        //      StringBuffer). A native that modelled StringWriter as char[]+count
        //      squatted those slots, so a subclass saw lock as a char[]/int[].
        LockSpy lspy = new LockSpy();
        lspy.write("abc"); lspy.append('d');
        check("abcd".equals(lspy.toString()), "StringWriter functional");
        check(lspy.peekLock() instanceof StringBuffer, "StringWriter.lock is the StringBuffer (subclass-visible)");
        check(lspy.peekLock() == lspy.getBuffer(), "StringWriter lock == buf (JDK invariant)");

        System.out.println("PASS RSerial (" + checks + " checks)");
    }

    /** Exposes the inherited protected {@code written} counter to verify the
     *  native byte-count and the real field slot agree (HIB-CV-25b). */
    static final class WrittenSpy extends DataOutputStream {
        private final OutputStream s;
        WrittenSpy(OutputStream out) { super(out); this.s = out; }
        int peek() { return this.written; }   // subclass getfield of inherited field
        OutputStream sink() { return s; }
    }

    /** Reads the inherited {@code Writer.lock} via getfield to verify the native
     *  StringWriter did not squat the real reference-typed slots (HIB-CV-25b). */
    static final class LockSpy extends StringWriter {
        Object peekLock() { return this.lock; }
    }
}
