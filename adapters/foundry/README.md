# Retrieval Engine — adaptador Foundry v1

Pacote independente de recuperação sobre arquivos selecionados. Entrypoint Node
somente para o protocolo do host; implementação Rust em lib/retrieval-worker.exe.
Preparar com scripts/prepare_foundry_plugin.py após build explícito do worker.

Workflow request, input operation retrieve ou expand, schema schemas/request.json.
Input usa artifact_id, query/modo/contagens/unidade, ou pack_artifact_id/reference.
Nenhum path, recurso, raiz ou URL é aceito no input. Arquivos vêm de inputArtifacts
fora do input e precisam estar em job-root/inputs/artifactId, com bytes/SHA iguais.
PDF/HTML/texto passam pelo auxiliar Rust isolado; pack mantém Unicode scalar
[começo,fim), página e origem lógica por projeto/artefato. Outro projeto, outro
artefato ou versão não herda referências. Rede direta não é utilizada.

Modos literal e bm25 dispensam modelo. Hybrid/hybrid_prefilter exigem build CPU e
resources com resourceId retrieval-e5-small-int8, permission resource.read e mesmo
workspaceId do projectId. Modelo/ORT/tokenizer têm hashes fixados no motor. Recurso
separado em assets/e5-small-int8/foundry.resource.json; não copiar os pesos para
este pacote. Grants são emitidos/revalidados pelo host; JSON de job não é endpoint
autenticado. Código local confiado, sem sandbox de permissões de SO.

Saída: retrieval.json (achados/cobertura/proveniência/isolamento), pack.json quando
há evidência, result.json publicado por último com caminhos relativos/hashes.
Vazio não prova ausência; scores não são confiança nem validação jurídica.
Para expandir em outro job, selecionar o mesmo artifact_id da fonte e o pack
como pack_artifact_id; informar reference e contagens. Snapshot/páginas/quote/trilha
são revalidados; não ocorre GET. Mudar só o path temporário do job não muda origem.
Não há sessão compartilhada entre jobs: E5 é carregado uma vez por job sem serviço
residente externo. Para compartilhar o modelo entre agentes, usar ProcessClient.

Manifesto solicita capacidades, não as concede. Não há instalação, registro,
concessão, migração ou publicação automáticos. O host deve verificar fingerprint,
exit0, árvore parada, IDs, arquivos e hashes. Artefatos parciais/resultado ausente
exigem reconciliação, sem replay automático. Teste direto do CLI qualifica o envelope; a instância temporária real do Foundry
qualificou descoberta/registro/grant/execução/download/idempotência/revogação. Isso
não instala ou ativa o pacote no aplicativo de produção do usuário.


Limites do adaptador: input/query4KiB, até16 seleções de host, fonte8MiB,
JSONpack4MiB, top_k1..8, before/after0..4096, contexto8192 escalares por achado,
pack65536 escalares, job60s e timeout externo120s. Os limites individuais não
asseguram que toda combinação caiba no orçamento de contexto; exceder é erro.
Scratch da fonte existe só durante a extração isolada e é removido ao final.
Resultado sem achados é completed com entries vazio e pack_sha256 null, sem pack
fabricado. Em falha após validação do envelope, failure.json guarda apenas fase
e fingerprint do erro, sem texto/paths/diagnósticos do parser. O host só aceita
result.json junto de exit0, fingerprint corrente e árvore parada.

Preparação local explícita (Windows x64):

```powershell
cargo build --bin retrieval-worker --all-features --locked --offline
python scripts/prepare_foundry_plugin.py
python scripts/qualify_foundry_adapter.py
python scripts/qualify_foundry_adapter.py --official
```

A preparação confere suporte CPU/Foundry sem carregar modelo, mantém só o binário
corrente em lib/ e escreve o manifesto do recurso no diretório E5 já existente.
Não baixa/copía pesos/runtime nem concede acesso. O host deve selecionar/cadastrar
a pasta deste pacote e instalar/conceder o recurso separado pela sua própria API.

No envelope atual, origin/publicHttp são metadados do host em inputArtifacts;
ficam separados da proveniência lógica do motor e não concedem grants ou leitura.
A qualificação no host incluiu esse campo origin real, ausente em uma referência
anterior do contrato. Campos não conhecidos continuam recusados no input.
