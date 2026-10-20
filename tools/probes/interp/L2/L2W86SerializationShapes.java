// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 86, lane L2 -- Java serialization as the VM serves
// it (the JDK's own ObjectOutputStream / ObjectInputStream bytecode over the
// VM's reflection, Unsafe, method-handle, class-init and stack-walk answers):
//
//   ctor-*         the first NON-serializable superclass's no-arg constructor
//                  runs on deserialization; serializable classes' constructors
//                  and field initializers do not; transient fields read back 0
//   resolve-*      readResolve keeps a singleton's identity; writeReplace
//                  substitutes a stand-in whose readResolve builds the original
//   record-*       a record is rebuilt through its canonical constructor (its
//                  compact body runs and validates)
//   enum-*         an enum constant (also one with a body) reads back as itself
//   ext-*          Externalizable: the public no-arg constructor runs, then
//                  readExternal
//   invalid-*      InvalidClassException texts: no valid constructor,
//                  serialVersionUID mismatch (stream patched), no public
//                  constructor for an Externalizable
//   notser         NotSerializableException names the class
//   filter-*       a stream ObjectInputFilter rejecting a class, the
//                  depth limit, and the default (no) process-wide filter
//   lambda-*       a serializable lambda round trip and the capturing class's
//                  synthetic $deserializeLambda$
//   hooks-*        private writeObject/readObject hooks with defaultWriteObject
//                  plus extra data
//
// Run (stdout must be the block below, byte for byte):
//   $J/javac -d out tools/probes/interp/L2/L2W86SerializationShapes.java
//   $B --java-home $J/.. -cp out L2W86SerializationShapes
//   $B --compatible --java-home $J/.. -cp out L2W86SerializationShapes
//   (each also with --nojit)
//   $J/java -cp out L2W86SerializationShapes
//
// HOTSPOT_EXPECTED_BEGIN
// ctor-log Base() Mid()
// ctor-read-log Base()
// ctor-fields x=3 midInit=5 t=0 baseInit=7 midCtor=false
// resolve-singleton true
// resolve-replaced true value=11
// record-log canonical(1,2) canonical(1,2)
// record-equal true
// record-invalid java.io.InvalidObjectException: negative
// enum-same true true
// enum-body-class true
// ext-log Ext() writeExternal Ext() readExternal
// ext-value 17
// invalid-noctor java.io.InvalidClassException: L2W86SerializationShapes$NoCtor; no valid constructor
// invalid-suid java.io.InvalidClassException: L2W86SerializationShapes$Versioned; local class incompatible: stream classdesc serialVersionUID = 2, local class serialVersionUID = 1
// invalid-ext java.io.InvalidClassException: L2W86SerializationShapes$ExtNoCtor; no valid constructor
// notser java.io.NotSerializableException: java.lang.Object
// filter-reject java.io.InvalidClassException: filter status: REJECTED
// filter-depth java.io.InvalidClassException: filter status: REJECTED
// filter-global null
// lambda-run 42
// lambda-deserialize private static synthetic=true
// hooks x=4 extra=hello
// HOTSPOT_EXPECTED_END
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.Externalizable;
import java.io.IOException;
import java.io.InvalidObjectException;
import java.io.ObjectInput;
import java.io.ObjectInputFilter;
import java.io.ObjectInputStream;
import java.io.ObjectOutput;
import java.io.ObjectOutputStream;
import java.io.ObjectStreamException;
import java.io.Serializable;
import java.lang.invoke.SerializedLambda;
import java.lang.reflect.Method;
import java.lang.reflect.Modifier;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import java.util.function.IntSupplier;

public class L2W86SerializationShapes {
    static final List<String> LOG = new ArrayList<>();

    static String drain() {
        String s = String.join(" ", LOG);
        LOG.clear();
        return s;
    }

    static byte[] write(Object o) throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        try (ObjectOutputStream out = new ObjectOutputStream(bytes)) {
            out.writeObject(o);
        }
        return bytes.toByteArray();
    }

    static Object read(byte[] b) throws IOException, ClassNotFoundException {
        try (ObjectInputStream in = new ObjectInputStream(new ByteArrayInputStream(b))) {
            return in.readObject();
        }
    }

    static Object roundTrip(Object o) throws IOException, ClassNotFoundException {
        return read(write(o));
    }

    // -- ctor-* -----------------------------------------------------------
    static class Base {
        int baseInit = 7;

        Base() {
            LOG.add("Base()");
        }
    }

    static class Mid extends Base implements Serializable {
        int midInit = 5;
        transient int t = 9;
        int x;
        transient boolean midCtor;

        Mid() {
            LOG.add("Mid()");
            midCtor = true;
        }
    }

    // -- resolve-* --------------------------------------------------------
    static final class Singleton implements Serializable {
        static final Singleton INSTANCE = new Singleton();

        private Object readResolve() throws ObjectStreamException {
            return INSTANCE;
        }
    }

    static final class Original implements Serializable {
        final int value;

        Original(int value) {
            this.value = value;
        }

        private Object writeReplace() throws ObjectStreamException {
            return new StandIn(value);
        }
    }

    static final class StandIn implements Serializable {
        final int v;

        StandIn(int v) {
            this.v = v;
        }

        private Object readResolve() throws ObjectStreamException {
            return new Original(v);
        }
    }

    // -- record-* ---------------------------------------------------------
    record Point(int x, int y) implements Serializable {
        Point {
            LOG.add("canonical(" + x + "," + y + ")");
            if (x < 0) {
                throw new IllegalArgumentException("negative");
            }
        }
    }

    // -- enum-* -----------------------------------------------------------
    enum Color {
        RED,
        GREEN {
            @Override
            public String toString() {
                return "green";
            }
        }
    }

    // -- ext-* ------------------------------------------------------------
    public static final class Ext implements Externalizable {
        int value;

        public Ext() {
            LOG.add("Ext()");
        }

        @Override
        public void writeExternal(ObjectOutput out) throws IOException {
            LOG.add("writeExternal");
            out.writeInt(value);
        }

        @Override
        public void readExternal(ObjectInput in) throws IOException {
            LOG.add("readExternal");
            value = in.readInt();
        }
    }

    public static final class ExtNoCtor implements Externalizable {
        ExtNoCtor(int ignored) {
        }

        @Override
        public void writeExternal(ObjectOutput out) {
        }

        @Override
        public void readExternal(ObjectInput in) {
        }
    }

    // -- invalid-* --------------------------------------------------------
    static class NoCtorBase {
        NoCtorBase(int x) {
        }
    }

    static class NoCtor extends NoCtorBase implements Serializable {
        NoCtor() {
            super(1);
        }
    }

    static class Versioned implements Serializable {
        private static final long serialVersionUID = 1L;
        int v = 1;
    }

    // -- hooks-* ----------------------------------------------------------
    static class Hooked implements Serializable {
        int x;
        transient String extra;

        private void writeObject(ObjectOutputStream out) throws IOException {
            out.defaultWriteObject();
            out.writeUTF("hello");
        }

        private void readObject(ObjectInputStream in) throws IOException, ClassNotFoundException {
            in.defaultReadObject();
            extra = in.readUTF();
        }
    }

    static class Nest implements Serializable {
        Nest next;
    }

    static int lambdaBase() {
        return 40;
    }

    /** Replace the 8 bytes after the class name's UTF in `stream`. */
    static byte[] patchSuid(byte[] stream, String className, long suid) {
        byte[] name = className.getBytes(StandardCharsets.UTF_8);
        outer:
        for (int i = 0; i + name.length + 8 <= stream.length; i++) {
            for (int j = 0; j < name.length; j++) {
                if (stream[i + j] != name[j]) {
                    continue outer;
                }
            }
            int at = i + name.length;
            for (int k = 0; k < 8; k++) {
                stream[at + k] = (byte) (suid >>> (56 - 8 * k));
            }
            return stream;
        }
        throw new AssertionError("class name not in the stream");
    }

    static String outcome(Throwable t) {
        return t.getClass().getName() + ": " + t.getMessage();
    }

    public static void main(String[] args) throws Exception {
        // ctor-*
        Mid mid = new Mid();
        mid.x = 3;
        System.out.println("ctor-log " + drain());
        Mid back = (Mid) roundTrip(mid);
        System.out.println("ctor-read-log " + drain());
        System.out.println("ctor-fields x=" + back.x + " midInit=" + back.midInit + " t=" + back.t
                + " baseInit=" + back.baseInit + " midCtor=" + back.midCtor);

        // resolve-*
        System.out.println("resolve-singleton " + (roundTrip(Singleton.INSTANCE) == Singleton.INSTANCE));
        Object replaced = roundTrip(new Original(11));
        System.out.println("resolve-replaced " + (replaced instanceof Original) + " value="
                + ((Original) replaced).value);

        // record-*
        Point p = new Point(1, 2);
        Point pBack = (Point) roundTrip(p);
        System.out.println("record-log " + drain());
        System.out.println("record-equal " + p.equals(pBack));
        // A stream whose x is negative: the canonical constructor's check runs.
        byte[] negative = write(new Point(5, 2));
        LOG.clear();
        // x is the first int field (fields sorted by name: x, y) -- the 8
        // bytes before the stream's end are x then y.
        int at = negative.length - 8;
        negative[at] = (byte) 0xff;
        negative[at + 1] = (byte) 0xff;
        negative[at + 2] = (byte) 0xff;
        negative[at + 3] = (byte) 0xfb;
        try {
            read(negative);
            System.out.println("record-invalid no-throw");
        } catch (Exception e) {
            System.out.println("record-invalid " + outcome(e));
        }
        LOG.clear();

        // enum-*
        System.out.println("enum-same " + (roundTrip(Color.RED) == Color.RED) + " "
                + (roundTrip(Color.GREEN) == Color.GREEN));
        System.out.println("enum-body-class " + (Color.GREEN.getClass() != Color.class));

        // ext-*
        Ext ext = new Ext();
        ext.value = 17;
        Ext extBack = (Ext) roundTrip(ext);
        System.out.println("ext-log " + drain());
        System.out.println("ext-value " + extBack.value);

        // invalid-*
        try {
            roundTrip(new NoCtor());
            System.out.println("invalid-noctor no-throw");
        } catch (Exception e) {
            System.out.println("invalid-noctor " + outcome(e));
        }
        try {
            read(patchSuid(write(new Versioned()), Versioned.class.getName(), 2L));
            System.out.println("invalid-suid no-throw");
        } catch (Exception e) {
            System.out.println("invalid-suid " + outcome(e));
        }
        try {
            roundTrip(new ExtNoCtor(1));
            System.out.println("invalid-ext no-throw");
        } catch (Exception e) {
            System.out.println("invalid-ext " + outcome(e));
        }

        // notser
        try {
            write(new Object[] {new Object()});
            System.out.println("notser no-throw");
        } catch (Exception e) {
            System.out.println("notser " + outcome(e));
        }

        // filter-*
        byte[] midBytes = write(mid);
        try (ObjectInputStream in = new ObjectInputStream(new ByteArrayInputStream(midBytes))) {
            in.setObjectInputFilter(ObjectInputFilter.Config.createFilter("!" + Mid.class.getName()));
            in.readObject();
            System.out.println("filter-reject no-throw");
        } catch (Exception e) {
            System.out.println("filter-reject " + outcome(e));
        }
        Nest head = new Nest();
        head.next = new Nest();
        head.next.next = new Nest();
        try (ObjectInputStream in = new ObjectInputStream(new ByteArrayInputStream(write(head)))) {
            in.setObjectInputFilter(ObjectInputFilter.Config.createFilter("maxdepth=2"));
            in.readObject();
            System.out.println("filter-depth no-throw");
        } catch (Exception e) {
            System.out.println("filter-depth " + outcome(e));
        }
        System.out.println("filter-global " + ObjectInputFilter.Config.getSerialFilter());

        // lambda-*
        int captured = 2;
        IntSupplier lambda = (IntSupplier & Serializable) () -> lambdaBase() + captured;
        IntSupplier lambdaBack = (IntSupplier) roundTrip(lambda);
        System.out.println("lambda-run " + lambdaBack.getAsInt());
        Method des = L2W86SerializationShapes.class.getDeclaredMethod("$deserializeLambda$",
                SerializedLambda.class);
        System.out.println("lambda-deserialize " + Modifier.toString(des.getModifiers()) + " synthetic="
                + des.isSynthetic());

        // hooks-*
        Hooked h = new Hooked();
        h.x = 4;
        Hooked hBack = (Hooked) roundTrip(h);
        System.out.println("hooks x=" + hBack.x + " extra=" + hBack.extra);
    }
}
