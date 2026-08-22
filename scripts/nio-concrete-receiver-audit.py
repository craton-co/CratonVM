#!/usr/bin/env python3
"""Cross-crate coverage audit for the abstract-to-concrete receiver relocations.

    python3 scripts/nio-concrete-receiver-audit.py <reg.json>

where `reg.json` comes from

    cratonvm --jdk-only --java-home <JDK> --dump-native-registry reg.json \\
             -cp <cp> <MainClass>

# What it checks, and why a build and a green corpus do not

`H21-1` and `WORKER-4-1` moved thirteen NIO factories from minting an instance
of the ABSTRACT public API class -- a receiver `new` cannot legally produce
(JVMS 6.5) -- to minting the concrete `sun.nio.*` class the JDK itself builds.
Native dispatch keys on the RECEIVER's runtime class (`H11-1`), so every such
move has a second half: the registrations have to move with it, or the JDK's own
bytecode takes over against an object whose `<init>` this VM never ran.

`concrete_receiver::mirror_class_registrations` does that half automatically --
but only for the rows the CALLING registrar wrote, which is deliberate: filtering
on the class name alone would also copy some other crate's rows and hand the new
class's slot to a body with an unrelated field layout (`H11-2` section 4).

**That leaves exactly one gap, and it is invisible to every other instrument: a
triple registered on the abstract class by a DIFFERENT crate.** This script is
the instrument for it. It found two, both of which turned a corpus vector red:

    java/nio/channels/AsynchronousFileChannel.force(Z)V
      registered only by native-builtins/src/phases_late/net_channels.rs
      -> RJdkAsyncChannel: NullPointerException, "this.threads" is null,
         at sun/nio/ch/SimpleAsynchronousFileChannelImpl.implForce

    java/nio/channels/AsynchronousSocketChannel
        .connect(Ljava/net/SocketAddress;)Ljava/util/concurrent/Future;
      registered only by the same file

# Reading the output

`GAP` means: this triple has a registration on the OLD (abstract) class and NONE
on any of the concrete classes that replaced it. Either mirror it, or state at
the registration site why the JDK's own body is the right answer for it.

`mirrored onto NOTHING` means the image declares none of the listed concrete
classes -- normal on a platform whose implementation is not in `MOVES` below,
and the reason every relocation keeps the abstract name as a final fallback.

STATIC methods are excluded: their dispatch key is the constant-pool class, not
the receiver, so their registration correctly stays on the public class
(`H11-1`). Add to `STATIC_METHODS` rather than "fixing" such a row.
"""
import json
import sys
import collections

# The relocations, old (abstract/interface) -> the concrete classes that must
# answer for it. Add a row here whenever a factory stops minting an abstract
# class; the whole point is that this table and the mint sites are checkable
# against each other.
MOVES = {
    "java/nio/channels/Pipe": ["sun/nio/ch/PipeImpl"],
    "java/nio/channels/SocketChannel": ["sun/nio/ch/SocketChannelImpl"],
    "java/nio/channels/ServerSocketChannel": ["sun/nio/ch/ServerSocketChannelImpl"],
    "java/nio/channels/DatagramChannel": ["sun/nio/ch/DatagramChannelImpl"],
    "java/nio/channels/MembershipKey": [
        "sun/nio/ch/MembershipKeyImpl",
        "sun/nio/ch/MembershipKeyImpl$Type4",
        "sun/nio/ch/MembershipKeyImpl$Type6",
    ],
    "java/nio/file/WatchService": [
        "sun/nio/fs/LinuxWatchService",
        "sun/nio/fs/WindowsWatchService",
        "sun/nio/fs/PollingWatchService",
        "sun/nio/fs/BsdWatchService",
        "sun/nio/fs/AbstractWatchService",
    ],
    "java/nio/file/WatchKey": [
        "sun/nio/fs/LinuxWatchService$LinuxWatchKey",
        "sun/nio/fs/WindowsWatchService$WindowsWatchKey",
        "sun/nio/fs/PollingWatchService$PollingWatchKey",
        "sun/nio/fs/AbstractWatchKey",
    ],
    "java/nio/file/WatchEvent": ["sun/nio/fs/AbstractWatchKey$Event"],
    "java/nio/channels/AsynchronousSocketChannel": [
        "sun/nio/ch/UnixAsynchronousSocketChannelImpl",
        "sun/nio/ch/WindowsAsynchronousSocketChannelImpl",
        "sun/nio/ch/AsynchronousSocketChannelImpl",
    ],
    "java/nio/channels/AsynchronousServerSocketChannel": [
        "sun/nio/ch/UnixAsynchronousServerSocketChannelImpl",
        "sun/nio/ch/WindowsAsynchronousServerSocketChannelImpl",
        "sun/nio/ch/AsynchronousServerSocketChannelImpl",
    ],
    "java/nio/channels/AsynchronousChannelGroup": [
        "sun/nio/ch/EPollPort",
        "sun/nio/ch/Iocp",
        "sun/nio/ch/KQueuePort",
        "sun/nio/ch/SolarisEventPort",
        "sun/nio/ch/Port",
        "sun/nio/ch/AsynchronousChannelGroupImpl",
    ],
    "java/nio/channels/AsynchronousFileChannel": [
        "sun/nio/ch/SimpleAsynchronousFileChannelImpl",
        "sun/nio/ch/WindowsAsynchronousFileChannelImpl",
        "sun/nio/ch/AsynchronousFileChannelImpl",
    ],
    "sun/nio/ch/SelectorImpl": [
        "sun/nio/ch/EPollSelectorImpl",
        "sun/nio/ch/WEPollSelectorImpl",
        "sun/nio/ch/KQueueSelectorImpl",
        "sun/nio/ch/WindowsSelectorImpl",
        "sun/nio/ch/DevPollSelectorImpl",
        "sun/nio/ch/PollSelectorImpl",
    ],
    "java/nio/file/FileStore": [
        "sun/nio/fs/LinuxFileStore",
        "sun/nio/fs/MacOSXFileStore",
        "sun/nio/fs/BsdFileStore",
        "sun/nio/fs/SolarisFileStore",
        "sun/nio/fs/WindowsFileStore",
        "sun/nio/fs/UnixFileStore",
    ],
    "java/nio/file/DirectoryStream": [
        "sun/nio/fs/UnixSecureDirectoryStream",
        "sun/nio/fs/UnixDirectoryStream",
        "sun/nio/fs/WindowsDirectoryStream",
    ],
}

# Dispatch key is the constant-pool class, not the receiver. See the header.
STATIC_METHODS = {
    "open",
    "withFixedThreadPool",
    "withThreadPool",
    "withCachedThreadPool",
    "provider",
    "getDefault",
}


def main():
    if len(sys.argv) != 2:
        print(__doc__)
        return 2
    payload = json.load(open(sys.argv[1]))
    rows = payload["natives"] if isinstance(payload, dict) else payload

    by_class = collections.defaultdict(set)
    owners = collections.defaultdict(set)
    present = set()
    for r in rows:
        key = (r["class"], r["name"], r["descriptor"])
        by_class[r["class"]].add(key[1:])
        owners[key].add(r.get("registered_by", "?"))
        present.add(r["class"])

    gaps = 0
    for old, news in MOVES.items():
        have = by_class.get(old)
        if not have:
            continue
        live = [n for n in news if n in present]
        covered = set()
        for n in live:
            covered |= by_class[n]
        missing = sorted(t for t in have if t not in covered and t[0] not in STATIC_METHODS)
        print("== %s  (%d triples, mirrored onto %s)"
              % (old, len(have), ", ".join(live) if live else "NOTHING"))
        for method, descriptor in missing:
            who = sorted(owners[(old, method, descriptor)])
            print("   GAP  %s%s   registered_by=%s" % (method, descriptor, who))
            gaps += 1

    print("\nTOTAL GAPS: %d" % gaps)
    return 1 if gaps else 0


if __name__ == "__main__":
    sys.exit(main())
