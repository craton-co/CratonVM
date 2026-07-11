# Refuted: NioEndpoint Acceptor/Poller scheduling latency

**Archived 2026-07-11.** The fixed-length HTTP streaming repair passed both
Tomcat regressions. This was never a VM scheduling defect.

The claimed scheduler/GC-blocking-region issue is **refuted**. The Acceptor
entered native `accept()` immediately; a direct `Socket` client was accepted
promptly under the same NIO/Poller shape. The apparent two-second stall was
the client-side legacy `HttpURLConnection` bridge buffering a fixed-length
stream until response retrieval.

The unresolved issue is now tracked at
[`httpurlconnection-fixed-length-streaming-deferred.md`](httpurlconnection-fixed-length-streaming-deferred.md).
