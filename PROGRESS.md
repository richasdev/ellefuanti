# PROGRESS — estado atual (2026-08-13)

**v0.3.1 lançada e a servir como `latest`.** Suite em **1406 testes, 41 suites**, clippy
limpo sob os flags do CI, binário **18.73 MB de 19 MB (98.6% do gate)**. Ledger das rondas
anteriores no histórico git deste ficheiro.

> **Antes de cortar qualquer versão:** correr a skill `/ellefuanti-release`, e ler
> **[RELEASE.md](RELEASE.md)**. Não é opcional — cada release desde a v0.1.0 partiu de uma
> forma nova e **nenhuma foi apanhada pelo CI**.

## Trabalho em curso — o que está a acontecer agora

Dois agentes a correr em worktrees isoladas, cada um numa branch própria. Se o contexto
desta sessão se perder, é aqui que se retoma:

| Branch             | O quê                                                                         | Estado             |
| ------------------ | ----------------------------------------------------------------------------- | ------------------ |
| `feat/inlay-hints` | Inlay hints do LSP (tipos e nomes de parâmetros, estilo PhpStorm/Zed)         | Agente a trabalhar |
| `feat/agent-mode`  | Modo Ask/Agent no chat: propostas de edição com diff e aprovação por ficheiro | Agente a trabalhar |

**Como retomar um agente parado:** as worktrees vivem em `.claude/worktrees/agent-<id>/`.
Se um agente cair (limite de sessão acontece), o trabalho **não se perde** — verificar
`git -C .claude/worktrees/agent-<id> log --oneline -2` e `git status`, porque costuma estar
commitado ou pelo menos escrito. Depois: `git worktree remove <path> --force`, `git checkout
<branch>`, `git rebase main`, correr os testes, e só então PR.

**O que fica na fila depois destes dois:**

1. **Anexos ricos no chat** — imagens, prints, drag & drop para o painel. Fundação já
   existe: `ExternalPaths` (drag & drop desde a v0.2.0) e o gpui renderiza imagens
   nativamente.
2. **Assinatura real + notarização** — US$ 99/ano. É o que remove o aviso do Gatekeeper
   de vez; ver a secção sobre isso mais abaixo.
3. **`.dmg` no CI** — hoje é construído e anexado à mão a cada release.
4. **Universal binary** — o build é `arm64` puro; um Mac Intel não corre.
5. **#30 Xdebug**, **#28 plugins**, **#31 browser embutido**, **#18 IME/dead keys**.

## Onde está o projeto

| Versão    | O que trouxe                                                                             |
| --------- | ---------------------------------------------------------------------------------------- |
| **0.1.0** | Editor, LSP, Laravel/Livewire, painéis Git/DB/Docker/Composer/testes                     |
| **0.2.0** | Drag & drop, auto-refresh da árvore, ficheiro ativo na árvore, fim do limite de 64 MB    |
| **0.2.1** | Auto-update in-app                                                                       |
| **0.3.0** | Chat IA + ghost text, smart typing PHP, zen/fullscreen, 8 temas, settings com secções    |
| **0.3.1** | Auto-import, lâmpada de quick fix, chat por assinatura (Codex), painéis redimensionáveis |

**Issues #29 (AI autocomplete) e #99 (AI chat) fechadas.**

## O arco da v0.3.1

| PR   | Entrega                                                                                         |
| ---- | ----------------------------------------------------------------------------------------------- |
| #217 | Correção do prompt da chave (⌘V era descartado; chave em texto claro) + botão de IA na titlebar |
| #218 | Painéis redimensionáveis: sidebar e chat com divisória arrastável                               |
| #219 | Auto-import de classes + lâmpada de quick fix no gutter                                         |
| #220 | Chat por assinatura via o `codex` do utilizador (modo leitura)                                  |
| #223 | ⌘C/⌘V e cursor visível em **seis** campos de texto (eram quatro na estimativa)                  |

## Arquitetura de IA — o mapa

Três ficheiros, com fronteiras claras. Quem mexer aqui deve lê-los por esta ordem:

- **`crates/app/src/ai.rs`** — a camada de provider, pura e testável sem rede.
  `resolve_auth`, `chat_body`, `curl_args`, `parse_sse`, `deny_reason`. Providers HTTP
  (Anthropic key, `ant` CLI, base URL compatível com OpenAI) via `curl` do sistema.
- **`crates/app/src/ai_codex.rs`** — cliente JSON-RPC do `codex app-server`. **O protocolo
  foi sondado contra o binário, não lido da documentação — que está desatualizada** (fala em
  `newConversation`, que o binário rejeita pelo nome). Superfície real: `initialize` →
  `thread/start` → `turn/start`, texto a chegar em `item/agentMessage/delta`. As linhas
  capturadas são as fixtures dos testes. Script de sondagem em
  `scratchpad/probe.py` (fora do repo, recriar se preciso).
- **`crates/app/src/ai_chat.rs`** — o painel. **Dois transportes atrás de uma UI:** HTTP e
  Codex alimentam o _mesmo_ canal de `StreamEvent`, o mesmo drain com batching de 50ms
  (#93) e o mesmo kill handle. A UI abaixo dessa costura não distingue os dois.

**Regra de egress, não negociável:** nada sai da máquina sem ato explícito. Chaves no
Keychain (nunca no settings JSON), contexto anexado por chips visíveis, e `deny_reason`
recusa `.env`, chaves SSH, PEMs, bases sqlite e ficheiros com nome de credencial — **sem
override**. Registado em `docs/RISKS.md` §9.

**Login Claude.ai não é oferecido, e não é por dificuldade técnica.** A política da
Anthropic reserva o OAuth às aplicações dela e proíbe terceiros de encaminhar credenciais
Pro/Max. É por isso que o PhpStorm tem sign-in ChatGPT para o Codex e só chave de API para
o Claude. Não reabrir sem a política mudar.

## Lições que custaram (as desta ronda)

1. **Uma verificação que não é a do CI não é uma verificação.** Quatro runs vermelhos
   seguidos, todos no mesmo passo, enquanto o `cargo clippy -p ellefuanti` local dizia
   "limpo". O comando do CI é `RUSTFLAGS="-D warnings" cargo clippy --all-targets
--all-features` — o `--all-targets` compila o harness de teste, onde um método chamado
   só atrás de `cfg(not(test))` fica sem chamadores e é código morto.
2. **Documentar não é o mesmo que garantir.** O codesign estava no RELEASE.md e mesmo
   assim foi esquecido no release seguinte. Daí a skill `/ellefuanti-release`: o
   documento explica, a skill executa.
3. **Sondar o protocolo antes de o codificar.** A documentação do `codex app-server` está
   desatualizada; descobrir isso numa conversa real de 5 minutos poupou uma implementação
   inteira contra métodos que não existem.
4. **Um agente que contesta o briefing costuma ter razão.** No trabalho dos inputs eu
   inventei um ficheiro que não existia (`editor/caret.rs`) e ignorei que a palette já
   tinha cursor com uma decisão escrita _contra_ o piscar, pelo mesmo motivo de perf que
   eu próprio citei. O agente seguiu a casa em vez de mim, e encontrou dois inputs que eu
   não tinha visto.
5. **Agentes paralelos em worktrees funcionam**, desde que cada prompt declare fronteiras
   de ficheiros explícitas ("não toques em `settings_panel.rs`"). Foi o que evitou
   conflitos entre três branches a mexer na mesma área.

## Instalação no macOS — o estado honesto

**Não há certificado nesta máquina** (`security find-identity -v -p codesigning` → 0). Sem
conta paga da Apple, o `spctl` responde `rejected` e o macOS avisa e oferece mandar para o
lixo. Isso **não** é evitável por código.

O que foi corrigido é a diferença entre dois avisos muito diferentes:

- **Bundle malformado** → _"is damaged and can't be opened"_, sem saída. Era o que
  acontecia até à v0.3.0 (assinatura ad-hoc só do binário, `Info.plist` não vinculado,
  recursos não selados) e está resolvido.
- **Bem formado mas sem certificado** → _"programador não identificado"_, e
  **clique-direito → Abrir → Abrir** passa. É o que acontece agora, e é o que o README
  ensina em primeiro lugar.

Nunca dizer ao utilizador que o aviso desapareceu. Dizer qual é a saída.

## Onde estão as coisas

- `RELEASE.md` — checklist executável e o registo de **sete** falhas de release, cada uma
  com causa raiz e porque não foi apanhada.
- `docs/RISKS.md` — §9 é o egress de dados da IA.
- `docs/superpowers/specs/` e `plans/` — specs e planos por entrega.
- Skill `/ellefuanti-release` (em `~/.claude/skills/`) — corre os checks do CI, verifica a
  assinatura e testa o download com quarentena.

**Atenção ao binário:** 18.73 de 19 MB. A próxima dependência não cabe sem decisão
explícita — foi por isso que a camada de IA usa o `curl` e o `security` do sistema em vez
de um cliente HTTP e de um keyring.

Debug: `ELLE_FOREGROUND=1 ellefuanti . > log 2>&1`; wire-tap do LSP via `ELLE_LSP_COMMAND`
apontado a um script com `tee`.
