// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 106, lane L3 (hunt): `jdk.internal.reflect.ConstantPool`'s
// class and reference readers -- `getClassAt0`, `getClassAtIfLoaded0`,
// `getClassRefIndexAt0`, `getNameAndTypeRefIndexAt0`, `getMemberRefInfoAt0`,
// `getNameAndTypeRefInfoAt0` -- were never registered: every call was an
// `UnsatisfiedLinkError` (only the annotation parser's readers, size and tag
// were). Libraries that resolve a lambda's generic types by reading its pool
// (TypeTools / ModelMapper / Jdbi's shape) call exactly these.
//
// The pool read is the one of the nested `Target` class below; rows keep only
// the entries the source of `Target` puts there (the class's own members,
// `List.of`, its one `invokedynamic`), sorted, so javac's pool order does not
// matter.
//
// Rows:
//   class <name> at=<getClassAt> ifLoaded=<getClassAtIfLoaded>
//   indy <name> <descriptor>
//   ref <kind> <class> <name> <descriptor> class-index=<class at it> nat=<name/desc at it>
//   never-loaded before=<ifLoaded> at=<getClassAt> after=<ifLoaded>
//   err-*: the exception class and message HotSpot's `bounds_check` / tag checks throw
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3; derived from the JDK 25 sources --
// jvm.cpp's JVM_ConstantPool* entries -- and javac 25's pool for `Target`)
// class L3W106ConstantPoolRefs$Target at=L3W106ConstantPoolRefs$Target ifLoaded=L3W106ConstantPoolRefs$Target
// class java/lang/Object at=java.lang.Object ifLoaded=java.lang.Object
// class java/util/List at=java.util.List ifLoaded=java.util.List
// indy run ()Ljava/lang/Runnable;
// ref FIELDREF L3W106ConstantPoolRefs$Target counter I class-index=L3W106ConstantPoolRefs$Target nat=counter/I
// ref INTERFACEMETHODREF java/util/List of (Ljava/lang/Object;)Ljava/util/List; class-index=java.util.List nat=of/(Ljava/lang/Object;)Ljava/util/List;
// ref METHODREF L3W106ConstantPoolRefs$Target body ()V class-index=L3W106ConstantPoolRefs$Target nat=body/()V
// ref METHODREF L3W106ConstantPoolRefs$Target s ()Ljava/lang/String; class-index=L3W106ConstantPoolRefs$Target nat=s/()Ljava/lang/String;
// never-loaded before=null at=L3W106ConstantPoolRefs$NeverLoaded after=L3W106ConstantPoolRefs$NeverLoaded
// err-index-0: java.lang.IllegalArgumentException Wrong type at constant pool index
// err-past-end: java.lang.IllegalArgumentException Constant pool index out of bounds
// err-negative: java.lang.IllegalArgumentException Constant pool index out of bounds
// err-utf8-at-class: java.lang.IllegalArgumentException Wrong type at constant pool index
// err-member-at-class: java.lang.IllegalArgumentException Wrong type at constant pool index
// err-nat-info-at-methodref: java.lang.IllegalArgumentException Wrong type at constant pool index
// err-class-ref-at-indy: java.lang.IllegalArgumentException Wrong type at constant pool index
// cp-of-int: null
// cp-of-array: null
// HOTSPOT_EXPECTED_END
//
// SETUP: `javac L3W106ConstantPoolRefs.java` (no flags: the internal API is
// reached by reflection), then
//   java|cratonvm [--compatible] [--nojit] --add-exports java.base/jdk.internal.access=ALL-UNNAMED
//     --add-exports java.base/jdk.internal.reflect=ALL-UNNAMED -cp . L3W106ConstantPoolRefs
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;

public class L3W106ConstantPoolRefs {
    static class NeverLoaded {
    }

    static class Target {
        static int counter;

        static String s() {
            return "x";
        }

        static void body() {
            counter++;
            String v = s();
            Runnable r = Target::body;
            List<String> l = List.of(v);
            if (counter < 0) {
                // Never runs: `NeverLoaded` stays unloaded until getClassAt loads it.
                System.out.println(new NeverLoaded());
            }
            if (l.isEmpty()) {
                r.run();
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

    static String err(String name, int index) {
        try {
            Object r = call(name, index);
            return "no exception: " + (r instanceof String[] a ? String.join(",", a) : String.valueOf(r));
        } catch (Throwable t) {
            return t.getClass().getName() + " " + t.getMessage();
        }
    }

    static String className(Object c) {
        return c == null ? "null" : ((Class<?>) c).getName();
    }

    public static void main(String[] args) throws Throwable {
        Target.body();
        Class<?> ss = Class.forName("jdk.internal.access.SharedSecrets");
        Object jla = ss.getMethod("getJavaLangAccess").invoke(null);
        Class<?> jlaI = Class.forName("jdk.internal.access.JavaLangAccess");
        cp = jlaI.getMethod("getConstantPool", Class.class).invoke(jla, Target.class);
        cpc = Class.forName("jdk.internal.reflect.ConstantPool");
        int size = (Integer) cpc.getMethod("getSize").invoke(cp);
        String target = Target.class.getName().replace('.', '/');
        // Never `NeverLoaded.class`: that ldc would load the class.
        String never = L3W106ConstantPoolRefs.class.getName() + "$NeverLoaded";
        List<String> rows = new ArrayList<>();
        // Class entry index -> internal name, read off the member references
        // (no reader names a Class entry without resolving it).
        Map<String, Integer> classIndex = new TreeMap<>();
        int aClass = -1, aMethodref = -1, anIndy = -1;
        for (int i = 1; i < size; i++) {
            String tag = String.valueOf(call("getTagAt", i));
            switch (tag) {
                case "FIELDREF", "METHODREF", "INTERFACEMETHODREF" -> {
                    if (tag.equals("METHODREF")) aMethodref = i;
                    String[] info = (String[]) call("getMemberRefInfoAt", i);
                    int ci = (Integer) call("getClassRefIndexAt", i);
                    classIndex.put(info[0], ci);
                    if (!info[0].equals(target) && !info[0].equals("java/util/List")) continue;
                    int ni = (Integer) call("getNameAndTypeRefIndexAt", i);
                    String[] nat = (String[]) call("getNameAndTypeRefInfoAt", ni);
                    rows.add("ref " + tag + " " + info[0] + " " + info[1] + " " + info[2]
                            + " class-index=" + className(call("getClassAt", ci))
                            + " nat=" + nat[0] + "/" + nat[1]);
                }
                case "INVOKEDYNAMIC" -> {
                    anIndy = i;
                    int ni = (Integer) call("getNameAndTypeRefIndexAt", i);
                    String[] nat = (String[]) call("getNameAndTypeRefInfoAt", ni);
                    rows.add("indy " + nat[0] + " " + nat[1]);
                }
                case "CLASS" -> aClass = i;
                default -> {
                }
            }
        }
        String neverRow = "never-loaded absent";
        Integer ni = classIndex.get(never.replace('.', '/'));
        if (ni != null) {
            String before = className(call("getClassAtIfLoaded", ni));
            String at = className(call("getClassAt", ni));
            String after = className(call("getClassAtIfLoaded", ni));
            neverRow = "never-loaded before=" + before + " at=" + at + " after=" + after;
        }
        for (String name : new String[] {target, "java/lang/Object", "java/util/List"}) {
            Integer ci = classIndex.get(name);
            if (ci == null) {
                rows.add("class " + name + " absent");
                continue;
            }
            rows.add("class " + name + " at=" + className(call("getClassAt", ci))
                    + " ifLoaded=" + className(call("getClassAtIfLoaded", ci)));
        }
        Collections.sort(rows);
        rows.forEach(System.out::println);
        System.out.println(neverRow);
        System.out.println("err-index-0: " + err("getMemberRefInfoAt", 0));
        System.out.println("err-past-end: " + err("getClassAt", size));
        System.out.println("err-negative: " + err("getNameAndTypeRefIndexAt", -1));
        System.out.println("err-utf8-at-class: " + err("getUTF8At", aClass));
        System.out.println("err-member-at-class: " + err("getMemberRefInfoAt", aClass));
        System.out.println("err-nat-info-at-methodref: " + err("getNameAndTypeRefInfoAt", aMethodref));
        System.out.println("err-class-ref-at-indy: " + err("getClassRefIndexAt", anIndy));
        Method getCp = jlaI.getMethod("getConstantPool", Class.class);
        System.out.println("cp-of-int: " + getCp.invoke(jla, int.class));
        System.out.println("cp-of-array: " + getCp.invoke(jla, String[].class));
    }
}
