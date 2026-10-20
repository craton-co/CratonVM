// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 106, lane L3 (hunt; page
// docs/internal/fixed-bugs/interpreter-i106-L3-lambda-proxy-classes-have-no-constant-pool-FIXED-20261016.md):
// on HotSpot a lambda's class is a real hidden class spun by
// `InnerClassLambdaMetafactory`, and its constant pool holds a member
// reference to the lambda's implementation method (the synthetic
// `lambda$main$0`, or a method reference's target). TypeTools' `TypeResolver`
// (ModelMapper, Jdbi, ...) finds a lambda's generic types exactly so: it scans
// `getConstantPool(lambda.getClass())` for that reference. CratonVM runs a
// lambda proxy with no class file, and its pool reads as empty (size 0).
//
// Rows: the pool is non-empty, and holds the expected member reference
// (scanned with `getMemberRefInfoAt` over every field / method reference).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3; derived from InnerClassLambdaMetafactory:
// the spun class calls the implementation method with an invokestatic /
// invokevirtual of a Methodref to it)
// runnable-pool-non-empty: true
// runnable-impl-ref: L3W106LambdaProxyConstantPool lambda$main$0 ()V
// function-pool-non-empty: true
// function-target-ref: java/lang/String length ()I
// HOTSPOT_EXPECTED_END
//
// SETUP: `javac L3W106LambdaProxyConstantPool.java`, then
//   java|cratonvm [--compatible] [--nojit] --add-exports java.base/jdk.internal.access=ALL-UNNAMED
//     --add-exports java.base/jdk.internal.reflect=ALL-UNNAMED -cp . L3W106LambdaProxyConstantPool
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.function.Function;

public class L3W106LambdaProxyConstantPool {
    static Object jla;
    static Method getConstantPool;
    static Class<?> cpc;

    static Object call(Object cp, String name, int index) throws Throwable {
        try {
            return cpc.getMethod(name, int.class).invoke(cp, index);
        } catch (InvocationTargetException e) {
            throw e.getCause();
        }
    }

    // "<pool size > 0>|<first member reference matching (owner, name)>".
    static String[] scan(Class<?> c, String owner, String name) throws Throwable {
        Object cp = getConstantPool.invoke(jla, c);
        if (cp == null) {
            return new String[] {"null pool", "-"};
        }
        int size = (Integer) cpc.getMethod("getSize").invoke(cp);
        String found = "none";
        for (int i = 1; i < size; i++) {
            String tag = String.valueOf(call(cp, "getTagAt", i));
            if (!tag.equals("METHODREF") && !tag.equals("INTERFACEMETHODREF") && !tag.equals("FIELDREF")) {
                continue;
            }
            String[] info = (String[]) call(cp, "getMemberRefInfoAt", i);
            if (info[0].equals(owner) && info[1].equals(name)) {
                found = info[0] + " " + info[1] + " " + info[2];
                break;
            }
        }
        return new String[] {String.valueOf(size > 0), found};
    }

    public static void main(String[] args) throws Throwable {
        Class<?> ss = Class.forName("jdk.internal.access.SharedSecrets");
        jla = ss.getMethod("getJavaLangAccess").invoke(null);
        getConstantPool = Class.forName("jdk.internal.access.JavaLangAccess")
                .getMethod("getConstantPool", Class.class);
        cpc = Class.forName("jdk.internal.reflect.ConstantPool");

        Runnable r = () -> System.out.print("");
        r.run();
        String[] rs = scan(r.getClass(), "L3W106LambdaProxyConstantPool", "lambda$main$0");
        System.out.println("runnable-pool-non-empty: " + rs[0]);
        System.out.println("runnable-impl-ref: " + rs[1]);

        Function<String, Integer> f = String::length;
        f.apply("abc");
        String[] fs = scan(f.getClass(), "java/lang/String", "length");
        System.out.println("function-pool-non-empty: " + fs[0]);
        System.out.println("function-target-ref: " + fs[1]);
    }
}
