import java.io.IOException;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.net.InetSocketAddress;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.util.List;
import java.util.Locale;

/**
 * Is a slow connected close a GC pause that happened to land in close()?
 *
 * For each close, reads the summed collection count of every collector MXBean
 * immediately before and after, and times close() alone. A slow close with a
 * count delta of 0 cannot be a GC pause; one with a delta >= 1 very likely is.
 * Also reports how many collections ran in total and during how many closes, so
 * a result can be read even if the MXBean counts are coarse.
 *
 * Usage: CloseGcCorrelate [closes] [slowMicros]
 */
public final class CloseGcCorrelate {
	private static final List<GarbageCollectorMXBean> GCS = ManagementFactory.getGarbageCollectorMXBeans();

	private static long gcCount() {
		long n = 0;
		for (GarbageCollectorMXBean b : GCS) {
			long c = b.getCollectionCount();
			if (c > 0) {
				n += c;
			}
		}
		return n;
	}

	public static void main(String[] a) throws Exception {
		int n = a.length > 0 ? Integer.parseInt(a[0]) : 3000;
		long slowNs = (a.length > 1 ? Long.parseLong(a[1]) : 5000) * 1000L;
		ServerSocketChannel ls = ServerSocketChannel.open().bind(new InetSocketAddress("127.0.0.1", 0), 1024);
		InetSocketAddress addr = (InetSocketAddress) ls.getLocalAddress();
		Thread t = new Thread(() -> {
			try {
				while (true) {
					ls.accept().close();
				}
			} catch (IOException ignored) {
			}
		});
		t.setDaemon(true);
		t.start();

		long start = System.nanoTime();
		long gcStart = gcCount();
		int slow = 0, slowWithGc = 0, fastWithGc = 0;
		StringBuilder detail = new StringBuilder();
		for (int i = 0; i < n; i++) {
			SocketChannel c = SocketChannel.open(addr);
			long g0 = gcCount();
			long t0 = System.nanoTime();
			c.close();
			long d = System.nanoTime() - t0;
			long g1 = gcCount();
			boolean gc = g1 > g0;
			if (d > slowNs) {
				slow++;
				if (gc) {
					slowWithGc++;
				}
				if (detail.length() < 1200) {
					detail.append(String.format(Locale.ROOT, "  #%d t=%dms close=%.1fms gcDelta=%d%n", i,
							(t0 - start) / 1_000_000, d / 1e6, g1 - g0));
				}
			} else if (gc) {
				fastWithGc++;
			}
		}
		System.out.printf(Locale.ROOT,
				"closes=%d slow(>%dus)=%d slowWithGc=%d fastWithGc=%d totalGcs=%d collectors=%d%n", n,
				slowNs / 1000, slow, slowWithGc, fastWithGc, gcCount() - gcStart, GCS.size());
		System.out.print(detail);
		System.exit(0);
	}
}
