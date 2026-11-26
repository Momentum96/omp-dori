<p align="center">
  <img src="docs/cataly-portrait.webp" width="320" alt="Porträt der Figur Cataly: silbernes Haar und ein purpurroter Heiligenschein">
</p>

<h1 align="center">Cataly</h1>

<p align="center">
  <strong>Die Absicht verstehen. Den Kontext erfassen. Die Arbeit zu Ende bringen.</strong>
</p>

<p align="center">
  Cataly ist ein KI-Teammitglied für Softwarearbeit. Gib ihm ein Ziel und die
  Bedingungen, auf die es ankommt, statt einer Anweisung für jeden Schritt. Es nimmt
  deine Anfragen in einem Discord-, Telegram- oder Slack-Raum entgegen, arbeitet in
  nachverfolgbaren <a href="https://omp.sh">OMP</a>-Coding-Sessions auf deinem
  Rechner und liefert ein Ergebnis, das du überprüfen kannst.
</p>

<p align="center">
  <a href="https://github.com/vntrevx/Cataly/releases/latest"><img alt="Neuestes Release" src="https://img.shields.io/github/v/release/vntrevx/Cataly?label=release&labelColor=2b2d33&color=c8102e"></a>
  <a href="https://www.npmjs.com/package/cataly"><img alt="npm" src="https://img.shields.io/npm/v/cataly?label=npm&labelColor=2b2d33&color=8b9099"></a>
  <a href="https://omp.sh"><img alt="OMP 19.1.1 oder neuer" src="https://img.shields.io/badge/omp-%E2%89%A519.1.1-8b9099?labelColor=2b2d33"></a>
  <a href="LICENSE"><img alt="Lizenz: AGPL-3.0" src="https://img.shields.io/badge/license-AGPL--3.0-8b9099?labelColor=2b2d33"></a>
</p>

<p align="center">
  <a href="#erste-schritte"><strong>Erste Schritte</strong></a> ·
  <a href="#so-funktioniert-es"><strong>So funktioniert es</strong></a> ·
  <a href="#im-alltag"><strong>Im Alltag</strong></a> ·
  <a href="#hilfe"><strong>Hilfe</strong></a>
</p>

<p align="center">
  <a href="README.md">English</a> · <a href="README.ko.md">한국어</a> · <a href="README.ja.md">日本語</a><br>
  <strong>Deutsch</strong> · <a href="README.it.md">Italiano</a> · <a href="README.zh.md">中文</a>
</p>

## Ein Ziel statt eines Drehbuchs

<table width="100%"><tr><td>

**Beispielanfrage**

*Untersuche den instabilen Login-Test. Lass die öffentliche API unverändert, führe
die relevanten Tests aus und öffne einen PR mit der Ursache und den Prüfergebnissen.*

</td></tr></table>

<table>
  <tr>
    <td width="33%" valign="top">
      <h4>Die Absicht verstehen</h4>
      Der instabile Test ist das Symptom. Das Ziel ist eine Korrektur, die du prüfen
      kannst, bei unveränderter öffentlicher API. Anhand dieser Unterscheidung
      entscheidet Cataly, was untersucht wird und was unangetastet bleibt.
    </td>
    <td width="33%" valign="top">
      <h4>Den Kontext erfassen</h4>
      Gespräch, Konventionen des Repositorys, Einschränkungen und deine genannten
      Vorlieben bestimmen den nächsten Schritt. Alltägliche Entscheidungen trifft
      Cataly; echte Abwägungen und Berechtigungsfragen entscheidest du.
    </td>
    <td width="33%" valign="top">
      <h4>Die Arbeit zu Ende bringen</h4>
      Ein Patch ist nicht das Ziel. Das vereinbarte Ergebnis prüfen, die Belege
      berichten und sagen, was noch unbestätigt ist.
    </td>
  </tr>
</table>

Der Unterschied ist nicht eine weitere Chat-Oberfläche. Es ist die Verbindung zwischen
der Anfrage, ihrer Arbeits-Session und dem Beleg, dass sie erledigt ist.

## So funktioniert es

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/how-it-works-dark.svg">
  <source media="(prefers-color-scheme: light)" srcset="docs/how-it-works-light.svg">
  <img src="docs/how-it-works-light.svg" width="100%" alt="Ablauf: Eine Anfrage trifft ein, der Lead weist sie zu, eine Lane arbeitet und berichtet, und der Abschluss wird geprüft, bevor ein überprüfbares Ergebnis samt Belegen zu dir zurückkommt.">
</picture>

1. **Eine Anfrage trifft ein.** Du schreibst in deinem Heimraum (`#cataly` nach der
   Einrichtung). Der Hub von Cataly läuft auf deinem Rechner und gibt deine
   Nachrichten samt Antwortkontext an den Lead weiter.
2. **Der Lead übernimmt.** Der Lead ist eine langlebige OMP-Session. Kleine Anfragen
   beantwortet er selbst, sonst öffnet er eine **Lane**: eine weitere OMP-Session in
   einem eigenen Worktree, mit einem Briefing und **Abschlussprüfungen** wie „der PR
   ist gemergt“ oder „dieser Befehl läuft erfolgreich“. Jede Lane bekommt einen
   eigenen Thread.
3. **Die Lane arbeitet und berichtet.** Lanes laufen parallel und behalten ihre
   Sessions über Neustarts und Updates hinweg, sodass die Arbeit dort weitergeht, wo
   sie war.
4. **Der Abschluss wird geprüft.** Dass eine Lane sich für fertig erklärt, ist kein
   Beweis. Cataly führt die Abschlussprüfungen aus und berichtet, was bestanden hat
   und was noch unbestätigt ist.

Neben der Unterhaltung hält ein Statusraum (`#cataly-ops`) ein einziges Live-Panel
bereit: Rechnerlast, Sessions, Provider-Nutzung und den Zustand deiner Watch-Jobs.
Watch-Jobs melden, was wissenswert ist: GitHub-Repositorys und Pull Requests,
Provider-Kontingente, Rechnerlast, neue E-Mails aus einem Befehl, Backups und
Zustand. Mit aktivierter Sprache kannst du auch in einem Discord-Sprachkanal mit dem
Lead sprechen.

## Du behältst die Kontrolle

- Cataly nutzt deine Konten, dein OMP und deine Berechtigungen. Initiative zu
  ergreifen erteilt keine Erlaubnis für externe Aktionen.
- Freigaben folgen den Einstellungen von OMP. Wenn OMP nachfragt, kommt die Frage als
  Buttons in deinen Raum, und du kannst auch in normalem Text antworten.
- Die Installation startet nichts und schreibt keine Konfiguration. `cataly setup`
  zeigt eine Zusammenfassung und ändert nichts, bis du zustimmst.
- Sprache ist aus, bis du sie einschaltest, und nutzt deinen eigenen OpenAI-Schlüssel.

## Erste Schritte

<table width="100%"><tr><td>

**Cataly 0.2.0 ist ein neues Produkt**

Releases vor v0.2.0 in diesem Repository und `cataly`-npm-Versionen vor 0.2.0 sind
ein früheres, eigenständiges Produkt. Von ihnen wird nichts übernommen: Ein Update
des npm-Pakets ersetzt den alten Befehl durch `cataly`. Führe `cataly setup` aus,
um neu zu beginnen.

</td></tr></table>

### Voraussetzungen

- Linux oder macOS, auf x86_64 oder arm64 (Apple Silicon und Intel-Macs). Windows wird nicht unterstützt.
- [OMP](https://omp.sh) 19.1.1 oder neuer.
- Ein Discord-Bot (die Einrichtung führt dich durch die Erstellung) oder ein Telegram- oder Slack-Bot.
- Für die Installation über npm und Bun: Node.js 18 oder neuer auf `PATH`.

### Installation

Wähle eine Variante:

```sh
curl -fsSL https://raw.githubusercontent.com/vntrevx/Cataly/main/install.sh | sh
```

```sh
npm install --global cataly
```

```sh
bun add --global cataly
```

`install.sh` lädt das neueste stabile Release herunter, prüft seine Hashes und seine
Signatur und legt `cataly` in `~/.local/bin` ab. npm und Bun installieren dasselbe
Binary aus dem Paket `cataly`.

### Erste Einrichtung

```sh
cataly setup
```

Die Einrichtung fragt nach deinem Bot-Token und gibt einen Einladungslink aus, dann
richtet sie den Rest ein: deinen Heimraum `#cataly`, den Statusraum `#cataly-ops`, das
Modell des Leads, Freigabe-Buttons, die Standard-Jobs und einen Benutzerdienst
(systemd unter Linux, ein launchd-Agent unter macOS), der den Hub startet. Danach schreib in `#cataly`.

`cataly setup --voice on` fügt Discord-Sprachanrufe mit sinnvollen Vorgaben hinzu; Anrufe kosten
Geld über deinen OpenAI-Schlüssel, begrenzt auf 30 Minuten und 3 USD am Tag.

## Im Alltag

| Befehl | Was er tut |
| --- | --- |
| `cataly status` | Hub, Lead und Lanes auf einen Blick. |
| `cataly health` | Was noch behoben werden muss, und wie. |
| `cataly update` | Stoppt den Hub geordnet (eine laufende Session wird nie abgeschnitten), installiert die neue Version, startet sie wieder und setzt Lead und Lanes fort. Antwortet die neue Version nicht, geht es zurück. |
| `cataly rollback` | Kehrt zur vorherigen Version zurück (Installationen über `install.sh`). |
| `cataly service disable`, dann `cataly uninstall` | Entfernt Dienst und Programm. Deine Konfiguration (`~/.config/cataly`) und dein Zustand (`~/.local/state/cataly`) bleiben erhalten. |

Wenn ein neues Release erscheint, sagt dir der Hub das einmal in deinem Heimraum.
Eine npm- oder Bun-Installation wird über den Paketmanager aktualisiert, aus dem sie
stammt.

<table width="100%"><tr><td>

**Frühe Version**

Am meisten genutzt ist Linux x86_64; macOS und arm64 bestehen dieselben Tests. Cataly ist täglich über Discord im Einsatz, Sprache
eingeschlossen; Telegram und Slack bestehen ihre Tests, wurden aber weniger real
genutzt.

</td></tr></table>

## Hilfe

Probleme und Ideen: [Issues](https://github.com/vntrevx/Cataly/issues). Bitte lass
Tokens, IDs und private Protokolle aus öffentlichen Berichten heraus.

Aus dem Quellcode bauen und mitwirken: siehe [CONTRIBUTING.md](CONTRIBUTING.md).

## Lizenz

Cataly ist Open Source unter der [GNU AGPL 3.0](LICENSE); der Quellcode liegt in diesem
Repository. Kommerzielle Lizenzen ohne die AGPL-Pflichten gibt es bei
[vntrevx](https://github.com/vntrevx). Cataly läuft auf
[oh-my-pi (OMP)](https://github.com/can1357/oh-my-pi), das separat installiert wird.
Lizenzen von Drittanbietern liegen jedem Release in `THIRD-PARTY-NOTICES-<target>.txt` bei.
