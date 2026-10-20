// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 120, lane L2: `CRATONVM_JIT_INVOKE_SITE_CLASSIFIER`
// (default off; proposal
// docs/internal/fixed-bugs/interpreter-i37-L2-proposal-one-invoke-site-classifier-for-every-compile-door-DONE-20261224.md),
// the SPLICE door. The OSR / eager twin is L2W119InvokespecialGrandparentOsr.
//
// C (extends B extends A) has a tiny private `step()` whose `invokespecial`
// names the GRANDPARENT A.m while B overrides m, and `loop(n)` calls `step()`.
// javac never emits such a super call, but JVMS 6.5 selects from C's direct
// superclass, so HotSpot runs B.m. `loop` is called many times with a short n,
// so it compiles at method entry and splices `step` (a two-instruction
// private callee); the splice used to be refused for the site
// (`inline-resolve REFUSED ... invokespecial-selection-start`), and with the
// classifier it binds B.m inside the splice. Every line must be the same in
// both modes, JIT and --nojit, with the switch unset and =1.
//
// Run:
//   javac -d out tools/probes/interp/L2/L2W120InvokespecialGrandparentSplice.java
//   java -cp out L2W120InvokespecialGrandparentSplice                                        (HotSpot)
//   cratonvm --jdk-only -cp out L2W120InvokespecialGrandparentSplice                         (and --compatible, --nojit)
//   CRATONVM_JIT_INVOKE_SITE_CLASSIFIER=1 cratonvm --jdk-only -cp out L2W120InvokespecialGrandparentSplice
// Census: CRATONVM_DBG_JITC=1 [switch] ... 2>&1 | grep -E 'invokespecial-selection|inline-resolve REFUSED.*selection'
//   switch unset: `inline-resolve REFUSED ... invokespecial-selection-start` for `step`;
//   switch on: `invokespecial-selection SUBSTITUTED splice: <pkg>/A.m()I ... binds <pkg>/B`.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags; -Xint gives the same)
// warm 2000000
// loop 2000000
// direct B
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

public class L2W120InvokespecialGrandparentSplice {
    static final ClassDesc I = ConstantDescs.CD_int;

    /// `m()` returns `value`; a public no-arg constructor calling `superDesc.<init>`.
    static byte[] klass(ClassDesc self, ClassDesc superDesc, int value) {
        return ClassFile.of().build(self, clb -> {
            clb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER).withSuperclass(superDesc);
            clb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC,
                    cb -> cb.aload(0).invokespecial(superDesc, ConstantDescs.INIT_NAME,
                            ConstantDescs.MTD_void).return_());
            clb.withMethodBody("m", MethodTypeDesc.of(I), ClassFile.ACC_PUBLIC,
                    cb -> cb.ldc(value).ireturn());
        });
    }

    /// C extends B: `int loop(int n)` sums `invokespecial A.m()` n times;
    /// `String who()` answers "A" / "B" from one call.
    static byte[] c(ClassDesc self, ClassDesc b, ClassDesc a) {
        return ClassFile.of().build(self, clb -> {
            clb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER).withSuperclass(b);
            clb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC,
                    cb -> cb.aload(0).invokespecial(b, ConstantDescs.INIT_NAME,
                            ConstantDescs.MTD_void).return_());
            // private int step() { return super-of-super.m(); }  (aload_0; invokespecial A.m; ireturn)
            clb.withMethodBody("step", MethodTypeDesc.of(I), ClassFile.ACC_PRIVATE,
                    cb -> cb.aload(0).invokespecial(a, "m", MethodTypeDesc.of(I)).ireturn());
            clb.withMethodBody("loop", MethodTypeDesc.of(I, I), ClassFile.ACC_PUBLIC, cb -> {
                Label top = cb.newLabel();
                Label done = cb.newLabel();
                cb.iconst_0().istore(2).iconst_0().istore(3);
                cb.labelBinding(top);
                cb.iload(3).iload(1).if_icmpge(done);
                cb.iload(2).aload(0).invokespecial(self, "step", MethodTypeDesc.of(I)).iadd().istore(2);
                cb.iinc(3, 1).goto_(top);
                cb.labelBinding(done);
                cb.iload(2).ireturn();
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

    public static void main(String[] args) throws Exception {
        Path dir = classPathDir();
        if (dir == null) {
            System.out.println("no class-path directory");
            return;
        }
        String pkg = "l2w120s" + ProcessHandle.current().pid();
        Path pdir = dir.resolve(pkg);
        Files.createDirectories(pdir);
        List<Path> written = new ArrayList<>();
        try {
            ClassDesc a = ClassDesc.of(pkg + ".A");
            ClassDesc b = ClassDesc.of(pkg + ".B");
            ClassDesc c = ClassDesc.of(pkg + ".C");
            Object[][] files = {
                {"A", klass(a, ConstantDescs.CD_Object, 1)},
                {"B", klass(b, a, 2)},
                {"C", c(c, b, a)},
            };
            for (Object[] f : files) {
                Path p = pdir.resolve(f[0] + ".class");
                Files.write(p, (byte[]) f[1]);
                written.add(p);
            }
            Class<?> cc = Class.forName(pkg + ".C");
            Object o = cc.getConstructor().newInstance();
            java.lang.reflect.Method loop = cc.getMethod("loop", int.class);
            // Many short calls: `loop` compiles at method entry and splices
            // `step` (20 000 x 50 iterations, each adding B.m's 2).
            int warm = 0;
            for (int i = 0; i < 20_000; i++) {
                warm += (int) loop.invoke(o, 50);
            }
            System.out.println("warm " + warm);
            int sum = (int) loop.invoke(o, 1_000_000);
            System.out.println("loop " + sum);
            int one = (int) loop.invoke(o, 1);
            System.out.println("direct " + (one == 2 ? "B" : one == 1 ? "A" : "?" + one));
        } finally {
            for (Path p : written) {
                Files.deleteIfExists(p);
            }
            Files.deleteIfExists(pdir);
        }
    }
}
