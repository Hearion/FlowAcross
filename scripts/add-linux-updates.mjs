#!/usr/bin/env node
// 把 Linux 的三种安装包写进这次发布的 latest.json，已安装的 Linux 版流界据此在应用内更新。
//
// 清单里各占一项（应用按自己的安装方式找，见 app/src-tauri/src/updater.rs 的 update_target）：
//   linux-x86_64-appimage   AppImage，直接替换文件
//   linux-x86_64-dpkg       deb，下载后交给 dpkg
//   linux-x86_64-pacman     Arch 安装包，下载后交给 pacman
//
// 不写 linux-x86_64 和 linux-x86_64-deb，已有的也删掉：0.2.3 及更早的 Linux 版不会分辨 Arch
// （Arch 的包是把 deb 里的文件原样重新打包的，程序以为自己是 deb 装的），清单里一旦有这两项，
// 装在 Arch 上的旧版本就会去下 deb、跑 dpkg，然后报安装失败。旧的 AppImage 只找
// linux-x86_64-appimage，能正常收到更新。
//
// 签名文件（<安装包>.sig）由「打包 Linux」任务用 tauri signer 生成并传到草稿里，这里只读取。
// Windows 和 macOS 的两个打包任务会同时改 latest.json，所以这一步放在它们都结束之后的发布任务里。
//
// 用法（在 GitHub Actions 里运行，见 .github/workflows/release.yml）：
//   node scripts/add-linux-updates.mjs            改好后传回草稿
//   node scripts/add-linux-updates.mjs --dry-run  只打印改好的清单
//
// 环境变量：
//   GITHUB_TOKEN   能写发布仓库的令牌
//   RELEASE_REPO   发布仓库，如 Hearion/FlowAcross-releases
//   RELEASE_ID     这次发布（草稿）的编号

import path from "node:path";
import { fileURLToPath } from "node:url";

/** 清单里的名字 → 安装包文件名的样子。 */
export const LINUX_TARGETS = [
  { target: "linux-x86_64-appimage", pattern: /_amd64\.AppImage$/, what: "AppImage" },
  { target: "linux-x86_64-dpkg", pattern: /_amd64\.deb$/, what: "deb" },
  { target: "linux-x86_64-pacman", pattern: /_x86_64\.pkg\.tar\.zst$/, what: "Arch 安装包" },
];

/** 旧版本会找、会装错的两项（见文件开头的说明）。 */
export const FORBIDDEN_TARGETS = ["linux-x86_64", "linux-x86_64-deb"];

/**
 * 把 Linux 的安装包加进清单。`names`：草稿里的文件名；`sigs`：签名文件名 → 内容；
 * `urlFor`：文件名 → 下载地址。缺安装包或缺签名时抛出错误，说明缺的是什么。
 */
export function addLinuxEntries(manifest, names, sigs, urlFor) {
  const out = structuredClone(manifest);
  out.platforms ??= {};
  for (const t of FORBIDDEN_TARGETS) delete out.platforms[t];
  const problems = [];
  for (const { target, pattern, what } of LINUX_TARGETS) {
    const found = names.filter((n) => pattern.test(n));
    if (found.length !== 1) {
      problems.push(found.length === 0 ? `没有 ${what}` : `${what} 不止一个（${found.join("、")}）`);
      continue;
    }
    const signature = (sigs[`${found[0]}.sig`] ?? "").trim();
    if (!signature) {
      problems.push(`${found[0]} 没有签名文件`);
      continue;
    }
    out.platforms[target] = { signature, url: urlFor(found[0]) };
  }
  if (problems.length) throw new Error(`草稿里的 Linux 安装包不全：${problems.join("；")}。请检查「打包 Linux」的日志。`);
  return out;
}

function fail(msg) {
  console.log(`::error::${msg}`);
  process.exit(1);
}

async function github(url, token, init = {}) {
  const res = await fetch(url, {
    redirect: "follow",
    ...init,
    headers: {
      accept: "application/vnd.github+json",
      "x-github-api-version": "2022-11-28",
      authorization: `Bearer ${token}`,
      ...init.headers,
    },
  });
  if (!res.ok) throw new Error(`GitHub 返回 ${res.status}：${init.method ?? "GET"} ${url}`);
  return res;
}

async function main() {
  const dryRun = process.argv.includes("--dry-run");
  const env = (name) => {
    const v = (process.env[name] ?? "").trim();
    if (!v) fail(`缺少环境变量 ${name}。`);
    return v;
  };
  const token = env("GITHUB_TOKEN");
  const repo = env("RELEASE_REPO");
  const releaseId = env("RELEASE_ID");
  const api = `https://api.github.com/repos/${repo}`;

  const release = await (await github(`${api}/releases/${releaseId}`, token)).json();
  // 文件多于一页（30 个）时 release.assets 不全，单独分页取。
  const assets = [];
  for (let page = 1; ; page++) {
    const batch = await (await github(`${api}/releases/${releaseId}/assets?per_page=100&page=${page}`, token)).json();
    assets.push(...batch);
    if (batch.length < 100) break;
  }
  const text = async (asset) => (await github(asset.url, token, { headers: { accept: "application/octet-stream" } })).text();

  const manifestAsset = assets.find((a) => a.name === "latest.json");
  if (!manifestAsset) fail("草稿里没有 latest.json（由 Windows、macOS 的打包任务生成）。请检查打包日志。");
  const manifest = JSON.parse(await text(manifestAsset));

  const sigs = {};
  for (const a of assets.filter((a) => a.name.endsWith(".sig"))) sigs[a.name] = await text(a);
  const tag = release.tag_name;
  const urlFor = (name) => `https://github.com/${repo}/releases/download/${tag}/${encodeURIComponent(name)}`;
  let next;
  try {
    next = addLinuxEntries(manifest, assets.map((a) => a.name), sigs, urlFor);
  } catch (e) {
    fail(e.message);
  }
  const body = JSON.stringify(next, null, 2) + "\n";
  console.log(`latest.json 的平台：${Object.keys(next.platforms).join(", ")}`);
  if (dryRun) {
    console.log(body);
    return;
  }
  // 发布的文件不能原地改：先删再传。
  await github(`${api}/releases/assets/${manifestAsset.id}`, token, { method: "DELETE" });
  await github(`https://uploads.github.com/repos/${repo}/releases/${releaseId}/assets?name=latest.json`, token, {
    method: "POST",
    headers: { "content-type": "application/json", "content-length": String(Buffer.byteLength(body)) },
    body,
  });
  console.log("已把 Linux 的三种安装包写进 latest.json。");
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch((e) => fail(e.message));
}
