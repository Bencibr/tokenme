import { useEffect, useState } from "react";
import type { CSSProperties } from "react";
import {
  getPetSkinConfig,
  type PetArtworkCrop,
  type PetDock,
  type RasterPetSkinConfig,
  type PetSkinName,
} from "../lib/petSkinRegistry";
import { ballTokens } from "../lib/format";
import { t } from "../lib/i18n";
import type { PetMotion } from "../lib/petCompanion";

export type { PetSkinName } from "../lib/petSkinRegistry";

type PetStyle = CSSProperties & Record<`--${string}`, string>;

const px = (value: number) => `${value}px`;
const point = ([x, y]: [number, number]) => `${px(x)} ${px(y)}`;
const frameClip = ([x, y]: [number, number], index: number, cell: number) => {
  const left = x + (index % 2) * cell;
  const top = y + Math.floor(index / 2) * cell;
  return `inset(${px(Math.max(0, top))} ${px(Math.max(0, -left))} ${px(Math.max(0, -top))} ${px(Math.max(0, left))})`;
};

/** Translate one manifest entry into the CSS variables consumed by the renderer. */
function skinStyle(config: RasterPetSkinConfig, dock: PetDock | null, happy: boolean): PetStyle {
  const pose = dock ?? "none";
  const eyes = happy && pose === "none" ? config.eyes.happy ?? config.eyes.none : config.eyes[pose];
  return {
    "--pet-layout-size": px(config.layout.content),
    "--pet-art-size": px(config.layout.art),
    "--pet-atlas-size": px(config.layout.atlas),
    "--pet-art-offset": px(config.layout.artOffset),
    "--pet-dock-right-shift": px(config.layout.dockShift.right),
    "--pet-dock-left-shift": px(config.layout.dockShift.left),
    "--pet-dock-top-shift": px(config.layout.dockShift.top),
    "--pet-blink-open": point(config.frames.open),
    "--pet-blink-half": point(config.frames.half),
    "--pet-blink-closed": point(config.frames.closed),
    "--pet-happy-position": point(config.frames.happy),
    "--pet-open-clip": frameClip(config.frames.open, 0, config.layout.art),
    "--pet-half-clip": frameClip(config.frames.half, 1, config.layout.art),
    "--pet-closed-clip": frameClip(config.frames.closed, 2, config.layout.art),
    "--pet-happy-clip": frameClip(config.frames.happy, 3, config.layout.art),
    "--pet-eye-lx": px(eyes.left[0]),
    "--pet-eye-ly": px(eyes.left[1]),
    "--pet-eye-rx": px(eyes.right[0]),
    "--pet-eye-ry": px(eyes.right[1]),
    "--pet-eye-size": px(eyes.size),
    "--pet-eye-height": px(eyes.height),
    "--pet-pupil-size": px(eyes.pupil),
    "--pet-eye-left-angle": `${eyes.rotation?.[0] ?? 0}deg`,
    "--pet-eye-right-angle": `${eyes.rotation?.[1] ?? 0}deg`,
    "--pet-eyelid-fill": config.eyelid.fill,
    "--pet-eyelid-stroke": config.eyelid.stroke,
    "--pet-eyelid-width": px(config.eyelid.strokeWidth ?? 2),
  };
}

/**
 * The renderer is deliberately data-blind: all asset paths, frame offsets,
 * layout dimensions and eye anchors come from the selected JSON skin package.
 */
type PetSkinProps = {
  skin: PetSkinName;
  dock: "left" | "right" | "top" | null;
  hovered: boolean;
  dragging: boolean;
  tokens: number;
  gaze: { x: number; y: number } | null;
  onAssetError: (error?: unknown) => void;
  action?: PetMotion;
  motions?: boolean;
  emotion?: PetMotion;
  crop?: PetArtworkCrop;
  centerInWindow?: boolean;
};

export function PetSkin(props: PetSkinProps) {
  // The archived model renderer stays in source but has no import here, so
  // its Three.js chunk and model assets cannot enter the default bundle.
  return <RasterPetSkin {...props} />;
}

function RasterPetSkin({ skin, dock, hovered, dragging, tokens, gaze, onAssetError, action = "idle", emotion = "idle", motions = true, crop, centerInWindow = false }: PetSkinProps) {
  const raster = getPetSkinConfig(skin);
  const assets = raster?.assets;
  const [decodedAssets, setDecodedAssets] = useState<typeof assets>();

  useEffect(() => {
    if (!assets) return;
    let alive = true;
    const loading = [assets.atlas, assets.side, assets.left, assets.top, ...(raster?.reactionAtlas ? [raster.reactionAtlas.file] : [])].map((src) => new Promise<void>((resolve, reject) => {
      const image = new Image();
      image.onload = () => {
        // All files must load, but a deferred GPU decode must not keep a
        // hidden utility surface invisible. Warm the decoder without a gate.
        void image.decode().catch(() => {});
        resolve();
      };
      image.onerror = () => reject(new Error("pet asset unavailable"));
      image.src = src;
    }));
    // Wait for the complete skin package; ignore replaced-skin completions.
    void Promise.all(loading).then(() => {
      if (alive) setDecodedAssets(assets);
    }).catch((error) => {
      if (alive) onAssetError(error);
    });
    return () => { alive = false; };
  }, [assets, raster, onAssetError]);

  if (!raster || !assets) return null;

  const pose = dock ?? "none";
  // Comparing the asset identity hides the new skin in its first render,
  // before the effect runs, even when React reuses this component.
  const ready = decodedAssets === assets;
  const tone = emotion === "idle" ? action : emotion;
  const happy = !dragging && (hovered || tone === "hello" || tone === "cheer");
  const eyes = happy && !dock ? raster.eyes.happy ?? raster.eyes.none : raster.eyes[pose];
  const activeCrop = !dock ? crop : undefined;
  const style = {
    ...skinStyle(raster, dock, happy),
    ...(centerInWindow && activeCrop ? {
      // Use this skin's manifest crop, with the original sprite layout
      // translated inside the smaller viewport below.
      position: "absolute" as const,
      width: px(activeCrop.width),
      height: px(activeCrop.height),
      left: `calc(50% - ${activeCrop.width / 2}px)`,
      top: `calc(50% - ${activeCrop.height / 2}px)`,
    } : centerInWindow ? {
      position: "absolute" as const,
      left: `calc(50% - ${raster.layout.content / 2}px)`,
      top: `calc(50% - ${raster.layout.content / 2}px)`,
    } : {}),
  };
  const poseStyle: CSSProperties | undefined = activeCrop ? {
    inset: "auto",
    right: "auto",
    bottom: "auto",
    width: px(raster.layout.content),
    height: px(raster.layout.content),
    left: px(-activeCrop.left),
    top: px(-activeCrop.top),
  } : undefined;
  const reaction = !dock && raster.reactionAtlas;
  const reactionKey = tone === "cheer" ? "hello" : tone;
  const reactionFrame = reaction && reactionKey in reaction.frames
    ? reaction.frames[reactionKey as keyof typeof reaction.frames] : null;
  // The long-legged adult skin has smaller eyes than the chibi characters.
  // Limit pupil travel to its socket rather than clipping it at a fixed reach.
  const pupilOffset = (offset: number, size: number) => {
    const reach = Math.max(0, (size - eyes.pupil) / 2);
    return px(Math.max(-reach, Math.min(reach, offset * .65)));
  };
  const eyeStyle = {
    "--pet-gx": pupilOffset(gaze?.x ?? 0, eyes.size),
    "--pet-gy": pupilOffset(gaze?.y ?? (pose === "top" ? 1.2 : 0), eyes.height),
  } as CSSProperties;

  return (
    <span className="pet-skin" data-skin={skin} data-dock={pose}
      data-motion={dragging ? "drag" : happy ? "happy" : "idle"}
      data-blink={raster.animation?.blink ?? "overlay"}
      data-ready={ready}
      data-action={dragging ? "idle" : action}
      data-cropped={Boolean(activeCrop)}
      data-animations={motions}
      data-reaction={Boolean(reactionFrame)}
      data-emotion={tone}
      style={style} aria-hidden="true">
      <span className="pet-pose" style={poseStyle}>
        <span className="pet-art">
          {dock ? <img key={dock} className="pet-edge-art" src={dock === "top" ? assets.top : dock === "left" ? assets.left : assets.side} alt="" draggable={false} />
            : <span className="pet-sprite" style={reactionFrame && reaction ? {
              backgroundImage:`url(${reaction.file})`, backgroundPosition:point(reactionFrame),
              backgroundSize:px(raster.layout.art * 2), clipPath:"none", animation:"none",
            } : { backgroundImage: `url(${assets.atlas})` }} />}
          {ready && !reactionFrame && eyes.enabled !== false && <span className="pet-eyes" data-tracking={eyes.tracking ?? "pupil"} style={eyeStyle}>
            <span className="pet-eye"><span className="pet-pupil" /><span className="pet-eyelid" /></span>
            <span className="pet-eye"><span className="pet-pupil" /><span className="pet-eyelid" /></span>
          </span>}
        </span>
      </span>
      <span className="pet-token-badge" style={activeCrop ? { top: px(activeCrop.height + 4) } : undefined}>
        <strong>{ballTokens(tokens)}</strong>
        <small>{t("bubble.today")} tokens</small>
      </span>
    </span>
  );
}
