// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 118, lane L3 (proposal
// `i55-L1-proposal-lambda-proxies-answer-the-tool-interfaces-from-one-record`):
// what core reflection answers about a lambda's class, for the shapes the spun
// class differs by: a plain lambda, a capturing one, a method reference, a
// serializable one (`FLAG_SERIALIZABLE`: `writeReplace`), an intersection with
// a marker interface (`FLAG_MARKERS`), one whose functional interface extends
// `Serializable` (javac flags it serializable too), and a bridge-needing one
// (`FLAG_BRIDGES`). Each row lists modifiers, interfaces, declared
// constructors, methods (sorted) with their modifiers, bridge bit and
// exception types, and declared fields.
//
// `CRATONVM_REFLECT_LAMBDA_PROXY_FACTS=1` (default off) has reflection answer
// from the VM's one lambda-proxy record (`runtime::lambda_proxy::LambdaProxyFacts`,
// which the JVMTI and JNI readers already use); every row must stay SAME.
//
// Run: javac -d . L3W118LambdaProxyReflection.java
//      java -cp . L3W118LambdaProxyReflection                       (HotSpot reference)
//      cratonvm --jdk-only|--compatible [--nojit] -cp . L3W118LambdaProxyReflection
//      CRATONVM_REFLECT_LAMBDA_PROXY_FACTS=1 cratonvm ... (both modes)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// plain class: hidden=true synthetic=true modifiers=final interfaces=java.lang.Runnable
// plain ctors: [private ()]
// plain methods: [public void run()]
// plain fields: []
// plain public: 10 getConstructors: 0
// capturing class: hidden=true synthetic=true modifiers=final interfaces=java.util.function.Supplier
// capturing ctors: [private (java.lang.String,int)]
// capturing methods: [public java.lang.Object get()]
// capturing fields: [private final java.lang.String arg$1, private final int arg$2]
// capturing public: 10 getConstructors: 0
// methodref class: hidden=true synthetic=true modifiers=final interfaces=java.util.function.Supplier
// methodref ctors: [private ()]
// methodref methods: [public java.lang.Object get()]
// methodref fields: []
// methodref public: 10 getConstructors: 0
// serializable class: hidden=true synthetic=true modifiers=final interfaces=java.lang.Runnable,java.io.Serializable
// serializable ctors: [private ()]
// serializable methods: [private final java.lang.Object writeReplace(), public void run()]
// serializable fields: []
// serializable public: 10 getConstructors: 0
// marker class: hidden=true synthetic=true modifiers=final interfaces=java.lang.Runnable,L3W118LambdaProxyReflection$Marker
// marker ctors: [private ()]
// marker methods: [public void run()]
// marker fields: []
// marker public: 10 getConstructors: 0
// ser-iface class: hidden=true synthetic=true modifiers=final interfaces=L3W118LambdaProxyReflection$SerRunnable
// ser-iface ctors: [private ()]
// ser-iface methods: [private final java.lang.Object writeReplace(), public void run()]
// ser-iface fields: []
// ser-iface public: 10 getConstructors: 0
// bridges class: hidden=true synthetic=true modifiers=final interfaces=L3W118LambdaProxyReflection$StrM,L3W118LambdaProxyReflection$ObjM
// bridges ctors: [private ()]
// bridges methods: [public java.lang.String m(), public volatile java.lang.Object m() bridge]
// bridges fields: []
// bridges public: 11 getConstructors: 0
// function class: hidden=true synthetic=true modifiers=final interfaces=java.util.function.Function
// function ctors: [private ()]
// function methods: [public java.lang.Object apply(java.lang.Object)]
// function fields: []
// function public: 12 getConstructors: 0
// HOTSPOT_EXPECTED_END

import java.io.Serializable;
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.lang.reflect.Modifier;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.function.Function;
import java.util.function.Supplier;

public class L3W118LambdaProxyReflection {
    interface Marker {}

    interface SerRunnable extends Runnable, Serializable {}

    interface ObjM {
        Object m();
    }

    interface StrM {
        String m();
    }

    static String names(Class<?>[] types) {
        List<String> out = new ArrayList<>();
        for (Class<?> t : types) {
            out.add(fold(t.getName()));
        }
        return String.join(",", out);
    }

    static String fold(String s) {
        return s.replaceAll("\\$\\$Lambda[/$]?[0-9a-zA-Z]*", "\\$\\$Lambda");
    }

    static void row(String label, Object lambda) {
        Class<?> c = lambda.getClass();
        System.out.println(label + " class: hidden=" + c.isHidden() + " synthetic=" + c.isSynthetic()
                + " modifiers=" + Modifier.toString(c.getModifiers()) + " interfaces=" + names(c.getInterfaces()));
        List<String> ctors = new ArrayList<>();
        for (Constructor<?> k : c.getDeclaredConstructors()) {
            ctors.add(Modifier.toString(k.getModifiers()) + " (" + names(k.getParameterTypes()) + ")");
        }
        System.out.println(label + " ctors: " + ctors);
        List<String> methods = new ArrayList<>();
        for (Method m : c.getDeclaredMethods()) {
            methods.add(Modifier.toString(m.getModifiers()) + " " + m.getReturnType().getName() + " " + m.getName()
                    + "(" + names(m.getParameterTypes()) + ")" + (m.isBridge() ? " bridge" : "")
                    + (m.isSynthetic() ? " synthetic" : "")
                    + (m.getExceptionTypes().length > 0 ? " throws " + names(m.getExceptionTypes()) : ""));
        }
        methods.sort(null);
        System.out.println(label + " methods: " + methods);
        List<String> fields = new ArrayList<>();
        for (Field f : c.getDeclaredFields()) {
            fields.add(Modifier.toString(f.getModifiers()) + " " + f.getType().getName() + " " + f.getName());
        }
        System.out.println(label + " fields: " + fields);
        System.out.println(label + " public: " + c.getMethods().length + " getConstructors: " + c.getConstructors().length);
    }

    static String hello() {
        return "hello";
    }

    public static void main(String[] args) {
        int captured = 42;
        String text = "t";
        row("plain", (Runnable) () -> {});
        row("capturing", (Supplier<String>) () -> text + captured);
        row("methodref", (Supplier<String>) L3W118LambdaProxyReflection::hello);
        row("serializable", (Runnable & Serializable) () -> {});
        row("marker", (Runnable & Marker) () -> {});
        row("ser-iface", (SerRunnable) () -> {});
        row("bridges", (ObjM & StrM) () -> "s");
        row("function", (Function<String, Integer>) String::length);
    }
}
