// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.BufferedReader;
import java.io.InputStreamReader;
import java.lang.invoke.MethodType;
import java.lang.reflect.Constructor;
import java.lang.reflect.Executable;
import java.lang.reflect.Method;
import java.lang.reflect.Modifier;
import java.util.ArrayDeque;
import java.util.Deque;
import java.util.LinkedHashSet;
import java.util.Set;

/**
 * Classify {@code class method descriptor} triples (one per stdin line, tab or
 * space separated, internal names) against the RUNNING image: does a call of
 * that triple on an instance of that class reach real bytecode?
 *
 * <pre>
 *   declared-code        declared on the class with a body
 *   inherited-code:C     not declared; the body a virtual call reaches is C's
 *   abstract / native    declared (or resolved) without a body
 *   missing              no such method anywhere in the hierarchy
 *   no-class             the class is not in this image
 * </pre>
 *
 * Used by the SBR lanes to decide which registered {@code Bridge} rows have
 * bytecode to yield to (a row without one must not be retired). Run it on the
 * JDK whose image is being classified (HotSpot 25 for the 25 tables).
 */
public class ShadowTripleClassifier {
    public static void main(String[] args) throws Exception {
        BufferedReader in = new BufferedReader(new InputStreamReader(System.in));
        String line;
        while ((line = in.readLine()) != null) {
            line = line.trim();
            if (line.isEmpty() || line.startsWith("#")) continue;
            String[] p = line.split("[\t ]+");
            if (p.length < 3) continue;
            System.out.println(p[0] + "\t" + p[1] + "\t" + p[2] + "\t" + classify(p[0], p[1], p[2]));
        }
    }

    static String classify(String cls, String name, String desc) {
        Class<?> c;
        try {
            c = Class.forName(cls.replace('/', '.'), false, ClassLoader.getSystemClassLoader());
        } catch (Throwable t) {
            return "no-class";
        }
        MethodType mt;
        try {
            mt = MethodType.fromMethodDescriptorString(desc, ClassLoader.getSystemClassLoader());
        } catch (Throwable t) {
            return "bad-descriptor";
        }
        if (name.equals("<init>")) {
            for (Constructor<?> k : c.getDeclaredConstructors()) {
                if (matches(k, mt)) return "declared-code";
            }
            return "missing";
        }
        if (name.equals("<clinit>")) return "clinit";
        // Declared on the class itself?
        for (Method m : c.getDeclaredMethods()) {
            if (m.getName().equals(name) && matches(m, mt) && m.getReturnType() == mt.returnType()) {
                int mod = m.getModifiers();
                if (Modifier.isNative(mod)) return "native";
                if (Modifier.isAbstract(mod)) return "abstract";
                return "declared-code";
            }
        }
        // Superclass chain first (a class body beats a default method).
        for (Class<?> s = c.getSuperclass(); s != null; s = s.getSuperclass()) {
            for (Method m : s.getDeclaredMethods()) {
                if (m.getName().equals(name) && matches(m, mt) && m.getReturnType() == mt.returnType()) {
                    int mod = m.getModifiers();
                    if (Modifier.isNative(mod)) return "native:" + s.getName();
                    if (Modifier.isAbstract(mod)) return "abstract:" + s.getName();
                    if (Modifier.isStatic(mod)) return "inherited-static:" + s.getName();
                    return "inherited-code:" + s.getName();
                }
            }
        }
        // Then default methods on the interfaces.
        Set<Class<?>> seen = new LinkedHashSet<>();
        Deque<Class<?>> q = new ArrayDeque<>();
        for (Class<?> s = c; s != null; s = s.getSuperclass()) {
            for (Class<?> i : s.getInterfaces()) q.add(i);
        }
        String abs = null;
        while (!q.isEmpty()) {
            Class<?> i = q.poll();
            if (!seen.add(i)) continue;
            for (Method m : i.getDeclaredMethods()) {
                if (m.getName().equals(name) && matches(m, mt) && m.getReturnType() == mt.returnType()) {
                    if (m.isDefault()) return "inherited-code:" + i.getName();
                    if (abs == null) abs = "abstract:" + i.getName();
                }
            }
            for (Class<?> j : i.getInterfaces()) q.add(j);
        }
        return abs != null ? abs : "missing";
    }

    static boolean matches(Executable e, MethodType mt) {
        Class<?>[] ps = e.getParameterTypes();
        if (ps.length != mt.parameterCount()) return false;
        for (int i = 0; i < ps.length; i++) {
            if (ps[i] != mt.parameterType(i)) return false;
        }
        return true;
    }
}
