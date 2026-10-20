// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4, wave 86, lane L5 -- the launcher's module launch:
// `-p <dir> -m <module>[/<class>]`. The probe writes exploded modules under a
// temp dir (module-info.class and classes with java.lang.classfile) and runs
// the SAME launcher as a child process for each case, printing the child's
// stdout, its stderr and its exit status:
//
// * m.app: ModuleMainClass app.Main; `-m m.app` and `-m m.app/app.Main`,
//   program arguments after the module operand;
// * m.nomain: no ModuleMainClass;
// * a module that is not observable; m.needs, which requires an absent module;
// * `-m m.app/app.Nope`, a class the module does not have.
//
// The child is `ProcessHandle.current().info().command()`; the probe's own
// program arguments are passed to it first (CratonVM needs its mode flag and
// --java-home there, HotSpot nothing).
//
// Run:
//   $J/javac -d out L5W86ModuleLaunch.java
//   $J/java -cp out L5W86ModuleLaunch                                          # HotSpot
//   $B --java-home $J/.. -cp out L5W86ModuleLaunch --java-home $J/..
//   $B --compatible --java-home $J/.. -cp out L5W86ModuleLaunch --compatible --java-home $J/..
//   and with no --java-home and no JAVA_HOME (the JDK found on PATH; page
//   i116-L5-module-launch-misses-absent-modules-without-java-home):
//   $B --jdk-only -cp out L5W86ModuleLaunch --jdk-only
// Expected block read from the JDK 25 launcher (LauncherHelper,
// launcher.properties) and the boot-layer abort; confirm it on the host first.
//
// HOTSPOT_EXPECTED_BEGIN
// descriptor-main: exit=0 out=[module=m.app args=[x, y]] err=[]
// explicit-main: exit=0 out=[module=m.app args=[z]] err=[]
// no-main-class: exit=1 out=[] err=[module m.nomain does not have a ModuleMainClass attribute, use -m <module>/<main-class>]
// missing-module: exit=1 out=[Error occurred during initialization of boot layer | java.lang.module.FindException: Module m.absent not found] err=[]
// missing-requires: exit=1 out=[Error occurred during initialization of boot layer | java.lang.module.FindException: Module m.absent not found, required by m.needs] err=[]
// missing-class: exit=1 out=[] err=[Error: Could not find or load main class app.Nope in module m.app]
// HOTSPOT_EXPECTED_END

import java.io.IOException;
import java.lang.classfile.ClassFile;
import java.lang.classfile.attribute.ModuleAttribute;
import java.lang.classfile.attribute.ModuleMainClassAttribute;
import java.lang.classfile.attribute.ModulePackagesAttribute;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.constant.ModuleDesc;
import java.lang.constant.PackageDesc;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.stream.Collectors;

public class L5W86ModuleLaunch {
    static final ClassDesc PRINT_STREAM = ClassDesc.of("java.io.PrintStream");
    static final ClassDesc SYSTEM = ClassDesc.of("java.lang.System");
    static final ClassDesc MODULE = ClassDesc.of("java.lang.Module");
    static final ClassDesc STRING_BUILDER = ClassDesc.of("java.lang.StringBuilder");
    static final MethodTypeDesc PRINTLN = MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_String);
    static final MethodTypeDesc APPEND = MethodTypeDesc.of(STRING_BUILDER, ConstantDescs.CD_String);

    static byte[] moduleInfo(String name, String requires, String mainClass, String pkg) {
        ModuleAttribute attr = ModuleAttribute.of(ModuleDesc.of(name), mb -> {
            mb.requires(ModuleDesc.of("java.base"), ClassFile.ACC_MANDATED, null);
            if (requires != null) {
                mb.requires(ModuleDesc.of(requires), 0, null);
            }
            mb.exports(PackageDesc.of(pkg), 0);
        });
        return ClassFile.of().buildModule(attr, cb -> {
            cb.with(ModulePackagesAttribute.ofNames(PackageDesc.of(pkg)));
            if (mainClass != null) {
                cb.with(ModuleMainClassAttribute.of(ClassDesc.of(mainClass)));
            }
        });
    }

    /** `pkg.Main.main` prints `module=<module name> args=<Arrays.toString(args)>`. */
    static byte[] mainClass(String pkg) {
        ClassDesc main = ClassDesc.of(pkg + ".Main");
        return ClassFile.of().build(main, cb -> cb
                .withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER)
                .withMethodBody("main", MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_String.arrayType()),
                        ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, c -> c
                                .getstatic(SYSTEM, "out", PRINT_STREAM)
                                .new_(STRING_BUILDER)
                                .dup()
                                .ldc("module=")
                                .invokespecial(STRING_BUILDER, "<init>",
                                        MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_String))
                                .ldc(main)
                                .invokevirtual(ConstantDescs.CD_Class, "getModule", MethodTypeDesc.of(MODULE))
                                .invokevirtual(MODULE, "getName", MethodTypeDesc.of(ConstantDescs.CD_String))
                                .invokevirtual(STRING_BUILDER, "append", APPEND)
                                .ldc(" args=")
                                .invokevirtual(STRING_BUILDER, "append", APPEND)
                                .aload(0)
                                .invokestatic(ClassDesc.of("java.util.Arrays"), "toString",
                                        MethodTypeDesc.of(ConstantDescs.CD_String, ConstantDescs.CD_Object.arrayType()))
                                .invokevirtual(STRING_BUILDER, "append", APPEND)
                                .invokevirtual(STRING_BUILDER, "toString", MethodTypeDesc.of(ConstantDescs.CD_String))
                                .invokevirtual(PRINT_STREAM, "println", PRINTLN)
                                .return_()));
    }

    static void module(Path dir, String name, String requires, String mainClass, String pkg) throws IOException {
        Path root = dir.resolve(name);
        Files.createDirectories(root.resolve(pkg));
        Files.write(root.resolve("module-info.class"), moduleInfo(name, requires, mainClass, pkg));
        Files.write(root.resolve(pkg).resolve("Main.class"), mainClass(pkg));
    }

    static String lines(byte[] bytes) {
        return new String(bytes, StandardCharsets.UTF_8).lines()
                .filter(l -> !l.isBlank())
                .collect(Collectors.joining(" | "));
    }

    static void run(String label, List<String> prefix, Path mods, String... tail) throws Exception {
        List<String> cmd = new ArrayList<>(prefix);
        cmd.add("-p");
        cmd.add(mods.toString());
        cmd.addAll(Arrays.asList(tail));
        Process p = new ProcessBuilder(cmd).start();
        p.getOutputStream().close();
        byte[] out = p.getInputStream().readAllBytes();
        byte[] err = p.getErrorStream().readAllBytes();
        int exit = p.waitFor();
        System.out.println(label + ": exit=" + exit + " out=[" + lines(out) + "] err=[" + lines(err) + "]");
    }

    public static void main(String[] args) throws Exception {
        Path mods = Files.createTempDirectory("l5w86-mods");
        module(mods, "m.app", null, "app.Main", "app");
        module(mods, "m.nomain", null, null, "nomain");
        module(mods, "m.needs", "m.absent", "needs.Main", "needs");

        List<String> prefix = new ArrayList<>();
        prefix.add(ProcessHandle.current().info().command().orElseThrow());
        prefix.addAll(Arrays.asList(args));

        run("descriptor-main", prefix, mods, "-m", "m.app", "x", "y");
        run("explicit-main", prefix, mods, "-m", "m.app/app.Main", "z");
        run("no-main-class", prefix, mods, "-m", "m.nomain");
        run("missing-module", prefix, mods, "-m", "m.absent");
        run("missing-requires", prefix, mods, "-m", "m.needs");
        run("missing-class", prefix, mods, "-m", "m.app/app.Nope");
    }
}
