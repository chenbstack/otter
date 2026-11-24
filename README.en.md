# Otter

[中文](README.md) · English

A native SSH client: terminal and files (SFTP) in one window. Built in Rust on [GPUI](https://www.gpui.rs) and [russh](https://github.com/Eugeny/russh).

## Screenshots

![Nodes](assets/screenshots/nodes.png)

![Files](assets/screenshots/files.png)

![Agent takeover approval](assets/screenshots/takeover.png)

## Highlights

- **Native and small**: a Rust app drawing straight on the GPU, with no Electron or browser engine bundled. The macOS download is about 16 MB.
- **Fast**: launches in about 0.25 s (MacBook Pro, Apple M5 Pro), and long scrolling lists stay smooth.
- **Safe by default**: passwords and key passphrases are encrypted on disk, with the master key in the system keychain. An agent can never take over a terminal without your approval.

## Features

- **Nodes & groups**: organize servers, search, import from CSV or `~/.ssh/config`.
- **SSH terminal**: password, private key or SSH agent auth.
- **Files (SFTP)**: browse, preview, drag-and-drop upload, create folders, delete; also inside containers.
- **Docker** (in development): stacks, containers, images and volumes; start, stop, restart, delete.
- **Auto update**: checks GitHub Releases and updates in place from the header.
- **Agent interface**: let an AI agent read your nodes and, with your approval, take over a terminal session.

## Install

Download from the [Releases](../../releases) page:

- **macOS**: `.dmg`. The app is not notarized, so macOS may say it "is damaged and can't be opened. You should move it to the Trash". It is not damaged: after dragging it to Applications, run `xattr -cr /Applications/Otter.app` once, then open it.
- **Windows**: `-setup.exe`. Installs per user, no admin rights needed.
- **Linux**: `.tar.gz` containing `otter-gui` and `otter`.

## Usage

Open Otter, add a node (or import), and click it to open its terminal, files and Docker tabs.

### Agents

Reading nodes and sessions needs no approval. Taking over a terminal always asks you first, shows an overlay while the agent types, and can be stopped with one click. Granted agents are listed on the **Trust** page, where you can revoke them.

Command line:

```bash
otter nodes                          # server nodes (JSON)
otter sessions                       # open terminal sessions
otter takeover --reason "check disk" --agent my-agent
otter run "df -h"                    # prints only the new output
otter release                        # hand the session back
otter --help                         # all commands
```

MCP (stdio), e.g. Claude Code:

```bash
claude mcp add otter -- otter mcp
```

## License

MIT
