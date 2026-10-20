// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 119, lane L3 (items 2-3 of
// `i42-L3-proposal-run-the-jdks-own-instrumentationimpl`;
// `CRATONVM_INSTRUMENT_JDK_CONSTRUCTOR=1` plus
// `CRATONVM_INSTRUMENT_JDK_TRANSFORMERS=1`, both default off): an agent's
// transformers kept by the JDK's own `TransformerManager`s and run through
// `InstrumentationImpl.transform`, against what HotSpot does with the same
// public API calls. Every row must be the same in the default arm (the VM's
// own chain) and in the switch arm.
//
// Rows:
//   * `load:` a class loaded after `premain` goes through every transformer
//     at load, the non-retransformable ones first (`a` registered second but
//     non-retransformable, runs before `b`), each seeing the previous answer;
//   * `retransform:` `retransformClasses` offers the class to the
//     retransformable transformers only, starting from the class file as it
//     was BEFORE the first retransformable one changed it;
//   * `removed:` after `removeTransformer(b)` a retransform runs nothing;
//   * `errors:` the JDK's checks (a null transformer, removing a null one).
//
// SETUP: a jar whose manifest has
//     Premain-Class: L3W119JdkTransformers$Agent
//     Can-Retransform-Classes: true
// containing L3W119JdkTransformers*.class, then
//     java|cratonvm [--jdk-only|--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W119JdkTransformers
//     CRATONVM_INSTRUMENT_JDK_CONSTRUCTOR=1 CRATONVM_INSTRUMENT_JDK_TRANSFORMERS=1 cratonvm ...
// Without the agent it prints "no agent". (No HOTSPOT_EXPECTED block: the
// battery runs no agent.) HotSpot 25.0.3 prints, with the agent:
//     load: Late.v=v0ab seen=[a, b]
//     retransform: Late.v=v0ab seen=[b]
//     removed: Late.v=v0a0 seen=[]
//     errors: add(null)=NullPointerException remove(null)=NullPointerException
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;
import java.util.ArrayList;
import java.util.List;

public class L3W119JdkTransformers {
    static volatile Instrumentation inst;
    static final List<String> SEEN = new ArrayList<>();
    static ClassFileTransformer b;

    /** Rewrites the literal's character at `pos` (0-based in "v000") to `to`. */
    static ClassFileTransformer patcher(String label, int pos, char to) {
        return new ClassFileTransformer() {
            @Override
            public byte[] transform(Module module, ClassLoader loader, String name, Class<?> redefined,
                    ProtectionDomain domain, byte[] bytes) {
                if (!"L3W119JdkTransformers$Late".equals(name)) {
                    return null;
                }
                synchronized (SEEN) {
                    SEEN.add(label);
                }
                byte[] out = bytes.clone();
                for (int i = 0; i + 4 <= out.length; i++) {
                    if (out[i] == 'v' && (out[i + 1] == '0' || out[i + 1] == 'a' || out[i + 1] == 'b')
                            && (out[i + 2] == '0' || out[i + 2] == 'a' || out[i + 2] == 'b')
                            && (out[i + 3] == '0' || out[i + 3] == 'a' || out[i + 3] == 'b')) {
                        out[i + pos] = (byte) to;
                        return out;
                    }
                }
                return null;
            }
        };
    }

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
            b = patcher("b", 3, 'b');
            // Registered first, but retransformable: runs after `a`.
            instrumentation.addTransformer(b, true);
            instrumentation.addTransformer(patcher("a", 2, 'a'), false);
        }
    }

    public static class Late {
        public static String v() {
            return "v000";
        }
    }

    static String seen() {
        synchronized (SEEN) {
            String s = SEEN.toString();
            SEEN.clear();
            return s;
        }
    }

    static String error(Runnable r) {
        try {
            r.run();
            return "none";
        } catch (RuntimeException e) {
            return e.getClass().getSimpleName();
        }
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null) {
            System.out.println("no agent");
            return;
        }
        String v = Late.v();
        System.out.println("load: Late.v=" + v + " seen=" + seen());
        i.retransformClasses(Late.class);
        System.out.println("retransform: Late.v=" + Late.v() + " seen=" + seen());
        i.removeTransformer(b);
        i.retransformClasses(Late.class);
        System.out.println("removed: Late.v=" + Late.v() + " seen=" + seen());
        System.out.println("errors: add(null)=" + error(() -> i.addTransformer(null))
                + " remove(null)=" + error(() -> i.removeTransformer(null)));
    }
}
