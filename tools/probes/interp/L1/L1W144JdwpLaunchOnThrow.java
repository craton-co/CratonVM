// Interpreter round i7 wave 144, lane L1: `-agentlib:jdwp=...,launch=`,
// `onthrow=` and `onuncaught=y`
// (docs/feature-designs/jvmti-delivery-threading.md,
// `CRATONVM_JDWP_AGENT_LAUNCH`).
//
// The launched "debugger" is this class again (`launched <transport>
// <address>`, the two words the back end appends): it attaches to the
// address, sets the debuggee's `seen` to what it was given (a numeric address
// as `port`), and disposes (which resumes a `suspend=y` VM). The debuggee
// waits up to 30 s for `seen` and prints it, so every line below is the
// debuggee's stdout (stderr: the uncaught exception's trace in case C, the
// error lines in D and E).
//
//   javac -g -d out L1W144JdwpLaunchOnThrow.java
//   L="launch=java -cp out L1W144JdwpLaunchOnThrow launched"     (java = HotSpot 25's, on PATH; no comma allowed)
//   A: <vm> "-agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5808,$L" -cp out L1W144JdwpLaunchOnThrow A
//   B: <vm> "-agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5809,onthrow=java.lang.IllegalStateException,$L" -cp out L1W144JdwpLaunchOnThrow B
//   C: <vm> "-agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5810,onuncaught=y,$L" -cp out L1W144JdwpLaunchOnThrow C
//   D: <vm> "-agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5811,onthrow=java.lang.Error" -cp out L1W144JdwpLaunchOnThrow A    (exit status 1)
//   E: <vm> "-agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5812,launch=" -cp out L1W144JdwpLaunchOnThrow A               (exit status 1)
//   F: <vm> "-agentlib:jdwp=transport=dt_socket,server=y,suspend=y,address=5813,onthrow=java.lang.IllegalStateException,$L" -cp out L1W144JdwpLaunchOnThrow B
//   with <vm> = java (HotSpot), cratonvm --java-home $JDK, cratonvm --compatible --java-home $JDK; also --nojit.
//
// CratonVM before wave 144: every case but D exits at start with
// `ERROR: JDWP launch, onthrow and onuncaught are not supported by the
// CratonVM JDWP server: ...` (status 1); `CRATONVM_JDWP_AGENT_LAUNCH=0`
// restores that (the negative control).
//
// With `launch=` HotSpot prints no `Listening for transport` line at all
// (MEASURED): not at the listen, not after the launched debugger disposes,
// and not at a deferred `onthrow` / `onuncaught` start. CratonVM suppresses
// both lines whenever `launch=` is set (wave 144,
// `debug::start_jdwp_transport`), so every row compares line for line.
//
// HOTSPOT_EXPECTED_BEGIN (MEASURED 2026-10-21, HotSpot 25.0.3, `l1w144.sh`; debuggee stdout per case;
// with `launch=` the back end prints no `Listening` line; the launched JDI child's own stderr is not compared)
// == A
// main A
// seen=dt_socket port
// == B (F the same; F's VM is suspended from the trigger until the launched debugger disposes)
// main B
// listening before the throw: false
// subclass thrown: listening false
// caught java.lang.IllegalStateException
// seen=dt_socket port
// == C (stderr: the thrower's uncaught RuntimeException trace)
// main C
// caught java.lang.IllegalStateException
// listening before the uncaught throw: false
// seen=dt_socket port
// == D (stderr, exit status 1)
// ERROR: JDWP Specify launch=<command line> when using onthrow or onuncaught suboption: -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5811,onthrow=java.lang.Error
// == E (stderr, exit status 1)
// ERROR: JDWP option syntax error: -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5812,launch=
// HOTSPOT_EXPECTED_END
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import java.util.*;

public class L1W144JdwpLaunchOnThrow {
    static volatile String seen;

    static final class SubState extends IllegalStateException {
    }

    static boolean listening(int port) {
        try (java.net.Socket s = new java.net.Socket()) {
            s.connect(new java.net.InetSocketAddress("127.0.0.1", port), 500);
            return true;
        } catch (java.io.IOException refused) {
            return false;
        }
    }

    static void awaitSeen() throws InterruptedException {
        long until = System.currentTimeMillis() + 30_000;
        while (seen == null && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        System.out.println("seen=" + seen);
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 3 && args[0].equals("launched")) {
            launched(args[1], args[2]);
            return;
        }
        String row = args.length > 0 ? args[0] : "A";
        System.out.println("main " + row);
        switch (row) {
            case "B" -> {
                System.out.println("listening before the throw: " + listening(5809));
                try {
                    throw new SubState();
                } catch (IllegalStateException e) {
                    // A subclass does not match `onthrow` (the signature is compared).
                }
                System.out.println("subclass thrown: listening " + listening(5809));
                try {
                    throw new IllegalStateException("now");
                } catch (IllegalStateException e) {
                    System.out.println("caught " + e.getClass().getName());
                }
            }
            case "C" -> {
                try {
                    throw new IllegalStateException("caught one");
                } catch (IllegalStateException e) {
                    System.out.println("caught " + e.getClass().getName());
                }
                System.out.println("listening before the uncaught throw: " + listening(5810));
                Thread t = new Thread(() -> {
                    throw new RuntimeException("uncaught");
                }, "thrower");
                t.start();
                t.join();
            }
            default -> {
            }
        }
        awaitSeen();
    }

    /** The launched debugger: attach, report what it was given, dispose. */
    static void launched(String transport, String address) throws Exception {
        AttachingConnector socket = null;
        for (AttachingConnector c : Bootstrap.virtualMachineManager().attachingConnectors()) {
            if (c.name().equals("com.sun.jdi.SocketAttach")) {
                socket = c;
            }
        }
        Map<String, Connector.Argument> a = socket.defaultArguments();
        int colon = address.lastIndexOf(':');
        a.get("hostname").setValue(colon >= 0 ? address.substring(0, colon) : "localhost");
        a.get("port").setValue(colon >= 0 ? address.substring(colon + 1) : address);
        a.get("timeout").setValue("20000");
        VirtualMachine vm = socket.attach(a);
        List<ReferenceType> main = vm.classesByName("L1W144JdwpLaunchOnThrow");
        if (!main.isEmpty()) {
            ClassType type = (ClassType) main.get(0);
            String shown = address.matches("\\d+") ? "port" : address;
            type.setValue(type.fieldByName("seen"), vm.mirrorOf(transport + " " + shown));
        }
        vm.dispose();
    }
}
