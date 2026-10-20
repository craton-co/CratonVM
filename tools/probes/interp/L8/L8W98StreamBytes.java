// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 98, lane L8 -- the serialization WIRE FORMAT as
// the VM serves it. In the shipping builds the JDK's own ObjectOutputStream
// bytecode writes the bytes, over the VM's reflection answers (declared
// fields, field types, modifiers, declared methods/constructors for the
// default serialVersionUID, `hasStaticInitializer`), its lambda proxies'
// `writeReplace` (SerializedLambda), enums, records and String encoding. Each
// row prints the stream (hex, or length + CRC32 for the long ones), so any
// difference from HotSpot shows as a byte difference; the round trip is
// checked too.
//
//   hier       serializable superclass chain: super descriptor first in the
//              class-desc chain, super data first; transient / static skipped
//   order      ObjectStreamClass.getFields(): primitives first, then
//              references, each by name; type strings
//   hook       private writeObject: defaultWriteObject + block data + object
//   ext        Externalizable (protocol 2 block data)
//   enum       TC_ENUM for a constant and for a constant with a body
//   shared     a String and an int[] written twice (TC_REFERENCE), String[]
//   classes    Class objects: String.class, int.class, an enum class
//   longstr    a 70000-char String (TC_LONGSTRING), modified UTF-8 of
//              U+0000 / U+00E9 / U+10400 in a short one
//   record     a record (canonical constructor) and its stream
//   lambda     a serializable capturing lambda (SerializedLambda via the
//              proxy's writeReplace) and its round trip
//   suid-*     DEFAULT serialVersionUIDs: inner class (synthetic this$0),
//              assert ($assertionsDisabled + <clinit>), compile-time constant
//              only (no <clinit>) vs a non-constant static, anonymous and
//              local (captured val$) classes, a Serializable interface with a
//              default method, an abstract class with a native and a
//              synchronized method, a class holding a serializable lambda
//              (private static synthetic lambda$ / $deserializeLambda$)
//
// The expected block is HotSpot's output; it was not derived by hand (hashes
// and stream bytes) -- the orchestrator's HotSpot run is the reference.
//
// Run (stdout must equal HotSpot's, byte for byte):
//   $J/javac -d out tools/probes/interp/L8/L8W98StreamBytes.java
//   $B --java-home $J/.. -cp out L8W98StreamBytes
//   $B --compatible --java-home $J/.. -cp out L8W98StreamBytes
//   (each also with --nojit)
//   $J/java -cp out L8W98StreamBytes
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.Externalizable;
import java.io.IOException;
import java.io.ObjectInput;
import java.io.ObjectInputStream;
import java.io.ObjectOutput;
import java.io.ObjectOutputStream;
import java.io.ObjectStreamClass;
import java.io.ObjectStreamField;
import java.io.Serializable;
import java.util.Arrays;
import java.util.function.IntSupplier;
import java.util.zip.CRC32;

public class L8W98StreamBytes {
    static class A implements Serializable {
        private static final long serialVersionUID = 11L;
        int a = 1;
        String s = "s";
    }

    static class B extends A {
        private static final long serialVersionUID = 12L;
        static int st = 4;
        long b = 2;
        Object o;
        transient int t = 3;
    }

    static class Order implements Serializable {
        private static final long serialVersionUID = 13L;
        Object o;
        long b;
        int c;
        String s;
        boolean z;
        int[] arr;
        String[][] grid;
        char ch;
    }

    static class Hook implements Serializable {
        private static final long serialVersionUID = 14L;
        int x = 3;

        private void writeObject(ObjectOutputStream out) throws IOException {
            out.defaultWriteObject();
            out.writeInt(7);
            out.writeUTF("u");
            out.writeObject("obj");
        }

        private void readObject(ObjectInputStream in) throws IOException, ClassNotFoundException {
            in.defaultReadObject();
            x += in.readInt();
            in.readUTF();
            in.readObject();
        }
    }

    public static class Ext implements Externalizable {
        private static final long serialVersionUID = 15L;
        int v = 9;

        public Ext() {
        }

        @Override
        public void writeExternal(ObjectOutput out) throws IOException {
            out.writeInt(v);
            out.writeObject("e");
        }

        @Override
        public void readExternal(ObjectInput in) throws IOException, ClassNotFoundException {
            v = in.readInt() + 1;
            in.readObject();
        }
    }

    enum Color {
        RED,
        GREEN {
            @Override
            public String toString() {
                return "green";
            }
        }
    }

    record Rec(int n, String s) implements Serializable {
    }

    // -- default serialVersionUID shapes ------------------------------------
    class Inner implements Serializable {
        int v;
    }

    static class Asserting implements Serializable {
        int v;

        void check() {
            assert v >= 0;
        }
    }

    static class ConstOnly implements Serializable {
        static final int K = 5;
        int v;
    }

    static class NonConst implements Serializable {
        static final Integer K = 5;
        int v;
    }

    interface Iface extends Serializable {
        default int d() {
            return 1;
        }
    }

    abstract static class Abs implements Serializable {
        native void n();

        synchronized void s() {
        }

        abstract void a();
    }

    static class HoldsLambda implements Serializable {
        transient IntSupplier f = (IntSupplier & Serializable) () -> 1;
    }

    static String hex(byte[] b) {
        StringBuilder sb = new StringBuilder(b.length * 2);
        for (byte x : b) {
            sb.append(Character.forDigit((x >> 4) & 0xf, 16)).append(Character.forDigit(x & 0xf, 16));
        }
        return sb.toString();
    }

    static String crc(byte[] b) {
        CRC32 c = new CRC32();
        c.update(b);
        return "len=" + b.length + " crc=" + Long.toHexString(c.getValue());
    }

    static byte[] write(Object... objs) throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        try (ObjectOutputStream out = new ObjectOutputStream(bytes)) {
            for (Object o : objs) {
                out.writeObject(o);
            }
        }
        return bytes.toByteArray();
    }

    static Object[] read(byte[] b, int n) throws IOException, ClassNotFoundException {
        Object[] out = new Object[n];
        try (ObjectInputStream in = new ObjectInputStream(new ByteArrayInputStream(b))) {
            for (int i = 0; i < n; i++) {
                out[i] = in.readObject();
            }
        }
        return out;
    }

    static String fields(Class<?> c) {
        StringBuilder sb = new StringBuilder();
        for (ObjectStreamField f : ObjectStreamClass.lookup(c).getFields()) {
            if (sb.length() > 0) {
                sb.append(' ');
            }
            sb.append(f.getName()).append(':').append(f.getTypeCode()).append(':').append(f.getTypeString());
        }
        return sb.toString();
    }

    static long suid(Class<?> c) {
        return ObjectStreamClass.lookup(c).getSerialVersionUID();
    }

    public static void main(String[] args) throws Exception {
        B b = new B();
        b.o = "oo";
        byte[] hier = write(b);
        System.out.println("hier " + hex(hier));
        B bBack = (B) read(hier, 1)[0];
        System.out.println("hier-back a=" + bBack.a + " s=" + bBack.s + " b=" + bBack.b + " o=" + bBack.o
                + " t=" + bBack.t);

        System.out.println("order " + fields(Order.class));
        System.out.println("order-bytes " + hex(write(new Order())));

        byte[] hook = write(new Hook());
        System.out.println("hook " + hex(hook));
        System.out.println("hook-back x=" + ((Hook) read(hook, 1)[0]).x);

        byte[] ext = write(new Ext());
        System.out.println("ext " + hex(ext));
        System.out.println("ext-back v=" + ((Ext) read(ext, 1)[0]).v);

        byte[] en = write(Color.RED, Color.GREEN, Color.RED);
        System.out.println("enum " + hex(en));
        Object[] enBack = read(en, 3);
        System.out.println("enum-back " + (enBack[0] == Color.RED) + " " + (enBack[1] == Color.GREEN) + " "
                + (enBack[2] == Color.RED));

        String shared = "sh";
        int[] ints = {1, 2};
        byte[] sh = write(shared, ints, shared, ints, new String[] {"x", shared});
        System.out.println("shared " + hex(sh));
        Object[] shBack = read(sh, 5);
        System.out.println("shared-back " + (shBack[0] == shBack[2]) + " " + (shBack[1] == shBack[3]) + " "
                + Arrays.toString((String[]) shBack[4]) + " " + (((String[]) shBack[4])[1] == shBack[0]));

        byte[] cls = write(String.class, int.class, Color.class, int[].class);
        System.out.println("classes " + hex(cls));
        System.out.println("classes-back " + Arrays.toString(read(cls, 4)));

        char[] big = new char[70000];
        Arrays.fill(big, 'q');
        String odd = "\u0000é𐐀";
        byte[] ls = write(new String(big), odd);
        System.out.println("longstr " + crc(ls) + " tail=" + hex(Arrays.copyOfRange(ls, ls.length - 20, ls.length)));
        Object[] lsBack = read(ls, 2);
        System.out.println("longstr-back " + ((String) lsBack[0]).length() + " " + odd.equals(lsBack[1]));

        byte[] rec = write(new Rec(4, "r"));
        System.out.println("record " + hex(rec));
        System.out.println("record-back " + read(rec, 1)[0]);

        int captured = 5;
        IntSupplier lambda = (IntSupplier & Serializable) () -> captured + 1;
        byte[] lam = write(lambda);
        System.out.println("lambda " + crc(lam));
        System.out.println("lambda-back " + ((IntSupplier) read(lam, 1)[0]).getAsInt());

        System.out.println("suid-inner " + suid(Inner.class) + " " + fields(Inner.class));
        System.out.println("suid-assert " + suid(Asserting.class));
        System.out.println("suid-const " + suid(ConstOnly.class));
        System.out.println("suid-nonconst " + suid(NonConst.class));
        Serializable anon = new Serializable() {
            int v = captured;
        };
        System.out.println("suid-anon " + suid(anon.getClass()) + " " + fields(anon.getClass()));
        class Local implements Serializable {
            int w = captured;
        }
        System.out.println("suid-local " + suid(Local.class) + " " + fields(Local.class));
        System.out.println("suid-iface " + suid(Iface.class));
        System.out.println("suid-abstract " + suid(Abs.class));
        System.out.println("suid-holds-lambda " + suid(HoldsLambda.class));
        System.out.println("suid-main-class-lookupAny " + ObjectStreamClass.lookupAny(L8W98StreamBytes.class)
                .getSerialVersionUID());
    }
}
