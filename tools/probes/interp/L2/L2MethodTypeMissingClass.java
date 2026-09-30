// Interpreter round i1, wave 4, lane L2 — `ldc` of a CONSTANT_MethodType whose
// descriptor names a missing class.
//
// HotSpot resolves the descriptor's classes itself
// (SystemDictionary::find_method_handle_type, NCDFError), so the `ldc` throws
// NoClassDefFoundError naming the class, and JVMS §5.4.3 makes a second
// execution of the same entry throw the same error again. CratonVM used to
// surface the JDK factory's TypeNotPresentException instead (not a
// LinkageError, and never recorded).
//
// No setup: the holder class is assembled in memory (javac never emits `ldc`
// of a MethodType from source) and defined by a throwaway class loader.
//
// Expected stdout (HotSpot 25):
//   good: (String)void
//   bad#1: java.lang.NoClassDefFoundError: nope/Missing
//   bad#2: java.lang.NoClassDefFoundError: nope/Missing
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class L2MethodTypeMissingClass {
    static final class Loader extends ClassLoader {
        Loader() {
            super(L2MethodTypeMissingClass.class.getClassLoader());
        }

        Class<?> define(byte[] b) {
            return defineClass("L2MtHolder", b, 0, b.length);
        }
    }

    static void utf8(DataOutputStream out, String s) throws IOException {
        out.writeByte(1);
        out.writeUTF(s);
    }

    // public static Object <name>() { ldc #<cp>; areturn }
    static void method(DataOutputStream out, int name, int cp) throws IOException {
        out.writeShort(0x0009); // public static
        out.writeShort(name);
        out.writeShort(6); // ()Ljava/lang/Object;
        out.writeShort(1); // attributes
        out.writeShort(7); // Code
        out.writeInt(12 + 3);
        out.writeShort(1); // max_stack
        out.writeShort(0); // max_locals
        out.writeInt(3);
        out.writeByte(0x12); // ldc
        out.writeByte(cp);
        out.writeByte(0xb0); // areturn
        out.writeShort(0); // exception table
        out.writeShort(0); // attributes
    }

    static byte[] holder() throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(51); // CONSTANT_MethodType needs 51; straight-line code needs no StackMapTable
        out.writeShort(13); // constant_pool_count
        utf8(out, "L2MtHolder"); // #1
        out.writeByte(7); // #2 Class #1
        out.writeShort(1);
        utf8(out, "java/lang/Object"); // #3
        out.writeByte(7); // #4 Class #3
        out.writeShort(3);
        utf8(out, "bad"); // #5
        utf8(out, "()Ljava/lang/Object;"); // #6
        utf8(out, "Code"); // #7
        utf8(out, "(Lnope/Missing;)V"); // #8
        out.writeByte(16); // #9 MethodType #8
        out.writeShort(8);
        utf8(out, "good"); // #10
        utf8(out, "(Ljava/lang/String;)V"); // #11
        out.writeByte(16); // #12 MethodType #11
        out.writeShort(11);
        out.writeShort(0x0021); // public super
        out.writeShort(2); // this_class
        out.writeShort(4); // super_class
        out.writeShort(0); // interfaces
        out.writeShort(0); // fields
        out.writeShort(2); // methods
        method(out, 10, 12);
        method(out, 5, 9);
        out.writeShort(0); // class attributes
        out.flush();
        return bytes.toByteArray();
    }

    static String call(Method m) throws IllegalAccessException {
        try {
            return String.valueOf(m.invoke(null));
        } catch (InvocationTargetException e) {
            Throwable t = e.getCause();
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    public static void main(String[] a) throws Exception {
        Class<?> c = new Loader().define(holder());
        System.out.println("good: " + call(c.getMethod("good")));
        Method bad = c.getMethod("bad");
        System.out.println("bad#1: " + call(bad));
        System.out.println("bad#2: " + call(bad));
    }
}
