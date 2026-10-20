// Interpreter round i2 wave 60, lane L1: the warning HotSpot prints on stderr
// when a Java agent is loaded dynamically, against HotSpot
// (docs/internal/fixed-bugs/interpreter-i59-L1-no-dynamic-agent-warning-on-stderr-FIXED-20261115.md).
//
// The probe runs a child of the same VM (the current process's executable,
// `ProcessHandle.current().info().command()`, else `<java.home>/bin/java`)
// with `-Djdk.attach.allowAttachSelf=true`, in a scratch directory holding
// three manifest-only agent JARs whose `Agent-Class` is this file's nested
// class (on the child's class path): the child self-attaches and loads the
// good JAR by its absolute path, then by its relative path, a JAR without
// `Agent-Class`, and a JAR whose `agentmain` throws, printing what each load
// answered. The parent prints the child's stdout, then the child's stderr
// lines that start with `WARNING:` or name `Agent-Class` (the throwing
// agent's stack trace is left out), with the scratch directory's path
// spelled `<dir>`; then the same with `-XX:+EnableDynamicAgentLoading`.
//
// Expected stdout (HotSpot 25.0.3, default flags): MEASURED on the Windows
// box, three runs, the same each time:
//
//   launcher from this process: true
//   child (default) exit=0
//     out: same vm=true
//     out: agentmain ran
//     out: good absolute: loaded
//     out: agentmain ran
//     out: good relative: loaded
//     out: no Agent-Class: com.sun.tools.attach.AgentLoadException
//     out: throws: com.sun.tools.attach.AgentInitializationException
//     err: WARNING: A Java agent has been loaded dynamically (<dir>\good.jar)
//     err: WARNING: If a serviceability tool is in use, please run with -XX:+EnableDynamicAgentLoading to hide this warning
//     err: WARNING: If a serviceability tool is not in use, please run with -Djdk.instrument.traceUsage for more information
//     err: WARNING: Dynamic loading of agents will be disallowed by default in a future release
//     err: WARNING: A Java agent has been loaded dynamically (<dir>\good.jar)
//     err: WARNING: If a serviceability tool is in use, please run with -XX:+EnableDynamicAgentLoading to hide this warning
//     err: WARNING: If a serviceability tool is not in use, please run with -Djdk.instrument.traceUsage for more information
//     err: WARNING: Dynamic loading of agents will be disallowed by default in a future release
//     err: Failed to find Agent-Class manifest attribute from <dir>\none.jar
//     err: WARNING: A Java agent has been loaded dynamically (<dir>\throws.jar)
//     err: WARNING: If a serviceability tool is in use, please run with -XX:+EnableDynamicAgentLoading to hide this warning
//     err: WARNING: If a serviceability tool is not in use, please run with -Djdk.instrument.traceUsage for more information
//     err: WARNING: Dynamic loading of agents will be disallowed by default in a future release
//   child (-XX:+EnableDynamicAgentLoading) exit=0
//     out: same vm=true
//     out: agentmain ran
//     out: good absolute: loaded
//     out: agentmain ran
//     out: good relative: loaded
//     out: no Agent-Class: com.sun.tools.attach.AgentLoadException
//     out: throws: com.sun.tools.attach.AgentInitializationException
//     err: Failed to find Agent-Class manifest attribute from <dir>\none.jar
//
// CratonVM before wave 60 (code read): no `WARNING:` line and no
// `Failed to find Agent-Class` line.
//
//   javac -d /tmp/l1w60warn tools/probes/interp/L1/L1W60DynamicAgentWarning.java
//   java     -cp /tmp/l1w60warn L1W60DynamicAgentWarning
//   cratonvm -cp /tmp/l1w60warn L1W60DynamicAgentWarning
import com.sun.tools.attach.VirtualMachine;
import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.FileOutputStream;
import java.io.InputStream;
import java.lang.instrument.Instrumentation;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.TimeUnit;
import java.util.jar.Attributes;
import java.util.jar.JarOutputStream;
import java.util.jar.Manifest;

public class L1W60DynamicAgentWarning {
    public static class Agent {
        public static void agentmain(String args, Instrumentation inst) {
            System.out.println("agentmain ran");
        }
    }

    public static class Throwing {
        public static void agentmain(String args, Instrumentation inst) {
            throw new IllegalStateException("agent refuses");
        }
    }

    static void jar(File file, String agentClass) throws Exception {
        Manifest m = new Manifest();
        m.getMainAttributes().put(Attributes.Name.MANIFEST_VERSION, "1.0");
        if (agentClass != null) {
            m.getMainAttributes().put(new Attributes.Name("Agent-Class"), agentClass);
        }
        try (JarOutputStream out = new JarOutputStream(new FileOutputStream(file), m)) {
            // manifest only
        }
    }

    static void load(VirtualMachine vm, String label, String path) {
        try {
            vm.loadAgent(path);
            System.out.println(label + ": loaded");
        } catch (Exception e) {
            System.out.println(label + ": " + e.getClass().getName());
        }
    }

    static void child() throws Exception {
        System.out.println("vm=" + System.getProperty("java.vm.name") + "/" + System.getProperty("java.vm.version"));
        VirtualMachine vm = VirtualMachine.attach(String.valueOf(ProcessHandle.current().pid()));
        String dir = new File(".").getCanonicalPath();
        load(vm, "good absolute", dir + File.separator + "good.jar");
        load(vm, "good relative", "good.jar");
        load(vm, "no Agent-Class", dir + File.separator + "none.jar");
        load(vm, "throws", dir + File.separator + "throws.jar");
        vm.detach();
    }

    static String read(InputStream in) throws Exception {
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        in.transferTo(out);
        return out.toString(StandardCharsets.UTF_8);
    }

    static void run(File dir, String launcher, String extra) throws Exception {
        List<String> cmd = new ArrayList<>();
        cmd.add(launcher);
        cmd.add("-Djdk.attach.allowAttachSelf=true");
        if (extra != null) {
            cmd.add(extra);
        }
        cmd.add("-cp");
        cmd.add(System.getProperty("java.class.path"));
        cmd.add("L1W60DynamicAgentWarning");
        cmd.add("child");
        Process p = new ProcessBuilder(cmd).directory(dir).start();
        p.getOutputStream().close();
        String out = read(p.getInputStream());
        String err = read(p.getErrorStream());
        boolean done = p.waitFor(120, TimeUnit.SECONDS);
        System.out.println("child (" + (extra == null ? "default" : extra) + ") exit="
                + (done ? String.valueOf(p.exitValue()) : "timeout"));
        String canonical = dir.getCanonicalPath();
        String self = "vm=" + System.getProperty("java.vm.name") + "/" + System.getProperty("java.vm.version");
        for (String line : out.split("\\R")) {
            if (line.startsWith("vm=")) {
                // The child must be this VM, not a JDK the launcher fell back to.
                System.out.println("  out: same vm=" + line.equals(self));
            } else if (!line.isEmpty()) {
                System.out.println("  out: " + line);
            }
        }
        for (String line : err.split("\\R")) {
            if (line.startsWith("WARNING:") || line.contains("Agent-Class")) {
                System.out.println("  err: " + line.replace(canonical, "<dir>"));
            }
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 1 && args[0].equals("child")) {
            child();
            return;
        }
        File dir = new File(System.getProperty("java.io.tmpdir"), "l1w60warn-" + ProcessHandle.current().pid());
        dir.mkdirs();
        jar(new File(dir, "good.jar"), "L1W60DynamicAgentWarning$Agent");
        jar(new File(dir, "none.jar"), null);
        jar(new File(dir, "throws.jar"), "L1W60DynamicAgentWarning$Throwing");
        String home = System.getProperty("java.home");
        String fallback = home + File.separator + "bin" + File.separator + "java";
        java.util.Optional<String> command = ProcessHandle.current().info().command();
        // false: the child would be the fallback JDK, not this VM.
        System.out.println("launcher from this process: " + command.isPresent());
        String launcher = command.orElse(fallback);
        run(dir, launcher, null);
        run(dir, launcher, "-XX:+EnableDynamicAgentLoading");
        for (File f : dir.listFiles()) {
            f.delete();
        }
        dir.delete();
    }
}
