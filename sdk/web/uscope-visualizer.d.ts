// The API a uscope renderer runs against (docs/visualizers.md). A renderer
// is one classic script that calls `uscope.draw` or `uscope.live` once; the
// only global it can use beyond the language and drawing is `uscope`.
//
// To have an editor check a renderer, start it with `// @ts-check` and
// point the editor at this file, as with `/// <reference path="…" />`.

declare namespace uscope {
  /** A value as a renderer receives it, decided by its program type alone. */
  type Value =
    | null
    | boolean
    | number
    | bigint
    | string
    | Value[]
    | Int8Array
    | Uint8Array
    | Int16Array
    | Uint16Array
    | Int32Array
    | Uint32Array
    | BigInt64Array
    | BigUint64Array
    | Float32Array
    | Float64Array
    | { [member: string]: Value };

  /** An enumeration: the enumerator it equals, or null, and its number. */
  interface Enumerator {
    name: string | null;
    value: number | bigint;
  }

  /** A sum type's variant: its name, and its payload when it has one. */
  interface Variant {
    variant: string;
    value?: Value;
  }

  /** A drawing's inputs, by the names its `visualize` gives them. */
  type Inputs = Record<string, any>;

  interface Context<I = Inputs> {
    /** The inputs this tab drew for the same value at its previous stop. */
    previous: I | null;
    /** CSS pixels the card offers; a picture is scaled to fit. */
    width: number;
    theme: "light" | "dark";
  }

  /** A color: `#rgb`, `#rrggbb`, `#rrggbbaa`, `rgb()`, `hsl()`, or a CSS name. */
  type Color = string;

  /** What every shape may also say. */
  interface Style {
    fill?: Color;
    stroke?: Color;
    strokeWidth?: number;
    opacity?: number;
    /** Dash and gap lengths, as `stroke-dasharray`. */
    dash?: number[];
    /** Hover text, and the shape's accessible name. */
    title?: string;
    /** A part of the drawn value, `member`, `[3]`, or `a.b[2].c`, which a
     * click opens as a row. */
    select?: string;
  }

  interface Rect extends Style {
    x: number;
    y: number;
    width: number;
    height: number;
    radius?: number;
  }
  interface Circle extends Style {
    x: number;
    y: number;
    r: number;
  }
  interface Line extends Style {
    x1: number;
    y1: number;
    x2: number;
    y2: number;
  }
  interface Points extends Style {
    /** x0, y0, x1, y1, …, as an array or a Float32Array or Float64Array. */
    points: ArrayLike<number>;
  }
  interface Path extends Style {
    /** SVG path data. */
    d: string;
  }
  interface Text extends Style {
    x: number;
    y: number;
    text: string;
    size?: number;
    weight?: "normal" | "bold" | number;
    family?: "sans" | "mono";
    anchor?: "start" | "middle" | "end";
    baseline?: "auto" | "alphabetic" | "middle" | "central" | "hanging" | "ideographic";
  }
  interface Group extends Style {
    shapes: Shape[];
    x?: number;
    y?: number;
    scale?: number;
    /** Degrees, clockwise. */
    rotate?: number;
  }
  interface Pixels extends Style {
    x: number;
    y: number;
    width: number;
    height: number;
    /** RGBA, four bytes a pixel, row by row. */
    pixels: Uint8ClampedArray | Uint8Array;
    columns: number;
    rows: number;
    /** Whether scaling blends pixels; they stay sharp unless so. */
    smooth?: boolean;
  }
  interface Canvas extends Style {
    x: number;
    y: number;
    width: number;
    height: number;
    /** A canvas the renderer drew, which the page receives as only pixels. */
    canvas: OffscreenCanvas;
    smooth?: boolean;
  }

  /** A shape, as the functions below make them. */
  interface Shape {
    readonly type: string;
  }

  interface Picture {
    readonly type: "picture";
  }

  /** The page's colors in its current theme. */
  interface Theme {
    ink: string;
    ink2: string;
    ink3: string;
    paper: string;
    surface: string;
    line: string;
    accent: string;
    changed: string;
    good: string;
    bad: string;
    /** Eight categorical colors, in their validated order. */
    series: readonly string[];
  }

  /** Draws each stop's inputs as a picture. */
  function draw<I = Inputs>(
    render: (input: I, context: Context<I>) => Picture | Promise<Picture>,
  ): void;

  function picture(picture: {
    width: number;
    height: number;
    shapes: Shape[];
    caption?: string;
  }): Picture;
  function rect(rect: Rect): Shape;
  function circle(circle: Circle): Shape;
  function line(line: Line): Shape;
  function polyline(polyline: Points): Shape;
  function polygon(polygon: Points): Shape;
  function path(path: Path): Shape;
  function text(text: Text): Shape;
  function group(group: Group): Shape;
  function image(image: Pixels | Canvas): Shape;

  /** The page's colors, in light or dark as the page shows. */
  const theme: Theme;
  /** Deep equality over input values, for marking what changed. */
  function same(a: unknown, b: unknown): boolean;
  const color: {
    /** The color `t` of the way from `from` to `to` (#rgb or #rrggbb). */
    scale(t: number, from: string, to: string): string;
  };
}
