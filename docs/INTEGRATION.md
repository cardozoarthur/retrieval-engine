# Contrato corrente de consumo — projeto independente

Raiz: `.`. Biblioteca Cargo
`retrieval-engine`, feature opcional `cpu-onnx`. Não depende de fontes IADV,
Foundry ou banco canônico e não concede permissões de consumidor.

Capacidades qualificadas até R06: aquisição local/HTTPS de TXT/HTML/PDF textual;
SHA256 bruto/texto e snapshot; busca literal/BM25/híbrida E5; paginação e
expansão em memória antes/depois por palavras/escalares Unicode. Recursos E5 e
ORT fixados sob `assets/e5-small-int8` (~148MB), uma única cópia; `MODEL_DIR`
é configuração confiável do host, nunca parâmetro de consulta não confiável.

CLI: `target/debug/retrieval-engine.exe SOURCE QUERY BEFORE AFTER chars|words
literal|bm25|hybrid|hybrid-prefilter [MODEL_DIR]`. É execução única, JSON schema1
em stdout; stderr/exit1 em falha. Para híbrida, construir com cpu-onnx. Consulta
não baixa modelo, não usa LLM grande, não grava fonte ou altera banco.

Processo residente de recuperação: `target/debug/retrieval-worker.exe
[--model-dir DIR] [--local-root DIR ...] [--session-ttl-ms N]
[--request-timeout-ms N] [--max-request-ids N]`. JSONL schema1/id/op:
capabilities/open/search/expand/pack/close/cancel/request_status e
start_open/poll_open/peek_open/expand_open/finish_open/cancel_open/task_status.
Contratos em contracts/retrieval-v1.schema.json
e contracts/retrieval-response-v1.schema.json; fluxos completos no README e
verification/r05-retrieval-worker-current.json. Open não exige índice prévio;
search constrói índice derivado quando preciso e E5 é carregado sob demanda
uma vez. Mode literal/bm25 sem feature CPU; hybrid/hybrid_prefilter com feature
e recursos configurados. Referências imutáveis preservam âncora/snapshot entre
chamadas; pack verificável inclui a trilha, sem persistir a fonte automaticamente.

Worker alternativo só para vetores: `target/debug/retrieval-embeddings.exe
MODEL_DIR`, JSONL schema1/id/kind(query|passage)/texts. Não iniciar ambos se isso
duplicar o modelo sem necessidade. O worker de recuperação já mantém sua sessão
E5 compartilhada entre consultas/fontes. Não houve instalação de serviço/tray.

Expansão não refaz GET: deve usar Document do snapshot que produziu a evidência.
Fechar o processo perde esse estado nesta versão. `complete_index` na híbrida é
cobertura de janelas, não vigência/validade jurídica. Não persistir índices como
registros canônicos nem tratar evidência de benchmark como admissão/integração.
O pack persistido é inspecionável offline; hashes não autenticam origem nem
vigência. Verificar contra o snapshot para validar quotes/expansões. IDs são
identificadores, não credenciais; permissão entre agentes/usuários é do broker.

R06: cache LRU/TTL de vetores e TTL de sessões com manutenção ociosa
implementados; detalhes, limites e métricas no README. Host embutido deve chamar
`RetrievalWorker::maintenance()` quando ocioso. Fechar sessão não limpa vetores
do cache compartilhado do modelo; TTL/expulsão/fim do processo os liberam.
SDK: RequestControl compartilhável, embed_with_control/hybrid_search_with_control
e handle_json_with_control permitem cancelar pedido em thread própria. ORT usa
RunOptions.terminate; admissão de referências/sessões usa commit serializado com
cancelamento. ACK requested não significa término observado. Timers têm deadline
positivo até300s e são liberados por finish/descarte. O processo JSONL recebe
cancel/request_status no leitor separado e anuncia jsonl_cancel=true; fila8+ativo1,
saturação com erro imediato, admitidos FIFO. ACK/status podem ultrapassar respostas
de trabalho: correlacionar por id e aguardar terminal do alvo. Registro de IDs
admitidos (default4096) não recicla durante a vida do processo; esgotar exige
liberar o processo, enquanto cancel/status permanecem disponíveis. Apenas IDs/
estado terminal retidos, sem resultados históricos. Configuração e detalhes no
README. `timeout_ms` opcional de trabalho apenas reduz o prazo do host, incluindo
fila; cancel/status não aceitam esse campo. Inicialização do modelo e callbacks
confiáveis continuam com checagens de borda; hard_native_cancellation=false.

Worker JSONL Windows usa o próprio executável como auxiliar de extração, sem
carregar E5 no filho. Job Object:256MiB de commit padrão (host32..512),1processo,
kill-on-close; handshake só enviado após associação e conferido pelo filho.
Falha do guard não faz fallback in-process. Open retorna isolation com limite/
pico de commit, bytes de saída e término confirmado. Não é cap de RSS do pai nem
sandbox de permissões. Biblioteca embutida usa in-process salvo configuração
explícita de extraction_helper/IsolationConfig; implementação Job/Task validada
somente no Windows.

SDK ExtractionTask emite origem/rawSHA e chunks/páginas provisórias; finish
exige terminal/EOF/exit e hashes/paginação iguais antes de materializar Document.
Falha/cancel invalida prefixo; drop encerra/aguarda só o filho próprio. Capabilities
isolated_extraction/incremental_extraction=true no native Windows. JSONL start/
poll/peek/expand/finish mantém namespace provisório p_, coverage explícita,
cancel/status fora da fila e vincula quote/página/rawSHA ao snapshot final sem
aquisição nova. Peek é literal; BM25/E5 na sessão final.2tarefas vivas/16registros/
64refs por tarefa, TTL300s/deadline30s por padrão; reservas globais e término
confirmado antes de liberar. Preview_lineage é retorno de vinculação, separado
da trilha canônica em caracteres do Evidence Pack. Detalhes/limites no README.
R08 entrega adaptadores Rust/SQLite e pacote Foundry qualificado em host QA;
nenhuma admissão ou execução no backend Foundry/IADV foi qualificada por esses
testes. Consumidor deve controlar um processo, roots locais e permissões;
sem root concedida o worker só aceita HTTPS público. Não aceitar model_dir ou
grants de raiz vindos de uma consulta. Sem callbacks para alterar banco canônico.

Não iniciar outro backend/model worker em paralelo sem dono/limites definidos
pelo consumidor. Se um broker compartilhar o processo entre agentes/usuários,
autorização, escopo de fontes e isolamento pertencem também ao host; JSONL local
não é endpoint público autenticado. Nenhum daemon/tray/serviço foi instalado.


R07 acrescenta codecs de payload RV01 e um driver separado de benchmark pinado
(E5/MiniLM FP32/Int8, mesma DLL ORT). Não muda o contrato JSONL nem o modelo do
worker. `docs/BENCHMARK.md` distingue o piloto completo de janelas e a fusão
RRF60 de todos os candidatos do orçamento menor do worker. Um banco consumidor
deve guardar modelo/receita junto de vetores, fonte/snapshot junto de evidências;
o cabeçalho RV01 sozinho só identifica formato/dimensões, não autoriza comparar
modelos. Não houve conexão ou admissão externa Foundry.


R08 concluída: process_client no Windows fornece ProcessClient/ClientSession e pedido
assíncrono correlacionado sobre JSONL. EvidenceStore e feature sqlite-store fazem
persistência derivada por namespace em banco dedicado; nenhuma operação nova foi
exposta ao JSONL para escrever banco. EvidenceSession::restore revalida snapshots/
trilha antes de continuar expansão offline. Instruções e limites em RUST_ADAPTER.md.
Cli Rust consumidor e SQLite real foram qualificados, inclusive PDF oficial/E5,
com hashes lidos por Python independente. Isso não é execução do pacote num host
Foundry. O pacote e uma instância QA real do host passaram, conforme R08.md e COMPLETION.md.


Estado final: pacote Node mínimo/native Rust em adapters/foundry, modelo em recurso
separado, sem rede direta no workflow. Host QA real autenticou operador novo,
conferiu fingerprint/grants/seleções, executou CPU e expansão e recusou modelo
revogado. Recibo r08-foundry-host.json e completion-audit.json. Nenhuma adoção em
produção ou alteração de código IADV/Core é declarada. Rodar audit_completion.py
reconfere o pacote atual e as evidências; testes não dependem dos modelos opcionais
de benchmark removidos após medição. A fonte E5/runtime corrente foi preservada.
