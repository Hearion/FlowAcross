#!/bin/sh
# 打包源码（不含依赖、构建产物和 git 历史），用于拷到 Windows 电脑上调试。
# 用法：scripts/pack-source.sh [输出路径]，默认输出到 share/flowacross-src.zip
set -e
cd "$(dirname "$0")/.."
out="${1:-share/flowacross-src.zip}"
mkdir -p "$(dirname "$out")"
rm -f "$out"
zip -qr "$out" . \
  -x 'target/*' 'share/*' '.git/*' 'app/node_modules/*' 'node_modules/*' \
     'app/dist/*' 'app/src-tauri/gen/*' '*.DS_Store' '*.tsbuildinfo'
echo "已生成：$out（$(du -h "$out" | cut -f1)）"
