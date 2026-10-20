// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 115, lane L2:
// `CRATONVM_JIT_DEFERRED_FIELD_SITE_RETIER_ROWLESS_ONLY` (default OFF; page
// docs/internal/fixed-bugs/interpreter-i100-L2-proposal-fall-through-carries-its-bail-site-KEPT-OFF-20261019.md).
//
// The optimizing compile of `Hot.applyAsInt` falls through to single-pass on a
// ROW-LESS field site of its own (a `getstatic` / `putstatic` of a class-path
// class nobody has loaded, placed after a call so the field-site trap is
// refused): `cratonvm_jit::IrFallThroughCause::RowlessFieldSite`. The re-tier
// must arm for it with the switch off (as since wave 100) AND with it on.
// `HotC` is the control (owner loaded before warm-up: no deferral, no arm).
// Output is the same in every arm; only the census differs.
//
// Run (one binary; --jdk-only and --compatible; JIT on -- with --nojit the
// census is empty and the lines still match):
//   javac -d out tools/probes/interp/L2/L2W115RetierCause.java
//   java -cp out L2W115RetierCause                                        (HotSpot reference)
//   cratonvm --jdk-only -cp out L2W115RetierCause
//   CRATONVM_JIT_DEFERRED_FIELD_SITE_RETIER_ROWLESS_ONLY=1 cratonvm --jdk-only -cp out L2W115RetierCause
// Census (both arms):
//   CRATONVM_DBG_JITC=1 [switch] cratonvm --jdk-only -cp out L2W115RetierCause 2>&1 \
//     | grep -E 'deferred-field-site retier|fell_through causes'
// expects, in BOTH arms, `deferred-field-site retier armed <pkg>/Hot.applyAsInt(II)I`
// and a `[c2-supersede] fell_through causes: rowless_field_site=N` line with N >= 1.
// No `armed` line with the switch on but one with it off: the builder's
// classification missed this bail (`ir.rs` `rowless_field_site_bail`).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// check-cold 1105 1105
// check-control 1105
// warm-sum-cold -414603325
// warm-sum-control -414603325
// HOTSPOT_EXPECTED_END
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

public class L2W115RetierCause {
    static final int CALLS = 200_000;
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
                // A call before the cold site: the IR trap's whole-method
                // replay rule refuses the site, so the build bails row-less.
                cb.iload(1).invokestatic(math, "abs", MethodTypeDesc.of(I, I)).istore(3);
                for (int k = 0; k < 8; k++) {
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

    static int warm(IntBinaryOperator op) {
        int sink = 0;
        for (int i = 0; i < CALLS; i++) {
            sink += op.applyAsInt(i, 0);
        }
        return sink;
    }

    public static void main(String[] args) throws Exception {
        Path dir = classPathDir();
        if (dir == null) {
            System.out.println("no class-path directory");
            return;
        }
        String pkg = "l2w115r" + ProcessHandle.current().pid();
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
            int sumCold = 0;
            int sumControl = 0;
            for (int w = 0; w < 3; w++) {
                sumCold = warm(hot);
                sumControl = warm(hotC);
            }
            Thread.sleep(300);
            // The one cold call: links the deferred sites, loads the owner.
            int first = hot.applyAsInt(5, 1);
            Thread.sleep(500);
            sumCold = warm(hot);
            sumControl = warm(hotC);
            System.out.println("check-cold " + first + " " + hot.applyAsInt(5, 1));
            System.out.println("check-control " + hotC.applyAsInt(5, 1));
            System.out.println("warm-sum-cold " + sumCold);
            System.out.println("warm-sum-control " + sumControl);
        } finally {
            for (Path p : written) {
                Files.deleteIfExists(p);
            }
            Files.deleteIfExists(pdir);
        }
    }
}
