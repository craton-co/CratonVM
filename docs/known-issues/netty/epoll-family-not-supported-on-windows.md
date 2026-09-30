# `io.netty.channel.epoll.Epoll*` — not a CratonVM bug, epoll is Linux-only

| | |
|---|---|
| **Status** | Confirmed NOT a CratonVM bug. Structural platform mismatch, not a defect. |
| **Scope** | 63 classes, all under `io.netty.channel.epoll.*`, in `all-739.txt`. |
| **Discovered** | 2026-09-23, full 739-class suite run on Windows host (`run-20260923-192409-passed`). |

## Symptom

Every class in the package fails immediately on Windows with one of two root causes:

1. Non-SSL transport classes (57 classes):
```
java.lang.UnsatisfiedLinkError: io.netty.channel.epoll.Native is not supported by CratonVM; use JDK NIO selector
    at io.netty.channel.epoll.Native.<clinit>(Native.java:68)
    at io.netty.channel.epoll.Epoll.<clinit>(Epoll.java:39)
```

2. SSL echo / loopback classes (6 classes: `EpollSocketSslEchoTest`, `EpollDomainSocketSslEchoTest`, `EpollJdkLoopbackSocketSslEchoTest`, `EpollDomainSocketSslGreetingTest`, `EpollDomainSocketStartTlsTest`, `EpollSocketSslGreetingTest`):
```
java.lang.IllegalArgumentException: Failed to load any of the given libraries: [netty_tcnative_windows_x86_64, netty_tcnative_x86_64, netty_tcnative]
    at io.netty.util.internal.NativeLibraryLoader.loadFirstAvailable(NativeLibraryLoader.java:119)
    at io.netty.handler.ssl.OpenSsl.loadTcNative(OpenSsl.java:773)
```

## Why this is not a CratonVM defect

`epoll(7)` is a Linux kernel facility. It does not exist on Windows (or macOS/BSD) — this is not a missing native library or an unimplemented CratonVM feature, it is an OS-level absence. Netty's `io.netty.channel.epoll.*` package provides native Linux socket transport via JNI bindings to Linux system calls (`epoll_create1`, `epoll_ctl`, `epoll_wait`).

On non-Linux hosts, Netty's native transport is completely unavailable on both HotSpot and CratonVM. This directly mirrors the situation documented in `kqueue-family-not-supported-on-linux.md` (where BSD/macOS `kqueue` classes fail identically when executed on Linux).

This is collector-independent (identical across Generational, G1, and ZGC) and accounts for 63 of the 102 FAIL classes in the full 739-class discovery run.

## Disposition

No fix needed or possible on Windows. The correct long-term action is to filter out `io.netty.channel.epoll.*` from test discovery when running on Windows hosts, which will eliminate 63 permanent, uninformative FAIL rows from suite reports.

## Affected Classes (63 total)

- `io.netty.channel.epoll.EpollCompositeBufferGatheringWriteTest`
- `io.netty.channel.epoll.EpollDatagramChannelConfigTest`
- `io.netty.channel.epoll.EpollDatagramChannelTest`
- `io.netty.channel.epoll.EpollDatagramConnectNotExistsTest`
- `io.netty.channel.epoll.EpollDatagramMulticastIPv6Test`
- `io.netty.channel.epoll.EpollDatagramMulticastIpv6WithIpv4AddrTest`
- `io.netty.channel.epoll.EpollDatagramUnicastIPv6MappedTest`
- `io.netty.channel.epoll.EpollDatagramUnicastIPv6Test`
- `io.netty.channel.epoll.EpollDatagramUnicastTest`
- `io.netty.channel.epoll.EpollDetectPeerCloseWithoutReadTest`
- `io.netty.channel.epoll.EpollDomainDatagramChannelTest`
- `io.netty.channel.epoll.EpollDomainDatagramPathTest`
- `io.netty.channel.epoll.EpollDomainSocketDataReadInitialStateTest`
- `io.netty.channel.epoll.EpollDomainSocketEchoTest`
- `io.netty.channel.epoll.EpollDomainSocketFdTest`
- `io.netty.channel.epoll.EpollDomainSocketFileRegionTest`
- `io.netty.channel.epoll.EpollDomainSocketFixedLengthEchoTest`
- `io.netty.channel.epoll.EpollDomainSocketGatheringWriteTest`
- `io.netty.channel.epoll.EpollDomainSocketShutdownOutputByPeerTest`
- `io.netty.channel.epoll.EpollDomainSocketSslEchoTest`
- `io.netty.channel.epoll.EpollDomainSocketSslGreetingTest`
- `io.netty.channel.epoll.EpollDomainSocketStartTlsTest`
- `io.netty.channel.epoll.EpollDomainSocketStringEchoTest`
- `io.netty.channel.epoll.EpollJdkLoopbackSocketSslEchoTest`
- `io.netty.channel.epoll.EpollOrderedChannelGatheringWriteTest`
- `io.netty.channel.epoll.EpollRecvByteBufAllocatorTest`
- `io.netty.channel.epoll.EpollReuseAddrTest`
- `io.netty.channel.epoll.EpollServerDomainSocketTest`
- `io.netty.channel.epoll.EpollServerSocketChannelConfigTest`
- `io.netty.channel.epoll.EpollSocketAddressesTest`
- `io.netty.channel.epoll.EpollSocketChannelConfigTest`
- `io.netty.channel.epoll.EpollSocketChannelNotYetConnectedTest`
- `io.netty.channel.epoll.EpollSocketChannelTest`
- `io.netty.channel.epoll.EpollSocketCloseForciblyTest`
- `io.netty.channel.epoll.EpollSocketConditionalWritabilityTest`
- `io.netty.channel.epoll.EpollSocketConnectTest`
- `io.netty.channel.epoll.EpollSocketConnectionAttemptTest`
- `io.netty.channel.epoll.EpollSocketEchoTest`
- `io.netty.channel.epoll.EpollSocketExceptionHandlingTest`
- `io.netty.channel.epoll.EpollSocketFdTest`
- `io.netty.channel.epoll.EpollSocketFileRegionTest`
- `io.netty.channel.epoll.EpollSocketFixedLengthEchoTest`
- `io.netty.channel.epoll.EpollSocketGatheringWriteTest`
- `io.netty.channel.epoll.EpollSocketHalfClosedTest`
- `io.netty.channel.epoll.EpollSocketMultipleConnectTest`
- `io.netty.channel.epoll.EpollSocketRstTest`
- `io.netty.channel.epoll.EpollSocketShutdownOutputByPeerTest`
- `io.netty.channel.epoll.EpollSocketShutdownOutputBySelfTest`
- `io.netty.channel.epoll.EpollSocketSslEchoTest`
- `io.netty.channel.epoll.EpollSocketSslGreetingTest`
- `io.netty.channel.epoll.EpollSocketStartTlsTest`
- `io.netty.channel.epoll.EpollSocketStringEchoTest`
- `io.netty.channel.epoll.EpollSpliceTest`
- `io.netty.channel.epoll.EpollTcpInfoTest`
- `io.netty.channel.epoll.EpollTest`
- `io.netty.channel.epoll.EpollTraceTest`
- `io.netty.channel.epoll.EpollWaitBatchTest`
- `io.netty.channel.epoll.EpollZeroCopyTest`
- `io.netty.channel.epoll.ManualEventLoopTest`
- `io.netty.channel.epoll.NativeTest`
- `io.netty.channel.epoll.SegmentedDatagramPacketGatheringWriteTest`
- `io.netty.channel.epoll.TcpFastOpenTest`
- `io.netty.channel.epoll.VSockTest`
