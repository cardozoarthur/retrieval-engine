"""Run a separate QA Foundry instance against the real external plugin APIs.

Uses an existing host executable read-only, a new runtime, fresh operator bootstrap
and private in-process cookie jar. Credentials never enter output/receipts. Does
not connect to an existing app, reuse its sessions, migrate it or copy its runtime.
"""
import argparse
import ctypes
import hashlib
import http.cookiejar
import json
import os
import re
import shutil
import socket
import subprocess
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from pathlib import Path
from qualify_foundry_adapter import verify_pack
ROOT=Path(__file__).resolve().parents[1]
PACKAGE=ROOT/'adapters/foundry'
def sha(b):return hashlib.sha256(b).hexdigest()
def file_sha(p):
    with p.open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()
def uid():return str(uuid.uuid4())

def local_known_folder():
    # Same authoritative Known Folder API used by the host, rather than an
    # environment variable which a launcher can redirect.
    folder_id=(ctypes.c_ubyte*16).from_buffer_copy(uuid.UUID('f1b32785-6fba-4fcf-9d55-7b8e7f157091').bytes_le)
    result=ctypes.c_wchar_p()
    shell=ctypes.WinDLL('shell32');ole=ctypes.WinDLL('ole32')
    shell.SHGetKnownFolderPath.argtypes=[ctypes.c_void_p,ctypes.c_uint32,ctypes.c_void_p,ctypes.POINTER(ctypes.c_wchar_p)]
    shell.SHGetKnownFolderPath.restype=ctypes.c_long
    ole.CoTaskMemFree.argtypes=[ctypes.c_void_p]
    if shell.SHGetKnownFolderPath(ctypes.byref(folder_id),0x4000,None,ctypes.byref(result))<0:raise RuntimeError('QA OS Known Folder unavailable')
    try:return Path(result.value).resolve()
    finally:ole.CoTaskMemFree(ctypes.cast(result,ctypes.c_void_p))

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--host',type=Path,required=True)
    args=parser.parse_args();host=args.host.resolve()
    if not host.is_file():raise RuntimeError('Selected QA host unavailable')
    root=(ROOT/'tmp'/('r08-host-'+uid())).resolve();root.mkdir(parents=True);(root/'dist').mkdir();runtime=root/'runtime'
    if root.parent!=(ROOT/'tmp').resolve():raise RuntimeError('Unexpected QA runtime root')
    with socket.socket() as probe:probe.bind(('127.0.0.1',0));port=probe.getsockname()[1]
    base=f'http://127.0.0.1:{port}'
    env={k:os.environ[k] for k in ['PATH','SystemRoot','WINDIR','PATHEXT','TEMP','TMP','USERPROFILE','LOCALAPPDATA','APPDATA','ProgramFiles','VOLTA_HOME'] if k in os.environ}
    node_probe=subprocess.run([shutil.which('node'),'-p','process.execPath'],capture_output=True,text=True,timeout=10,creationflags=subprocess.CREATE_NO_WINDOW)
    node_image=Path(node_probe.stdout.strip()).resolve()
    if node_probe.returncode or not node_image.is_file():raise RuntimeError('QA actual Node executable unavailable')
    env.update(ORQ_RUNTIME=str(runtime),ORQ_PORT=str(port),ORQ_NODE_BIN=str(node_image))
    jar=http.cookiejar.CookieJar();client=urllib.request.build_opener(urllib.request.ProxyHandler({}),urllib.request.HTTPCookieProcessor(jar));anonymous=urllib.request.build_opener(urllib.request.ProxyHandler({}))
    stage='start';access_file=None;owned_access=False;process=None;records={};success=False
    prior_file=ROOT/'verification/r08-foundry-host.json'
    if prior_file.exists():
        try:prior=json.loads(prior_file.read_text(encoding='utf-8'))
        except json.JSONDecodeError:prior={};records['previous_receipt_incomplete_after_disk_exhaustion']=True
        if prior.get('failure'):records['previous_failure']=prior['failure']
    def api(path,data=None,raw=None,auth=True,expect=200):
        headers={'Origin':base,'x-orquestrador':'local'};payload=None
        if data is not None:payload=json.dumps(data).encode();headers['Content-Type']='application/json'
        if raw is not None:payload=raw;headers['Content-Type']='application/octet-stream'
        req=urllib.request.Request(base+path,data=payload,headers=headers)
        try:
            with (client if auth else anonymous).open(req,timeout=60) as response:status=response.status;body=response.read(16*1024**2+1)
        except urllib.error.HTTPError as e:status=e.code;body=e.read(16384)
        if status!=expect:raise RuntimeError(f'QA host stage={stage} HTTP={status} expected={expect}')
        if expect!=200:return None
        return json.loads(body)
    def poll_job(id,expected_status='completed'):
        deadline=time.monotonic()+120
        while time.monotonic()<deadline:
            state=api('/api/plugins');job=next((j for j in state['jobs'] if j['id']==id),None)
            if job and job['status'] not in ['running','queued','pending']:
                if job['status']!=expected_status or job.get('process_stopped')is not True or (expected_status=='completed' and job.get('exit_code')!=0):
                    records['host_job_failure']={k:job.get(k) for k in ['status','process_stopped','exit_code','error']}
                    failure_path=runtime/'plugin-jobs'/id/'out/failure.json'
                    if failure_path.is_file():records['native_failure_observation']=json.loads(failure_path.read_text(encoding='utf-8'))
                    out_path=runtime/'plugin-jobs'/id/'out'
                    records['failed_output_file_names']=sorted(p.name for p in out_path.iterdir()) if out_path.is_dir() else []
                    records['qa_output_directory_exists']=out_path.is_dir()
                    envelope_file=runtime/'plugin-jobs'/id/'job.json'
                    if envelope_file.is_file():
                        wire=json.loads(envelope_file.read_text(encoding='utf-8'))
                        records['host_envelope_field_names']={'root':sorted(wire),'selected':[sorted(a) for a in wire.get('inputArtifacts',[])]}
                    raise RuntimeError('QA host job did not confirm successful stopped completion: '+str(job['status']))
                return job
            time.sleep(.1)
        raise RuntimeError('QA host job exceeded qualification deadline')
    def artifact(job,filename):
        entries=job['plugin_result']['artifacts'];index=next(i for i,a in enumerate(entries) if a['path']==filename)
        req=urllib.request.Request(base+f'/api/plugins/artifact/{job["id"]}/{index}',headers={'Origin':base})
        with client.open(req,timeout=30) as response:b=response.read(4*1024**2+1)
        expected=entries[index]
        if sha(b)!=expected['sha256'] or len(b)!=expected['bytes']:raise RuntimeError('Downloaded host artifact hash differs')
        return json.loads(b),b
    log_path=root/'host-output.private.log'
    try:
        with log_path.open('wb') as logs:
            process=subprocess.Popen([str(host)],cwd=root,env=env,stdin=subprocess.DEVNULL,stdout=logs,stderr=logs,creationflags=subprocess.CREATE_NO_WINDOW)
        deadline=time.monotonic()+30
        while True:
            if process.poll() is not None:raise RuntimeError('QA host exited during isolated startup')
            try:status=api('/api/operator/status',auth=False);break
            except (urllib.error.URLError,TimeoutError):
                if time.monotonic()>deadline:raise RuntimeError('QA host startup deadline')
                time.sleep(.1)
        # Only the newly created exact bootstrap file from our own QA instance.
        stage='fresh-local-bootstrap'
        access_file=Path(status['access_file']).resolve()
        local=local_known_folder()
        checks={'filename_matches':access_file.name=='open-operator.html','inside_os_known_folder':access_file.is_relative_to(local),'fresh_directory_prefix':access_file.parent.name.startswith('FoundryOperatorAccess-'),'created_after_qa_launch':access_file.stat().st_ctime>=process_start_wall-2}
        # Verify that this actual listening endpoint belongs to the child we
        # launched, not another app on a reused port. No auth material is output.
        command=f"(Get-NetTCPConnection -LocalAddress 127.0.0.1 -LocalPort {port} -State Listen -ErrorAction Stop).OwningProcess"
        owner=subprocess.run(['powershell','-NoProfile','-NonInteractive','-Command',command],capture_output=True,text=True,timeout=15,creationflags=subprocess.CREATE_NO_WINDOW)
        checks['listener_owned_by_qa_child']=owner.returncode==0 and owner.stdout.strip()==str(process.pid)
        records['bootstrap_ownership_checks']=checks
        if not all(checks.values()):raise RuntimeError('QA bootstrap ownership checks did not all pass')
        owned_access=True
        text=access_file.read_text(encoding='utf-8');match=re.search(r'#operator-access=([a-zA-Z0-9_-]+)',text)
        if not match:raise RuntimeError('Fresh operator bootstrap unavailable')
        api('/api/plugins',auth=False,expect=401)
        api('/api/operator/session',{'token':match.group(1)})
        del text,match
        if not api('/api/operator/status')['authenticated']:raise RuntimeError('QA operator authentication did not complete')
        records['anonymous_plugins_status']=401;records['fresh_authenticated_operator']=True
        stage='discover'
        candidate=api('/api/plugins/discover',{'path':str(PACKAGE)})
        records['plugin_fingerprint']=candidate['fingerprint']
        stage='workspace'
        workspace=api('/api/plugins/workspaces',{'request_id':uid(),'name':'Retrieval QA','purpose':'Temporary retrieval package integration qualification','confirm_plugin_workspace':True})
        project=workspace['id']
        stage='register'
        registration=api('/api/plugins/register',{'path':str(PACKAGE),'project_id':project,'expected_fingerprint':candidate['fingerprint'],'trusted_local_code':True,'enabled':True,'capabilities':['project.read','artifact.write','resource.read']})
        registration_id=registration['id'];records['registration_id']=registration_id;records['workspace_id']=project
        stage='resource-inspect-install-grant'
        inspected=api('/api/plugins/resources/inspect',{'request_id':uid(),'path':str(ROOT/'assets/e5-small-int8')})
        install_id=uid();installed=api('/api/plugins/resources/install',{'request_id':install_id,'preview_id':inspected['preview_id'],'expected_fingerprint':inspected['fingerprint']})
        deadline=time.monotonic()+60
        while installed['status']!='installed':
            if time.monotonic()>deadline:raise RuntimeError('QA resource install deadline')
            time.sleep(.1);installed=api('/api/plugins/resources/installations/'+install_id)
        granted=api('/api/plugins/resources/grants',{'request_id':uid(),'registration_id':registration_id,'expected_fingerprint':candidate['fingerprint'],'installation_id':install_id,'expected_resource_fingerprint':installed['fingerprint'],'confirm_readonly_access':True})
        records['resource_fingerprint']=installed['fingerprint'];records['resource_grant_id']=granted['request_id']
        stage='selected-input-upload'
        source=(ROOT/'tests/fixtures/two-pages.pdf').read_bytes();source_id=uid()
        query=urllib.parse.urlencode({'request_id':source_id,'project_id':project,'name':'selected.pdf','sha256':sha(source)})
        api('/api/plugins/inputs?'+query,raw=source)
        input={'operation':'retrieve','artifact_id':source_id,'query':'responsabilidade sem culpa','mode':'hybrid','top_k':1,'before':2,'after':4,'unit':'words'}
        run_request={'request_id':uid(),'registration_id':registration_id,'workflow_id':'request','input':input,'input_artifacts':[{'artifact_id':source_id,'sha256':sha(source)}],'confirm_local_execution':True}
        stage='run-retrieve'
        started=api('/api/plugins/run',run_request);job=poll_job(started['id'])
        pack,pack_bytes=artifact(job,'pack.json');verify_pack(pack);report,_=artifact(job,'retrieval.json')
        if report['search']['model']['provider']!='CPUExecutionProvider':raise RuntimeError('QA host did not run actual CPU model')
        repeated=api('/api/plugins/run',run_request)
        if repeated['id']!=job['id']:raise RuntimeError('Host idempotency changed job')
        records['retrieve']={'job_id':job['id'],'process_stopped':True,'exit_code':0,'pack_sha256':pack['sha256'],'model':report['search']['model'],'artifacts':job['plugin_result']['artifacts'],'repeated_request_same_job':True,'selected_entry':pack['payload']['entries'][0]}
        stage='upload-pack-and-expand'
        pack_id=uid();query=urllib.parse.urlencode({'request_id':pack_id,'project_id':project,'name':'pack.json','sha256':sha(pack_bytes)})
        api('/api/plugins/inputs?'+query,raw=pack_bytes)
        expand={'operation':'expand','artifact_id':source_id,'pack_artifact_id':pack_id,'reference':pack['payload']['entries'][0]['reference'],'before':5,'after':7,'unit':'characters'}
        request={'request_id':uid(),'registration_id':registration_id,'workflow_id':'request','input':expand,'input_artifacts':[{'artifact_id':source_id,'sha256':sha(source)},{'artifact_id':pack_id,'sha256':sha(pack_bytes)}],'confirm_local_execution':True}
        next_job=poll_job(api('/api/plugins/run',request)['id']);expanded,_=artifact(next_job,'pack.json');verify_pack(expanded)
        parent=pack['payload']['entries'][0];child=expanded['payload']['entries'][0]
        if expanded['payload']['documents'][0]['snapshot_id']!=pack['payload']['documents'][0]['snapshot_id'] or child['record']['parent_reference']!=parent['reference'] or child['record']['anchor_start']!=parent['record']['anchor_start'] or child['record']['anchor_end']!=parent['record']['anchor_end'] or child['record']['start']!=max(0,parent['record']['start']-5) or child['record']['end']<parent['record']['end']:raise RuntimeError('Host cross-job snapshot/anchor/expansion changed')
        records['independent_download_and_pack_hash_verification']=True
        records['expand']={'job_id':next_job['id'],'process_stopped':True,'exit_code':0,'pack_sha256':expanded['sha256'],'artifacts':next_job['plugin_result']['artifacts'],'selected_entry':expanded['payload']['entries'][0]}
        stage='revoke'
        api('/api/plugins/resources/grants/'+granted['request_id']+'/revoke',{'request_id':uid(),'registration_id':registration_id,'expected_fingerprint':candidate['fingerprint']})
        records['resource_revoked']=True
        stage='revoked-model-denial'
        denied_request=dict(run_request,request_id=uid())
        denied=poll_job(api('/api/plugins/run',denied_request)['id'],expected_status='failed')
        observed=json.loads((runtime/'plugin-jobs'/denied['id']/'out/failure.json').read_text(encoding='utf-8'))
        if observed['phase']!='model_grant' or (runtime/'plugin-jobs'/denied['id']/'out/result.json').exists():raise RuntimeError('QA revoked model grant was not refused before inference')
        records['revoked_model_grant_denied']={'job_id':denied['id'],'status':denied['status'],'process_stopped':denied['process_stopped'],'exit_code':denied['exit_code'],'phase':observed['phase'],'completed_result_absent':True}
        stage='logout' ;api('/api/operator/logout',{})
        records['operator_logged_out']=True;success=True
    except Exception as e:
        # Only sanitized stage/classification. Never serialize HTTP bodies/cookies.
        records['failure']={'stage':stage,'category':type(e).__name__,'reason':str(e) if str(e).startswith('QA ') else 'qualification did not complete'}
        raise RuntimeError('QA Foundry qualification failed at '+stage) from None
    finally:
        if process and process.poll() is None:process.terminate()
        if process:process.wait(timeout=20)
        records.update(schema_version=1,scope='Separate real QA host, fresh operator, selected inputs and actual grants; no production/app adoption',host_exe_sha256=file_sha(host),native_exe_sha256=file_sha(PACKAGE/'lib/retrieval-worker.exe'),complete=success,host_exit_observed=process is not None and process.poll() is not None,existing_runtime_reused=False,model_credential_output=False,production_adoption=False)
        # Cleanup is independent of receipt I/O, and frees the installed QA
        # model before publishing the small receipt on a contested volume.
        if owned_access and access_file and access_file.is_relative_to(local_known_folder()) and access_file.parent.name.startswith('FoundryOperatorAccess-'):
            access_file.unlink(missing_ok=True)
            try:access_file.parent.rmdir()
            except OSError:pass
        if root.parent==(ROOT/'tmp').resolve() and root.name.startswith('r08-host-'):shutil.rmtree(root)
        records['qa_runtime_removed']=not root.exists()
        destination=ROOT/'verification/r08-foundry-host.json'
        pending=destination.with_suffix('.pending.json')
        pending.write_text(json.dumps(records,ensure_ascii=False,indent=2)+'\n',encoding='utf-8')
        pending.replace(destination)
    print('Real isolated Foundry: discover/register/resource grant/CPU retrieval/expansion/download/idempotency/revoke passed')

if __name__=='__main__':
    process_start_wall=time.time()
    main()
