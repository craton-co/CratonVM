import com.mysql.cj.util.SearchMode;
import java.util.EnumSet;
import java.util.Set;

/**
 * hibfix-20260822: isolates the individual JDK primitives that
 * `--dump-native-registry`'s invocation census names as the top natives on the
 * MySQL Connector/J statement path (`Enum.ordinal`, `Object.getClass`,
 * `Character.isWhitespace`), so a CratonVM/HotSpot ns-per-op ratio can be read
 * per primitive instead of per composite method.
 */
public final class HibfixNativeProbe {
	static final long[] SINK = new long[1];

	static void bench(String name, int iters, Runnable body) {
		for (int i = 0; i < iters / 10 + 1; i++) body.run();
		long t0 = System.nanoTime();
		for (int i = 0; i < iters; i++) body.run();
		long ns = System.nanoTime() - t0;
		System.out.printf("@@ROW %-28s iters=%-9d ns_per_op=%d%n", name, iters, ns / iters);
	}

	public static void main(String[] args) {
		int scale = Integer.getInteger("probe.scale", 1);
		final SearchMode m = SearchMode.SKIP_BLOCK_COMMENTS;
		final Object o = m;
		final Set<SearchMode> modes = EnumSet.of(SearchMode.SKIP_BETWEEN_MARKERS,
				SearchMode.SKIP_BLOCK_COMMENTS, SearchMode.SKIP_LINE_COMMENTS);
		final char[] chars = "insert into DataPoint values (?,?,?,?)".toCharArray();

		bench("enum.ordinal", 1000000 * scale, () -> SINK[0] += m.ordinal());
		bench("enum.name", 1000000 * scale, () -> SINK[0] += m.name().length());
		bench("object.getClass", 1000000 * scale, () -> SINK[0] += o.getClass().hashCode());
		bench("character.isWhitespace", 1000000 * scale,
				() -> { for (char c : chars) if (Character.isWhitespace(c)) SINK[0]++; });
		bench("enumset.contains.one", 1000000 * scale,
				() -> { if (modes.contains(SearchMode.SKIP_BLOCK_COMMENTS)) SINK[0]++; });
		System.out.println("@@SINK " + SINK[0]);
	}
}
