package cratonvm;

import java.io.IOException;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.channels.SelectionKey;
import java.nio.channels.Selector;
import java.nio.channels.SocketChannel;
import java.util.Iterator;
import java.util.Set;

/// Regression fixture for the `build_set` cross-call GC-safety fix
/// (native-io/src/nio_selector.rs, 2026-07-07). `Selector.selectedKeys()` /
/// `.keys()` build a fresh `java.util.HashSet` natively and populate it via
/// `HashSet.<init>` + repeated `Set.add` calls; each of those runs Java
/// bytecode that can trigger a moving GC. The Rust-side `set` local (and
/// each pending key local) were not GC roots, so under GC pressure the
/// returned Set (or a key inside it) could go stale before the caller ever
/// dereferences it -- matching the Tomcat Tribes `NioReceiver.listen()`
/// crash shape: an all-zero-header `java/util/Set` receiver +
/// `NoSuchMethodError Object.add` right after `selectedKeys()` returns.
///
/// Drives many connect/register/select/selectedKeys().iterator() cycles
/// under `CRATONVM_GC_STRESS` so the window is exercised many times per run.
public class NioSelectorBuildSetGc {
    public static void main(String[] args) throws IOException {
        ServerSocket server = new ServerSocket(0);
        server.setReuseAddress(true);
        int port = server.getLocalPort();

        Selector selector = Selector.open();
        int iterations = 400;
        int observedReady = 0;

        for (int i = 0; i < iterations; i++) {
            // Allocation churn between selects to keep the young gen under
            // pressure (on top of CRATONVM_GC_STRESS forcing GC every N bytes).
            Object[] churn = new Object[16];
            for (int c = 0; c < churn.length; c++) {
                churn[c] = new byte[512];
            }

            SocketChannel[] clients = new SocketChannel[2];
            Socket[] accepted = new Socket[2];
            for (int c = 0; c < clients.length; c++) {
                SocketChannel client = SocketChannel.open();
                client.configureBlocking(false);
                client.connect(new InetSocketAddress("127.0.0.1", port));
                // Finish the handshake (accept + finishConnect) so the key is
                // actually READ/WRITE-ready on the next select. Using two keys
                // exercises the selectedKeys().iterator().next()/remove() path
                // that Tomcat Tribes' ParallelNioSender drives.
                accepted[c] = server.accept();
                long deadline = System.nanoTime() + 2_000_000_000L;
                while (!client.finishConnect()) {
                    if (System.nanoTime() > deadline) {
                        throw new AssertionError(
                            "finishConnect timed out at iter " + i + " client " + c);
                    }
                }
                client.register(selector, SelectionKey.OP_WRITE);
                clients[c] = client;
            }

            int n = selector.select(1000);
            if (n > 0) {
                // The exact call sequence that crashed: selectedKeys() builds
                // a fresh native HashSet, then .iterator()/hasNext()/next()
                // dereference the (possibly-stale) receiver + elements.
                Set<SelectionKey> ready = selector.selectedKeys();
                Iterator<SelectionKey> it = ready.iterator();
                while (it.hasNext()) {
                    SelectionKey key = it.next();
                    if (key.isWritable()) {
                        observedReady++;
                    }
                    it.remove();
                    key.cancel();
                }
                if (!ready.isEmpty()) {
                    throw new AssertionError(
                        "Iterator.remove left selectedKeys non-empty at iter " + i);
                }
            }

            // Also exercise keys() (the other build_set caller) every few
            // iterations.
            if (i % 7 == 0) {
                Set<SelectionKey> all = selector.keys();
                int sz = all.size();
                if (sz < 0) {
                    throw new AssertionError("negative keys() size at iter " + i);
                }
            }

            for (int c = 0; c < clients.length; c++) {
                clients[c].close();
                accepted[c].close();
            }
        }

        selector.close();
        server.close();

        if (observedReady == 0) {
            throw new AssertionError("never observed a ready key across " + iterations + " iterations");
        }
        System.out.println("NIO_SELECTOR_BUILD_SET_GC_OK observedReady=" + observedReady);
    }
}
