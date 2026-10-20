// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 28, lane L3: the ClassLoader argument a
// ClassFileTransformer is handed when a class is retransformed or
// redefined. JVMTI passes the class's defining loader
// (JvmtiClassFileLoadHookPoster: the_class->class_loader()); an agent that
// builds a type pool or class-file locator from it (Byte Buddy's
// AgentBuilder) resolves the class's types through that loader.
//
// HotSpot 25 prints (agent; -Xint too):
//     retransform app: loader=defining
//     redefine app: loader=defining
//     retransform own-loader: loader=defining
// CratonVM before wave 28 (read from the code: run_transformer_chain passed
// null on these paths):
//     retransform app: loader=null
//     redefine app: loader=null
//     retransform own-loader: loader=null
//
// SETUP: a jar whose manifest has
//     Premain-Class: L3W28RetransformLoader$Agent
//     Can-Redefine-Classes: true
//     Can-Retransform-Classes: true
// containing L3W28RetransformLoader*.class, then
//     java|cratonvm --java-home <jdk25> [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W28RetransformLoader
// Without the agent both VMs print "no agent".
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;

public class L3W28RetransformLoader {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class Target { public static String value() { return "old"; } }
    // Renamed to ...$OwnCpy (same length) and defined by the probe's own loader.
    public static class OwnDnr { public static String value() { return "old"; } }

    /** Records the loader it is handed for one class. */
    static final class Seen implements ClassFileTransformer {
        final String name;
        ClassLoader loader;
        boolean called;

        Seen(String name) {
            this.name = name;
        }

        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (redefined != null && name.equals(className)) {
                this.loader = loader;
                called = true;
            }
            return null;
        }

        String describe(Class<?> c) {
            if (!called) {
                return "not-called";
            }
            if (loader == null) {
                return "null";
            }
            return loader == c.getClassLoader() ? "defining" : "other:" + loader;
        }
    }

    static final class Definer extends ClassLoader {
        Definer(ClassLoader parent) {
            super(parent);
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static byte[] bytesOf(String simple) throws Exception {
        try (InputStream in = L3W28RetransformLoader.class
                .getResourceAsStream("L3W28RetransformLoader$" + simple + ".class")) {
            return in.readAllBytes();
        }
    }

    static byte[] renamed(String donor, String target) throws Exception {
        byte[] b = bytesOf(donor);
        String from = "L3W28RetransformLoader$" + donor;
        String to = "L3W28RetransformLoader$" + target;
        for (int i = 0; i + from.length() <= b.length; i++) {
            boolean match = true;
            for (int k = 0; k < from.length() && match; k++) {
                match = b[i + k] == (byte) from.charAt(k);
            }
            if (match) {
                for (int k = 0; k < to.length(); k++) {
                    b[i + k] = (byte) to.charAt(k);
                }
            }
        }
        return b;
    }

    static void retransform(String label, Class<?> c) throws Exception {
        Seen seen = new Seen(c.getName().replace('.', '/'));
        inst.addTransformer(seen, true);
        try {
            inst.retransformClasses(c);
        } finally {
            inst.removeTransformer(seen);
        }
        System.out.println(label + ": loader=" + seen.describe(c));
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRedefineClassesSupported() || !i.isRetransformClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        Target.value();
        retransform("retransform app", Target.class);

        Seen seen = new Seen("L3W28RetransformLoader$Target");
        i.addTransformer(seen, true);
        try {
            i.redefineClasses(new ClassDefinition(Target.class, bytesOf("Target")));
        } finally {
            i.removeTransformer(seen);
        }
        System.out.println("redefine app: loader=" + seen.describe(Target.class));

        Definer definer = new Definer(L3W28RetransformLoader.class.getClassLoader());
        Class<?> own = definer.define("L3W28RetransformLoader$OwnCpy", renamed("OwnDnr", "OwnCpy"));
        own.getMethod("value").invoke(null);
        retransform("retransform own-loader", own);
    }
}
