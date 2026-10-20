// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 107, lane L3 (page
// docs/internal/fixed-bugs/interpreter-i106-L3-constant-pool-member-resolvers-unregistered-FIXED-20261016.md):
// `jdk.internal.reflect.ConstantPool`'s member resolvers -- `getMethodAt0`,
// `getMethodAtIfLoaded0`, `getFieldAt0`, `getFieldAtIfLoaded0` -- were never
// registered (`UnsatisfiedLinkError`), and `getClassAt0` made no access check
// (HotSpot's `verify_constant_pool_resolve`: `IllegalAccessError`).
//
// The pool read is the nested `Target`'s (rows keep only references to the
// probe's own classes and `java/util/List`, sorted, so javac's pool order does
// not matter), plus one lambda proxy's (TypeTools' fallback: `getMethodAt` on
// the implementation reference).
//
// Rows:
//   method <owner>.<name><desc>: <getMethodAt> | ifLoaded=<getMethodAtIfLoaded>
//   field <owner>.<name>:<desc>: <getFieldAt> | ifLoaded=<getFieldAtIfLoaded>
//   (a member as `toString()` prints it, or the exception class and message)
//   never-*: before / at / after for an entry whose class is not loaded yet
//   err-*: the bounds / tag checks; inaccessible-*: the access check (module
//   hash printed as @H)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3; derived from jvm.cpp's
// get_method_at_helper / get_field_at_helper / ConstantPool::klass_at_impl and
// Reflection::verify_class_access_msg, and javac 25's pool for `Target`)
// field L3W107ConstantPoolMembers$Target.KO:Ljava/lang/Object;: public static final java.lang.Object L3W107ConstantPoolMembers$Konst.KO | ifLoaded=public static final java.lang.Object L3W107ConstantPoolMembers$Konst.KO
// field L3W107ConstantPoolMembers$Target.baseField:I: static int L3W107ConstantPoolMembers$Base.baseField | ifLoaded=static int L3W107ConstantPoolMembers$Base.baseField
// field L3W107ConstantPoolMembers$Target.counter:I: static int L3W107ConstantPoolMembers$Target.counter | ifLoaded=static int L3W107ConstantPoolMembers$Target.counter
// field L3W107ConstantPoolMembers$Target.inst:I: int L3W107ConstantPoolMembers$Target.inst | ifLoaded=int L3W107ConstantPoolMembers$Target.inst
// method L3W107ConstantPoolMembers$Base.<init>()V: L3W107ConstantPoolMembers$Base() | ifLoaded=L3W107ConstantPoolMembers$Base()
// method L3W107ConstantPoolMembers$Target.<init>()V: L3W107ConstantPoolMembers$Target() | ifLoaded=L3W107ConstantPoolMembers$Target()
// method L3W107ConstantPoolMembers$Target.inherited()V: java.lang.RuntimeException Unable to look up method in target class | ifLoaded=java.lang.RuntimeException Unable to look up method in target class
// method L3W107ConstantPoolMembers$Target.s()Ljava/lang/String;: static java.lang.String L3W107ConstantPoolMembers$Target.s() | ifLoaded=static java.lang.String L3W107ConstantPoolMembers$Target.s()
// method java/util/List.of(Ljava/lang/Object;)Ljava/util/List;: public static java.util.List java.util.List.of(java.lang.Object) | ifLoaded=public static java.util.List java.util.List.of(java.lang.Object)
// never-method before=null at=L3W107ConstantPoolMembers$NeverLoaded() after=L3W107ConstantPoolMembers$NeverLoaded()
// never-field before=null at=static int L3W107ConstantPoolMembers$NeverLoadedToo.f after=static int L3W107ConstantPoolMembers$NeverLoadedToo.f
// err-method-at-fieldref: java.lang.IllegalArgumentException Wrong type at constant pool index
// err-field-at-methodref: java.lang.IllegalArgumentException Wrong type at constant pool index
// err-field-at-index-0: java.lang.IllegalArgumentException Wrong type at constant pool index
// err-method-past-end: java.lang.IllegalArgumentException Constant pool index out of bounds
// err-field-negative: java.lang.IllegalArgumentException Constant pool index out of bounds
// inaccessible-class-at: java.lang.IllegalAccessError class L3W107ConstantPoolMembers$Target (in unnamed module @H) cannot access class jdk.internal.misc.VM (in module java.base) because module java.base does not export jdk.internal.misc to unnamed module @H
// inaccessible-method-at: java.lang.IllegalAccessError class L3W107ConstantPoolMembers$Target (in unnamed module @H) cannot access class jdk.internal.misc.VM (in module java.base) because module java.base does not export jdk.internal.misc to unnamed module @H
// inaccessible-method-if-loaded: null
// lambda-impl-method: private static void L3W107ConstantPoolMembers.lambda$main$0()
// lambda-this-class-is-proxy: true
// HOTSPOT_EXPECTED_END
//
// SETUP: `javac --add-exports java.base/jdk.internal.misc=ALL-UNNAMED L3W107ConstantPoolMembers.java`
// (the export is for COMPILING the never-run `VM.isBooted()` reference only;
// it must NOT be given at run time), then
//   java|cratonvm [--compatible] [--nojit] --add-exports java.base/jdk.internal.access=ALL-UNNAMED
//     --add-exports java.base/jdk.internal.reflect=ALL-UNNAMED -cp . L3W107ConstantPoolMembers
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;

public class L3W107ConstantPoolMembers {
    static class NeverLoaded {
    }

    static class NeverLoadedToo {
        static int f;
    }

    interface Konst {
        Object KO = new Object();
    }

    static class Base {
        static int baseField;

        Base() {
        }

        void inherited() {
        }
    }

    static class Target extends Base implements Konst {
        static int counter;
        int inst;

        Target() {
        }

        static String s() {
            return "x";
        }

        static void body() {
            counter++;
            baseField++;
            Object o = KO;
            Target t = new Target();
            t.inst = 1;
            t.inherited();
            List<Object> l = List.of(o);
            if (counter < 0) {
                // Never runs: the classes stay unloaded / the access check
                // is never made by bytecode.
                System.out.println(new NeverLoaded());
                System.out.println(NeverLoadedToo.f);
                System.out.println(jdk.internal.misc.VM.isBooted());
            }
            if (l.isEmpty()) {
                System.out.println(s());
            }
        }
    }

    static Object cp;
    static Class<?> cpc;

    static Object call(String name, int index) throws Throwable {
        Method m = cpc.getMethod(name, int.class);
        try {
            return m.invoke(cp, index);
        } catch (InvocationTargetException e) {
            throw e.getCause();
        }
    }

    static String show(String name, int index) {
        try {
            return String.valueOf(call(name, index));
        } catch (Throwable t) {
            String msg = String.valueOf(t.getMessage()).replaceAll("@0x[0-9a-fA-F]+", "@H");
            return t.getClass().getName() + " " + msg;
        }
    }

    public static void main(String[] args) throws Throwable {
        Target.body();
        Class<?> ss = Class.forName("jdk.internal.access.SharedSecrets");
        Object jla = ss.getMethod("getJavaLangAccess").invoke(null);
        Method getCp = Class.forName("jdk.internal.access.JavaLangAccess")
                .getMethod("getConstantPool", Class.class);
        cpc = Class.forName("jdk.internal.reflect.ConstantPool");
        cp = getCp.invoke(jla, Target.class);
        int size = (Integer) cpc.getMethod("getSize").invoke(cp);
        String self = L3W107ConstantPoolMembers.class.getName();
        List<String> rows = new ArrayList<>();
        int aFieldref = -1, aMethodref = -1, neverM = -1, neverF = -1, vmRef = -1;
        for (int i = 1; i < size; i++) {
            String tag = String.valueOf(call("getTagAt", i));
            if (!tag.equals("FIELDREF") && !tag.equals("METHODREF") && !tag.equals("INTERFACEMETHODREF")) {
                continue;
            }
            String[] info = (String[]) call("getMemberRefInfoAt", i);
            if (info[0].endsWith("$NeverLoaded")) {
                neverM = i;
                continue;
            }
            if (info[0].endsWith("$NeverLoadedToo")) {
                neverF = i;
                continue;
            }
            if (info[0].equals("jdk/internal/misc/VM")) {
                vmRef = i;
                continue;
            }
            if (!info[0].startsWith(self + "$") && !info[0].equals("java/util/List")) {
                continue;
            }
            if (tag.equals("FIELDREF")) {
                aFieldref = i;
                rows.add("field " + info[0] + "." + info[1] + ":" + info[2] + ": "
                        + show("getFieldAt", i) + " | ifLoaded=" + show("getFieldAtIfLoaded", i));
            } else {
                aMethodref = i;
                rows.add("method " + info[0] + "." + info[1] + info[2] + ": "
                        + show("getMethodAt", i) + " | ifLoaded=" + show("getMethodAtIfLoaded", i));
            }
        }
        Collections.sort(rows);
        rows.forEach(System.out::println);
        if (neverM > 0) {
            System.out.println("never-method before=" + show("getMethodAtIfLoaded", neverM)
                    + " at=" + show("getMethodAt", neverM)
                    + " after=" + show("getMethodAtIfLoaded", neverM));
        } else {
            System.out.println("never-method absent");
        }
        if (neverF > 0) {
            System.out.println("never-field before=" + show("getFieldAtIfLoaded", neverF)
                    + " at=" + show("getFieldAt", neverF)
                    + " after=" + show("getFieldAtIfLoaded", neverF));
        } else {
            System.out.println("never-field absent");
        }
        System.out.println("err-method-at-fieldref: " + show("getMethodAt", aFieldref));
        System.out.println("err-field-at-methodref: " + show("getFieldAt", aMethodref));
        System.out.println("err-field-at-index-0: " + show("getFieldAt", 0));
        System.out.println("err-method-past-end: " + show("getMethodAt", size));
        System.out.println("err-field-negative: " + show("getFieldAt", -1));
        if (vmRef > 0) {
            int vmClass = (Integer) call("getClassRefIndexAt", vmRef);
            System.out.println("inaccessible-class-at: " + show("getClassAt", vmClass));
            System.out.println("inaccessible-method-at: " + show("getMethodAt", vmRef));
            System.out.println("inaccessible-method-if-loaded: " + show("getMethodAtIfLoaded", vmRef));
        } else {
            System.out.println("inaccessible absent");
        }

        Runnable r = () -> System.out.print("");
        r.run();
        cp = getCp.invoke(jla, r.getClass());
        int lsize = cp == null ? 0 : (Integer) cpc.getMethod("getSize").invoke(cp);
        String impl = "absent";
        String thisClass = "absent";
        for (int i = 1; i < lsize; i++) {
            String tag = String.valueOf(call("getTagAt", i));
            if (tag.equals("CLASS") && thisClass.equals("absent")) {
                thisClass = String.valueOf(call("getClassAt", i) == r.getClass());
            }
            if (!tag.equals("METHODREF")) {
                continue;
            }
            String[] info = (String[]) call("getMemberRefInfoAt", i);
            if (info[1].equals("lambda$main$0")) {
                impl = show("getMethodAt", i);
            }
        }
        System.out.println("lambda-impl-method: " + impl);
        System.out.println("lambda-this-class-is-proxy: " + thisClass);
    }
}
