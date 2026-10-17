# TakeScreen

Painel do operador para escolher o que aparece no datashow (segunda tela), no estilo do Modo Estúdio do OBS.

- **Programas abertos** embaixo, em cartões com o ícone e uma prévia ao vivo: clique para ver na **PRÉVIA**, clique duplo para exibir direto.
- **PRÉVIA** (verde) e **NO AR** (vermelho) são imagens ao vivo das janelas, desenhadas pelo próprio Windows.
- **EXIBIR** (ou Enter): a janela da prévia é montada no datashow *por trás* da que está no ar e só depois vem para a frente. O público vê só a troca.
- A janela que estava no ar volta para a tela 1 do jeito que estava.
- **Limpar datashow** (ou Esc): tela preta no datashow.
- **Tela cheia do programa (F11)**: aperta F11 na janela que está no ar (Chrome, Fotos).
- **Proteger datashow** (vem ligado): qualquer janela que abrir ou for arrastada para a tela 2 sem passar pelo TakeScreen volta na hora para a tela 1. A janela que sai do ar também sempre volta para a tela 1.
- O datashow é lembrado pelo nome do aparelho, então continua certo se a ordem dos monitores mudar.

## Baixar

Cada envio para a branch `main` gera o `TakeScreen.exe` na aba **Actions** do GitHub (em *Artifacts*). Para publicar uma versão em **Releases**, crie uma tag começando com `v`:

```
git tag v0.1.0
git push origin v0.1.0
```

## Ajuste da janela

- *Tela inteira sem bordas*: tira a barra de título e ocupa o monitor todo. Padrão para programas comuns (PowerPoint, Chrome).
- *Maximizar*: padrão para apps da Microsoft Store (Fotos, Filmes e TV), que desenham a própria moldura.

## Compilar

No Windows, com Rust instalado:

```
cargo build --release
```

O programa sai em `target/release/TakeScreen.exe`.

No Linux, gerando o .exe para Windows (precisa do `gcc-mingw-w64-x86-64`):

```
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
```

O programa sai em `target/x86_64-pc-windows-gnu/release/TakeScreen.exe`.

## Dica

No Windows 11, desligue "Mostrar a barra de tarefas em todos os monitores" (Configurações > Personalização > Barra de tarefas) para a barra não aparecer no datashow.
