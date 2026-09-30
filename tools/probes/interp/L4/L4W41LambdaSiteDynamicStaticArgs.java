// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 41, lane L4 (row 4 of
// `i37-L4-lambda-site-validation-leaves-undecidable-shapes-linked`): a
// dynamic constant (`CONSTANT_Dynamic`) among a `LambdaMetafactory` site's
// static arguments is resolved before the bootstrap runs, and its VALUE is
// what `BootstrapMethodInvoker` casts and `metafactory` checks. A valid
// `MethodType` links; `null` is a `NullPointerException` (from
// `Objects.requireNonNull`, no message), a value of another class the cast's
// `ClassCastException`, both wrapped in `BootstrapMethodError`; a failing
// condy bootstrap is the condy's own error. CratonVM read positions 0 and 2
// from the constant pool, so every such site, the valid one included, failed
// with an uncatchable internal error (`invalid SAM erased MethodType`).
//
// Each row generates (java.lang.classfile) a class `Lo<Row>` whose static
// `make()` is one `invokedynamic` over `LambdaMetafactory.metafactory` for
// `Supplier.get` with the static arguments `(()Object, hello()String,
// ()String)`, one of them replaced by a dynamic constant whose bootstrap
// (`L4W41LambdaSiteDynamicStaticArgs.condy`) answers by its name: `mt0` /
// `mt2` the `MethodType` that belongs there, `null`, `str` a `String` (the
// constant is declared `String`, so the cast that fails is the invoker's),
// `boom` throws `IllegalStateException("boom")` (the condy's own
// `BootstrapMethodError`). Each `make()` runs twice.
//
// Positive control: CRATONVM_DBG_LAMBDA_DISPATCH=1 prints
//   `[DBG_LAMBDA] link-check get()Ljava/util/function/Supplier;: dynamic static argument 0 is a MethodType`
// for `mt-0` (argument 2 for `mt-2`), then `... validated`.
//
// Run: javac -d out L4W41LambdaSiteDynamicStaticArgs.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W41LambdaSiteDynamicStaticArgs
//
// Expected HotSpot 25 output (default and -Xint), also `--compatible`'s:
//   mt-0: hi | hi
//   mt-2: hi | hi
//   null-0: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.NullPointerException: null | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   null-1: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.NullPointerException: null | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   null-2: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.NullPointerException: null | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   str-0: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.ClassCastException: class java.lang.String cannot be cast to class java.lang.invoke.MethodType (java.lang.String and java.lang.invoke.MethodType are in module java.base of loader 'bootstrap') | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   str-1: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.ClassCastException: class java.lang.String cannot be cast to class java.lang.invoke.MethodHandle (java.lang.String and java.lang.invoke.MethodHandle are in module java.base of loader 'bootstrap') | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   str-2: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.ClassCastException: class java.lang.String cannot be cast to class java.lang.invoke.MethodType (java.lang.String and java.lang.invoke.MethodType are in module java.base of loader 'bootstrap') | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   boom-0: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.IllegalStateException: boom | java.lang.BootstrapMethodError: bootstrap method initialization exception
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

public class L4W41LambdaSiteDynamicStaticArgs {
    static final ClassDesc SELF = ClassDesc.of("L4W41LambdaSiteDynamicStaticArgs");
    static final ClassDesc LMF = ClassDesc.of("java.lang.invoke.LambdaMetafactory");
    static final ClassDesc MT = ClassDesc.of("java.lang.invoke.MethodType");
    static final ClassDesc MH = ClassDesc.of("java.lang.invoke.MethodHandle");
    static final ClassDesc STR = ConstantDescs.CD_String;
    static final DirectMethodHandleDesc BSM = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, LMF,
            "metafactory", MethodTypeDesc.of(ClassDesc.of("java.lang.invoke.CallSite"),
                    ClassDesc.of("java.lang.invoke.MethodHandles$Lookup"), STR, MT, MT, MH, MT));
    static final DirectMethodHandleDesc CONDY = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, SELF,
            "condy", MethodTypeDesc.of(ConstantDescs.CD_Object,
                    ClassDesc.of("java.lang.invoke.MethodHandles$Lookup"), STR, ConstantDescs.CD_Class));

    /** The dynamic constants' bootstrap: answers by the constant's name. */
    public static Object condy(MethodHandles.Lookup lookup, String name, Class<?> type) {
        return switch (name) {
            case "mt0" -> MethodType.methodType(Object.class);
            case "mt2" -> MethodType.methodType(String.class);
            case "str" -> "not a constant of that class";
            case "boom" -> throw new IllegalStateException("boom");
            default -> null;
        };
    }

    public static String hello() {
        return "hi";
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W41LambdaSiteDynamicStaticArgs.class.getClassLoader());
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

    /** `position` of the three static arguments is the dynamic constant `name`. */
    @SuppressWarnings("unchecked")
    static String row(String rowName, int position, String name) {
        ClassDesc[] types = {MT, MH, MT};
        ConstantDesc[] args = {
            MethodTypeDesc.of(ConstantDescs.CD_Object),
            MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, SELF, "hello", MethodTypeDesc.of(STR)),
            MethodTypeDesc.of(STR),
        };
        // `str` is declared `String` (the indy's cast fails, not the condy's).
        ClassDesc type = name.equals("str") ? STR : types[position];
        args[position] = DynamicConstantDesc.ofNamed(CONDY, name, type);
        DynamicCallSiteDesc site = DynamicCallSiteDesc.of(BSM, "get",
                MethodTypeDesc.of(ClassDesc.of("java.util.function.Supplier")), args);
        String cls = "Lo" + rowName.replace("-", "");
        byte[] b = ClassFile.of().build(ClassDesc.of(cls), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("make", MethodTypeDesc.of(ConstantDescs.CD_Object),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> code.invokedynamic(site).areturn());
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
        System.out.println("mt-0: " + row("mt-0", 0, "mt0"));
        System.out.println("mt-2: " + row("mt-2", 2, "mt2"));
        System.out.println("null-0: " + row("null-0", 0, "null"));
        System.out.println("null-1: " + row("null-1", 1, "null"));
        System.out.println("null-2: " + row("null-2", 2, "null"));
        System.out.println("str-0: " + row("str-0", 0, "str"));
        System.out.println("str-1: " + row("str-1", 1, "str"));
        System.out.println("str-2: " + row("str-2", 2, "str"));
        System.out.println("boom-0: " + row("boom-0", 0, "boom"));
    }
}
