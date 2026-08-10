declare module "@novnc/novnc" {
  export default class RFB {
    constructor(
      target: HTMLElement,
      url: string,
      options?: { credentials?: { password?: string } },
    );
    scaleViewport: boolean;
    addEventListener(type: string, listener: (event: Event) => void): void;
    sendCredentials(credentials: { password?: string }): void;
    disconnect(): void;
  }
}
