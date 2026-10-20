// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 38, lane L6: JVMS §6.5 `invokeinterface` — "if
// the class of objectref does not implement the resolved interface,
// invokeinterface throws an IncompatibleClassChangeError" — on COMPILED call
// sites. Wave 30 added the check to the interpreter's slow path; the JIT's
// dispatch helpers (`jit_invoke_dispatch`, `jit_invoke_virtual_mic`) never
// asked it, so a compiled `invokeinterface` ran the receiver's same-named
// public method, or the interface's default method (the by-name route and its
// NoSuchMethodError rescue), on a receiver that does not implement the
// interface. Each row's `call` is invoked 30 000 times; the `-cold` rows pass
// a receiver that implements the interface for the first 20 000 calls and
// the erring one after, so the erring receiver meets only compiled code.
//
//   public-method   `invokeinterface NiI.m` on `NiX`, which does not
//                   implement NiI but declares a public `m`
//   default-method  `invokeinterface NdI.m` (NdI has a default `m`) on `NdX`,
//                   which does not implement NdI and has no `m`
//   object          `invokeinterface NdI.m` on a plain `java.lang.Object`
//
// The verifier treats an interface type as `Object`, so javac output always
// passes a receiver of the interface type; only hand-assembled bytecode (no
// `checkcast` before the call) reaches these shapes.
//
// Run: javac -d out L6W38InterfaceReceiverHot.java
//      cratonvm --java-home <jdk25> [--nojit] -cp out L6W38InterfaceReceiverHot
// Positive control (see the lane report): CRATONVM_DBG_JITC=1 prints
//   `[cratonvm-jit] invokeinterface receiver does not implement ...`.
//
// Expected HotSpot 25 output (default and -Xint):
//   public-method-hot: ok=0 IncompatibleClassChangeError=30000 other=0
//   public-method-cold: ok=20000 IncompatibleClassChangeError=10000 other=0
//   default-method-hot: ok=0 IncompatibleClassChangeError=30000 other=0
//   default-method-cold: ok=20000 IncompatibleClassChangeError=10000 other=0
//   object-hot: ok=0 IncompatibleClassChangeError=30000 other=0
//   object-cold: ok=20000 IncompatibleClassChangeError=10000 other=0
//   message: Class NiX does not implement the requested interface NiI
//
// `--compatible` keeps its duck typing on compiled sites by design (the fix
// is `--jdk-only`); its rows are not compared.
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

public class L6W38InterfaceReceiverHot {
    static final MethodTypeDesc INT = MethodTypeDesc.of(ConstantDescs.CD_int);
    static final MethodTypeDesc OBJ_INT = MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_Object);
    static final int PUB = ClassFile.ACC_PUBLIC;

    static final class Loader extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();

        Loader() {
            super(L6W38InterfaceReceiverHot.class.getClassLoader());
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

    static byte[] iface(String name, Integer defaultAnswer) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(PUB | ClassFile.ACC_INTERFACE | ClassFile.ACC_ABSTRACT);
            if (defaultAnswer == null) {
                cb.withMethod("m", INT, PUB | ClassFile.ACC_ABSTRACT, mb -> { });
            } else {
                int a = defaultAnswer;
                cb.withMethodBody("m", INT, PUB, code -> code.bipush(a).ireturn());
            }
        });
    }

    static Consumer<ClassBuilder> m(int answer) {
        return cb -> cb.withMethodBody("m", INT, PUB, code -> code.bipush(answer).ireturn());
    }

    /** `static int call(Object o) { aload_0; invokeinterface iface.m; ireturn }` — no checkcast. */
    static byte[] caller(String name, String iface) {
        ClassDesc i = ClassDesc.of(iface);
        return cls(name, null, cb -> cb.withMethodBody("call", OBJ_INT, PUB | ClassFile.ACC_STATIC,
                code -> code.aload(0).invokeinterface(i, "m", INT).ireturn()));
    }

    static String firstIcce;

    static String run(String label, MethodHandle call, Object good, Object bad, int want) {
        int ok = 0;
        int icce = 0;
        int other = 0;
        String firstOther = null;
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

    static String publicRow(String label, boolean cold) throws Exception {
        Loader l = new Loader();
        l.bytes.put("NiI", iface("NiI", null));
        l.bytes.put("NiX", cls("NiX", null, m(7)));
        l.bytes.put("NiGood", cls("NiGood", "NiI", m(1)));
        l.bytes.put("NiCall", caller("NiCall", "NiI"));
        return run(label, callOf(l, "NiCall"), cold ? l.make("NiGood") : null, l.make("NiX"), 1);
    }

    static String defaultRow(String label, boolean cold) throws Exception {
        Loader l = new Loader();
        l.bytes.put("NdI", iface("NdI", 3));
        l.bytes.put("NdX", cls("NdX", null, cb -> { }));
        l.bytes.put("NdGood", cls("NdGood", "NdI", cb -> { }));
        l.bytes.put("NdCall", caller("NdCall", "NdI"));
        return run(label, callOf(l, "NdCall"), cold ? l.make("NdGood") : null, l.make("NdX"), 3);
    }

    static String objectRow(String label, boolean cold) throws Exception {
        Loader l = new Loader();
        l.bytes.put("NoI", iface("NoI", 3));
        l.bytes.put("NoGood", cls("NoGood", "NoI", cb -> { }));
        l.bytes.put("NoCall", caller("NoCall", "NoI"));
        return run(label, callOf(l, "NoCall"), cold ? l.make("NoGood") : null, new Object(), 3);
    }

    public static void main(String[] args) throws Exception {
        String first = publicRow("public-method-hot", false);
        String firstMessage = firstIcce;
        System.out.println(first);
        System.out.println(publicRow("public-method-cold", true));
        System.out.println(defaultRow("default-method-hot", false));
        System.out.println(defaultRow("default-method-cold", true));
        System.out.println(objectRow("object-hot", false));
        System.out.println(objectRow("object-cold", true));
        System.out.println("message: " + firstMessage);
    }
}
