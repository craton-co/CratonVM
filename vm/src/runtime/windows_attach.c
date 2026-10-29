// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
// The JDK's injected thread has only one page of stack. It never enters Rust
// or Java: copy bounded fields into a preallocated transport queue, then return.
#include <windows.h>
#include <string.h>

typedef struct {
    int version;
    char command[17];
    char args[3][1025];
    char pipe[257];
} Request;
static CRITICAL_SECTION lock;
static Request queue[16];
static unsigned head, count;
static int active;

BOOL WINAPI DllMain(HINSTANCE module, DWORD reason, LPVOID reserved) {
    if (reason == DLL_PROCESS_ATTACH) {
        InitializeCriticalSection(&lock);
        DisableThreadLibraryCalls(module);
    }
    return TRUE;
}

// Process transport ownership, not VM policy. Exactly one VM owns the PID
// endpoint; a second embedded VM must never silently take over its requests.
__declspec(dllexport) int WINAPI CratonAttachClaim(void) {
    int result;
    EnterCriticalSection(&lock);
    result = !active;
    if (result) { active = 1; head = count = 0; }
    LeaveCriticalSection(&lock);
    return result;
}
__declspec(dllexport) void WINAPI CratonAttachRelease(void) {
    EnterCriticalSection(&lock);
    active = 0; head = count = 0;
    LeaveCriticalSection(&lock);
}
__declspec(dllexport) int WINAPI CratonAttachTake(Request *out) {
    int result = 0;
    EnterCriticalSection(&lock);
    if (active && count) {
        memcpy(out, &queue[head], sizeof(*out));
        head = (head + 1) % 16; --count; result = 1;
    }
    LeaveCriticalSection(&lock);
    return result;
}
static int fits(const char *s, unsigned capacity) {
    unsigned n;
    if (!s) return 1;
    for (n = 0; n < capacity; ++n) if (!s[n]) return 1;
    return 0;
}
static int enqueue(int version, const char *cmd, const char *a,
                   const char *b, const char *c, const char *pipe) {
    Request *r;
    int result = -1;
    // A local JDK pipe only; never open arbitrary paths supplied by a client.
    if (!pipe || strncmp(pipe, "\\\\.\\pipe\\javatool", 17) != 0 ||
        !fits(pipe, 257) || !fits(cmd, 17) || !fits(a, 1025) ||
        !fits(b, 1025) || !fits(c, 1025)) return -1;
    EnterCriticalSection(&lock);
    if (active && count < 16) {
        r = &queue[(head + count) % 16];
        r->version = version;
        strcpy(r->command, cmd ? cmd : "");
        strcpy(r->args[0], a ? a : "");
        strcpy(r->args[1], b ? b : "");
        strcpy(r->args[2], c ? c : "");
        strcpy(r->pipe, pipe);
        ++count; result = 0;
    }
    LeaveCriticalSection(&lock);
    return result;
}
__declspec(dllexport) int WINAPI JVM_EnqueueOperation(
    const char *cmd, const char *a, const char *b, const char *c, const char *pipe) {
    return enqueue(1, cmd, a, b, c, pipe);
}
__declspec(dllexport) int WINAPI JVM_EnqueueOperation_v2(const char *pipe) {
    return enqueue(2, "", "", "", "", pipe);
}
