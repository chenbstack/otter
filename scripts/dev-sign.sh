#!/bin/sh
# 开发期:用固定证书给 release 二进制重新签名,让 macOS 钥匙串认得它,
# 点一次「始终允许」后,以后每次重新构建都不会再弹密码框。
# (cargo 每次链接出来的是 ad-hoc 签名,哈希每次都变,钥匙串就把它当新程序。)
# 用法:cargo build --release && scripts/dev-sign.sh [证书名]
#   证书名默认取本机第一个 Apple Development 证书,也可用环境变量 DOCKYARD_SIGN_ID 指定。
set -e
BIN="$(dirname "$0")/../target/release/dockyard"
ID="${1:-${DOCKYARD_SIGN_ID:-$(security find-identity -v -p codesigning | sed -n 's/.*"\(Apple Development:[^"]*\)".*/\1/p' | head -1)}}"
[ -n "$ID" ] || { echo "没找到代码签名证书(需要 Xcode 里登录的 Apple Development 证书)" >&2; exit 1; }
codesign -f -s "$ID" -i dev.dockyard.app "$BIN"
echo "已签名: $ID"
