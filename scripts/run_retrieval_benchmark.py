"""Real CPU benchmark supervisor; writes complete reports only on success.

Memory is for the one benchmark process (ORT is in-process), not an entire app.
Source/model setup stays outside this runner. No LLM or network model API.
"""
import argparse
import ctypes
import hashlib
import json
import platform
import subprocess
import tempfile
import time
from pathlib import Path
from ctypes import wintypes

ROOT = Path(__file__).resolve().parents[1]

class Counters(ctypes.Structure):
    _fields_=[('cb',wintypes.DWORD),('PageFaultCount',wintypes.DWORD)]+[(name,ctypes.c_size_t) for name in [
        'PeakWorkingSetSize','WorkingSetSize','QuotaPeakPagedPoolUsage','QuotaPagedPoolUsage',
        'QuotaPeakNonPagedPoolUsage','QuotaNonPagedPoolUsage','PagefileUsage','PeakPagefileUsage','PrivateUsage']]

def check_report(report, dataset):
    if report['complete'] is not True or report['dataset_sha256']!=hashlib.sha256(dataset).hexdigest():
        raise RuntimeError('Incomplete report/dataset identity mismatch')
    corpus=json.loads(dataset)
    gold_by_id={q['id']:q['gold'] for q in corpus['queries']}
    seen=set()
    for result in report['results']:
        key=(result['query_id'],result['format'],result['repeat'])
        if key in seen: raise RuntimeError('Duplicate result')
        seen.add(key)
        gold=gold_by_id[result['query_id']]
        if gold!=result['gold']: raise RuntimeError('Result gold changed')
        if sorted(r['mode'] for r in result['rankings'])!=['bm25','hybrid_rrf60','semantic']:
            raise RuntimeError('Missing/duplicate retrieval mode')
        for ranking in result['rankings']:
            top=ranking['top']
            if [m['k'] for m in ranking['metrics']]!=[1,3,5,10]: raise RuntimeError('Missing metric cutoff')
            for metric in ranking['metrics']:
                k=metric['k']
                hits=[rank+1 for rank,s in enumerate(top[:k]) if any(s['source_index']==g['source_index'] and s['start']<=g['start'] and s['end']>=g['end'] for g in gold)]
                if gold:
                    first=hits[0] if hits else None
                    expected_recall=float(bool(first))
                    expected_rr=1/first if first else 0.
                    import math
                    expected_ndcg=1/math.log2(first+1) if first else 0.
                    if abs(metric['recall']-expected_recall)>1e-9 or abs(metric['reciprocal_rank']-expected_rr)>1e-9 or abs(metric['ndcg']-expected_ndcg)>1e-9:
                        raise RuntimeError('Independent metric verification failed')
                elif metric['negative_returned_any']!=bool(top): raise RuntimeError('Negative result mismatch')
    expected={(q['id'],fmt,repeat) for q in corpus['queries'] for fmt in ['fp32','int8','binary'] for repeat in [0,1]}
    if seen!=expected: raise RuntimeError('Missing evaluation row')
    count=report['passage_embeddings']['count']
    if report['model']['inference_runs']!=1+count+len(expected) or report['model']['cache_enabled']:
        raise RuntimeError('Inference/cache accounting differs')

def run(profile, dataset_path):
    exe=ROOT/'target/debug/examples/retrieval_benchmark.exe'
    started=time.monotonic()
    peaks={'peak_working_set_bytes':0,'peak_pagefile_commit_bytes':0,'sampled_max_private_commit_bytes':0,'successful_samples':0,'failed_samples':0,'sampling_interval_ms':50,'scope':'one benchmark process; ORT in-process; not system-wide RAM'}
    psapi=ctypes.WinDLL('psapi',use_last_error=True)
    psapi.GetProcessMemoryInfo.argtypes=[wintypes.HANDLE,ctypes.POINTER(Counters),wintypes.DWORD]
    psapi.GetProcessMemoryInfo.restype=wintypes.BOOL
    # Temporary files are exact test-owned handles, automatically removed. Pipes
    # are avoided so the final JSON cannot deadlock a memory-observation loop.
    with tempfile.TemporaryFile() as stdout,tempfile.TemporaryFile() as stderr:
        process=subprocess.Popen([str(exe),str(dataset_path),profile],cwd=ROOT,stdout=stdout,stderr=stderr,creationflags=subprocess.CREATE_NO_WINDOW)
        try:
            while process.poll() is None:
                c=Counters();c.cb=ctypes.sizeof(c)
                if psapi.GetProcessMemoryInfo(int(process._handle),ctypes.byref(c),c.cb):
                    peaks['successful_samples']+=1
                    peaks['peak_working_set_bytes']=max(peaks['peak_working_set_bytes'],c.PeakWorkingSetSize)
                    peaks['peak_pagefile_commit_bytes']=max(peaks['peak_pagefile_commit_bytes'],c.PeakPagefileUsage)
                    peaks['sampled_max_private_commit_bytes']=max(peaks['sampled_max_private_commit_bytes'],c.PrivateUsage)
                else: peaks['failed_samples']+=1
                if time.monotonic()-started>360: raise TimeoutError('Benchmark exceeded host 360s wall limit')
                time.sleep(.05)
            if process.returncode:
                stderr.seek(0);raise RuntimeError(f'Benchmark exit {process.returncode}: '+stderr.read(65536).decode('utf-8','replace'))
            stdout.seek(0)
            raw=stdout.read(16*1024**2+1)
            if len(raw)>16*1024**2: raise RuntimeError('Benchmark report exceeds host budget')
            report=json.loads(raw)
        finally:
            if process.poll() is None: process.kill()
            process.wait()
    check_report(report,dataset_path.read_bytes())
    report['host_observation']={'platform':platform.platform(),'machine':platform.machine(),'logical_cpus':__import__('os').cpu_count(),'wall_seconds':time.monotonic()-started,'memory':peaks,'independent_metric_verification':True,'exe_sha256':hashlib.sha256(exe.read_bytes()).hexdigest()}
    if peaks['successful_samples']==0: raise RuntimeError('No process-memory evidence captured')
    target=ROOT/'verification'/f'r07-benchmark-{profile}.json'
    target.write_text(json.dumps(report,ensure_ascii=False,separators=(',',':'))+'\n',encoding='utf-8')
    print(profile,'complete',report['model']['inference_runs'],'native runs;',round(time.monotonic()-started,2),'s',flush=True)

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('profiles',nargs='+',choices=['e5-int8','e5-fp32','minilm-int8','minilm-fp32'])
    parser.add_argument('--dataset',type=Path,default=ROOT/'benchmarks/pt-official-pilot-v1.json')
    args=parser.parse_args()
    if platform.system()!='Windows': raise RuntimeError('This memory qualification targets Windows')
    for profile in args.profiles: run(profile,args.dataset.resolve())

if __name__=='__main__': main()
