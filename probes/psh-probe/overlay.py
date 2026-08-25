#!/usr/bin/env python3
"""Build an INSTRUMENTED copy of ParameterizedSslHandlerTest in the overlay dir.

The classpath puts /data/nres/overlay ahead of netty's own test-classes, so
this copy shadows the original for MY runs only — the shared fixture is not
touched and no other session sees the instrument.

Every insertion is a registration or a print. `PshProbe.await` calls exactly
the `syncUninterruptibly()` the test called, so the sequence of netty
operations is unchanged.
"""
import io, os, sys

SRC = "/data/cratonvm/apps/netty/handler/src/test/java/io/netty/handler/ssl/ParameterizedSslHandlerTest.java"
OUT = "/data/nres/src/io/netty/handler/ssl/ParameterizedSslHandlerTest.java"

s = io.open(SRC, encoding="utf-8").read()

def sub(old, new, n=1):
    global s
    c = s.count(old)
    if c != n:
        sys.exit("ANCHOR MISS (%d/%d): %r" % (c, n, old[:200]))
    s = s.replace(old, new)

# --- composite test: bracket the three waits in the body and the finally ----
sub("""            donePromise.get();
        } finally {
            if (cc != null) {
                cc.close().syncUninterruptibly();
            }
            if (sc != null) {
                sc.close().syncUninterruptibly();
            }
            group.shutdownGracefully();

            ReferenceCountUtil.release(sslServerCtx);
            ReferenceCountUtil.release(sslClientCtx);
        }
    }

    @ParameterizedTest(name = PARAMETERIZED_NAME)
    @MethodSource("data")
    @Timeout(value = 30000, unit = TimeUnit.MILLISECONDS)
    public void testAlertProducedAndSend(SslProvider clientProvider, SslProvider serverProvider) throws Exception {""",
"""            String __k = PshProbe.enter("composite.donePromise",
                    new Object[] { donePromise, cc });
            try {
                donePromise.get();
            } finally {
                PshProbe.leave(__k);
            }
        } finally {
            if (cc != null) {
                PshProbe.await("composite.cc.close", cc.close());
            }
            if (sc != null) {
                PshProbe.await("composite.sc.close", sc.close());
            }
            group.shutdownGracefully();

            ReferenceCountUtil.release(sslServerCtx);
            ReferenceCountUtil.release(sslClientCtx);
        }
    }

    @ParameterizedTest(name = PARAMETERIZED_NAME)
    @MethodSource("data")
    @Timeout(value = 30000, unit = TimeUnit.MILLISECONDS)
    public void testAlertProducedAndSend(SslProvider clientProvider, SslProvider serverProvider) throws Exception {""")

# --- alert test: what the client's exceptionCaught actually receives --------
sub("""                                @Override
                                public void exceptionCaught(ChannelHandlerContext ctx, Throwable cause) {
                                    if (cause.getCause() instanceof SSLException) {
                                        // We received the alert and so produce an SSLException.
                                        promise.trySuccess(null);
                                    }
                                }""",
"""                                @Override
                                public void exceptionCaught(ChannelHandlerContext ctx, Throwable cause) {
                                    PshProbe.log("alert.client exceptionCaught cause="
                                            + cause.getClass().getName() + " getCause="
                                            + (cause.getCause() == null
                                                ? "null" : cause.getCause().getClass().getName())
                                            + " msg=" + cause.getMessage());
                                    if (cause.getCause() instanceof SSLException) {
                                        // We received the alert and so produce an SSLException.
                                        promise.trySuccess(null);
                                    }
                                }

                                @Override
                                public void channelInactive(ChannelHandlerContext ctx) {
                                    PshProbe.log("alert.client channelInactive promiseDone="
                                            + promise.isDone());
                                    ctx.fireChannelInactive();
                                }""")

# --- alert test: the server side, which is what has to produce the alert ----
sub("""                            ch.pipeline().addLast(new ChannelInboundHandlerAdapter() {
                                @Override
                                public void exceptionCaught(ChannelHandlerContext ctx, Throwable cause) {
                                    // Just trigger a close
                                    ctx.close();
                                }
                            });""",
"""                            ch.pipeline().addLast(new ChannelInboundHandlerAdapter() {
                                @Override
                                public void exceptionCaught(ChannelHandlerContext ctx, Throwable cause) {
                                    PshProbe.log("alert.server exceptionCaught cause="
                                            + cause.getClass().getName() + " msg=" + cause.getMessage());
                                    // Just trigger a close
                                    ctx.close();
                                }

                                @Override
                                public void userEventTriggered(ChannelHandlerContext ctx, Object evt) {
                                    PshProbe.log("alert.server userEvent " + evt);
                                    ctx.fireUserEventTriggered(evt);
                                }
                            });""")

sub("""                            public void checkClientTrusted(X509Certificate[] x509Certificates, String s)
                                    throws CertificateException {
                                // Fail verification which should produce an alert that is send back to the client.
                                throw new CertificateException();
                            }""",
"""                            public void checkClientTrusted(X509Certificate[] x509Certificates, String s)
                                    throws CertificateException {
                                // Fail verification which should produce an alert that is send back to the client.
                                PshProbe.log("alert.server checkClientTrusted -> throwing");
                                throw new CertificateException();
                            }""")

# --- alert test: bracket the promise wait and the two closes ----------------
sub("""            promise.syncUninterruptibly();
        } finally {
            if (cc != null) {
                cc.close().syncUninterruptibly();
            }
            if (sc != null) {
                sc.close().syncUninterruptibly();
            }""",
"""            String __k = PshProbe.enter("alert.promise", new Object[] { promise, cc });
            try {
                promise.syncUninterruptibly();
            } finally {
                PshProbe.leave(__k);
            }
        } finally {
            if (cc != null) {
                PshProbe.await("alert.cc.close", cc.close());
            }
            if (sc != null) {
                PshProbe.await("alert.sc.close", sc.close());
            }""")

os.makedirs(os.path.dirname(OUT), exist_ok=True)
io.open(OUT, "w", encoding="utf-8", newline="\n").write(s)
print("wrote", OUT, len(s), "bytes")
