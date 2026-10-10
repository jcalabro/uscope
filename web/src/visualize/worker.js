// The worker a renderer runs in, started by the sandboxed frame. It takes
// what it needs from the global scope, deletes every global that is not on
// its list, and only then evaluates the renderer's source, which so reaches
// nothing but the language, drawing, and `uscope`. Pictures go back to the
// frame as plain data; the page checks every shape before showing one.
//
// This file is a classic script, never a module: Chromium refuses a module
// worker from a blob in an opaque-origin frame.

(() => {
  // Taken before the lockdown, which deletes them from the global scope.
  const post = self.postMessage.bind(self);
  const listen = self.addEventListener.bind(self);
  const evaluate = self.eval;
  const { defineProperty, freeze, getOwnPropertyDescriptor, getPrototypeOf, keys } = Object;
  const { deleteProperty, ownKeys } = Reflect;

  // The globals a renderer keeps: the language, timers, and drawing.
  const KEEP = new Set([
    "AggregateError",
    "Array",
    "ArrayBuffer",
    "AsyncDisposableStack",
    "BigInt",
    "BigInt64Array",
    "BigUint64Array",
    "Boolean",
    "DataView",
    "Date",
    "DisposableStack",
    "Error",
    "EvalError",
    "FinalizationRegistry",
    "Float16Array",
    "Float32Array",
    "Float64Array",
    "Function",
    "Infinity",
    "Int16Array",
    "Int32Array",
    "Int8Array",
    "Intl",
    "Iterator",
    "JSON",
    "Map",
    "Math",
    "NaN",
    "Number",
    "Object",
    "Promise",
    "Proxy",
    "RangeError",
    "ReferenceError",
    "Reflect",
    "RegExp",
    "Set",
    "String",
    "SuppressedError",
    "Symbol",
    "SyntaxError",
    "TypeError",
    "URIError",
    "Uint16Array",
    "Uint32Array",
    "Uint8Array",
    "Uint8ClampedArray",
    "WeakMap",
    "WeakRef",
    "WeakSet",
    "decodeURI",
    "decodeURIComponent",
    "encodeURI",
    "encodeURIComponent",
    "escape",
    "eval",
    "globalThis",
    "isFinite",
    "isNaN",
    "parseFloat",
    "parseInt",
    "undefined",
    "unescape",
    "clearInterval",
    "clearTimeout",
    "queueMicrotask",
    "setInterval",
    "setTimeout",
    "structuredClone",
    "console",
    "TextDecoder",
    "TextEncoder",
    "CanvasGradient",
    "CanvasPattern",
    "DOMMatrix",
    "DOMMatrixReadOnly",
    "DOMPoint",
    "DOMPointReadOnly",
    "DOMQuad",
    "DOMRect",
    "DOMRectReadOnly",
    "ImageBitmap",
    "ImageBitmapRenderingContext",
    "ImageData",
    "OffscreenCanvas",
    "OffscreenCanvasRenderingContext2D",
    "Path2D",
    "TextMetrics",
    "WebGL2RenderingContext",
    "WebGLActiveInfo",
    "WebGLBuffer",
    "WebGLContextEvent",
    "WebGLFramebuffer",
    "WebGLProgram",
    "WebGLQuery",
    "WebGLRenderbuffer",
    "WebGLRenderingContext",
    "WebGLSampler",
    "WebGLShader",
    "WebGLShaderPrecisionFormat",
    "WebGLSync",
    "WebGLTexture",
    "WebGLTransformFeedback",
    "WebGLUniformLocation",
    "WebGLVertexArrayObject",
    "uscope",
  ]);

  /** Deletes every name not kept from the global object and the objects it
   * inherits from, short of `Object.prototype`, and fails if one stays. */
  function lockDown() {
    let holder = globalThis;
    while (holder !== null && holder !== Object.prototype) {
      for (const name of ownKeys(holder)) {
        if (typeof name === "string" && KEEP.has(name)) {
          continue;
        }
        if (name === "constructor" && holder !== globalThis) {
          continue;
        }
        if (deleteProperty(holder, name)) {
          continue;
        }
        // A name that cannot be deleted may stay only as a plain number,
        // such as Chromium's TEMPORARY and PERSISTENT.
        const kept = getOwnPropertyDescriptor(holder, name);
        if (kept === undefined || typeof kept.value === "number") {
          continue;
        }
        throw new Error(`the sandbox cannot remove ${String(name)}`);
      }
      holder = getPrototypeOf(holder);
    }
  }

  /** The renderer's file and the line and column of an error in it. */
  function place(error, file) {
    const stack = typeof error?.stack === "string" ? error.stack : "";
    const escaped = file.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    const found = new RegExp(`${escaped}:(\\d+):(\\d+)`).exec(stack);
    if (found) {
      return { file, line: Number(found[1]), column: Number(found[2]) };
    }
    // Firefox says where a syntax error is, Chromium does not.
    if (typeof error?.lineNumber === "number") {
      return { file, line: error.lineNumber, column: (error.columnNumber ?? 0) + 1 };
    }
    return { file, line: null, column: null };
  }

  function failure(error, file) {
    let message;
    try {
      message =
        error instanceof Error ? `${error.name}: ${error.message}` : `threw ${String(error)}`;
    } catch {
      message = "threw a value that cannot be shown";
    }
    return { message, ...place(error, file) };
  }

  // The renderer API, `uscope`.
  let drawer = null;
  let theme = freeze({});

  const shape = (type) => (properties) => ({ ...properties, type });

  /** Deep equality over input values: numbers, bigints, strings, arrays,
   * typed arrays, and plain objects. NaN equals NaN. */
  function same(a, b) {
    if (Object.is(a, b)) {
      return true;
    }
    if (typeof a !== "object" || typeof b !== "object" || a === null || b === null) {
      return false;
    }
    if (ArrayBuffer.isView(a) || ArrayBuffer.isView(b)) {
      if (!ArrayBuffer.isView(a) || !ArrayBuffer.isView(b)) {
        return false;
      }
      if (getPrototypeOf(a) !== getPrototypeOf(b) || a.length !== b.length) {
        return false;
      }
      for (let i = 0; i < a.length; i++) {
        if (!Object.is(a[i], b[i])) {
          return false;
        }
      }
      return true;
    }
    if (Array.isArray(a) !== Array.isArray(b)) {
      return false;
    }
    const names = keys(a);
    if (names.length !== keys(b).length) {
      return false;
    }
    return names.every((name) => Object.hasOwn(b, name) && same(a[name], b[name]));
  }

  /** A color `#rgb` or `#rrggbb` as linear OKLab. */
  function oklab(color) {
    const hex = /^#([0-9a-f]{3}|[0-9a-f]{6})$/i.exec(color)?.[1];
    if (hex === undefined) {
      throw new TypeError(`uscope.color.scale takes #rgb or #rrggbb colors, not ${color}`);
    }
    const full = hex.length === 3 ? [...hex].map((digit) => digit + digit).join("") : hex;
    const [r, g, b] = [0, 2, 4].map((at) => {
      const value = Number.parseInt(full.slice(at, at + 2), 16) / 255;
      return value <= 0.04045 ? value / 12.92 : ((value + 0.055) / 1.055) ** 2.4;
    });
    const l = Math.cbrt(0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b);
    const m = Math.cbrt(0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b);
    const s = Math.cbrt(0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b);
    return [
      0.2104542553 * l + 0.793617785 * m - 0.0040720468 * s,
      1.9779984951 * l - 2.428592205 * m + 0.4505937099 * s,
      0.0259040371 * l + 0.7827717662 * m - 0.808675766 * s,
    ];
  }

  function hex([L, a, b]) {
    const l = (L + 0.3963377774 * a + 0.2158037573 * b) ** 3;
    const m = (L - 0.1055613458 * a - 0.0638541728 * b) ** 3;
    const s = (L - 0.0894841775 * a - 1.291485548 * b) ** 3;
    const linear = [
      4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
      -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
      -0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s,
    ];
    return `#${linear
      .map((value) => {
        const clamped = Math.min(1, Math.max(0, value));
        const encoded =
          clamped <= 0.0031308 ? clamped * 12.92 : 1.055 * clamped ** (1 / 2.4) - 0.055;
        return Math.round(encoded * 255)
          .toString(16)
          .padStart(2, "0");
      })
      .join("")}`;
  }

  /** The color `t` of the way from `from` to `to`, blended in OKLab. */
  function scale(t, from, to) {
    const amount = Number.isFinite(t) ? Math.min(1, Math.max(0, t)) : 0;
    const start = oklab(from);
    const end = oklab(to);
    return hex(start.map((value, index) => value + (end[index] - value) * amount));
  }

  /** An image of RGBA pixels, or of an OffscreenCanvas the renderer drew,
   * which goes to the page as only its pixels. */
  function image(properties) {
    const { canvas, ...rest } = properties ?? {};
    if (canvas === undefined) {
      return { ...rest, type: "image" };
    }
    if (!(canvas instanceof OffscreenCanvas)) {
      throw new TypeError("uscope.image's canvas must be an OffscreenCanvas");
    }
    return { ...rest, type: "image", bitmap: canvas.transferToImageBitmap() };
  }

  const uscope = freeze({
    draw(render) {
      if (typeof render !== "function") {
        throw new TypeError("uscope.draw takes a function");
      }
      if (drawer !== null) {
        throw new Error("a renderer calls uscope.draw once");
      }
      drawer = render;
    },
    picture: shape("picture"),
    rect: shape("rect"),
    circle: shape("circle"),
    line: shape("line"),
    polyline: shape("polyline"),
    polygon: shape("polygon"),
    path: shape("path"),
    text: shape("text"),
    group: shape("group"),
    image,
    get theme() {
      return theme;
    },
    same,
    color: freeze({ scale }),
  });

  let file = "renderer.js";

  function load(source, name) {
    file = `${name}.js`;
    try {
      evaluate(`${source}\n//# sourceURL=${file}`);
    } catch (error) {
      return failure(error, file);
    }
    if (drawer === null) {
      return { message: "the renderer never calls uscope.draw", file, line: null, column: null };
    }
    return null;
  }

  async function draw(id, input, context) {
    let picture;
    try {
      theme = freeze({ ...context.palette, series: freeze([...context.palette.series]) });
      const { palette: _, ...given } = context;
      picture = await drawer(input, freeze(given));
    } catch (error) {
      post({ type: "error", id, error: failure(error, file) });
      return;
    }
    try {
      post({ type: "picture", id, picture });
    } catch (error) {
      post({
        type: "error",
        id,
        error: {
          message: `the picture cannot be sent: ${error.message}`,
          file,
          line: null,
          column: null,
        },
      });
    }
  }

  // An error the renderer leaves uncaught, as in a timer, fails its draw.
  listen("error", (event) => {
    event.preventDefault();
    post({ type: "error", id: null, error: failure(event.error ?? event.message, file) });
  });
  listen("unhandledrejection", (event) => {
    event.preventDefault();
    post({ type: "error", id: null, error: failure(event.reason, file) });
  });

  let loaded = false;
  listen("message", (event) => {
    const message = event.data;
    if (message?.type === "load" && !loaded) {
      loaded = true;
      post({ type: "loaded", error: load(message.source, message.name) });
    } else if (message?.type === "draw" && loaded && drawer !== null) {
      draw(message.id, message.input, message.context);
    }
  });

  defineProperty(globalThis, "uscope", { value: uscope, enumerable: true });
  try {
    lockDown();
  } catch (error) {
    // Nothing runs in a sandbox that could not be closed.
    post({ type: "fatal", error: failure(error, "sandbox") });
    loaded = true;
  }
})();
