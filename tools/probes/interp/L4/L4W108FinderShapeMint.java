// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 108, lane L4: semantics of
// `CRATONVM_MH_METHOD_FINDER_SHAPE_MINT` (default off). `findStatic` /
// `findConstructor` over a run of freshly defined classes of one shape (the
// switch mints each answer from the previous class's), then classes that
// differ from the filed shape in exactly one input of the JDK body's verdicts
// (package, class flags, member flags, private / protected member, missing
// member, initialization state, constructor arity, another loader). Every
// line must be the same with the switch off and on, and equal HotSpot's.
//
// Run: javac -d out L4W108FinderShapeMint.java
//      java -cp out L4W108FinderShapeMint                                  (HotSpot)
//      cratonvm --jdk-only -cp out L4W108FinderShapeMint
//      CRATONVM_MH_METHOD_FINDER_SHAPE_MINT=1 cratonvm --jdk-only -cp out L4W108FinderShapeMint
//      CRATONVM_MH_METHOD_FINDER_SHAPE_MINT=1 cratonvm --jdk-only --nojit -cp out L4W108FinderShapeMint
//      cratonvm --compatible -cp out L4W108FinderShapeMint
//      (the memo and so the switch exist only under --jdk-only; the
//      --compatible run checks the bridged finders' refusal texts against
//      the same HotSpot lines)
//      (with CRATONVM_DBG=mh-finder-census the switch-on run should count
//      `method-finder.shape.served` on the run rows and a `declined` reason on
//      each odd row)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, by reading the JDK source; the battery compares live)
// run-ctor S0 type ()S0 interned true instance true reveal S0.<init> newInvokeSpecial ()void distinct true
// run-static S0 7 type ()int reveal S0.seven invokeStatic ()int
// run-ctor S1 type ()S1 interned true instance true reveal S1.<init> newInvokeSpecial ()void distinct true
// run-static S1 7 type ()int reveal S1.seven invokeStatic ()int
// run-ctor S2 type ()S2 interned true instance true reveal S2.<init> newInvokeSpecial ()void distinct true
// run-static S2 7 type ()int reveal S2.seven invokeStatic ()int
// run-ctor S3 type ()S3 interned true instance true reveal S3.<init> newInvokeSpecial ()void distinct true
// run-static S3 7 type ()int reveal S3.seven invokeStatic ()int
// run-ctor S4 type ()S4 interned true instance true reveal S4.<init> newInvokeSpecial ()void distinct true
// run-static S4 7 type ()int reveal S4.seven invokeStatic ()int
// other-package p/S5 ctor ()S5 static 7
// synchronized-static S6 7 modifiers 41
// final-class S7 ctor ()S7 static 7
// private-ctor S8: java.lang.IllegalAccessException: no such constructor: S8.<init>()void/newInvokeSpecial
// protected-static S9: java.lang.IllegalAccessException: no such method: S9.seven()int/invokeStatic
// missing-static S10: java.lang.NoSuchMethodException: no such method: S10.seven()int/invokeStatic
// package-private-class S11: java.lang.IllegalAccessException
// int-ctor S12 type (int)S12 instance true
// int-ctor S13 type (int)S13 instance true
// clinit-uninit S14 found
// clinit S14
// clinit-uninit S14 7
// clinit-uninit S15 found
// clinit S15
// clinit-uninit S15 7
// clinit S16
// clinit-inited S16 7
// other-loader S17 ctor ()S17 static 7 loader-other true
// repeat S18 same-type true distinct true 7
// HOTSPOT_EXPECTED_END
import java.lang.classfile.ClassBuilder;
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandleInfo;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.function.Consumer;

public class L4W108FinderShapeMint {
    static final MethodHandles.Lookup LOOKUP = MethodHandles.lookup();
    static final MethodType VOID = MethodType.methodType(void.class);
    static final MethodType INT = MethodType.methodType(int.class);
    static final MethodTypeDesc VOID_DESC = MethodTypeDesc.of(ConstantDescs.CD_void);
    static final MethodTypeDesc INT_DESC = MethodTypeDesc.of(ConstantDescs.CD_int);

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W108FinderShapeMint.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    /**
     * A class `name` with flags `classFlags`, a no-arg constructor with
     * `ctorFlags` (none when -1), an `(int)` constructor when `intCtor`, a static
     * `seven()I` with `sevenFlags` (none when -1) and, when `clinit`, a
     * `<clinit>` printing "clinit <simple name>".
     */
    static byte[] build(String name, int classFlags, int ctorFlags, boolean intCtor, int sevenFlags,
            boolean clinit) {
        String simple = name.substring(name.lastIndexOf('.') + 1);
        Consumer<ClassBuilder> body = cb -> {
            cb.withFlags(classFlags | ClassFile.ACC_SUPER);
            if (ctorFlags >= 0) {
                cb.withMethodBody(ConstantDescs.INIT_NAME, VOID_DESC, ctorFlags, code -> code.aload(0)
                        .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME, VOID_DESC).return_());
            }
            if (intCtor) {
                cb.withMethodBody(ConstantDescs.INIT_NAME, MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_int),
                        ClassFile.ACC_PUBLIC, code -> code.aload(0)
                                .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME, VOID_DESC)
                                .return_());
            }
            if (sevenFlags >= 0) {
                cb.withMethodBody("seven", INT_DESC, sevenFlags | ClassFile.ACC_STATIC,
                        code -> code.bipush(7).ireturn());
            }
            if (clinit) {
                ClassDesc ps = ClassDesc.of("java.io.PrintStream");
                cb.withMethodBody(ConstantDescs.CLASS_INIT_NAME, VOID_DESC, ClassFile.ACC_STATIC,
                        code -> code.getstatic(ClassDesc.of("java.lang.System"), "out", ps)
                                .ldc("clinit " + simple)
                                .invokevirtual(ps, "println",
                                        MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_String))
                                .return_());
            }
        };
        return ClassFile.of().build(ClassDesc.of(name), body);
    }

    static final int PUB = ClassFile.ACC_PUBLIC;

    static Class<?> plain(Loader l, String name) {
        return l.define(name, build(name, PUB, PUB, false, PUB, false));
    }

    static String reveal(MethodHandle h) {
        MethodHandleInfo info = LOOKUP.revealDirect(h);
        return info.getDeclaringClass().getName() + "." + info.getName() + " "
                + MethodHandleInfo.referenceKindToString(info.getReferenceKind()) + " " + info.getMethodType();
    }

    static String error(Throwable t) {
        return t.getClass().getName() + ": " + t.getMessage();
    }

    public static void main(String[] args) throws Throwable {
        Loader l = new Loader();
        // The run: one shape, five classes in a row.
        for (int i = 0; i < 5; i++) {
            Class<?> c = plain(l, "S" + i);
            MethodHandle ctor = LOOKUP.findConstructor(c, VOID);
            MethodHandle again = LOOKUP.findConstructor(c, VOID);
            Object o = ctor.invoke();
            System.out.println("run-ctor " + c.getName() + " type " + ctor.type() + " interned "
                    + (ctor.type() == MethodType.methodType(c)) + " instance " + (o.getClass() == c) + " reveal "
                    + reveal(ctor) + " distinct " + (ctor != again));
            MethodHandle st = LOOKUP.findStatic(c, "seven", INT);
            System.out.println("run-static " + c.getName() + " " + (int) st.invokeExact() + " type " + st.type()
                    + " reveal " + reveal(st));
        }
        // Another package: the shape does not apply (body), same answers.
        {
            Class<?> c = plain(l, "p.S5");
            MethodHandle ctor = LOOKUP.findConstructor(c, VOID);
            MethodHandle st = LOOKUP.findStatic(c, "seven", INT);
            System.out.println("other-package p/S5 ctor " + ctor.type() + " static " + (int) st.invokeExact());
        }
        plain(l, "S5b"); // re-file the "" package shape
        LOOKUP.findStatic(l.loadClass("S5b"), "seven", INT);
        LOOKUP.findConstructor(l.loadClass("S5b"), VOID);
        // A member with other access flags.
        {
            Class<?> c = l.define("S6", build("S6", PUB, PUB, false, PUB | ClassFile.ACC_SYNCHRONIZED, false));
            MethodHandle st = LOOKUP.findStatic(c, "seven", INT);
            System.out.println("synchronized-static S6 " + (int) st.invokeExact() + " modifiers "
                    + LOOKUP.revealDirect(st).getModifiers());
        }
        // A class with other flags.
        {
            Class<?> c = l.define("S7", build("S7", PUB | ClassFile.ACC_FINAL, PUB, false, PUB, false));
            MethodHandle ctor = LOOKUP.findConstructor(c, VOID);
            MethodHandle st = LOOKUP.findStatic(c, "seven", INT);
            System.out.println("final-class S7 ctor " + ctor.type() + " static " + (int) st.invokeExact());
        }
        plain(l, "S7b");
        LOOKUP.findStatic(l.loadClass("S7b"), "seven", INT);
        LOOKUP.findConstructor(l.loadClass("S7b"), VOID);
        // A private constructor: the body refuses it.
        try {
            LOOKUP.findConstructor(l.define("S8", build("S8", PUB, ClassFile.ACC_PRIVATE, false, PUB, false)), VOID);
            System.out.println("private-ctor S8: found");
        } catch (ReflectiveOperationException e) {
            System.out.println("private-ctor S8: " + error(e));
        }
        // A protected static.
        try {
            LOOKUP.findStatic(l.define("S9", build("S9", PUB, PUB, false, ClassFile.ACC_PROTECTED, false)), "seven",
                    INT);
            System.out.println("protected-static S9: found");
        } catch (ReflectiveOperationException e) {
            System.out.println("protected-static S9: " + error(e));
        }
        // No such method.
        try {
            LOOKUP.findStatic(l.define("S10", build("S10", PUB, PUB, false, -1, false)), "seven", INT);
            System.out.println("missing-static S10: found");
        } catch (ReflectiveOperationException e) {
            System.out.println("missing-static S10: " + error(e));
        }
        // A package-private class of another runtime package.
        try {
            LOOKUP.findConstructor(l.define("S11", build("S11", 0, PUB, false, PUB, false)), VOID);
            System.out.println("package-private-class S11: found");
        } catch (ReflectiveOperationException e) {
            System.out.println("package-private-class S11: " + e.getClass().getName());
        }
        // Another constructor type: its own shape, served on its second class.
        for (int i = 12; i <= 13; i++) {
            Class<?> c = l.define("S" + i, build("S" + i, PUB, -1, true, -1, false));
            MethodHandle ctor = LOOKUP.findConstructor(c, MethodType.methodType(void.class, int.class));
            System.out.println("int-ctor S" + i + " type " + ctor.type() + " instance "
                    + (ctor.invoke(3).getClass() == c));
        }
        // Initialization: findStatic initializes nothing; the first call does.
        for (int i = 14; i <= 15; i++) {
            Class<?> c = l.define("S" + i, build("S" + i, PUB, PUB, false, PUB, true));
            MethodHandle st = LOOKUP.findStatic(c, "seven", INT);
            System.out.println("clinit-uninit S" + i + " found");
            System.out.println("clinit-uninit S" + i + " " + (int) st.invokeExact());
        }
        {
            Class<?> c = l.define("S16", build("S16", PUB, PUB, false, PUB, true));
            Class.forName("S16", true, l);
            MethodHandle st = LOOKUP.findStatic(c, "seven", INT);
            System.out.println("clinit-inited S16 " + (int) st.invokeExact());
        }
        // Another loader: its own shape.
        {
            Loader other = new Loader();
            Class<?> c = plain(other, "S17");
            MethodHandle ctor = LOOKUP.findConstructor(c, VOID);
            MethodHandle st = LOOKUP.findStatic(c, "seven", INT);
            System.out.println("other-loader S17 ctor " + ctor.type() + " static " + (int) st.invokeExact()
                    + " loader-other " + (ctor.invoke().getClass().getClassLoader() == other));
        }
        // A repeat of a minted answer.
        {
            Class<?> c = plain(l, "S18");
            MethodHandle a = LOOKUP.findStatic(c, "seven", INT);
            MethodHandle b = LOOKUP.findStatic(c, "seven", INT);
            System.out.println("repeat S18 same-type " + (a.type() == b.type()) + " distinct " + (a != b) + " "
                    + (int) b.invokeExact());
        }
    }
}
