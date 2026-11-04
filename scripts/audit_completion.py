"""Audit recorded executions against the delivered files and pinned resources.

Does not rerun inference, contact a host, install resources or grant permissions.
Functional evidence comes from the named tests/qualifiers, not this inventory.
"""
import hashlib
import json
import shutil
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]
def digest(p):
    with p.open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()
def read(name):return json.loads((ROOT/'verification'/name).read_text(encoding='utf-8'))
def require(value,message):
    if not value:raise RuntimeError(message)
def main():
    tests=read('r08-foundry-final-checks.json')
    require(all(v['exit_code']==0 for v in tests.values()),'Required checks did not all pass')
    require(tests['default']['passed']==85 and tests['all-features']['passed']==97 and tests['actual']['passed']==14,'Test counts differ from final audit')
    host=read('r08-foundry-host.json')
    require(host['complete'] and host['fresh_authenticated_operator'] and host['host_exit_observed'] and host['qa_runtime_removed'],'Actual host lifecycle is incomplete')
    require(all(host['bootstrap_ownership_checks'].values()),'QA host bootstrap ownership not proven')
    require(host['anonymous_plugins_status']==401 and host['resource_revoked'] and host['operator_logged_out'],'Host access/grant lifecycle incomplete')
    require(host['independent_download_and_pack_hash_verification'],'Host downloads/pack were not independently checked')
    for name in ['retrieve','expand']:
        require(host[name]['process_stopped'] and host[name]['exit_code']==0,'Positive host workflow did not finish successfully')
    require(host['retrieve']['repeated_request_same_job'],'Host idempotency missing')
    denied=host['revoked_model_grant_denied']
    require(denied['status']=='failed' and denied['exit_code']!=0 and denied['process_stopped'] and denied['phase']=='model_grant' and denied['completed_result_absent'],'Revoked model was not refused')
    package=ROOT/'adapters/foundry';native=package/'lib/retrieval-worker.exe';native_sha=digest(native)
    require(native_sha==host['native_exe_sha256']==digest(ROOT/'target/debug/retrieval-worker.exe'),'Delivered native bytes differ from host-qualified bytes')
    # Same ASCII relative paths + sorted (path,SHA) pairs as the observed v1 host.
    manifest=json.loads((package/'foundry.plugin.json').read_text(encoding='utf-8'))
    paths=set()
    for name in ['foundry.plugin.json','package.json','package-lock.json','src','lib','schemas','design','assets']:
        p=package/name
        if p.is_file():paths.add(p)
        elif p.is_dir():paths.update(q for q in p.rglob('*') if q.is_file())
    paths.update(package/p for p in manifest.get('context',[]))
    paths.update(package/w['inputsSchema'] for w in manifest['workflows'])
    pairs=sorted((p.relative_to(package).as_posix(),digest(p)) for p in paths)
    fingerprint=hashlib.sha256(json.dumps(pairs,separators=(',',':'),ensure_ascii=False).encode()).hexdigest()
    require(fingerprint==host['plugin_fingerprint'],'Current package fingerprint differs from real host registration')
    for name,count in [('r08-foundry-synthetic.json',7),('r08-foundry-official.json',8)]:
        r=read(name)
        require(r['native_exe_sha256']==native_sha and r['independent_hash_verification'] and r['no_match_completed_without_pack'],'Current package local flow evidence differs')
        require(len(r['negative_cases'])==count and all(n['exit_code']!=0 and n['completed_result_absent'] for n in r['negative_cases']),'Negative package cases missing')
        require(len(r['generic_formats'])==2 and all(f['retrieve_and_later_expand'] for f in r['generic_formats']),'Text/HTML continuation evidence missing')
    consumer=read('r08-rust-consumer-official.json')
    require(consumer['independent_python_readonly_verification'] and consumer['worker_stopped'] and consumer['shutdown']['exit_observed'] and consumer['shutdown']['tree_exit_observed'],'Rust/SQLite consumer evidence missing')
    model=ROOT/'assets/e5-small-int8';resources=json.loads((model/'manifest.json').read_text(encoding='utf-8'))
    for name,r in resources['files'].items():require((model/name).stat().st_size==r['bytes'] and digest(model/name)==r['sha256'],'Pinned runtime/model integrity differs')
    benchmark=read('r07-benchmark-summary.json')
    require(sum(benchmark['native_runs'].values())==5692 and not benchmark['bge_m3']['measured'],'Benchmark evidence/scope differs')
    source_files=sorted({*ROOT.glob('src/**/*.rs'),*ROOT.glob('tests/**/*.rs'),*ROOT.glob('examples/*.rs'),*ROOT.glob('scripts/*.py'),*ROOT.glob('contracts/*.json'),*ROOT.glob('benchmarks/*.json'),*ROOT.glob('docs/*.md'),*ROOT.glob('plan/**/*.md'),*package.glob('src/*'),*package.glob('schemas/*'),package/'README.md',package/'foundry.plugin.json',ROOT/'Cargo.toml',ROOT/'Cargo.lock',ROOT/'README.md',ROOT/'AGENTS.md',ROOT/'.cargo/config.toml'})
    report={'schema_version':1,'scope':'Final local delivery and separate real QA host; no production/app adoption','required_checks_passed':True,'distinct_all_features_tests':111,'actual_onnx_opt_in_tests':14,'native_exe_sha256':native_sha,'host_exe_sha256':host['host_exe_sha256'],'plugin_fingerprint':fingerprint,'source_hashes':[{'path':p.relative_to(ROOT).as_posix(),'sha256':digest(p)} for p in source_files if p.is_file()],'production_adoption':False,'benchmark_scope':'Four actual E5/MiniLM profiles, 5692 runs; BGE audited, not measured','optional_benchmark_models_retained':False,'runtime_model_retained':True,'observed_free_bytes':shutil.disk_usage(ROOT).free}
    target=ROOT/'verification/completion-audit.json';pending=target.with_suffix('.pending.json')
    pending.write_text(json.dumps(report,indent=2)+'\n',encoding='utf-8');pending.replace(target)
    print('Current package fingerprint/native/model hashes match qualified evidence; 111 distinct tests; real QA host lifecycle confirmed')
if __name__=='__main__':main()
