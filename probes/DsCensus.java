import java.net.*;
import java.util.*;
import java.util.function.*;

public class DsCensus {
    static void t(String label, Callable c) {
        try { System.out.println("OK    " + label + " -> " + c.call()); }
        catch (Throwable e) { System.out.println("THROW " + label + " -> " + e.getClass().getName() + ": " + e.getMessage()); }
    }
    interface Callable { Object call() throws Exception; }
    public static void main(String[] a) throws Exception {
        DatagramSocket s = new DatagramSocket();
        t("isBound", s::isBound);
        t("getLocalPort", s::getLocalPort);
        t("getLocalAddress", s::getLocalAddress);
        t("getLocalSocketAddress", s::getLocalSocketAddress);
        t("getInetAddress", s::getInetAddress);
        t("getPort", s::getPort);
        t("getRemoteSocketAddress", s::getRemoteSocketAddress);
        t("getChannel", s::getChannel);
        t("getSendBufferSize", s::getSendBufferSize);
        t("getReceiveBufferSize", s::getReceiveBufferSize);
        t("getTrafficClass", s::getTrafficClass);
        t("getSoTimeout", s::getSoTimeout);
        t("getReuseAddress", s::getReuseAddress);
        t("getBroadcast", s::getBroadcast);
        t("supportedOptions", s::supportedOptions);
        t("setSendBufferSize", () -> { s.setSendBufferSize(65536); return "ok"; });
        t("setReceiveBufferSize", () -> { s.setReceiveBufferSize(65536); return "ok"; });
        t("setTrafficClass", () -> { s.setTrafficClass(0); return "ok"; });
        t("getOption(SO_RCVBUF)", () -> s.getOption(StandardSocketOptions.SO_RCVBUF));
        t("connect(SocketAddress)", () -> { s.connect(new InetSocketAddress("127.0.0.1", 9)); return "ok"; });
        t("after connect getRemoteSocketAddress", s::getRemoteSocketAddress);
        t("after connect getInetAddress", s::getInetAddress);
        t("disconnect", () -> { s.disconnect(); return "ok"; });
        s.close();
        t("closed getLocalSocketAddress", s::getLocalSocketAddress);

        System.out.println("--- unbound ctor ---");
        DatagramSocket u = new DatagramSocket(null);
        t("u.isBound", u::isBound);
        t("u.bind(127.0.0.1:0)", () -> { u.bind(new InetSocketAddress("127.0.0.1", 0)); return "ok"; });
        t("u.isBound after", u::isBound);
        t("u.getLocalSocketAddress", u::getLocalSocketAddress);
        u.close();

        System.out.println("--- ctor(SocketAddress) ---");
        DatagramSocket v = new DatagramSocket(new InetSocketAddress("127.0.0.1", 0));
        t("v.getLocalSocketAddress", v::getLocalSocketAddress);
        t("v.getLocalAddress", v::getLocalAddress);

        System.out.println("--- send/receive roundtrip ---");
        DatagramSocket w = new DatagramSocket();
        byte[] msg = "ping".getBytes();
        t("w.send", () -> { w.send(new DatagramPacket(msg, msg.length, (InetSocketAddress) v.getLocalSocketAddress())); return "ok"; });
        t("v.receive", () -> { byte[] b = new byte[16]; DatagramPacket p = new DatagramPacket(b, 16); v.setSoTimeout(2000); v.receive(p); return p.getLength() + " bytes from " + p.getSocketAddress(); });
        v.close(); w.close();
    }
}
