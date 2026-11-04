"""Explicit pinned R07 assets; shares the existing runtime and E5 tokenizer.

Does not download an SDK or load an inference session. Invoke individual profiles
or all four. No replacement of an existing file with a different hash.
"""
import argparse
import hashlib
import json
import shutil
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DEST = ROOT / 'assets' / 'benchmark'
MINI = 'https://huggingface.co/sentence-transformers/paraphrase-multilingual-MiniLM-L12-v2/resolve/e8f8c211226b894fcb81acc59f3b34ba3efd5f42/'
E5 = 'https://huggingface.co/Xenova/multilingual-e5-small/resolve/761b726dd34fb83930e26aab4e9ac3899aa1fa78/'
FILES = {
    'e5-fp32': ('e5-fp32.onnx', E5+'onnx/model.onnx', 470268533, '4aa845c27760e06e9a686b9d8b5d440eae4b6612cd09e5b522b716d3941f77ff'),
    'minilm-int8': ('minilm-int8.onnx', MINI+'onnx/model_quint8_avx2.onnx', 118453870, '98a01d88b7de996cdea58c32ca71208c09968d143798814b2ea09d3439dc334f'),
    'minilm-fp32': ('minilm-fp32.onnx', MINI+'onnx/model.onnx', 470301610, '10f7a088420252b26caf819236ca2c9d2987afd0fc06fec7553b542a5655a05a'),
    'minilm-tokenizer': ('minilm-tokenizer.json', MINI+'tokenizer.json', 9081518, '2c3387be76557bd40970cec13153b3bbf80407865484b209e655e5e4729076b8'),
}

def digest(path):
    with path.open('rb') as f:
        return hashlib.file_digest(f, 'sha256').hexdigest()

def download(item):
    name,url,size,sha = item
    target = DEST/name
    if target.exists():
        if target.stat().st_size != size or digest(target) != sha:
            raise RuntimeError(f'Existing {name} differs; refusing overwrite')
    else:
        partial = target.with_suffix(target.suffix+'.part')
        if partial.exists():
            raise RuntimeError(f'Inspect existing incomplete {partial.name} before retry')
        req=urllib.request.Request(url,headers={'User-Agent':'retrieval-benchmark-setup/1'})
        with urllib.request.urlopen(req,timeout=90) as response, partial.open('xb') as out:
            count=0
            while block := response.read(1024*1024):
                count+=len(block)
                if count>size: raise RuntimeError(f'Unexpected {name} size')
                out.write(block)
        if count!=size or digest(partial)!=sha: raise RuntimeError(f'{name} failed integrity check')
        partial.replace(target)
    print(f'Verified {name}: {size} bytes',flush=True)
    return {'path':str(target.relative_to(ROOT)),'source':url,'bytes':size,'sha256':sha}

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('profiles',nargs='+',choices=['e5-fp32','minilm-int8','minilm-fp32','all'])
    args=parser.parse_args()
    profiles=set(args.profiles)
    if 'all' in profiles: profiles=set(FILES)-{'minilm-tokenizer'}
    if any(p.startswith('minilm-') for p in profiles): profiles.add('minilm-tokenizer')
    pending=sum(FILES[p][2] for p in profiles if not (DEST/FILES[p][0]).exists())
    free=shutil.disk_usage(ROOT).free
    if free < pending+1024**3: raise RuntimeError(f'Need {pending} additional bytes plus 1GiB workspace reserve; free={free}')
    runtime=ROOT/'assets'/'e5-small-int8'/'onnxruntime.dll'
    if not runtime.exists(): raise RuntimeError('Run pinned prepare_e5.py first; runtime is shared')
    DEST.mkdir(parents=True,exist_ok=True)
    records=[download(FILES[p]) for p in sorted(profiles)]
    # The revisions have no standalone LICENSE file. Preserve the published
    # license metadata with its exact source/revision, without large model cards.
    for name,repo,rev,expected in [
        ('MiniLM-LICENSE-METADATA.json','sentence-transformers/paraphrase-multilingual-MiniLM-L12-v2','e8f8c211226b894fcb81acc59f3b34ba3efd5f42','apache-2.0'),
        ('E5-LICENSE-METADATA.json','intfloat/multilingual-e5-small','614241f622f53c4eeff9890bdc4f31cfecc418b3','mit')]:
        url=f'https://huggingface.co/api/models/{repo}/revision/{rev}'
        with urllib.request.urlopen(url,timeout=30) as response: raw=response.read(2*1024**2+1)
        if len(raw)>2*1024**2: raise RuntimeError('Model metadata exceeds budget')
        metadata=json.loads(raw)
        license_id=metadata.get('cardData',{}).get('license')
        if metadata['sha']!=rev or license_id!=expected: raise RuntimeError('Unexpected license/revision')
        (DEST/name).write_text(json.dumps({'source':url,'revision':rev,'license':license_id,'scope':'Published model license declaration, not a standalone license text'},indent=2)+'\n',encoding='utf-8')
    print(json.dumps({'scope':'setup only, not inference','free_before':free,'new_model_bytes_planned':pending,'verified':records}))

if __name__=='__main__': main()
