"""Actual Windows worker reuse and OS memory qualification, without vector dumps."""
import ctypes
from ctypes import wintypes
import json
import os
from pathlib import Path
import subprocess
import queue
import threading

ROOT = Path(__file__).resolve().parents[1]


class Memory(ctypes.Structure):
    _fields_ = [('cb', wintypes.DWORD), ('PageFaultCount', wintypes.DWORD)] + [
        (name, ctypes.c_size_t) for name in ['PeakWorkingSetSize', 'WorkingSetSize',
        'QuotaPeakPagedPoolUsage', 'QuotaPagedPoolUsage', 'QuotaPeakNonPagedPoolUsage',
        'QuotaNonPagedPoolUsage', 'PagefileUsage', 'PeakPagefileUsage', 'PrivateUsage']]


def measure(pid):
    kernel = ctypes.WinDLL('kernel32', use_last_error=True)
    psapi = ctypes.WinDLL('psapi', use_last_error=True)
    kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    kernel.OpenProcess.restype = wintypes.HANDLE
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    psapi.GetProcessMemoryInfo.argtypes = [wintypes.HANDLE, ctypes.POINTER(Memory), wintypes.DWORD]
    handle = kernel.OpenProcess(0x410, False, pid)
    if not handle:
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        info = Memory()
        info.cb = ctypes.sizeof(Memory)
        if not psapi.GetProcessMemoryInfo(handle, ctypes.byref(info), info.cb):
            raise ctypes.WinError(ctypes.get_last_error())
        return {'peak_working_set_bytes': info.PeakWorkingSetSize,
                'working_set_bytes': info.WorkingSetSize, 'private_bytes': info.PrivateUsage}
    finally:
        kernel.CloseHandle(handle)


def main():
    if os.name != 'nt':
        raise RuntimeError('This OS memory qualification requires Windows')
    process = subprocess.Popen([str(ROOT/'target/debug/retrieval-embeddings.exe'), str(ROOT/'assets/e5-small-int8')],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, encoding='utf-8', creationflags=subprocess.CREATE_NO_WINDOW)
    observations = []
    try:
        for identity, kind, texts in [('q1', 'query', ['Quem responde por danos?']),
                    ('p1', 'passage', ['A responsabilidade exige prova.', 'A receita de bolo contém farinha.']),
                    ('q2', 'query', ['Como reparar o prejuízo?'])]:
                request = {'schema_version': 1, 'id': identity, 'kind': kind, 'texts': texts}
                process.stdin.write(json.dumps(request, ensure_ascii=False) + '\n')
                process.stdin.flush()
                received = queue.Queue(maxsize=1)
                threading.Thread(target=lambda: received.put(process.stdout.readline()), daemon=True).start()
                response = json.loads(received.get(timeout=45))
                assert response['id'] == identity and response['ok']
                result = response['result']
                assert all(len(v) == 384 for v in result['vectors'])
                observations.append({'id': identity, 'model': response['model'],
                    'vector_count': len(result['vectors']), 'truncated': result['truncated'],
                    'tokenization_us': result['tokenization_us'], 'inference_us': result['inference_us'],
                    'pooling_us': result['pooling_us'], 'os_memory': measure(process.pid)})
        process.stdin.close()
        assert process.wait(timeout=10) == 0, process.stderr.read()
        assert [x['model']['completed_batches'] for x in observations] == [1, 2, 3]
        assert len({x['model']['load_ms'] for x in observations}) == 1
        print(json.dumps({'schema_version': 1, 'actual_native_worker': True,
            'worker_exited': True, 'model_initializations_in_worker_source': 1,
            'memory_measurement': 'Windows GetProcessMemoryInfo on live child after each response',
            'observations': observations}, indent=2))
    finally:
        if process.poll() is None:
            process.kill()
            process.wait(timeout=10)


if __name__ == '__main__':
    main()
