"""Opt-in public PDF + actual hybrid CPU CLI; observed OS memory, not a RAM cap."""
import json
import subprocess
import threading
from qualify_worker import ROOT, measure


def main():
    source = 'https://www2.camara.leg.br/legin/fed/lei/2018/lei-13709-14-agosto-2018-787077-normaatualizada-pl.pdf'
    args = [str(ROOT/'target/debug/retrieval-engine.exe'), source,
        'Quem deve reparar danos provocados por uso indevido de informações pessoais?',
        '10', '20', 'words', 'hybrid-prefilter']
    process = subprocess.Popen(args, cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, encoding='utf-8', creationflags=subprocess.CREATE_NO_WINDOW)
    stop = threading.Event()
    observations = []

    def monitor():
        while not stop.is_set():
            try:
                observations.append(measure(process.pid))
            except OSError:
                pass
            stop.wait(0.05)

    thread = threading.Thread(target=monitor, daemon=True)
    thread.start()
    try:
        stdout, stderr = process.communicate(timeout=90)
        if process.returncode != 0:
            raise RuntimeError(f'Hybrid CLI failed: {stderr}')
        response = json.loads(stdout)
        coverage = response['search']['coverage']
        assert coverage['semantic_chunks'] == 16 and coverage['omitted_chunks'] > 0
        assert not coverage['complete_index'] and response['results']
        assert all(item['hit']['pages'] for item in response['results'])
        assert response['search']['model']['provider'] == 'CPUExecutionProvider'
        if not observations:
            raise RuntimeError('No live OS memory observation')
        receipt = {'schema_version': 1, 'actual_public_hybrid_cli': True,
            'provenance': response['provenance'], 'search': response['search'],
            'sample_interval_ms': 50, 'observed_peak_working_set_bytes': max(x['peak_working_set_bytes'] for x in observations),
            'memory_is_sampled_not_hard_limit': True, 'worker_exited': True,
            'returned_hits': len(response['results']), 'large_llm_calls': 0,
            'juridical_quality_validated': False}
        print(json.dumps(receipt, indent=2))
    finally:
        stop.set()
        if process.poll() is None:
            process.kill()
            process.wait(timeout=10)
        thread.join(timeout=2)


if __name__ == '__main__':
    main()
