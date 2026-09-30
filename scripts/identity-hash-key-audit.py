#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
"""Fail on a new `identity_hash_code(` call in the native crates.

WHY THIS EXISTS
---------------

An identity hash is 31 bits, and since the 8-byte object header a compact
instance's is only 20 bits of a per-heap counter plus 11 bits derived from
its class, so two live instances of ONE class share one every 2^20 mints.
Every heap numbers its hashes from the same seed, so two VMs in one process
collide on their first objects too. A native side table keyed by `(VM, identity hash)` -- or, worse,
by the bare hash -- then serves one object's row to the other, overwrites it,
or drops it. Waves 23 to 31 of the gc-common round (2026-09-23) re-keyed about
forty such tables (`common-w28b-remaining-identity-hash-keyed-side-tables`,
now FIXED under `docs/internal/gc-common-round-20260923/`):

  * R1: the object's weak lock key (`crate::gc_stable_weak_lock_key` /
    `crate::existing_weak_lock_key`) plus a `forget_*_keys` hook in
    `native-builtins/src/lib.rs::{sweep_lock_keys, forget_vm_lock_keys}`;
  * R2: the hash as a BUCKET whose every row carries its owner (current
    address, a JNI weak global, or the object in a rooted row) and is
    compared on lookup (`native-io`, `native-awt`, `native-collections`
    cannot reach the lock-key registry).

Every call that remains is one of the census page's exclusions: a Java-visible
`hashCode()` / `identityHashCode()` answer, a hash-bucketed table whose rows
compare their object, a seed, debug output, lock striping, or a memo validated
by something else. Each is listed below, by FILE and ENCLOSING FUNCTION (never
by line number), with the number of calls that function may make and the
reason. A new call anywhere else fails the job; so does a listed function that
makes MORE calls than recorded, or fewer (the list is exact, so it cannot rot
into a blanket pass). A site that is fixed is removed from the list in the
same commit.

WHAT IT DOES NOT SEE
--------------------

  * A wrapper: a helper that calls `identity_hash_code` once is one listed
    site, and its callers are invisible (`io_side_key`, `obj_key` in
    `native-awt`, `map_hash_key`). Review a new caller of such a helper by hand.
  * `NativeContext::vm_identity_hash` and other spellings of the same hash.
  * Test code (`#[cfg(test)]` items and test-only files) is not scanned.

Usage:
    scripts/identity-hash-key-audit.py             # scan; exit 1 on a change
    scripts/identity-hash-key-audit.py --list      # print every site with its fn
    scripts/identity-hash-key-audit.py --selftest  # no tree needed

Exit: 0 ok, 1 the population changed, 3 the gate is broken.
"""
import argparse
import collections
import importlib.util
import io
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
_spec = importlib.util.spec_from_file_location(
    "stale_handle_audit", os.path.join(HERE, "stale-handle-across-alloc-audit.py"))
audit = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(audit)

CRATES = ["native-builtins", "native-io", "native-awt", "native-collections"]

# A call, through any receiver (`ctx.`, `scope.`, `self.inner.`); not the
# trait method's own definition (`fn identity_hash_code(`).
CALL = re.compile(r"\.\s*identity_hash_code\s*\(")

# Categories (the census page's exclusions):
#   JAVA    a Java-visible hashCode() / identityHashCode() / toString() answer
#   BUCKET  a hash-bucketed table whose every row compares its object/owner
#   SEED    a hash used only as a seed or salt; no row is keyed by it
#   STRIPE  lock striping / sharding; a collision only shares a lock
#   MEMO    a memo validated by something else (a token, a pattern compare)
#   DEBUG   diagnostic output only
#   WRAP    a helper that returns the hash; its callers are reviewed as a set
#   PROBE   only tests the hash for 0 ("not a heap object"); keys by lock key
# Two categories may be joined with "+" when one function has both kinds.
#
# The classification behind every row is the final STATUS block of
# docs/internal/gc-common-round-20260923/common-w28b-remaining-identity-hash-keyed-side-tables-FIXED-20260923.md
#
# "path::function": (calls, category, reason)
_NB = "native-builtins/src/"
_NC = "native-collections/src/"
_NI = "native-io/src/"
_NA = "native-awt/src/"
ALLOWED = {
    # ---- native-awt ------------------------------------------------------
    _NA + "natives.rs::lookup_peer_source": (1, "MEMO", "re-checks a row already found by peer id and resolved through its global root"),
    _NA + "natives.rs::obj_key": (1, "WRAP", "(vm, hash) bucket of KeyedRows; every row carries a weak root compared on lookup (w24-b)"),
    _NA + "natives.rs::register_event_natives": (2, "BUCKET", "invocation-event bucket; rows compare the event's weak root (w29-c)"),
    # ---- native-builtins -------------------------------------------------
    _NB + "antlr_intrinsics.rs::antlr_object_hash": (1, "JAVA", "Object.hashCode fallback of an ANTLR hash"),
    _NB + "antlr_intrinsics.rs::antlr_semantic_context_hash": (3, "JAVA", "SemanticContext.hashCode (class mirror seed, identity fallback)"),
    _NB + "antlr_intrinsics.rs::antlr_semantic_sort_operands": (1, "JAVA", "last tie-break of an operand sort, as the Java comparator"),
    _NB + "classloader.rs::ucl_is_closed": (1, "BUCKET", "closed-loader bucket compares the loader address (w29-e)"),
    _NB + "classloader.rs::ucl_mark_closed": (1, "BUCKET", "closed-loader bucket compares the loader address (w29-e)"),
    _NB + "classloader.rs::ucl_mark_open": (1, "BUCKET", "closed-loader bucket compares the loader address (w29-e)"),
    _NB + "date_format_fast.rs::register_date_format_fast": (1, "MEMO", "plan row filed under (vm, formatter hash); answers only when its stamp names the pattern and symbols objects by address + collection epoch, else re-proven by content (w34-a)"),
    _NB + "http_url_connection.rs::register_https_delegate_forwarders": (1, "JAVA", "HttpsURLConnection.hashCode"),
    _NB + "intrinsics/record.rs::component_hash": (1, "JAVA", "Enum component of Record.hashCode"),
    _NB + "jca/message_digest.rs::acc_keys": (1, "BUCKET", "accumulator bucket; rows compare the weak lock key"),
    _NB + "jdk25_concurrency.rs::sts_find_row": (1, "BUCKET", "STS bucket; rows compare the resolved global root"),
    _NB + "jdk25_concurrency.rs::sts_row_for": (1, "BUCKET", "STS bucket; rows compare the resolved global root"),
    _NB + "jmx.rs::alloc_jmx_lock_info": (1, "JAVA", "LockInfo.identityHashCode"),
    _NB + "jmx.rs::alloc_jmx_monitor_info": (1, "JAVA", "MonitorInfo.identityHashCode"),
    _NB + "jmx.rs::jmx_lock_name": (1, "JAVA", "LockInfo.toString text"),
    _NB + "keystore.rs::store_row_key": (1, "PROBE", "non-heap-object check; rows keyed by weak lock key (w27-b)"),
    _NB + "lang_class.rs::ctx_annotation_value_hash": (1, "JAVA", "annotation member hashCode (Class / Enum identity)"),
    _NB + "lang_invoke.rs::string_concat_render_value": (1, "JAVA", "Object.toString text in string concat"),
    _NB + "lang_invoke.rs::vh_memo_lookup": (1, "MEMO", "thread-local line validated by address and a moving-collection generation (w29-b)"),
    _NB + "lang_misc.rs::capture_throwable_trace_body": (1, "DEBUG", "CRATONVM dbg_sttrace output"),
    _NB + "lang_misc.rs::native_enum_hash_code": (1, "JAVA", "Enum.hashCode"),
    _NB + "lang_misc.rs::native_throwable_get_stack_trace_array": (1, "DEBUG", "dbg_sttrace output"),
    _NB + "lang_misc.rs::native_throwable_init_cause": (1, "DEBUG", "CAUSE_DBG output"),
    _NB + "lang_misc.rs::register_phase53_record": (1, "JAVA", "Record.hashCode component fallback"),
    _NB + "lang_misc.rs::throwable_cause": (2, "DEBUG", "CAUSE_DBG output"),
    _NB + "lang_misc.rs::write_throwable_cause": (3, "DEBUG", "CAUSE_DBG output"),
    _NB + "lang_reflect.rs::register_wp2_1_natives": (1, "JAVA", "TypeVariable.hashCode (declaration identity)"),
    _NB + "lang_string.rs::invoke_to_string_units_opt": (1, "JAVA", "Object.toString text"),
    _NB + "lang_system.rs::native_system_identity_hash_code": (1, "JAVA", "System.identityHashCode"),
    _NB + "lib.rs::class_atomic_key": (1, "MEMO", "slot validated by ClassId; colliders only thrash (rank 38)"),
    _NB + "lib.rs::existing_weak_lock_key": (1, "BUCKET", "the lock-key registry: slots compare VM and address (w24-a/w25-a)"),
    _NB + "lib.rs::lock_key_for": (1, "BUCKET", "the lock-key registry: slots compare VM and address (w24-a/w25-a)"),
    _NB + "lib.rs::native_mapper_internal_map": (1, "BUCKET", "Mapper memo bucket; the row compares the Mapper, hosts, defaultHost and version by address within its gc_collection_count epoch, and the host/URI text (w34-b)"),
    _NB + "lib.rs::native_object_hash_code": (1, "JAVA", "Object.hashCode"),
    _NB + "lib.rs::native_object_to_string": (1, "JAVA", "Object.toString text"),
    _NB + "lib.rs::native_objects_value_hash_code": (1, "JAVA", "Objects.hashCode identity arm"),
    _NB + "lib.rs::register_essential_natives_with_shims": (2, "JAVA+DEBUG", "Object.toString fallback; trace_arrays_hashcode output"),
    _NB + "logging_shims.rs::slf4j_render_arg": (1, "JAVA", "Object.toString text of a log argument"),
    _NB + "logmanager.rs::dump_throwable_to_stderr": (1, "DEBUG", "cycle guard of a stderr cause-chain dump"),
    _NB + "lucene_es.rs::randomized_context_for_thread": (2, "BUCKET", "row answers only its thread's address (w31-b)"),
    _NB + "lucene_es.rs::randomized_per_thread_key": (3, "BUCKET", "row answers only its context and thread addresses (w31-b)"),
    _NB + "lucene_es.rs::randomized_random_cache_key_for_context": (2, "BUCKET", "row answers only its context and thread addresses (w31-b)"),
    _NB + "lucene_es.rs::randomized_resolve_root": (1, "MEMO", "re-checks the value a global root resolves to"),
    _NB + "lucene_es.rs::randomized_root_entry": (1, "MEMO", "records the value's hash for randomized_resolve_root"),
    _NB + "messaging_shims.rs::message_bytes_to_chars_with": (1, "MEMO", "row keyed by weak lock key; String hash AND address AND collection count validate it (w33-a)"),
    _NB + "messaging_shims.rs::native_netty_mpsc_offer": (1, "DEBUG", "NETTYQ output"),
    _NB + "messaging_shims.rs::native_netty_mpsc_poll": (1, "DEBUG", "NETTYQ output"),
    _NB + "net_phase_e.rs::re2_bind_listener": (1, "BUCKET", "server_socket_ports rows compare the object"),
    _NB + "net_phase_e.rs::re2_server_socket_close": (1, "BUCKET", "server_socket_ports rows compare the object"),
    _NB + "net_phase_e.rs::register_re5_http_client": (1, "JAVA", "HttpHeaders.toString text"),
    _NB + "net_phase_e.rs::register_re6_ssl_context": (2, "DEBUG", "dbg_tls_auth_ok output"),
    _NB + "phases_early.rs::ihm_hash_code_inner": (2, "JAVA", "IdentityHashMap.hashCode"),
    _NB + "phases_early.rs::lookup_index": (1, "MEMO", "Properties index row keyed by weak lock key; array hash plus size validate it"),
    _NB + "phases_early.rs::note_append": (1, "MEMO", "as lookup_index"),
    _NB + "phases_early.rs::object_array_element_hash_code": (1, "JAVA", "array element hashCode (arrays are identity)"),
    _NB + "phases_early.rs::ph_bad_arrive_message": (1, "JAVA", "Phaser.toString text in an exception message"),
    _NB + "phases_early.rs::tl_bucket_hash": (1, "BUCKET", "TL_MAP bucket; rows compare the ThreadLocal's weak root (w29-a)"),
    _NB + "phases_late.rs::classvalue_key": (1, "BUCKET", "Class half is a bucket whose entries compare the mirror address (w29-e)"),
    _NB + "phases_late.rs::existing_classvalue_key": (1, "BUCKET", "as classvalue_key"),
    _NB + "phases_late/concurrent.rs::p58_sq_offer": (1, "BUCKET", "SQ_TABLE bucket; rows compare the resolved global root"),
    _NB + "phases_late/concurrent.rs::p58_sq_offer_timed": (1, "BUCKET", "SQ_TABLE bucket; rows compare the resolved global root"),
    _NB + "phases_late/concurrent.rs::p58_sq_poll": (1, "BUCKET", "SQ_TABLE bucket; rows compare the resolved global root"),
    _NB + "phases_late/concurrent.rs::p58_sq_poll_timed": (1, "BUCKET", "SQ_TABLE bucket; rows compare the resolved global root"),
    _NB + "phases_late/concurrent.rs::p58_sq_put": (1, "BUCKET", "SQ_TABLE bucket; rows compare the resolved global root"),
    _NB + "phases_late/concurrent.rs::p58_sq_take": (1, "BUCKET", "SQ_TABLE bucket; rows compare the resolved global root"),
    _NB + "phases_late/concurrent.rs::register_p69_submission_publisher": (1, "BUCKET", "SP_CLOSED_EXCEPTIONS bucket; rows compare the owner address (w29-a)"),
    _NB + "phases_late/concurrent.rs::sp_closed_exception_root": (1, "BUCKET", "SP_CLOSED_EXCEPTIONS bucket; rows compare the owner address (w29-a)"),
    _NB + "phases_late/foreign_ffm.rs::register_p67_segment_surface": (1, "JAVA", "MemorySegment.hashCode (heap base identity)"),
    _NB + "phases_late/ssl_security.rs::register_p68_ssl": (4, "DEBUG", "dbg_tls_auth output"),
    _NB + "reflect_annotations.rs::register_module_builder_overrides": (1, "JAVA", "ModuleDescriptor stand-ins' identity hashCode (compareTo orders by name, then lock key or real bytecode, w34-a)"),
    _NB + "regex_matcher.rs::matcher_cache_key": (1, "BUCKET", "row answers only its Matcher's address (w31-b)"),
    _NB + "regex_matcher.rs::matcher_read_input_cached": (1, "MEMO", "input validator: hash AND address of the input String (w31-b)"),
    _NB + "regex_matcher.rs::matcher_realjdk_cached": (2, "MEMO", "row keyed by weak lock key (w29-d); hashes compared, but the row answers only when its stamp names the text / pattern objects (address in-epoch, else unchanged modCount, w34-a)"),
    _NB + "regex_matcher.rs::matcher_realjdk_cached_group_string": (1, "MEMO", "as matcher_realjdk_cached; answers only the text String's own address in the row's epoch (w34-a)"),
    _NB + "test_frameworks.rs::bytebuddy_object_hash": (1, "JAVA", "Object.hashCode fallback"),
    _NB + "test_frameworks.rs::native_assertj_standard_comparison_are_equal": (1, "DEBUG", "dbg_assertj_arr output"),
    _NB + "util_concurrent_ext.rs::native_cdl_to_string": (1, "JAVA", "CountDownLatch.toString text"),
    _NB + "util_concurrent_ext.rs::native_rl_to_string": (1, "JAVA", "ReentrantLock.toString text"),
    _NB + "util_concurrent_ext.rs::native_sem_to_string": (1, "JAVA", "Semaphore.toString text"),
    _NB + "x509_manager.rs::manager_id_row_key": (1, "PROBE", "non-heap-object check; rows keyed by weak lock key (w27-b)"),
    # ---- native-collections ----------------------------------------------
    _NC + "identity_hash.rs::obj_key": (1, "WRAP", "no production caller (tests/gc_relocation_harness.rs only)"),
    _NC + "identity_hash.rs::seed": (1, "SEED", "primes nothing any more; result discarded"),
    _NC + "lib.rs::acquire": (1, "STRIPE", "CHM segment stripe epochs"),
    _NC + "lib.rs::acquire_gc_safe": (2, "STRIPE", "CHM segment stripe lock and epochs"),
    _NC + "lib.rs::chm_seg_get": (1, "STRIPE", "CHM segment stripe lock"),
    _NC + "lib.rs::chm_seg_get_wrapper_key_in_region": (1, "STRIPE", "CHM segment stripe epoch"),
    _NC + "lib.rs::cslm_stripe_for": (1, "STRIPE", "ConcurrentSkipListMap lock stripe"),
    _NC + "lib.rs::element_hash_code": (1, "JAVA", "element hashCode identity arm"),
    _NC + "lib.rs::hm_int_fast_obj_key": (1, "MEMO", "thread-local memo validated by VM, address and hash"),
    _NC + "lib.rs::make_hashset_with_elements": (1, "JAVA", "identity fallback when an element's hashCode throws"),
    _NC + "lib.rs::make_static_entry_set": (1, "BUCKET", "identity bucket of a native map; nodes compare the entry"),
    _NC + "lib.rs::map_hash_key": (2, "JAVA", "map key hashCode identity arms"),
    _NC + "lib.rs::map_hash_key_identity": (1, "JAVA", "IdentityHashMap key hash; nodes compare the key"),
    _NC + "lib.rs::native_al_hash_code": (1, "JAVA", "AbstractList.hashCode identity arm"),
    _NC + "lib.rs::native_chm_entry_set": (1, "BUCKET", "identity bucket of a native map; nodes compare the entry"),
    _NC + "lib.rs::native_chm_get_string_fast": (1, "STRIPE", "CHM segment stripe epochs"),
    _NC + "lib.rs::native_lhm_entry_set": (1, "BUCKET", "identity bucket of a native map; nodes compare the entry"),
    _NC + "lib.rs::native_map_entry_set": (1, "BUCKET", "identity bucket of a native map; nodes compare the entry"),
    _NC + "lib.rs::obj_to_display_units": (1, "JAVA", "Object.toString text"),
    _NC + "lib.rs::reentry_guard_key": (1, "WRAP", "test hook only since w26-a (guards match a pin's address)"),
    _NC + "lib.rs::resync_view_set_inner": (1, "BUCKET", "identity bucket of a native map; nodes compare the entry"),
    _NC + "lib.rs::view_element_fingerprints": (1, "DEBUG", "view-cache verification output"),
    _NC + "lib.rs::widened_obj_key": (1, "BUCKET", "overlay key registry: slots compare VM and address (w26-a)"),
    # ---- native-io -------------------------------------------------------
    _NI + "lib.rs::io_side_key": (1, "WRAP", "completed by the owner address into IoRowKey (w29-c); dc_socket_cache rows compare the resolved root"),
    _NI + "nio_selector.rs::channel_key_for_native": (1, "BUCKET", "sk_table bucket; rows compare the key object"),
    _NI + "nio_selector.rs::channel_register_native": (1, "BUCKET", "sk_table bucket; rows compare the key object"),
    _NI + "nio_selector.rs::refresh_selector_handles": (1, "BUCKET", "sk_table bucket; rows compare the key object"),
    _NI + "nio_selector.rs::selector_close_native": (1, "BUCKET", "sel_obj_ids bucket; rows compare the selector object"),
    _NI + "nio_selector.rs::selector_id_from_obj": (1, "BUCKET", "sel_obj_ids bucket; rows compare the selector object"),
    _NI + "nio_selector.rs::selector_open_native": (1, "BUCKET", "sel_obj_ids bucket; rows compare the selector object"),
    _NI + "nio_selector.rs::sk_state_get_field": (1, "BUCKET", "sk_table bucket; rows compare the key object"),
    _NI + "nio_selector.rs::sk_state_with_mut": (1, "BUCKET", "sk_table bucket; rows compare the key object"),
    _NI + "socket_channel.rs::cf_clear": (1, "BUCKET", "chan_fields bucket; rows compare the channel object"),
    _NI + "socket_channel.rs::cf_get": (1, "BUCKET", "chan_fields bucket; rows compare the channel object"),
    _NI + "socket_channel.rs::cf_get_str": (1, "BUCKET", "chan_fields bucket; rows compare the channel object"),
    _NI + "socket_channel.rs::cf_opt_get": (1, "BUCKET", "chan_fields bucket; rows compare the channel object"),
    _NI + "socket_channel.rs::cf_opt_set": (1, "BUCKET", "chan_fields bucket; rows compare the channel object"),
    _NI + "socket_channel.rs::cf_remote": (1, "BUCKET", "chan_fields bucket; rows compare the channel object"),
    _NI + "socket_channel.rs::cf_set": (1, "BUCKET", "chan_fields bucket; rows compare the channel object"),
    _NI + "socket_channel.rs::ss_back_ref": (1, "BUCKET", "ss_back_ref bucket; rows compare the wrapper object"),
    _NI + "socket_channel.rs::ss_record_back_ref": (1, "BUCKET", "ss_back_ref bucket; rows compare the wrapper object"),
    _NI + "socket_channel.rs::ss_remove_back_ref": (1, "BUCKET", "ss_back_ref bucket; rows compare the wrapper object"),
    _NI + "socket_channel.rs::ss_wrapper_is_bound": (1, "BUCKET", "server_socket_ports rows compare the object"),
    _NI + "socket_channel.rs::ss_wrapper_local_address": (1, "BUCKET", "server_socket_ports rows compare the object"),
    _NI + "socket_channel.rs::ss_wrapper_local_port": (1, "BUCKET", "server_socket_ports rows compare the object"),
    _NI + "socket_channel.rs::ssc_socket_cache_clear": (1, "BUCKET", "ssc_socket_cache bucket; rows compare the channel object"),
    _NI + "socket_channel.rs::ssc_socket_cache_get": (1, "BUCKET", "ssc_socket_cache bucket; rows compare the channel object"),
    _NI + "socket_channel.rs::ssc_socket_cache_put": (1, "BUCKET", "ssc_socket_cache bucket; rows compare the channel object"),
}

CATEGORIES = ("JAVA", "BUCKET", "SEED", "STRIPE", "MEMO", "DEBUG", "WRAP", "PROBE")


def enclosing_functions(lines):
    """For each 0-based line index, the innermost enclosing `fn` name (or None)."""
    out = [None] * len(lines)
    stack = []            # (name, depth at which its body opened)
    pending = None        # (name, depth before the signature)
    depth = 0
    nest = 0              # ( and [ depth inside a pending signature
    for i, raw in enumerate(lines):
        code = audit.strip_code(raw)
        m = audit.FNDEF.match(raw)
        if m:
            pending = (m.group(2), depth)
            nest = 0
        cur = stack[-1][0] if stack else None
        if pending is not None:
            cur = pending[0]
        out[i] = cur
        for ch in code:
            if ch == "{":
                if pending is not None:
                    stack.append((pending[0], depth))
                    pending = None
                depth += 1
            elif ch == "}":
                depth -= 1
                if stack and depth <= stack[-1][1]:
                    stack.pop()
            elif ch in "([":
                nest += 1
            elif ch in ")]":
                nest -= 1
            elif ch == ";" and pending is not None and depth <= pending[1] and nest <= 0:
                pending = None   # a body-less trait declaration (not `[T; N]`)
    return out


def scan_lines(lines, path="<mem>"):
    tests = audit.cfg_test_lines(lines)
    fns = enclosing_functions(lines)
    hits = []
    for i, raw in enumerate(lines):
        if (i + 1) in tests:
            continue
        code = audit.strip_code(raw)
        for _ in CALL.finditer(code):
            hits.append((path, i + 1, fns[i] or "<module>", raw.strip()))
    return hits


def scan_tree(root):
    files = []
    for crate in CRATES:
        base = os.path.join(root, crate, "src")
        for dirpath, _, names in os.walk(base):
            for f in sorted(names):
                if f.endswith(".rs"):
                    p = os.path.join(dirpath, f)
                    files.append((p, io.open(p, encoding="utf-8",
                                             errors="replace").read().splitlines()))
    excluded = set()
    for p, lines in files:
        excluded.update(audit.test_only_files(p, lines))

    def is_test_file(p):
        q = os.path.normcase(os.path.normpath(p))
        return q in excluded or any(q.startswith(e + os.sep) for e in excluded)

    hits = []
    for p, lines in files:
        if is_test_file(p):
            continue
        rel = os.path.relpath(p, root).replace("\\", "/")
        hits += scan_lines(lines, rel)
    return hits


def judge(hits, allowed):
    counts = collections.Counter("%s::%s" % (p, f) for p, _, f, _ in hits)
    problems = []
    for key, n in sorted(counts.items()):
        if key not in allowed:
            problems.append("  NEW  %s: %d call(s), not on the allow-list" % (key, n))
        elif n != allowed[key][0]:
            problems.append("  %s  %s: %d call(s), the allow-list says %d"
                            % ("MORE" if n > allowed[key][0] else "LESS",
                               key, n, allowed[key][0]))
    for key in sorted(set(allowed) - set(counts)):
        problems.append("  GONE %s: listed, but it makes no call now" % key)
    return problems


def selftest():
    src = """
fn java_hash(ctx: &mut dyn NativeContext, o: ObjectRef) -> i32 {
    ctx.identity_hash_code(o)
}
trait T {
    fn identity_hash_code(&self, o: ObjectRef) -> i32;
}
fn table_key(ctx: &mut dyn NativeContext, o: [ObjectRef; 2]) -> (u64, i32) {
    let s = "ctx.identity_hash_code(o)"; // ctx.identity_hash_code(o)
    let inner = |x: ObjectRef| {
        fn nested(c: &dyn NativeContext, y: ObjectRef) -> i32 { c.identity_hash_code(y) }
        nested(ctx, x)
    };
    (ctx.vm_identity(), scope.identity_hash_code(o))
}
#[cfg(test)]
mod tests {
    fn t(c: &Mock, o: ObjectRef) { c.identity_hash_code(o); }
}
""".splitlines()
    hits = scan_lines(src, "x.rs")
    got = sorted((f, l) for _, l, f, _ in hits)
    want = [("java_hash", 3), ("nested", 11), ("table_key", 14)]
    broken = 0
    if got != want:
        print("  selftest: scan found %r, expected %r" % (got, want))
        broken += 1
    allowed = {"x.rs::java_hash": (1, "JAVA", ""), "x.rs::nested": (1, "SEED", ""),
               "x.rs::gone": (1, "SEED", "")}
    problems = judge(hits, allowed)
    kinds = sorted(p.split()[0] for p in problems)
    if kinds != ["GONE", "NEW"]:
        print("  selftest: judge said %r" % (problems,))
        broken += 1
    for key, (n, cat, why) in ALLOWED.items():
        if any(c not in CATEGORIES for c in cat.split("+")) \
                or not why or n < 1 or "::" not in key:
            print("  selftest: malformed allow-list row %r" % key)
            broken += 1
    print("  selftest: %s" % ("BROKEN" if broken else "ok"))
    return 3 if broken else 0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", default=os.path.dirname(HERE))
    ap.add_argument("--selftest", action="store_true")
    ap.add_argument("--list", action="store_true")
    a = ap.parse_args()
    rc = selftest()
    if a.selftest or rc:
        return rc
    hits = scan_tree(a.root)
    if a.list:
        for p, ln, f, text in hits:
            print("%s:%d\t%s\t%s" % (p, ln, f, text))
        return 0
    problems = judge(hits, ALLOWED)
    if problems:
        print("\n".join(problems))
        print("  An identity hash is not a key: two live objects of one VM can share"
              " one. Key a per-object row by the weak lock key (R1) or compare the"
              " owner in a hash bucket (R2); see the header of this script. A"
              " reviewed exclusion goes in ALLOWED with its reason.")
        return 1
    print("  ok -- %d identity_hash_code call(s) in %d reviewed function(s)"
          % (len(hits), len(ALLOWED)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
