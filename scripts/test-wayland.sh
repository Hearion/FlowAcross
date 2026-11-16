#!/usr/bin/env bash
# 在无头的 sway 里跑 Wayland（wlroots 系）平台层的集成测试，不需要真实的显示器和键鼠。
# 需要：sway、wl-clipboard、grim。CI 的 Ubuntu 任务和本机容器都用它。
# 用法：scripts/test-wayland.sh            跑全部 Wayland 测试
#       scripts/test-wayland.sh <过滤词>    只跑名字里带这个词的
set -euo pipefail

filter="${1:-linux_wl}"

if [ "$(id -u)" = "0" ]; then
  echo "sway 不肯以 root 运行，请用普通用户执行本脚本。" >&2
  exit 1
fi

export XDG_RUNTIME_DIR
XDG_RUNTIME_DIR="$(mktemp -d)"
chmod 700 "$XDG_RUNTIME_DIR"
unset DISPLAY WAYLAND_DISPLAY

WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=pixman \
  sway -c /dev/null >"$XDG_RUNTIME_DIR/sway.log" 2>&1 &
sway_pid=$!
trap 'kill "$sway_pid" 2>/dev/null || true; rm -rf "$XDG_RUNTIME_DIR"' EXIT

for _ in $(seq 1 100); do
  [ -S "$XDG_RUNTIME_DIR/wayland-1" ] && break
  sleep 0.1
done
if [ ! -S "$XDG_RUNTIME_DIR/wayland-1" ]; then
  echo "sway 没有启动成功：" >&2
  cat "$XDG_RUNTIME_DIR/sway.log" >&2
  exit 1
fi
export WAYLAND_DISPLAY=wayland-1

# 这些测试共用一个光标、键盘和剪贴板，必须串行。
cargo test -p flow-platform -- --ignored --test-threads=1 "$filter"
