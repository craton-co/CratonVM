/*
 * CalleeTierUpProbe - does a callee invoked from a hot loop ever tier up?
 *
 * Dependency-free reduction of the regression found behind known-issue
 * tomcat/32.1. There, `MappingData.recycle()` -- ordinary bytecode, a handful
 * of field stores plus four nested `recycle()` calls -- went from 0.73 us to
 * 24-30 us per call between dev b695d468f (2026-07-27) and 9b86a9ac1
 * (2026-07-30), while the native Mapper shadow the doc blamed barely moved.
 *
 * `CRATONVM_DBG_JIT_METHOD_STATS=1` on the Tomcat form reported, for the same
 * recycle-only loop:
 *     old: 5 methods tracked, 4 ever invoked, 2192 invocations, c1=4 c2=4
 *     new: 1 method  tracked, 0 ever invoked,    0 invocations, c1=0 c2=0
 * The driving loop OSR-compiles in both; on the new binary its callees are
 * never counted, never enqueued and never compiled, so they interpret forever.
 *
 * This probe reproduces that shape with no Tomcat on the classpath: a
 * once-invoked driver holding a hot loop, calling a virtual method that writes
 * fields and calls further nested methods. Run it under
 * CRATONVM_DBG_JIT_METHOD_STATS=1 and compare `c1=`/`c2=` and
 * `total invocations` against HotSpot, or across two CratonVM builds.
 *
 * Usage:  cratonvm -cp <dir> CalleeTierUpProbe [iters] [rounds]
 */
public class CalleeTierUpProbe {

    /** Stands in for AbstractChunk: the innermost callee, pure field stores. */
    static final class Chunk {
        int start;
        int end;
        boolean isSet;
        boolean hasHashCode;
        char[] buff;

        void recycle() {
            // Deliberately mirrors AbstractChunk.recycle(): clears the cursor
            // state but KEEPS buff, so nothing here allocates.
            start = 0;
            end = 0;
            isSet = false;
            hasHashCode = false;
        }
    }

    /** Stands in for MessageBytes. */
    static final class Bytes {
        int type;
        String strValue;
        boolean hasHashCode;
        final Chunk byteC = new Chunk();
        final Chunk charC = new Chunk();

        void recycle() {
            type = 0;
            byteC.recycle();
            charC.recycle();
            strValue = null;
            hasHashCode = false;
        }
    }

    /** Stands in for MappingData. */
    static final class Data {
        Object host;
        Object context;
        int contextSlashCount;
        Object[] contexts;
        Object wrapper;
        boolean jspWildCard;
        Object matchType;
        final Bytes requestPath = new Bytes();
        final Bytes wrapperPath = new Bytes();
        final Bytes pathInfo = new Bytes();
        final Bytes redirectPath = new Bytes();

        void recycle() {
            host = null;
            context = null;
            contextSlashCount = 0;
            contexts = null;
            wrapper = null;
            jspWildCard = false;
            requestPath.recycle();
            wrapperPath.recycle();
            pathInfo.recycle();
            redirectPath.recycle();
            matchType = null;
        }
    }

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 1000000;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 3;

        Data data = new Data();
        for (int r = 0; r < rounds; r++) {
            long start = System.nanoTime();
            for (int i = 0; i < iters; i++) {
                data.recycle();
            }
            long ns = System.nanoTime() - start;
            // Keep `data` observably live so nothing above can be elided.
            if (data.contextSlashCount != 0) {
                throw new IllegalStateException("recycle did not run");
            }
            System.out.println("round=" + r + " iters=" + iters + " total=" + (ns / 1000000L)
                    + "ms per-call=" + (ns / (double) iters) + "ns");
        }
    }
}
