// Interpreter round i3 wave 81, lane L1: what a dynamically loaded Java agent
// prints when its agentmain throws, and the load refused under
// -XX:-EnableDynamicAgentLoading, against HotSpot
// (docs/internal/fixed-bugs/interpreter-i59-L1-no-dynamic-agent-warning-on-stderr-FIXED-20261115.md).
//
// The probe runs a child of the same VM (the current process's executable,
// `ProcessHandle.current().info().command()`, else `<java.home>/bin/java`)
// with `-Djdk.attach.allowAttachSelf=true`, in a scratch directory holding
// two manifest-only agent JARs whose `Agent-Class` is this file's nested
// class (on the child's class path): the child self-attaches and loads the
// good JAR and the JAR whose `agentmain` throws, printing what each load
// answered (and, under the flag, what `loadAgentPath` of a library that does
// not exist answers). The parent prints the child's stdout, then the child's
// stderr lines that JPLIS and the launcher print for an agent (`WARNING:`,
// the uncaught-exception header and its trace, JPLIS's assertion lines,
// `Agent failed to start!`), with the scratch directory spelled `<dir>`;
// then the same with `-XX:-EnableDynamicAgentLoading`.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// launcher from this process: true
// child (default) exit=0
//   out: same vm=true
//   out: agentmain ran
//   out: good: loaded
//   out: throws: com.sun.tools.attach.AgentInitializationException: Agent JAR loaded but agent failed to initialize
//   err: WARNING: A Java agent has been loaded dynamically (<dir>/good.jar)
//   err: WARNING: If a serviceability tool is in use, please run with -XX:+EnableDynamicAgentLoading to hide this warning
//   err: WARNING: If a serviceability tool is not in use, please run with -Djdk.instrument.traceUsage for more information
//   err: WARNING: Dynamic loading of agents will be disallowed by default in a future release
//   err: WARNING: A Java agent has been loaded dynamically (<dir>/throws.jar)
//   err: WARNING: If a serviceability tool is in use, please run with -XX:+EnableDynamicAgentLoading to hide this warning
//   err: WARNING: If a serviceability tool is not in use, please run with -Djdk.instrument.traceUsage for more information
//   err: WARNING: Dynamic loading of agents will be disallowed by default in a future release
//   err: Exception in thread "Attach Listener" java.lang.reflect.InvocationTargetException
//   err: <TAB>at java.base/jdk.internal.reflect.DirectMethodHandleAccessor.invoke(DirectMethodHandleAccessor.java:119)
//   err: <TAB>at java.base/java.lang.reflect.Method.invoke(Method.java:565)
//   err: <TAB>at java.instrument/sun.instrument.InstrumentationImpl.loadClassAndStartAgent(InstrumentationImpl.java:546)
//   err: <TAB>at java.instrument/sun.instrument.InstrumentationImpl.loadClassAndCallAgentmain(InstrumentationImpl.java:566)
//   err: Caused by: java.lang.IllegalStateException: boom
//   err: <TAB>at L1W81DynamicAgentRefusal$Thrower.agentmain(L1W81DynamicAgentRefusal.java:87)
//   err: <TAB>at java.base/jdk.internal.reflect.DirectMethodHandleAccessor.invoke(DirectMethodHandleAccessor.java:104)
//   err: <TAB>... 3 more
//   err: *** java.lang.instrument ASSERTION FAILED ***: "!errorOutstanding" with message Outstanding error when calling method in invokeJavaAgentMainMethod at src/java.instrument/share/native/libinstrument/JPLISAgent.c line: 627
//   err: *** java.lang.instrument ASSERTION FAILED ***: "success" with message invokeJavaAgentMainMethod failed at src/java.instrument/share/native/libinstrument/JPLISAgent.c line: 466
//   err: *** java.lang.instrument ASSERTION FAILED ***: "success" with message startJavaAgent failed at src/java.instrument/share/native/libinstrument/InvocationAdapter.c line: 444
//   err: Agent failed to start!
// child (-XX:-EnableDynamicAgentLoading) exit=0
//   out: same vm=true
//   out: good: com.sun.tools.attach.AgentLoadException: Failed to load agent library: Dynamic agent loading is not enabled. Use -XX:+EnableDynamicAgentLoading to launch target VM.
//   out: throws: com.sun.tools.attach.AgentLoadException: Failed to load agent library: Dynamic agent loading is not enabled. Use -XX:+EnableDynamicAgentLoading to launch target VM.
//   out: native: com.sun.tools.attach.AgentLoadException: Failed to load agent library: Dynamic agent loading is not enabled. Use -XX:+EnableDynamicAgentLoading to launch target VM.
// HOTSPOT_EXPECTED_END
//
// The block is the host oracle of 2026-10-09 (HotSpot 25.0.4 on Linux,
// `m81` / `M81Agent.java`: the same two JARs, one-argument `agentmain`s)
// with this probe's class name and line; `<TAB>` is a tab character. The
// `native:` row is derived from the JDK 25 sources (the listener refuses
// the `load` operation whatever its library; `HotSpotVirtualMachine`
// prefixes "Failed to load agent library: "), not measured.
//
// CratonVM at w80b (MEASURED, same oracle): the WARNING lines and the
// client's exceptions in the default child, but none of the throwing
// agent's stderr; under the flag both JARs load and `agentmain` runs.
// Wave 81: the flag refuses every load (unconditional); the throwing
// agent's stderr needs CRATONVM_ATTACH_AGENTMAIN_JDK=1 (default off), whose
// trace's reflective frames are this VM's `Method.invoke`'s.
//
//   javac -d /tmp/l1w81agent tools/probes/interp/L1/L1W81DynamicAgentRefusal.java
//   java     -cp /tmp/l1w81agent L1W81DynamicAgentRefusal
//   cratonvm -cp /tmp/l1w81agent L1W81DynamicAgentRefusal
//   CRATONVM_ATTACH_AGENTMAIN_JDK=1 cratonvm -cp /tmp/l1w81agent L1W81DynamicAgentRefusal
import com.sun.tools.attach.VirtualMachine;
import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.FileOutputStream;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.TimeUnit;
import java.util.jar.Attributes;
import java.util.jar.JarOutputStream;
import java.util.jar.Manifest;

public class L1W81DynamicAgentRefusal {
    public static class Thrower {
        public static void agentmain(String a) { throw new IllegalStateException("boom"); }
    }

    public static class Good {
        public static void agentmain(String a) {
            System.out.println("agentmain ran");
        }
    }

    static void jar(File file, String agentClass) throws Exception {
        Manifest m = new Manifest();
        m.getMainAttributes().put(Attributes.Name.MANIFEST_VERSION, "1.0");
        m.getMainAttributes().put(new Attributes.Name("Agent-Class"), agentClass);
        try (JarOutputStream out = new JarOutputStream(new FileOutputStream(file), m)) {
            // manifest only
        }
    }

    static void load(VirtualMachine vm, String label, String path, boolean nativeAgent) {
        try {
            if (nativeAgent) {
                vm.loadAgentPath(path);
            } else {
                vm.loadAgent(path);
            }
            System.out.println(label + ": loaded");
        } catch (Exception e) {
            System.out.println(label + ": " + e);
        }
    }

    static void child(boolean refused) throws Exception {
        System.out.println("vm=" + System.getProperty("java.vm.name") + "/" + System.getProperty("java.vm.version"));
        VirtualMachine vm = VirtualMachine.attach(String.valueOf(ProcessHandle.current().pid()));
        String dir = new File(".").getCanonicalPath();
        load(vm, "good", dir + File.separator + "good.jar", false);
        load(vm, "throws", dir + File.separator + "throws.jar", false);
        if (refused) {
            load(vm, "native", dir + File.separator + "libnosuchagent.so", true);
        }
        vm.detach();
    }

    static String read(InputStream in) throws Exception {
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        in.transferTo(out);
        return out.toString(StandardCharsets.UTF_8);
    }

    static boolean jplisLine(String line) {
        return line.startsWith("WARNING:")
                || line.startsWith("Exception in thread ")
                || line.startsWith("\tat ")
                || line.startsWith("\t... ")
                || line.startsWith("Caused by: ")
                || line.startsWith("*** java.lang.instrument")
                || line.equals("Agent failed to start!");
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
        cmd.add("L1W81DynamicAgentRefusal");
        cmd.add(extra == null ? "child" : "child-refused");
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
                System.out.println("  out: " + line.replace(canonical, "<dir>"));
            }
        }
        for (String line : err.split("\\R")) {
            if (jplisLine(line)) {
                System.out.println("  err: " + line.replace(canonical, "<dir>").replace("\t", "<TAB>"));
            }
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 1 && args[0].startsWith("child")) {
            child(args[0].equals("child-refused"));
            return;
        }
        File dir = new File(System.getProperty("java.io.tmpdir"), "l1w81agent-" + ProcessHandle.current().pid());
        dir.mkdirs();
        jar(new File(dir, "good.jar"), "L1W81DynamicAgentRefusal$Good");
        jar(new File(dir, "throws.jar"), "L1W81DynamicAgentRefusal$Thrower");
        String home = System.getProperty("java.home");
        String fallback = home + File.separator + "bin" + File.separator + "java";
        java.util.Optional<String> command = ProcessHandle.current().info().command();
        // false: the child would be the fallback JDK, not this VM.
        System.out.println("launcher from this process: " + command.isPresent());
        String launcher = command.orElse(fallback);
        run(dir, launcher, null);
        run(dir, launcher, "-XX:-EnableDynamicAgentLoading");
        for (File f : dir.listFiles()) {
            f.delete();
        }
        dir.delete();
    }
}
