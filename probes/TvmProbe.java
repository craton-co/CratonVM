import java.io.ByteArrayInputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.Reader;
import java.io.StringReader;
import java.math.BigDecimal;
import java.sql.SQLException;
import java.util.ArrayList;
import java.util.IdentityHashMap;
import java.util.Random;

import org.h2.api.IntervalQualifier;
import org.h2.engine.Constants;
import org.h2.store.DataHandler;
import org.h2.store.FileStore;
import org.h2.store.LobStorageInterface;
import org.h2.util.DateTimeUtils;
import org.h2.util.SmallLRUCache;
import org.h2.util.TempFileDeleter;
import org.h2.util.Utils;
import org.h2.value.CompareMode;
import org.h2.value.Value;
import org.h2.value.ValueArray;
import org.h2.value.ValueBigint;
import org.h2.value.ValueBinary;
import org.h2.value.ValueBlob;
import org.h2.value.ValueBoolean;
import org.h2.value.ValueChar;
import org.h2.value.ValueClob;
import org.h2.value.ValueDate;
import org.h2.value.ValueDecfloat;
import org.h2.value.ValueDouble;
import org.h2.value.ValueGeometry;
import org.h2.value.ValueInteger;
import org.h2.value.ValueInterval;
import org.h2.value.ValueJavaObject;
import org.h2.value.ValueJson;
import org.h2.value.ValueLob;
import org.h2.value.ValueNull;
import org.h2.value.ValueNumeric;
import org.h2.value.ValueReal;
import org.h2.value.ValueRow;
import org.h2.value.ValueSmallint;
import org.h2.value.ValueTime;
import org.h2.value.ValueTimeTimeZone;
import org.h2.value.ValueTimestamp;
import org.h2.value.ValueTimestampTimeZone;
import org.h2.value.ValueTinyint;
import org.h2.value.ValueUuid;
import org.h2.value.ValueVarbinary;
import org.h2.value.ValueVarchar;
import org.h2.value.ValueVarcharIgnoreCase;

/**
 * Standalone port of {@code org.h2.test.unit.TestValueMemory}, whose
 * {@code testType} loop is the workload of
 * {@code docs/known-issues/h2/testvaluememory-fails-under-g1-on-conservative-jit-roots-20260908.md}.
 *
 * <h2>Why a port and not the test</h2>
 *
 * {@code TestValueMemory} extends {@code org.h2.test.TestBase}, which lives in
 * H2's {@code src/test} tree and is not published to Maven Central -- the
 * {@code h2-<v>.jar} carries {@code org/h2/**} and no {@code org/h2/test/**} at
 * all. Everything this page's measurement needs from {@code TestBase} is
 * {@code assertEquals}, {@code fail} and a {@code config.traceTest} flag, so
 * they are inlined here and the rest of the class -- {@code testType},
 * {@code create} and the {@code DataHandler} / {@code LobStorageInterface}
 * implementations the LOB types need -- is a transcription.
 *
 * <h2>What it measures</h2>
 *
 * Per H2 value type: build values into an {@code ArrayList} until their
 * self-reported memory passes 1 MB, identity-map them, drop the list and the
 * map, collect twice, and read {@code totalMemory() - freeMemory()}. The
 * assertion is H2's: {@code used > memory * 3} fails the type. Note that
 * {@code array} -- a 125 000-slot {@code Object[]} on Type 0 -- is NOT dropped
 * and is read after the measurement, so ~977 KB of it is a floor no collector
 * can go below at 8-byte references.
 *
 * <pre>
 * H2J=~/.m2/repository/com/h2database/h2/2.4.240/h2-2.4.240.jar
 * javac -cp "$H2J" -d /tmp/p probes/TvmProbe.java
 * cratonvm --java-home "$JDK25" -XX:+UseG1GC -Xmx2g -cp "/tmp/p:$H2J" TvmProbe
 * </pre>
 *
 * {@code argv[0]}, when present, restricts the run to one type number, which is
 * how a single row is A/B'd without paying for the other forty.
 */
public class TvmProbe implements DataHandler {

    private static final long MIN_ABSOLUTE_DAY =
            DateTimeUtils.absoluteDayFromDateValue(DateTimeUtils.MIN_DATE_VALUE);

    private static final long MAX_ABSOLUTE_DAY =
            DateTimeUtils.absoluteDayFromDateValue(DateTimeUtils.MAX_DATE_VALUE);

    private final Random random = new Random(1);
    private final SmallLRUCache<String, String[]> lobFileListCache =
            SmallLRUCache.newInstance(128);
    private LobStorageInterface lobStorage;

    /** Every row is printed, not only the failing one, as `traceTest` does. */
    private static final boolean TRACE = true;

    private int failures;

    public static void main(String... a) throws Exception {
        TvmProbe t = new TvmProbe();
        int only = a.length > 0 ? Integer.parseInt(a[0]) : -1;
        for (int i = 0; i < Value.TYPE_COUNT; i++) {
            if (i == 23) {
                // "TIMESTAMP UTC" in the original: a short-lived experiment.
                continue;
            }
            if (i == Value.ENUM) {
                continue;
            }
            if (only >= 0 && i != only) {
                continue;
            }
            Value v = t.create(i);
            if (v == ValueNull.INSTANCE && i == Value.GEOMETRY) {
                // jts not on the classpath, OK
                continue;
            }
            if (v.getValueType() != i) {
                System.out.println("FAIL type mismatch: " + v.getValueType() + " != " + i);
                t.failures++;
                continue;
            }
            t.testType(i);
        }
        System.out.println(t.failures == 0 ? "DONE ok" : "DONE failures=" + t.failures);
        if (t.failures != 0) {
            System.exit(1);
        }
    }

    private void testType(int type) throws SQLException {
        System.gc();
        System.gc();
        long first = Utils.getMemoryUsed();
        ArrayList<Value> list = new ArrayList<>();
        long memory = 0;
        while (memory < 1000000) {
            Value v = create(type);
            memory += v.getMemory() + Constants.MEMORY_POINTER;
            list.add(v);
        }
        Object[] array = list.toArray();
        IdentityHashMap<Object, Object> map = new IdentityHashMap<>();
        for (Object a : array) {
            map.put(a, a);
        }
        int size = map.size();
        map.clear();
        map = null;
        list = null;
        System.gc();
        System.gc();
        long used = Utils.getMemoryUsed() - first;
        memory /= 1024;
        boolean bad = used > memory * 3;
        String msg = "Type: " + type + " Used memory: " + used + " calculated: " + memory
                + " length: " + array.length + " size: " + size;
        if (TRACE) {
            System.out.println((bad ? "FAIL " : "ok   ") + msg);
        }
        if (bad) {
            failures++;
        }
    }

    private Value create(int type) throws SQLException {
        switch (type) {
        case Value.NULL:
            return ValueNull.INSTANCE;
        case Value.BOOLEAN:
            return ValueBoolean.FALSE;
        case Value.TINYINT:
            return ValueTinyint.get((byte) random.nextInt());
        case Value.SMALLINT:
            return ValueSmallint.get((short) random.nextInt());
        case Value.INTEGER:
            return ValueInteger.get(random.nextInt());
        case Value.BIGINT:
            return ValueBigint.get(random.nextLong());
        case Value.NUMERIC:
            return ValueNumeric.get(new BigDecimal(random.nextInt()));
        case Value.DOUBLE:
            return ValueDouble.get(random.nextDouble());
        case Value.REAL:
            return ValueReal.get(random.nextFloat());
        case Value.DECFLOAT:
            return ValueDecfloat.get(new BigDecimal(random.nextInt()));
        case Value.TIME:
            return ValueTime.fromNanos(randomTimeNanos());
        case Value.TIME_TZ:
            return ValueTimeTimeZone.fromNanos(randomTimeNanos(), randomZoneOffset());
        case Value.DATE:
            return ValueDate.fromDateValue(randomDateValue());
        case Value.TIMESTAMP:
            return ValueTimestamp.fromDateValueAndNanos(randomDateValue(), randomTimeNanos());
        case Value.TIMESTAMP_TZ:
            return ValueTimestampTimeZone.fromDateValueAndNanos(
                    randomDateValue(), randomTimeNanos(), randomZoneOffset());
        case Value.VARBINARY:
            return ValueVarbinary.get(randomBytes(random.nextInt(1000)));
        case Value.VARCHAR:
            return ValueVarchar.get(randomString(random.nextInt(100)));
        case Value.VARCHAR_IGNORECASE:
            return ValueVarcharIgnoreCase.get(randomString(random.nextInt(100)));
        case Value.BLOB: {
            int len = (int) Math.abs(random.nextGaussian() * 10);
            byte[] data = randomBytes(len);
            return getLobStorage().createBlob(new ByteArrayInputStream(data), len);
        }
        case Value.CLOB: {
            int len = (int) Math.abs(random.nextGaussian() * 10);
            String s = randomString(len);
            return getLobStorage().createClob(new StringReader(s), len);
        }
        case Value.ARRAY:
            return ValueArray.get(createArray(), null);
        case Value.ROW:
            return ValueRow.get(createArray());
        case Value.JAVA_OBJECT:
            return ValueJavaObject.getNoCopy(randomBytes(random.nextInt(100)));
        case Value.UUID:
            return ValueUuid.get(random.nextLong(), random.nextLong());
        case Value.CHAR:
            return ValueChar.get(randomString(random.nextInt(100)));
        case Value.GEOMETRY:
            return ValueGeometry.get(
                    "POINT (" + random.nextInt(100) + ' ' + random.nextInt(100) + ')');
        case Value.INTERVAL_YEAR:
        case Value.INTERVAL_MONTH:
        case Value.INTERVAL_DAY:
        case Value.INTERVAL_HOUR:
        case Value.INTERVAL_MINUTE:
            return ValueInterval.from(IntervalQualifier.valueOf(type - Value.INTERVAL_YEAR),
                    random.nextBoolean(), random.nextInt(Integer.MAX_VALUE), 0);
        case Value.INTERVAL_SECOND:
        case Value.INTERVAL_DAY_TO_SECOND:
        case Value.INTERVAL_HOUR_TO_SECOND:
        case Value.INTERVAL_MINUTE_TO_SECOND:
            return ValueInterval.from(IntervalQualifier.valueOf(type - Value.INTERVAL_YEAR),
                    random.nextBoolean(), random.nextInt(Integer.MAX_VALUE),
                    random.nextInt(1_000_000_000));
        case Value.INTERVAL_YEAR_TO_MONTH:
        case Value.INTERVAL_DAY_TO_HOUR:
        case Value.INTERVAL_DAY_TO_MINUTE:
        case Value.INTERVAL_HOUR_TO_MINUTE:
            return ValueInterval.from(IntervalQualifier.valueOf(type - Value.INTERVAL_YEAR),
                    random.nextBoolean(), random.nextInt(Integer.MAX_VALUE), random.nextInt(12));
        case Value.JSON:
            return ValueJson.fromJson("{\"key\":\"value\"}");
        case Value.BINARY:
            return ValueBinary.get(randomBytes(random.nextInt(1000)));
        default:
            throw new AssertionError("type=" + type);
        }
    }

    private long randomDateValue() {
        return DateTimeUtils.dateValueFromAbsoluteDay(
                (random.nextLong() & Long.MAX_VALUE)
                        % (MAX_ABSOLUTE_DAY - MIN_ABSOLUTE_DAY + 1) + MIN_ABSOLUTE_DAY);
    }

    private long randomTimeNanos() {
        return (random.nextLong() & Long.MAX_VALUE) % DateTimeUtils.NANOS_PER_DAY;
    }

    private short randomZoneOffset() {
        return (short) (random.nextInt() % (18 * 60));
    }

    private Value[] createArray() throws SQLException {
        int len = random.nextInt(20);
        Value[] list = new Value[len];
        for (int i = 0; i < list.length; i++) {
            list[i] = create(Value.VARCHAR);
        }
        return list;
    }

    private byte[] randomBytes(int len) {
        byte[] data = new byte[len];
        if (random.nextBoolean()) {
            // don't initialize always (compression)
            random.nextBytes(data);
        }
        return data;
    }

    private String randomString(int len) {
        char[] chars = new char[len];
        if (random.nextBoolean()) {
            // don't initialize always (compression)
            for (int i = 0; i < chars.length; i++) {
                chars[i] = (char) (random.nextGaussian() * 100);
            }
        }
        return new String(chars);
    }

    // ---- DataHandler, as the original implements it ----

    @Override
    public void checkPowerOff() {
        // nothing to do
    }

    @Override
    public void checkWritingAllowed() {
        // nothing to do
    }

    @Override
    public String getDatabasePath() {
        return "./data/valueMemory";
    }

    @Override
    public Object getLobSyncObject() {
        return this;
    }

    @Override
    public int getMaxLengthInplaceLob() {
        return 100;
    }

    @Override
    public FileStore openFile(String name, String mode, boolean mustExist) {
        return FileStore.open(this, name, mode);
    }

    @Override
    public SmallLRUCache<String, String[]> getLobFileListCache() {
        return lobFileListCache;
    }

    @Override
    public TempFileDeleter getTempFileDeleter() {
        return TempFileDeleter.getInstance();
    }

    @Override
    public LobStorageInterface getLobStorage() {
        if (lobStorage == null) {
            lobStorage = new LobStorageTest();
        }
        return lobStorage;
    }

    @Override
    public int readLob(long lobId, byte[] hmac, long offset, byte[] buff, int off, int length) {
        return -1;
    }

    @Override
    public CompareMode getCompareMode() {
        return CompareMode.getInstance(null, 0);
    }

    private class LobStorageTest implements LobStorageInterface {

        LobStorageTest() {
        }

        @Override
        public void removeLob(ValueLob lob) {
            // not stored in the database
        }

        @Override
        public InputStream getInputStream(long lobId, long byteCount) throws IOException {
            throw new IllegalStateException();
        }

        @Override
        public InputStream getInputStream(long lobId, int tableId, long byteCount)
                throws IOException {
            throw new IllegalStateException();
        }

        @Override
        public boolean isReadOnly() {
            return false;
        }

        @Override
        public ValueLob copyLob(ValueLob old, int tableId) {
            throw new UnsupportedOperationException();
        }

        @Override
        public void removeAllForTable(int tableId) {
            throw new UnsupportedOperationException();
        }

        @Override
        public ValueBlob createBlob(InputStream in, long maxLength) {
            return ValueBlob.createTempBlob(in, maxLength, TvmProbe.this);
        }

        @Override
        public ValueClob createClob(Reader reader, long maxLength) {
            return ValueClob.createTempClob(reader, maxLength, TvmProbe.this);
        }
    }
}
