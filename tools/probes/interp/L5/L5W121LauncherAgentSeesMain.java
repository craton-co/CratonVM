// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 121, lane L5 -- `java -jar` with a
// `Launcher-Agent-Class` (`i58-L5-proposal-the-launcher-runs-launcherhelper`,
// "Wave 121"; switch `CRATONVM_LAUNCHER_HELPER_JAR_NO_PRELOAD`, default off).
// HotSpot's `LauncherHelper` starts the jar's agent BEFORE it loads the main
// class, so the agent's transformer is offered the main class's bytes. The
// probe writes a jar (its own nested `App` and `Agent` classes) at run time
// and launches the SAME launcher on it as a child, printing the child's
// stdout and exit status.
//
// Run:
//   javac -d out L5W121LauncherAgentSeesMain.java
//   java -cp out L5W121LauncherAgentSeesMain                                   (HotSpot)
//   cratonvm --jdk-only -cp out L5W121LauncherAgentSeesMain --jdk-only
//   CRATONVM_LAUNCHER_HELPER_JAR_NO_PRELOAD=1 cratonvm --jdk-only -cp out L5W121LauncherAgentSeesMain --jdk-only
//   (and both with --compatible; CratonVM needs its mode flag passed on to the child)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// child: exit=0 out=[agentmain | saw L5W121LauncherAgentSeesMain$App | main ran]
// HOTSPOT_EXPECTED_END

import java.io.InputStream;
import java.io.OutputStream;
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.ProtectionDomain;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.jar.Attributes;
import java.util.jar.JarEntry;
import java.util.jar.JarOutputStream;
import java.util.jar.Manifest;
import java.util.stream.Collectors;

public class L5W121LauncherAgentSeesMain {
    public static class App {
        public static void main(String[] args) {
            System.out.println("main ran");
        }
    }

    public static class Agent {
        public static void agentmain(String args, Instrumentation inst) {
            System.out.println("agentmain");
            inst.addTransformer(new ClassFileTransformer() {
                @Override
                public byte[] transform(ClassLoader loader, String name, Class<?> redefined,
                        ProtectionDomain pd, byte[] bytes) {
                    if ("L5W121LauncherAgentSeesMain$App".equals(name)) {
                        System.out.println("saw " + name);
                    }
                    return null;
                }
            });
        }
    }

    static byte[] own(String name) throws Exception {
        try (InputStream in = L5W121LauncherAgentSeesMain.class.getResourceAsStream(name + ".class")) {
            return in.readAllBytes();
        }
    }

    static String lines(byte[] bytes) {
        return new String(bytes, StandardCharsets.UTF_8).lines()
                .filter(l -> !l.isBlank())
                .collect(Collectors.joining(" | "));
    }

    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("l5w121");
        Path jar = dir.resolve("app.jar");
        Manifest mf = new Manifest();
        Attributes a = mf.getMainAttributes();
        a.put(Attributes.Name.MANIFEST_VERSION, "1.0");
        a.put(Attributes.Name.MAIN_CLASS, "L5W121LauncherAgentSeesMain$App");
        a.put(new Attributes.Name("Launcher-Agent-Class"), "L5W121LauncherAgentSeesMain$Agent");
        try (OutputStream os = Files.newOutputStream(jar); JarOutputStream out = new JarOutputStream(os, mf)) {
            for (String n : new String[] {"L5W121LauncherAgentSeesMain$App", "L5W121LauncherAgentSeesMain$Agent",
                    "L5W121LauncherAgentSeesMain$Agent$1"}) {
                out.putNextEntry(new JarEntry(n + ".class"));
                out.write(own(n));
                out.closeEntry();
            }
        }
        List<String> cmd = new ArrayList<>();
        cmd.add(ProcessHandle.current().info().command().orElseThrow());
        cmd.addAll(Arrays.asList(args));
        cmd.add("-jar");
        cmd.add(jar.toString());
        Process p = new ProcessBuilder(cmd).start();
        p.getOutputStream().close();
        byte[] out = p.getInputStream().readAllBytes();
        p.getErrorStream().readAllBytes();
        int exit = p.waitFor();
        System.out.println("child: exit=" + exit + " out=[" + lines(out) + "]");
        Files.deleteIfExists(jar);
        Files.deleteIfExists(dir);
    }
}
