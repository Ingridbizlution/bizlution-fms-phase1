declare module "@tabler/core/dist/js/tabler.esm.min.js" {
  export class Tooltip {
    constructor(element: Element, options?: Record<string, unknown>);
    dispose(): void;
    static getInstance(element: Element): Tooltip | null;
  }
  export class Collapse {
    constructor(element: Element, options?: Record<string, unknown>);
    hide(): void;
    show(): void;
    static getOrCreateInstance(element: Element, config?: Record<string, unknown>): Collapse;
  }
}
