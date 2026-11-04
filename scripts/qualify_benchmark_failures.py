"""Negative native-run checks: stale source and wrong gold admit no report."""
import copy
import json
import subprocess
import tempfile
from pathlib import Path

ROOT=Path(__file__).resolve().parents[1]

def main():
    corpus=json.loads((ROOT/'benchmarks/pt-official-pilot-v1.json').read_text(encoding='utf-8'))
    cases=[]
    with tempfile.TemporaryDirectory(prefix='retrieval-benchmark-negative-') as folder:
        for name,mutate,expected in [
            ('stale_snapshot',lambda c:c['sources'][0].__setitem__('raw_sha256','0'*64),'source snapshot differs'),
            ('wrong_gold',lambda c:c['queries'][0]['gold'][0].__setitem__('quote_sha256','0'*64),'gold quote hash differs'),
        ]:
            value=copy.deepcopy(corpus);mutate(value)
            path=Path(folder)/(name+'.json');path.write_text(json.dumps(value),encoding='utf-8')
            result=subprocess.run([str(ROOT/'target/debug/examples/retrieval_benchmark.exe'),str(path),'e5-int8'],cwd=ROOT,capture_output=True,text=True,encoding='utf-8',timeout=90,creationflags=subprocess.CREATE_NO_WINDOW)
            if result.returncode==0 or result.stdout.strip() or expected not in result.stderr:raise RuntimeError('Negative case did not fail closed: '+name)
            cases.append({'case':name,'exit_code':result.returncode,'stdout_empty':True,'expected_error_observed':expected})
    (ROOT/'verification/r07-negative-native.json').write_text(json.dumps({'schema_version':1,'cases':cases,'scope':'Native benchmark rejects invalid identity/gold before inference; no result report'},indent=2)+'\n',encoding='utf-8')
    print('Two native negative cases passed')

if __name__=='__main__':main()
