// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Item 1 of docs/internal/fixed-bugs/interpreter-L6-a-compiled-frame-in-a-call-at-the-redefinition-keeps-its-old-splice-FIXED-20261007.md
// (2026-10-07): a compiled body whose only way into Java code at the
// redefinition is a HELPER the compiled code calls -- here the class
// initializer a `new` / `getstatic` / `putstatic` of a not-yet-initialized
// class runs -- and that helper's Java code retransforms a class the body
// spliced. HotSpot's compiled frame (an uncommon trap or a patching stub at
// the uninitialized access, and the lazy deopt of a marked frame on return)
// reads the NEW bytecode right after; so must ours. Each row is warmed up
// with `go == false` (the access never runs, so its class stays
// uninitialized), then run once with `go == true`.
//
// Rows (`*-sync`: the same body as a `static synchronized` method):
//   new        `new InitNew()`, whose `<clinit>` retransforms `Callee`;
//   getstatic  `InitGet.X`, likewise;
//   putstatic  `InitPut.X = 1`, likewise;
//   new-ctor   `new InitCtor()` (an empty constructor), whose `<clinit>`
//              retransforms `Marker`'s empty constructor into one that
//              counts, then `new Marker()`: the compiled body may have elided
//              both constructors, so no call is left after the helper.
//
// HOTSPOT_EXPECTED_BEGIN (Temurin 25.0.3 with the agent, default flags and -Xint)
// new first-after-new=true
// getstatic first-after-new=true
// putstatic first-after-new=true
// new-ctor ctor-ran=true
// new-sync first-after-new=true
// getstatic-sync first-after-new=true
// putstatic-sync first-after-new=true
// new-ctor-sync ctor-ran=true
// HOTSPOT_EXPECTED_END
// Without the agent both VMs print `no agent`.
//
// CratonVM before 2026-10-07: `getstatic`, `putstatic` and their `-sync`
// twins printed `false` in both tiers; since then HotSpot's lines (the i24-L6
// page's "Fixed (2026-10-07)").
//
// SETUP: a jar whose manifest has
//     Premain-Class: I24PostHelperRedefinition$Agent
//     Can-Retransform-Classes: true
// containing I24PostHelperRedefinition*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar I24PostHelperRedefinition
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;

public class I24PostHelperRedefinition {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class Callee {
        static int value() {
            return 12345;
        }
    }

    /** Counted by `Marker`'s constructor once a retransform rewrote it. */
    public static int made;

    public static class Marker {
        public Marker() {}
    }

    public static class MarkerS {
        public MarkerS() {}
    }

    /**
     * Rewrites `sipush 12345; ireturn` to answer 12345 + n on the n-th
     * transform of `Callee`; on a retransform of `Marker` / `MarkerS`, the
     * empty constructor `aload_0; invokespecial Object.<init>; return`
     * becomes `... ; getstatic made; iconst_1; iadd; putstatic made; return`.
     */
    static final class Bump implements ClassFileTransformer {
        int bumps;

        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (redefined != null && ("I24PostHelperRedefinition$Marker".equals(className)
                    || "I24PostHelperRedefinition$MarkerS".equals(className))) {
                return countingConstructor(bytes);
            }
            if (!"I24PostHelperRedefinition$Callee".equals(className)) {
                return null;
            }
            bumps++;
            byte[] out = bytes.clone();
            for (int i = 0; i + 3 < out.length; i++) {
                if (out[i] == 0x11 && out[i + 1] == 0x30 && out[i + 2] == 0x39
                        && out[i + 3] == (byte) 0xAC) {
                    out[i + 2] = (byte) (0x39 + bumps);
                }
            }
            return out;
        }
    }

    static byte[] countingConstructor(byte[] bytes) {
        java.lang.classfile.ClassFile cf = java.lang.classfile.ClassFile.of();
        java.lang.classfile.ClassModel model = cf.parse(bytes);
        java.lang.constant.ClassDesc me = java.lang.constant.ClassDesc.of("I24PostHelperRedefinition");
        return cf.transformClass(model, java.lang.classfile.ClassTransform.transformingMethodBodies(
                m -> m.methodName().equalsString("<init>"),
                (code, element) -> {
                    if (element instanceof java.lang.classfile.instruction.ReturnInstruction) {
                        code.getstatic(me, "made", java.lang.constant.ConstantDescs.CD_int)
                                .iconst_1().iadd()
                                .putstatic(me, "made", java.lang.constant.ConstantDescs.CD_int);
                    }
                    code.with(element);
                }));
    }

    static void retransformMarker(Class<?> c) {
        try {
            inst.retransformClasses(c);
        } catch (Exception e) {
            throw new RuntimeException(e);
        }
    }

    static void retransform() {
        try {
            inst.retransformClasses(Callee.class);
        } catch (Exception e) {
            throw new RuntimeException(e);
        }
    }

    static class InitNew { static { retransform(); } }
    static class InitGet { static int X = 1; static { retransform(); } }
    static class InitPut { static int X; static { retransform(); } }
    static class InitNewS { static { retransform(); } }
    static class InitGetS { static int X = 1; static { retransform(); } }
    static class InitPutS { static int X; static { retransform(); } }

    static class InitCtor { static { retransformMarker(Marker.class); } }
    static class InitCtorS { static { retransformMarker(MarkerS.class); } }

    static volatile Object sinkObj;

    static int ctorRow(boolean go) {
        if (go) {
            sinkObj = new InitCtor();
            sinkObj = new Marker();
        }
        return made;
    }

    static synchronized int ctorSync(boolean go) {
        if (go) {
            sinkObj = new InitCtorS();
            sinkObj = new MarkerS();
        }
        return made;
    }

    static int newRow(boolean go) {
        if (go) {
            sinkObj = new InitNew();
            return Callee.value();
        }
        return Callee.value();
    }

    static int getRow(boolean go) {
        if (go) {
            int x = InitGet.X;
            return Callee.value() + (x >>> 31);
        }
        return Callee.value();
    }

    static int putRow(boolean go) {
        if (go) {
            InitPut.X = 1;
            return Callee.value();
        }
        return Callee.value();
    }

    static synchronized int newSync(boolean go) {
        if (go) {
            sinkObj = new InitNewS();
            return Callee.value();
        }
        return Callee.value();
    }

    static synchronized int getSync(boolean go) {
        if (go) {
            int x = InitGetS.X;
            return Callee.value() + (x >>> 31);
        }
        return Callee.value();
    }

    static synchronized int putSync(boolean go) {
        if (go) {
            InitPutS.X = 1;
            return Callee.value();
        }
        return Callee.value();
    }

    interface Row {
        int run(boolean go);
    }

    static final long WARM = 1L << 16;
    static volatile long sink;

    static void row(String name, Row r) {
        long sum = 0;
        for (long k = 0; k < WARM; k++) {
            sum += r.run(false);
        }
        sink = sum;
        int before = Callee.value();
        int after = r.run(true);
        System.out.println(name + " first-after-new=" + (after != before));
    }

    static void ctorRow(String name, Row r) {
        long sum = 0;
        for (long k = 0; k < WARM; k++) {
            sum += r.run(false);
        }
        sink = sum;
        int before = made;
        int after = r.run(true);
        System.out.println(name + " ctor-ran=" + (after != before));
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRetransformClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        i.addTransformer(new Bump(), true);
        row("new", I24PostHelperRedefinition::newRow);
        row("getstatic", I24PostHelperRedefinition::getRow);
        row("putstatic", I24PostHelperRedefinition::putRow);
        ctorRow("new-ctor", I24PostHelperRedefinition::ctorRow);
        row("new-sync", I24PostHelperRedefinition::newSync);
        row("getstatic-sync", I24PostHelperRedefinition::getSync);
        row("putstatic-sync", I24PostHelperRedefinition::putSync);
        ctorRow("new-ctor-sync", I24PostHelperRedefinition::ctorSync);
    }
}
