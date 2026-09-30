import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Method;
import java.lang.reflect.Proxy;
import java.nio.file.Path;
import java.sql.Connection;
import java.sql.DatabaseMetaData;
import java.sql.PreparedStatement;
import java.sql.ResultSet;
import java.sql.ResultSetMetaData;
import java.sql.Statement;
import java.util.ArrayList;
import java.util.List;
import javax.net.ssl.SSLSession;

/**
 * Do the natives registered on JDK INTERFACES intercept a foreign implementor?
 *
 * WHY THIS EXISTS. CratonVM registers natives on abstract JDK types because
 * that is where a synthetic implementation's methods resolve. On JDK 25 that
 * leaves 1,435 live {@code Bridge} registrations whose image target is an
 * ABSTRACT method — 313 in {@code java.util} and 1,122 outside it. An abstract
 * target means the registration stands in front of EVERY implementor, including
 * ones the VM has never seen, and the previous answer for that population was
 * an argument rather than a measurement.
 *
 * Only one family had been probed: {@code java.util.Abstract*}, by
 * {@code RForeignLayoutCollections}, which found the natives ask the receiver
 * rather than index into an assumed layout. That result does not carry: the
 * identical shape one package over IS broken — {@code java.lang.Process}'s
 * natives answer for a user subclass out of the VM's fixed field layout — and
 * the difference is not dispatch but whether the native ASKS THE RECEIVER or
 * INDEXES INTO A LAYOUT IT ASSUMES.
 *
 * WHY A PROXY. A {@code java.lang.reflect.Proxy} is the most foreign implementor
 * there is: it has no fields at all, and every interface method must reach the
 * {@code InvocationHandler}. It is also the realistic one — JDBC pools,
 * Mockito, Spring AOP and the JDK's own tracing wrappers all hand out proxies
 * over exactly these interfaces. If a native intercepts, the handler is not
 * called and a value the VM fabricated comes back instead; both are visible
 * here, because every call asserts BOTH the returned value and that the handler
 * recorded the invocation.
 *
 * The families covered are the ones the census shows carrying the largest live
 * abstract-target populations AND that an application actually implements:
 * {@code java.sql} (Connection 19, ResultSet 21, Statement 12, PreparedStatement
 * 28, DatabaseMetaData 13, ResultSetMetaData 11), {@code java.nio.file.Path}
 * (21), {@code java.lang.ProcessHandle} (11) and {@code javax.net.ssl.SSLSession}
 * (16). The abstract CLASSES in that population — {@code java.nio.ByteBuffer},
 * {@code java.nio.channels.SocketChannel}, {@code java.net.http.HttpClient},
 * {@code java.lang.foreign.MemorySegment} — cannot be implemented from outside
 * their packages (package-private constructors, sealed types) and so cannot
 * receive a foreign implementor at all; that is a fact about the type, not an
 * untested gap.
 *
 * {@code foreignArenaLifetime()} is the one section that is NOT a dispatch
 * question. It is here because the sentence above was, until it was written, the
 * only mention of {@code java.lang.foreign} in any scheduled class — so the FFM
 * lifetime model had no vector anywhere in the suite. See its own javadoc.
 *
 * Determinism: no I/O, no clock, no identity hashes — the proxy's own
 * {@code hashCode} is routed through the handler and answers a constant.
 */
public class RForeignLayoutJdkInterfaces {
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError("RForeignLayoutJdkInterfaces: " + m);
        }
    }

    /** Sentinels chosen so no plausible fabricated answer collides with them. */
    static final boolean B = true;
    static final int I = 424242;
    static final long L = 424242424242L;
    static final double D = 42.5;
    static final float F = 4.25f;
    static final short S = 4242;
    static final byte BY = 42;
    static final char C = 'Z';

    /**
     * Records every call and answers a sentinel. Reference returns are
     * {@code null}, which is legal for every reference type and cannot be
     * mistaken for a fabricated object; {@code toString} is the one exception,
     * because a null there breaks printing rather than reporting.
     */
    static final class Recorder implements InvocationHandler {
        final List<String> seen = new ArrayList<>();
        final String tag;

        Recorder(String tag) {
            this.tag = tag;
        }

        @Override
        public Object invoke(Object proxy, Method m, Object[] args) {
            seen.add(m.getName());
            Class<?> r = m.getReturnType();
            if (r == boolean.class) return B;
            if (r == int.class) return I;
            if (r == long.class) return L;
            if (r == double.class) return D;
            if (r == float.class) return F;
            if (r == short.class) return S;
            if (r == byte.class) return BY;
            if (r == char.class) return C;
            if (r == void.class) return null;
            if (r == String.class) return "SENTINEL-" + tag + "-" + m.getName();
            return null;
        }

        boolean sawExactly(String... names) {
            if (seen.size() != names.length) {
                return false;
            }
            for (int i = 0; i < names.length; i++) {
                if (!seen.get(i).equals(names[i])) {
                    return false;
                }
            }
            return true;
        }

        void reset() {
            seen.clear();
        }
    }

    @SuppressWarnings("unchecked")
    static <T> T proxy(Class<T> iface, Recorder r) {
        return (T) Proxy.newProxyInstance(
                RForeignLayoutJdkInterfaces.class.getClassLoader(), new Class<?>[] { iface }, r);
    }

    /** java.sql.Connection — 19 live abstract-target registrations. */
    static void sqlConnection() throws Exception {
        Recorder r = new Recorder("conn");
        Connection c = proxy(Connection.class, r);

        check(c.getAutoCommit() == B, "Connection.getAutoCommit reached the handler");
        check(c.isClosed() == B, "Connection.isClosed reached the handler");
        check(c.isValid(3) == B, "Connection.isValid(int) reached the handler");
        check(c.getTransactionIsolation() == I, "Connection.getTransactionIsolation");
        check(c.createStatement() == null, "Connection.createStatement is the handler's null");
        check(c.getMetaData() == null, "Connection.getMetaData is the handler's null");
        check(c.prepareStatement("select 1") == null, "Connection.prepareStatement");
        check(c.prepareCall("call x") == null, "Connection.prepareCall");
        check(c.setSavepoint() == null, "Connection.setSavepoint");
        c.setAutoCommit(false);
        c.setTransactionIsolation(2);
        c.commit();
        c.rollback();
        c.close();
        check(r.sawExactly("getAutoCommit", "isClosed", "isValid", "getTransactionIsolation",
                "createStatement", "getMetaData", "prepareStatement", "prepareCall",
                "setSavepoint", "setAutoCommit", "setTransactionIsolation",
                "commit", "rollback", "close"),
                "Connection: the handler saw exactly the 14 calls, in order: " + r.seen);
        System.out.println("CK RForeignLayoutJdkInterfaces conn=" + r.seen.size());
    }

    /** java.sql.ResultSet / ResultSetMetaData — 21 + 11. */
    static void sqlResultSet() throws Exception {
        Recorder r = new Recorder("rs");
        ResultSet rs = proxy(ResultSet.class, r);

        check(rs.next() == B, "ResultSet.next");
        check(rs.wasNull() == B, "ResultSet.wasNull");
        check(rs.isBeforeFirst() == B, "ResultSet.isBeforeFirst");
        check(rs.isAfterLast() == B, "ResultSet.isAfterLast");
        check(rs.getRow() == I, "ResultSet.getRow");
        check(rs.getInt(1) == I, "ResultSet.getInt(int)");
        check(rs.getInt("c") == I, "ResultSet.getInt(String)");
        check(rs.getLong(1) == L, "ResultSet.getLong(int)");
        check(rs.getDouble(1) == D, "ResultSet.getDouble(int)");
        check(rs.getFloat(1) == F, "ResultSet.getFloat(int)");
        check(rs.getBoolean(1) == B, "ResultSet.getBoolean(int)");
        check(rs.getBytes(1) == null, "ResultSet.getBytes is the handler's null");
        check("SENTINEL-rs-getString".equals(rs.getString(1)),
                "ResultSet.getString(int) is the handler's sentinel");
        check(rs.getObject(1) == null, "ResultSet.getObject(int)");
        check(rs.getMetaData() == null, "ResultSet.getMetaData");
        rs.close();
        check(r.seen.size() == 16, "ResultSet: 16 calls reached the handler, saw " + r.seen.size());

        Recorder rm = new Recorder("rsmd");
        ResultSetMetaData md = proxy(ResultSetMetaData.class, rm);
        check(md.getColumnCount() == I, "ResultSetMetaData.getColumnCount");
        check(md.getColumnType(1) == I, "ResultSetMetaData.getColumnType");
        check(md.getPrecision(1) == I, "ResultSetMetaData.getPrecision");
        check(md.getColumnDisplaySize(1) == I, "ResultSetMetaData.getColumnDisplaySize");
        check("SENTINEL-rsmd-getColumnName".equals(md.getColumnName(1)),
                "ResultSetMetaData.getColumnName");
        check("SENTINEL-rsmd-getColumnTypeName".equals(md.getColumnTypeName(1)),
                "ResultSetMetaData.getColumnTypeName");
        check("SENTINEL-rsmd-getColumnClassName".equals(md.getColumnClassName(1)),
                "ResultSetMetaData.getColumnClassName");
        check(rm.seen.size() == 7, "ResultSetMetaData: 7 calls, saw " + rm.seen.size());
        System.out.println("CK RForeignLayoutJdkInterfaces rs=" + r.seen.size()
                + " rsmd=" + rm.seen.size());
    }

    /** java.sql.Statement / PreparedStatement / DatabaseMetaData. */
    static void sqlStatements() throws Exception {
        Recorder r = new Recorder("stmt");
        Statement st = proxy(Statement.class, r);
        check(st.execute("select 1") == B, "Statement.execute");
        check(st.executeUpdate("update t set a=1") == I, "Statement.executeUpdate");
        check(st.executeQuery("select 1") == null, "Statement.executeQuery");
        check(st.getResultSet() == null, "Statement.getResultSet");
        check(st.getUpdateCount() == I, "Statement.getUpdateCount");
        check(st.isClosed() == B, "Statement.isClosed");
        st.close();
        check(r.seen.size() == 7, "Statement: 7 calls, saw " + r.seen.size());

        Recorder p = new Recorder("ps");
        PreparedStatement ps = proxy(PreparedStatement.class, p);
        ps.setInt(1, 7);
        ps.setString(2, "x");
        ps.setLong(3, 9L);
        ps.setNull(4, java.sql.Types.INTEGER);
        check(ps.execute() == B, "PreparedStatement.execute");
        check(ps.executeQuery() == null, "PreparedStatement.executeQuery");
        check(ps.executeUpdate() == I, "PreparedStatement.executeUpdate");
        ps.clearParameters();
        check(p.seen.size() == 8, "PreparedStatement: 8 calls, saw " + p.seen.size());

        Recorder d = new Recorder("dbmd");
        DatabaseMetaData dm = proxy(DatabaseMetaData.class, d);
        check("SENTINEL-dbmd-getDatabaseProductName".equals(dm.getDatabaseProductName()),
                "DatabaseMetaData.getDatabaseProductName");
        check("SENTINEL-dbmd-getDriverName".equals(dm.getDriverName()),
                "DatabaseMetaData.getDriverName");
        check(dm.getDatabaseMajorVersion() == I, "DatabaseMetaData.getDatabaseMajorVersion");
        check(dm.supportsTransactions() == B, "DatabaseMetaData.supportsTransactions");
        check(dm.getConnection() == null, "DatabaseMetaData.getConnection");
        check(d.seen.size() == 5, "DatabaseMetaData: 5 calls, saw " + d.seen.size());
        System.out.println("CK RForeignLayoutJdkInterfaces stmt=" + r.seen.size()
                + " ps=" + p.seen.size() + " dbmd=" + d.seen.size());
    }

    /**
     * java.nio.file.Path — 21 live registrations, and the family that also
     * carries {@code equals}, {@code hashCode} and {@code toString}, so it
     * exercises the three methods a Proxy routes to the handler that most code
     * assumes it never sees.
     */
    static void nioPath() {
        Recorder r = new Recorder("path");
        Path p = proxy(Path.class, r);

        check(p.isAbsolute() == B, "Path.isAbsolute");
        check(p.getNameCount() == I, "Path.getNameCount");
        check(p.startsWith("a") == B, "Path.startsWith(String)");
        check(p.endsWith("b") == B, "Path.endsWith(String)");
        check(p.getFileName() == null, "Path.getFileName is the handler's null");
        check(p.getParent() == null, "Path.getParent");
        check(p.getRoot() == null, "Path.getRoot");
        check(p.normalize() == null, "Path.normalize");
        check(p.toAbsolutePath() == null, "Path.toAbsolutePath");
        check(p.getFileSystem() == null, "Path.getFileSystem");
        check(p.toFile() == null, "Path.toFile");
        check(p.toUri() == null, "Path.toUri");
        // Captured into locals first: a `p.toString()` inside an assertion
        // MESSAGE is evaluated eagerly and would be a 17th call through the
        // handler, which the count below would then report as interception
        // failing to happen twice.
        String ts = p.toString();
        int hc = p.hashCode();
        boolean eq = p.equals("anything");
        check("SENTINEL-path-toString".equals(ts), "Path.toString = " + ts);
        check(hc == I, "Path.hashCode = " + hc);
        check(eq == B, "Path.equals");
        int seen = r.seen.size();
        check(seen == 15, "Path: 15 calls, saw " + seen + " " + r.seen);
        System.out.println("CK RForeignLayoutJdkInterfaces path=" + r.seen.size());

        // A REAL Path must be unaffected: the interception question is about
        // foreign implementors, and a vector that broke the real one would be
        // reporting the wrong thing.
        Path real = Path.of("a", "b", "c.txt");
        check(real.getNameCount() == 3, "a real Path still answers 3");
        check("c.txt".equals(real.getFileName().toString()), "a real Path's file name");
        check(!real.isAbsolute(), "a real relative Path is not absolute");
        System.out.println("CK RForeignLayoutJdkInterfaces realPath=" + real.getNameCount());
    }

    /** java.lang.ProcessHandle (11) and javax.net.ssl.SSLSession (16). */
    static void processAndSession() {
        Recorder r = new Recorder("ph");
        ProcessHandle h = proxy(ProcessHandle.class, r);
        check(h.pid() == L, "ProcessHandle.pid");
        check(h.isAlive() == B, "ProcessHandle.isAlive");
        check(h.destroy() == B, "ProcessHandle.destroy");
        check(h.destroyForcibly() == B, "ProcessHandle.destroyForcibly");
        check(h.supportsNormalTermination() == B, "ProcessHandle.supportsNormalTermination");
        check(h.info() == null, "ProcessHandle.info is the handler's null");
        check(h.parent() == null, "ProcessHandle.parent");
        check(h.children() == null, "ProcessHandle.children");
        check(h.descendants() == null, "ProcessHandle.descendants");
        check(h.onExit() == null, "ProcessHandle.onExit");
        check(r.seen.size() == 10, "ProcessHandle: 10 calls, saw " + r.seen.size());

        Recorder s = new Recorder("sess");
        SSLSession sess = proxy(SSLSession.class, s);
        check(sess.getId() == null, "SSLSession.getId");
        check(sess.isValid() == B, "SSLSession.isValid");
        check(sess.getCreationTime() == L, "SSLSession.getCreationTime");
        check(sess.getLastAccessedTime() == L, "SSLSession.getLastAccessedTime");
        check(sess.getApplicationBufferSize() == I, "SSLSession.getApplicationBufferSize");
        check(sess.getPacketBufferSize() == I, "SSLSession.getPacketBufferSize");
        check("SENTINEL-sess-getCipherSuite".equals(sess.getCipherSuite()),
                "SSLSession.getCipherSuite");
        check("SENTINEL-sess-getProtocol".equals(sess.getProtocol()), "SSLSession.getProtocol");
        check("SENTINEL-sess-getPeerHost".equals(sess.getPeerHost()), "SSLSession.getPeerHost");
        check(sess.getPeerPort() == I, "SSLSession.getPeerPort");
        check(sess.getLocalCertificates() == null, "SSLSession.getLocalCertificates");
        check(sess.getSessionContext() == null, "SSLSession.getSessionContext");
        sess.invalidate();
        check(s.seen.size() == 13, "SSLSession: 13 calls, saw " + s.seen.size());
        System.out.println("CK RForeignLayoutJdkInterfaces ph=" + r.seen.size()
                + " sess=" + s.seen.size());

        // A REAL ProcessHandle is unaffected.
        check(ProcessHandle.current().pid() > 0, "the real current process has a pid");
        check(ProcessHandle.current().isAlive(), "…and is alive");
        System.out.println("CK RForeignLayoutJdkInterfaces realProcess=ok");
    }

    /**
     * java.lang.management (ThreadMXBean 26, RuntimeMXBean 17) and
     * javax.management.MBeanServer (15). The MXBean interfaces are the family
     * an agent or a JMX connector most often stands a proxy in front of -- the
     * JDK's own {@code ManagementFactory.newPlatformMXBeanProxy} does exactly
     * this -- so a native intercepting them would answer THIS VM's numbers for
     * a bean that is not this VM.
     */
    static void managementBeans() throws Exception {
        Recorder t = new Recorder("thr");
        java.lang.management.ThreadMXBean tb =
                proxy(java.lang.management.ThreadMXBean.class, t);
        check(tb.getThreadCount() == I, "ThreadMXBean.getThreadCount");
        check(tb.getPeakThreadCount() == I, "ThreadMXBean.getPeakThreadCount");
        check(tb.getDaemonThreadCount() == I, "ThreadMXBean.getDaemonThreadCount");
        check(tb.getTotalStartedThreadCount() == L, "ThreadMXBean.getTotalStartedThreadCount");
        check(tb.getCurrentThreadCpuTime() == L, "ThreadMXBean.getCurrentThreadCpuTime");
        check(tb.isThreadCpuTimeSupported() == B, "ThreadMXBean.isThreadCpuTimeSupported");
        check(tb.getAllThreadIds() == null, "ThreadMXBean.getAllThreadIds");
        check(tb.findDeadlockedThreads() == null, "ThreadMXBean.findDeadlockedThreads");
        check(t.seen.size() == 8, "ThreadMXBean: 8 calls, saw " + t.seen.size());

        Recorder rt = new Recorder("rt");
        java.lang.management.RuntimeMXBean rb =
                proxy(java.lang.management.RuntimeMXBean.class, rt);
        check(rb.getUptime() == L, "RuntimeMXBean.getUptime");
        check(rb.getStartTime() == L, "RuntimeMXBean.getStartTime");
        check("SENTINEL-rt-getName".equals(rb.getName()), "RuntimeMXBean.getName");
        check("SENTINEL-rt-getVmName".equals(rb.getVmName()), "RuntimeMXBean.getVmName");
        check("SENTINEL-rt-getVmVersion".equals(rb.getVmVersion()), "RuntimeMXBean.getVmVersion");
        check(rb.getInputArguments() == null, "RuntimeMXBean.getInputArguments");
        check(rb.getSystemProperties() == null, "RuntimeMXBean.getSystemProperties");
        check(rt.seen.size() == 7, "RuntimeMXBean: 7 calls, saw " + rt.seen.size());

        Recorder ms = new Recorder("mbs");
        javax.management.MBeanServer srv = proxy(javax.management.MBeanServer.class, ms);
        check(srv.getMBeanCount() == null, "MBeanServer.getMBeanCount");
        check("SENTINEL-mbs-getDefaultDomain".equals(srv.getDefaultDomain()),
                "MBeanServer.getDefaultDomain");
        check(srv.getDomains() == null, "MBeanServer.getDomains");
        check(srv.queryNames(null, null) == null, "MBeanServer.queryNames");
        check(srv.isRegistered(null) == B, "MBeanServer.isRegistered");
        check(ms.seen.size() == 5, "MBeanServer: 5 calls, saw " + ms.seen.size());
        System.out.println("CK RForeignLayoutJdkInterfaces thr=" + t.seen.size()
                + " rt=" + rt.seen.size() + " mbs=" + ms.seen.size());

        // The REAL beans are unaffected. `> 0` and `>= 0` are satisfied by any
        // plausible constant, which is what the proxies above hand back by
        // design -- so each weak bound is paired with a cross-accessor
        // invariant, the same shape RJdkJmx's platform-bean block uses. Nothing
        // below is a host constant: every right-hand side is read at run time
        // from the same VM, in an order that can only widen the comparison.
        java.lang.management.ThreadMXBean realTh =
                java.lang.management.ManagementFactory.getThreadMXBean();
        check(realTh.getThreadCount() > 0, "the real ThreadMXBean reports at least one thread");
        // The thread executing this line is live by construction, so it MUST be
        // in the list. A fabricated array of plausible-looking ids is not.
        long selfId = Thread.currentThread().getId();
        boolean sawSelfId = false;
        for (long id : realTh.getAllThreadIds()) {
            if (id == selfId) {
                sawSelfId = true;
            }
        }
        check(sawSelfId, "getAllThreadIds() must contain the current thread " + selfId);

        java.lang.management.RuntimeMXBean realRt =
                java.lang.management.ManagementFactory.getRuntimeMXBean();
        long realStart = realRt.getStartTime();
        long realUptime = realRt.getUptime();
        long nowMs = System.currentTimeMillis();
        check(realUptime >= 0, "the real RuntimeMXBean reports a non-negative uptime");
        // The VM cannot have started in the future.
        check(realStart > 0 && realStart <= nowMs,
                "RuntimeMXBean.getStartTime " + realStart + " is not in the past");
        // NOT `start + uptime <= now`. That reads as sound and is not: MEASURED
        // on HotSpot 25.0.3.9 it fails by ~48ms, because getStartTime() is a
        // wall-clock instant captured at VM start while getUptime() is measured
        // from a monotonic source with a different base. The two are only
        // approximately commensurable, so any strict inequality between them is
        // a latent flake -- on the ORACLE as well as on CratonVM.
        //
        // What the row is actually for is catching an uptime that is a constant
        // unrelated to this VM. Two properties express that without pitting the
        // clocks against each other:
        //   * uptime must ADVANCE across a real sleep -- a constant cannot;
        //   * uptime must agree with (now - start) to within a generous slack,
        //     which a fabricated value unrelated to the start instant fails.
        long uptimeBefore = realRt.getUptime();
        try {
            Thread.sleep(50L);
        } catch (InterruptedException ie) {
            Thread.currentThread().interrupt();
        }
        check(realRt.getUptime() > uptimeBefore,
                "getUptime() must advance across a sleep; stayed at " + uptimeBefore);
        long drift = Math.abs((nowMs - realStart) - realUptime);
        check(drift <= 5000L,
                "uptime " + realUptime + " must agree with now-start " + (nowMs - realStart)
                        + " to within 5s; drift was " + drift);
        System.out.println("CK RForeignLayoutJdkInterfaces realBeans=ok");
    }

    /**
     * javax.xml.stream.XMLStreamReader (43) and
     * java.nio.file.attribute.DosFileAttributes (13). Both are interfaces an
     * application implements directly: a custom StAX source, and the attribute
     * view a custom FileSystemProvider hands back.
     */
    static void xmlAndAttributes() throws Exception {
        Recorder x = new Recorder("xml");
        javax.xml.stream.XMLStreamReader xr = proxy(javax.xml.stream.XMLStreamReader.class, x);
        check(xr.getEventType() == I, "XMLStreamReader.getEventType");
        check(xr.next() == I, "XMLStreamReader.next");
        check(xr.hasNext() == B, "XMLStreamReader.hasNext");
        check(xr.isStartElement() == B, "XMLStreamReader.isStartElement");
        check(xr.isEndElement() == B, "XMLStreamReader.isEndElement");
        check(xr.isCharacters() == B, "XMLStreamReader.isCharacters");
        check(xr.getAttributeCount() == I, "XMLStreamReader.getAttributeCount");
        check("SENTINEL-xml-getText".equals(xr.getText()), "XMLStreamReader.getText");
        check("SENTINEL-xml-getLocalName".equals(xr.getLocalName()),
                "XMLStreamReader.getLocalName");
        check(xr.getName() == null, "XMLStreamReader.getName");
        check(xr.getLocation() == null, "XMLStreamReader.getLocation");
        xr.close();
        check(x.seen.size() == 12, "XMLStreamReader: 12 calls, saw " + x.seen.size());

        Recorder a = new Recorder("dos");
        java.nio.file.attribute.DosFileAttributes da =
                proxy(java.nio.file.attribute.DosFileAttributes.class, a);
        check(da.isReadOnly() == B, "DosFileAttributes.isReadOnly");
        check(da.isHidden() == B, "DosFileAttributes.isHidden");
        check(da.isArchive() == B, "DosFileAttributes.isArchive");
        check(da.isSystem() == B, "DosFileAttributes.isSystem");
        check(da.isDirectory() == B, "DosFileAttributes.isDirectory");
        check(da.isRegularFile() == B, "DosFileAttributes.isRegularFile");
        check(da.isSymbolicLink() == B, "DosFileAttributes.isSymbolicLink");
        check(da.isOther() == B, "DosFileAttributes.isOther");
        check(da.size() == L, "DosFileAttributes.size");
        check(da.lastModifiedTime() == null, "DosFileAttributes.lastModifiedTime");
        check(da.creationTime() == null, "DosFileAttributes.creationTime");
        check(da.fileKey() == null, "DosFileAttributes.fileKey");
        check(a.seen.size() == 12, "DosFileAttributes: 12 calls, saw " + a.seen.size());
        System.out.println("CK RForeignLayoutJdkInterfaces xml=" + x.seen.size()
                + " dos=" + a.seen.size());
    }

    /** Run {@code body}, and check it threw exactly {@code expected}. */
    static void checkThrows(Class<?> expected, Runnable body, String what) {
        checks++;
        try {
            body.run();
        } catch (Throwable t) {
            if (t.getClass() == expected) {
                return;
            }
            throw new AssertionError("RForeignLayoutJdkInterfaces: " + what + " threw "
                    + t.getClass().getName() + ": " + t.getMessage()
                    + ", expected exactly " + expected.getName());
        }
        throw new AssertionError("RForeignLayoutJdkInterfaces: " + what
                + " did not throw " + expected.getName());
    }

    /**
     * java.lang.foreign arena LIFETIME — the one thing in this package that is
     * not a dispatch question.
     *
     * WHY IT IS HERE. This file's header explains that {@code
     * java.lang.foreign.MemorySegment} is sealed and so cannot receive a foreign
     * implementor; that sentence was, until this method, the ONLY mention of
     * {@code java.lang.foreign} anywhere in the 70 classes {@code run.sh}
     * schedules. So no scheduled fixture opened an {@code Arena}, and the FFM
     * lifetime model — {@code close()}, {@code isAlive()}, {@code scope()}
     * identity — had no vector at all.
     *
     * WHAT WAS WRONG. The model keeps four words on a {@code
     * jdk.internal.foreign.MemorySessionImpl} carrier, and in Compatible mode
     * that carrier is the REAL loaded class, whose slot 0 is a declared
     * REFERENCE. The model wrote its {@code int} state word there, the
     * "is this a session we built?" predicate was "does slot 0 read back as an
     * int", and it therefore answered false for every session this VM mints.
     * With it the whole model went inert: {@code close()} recorded nothing,
     * {@code isAlive()} answered true forever, a closed arena still allocated,
     * and a second {@code close()} was silently accepted. Every row below marked
     * RED was measured in that state on a current-dev binary
     * (probes/MemorySessionValidStateProbe.java sections C/D,
     * probes/MemorySessionIdentityProbe.java, probes/MemorySessionPreAllocProbe.java);
     * the HotSpot column is Eclipse Adoptium jdk-25.0.3.9.
     *
     * WHAT IT DELIBERATELY DOES NOT TOUCH. {@code MemorySegment.get}/{@code set}
     * and the whole raw-address family are gated on {@code
     * --enable-native-access}, which this suite does not pass, so they would
     * raise {@code IllegalCallerException} here for a reason that has nothing to
     * do with liveness. That is also why thread confinement — the widest of the
     * four behaviour changes — is absent: it fires on the segment ACCESS path
     * only. {@code allocate}, {@code close}, {@code scope} and {@code byteSize}
     * are ungated, which is the whole surface used below.
     *
     * The over-correction arm is not optional and is interleaved on purpose: a
     * gate that refuses everything satisfies every RED row and fails every LIVE
     * one, and the arm a fix is most likely to have broken is the one that must
     * still pass.
     */
    static void foreignArenaLifetime() {
        // --- LIVE (green before AND after): none of this may start throwing ---
        Arena confined = Arena.ofConfined();
        check(confined.scope().isAlive(), "a live confined arena's scope is alive");
        MemorySegment live = confined.allocate(16L);
        check(live != null, "a live confined arena allocates");
        check(live.byteSize() == 16L, "the segment is 16 bytes, got " + live.byteSize());

        // scope() must return the SAME session object every time it is asked.
        // RED: `false` — `Arena.scope()` minted a fresh, always-open session on
        // every call, because the predicate that recognises the one it is holding
        // was answering false. A fresh session is alive forever, so this row and
        // the closed-arena rows below are two faces of one defect.
        check(confined.scope() == confined.scope(),
                "arena.scope() is stable across calls");
        check(live.scope() == confined.scope(),
                "a segment's scope is its arena's scope");
        check(confined.allocate(8L).scope() == confined.scope(),
                "a second segment shares the same scope");

        // --- the RED rows: a CLOSED confined arena ---
        confined.close();
        // RED: `true`. HotSpot: false.
        check(!confined.scope().isAlive(),
                "a closed arena's scope is NOT alive");
        // RED: NO-THROW, returning a fresh 8-byte segment from a dead arena.
        checkThrows(IllegalStateException.class,
                () -> confined.allocate(8L), "allocate() on a closed arena");
        // RED: NO-THROW. A second close is an IllegalStateException on HotSpot.
        checkThrows(IllegalStateException.class,
                () -> confined.close(), "close() on an already-closed arena");
        System.out.println("CK RForeignLayoutJdkInterfaces confinedArena=closed");

        // --- the same three on a SHARED arena, which is a different factory ---
        Arena shared = Arena.ofShared();
        check(shared.scope().isAlive(), "a live shared arena's scope is alive");
        check(shared.allocate(16L).byteSize() == 16L, "a live shared arena allocates 16");
        check(shared.scope() == shared.scope(), "shared arena.scope() is stable");
        shared.close();
        check(!shared.scope().isAlive(), "a closed shared arena's scope is NOT alive");
        checkThrows(IllegalStateException.class,
                () -> shared.allocate(8L), "allocate() on a closed shared arena");
        checkThrows(IllegalStateException.class,
                () -> shared.close(), "close() on an already-closed shared arena");
        System.out.println("CK RForeignLayoutJdkInterfaces sharedArena=closed");

        // --- the arenas that can NEVER close: the over-correction guard ---
        // Nothing here changed and nothing here may change. A liveness gate that
        // reads the wrong state encoding reports these as closed, which is the
        // exact over-correction the repair had to avoid: the real JDK's own
        // sessions encode OPEN as 0 and CLOSED as -1, the inverse of the model's.
        check(Arena.global().scope().isAlive(), "the global arena is alive");
        check(Arena.global().allocate(16L).byteSize() == 16L, "the global arena allocates");
        check(Arena.ofAuto().scope().isAlive(), "an automatic arena is alive");
        check(Arena.ofAuto().allocate(16L).byteSize() == 16L, "an automatic arena allocates");

        // A HEAP segment's scope is a session that can never close, and it is the
        // one scope-stability row that was ALREADY green before the repair — so it
        // is the row that catches a change to the slot map rather than to the gate.
        MemorySegment heap = MemorySegment.ofArray(new byte[16]);
        check(heap.byteSize() == 16L, "a heap segment is 16 bytes");
        check(heap.scope().isAlive(), "a heap segment's scope is alive");
        check(heap.scope() == heap.scope(), "a heap segment's scope is stable");
        System.out.println("CK RForeignLayoutJdkInterfaces neverCloses=ok");
    }

    public static void main(String[] args) throws Exception {
        sqlConnection();
        sqlResultSet();
        sqlStatements();
        nioPath();
        processAndSession();
        managementBeans();
        xmlAndAttributes();
        foreignArenaLifetime();
        System.out.println("CK RForeignLayoutJdkInterfaces checks=" + checks);
        System.out.println("PASS RForeignLayoutJdkInterfaces (" + checks + " checks)");
    }
}
