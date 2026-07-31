import java.lang.reflect.Field;
import java.net.ServerSocket;
import java.net.Socket;

import org.apache.hc.core5.http.io.SocketConfig;

/** What httpclient5 decides to do to a socket BEFORE connect, and whether the VM lets it. */
public class SockCfgProbe {

	public static void main(String[] args) throws Exception {
		SocketConfig cfg = SocketConfig.DEFAULT;
		System.out.println("PROBE cfg keepIdle=" + cfg.getTcpKeepIdle()
				+ " keepInterval=" + cfg.getTcpKeepInterval()
				+ " keepCount=" + cfg.getTcpKeepCount()
				+ " soKeepAlive=" + cfg.isSoKeepAlive()
				+ " rcvBuf=" + cfg.getRcvBufSize()
				+ " sndBuf=" + cfg.getSndBufSize());

		try {
			Class<?> op = Class.forName("org.apache.hc.client5.http.impl.io.DefaultHttpClientConnectionOperator");
			Field f = op.getDeclaredField("SUPPORTS_KEEPALIVE_OPTIONS");
			f.setAccessible(true);
			System.out.println("PROBE SUPPORTS_KEEPALIVE_OPTIONS=" + f.get(null));
		}
		catch (Throwable t) {
			System.out.println("PROBE SUPPORTS_KEEPALIVE_OPTIONS unknown: " + t);
		}

		// The bare VM question: option-setting on a created-but-unconnected socket.
		try (Socket s = new Socket()) {
			s.setKeepAlive(true);
			jdk.net.Sockets.setOption(s, jdk.net.ExtendedSocketOptions.TCP_KEEPIDLE, 100);
			System.out.println("PROBE preconnect setOption OK");
			try (ServerSocket ss = new ServerSocket(0)) {
				s.connect(ss.getLocalSocketAddress(), 2000);
				System.out.println("PROBE connected; keepAlive=" + s.getKeepAlive()
						+ " keepIdle=" + jdk.net.Sockets.getOption(s, jdk.net.ExtendedSocketOptions.TCP_KEEPIDLE));
			}
		}
		catch (Throwable t) {
			System.out.println("PROBE preconnect setOption THREW " + t);
		}
		System.out.println("PROBE done");
		System.exit(0);
	}
}
