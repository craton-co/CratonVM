// Interpreter round i2 wave 59, lane L1: what ANOTHER process sees when it
// attaches to this VM through its attach listener -- the `load` operation
// (`VirtualMachine.loadAgent` / `loadAgentPath`), the `properties` and
// `agentProperties` operations, and `jcmd`'s `JVMTI.agent_load` -- against
// HotSpot. The client is `<java.home>/bin/java` running this file's `client`
// branch, as ByteBuddy's external attacher runs (under CratonVM `java.home` is
// the host JDK, so the client is HotSpot on both sides). The probe also prints
// what a self-attach answers with default flags, and whether the listener's
// socket exists before anyone attached (HotSpot starts it lazily).
//
// docs/internal/fixed-bugs/i57-L1-proposal-an-attach-listener-serves-external-attach-20261021.md
// docs/internal/fixed-bugs/i56-L1-the-vm-sets-jdk-attach-allowattachself-by-default-20261020.md
//
// Default flags. The agent JARs carry only a manifest; their `Agent-Class`
// is this file's nested class, already on the class path, so the agent
// records what it received in the probe's own static list.
//
// Expected stdout (HotSpot 25.0.3, default flags), MEASURED on the Windows
// box (the attach transport there is HotSpot's remote-thread enqueue; the
// Linux host's is `/tmp/.java_pid<pid>`; the rows are the client's view and
// carry no path):
//
//   allowAttachSelf=null
//   self attach: java.io.IOException [Can not attach to current VM]
//   socket before attach: false
//   round 1
//   client: attach ok
//   client: good: loaded
//   client: good opts: loaded
//   client: no Agent-Class: com.sun.tools.attach.AgentLoadException [Agent JAR not found or no Agent-Class attribute]
//   client: throws: com.sun.tools.attach.AgentInitializationException [Agent JAR loaded but agent failed to initialize]
//   client: missing: com.sun.tools.attach.AgentLoadException [Agent JAR not found or no Agent-Class attribute]
//   client: agentpath missing: com.sun.tools.attach.AgentLoadException prefix=true
//   client: props custom=[a=b c\né\\ #x] lineSeparatorMatches=true count>20=true
//   client: agentProps keys=[sun.java.command, sun.jvm.args, sun.jvm.flags] command=[L1W59AttachListenerOps]
//   client: jcmd agent_load good: [return code: 0]
//   client: jcmd agent_load good opt: [return code: 0]
//   client: jcmd agent_load no Agent-Class: [return code: 100]
//   client: jcmd agent_load no args: com.sun.tools.attach.AttachOperationFailedException [java.lang.IllegalArgumentException: The argument 'library path' is mandatory.]
//   client: detached
//   client exit=0
//   agent saw [null, k=v, null, opt]
//   round 2
//   (the round-1 rows again, the same)
//
// The `self attach` row and `allowAttachSelf=null` are the i56 page's rows;
// CratonVM before wave 59 printed `allowAttachSelf=true` / `self attach:
// attach ok`. On Linux it printed `socket before attach: true` (bound at
// boot) and, with `CRATONVM_ATTACH_LISTENER_LOAD` off, every `load` row
// `AgentLoadException`, the `properties` row with `\n` unescaped (the client's
// `Properties.load` read a broken value), `getAgentProperties` failing, and
// `JVMTI.agent_load` an unknown command. On Windows CratonVM has no listener
// and every client row is `AttachNotSupportedException` (a known gap).
//
//   javac -d /tmp/l1w59ops tools/probes/interp/L1/L1W59AttachListenerOps.java
//   java     -cp /tmp/l1w59ops L1W59AttachListenerOps
//   cratonvm -cp /tmp/l1w59ops L1W59AttachListenerOps
import com.sun.tools.attach.VirtualMachine;
import java.io.BufferedReader;
import java.io.File;
import java.io.FileOutputStream;
import java.io.InputStream;
import java.io.InputStreamReader;
import java.lang.instrument.Instrumentation;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.Properties;
import java.util.TreeSet;
import java.util.concurrent.TimeUnit;
import java.util.jar.Attributes;
import java.util.jar.JarOutputStream;
import java.util.jar.Manifest;

public class L1W59AttachListenerOps {
    static final List<String> SEEN = Collections.synchronizedList(new ArrayList<>());
    static final String CUSTOM = "a=b c\né\\ #x";

    public static class Agent {
        public static void agentmain(String args, Instrumentation inst) {
            SEEN.add(String.valueOf(args));
        }
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

    static String describe(Throwable t) {
        return t.getClass().getName() + " [" + t.getMessage() + "]";
    }

    static String visible(String s) {
        if (s == null) {
            return "null";
        }
        StringBuilder b = new StringBuilder();
        for (char c : s.toCharArray()) {
            if (c == '\n') {
                b.append("\\n");
            } else if (c == '\r') {
                b.append("\\r");
            } else if (c == '\\') {
                b.append("\\\\");
            } else if (c < 0x20 || c > 0x7e) {
                b.append(String.format("\\u%04x", (int) c));
            } else {
                b.append(c);
            }
        }
        return b.toString();
    }

    // ---- the client, in a separate `java` process ----------------------------

    static void load(VirtualMachine vm, String label, String path, String options) {
        try {
            vm.loadAgent(path, options);
            System.out.println(label + ": loaded");
        } catch (Throwable t) {
            System.out.println(label + ": " + describe(t));
        }
    }

    static String jcmd(VirtualMachine vm, String line) {
        try {
            // `HotSpotVirtualMachine.executeJCmd`, by reflection (the client
            // runs with `--add-exports jdk.attach/sun.tools.attach=ALL-UNNAMED`).
            java.lang.reflect.Method m = vm.getClass().getSuperclass().getDeclaredMethod("executeJCmd", String.class);
            m.setAccessible(true);
            try (InputStream in = (InputStream) m.invoke(vm, line)) {
                String text = new String(in.readAllBytes(), StandardCharsets.UTF_8);
                return "[" + visible(text.strip()) + "]";
            }
        } catch (java.lang.reflect.InvocationTargetException e) {
            return describe(e.getCause());
        } catch (Throwable t) {
            return describe(t);
        }
    }

    static void client(String pid, String dir) throws Exception {
        VirtualMachine vm;
        try {
            vm = VirtualMachine.attach(pid);
            System.out.println("attach ok");
        } catch (Throwable t) {
            System.out.println("attach: " + describe(t));
            return;
        }
        String good = new File(dir, "good.jar").getAbsolutePath();
        String plain = new File(dir, "plain.jar").getAbsolutePath();
        load(vm, "good", good, null);
        load(vm, "good opts", good, "k=v");
        load(vm, "no Agent-Class", plain, null);
        load(vm, "throws", new File(dir, "thrower.jar").getAbsolutePath(), null);
        load(vm, "missing", new File(dir, "absent.jar").getAbsolutePath(), null);
        try {
            vm.loadAgentPath(new File(dir, "absent-native-agent").getAbsolutePath(), null);
            System.out.println("agentpath missing: loaded");
        } catch (Throwable t) {
            System.out.println("agentpath missing: " + t.getClass().getName() + " prefix="
                    + String.valueOf(t.getMessage()).startsWith("Failed to load agent library:"));
        }
        try {
            Properties p = vm.getSystemProperties();
            System.out.println("props custom=[" + visible(p.getProperty("l1w59.custom")) + "]"
                    + " lineSeparatorMatches=" + System.lineSeparator().equals(p.getProperty("line.separator"))
                    + " count>20=" + (p.size() > 20));
        } catch (Throwable t) {
            System.out.println("props: " + describe(t));
        }
        try {
            Properties p = vm.getAgentProperties();
            System.out.println("agentProps keys=" + new TreeSet<>(p.stringPropertyNames())
                    + " command=[" + visible(p.getProperty("sun.java.command")) + "]");
        } catch (Throwable t) {
            System.out.println("agentProps: " + describe(t));
        }
        System.out.println("jcmd agent_load good: " + jcmd(vm, "JVMTI.agent_load " + good));
        System.out.println("jcmd agent_load good opt: " + jcmd(vm, "JVMTI.agent_load " + good + " opt"));
        System.out.println("jcmd agent_load no Agent-Class: " + jcmd(vm, "JVMTI.agent_load " + plain));
        System.out.println("jcmd agent_load no args: " + jcmd(vm, "JVMTI.agent_load"));
        vm.detach();
        System.out.println("detached");
    }

    // ---- the target ----------------------------------------------------------

    public static void main(String[] args) throws Exception {
        if (args.length == 3 && args[0].equals("client")) {
            client(args[1], args[2]);
            return;
        }
        String pid = Long.toString(ProcessHandle.current().pid());
        System.out.println("allowAttachSelf=" + System.getProperty("jdk.attach.allowAttachSelf"));
        try {
            VirtualMachine.attach(pid).detach();
            System.out.println("self attach: attach ok");
        } catch (Throwable t) {
            System.out.println("self attach: " + describe(t));
        }
        System.out.println("socket before attach: " + new File("/tmp/.java_pid" + pid).exists());
        System.setProperty("l1w59.custom", CUSTOM);
        File dir = java.nio.file.Files.createTempDirectory("l1w59ops").toFile();
        String nested = L1W59AttachListenerOps.class.getName();
        jar(dir, "good.jar", nested + "$Agent");
        jar(dir, "plain.jar", null);
        jar(dir, "thrower.jar", nested + "$Thrower");
        String java = System.getProperty("java.home") + File.separator + "bin" + File.separator + "java";
        for (int round = 1; round <= 2; round++) {
            System.out.println("round " + round);
            SEEN.clear();
            List<String> cmd = new ArrayList<>();
            cmd.add(java);
            // Exercise the legacy Windows enqueue and Unix v1 framing too.
            if (Boolean.getBoolean("i47.attach.compat")) cmd.add("-Djdk.attach.compat=true");
            cmd.addAll(List.of("--add-exports", "jdk.attach/sun.tools.attach=ALL-UNNAMED",
                    "-cp", System.getProperty("java.class.path"), nested, "client", pid, dir.getAbsolutePath()));
            Process p = new ProcessBuilder(cmd).redirectError(ProcessBuilder.Redirect.DISCARD).start();
            try (BufferedReader r = new BufferedReader(new InputStreamReader(p.getInputStream(), StandardCharsets.UTF_8))) {
                String line;
                while ((line = r.readLine()) != null) {
                    System.out.println("client: " + line);
                }
            }
            boolean done = p.waitFor(120, TimeUnit.SECONDS);
            System.out.println("client exit=" + (done ? Integer.toString(p.exitValue()) : "timeout"));
            System.out.println("agent saw " + SEEN);
        }
    }
}
