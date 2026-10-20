// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 87, lane L4 (page
// `docs/internal/fixed-bugs/interpreter-i86-L4-lambda-and-switch-residuals-FIXED-20261120.md`,
// residual 1): an `altMetafactory` site with FLAG_MARKERS naming a marker that
// extends `Serializable` but WITHOUT FLAG_SERIALIZABLE is "accidentally
// serializable" in JDK 25's `InnerClassLambdaMetafactory`
// (`accidentallySerializable |= !isSerializable && Serializable.class.isAssignableFrom(i)`
// per alt interface): the proxy IS a Serializable, its class declares the
// hostile `writeObject` / `readObject` and no `writeReplace`, and writing it
// throws `NotSerializableException: Non-serializable lambda`. The site is
// generated with the java.lang.classfile API (javac always passes the flag).
//
// Run: javac -d . L4W87SerializableMarker.java
//      cratonvm --jdk-only [--nojit] -cp . L4W87SerializableMarker
//      cratonvm --compatible [--nojit] -cp . L4W87SerializableMarker
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// interfaces=[Runnable, SerMarker] serializable=true
// private final void readObject(ObjectInputStream)
// private final void writeObject(ObjectOutputStream)
// public void run()
// write=java.io.NotSerializableException: Non-serializable lambda
// run=ran
// HOTSPOT_EXPECTED_END
import java.io.ByteArrayOutputStream;
import java.io.ObjectOutputStream;
import java.io.Serializable;
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.DirectMethodHandleDesc;
import java.lang.constant.DynamicCallSiteDesc;
import java.lang.constant.MethodHandleDesc;
import java.lang.constant.MethodTypeDesc;
import java.lang.reflect.Method;
import java.lang.reflect.Modifier;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;

public class L4W87SerializableMarker {
    public interface SerMarker extends Serializable {
    }

    static String ran = "no";

    public static void body() {
        ran = "ran";
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W87SerializableMarker.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    public static void main(String[] args) throws Throwable {
        ClassDesc self = ClassDesc.of("L4W87SerializableMarker");
        ClassDesc runnable = ClassDesc.of("java.lang.Runnable");
        DirectMethodHandleDesc bsm = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC,
                ClassDesc.of("java.lang.invoke.LambdaMetafactory"), "altMetafactory",
                MethodTypeDesc.of(ConstantDescs.CD_CallSite, ConstantDescs.CD_MethodHandles_Lookup,
                        ConstantDescs.CD_String, ConstantDescs.CD_MethodType,
                        ConstantDescs.CD_Object.arrayType()));
        MethodTypeDesc voidType = MethodTypeDesc.of(ConstantDescs.CD_void);
        DirectMethodHandleDesc impl = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, self, "body",
                voidType);
        // FLAG_MARKERS (2), one marker.
        DynamicCallSiteDesc site = DynamicCallSiteDesc.of(bsm, "run", MethodTypeDesc.of(runnable), voidType, impl,
                voidType, 2, 1, ClassDesc.of("L4W87SerializableMarker$SerMarker"));
        byte[] bytes = ClassFile.of().build(ClassDesc.of("L4W87MarkerSite"), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("make", MethodTypeDesc.of(runnable), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.invokedynamic(site).areturn());
        });
        Runnable r = (Runnable) new Loader().define("L4W87MarkerSite", bytes).getMethod("make").invoke(null);
        Class<?> c = r.getClass();
        List<String> ifaces = new ArrayList<>();
        for (Class<?> i : c.getInterfaces()) {
            ifaces.add(i.getSimpleName());
        }
        System.out.println("interfaces=" + ifaces + " serializable=" + (r instanceof Serializable));
        List<String> methods = new ArrayList<>();
        for (Method m : c.getDeclaredMethods()) {
            StringBuilder sb = new StringBuilder(Modifier.toString(m.getModifiers() & Modifier.methodModifiers()))
                    .append(' ').append(m.getReturnType().getSimpleName()).append(' ').append(m.getName()).append('(');
            Class<?>[] ps = m.getParameterTypes();
            for (int i = 0; i < ps.length; i++) {
                sb.append(i > 0 ? ", " : "").append(ps[i].getSimpleName());
            }
            methods.add(sb.append(')').toString());
        }
        Collections.sort(methods);
        methods.forEach(System.out::println);
        String out;
        try (ObjectOutputStream oos = new ObjectOutputStream(new ByteArrayOutputStream())) {
            oos.writeObject(r);
            out = "ok";
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println("write=" + out);
        r.run();
        System.out.println("run=" + ran);
    }
}
