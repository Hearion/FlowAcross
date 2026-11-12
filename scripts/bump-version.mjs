#!/usr/bin/env node
// 统一修改版本号：Cargo.toml（workspace）、tauri.conf.json、package.json、package-lock.json、Cargo.lock。
// 用法：node scripts/bump-version.mjs 0.2.0
import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const version = process.argv[2]?.replace(/^v/, "");
if (!version || !/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version)) {
  console.error("用法：node scripts/bump-version.mjs <版本号>，例如 0.2.0");
  process.exit(1);
}

const file = (p) => join(root, p);

// Cargo.toml：只改 [workspace.package] 里的 version。
const cargoPath = file("Cargo.toml");
const cargo = readFileSync(cargoPath, "utf8");
const nextCargo = cargo.replace(/(\[workspace\.package\][^[]*?\nversion = ")[^"]+(")/, `$1${version}$2`);
if (nextCargo === cargo && !cargo.includes(`version = "${version}"`)) {
  console.error("没有在 Cargo.toml 的 [workspace.package] 里找到 version。");
  process.exit(1);
}
writeFileSync(cargoPath, nextCargo);

const editJson = (p, edit) => {
  const path = file(p);
  const data = JSON.parse(readFileSync(path, "utf8"));
  edit(data);
  writeFileSync(path, JSON.stringify(data, null, 2) + "\n");
};
editJson("app/src-tauri/tauri.conf.json", (c) => (c.version = version));
editJson("app/package.json", (c) => (c.version = version));
editJson("app/package-lock.json", (c) => {
  c.version = version;
  if (c.packages?.[""]) c.packages[""].version = version;
});

// 刷新 Cargo.lock 里 workspace 成员的版本号（不升级第三方依赖）。
execFileSync("cargo", ["update", "--workspace", "--offline"], { cwd: root, stdio: "inherit" });

console.log(`
已把版本号改为 ${version}。接下来：
  git commit -am "发布 v${version}"
  git tag -a v${version} -m "更新说明（会显示在 App 的更新提示里）"
  git push origin main v${version}
`);
