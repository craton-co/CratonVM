/*
 * Interpreter round i1 wave 3, lane L6: static arguments of an `invokedynamic`
 * whose bootstrap is NOT one of the JDK factories (JVMS 4.7.23: any loadable
 * constant), and their fit to the bootstrap's declared parameters (the
 * `invokeWithArguments` rules `BootstrapMethodInvoker` applies). javac cannot
 * emit such sites, so this probe assembles a class `GenArgs` by hand and
 * defines it with `Lookup.defineClass`. Its five static `()I` methods are:
 *
 *   m1()  bsmHandle(Lookup, String, MethodType, MethodHandle) with a
 *         CONSTANT_MethodHandle static argument (-> seven())
 *   m2()  bsmObject(Lookup, String, MethodType, Object) with a CONSTANT_Integer
 *         41 (must arrive BOXED, as an Integer)
 *   m3()  bsmVarargs(Lookup, String, MethodType, Object...) with ONE String
 *         static argument (must arrive as a one-element Object[])
 *   m4()  bsmObject with a CONSTANT_Dynamic static argument whose bootstrap
 *         condyBsm returns Integer 99
 *   m5()  bsmHandle with a CONSTANT_MethodHandle to a method that does not
 *         exist (a LinkageError, passed through unwrapped and recorded)
 *
 * Stdout is deterministic. HotSpot 25 prints exactly:
 *
 *   handle: 7
 *   boxed: 42
 *   varargs: 11
 *   condy: 100
 *   missing#1: java.lang.NoSuchMethodError
 *   missing#2: java.lang.NoSuchMethodError
 *
 * CratonVM before wave 3 died at m1 with an uncatchable VM-internal
 * "unsupported bootstrap static arg kind" error (MethodHandle and
 * CONSTANT_Dynamic static arguments were refused), passed the Integer of m2
 * as a raw `int` into an `Object` parameter, and passed m3's single String as
 * a bare scalar into the `Object[]` parameter. Through wave 3 the two
 * `missing#` lines printed `java.lang.NoSuchMethodException`: the compatible
 * native `Lookup.findStatic` reports its miss as an unmaterialised
 * `VmError::Runtime`, which `constants::map_lookup_exception_to_error` did not
 * map (fixed in wave 4).
 */
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.lang.invoke.CallSite;
import java.lang.invoke.ConstantCallSite;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class GenericIndyStaticArgsProbe {
    public static int seven() {
        return 7;
    }

    public static CallSite bsmHandle(MethodHandles.Lookup l, String name, MethodType type, MethodHandle mh) {
        return new ConstantCallSite(mh);
    }

    public static CallSite bsmObject(MethodHandles.Lookup l, String name, MethodType type, Object o) {
        int v = (o instanceof Integer i) ? i + 1 : -1;
        return new ConstantCallSite(MethodHandles.constant(int.class, v));
    }

    public static CallSite bsmVarargs(MethodHandles.Lookup l, String name, MethodType type, Object... args) {
        int v = args.length * 10 + (args.length > 0 && args[0] instanceof String ? 1 : 0);
        return new ConstantCallSite(MethodHandles.constant(int.class, v));
    }

    public static Object condyBsm(MethodHandles.Lookup l, String name, Class<?> type) {
        return Integer.valueOf(99);
    }

    public static void main(String[] args) throws Exception {
        Class<?> gen = MethodHandles.lookup().defineClass(genArgsBytes());
        System.out.println("handle: " + gen.getMethod("m1").invoke(null));
        System.out.println("boxed: " + gen.getMethod("m2").invoke(null));
        System.out.println("varargs: " + gen.getMethod("m3").invoke(null));
        System.out.println("condy: " + gen.getMethod("m4").invoke(null));
        Method m5 = gen.getMethod("m5");
        failure(m5, "missing#1");
        failure(m5, "missing#2");
    }

    static void failure(Method m, String label) throws Exception {
        try {
            System.out.println(label + ": no exception, " + m.invoke(null));
        } catch (InvocationTargetException ite) {
            System.out.println(label + ": " + ite.getCause().getClass().getName());
        }
    }

    // ---------------------------------------------------------------------
    // Class file assembly
    // ---------------------------------------------------------------------

    static final String LOOKUP_PREFIX =
            "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/invoke/MethodType;";
    static final String CALL_SITE = ")Ljava/lang/invoke/CallSite;";

    static byte[] genArgsBytes() throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(55); // CONSTANT_Dynamic needs class file version 55
        out.writeShort(57); // constant_pool_count: entries 1..56
        utf8(out, "GenArgs");                                                 // 1
        cls(out, 1);                                                          // 2
        utf8(out, "java/lang/Object");                                        // 3
        cls(out, 3);                                                          // 4
        utf8(out, "GenericIndyStaticArgsProbe");                              // 5
        cls(out, 5);                                                          // 6
        utf8(out, "bsmHandle");                                               // 7
        utf8(out, LOOKUP_PREFIX + "Ljava/lang/invoke/MethodHandle;" + CALL_SITE); // 8
        nameAndType(out, 7, 8);                                               // 9
        methodRef(out, 6, 9);                                                 // 10
        methodHandleStatic(out, 10);                                          // 11
        utf8(out, "bsmObject");                                               // 12
        utf8(out, LOOKUP_PREFIX + "Ljava/lang/Object;" + CALL_SITE);          // 13
        nameAndType(out, 12, 13);                                             // 14
        methodRef(out, 6, 14);                                                // 15
        methodHandleStatic(out, 15);                                          // 16
        utf8(out, "bsmVarargs");                                              // 17
        utf8(out, LOOKUP_PREFIX + "[Ljava/lang/Object;" + CALL_SITE);         // 18
        nameAndType(out, 17, 18);                                             // 19
        methodRef(out, 6, 19);                                                // 20
        methodHandleStatic(out, 20);                                          // 21
        utf8(out, "condyBsm");                                                // 22
        utf8(out, "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/Object;"); // 23
        nameAndType(out, 22, 23);                                             // 24
        methodRef(out, 6, 24);                                                // 25
        methodHandleStatic(out, 25);                                          // 26
        utf8(out, "seven");                                                   // 27
        utf8(out, "()I");                                                     // 28
        nameAndType(out, 27, 28);                                             // 29
        methodRef(out, 6, 29);                                                // 30
        methodHandleStatic(out, 30);                                          // 31
        utf8(out, "missing");                                                 // 32
        nameAndType(out, 32, 28);                                             // 33
        methodRef(out, 6, 33);                                                // 34
        methodHandleStatic(out, 34);                                          // 35
        out.writeByte(3);                                                     // 36 Integer
        out.writeInt(41);
        utf8(out, "x");                                                       // 37
        out.writeByte(8);                                                     // 38 String
        out.writeShort(37);
        utf8(out, "c");                                                       // 39
        utf8(out, "Ljava/lang/Object;");                                      // 40
        nameAndType(out, 39, 40);                                             // 41
        out.writeByte(17);                                                    // 42 Dynamic
        out.writeShort(3);
        out.writeShort(41);
        utf8(out, "site");                                                    // 43
        nameAndType(out, 43, 28);                                             // 44
        invokeDynamic(out, 0, 44);                                            // 45
        invokeDynamic(out, 1, 44);                                            // 46
        invokeDynamic(out, 2, 44);                                            // 47
        invokeDynamic(out, 4, 44);                                            // 48
        invokeDynamic(out, 5, 44);                                            // 49
        utf8(out, "m1");                                                      // 50
        utf8(out, "m2");                                                      // 51
        utf8(out, "m3");                                                      // 52
        utf8(out, "m4");                                                      // 53
        utf8(out, "m5");                                                      // 54
        utf8(out, "Code");                                                    // 55
        utf8(out, "BootstrapMethods");                                        // 56
        out.writeShort(0x0021); // ACC_PUBLIC | ACC_SUPER
        out.writeShort(2);
        out.writeShort(4);
        out.writeShort(0); // interfaces
        out.writeShort(0); // fields
        out.writeShort(5); // methods
        for (int i = 0; i < 5; i++) {
            method(out, 50 + i, new byte[] {(byte) 0xba, 0, (byte) (45 + i), 0, 0, (byte) 0xac});
        }
        // BootstrapMethods: {handle, args...}
        int[][] bsms = {
            {11, 31}, // 0: bsmHandle(seven)
            {16, 36}, // 1: bsmObject(41)
            {21, 38}, // 2: bsmVarargs("x")
            {26},     // 3: condyBsm()
            {16, 42}, // 4: bsmObject(condy)
            {11, 35}, // 5: bsmHandle(missing)
        };
        int length = 2;
        for (int[] b : bsms) {
            length += 4 + 2 * (b.length - 1);
        }
        out.writeShort(1); // class attributes
        out.writeShort(56);
        out.writeInt(length);
        out.writeShort(bsms.length);
        for (int[] b : bsms) {
            out.writeShort(b[0]);
            out.writeShort(b.length - 1);
            for (int i = 1; i < b.length; i++) {
                out.writeShort(b[i]);
            }
        }
        out.flush();
        return bytes.toByteArray();
    }

    static void utf8(DataOutputStream out, String s) throws IOException {
        out.writeByte(1);
        out.writeUTF(s);
    }

    static void cls(DataOutputStream out, int nameIndex) throws IOException {
        out.writeByte(7);
        out.writeShort(nameIndex);
    }

    static void nameAndType(DataOutputStream out, int name, int desc) throws IOException {
        out.writeByte(12);
        out.writeShort(name);
        out.writeShort(desc);
    }

    static void methodRef(DataOutputStream out, int cls, int nat) throws IOException {
        out.writeByte(10);
        out.writeShort(cls);
        out.writeShort(nat);
    }

    static void methodHandleStatic(DataOutputStream out, int ref) throws IOException {
        out.writeByte(15);
        out.writeByte(6); // REF_invokeStatic
        out.writeShort(ref);
    }

    static void invokeDynamic(DataOutputStream out, int bsm, int nat) throws IOException {
        out.writeByte(18);
        out.writeShort(bsm);
        out.writeShort(nat);
    }

    static void method(DataOutputStream out, int name, byte[] code) throws IOException {
        out.writeShort(0x0009); // ACC_PUBLIC | ACC_STATIC
        out.writeShort(name);
        out.writeShort(28); // ()I
        out.writeShort(1);
        out.writeShort(55); // Code
        out.writeInt(12 + code.length);
        out.writeShort(1); // max_stack
        out.writeShort(0); // max_locals
        out.writeInt(code.length);
        out.write(code);
        out.writeShort(0); // exception table
        out.writeShort(0); // attributes
    }
}
