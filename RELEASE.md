# RELEASE — como cortar uma versão, e tudo o que já correu mal

Este ficheiro existe porque **cada release desde a v0.1.0 partiu de uma maneira nova**, e
nenhuma das falhas foi detetada pelo CI: todas passaram nos testes e chegaram ao utilizador
partidas. O que se segue é o checklist, e a seguir o registo de cada falha com a causa raiz —
porque um checklist sem o "porquê" é uma lista que alguém há de encurtar.

**A regra que resume tudo:** um release verde no CI não é um release verificado. O CI compila
e testa; não descarrega o `.dmg`, não o monta, não o abre. As três coisas que partiram a
instalação — prerelease, assinatura, cópia — são todas invisíveis a `cargo test`.

---

## Checklist

### Antes de taggar

- [ ] `git checkout main && git pull` — a tag sai do main, nunca de uma branch.
- [ ] `cargo test --workspace` verde. **Ler o número**, não só o "ok": uma suite que deixou
      de compilar desaparece silenciosamente da contagem.
- [ ] **Clippy exatamente como o CI o corre** (comando abaixo, fora da lista) — um
      `cargo clippy -p ellefuanti` limpo diz muito pouco.
- [ ] `cargo fmt --all --check` limpo.
- [ ] **Binário dentro do gate**: `cargo build --release` e comparar com `BIN_LIMIT_MB` em
      `scripts/perf-gate.sh`. Está em **18.69 MB de 19 MB (98.4%)** — qualquer dependência
      nova é uma decisão de release, não de implementação. O limite vale **por slice**: o
      release é universal e o ficheiro pesa a soma dos dois, mas o que o gate mede é uma
      cópia do programa (o script extrai cada slice com `lipo -thin`).
- [ ] CHANGELOG com uma secção `## [x.y.z] — data` (não deixar em `[Unreleased]`).
- [ ] `version` em `Cargo.toml` bumpado, e `cargo check` corrido depois para o `Cargo.lock`
      apanhar o novo número.

**O clippy do CI, na íntegra:**

```sh
RUSTFLAGS="-D warnings" cargo clippy --all-targets --all-features
```

As três partes importam. `--all-targets` compila o harness de teste, onde um método chamado
só atrás de `cfg(not(test))` fica sem chamadores e passa a código morto; `--all-features`
liga código que de outra forma nem compila; e `-D warnings` (definido no workflow)
transforma qualquer aviso em erro. Correr só `cargo clippy -p ellefuanti` foi o que deixou
quatro runs vermelhos passarem por verificados (§7). Cuidado também com o `rtk`, que agrega
o output e pode esconder o texto do aviso: em caso de dúvida, `rtk proxy cargo clippy`.

### Taggar

```sh
git tag -a vX.Y.Z -m "vX.Y.Z — resumo"
git push origin vX.Y.Z
```

O workflow `release.yml` dispara em `refs/tags/v*`: corre os testes, compila com
`--no-default-features` (shaders precompilados, precisa de Xcode completo no runner),
empacota o `.zip` e publica o release.

### Depois de o CI publicar

**1. `latest` aponta para a tag nova.** Se responder uma antiga, é o bug do prerelease (§1):

```sh
gh api repos/richasdev/ellefuanti/releases/latest -q .tag_name
```

**2. Construir e anexar o `.dmg`** — o CI só produz o `.zip`:

```sh
cargo build --release
scripts/bundle-macos.sh
scripts/dmg-macos.sh
gh release upload vX.Y.Z target/ellefuanti-vX.Y.Z-macos.dmg
```

**3. Verificar a assinatura do bundle** — o passo que faltou em três releases:

```sh
codesign -dv --verbose=2 target/ellefuanti.app 2>&1 | grep -E "Info.plist|Sealed"
# Tem de dizer:  Info.plist entries=N   e   Sealed Resources version=2
# "Info.plist=not bound" ou "Sealed Resources=none" = bundle malformado,
# e o macOS vai chamar-lhe "damaged".

codesign --verify --deep --strict target/ellefuanti.app   # exit 0
```

**4. Verificar que a assinatura sobreviveu ao `.dmg`** (o `cp -R` come o `_CodeSignature`):

```sh
hdiutil attach target/ellefuanti-vX.Y.Z-macos.dmg -nobrowse -quiet
ls "/Volumes/ellefuanti X.Y.Z/ellefuanti.app/Contents/"   # tem de listar _CodeSignature/
hdiutil detach "/Volumes/ellefuanti X.Y.Z" -quiet
```

**5. Teste do utilizador real** — descarregar do GitHub _com quarentena_ e ver o veredito:

```sh
cd /tmp && curl -sL -o t.dmg \
  https://github.com/richasdev/ellefuanti/releases/latest/download/ellefuanti-vX.Y.Z-macos.dmg
xattr -w com.apple.quarantine "0083;00000000;Safari;" t.dmg
hdiutil attach t.dmg -nobrowse -quiet
ditto "/Volumes/ellefuanti X.Y.Z/ellefuanti.app" /tmp/t.app
hdiutil detach "/Volumes/ellefuanti X.Y.Z" -quiet
spctl -a -vvv -t exec /tmp/t.app
```

**`rejected` sozinho é o resultado correto** — é o veredito normal de um app sem certificado,
e produz o diálogo "desenvolvedor não identificado" com opção de abrir. Qualquer outra
mensagem (sobretudo _"code has no resources but signature indicates they must be present"_)
significa bundle partido.

**6. Últimos dois olhares:**

```sh
# `defaults read` exige caminho ABSOLUTO — com caminho relativo diz
# "domain/default pair does not exist", que parece plist em falta e não é.
/usr/libexec/PlistBuddy -c "Print :CFBundleShortVersionString" \
  target/ellefuanti.app/Contents/Info.plist          # a versão certa
ls target/ellefuanti.app/Contents/Resources/themes/ | wc -l   # 8 temas
```

### Deprecations — verificar de release em release

- [ ] `cargo build 2>&1 | grep -i deprecat` — nenhum aviso de API deprecada nosso.
- [ ] `cargo report future-incompatibilities` — hoje acusa `block v0.1.6` e
      `proc-macro-error2 v2.0.1`, ambos **transitivos via gpui**, nada que possamos corrigir
      aqui. Se aparecer um terceiro, verificar se é nosso antes de ignorar.
- [ ] Actions do workflow com major pinado (`actions/checkout@v5`,
      `actions/upload-artifact@v5`, `softprops/action-gh-release@v2`) — um major novo é uma
      mudança deliberada, nunca automática. **O aviso a vigiar é o do runtime:** o GitHub
      deprecou o Node 20 e mostra "forced to run on Node.js 24" em cada run enquanto uma
      action estiver numa major antiga. É aviso, não erro, por isso não parte nada — e é
      exatamente por isso que passa despercebido até alguém olhar para o separador
      Annotations.
- [ ] Modelos de IA em `crates/settings/src/file.rs` (`ai.chat_model`,
      `ai.completion_model`): os defaults apontam para modelos que existem hoje. Modelo
      retirado = 404 no primeiro pedido do utilizador.
- [ ] `rust-toolchain.toml` — subir o pin é um commit próprio, para os lints novos
      chegarem como mudança revisível e não como build vermelho de outra pessoa.

---

## O registo: cada falha, e porque nenhuma foi apanhada

### 1. Todos os releases eram "prerelease" — o download servia a v0.1.0

**Sintoma.** Um utilizador que clicasse em Download no README recebia a **v0.1.0**, com
três versões já publicadas. E o auto-update — construído na v0.2.1 — nunca disparou para
ninguém.

**Causa raiz.** O `release.yml` publicava com `prerelease: true`, decidido na v0.1.0 com
justificação legítima (rendering não verificado, issue #35) e **nunca revisto**. O GitHub
exclui prereleases ao resolver `releases/latest` — que é exatamente a URL do botão do README
_e_ a que o updater in-app consulta.

**Porque não foi apanhado.** O release publicava com sucesso; o `.dmg` estava lá; a página
da tag estava correta. Só olhando para `/releases/latest` — coisa que ninguém fazia — é que
aparecia. O bug estava numa linha de YAML que passou em todas as revisões por estar
comentada com uma razão que já não valia.

**Correção.** `prerelease: false` + `make_latest: true` (PR #214), e a v0.3.0 promovida à
mão. **Lição: uma flag com uma justificação temporal precisa de data de validade escrita ao
lado.**

### 2. O macOS dizia "is damaged" — e não era o que o README dizia

**Sintoma.** Toda instalação nova: _"ellefuanti is damaged and can't be opened."_ O README
mandava correr `xattr -dr com.apple.quarantine`, o que "resolvia" e mascarou o problema real
durante três releases.

**Causa raiz.** O linker do Rust deixa o executável com uma assinatura ad-hoc mínima
(`linker-signed`) que cobre **só o binário**: `Info.plist=not bound`, `Sealed Resources=none`.
Um bundle nesse estado, com flag de quarentena, não é "não assinado" — é **malformado**, e a
mensagem que o macOS escolhe para malformado é literalmente "danificado". O `xattr` funcionava
porque, sem quarentena, o Gatekeeper nem chega a avaliar a assinatura.

**Porque não foi apanhado.** Quem desenvolve corre `cargo run` (sem bundle) ou copia o `.app`
localmente (sem quarentena). A quarentena só existe no que **desce da internet** — condição
que nenhum teste reproduzia.

**Correção.** `scripts/bundle-macos.sh` assina o bundle inteiro com `codesign --sign -` **em
último lugar**, depois de todos os recursos estarem no sítio (assinar sela os recursos; copiar
alguma coisa depois invalida o selo). PR #215.

### 3. O `.dmg` comia a assinatura — pior que não assinar

**Sintoma.** Já com a correção do §2, o download continuava partido, agora com outra
mensagem: _"code has no resources but signature indicates they must be present."_

**Causa raiz.** `scripts/dmg-macos.sh` copiava o `.app` com `cp -R`, e **o `cp` não leva o
diretório `_CodeSignature/`**. O app chegava assinado-mas-sem-selo — um estado pior que não
assinado, porque a assinatura promete recursos selados que não estão lá.

**Porque não foi apanhado.** Só aparece depois do round-trip completo: assinar → `.dmg` →
montar → copiar. Verificar o `.app` acabado de construir dava verde.

**Correção.** `ditto` em vez de `cp -R` — é a cópia da Apple, feita para preservar bundles
(assinatura, xattrs, resource forks). PR #215. Depois disto o veredito passou de erro de
bundle malformado para `rejected` simples, que é o correto para app sem certificado.

### 4. O `Info.plist` mentia a versão em todos os builds

**Sintoma.** "Get Info" no Finder dizia **0.1.0** num app v0.2.1.

**Causa raiz.** `assets/macos/Info.plist` tinha `0.1.0` hardcoded e o script copiava-o tal e
qual. O `Cargo.toml` era a fonte de verdade só para o binário.

**Correção.** O script carimba a versão com `PlistBuddy` a partir do `Cargo.toml`. PR #203.

### 5. Um teste flaky bloqueou o release da v0.2.1 — duas vezes

**Sintoma.** O CI da tag falhou duas vezes seguidas em
`a_cancelled_run_stops_the_child_rather_than_waiting_for_it`, com 30s cravados.

**Causa raiz.** O cancelamento matava só o filho direto (`sh`); o neto (`sleep`, e na vida
real `pest`/`php`) sobrevivia e **segurava os pipes de stdout/stderr abertos**, mantendo a
thread leitora bloqueada. Localmente o SIGKILL ganhava a corrida contra o fork do shell; num
runner carregado, perdia sempre.

**Correção.** O processo nasce no seu próprio process group e o cancel mata o grupo inteiro
(`kill(-pid, SIGKILL)`). PR #201. **Lição: um teste que só falha em máquina carregada
costuma ser um bug real de concorrência, não flakiness.**

### 6. `cargo fmt --check` vermelho há muito, e o perf-gate a falhar por carga

**Sintoma.** CI vermelho em 28 ficheiros de formatação, e o perf-gate a sair com código 2 num
runner com load 20.

**Causa raiz.** Dois problemas distintos com o mesmo efeito. O gate de formatação nunca tinha
sido corrido em conjunto; e o `perf-gate.sh` usa exit 2 para dizer _"não consigo medir"_ (load
alto contamina memória e tempo), que o workflow tratava como regressão.

**Correção.** Árvore formatada, e o workflow passou a tratar exit 2 como neutro — uma medição
que não aconteceu não é prova de regressão. Regressão real (exit 1) continua a falhar o build.
PR #202.

---

## O que continua por resolver

**Assinatura real.** Tudo acima remove a acusação falsa de "danificado", mas **não** remove o
aviso do Gatekeeper: para isso é preciso certificado Apple Developer (US$ 99/ano) e
notarização. Enquanto não houver, o README ensina `xattr` — e agora o clique-direito → Abrir
também funciona, coisa que com o bundle malformado não era fiável.

**O `.dmg` é manual.** O CI produz só o `.zip`; o `.dmg` — que é o que o README recomenda — é
construído e anexado à mão a cada release. Automatizá-lo no workflow eliminaria o passo mais
fácil de esquecer.

**Universal binary — o slice x86_64 nunca correu.** O release passou a ser universal
(`arm64` + `x86_64`, unidos com `lipo`), por isso um Mac Intel já consegue abrir isto. Mas
só uma máquina Intel — ou um Mac com Rosetta — pode confirmar que o slice x86_64 _funciona_:
quem o construiu não tinha nenhuma das duas, e `lipo -info` prova que o código lá está, não
que ele desenha uma janela. **O primeiro release universal precisa de um teste manual num
Intel antes de se anunciar suporte a Intel.**

**Binário a 98.4% do gate** (18.69 de 19 MB). A próxima dependência não cabe sem decisão
explícita. O limite é **por slice**, não pelo ficheiro: um universal pesa a soma dos dois
(~37 MB) e medir o ficheiro obrigaria a duplicar o limite, o que voltaria a admitir ~18 MB
de crescimento acidental sem ninguém reparar. Ver o comentário em `scripts/perf-gate.sh`.

### 7. Quatro runs de CI vermelhos por um lint que localmente não aparecia

**Sintoma.** Quatro runs consecutivos falharam no passo Clippy, com o trabalho verde na
máquina de quem o escreveu.

**Causa raiz.** Duas coisas ao mesmo tempo. O código: `probe_codex` só é chamado atrás de
`cfg(not(test))` — um teste nunca pode lançar um CLI — por isso quando o clippy compila o
harness de teste o método fica sem chamadores e é código morto. E a verificação local:
corria-se `cargo clippy -p ellefuanti`, que **não** compila os targets de teste e **não**
tem `-D warnings`, por isso mostrava "1 warning" e passava por limpo.

**Porque não foi apanhado.** Porque o comando local não era o comando do CI. Um lint que só
dispara sob `--all-targets` é invisível a quem não o corre com `--all-targets`.

**Correção.** `cfg_attr(test, allow(dead_code))` com a razão escrita ao lado, e o comando
completo do CI promovido a passo obrigatório deste checklist. **Lição: uma verificação que
não é a do CI não é uma verificação.**

### 8. O binário universal não compilava no CI — e o workflow já pedia o target

**Sintoma.** A primeira run da v0.4.0 morreu no passo `Build release (x86_64)` com
`error[E0463]: can't find crate for \`core\``, seguido do mesmo para `std`.

**Causa raiz.** O target `x86_64-apple-darwin` não estava instalado no runner na altura de
compilar. O que torna isto digno de registo é que o workflow **já** o pedia: o passo
`dtolnay/rust-toolchain` recebeu `targets: aarch64-apple-darwin, x86_64-apple-darwin`,
reportou `success`, e o log mostra `info: downloading component rust-std` a acontecer. A
causa mais provável é o `Swatinem/rust-cache`, que corre logo a seguir, restaurar por cima
do toolchain acabado de instalar.

**Porque não foi apanhado.** Porque o binário universal foi validado numa máquina onde
alguém já tinha corrido `rustup target add x86_64-apple-darwin` à mão. É a lição da falha 7
noutra roupagem: ali a diferença estava no **comando**, aqui está no **ambiente**. Um build
que só funciona porque a máquina foi preparada antes não é um build reproduzível, e a
preparação não estava escrita em lado nenhum.

**Correção.** Um passo explícito depois do cache: `rustup target add` para ambos
(idempotente, portanto grátis quando a ação fez o seu trabalho), `rustup target list
--installed` impresso, e um `grep -qx` que falha ali — não 200 linhas dentro de um `cargo
build`. Não fui atrás da causa raiz dentro da ação de terceiros: afirmar o pré-requisito é
mais barato e mais duradouro do que descobrir de quem é a culpa.

**O que correu bem, e vale registar.** A release **não chegou a ser publicada** e o
`releases/latest` continuou a servir a v0.3.2. Nenhum utilizador viu nada partido. O gate
falhou antes de publicar, que é exatamente o comportamento que os gates existem para ter —
ao contrário das falhas 1 a 4, que só se descobriram porque alguém instalou e reclamou.
