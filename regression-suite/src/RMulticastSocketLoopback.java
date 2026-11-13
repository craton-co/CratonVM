// Regression vector for the MulticastSocket.joinGroup(InetAddress) /
// leaveGroup(InetAddress) / setLoopbackMode / getLoopbackMode fix landed
// 2026-09-21 (native-io/src/net.rs).
//
// MulticastSocket declares joinGroup(InetAddress)/leaveGroup(InetAddress)
// itself in real JDK bytecode (unlike close()/isClosed(), inherited from
// DatagramSocket), and both methods' first bytecode instruction is
// `this.delegate()`, which throws InternalError("Should not get here")
// because this VM never populates that field. The existing native
// registration for these exact triples in
// native-builtins/src/phases_late/net_channels.rs (register_p72_datagram)
// is never reached by ordinary dispatch -- measured directly with a debug
// print that never fired. setLoopbackMode/getLoopbackMode had no native
// registration anywhere at all. Fixed by adding all four to
// native-io/src/net.rs's register_multicast_socket_overrides, the one
// registrar proven (by close()/getTimeToLive(), and now these) to reliably
// win dispatch for MulticastSocket.
import java.net.*;

public class RMulticastSocketLoopback {

    static int checks = 0;
    static int fails = 0;

    static void check(String name, boolean actual, boolean expected) {
        checks++;
        System.out.println("CK RMulticastSocketLoopback " + name + "=" + actual);
        if (actual != expected) {
            fails++;
            System.out.println("CK RMulticastSocketLoopback FAILED " + name + " expected=" + expected);
        }
    }

    public static void main(String[] args) throws Exception {
        InetAddress group = InetAddress.getByName("230.0.0.1");
        MulticastSocket s = new MulticastSocket();

        // Default loopback delivery is enabled, i.e. NOT disabled.
        check("defaultDisabled", s.getLoopbackMode(), false);

        s.setLoopbackMode(true);
        check("afterSetDisableTrue", s.getLoopbackMode(), true);

        s.setLoopbackMode(false);
        check("afterSetDisableFalse", s.getLoopbackMode(), false);

        boolean joined = true;
        try {
            s.joinGroup(group);
        } catch (Throwable t) {
            joined = false;
            System.out.println("CK RMulticastSocketLoopback joinGroup threw: " + t);
        }
        check("joinGroupSucceeded", joined, true);

        boolean left = true;
        try {
            s.leaveGroup(group);
        } catch (Throwable t) {
            left = false;
            System.out.println("CK RMulticastSocketLoopback leaveGroup threw: " + t);
        }
        check("leaveGroupSucceeded", left, true);

        check("beforeClose", s.isClosed(), false);
        s.close();
        check("afterClose", s.isClosed(), true);

        System.out.println("CK RMulticastSocketLoopback fails=" + fails);
        System.out.println("CK RMulticastSocketLoopback checks=" + checks);
        if (fails != 0) {
            throw new RuntimeException(fails + " checks failed");
        }
        System.out.println("PASS RMulticastSocketLoopback (" + checks + " checks)");
    }
}
