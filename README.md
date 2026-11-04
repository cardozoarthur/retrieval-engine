# Retrieval Engine

Novo motor Rust independente, genérico, sem dependência de IADV ou Foundry.
## Instalação

Ambiente validado: Windows x64, Rust 1.96 e ferramentas de compilação MSVC.
O primeiro build baixa as dependências fixadas em Cargo.lock.

```powershell
cargo build --locked --bins
cargo test --locked --all-features
cargo run --locked -- README.md "motor" 5 8 words bm25
```

A inferência em CPU requer Python 3 para executar `python scripts/prepare_e5.py`
e o recurso Cargo `cpu-onnx`. O adaptador Foundry usa Node.js. A
[documentação técnica](docs/COMPLETION.md) descreve arquitetura, validações e limites.
O código tem licença [MIT](LICENSE). Os recursos externos mantêm suas licenças.
Os recibos em verification/ são produzidos localmente pelos scripts de qualificação.

## Uso atual

Carrega TXT/MD/CSV, HTML ou PDF textual local e HTTPS público sob demanda,
busca expressões literais e amplia contexto antes/depois por palavras ou
caracteres. Retorna JSON com URL/caminho solicitado e final, SHA256 dos bytes e
do texto extraído, snapshot, offsets Unicode, páginas PDF e avisos de cobertura.
Não exige índice prévio, não chama LLM e não grava fontes automaticamente.

```powershell
cargo test --locked --offline
cargo run --locked --offline -- README.md "motor" 5 8 words
cargo run --locked --offline -- tests/fixtures/two-pages.pdf "responsabilidade" 5 15 words
cargo run --locked --offline -- tests/fixtures/two-pages.pdf "RESPONSABILIDADE culpa" 5 15 words bm25
cargo run --locked --offline --example official_sources
cargo run --locked --offline --example lexical_qualification -- --official
```

CLI: `retrieval-engine SOURCE QUERY [BEFORE=20] [AFTER=20] [chars|words] [literal|bm25|hybrid|hybrid-prefilter] [MODEL_DIR]`.
Primeiro build requer dependências Rust de Cargo.lock; depois funciona offline
para fontes locais. O exemplo official_sources faz dois GET reais opt-in na
Câmara; testes normais não dependem de rede. Recibo real em
[verification/r01-official-current.json](verification/r01-official-current.json).

Na biblioteca, `Document::load(Source, Limits)` adquire a fonte; `find` retorna
evidências; `expand(&evidence, before, after, Unit, max_chars)` amplia no mesmo
snapshot sem rede, inclusive entre páginas. Evidência de outra versão/fonte ou
com texto adulterado é rejeitada. Manter o Document vivo entre chamadas para
expansões posteriores. A CLI é uma execução local; o worker abaixo mantém sessões
para chamadas posteriores.

O modo `bm25` cria índice invertido temporário na própria consulta, em janelas
de 192 tokens com sobreposição de 32. Retorna até dez chunks, score e termos
encontrados; o score serve para ranking, não é probabilidade. Usa NFKD, remoção
de marcas Unicode e caixa baixa; não remove stopwords/negações nem aplica
stemming. Números são tokens inteiros, 42 não casa 142. Palavras de uma query
não precisam aparecer juntas ou na mesma ordem. Identificadores com pontuação
são fragmentados; vizinhos sobrepostos podem aparecer mais de uma vez.
Os offsets e o texto retornado continuam sendo os originais extraídos.

Biblioteca: `LexicalIndex::build(Vec<Arc<Document>>, ChunkConfig, IndexLimits)`
constrói o índice; `search(query, top_k)` retorna RankedHit com Evidence ampliável
no Document original. Não há escrita no banco/disco. Limites padrão: 64
documentos, 20 mil chunks, 2 milhões de tokens originais, 128 MiB em unidades
contabilizadas (incluindo fonte e tokens temporários); isso não mede nem limita
RSS real. Consultas limitadas a 4096 bytes/128 tokens; termos a 1024 caracteres.
Normalização pode aproximar palavras distintas; não é recuperação semântica.

## Contratos e limites

Intervalos [início,fim) em valores escalares Unicode, não bytes/grafemas. Busca
literal sensível a caixa/acentos, não sobreposta e pode casar parte de palavra.
Palavras são separadas por whitespace e preservam pontuação. HTML normaliza
espaços/linhas, ignora scripts/estilos/atributos hidden e marca texto tachado com
`[deleted]...[/deleted]`. CSS/JavaScript não são avaliados; não se certifica
vigência. Páginas/offsets referem-se ao texto extraído, não coordenadas visuais.
PDF tem ordem de leitura heurística, sem OCR. Páginas vazias são sinalizadas;
documento sem texto, criptografado ou com erro de extração é rejeitado.

Padrões: 8 MiB por fonte, 2 milhões de caracteres, 500 páginas, 30 segundos de
rede incluindo DNS/redirecionamentos, 5 redirecionamentos; CLI retorna até 100
achados e 16.384 caracteres por contexto. Excesso dá erro, sem truncamento
silencioso da fonte. HTTPS só porta 443/endereço público, sem cookies/proxy;
DNS validado/fixado e cada redirect revalidado. TLS não é desabilitado.
Resposta comprimida, status diferente de 200 ou MIME desconhecido são rejeitados.

`Document::load` e o CLI de consulta única executam parsers no próprio processo:
limites de bytes/texto não são um teto de RAM. No worker JSONL Windows, `open`
agora usa auxiliar com Job Object (limite de commit256MiB, kill-on-close e um
processo), antes de entregar a fonte ao parser. DNS/HTML/PDF ficam nesse auxiliar.
Falhar ao criar/associar o job rejeita a operação; não há fallback in-process.
Isso isola recursos/cancelamento, sem token de segurança restrito ou promessa de
sandbox de permissões. Escopo/limites detalhados abaixo; não é endpoint público
autenticado.

## Extração isolada e progresso no SDK Windows

`ExtractionTask::start(program, source, limits, isolation, control)` inicia o
`retrieval-worker --extract-helper` confiável do host, em janela oculta. O filho
aguarda handshake em stdin; só adquire bytes depois de associado ao job e confere
seus limites. Se o pai morrer antes de enviar entrada, o filho recebe EOF; depois
da associação, fechar o último handle encerra o job. Somente processos criados
pelo motor são controlados, sem API de kill por PID arbitrário.

`IsolationConfig`: memória comprometida32..512MiB (padrão256), saída total até
128MiB (padrão16), evento até1MiB (padrão256KiB), mínimos de1KiB. São tetos
distintos de RSS e da memória do modelo residente. No CLI Windows, host pode usar
`--extraction-memory-mib N`; consumidores Rust configuram `WorkerConfig` e
`extraction_helper:Some(program)`. Worker Rust embutido mantém `None` por padrão;
isso usa extração in-process explicitamente. A implementação Job Object/Task foi
validada no Windows; outros alvos não anunciam esse isolamento.

`next_event(wait)` retorna Source(rawSHA/origem), Chunk(texto/página/offset),
Complete(metadados) ou erro. Chunks têm até8192 escalares e fila interna de2eventos.
`provisional_text`/`completed_pages` permitem observar progresso sem criar evidência
final. PDFs podem precisar do arquivo inteiro para xref; progresso é por página
extraída, não promessa de streaming de rede. `Document::load_observed` oferece
callback semelhante in-process, com as limitações de recursos desse caminho.

`finish()` só entrega Document/IsolationReport após evento terminal válido,
EOF, término real do filho e conferência de rawSHA original, texto, páginas e
snapshot. Complete ou exit0 isoladamente não bastam: kill-on-close do Windows
pode retornar0. Erro/cancelamento invalida prefixo; descarte encerra e aguarda o
filho, inclusive se o leitor estiver sob backpressure. O host ainda deve admitir
referências usando `control.commit` na operação completa. `open` já faz isso e
inclui `isolation` com limite/pico de commit do auxiliar, bytes de saída e término
confirmado. O modelo E5 fica no pai e não é carregado no auxiliar.

O JSONL Windows também oferece recuperação provisória. `incremental_extraction`
acompanha a disponibilidade do auxiliar isolado configurado pelo host.
Documentação nativa: [limites de commit e kill-on-close](https://learn.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-jobobject_basic_limit_information),
[consulta do job imediato](https://learn.microsoft.com/en-us/windows/win32/api/jobapi2/nf-jobapi2-queryinformationjobobject).

## Buscar enquanto a fonte é extraída

```json
{"schema_version":1,"id":"s1","op":"start_open","source":{"kind":"remote","url":"https://example.org/document.pdf"}}
{"schema_version":1,"id":"p1","op":"poll_open","task_id":"TASK_ID","max_events":2}
{"schema_version":1,"id":"q1","op":"peek_open","task_id":"TASK_ID","query":"dados pessoais","top_k":1}
{"schema_version":1,"id":"e1","op":"expand_open","task_id":"TASK_ID","reference":"PROVISIONAL_REF","before":20,"after":40,"unit":"words"}
{"schema_version":1,"id":"e2","op":"expand_open","task_id":"TASK_ID","reference":"NEXT_PROVISIONAL_REF","before":50,"after":80,"unit":"characters"}
{"schema_version":1,"id":"p2","op":"poll_open","task_id":"TASK_ID","max_events":64}
{"schema_version":1,"id":"f1","op":"finish_open","task_id":"TASK_ID","references":["SELECTED_PROVISIONAL_REF"]}
```

Substituir IDs pelos retornados. O exemplo é uma sequência de comandos: repetir
poll até ter texto e depois até `state:ready`. Cada comando de trabalho precisa
de ID novo. Poll consome até1..64eventos; `task_status` observa sem consumir eventos.
Os estados são extracting/ready/cancel_requested/cancelled/expired/failed/finalized
(expire_requested é transitório). Nenhum índice prévio é exigido.

`peek_open` é literal, sensível a caixa/acentos e com até32achados; modos
BM25/híbrido continuam na sessão final. Em PDF, usa somente páginas completas já
recebidas. Resultado vazio tem cobertura de prefixo, sem certificar ausência no
restante. `coverage` informa páginas totais conhecidas, recebidas, sem texto,
caracteres disponíveis e confirmação do snapshot inteiro. Página sem texto/OCR
ausente é diferente de hash/paginação conferidos. `after_limited_by_progress`
indica contexto que pode crescer ao receber mais páginas.

Referências provisórias começam com `p_`, têm task/rawSHA/origem/offsets/páginas/
hash da quote e mantêm âncora e histórico imutáveis. Não são referências de
Evidence Pack. Falha posterior do PDF, cancelamento ou TTL invalidam todas elas.
`finish_open` só aceita tarefa ready e até32referências selecionadas. Confere
mesma versão/página/quote e cria uma sessão final atomicamente com bindings para
referências R05; erro em uma seleção não cria sessão parcial. A vinculação mantém
o intervalo exato. O registro final usa expansão por caracteres para reproduzir
esse intervalo; `preview_lineage` conserva pedidos por palavras/caracteres feitos
durante o progresso. Para ampliar depois, usar `expand` da sessão final.

`cancel_open {task_id}` e `task_status {task_id}` passam pelo leitor de controle,
inclusive com E5 ocupado em outra sessão. Cancel responde requested/
already_requested/too_late e work_finished; aguardar estado terminal para observar
descarte/saída do filho. Cancelar tarefa já finalizada não revoga a sessão criada;
usar close. Callers Rust podem clonar `worker.open_task_controller()` antes de
mover o worker para outra thread. IDs são correlação, não autorização.

Padrões:2tarefas vivas,16registros (terminais podem ser descartados),64referências
por tarefa, TTL ocioso300s e deadline de extração30s. Host configura
`--max-open-tasks`, `--open-task-ttl-ms`, `--open-task-timeout-ms` ou OpenTaskConfig.
O prazo de `start_open` cobre aceitação/inicialização; a extração aceita usa sua
própria lease. Cancelar start antes da aceitação encerra o filho ainda não admitido.
Tarefas reservam slots de sessão/caracteres no orçamento global antes de iniciar;
ao ficarem ready, reserva cai para texto real. Finish transfere a reserva para a
sessão; falha/cancel/TTL/drop libera dados, refs e filho. EOF drena comandos e
descarta tarefas restantes. Os limites são de dados/commit por filho, não RSS
total do motor/modelo. Poll deve avançar a extração dentro da lease configurada.

Validação reproduzível: `python scripts/qualify_incremental.py` (fixture128p)
e `python scripts/qualify_incremental.py --official` (PDF público Câmara/LGPD).
Ambos conferem todos os schemas e hashes de referências/pack independentemente.

## Sessões e contexto solicitado depois do achado

```powershell
cargo run --features cpu-onnx --locked --offline --bin retrieval-worker -- --model-dir assets/e5-small-int8 --local-root tests/fixtures
```

O processo lê JSONL de stdin e devolve uma resposta por linha. Não abre porta,
instala daemon ou grava consultas. Sem feature CPU, funciona com literal/BM25;
sem `--local-root`, só admite HTTPS público. A raiz local é permissão concedida
pelo host na inicialização e não pode ser habilitada por uma consulta.

Envie `open` com uma fonte ainda não indexada; use os identificadores retornados
nas chamadas seguintes, substituindo os placeholders do exemplo:

```json
{"schema_version":1,"id":"o","op":"open","source":{"kind":"local","path":"tests/fixtures/two-pages.pdf"}}
{"schema_version":1,"id":"q","op":"search","session_id":"SESSION_ID","query":"responsabilidade culpa","mode":"hybrid_prefilter","top_k":1}
{"schema_version":1,"id":"e1","op":"expand","session_id":"SESSION_ID","reference":"REFERENCE","before":30,"after":60,"unit":"words"}
{"schema_version":1,"id":"e2","op":"expand","session_id":"SESSION_ID","reference":"NEW_REFERENCE","before":128,"after":256,"unit":"characters"}
{"schema_version":1,"id":"p","op":"pack","session_id":"SESSION_ID","references":["LAST_REFERENCE"]}
{"schema_version":1,"id":"c","op":"close","session_id":"SESSION_ID"}
```

Para URL, use `source:{"kind":"remote","url":"https://..."}`. `expand` amplia o
contexto anterior, mantém a âncora original e cria referência filha; não troca o
trecho antigo nem faz novo GET. Pode atravessar páginas. Trocar o arquivo/site
depois de open não altera o snapshot em memória. `close`/EOF descarta a sessão;
referência antiga é erro, inclusive após reiniciar. O modelo permanece carregado
entre consultas e sessões até o worker terminar. Não iniciar simultaneamente o
worker de embeddings se isso duplicar o modelo sem necessidade.

`capabilities` informa os modos compilados/configurados e se o modelo já foi
carregado. Configurado não significa validado antes da carga. `id` apenas
correlaciona respostas; não deduplica operações. IDs de sessão/referência não
são credenciais ou autorização entre usuários: o broker consumidor deve aplicar
seus escopos. O worker é serial; cancelamento nativo rígido continua pendente.
Sessões têm TTL de inatividade de 300s, configurável pelo host com
`--session-ttl-ms N` (1..3.600.000). Atividade admitida renova o prazo; trabalho
em execução não expira a própria sessão. Expiração descarta texto/índice/refs e
retorna `unknown_session`, sem reabrir a fonte. O worker limpa a cada 100ms
ocioso, além das fronteiras de requisição. Na biblioteca, chamar `maintenance()`
periodicamente para liberação ociosa; consultas sempre verificam expiração.
`capabilities` expõe contadores de expiração, inclusive a ociosa, e caracteres
retidos. Fila limitada a oito frames mais um trabalho ativo. Saturação rejeita
novos trabalhos com `resource_limit`, mantendo o leitor disponível para cancel.
Admitidos conservam FIFO; erros imediatos e ACKs podem chegar antes deles.
Correlacionar por `id`, não pela ordem das linhas. Uma resposta bloqueada no stdout também bloqueia manutenção:
o consumidor deve drenar respostas ou encerrar seu processo.

O E5 mantém cache LRU de vetores em memória, com orçamento contabilizado de
16MiB e TTL absoluto de 300s; `CpuConfig.cache_max_bytes/cache_ttl_ms` são opções
do host na biblioteca (zero desabilita, máximos 64MiB/1h). Nenhum texto de entrada
é guardado no cache, apenas chave SHA256, vetor e metadados de tokens. A chave
inclui modelo/tokenizador/runtime, pooling, perfil CPU, janela, prefixo e texto
exato. Cache compartilhado permanece após fechar uma sessão, até TTL, expulsão
ou término do worker; não persiste em disco. `maintenance()` libera entradas
expiradas enquanto ocioso. Packed batching desabilita esse cache: a quantização
dinâmica pode depender das outras linhas do tensor. Resultados mantêm tokens
planejados para orçamento (`model_tokens`), distinguindo `executed_model_tokens`,
`cache_hits/cache_misses` e inferências nativas reais. Bytes contabilizados não
são limite de RSS e excluem as cópias de saída já previstas no orçamento híbrido.

Biblioteca: `EvidenceSession::new`, `register_many`, `expand`, `resolve`, `export`.
Guarda o texto extraído uma vez por Arc e apenas offsets/hashes nas revisões, materializando
quotes quando necessário. Limites do worker: quatro sessões, oito milhões de
caracteres retidos no total, 256 referências por sessão, 16.384 caracteres por
contexto, 32 entradas/65.536 caracteres por pack e 128 KiB por linha de entrada.
São limites de dados, não teto de RSS do runtime. Admissão de referências em lote
é atômica; não deixa resultados parciais se um item falhar.

Evidence Pack é JSON exportável com origem, hashes bruto/textual, páginas,
intervalos, quotes e cadeia de expansões. `verify_integrity` verifica consistência
e hashes; `verify_against` compara quotes/páginas e reexecuta expansões no mesmo
Document. Hash não é assinatura, autenticação do site ou prova de vigência. O pack
verifica as evidências selecionadas, não a completude do ranking. Persistir o
JSON permite inspecionar quotes depois; não preserva uma sessão de expansão ao
reiniciar. A trilha guarda hashes/offsets dos predecessores, sem copiar a fonte.

Canonicalização: SHA256 do JSON UTF-8 compacto de `[schema_version,payload]`, com
chaves de objetos ordenadas e sem normalizar texto. A referência é SHA256 do
registro canônico. Não há números de ponto flutuante nesses payloads. Recibo com
checksum independente Python e fluxo real em fonte oficial:
[r05-retrieval-worker-current.json](verification/r05-retrieval-worker-current.json).

Contratos de [requisição](contracts/retrieval-v1.schema.json) e
[resposta](contracts/retrieval-response-v1.schema.json); erros têm código estável,
mensagem limitada e não ecoam fonte/URL/segredos. Desenvolvimento opcional usa
jsonschema4.26.0 isolado em `.tools/schema-validator`, sem modificar Python global:

```powershell
python -m pip --isolated install --no-cache-dir --index-url https://pypi.org/simple --target .tools/schema-validator jsonschema==4.26.0
python scripts/validate_retrieval_contract.py
python scripts/qualify_retrieval.py
```

O último comando é opt-in: exige binary CPU já construído, recursos instalados e
faz uma aquisição real de PDF da Câmara. Python é bootstrap/QA, não dependência
do motor Rust nem do worker em execução.

## Busca híbrida

```powershell
cargo run --features cpu-onnx --locked --offline -- tests/fixtures/two-pages.pdf "Quem responde pelo prejuízo?" 10 20 words hybrid
cargo run --features cpu-onnx --locked --offline -- tests/fixtures/two-pages.pdf "responsabilidade culpa" 10 20 words hybrid-prefilter
cargo run --features cpu-onnx --locked --offline --example hybrid_qualification -- --official
```

`hybrid` avalia todas as janelas indexadas, inclusive sem termos comuns com a
consulta, e combina ranks BM25/E5 por RRF `1/(60+rank)` em cada canal. Scores BM25
e cossenos são retornados separadamente, sem tratá-los como probabilidades.
`hybrid-prefilter` na CLI limita candidatos lexicais/semânticos a 16 para reduzir
CPU; pode perder paráfrases sem palavras em comum. A saída informa janelas
avaliadas/omitidas e `complete_index`. Essa cobertura se refere às janelas de
tokens, não certifica OCR, fonte bruta completa ou vigência. Páginas PDF sem
texto são contabilizadas mesmo quando o índice foi integralmente analisado.

Full que excede orçamento falha explicitamente. Não muda para busca parcial ou
BM25 silenciosamente. O pré-check usa o tokenizador do modelo; janela que não
cabe é rejeitada, mesmo com allow_truncation no embedder. Use ChunkConfig menor
no SDK. Limites padrão do SDK: 128 chunks semânticos, 65.536 tokens reais
(query/prefixos inclusos), 512 KiB de texto dos candidatos, 1 MiB para payload
FP32 de query+um batch e deadline de 30s. A memória do runtime/tensores
intermediários é separada desse payload. A busca híbrida agora usa timer do pedido
e `RunOptions.terminate` para interromper inferência ORT em curso; não é preempção
instantânea de qualquer chamada nativa. Parsers e inicialização do modelo ainda
exigem controles próprios. Limiar cosseno opcional não é calibrado
como confiança. Sem limiar, E5 pode retornar candidatos para perguntas alheias.

API: `hybrid_search(&index, &mut model, query, &HybridOptions, reranker)`.
Índice e modelo podem permanecer vivos para novas consultas; a busca não carrega
modelo nem persiste vetores. `Reranker` é uma extensão local opcional, sem modelo
aprendido instalado por padrão: recebe candidatos imutáveis e scores por posição;
resultado inválido falha, sem substituir texto/origem. Apenas o grupo limitado é
reordenado e a cauda não avaliada fica identificável. Callback é código confiável
do consumidor, sem sandbox interno ou reranker remoto fornecido pelo motor.

Cancelamento no SDK: criar `RequestControl::new(Duration)` (positivo, até300s),
compartilhar um clone com o chamador e usar `hybrid_search_with_control` ou
`E5Embedder::embed_with_control`. `cancel()` retorna `requested`,
`already_requested` ou `too_late`; solicitar não comprova término nativo.
`checkpoint()` observa cancelamento/deadline. Um timer por pedido sinaliza o hook
ORT sem depender de polling do chamador. O controle deve ser exclusivo do pedido;
operação híbrida pode encurtar seu deadline ao orçamento dela.

`RetrievalWorker::handle_json_with_control(bytes, &control)` liga o controle à
criação/expansão de referências, abertura e fechamento de sessões. A admissão usa
`control.commit(...)`: cancelar e admitir serializam sob o mesmo lock. Se cancelar
ganhar, não são registradas referências; se a admissão ganhar, cancelar é
`too_late`. O callback de commit é curto, sem I/O bloqueante e atômico ao falhar.
`finish()` é para trabalho já encerrado e acorda seu timer; o worker o chama ao
retornar. Descartar o último proprietário também libera o timer. Cache/índice
derivados válidos podem sobreviver a uma busca cancelada, sem referências parciais.

No SDK controlado, erros de pedido são `cancelled`/`deadline_exceeded`.
`ModelInfo.inference_attempts/interrupted_runs/inference_runs` distingue tentativas,
erros nativos sob cancelamento/deadline e execuções concluídas. Um hit do cache
não é inferência. Inicialização do modelo fixo é conferida nas bordas; parsers e
DNS chamados in-process e callbacks de reranker confiáveis não têm preempção
seletiva. No worker Windows, extração fica no auxiliar cancelável descrito acima.
O binário JSONL recebe `cancel` e `request_status` fora da fila de execução:

```json
{"schema_version":1,"id":"cancel-q1","op":"cancel","request_id":"q1"}
{"schema_version":1,"id":"status-q1","op":"request_status","request_id":"q1"}
```

Cancel responde `{request_id,status,work_finished}`. `requested` é um ACK: aguardar
também a resposta final do trabalho `q1`. `work_finished` registra término
observado pelo dispatcher, não recebimento da resposta pelo consumidor.
`request_status` devolve `state:queued|running|finished` e
`native_interrupt_armed`; hook armado não comprova término nem progresso em
percentual. `unknown_request` significa não admitido neste processo. ID de cancel
ou status deve ser diferente dos IDs de trabalho. Só é possível cancelar pedidos
do próprio pipe; o broker consumidor continua responsável por autorização.

IDs de trabalho admitidos não podem ser reutilizados durante a vida do processo,
inclusive após erro/término; repetição gera `duplicate_request_id`. O registro
retém apenas IDs/estado final, sem resultados/texto. Orçamento padrão4096IDs,
host `--max-request-ids N` (1..65536); `capabilities` informa total admitido e
limite. Ao esgotar, novos trabalhos são rejeitados, mas cancel/status continuam
disponíveis. Exportar packs/fechar sessões antes do limite, ou fechar stdin para
drenar os admitidos e liberar o processo; nova instância perde snapshots/refs.
Pedidos rejeitados por fila cheia não reservam ID e podem ser reenviados.
Manter também IDs de gerenciamento únicos na correlação do consumidor.

Host `--request-timeout-ms N` (1..300000, padrão30000) inclui tempo na fila.
Trabalhos podem fornecer `timeout_ms` (1..300000), que só reduz o teto do host;
não o amplia nem reinicia o prazo depois da fila. Cancel/status não aceitam esse
campo. Isso permite preparar a sessão e aplicar um prazo menor só à consulta.
Deadline vence antes da admissão de referências; carga do modelo e caminhos
in-process ainda têm checagens de borda nas etapas não preemptíveis. `capabilities.jsonl_cancel:true`
confirma o transporte; `hard_native_cancellation:false` continua declarando que
preempção instantânea de todos os caminhos nativos, inclusive carga do modelo,
não é uma garantia desta API.

Validação real: PDF oficial de 28 páginas, 81 janelas/22.552 tokens analisados em
~14,9s de busca Full; modo rápido 16 janelas/4.479 tokens em ~2,1s, omitindo 65.
Tempos observados em perfil dev/CPU local, excluem aquisição/carga do modelo e
não constituem comparação controlada ou garantia de latência. Pico observado
do processo rápido ~503 MiB (amostragem Windows). Recibos:
[Full](verification/r04-hybrid-current.json) e
[prefilter/CLI/memória](verification/r04-prefilter-current.json).
Fixture portuguesa sem tokens em comum retornou vazio no BM25 e evidência
pertinente no Full E5; isso não valida precisão jurídica ou superioridade geral.

## E5 Small CPU opcional

Instalação explícita de uma única cópia dos recursos (bootstrap atual Windows
x64, aproximadamente 148 MB finais; sem ZIP/SDK duplicado):

```powershell
python scripts/prepare_e5.py
cargo run --features cpu-onnx --locked --offline --example e5_qualification
cargo run --features cpu-onnx --locked --offline --bin retrieval-embeddings -- assets/e5-small-int8
```

O worker lê um JSON por linha em stdin e responde por linha em stdout:

```json
{"schema_version":1,"id":"consulta-1","kind":"query","texts":["Quem responde pelo prejuízo?"]}
```

Use `kind: "passage"` para trechos. A sessão é carregada uma vez antes do loop,
reutilizada até EOF; um broker consumidor pode compartilhar esse processo.
O retrieval-worker fornece sessões de documentos; não há servidor HTTP nem
descoberta automática entre agentes.
Erros de requisição têm `ok:false`; linha acima de 128 KiB encerra o worker.
Respostas preservam id e incluem vetores, tempos, contagens de tokens,
truncamentos e metadados do modelo. Não gravar consultas automaticamente.

Biblioteca: `E5Embedder::load(dir, CpuConfig)` e `embed(InputKind, &[String])`.
Sessão mutável serial; consumidores concorrentes podem compartilhar via Mutex.
Prefixos E5 são aplicados pelo motor, com pooling médio mascarado e L2.
Resultados têm 384 dimensões FP32 normalizadas; Int8 descreve os **pesos** ONNX,
não armazenamento quantizado dos vetores. Limites padrão: duas threads CPU,
quatro textos por solicitação, 16 KiB por texto e 512 tokens incluindo prefixos/
especiais. Exceder a janela é erro por padrão. `--allow-truncation` permite
corte explícito com flags/tokens usados; não representa cobertura completa.

Quantização dinâmica pode variar conforme os outros textos no tensor. O padrão
executa microbatches unitários sem recarregar a sessão, preservando consistência
por trecho; `--packed-batching` é opt-in e pode alterar levemente os vetores.
Não misturar políticas ao criar/comparar índices. A latência não é garantida.
Pico real observado em solicitações curtas: ~495 MiB de working set no Windows,
incluindo runtime/tokenizador; não confundir com os 118 MB dos pesos. Ver
[recibo CPU](verification/r03-e5-cpu-current.json) e
[worker/memória](verification/r03-worker-current.json). Não são benchmark
comparativo nem prova de relevância jurídica.

Pesos: conversão Xenova do intfloat/multilingual-e5-small, revisão
761b726dd34fb83930e26aab4e9ac3899aa1fa78; licença original declarada MIT.
Hashes dos pesos, tokenizador e DLLs são conferidos antes da carga. Runtime
Microsoft ONNX Runtime 1.22.0 CPU via ort 2rc10; feature não baixa binários na
compilação ou inferência. `assets/` não é versionado; instalador verifica TLS/
hash e mantém só os recursos necessários. Default build não depende de ONNX.

Verificação explícita com recursos reais instalados:

```powershell
cargo test --features cpu-onnx --locked --offline
cargo test --features cpu-onnx --test semantic --locked --offline -- --ignored
cargo test --features cpu-onnx --test cli actual_worker --locked --offline -- --ignored
python scripts/qualify_worker.py
```

Benchmark comparativo local: [resultados, escopo e reprodução](docs/BENCHMARK.md).
R07 comparou E5/MiniLM com pesos FP32/Int8 e vetores FP32/Int8/binários em CPU
real. Dataset piloto congelado, 20 positivos + 4 negativos; não estabelece
qualidade geral nem promove outro modelo automaticamente. BGE-M3 foi auditado,
sem medição por orçamento de recursos. Adaptadores Rust/SQLite e Foundry concluídos; ver docs/COMPLETION.md. Descoberta/crawler não é fornecido pela aquisição de uma URL.
Nenhum resultado sintético é
apresentado como inferência semântica ou validação jurídica.


A biblioteca expõe `StoredVector::encode`, `similarity`, `to_bytes/from_bytes`
para os formatos `VectorFormat::{Fp32, Int8, Binary}`. RV01 é um payload derivado
versionado; não contém texto ou proveniência e não substitui Evidence Pack. Não
comparar modelos/receitas diferentes só porque têm a mesma dimensão. Int8 usa
escala por vetor (388 B/384 dimensões), além do cabeçalho de 9 B. Binário por sinais
é experimental; o benchmark mostrou perda importante no E5. O worker corrente
continua produzindo vetores FP32 normalizados e usando o E5 Int8 pinado; os perfis
comparativos são opt-in em `benchmark_model`, sem parâmetro de modelo vindo de
uma consulta não confiável.


## Integração Rust e Foundry (R08)

[API de processo residente, armazenamento e exemplos](docs/RUST_ADAPTER.md).
Cliente Windows com correlação/cancelamento/budgets e confirmação de término;
EvidenceStore genérico, SQLite dedicado opcional (`sqlite-store`) e restauração
explícita de pack contra os mesmos snapshots para ampliar contexto depois.
Qualificados localmente com worker Rust, banco real e PDF oficial/E5. Pacote
Foundry e integração em instância QA real concluídos; sem adoção automática em aplicação.


[Auditoria da entrega e limites](docs/COMPLETION.md):111 testes distintos completos,
qualificação Node/Rust em fonte oficial e host Foundry QA real. Pacote em
[adapters/foundry](adapters/foundry/README.md), com modelo como recurso separado.
O projeto está concluído no escopo local Windows x64; instalação no aplicativo e
publicação permanecem externas a essa qualificação. Para economizar disco, modelos
auxiliares do benchmark já medidos não ficam no runtime; recibos/pins/scripts
permitem preparação explícita e reprodução quando houver capacidade.
