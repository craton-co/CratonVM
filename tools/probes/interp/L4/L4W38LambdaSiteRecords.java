// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 38, lane L4: items 2, 3 and 6 of
// docs/known-issues/interpreter/i37-L4-lambda-site-validation-leaves-undecidable-shapes-linked-20261001.md.
// Each caller is generated with the java.lang.classfile API; `make` is one
// `invokedynamic` to `LambdaMetafactory.metafactory` and is called TWICE.
// A row prints the first failure, then the second (`[no cause]` when it has
// none) and whether the two are one object.
//
//   rec-*     JVMS 5.4.3: a failed `invokedynamic` rethrows the SAME error
//             class and message on every later execution, as a NEW object
//             without the cause (HotSpot records the failure). Before wave
//             38 CratonVM validated again and threw a new error WITH its
//             cause. `rec-nsme` is a missing implementation method, which
//             HotSpot re-resolves (a new error, no cause either time).
//   cce-*     a `MethodHandle` constant where `metafactory` takes a
//             `MethodType`: `BootstrapMethodInvoker`'s `ClassCastException`
//             names the handle's class, which depends on its reference kind.
//             Before wave 38 CratonVM could not name it and the site failed
//             with an internal error Java cannot catch (every mode).
//   field-*   a field handle as the implementation:
//             `LambdaConversionException: Unsupported MethodHandle kind: ...`
//             Before wave 38 CratonVM linked it.
//
// Positive control: CRATONVM_DBG_LAMBDA_DISPATCH=1 prints
// `[DBG_LAMBDA] link-check recorded java/lang/BootstrapMethodError for cp#N`
// once per `rec-bme-*` / `cce-*` / `field-*` row (the first execution), and
// `[DBG_LAMBDA] link-check get()Ljava/util/function/Supplier;: refused ...`
// once, not twice, per such row.
//
// Run: javac -d out L4W38LambdaSiteRecords.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W38LambdaSiteRecords
//
// `--compatible` validates only the static arguments (by design, the lambda
// conversion checks are `--jdk-only`): `rec-bme-arity` and the `field-*` rows
// link there (their lines are not recorded); every other row matches.
//
// Expected HotSpot 25 output (default and -Xint):
//   rec-bme-two: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(Lookup,String,MethodType,MethodType,MethodHandle,MethodType)CallSite to (Lookup,String,MethodType,Object,Object)Object | java.lang.BootstrapMethodError: bootstrap method initialization exception [no cause] | same=false
//   rec-bme-arity: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.invoke.LambdaConversionException: Incorrect number of parameters for static method invokeStatic L4W38LambdaSiteRecords$Impl.one:()String; 1 dynamic parameters, 0 functional interface method parameters | java.lang.BootstrapMethodError: bootstrap method initialization exception [no cause] | same=false
//   rec-nsme: java.lang.NoSuchMethodError: 'java.lang.String L4W38LambdaSiteRecords$Impl.one(java.lang.Object)' [no cause] | java.lang.NoSuchMethodError: 'java.lang.String L4W38LambdaSiteRecords$Impl.one(java.lang.Object)' [no cause] | same=false
//   cce-static: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.ClassCastException: class java.lang.invoke.DirectMethodHandle cannot be cast to class java.lang.invoke.MethodType (java.lang.invoke.DirectMethodHandle and java.lang.invoke.MethodType are in module java.base of loader 'bootstrap') | java.lang.BootstrapMethodError: bootstrap method initialization exception [no cause] | same=false
//   cce-virtual: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.ClassCastException: class java.lang.invoke.DirectMethodHandle cannot be cast to class java.lang.invoke.MethodType (java.lang.invoke.DirectMethodHandle and java.lang.invoke.MethodType are in module java.base of loader 'bootstrap') | java.lang.BootstrapMethodError: bootstrap method initialization exception [no cause] | same=false
//   cce-iface: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.ClassCastException: class java.lang.invoke.DirectMethodHandle$Interface cannot be cast to class java.lang.invoke.MethodType (java.lang.invoke.DirectMethodHandle$Interface and java.lang.invoke.MethodType are in module java.base of loader 'bootstrap') | java.lang.BootstrapMethodError: bootstrap method initialization exception [no cause] | same=false
//   cce-iface-default: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.ClassCastException: class java.lang.invoke.DirectMethodHandle$Interface cannot be cast to class java.lang.invoke.MethodType (java.lang.invoke.DirectMethodHandle$Interface and java.lang.invoke.MethodType are in module java.base of loader 'bootstrap') | java.lang.BootstrapMethodError: bootstrap method initialization exception [no cause] | same=false
//   cce-iface-static: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.ClassCastException: class java.lang.invoke.DirectMethodHandle cannot be cast to class java.lang.invoke.MethodType (java.lang.invoke.DirectMethodHandle and java.lang.invoke.MethodType are in module java.base of loader 'bootstrap') | java.lang.BootstrapMethodError: bootstrap method initialization exception [no cause] | same=false
//   cce-ctor: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.ClassCastException: class java.lang.invoke.DirectMethodHandle$Constructor cannot be cast to class java.lang.invoke.MethodType (java.lang.invoke.DirectMethodHandle$Constructor and java.lang.invoke.MethodType are in module java.base of loader 'bootstrap') | java.lang.BootstrapMethodError: bootstrap method initialization exception [no cause] | same=false
//   cce-getter: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.ClassCastException: class java.lang.invoke.DirectMethodHandle$Accessor cannot be cast to class java.lang.invoke.MethodType (java.lang.invoke.DirectMethodHandle$Accessor and java.lang.invoke.MethodType are in module java.base of loader 'bootstrap') | java.lang.BootstrapMethodError: bootstrap method initialization exception [no cause] | same=false
//   cce-setter: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.ClassCastException: class java.lang.invoke.DirectMethodHandle$Accessor cannot be cast to class java.lang.invoke.MethodType (java.lang.invoke.DirectMethodHandle$Accessor and java.lang.invoke.MethodType are in module java.base of loader 'bootstrap') | java.lang.BootstrapMethodError: bootstrap method initialization exception [no cause] | same=false
//   cce-static-getter: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.ClassCastException: class java.lang.invoke.DirectMethodHandle$StaticAccessor cannot be cast to class java.lang.invoke.MethodType (java.lang.invoke.DirectMethodHandle$StaticAccessor and java.lang.invoke.MethodType are in module java.base of loader 'bootstrap') | java.lang.BootstrapMethodError: bootstrap method initialization exception [no cause] | same=false
//   cce-static-setter: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.ClassCastException: class java.lang.invoke.DirectMethodHandle$StaticAccessor cannot be cast to class java.lang.invoke.MethodType (java.lang.invoke.DirectMethodHandle$StaticAccessor and java.lang.invoke.MethodType are in module java.base of loader 'bootstrap') | java.lang.BootstrapMethodError: bootstrap method initialization exception [no cause] | same=false
//   cce-arg2: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.ClassCastException: class java.lang.invoke.DirectMethodHandle cannot be cast to class java.lang.invoke.MethodType (java.lang.invoke.DirectMethodHandle and java.lang.invoke.MethodType are in module java.base of loader 'bootstrap') | java.lang.BootstrapMethodError: bootstrap method initialization exception [no cause] | same=false
//   field-get-static: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.invoke.LambdaConversionException: Unsupported MethodHandle kind: getStatic L4W38LambdaSiteRecords$Impl.F:()String | java.lang.BootstrapMethodError: bootstrap method initialization exception [no cause] | same=false
//   field-get: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.invoke.LambdaConversionException: Unsupported MethodHandle kind: getField L4W38LambdaSiteRecords$Impl.g:()String | java.lang.BootstrapMethodError: bootstrap method initialization exception [no cause] | same=false
//   field-put-static: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.invoke.LambdaConversionException: Unsupported MethodHandle kind: putStatic L4W38LambdaSiteRecords$Impl.F:(String)void | java.lang.BootstrapMethodError: bootstrap method initialization exception [no cause] | same=false
//   field-put: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.invoke.LambdaConversionException: Unsupported MethodHandle kind: putField L4W38LambdaSiteRecords$Impl.g:(String)void | java.lang.BootstrapMethodError: bootstrap method initialization exception [no cause] | same=false
//   field-inherited: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.invoke.LambdaConversionException: Unsupported MethodHandle kind: getStatic L4W38LambdaSiteRecords$Impl.F:()String | java.lang.BootstrapMethodError: bootstrap method initialization exception [no cause] | same=false
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.DirectMethodHandleDesc;
import java.lang.constant.DynamicCallSiteDesc;
import java.lang.constant.MethodHandleDesc;
import java.lang.constant.MethodTypeDesc;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.function.Supplier;

public class L4W38LambdaSiteRecords {
    public static class Impl {
        public static String F = "f";
        public String g = "g";

        public Impl() {
        }

        public static String one() {
            return "one";
        }

        public String inst() {
            return "inst";
        }
    }

    public static class Sub extends Impl {
    }

    public interface Iface {
        String im();

        static String sm() {
            return "s";
        }

        default String dm() {
            return "d";
        }
    }

    static final ClassDesc MT = ClassDesc.of("java.lang.invoke.MethodType");
    static final ClassDesc MH = ClassDesc.of("java.lang.invoke.MethodHandle");
    static final ClassDesc LOOKUP = ClassDesc.of("java.lang.invoke.MethodHandles$Lookup");
    static final ClassDesc CALLSITE = ClassDesc.of("java.lang.invoke.CallSite");
    static final ClassDesc LMF = ClassDesc.of("java.lang.invoke.LambdaMetafactory");
    static final DirectMethodHandleDesc META = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, LMF,
            "metafactory", MethodTypeDesc.of(CALLSITE, LOOKUP, ConstantDescs.CD_String, MT, MT, MH, MT));
    static final ClassDesc IMPL = ClassDesc.of(Impl.class.getName());
    static final ClassDesc SUB = ClassDesc.of(Sub.class.getName());
    static final ClassDesc IFACE = ClassDesc.of(Iface.class.getName());
    static final ClassDesc STR = ConstantDescs.CD_String;
    static final ClassDesc OBJ = ConstantDescs.CD_Object;
    static final ClassDesc SUPPLIER = ClassDesc.of("java.util.function.Supplier");

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W38LambdaSiteRecords.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static final Loader LOADER = new Loader();

    static String describe(Throwable t) {
        Throwable c = t.getCause();
        return t.getClass().getName() + ": " + t.getMessage()
                + (c == null ? " [no cause]" : " / " + c.getClass().getName() + ": " + c.getMessage());
    }

    static String row(String name, MethodTypeDesc factory, ConstantDesc... staticArgs) {
        DynamicCallSiteDesc site = DynamicCallSiteDesc.of(META, "get", factory, staticArgs);
        MethodTypeDesc makeType = MethodTypeDesc.of(OBJ, factory.parameterArray());
        byte[] b = ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("make", makeType, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> {
                for (int i = 0; i < factory.parameterCount(); i++) {
                    code.aload(i);
                }
                code.invokedynamic(site).areturn();
            });
        });
        Class<?>[] ps = new Class<?>[factory.parameterCount()];
        Object[] callArgs = new Object[ps.length];
        for (int i = 0; i < ps.length; i++) {
            ps[i] = Impl.class;
            callArgs[i] = new Impl();
        }
        Method make;
        try {
            make = LOADER.define(name, b).getMethod("make", ps);
        } catch (Throwable t) {
            return "setup: " + t;
        }
        StringBuilder out = new StringBuilder();
        Throwable first = null;
        for (int i = 0; i < 2; i++) {
            if (i > 0) {
                out.append(" | ");
            }
            try {
                Object r = make.invoke(null, callArgs);
                out.append("linked ").append(r instanceof Supplier<?> s ? s.get() : r);
            } catch (InvocationTargetException e) {
                Throwable t = e.getCause();
                out.append(describe(t));
                if (first == null) {
                    first = t;
                } else {
                    out.append(" | same=").append(first == t);
                }
            } catch (Throwable t) {
                out.append("call: ").append(t);
            }
        }
        return out.toString();
    }

    // HotSpot resolves every type of the three `MethodType`s before the
    // factory runs; CratonVM decides only over loaded classes.
    static final Class<?>[] PRELOAD = {Supplier.class, Impl.class, Sub.class, Iface.class};

    public static void main(String[] args) {
        if (PRELOAD.length != 4) {
            throw new AssertionError();
        }
        MethodTypeDesc toStr = MethodTypeDesc.of(STR);
        MethodTypeDesc toObj = MethodTypeDesc.of(OBJ);
        MethodTypeDesc sup = MethodTypeDesc.of(SUPPLIER);
        DirectMethodHandleDesc one = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, IMPL, "one", toStr);

        System.out.println("rec-bme-two: " + row("LrRecTwo", sup, toObj, one));
        System.out.println("rec-bme-arity: " + row("LrRecArity", sup, toObj, one, MethodTypeDesc.of(STR, OBJ)));
        System.out.println("rec-nsme: " + row("LrRecNsme", sup, toObj,
                MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, IMPL, "one", MethodTypeDesc.of(STR, OBJ)),
                toStr));

        System.out.println("cce-static: " + row("LrCceStatic", sup, one, one, toStr));
        System.out.println("cce-virtual: " + row("LrCceVirtual", sup,
                MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.VIRTUAL, IMPL, "inst", toStr), one, toStr));
        System.out.println("cce-iface: " + row("LrCceIface", sup,
                MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.INTERFACE_VIRTUAL, IFACE, "im", toStr), one,
                toStr));
        System.out.println("cce-iface-default: " + row("LrCceIfaceDefault", sup,
                MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.INTERFACE_VIRTUAL, IFACE, "dm", toStr), one,
                toStr));
        System.out.println("cce-iface-static: " + row("LrCceIfaceStatic", sup,
                MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.INTERFACE_STATIC, IFACE, "sm", toStr), one,
                toStr));
        System.out.println("cce-ctor: " + row("LrCceCtor", sup, MethodHandleDesc.ofConstructor(IMPL), one, toStr));
        System.out.println("cce-getter: " + row("LrCceGetter", sup,
                MethodHandleDesc.ofField(DirectMethodHandleDesc.Kind.GETTER, IMPL, "g", STR), one, toStr));
        System.out.println("cce-setter: " + row("LrCceSetter", sup,
                MethodHandleDesc.ofField(DirectMethodHandleDesc.Kind.SETTER, IMPL, "g", STR), one, toStr));
        System.out.println("cce-static-getter: " + row("LrCceStaticGetter", sup,
                MethodHandleDesc.ofField(DirectMethodHandleDesc.Kind.STATIC_GETTER, IMPL, "F", STR), one, toStr));
        System.out.println("cce-static-setter: " + row("LrCceStaticSetter", sup,
                MethodHandleDesc.ofField(DirectMethodHandleDesc.Kind.STATIC_SETTER, IMPL, "F", STR), one, toStr));
        System.out.println("cce-arg2: " + row("LrCceArg2", sup, toObj, one, one));

        System.out.println("field-get-static: " + row("LrFieldGetStatic", sup, toObj,
                MethodHandleDesc.ofField(DirectMethodHandleDesc.Kind.STATIC_GETTER, IMPL, "F", STR), toStr));
        System.out.println("field-get: " + row("LrFieldGet", MethodTypeDesc.of(SUPPLIER, IMPL), toObj,
                MethodHandleDesc.ofField(DirectMethodHandleDesc.Kind.GETTER, IMPL, "g", STR), toStr));
        System.out.println("field-put-static: " + row("LrFieldPutStatic", sup, toObj,
                MethodHandleDesc.ofField(DirectMethodHandleDesc.Kind.STATIC_SETTER, IMPL, "F", STR), toStr));
        System.out.println("field-put: " + row("LrFieldPut", sup, toObj,
                MethodHandleDesc.ofField(DirectMethodHandleDesc.Kind.SETTER, IMPL, "g", STR), toStr));
        System.out.println("field-inherited: " + row("LrFieldInherited", sup, toObj,
                MethodHandleDesc.ofField(DirectMethodHandleDesc.Kind.STATIC_GETTER, SUB, "F", STR), toStr));
    }
}
