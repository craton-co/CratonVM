import java.net.*;
import java.util.*;

public class NetworkInterfaceProbe {
    static void p(String k, Object v) { System.out.println(k + "=" + v); }

    public static void main(String[] a) throws Exception {
        Enumeration<NetworkInterface> e = null;
        try {
            e = NetworkInterface.getNetworkInterfaces();
        } catch (Throwable t) {
            p("enum.throw", t.getClass().getName() + ":" + t.getMessage());
        }
        p("enum.null", e == null);
        int n = 0;
        List<NetworkInterface> all = new ArrayList<>();
        if (e != null) {
            while (e.hasMoreElements() && n < 40) { all.add(e.nextElement()); n++; }
        }
        p("enum.count", n);
        // Sort by name so enumeration ORDER (a known, separate divergence)
        // does not shuffle every row of the diff.
        all.sort(Comparator.comparing(NetworkInterface::getName));
        for (NetworkInterface nif : all) {
            String name = nif.getName();
            String up, lo, idx;
            try { up = String.valueOf(nif.isUp()); } catch (Throwable t) { up = "THROW:" + t.getClass().getSimpleName(); }
            try { lo = String.valueOf(nif.isLoopback()); } catch (Throwable t) { lo = "THROW:" + t.getClass().getSimpleName(); }
            try { idx = String.valueOf(nif.getIndex()); } catch (Throwable t) { idx = "THROW:" + t.getClass().getSimpleName(); }
            p("nif." + name + ".flags", "up=" + up + " loop=" + lo + " idx=" + idx);
            try {
                List<String> rows = new ArrayList<>();
                for (Enumeration<InetAddress> ae = nif.getInetAddresses(); ae.hasMoreElements(); ) {
                    InetAddress ia = ae.nextElement();
                    String scope = (ia instanceof Inet6Address)
                        ? String.valueOf(((Inet6Address) ia).getScopeId()) : "-";
                    String sif = (ia instanceof Inet6Address && ((Inet6Address) ia).getScopedInterface() != null)
                        ? ((Inet6Address) ia).getScopedInterface().getName() : "-";
                    // toString BEFORE getHostName: getHostName mutates the holder.
                    rows.add(ia.getHostAddress() + " ts=" + ia.toString()
                             + " scopeId=" + scope + " scopeIf=" + sif
                             + " len=" + ia.getAddress().length);
                }
                Collections.sort(rows);
                for (int i = 0; i < rows.size(); i++) p("nif." + name + ".addr" + i, rows.get(i));
            } catch (Throwable t) { p("nif." + name + ".addr.throw", t.getClass().getSimpleName()); }
        }
        try {
            NetworkInterface byName = NetworkInterface.getByName("lo");
            p("byName.lo", byName == null ? "null" : byName.getName() + " idx=" + byName.getIndex()
                          + " up=" + byName.isUp() + " loop=" + byName.isLoopback());
        } catch (Throwable t) { p("byName.lo.throw", t.getClass().getName()); }
        try {
            NetworkInterface byIdx = NetworkInterface.getByIndex(1);
            p("byIndex.1", byIdx == null ? "null" : byIdx.getName());
        } catch (Throwable t) { p("byIndex.1.throw", t.getClass().getName()); }

        // The "find a usable interface" shape that isUp()==false breaks.
        NetworkInterface firstUsable = null;
        for (NetworkInterface nif : all) {
            if (!nif.isLoopback() && nif.isUp()) { firstUsable = nif; break; }
        }
        p("firstUsable", firstUsable == null ? "none" : firstUsable.getName());

        // Scope by NetworkInterface — the holder6.scope_ifname path.
        byte[] linkLocal = {
            (byte) 0xfe, (byte) 0x80, '0', '0', '0', '0', '0', '0',
            '0', '0', '0', '0', '0', '0', '0', '1'
        };
        if (firstUsable != null) {
            try {
                Inet6Address byIf = Inet6Address.getByAddress(null, linkLocal, firstUsable);
                p("byIf.hostAddress", byIf.getHostAddress());
                p("byIf.scopeId", byIf.getScopeId());
                p("byIf.scopeIfName", byIf.getScopedInterface() == null ? "null"
                                      : byIf.getScopedInterface().getName());
            } catch (Throwable t) { p("byIf.throw", t.getClass().getName() + ":" + t.getMessage()); }
        }
        System.out.println("NIF-DONE");
    }
}
