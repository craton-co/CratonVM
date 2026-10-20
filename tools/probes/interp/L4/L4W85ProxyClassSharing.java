// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 85, lane L4: why `L4W85MethodHandleProxies` row
// `shared-class` answered `false` on CratonVM (two `asInterfaceInstance`
// calls for one interface spun TWO proxy classes). JDK 25's
// `MethodHandleProxies.getProxyClass` keeps the class in
// `PROXIES` (a `ClassValue<WeakReferenceHolder<Class<?>>>`) behind a
// `WeakReference`, so a second class is spun only when (a) the ClassValue
// recomputes, or (b) the weak referent -- the hidden class's mirror -- was
// cleared while a proxy instance (and so the class) was still alive. Each row
// isolates one of those mechanisms with plain Java; HotSpot answers `true`
// for every row.
//
//   classvalue-once         an own ClassValue computes once per class
//   classvalue-twice-gc     ... and still once across System.gc()
//   hidden-weak-instance    a WeakReference to a non-strong hidden class's
//                           mirror survives System.gc() while an instance lives
//   hidden-weak-mirror-only ... and while only its Class object is held
//   proxy-weak-instance     the same for a MethodHandleProxies proxy class
//   proxy-shared-after-gc   asInterfaceInstance after System.gc() reuses the class
//   module-named            the proxy class is in a named jdk.MHProxyN module
//                           (wave-85 fix, `record_layer_module`)
//
// Run: javac -d . L4W85ProxyClassSharing.java
//      cratonvm --jdk-only [--nojit] -cp . L4W85ProxyClassSharing
//      cratonvm --compatible [--nojit] -cp . L4W85ProxyClassSharing
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// classvalue-once: true
// classvalue-twice-gc: true
// hidden-weak-instance: true
// hidden-weak-mirror-only: true
// proxy-weak-instance: true
// proxy-shared-after-gc: true
// module-named: true
// HOTSPOT_EXPECTED_END
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandleProxies;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.ref.WeakReference;

public class L4W85ProxyClassSharing {
    public interface Greeter {
        String greet(String who);
    }

    public static String hi(String who) {
        return "hi " + who;
    }

    static int computes;

    static final ClassValue<Object> CV = new ClassValue<>() {
        @Override
        protected Object computeValue(Class<?> type) {
            computes++;
            return new Object();
        }
    };

    /** A non-strong hidden class nested in this class's package, with a no-arg constructor. */
    static Class<?> defineHidden() throws Exception {
        byte[] bytes = ClassFile.of().build(ClassDesc.of("L4W85Hidden"), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody(ConstantDescs.INIT_NAME, MethodTypeDesc.of(ConstantDescs.CD_void),
                    ClassFile.ACC_PUBLIC, code -> code.aload(0)
                            .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                    MethodTypeDesc.of(ConstantDescs.CD_void))
                            .return_());
        });
        return MethodHandles.lookup().defineHiddenClass(bytes, true).lookupClass();
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = t.toString();
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) throws Throwable {
        row("classvalue-once", () -> {
            Object a = CV.get(Greeter.class);
            Object b = CV.get(Greeter.class);
            return a == b && computes == 1;
        });
        row("classvalue-twice-gc", () -> {
            Object a = CV.get(Greeter.class);
            System.gc();
            Object b = CV.get(Greeter.class);
            return a == b && computes == 1;
        });
        row("hidden-weak-instance", () -> {
            Object instance = defineHidden().getConstructor().newInstance();
            WeakReference<Class<?>> w = new WeakReference<>(instance.getClass());
            System.gc();
            boolean alive = w.get() == instance.getClass();
            return alive && instance != null;
        });
        row("hidden-weak-mirror-only", () -> {
            Class<?> c = defineHidden();
            WeakReference<Class<?>> w = new WeakReference<>(c);
            System.gc();
            return w.get() == c;
        });
        MethodHandle hi = MethodHandles.lookup().findStatic(L4W85ProxyClassSharing.class, "hi",
                MethodType.methodType(String.class, String.class));
        Greeter g = MethodHandleProxies.asInterfaceInstance(Greeter.class, hi);
        row("proxy-weak-instance", () -> {
            WeakReference<Class<?>> w = new WeakReference<>(g.getClass());
            System.gc();
            return w.get() == g.getClass();
        });
        row("proxy-shared-after-gc", () -> {
            System.gc();
            Greeter g2 = MethodHandleProxies.asInterfaceInstance(Greeter.class, hi);
            return g2.getClass() == g.getClass() && "hi x".equals(g2.greet("x"));
        });
        row("module-named", () -> g.getClass().getModule().isNamed()
                && String.valueOf(g.getClass().getModule().getName()).startsWith("jdk.MHProxy"));
    }
}
