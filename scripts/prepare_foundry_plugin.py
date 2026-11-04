"""Materialize the one current native plugin payload, not a second build tree.

Foundry v1 fingerprints lib/. Models stay in the existing separate resource dir.
No host registration/install/grant is performed by this setup command.
"""
import hashlib
import json
import shutil
import subprocess
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]
def digest(p):
    with p.open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()
def main():
    binary=ROOT/'target/debug/retrieval-worker.exe'
    if not binary.is_file():raise RuntimeError('Build retrieval-worker explicitly first')
    probe=subprocess.run([str(binary)],input=b'{"schema_version":1,"id":"package-probe","op":"capabilities"}\n',capture_output=True,timeout=15,creationflags=subprocess.CREATE_NO_WINDOW)
    capabilities=json.loads(probe.stdout)['result']
    help_probe=subprocess.run([str(binary),'--help'],capture_output=True,timeout=15,creationflags=subprocess.CREATE_NO_WINDOW)
    if probe.returncode or help_probe.returncode or not capabilities.get('cpu_compiled') or capabilities.get('model_loaded') or b'--foundry-job' not in help_probe.stdout:
        raise RuntimeError('Build the current CPU-capable Foundry worker explicitly before packaging')
    if binary.stat().st_size>32*1024**2:raise RuntimeError('Native payload exceeds observed host per-code-file limit')
    if shutil.disk_usage(ROOT).free<binary.stat().st_size+256*1024**2:raise RuntimeError('Insufficient package workspace reserve')
    package=ROOT/'adapters/foundry';dest=package/'lib/retrieval-worker.exe';dest.parent.mkdir(exist_ok=True)
    sha=digest(binary)
    if not dest.exists() or digest(dest)!=sha:
        partial=dest.with_suffix('.pending')
        if partial.exists():raise RuntimeError('Inspect prior incomplete native package before retry')
        shutil.copyfile(binary,partial)
        if digest(partial)!=sha:raise RuntimeError('Native copy integrity differs')
        partial.replace(dest) # only the current generated payload; no backups
    model=ROOT/'assets/e5-small-int8'
    files=[]
    for name in ['LICENSE','ThirdPartyNotices.txt','manifest.json','model.onnx','tokenizer.json','onnxruntime.dll','onnxruntime_providers_shared.dll']:
        p=model/name
        if not p.is_file():raise RuntimeError('Explicitly prepare E5 assets before resource manifest')
        files.append({'path':name,'sha256':digest(p),'bytes':p.stat().st_size})
    resource={'apiVersion':1,'id':'retrieval-e5-small-int8','version':'1.22.0-761b726d','name':'E5 Small CPU Int8 and pinned ORT','files':sorted(files,key=lambda f:f['path'])}
    (model/'foundry.resource.json').write_text(json.dumps(resource,indent=2)+'\n',encoding='utf-8')
    print(json.dumps({'scope':'local package preparation only; no host admission','native_bytes':dest.stat().st_size,'native_sha256':sha,'resource_bytes':sum(f['bytes'] for f in files),'model_copies_created':0,'runtime_copies_created':0,'package':str(package)}))
if __name__=='__main__':main()
