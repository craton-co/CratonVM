// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 101, lane L4: a `CONSTANT_Dynamic` whose bootstrap
// method's type has fewer than two parameters, or a first parameter that is
// not `MethodHandles.Lookup`.
//
// JDK 25 `ConstantBootstraps.makeConstant` (the JVM's upcall for every dynamic
// constant, through `MethodHandleNatives.linkDynamicConstant`) refuses such a
// bootstrap before invoking it:
//   BootstrapMethodError("Invalid bootstrap method declared for resolving a
//   dynamic constant: " + bootstrapMethod)
// with no cause; a second `ldc` of the entry fails the same way (JVMS 5.4.3).
// CratonVM (both modes) invoked the bootstrap anyway: `(Object,String,Class)`
// linked, `(Object...)` collected every argument and linked, `(Lookup)` was an
// arity `WrongMethodTypeException` wrapped in `BootstrapMethodError`.
// (An `invokedynamic` has no such rule: `CallSite.makeSite` invokes an
// `(Object,String,MethodType)` bootstrap, so only the constant is checked.)
//
// Each row generates (java.lang.classfile) a class whose `get()` is one `ldc`
// of an `Object`-typed dynamic constant over the named bootstrap.
//
// Run: javac -d . L4W101CondyBootstrapNotLookup.java
//      java -cp . L4W101CondyBootstrapNotLookup
//      cratonvm --jdk-only -cp . L4W101CondyBootstrapNotLookup
//      cratonvm --compatible -cp . L4W101CondyBootstrapNotLookup
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25; derived from the sources)
// lookup-first ok
// object-first java.lang.BootstrapMethodError: Invalid bootstrap method declared for resolving a dynamic constant: MethodHandle(Object,String,Class)Object cause=null
// object-first again java.lang.BootstrapMethodError: Invalid bootstrap method declared for resolving a dynamic constant: MethodHandle(Object,String,Class)Object
// varargs java.lang.BootstrapMethodError: Invalid bootstrap method declared for resolving a dynamic constant: MethodHandle(Object[])Object cause=null
// varargs again java.lang.BootstrapMethodError: Invalid bootstrap method declared for resolving a dynamic constant: MethodHandle(Object[])Object
// lookup-only java.lang.BootstrapMethodError: Invalid bootstrap method declared for resolving a dynamic constant: MethodHandle(Lookup)Object cause=null
// lookup-only again java.lang.BootstrapMethodError: Invalid bootstrap method declared for resolving a dynamic constant: MethodHandle(Lookup)Object
// bootstrap-calls 1
// HOTSPOT_EXPECTED_END
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.DirectMethodHandleDesc;
import java.lang.constant.DynamicConstantDesc;
import java.lang.constant.MethodHandleDesc;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandles;
import java.util.function.Supplier;

public class L4W101CondyBootstrapNotLookup {
    static final ClassDesc SELF = ClassDesc.of("L4W101CondyBootstrapNotLookup");
    static final ClassDesc LOOKUP = ClassDesc.of("java.lang.invoke.MethodHandles$Lookup");
    static final ClassDesc SUPPLIER = ClassDesc.of("java.util.function.Supplier");
    static int calls;

    public static Object bOk(MethodHandles.Lookup l, String n, Class<?> t) {
        calls++;
        return "ok";
    }

    public static Object bObject(Object l, String n, Class<?> t) {
        calls++;
        return "linked";
    }

    public static Object bVarargs(Object... a) {
        calls++;
        return "linked";
    }

    public static Object bLookupOnly(MethodHandles.Lookup l) {
        calls++;
        return "linked";
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W101CondyBootstrapNotLookup.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static int classes;

    @SuppressWarnings("unchecked")
    static Supplier<Object> make(String bsm, MethodTypeDesc bsmType) throws Exception {
        String name = "CdyNL" + (classes++);
        DirectMethodHandleDesc handle =
                MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, SELF, bsm, bsmType);
        DynamicConstantDesc<Object> condy = DynamicConstantDesc.ofNamed(handle, "c", ConstantDescs.CD_Object);
        byte[] bytes = ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withInterfaceSymbols(SUPPLIER);
            cb.withMethodBody(ConstantDescs.INIT_NAME, MethodTypeDesc.of(ConstantDescs.CD_void),
                    ClassFile.ACC_PUBLIC,
                    code -> code.aload(0)
                            .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                    MethodTypeDesc.of(ConstantDescs.CD_void))
                            .return_());
            cb.withMethodBody("get", MethodTypeDesc.of(ConstantDescs.CD_Object), ClassFile.ACC_PUBLIC,
                    code -> code.ldc(condy).areturn());
        });
        return (Supplier<Object>) new Loader().define(name, bytes).getConstructor().newInstance();
    }

    static void row(String label, String bsm, MethodTypeDesc type) throws Exception {
        Supplier<Object> s = make(bsm, type);
        for (int i = 0; i < 2; i++) {
            String tag = i == 0 ? label : label + " again";
            try {
                System.out.println(tag + " " + s.get());
            } catch (Throwable t) {
                System.out.println(tag + " " + t.getClass().getName() + ": " + t.getMessage()
                        + (i == 0 ? " cause=" + t.getCause() : ""));
            }
        }
    }

    public static void main(String[] args) throws Exception {
        Supplier<Object> ok = make("bOk", MethodTypeDesc.of(ConstantDescs.CD_Object, LOOKUP,
                ConstantDescs.CD_String, ConstantDescs.CD_Class));
        System.out.println("lookup-first " + ok.get());
        row("object-first", "bObject", MethodTypeDesc.of(ConstantDescs.CD_Object, ConstantDescs.CD_Object,
                ConstantDescs.CD_String, ConstantDescs.CD_Class));
        row("varargs", "bVarargs", MethodTypeDesc.of(ConstantDescs.CD_Object, ConstantDescs.CD_Object.arrayType()));
        row("lookup-only", "bLookupOnly", MethodTypeDesc.of(ConstantDescs.CD_Object, LOOKUP));
        System.out.println("bootstrap-calls " + calls);
    }
}
