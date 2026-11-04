"""Opt-in real JSONL ORT cancellation; synthetic PDF, independent response schemas."""
import json
import queue
import subprocess
import threading
import time
from qualify_worker import ROOT, measure
from validate_retrieval_contract import Draft202012Validator


def main():
    validator = Draft202012Validator(json.loads((ROOT/'contracts/retrieval-response-v1.schema.json').read_text('utf-8')))
    process = subprocess.Popen([str(ROOT/'target/debug/retrieval-worker.exe'), '--model-dir',
        str(ROOT/'assets/e5-small-int8'), '--local-root', str(ROOT/'tests/fixtures')],
        cwd=ROOT, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        text=True, encoding='utf-8', creationflags=subprocess.CREATE_NO_WINDOW)
    incoming = queue.Queue(maxsize=32)
    replies = []
    pending = {}

    def reader():
        for line in process.stdout:
            incoming.put(line)
        incoming.put(None)

    threading.Thread(target=reader, daemon=True).start()

    def send(identity, operation, **fields):
        process.stdin.write(json.dumps({'schema_version':1, 'id':identity, 'op':operation, **fields})+'\n')
        process.stdin.flush()

    def receive():
        line = incoming.get(timeout=45)
        assert line is not None, 'worker exited before terminal response'
        reply = json.loads(line)
        validator.validate(reply)
        replies.append(reply)
        return reply

    def until(identity):
        while identity not in pending:
            reply = receive()
            assert reply['id'] not in pending, 'unexpected duplicate response'
            pending[reply['id']] = reply
        return pending.pop(identity)

    def call(identity, operation, **fields):
        send(identity, operation, **fields)
        return until(identity)

    try:
        source = {'kind':'local', 'path':str(ROOT/'tests/fixtures/two-pages.pdf')}
        opened = call('open', 'open', source=source)
        assert opened['ok'], opened
        sid = opened['result']['session_id']
        warm = call('warm', 'search', session_id=sid, query='prova', mode='hybrid', top_k=1)
        assert warm['ok'], warm
        send('active', 'search', session_id=sid, query='qzx'*160, mode='hybrid', top_k=1)
        for poll in range(100):
            status = call('poll-'+str(poll), 'request_status', request_id='active')
            assert status['ok'] and status['result']['state'] != 'finished', status
            if status['result']['native_interrupt_armed']:
                break
            time.sleep(0.001)
        else:
            raise AssertionError('native hook not observed')
        for number in range(8):
            send('queued-'+str(number), 'capabilities')
        saturated = call('over', 'capabilities')
        assert not saturated['ok'] and saturated['error']['code'] == 'resource_limit', saturated
        queued = call('queued-state', 'request_status', request_id='queued-7')
        assert queued['result']['state'] == 'queued', queued
        queued_ack = call('cancel-queued', 'cancel', request_id='queued-7')
        assert queued_ack['result']['status'] == 'requested' and not queued_ack['result']['work_finished']
        active_ack = call('cancel-active', 'cancel', request_id='active')
        assert active_ack['result']['status'] == 'requested', active_ack
        terminal = until('active')
        assert not terminal['ok'] and terminal['error']['code'] == 'cancelled', terminal
        for number in range(8):
            result = until('queued-'+str(number))
            if number == 7:
                assert not result['ok'] and result['error']['code'] == 'cancelled', result
            else:
                assert result['ok'], result
        reused = call('reuse', 'search', session_id=sid, query='prova', mode='hybrid', top_k=1)
        assert reused['ok'], reused
        assert reused['result']['entries'][0]['record']['ordinal'] == 2
        assert reused['result']['search']['model']['load_ms'] == warm['result']['search']['model']['load_ms']
        assert reused['result']['search']['model']['interrupted_runs'] == 1
        late = call('late', 'cancel', request_id='active')
        assert late['result']['status'] == 'too_late' and late['result']['work_finished']
        unknown = call('unknown', 'cancel', request_id='over')
        assert not unknown['ok'] and unknown['error']['code'] == 'unknown_request'
        call('close', 'close', session_id=sid)
        memory = measure(process.pid)
        process.stdin.close()
        assert process.wait(timeout=10) == 0
        print(json.dumps({'schema_version':1, 'actual_native_jsonl_cancel':True,
            'fixture':'synthetic two-pages.pdf, not a real law', 'all_response_schemas_verified':True,
            'native_interrupt_observed':True, 'cancelled_reference_admissions':0,
            'queue_saturation_verified':True, 'resident_model_reused':True, 'os_memory':memory,
            'worker_exited':True, 'replies':replies,
            'scope':'ORT cancellation and cooperative boundaries; parser isolation/incremental extraction pending'},
            indent=2, ensure_ascii=False))
    finally:
        if process.poll() is None:
            process.kill()
            process.wait(timeout=10)


if __name__ == '__main__':
    main()
