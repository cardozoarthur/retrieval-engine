"""Local real Node/Rust Foundry-envelope qualification; not host registration.

The envelope/resource grant is a QA fixture. CPU inference and guarded parsing
are real. Official mode selects public bytes once, then expands without HTTP.
"""
import argparse
import copy
import hashlib
import json
import shutil
import subprocess
import sys
import urllib.request
import uuid
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]
sys.path.insert(0,str(ROOT/'.tools/schema-validator'))
from jsonschema import Draft202012Validator
PACKAGE=ROOT/'adapters/foundry'
URL='https://www2.camara.leg.br/legin/fed/lei/2018/lei-13709-14-agosto-2018-787077-normaatualizada-pl.pdf'
def sha(b):return hashlib.sha256(b).hexdigest()
def canonical(value):return sha(json.dumps(value,sort_keys=True,ensure_ascii=False,separators=(',',':')).encode())
def verify_pack(pack):
    if canonical([pack['schema_version'],pack['payload']])!=pack['sha256']:raise RuntimeError('Pack checksum differs')
    for record in pack['payload']['ancestors']+[v['record'] for v in pack['payload']['entries']]:
        if record['end']<=record['start']:raise RuntimeError('Invalid record interval')
    for v in pack['payload']['entries']:
        if canonical(v['record'])!=v['reference'] or sha(v['evidence']['text'].encode())!=v['record']['quote_sha256'] or len(v['evidence']['text'])!=v['record']['end']-v['record']['start']:raise RuntimeError('Reference/quote differs')
def main():
    parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('--official',action='store_true');args=parser.parse_args()
    schema=json.loads((PACKAGE/'schemas/request.json').read_text());Draft202012Validator.check_schema(schema);validator=Draft202012Validator(schema)
    if args.official:
        with urllib.request.urlopen(URL,timeout=30) as r:source=r.read(8*1024**2+1)
        if len(source)>8*1024**2:raise RuntimeError('QA selected source exceeds byte budget')
    else:source=(ROOT/'tests/fixtures/two-pages.pdf').read_bytes()
    temp=(ROOT/'tmp'/('r08-foundry-'+str(uuid.uuid4()))).resolve();temp.mkdir(parents=True)
    if temp.parent!=(ROOT/'tmp').resolve() or not temp.name.startswith('r08-foundry-'):raise RuntimeError('Invalid QA workspace')
    project=str(uuid.uuid4());source_id=str(uuid.uuid4());pack_id=str(uuid.uuid4())
    model_manifest=json.loads((ROOT/'assets/e5-small-int8/foundry.resource.json').read_text())
    grant={'apiVersion':1,'grantId':str(uuid.uuid4()),'installationId':str(uuid.uuid4()),'resourceId':'retrieval-e5-small-int8','resourceFingerprint':canonical(model_manifest),'registrationId':str(uuid.uuid4()),'pluginFingerprint':'b'*64,'workspaceId':project,'permission':'resource.read','path':str(ROOT/'assets/e5-small-int8'),'manifest':model_manifest}
    jobs=[]
    def job(input,pack=None,resources=None,change=None,source_override=None):
        job_id=str(uuid.uuid4());base=temp/job_id;(base/'inputs').mkdir(parents=True);out=base/'out';out.mkdir()
        artifacts=[]
        for artifact_id,name,data in [(source_id,source_override[0],source_override[1]) if source_override else (source_id,'selected.pdf',source)]+([(pack_id,'pack.json',json.dumps(pack,ensure_ascii=False).encode())] if pack else []):
            path=base/'inputs'/artifact_id;path.write_bytes(data);artifacts.append({'artifactId':artifact_id,'path':str(path),'name':name,'sha256':sha(data),'bytes':len(data)})
        value={'apiVersion':1,'jobId':job_id,'projectId':project,'workflowId':'request','input':input,'outputDirectory':str(out),'inputArtifacts':artifacts,'resources':resources or []}
        if change:change(value,base)
        job_file=base/'job.json';job_file.write_text(json.dumps(value,ensure_ascii=False),encoding='utf-8')
        p=subprocess.run([shutil.which('node'),str(PACKAGE/'src/cli.mjs'),'--job',str(job_file),'--out',str(out)],cwd=PACKAGE,capture_output=True,timeout=120,creationflags=subprocess.CREATE_NO_WINDOW)
        return p,out,value
    def success(p,out,value):
        if p.returncode:raise RuntimeError('Node/native adapter failed a positive envelope '+value['jobId'])
        result=json.loads((out/'result.json').read_text(encoding='utf-8'))
        if result['jobId']!=value['jobId'] or result['workflowId']!='request' or result['status']!='completed':raise RuntimeError('Result identity differs')
        for a in result['artifacts']:
            target=(out/a['path']).resolve()
            if target.parent!=out.resolve():raise RuntimeError('Artifact outside output')
            b=target.read_bytes()
            if a['sha256']!=sha(b) or a['bytes']!=len(b):raise RuntimeError('Artifact hash/size differs')
        report=json.loads((out/'retrieval.json').read_text(encoding='utf-8'))
        pack=json.loads((out/'pack.json').read_text(encoding='utf-8'));verify_pack(pack)
        if report['http_requests_by_adapter']!=0 or not report['isolation']['helper_exit_confirmed']:raise RuntimeError('Adapter acquisition/process scope differs')
        jobs.append({'job_id':value['jobId'],'exit_code':p.returncode,'artifact_hashes':result['artifacts'],'isolation':report['isolation'],'snapshot_id':report['provenance']['snapshot_id'],'pack_sha256':pack['sha256'],'selected_entry':pack['payload']['entries'][0]})
        return report,pack
    try:
        request={'operation':'retrieve','artifact_id':source_id,'query':'Quem deve reparar danos por uso indevido de informações pessoais?' if args.official else 'responsabilidade','mode':'hybrid' if args.official else 'literal','top_k':1,'before':2,'after':3,'unit':'words'}
        validator.validate(request)
        p,out,value=job(request,resources=[grant] if args.official else [])
        report,pack=success(p,out,value)
        original=pack['payload']['entries'][0];reference=original['reference']
        expand={'operation':'expand','artifact_id':source_id,'pack_artifact_id':pack_id,'reference':reference,'before':7,'after':11,'unit':'characters'}
        validator.validate(expand)
        p,out,value=job(expand,pack)
        next_report,next_pack=success(p,out,value)
        child=next_pack['payload']['entries'][0]
        if next_report['provenance']['snapshot_id']!=report['provenance']['snapshot_id'] or child['record']['parent_reference']!=reference or child['record']['anchor_start']!=original['record']['anchor_start'] or child['record']['anchor_end']!=original['record']['anchor_end'] or child['record']['start']!=max(0,original['record']['start']-7) or child['record']['end']<original['record']['end']:raise RuntimeError('Cross-job expansion changed snapshot/anchor')
        # Generic formats and accents are qualified through the same guarded path.
        formats=[]
        for name,data in [('selected.txt','Antes do café há medição. Café e ação pertencem ao sensor. Depois da medição há registro.'.encode()),('selected.html','<!doctype html><meta charset="utf-8"><p>Antes do café há medição. Café e ação pertencem ao sensor. Depois há registro.</p><script>texto invisível</script>'.encode())]:
            text_request=dict(request,query='Café',mode='literal')
            p,out,value=job(text_request,source_override=(name,data))
            typed_report,typed_pack=success(p,out,value)
            if 'Café' not in typed_pack['payload']['entries'][0]['evidence']['text']:raise RuntimeError('Unicode source match was lost')
            p,out,value=job(dict(expand,reference=typed_pack['payload']['entries'][0]['reference']),typed_pack,source_override=(name,data))
            success(p,out,value)
            formats.append({'name':name,'retrieve_and_later_expand':True})
        no_match=dict(request,query='NO_MATCH_FOR_SELECTED_SOURCE_849311',mode='literal')
        p,out,value=job(no_match)
        if p.returncode!=0 or not (out/'result.json').exists():raise RuntimeError('No-match query must complete without a fabricated pack')
        empty=json.loads((out/'retrieval.json').read_text(encoding='utf-8'))
        if empty['entries'] or empty['pack_sha256'] is not None or (out/'pack.json').exists():raise RuntimeError('No-match query fabricated evidence')
        negatives=[]
        negatives=[]
        cases=[('input_path_injection',dict(request,path='not-a-grant'),None,None),('source_bytes_changed',request,None,lambda j,b:(b/'inputs'/source_id).write_bytes(b'changed')),('selected_hash_changed',request,None,lambda j,b:j['inputArtifacts'][0].__setitem__('sha256','0'*64)),('different_project_pack',expand,pack,lambda j,b:j.__setitem__('projectId',str(uuid.uuid4()))),('unselected_reference',dict(expand,reference='f'*64),pack,None),('missing_model_grant',dict(request,mode='hybrid'),None,None)]
        for name,req,previous,change in cases:
            p,out,value=job(req,previous,change=change)
            if p.returncode==0 or (out/'result.json').exists():raise RuntimeError('Negative job admitted: '+name)
            negatives.append({'case':name,'exit_code':p.returncode,'completed_result_absent':True})
        # Same-size forged weights exercise SHA verification, without touching
        # any shared source model. This one temporary fixture is not a backup.
        expected_bytes=next(f['bytes'] for f in model_manifest['files'] if f['path']=='model.onnx')
        if shutil.disk_usage(ROOT).free<expected_bytes+512*1024**2:raise RuntimeError('QA forged-model fixture lacks workspace reserve')
        fake=temp/'forged-model';fake.mkdir();(fake/'manifest.json').write_bytes((ROOT/'assets/e5-small-int8/manifest.json').read_bytes())
        with (fake/'model.onnx').open('xb') as f:
            left=expected_bytes;block=bytes(1024**2)
            while left:amount=min(left,len(block));f.write(block[:amount]);left-=amount
        altered=copy.deepcopy(grant);altered['path']=str(fake)
        p,out,value=job(dict(request,mode='hybrid'),resources=[altered])
        observation=json.loads((out/'failure.json').read_text(encoding='utf-8'))
        if p.returncode==0 or (out/'result.json').exists() or observation['phase']!='model_load' or observation['error_fingerprint']!=sha(b'embedding resource SHA256 mismatch'):raise RuntimeError('Same-size altered weights were not rejected by SHA check')
        negatives.append({'case':'same_size_altered_model_weights','exit_code':p.returncode,'phase':observation['phase'],'sha_mismatch_observed':True,'completed_result_absent':True})
        if args.official:
            bad=copy.deepcopy(grant);bad['workspaceId']=str(uuid.uuid4())
            p,out,value=job(request,resources=[bad])
            if p.returncode==0 or (out/'result.json').exists():raise RuntimeError('Wrong-scope model grant admitted')
            negatives.append({'case':'wrong_scope_model_grant','exit_code':p.returncode,'completed_result_absent':True})
        receipt={'schema_version':1,'scope':'Real Node/native package against synthetic Foundry envelope, not authenticated host registration/grant','official_selected_bytes':args.official,'source_raw_sha256':sha(source),'qa_source_url':URL if args.official else None,'native_exe_sha256':sha((PACKAGE/'lib/retrieval-worker.exe').read_bytes()),'request_schema_sha256':sha((PACKAGE/'schemas/request.json').read_bytes()),'independent_hash_verification':True,'jobs':jobs,'generic_formats':formats,'no_match_completed_without_pack':True,'negative_cases':negatives,'model':report['search'].get('model'),'large_llm_calls':0,'adapter_http_requests':0,'external_host_admission':False,'qa_files_removed':True}
        (ROOT/'verification'/('r08-foundry-official.json' if args.official else 'r08-foundry-synthetic.json')).write_text(json.dumps(receipt,ensure_ascii=False,indent=2)+'\n',encoding='utf-8')
        print('Node/Rust selected-source retrieve + cross-job expansion passed;',len(negatives),'negative cases')
    finally:
        if temp.parent==(ROOT/'tmp').resolve() and temp.name.startswith('r08-foundry-'):shutil.rmtree(temp)

if __name__=='__main__':main()
