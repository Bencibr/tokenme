import type { CSSProperties } from "react";
import { ballTokens } from "../lib/format";
import { t } from "../lib/i18n";

/** The classic pet keeps its LED face as code-native artwork. */
export function WaterdropPet({ tokens, gaze }: { tokens: number; gaze: { x:number; y:number } | null }) {
  return <>
    <span className="bubble-ripple" aria-hidden="true" />
    <span className="bubble-core">
      <span className="bubble-value">{ballTokens(tokens)}</span>
      <span className="bubble-unit">{t("bubble.today")} tokens</span>
      <span className="bubble-face" aria-hidden="true"
        style={gaze ? ({ "--gx":`${gaze.x}px`, "--gy":`${gaze.y}px` } as CSSProperties) : undefined}>
        <span className="bubble-eye"><span className="bubble-pupil" /></span>
        <span className="bubble-eye"><span className="bubble-pupil" /></span>
        <span className="bubble-mouth" />
        <span className="bubble-cheek bubble-cheek-left" />
        <span className="bubble-cheek bubble-cheek-right" />
      </span>
    </span>
  </>;
}
