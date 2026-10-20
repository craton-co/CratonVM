// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 25, lane L3
// (docs/internal/fixed-bugs/interpreter-L3-retransform-redefines-each-class-twice-FIXED-20260928.md):
// what a class runs WHILE it is being retransformed. The transformer calls
// Target.value() from inside transform() on every retransform, then renames
// the literal "baseQ0" to "baseQ<n>". JVMTI RetransformClasses hands the
// transformer the retransformation base (the class file as first defined)
// but does not install it: the class keeps running its current version until
// the transformer's output is installed.
//
// HotSpot 25 prints (with the agent; -Xint too):
//     during=baseQ0,baseQ1,baseQ2 after=baseQ3
// CratonVM (both modes, read from the code, not run):
//     during=baseQ0,baseQ0,baseQ0 after=baseQ3
// because instrument::native_retransform_classes0 first redefines the class
// back to the base bytes, runs the transformers, then redefines it again with
// their output -- every retransform is two redefinitions, and code that runs
// the class meanwhile (here the transformer itself; in general any thread)
// runs the base body.
// Wave 26 (lane L3): the swap back to the base is gone
// (instrument::native_retransform_classes0 redefines each class once, with the
// chain's output). Expected now on CratonVM, every mode: HotSpot's line.
//
// SETUP: needs a java agent. Build a jar whose manifest has
//     Premain-Class: L3W25RetransformRunsTheBase$Agent
//     Can-Retransform-Classes: true
// containing L3W25RetransformRunsTheBase*.class, then run
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W25RetransformRunsTheBase
// Without the agent both VMs print "no agent" and then
//     during= after=baseQ0
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;
import java.util.ArrayList;
import java.util.List;

public class L3W25RetransformRunsTheBase {
    static volatile Instrumentation inst;
    static int retransforms;
    static final List<String> during = new ArrayList<>();

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class Target {
        public static String value() {
            return "baseQ0";
        }
    }

    static final class Rename implements ClassFileTransformer {
        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (redefined == null
                    || !"L3W25RetransformRunsTheBase$Target".equals(className)) {
                return null;
            }
            during.add(Target.value());
            int n = ++retransforms;
            byte[] out = bytes.clone();
            for (int i = 0; i + 5 < out.length; i++) {
                if (out[i] == 'b' && out[i + 1] == 'a' && out[i + 2] == 's'
                        && out[i + 3] == 'e' && out[i + 4] == 'Q') {
                    out[i + 5] = (byte) ('0' + n);
                }
            }
            return out;
        }
    }

    public static void main(String[] args) throws Exception {
        if (inst == null) {
            System.out.println("no agent");
        } else {
            inst.addTransformer(new Rename(), true);
            for (int i = 0; i < 3; i++) {
                inst.retransformClasses(Target.class);
            }
        }
        System.out.println("during=" + String.join(",", during) + " after=" + Target.value());
    }
}
