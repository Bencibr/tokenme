import { useCallback, useState } from "react";
import { PetSkin } from "./PetSkin";
import { PetNoticeCard } from "./PetNoticeApp";
import { isPetSkinName, PET_SKIN_NAMES, getPetSkinConfig } from "../lib/petSkinRegistry";
import { WaterdropPet } from "./WaterdropPet";
import { t } from "../lib/i18n";
import { noticeMotion } from "../lib/petCompanion";
import type { PetNoticeKind, PetMotion } from "../lib/petCompanion";

/** Browser-only visual QA, following the panel's existing query-string pins. */
export function PetPreview() {
  const query = new URLSearchParams(location.search);
  const selected = query.get("pet-preview") ?? "kitten";
  const kind = query.get("kind") as PetNoticeKind;
  const initial = ["morning","noon","evening","night","rest","hello"].includes(kind) ? kind : "morning";
  const [notice, setNotice] = useState<PetNoticeKind>(initial);
  const [motion, setMotion] = useState<PetMotion>(noticeMotion(initial));
  const [assetError, setAssetError] = useState("");
  const onAssetError = useCallback((error?:unknown) => setAssetError(error instanceof Error ? error.message : "asset unavailable"), []);
  if (selected === "all") return <div className="pet-preview-gallery" data-asset-error={assetError}>
    <span /><span className="pet-preview-heading">{t("pet.preview.greet")}</span><span className="pet-preview-heading">{t("pet.preview.care")}</span>
    <span className="pet-preview-heading">{t("pet.preview.sleepy")}</span><span className="pet-preview-heading">{t("pet.preview.stretch")}</span>
    {["waterdrop", ...PET_SKIN_NAMES].map((skin) => <GalleryRow key={skin} skin={skin} onAssetError={onAssetError} />)}
  </div>;
  if (selected !== "waterdrop" && !isPetSkinName(selected)) return null;
  return <div className="pet-preview-stage" data-asset-error={assetError}>
    {selected === "waterdrop" ? <div className="bubble-app pet-preview-waterdrop" data-dock="none" data-emotion={motion} data-action={motion} data-animations={query.get("motion") !== "off"}>
      <WaterdropPet tokens={12345} gaze={null} />
    </div> : <PetSkin skin={selected} dock={null} hovered={false} dragging={false} tokens={12345} gaze={null}
      action={motion} motions={query.get("motion") !== "off"} onAssetError={onAssetError} />}
    <PetNoticeCard notice={{ id:1, kind:notice, minutes:60 }} onResponse={(action) => setMotion(action === "break" ? "sleep" : "cheer")} />
    <div className="pet-preview-picks">{(["morning","noon","evening","night","rest"] as PetNoticeKind[]).map((kind) =>
      <button key={kind} type="button" onClick={() => { setNotice(kind); setMotion(noticeMotion(kind)); }}>{kind}</button>)}</div>
  </div>;
}

function GalleryRow({ skin, onAssetError }: { skin:string; onAssetError:(error?:unknown) => void }) {
  const label = skin === "waterdrop" ? t("set.pet.skin.waterdrop") : t(getPetSkinConfig(skin)!.labelKey as Parameters<typeof t>[0]);
  return <>
    <span className="pet-preview-heading">{label}</span>
    {(["hello","care","sleep","stretch"] as const).map((tone) => <div className="pet-preview-cell" key={tone}>
      {skin === "waterdrop" ? <div className="bubble-app pet-preview-waterdrop" data-dock="none" data-emotion={tone} data-action={tone} data-animations="false">
        <WaterdropPet tokens={12345} gaze={null} />
      </div> : isPetSkinName(skin) && <PetSkin skin={skin} dock={null} hovered={false} dragging={false} tokens={12345} gaze={null}
        emotion={tone} motions={false} onAssetError={onAssetError} />}
    </div>)}
  </>;
}
