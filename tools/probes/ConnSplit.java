import java.io.IOException;
import java.net.*;
import java.nio.channels.*;
import java.util.Locale;

/** Prices open(connect) and close() SEPARATELY, plus the server's accept,
 *  with a percentile view: a sleep-poll shows up as a bimodal tail (p50 tiny,
 *  p90 ~ the sleep quantum), where a uniformly slow call does not. */
public final class ConnSplit {
	public static void main(String[] a) throws Exception {
		int n = a.length > 0 ? Integer.parseInt(a[0]) : 300;
		ServerSocketChannel ls = ServerSocketChannel.open().bind(new InetSocketAddress("127.0.0.1", 0), 1024);
		InetSocketAddress addr = (InetSocketAddress) ls.getLocalAddress();
		long[] acc = new long[n + 60];
		Thread t = new Thread(() -> {
			try {
				for (int i = 0; ; i++) {
					long t0 = System.nanoTime();
					SocketChannel c = ls.accept();
					if (i < acc.length) acc[i] = System.nanoTime() - t0;
					c.close();
				}
			} catch (IOException ignored) {}
		});
		t.setDaemon(true); t.start();
		for (int i = 0; i < 50; i++) SocketChannel.open(addr).close(); // warm-up
		long[] open = new long[n], close = new long[n];
		for (int i = 0; i < n; i++) {
			long t0 = System.nanoTime();
			SocketChannel c = SocketChannel.open(addr);
			long t1 = System.nanoTime();
			c.close();
			long t2 = System.nanoTime();
			open[i] = t1 - t0; close[i] = t2 - t1;
			Thread.sleep(0, 200_000); // let the acceptor keep pace
		}
		p("open (connect)", open); p("close", close);
		long[] acc2 = java.util.Arrays.copyOfRange(acc, 50, 50 + n); p("server accept wait", acc2);
		System.exit(0);
	}
	static void p(String k, long[] x) {
		long[] s = x.clone(); java.util.Arrays.sort(s);
		System.out.printf(Locale.ROOT, "%-20s p50=%8.1f  p90=%8.1f  p99=%8.1f  max=%8.1f us%n", k,
				s[s.length / 2] / 1e3, s[s.length * 9 / 10] / 1e3, s[s.length * 99 / 100] / 1e3, s[s.length - 1] / 1e3);
	}
}
