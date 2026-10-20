// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 38, lane L6: a receiver-guarded (PGO or CHA) or
// exact-receiver inline splices the body the lenient walk finds, and the
// inlining screens never asked JVMS §5.4.6 selection whether that walk ends
// in an ERROR. Each row's `call` is invoked 30 000 times, so it is compiled
// with the erring receiver in its profile (the `-hot` rows) or behind a
// guard warmed on a good receiver (the `-cold` rows: 20 000 good calls, then
// the erring receiver).
//
//   abstract-redecl     `invokevirtual RdA.m` on `RdC extends abstract RdB
//                       { abstract m } extends RdA { m }` -> AbstractMethodError
//   conflicting-default `invokeinterface CdI1.m` on `CdC implements CdI1,
//                       CdI2`, both with a default `m` -> ICCE (conflicting
//                       default methods)
//   protected-iface     `invokeinterface PiI.m` on `PiD`, whose `m` is
//                       PROTECTED -> IllegalAccessError
//   final-abstract      `invokevirtual FaC.m` on `final FaC extends abstract
//                       FaB { abstract m } extends FaA { m }` (the constant-
//                       pool class is final: the optimizing tier splices by
//                       the constant pool, with no receiver) -> AbstractMethodError
//   exact-new           `new RdC().m()` (`invokevirtual RdC.m` on a receiver
//                       the optimizing tier proves exact) -> AbstractMethodError
//
// Only hand-assembled bytecode reaches these shapes (javac refuses a concrete
// class without an implementation, or with two inherited defaults).
//
// Run: javac -d out L6W38InlineSelectionErrorsHot.java
//      cratonvm --java-home <jdk25> [--nojit] -cp out L6W38InlineSelectionErrorsHot
// Positive control (see the lane report): CRATONVM_DBG_JITC=1 prints
//   `inline-resolve REFUSED ... receiver-selection-error`.
//
// Expected HotSpot 25 output (default and -Xint):
//   abstract-redecl-hot: ok=0 AbstractMethodError=30000 IncompatibleClassChangeError=0 IllegalAccessError=0 other=0
//   abstract-redecl-cold: ok=20000 AbstractMethodError=10000 IncompatibleClassChangeError=0 IllegalAccessError=0 other=0
//   conflicting-default-hot: ok=0 AbstractMethodError=0 IncompatibleClassChangeError=30000 IllegalAccessError=0 other=0
//   conflicting-default-cold: ok=20000 AbstractMethodError=0 IncompatibleClassChangeError=10000 IllegalAccessError=0 other=0
//   protected-iface-hot: ok=0 AbstractMethodError=0 IncompatibleClassChangeError=0 IllegalAccessError=30000 other=0
//   protected-iface-cold: ok=20000 AbstractMethodError=0 IncompatibleClassChangeError=0 IllegalAccessError=10000 other=0
//   final-abstract-hot: ok=0 AbstractMethodError=30000 IncompatibleClassChangeError=0 IllegalAccessError=0 other=0
//   exact-new-hot: ok=0 AbstractMethodError=30000 IncompatibleClassChangeError=0 IllegalAccessError=0 other=0
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

public class L6W38InlineSelectionErrorsHot {
    static final MethodTypeDesc INT = MethodTypeDesc.of(ConstantDescs.CD_int);
    static final MethodTypeDesc OBJ_INT = MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_Object);

    static final class Loader extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();

        Loader() {
            super(L6W38InlineSelectionErrorsHot.class.getClassLoader());
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

    static void ctor(ClassBuilder cb, ClassDesc superclass) {
        cb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC,
                code -> code.aload(0).invokespecial(superclass, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                        .return_());
    }

    /** A class: `flags`, superclass (null = Object), interfaces, a constructor, and `body`. */
    static byte[] cls(String name, int flags, String superName, String[] ifaces, Consumer<ClassBuilder> body) {
        ClassDesc sup = superName == null ? ConstantDescs.CD_Object : ClassDesc.of(superName);
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(flags | ClassFile.ACC_SUPER);
            cb.withSuperclass(sup);
            // One call: each `withInterfaceSymbols` replaces the list.
            ClassDesc[] is = new ClassDesc[ifaces.length];
            for (int i = 0; i < ifaces.length; i++) {
                is[i] = ClassDesc.of(ifaces[i]);
            }
            cb.withInterfaceSymbols(is);
            ctor(cb, sup);
            body.accept(cb);
        });
    }

    static byte[] iface(String name, Consumer<ClassBuilder> body) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_INTERFACE | ClassFile.ACC_ABSTRACT);
            body.accept(cb);
        });
    }

    static Consumer<ClassBuilder> m(int flags, int answer) {
        return cb -> cb.withMethodBody("m", INT, flags, code -> code.bipush(answer).ireturn());
    }

    static final Consumer<ClassBuilder> ABSTRACT_M =
            cb -> cb.withMethod("m", INT, ClassFile.ACC_PUBLIC | ClassFile.ACC_ABSTRACT, mb -> { });

    static final Consumer<ClassBuilder> NOTHING = cb -> { };

    /** `static int call(Object o) { return ((owner) o).m(); }` through `invokevirtual`/`invokeinterface`. */
    static byte[] caller(String name, String owner, boolean iface) {
        ClassDesc o = ClassDesc.of(owner);
        return cls(name, ClassFile.ACC_PUBLIC, null, new String[0], cb -> cb.withMethodBody("call", OBJ_INT,
                ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> {
                    code.aload(0).checkcast(o);
                    if (iface) {
                        code.invokeinterface(o, "m", INT);
                    } else {
                        code.invokevirtual(o, "m", INT);
                    }
                    code.ireturn();
                }));
    }

    /** `static int call(Object o) { return new exact().m(); }` */
    static byte[] newCaller(String name, String exact) {
        ClassDesc e = ClassDesc.of(exact);
        return cls(name, ClassFile.ACC_PUBLIC, null, new String[0], cb -> cb.withMethodBody("call", OBJ_INT,
                ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                code -> code.new_(e).dup().invokespecial(e, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                        .invokevirtual(e, "m", INT).ireturn()));
    }

    static String run(String label, MethodHandle call, Object good, Object bad, int want) {
        int ok = 0;
        int ame = 0;
        int icce = 0;
        int iae = 0;
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
            } catch (AbstractMethodError e) {
                ame++;
            } catch (IllegalAccessError e) {
                iae++;
            } catch (IncompatibleClassChangeError e) {
                icce++;
            } catch (Throwable t) {
                other++;
                if (firstOther == null) {
                    firstOther = t.toString();
                }
            }
        }
        return label + ": ok=" + ok + " AbstractMethodError=" + ame + " IncompatibleClassChangeError=" + icce
                + " IllegalAccessError=" + iae + " other=" + other + (firstOther == null ? "" : " first=" + firstOther);
    }

    static MethodHandle callOf(Loader l, String name) throws Exception {
        return MethodHandles.lookup().findStatic(l.loadClass(name), "call",
                MethodType.methodType(int.class, Object.class));
    }

    static final int PUB = ClassFile.ACC_PUBLIC;

    static Loader abstractRedecl() {
        Loader l = new Loader();
        l.bytes.put("RdA", cls("RdA", PUB, null, new String[0], m(PUB, 1)));
        l.bytes.put("RdB", cls("RdB", PUB | ClassFile.ACC_ABSTRACT, "RdA", new String[0], ABSTRACT_M));
        l.bytes.put("RdC", cls("RdC", PUB, "RdB", new String[0], NOTHING));
        l.bytes.put("RdCall", caller("RdCall", "RdA", false));
        l.bytes.put("RdNew", newCaller("RdNew", "RdC"));
        return l;
    }

    static String abstractRow(String label, boolean cold) throws Exception {
        Loader l = abstractRedecl();
        return run(label, callOf(l, "RdCall"), cold ? l.make("RdA") : null, l.make("RdC"), 1);
    }

    static String conflictRow(String label, boolean cold) throws Exception {
        Loader l = new Loader();
        l.bytes.put("CdI1", iface("CdI1", m(PUB, 1)));
        l.bytes.put("CdI2", iface("CdI2", m(PUB, 2)));
        l.bytes.put("CdC", cls("CdC", PUB, null, new String[] {"CdI1", "CdI2"}, NOTHING));
        l.bytes.put("CdGood", cls("CdGood", PUB, null, new String[] {"CdI1"}, NOTHING));
        l.bytes.put("CdCall", caller("CdCall", "CdI1", true));
        return run(label, callOf(l, "CdCall"), cold ? l.make("CdGood") : null, l.make("CdC"), 1);
    }

    static String protectedRow(String label, boolean cold) throws Exception {
        Loader l = new Loader();
        l.bytes.put("PiI", iface("PiI", ABSTRACT_M));
        l.bytes.put("PiD", cls("PiD", PUB, null, new String[] {"PiI"}, m(ClassFile.ACC_PROTECTED, 1)));
        l.bytes.put("PiGood", cls("PiGood", PUB, null, new String[] {"PiI"}, m(PUB, 1)));
        l.bytes.put("PiCall", caller("PiCall", "PiI", true));
        return run(label, callOf(l, "PiCall"), cold ? l.make("PiGood") : null, l.make("PiD"), 1);
    }

    static String finalRow(String label) throws Exception {
        Loader l = new Loader();
        l.bytes.put("FaA", cls("FaA", PUB, null, new String[0], m(PUB, 1)));
        l.bytes.put("FaB", cls("FaB", PUB | ClassFile.ACC_ABSTRACT, "FaA", new String[0], ABSTRACT_M));
        l.bytes.put("FaC", cls("FaC", PUB | ClassFile.ACC_FINAL, "FaB", new String[0], NOTHING));
        l.bytes.put("FaCall", caller("FaCall", "FaC", false));
        return run(label, callOf(l, "FaCall"), null, l.make("FaC"), 1);
    }

    static String exactNewRow(String label) throws Exception {
        Loader l = abstractRedecl();
        return run(label, callOf(l, "RdNew"), null, "unused", 1);
    }

    public static void main(String[] args) throws Exception {
        System.out.println(abstractRow("abstract-redecl-hot", false));
        System.out.println(abstractRow("abstract-redecl-cold", true));
        System.out.println(conflictRow("conflicting-default-hot", false));
        System.out.println(conflictRow("conflicting-default-cold", true));
        System.out.println(protectedRow("protected-iface-hot", false));
        System.out.println(protectedRow("protected-iface-cold", true));
        System.out.println(finalRow("final-abstract-hot"));
        System.out.println(exactNewRow("exact-new-hot"));
    }
}
