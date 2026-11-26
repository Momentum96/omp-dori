<p align="center">
  <img src="docs/cataly-portrait.webp" width="320" alt="Cataly のキャラクター肖像：銀色の髪と深紅の光輪">
</p>

<h1 align="center">Cataly</h1>

<p align="center">
  <strong>意図を理解し、文脈を読み取り、最後までやり遂げる。</strong>
</p>

<p align="center">
  Cataly は、ソフトウェア開発のための AI のチームメイトです。一つひとつの手順を指示する代わりに、目標と守るべき条件を伝えてください。Discord、Telegram、Slack のルームで依頼を受け取り、あなたのマシン上の追跡可能な <a href="https://omp.sh">OMP</a> コーディングセッションで作業し、あなた自身が確かめられる成果を返します。
</p>

<p align="center">
  <a href="https://github.com/vntrevx/Cataly/releases/latest"><img alt="最新リリース" src="https://img.shields.io/github/v/release/vntrevx/Cataly?label=release&labelColor=2b2d33&color=c8102e"></a>
  <a href="https://www.npmjs.com/package/cataly"><img alt="npm" src="https://img.shields.io/npm/v/cataly?label=npm&labelColor=2b2d33&color=8b9099"></a>
  <a href="https://omp.sh"><img alt="OMP 19.1.1 以降" src="https://img.shields.io/badge/omp-%E2%89%A519.1.1-8b9099?labelColor=2b2d33"></a>
  <a href="LICENSE"><img alt="ライセンス：AGPL-3.0" src="https://img.shields.io/badge/license-AGPL--3.0-8b9099?labelColor=2b2d33"></a>
</p>

<p align="center">
  <a href="#はじめに"><strong>はじめに</strong></a> ·
  <a href="#仕組み"><strong>仕組み</strong></a> ·
  <a href="#日常の使い方"><strong>日常の使い方</strong></a> ·
  <a href="#ヘルプ"><strong>ヘルプ</strong></a>
</p>

<p align="center">
  <a href="README.md">English</a> · <a href="README.ko.md">한국어</a> · <strong>日本語</strong><br>
  <a href="README.de.md">Deutsch</a> · <a href="README.it.md">Italiano</a> · <a href="README.zh.md">中文</a>
</p>

## 手順ではなく成果を任せる

<table width="100%"><tr><td>

**依頼の例**

*不安定なログインテストを調査して。公開 API は変えずに、関連するテストを実行し、原因と検証結果をまとめた PR を作成して。*

</td></tr></table>

<table>
  <tr>
    <td width="33%" valign="top">
      <h4>意図を理解する</h4>
      不安定なテストは症状にすぎません。目標は、公開 API を保ったまま、あなたがレビューできる修正です。Cataly はこの違いをもとに、何を調べ、何に手を付けないかを決めます。
    </td>
    <td width="33%" valign="top">
      <h4>文脈を読み取る</h4>
      会話、リポジトリの慣習、制約、あなたが伝えた好みが次の一手を決めます。日常的な判断は Cataly が担い、本当のトレードオフや権限に関わる判断はあなたが下します。
    </td>
    <td width="33%" valign="top">
      <h4>最後までやり遂げる</h4>
      パッチを書いたら終わりではありません。合意した成果を確認し、根拠を報告し、まだ検証できていないことを伝えます。
    </td>
  </tr>
</table>

違いは、新しいチャット画面にあるのではありません。依頼と、その作業セッションと、完了の根拠とを結びつけることにあります。

## 仕組み

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/how-it-works-dark.svg">
  <source media="(prefers-color-scheme: light)" srcset="docs/how-it-works-light.svg">
  <img src="docs/how-it-works-light.svg" width="100%" alt="ワークフロー：依頼が届き、リードが割り当て、レーンが作業して報告し、完了が確認されたうえで、検証できる成果が根拠とともにあなたへ戻ります。">
</picture>

1. **依頼が届く。** ホームルーム（セットアップ後は `#cataly`）に書き込みます。あなたのマシンで動く Cataly のハブが、返信の文脈を含めてメッセージをリードに渡します。
2. **リードが引き受ける。** リードは長時間動き続ける一つの OMP セッションです。小さな依頼は自分で答え、それ以外は**レーン**を開きます。レーンは専用の worktree で動く別の OMP セッションで、ブリーフと「PR がマージされた」「このコマンドが成功する」といった**完了チェック**を持ちます。レーンごとに専用のスレッドが作られます。
3. **レーンが作業して報告する。** レーンは並行して動き、再起動やアップデートをまたいでセッションを保つため、作業は中断したところから再開します。
4. **完了を確認する。** レーンが「終わった」と言っても証明にはなりません。Cataly は完了チェックを実行し、何が通り、何がまだ検証されていないかを報告します。

会話とは別に、ステータスルーム（`#cataly-ops`）がライブパネルを一つ保ちます。ホストの負荷、セッション、プロバイダーの使用量、ウォッチジョブの状態です。ウォッチジョブは知っておくべきことを投稿します。GitHub のリポジトリとプルリクエスト、プロバイダーのクォータ、ホストの負荷、コマンド経由の新着メール、バックアップ、ヘルスなどです。音声を有効にすれば、Discord のボイスチャンネルでリードと話すこともできます。

## 主導権はあなたに

- Cataly はあなたのアカウント、あなたの OMP、あなたの権限を使います。主体的に動くことが、外部への操作の許可になるわけではありません。
- 承認は OMP 自身の設定に従います。OMP が確認を求めると、その質問がボタンとしてルームに届き、普通のテキストで答えることもできます。
- インストールだけでは何も起動せず、設定も書き込みません。`cataly setup` は概要を表示し、あなたが同意するまで何も変更しません。
- 音声はあなたが有効にするまでオフで、あなた自身の OpenAI キーを使います。

## はじめに

<table width="100%"><tr><td>

**Cataly 0.2.0 は新しい製品です**

このリポジトリにある v0.2.0 より前のリリースと、0.2.0 より前の `cataly` npm バージョンは、以前の別製品です。そこから引き継がれるものはありません。npm パッケージを更新すると、古いコマンドが `cataly` に置き換わります。`cataly setup` を実行して新しく始めてください。

</td></tr></table>

### 必要なもの

- Linux x86_64（macOS arm64 は対応予定）。
- [OMP](https://omp.sh) 19.1.1 以降。
- Discord ボット（作り方はセットアップが案内します）、または Telegram か Slack のボット。
- npm と Bun でインストールする場合：`PATH` 上の Node.js 18 以降。

### インストール

いずれか一つを選びます。

```sh
curl -fsSL https://raw.githubusercontent.com/vntrevx/Cataly/main/install.sh | sh
```

```sh
npm install --global cataly
```

```sh
bun add --global cataly
```

`install.sh` は最新の安定版リリースをダウンロードし、ハッシュと署名を確認して、`cataly` を `~/.local/bin` に配置します。npm と Bun は `cataly` パッケージから同じバイナリをインストールします。

### 初回セットアップ

```sh
cataly setup
```

セットアップはボットトークンを尋ねて招待リンクを表示し、残りを整えます。ホームルーム `#cataly`、ステータスルーム `#cataly-ops`、リードのモデル、承認ボタン、既定のジョブ、そしてハブを起動するユーザーサービス（systemd）です。あとは `#cataly` に書き込むだけです。

## 日常の使い方

| コマンド | 内容 |
| --- | --- |
| `cataly status` | ハブ、リード、レーンの状態をひと目で確認します。 |
| `cataly health` | まだ直す必要があることと、その直し方を示します。 |
| `cataly update` | ハブを順序どおりに停止し（作業中のセッションが途中で切られることはありません）、新しいバージョンをインストールして再起動し、リードとレーンを再開します。新しいバージョンが応答しなければ元に戻します。 |
| `cataly rollback` | 一つ前のバージョンに戻します（`install.sh` でのインストール）。 |
| `cataly service disable` のあと `cataly uninstall` | サービスとプログラムを削除します。設定（`~/.config/cataly`）と状態（`~/.local/state/cataly`）は残ります。 |

新しいリリースが出ると、ハブがホームルームで一度だけ知らせます。npm や Bun でインストールした場合は、インストール元のパッケージマネージャーで更新します。

<table width="100%"><tr><td>

**初期リリース**

現時点では Linux x86_64 のみです。Cataly は音声を含め Discord で日常的に使われています。Telegram と Slack はテストに合格していますが、実際の利用はまだ多くありません。

</td></tr></table>

## ヘルプ

問題やアイデアは [Issues](https://github.com/vntrevx/Cataly/issues) へ。公開の報告には、トークン、ID、非公開のやり取りを含めないでください。

ソースからのビルドや貢献については [CONTRIBUTING.md](CONTRIBUTING.md) をご覧ください。

## ライセンス

Cataly は [GNU AGPL 3.0](LICENSE) のオープンソースです。ソースはこのリポジトリにあります。AGPL の義務なしで使いたい場合は、[vntrevx](https://github.com/vntrevx) に商用ライセンスをお問い合わせください。Cataly は別途インストールする [oh-my-pi (OMP)](https://github.com/can1357/oh-my-pi) 上で動作します。サードパーティのライセンスは、各リリースの `THIRD-PARTY-NOTICES.txt` に同梱されています。
