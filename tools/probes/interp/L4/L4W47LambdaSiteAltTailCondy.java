// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 47, lane L4 (row 4 of
// `docs/internal/fixed-bugs/interpreter-L4-lambda-site-validation-leaves-undecidable-shapes-linked-FIXED-20261012.md`,
// `altMetafactory`'s tail): a dynamic constant among an `altMetafactory`
// site's trailing static arguments (the flags word, a marker or bridge
// count, a marker class, a bridge type). `altMetafactory` reads each one
// through `extractArg`, so the constant's VALUE is what counts. CratonVM
// classified the value (wave 41) but read the tail from the constant pool,
// so the checks stopped undecided and the linkage read flags 0 (no marker:
// `instanceof Cloneable` answered false where HotSpot says true).
//
// Each row generates (java.lang.classfile) a class `Lo<Row>` whose static
// `make()` is one `invokedynamic` over `LambdaMetafactory.altMetafactory`
// for `Supplier.get` with `(()Object, hello()String, ()String, <tail>)`, one
// tail value replaced by a dynamic constant whose bootstrap answers by its
// name (declared `Object`, or `int` for `prim-flags`). Each `make()` runs
// twice; a linked row prints `get()` and `instanceof Cloneable`.
//
// Positive control: CRATONVM_DBG_LAMBDA_DISPATCH=1 prints, for row
// `flags`, `[DBG_LAMBDA] link-check get()Ljava/util/function/Supplier;:
// dynamic static argument 3 is a constant of another class` and then
// `... validated` (the base printed `... undecided (an altMetafactory int)`).
//
// Run: javac -d out L4W47LambdaSiteAltTailCondy.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W47LambdaSiteAltTailCondy
//
// Expected HotSpot 25 output (default and -Xint, identical):
//   flags: hi true | hi true
//   prim-flags: hi true | hi true
//   count: hi true | hi true
//   marker: hi true | hi true
//   bridge-count: hi false | hi false
//   bridge: hi false | hi false
//   neg-count: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.IllegalArgumentException: negative argument count | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   null-flags: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.NullPointerException: null | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   str-marker: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.IllegalArgumentException: argument has wrong type | java.lang.BootstrapMethodError: bootstrap method initialization exception
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

public class L4W47LambdaSiteAltTailCondy {
    static final ClassDesc SELF = ClassDesc.of("L4W47LambdaSiteAltTailCondy");
    static final ClassDesc LMF = ClassDesc.of("java.lang.invoke.LambdaMetafactory");
    static final ClassDesc MT = ClassDesc.of("java.lang.invoke.MethodType");
    static final ClassDesc LOOKUP = ClassDesc.of("java.lang.invoke.MethodHandles$Lookup");
    static final ClassDesc CALL_SITE = ClassDesc.of("java.lang.invoke.CallSite");
    static final ClassDesc STR = ConstantDescs.CD_String;
    static final DirectMethodHandleDesc ALT = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, LMF,
            "altMetafactory", MethodTypeDesc.of(CALL_SITE, LOOKUP, STR, MT, ConstantDescs.CD_Object.arrayType()));
    static final DirectMethodHandleDesc CONDY = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, SELF,
            "condy", MethodTypeDesc.of(ConstantDescs.CD_Object, LOOKUP, STR, ConstantDescs.CD_Class));

    static final int FLAG_MARKERS = 2;
    static final int FLAG_BRIDGES = 4;

    /** The dynamic constants' bootstrap: answers by the constant's name. */
    public static Object condy(MethodHandles.Lookup lookup, String name, Class<?> type) {
        return switch (name) {
            case "markers" -> Integer.valueOf(FLAG_MARKERS);
            case "bridges" -> Integer.valueOf(FLAG_BRIDGES);
            case "one" -> Integer.valueOf(1);
            case "minus" -> Integer.valueOf(-1);
            case "cloneable" -> Cloneable.class;
            case "bridgeType" -> MethodType.methodType(String.class);
            case "str" -> "x";
            default -> null;
        };
    }

    public static String hello() {
        return "hi";
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W47LambdaSiteAltTailCondy.class.getClassLoader());
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

    static ConstantDesc dyn(String name) {
        return DynamicConstantDesc.ofNamed(CONDY, name, ConstantDescs.CD_Object);
    }

    @SuppressWarnings("unchecked")
    static String row(String rowName, ConstantDesc... tail) {
        ConstantDesc[] args = new ConstantDesc[3 + tail.length];
        args[0] = MethodTypeDesc.of(ConstantDescs.CD_Object);
        args[1] = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, SELF, "hello", MethodTypeDesc.of(STR));
        args[2] = MethodTypeDesc.of(STR);
        System.arraycopy(tail, 0, args, 3, tail.length);
        DynamicCallSiteDesc site = DynamicCallSiteDesc.of(ALT, "get",
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
                    Object s = make.invoke(null);
                    out.append(((Supplier<Object>) s).get()).append(' ').append(s instanceof Cloneable);
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
        ClassDesc cloneable = ClassDesc.of("java.lang.Cloneable");
        System.out.println("flags: " + row("flags", dyn("markers"), 1, cloneable));
        System.out.println("prim-flags: " + row("prim-flags",
                DynamicConstantDesc.ofNamed(CONDY, "markers", ConstantDescs.CD_int), 1, cloneable));
        System.out.println("count: " + row("count", FLAG_MARKERS, dyn("one"), cloneable));
        System.out.println("marker: " + row("marker", FLAG_MARKERS, 1, dyn("cloneable")));
        System.out.println("bridge-count: " + row("bridge-count", FLAG_BRIDGES, dyn("one"),
                MethodTypeDesc.of(STR)));
        System.out.println("bridge: " + row("bridge", dyn("bridges"), 1, dyn("bridgeType")));
        System.out.println("neg-count: " + row("neg-count", FLAG_MARKERS, dyn("minus")));
        System.out.println("null-flags: " + row("null-flags", dyn("nothing")));
        System.out.println("str-marker: " + row("str-marker", FLAG_MARKERS, 1, dyn("str")));
    }
}
