// Interpreter round i1, wave 29, lane L5 -- the `ProtectionDomain` a load-time
// `ClassFileTransformer` is handed for a class defined through
// `MethodHandles.Lookup.defineClass`, and the one the class then reports.
//
// HotSpot: `Lookup.ClassDefiner.defineClass` passes the lookup class's domain
// (`lookupClassProtectionDomain()`, `null` only for a bootstrap lookup class)
// to `ClassLoader.defineClass0`; `JvmtiClassFileLoadHookPoster` hands it to the
// transformers, and the new class's `getProtectionDomain()` answers it -- the
// SAME object as the lookup class's.
//
// CratonVM before wave 29 (from the code, not run;
// `docs/known-issues/interpreter/i28-L3-transformers-are-handed-a-null-protection-domain-20260929.md`
// item 2): the `lookup_define.rs` `Lookup.defineClass` native defined the class
// under the name `""`, and `define_class_full` runs the transformer chain only
// for a named define, so the class was never offered (`load pd=not offered`,
// `false`); `defineClass0` passed no domain to the chain. Wave 29, `--jdk-only`:
// both doors pass the name and the lookup class's domain, and install it in the
// new class's mirror. `--compatible` is unchanged (the page's item 3, which
// waits for a census): expected `load pd=not offered`, `false`, and `false`.
//
// Setup: one jar holds every class of this file; manifest
// `Premain-Class: L5W29LookupDefineDomain$Agent`.
//   javac -d out L5W29LookupDefineDomain.java
//   jar cfm p.jar manifest.txt -C out .
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -javaagent:p.jar -cp p.jar L5W29LookupDefineDomain
//
// Expected HotSpot 25 output (compare verbatim):
//   defined L5W29LookupDefineDomain$Gen value=3
//   load pd=location
//   load pd is the lookup class's=true
//   class pd is the lookup class's=true

import java.io.InputStream;
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.lang.invoke.MethodHandles;
import java.security.ProtectionDomain;

public class L5W29LookupDefineDomain {
    static volatile String offered = "not offered";
    static volatile ProtectionDomain offeredPd;

    public static class Agent {
        public static void premain(String args, Instrumentation inst) {
            inst.addTransformer(new ClassFileTransformer() {
                @Override
                public byte[] transform(ClassLoader loader, String name, Class<?> redefined,
                        ProtectionDomain pd, byte[] bytes) {
                    if ("L5W29LookupDefineDomain$Gen".equals(name) && redefined == null) {
                        offered = pd == null ? "null"
                                : pd.getCodeSource() == null ? "no-codesource"
                                : pd.getCodeSource().getLocation() == null ? "no-location"
                                : "location";
                        offeredPd = pd;
                    }
                    return null;
                }
            });
        }
    }

    /** Never named in a constant pool here: only `Lookup.defineClass` defines it. */
    public static class Gen {
        public static int value() {
            return 3;
        }
    }

    public static void main(String[] args) throws Throwable {
        byte[] bytes;
        try (InputStream in =
                L5W29LookupDefineDomain.class.getResourceAsStream("L5W29LookupDefineDomain$Gen.class")) {
            bytes = in.readAllBytes();
        }
        Class<?> gen = MethodHandles.lookup().defineClass(bytes);
        ProtectionDomain mine = L5W29LookupDefineDomain.class.getProtectionDomain();
        System.out.println("defined " + gen.getName() + " value=" + gen.getMethod("value").invoke(null));
        System.out.println("load pd=" + offered);
        System.out.println("load pd is the lookup class's=" + (offeredPd == mine));
        System.out.println("class pd is the lookup class's=" + (gen.getProtectionDomain() == mine));
    }
}
