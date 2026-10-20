// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench -- Interpreter round i5 wave 100, lane L2.
// `CRATONVM_JIT_DEFERRED_FIELD_SITE_RETIER` (default OFF; proposal A of
// i94-L2-proposal-deferred-field-site-retier-and-self-patch).
//
// An application-loader method whose cold branch touches a class-path class
// nobody has loaded, placed AFTER a call, so the optimizing tier's field-site
// trap is refused (whole-method replay rule) and the C2 task falls through to
// a single-pass body that defers the site. Before the switch that bail was for
// good: the method ran single-pass for the rest of the process even after the
// owner loaded. With the switch, the fell-through body is armed, and the first
// execution of its deferred site that links asks ONE more optimizing compile,
// which now finds the field row.
//
// Phases: warm-up with the cold branch never taken (C1 then C2; C2 falls
// through), a pause, ONE call that takes the cold branch (links the site and
// loads/initialises the owner), a pause for the background recompile, then
// interleaved timings.
//   cp-cold     Hot  (its cold owner loaded by the single cold call)
//   cp-control  HotC (its owner loaded and initialised before warm-up)
//
// A/B (one binary; interleave the arms, take medians; --jdk-only and --compatible):
//   $J/javac -d out tools/probes/interp/L2/L2W100DeferredFieldRetierBench.java
//   A (off)  $B --java-home $J/.. -cp out L2W100DeferredFieldRetierBench
//   B (on)   CRATONVM_JIT_DEFERRED_FIELD_SITE_RETIER=1 $B --java-home $J/.. -cp out L2W100DeferredFieldRetierBench
//   $J/java -cp out L2W100DeferredFieldRetierBench    (reference)
// Expected: A: cp-cold at the single-pass tier's speed (slower than cp-control);
// B: cp-cold within noise of cp-control.
//
// Census (B, --jdk-only):
//   CRATONVM_DBG_JITC=1 CRATONVM_JIT_DEFERRED_FIELD_SITE_RETIER=1 $B ... 2>&1 \
//     | grep -E 'deferred-field-site retier|ir field-site trap refused|deferred-field-sites' | sort | uniq -c
// expects `deferred-field-site retier armed <pkg>/Hot.applyAsInt(II)I compile_id=<n>` and then
// `deferred-field-site retier <pkg>/Hot.applyAsInt(II)I compile_id=<n>: Enqueued`.
// No `armed` line: the C2 task did not fall through (the trap was planted, or
// C2 never ran); no second line: the site's helper did not see the armed body
// (innermost compile id unpublished).
//
// Timing lines start with "TIME "; the last lines are deterministic:
//   check-cold 1105 1105
//   check-control 1105
import java.io.File;
import java.lang.classfile.ClassFile;
import java.lang.classfile.Label;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.function.IntBinaryOperator;

public class L2W100DeferredFieldRetierBench {
    static final int CALLS = 200_000;
    static final int REPS = 7;
    static final ClassDesc I = ConstantDescs.CD_int;
    static final int PUBLIC = ClassFile.ACC_PUBLIC;
    static final int STATIC = ClassFile.ACC_STATIC;

    /// `public static int seen, BASE` with `BASE = 1100` in `<clinit>`.
    static byte[] cold(ClassDesc self) {
        return ClassFile.of().build(self, clb -> clb
                .withFlags(PUBLIC)
                .withField("seen", I, PUBLIC | STATIC)
                .withField("BASE", I, PUBLIC | STATIC)
                .withMethodBody(ConstantDescs.CLASS_INIT_NAME, ConstantDescs.MTD_void, STATIC,
                        cb -> cb.sipush(1100).putstatic(self, "BASE", I).return_()));
    }

    static byte[] hot(ClassDesc self, ClassDesc owner) {
        ClassDesc math = ClassDesc.of("java.lang.Math");
        return ClassFile.of().build(self, clb -> {
            clb.withFlags(PUBLIC).withInterfaceSymbols(ClassDesc.of("java.util.function.IntBinaryOperator"));
            clb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, PUBLIC,
                    cb -> cb.aload(0)
                            .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                    ConstantDescs.MTD_void)
                            .return_());
            clb.withMethodBody("applyAsInt", MethodTypeDesc.of(I, I, I), PUBLIC, cb -> {
                Label hotExit = cb.newLabel();
                // A call textually before the cold site: the IR trap's
                // whole-method replay rule refuses the site, so C2 bails.
                cb.iload(1).invokestatic(math, "abs", MethodTypeDesc.of(I, I)).istore(3);
                for (int k = 0; k < 16; k++) {
                    cb.iload(3).bipush(31).imul().bipush(k).iadd().istore(3);
                    cb.iload(3).iload(3).bipush(7).iushr().ixor().istore(3);
                }
                cb.iload(2).iconst_1().if_icmpne(hotExit);
                cb.getstatic(owner, "BASE", I).iload(1).iadd().istore(4);
                cb.iload(3).putstatic(owner, "seen", I);
                cb.iload(4).ireturn();
                cb.labelBinding(hotExit);
                cb.iload(3).ireturn();
            });
        });
    }

    static Path classPathDir() {
        for (String e : System.getProperty("java.class.path").split(File.pathSeparator)) {
            File f = new File(e.isEmpty() ? "." : e);
            if (f.isDirectory()) {
                return f.toPath();
            }
        }
        return null;
    }

    static long time(IntBinaryOperator op) {
        int sink = 0;
        long t0 = System.nanoTime();
        for (int i = 0; i < CALLS; i++) {
            sink += op.applyAsInt(i, 0);
        }
        long t = System.nanoTime() - t0;
        if (sink == 7) {
            System.out.println("!");
        }
        return t;
    }

    static long median(long[] xs) {
        long[] c = xs.clone();
        java.util.Arrays.sort(c);
        return c[c.length / 2];
    }

    public static void main(String[] args) throws Exception {
        Path dir = classPathDir();
        if (dir == null) {
            System.out.println("no class-path directory");
            return;
        }
        String pkg = "l2w100r" + ProcessHandle.current().pid();
        Path pdir = dir.resolve(pkg);
        Files.createDirectories(pdir);
        List<Path> written = new ArrayList<>();
        try {
            String[][] classes = {{"Hot", "Cold"}, {"HotC", "ColdC"}};
            for (String[] c : classes) {
                ClassDesc hotD = ClassDesc.of(pkg + "." + c[0]);
                ClassDesc ownerD = ClassDesc.of(pkg + "." + c[1]);
                Path h = pdir.resolve(c[0] + ".class");
                Path o = pdir.resolve(c[1] + ".class");
                Files.write(h, hot(hotD, ownerD));
                Files.write(o, cold(ownerD));
                written.add(h);
                written.add(o);
            }
            ClassLoader app = ClassLoader.getSystemClassLoader();
            Class.forName(pkg + ".ColdC", true, app);
            IntBinaryOperator hot = (IntBinaryOperator) Class.forName(pkg + ".Hot", true, app)
                    .getConstructor().newInstance();
            IntBinaryOperator hotC = (IntBinaryOperator) Class.forName(pkg + ".HotC", true, app)
                    .getConstructor().newInstance();
            // Warm-up: the cold branch never runs; C1, then C2 (which bails).
            for (int w = 0; w < 3; w++) {
                time(hot);
                time(hotC);
            }
            Thread.sleep(300);
            // The one cold call: links the deferred sites, loads the owner.
            int first = hot.applyAsInt(5, 1);
            Thread.sleep(500);
            long[] cold = new long[REPS];
            long[] control = new long[REPS];
            for (int r = 0; r < REPS; r++) {
                cold[r] = time(hot);
                control[r] = time(hotC);
            }
            System.out.println("TIME cp-cold " + median(cold) / CALLS + " ns/call");
            System.out.println("TIME cp-control " + median(control) / CALLS + " ns/call");
            System.out.println("check-cold " + first + " " + hot.applyAsInt(5, 1));
            System.out.println("check-control " + hotC.applyAsInt(5, 1));
        } finally {
            for (Path p : written) {
                Files.deleteIfExists(p);
            }
            Files.deleteIfExists(pdir);
        }
    }
}
