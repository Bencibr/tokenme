import { useEffect } from "react";
import kittenAtlas from "../assets/pets/kitten-sprites.png";
import kittenSide from "../assets/pets/kitten-edge-side.png";
import kittenLeft from "../assets/pets/kitten-edge-left.png";
import kittenTop from "../assets/pets/kitten-edge-top.png";
import { ballTokens } from "../lib/format";
import { t } from "../lib/i18n";

/** Four aligned alpha tiles: open eyes, half blink, closed blink, happy wave.
 * Skin changes only the page renderer. The existing native bubble stays alive. */
export function PetSkin({ dock, hovered, dragging, tokens, gaze, onAssetError }: {
  dock: "left" | "right" | "top" | null;
  hovered: boolean;
  dragging: boolean;
  tokens: number;
  gaze: { x: number; y: number } | null;
  onAssetError: () => void;
}) {
  useEffect(() => {
    const images = [kittenAtlas, kittenSide, kittenLeft, kittenTop].map((src) => {
      const image = new Image();
      image.onerror = onAssetError;
      image.src = src;
      return image;
    });
    return () => { images.forEach((image) => { image.onerror = null; }); };
  }, [onAssetError]);

  return (
    <span className="pet-skin" data-dock={dock ?? "none"}
      data-motion={dragging ? "drag" : hovered ? "happy" : "idle"} aria-hidden="true">
      <span className="pet-pose">
        <span className="pet-art">
          {dock ? <img className="pet-edge-art" src={dock === "top" ? kittenTop : dock === "left" ? kittenLeft : kittenSide} alt="" draggable={false} />
            : <span className="pet-sprite" style={{ backgroundImage: `url(${kittenAtlas})` }} />}
          <span className="pet-eyes" style={{
            "--pet-gx": `${(gaze?.x ?? 0) * .65}px`,
            "--pet-gy": `${(gaze?.y ?? (dock === "top" ? 1.2 : 0)) * .65}px`,
          } as React.CSSProperties}>
            <span className="pet-eye"><span className="pet-pupil" /><span className="pet-eyelid" /></span>
            <span className="pet-eye"><span className="pet-pupil" /><span className="pet-eyelid" /></span>
          </span>
        </span>
      </span>
      <span className="pet-token-badge">
        <strong>{ballTokens(tokens)}</strong>
        <small>{t("bubble.today")} tokens</small>
      </span>
    </span>
  );
}
