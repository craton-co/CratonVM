#!/usr/bin/env python3
"""Patch org.h2.mvstore.tx.TransactionStore with recovery diagnostics.

Usage: patch-transactionstore.py <upstream TransactionStore.java> <output TransactionStore.java>

Adds:
  [cvm-leftover]     every transaction init() recovers, with its status.
  [cvm-leftover-rec] that transaction's undo-log records — map name + key. This
                     is what shows whether the leftover holds a low `table.0`
                     meta key (rewritten on reopen -> livelock) or a harmless one.
  [cvm-endleftover]  what endLeftoverTransactions() decides for each.
"""
import io
import sys

src, dst = sys.argv[1], sys.argv[2]
s = io.open(src, encoding="utf-8").read()

a1 = "                                    leftoverTransactions.add(transaction);\n"
if a1 not in s:
    raise SystemExit("leftover anchor not found — upstream TransactionStore.java changed")
s = s.replace(a1, """                                    System.out.println("[cvm-leftover] map=" + mapName
                                            + " txId=" + transactionId + " committed=" + committed
                                            + " status=" + status + " logId=" + logId
                                            + " commitOrder=" + commitOrder);
                                    try {
                                        for (java.util.Iterator<Long> __it = undoLog.keyIterator(null);
                                                __it.hasNext(); ) {
                                            Long __k = __it.next();
                                            Record<?, ?> __r = undoLog.get(__k);
                                            System.out.println("[cvm-leftover-rec] txId=" + transactionId
                                                    + " undoKey=" + __k + " logId=" + getLogId(__k)
                                                    + " mapId=" + (__r == null ? "null" : __r.mapId)
                                                    + " mapName=" + (__r == null || __r.mapId < 0 ? "-"
                                                            : store.getMapName(__r.mapId))
                                                    + " key=" + (__r == null ? "null" : __r.key));
                                        }
                                    } catch (Throwable __t) {
                                        System.out.println("[cvm-leftover-rec] dump failed: " + __t);
                                    }
""" + a1, 1)

a2 = """            int status = t.getStatus();
            if (status == Transaction.STATUS_COMMITTED) {
"""
if a2 not in s:
    raise SystemExit("endLeftoverTransactions anchor not found — upstream changed")
s = s.replace(a2, """            int status = t.getStatus();
            System.out.println("[cvm-endleftover] txId=" + t.transactionId + " status=" + status);
            if (status == Transaction.STATUS_COMMITTED) {
""", 1)

io.open(dst, "w", encoding="utf-8", newline="\n").write(s)
print("wrote", dst)
