import manifest from "../assets/pets/pet-skins.json";

export type PetDock = "none" | "right" | "left" | "top";
export type Point = [number, number];
export type PetEyes = {
  left: Point;
  right: Point;
  size: number;
  height: number;
  pupil: number;
  enabled?: boolean;
  tracking?: "pupil" | "native";
  rotation?: Point;
};

export type PetModelPose = {
  cameraYaw?: number;
  cameraPitch?: number;
  cameraDistance?: number;
  rotationY?: number;
  anchor?: Point;
};

export type PetModel = {
  /** A local .glb or .vrm file in the skin package. */
  file: string;
  format?: "glb" | "vrm";
  scale?: number;
  rotationY?: number;
  animation?: {
    /** Keep the procedural idle loop enabled when the asset has no clips. */
    idle?: boolean;
    /** Bob amplitude as a fraction of the normalized model height. */
    bob?: number;
    /** Sway amplitude in degrees. */
    sway?: number;
    /** Idle cycle length in seconds. */
    period?: number;
    /** Optional embedded GLTF animation name. */
    clip?: string;
    blink?: boolean;
  };
  camera?: {
    fov?: number;
    distance?: number;
    height?: number;
    targetY?: number;
  };
  head?: {
    enabled?: boolean;
    /** Normalized VRM bone name, or a custom node name for plain GLB. */
    bone?: string;
    maxYaw?: number;
    maxPitch?: number;
  };
  poses?: Partial<Record<Exclude<PetDock, "none">, PetModelPose>>;
};

type RawLayout = {
  content: number;
  dockShift: { right: number; left: number; top: number };
};

type RawRasterSkin = {
  labelKey: string;
  render?: "raster";
  assets: { atlas: string; side: string; left: string; top: string };
  layout: RawLayout & {
    art: number;
    atlas: number;
    artOffset: number;
  };
  frames: { open: Point; half: Point; closed: Point; happy: Point };
  // Atlas blink is suitable for baked eyes; overlay blink keeps the same body
  // in place for skins whose empty eye sockets are filled by tracked pupils.
  animation?: { blink: "atlas" | "overlay" };
  eyes: Record<PetDock, PetEyes> & { happy?: PetEyes };
  eyelid: { fill: string; stroke: string; strokeWidth?: number };
};

type RawModelSkin = {
  labelKey: string;
  render: "model";
  model: PetModel;
  layout: RawLayout;
};

type RawSkin = RawRasterSkin | RawModelSkin;
type RawManifest = { version: number; skins: Record<string, RawSkin> };

const rawManifest = manifest as unknown as RawManifest;
const petPngs = import.meta.glob("../assets/pets/*.png", {
  eager: true,
  query: "?url",
  import: "default",
}) as Record<string, string>;
const petModels = import.meta.glob("../assets/pets/**/*.{glb,vrm}", {
  eager: true,
  query: "?url",
  import: "default",
}) as Record<string, string>;

export type PetSkinName = keyof typeof manifest.skins;
export type RasterPetSkinConfig = RawRasterSkin & {
  assets: { atlas: string; side: string; left: string; top: string };
};
export type ModelPetSkinConfig = RawModelSkin & {
  model: PetModel & { url: string };
};
export type PetSkinConfig = RasterPetSkinConfig | ModelPetSkinConfig;

const assetUrl = (skin: string, file: string) => {
  const key = `../assets/pets/${file}`;
  const url = petPngs[key];
  if (!url) throw new Error(`Pet skin "${skin}" references missing asset "${file}"`);
  return url;
};

const modelUrl = (skin: string, file: string) => {
  const key = `../assets/pets/${file}`;
  const url = petModels[key];
  if (!url) throw new Error(`Pet skin "${skin}" references missing model "${file}"`);
  return url;
};

const compileSkin = ([name, raw]: [string, RawSkin]): [string, PetSkinConfig] => [name, {
  ...raw,
  ...(raw.render === "model" ? {
    model: { ...raw.model, url: modelUrl(name, raw.model.file) },
  } : {
    assets: {
      atlas: assetUrl(name, raw.assets.atlas),
      side: assetUrl(name, raw.assets.side),
      left: assetUrl(name, raw.assets.left),
      top: assetUrl(name, raw.assets.top),
    },
  }),
} as PetSkinConfig];

export const PET_SKINS = Object.fromEntries(
  Object.entries(rawManifest.skins).map(compileSkin),
) as Record<PetSkinName, PetSkinConfig>;

// Keep model packages registered for the archived TODO path, but do not expose
// them in the user-facing selector until the model/material/action contract is
// production-ready. Old saved model values can therefore be recognized and
// safely migrated by the UI without deleting the assets or renderer.
export const PET_SKIN_NAMES = Object.entries(PET_SKINS)
  .filter(([, config]) => config.render !== "model")
  .map(([name]) => name) as PetSkinName[];
export const PET_MODEL_SKIN_NAMES = Object.entries(PET_SKINS)
  .filter(([, config]) => config.render === "model")
  .map(([name]) => name) as PetSkinName[];

export function getPetSkinConfig(skin: string): PetSkinConfig | undefined {
  return PET_SKINS[skin as PetSkinName];
}

export function isPetSkinName(skin: string): skin is PetSkinName {
  return PET_SKIN_NAMES.includes(skin as PetSkinName);
}

export function isModelSkinConfig(config: PetSkinConfig): config is ModelPetSkinConfig {
  return config.render === "model";
}
