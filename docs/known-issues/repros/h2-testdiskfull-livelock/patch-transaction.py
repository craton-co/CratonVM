#!/usr/bin/env python3
"""Patch org.h2.mvstore.tx.Transaction with the livelock diagnostics.

Usage: patch-transaction.py <upstream Transaction.java> <output Transaction.java>

Adds:
  [cvm-spin]         every 20000 waitFor() calls: who is blocking, its status,
                     whether its slot is still occupied, its committing bit.
  [cvm-commit-exit]  every commit(), with wasActive/hasChanges. `wasActive=false`
                     is the recovered-COMMITTED-leftover case that never closes.
  [cvm-commit-throw] a Throwable escaping commit()'s body (silently swallowed
                     upstream when wasActive is false).
"""
import io
import sys

src, dst = sys.argv[1], sys.argv[2]
s = io.open(src, encoding="utf-8").read()

anchor = "    public boolean waitFor(Transaction toWaitFor, String mapName, Object key, int timeoutMillis) {\n"
if anchor not in s:
    raise SystemExit("waitFor anchor not found — upstream Transaction.java changed")
s = s.replace(anchor, anchor + """        if ((++__cvmWaitCount % 20000) == 0) {
            Transaction slot = store.getTransaction(toWaitFor.transactionId);
            System.out.println("[cvm-spin] n=" + __cvmWaitCount
                + " me=" + transactionId + " meStatus=" + getStatus()
                + " blocking=" + toWaitFor.transactionId
                + " blockingStatus=" + toWaitFor.getStatus()
                + " slotIsSame=" + (slot == toWaitFor) + " slotNull=" + (slot == null)
                + " committingBit=" + store.committingTransactions.get().get(toWaitFor.transactionId)
                + " map=" + mapName + " key=" + key);
        }
""", 1)
s = s.replace("    public boolean waitFor(",
              "    private int __cvmWaitCount;\n\n    public boolean waitFor(", 1)

old_commit = """        } catch (Throwable e) {
            if (wasActive) {
                ex = e;
                throw e;
            }
        } finally {
            if (wasActive) {
                close(hasChanges, ex);
            }
        }
    }
"""
if old_commit not in s:
    raise SystemExit("commit() anchor not found — upstream Transaction.java changed")
s = s.replace(old_commit, """        } catch (Throwable e) {
            System.out.println("[cvm-commit-throw] tx=" + transactionId + " status=" + getStatus()
                + " wasActive=" + wasActive + " hasChanges=" + hasChanges + " ex=" + e);
            e.printStackTrace(System.out);
            if (wasActive) {
                ex = e;
                throw e;
            }
        } finally {
            System.out.println("[cvm-commit-exit] tx=" + transactionId + " status=" + getStatus()
                + " wasActive=" + wasActive + " hasChanges=" + hasChanges);
            if (wasActive) {
                close(hasChanges, ex);
            }
        }
    }
""", 1)

io.open(dst, "w", encoding="utf-8", newline="\n").write(s)
print("wrote", dst)
