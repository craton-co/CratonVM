// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 26, lane L3
// (docs/internal/fixed-bugs/interpreter-L3-class-redefined-count-is-never-bumped-FIXED-20260928.md):
// what reflection answers for a class AFTER it is retransformed, when the
// answer was already asked (and cached by java.lang.Class) before.
//
// Target carries @Tag("annoA") on the class and on m(), and m() returns the
// literal "annoA" (one Utf8 constant serves all three). The transformer renames
// that constant to "annoB". The JDK's Class caches its reflection data and its
// annotation data and drops both when `Class.classRedefinedCount` moved
// (Class.reflectionData / Class.annotationData); HotSpot increments that field
// on every redefinition (VM_RedefineClasses::increment_class_counter).
//
// HotSpot 25 prints (agent; -Xint too; run 2026-09-28):
//     before class=annoA method=annoA m()=annoA
//     after class=annoB method=annoB m()=annoB
// CratonVM (read from the code, not run): `classRedefinedCount` is never
// incremented (native-builtins lib.rs, "classRedefinedCount is always 0"), so
// every answer served from the JDK's own per-Class caches keeps the first
// line's value. Expected:
//     after class=annoA method=<annoA or annoB> m()=annoB
// `class=` comes from Class.annotationData (cached against the count);
// `method=` comes from the Method object Class.reflectionData cached, whose
// annotations CratonVM serves through a VM-side native (see
// native_override.rs::redefine_immune_reflection_native) that may or may not
// read the current attributes -- the run decides. m() runs the new body either
// way.
// Since wave 27 (lane L3) the redefinition advances classRedefinedCount on the
// mirror and drops the annotation natives' cached proxies: HotSpot's lines.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L3W26RedefinedReflection$Agent
//     Can-Retransform-Classes: true
// containing L3W26RedefinedReflection*.class, then
//     java|cratonvm --java-home <jdk25> [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W26RedefinedReflection
// Without the agent both VMs print "no agent".
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;

public class L3W26RedefinedReflection {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    @Retention(RetentionPolicy.RUNTIME)
    public @interface Tag {
        String value();
    }

    @Tag("annoA")
    public static class Target {
        @Tag("annoA")
        public static String m() {
            return "annoA";
        }
    }

    static final class Rename implements ClassFileTransformer {
        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (redefined == null || !"L3W26RedefinedReflection$Target".equals(className)) {
                return null;
            }
            byte[] out = bytes.clone();
            for (int i = 0; i + 5 <= out.length; i++) {
                if (out[i] == 'a' && out[i + 1] == 'n' && out[i + 2] == 'n'
                        && out[i + 3] == 'o' && out[i + 4] == 'A') {
                    out[i + 4] = 'B';
                }
            }
            return out;
        }
    }

    static String line(String label) throws Exception {
        String cls = Target.class.getAnnotation(Tag.class).value();
        String method = Target.class.getDeclaredMethod("m").getAnnotation(Tag.class).value();
        return label + " class=" + cls + " method=" + method + " m()=" + Target.m();
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRetransformClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        System.out.println(line("before"));
        i.addTransformer(new Rename(), true);
        i.retransformClasses(Target.class);
        System.out.println(line("after"));
    }
}
