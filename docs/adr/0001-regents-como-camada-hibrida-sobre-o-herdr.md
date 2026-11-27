# Regents é uma camada híbrida sobre o modelo do herdr

O Regents não substitui tabs, panes e o layout BSP do herdr. O canal é um tipo novo de pane (view de chat) dentro do layout, ao lado de panes de terminal. As threads são panes fora do layout, listadas na sidebar e abertas como popup. Escolhemos isso para reusar o layout, o foco, o mouse e a sidebar que já existem, e para manter o terminal livre disponível.

## Considered Options

- **Camada separada** (o workspace ganha canais, mas eles ficam fora do layout): rejeitada porque cria uma segunda linguagem de UI.
- **Substituir o layout** (a TUI tem só canais e threads): rejeitada porque perde o uso do Regents como multiplexer normal e afasta o fork do upstream.

## Consequences

- O pane de chat é um pane de terminal normal que executa `regents channel view <canal>`, um cliente da JSON API. Por isso, o rascunho e a rolagem do chat ficam no processo do chat, e não no cliente da TUI. Todos os clientes ligados ao mesmo servidor veem o mesmo rascunho e a mesma rolagem. Aceitamos esta exceção à regra "seleção, rolagem e rascunho são estado do cliente", porque ela reusa o layout, o foco, o popup e os clientes remotos sem código novo de render.
