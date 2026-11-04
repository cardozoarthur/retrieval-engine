# Consumo Rust e armazenamento de evidência derivada

R08 concluída. APIs abaixo foram implementadas e qualificadas localmente;
o pacote Foundry passou também em instância QA real. O núcleo não
depende de IADV ou de bancos consumidores e não altera seus registros canônicos.

## Um processo residente por escopo do host (Windows)

`process_client::{ClientConfig, ProcessClient}` inicia somente o executável
selecionado pelo host, sem shell, com stdin/stdout JSONL e stderr descartado.
`program`, `prefix_args`, `local_roots` e `model_dir` são configuração confiável,
nunca argumentos de ferramenta fornecidos pelo modelo. O ambiente é reduzido a
PATH/SystemRoot/WINDIR/PATHEXT/TEMP/TMP; não se encaminham tokens do aplicativo.
O executável é código local confiado; esta API não autentica/instala esse código.

```rust,ignore
let client = ProcessClient::spawn(ClientConfig {
    program: worker_binary,
    local_roots: permitted_roots,
    model_dir: approved_model_dir, // None: literal/BM25
    ..Default::default()
})?;
let session = client.open(source, Duration::from_secs(30))?;
let hits = session.search("responsabilidade", SearchMode::Bm25, 3,
                          Duration::from_secs(5))?;
let wider = session.expand(&hits.entries[0].reference, 8, 16, Unit::Words,
                           Duration::from_secs(5))?;
let verified = session.verified_pack(&[wider.reference], Duration::from_secs(5))?;
session.close(Duration::from_secs(5))?;
let stopped = client.shutdown();
assert!(stopped.exit_observed && stopped.tree_exit_observed);
```

O cliente é clonável para agentes do mesmo escopo autorizado. As sessões mantêm
esse cliente vivo, referenciam uma origem/snapshot e conferem query/hash, quote,
página, intervalo e referência. Não compartilhar uma instância entre titulares
sem controle de acesso no host. IDs não são credenciais. O transporte não concede
leitura de banco/fonte nem transfere política de autorização para o documento.

Limites: até oito pedidos de trabalho pendentes, dois slots separados para
cancel/status, canal de escrita limitado, até4096 trabalhos no ciclo padrão
(configurável até65536), frame de resposta4MiB (1KiB..16MiB), pedidos128KiB.
Management continua disponível quando acaba o orçamento de trabalhos. IDs são
monotônicos por instância e não são reciclados; não há restart ou replay automático.
Reader separado correlaciona respostas fora de ordem; envelope/version/id/forma
inválidos, EOF e excesso de frame encerram o transporte e falham os pendentes.
`submit(Operation, timeout)` dá acesso ao protocolo já existente, incluindo
extração incremental/cancelamento; operações tipadas adicionais estão em
ClientSession. O resultado bruto de submit é Value, não validação completa de cada
payload: usar os métodos tipados e os schemas do protocolo ao atravessar outra
fronteira. Request/response schemas não mudaram nesta etapa.

Wait local usa deadline de1..300000ms, incluindo a fila do cliente; o prazo no
worker inclui a fila dele e só reduz o limite do host. Timeout local é resultado
incerto, não cancelamento observado. Conserve request.id() e use Cancel/
RequestStatus para reconciliar. Não repetir um pedido automaticamente. Um pedido
que expirar localmente pode ainda terminar; sua resposta é drenada, sem crescer o
histórico. A criação/tokenização/modelo e callbacks mantêm os limites já declarados
em INTEGRATION.md; não há alegação de preempção universal.

`shutdown` termina o Job Object privado (incluindo helpers) e espera o filho;
retorna ClientShutdown com exit_observed, exit_code, tree_processes_before_stop e
tree_exit_observed. A árvore é consultada por até2s após encerramento. Falha de
observação fica explícita; is_closed indica somente transporte fechado. Drop do
último cliente também encerra/espera seus processos e threads, sem matar PIDs de
outros aplicativos. O guard é kill-on-close e não é sandbox de permissões. Cliente
de processo atualmente compilado/qualificado somente no Windows; a biblioteca
embutida e o contrato de store continuam independentes dessa implementação.

## Banco existente ou SQLite derivado separado

`evidence_store::{VerifiedEvidence, EvidenceStore, EvidenceReceipt}` não exige
SQLite. Um banco Rust pode implementar o trait EvidenceStore com sua própria
transação, usando EvidenceReceipt::for_write para preparar recibo vinculante e
publicando pack/recibo juntos. Namespace é definido pelo host no construtor do
adaptador; não aceitar seleção de organização/namespace pelo texto da consulta.

VerifiedEvidence é criado por `against_snapshots(pack, &[&Document])`, que replaya
quotes/páginas/expansões, ou por ClientSession::verified_pack, cujo worker confiado
verificou contra o snapshot retido antes de exportar. O recibo distingue
snapshot_replay de trusted_worker_export. Isso registra a fronteira de confiança,
não assinatura, autenticação da fonte ou autoridade jurídica. Ler um recibo antigo
não refaz a verificação contra a fonte atual.

Feature `sqlite-store` fornece SqliteEvidenceStore usando rusqlite0.37.0 pinado e
SQLite bundled como dependência de compilação necessária; nenhum serviço/installer
é instalado. `create(path, namespace, limits)` exige arquivo novo e banco dedicado
(application_id0x52455631,user_version1). `open` recusa outros formatos. Não passe
o caminho do banco canônico. O schema tem somente retrieval_evidence; namespace
fixo +write_id é chave primária. put com mesma identidade/pack/método devolve o
recibo exato; conteúdo ou método diferente conflita e não sobrescreve. Writes
serializam numa transação SQLite imediata; reads usam snapshot transacional e
conferem tamanho antes de carregar BLOB, pack/hash/recibo/namespace/chave depois.
Não há migração de banco consumidor, fallback de autoridade ou adoção canônica.

StoreLimits padrão:1024 registros,4MiB por pack,64MiB de payloads somados no banco.
São bytes contabilizados de payload, não cap físico do arquivo SQLite, índices,
recibos ou RSS. Limites máximos e chaves são validados; JSON adulterado e hashes
inconsistentes falham. Configurar namespace/limites em outro processo continua
responsabilidade do host; o arquivo não é endpoint autenticado. Busy timeout2s e
synchronous FULL não representam teste de perda de energia ou tolerância a toda
falha de filesystem. A implementação não faz retries implícitos de outcomes incertos.

```powershell
cargo build --bin retrieval-worker --example rust_consumer --features cpu-onnx,sqlite-store --locked --offline
# SOURCE pode ser caminho concedido ou URL HTTPS pública; DB deve ser novo.
cargo run --example rust_consumer --features sqlite-store --locked --offline -- WORKER SOURCE QUERY NEW_DB
# Incluir TRUSTED_MODEL_DIR como quinto argumento escolhe hybrid explicitamente.
python scripts/qualify_rust_consumer.py
python scripts/qualify_rust_consumer.py --official
```

O exemplo usa um namespace demonstrativo fixo, amplia as referências, verifica o
pack, grava/fecha/reabre o SQLite e confirma encerramento observado do worker.
A qualificação cria/apaga somente um DB temporário próprio, confere-o read-only
com Python e recalcula hashes de pack/recibo/referências/quotes. Fluxo oficial
usou PDF da Câmara/LGPD e E5 real, sem LLM grande ou publicação.

## Pedir mais contexto depois de fechar o processo

Preferir ClientSession::expand enquanto o snapshot estiver retido: não há GET
novo. Fechar/TTL perde a expansão via handle de worker, embora o pack permaneça
inspecionável. Host que reteve os bytes originais pode reconstruir os Documents
com a mesma identidade e usar EvidenceSession::restore(&pack, documents, limits).
A restauração exige mesma ordem/fontes/versões e reexecuta quotes, páginas e cada
expansão ancestral antes de aceitar os refs. Ordinais continuam sem colisão e a
âncora original permanece. Origem diferente, bytes alterados, página forjada ou
limites menores são recusados. Não refaz aquisição ou busca rede automaticamente.

Restore é continuação offline explícita, não ressurreição de uma sessão no worker,
concessão de acesso ou merge concorrente de branches. A fonte só é extraída de
novo se o host fornecer os mesmos bytes; não é necessário persistir índice.
Recibos e packs são artefatos derivados, não registros canônicos do banco.

## Foundry qualificado

Referência de contrato somente leitura em verification/r08-foundry-contract-reference.json.
O contrato inspecionado exige Node local, manifesto/schema/fingerprint, arquivos
em inputArtifacts e recursos em resources fora de input. Rede externa direta não
é suportada nesse plugin v1; o host pode selecionar bytes adquiridos por seu próprio
canal. O pacote foi descoberto/registrado/executado com recursos e arquivos selecionados
em host QA próprio. Downloads, idempotência, expansão entre jobs, revogação e saída
foram conferidos. Não significa adoção no aplicativo do usuário. Pacote e comandos
em adapters/foundry/README.md, evidências em r08-foundry-host.json e auditoria em
COMPLETION.md.
