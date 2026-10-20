// Interpreter round i2 wave 59, lane L1: `-XX:+DisableAttachMechanism`
// (docs/internal/fixed-bugs/interpreter-L1-disable-attach-mechanism-is-not-honoured-FIXED-20261023.md).
//
// The battery runs probes with default flags, so the probe starts ITSELF
// again as a child with the flag: the same launcher
// (`ProcessHandle.current().info().command()`) and the launcher options that
// preceded the main class (so a CratonVM child runs in the parent's mode),
// once with `-XX:+DisableAttachMechanism` and once with it and
// `-Djdk.attach.allowAttachSelf=true`. The child self-attaches (its own pid,
// an id that is not a number, its own pid again) and reports whether a
// listener socket exists; the parent relays the child's lines.
//
// Expected stdout (HotSpot 25.0.3, default flags), MEASURED on the Windows
// box:
//
//   disabled: allowAttachSelf=null
//   disabled: own: com.sun.tools.attach.AttachNotSupportedException [The VM does not support the attach mechanism]
//   disabled: abc: com.sun.tools.attach.AttachNotSupportedException [Invalid process identifier: abc]
//   disabled: own again: com.sun.tools.attach.AttachNotSupportedException [The VM does not support the attach mechanism]
//   disabled: socket: false
//   disabled exit=0
//   disabled+allow: allowAttachSelf=true
//   disabled+allow: own: com.sun.tools.attach.AttachNotSupportedException [The VM does not support the attach mechanism]
//   disabled+allow: abc: com.sun.tools.attach.AttachNotSupportedException [Invalid process identifier: abc]
//   disabled+allow: own again: com.sun.tools.attach.AttachNotSupportedException [The VM does not support the attach mechanism]
//   disabled+allow: socket: false
//   disabled+allow exit=0
//   enabled+allow: own: attach ok
//   enabled+allow exit=0
//
// CratonVM before wave 59 (MEASURED in wave 58, debug build, both modes):
// `disabled: allowAttachSelf=true`, `own: attach ok` in both children, and on
// Linux `socket: true` (bound at boot whatever the flag).
//
//   javac -d /tmp/l1w59dis tools/probes/interp/L1/L1W59DisableAttachMechanism.java
//   java     -cp /tmp/l1w59dis L1W59DisableAttachMechanism
//   cratonvm -cp /tmp/l1w59dis L1W59DisableAttachMechanism
import com.sun.tools.attach.VirtualMachine;
import java.io.BufferedReader;
import java.io.File;
import java.io.InputStreamReader;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.TimeUnit;

public class L1W59DisableAttachMechanism {
    static String attach(String id) {
        try {
            VirtualMachine.attach(id).detach();
            return "attach ok";
        } catch (Throwable t) {
            return t.getClass().getName() + " [" + t.getMessage() + "]";
        }
    }

    static void child(boolean full) {
        String pid = Long.toString(ProcessHandle.current().pid());
        if (full) {
            System.out.println("allowAttachSelf=" + System.getProperty("jdk.attach.allowAttachSelf"));
        }
        System.out.println("own: " + attach(pid));
        if (full) {
            System.out.println("abc: " + attach("abc"));
            System.out.println("own again: " + attach(pid));
            System.out.println("socket: " + new File("/tmp/.java_pid" + pid).exists());
        }
    }

    static void run(String label, String... flags) throws Exception {
        String self = L1W59DisableAttachMechanism.class.getName();
        List<String> cmd = new ArrayList<>();
        cmd.add(ProcessHandle.current().info().command().orElse("java"));
        String[] argv = ProcessHandle.current().info().arguments().orElse(new String[0]);
        int main = Arrays.asList(argv).indexOf(self);
        if (main > 0) {
            cmd.addAll(Arrays.asList(argv).subList(0, main));
        } else {
            cmd.add("-cp");
            cmd.add(System.getProperty("java.class.path"));
        }
        cmd.addAll(Arrays.asList(flags));
        cmd.add(self);
        cmd.add(label.startsWith("enabled") ? "short" : "child");
        Process p = new ProcessBuilder(cmd).redirectError(ProcessBuilder.Redirect.DISCARD).start();
        try (BufferedReader r = new BufferedReader(new InputStreamReader(p.getInputStream(), StandardCharsets.UTF_8))) {
            String line;
            while ((line = r.readLine()) != null) {
                System.out.println(label + ": " + line);
            }
        }
        boolean done = p.waitFor(120, TimeUnit.SECONDS);
        System.out.println(label + " exit=" + (done ? Integer.toString(p.exitValue()) : "timeout"));
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 1 && args[0].equals("child")) {
            child(true);
            return;
        }
        if (args.length == 1 && args[0].equals("short")) {
            child(false);
            return;
        }
        run("disabled", "-XX:+DisableAttachMechanism");
        run("disabled+allow", "-XX:+DisableAttachMechanism", "-Djdk.attach.allowAttachSelf=true");
        // The control: the same child without the flag attaches.
        run("enabled+allow", "-Djdk.attach.allowAttachSelf=true");
    }
}
