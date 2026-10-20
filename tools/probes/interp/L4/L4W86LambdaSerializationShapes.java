// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 86, lane L4: what a lambda proxy class declares
// for each `LambdaMetafactory` flag shape, against JDK 25's
// `InnerClassLambdaMetafactory.generateInnerClass`:
//   interfaces  the functional interface first, then each marker in order
//               (`LinkedHashSet`), `Serializable` appended only for
//               FLAG_SERIALIZABLE when no interface already extends it;
//   methods     the SAM method (`public`), each FLAG_BRIDGES bridge
//               (`public` + ACC_BRIDGE, which `Modifier.toString` spells
//               `volatile`), then `private final writeReplace()` for
//               FLAG_SERIALIZABLE, or -- an interface that extends
//               `Serializable` WITHOUT the flag ("accidentally serializable")
//               -- `private final writeObject` / `readObject`, each `throws
//               NotSerializableException`, whose body throws
//               `NotSerializableException("Non-serializable lambda")`;
//   class       ACC_FINAL | ACC_SYNTHETIC, hidden.
// The `accidental` row is a plain `metafactory` site over a `Serializable`
// interface, generated with the java.lang.classfile API (javac always passes
// FLAG_SERIALIZABLE there). The `bridges` row is javac's lambda over a
// covariant SAM: javac writes the `Object m()` bridge into the interface
// itself (a default method), so its site passes no FLAG_BRIDGES and the spun
// class declares no bridge (measured on HotSpot 25.0.3). The `alt-bridges`
// row is a generated `altMetafactory` site that DOES pass FLAG_BRIDGES with
// `()Object`. Methods are printed sorted, as
// "modifiers return name(params)[ throws X][ bridge]".
//
// Run: javac -d . L4W86LambdaSerializationShapes.java
//      cratonvm --jdk-only [--nojit] -cp . L4W86LambdaSerializationShapes
//      cratonvm --compatible [--nojit] -cp . L4W86LambdaSerializationShapes
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// ser-fn: class=final synthetic=true hidden=true interfaces=[SerFn] serializable=true
// ser-fn:   private final Object writeReplace()
// ser-fn:   public void run()
// ser-fn: write=ok
// intersection: class=final synthetic=true hidden=true interfaces=[Runnable, Serializable] serializable=true
// intersection:   private final Object writeReplace()
// intersection:   public void run()
// intersection: write=ok
// markers: class=final synthetic=true hidden=true interfaces=[Runnable, Marker] serializable=false
// markers:   public void run()
// markers: write=java.io.NotSerializableException
// bridges: class=final synthetic=true hidden=true interfaces=[StrM] serializable=false
// bridges:   public String m()
// bridges: call=s bridge-call=s
// alt-bridges: class=final synthetic=true hidden=true interfaces=[StrM] serializable=false
// alt-bridges:   public String m()
// alt-bridges:   public volatile Object m() bridge
// alt-bridges: call=s bridge-call=s
// accidental: class=final synthetic=true hidden=true interfaces=[SerFn] serializable=true
// accidental:   private final void readObject(ObjectInputStream) throws NotSerializableException
// accidental:   private final void writeObject(ObjectOutputStream) throws NotSerializableException
// accidental:   public void run()
// accidental: write=java.io.NotSerializableException: Non-serializable lambda
// accidental: run=ran
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
import java.util.Arrays;
import java.util.Collections;
import java.util.List;

public class L4W86LambdaSerializationShapes {
    public interface SerFn extends Runnable, Serializable {
    }

    public interface Marker {
    }

    public interface ObjM {
        Object m();
    }

    public interface StrM extends ObjM {
        String m();
    }

    static String ran = "no";

    public static void body() {
        ran = "ran";
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W86LambdaSerializationShapes.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    /** A plain `metafactory` site producing a SerFn over `body()`. */
    static SerFn accidental() throws Throwable {
        ClassDesc self = ClassDesc.of("L4W86LambdaSerializationShapes");
        ClassDesc serFn = ClassDesc.of("L4W86LambdaSerializationShapes$SerFn");
        DirectMethodHandleDesc bsm = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC,
                ClassDesc.of("java.lang.invoke.LambdaMetafactory"), "metafactory",
                MethodTypeDesc.of(ConstantDescs.CD_CallSite, ConstantDescs.CD_MethodHandles_Lookup,
                        ConstantDescs.CD_String, ConstantDescs.CD_MethodType, ConstantDescs.CD_MethodType,
                        ConstantDescs.CD_MethodHandle, ConstantDescs.CD_MethodType));
        MethodTypeDesc voidType = MethodTypeDesc.of(ConstantDescs.CD_void);
        DirectMethodHandleDesc impl = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, self, "body",
                voidType);
        DynamicCallSiteDesc site = DynamicCallSiteDesc.of(bsm, "run", MethodTypeDesc.of(serFn), voidType, impl,
                voidType);
        byte[] bytes = ClassFile.of().build(ClassDesc.of("L4W86Accidental"), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("make", MethodTypeDesc.of(serFn), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.invokedynamic(site).areturn());
        });
        Class<?> c = new Loader().define("L4W86Accidental", bytes);
        return (SerFn) c.getMethod("make").invoke(null);
    }

    public static String str() {
        return "s";
    }

    /** An `altMetafactory` site with FLAG_BRIDGES (4) and one bridge `()Object`, producing a StrM. */
    static StrM altBridges() throws Throwable {
        ClassDesc self = ClassDesc.of("L4W86LambdaSerializationShapes");
        ClassDesc strM = ClassDesc.of("L4W86LambdaSerializationShapes$StrM");
        DirectMethodHandleDesc bsm = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC,
                ClassDesc.of("java.lang.invoke.LambdaMetafactory"), "altMetafactory",
                MethodTypeDesc.of(ConstantDescs.CD_CallSite, ConstantDescs.CD_MethodHandles_Lookup,
                        ConstantDescs.CD_String, ConstantDescs.CD_MethodType,
                        ConstantDescs.CD_Object.arrayType()));
        MethodTypeDesc strType = MethodTypeDesc.of(ConstantDescs.CD_String);
        DirectMethodHandleDesc impl = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, self, "str",
                strType);
        DynamicCallSiteDesc site = DynamicCallSiteDesc.of(bsm, "m", MethodTypeDesc.of(strM), strType, impl,
                strType, 4, 1, MethodTypeDesc.of(ConstantDescs.CD_Object));
        byte[] bytes = ClassFile.of().build(ClassDesc.of("L4W86AltBridges"), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("make", MethodTypeDesc.of(strM), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.invokedynamic(site).areturn());
        });
        Class<?> c = new Loader().define("L4W86AltBridges", bytes);
        return (StrM) c.getMethod("make").invoke(null);
    }

    static String describe(Method m) {
        StringBuilder sb = new StringBuilder();
        sb.append(Modifier.toString(m.getModifiers() & Modifier.methodModifiers() | (m.isBridge() ? Modifier.VOLATILE : 0)))
                .append(' ').append(m.getReturnType().getSimpleName()).append(' ').append(m.getName()).append('(');
        Class<?>[] ps = m.getParameterTypes();
        for (int i = 0; i < ps.length; i++) {
            if (i > 0) {
                sb.append(", ");
            }
            sb.append(ps[i].getSimpleName());
        }
        sb.append(')');
        Class<?>[] ex = m.getExceptionTypes();
        if (ex.length > 0) {
            sb.append(" throws");
            for (Class<?> e : ex) {
                sb.append(' ').append(e.getSimpleName());
            }
        }
        if (m.isBridge()) {
            sb.append(" bridge");
        }
        return sb.toString();
    }

    static void shape(String name, Object lambda, boolean write) {
        Class<?> c = lambda.getClass();
        List<String> ifaces = new ArrayList<>();
        for (Class<?> i : c.getInterfaces()) {
            ifaces.add(i.getSimpleName());
        }
        System.out.println(name + ": class=" + Modifier.toString(c.getModifiers()) + " synthetic=" + c.isSynthetic()
                + " hidden=" + c.isHidden() + " interfaces=" + ifaces + " serializable="
                + (lambda instanceof Serializable));
        List<String> methods = new ArrayList<>();
        for (Method m : c.getDeclaredMethods()) {
            methods.add(describe(m));
        }
        Collections.sort(methods);
        for (String m : methods) {
            System.out.println(name + ":   " + m);
        }
        if (write) {
            String out;
            try (ObjectOutputStream oos = new ObjectOutputStream(new ByteArrayOutputStream())) {
                oos.writeObject(lambda);
                out = "ok";
            } catch (Throwable t) {
                out = t.getClass().getName() + (t.getMessage() != null && !(t.getMessage().contains("$$Lambda"))
                        ? ": " + t.getMessage() : "");
            }
            System.out.println(name + ": write=" + out);
        }
    }

    public static void main(String[] args) throws Throwable {
        SerFn serFn = () -> { };
        shape("ser-fn", serFn, true);
        Runnable inter = (Runnable & Serializable) () -> { };
        shape("intersection", inter, true);
        Runnable marked = (Runnable & Marker) () -> { };
        shape("markers", marked, true);
        StrM strM = () -> "s";
        shape("bridges", strM, false);
        ObjM asObj = strM;
        System.out.println("bridges: call=" + strM.m() + " bridge-call=" + asObj.m());
        StrM alt = altBridges();
        shape("alt-bridges", alt, false);
        ObjM altObj = alt;
        System.out.println("alt-bridges: call=" + alt.m() + " bridge-call=" + altObj.m());
        SerFn acc = accidental();
        shape("accidental", acc, true);
        acc.run();
        System.out.println("accidental: run=" + ran);
    }
}
