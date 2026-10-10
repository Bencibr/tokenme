#!/usr/bin/env node
// Read-only, dependency-free gate for the JSON pet registry. Paths are relative
// to this script so the same command works from the repo or the panel directory.
import { closeSync, existsSync, openSync, readFileSync, readSync } from "node:fs";
import { fileURLToPath } from "node:url";

const petsDir = new URL("../apps/tokenme-bar/src/assets/pets/", import.meta.url);
const i18nFile = new URL("../apps/tokenme-bar/src/lib/i18n.ts", import.meta.url);
const assetRoles = ["atlas", "side", "left", "top"];
const frameNames = ["open", "half", "closed", "happy"];
const eyePoses = ["none", "right", "left", "top"];
const isObject = (value) => value !== null && typeof value === "object" && !Array.isArray(value);
const positive = (value) => Number.isFinite(value) && value > 0;
const pngSignature = Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]);

function pngSize(header) {
  if (header.length < 33 || !header.subarray(0, 8).equals(pngSignature)
      || header.readUInt32BE(8) !== 13 || header.toString("ascii", 12, 16) !== "IHDR") {
    throw new Error("expected a PNG signature and complete IHDR header");
  }
  const width = header.readUInt32BE(16), height = header.readUInt32BE(20);
  if (!width || !height || width > 0x7fffffff || height > 0x7fffffff) {
    throw new Error("PNG width and height must be positive 31-bit integers");
  }
  return { width, height };
}

function readPng(file) {
  const fd = openSync(new URL(file, petsDir), "r");
  try {
    const header = Buffer.alloc(33);
    return pngSize(header.subarray(0, readSync(fd, header, 0, header.length, 0)));
  } finally {
    closeSync(fd);
  }
}

function readModel(file) {
  if (!existsSync(new URL(file, petsDir))) {
    const error = new Error("file does not exist");
    error.code = "ENOENT";
    throw error;
  }
  return true;
}

function readLabels(source) {
  // Keep literals intact when removing comments: a commented-out label must not
  // count as a translation. Do not execute the browser's TypeScript module.
  const text = source.replace(
    /"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|`(?:\\.|[^`\\])*`|\/\*[\s\S]*?\*\/|\/\/[^\r\n]*/g,
    (token) => token.startsWith("/") ? token.replace(/[^\r\n]/g, " ") : token,
  );
  const labels = {};
  const dictionaries = /^[ \t]*const (zh|en)(?::[^=\r\n]+)?\s*=\s*\{([\s\S]*?)^[ \t]*\}(?: as const)?;?[ \t]*$/gm;
  for (const match of text.matchAll(dictionaries)) {
    const entries = /^[ \t]*("(?:\\.|[^"\\])*")\s*:\s*("(?:\\.|[^"\\])*")/gm;
    labels[match[1]] = new Map(Array.from(match[2].matchAll(entries), (entry) => [
      JSON.parse(entry[1]), JSON.parse(entry[2]),
    ]));
  }
  for (const language of ["zh", "en"]) {
    if (!labels[language]) throw new Error(`i18n.${language}: cannot read the literal dictionary in i18n.ts`);
  }
  return labels;
}

function validate(manifest, labels, loadPng = readPng, loadModel = readModel) {
  const errors = [];
  const fail = (path, message) => errors.push(`${path}: ${message}`);
  const object = (value, path) => {
    if (isObject(value)) return true;
    fail(path, "expected an object");
    return false;
  };
  const number = (value, path, mustBePositive = false) => {
    if (Number.isFinite(value) && (!mustBePositive || value > 0)) return true;
    fail(path, `expected a finite${mustBePositive ? " positive" : ""} number`);
    return false;
  };
  const point = (value, path) => {
    if (!Array.isArray(value) || value.length !== 2) {
      fail(path, "expected a pair [x, y]");
      return false;
    }
    const x = number(value[0], `${path}[0]`), y = number(value[1], `${path}[1]`);
    return x && y;
  };
  const string = (value, path) => {
    if (typeof value === "string" && value.trim()) return true;
    fail(path, "expected a non-empty string");
    return false;
  };

  if (!object(manifest, "$")) return errors;
  if (manifest.version !== 1) fail("version", "expected supported version 1");
  if (!object(manifest.skins, "skins")) return errors;
  if (!Object.keys(manifest.skins).length) fail("skins", "expected at least one skin");
  for (const [id, skin] of Object.entries(manifest.skins)) {
    const path = /^[a-z][a-z0-9_-]*$/.test(id) ? `skins.${id}` : `skins[${JSON.stringify(id)}]`;
    if (!/^[a-z][a-z0-9]*(?:[_-][a-z0-9]+)*$/.test(id)) {
      fail(path, "ID must start with a lowercase letter and use lowercase letters, digits, single hyphens or underscores");
    }
    if (!object(skin, path)) continue;
    if (string(skin.labelKey, `${path}.labelKey`)) {
      for (const language of ["zh", "en"]) {
        if (!labels[language].get(skin.labelKey)?.trim()) {
          fail(`${path}.labelKey`, `${JSON.stringify(skin.labelKey)} needs a non-empty translation in i18n.${language}`);
        }
      }
    }
    if (Object.hasOwn(skin, "render") && skin.render !== "raster" && skin.render !== "model") {
      fail(`${path}.render`, 'expected "raster" or "model"');
    }
    if (skin.render === "model") {
      if (object(skin.layout, `${path}.layout`)) {
        number(skin.layout.content, `${path}.layout.content`, true);
        if (object(skin.layout.dockShift, `${path}.layout.dockShift`)) {
          for (const dock of ["right", "left", "top"]) number(skin.layout.dockShift[dock], `${path}.layout.dockShift.${dock}`);
        }
      }
      if (object(skin.model, `${path}.model`)) {
        const file = skin.model.file, field = `${path}.model.file`;
        if (typeof file !== "string" || !/^[A-Za-z0-9][A-Za-z0-9._/-]*\.(?:glb|vrm)$/i.test(file) || file.includes("..")) {
          fail(field, "expected a local .glb or .vrm filename without parent traversal");
        } else {
          try {
            loadModel(file);
          } catch (error) {
            fail(field, `${file}: ${error.code === "ENOENT" ? "file does not exist" : error.message}`);
          }
        }
        if (Object.hasOwn(skin.model, "format") && !["glb", "vrm"].includes(skin.model.format)) {
          fail(`${path}.model.format`, 'expected "glb" or "vrm"');
        }
        if (Object.hasOwn(skin.model, "scale")) number(skin.model.scale, `${path}.model.scale`, true);
        if (Object.hasOwn(skin.model, "rotationY")) number(skin.model.rotationY, `${path}.model.rotationY`);
        if (Object.hasOwn(skin.model, "animation") && object(skin.model.animation, `${path}.model.animation`)) {
          const animation = skin.model.animation;
          if (Object.hasOwn(animation, "idle") && typeof animation.idle !== "boolean") {
            fail(`${path}.model.animation.idle`, "expected a boolean");
          }
          if (Object.hasOwn(animation, "blink") && typeof animation.blink !== "boolean") {
            fail(`${path}.model.animation.blink`, "expected a boolean");
          }
          for (const fieldName of ["bob", "sway"]) {
            if (Object.hasOwn(animation, fieldName)) number(animation[fieldName], `${path}.model.animation.${fieldName}`, true);
          }
          if (Object.hasOwn(animation, "period")) number(animation.period, `${path}.model.animation.period`, true);
          if (Object.hasOwn(animation, "clip") && typeof animation.clip !== "string") {
            fail(`${path}.model.animation.clip`, "expected a string");
          }
        }
        if (Object.hasOwn(skin.model, "camera") && object(skin.model.camera, `${path}.model.camera`)) {
          for (const fieldName of ["fov", "distance", "height", "targetY"]) {
            if (Object.hasOwn(skin.model.camera, fieldName)) number(skin.model.camera[fieldName], `${path}.model.camera.${fieldName}`, fieldName === "fov" || fieldName === "distance");
          }
        }
        if (Object.hasOwn(skin.model, "head") && object(skin.model.head, `${path}.model.head`)) {
          if (Object.hasOwn(skin.model.head, "enabled") && typeof skin.model.head.enabled !== "boolean") {
            fail(`${path}.model.head.enabled`, "expected a boolean");
          }
          if (Object.hasOwn(skin.model.head, "bone") && typeof skin.model.head.bone !== "string") {
            fail(`${path}.model.head.bone`, "expected a string");
          }
          for (const fieldName of ["maxYaw", "maxPitch"]) {
            if (Object.hasOwn(skin.model.head, fieldName)) number(skin.model.head[fieldName], `${path}.model.head.${fieldName}`, true);
          }
        }
        if (Object.hasOwn(skin.model, "poses") && object(skin.model.poses, `${path}.model.poses`)) {
          for (const dock of ["right", "left", "top"]) {
            const pose = skin.model.poses[dock];
            if (!Object.hasOwn(skin.model.poses, dock)) continue;
            if (!object(pose, `${path}.model.poses.${dock}`)) continue;
            for (const fieldName of ["cameraYaw", "cameraPitch", "cameraDistance", "rotationY"]) {
              if (Object.hasOwn(pose, fieldName)) number(pose[fieldName], `${path}.model.poses.${dock}.${fieldName}`, fieldName === "cameraDistance");
            }
            if (Object.hasOwn(pose, "anchor")) point(pose.anchor, `${path}.model.poses.${dock}.anchor`);
          }
        }
      }
      // A model skin has no raster atlas, eye coordinates or eyelid overlay.
      continue;
    }
    if (object(skin.assets, `${path}.assets`)) {
      // Inspect only referenced assets; unused/old PNGs are outside this gate.
      for (const role of assetRoles) {
        const file = skin.assets[role], field = `${path}.assets.${role}`;
        if (typeof file !== "string" || !/^[A-Za-z0-9][A-Za-z0-9._-]*\.png$/.test(file) || file.includes("..")) {
          fail(field, "expected a local .png filename using letters, digits, dots, hyphens or underscores (no directories)");
          continue;
        }
        try {
          const { width, height } = loadPng(file);
          if (role === "atlas" && (width !== height || width % 2 !== 0 || height % 2 !== 0)) {
            fail(field, `${file} is ${width}x${height}; a 2x2 atlas must be square with even pixel dimensions`);
          }
        } catch (error) {
          fail(field, `${file}: ${error.code === "ENOENT" ? "file does not exist" : error.message}`);
        }
      }
    }
    const layout = skin.layout;
    if (object(layout, `${path}.layout`)) {
      for (const field of ["content", "art", "atlas"]) number(layout[field], `${path}.layout.${field}`, true);
      number(layout.artOffset, `${path}.layout.artOffset`);
      if (object(layout.dockShift, `${path}.layout.dockShift`)) {
        for (const dock of ["right", "left", "top"]) number(layout.dockShift[dock], `${path}.layout.dockShift.${dock}`);
      }
      // These are CSS pixels, not the PNG's physical pixel dimensions.
      if (positive(layout.art) && positive(layout.atlas) && layout.atlas !== layout.art * 2) {
        fail(`${path}.layout.atlas`, "must equal 2 * layout.art for a 2x2 atlas");
      }
    }
    const art = layout?.art;
    if (object(skin.frames, `${path}.frames`)) {
      frameNames.forEach((name, index) => {
        const field = `${path}.frames.${name}`, frame = skin.frames[name];
        if (!point(frame, field) || !positive(art)) return;
        // CSS background offsets are signed. Legacy art has small alignment
        // corrections; allow a quarter cell around each reading-order tile.
        const expected = [-(index % 2) * art, -Math.floor(index / 2) * art];
        frame.forEach((offset, axis) => {
          if (Math.abs(offset - expected[axis]) > art / 4) {
            fail(`${field}[${axis}]`, `must be within ${art / 4} of ${expected[axis]} for its 2x2 tile`);
          }
        });
      });
    }
    if (object(skin.eyes, `${path}.eyes`)) {
      const poses = [...eyePoses, ...(Object.hasOwn(skin.eyes, "happy") ? ["happy"] : [])];
      for (const pose of poses) {
        const field = `${path}.eyes.${pose}`, eyes = skin.eyes[pose];
        if (!object(eyes, field)) continue;
        if (Object.hasOwn(eyes, "rotation")) point(eyes.rotation, `${field}.rotation`);
        if (Object.hasOwn(eyes, "enabled") && typeof eyes.enabled !== "boolean") {
          fail(`${field}.enabled`, "expected a boolean");
        }
        if (Object.hasOwn(eyes, "tracking") && !["pupil", "native"].includes(eyes.tracking)) {
          fail(`${field}.tracking`, 'expected "pupil" or "native"');
        }
        for (const dimension of ["size", "height", "pupil"]) number(eyes[dimension], `${field}.${dimension}`, true);
        if (positive(eyes.pupil) && positive(eyes.size) && positive(eyes.height)
            && (eyes.pupil > eyes.size || eyes.pupil > eyes.height)) {
          fail(`${field}.pupil`, "must fit both eye size and height");
        }
        for (const side of ["left", "right"]) {
          if (!point(eyes[side], `${field}.${side}`) || !positive(art)) continue;
          eyes[side].forEach((coordinate, axis) => {
            // Anchors are eye centres (the renderer translates by -50%).
            const dimension = axis === 0 ? eyes.size : eyes.height;
            const radius = positive(dimension) ? dimension / 2 : 0;
            if (coordinate < radius || coordinate > art - radius) {
              fail(`${field}.${side}[${axis}]`, `eye box must stay inside the artwork (0..${art})`);
            }
          });
        }
      }
    }
    if (object(skin.eyelid, `${path}.eyelid`)) {
      for (const color of ["fill", "stroke"]) string(skin.eyelid[color], `${path}.eyelid.${color}`);
      if (Object.hasOwn(skin.eyelid, "strokeWidth")) number(skin.eyelid.strokeWidth, `${path}.eyelid.strokeWidth`, true);
    }
    if (Object.hasOwn(skin, "animation") && object(skin.animation, `${path}.animation`)) {
      if (skin.animation.blink !== "overlay" && skin.animation.blink !== "atlas") {
        fail(`${path}.animation.blink`, 'expected "overlay" or "atlas"');
      }
    }
    // Unknown fields are deliberately allowed for future renderer extensions.
  }
  return errors;
}

function selfTest(manifest, labels, source) {
  const id = Object.keys(manifest.skins)[0], path = `skins.${id}`;
  const cases = [
    ["version", (m) => { m.version = 2; }],
    ["skins", (m) => { m.skins = []; }],
    [`${path}.layout.art`, (m) => { m.skins[id].layout.art = 0; }],
    [`${path}.layout.atlas`, (m) => { m.skins[id].layout.atlas += 1; }],
    [`${path}.layout.dockShift.top`, (m) => { m.skins[id].layout.dockShift.top = Infinity; }],
    [`${path}.frames.open`, (m) => { m.skins[id].frames.open = [0]; }],
    [`${path}.frames.happy[0]`, (m) => { m.skins[id].frames.happy[0] = -1e6; }],
    [`${path}.eyes.none.left[0]`, (m) => { m.skins[id].eyes.none.left[0] = NaN; }],
    [`${path}.eyes.none.rotation`, (m) => { m.skins[id].eyes.none.rotation = [25]; }],
    [`${path}.eyes.right.rotation[1]`, (m) => { m.skins[id].eyes.right.rotation = [25, Infinity]; }],
    [`${path}.eyes.none.enabled`, (m) => { m.skins[id].eyes.none.enabled = "false"; }],
    [`${path}.eyes.top.right[1]`, (m) => { m.skins[id].eyes.top.right[1] = m.skins[id].layout.art; }],
    [`${path}.eyes.none.pupil`, (m) => { m.skins[id].eyes.none.pupil = m.skins[id].eyes.none.height + 1; }],
    [`${path}.eyes.happy.right`, (m) => { m.skins[id].eyes.happy = { ...m.skins[id].eyes.none, right: null }; }],
    [`${path}.eyes.happy.rotation[0]`, (m) => { m.skins[id].eyes.happy = { ...m.skins[id].eyes.none, rotation: [NaN, 0] }; }],
    [`${path}.assets.atlas`, (m) => { m.skins[id].assets.atlas = "../escape.png"; }],
    [`${path}.assets.top`, (m) => { m.skins[id].assets.top = "missing-pet-skin-self-test.png"; }],
    [`${path}.labelKey`, (m) => { m.skins[id].labelKey = "missing.skin.label"; }],
    [`${path}.animation.blink`, (m) => { m.skins[id].animation = { blink: "unknown" }; }],
    ['skins["../escape"]', (m) => { m.skins["../escape"] = m.skins[id]; delete m.skins[id]; }],
  ];
  for (const [field, mutate] of cases) {
    const malformed = structuredClone(manifest);
    mutate(malformed);
    if (!validate(malformed, labels).some((error) => error.startsWith(`${field}:`))) {
      throw new Error(`self-test: malformed ${field} was not rejected at its field path`);
    }
  }
  const extended = structuredClone(manifest);
  extended.skins[id].eyes.happy = structuredClone(extended.skins[id].eyes.none);
  extended.skins[id].eyes.happy.rotation = [-25, 25];
  extended.skins[id].eyes.happy.enabled = true;
  extended.skins[id].eyes.none.enabled = false;
  extended.skins[id].future = { enabled: true };
  for (const blink of ["overlay", "atlas"]) {
    extended.skins[id].animation = { blink, future: true };
    if (validate(extended, labels).length) throw new Error("self-test: valid extension rejected");
  }
  delete extended.skins[id].animation;
  if (validate(extended, labels).length) throw new Error("self-test: optional animation required");
  const modelManifest = structuredClone(manifest);
  modelManifest.skins.model_fixture = {
    labelKey: manifest.skins[id].labelKey,
    render: "model",
    model: { file: "models/fixture.vrm", format: "vrm" },
    layout: { content: 224, dockShift: { right: -72, left: 72, top: 66 } },
  };
  if (validate(modelManifest, labels, undefined, () => true).length) {
    throw new Error("self-test: valid model skin rejected");
  }
  modelManifest.skins.model_fixture.model.file = "../escape.vrm";
  if (!validate(modelManifest, labels, undefined, () => true).some((error) => error.startsWith("skins.model_fixture.model.file:"))) {
    throw new Error("self-test: model path traversal accepted");
  }
  const english = source.indexOf("const en:");
  const commented = source.slice(0, english) + source.slice(english).replace(
    `"${manifest.skins[id].labelKey}":`, `// "${manifest.skins[id].labelKey}":`,
  );
  if (!validate(manifest, readLabels(commented)).some((error) => error.includes("i18n.en"))) {
    throw new Error("self-test: commented-out English label counted as a translation");
  }
  const header = Buffer.alloc(33);
  pngSignature.copy(header);
  header.writeUInt32BE(13, 8);
  header.write("IHDR", 12, "ascii");
  header.writeUInt32BE(1254, 16);
  header.writeUInt32BE(1254, 20);
  if (pngSize(header).width !== 1254) throw new Error("self-test: valid PNG header rejected");
  for (const broken of [Buffer.alloc(33), header.subarray(0, 24), Buffer.from(header)]) {
    if (broken.length === 33 && broken[0] === 137) broken.writeUInt32BE(0, 16);
    let rejected = false;
    try { pngSize(broken); } catch { rejected = true; }
    if (!rejected) throw new Error("self-test: malformed PNG header accepted");
  }
  for (const size of [{ width: 1253, height: 1253 }, { width: 1254, height: 1252 }]) {
    if (!validate(manifest, labels, () => size).some((error) => error.startsWith(`${path}.assets.atlas:`))) {
      throw new Error("self-test: invalid 2x2 PNG dimensions accepted");
    }
  }
  console.log("pet-skins: self-test OK (malformed manifests, PNGs, translations and valid extensions)");
}

try {
  if (process.argv.length > 3 || (process.argv[2] && process.argv[2] !== "--self-test")) {
    throw new Error("usage: node scripts/check-pet-skins.mjs [--self-test]");
  }
  const manifest = JSON.parse(readFileSync(new URL("pet-skins.json", petsDir), "utf8"));
  const source = readFileSync(i18nFile, "utf8"), labels = readLabels(source);
  const errors = validate(manifest, labels);
  if (errors.length) {
    console.error(`pet-skins: FAIL (${fileURLToPath(new URL("pet-skins.json", petsDir))})`);
    for (const error of errors) console.error(`  - ${error}`);
    process.exitCode = 1;
  } else {
    if (process.argv[2] === "--self-test") selfTest(manifest, labels, source);
    console.log(`pet-skins: OK (${Object.keys(manifest.skins).length} skins; referenced PNGs, layout, eyes and labels)`);
  }
} catch (error) {
  console.error(`pet-skins: FAIL: ${error.message}`);
  process.exitCode = 1;
}
