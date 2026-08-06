/*
 * SelectorEintrProbe — does `Selector.select(timeout)` honour its timeout when
 * the thread is hit by a signal?
 *
 * `epoll_wait` (and `poll`) return -1/EINTR when a signal is delivered, and
 * that is NOT a select result: the JDK's `EPollSelectorImpl.doSelect` re-enters
 * the syscall with the time that is LEFT and only reports 0 once the caller's
 * own deadline has passed. An implementation that instead returns 0 to Java
 * turns every signal into a spurious wakeup.
 *
 * Netty notices. `NioIoHandler` counts selects that returned no keys BEFORE the
 * timeout elapsed and, after 512 in a row, logs
 *
 *   Selector.select() returned prematurely 512 times in a row; rebuilding Selector
 *
 * and rebuilds the selector — which is what the 2026-08-05 Azure full-suite log
 * shows 268 times for `ReactorClientHttpRequestFactoryBuilderTests`.
 *
 * The probe registers a connected socket that will never become readable, so a
 * correct `select(timeout)` blocks for the whole timeout every time. Drive it
 * while something signals the process (see `scripts/selector-eintr-probe.sh`)
 * and count the calls that came back early.
 *
 * Exit code 1 when any call returned early, so a harness can score it.
 */
import java.net.InetSocketAddress;
import java.nio.channels.SelectionKey;
import java.nio.channels.Selector;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;

public final class SelectorEintrProbe {

	public static void main(String[] args) throws Exception {
		int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 20;
		long timeoutMs = args.length > 1 ? Long.parseLong(args[1]) : 1000L;

		ServerSocketChannel listener = ServerSocketChannel.open();
		listener.bind(new InetSocketAddress("127.0.0.1", 0));
		SocketChannel client = SocketChannel.open(listener.getLocalAddress());
		SocketChannel accepted = listener.accept();
		client.configureBlocking(false);
		Selector selector = Selector.open();
		client.register(selector, SelectionKey.OP_READ);

		// Nothing is ever written to `client`, so OP_READ can never fire and
		// every select must wait out its full timeout.
		System.out.println("SELECT_EINTR_PROBE ready iterations=" + iterations
				+ " timeoutMs=" + timeoutMs);
		System.out.flush();

		int premature = 0;
		long minElapsed = Long.MAX_VALUE;
		long totalElapsed = 0;
		for (int i = 0; i < iterations; i++) {
			long t0 = System.nanoTime();
			int n = selector.select(timeoutMs);
			long ms = (System.nanoTime() - t0) / 1_000_000L;
			selector.selectedKeys().clear();
			minElapsed = Math.min(minElapsed, ms);
			totalElapsed += ms;
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

		System.out.println("SELECT_EINTR_PROBE_RESULT iterations=" + iterations
				+ " timeoutMs=" + timeoutMs
				+ " premature=" + premature
				+ " minElapsedMs=" + minElapsed
				+ " avgElapsedMs=" + (totalElapsed / iterations));
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
