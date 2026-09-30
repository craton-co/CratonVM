// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 26, lane L3
// (docs/internal/fixed-bugs/interpreter-L3-a-retransform-probe-sees-one-transform-fewer-than-hotspot-FIXED-20260928.md):
// which first touches of a class offer it to a registered ClassFileTransformer
// at load, in what order the transformers run, and which bytes a later
// retransform hands a retransform-capable transformer.
//
// Two transformers, registered in this order:
//   R  retransform-capable; records every load it is offered (redefined ==
//      null) and, for Base, rewrites "baseR0" to "baseR1" at load and to
//      "baseR2" on a retransform; it records the markers of the bytes it is
//      handed.
//   N  NOT retransform-capable; rewrites Base's "tagA" to "tagN" at load.
// JVMTI runs every non-retransformable transformer before any
// retransform-capable one (libinstrument's two environments), whatever the
// registration order, and caches the class file as it was BEFORE the first
// retransform-capable transformer changed it: that cache is what a retransform
// hands R.
//
// HotSpot 25 prints (agent; -Xint too; run 2026-09-28):
//     loads=N,ByStatic,ByField,ByNew,ByLdc,ByForName,Base
//     load: R saw baseR0/tagN, Base.value()=baseR1/tagN
//     retransform: R saw baseR0/tagN, Base.value()=baseR2/tagN
// (N is the transformer class itself, loaded by `new N()` after R was added.)
// CratonVM before wave 26 (read from the code, not run):
//     loads=N,ByField,ByNew,ByLdc,ByForName
//         (an invokestatic owner was loaded by dispatch_static.rs's flat
//         `load_class_concurrent`, which never offers the class: ByStatic and
//         Base are missing)
//     load: R saw -, Base.value()=baseR0/tagA
//     retransform: R saw baseR0/tagA, Base.value()=baseR2/tagA
//         (and, with the load hook firing, R ran before N and the retransform
//         base was the bytes as DEFINED, R's own load output included:
//         `load: R saw baseR0/tagA`, `retransform: R saw baseR1/tagN`)
// Wave 26 (lane L3): the invokestatic owner load offers the class
// (`SharedVm::load_class_transformed`), the chain runs non-retransformable
// transformers first, and the class-bytes cache keeps the bytes the first
// retransform-capable transformer was handed. Expected now: HotSpot's lines,
// every mode (--nojit, JIT, default, --compatible).
//
// SETUP: a jar whose manifest has
//     Premain-Class: L3W26LoadHookFirstTouch$Agent
//     Can-Retransform-Classes: true
// containing L3W26LoadHookFirstTouch*.class, then
//     java|cratonvm --java-home <jdk25> [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W26LoadHookFirstTouch
// Without the agent both VMs print "no agent".
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;
import java.util.ArrayList;
import java.util.List;

public class L3W26LoadHookFirstTouch {
    static volatile Instrumentation inst;
    static final String PREFIX = "L3W26LoadHookFirstTouch$";

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class ByStatic {
        static int value() {
            return 1;
        }
    }

    public static class ByField {
        static int x = 2;
    }

    public static class ByNew {
    }

    public static class ByLdc {
    }

    public static class ByForName {
    }

    public static class Base {
        static String value() {
            return "baseR0";
        }

        static String tag() {
            return "tagA";
        }
    }

    /** Index of the first occurrence of `s` (ASCII) in `b`, or -1. */
    static int find(byte[] b, String s) {
        outer:
        for (int i = 0; i + s.length() <= b.length; i++) {
            for (int k = 0; k < s.length(); k++) {
                if (b[i + k] != (byte) s.charAt(k)) {
                    continue outer;
                }
            }
            return i;
        }
        return -1;
    }

    /** "baseR<d>/tag<c>" as carried by the class file `b`. */
    static String markers(byte[] b) {
        int r = find(b, "baseR");
        String base = r < 0 ? "?" : "baseR" + (char) b[r + 5];
        // Not a bare "tag": that is also the method's own name.
        String tag = find(b, "tagA") >= 0 ? "tagA" : find(b, "tagN") >= 0 ? "tagN" : "?";
        return base + "/" + tag;
    }

    static final class R implements ClassFileTransformer {
        final List<String> loads = new ArrayList<>();
        String loadSaw = "-";
        String retransformSaw = "-";

        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (className == null || !className.startsWith(PREFIX)) {
                return null;
            }
            String simple = className.substring(PREFIX.length());
            if (redefined == null) {
                loads.add(simple);
            }
            if (!simple.equals("Base")) {
                return null;
            }
            if (redefined == null) {
                loadSaw = markers(bytes);
            } else {
                retransformSaw = markers(bytes);
            }
            byte[] out = bytes.clone();
            int r = find(out, "baseR");
            if (r >= 0) {
                out[r + 5] = (byte) (redefined == null ? '1' : '2');
            }
            return out;
        }
    }

    static final class N implements ClassFileTransformer {
        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (!(PREFIX + "Base").equals(className)) {
                return null;
            }
            byte[] out = bytes.clone();
            int t = find(out, "tagA");
            if (t >= 0) {
                out[t + 3] = (byte) 'N';
            }
            return out;
        }
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRetransformClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        R r = new R();
        i.addTransformer(r, true);
        i.addTransformer(new N(), false);
        int sum = ByStatic.value();
        sum += ByField.x;
        Object o = new ByNew();
        Class<?> c = ByLdc.class;
        Class<?> f = Class.forName(PREFIX + "ByForName");
        String loaded = Base.value() + "/" + Base.tag();
        i.retransformClasses(Base.class);
        String retransformed = Base.value() + "/" + Base.tag();
        if (sum != 3 || o == null || c == null || f == null) {
            System.out.println("unexpected");
        }
        System.out.println("loads=" + String.join(",", r.loads));
        System.out.println("load: R saw " + r.loadSaw + ", Base.value()=" + loaded);
        System.out.println("retransform: R saw " + r.retransformSaw
                + ", Base.value()=" + retransformed);
    }
}
