/*
 * SelectorInterestNudgeProbe — does `interestOps()` make the NEXT
 * `select(timeout)` return immediately with nothing ready?
 *
 * CratonVM nudges a selector's wakeup pipe when interest ops change, because
 * `epoll_ctl(MOD)` does not reliably interrupt an already-blocked `epoll_wait`
 * (Tomcat arming OP_WRITE after a partial gathering write needs it). The nudge
 * is only meaningful while a select is IN FLIGHT: sent when no thread is parked,
 * the byte simply sits in the pipe and the next `select(timeout)` finds it
 * readable and returns at once, having selected nothing.
 *
 * Netty's event loop sets interest ops BETWEEN selects, which is precisely that
 * case. It counts selects that returned no keys before the timeout elapsed and,
 * after 512 in a row, logs
 *
 *   Selector.select() returned prematurely 512 times in a row; rebuilding Selector
 *
 * then rebuilds the selector — re-registering every channel, which sets more
 * interest ops, which queues more nudges. That is the self-sustaining storm the
 * 2026-08-05 Azure full-suite log shows 268 times for
 * `ReactorClientHttpRequestFactoryBuilderTests`.
 *
 * The probe registers a connected socket that never becomes readable, so a
 * correct `select(timeout)` blocks for the whole timeout every time — with or
 * without an `interestOps()` call in front of it.
 *
 * Exit code 1 when any call returned early, so a harness can score it.
 */
import java.net.InetSocketAddress;
import java.nio.channels.SelectionKey;
import java.nio.channels.Selector;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;

public final class SelectorInterestNudgeProbe {

	public static void main(String[] args) throws Exception {
		int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 10;
		long timeoutMs = args.length > 1 ? Long.parseLong(args[1]) : 1000L;
		// "nudge" (default) sets interestOps before each select; "plain" does
		// not, and is the control that says the harness itself is sound.
		boolean nudge = args.length <= 2 || !"plain".equals(args[2]);

		ServerSocketChannel listener = ServerSocketChannel.open();
		listener.bind(new InetSocketAddress("127.0.0.1", 0));
		SocketChannel client = SocketChannel.open(listener.getLocalAddress());
		SocketChannel accepted = listener.accept();
		client.configureBlocking(false);
		Selector selector = Selector.open();
		SelectionKey key = client.register(selector, SelectionKey.OP_READ);

		System.out.println("SELECT_PROBE ready iterations=" + iterations
				+ " timeoutMs=" + timeoutMs + " mode=" + (nudge ? "nudge" : "plain"));
		System.out.flush();

		int premature = 0;
		long minElapsed = Long.MAX_VALUE;
		long total = 0;
		for (int i = 0; i < iterations; i++) {
			if (nudge) {
				// Exactly what a Netty event loop does between selects. The
				// value is deliberately unchanged: re-arming the same ops is
				// the common case and must not perturb the next select.
				key.interestOps(SelectionKey.OP_READ);
			}
			long t0 = System.nanoTime();
			int n = selector.select(timeoutMs);
			long ms = (System.nanoTime() - t0) / 1_000_000L;
			selector.selectedKeys().clear();
			minElapsed = Math.min(minElapsed, ms);
			total += ms;
			// 10% slack: a real select may return marginally early, and a
			// loaded host adds scheduling noise in the other direction.
			boolean early = (n == 0) && ms < (timeoutMs * 9L / 10L);
			if (early) {
				premature++;
			}
			System.out.println("  select#" + i + " keys=" + n + " elapsed=" + ms + "ms"
					+ (early ? "   <-- PREMATURE" : ""));
			System.out.flush();
		}

		System.out.println("SELECT_PROBE_RESULT mode=" + (nudge ? "nudge" : "plain")
				+ " iterations=" + iterations
				+ " timeoutMs=" + timeoutMs
				+ " premature=" + premature
				+ " minElapsedMs=" + minElapsed
				+ " avgElapsedMs=" + (total / iterations));
		System.out.flush();

		selector.close();
		client.close();
		accepted.close();
		listener.close();
		if (premature > 0) {
			System.exit(1);
		}
	}
}
