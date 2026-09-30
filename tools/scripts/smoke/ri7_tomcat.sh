#!/usr/bin/env bash
# RI.7 — Tomcat 10 Embed: respond to GET /

source "$(dirname "$0")/common.sh"

MVN_CENTRAL="https://repo1.maven.org/maven2"
V=10.1.24
EMBED_JAR="$FIXTURE_CACHE/tomcat-embed-core-$V.jar"
SERVLET_JAR="$FIXTURE_CACHE/tomcat-annotations-api-$V.jar"
FIX_DIR="$FIXTURE_CACHE/fixture"
mkdir -p "$FIX_DIR"

smoke_download "$MVN_CENTRAL/org/apache/tomcat/embed/tomcat-embed-core/$V/tomcat-embed-core-$V.jar" "$EMBED_JAR"
smoke_download "$MVN_CENTRAL/org/apache/tomcat/tomcat-annotations-api/$V/tomcat-annotations-api-$V.jar" "$SERVLET_JAR"

cat > "$FIX_DIR/TomcatSmoke.java" <<'JAVA'
import java.io.*;
import java.net.*;
import jakarta.servlet.http.*;
import org.apache.catalina.Context;
import org.apache.catalina.startup.Tomcat;

public class TomcatSmoke {
    public static void main(String[] args) throws Exception {
        Tomcat t = new Tomcat();
        t.setBaseDir(System.getProperty("java.io.tmpdir"));
        t.setPort(0);
        Context ctx = t.addContext("", null);
        Tomcat.addServlet(ctx, "ok", new HttpServlet() {
            protected void doGet(HttpServletRequest req, HttpServletResponse resp) throws IOException {
                resp.getWriter().write("TOMCAT_SMOKE_OK");
            }
        });
        ctx.addServletMappingDecoded("/", "ok");
        t.getConnector();
        t.start();
        int port = t.getConnector().getLocalPort();
        URL url = new URL("http://127.0.0.1:" + port + "/");
        HttpURLConnection c = (HttpURLConnection) url.openConnection();
        try (BufferedReader r = new BufferedReader(new InputStreamReader(c.getInputStream()))) {
            System.out.println("TOMCAT_BODY=" + r.readLine());
        }
        t.stop();
    }
}
JAVA

"$JAVA_HOME_FOR_SMOKE/bin/javac" -cp "$EMBED_JAR:$SERVLET_JAR" -d "$FIX_DIR" "$FIX_DIR/TomcatSmoke.java"

SMOKE_TIMEOUT=300 smoke_run_cratonvm \
    --Xmx 1g \
    --classpath "$FIX_DIR:$EMBED_JAR:$SERVLET_JAR" \
    -- TomcatSmoke

smoke_require_signal "TOMCAT_BODY=TOMCAT_SMOKE_OK"
