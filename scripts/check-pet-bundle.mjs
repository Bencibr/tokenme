#!/usr/bin/env node
// Audit the built files: a lazy import can still ship an archived model chunk.
import { readdirSync, readFileSync } from "node:fs";
import { basename, join } from "node:path";
import { fileURLToPath } from "node:url";

const panel = fileURLToPath(new URL("../apps/tokenme-bar/", import.meta.url));
const manifest = JSON.parse(readFileSync(join(panel, "src/assets/pets/pet-skins.json"), "utf8"));
const sourcePngs = readdirSync(join(panel, "src/assets/pets")).filter((file) => /\.(png|webp)$/.test(file));
const pngs = [...new Set(Object.values(manifest.skins)
  .filter((skin) => skin.render !== "model")
  .flatMap((skin) => [...Object.values(skin.assets), ...(skin.reactionAtlas ? [skin.reactionAtlas.file] : [])]))];
const escape = (text) => text.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
const matchesPng = (file, png) => new RegExp(`^${escape(png.slice(0, png.lastIndexOf(".")))}-[\\w-]+${escape(png.slice(png.lastIndexOf(".")))}$`).test(basename(file));

function validate(files) {
  const errors = [];
  for (const file of files) {
    if (/\.(glb|vrm)$/i.test(file) || /^PetModel-[\w-]+\.js$/.test(basename(file))) {
      errors.push(`archived model payload: ${file}`);
    }
    if (sourcePngs.some((png) => matchesPng(file, png)) && !pngs.some((png) => matchesPng(file, png))) {
      errors.push(`unreferenced PNG: ${file}`);
    }
  }
  for (const png of pngs) {
    if (files.filter((file) => matchesPng(file, png)).length !== 1) {
      errors.push(`expected exactly one built asset for ${png}`);
    }
  }
  return errors;
}

function walk(directory) {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const path = join(directory, entry.name);
    return entry.isDirectory() ? walk(path) : [path];
  });
}

if (process.argv.includes("--self-test")) {
  const valid = pngs.map((png) => `assets/${png.slice(0, png.lastIndexOf("."))}-fixture${png.slice(png.lastIndexOf("."))}`);
  if (validate(valid).length) throw new Error("valid raster bundle rejected");
  if (validate([...valid, "assets/application-icon-fixture.png"]).length) throw new Error("unrelated application image rejected");
  for (const file of ["assets/hidden.glb", "assets/hidden.vrm", "assets/PetModel-fixture.js", "assets/kitten-edge-top-fixture.png"]) {
    if (!validate([...valid, file]).length) throw new Error(`unexpected payload accepted: ${file}`);
  }
  if (!validate(valid.slice(1)).length) throw new Error("missing visible skin image accepted");
  if (!validate([...valid, valid[0]]).length) throw new Error("duplicate skin image accepted");
  console.log("pet-bundle self-test: OK (missing, duplicate and archived payloads rejected)");
} else {
  const errors = validate(walk(join(panel, "dist")));
  if (errors.length) {
    console.error(errors.join("\n"));
    process.exitCode = 1;
  } else {
    console.log(`pet-bundle: OK (${pngs.length} referenced images; archived models and renderer excluded)`);
  }
}
