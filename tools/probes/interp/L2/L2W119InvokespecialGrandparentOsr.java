// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 119, lane L2: `CRATONVM_JIT_INVOKE_SITE_CLASSIFIER`
// (default off; proposal
// docs/internal/fixed-bugs/interpreter-i37-L2-proposal-one-invoke-site-classifier-for-every-compile-door-DONE-20261224.md).
//
// A hot loop in C (extends B extends A) whose `invokespecial` names the
// GRANDPARENT A.m while B overrides m. javac never emits this (it names the
// direct superclass), but JVMS 6.5 selects from C's direct superclass, so
// HotSpot runs B.m. The single-pass OSR door used to deny the loop for this
// site (`invokespecial-selection REFUSED osr ...`); with the classifier it
// binds B.m. Every line must be the same in both modes, JIT and --nojit,
// with the switch unset and =1.
//
// Run:
//   javac -d out tools/probes/interp/L2/L2W119InvokespecialGrandparentOsr.java
//   java -cp out L2W119InvokespecialGrandparentOsr                                        (HotSpot)
//   cratonvm --jdk-only -cp out L2W119InvokespecialGrandparentOsr                         (and --compatible, --nojit)
//   CRATONVM_JIT_INVOKE_SITE_CLASSIFIER=1 cratonvm --jdk-only -cp out L2W119InvokespecialGrandparentOsr
// Census: CRATONVM_DBG_JITC=1 [switch] ... 2>&1 | grep -E 'invokespecial-selection (REFUSED|SUBSTITUTED)|invokespecial-selection:'
//   switch unset: `REFUSED osr ... C.loop`; switch on: `SUBSTITUTED osr ... binds <pkg>/B`.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags; -Xint gives the same)
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

public class L2W119InvokespecialGrandparentOsr {
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
            clb.withMethodBody("loop", MethodTypeDesc.of(I, I), ClassFile.ACC_PUBLIC, cb -> {
                Label top = cb.newLabel();
                Label done = cb.newLabel();
                cb.iconst_0().istore(2).iconst_0().istore(3);
                cb.labelBinding(top);
                cb.iload(3).iload(1).if_icmpge(done);
                cb.iload(2).aload(0).invokespecial(a, "m", MethodTypeDesc.of(I)).iadd().istore(2);
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
        String pkg = "l2w119g" + ProcessHandle.current().pid();
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
            // One long call: the loop is OSR'd mid-activation.
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
