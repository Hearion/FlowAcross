// Collect license notices from the locked dependencies actually available locally.
// Includes build/dev and other-platform Cargo dependencies conservatively; it is
// an inventory of declarations, not a legal opinion on compatibility.
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const metadata = JSON.parse(execFileSync("cargo", ["metadata", "--format-version", "1", "--locked"], {
  cwd: root, encoding: "utf8", maxBuffer: 30_000_000,
}));
const workspace = new Set(metadata.workspace_members);
const lock = JSON.parse(readFileSync(path.join(root, "app/package-lock.json"), "utf8"));
const dependencies = [];
const texts = new Map();
const missing = [];
const pending = [];
const cachePath = path.join(root, "docs/third-party-license-sources.json");
const cache = existsSync(cachePath) ? JSON.parse(readFileSync(cachePath, "utf8")) : { records: {}, texts: {} };
const fetchMissing = process.argv.includes("--fetch-missing");
const noticeName = /^(?:licen[cs]e(?:[._-].*)?|copying(?:[._-].*)?|copyright(?:[._-].*)?|notice(?:[._-].*)?|authors(?:[._-].*)?|ofl(?:[._-].*)?|unlicense(?:\..*)?)$/i;
const excluded = new Set([".git", "target", "node_modules", "tests", "test", "fixtures", "examples", "benches"]);

function licenseFiles(directory, depth = 0) {
  const result = [];
  for (const item of readdirSync(directory, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name, "en"))) {
    const file = path.join(directory, item.name);
    if (item.isFile() && noticeName.test(item.name)) result.push(file);
    else if (item.isDirectory() && depth < 3 && !excluded.has(item.name) && !item.name.startsWith(".")) {
      result.push(...licenseFiles(file, depth + 1));
    }
  }
  return result;
}

function collect(dependency, directory, extraFile) {
  const files = new Set(licenseFiles(directory));
  if (extraFile) {
    const file = path.resolve(directory, extraFile);
    const relative = path.relative(directory, file);
    if (!relative.startsWith("..") && !path.isAbsolute(relative) && existsSync(file)) files.add(file);
  }
  const notices = [];
  for (const file of [...files].sort()) {
    const text = readFileSync(file, "utf8").replace(/^\uFEFF/, "").replace(/\r\n/g, "\n").trim();
    if (!text) continue;
    const digest = createHash("sha256").update(text).digest("hex");
    texts.set(digest, text);
    notices.push({ file: path.relative(directory, file).replaceAll("\\", "/"), sha256: digest });
  }
  dependencies.push({ ...dependency, notices });
  if (!notices.some((n) => !/^authors/i.test(n.file))) pending.push({ dependency: dependencies.at(-1), directory });
}

for (const pkg of metadata.packages.filter((p) => !workspace.has(p.id)).sort((a, b) => a.id.localeCompare(b.id, "en"))) {
  collect({ ecosystem: "cargo", name: pkg.name, version: pkg.version, license: pkg.license,
    repository: pkg.repository, source: pkg.source }, path.dirname(pkg.manifest_path), pkg.license_file);
}
for (const [location, entry] of Object.entries(lock.packages).filter(([p, e]) => p && !e.dev).sort(([a], [b]) => a.localeCompare(b, "en"))) {
  const directory = path.join(root, "app", location);
  if (!existsSync(directory)) {
    missing.push(`npm:${location}@${entry.version} (run npm ci first)`);
    continue;
  }
  const pkg = JSON.parse(readFileSync(path.join(directory, "package.json"), "utf8"));
  collect({ ecosystem: "npm", name: pkg.name, version: pkg.version, license: pkg.license || entry.license,
    repository: typeof pkg.repository === "string" ? pkg.repository : pkg.repository?.url || null,
    source: entry.resolved || null }, directory);
}

// Some crate tarballs omit the repository's license files. Supplement them only
// from the exact upstream commit recorded in .cargo_vcs_info.json, with a local
// cache so ordinary regeneration does not depend on raw.githubusercontent.com.
const requests = new Map();
function download(url) {
  if (!requests.has(url)) requests.set(url, (async () => {
    try {
      const response = await fetch(url, { signal: AbortSignal.timeout(12000) });
      if (!response.ok) return null;
      const value = (await response.text()).replace(/\r\n/g, "\n").trim();
      return value.length > 100 && !value.startsWith("<!DOCTYPE") ? value : null;
    } catch { return null; }
  })());
  return requests.get(url);
}
async function supplement({ dependency, directory }) {
  const key = `${dependency.ecosystem}:${dependency.name}@${dependency.version}`;
  let urls = cache.records[key] || [];
  if (!urls.length && fetchMissing) {
    const vcsPath = path.join(directory, ".cargo_vcs_info.json");
    const vcs = existsSync(vcsPath) ? JSON.parse(readFileSync(vcsPath, "utf8")) : null;
    // libappindicator-sys has no repository field; the sibling parent crate
    // records the upstream repository for this same workspace.
    const repository = dependency.repository || (dependency.name === "libappindicator-sys"
      ? metadata.packages.find((p) => p.name === "libappindicator")?.repository : null);
    const match = repository?.match(/^https:\/\/github\.com\/([^/]+\/[^/#]+?)(?:\.git)?\/?$/);
    if (match && vcs?.git?.sha1) {
      const base = `https://raw.githubusercontent.com/${match[1]}/${vcs.git.sha1}/`;
      const names = ["LICENSE", "LICENSE.md", "LICENSE.txt", "LICENSE-MIT", "LICENSE-APACHE", "COPYING", "COPYING.MIT", "COPYING.APACHE"];
      const results = await Promise.all(names.map(async (name) => ({ url: base + name, text: await download(base + name) })));
      for (const result of results.filter((r) => r.text)) {
        cache.texts[result.url] = result.text;
        urls.push(result.url);
      }
      if (urls.length) cache.records[key] = urls;
    }
  }
  // The winapi target-library crates omit VCS/notice files. Preserve the
  // installed parent winapi crate's repository-wide license notices as well.
  if (!urls.length && dependency.name.startsWith("winapi-") && dependency.repository === "https://github.com/retep998/winapi-rs") {
    const parent = metadata.packages.find((p) => p.name === "winapi");
    if (parent) for (const file of licenseFiles(path.dirname(parent.manifest_path))) {
      const text = readFileSync(file, "utf8").replace(/\r\n/g, "\n").trim();
      const sha256 = createHash("sha256").update(text).digest("hex");
      texts.set(sha256, text);
      dependency.notices.push({ file: `winapi-${parent.version}/${path.basename(file)} (upstream repository notice)`, sha256 });
    }
  }
  if (!urls.length && dependency.name === "libappindicator-sys") {
    const parent = metadata.packages.find((p) => p.name === "libappindicator");
    if (parent) for (const file of licenseFiles(path.dirname(parent.manifest_path))) {
      const text = readFileSync(file, "utf8").replace(/\r\n/g, "\n").trim();
      const sha256 = createHash("sha256").update(text).digest("hex");
      texts.set(sha256, text);
      dependency.notices.push({ file: `libappindicator-${parent.version}/${path.basename(file)} (parent workspace notice)`, sha256 });
    }
  }
  // These upstreams declare licenses in source/README but ship no license text.
  // For r-efi, select Apache-2.0 from its explicitly offered alternatives.
  const canonical = dependency.name === "r-efi" ? "Apache-2.0" : dependency.name === "selectors" ? "MPL-2.0" : null;
  if (!urls.length && canonical && fetchMissing) {
    const url = `https://raw.githubusercontent.com/spdx/license-list-data/v3.27.0/text/${canonical}.txt`;
    const text = await download(url);
    if (text) {
      cache.texts[url] = text;
      cache.records[key] = urls = [url];
    }
  }
  if (canonical) dependency.selectedLicense = canonical;
  for (const url of urls) {
    const text = cache.texts[url];
    if (!text) continue;
    const sha256 = createHash("sha256").update(text).digest("hex");
    texts.set(sha256, text);
    dependency.notices.push({ file: url, sha256 });
  }
}
for (let start = 0; start < pending.length; start += 4) {
  await Promise.all(pending.slice(start, start + 4).map(supplement));
}
for (const d of dependencies) if (!d.license || !d.notices.some((n) => !/^authors/i.test(n.file))) missing.push(`${d.ecosystem}:${d.name}@${d.version}`);
if (fetchMissing) {
  cache.records = Object.fromEntries(Object.entries(cache.records).sort(([a], [b]) => a.localeCompare(b, "en")));
  cache.texts = Object.fromEntries(Object.entries(cache.texts).sort(([a], [b]) => a.localeCompare(b, "en")));
  writeFileSync(cachePath, JSON.stringify(cache, null, 2) + "\n");
}
const inventory = {
  description: "Declared licenses and notices from all locked Cargo dependencies (including build/dev and non-host targets) and production npm dependencies. Not a legal compatibility certification.",
  lockfiles: Object.fromEntries(["Cargo.lock", "app/package-lock.json"].map((file) => [file,
    createHash("sha256").update(readFileSync(path.join(root, file))).digest("hex")])),
  dependencies, missing,
};
const header = `FlowAcross third-party copyright and license notices

FlowAcross-owned work is GPL-3.0-only. The components below retain their own
licenses. This file includes production npm dependencies and all locked Cargo
dependencies (including build/dev and other platforms) conservatively; listing
a component does not mean it is shipped in every build.
License expressions are upstream declarations, not a compatibility certification.
For dependency source locations, versions, license expressions and lock hashes,
see docs/third-party-dependencies.json and the corresponding release lockfiles.
Regenerate with: node scripts/generate-third-party-notices.mjs
Run npm ci in app first, on the target release platform.
Review missing entries and additional upstream notice requirements before release.

`;
const listing = dependencies.map((d) => `${d.ecosystem}: ${d.name} ${d.version}\nLicense: ${d.license || "UNKNOWN"}\nSource: ${d.repository || d.source || "See lockfile"}\n${d.notices.map((n) => `  ${n.file} -> ${n.sha256}`).join("\n")}\n`).join("\n");
const sections = [...texts].sort(([a], [b]) => a.localeCompare(b)).map(([id, text]) => `\n${"=".repeat(72)}\nNotice SHA256: ${id}\n${"=".repeat(72)}\n${text}\n`).join("");
writeFileSync(path.join(root, "THIRD_PARTY_NOTICES.txt"), header + listing + (missing.length ? `\nEntries requiring review:\n${missing.join("\n")}\n` : "") + sections);
writeFileSync(path.join(root, "docs/third-party-dependencies.json"), JSON.stringify(inventory, null, 2) + "\n");
console.log(`Collected ${dependencies.length} dependencies and ${texts.size} distinct notices. ${missing.length} entries require review.`);
if (missing.length) {
  console.log(missing.join("\n"));
  process.exitCode = 1;
}
