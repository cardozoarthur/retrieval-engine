# Plano — motor independente de recuperação

Objetivo: construir um motor Rust genérico de recuperação sob demanda, sem LLM
grande na primeira camada, com fontes locais/remotas, evidências verificáveis,
busca lexical/híbrida e expansão posterior por palavras ou caracteres. Leis e
normas em seus sites oficiais são um cenário de aceite, não o domínio do núcleo.
Este é um projeto novo; não modifica nem depende do código do IADV.

| Ordem | Entrega vertical | Estado |
|---|---|---|
| 1 | R00: recuperar texto local e expandir achados Unicode em snapshot | concluída — tarefas/R00.md |
| 2 | R01: consultar HTML/PDF local/remoto com URL final, hash e páginas | concluída — tarefas/R01.md |
| 3 | R02: BM25 temporário, ranking e evidências com orçamento | concluída — tarefas/R02.md |
| 4 | R03: E5 Small CPU residente, batching e erros explícitos | concluída — tarefas/R03.md |
| 5 | R04: fusão híbrida e reranker opcional com cobertura declarada | concluída — tarefas/R04.md |
| 6 | R05: Evidence Pack e expansão auditável entre chamadas | concluída — tarefas/R05.md |
| 7 | R06: extração incremental, cancelamento, TTL e limites globais | concluída — tarefas/R06.md |
| 8 | R07: benchmark real de modelos/formatos em CPU | concluída — tarefas/R07.md |
| 9 | R08: adaptadores de banco/Foundry independentes do núcleo | concluída — tarefas/R08.md |

R00–R06 verificadas: extração isolada Windows com progresso/recuperação provisória
JSONL, expansão/vinculação exata ao snapshot final, cache/TTL/limites/cancelamento.
Na conclusão de R06: 65 testes default; 73 CPU regulares + 12 reais opt-in passaram.
Qualificação independente do PDF oficial encontrou achado na 1ª de 28 páginas antes
do fim e conferiu 319 respostas/schemas/hashes até o pack final. Receipts
r06-incremental-* e r06-final-public.json. Hard_native_cancellation=false mantém
escopo exato de chamadas não preemptíveis. Nenhuma integração Foundry admitida.
R07 comparou E5/MiniLM FP32/Int8 e três formatos de vetores em CPU real;
ver docs/BENCHMARK.md e recibos r07-benchmark-*.json. Regressão corrente:
74 default; 83 CPU regulares +13 reais opt-in, todos passaram (96 CPU distintos).
R08 concluída com adaptadores e host QA real; tarefas/R08.md. Última margem observada C: 3623985152 B, revalidar antes de
novos recursos grandes. Um target; sem limpeza de targets de terceiros.

Condição de término: aceites demonstrados por comandos e resultados reais, sem
prazo inventado. Sem download automático de modelos grandes. Benchmark mede
separadamente aquisição, extração, tokenização, inferência, ranking, RAM e disco.
Não promover semântica a padrão sem ganho sobre BM25 e orçamento demonstrado.

Estado atual: R00–R08 concluídas; auditoria em docs/COMPLETION.md.
O histórico de restrições de disco e verificações anteriores permanece nas tarefas.
Revalidar capacidade antes de compilar ou baixar modelos; não remover recursos de
outros projetos nem contornar recusas de exclusão. R08 qualifica o pacote no projeto independente e numa instância QA real.

Contrato futuro: consulta → aquisição determinística → lexical e semântica local
→ seleção com proveniência → consumidor IA. Snapshot identifica a versão
consultada; expandir não refaz GET silenciosamente. Caracteres significam valores
escalares Unicode; palavras têm segmentação documentada. Fonte oficial não
implica vigência jurídica verificada. Registrar cobertura parcial e OCR ausente.

Fontes técnicas consultadas em 2026-11-03: modelos em
https://huggingface.co/intfloat/multilingual-e5-small,
https://huggingface.co/sentence-transformers/paraphrase-multilingual-MiniLM-L12-v2
e https://huggingface.co/BAAI/bge-m3. R03 deve fixar revisão/hash/licença,
prefixos E5 query:/passage:, pooling, truncamento e runtime antes da qualificação.


Checkpoint anterior Rust (histórico, consolidado no fechamento abaixo): cliente de processo, EvidenceStore/SQLite dedicado e restore
verificável implementados/qualificados. Default84/all-features96+14reais passaram
(110 distintos completos), clippy/fmt/contratos verdes; fluxo oficial Rust→worker
E5→SQLite→reabertura conferido por Python. Ver docs/RUST_ADAPTER.md e tarefas/R08.md.
Pacote/workflows Foundry e prova em host QA real concluídos; sem adoção em produção.
Última margem observada C:2337808384B, volátil; um target, sem limpezas de terceiros.


Fechamento atual: R00–R08 concluídas.85 testes default;97 regulares +14 nativos
opt-in na configuração completa =111 distintos aprovados. Pacote atual, recurso
E5 e fingerprint conferidos por audit_completion.py. Integração Foundry em instância
própria, com auth/grants/execução/download/continuação/revogação reais. Ver
COMPLETION.md, R08.md e r08-foundry-host.json. Recursos opcionais de benchmark e
exes de teste dispensáveis removidos após verificar ownership; runtime E5/ORT e
binários necessários preservados. A margem de disco flutua e precisa de conferência
antes de novos builds/downloads. Não há pendência obrigatória no escopo local.

## Publicação

[R09](tarefas/R09.md) publicada e validada por clone anônimo.
