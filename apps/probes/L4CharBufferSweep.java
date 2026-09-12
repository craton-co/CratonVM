import java.nio.CharBuffer;
import java.nio.ByteBuffer;

/**
 * Lane 4 wave 4, {@code java/nio/CharBuffer}.
 *
 * Section 4 of the lane page names this lane's failure mode -- a silent wrong
 * answer -- and names the shape of the instrument that can see one: read the
 * DATA back, do not check a return code. So every buffer operation here is
 * followed by a state triple AND by the content the buffer would hand a reader,
 * taken absolutely so that reading it does not disturb the state being reported.
 *
 * Every row goes through {@link #t}, so a throw is a LINE rather than an abort;
 * a section that stops at its first throw hides every silent defect behind it.
 */
public class L4CharBufferSweep {

    static void p(String tag, Object v) { System.out.println(tag + " |" + v + "|"); }

    static void t(String tag, java.util.concurrent.Callable<Object> c) {
        try { p(tag, c.call()); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName() + ": " + e.getMessage()); }
    }

    /** Exception KIND only -- for rows whose message carries an identity hash. */
    static void k(String tag, java.util.concurrent.Callable<Object> c) {
        try { c.call(); p(tag, "no-throw"); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }

    static String cls(Object o) { return o == null ? "null" : o.getClass().getName(); }

    /** position/limit/capacity/remaining -- the four the real accessors derive. */
    static String st(CharBuffer b) {
        return b.position() + "/" + b.limit() + "/" + b.capacity() + "/" + b.remaining();
    }

    /** Everything up to the limit, read ABSOLUTELY so position does not move. */
    static String win(CharBuffer b) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < b.limit(); i++) {
            try { sb.append(b.get(i)); } catch (Throwable e) { sb.append('?'); }
        }
        return sb.toString();
    }

    /** The whole backing store, past the limit -- a compact() that does not
     *  clear the tail is invisible to {@link #win}. */
    static String backing(CharBuffer b) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < b.capacity(); i++) {
            try { sb.append(b.get(i)); } catch (Throwable e) { sb.append('?'); }
        }
        return sb.toString();
    }

    /** The four facts about one buffer that a wrong carrier moves. */
    static void dump(String tag, CharBuffer b) {
        p(tag + ".class", cls(b));
        t(tag + ".state", () -> st(b));
        t(tag + ".win", () -> win(b));
        t(tag + ".backing", () -> backing(b));
    }

    static CharBuffer abcdef() {
        CharBuffer b = CharBuffer.allocate(6);
        b.put("abcdef");
        b.clear();
        return b;
    }

    /**
     * The same rows the sections below ask through a lambda, asked DIRECTLY.
     *
     * Not redundant, and not style. `hasArray()` is one of this wave's
     * seventeen and the census read `invocations: 0` for it across four
     * lambda-wrapped calls that all answered correctly -- so the answer came
     * from somewhere the registry never saw. Called plainly off a local, the
     * same triple reads 5. A funnel that cannot show a row was INVOKED must not
     * take that row, so the probe asks in the shape that reaches it.
     */
    static void direct() {
        CharBuffer b = CharBuffer.allocate(4);
        p("direct.hasArray", b.hasArray());
        p("direct.array.len", b.array().length);
        p("direct.arrayOffset", b.arrayOffset());
        p("direct.capacity", b.capacity());
        p("direct.position", b.position());
        p("direct.limit", b.limit());
        p("direct.remaining", b.remaining());
        p("direct.hasRemaining", b.hasRemaining());
        p("direct.position.set", st(b.position(1)));
        p("direct.limit.set", st(b.limit(3)));
        p("direct.flip", st(b.flip()));
        p("direct.rewind", st(b.rewind()));
        p("direct.clear", st(b.clear()));
        p("direct.toString.len", b.toString().length());
        CharBuffer w = CharBuffer.wrap(new char[]{'q'});
        p("direct.wrap.hasArray", w.hasArray());
        p("direct.wrap.arrayOffset", w.arrayOffset());
    }

    public static void main(String[] args) {
        direct();
        factories();
        stateMachine();
        views();
        charSequence();
        bulk();
        boundaries();
        valueSemantics();
        readOnly();
        sharing();
        interop();
    }

    // ------------------------------------------------------------------ 1
    static void factories() {
        t("alloc.class", () -> cls(CharBuffer.allocate(4)));
        t("alloc.state", () -> st(CharBuffer.allocate(4)));
        t("alloc.hasArray", () -> CharBuffer.allocate(4).hasArray());
        t("alloc.arrayOffset", () -> CharBuffer.allocate(4).arrayOffset());
        t("alloc.arrayLen", () -> CharBuffer.allocate(4).array().length);
        t("alloc.isReadOnly", () -> CharBuffer.allocate(4).isReadOnly());
        t("alloc.isDirect", () -> CharBuffer.allocate(4).isDirect());
        t("alloc.isEmpty", () -> CharBuffer.allocate(4).isEmpty());
        t("alloc.0.isEmpty", () -> CharBuffer.allocate(0).isEmpty());
        t("alloc.neg", () -> CharBuffer.allocate(-1));

        char[] src = {'a', 'b', 'c', 'd'};
        t("wrapA.class", () -> cls(CharBuffer.wrap(src)));
        t("wrapA.state", () -> st(CharBuffer.wrap(src)));
        t("wrapA.win", () -> win(CharBuffer.wrap(src)));
        // IDENTITY, not equality: a wrap that COPIES is a silent wrong answer.
        t("wrapA.array.same", () -> CharBuffer.wrap(src).array() == src);
        t("wrapA.arrayOffset", () -> CharBuffer.wrap(src).arrayOffset());
        t("wrapA.writeThrough", () -> {
            char[] a = {'a', 'b', 'c', 'd'};
            CharBuffer b = CharBuffer.wrap(a);
            b.put(1, 'Z');
            return new String(a);
        });
        t("wrapA.readThrough", () -> {
            char[] a = {'a', 'b', 'c', 'd'};
            CharBuffer b = CharBuffer.wrap(a);
            a[2] = 'Y';
            return win(b);
        });

        t("wrapAR.state", () -> st(CharBuffer.wrap(src, 1, 2)));
        t("wrapAR.win", () -> win(CharBuffer.wrap(src, 1, 2)));
        t("wrapAR.arrayOffset", () -> CharBuffer.wrap(src, 1, 2).arrayOffset());
        t("wrapAR.remaining", () -> CharBuffer.wrap(src, 1, 2).remaining());
        t("wrapAR.bad", () -> CharBuffer.wrap(src, 3, 9));

        t("wrapS.class", () -> cls(CharBuffer.wrap("abcd")));
        t("wrapS.state", () -> st(CharBuffer.wrap("abcd")));
        t("wrapS.win", () -> win(CharBuffer.wrap("abcd")));
        t("wrapS.isReadOnly", () -> CharBuffer.wrap("abcd").isReadOnly());
        t("wrapS.hasArray", () -> CharBuffer.wrap("abcd").hasArray());
        t("wrapS.array", () -> CharBuffer.wrap("abcd").array());
        t("wrapS.put", () -> CharBuffer.wrap("abcd").put(0, 'Z'));
        t("wrapS.toString", () -> CharBuffer.wrap("abcd").toString());

        t("wrapSR.state", () -> st(CharBuffer.wrap("abcdef", 1, 4)));
        t("wrapSR.win", () -> win(CharBuffer.wrap("abcdef", 1, 4)));
        t("wrapSR.toString", () -> CharBuffer.wrap("abcdef", 1, 4).toString());
        t("wrapSR.remaining", () -> CharBuffer.wrap("abcdef", 1, 4).remaining());
        t("wrapSR.bad", () -> CharBuffer.wrap("abcdef", 4, 1));
    }

    // ------------------------------------------------------------------ 2
    static void stateMachine() {
        t("put.rel.state", () -> { CharBuffer b = CharBuffer.allocate(6); b.put('x'); b.put('y'); return st(b); });
        t("put.rel.backing", () -> { CharBuffer b = CharBuffer.allocate(6); b.put('x'); b.put('y'); return backing(b); });
        t("put.rel.returns.this", () -> { CharBuffer b = CharBuffer.allocate(6); return b.put('x') == b; });
        t("put.abs.state", () -> { CharBuffer b = CharBuffer.allocate(6); b.put(3, 'q'); return st(b); });
        t("put.abs.backing", () -> { CharBuffer b = CharBuffer.allocate(6); b.put(3, 'q'); return backing(b); });

        t("get.rel.value", () -> { CharBuffer b = abcdef(); return "" + b.get() + b.get(); });
        t("get.rel.state", () -> { CharBuffer b = abcdef(); b.get(); b.get(); return st(b); });
        t("get.abs.value", () -> abcdef().get(4));
        t("get.abs.state", () -> { CharBuffer b = abcdef(); b.get(4); return st(b); });

        t("flip.state", () -> { CharBuffer b = CharBuffer.allocate(6); b.put("abc"); b.flip(); return st(b); });
        t("flip.win", () -> { CharBuffer b = CharBuffer.allocate(6); b.put("abc"); b.flip(); return win(b); });
        t("flip.returns.this", () -> { CharBuffer b = CharBuffer.allocate(6); return b.flip() == b; });
        t("flip.class", () -> { CharBuffer b = CharBuffer.allocate(6); return cls(b.flip()); });
        t("flip.twice", () -> { CharBuffer b = CharBuffer.allocate(6); b.put("abc"); b.flip(); b.flip(); return st(b); });

        t("rewind.state", () -> { CharBuffer b = abcdef(); b.get(); b.get(); b.rewind(); return st(b); });
        t("rewind.afterFlip", () -> { CharBuffer b = CharBuffer.allocate(6); b.put("abc"); b.flip(); b.get(); b.rewind(); return st(b); });

        t("clear.state", () -> { CharBuffer b = abcdef(); b.get(); b.flip(); b.clear(); return st(b); });
        t("clear.keepsData", () -> { CharBuffer b = abcdef(); b.clear(); return backing(b); });

        t("mark.reset.state", () -> { CharBuffer b = abcdef(); b.get(); b.get(); b.mark(); b.get(); b.reset(); return st(b); });
        t("mark.reset.value", () -> { CharBuffer b = abcdef(); b.get(); b.get(); b.mark(); b.get(); b.reset(); return b.get(); });
        t("reset.noMark", () -> { CharBuffer b = abcdef(); return b.reset(); });
        t("reset.afterClear", () -> { CharBuffer b = abcdef(); b.get(); b.mark(); b.clear(); return b.reset(); });
        t("reset.afterFlip", () -> { CharBuffer b = abcdef(); b.get(); b.mark(); b.flip(); return b.reset(); });
        t("reset.afterRewind", () -> { CharBuffer b = abcdef(); b.get(); b.mark(); b.rewind(); return b.reset(); });
        // position() BELOW the mark discards it; a carrier that keeps the mark
        // where the real accessor cannot see it answers this one wrong.
        t("mark.discardedByPosition", () -> { CharBuffer b = abcdef(); b.position(3); b.mark(); b.position(1); return b.reset(); });
        t("mark.keptByPosition", () -> { CharBuffer b = abcdef(); b.position(1); b.mark(); b.position(3); b.reset(); return st(b); });
        t("mark.discardedByLimit", () -> { CharBuffer b = abcdef(); b.position(4); b.mark(); b.limit(2); return b.reset(); });

        t("position.set.state", () -> { CharBuffer b = abcdef(); b.position(2); return st(b); });
        t("position.set.returns.this", () -> { CharBuffer b = abcdef(); return b.position(2) == b; });
        t("position.set.class", () -> cls(abcdef().position(2)));
        t("position.past.limit", () -> { CharBuffer b = abcdef(); b.limit(3); return b.position(4); });
        t("position.neg", () -> abcdef().position(-1));

        t("limit.set.state", () -> { CharBuffer b = abcdef(); b.limit(3); return st(b); });
        t("limit.shrinks.position", () -> { CharBuffer b = abcdef(); b.position(5); b.limit(2); return st(b); });
        t("limit.past.capacity", () -> abcdef().limit(9));
        t("limit.neg", () -> abcdef().limit(-1));

        t("compact.state", () -> { CharBuffer b = abcdef(); b.position(2); b.limit(5); b.compact(); return st(b); });
        t("compact.win", () -> { CharBuffer b = abcdef(); b.position(2); b.limit(5); b.compact(); return win(b); });
        t("compact.backing", () -> { CharBuffer b = abcdef(); b.position(2); b.limit(5); b.compact(); return backing(b); });
        t("compact.class", () -> { CharBuffer b = abcdef(); b.position(2); return cls(b.compact()); });
        t("compact.empty", () -> { CharBuffer b = abcdef(); b.position(6); b.compact(); return st(b); });
        t("compact.discardsMark", () -> { CharBuffer b = abcdef(); b.position(1); b.mark(); b.position(2); b.compact(); return b.reset(); });

        t("hasRemaining.full", () -> abcdef().hasRemaining());
        t("hasRemaining.drained", () -> { CharBuffer b = abcdef(); b.position(6); return b.hasRemaining(); });
        t("remaining.mid", () -> { CharBuffer b = abcdef(); b.position(2); b.limit(5); return b.remaining(); });
    }

    // ------------------------------------------------------------------ 3
    static void views() {
        t("slice.class", () -> { CharBuffer b = abcdef(); b.position(2); return cls(b.slice()); });
        t("slice.state", () -> { CharBuffer b = abcdef(); b.position(2); return st(b.slice()); });
        t("slice.win", () -> { CharBuffer b = abcdef(); b.position(2); return win(b.slice()); });
        t("slice.arrayOffset", () -> { CharBuffer b = abcdef(); b.position(2); return b.slice().arrayOffset(); });
        t("slice.array.same", () -> { CharBuffer b = abcdef(); b.position(2); return b.slice().array() == b.array(); });
        t("slice.limited", () -> { CharBuffer b = abcdef(); b.position(2); b.limit(5); return st(b.slice()); });
        t("slice.sharesWrite", () -> { CharBuffer b = abcdef(); b.position(2); CharBuffer s = b.slice(); s.put(0, 'Z'); return backing(b); });
        t("slice.seesWrite", () -> { CharBuffer b = abcdef(); b.position(2); CharBuffer s = b.slice(); b.put(3, 'Y'); return win(s); });
        t("slice.leavesSrc", () -> { CharBuffer b = abcdef(); b.position(2); b.slice(); return st(b); });

        t("slice2.state", () -> { CharBuffer b = abcdef(); b.position(1); return st(b.slice(2, 3)); });
        t("slice2.win", () -> { CharBuffer b = abcdef(); b.position(1); return win(b.slice(2, 3)); });
        t("slice2.arrayOffset", () -> { CharBuffer b = abcdef(); b.position(1); return b.slice(2, 3).arrayOffset(); });
        t("slice2.compose", () -> { CharBuffer b = abcdef(); return b.slice(2, 4).slice(1, 2).arrayOffset(); });
        t("slice2.bad", () -> abcdef().slice(4, 9));

        t("dup.class", () -> cls(abcdef().duplicate()));
        t("dup.state", () -> { CharBuffer b = abcdef(); b.position(2); b.limit(5); return st(b.duplicate()); });
        t("dup.win", () -> { CharBuffer b = abcdef(); b.position(2); return win(b.duplicate()); });
        t("dup.array.same", () -> abcdef().duplicate().array() == abcdef().array());
        t("dup.sharesWrite", () -> { CharBuffer b = abcdef(); CharBuffer d = b.duplicate(); d.put(0, 'Z'); return backing(b); });
        t("dup.independentPosition", () -> { CharBuffer b = abcdef(); CharBuffer d = b.duplicate(); d.get(); return st(b) + " vs " + st(d); });
        t("dup.carriesMark", () -> { CharBuffer b = abcdef(); b.position(2); b.mark(); b.position(4); CharBuffer d = b.duplicate(); d.reset(); return st(d); });

        t("ro.class", () -> cls(abcdef().asReadOnlyBuffer()));
        t("ro.isReadOnly", () -> abcdef().asReadOnlyBuffer().isReadOnly());
        t("ro.state", () -> { CharBuffer b = abcdef(); b.position(2); return st(b.asReadOnlyBuffer()); });
        t("ro.win", () -> abcdef().asReadOnlyBuffer().toString());
        t("ro.hasArray", () -> abcdef().asReadOnlyBuffer().hasArray());
        t("ro.array", () -> abcdef().asReadOnlyBuffer().array());
        t("ro.arrayOffset", () -> abcdef().asReadOnlyBuffer().arrayOffset());
        t("ro.put", () -> abcdef().asReadOnlyBuffer().put('z'));
        t("ro.putAbs", () -> abcdef().asReadOnlyBuffer().put(0, 'z'));
        t("ro.compact", () -> abcdef().asReadOnlyBuffer().compact());
        t("ro.seesWrite", () -> { CharBuffer b = abcdef(); CharBuffer r = b.asReadOnlyBuffer(); b.put(0, 'Z'); return win(r); });
        t("ro.dup.isReadOnly", () -> abcdef().asReadOnlyBuffer().duplicate().isReadOnly());
        t("ro.slice.isReadOnly", () -> abcdef().asReadOnlyBuffer().slice().isReadOnly());

        t("order.value", () -> abcdef().order());
        t("order.wrapS", () -> CharBuffer.wrap("ab").order());
        t("order.ro", () -> abcdef().asReadOnlyBuffer().order());
    }

    // ------------------------------------------------------------------ 4
    static void charSequence() {
        t("length.full", () -> abcdef().length());
        t("length.windowed", () -> { CharBuffer b = abcdef(); b.position(2); b.limit(5); return b.length(); });
        t("charAt.0", () -> abcdef().charAt(0));
        t("charAt.windowed", () -> { CharBuffer b = abcdef(); b.position(2); return b.charAt(0); });
        t("charAt.windowed.1", () -> { CharBuffer b = abcdef(); b.position(2); return b.charAt(1); });
        t("charAt.oob", () -> { CharBuffer b = abcdef(); b.position(4); return b.charAt(3); });
        t("charAt.neg", () -> abcdef().charAt(-1));
        t("charAt.leavesPosition", () -> { CharBuffer b = abcdef(); b.position(2); b.charAt(1); return st(b); });

        // The row the lane page names: subSequence(1,3) of "bcdef" is "cd".
        t("subSeq.class", () -> cls(abcdef().subSequence(1, 3)));
        t("subSeq.plain", () -> abcdef().subSequence(1, 3).toString());
        t("subSeq.windowed", () -> { CharBuffer b = abcdef(); b.position(1); return b.subSequence(1, 3).toString(); });
        t("subSeq.windowed.state", () -> { CharBuffer b = abcdef(); b.position(1); return st(b.subSequence(1, 3)); });
        t("subSeq.leavesSrc", () -> { CharBuffer b = abcdef(); b.position(1); b.subSequence(1, 3); return st(b); });
        t("subSeq.whole", () -> abcdef().subSequence(0, 6).toString());
        t("subSeq.empty", () -> abcdef().subSequence(2, 2).toString());
        t("subSeq.oob", () -> abcdef().subSequence(1, 9));
        t("subSeq.reversed", () -> abcdef().subSequence(3, 1));
        t("subSeq.sharesWrite", () -> { CharBuffer b = abcdef(); CharBuffer s = b.subSequence(1, 3); s.put(0, 'Z'); return backing(b); });
        t("subSeq.wrapS", () -> CharBuffer.wrap("abcdef").subSequence(1, 3).toString());

        t("toString.full", () -> abcdef().toString());
        t("toString.windowed", () -> { CharBuffer b = abcdef(); b.position(2); b.limit(5); return b.toString(); });
        t("toString.empty", () -> { CharBuffer b = abcdef(); b.position(6); return b.toString(); });
        t("toString.leavesPosition", () -> { CharBuffer b = abcdef(); b.position(2); b.toString(); return st(b); });
        t("toString.fresh", () -> CharBuffer.allocate(3).toString().length());

        t("chars.count", () -> { CharBuffer b = abcdef(); b.position(2); return b.chars().count(); });
        t("chars.sum", () -> { CharBuffer b = abcdef(); b.position(2); return b.chars().sum(); });
        t("chars.leavesPosition", () -> { CharBuffer b = abcdef(); b.position(2); b.chars().count(); return st(b); });

        t("append.cs.state", () -> { CharBuffer b = CharBuffer.allocate(6); b.append("xy"); return st(b); });
        t("append.cs.backing", () -> { CharBuffer b = CharBuffer.allocate(6); b.append("xy"); return backing(b); });
        t("append.cs.returns.this", () -> { CharBuffer b = CharBuffer.allocate(6); return b.append("xy") == b; });
        t("append.csr", () -> { CharBuffer b = CharBuffer.allocate(6); b.append("abcdef", 1, 3); return backing(b); });
        t("append.char", () -> { CharBuffer b = CharBuffer.allocate(6); b.append('z'); return st(b) + " " + backing(b); });
        t("append.null", () -> { CharBuffer b = CharBuffer.allocate(6); b.append(null); return backing(b); });
        t("append.overflow", () -> { CharBuffer b = CharBuffer.allocate(2); return b.append("abc"); });
    }

    // ------------------------------------------------------------------ 5
    static void bulk() {
        t("getArr.state", () -> { CharBuffer b = abcdef(); char[] d = new char[3]; b.get(d); return st(b) + " " + new String(d); });
        t("getArr.range", () -> { CharBuffer b = abcdef(); char[] d = new char[5]; b.get(d, 1, 3); return st(b) + " " + new String(d).replace('\0', '.'); });
        t("getArr.underflow", () -> { CharBuffer b = abcdef(); b.position(4); return b.get(new char[3]); });
        t("getArr.abs", () -> { CharBuffer b = abcdef(); char[] d = new char[3]; b.get(2, d); return st(b) + " " + new String(d); });
        t("getArr.abs.range", () -> { CharBuffer b = abcdef(); char[] d = new char[4]; b.get(1, d, 1, 2); return new String(d).replace('\0', '.'); });
        t("getArr.abs.oob", () -> abcdef().get(5, new char[3]));

        t("putArr.state", () -> { CharBuffer b = CharBuffer.allocate(6); b.put(new char[]{'p', 'q'}); return st(b) + " " + backing(b); });
        t("putArr.range", () -> { CharBuffer b = CharBuffer.allocate(6); b.put(new char[]{'p', 'q', 'r'}, 1, 2); return st(b) + " " + backing(b); });
        t("putArr.abs", () -> { CharBuffer b = CharBuffer.allocate(6); b.put(2, new char[]{'p', 'q'}); return st(b) + " " + backing(b); });
        t("putArr.overflow", () -> { CharBuffer b = CharBuffer.allocate(2); return b.put(new char[]{'p', 'q', 'r'}); });

        t("putStr.state", () -> { CharBuffer b = CharBuffer.allocate(6); b.put("pq"); return st(b) + " " + backing(b); });
        t("putStr.range", () -> { CharBuffer b = CharBuffer.allocate(6); b.put("abcdef", 2, 4); return st(b) + " " + backing(b); });
        t("putStr.overflow", () -> { CharBuffer b = CharBuffer.allocate(2); return b.put("abc"); });
        t("putStr.returns.this", () -> { CharBuffer b = CharBuffer.allocate(6); return b.put("pq") == b; });

        t("putBuf.state", () -> {
            CharBuffer dst = CharBuffer.allocate(6);
            CharBuffer src = CharBuffer.wrap("xy");
            dst.put(src);
            return st(dst) + " " + st(src) + " " + backing(dst);
        });
        t("putBuf.abs", () -> {
            CharBuffer dst = CharBuffer.allocate(6);
            CharBuffer src = CharBuffer.wrap("xyz");
            dst.put(1, src, 1, 2);
            return st(dst) + " " + st(src) + " " + backing(dst);
        });
        t("putBuf.self", () -> { CharBuffer b = abcdef(); return b.put(b); });
        t("putBuf.overflow", () -> CharBuffer.allocate(1).put(CharBuffer.wrap("xy")));

        t("read.state", () -> { CharBuffer src = CharBuffer.wrap("abcd"); CharBuffer dst = CharBuffer.allocate(2); int n = src.read(dst); return n + " " + st(src) + " " + backing(dst); });
        t("read.drained", () -> { CharBuffer src = CharBuffer.wrap("ab"); src.position(2); return src.read(CharBuffer.allocate(2)); });
    }

    // ------------------------------------------------------------------ 6
    static void boundaries() {
        t("get.underflow", () -> { CharBuffer b = abcdef(); b.position(6); return b.get(); });
        t("get.abs.oob", () -> abcdef().get(6));
        t("get.abs.neg", () -> abcdef().get(-1));
        t("get.abs.pastLimit", () -> { CharBuffer b = abcdef(); b.limit(3); return b.get(4); });
        t("put.overflow", () -> { CharBuffer b = CharBuffer.allocate(1); b.put('a'); return b.put('b'); });
        t("put.abs.oob", () -> CharBuffer.allocate(2).put(2, 'a'));
        t("put.abs.pastLimit", () -> { CharBuffer b = CharBuffer.allocate(4); b.limit(2); return b.put(3, 'a'); });
        t("charAt.pastLimit", () -> { CharBuffer b = abcdef(); b.limit(2); return b.charAt(2); });
        t("slice2.neg", () -> abcdef().slice(-1, 2));
        t("wrapA.null", () -> CharBuffer.wrap((char[]) null));
        t("wrapS.null", () -> CharBuffer.wrap((CharSequence) null));
        t("putArr.null", () -> CharBuffer.allocate(2).put((char[]) null));
    }

    // ------------------------------------------------------------------ 7
    static void valueSemantics() {
        t("equals.same", () -> { CharBuffer a = abcdef(); return a.equals(a); });
        t("equals.equalContent", () -> CharBuffer.wrap("abc").equals(CharBuffer.wrap("abc")));
        t("equals.windowed", () -> {
            CharBuffer a = abcdef(); a.position(1); a.limit(4);
            return a.equals(CharBuffer.wrap("bcd"));
        });
        t("equals.differentContent", () -> CharBuffer.wrap("abc").equals(CharBuffer.wrap("abd")));
        t("equals.differentLength", () -> CharBuffer.wrap("abc").equals(CharBuffer.wrap("ab")));
        t("equals.nonBuffer", () -> CharBuffer.wrap("abc").equals("abc"));
        t("hashCode.stable", () -> { CharBuffer a = CharBuffer.wrap("abc"); return a.hashCode() == a.hashCode(); });
        t("hashCode.equalPair", () -> CharBuffer.wrap("abc").hashCode() == CharBuffer.wrap("abc").hashCode());
        t("hashCode.windowIndependent", () -> {
            CharBuffer a = abcdef(); a.position(1); a.limit(4);
            return a.hashCode() == CharBuffer.wrap("bcd").hashCode();
        });
        t("hashCode.value", () -> CharBuffer.wrap("abc").hashCode());
        t("compareTo.lt", () -> CharBuffer.wrap("abc").compareTo(CharBuffer.wrap("abd")));
        t("compareTo.eq", () -> CharBuffer.wrap("abc").compareTo(CharBuffer.wrap("abc")));
        t("compareTo.prefix", () -> CharBuffer.wrap("ab").compareTo(CharBuffer.wrap("abc")));
        t("mismatch.none", () -> CharBuffer.wrap("abc").mismatch(CharBuffer.wrap("abc")));
        t("mismatch.at1", () -> CharBuffer.wrap("abc").mismatch(CharBuffer.wrap("axc")));
        t("mismatch.shorter", () -> CharBuffer.wrap("ab").mismatch(CharBuffer.wrap("abc")));
        t("getChars", () -> { char[] d = new char[4]; CharBuffer b = abcdef(); b.position(1); b.getChars(1, 3, d, 0); return new String(d).replace('\0', '.'); });
    }

    // ------------------------------------------------------------------ 8
    static void readOnly() {
        t("wrapS.isEmpty", () -> CharBuffer.wrap("").isEmpty());
        t("wrapS.compact", () -> CharBuffer.wrap("abc").compact());
        t("wrapS.slice.class", () -> cls(CharBuffer.wrap("abcdef").slice()));
        t("wrapS.slice.win", () -> { CharBuffer b = CharBuffer.wrap("abcdef"); b.position(2); return b.slice().toString(); });
        t("wrapS.dup.isReadOnly", () -> CharBuffer.wrap("abc").duplicate().isReadOnly());
        t("wrapS.equalsHeap", () -> CharBuffer.wrap("abc").equals(CharBuffer.wrap(new char[]{'a', 'b', 'c'})));
        t("wrapS.hashEqHeap", () -> CharBuffer.wrap("abc").hashCode() == CharBuffer.wrap(new char[]{'a', 'b', 'c'}).hashCode());
        t("wrapS.getRel", () -> { CharBuffer b = CharBuffer.wrap("abc"); return "" + b.get() + b.get(); });
        t("wrapS.charAt", () -> CharBuffer.wrap("abcdef").charAt(2));
        t("wrapS.length", () -> CharBuffer.wrap("abcdef").length());
    }

    // ------------------------------------------------------------------ 9
    static void sharing() {
        t("share.slice.chain", () -> {
            CharBuffer b = abcdef();
            b.position(1);
            CharBuffer s1 = b.slice();
            s1.position(1);
            CharBuffer s2 = s1.slice();
            s2.put(0, 'Z');
            return backing(b) + " " + s2.arrayOffset();
        });
        t("share.dupOfSlice", () -> {
            CharBuffer b = abcdef();
            b.position(2);
            CharBuffer d = b.slice().duplicate();
            d.put(0, 'Z');
            return backing(b) + " " + st(d);
        });
        t("share.roOfSlice", () -> {
            CharBuffer b = abcdef();
            b.position(2);
            CharBuffer r = b.slice().asReadOnlyBuffer();
            b.put(2, 'Z');
            return win(r) + " " + r.isReadOnly();
        });
        t("share.arrayIdentityChain", () -> {
            CharBuffer b = abcdef();
            return b.slice().array() == b.duplicate().array();
        });
    }

    // ------------------------------------------------------------------ 10
    static void interop() {
        t("bb.asCharBuffer.class", () -> cls(ByteBuffer.allocate(8).asCharBuffer()));
        t("bb.asCharBuffer.state", () -> st(ByteBuffer.allocate(8).asCharBuffer()));
        t("bb.asCharBuffer.hasArray", () -> ByteBuffer.allocate(8).asCharBuffer().hasArray());
        t("bb.asCharBuffer.order", () -> ByteBuffer.allocate(8).asCharBuffer().order());
        t("bb.asCharBuffer.roundtrip", () -> {
            ByteBuffer bb = ByteBuffer.allocate(8);
            CharBuffer cb = bb.asCharBuffer();
            cb.put("ab");
            return bb.getChar(0) + "" + bb.getChar(2);
        });
        t("bb.asCharBuffer.toString", () -> {
            ByteBuffer bb = ByteBuffer.allocate(8);
            CharBuffer cb = bb.asCharBuffer();
            cb.put("abcd");
            cb.flip();
            return cb.toString();
        });
        t("decode.state", () -> {
            CharBuffer cb = java.nio.charset.StandardCharsets.UTF_8
                    .decode(ByteBuffer.wrap(new byte[]{'h', 'i'}));
            return st(cb) + " " + cb.toString();
        });
        t("encode.roundtrip", () -> {
            ByteBuffer bb = java.nio.charset.StandardCharsets.UTF_8.encode(CharBuffer.wrap("hi"));
            return bb.remaining() + " " + (char) bb.get(0) + (char) bb.get(1);
        });
        t("sb.append.cb", () -> new StringBuilder().append(abcdef()).toString());
        t("string.valueOf", () -> String.valueOf((Object) CharBuffer.wrap("abc")));
        t("matcher.on.cb", () -> java.util.regex.Pattern.compile("cd").matcher(abcdef()).find());
        t("matcher.windowed", () -> {
            CharBuffer b = abcdef();
            b.position(3);
            return java.util.regex.Pattern.compile("^def$").matcher(b).matches();
        });
    }
}
