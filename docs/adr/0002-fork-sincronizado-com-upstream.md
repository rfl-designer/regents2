# O fork continua sincronizado com o herdr upstream

O Regents recebe as melhorias do `herdrdev/herdr` por sincronização periódica. Para os merges ficarem baratos, os nomes internos (crate, módulos, `AppState`) continuam como estão. Só o binário, a CLI (`regents`), os caminhos de configuração e a marca mudam. O domínio novo (workspace com pasta, canal, thread, wiki) fica em módulos novos e separados, e os pontos de contato com o código do herdr ficam o menor possível. Quem vê `herdr` dentro do código não deve "corrigir" para `regents`: isso é intencional.

## Considered Options

- **Fork definitivo com renomeação total**: rejeitado porque cada merge do upstream teria conflitos em quase todos os arquivos.
- **Só cherry-pick**: rejeitado porque o upstream evolui rápido (detecção, performance, Windows), e escolher commits um a um não escala.
