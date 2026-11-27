# Regents é uma camada híbrida sobre o modelo do herdr

O Regents não substitui tabs, panes e o layout BSP do herdr. O canal é um tipo novo de pane (view de chat) dentro do layout, ao lado de panes de terminal. As threads são panes fora do layout, listadas na sidebar e abertas como popup. Escolhemos isso para reusar o layout, o foco, o mouse e a sidebar que já existem, e para manter o terminal livre disponível.

## Considered Options

- **Camada separada** (o workspace ganha canais, mas eles ficam fora do layout): rejeitada porque cria uma segunda linguagem de UI.
- **Substituir o layout** (a TUI tem só canais e threads): rejeitada porque perde o uso do Regents como multiplexer normal e afasta o fork do upstream.
