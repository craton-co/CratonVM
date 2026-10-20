import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLEngine;
import javax.net.ssl.SSLParameters;

/**
 * gc-common w12-a: churn SSLEngines that die without a handshake, the shape of
 * a TLS server whose connections close early. Each engine gets an ALPN
 * selector that captures the engine itself (netty's JdkAlpnSslEngine lambda
 * does), an SSLParameters round trip with an ALPN list, a getSession(), and is
 * closed. Every row the VM filed for it (engine state, cached session,
 * selector, parameters list) must go once it dies: the "used" column should
 * stay flat after the first report, on every backend and on HotSpot.
 *
 * Ends on its own. Arg 0: engine count (default 20000).
 */
public final class SslEngineChurnProbe {
	public static void main(String[] a) throws Exception {
		int n = a.length > 0 ? Integer.parseInt(a[0]) : 20000;
		SSLContext ctx = SSLContext.getInstance("TLS");
		ctx.init(null, null, null);
		Runtime rt = Runtime.getRuntime();
		long first = -1, last = -1;
		int alpnSeen = 0;
		for (int i = 1; i <= n; i++) {
			SSLEngine e = ctx.createSSLEngine("peer" + (i % 7) + ".example", 443);
			e.setUseClientMode(false);
			final SSLEngine self = e;
			e.setHandshakeApplicationProtocolSelector((eng, offered) -> eng == self ? "h2" : null);
			SSLParameters p = e.getSSLParameters();
			p.setApplicationProtocols(new String[] {"h2", "http/1.1"});
			e.setSSLParameters(p);
			if (e.getSSLParameters().getApplicationProtocols().length == 2) alpnSeen++;
			if (e.getSession() == null) throw new AssertionError("null session");
			e.closeOutbound();
			e.closeInbound();
			if (i % 5000 == 0) {
				System.gc();
				System.gc();
				long used = rt.totalMemory() - rt.freeMemory();
				if (first < 0) first = used;
				last = used;
				System.out.println("PROBE sslengine-churn i=" + i + " usedKB=" + used / 1024);
			}
		}
		// A fresh parameters object never configured answers the empty list.
		int fresh = new SSLParameters().getApplicationProtocols().length;
		System.out.println("PROBE sslengine-churn done n=" + n + " alpnSeen=" + alpnSeen
				+ " freshAlpn=" + fresh + " firstKB=" + first / 1024 + " lastKB=" + last / 1024);
	}
}
