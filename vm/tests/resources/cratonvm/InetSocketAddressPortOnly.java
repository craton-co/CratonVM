package cratonvm;

import java.net.InetAddress;
import java.net.InetSocketAddress;

public final class InetSocketAddressPortOnly {

    public static void main(String[] args) {
        InetSocketAddress socketAddress = new InetSocketAddress(0);
        InetAddress address = socketAddress.getAddress();
        if (address == null) {
            throw new AssertionError("port-only InetSocketAddress has null address");
        }
        if (socketAddress.isUnresolved()) {
            throw new AssertionError("port-only InetSocketAddress is unresolved");
        }
        if (!address.isAnyLocalAddress()) {
            throw new AssertionError("port-only InetSocketAddress is not wildcard: " + address);
        }
        System.out.println("INET_SOCKET_ADDRESS_PORT_ONLY_OK " + address.getHostAddress());
    }
}
