#!/usr/bin/env bash
# 在本机登录钥匙串里创建一个自签名的代码签名证书「FlowAcross Dev」（只需运行一次）。
#
# 为什么需要：没有签名（ad-hoc）的 app，macOS 按程序指纹记录「辅助功能」「输入监控」授权，
# 每次重新打包指纹都会变，授权就失效了。用固定证书签名后，系统按"证书 + 包名"识别流界，
# 授权一次以后重新打包也一直有效。
#
# 这个证书只在本机有效，不能用于发布给别人（发布需要 Apple Developer ID 签名和公证）。
set -euo pipefail

NAME="FlowAcross Dev"
KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"

if security find-identity -p codesigning "$KEYCHAIN" | grep -q "\"$NAME\""; then
  echo "证书「$NAME」已存在，无需重复创建。"
  exit 0
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

cat > "$WORK/cert.cnf" <<EOF
[ req ]
distinguished_name = dn
prompt = no
x509_extensions = ext
[ dn ]
CN = $NAME
[ ext ]
basicConstraints = critical,CA:false
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
EOF

openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
  -keyout "$WORK/key.pem" -out "$WORK/cert.pem" -config "$WORK/cert.cnf" 2>/dev/null

# p12 需要一个密码才能被 security 正确导入，用随机的临时密码。
PASS="$(openssl rand -hex 16)"
openssl pkcs12 -export -inkey "$WORK/key.pem" -in "$WORK/cert.pem" \
  -name "$NAME" -out "$WORK/cert.p12" -passout "pass:$PASS"

# -T 允许 codesign 使用这把私钥，签名时不再反复弹窗询问。
security import "$WORK/cert.p12" -k "$KEYCHAIN" -P "$PASS" -T /usr/bin/codesign >/dev/null

echo "已创建证书「$NAME」。"
security find-identity -p codesigning "$KEYCHAIN" | grep "$NAME" || true
