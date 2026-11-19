// 运行：node --test "scripts/*.test.mjs"
import assert from "node:assert/strict";
import test from "node:test";

import { addLinuxEntries } from "./add-linux-updates.mjs";
import { rewriteManifest } from "./mirror-to-oss.mjs";

const names = [
  "FlowAcross_0.2.4_x64-setup.exe",
  "FlowAcross_0.2.4_amd64.AppImage",
  "FlowAcross_0.2.4_amd64.AppImage.sig",
  "FlowAcross_0.2.4_amd64.deb",
  "FlowAcross_0.2.4_amd64.deb.sig",
  "FlowAcross_0.2.4_x86_64.pkg.tar.zst",
  "FlowAcross_0.2.4_x86_64.pkg.tar.zst.sig",
  "latest.json",
];
const sigs = {
  "FlowAcross_0.2.4_amd64.AppImage.sig": "sig-appimage\n",
  "FlowAcross_0.2.4_amd64.deb.sig": "sig-deb\n",
  "FlowAcross_0.2.4_x86_64.pkg.tar.zst.sig": "sig-pacman\n",
};
const urlFor = (n) => `https://github.com/o/r/releases/download/v0.2.4/${encodeURIComponent(n)}`;
const base = () => ({
  version: "0.2.4",
  platforms: {
    "windows-x86_64": { signature: "sig-win", url: urlFor("FlowAcross_0.2.4_x64-setup.exe") },
  },
});

test("三种 Linux 安装包各占一项，签名去掉首尾空白", () => {
  const out = addLinuxEntries(base(), names, sigs, urlFor);
  assert.deepEqual(Object.keys(out.platforms).sort(), ["linux-x86_64-appimage", "linux-x86_64-dpkg", "linux-x86_64-pacman", "windows-x86_64"]);
  assert.deepEqual(out.platforms["linux-x86_64-dpkg"], { signature: "sig-deb", url: urlFor("FlowAcross_0.2.4_amd64.deb") });
  assert.equal(out.platforms["linux-x86_64-pacman"].url, urlFor("FlowAcross_0.2.4_x86_64.pkg.tar.zst"));
  assert.equal(out.platforms["linux-x86_64-appimage"].signature, "sig-appimage");
  // 原有的平台原样保留。
  assert.equal(out.platforms["windows-x86_64"].signature, "sig-win");
});

test("旧版本会装错的两项不会留在清单里", () => {
  const m = base();
  m.platforms["linux-x86_64"] = { signature: "x", url: urlFor("FlowAcross_0.2.4_amd64.AppImage") };
  m.platforms["linux-x86_64-deb"] = { signature: "x", url: urlFor("FlowAcross_0.2.4_amd64.deb") };
  const out = addLinuxEntries(m, names, sigs, urlFor);
  assert.equal(out.platforms["linux-x86_64"], undefined);
  assert.equal(out.platforms["linux-x86_64-deb"], undefined);
  // 传进来的清单不被改动。
  assert.ok(m.platforms["linux-x86_64"]);
});

test("缺安装包或缺签名时说清楚缺什么", () => {
  assert.throws(() => addLinuxEntries(base(), names.filter((n) => !n.includes(".pkg.tar.zst")), sigs, urlFor), /没有 Arch 安装包/);
  const { "FlowAcross_0.2.4_amd64.deb.sig": _, ...rest } = sigs;
  assert.throws(() => addLinuxEntries(base(), names, rest, urlFor), /FlowAcross_0\.2\.4_amd64\.deb 没有签名文件/);
});

test("加进去的地址能被复制到 OSS 的脚本改写", () => {
  const out = rewriteManifest(addLinuxEntries(base(), names, sigs, urlFor), "https://dl.example.com/FlowAcross", "v0.2.4", names);
  assert.equal(out.platforms["linux-x86_64-pacman"].url, "https://dl.example.com/FlowAcross/releases/v0.2.4/FlowAcross_0.2.4_x86_64.pkg.tar.zst");
});
