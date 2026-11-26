<p align="center">
  <img src="docs/cataly-portrait.webp" width="320" alt="Ritratto del personaggio Cataly: capelli argentati e un'aureola cremisi">
</p>

<h1 align="center">Cataly</h1>

<p align="center">
  <strong>Capire l'intento. Leggere il contesto. Portare il lavoro fino in fondo.</strong>
</p>

<p align="center">
  Cataly è un compagno di squadra AI per il lavoro sul software. Dagli un obiettivo e
  i vincoli che contano, non un comando per ogni passo. Riceve le tue richieste in una
  stanza Discord, Telegram o Slack, lavora in sessioni di coding
  <a href="https://omp.sh">OMP</a> tracciate sulla tua macchina e ti restituisce un
  risultato che puoi verificare.
</p>

<p align="center">
  <a href="https://github.com/vntrevx/Cataly/releases/latest"><img alt="Ultima release" src="https://img.shields.io/github/v/release/vntrevx/Cataly?label=release&labelColor=2b2d33&color=c8102e"></a>
  <a href="https://www.npmjs.com/package/cataly"><img alt="npm" src="https://img.shields.io/npm/v/cataly?label=npm&labelColor=2b2d33&color=8b9099"></a>
  <a href="https://omp.sh"><img alt="OMP 19.1.1 o successivo" src="https://img.shields.io/badge/omp-%E2%89%A519.1.1-8b9099?labelColor=2b2d33"></a>
  <a href="LICENSE"><img alt="Licenza: AGPL-3.0" src="https://img.shields.io/badge/license-AGPL--3.0-8b9099?labelColor=2b2d33"></a>
</p>

<p align="center">
  <a href="#per-iniziare"><strong>Per iniziare</strong></a> ·
  <a href="#come-funziona"><strong>Come funziona</strong></a> ·
  <a href="#uso-quotidiano"><strong>Uso quotidiano</strong></a> ·
  <a href="#aiuto"><strong>Aiuto</strong></a>
</p>

<p align="center">
  <a href="README.md">English</a> · <a href="README.ko.md">한국어</a> · <a href="README.ja.md">日本語</a><br>
  <a href="README.de.md">Deutsch</a> · <strong>Italiano</strong> · <a href="README.zh.md">中文</a>
</p>

## Un risultato da ottenere, non un copione

<table width="100%"><tr><td>

**Esempio di richiesta**

*Indaga sul test di login instabile. Lascia invariata l'API pubblica, esegui i test
pertinenti e apri una PR con la causa e i risultati della verifica.*

</td></tr></table>

<table>
  <tr>
    <td width="33%" valign="top">
      <h4>Capire l'intento</h4>
      Il test instabile è il sintomo. L'obiettivo è una correzione che puoi
      revisionare, con l'API pubblica intatta. Cataly usa questa distinzione per
      scegliere cosa indagare e cosa lasciare stare.
    </td>
    <td width="33%" valign="top">
      <h4>Leggere il contesto</h4>
      La conversazione, le convenzioni del repository, i vincoli e le preferenze che
      hai espresso orientano il passo successivo. Le decisioni di routine spettano a
      Cataly; i veri compromessi e le decisioni sui permessi spettano a te.
    </td>
    <td width="33%" valign="top">
      <h4>Portare il lavoro fino in fondo</h4>
      Una patch non è il traguardo. Verificare il risultato concordato, riportare le
      prove e dire cosa resta non verificato.
    </td>
  </tr>
</table>

La differenza non è un'altra interfaccia di chat. È il legame tra la richiesta, la
sua sessione di lavoro e la prova che è stata completata.

## Come funziona

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/how-it-works-dark.svg">
  <source media="(prefers-color-scheme: light)" srcset="docs/how-it-works-light.svg">
  <img src="docs/how-it-works-light.svg" width="100%" alt="Flusso di lavoro: arriva una richiesta, il lead la assegna, una lane lavora e riferisce, e il completamento viene verificato prima che ti torni un risultato verificabile, con le relative prove.">
</picture>

1. **Arriva una richiesta.** Scrivi nella tua stanza principale (`#cataly` dopo la
   configurazione). L'hub di Cataly gira sulla tua macchina e passa i tuoi messaggi,
   con il contesto delle risposte, al lead.
2. **Il lead la prende in carico.** Il lead è una sessione OMP di lunga durata.
   Risponde da sé alle richieste piccole, oppure apre una **lane**: un'altra sessione
   OMP nel proprio worktree, con un brief e dei **controlli di completamento** come
   "la PR è stata unita" o "questo comando va a buon fine". Ogni lane ha il proprio
   thread.
3. **La lane lavora e riferisce.** Le lane girano in parallelo e mantengono le loro
   sessioni attraverso riavvii e aggiornamenti, così il lavoro riprende da dove era.
4. **Il completamento viene verificato.** Una lane che dice di aver finito non è una
   prova. Cataly esegue i controlli di completamento e riferisce cosa è passato e
   cosa resta non verificato.

Accanto alla conversazione, una stanza di stato (`#cataly-ops`) mantiene un unico
pannello in tempo reale: carico della macchina, sessioni, uso dei provider e lo stato
dei tuoi job di monitoraggio. I job di monitoraggio pubblicano ciò che vale la pena
sapere: repository e pull request GitHub, quote dei provider, carico della macchina,
nuova posta da un comando, backup e stato di salute. Con la voce attivata puoi anche
parlare con il lead in un canale vocale Discord.

## Il controllo resta a te

- Cataly usa i tuoi account, il tuo OMP e i tuoi permessi. Prendere l'iniziativa non
  concede il permesso di compiere azioni esterne.
- Le approvazioni seguono le impostazioni di OMP. Quando OMP chiede, la domanda arriva
  nella tua stanza come pulsanti, e puoi anche rispondere in testo normale.
- L'installazione non avvia nulla e non scrive alcuna configurazione. `cataly setup`
  mostra un riepilogo e non cambia nulla finché non dici di sì.
- La voce è disattivata finché non la attivi e usa la tua chiave OpenAI.

## Per iniziare

<table width="100%"><tr><td>

**Cataly 0.2.0 è un prodotto nuovo**

Le release precedenti alla v0.2.0 in questo repository e le versioni npm di `cataly`
precedenti alla 0.2.0 sono un prodotto precedente e distinto. Da esse non viene
ripreso nulla: aggiornare il pacchetto npm sostituisce il vecchio comando con
`cataly`. Esegui `cataly setup` per ripartire da zero.

</td></tr></table>

### Requisiti

- Linux o macOS, su x86_64 o arm64 (Apple Silicon e Mac Intel). Windows non è supportato.
- [OMP](https://omp.sh) 19.1.1 o successivo.
- Un bot Discord (la configurazione ti guida nella creazione) oppure un bot Telegram o Slack.
- Per le installazioni con npm e Bun: Node.js 18 o successivo su `PATH`.

### Installazione

Scegline una:

```sh
curl -fsSL https://raw.githubusercontent.com/vntrevx/Cataly/main/install.sh | sh
```

```sh
npm install --global cataly
```

```sh
bun add --global cataly
```

`install.sh` scarica l'ultima release stabile, ne controlla gli hash e la firma e
mette `cataly` in `~/.local/bin`. npm e Bun installano lo stesso binario dal
pacchetto `cataly`.

### Prima configurazione

```sh
cataly setup
```

La configurazione chiede il token del bot e stampa un link di invito, poi prepara il
resto: la tua stanza principale `#cataly`, la stanza di stato `#cataly-ops`, il
modello del lead, i pulsanti di approvazione, i job predefiniti e un servizio utente
(systemd su Linux, un agente launchd su macOS) che avvia l'hub. Poi scrivi in `#cataly`.

`cataly setup --voice on` aggiunge le chiamate vocali su Discord con impostazioni predefinite
sensate; le chiamate costano denaro sulla tua chiave OpenAI, con un limite di 30 minuti e 3 USD al giorno.

## Uso quotidiano

| Comando | Cosa fa |
| --- | --- |
| `cataly status` | L'hub, il lead e le lane a colpo d'occhio. |
| `cataly health` | Cosa resta da sistemare, e come. |
| `cataly update` | Arresta l'hub in ordine (una sessione al lavoro non viene mai interrotta), installa la nuova versione, la riavvia e riprende il lead e le lane. Se la nuova versione non risponde, torna indietro. |
| `cataly rollback` | Torna alla versione precedente (installazioni con `install.sh`). |
| `cataly service disable`, poi `cataly uninstall` | Rimuove il servizio e il programma. La tua configurazione (`~/.config/cataly`) e il tuo stato (`~/.local/state/cataly`) vengono conservati. |

Quando esce una nuova release, l'hub te lo dice una volta nella tua stanza principale.
Un'installazione con npm o Bun si aggiorna tramite il gestore di pacchetti da cui
proviene.

<table width="100%"><tr><td>

**Versione iniziale**

Linux x86_64 è il più usato; macOS e arm64 superano gli stessi test. Cataly è in uso quotidiano tramite Discord, voce inclusa;
Telegram e Slack superano i loro test ma sono stati usati meno sul campo.

</td></tr></table>

## Aiuto

Problemi e idee: [Issues](https://github.com/vntrevx/Cataly/issues). Per favore non
includere token, ID e trascrizioni private nelle segnalazioni pubbliche.

Compilare dal sorgente e contribuire: vedi [CONTRIBUTING.md](CONTRIBUTING.md).

## Licenza

Cataly è open source sotto la [GNU AGPL 3.0](LICENSE); il sorgente è in questo
repository. Licenze commerciali senza gli obblighi della AGPL sono disponibili da
[vntrevx](https://github.com/vntrevx). Cataly gira su
[oh-my-pi (OMP)](https://github.com/can1357/oh-my-pi), installato separatamente.
Le licenze di terze parti sono incluse in ogni release in `THIRD-PARTY-NOTICES-<target>.txt`.
