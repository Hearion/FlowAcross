#!/usr/bin/env node
// 把一次发布的安装包和更新文件从 GitHub 的发布仓库复制到阿里云 OSS，国内下载和应用内更新走 OSS。
//
// OSS 里的布局（都在 OSS_PREFIX 目录下，如 FlowAcross/；DOWNLOAD_BASE_URL 指向存储桶绑定的域名）：
//   <目录>/releases/v0.2.3/<原文件名>     每个版本一份，内容不再改动，可以长期缓存
//   <目录>/latest.json                    应用内更新读的清单（只有正式版才更新），里面的下载地址指向上面的版本目录
//   <目录>/latest/FlowAcross_<平台>.<扩展名>  最新正式版的安装包，文件名不带版本号，给官网的下载按钮用；
//                                  下载时浏览器保存的文件名仍带版本号（Content-Disposition）
//
// 顺序：先传版本目录，再传 latest/，最后才换 latest.json。中途失败时已安装的流界看到的仍是旧清单，
// 不会拿到指向还没传完的文件的地址。
//
// 用法（在 GitHub Actions 里运行，见 .github/workflows/release.yml 和 mirror-oss.yml）：
//   node scripts/mirror-to-oss.mjs            真的上传
//   node scripts/mirror-to-oss.mjs --dry-run  只下载并列出要上传的内容，不连 OSS
//
// 环境变量：
//   GITHUB_TOKEN            读发布仓库（草稿发布需要有写权限的令牌）
//   RELEASE_REPO            发布仓库，如 Hearion/FlowAcross-releases
//   RELEASE_ID 或 TAG       哪一次发布：草稿用 RELEASE_ID；已发布的可以用 TAG（如 v0.2.3）
//   PRERELEASE              "true" 时只传版本目录，不动 latest.json 和 latest/
//   OSS_REGION              存储桶地域，如 oss-cn-hangzhou
//   OSS_BUCKET              存储桶名称
//   OSS_ACCESS_KEY_ID / OSS_ACCESS_KEY_SECRET  只能写这个存储桶的 RAM 子账号
//   DOWNLOAD_BASE_URL       存储桶绑定的 HTTPS 域名，如 https://dl.bapihou.com
//   OSS_PREFIX              存储桶里放流界文件的目录，如 FlowAcross（同一个存储桶还放别的软件）
//   OSS_SDK_DIR             装了 ali-oss 的目录（npm install --prefix 的目录）

import fs from "node:fs";
import path from "node:path";
import os from "node:os";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const dryRun = process.argv.includes("--dry-run");
const env = (name, required = true) => {
  const v = (process.env[name] ?? "").trim();
  if (!v && required) fail(`缺少环境变量 ${name}。`);
  return v;
};

function fail(msg) {
  console.log(`::error::${msg}`);
  process.exit(1);
}

/** 给官网用的固定文件名：去掉文件名里的版本号。只处理面向用户的安装包。 */
export function stableName(name, version) {
  const installers = [/_x64-setup\.exe$/, /_universal\.dmg$/, /_amd64\.deb$/, /_amd64\.AppImage$/, /_x86_64\.pkg\.tar\.zst$/];
  if (!installers.some((re) => re.test(name))) return null;
  const tag = `_${version}_`;
  return name.includes(tag) ? name.replace(tag, "_") : null;
}

/** 存储桶里的目录名：去掉首尾的 `/`，只允许字母、数字、`-`、`_`、`.` 和中间的 `/`。 */
export function cleanPrefix(prefix) {
  const p = prefix.replace(/^\/+|\/+$/g, "");
  if (!p || !/^[A-Za-z0-9._-]+(\/[A-Za-z0-9._-]+)*$/.test(p) || p.split("/").some((s) => s === "." || s === "..")) {
    throw new Error(`OSS_PREFIX「${prefix}」不是有效的目录名`);
  }
  return p;
}

/** 把 latest.json 里的下载地址改到 OSS 的版本目录（`base` 已经带上目录）。地址指向的文件必须在这次上传的文件里。 */
export function rewriteManifest(manifest, base, tag, names) {
  const out = structuredClone(manifest);
  for (const [platform, entry] of Object.entries(out.platforms ?? {})) {
    const file = decodeURIComponent(new URL(entry.url).pathname.split("/").pop());
    if (!names.includes(file)) {
      throw new Error(`latest.json 里 ${platform} 的文件 ${file} 不在这次发布里`);
    }
    entry.url = `${base}/releases/${tag}/${encodeURIComponent(file)}`;
  }
  return out;
}

const CONTENT_TYPES = {
  ".exe": "application/vnd.microsoft.portable-executable",
  ".dmg": "application/x-apple-diskimage",
  ".deb": "application/vnd.debian.binary-package",
  ".AppImage": "application/octet-stream",
  ".zst": "application/zstd",
  ".gz": "application/gzip",
  ".sig": "text/plain; charset=utf-8",
  ".json": "application/json; charset=utf-8",
};
const contentType = (name) => CONTENT_TYPES[path.extname(name)] ?? "application/octet-stream";

async function github(url, token, accept = "application/vnd.github+json") {
  const headers = { accept, "x-github-api-version": "2022-11-28" };
  // 本机 --dry-run 读公开的发布时可以不给令牌。
  if (token) headers.authorization = `Bearer ${token}`;
  const res = await fetch(url, {
    headers,
    redirect: "follow",
  });
  if (!res.ok) throw new Error(`GitHub 返回 ${res.status}：${url}`);
  return res;
}

async function main() {
  const token = env("GITHUB_TOKEN", !dryRun);
  const repo = env("RELEASE_REPO");
  const releaseId = env("RELEASE_ID", false);
  const tagInput = env("TAG", false);
  if (!releaseId && !tagInput) fail("RELEASE_ID 和 TAG 至少要给一个。");
  const prerelease = env("PRERELEASE", false) === "true";
  const base = dryRun ? (env("DOWNLOAD_BASE_URL", false) || "https://dl.example.com") : env("DOWNLOAD_BASE_URL");
  if (!/^https:\/\/[^/]+$/.test(base.replace(/\/$/, ""))) {
    fail(`DOWNLOAD_BASE_URL 要写成 https://域名 的形式（不带路径），现在是「${base}」。应用内更新只接受 HTTPS。`);
  }
  let prefix;
  try {
    prefix = cleanPrefix(env("OSS_PREFIX"));
  } catch (e) {
    fail(e.message);
  }
  // 公开的下载地址和存储桶里的路径都带上这个目录。
  const baseUrl = `${base.replace(/\/$/, "")}/${prefix}`;

  const api = `https://api.github.com/repos/${repo}`;
  const release = await (await github(releaseId ? `${api}/releases/${releaseId}` : `${api}/releases/tags/${tagInput}`, token)).json();
  const tag = release.tag_name;
  const version = tag.replace(/^v/, "");
  // 发布信息里附带的文件列表只有第一页（30 个），单独分页取全。
  release.assets = [];
  for (let page = 1; ; page++) {
    const batch = await (await github(`${api}/releases/${release.id}/assets?per_page=100&page=${page}`, token)).json();
    release.assets.push(...batch);
    if (batch.length < 100) break;
  }
  console.log(`发布 ${tag}（${release.draft ? "草稿" : "已发布"}${prerelease ? "，测试版" : ""}），${release.assets.length} 个文件`);
  if (release.prerelease && !prerelease) {
    fail(`${tag} 在发布仓库里是测试版，补传时要勾选「这是测试版」，否则会把应用内更新指向测试版。`);
  }
  // 手动补传一个旧的正式版时，不能把 latest.json 换回旧版本。
  if (!releaseId && !prerelease) {
    const latest = await (await github(`${api}/releases/latest`, token)).json();
    if (latest.tag_name !== tag) {
      fail(`最新的正式版是 ${latest.tag_name}，不是 ${tag}。补传旧的正式版会让 latest.json 退回旧版本；只想要它的安装包的话，勾选「这是测试版」（只传版本目录）。`);
    }
  }

  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "flow-oss-"));
  try {
    await mirror({ token, release, tag, version, prerelease, baseUrl, prefix, dir });
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
}

async function mirror({ token, release, tag, version, prerelease, baseUrl, prefix, dir }) {
  const files = [];
  for (const asset of release.assets) {
    const res = await github(asset.url, token, "application/octet-stream");
    const buf = Buffer.from(await res.arrayBuffer());
    if (buf.length !== asset.size) throw new Error(`${asset.name} 下载不完整（${buf.length} / ${asset.size} 字节）`);
    const file = path.join(dir, asset.name);
    fs.writeFileSync(file, buf);
    files.push({ name: asset.name, file, size: buf.length });
    console.log(`  已下载 ${asset.name}（${(buf.length / 1048576).toFixed(1)} MB）`);
  }
  const names = files.map((f) => f.name);

  let manifest = null;
  const manifestFile = files.find((f) => f.name === "latest.json");
  if (!prerelease) {
    if (!manifestFile) fail("这次发布里没有 latest.json，无法更新应用内更新的清单。");
    manifest = rewriteManifest(JSON.parse(fs.readFileSync(manifestFile.file, "utf8")), baseUrl, tag, names);
    if (manifest.version !== version) fail(`latest.json 的版本 ${manifest.version} 和标签 ${tag} 不一致。`);
  }

  // 要做的上传，按顺序执行。
  const plan = [];
  for (const f of files) {
    if (f.name === "latest.json") continue;
    plan.push({
      key: `${prefix}/releases/${tag}/${f.name}`,
      file: f.file,
      headers: { "Cache-Control": "public, max-age=31536000, immutable", "Content-Type": contentType(f.name) },
    });
  }
  if (!prerelease) {
    for (const f of files) {
      const stable = stableName(f.name, version);
      if (!stable) continue;
      plan.push({
        key: `${prefix}/latest/${stable}`,
        file: f.file,
        headers: {
          "Cache-Control": "no-cache",
          "Content-Type": contentType(f.name),
          "Content-Disposition": `attachment; filename="${f.name}"`,
        },
      });
    }
    const out = path.join(dir, "latest.oss.json");
    fs.writeFileSync(out, JSON.stringify(manifest, null, 2) + "\n");
    plan.push({
      key: `${prefix}/latest.json`,
      file: out,
      headers: { "Cache-Control": "no-cache", "Content-Type": contentType("latest.json") },
    });
  }

  if (dryRun) {
    console.log("\n--dry-run：不上传，计划如下：");
    for (const p of plan) console.log(`  ${p.key}  ${JSON.stringify(p.headers)}`);
    if (manifest) console.log("\n改写后的 latest.json：\n" + JSON.stringify(manifest, null, 2));
    return;
  }

  const require = createRequire(path.join(env("OSS_SDK_DIR"), "node_modules", "noop.js"));
  const OSS = require("ali-oss");
  const client = new OSS({
    region: env("OSS_REGION"),
    bucket: env("OSS_BUCKET"),
    accessKeyId: env("OSS_ACCESS_KEY_ID"),
    accessKeySecret: env("OSS_ACCESS_KEY_SECRET"),
    secure: true,
    timeout: 10 * 60 * 1000,
  });

  for (const p of plan) {
    const size = fs.statSync(p.file).size;
    for (let attempt = 1; ; attempt++) {
      try {
        if (size > 50 * 1048576) {
          await client.multipartUpload(p.key, p.file, { headers: p.headers, partSize: 8 * 1048576, parallel: 4 });
        } else {
          await client.put(p.key, p.file, { headers: p.headers });
        }
        break;
      } catch (e) {
        if (attempt >= 3) {
          const hint =
            e.code === "AccessDenied" || e.status === 403
              ? "RAM 子账号没有这个存储桶的写权限，或者 AccessKey 填错了。"
              : e.code === "NoSuchBucket"
                ? "存储桶名称或地域填错了。"
                : "检查网络和 OSS 配置。";
          fail(`上传 ${p.key} 失败（${e.code ?? e.message}）。${hint}`);
        }
        console.log(`  上传 ${p.key} 失败，第 ${attempt} 次重试：${e.code ?? e.message}`);
      }
    }
    console.log(`  已上传 ${p.key}`);
  }

  // 从外网按用户的方式取一遍，确认域名、证书和公共读都配好了。
  const check = prerelease ? `${baseUrl}/releases/${tag}/${encodeURIComponent(files[0].name)}` : `${baseUrl}/latest.json`;
  const res = await fetch(check, { method: prerelease ? "HEAD" : "GET", headers: { "cache-control": "no-cache" } });
  if (!res.ok) {
    fail(`文件已经传到 OSS，但 ${check} 返回 ${res.status}。检查存储桶是否为公共读、域名是否已绑定并配好 HTTPS 证书。`);
  }
  if (!prerelease) {
    const online = await res.json();
    if (online.version !== version) {
      fail(`${check} 上的版本是 ${online.version}，不是 ${version}。如果域名前面有 CDN，刷新它的缓存。`);
    }
  }
  console.log(`\n完成：${check} 可以访问。`);
}

// 用 fileURLToPath：路径里有空格或中文时，URL 的 pathname 是转义过的，和命令行给的路径对不上，脚本会什么都不做就退出。
if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch((e) => fail(e.message));
}
