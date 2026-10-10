// Builds a checked picture as SVG, element by element: every attribute is
// one the builder names, and text goes in as text, never as markup. Images
// are canvases the page paints.

import type { Picture, Shape, Style } from "./picture";

const SVG = "http://www.w3.org/2000/svg";
const HTML = "http://www.w3.org/1999/xhtml";

/** What clicking or pressing Enter on a shape with `select` does. */
export type OnSelect = (path: string) => void;

/** The picture as an `<svg>`, scaled to fit its container and never cropped. */
export function buildSvg(picture: Picture, onSelect?: OnSelect): SVGSVGElement {
  const svg = document.createElementNS(SVG, "svg");
  svg.setAttribute("viewBox", `0 0 ${picture.width} ${picture.height}`);
  svg.setAttribute("width", String(picture.width));
  svg.setAttribute("height", String(picture.height));
  svg.setAttribute("role", "img");
  if (picture.caption !== undefined) {
    svg.setAttribute("aria-label", picture.caption);
  }
  svg.classList.add("picture");
  // Shapes inherit the page's ink unless they say otherwise.
  svg.style.fill = "var(--ink)";
  svg.style.stroke = "none";
  for (const shape of picture.shapes) {
    svg.append(element(shape, onSelect));
  }
  return svg;
}

function element(shape: Shape, onSelect: OnSelect | undefined): SVGElement {
  let node: SVGElement;
  switch (shape.type) {
    case "rect":
      node = make("rect", {
        x: shape.x,
        y: shape.y,
        width: shape.width,
        height: shape.height,
        rx: shape.radius,
        ry: shape.radius,
      });
      break;
    case "circle":
      node = make("circle", { cx: shape.x, cy: shape.y, r: shape.r });
      break;
    case "line":
      node = make("line", { x1: shape.x1, y1: shape.y1, x2: shape.x2, y2: shape.y2 });
      outline(node, shape);
      break;
    case "polyline":
    case "polygon":
      node = make(shape.type, { points: Array.prototype.join.call(shape.points, " ") });
      if (shape.type === "polyline") {
        outline(node, shape);
      }
      break;
    case "path":
      node = make("path", { d: shape.d });
      break;
    case "text":
      node = make("text", {
        x: shape.x,
        y: shape.y,
        "font-size": shape.size,
        "font-weight": shape.weight,
        "text-anchor": shape.anchor,
        "dominant-baseline": shape.baseline,
      });
      node.style.fontFamily = shape.family === "mono" ? "var(--mono)" : "var(--sans)";
      node.textContent = shape.text;
      break;
    case "group": {
      const transform = [
        shape.x !== undefined || shape.y !== undefined
          ? `translate(${shape.x ?? 0} ${shape.y ?? 0})`
          : "",
        shape.rotate !== undefined ? `rotate(${shape.rotate})` : "",
        shape.scale !== undefined ? `scale(${shape.scale})` : "",
      ]
        .filter(Boolean)
        .join(" ");
      node = make("g", { transform: transform || undefined });
      for (const child of shape.shapes) {
        node.append(element(child, onSelect));
      }
      break;
    }
    case "image":
      node = image(shape);
      break;
  }
  styled(node, shape);
  if (shape.title !== undefined) {
    const title = document.createElementNS(SVG, "title");
    title.textContent = shape.title;
    node.prepend(title);
  }
  if (shape.select !== undefined && onSelect !== undefined) {
    const path = shape.select;
    node.setAttribute("tabindex", "0");
    node.setAttribute("role", "button");
    node.classList.add("selectable");
    node.addEventListener("click", () => onSelect(path));
    node.addEventListener("keydown", (event) => {
      if ((event as KeyboardEvent).key === "Enter") {
        onSelect(path);
      }
    });
  }
  return node;
}

function make(name: string, attributes: Record<string, string | number | undefined>): SVGElement {
  const node = document.createElementNS(SVG, name);
  for (const [attribute, value] of Object.entries(attributes)) {
    if (value !== undefined) {
      node.setAttribute(attribute, String(value));
    }
  }
  return node;
}

/** Lines are drawn by their stroke, in ink unless they say otherwise. */
function outline(node: SVGElement, shape: Style): void {
  node.setAttribute("fill", "none");
  if (shape.stroke === undefined) {
    node.style.stroke = "var(--ink)";
  }
}

function styled(node: SVGElement, shape: Style): void {
  const attributes: Record<string, string | number | undefined> = {
    fill: shape.fill,
    stroke: shape.stroke,
    "stroke-width": shape.strokeWidth,
    opacity: shape.opacity,
    "stroke-dasharray": shape.dash?.join(" "),
  };
  for (const [attribute, value] of Object.entries(attributes)) {
    if (value !== undefined) {
      node.setAttribute(attribute, String(value));
    }
  }
}

function image(shape: Extract<Shape, { type: "image" }>): SVGElement {
  const holder = make("foreignObject", {
    x: shape.x,
    y: shape.y,
    width: shape.width,
    height: shape.height,
  });
  const canvas = document.createElementNS(HTML, "canvas") as HTMLCanvasElement;
  canvas.style.display = "block";
  canvas.style.width = "100%";
  canvas.style.height = "100%";
  canvas.style.imageRendering = shape.smooth === true ? "auto" : "pixelated";
  if ("bitmap" in shape) {
    canvas.width = shape.bitmap.width;
    canvas.height = shape.bitmap.height;
    canvas.getContext("2d")?.drawImage(shape.bitmap, 0, 0);
  } else {
    canvas.width = shape.columns;
    canvas.height = shape.rows;
    const pixels = new Uint8ClampedArray(shape.pixels);
    canvas.getContext("2d")?.putImageData(new ImageData(pixels, shape.columns, shape.rows), 0, 0);
  }
  holder.append(canvas);
  return holder;
}
