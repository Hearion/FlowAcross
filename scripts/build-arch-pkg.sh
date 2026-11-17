#!/usr/bin/env bash
# 用发版流程打出的 .deb 做一个 Arch 安装包（.pkg.tar.zst），给 Arch、CachyOS、Manjaro 等用 pacman -U 安装。
# 在 Arch 的官方容器里用 makepkg 构建，所以需要 docker；不重新编译，只是重新打包。
# 用法：scripts/build-arch-pkg.sh <.deb 路径> <版本号，如 0.2.2-beta.3> <输出目录>
set -euo pipefail

if [ $# -ne 3 ]; then
  echo "用法：$0 <.deb 路径> <版本号> <输出目录>" >&2
  exit 1
fi
deb="$1"
version="${2#v}"
out="$3"
[ -f "$deb" ] || { echo "找不到 $deb" >&2; exit 1; }

here="$(cd "$(dirname "$0")" && pwd)"
# Arch 的版本号里不能有连字符。
pkgver="${version//-/_}"

work="$(mktemp -d)"
# 清理失败不能让整个脚本失败（包已经打好并拷出去了）。
trap 'rm -rf "$work" 2>/dev/null || true' EXIT
cp "$deb" "$work/flowacross.deb"
sed "s/@PKGVER@/$pkgver/" "$here/arch/PKGBUILD" > "$work/PKGBUILD"

# makepkg 不肯以 root 运行：在容器里建一个普通用户来跑。依赖只写进包的元数据，构建时不需要装（--nodeps）。
# 构建时目录要归容器里的那个用户；结束后（不管成功失败）把属主还给外面运行本脚本的用户，
# 否则外面读不了也删不掉这些文件（Linux 上容器内外的用户编号是同一套）。
docker run --rm --platform linux/amd64 \
  -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
  -v "$work":/build archlinux:base-devel bash -euc '
  trap "chown -R \"\$HOST_UID:\$HOST_GID\" /build" EXIT
  useradd -m builder
  chown -R builder /build
  su builder -c "cd /build && makepkg --force --nodeps"
'

mkdir -p "$out"
built="$(ls "$work"/*.pkg.tar.zst | grep -v -- "-debug-" | head -1)"
[ -n "$built" ] || { echo "没有打出安装包" >&2; exit 1; }
target="$out/FlowAcross_${version}_x86_64.pkg.tar.zst"
cp "$built" "$target"
echo "$target"
