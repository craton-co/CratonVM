// Regression vector for the MulticastSocket.close()/isClosed() split-state
// bug fixed 2026-09-21 (native-io/src/net.rs).
//
// `MulticastSocket.close()` and `.isClosed()` are both inherited from
// `DatagramSocket` in real JDK bytecode. This VM's `close()` for
// `java/net/MulticastSocket` is overridden by a field-slot-based native
// (native-io/src/net.rs, registered after phase-72 so it wins), while
// `isClosed()` had no such override and fell back to
// `net_phase_e::register_re7_datagram_socket`'s `ds_side_table` (keyed by
// ObjectRef, entirely separate storage) -- so `close()` set one piece of
// state and `isClosed()` read another, and a closed MulticastSocket reported
// `isClosed() == false` forever. Fixed by adding a matching MulticastSocket
// `isClosed()` override that reads the same field slot `close()` writes.
//
// Plain `DatagramSocket` never had this defect: it has no MulticastSocket-
// specific close() override competing with RE7's, so both its close() and
// isClosed() already agreed via the same side table -- this vector checks
// that shape too, as a control.
import java.net.*;

public class RMulticastSocketClose {

    static int checks = 0;
    static int fails = 0;

    static void check(String name, boolean actual, boolean expected) {
        checks++;
        System.out.println("CK RMulticastSocketClose " + name + "=" + actual);
        if (actual != expected) {
            fails++;
            System.out.println("CK RMulticastSocketClose FAILED " + name + " expected=" + expected);
        }
    }

    public static void main(String[] args) throws Exception {
        MulticastSocket ms = new MulticastSocket();
        check("mcast.beforeClose", ms.isClosed(), false);
        ms.close();
        check("mcast.afterClose", ms.isClosed(), true);

        DatagramSocket ds = new DatagramSocket();
        check("dgram.beforeClose", ds.isClosed(), false);
        ds.close();
        check("dgram.afterClose", ds.isClosed(), true);

        System.out.println("CK RMulticastSocketClose fails=" + fails);
        System.out.println("CK RMulticastSocketClose checks=" + checks);
        if (fails != 0) {
            throw new RuntimeException(fails + " checks failed");
        }
        System.out.println("PASS RMulticastSocketClose (" + checks + " checks)");
    }
}
