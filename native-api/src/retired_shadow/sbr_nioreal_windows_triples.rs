#[allow(unused_imports)]
use super::*;


/// Windows: the same family over `WEPollSelectorImpl` / `WEPollSelectorProvider` (the platform's default
/// selector and provider) instead of `EPollSelectorImpl` / `EPollSelectorProvider`; `PollSelectorImpl` does not
/// exist there. Generated from the table above, so the two cannot drift: every row on a class both platforms
/// share is the same row. It is NOT in `RETIRED_SHADOW_TABLES`: `retired_shadow_row_set` adds it on Windows
/// only with `CRATONVM_WINDOWS_CHANNEL_LAYER=1` (a retirement is one answer for every VM in the process,
/// and without the layer the carriers must keep serving), until the Windows layer (`native-io/src/net_wepoll.rs`, `net_nio_win.rs`) is proven; see
/// `docs/feature-designs/profile-guided-inlining.md`.
#[cfg(windows)]
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR_NIOREAL_WINDOWS_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/lang/System$1",
        "blockedOn",
        "(Lsun/nio/ch/Interruptible;)V",
    ),
    ("java/net/DatagramSocket", "<init>", "()V"),
    ("java/net/DatagramSocket", "<init>", "(I)V"),
    (
        "java/net/DatagramSocket",
        "<init>",
        "(ILjava/net/InetAddress;)V",
    ),
    (
        "java/net/DatagramSocket",
        "<init>",
        "(Ljava/net/SocketAddress;)V",
    ),
    (
        "java/net/DatagramSocket",
        "bind",
        "(Ljava/net/SocketAddress;)V",
    ),
    ("java/net/DatagramSocket", "close", "()V"),
    (
        "java/net/DatagramSocket",
        "connect",
        "(Ljava/net/InetAddress;I)V",
    ),
    (
        "java/net/DatagramSocket",
        "connect",
        "(Ljava/net/SocketAddress;)V",
    ),
    ("java/net/DatagramSocket", "disconnect", "()V"),
    ("java/net/DatagramSocket", "getBroadcast", "()Z"),
    (
        "java/net/DatagramSocket",
        "getInetAddress",
        "()Ljava/net/InetAddress;",
    ),
    (
        "java/net/DatagramSocket",
        "getLocalAddress",
        "()Ljava/net/InetAddress;",
    ),
    ("java/net/DatagramSocket", "getLocalPort", "()I"),
    (
        "java/net/DatagramSocket",
        "getLocalSocketAddress",
        "()Ljava/net/SocketAddress;",
    ),
    (
        "java/net/DatagramSocket",
        "getOption",
        "(Ljava/net/SocketOption;)Ljava/lang/Object;",
    ),
    ("java/net/DatagramSocket", "getPort", "()I"),
    ("java/net/DatagramSocket", "getReceiveBufferSize", "()I"),
    (
        "java/net/DatagramSocket",
        "getRemoteSocketAddress",
        "()Ljava/net/SocketAddress;",
    ),
    ("java/net/DatagramSocket", "getReuseAddress", "()Z"),
    ("java/net/DatagramSocket", "getSendBufferSize", "()I"),
    ("java/net/DatagramSocket", "getSoTimeout", "()I"),
    ("java/net/DatagramSocket", "getTrafficClass", "()I"),
    ("java/net/DatagramSocket", "isBound", "()Z"),
    ("java/net/DatagramSocket", "isClosed", "()Z"),
    ("java/net/DatagramSocket", "isConnected", "()Z"),
    (
        "java/net/DatagramSocket",
        "receive",
        "(Ljava/net/DatagramPacket;)V",
    ),
    (
        "java/net/DatagramSocket",
        "send",
        "(Ljava/net/DatagramPacket;)V",
    ),
    ("java/net/DatagramSocket", "setBroadcast", "(Z)V"),
    (
        "java/net/DatagramSocket",
        "setOption",
        "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/net/DatagramSocket;",
    ),
    ("java/net/DatagramSocket", "setReceiveBufferSize", "(I)V"),
    ("java/net/DatagramSocket", "setReuseAddress", "(Z)V"),
    ("java/net/DatagramSocket", "setSendBufferSize", "(I)V"),
    ("java/net/DatagramSocket", "setSoTimeout", "(I)V"),
    ("java/net/DatagramSocket", "setTrafficClass", "(I)V"),
    (
        "java/net/DatagramSocket",
        "supportedOptions",
        "()Ljava/util/Set;",
    ),
    ("java/net/MulticastSocket", "<init>", "()V"),
    ("java/net/MulticastSocket", "<init>", "(I)V"),
    ("java/net/MulticastSocket", "close", "()V"),
    ("java/net/MulticastSocket", "getLoopbackMode", "()Z"),
    (
        "java/net/MulticastSocket",
        "getOption",
        "(Ljava/net/SocketOption;)Ljava/lang/Object;",
    ),
    ("java/net/MulticastSocket", "getSoTimeout", "()I"),
    ("java/net/MulticastSocket", "getTimeToLive", "()I"),
    ("java/net/MulticastSocket", "isClosed", "()Z"),
    (
        "java/net/MulticastSocket",
        "joinGroup",
        "(Ljava/net/InetAddress;)V",
    ),
    (
        "java/net/MulticastSocket",
        "joinGroup",
        "(Ljava/net/SocketAddress;Ljava/net/NetworkInterface;)V",
    ),
    (
        "java/net/MulticastSocket",
        "leaveGroup",
        "(Ljava/net/InetAddress;)V",
    ),
    (
        "java/net/MulticastSocket",
        "leaveGroup",
        "(Ljava/net/SocketAddress;Ljava/net/NetworkInterface;)V",
    ),
    (
        "java/net/MulticastSocket",
        "receive",
        "(Ljava/net/DatagramPacket;)V",
    ),
    (
        "java/net/MulticastSocket",
        "send",
        "(Ljava/net/DatagramPacket;)V",
    ),
    ("java/net/MulticastSocket", "setLoopbackMode", "(Z)V"),
    (
        "java/net/MulticastSocket",
        "setOption",
        "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/net/DatagramSocket;",
    ),
    ("java/net/MulticastSocket", "setSoTimeout", "(I)V"),
    ("java/net/MulticastSocket", "setTimeToLive", "(I)V"),
    ("java/nio/channels/DatagramChannel", "close", "()V"),
    (
        "java/nio/channels/DatagramChannel",
        "configureBlocking",
        "(Z)Ljava/nio/channels/SelectableChannel;",
    ),
    (
        "java/nio/channels/DatagramChannel",
        "implCloseChannel",
        "()V",
    ),
    ("java/nio/channels/DatagramChannel", "isBlocking", "()Z"),
    ("java/nio/channels/DatagramChannel", "isOpen", "()Z"),
    ("java/nio/channels/DatagramChannel", "isRegistered", "()Z"),
    (
        "java/nio/channels/DatagramChannel",
        "keyFor",
        "(Ljava/nio/channels/Selector;)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "java/nio/channels/DatagramChannel",
        "open",
        "()Ljava/nio/channels/DatagramChannel;",
    ),
    (
        "java/nio/channels/DatagramChannel",
        "open",
        "(Ljava/net/ProtocolFamily;)Ljava/nio/channels/DatagramChannel;",
    ),
    (
        "java/nio/channels/DatagramChannel",
        "provider",
        "()Ljava/nio/channels/spi/SelectorProvider;",
    ),
    (
        "java/nio/channels/DatagramChannel",
        "read",
        "([Ljava/nio/ByteBuffer;)J",
    ),
    (
        "java/nio/channels/DatagramChannel",
        "register",
        "(Ljava/nio/channels/Selector;I)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "java/nio/channels/DatagramChannel",
        "register",
        "(Ljava/nio/channels/Selector;ILjava/lang/Object;)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "java/nio/channels/DatagramChannel",
        "setOption",
        "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/NetworkChannel;",
    ),
    (
        "java/nio/channels/DatagramChannel",
        "write",
        "([Ljava/nio/ByteBuffer;)J",
    ),
    (
        "java/nio/channels/SelectableChannel",
        "register",
        "(Ljava/nio/channels/Selector;I)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "java/nio/channels/SelectionKey",
        "attach",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/nio/channels/SelectionKey",
        "attachment",
        "()Ljava/lang/Object;",
    ),
    (
        "java/nio/channels/Selector",
        "open",
        "()Ljava/nio/channels/Selector;",
    ),
    (
        "java/nio/channels/ServerSocketChannel",
        "bind",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/NetworkChannel;",
    ),
    (
        "java/nio/channels/ServerSocketChannel",
        "bind",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/ServerSocketChannel;",
    ),
    ("java/nio/channels/ServerSocketChannel", "close", "()V"),
    (
        "java/nio/channels/ServerSocketChannel",
        "configureBlocking",
        "(Z)Ljava/nio/channels/SelectableChannel;",
    ),
    (
        "java/nio/channels/ServerSocketChannel",
        "implCloseChannel",
        "()V",
    ),
    ("java/nio/channels/ServerSocketChannel", "isBlocking", "()Z"),
    ("java/nio/channels/ServerSocketChannel", "isOpen", "()Z"),
    (
        "java/nio/channels/ServerSocketChannel",
        "isRegistered",
        "()Z",
    ),
    (
        "java/nio/channels/ServerSocketChannel",
        "keyFor",
        "(Ljava/nio/channels/Selector;)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "java/nio/channels/ServerSocketChannel",
        "open",
        "()Ljava/nio/channels/ServerSocketChannel;",
    ),
    (
        "java/nio/channels/ServerSocketChannel",
        "open",
        "(Ljava/net/ProtocolFamily;)Ljava/nio/channels/ServerSocketChannel;",
    ),
    (
        "java/nio/channels/ServerSocketChannel",
        "provider",
        "()Ljava/nio/channels/spi/SelectorProvider;",
    ),
    (
        "java/nio/channels/ServerSocketChannel",
        "register",
        "(Ljava/nio/channels/Selector;I)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "java/nio/channels/ServerSocketChannel",
        "register",
        "(Ljava/nio/channels/Selector;ILjava/lang/Object;)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "java/nio/channels/ServerSocketChannel",
        "setOption",
        "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/NetworkChannel;",
    ),
    (
        "java/nio/channels/SocketChannel",
        "bind",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/NetworkChannel;",
    ),
    ("java/nio/channels/SocketChannel", "close", "()V"),
    (
        "java/nio/channels/SocketChannel",
        "configureBlocking",
        "(Z)Ljava/nio/channels/SelectableChannel;",
    ),
    ("java/nio/channels/SocketChannel", "implCloseChannel", "()V"),
    ("java/nio/channels/SocketChannel", "isBlocking", "()Z"),
    ("java/nio/channels/SocketChannel", "isOpen", "()Z"),
    ("java/nio/channels/SocketChannel", "isRegistered", "()Z"),
    (
        "java/nio/channels/SocketChannel",
        "keyFor",
        "(Ljava/nio/channels/Selector;)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "java/nio/channels/SocketChannel",
        "open",
        "()Ljava/nio/channels/SocketChannel;",
    ),
    (
        "java/nio/channels/SocketChannel",
        "open",
        "(Ljava/net/ProtocolFamily;)Ljava/nio/channels/SocketChannel;",
    ),
    (
        "java/nio/channels/SocketChannel",
        "open",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/SocketChannel;",
    ),
    (
        "java/nio/channels/SocketChannel",
        "provider",
        "()Ljava/nio/channels/spi/SelectorProvider;",
    ),
    (
        "java/nio/channels/SocketChannel",
        "read",
        "([Ljava/nio/ByteBuffer;)J",
    ),
    (
        "java/nio/channels/SocketChannel",
        "register",
        "(Ljava/nio/channels/Selector;I)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "java/nio/channels/SocketChannel",
        "register",
        "(Ljava/nio/channels/Selector;ILjava/lang/Object;)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "java/nio/channels/SocketChannel",
        "setOption",
        "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/NetworkChannel;",
    ),
    (
        "java/nio/channels/SocketChannel",
        "write",
        "([Ljava/nio/ByteBuffer;)J",
    ),
    (
        "java/nio/channels/spi/AbstractSelectableChannel",
        "isRegistered",
        "()Z",
    ),
    (
        "java/nio/channels/spi/AbstractSelectableChannel",
        "keyFor",
        "(Ljava/nio/channels/Selector;)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "java/nio/channels/spi/AbstractSelectableChannel",
        "provider",
        "()Ljava/nio/channels/spi/SelectorProvider;",
    ),
    (
        "java/nio/channels/spi/AbstractSelectableChannel",
        "register",
        "(Ljava/nio/channels/Selector;I)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "java/nio/channels/spi/AbstractSelectableChannel",
        "register",
        "(Ljava/nio/channels/Selector;ILjava/lang/Object;)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "bind",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/DatagramChannel;",
    ),
    ("sun/nio/ch/DatagramChannelImpl", "close", "()V"),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "configureBlocking",
        "(Z)Ljava/nio/channels/SelectableChannel;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "connect",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/DatagramChannel;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "disconnect",
        "()Ljava/nio/channels/DatagramChannel;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "getLocalAddress",
        "()Ljava/net/SocketAddress;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "getOption",
        "(Ljava/net/SocketOption;)Ljava/lang/Object;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "getRemoteAddress",
        "()Ljava/net/SocketAddress;",
    ),
    ("sun/nio/ch/DatagramChannelImpl", "implCloseChannel", "()V"),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "implCloseSelectableChannel",
        "()V",
    ),
    ("sun/nio/ch/DatagramChannelImpl", "isBlocking", "()Z"),
    ("sun/nio/ch/DatagramChannelImpl", "isConnected", "()Z"),
    ("sun/nio/ch/DatagramChannelImpl", "isOpen", "()Z"),
    ("sun/nio/ch/DatagramChannelImpl", "isRegistered", "()Z"),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "join",
        "(Ljava/net/InetAddress;Ljava/net/NetworkInterface;)Ljava/nio/channels/MembershipKey;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "keyFor",
        "(Ljava/nio/channels/Selector;)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "localAddress",
        "()Ljava/net/InetSocketAddress;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "open",
        "()Ljava/nio/channels/DatagramChannel;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "open",
        "(Ljava/net/ProtocolFamily;)Ljava/nio/channels/DatagramChannel;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "provider",
        "()Ljava/nio/channels/spi/SelectorProvider;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "read",
        "(Ljava/nio/ByteBuffer;)I",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "read",
        "([Ljava/nio/ByteBuffer;)J",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "read",
        "([Ljava/nio/ByteBuffer;II)J",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "receive",
        "(Ljava/nio/ByteBuffer;)Ljava/net/SocketAddress;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "register",
        "(Ljava/nio/channels/Selector;I)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "register",
        "(Ljava/nio/channels/Selector;ILjava/lang/Object;)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "remoteAddress",
        "()Ljava/net/InetSocketAddress;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "send",
        "(Ljava/nio/ByteBuffer;Ljava/net/SocketAddress;)I",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "setOption",
        "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/DatagramChannel;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "setOption",
        "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/NetworkChannel;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "socket",
        "()Ljava/net/DatagramSocket;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "supportedOptions",
        "()Ljava/util/Set;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "write",
        "(Ljava/nio/ByteBuffer;)I",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "write",
        "([Ljava/nio/ByteBuffer;)J",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "write",
        "([Ljava/nio/ByteBuffer;II)J",
    ),
    (
        "sun/nio/ch/DatagramSocketAdaptor",
        "bind",
        "(Ljava/net/SocketAddress;)V",
    ),
    ("sun/nio/ch/DatagramSocketAdaptor", "isConnected", "()Z"),
    (
        "sun/nio/ch/DatagramSocketAdaptor",
        "setReceiveBufferSize",
        "(I)V",
    ),
    (
        "sun/nio/ch/DatagramSocketAdaptor",
        "setReuseAddress",
        "(Z)V",
    ),
    (
        "sun/nio/ch/DatagramSocketAdaptor",
        "setSendBufferSize",
        "(I)V",
    ),
    ("sun/nio/ch/DatagramSocketAdaptor", "setSoTimeout", "(I)V"),
    (
        "sun/nio/ch/DatagramSocketAdaptor",
        "setTrafficClass",
        "(I)V",
    ),
    (
        "sun/nio/ch/MembershipKeyImpl",
        "block",
        "(Ljava/net/InetAddress;)Ljava/nio/channels/MembershipKey;",
    ),
    ("sun/nio/ch/MembershipKeyImpl", "drop", "()V"),
    ("sun/nio/ch/MembershipKeyImpl", "isValid", "()Z"),
    (
        "sun/nio/ch/MembershipKeyImpl",
        "unblock",
        "(Ljava/net/InetAddress;)Ljava/nio/channels/MembershipKey;",
    ),
    (
        "sun/nio/ch/MembershipKeyImpl$Type4",
        "block",
        "(Ljava/net/InetAddress;)Ljava/nio/channels/MembershipKey;",
    ),
    ("sun/nio/ch/MembershipKeyImpl$Type4", "drop", "()V"),
    ("sun/nio/ch/MembershipKeyImpl$Type4", "isValid", "()Z"),
    (
        "sun/nio/ch/MembershipKeyImpl$Type4",
        "unblock",
        "(Ljava/net/InetAddress;)Ljava/nio/channels/MembershipKey;",
    ),
    (
        "sun/nio/ch/MembershipKeyImpl$Type6",
        "block",
        "(Ljava/net/InetAddress;)Ljava/nio/channels/MembershipKey;",
    ),
    ("sun/nio/ch/MembershipKeyImpl$Type6", "drop", "()V"),
    ("sun/nio/ch/MembershipKeyImpl$Type6", "isValid", "()Z"),
    (
        "sun/nio/ch/MembershipKeyImpl$Type6",
        "unblock",
        "(Ljava/net/InetAddress;)Ljava/nio/channels/MembershipKey;",
    ),
    (
        "sun/nio/ch/SelectionKeyImpl",
        "attach",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "sun/nio/ch/SelectionKeyImpl",
        "attachment",
        "()Ljava/lang/Object;",
    ),
    ("sun/nio/ch/SelectionKeyImpl", "cancel", "()V"),
    (
        "sun/nio/ch/SelectionKeyImpl",
        "channel",
        "()Ljava/nio/channels/SelectableChannel;",
    ),
    ("sun/nio/ch/SelectionKeyImpl", "interestOps", "()I"),
    (
        "sun/nio/ch/SelectionKeyImpl",
        "interestOps",
        "(I)Ljava/nio/channels/SelectionKey;",
    ),
    ("sun/nio/ch/SelectionKeyImpl", "isValid", "()Z"),
    ("sun/nio/ch/SelectionKeyImpl", "readyOps", "()I"),
    (
        "sun/nio/ch/SelectionKeyImpl",
        "selector",
        "()Ljava/nio/channels/Selector;",
    ),
    ("sun/nio/ch/SelectorImpl", "close", "()V"),
    ("sun/nio/ch/SelectorImpl", "isOpen", "()Z"),
    ("sun/nio/ch/SelectorImpl", "keys", "()Ljava/util/Set;"),
    (
        "sun/nio/ch/SelectorImpl",
        "lockAndDoSelect",
        "(Ljava/util/function/Consumer;J)I",
    ),
    (
        "sun/nio/ch/SelectorImpl",
        "provider",
        "()Ljava/nio/channels/spi/SelectorProvider;",
    ),
    ("sun/nio/ch/SelectorImpl", "select", "()I"),
    ("sun/nio/ch/SelectorImpl", "select", "(J)I"),
    ("sun/nio/ch/SelectorImpl", "selectNow", "()I"),
    (
        "sun/nio/ch/SelectorImpl",
        "selectedKeys",
        "()Ljava/util/Set;",
    ),
    (
        "sun/nio/ch/SelectorProviderImpl",
        "openDatagramChannel",
        "()Ljava/nio/channels/DatagramChannel;",
    ),
    (
        "sun/nio/ch/SelectorProviderImpl",
        "openDatagramChannel",
        "(Ljava/net/ProtocolFamily;)Ljava/nio/channels/DatagramChannel;",
    ),
    (
        "sun/nio/ch/SelectorProviderImpl",
        "openServerSocketChannel",
        "()Ljava/nio/channels/ServerSocketChannel;",
    ),
    (
        "sun/nio/ch/SelectorProviderImpl",
        "openServerSocketChannel",
        "(Ljava/net/ProtocolFamily;)Ljava/nio/channels/ServerSocketChannel;",
    ),
    (
        "sun/nio/ch/SelectorProviderImpl",
        "openSocketChannel",
        "()Ljava/nio/channels/SocketChannel;",
    ),
    (
        "sun/nio/ch/SelectorProviderImpl",
        "openSocketChannel",
        "(Ljava/net/ProtocolFamily;)Ljava/nio/channels/SocketChannel;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "accept",
        "()Ljava/nio/channels/SocketChannel;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "bind",
        "(Ljava/net/SocketAddress;I)Ljava/nio/channels/ServerSocketChannel;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "blockingAccept",
        "(J)Ljava/nio/channels/SocketChannel;",
    ),
    ("sun/nio/ch/ServerSocketChannelImpl", "close", "()V"),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "configureBlocking",
        "(Z)Ljava/nio/channels/SelectableChannel;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "getLocalAddress",
        "()Ljava/net/SocketAddress;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "getOption",
        "(Ljava/net/SocketOption;)Ljava/lang/Object;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "implCloseChannel",
        "()V",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "implCloseSelectableChannel",
        "()V",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "implConfigureBlocking",
        "(Z)V",
    ),
    ("sun/nio/ch/ServerSocketChannelImpl", "isBlocking", "()Z"),
    ("sun/nio/ch/ServerSocketChannelImpl", "isBound", "()Z"),
    ("sun/nio/ch/ServerSocketChannelImpl", "isOpen", "()Z"),
    ("sun/nio/ch/ServerSocketChannelImpl", "isRegistered", "()Z"),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "keyFor",
        "(Ljava/nio/channels/Selector;)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "localAddress",
        "()Ljava/net/SocketAddress;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "open",
        "()Ljava/nio/channels/ServerSocketChannel;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "open",
        "(Ljava/net/ProtocolFamily;)Ljava/nio/channels/ServerSocketChannel;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "provider",
        "()Ljava/nio/channels/spi/SelectorProvider;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "register",
        "(Ljava/nio/channels/Selector;I)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "register",
        "(Ljava/nio/channels/Selector;ILjava/lang/Object;)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "setOption",
        "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/ServerSocketChannel;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "socket",
        "()Ljava/net/ServerSocket;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "supportedOptions",
        "()Ljava/util/Set;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "bind",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/SocketChannel;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "blockingConnect",
        "(Ljava/net/SocketAddress;J)V",
    ),
    ("sun/nio/ch/SocketChannelImpl", "blockingRead", "([BIIJ)I"),
    (
        "sun/nio/ch/SocketChannelImpl",
        "blockingWriteFully",
        "([BII)V",
    ),
    ("sun/nio/ch/SocketChannelImpl", "close", "()V"),
    (
        "sun/nio/ch/SocketChannelImpl",
        "configureBlocking",
        "(Z)Ljava/nio/channels/SelectableChannel;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "connect",
        "(Ljava/net/SocketAddress;)Z",
    ),
    ("sun/nio/ch/SocketChannelImpl", "finishConnect", "()Z"),
    (
        "sun/nio/ch/SocketChannelImpl",
        "getLocalAddress",
        "()Ljava/net/SocketAddress;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "getOption",
        "(Ljava/net/SocketOption;)Ljava/lang/Object;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "getRemoteAddress",
        "()Ljava/net/SocketAddress;",
    ),
    ("sun/nio/ch/SocketChannelImpl", "implCloseChannel", "()V"),
    (
        "sun/nio/ch/SocketChannelImpl",
        "implCloseSelectableChannel",
        "()V",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "implConfigureBlocking",
        "(Z)V",
    ),
    ("sun/nio/ch/SocketChannelImpl", "isBlocking", "()Z"),
    ("sun/nio/ch/SocketChannelImpl", "isConnected", "()Z"),
    ("sun/nio/ch/SocketChannelImpl", "isConnectionPending", "()Z"),
    ("sun/nio/ch/SocketChannelImpl", "isInputOpen", "()Z"),
    ("sun/nio/ch/SocketChannelImpl", "isOpen", "()Z"),
    ("sun/nio/ch/SocketChannelImpl", "isOutputOpen", "()Z"),
    ("sun/nio/ch/SocketChannelImpl", "isRegistered", "()Z"),
    (
        "sun/nio/ch/SocketChannelImpl",
        "keyFor",
        "(Ljava/nio/channels/Selector;)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "localAddress",
        "()Ljava/net/SocketAddress;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "open",
        "()Ljava/nio/channels/SocketChannel;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "open",
        "(Ljava/net/ProtocolFamily;)Ljava/nio/channels/SocketChannel;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "open",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/SocketChannel;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "provider",
        "()Ljava/nio/channels/spi/SelectorProvider;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "read",
        "(Ljava/nio/ByteBuffer;)I",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "read",
        "([Ljava/nio/ByteBuffer;)J",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "read",
        "([Ljava/nio/ByteBuffer;II)J",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "register",
        "(Ljava/nio/channels/Selector;I)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "register",
        "(Ljava/nio/channels/Selector;ILjava/lang/Object;)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "remoteAddress",
        "()Ljava/net/SocketAddress;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "setOption",
        "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/SocketChannel;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "shutdownInput",
        "()Ljava/nio/channels/SocketChannel;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "shutdownOutput",
        "()Ljava/nio/channels/SocketChannel;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "socket",
        "()Ljava/net/Socket;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "supportedOptions",
        "()Ljava/util/Set;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "write",
        "(Ljava/nio/ByteBuffer;)I",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "write",
        "([Ljava/nio/ByteBuffer;)J",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "write",
        "([Ljava/nio/ByteBuffer;II)J",
    ),
    ("sun/nio/ch/WEPollSelectorImpl", "close", "()V"),
    ("sun/nio/ch/WEPollSelectorImpl", "isOpen", "()Z"),
    ("sun/nio/ch/WEPollSelectorImpl", "keys", "()Ljava/util/Set;"),
    (
        "sun/nio/ch/WEPollSelectorImpl",
        "lockAndDoSelect",
        "(Ljava/util/function/Consumer;J)I",
    ),
    (
        "sun/nio/ch/WEPollSelectorImpl",
        "provider",
        "()Ljava/nio/channels/spi/SelectorProvider;",
    ),
    ("sun/nio/ch/WEPollSelectorImpl", "select", "()I"),
    ("sun/nio/ch/WEPollSelectorImpl", "select", "(J)I"),
    ("sun/nio/ch/WEPollSelectorImpl", "selectNow", "()I"),
    (
        "sun/nio/ch/WEPollSelectorImpl",
        "selectedKeys",
        "()Ljava/util/Set;",
    ),
    (
        "sun/nio/ch/WEPollSelectorImpl",
        "wakeup",
        "()Ljava/nio/channels/Selector;",
    ),
    (
        "sun/nio/ch/WEPollSelectorProvider",
        "openDatagramChannel",
        "()Ljava/nio/channels/DatagramChannel;",
    ),
    (
        "sun/nio/ch/WEPollSelectorProvider",
        "openDatagramChannel",
        "(Ljava/net/ProtocolFamily;)Ljava/nio/channels/DatagramChannel;",
    ),
    (
        "sun/nio/ch/WEPollSelectorProvider",
        "openSelector",
        "()Ljava/nio/channels/spi/AbstractSelector;",
    ),
    (
        "sun/nio/ch/WEPollSelectorProvider",
        "openServerSocketChannel",
        "()Ljava/nio/channels/ServerSocketChannel;",
    ),
    (
        "sun/nio/ch/WEPollSelectorProvider",
        "openServerSocketChannel",
        "(Ljava/net/ProtocolFamily;)Ljava/nio/channels/ServerSocketChannel;",
    ),
    (
        "sun/nio/ch/WEPollSelectorProvider",
        "openSocketChannel",
        "()Ljava/nio/channels/SocketChannel;",
    ),
    (
        "sun/nio/ch/WEPollSelectorProvider",
        "openSocketChannel",
        "(Ljava/net/ProtocolFamily;)Ljava/nio/channels/SocketChannel;",
    ),
];

/// SBR wave 6, lane `httpstack`: the HTTP stack wave 5 held -- the
/// `com.sun.net.httpserver` server (19 rows), the `java.net.http` client (50),
/// the HTTPS `HttpURLConnection` carrier (`javax/net/ssl/HttpsURLConnection` 38,
/// `HttpsURLConnectionImpl` 65) and `DefaultProxySelector` (2) -- retired as one
/// unit, over the real NIO layer (`RETIRED_SHADOW_SBR_NIOREAL_TRIPLES`) and the
/// real JSSE (`RETIRED_SHADOW_SBR_JSSE_TRIPLES`). 174 triples, 181 registrations.
///
/// Every row has `Code` on JDK 21 and 25, and on 17 except six client rows
/// (`HttpClient.close`/`shutdown*`/`isTerminated`/`awaitTermination`,
/// `Builder.HEAD`), which JDK 17 does not declare. Kept, no `Code`: the abstract
/// `HttpsURLConnection` rows, `DefaultProxySelector.init` (a JDK native),
/// `HttpClient.send`, `HttpExchange`/`HttpContext` rows.
///
/// # Why it is safe
///
/// Wave 5 held the server for a keep-alive hang and a `500 No handler for
/// context`; on the merged NIOREAL build neither reproduces (20 iterations,
/// drift 0), and the client is correct. One native edit
/// (`http_url_connection::subclass_runs_its_own_bytecode`) extends wave 5's
/// `--real-jdk` hand-off to `HttpsURLConnectionImpl`, keyed on the row.
///
/// # Measured (JDK 25.0.4, linux; control `cvm-sbr-int14`, final `cvm-sbr-httpstack-2`)
///
/// ```text
///   suite (152):    152/152 both.  strict corpus: 0 divergent both
///   native-won:    400 -> 390 (the family's 13 rows -> 0)
///   SbrW5Http:     26 lines from HotSpot -> 1 (Net.connect's refused text); UNRETIRE = control;
///                  --compatible = control --compatible
///   SbrW6Https:    14 -> 0 (client vs a HotSpot HttpsServer); a custom-SSLContext
///                  HttpClient went from UnknownIssuer to working
///   SbrW5HttpBench us/req: huc.get 1068-1361 -> 2007-3122, huc.post 3054-3939 -> 3684-5174,
///                  hc.get 564-809 -> 6441-8802, hc.post 812-1980 -> 7322-8044 (HotSpot 278-606 /
///                  1295-1562 / 570-768 / 354-465). CPU-bound, not waiting; a maintainer call.
/// ```
///
/// Record: `docs/feature-designs/native-dispatch-architecture.md`.
/// `--compatible` is unchanged (the re-tag only; the native edit is behind `is_jdk_only()`).
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR_HTTPSTACK_TRIPLES: &[(&str, &str, &str)] = &[
    ("com/sun/net/httpserver/HttpServer", "create", "()Lcom/sun/net/httpserver/HttpServer;"),
    ("com/sun/net/httpserver/HttpServer", "create", "(Ljava/net/InetSocketAddress;I)Lcom/sun/net/httpserver/HttpServer;"),
    ("java/net/http/HttpClient", "awaitTermination", "(Ljava/time/Duration;)Z"),
    ("java/net/http/HttpClient", "close", "()V"),
    ("java/net/http/HttpClient", "isTerminated", "()Z"),
    ("java/net/http/HttpClient", "newBuilder", "()Ljava/net/http/HttpClient$Builder;"),
    ("java/net/http/HttpClient", "newHttpClient", "()Ljava/net/http/HttpClient;"),
    ("java/net/http/HttpClient", "shutdown", "()V"),
    ("java/net/http/HttpClient", "shutdownNow", "()V"),
    ("java/net/http/HttpHeaders", "allValues", "(Ljava/lang/String;)Ljava/util/List;"),
    ("java/net/http/HttpHeaders", "firstValue", "(Ljava/lang/String;)Ljava/util/Optional;"),
    ("java/net/http/HttpHeaders", "firstValueAsLong", "(Ljava/lang/String;)Ljava/util/OptionalLong;"),
    ("java/net/http/HttpHeaders", "map", "()Ljava/util/Map;"),
    ("java/net/http/HttpHeaders", "toString", "()Ljava/lang/String;"),
    ("java/net/http/HttpRequest", "newBuilder", "()Ljava/net/http/HttpRequest$Builder;"),
    ("java/net/http/HttpRequest", "newBuilder", "(Ljava/net/URI;)Ljava/net/http/HttpRequest$Builder;"),
    ("java/net/http/HttpRequest$BodyPublishers", "fromPublisher", "(Ljava/util/concurrent/Flow$Publisher;)Ljava/net/http/HttpRequest$BodyPublisher;"),
    ("java/net/http/HttpRequest$BodyPublishers", "fromPublisher", "(Ljava/util/concurrent/Flow$Publisher;J)Ljava/net/http/HttpRequest$BodyPublisher;"),
    ("java/net/http/HttpRequest$BodyPublishers", "noBody", "()Ljava/net/http/HttpRequest$BodyPublisher;"),
    ("java/net/http/HttpRequest$BodyPublishers", "ofByteArray", "([B)Ljava/net/http/HttpRequest$BodyPublisher;"),
    ("java/net/http/HttpRequest$BodyPublishers", "ofString", "(Ljava/lang/String;)Ljava/net/http/HttpRequest$BodyPublisher;"),
    ("java/net/http/HttpRequest$Builder", "HEAD", "()Ljava/net/http/HttpRequest$Builder;"),
    ("java/net/http/HttpResponse$BodyHandlers", "discarding", "()Ljava/net/http/HttpResponse$BodyHandler;"),
    ("java/net/http/HttpResponse$BodyHandlers", "ofByteArray", "()Ljava/net/http/HttpResponse$BodyHandler;"),
    ("java/net/http/HttpResponse$BodyHandlers", "ofInputStream", "()Ljava/net/http/HttpResponse$BodyHandler;"),
    ("java/net/http/HttpResponse$BodyHandlers", "ofString", "()Ljava/net/http/HttpResponse$BodyHandler;"),
    ("javax/net/ssl/HttpsURLConnection", "<init>", "(Ljava/net/URL;)V"),
    ("javax/net/ssl/HttpsURLConnection", "addRequestProperty", "(Ljava/lang/String;Ljava/lang/String;)V"),
    ("javax/net/ssl/HttpsURLConnection", "getContentLength", "()I"),
    ("javax/net/ssl/HttpsURLConnection", "getContentLengthLong", "()J"),
    ("javax/net/ssl/HttpsURLConnection", "getDefaultHostnameVerifier", "()Ljavax/net/ssl/HostnameVerifier;"),
    ("javax/net/ssl/HttpsURLConnection", "getDefaultSSLSocketFactory", "()Ljavax/net/ssl/SSLSocketFactory;"),
    ("javax/net/ssl/HttpsURLConnection", "getErrorStream", "()Ljava/io/InputStream;"),
    ("javax/net/ssl/HttpsURLConnection", "getHeaderField", "(I)Ljava/lang/String;"),
    ("javax/net/ssl/HttpsURLConnection", "getHeaderField", "(Ljava/lang/String;)Ljava/lang/String;"),
    ("javax/net/ssl/HttpsURLConnection", "getHeaderFieldKey", "(I)Ljava/lang/String;"),
    ("javax/net/ssl/HttpsURLConnection", "getHeaderFields", "()Ljava/util/Map;"),
    ("javax/net/ssl/HttpsURLConnection", "getHostnameVerifier", "()Ljavax/net/ssl/HostnameVerifier;"),
    ("javax/net/ssl/HttpsURLConnection", "getInputStream", "()Ljava/io/InputStream;"),
    ("javax/net/ssl/HttpsURLConnection", "getInstanceFollowRedirects", "()Z"),
    ("javax/net/ssl/HttpsURLConnection", "getLocalPrincipal", "()Ljava/security/Principal;"),
    ("javax/net/ssl/HttpsURLConnection", "getOutputStream", "()Ljava/io/OutputStream;"),
    ("javax/net/ssl/HttpsURLConnection", "getPeerPrincipal", "()Ljava/security/Principal;"),
    ("javax/net/ssl/HttpsURLConnection", "getRequestMethod", "()Ljava/lang/String;"),
    ("javax/net/ssl/HttpsURLConnection", "getRequestProperties", "()Ljava/util/Map;"),
    ("javax/net/ssl/HttpsURLConnection", "getRequestProperty", "(Ljava/lang/String;)Ljava/lang/String;"),
    ("javax/net/ssl/HttpsURLConnection", "getResponseCode", "()I"),
    ("javax/net/ssl/HttpsURLConnection", "getResponseMessage", "()Ljava/lang/String;"),
    ("javax/net/ssl/HttpsURLConnection", "getSSLSession", "()Ljava/util/Optional;"),
    ("javax/net/ssl/HttpsURLConnection", "getSSLSocketFactory", "()Ljavax/net/ssl/SSLSocketFactory;"),
    ("javax/net/ssl/HttpsURLConnection", "setChunkedStreamingMode", "(I)V"),
    ("javax/net/ssl/HttpsURLConnection", "setConnectTimeout", "(I)V"),
    ("javax/net/ssl/HttpsURLConnection", "setDefaultHostnameVerifier", "(Ljavax/net/ssl/HostnameVerifier;)V"),
    ("javax/net/ssl/HttpsURLConnection", "setDefaultSSLSocketFactory", "(Ljavax/net/ssl/SSLSocketFactory;)V"),
    ("javax/net/ssl/HttpsURLConnection", "setDoInput", "(Z)V"),
    ("javax/net/ssl/HttpsURLConnection", "setDoOutput", "(Z)V"),
    ("javax/net/ssl/HttpsURLConnection", "setFixedLengthStreamingMode", "(I)V"),
    ("javax/net/ssl/HttpsURLConnection", "setFixedLengthStreamingMode", "(J)V"),
    ("javax/net/ssl/HttpsURLConnection", "setHostnameVerifier", "(Ljavax/net/ssl/HostnameVerifier;)V"),
    ("javax/net/ssl/HttpsURLConnection", "setInstanceFollowRedirects", "(Z)V"),
    ("javax/net/ssl/HttpsURLConnection", "setReadTimeout", "(I)V"),
    ("javax/net/ssl/HttpsURLConnection", "setRequestMethod", "(Ljava/lang/String;)V"),
    ("javax/net/ssl/HttpsURLConnection", "setRequestProperty", "(Ljava/lang/String;Ljava/lang/String;)V"),
    ("javax/net/ssl/HttpsURLConnection", "setSSLSocketFactory", "(Ljavax/net/ssl/SSLSocketFactory;)V"),
    ("jdk/internal/net/http/Http1HeaderParser", "<init>", "()V"),
    ("jdk/internal/net/http/Http2ClientImpl", "<init>", "(Ljdk/internal/net/http/HttpClientImpl;)V"),
    ("jdk/internal/net/http/HttpClientImpl", "authenticator", "()Ljava/util/Optional;"),
    ("jdk/internal/net/http/HttpClientImpl", "connectTimeout", "()Ljava/util/Optional;"),
    ("jdk/internal/net/http/HttpClientImpl", "cookieHandler", "()Ljava/util/Optional;"),
    ("jdk/internal/net/http/HttpClientImpl", "executor", "()Ljava/util/Optional;"),
    ("jdk/internal/net/http/HttpClientImpl", "followRedirects", "()Ljava/net/http/HttpClient$Redirect;"),
    ("jdk/internal/net/http/HttpClientImpl", "proxy", "()Ljava/util/Optional;"),
    ("jdk/internal/net/http/HttpClientImpl", "send", "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/net/http/HttpResponse;"),
    ("jdk/internal/net/http/HttpClientImpl", "sendAsync", "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/util/concurrent/CompletableFuture;"),
    ("jdk/internal/net/http/HttpClientImpl", "sslContext", "()Ljavax/net/ssl/SSLContext;"),
    ("jdk/internal/net/http/HttpClientImpl", "version", "()Ljava/net/http/HttpClient$Version;"),
    ("jdk/internal/net/http/HttpRequestImpl", "bodyPublisher", "()Ljava/util/Optional;"),
    ("jdk/internal/net/http/HttpRequestImpl", "expectContinue", "()Z"),
    ("jdk/internal/net/http/HttpRequestImpl", "headers", "()Ljava/net/http/HttpHeaders;"),
    ("jdk/internal/net/http/HttpRequestImpl", "method", "()Ljava/lang/String;"),
    ("jdk/internal/net/http/HttpRequestImpl", "timeout", "()Ljava/util/Optional;"),
    ("jdk/internal/net/http/HttpRequestImpl", "uri", "()Ljava/net/URI;"),
    ("jdk/internal/net/http/HttpRequestImpl", "version", "()Ljava/util/Optional;"),
    ("jdk/internal/net/http/HttpResponseImpl", "body", "()Ljava/lang/Object;"),
    ("jdk/internal/net/http/HttpResponseImpl", "headers", "()Ljava/net/http/HttpHeaders;"),
    ("jdk/internal/net/http/HttpResponseImpl", "previousResponse", "()Ljava/util/Optional;"),
    ("jdk/internal/net/http/HttpResponseImpl", "request", "()Ljava/net/http/HttpRequest;"),
    ("jdk/internal/net/http/HttpResponseImpl", "statusCode", "()I"),
    ("jdk/internal/net/http/HttpResponseImpl", "uri", "()Ljava/net/URI;"),
    ("jdk/internal/net/http/HttpResponseImpl", "version", "()Ljava/net/http/HttpClient$Version;"),
    ("sun/net/httpserver/HttpContextImpl", "getAttributes", "()Ljava/util/Map;"),
    ("sun/net/httpserver/HttpContextImpl", "getAuthenticator", "()Lcom/sun/net/httpserver/Authenticator;"),
    ("sun/net/httpserver/HttpContextImpl", "getHandler", "()Lcom/sun/net/httpserver/HttpHandler;"),
    ("sun/net/httpserver/HttpContextImpl", "getPath", "()Ljava/lang/String;"),
    ("sun/net/httpserver/HttpContextImpl", "getServer", "()Lcom/sun/net/httpserver/HttpServer;"),
    ("sun/net/httpserver/HttpContextImpl", "setAuthenticator", "(Lcom/sun/net/httpserver/Authenticator;)Lcom/sun/net/httpserver/Authenticator;"),
    ("sun/net/httpserver/HttpServerImpl", "bind", "(Ljava/net/InetSocketAddress;I)V"),
    ("sun/net/httpserver/HttpServerImpl", "create", "()Lcom/sun/net/httpserver/HttpServer;"),
    ("sun/net/httpserver/HttpServerImpl", "create", "(Ljava/net/InetSocketAddress;I)Lcom/sun/net/httpserver/HttpServer;"),
    ("sun/net/httpserver/HttpServerImpl", "createContext", "(Ljava/lang/String;Lcom/sun/net/httpserver/HttpHandler;)Lcom/sun/net/httpserver/HttpContext;"),
    ("sun/net/httpserver/HttpServerImpl", "getAddress", "()Ljava/net/InetSocketAddress;"),
    ("sun/net/httpserver/HttpServerImpl", "getExecutor", "()Ljava/util/concurrent/Executor;"),
    ("sun/net/httpserver/HttpServerImpl", "removeContext", "(Lcom/sun/net/httpserver/HttpContext;)V"),
    ("sun/net/httpserver/HttpServerImpl", "removeContext", "(Ljava/lang/String;)V"),
    ("sun/net/httpserver/HttpServerImpl", "setExecutor", "(Ljava/util/concurrent/Executor;)V"),
    ("sun/net/httpserver/HttpServerImpl", "start", "()V"),
    ("sun/net/httpserver/HttpServerImpl", "stop", "(I)V"),
    ("sun/net/spi/DefaultProxySelector", "connectFailed", "(Ljava/net/URI;Ljava/net/SocketAddress;Ljava/io/IOException;)V"),
    ("sun/net/spi/DefaultProxySelector", "select", "(Ljava/net/URI;)Ljava/util/List;"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "addRequestProperty", "(Ljava/lang/String;Ljava/lang/String;)V"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "connect", "()V"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "disconnect", "()V"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "equals", "(Ljava/lang/Object;)Z"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getAllowUserInteraction", "()Z"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getCipherSuite", "()Ljava/lang/String;"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getConnectTimeout", "()I"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getContent", "()Ljava/lang/Object;"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getContent", "([Ljava/lang/Class;)Ljava/lang/Object;"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getContentEncoding", "()Ljava/lang/String;"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getContentLength", "()I"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getContentLengthLong", "()J"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getContentType", "()Ljava/lang/String;"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getDate", "()J"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getDefaultUseCaches", "()Z"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getDoInput", "()Z"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getDoOutput", "()Z"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getErrorStream", "()Ljava/io/InputStream;"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getExpiration", "()J"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getHeaderField", "(I)Ljava/lang/String;"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getHeaderField", "(Ljava/lang/String;)Ljava/lang/String;"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getHeaderFieldDate", "(Ljava/lang/String;J)J"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getHeaderFieldInt", "(Ljava/lang/String;I)I"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getHeaderFieldKey", "(I)Ljava/lang/String;"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getHeaderFieldLong", "(Ljava/lang/String;J)J"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getHeaderFields", "()Ljava/util/Map;"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getIfModifiedSince", "()J"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getInputStream", "()Ljava/io/InputStream;"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getInstanceFollowRedirects", "()Z"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getLastModified", "()J"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getLocalCertificates", "()[Ljava/security/cert/Certificate;"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getLocalPrincipal", "()Ljava/security/Principal;"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getOutputStream", "()Ljava/io/OutputStream;"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getPeerPrincipal", "()Ljava/security/Principal;"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getPermission", "()Ljava/security/Permission;"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getReadTimeout", "()I"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getRequestMethod", "()Ljava/lang/String;"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getRequestProperties", "()Ljava/util/Map;"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getRequestProperty", "(Ljava/lang/String;)Ljava/lang/String;"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getResponseCode", "()I"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getResponseMessage", "()Ljava/lang/String;"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getSSLSession", "()Ljava/util/Optional;"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getServerCertificates", "()[Ljava/security/cert/Certificate;"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getURL", "()Ljava/net/URL;"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "getUseCaches", "()Z"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "hashCode", "()I"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "isConnected", "()Z"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "setAllowUserInteraction", "(Z)V"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "setAuthenticator", "(Ljava/net/Authenticator;)V"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "setChunkedStreamingMode", "(I)V"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "setConnectTimeout", "(I)V"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "setConnected", "(Z)V"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "setDefaultUseCaches", "(Z)V"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "setDoInput", "(Z)V"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "setDoOutput", "(Z)V"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "setFixedLengthStreamingMode", "(I)V"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "setFixedLengthStreamingMode", "(J)V"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "setIfModifiedSince", "(J)V"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "setInstanceFollowRedirects", "(Z)V"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "setReadTimeout", "(I)V"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "setRequestMethod", "(Ljava/lang/String;)V"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "setRequestProperty", "(Ljava/lang/String;Ljava/lang/String;)V"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "setUseCaches", "(Z)V"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "toString", "()Ljava/lang/String;"),
    ("sun/net/www/protocol/https/HttpsURLConnectionImpl", "usingProxy", "()Z"),
];

/// SBR wave 6, lane `asyncnet` (2026-09-29): the asynchronous half of NIO wave
/// 5 (`RETIRED_SHADOW_SBR_NIOREAL_TRIPLES`) held -- the async socket and server
/// channels, the channel groups, `AsynchronousFileChannel`, `FileLockImpl`,
/// `newAsynchronousFileChannel` on the providers, `UnixFileSystem.getFileStores`
/// -- retired over the real `sun.nio.ch` layer. 125 triples over nineteen
/// classes, 132 `Bridge` registrations re-tagged. No new prefix: `java/nio/`,
/// `sun/nio/ch/` and `sun/nio/fs/` are all admitted.
///
/// # What had to exist first
///
/// Wave 5's list was `IOUtil.makePipe`/`write1`/`drain1`; the measurement
/// (dial arm, `cvm-sbr-int14` + `CRATONVM_ENFORCE_NATIVE_SHADOW`) confirmed it
/// was the ONLY thing missing: every probe row died on
/// `UnsatisfiedLinkError: IOUtil.makePipe` and nothing else. Four genuine
/// `ACC_NATIVE` methods (the same four on JDK 17, 21 and 25; `javap -p` on all
/// three) now stand under the real `EPollPort` and
/// `UnixAsynchronousSocketChannelImpl`, registered in BOTH modes, each with
/// its kind stated (`native-io/src/net_nio_real.rs`):
///
/// * `IOUtil.makePipe(Z)J`, `write1(IB)I`, `drain1(I)I` -- real `pipe(2)`
///   descriptors (small, outside the `net_sockets()` id range, exactly as the
///   selector's `EventFD` already is), so `EPoll.ctl` passes them through and
///   `FileDispatcherImpl.closeIntFD` closes them; libnio's return codes
///   (`IOS_UNAVAILABLE`, `IOS_INTERRUPTED`, `IOException`).
/// * `UnixAsynchronousSocketChannelImpl.checkConnect(I)V` -- `SO_ERROR` off
///   the socket behind the Java id (`nio_os_fd`), thrown through libnio's
///   `handleSocketError` mapping.
///
/// Everything else the real bytecode ends in already existed: NIOREAL's real
/// `Net`, `EPoll.ctl`/`wait` (ids stay virtual, the kernel sees the real fd),
/// `FileDispatcherImpl`/`UnixNativeDispatcher` under the real file channel,
/// the mount natives FS2 added (`getFileStores`).
///
/// # Why it is safe
///
/// The state under every class is real: nothing in this table mints an object
/// without `<init>` any more, because the natives that did (`aio_asc_open`,
/// `aio_assc_open`, `alloc_afc_channel`, ...) are rows of this table, so the
/// JDK's own provider builds the channels. The abstract-class rows
/// (`java/nio/channels/Asynchronous*` methods with no body, 43 of them) stay
/// registered and are NOT reached for a real receiver: dispatch keys on the
/// receiver's runtime class, measured with a receiver-yield-free build (no
/// yield was needed in `native_afc_provider_open` -- the wave-5 FS2 record
/// expected one). Only rows with `Code` on the JDK 25 image (declared or
/// inherited, `ShadowTripleClassifier`) are in the table: 180 candidate
/// `bridge` rows, 125 taken, 43 abstract, 7 declared by no image, 3
/// `ACC_NATIVE` (`NativeThread.current0`/`init`/`supportPendingSignals0`).
///
/// # Measured (`cvm-sbr-int14` control, `cvm-sbr-asyncnet-*`, HotSpot 25.0.4)
///
/// * `SbrW6Asyncnet` (45 rows, first-run value + 30 hot iterations): control
///   `--real-jdk` differs from HotSpot on 40 of 45 rows (the `Future` form of
///   `accept()` is `UnsupportedOperationException: not implemented by
///   CratonVM`, `connect` twice succeeds, `connect` to a closed port is an
///   `IOException` with a Rust error text, `getOption` NPEs, closed-channel
///   operations answer never-completing `CompletableFuture`s, `AsynchronousChannelGroup`
///   is `null`-provided and never terminates, `AsynchronousFileChannel.open(..,
///   CREATE, ..)` ignores `CREATE` (`NoSuchFileException: ""`), the handler
///   thread is the caller, `getFileStores()` is EMPTY, `FileLock.isValid`
///   survives a channel close); the retired build differs on the rows recorded
///   in the lane's record (the two `vt.*` rows are a pre-existing virtual-thread
///   defect the control has too).
/// * Full suite `SUITE=all --real-jdk`: 152/152; strict corpus: see the record.
/// * Cost, one host, interleaved (`SbrW6AsyncnetPerf`): the retired path is
///   slower than the native carrier on a 1-byte round trip and equal on
///   throughput; see the record. HELD on speed would have kept a carrier that
///   is wrong on 40 of 45 probe rows, so the rule "retire anyway when the
///   NATIVE gives wrong answers" applies. `CRATONVM_UNRETIRE_NATIVE_SHADOW`
///   over the table's classes restores the old path (armed: 125 rows,
///   measured to reproduce the control).
///
/// # Left out, and why
///
/// * `NativeThread.current()J` and `signal(J)V`: `ACC_NATIVE` on JDK 17 (the
///   tables are version-blind: a `SyntheticStub` there is dropped and becomes
///   an `UnsatisfiedLinkError`). `current` is a reviewed `Intrinsic` with the
///   JDK 21/25 body's exact answer instead (`-1` for a virtual thread, the
///   platform id otherwise); the old native answered a positive id for a
///   virtual thread.
/// * The 43 abstract `java/nio/channels/Asynchronous*` /
///   `java/nio/file/FileSystem.getFileStores` rows: no body to yield to.
/// * The Windows/BSD/macOS/Solaris/AIX async classes and providers: their
///   real bodies end in IOCP/kqueue/event-port natives this VM does not have.
///   MEASURED ON LINUX/x86_64 ONLY (key `25/linux`); like every table before
///   it this one is version- and OS-blind, so a Windows `--real-jdk` run of the
///   shared `java/nio/channels` factories would reach the Windows classes.
///
/// # Unix only
///
/// The table is `#[cfg(unix)]`: the genuine natives it stands on (`IOUtil`
/// pipes, `checkConnect`, the real `Net`/`EPoll` layer) are Unix code, and a
/// non-Unix `--real-jdk` VM would otherwise hand the shared
/// `java/nio/channels/Asynchronous*` factories to the real IOCP classes, whose
/// natives this VM does not have. On other targets the table is empty and the
/// native carriers keep serving, exactly as before this lane.
///
/// Record: `docs/feature-designs/profile-guided-inlining.md`.
#[cfg(unix)]
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR_ASYNCNET_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/nio/channels/AsynchronousChannelGroup", "withCachedThreadPool", "(Ljava/util/concurrent/ExecutorService;I)Ljava/nio/channels/AsynchronousChannelGroup;"),
    ("java/nio/channels/AsynchronousChannelGroup", "withFixedThreadPool", "(ILjava/util/concurrent/ThreadFactory;)Ljava/nio/channels/AsynchronousChannelGroup;"),
    ("java/nio/channels/AsynchronousChannelGroup", "withThreadPool", "(Ljava/util/concurrent/ExecutorService;)Ljava/nio/channels/AsynchronousChannelGroup;"),
    ("java/nio/channels/AsynchronousFileChannel", "lock", "()Ljava/util/concurrent/Future;"),
    ("java/nio/channels/AsynchronousFileChannel", "open", "(Ljava/nio/file/Path;[Ljava/nio/file/OpenOption;)Ljava/nio/channels/AsynchronousFileChannel;"),
    ("java/nio/channels/AsynchronousServerSocketChannel", "bind", "(Ljava/net/SocketAddress;)Ljava/nio/channels/AsynchronousServerSocketChannel;"),
    ("java/nio/channels/AsynchronousServerSocketChannel", "open", "()Ljava/nio/channels/AsynchronousServerSocketChannel;"),
    ("java/nio/channels/AsynchronousServerSocketChannel", "open", "(Ljava/nio/channels/AsynchronousChannelGroup;)Ljava/nio/channels/AsynchronousServerSocketChannel;"),
    ("java/nio/channels/AsynchronousServerSocketChannel", "provider", "()Ljava/nio/channels/spi/AsynchronousChannelProvider;"),
    ("java/nio/channels/AsynchronousSocketChannel", "open", "()Ljava/nio/channels/AsynchronousSocketChannel;"),
    ("java/nio/channels/AsynchronousSocketChannel", "open", "(Ljava/nio/channels/AsynchronousChannelGroup;)Ljava/nio/channels/AsynchronousSocketChannel;"),
    ("java/nio/channels/AsynchronousSocketChannel", "provider", "()Ljava/nio/channels/spi/AsynchronousChannelProvider;"),
    ("java/nio/channels/AsynchronousSocketChannel", "read", "(Ljava/nio/ByteBuffer;Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("java/nio/channels/AsynchronousSocketChannel", "write", "(Ljava/nio/ByteBuffer;Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("java/nio/file/spi/FileSystemProvider", "newAsynchronousFileChannel", "(Ljava/nio/file/Path;Ljava/util/Set;Ljava/util/concurrent/ExecutorService;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/channels/AsynchronousFileChannel;"),
    ("sun/nio/ch/AsynchronousChannelGroupImpl", "awaitTermination", "(JLjava/util/concurrent/TimeUnit;)Z"),
    ("sun/nio/ch/AsynchronousChannelGroupImpl", "isShutdown", "()Z"),
    ("sun/nio/ch/AsynchronousChannelGroupImpl", "isTerminated", "()Z"),
    ("sun/nio/ch/AsynchronousChannelGroupImpl", "shutdown", "()V"),
    ("sun/nio/ch/AsynchronousChannelGroupImpl", "shutdownNow", "()V"),
    ("sun/nio/ch/AsynchronousChannelGroupImpl", "withCachedThreadPool", "(Ljava/util/concurrent/ExecutorService;I)Ljava/nio/channels/AsynchronousChannelGroup;"),
    ("sun/nio/ch/AsynchronousChannelGroupImpl", "withFixedThreadPool", "(ILjava/util/concurrent/ThreadFactory;)Ljava/nio/channels/AsynchronousChannelGroup;"),
    ("sun/nio/ch/AsynchronousChannelGroupImpl", "withThreadPool", "(Ljava/util/concurrent/ExecutorService;)Ljava/nio/channels/AsynchronousChannelGroup;"),
    ("sun/nio/ch/AsynchronousFileChannelImpl", "isOpen", "()Z"),
    ("sun/nio/ch/AsynchronousFileChannelImpl", "lock", "()Ljava/util/concurrent/Future;"),
    ("sun/nio/ch/AsynchronousFileChannelImpl", "lock", "(JJZ)Ljava/util/concurrent/Future;"),
    ("sun/nio/ch/AsynchronousFileChannelImpl", "open", "(Ljava/nio/file/Path;[Ljava/nio/file/OpenOption;)Ljava/nio/channels/AsynchronousFileChannel;"),
    ("sun/nio/ch/AsynchronousFileChannelImpl", "read", "(Ljava/nio/ByteBuffer;J)Ljava/util/concurrent/Future;"),
    ("sun/nio/ch/AsynchronousFileChannelImpl", "read", "(Ljava/nio/ByteBuffer;JLjava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("sun/nio/ch/AsynchronousFileChannelImpl", "write", "(Ljava/nio/ByteBuffer;J)Ljava/util/concurrent/Future;"),
    ("sun/nio/ch/AsynchronousFileChannelImpl", "write", "(Ljava/nio/ByteBuffer;JLjava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("sun/nio/ch/AsynchronousServerSocketChannelImpl", "accept", "()Ljava/util/concurrent/Future;"),
    ("sun/nio/ch/AsynchronousServerSocketChannelImpl", "accept", "(Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("sun/nio/ch/AsynchronousServerSocketChannelImpl", "bind", "(Ljava/net/SocketAddress;)Ljava/nio/channels/AsynchronousServerSocketChannel;"),
    ("sun/nio/ch/AsynchronousServerSocketChannelImpl", "bind", "(Ljava/net/SocketAddress;I)Ljava/nio/channels/AsynchronousServerSocketChannel;"),
    ("sun/nio/ch/AsynchronousServerSocketChannelImpl", "close", "()V"),
    ("sun/nio/ch/AsynchronousServerSocketChannelImpl", "getLocalAddress", "()Ljava/net/SocketAddress;"),
    ("sun/nio/ch/AsynchronousServerSocketChannelImpl", "getOption", "(Ljava/net/SocketOption;)Ljava/lang/Object;"),
    ("sun/nio/ch/AsynchronousServerSocketChannelImpl", "isOpen", "()Z"),
    ("sun/nio/ch/AsynchronousServerSocketChannelImpl", "open", "()Ljava/nio/channels/AsynchronousServerSocketChannel;"),
    ("sun/nio/ch/AsynchronousServerSocketChannelImpl", "open", "(Ljava/nio/channels/AsynchronousChannelGroup;)Ljava/nio/channels/AsynchronousServerSocketChannel;"),
    ("sun/nio/ch/AsynchronousServerSocketChannelImpl", "provider", "()Ljava/nio/channels/spi/AsynchronousChannelProvider;"),
    ("sun/nio/ch/AsynchronousServerSocketChannelImpl", "setOption", "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/AsynchronousServerSocketChannel;"),
    ("sun/nio/ch/AsynchronousServerSocketChannelImpl", "supportedOptions", "()Ljava/util/Set;"),
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "close", "()V"),
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "connect", "(Ljava/net/SocketAddress;)Ljava/util/concurrent/Future;"),
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "connect", "(Ljava/net/SocketAddress;Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "getOption", "(Ljava/net/SocketOption;)Ljava/lang/Object;"),
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "getRemoteAddress", "()Ljava/net/SocketAddress;"),
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "isOpen", "()Z"),
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "open", "()Ljava/nio/channels/AsynchronousSocketChannel;"),
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "open", "(Ljava/nio/channels/AsynchronousChannelGroup;)Ljava/nio/channels/AsynchronousSocketChannel;"),
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "provider", "()Ljava/nio/channels/spi/AsynchronousChannelProvider;"),
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "read", "(Ljava/nio/ByteBuffer;)Ljava/util/concurrent/Future;"),
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "read", "(Ljava/nio/ByteBuffer;JLjava/util/concurrent/TimeUnit;Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "read", "(Ljava/nio/ByteBuffer;Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "setOption", "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/AsynchronousSocketChannel;"),
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "supportedOptions", "()Ljava/util/Set;"),
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "write", "(Ljava/nio/ByteBuffer;)Ljava/util/concurrent/Future;"),
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "write", "(Ljava/nio/ByteBuffer;JLjava/util/concurrent/TimeUnit;Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "write", "(Ljava/nio/ByteBuffer;Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("sun/nio/ch/EPollPort", "awaitTermination", "(JLjava/util/concurrent/TimeUnit;)Z"),
    ("sun/nio/ch/EPollPort", "isShutdown", "()Z"),
    ("sun/nio/ch/EPollPort", "isTerminated", "()Z"),
    ("sun/nio/ch/EPollPort", "shutdown", "()V"),
    ("sun/nio/ch/EPollPort", "shutdownNow", "()V"),
    ("sun/nio/ch/EPollPort", "withCachedThreadPool", "(Ljava/util/concurrent/ExecutorService;I)Ljava/nio/channels/AsynchronousChannelGroup;"),
    ("sun/nio/ch/EPollPort", "withFixedThreadPool", "(ILjava/util/concurrent/ThreadFactory;)Ljava/nio/channels/AsynchronousChannelGroup;"),
    ("sun/nio/ch/EPollPort", "withThreadPool", "(Ljava/util/concurrent/ExecutorService;)Ljava/nio/channels/AsynchronousChannelGroup;"),
    ("sun/nio/ch/FileLockImpl", "release", "()V"),
    ("sun/nio/ch/NativeDispatcher", "preClose", "(Ljava/io/FileDescriptor;JJ)V"),
    ("sun/nio/ch/Port", "awaitTermination", "(JLjava/util/concurrent/TimeUnit;)Z"),
    ("sun/nio/ch/Port", "isShutdown", "()Z"),
    ("sun/nio/ch/Port", "isTerminated", "()Z"),
    ("sun/nio/ch/Port", "shutdown", "()V"),
    ("sun/nio/ch/Port", "shutdownNow", "()V"),
    ("sun/nio/ch/Port", "withCachedThreadPool", "(Ljava/util/concurrent/ExecutorService;I)Ljava/nio/channels/AsynchronousChannelGroup;"),
    ("sun/nio/ch/Port", "withFixedThreadPool", "(ILjava/util/concurrent/ThreadFactory;)Ljava/nio/channels/AsynchronousChannelGroup;"),
    ("sun/nio/ch/Port", "withThreadPool", "(Ljava/util/concurrent/ExecutorService;)Ljava/nio/channels/AsynchronousChannelGroup;"),
    ("sun/nio/ch/SimpleAsynchronousFileChannelImpl", "close", "()V"),
    ("sun/nio/ch/SimpleAsynchronousFileChannelImpl", "force", "(Z)V"),
    ("sun/nio/ch/SimpleAsynchronousFileChannelImpl", "isOpen", "()Z"),
    ("sun/nio/ch/SimpleAsynchronousFileChannelImpl", "lock", "()Ljava/util/concurrent/Future;"),
    ("sun/nio/ch/SimpleAsynchronousFileChannelImpl", "lock", "(JJZ)Ljava/util/concurrent/Future;"),
    ("sun/nio/ch/SimpleAsynchronousFileChannelImpl", "open", "(Ljava/nio/file/Path;[Ljava/nio/file/OpenOption;)Ljava/nio/channels/AsynchronousFileChannel;"),
    ("sun/nio/ch/SimpleAsynchronousFileChannelImpl", "read", "(Ljava/nio/ByteBuffer;J)Ljava/util/concurrent/Future;"),
    ("sun/nio/ch/SimpleAsynchronousFileChannelImpl", "read", "(Ljava/nio/ByteBuffer;JLjava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("sun/nio/ch/SimpleAsynchronousFileChannelImpl", "size", "()J"),
    ("sun/nio/ch/SimpleAsynchronousFileChannelImpl", "truncate", "(J)Ljava/nio/channels/AsynchronousFileChannel;"),
    ("sun/nio/ch/SimpleAsynchronousFileChannelImpl", "tryLock", "(JJZ)Ljava/nio/channels/FileLock;"),
    ("sun/nio/ch/SimpleAsynchronousFileChannelImpl", "write", "(Ljava/nio/ByteBuffer;J)Ljava/util/concurrent/Future;"),
    ("sun/nio/ch/SimpleAsynchronousFileChannelImpl", "write", "(Ljava/nio/ByteBuffer;JLjava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("sun/nio/ch/UnixAsynchronousServerSocketChannelImpl", "accept", "()Ljava/util/concurrent/Future;"),
    ("sun/nio/ch/UnixAsynchronousServerSocketChannelImpl", "accept", "(Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("sun/nio/ch/UnixAsynchronousServerSocketChannelImpl", "bind", "(Ljava/net/SocketAddress;)Ljava/nio/channels/AsynchronousServerSocketChannel;"),
    ("sun/nio/ch/UnixAsynchronousServerSocketChannelImpl", "bind", "(Ljava/net/SocketAddress;I)Ljava/nio/channels/AsynchronousServerSocketChannel;"),
    ("sun/nio/ch/UnixAsynchronousServerSocketChannelImpl", "close", "()V"),
    ("sun/nio/ch/UnixAsynchronousServerSocketChannelImpl", "getLocalAddress", "()Ljava/net/SocketAddress;"),
    ("sun/nio/ch/UnixAsynchronousServerSocketChannelImpl", "getOption", "(Ljava/net/SocketOption;)Ljava/lang/Object;"),
    ("sun/nio/ch/UnixAsynchronousServerSocketChannelImpl", "isOpen", "()Z"),
    ("sun/nio/ch/UnixAsynchronousServerSocketChannelImpl", "open", "()Ljava/nio/channels/AsynchronousServerSocketChannel;"),
    ("sun/nio/ch/UnixAsynchronousServerSocketChannelImpl", "open", "(Ljava/nio/channels/AsynchronousChannelGroup;)Ljava/nio/channels/AsynchronousServerSocketChannel;"),
    ("sun/nio/ch/UnixAsynchronousServerSocketChannelImpl", "provider", "()Ljava/nio/channels/spi/AsynchronousChannelProvider;"),
    ("sun/nio/ch/UnixAsynchronousServerSocketChannelImpl", "setOption", "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/AsynchronousServerSocketChannel;"),
    ("sun/nio/ch/UnixAsynchronousServerSocketChannelImpl", "supportedOptions", "()Ljava/util/Set;"),
    ("sun/nio/ch/UnixAsynchronousSocketChannelImpl", "close", "()V"),
    ("sun/nio/ch/UnixAsynchronousSocketChannelImpl", "connect", "(Ljava/net/SocketAddress;)Ljava/util/concurrent/Future;"),
    ("sun/nio/ch/UnixAsynchronousSocketChannelImpl", "connect", "(Ljava/net/SocketAddress;Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("sun/nio/ch/UnixAsynchronousSocketChannelImpl", "getOption", "(Ljava/net/SocketOption;)Ljava/lang/Object;"),
    ("sun/nio/ch/UnixAsynchronousSocketChannelImpl", "getRemoteAddress", "()Ljava/net/SocketAddress;"),
    ("sun/nio/ch/UnixAsynchronousSocketChannelImpl", "isOpen", "()Z"),
    ("sun/nio/ch/UnixAsynchronousSocketChannelImpl", "open", "()Ljava/nio/channels/AsynchronousSocketChannel;"),
    ("sun/nio/ch/UnixAsynchronousSocketChannelImpl", "open", "(Ljava/nio/channels/AsynchronousChannelGroup;)Ljava/nio/channels/AsynchronousSocketChannel;"),
    ("sun/nio/ch/UnixAsynchronousSocketChannelImpl", "provider", "()Ljava/nio/channels/spi/AsynchronousChannelProvider;"),
    ("sun/nio/ch/UnixAsynchronousSocketChannelImpl", "read", "(Ljava/nio/ByteBuffer;)Ljava/util/concurrent/Future;"),
    ("sun/nio/ch/UnixAsynchronousSocketChannelImpl", "read", "(Ljava/nio/ByteBuffer;JLjava/util/concurrent/TimeUnit;Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("sun/nio/ch/UnixAsynchronousSocketChannelImpl", "read", "(Ljava/nio/ByteBuffer;Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("sun/nio/ch/UnixAsynchronousSocketChannelImpl", "setOption", "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/AsynchronousSocketChannel;"),
    ("sun/nio/ch/UnixAsynchronousSocketChannelImpl", "supportedOptions", "()Ljava/util/Set;"),
    ("sun/nio/ch/UnixAsynchronousSocketChannelImpl", "write", "(Ljava/nio/ByteBuffer;)Ljava/util/concurrent/Future;"),
    ("sun/nio/ch/UnixAsynchronousSocketChannelImpl", "write", "(Ljava/nio/ByteBuffer;JLjava/util/concurrent/TimeUnit;Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("sun/nio/ch/UnixAsynchronousSocketChannelImpl", "write", "(Ljava/nio/ByteBuffer;Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("sun/nio/fs/LinuxFileSystemProvider", "newAsynchronousFileChannel", "(Ljava/nio/file/Path;Ljava/util/Set;Ljava/util/concurrent/ExecutorService;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/channels/AsynchronousFileChannel;"),
    ("sun/nio/fs/UnixFileSystem", "getFileStores", "()Ljava/lang/Iterable;"),
    ("sun/nio/fs/UnixFileSystemProvider", "newAsynchronousFileChannel", "(Ljava/nio/file/Path;Ljava/util/Set;Ljava/util/concurrent/ExecutorService;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/channels/AsynchronousFileChannel;"),
];

#[cfg(not(unix))]
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR_ASYNCNET_TRIPLES: &[(&str, &str, &str)] = &[];

/// Windows: `RETIRED_SHADOW_SBR_ASYNCNET_TRIPLES` over the Windows classes -- the table above with
/// `UnixAsynchronous{Server,}SocketChannelImpl` -> `WindowsAsynchronous{Server,}SocketChannelImpl`,
/// `EPollPort` -> `Iocp`, `SimpleAsynchronousFileChannelImpl` -> `WindowsAsynchronousFileChannelImpl` and
/// `{Unix,Linux}FileSystemProvider.newAsynchronousFileChannel` -> `WindowsFileSystemProvider`'s (the
/// Windows `UnixFileSystem.getFileStores` row is the file-system family's, not this one's). Added to the
/// row set by `retired_shadow_row_set` only with the Windows channel layer on: the JDK's classes then run
/// over `native-io/src/net_iocp_win.rs` (the `Iocp` / `WindowsAsynchronous*ChannelImpl` natives), and the
/// carriers (`async_socket.rs`, `lib.rs` `AFC_IMPLS`) stay registered for the layer-off VM, as the NIOREAL
/// Windows table does. The four `Iocp` facade rows the carrier also registers (`open`, `close`, `drain`,
/// `poll`) name methods the image does not declare, like the Linux `EPollPort` ones, and are left.
/// Witness: `WinAsync.java` (both modes against HotSpot).
#[cfg(windows)]
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR_ASYNCNET_WINDOWS_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/nio/channels/AsynchronousChannelGroup", "withCachedThreadPool", "(Ljava/util/concurrent/ExecutorService;I)Ljava/nio/channels/AsynchronousChannelGroup;"),
    ("java/nio/channels/AsynchronousChannelGroup", "withFixedThreadPool", "(ILjava/util/concurrent/ThreadFactory;)Ljava/nio/channels/AsynchronousChannelGroup;"),
    ("java/nio/channels/AsynchronousChannelGroup", "withThreadPool", "(Ljava/util/concurrent/ExecutorService;)Ljava/nio/channels/AsynchronousChannelGroup;"),
    ("java/nio/channels/AsynchronousFileChannel", "lock", "()Ljava/util/concurrent/Future;"),
    ("java/nio/channels/AsynchronousFileChannel", "open", "(Ljava/nio/file/Path;[Ljava/nio/file/OpenOption;)Ljava/nio/channels/AsynchronousFileChannel;"),
    ("java/nio/channels/AsynchronousServerSocketChannel", "bind", "(Ljava/net/SocketAddress;)Ljava/nio/channels/AsynchronousServerSocketChannel;"),
    ("java/nio/channels/AsynchronousServerSocketChannel", "open", "()Ljava/nio/channels/AsynchronousServerSocketChannel;"),
    ("java/nio/channels/AsynchronousServerSocketChannel", "open", "(Ljava/nio/channels/AsynchronousChannelGroup;)Ljava/nio/channels/AsynchronousServerSocketChannel;"),
    ("java/nio/channels/AsynchronousServerSocketChannel", "provider", "()Ljava/nio/channels/spi/AsynchronousChannelProvider;"),
    ("java/nio/channels/AsynchronousSocketChannel", "open", "()Ljava/nio/channels/AsynchronousSocketChannel;"),
    ("java/nio/channels/AsynchronousSocketChannel", "open", "(Ljava/nio/channels/AsynchronousChannelGroup;)Ljava/nio/channels/AsynchronousSocketChannel;"),
    ("java/nio/channels/AsynchronousSocketChannel", "provider", "()Ljava/nio/channels/spi/AsynchronousChannelProvider;"),
    ("java/nio/channels/AsynchronousSocketChannel", "read", "(Ljava/nio/ByteBuffer;Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("java/nio/channels/AsynchronousSocketChannel", "write", "(Ljava/nio/ByteBuffer;Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("java/nio/file/spi/FileSystemProvider", "newAsynchronousFileChannel", "(Ljava/nio/file/Path;Ljava/util/Set;Ljava/util/concurrent/ExecutorService;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/channels/AsynchronousFileChannel;"),
    ("sun/nio/ch/AsynchronousChannelGroupImpl", "awaitTermination", "(JLjava/util/concurrent/TimeUnit;)Z"),
    ("sun/nio/ch/AsynchronousChannelGroupImpl", "isShutdown", "()Z"),
    ("sun/nio/ch/AsynchronousChannelGroupImpl", "isTerminated", "()Z"),
    ("sun/nio/ch/AsynchronousChannelGroupImpl", "shutdown", "()V"),
    ("sun/nio/ch/AsynchronousChannelGroupImpl", "shutdownNow", "()V"),
    ("sun/nio/ch/AsynchronousChannelGroupImpl", "withCachedThreadPool", "(Ljava/util/concurrent/ExecutorService;I)Ljava/nio/channels/AsynchronousChannelGroup;"),
    ("sun/nio/ch/AsynchronousChannelGroupImpl", "withFixedThreadPool", "(ILjava/util/concurrent/ThreadFactory;)Ljava/nio/channels/AsynchronousChannelGroup;"),
    ("sun/nio/ch/AsynchronousChannelGroupImpl", "withThreadPool", "(Ljava/util/concurrent/ExecutorService;)Ljava/nio/channels/AsynchronousChannelGroup;"),
    ("sun/nio/ch/AsynchronousFileChannelImpl", "isOpen", "()Z"),
    ("sun/nio/ch/AsynchronousFileChannelImpl", "lock", "()Ljava/util/concurrent/Future;"),
    ("sun/nio/ch/AsynchronousFileChannelImpl", "lock", "(JJZ)Ljava/util/concurrent/Future;"),
    ("sun/nio/ch/AsynchronousFileChannelImpl", "open", "(Ljava/nio/file/Path;[Ljava/nio/file/OpenOption;)Ljava/nio/channels/AsynchronousFileChannel;"),
    ("sun/nio/ch/AsynchronousFileChannelImpl", "read", "(Ljava/nio/ByteBuffer;J)Ljava/util/concurrent/Future;"),
    ("sun/nio/ch/AsynchronousFileChannelImpl", "read", "(Ljava/nio/ByteBuffer;JLjava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("sun/nio/ch/AsynchronousFileChannelImpl", "write", "(Ljava/nio/ByteBuffer;J)Ljava/util/concurrent/Future;"),
    ("sun/nio/ch/AsynchronousFileChannelImpl", "write", "(Ljava/nio/ByteBuffer;JLjava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("sun/nio/ch/AsynchronousServerSocketChannelImpl", "accept", "()Ljava/util/concurrent/Future;"),
    ("sun/nio/ch/AsynchronousServerSocketChannelImpl", "accept", "(Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("sun/nio/ch/AsynchronousServerSocketChannelImpl", "bind", "(Ljava/net/SocketAddress;)Ljava/nio/channels/AsynchronousServerSocketChannel;"),
    ("sun/nio/ch/AsynchronousServerSocketChannelImpl", "bind", "(Ljava/net/SocketAddress;I)Ljava/nio/channels/AsynchronousServerSocketChannel;"),
    ("sun/nio/ch/AsynchronousServerSocketChannelImpl", "close", "()V"),
    ("sun/nio/ch/AsynchronousServerSocketChannelImpl", "getLocalAddress", "()Ljava/net/SocketAddress;"),
    ("sun/nio/ch/AsynchronousServerSocketChannelImpl", "getOption", "(Ljava/net/SocketOption;)Ljava/lang/Object;"),
    ("sun/nio/ch/AsynchronousServerSocketChannelImpl", "isOpen", "()Z"),
    ("sun/nio/ch/AsynchronousServerSocketChannelImpl", "open", "()Ljava/nio/channels/AsynchronousServerSocketChannel;"),
    ("sun/nio/ch/AsynchronousServerSocketChannelImpl", "open", "(Ljava/nio/channels/AsynchronousChannelGroup;)Ljava/nio/channels/AsynchronousServerSocketChannel;"),
    ("sun/nio/ch/AsynchronousServerSocketChannelImpl", "provider", "()Ljava/nio/channels/spi/AsynchronousChannelProvider;"),
    ("sun/nio/ch/AsynchronousServerSocketChannelImpl", "setOption", "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/AsynchronousServerSocketChannel;"),
    ("sun/nio/ch/AsynchronousServerSocketChannelImpl", "supportedOptions", "()Ljava/util/Set;"),
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "close", "()V"),
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "connect", "(Ljava/net/SocketAddress;)Ljava/util/concurrent/Future;"),
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "connect", "(Ljava/net/SocketAddress;Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "getOption", "(Ljava/net/SocketOption;)Ljava/lang/Object;"),
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "getRemoteAddress", "()Ljava/net/SocketAddress;"),
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "isOpen", "()Z"),
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "open", "()Ljava/nio/channels/AsynchronousSocketChannel;"),
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "open", "(Ljava/nio/channels/AsynchronousChannelGroup;)Ljava/nio/channels/AsynchronousSocketChannel;"),
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "provider", "()Ljava/nio/channels/spi/AsynchronousChannelProvider;"),
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "read", "(Ljava/nio/ByteBuffer;)Ljava/util/concurrent/Future;"),
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "read", "(Ljava/nio/ByteBuffer;JLjava/util/concurrent/TimeUnit;Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "read", "(Ljava/nio/ByteBuffer;Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "setOption", "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/AsynchronousSocketChannel;"),
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "supportedOptions", "()Ljava/util/Set;"),
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "write", "(Ljava/nio/ByteBuffer;)Ljava/util/concurrent/Future;"),
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "write", "(Ljava/nio/ByteBuffer;JLjava/util/concurrent/TimeUnit;Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("sun/nio/ch/AsynchronousSocketChannelImpl", "write", "(Ljava/nio/ByteBuffer;Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("sun/nio/ch/FileLockImpl", "release", "()V"),
    ("sun/nio/ch/Iocp", "awaitTermination", "(JLjava/util/concurrent/TimeUnit;)Z"),
    ("sun/nio/ch/Iocp", "isShutdown", "()Z"),
    ("sun/nio/ch/Iocp", "isTerminated", "()Z"),
    ("sun/nio/ch/Iocp", "shutdown", "()V"),
    ("sun/nio/ch/Iocp", "shutdownNow", "()V"),
    ("sun/nio/ch/Iocp", "withCachedThreadPool", "(Ljava/util/concurrent/ExecutorService;I)Ljava/nio/channels/AsynchronousChannelGroup;"),
    ("sun/nio/ch/Iocp", "withFixedThreadPool", "(ILjava/util/concurrent/ThreadFactory;)Ljava/nio/channels/AsynchronousChannelGroup;"),
    ("sun/nio/ch/Iocp", "withThreadPool", "(Ljava/util/concurrent/ExecutorService;)Ljava/nio/channels/AsynchronousChannelGroup;"),
    ("sun/nio/ch/NativeDispatcher", "preClose", "(Ljava/io/FileDescriptor;JJ)V"),
    ("sun/nio/ch/Port", "awaitTermination", "(JLjava/util/concurrent/TimeUnit;)Z"),
    ("sun/nio/ch/Port", "isShutdown", "()Z"),
    ("sun/nio/ch/Port", "isTerminated", "()Z"),
    ("sun/nio/ch/Port", "shutdown", "()V"),
    ("sun/nio/ch/Port", "shutdownNow", "()V"),
    ("sun/nio/ch/Port", "withCachedThreadPool", "(Ljava/util/concurrent/ExecutorService;I)Ljava/nio/channels/AsynchronousChannelGroup;"),
    ("sun/nio/ch/Port", "withFixedThreadPool", "(ILjava/util/concurrent/ThreadFactory;)Ljava/nio/channels/AsynchronousChannelGroup;"),
    ("sun/nio/ch/Port", "withThreadPool", "(Ljava/util/concurrent/ExecutorService;)Ljava/nio/channels/AsynchronousChannelGroup;"),
    ("sun/nio/ch/WindowsAsynchronousFileChannelImpl", "close", "()V"),
    ("sun/nio/ch/WindowsAsynchronousFileChannelImpl", "force", "(Z)V"),
    ("sun/nio/ch/WindowsAsynchronousFileChannelImpl", "isOpen", "()Z"),
    ("sun/nio/ch/WindowsAsynchronousFileChannelImpl", "lock", "()Ljava/util/concurrent/Future;"),
    ("sun/nio/ch/WindowsAsynchronousFileChannelImpl", "lock", "(JJZ)Ljava/util/concurrent/Future;"),
    ("sun/nio/ch/WindowsAsynchronousFileChannelImpl", "open", "(Ljava/nio/file/Path;[Ljava/nio/file/OpenOption;)Ljava/nio/channels/AsynchronousFileChannel;"),
    ("sun/nio/ch/WindowsAsynchronousFileChannelImpl", "read", "(Ljava/nio/ByteBuffer;J)Ljava/util/concurrent/Future;"),
    ("sun/nio/ch/WindowsAsynchronousFileChannelImpl", "read", "(Ljava/nio/ByteBuffer;JLjava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("sun/nio/ch/WindowsAsynchronousFileChannelImpl", "size", "()J"),
    ("sun/nio/ch/WindowsAsynchronousFileChannelImpl", "truncate", "(J)Ljava/nio/channels/AsynchronousFileChannel;"),
    ("sun/nio/ch/WindowsAsynchronousFileChannelImpl", "tryLock", "(JJZ)Ljava/nio/channels/FileLock;"),
    ("sun/nio/ch/WindowsAsynchronousFileChannelImpl", "write", "(Ljava/nio/ByteBuffer;J)Ljava/util/concurrent/Future;"),
    ("sun/nio/ch/WindowsAsynchronousFileChannelImpl", "write", "(Ljava/nio/ByteBuffer;JLjava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("sun/nio/ch/WindowsAsynchronousServerSocketChannelImpl", "accept", "()Ljava/util/concurrent/Future;"),
    ("sun/nio/ch/WindowsAsynchronousServerSocketChannelImpl", "accept", "(Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("sun/nio/ch/WindowsAsynchronousServerSocketChannelImpl", "bind", "(Ljava/net/SocketAddress;)Ljava/nio/channels/AsynchronousServerSocketChannel;"),
    ("sun/nio/ch/WindowsAsynchronousServerSocketChannelImpl", "bind", "(Ljava/net/SocketAddress;I)Ljava/nio/channels/AsynchronousServerSocketChannel;"),
    ("sun/nio/ch/WindowsAsynchronousServerSocketChannelImpl", "close", "()V"),
    ("sun/nio/ch/WindowsAsynchronousServerSocketChannelImpl", "getLocalAddress", "()Ljava/net/SocketAddress;"),
    ("sun/nio/ch/WindowsAsynchronousServerSocketChannelImpl", "getOption", "(Ljava/net/SocketOption;)Ljava/lang/Object;"),
    ("sun/nio/ch/WindowsAsynchronousServerSocketChannelImpl", "isOpen", "()Z"),
    ("sun/nio/ch/WindowsAsynchronousServerSocketChannelImpl", "open", "()Ljava/nio/channels/AsynchronousServerSocketChannel;"),
    ("sun/nio/ch/WindowsAsynchronousServerSocketChannelImpl", "open", "(Ljava/nio/channels/AsynchronousChannelGroup;)Ljava/nio/channels/AsynchronousServerSocketChannel;"),
    ("sun/nio/ch/WindowsAsynchronousServerSocketChannelImpl", "provider", "()Ljava/nio/channels/spi/AsynchronousChannelProvider;"),
    ("sun/nio/ch/WindowsAsynchronousServerSocketChannelImpl", "setOption", "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/AsynchronousServerSocketChannel;"),
    ("sun/nio/ch/WindowsAsynchronousServerSocketChannelImpl", "supportedOptions", "()Ljava/util/Set;"),
    ("sun/nio/ch/WindowsAsynchronousSocketChannelImpl", "close", "()V"),
    ("sun/nio/ch/WindowsAsynchronousSocketChannelImpl", "connect", "(Ljava/net/SocketAddress;)Ljava/util/concurrent/Future;"),
    ("sun/nio/ch/WindowsAsynchronousSocketChannelImpl", "connect", "(Ljava/net/SocketAddress;Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("sun/nio/ch/WindowsAsynchronousSocketChannelImpl", "getOption", "(Ljava/net/SocketOption;)Ljava/lang/Object;"),
    ("sun/nio/ch/WindowsAsynchronousSocketChannelImpl", "getRemoteAddress", "()Ljava/net/SocketAddress;"),
    ("sun/nio/ch/WindowsAsynchronousSocketChannelImpl", "isOpen", "()Z"),
    ("sun/nio/ch/WindowsAsynchronousSocketChannelImpl", "open", "()Ljava/nio/channels/AsynchronousSocketChannel;"),
    ("sun/nio/ch/WindowsAsynchronousSocketChannelImpl", "open", "(Ljava/nio/channels/AsynchronousChannelGroup;)Ljava/nio/channels/AsynchronousSocketChannel;"),
    ("sun/nio/ch/WindowsAsynchronousSocketChannelImpl", "provider", "()Ljava/nio/channels/spi/AsynchronousChannelProvider;"),
    ("sun/nio/ch/WindowsAsynchronousSocketChannelImpl", "read", "(Ljava/nio/ByteBuffer;)Ljava/util/concurrent/Future;"),
    ("sun/nio/ch/WindowsAsynchronousSocketChannelImpl", "read", "(Ljava/nio/ByteBuffer;JLjava/util/concurrent/TimeUnit;Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("sun/nio/ch/WindowsAsynchronousSocketChannelImpl", "read", "(Ljava/nio/ByteBuffer;Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("sun/nio/ch/WindowsAsynchronousSocketChannelImpl", "setOption", "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/AsynchronousSocketChannel;"),
    ("sun/nio/ch/WindowsAsynchronousSocketChannelImpl", "supportedOptions", "()Ljava/util/Set;"),
    ("sun/nio/ch/WindowsAsynchronousSocketChannelImpl", "write", "(Ljava/nio/ByteBuffer;)Ljava/util/concurrent/Future;"),
    ("sun/nio/ch/WindowsAsynchronousSocketChannelImpl", "write", "(Ljava/nio/ByteBuffer;JLjava/util/concurrent/TimeUnit;Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("sun/nio/ch/WindowsAsynchronousSocketChannelImpl", "write", "(Ljava/nio/ByteBuffer;Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V"),
    ("sun/nio/fs/WindowsFileSystemProvider", "newAsynchronousFileChannel", "(Ljava/nio/file/Path;Ljava/util/Set;Ljava/util/concurrent/ExecutorService;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/channels/AsynchronousFileChannel;"),
];

/// 2026-09-29, SBR wave 6 lane MHLOOKUP: the `java.lang.invoke` rows whose STATE
/// is real, 22 triples (22 registrations, all `Bridge`).
///
/// | class | rows | why the state is real |
/// |---|---|---|
/// | `MethodType` | 6: `methodType` x4, `parameterArray`, `parameterType` | every `MethodType` a native hands out is already the JDK's interned instance: the four `methodType` natives ran `genericMethodType(0)` -> `insertParameterTypes` -> `changeReturnType` in real bytecode and returned its answer, i.e. they were a slower detour to the JDK's own factory. Real `methodType` is `makeImpl` directly |
/// | `MemberName` | 7: `getDeclaringClass`, `getFieldType`, `getMethodOrFieldType`, `getMethodType`, `getModifiers`, `getName`, `getReferenceKind` | the natives read slots 0..3 of the real six-field layout (`clazz`, `name`, `type`, `flags`), which `revealDirect` and `MethodHandleNatives.init` write |
/// | `MethodHandles$Lookup` | 5: `lookupClass`, `lookupModes`, `in`, `ensureInitialized`, `toString` | the four native mints write the real `lookupClass`/`prevLookupClass`/`allowedModes` fields |
/// | `MethodHandles` | 3: `lookup`, `publicLookup`, `privateLookupIn` | the same mints; retired, the JDK's own `new Lookup(caller)` / `PUBLIC_LOOKUP` runs |
/// | `MethodHandle` | 1: `type` | `return type;`; every carrier the VM mints writes the real `type` field |
///
/// **The natives were WRONG, in both modes** (`SbrW6Mhlookup.java`,
/// 218 rows, int14 vs HotSpot 25). Retired, every one of these rows equals
/// HotSpot: `Lookup.toString` answered the constant `MethodHandles.Lookup`
/// (HotSpot `SbrW6Mhlookup`, `SbrW6Mhlookup/package`, `java.lang.String/...`);
/// `publicLookup()` minted a new object per call (HotSpot: one singleton, `==`)
/// and `toString` said `MethodHandles.Lookup` instead of `java.lang.Object/publicLookup`;
/// `privateLookupIn(String.class, lookup)` SUCCEEDED where HotSpot refuses
/// (`module java.base does not open java.lang to unnamed module`) -- a security
/// answer; `Lookup.in(String.class).in(self)` answered modes 1 (HotSpot 0);
/// `ensureInitialized` skipped the whole access check (a `publicLookup` could
/// initialize a package-private class; an `int`/array target answered a class
/// or a private message); `in(array)`'s message lacked `class `;
/// `MethodType.methodType(int.class, (Class[]) null)` answered `()int` (HotSpot
/// NullPointerException). `--compatible` keeps the natives (a `SyntheticStub`
/// still registers there) except the fixes below.
///
/// **Mint fixes this needed.** `build_method_type_from_descriptor` (the mint under
/// `findStatic`/`revealDirect`/`Constructor` handles) wrote an untyped `Object[]` as
/// `ptypes`; the real `parameterArray()` is `ptypes.clone()` + `checkcast Class[]`,
/// so `revealDirect(mh).reflectAs(Method.class, ..)` threw `ClassCastException`
/// the moment the native yielded (found on the first trial build). It now
/// builds a `Class[]` (a `--compatible` change TOWARD HotSpot: `parameterArray`
/// there is the native, which never noticed).
///
/// **Measured** (record `docs/feature-designs/native-dispatch-architecture.md`):
/// faster: `methodType` 3.0-6.0 -> 1.3-2.5 us, `parameterType` 0.3 -> 0.03,
/// `lookupClass` 0.2 -> 0.03, `lookupModes` 0.2 -> 0.05, `publicLookup` 0.8 -> 0.03,
/// `MethodHandle.type` 0.45 -> 0.035, `lookup()` 1.7 -> 1.0-1.5. Slower and
/// CORRECT-WHERE-THE-NATIVE-WAS-WRONG (retired under the "native gives wrong
/// answers" arm of the speed rule): `Lookup.in` 1.5 -> 8-13 us, `ensureInitialized`
/// 0.25 -> 1.4, `privateLookupIn` 1.1 -> 3.6, `toString` 0.15 -> 0.7 (HotSpot
/// is below 0.1 for each; these are class-init-time calls). On JDK 25 a
/// large share of `in`'s cost was a `NoSuchMethodError` WARN per call from the
/// `Module.isOpen`/`isExported` natives, which ask for `isReflectivelyExportedOrOpen`
/// (the 17/21 name; 25 renamed it `...Opened`): fixing that name took `in` from
/// 17-31 us to 8-13 -- a `lib.rs` (module lane) line, reported not landed.
///
/// # Held, with the measured blocker
///
/// * `Lookup.find*` (`findStatic`, `findVirtual`, `findConstructor`, `findGetter`,
///   `findSetter`, `findStaticGetter/Setter`, `findSpecial`, `findVarHandle`,
///   `findStaticVarHandle`), `unreflect*`, `revealDirect`, `defineClass`,
///   `defineHiddenClass*`, and every `MethodHandles` combinator
///   (`identity`, `constant`, `insertArguments`, `dropArguments`,
///   `filterArguments`, `filterReturnValue`, `foldArguments`,
///   `collectArguments`, `permuteArguments`, `guardWithTest`, `catchException`,
///   `arrayElementGetter`), `MethodHandle.asType`/`asSpreader`/`asCollector`/
///   `asVarargsCollector`/`bindTo`/`invokeWithArguments`/`isVarargsCollector`/
///   `asFixedArity`: the VM's MethodHandle is its own CARRIER (private slots
///   `MH_CLASS`..`MH_VARARGS` at 16+, dispatched by `mh_dispatch`), not a
///   `DirectMethodHandle`/`BoundMethodHandle`/`LambdaForm`. Yielded to the real
///   bytecode (dial `CRATONVM_ENFORCE_NATIVE_SHADOW=java/lang/invoke/MethodHandles`,
///   `SbrW6MhlookupComb.java`, 27 rows, 25 wrong): `identity` ->
///   `InternalError`, `constant`/`insertArguments`/`dropArguments`/`filter*`/
///   `fold*`/`collectArguments`/`permuteArguments`/`guardWithTest`/`catchException`
///   -> `NoClassDefFoundError`, `asType`/`asSpreader`/`asCollector`/
///   `invokeWithArguments`/`arrayElementGetter`/`findGetter`/`findVarHandle` ->
///   a silent `null`, `bindTo` -> `NoSuchMethodError`, `invokeExact` ->
///   `WrongMethodTypeException`, `byteArrayViewVarHandle` ->
///   `WrongMethodTypeException`; with only `Lookup` armed the combinators answer
///   `0`/`null`. Two full-suite trial arms armed on `MethodHandles$Lookup` and
///   `MethodHandle` failed RCrypto and RBlockingQueue (600 s timeouts) and
///   RExecutorShutdown (AssertionError). Unblocks: the real MethodHandle stack
///   (`DirectMethodHandle`, `BoundMethodHandle`, `LambdaForm`,
///   `InvokerBytecodeGenerator`/`ClassSpecializer` emitting classes this VM
///   can define, `MethodHandleNatives.linkMethod`), i.e. a JIT/interpreter
///   entry for `LambdaForm` -- not a lane.
/// * `Lookup.findClass`: native correct; real 1.3x (2.5x before the `Module`
///   name fix) slower.
/// * `VarHandle.varType`/`coordinateTypes`/`toMethodHandle`/`accessModeTypeUncached`/
///   `withInvoke*Behavior`, the access-mode natives: the VM's VarHandle keeps an
///   `Int` kind tag in `vform` (slot 0); the real bodies read `vform`.
///   `accessModeTypeUncached` and `withInvoke*Behavior` have NO bytecode
///   (see their registration). The dial is inert on `varType` (a `check_override`
///   allow-list entry in door code, dispatch lane's).
/// * `LambdaMetafactory.metafactory`/`altMetafactory`,
///   `InvokerBytecodeGenerator.generateCustomizedCode`: the VM's own lambda
///   linkage; the dial arms were inert on the probe (the VM links lambdas
///   without consulting them). Real linkage needs the real `InnerClassLambdaMetafactory`
///   to `defineHiddenClass` over ASM-generated bytes.
///
/// Record: `docs/feature-designs/native-dispatch-architecture.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR_MHLOOKUP_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/lang/invoke/MemberName", "getDeclaringClass", "()Ljava/lang/Class;"),
    ("java/lang/invoke/MemberName", "getFieldType", "()Ljava/lang/Class;"),
    ("java/lang/invoke/MemberName", "getMethodOrFieldType", "()Ljava/lang/invoke/MethodType;"),
    ("java/lang/invoke/MemberName", "getMethodType", "()Ljava/lang/invoke/MethodType;"),
    ("java/lang/invoke/MemberName", "getModifiers", "()I"),
    ("java/lang/invoke/MemberName", "getName", "()Ljava/lang/String;"),
    ("java/lang/invoke/MemberName", "getReferenceKind", "()B"),
    ("java/lang/invoke/MethodHandle", "type", "()Ljava/lang/invoke/MethodType;"),
    ("java/lang/invoke/MethodHandles", "lookup", "()Ljava/lang/invoke/MethodHandles$Lookup;"),
    ("java/lang/invoke/MethodHandles", "privateLookupIn", "(Ljava/lang/Class;Ljava/lang/invoke/MethodHandles$Lookup;)Ljava/lang/invoke/MethodHandles$Lookup;"),
    ("java/lang/invoke/MethodHandles", "publicLookup", "()Ljava/lang/invoke/MethodHandles$Lookup;"),
    ("java/lang/invoke/MethodHandles$Lookup", "ensureInitialized", "(Ljava/lang/Class;)Ljava/lang/Class;"),
    ("java/lang/invoke/MethodHandles$Lookup", "in", "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandles$Lookup;"),
    ("java/lang/invoke/MethodHandles$Lookup", "lookupClass", "()Ljava/lang/Class;"),
    ("java/lang/invoke/MethodHandles$Lookup", "lookupModes", "()I"),
    ("java/lang/invoke/MethodHandles$Lookup", "toString", "()Ljava/lang/String;"),
    ("java/lang/invoke/MethodType", "methodType", "(Ljava/lang/Class;)Ljava/lang/invoke/MethodType;"),
    ("java/lang/invoke/MethodType", "methodType", "(Ljava/lang/Class;Ljava/lang/Class;)Ljava/lang/invoke/MethodType;"),
    ("java/lang/invoke/MethodType", "methodType", "(Ljava/lang/Class;Ljava/lang/Class;[Ljava/lang/Class;)Ljava/lang/invoke/MethodType;"),
    ("java/lang/invoke/MethodType", "methodType", "(Ljava/lang/Class;[Ljava/lang/Class;)Ljava/lang/invoke/MethodType;"),
    ("java/lang/invoke/MethodType", "parameterArray", "()[Ljava/lang/Class;"),
    ("java/lang/invoke/MethodType", "parameterType", "(I)Ljava/lang/Class;"),
];

/// 2026-09-29, SBR wave 6 lane CLASSREFL: the `java/lang/Class` reflection
/// surface, retired so the JDK's own bytecode runs over the real
/// `ReflectionData`, `AnnotationData`, `enumConstants` and `componentType`
/// fields: 23 triples, 23 `bridge` registrations.
///
/// No new prefix: `java/lang/` admits every row.
///
/// | rows | what |
/// |---|---|
/// | 6 | member lookup: `getMethod`, `getField`, `getDeclaredField`, `getDeclaredFields`, `getConstructor`, `getDeclaredConstructor` |
/// | 2 | names: `getSimpleName`, `getCanonicalName` |
/// | 1 | `descriptorString` |
/// | 7 | class-level annotations: `getAnnotation`, `getAnnotations`, `getAnnotationsByType`, `getDeclaredAnnotation`, `getDeclaredAnnotations`, `getDeclaredAnnotationsByType`, `isAnnotationPresent` |
/// | 1 | `getEnumConstantsShared` |
/// | 6 | the reflection-data cache: `reflectionData`, `newReflectionData`, `Class$ReflectionData.<init>` and the three `Class$Atomic` CAS rows |
///
/// # What made the state real
///
/// * **`Class.componentType`** is written on every array mirror
///   (`vm_object.rs`, `class_mirror_impl`), as HotSpot's `create_mirror` does.
///   JDK 25's `descriptorString`, `isArray`, `getComponentType` and `arrayType`
///   read that field; wave 2's L0 held `descriptorString` because it was never
///   filled (`NPE: componentType is null`). The component's mirror is minted
///   before the `class_mirrors` write guard and read back under it.
/// * **The reflection-data cache is the JDK's.** `Class.reflectionData()` /
///   `newReflectionData` / `Class$Atomic` were natives over a side store because
///   the mirror's `reflectionData` slot was written by natives; under `--real-jdk`
///   the real `Atomic` CAS on the real slot works (probe `gcdrift 0`, also under
///   `CRATONVM_DBG=gc-stress=262144`), and `Unsafe.objectFieldOffset(Class, ..)`
///   answers the mirror's slots.
///
/// # What it fixes, and what it costs
///
/// Probe `SbrW6Classrefl.java` (413 rows, 200 hot iterations and 4
/// forced collections, diffed against HotSpot 25): the control differs on 18
/// rows and drifts (`getAnnotationsByType(null)` answered a fresh array every
/// call); this table differs on 4 (see the record) and drifts 0.
///
/// ```text
///   fixed: getAnnotation/getDeclaredAnnotation/getAnnotationsByType/
///          isAnnotationPresent(null) -- the native returned null / an array /
///          false where the JDK throws a bare NPE; the annotation handler class
///          (`SbrW6Classrefl$Plain`, not AnnotationInvocationHandler);
///          getMethod on an interface answered Object's public methods;
///          NoSuchMethodException/NoSuchFieldException messages for a null name,
///          a primitive receiver (`int.getClass()`, `int.<init>()`);
///          descriptorString of a hidden (lambda) class (`L;`)
/// ```
///
/// Interleaved A/B, `SbrW6ClassreflBench`, ns/op, rounds 3-4, one pinned core
/// (control `cvm-sbr-int14` -> this table): `getMethod` 3.5-4.6k -> 1.2-1.3k,
/// `getField` 1.9-2.4k -> 0.9-1.0k, `getDeclaredField` 2.9-3.2k -> 0.9-1.0k,
/// `getConstructor` 2.3-2.5k -> 1.0-1.1k, `getDeclaredFields` (10) 11.0-11.4k ->
/// 3.9-4.0k, `getSimpleName` 0.44-0.80k -> 0.35-0.39k, `Class.getAnnotation`
/// 0.67-0.71k -> 0.45-0.51k, and the known SBR-W5-REFLMETA defect -- a fresh
/// `getMethod` + `getAnnotation`, 36-39k, every call minting a fresh root
/// whose `declaredAnnotations` cache was never shared -- is **2.4-2.7k**.
/// Slower: `Class.getAnnotations()` 0.9-1.3k -> 2.5k; `descriptorString` of an
/// array 0.22-0.35k -> 4.7-5.3k (retired anyway: the native answered `L;` for a
/// hidden class).
///
/// # Left out, with the measured reason
///
/// * `getModifiers`, `isInterface`, `isPrimitive`: **reviewed `Intrinsic`s**
///   (`lib.rs`), not retirements. JDK 25's bodies read the `modifiers` /
///   `primitive` fields, which this VM fills from the class file's raw
///   `access_flags` (`ACC_SUPER` set, no InnerClasses flags, 0 for primitives
///   and arrays), and all three are `ACC_NATIVE` on 17/21, where a retirement
///   would be an `UnsatisfiedLinkError`.
/// * `newInstance`: HELD. Trial `cvm-sbr-classrefl-2` retired it and every
///   `Class.newInstance()` row failed (`NPE: "ctor" is null`; `Object.class.
///   newInstance()` returned a `cratonvm.synthetic.AnonymousObject`): the mirror's
///   slot 0 is `cachedConstructor`, and the VM overlays its class-id `Int` on it
///   (boxed to a NON-NULL wrapper) at mirror creation.
/// * `getTypeName`: HELD on cost -- correct native, but the JDK body for an
///   array crosses ~7 native rows (`isArray`, `getComponentType`, `getName`,
///   `repeat`, `concat`; ~0.55-0.75 us each here) and goes 0.35-0.39k -> 4.0-4.2k.
/// * `isArray`, `getComponentType`, `componentType`, `arrayType` (wave 2's L0
///   rows): `isArray` is `ACC_NATIVE` on 17/21, `arrayType` needs an array class
///   for a hidden class (`lambda.arrayType()` is a stand-in `[L;` mirror in
///   both arms), so they stay as they were.
///
/// Record: `docs/feature-designs/native-dispatch-architecture.md`
/// (held rows: `docs/feature-designs/native-dispatch-architecture.md`).
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR_CLASSREFL_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/lang/Class", "descriptorString", "()Ljava/lang/String;"),
    ("java/lang/Class", "getAnnotation", "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;"),
    ("java/lang/Class", "getAnnotations", "()[Ljava/lang/annotation/Annotation;"),
    ("java/lang/Class", "getAnnotationsByType", "(Ljava/lang/Class;)[Ljava/lang/annotation/Annotation;"),
    ("java/lang/Class", "getCanonicalName", "()Ljava/lang/String;"),
    ("java/lang/Class", "getConstructor", "([Ljava/lang/Class;)Ljava/lang/reflect/Constructor;"),
    ("java/lang/Class", "getDeclaredAnnotation", "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;"),
    ("java/lang/Class", "getDeclaredAnnotations", "()[Ljava/lang/annotation/Annotation;"),
    ("java/lang/Class", "getDeclaredAnnotationsByType", "(Ljava/lang/Class;)[Ljava/lang/annotation/Annotation;"),
    ("java/lang/Class", "getDeclaredConstructor", "([Ljava/lang/Class;)Ljava/lang/reflect/Constructor;"),
    ("java/lang/Class", "getDeclaredField", "(Ljava/lang/String;)Ljava/lang/reflect/Field;"),
    ("java/lang/Class", "getDeclaredFields", "()[Ljava/lang/reflect/Field;"),
    ("java/lang/Class", "getEnumConstantsShared", "()[Ljava/lang/Object;"),
    ("java/lang/Class", "getField", "(Ljava/lang/String;)Ljava/lang/reflect/Field;"),
    ("java/lang/Class", "getMethod", "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;"),
    ("java/lang/Class", "getSimpleName", "()Ljava/lang/String;"),
    ("java/lang/Class", "isAnnotationPresent", "(Ljava/lang/Class;)Z"),
    ("java/lang/Class", "newReflectionData", "(Ljava/lang/ref/SoftReference;I)Ljava/lang/Class$ReflectionData;"),
    ("java/lang/Class", "reflectionData", "()Ljava/lang/Class$ReflectionData;"),
    ("java/lang/Class$Atomic", "casAnnotationData", "(Ljava/lang/Class;Ljava/lang/Class$AnnotationData;Ljava/lang/Class$AnnotationData;)Z"),
    ("java/lang/Class$Atomic", "casAnnotationType", "(Ljava/lang/Class;Lsun/reflect/annotation/AnnotationType;Lsun/reflect/annotation/AnnotationType;)Z"),
    ("java/lang/Class$Atomic", "casReflectionData", "(Ljava/lang/Class;Ljava/lang/ref/SoftReference;Ljava/lang/ref/SoftReference;)Z"),
    ("java/lang/Class$ReflectionData", "<init>", "(I)V"),
];

/// SBR wave 6, lane THREAD (2026-09-29): 19 `java/lang/Thread` triples, 21
/// registrations (the `bridge` rows of the 25/linux kind map over them:
/// `getUncaughtExceptionHandler` and `setUncaughtExceptionHandler` are
/// registered twice). `java/lang/` is already admitted by prefix.
///
/// # Why it is safe
///
/// The state these bodies read is real. `name`, `holder` (`group`, `priority`,
/// `daemon`), `contextClassLoader`, `uncaughtExceptionHandler` and `interrupted`
/// are fields the VM already writes; two things were missing and are fixed in
/// this wave. (1) The Java `interrupted` FIELD was a mirror the VM wrote
/// only in `interrupt0` and never cleared when a `sleep`/`wait`/`join`/`park`
/// consumed the interrupt, so the VM now writes it in `thread_interrupt`
/// (before raising its own flag) and clears it at every consume site
/// (`is_interrupted(true)`, both `Object.wait` swaps, virtual-thread remount).
/// (2) The native `Thread.<init>` overloads did not run the JDK constructor's
/// tail, so they now seed `interruptLock` (the retired `interrupt()` does
/// `synchronized (interruptLock)` for a cross-thread interrupt) and
/// `contextClassLoader` (the creator's; the system loader for
/// `inheritThreadLocals == false`; the retired getter is `return
/// contextClassLoader`).
///
/// # What it fixes (probe `SbrW6Thread`, 70 rows, 0 drift)
///
/// The control differs from HotSpot on four rows; this table fixes three:
/// `getThreadGroup()`
/// on a terminated platform thread (HotSpot `null`) and on a finished virtual
/// thread, and an interrupt delivered to a not-yet-started thread (lost when
/// it started). The fourth, `start.virtualTwice`'s exception message (`null`
/// vs `Already started`), is unchanged: retiring `Thread.start` does not
/// reach it (not this lane's row).
///
/// # Speed (one 5-round run each on the same loaded host, ns/op best/median, fat LTO)
///
/// `getName` 195-215 -> 6-11, `getPriority` 215-245 -> 44, `isDaemon` 213-228
/// -> 44-46, `isInterrupted` 210-265 -> 10-12, `Thread.interrupted()` 65 ->
/// 29-32, `getContextClassLoader` 150-260 -> 10-13 (HotSpot 0.1-2.7).
/// Slower: `getThreadGroup` 245-260 -> 365-510 (the JDK body asks the
/// `threadState()` native whether the thread is terminated), `getState` 340-370
/// -> 430, `setName` 400-470 -> 1010-1160 (HotSpot 530), `Thread.sleep(0)` 95
/// -> 860-960 (HotSpot 730-900: the JDK's `sleepNanos` path, JFR event probe
/// included). `lockInterruptibly` and the `ArrayBlockingQueue` ping-pong are
/// unchanged within the host's 8-40% noise.
///
/// # Held (not in this table), each with its blocker
///
/// * The nine `<init>` overloads: the real constructor copies inheritable
///   thread-local values from `parent.inheritableThreadLocals`, but
///   `ThreadLocal`/`InheritableThreadLocal` are Rust-side `Intrinsic`s (the
///   `TL_MAP`), so that field is never written and a retired constructor loses
///   the constructor-time capture (probe `itl.inherit`, retired trial build:
///   `later later later` where HotSpot answers `parent null later`: every child
///   saw the parent's start-time value, not the construction-time one, and the
///   `inheritThreadLocals == false` child inherited too). The rest of the
///   constructor tail (`tid`, `name`, `holder`) is already right.
///   prevent). Unblocks with a real `inheritableThreadLocals` (lane
///   `dispatch`'s ThreadLocal item).
/// * `isAlive`: real body is `eetop != 0`; virtual threads are driven by the
///   VM's continuation manager and never reach the real `VirtualThread` state
///   that `alive()` reads, so retiring it hangs every virtual-thread `join`.
/// * `threadState()` (and so `getState`'s answer): blocked states live in the
///   registry's block state, not in `holder.threadStatus`; the retired
///   `getState` and `getThreadGroup` call the native `threadState()`.
/// * `sleep(JI)V`, `join`, `interrupt0`, `start0` and the other natives: their
///   JDK declarations are `ACC_NATIVE` or were not census rows.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR_THREAD_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/lang/Thread", "checkAccess", "()V"),
    (
        "java/lang/Thread",
        "getContextClassLoader",
        "()Ljava/lang/ClassLoader;",
    ),
    ("java/lang/Thread", "getName", "()Ljava/lang/String;"),
    ("java/lang/Thread", "getPriority", "()I"),
    ("java/lang/Thread", "getState", "()Ljava/lang/Thread$State;"),
    (
        "java/lang/Thread",
        "getThreadGroup",
        "()Ljava/lang/ThreadGroup;",
    ),
    (
        "java/lang/Thread",
        "getUncaughtExceptionHandler",
        "()Ljava/lang/Thread$UncaughtExceptionHandler;",
    ),
    ("java/lang/Thread", "interrupt", "()V"),
    ("java/lang/Thread", "interrupted", "()Z"),
    ("java/lang/Thread", "isDaemon", "()Z"),
    ("java/lang/Thread", "isInterrupted", "()Z"),
    ("java/lang/Thread", "run", "()V"),
    (
        "java/lang/Thread",
        "setContextClassLoader",
        "(Ljava/lang/ClassLoader;)V",
    ),
    ("java/lang/Thread", "setDaemon", "(Z)V"),
    ("java/lang/Thread", "setName", "(Ljava/lang/String;)V"),
    (
        "java/lang/Thread",
        "setUncaughtExceptionHandler",
        "(Ljava/lang/Thread$UncaughtExceptionHandler;)V",
    ),
    ("java/lang/Thread", "sleep", "(J)V"),
    ("java/lang/Thread", "start", "()V"),
    ("java/lang/Thread", "yield", "()V"),
];

/// SBR round 2 (wave 7), lane THREAD: the rows wave 6 held on `java/lang/Thread`
/// -- the nine `<init>` overloads, `isAlive` and `threadState()`, plus the static
/// `getDefaultUncaughtExceptionHandler` the same change reaches -- once the
/// state their real bodies read is real. The measured record is
/// `docs/feature-designs/jvmti-delivery-threading.md`.
///
/// What had to become true (each is a blocker `SBR-W6-THREAD` measured):
///
/// * **`Thread.threadLocals` / `inheritableThreadLocals` are real.** Under
///   `--real-jdk` `register_thread_local_natives` no longer registers the
///   `ThreadLocal` / `InheritableThreadLocal` `Intrinsic`s (default flipped;
///   `CRATONVM_TL_INTRINSIC=1` restores them), so the JDK's own
///   `ThreadLocalMap` bytecode writes the fields the real constructor copies
///   from (`createInheritedMap`, `childValue`, the `NO_INHERIT_THREAD_LOCALS`
///   opt-out) and `Thread.exit()` clears.
/// * **`eetop` is truthful.** `NativeContextImpl::thread_start`, the lazy
///   current-thread mirror and `set_native_thread_java_obj` write a nonzero
///   `eetop`; both death paths write `0` while the dying thread owns its
///   `Thread` object's monitor, before the `notify_all` that ends `join()`
///   (HotSpot's `ensure_join`). Virtual threads on this VM are
///   `ThreadBuilders$BoundVirtualThread`s on real threads, so `alive()` is the
///   same `eetop != 0` for them.
/// * **`holder.threadStatus` is written at every block-state change**: start
///   (RUNNABLE), `deposit_root_snapshot` / `check_post_block_gc` (the
///   `GcBlockState::java_state` the registry already keeps), termination
///   (TERMINATED). The real `threadState()` decodes it with
///   `VM.toThreadState`.
/// * **`ThreadIdentifiers`' counter starts at 4** (wave 82; was 2), so the
///   real constructor's `next()` cannot hand out the main thread's id
///   (`Thread.PRIMORDIAL_TID`, 3 on JDK 25).
///
/// `Compatible` mode still registers all twelve natives (retirement drops them
/// only under `--real-jdk`) and keeps the `ThreadLocal` Intrinsics.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR2_THREAD_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/lang/Thread", "<init>", "()V"),
    ("java/lang/Thread", "<init>", "(Ljava/lang/Runnable;)V"),
    (
        "java/lang/Thread",
        "<init>",
        "(Ljava/lang/Runnable;Ljava/lang/String;)V",
    ),
    ("java/lang/Thread", "<init>", "(Ljava/lang/String;)V"),
    (
        "java/lang/Thread",
        "<init>",
        "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;)V",
    ),
    (
        "java/lang/Thread",
        "<init>",
        "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;Ljava/lang/String;)V",
    ),
    (
        "java/lang/Thread",
        "<init>",
        "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;Ljava/lang/String;J)V",
    ),
    (
        "java/lang/Thread",
        "<init>",
        "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;Ljava/lang/String;JZ)V",
    ),
    (
        "java/lang/Thread",
        "<init>",
        "(Ljava/lang/ThreadGroup;Ljava/lang/String;)V",
    ),
    // Reached only once `ThreadGroup.uncaughtException` (real bytecode) runs
    // for a dying thread: the static getter of the field
    // `setDefaultUncaughtExceptionHandler` (kept native) already mirrors into.
    (
        "java/lang/Thread",
        "getDefaultUncaughtExceptionHandler",
        "()Ljava/lang/Thread$UncaughtExceptionHandler;",
    ),
    ("java/lang/Thread", "isAlive", "()Z"),
    (
        "java/lang/Thread",
        "threadState",
        "()Ljava/lang/Thread$State;",
    ),
];

/// SBR wave 6, lane `tail`: `java.lang.reflect.Proxy`
/// (4 rows), six `jdk.internal.misc.Unsafe`
/// wrapper rows, `KeyAgreement` (13), `IvParameterSpec` (2) and
/// `reflect.Array.newInstance` (2). The measured record is
/// `docs/feature-designs/native-dispatch-architecture.md`.
///
/// # Proxy (4 rows)
///
/// Wave 5 tried these and backed out (`SBR-W5-REFLMETA` section 3): the real
/// `ProxyBuilder` ran but (1) its `ClassLoaderValue.sub(..)` caches never
/// answered, because the side table keyed a `Sub` by identity and `sub(k)`
/// mints a fresh one per call; (2) the VM's own `$ProxyN` generator (still used
/// for `Class.getAnnotation`'s annotation instances) and the real
/// `ProxyGenerator` both named classes `jdk.proxy1.$Proxy0` and the VM resolves
/// by name. Both are fixed by this wave: `classloader_value_sidetable.rs` interns
/// a `Sub` by `(parent, key)` (a `Class` or a `List` of them, which is every
/// `Proxy` key), and the VM draws its `$ProxyN` numbers from the JDK's own
/// `ProxyBuilder.nextUniqueNumber` under `--real-jdk`. A VM-generated annotation
/// proxy is registered in `reverseProxyCache` so the real `isProxyClass` sees it.
/// `Proxy$Instance`, `Proxy$Dispatch` and `Proxy$Instance.<init>` are not here:
/// no image declares them.
///
/// # Left out on purpose (each has a guard test or a record)
///
/// * `java/io/ByteArrayInputStream` (`close`, `read()`, `read([BII)`): retiring
///   them fails `RSslLiveSession`'s drain check -- the native `HttpsURLConnection`
///   observes the body stream's end through the native `read`/`close`
///   (`BaisEvent`, `http_url_connection.rs`). It goes with that carrier.
/// * `Unsafe.compareAndSetByte` (structural: `objectFieldOffset` answers a slot
///   index, `the_l5_prefixes_retire_nothing_on_their_own`) and
///   `Unsafe.allocateMemory` (the JDK path hands back an address the arena does
///   not know, `the_l5t_wave_refuses_the_two_allocator_rows_the_dial_passed`).
/// * `java/nio/Bits`, `Bits$1`, `Buffer$2`, `DirectByteBuffer.address/session`,
///   `HeapByteBuffer.checkSession/session`: held by SBR-W1-BYTES / JOTP (the
///   segment-backed `session()` runs against the VM's arena carrier).
/// * `InvocationHandler.invokeDefault`: the real body builds a `MethodType`
///   from `MethodHandle.type()` of a `findSpecial` handle, and the VM's
///   `Lookup.findVirtual(..).type().changeReturnType(..)` is a
///   `ClassCastException` (its parameter array is an `Object[]`, `lang_invoke.rs`,
///   the `mhlookup` lane). The native answers correctly on real proxies.
/// * `ResourceBundle$1` (the `SharedSecrets` accessor): the `system` lane's
///   family; LOCALE2 guards it.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR_TAIL_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/lang/reflect/Proxy", "getInvocationHandler", "(Ljava/lang/Object;)Ljava/lang/reflect/InvocationHandler;"),
    ("java/lang/reflect/Proxy", "getProxyClass", "(Ljava/lang/ClassLoader;[Ljava/lang/Class;)Ljava/lang/Class;"),
    ("java/lang/reflect/Proxy", "isProxyClass", "(Ljava/lang/Class;)Z"),
    ("java/lang/reflect/Proxy", "newProxyInstance", "(Ljava/lang/ClassLoader;[Ljava/lang/Class;Ljava/lang/reflect/InvocationHandler;)Ljava/lang/Object;"),
    ("javax/crypto/KeyAgreement", "doPhase", "(Ljava/security/Key;Z)Ljava/security/Key;"),
    ("javax/crypto/KeyAgreement", "generateSecret", "()[B"),
    ("javax/crypto/KeyAgreement", "generateSecret", "(Ljava/lang/String;)Ljavax/crypto/SecretKey;"),
    ("javax/crypto/KeyAgreement", "generateSecret", "([BI)I"),
    ("javax/crypto/KeyAgreement", "getAlgorithm", "()Ljava/lang/String;"),
    ("javax/crypto/KeyAgreement", "getInstance", "(Ljava/lang/String;)Ljavax/crypto/KeyAgreement;"),
    ("javax/crypto/KeyAgreement", "getInstance", "(Ljava/lang/String;Ljava/lang/String;)Ljavax/crypto/KeyAgreement;"),
    ("javax/crypto/KeyAgreement", "getInstance", "(Ljava/lang/String;Ljava/security/Provider;)Ljavax/crypto/KeyAgreement;"),
    ("javax/crypto/KeyAgreement", "getProvider", "()Ljava/security/Provider;"),
    ("javax/crypto/KeyAgreement", "init", "(Ljava/security/Key;)V"),
    ("javax/crypto/KeyAgreement", "init", "(Ljava/security/Key;Ljava/security/SecureRandom;)V"),
    ("javax/crypto/KeyAgreement", "init", "(Ljava/security/Key;Ljava/security/spec/AlgorithmParameterSpec;)V"),
    ("javax/crypto/KeyAgreement", "init", "(Ljava/security/Key;Ljava/security/spec/AlgorithmParameterSpec;Ljava/security/SecureRandom;)V"),
    ("javax/crypto/spec/IvParameterSpec", "<init>", "([B)V"),
    ("javax/crypto/spec/IvParameterSpec", "getIV", "()[B"),
    ("jdk/internal/misc/Unsafe", "arrayIndexScale", "(Ljava/lang/Class;)I"),
    ("jdk/internal/misc/Unsafe", "ensureClassInitialized", "(Ljava/lang/Class;)V"),
    ("jdk/internal/misc/Unsafe", "getUnsafe", "()Ljdk/internal/misc/Unsafe;"),
    ("jdk/internal/misc/Unsafe", "objectFieldOffset", "(Ljava/lang/reflect/Field;)J"),
    ("jdk/internal/misc/Unsafe", "staticFieldBase", "(Ljava/lang/reflect/Field;)Ljava/lang/Object;"),
    ("jdk/internal/misc/Unsafe", "staticFieldOffset", "(Ljava/lang/reflect/Field;)J"),
];

/// SBR wave 6, lane MODULE (2026-09-29): the part of the module / class-loader /
/// code-source / access-controller surface whose state is REAL. 29 triples, 30
/// `bridge` registrations (`Module.getResourceAsStream` is registered twice),
/// every one with `Code` on 17, 21 and 25. The rest of the lane's census is HELD
/// on state the VM keeps off the JDK's objects (list and measurements below).
///
/// # What is retired, and why it is safe
///
/// * `java/security/CodeSource.getLocation`/`getCertificates`,
///   `ProtectionDomain.getCodeSource` (3): field reads. Every mint site
///   (`Class.getProtectionDomain`, `init_classloader_common_fields`) allocates
///   the REAL class and writes `location`/`certs`/`codesource` by name.
/// * `java/security/AccessController.doPrivileged` (the two one-argument
///   overloads), `AccessControlContext.<init>([ProtectionDomain)` and
///   `.checkPermission` (4): JDK 25 has no SecurityManager; the JDK bodies run
///   the action and answer HotSpot's messages (null `action` NPE,
///   `AccessControlException: checking permissions is not supported`). The
///   natives kept a VM-side "privileged frame" stack and a `java.policy` grant
///   model for a SecurityManager that cannot be installed.
/// * `java/lang/ClassLoader.<init>` x3, `checkCerts`, `getNamedPackage` (5): the
///   real constructors set every field `init_classloader_common_fields` set, and
///   validate the name (`IllegalArgumentException` for `""`).
/// * `java/lang/Class.forName(String)` and `forName(String, boolean, ClassLoader)`
///   (2): the JDK bodies reduce to `forName0`, which stays the same native
///   (`native_class_for_name`, a `Bridge`) with the caller's loader from
///   `Class.getClassLoader0`.
/// * `java/lang/Class.getPackageName` (1): computed from the name and cached in
///   the real `packageName` field.
/// * `java/lang/Module` `getName`, `getClassLoader`, `getDescriptor`, `getLayer`,
///   `canUse`, `getResourceAsStream` (6 triples): `name`, `loader`, `descriptor`
///   and `layer` are real fields on every mirror the VM mints
///   (`Class.getModule()` builder and `jboss_jdkspecific::build_module`, which
///   this wave taught to write `loader`/`enableNativeAccess` as its sibling
///   does), and `canUse` reads the descriptor's `uses`.
/// * `jdk/internal/module/Builder` (8): pure descriptor construction over the
///   real `ModuleDescriptor$*` classes; the natives fabricated stand-ins.
///
/// # Measured (JDK 25.0.4, linux; control `cvm-sbr-int14`, final `cvm-sbr-module-5`)
///
/// ```text
///   suite 152/152; strict corpus 0 divergent sections; native-won 400 -> 375
///   SbrW6Module probe (406 rows, drift 0), rows off HotSpot 25:
///     --real-jdk    38 -> 13 (the 13 are held rows or other lanes')
///     --compatible  51 -> 34, 17 lines moved, every one TOWARD HotSpot (the native
///                   fixes below); the UNRETIRE arm moves the same 17
///   ns/op, jdk-only, 3 interleaved rounds (int14 -> final):
///     Module.getName 250 -> 32-39, getClassLoader 510-600 -> 28-34, getDescriptor
///     240-420 -> 33-39, getLayer 310-460 -> 51, CodeSource.getLocation 260-570 -> 33,
///     getCertificates 560-1000 -> 35, doPrivileged 860-1300 -> 43-46,
///     getPackageName 410-550 -> 46-50, `new ClassLoader(){}` 13.5 -> 6.5 us;
///     Class.forName(String) 5.2-6.2 -> 6.4 us (up to +30%: the JDK body's
///     `Reflection.getCallerClass`), forName(String,boolean,ClassLoader) 3.8-4.0 -> 3.6,
///     Module.canUse 1.7-2.3 -> 1.9 us; --compatible within noise
/// ```
///
/// # Held, with the measured reason (retired in trial builds, backed out)
///
/// Trial 1 retired all 43 plausible rows: 123/152. Each group was then run
/// alone; the ones below are the groups that broke a discriminating probe row or a vector.
///
/// * `Module.canRead`, `isExported` x2, `isOpen` x2: the JDK bodies read `reads`,
///   `exportedPackages` and `openPackages`, which the VM never fills (it answers
///   from its `ModuleRegistry`): `java.base.isExported("java.lang")` false,
///   `java.logging.canRead(java.base)` false, `java.sql` reads false.
/// * `ModuleLayer.boot`/`findModule`/`modules`: `ModuleLayer.boot()` is
///   `System.bootLayer`, never set (24 probe rows, six vectors under the dial).
/// * `Class.forName(Module, String)`: the JDK body asks the module's loader's
///   `loadClass(Module, String)`; the platform loader's module tables are empty
///   -- `ServiceLoader` cannot find `sun.util.resources.cldr.provider.CLDRLocaleDataMetaInfo`,
///   16 vectors fail (`Locale provider adapter "CLDR" cannot be instantiated`).
/// * `ClassLoader.getSystemClassLoader`/`getPlatformClassLoader`/`loadClass` x2:
///   the VM's minted built-in loaders are not the objects in `ClassLoaders`;
///   a real `loadClass` on them finds nothing (4 probe rows: a custom loader
///   over the app loader cannot load app classes; `system.getParent()` false).
/// * `BootLoader.findResource`: no discriminating row exists; its answer is the
///   native's until the boot loader is the JDK's. `Class.getClassLoader0`,
///   `desiredAssertionStatus`, `getResourceAsStream`, `getPackage`: they read the
///   `Class.classLoader` field, which the VM never fills (the loader lives in a
///   VM side table): held without a trial, by that one fact.
///
/// # Native fixes that ride along (both modes, each toward HotSpot, measured above)
///
/// `jboss_jdkspecific::build_module` seeds the real `loader`/`enableNativeAccess` as
/// the `Class.getModule()` builder does; bare-NPE messages for `Module.canRead`/
/// `canUse`/`isExported`/`isOpen` and `ModuleLayer.findModule`, `Class.forName((Module)
/// null, ..)`, `ClassLoader.loadClass(null)`; `Class.forName(Module, "[I")` is null;
/// `int.class.getPackage()` is null.
///
/// Record: `docs/feature-designs/native-dispatch-architecture.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR_MODULE_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/lang/Class", "forName", "(Ljava/lang/String;)Ljava/lang/Class;"),
    ("java/lang/Class", "forName", "(Ljava/lang/String;ZLjava/lang/ClassLoader;)Ljava/lang/Class;"),
    ("java/lang/Class", "getPackageName", "()Ljava/lang/String;"),
    ("java/lang/ClassLoader", "<init>", "()V"),
    ("java/lang/ClassLoader", "<init>", "(Ljava/lang/ClassLoader;)V"),
    ("java/lang/ClassLoader", "<init>", "(Ljava/lang/String;Ljava/lang/ClassLoader;)V"),
    ("java/lang/ClassLoader", "checkCerts", "(Ljava/lang/String;Ljava/security/CodeSource;)V"),
    ("java/lang/ClassLoader", "getNamedPackage", "(Ljava/lang/String;Ljava/lang/Module;)Ljava/lang/NamedPackage;"),
    ("java/lang/Module", "canUse", "(Ljava/lang/Class;)Z"),
    ("java/lang/Module", "getClassLoader", "()Ljava/lang/ClassLoader;"),
    ("java/lang/Module", "getDescriptor", "()Ljava/lang/module/ModuleDescriptor;"),
    ("java/lang/Module", "getLayer", "()Ljava/lang/ModuleLayer;"),
    ("java/lang/Module", "getName", "()Ljava/lang/String;"),
    ("java/lang/Module", "getResourceAsStream", "(Ljava/lang/String;)Ljava/io/InputStream;"),
    ("java/security/AccessControlContext", "<init>", "([Ljava/security/ProtectionDomain;)V"),
    ("java/security/AccessControlContext", "checkPermission", "(Ljava/security/Permission;)V"),
    ("java/security/AccessController", "doPrivileged", "(Ljava/security/PrivilegedAction;)Ljava/lang/Object;"),
    ("java/security/AccessController", "doPrivileged", "(Ljava/security/PrivilegedExceptionAction;)Ljava/lang/Object;"),
    ("java/security/CodeSource", "getCertificates", "()[Ljava/security/cert/Certificate;"),
    ("java/security/CodeSource", "getLocation", "()Ljava/net/URL;"),
    ("java/security/ProtectionDomain", "getCodeSource", "()Ljava/security/CodeSource;"),
    ("jdk/internal/module/Builder", "build", "(I)Ljava/lang/module/ModuleDescriptor;"),
    ("jdk/internal/module/Builder", "newExports", "(Ljava/util/Set;Ljava/lang/String;)Ljava/lang/module/ModuleDescriptor$Exports;"),
    ("jdk/internal/module/Builder", "newExports", "(Ljava/util/Set;Ljava/lang/String;Ljava/util/Set;)Ljava/lang/module/ModuleDescriptor$Exports;"),
    ("jdk/internal/module/Builder", "newOpens", "(Ljava/util/Set;Ljava/lang/String;)Ljava/lang/module/ModuleDescriptor$Opens;"),
    ("jdk/internal/module/Builder", "newOpens", "(Ljava/util/Set;Ljava/lang/String;Ljava/util/Set;)Ljava/lang/module/ModuleDescriptor$Opens;"),
    ("jdk/internal/module/Builder", "newProvides", "(Ljava/lang/String;Ljava/util/List;)Ljava/lang/module/ModuleDescriptor$Provides;"),
    ("jdk/internal/module/Builder", "newRequires", "(Ljava/util/Set;Ljava/lang/String;)Ljava/lang/module/ModuleDescriptor$Requires;"),
    ("jdk/internal/module/Builder", "newRequires", "(Ljava/util/Set;Ljava/lang/String;Ljava/lang/String;)Ljava/lang/module/ModuleDescriptor$Requires;"),
];

/// SBR round 2, wave 7, lane MODULE (2026-09-30): the module graph read from the
/// JDK's own objects. The boot layer's population now fills `Module.reads`,
/// `exportedPackages` and `openPackages` from the registry
/// (`jboss_jdkspecific::fill_boot_module_tables`) and publishes the layer in
/// `System.bootLayer`, the blockers wave 6 recorded. Record:
/// `docs/feature-designs/native-dispatch-architecture.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR2_MODULE_TRIPLES: &[(&str, &str, &str)] = &[
    // The mutators retire WITH the predicates: a native that only wrote the
    // registry would leave the JDK's own `isExported`/`canRead` (which read
    // `ReflectionData`) blind to the edge.
    (
        "java/lang/Module",
        "addExports",
        "(Ljava/lang/String;Ljava/lang/Module;)Ljava/lang/Module;",
    ),
    (
        "java/lang/Module",
        "addOpens",
        "(Ljava/lang/String;Ljava/lang/Module;)Ljava/lang/Module;",
    ),
    (
        "java/lang/Module",
        "addReads",
        "(Ljava/lang/Module;)Ljava/lang/Module;",
    ),
    (
        "java/lang/Module",
        "addUses",
        "(Ljava/lang/Class;)Ljava/lang/Module;",
    ),
    ("java/lang/Module", "canRead", "(Ljava/lang/Module;)Z"),
    (
        "java/lang/Module",
        "implAddExports",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/Module",
        "implAddExports",
        "(Ljava/lang/String;Ljava/lang/Module;)V",
    ),
    (
        "java/lang/Module",
        "implAddExportsNoSync",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/Module",
        "implAddExportsNoSync",
        "(Ljava/lang/String;Ljava/lang/Module;)V",
    ),
    (
        "java/lang/Module",
        "implAddExportsToAllUnnamed",
        "(Ljava/lang/String;)V",
    ),
    ("java/lang/Module", "implAddOpens", "(Ljava/lang/String;)V"),
    (
        "java/lang/Module",
        "implAddOpens",
        "(Ljava/lang/String;Ljava/lang/Module;)V",
    ),
    (
        "java/lang/Module",
        "implAddOpensToAllUnnamed",
        "(Ljava/lang/String;)V",
    ),
    ("java/lang/Module", "isExported", "(Ljava/lang/String;)Z"),
    (
        "java/lang/Module",
        "isExported",
        "(Ljava/lang/String;Ljava/lang/Module;)Z",
    ),
    ("java/lang/Module", "isOpen", "(Ljava/lang/String;)Z"),
    (
        "java/lang/Module",
        "isOpen",
        "(Ljava/lang/String;Ljava/lang/Module;)Z",
    ),
    (
        "java/lang/ModuleLayer",
        "findModule",
        "(Ljava/lang/String;)Ljava/util/Optional;",
    ),
    ("java/lang/ModuleLayer", "modules", "()Ljava/util/Set;"),
    (
        "jdk/internal/module/ModuleReferenceImpl",
        "descriptor",
        "()Ljava/lang/module/ModuleDescriptor;",
    ),
];

/// SBR round 2, wave 8, lane FFM (2026-09-30): the FFM rows whose real bytecode
/// runs correctly over this VM's layout carriers and scoped-access natives AND
/// costs nothing (or is faster). Everything else under `java/lang/foreign/` and
/// `jdk/internal/foreign/` stays native BY MEASUREMENT (the segment, arena and
/// session carriers are the VM's own allocation shape; see the HELD page).
///
/// * `MemoryLayout.paddingLayout` / `sequenceLayout` (real factories: 0.25x and
///   0.76x of the native's cost);
/// * the seven public `ScopedMemoryAccess` wrappers (`get/put` x `Byte`, `Int`,
///   `Long`, `Short`). Their scalar Internal twins are reviewed intrinsics as
///   of 2026-10-04: the callback preserves the authoritative session check,
///   native-memory capability and shared-close access window. The wrappers'
///   Java finally blocks remain authoritative. See
///   `docs/feature-designs/jit-ir-optimizations.md`.
///
/// Record: `docs/feature-designs/native-dispatch-architecture.md`;
/// tried-and-held rows: [`RETIRED_SHADOW_SBR2_FFM_HELD_TRIPLES`] (unlisted) and
/// `docs/feature-designs/native-dispatch-architecture.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR2_FFM_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/lang/foreign/MemoryLayout",
        "paddingLayout",
        "(J)Ljava/lang/foreign/PaddingLayout;",
    ),
    (
        "java/lang/foreign/MemoryLayout",
        "sequenceLayout",
        "(JLjava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/SequenceLayout;",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "getByte",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)B",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "getInt",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)I",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "getLong",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)J",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "getShort",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)S",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "putByte",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JB)V",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "putLong",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJ)V",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "putShort",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JS)V",
    ),
];

/// SBR round 2, wave 8, lane FFM: the four rows the lane TRIED and HELD on cost
/// (UNLISTED: not in `RETIRED_SHADOW_TABLES`, so they stay live `Bridge`s; the
/// table is kept sorted so it can be spliced into the const unchanged).
///
/// The real bytecode is CORRECT over the carriers (probe `Sbr2W8FfmBase`, both
/// modes; `FunctionDescriptor`'s `toMethodType`/class-identity rows move to
/// HotSpot's, and the readers learned the real shapes -- `List12` groups,
/// `FunctionDescriptorImpl.argLayouts` -- see `p67_group_member_at` and
/// `panama_libffi::descriptor_param_layouts`), but creation costs, interleaved
/// medians of 6 (`Sbr2W8FfmBench`, `--real-jdk`): `structLayout(i4, i4, j8)`
/// 3.4x (20.8 vs 6.1 us), `unionLayout(i4, j8)` 2.5x, `FunctionDescriptor.of`
/// 2.5x (15.9 vs 6.4 us), and `Linker.downcallHandle` set-up with a fresh
/// descriptor 1.9-2.7x. Held as a maintainer call; flip them by moving the rows
/// into [`RETIRED_SHADOW_SBR2_FFM_TRIPLES`].
#[allow(dead_code)]
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR2_FFM_HELD_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/lang/foreign/FunctionDescriptor", "of", "(Ljava/lang/foreign/MemoryLayout;[Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/FunctionDescriptor;"),
    ("java/lang/foreign/FunctionDescriptor", "ofVoid", "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/FunctionDescriptor;"),
    ("java/lang/foreign/MemoryLayout", "structLayout", "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/StructLayout;"),
    ("java/lang/foreign/MemoryLayout", "unionLayout", "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/UnionLayout;"),
];

/// SBR round 3, lane FFM (2026-10-04): the layout rows round 2 HELD (the four
/// struct/union/`FunctionDescriptor` factories on cost, the twelve layout
/// `varHandle`s on the `BoundMethodHandle` / `editor()` blocker the MH lane has
/// since removed). The real bytecode builds the real layouts, descriptors and
/// `SegmentVarHandle`s, which the VM's segment natives (the carrier readers of
/// `segment_raw_access`, `segment_vh_form_access`, and this lane's
/// `segment_vh_rmw`) serve over the carrier segments. `Sbr3FfmBase`
/// (HotSpot-diffed, `--real-jdk`): 57 -> 53 rows differ with only this table
/// (14 rows move TO HotSpot, the splitter rows are held with the segments);
/// `--compatible` is untouched (a `SyntheticStub` registers and dispatches
/// there). Cost, interleaved `Sbr3FfmBench`: struct mint 2.0x (4.4 -> 8.9 us),
/// union 1.5x, `FunctionDescriptor.of` 1.27x, `varHandle` get/set 0.92x.
/// Everything segment-shaped is in [`RETIRED_SHADOW_SBR4_SEGACC_TRIPLES`] (listed,
/// SBR4 lane SEGACC). Record:
/// `docs/feature-designs/native-dispatch-architecture.md`,
/// `docs/feature-designs/native-dispatch-architecture.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR3_FFM_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/lang/foreign/FunctionDescriptor", "equals", "(Ljava/lang/Object;)Z"),
    ("java/lang/foreign/FunctionDescriptor", "of", "(Ljava/lang/foreign/MemoryLayout;[Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/FunctionDescriptor;"),
    ("java/lang/foreign/FunctionDescriptor", "ofVoid", "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/FunctionDescriptor;"),
    ("java/lang/foreign/FunctionDescriptor", "toString", "()Ljava/lang/String;"),
    ("java/lang/foreign/MemoryLayout", "structLayout", "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/StructLayout;"),
    ("java/lang/foreign/MemoryLayout", "unionLayout", "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/UnionLayout;"),
    ("jdk/internal/foreign/layout/SequenceLayoutImpl", "varHandle", "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;"),
    ("jdk/internal/foreign/layout/StructLayoutImpl", "varHandle", "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;"),
    ("jdk/internal/foreign/layout/UnionLayoutImpl", "varHandle", "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;"),
    ("jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl", "varHandle", "()Ljava/lang/invoke/VarHandle;"),
    ("jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl", "varHandle", "()Ljava/lang/invoke/VarHandle;"),
    ("jdk/internal/foreign/layout/ValueLayouts$OfByteImpl", "varHandle", "()Ljava/lang/invoke/VarHandle;"),
    ("jdk/internal/foreign/layout/ValueLayouts$OfCharImpl", "varHandle", "()Ljava/lang/invoke/VarHandle;"),
    ("jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl", "varHandle", "()Ljava/lang/invoke/VarHandle;"),
    ("jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl", "varHandle", "()Ljava/lang/invoke/VarHandle;"),
    ("jdk/internal/foreign/layout/ValueLayouts$OfIntImpl", "varHandle", "()Ljava/lang/invoke/VarHandle;"),
    ("jdk/internal/foreign/layout/ValueLayouts$OfLongImpl", "varHandle", "()Ljava/lang/invoke/VarHandle;"),
    ("jdk/internal/foreign/layout/ValueLayouts$OfShortImpl", "varHandle", "()Ljava/lang/invoke/VarHandle;"),
];

/// SBR round 4, lane SEGACC: the segment / arena / session / splitter rows
/// (`Arena` statics, `MemorySegment.ofArray/ofAddress/copy`,
/// `SegmentAllocator.allocateFrom`, `ArenaImpl`, `MemorySessionImpl`,
/// `SegmentFactories`, `Abstract/Native/MappedMemorySegmentImpl`,
/// `SegmentSplitter`), sorted, LISTED in `RETIRED_SHADOW_TABLES` (with the
/// `jdk/internal/foreign/` prefix and the lever exemption).
///
/// History: SBR3 lane FFM retired them and held them (`get/set` 89x), lane
/// FFMPERF gave a real segment a lean `VarHandle` road and held them again
/// (27x: the compiled `MemorySegment.get/set` door declined every real
/// segment). Lane SEGACC made that door serve a real segment: the
/// `FFM_SEGMENT` intrinsic hands its helper the VM pointer and a block, the
/// helper files per-thread facts for `(segment, layout, epoch)` (the layout's
/// plan, the segment's final fields, the session's `state` slot, whether this
/// thread owns it) and serves a hit with raw reads, the JDK's checks in the
/// JDK's order and the session state read on every access. Measured against the
/// carriers (interleaved CPU, `Sbr3FfmBench`): `get/setInt` pair 62 -> ~75-110
/// ns (1.2-1.3x, was 27x), `get/setLong` 1.2x, `copyToArray` 1.3x, `ofArray`
/// get 1.6x; `asSlice` 0.10x, `copy4k` 0.39x, `fill4k` 0.41x, confined
/// `alloc+close` 0.25x, layout `varHandle` 0.11x. Record:
/// `docs/feature-designs/native-dispatch-architecture.md`;
/// what stays native: `docs/feature-designs/native-dispatch-architecture.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR4_SEGACC_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/lang/foreign/Arena",
        "global",
        "()Ljava/lang/foreign/Arena;",
    ),
    (
        "java/lang/foreign/Arena",
        "ofAuto",
        "()Ljava/lang/foreign/Arena;",
    ),
    (
        "java/lang/foreign/Arena",
        "ofConfined",
        "()Ljava/lang/foreign/Arena;",
    ),
    (
        "java/lang/foreign/Arena",
        "ofShared",
        "()Ljava/lang/foreign/Arena;",
    ),
    (
        "java/lang/foreign/MemorySegment",
        "copy",
        "(Ljava/lang/Object;ILjava/lang/foreign/MemorySegment;Ljava/lang/foreign/ValueLayout;JI)V",
    ),
    (
        "java/lang/foreign/MemorySegment",
        "copy",
        "(Ljava/lang/foreign/MemorySegment;JLjava/lang/foreign/MemorySegment;JJ)V",
    ),
    (
        "java/lang/foreign/MemorySegment",
        "copy",
        "(Ljava/lang/foreign/MemorySegment;Ljava/lang/foreign/ValueLayout;JLjava/lang/Object;II)V",
    ),
    (
        "java/lang/foreign/MemorySegment",
        "ofAddress",
        "(J)Ljava/lang/foreign/MemorySegment;",
    ),
    (
        "java/lang/foreign/MemorySegment",
        "ofArray",
        "([B)Ljava/lang/foreign/MemorySegment;",
    ),
    (
        "java/lang/foreign/MemorySegment",
        "ofArray",
        "([C)Ljava/lang/foreign/MemorySegment;",
    ),
    (
        "java/lang/foreign/MemorySegment",
        "ofArray",
        "([D)Ljava/lang/foreign/MemorySegment;",
    ),
    (
        "java/lang/foreign/MemorySegment",
        "ofArray",
        "([F)Ljava/lang/foreign/MemorySegment;",
    ),
    (
        "java/lang/foreign/MemorySegment",
        "ofArray",
        "([I)Ljava/lang/foreign/MemorySegment;",
    ),
    (
        "java/lang/foreign/MemorySegment",
        "ofArray",
        "([J)Ljava/lang/foreign/MemorySegment;",
    ),
    (
        "java/lang/foreign/MemorySegment",
        "ofArray",
        "([S)Ljava/lang/foreign/MemorySegment;",
    ),
    (
        "java/lang/foreign/SegmentAllocator",
        "allocateFrom",
        "(Ljava/lang/foreign/ValueLayout$OfInt;[I)Ljava/lang/foreign/MemorySegment;",
    ),
    (
        "jdk/internal/foreign/AbstractMemorySegmentImpl",
        "address",
        "()J",
    ),
    (
        "jdk/internal/foreign/AbstractMemorySegmentImpl",
        "byteSize",
        "()J",
    ),
    (
        "jdk/internal/foreign/AbstractMemorySegmentImpl",
        "get",
        "(Ljava/lang/foreign/ValueLayout$OfByte;J)B",
    ),
    (
        "jdk/internal/foreign/AbstractMemorySegmentImpl",
        "get",
        "(Ljava/lang/foreign/ValueLayout$OfInt;J)I",
    ),
    (
        "jdk/internal/foreign/AbstractMemorySegmentImpl",
        "get",
        "(Ljava/lang/foreign/ValueLayout$OfLong;J)J",
    ),
    (
        "jdk/internal/foreign/AbstractMemorySegmentImpl",
        "get",
        "(Ljava/lang/foreign/ValueLayout$OfShort;J)S",
    ),
    (
        "jdk/internal/foreign/AbstractMemorySegmentImpl",
        "getString",
        "(J)Ljava/lang/String;",
    ),
    (
        "jdk/internal/foreign/AbstractMemorySegmentImpl",
        "isMapped",
        "()Z",
    ),
    (
        "jdk/internal/foreign/AbstractMemorySegmentImpl",
        "isNative",
        "()Z",
    ),
    (
        "jdk/internal/foreign/AbstractMemorySegmentImpl",
        "isReadOnly",
        "()Z",
    ),
    (
        "jdk/internal/foreign/AbstractMemorySegmentImpl",
        "scope",
        "()Ljava/lang/foreign/MemorySegment$Scope;",
    ),
    (
        "jdk/internal/foreign/AbstractMemorySegmentImpl$SegmentSplitter",
        "characteristics",
        "()I",
    ),
    (
        "jdk/internal/foreign/AbstractMemorySegmentImpl$SegmentSplitter",
        "estimateSize",
        "()J",
    ),
    (
        "jdk/internal/foreign/AbstractMemorySegmentImpl$SegmentSplitter",
        "forEachRemaining",
        "(Ljava/util/function/Consumer;)V",
    ),
    (
        "jdk/internal/foreign/AbstractMemorySegmentImpl$SegmentSplitter",
        "getExactSizeIfKnown",
        "()J",
    ),
    (
        "jdk/internal/foreign/AbstractMemorySegmentImpl$SegmentSplitter",
        "hasCharacteristics",
        "(I)Z",
    ),
    (
        "jdk/internal/foreign/AbstractMemorySegmentImpl$SegmentSplitter",
        "tryAdvance",
        "(Ljava/util/function/Consumer;)Z",
    ),
    (
        "jdk/internal/foreign/AbstractMemorySegmentImpl$SegmentSplitter",
        "trySplit",
        "()Ljava/util/Spliterator;",
    ),
    (
        "jdk/internal/foreign/ArenaImpl",
        "allocate",
        "(J)Ljava/lang/foreign/MemorySegment;",
    ),
    (
        "jdk/internal/foreign/ArenaImpl",
        "allocate",
        "(JJ)Ljava/lang/foreign/MemorySegment;",
    ),
    (
        "jdk/internal/foreign/ArenaImpl",
        "allocate",
        "(JJ)Ljdk/internal/foreign/NativeMemorySegmentImpl;",
    ),
    (
        "jdk/internal/foreign/ArenaImpl",
        "allocateFrom",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemorySegment;",
    ),
    (
        "jdk/internal/foreign/ArenaImpl",
        "allocateFrom",
        "(Ljava/lang/foreign/ValueLayout$OfByte;[B)Ljava/lang/foreign/MemorySegment;",
    ),
    (
        "jdk/internal/foreign/ArenaImpl",
        "allocateFrom",
        "(Ljava/lang/foreign/ValueLayout$OfChar;[C)Ljava/lang/foreign/MemorySegment;",
    ),
    (
        "jdk/internal/foreign/ArenaImpl",
        "allocateFrom",
        "(Ljava/lang/foreign/ValueLayout$OfDouble;[D)Ljava/lang/foreign/MemorySegment;",
    ),
    (
        "jdk/internal/foreign/ArenaImpl",
        "allocateFrom",
        "(Ljava/lang/foreign/ValueLayout$OfFloat;[F)Ljava/lang/foreign/MemorySegment;",
    ),
    (
        "jdk/internal/foreign/ArenaImpl",
        "allocateFrom",
        "(Ljava/lang/foreign/ValueLayout$OfInt;[I)Ljava/lang/foreign/MemorySegment;",
    ),
    (
        "jdk/internal/foreign/ArenaImpl",
        "allocateFrom",
        "(Ljava/lang/foreign/ValueLayout$OfLong;[J)Ljava/lang/foreign/MemorySegment;",
    ),
    (
        "jdk/internal/foreign/ArenaImpl",
        "allocateFrom",
        "(Ljava/lang/foreign/ValueLayout$OfShort;[S)Ljava/lang/foreign/MemorySegment;",
    ),
    (
        "jdk/internal/foreign/ArenaImpl",
        "allocateNoInit",
        "(JJ)Ljdk/internal/foreign/NativeMemorySegmentImpl;",
    ),
    ("jdk/internal/foreign/ArenaImpl", "close", "()V"),
    (
        "jdk/internal/foreign/ArenaImpl",
        "scope",
        "()Ljava/lang/foreign/MemorySegment$Scope;",
    ),
    (
        "jdk/internal/foreign/MappedMemorySegmentImpl",
        "address",
        "()J",
    ),
    (
        "jdk/internal/foreign/MappedMemorySegmentImpl",
        "byteSize",
        "()J",
    ),
    (
        "jdk/internal/foreign/MappedMemorySegmentImpl",
        "get",
        "(Ljava/lang/foreign/ValueLayout$OfByte;J)B",
    ),
    (
        "jdk/internal/foreign/MappedMemorySegmentImpl",
        "get",
        "(Ljava/lang/foreign/ValueLayout$OfInt;J)I",
    ),
    (
        "jdk/internal/foreign/MappedMemorySegmentImpl",
        "get",
        "(Ljava/lang/foreign/ValueLayout$OfLong;J)J",
    ),
    (
        "jdk/internal/foreign/MappedMemorySegmentImpl",
        "get",
        "(Ljava/lang/foreign/ValueLayout$OfShort;J)S",
    ),
    (
        "jdk/internal/foreign/MappedMemorySegmentImpl",
        "getString",
        "(J)Ljava/lang/String;",
    ),
    (
        "jdk/internal/foreign/MappedMemorySegmentImpl",
        "isMapped",
        "()Z",
    ),
    (
        "jdk/internal/foreign/MappedMemorySegmentImpl",
        "isNative",
        "()Z",
    ),
    (
        "jdk/internal/foreign/MappedMemorySegmentImpl",
        "isReadOnly",
        "()Z",
    ),
    (
        "jdk/internal/foreign/MappedMemorySegmentImpl",
        "scope",
        "()Ljava/lang/foreign/MemorySegment$Scope;",
    ),
    (
        "jdk/internal/foreign/MemorySessionImpl",
        "addCloseAction",
        "(Ljava/lang/Runnable;)V",
    ),
    (
        "jdk/internal/foreign/MemorySessionImpl",
        "addInternal",
        "(Ljdk/internal/foreign/MemorySessionImpl$ResourceList$ResourceCleanup;)V",
    ),
    (
        "jdk/internal/foreign/MemorySessionImpl",
        "addOrCleanupIfFail",
        "(Ljdk/internal/foreign/MemorySessionImpl$ResourceList$ResourceCleanup;)V",
    ),
    (
        "jdk/internal/foreign/MemorySessionImpl",
        "checkValidState",
        "()V",
    ),
    (
        "jdk/internal/foreign/MemorySessionImpl",
        "checkValidState",
        "(Ljava/lang/foreign/MemorySegment;)V",
    ),
    (
        "jdk/internal/foreign/MemorySessionImpl",
        "checkValidStateRaw",
        "()V",
    ),
    ("jdk/internal/foreign/MemorySessionImpl", "close", "()V"),
    (
        "jdk/internal/foreign/MemorySessionImpl",
        "createConfined",
        "(Ljava/lang/Thread;)Ljdk/internal/foreign/MemorySessionImpl;",
    ),
    (
        "jdk/internal/foreign/MemorySessionImpl",
        "createHeap",
        "(Ljava/lang/Object;)Ljdk/internal/foreign/MemorySessionImpl;",
    ),
    (
        "jdk/internal/foreign/MemorySessionImpl",
        "createImplicit",
        "(Ljava/lang/ref/Cleaner;)Ljdk/internal/foreign/MemorySessionImpl;",
    ),
    (
        "jdk/internal/foreign/MemorySessionImpl",
        "createShared",
        "()Ljdk/internal/foreign/MemorySessionImpl;",
    ),
    (
        "jdk/internal/foreign/MemorySessionImpl",
        "isAccessibleBy",
        "(Ljava/lang/Thread;)Z",
    ),
    ("jdk/internal/foreign/MemorySessionImpl", "isAlive", "()Z"),
    (
        "jdk/internal/foreign/MemorySessionImpl",
        "isCloseable",
        "()Z",
    ),
    (
        "jdk/internal/foreign/MemorySessionImpl",
        "ownerThread",
        "()Ljava/lang/Thread;",
    ),
    (
        "jdk/internal/foreign/MemorySessionImpl",
        "toMemorySession",
        "(Ljava/lang/foreign/Arena;)Ljdk/internal/foreign/MemorySessionImpl;",
    ),
    (
        "jdk/internal/foreign/MemorySessionImpl",
        "whileAlive",
        "(Ljava/lang/Runnable;)V",
    ),
    (
        "jdk/internal/foreign/NativeMemorySegmentImpl",
        "address",
        "()J",
    ),
    (
        "jdk/internal/foreign/NativeMemorySegmentImpl",
        "byteSize",
        "()J",
    ),
    (
        "jdk/internal/foreign/NativeMemorySegmentImpl",
        "get",
        "(Ljava/lang/foreign/ValueLayout$OfByte;J)B",
    ),
    (
        "jdk/internal/foreign/NativeMemorySegmentImpl",
        "get",
        "(Ljava/lang/foreign/ValueLayout$OfInt;J)I",
    ),
    (
        "jdk/internal/foreign/NativeMemorySegmentImpl",
        "get",
        "(Ljava/lang/foreign/ValueLayout$OfLong;J)J",
    ),
    (
        "jdk/internal/foreign/NativeMemorySegmentImpl",
        "get",
        "(Ljava/lang/foreign/ValueLayout$OfShort;J)S",
    ),
    (
        "jdk/internal/foreign/NativeMemorySegmentImpl",
        "getString",
        "(J)Ljava/lang/String;",
    ),
    (
        "jdk/internal/foreign/NativeMemorySegmentImpl",
        "isMapped",
        "()Z",
    ),
    (
        "jdk/internal/foreign/NativeMemorySegmentImpl",
        "isNative",
        "()Z",
    ),
    (
        "jdk/internal/foreign/NativeMemorySegmentImpl",
        "isReadOnly",
        "()Z",
    ),
    (
        "jdk/internal/foreign/NativeMemorySegmentImpl",
        "scope",
        "()Ljava/lang/foreign/MemorySegment$Scope;",
    ),
    (
        "jdk/internal/foreign/SegmentFactories",
        "fromArray",
        "([F)Ljdk/internal/foreign/HeapMemorySegmentImpl$OfFloat;",
    ),
];

/// SBR round 5, lane FFMLINK (2026-10-06): the `JavaNioAccess` accounting pair and
/// the `char` unaligned scoped wrappers. Sorted, LISTED in `RETIRED_SHADOW_TABLES`.
///
/// * `Buffer$2.reserveMemory` / `unreserveMemory`: the real bodies are
///   `Bits.reserveMemory` / `Bits.unreserveMemory`, the JDK's counters and the
///   `MaxDirectMemorySize` cap, which `DirectByteBuffer` already runs for real
///   (SBR3 NIOBUF). The two natives were no-ops, so a segment allocated by
///   `Arena` never counted against `BufferPoolMXBean("direct")` or the cap.
///   They retire as a PAIR: retiring one alone would leak the counter.
/// * `SymbolLookup.libraryLookup(String, Arena)` / `loaderLookup()`: the JDK's own
///   lookups over `RawNativeLibraries` / `ClassLoader.findNative` (the natives
///   `load0`, `unload0`, `findEntry0` exist). The bridge's errors differed from
///   HotSpot's (`IllegalStateException: Failed to load library`, a fabricated
///   lookup for a null arena, `find` answering after close).
/// * `Linker.nativeLinker()`: the JDK's `SharedUtils.getSystemLinker()` over `CABI.FALLBACK` (`ForeignLinkerSupport.isSupported0` false, `FallbackLinker.isSupported` an `Intrinsic`); the carriers still come from the VM's libffi road through reviewed `Intrinsic`s on `AbstractLinker.downcallHandle` x2 / `upcallStub` (`ffm_fallback_linker.rs`), which run the JDK's own validation first. The JDK's `FallbackLinker.arrangeDowncall` costs ~130 us per call here: `docs/feature-designs/native-dispatch-architecture.md`.
/// * `ScopedMemoryAccess.getCharUnaligned` / `putCharUnaligned` (the public
///   wrappers; their `...Internal` twins are reviewed intrinsics): the same
///   wrapper rule as the `Short`/`Int`/`Long` rows of `RETIRED_SHADOW_L5_TRIPLES`.
///
/// Record: `docs/feature-designs/profile-guided-inlining.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR5_FFMLINK_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/lang/foreign/Linker",
        "nativeLinker",
        "()Ljava/lang/foreign/Linker;",
    ),
    (
        "java/lang/foreign/SymbolLookup",
        "libraryLookup",
        "(Ljava/lang/String;Ljava/lang/foreign/Arena;)Ljava/lang/foreign/SymbolLookup;",
    ),
    (
        "java/lang/foreign/SymbolLookup",
        "loaderLookup",
        "()Ljava/lang/foreign/SymbolLookup;",
    ),
    ("java/nio/Buffer$2", "reserveMemory", "(JJ)V"),
    ("java/nio/Buffer$2", "unreserveMemory", "(JJ)V"),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "getCharUnaligned",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)C",
    ),
    (
        "jdk/internal/misc/ScopedMemoryAccess",
        "putCharUnaligned",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JCZ)V",
    ),
];

/// SBR round 5, lane FIX3 (2026-10-07): the last two native-won rows of the suite
/// census. Sorted (binary-searched), LISTED in `RETIRED_SHADOW_TABLES`.
///
/// * `IndirectVarHandle.withInvokeExactBehavior()`: a `Bridge` that RAN the JDK's
///   own body (`invoke_virtual_bytecode_only`) and then told the interpreter and
///   JIT doors an exact handle exists (`note_exact_var_handle_minted`). The doors
///   resolve the `VarHandle.exact` slot on their first ask
///   (`var_handle_exact_gate`), so the note was redundant; the real bytecode runs.
/// * `ReferenceQueue.remove()`: the native waited on the queue's own `lock` in
///   slices (`rq_wait_on_lock`), which is what the JDK's `remove` bytecode does
///   itself now that the VM's delivery thread notifies `lock` after a collection
///   (`gc_enqueued_queues_notify`). Its sibling `remove(long)` was retired by
///   SBR2 SMALL2. `--compatible` keeps the polling native: nothing notifies `lock`
///   there.
///
/// Record: `docs/feature-designs/native-dispatch-architecture.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR5_FIX3_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/lang/invoke/IndirectVarHandle",
        "withInvokeExactBehavior",
        "()Ljava/lang/invoke/VarHandle;",
    ),
    (
        "java/lang/ref/ReferenceQueue",
        "remove",
        "()Ljava/lang/ref/Reference;",
    ),
];

/// SBR2 wave 8, lane LOADERS (2026-09-30): the built-in loaders are the JDK's own
/// `ClassLoaders` objects (`classloader::adopt_real_builtin_loader`), so the JDK
/// bytecode over them runs. See `SBR2-W8-LOADERS-*-RETIRED-20260930.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR2_LOADERS_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/lang/Class", "desiredAssertionStatus", "()Z"),
    // `Class.forName(Module, String)` stays native (HELD): the JDK body needs
    // `BuiltinClassLoader.loadModule` run for every boot module (`nameToModule`).
    // `Class.getPackage` (the I2 `definePackage` natives mint a fresh Package per call)
    // and `ClassLoader.loadClass` x2 (module-path classes need `nameToModule`) stay native too.
    (
        "java/lang/Class",
        "getClassLoader0",
        "()Ljava/lang/ClassLoader;",
    ),
    (
        "java/lang/Class",
        "getResourceAsStream",
        "(Ljava/lang/String;)Ljava/io/InputStream;",
    ),
    (
        "java/lang/ClassLoader",
        "getPlatformClassLoader",
        "()Ljava/lang/ClassLoader;",
    ),
    (
        "java/lang/ClassLoader",
        "getSystemClassLoader",
        "()Ljava/lang/ClassLoader;",
    ),
    ("java/lang/Module", "getPackages", "()Ljava/util/Set;"),
    (
        "jdk/internal/loader/BootLoader",
        "findResource",
        "(Ljava/lang/String;)Ljava/net/URL;",
    ),
    (
        "jdk/internal/loader/BootLoader",
        "findResourceAsStream",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/io/InputStream;",
    ),
];

/// SBR2 wave 9, lane MODLOAD (2026-10-01): the boot modules are DEFINED to the
/// built-in loaders and the boot `Configuration` / system `ModuleFinder` are the
/// JDK's own objects (`jboss_jdkspecific::finish_boot_layer`, run by the
/// launcher right after the boot layer is built), so the JDK bytecode over them
/// answers. Record: `docs/feature-designs/native-dispatch-architecture.md`;
/// tried-and-held rows: [`RETIRED_SHADOW_SBR2_MODLOAD_HELD_TRIPLES`] (unlisted)
/// and `docs/jdk-only/SBR2-W9-MODLOAD-*-HELD-20261001.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR2_MODLOAD_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/lang/Class",
        "forName",
        "(Ljava/lang/Module;Ljava/lang/String;)Ljava/lang/Class;",
    ),
    (
        "java/lang/Class",
        "getResource",
        "(Ljava/lang/String;)Ljava/net/URL;",
    ),
    ("java/lang/ModuleLayer", "boot", "()Ljava/lang/ModuleLayer;"),
    (
        "java/lang/ModuleLayer",
        "configuration",
        "()Ljava/lang/module/Configuration;",
    ),
    (
        "jdk/internal/module/SystemModuleFinders$SystemModuleFinder",
        "find",
        "(Ljava/lang/String;)Ljava/util/Optional;",
    ),
];

/// SBR2 wave 9, lane MODLOAD: HELD, deliberately NOT in `RETIRED_SHADOW_TABLES`.
/// `ClassLoader.loadClass` x2: the real `defineOrCheckPackage` finds the package
/// of a `Sealed: true` jar already defined when its first class is defined
/// (`SecurityException: sealing violation`). `Class.getPackage`: fixes the jar
/// manifest cases but answers `null` for a lambda class and `isSealed() == false`
/// for `java.lang`.
#[allow(dead_code)]
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR2_MODLOAD_HELD_TRIPLES: &[(&str, &str, &str)] = &[];

/// SBR wave 6, lane SYSTEM (2026-09-29): `java.lang.System`, the
/// `JavaLangAccess` carrier `System$1`, the `SharedSecrets` getters,
/// `jdk.internal.misc.VM`, `DefaultLoggerFinder` and `BootLoader.loadLibrary` --
/// the JDK's own bootstrap plumbing. 47 triples, 49 `bridge` registrations in
/// the 25/linux kind map (`System` 10 (10), `System$1` 22 (23),
/// `SharedSecrets` 9 (10), `VM` 4 (4), `DefaultLoggerFinder` 1 (1),
/// `BootLoader` 1 (1); no other kind occurs on these triples). Every row has
/// `Code` on 25, checked with `javap -p -c`; the rows the `System$1` carrier
/// has only on 25 are inert on 17/21 (the carrier is `System$2` there) and
/// unmeasured there.
///
/// # What made each group safe
///
/// * **`SharedSecrets` getters, `VM.initLevel`/`saveProperties`/
///   `isJavaLangInvokeInited`/`setJavaLangInvokeInited`,
///   `DefaultLoggerFinder.isSystem`.** Bare static reads (for `isSystem`, a
///   loader comparison) over fields the real class initialisers and the boot
///   sequence already fill: the VM publishes `javaLangAccess` and
///   `javaLangReflectAccess` (`publish_shared_secrets`), every other accessor is
///   set by its own class's `<clinit>`, and the launcher writes
///   `VM.initLevel`. The two `Invoke` rows are a pair and move together (one
///   real static). Dial screen (`CRATONVM_ENFORCE_NATIVE_SHADOW` over
///   `SharedSecrets`, `VM`, `DefaultLoggerFinder`, control `cvm-sbr-int14`):
///   **152/152**, every row `bytecode-won` in every vector that reached it.
/// * **`System.getProperty` x2, `lineSeparator`, `getenv` x2, `gc`,
///   `getLogger` x2.** The JDK bodies read `System.props` (kept true by
///   `publish_real_system_props` and the `setProperty`/`clearProperty`/
///   `setProperties` mirrors; `setProperties` now also publishes the new
///   object on the static, `--real-jdk`), the `lineSeparator` static
///   `initPhase1` sets, `ProcessEnvironment` (its `environ` native is genuine),
///   `Runtime.gc` (genuine) and the real `LazyLoggers`.
///
///   gch h1/jo (2026-10-08): retiring `gc` changes where a COMPILED caller
///   collects. The dropped registration no longer stops the JIT's inline
///   resolver (`native-shadow`), so a caller that splices `System.gc()` now
///   reaches the collection at `Runtime.gc()` one splice level deeper; and an
///   interpreted caller runs one more Java frame (`System.gc`) above the native.
///   `GenR4W6JitOomRootProbe`'s `catch-inline-receiver` and
///   `oome-thread-exit` keep their dropped data under `--real-jdk` since this
///   table (bisected to `3fab40ab77`). That is a VM retention defect at that GC
///   point, not a reason to restore the row: the real bytes are HotSpot's.
///   `CRATONVM_UNRETIRE_NATIVE_SHADOW=java/lang/System.gc()V` is the per-row
///   bisect lever, and `GchH1JoWrapperGcProbe.java` builds the
///   same call shape under `--compatible`. Page:
///   `docs/feature-designs/jit-ir-optimizations.md`.
/// * **`System$1` (`JavaLangAccess`).** Every row is a one-line forward in the
///   JDK (`Module.impl*`, `Thread.start(ThreadContainer)`,
///   `Class.getDeclaredPublicMethods`, `String.getBytesNoRepl`,
///   `ModuleLayer.layers`, `ClassLoader.defineClass0/1`, ...) that the native
///   duplicated. Where the native was not the forward it was wrong:
///   `defineUnnamedModule` minted a 2-field synthetic `Module` (the boot
///   loader's `UNNAMED_MODULE`), `addEnableNativeAccess` was an identity that
///   never set the flag, and the `getBytes*NoRepl` natives ignored the
///   `Charset` (UTF-8 only).
/// * **`System.load`/`loadLibrary` + `BootLoader.loadLibrary`, one unit.** The
///   dial over `java/lang/System` alone failed two vectors: `RJdkJni`
///   (`System.loadLibrary must FAIL for zip once java.util.zip has boot-loaded
///   it`: the real `ClassLoader.loadLibrary` refuses a library the BOOT loader
///   holds, and `BootLoader.loadLibrary` was a registered no-op, so nothing was
///   ever recorded) and `RJdkAwtHeadless` (`System.loadLibrary("awt")` reached
///   the genuine `NativeLibraries.load` with `<java.home>/lib/libawt.so`, whose
///   `dlopen` fails here because it links `libjvm`). With `BootLoader` in the
///   dial `RJdkJni` passes; `NativeLibraries.load` now reports a file under
///   `java.home` as loaded (`native-builtins/src/lib.rs`, the test
///   `System.loadLibrary` used to make by name in
///   `lang_system::jdk_image_ships_library`). The `BootLoader` row belongs to
///   the `module` lane's class; a duplicate there is harmless.
///
/// # Measured (JDK 25.0.4, linux; control `cvm-sbr-int14`, final `cvm-sbr-system-2`)
///
/// ```text
///   suite (152):     int14 152/152; system-1 152/152; system-2 152/152
///   dial screen:     System (2 classes) 150/152 (RJdkJni, RJdkAwtHeadless: the
///                    load pair above); SharedSecrets+VM+DefaultLoggerFinder 152/152
///   strict corpus:   int14 0 divergent sections; system-1 0; system-2 0
///   SbrW6System (127 rows) vs HotSpot 25:
///                    int14 --real-jdk 11 rows differ; system-2 --real-jdk 4
///                    (java.library.path's value, getLogger provider class and
///                    isLoggable(OFF) -- the same two as control --, and
///                    `Paths.get("/a\0b")` -- the FS lane's); drift 0 in every arm;
///                    --nojit identical; UNRETIRE (all 6 classes armed: 55 rows, the 47 + 8 of earlier waves)
///                    = control except mapLibraryName(null) moving TO HotSpot;
///                    --compatible = control except that one line
///   native-won census (152 reports): 400 -> 363 triples (this lane's classes
///                    43 -> 5: exit, getProperties, setProperty, clearProperty,
///                    findBootstrapClassOrNull)
///   cost (ns/op, interleaved x3 vs control, ranges):
///     getProperty hit/miss/(k,def)   3796-3952 / 1914-1975 / 1966-2128
///                                    -> 501-659 / 472-604 / 570-668   (HotSpot ~20)
///     lineSeparator                  185-238 -> 60-73
///     EnumSet.allOf                  2484-2615 -> 1206-1257
///     StringJoiner                   2767-2942 -> 2107-2458
///     Paths.get+toString             2139-2239 -> 2228-2356   (+4-6%)
///     getenv(name) / getenv().get    525-592 / 250-267 -> 1867-1976 / 1759-1878
///     System.getLogger (cached)      2667-2838 -> 5088-5344
///     virtual thread start+join, thread-per-task, Files.exists, String.join,
///     System.gc: overlapping ranges
/// ```
///
/// `getenv` and `getLogger` are the two rows that cost. `getenv` is retired on
/// the rule "retire anyway when the native gives wrong answers": the native
/// answered `null`/`false` for `getenv().get(null)`, `.get(1)` and
/// `.containsKey(null)` where HotSpot throws (`SbrW6System` rows), and the JDK
/// body is 1.9 us where the native was 0.25-0.55 (config reads; rarely hot).
/// `getLogger`'s answers are identical to the native's in every probe row and in
/// `SystemRuntimeObjectSweep` (both hand back a `SimpleConsoleLogger` where
/// HotSpot hands back the JUL wrapper: the platform loader's `ServiceLoader`
/// finds no `java.logging` provider, the `module` lane's); it moves from 2.7 to
/// 5.2 us, once per logger field.
///
/// # Left out, with the blocker
///
/// * `System.getProperties`, `setProperty`, `clearProperty`, `setProperties`:
///   the VM's own property store is the authority for Rust-side readers and
///   these natives are its write/harvest bridge to the real `Properties`.
/// * `System.exit` (the VM exit path), `System$1.findBootstrapClassOrNull`
///   (`ClassLoader.findBootstrapClass` has no native), `VM.awaitInitLevel` (the
///   real body waits on a lock the launcher never notifies).
/// * The `System$1` rows with no `Code` on 25, `System$2` (inert on 25) and
///   `setCause` (no vector or probe reaches it).
///
/// Records: `docs/feature-designs/jit-ir-optimizations.md`,
/// held rows `docs/feature-designs/native-dispatch-architecture.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR_SYSTEM_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/lang/System", "gc", "()V"),
    ("java/lang/System", "getLogger", "(Ljava/lang/String;)Ljava/lang/System$Logger;"),
    ("java/lang/System", "getLogger", "(Ljava/lang/String;Ljava/util/ResourceBundle;)Ljava/lang/System$Logger;"),
    ("java/lang/System", "getProperty", "(Ljava/lang/String;)Ljava/lang/String;"),
    ("java/lang/System", "getProperty", "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;"),
    ("java/lang/System", "getenv", "()Ljava/util/Map;"),
    ("java/lang/System", "getenv", "(Ljava/lang/String;)Ljava/lang/String;"),
    ("java/lang/System", "lineSeparator", "()Ljava/lang/String;"),
    ("java/lang/System", "load", "(Ljava/lang/String;)V"),
    ("java/lang/System", "loadLibrary", "(Ljava/lang/String;)V"),
    ("java/lang/System$1", "addEnableNativeAccess", "(Ljava/lang/Module;)Ljava/lang/Module;"),
    ("java/lang/System$1", "addExports", "(Ljava/lang/Module;Ljava/lang/String;)V"),
    ("java/lang/System$1", "addExports", "(Ljava/lang/Module;Ljava/lang/String;Ljava/lang/Module;)V"),
    ("java/lang/System$1", "addExportsToAllUnnamed", "(Ljava/lang/Module;Ljava/lang/String;)V"),
    ("java/lang/System$1", "addOpens", "(Ljava/lang/Module;Ljava/lang/String;Ljava/lang/Module;)V"),
    ("java/lang/System$1", "addOpensToAllUnnamed", "(Ljava/lang/Module;Ljava/lang/String;)V"),
    ("java/lang/System$1", "addReads", "(Ljava/lang/Module;Ljava/lang/Module;)V"),
    ("java/lang/System$1", "addReadsAllUnnamed", "(Ljava/lang/Module;)V"),
    ("java/lang/System$1", "addUses", "(Ljava/lang/Module;Ljava/lang/Class;)V"),
    ("java/lang/System$1", "currentCarrierThread", "()Ljava/lang/Thread;"),
    ("java/lang/System$1", "defineClass", "(Ljava/lang/ClassLoader;Ljava/lang/Class;Ljava/lang/String;[BLjava/security/ProtectionDomain;ZILjava/lang/Object;)Ljava/lang/Class;"),
    ("java/lang/System$1", "defineClass", "(Ljava/lang/ClassLoader;Ljava/lang/String;[BLjava/security/ProtectionDomain;Ljava/lang/String;)Ljava/lang/Class;"),
    ("java/lang/System$1", "defineUnnamedModule", "(Ljava/lang/ClassLoader;)Ljava/lang/Module;"),
    ("java/lang/System$1", "getBytesUTF8NoRepl", "(Ljava/lang/String;)[B"),
    ("java/lang/System$1", "getConstantPool", "(Ljava/lang/Class;)Ljdk/internal/reflect/ConstantPool;"),
    ("java/lang/System$1", "getDeclaredPublicMethods", "(Ljava/lang/Class;Ljava/lang/String;[Ljava/lang/Class;)Ljava/util/List;"),
    ("java/lang/System$1", "getEnumConstantsShared", "(Ljava/lang/Class;)[Ljava/lang/Enum;"),
    ("java/lang/System$1", "join", "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;[Ljava/lang/String;I)Ljava/lang/String;"),
    ("java/lang/System$1", "layers", "(Ljava/lang/ClassLoader;)Ljava/util/stream/Stream;"),
    ("java/lang/System$1", "newStringUTF8NoRepl", "([BII)Ljava/lang/String;"),
    ("java/lang/System$1", "start", "(Ljava/lang/Thread;Ljdk/internal/vm/ThreadContainer;)V"),
    ("java/lang/System$1", "uncheckedGetBytesNoRepl", "(Ljava/lang/String;Ljava/nio/charset/Charset;)[B"),
    ("jdk/internal/access/SharedSecrets", "getJavaIOFileDescriptorAccess", "()Ljdk/internal/access/JavaIOFileDescriptorAccess;"),
    ("jdk/internal/access/SharedSecrets", "getJavaLangAccess", "()Ljdk/internal/access/JavaLangAccess;"),
    ("jdk/internal/access/SharedSecrets", "getJavaLangInvokeAccess", "()Ljdk/internal/access/JavaLangInvokeAccess;"),
    ("jdk/internal/access/SharedSecrets", "getJavaLangRefAccess", "()Ljdk/internal/access/JavaLangRefAccess;"),
    ("jdk/internal/access/SharedSecrets", "getJavaLangReflectAccess", "()Ljdk/internal/access/JavaLangReflectAccess;"),
    ("jdk/internal/access/SharedSecrets", "getJavaNetInetAddressAccess", "()Ljdk/internal/access/JavaNetInetAddressAccess;"),
    ("jdk/internal/access/SharedSecrets", "getJavaNioAccess", "()Ljdk/internal/access/JavaNioAccess;"),
    ("jdk/internal/access/SharedSecrets", "getJavaUtilResourceBundleAccess", "()Ljdk/internal/access/JavaUtilResourceBundleAccess;"),
    ("jdk/internal/access/SharedSecrets", "getJavaUtilZipFileAccess", "()Ljdk/internal/access/JavaUtilZipFileAccess;"),
    ("jdk/internal/loader/BootLoader", "loadLibrary", "(Ljava/lang/String;)V"),
    ("jdk/internal/logger/DefaultLoggerFinder", "isSystem", "(Ljava/lang/Module;)Z"),
    ("jdk/internal/misc/VM", "initLevel", "()I"),
    ("jdk/internal/misc/VM", "isJavaLangInvokeInited", "()Z"),
    ("jdk/internal/misc/VM", "saveProperties", "(Ljava/util/Map;)V"),
    ("jdk/internal/misc/VM", "setJavaLangInvokeInited", "()V"),
];

/// SBR wave 6, lane DISPATCH (2026-09-29): `javax/crypto/Cipher`, 35
/// registrations, retired because the NATIVE `Cipher` answers 13 probe rows
/// WRONG that the JDK's own `Cipher`/`CipherSpi` bytecode answers as HotSpot
/// does (`SbrW1Jca`, wave-5 section; 27 lines off HotSpot -> 1, the one left
/// being a timing line):
///
/// * `AES/PCBC/PKCS5Padding` (x3 key sizes) and `AES/CTS/NoPadding` (x3):
///   `NoSuchAlgorithmException: Cannot find any provider supporting ...`;
/// * `DESede/CTR/NoPadding`, `Blowfish/CBC/PKCS5Padding`, `Blowfish/CTR/NoPadding`:
///   the same refusal;
/// * `AES/GCM/NoPadding` with a 16-byte IV: `InvalidAlgorithmParameterException`
///   ("12-byte IV only");
/// * `AESWrapPad` of 21 bytes: `InvalidKeyException` where HotSpot wraps;
/// * `ChaCha20-Poly1305` over overlapping in/out buffers (wrong output for 3 of
///   6 offsets) and a ChaCha20 key stream crossing the 2^32 block counter (empty
///   output).
///
/// (`Mac` is NOT here: the native `Mac` is correct on every probe row, and
/// retired it is 4x slower to set up -- held.) The wave-5 lane held `Cipher` for
/// setup cost (`getInstance` 158 us against the native's 3, the TLS handshake
/// 30-60% slower); this wave's speed rule retires a class whose native gives
/// wrong answers anyway, with the cost named. Measured this wave, JIT,
/// `SbrW6DispatchBench` `cipher` (`Cipher.getInstance("AES/CBC/PKCS5Padding")`):
/// native 2.6-3.1 us, retired 145-152 us (HotSpot 5.2 us cold, 7-37 us in the
/// wave-5 record); `Hs` handshake micro (fresh process, n=300): native 109-179
/// us/op, retired 1614-1750 (mostly interpreted `getInstance`/`init` bytecode).
/// The setup path is `Provider.getService`/`Constructor.newInstance`/
/// `ThreadLocal` crossings, which this wave made 15-40% cheaper
/// (`docs/feature-designs/native-dispatch-architecture.md`) and
/// which remain the cost.
///
/// Record: `docs/feature-designs/native-dispatch-architecture.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR_DISPATCH_TRIPLES: &[(&str, &str, &str)] = &[
    ("javax/crypto/Cipher", "doFinal", "()[B"),
    ("javax/crypto/Cipher", "doFinal", "(Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)I"),
    ("javax/crypto/Cipher", "doFinal", "([B)[B"),
    ("javax/crypto/Cipher", "doFinal", "([BI)I"),
    ("javax/crypto/Cipher", "doFinal", "([BII)[B"),
    ("javax/crypto/Cipher", "doFinal", "([BII[B)I"),
    ("javax/crypto/Cipher", "doFinal", "([BII[BI)I"),
    ("javax/crypto/Cipher", "getAlgorithm", "()Ljava/lang/String;"),
    ("javax/crypto/Cipher", "getBlockSize", "()I"),
    ("javax/crypto/Cipher", "getExemptionMechanism", "()Ljavax/crypto/ExemptionMechanism;"),
    ("javax/crypto/Cipher", "getIV", "()[B"),
    ("javax/crypto/Cipher", "getInstance", "(Ljava/lang/String;)Ljavax/crypto/Cipher;"),
    ("javax/crypto/Cipher", "getInstance", "(Ljava/lang/String;Ljava/lang/String;)Ljavax/crypto/Cipher;"),
    ("javax/crypto/Cipher", "getInstance", "(Ljava/lang/String;Ljava/security/Provider;)Ljavax/crypto/Cipher;"),
    ("javax/crypto/Cipher", "getOutputSize", "(I)I"),
    ("javax/crypto/Cipher", "getParameters", "()Ljava/security/AlgorithmParameters;"),
    ("javax/crypto/Cipher", "getProvider", "()Ljava/security/Provider;"),
    ("javax/crypto/Cipher", "init", "(ILjava/security/Key;)V"),
    ("javax/crypto/Cipher", "init", "(ILjava/security/Key;Ljava/security/AlgorithmParameters;)V"),
    ("javax/crypto/Cipher", "init", "(ILjava/security/Key;Ljava/security/AlgorithmParameters;Ljava/security/SecureRandom;)V"),
    ("javax/crypto/Cipher", "init", "(ILjava/security/Key;Ljava/security/SecureRandom;)V"),
    ("javax/crypto/Cipher", "init", "(ILjava/security/Key;Ljava/security/spec/AlgorithmParameterSpec;)V"),
    ("javax/crypto/Cipher", "init", "(ILjava/security/Key;Ljava/security/spec/AlgorithmParameterSpec;Ljava/security/SecureRandom;)V"),
    ("javax/crypto/Cipher", "init", "(ILjava/security/cert/Certificate;)V"),
    ("javax/crypto/Cipher", "init", "(ILjava/security/cert/Certificate;Ljava/security/SecureRandom;)V"),
    ("javax/crypto/Cipher", "unwrap", "([BLjava/lang/String;I)Ljava/security/Key;"),
    ("javax/crypto/Cipher", "update", "(Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)I"),
    ("javax/crypto/Cipher", "update", "([B)[B"),
    ("javax/crypto/Cipher", "update", "([BII)[B"),
    ("javax/crypto/Cipher", "update", "([BII[B)I"),
    ("javax/crypto/Cipher", "update", "([BII[BI)I"),
    ("javax/crypto/Cipher", "updateAAD", "(Ljava/nio/ByteBuffer;)V"),
    ("javax/crypto/Cipher", "updateAAD", "([B)V"),
    ("javax/crypto/Cipher", "updateAAD", "([BII)V"),
    ("javax/crypto/Cipher", "wrap", "(Ljava/security/Key;)[B"),
];

/// SBR round 2, wave 7, lane TAIL: the small rows round 1 held (trial set; each
/// row is attributed by `CRATONVM_UNRETIRE_NATIVE_SHADOW` in the record).
/// Record: `docs/feature-designs/native-dispatch-architecture.md`,
/// held rows: `docs/feature-designs/native-dispatch-architecture.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR2_TAIL_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/io/ByteArrayInputStream", "close", "()V"),
    ("java/io/ByteArrayInputStream", "read", "()I"),
    ("java/io/ByteArrayInputStream", "read", "([BII)I"),
    (
        "java/lang/System$1",
        "findBootstrapClassOrNull",
        "(Ljava/lang/String;)Ljava/lang/Class;",
    ),
    ("java/nio/DirectByteBuffer", "address", "()J"),
    (
        "java/nio/file/FileSystems",
        "getDefault",
        "()Ljava/nio/file/FileSystem;",
    ),
    (
        "java/util/ResourceBundle$1",
        "getParent",
        "(Ljava/util/ResourceBundle;)Ljava/util/ResourceBundle;",
    ),
    (
        "java/util/ResourceBundle$1",
        "setLocale",
        "(Ljava/util/ResourceBundle;Ljava/util/Locale;)V",
    ),
    (
        "java/util/ResourceBundle$1",
        "setParent",
        "(Ljava/util/ResourceBundle;Ljava/util/ResourceBundle;)V",
    ),
    ("javax/crypto/spec/GCMParameterSpec", "<init>", "(I[B)V"),
    ("javax/crypto/spec/GCMParameterSpec", "getIV", "()[B"),
    ("javax/crypto/spec/GCMParameterSpec", "getTLen", "()I"),
    (
        "jdk/internal/constant/ConstantUtils",
        "concat",
        "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/String;)Ljava/lang/String;",
    ),
    ("jdk/internal/event/EventHelper", "isLoggingSecurity", "()Z"),
    (
        "jdk/internal/logger/LazyLoggers",
        "getLogger",
        "(Ljava/lang/String;Ljava/lang/Module;)Ljava/lang/System$Logger;",
    ),
    ("jdk/internal/misc/Unsafe", "allocateMemory", "(J)J"),
    (
        "jdk/internal/misc/Unsafe",
        "compareAndSetByte",
        "(Ljava/lang/Object;JBB)Z",
    ),
    (
        "sun/util/locale/provider/CalendarDataUtility",
        "retrieveJavaTimeFieldValueName",
        "(Ljava/lang/String;IIILjava/util/Locale;)Ljava/lang/String;",
    ),
    (
        "sun/util/locale/provider/CalendarDataUtility",
        "retrieveJavaTimeFieldValueNames",
        "(Ljava/lang/String;IILjava/util/Locale;)Ljava/util/Map;",
    ),
];

/// SBR round 2, wave 8, lane SMALL (2026-09-30): small rows nobody owned
/// (trial set; each row is attributed by `CRATONVM_UNRETIRE_NATIVE_SHADOW` in
/// the record `docs/feature-designs/native-dispatch-architecture.md`, held
/// rows in `docs/feature-designs/native-dispatch-architecture.md`).
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR2_SMALL_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "com/sun/jmx/mbeanserver/MXBeanIntrospector",
        "getMethods",
        "(Ljava/lang/Class;)Ljava/util/List;",
    ),
    (
        "com/sun/jmx/mbeanserver/StandardMBeanIntrospector",
        "getMethods",
        "(Ljava/lang/Class;)Ljava/util/List;",
    ),
    ("java/io/BufferedOutputStream", "close", "()V"),
    (
        "java/io/FileDescriptor$1",
        "set",
        "(Ljava/io/FileDescriptor;I)V",
    ),
    ("java/io/FilterInputStream", "skip", "(J)J"),
    ("java/lang/ref/Reference", "clear", "()V"),
    (
        "java/lang/ref/Reference",
        "reachabilityFence",
        "(Ljava/lang/Object;)V",
    ),
    (
        "java/lang/ref/ReferenceQueue",
        "poll",
        "()Ljava/lang/ref/Reference;",
    ),
];

/// SBR round 2, wave 7, lane CALLER (2026-09-30): `java/lang/reflect/Field`'s
/// value accessors (18) and `javax/crypto/Mac` (17), retired on the wave-6
/// precedent that retired `Cipher` -- the native answers probe rows WRONG that
/// the JDK bytecode answers as HotSpot does (`Sbr2W7CallerMix`, 321 rows:
/// native 39 lines off HotSpot -> retired 8, the 8 being object-hash text and
/// HotSpot's own hot-path NPE message change). Native `Field`: a `null`
/// receiver's NPE carries no message, `set` of a `long[]` into an `int[]`
/// field SUCCEEDS, the access/`InaccessibleObjectException` texts are the
/// VM's own. Native `Mac`: `update`/`update(byte)` before `init` return
/// instead of throwing `IllegalStateException`, `init(key, badSpec)` and
/// `update((ByteBuffer) null)` succeed, `update(b, off, len)` out of range
/// throws the VM's `ArrayIndexOutOfBoundsException` text.
///
/// Cost, stated (Windows, JIT, interleaved, min of reps): `Field.getInt`
/// 3.1 -> 4.0-5.8 us (1.3-1.9x), `setInt` 4.4 -> 6.5 (1.5x), `get`/`set`
/// 1.0-1.3x, `--nojit` 7-8x; `Mac.getInstance` 4.3 -> 24 us (5.5x).
/// `StackWalker` (3 rows) is NOT here: its native is correct on every row and
/// retired it is 25x (`getCallerClass`) / 9x (`walk`) slower.
/// Record: `docs/feature-designs/native-dispatch-architecture.md`.
///
/// **The 18 `Field` rows left this table in interpreter round i3 wave 78
/// (2026-11-11).** The wave-73 doors (`CRATONVM_FIELD_DOORS_INTRINSIC`, default
/// on since wave 78) answer `Sbr2W7CallerMix` and the L3 field probes as
/// HotSpot does in both modes, with and without the JIT, so they serve
/// `--real-jdk` as reviewed `Intrinsic`s; with the switch at `0` they register
/// as `SyntheticStub` there and the strict boot refuses them, which is what
/// the rows did (`NativeMethodRegistry::field_door_kind`). Record:
/// `docs/feature-designs/native-dispatch-architecture.md`. The 17
/// `Mac` rows stay.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR2_CALLER_TRIPLES: &[(&str, &str, &str)] = &[
    ("javax/crypto/Mac", "clone", "()Ljava/lang/Object;"),
    ("javax/crypto/Mac", "doFinal", "()[B"),
    ("javax/crypto/Mac", "doFinal", "([B)[B"),
    ("javax/crypto/Mac", "doFinal", "([BI)V"),
    ("javax/crypto/Mac", "getAlgorithm", "()Ljava/lang/String;"),
    (
        "javax/crypto/Mac",
        "getInstance",
        "(Ljava/lang/String;)Ljavax/crypto/Mac;",
    ),
    (
        "javax/crypto/Mac",
        "getInstance",
        "(Ljava/lang/String;Ljava/lang/String;)Ljavax/crypto/Mac;",
    ),
    (
        "javax/crypto/Mac",
        "getInstance",
        "(Ljava/lang/String;Ljava/security/Provider;)Ljavax/crypto/Mac;",
    ),
    ("javax/crypto/Mac", "getMacLength", "()I"),
    (
        "javax/crypto/Mac",
        "getProvider",
        "()Ljava/security/Provider;",
    ),
    ("javax/crypto/Mac", "init", "(Ljava/security/Key;)V"),
    (
        "javax/crypto/Mac",
        "init",
        "(Ljava/security/Key;Ljava/security/spec/AlgorithmParameterSpec;)V",
    ),
    ("javax/crypto/Mac", "reset", "()V"),
    ("javax/crypto/Mac", "update", "(B)V"),
    ("javax/crypto/Mac", "update", "(Ljava/nio/ByteBuffer;)V"),
    ("javax/crypto/Mac", "update", "([B)V"),
    ("javax/crypto/Mac", "update", "([BII)V"),
];

/// 2026-09-30, SBR round 2 wave 7 lane MH: the `java.lang.invoke` rows whose
/// real JDK bytecode runs correctly over the VM's own MethodHandle/VarHandle
/// carrier, with a measured gain (a fixed answer or a faster call) or no cost.
///
/// **Four rows, each proved by a trial build with ONLY that row retired
/// (`CRATONVM_UNRETIRE_NATIVE_SHADOW` toggles the rest back in one run) and a
/// HotSpot-diffed probe (`Sbr2W7MhRows.java`, 414 rows).** Every
/// other row of the carrier surface breaks a probe row or costs more than the
/// native (record `docs/feature-designs/native-dispatch-architecture.md`,
/// design `docs/feature-designs/native-dispatch-architecture.md`).
///
/// * `VarHandle.varType` / `coordinateTypes`: the real bodies ask
///   `accessModeType(AccessMode)` (kept native) and read the answer; 20x faster
///   than the native (0.06 vs 1.2 us and 0.10 vs 7.7 us per call).
/// * `MethodHandle.toString`: the native answered the constant `MethodHandle`,
///   HotSpot answers `MethodHandle(int)int`; the real body is
///   `"MethodHandle" + type` (4 probe rows fixed). Slower per call (1.2 -> 19.6 us,
///   a diagnostic string): retired on the round-1 rule "a wrong native goes,
///   microseconds notwithstanding".
/// * `MethodHandles.foldArguments(MH, MH)`: a one-line delegator to
///   `foldArguments(MH, int, MH)`, whose native stays; neutral (0.97x). The
///   delegate is now what the suite exercises, so the native-won census swaps
///   one `foldArguments` row for the other rather than shrinking.
///
/// `--compatible` is unchanged: a retired triple registers as `SyntheticStub`
/// there and dispatches normally.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR2_MH_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/lang/invoke/MethodHandle", "toString", "()Ljava/lang/String;"),
    ("java/lang/invoke/MethodHandles", "foldArguments", "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/VarHandle", "coordinateTypes", "()Ljava/util/List;"),
    ("java/lang/invoke/VarHandle", "varType", "()Ljava/lang/Class;"),
];

/// 2026-09-30, SBR round 2 wave 7 lane MH: the rows of the MethodHandle carrier
/// surface that were TRIED and HELD. **Not in `RETIRED_SHADOW_TABLES`** (adding
/// it there re-creates the trial build the record measured: one row per
/// `CRATONVM_UNRETIRE_NATIVE_SHADOW` run, see the HELD page for each row's
/// probe rows and blocker). Kept sorted so it can be spliced into the const
/// unchanged; `#[allow(dead_code)]` because nothing reads it by default.
///
/// The blocker classes, measured (HELD page section 2):
///
/// * **the real body builds a `BoundMethodHandle` species / `LambdaForm` the
///   carrier cannot run** (`BoundMethodHandle.<clinit>`: `Failed to link
///   speciesData to speciesCode`, first cause `Unsafe.putReference(null, 0, ..)`
///   from `MethodHandleNatives.staticFieldBase`; closed behind
///   `CRATONVM_MHN_STATIC_FIELD_REAL=1`, the next blocker is in the design
///   page): identity, constant, insert/drop/filter/collect/permute/guard/catch,
///   `asType`, `asSpreader`, `asCollector`, `asVarargsCollector`, `bindTo`,
///   `invokeWithArguments`, the invokers, the loops, `tryFinally`, `zero`,
///   `explicitCastArguments`;
/// * **the real body returns a real JDK handle the carrier's readers cannot
///   take** (`Lookup.find*`, `unreflect*`, `revealDirect`, `arrayElementGetter/
///   Setter`, `arrayLength`: every native combinator that reads a target's
///   private slots answers `null` / `0` / `WrongMethodTypeException`);
/// * **correct but slower** (`LambdaMetafactory.metafactory` 41x and
///   `altMetafactory` 146x per direct call, `defineHiddenClass*` 4x,
///   `defineClass`; the no-gain delegators `MethodHandles.empty` 2.4x and
///   `countedLoop(MH, MH, MH)` 1.7x construction): held on cost;
/// * **the product is unused** (`InvokerBytecodeGenerator.generate*`: the VM
///   never runs a `LambdaForm`, so spinning the class is pure cost).
#[allow(dead_code)]
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR2_MH_HELD_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/lang/invoke/InvokerBytecodeGenerator", "generateCustomizedCode", "(Ljava/lang/invoke/LambdaForm;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MemberName;"),
    ("java/lang/invoke/LambdaMetafactory", "altMetafactory", "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/invoke/MethodType;[Ljava/lang/Object;)Ljava/lang/invoke/CallSite;"),
    ("java/lang/invoke/LambdaMetafactory", "metafactory", "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/invoke/MethodType;Ljava/lang/invoke/MethodType;Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/CallSite;"),
    ("java/lang/invoke/MethodHandle", "asCollector", "(Ljava/lang/Class;I)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandle", "asFixedArity", "()Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandle", "asSpreader", "(Ljava/lang/Class;I)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandle", "asType", "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandle", "asVarargsCollector", "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandle", "bindTo", "(Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandle", "invokeWithArguments", "([Ljava/lang/Object;)Ljava/lang/Object;"),
    ("java/lang/invoke/MethodHandle", "isVarargsCollector", "()Z"),
    ("java/lang/invoke/MethodHandles", "arrayElementGetter", "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "arrayElementSetter", "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "arrayLength", "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "byteArrayViewVarHandle", "(Ljava/lang/Class;Ljava/nio/ByteOrder;)Ljava/lang/invoke/VarHandle;"),
    ("java/lang/invoke/MethodHandles", "byteBufferViewVarHandle", "(Ljava/lang/Class;Ljava/nio/ByteOrder;)Ljava/lang/invoke/VarHandle;"),
    ("java/lang/invoke/MethodHandles", "catchException", "(Ljava/lang/invoke/MethodHandle;Ljava/lang/Class;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "collectArguments", "(Ljava/lang/invoke/MethodHandle;ILjava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "constant", "(Ljava/lang/Class;Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "countedLoop", "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "doWhileLoop", "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "dropArgumentsTrusted", "(Ljava/lang/invoke/MethodHandle;I[Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "empty", "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "exactInvoker", "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "explicitCastArguments", "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "filterArguments", "(Ljava/lang/invoke/MethodHandle;I[Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "filterReturnValue", "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "guardWithTest", "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "identity", "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "insertArguments", "(Ljava/lang/invoke/MethodHandle;I[Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "invoker", "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "iteratedLoop", "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "permuteArguments", "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodType;[I)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "spreadInvoker", "(Ljava/lang/invoke/MethodType;I)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "tryFinally", "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "whileLoop", "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "zero", "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "defineClass", "([B)Ljava/lang/Class;"),
    ("java/lang/invoke/MethodHandles$Lookup", "defineHiddenClass", "([BZ[Ljava/lang/invoke/MethodHandles$Lookup$ClassOption;)Ljava/lang/invoke/MethodHandles$Lookup;"),
    ("java/lang/invoke/MethodHandles$Lookup", "defineHiddenClassWithClassData", "([BLjava/lang/Object;Z[Ljava/lang/invoke/MethodHandles$Lookup$ClassOption;)Ljava/lang/invoke/MethodHandles$Lookup;"),
    // `Lookup.findClass` left this table for `RETIRED_SHADOW_SBR2_SPRINGAPP_TRIPLES`
    // (wave 8 lane SPRINGAPP: the native hands an UNPRIVILEGED lookup
    // `jdk.internal.misc.Unsafe`, where HotSpot throws `IllegalAccessException`).
    ("java/lang/invoke/MethodHandles$Lookup", "findConstructor", "(Ljava/lang/Class;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "findGetter", "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "findSetter", "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "findSpecial", "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "findStatic", "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "findStaticGetter", "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "findStaticSetter", "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "findStaticVarHandle", "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/VarHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "findVarHandle", "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/VarHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "findVirtual", "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "revealDirect", "(Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandleInfo;"),
    ("java/lang/invoke/MethodHandles$Lookup", "unreflect", "(Ljava/lang/reflect/Method;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "unreflectConstructor", "(Ljava/lang/reflect/Constructor;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "unreflectGetter", "(Ljava/lang/reflect/Field;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "unreflectSetter", "(Ljava/lang/reflect/Field;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "unreflectSpecial", "(Ljava/lang/reflect/Method;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "unreflectVarHandle", "(Ljava/lang/reflect/Field;)Ljava/lang/invoke/VarHandle;"),
    ("java/lang/invoke/VarHandle", "isAccessModeSupported", "(Ljava/lang/invoke/VarHandle$AccessMode;)Z"),
    ("java/lang/invoke/VarHandle", "toMethodHandle", "(Ljava/lang/invoke/VarHandle$AccessMode;)Ljava/lang/invoke/MethodHandle;"),
];

/// SBR round 2, wave 7, lane CLASSREFL: `Class.newInstance` and the two
/// `getRoot` getters.
///
/// * `Class.newInstance`: the real body now runs. Three things made that
///   possible: the mirror's slot 0 (`cachedConstructor`) is a true null under
///   `--real-jdk` (`vm_object.rs::mirror_slot0_overlay`); `ReflectionFactory`'s
///   `<clinit>` first initialises `AccessibleObject` (`vm_util.rs`), so its
///   `langReflectAccess` is no longer captured as null when `newInstance` is the
///   first reflection call of the process; and the genuine HotSpot native
///   `DirectConstructorHandleAccessor$NativeAccessor.newInstance0` is a `Bridge`
///   over the `Constructor.newInstance` body (`lang_reflect.rs`).
/// * `Method.getRoot`/`Constructor.getRoot`: the natives read the same `root`
///   field the JDK getter returns.
///
/// Held, each with its measured cost (HELD page
/// `docs/feature-designs/native-dispatch-architecture.md`):
/// `Class.getTypeName` (array 5-9x), `InvocationHandler.invokeDefault` (default
/// call 3-6x), `RecordComponent.getGenericType` (~6x). Record:
/// `docs/feature-designs/native-dispatch-architecture.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR2_CLASSREFL_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/lang/Class", "newInstance", "()Ljava/lang/Object;"),
    (
        "java/lang/reflect/Constructor",
        "getRoot",
        "()Ljava/lang/reflect/Constructor;",
    ),
    (
        "java/lang/reflect/Method",
        "getRoot",
        "()Ljava/lang/reflect/Method;",
    ),
];

/// SBR round 2, wave 8, lane SPRINGAPP: the JDK rows a real workload (the
/// Spring Boot 4.2 sample under `--real-jdk`) reaches and the regression suite
/// does not. TRIAL table: rows are bisected in runs with
/// `CRATONVM_UNRETIRE_NATIVE_SHADOW`; the record pages say which stay.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR2_SPRINGAPP_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/beans/Introspector",
        "decapitalize",
        "(Ljava/lang/String;)Ljava/lang/String;",
    ),
    (
        "java/lang/Class",
        "getGenericInterfaces",
        "()[Ljava/lang/reflect/Type;",
    ),
    (
        "java/lang/Class",
        "getGenericSuperclass",
        "()Ljava/lang/reflect/Type;",
    ),
    ("java/lang/System", "console", "()Ljava/io/Console;"),
    (
        "java/lang/System",
        "getSecurityManager",
        "()Ljava/lang/SecurityManager;",
    ),
    (
        "java/lang/invoke/MethodHandles$Lookup",
        "findClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
    ),
    (
        "java/lang/reflect/Constructor",
        "getParameters",
        "()[Ljava/lang/reflect/Parameter;",
    ),
    (
        "java/lang/reflect/Method",
        "getParameters",
        "()[Ljava/lang/reflect/Parameter;",
    ),
    ("java/lang/reflect/Parameter", "isNamePresent", "()Z"),
    ("java/util/concurrent/CopyOnWriteArraySet", "<init>", "()V"),
    ("java/util/concurrent/CopyOnWriteArraySet", "isEmpty", "()Z"),
    (
        "java/util/concurrent/CopyOnWriteArraySet",
        "iterator",
        "()Ljava/util/Iterator;",
    ),
    (
        "jdk/internal/module/SystemModuleFinders$SystemModuleFinder",
        "findAll",
        "()Ljava/util/Set;",
    ),
    (
        "sun/reflect/annotation/AnnotatedTypeFactory$AnnotatedParameterizedTypeImpl",
        "getAnnotatedActualTypeArguments",
        "()[Ljava/lang/reflect/AnnotatedType;",
    ),
];

/// SBR round 2, wave 8, lane SPRINGAPP: HELD, deliberately NOT in
/// `RETIRED_SHADOW_TABLES` (the same convention as `RETIRED_SHADOW_SBR2_MH_HELD_TRIPLES`).
/// `Class.getResource`: the real body answers `null` for every `jrt:` resource
/// (`String.class.getResource("String.class")`, `Object.class.getResource("/java/lang/Object.class")`,
/// an array class's) on the Windows trial host, where the `jimage` library is
/// missing, and is right on every other row (`null` name NPE, empty name).
/// Candidate for retirement once the orchestrator's Linux run of
/// `Sbr2W8SpringappJdk.java` shows the `res ` rows equal HotSpot.
/// See `docs/feature-designs/native-dispatch-architecture.md`.
#[allow(dead_code)]
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR2_SPRINGAPP_HELD_TRIPLES: &[(&str, &str, &str)] = &[
    // `Class.arrayType`: the real body answers a stand-in `Object[]` for a lambda
    // class and `componentType == proxy class` false for a `Proxy` class (the
    // native is right for the proxy, wrong for the lambda); VM-synthesised classes
    // have no array class. Also 2.4x slower (0.9 -> 2.1 us). Unchanged from L0.
    ("java/lang/Class", "arrayType", "()Ljava/lang/Class;"),
    // (`Class.getResource` was RE-EARNED by SBR2 wave 9: `RETIRED_SHADOW_SBR2_MODLOAD_TRIPLES`.)
];

/// SBR round 2, wave 8, lane SPRINGAPP: the APPLICATION-class rows (Spring,
/// jboss-logging) whose natives shadow framework bytecode. Unusual for this
/// file, which is about the JDK: the prefixes are the framework class names.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR2_SPRINGAPP_APP_TRIPLES: &[(&str, &str, &str)] = &[
    ("org/jboss/logging/Slf4jLocationAwareLogger", "doLog", "(Lorg/jboss/logging/Logger$Level;Ljava/lang/String;Ljava/lang/Object;[Ljava/lang/Object;Ljava/lang/Throwable;)V"),
    ("org/jboss/logging/Slf4jLocationAwareLogger", "doLogf", "(Lorg/jboss/logging/Logger$Level;Ljava/lang/String;Ljava/lang/String;[Ljava/lang/Object;Ljava/lang/Throwable;)V"),
    // The JBoss subclass ctor must run: its `super()` is the real (already retired)
    // `java.util.logging.LogManager.<init>`, which builds `configurationLock` & co.
    // The no-op native left them null -> NPE in `ensureLogManagerInitialized` under
    // `-Djava.util.logging.manager=org.jboss.logmanager.LogManager`.
    // docs/feature-designs/native-dispatch-architecture.md
    ("org/jboss/logmanager/LogManager", "<init>", "()V"),
    ("org/springframework/beans/GenericTypeAwarePropertyDescriptor", "getPropertyType", "()Ljava/lang/Class;"),
    ("org/springframework/beans/GenericTypeAwarePropertyDescriptor", "getReadMethod", "()Ljava/lang/reflect/Method;"),
    ("org/springframework/beans/GenericTypeAwarePropertyDescriptor", "getWriteMethod", "()Ljava/lang/reflect/Method;"),
    ("org/springframework/boot/SpringApplicationBannerPrinter", "getTextBanner", "(Lorg/springframework/core/env/Environment;)Lorg/springframework/boot/Banner;"),
    ("org/springframework/util/ClassUtils", "forName", "(Ljava/lang/String;Ljava/lang/ClassLoader;)Ljava/lang/Class;"),
    ("org/springframework/util/ClassUtils", "isPresent", "(Ljava/lang/String;Ljava/lang/ClassLoader;)Z"),
    ("org/springframework/util/ResourceUtils", "isFileURL", "(Ljava/net/URL;)Z"),
    ("org/springframework/util/ResourceUtils", "isJarURL", "(Ljava/net/URL;)Z"),
    ("org/springframework/util/ResourceUtils", "useCachesIfNecessary", "(Ljava/net/URLConnection;)V"),
];

/// 2026-10-04, Spring Boot residual retirement: `Class.getAnnotatedInterfaces`
/// and `Class.getAnnotatedSuperclass`.
///
/// SBR wave 5 (REFLMETA) made the machinery the real bodies read answer under
/// `--real-jdk`: `Class.getRawTypeAnnotations` returns the class's real
/// `RuntimeVisibleTypeAnnotations` bytes and `jdk.internal.reflect.ConstantPool`
/// is live, so the real `TypeAnnotationParser.buildAnnotatedInterfaces` /
/// `buildAnnotatedSuperclass` produce the same `AnnotatedType` tree HotSpot does
/// (measured: `implements ValueExtractor<ArgumentValue<@ExtractedValue ?>>`
/// reports the wildcard's annotation through the real path and loses it through
/// the native). The wave retired `AnnotatedParameterizedTypeImpl.
/// getAnnotatedActualTypeArguments` for exactly that reason, but left these two
/// natives in front: they minted CratonVM-shaped `AnnotatedType`s (an
/// `Annotation[]` in the `annotations` field the JDK declares as a `Map`) whose
/// `getAnnotatedActualTypeArguments()` now ran the REAL body against a
/// non-real receiver and answered every nested argument with no annotations.
/// Hibernate Validator's `ValueExtractorResolver` then rejected Spring GraphQL's
/// `ArgumentValueValueExtractor` with `HV000203: ... fails to declare the
/// extracted type parameter using @ExtractedValue`, so every context that built
/// a `defaultValidator` bean failed to start (`ControllerEndpointDiscovererTests`,
/// `ConfigurationPropertiesReportEndpointProxyTests`).
///
/// `--compatible` is unchanged: the rows register and dispatch there as the
/// `Bridge`s they were.
///
/// Control and kill switch (two rows, one rule each):
/// `CRATONVM_UNRETIRE_NATIVE_SHADOW=java/lang/Class.getAnnotatedInterfaces,java/lang/Class.getAnnotatedSuperclass`.
/// Record: `docs/feature-designs/class-resolution-and-caching.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SB_CLASS_ANNOTATED_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/lang/Class",
        "getAnnotatedInterfaces",
        "()[Ljava/lang/reflect/AnnotatedType;",
    ),
    (
        "java/lang/Class",
        "getAnnotatedSuperclass",
        "()Ljava/lang/reflect/AnnotatedType;",
    ),
];

/// SBR round 2, wave 8, lane CTORPLAN: the rows wave 7 held for the cost of
/// reflective construction, re-tried after the per-`Constructor` plan
/// (`lang_class.rs::CtorPlan`, named by the constructor's `slot`) and the
/// id-only `Reflection.getCallerClass`. ONE row retires, for correctness, at a
/// stated cost: `InvocationHandler.invokeDefault` (the native answered `1`
/// where HotSpot throws; the JDK body throws, 2.7x the native's default call).
/// `StackWalker.walk/forEach/getCallerClass`, `Class.getTypeName` and
/// `RecordComponent.getGenericType` stay native (re-measured, still 2-38x).
/// Records:
/// `docs/feature-designs/native-dispatch-architecture.md`,
/// `docs/feature-designs/native-dispatch-architecture.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR2_CTORPLAN_TRIPLES: &[(&str, &str, &str)] = &[(
    "java/lang/reflect/InvocationHandler",
    "invokeDefault",
    "(Ljava/lang/Object;Ljava/lang/reflect/Method;[Ljava/lang/Object;)Ljava/lang/Object;",
)];

/// 2026-10-01, SBR round 2 wave 9 lane MHFIX: **no row retires.** The lane fixed
/// the carrier defect (`MH_KIND_SPREAD` converts the spread elements against the
/// target's parameter types before the target runs, so a failed conversion is
/// thrown outside a `catchException` the target sits in: `InvocationHandler.invokeDefault`
/// with a wrong or `null` argument throws HotSpot's `IllegalArgumentException`
/// wrapping the `ClassCastException` / `NullPointerException`), then retired each
/// row the lane brief named ALONE (`asFixedArity`, `asSpreader`, `asType`,
/// `isVarargsCollector`, `withVarargs`, `foldArguments(MH, int, MH)`,
/// `Lookup.findSpecial`, `Lookup.unreflectConstructor`) against a probe of 400+
/// rows diffed with HotSpot 25: every one moves rows OFF HotSpot's line (the
/// carrier is not a `BoundMethodHandle` / `DelegatingMethodHandle`, and the real
/// handle a `find*` yields is unreadable by the carrier's combinators), so they
/// stay native. Records:
/// `docs/feature-designs/native-dispatch-architecture.md`,
/// `docs/feature-designs/native-dispatch-architecture.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR2_MHFIX_TRIPLES: &[(&str, &str, &str)] = &[];

/// 2026-10-04, SBR round 3 lane mh: NOTHING retired. The whole MethodHandle
/// carrier surface is held as a family (`RETIRED_SHADOW_SBR3_MH_HELD_TRIPLES`).
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR3_MH_TRIPLES: &[(&str, &str, &str)] = &[];

/// 2026-10-04, SBR round 3 lane mh: HELD, deliberately NOT in
/// `RETIRED_SHADOW_TABLES`. The registered MethodHandle surface that the
/// SBR2 `RETIRED_SHADOW_SBR2_MH_HELD_TRIPLES` did not name (the siblings the
/// suite never reaches) plus the two `InvokerBytecodeGenerator` rows the real
/// stack needs and the `asSpreader(int, Class, int)` / `withVarargs` /
/// `foldArguments(MH, int, MH)` rows of the census. Together with the SBR2 held
/// table this is the set that must retire AS ONE FAMILY: a carrier combinator
/// left native reads garbage from a real handle a retired `Lookup.find*`
/// built (a `StackOverflowError` in `asCollector`, `null` from a door), and
/// the other way round. Measured, trial binary `cvm-sbr3-mh-6` (both tables
/// listed): default `--real-jdk` suite 152/152 (see the page), probe 412 of 414
/// rows on HotSpot's line, native-won census 135 -> 95. HELD on cost: a call
/// through a real handle is 7-60x slower. Page:
/// `docs/feature-designs/native-dispatch-architecture.md`.
/// SORTED: the lookup binary-searches each table.
#[allow(dead_code)]
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR3_MH_HELD_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/lang/invoke/InvokerBytecodeGenerator", "generateCustomizedCode", "(Ljava/lang/invoke/LambdaForm;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MemberName;"),
    ("java/lang/invoke/InvokerBytecodeGenerator", "generateLambdaFormInterpreterEntryPoint", "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MemberName;"),
    ("java/lang/invoke/InvokerBytecodeGenerator", "generateNamedFunctionInvoker", "(Ljava/lang/invoke/MethodTypeForm;)Ljava/lang/invoke/MemberName;"),
    ("java/lang/invoke/MethodHandle", "asCollector", "(ILjava/lang/Class;I)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandle", "asCollector", "(Ljava/lang/Class;I)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandle", "asFixedArity", "()Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandle", "asSpreader", "(ILjava/lang/Class;I)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandle", "asSpreader", "(Ljava/lang/Class;I)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandle", "asType", "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandle", "asVarargsCollector", "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandle", "bindTo", "(Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandle", "invokeWithArguments", "(Ljava/util/List;)Ljava/lang/Object;"),
    ("java/lang/invoke/MethodHandle", "invokeWithArguments", "([Ljava/lang/Object;)Ljava/lang/Object;"),
    ("java/lang/invoke/MethodHandle", "isVarargsCollector", "()Z"),
    ("java/lang/invoke/MethodHandle", "withVarargs", "(Z)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandleProxies", "asInterfaceInstance", "(Ljava/lang/Class;Ljava/lang/invoke/MethodHandle;)Ljava/lang/Object;"),
    ("java/lang/invoke/MethodHandleProxies", "isWrapperInstance", "(Ljava/lang/Object;)Z"),
    ("java/lang/invoke/MethodHandleProxies", "wrapperInstanceTarget", "(Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "arrayElementGetter", "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "byteArrayViewVarHandle", "(Ljava/lang/Class;Ljava/nio/ByteOrder;)Ljava/lang/invoke/VarHandle;"),
    ("java/lang/invoke/MethodHandles", "catchException", "(Ljava/lang/invoke/MethodHandle;Ljava/lang/Class;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "collectArguments", "(Ljava/lang/invoke/MethodHandle;ILjava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "collectReturnValue", "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "constant", "(Ljava/lang/Class;Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "countedLoop", "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "dropArguments", "(Ljava/lang/invoke/MethodHandle;I[Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "dropArgumentsTrusted", "(Ljava/lang/invoke/MethodHandle;I[Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "dropReturn", "(Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "filterArguments", "(Ljava/lang/invoke/MethodHandle;I[Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "filterReturnValue", "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "foldArguments", "(Ljava/lang/invoke/MethodHandle;ILjava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "guardWithTest", "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "identity", "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "insertArguments", "(Ljava/lang/invoke/MethodHandle;I[Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "permuteArguments", "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodType;[I)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "tableSwitch", "(Ljava/lang/invoke/MethodHandle;[Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "throwException", "(Ljava/lang/Class;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "varHandleExactInvoker", "(Ljava/lang/invoke/VarHandle$AccessMode;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "varHandleInvoker", "(Ljava/lang/invoke/VarHandle$AccessMode;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "findConstructor", "(Ljava/lang/Class;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "findGetter", "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "findSetter", "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "findSpecial", "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "findStatic", "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "findStaticGetter", "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "findStaticVarHandle", "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/VarHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "findVarHandle", "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/VarHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "findVirtual", "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "unreflect", "(Ljava/lang/reflect/Method;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "unreflectConstructor", "(Ljava/lang/reflect/Constructor;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "unreflectGetter", "(Ljava/lang/reflect/Field;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "unreflectSetter", "(Ljava/lang/reflect/Field;)Ljava/lang/invoke/MethodHandle;"),
];

/// 2026-10-05, SBR round 3 wave 2 lane mhperf: the MethodHandle family, retired
/// AS ONE UNIT (the union of the SBR2 and SBR3 held tables). SORTED: binary-searched.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR4_MHCONS_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/lang/invoke/InvokerBytecodeGenerator", "generateCustomizedCode", "(Ljava/lang/invoke/LambdaForm;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MemberName;"),
    ("java/lang/invoke/InvokerBytecodeGenerator", "generateLambdaFormInterpreterEntryPoint", "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MemberName;"),
    ("java/lang/invoke/InvokerBytecodeGenerator", "generateNamedFunctionInvoker", "(Ljava/lang/invoke/MethodTypeForm;)Ljava/lang/invoke/MemberName;"),
    ("java/lang/invoke/LambdaMetafactory", "altMetafactory", "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/invoke/MethodType;[Ljava/lang/Object;)Ljava/lang/invoke/CallSite;"),
    ("java/lang/invoke/LambdaMetafactory", "metafactory", "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/invoke/MethodType;Ljava/lang/invoke/MethodType;Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/CallSite;"),
    ("java/lang/invoke/MethodHandle", "asCollector", "(ILjava/lang/Class;I)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandle", "asCollector", "(Ljava/lang/Class;I)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandle", "asFixedArity", "()Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandle", "asSpreader", "(ILjava/lang/Class;I)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandle", "asSpreader", "(Ljava/lang/Class;I)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandle", "asVarargsCollector", "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandle", "isVarargsCollector", "()Z"),
    ("java/lang/invoke/MethodHandle", "withVarargs", "(Z)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandleProxies", "asInterfaceInstance", "(Ljava/lang/Class;Ljava/lang/invoke/MethodHandle;)Ljava/lang/Object;"),
    ("java/lang/invoke/MethodHandleProxies", "isWrapperInstance", "(Ljava/lang/Object;)Z"),
    ("java/lang/invoke/MethodHandleProxies", "wrapperInstanceTarget", "(Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "arrayElementGetter", "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "arrayElementSetter", "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "arrayLength", "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "byteArrayViewVarHandle", "(Ljava/lang/Class;Ljava/nio/ByteOrder;)Ljava/lang/invoke/VarHandle;"),
    ("java/lang/invoke/MethodHandles", "byteBufferViewVarHandle", "(Ljava/lang/Class;Ljava/nio/ByteOrder;)Ljava/lang/invoke/VarHandle;"),
    ("java/lang/invoke/MethodHandles", "catchException", "(Ljava/lang/invoke/MethodHandle;Ljava/lang/Class;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "collectArguments", "(Ljava/lang/invoke/MethodHandle;ILjava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "collectReturnValue", "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "constant", "(Ljava/lang/Class;Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "countedLoop", "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "countedLoop", "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "doWhileLoop", "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "dropArguments", "(Ljava/lang/invoke/MethodHandle;I[Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "dropArgumentsTrusted", "(Ljava/lang/invoke/MethodHandle;I[Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "dropReturn", "(Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "empty", "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "exactInvoker", "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "explicitCastArguments", "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "filterArguments", "(Ljava/lang/invoke/MethodHandle;I[Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "filterReturnValue", "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "foldArguments", "(Ljava/lang/invoke/MethodHandle;ILjava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "guardWithTest", "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "identity", "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "insertArguments", "(Ljava/lang/invoke/MethodHandle;I[Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "invoker", "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "iteratedLoop", "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "permuteArguments", "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodType;[I)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "spreadInvoker", "(Ljava/lang/invoke/MethodType;I)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "tableSwitch", "(Ljava/lang/invoke/MethodHandle;[Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "throwException", "(Ljava/lang/Class;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "tryFinally", "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "varHandleExactInvoker", "(Ljava/lang/invoke/VarHandle$AccessMode;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "varHandleInvoker", "(Ljava/lang/invoke/VarHandle$AccessMode;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "whileLoop", "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles", "zero", "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "defineClass", "([B)Ljava/lang/Class;"),
    ("java/lang/invoke/MethodHandles$Lookup", "defineHiddenClass", "([BZ[Ljava/lang/invoke/MethodHandles$Lookup$ClassOption;)Ljava/lang/invoke/MethodHandles$Lookup;"),
    ("java/lang/invoke/MethodHandles$Lookup", "defineHiddenClassWithClassData", "([BLjava/lang/Object;Z[Ljava/lang/invoke/MethodHandles$Lookup$ClassOption;)Ljava/lang/invoke/MethodHandles$Lookup;"),
    ("java/lang/invoke/MethodHandles$Lookup", "findConstructor", "(Ljava/lang/Class;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "findGetter", "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "findSetter", "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "findSpecial", "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "findStatic", "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "findStaticGetter", "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "findStaticSetter", "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "findStaticVarHandle", "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/VarHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "findVarHandle", "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/VarHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "findVirtual", "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "revealDirect", "(Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandleInfo;"),
    ("java/lang/invoke/MethodHandles$Lookup", "unreflect", "(Ljava/lang/reflect/Method;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "unreflectConstructor", "(Ljava/lang/reflect/Constructor;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "unreflectGetter", "(Ljava/lang/reflect/Field;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "unreflectSetter", "(Ljava/lang/reflect/Field;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "unreflectSpecial", "(Ljava/lang/reflect/Method;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/MethodHandles$Lookup", "unreflectVarHandle", "(Ljava/lang/reflect/Field;)Ljava/lang/invoke/VarHandle;"),
    ("java/lang/invoke/MutableCallSite", "dynamicInvoker", "()Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/VarHandle", "isAccessModeSupported", "(Ljava/lang/invoke/VarHandle$AccessMode;)Z"),
    ("java/lang/invoke/VarHandle", "toMethodHandle", "(Ljava/lang/invoke/VarHandle$AccessMode;)Ljava/lang/invoke/MethodHandle;"),
    ("java/lang/invoke/VolatileCallSite", "dynamicInvoker", "()Ljava/lang/invoke/MethodHandle;"),
];

/// 2026-10-01, SBR round 2 wave 9, lane SMALL2: `ReferenceQueue.remove(long)`.
/// Wave 8 held it: as bytecode it was 12x slower on an empty-queue timed wait
/// because `Object.wait(long)` rounded its deadline up to the 15.625 ms
/// Windows tick. The monitor's timed wait now parks on a high-resolution timer
/// (`monitor.rs`, `timed_event`): 473-485 ms for 300 `remove(1)` against the
/// native's 385-426 ms (HotSpot 562 ms on the same host).
///
/// `StackTraceElement.computeFormat` is NOT here: its wave-8 blocker (the real
/// body printed `java.base@25.0.3/...`) is fixed -- the boot `java.base`
/// reference now carries `recordedHashes`, so `StackTraceElement.HashedModules`
/// answers HotSpot's set -- but the real body costs ~7 us per frame against
/// the native's ~0.2 us (`getStackTrace` 2.3x), which is a maintainer call; one
/// row retires it. Records:
/// `docs/feature-designs/native-dispatch-architecture.md`,
/// `docs/feature-designs/native-dispatch-architecture.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR2_SMALL2_TRIPLES: &[(&str, &str, &str)] = &[(
    "java/lang/ref/ReferenceQueue",
    "remove",
    "(J)Ljava/lang/ref/Reference;",
)];

/// Round 15 wave 1 (lane shadow), 2026-09-30 -- `System.setIn` / `setOut` /
/// `setErr`, the first family the CA6-1 thin-wrapper census
/// (`native-builtins/tests/r15_shadow_thin_wrapper_census.rs`) names: a Java
/// body that is one call of an `ACC_NATIVE` method whose `Bridge` is live,
/// the `Array.newInstance` species.
///
/// # The screen, read on this tree
///
/// * Pure-Java bytecode to yield to: JDK 25 `setOut(out)` is `setOut0(out);`
///   (same for `setIn` / `setErr`); 17 and 21 run `checkIO()` first, which
///   asks `getSecurityManager()` (a live `Bridge`) and checks
///   `RuntimePermission("setIO")` only when a manager is installed -- the
///   HotSpot behaviour the natives skipped.
/// * Same answer: the three natives (`native-builtins/src/lib.rs`,
///   `register_essential_natives_with_shims`, each triple registered once, no
///   `real_jdk()` branch) were copies of the `setIn0` / `setOut0` / `setErr0`
///   bodies beside them (write the static field, then file or clear the
///   per-VM override row); this wave binds each pair to ONE function
///   (`native_system_set_in` / `_out` / `_err`) and states both kinds
///   `Bridge`. The `*0` natives stay live `Bridge`s, so the real bodies reach
///   exactly the function that answered before.
/// * State: none (static methods). Unconstructed carrier: none. VM
///   invariant: none -- nothing in `vm/`, `jit/` or `native-*/` looks the
///   three triples up by name (`getstatic System.out` honours the static
///   field the `*0` natives write; `vm_init.rs`' S9 test names only the `*0`
///   triples). `java/lang/System`'s only real-layout drop arm names
///   `initPhase1..3`.
///
/// `--compatible` is unchanged: a `SyntheticStub` registers and dispatches
/// there as the `Bridge` did.
///
/// Control and kill switch (three rules, one row each):
/// `CRATONVM_UNRETIRE_NATIVE_SHADOW=java/lang/System.setIn,java/lang/System.setOut,java/lang/System.setErr`.
/// Probe: `C:\craton\jitr15-probes\src\R15ShadowThinWrappers.java`. Written
/// from reading, not from a trial binary.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SYSTEM_STREAM_SETTER_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/lang/System", "setErr", "(Ljava/io/PrintStream;)V"),
    ("java/lang/System", "setIn", "(Ljava/io/InputStream;)V"),
    ("java/lang/System", "setOut", "(Ljava/io/PrintStream;)V"),
];

/// Round 15 wave 1 (lane shadow), 2026-09-30 -- the `NetworkChannel`
/// covariant-return rows on the two `sun.nio.ch` TCP channel classes, a
/// CA6-1 census family: every row is a javac-generated `ACC_BRIDGE` method
/// (or, for `ServerSocketChannelImpl.bind`, a row inherited from one on
/// `ServerSocketChannel`) whose body is `aload_0; aload_1..; invokevirtual
/// <the covariant overload>; areturn`.
///
/// # The screen, read on this tree
///
/// * The body reads no field and has no logic: it forwards to the covariant
///   overload on the SAME receiver. `ServerSocketChannel.bind(SocketAddress)`
///   is `bind(local, 0)`, which `ssc_bind` already treats as the one-argument
///   form (`int_arg(args, 2)` answers 0 for a missing backlog).
/// * Same answer: `register_socket_channel_real`
///   (`native-io/src/socket_channel.rs`, explicit `Bridge` scope, every arm,
///   each triple registered once) binds the SAME function to both
///   descriptors (`sc_bind`, `sc_set_option`, `ssc_bind`), and the
///   covariant rows stay live `Bridge`s, so the real bodies reach exactly
///   the native that answered before.
/// * Unconstructed carrier: not observable -- nothing here reads a field.
///   The abstract `java/nio/channels/SocketChannel` / `ServerSocketChannel`
///   rows are deliberately NOT here: both classes are mint sites
///   (`try_alloc_concurrent_synthetic`), so they would widen
///   `unconstructed_carrier_gate.rs`' population for no behavioural gain.
///   Those receivers keep today's natives.
/// * `DatagramChannelImpl.setOption(..)NetworkChannel` is NOT here: both of
///   its descriptors are registered twice (kind-map ordinals 0 and 1;
///   `native-io` `register_datagram_channel` runs from two call sites, and
///   the `Impl` rows come from another registrar), and which registration
///   owns each descriptor last was not screened this wave.
///
/// `--compatible` is unchanged: a `SyntheticStub` registers and dispatches
/// there as the `Bridge` did (no `sun/nio/ch/` class is on a real-layout
/// drop arm).
///
/// Control and kill switch (five rules, one row each):
/// `CRATONVM_UNRETIRE_NATIVE_SHADOW=sun/nio/ch/ServerSocketChannelImpl.bind,sun/nio/ch/ServerSocketChannelImpl.setOption(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/NetworkChannel;,sun/nio/ch/SocketChannelImpl.bind(Ljava/net/SocketAddress;)Ljava/nio/channels/NetworkChannel;,sun/nio/ch/SocketChannelImpl.setOption(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/NetworkChannel;`
/// (the first rule matches the two `bind` rows). Written from reading, not
/// from a trial binary.
///
/// # Windows: the table is EMPTY (2026-10-02, measured)
///
/// The screen above holds only where the channel is the JDK's own object, which
/// is Linux (see `RETIRED_SHADOW_SBR_NIOREAL_TRIPLES`, same cause and same
/// platform split). On Windows the bridge natives answer for the channels this
/// VM minted.
#[cfg(not(windows))]
pub(in crate::retired_shadow) static RETIRED_SHADOW_NIO_COVARIANT_BRIDGE_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "bind",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/NetworkChannel;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "bind",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/ServerSocketChannel;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "setOption",
        "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/NetworkChannel;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "bind",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/NetworkChannel;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "setOption",
        "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/NetworkChannel;",
    ),
];
#[cfg(windows)]
pub(in crate::retired_shadow) static RETIRED_SHADOW_NIO_COVARIANT_BRIDGE_TRIPLES: &[(&str, &str, &str)] = &[];

/// The javac covariant bridges of the channel classes, for Windows: the same rows as the table above, added
/// by `retired_shadow_row_set` together with `RETIRED_SHADOW_SBR_NIOREAL_WINDOWS_TRIPLES` and only with the
/// Windows channel layer (a row of the real classes the VM has a bridge native over).
#[cfg(windows)]
pub(in crate::retired_shadow) static RETIRED_SHADOW_NIO_COVARIANT_BRIDGE_WINDOWS_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "bind",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/NetworkChannel;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "bind",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/ServerSocketChannel;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "setOption",
        "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/NetworkChannel;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "bind",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/NetworkChannel;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "setOption",
        "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/NetworkChannel;",
    ),
];

/// Round 15 wave 1 (lane shadow), 2026-09-30 -- three delegators on the
/// JDK's reflective generic-type objects, a CA6-1 census family:
///
/// * `ParameterizedTypeImpl.getRawType()Ljava/lang/reflect/Type;` -- the
///   javac `ACC_BRIDGE` for the covariant `getRawType()Ljava/lang/Class;`;
/// * `TypeVariableImpl.toString()` -- `return getName();`;
/// * `TypeVariableImpl.getDeclaredAnnotations()` -- `return getAnnotations();`.
///
/// # The screen, read on this tree
///
/// * Each body is one virtual call on `this`, no field read. The targets
///   (`getRawType()Class`, `getName`, `getAnnotations`) stay live `Bridge`s,
///   all registered by `register_wp2_1_natives`
///   (`native-builtins/src/lang_reflect.rs`, explicit `Bridge` scope, each
///   triple once, no `real_jdk()` branch).
/// * Same answer: the two `getRawType` natives are the same body (`rawType`
///   by resolved index). The two annotation natives both answer the empty
///   array. `toString` differed only on a null `name` ("?" against the
///   real `null`), which a real `TypeVariableImpl` never has (its
///   constructor writes the final field); and `toString` read `name` with
///   the descriptor-blind `get_field_by_name` where `getName` uses
///   `field_read::ref_field`.
/// * Unconstructed carrier: none -- no `try_alloc_concurrent_synthetic(..,
///   "sun/reflect/generics/..")` site. The VM's own type-variable
///   representation is an instance of the INTERFACE
///   `java/lang/reflect/TypeVariable` (`vm_exec.rs`
///   `prefer_exact_class_native`), a different class, untouched.
/// * VM invariant: none -- the proxy fallback that re-dispatches
///   `getRawType` by descriptor (`vm_exec.rs`
///   `proxy_method_write_extra_slots`) now reaches the bridge bytecode and
///   through it the same native.
///
/// `--compatible` is unchanged: a `SyntheticStub` registers and dispatches
/// there as the `Bridge` did.
///
/// Control and kill switch (three rules, one row each):
/// `CRATONVM_UNRETIRE_NATIVE_SHADOW=sun/reflect/generics/reflectiveObjects/ParameterizedTypeImpl.getRawType()Ljava/lang/reflect/Type;,sun/reflect/generics/reflectiveObjects/TypeVariableImpl.toString,sun/reflect/generics/reflectiveObjects/TypeVariableImpl.getDeclaredAnnotations`.
/// Probe: `C:\craton\jitr15-probes\src\R15ShadowThinWrappers.java`. Written
/// from reading, not from a trial binary.
pub(in crate::retired_shadow) static RETIRED_SHADOW_REFLECT_GENERICS_DELEGATE_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "sun/reflect/generics/reflectiveObjects/ParameterizedTypeImpl",
        "getRawType",
        "()Ljava/lang/reflect/Type;",
    ),
    (
        "sun/reflect/generics/reflectiveObjects/TypeVariableImpl",
        "getDeclaredAnnotations",
        "()[Ljava/lang/annotation/Annotation;",
    ),
    (
        "sun/reflect/generics/reflectiveObjects/TypeVariableImpl",
        "toString",
        "()Ljava/lang/String;",
    ),
];

/// Round 16 wave 15 (lane shadow), 2026-10-02 -- the two `KeyStoreSpi`
/// engine surfaces SBR wave 5 (lane JSSE) left behind:
/// `com/sun/crypto/provider/JceKeyStore` (SunJCE `KeyStore.JCEKS`) and
/// `sun/security/provider/DomainKeyStore$DKS` (SUN `KeyStore.DKS`), 20 rows
/// each -- the same twenty `engine*` triples `keystore::register_engine_surface`
/// binds on every advertised keystore class (16 + 17 declared, 4 + 3 inherited
/// from `KeyStoreSpi`). JSSE retired the six PKCS12/JKS classes' surfaces and
/// did not list these two; nothing on record held them.
///
/// # The screen, read on this tree
///
/// * Pure-Java bytecode to yield to: every row has `Code` on JDK 25 (`javap`,
///   the census below). `engineGetAttributes` is declared on 18+ only, and is
///   retired here anyway where JSSE held it for its six classes: on 17
///   `KeyStore.getAttributes` does not exist, so no call can reach the row,
///   and on 21/25 a held `engineGetAttributes` would be the one native left
///   answering from a side table nothing fills once `engineLoad` is real.
/// * State: the natives ARE a second object model -- entries in
///   `keystore.rs`' side table keyed by receiver identity, the real
///   `JceKeyStore.entries` / `DomainKeyStore.keystores` never written -- so
///   the surface moves as one unit, as JSSE's did.
/// * Unconstructed carrier: none. No `try_alloc_concurrent_synthetic(..,
///   "com/sun/crypto/provider/JceKeyStore" | "sun/security/provider/DomainKeyStore$DKS")`
///   site; both receivers come from the real provider's `newInstance`
///   (`provider_chain.rs` names the real classes) through their real
///   constructors.
/// * VM invariant: none. Nothing in `vm/` names either class; the
///   `KeyManagerFactory.init` force arms (`native_override.rs`,
///   `vm_exec.rs`) are `--compatible`-only since JSSE, and the real manager
///   reads keys through the real `KeyStore.getKey` -> `engineGetKey`.
/// * Registration: `keystore::register_keystore_real` (`lib.rs`
///   `register_essential_natives_with_shims`), one explicit `Bridge` scope,
///   each triple once (kind-map ordinal 0), no `real_jdk()` branch.
/// * HotSpot difference today (`--real-jdk`): yes, many.
///   `R16ShadowRetire` (`C:\craton\jitr16-probes\src\`), dev binary, natives
///   live: JCEKS `getKey` with a wrong password answers `Cannot recover key`
///   (HotSpot: the PBE padding message), `getCertificateAlias` finds the key
///   entry's chain before the trusted entry, a stored-and-reloaded JCEKS loses
///   its secret keys (`getKey` null, NPE), `load(.., null)` drops two of four
///   entries, a garbage stream answers this VM's own format message; DKS is
///   served by the JKS engine (`provider_chain.rs` says so), so aliases lose
///   their `iostream1 ` domain prefix, writes the real DKS refuses succeed,
///   and `load(DomainLoadStoreParameter)` loads nothing (0 entries).
///
/// # Measured (Windows, `cratonvm-jitr16-dev.exe`, the dial standing in)
///
/// `CRATONVM_ENFORCE_NATIVE_SHADOW` over the two classes plus the prefixes
/// this tree already retires and that binary predates (`javax/crypto/`,
/// `sun/security/jca/`, the JKS/PKCS12 SPIs, `java/security/KeyStore`):
/// all 51 `J*`/`D*` lines of `R16ShadowRetire` equal HotSpot 25.0.3, with the
/// JIT and with `--nojit`. Written from that dial run, not from a trial
/// binary: the Linux arms in the census page's round-16 wave-15 section are
/// the verification.
///
/// `--compatible` is unchanged: a `SyntheticStub` registers and dispatches
/// there as the `Bridge` did.
///
/// Control and kill switch (two rules, 20 rows each):
/// `CRATONVM_UNRETIRE_NATIVE_SHADOW=com/sun/crypto/provider/JceKeyStore,sun/security/provider/DomainKeyStore$DKS`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_R16_KEYSTORE_SPI_TRIPLES: &[(&str, &str, &str)] = &[
    ("com/sun/crypto/provider/JceKeyStore", "engineAliases", "()Ljava/util/Enumeration;"),
    ("com/sun/crypto/provider/JceKeyStore", "engineContainsAlias", "(Ljava/lang/String;)Z"),
    ("com/sun/crypto/provider/JceKeyStore", "engineDeleteEntry", "(Ljava/lang/String;)V"),
    (
        "com/sun/crypto/provider/JceKeyStore",
        "engineEntryInstanceOf",
        "(Ljava/lang/String;Ljava/lang/Class;)Z",
    ),
    (
        "com/sun/crypto/provider/JceKeyStore",
        "engineGetAttributes",
        "(Ljava/lang/String;)Ljava/util/Set;",
    ),
    (
        "com/sun/crypto/provider/JceKeyStore",
        "engineGetCertificate",
        "(Ljava/lang/String;)Ljava/security/cert/Certificate;",
    ),
    (
        "com/sun/crypto/provider/JceKeyStore",
        "engineGetCertificateAlias",
        "(Ljava/security/cert/Certificate;)Ljava/lang/String;",
    ),
    (
        "com/sun/crypto/provider/JceKeyStore",
        "engineGetCertificateChain",
        "(Ljava/lang/String;)[Ljava/security/cert/Certificate;",
    ),
    (
        "com/sun/crypto/provider/JceKeyStore",
        "engineGetCreationDate",
        "(Ljava/lang/String;)Ljava/util/Date;",
    ),
    (
        "com/sun/crypto/provider/JceKeyStore",
        "engineGetEntry",
        "(Ljava/lang/String;Ljava/security/KeyStore$ProtectionParameter;)Ljava/security/KeyStore$Entry;",
    ),
    (
        "com/sun/crypto/provider/JceKeyStore",
        "engineGetKey",
        "(Ljava/lang/String;[C)Ljava/security/Key;",
    ),
    ("com/sun/crypto/provider/JceKeyStore", "engineIsCertificateEntry", "(Ljava/lang/String;)Z"),
    ("com/sun/crypto/provider/JceKeyStore", "engineIsKeyEntry", "(Ljava/lang/String;)Z"),
    ("com/sun/crypto/provider/JceKeyStore", "engineLoad", "(Ljava/io/InputStream;[C)V"),
    (
        "com/sun/crypto/provider/JceKeyStore",
        "engineSetCertificateEntry",
        "(Ljava/lang/String;Ljava/security/cert/Certificate;)V",
    ),
    (
        "com/sun/crypto/provider/JceKeyStore",
        "engineSetEntry",
        "(Ljava/lang/String;Ljava/security/KeyStore$Entry;Ljava/security/KeyStore$ProtectionParameter;)V",
    ),
    (
        "com/sun/crypto/provider/JceKeyStore",
        "engineSetKeyEntry",
        "(Ljava/lang/String;Ljava/security/Key;[C[Ljava/security/cert/Certificate;)V",
    ),
    (
        "com/sun/crypto/provider/JceKeyStore",
        "engineSetKeyEntry",
        "(Ljava/lang/String;[B[Ljava/security/cert/Certificate;)V",
    ),
    ("com/sun/crypto/provider/JceKeyStore", "engineSize", "()I"),
    ("com/sun/crypto/provider/JceKeyStore", "engineStore", "(Ljava/io/OutputStream;[C)V"),
    ("sun/security/provider/DomainKeyStore$DKS", "engineAliases", "()Ljava/util/Enumeration;"),
    ("sun/security/provider/DomainKeyStore$DKS", "engineContainsAlias", "(Ljava/lang/String;)Z"),
    ("sun/security/provider/DomainKeyStore$DKS", "engineDeleteEntry", "(Ljava/lang/String;)V"),
    (
        "sun/security/provider/DomainKeyStore$DKS",
        "engineEntryInstanceOf",
        "(Ljava/lang/String;Ljava/lang/Class;)Z",
    ),
    (
        "sun/security/provider/DomainKeyStore$DKS",
        "engineGetAttributes",
        "(Ljava/lang/String;)Ljava/util/Set;",
    ),
    (
        "sun/security/provider/DomainKeyStore$DKS",
        "engineGetCertificate",
        "(Ljava/lang/String;)Ljava/security/cert/Certificate;",
    ),
    (
        "sun/security/provider/DomainKeyStore$DKS",
        "engineGetCertificateAlias",
        "(Ljava/security/cert/Certificate;)Ljava/lang/String;",
    ),
    (
        "sun/security/provider/DomainKeyStore$DKS",
        "engineGetCertificateChain",
        "(Ljava/lang/String;)[Ljava/security/cert/Certificate;",
    ),
    (
        "sun/security/provider/DomainKeyStore$DKS",
        "engineGetCreationDate",
        "(Ljava/lang/String;)Ljava/util/Date;",
    ),
    (
        "sun/security/provider/DomainKeyStore$DKS",
        "engineGetEntry",
        "(Ljava/lang/String;Ljava/security/KeyStore$ProtectionParameter;)Ljava/security/KeyStore$Entry;",
    ),
    (
        "sun/security/provider/DomainKeyStore$DKS",
        "engineGetKey",
        "(Ljava/lang/String;[C)Ljava/security/Key;",
    ),
    (
        "sun/security/provider/DomainKeyStore$DKS",
        "engineIsCertificateEntry",
        "(Ljava/lang/String;)Z",
    ),
    ("sun/security/provider/DomainKeyStore$DKS", "engineIsKeyEntry", "(Ljava/lang/String;)Z"),
    ("sun/security/provider/DomainKeyStore$DKS", "engineLoad", "(Ljava/io/InputStream;[C)V"),
    (
        "sun/security/provider/DomainKeyStore$DKS",
        "engineSetCertificateEntry",
        "(Ljava/lang/String;Ljava/security/cert/Certificate;)V",
    ),
    (
        "sun/security/provider/DomainKeyStore$DKS",
        "engineSetEntry",
        "(Ljava/lang/String;Ljava/security/KeyStore$Entry;Ljava/security/KeyStore$ProtectionParameter;)V",
    ),
    (
        "sun/security/provider/DomainKeyStore$DKS",
        "engineSetKeyEntry",
        "(Ljava/lang/String;Ljava/security/Key;[C[Ljava/security/cert/Certificate;)V",
    ),
    (
        "sun/security/provider/DomainKeyStore$DKS",
        "engineSetKeyEntry",
        "(Ljava/lang/String;[B[Ljava/security/cert/Certificate;)V",
    ),
    ("sun/security/provider/DomainKeyStore$DKS", "engineSize", "()I"),
    ("sun/security/provider/DomainKeyStore$DKS", "engineStore", "(Ljava/io/OutputStream;[C)V"),
];

/// Round 16 wave 15 (lane shadow), 2026-10-02 -- `javax.crypto.KEM` and its
/// two nested handles, 17 rows: the three `getInstance` overloads,
/// `getAlgorithm`, the five `newEncapsulator` / `newDecapsulator` overloads,
/// and `encapsulate` x2 / `decapsulate` x2 / `secretSize` /
/// `encapsulationSize` on `KEM$Encapsulator` and `KEM$Decapsulator`.
///
/// # The screen, read on this tree
///
/// * Pure-Java bytecode to yield to: every row is declared with `Code` on
///   JDK 25 (21 introduced the class; 17 has no `KEM`, so the rows have no
///   target there and nothing can call them).
/// * What the natives were: `jca/kem.rs` mints SYNTHETIC `KEM` /
///   `Encapsulator` / `Decapsulator` carriers (`try_alloc_concurrent_synthetic`,
///   private slots past the declared fields) and drives the REAL SunJCE SPI
///   (`ML_KEM_Impls$K*`, `DHKEM`) through them. The real fields of the
///   carrier are never written, so every member the registrar does not cover
///   reads null: `Encapsulator.providerName()` throws NPE (`this.p`), and
///   `getInstance(alg, "NoSuchProvider")` hands back a carrier instead of
///   `NoSuchProviderException`. The mint sites are inside the retired
///   natives, so retiring the whole surface retires the carriers with it
///   (the `kem.rs` row of `unconstructed_carrier_gate.rs`, if listed, stops
///   being reachable under `--real-jdk`).
/// * Speed: the native and the real path drive the SAME SPI object; the
///   retirement adds only the JDK's `GetInstance` lookup and the delegating
///   wrapper bodies, so no `Intrinsic` case exists.
/// * VM invariant: none. Nothing in `vm/` names the class; JSSE (real since
///   SBR wave 5) reaches `KEM` only through its public API.
/// * Registration: `jca::kem::register` (from `register_jca_natives`, every
///   arm), one explicit `Bridge` scope, each triple once, gated only on
///   `route_pqc_to_real()` (default on) and not `real_jca_mode()`.
/// * HotSpot difference today (`--real-jdk`), `R16ShadowRetire`, dev binary
///   with the natives: `K05` (`getInstance(alg, "NoSuchProvider")` returns),
///   `K06` (`getInstance(null)`: `NoSuchAlgorithmException` for HotSpot's
///   `NullPointerException: null algorithm name`), `K08`/`K11`
///   (`providerName()` NPE), `K16` (null encapsulation message). Retired
///   under the dial, all 23 `K*` lines equal HotSpot 25.0.3 (`DHKEM` over
///   X25519 and P-256, `ML-KEM-512`), JIT and `--nojit`.
///
/// `--compatible` is unchanged: a `SyntheticStub` registers and dispatches
/// there as the `Bridge` did.
///
/// Control and kill switch (three class rules, 9 + 4 + 4 rows):
/// `CRATONVM_UNRETIRE_NATIVE_SHADOW=javax/crypto/KEM,javax/crypto/KEM$Encapsulator,javax/crypto/KEM$Decapsulator`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_R16_KEM_TRIPLES: &[(&str, &str, &str)] = &[
    ("javax/crypto/KEM", "getAlgorithm", "()Ljava/lang/String;"),
    ("javax/crypto/KEM", "getInstance", "(Ljava/lang/String;)Ljavax/crypto/KEM;"),
    ("javax/crypto/KEM", "getInstance", "(Ljava/lang/String;Ljava/lang/String;)Ljavax/crypto/KEM;"),
    (
        "javax/crypto/KEM",
        "getInstance",
        "(Ljava/lang/String;Ljava/security/Provider;)Ljavax/crypto/KEM;",
    ),
    (
        "javax/crypto/KEM",
        "newDecapsulator",
        "(Ljava/security/PrivateKey;)Ljavax/crypto/KEM$Decapsulator;",
    ),
    (
        "javax/crypto/KEM",
        "newDecapsulator",
        "(Ljava/security/PrivateKey;Ljava/security/spec/AlgorithmParameterSpec;)Ljavax/crypto/KEM$Decapsulator;",
    ),
    (
        "javax/crypto/KEM",
        "newEncapsulator",
        "(Ljava/security/PublicKey;)Ljavax/crypto/KEM$Encapsulator;",
    ),
    (
        "javax/crypto/KEM",
        "newEncapsulator",
        "(Ljava/security/PublicKey;Ljava/security/SecureRandom;)Ljavax/crypto/KEM$Encapsulator;",
    ),
    (
        "javax/crypto/KEM",
        "newEncapsulator",
        "(Ljava/security/PublicKey;Ljava/security/spec/AlgorithmParameterSpec;Ljava/security/SecureRandom;)Ljavax/crypto/KEM$Encapsulator;",
    ),
    ("javax/crypto/KEM$Decapsulator", "decapsulate", "([B)Ljavax/crypto/SecretKey;"),
    (
        "javax/crypto/KEM$Decapsulator",
        "decapsulate",
        "([BIILjava/lang/String;)Ljavax/crypto/SecretKey;",
    ),
    ("javax/crypto/KEM$Decapsulator", "encapsulationSize", "()I"),
    ("javax/crypto/KEM$Decapsulator", "secretSize", "()I"),
    ("javax/crypto/KEM$Encapsulator", "encapsulate", "()Ljavax/crypto/KEM$Encapsulated;"),
    (
        "javax/crypto/KEM$Encapsulator",
        "encapsulate",
        "(IILjava/lang/String;)Ljavax/crypto/KEM$Encapsulated;",
    ),
    ("javax/crypto/KEM$Encapsulator", "encapsulationSize", "()I"),
    ("javax/crypto/KEM$Encapsulator", "secretSize", "()I"),
];

