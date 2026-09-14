import java.io.*;
import java.lang.reflect.Modifier;
import java.net.*;
import java.nio.*;
import java.nio.channels.*;
import java.nio.channels.spi.SelectorProvider;
import java.nio.file.*;
import java.util.*;
import java.util.concurrent.*;

/**
 * H21-1 N3 — the universal assertion nobody makes.
 *
 * For every receiver this VM mints in the java.io / NIO families, print the
 * runtime class name and whether it is ABSTRACT or an INTERFACE. A `new`
 * opcode cannot legally produce either (JVMS 6.5), so any `ABSTRACT` or
 * `INTERFACE` line is a defect WITH NO ORACLE RUN REQUIRED.
 *
 * One line per case: `<tag> <class> <verdict>`. `verdict` is CONCRETE,
 * ABSTRACT, INTERFACE, NULL or ERR:<exception>.
 */
public class W4Abstract {
    static int abstractCount = 0;
    static int total = 0;

    static void show(String tag, Object o) {
        total++;
        if (o == null) {
            System.out.println(tag + " <null> NULL");
            return;
        }
        Class<?> c = o.getClass();
        int m = c.getModifiers();
        String verdict;
        if (c.isInterface()) { verdict = "INTERFACE"; abstractCount++; }
        else if (Modifier.isAbstract(m)) { verdict = "ABSTRACT"; abstractCount++; }
        else verdict = "CONCRETE";
        System.out.println(tag + " " + c.getName() + " " + verdict);
    }

    interface Case { Object run() throws Exception; }

    static void probe(String tag, Case c) {
        Object o;
        try {
            o = c.run();
        } catch (Throwable t) {
            total++;
            System.out.println(tag + " <err> ERR:" + t.getClass().getName() + ":" + String.valueOf(t.getMessage()));
            return;
        }
        show(tag, o);
    }

    public static void main(String[] args) throws Exception {
        final Path tmp = Files.createTempDirectory("w4abs");
        final Path f = tmp.resolve("f.txt");
        Files.write(f, "hello world".getBytes("UTF-8"));

        probe("Paths.get", () -> Paths.get("."));
        probe("Path.resolve", () -> Paths.get(".").resolve("x"));
        probe("FileSystems.getDefault", () -> FileSystems.getDefault());
        probe("FileSystem.provider", () -> FileSystems.getDefault().provider());

        probe("Pipe.open", () -> Pipe.open());
        probe("Pipe.source", () -> Pipe.open().source());
        probe("Pipe.sink", () -> Pipe.open().sink());

        probe("SocketChannel.open", () -> SocketChannel.open());
        probe("ServerSocketChannel.open", () -> ServerSocketChannel.open());
        probe("DatagramChannel.open", () -> DatagramChannel.open());
        probe("Selector.open", () -> Selector.open());
        probe("SelectorProvider.provider", () -> SelectorProvider.provider());

        probe("FileChannel.open", () -> FileChannel.open(f, StandardOpenOption.READ));
        probe("FileInputStream.getChannel", () -> new FileInputStream(f.toFile()).getChannel());
        probe("RandomAccessFile.getChannel", () -> new RandomAccessFile(f.toFile(), "rw").getChannel());
        probe("FileChannel.lock", () -> new RandomAccessFile(f.toFile(), "rw").getChannel().lock());
        probe("FileChannel.map", () -> new RandomAccessFile(f.toFile(), "rw").getChannel()
                .map(FileChannel.MapMode.READ_ONLY, 0, 4));

        probe("AsynchronousFileChannel.open", () -> AsynchronousFileChannel.open(f, StandardOpenOption.READ));
        probe("AsynchronousServerSocketChannel.open", () -> AsynchronousServerSocketChannel.open());
        probe("AsynchronousSocketChannel.open", () -> AsynchronousSocketChannel.open());
        probe("AsynchronousChannelGroup.withFixedThreadPool",
                () -> AsynchronousChannelGroup.withFixedThreadPool(1, Executors.defaultThreadFactory()));

        probe("newWatchService", () -> FileSystems.getDefault().newWatchService());
        probe("WatchKey", () -> {
            WatchService ws = FileSystems.getDefault().newWatchService();
            return tmp.register(ws, StandardWatchEventKinds.ENTRY_CREATE);
        });
        probe("WatchEvent.Kind", () -> StandardWatchEventKinds.ENTRY_CREATE);

        probe("ByteBuffer.allocate", () -> ByteBuffer.allocate(8));
        probe("ByteBuffer.allocateDirect", () -> ByteBuffer.allocateDirect(8));
        probe("ByteBuffer.wrap", () -> ByteBuffer.wrap(new byte[8]));
        probe("ByteBuffer.slice", () -> ByteBuffer.allocate(8).slice());
        probe("ByteBuffer.asCharBuffer", () -> ByteBuffer.allocate(8).asCharBuffer());
        probe("ByteBuffer.asIntBuffer", () -> ByteBuffer.allocate(8).asIntBuffer());
        probe("ByteBuffer.asLongBuffer", () -> ByteBuffer.allocate(8).asLongBuffer());
        probe("ByteBuffer.asShortBuffer", () -> ByteBuffer.allocate(8).asShortBuffer());
        probe("ByteBuffer.asFloatBuffer", () -> ByteBuffer.allocate(8).asFloatBuffer());
        probe("ByteBuffer.asDoubleBuffer", () -> ByteBuffer.allocate(8).asDoubleBuffer());
        probe("CharBuffer.allocate", () -> CharBuffer.allocate(8));
        probe("CharBuffer.wrap", () -> CharBuffer.wrap("abc"));
        probe("IntBuffer.allocate", () -> IntBuffer.allocate(8));
        probe("LongBuffer.allocate", () -> LongBuffer.allocate(8));
        probe("ShortBuffer.allocate", () -> ShortBuffer.allocate(8));
        probe("FloatBuffer.allocate", () -> FloatBuffer.allocate(8));
        probe("DoubleBuffer.allocate", () -> DoubleBuffer.allocate(8));

        probe("Channels.newInputStream", () -> Channels.newInputStream(FileChannel.open(f, StandardOpenOption.READ)));
        probe("Channels.newOutputStream", () -> Channels.newOutputStream(
                FileChannel.open(f, StandardOpenOption.WRITE)));
        probe("Channels.newChannel(in)", () -> Channels.newChannel(new ByteArrayInputStream(new byte[4])));
        probe("Channels.newChannel(out)", () -> Channels.newChannel(new ByteArrayOutputStream()));
        probe("Channels.newReader", () -> Channels.newReader(
                Channels.newChannel(new ByteArrayInputStream(new byte[4])), "UTF-8"));
        probe("Channels.newWriter", () -> Channels.newWriter(
                Channels.newChannel(new ByteArrayOutputStream()), "UTF-8"));

        probe("Files.lines", () -> Files.lines(f));
        probe("Files.newInputStream", () -> Files.newInputStream(f));
        probe("Files.newOutputStream", () -> Files.newOutputStream(tmp.resolve("o.txt")));
        probe("Files.newBufferedReader", () -> Files.newBufferedReader(f));
        probe("Files.newByteChannel", () -> Files.newByteChannel(f));
        probe("Files.newDirectoryStream", () -> Files.newDirectoryStream(tmp));
        probe("Files.walk", () -> Files.walk(tmp));
        probe("Files.list", () -> Files.list(tmp));
        probe("Files.readAttributes", () -> Files.readAttributes(f, java.nio.file.attribute.BasicFileAttributes.class));
        probe("Files.getFileStore", () -> Files.getFileStore(f));

        probe("ProcessHandle.current", () -> ProcessHandle.current());
        probe("ProcessBuilder.start", () -> new ProcessBuilder("true").start());

        probe("System.in", () -> System.in);
        probe("System.out", () -> System.out);
        probe("InputStreamReader", () -> new InputStreamReader(new ByteArrayInputStream(new byte[2])));
        probe("Scanner", () -> new Scanner(f));

        System.out.println("W4ABS-SUMMARY total=" + total + " abstractOrInterface=" + abstractCount);
        if (abstractCount == 0) System.out.println("PASS W4Abstract");
        else System.out.println("FAIL W4Abstract");
        System.out.flush();
        // Non-daemon threads (the AsynchronousChannelGroup pool, the watch
        // service poller) keep the JVM alive after main returns on BOTH VMs.
        Runtime.getRuntime().halt(0);
    }
}
