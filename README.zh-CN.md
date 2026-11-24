<p align="center"><a href="README.md">English</a> | 中文</p>

<div align="center">
  <img src="crates/wu/resources/app-icon.png" alt="WuIDE" width="128">
  <h1>WuIDE</h1>
  <p>快速的原生代码编辑器，内置编程智能体。</p>
  <p>用 Rust 编写，开箱即用。</p>
  <p><a href="#下载"><strong>下载</strong></a> · <a href="./FORK.md">相对上游改了什么</a></p>
</div>

---

> **这是个人分叉。** 基于 [dstrml/wu](https://github.com/dstrml/wu) 构建，而后者又是 [Zed](https://github.com/zed-industries/zed) 的分叉。本仓库做了修改：产品改名为 **WuIDE**，内置智能体列表有增减，并自带打包好的智能体运行时。完整改动见 [FORK.md](./FORK.md)。这里的发行版由本仓库 CI 从打了 tag 的提交构建，对应源码就是本仓库。

WuIDE 是一款用 Rust 编写的快速原生代码编辑器，界面简洁现代，并内置了编程智能体。它是 Zed 的分叉，但已经和它有了很大的不同。

## 功能

- ⚡ **原生、快速。** 不用 Electron，也没有网页视图。打开就能用，大型视图下也一直流畅。
- 🍃 **轻量。** 内存占用比 VS Code 和 Zed 都少。查看[测试结果](./docs/memory-benchmark.md)。
- 👾 **内置编程智能体。** 直接支持 Claude Code、Codex、OpenCode、DeepSeek 和 Pi，每种都能在设置里单独开关。
- 🧠 **智能体按需启动。** 每个智能体进程只在一轮对话期间存在，一轮结束就释放，不常驻后台。
- ✨ **默认就好看。** 外观简洁现代，用起来也顺手。
- 🤫 **无需账号。** 不用登录任何东西，也没有内置统计上报或崩溃上报。

## 下载

发行版在本仓库的 [releases 页面](https://github.com/Fenghu146/wuIDE/releases/latest)。

- [macOS（Apple 芯片）](https://github.com/Fenghu146/wuIDE/releases/latest/download/IDE-aarch64.dmg)
- [macOS（Intel 芯片）](https://github.com/Fenghu146/wuIDE/releases/latest/download/IDE-x86_64.dmg)
- [Windows x86-64](https://github.com/Fenghu146/wuIDE/releases/latest/download/IDE-x86_64.exe)
- [Linux x86-64](https://github.com/Fenghu146/wuIDE/releases/latest/download/ide-linux-x86_64.tar.gz)
- [Linux AArch64](https://github.com/Fenghu146/wuIDE/releases/latest/download/ide-linux-aarch64.tar.gz)

在 macOS 和 Linux 上也可以直接运行：

```sh
curl -fsSL https://raw.githubusercontent.com/Fenghu146/wuIDE/main/script/install.sh | sh
```

### 构建产物没有代码签名

这些发行版背后没有 Apple 开发者证书，也没有 Windows 代码签名证书，所以两个平台第一次打开都会拦一下：

- **macOS** 下载后会被拦。要么先打开一次，然后到 **系统设置 → 隐私与安全性 → 仍要打开** 放行；要么清掉隔离属性：

  ```sh
  xattr -d com.apple.quarantine /Applications/IDE.app
  ```

- **Windows** 会弹 SmartScreen。点 **更多信息**，再点 **仍要运行**。

正因为没有签名，**运行前请核对下载文件的 SHA-256**。每个发行资产都有 GitHub 在 release 页上给出的 digest，本地比对一下：

```sh
shasum -a 256 IDE-aarch64.dmg        # macOS
sha256sum IDE-x86_64.exe             # Linux
```

各平台的细节见 [docs/INSTALL.md](./docs/INSTALL.md)。

## 更新

自动更新**默认开启**，查的是本仓库自己的 release，不是上游的。启动时查一次，之后每 12 小时一次。它只负责通知：标题栏出现**有可用更新**，同时弹一张带**下载并安装** / **跳过这个版本**的通知卡。不点就不会装。

有两点需要知道：

- 下载会用 GitHub 给该资产发布的 SHA-256 digest 校验。这能发现传输损坏，但**不能证明文件是谁构建的**。如果你不想信任一次下载来的更新，就在设置里关掉 `auto_update`，手动核对 digest 后自己装。
- 这个仓库必须保持 **public**，更新才能用：客户端是匿名拉 release 元数据的，私有仓库的 release 不带 token 会 404。

## 网络访问

WuIDE 不做统计上报、不发崩溃报告、不采集使用遥测。它确实会发起下面这些出站请求，都可以在源码里查证：

| 用途 | 目标 | 时机 |
| --- | --- | --- |
| 检查更新用的 release 元数据 | `api.github.com/repos/Fenghu146/wuIDE/releases` | 启动时一次，之后每 12 小时 |
| 扩展索引与扩展下载 | `api.zed.dev`（经 `server_url` 设置，目前仍是 `https://zed.dev`） | 打开扩展视图，或某种语言需要扩展时 |
| 下载 Node 运行时 | `nodejs.org`，只在打包时，运行期不会 | 构建过程中 |
| 智能体自己的 API 请求 | 你自己配置的供应商 | 你给智能体发消息时 |

如果不想让编辑器访问扩展仓库，可以把设置里的 `server_url` 指向你自己控制的服务，或者干脆不打开扩展视图。

## 构建

WuIDE 的构建方式和 Zed 一样。请看 [Zed 开发文档](https://zed.dev/docs/development)、[FORK.md](./FORK.md) 里分叉相关的打包说明（内置智能体运行时、图标与打包脚本），以及 [SYNC.md](./SYNC.md) 里跟进上游的方式。

## 许可证

WuIDE 以 **GPL-3.0-or-later** 分发，标注的部分采用 Apache-2.0。请看 [LICENSE-GPL](./LICENSE-GPL)、[LICENSE-APACHE](./LICENSE-APACHE)，以及 [NOTICE](./NOTICE) 里本分叉保留的上游署名。

随包分发的第三方运行时各有自己的许可证，会一起附带：

- Node.js —— MIT，外加它自身依赖的许可证
- `@brokkai/dsh-acp` —— MIT
- `pi-acp` —— MIT
- `@earendil-works/pi-coding-agent` —— MIT

这是 GPL 软件，所以拿到你构建产物的人有权获得对应源码。对应源码就是本仓库在发行 tag 上的代码——分享构建产物时，请把 tag 一起告诉对方。

## 致谢

感谢 [Zed](https://github.com/zed-industries/zed) 团队打造了这样出色的编辑器并把它开源，也感谢 [dstrml](https://github.com/dstrml/wu) 做出这个以内存为目标的、本仓库所基于的分叉。两个项目都不为本分叉背书。
