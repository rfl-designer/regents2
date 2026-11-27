# Agentes leem e publicam em canais pela CLI `regents`, com captura de saída como fallback

Uma thread recebe um prompt inicial com a mensagem da menção e as últimas N mensagens do canal. Ela lê mais com `regents channel read` e responde com `regents channel post`. Se a thread termina sem publicar, o Regents copia a última resposta do pane para o canal, e usa a detecção de idle/done do herdr para saber que ela terminou. Escolhemos uma CLI, e não MCP, porque toda CLI de agente consegue executar comandos shell sem configuração. Um servidor MCP pode ser uma camada fina sobre a mesma API depois. Canais e mensagens são estado do servidor, expostos pela JSON API, e não pelo socket privado da TUI.

## Considered Options

- **Só captura de saída**: rejeitada porque a resposta fica suja (UI da CLI misturada) e o agente não consegue mencionar outros agentes.
- **Histórico completo no prompt**: rejeitado porque estoura o contexto em canais longos.
