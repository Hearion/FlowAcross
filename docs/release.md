# 发布新版本与自动更新

推送 `v*` 标签后，GitHub Actions（`.github/workflows/release.yml`）会自动：

1. 校验标签和各处版本号一致，在公开仓库 **Hearion/FlowAcross-releases** 创建草稿发布；
2. 同时打包 Windows（NSIS 安装包）和 macOS（通用版 .dmg + 更新用的 `.app.tar.gz`），用更新私钥签名后上传；
3. 检查 `latest.json` 同时包含 Windows 和 macOS，再正式发布。

已安装的流界启动 15 秒后、之后每 6 小时读取一次
`https://github.com/Hearion/FlowAcross-releases/releases/latest/download/latest.json`，
发现新版本就在主界面提示。用户点「立即更新」后：下载并校验签名 → 释放远端按键、恢复本机输入、断开连接 → 安装 → 自动重新打开。

源码仓库是私有的，App 不能从私有仓库下载（需要令牌，而令牌不能打包进 App），所以安装包单独放在公开的发布仓库，那里只有安装包，没有源码。

## 一次性配置

### 1. 创建公开的发布仓库

在 GitHub 新建仓库 `FlowAcross-releases`，选 **Public**，勾选 **Add a README file**（空仓库没有提交，无法创建发布）。

### 2. 创建写入发布仓库的令牌

源码仓库 Actions 自带的令牌只能写本仓库，写另一个仓库需要单独的令牌：

1. GitHub 头像 → Settings → Developer settings → Personal access tokens → **Fine-grained tokens** → Generate new token；
2. Repository access 选 **Only select repositories** → `FlowAcross-releases`；
3. Permissions → Repository permissions → **Contents: Read and write**；
4. 有效期按需要设置。过期后发版会在第一步报错，重新生成并更新密钥即可。

### 3. 在源码仓库添加密钥

FlowAcross 仓库 → Settings → Secrets and variables → Actions → New repository secret：

| 名称 | 内容 | 必需 |
|---|---|---|
| `RELEASES_TOKEN` | 上一步生成的令牌 | 是 |
| `TAURI_SIGNING_PRIVATE_KEY` | 更新签名私钥文件的**全部内容**（见下文） | 是 |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | 私钥密码。当前私钥没有密码，可以不加 | 否 |
| `APPLE_CERTIFICATE` | macOS 签名证书（.p12）的 base64 | 建议 |
| `APPLE_CERTIFICATE_PASSWORD` | 导出 .p12 时设置的密码 | 配了证书就必需 |
| `APPLE_SIGNING_IDENTITY` | 证书名称，默认 `FlowAcross Dev` | 否 |

**更新签名私钥**：在 Windows 开发机上生成，位于 `C:\Users\Administrator\.tauri\flowacross-updater.key`，对应的公钥已写进 `app/src-tauri/tauri.conf.json`。用记事本打开私钥文件，复制全部内容粘贴到密钥里。

> **一定要备份这个私钥**（比如存进密码管理器）。丢了它，已安装的流界就再也无法验证新版本，只能让所有用户手动下载安装。私钥不要提交到仓库。

### 4. macOS 签名证书（建议配置）

macOS 按签名记住「辅助功能」「输入监控」授权。不配证书时，CI 用 ad-hoc 签名，**用户每次更新后都要重新授权**。配置方法：用 `scripts/setup-mac-signing.sh` 在本机创建的「FlowAcross Dev」证书，导出给 CI 使用：

1. 打开「钥匙串访问」→ 登录 → 我的证书，右键「FlowAcross Dev」→ 导出，格式选 .p12，设置一个密码；
2. 终端里运行 `base64 -i FlowAcross-Dev.p12 | pbcopy`，把剪贴板内容粘贴到 `APPLE_CERTIFICATE`；
3. 把导出密码填到 `APPLE_CERTIFICATE_PASSWORD`；
4. 导出的 .p12 用完就删掉。

用同一张证书签名后，用户授权一次，之后自动更新都不用重新授权。注意：这是自签名证书，用户**第一次**打开时 macOS 仍会提示「无法验证开发者」，需要在「系统设置 → 隐私与安全性」里点「仍要打开」。要彻底去掉这个提示，需要 Apple Developer ID（每年 99 美元）签名并公证。

Windows 安装包没有代码签名，第一次安装时 SmartScreen 会拦一下，点「更多信息 → 仍要运行」。之后的自动更新不受影响。

## 每次发版

```bash
node scripts/bump-version.mjs 0.2.0
git commit -am "发布 v0.2.0"
git tag -a v0.2.0 -m "修复断网后偶尔卡键；布局图支持缩放"
git push origin main v0.2.0
```

- 标签说明（`-m` 的内容）会作为更新说明，显示在发布页和 App 的更新提示里。可以写多行。
- 打开源码仓库的 Actions 页面看进度，一次大约 15–25 分钟。完成后在 https://github.com/Hearion/FlowAcross-releases/releases 能看到新版本。
- 私有仓库每月有 2000 分钟免费 Actions 时长，macOS 按 10 倍、Windows 按 2 倍计费，一次发版大约用掉 200–300 分钟。

## 常见问题

- **「标签和版本号不一致」**：忘了运行 `bump-version.mjs`。删掉标签（`git tag -d v0.2.0 && git push origin :refs/tags/v0.2.0`），改好版本号提交后重新打标签。
- **「latest.json 缺少 …」**：两个打包任务同时写 `latest.json` 时偶尔会互相覆盖。在 Actions 页面点 **Re-run all jobs**，会复用同一个草稿。
- **「已经发布过 vX」**：同一个版本号不能发两次，改用新的版本号。
- **只想要一个 Windows 测试包**：Actions 页面手动运行「Windows 测试包」，安装包在运行结果的附件里，不会触发自动更新。
- **想撤回一个有问题的版本**：在发布仓库把它删掉或改回草稿，`latest/download` 会指向上一个正式版本；已经更新的用户需要等下一个版本。
