# PROGRESS — sessão autónoma (2026-08-11, segunda ronda)

Continuação do ledger anterior (o arco #128–#140 está no histórico git deste ficheiro).
Tudo abaixo está **merged em main e verificado**; a suite fecha em **1200 testes, 37
suites** (o PTY flaky pré-existente mantém-se verde solo), clippy limpo, binário
17.81MB / 19MB.

## O arco desta ronda

Pedido: loop pelas issues das milestones — atacar, fechar com evidência, seguir.
Resultado: **Milestone 3 (Laravel) fechada por inteiro** (#21, #22, #23) e **#20
fechada** (Milestone 2). Dez PRs, #141–#150.

## PRs merged, em ordem

| PR   | Conteúdo                                                                                                                                                                                        |
| ---- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| #141 | #22: relationships como items do popup (`hasMany · Post`, variante própria); fix da race de HOME entre testes de file_cache e popup-índice (o teste do #140 passava por sorte de escalonamento) |
| #142 | #22: colunas dentro de `where('…')` e dez amigos — `column_context_at` tree-based, chain-walk até à classe, `$user->` honesto (nada)                                                            |
| #143 | #22: relationships dentro de `with`/`whereHas`/etc — `expects: Argument` decide qual lista; a outra seria resposta errada com badge confiante                                                   |
| #144 | #22: scopes por nome de chamada (`scopeActive`→`active`) após `Model::` — schema v3 (`model_scopes`), scanner atravessa o ERROR node do meio-da-digitação                                       |
| #145 | #22: accessors (ambos os estilos → `full_name`) e `$guarded` como colunas-por-implicação; sem mudança de schema (source é TEXT)                                                                 |
| #146 | #23: paleta "Artisan Command…" — lista do _próprio_ artisan (`list --raw`), confirmar **digita** `php artisan <nome> ` no terminal sem newline; nada executa fora da vista                      |
| #147 | #21: rebuild do índice no save de model/migration (`is_under` canonicalizado — 6ª aparição da armadilha /var, desta vez prevenida)                                                              |
| #148 | #21: rebuild no window focus — o trigger de mudanças externas, mesmo raciocínio dos 3 triggers do git (#64); sem watcher, sem timer                                                             |
| #149 | #20: ranking — qualidade do match, depois claim do projeto, depois brevidade; estável dentro da banda; query vazia mantém ordem de fontes                                                       |
| #150 | #20: buffer words sem servidor — invoke a meio da palavra oferece os identificadores do ficheiro (badge `text`); sem palavra digitada, nada (sem sinal, tudo é ruído)                           |
| #151 | #19: Format Document (⇧⌥F) — resync antes de pedir, aplicar só ao texto perguntado, `apply_edits` = um undo step (splice_at generalizado); batch com overlap rejeitado inteiro                  |

## Issues fechadas, cada uma com auditoria no fecho

- **#22 Eloquent intelligence** — superfície declarada completa; gaps registados:
  setter-only mutators, segundo segmento de `with('a.b')`, `User::` sem letra ainda.
- **#23 Routes + Artisan** — indexação de rotas era do #46; Artisan agora; GUIs
  opcionais e tabela de rotas adiadas com razão escrita (viva-e-nunca-stale).
- **#21 Índice SQLite** — construído, consumido, fresco (3 triggers, sem timer).
  Incremental pass adiado: otimizar sem perfil é proibido pelas convenções; a tabela
  `dependencies` está pronta para quando um projeto real o medir como lento.
- **#20 Merged completion** — fontes reais, proveniência no tipo desde #118, ranking,
  cancelamento; pipeline local warm p50 1.4ms contra o alvo de 50ms.

## Estado das milestones

- **M3 Laravel: 0 abertas.** M2 PHP: restam #18 (IME/dead keys — precisa de teclado
  real e olhos do dono) e #19 (formatting entregue em #151; faltam rename, workspace
  symbols, semantic tokens e code actions — cada um quer a sua decisão de UI, ver o
  comentário na issue: pode valer declinar semantic tokens de propósito).
- Fora de milestone continuam #82 (só falta folding, com o aviso do uniform_list),
  #64 item 5 (push/pull atrás da nota de perigo), #65 (ADR rusqlite por escrever),
  #112 (à espera de ack do dono), #35 (checklist humano).

## Lições novas desta ronda (as que custaram)

1. **`git checkout -- ficheiro` durante mutation-testing destrói trabalho untracked/
   unstaged.** Aconteceu duas vezes (scanner de scopes reescrito). Regra: `git add`
   ANTES de cada mutação sed/python; reverter por edit, não por checkout.
2. **A race de HOME** — testes que resolvem `index_path` (derivado de HOME) num task
   de background correm contra `with_home` de outros testes. `HOME_LOCK` é agora
   pub(crate) a nível de ficheiro; qualquer teste novo que toque no índice via popup
   segura-o a sessão inteira. O teste do #140 passava por sorte.
3. **Fonte sem sinal é ruído com badge** — a decisão do #150 (buffer words só com
   prefixo digitado) resolveu simultaneamente o UX e cinco testes de layout que
   teriam de mentir. Quando um fix de testes e um fix de design coincidem, é o design.
4. **tree-sitter dá contexto a meio da digitação** — `User::ac` parseia como
   `class_constant_access_expression` dentro de um ERROR node; `descendant_for_byte_range`
   no fim de palavra aterra no token SEGUINTE (sonda offset-1). Ver `scope_context_at`.
5. **`php artisan list --raw`** evita JSON (e a dependência serde_json no app crate).
   O caminho Herd (`~/Library/Application Support/Herd/bin`) juntou-se aos prefixos.

## Para quem continua

Ordem de valor sugerida: **#82 folding** (o resto do multi-cursor está feito; cuidado
com o mapeamento row↔linha do uniform_list) → **#19 critério de fecho + auditoria** →
**#65 escrever o ADR rusqlite** (a resolução está anotada no ledger anterior) →
**#24 decisão ADR-0006 Blade tree** (destrava Livewire e as tabelas restantes do
índice). #18 e #35 precisam do dono. Debug: `ELLE_FOREGROUND=1 ellefuanti . > log 2>&1`;
wire-tap LSP via `ELLE_LSP_COMMAND` num script tee.
