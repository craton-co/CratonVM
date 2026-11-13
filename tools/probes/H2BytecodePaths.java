// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2026 Craton Software Company
//
// Compile with H2 on the classpath, then run on HotSpot and both CratonVM modes.
import java.sql.Connection;
import java.sql.DriverManager;
import java.sql.ResultSet;
import java.sql.Statement;
import org.h2.command.Command;
import org.h2.engine.SessionLocal;
import org.h2.jdbc.JdbcConnection;
import org.h2.mvstore.type.DataType;
import org.h2.mvstore.type.LongDataType;
import org.h2.result.DefaultRow;
import org.h2.value.Value;
import org.h2.value.ValueInteger;

public class H2BytecodePaths {
    public static void main(String[] args) throws Exception {
        checkCommandReuse();
        checkLongSearch();
        checkRowBounds();
        System.out.println("PASS H2BytecodePaths");
    }

    private static void checkCommandReuse() throws Exception {
        try (Connection connection = DriverManager.getConnection("jdbc:h2:mem:reuse", "sa", "");
             Statement statement = connection.createStatement()) {
            statement.execute("CREATE TABLE example(id INT PRIMARY KEY)");
            SessionLocal session = (SessionLocal) connection.unwrap(JdbcConnection.class).getSession();
            String sql = "INSERT INTO example(id) VALUES (?)";
            Command first = session.prepareLocal(sql);
            insert(first, 1);

            Command reused = session.prepareLocal(sql);
            if (reused != first || reused.canReuse() || reused.getParameters().get(0).isValueSet()) {
                throw new AssertionError("closed INSERT must be reused with cleared parameters");
            }
            insert(reused, 2);

            statement.execute("ALTER TABLE example ADD COLUMN name VARCHAR DEFAULT ''");
            Command afterDdl = session.prepareLocal(sql);
            if (afterDdl == reused) {
                throw new AssertionError("DDL must invalidate the cached INSERT");
            }
            insert(afterDdl, 3);
            try (ResultSet rows = statement.executeQuery("SELECT SUM(id) FROM example")) {
                if (!rows.next() || rows.getInt(1) != 6) {
                    throw new AssertionError("unexpected rows after INSERT and DDL");
                }
            }
        }
    }

    private static void insert(Command command, int id) {
        command.getParameters().get(0).setValue(ValueInteger.get(id), true);
        if (command.executeUpdate(null).getUpdateCount() != 1) {
            throw new AssertionError("expected one inserted row");
        }
        command.close();
    }

    private static void checkLongSearch() {
        DataType<Long> search = LongDataType.INSTANCE;
        Long[] keys = {Long.MIN_VALUE, -5L, 0L, 5L, Long.MAX_VALUE};
        for (int guess : new int[] {0, 1, 3, 5, 9}) {
            for (int i = 0; i < keys.length; i++) {
                if (search.binarySearch(keys[i], keys, keys.length, guess) != i) {
                    throw new AssertionError("wrong index for key " + keys[i]);
                }
            }
            if (search.binarySearch(3L, keys, keys.length, guess) != ~3) {
                throw new AssertionError("wrong insertion point");
            }
        }
        try {
            search.binarySearch(5L, new Object[] {5L}, 1, 0);
            throw new AssertionError("search must require Long[] storage");
        } catch (ClassCastException expected) {
            // The erased DataType method still enforces the Long[] cast.
        }
    }

    private static void checkRowBounds() {
        DefaultRow row = new DefaultRow(new Value[] {ValueInteger.get(17)});
        row.setKey(123);
        if (row.getValue(0).getInt() != 17 || row.getValue(-1).getLong() != 123) {
            throw new AssertionError("wrong row data or row ID");
        }
        for (int index : new int[] {-2, 1}) {
            try {
                row.getValue(index);
                throw new AssertionError("accepted invalid column index " + index);
            } catch (ArrayIndexOutOfBoundsException expected) {
                // The backing array defines the bounds for column indexes.
            }
        }
    }
}
