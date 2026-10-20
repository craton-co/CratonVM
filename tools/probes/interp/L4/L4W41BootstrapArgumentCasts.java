// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 41, lane L4 (review): a bootstrap method's
// static arguments are CAST to its parameter types, as
// `BootstrapMethodInvoker` invokes it (`invoke` with `Object` arguments,
// whose `asType` casts each reference, and a varargs collector that stores
// into an array of its declared type). A constant of another class is a
// `ClassCastException`, wrapped in `BootstrapMethodError`; the bootstrap
// never runs. CratonVM passed the constant as it was: an `Integer` reached a
// `String` parameter, or was stored into the `String[]` the varargs
// parameter collected (wave 40's typed collection), and the bootstrap ran
// with a value its declared type excludes.
//
// Each row generates (java.lang.classfile) a class whose static `make()` is
// one `invokedynamic` over a user bootstrap of this class, and runs it twice.
//
//   fixed-*     `(Lookup, String, MethodType, String)`
//   varargs-*   `(Lookup, String, MethodType, String...)`
//   class-*     `(Lookup, String, MethodType, Class)`
//
// Every bootstrap answers a `ConstantCallSite` of a constant naming what it
// received, so a row that links prints what the bootstrap saw.
//
// Run: javac -d out L4W41BootstrapArgumentCasts.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W41BootstrapArgumentCasts
//
// Expected HotSpot 25 output (default and -Xint):
//   fixed-ok: fixed:p | fixed:p
//   fixed-int: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.String | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   fixed-class: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.ClassCastException: Cannot cast java.lang.Class to java.lang.String | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   varargs-ok: varargs:String=p;String=q;String[] | varargs:String=p;String=q;String[]
//   varargs-int: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.String | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   varargs-mixed: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.String | java.lang.BootstrapMethodError: bootstrap method initialization exception
//   class-ok: class:java.lang.String | class:java.lang.String
//   class-string: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.ClassCastException: Cannot cast java.lang.String to java.lang.Class | java.lang.BootstrapMethodError: bootstrap method initialization exception
//
// `--compatible` does not judge these casts (the check reads class
// hierarchies it trusts only under `--jdk-only`); its lines are not recorded.
//
// Positive control: CRATONVM_DBG_INDY_ALL=1 prints
//   `[indy-all] bootstrap L4W41BootstrapArgumentCasts.fixed: static argument 3: Cannot cast java.lang.Integer to java.lang.String`
// for `fixed-int`.
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.DirectMethodHandleDesc;
import java.lang.constant.DynamicCallSiteDesc;
import java.lang.constant.MethodHandleDesc;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.CallSite;
import java.lang.invoke.ConstantCallSite;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.InvocationTargetException;

public class L4W41BootstrapArgumentCasts {
    static final ClassDesc SELF = ClassDesc.of("L4W41BootstrapArgumentCasts");
    static final ClassDesc STR = ConstantDescs.CD_String;
    static final ClassDesc LOOKUP = ClassDesc.of("java.lang.invoke.MethodHandles$Lookup");
    static final ClassDesc MT = ClassDesc.of("java.lang.invoke.MethodType");
    static final ClassDesc CS = ClassDesc.of("java.lang.invoke.CallSite");

    static CallSite answer(String text) {
        return new ConstantCallSite(MethodHandles.constant(String.class, text));
    }

    public static CallSite fixed(MethodHandles.Lookup l, String n, MethodType t, String s) {
        return answer("fixed:" + s);
    }

    public static CallSite varargs(MethodHandles.Lookup l, String n, MethodType t, String... parts) {
        StringBuilder b = new StringBuilder("varargs:");
        for (Object p : (Object[]) parts) {
            b.append(p.getClass().getSimpleName()).append('=').append(p).append(';');
        }
        return answer(b.append(parts.getClass().getSimpleName()).toString());
    }

    public static CallSite klass(MethodHandles.Lookup l, String n, MethodType t, Class<?> c) {
        return answer("class:" + c.getName());
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W41BootstrapArgumentCasts.class.getClassLoader());
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

    static String row(String cls, String bsmName, ClassDesc last, ConstantDesc... args) {
        DirectMethodHandleDesc bsm = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, SELF, bsmName,
                MethodTypeDesc.of(CS, LOOKUP, STR, MT, last));
        DynamicCallSiteDesc site = DynamicCallSiteDesc.of(bsm, "x", MethodTypeDesc.of(STR), args);
        byte[] b = ClassFile.of().build(ClassDesc.of(cls), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("make", MethodTypeDesc.of(STR), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.invokedynamic(site).areturn());
        });
        try {
            var make = LOADER.define(cls, b).getMethod("make");
            StringBuilder out = new StringBuilder();
            for (int i = 0; i < 2; i++) {
                if (i > 0) {
                    out.append(" | ");
                }
                try {
                    out.append(make.invoke(null));
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
        ClassDesc strArr = STR.arrayType();
        ClassDesc cls = ConstantDescs.CD_Class;
        System.out.println("fixed-ok: " + row("LoFixedOk", "fixed", STR, "p"));
        System.out.println("fixed-int: " + row("LoFixedInt", "fixed", STR, 1));
        System.out.println("fixed-class: " + row("LoFixedClass", "fixed", STR, ConstantDescs.CD_int.arrayType()));
        System.out.println("varargs-ok: " + row("LoVarargsOk", "varargs", strArr, "p", "q"));
        System.out.println("varargs-int: " + row("LoVarargsInt", "varargs", strArr, 1));
        System.out.println("varargs-mixed: " + row("LoVarargsMixed", "varargs", strArr, "p", 2));
        System.out.println("class-ok: " + row("LoClassOk", "klass", cls, STR));
        System.out.println("class-string: " + row("LoClassString", "klass", cls, "p"));
    }
}
