# Spring messaging.simp CratonVM-only failures

Status: fixed on 2026-07-08 in branch `codex/messaging-simp-20260708`.

## Original symptoms

`messaging.simp` had CratonVM-only failures while HotSpot passed:

- `DefaultSubscriptionRegistryTests`: assertion failures in subscription registry mutation.
- `OrderedMessageChannelDecoratorTests`: assertion failure.
- `BufferingStompDecoderTests`: `NullPointerException` from a missing `AtomicInteger` count.
- `ReactorNettyStompBrokerRelayIntegrationTests`: ABEND / missing relay messages.
- `ReactorNettyTcpStompClientTests`: DNS native gap and then STOMP subscription timeout.

## Root causes fixed

- `ConcurrentHashMap.computeIfPresent` was missing from the native CHM bridge, so Spring subscription-registry updates did not match HotSpot.
- `LinkedBlockingQueue.clear()` in the real-JDK essential native path treated slot 1 as the synthetic `size` int and overwrote the real `count: AtomicInteger` reference; `BufferingStompDecoderTests` exposed this when chunk assembly cleared its queue and later called real queue bytecode that dereferenced `count`.
- `Collections.max(Collection)` returned `null` for non-ArrayList collections. ActiveMQ STOMP version negotiation uses `HashSet` plus `Collections.max`, which produced `CONNECTED version:null` and broke receipt handling.
- Reactor Netty was forced to too small a worker pool. One or two workers could strand simultaneous STOMP connects; a deterministic default of four workers keeps the Spring concurrent-connect path live while preserving explicit user `-Dreactor.netty.ioWorkerCount` values.
- The Spring/Netty/ActiveMQ bridge layer needed explicit real-JDK-mode coverage for Netty JCTools queues, Reactor `Mono.just`, STOMP frame sends, ActiveMQ topic/cursor helpers, and `ByteBuffer.wrap`/shared-secret access used in this cluster.

## Validation

HotSpot baseline:

- Runner: `/data/data/cratonvm-broken-git-backup/apps/spring-suite-runner`
- Spring tree: `/data/data/wt-osr-nonpassed-20260706-1945/apps/spring-framework`
- Output: `out/hotspot-all-20260708-170357`
- Result: `classes: EMPTY=1 OK=33`, `test-methods: found=338 passed=338 failed=0`

CratonVM JIT-real final:

- Binary: `/data/data/cratonvm-worktrees/20260708-messaging-simp/cvm-messaging-simp-20260708-merged`
- Output: `out/jit-real-all-20260708-191453`
- Result: `classes: EMPTY=1 OK=33`, `test-methods: found=338 passed=338 failed=0`

Focused probes also confirmed:

- `StompCodec.detectVersion` now returns `1.2` for `accept-version: 1.1,1.2`.
- Plain ActiveMQ STOMP socket probe receives `RECEIPT` for a SUBSCRIBE with `receipt:0`.
- Two simultaneous Spring `ReactorNettyTcpStompClient.connectAsync` sessions both connect, receive subscription receipts, and receive a published message with the four-worker fallback.
