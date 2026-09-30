// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 37, lane L4: the rest of item 2 of
// docs/internal/fixed-bugs/interpreter-L4-native-indy-linkage-skips-the-jdks-validation-FIXED-20261001.md.
// A `LambdaMetafactory` call site the JDK refuses fails linkage as HotSpot's
// does. Each caller is generated with the java.lang.classfile API; `make`
// takes the factory type's parameters and is one `invokedynamic`.
//
//   meta-two / meta-none   `metafactory` with 2 / 0 static arguments
//   alt-*                  `altMetafactory` with a malformed tail
//   arg-*                  a SAM argument the implementation cannot take
//   ret-*                  an implementation return the call cannot answer
//   captured, receiver     a capture of the wrong type
//   dyn-*                  the dynamic (instantiated) type against the SAM's
//   sam-arity              the SAM's arity is the ERASED type's (arg 0)
//   inherited              `MethodHandleInfo` names the declaring class
//   ok-*                   the controls, which link and answer
//
// Before wave 37 CratonVM linked every refused shape except `meta-two`,
// `meta-none`, `meta-arg0-string` and `meta-arg1-type`, which were an
// uncatchable internal error. Those four print HotSpot's line in every mode.
// `--compatible` still links the other refused shapes, as before (its lines
// for them are not recorded).
//
// Positive control: CRATONVM_DBG_LAMBDA_DISPATCH=1 prints one
// `[DBG_LAMBDA] link-check <name><type>: refused|validated|undecided (...)`
// line per lambda site; every `ok-*` row must say `validated`.
//
// Run: javac -d out L4W37LambdaSiteValidation.java && cratonvm --java-home <jdk25> [--nojit] -cp out L4W37LambdaSiteValidation
//
// Expected HotSpot 25 output (default and -Xint):
//   meta-two: java.lang.BootstrapMethodError / java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(Lookup,String,MethodType,MethodType,MethodHandle,MethodType)CallSite to (Lookup,String,MethodType,Object,Object)Object
//   meta-four: java.lang.BootstrapMethodError / java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(Lookup,String,MethodType,MethodType,MethodHandle,MethodType)CallSite to (Lookup,String,MethodType,Object,Object,Object,Object)Object
//   meta-seven: java.lang.BootstrapMethodError / java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(Lookup,String,MethodType,MethodType,MethodHandle,MethodType)CallSite to (Object,Object,Object,Object,Object,Object,Object,Object,Object,Object)Object
//   meta-arg0-string: java.lang.BootstrapMethodError / java.lang.ClassCastException: class java.lang.String cannot be cast to class java.lang.invoke.MethodType (java.lang.String and java.lang.invoke.MethodType are in module java.base of loader 'bootstrap')
//   meta-arg1-type: java.lang.BootstrapMethodError / java.lang.ClassCastException: class java.lang.invoke.MethodType cannot be cast to class java.lang.invoke.MethodHandle (java.lang.invoke.MethodType and java.lang.invoke.MethodHandle are in module java.base of loader 'bootstrap')
//   meta-none: java.lang.BootstrapMethodError / java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(Lookup,String,MethodType,MethodType,MethodHandle,MethodType)CallSite to (Lookup,String,MethodType)Object
//   alt-short: java.lang.BootstrapMethodError / java.lang.IllegalArgumentException: missing argument
//   alt-extra: java.lang.BootstrapMethodError / java.lang.IllegalArgumentException: too many arguments
//   alt-flags-type: java.lang.BootstrapMethodError / java.lang.IllegalArgumentException: argument has wrong type
//   alt-negative: java.lang.BootstrapMethodError / java.lang.IllegalArgumentException: negative argument count
//   ok-alt: supplier one
//   arg-unbox: java.lang.BootstrapMethodError / java.lang.invoke.LambdaConversionException: Type mismatch for lambda argument 0: class java.lang.Object is not convertible to int
//   arg-narrow: java.lang.BootstrapMethodError / java.lang.invoke.LambdaConversionException: Type mismatch for lambda argument 0: int is not convertible to short
//   arg-iface: java.lang.BootstrapMethodError / java.lang.invoke.LambdaConversionException: Type mismatch for lambda argument 0: interface java.util.List is not convertible to class java.lang.String
//   ok-arg-widen: function long 5
//   ret-void: java.lang.BootstrapMethodError / java.lang.invoke.LambdaConversionException: Type mismatch for lambda return: void is not convertible to class java.lang.String
//   ret-narrow: java.lang.BootstrapMethodError / java.lang.invoke.LambdaConversionException: Type mismatch for lambda return: long is not convertible to int
//   ok-ret-box: supplier 7
//   captured: java.lang.BootstrapMethodError / java.lang.invoke.LambdaConversionException: Type mismatch in captured lambda parameter 0: expecting class java.lang.String, found class java.lang.Object
//   receiver: java.lang.BootstrapMethodError / java.lang.invoke.LambdaConversionException: Invalid receiver type class java.lang.String; not a subtype of implementation type class L4W37LambdaSiteValidation$Impl
//   dyn-arity: java.lang.BootstrapMethodError / java.lang.invoke.LambdaConversionException: Incorrect number of parameters for static method invokeStatic L4W37LambdaSiteValidation$Impl.one:()String; 1 dynamic parameters, 0 functional interface method parameters
//   dyn-param: java.lang.BootstrapMethodError / java.lang.invoke.LambdaConversionException: Type mismatch for dynamic parameter 0: class java.lang.Object is not a subtype of class java.lang.String
//   dyn-return: java.lang.BootstrapMethodError / java.lang.invoke.LambdaConversionException: Type mismatch for lambda expected return: class java.lang.Object is not convertible to class java.lang.String
//   sam-arity: java.lang.BootstrapMethodError / java.lang.invoke.LambdaConversionException: Incorrect number of parameters for static method invokeStatic L4W37LambdaSiteValidation$Impl.one:()String; 0 captured parameters, 1 functional interface method parameters, 0 implementation parameters
//   inherited: java.lang.BootstrapMethodError / java.lang.invoke.LambdaConversionException: Incorrect number of parameters for instance method invokeVirtual L4W37LambdaSiteValidation$Base.baseMethod:(int)String; 0 captured parameters, 0 functional interface method parameters, 2 implementation parameters
//   ok-meta: supplier one
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.DirectMethodHandleDesc;
import java.lang.constant.DynamicCallSiteDesc;
import java.lang.constant.MethodHandleDesc;
import java.lang.constant.MethodTypeDesc;
import java.lang.reflect.InvocationTargetException;
import java.util.function.Function;
import java.util.function.IntFunction;
import java.util.function.IntSupplier;
import java.util.function.Supplier;

public class L4W37LambdaSiteValidation {
    public static class Base {
        public String baseMethod(int x) {
            return "base";
        }
    }

    public static class Impl extends Base {
        public static String one() {
            return "one";
        }

        public static void voidOne() {
        }

        public static long retLong() {
            return 7L;
        }

        public static int retInt() {
            return 7;
        }

        public static String takesInt(int x) {
            return "int " + x;
        }

        public static String takesLong(long x) {
            return "long " + x;
        }

        public static String takesShort(short x) {
            return "short " + x;
        }

        public static String takesObj(Object x) {
            return "obj " + x;
        }

        public static String takesString(String x) {
            return "str " + x;
        }

        public static String capObj(Object x) {
            return "cap " + x;
        }

        public String inst() {
            return "inst";
        }
    }

    static final ClassDesc MT = ClassDesc.of("java.lang.invoke.MethodType");
    static final ClassDesc MH = ClassDesc.of("java.lang.invoke.MethodHandle");
    static final ClassDesc LOOKUP = ClassDesc.of("java.lang.invoke.MethodHandles$Lookup");
    static final ClassDesc CALLSITE = ClassDesc.of("java.lang.invoke.CallSite");
    static final ClassDesc LMF_CLASS = ClassDesc.of("java.lang.invoke.LambdaMetafactory");
    static final DirectMethodHandleDesc META = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC,
            LMF_CLASS, "metafactory",
            MethodTypeDesc.of(CALLSITE, LOOKUP, ConstantDescs.CD_String, MT, MT, MH, MT));
    static final DirectMethodHandleDesc ALT = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC,
            LMF_CLASS, "altMetafactory",
            MethodTypeDesc.of(CALLSITE, LOOKUP, ConstantDescs.CD_String, MT, ConstantDescs.CD_Object.arrayType()));

    static final ClassDesc IMPL = ClassDesc.of(Impl.class.getName());
    static final ClassDesc SUPPLIER = ClassDesc.of("java.util.function.Supplier");
    static final ClassDesc FUNCTION = ClassDesc.of("java.util.function.Function");
    static final ClassDesc INT_FUNCTION = ClassDesc.of("java.util.function.IntFunction");
    static final ClassDesc INT_SUPPLIER = ClassDesc.of("java.util.function.IntSupplier");
    static final ClassDesc OBJ = ConstantDescs.CD_Object;
    static final ClassDesc STR = ConstantDescs.CD_String;

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W37LambdaSiteValidation.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static final Loader LOADER = new Loader();

    static MethodTypeDesc mt(ClassDesc ret, ClassDesc... params) {
        return MethodTypeDesc.of(ret, params);
    }

    static DirectMethodHandleDesc stat(String name, MethodTypeDesc type) {
        return MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, IMPL, name, type);
    }

    static String row(String name, DirectMethodHandleDesc bsm, String samName, MethodTypeDesc factory,
            Object[] callArgs, ConstantDesc... staticArgs) {
        DynamicCallSiteDesc site = DynamicCallSiteDesc.of(bsm, samName, factory, staticArgs);
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
        Object r;
        try {
            Class<?>[] ps = new Class<?>[factory.parameterCount()];
            // `make`'s parameters are the factory's reference types.
            for (int i = 0; i < ps.length; i++) {
                ps[i] = factory.parameterType(i).equals(STR) ? String.class : Object.class;
            }
            r = LOADER.define(name, b).getMethod("make", ps).invoke(null, callArgs);
        } catch (InvocationTargetException e) {
            Throwable t = e.getCause();
            Throwable c = t.getCause();
            return t.getClass().getName() + (c == null ? ": " + t.getMessage()
                    : " / " + c.getClass().getName() + ": " + c.getMessage());
        } catch (Throwable t) {
            return "setup: " + t;
        }
        try {
            if (r instanceof Supplier<?> s) {
                return "supplier " + s.get();
            }
            if (r instanceof IntSupplier s) {
                return "int-supplier " + s.getAsInt();
            }
            if (r instanceof IntFunction<?> f) {
                return "int-function " + f.apply(5);
            }
            if (r instanceof Function<?, ?> f) {
                @SuppressWarnings("unchecked")
                Function<Object, Object> g = (Function<Object, Object>) f;
                return "function " + g.apply(5);
            }
            return "made " + r;
        } catch (Throwable t) {
            return "call: " + t;
        }
    }

    // HotSpot resolves every type of the three `MethodType`s before the
    // factory runs; CratonVM decides only over loaded classes, so the probe
    // loads its functional interfaces first (no difference on HotSpot).
    static final Class<?>[] PRELOAD = {Supplier.class, Function.class, IntFunction.class, IntSupplier.class,
            java.util.List.class};

    public static void main(String[] args) {
        if (PRELOAD.length != 5) {
            throw new AssertionError();
        }
        Object[] none = new Object[0];
        MethodTypeDesc toStr = mt(STR);
        MethodTypeDesc toObj = mt(OBJ);
        MethodTypeDesc objToObj = mt(OBJ, OBJ);
        MethodTypeDesc supplierType = mt(SUPPLIER);
        MethodTypeDesc functionType = mt(FUNCTION);

        System.out.println("meta-two: " + row("LvMetaTwo", META, "get", supplierType, none,
                toObj, stat("one", toStr)));
        System.out.println("meta-four: " + row("LvMetaFour", META, "get", supplierType, none,
                toObj, stat("one", toStr), toStr, toStr));
        System.out.println("meta-seven: " + row("LvMetaSeven", META, "get", supplierType, none,
                toObj, stat("one", toStr), toStr, toStr, toStr, toStr, toStr));
        System.out.println("meta-arg0-string: " + row("LvMetaArg0Str", META, "get", supplierType, none,
                "x", stat("one", toStr), toStr));
        System.out.println("meta-arg1-type: " + row("LvMetaArg1Type", META, "get", supplierType, none,
                toObj, toStr, toStr));
        System.out.println("meta-none: " + row("LvMetaNone", META, "get", supplierType, none));
        System.out.println("alt-short: " + row("LvAltShort", ALT, "get", supplierType, none,
                toObj, stat("one", toStr), toStr));
        System.out.println("alt-extra: " + row("LvAltExtra", ALT, "get", supplierType, none,
                toObj, stat("one", toStr), toStr, 0, 7));
        System.out.println("alt-flags-type: " + row("LvAltFlagsType", ALT, "get", supplierType, none,
                toObj, stat("one", toStr), toStr, "x"));
        System.out.println("alt-negative: " + row("LvAltNeg", ALT, "get", supplierType, none,
                toObj, stat("one", toStr), toStr, 2, -1));
        System.out.println("ok-alt: " + row("LvOkAlt", ALT, "get", supplierType, none,
                toObj, stat("one", toStr), toStr, 0));
        System.out.println("arg-unbox: " + row("LvArgUnbox", META, "apply", functionType, none,
                objToObj, stat("takesInt", mt(STR, ConstantDescs.CD_int)), objToObj));
        System.out.println("arg-narrow: " + row("LvArgNarrow", META, "apply", mt(INT_FUNCTION), none,
                mt(OBJ, ConstantDescs.CD_int), stat("takesShort", mt(STR, ConstantDescs.CD_short)),
                mt(OBJ, ConstantDescs.CD_int)));
        System.out.println("arg-iface: " + row("LvArgIface", META, "apply", functionType, none,
                objToObj, stat("takesString", mt(STR, STR)), mt(OBJ, ClassDesc.of("java.util.List"))));
        System.out.println("ok-arg-widen: " + row("LvOkArgWiden", META, "apply", functionType, none,
                objToObj, stat("takesLong", mt(STR, ConstantDescs.CD_long)),
                mt(OBJ, ConstantDescs.CD_Integer)));
        System.out.println("ret-void: " + row("LvRetVoid", META, "get", supplierType, none,
                toObj, stat("voidOne", mt(ConstantDescs.CD_void)), toStr));
        System.out.println("ret-narrow: " + row("LvRetNarrow", META, "getAsInt", mt(INT_SUPPLIER), none,
                mt(ConstantDescs.CD_int), stat("retLong", mt(ConstantDescs.CD_long)), mt(ConstantDescs.CD_int)));
        System.out.println("ok-ret-box: " + row("LvOkRetBox", META, "get", supplierType, none,
                toObj, stat("retInt", mt(ConstantDescs.CD_int)), mt(ConstantDescs.CD_Integer)));
        System.out.println("captured: " + row("LvCaptured", META, "get", mt(SUPPLIER, STR), new Object[] {"c"},
                toObj, stat("capObj", mt(STR, OBJ)), toStr));
        System.out.println("receiver: " + row("LvReceiver", META, "get", mt(SUPPLIER, STR), new Object[] {"c"},
                toObj, MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.VIRTUAL, IMPL, "inst", toStr), toStr));
        System.out.println("dyn-arity: " + row("LvDynArity", META, "get", supplierType, none,
                toObj, stat("one", toStr), mt(STR, OBJ)));
        System.out.println("dyn-param: " + row("LvDynParam", META, "apply", functionType, none,
                mt(OBJ, STR), stat("takesObj", mt(STR, OBJ)), objToObj));
        System.out.println("dyn-return: " + row("LvDynReturn", META, "get", supplierType, none,
                toStr, stat("one", toStr), toObj));
        System.out.println("sam-arity: " + row("LvSamArity", META, "get", supplierType, none,
                objToObj, stat("one", toStr), toStr));
        System.out.println("inherited: " + row("LvInherited", META, "get", supplierType, none,
                toObj, MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.VIRTUAL, IMPL, "baseMethod",
                        mt(STR, ConstantDescs.CD_int)), toStr));
        System.out.println("ok-meta: " + row("LvOkMeta", META, "get", supplierType, none,
                toObj, stat("one", toStr), toStr));
    }
}
