# 发布新版本与自动更新

推送 `v*` 标签后，GitHub Actions（`.github/workflows/release.yml`）会自动：

1. 校验标签和各处版本号一致，在公开仓库 **Hearion/FlowAcross-releases** 创建草稿发布；
2. 同时打包 Windows（NSIS 安装包）、macOS（通用版 .dmg + 更新用的 `.app.tar.gz`）和 Linux（x86_64 的 `.deb`、`.AppImage`，以及给 Arch 系用的 `.pkg.tar.zst`），Windows 和 macOS 的更新包用更新私钥签名后上传；
3. 检查 `latest.json` 同时包含 Windows 和 macOS、Linux 的三种安装包都在；
4. 把全部文件复制到阿里云 OSS（见下文「阿里云 OSS」），成功后再正式发布。

Linux 版从 v0.2.2 起随正式版一起发布，但没有接入应用内自动更新（`latest.json` 里没有 Linux），用户要回到发布页手动下载新版本。Arch 包由 `scripts/build-arch-pkg.sh` 在 Arch 的官方容器里把 `.deb` 重新打包而成（`scripts/arch/PKGBUILD`，包名 `flowacross-bin`），不重新编译；Arch 的版本号不能带连字符，`0.2.2-beta.3` 在包里写成 `0.2.2_beta.3`。

已安装的流界启动 15 秒后、之后每 6 小时检查一次：先读阿里云 OSS 上的 `latest.json`，读不到再读
`https://github.com/Hearion/FlowAcross-releases/releases/latest/download/latest.json`（0.2.3 及更早的版本只读 GitHub 的），
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

### 5. 阿里云 OSS（国内下载和应用内更新）

发版时会在正式发布之前把安装包和 `latest.json` 复制到阿里云 OSS（`scripts/mirror-to-oss.mjs`），**没配好的话发版会在第一步就报错**。

存储桶是 `bapihou-app-release`（里面还放别的软件），流界的文件都在 `FlowAcross/` 目录下。这个目录名写在两个工作流的 `OSS_PREFIX` 里（不是密钥），要改的话两处一起改。布局：

| 路径 | 内容 | 缓存 |
|---|---|---|
| `FlowAcross/releases/v0.2.3/` | 这个版本的全部文件，原文件名 | 一年（内容不再改动） |
| `FlowAcross/latest/FlowAcross_x64-setup.exe` 等 | 最新正式版的安装包，文件名不带版本号，给官网下载按钮用 | 不缓存 |
| `FlowAcross/latest.json` | 应用内更新的清单，下载地址指向 `FlowAcross/releases/<版本>/` | 不缓存 |

对外的地址是「下载域名 + 上面的路径」，例如 `https://下载域名/FlowAcross/latest.json`。

一次性配置：

1. **存储桶**：阿里云控制台 → 对象存储 OSS → 创建 Bucket。地域选离用户近的（如华东 1 杭州），存储类型「标准存储」，读写权限选**公共读**（安装包本来就是公开下载的；如果开着「阻止公共访问」要先关掉）。
2. **绑定下载域名**（如 `dl.bapihou.com`）：Bucket → Bucket 配置 → 域名管理 → 绑定域名，按提示在 DNS 里加 CNAME 记录；再在同一页「证书托管」里上传或选择这个域名的 HTTPS 证书（阿里云的免费证书即可）。应用内更新只接受 HTTPS。大陆地域的存储桶绑定自定义域名要求域名已备案。
3. **跨域（CORS）不需要**：官网只是链接到文件，不用脚本读取。
4. **RAM 子账号**：RAM 访问控制 → 用户 → 创建用户，勾选「OpenAPI 调用访问」，记下 AccessKey ID 和 Secret（只显示一次）。再给它授权一条只针对这个存储桶的自定义策略：

   ```json
   {
     "Version": "1",
     "Statement": [
       {
         "Effect": "Allow",
         "Action": ["oss:PutObject", "oss:GetObject", "oss:AbortMultipartUpload", "oss:ListParts"],
         "Resource": ["acs:oss:*:*:bapihou-app-release/FlowAcross/*"]
       }
     ]
   }
   ```

5. **源码仓库的密钥**（FlowAcross → Settings → Secrets and variables → Actions）：

   | 名称 | 内容 |
   |---|---|
   | `OSS_REGION` | 存储桶地域，如 `oss-cn-hangzhou`（Bucket 概览页「地域节点」的前半段） |
   | `OSS_BUCKET` | `bapihou-app-release` |
   | `OSS_ACCESS_KEY_ID` | RAM 子账号的 AccessKey ID |
   | `OSS_ACCESS_KEY_SECRET` | RAM 子账号的 AccessKey Secret |
   | `DOWNLOAD_BASE_URL` | 绑定的下载域名，写成 `https://dl.bapihou.com`（不带路径和结尾的 `/`） |

6. **补传当前版本**：Actions → 「补传到阿里云 OSS」→ Run workflow，填当前的正式版标签（如 `v0.2.3`）。运行完会从外网读一遍 `https://下载域名/FlowAcross/latest.json` 确认能访问。
7. **防止被刷流量**：OSS 按下载流量计费，建议在「费用 → 预算管理」设一个月度预算提醒。

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

## 发测试版（Pre-release）

版本号带后缀的标签就是测试版，例如 `v0.2.2-beta.1`、`v0.3.0-rc.1`：

```bash
node scripts/bump-version.mjs 0.2.2-beta.1
git commit -am "发布 v0.2.2-beta.1（测试版）"
git tag -a v0.2.2-beta.1 -m "测试版：Linux（X11）支持"
git push origin main v0.2.2-beta.1
```

和正式版的区别：

- 在发布仓库里标成 **Pre-release**，不会成为 latest。`releases/latest/download/latest.json` 仍指向上一个正式版，**已安装的流界收不到更新提示**，测试的人要到发布页手动下载安装。
- 用快一些的构建配置（轻量 LTO、16 个代码生成单元），出包更快，安装包比正式版的配置大 1 MB 左右；正式版仍按 `Cargo.toml` 里的配置（全量 LTO）。这两个设置是打包任务里按标签写入的环境变量，不要改成"正式版时留空"：值为空的话 Cargo 不会忽略，会直接报错。
- 装了测试版的 Windows / macOS，之后发布同号或更高的正式版（如 0.2.2）时会正常收到更新提示（`0.2.2-beta.1` 低于 `0.2.2`）。
- 测试完直接发正式版即可（把版本号改成不带后缀的），不需要撤回测试版；想清理就在发布仓库把它删掉。

## 常见问题

- **「标签和版本号不一致」**：忘了运行 `bump-version.mjs`。删掉标签（`git tag -d v0.2.0 && git push origin :refs/tags/v0.2.0`），改好版本号提交后重新打标签。
- **「latest.json 缺少 …」**：两个打包任务同时写 `latest.json` 时偶尔会互相覆盖。在 Actions 页面点 **Re-run all jobs**，会复用同一个草稿。
- **「已经发布过 vX」**：同一个版本号不能发两次，改用新的版本号。
- **「复制到阿里云 OSS」失败**：草稿不会发布。按日志里的提示改好 OSS 配置后，在 Actions 页面重新运行失败的任务（会复用同一个草稿）。常见原因：`AccessDenied`（子账号没有这个存储桶的写权限、AccessKey 填错）、`NoSuchBucket`（存储桶名称或地域填错）、「返回 403 / 404」（存储桶不是公共读，或域名没绑定、证书没配）。
- **测试版**也会复制到 OSS 的 `releases/<标签>/`，但不改 `latest.json` 和 `latest/`。
- **「草稿里缺少 Linux 安装包」**：要等 Linux 包也打完才发布。看「打包 Linux」的日志，修好后重新运行失败的任务。
- **只想要一个 Windows 测试包**：Actions 页面手动运行「Windows 测试包」，安装包在运行结果的附件里，不会触发自动更新。
- **想撤回一个有问题的版本**：在发布仓库把它删掉或改回草稿，`latest/download` 会指向上一个正式版本；已经更新的用户需要等下一个版本。
