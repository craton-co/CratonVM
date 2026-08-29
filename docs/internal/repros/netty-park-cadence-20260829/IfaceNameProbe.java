import java.net.Inet6Address;
import java.net.InetAddress;
import java.net.NetworkInterface;
import java.util.Collections;

public final class IfaceNameProbe {
    public static void main(String[] a) throws Exception {
        for (NetworkInterface ni : Collections.list(NetworkInterface.getNetworkInterfaces())) {
            System.out.printf("idx=%-3d name=%-14s display=%s%n", ni.getIndex(), ni.getName(), ni.getDisplayName());
            for (InetAddress ip : Collections.list(ni.getInetAddresses())) {
                String s = ip.getHostAddress();
                String sc = "";
                if (ip instanceof Inet6Address) {
                    Inet6Address v6 = (Inet6Address) ip;
                    sc = " scopeId=" + v6.getScopeId()
                       + " scopedIface=" + (v6.getScopedInterface() == null ? "null" : v6.getScopedInterface().getName());
                }
                System.out.printf("        %s%s%n", s, sc);
            }
        }
    }
}
