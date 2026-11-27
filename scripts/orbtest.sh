#!/bin/bash
# orbtest:在 OrbStack 虚拟机里真机测试服务器端脚本。只在要求时运行,不随构建和打包。
#
#   scripts/orbtest.sh                     跑全部用例(不含慢用例)
#   scripts/orbtest.sh firewall            只跑一个模块;也可以更细:firewall::rocky、firewall::ubuntu_switch
#   scripts/orbtest.sh --slow [模块]        连等回滚定时器的慢用例一起跑
#   scripts/orbtest.sh -v ...              打印每个用例的详细输出(默认只在失败时显示)
#   scripts/orbtest.sh init [发行版…]       建模板机(ubuntu rocky debian,不写就全建);已是最新的跳过
#   scripts/orbtest.sh rebuild [发行版…]    强制重建模板机
#   scripts/orbtest.sh status              列出模板机和残留的临时机器
#   scripts/orbtest.sh clean               删掉所有 otter-orbtest-* 机器(含模板机)
#
# 环境变量:OTTER_VM=<机器名> 直接用现成机器(不建不删,调试用);OTTER_VM_KEEP=1 用例失败时保留机器;
#          ORBTEST_THREADS=<n> 并行用例数(默认 4,每个用例一台克隆机)。
set -euo pipefail
cd "$(dirname "$0")/.."

if ! command -v orbctl >/dev/null 2>&1 || ! orbctl list >/dev/null 2>&1; then
  echo "需要 OrbStack 在运行(orbctl 不可用)" >&2
  exit 1
fi

export OTTER_ORBTEST=1
T="cargo test --release --features orbtest"
CMD="-- --ignored --nocapture --test-threads=1"

sub="${1:-run}"
case "$sub" in
  init|rebuild)
    shift
    OTTER_ORBTEST_DISTROS="$(IFS=,; echo "$*")" $T "orbtest::provision::cmd_$sub" $CMD
    exit $?
    ;;
  status|clean)
    $T -q "orbtest::provision::cmd_$sub" $CMD 2>/dev/null | grep -vE '^(running|test result|$)|^test orbtest'
    exit 0
    ;;
esac

verbose=""
filter=""
for a in "$@"; do
  case "$a" in
    --slow) export OTTER_ORBTEST_SLOW=1 ;;
    -v) verbose="--nocapture" ;;
    *) filter="$a" ;;
  esac
done

sweep() { $T orbtest::provision::cmd_sweep $CMD >/dev/null 2>&1 || true; }
trap sweep EXIT
sweep

target="orbtest::${filter}"
$T "$target" -- --ignored $verbose --skip provision::cmd_ --test-threads="${ORBTEST_THREADS:-4}"
