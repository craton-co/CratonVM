// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 118, lane L3: the "What to measure first" run of
// `i42-L3-proposal-run-the-jdks-own-instrumentationimpl`. It drives the JDK's
// OWN transformer pipeline, `TransformerManager.addTransformer` and
// `InstrumentationImpl.transform(Module, ClassLoader, String, Class,
// ProtectionDomain, byte[], boolean)`, directly and reflectively, on the
// `InstrumentationImpl` the agent was handed. The VM's
// `run_transformer_chain` comment says this pipeline "panics deep inside the
// JDK 25 transformer pipeline (length-6 array indexed at 6)"; this probe shows
// whether it still does, before items 2-4 of the page are built.
//
// The object has the JDK's `TransformerManager` only when the JDK constructor
// ran: always on HotSpot, and on CratonVM only with
// `CRATONVM_INSTRUMENT_JDK_CONSTRUCTOR=1` (default off; without it the
// `manager:` row reads `null` and the rest is skipped).
//
// Two transformers are added to the manager (not through `addTransformer`, so
// no class-load hook is armed): the first rewrites the literal `v000` of
// `Target`'s class file to `v001`, the second sees the first's answer and
// rewrites `v001` to `v002`; a third returns null (no change). One more
// `transform` call with `isRetransformer = true` reaches the (absent)
// retransformable manager and must answer null.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L3W118InstrumentationImplTransform$Agent
//     Can-Retransform-Classes: true
// containing L3W118InstrumentationImplTransform*.class, then
//     java|cratonvm [--jdk-only|--compatible] [--nojit]
//         --add-opens java.instrument/sun.instrument=ALL-UNNAMED
//         -javaagent:probe.jar -cp probe.jar L3W118InstrumentationImplTransform
//     CRATONVM_INSTRUMENT_JDK_CONSTRUCTOR=1 cratonvm ... (the switch arm)
// Without the agent it prints "no agent". (No HOTSPOT_EXPECTED block: the
// battery runs no agent.) HotSpot 25.0.3 prints, with the agent:
//     manager: sun.instrument.TransformerManager
//     order: t1 t2 t3
//     result: v002
//     retransformer: null
import java.io.InputStream;
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.security.ProtectionDomain;
import java.util.ArrayList;
import java.util.List;

public class L3W118InstrumentationImplTransform {
    static volatile Instrumentation inst;
    static final List<String> ORDER = new ArrayList<>();

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class Target {
        public static String v() {
            return "v000";
        }
    }

    static byte[] patch(byte[] bytes, char from, char to) {
        byte[] b = bytes.clone();
        for (int i = 0; i + 4 <= b.length; i++) {
            if (b[i] == 'v' && b[i + 1] == '0' && b[i + 2] == '0' && b[i + 3] == from) {
                b[i + 3] = (byte) to;
            }
        }
        return b;
    }

    static ClassFileTransformer transformer(String label, char from, char to) {
        return new ClassFileTransformer() {
            @Override
            public byte[] transform(Module module, ClassLoader loader, String name, Class<?> redefined,
                    ProtectionDomain domain, byte[] bytes) {
                ORDER.add(label);
                return from == 0 ? null : patch(bytes, from, to);
            }
        };
    }

    static String literal(byte[] bytes) {
        if (bytes == null) {
            return "null";
        }
        for (int i = 0; i + 4 <= bytes.length; i++) {
            if (bytes[i] == 'v' && bytes[i + 1] == '0' && bytes[i + 2] == '0') {
                return new String(bytes, i, 4, java.nio.charset.StandardCharsets.ISO_8859_1);
            }
        }
        return "?";
    }

    public static void main(String[] args) throws Throwable {
        Instrumentation i = inst;
        if (i == null) {
            System.out.println("no agent");
            return;
        }
        Class<?> impl = i.getClass();
        Field managerField = impl.getDeclaredField("mTransformerManager");
        managerField.setAccessible(true);
        Object manager = managerField.get(i);
        System.out.println("manager: " + (manager == null ? "null" : manager.getClass().getName()));
        if (manager == null) {
            return;
        }
        Method add = manager.getClass().getDeclaredMethod("addTransformer", ClassFileTransformer.class);
        add.setAccessible(true);
        add.invoke(manager, transformer("t1", '0', '1'));
        add.invoke(manager, transformer("t2", '1', '2'));
        add.invoke(manager, transformer("t3", (char) 0, (char) 0));
        byte[] bytes;
        try (InputStream in = L3W118InstrumentationImplTransform.class
                .getResourceAsStream("L3W118InstrumentationImplTransform$Target.class")) {
            bytes = in.readAllBytes();
        }
        Method transform = impl.getDeclaredMethod("transform", Module.class, ClassLoader.class, String.class,
                Class.class, ProtectionDomain.class, byte[].class, boolean.class);
        transform.setAccessible(true);
        String name = "L3W118InstrumentationImplTransform$Target";
        Module module = Target.class.getModule();
        ClassLoader loader = Target.class.getClassLoader();
        Object result;
        try {
            result = transform.invoke(i, module, loader, name, null, null, bytes, false);
        } catch (java.lang.reflect.InvocationTargetException e) {
            result = e.getCause();
        }
        System.out.println("order: " + String.join(" ", ORDER));
        System.out.println("result: " + (result instanceof byte[] b ? literal(b) : String.valueOf(result)));
        Object again;
        try {
            again = transform.invoke(i, module, loader, name, Target.class, null, bytes, true);
        } catch (java.lang.reflect.InvocationTargetException e) {
            again = e.getCause();
        }
        System.out.println("retransformer: " + (again instanceof byte[] b ? literal(b) : String.valueOf(again)));
    }
}
