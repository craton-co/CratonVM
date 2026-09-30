/*
 * Interpreter round i1 wave 2, lane L6: linkage of an invokedynamic whose
 * bootstrap is NOT one of the JDK factories (JVMS 5.4.3.6 / 6.5). javac cannot
 * emit such a site, so this probe assembles a tiny class `GenIndy` by hand and
 * defines it with `Lookup.defineClass`. Its four static methods are:
 *
 *   a()  one indy -> bsmConst   (ConstantCallSite of constant 42)
 *   b()  TWO indy instructions sharing a()'s CONSTANT_InvokeDynamic entry
 *   c()  one indy -> bsmFail    (throws IllegalStateException("boom"))
 *   d()  one indy -> bsmMutable (MutableCallSite of constant 1)
 *
 * Stdout is deterministic. HotSpot 25 prints exactly:
 *
 *   const: 42 42 42 bootstraps=1
 *   shared-cp: 42 42 bootstraps=3
 *   fail#1: java.lang.BootstrapMethodError: bootstrap method initialization exception / cause=java.lang.IllegalStateException: boom
 *   fail#2: java.lang.BootstrapMethodError: bootstrap method initialization exception / cause=null
 *   fail: bootstraps=1 sameObject=false
 *   mutable: 1 1 bootstraps=1
 *   mutable after setTarget: 2 bootstraps=1
 *
 * Every instruction is linked ONCE; `b()`'s two instructions link separately
 * even though they share one constant-pool entry; a failed linkage is rethrown
 * as a NEW error of the same class and message and NO cause (HotSpot's
 * `save_and_throw_indy_exc` records class and message only) without
 * re-running the bootstrap; a `setTarget` is seen by the next execution.
 *
 * CratonVM before wave 2 re-ran the bootstrap on every execution:
 * const bootstraps=3, shared-cp bootstraps=7, fail bootstraps=2, mutable
 * bootstraps=2 and then 3 with `after setTarget: 1`.
 * CratonVM after wave 2, default `CRATONVM_INDY_CALLSITE_CACHE`: every line
 * matched HotSpot except the two `mutable` lines (a MutableCallSite was still
 * re-bootstrapped per execution by default). Since wave 6 the default caches
 * a MutableCallSite from any bootstrap but Groovy's, so EVERY line must match
 * in the default mode too, as with `CRATONVM_INDY_CALLSITE_CACHE=all`. With
 * `=0` the pre-wave-2 output must come back.
 */
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.lang.invoke.CallSite;
import java.lang.invoke.ConstantCallSite;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.MutableCallSite;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class GenericIndyLinkProbe {
    static int constBootstraps;
    static int failBootstraps;
    static int mutableBootstraps;
    static MutableCallSite lastMutable;

    public static CallSite bsmConst(MethodHandles.Lookup l, String name, MethodType type) {
        constBootstraps++;
        return new ConstantCallSite(MethodHandles.constant(int.class, 42));
    }

    public static CallSite bsmFail(MethodHandles.Lookup l, String name, MethodType type) {
        failBootstraps++;
        throw new IllegalStateException("boom");
    }

    public static CallSite bsmMutable(MethodHandles.Lookup l, String name, MethodType type) {
        mutableBootstraps++;
        lastMutable = new MutableCallSite(MethodHandles.constant(int.class, 1));
        return lastMutable;
    }

    public static void main(String[] args) throws Exception {
        Class<?> gen = MethodHandles.lookup().defineClass(genIndyBytes());
        Method a = gen.getMethod("a");
        Method b = gen.getMethod("b");
        Method c = gen.getMethod("c");
        Method d = gen.getMethod("d");

        StringBuilder sb = new StringBuilder("const:");
        for (int i = 0; i < 3; i++) {
            sb.append(' ').append(a.invoke(null));
        }
        System.out.println(sb.append(" bootstraps=").append(constBootstraps));

        sb = new StringBuilder("shared-cp:");
        for (int i = 0; i < 2; i++) {
            sb.append(' ').append(b.invoke(null));
        }
        System.out.println(sb.append(" bootstraps=").append(constBootstraps));

        Throwable first = failure(c, "fail#1");
        Throwable second = failure(c, "fail#2");
        System.out.println("fail: bootstraps=" + failBootstraps + " sameObject=" + (first == second));

        sb = new StringBuilder("mutable:");
        for (int i = 0; i < 2; i++) {
            sb.append(' ').append(d.invoke(null));
        }
        System.out.println(sb.append(" bootstraps=").append(mutableBootstraps));
        lastMutable.setTarget(MethodHandles.constant(int.class, 2));
        System.out.println("mutable after setTarget: " + d.invoke(null) + " bootstraps=" + mutableBootstraps);
    }

    static Throwable failure(Method m, String label) throws Exception {
        try {
            m.invoke(null);
            System.out.println(label + ": no exception");
            return null;
        } catch (InvocationTargetException ite) {
            Throwable t = ite.getCause();
            Throwable cause = t.getCause();
            System.out.println(label + ": " + t.getClass().getName() + ": " + t.getMessage()
                    + " / cause=" + (cause == null ? "null" : cause.getClass().getName() + ": " + cause.getMessage()));
            return t;
        }
    }

    // ---------------------------------------------------------------------
    // Class file assembly
    // ---------------------------------------------------------------------

    static final String BSM_DESC =
            "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/CallSite;";

    static byte[] genIndyBytes() throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(52);
        out.writeShort(34); // constant_pool_count: entries 1..33
        utf8(out, "GenIndy");                      // 1
        cls(out, 1);                               // 2
        utf8(out, "java/lang/Object");             // 3
        cls(out, 3);                               // 4
        utf8(out, "GenericIndyLinkProbe");         // 5
        cls(out, 5);                               // 6
        utf8(out, "bsmConst");                     // 7
        utf8(out, BSM_DESC);                       // 8
        nameAndType(out, 7, 8);                    // 9
        methodRef(out, 6, 9);                      // 10
        methodHandleStatic(out, 10);               // 11
        utf8(out, "bsmFail");                      // 12
        utf8(out, BSM_DESC);                       // 13
        nameAndType(out, 12, 13);                  // 14
        methodRef(out, 6, 14);                     // 15
        methodHandleStatic(out, 15);               // 16
        utf8(out, "bsmMutable");                   // 17
        utf8(out, BSM_DESC);                       // 18
        nameAndType(out, 17, 18);                  // 19
        methodRef(out, 6, 19);                     // 20
        methodHandleStatic(out, 20);               // 21
        utf8(out, "site");                         // 22
        utf8(out, "()I");                          // 23
        nameAndType(out, 22, 23);                  // 24
        invokeDynamic(out, 0, 24);                 // 25
        invokeDynamic(out, 1, 24);                 // 26
        invokeDynamic(out, 2, 24);                 // 27
        utf8(out, "a");                            // 28
        utf8(out, "b");                            // 29
        utf8(out, "c");                            // 30
        utf8(out, "d");                            // 31
        utf8(out, "Code");                         // 32
        utf8(out, "BootstrapMethods");             // 33
        out.writeShort(0x0021); // ACC_PUBLIC | ACC_SUPER
        out.writeShort(2);
        out.writeShort(4);
        out.writeShort(0); // interfaces
        out.writeShort(0); // fields
        out.writeShort(4); // methods
        byte[] indy25 = {(byte) 0xba, 0, 25, 0, 0};
        method(out, 28, concat(indy25, new byte[] {(byte) 0xac}));
        method(out, 29, concat(indy25, new byte[] {0x57}, indy25, new byte[] {(byte) 0xac}));
        method(out, 30, new byte[] {(byte) 0xba, 0, 26, 0, 0, (byte) 0xac});
        method(out, 31, new byte[] {(byte) 0xba, 0, 27, 0, 0, (byte) 0xac});
        out.writeShort(1); // class attributes
        out.writeShort(33);
        out.writeInt(2 + 3 * 4);
        out.writeShort(3);
        for (int handle : new int[] {11, 16, 21}) {
            out.writeShort(handle);
            out.writeShort(0);
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
        out.writeShort(23); // ()I
        out.writeShort(1);
        out.writeShort(32); // Code
        out.writeInt(12 + code.length);
        out.writeShort(1); // max_stack
        out.writeShort(0); // max_locals
        out.writeInt(code.length);
        out.write(code);
        out.writeShort(0); // exception table
        out.writeShort(0); // attributes
    }

    static byte[] concat(byte[]... parts) {
        ByteArrayOutputStream b = new ByteArrayOutputStream();
        for (byte[] p : parts) {
            b.write(p, 0, p.length);
        }
        return b.toByteArray();
    }
}
