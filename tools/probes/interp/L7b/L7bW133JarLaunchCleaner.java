// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 133, lane L7b -- the gate the switch triage set for
// `CRATONVM_LAUNCHER_HELPER_JAR_NO_PRELOAD`
// (`i58-L5-proposal-the-launcher-runs-launcherhelper`, "Wave 133"): with the
// VM's pre-load of the manifest's class skipped, the JDK's `Common-Cleaner`
// thread must still exist before any agent or program thread, as on HotSpot
// (its `LauncherHelper` opens the jar -- `ZipFile`'s `CleanableResource`
// registers with `CleanerFactory.cleaner()` -- before it starts the
// `Launcher-Agent-Class` and loads the main class). The thread-dump probe
// `L6W81ThreadDumpShape` checks the same order, but only for a `-cp` launch.
//
// The probe writes two jars at run time from its own nested classes and
// launches the SAME launcher on each as a child (`-jar`), printing the child's
// stdout and exit status:
//   agent-jar: `Launcher-Agent-Class` = Agent. agentmain reports whether the
//              cleaner thread exists, starts and joins one thread of its own,
//              and adds a transformer that reports the main class's definition.
//              main reports the cleaner, its shape, and whether its threadId
//              is below the agent's thread and below a thread main starts.
//   plain-jar: no agent; main reports the same.
//
// Run:
//   javac -d out L7bW133JarLaunchCleaner.java
//   java -cp out L7bW133JarLaunchCleaner                                    (HotSpot)
//   for MODE in --jdk-only --compatible; do for SW in CRATONVM_NONE=1 CRATONVM_LAUNCHER_HELPER_JAR_NO_PRELOAD=1; do
//     env $SW cratonvm $MODE -cp out L7bW133JarLaunchCleaner $MODE
//   done; done
//   CRATONVM_COMPAT_ALIGN=-jdk-jar-file-bodies CRATONVM_LAUNCHER_HELPER_JAR_NO_PRELOAD=1 \
//     cratonvm --compatible -cp out L7bW133JarLaunchCleaner --compatible
//   (the arguments after the class name are passed to the child before `-jar`;
//   the environment, and so the switch, is inherited)
//
// Expected on CratonVM: the default arm prints no `saw` in the agent-jar line
// (the pre-load defined the class first: `L5W121LauncherAgentSeesMain`'s DIFF);
// every other token equals HotSpot's in every arm.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint)
// agent-jar: exit=0 out=[agentmain cleaner=true | saw L7bW133JarLaunchCleaner$App | main cleaner=true daemon=true prio=8 | cleaner-before-agent-thread=true | cleaner-before-main-thread=true]
// plain-jar: exit=0 out=[main cleaner=true daemon=true prio=8 | cleaner-before-agent-thread=n/a | cleaner-before-main-thread=true]
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

public class L7bW133JarLaunchCleaner {
    static final String AGENT_THREAD_ID = "l7bw133.agentThreadId";

    static Thread cleaner() {
        for (Thread t : Thread.getAllStackTraces().keySet()) {
            if ("Common-Cleaner".equals(t.getName())) {
                return t;
            }
        }
        return null;
    }

    // Must not name App: the agent runs before the main class is loaded.
    public static class Agent {
        public static void agentmain(String args, Instrumentation inst) throws Exception {
            System.out.println("agentmain cleaner=" + (cleaner() != null));
            Thread t = new Thread(() -> { }, "l7bw133-agent");
            t.start();
            t.join();
            System.setProperty(AGENT_THREAD_ID, Long.toString(t.threadId()));
            inst.addTransformer(new ClassFileTransformer() {
                @Override
                public byte[] transform(ClassLoader loader, String name, Class<?> redefined,
                        ProtectionDomain pd, byte[] bytes) {
                    if ("L7bW133JarLaunchCleaner$App".equals(name)) {
                        System.out.println("saw " + name);
                    }
                    return null;
                }
            });
        }
    }

    public static class App {
        public static void main(String[] args) throws Exception {
            Thread c = cleaner();
            if (c == null) {
                System.out.println("main cleaner=false");
            } else {
                System.out.println("main cleaner=true daemon=" + c.isDaemon() + " prio=" + c.getPriority());
            }
            String agentId = System.getProperty(AGENT_THREAD_ID);
            System.out.println("cleaner-before-agent-thread="
                    + (agentId == null ? "n/a" : String.valueOf(c != null && c.threadId() < Long.parseLong(agentId))));
            Thread t = new Thread(() -> { }, "l7bw133-main");
            t.start();
            t.join();
            System.out.println("cleaner-before-main-thread=" + (c != null && c.threadId() < t.threadId()));
        }
    }

    static byte[] own(String name) throws Exception {
        try (InputStream in = L7bW133JarLaunchCleaner.class.getResourceAsStream(name + ".class")) {
            return in.readAllBytes();
        }
    }

    static String lines(byte[] bytes) {
        return new String(bytes, StandardCharsets.UTF_8).lines()
                .filter(l -> !l.isBlank())
                .collect(Collectors.joining(" | "));
    }

    static Path jar(Path dir, String file, boolean agent) throws Exception {
        Path jar = dir.resolve(file);
        Manifest mf = new Manifest();
        Attributes a = mf.getMainAttributes();
        a.put(Attributes.Name.MANIFEST_VERSION, "1.0");
        a.put(Attributes.Name.MAIN_CLASS, "L7bW133JarLaunchCleaner$App");
        if (agent) {
            a.put(new Attributes.Name("Launcher-Agent-Class"), "L7bW133JarLaunchCleaner$Agent");
        }
        List<String> names = new ArrayList<>(List.of("L7bW133JarLaunchCleaner", "L7bW133JarLaunchCleaner$App"));
        if (agent) {
            names.add("L7bW133JarLaunchCleaner$Agent");
            names.add("L7bW133JarLaunchCleaner$Agent$1");
        }
        try (OutputStream os = Files.newOutputStream(jar); JarOutputStream out = new JarOutputStream(os, mf)) {
            for (String n : names) {
                out.putNextEntry(new JarEntry(n + ".class"));
                out.write(own(n));
                out.closeEntry();
            }
        }
        return jar;
    }

    static void launch(String label, Path jar, String[] args) throws Exception {
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
        System.out.println(label + ": exit=" + exit + " out=[" + lines(out) + "]");
        Files.deleteIfExists(jar);
    }

    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("l7bw133");
        launch("agent-jar", jar(dir, "agent.jar", true), args);
        launch("plain-jar", jar(dir, "plain.jar", false), args);
        Files.deleteIfExists(dir);
    }
}
