# Code signing policy

Status: application preparation; SignPath Foundation signing is **not approved or active**. Existing downloads must not be described as SignPath-signed without verifying their signatures. This policy describes the intended controls once accepted.

## Project and licensing

- Project: [FlowAcross](https://github.com/Hearion/FlowAcross), a desktop keyboard, mouse and clipboard sharing application.
- License for project-owned work: [GPL-3.0-only](../LICENSE). Third-party work retains its original license.
- No commercial dual licensing is offered for project-owned work.
- Downloads: [FlowAcross download page](downloads.md) and [release artifacts](https://github.com/Hearion/FlowAcross-releases/releases).
- Privacy policy: [PRIVACY.md](../PRIVACY.md).
- Contact: [Hearion](https://github.com/Hearion), xiaoyan00110@outlook.com.

## Team roles

| Role | Member | Responsibility |
|---|---|---|
| Author / committer | [Hearion](https://github.com/Hearion) | Maintain source code and build definitions |
| Reviewer | [Hearion](https://github.com/Hearion) | Review external contributions, especially dependencies and CI changes |
| Signing approver | [Hearion](https://github.com/Hearion) | Check the release commit, build provenance, artifact contents and version before approving signing |

This is currently a single-maintainer project. These roles do not claim an independent second reviewer. All people with repository write access or SignPath access must enable multi-factor authentication. Account enrollment and MFA must be confirmed before activation; no completed account configuration is asserted here.

## Signing and release controls

Only artifacts built from the project's public source and checked-in build scripts may be submitted. Windows signing builds must use GitHub-hosted runners and upload workflow artifacts so SignPath can verify their origin. Every production release requires explicit manual signing approval. External contributions require review before inclusion. Production signing credentials must not be exposed to untrusted pull requests.

Sign only FlowAcross-owned binaries; do not apply the project's signing identity to third-party binaries. Constrain the artifact configuration to the intended files and enforce product name `FlowAcross` and a consistent release version. Validate the signed main executable and installer before publishing.

The installer must contain the signed main executable. Signing only the installer does not sign its contents. If the chosen NSIS integration cannot sign nested files, sign the main executable first, bundle it without rebuilding or patching the executable, then sign the final installer. With a compatible Tauri CLI, use `tauri bundle --no-binary-patching` to preserve the existing executable signature; verify this behavior against the CLI version used by CI. Generate the Tauri updater signature **after** Authenticode signing has finished. Do not modify the resulting installation artifact after either final signature.

The source commit/tag, license, privacy policy and this policy must be linked from each download/release page. The corresponding source and build scripts must remain available alongside each binary release.

## Service attribution after approval

Once the Foundation has accepted the project and production signing is operational, publish this attribution on the project home and download pages:

> Free code signing provided by [SignPath.io](https://signpath.io), certificate by [SignPath Foundation](https://signpath.org).

The certificate subject belongs to SignPath Foundation. Acceptance does not transfer its private key to the maintainer. The attribution above is conditional and is not a claim that this project has already been accepted.

## References

- [Foundation conditions](https://signpath.org/terms)
- [GitHub build integration](https://docs.signpath.io/trusted-build-systems/github)
- [Application preparation](signpath-application.md)

中文说明：当前仅准备申请，尚未取得 SignPath 签名。Hearion 负责代码维护、外部贡献审查和发版签名批准；每次生产签名需人工批准。隐私行为、数据清理和卸载方式见对应文档。
