# Superseded: NioEndpoint Acceptor/Poller scheduling latency

**Status:** SUPERSEDED/REFUTED 2026-07-11 — the claimed scheduler/GC-blocking-region
issue does not hold up; see below. **Retired from `docs/known-issues/` to
`docs/internal/tomcat-08-07/`** per this repo's convention that `known-issues/`
holds only open items.

The claimed scheduler/GC-blocking-region issue is **refuted**. The Acceptor
entered native `accept()` immediately; a direct `Socket` client was accepted
promptly under the same NIO/Poller shape. The apparent two-second stall was
the client-side legacy `HttpURLConnection` bridge buffering a fixed-length
stream until response retrieval.

The unresolved issue is tracked at
[`httpurlconnection-fixed-length-streaming-deferred.md`](../../known-issues/httpurlconnection-fixed-length-streaming-deferred.md)
(still open in `docs/known-issues/` as of this doc's retirement).
