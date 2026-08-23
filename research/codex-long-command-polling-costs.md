# Comandos longos no Codex: espera, polling e prompt cache

Pesquisa realizada em 2026-08-23. O checkout analisado do fork Electivus estava em
`48d4153a93f156445fa8e745bd62b9d778c79644`; a referência local de `upstream/main`
estava em `c9b19deb09c1841ce7acc33ddb96276030936a29`. Todas as referências a código usam
permalinks para esses commits ou para commits identificados dos outros projetos.

## Veredito

Para um comando que apenas está trabalhando, a estratégia padrão deve ser **uma espera longa no
host e o menor número possível de retornos ao modelo**. Prompt cache reduz bastante o preço do
prefixo repetido, mas não torna uma inferência gratuita: cada polling acrescenta leitura de cache,
possível escrita do sufixo, tokens de raciocínio/saída, RPM, TPM e mais histórico. O host do Codex
já espera por saída/término usando notificações, sem inferência, até expirar o `yield` da chamada.

Há uma exceção real, mas estreita: se a espera atravessar o TTL de 30 minutos do GPT-5.6, um
**keepalive esparso pouco antes do vencimento** pode custar menos que perder um prefixo grande. Isso
não favorece polling frequente: favorece, quando mensurado, no máximo a quantidade mínima de
checkpoints espaçados pelo TTL. Para espera curta, nenhuma; para espera indeterminada, deixar
expirar costuma ser mais previsível. No Codex incluído em ChatGPT há uma rate card de input/cache,
mas a transformação completa em allowance e a cobrança de cache write não são publicadas, então a
decisão deve ser orientada por `/status` e telemetria, não por um heartbeat automático universal.

## O que está confirmado no Codex

### Uma chamada de tool é uma fronteira de inferência

O loop da sessão recebe uma function call do modelo, executa a tool no host, acrescenta o resultado
ao histórico e só então faz a próxima amostragem. Isso aparece tanto no comentário do fluxo quanto
no `continue` após `needs_follow_up` em
[`session/turn.rs`](https://github.com/Electivus/electivus-codex/blob/48d4153a93f156445fa8e745bd62b9d778c79644/codex-rs/core/src/session/turn.rs#L140-L151),
na coleta concorrente dos resultados em
[`stream_events_utils.rs`](https://github.com/Electivus/electivus-codex/blob/48d4153a93f156445fa8e745bd62b9d778c79644/codex-rs/core/src/stream_events_utils.rs#L289-L327)
e na espera dessas futures em
[`session/turn.rs`](https://github.com/Electivus/electivus-codex/blob/48d4153a93f156445fa8e745bd62b9d778c79644/codex-rs/core/src/session/turn.rs#L2151-L2161).
O upstream atual tem o mesmo desenho no
[`turn.rs`](https://github.com/openai/codex/blob/c9b19deb09c1841ce7acc33ddb96276030936a29/codex-rs/core/src/session/turn.rs#L140-L151).

Consequência operacional, ignorando retries, compactação e outras tools:

| Caminho | Amostragens aproximadas |
|---|---:|
| `exec_command` espera e termina na chamada inicial | 2: uma gera a tool; outra recebe o resultado e continua/finaliza |
| chamada inicial rende e há `P` chamadas de polling até terminar | `P + 2` |
| cada `write_stdin` vazio adicional | +1 amostragem |
| cada `functions.wait` após o script ter rendido | +1 amostragem |

O modelo pode emitir mais de uma tool em uma resposta, portanto isso é a contagem do caminho serial
típico, não uma identidade universal. O ponto invariável é que **esperar dentro da mesma execução da
tool não amostra o modelo; devolver um resultado de tool e pedir a próxima decisão, sim**.

### O host espera sem inferência

No unified exec, a chamada inicial e `write_stdin` calculam um deadline e aguardam
`collect_output_until_deadline` no próprio host
([início](https://github.com/Electivus/electivus-codex/blob/48d4153a93f156445fa8e745bd62b9d778c79644/codex-rs/core/src/unified_exec/process_manager.rs#L564-L574),
[`write_stdin`](https://github.com/Electivus/electivus-codex/blob/48d4153a93f156445fa8e745bd62b9d778c79644/codex-rs/core/src/unified_exec/process_manager.rs#L820-L825)).
Essa espera não é busy polling: ela usa `tokio::select!` sobre notificações de saída, término,
mudança de pausa e o deadline
([implementação](https://github.com/Electivus/electivus-codex/blob/48d4153a93f156445fa8e745bd62b9d778c79644/codex-rs/core/src/unified_exec/process_manager.rs#L1320-L1410)).
A saída também é transmitida como eventos enquanto a resposta final da tool continua pendente.

No fork, a política padrão é timeout de 10 minutos (faixa 10 s–1 h) e yield de 30 segundos
(faixa 10 s–5 min), em
[`config/src/tool_execution.rs`](https://github.com/Electivus/electivus-codex/blob/48d4153a93f156445fa8e745bd62b9d778c79644/codex-rs/config/src/tool_execution.rs#L8-L23).
Ela alcança exec não interativo, `write_stdin` vazio e code-mode `exec`/`wait`; operações interativas
mantêm janela curta. O ADR do fork registra explicitamente que janelas curtas geram inferências
extras e que não há teto de produto acima do máximo configurado pelo administrador
([ADR 0001](https://github.com/Electivus/electivus-codex/blob/48d4153a93f156445fa8e745bd62b9d778c79644/codex-rs/docs/adr/0001-tool-execution-timing-policy.md#L7-L49)).

No upstream observado, o `exec_command` inicial ainda usa 10 s por padrão e no máximo 30 s
([`shell_spec.rs`](https://github.com/openai/codex/blob/c9b19deb09c1841ce7acc33ddb96276030936a29/codex-rs/core/src/tools/handlers/shell_spec.rs#L26-L30)),
enquanto o polling vazio de `write_stdin` aceita até 300 s
([contrato](https://github.com/openai/codex/blob/c9b19deb09c1841ce7acc33ddb96276030936a29/codex-rs/core/src/tools/handlers/shell_spec.rs#L113-L145),
[`process_manager.rs`](https://github.com/openai/codex/blob/c9b19deb09c1841ce7acc33ddb96276030936a29/codex-rs/core/src/unified_exec/process_manager.rs#L823-L836)).

### Code mode: nested tools não precisam voltar ao modelo

Uma função JavaScript passada a `functions.exec` pode chamar e aguardar nested tools inteiramente no
runtime host-side. Não há nova inferência enquanto esse programa continua aguardando. Por outro lado,
`yield_control()` devolve o controle ao modelo; se o script/célula ainda roda, a continuação por
`functions.wait` cria outra fronteira de tool e, portanto, outra amostragem. O runtime expõe exatamente
`ObserveMode::YieldAfter` para `execute` e `wait`
([`service.rs`](https://github.com/Electivus/electivus-codex/blob/48d4153a93f156445fa8e745bd62b9d778c79644/codex-rs/code-mode-runtime/src/service.rs#L77-L149)),
e a camada core transforma `RuntimeResponse::Yielded` em resultado de tool
([`tools/code_mode/mod.rs`](https://github.com/Electivus/electivus-codex/blob/48d4153a93f156445fa8e745bd62b9d778c79644/codex-rs/core/src/tools/code_mode/mod.rs#L223-L237)).

### Continuidade do prompt cache

O cliente Codex usa o ID da sessão como `prompt_cache_key` e o mantém na request
([criação](https://github.com/Electivus/electivus-codex/blob/48d4153a93f156445fa8e745bd62b9d778c79644/codex-rs/core/src/client.rs#L488-L492),
[`ResponsesApiRequest`](https://github.com/Electivus/electivus-codex/blob/48d4153a93f156445fa8e745bd62b9d778c79644/codex-rs/core/src/client.rs#L939-L956)).
Na conexão incremental, ele só reutiliza `previous_response_id` quando input anterior e response items
são prefixo exato da request atual
([checagem](https://github.com/Electivus/electivus-codex/blob/48d4153a93f156445fa8e745bd62b9d778c79644/codex-rs/core/src/client.rs#L1260-L1331)).
Também invalida a reutilização se mudarem modelo, instruções, tools, tool choice, reasoning, store,
include, service tier, cache key ou formato de texto
([comparação](https://github.com/Electivus/electivus-codex/blob/48d4153a93f156445fa8e745bd62b9d778c79644/codex-rs/core/src/client.rs#L300-L365)).

Logo, acrescentar function calls e outputs no fim do histórico normalmente preserva o prefixo e é
amigável ao cache. Reescrever o início, compactar/resumir, trocar tools/settings ou usar outra chave
pode perder o hit. Cache torna o prefixo repetido mais barato; não elimina a nova decisão do modelo.

## API OpenAI: preço, TTL e limites

Para GPT-5.6, o prompt cache é automático a partir de 1.024 tokens, exige prefixo exato e usa por
padrão o último item de usuário/tool como breakpoint implícito. A primeira request escreve o cache;
as seguintes podem lê-lo. O TTL oficial é **30 minutos desde a escrita e é renovado a cada reuse**.
Leitura custa `0,1×` input, escrita custa `1,25×`, input sem cache custa `1×`; o cache pode escrever
até quatro breakpoints mais recentes. Isso está na documentação oficial de
[Prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching).

Na API, uma request de polling adicional `i` custa conceitualmente:

```text
custo_i = r_input * (0,10*C_i + 1,25*W_i + U_i) + r_output*O_i
```

onde `C_i` é o prefixo lido do cache, `W_i` os tokens recém-escritos, `U_i` o input não cacheado e
`O_i` a saída visível **mais reasoning tokens**. Divide-se por 1 milhão ao aplicar os preços por
milhão. Para `N` polls, somam-se os `N` custos. Cada um também acrescenta uma request a RPM; input
cacheado continua contando para TPM. Prompt caching não reduz geração de output nem rate limits,
conforme a mesma [documentação oficial](https://developers.openai.com/api/docs/guides/prompt-caching#prompt-caching).

O preço vigente do GPT-5.6 Sol é US$ 4/M input, US$ 0,40/M cached input, US$ 5/M cache write e
US$ 20/M output, com rate limits dependentes do tier
([página do modelo](https://developers.openai.com/api/docs/models/gpt-5.6-sol)).
Exemplo apenas ilustrativo, por poll: prefixo cacheado de 50 mil tokens, 200 tokens escritos e 50
tokens de saída/raciocínio custariam aproximadamente:

```text
50.000 * $0,40/M + 200 * $5/M + 50 * $20/M = $0,022
```

Para um processo que termina em 10 min, uma janela de 10 s produz aproximadamente 59 polls adicionais
(cerca de US$ 1,30); uma janela de 5 min produz aproximadamente um (US$ 0,022). A contagem exata
depende de quando o término cruza o deadline. Isso exclui as chamadas inicial/final necessárias e
subestima casos com mais raciocínio, output ou cache miss. Modelos anteriores, como GPT-5.3-Codex,
têm preços diferentes e não
devem herdar automaticamente a cobrança de escrita 1,25× introduzida para GPT-5.6
([página do modelo](https://developers.openai.com/api/docs/models/gpt-5.3-codex)).

### Vale fazer polling só para manter o cache quente?

Uma espera no host não envia request, portanto não renova o TTL. Considere um prefixo `C` que
expiraria antes da próxima continuação necessária:

- deixar expirar e reescrever: custo de prefixo aproximadamente `1,25*C*r_input`;
- um keepalive antes do vencimento e depois a continuação: duas leituras, aproximadamente
  `0,20*C*r_input`, além dos sufixos, reasoning e output de ambas;
- economia bruta máxima do keepalive: `1,05*C*r_input`, antes de todo overhead.

Com o Sol e `C = 50.000`, reescrever o prefixo seria US$ 0,25; keepalive + continuação leriam o
prefixo por US$ 0,04, deixando no máximo US$ 0,21 para pagar o overhead do keepalive. Generalizando,
`K` keepalives mais a continuação custam `(K+1)*0,10*C`; a comparação de prefixo isolado permanece
favorável a reescrever somente enquanto `K+1 < 12,5`. Na prática o break-even chega antes por causa
de output/raciocínio, sufixos, contexto crescente e risco de miss.

Portanto:

1. Se a espera prevista é menor que 30 min, não faça polling para aquecer cache; uma request dentro
   do TTL já o renova.
2. Para espera longa e indeterminada, não mantenha o cache vivo indefinidamente. Com polls a cada
   5 min, doze polls em uma hora já aproximam/superam o custo de uma reescrita só nas leituras do
   prefixo, antes do restante.
3. Em uma aplicação própria da API, considere o **menor número possível** de keepalives, cada um
   pouco antes de 30 min, somente quando `C` é grande, a retomada é conhecida e métricas reais
   justificam. Para um único keepalive, o requisito simplificado é
   `overhead_keepalive < 1,05*C*r_input`; para `K`, compare o custo completo de `K` requests com a
   economia de uma reescrita. Use prefixo estável/explicit-only quando apropriado e confira
   `cached_tokens` e `cache_write_tokens` na resposta.
4. Uma request de modelo vazia não é um heartbeat grátis. Se for preciso manter infraestrutura
   viva, use heartbeat do host/transporte, não uma inferência.

## Codex incluído em ChatGPT não é a mesma conta da API

A página oficial de [preços e limites do Codex](https://learn.chatgpt.com/docs/pricing)
confirma que Codex vem incluído nos planos ChatGPT, que tarefas locais e cloud compartilham uma
janela de cinco horas e que limites semanais podem existir. Ela publica faixas estimadas de mensagens,
mas diz que o consumo varia com modelo, tamanho/complexidade da tarefa, contexto, execução local/cloud,
reasoning e tools. Não há fórmula pública que permita converter `N` polls em exatamente `X%` da
allowance incluída de uma assinatura.

A mesma página mostra uma rate card de créditos com input, cached input e output — o cached input
vale 10% do input nos modelos listados — e permite API key para cobrança tokenizada separada. Porém a
tabela de créditos ChatGPT não publica uma linha de cache write. Assim, é incorreto aplicar o `1,25×`
da API do GPT-5.6 como se fosse a fórmula oculta da allowance ChatGPT.

Ainda é possível obter um limite superior útil. Para um prefixo estável `C`, ignorando todo o
overhead, perder o hit troca uma leitura de `0,10*C` por input de `1,00*C`; a perda marginal é
`0,90*C`. Cada keepalive acrescenta pelo menos `0,10*C`, então **nove keepalives empatam** com um
único miss só no prefixo. O break-even real chega antes, pois cada request também tem sufixo,
reasoning e output. Isso torna plausível um checkpoint único perto do TTL para uma conversa grande
que certamente retomará logo depois, mas torna polls de segundos ou poucos minutos claramente
desfavoráveis.

**Confirmado:** cada poll volta ao modelo com contexto e pode gerar reasoning/output; a documentação
diz que o uso do Codex varia com esses fatores e publica a razão `0,10×` para cached input.
**Inferência conservadora:** o menor número de polls preserva mais allowance em média; um checkpoint
esparso só merece existir se evitar um miss grande. **Incerto/não publicado:** cache write e a
conversão exata de cada request na allowance incluída. Para ChatGPT, use `/status` e o dashboard de
uso para medir; não faça keepalive especulativo ou periódico sem um horizonte de retomada.

## Comparação com outros harnesses maduros

| Harness (commit inspecionado) | Como espera um job | Quando volta ao modelo |
|---|---|---|
| OpenHands SDK `9421149…` | A sessão de terminal faz polling de tela/processo **no host**, com soft/hard timeout; a descrição da tool orienta aumentar o timeout para comandos longos e usar input vazio apenas depois de um soft timeout ([descrição](https://github.com/OpenHands/software-agent-sdk/blob/9421149592da215066f58cb68cb04599d896ae74/openhands-tools/openhands/tools/terminal/descriptions.py#L13-L29), [loop host-side](https://github.com/OpenHands/software-agent-sdk/blob/9421149592da215066f58cb68cb04599d896ae74/openhands-tools/openhands/tools/terminal/terminal/terminal_session.py#L570-L638)). | O agent executa uma action pendente e retorna antes de chamar o LLM; a amostragem ocorre em outro passo ([`agent.py`](https://github.com/OpenHands/software-agent-sdk/blob/9421149592da215066f58cb68cb04599d896ae74/openhands-sdk/openhands/sdk/agent/agent.py#L637-L653), [chamada do LLM](https://github.com/OpenHands/software-agent-sdk/blob/9421149592da215066f58cb68cb04599d896ae74/openhands-sdk/openhands/sdk/agent/agent.py#L719-L726)). Espera longa na tool não consome passos do modelo; cada recuperação após soft timeout, sim. |
| SWE-agent `3ea751c…` | `communicate` aguarda sincronicamente a `BashAction`; defaults: 30 s por comando, 300 s para install, 1.800 s total e no máximo três timeouts ([config](https://github.com/SWE-agent/SWE-agent/blob/3ea751c087f32b16e039a2233dd6eefecef325d5/sweagent/tools/tools.py#L139-L151), [ambiente](https://github.com/SWE-agent/SWE-agent/blob/3ea751c087f32b16e039a2233dd6eefecef325d5/sweagent/environment/swe_env.py#L197-L222)). | Só depois da observação. Timeout interrompe/cancela a sessão em vez de iniciar um protocolo de polls retomáveis ([agent](https://github.com/SWE-agent/SWE-agent/blob/3ea751c087f32b16e039a2233dd6eefecef325d5/sweagent/agent/agents.py#L958-L989)); jobs longos exigem timeout adequado. |
| Aider `5dc9490…` | `/run` cria `subprocess.Popen`, drena stdout até EOF e espera o processo ([`run_cmd.py`](https://github.com/Aider-AI/aider/blob/5dc9490bb35f9729ef2c95d00a19ccd30c26339c/aider/run_cmd.py#L42-L84)). | Não há loop autônomo de polling do modelo durante o subprocesso. Depois, o usuário escolhe se adiciona a saída ao chat ([`commands.py`](https://github.com/Aider-AI/aider/blob/5dc9490bb35f9729ef2c95d00a19ccd30c26339c/aider/commands.py#L1013-L1044)). |

Os três desenhos reforçam a mesma separação: observar processo no host é barato para o modelo;
transformar uma observação intermediária em nova action/turn é o que cria custo de inferência.

## Recomendação concreta para o fork Electivus

### Blocos que o fork já possui

O fork está mais perto dessa capacidade do que o upstream puro:

- `clock.sleep` já aguarda no host por até 12 h e é interrompido por input novo
  ([`sleep.rs`](https://github.com/Electivus/electivus-codex/blob/48d4153a93f156445fa8e745bd62b9d778c79644/codex-rs/core/src/tools/handlers/sleep.rs#L26-L145)).
  Porém ele mantém a function call e a turn abertas. Isso é uma espera sem polling, não o contrato
  “encerre a turn agora, persista a espera e acorde depois”.
- O pending-work scheduler já consegue iniciar uma turn ociosa quando existe mailbox work e contém
  um gancho chamado `has_outstanding_durable_sleep`
  ([`tasks/mod.rs`](https://github.com/Electivus/electivus-codex/blob/48d4153a93f156445fa8e745bd62b9d778c79644/codex-rs/core/src/tasks/mod.rs#L435-L494)).
- A fila do fork já persiste input, descobre mudanças e acorda threads carregadas
  ([`ext/queue/service.rs`](https://github.com/Electivus/electivus-codex/blob/48d4153a93f156445fa8e745bd62b9d778c79644/codex-rs/ext/queue/src/service.rs#L70-L159),
  [`wake_if_loaded`](https://github.com/Electivus/electivus-codex/blob/48d4153a93f156445fa8e745bd62b9d778c79644/codex-rs/ext/queue/src/service.rs#L463-L476)).
- Unified exec já observa output e término por notificação, e a sessão é registrada antes do yield
  inicial para não perder o processo vivo
  ([`process_manager.rs`](https://github.com/Electivus/electivus-codex/blob/48d4153a93f156445fa8e745bd62b9d778c79644/codex-rs/core/src/unified_exec/process_manager.rs#L525-L574)).

Esses blocos sugerem aprofundar o scheduler de wake existente. Não vale criar um segundo loop que
apenas envolva `write_stdin`.

### Quick wins: configuração e política

1. **Subir a janela padrão não interativa de 30 s para 300 s e permitir até 25 min em um profile
   de comandos longos.** O fork não impõe teto de produto acima do configurado:

   ```toml
   [tool_execution.yield]
   min_ms = 10000
   default_ms = 300000
   max_ms = 1500000
   ```

   Cinco minutos é uma mudança conservadora para o default; 25 min permite ao modelo escolher uma
   observação próxima, mas abaixo, do TTL atual do GPT-5.6. O processo continua retornando
   imediatamente quando termina. Antes de promover 25 min a default global, validar chamadas
   silenciosas através de CLI, desktop, app-server/proxy, cancelamento e reconexão.

2. **Orientar o agente a pedir o maior yield útil já na primeira chamada** e a não chamar
   `write_stdin`/`wait` antes da janela terminar. Em code mode, compor e aguardar nested tools no
   mesmo `functions.exec`; usar `yield_control()` somente quando o usuário realmente precisa
   ver/intervir.

3. **Não usar poll como mecanismo de progresso.** Unified exec já transmite output por eventos
   enquanto a tool espera. Frontends devem renderizar esses eventos sem transformá-los em itens no
   contexto do modelo.

4. **Medir antes de automatizar keepalive:** por sessão/processo, contar yields, polls vazios,
   sampling requests, `cached_tokens`, `cache_write_tokens`, reasoning/output, compactações e tempo
   de reação a input. A métrica é “amostragens evitadas sem perder responsividade”, não só cache-hit.

5. **Oferecer um profile híbrido, não polling infinito.** Para GPT-5.6, uma conversa grande com ETA
   logo depois de 30 min pode fazer um único checkpoint em 25–29 min e então voltar à espera. Se a
   duração for incerta ou de horas, registrar wake de conclusão e deixar o cache expirar. A política
   de custo deve ficar fora do lifecycle do processo porque TTL e preços variam por modelo.

### Estágio 1: wake de conclusão, pequeno e upstreamável

A primeira mudança arquitetural recomendada é um modo opt-in de conclusão em `exec_command`, com
enum em vez de booleano, por exemplo `CompletionDisposition::{ObserveManually, WakeThread}`. Quando
o comando rende uma sessão viva com `WakeThread`, a turn pode terminar; o host continua observando o
processo e enfileira **um receipt terminal, limitado e deduplicado** quando ele sai. O scheduler inicia
uma única continuação quando a thread estiver ociosa.

Essa proposta coincide com a issue upstream aberta
[#32188, “Event-driven wakeup when background exec sessions complete”](https://github.com/openai/codex/issues/32188):
esperar sem inferência, acordar uma vez no exit, limitar output e impedir duplicação se
`write_stdin` já consumiu a conclusão. Há também a proposta mais geral de wait/wake durável para
goals em [#28144](https://github.com/openai/codex/issues/28144). São propostas abertas, não
funcionalidade confirmada do upstream atual.

O seam deve ficar no lifecycle de pending work já existente, não no loop de sampling. Unified exec
publica apenas um evento terminal estruturado; uma extensão/scheduler fora de `codex-core` possui a
política de persistir, coalescer e iniciar a continuação. Isso preserva locality do processo em
unified exec e locality da orquestração no scheduler, sem aumentar ainda mais a interface pública de
`codex-core`.

Não recomendo `wake_on_output` genérico. Um processo ruidoso ou output não confiável poderia disparar
turns sem limite. Uma proposta upstream desse tipo foi fechada como “not planned”
([#29865](https://github.com/openai/codex/issues/29865)). No primeiro estágio, acordar somente em
estado terminal; progresso permanece UI-only.

Contrato mínimo do receipt:

```text
registration_id, thread_id, process_id, terminal_state,
exit_code, elapsed, output_cursor, bounded_output_digest
```

O registro só fica armado depois de o primeiro resultado da tool estar persistido. Uma leitura manual
que observar o término deve fazer `claim` atômico do mesmo registro, impedindo wake posterior. Se o
exit ocorrer enquanto outra turn roda, o receipt fica enfileirado e coalescido; ele não interrompe
semanticamente a turn ativa.

### Estágio 2: scheduler durável como deep module

Depois de existirem pelo menos dois adapters reais — por exemplo process exit, timer, batch de
subagentes ou webhook — vale generalizar um deep module de wake fora de `codex-core`. Sua interface
deve ser pequena, aproximadamente `arm`, `cancel` e `claim`; condições, deduplicação, persistência,
coalescing e recuperação ficam escondidos na implementação. Adapters de processo, relógio e evento
externo convertem suas notificações para o mesmo receipt. O thread store é o port persistente e um
adapter em memória serve aos testes.

Pontos de implementação existentes:

- unified exec: requests hoje carregam `yield_time_ms` em
  [`unified_exec/mod.rs`](https://github.com/Electivus/electivus-codex/blob/48d4153a93f156445fa8e745bd62b9d778c79644/codex-rs/core/src/unified_exec/mod.rs#L68-L106),
  handlers resolvem a política em
  [`exec_command.rs`](https://github.com/Electivus/electivus-codex/blob/48d4153a93f156445fa8e745bd62b9d778c79644/codex-rs/core/src/tools/handlers/unified_exec/exec_command.rs#L380-L425)
  e
  [`write_stdin.rs`](https://github.com/Electivus/electivus-codex/blob/48d4153a93f156445fa8e745bd62b9d778c79644/codex-rs/core/src/tools/handlers/unified_exec/write_stdin.rs#L90-L121);
- a observação dentro da turn permanece privada junto de
  [`collect_output_until_deadline`](https://github.com/Electivus/electivus-codex/blob/48d4153a93f156445fa8e745bd62b9d778c79644/codex-rs/core/src/unified_exec/process_manager.rs#L1320-L1410),
  podendo ganhar `UntilCompletion` sem duplicar lifecycle/approvals;
- code mode: substituir/estender `ObserveMode::YieldAfter` em
  [`code-mode-runtime/src/service.rs`](https://github.com/Electivus/electivus-codex/blob/48d4153a93f156445fa8e745bd62b9d778c79644/codex-rs/code-mode-runtime/src/service.rs#L77-L149)
  e manter o facade de
  [`tools/code_mode/mod.rs`](https://github.com/Electivus/electivus-codex/blob/48d4153a93f156445fa8e745bd62b9d778c79644/codex-rs/core/src/tools/code_mode/mod.rs#L114-L132);
- o loop em `session/turn.rs` não precisa pollar: ele já aguarda a future da tool. Só deve receber
  `needs_follow_up` quando a observação resolver com um evento relevante.

Não misturar dois lifecycles na mesma interface:

- **observação dentro da turn:** `YieldAfter`/`UntilCompletion`, future da tool ainda pendente;
- **wake durável entre turns:** registro persistido, turn ociosa, nova continuação quando o evento
  chegar.

Para ser realmente **durável**, registro, condição, cursor e estado terminal devem sobreviver a
restart; reattach não pode duplicar o subprocesso nem o receipt. O fork já mantém a sessão viva antes
do yield inicial, mas isso é durabilidade em memória, não persistência completa. Se processo e
app-server vivem em hosts distintos, a identidade durável deve pertencer ao executor que realmente
possui o processo.

Uma versão upstreamável deve começar pelo modo opt-in de process exit e reutilizar as notificações
que já existem em `codex-rs/core/src/unified_exec`. A configuração global e defaults mais longos
podem permanecer como política do fork. A generalização para relógio/webhook deve ocorrer no
scheduler externo, não ampliando a interface pública de `codex-core` sem necessidade.

Critérios de segurança/aceitação:

- cancelamento, aprovação e stdin interativo interrompem/invalidam a espera prontamente;
- output model-visible é marcado como não confiável, limitado e sem duplicação;
- o receipt injetado no contexto é um `ContextualUserFragment` estruturado, com hard cap muito
  abaixo de 10 mil tokens;
- disconnect/reconnect e corrida exit/manual-poll entregam no máximo um receipt;
- um job silencioso de 20–30 min produz somente a inferência inicial e a continuação final;
- um job que atravessa restart recupera o registro ou termina com estado explícito `lost`, nunca
  fica silenciosamente pendurado;
- testes de integração contam requests `/responses` e exercitam a interface pública do scheduler;
- telemetria compara latência, sampling count, cached/cache-write/output tokens e compactações.

## Regra prática final

- **Codex/ChatGPT:** escolha o maior `yield` compatível com responsividade (o máximo embutido atual é
  300 s no fork e pode ser elevado por configuração), aguarde em foreground/eventos do host e só
  consulte de novo quando houver alta chance de término ou necessidade de intervenção. Não faça
  polling frequente para manter cache; um único checkpoint perto do TTL só é razoável para contexto
  grande, ETA conhecido e medição favorável.
- **Aplicação própria da API, espera <30 min:** uma espera host-side; zero keepalives.
- **API, espera >30 min com retomada certa:** use o menor número de keepalives perto do TTL, somente
  se a desigualdade medida justificar e com prefixo/chave estáveis.
- **Espera longa/indeterminada:** deixe expirar e pague uma reescrita quando realmente retomar; é mais
  previsível que gastar inferências periódicas.
- **Fork Electivus:** primeiro entregue `on_exit: wake` exato-once usando o pending-work scheduler;
  depois generalize timer/webhook em um scheduler persistente fora de `codex-core`.

## Classificação das conclusões

**Confirmado por código/documentação primária:** cada retorno de tool pede uma nova amostragem; o host
espera sem amostrar; o fork já tem yield configurável e espera por notificações; cache exige prefixo
exato; GPT-5.6 usa TTL de 30 min, read 0,1× e write 1,25×; cached input ainda pesa em TPM; planos ChatGPT
publicam estimativas variáveis, não uma fórmula exata de allowance.

**Inferência suportada:** diminuir polls preserva allowance do Codex em média; completion-driven wait
reduz custo sem prejudicar progress events; um keepalive perto do TTL pode economizar API — e talvez
créditos ChatGPT — quando o prefixo é grande, a retomada é certa e o overhead é pequeno.

**Incerto/não publicado:** conversão exata dos créditos na allowance incluída, tratamento de cache
write em ChatGPT e se todos os backends/modelos usados pelo produto Codex aplicam a mesma
contabilidade pública da API GPT-5.6. Essas incertezas impedem tornar keepalive periódico o default.
