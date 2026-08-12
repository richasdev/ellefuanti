# PROGRESS — estado atual (2026-08-12)

**v0.3.0 lançada e a servir como `latest`.** Suite em **1369 testes, 41 suites**, clippy
limpo, binário **18.69 MB de 19 MB (98.4% do gate)**. Ledger das rondas anteriores no
histórico git deste ficheiro.

> Para cortar uma versão — e para o registo de tudo o que já partiu num release —
> ver **[RELEASE.md](RELEASE.md)**. Não é opcional: cada release desde a v0.1.0 partiu de
> uma forma nova, e **nenhuma foi apanhada pelo CI**.

## Onde está o projeto

Três versões saíram no mesmo dia, cada uma com um arco próprio:

| Versão    | O que trouxe                                                                          |
| --------- | ------------------------------------------------------------------------------------- |
| **0.1.0** | Editor, LSP, Laravel/Livewire, painéis Git/DB/Docker/Composer/testes                  |
| **0.2.0** | Drag & drop, auto-refresh da árvore, ficheiro ativo na árvore, fim do limite de 64 MB |
| **0.2.1** | Auto-update in-app (verifica, instala, reinicia)                                      |
| **0.3.0** | Chat IA + ghost text, smart typing PHP, zen/fullscreen, 8 temas, settings com secções |

**Issues #29 (AI autocomplete) e #99 (AI chat) fechadas** — eram as duas maiores por fechar
fora das milestones originais.

## O arco da v0.3.0

Sete entregas, executadas em sequência, cada uma com spec → branch → PR → merge:

| PR   | Entrega                                                                                                                                                                                                       |
| ---- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| #204 | Smart typing PHP: aspas auto-fecham **em código** (a árvore sintática protege `don't` em prosa), `=` vira `=>` dentro de array literal, `>` seguinte engolido                                                 |
| #205 | Fullscreen (⌃⌘F, delega à plataforma) e Zen (⌘K Z, esconde chrome + centra com padding fracionário; session-only para nunca reabrir preso)                                                                    |
| #206 | 8 temas: Dracula, Nord, Catppuccin Mocha/Latte, Gruvbox, Tokyo Night, Solarized Dark/Light — como disk themes, com o `bundle-macos.sh` a copiá-los para `Contents/Resources/themes`                           |
| #207 | Settings com secções (Editor/Aparência) e picker de tema em grelha com 4 swatches por tema; `preview()` vive em `theme.rs` porque construir `Theme` é monopólio desse módulo (teste de arquitetura #48 impõe) |
| #208 | Camada de provider IA: 3 providers × 2 formatos de wire, transporte via `curl` do sistema (zero crates novas contra o gate), chaves no Keychain, **denylist sem override**                                    |
| #210 | Painel de chat IA (⌘⇧A): streaming com repaint agregado a 50ms (gate #93), cancel mata o filho, chips de contexto explícitos, code blocks com copiar                                                          |
| #211 | Ghost text: primeira linha inline + overlay para as seguintes, Tab aceita como um undo step, debounce 400ms, uma única request em voo                                                                         |

**#210 e #211 foram feitos por dois agentes em paralelo**, cada um em worktree isolada; o do
ghost text caiu a meio (máquina adormeceu) e foi retomado do ponto exato. O merge do ghost
text sobre o chat panel foi limpo.

## Correções de release desta ronda

Quatro PRs que não são features — são a instalação a funcionar. **Detalhe completo em
[RELEASE.md](RELEASE.md)**, resumo:

| PR   | O que estava partido                                                                                                                        |
| ---- | ------------------------------------------------------------------------------------------------------------------------------------------- |
| #201 | Cancelamento de testes matava só o filho direto; o neto segurava os pipes → "cancel" de 30s                                                 |
| #202 | `cargo fmt --check` vermelho em 28 ficheiros; perf-gate a falhar o build por load do runner                                                 |
| #214 | Todos os releases marcados `prerelease` → `releases/latest` servia a **v0.1.0**, e o auto-update nunca disparou                             |
| #215 | Bundle malformado (`Info.plist` não vinculado, recursos não selados) → macOS dizia "damaged"; e `cp -R` no `.dmg` comia o `_CodeSignature/` |

## Arquitetura — o que mudou

- **`crates/app/src/ai.rs`** — camada de provider, pura e testável sem rede: `resolve_auth`,
  `chat_body`, `curl_args`, `parse_sse` (dois wires), `deny_reason`. Tudo o que toca rede
  está atrás de um `curl` filho, o que também dá cancelamento grátis (matar = cancelar).
- **`crates/app/src/ai_chat.rs`** e **`crates/app/src/editor/ghost.rs`** — as duas
  superfícies de IA, ambas consumindo `ai.rs` sem duplicar nada.
- **15 crates** (eram 10 no README antigo): entraram `git`, `db`, `docker`, `test-runner`,
  `theme`.
- **`docs/RISKS.md` ganhou a entrada #9** (egress de dados) que a issue #99 exigia — a regra
  vivia só no corpo da issue, a uma issue fechada de ser esquecida.

## Lições desta ronda (as que custaram)

1. **Um release verde no CI não é um release verificado.** As três falhas que partiram a
   instalação — prerelease, assinatura, cópia — são todas invisíveis a `cargo test`. O único
   teste que as apanha é descarregar do GitHub com quarentena e correr `spctl`.
2. **Uma flag com justificação temporal precisa de data de validade escrita ao lado.** O
   `prerelease: true` tinha razão legítima na v0.1.0 e sobreviveu a três releases porque a
   razão estava no comentário mas a validade não.
3. **`cp -R` não é `ditto`.** Para bundles assinados, `cp` produz um estado _pior_ que não
   assinar: promete recursos selados que não existem.
4. **Um teste que só falha em máquina carregada costuma ser bug de concorrência real.** O
   flaky do test-runner era um process group em falta, não escalonamento azarado.
5. **Agentes paralelos em worktrees funcionam** para trabalho grande e independente — desde
   que cada um tenha fronteiras de ficheiros explícitas no prompt (dizer a um "não toques em
   `settings_panel.rs`" evitou o único conflito possível).

## Para quem continua

**Por fechar, em ordem de valor:**

1. **Assinatura real + notarização** — US$ 99/ano; é o que remove o aviso do Gatekeeper de
   vez e torna o README uma linha em vez de três. Tudo o resto na instalação já está feito.
2. **`.dmg` no CI** — hoje é construído e anexado à mão a cada release; é o passo mais fácil
   de esquecer e o que o README recomenda.
3. **Universal binary** — o build é `arm64` puro, um Mac Intel não corre.
4. **#30 Xdebug** (decisão por tomar: DBGp nativo vs DAP+bridge), **#28 plugins**
   (recomendação subprocess+IPC à espera de 👍), **#31 browser embutido** (colide com o gate
   de binário — spike de medição primeiro), **#18 IME/dead keys** (precisa de teclado real),
   **#112** (ack do dono).

**Atenção ao binário:** 18.69 de 19 MB. A próxima dependência não cabe sem uma decisão
explícita — foi por isso que a camada de IA usa o `curl` do sistema em vez de um cliente HTTP.

Debug: `ELLE_FOREGROUND=1 ellefuanti . > log 2>&1`; wire-tap do LSP via `ELLE_LSP_COMMAND`
apontado a um script com `tee`.
