// Interpreter round i2 wave 59, lane L1: which `agentmain` shapes a dynamic
// load accepts, against HotSpot. JPLIS's `InstrumentationImpl.loadClassAndStartAgent`
// looks up `agentmain(String, Instrumentation)` then `agentmain(String)` --
// declared first, then public inherited -- refuses a non-public one with an
// `IllegalAccessException`, and invokes it with a null receiver (so an
// instance method fails); any failure is the client's
// `AgentInitializationException`.
//
// The probe self-attaches, so on HotSpot it needs
// `-Djdk.attach.allowAttachSelf=true` (CratonVM defaults it on except on
// Linux with the wave-59 switches; the probe prints the self-attach answer
// and stops when it is refused, so a default-flags run on both prints that
// one row):
//
//   javac -d /tmp/l1w59shape tools/probes/interp/L1/L1W59AgentmainShapes.java
//   java     -Djdk.attach.allowAttachSelf=true -cp /tmp/l1w59shape L1W59AgentmainShapes
//   cratonvm -Djdk.attach.allowAttachSelf=true -cp /tmp/l1w59shape L1W59AgentmainShapes
//
// Expected stdout (HotSpot 25.0.3, -Djdk.attach.allowAttachSelf=true),
// MEASURED on the Windows box:
//
//   round 1
//   OneArg: loaded, one-arg args=x
//   Both: loaded, both: two-arg ran
//   PrivateMain: com.sun.tools.attach.AgentInitializationException [Agent JAR loaded but agent failed to initialize] saw none
//   InstanceMain: com.sun.tools.attach.AgentInitializationException [Agent JAR loaded but agent failed to initialize] saw none
//   Derived: com.sun.tools.attach.AgentInitializationException [Agent JAR loaded but agent failed to initialize] saw none
//   PackagePrivateClass: loaded, package-private class ran
//   round 2
//   (the six rows again, the same)
//
// With default flags HotSpot prints the one row
// `attach: java.io.IOException [Can not attach to current VM]`.
//
// CratonVM before wave 59 (code read: `run_self_attach` asked
// `method_exists`, which walks superclasses and ignores access): PrivateMain
// `loaded, private ran`, Derived `loaded, inherited ran`, InstanceMain
// invoked as a static method. Since wave 59 (under `CRATONVM_ATTACH_CHECKS`,
// on by default): JPLIS's declared-only, public, static lookup.
import com.sun.tools.attach.VirtualMachine;
import java.io.File;
import java.io.FileOutputStream;
import java.lang.instrument.Instrumentation;
import java.util.jar.Attributes;
import java.util.jar.JarOutputStream;
import java.util.jar.Manifest;

public class L1W59AgentmainShapes {
    static volatile String received = "none";

    public static class OneArg {
        public static void agentmain(String args) {
            received = "one-arg args=" + args;
        }
    }

    public static class Both {
        public static void agentmain(String args) {
            received = "both: one-arg ran";
        }

        public static void agentmain(String args, Instrumentation inst) {
            received = "both: two-arg ran";
        }
    }

    public static class PrivateMain {
        private static void agentmain(String args, Instrumentation inst) {
            received = "private ran";
        }
    }

    public static class InstanceMain {
        public void agentmain(String args, Instrumentation inst) {
            received = "instance ran";
        }
    }

    public static class Base {
        public static void agentmain(String args, Instrumentation inst) {
            received = "inherited ran";
        }
    }

    public static class Derived extends Base {
    }

    static class PackagePrivateClass {
        public static void agentmain(String args, Instrumentation inst) {
            received = "package-private class ran";
        }
    }

    static String jar(File dir, String name, String agentClass) throws Exception {
        Manifest manifest = new Manifest();
        manifest.getMainAttributes().put(Attributes.Name.MANIFEST_VERSION, "1.0");
        manifest.getMainAttributes().put(new Attributes.Name("Agent-Class"), agentClass);
        File file = new File(dir, name);
        try (JarOutputStream out = new JarOutputStream(new FileOutputStream(file), manifest)) {
            out.flush();
        }
        return file.getAbsolutePath();
    }

    static String load(VirtualMachine vm, String path) {
        received = "none";
        try {
            vm.loadAgent(path, "x");
            return "loaded, " + received;
        } catch (Throwable t) {
            return t.getClass().getName() + " [" + t.getMessage() + "] saw " + received;
        }
    }

    public static void main(String[] args) throws Exception {
        File dir = java.nio.file.Files.createTempDirectory("l1w59shape").toFile();
        String self = L1W59AgentmainShapes.class.getName();
        String[] names = {"OneArg", "Both", "PrivateMain", "InstanceMain", "Derived", "PackagePrivateClass"};
        String[] jars = new String[names.length];
        for (int i = 0; i < names.length; i++) {
            jars[i] = jar(dir, names[i] + ".jar", self + "$" + names[i]);
        }
        VirtualMachine vm;
        try {
            vm = VirtualMachine.attach(Long.toString(ProcessHandle.current().pid()));
        } catch (Exception e) {
            System.out.println("attach: " + e.getClass().getName() + " [" + e.getMessage() + "]");
            return;
        }
        for (int round = 1; round <= 2; round++) {
            System.out.println("round " + round);
            for (int i = 0; i < names.length; i++) {
                System.out.println(names[i] + ": " + load(vm, jars[i]));
            }
        }
        vm.detach();
    }
}
