import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.io.Writer;
import java.nio.ByteBuffer;
import java.nio.channels.FileChannel;
import java.nio.channels.SeekableByteChannel;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.LinkOption;
import java.nio.file.OpenOption;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.nio.file.StandardOpenOption;
import java.util.ArrayList;
import java.util.EnumSet;
import java.util.HashSet;
import java.util.List;
import java.util.Set;

/**
 * Probe for `LinkOption.NOFOLLOW_LINKS` passed as an `OpenOption` to the
 * java.nio.file open paths.
 *
 * Spec: when the final component of the path is itself a symbolic link,
 * opening it with NOFOLLOW_LINKS must fail with an IOException (the platform
 * provider passes O_NOFOLLOW, which returns ELOOP). It must NOT silently
 * follow the link and read/write the target.
 *
 * See docs/known-issues/springboot/nio-write-ignores-nofollow-links-symlink-20260804.md
 * (org.springframework.boot.system.ApplicationPidTests).
 *
 * Every positive case is paired with two negative controls so a blanket
 * "always throw" regression cannot pass this probe:
 *   - the same call WITHOUT NOFOLLOW_LINKS must succeed through the link,
 *   - the same call WITH NOFOLLOW_LINKS on a REGULAR file must succeed.
 */
public final class NoFollowLinksOpenProbe {

	private static int pass;
	private static final List<String> failures = new ArrayList<>();

	public static void main(String[] args) throws Exception {
		Path dir = Files.createTempDirectory("nofollow-probe");
		if (!symlinksWork(dir)) {
			System.out.println("SKIP: this platform cannot create symbolic links");
			return;
		}

		// ---- 1. Files.writeString: the ApplicationPid.write shape ----------
		check("writeString/symlink-to-existing-target", throwsIo(() -> {
			Path link = link(dir, "a", /* targetExists */ true);
			Files.writeString(link, "123", StandardOpenOption.TRUNCATE_EXISTING,
					StandardOpenOption.CREATE, LinkOption.NOFOLLOW_LINKS);
		}));
		// ...and the target must be untouched by the failed write.
		Path t1 = target(dir, "a");
		check("writeString/target-untouched", "target".equals(readOrNull(t1)));

		// Symlink whose target does NOT exist: O_NOFOLLOW|O_CREAT still ELOOPs,
		// and crucially must not create the target behind the link.
		check("writeString/symlink-to-missing-target", throwsIo(() -> {
			Path link = link(dir, "b", /* targetExists */ false);
			Files.writeString(link, "123", StandardOpenOption.TRUNCATE_EXISTING,
					StandardOpenOption.CREATE, LinkOption.NOFOLLOW_LINKS);
		}));
		check("writeString/missing-target-not-created", !Files.exists(target(dir, "b")));

		// ---- 2. Files.newOutputStream --------------------------------------
		check("newOutputStream/symlink", throwsIo(() -> {
			Path link = link(dir, "c", true);
			try (OutputStream out = Files.newOutputStream(link, StandardOpenOption.CREATE,
					StandardOpenOption.WRITE, LinkOption.NOFOLLOW_LINKS)) {
				out.write('x');
			}
		}));
		check("newOutputStream/target-untouched", "target".equals(readOrNull(target(dir, "c"))));

		// ---- 3. Files.newInputStream ---------------------------------------
		check("newInputStream/symlink", throwsIo(() -> {
			Path link = link(dir, "d", true);
			try (InputStream in = Files.newInputStream(link, LinkOption.NOFOLLOW_LINKS)) {
				in.read();
			}
		}));

		// ---- 4. Files.newByteChannel (Set overload) ------------------------
		check("newByteChannel/symlink", throwsIo(() -> {
			Path link = link(dir, "e", true);
			Set<OpenOption> opts = new HashSet<>(
					EnumSet.of(StandardOpenOption.WRITE, StandardOpenOption.CREATE));
			opts.add(LinkOption.NOFOLLOW_LINKS);
			try (SeekableByteChannel ch = Files.newByteChannel(link, opts)) {
				ch.write(ByteBuffer.wrap(new byte[] { 'x' }));
			}
		}));
		check("newByteChannel/target-untouched", "target".equals(readOrNull(target(dir, "e"))));

		// ---- 5. FileChannel.open -------------------------------------------
		check("FileChannel.open/symlink", throwsIo(() -> {
			Path link = link(dir, "f", true);
			try (FileChannel ch = FileChannel.open(link, StandardOpenOption.WRITE,
					StandardOpenOption.CREATE, LinkOption.NOFOLLOW_LINKS)) {
				ch.write(ByteBuffer.wrap(new byte[] { 'x' }));
			}
		}));
		check("FileChannel.open/target-untouched", "target".equals(readOrNull(target(dir, "f"))));

		// ---- 6. Files.newBufferedWriter / newBufferedReader ----------------
		check("newBufferedWriter/symlink", throwsIo(() -> {
			Path link = link(dir, "g", true);
			try (Writer w = Files.newBufferedWriter(link, StandardCharsets.UTF_8,
					StandardOpenOption.CREATE, StandardOpenOption.WRITE, LinkOption.NOFOLLOW_LINKS)) {
				w.write("x");
			}
		}));
		check("newBufferedWriter/target-untouched", "target".equals(readOrNull(target(dir, "g"))));

		// ---- NEGATIVE CONTROLS ---------------------------------------------
		// (a) same calls WITHOUT NOFOLLOW_LINKS must succeed *through* the link.
		Path follow = link(dir, "h", true);
		Files.writeString(follow, "written", StandardOpenOption.TRUNCATE_EXISTING,
				StandardOpenOption.CREATE);
		check("control/write-through-link-without-nofollow",
				"written".equals(readOrNull(target(dir, "h"))));
		check("control/link-is-still-a-link", Files.isSymbolicLink(follow));

		// (b) NOFOLLOW_LINKS on an ordinary (non-link) file must succeed.
		Path plain = dir.resolve("plain");
		Files.writeString(plain, "plain-1", StandardOpenOption.CREATE, LinkOption.NOFOLLOW_LINKS);
		check("control/nofollow-on-regular-file-writes", "plain-1".equals(readOrNull(plain)));
		try (InputStream in = Files.newInputStream(plain, LinkOption.NOFOLLOW_LINKS)) {
			check("control/nofollow-on-regular-file-reads", in.read() == 'p');
		}
		try (FileChannel ch = FileChannel.open(plain, StandardOpenOption.READ,
				LinkOption.NOFOLLOW_LINKS)) {
			check("control/nofollow-fc-on-regular-file", ch.size() == 7);
		}

		// (c) NOFOLLOW_LINKS on a path *under* a symlinked directory is fine:
		// only the FINAL component matters.
		Path realDir = Files.createDirectory(dir.resolve("realdir"));
		Path dirLink = dir.resolve("dirlink");
		Files.createSymbolicLink(dirLink, realDir);
		Files.writeString(dirLink.resolve("inner"), "inner", StandardOpenOption.CREATE,
				LinkOption.NOFOLLOW_LINKS);
		check("control/nofollow-only-checks-final-component",
				"inner".equals(readOrNull(realDir.resolve("inner"))));

		System.out.println("PASS=" + pass + " FAIL=" + failures.size());
		for (String f : failures) {
			System.out.println("  FAILED: " + f);
		}
		if (!failures.isEmpty()) {
			System.exit(1);
		}
	}

	// --- helpers ---------------------------------------------------------

	private interface Body {
		void run() throws Exception;
	}

	private static boolean throwsIo(Body body) {
		try {
			body.run();
			return false;
		}
		catch (IOException expected) {
			return true;
		}
		catch (Exception other) {
			failures.add("wrong exception type: " + other);
			return false;
		}
	}

	private static Path target(Path dir, String name) {
		return dir.resolve(name + ".target");
	}

	/** Create `<name>` as a symbolic link to `<name>.target`. */
	private static Path link(Path dir, String name, boolean targetExists) throws IOException {
		Path target = target(dir, name);
		if (targetExists && !Files.exists(target)) {
			Files.write(target, "target".getBytes(StandardCharsets.UTF_8),
					StandardOpenOption.CREATE_NEW);
		}
		Path link = dir.resolve(name);
		if (!Files.exists(link, LinkOption.NOFOLLOW_LINKS)) {
			Files.createSymbolicLink(link, target);
		}
		return link;
	}

	private static String readOrNull(Path p) {
		try {
			return Files.readString(p);
		}
		catch (IOException ex) {
			return null;
		}
	}

	private static boolean symlinksWork(Path dir) {
		try {
			Files.createSymbolicLink(dir.resolve(".probe-link"), dir.resolve(".probe-target"));
			return true;
		}
		catch (Exception ex) {
			return false;
		}
	}

	private static void check(String name, boolean ok) {
		if (ok) {
			pass++;
		}
		else {
			failures.add(name);
		}
	}

	private NoFollowLinksOpenProbe() {
	}
}
