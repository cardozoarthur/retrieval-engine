"""Summarize real reports without changing the frozen gold or selecting winners.

Uses repeat=1 for the table; both repeats remain in the receipts. First session
inference and acquisition/extraction/model load are reported separately there.
"""
import hashlib
import json
import shutil
import statistics
from pathlib import Path
from run_retrieval_benchmark import check_report

ROOT=Path(__file__).resolve().parents[1]
PROFILES=['e5-int8','e5-fp32','minilm-int8','minilm-fp32']

def main():
    dataset=(ROOT/'benchmarks/pt-official-pilot-v1.json').read_bytes()
    if hashlib.sha256(dataset).hexdigest()!='d7912c36dda5b0c817e9f969ec91abc925d670bdc94df687b338e74d1f0ea65b':
        raise RuntimeError('This summary documents frozen pilot v1; review a new dataset version explicitly')
    reports={p:json.loads((ROOT/'verification'/f'r07-benchmark-{p}.json').read_text(encoding='utf-8')) for p in PROFILES}
    if len({d['host_observation']['exe_sha256'] for d in reports.values()})!=1:raise RuntimeError('Reports use different runner executables')
    for d in reports.values():check_report(d,dataset)
    rows=[];latencies=[]
    for profile,d in reports.items():
        for fmt in ['fp32','int8','binary']:
            selected=[r for r in d['results'] if r['format']==fmt and r['repeat']==1]
            for mode in ['bm25','semantic','hybrid_rrf60']:
                ms=[next(m for m in next(x for x in r['rankings'] if x['mode']==mode)['metrics'] if m['k']==5) for r in selected]
                positive=[m for m in ms if m['relevant_anchors']]
                negative=[m for m in ms if not m['relevant_anchors']]
                rows.append({'profile':profile,'vector_format':fmt,'mode':mode,'positive_queries':len(positive),'negative_queries':len(negative),'recall_at_5':statistics.mean(m['recall'] for m in positive),'mrr_at_5':statistics.mean(m['reciprocal_rank'] for m in positive),'ndcg_at_5':statistics.mean(m['ndcg'] for m in positive),'negative_returned_any_rate':statistics.mean(m['negative_returned_any'] for m in negative)})
            latencies.append({'profile':profile,'vector_format':fmt,**{f'median_{field}':statistics.median(r[field] for r in selected) for field in ['tokenization_us','inference_us','pooling_us','query_encode_us','comparison_us','sorting_us','bm25_us','fusion_us']}})
    # BGE reference gate: graph + external tensor data + extra constant and
    # tokenizer as published at the audited revision; model not downloaded.
    bge_bytes=724923+2266820608+65552+17082821
    free=shutil.disk_usage(ROOT).free
    summary={'schema_version':1,'dataset_sha256':hashlib.sha256(dataset).hexdigest(),'hardware':json.loads((ROOT/'verification/r07-hardware.json').read_text(encoding='utf-8-sig')),'query_summary':'20 hand-curated positives, 4 no-evidence queries, no held-out evaluation','rows_repeat_1':rows,'latencies_repeat_1':latencies,'memory':{p:d['host_observation']['memory'] for p,d in reports.items()},'native_runs':{p:d['model']['inference_runs'] for p,d in reports.items()},'bge_m3':{'measured':False,'reason':'Published dense ONNX resources plus 1GiB workspace reserve exceed observed free disk','published_required_bytes':bge_bytes,'workspace_reserve_bytes':1024**3,'observed_free_bytes':free,'gate_satisfied':free>=bge_bytes+1024**3},'conclusion_scope':'Local pilot only. Do not promote a default or claim general legal/medical quality. Binary is uncalibrated zero-threshold sign packing.'}
    if summary['bge_m3']['gate_satisfied']:raise RuntimeError('BGE resource gate is now open; re-evaluate admission before finalizing R07')
    (ROOT/'verification/r07-benchmark-summary.json').write_text(json.dumps(summary,ensure_ascii=False,indent=2)+'\n',encoding='utf-8')
    lines=['# Benchmark R07 — piloto local real em CPU','',
        'Dataset congelado: `'+summary['dataset_sha256']+'`. Duas fontes oficiais em português (PDF Câmara/LGPD e HTML Ministério da Saúde/dengue), 20 consultas positivas e quatro sem evidência. Os textos completos extraídos, inclusive navegação HTML, formam o corpus de candidatos. São 1.278 janelas de 24 tokens lexicais, sobreposição de 12; todas passam na janela dos quatro perfis sem truncamento.',
        '', 'Cada positivo aponta uma única âncora literal com origem, intervalo Unicode e hash. Um resultado só conta se contiver a âncora inteira. Janelas sobrepostas não multiplicam o ganho. O schema v1 aceita zero ou uma âncora por consulta. Recall/MRR/nDCG são diagnósticos desse gold pequeno, não julgamento exaustivo de relevância nem qualidade de respostas. Não houve ajuste do gold após pontuar.',
        '', 'Hardware: '+summary['hardware']['Name']+', '+str(summary['hardware']['NumberOfLogicalProcessors'])+' processadores lógicos; dois threads ORT, uma linha por inferência, cache desligado. Cargo dev sem otimização Rust, debug=0/incremental=false; runtime nativo otimizado. Há carga externa e cache de páginas do SO sem controle. Tempos são observações locais, sem garantia universal de latência.',
        '', '| Perfil de pesos | Vetores | BM25 recall@5 | Semântica recall@5 | Híbrida recall@5 | Híbrida MRR@5 |', '|---|---|---:|---:|---:|---:|']
    for profile in PROFILES:
        for fmt in ['fp32','int8','binary']:
            def row(mode):return next(r for r in rows if r['profile']==profile and r['vector_format']==fmt and r['mode']==mode)
            lines.append(f"| {profile} | {fmt} | {row('bm25')['recall_at_5']:.2f} | {row('semantic')['recall_at_5']:.2f} | {row('hybrid_rrf60')['recall_at_5']:.2f} | {row('hybrid_rrf60')['mrr_at_5']:.3f} |")
    lines+=['','Tabela: segunda repetição, k=5, média sobre 20 positivos. Os recibos mantêm as duas repetições, métricas em k=1/3/5/10, rankings, checksums e custos. O supervisor Python recalcula as métricas a partir dos primeiros dez intervalos e confere todas as linhas/contadores. Híbrida usa RRF60 sobre ranking semântico completo e até 1.000 candidatos BM25; isso não é o orçamento padrão do worker.',
        '', '| Perfil | Pico working set do processo (MiB) | Mediana inferência de consulta (ms), vetores FP32 |', '|---|---:|---:|']
    for profile,d in reports.items():
        latency=next(l for l in latencies if l['profile']==profile and l['vector_format']=='fp32')
        lines.append(f"| {profile} | {d['host_observation']['memory']['peak_working_set_bytes']/1024**2:.1f} | {latency['median_inference_us']/1000:.3f} |")
    lines+=['','Working set é do processo do benchmark, com ORT dentro dele; não é tamanho dos pesos nem RAM do aplicativo inteiro. Commit e amostragem ficam separados nos recibos. Cada perfil fez 1.423 inferências nativas bem-sucedidas (uma primeira consulta, 1.278 passagens, 144 consultas sem cache). Aquisição, extração/construção do snapshot, índice, preflight de tokens, carregamento, primeira inferência, tokenização, pooling, conversão, comparação, ordenação e fusão têm medidas distintas. Preflight/load não são preemptíveis por RequestControl; o supervisor limita o processo próprio a 360s e o laço de inferência tem orçamento de 180s.',
        '', 'FP32 armazena 1.536 B/vetor; Int8, 384 códigos + 4 B de escala = 388 B; binário, 48 B. O formato RV01 acrescenta 9 B de cabeçalho por vetor. Não incluem texto, índice, alocador ou modelo. Int8 usa escala simétrica por vetor e cosseno dos códigos com normas recomputadas. Binário usa sinal, limiar zero e acordo de bits (1 − 2·Hamming/d), sem calibração aprendida. Pesos Int8 e esses formatos de vetores são escolhas independentes.',
        '', 'Resultado limitado: a fusão com vetores FP32/Int8 supera BM25 no recall@5 deste piloto; Int8 armazenado preserva o recall observado. Binário por sinais degrada muito o E5 e fica experimental. Todas as estratégias retornaram candidatos nas quatro consultas sem evidência: ranking não é detecção de ausência e score não é confiança. O piloto não sustenta promoção automática de modelo, redução de cobertura ou resposta jurídica/médica sem conferência.',
        '', f"BGE-M3 não foi medido. Recursos ONNX densos publicados exigem pelo menos {bge_bytes:,} B; com reserva de 1 GiB, excedem os {free:,} B livres observados. O grafo pequeno tem pesos externos de 2.266.820.608 B. A auditoria registra revisão/arquivos/hash/licença/pooling; não atribuímos qualidade ou velocidade não medidas. O runner atual compara E5/MiniLM; BGE exigirá driver CLS/1.024 dimensões e admissão de recursos.",
        '', 'Execução reproduzível (Windows x64, recursos locais pinados):', '', '```powershell', 'python scripts/prepare_e5.py', 'python scripts/prepare_benchmark.py all', 'cargo build --example retrieval_benchmark --features cpu-onnx --locked --offline', 'python scripts/run_retrieval_benchmark.py e5-int8 e5-fp32 minilm-int8 minilm-fp32', 'python scripts/summarize_retrieval_benchmark.py', '```', '', 'A preparação verifica capacidade/hash e não duplica runtime/tokenizer E5. Inferência não baixa modelos. Se a fonte remota mudar, o runner recusa o snapshot antes de indexar/inferir: revise/versione o dataset, sem adaptar silenciosamente o gold. Falha/cancelamento não admite JSON de sucesso. Os textos de origem são dados públicos consultados; presença no site não comprova vigência jurídica ou atualização clínica. Não houve integração externa Foundry nem publicação.']
    (ROOT/'docs/BENCHMARK.md').write_text('\n'.join(lines)+'\n',encoding='utf-8')
    print('Summarized four actual profiles; independent metrics verified; BGE resource gate closed')

if __name__=='__main__':main()
