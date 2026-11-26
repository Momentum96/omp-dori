<p align="center">
  <img src="docs/cataly-portrait.webp" width="320" alt="Cataly 角色肖像：银色头发与深红色光环">
</p>

<h1 align="center">Cataly</h1>

<p align="center">
  <strong>理解意图。读懂上下文。把工作做到底。</strong>
</p>

<p align="center">
  Cataly 是面向软件工作的 AI 队友。交给它一个目标和需要遵守的约束，而不是每一步的指令。它在 Discord、Telegram 或 Slack 房间里接收你的请求，在你机器上可追踪的 <a href="https://omp.sh">OMP</a> 编码会话中工作，并交回你可以核验的结果。
</p>

<p align="center">
  <a href="https://github.com/vntrevx/Cataly/releases/latest"><img alt="最新发行版" src="https://img.shields.io/github/v/release/vntrevx/Cataly?label=release&labelColor=2b2d33&color=c8102e"></a>
  <a href="https://www.npmjs.com/package/cataly"><img alt="npm" src="https://img.shields.io/npm/v/cataly?label=npm&labelColor=2b2d33&color=8b9099"></a>
  <a href="https://omp.sh"><img alt="OMP 19.1.1 或更高版本" src="https://img.shields.io/badge/omp-%E2%89%A519.1.1-8b9099?labelColor=2b2d33"></a>
  <a href="LICENSE"><img alt="许可证：AGPL-3.0" src="https://img.shields.io/badge/license-AGPL--3.0-8b9099?labelColor=2b2d33"></a>
</p>

<p align="center">
  <a href="#从这里开始"><strong>从这里开始</strong></a> ·
  <a href="#工作方式"><strong>工作方式</strong></a> ·
  <a href="#日常使用"><strong>日常使用</strong></a> ·
  <a href="#帮助"><strong>帮助</strong></a>
</p>

<p align="center">
  <a href="README.md">English</a> · <a href="README.ko.md">한국어</a> · <a href="README.ja.md">日本語</a><br>
  <a href="README.de.md">Deutsch</a> · <a href="README.it.md">Italiano</a> · <strong>中文</strong>
</p>

## 交给它结果而不是脚本

<table width="100%"><tr><td>

**请求示例**

*排查那个时好时坏的登录测试。保持公共 API 不变，运行相关测试，并提交一个写明原因和验证结果的 PR。*

</td></tr></table>

<table>
  <tr>
    <td width="33%" valign="top">
      <h4>理解意图</h4>
      时好时坏的测试只是症状。目标是一个你可以审查、且公共 API 保持不变的修复。Cataly 依据这一区别来决定该调查什么、不该动什么。
    </td>
    <td width="33%" valign="top">
      <h4>读懂上下文</h4>
      对话、仓库约定、约束条件以及你表达过的偏好，共同决定下一步。常规决定由 Cataly 负责；真正的取舍和权限决定由你来做。
    </td>
    <td width="33%" valign="top">
      <h4>把工作做到底</h4>
      写出补丁并不是终点。核对约定的结果，报告证据，并说明哪些内容尚未验证。
    </td>
  </tr>
</table>

区别不在于又多了一个聊天界面，而在于把请求、它的工作会话和完成的证据连在一起。

## 工作方式

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/how-it-works-dark.svg">
  <source media="(prefers-color-scheme: light)" srcset="docs/how-it-works-light.svg">
  <img src="docs/how-it-works-light.svg" width="100%" alt="工作流程：请求到达，lead 分派任务，lane 工作并汇报，完成情况经过检查后，一个可核验的结果连同证据回到你手中。">
</picture>

1. **请求到达。** 你在主房间（设置完成后为 `#cataly`）里发消息。Cataly 的 hub 运行在你的机器上，把你的消息连同回复上下文转交给 lead。
2. **lead 接手。** lead 是一个长期运行的 OMP 会话。小请求它自己回答；否则就开一条 **lane**：在独立 worktree 中运行的另一个 OMP 会话，带有一份简报和**完成检查**，例如“PR 已合并”或“这条命令执行成功”。每条 lane 都有自己的线程。
3. **lane 工作并汇报。** lane 并行运行，并在重启和更新之后保留各自的会话，因此工作会从中断处继续。
4. **检查是否完成。** lane 说自己完成了并不能算证明。Cataly 会运行完成检查，报告哪些通过、哪些仍未验证。

在对话之外，状态房间（`#cataly-ops`）维护一个实时面板：主机负载、会话、提供商用量以及监视任务的状态。监视任务会发布值得关注的信息：GitHub 仓库和拉取请求、提供商配额、主机负载、通过命令收到的新邮件、备份和健康状况。开启语音后，你还可以在 Discord 语音频道里与 lead 交谈。

## 掌控权在你

- Cataly 使用你的账号、你的 OMP 和你的权限。主动行事并不等于获得了执行外部操作的许可。
- 审批遵循 OMP 自身的设置。当 OMP 需要确认时，问题会以按钮的形式出现在你的房间里，你也可以直接用文字回答。
- 安装不会启动任何东西，也不会写入任何配置。`cataly setup` 会先显示摘要，在你同意之前不做任何更改。
- 语音默认关闭，需要你自己开启，并使用你自己的 OpenAI 密钥。

## 从这里开始

<table width="100%"><tr><td>

**Cataly 0.2.0 是一个新产品**

本仓库中 v0.2.0 之前的发布版本以及 0.2.0 之前的 `cataly` npm 版本是一个更早的、独立的产品。它们的任何内容都不会被沿用：更新 npm 包会用 `cataly` 替换旧的命令。运行 `cataly setup` 重新开始。

</td></tr></table>

### 环境要求

- Linux 或 macOS，x86_64 或 arm64（Apple Silicon 与 Intel Mac）。不支持 Windows。
- [OMP](https://omp.sh) 19.1.1 或更高版本。
- 一个 Discord 机器人（设置过程会引导你创建），或一个 Telegram 或 Slack 机器人。
- 使用 npm 和 Bun 安装时：`PATH` 中有 Node.js 18 或更高版本。

### 安装

任选其一：

```sh
curl -fsSL https://raw.githubusercontent.com/vntrevx/Cataly/main/install.sh | sh
```

```sh
npm install --global cataly
```

```sh
bun add --global cataly
```

`install.sh` 会下载最新的稳定发行版，校验其哈希和签名，并把 `cataly` 放到 `~/.local/bin`。npm 和 Bun 从 `cataly` 包安装同一个二进制文件。

### 首次设置

```sh
cataly setup
```

设置会询问你的机器人令牌并打印邀请链接，然后配置其余部分：主房间 `#cataly`、状态房间 `#cataly-ops`、lead 的模型、审批按钮、默认任务，以及一个启动 hub 的用户服务（Linux 上为 systemd，macOS 上为 launchd agent）。之后在 `#cataly` 里发消息即可。

## 日常使用

| 命令 | 作用 |
| --- | --- |
| `cataly status` | 一眼查看 hub、lead 和各条 lane。 |
| `cataly health` | 还有什么需要修复，以及如何修复。 |
| `cataly update` | 有序停止 hub（正在工作的会话绝不会被打断），安装新版本，重新启动并恢复 lead 和 lane。如果新版本没有响应，就回退。 |
| `cataly rollback` | 退回到上一个版本（适用于 `install.sh` 安装）。 |
| 先 `cataly service disable` 再 `cataly uninstall` | 移除服务和程序。你的配置（`~/.config/cataly`）和状态（`~/.local/state/cataly`）会保留。 |

有新发行版时，hub 会在你的主房间里通知一次。通过 npm 或 Bun 安装的，请用原来的包管理器更新。

<table width="100%"><tr><td>

**早期版本**

使用最多的是 Linux x86_64；macOS 和 arm64 通过同样的测试。Cataly 每天都在通过 Discord 使用，包括语音；Telegram 和 Slack 通过了测试，但实际使用较少。

</td></tr></table>

## 帮助

问题和想法：[Issues](https://github.com/vntrevx/Cataly/issues)。请不要在公开报告中包含令牌、ID 和私人对话记录。

从源代码构建或参与贡献：请参阅 [CONTRIBUTING.md](CONTRIBUTING.md)。

## 许可证

Cataly 是基于 [GNU AGPL 3.0](LICENSE) 的开源软件，源代码就在本仓库中。如需不受 AGPL 义务约束的商业许可，请联系 [vntrevx](https://github.com/vntrevx)。Cataly 运行在需单独安装的 [oh-my-pi (OMP)](https://github.com/can1357/oh-my-pi) 之上。第三方许可证随每个发行版附带于 `THIRD-PARTY-NOTICES-<target>.txt` 中。
