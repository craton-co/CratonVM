// L3W28TransformerDomain -- the `protectionDomain` a ClassFileTransformer is
// handed for an application class, at load time and on retransformation.
//
// HotSpot passes the domain the class is defined with (load time) and the
// class's own domain (retransformation); for a class on the class path that
// domain has a CodeSource with the jar's location. JaCoCo's agent skips every
// class whose domain has no source location unless `inclnolocationclasses` is
// set, so a `null` here silently instruments nothing.
//
// Before interpreter round i1 wave 28 CratonVM passed `null` on every path
// (`vm/src/runtime/instrument.rs` `run_chain_over_bytes`). Since then
// `--jdk-only` passes the class's own domain on retransformation, and the
// `defineClass1/2` native's domain at load time; after wave 28, also the
// application loader's domain for a class-path class the VM loads itself,
// one per code base, which the class's `getProtectionDomain()` answers too
// (`docs/known-issues/interpreter/i28-L3-transformers-are-handed-a-null-protection-domain-20260929.md`).
// `--compatible` is unchanged (the page's open item 3): `pd=null` twice, and
// `false` twice, since it builds a fresh domain per `getProtectionDomain()`.
//
// Run (the agent jar holds these classes; manifest `Premain-Class:
// L3W28TransformerDomain$Agent`, `Can-Retransform-Classes: true`):
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -javaagent:p.jar -cp p.jar L3W28TransformerDomain
//
// Expected HotSpot 25 output (compare verbatim):
//   target 1
//   load pd=location
//   retransform pd=location
//   load pd is the class's=true
//   one pd per jar=true

import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;

public class L3W28TransformerDomain {
    static volatile String loadDomain = "not offered";
    static volatile String retransformDomain = "not offered";
    static volatile ProtectionDomain loadPd;
    static Instrumentation inst;

    static String describe(ProtectionDomain pd) {
        if (pd == null) {
            return "null";
        }
        if (pd.getCodeSource() == null) {
            return "no-codesource";
        }
        return pd.getCodeSource().getLocation() == null ? "no-location" : "location";
    }

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            instrumentation.addTransformer(new ClassFileTransformer() {
                @Override
                public byte[] transform(ClassLoader loader, String name, Class<?> redefined,
                        ProtectionDomain pd, byte[] bytes) {
                    if ("L3W28TransformerDomain$Target".equals(name)) {
                        if (redefined == null) {
                            loadDomain = describe(pd);
                            loadPd = pd;
                        } else {
                            retransformDomain = describe(pd);
                        }
                    }
                    return null;
                }
            }, true);
            inst = instrumentation;
        }
    }

    public static class Target {
        static int value() {
            return 1;
        }
    }

    public static void main(String[] args) throws Exception {
        System.out.println("target " + Target.value());
        System.out.println("load pd=" + loadDomain);
        inst.retransformClasses(Target.class);
        System.out.println("retransform pd=" + retransformDomain);
        ProtectionDomain targetPd = Target.class.getProtectionDomain();
        System.out.println("load pd is the class's=" + (loadPd != null && loadPd == targetPd));
        System.out.println("one pd per jar="
                + (targetPd != null && targetPd == L3W28TransformerDomain.class.getProtectionDomain()));
    }
}
