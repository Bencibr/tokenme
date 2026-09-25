import { createContext, useContext } from "react";

/**
 * Display preferences the whole panel reads. `money` gates the converted
 * API-dollar figures: they are an equal-value yardstick across models, not a
 * bill (subscribers pay a fixed plan), so the settings sheet lets users turn
 * them off and see pure tokens/credits.
 */
export const DisplayCtx = createContext({ money: true });

export function useMoney(): boolean {
  return useContext(DisplayCtx).money;
}
