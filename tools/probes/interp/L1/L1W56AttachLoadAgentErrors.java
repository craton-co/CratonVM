// Interpreter round i2 wave 56, lane L1: what `VirtualMachine.loadAgent`
// throws, and what the agent's `agentmain` receives, when a self-attach loads
// a Java agent, against HotSpot.
//
// The probe writes its own agent JARs (a manifest only; the agent classes are
// this file's nested classes, already on the class path) and loads each into
// its own VM: a null path, a path naming no file, a JAR without
// `Agent-Class`, an `Agent-Class` naming no class, one without `agentmain`,
// one whose `agentmain` throws, then a working agent with no options and with
// options. Each row prints the exception's class and message, or what the
// agent received. The whole list twice (the first-call row again).
//
// A self-attach needs `-Djdk.attach.allowAttachSelf=true` on both VMs.
// The forced CratonVM default has been removed; macOS transport acceptance
// remains tracked in:
// docs/internal/fixed-bugs/i56-L1-the-vm-sets-jdk-attach-allowattachself-by-default-20261020.md.
// Run this in-process error probe with that explicit flag on both:
//
//   javac -d /tmp/l1w56load tools/probes/interp/L1/L1W56AttachLoadAgentErrors.java
//   java     -Djdk.attach.allowAttachSelf=true -cp /tmp/l1w56load L1W56AttachLoadAgentErrors
//   cratonvm -Djdk.attach.allowAttachSelf=true -cp /tmp/l1w56load L1W56AttachLoadAgentErrors
//
// (HotSpot also prints a dynamic-agent warning and the failing agents' stack
// traces to stderr; only stdout is compared.)
//
// Expected stdout (HotSpot 25.0.3, -Djdk.attach.allowAttachSelf=true):
// MEASURED on the Windows box, three runs, the same each time:
//
//   round 1
//   null path: java.lang.NullPointerException [agent cannot be null]
//   no file: com.sun.tools.attach.AgentLoadException [Agent JAR not found or no Agent-Class attribute]
//   no Agent-Class: com.sun.tools.attach.AgentLoadException [Agent JAR not found or no Agent-Class attribute]
//   no such class: com.sun.tools.attach.AgentInitializationException [Agent JAR loaded but agent failed to initialize]
//   no agentmain: com.sun.tools.attach.AgentInitializationException [Agent JAR loaded but agent failed to initialize]
//   agentmain throws: com.sun.tools.attach.AgentInitializationException [Agent JAR loaded but agent failed to initialize]
//   no options: loaded, agent saw args=null inst=true
//   options: loaded, agent saw args='k=v,x' inst=true
//   empty options: loaded, agent saw args='' inst=true
//   round 2
//   (the nine rows again, the same)
//
// (On Windows, `--compatible` CratonVM writes the JARs under a path whose
// `getAbsolutePath()` lost its drive letter, so every row there reads
// `AgentLoadException`: a file-system gap outside this probe's subject;
// not measured on Linux.)
//
// CratonVM before wave 56 (MEASURED, debug build, `--jdk-only`): the first five rows `loaded, agent
// saw none` (a failed load returned quietly), `agentmain throws:
// java.lang.IllegalStateException [agent fails]`, and `no options: ...
// args=''`. Since wave 56 (`CRATONVM_ATTACH_CHECKS` on, the default from the
// lane's last commit): HotSpot's lines (`runtime::instrument::native_vm_load_agent`).
import com.sun.tools.attach.VirtualMachine;
import java.io.File;
import java.io.FileOutputStream;
import java.lang.instrument.Instrumentation;
import java.util.jar.Attributes;
import java.util.jar.JarOutputStream;
import java.util.jar.Manifest;

public class L1W56AttachLoadAgentErrors {
    static volatile String received = "none";

    public static class Agent {
        public static void agentmain(String args, Instrumentation inst) {
            received = "args=" + (args == null ? "null" : "'" + args + "'") + " inst=" + (inst != null);
        }
    }

    public static class NoMain {
    }

    public static class Thrower {
        public static void agentmain(String args, Instrumentation inst) {
            throw new IllegalStateException("agent fails");
        }
    }

    static String jar(File dir, String name, String agentClass) throws Exception {
        Manifest manifest = new Manifest();
        manifest.getMainAttributes().put(Attributes.Name.MANIFEST_VERSION, "1.0");
        if (agentClass != null) {
            manifest.getMainAttributes().put(new Attributes.Name("Agent-Class"), agentClass);
        }
        File file = new File(dir, name);
        try (JarOutputStream out = new JarOutputStream(new FileOutputStream(file), manifest)) {
            out.flush();
        }
        return file.getAbsolutePath();
    }

    static String load(VirtualMachine vm, String path, String options) {
        received = "none";
        try {
            if (options == null) {
                vm.loadAgent(path);
            } else {
                vm.loadAgent(path, options);
            }
            return "loaded, agent saw " + received;
        } catch (Throwable t) {
            return t.getClass().getName() + " [" + t.getMessage() + "]";
        }
    }

    public static void main(String[] args) throws Exception {
        File dir = java.nio.file.Files.createTempDirectory("l1w56load").toFile();
        String nested = L1W56AttachLoadAgentErrors.class.getName();
        String missingFile = new File(dir, "absent.jar").getAbsolutePath();
        String noAgentClass = jar(dir, "plain.jar", null);
        String noClass = jar(dir, "noclass.jar", "no.such.AgentClass");
        String noMain = jar(dir, "nomain.jar", nested + "$NoMain");
        String thrower = jar(dir, "thrower.jar", nested + "$Thrower");
        String good = jar(dir, "good.jar", nested + "$Agent");
        VirtualMachine vm;
        try {
            vm = VirtualMachine.attach(Long.toString(ProcessHandle.current().pid()));
        } catch (Exception e) {
            System.out.println("attach: " + e.getClass().getName() + " [" + e.getMessage() + "]");
            return;
        }
        for (int round = 1; round <= 2; round++) {
            System.out.println("round " + round);
            System.out.println("null path: " + load(vm, null, null));
            System.out.println("no file: " + load(vm, missingFile, null));
            System.out.println("no Agent-Class: " + load(vm, noAgentClass, null));
            System.out.println("no such class: " + load(vm, noClass, null));
            System.out.println("no agentmain: " + load(vm, noMain, null));
            System.out.println("agentmain throws: " + load(vm, thrower, null));
            System.out.println("no options: " + load(vm, good, null));
            System.out.println("options: " + load(vm, good, "k=v,x"));
            System.out.println("empty options: " + load(vm, good, ""));
        }
        vm.detach();
    }
}
