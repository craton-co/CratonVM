import java.nio.ByteBuffer;
import javax.net.ssl.SSLEngine;
import javax.net.ssl.SSLEngineResult;
import javax.net.ssl.SSLEngineResult.HandshakeStatus;
import javax.net.ssl.SSLEngineResult.Status;
import javax.net.ssl.SSLSession;

/**
 * An {@code SSLEngine} subclass that overrides only the abstract four-argument {@code wrap} /
 * {@code unwrap} (Tomcat's {@code OpenSSLEngine} is exactly this) must be reached through the
 * JDK's concrete {@code wrap(ByteBuffer, ByteBuffer)} / {@code unwrap(ByteBuffer, ByteBuffer)},
 * which are one-line delegations to those overrides.
 *
 * <p>Found by Tomcat {@code TestTlsChannel} under {@code --fast-boot}: the carrier's
 * {@code SSLEngine.wrap(ByteBuffer, ByteBuffer)} native shadowed that concrete JDK body for every
 * receiver, answered a status-less {@code SSLEngineResult} and never called the override, so the
 * TLS handshake never produced a byte.
 */
public class RSslEngineSubclassWrap {

    /** Answers a distinctive result so a call that never reached the override is visible. */
    static final class Probe extends SSLEngine {
        @Override
        public SSLEngineResult wrap(ByteBuffer[] srcs, int offset, int length, ByteBuffer dst) {
            return new SSLEngineResult(Status.OK, HandshakeStatus.NEED_UNWRAP, 7, 11);
        }

        @Override
        public SSLEngineResult unwrap(ByteBuffer src, ByteBuffer[] dsts, int offset, int length) {
            return new SSLEngineResult(Status.BUFFER_UNDERFLOW, HandshakeStatus.NEED_WRAP, 3, 5);
        }

        @Override public Runnable getDelegatedTask() { return null; }
        @Override public void closeInbound() { }
        @Override public boolean isInboundDone() { return false; }
        @Override public void closeOutbound() { }
        @Override public boolean isOutboundDone() { return false; }
        @Override public String[] getSupportedCipherSuites() { return new String[0]; }
        @Override public String[] getEnabledCipherSuites() { return new String[0]; }
        @Override public void setEnabledCipherSuites(String[] suites) { }
        @Override public String[] getSupportedProtocols() { return new String[0]; }
        @Override public String[] getEnabledProtocols() { return new String[0]; }
        @Override public void setEnabledProtocols(String[] protocols) { }
        @Override public SSLSession getSession() { return null; }
        @Override public void beginHandshake() { }
        @Override public HandshakeStatus getHandshakeStatus() { return HandshakeStatus.NOT_HANDSHAKING; }
        @Override public void setUseClientMode(boolean mode) { }
        @Override public boolean getUseClientMode() { return true; }
        @Override public void setNeedClientAuth(boolean need) { }
        @Override public boolean getNeedClientAuth() { return false; }
        @Override public void setWantClientAuth(boolean want) { }
        @Override public boolean getWantClientAuth() { return false; }
        @Override public void setEnableSessionCreation(boolean flag) { }
        @Override public boolean getEnableSessionCreation() { return true; }
    }

    private static String show(SSLEngineResult r) {
        return r.getStatus() + "/" + r.getHandshakeStatus() + "/" + r.bytesConsumed() + "/" + r.bytesProduced();
    }

    public static void main(String[] args) throws Exception {
        SSLEngine e = new Probe();
        ByteBuffer b = ByteBuffer.allocate(16);
        ByteBuffer[] bs = {b};
        int failures = 0;
        String[][] rows = {
            {"wrap(BB,BB)", show(e.wrap(b, b)), "OK/NEED_UNWRAP/7/11"},
            {"wrap(BB[],BB)", show(e.wrap(bs, b)), "OK/NEED_UNWRAP/7/11"},
            {"unwrap(BB,BB)", show(e.unwrap(b, b)), "BUFFER_UNDERFLOW/NEED_WRAP/3/5"},
            {"unwrap(BB,BB[])", show(e.unwrap(b, bs)), "BUFFER_UNDERFLOW/NEED_WRAP/3/5"},
        };
        for (String[] row : rows) {
            boolean ok = row[1].equals(row[2]);
            if (!ok) failures++;
            System.out.println(row[0] + " " + row[1] + (ok ? "" : " EXPECTED " + row[2]));
        }
        System.out.println("CK RSslEngineSubclassWrap checks=" + rows.length + " failures=" + failures);
        if (failures != 0) System.exit(1);
    }
}
