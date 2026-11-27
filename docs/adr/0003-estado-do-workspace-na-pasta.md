# O estado do workspace fica na pasta, em `.regents/`, com SQLite

Todo workspace está ligado a uma pasta. O estado dele (`workspace.toml` com um ID estável, agentes, canais, wiki e registro de threads) fica em `<pasta>/.regents/`, e não no `session.json` global do herdr. Cada canal tem o próprio `channel.sqlite`, e as threads ficam em `.regents/threads.sqlite`. Escolhemos isso para o workspace ser autocontido e portátil: você pode copiar, versionar ou mover a pasta sem perder conversas nem agentes. O registro global em `~/.config/regents/` guarda só o caminho de cada workspace e a biblioteca de agentes.

## Consequences

- O projeto ganha uma dependência de SQLite. Hoje a persistência do herdr é só JSON.
- A identidade do workspace passa a ser a pasta, e não o cwd do primeiro pane da primeira tab.
- Os agentes da biblioteca global entram no workspace por cópia, e nunca por referência, para não criar dependências fora da pasta.
