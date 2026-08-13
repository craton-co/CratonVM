package org.apache.tomcat.websocket;

import java.net.URI;
import java.util.Collections;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

import jakarta.websocket.ClientEndpointConfig;
import jakarta.websocket.ClientEndpointConfig.Configurator;
import jakarta.websocket.ContainerProvider;
import jakarta.websocket.Session;
import jakarta.websocket.WebSocketContainer;

import org.junit.Test;

import org.apache.catalina.Context;
import org.apache.catalina.servlets.DefaultServlet;
import org.apache.catalina.startup.Tomcat;
import org.apache.tomcat.websocket.TesterMessageCountClient.BasicText;
import org.apache.tomcat.websocket.TesterMessageCountClient.TesterProgrammaticEndpoint;

/**
 * Instrumented clone of TestWebSocketFrameClient#testConnectToServerEndpoint.
 * Prints the running message count so a slow-but-progressing receive can be
 * told apart from one that stops dead.
 */
public class FirehoseProbe extends WebSocketBaseTest {

    @Test
    public void probe() throws Exception {
        Tomcat tomcat = getTomcatInstance();
        Context ctx = getProgrammaticRootContext();
        ctx.addApplicationListener(TesterFirehoseServer.ConfigInline.class.getName());
        Tomcat.addServlet(ctx, "default", new DefaultServlet());
        ctx.addServletMappingDecoded("/", "default");

        tomcat.start();

        WebSocketContainer wsContainer = ContainerProvider.getWebSocketContainer();

        ClientEndpointConfig clientEndpointConfig = ClientEndpointConfig.Builder.create()
                .configurator(new Configurator() {
                    @Override
                    public void beforeRequest(Map<String, List<String>> headers) {
                        headers.put("Dummy",
                                Collections.singletonList(String.join("", Collections.nCopies(4000, "A"))));
                        super.beforeRequest(headers);
                    }
                }).build();

        Session wsSession = wsContainer.connectToServer(TesterProgrammaticEndpoint.class, clientEndpointConfig,
                new URI("ws://localhost:" + getPort() + TesterFirehoseServer.PATH));
        CountDownLatch latch = new CountDownLatch(TesterFirehoseServer.MESSAGE_COUNT);
        BasicText handler = new BasicText(latch, TesterFirehoseServer.MESSAGE);
        wsSession.addMessageHandler(handler);

        final long t0 = System.nanoTime();
        Thread mon = new Thread(() -> {
            int last = 0;
            long lastT = t0;
            try {
                while (true) {
                    Thread.sleep(2000);
                    int now = handler.getMessageCount();
                    long t = System.nanoTime();
                    System.out.printf("[probe] t=%.1fs count=%d delta=%d rate=%.0f msg/s open=%s err=%d%n",
                            (t - t0) / 1e9, now, now - last,
                            (now - last) / ((t - lastT) / 1e9),
                            Boolean.valueOf(wsSession.isOpen()),
                            Integer.valueOf(TesterFirehoseServer.Endpoint.getErrorCount()));
                    System.out.flush();
                    last = now;
                    lastT = t;
                }
            } catch (InterruptedException e) {
                // done
            }
        });
        mon.setDaemon(true);
        mon.start();

        wsSession.getBasicRemote().sendText("Hello");
        System.out.println("[probe] Sent Hello message, waiting for data");

        boolean done = handler.getLatch().await(TesterFirehoseServer.WAIT_TIME_MILLIS, TimeUnit.MILLISECONDS);
        mon.interrupt();
        System.out.printf("[probe] DONE latch=%s count=%d wall=%.1fs open=%s errors=%d%n",
                Boolean.valueOf(done), Integer.valueOf(handler.getMessageCount()),
                (System.nanoTime() - t0) / 1e9, Boolean.valueOf(wsSession.isOpen()),
                Integer.valueOf(TesterFirehoseServer.Endpoint.getErrorCount()));
        System.out.flush();
    }
}
