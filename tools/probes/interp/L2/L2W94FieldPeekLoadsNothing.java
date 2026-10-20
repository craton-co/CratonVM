// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// probe -- Interpreter round i4 wave 94, lane L2.
// `interpreter-i93-L2-compile-door-field-peek-loads-class-path-owners-FIXED-20261127.md`: a
// compile must not load the owner of a field site the program has not run.
//
// HotSpot's compilers never load a class (C1 patches the site, C2 traps;
// JVMS 5.4.3 loads the owner when the site first runs). CratonVM's compile
// doors resolve each field site with a threadless peek whose last arm LOADED a
// class-path owner (`field_access.rs` `resolve_field_ref` ->
// `load_class_concurrent`), so the owner of a COLD site was defined when the
// method compiled.
//
// Observable without any diagnostic flag: WHICH bytes of the owner are
// defined. The probe writes three generated classes into the first directory
// of its class path (a fresh package per process: `l2w94p<pid>`), version 1 of
// `Cold` / `ColdF`, makes `Hot.applyAsInt` hot (its cold branches untouched),
// then OVERWRITES `Cold.class` / `ColdF.class` with version 2 and only then
// runs the cold sites. The application loader reads a class file when it
// loads the class, so the version the program sees says when that was:
//   HotSpot: version 2 for both (loaded at the site, after the overwrite);
//   a compile-time load: version 1.
//
// Rows:
//   getstatic version=<v>      a cold `getstatic Cold.VERSION` (returns it)
//   getfield npe=<b> version=<v>
//                              a cold `aconst_null; getfield ColdF.x` (the
//                              field resolution loads ColdF, then the null
//                              receiver throws NPE); the version read after
//   hot-after-cold sum=<n>     the cold getstatic site made hot after it ran
//                              (exercises CRATONVM_JIT_IR_FIELD_SITE_TRAP's
//                              trap + recompile when that switch is on)
//
// CratonVM, expected (the fix is coupled to the deferral, see the page):
//   default build: getstatic version=1 (the old compile-time load, kept while
//     CRATONVM_JIT_DEFERRED_FIELD_SITES is off) -- unless the method was not
//     compiled before the overwrite (then 2; timing-dependent);
//   CRATONVM_JIT_DEFERRED_FIELD_SITES=1: HotSpot's lines, also with
//     CRATONVM_JIT_IR_FIELD_SITE_TRAP=1;
//   CRATONVM_JIT_DEFERRED_FIELD_SITES=1 CRATONVM_JIT_DEFERRED_FIELD_PEEK_NO_LOAD=0:
//     version=1 again (the kill switch).
//   --nojit: HotSpot's lines.
// The getfield row also depends on the verifier not loading ColdF (HotSpot's
// does not: a null receiver is assignable to any class without loading it).
//
// Run (both modes):
//   $J/javac -d out tools/probes/interp/L2/L2W94FieldPeekLoadsNothing.java
//   $J/java -cp out L2W94FieldPeekLoadsNothing
//   CRATONVM_JIT_DEFERRED_FIELD_SITES=1 $B --java-home $J/.. -cp out L2W94FieldPeekLoadsNothing
//   (and --compatible; the class path's first entry must be a writable directory)
// Census: CRATONVM_DBG_JITC=1 ... 2>&1 | grep -E 'field peek: owner|unresolved-field TRAP|ir field-site traps|deferred-field-sites'
//
// HOTSPOT_EXPECTED_BEGIN
// warm 600000 sum=-1243809975
// getstatic version=2
// getfield npe=true version=2
// hot-after-cold sum=200000
// HOTSPOT_EXPECTED_END
import java.io.File;
import java.lang.classfile.ClassFile;
import java.lang.classfile.Label;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.function.IntBinaryOperator;

public class L2W94FieldPeekLoadsNothing {
    static final ClassDesc I = ConstantDescs.CD_int;
    static final int PUBLIC = ClassFile.ACC_PUBLIC;
    static final int STATIC = ClassFile.ACC_STATIC;

    /// `public static int VERSION` (= `version` in `<clinit>`), `public int x`.
    static byte[] cold(ClassDesc self, int version) {
        return ClassFile.of().build(self, clb -> clb
                .withFlags(PUBLIC)
                .withField("VERSION", I, PUBLIC | STATIC)
                .withField("x", I, PUBLIC)
                .withMethodBody(ConstantDescs.CLASS_INIT_NAME, ConstantDescs.MTD_void, STATIC,
                        cb -> cb.bipush(version).putstatic(self, "VERSION", I).return_()));
    }

    /// `applyAsInt(i, mode)`: eight mixing rounds; mode 1 returns
    /// `Cold.VERSION`, mode 2 `((ColdF) null).x`, any other mode the mix.
    static byte[] hot(ClassDesc self, ClassDesc cold, ClassDesc coldF) {
        return ClassFile.of().build(self, clb -> {
            clb.withFlags(PUBLIC).withInterfaceSymbols(ClassDesc.of("java.util.function.IntBinaryOperator"));
            clb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, PUBLIC,
                    cb -> cb.aload(0)
                            .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                    ConstantDescs.MTD_void)
                            .return_());
            clb.withMethodBody("applyAsInt", MethodTypeDesc.of(I, I, I), PUBLIC, cb -> {
                Label notOne = cb.newLabel();
                Label notTwo = cb.newLabel();
                cb.iload(1).istore(3);
                for (int k = 0; k < 8; k++) {
                    cb.iload(3).bipush(31).imul().bipush(k).iadd().istore(3);
                    cb.iload(3).iload(3).bipush(7).iushr().ixor().istore(3);
                }
                cb.iload(2).iconst_1().if_icmpne(notOne);
                cb.getstatic(cold, "VERSION", I).ireturn();
                cb.labelBinding(notOne);
                cb.iload(2).iconst_2().if_icmpne(notTwo);
                cb.aconst_null().getfield(coldF, "x", I).ireturn();
                cb.labelBinding(notTwo);
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

    public static void main(String[] args) throws Exception {
        Path dir = classPathDir();
        if (dir == null) {
            System.out.println("no class-path directory");
            return;
        }
        String pkg = "l2w94p" + ProcessHandle.current().pid();
        ClassDesc hotD = ClassDesc.of(pkg + ".Hot");
        ClassDesc coldD = ClassDesc.of(pkg + ".Cold");
        ClassDesc coldFD = ClassDesc.of(pkg + ".ColdF");
        Path pdir = dir.resolve(pkg);
        Files.createDirectories(pdir);
        Path hotP = pdir.resolve("Hot.class");
        Path coldP = pdir.resolve("Cold.class");
        Path coldFP = pdir.resolve("ColdF.class");
        try {
            Files.write(hotP, hot(hotD, coldD, coldFD));
            Files.write(coldP, cold(coldD, 1));
            Files.write(coldFP, cold(coldFD, 1));
            ClassLoader app = ClassLoader.getSystemClassLoader();
            IntBinaryOperator op = (IntBinaryOperator) Class.forName(pkg + ".Hot", true, app)
                    .getConstructor().newInstance();

            // Make it hot; give a background compiler time to finish.
            int sum = 0;
            int calls = 0;
            for (int round = 0; round < 3; round++) {
                for (int i = 0; i < 200_000; i++) {
                    sum += op.applyAsInt(i, 0);
                    calls++;
                }
                Thread.sleep(200);
            }
            System.out.println("warm " + calls + " sum=" + sum);

            // Version 2 on disk before any cold site runs.
            Files.write(coldP, cold(coldD, 2));
            Files.write(coldFP, cold(coldFD, 2));

            System.out.println("getstatic version=" + op.applyAsInt(0, 1));
            boolean npe = false;
            try {
                op.applyAsInt(0, 2);
            } catch (NullPointerException e) {
                npe = true;
            }
            int v = Class.forName(pkg + ".ColdF", true, app).getField("VERSION").getInt(null);
            System.out.println("getfield npe=" + npe + " version=" + v);

            int hotSum = 0;
            for (int i = 0; i < 100_000; i++) {
                hotSum += op.applyAsInt(i, 1);
            }
            System.out.println("hot-after-cold sum=" + hotSum);
        } finally {
            Files.deleteIfExists(hotP);
            Files.deleteIfExists(coldP);
            Files.deleteIfExists(coldFP);
            Files.deleteIfExists(pdir);
        }
    }
}
