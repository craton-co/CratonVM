import java.net.*;
import java.util.*;

/**
 * Ground truth for `InetAddress.getByAddress`: does the returned address
 * REMEMBER a host name, and does it keep HotSpot's uncompressed IPv6 text?
 *
 * Written to settle a disagreement between a `native-builtins` unit test
 * (`re3_get_by_address_uses_hotspot_ipv6_text_and_concrete_layout`, which
 * asserts the holder's hostName equals the address text) and the fix that
 * deliberately stopped recording one (`e092b0f3b`). One of the two is wrong,
 * and neither the test's comment nor the commit message is evidence.
 *
 * `InetAddress.toString()` is
 * `Objects.toString(holder().getHostName(), "") + "/" + getHostAddress()`,
 * so the STORED name is directly observable without reflection: an empty
 * left-hand side means the holder's hostName is null.
 *
 * Order matters. `getHostName()` performs a reverse lookup and HotSpot caches
 * the answer into the holder, which changes what a later `toString()` prints —
 * so every section prints `toString()` BEFORE calling `getHostName()`, then
 * prints `toString()` again to show whether the call mutated the object.
 */
public class InetGetByAddressProbe {

    static final byte[] V6 = {
        (byte) 0xfe, (byte) 0x80, 0, 0, 0, 0, 0, 0,
        (byte) 0x67, (byte) 0xb0, 0x09, (byte) 0x9e, 0x5a, (byte) 0x9b, 0x28, 0x7e,
    };
    static final byte[] V4 = {10, 1, 2, 3};

    public static void main(String[] args) throws Exception {
        section("v6-unnamed", InetAddress.getByAddress(V6));
        section("v4-unnamed", InetAddress.getByAddress(V4));
        section("v6-named", InetAddress.getByAddress("example.invalid", V6));
        section("v4-named", InetAddress.getByAddress("example.invalid", V4));
        // The paired control the fix's message calls out: a numeric literal
        // through getByName has no name either, while the wildcard singleton
        // genuinely carries one.
        section("v4-byname-literal", InetAddress.getByName("10.1.2.3"));
        section("v6-byname-literal", InetAddress.getByName("fe80::67b0:99e:5a9b:287e"));

        // Error behaviour: the JDK declares UnknownHostException for a length
        // that is neither 4 nor 16.
        System.out.println("badlen thrown=" + thrown(() -> {
            try {
                InetAddress.getByAddress(new byte[3]);
            } catch (UnknownHostException e) {
                throw new RuntimeException(e);
            }
        }));
        System.out.println("GETBYADDR done");
    }

    static void section(String tag, InetAddress a) {
        // Before any name lookup: this is the constructed state.
        System.out.println(tag + " toString=" + a);
        System.out.println(tag + " class=" + a.getClass().getName());
        System.out.println(tag + " hostAddress=" + a.getHostAddress());
        System.out.println(tag + " isAnyLocal=" + a.isAnyLocalAddress()
                + " isLinkLocal=" + a.isLinkLocalAddress());
        System.out.println(tag + " addressLen=" + a.getAddress().length);
        // getHostName() may do a reverse lookup; print what it answers and
        // whether the object changed afterwards.
        System.out.println(tag + " getHostName=" + a.getHostName());
        System.out.println(tag + " toStringAfterGetHostName=" + a);
    }

    static String thrown(Runnable body) {
        try {
            body.run();
            return "none";
        } catch (Throwable t) {
            Throwable root = t;
            while (root.getClass() == RuntimeException.class && root.getCause() != null) {
                root = root.getCause();
            }
            return root.getClass().getName();
        }
    }
}
