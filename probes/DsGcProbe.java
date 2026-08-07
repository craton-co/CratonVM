import java.net.*;

/** DatagramSocket option/peer state must survive a moving young collection.
 *  Both live in ObjectRef-keyed side tables, so a relocated socket that loses
 *  its entry silently reverts to defaults instead of failing. */
public class DsGcProbe {
    static final int N = 64;
    public static void main(String[] a) throws Exception {
        DatagramSocket[] socks = new DatagramSocket[N];
        int[] timeouts = new int[N];
        for (int i = 0; i < N; i++) {
            socks[i] = new DatagramSocket();
            timeouts[i] = 1000 + i;
            socks[i].setSoTimeout(timeouts[i]);
            socks[i].setBroadcast(i % 2 == 0);
            socks[i].connect(InetAddress.getByName("127.0.0.1"), 40000 + i);
        }
        // Churn the young generation hard enough to relocate the sockets.
        Object sink = null;
        for (int r = 0; r < 400; r++) {
            for (int k = 0; k < 2000; k++) sink = new byte[256];
            if (r % 100 == 0) System.gc();
        }
        if (sink == null) throw new IllegalStateException();

        int bad = 0;
        for (int i = 0; i < N; i++) {
            int t = socks[i].getSoTimeout();
            boolean b = socks[i].getBroadcast();
            int port = socks[i].getPort();
            InetAddress peer = socks[i].getInetAddress();
            boolean conn = socks[i].isConnected();
            if (t != timeouts[i]) { System.out.println("FAIL[" + i + "] soTimeout " + t + " != " + timeouts[i]); bad++; }
            if (b != (i % 2 == 0)) { System.out.println("FAIL[" + i + "] broadcast " + b); bad++; }
            if (port != 40000 + i) { System.out.println("FAIL[" + i + "] port " + port + " != " + (40000 + i)); bad++; }
            if (peer == null || !"127.0.0.1".equals(peer.getHostAddress())) { System.out.println("FAIL[" + i + "] peer " + peer); bad++; }
            if (!conn) { System.out.println("FAIL[" + i + "] isConnected false"); bad++; }
        }
        for (DatagramSocket s : socks) s.close();
        System.out.println(bad == 0 ? "DSGCPROBE OK" : "DSGCPROBE FAILED count=" + bad);
    }
}
