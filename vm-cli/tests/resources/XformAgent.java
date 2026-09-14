// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;

/**
 * `-javaagent:` fixture for `cli_javaagent_transform.rs`.
 *
 * <p>It does the two things the known-issue doc says CratonVM accepted and then
 * did not do: it is <em>offered</em> classes as they are defined, and the bytes
 * it hands back are the ones that get defined.
 *
 * <p>The rewrite is a same-length constant-pool substitution ("ORIGINAL" ->
 * "MODIFIED") rather than a real bytecode edit. That is deliberate: it needs no
 * ASM on the test classpath, it cannot shift an offset, and it is still a
 * genuine change to the class file — if the VM ignores the returned array,
 * {@code XformTarget.main} prints the original marker and the test fails.
 *
 * <p>It prints one line per interesting event so a failure says which half
 * broke: never called at all, or called and the result discarded.
 */
public final class XformAgent {
    private static final byte[] FROM = { 'O', 'R', 'I', 'G', 'I', 'N', 'A', 'L' };
    private static final byte[] TO = { 'M', 'O', 'D', 'I', 'F', 'I', 'E', 'D' };

    private XformAgent() { }

    public static void premain(String args, Instrumentation inst) {
        System.out.println("agent premain inst=" + (inst != null));
        if (inst == null) {
            return;
        }
        inst.addTransformer(new ClassFileTransformer() {
            @Override
            public byte[] transform(ClassLoader loader, String className,
                                    Class<?> classBeingRedefined, ProtectionDomain pd,
                                    byte[] classfileBuffer) {
                if (!"XformTarget".equals(className)) {
                    return null;
                }
                System.out.println("agent offered XformTarget loaderNull=" + (loader == null));
                byte[] out = classfileBuffer.clone();
                int at = indexOf(out, FROM);
                if (at < 0) {
                    System.out.println("agent marker-not-found");
                    return null;
                }
                System.arraycopy(TO, 0, out, at, TO.length);
                return out;
            }
        }, true);
    }

    public static void agentmain(String args, Instrumentation inst) {
        premain(args, inst);
    }

    private static int indexOf(byte[] haystack, byte[] needle) {
        outer:
        for (int i = 0; i + needle.length <= haystack.length; i++) {
            for (int j = 0; j < needle.length; j++) {
                if (haystack[i + j] != needle[j]) {
                    continue outer;
                }
            }
            return i;
        }
        return -1;
    }
}
