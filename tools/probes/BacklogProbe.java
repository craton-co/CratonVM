import java.net.*;
import java.nio.channels.*;
import java.util.ArrayList;

/** Effective listen backlog: connect with NOBODY accepting until the first
 *  refusal (or `max` connects), holding every client open. */
public final class BacklogProbe {
	public static void main(String[] a) throws Exception {
		int backlog = Integer.parseInt(a[0]);
		int max = Integer.parseInt(a[1]);
		ServerSocketChannel ls = ServerSocketChannel.open().bind(new InetSocketAddress("127.0.0.1", 0), backlog);
		InetSocketAddress addr = (InetSocketAddress) ls.getLocalAddress();
		ArrayList<SocketChannel> held = new ArrayList<>();
		int ok = 0;
		String why = "reached max";
		for (int i = 0; i < max; i++) {
			try { held.add(SocketChannel.open(addr)); ok++; }
			catch (Exception e) { why = e.getClass().getSimpleName() + ": " + e.getMessage(); break; }
		}
		System.out.println("requested backlog=" + backlog + "  connects accepted by kernel=" + ok + "  stop=" + why);
		for (SocketChannel c : held) c.close();
		System.exit(0);
	}
}
