// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i6 wave 117, lane L4 (`CRATONVM_INDY_GENERIC_DIRECT_INVOKE`,
// default off; stage 2 of
// docs/internal/proposals/interpreter/i2-L6-proposal-generic-indy-invoke-path-20260923.md):
// a linked GENERIC `invokedynamic` (a bootstrap that is not a JDK factory, the
// shape of every Groovy / JRuby call site) executed in a loop. javac cannot
// emit such a site, so the probe assembles `GenIndyBench` (two instance
// methods, each one `invokedynamic (Object,int,boolean)Object`, the trailing
// `Z` as Groovy's flags) and defines it with `Lookup.defineClass`.
//
// Sites: `mutable` (a MutableCallSite over a static of the site's own type),
// `converted` (a ConstantCallSite over `asType` of a (String,long,Boolean)
// handle: cast, widening, boxing) and `retarget` (the mutable site after
// `setTarget`). Behaviour lines are HotSpot's in every arm, both modes, JIT
// and --nojit; timing lines start with "TIME ".
//
// Run: javac -d . L4W117GenericIndyBench.java
//      java -cp . L4W117GenericIndyBench [iters]                                   (HotSpot)
//      cratonvm --jdk-only|--compatible [--nojit] -cp . L4W117GenericIndyBench [iters]
//      CRATONVM_INDY_GENERIC_DIRECT_INVOKE=1 cratonvm ... L4W117GenericIndyBench [iters]
//      (interleave the arms, take medians)
//
// (the two `-sum` lines print `iters`: compare with the default 200000)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags; TIME lines removed)
// mutable x 7 bootstraps=1
// converted s:7:true s:8:false
// mutable-sum 200000 nulls=0
// converted-sum 200000
// retarget other:3 bootstraps=1
// null-converted null:1:true
// cce java.lang.ClassCastException
// HOTSPOT_EXPECTED_END
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.lang.invoke.CallSite;
import java.lang.invoke.ConstantCallSite;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.MutableCallSite;
import java.util.Arrays;

public class L4W117GenericIndyBench {
    public interface Shape {
        Object call(Object o, int i, boolean b);

        Object callConverted(Object o, int i, boolean b);
    }

    static int bootstraps;
    static MutableCallSite mutable;

    public static Object target(Object o, int i, boolean b) {
        return b ? o : (Object) Integer.valueOf(i);
    }

    public static Object other(Object o, int i, boolean b) {
        return "other:" + i;
    }

    public static String conv(String s, long l, Boolean b) {
        return s + ":" + l + ":" + b;
    }

    public static CallSite bsmMutable(MethodHandles.Lookup l, String name, MethodType type)
            throws ReflectiveOperationException {
        bootstraps++;
        mutable = new MutableCallSite(l.findStatic(L4W117GenericIndyBench.class, "target", type));
        return mutable;
    }

    public static CallSite bsmConvert(MethodHandles.Lookup l, String name, MethodType type)
            throws ReflectiveOperationException {
        MethodHandle h = l.findStatic(L4W117GenericIndyBench.class, "conv",
                MethodType.methodType(String.class, String.class, long.class, Boolean.class));
        return new ConstantCallSite(h.asType(type));
    }

    public static void main(String[] args) throws Throwable {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 200_000;
        Class<?> gen = MethodHandles.lookup().defineClass(genBytes());
        Shape s = (Shape) gen.getConstructor().newInstance();

        System.out.println("mutable " + s.call("x", 1, true) + " " + s.call("x", 7, false)
                + " bootstraps=" + bootstraps);
        System.out.println("converted " + s.callConverted("s", 7, true) + " " + s.callConverted("s", 8, false));

        long[] mt = new long[5];
        long[] ct = new long[5];
        long msum = 0;
        long csum = 0;
        int nulls = 0;
        for (int rep = 0; rep < 5; rep++) {
            long t0 = System.nanoTime();
            msum = 0;
            for (int i = 0; i < iters; i++) {
                Object r = s.call("y", i, (i & 1) == 0);
                if (r == null) {
                    nulls++;
                }
                msum++;
            }
            mt[rep] = System.nanoTime() - t0;
            t0 = System.nanoTime();
            csum = 0;
            for (int i = 0; i < iters; i++) {
                Object r = s.callConverted("c", i & 7, (i & 1) == 0);
                csum += r != null ? 1 : 0;
            }
            ct[rep] = System.nanoTime() - t0;
        }
        System.out.println("mutable-sum " + msum + " nulls=" + nulls);
        System.out.println("converted-sum " + csum);
        Arrays.sort(mt);
        Arrays.sort(ct);
        System.out.println("TIME mutable med " + (mt[2] / iters) + " ns/op");
        System.out.println("TIME converted med " + (ct[2] / iters) + " ns/op");

        mutable.setTarget(MethodHandles.lookup().findStatic(L4W117GenericIndyBench.class, "other",
                MethodType.methodType(Object.class, Object.class, int.class, boolean.class)));
        System.out.println("retarget " + s.call("z", 3, true) + " bootstraps=" + bootstraps);
        System.out.println("null-converted " + s.callConverted(null, 1, true));
        try {
            System.out.println("cce none " + s.callConverted(Integer.valueOf(1), 1, true));
        } catch (ClassCastException e) {
            System.out.println("cce " + e.getClass().getName());
        }
    }

    // ---------------------------------------------------------------------
    // Class file assembly (version 52: no branches, so no StackMapTable)
    // ---------------------------------------------------------------------

    static final String BSM_DESC =
            "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/CallSite;";

    static byte[] genBytes() throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(52);
        out.writeShort(31); // constant_pool_count: entries 1..30
        utf8(out, "GenIndyBench");                                // 1
        cls(out, 1);                                              // 2
        utf8(out, "java/lang/Object");                            // 3
        cls(out, 3);                                              // 4
        utf8(out, "L4W117GenericIndyBench$Shape");                // 5
        cls(out, 5);                                              // 6
        utf8(out, "L4W117GenericIndyBench");                      // 7
        cls(out, 7);                                              // 8
        utf8(out, "bsmMutable");                                  // 9
        utf8(out, BSM_DESC);                                      // 10
        nameAndType(out, 9, 10);                                  // 11
        methodRef(out, 8, 11);                                    // 12
        methodHandleStatic(out, 12);                              // 13
        utf8(out, "bsmConvert");                                  // 14
        nameAndType(out, 14, 10);                                 // 15
        methodRef(out, 8, 15);                                    // 16
        methodHandleStatic(out, 16);                              // 17
        utf8(out, "site");                                        // 18
        utf8(out, "(Ljava/lang/Object;IZ)Ljava/lang/Object;");    // 19
        nameAndType(out, 18, 19);                                 // 20
        invokeDynamic(out, 0, 20);                                // 21
        invokeDynamic(out, 1, 20);                                // 22
        utf8(out, "<init>");                                      // 23
        utf8(out, "()V");                                         // 24
        nameAndType(out, 23, 24);                                 // 25
        methodRef(out, 4, 25);                                    // 26
        utf8(out, "call");                                        // 27
        utf8(out, "callConverted");                               // 28
        utf8(out, "Code");                                        // 29
        utf8(out, "BootstrapMethods");                            // 30
        out.writeShort(0x0021); // ACC_PUBLIC | ACC_SUPER
        out.writeShort(2);
        out.writeShort(4);
        out.writeShort(1); // interfaces
        out.writeShort(6);
        out.writeShort(0); // fields
        out.writeShort(3); // methods
        // public <init>()V { aload_0; invokespecial Object.<init>; return }
        method(out, 23, 24, 1, 1, new byte[] {0x2a, (byte) 0xb7, 0, 26, (byte) 0xb1});
        // public Object call(Object, int, boolean) { aload_1; iload_2; iload_3; indy; areturn }
        method(out, 27, 19, 3, 4, new byte[] {0x2b, 0x1c, 0x1d, (byte) 0xba, 0, 21, 0, 0, (byte) 0xb0});
        method(out, 28, 19, 3, 4, new byte[] {0x2b, 0x1c, 0x1d, (byte) 0xba, 0, 22, 0, 0, (byte) 0xb0});
        out.writeShort(1); // class attributes
        out.writeShort(30);
        out.writeInt(2 + 2 * 4);
        out.writeShort(2);
        for (int handle : new int[] {13, 17}) {
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

    static void method(DataOutputStream out, int name, int desc, int maxStack, int maxLocals, byte[] code)
            throws IOException {
        out.writeShort(0x0001); // ACC_PUBLIC
        out.writeShort(name);
        out.writeShort(desc);
        out.writeShort(1);
        out.writeShort(29); // Code
        out.writeInt(12 + code.length);
        out.writeShort(maxStack);
        out.writeShort(maxLocals);
        out.writeInt(code.length);
        out.write(code);
        out.writeShort(0); // exception table
        out.writeShort(0); // attributes
    }
}
