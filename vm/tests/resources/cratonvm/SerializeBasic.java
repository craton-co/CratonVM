package cratonvm;

import java.io.*;

/**
 * Phase 91.1 + 91.2: ObjectOutputStream/ObjectInputStream basic tests.
 */
public class SerializeBasic implements Serializable {

    private static final long serialVersionUID = 1L;

    public int intValue;
    public String stringValue;
    public transient int transientValue;

    public SerializeBasic() {}

    public SerializeBasic(int i, String s, int t) {
        this.intValue = i;
        this.stringValue = s;
        this.transientValue = t;
    }

    // Nested serializable class
    static class Nested implements Serializable {
        private static final long serialVersionUID = 2L;
        public int x;
        public int y;
        public Nested() {}
        public Nested(int x, int y) { this.x = x; this.y = y; }
    }

    // Non-serializable class
    static class NotSerializable {
        public int value = 42;
    }

    // 91.1: Simple object write + 91.2: round-trip read
    public static int testSimpleRoundTrip() throws Exception {
        ByteArrayOutputStream baos = new ByteArrayOutputStream();
        ObjectOutputStream oos = new ObjectOutputStream(baos);
        SerializeBasic obj = new SerializeBasic(42, "hello", 99);
        oos.writeObject(obj);
        oos.flush();

        byte[] data = baos.toByteArray();
        ByteArrayInputStream bais = new ByteArrayInputStream(data);
        ObjectInputStream ois = new ObjectInputStream(bais);
        SerializeBasic result = (SerializeBasic) ois.readObject();

        return result.intValue;  // 42
    }

    // 91.1: Nested object serialization
    public static int testNestedObject() throws Exception {
        ByteArrayOutputStream baos = new ByteArrayOutputStream();
        ObjectOutputStream oos = new ObjectOutputStream(baos);
        Nested obj = new Nested(10, 20);
        oos.writeObject(obj);
        oos.flush();

        byte[] data = baos.toByteArray();
        ByteArrayInputStream bais = new ByteArrayInputStream(data);
        ObjectInputStream ois = new ObjectInputStream(bais);
        Nested result = (Nested) ois.readObject();

        return result.x + result.y;  // 30
    }

    // 91.1: Transient field is skipped (should be 0 after deserialization)
    public static int testTransientField() throws Exception {
        ByteArrayOutputStream baos = new ByteArrayOutputStream();
        ObjectOutputStream oos = new ObjectOutputStream(baos);
        SerializeBasic obj = new SerializeBasic(10, "test", 999);
        oos.writeObject(obj);
        oos.flush();

        byte[] data = baos.toByteArray();
        ByteArrayInputStream bais = new ByteArrayInputStream(data);
        ObjectInputStream ois = new ObjectInputStream(bais);
        SerializeBasic result = (SerializeBasic) ois.readObject();

        // transientValue should be 0 (default) after deserialization
        return result.transientValue == 0 ? 1 : 0;  // 1
    }

    // 91.1: Non-serializable throws NotSerializableException
    public static int testNonSerializableThrows() {
        try {
            ByteArrayOutputStream baos = new ByteArrayOutputStream();
            ObjectOutputStream oos = new ObjectOutputStream(baos);
            oos.writeObject(new NotSerializable());
            return 0;  // should not reach here
        } catch (Exception e) {
            return 1;  // caught NotSerializableException (wrapped as IOException)
        }
    }
}
