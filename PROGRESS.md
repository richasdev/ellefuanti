# PROGRESS — sessão autónoma (2026-08-11)

Escrito a pedido do dono com o contexto a ~95%. `CONTEXT.md` guarda as lições
duráveis; isto é o ledger completo da sessão. **Tudo o que está aqui foi merged em
main e verificado; a app instalada em `~/.local/bin/ellefuanti` e o `.app` em
`target/` são o build de `7501445`.**

## O arco da sessão

Começou com "o popup de completion nunca abre" e terminou com o Milestone 3
destravado. Pelo caminho: 13 PRs merged (#128–#140), ~21 issues fechadas com
evidência, e o editor passou de sem-LSP-funcional para: completion real
(Intelephense + rotas + colunas Eloquent com proveniência), navegação completa
(F12/⌘-click no identificador, refs, símbolos), multi-cursor completo menos
folding, menu de contexto com operações de ficheiro seguras, painel de settings,
git stage/unstage/commit-com-hooks, grammars Rust+Markdown, Blade colorido,
`ellefuanti .` à la `code .`, e o índice Laravel (#21) a alimentar o popup (#22).

## PRs merged, em ordem

| PR | Conteúdo |
|---|---|
| #128 | Saga LSP: arranque (pasta/ficheiro/Finder), PATH+shebang, resync de documento, altura do popup → fecha #123 #125 #126 #127 |
| #129 | ⌘-click em paths no terminal; definition aterra no identificador (UTF-16); ⌘-hover sublinhado+mãozinha → fecha #70 |
| #130 | Painel de settings (⌘,) + resolução do release-config → fecha #57 #100 |
| #131 | Multi-cursor fase 1: ⌘D, escrever em todos, Esc |
| #132 | ⌥click; ⌘C/⌘X multi; refresh do CONTEXT.md |
| #133 | Seleção em coluna (⌥-drag, arestas em pixels) |
| #134 | Fase 2: motions movem o pack; colisões fundem |
| #135 | Blade/Volt deixa de ser uma cor só (lexer HTML nas regiões text) |
| #136 | Tint+● de modificados na árvore; titlebar do tema |
| #137 | `ellefuanti .` desanexa (3 tentativas mortas documentadas); faixa da titlebar |
| #138 | #53 grammars Rust+MD (gate 17→19MB atribuído) + #64 itens 3–4 (stage/commit-CLI-com-hooks) + ledger |
| #139 | **#21**: extractors model/migration, schema v2 com proveniência por coluna, build cancelável no open |
| #140 | **#22 fatia 1**: colunas do model no popup, proveniência no detail |

## Issues fechadas com evidência (sem PR dedicado)

#47 #48 #49 #50 #53 #54 #58 #60 #62 #69 #71 #81 #125 — auditorias comentadas em
cada uma. #112 tem a decisão escrita (verificação em duas camadas: debug_bounds
para caixas, olhos do dono para tinta) e espera o ack dele.

## Estado das abertas

- **#21** — fatia 1 merged. Falta: reanálise incremental via grafo de deps,
  tabelas de rotas/Livewire/Blade, watch de mudanças externas.
- **#22** — colunas entregues. Falta: relationships como items, contexto
  `Model::`, colunas dentro de `where('...')`.
- **#20** — agora tem DUAS fontes vivas para rankear (LSP + colunas). É a próxima
  peça de design.
- **#82** — só falta **folding**; o aviso da issue mantém-se (quebra o mapeamento
  row↔linha do uniform_list, "carefully or not at all").
- **#64** — itens 1–4 entregues; **item 5 (push/pull/branch/stash) deliberadamente
  por construir** atrás da nota de perigo.
- **#65** — a resolução óbvia do conflito ADR-0007 está anotada aqui e não escrita
  em ADR: **rusqlite já está na árvore e é síncrono; SQLx era a pergunta errada**.
- **#23** — Artisan via paleta por começar. #83 umbrella. #99 AI chat. #24
  Livewire (atrás da decisão Blade-tree/ADR-0006). #35 é do dono. #63 deferido.

## Postura de verificação

~1155 testes, 37 suites (1 PTY flaky sob carga, verde solo — pré-existente).
Clippy limpo de warnings novos (dois cosméticos entraram com #140: um `if`
colapsável e um needless-ref em workspace_view — triviais, primeira coisa a
limpar). Gate de binário: 17.64MB / 19MB. **Toda garantia nova passou por
mutação**; quatro testes vazios foram apanhados pela mutação e ou reforçados
(extras-collide, fixture Volt, declared-table-wins) ou apagados com razão
registada (row-height ink).

## As armadilhas recorrentes, contadas

1. **`/var` vs `/private/var`** — **5 aparições** (tabs-no-delete, rename
   retarget, git stage, e 2× em testes de índice). Regra: dois paths de origens
   diferentes NUNCA se comparam crus.
2. **Medição culpada antes do código** — 3+ (histórico #79, harness pty do
   detach, fixture do Blade). O harness mata mais hipóteses que o código.
3. **flex_1 em pai não-flex = altura zero** — 3 (popup invisível, near-miss da
   árvore; 2 testes debug_bounds guardam agora).
4. **Fixture não-falsificável** = teste vazio vestido de dados (declared-table ==
   convenção; Blade text vs text_interpolation).
5. **macOS engole o clique de ativação** em janela de fundo, modificadores
   incluídos — a causa recorrente de "⌘-click não funciona".

## Para quem continua (humano ou não)

Ordem de valor: **#22 restos** (relationships são leitura direta do índice) →
**#20** (design: rankear LSP vs índice, proveniência já no tipo) → **#82
folding** (com cuidado) → **#23 Artisan** → **#65** (escrever o ADR rusqlite).
Debug: `ELLE_FOREGROUND=1 ellefuanti . > log 2>&1`; wire-tap LSP via
`ELLE_LSP_COMMAND` num script tee; UI automation precisa de Acessibilidade que o
terminal não tem. O dono testa de verdade e reporta em uma linha — o log
instrumentado + uma ronda dele vale mais que dez teorias.
