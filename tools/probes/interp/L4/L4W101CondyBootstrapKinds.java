// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 101, lane L4 (page
// `docs/internal/fixed-bugs/interpreter-i101-L4-condy-bootstrap-of-a-non-static-kind-FIXED-20261015.md`):
// a `CONSTANT_Dynamic` whose bootstrap `CONSTANT_MethodHandle` is not
// `REF_invokeStatic`.
//
// JDK 25 hands any direct handle to `ConstantBootstraps.makeConstant`:
//   * `REF_newInvokeSpecial` of a constructor `(Lookup,String,Class)`: the
//     handle's type is `(Lookup,String,Class)Made`, the check passes, and the
//     constant is a NEW `Made` (one per entry, cached by the entry);
//   * `REF_invokeVirtual` of an instance method: the handle's type leads with
//     the receiver class, so `makeConstant` refuses it with
//     `BootstrapMethodError: Invalid bootstrap method declared for resolving a
//     dynamic constant: MethodHandle(L4W101CondyBootstrapKinds,Lookup,String,Class)Object`.
// CratonVM refuses both with its own `BootstrapMethodError` text ("has
// reference kind N; only a static bootstrap method is supported").
//
// Run: javac -d . L4W101CondyBootstrapKinds.java
//      java -cp . L4W101CondyBootstrapKinds
//      cratonvm --jdk-only -cp . L4W101CondyBootstrapKinds
//      cratonvm --compatible -cp . L4W101CondyBootstrapKinds
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25; derived from the sources)
// ctor Made[c] same=true
// virtual java.lang.BootstrapMethodError: Invalid bootstrap method declared for resolving a dynamic constant: MethodHandle(L4W101CondyBootstrapKinds,Lookup,String,Class)Object
// made 1
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

public class L4W101CondyBootstrapKinds {
    static final ClassDesc SELF = ClassDesc.of("L4W101CondyBootstrapKinds");
    static final ClassDesc MADE = ClassDesc.of("L4W101CondyBootstrapKinds$Made");
    static final ClassDesc LOOKUP = ClassDesc.of("java.lang.invoke.MethodHandles$Lookup");
    static final ClassDesc SUPPLIER = ClassDesc.of("java.util.function.Supplier");
    static int made;

    public static final class Made {
        final String name;

        public Made(MethodHandles.Lookup l, String n, Class<?> t) {
            made++;
            this.name = n;
        }

        @Override
        public String toString() {
            return "Made[" + name + "]";
        }
    }

    public Object vb(MethodHandles.Lookup l, String n, Class<?> t) {
        return "virtual";
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W101CondyBootstrapKinds.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static int classes;

    @SuppressWarnings("unchecked")
    static Supplier<Object> make(DirectMethodHandleDesc handle) throws Exception {
        String name = "CdyK" + (classes++);
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

    static String run(Supplier<Object> s) {
        try {
            return String.valueOf(s.get());
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    public static void main(String[] args) throws Exception {
        Supplier<Object> ctor = make(MethodHandleDesc.ofConstructor(MADE, LOOKUP, ConstantDescs.CD_String,
                ConstantDescs.CD_Class));
        Object first = null;
        String shown;
        try {
            first = ctor.get();
            shown = first + " same=" + (ctor.get() == first);
        } catch (Throwable t) {
            shown = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println("ctor " + shown);
        Supplier<Object> virtual = make(MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.VIRTUAL, SELF, "vb",
                MethodTypeDesc.of(ConstantDescs.CD_Object, LOOKUP, ConstantDescs.CD_String, ConstantDescs.CD_Class)));
        System.out.println("virtual " + run(virtual));
        System.out.println("made " + made);
    }
}
