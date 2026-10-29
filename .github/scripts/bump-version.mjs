#!/usr/bin/env node
// Set the [package] version in Cargo.toml from semantic-release.
//
// This exists so the release workflow does not need cargo-edit (or any other
// tool) just to change one line. Cargo.lock is refreshed afterwards by
// `cargo update -p skot` in the workflow's prepare command.

import { readFile, writeFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";

const version = process.argv[2];
if (!version || !/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(version)) {
  throw new Error(`expected a semantic version, got ${JSON.stringify(version)}`);
}

const cargoToml = fileURLToPath(new URL("../../Cargo.toml", import.meta.url));
const text = await readFile(cargoToml, "utf8");
const newline = text.includes("\r\n") ? "\r\n" : "\n";
const lines = text.split(/\r?\n/);

let inPackage = false;
let updated = false;

for (let i = 0; i < lines.length; i += 1) {
  const line = lines[i];
  if (/^\s*\[/.test(line)) {
    inPackage = line.trim() === "[package]";
    continue;
  }
  if (inPackage && /^\s*version\s*=/.test(line)) {
    lines[i] = line.replace(/"[^"]*"/, `"${version}"`);
    updated = true;
    break;
  }
}

if (!updated) {
  throw new Error(`could not find a version field in [package] in ${cargoToml}`);
}

await writeFile(cargoToml, lines.join(newline), "utf8");
