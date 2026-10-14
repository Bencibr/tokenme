declare module "jsqr" {
  interface QRCodeResult {
    data: string;
  }

  interface QRCodeOptions {
    inversionAttempts?: "dontInvert" | "onlyInvert" | "attemptBoth" | "invertFirst";
  }

  export default function jsQR(data: Uint8ClampedArray, width: number, height: number, options?: QRCodeOptions): QRCodeResult | null;
}
