// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 42, lane L4 (row 4 of
// `i37-L4-lambda-site-validation-leaves-undecidable-shapes-linked`, "What
// remains of row 4 (wave 41)"): a dynamic constant (`CONSTANT_Dynamic`) that
// answers the IMPLEMENTATION handle of a `LambdaMetafactory` site (static
// argument 1). HotSpot resolves the constant, casts it to `MethodHandle` and
// links the lambda. CratonVM read position 1 from the constant pool only, so
// such a site failed with an uncatchable internal error (`invokedynamic: cp#N
// is not a MethodHandle`).
//
// Each row generates (java.lang.classfile) a class `Lo<Row>` whose static
// `make()` is one `invokedynamic` over `LambdaMetafactory.metafactory` for
// `Supplier.get` whose static argument 1 is a dynamic constant (declared
// `MethodHandle`) answered by `L4W42LambdaSiteDynamicMethodHandle.condy`:
//
//   static      `findStatic(Probe, "hello", ()String)`         -> "hi"
//   virtual     `findVirtual(String, "trim", ()String)`, the site captures " hi "
//   interface   `findVirtual(CharSequence, "length", ()int)`, captures "abc",
//               instantiated `()Integer`                        -> 3
//
// Each `make()` runs twice and calls `get()` on what it answers.
//
// Positive control: CRATONVM_DBG_LAMBDA_DISPATCH=1 prints
//   `[DBG_LAMBDA] link-check get()Ljava/util/function/Supplier;: dynamic static argument 1 is a MethodHandle (REF_invokeStatic L4W42LambdaSiteDynamicMethodHandle.hello()Ljava/lang/String;)`
// for row `static` (`REF_invokeVirtual java/lang/String.trim...` for
// `virtual`, `REF_invokeInterface java/lang/CharSequence.length()I` for
// `interface`).
//
// Run: javac -d out L4W42LambdaSiteDynamicMethodHandle.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W42LambdaSiteDynamicMethodHandle
//
// Expected HotSpot 25 output (default and -Xint, measured locally), also
// `--compatible`'s (the fix is in every mode):
//   static: hi | hi
//   virtual: hi | hi
//   interface: 3 | 3
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.DirectMethodHandleDesc;
import java.lang.constant.DynamicCallSiteDesc;
import java.lang.constant.DynamicConstantDesc;
import java.lang.constant.MethodHandleDesc;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.InvocationTargetException;
import java.util.function.Supplier;

public class L4W42LambdaSiteDynamicMethodHandle {
    static final ClassDesc SELF = ClassDesc.of("L4W42LambdaSiteDynamicMethodHandle");
    static final ClassDesc LMF = ClassDesc.of("java.lang.invoke.LambdaMetafactory");
    static final ClassDesc MT = ClassDesc.of("java.lang.invoke.MethodType");
    static final ClassDesc MH = ClassDesc.of("java.lang.invoke.MethodHandle");
    static final ClassDesc STR = ConstantDescs.CD_String;
    static final ClassDesc SUPPLIER = ClassDesc.of("java.util.function.Supplier");
    static final DirectMethodHandleDesc BSM = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, LMF,
            "metafactory", MethodTypeDesc.of(ClassDesc.of("java.lang.invoke.CallSite"),
                    ClassDesc.of("java.lang.invoke.MethodHandles$Lookup"), STR, MT, MT, MH, MT));
    static final DirectMethodHandleDesc CONDY = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, SELF,
            "condy", MethodTypeDesc.of(ConstantDescs.CD_Object,
                    ClassDesc.of("java.lang.invoke.MethodHandles$Lookup"), STR, ConstantDescs.CD_Class));

    /** The dynamic constants' bootstrap: answers by the constant's name. */
    public static Object condy(MethodHandles.Lookup lookup, String name, Class<?> type) throws Exception {
        return switch (name) {
            case "static" -> lookup.findStatic(L4W42LambdaSiteDynamicMethodHandle.class, "hello",
                    MethodType.methodType(String.class));
            case "virtual" -> lookup.findVirtual(String.class, "trim", MethodType.methodType(String.class));
            case "interface" -> lookup.findVirtual(CharSequence.class, "length", MethodType.methodType(int.class));
            default -> null;
        };
    }

    public static String hello() {
        return "hi";
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W42LambdaSiteDynamicMethodHandle.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static final Loader LOADER = new Loader();

    static String describe(Throwable t) {
        Throwable c = t.getCause();
        return t.getClass().getName() + ": " + t.getMessage()
                + (c == null ? "" : " / " + c.getClass().getName() + ": " + c.getMessage());
    }

    /** `captured` is `null` for no captured argument, else its declared type and value. */
    @SuppressWarnings("unchecked")
    static String row(String rowName, ClassDesc capturedType, String captured, ClassDesc instantiatedReturn) {
        ConstantDesc[] args = {
            MethodTypeDesc.of(ConstantDescs.CD_Object),
            DynamicConstantDesc.ofNamed(CONDY, rowName, MH),
            MethodTypeDesc.of(instantiatedReturn),
        };
        MethodTypeDesc siteType = capturedType == null
                ? MethodTypeDesc.of(SUPPLIER)
                : MethodTypeDesc.of(SUPPLIER, capturedType);
        DynamicCallSiteDesc site = DynamicCallSiteDesc.of(BSM, "get", siteType, args);
        String cls = "Lo" + rowName;
        byte[] b = ClassFile.of().build(ClassDesc.of(cls), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("make", MethodTypeDesc.of(ConstantDescs.CD_Object),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> {
                        if (captured != null) {
                            code.ldc(captured);
                        }
                        code.invokedynamic(site).areturn();
                    });
        });
        try {
            var make = LOADER.define(cls, b).getMethod("make");
            StringBuilder out = new StringBuilder();
            for (int i = 0; i < 2; i++) {
                if (i > 0) {
                    out.append(" | ");
                }
                try {
                    out.append(((Supplier<Object>) make.invoke(null)).get());
                } catch (InvocationTargetException e) {
                    out.append(describe(e.getCause()));
                }
            }
            return out.toString();
        } catch (Throwable t) {
            return "setup: " + t;
        }
    }

    public static void main(String[] args) {
        System.out.println("static: " + row("static", null, null, STR));
        System.out.println("virtual: " + row("virtual", STR, " hi ", STR));
        System.out.println("interface: " + row("interface", ClassDesc.of("java.lang.CharSequence"), "abc",
                ClassDesc.of("java.lang.Integer")));
    }
}
