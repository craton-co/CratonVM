// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 101, lane L4: an `invokedynamic` whose bootstrap
// method is declared with a primitive (or void) return.
//
// JDK 25 `BootstrapMethodInvoker.invoke` calls the bootstrap through
// `invoke` with an `Object` result, so the answer is boxed by the DECLARED
// return type, and `CallSite.class.cast(box)` fails:
//   BootstrapMethodError("bootstrap method initialization exception")
//   caused by ClassCastException("Cannot cast java.lang.Integer to java.lang.invoke.CallSite")
// A `void` bootstrap answers `null`, which passes the cast; `CallSite.makeSite`
// then throws its own
//   BootstrapMethodError("CallSite bootstrap method initialization exception")
//   caused by ClassCastException("CallSite bootstrap method failed to produce an instance of CallSite").
// CratonVM (both modes) reported the primitive answers as the `void` one.
//
// Run: javac -d . L4W101IndyPrimitiveBootstrapResult.java
//      java -cp . L4W101IndyPrimitiveBootstrapResult
//      cratonvm --jdk-only -cp . L4W101IndyPrimitiveBootstrapResult
//      cratonvm --compatible -cp . L4W101IndyPrimitiveBootstrapResult
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25; derived from the sources)
// int java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.invoke.CallSite
// boolean java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.ClassCastException: Cannot cast java.lang.Boolean to java.lang.invoke.CallSite
// long java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.ClassCastException: Cannot cast java.lang.Long to java.lang.invoke.CallSite
// void java.lang.BootstrapMethodError: CallSite bootstrap method initialization exception / java.lang.ClassCastException: CallSite bootstrap method failed to produce an instance of CallSite
// calls 4
// HOTSPOT_EXPECTED_END
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.DirectMethodHandleDesc;
import java.lang.constant.DynamicCallSiteDesc;
import java.lang.constant.MethodHandleDesc;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W101IndyPrimitiveBootstrapResult {
    static final ClassDesc SELF = ClassDesc.of("L4W101IndyPrimitiveBootstrapResult");
    static final ClassDesc LOOKUP = ClassDesc.of("java.lang.invoke.MethodHandles$Lookup");
    static final ClassDesc RUNNABLE = ClassDesc.of("java.lang.Runnable");
    static int calls;

    public static int bInt(MethodHandles.Lookup l, String n, MethodType t) {
        calls++;
        return 5;
    }

    public static boolean bBoolean(MethodHandles.Lookup l, String n, MethodType t) {
        calls++;
        return true;
    }

    public static long bLong(MethodHandles.Lookup l, String n, MethodType t) {
        calls++;
        return 7L;
    }

    public static void bVoid(MethodHandles.Lookup l, String n, MethodType t) {
        calls++;
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W101IndyPrimitiveBootstrapResult.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static int classes;

    static Runnable make(String bsm, ClassDesc ret) throws Exception {
        String name = "IdyP" + (classes++);
        DirectMethodHandleDesc handle = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, SELF, bsm,
                MethodTypeDesc.of(ret, LOOKUP, ConstantDescs.CD_String, ConstantDescs.CD_MethodType));
        MethodTypeDesc voidType = MethodTypeDesc.of(ConstantDescs.CD_void);
        byte[] bytes = ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withInterfaceSymbols(RUNNABLE);
            cb.withMethodBody(ConstantDescs.INIT_NAME, voidType, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0)
                            .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME, voidType)
                            .return_());
            cb.withMethodBody("run", voidType, ClassFile.ACC_PUBLIC,
                    code -> code.invokedynamic(DynamicCallSiteDesc.of(handle, "site", voidType)).return_());
        });
        return (Runnable) new Loader().define(name, bytes).getConstructor().newInstance();
    }

    static void row(String label, String bsm, ClassDesc ret) throws Exception {
        Runnable r = make(bsm, ret);
        try {
            r.run();
            System.out.println(label + " linked");
        } catch (Throwable t) {
            Throwable c = t.getCause();
            System.out.println(label + " " + t.getClass().getName() + ": " + t.getMessage() + " / "
                    + (c == null ? "null" : c.getClass().getName() + ": " + c.getMessage()));
        }
    }

    public static void main(String[] args) throws Exception {
        row("int", "bInt", ConstantDescs.CD_int);
        row("boolean", "bBoolean", ConstantDescs.CD_boolean);
        row("long", "bLong", ConstantDescs.CD_long);
        row("void", "bVoid", ConstantDescs.CD_void);
        System.out.println("calls " + calls);
    }
}
