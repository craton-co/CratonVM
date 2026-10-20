// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench -- Interpreter round i4 wave 94, lane L2. The price of HotSpot's
// class-load timing at a compile door (`CRATONVM_JIT_DEFERRED_FIELD_PEEK_NO_LOAD`,
// default ON but inert unless `CRATONVM_JIT_DEFERRED_FIELD_SITES` is on), and
// what `CRATONVM_JIT_IR_FIELD_SITE_TRAP` (default OFF) buys back.
//
// The case `L2W93DeferredFieldSiteBench` cannot reach: an APPLICATION-loader
// method whose cold branch touches a CLASS-PATH class nobody has loaded. Until
// wave 94 the compile doors' field peek loaded that owner when the method
// compiled, so the method compiled as if the site were warm. With the deferral
// on, the peek loads nothing (HotSpot parity) and the site has no field row:
// the single-pass tier defers it (`DirectHelperTable::field_site_cp`) and the
// optimizing tier bails the method to single-pass -- unless the IR trap
// switch lets it plant an uncommon trap at the site and keep the method.
//
// Classes are generated with java.lang.classfile and written into the first
// class-path directory (a fresh package per process, removed at exit), so the
// application loader defines them from the class path:
//   Hot.applyAsInt(i, cold)   sixteen mixing rounds; cold == 1 only: a
//                             `getstatic Cold.BASE` + `putstatic Cold.seen`
//   HotC.applyAsInt(i, cold)  the same body, its owner `ColdC` loaded and
//                             initialised before warm-up (the ceiling)
// Rows (NOT deterministic; medians of 7 interleaved reps, ns/call):
//   cp-cold     Hot  (its cold owner never loaded during the timing)
//   cp-control  HotC
//
// A/B (one binary; interleave the arms, take medians; --jdk-only and --compatible):
//   $J/javac -d out tools/probes/interp/L2/L2W94ClassPathColdFieldBench.java
//   A (old timing, compiled as warm)   $B --java-home $J/.. -cp out L2W94ClassPathColdFieldBench
//   B (parity, single-pass)            CRATONVM_JIT_DEFERRED_FIELD_SITES=1 $B ...
//   C (parity + IR trap)               CRATONVM_JIT_DEFERRED_FIELD_SITES=1 CRATONVM_JIT_IR_FIELD_SITE_TRAP=1 $B ...
//   D (w93 deferral, old timing)       CRATONVM_JIT_DEFERRED_FIELD_SITES=1 CRATONVM_JIT_DEFERRED_FIELD_PEEK_NO_LOAD=0 $B ...
//   $J/java -cp out L2W94ClassPathColdFieldBench    (reference)
// Expected: A ~ D ~ cp-control; B: cp-cold at the single-pass tier's speed
// (the price of parity without the trap); C: cp-cold back within noise of
// cp-control. If B ~ A the optimizing tier does not matter for this body and
// the IR trap is not worth turning on for it.
//
// Census (once per arm, --jdk-only):
//   CRATONVM_DBG_JITC=1 $B ... 2>&1 | grep -E 'field peek: owner|deferred-field-sites|ir field-site traps|unresolved-field TRAP|resolver-bail site=(static_)?field_resolve' | sort | uniq -c
// A and D: no line for `.../Hot.applyAsInt`. B: `field peek: owner <pkg>/Cold
// not loaded` and `deferred-field-sites <pkg>/Hot.applyAsInt(II)I: 2 site(s)`.
// C: also `unresolved-field TRAP admitted <pkg>/Hot.applyAsInt(II)I bci=..`
// (twice: the putstatic and the getstatic) and `ir field-site traps admitted
// <pkg>/Hot.applyAsInt(II)I: 2 site(s)`; and, where the exit census prints
// its `[c2-supersede] ir site traps planted:` line, `unresolved-field=2`.
//
// Wave 95 (lane L2): the timed calls never reach the cold branch, so cp-cold
// measures the TIER the method runs at, not the deferred helper: B and the
// wave-94 C both ran the single-pass body (18 ns) because the background C2
// task compiles through the callee door, which wave 94 did not wire (its
// admission lines never printed under the background compiler). Since wave
// 95 the C census lines carry `door=callee`, and the builder adds `ir
// field-site trap planted at pc=<p> in <pkg>/Hot.applyAsInt(II)I` per site
// (or `refused (...)`, which names why the method still bails).
//
// The last lines are deterministic:
//   check-cold 1105
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

public class L2W94ClassPathColdFieldBench {
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
        return ClassFile.of().build(self, clb -> {
            clb.withFlags(PUBLIC).withInterfaceSymbols(ClassDesc.of("java.util.function.IntBinaryOperator"));
            clb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, PUBLIC,
                    cb -> cb.aload(0)
                            .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                    ConstantDescs.MTD_void)
                            .return_());
            clb.withMethodBody("applyAsInt", MethodTypeDesc.of(I, I, I), PUBLIC, cb -> {
                Label hotExit = cb.newLabel();
                cb.iload(1).istore(3);
                for (int k = 0; k < 16; k++) {
                    cb.iload(3).bipush(31).imul().bipush(k).iadd().istore(3);
                    cb.iload(3).iload(3).bipush(7).iushr().ixor().istore(3);
                }
                cb.iload(2).iconst_1().if_icmpne(hotExit);
                // The read first: an IR trap is planted only where no store
                // or call precedes it in the bytecode (whole-method replay).
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
        String pkg = "l2w94b" + ProcessHandle.current().pid();
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
            // The control's owner is loaded and initialised up front.
            Class.forName(pkg + ".ColdC", true, app);
            IntBinaryOperator hot = (IntBinaryOperator) Class.forName(pkg + ".Hot", true, app)
                    .getConstructor().newInstance();
            IntBinaryOperator hotC = (IntBinaryOperator) Class.forName(pkg + ".HotC", true, app)
                    .getConstructor().newInstance();
            long[] cold = new long[REPS];
            long[] control = new long[REPS];
            for (int r = 0; r < REPS; r++) {
                cold[r] = time(hot);
                control[r] = time(hotC);
            }
            System.out.println("cp-cold " + median(cold) / CALLS + " ns/call");
            System.out.println("cp-control " + median(control) / CALLS + " ns/call");
            System.out.println("check-cold " + hot.applyAsInt(5, 1));
            System.out.println("check-control " + hotC.applyAsInt(5, 1));
        } finally {
            for (Path p : written) {
                Files.deleteIfExists(p);
            }
            Files.deleteIfExists(pdir);
        }
    }
}
