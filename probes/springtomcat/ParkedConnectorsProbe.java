package org.springframework.boot.tomcat;

import java.util.Map;

import org.apache.catalina.Service;
import org.apache.catalina.connector.Connector;

import org.springframework.boot.tomcat.servlet.TomcatServletWebServerFactory;

/**
 * Paired probe for Spring Boot's park-and-restore of embedded Tomcat
 * connectors, diffed against the host JDK.
 *
 * `TomcatWebServer.initialize()` removes every connector from every Service and
 * parks them in a `Map<Service, Connector[]>` (`removeServiceConnectors`), so
 * that nothing binds a port until `start()` is called. `start()` then calls
 * `addPreviouslyRemovedConnectors()` to put them back.
 *
 * If the restore does not happen, `Tomcat.getConnector()` — which Spring calls
 * immediately afterwards — does not return null. It *fabricates* a connector on
 * port 8080 and adds it to the Service. So a lost park/restore does not look
 * like "no server"; it looks like "a server on the wrong, fixed port", which
 * then collides with anything already on 8080.
 *
 * Observed: HotSpot restores the port-0 connector and binds an ephemeral port;
 * CratonVM starts `http-nio-8080` instead and fails. `Service` identity,
 * `HashMap` round-trips, `findConnectors()`, `Connector.setPort` and
 * identity-hash stability across GC were each checked in isolation and are
 * identical between the two VMs — so this reads the real parked map on the real
 * object instead of another synthetic stand-in.
 *
 * Lives in `org.springframework.boot.tomcat` because `getServiceConnectors()`
 * is package-private. That is the point: it is the only way to see the parked
 * state without guessing at it.
 */
public class ParkedConnectorsProbe {

    static String describe(Connector[] cs) {
        if (cs == null) {
            return "null";
        }
        StringBuilder sb = new StringBuilder("len=").append(cs.length).append(" [");
        for (int i = 0; i < cs.length; i++) {
            if (i > 0) {
                sb.append(", ");
            }
            sb.append(cs[i].getPort()).append('@').append(System.identityHashCode(cs[i]));
        }
        return sb.append(']').toString();
    }

    public static void main(String[] args) throws Exception {
        TomcatServletWebServerFactory factory = new TomcatServletWebServerFactory(0);
        TomcatWebServer ws = (TomcatWebServer) factory.getWebServer();

        org.apache.catalina.startup.Tomcat tomcatField = ws.getTomcat();
        Service[] services = tomcatField.getServer().findServices();
        Map<Service, Connector[]> parked = ws.getServiceConnectors();

        System.out.println("services.length           = " + services.length);
        System.out.println("parked map size           = " + parked.size());
        for (Service s : services) {
            System.out.println("  service id              = " + System.identityHashCode(s));
            System.out.println("  live findConnectors()   = " + describe(s.findConnectors()));
            System.out.println("  parked.get(service)     = " + describe(parked.get(s)));
            System.out.println("  parked.containsKey      = " + parked.containsKey(s));
        }
        for (Map.Entry<Service, Connector[]> e : parked.entrySet()) {
            Service k = e.getKey();
            System.out.println("  entry key id            = " + System.identityHashCode(k)
                    + " value " + describe(e.getValue()));
            // The three questions that separate "wrong key" from "right key,
            // wrong bucket". If the key IS the object and equals() agrees, then
            // a failed containsKey means the map hashed it to one bucket at
            // put() time and to another at get() time -- i.e. hashCode() did
            // not stay constant for the lifetime of the entry, which is the one
            // thing HashMap is entitled to assume.
            System.out.println("    key == service        = " + (k == services[0]));
            System.out.println("    key.equals(service)   = " + k.equals(services[0]));
            System.out.println("    key.hashCode()        = " + k.hashCode());
            System.out.println("    service.hashCode()    = " + services[0].hashCode());
            System.out.println("    identityHashCode(key) = " + System.identityHashCode(k));
            System.out.println("    re-get by entry key   = " + describe(parked.get(k)));
        }

        try {
            ws.start();
            System.out.println("started port              = " + ws.getPort());
        }
        catch (Throwable t) {
            System.out.println("start threw               = " + t.getClass().getName()
                    + ": " + t.getMessage());
        }
        System.out.println("after start connectors    = " + describe(services[0].findConnectors()));
        try {
            ws.stop();
        }
        catch (Throwable ignored) {
            // best effort
        }
    }
}
