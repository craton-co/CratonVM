/* tsdspy.c -- name every pthread thread-specific-data destructor registered in
 * a run, and the library that registered it.
 *
 * `jni_detach_current_thread` was reached from `__nptl_deallocate_tsd` with the
 * intervening frame unresolved (`<unknown>`), so the question "who registers
 * the key whose destructor ends in DetachCurrentThread" cannot be answered from
 * the backtrace. This answers it from the other end: interpose
 * pthread_key_create, and for every key that carries a destructor print the
 * destructor's symbol and owning object plus the caller's owning object.
 *
 *   gcc -shared -fPIC -o tsdspy.so tsdspy.c -ldl
 *   LD_PRELOAD=./tsdspy.so <cmd>
 */
#define _GNU_SOURCE
#include <pthread.h>
#include <dlfcn.h>
#include <stdio.h>

static int (*real_key_create)(pthread_key_t *, void (*)(void *));

int pthread_key_create(pthread_key_t *key, void (*dtor)(void *)) {
    if (!real_key_create) {
        real_key_create =
            (int (*)(pthread_key_t *, void (*)(void *)))dlsym(RTLD_NEXT, "pthread_key_create");
    }
    int rc = real_key_create(key, dtor);
    if (dtor) {
        Dl_info dd, dc;
        void *caller = __builtin_return_address(0);
        const char *dfile = "?", *dname = "?", *cfile = "?", *cname = "?";
        if (dladdr((void *)dtor, &dd)) {
            if (dd.dli_fname) dfile = dd.dli_fname;
            if (dd.dli_sname) dname = dd.dli_sname;
        }
        if (dladdr(caller, &dc)) {
            if (dc.dli_fname) cfile = dc.dli_fname;
            if (dc.dli_sname) cname = dc.dli_sname;
        }
        fprintf(stderr, "[TSDKEY] dtor=%s@%s caller=%s@%s\n", dname, dfile, cname, cfile);
        fflush(stderr);
    }
    return rc;
}
