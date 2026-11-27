# Regents

Plataforma de orquestração de agentes de código no terminal. Cada pasta registrada é um ambiente isolado com agentes, canais de conversa e uma wiki própria.

## Language

### Ambiente

**Workspace**:
Ambiente isolado ligado a exatamente uma pasta do filesystem. Contém os agentes, os canais e a wiki daquele ambiente. Todo workspace tem pasta.
_Avoid_: Tenant (usar só como explicação), projeto, space

**Workspace desconectado**:
Workspace registrado cuja pasta não está mais no caminho conhecido.
_Avoid_: Workspace quebrado, órfão

**Workspace legado**:
Workspace antigo, de antes do Regents, que não está ligado a uma pasta. Os terminais dele continuam a funcionar, mas ele não tem agentes, canais nem wiki até você ligá-lo a uma pasta.
_Avoid_: Workspace livre, workspace antigo

**Wiki**:
Base de conhecimento do workspace: documentos em Markdown com um índice, no padrão LLM Wiki. Todos os agentes leem a wiki; só o Chief of Staff escreve nela.
_Avoid_: Docs, knowledge base

**Fonte**:
Documento original e imutável que você adiciona à wiki. O Chief of Staff transforma fontes em páginas da wiki.
_Avoid_: Raw, anexo, input

**Ingestão**:
Ato do Chief of Staff de ler uma fonte e atualizar as páginas, o índice e o log da wiki. Uma ingestão acontece só quando você pede.
_Avoid_: Import, sync

**Lint da wiki**:
Verificação automática e sem LLM da integridade da wiki: links quebrados, páginas órfãs, páginas fora do índice e fontes não ingeridas.
_Avoid_: Sincronização, auditoria

### Agentes

**Agente**:
Definição persistente de um participante não humano do workspace: nome, CLI base, modelo e papel. Um agente não é um processo; cada execução dele é uma thread.
_Avoid_: Bot, assistente, CLI (quando se refere ao agente)

**Biblioteca de agentes**:
Coleção global de agentes, fora de qualquer workspace. Um workspace usa um agente da biblioteca por cópia, e nunca por referência.
_Avoid_: Templates, catálogo, registry

**Chief of Staff**:
Agente reservado do workspace. Ele existe sempre, é o único agente com permissão para mudar a estrutura do workspace (agentes, canais, configuração) e recebe as mensagens sem menção.
_Avoid_: Agente principal, orquestrador, admin

### Conversa

**Canal**:
Conversa persistente de um workspace, guardada numa subpasta dele. Humanos e agentes trocam mensagens nela.
_Avoid_: Chat, sala, room

**Mensagem do sistema**:
Mensagem de canal escrita pelo próprio Regents, e não por um humano ou um agente. Ela informa erros e eventos, como um bloqueio, uma interrupção ou uma menção inválida.
_Avoid_: Log, notificação, alerta

**Agente padrão**:
Agente de um canal que recebe as mensagens sem menção. Um canal pode não ter agente padrão; nesse caso, essas mensagens são só registro.
_Avoid_: Responsável, owner

**Menção**:
Referência `@agente` numa mensagem de canal. Uma menção aciona o agente referenciado.
_Avoid_: Tag, ping

**Thread**:
Uma execução de um agente. Uma thread pode ser de canal (aberta por menção, em segundo plano) ou uma conversa direta (aberta por você, visível). Todas as threads ficam no índice de threads.
_Avoid_: Job, task, sessão

**Conversa direta**:
Thread que você abre com um agente, fora de um canal, num pane visível em que você usa a CLI do agente diretamente.
_Avoid_: DM, chat privado

**Thread ativa**:
Thread que ainda não terminou. Quando o mesmo agente recebe uma nova menção no mesmo canal, a thread ativa decide se absorve a menção ou se o Regents abre uma thread nova.
_Avoid_: Thread aberta, thread viva

**Thread interrompida**:
Thread ativa que parou porque o Regents reiniciou. Ela só continua quando alguém retoma a thread.
_Avoid_: Thread morta, falha

**Fila de threads**:
Threads que esperam porque o workspace está no limite de threads ativas.
_Avoid_: Backlog, pendentes

**Cadeia de delegação**:
Sequência de threads em que cada uma foi aberta por uma menção feita por um agente na thread anterior. A cadeia tem profundidade máxima.
_Avoid_: Recursão, sub-agentes

**Resposta encadeada**:
Mensagem de canal que responde a uma mensagem de um agente. Ela continua a thread daquela mensagem em vez de abrir uma nova.
_Avoid_: Reply, follow-up
