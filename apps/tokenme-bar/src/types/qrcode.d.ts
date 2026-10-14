declare module "qrcode" {
  interface QRCodeOptions {
    errorCorrectionLevel?: "L" | "M" | "Q" | "H";
    margin?: number;
    width?: number;
  }

  const QRCode: { toDataURL: (text: string, options?: QRCodeOptions) => Promise<string> };
  export default QRCode;
}
