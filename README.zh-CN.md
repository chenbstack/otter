<div align="center">

<img src="assets/logo/otter-1024.png" width="112" alt="Otter logo">

# Otter

**只靠 SSH 管理服务器:终端和文件放在一个原生窗口里,服务器上什么都不用装。**

Rust · [GPUI](https://www.gpui.rs) · [russh](https://github.com/Eugeny/russh) · tokio

[English](README.md) · 简体中文

</div>

## 功能

- **节点与分组**:服务器分组、搜索,支持从 CSV 或 `~/.ssh/config` 导入。
- **SSH 终端**:密码、私钥、SSH Agent 认证。
- **文件(SFTP)**:浏览、预览、拖入上传、新建文件夹、删除;容器内的文件也能看。
- **Docker**(开发中):堆栈、容器、镜像、存储卷;启动、停止、重启、删除。
- **Agent 接口**:让 AI agent 读取节点信息,并在你同意后接管终端会话。

## 安装

在 [Releases](../../releases) 页面下载:

- **macOS**:`.dmg`。未公证,首次打开请右键 → 打开。
- **Windows**:`-setup.exe`。装到当前用户目录,不需要管理员权限。
- **Linux**:`.tar.gz`,内含 `otter-gui` 和 `otter`。

## 使用

打开 Otter,添加节点(或导入),点节点卡片即可打开它的终端、文件和 Docker 标签。

### Agent

读取节点和会话不需要确认。接管终端每次都会先问你,接管期间界面有遮罩提示,随时可以一键停止。已授权的 agent 在「信任」页里,可以撤销。

命令行:

```bash
otter nodes                          # 服务器节点(JSON)
otter sessions                       # 已打开的终端会话
otter takeover --reason "查看磁盘" --agent my-agent
otter run "df -h"                    # 只返回这条命令的新输出
otter release                        # 把会话还给用户
otter --help                         # 全部命令
```

MCP(stdio),例如 Claude Code:

```bash
claude mcp add otter -- otter mcp
```
