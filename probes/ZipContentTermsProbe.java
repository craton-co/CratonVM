import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.IOException;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.util.zip.CRC32;
import java.util.zip.Deflater;
import java.util.zip.Inflater;
import java.util.zip.ZipEntry;
import java.util.zip.ZipOutputStream;

/**
 * `ZipContentTests` runs ~10x HotSpot on CratonVM with a flat profile — no
 * single dominant frame. This prices each term the sampler named, so the ratio
 * against a HotSpot control says which one carries the gap rather than which
 * one merely appears most.
 *
 * The terms, and why each is here (share of leaf samples, `--nojit`, 200ms):
 *   ByteBuffer scalar reads   ~18%  getShort/getInt/nextGetIndex/byteOffset
 *   zip deflate/inflate       ~14%  Inflater.inflate, Deflater.deflate
 *   ZipOutputStream framing    ~7%  writeCEN/writeLOC/putNextEntry
 *   file I/O                    —   the fixture writes ~7GB
 *
 * Prints ns/op so the two VMs can be diffed line by line.
 */
public final class ZipContentTermsProbe {

	public static void main(String[] args) throws Exception {
		System.out.println("term,ops,millis,ns_per_op");
		byteBufferScalarReads();
		deflateThroughput();
		inflateThroughput();
		crc32Throughput();
		fileWriteThenRead();
		zipFraming();
	}

	/**
	 * Spring Boot's zip reader pulls 2- and 4-byte little-endian fields out of a
	 * `ByteBuffer` one at a time, once per central-directory record.
	 */
	private static void byteBufferScalarReads() {
		ByteBuffer buffer = ByteBuffer.allocate(64 * 1024).order(ByteOrder.LITTLE_ENDIAN);
		for (int i = 0; i < buffer.capacity(); i++) {
			buffer.put(i, (byte) i);
		}
		int ops = 20_000_000;
		// Warm up so an interpreted first pass does not dominate.
		scalarLoop(buffer, 200_000);
		long start = System.nanoTime();
		long sink = scalarLoop(buffer, ops);
		report("bytebuffer.getShort+getInt", ops, start);
		if (sink == Long.MIN_VALUE) {
			System.out.println("unreachable " + sink);
		}
	}

	private static long scalarLoop(ByteBuffer buffer, int ops) {
		long sink = 0;
		int limit = buffer.capacity() - 8;
		for (int i = 0; i < ops; i++) {
			int offset = (i * 7) & (limit - 1);
			sink += buffer.getShort(offset);
			sink += buffer.getInt(offset + 2);
		}
		return sink;
	}

	private static void deflateThroughput() throws IOException {
		byte[] source = compressibleBytes(32 * 1024 * 1024);
		byte[] out = new byte[64 * 1024];
		long start = System.nanoTime();
		Deflater deflater = new Deflater(Deflater.DEFAULT_COMPRESSION);
		deflater.setInput(source);
		deflater.finish();
		long produced = 0;
		while (!deflater.finished()) {
			produced += deflater.deflate(out);
		}
		deflater.end();
		report("deflate.32MB", source.length, start);
		if (produced < 0) {
			System.out.println("unreachable " + produced);
		}
	}

	private static void inflateThroughput() throws Exception {
		byte[] source = compressibleBytes(32 * 1024 * 1024);
		ByteArrayOutputStream compressed = new ByteArrayOutputStream();
		Deflater deflater = new Deflater(Deflater.DEFAULT_COMPRESSION);
		deflater.setInput(source);
		deflater.finish();
		byte[] chunk = new byte[64 * 1024];
		while (!deflater.finished()) {
			compressed.write(chunk, 0, deflater.deflate(chunk));
		}
		deflater.end();

		byte[] packed = compressed.toByteArray();
		long start = System.nanoTime();
		Inflater inflater = new Inflater();
		inflater.setInput(packed);
		long produced = 0;
		while (!inflater.finished() && produced < source.length) {
			int n = inflater.inflate(chunk);
			if (n == 0 && inflater.needsInput()) {
				break;
			}
			produced += n;
		}
		inflater.end();
		report("inflate.32MB", (int) produced, start);
	}

	private static void crc32Throughput() {
		byte[] source = compressibleBytes(32 * 1024 * 1024);
		long start = System.nanoTime();
		CRC32 crc = new CRC32();
		crc.update(source);
		report("crc32.32MB", source.length, start);
		if (crc.getValue() == -1) {
			System.out.println("unreachable");
		}
	}

	private static void fileWriteThenRead() throws IOException {
		File file = File.createTempFile("zip-terms-probe", ".bin");
		file.deleteOnExit();
		byte[] block = new byte[1024 * 1024];
		int blocks = 256;
		long start = System.nanoTime();
		try (FileOutputStream out = new FileOutputStream(file)) {
			for (int i = 0; i < blocks; i++) {
				block[0] = (byte) i;
				out.write(block);
			}
		}
		report("file.write.256MB", blocks * block.length, start);

		start = System.nanoTime();
		long read = 0;
		try (FileInputStream in = new FileInputStream(file)) {
			int n;
			while ((n = in.read(block)) > 0) {
				read += n;
			}
		}
		report("file.read.256MB", (int) read, start);
		if (!file.delete()) {
			file.deleteOnExit();
		}
	}

	private static void zipFraming() throws IOException {
		File file = File.createTempFile("zip-terms-probe", ".zip");
		file.deleteOnExit();
		byte[] payload = compressibleBytes(4 * 1024);
		int entries = 20_000;
		long start = System.nanoTime();
		try (ZipOutputStream zip = new ZipOutputStream(new FileOutputStream(file))) {
			for (int i = 0; i < entries; i++) {
				zip.putNextEntry(new ZipEntry("entry-" + i + ".bin"));
				zip.write(payload);
				zip.closeEntry();
			}
		}
		report("zip.20k-entries", entries, start);
		if (!file.delete()) {
			file.deleteOnExit();
		}
	}

	private static byte[] compressibleBytes(int size) {
		byte[] bytes = new byte[size];
		for (int i = 0; i < size; i++) {
			bytes[i] = (byte) (i % 251);
		}
		return bytes;
	}

	private static void report(String term, int ops, long startNanos) {
		long elapsed = System.nanoTime() - startNanos;
		System.out.printf("%s,%d,%d,%.2f%n", term, ops, elapsed / 1_000_000,
				(double) elapsed / Math.max(ops, 1));
	}

}
