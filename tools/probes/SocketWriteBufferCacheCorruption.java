import java.io.*;
import java.net.*;

/// Repro for the `Util$BufferCache` NullPointerException /
/// `os error 10053` connection-abort family found via the 2026-09-22 Tomcat
/// census (`TestJNDIRealmIntegration`, `TestNonBlockingAPI`). No Tomcat, no
/// LDAP, a single Java thread: a plain blocking `Socket.getOutputStream()
/// .write()` in a tight loop, which routes through `sun.nio.ch
/// .NioSocketImpl.tryWrite` -> `Util.getTemporaryDirectBuffer` ->
/// `Util$BufferCache.get()`. Non-deterministically, sometimes after only a
/// few hundred bytes and sometimes after hundreds of thousands, that
/// per-thread cache's own invariant (`count > 0` implies `buffers[start]`
/// non-null) is violated, throwing `NullPointerException: Cannot invoke
/// "java.nio.ByteBuffer.capacity()" because "buf" is null` from inside
/// `BufferCache.get()` itself — or the connection aborts first instead.
///
/// Ruled out already (see fixed-suite-bugs/tomcat/33...): cross-thread
/// `ThreadLocal` sharing (a 16-thread ThreadLocal probe is clean on both
/// VMs; this repro is single-threaded, so that's moot anyway), JIT
/// miscompilation (`--nojit` still fails, just earlier and with a
/// different shape), ZGC-specific relocation (`-XX:+UseG1GC` reproduces
/// the identical NPE). Not yet root-caused; the working hypothesis is a
/// native-level race around the GC-blocking-region protocol
/// `native-io/src/socket_channel.rs`'s `sc_write` documents.
///
/// Usage: `cratonvm --jdk-only -cp <dir> SocketWriteBufferCacheCorruption [iters]`
/// (default 2000000). Compare against a real JDK for a clean run every
/// time.
public class SocketWriteBufferCacheCorruption {
    public static void main(String[] args) throws Exception {
        int iters = Integer.parseInt(args.length > 0 ? args[0] : "2000000");
        ServerSocket ss = new ServerSocket(0, 50, InetAddress.getByName("127.0.0.1"));
        int port = ss.getLocalPort();
        System.out.println("server on port " + port);
        Thread server = new Thread(() -> {
            try (Socket s = ss.accept()) {
                InputStream is = s.getInputStream();
                byte[] buf = new byte[65536];
                long total = 0;
                int n;
                while ((n = is.read(buf)) > 0) total += n;
                System.out.println("server read total = " + total);
            } catch (Exception e) {
                System.out.println("server exception: " + e);
            }
        });
        server.setDaemon(true);
        server.start();

        Socket client = new Socket("127.0.0.1", port);
        OutputStream os = client.getOutputStream();
        byte[] chunk = new byte[8];
        long start = System.currentTimeMillis();
        long written = 0;
        try {
            for (int i = 0; i < iters; i++) {
                os.write(chunk);
                os.flush();
                written += chunk.length;
            }
            System.out.println("write done, wrote=" + written + " in " + (System.currentTimeMillis() - start) + "ms");
        } catch (IOException e) {
            System.out.println("write FAILED after " + written + " bytes: " + e);
        }
        client.shutdownOutput();
        Thread.sleep(500);
        client.close();
        ss.close();
        System.out.println("DONE OK");
    }
}
