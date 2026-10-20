// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 27, lane L3: the retransformation base a
// redefineClasses leaves behind when a retransform-capable transformer
// changed the redefined class file.
//
// Target.value() returns "mark-a". Bump, a retransform-capable transformer,
// turns "mark-X" into "mark-(X+1)" in every class file it is offered for
// Target. JVMTI caches, for a later RetransformClasses, the class file as it
// was BEFORE the first retransform-capable transformer changed it; for a
// redefinition that is the redefinition's own input
// (VM_RedefineClasses::redefine_single_class replaces the cached class file
// with the new version's). So after redefineClasses(Target, <Target's own
// class file>) the class runs "mark-b" and its base is the "mark-a" file; a
// retransform then runs Bump over "mark-a" again and installs "mark-b", not
// "mark-c". A third step removes Bump and retransforms: the class returns to
// its base.
//
// HotSpot 25 prints (agent; -Xint too; run 2026-09-28):
//     start value=mark-a
//     redefine value=mark-b
//     retransform value=mark-b
//     removed value=mark-a
// CratonVM before wave 27 (read from the code): redefine_class recorded the
// chain's OUTPUT ("mark-b") as the class's cached bytes, so
//     retransform value=mark-c
//     removed value=mark-b
//
// SETUP: a jar whose manifest has
//     Premain-Class: L3W27RedefineKeepsTheBase$Agent
//     Can-Redefine-Classes: true
//     Can-Retransform-Classes: true
// containing L3W27RedefineKeepsTheBase*.class, then
//     java|cratonvm --java-home <jdk25> [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W27RedefineKeepsTheBase
// Without the agent both VMs print "no agent".
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;

public class L3W27RedefineKeepsTheBase {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class Target {
        public static String value() {
            return "mark-a";
        }
    }

    static final class Bump implements ClassFileTransformer {
        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (redefined == null || !"L3W27RedefineKeepsTheBase$Target".equals(className)) {
                return null;
            }
            byte[] out = bytes.clone();
            for (int i = 0; i + 6 <= out.length; i++) {
                if (out[i] == 'm' && out[i + 1] == 'a' && out[i + 2] == 'r'
                        && out[i + 3] == 'k' && out[i + 4] == '-') {
                    out[i + 5] = (byte) (out[i + 5] + 1);
                }
            }
            return out;
        }
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRedefineClassesSupported() || !i.isRetransformClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        System.out.println("start value=" + Target.value());
        byte[] own;
        try (InputStream in = L3W27RedefineKeepsTheBase.class
                .getResourceAsStream("L3W27RedefineKeepsTheBase$Target.class")) {
            own = in.readAllBytes();
        }
        Bump bump = new Bump();
        i.addTransformer(bump, true);
        i.redefineClasses(new ClassDefinition(Target.class, own));
        System.out.println("redefine value=" + Target.value());
        i.retransformClasses(Target.class);
        System.out.println("retransform value=" + Target.value());
        i.removeTransformer(bump);
        i.retransformClasses(Target.class);
        System.out.println("removed value=" + Target.value());
    }
}
