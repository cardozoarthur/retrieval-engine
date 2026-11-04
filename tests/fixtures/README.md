# Fixtures sintéticas

`two-pages.pdf`: duas páginas textuais criadas com ReportLab, Helvetica, posição
(72,760), canvas invariant=1. Primeira: "Norma sintetica: primeira pagina. Nao
constitui lei real." Segunda: "A responsabilidade exige prova. Excecao: nao se
aplica sem culpa." `blank.pdf`: uma página vazia. `mixed.pdf`: três páginas,
com texto na primeira/terceira e segunda vazia para testar cobertura. Não são
fontes jurídicas reais.

Verificação: renderizadas com PyMuPDF e conferidas visualmente em 2026-11-03.
Fixtures pequenas; não armazenar renders como cópias históricas.

`progressive.pdf`:128páginas textuais sintéticas ReportLab, Helvetica12,
posição(72,720), invariant=1/pageCompression=1. PáginaN contém
"Synthetic retrieval page N. Exception requires proof.".64641B, usada para
provar que uma página fica disponível com auxiliar ainda ativo e pipe limitado;
sem lei real ou benchmark de qualidade. Não é cópia de documento do usuário.

`late-error.pdf`: cópia de trabalho sintética de progressive.pdf regravada com
pypdf, substituindo somente o stream de conteúdo da página128 por
`BT /F1 12 Tf 1 Tm ET`. OperadorTm com aridade inválida provoca falha tardia
capturada pelo auxiliar;63635B. Usada para invalidar achados anteriores ao erro.
A tentativa inicial com string não terminada foi tolerada pelo parser como
página sem texto, não uma falha. Coverage.pages_without_text torna esse caso
distinto de conteúdo todo legível; versões intermediárias do fixture não retidas.
