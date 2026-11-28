# Regents é uma camada híbrida sobre o modelo do herdr

O Regents não substitui tabs, panes e o layout BSP do herdr. O canal é um tipo novo de pane (view de chat) dentro do layout, ao lado de panes de terminal. As threads são panes fora do layout, listadas na sidebar e abertas como popup. Escolhemos isso para reusar o layout, o foco, o mouse e a sidebar que já existem, e para manter o terminal livre disponível.

## Considered Options

- **Camada separada** (o workspace ganha canais, mas eles ficam fora do layout): rejeitada porque cria uma segunda linguagem de UI.
- **Substituir o layout** (a TUI tem só canais e threads): rejeitada porque perde o uso do Regents como multiplexer normal e afasta o fork do upstream.

## Consequences

- O pane de chat é um pane de terminal normal que executa `regents channel view <canal>`, um cliente da JSON API. Por isso, o rascunho e a rolagem do chat ficam no processo do chat, e não no cliente da TUI. Todos os clientes ligados ao mesmo servidor veem o mesmo rascunho e a mesma rolagem. Aceitamos esta exceção à regra "seleção, rolagem e rascunho são estado do cliente", porque ela reusa o layout, o foco, o popup e os clientes remotos sem código novo de render.
- A thread abre no popup do herdr, que é estado do servidor. O popup de uma thread ativa mostra o pane oculto dela, sem uma cópia, e fechar o popup não encerra a thread. O popup de uma thread que terminou executa `regents thread output <id>`, que mostra a última saída gravada em `.regents/thread-output/<id>.ansi`, só para leitura. Por isso, todos os clientes de um servidor veem o mesmo popup. Aceitamos esta segunda exceção à regra "o popup é estado do cliente", porque um popup só do cliente precisaria de um fluxo novo de superfície de pane por cliente. A seleção na lista de threads continua a ser estado do cliente, e a lista vem do servidor (`thread.list` e o snapshot do cliente).
