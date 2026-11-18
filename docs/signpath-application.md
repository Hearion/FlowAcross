# SignPath Foundation 申请资料

更新日期：2026-11-19。状态：**申请资料已整理，尚未向 SignPath 提交、获批或启用签名**。填写申请时，请确认公开链接已展示本次修改。

申请入口：<https://signpath.org/apply>。资格和约束以 [Foundation conditions](https://signpath.org/terms) 为准。以下字段名称及下拉选项已于 2026-11-19 在公开申请表中核对。

## 按当前申请表逐项填写

| 表单字段 | 建议填写 |
|---|---|
| Project Name * | `FlowAcross` |
| Repository URL * | `https://github.com/Hearion/FlowAcross` |
| Homepage URL * | `https://github.com/Hearion/FlowAcross` |
| Download URL | `https://github.com/Hearion/FlowAcross/blob/main/docs/downloads.md`，该页面包含实际下载入口和申请中的签名状态 |
| Privacy Policy URL | `https://github.com/Hearion/FlowAcross/blob/main/PRIVACY.md` |
| Wikipedia URL (optional) | 留空；不要填写无关词条 |
| Tagline * | `Share one keyboard, mouse and clipboard across your computers.` |
| Category * | `Productivity` |
| Description * | 复制下方 “Description 表单短文” |
| Reputation * | 复制下方 “Reputation 表单短文”；可补充真实、可核验的用户反馈及统计 |
| Maintainer Type | `Individual maintainer(s)` |
| Build System * | `GitHub Actions` |
| First Name * | 填写你用于创建 SignPath 账号的名；使用姓名拼音时，名填这里 |
| Last Name * | 填写你用于创建 SignPath 账号的姓；使用姓名拼音时，姓填这里 |
| Email * | `xiaoyan00110@outlook.com` |
| Company Name | 没有公司则留空，不要编造公司名 |
| Primary Discovery Channel * | `AI / LLM tools`（本次通过 Codex 了解到该服务） |
| Please specify the exact source (optional) | `OpenAI Codex` |

表单末尾的行为准则/证书归属和个人数据处理是必选同意项，请本人阅读后决定是否勾选。`I agree to receive other communications from SignPath.` 是可选营销通信同意，可以不勾选。验证码和最终提交由申请人操作；本次没有替你填写或提交在线表单。

### Description 表单短文

```text
FlowAcross is an open-source desktop application that lets users share one keyboard and mouse across their Windows, macOS and Linux computers, with clipboard text, image and file sharing. Users pair trusted computers on their local network, and shared contents travel over encrypted direct connections without a relay server or a FlowAcross account. We request code signing for Windows executables and installers built from the public repository. Project-owned code is licensed under GPL-3.0-only.
```

### Reputation 表单短文

```text
FlowAcross is an early-stage, actively maintained project with public source code, automated builds and published binary releases. Its source repository has recently been made public. We do not claim broad adoption or independent media coverage at this stage. Please review the public evidence below:

Source and development history: https://github.com/Hearion/FlowAcross
Release history and downloadable builds: https://github.com/Hearion/FlowAcross-releases/releases
Automated build history: https://github.com/Hearion/FlowAcross/actions
Code signing policy and team roles: https://github.com/Hearion/FlowAcross/blob/main/docs/code-signing-policy.md

We understand that acceptance depends on the Foundation's assessment of the project's verifiable reputation and are happy to provide further evidence as the project grows.
```

## 可直接填写的信息

| 项目 | 内容 |
|---|---|
| Project name | FlowAcross |
| Maintainer / GitHub account | Hearion — https://github.com/Hearion |
| Contact email | xiaoyan00110@outlook.com |
| Source repository | https://github.com/Hearion/FlowAcross |
| Project home / documentation | https://github.com/Hearion/FlowAcross#readme |
| Download page | https://github.com/Hearion/FlowAcross/blob/main/docs/downloads.md |
| Release artifacts | https://github.com/Hearion/FlowAcross-releases/releases |
| License | GNU General Public License v3.0 only (`GPL-3.0-only`) |
| License URL | https://github.com/Hearion/FlowAcross/blob/main/LICENSE |
| Code signing policy | https://github.com/Hearion/FlowAcross/blob/main/docs/code-signing-policy.md |
| Privacy policy | https://github.com/Hearion/FlowAcross/blob/main/PRIVACY.md |
| Uninstallation | https://github.com/Hearion/FlowAcross/blob/main/docs/uninstall.md |
| CI workflows | https://github.com/Hearion/FlowAcross/actions |
| Build definition | https://github.com/Hearion/FlowAcross/blob/main/.github/workflows/release.yml |
| Third-party inventory | https://github.com/Hearion/FlowAcross/blob/main/docs/third-party-dependencies.json |

如果表单要求 legal name / 真实姓名，请由申请人填写本人信息；`Hearion` 是维护者账号，不应冒充已核实的法定姓名。不要填报未经确认的用户数、Star 数或下载量。

## English project description

FlowAcross is a desktop application that lets users share one keyboard and mouse across their own computers. It also supports clipboard text, images and file transfer. The current source tree supports Windows, macOS and Linux, using a Rust workspace with a Tauri 2 and React desktop interface.

Devices discover each other on the local network using mDNS. Users explicitly pair trusted devices, after which keyboard, mouse and clipboard/file traffic uses encrypted direct connections. The maintainer does not operate a relay service. No FlowAcross account is required. The desktop application contains no advertising, usage analytics or automatic crash-reporting integration. Optional update checks on Windows/macOS contact a maintainer-controlled download domain backed by Alibaba Cloud OSS, with GitHub as a fallback, and can be disabled in settings. The privacy policy explains download requests, discovery metadata, local logs, received files and system permissions.

We request Windows Authenticode signing for the FlowAcross executable and NSIS installer built from this public repository. Project-owned work is licensed under GPL-3.0-only. Third-party components retain their original licenses and are inventoried with their available license notices.

On Windows, the UI uses Microsoft's WebView2 system runtime. A missing runtime may be installed through the standard Tauri installer bootstrapper. WebView2 is a separately licensed Microsoft dependency; we do not request that it be signed under the project's Foundation identity. Its privacy policy is linked from our application privacy documentation.

## English team and build description

The maintainer is Hearion (https://github.com/Hearion); the application contact is xiaoyan00110@outlook.com. This is a single-maintainer project. Hearion maintains the code, reviews external contributions and will manually approve production signing requests. The published policy requires MFA for repository writers and SignPath users; account settings must be confirmed before signing is activated.

The existing release workflow runs on GitHub-hosted Windows, macOS and Linux runners. Source code and checked-in build definitions are public, while installation artifacts are distributed from a separate public release repository and an Alibaba Cloud OSS download mirror. We plan to integrate SignPath's GitHub build-origin verification, retain manual production signing approval, restrict signing to FlowAcross-owned binaries, and enforce product/version metadata. SignPath integration and production credentials are not yet configured.

The project is actively developed and has published binary releases. Please evaluate its actual public commit history, releases and community feedback; we do not claim a particular adoption level or guarantee that it meets the Foundation's reputation threshold.

## 提交前清单

- [x] 加入完整 GPL-3.0 许可证，并将 Cargo、npm、Tauri 的自有项目声明统一为 `GPL-3.0-only`。
- [x] 编写隐私说明、卸载说明、维护者角色及签名政策，加入 README 入口。
- [x] 准备安装时的许可与隐私提示，以及安装包内的许可、版权及第三方声明资源。
- [x] 准备英文申请介绍与联系方式。
- [x] 建立带实际下载入口、隐私和签名政策的统一下载页 `docs/downloads.md`，可用于申请表 Download URL。
- [ ] 提交并推送本次修改；核对上述源码、许可证和政策链接可匿名访问。
- [ ] 维护者亲自确认 GitHub 已开启 MFA；获批后确认 SignPath MFA。当前没有读取账号设置。
- [ ] 在官网和发布仓库首页加入下方的 **Code signing policy** 入口；现有发布页也应加入入口。
- [ ] 对照真实下载版本填写具体 tag、源码提交、Actions run 和安装包链接；申请时不要使用与安装包不符的本地旧版本号。
- [x] 收集当前锁文件对应的 571 个依赖许可信息；收集器报告 `missing: []`。对部分缺少许可文件的 crate，保留了固定上游提交、父项目许可或标准许可文本来源。
- [ ] 在实际发布平台重新生成并审阅第三方声明，确认上游源码和必要许可材料可获取。当前清单是声明收集，不是完整法律审计。
- [ ] 确认有权按 GPL-3.0 发布项目原创代码和资源；核对所有外部代码/素材来源，不将第三方资源重新标为 GPL。
- [ ] 根据真实维护历史和用户反馈补充项目信誉材料；不虚构活跃用户或下载数据。
- [ ] 在申请表中填入所需本人信息并亲自提交。

## 发布仓库 / 官网可复制内容

```markdown
## License and source

FlowAcross project-owned work is licensed under GPL-3.0-only.
[License](https://github.com/Hearion/FlowAcross/blob/main/LICENSE) ·
[Source and build instructions](https://github.com/Hearion/FlowAcross) ·
[Privacy policy](https://github.com/Hearion/FlowAcross/blob/main/PRIVACY.md) ·
[Uninstallation](https://github.com/Hearion/FlowAcross/blob/main/docs/uninstall.md)

## Code signing policy

[Code signing policy and team roles](https://github.com/Hearion/FlowAcross/blob/main/docs/code-signing-policy.md).
SignPath Foundation signing is being prepared and is not yet approved or active.
Check each downloaded file's actual digital signature.
```

接受并启用后，按签名政策中的归属文字更新官网和下载页。不能在申请前声称已受资助或已签名。

## 获批后的接入安排

1. 在 SignPath 创建组织/项目，连接源码仓库和 GitHub 构建来源验证；按官方流程配置 GitHub App 及最小权限。
2. 在仓库 Secrets 保存 `SIGNPATH_API_TOKEN`，在配置中填写真实 organization ID、project slug 和 signing policy slug，不公开令牌。
3. 使用 GitHub 托管 Windows runner 构建并上传未签名 workflow artifact，再由官方 Action 提交签名申请；保留人工批准。
4. 分别覆盖主程序和安装包。NSIS 在当前 SignPath 文档中不是可自动深入签名的组合格式，不能假设签安装器会改变内部 EXE：先签主程序、再打包、最后签安装器，或采用经 SignPath 确认的等效方案。再次打包时不能重新编译或修补已签名的 EXE；兼容的 Tauri CLI 提供 `tauri bundle --no-binary-patching`，接入时需在实际 CI 版本上确认。
5. Authenticode 完成后再生成 Tauri 更新签名及 `latest.json`，验证最终安装包和安装后的 EXE，再发布。现有 Tauri 更新密钥不能代替 Windows 代码签名。
6. 保留版本对应的源码、构建脚本、锁文件、依赖声明及上游源码获取资料，按 GPL 要求提供对应源码。构建来源可核验不等于已经达到逐字节可重复构建。

当前没有生成虚假的 SignPath ID、令牌或“已启用”配置，也没有修改现有发布工作流去请求尚不存在的服务。

参考：[GitHub integration](https://docs.signpath.io/trusted-build-systems/github)、[artifact formats and metadata restrictions](https://docs.signpath.io/artifact-configuration/reference)。
