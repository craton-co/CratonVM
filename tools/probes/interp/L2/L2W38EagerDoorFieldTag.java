// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 38, lane L2: item 9 of the wave-37 compile-door
// review. The eager first-call door (`execute` in
// vm/src/runtime/interpreter.rs, `CRATONVM_BG_COMPILE=0`) baked a field
// site's slot from the resolved field and its type tag from the constant
// pool's descriptor without asking whether the two describe the same field
// (`jit_bridge::jit_field_tag_agrees`, which the OSR door and the
// method-entry doors' `CpResolvers` ask), and defaulted an instance site with
// an empty descriptor to `I`.
//
// JVMS 4.5 lets one class declare two fields of one name with different
// descriptors. `l2t.H` declares `static int f`, `static long f`, `int g` and
// `String g`; its static methods read each pair, and each is first called
// reflectively (the route to `execute`, so the eager door compiles it at that
// call under `CRATONVM_BG_COMPILE=0`). Field resolution is keyed by name AND
// descriptor since 2026-08-28, so every door must read the right slot; the
// tag check is a tripwire that stays silent on this probe.
//
// Run: javac -d out L2W38EagerDoorFieldTag.java
//      CRATONVM_BG_COMPILE=0 cratonvm --java-home <jdk25> [--compatible] -cp out L2W38EagerDoorFieldTag
//      cratonvm --java-home <jdk25> [--nojit] -cp out L2W38EagerDoorFieldTag
//
// Expected HotSpot 25 output (default and -Xint), every CratonVM mode alike:
//   static f:I=7 f:J=8000000000
//   instance g:I=5 g:String=five
//
// Positive control (wave 38): the name-only resolution lever makes the two
// halves of a site disagree, and the eager door must now refuse the site:
//   CRATONVM_FIELD_RESOLUTION_NAME_ONLY=1 CRATONVM_BG_COMPILE=0 CRATONVM_DBG_JITC=1 \
//     cratonvm --java-home <jdk25> -cp out L2W38EagerDoorFieldTag 2>&1 \
//     | grep 'eager-first-call field-tag refusal l2t/H'
// prints at least one line (the stdout rows are then the interpreter's
// name-only answer, not HotSpot's: the lever is a diagnostic, not a mode).
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.reflect.Method;

public class L2W38EagerDoorFieldTag {
    static final ClassDesc H = ClassDesc.of("l2t.H");
    static final ClassDesc STRING = ConstantDescs.CD_String;
    static final int PUBLIC = ClassFile.ACC_PUBLIC;
    static final int STATIC = ClassFile.ACC_STATIC;

    static byte[] h() {
        return ClassFile.of().build(H, clb -> clb
                .withFlags(PUBLIC)
                .withField("f", ConstantDescs.CD_int, PUBLIC | STATIC)
                .withField("f", ConstantDescs.CD_long, PUBLIC | STATIC)
                .withField("g", ConstantDescs.CD_int, PUBLIC)
                .withField("g", STRING, PUBLIC)
                .withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, PUBLIC,
                        cb -> cb.aload(0)
                                .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                        ConstantDescs.MTD_void)
                                .aload(0).iconst_5().putfield(H, "g", ConstantDescs.CD_int)
                                .aload(0).ldc("five").putfield(H, "g", STRING)
                                .return_())
                .withMethodBody(ConstantDescs.CLASS_INIT_NAME, ConstantDescs.MTD_void, STATIC,
                        cb -> cb.bipush(7).putstatic(H, "f", ConstantDescs.CD_int)
                                .ldc(8_000_000_000L).putstatic(H, "f", ConstantDescs.CD_long)
                                .return_())
                // "static f:I=" + f:I + " f:J=" + f:J, built by the caller
                // from the two getters below.
                .withMethodBody("fi", MethodTypeDesc.of(ConstantDescs.CD_int), PUBLIC | STATIC,
                        cb -> cb.getstatic(H, "f", ConstantDescs.CD_int).ireturn())
                .withMethodBody("fj", MethodTypeDesc.of(ConstantDescs.CD_long), PUBLIC | STATIC,
                        cb -> cb.getstatic(H, "f", ConstantDescs.CD_long).lreturn())
                .withMethodBody("gi", MethodTypeDesc.of(ConstantDescs.CD_int, H), PUBLIC | STATIC,
                        cb -> cb.aload(0).getfield(H, "g", ConstantDescs.CD_int).ireturn())
                .withMethodBody("gs", MethodTypeDesc.of(STRING, H), PUBLIC | STATIC,
                        cb -> cb.aload(0).getfield(H, "g", STRING).areturn()));
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(L2W38EagerDoorFieldTag.class.getClassLoader());
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            if (!name.equals("l2t.H")) {
                throw new ClassNotFoundException(name);
            }
            byte[] b = h();
            return defineClass(name, b, 0, b.length);
        }
    }

    public static void main(String[] args) throws Exception {
        Class<?> h = new Loader().loadClass("l2t.H");
        Object obj = h.getConstructor().newInstance();
        Method fi = h.getMethod("fi");
        Method fj = h.getMethod("fj");
        Method gi = h.getMethod("gi", h);
        Method gs = h.getMethod("gs", h);
        Object a = null;
        Object b = null;
        Object c = null;
        Object d = null;
        for (int i = 0; i < 3; i++) {
            a = fi.invoke(null);
            b = fj.invoke(null);
            c = gi.invoke(null, obj);
            d = gs.invoke(null, obj);
        }
        System.out.println("static f:I=" + a + " f:J=" + b);
        System.out.println("instance g:I=" + c + " g:String=" + d);
    }
}
