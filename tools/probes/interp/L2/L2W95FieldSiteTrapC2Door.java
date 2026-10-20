// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// probe -- Interpreter round i4 wave 95, lane L2. `CRATONVM_JIT_IR_FIELD_SITE_TRAP`
// (default OFF) at the door the background C2 task compiles through
// (`jit_bridge.rs` `try_jit_compile_callee_slow`, wired in wave 95; wave 94
// wired only the upgrade door, which answers an existing single-pass body
// instead of compiling, so the trap was never admitted under the background
// compiler). Correctness half of `L2W94ClassPathColdFieldBench` arm C.
//
// One generated APPLICATION-loader method (`Hot.applyAsInt(i, mode)`, classes
// written into the first class-path directory, a fresh package per process)
// is made hot with mode 0, so its three cold sites name class-path owners
// nobody has loaded; with the deferral and the trap switch the optimizing body
// plants an uncommon trap at each. Then each cold site runs once:
//   mode 1: `getstatic Cold.BASE` (+ i)  -- Cold.<clinit> prints, at the site
//   mode 2: `aconst_null; getfield Holder.f` -- resolves Holder, then NPE
//   mode 3: `putstatic Cold2.seen = i; getstatic Cold2.seen` -- Cold2.<clinit>
//           prints, at the site
// and the method is made hot again (the trap's recompile) and mode 1 / 3 rerun.
// Every row is deterministic; the warm rows compare the compiled answers with
// a plain-Java reference of the same mixing.
//
// What a wrong trap looks like: a `<clinit>` line printed early (at compile
// time) or twice, a mode row with the typed zero the trap pushes for its
// unreachable tail (`mode1 5`, `mode3 0`), `mode2` naming another exception,
// or an InternalError from a refused replay.
//
// Run (both modes; the class path's first entry must be a writable directory):
//   $J/javac -d out tools/probes/interp/L2/L2W95FieldSiteTrapC2Door.java
//   $J/java -cp out L2W95FieldSiteTrapC2Door
//   $B --java-home $J/.. -cp out L2W95FieldSiteTrapC2Door
//   CRATONVM_JIT_IR_FIELD_SITE_TRAP=1 $B --java-home $J/.. -cp out L2W95FieldSiteTrapC2Door
//   (CRATONVM_JIT_DEFERRED_FIELD_SITES is default ON on x86-64 since wave 94)
// Census (proves the trap reached the C2 body):
//   CRATONVM_DBG_JITC=1 CRATONVM_JIT_IR_FIELD_SITE_TRAP=1 $B ... 2>&1 \
//     | grep -E 'unresolved-field TRAP|ir field-site trap|uninit-new TRAP taken' | sort | uniq -c
// Expected with the switch: `unresolved-field TRAP admitted <pkg>/Hot.applyAsInt(II)I
// bci=.. cp=.. door=callee` (three sites), `ir field-site traps admitted
// <pkg>/Hot.applyAsInt(II)I: 3 site(s)`, `ir field-site trap planted at pc=..`
// (three), and after the mode rows `uninit-new TRAP taken ... site spent,
// late-bind recompile requested` (the trap service is shared code). Without
// the switch: none of these lines.
//
// HOTSPOT_EXPECTED_BEGIN
// warm ok
// Cold.<clinit>
// mode1 1105
// mode1 1107
// mode2 java.lang.NullPointerException
// Cold2.<clinit>
// mode3 42
// rewarm ok
// mode1 1109
// mode3 43
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

public class L2W95FieldSiteTrapC2Door {
    static final ClassDesc I = ConstantDescs.CD_int;
    static final int PUBLIC = ClassFile.ACC_PUBLIC;
    static final int STATIC = ClassFile.ACC_STATIC;
    static final ClassDesc PRINT_STREAM = ClassDesc.of("java.io.PrintStream");

    /// `public static int seen, BASE`; `<clinit>` prints `<name>.<clinit>`
    /// and sets `BASE = 1100`.
    static byte[] cold(ClassDesc self, String name) {
        return ClassFile.of().build(self, clb -> clb
                .withFlags(PUBLIC)
                .withField("seen", I, PUBLIC | STATIC)
                .withField("BASE", I, PUBLIC | STATIC)
                .withMethodBody(ConstantDescs.CLASS_INIT_NAME, ConstantDescs.MTD_void, STATIC,
                        cb -> cb.getstatic(ClassDesc.of("java.lang.System"), "out", PRINT_STREAM)
                                .ldc(name + ".<clinit>")
                                .invokevirtual(PRINT_STREAM, "println",
                                        MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_String))
                                .sipush(1100)
                                .putstatic(self, "BASE", I)
                                .return_()));
    }

    /// `public int f` and a no-arg constructor.
    static byte[] holder(ClassDesc self) {
        return ClassFile.of().build(self, clb -> clb
                .withFlags(PUBLIC)
                .withField("f", I, PUBLIC)
                .withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, PUBLIC,
                        cb -> cb.aload(0)
                                .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                        ConstantDescs.MTD_void)
                                .return_()));
    }

    static byte[] hot(ClassDesc self, ClassDesc cold, ClassDesc holder, ClassDesc cold2) {
        return ClassFile.of().build(self, clb -> {
            clb.withFlags(PUBLIC).withInterfaceSymbols(ClassDesc.of("java.util.function.IntBinaryOperator"));
            clb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, PUBLIC,
                    cb -> cb.aload(0)
                            .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                    ConstantDescs.MTD_void)
                            .return_());
            clb.withMethodBody("applyAsInt", MethodTypeDesc.of(I, I, I), PUBLIC, cb -> {
                Label not1 = cb.newLabel();
                Label not2 = cb.newLabel();
                Label not3 = cb.newLabel();
                cb.iload(1).istore(3);
                for (int k = 0; k < 16; k++) {
                    cb.iload(3).bipush(31).imul().bipush(k).iadd().istore(3);
                    cb.iload(3).iload(3).bipush(7).iushr().ixor().istore(3);
                }
                // The reads first, the store last: the optimizing tier plants a
                // trap only where no store or call precedes it in the bytecode.
                cb.iload(2).iconst_1().if_icmpne(not1);
                cb.getstatic(cold, "BASE", I).iload(1).iadd().ireturn();
                cb.labelBinding(not1);
                cb.iload(2).iconst_2().if_icmpne(not2);
                cb.aconst_null().getfield(holder, "f", I).ireturn();
                cb.labelBinding(not2);
                cb.iload(2).iconst_3().if_icmpne(not3);
                cb.iload(1).putstatic(cold2, "seen", I);
                cb.getstatic(cold2, "seen", I).ireturn();
                cb.labelBinding(not3);
                cb.iload(3).ireturn();
            });
        });
    }

    /// The same mixing in plain Java: the reference for the warm rows.
    static int mix(int i) {
        int x = i;
        for (int k = 0; k < 16; k++) {
            x = x * 31 + k;
            x ^= x >>> 7;
        }
        return x;
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

    /// `reps` x 200k mode-0 calls; true when every answer matched `mix`.
    static boolean warm(IntBinaryOperator op, int reps) throws InterruptedException {
        boolean ok = true;
        for (int r = 0; r < reps; r++) {
            for (int i = 0; i < 200_000; i++) {
                if (op.applyAsInt(i, 0) != mix(i)) {
                    ok = false;
                }
            }
            // Give a background compile a chance to publish between reps.
            Thread.sleep(20);
        }
        return ok;
    }

    public static void main(String[] args) throws Exception {
        Path dir = classPathDir();
        if (dir == null) {
            System.out.println("no class-path directory");
            return;
        }
        String pkg = "l2w95t" + ProcessHandle.current().pid();
        Path pdir = dir.resolve(pkg);
        Files.createDirectories(pdir);
        List<Path> written = new ArrayList<>();
        try {
            ClassDesc hotD = ClassDesc.of(pkg + ".Hot");
            ClassDesc coldD = ClassDesc.of(pkg + ".Cold");
            ClassDesc holderD = ClassDesc.of(pkg + ".Holder");
            ClassDesc cold2D = ClassDesc.of(pkg + ".Cold2");
            Object[][] files = {
                {"Hot", hot(hotD, coldD, holderD, cold2D)},
                {"Cold", cold(coldD, "Cold")},
                {"Holder", holder(holderD)},
                {"Cold2", cold(cold2D, "Cold2")},
            };
            for (Object[] f : files) {
                Path p = pdir.resolve(f[0] + ".class");
                Files.write(p, (byte[]) f[1]);
                written.add(p);
            }
            IntBinaryOperator hot = (IntBinaryOperator) Class.forName(pkg + ".Hot", true,
                    ClassLoader.getSystemClassLoader()).getConstructor().newInstance();
            System.out.println(warm(hot, 8) ? "warm ok" : "warm MISMATCH");
            System.out.println("mode1 " + hot.applyAsInt(5, 1));
            System.out.println("mode1 " + hot.applyAsInt(7, 1));
            try {
                System.out.println("mode2 returned " + hot.applyAsInt(0, 2));
            } catch (Throwable t) {
                System.out.println("mode2 " + t.getClass().getName());
            }
            System.out.println("mode3 " + hot.applyAsInt(42, 3));
            System.out.println(warm(hot, 8) ? "rewarm ok" : "rewarm MISMATCH");
            System.out.println("mode1 " + hot.applyAsInt(9, 1));
            System.out.println("mode3 " + hot.applyAsInt(43, 3));
        } finally {
            for (Path p : written) {
                Files.deleteIfExists(p);
            }
            Files.deleteIfExists(pdir);
        }
    }
}
