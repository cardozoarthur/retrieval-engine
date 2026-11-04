"""Opt-in actual public retrieval -> expand -> pack -> close over one JSONL worker."""
import hashlib
import json
import sys
import queue
import subprocess
import threading
from qualify_worker import ROOT, measure
from validate_retrieval_contract import Draft202012Validator


def main():
    validator = Draft202012Validator(json.loads((ROOT/'contracts/retrieval-response-v1.schema.json').read_text('utf-8')))
    process = subprocess.Popen([str(ROOT/'target/debug/retrieval-worker.exe'), '--model-dir', str(ROOT/'assets/e5-small-int8')],
        cwd=ROOT, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, encoding='utf-8', creationflags=subprocess.CREATE_NO_WINDOW)
    observations = []

    def call(identity, operation, **fields):
        request = {'schema_version': 1, 'id': identity, 'op': operation, **fields}
        process.stdin.write(json.dumps(request, ensure_ascii=False) + '\n')
        process.stdin.flush()
        received = queue.Queue(maxsize=1)
        threading.Thread(target=lambda: received.put(process.stdout.readline()), daemon=True).start()
        line = received.get(timeout=45)
        if not line:
            raise RuntimeError('Worker terminated without response')
        reply = json.loads(line)
        validator.validate(reply)
        assert reply['id'] == identity, reply
        observations.append({'id': identity, 'op': operation, 'ok': reply['ok'], 'os_memory': measure(process.pid)})
        return reply

    def result(identity, operation, **fields):
        reply = call(identity, operation, **fields)
        assert reply['ok'], reply
        return reply['result']

    try:
        initial = result('capabilities', 'capabilities')
        assert initial['cpu_compiled'] and initial['model_configured'] and not initial['model_loaded']
        assert initial['isolated_extraction']
        opened = result('open', 'open', source={'kind': 'remote', 'url':
            'https://www2.camara.leg.br/legin/fed/lei/2018/lei-13709-14-agosto-2018-787077-normaatualizada-pl.pdf'})
        session = opened['session_id']
        assert not opened['index_built']
        query = 'Quem deve reparar danos provocados por uso indevido de informações pessoais?'
        first = result('q1', 'search', session_id=session, query=query, mode='hybrid_prefilter', top_k=1)
        entry = first['entries'][0]
        words = result('words', 'expand', session_id=session, reference=entry['reference'], before=30, after=60, unit='words')
        chars = result('characters', 'expand', session_id=session, reference=words['reference'], before=128, after=256, unit='characters')
        assert chars['record']['anchor_start'] == entry['record']['anchor_start']
        assert chars['record']['anchor_end'] == entry['record']['anchor_end']
        assert chars['record']['parent_reference'] == words['reference']
        assert chars['evidence']['start'] <= words['evidence']['start']
        assert chars['evidence']['end'] >= words['evidence']['end']
        pack = result('pack', 'pack', session_id=session, references=[chars['reference']])
        # Independent cross-language checksum; integrity, not signature/authenticity.
        canonical = json.dumps([pack['schema_version'], pack['payload']], sort_keys=True,
            separators=(',', ':'), ensure_ascii=False).encode('utf-8')
        assert hashlib.sha256(canonical).hexdigest() == pack['sha256']
        assert len(pack['payload']['ancestors']) == 2
        assert hashlib.sha256(chars['evidence']['text'].encode('utf-8')).hexdigest() == chars['record']['quote_sha256']
        second = result('q2', 'search', session_id=session, query=query, mode='hybrid_prefilter', top_k=1)
        assert first['search']['model']['load_ms'] == second['search']['model']['load_ms']
        assert second['search']['model']['completed_batches'] > first['search']['model']['completed_batches']
        assert second['search']['cost']['native_inference_runs'] == 0
        assert second['search']['cost']['executed_model_tokens'] == 0
        assert second['search']['cost']['cache_hits'] > 0
        assert second['entries'][0]['evidence'] == first['entries'][0]['evidence']
        result('close', 'close', session_id=session)
        stale = call('stale', 'expand', session_id=session, reference=chars['reference'], before=1, after=1, unit='words')
        assert not stale['ok'] and stale['error']['code'] == 'unknown_session'
        closed = result('closed-capabilities', 'capabilities')
        assert closed['active_sessions'] == 0 and closed['model_loaded']
        process.stdin.close()
        assert process.wait(timeout=10) == 0
        receipt = {'schema_version': 1, 'actual_public_retrieval_worker': True,
            'source_open_operations': 1, 'provenance': opened['provenance'],
            'source_isolation': opened['isolation'],
            'first_search': first['search'], 'second_search': second['search'],
            'initial_reference': entry['reference'], 'word_context_reference': words['reference'],
            'character_context_reference': chars['reference'], 'evidence_pack': pack,
            'independent_python_pack_checksum_verified': True,
            'all_rpc_response_schemas_verified': True,
            'stale_reference_after_close': stale['error']['code'], 'worker_exited': True,
            'observations': observations, 'large_llm_calls': 0,
            'legal_validity_verified': False, 'retrieval_quality_benchmark': False}
        print(json.dumps(receipt, indent=2, ensure_ascii=False))
    finally:
        if process.poll() is None:
            process.kill()
            process.wait(timeout=10)


if __name__ == '__main__':
    sys.stdout.reconfigure(encoding='utf-8')
    main()
