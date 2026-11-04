"""Real incremental JSONL flow and independent schemas/hashes; --official fetches Camara once."""
import hashlib
import json
import queue
import subprocess
import sys
import threading
import time
from qualify_worker import ROOT, measure
from validate_retrieval_contract import Draft202012Validator


def canonical(value):
    return hashlib.sha256(json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(',',':')).encode('utf-8')).hexdigest()


def main():
    official = '--official' in sys.argv
    schema = json.loads((ROOT/'contracts/retrieval-response-v1.schema.json').read_text('utf-8'))
    validator = Draft202012Validator(schema)
    process = subprocess.Popen([str(ROOT/'target/debug/retrieval-worker.exe'), '--local-root', str(ROOT/'tests/fixtures')],
        cwd=ROOT, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        text=True, encoding='utf-8', creationflags=subprocess.CREATE_NO_WINDOW)
    checked = 0
    sequence = 0
    timings={}
    incoming=queue.Queue(maxsize=32)
    def reader():
        for line in process.stdout:incoming.put(line)
        incoming.put(None)
    threading.Thread(target=reader,daemon=True).start()

    def call(op, **fields):
        nonlocal checked, sequence
        sequence += 1
        request = {'schema_version':1, 'id':'i'+str(sequence), 'op':op, **fields}
        began=time.perf_counter()
        process.stdin.write(json.dumps(request)+'\n'); process.stdin.flush()
        # One outstanding request here. Every reply is checked, including polls/errors.
        line = incoming.get(timeout=45)
        elapsed=(time.perf_counter()-began)*1000
        timing=timings.setdefault(op,{'count':0,'total_ms':0.0,'max_ms':0.0})
        timing['count']+=1;timing['total_ms']+=elapsed;timing['max_ms']=max(timing['max_ms'],elapsed)
        assert line, 'worker exited before response'
        reply = json.loads(line)
        validator.validate(reply); checked += 1
        assert reply['id'] == request['id'], reply
        return reply

    def result(op, **fields):
        reply = call(op, **fields)
        assert reply['ok'], reply
        return reply['result']

    def verify_preview(view):
        assert view['provisional']
        assert view['reference'] == 'p_'+canonical(view['record'])
        assert hashlib.sha256(view['text'].encode('utf-8')).hexdigest() == view['record']['quote_sha256']
        assert len(view['text']) == view['record']['end']-view['record']['start']

    try:
        source = {'kind':'remote', 'url':'https://www2.camara.leg.br/legin/fed/lei/2018/lei-13709-14-agosto-2018-787077-normaatualizada-pl.pdf'} if official else {'kind':'local','path':str(ROOT/'tests/fixtures/progressive.pdf')}
        query = 'dados pessoais' if official else 'retrieval page 1.'
        began=time.perf_counter()
        opened = result('start_open', source=source); task = opened['task_id']
        partial = None
        for _ in range(2000):
            status = result('poll_open', task_id=task, max_events=2)
            assert status['state'] == 'extracting', status
            if status['coverage']['completed_pages']:
                peek = result('peek_open', task_id=task, query=query, top_k=1)
                if peek['entries']:
                    partial = status
                    first = peek['entries'][0]
                    break
            time.sleep(0.005)
        assert partial is not None, 'no partial match within bounded polls'
        first_match_ms=(time.perf_counter()-began)*1000
        verify_preview(first)
        assert not first['coverage']['end_of_source_known']
        expanded = result('expand_open', task_id=task, reference=first['reference'], before=2, after=100, unit='words')['entry']
        verify_preview(expanded)
        letters = result('expand_open', task_id=task, reference=expanded['reference'], before=3, after=2, unit='characters')['entry']
        verify_preview(letters)
        assert letters['record']['anchor_start'] == first['record']['anchor_start']
        for _ in range(2000):
            status = result('poll_open', task_id=task, max_events=64)
            if status['state'] == 'ready': break
            assert status['state'] == 'extracting', status
            time.sleep(0.001)
        else: raise AssertionError('no verified completion')
        verified_ms=(time.perf_counter()-began)*1000
        finished = result('finish_open', task_id=task, references=[letters['reference']])
        view = finished['bindings'][0]['final']
        assert not finished['provisional']
        assert view['evidence']['text'] == letters['text']
        assert view['evidence']['pages'] == letters['record']['pages']
        assert finished['provenance']['raw_sha256'] == first['record']['raw_sha256']
        assert view['record']['anchor_start'] == first['record']['anchor_start']
        assert len(finished['preview_lineage']) == 3
        pack = result('pack', session_id=finished['session_id'], references=[view['reference']])
        assert pack['sha256'] == canonical([pack['schema_version'],pack['payload']])
        # Cancelling a separate task cannot revoke the already committed session.
        other = result('start_open', source={'kind':'local','path':str(ROOT/'tests/fixtures/progressive.pdf')})['task_id']
        ack = result('cancel_open', task_id=other)
        assert ack['status'] == 'requested'
        for _ in range(1000):
            cancelled = result('task_status', task_id=other)
            if cancelled['state'] == 'cancelled': break
            time.sleep(0.001)
        else: raise AssertionError('cancellation not observed terminal')
        stale = call('peek_open', task_id=other, query='Synthetic')
        assert not stale['ok'] and stale['error']['code'] == 'task_cancelled'
        expanded_final = result('expand', session_id=finished['session_id'], reference=view['reference'], before=1, after=1, unit='words')
        result('close', session_id=finished['session_id'])
        caps = result('capabilities')
        assert caps['live_open_tasks'] == 0 and caps['reserved_open_chars'] == 0 and caps['active_sessions'] == 0
        memory=measure(process.pid)
        process.stdin.close(); assert process.wait(timeout=10) == 0
        print(json.dumps({'schema_version':1, 'actual_incremental_jsonl':True, 'official_source':official,
            'source_kind':'official public PDF' if official else 'synthetic 128-page fixture',
            'response_schemas_checked':checked, 'independent_preview_and_pack_hashes_verified':True,
            'observed_first_match_ms':first_match_ms,'observed_verified_ms':verified_ms,'rpc_roundtrip_ms':timings,
            'memory_parent':memory,'timing_scope':'observed local RPC arrival before Python schema validation; includes IPC, no controlled quality/latency benchmark',
            'partial_status':partial, 'first':first, 'word_context':expanded, 'character_context':letters,
            'finished':finished, 'pack':pack, 'cancel_ack':ack, 'cancel_terminal':cancelled,
            'other_session_preserved':expanded_final['record']['session_id']==finished['session_id'],
            'worker_exited':True, 'large_llm_calls':0, 'legal_validity_verified':False}, indent=2, ensure_ascii=False))
    finally:
        if process.poll() is None:
            process.kill(); process.wait(timeout=10)


if __name__ == '__main__':
    sys.stdout.reconfigure(encoding='utf-8')
    main()
