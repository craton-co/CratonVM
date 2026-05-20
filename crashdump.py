import ctypes, ctypes.wintypes as wt, sys, struct

k32 = ctypes.WinDLL('kernel32', use_last_error=True)
dbghelp = ctypes.WinDLL('dbghelp')

DEBUG_ONLY_THIS_PROCESS = 0x00000002
EXCEPTION_DEBUG_EVENT = 1
EXIT_PROCESS_DEBUG_EVENT = 5
DBG_EXCEPTION_NOT_HANDLED = 0x80010001
DBG_CONTINUE = 0x00010002
EXCEPTION_ACCESS_VIOLATION = 0xC0000005
INFINITE = 0xFFFFFFFF

class STARTUPINFO(ctypes.Structure):
    _fields_ = [("cb", wt.DWORD),("lpReserved", wt.LPWSTR),("lpDesktop", wt.LPWSTR),
        ("lpTitle", wt.LPWSTR),("dwX", wt.DWORD),("dwY", wt.DWORD),("dwXSize", wt.DWORD),
        ("dwYSize", wt.DWORD),("dwXCountChars", wt.DWORD),("dwYCountChars", wt.DWORD),
        ("dwFillAttribute", wt.DWORD),("dwFlags", wt.DWORD),("wShowWindow", wt.WORD),
        ("cbReserved2", wt.WORD),("lpReserved2", ctypes.c_void_p),
        ("hStdInput", wt.HANDLE),("hStdOutput", wt.HANDLE),("hStdError", wt.HANDLE)]

class PROCESS_INFORMATION(ctypes.Structure):
    _fields_ = [("hProcess", wt.HANDLE),("hThread", wt.HANDLE),
        ("dwProcessId", wt.DWORD),("dwThreadId", wt.DWORD)]

class EXCEPTION_RECORD(ctypes.Structure):
    pass
EXCEPTION_RECORD._fields_ = [
    ("ExceptionCode", wt.DWORD),("ExceptionFlags", wt.DWORD),
    ("ExceptionRecord", ctypes.POINTER(EXCEPTION_RECORD)),
    ("ExceptionAddress", ctypes.c_void_p),("NumberParameters", wt.DWORD),
    ("ExceptionInformation", ctypes.c_ulonglong * 15)]

class EXCEPTION_DEBUG_INFO(ctypes.Structure):
    _fields_ = [("ExceptionRecord", EXCEPTION_RECORD),("dwFirstChance", wt.DWORD)]

class DEBUG_EVENT(ctypes.Structure):
    # u is a union of event structs; it is 8-byte aligned (contains pointers),
    # so it starts at offset 16, not 12 — the explicit pad is required.
    _fields_ = [("dwDebugEventCode", wt.DWORD),("dwProcessId", wt.DWORD),
        ("dwThreadId", wt.DWORD),("_pad", wt.DWORD),("u", ctypes.c_byte * 256)]

class MINIDUMP_EXCEPTION_INFORMATION(ctypes.Structure):
    _fields_ = [("ThreadId", wt.DWORD),("ExceptionPointers", ctypes.c_void_p),
        ("ClientPointers", wt.BOOL)]

cmd = " ".join(f'"{a}"' if " " in a else a for a in sys.argv[1:])
dump_path = r'C:\craton\CratonVM\crash.dmp'

si = STARTUPINFO(); si.cb = ctypes.sizeof(si)
pi = PROCESS_INFORMATION()
ok = k32.CreateProcessW(None, ctypes.create_unicode_buffer(cmd), None, None, False,
                        DEBUG_ONLY_THIS_PROCESS, None, None, ctypes.byref(si), ctypes.byref(pi))
if not ok:
    print("CreateProcess failed", ctypes.get_last_error()); sys.exit(1)
print(f"launched pid={pi.dwProcessId}")

evt = DEBUG_EVENT()
av_count = 0
while True:
    if not k32.WaitForDebugEvent(ctypes.byref(evt), INFINITE):
        print("WaitForDebugEvent failed", ctypes.get_last_error()); break
    code = evt.dwDebugEventCode
    cont = DBG_CONTINUE
    if code == EXCEPTION_DEBUG_EVENT:
        edi = ctypes.cast(ctypes.byref(evt.u), ctypes.POINTER(EXCEPTION_DEBUG_INFO)).contents
        exc_code = edi.ExceptionRecord.ExceptionCode & 0xFFFFFFFF
        first = edi.dwFirstChance
        if exc_code == EXCEPTION_ACCESS_VIOLATION:
            av_count += 1
            addr = edi.ExceptionRecord.ExceptionAddress
            op = edi.ExceptionRecord.ExceptionInformation[0]
            fault = edi.ExceptionRecord.ExceptionInformation[1]
            print(f"ACCESS_VIOLATION #{av_count} firstChance={first} at {addr:#x} "
                  f"op={'write' if op==1 else 'read' if op==0 else op} faultaddr={fault:#x} tid={evt.dwThreadId}")
            # dump on the second-chance (fatal) AV, or first-chance if it repeats
            if not first or av_count >= 2:
                mei = MINIDUMP_EXCEPTION_INFORMATION()
                mei.ThreadId = evt.dwThreadId
                # ExceptionPointers must point to EXCEPTION_POINTERS in the
                # debuggee; we don't have that addr. Use ClientPointers=False
                # with a locally-built EXCEPTION_POINTERS is not possible
                # cross-process. Capture without exception info.
                hFile = k32.CreateFileW(dump_path, 0x40000000, 0, None, 2, 0x80, None)
                MiniDumpWithFullMemory = 2
                r = dbghelp.MiniDumpWriteDump(pi.hProcess, pi.dwProcessId, hFile,
                                              MiniDumpWithFullMemory, None, None, None)
                k32.CloseHandle(hFile)
                print(f"dump written ok={r} -> {dump_path}")
                k32.TerminateProcess(pi.hProcess, 1)
                cont = DBG_EXCEPTION_NOT_HANDLED
                k32.ContinueDebugEvent(evt.dwProcessId, evt.dwThreadId, cont)
                break
        cont = DBG_EXCEPTION_NOT_HANDLED
    elif code == EXIT_PROCESS_DEBUG_EVENT:
        print("process exited without AV dump")
        break
    k32.ContinueDebugEvent(evt.dwProcessId, evt.dwThreadId, cont)
