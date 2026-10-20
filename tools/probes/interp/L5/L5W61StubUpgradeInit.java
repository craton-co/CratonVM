// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 61, lane L5 -- a class whose real bytes arrive
// AFTER a thread saw it initialized must still run its real `<clinit>` for
// that thread (`i60-L7-the-class-initialized-memo-survives-a-stub-upgrade`).
//
// Only `--compatible` can reach it: a constant-pool reference to a name under
// an enterprise stub prefix (`io/smallrye/`) that is absent from the class
// path resolves to a fabricated synthetic stub there. Per row the probe
// defines (`Lookup.defineClass`, its own package) a "toucher" whose `touch`
// runs `new <name>()` twice and swallows what it gets (HotSpot:
// `NoClassDefFoundError`; CratonVM --compatible: the stub, initialized, and
// the calling thread's "class initialized" memo filled). Then a fresh loader
// defines the REAL bytes of `<name>` (CratonVM: the stub is upgraded IN PLACE,
// same `ClassId`, `init_state` reset) and the probe reads a static only the
// real `<clinit>` sets, reflectively, on the SAME thread. Before wave 61 that
// thread's memo still held the stub's positive answer, so the `Field.get`
// door could skip `<clinit>`. A second row reads on another thread (the
// control: no memo entry there); the third repeats the first (cold first
// call).
//
// The bytes are this file's own `L5W61StubUpgradeInitTarget` /
// `L5W61StubUpgradeInitToucher` with same-length names patched in.
//
// Run: javac -d out L5W61StubUpgradeInit.java
//      cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W61StubUpgradeInit
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   first=clinit-ran
//   first-other-thread=clinit-ran
//   again=clinit-ran

import java.io.InputStream;
import java.lang.invoke.MethodHandles;
import java.nio.charset.StandardCharsets;

public class L5W61StubUpgradeInit {
    static final String TARGET = "L5W61StubUpgradeInitTarget";
    static final String TOUCHER = "L5W61StubUpgradeInitToucher";

    static byte[] bytesOf(String name) throws Exception {
        try (InputStream in = L5W61StubUpgradeInit.class.getResourceAsStream(name + ".class")) {
            return in.readAllBytes();
        }
    }

    static void patch(byte[] b, String oldName, String newName) {
        byte[] from = oldName.getBytes(StandardCharsets.UTF_8);
        byte[] to = newName.getBytes(StandardCharsets.UTF_8);
        if (from.length != to.length) throw new IllegalStateException("length " + newName);
        outer:
        for (int i = 0; i + from.length <= b.length; i++) {
            for (int j = 0; j < from.length; j++) {
                if (b[i + j] != from[j]) continue outer;
            }
            System.arraycopy(to, 0, b, i, to.length);
        }
    }

    static final class Definer extends ClassLoader {
        Definer() { super(null); }
        Class<?> define(String name, byte[] b) { return defineClass(name, b, 0, b.length); }
    }

    static String read(Class<?> c) {
        try {
            java.lang.reflect.Field f = c.getDeclaredField("state");
            f.setAccessible(true);
            return String.valueOf(f.get(null));
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    // `suffix` is two characters: the target is `io/smallrye/l5w61/Target<suffix>`
    // (26 bytes, as `L5W61StubUpgradeInitTarget`), the toucher
    // `L5W61StubUpgradeInitTouch<suffix>` (27 bytes, as the original).
    static String row(String suffix, boolean otherThread) throws Exception {
        String internal = "io/smallrye/l5w61/Target" + suffix;
        byte[] toucher = bytesOf(TOUCHER);
        patch(toucher, TARGET, internal);
        patch(toucher, TOUCHER, "L5W61StubUpgradeInitTouch" + suffix);
        Class<?> t = MethodHandles.lookup().defineClass(toucher);
        t.getDeclaredMethod("touch").invoke(null);
        byte[] target = bytesOf(TARGET);
        patch(target, TARGET, internal);
        Class<?> real;
        try {
            real = new Definer().define(internal.replace('/', '.'), target);
        } catch (Throwable e) {
            return "define " + e.getClass().getName() + ": " + e.getMessage();
        }
        if (!otherThread) return read(real);
        String[] out = new String[1];
        Thread th = new Thread(() -> out[0] = read(real));
        th.start();
        th.join();
        return out[0];
    }

    public static void main(String[] args) throws Exception {
        System.out.println("first=" + row("AA", false));
        System.out.println("first-other-thread=" + row("AB", true));
        System.out.println("again=" + row("AC", false));
    }
}

class L5W61StubUpgradeInitTarget {
    public static String state = "clinit-ran";
    public L5W61StubUpgradeInitTarget() {}
}

class L5W61StubUpgradeInitToucher {
    static void touch() {
        for (int i = 0; i < 2; i++) {
            try {
                new L5W61StubUpgradeInitTarget();
            } catch (Throwable ignored) {
            }
        }
    }
}
