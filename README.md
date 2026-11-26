# Otter

中文 · [English](README.en.md)

原生 SSH 客户端:终端、文件(SFTP)和服务器管理(Docker、数据库、网站、应用商店)放在一个窗口里,服务器上不用装任何东西。基于 [GPUI](https://www.gpui.rs) 和 [russh](https://github.com/Eugeny/russh),Rust 编写。

## 截图

<table>
  <tr>
    <td width="50%" valign="top">
      <img src="assets/screenshots/stacks.png" alt="堆栈"><br>
      <b>堆栈</b><br>Compose 堆栈和容器的实时 CPU、内存、网络、磁盘;启动、停止、重启、删除,编辑 compose 文件并部署。
    </td>
    <td width="50%" valign="top">
      <img src="assets/screenshots/store.png" alt="应用商店"><br>
      <b>应用商店</b><br>一键安装 PostgreSQL、MySQL、Redis、OpenResty 等;已装应用静默检查可升级版本。
    </td>
  </tr>
  <tr>
    <td width="50%" valign="top">
      <img src="assets/screenshots/storage.png" alt="存储与空间透镜"><br>
      <b>存储与空间透镜</b><br>磁盘和挂载点用量;色块图直观看出哪个目录占空间,点击展开、双击放大。
    </td>
    <td width="50%" valign="top">
      <img src="assets/screenshots/files.png" alt="文件(SFTP)"><br>
      <b>文件(SFTP)</b><br>浏览、预览、拖入上传;可开双栏对照本地和服务器,容器内的文件也能看。
    </td>
  </tr>
  <tr>
    <td width="50%" valign="top">
      <img src="assets/screenshots/takeover.png" alt="Agent 接管确认"><br>
      <b>Agent 接管确认</b><br>AI agent 接管终端前必须先问你,可只允许一次、24 小时或总是允许,随时一键停止。
    </td>
    <td width="50%"></td>
  </tr>
</table>

## 特点

- **原生、体积小**:Rust 编写,直接用 GPU 渲染,没有打包 Electron 或浏览器内核。macOS 安装包约 16 MB。
- **启动快**:启动约 0.25 秒(MacBook Pro,Apple M5 Pro),长列表滚动也流畅。
- **服务器零安装**:所有操作走 SSH,服务器上不装 agent、守护进程和 Web 面板,也不对外开管理端口。
- **默认安全**:密码和私钥口令加密存储,主密钥放在系统钥匙串;agent 没有你的同意绝不能接管终端。

## 功能

- **节点与分组**:服务器分组、搜索,支持从 CSV 或 `~/.ssh/config` 导入。
- **SSH 终端**:密码、私钥、SSH Agent 认证。
- **文件(SFTP)**:浏览、预览、拖入上传、新建文件夹、删除;容器内的文件也能看。
- **Docker**:堆栈、容器、镜像、存储卷、镜像仓库;启动、停止、重启、删除,编辑 compose 文件并部署,删除堆栈。
- **应用商店**:一键安装 PostgreSQL、MySQL、MariaDB、Redis、OpenResty 等,统一的安装、升级、卸载流程;商店列表每天从仓库更新,已装应用静默检查可升级版本。
- **数据库**:管理数据库和用户,查看和修改常用参数、配置文件,连接信息一键复制。
- **网站**:基于 OpenResty 管理站点(反向代理、静态站、跳转)、HTTPS 证书(签发与续期)、性能和安全设置。
- **服务器运维**:进程、服务、计划任务、存储(空间透镜)。
- **自动更新**:通过 GitHub Release 检查新版本,在标题栏一键更新。
- **Agent 接口**:让 AI agent 读取节点信息,并在你同意后接管终端会话。

## 安装

在 [Releases](../../releases) 页面下载:

- **macOS**:`.dmg`。应用没有公证,macOS 可能提示"已损坏,无法打开,您应该将它移到废纸篓"。其实没有损坏:拖进「应用程序」后执行一次 `xattr -cr /Applications/Otter.app`,再打开即可。
- **Windows**:`-setup.exe`。装到当前用户目录,不需要管理员权限。
- **Linux**:`.tar.gz`,内含 `otter-gui` 和 `otter`。

## 使用

打开 Otter,添加节点(或导入),点节点卡片即可打开它的终端、文件(SFTP)和服务器管理(容器、数据库、网站、应用商店等)。

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

## 许可证

MIT
