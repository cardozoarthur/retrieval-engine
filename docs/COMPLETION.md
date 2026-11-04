# Auditoria de conclusão — Retrieval Engine

Motor novo e genérico em `.`, independente
por construção. Implementação Rust, executáveis CLI/JSONL e pacote externo Foundry.
Leis/normas em sites oficiais são casos de uso/teste; não há tipos/regras jurídicos
no núcleo nem interpretação de vigência. Qualificação atual: Windows x64/CPU.

## Requisitos confrontados com evidência

| Capacidade entregue | Evidência executada/currente | Limite da conclusão |
|---|---|---|
| R00: achado e mais palavras/caracteres antes/depois | tests/context.rs; tests/evidence.rs; evidence_restore.rs | caracteres = escalares Unicode, palavras = whitespace |
| R01: TXT/HTML/PDF local e HTTPS, origem/hash/página | acquisition/isolation tests; r06-incremental-official.json; r08-rust-consumer-official.json | PDF textual; sem OCR ou crawler; URL explícita |
| R02: BM25 temporário determinístico | lexical tests, benchmarks e index_built=false antes de search | normalização não altera offsets/quotes; limites explícitos |
| R03: E5 Small local CPU residente | semantic/benchmark_model/process_client opt-in reais; runtime/model hashes | pesos Int8, saída FP32; sem download em inferência/sem garantia universal de latência |
| R04: lexical/semântica/RRF e reranker opcional | hybrid tests, 4 perfis reais e fixtures negativas | reranker é callback confiado opcional, não modelo treinado padrão; cobertura declarada |
| R05: pack e expansão verificáveis | evidence/evidence_restore tests; Python independente no consumo Rust/Foundry | SHA é integridade, não autenticação/verdade jurídica |
| R06: consulta incremental, TTL/cache/cancel/budgets | native_retrieval/control/incremental/isolation tests; execução pública atual | primeira página de28 antes do fim; preview literal; sem índice prévio |
| R07: E5 vs MiniLM, pesos/vetores/CPU/RAM | r07-benchmark-summary.json e4reports,5.692inferências reais | piloto20positivos+4negativos; BGE-M3 auditado, não medido; sem promoção automática |
| R08: consumidor/banco Rust | process_client/evidence_store tests; r08-rust-consumer-{synthetic,official}.json | SQLite derivado dedicado, namespace do host; não altera schema canônico |
| R08: pacote/host Foundry | r08-foundry-{synthetic,official,host}.json; fingerprints/artefatos atuais conferidos | instância QA real própria, não instalação no aplicativo do usuário |
| Genericidade e independência | Cargo sem dependência de IADV/Foundry; Doc/Source/Evidence/Store e adaptador separados | referência de protocolo lida e executável Core reutilizado somente para QA |

Configuração completa:97 testes regulares +14 opt-in nativos reais =111 testes
distintos aprovados. Default85 aprovados. Fmt/clippy all-targets default/completo
-Dwarnings e schemas16positivos/18negativos passaram. Logs e comandos exatos em
`verification/r08-foundry-final-checks.json`. Casos CLI do pacote são adicionais:
PDF/texto/HTML, Unicode, expansão entre jobs, vazio sem pack fabricado;7negativos
na fixture e8na fonte oficial, incluindo pesos adulterados com mesmo tamanho.

## Prova de integração Foundry

Host executável existente, lido sem alteração, usado com novo cwd/runtime/porta.
Endpoint/PID do filho conferidos pelo SO; autenticação nova pelo bootstrap oficial
em memória, sem tokens/cookies em stdout/recibos. Anônimo401. Descoberta/schema/
fingerprint, workspace plugin_data_only, registro do pacote, instalação/concessão
real do recurso E5, upload/seleção de PDF, execução Node→Rust/CPU, downloads com
SHA/bytes conferidos e continuação via seleção do pack em outro job passaram.
Repetir request_id devolveu o mesmo job. Dois jobs positivos terminaram exit0 com
árvore confirmada parada; revogação do grant fez o job posterior falhar antes da
inferência em model_grant, sem result.json. Logout, host terminado e runtime de
QA removido. `production_adoption:false` é intencional: publicação não é condição
de conclusão desta entrega local.

Fingerprint corrente do pacote: `8f50782d017575dc2a5f614ecf2dbd8a072a883d0b488230499f01e0aa04af8e`.
Binário qualificado: `9d1e5b18ce4b34d5eb98ba4d92606a13c16f32f811f001ae15cbaf91faf2dbb2`.
O auditor `scripts/audit_completion.py` recomputa os bytes/fingerprint atuais,
confere recursos pinados, execuções, resultados negativos esperados e111 testes;
inventário não substitui esses ensaios funcionais.

## Expansão após recuperação

Dentro de ClientSession, expand usa o snapshot retido e não repete GET. Depois do
TTL/fim do processo, o pack continua inspecionável; para expandir, o host fornece
os mesmos bytes/snapshots e chama restore. No plugin, re-seleciona a mesma fonte e
pack; URI inclui projeto+artifactId. Troca de diretório temporário é permitida,
troca de origem/versão não. Origin/publicHttp do envelope é metadado do host,
separado da identidade do motor e dos grants. Fonte oficial não prova vigência.

## Recursos e recuperação de falhas de QA

O volume C: ficou sem espaço numa repetição do host após os ensaios terem passado.
O host próprio foi terminado; a tentativa não foi tratada como sucesso. Removidos
somente4recursos opcionais de benchmark já medidos (~1,068GB) após conferir SHA/
bytes e processos, e123executáveis gerados de teste (~922MB) após conferir paths/
processos. Preservados fontes, Cargo deps necessários, binários correntes CLI/
worker/pacote, E5/ORT e recibos. Nenhum target de outro projeto ou .git tocado.
Uma preparação QA incompleta própria (~16,5MB) foi removida após validar o path.
A repetição posterior completa passou; o host libera seus recursos de QA antes
de publicar o recibo pequeno por arquivo temporário+replace, preservando evidência
anterior em falha de espaço. A margem segue variável: revalidar antes de builds
ou downloads grandes. Para reexecutar os4perfis do benchmark, preparar novamente
os modelos opcionais pelo script pinado e seu gate de capacidade; não são parte
do runtime mínimo E5, de~148MB.

A leitura da evidência detectou6recibos históricos JSON com stdout Windows-1252.
Foram convertidos para UTF-8 apenas depois de conferir hashes de packs/referências/
quotes, guardando SHA original/novo em receipt-utf8-recovery.json. Não são execuções
novas. Produtores agora declaram UTF-8; fluxo incremental oficial foi executado
novamente com o código corrente (386respostas conferidas). Dados/tempos históricos
não são benchmark controlado nem prova ampla de relevância.

Falhas observadas preservadas na tarefa R08: expectativa errada de layout de
bootstrap, cabeçalho local ausente, campo origin do host não contemplado, chave
errada no observador de isolamento, encoding de fixture e caso sem achados.
Correções ocorreram no consumidor/QA, sem remover guard/grant/auth ou alterar
Core/IADV. Código local é confiado; Job supervisiona processos, não isola permissões
OS/rede. Inicialização de modelo/callbacks têm as bordas descritas, não preempção
universal. Não há serviço instalado, GPU, treinamento ou chamada de LLM grande.

## Operação

- Motor/contrato: README.md e INTEGRATION.md.
- Rust/SQLite e sessões: RUST_ADAPTER.md; exemplo rust_consumer.
- Pacote: adapters/foundry/README.md e foundry.plugin.json; recurso E5 separado.
- Qualificação: scripts/qualify_foundry_adapter.py [--official] e
  scripts/qualify_foundry_host.py --host EXECUTAVEL, sempre runtime QA novo.
- Benchmark: BENCHMARK.md, scripts de preparação/runner/sumário e dataset congelado.
- Auditoria de arquivos/evidências: python scripts/audit_completion.py.

DoD local do projeto demonstrada no escopo acima. Não há aceites obrigatórios
pendentes ou regressões abertas; não implica certificação jurídica, qualidade
representativa geral, suporte Linux ou adoção em produção.
