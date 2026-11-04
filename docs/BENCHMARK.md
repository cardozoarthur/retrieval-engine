# Benchmark R07 — piloto local real em CPU

Dataset congelado: `d7912c36dda5b0c817e9f969ec91abc925d670bdc94df687b338e74d1f0ea65b`. Duas fontes oficiais em português (PDF Câmara/LGPD e HTML Ministério da Saúde/dengue), 20 consultas positivas e quatro sem evidência. Os textos completos extraídos, inclusive navegação HTML, formam o corpus de candidatos. São 1.278 janelas de 24 tokens lexicais, sobreposição de 12; todas passam na janela dos quatro perfis sem truncamento.

Cada positivo aponta uma única âncora literal com origem, intervalo Unicode e hash. Um resultado só conta se contiver a âncora inteira. Janelas sobrepostas não multiplicam o ganho. O schema v1 aceita zero ou uma âncora por consulta. Recall/MRR/nDCG são diagnósticos desse gold pequeno, não julgamento exaustivo de relevância nem qualidade de respostas. Não houve ajuste do gold após pontuar.

Hardware: 12th Gen Intel(R) Core(TM) i7-12650H, 16 processadores lógicos; dois threads ORT, uma linha por inferência, cache desligado. Cargo dev sem otimização Rust, debug=0/incremental=false; runtime nativo otimizado. Há carga externa e cache de páginas do SO sem controle. Tempos são observações locais, sem garantia universal de latência.

| Perfil de pesos | Vetores | BM25 recall@5 | Semântica recall@5 | Híbrida recall@5 | Híbrida MRR@5 |
|---|---|---:|---:|---:|---:|
| e5-int8 | fp32 | 0.35 | 0.40 | 0.45 | 0.308 |
| e5-int8 | int8 | 0.35 | 0.40 | 0.45 | 0.304 |
| e5-int8 | binary | 0.35 | 0.10 | 0.15 | 0.125 |
| e5-fp32 | fp32 | 0.35 | 0.45 | 0.50 | 0.285 |
| e5-fp32 | int8 | 0.35 | 0.45 | 0.50 | 0.285 |
| e5-fp32 | binary | 0.35 | 0.10 | 0.30 | 0.267 |
| minilm-int8 | fp32 | 0.35 | 0.45 | 0.50 | 0.388 |
| minilm-int8 | int8 | 0.35 | 0.45 | 0.50 | 0.388 |
| minilm-int8 | binary | 0.35 | 0.45 | 0.50 | 0.371 |
| minilm-fp32 | fp32 | 0.35 | 0.45 | 0.50 | 0.379 |
| minilm-fp32 | int8 | 0.35 | 0.45 | 0.50 | 0.379 |
| minilm-fp32 | binary | 0.35 | 0.40 | 0.50 | 0.364 |

Tabela: segunda repetição, k=5, média sobre 20 positivos. Os recibos mantêm as duas repetições, métricas em k=1/3/5/10, rankings, checksums e custos. O supervisor Python recalcula as métricas a partir dos primeiros dez intervalos e confere todas as linhas/contadores. Híbrida usa RRF60 sobre ranking semântico completo e até 1.000 candidatos BM25; isso não é o orçamento padrão do worker.

| Perfil | Pico working set do processo (MiB) | Mediana inferência de consulta (ms), vetores FP32 |
|---|---:|---:|
| e5-int8 | 503.2 | 4.529 |
| e5-fp32 | 1118.3 | 10.374 |
| minilm-int8 | 505.0 | 4.832 |
| minilm-fp32 | 1113.6 | 9.242 |

Working set é do processo do benchmark, com ORT dentro dele; não é tamanho dos pesos nem RAM do aplicativo inteiro. Commit e amostragem ficam separados nos recibos. Cada perfil fez 1.423 inferências nativas bem-sucedidas (uma primeira consulta, 1.278 passagens, 144 consultas sem cache). Aquisição, extração/construção do snapshot, índice, preflight de tokens, carregamento, primeira inferência, tokenização, pooling, conversão, comparação, ordenação e fusão têm medidas distintas. Preflight/load não são preemptíveis por RequestControl; o supervisor limita o processo próprio a 360s e o laço de inferência tem orçamento de 180s.

FP32 armazena 1.536 B/vetor; Int8, 384 códigos + 4 B de escala = 388 B; binário, 48 B. O formato RV01 acrescenta 9 B de cabeçalho por vetor. Não incluem texto, índice, alocador ou modelo. Int8 usa escala simétrica por vetor e cosseno dos códigos com normas recomputadas. Binário usa sinal, limiar zero e acordo de bits (1 − 2·Hamming/d), sem calibração aprendida. Pesos Int8 e esses formatos de vetores são escolhas independentes.

Resultado limitado: a fusão com vetores FP32/Int8 supera BM25 no recall@5 deste piloto; Int8 armazenado preserva o recall observado. Binário por sinais degrada muito o E5 e fica experimental. Todas as estratégias retornaram candidatos nas quatro consultas sem evidência: ranking não é detecção de ausência e score não é confiança. O piloto não sustenta promoção automática de modelo, redução de cobertura ou resposta jurídica/médica sem conferência.

BGE-M3 não foi medido. Recursos ONNX densos publicados exigem pelo menos 2,284,693,904 B; com reserva de 1 GiB, excedem os 2,363,478,016 B livres observados. O grafo pequeno tem pesos externos de 2.266.820.608 B. A auditoria registra revisão/arquivos/hash/licença/pooling; não atribuímos qualidade ou velocidade não medidas. O runner atual compara E5/MiniLM; BGE exigirá driver CLS/1.024 dimensões e admissão de recursos.

Execução reproduzível (Windows x64, recursos locais pinados):

```powershell
python scripts/prepare_e5.py
python scripts/prepare_benchmark.py all
cargo build --example retrieval_benchmark --features cpu-onnx --locked --offline
python scripts/run_retrieval_benchmark.py e5-int8 e5-fp32 minilm-int8 minilm-fp32
python scripts/summarize_retrieval_benchmark.py
```

A preparação verifica capacidade/hash e não duplica runtime/tokenizer E5. Inferência não baixa modelos. Se a fonte remota mudar, o runner recusa o snapshot antes de indexar/inferir: revise/versione o dataset, sem adaptar silenciosamente o gold. Falha/cancelamento não admite JSON de sucesso. Os textos de origem são dados públicos consultados; presença no site não comprova vigência jurídica ou atualização clínica. Não houve integração externa Foundry nem publicação.
