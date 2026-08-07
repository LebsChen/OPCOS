declare module "@novnc/novnc" {
  export default class RFB {
    constructor(target: HTMLElement, url: string);
    scaleViewport: boolean;
    disconnect(): void;
  }
}
