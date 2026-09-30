// Interpreter round i1, wave 2, lane L2 — CONSTANT_Dynamic runs its bootstrap.
//
// Assembles a tiny class file by hand (no ASM) whose three methods each `ldc`
// a CONSTANT_Dynamic, defines it through a ClassLoader, and calls them:
//
//   get()    condy Object, bootstrap bsmObj(Lookup,String,Class) -> new String
//   fail()   condy Object, bootstrap bsmThrow(...) throws IllegalStateException
//   getInt() condy int,    bootstrap bsmInt(Lookup,String,Class,int) with the
//            static argument 21 -> Integer.valueOf(42)
//
// Expected HotSpot 25 output (compare verbatim):
//   get: v42 same=true
//   calls(bsmObj)=1
//   fail#1: java.lang.BootstrapMethodError: bootstrap method initialization exception
//   fail#1 cause: java.lang.IllegalStateException: boom
//   fail#2: java.lang.BootstrapMethodError: bootstrap method initialization exception
//   calls(bsmThrow)=1
//   getInt: 42 name=twice type=int
//
// Before 2026-09-23 CratonVM never called a user bootstrap: `get` printed
// `null`, `fail` returned null instead of throwing, `getInt` printed 0, and
// every `calls(...)` line read 0.
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class L2CondyBootstrap {
    static int objCalls, throwCalls;
    static String seenName;
    static Class<?> seenType;

    public static Object bsmObj(MethodHandles.Lookup l, String name, Class<?> type) {
        objCalls++;
        return new String("v42");
    }

    public static Object bsmThrow(MethodHandles.Lookup l, String name, Class<?> type) {
        throwCalls++;
        throw new IllegalStateException(name);
    }

    public static Object bsmInt(MethodHandles.Lookup l, String name, Class<?> type, int k) {
        seenName = name;
        seenType = type;
        return Integer.valueOf(k * 2);
    }

    // ---- class-file assembly -------------------------------------------------

    static final String OBJ_BSM_DESC =
        "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/Object;";
    static final String INT_BSM_DESC =
        "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/Class;I)Ljava/lang/Object;";

    static byte[] assemble() throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream o = new DataOutputStream(bytes);
        o.writeInt(0xCAFEBABE);
        o.writeShort(0);
        o.writeShort(55); // Java 11: the first version with CONSTANT_Dynamic
        o.writeShort(40); // constant_pool_count = highest index + 1
        utf8(o, "CondyGen");                              // 1
        classRef(o, 1);                                   // 2
        utf8(o, "java/lang/Object");                      // 3
        classRef(o, 3);                                   // 4
        utf8(o, "L2CondyBootstrap");                      // 5
        classRef(o, 5);                                   // 6
        utf8(o, "bsmObj");                                // 7
        utf8(o, OBJ_BSM_DESC);                            // 8
        nameAndType(o, 7, 8);                             // 9
        methodRef(o, 6, 9);                               // 10
        methodHandle(o, 6, 10);                           // 11 REF_invokeStatic
        utf8(o, "c42");                                   // 12
        utf8(o, "Ljava/lang/Object;");                    // 13
        nameAndType(o, 12, 13);                           // 14
        dynamic(o, 0, 14);                                // 15
        utf8(o, "get");                                   // 16
        utf8(o, "()Ljava/lang/Object;");                  // 17
        utf8(o, "Code");                                  // 18
        utf8(o, "BootstrapMethods");                      // 19
        utf8(o, "bsmThrow");                              // 20
        nameAndType(o, 20, 8);                            // 21
        methodRef(o, 6, 21);                              // 22
        methodHandle(o, 6, 22);                           // 23
        utf8(o, "boom");                                  // 24
        nameAndType(o, 24, 13);                           // 25
        dynamic(o, 1, 25);                                // 26
        utf8(o, "fail");                                  // 27
        utf8(o, "bsmInt");                                // 28
        utf8(o, INT_BSM_DESC);                            // 29
        nameAndType(o, 28, 29);                           // 30
        methodRef(o, 6, 30);                              // 31
        methodHandle(o, 6, 31);                           // 32
        o.writeByte(3); o.writeInt(21);                   // 33 CONSTANT_Integer
        utf8(o, "twice");                                 // 34
        utf8(o, "I");                                     // 35
        nameAndType(o, 34, 35);                           // 36
        dynamic(o, 2, 36);                                // 37
        utf8(o, "getInt");                                // 38
        utf8(o, "()I");                                   // 39

        o.writeShort(0x0021); // ACC_PUBLIC | ACC_SUPER
        o.writeShort(2);      // this_class
        o.writeShort(4);      // super_class
        o.writeShort(0);      // interfaces
        o.writeShort(0);      // fields
        o.writeShort(3);      // methods
        method(o, 16, 17, 15, 0xB0); // get:    ldc #15; areturn
        method(o, 27, 17, 26, 0xB0); // fail:   ldc #26; areturn
        method(o, 38, 39, 37, 0xAC); // getInt: ldc #37; ireturn
        o.writeShort(1);      // class attributes
        o.writeShort(19);     // BootstrapMethods
        o.writeInt(2 + 4 + 4 + 6);
        o.writeShort(3);
        o.writeShort(11); o.writeShort(0);
        o.writeShort(23); o.writeShort(0);
        o.writeShort(32); o.writeShort(1); o.writeShort(33);
        o.flush();
        return bytes.toByteArray();
    }

    static void utf8(DataOutputStream o, String s) throws IOException { o.writeByte(1); o.writeUTF(s); }
    static void classRef(DataOutputStream o, int name) throws IOException { o.writeByte(7); o.writeShort(name); }
    static void nameAndType(DataOutputStream o, int n, int t) throws IOException {
        o.writeByte(12); o.writeShort(n); o.writeShort(t);
    }
    static void methodRef(DataOutputStream o, int c, int nt) throws IOException {
        o.writeByte(10); o.writeShort(c); o.writeShort(nt);
    }
    static void methodHandle(DataOutputStream o, int kind, int ref) throws IOException {
        o.writeByte(15); o.writeByte(kind); o.writeShort(ref);
    }
    static void dynamic(DataOutputStream o, int bsm, int nt) throws IOException {
        o.writeByte(17); o.writeShort(bsm); o.writeShort(nt);
    }
    static void method(DataOutputStream o, int name, int desc, int ldcIndex, int ret) throws IOException {
        o.writeShort(0x0009); // ACC_PUBLIC | ACC_STATIC
        o.writeShort(name);
        o.writeShort(desc);
        o.writeShort(1);      // attributes
        o.writeShort(18);     // Code
        o.writeInt(12 + 3);
        o.writeShort(1);      // max_stack
        o.writeShort(0);      // max_locals
        o.writeInt(3);        // code_length
        o.writeByte(0x12); o.writeByte(ldcIndex); o.writeByte(ret);
        o.writeShort(0);      // exception_table_length
        o.writeShort(0);      // attributes
    }

    static final class Definer extends ClassLoader {
        Definer(ClassLoader parent) { super(parent); }
        Class<?> define(byte[] b) { return defineClass("CondyGen", b, 0, b.length); }
    }

    public static void main(String[] a) throws Exception {
        Class<?> gen = new Definer(L2CondyBootstrap.class.getClassLoader()).define(assemble());
        Method get = gen.getMethod("get");
        Object first = get.invoke(null);
        Object second = get.invoke(null);
        System.out.println("get: " + first + " same=" + (first == second));
        System.out.println("calls(bsmObj)=" + objCalls);

        Method fail = gen.getMethod("fail");
        for (int i = 1; i <= 2; i++) {
            try {
                System.out.println("fail#" + i + ": returned " + fail.invoke(null));
            } catch (InvocationTargetException e) {
                Throwable t = e.getCause();
                System.out.println("fail#" + i + ": " + t.getClass().getName() + ": " + t.getMessage());
                if (i == 1 && t.getCause() != null) {
                    System.out.println("fail#1 cause: " + t.getCause().getClass().getName()
                            + ": " + t.getCause().getMessage());
                }
            }
        }
        System.out.println("calls(bsmThrow)=" + throwCalls);

        Object n = gen.getMethod("getInt").invoke(null);
        System.out.println("getInt: " + n + " name=" + seenName + " type=" + seenType);
    }
}
