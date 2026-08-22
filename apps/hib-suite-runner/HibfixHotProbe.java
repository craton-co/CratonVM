import com.mysql.cj.otel.OpenTelemetryHandler;
import com.mysql.cj.telemetry.TelemetrySpan;
import com.mysql.cj.telemetry.TelemetrySpanName;
import com.mysql.cj.util.SearchMode;
import com.mysql.cj.util.StringInspector;
import java.util.ArrayList;
import java.util.EnumSet;
import java.util.List;
import java.util.Set;

/**
 * hibfix-20260822: per-operation A/B probe for the MySQL Connector/J methods
 * that `CRATONVM_DBG=jit-method-stats` reports as hot-but-never-compiled on
 * `org.hibernate.orm.test.batch.BatchTest`. Prints ns/op per row so a CratonVM
 * run can be divided by a HotSpot run of the same rows: a row near the whole
 * workload's ~4x wall ratio is the general gap, a row far above it is a
 * specific defect.
 */
public final class HibfixHotProbe {
	static final String INSERT_SQL =
			"insert into DataPoint (description,xval,yval,id) values (?,?,?,?)";

	static final long[] SINK = new long[1];

	static void bench(String name, int iters, Runnable body) {
		for (int i = 0; i < iters / 10 + 1; i++) body.run();     // warm
		long t0 = System.nanoTime();
		for (int i = 0; i < iters; i++) body.run();
		long ns = System.nanoTime() - t0;
		System.out.printf("@@ROW %-32s iters=%-8d ns_per_op=%d%n", name, iters, ns / iters);
	}

	public static void main(String[] args) throws Exception {
		int scale = Integer.getInteger("probe.scale", 1);

		bench("calibrate.arith", 20000 * scale, () -> {
			long s = 0; for (int i = 0; i < 1000; i++) s += i * 3L; SINK[0] += s;
		});

		final List<Object> empty = new ArrayList<>();
		bench("stream.map.forEach.empty", 100000 * scale,
				() -> empty.stream().map(Object::toString).forEach(SINK::equals));

		final Set<SearchMode> modes = EnumSet.of(SearchMode.SKIP_BETWEEN_MARKERS,
				SearchMode.SKIP_BLOCK_COMMENTS, SearchMode.SKIP_LINE_COMMENTS);
		bench("enumset.contains", 200000 * scale, () -> {
			if (modes.contains(SearchMode.SKIP_BLOCK_COMMENTS)) SINK[0]++;
			if (modes.contains(SearchMode.ALLOW_BACKSLASH_ESCAPE)) SINK[0]++;
		});

		bench("stringinspector.scan", 20000 * scale, () -> {
			StringInspector si = new StringInspector(INSERT_SQL, "", "", "", SearchMode.__FULL);
			int n = 0;
			while (si.indexOfNextChar() != -1) { n++; si.incrementPosition(); }
			SINK[0] += n;
		});

		if (OpenTelemetryHandler.isOpenTelemetryApiAvailable()) {
			final OpenTelemetryHandler h = new OpenTelemetryHandler();
			bench("otel.startSpan", 20000 * scale, () -> {
				TelemetrySpan s = h.startSpan(TelemetrySpanName.STMT_EXECUTE_PREPARED);
				s.end();
			});
		} else {
			System.out.println("@@ROW otel.startSpan SKIPPED (api not available)");
		}
		System.out.println("@@SINK " + SINK[0]);
	}
}
