# PROGRESS — estado atual (2026-08-13)

**v0.3.2 lançada e a servir como `latest`.** A `main` já traz a ronda seguinte por lançar:
suite em **1485 testes, 41 suites**, clippy limpo sob os flags do CI, binário **18.86 MB de
19 MB (99.3% do gate)**. Ledger das rondas anteriores no histórico git deste ficheiro.

> **Antes de cortar qualquer versão:** correr a skill `/ellefuanti-release`, e ler
> **[RELEASE.md](RELEASE.md)**. Não é opcional — cada release desde a v0.1.0 partiu de uma
> forma nova e **nenhuma foi apanhada pelo CI**.

## Trabalho em curso — o que está a acontecer agora

**Em curso:** painel de preview embutido (#31), na branch `feat/preview-pane`, com a
**[ADR-0011](docs/adr/0011-webview-for-preview-pane-only.md)** já commitada a fixar a
fronteira. O custo em binário foi **medido antes de escrever funcionalidade**: 18.86 →
18.85 MB com uma `WKWebView` real construída — o motor é framework do sistema, só os
bindings são ligados. (Medir com a dependência declarada e _não_ usada teria dado zero
enganador: o linker descarta-a.)

O problema por resolver é de composição, não de tamanho: o GPUI desenha com Metal na sua
janela e uma `WKWebView` é uma `NSView`. Se o GPUI 0.2.2 não tiver costura para hospedar
subviews nativas, a resposta certa é **parar e dizer qual é o bloqueio**, não forçar.

**Como retomar um agente parado** (guardado porque volta a ser preciso): as worktrees vivem
em `.claude/worktrees/agent-<id>/`. Se um agente cair, o trabalho **não se perde** —
verificar `git -C .claude/worktrees/agent-<id> log --oneline -2` e `git status`, porque
costuma estar commitado ou pelo menos escrito. Depois: `git worktree remove <path> --force`,
`git checkout <branch>`, `git rebase main`, correr os testes, e só então PR.

**Uma armadilha nova, desta ronda:** um agente que branche antes de outro PR fazer merge
reporta números da _base dele_, não da `main`. O agente dos anexos reportou três
"contradições" ao briefing (um ficheiro que não existia, 1427 testes, 18.76 MB) — as três
eram o mesmo facto: base velha. `apply_proposal` existia, em `ai_chat.rs:858`. **O rebase é
que resolve a discussão**, não o relatório de nenhum dos lados.

**O que fica na fila:**

1. **Assinatura real + notarização** — US$ 99/ano. É o que remove o aviso do Gatekeeper
   de vez; ver a secção sobre isso mais abaixo. **É o único item que depende de dinheiro e
   não de trabalho.**
2. **Decidir o limite da slice x86_64.** O universal binary saiu (#230), mas a slice Intel
   mede **20.08 MB** contra os 18.84 MB da arm64 — acima do gate de 19 MB. Binários Intel
   são maiores para o mesmo código e nenhuma flag corrige. E **o gate nem sequer vê o
   binário fat**: corre do `ci.yml` sobre o build thin nativo, enquanto o `lipo` vive no
   `release.yml`, que não invoca o script. Escolha em aberto: limite próprio para x86_64,
   trabalho de tamanho, ou exceção assumida. Registado no `perf-gate.sh`.
3. **Testar a slice x86_64 num Mac Intel real.** Nunca foi executada — esta máquina é Apple
   Silicon sem Rosetta. O `lipo` prova que o código lá está, não que abre janela. **Não
   anunciar suporte Intel antes disso.**
4. **#30 Xdebug**, **#28 plugins**. A #31 (browser) está em curso; a #18 (IME) saiu.

**Feito nesta ronda:** `.dmg` no CI com os dois nomes e gate de bundle (#229), universal
binary (#230), IME e dead-keys (#231).

## Instalação por uma linha (v0.3.2)

`scripts/install.sh`, servido de `raw.githubusercontent.com/.../main/scripts/install.sh`.
Descarrega a release atual, instala com `ditto`, limpa a quarentena e abre — **sem aviso
nenhum**, porque a quarentena sai antes do primeiro arranque.

Três coisas que o README fazia mal e que isto corrige: URL com versão fixa (partia a cada
release), `cp -R` (come o `_CodeSignature/` e produz o "danificado" que o próprio README
tentava evitar) e o `xattr` — a correção real — escondido num `<details>`.

**O ponto de montagem é lido do `hdiutil`, nunca adivinhado.** Duas ciladas, ambas
descobertas a correr: `-quiet` suprime as linhas que é preciso parsear, e o `hdiutil` separa
colunas por **tabs** — cortar em espaços trunca `ellefuanti 0.3.2` no espaço.

## Onde está o projeto

| Versão    | O que trouxe                                                                             |
| --------- | ---------------------------------------------------------------------------------------- |
| **0.1.0** | Editor, LSP, Laravel/Livewire, painéis Git/DB/Docker/Composer/testes                     |
| **0.2.0** | Drag & drop, auto-refresh da árvore, ficheiro ativo na árvore, fim do limite de 64 MB    |
| **0.2.1** | Auto-update in-app                                                                       |
| **0.3.0** | Chat IA + ghost text, smart typing PHP, zen/fullscreen, 8 temas, settings com secções    |
| **0.3.1** | Auto-import, lâmpada de quick fix, chat por assinatura (Codex), painéis redimensionáveis |
| **0.3.2** | Inlay hints, modo agente com diff e aprovação por ficheiro, anexos no chat, install.sh   |

**Issues #29 (AI autocomplete) e #99 (AI chat) fechadas.**

## O arco da v0.3.2

| PR   | Entrega                                                                                              |
| ---- | ---------------------------------------------------------------------------------------------------- |
| #226 | Inlay hints do LSP — fatias por coluna descendente; dica fora da linha é ignorada, não encostada     |
| #227 | Modo agente: sandbox fica `read-only` nos **dois** modos, e é isso que torna a aprovação obrigatória |
| #228 | Anexos no chat: imagens em blocos na wire, base64 à mão, `deny_reason` em dois sítios                |

**O achado da #227, que inverteu o desenho.** Sondagem contra o `codex-cli` 0.146.0: em
`workspace-write` a CLI **escreve sem pedir nada**; em `read-only` emite
`item/fileChange/requestApproval` e **espera**. Pedir permissão de escrita _removia_ o passo
de consentimento. Por isso o sandbox fica `read-only` nos dois modos — "nada chega ao disco
sem aprovação" é propriedade do protocolo, não promessa do painel. O painel responde sempre
`decline`, mesmo ao que o utilizador aplicou: quem escreve é o editor, e `accept` faria a
Codex reaplicar o patch por cima de bytes já mudados.

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
