// Interpreter round i1 wave 15, lane L4 -- a bootstrap method named through a
// subclass initializes only the class that DECLARES it (JVMS 5.5).
//
// A `CONSTANT_MethodHandle` REF_invokeStatic -> `Methodref CondyChild.bsm`
// resolves to `bsm` declared in `CondyParent`; invoking it runs
// `CondyParent.<clinit>` and never `CondyChild.<clinit>` (HotSpot's
// DirectMethodHandle initializes the member's declaring class). The same for
// an invokedynamic bootstrap (`IndyChild.bsm`, declared in `IndyParent`).
// javac always names the declaring class, so the probe assembles the class
// file that names the subclass (`L7BsmGen`, class file version 55) and
// defines it through its own class loader. Bytecode generators (ByteBuddy,
// ASM-based frameworks, language runtimes) emit this shape.
//
// HotSpot 25 prints exactly:
//   condy value: condy-value
//   indy value: indy-value
//   CondyParent.<clinit>
//   CondyParent.bsm
//   IndyParent.<clinit>
//   IndyParent.bsm
//
// CratonVM before wave 15 also printed `CondyChild.<clinit>` and
// `IndyChild.<clinit>` (the condy route initialized the named class, the
// generic indy route went through `invoke_shared`, which does the same).
// Compare `--compatible` with and without `--nojit`: identical stdout.
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.lang.invoke.CallSite;
import java.lang.invoke.ConstantCallSite;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.ArrayList;
import java.util.List;

public class L7BootstrapInitializesDeclaringClass {
    static final List<String> LOG = new ArrayList<>();

    public static class CondyParent {
        static {
            LOG.add("CondyParent.<clinit>");
        }

        public static Object bsm(MethodHandles.Lookup lookup, String name, Class<?> type) {
            LOG.add("CondyParent.bsm");
            return "condy-value";
        }
    }

    public static class CondyChild extends CondyParent {
        static {
            LOG.add("CondyChild.<clinit>");
        }
    }

    public static class IndyParent {
        static {
            LOG.add("IndyParent.<clinit>");
        }

        public static CallSite bsm(MethodHandles.Lookup lookup, String name, MethodType type) {
            LOG.add("IndyParent.bsm");
            return new ConstantCallSite(MethodHandles.constant(Object.class, "indy-value"));
        }
    }

    public static class IndyChild extends IndyParent {
        static {
            LOG.add("IndyChild.<clinit>");
        }
    }

    static final class Loader extends ClassLoader {
        Loader(ClassLoader parent) {
            super(parent);
        }

        Class<?> define(String name, byte[] bytes) {
            return defineClass(name, bytes, 0, bytes.length);
        }
    }

    // Hard-coded rather than `CondyChild.class.getName()`, so nothing but the
    // bootstrap calls below touches the four nested classes.
    static final String OUTER = "L7BootstrapInitializesDeclaringClass";
    static final String GEN = "L7BsmGen";

    public static void main(String[] args) throws Exception {
        byte[] bytes = generate();
        Class<?> gen = new Loader(L7BootstrapInitializesDeclaringClass.class.getClassLoader())
                .define(GEN, bytes);
        System.out.println("condy value: " + call(gen, "condy"));
        System.out.println("indy value: " + call(gen, "indy"));
        for (String line : LOG) {
            System.out.println(line);
        }
    }

    static String call(Class<?> gen, String method) {
        try {
            return String.valueOf(gen.getMethod(method).invoke(null));
        } catch (java.lang.reflect.InvocationTargetException e) {
            return "threw " + e.getCause().getClass().getName();
        } catch (ReflectiveOperationException e) {
            return "threw " + e.getClass().getName();
        }
    }

    // --- A class file with a condy and an indy whose bootstraps are named
    // through the child classes. No branches, so no StackMapTable. ---

    static final int UTF8 = 1, CLASS = 7, METHODREF = 10, NAME_AND_TYPE = 12,
            METHOD_HANDLE = 15, DYNAMIC = 17, INVOKE_DYNAMIC = 18;
    static final int REF_INVOKE_STATIC = 6;

    static byte[] generate() throws IOException {
        ByteArrayOutputStream buf = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(buf);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(55);
        out.writeShort(30); // constant_pool_count: entries 1..29
        utf8(out, GEN); // 1
        ref(out, CLASS, 1); // 2
        utf8(out, "java/lang/Object"); // 3
        ref(out, CLASS, 3); // 4
        utf8(out, OUTER + "$CondyChild"); // 5
        ref(out, CLASS, 5); // 6
        utf8(out, "bsm"); // 7
        utf8(out, "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/Class;)"
                + "Ljava/lang/Object;"); // 8
        pair(out, NAME_AND_TYPE, 7, 8); // 9
        pair(out, METHODREF, 6, 9); // 10
        handle(out, 10); // 11
        utf8(out, "value"); // 12
        utf8(out, "Ljava/lang/Object;"); // 13
        pair(out, NAME_AND_TYPE, 12, 13); // 14
        pair(out, DYNAMIC, 0, 14); // 15: bootstrap 0
        utf8(out, OUTER + "$IndyChild"); // 16
        ref(out, CLASS, 16); // 17
        utf8(out, "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;"
                + "Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/CallSite;"); // 18
        pair(out, NAME_AND_TYPE, 7, 18); // 19
        pair(out, METHODREF, 17, 19); // 20
        handle(out, 20); // 21
        utf8(out, "call"); // 22
        utf8(out, "()Ljava/lang/Object;"); // 23
        pair(out, NAME_AND_TYPE, 22, 23); // 24
        pair(out, INVOKE_DYNAMIC, 1, 24); // 25: bootstrap 1
        utf8(out, "condy"); // 26
        utf8(out, "indy"); // 27
        utf8(out, "Code"); // 28
        utf8(out, "BootstrapMethods"); // 29

        out.writeShort(0x0021); // ACC_PUBLIC | ACC_SUPER
        out.writeShort(2); // this_class
        out.writeShort(4); // super_class
        out.writeShort(0); // interfaces
        out.writeShort(0); // fields
        out.writeShort(2); // methods
        // public static Object condy() { ldc #15; areturn }
        method(out, 26, new byte[] {0x12, 15, (byte) 0xB0});
        // public static Object indy() { invokedynamic #25; areturn }
        method(out, 27, new byte[] {(byte) 0xBA, 0, 25, 0, 0, (byte) 0xB0});
        out.writeShort(1); // attributes
        out.writeShort(29); // BootstrapMethods
        out.writeInt(2 + 2 * 4);
        out.writeShort(2);
        out.writeShort(11); // CondyChild.bsm, no static arguments
        out.writeShort(0);
        out.writeShort(21); // IndyChild.bsm, no static arguments
        out.writeShort(0);
        out.flush();
        return buf.toByteArray();
    }

    static void utf8(DataOutputStream out, String s) throws IOException {
        out.writeByte(UTF8);
        out.writeUTF(s);
    }

    static void ref(DataOutputStream out, int tag, int index) throws IOException {
        out.writeByte(tag);
        out.writeShort(index);
    }

    static void pair(DataOutputStream out, int tag, int a, int b) throws IOException {
        out.writeByte(tag);
        out.writeShort(a);
        out.writeShort(b);
    }

    static void handle(DataOutputStream out, int methodref) throws IOException {
        out.writeByte(METHOD_HANDLE);
        out.writeByte(REF_INVOKE_STATIC);
        out.writeShort(methodref);
    }

    static void method(DataOutputStream out, int name, byte[] code) throws IOException {
        out.writeShort(0x0009); // ACC_PUBLIC | ACC_STATIC
        out.writeShort(name);
        out.writeShort(23); // ()Ljava/lang/Object;
        out.writeShort(1); // attributes
        out.writeShort(28); // Code
        out.writeInt(2 + 2 + 4 + code.length + 2 + 2);
        out.writeShort(1); // max_stack
        out.writeShort(0); // max_locals
        out.writeInt(code.length);
        out.write(code);
        out.writeShort(0); // exception_table_length
        out.writeShort(0); // attributes
    }
}
