#!/bin/sh
# 打包源码（不含依赖、构建产物和 git 历史），用于拷到 Windows 电脑上调试。
# 用法：scripts/pack-source.sh [输出路径]，默认输出到 share/flowacross-src.zip
#
# 只打包 git 跟踪的文件（含尚未提交的修改），工作区里未跟踪的文件（令牌、密钥、临时文件）
# 一律不打包。新建的源文件请先 git add。
set -e
cd "$(dirname "$0")/.."
out="${1:-share/flowacross-src.zip}"
mkdir -p "$(dirname "$out")"
rm -f "$out"
untracked=$(git ls-files --others --exclude-standard | wc -l | tr -d ' ')
# 已跟踪但在工作区里删掉的文件跳过。
git ls-files | while IFS= read -r f; do
  [ -e "$f" ] && printf '%s\n' "$f"
done | zip -q "$out" -@
echo "已生成：$out（$(du -h "$out" | cut -f1)）"
if [ "$untracked" -gt 0 ]; then
  echo "提示：有 $untracked 个未跟踪的文件没有打包（git status 查看；源文件请先 git add）。"
fi
