# Design: compartilhamento de áudio

## Intenção

Ampliar o Lan Mouse para encaminhar áudio em tempo real entre computadores autorizados na rede local. A pessoa configura cada computador conectado como remetente ou receptor; o áudio continua protegido pela identidade e autenticação de pares já usadas pelo Lan Mouse.

## Objetivos

- Suportar Windows e Linux, inclusive pares mistos Windows↔Linux.
- Permitir uma direção de transmissão por par de computadores por vez. Os dois lados precisam estar configurados com papéis compatíveis; uma combinação incompatível mostra estado de espera e não inicia o fluxo.
- No remetente, permitir enviar o áudio reproduzido pelo sistema e, opcionalmente, capturar também um microfone selecionado.
- No receptor, permitir escolher a saída local para o áudio recebido e apresentar o microfone remoto como um dispositivo de gravação virtual utilizável por outros programas.
- Manter o áudio do sistema e o microfone em fluxos separados. Proposta: ambos podem ser enviados simultaneamente na direção ativa; o microfone não é misturado aos alto-falantes.
- Salvar papel e escolhas de dispositivos por computador conectado, junto às preferências atuais de cliente.
- Indicar conexão, papel incompatível, dispositivo ausente e falha de captura/reprodução na interface.

## Escopo e limites

- A primeira versão não inclui macOS, captura por aplicativo, relay de áudio entre pares, ou transmissão simultânea nos dois sentidos do mesmo par.
- O escopo é áudio do sistema e microfone. Não inclui vídeo, controle remoto de volume do sistema ou efeitos de processamento de voz.
- A proposta para Linux cobre sessões PipeWire e PulseAudio puro através de uma interface cliente compatível com PulseAudio. Sistemas apenas ALSA/JACK ficam fora da primeira versão.
- A proposta para Windows inclui um driver de gravação virtual próprio, necessário para que aplicativos locais possam escolher o microfone remoto. Ele será um componente WDM separado do workspace Cargo e precisará de canal local entre o serviço Rust e o driver.
- Primeiro ciclo de implementação e teste do driver usa assinatura de teste em máquinas de desenvolvimento. Assinatura para distribuição pública e integração de instalador não estão incluídas nesta etapa; drivers de kernel precisam cumprir a política atual de assinatura do Windows.

## Interface e configuração

- Cada computador já aparece numa linha expansível na tela principal. Adicionar nela um grupo “Compartilhamento de áudio” com papel `Desativado`, `Enviar` ou `Receber`.
- Em `Enviar`, selecionar a saída local cujo mix será capturado e, opcionalmente, ativar o envio do microfone e selecionar o dispositivo de gravação.
- Em `Receber`, selecionar a saída local para reproduzir o áudio do sistema recebido e ativar ou desativar a publicação do microfone remoto como entrada virtual.
- Exibir o estado negociado com o par (por exemplo, “aguardando o outro computador configurar Receber”) e erros de dispositivo sem exigir leitura de logs.
- Persistir essas opções por cliente no TOML e encaminhá-las entre GTK e serviço pela IPC existente.

## Rede e protocolo

- Não transportar amostras de áudio como `ProtoEvent` nem reutilizar o protocolo fragmentado do clipboard: o primeiro tem tamanho fixo para eventos de entrada, e o segundo prioriza transferência de conteúdo, não latência contínua.
- Adicionar uma via de mídia de baixa latência, com identificação de faixas (sistema/microfone), sequência, tempo e limites de buffer. Reutilizar a identidade autenticada do par Lan Mouse; nunca retransmitir áudio por outro par.
- Negociar capacidade e papel de áudio explicitamente. Pares antigos continuam compartilhando entrada e clipboard sem iniciar áudio; áudio exige suporte nos dois lados.
- Codec, enquadramento, jitter buffer, recuperação de perda e se a via de mídia multiplexa o DTLS atual ou abre conexão derivada ficam para o plano de implementação, após prova de conceito com Windows e PipeWire.
- Desconexão, troca de papel, pausa ou remoção de cliente interrompem captura, reprodução e encaminhamento ao driver e limpam buffers pendentes.

## Plataformas

- **Windows:** WASAPI para listar dispositivos, capturar mix de saída e microfone e reproduzir áudio. Um driver WDM apresenta o microfone remoto como endpoint de gravação e recebe áudio do serviço em user mode. A instalação exige privilégio administrativo; desenvolvimento usa test signing. Distribuição normal requer assinatura de produção pelo fluxo Microsoft.
- **Linux:** Interface cliente compatível com PulseAudio para enumerar fontes/saídas, capturar o monitor de saída, reproduzir áudio e criar uma fonte virtual por par para o microfone recebido. Isso deve cobrir PulseAudio puro e PipeWire através do seu serviço de compatibilidade PulseAudio. A disponibilidade do servidor e as permissões de sessão precisam ser verificadas em runtime.

## Abordagens consideradas

1. **Dispositivos virtuais mantidos pelo Lan Mouse (escolhida).** Criar a fonte virtual Windows com driver WDM próprio e usar capacidades do PipeWire no Linux. Dá ao Lan Mouse controle do ciclo de vida e evita dependência de um driver de terceiros. Exige projeto, testes, assinatura e distribuição do driver Windows.
2. **Dispositivo virtual externo no Windows.** O Lan Mouse captura/reproduz áudio e encaminha o microfone remoto para um driver virtual instalado à parte. Reduz o escopo do projeto, mas exige dependência e configuração manual de terceiros. Não foi escolhida.

## Arquitetura atual que será alterada

- A GUI GTK representa computadores como `ClientRow`; a IPC entre frontend e serviço usa `FrontendRequest`/`FrontendEvent`.
- O `ConfigClient` e sua estrutura TOML guardam preferências por computador, incluindo host, porta e posição.
- O serviço gerencia clientes e tráfego entre uma conexão de saída e conexões de entrada autenticadas por DTLS.
- `lan-mouse-proto` codifica eventos pequenos de teclado/mouse; clipboard usa uma via separada com quadros de até 1200 bytes. Áudio requer novas abstrações de mídia e capacidades.
- Não existe hoje pipeline de áudio, projeto de driver Windows, nem pacote/instalador do driver. O workflow atual do Windows publica um ZIP com executável e DLLs GTK.

## Decisões de projeto propostas para revisão

- Cada par pode enviar áudio do sistema e microfone simultaneamente na direção ativa, em faixas separadas.
- Cada par tem papel próprio salvo no computador local; para iniciar, o outro endpoint precisa indicar o papel complementar.
- Um microfone virtual recebido deve ser uma entrada selecionável por outros programas, não apenas áudio misturado nos alto-falantes do Lan Mouse.
- Linux inclui PipeWire e PulseAudio puro usando a interface cliente compatível; sistemas apenas ALSA/JACK ficam fora do escopo inicial.
- O driver Windows é desenvolvido e testado localmente com assinatura de teste; assinar e publicar o driver para usuários finais é uma etapa posterior.

## O que fica para o plano de implementação

- Escolher crate/API de áudio Rust que funcione nos alvos Windows e Linux, codec, formatos e tamanhos de bloco.
- Especificar o contrato do driver (formato PCM, buffers, sincronização, comportamento em pausa/desconexão e atualização do driver).
- Definir a negociação de capacidades e a via de mídia com compatibilidade para pares sem suporte.
- Definir política para múltiplos pares recebendo ao mesmo tempo e nomes de fontes virtuais por par.
- Verificar hotplug, permissões e formatos de dispositivo nos ambientes Windows, PipeWire e PulseAudio.
- Atualizar README/DOC com instalação e limitações depois que o escopo de empacotamento e assinatura for decidido.

## Referências técnicas

- [WASAPI loopback recording](https://learn.microsoft.com/en-us/windows/win32/coreaudio/loopback-recording)
- [Microsoft SYSVAD virtual audio driver sample](https://learn.microsoft.com/en-us/samples/microsoft/windows-driver-samples/sysvad-virtual-audio-device-driver-sample/)
- [Windows kernel-mode signing requirements](https://learn.microsoft.com/en-us/windows-hardware/drivers/install/kernel-mode-code-signing-requirements--windows-vista-and-later-)
- [PipeWire virtual sink](https://docs.pipewire.org/page_pulse_module_virtual_sink.html)
- [PulseAudio virtual source and modules](https://wiki.freedesktop.org/www/Software/PulseAudio/Documentation/User/Modules/)
- [PipeWire PulseAudio replacement](https://docs.pipewire.org/devel/page_man_pipewire-pulse_1.html)
- [PipeWire PulseAudio modules, including virtual source](https://docs.pipewire.org/page_pulse_modules.html)
