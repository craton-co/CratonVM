# `PcapWriteHandlerTest` — three separate residuals (UDP bind, TCP close packets, >4 GB)

**Status:** OPEN (2026-08-12). Found while working
[investigate-batch-09.md](investigate-batch-09.md). Not one bug — the 7 failing
tests split into three independent causes, and the first has a minimal repro
that needs no netty test.

`io.netty.handler.pcap.PcapWriteHandlerTest`: **25/25 in 4.5 s on stock HotSpot
JDK 25**, 18 ok / 7 failed in 176 s on CratonVM.

## Residual 1 — `NioDatagramChannel.bind()` throws `StacklessClosedChannelException`

The most valuable of the three, because it reproduces in ~50 lines with no
netty *test* involved: bind two `NioDatagramChannel`s on loopback and send one
4-byte datagram.

```
HotSpot : received=true
          content: readable=4 ridx=0 widx=4 cap=2048 maxCap=2147483647 text=Meow
CratonVM: Exception in thread "main" io/netty/channel/StacklessClosedChannelException
              at io.netty.channel.AbstractChannel$AbstractUnsafe.ensureOpen(ChannelPromise)
```

Deterministic — reproduced on every run. The channel is already closed by the
time `bind` reaches `ensureOpen`, so the datagram is never sent or received.

This accounts for the three `udpV4*` failures and for the
`StacklessClosedChannelException`s in the class's output.

A related symptom in the same run, worth recording because it looks like a
second bug and is not:

```
WARN i.n.handler.pcap.PcapWriteHandler - Unable to write UDP packet to PCAP.
     Payload of size {} exceeds max size of 65507
```

The unsubstituted `{}` is **netty's own bug** — `PcapWriteHandler.java:495`
passes no argument to the placeholder — not a CratonVM logging-shim defect.
Confirmed by reading the source before filing it.

## Residual 2 — TCP close packets are never written

`tcpV4NonOutputStream`, `tcpV4NoGlobalHeaderOutputStream` and
`tcpV4SharedOutputStreamTest` fail with:

```
IndexOutOfBoundsException: readerIndex(522) + length(4) exceeds writerIndex(522)
    at PcapWriteHandlerTest.verifyTcpBaseHeaders(PcapWriteHandlerTest.java:1248)
    at PcapWriteHandlerTest.verifyTcpCloseCapture(PcapWriteHandlerTest.java:1206)
```

The byte arithmetic says exactly what is missing. A pcap record is a 16-byte
header plus the packet; these packets are 54 bytes (+ payload), and the file
opens with a 24-byte global header:

| section | packets | bytes |
|---|---|---|
| global header | — | 24 |
| handshake (SYN, SYN-ACK, ACK) | 3 | 210 |
| data capture #1 (data + ack) | 2 | 144 |
| data capture #2 (data + ack) | 2 | 144 |
| **subtotal** | | **522** |
| close (FIN, FIN-ACK, ACK) | 3 | 210 |
| expected total | | 732 |

The capture is 522 bytes — **byte-exact through the second data capture, with
all three close packets absent.** Not truncation: everything written is
correct, and the close sequence was never produced.

`PcapWriteHandler` writes that sequence from **`handlerRemoved`**, not
`channelInactive`. Both hypotheses about the hook itself are **refuted** — a
probe recording `channelActive`/`channelInactive`/`channelUnregistered`/
`handlerRemoved` on a real NIO socket pair produces an identical event
sequence on both VMs:

```
[client:active, server:active, client:close-called, client:inactive,
 client:unregistered, client:handlerRemoved, server:inactive,
 server:unregistered, server:handlerRemoved]
```

So `handlerRemoved` **runs**, and what it does inside is where the writes are
lost. That is where the next session should start — instrument
`PcapWriteHandler.handlerRemoved`'s fake-FIN flow (it allocates via
`ctx.alloc()` and writes after the channel is closed) rather than re-testing
the lifecycle hooks, which are already cleared.

## Residual 3 — `writePcapGreaterThan4Gb` times out

```
java.util.concurrent.TimeoutException: writePcapGreaterThan4Gb() timed out after 120 seconds
```

The test writes more than 4 GB through the handler to exercise the pcap
length-field rollover. HotSpot completes the whole class in 4.5 s; CratonVM
spent 176 s on it. This one looks like the documented throughput gap rather
than a correctness defect, but it has **not** been separated the way batch 08's
hang was — nobody has re-run it with the timeout disabled to confirm it
finishes at all. Do that before assuming.

## Not investigated further

Time-boxed: the batch-09 session fixed the zlib preset-dictionary defect
([record](inflater-preset-dictionary-was-a-no-op-20260812.md)) and left this
class characterized rather than fixed. Residual 1 is the recommended entry
point — smallest repro, clearest signal, and it is a datagram-channel defect
that almost certainly reaches past netty.
