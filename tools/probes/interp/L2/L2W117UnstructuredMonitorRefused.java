// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 117, lane L2: `CRATONVM_JIT_UNSTRUCTURED_MONITOR_REFUSE`
// (HotSpot parity, default ON, `=0` kill switch; page
// docs/internal/fixed-bugs/interpreter-i116-L6-a-compiled-unpaired-monitorexit-releases-its-callers-lock-FIXED-20261224.md,
// "What is still open").
//
// Hand-written bodies whose monitor bytecode is paired by COUNT but not
// structured, each called hot (30 000 times) so a JIT would compile it. HotSpot
// never compiles them (`has_balanced_monitors`) and its interpreter enforces
// structured locking; with the refusal, CratonVM runs them interpreted too.
//   exit-other     aload_0; monitorenter; aload_1; monitorexit; return
//   held-at-return aload_0; monitorenter; aload_0; monitorexit; aload_0; monitorenter; return
//   javac-control  synchronized (o) { n++ } as javac emits it (must still compile and work)
// Each row prints the exception (class and whether the message is null), and
// whether the caller still holds / can take the locks afterwards. Every line
// must be the same in both modes, JIT and --nojit; with
// CRATONVM_JIT_UNSTRUCTURED_MONITOR_REFUSE=0 the JIT arm may differ (that is
// the defect the switch closes).
//
// Run:
//   javac -d out tools/probes/interp/L2/L2W117UnstructuredMonitorRefused.java
//   java -cp out L2W117UnstructuredMonitorRefused                          (HotSpot)
//   cratonvm --jdk-only -cp out L2W117UnstructuredMonitorRefused           (and --compatible, --nojit)
//   CRATONVM_DBG_JITC=1 cratonvm --jdk-only -cp out L2W117UnstructuredMonitorRefused 2>&1 | grep unstructured-monitor
//     (the bail site names exitOther and heldAtReturn; never javacControl)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags; -Xint gives the same)
// exit-other thrown=30000 last=java.lang.IllegalMonitorStateException nullMessage=true
// exit-other heldA=false heldB=false
// held-at-return thrown=30000 last=java.lang.IllegalMonitorStateException nullMessage=true
// held-at-return held=false
// javac-control thrown=0 last=none
// javac-control n=30000 held=false
// other-thread locks ok
// HOTSPOT_EXPECTED_END
import java.io.File;
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.nio.file.Files;
import java.nio.file.Path;

public class L2W117UnstructuredMonitorRefused {
    static final int CALLS = 30_000;
    static final ClassDesc OBJ = ConstantDescs.CD_Object;

    static byte[] bodies(ClassDesc self) {
        int ps = ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC;
        return ClassFile.of().build(self, clb -> {
            clb.withFlags(ClassFile.ACC_PUBLIC);
            clb.withMethodBody("exitOther", MethodTypeDesc.of(ConstantDescs.CD_void, OBJ, OBJ), ps,
                    cb -> cb.aload(0).monitorenter().aload(1).monitorexit().return_());
            clb.withMethodBody("heldAtReturn", MethodTypeDesc.of(ConstantDescs.CD_void, OBJ), ps,
                    cb -> cb.aload(0).monitorenter().aload(0).monitorexit()
                            .aload(0).monitorenter().return_());
        });
    }

    static int n;

    static void javacControl(Object o) {
        synchronized (o) {
            n++;
        }
    }

    static String run(String row, Runnable r) {
        int thrown = 0;
        String last = "none";
        for (int i = 0; i < CALLS; i++) {
            try {
                r.run();
            } catch (Throwable e) {
                thrown++;
                last = e.getClass().getName() + " nullMessage=" + (e.getMessage() == null);
            }
        }
        return row + " thrown=" + thrown + " last=" + last;
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

    public static void main(String[] args) throws Throwable {
        Path dir = classPathDir();
        if (dir == null) {
            System.out.println("no class-path directory");
            return;
        }
        String name = "L2W117Unstructured" + ProcessHandle.current().pid();
        Path file = dir.resolve(name + ".class");
        Files.write(file, bodies(ClassDesc.of(name)));
        try {
            Class<?> c = Class.forName(name);
            MethodHandles.Lookup l = MethodHandles.publicLookup();
            MethodHandle exitOther = l.findStatic(c, "exitOther",
                    MethodType.methodType(void.class, Object.class, Object.class));
            MethodHandle heldAtReturn = l.findStatic(c, "heldAtReturn",
                    MethodType.methodType(void.class, Object.class));
            Object a = new Object();
            Object b = new Object();
            System.out.println(run("exit-other", () -> {
                try {
                    exitOther.invokeExact(a, b);
                } catch (RuntimeException | Error e) {
                    throw e;
                } catch (Throwable t) {
                    throw new RuntimeException(t);
                }
            }));
            System.out.println("exit-other heldA=" + Thread.holdsLock(a) + " heldB=" + Thread.holdsLock(b));
            Object h = new Object();
            System.out.println(run("held-at-return", () -> {
                try {
                    heldAtReturn.invokeExact(h);
                } catch (RuntimeException | Error e) {
                    throw e;
                } catch (Throwable t) {
                    throw new RuntimeException(t);
                }
            }));
            System.out.println("held-at-return held=" + Thread.holdsLock(h));
            Object o = new Object();
            System.out.println(run("javac-control", () -> javacControl(o)));
            System.out.println("javac-control n=" + n + " held=" + Thread.holdsLock(o));
            // Another thread can take every lock the rows touched.
            Thread t = new Thread(() -> {
                synchronized (a) { synchronized (b) { synchronized (h) { synchronized (o) {
                    System.out.println("other-thread locks ok");
                } } } }
            });
            t.setDaemon(true);
            t.start();
            t.join(10_000);
            if (t.isAlive()) {
                System.out.println("other-thread timeout");
            }
        } finally {
            Files.deleteIfExists(file);
        }
    }
}
