// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 39, lane L2: item 1 of
// `docs/internal/fixed-bugs/interpreter-L6-compiled-dispatch-leftovers-FIXED-20261004.md`.
// JVMS §6.5 `invokeinterface`: "if the class of objectref does not implement
// the resolved interface, invokeinterface throws an
// IncompatibleClassChangeError". An array class implements only Cloneable and
// Serializable (JLS 10.8), so an `invokeinterface AI.m` on an `int[]` raises
// the ICCE. Every CratonVM door dispatched an array receiver on
// java/lang/Object and set its header's class id (the COMPONENT's) aside, so
// the class check wave 30 (interpreter) and wave 38 (compiled) added never
// ran for one, and the interface's DEFAULT method ran on the array instead.
// Each row's `call` is invoked 30 000 times; the `-cold` rows pass a receiver
// that implements the interface for the first 20 000 calls and the array
// after, so the array meets only compiled code.
//
//   int-array       `invokeinterface AI.m` (AI has a default `m`) on int[]
//   string-array    the same on String[]
//   redeclared      `invokeinterface NtI.toString` (NtI redeclares
//                   `String toString()`) on int[]
//
// The verifier treats an interface type as `Object`, so javac output always
// passes a receiver of the interface type; only hand-assembled bytecode (no
// `checkcast` before the call) reaches these shapes.
//
// Run: javac -d out L2W39ArrayInterfaceReceiver.java
//      cratonvm [--nojit] -cp out L2W39ArrayInterfaceReceiver
//
// Expected HotSpot 25 output (default and -Xint):
//   int-array-hot: ok=0 IncompatibleClassChangeError=30000 other=0
//   int-array-cold: ok=20000 IncompatibleClassChangeError=10000 other=0
//   string-array-hot: ok=0 IncompatibleClassChangeError=30000 other=0
//   string-array-cold: ok=20000 IncompatibleClassChangeError=10000 other=0
//   redeclared-hot: ok=0 IncompatibleClassChangeError=30000 other=0
//   message int[]: Class [I does not implement the requested interface AI
//   message String[]: Class [Ljava.lang.String; does not implement the requested interface AI
//
// Fixed under `--jdk-only` (the default) only; `--compatible` keeps running
// the default method on an array (`ok=` rows), as it keeps duck typing on
// compiled class receivers (`i37-L2-compiled-invokeinterface-selection-edges`).
import java.lang.classfile.ClassBuilder;
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.HashMap;
import java.util.Map;
import java.util.function.Consumer;

public class L2W39ArrayInterfaceReceiver {
    static final MethodTypeDesc INT = MethodTypeDesc.of(ConstantDescs.CD_int);
    static final MethodTypeDesc STR = MethodTypeDesc.of(ConstantDescs.CD_String);
    static final MethodTypeDesc OBJ_INT = MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_Object);
    static final int PUB = ClassFile.ACC_PUBLIC;

    static final class Loader extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();

        Loader() {
            super(L2W39ArrayInterfaceReceiver.class.getClassLoader());
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            byte[] b = bytes.get(name);
            if (b == null) {
                throw new ClassNotFoundException(name);
            }
            return defineClass(name, b, 0, b.length);
        }

        Object make(String name) throws Exception {
            return loadClass(name).getConstructor().newInstance();
        }
    }

    static byte[] cls(String name, String iface, Consumer<ClassBuilder> body) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(PUB | ClassFile.ACC_SUPER);
            if (iface != null) {
                cb.withInterfaceSymbols(ClassDesc.of(iface));
            }
            cb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, PUB,
                    code -> code.aload(0).invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                            ConstantDescs.MTD_void).return_());
            body.accept(cb);
        });
    }

    /** `interface AI { default int m() { return 3; } }` */
    static byte[] defaultIface(String name) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(PUB | ClassFile.ACC_INTERFACE | ClassFile.ACC_ABSTRACT);
            cb.withMethodBody("m", INT, PUB, code -> code.iconst_3().ireturn());
        });
    }

    /** `interface NtI { String toString(); }` */
    static byte[] redeclaringIface(String name) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(PUB | ClassFile.ACC_INTERFACE | ClassFile.ACC_ABSTRACT);
            cb.withMethod("toString", STR, PUB | ClassFile.ACC_ABSTRACT, mb -> { });
        });
    }

    /** `static int call(Object o) { aload_0; invokeinterface AI.m; ireturn }` — no checkcast. */
    static byte[] caller(String name, String iface) {
        ClassDesc i = ClassDesc.of(iface);
        return cls(name, null, cb -> cb.withMethodBody("call", OBJ_INT, PUB | ClassFile.ACC_STATIC,
                code -> code.aload(0).invokeinterface(i, "m", INT).ireturn()));
    }

    /**
     * `static int call(Object o) { aload_0; invokeinterface NtI.toString; pop; iconst_3; ireturn }`.
     */
    static byte[] toStringCaller(String name, String iface) {
        ClassDesc i = ClassDesc.of(iface);
        return cls(name, null, cb -> cb.withMethodBody("call", OBJ_INT, PUB | ClassFile.ACC_STATIC,
                code -> code.aload(0).invokeinterface(i, "toString", STR).pop().iconst_3().ireturn()));
    }

    static String firstIcce;

    static String run(String label, MethodHandle call, Object good, Object bad, int want) {
        int ok = 0;
        int icce = 0;
        int other = 0;
        String firstOther = null;
        firstIcce = null;
        for (int i = 0; i < 30_000; i++) {
            Object recv = good != null && i < 20_000 ? good : bad;
            try {
                int r = (int) call.invokeExact(recv);
                if (r == want) {
                    ok++;
                } else {
                    other++;
                    if (firstOther == null) {
                        firstOther = "returned " + r;
                    }
                }
            } catch (IncompatibleClassChangeError e) {
                if (e.getClass() == IncompatibleClassChangeError.class) {
                    icce++;
                    if (firstIcce == null) {
                        firstIcce = e.getMessage();
                    }
                } else {
                    other++;
                    if (firstOther == null) {
                        firstOther = e.toString();
                    }
                }
            } catch (Throwable t) {
                other++;
                if (firstOther == null) {
                    firstOther = t.toString();
                }
            }
        }
        return label + ": ok=" + ok + " IncompatibleClassChangeError=" + icce + " other=" + other
                + (firstOther == null ? "" : " first=" + firstOther);
    }

    static MethodHandle callOf(Loader l, String name) throws Exception {
        return MethodHandles.lookup().findStatic(l.loadClass(name), "call",
                MethodType.methodType(int.class, Object.class));
    }

    static String defaultRow(String label, boolean cold, Object array) throws Exception {
        Loader l = new Loader();
        l.bytes.put("AI", defaultIface("AI"));
        l.bytes.put("AGood", cls("AGood", "AI", cb -> { }));
        l.bytes.put("ACall", caller("ACall", "AI"));
        return run(label, callOf(l, "ACall"), cold ? l.make("AGood") : null, array, 3);
    }

    static String redeclaredRow(String label) throws Exception {
        Loader l = new Loader();
        l.bytes.put("NtI", redeclaringIface("NtI"));
        l.bytes.put("NtCall", toStringCaller("NtCall", "NtI"));
        return run(label, callOf(l, "NtCall"), null, new int[1], 3);
    }

    public static void main(String[] args) throws Exception {
        System.out.println(defaultRow("int-array-hot", false, new int[1]));
        String intMessage = firstIcce;
        System.out.println(defaultRow("int-array-cold", true, new int[1]));
        System.out.println(defaultRow("string-array-hot", false, new String[1]));
        String stringMessage = firstIcce;
        System.out.println(defaultRow("string-array-cold", true, new String[1]));
        System.out.println(redeclaredRow("redeclared-hot"));
        System.out.println("message int[]: " + intMessage);
        System.out.println("message String[]: " + stringMessage);
    }
}
