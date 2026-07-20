import org.eclipse.jetty.client.ContentResponse;
import org.eclipse.jetty.client.StringRequestContent;
import org.eclipse.jetty.http2.client.HTTP2Client;
import org.eclipse.jetty.http2.client.transport.HttpClientTransportOverHTTP2;
import reactor.netty.DisposableServer;
import reactor.netty.http.Http2SettingsSpec;
import reactor.netty.http.server.HttpServer;

public class H2cRepro {
    public static void main(String[] args) throws Exception {
        if (args.length > 0) {
            System.setProperty("io.netty.allocator.type", args[0]);
        }
        System.out.println("io.netty.allocator.type=" + System.getProperty("io.netty.allocator.type"));
        HttpServer httpServer = HttpServer.create()
                .port(0)
                .protocol(reactor.netty.http.HttpProtocol.HTTP11, reactor.netty.http.HttpProtocol.H2C)
                .handle((req, res) -> res.sendString(req.receive().aggregate().asString()));
        DisposableServer server = httpServer.bindNow();
        System.out.println("bound on port " + server.port());

        try (org.eclipse.jetty.client.HttpClient client = new org.eclipse.jetty.client.HttpClient(
                new HttpClientTransportOverHTTP2(new HTTP2Client()))) {
            client.start();
            ContentResponse response = client.POST("http://localhost:" + server.port())
                    .body(new StringRequestContent("text/plain", "Hello World"))
                    .send();
            System.out.println("status=" + response.getStatus());
            System.out.println("body=" + response.getContentAsString());
        } catch (Exception ex) {
            System.out.println("FAILED: " + ex);
            ex.printStackTrace();
        }
        server.disposeNow();
    }
}
