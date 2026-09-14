#!/usr/bin/env bash
# RI.8 — Jetty 11 Embedded: respond to GET /

source "$(dirname "$0")/common.sh"

MVN_CENTRAL="https://repo1.maven.org/maven2"
V=11.0.20
JETTY_SERVER="$FIXTURE_CACHE/jetty-server-$V.jar"
JETTY_HTTP="$FIXTURE_CACHE/jetty-http-$V.jar"
JETTY_IO="$FIXTURE_CACHE/jetty-io-$V.jar"
JETTY_UTIL="$FIXTURE_CACHE/jetty-util-$V.jar"
SERVLET="$FIXTURE_CACHE/jakarta.servlet-api-5.0.0.jar"
FIX_DIR="$FIXTURE_CACHE/fixture"
mkdir -p "$FIX_DIR"

smoke_download "$MVN_CENTRAL/org/eclipse/jetty/jetty-server/$V/jetty-server-$V.jar" "$JETTY_SERVER"
smoke_download "$MVN_CENTRAL/org/eclipse/jetty/jetty-http/$V/jetty-http-$V.jar" "$JETTY_HTTP"
smoke_download "$MVN_CENTRAL/org/eclipse/jetty/jetty-io/$V/jetty-io-$V.jar" "$JETTY_IO"
smoke_download "$MVN_CENTRAL/org/eclipse/jetty/jetty-util/$V/jetty-util-$V.jar" "$JETTY_UTIL"
smoke_download "$MVN_CENTRAL/jakarta/servlet/jakarta.servlet-api/5.0.0/jakarta.servlet-api-5.0.0.jar" "$SERVLET"

cat > "$FIX_DIR/JettySmoke.java" <<'JAVA'
import java.io.*;
import java.net.*;
import jakarta.servlet.http.*;
import org.eclipse.jetty.server.*;
import org.eclipse.jetty.server.handler.AbstractHandler;

public class JettySmoke {
    public static void main(String[] args) throws Exception {
        Server srv = new Server(0);
        srv.setHandler(new AbstractHandler() {
            public void handle(String target, Request base, HttpServletRequest req, HttpServletResponse resp) throws IOException {
                resp.getWriter().write("JETTY_SMOKE_OK");
                base.setHandled(true);
            }
        });
        srv.start();
        int port = ((ServerConnector) srv.getConnectors()[0]).getLocalPort();
        URL url = new URL("http://127.0.0.1:" + port + "/");
        try (BufferedReader r = new BufferedReader(new InputStreamReader(url.openStream()))) {
            System.out.println("JETTY_BODY=" + r.readLine());
        }
        srv.stop();
    }
}
JAVA

CP="$FIX_DIR:$JETTY_SERVER:$JETTY_HTTP:$JETTY_IO:$JETTY_UTIL:$SERVLET"
"$JAVA_HOME_FOR_SMOKE/bin/javac" -cp "$CP" -d "$FIX_DIR" "$FIX_DIR/JettySmoke.java"

SMOKE_TIMEOUT=300 smoke_run_cratonvm --Xmx 1g --classpath "$CP" -- JettySmoke

smoke_require_signal "JETTY_BODY=JETTY_SMOKE_OK"
