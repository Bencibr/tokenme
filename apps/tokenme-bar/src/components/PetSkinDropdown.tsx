import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import type { BubbleSkin } from "../types";
import { bridge, isWindows } from "../lib/bridge";
import { t } from "../lib/i18n";
import { IconChevron } from "./Icons";

const SKINS: BubbleSkin[] = ["waterdrop", "kitten"];
const label = (skin: BubbleSkin) => skin === "waterdrop" ? t("set.bubble.skin.waterdrop") : t("set.bubble.skin.kitten");

export function PetSkinDropdown({ value, onChange, disabled = false }: {
  value: BubbleSkin;
  onChange: (skin: BubbleSkin) => void;
  disabled?: boolean;
}) {
  const trigger = useRef<HTMLButtonElement | null>(null);
  const menu = useRef<HTMLDivElement | null>(null);
  const keyboardBorrowed = useRef(false);
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState<BubbleSkin>(value);
  const [position, setPosition] = useState({ left: 0, top: 0, width: 180, maxHeight: 120 });

  const keyboard = useCallback((on: boolean) => {
    if (!isWindows || keyboardBorrowed.current === on) return;
    keyboardBorrowed.current = on;
    void bridge.setKeyboardMode(on).then(() => {
      if (on && keyboardBorrowed.current) trigger.current?.focus({ preventScroll: true });
    }).catch((e) => console.error("pet skin keyboard mode:", e));
  }, []);

  const close = useCallback(() => {
    setOpen(false);
    keyboard(false);
  }, [keyboard]);

  const show = (first = value) => {
    if (disabled) return;
    setActive(first);
    setOpen(true);
    keyboard(true);
    trigger.current?.focus({ preventScroll: true });
  };

  const pick = useCallback((skin: BubbleSkin) => {
    close();
    if (!disabled) onChange(skin);
  }, [close, disabled, onChange]);

  useEffect(() => () => keyboard(false), [keyboard]);
  useEffect(() => { if (disabled) close(); }, [disabled, close]);

  // The portal escapes sheet-body overflow; keep its viewport anchor in sync.
  useLayoutEffect(() => {
    if (!open) return;
    const place = () => {
      if (!trigger.current || !menu.current) return;
      const rect = trigger.current.getBoundingClientRect();
      const inset = 8, gap = 6;
      const width = Math.min(Math.max(rect.width, 180), window.innerWidth - inset * 2);
      const height = menu.current.scrollHeight + 2;
      const below = window.innerHeight - rect.bottom - gap - inset;
      const above = rect.top - gap - inset;
      const upwards = height > below && above > below;
      const maxHeight = Math.max(0, upwards ? above : below);
      setPosition({
        left: Math.max(inset, Math.min(rect.right - width, window.innerWidth - width - inset)),
        top: Math.max(inset, upwards ? rect.top - gap - Math.min(height, maxHeight) : rect.bottom + gap),
        width,
        maxHeight,
      });
    };
    place();
    window.addEventListener("resize", place);
    window.addEventListener("scroll", close, true);
    return () => {
      window.removeEventListener("resize", place);
      window.removeEventListener("scroll", close, true);
    };
  }, [open, close]);

  useEffect(() => {
    if (!open) return;
    const onDown = (e: PointerEvent) => {
      if (!trigger.current?.contains(e.target as Node) && !menu.current?.contains(e.target as Node)) close();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Tab") { close(); return; }
      if (!["Escape", "ArrowDown", "ArrowUp", "Home", "End", "Enter", " "].includes(e.key)) return;
      e.preventDefault();
      // Capture before App's Escape listener: the settings sheet must stay open.
      e.stopPropagation();
      if (e.key === "Escape") close();
      else if (e.key === "Enter" || e.key === " ") pick(active);
      else if (e.key === "Home") setActive(SKINS[0]);
      else if (e.key === "End") setActive(SKINS[SKINS.length - 1]);
      else setActive((skin) => SKINS[(SKINS.indexOf(skin) + (e.key === "ArrowDown" ? 1 : SKINS.length - 1)) % SKINS.length]);
    };
    window.addEventListener("pointerdown", onDown, true);
    window.addEventListener("keydown", onKey, true);
    window.addEventListener("blur", close);
    return () => {
      window.removeEventListener("pointerdown", onDown, true);
      window.removeEventListener("keydown", onKey, true);
      window.removeEventListener("blur", close);
    };
  }, [open, active, close, pick]);

  return (
    <>
      <button
        ref={trigger}
        type="button"
        id="bubble-skin"
        role="combobox"
        className="scope-btn pet-skin-btn"
        data-value={value}
        data-open={open || undefined}
        aria-label={t("set.bubble.skin")}
        aria-describedby="bubble-skin-hint"
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-controls={open ? "bubble-skin-options" : undefined}
        aria-activedescendant={open ? `bubble-skin-option-${active}` : undefined}
        disabled={disabled}
        onFocus={() => keyboard(true)}
        onBlur={(e) => {
          if (!menu.current?.contains(e.relatedTarget as Node)) close();
        }}
        onClick={() => open ? close() : show()}
        onKeyDown={(e) => {
          if (open || !["ArrowDown", "ArrowUp", "Enter", " "].includes(e.key)) return;
          e.preventDefault();
          e.stopPropagation();
          show();
        }}
      >
        <span>{label(value)}</span>
        <IconChevron size={9} className="scope-chev" />
      </button>
      {open ? createPortal(
        <div
          ref={menu}
          id="bubble-skin-options"
          className="scope-menu pet-skin-menu"
          role="listbox"
          aria-label={t("set.bubble.skin")}
          style={position}
          onClick={(e) => e.stopPropagation()}
        >
          {SKINS.map((skin) => (
            <button
              key={skin}
              type="button"
              id={`bubble-skin-option-${skin}`}
              role="option"
              className="scope-item"
              data-value={skin}
              data-active={active === skin || undefined}
              aria-selected={value === skin}
              tabIndex={-1}
              onPointerDown={(e) => e.preventDefault()}
              onPointerMove={() => setActive(skin)}
              onClick={() => pick(skin)}
            >
              <span className="si-check" aria-hidden="true">{value === skin ? "✓" : ""}</span>
              <span className="si-name">{label(skin)}</span>
            </button>
          ))}
        </div>,
        document.body,
      ) : null}
    </>
  );
}
