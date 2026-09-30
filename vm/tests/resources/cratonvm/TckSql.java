package cratonvm;

import java.sql.*;

/**
 * T4.4 conformance tests for java.sql constants and constructors.
 *
 * No database connection needed -- exercises Types constants,
 * SQLException construction/chaining, and interface constant values.
 * Every method returns 1 on pass, 0 on fail.
 */
public class TckSql {

    public static int types_integer() {
        return Types.INTEGER == 4 ? 1 : 0;
    }

    public static int types_varchar() {
        return Types.VARCHAR == 12 ? 1 : 0;
    }

    public static int types_bigint() {
        return Types.BIGINT == -5 ? 1 : 0;
    }

    public static int types_double() {
        return Types.DOUBLE == 8 ? 1 : 0;
    }

    public static int types_timestamp() {
        return Types.TIMESTAMP == 93 ? 1 : 0;
    }

    public static int sqlException_message() {
        SQLException e = new SQLException("test error");
        return "test error".equals(e.getMessage()) ? 1 : 0;
    }

    public static int sqlException_state() {
        SQLException e = new SQLException("msg", "42000");
        return "42000".equals(e.getSQLState()) ? 1 : 0;
    }

    public static int sqlException_code() {
        SQLException e = new SQLException("msg", "state", 100);
        return e.getErrorCode() == 100 ? 1 : 0;
    }

    public static int sqlException_chain() {
        SQLException e1 = new SQLException("first");
        SQLException e2 = new SQLException("second");
        e1.setNextException(e2);
        return e1.getNextException() == e2 ? 1 : 0;
    }

    public static int resultSet_types() {
        return ResultSet.TYPE_FORWARD_ONLY == 1003 ? 1 : 0;
    }

    public static int connection_isolation() {
        return Connection.TRANSACTION_READ_COMMITTED == 2 ? 1 : 0;
    }

    public static int statement_no_keys() {
        return Statement.NO_GENERATED_KEYS == 2 ? 1 : 0;
    }
}
