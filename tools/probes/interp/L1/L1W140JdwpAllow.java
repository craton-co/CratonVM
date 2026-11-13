// Interpreter round i7 wave 140, lane L1: `-agentlib:jdwp=...,allow=<peers>`
// (docs/feature-designs/jvmti-delivery-threading.md).
//
// The debuggee (`wait`) listens with an `allow=` list and waits up to 20 s
// for the debugger to set `go`; it then prints `ended`. The debugger
// (`debug <port>`) attaches from 127.0.0.1 and prints `attached` (and sets
// `go`) or `refused: <exception class>`. Each case is one debuggee and one
// debugger run; compare the debugger's stdout and the debuggee's stdout and
// stderr (the `Debugger failed to attach` / `ERROR` lines; the JVM's own
// warnings aside).
//
//   javac -g -d out L1W140JdwpAllow.java
//   A: cratonvm --java-home $JDK "-agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=127.0.0.1:5802,allow=127.0.0.2" -cp out L1W140JdwpAllow wait
//   B: ... allow=127.0.0.1
//   C: ... allow=127.0.0.0/8
//   D: ... allow=10.1.2.3+127.0.0.1
//   E: ... allow=*                 (quote it in a shell)
//   F: ... allow=::ffff:127.0.0.1
//   G: ... allow=10.1.2.3;127.0.0.1  (fails at start: `;` is no separator)
//   H: ... allow=127.0.0.1/40      (fails at start)
//   I: ... allow=*+127.0.0.1       (fails at start)
//   J: "-agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=127.0.0.1:5802,timeout=1500" (no debugger)
//   for each: start the debuggee, wait 2 s, run `java -cp out L1W140JdwpAllow debug 5802`
//   (HotSpot: the same `java` command lines with `java` in place of `cratonvm --java-home $JDK`.)
//
// CratonVM before wave 140 (predicted): `allow` dropped, so case A attaches
// and G-I start and wait; and the `Listening` line is printed once (cases
// B-F print it twice on HotSpot: the transport listens anew after the
// session). `CRATONVM_JDWP_AGENT_ALLOW=0` and `CRATONVM_JDWP_AGENT_RELISTEN_LINE=0`
// are the negative controls.
//
// HotSpot 25.0.3, MEASURED on Windows (Temurin 25.0.3+9), default flags. Per
// case: the debugger's stdout, then `|` the debuggee's stdout, then `!` its
// stderr (exit status 0 for A-F, 2 for G-I). More lists measured the same
// way are in vm/src/debug/mod.rs `i140_l1_jdwp_allow_tests`.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, Windows, default flags)
// == A allow=127.0.0.2
// refused: java.net.SocketException
// | Listening for transport dt_socket at address: 5802
// | ended go=0
// ! Debugger failed to attach: ERROR: Peer not allowed to connect: 127.0.0.1
// == B allow=127.0.0.1 (C 127.0.0.0/8, D 10.1.2.3+127.0.0.1, E *, F ::ffff:127.0.0.1: the same)
// attached
// | Listening for transport dt_socket at address: 5802
// | Listening for transport dt_socket at address: 5802
// | ended go=1
// == G allow=10.1.2.3;127.0.0.1
// refused: java.net.ConnectException
// ! Error in allow option: '10.1.2.3;127.0.0.1'
// ! ERROR: transport error 103: invalid IP address in allow option
// ! ERROR: JDWP Transport dt_socket failed to initialize, TRANSPORT_INIT(510)
// ! JDWP exit error AGENT_ERROR_TRANSPORT_INIT(197): No transports initialized [s\src\jdk.jdwp.agent\share\native\libjdwp\debugInit.c:697]
// == H allow=127.0.0.1/40
// refused: java.net.ConnectException
// ! Error in allow option: '40'
// ! ERROR: transport error 103: invalid netmask in allow option
// ! (the same two lines as G)
// == I allow=*+127.0.0.1
// refused: java.net.ConnectException
// ! Error in allow option: '*+127.0.0.1'
// ! ERROR: transport error 103: allow option '*' cannot be expanded
// ! (the same two lines as G)
// == J timeout=1500 (no allow), no debugger started (CRATONVM_JDWP_AGENT_ACCEPT_TIMEOUT)
// | Listening for transport dt_socket at address: 5802
// ! ERROR: transport error 203: timed out waiting for connection
// ! JDWP exit error JVMTI_ERROR_NONE(0): could not connect, timeout or fatal error [s\src\jdk.jdwp.agent\share\native\libjdwp\transport.c:368]
// (exit status 0 after about 1.5 s; no `ended` line. The same after a
// session: attached and disposed at 1 s with timeout=3000, the process
// ended at 4 s with these two lines.)
// HOTSPOT_EXPECTED_END
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import java.util.*;

public class L1W140JdwpAllow {
    static volatile int go;

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            debug(Integer.parseInt(args[1]));
            return;
        }
        long until = System.currentTimeMillis() + 20_000;
        while (go == 0 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        System.out.println("ended go=" + go);
    }

    static void debug(int port) throws Exception {
        AttachingConnector socket = null;
        for (AttachingConnector c : Bootstrap.virtualMachineManager().attachingConnectors()) {
            if (c.name().equals("com.sun.jdi.SocketAttach")) {
                socket = c;
            }
        }
        Map<String, Connector.Argument> a = socket.defaultArguments();
        a.get("hostname").setValue("127.0.0.1");
        a.get("port").setValue(Integer.toString(port));
        a.get("timeout").setValue("10000");
        try {
            VirtualMachine vm = socket.attach(a);
            System.out.println("attached");
            List<ReferenceType> main = vm.classesByName("L1W140JdwpAllow");
            if (!main.isEmpty()) {
                ClassType type = (ClassType) main.get(0);
                type.setValue(type.fieldByName("go"), vm.mirrorOf(1));
            }
            vm.dispose();
        } catch (Exception refused) {
            System.out.println("refused: " + refused.getClass().getName());
        }
    }
}
