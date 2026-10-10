// The page's side of the sandbox: one hidden, sandboxed frame per tab,
// served by uscope at /visualizer-frame, which keeps a worker per card.
// The page talks to it over a port it hands the frame, never through the
// window, runs at most four draws at once, and ends a draw that takes
// longer than two seconds by ending its worker.

/** A renderer: its name, its JavaScript, and the digest that names both. */
export interface RendererCode {
  name: string;
  digest: string;
  source: string;
}

/** Where a renderer failed: its file and line when the browser says. */
export interface RendererFailure {
  message: string;
  file: string | null;
  line: number | null;
  column: number | null;
}

/** The colors `uscope.theme` holds, in the page's current theme. */
export interface Palette {
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
  series: string[];
}

/** What a renderer's function gets beside its inputs. */
export interface DrawContext {
  previous: unknown;
  width: number;
  theme: "light" | "dark";
  palette: Palette;
}

export type Outcome =
  | { kind: "picture"; picture: unknown }
  | { kind: "failed"; failure: RendererFailure }
  | { kind: "timeout" };

/** How long a draw may take before its worker is ended. */
export const DRAW_TIMEOUT = 2_000;
/** How many draws run at once. */
export const PARALLEL = 4;

interface Pending {
  card: string;
  resolve: (outcome: Outcome) => void;
  timer: ReturnType<typeof setTimeout> | null;
}

type FrameMessage =
  | { type: "ready" }
  | { type: "loaded"; card: string; error: RendererFailure | null }
  | { type: "picture"; card: string; id: number; picture: unknown }
  | { type: "error"; card: string; id: number | null; error: RendererFailure }
  | { type: "fatal"; card: string; error: RendererFailure };

export class Sandbox {
  private readonly frame: HTMLIFrameElement;
  /** The frame's port once it is ready, or null once the page closes it. */
  private readonly port: Promise<MessagePort | null>;
  private closePort: () => void = () => {};
  private readonly pending = new Map<number, Pending>();
  /** Draws waiting for a turn, told whether they may go ahead. */
  private readonly waiting: ((go: boolean) => void)[] = [];
  private running = 0;
  private disposed = false;
  private next = 1;

  /** Starts the frame at `url`, the server's /visualizer-frame. */
  constructor(url: string) {
    this.frame = document.createElement("iframe");
    this.frame.setAttribute("sandbox", "allow-scripts");
    this.frame.hidden = true;
    this.frame.title = "renderers";
    this.port = new Promise((resolve) => {
      this.closePort = () => resolve(null);
      this.frame.addEventListener(
        "load",
        () => {
          const channel = new MessageChannel();
          channel.port1.addEventListener("message", (event) => {
            const message = event.data as FrameMessage;
            if (message.type === "ready") {
              resolve(channel.port1);
            } else {
              this.receive(message);
            }
          });
          channel.port1.start();
          // The frame's origin is opaque, so it has no name to post to.
          this.frame.contentWindow?.postMessage({ type: "hello" }, "*", [channel.port2]);
        },
        { once: true },
      );
    });
    this.frame.src = url;
    document.body.append(this.frame);
  }

  /** Draws `input` with `renderer` in the worker of `card`, starting one
   * when the card has none or had another renderer. */
  async draw(
    card: string,
    renderer: RendererCode,
    input: unknown,
    context: DrawContext,
  ): Promise<Outcome> {
    const closed: Outcome = {
      kind: "failed",
      failure: { message: "the page closed the sandbox", file: null, line: null, column: null },
    };
    if (!(await this.turn())) {
      return closed;
    }
    const port = await this.port;
    if (port === null || this.disposed) {
      this.leave();
      return closed;
    }
    const id = this.next++;
    return new Promise<Outcome>((resolve) => {
      const pending: Pending = { card, resolve, timer: null };
      pending.timer = setTimeout(() => {
        this.finish(id, { kind: "timeout" });
        this.release(card);
      }, DRAW_TIMEOUT);
      this.pending.set(id, pending);
      port.postMessage({ type: "draw", card, id, renderer, input, context });
    });
  }

  /** Ends the card's worker, and with it any draw it has not answered. */
  release(card: string): void {
    this.fail(card, {
      message: "the drawing was cancelled",
      file: null,
      line: null,
      column: null,
    });
    void this.port.then((port) => port?.postMessage({ type: "release", card }));
  }

  /** Removes the frame, ending every worker. */
  dispose(): void {
    this.disposed = true;
    this.closePort();
    for (const waiting of this.waiting.splice(0)) {
      waiting(false);
    }
    for (const id of [...this.pending.keys()]) {
      this.finish(id, {
        kind: "failed",
        failure: { message: "the page closed the sandbox", file: null, line: null, column: null },
      });
    }
    this.frame.remove();
  }

  private receive(message: FrameMessage): void {
    switch (message.type) {
      case "picture":
        this.finish(message.id, { kind: "picture", picture: message.picture });
        break;
      case "error":
        if (message.id !== null) {
          this.finish(message.id, { kind: "failed", failure: message.error });
        } else {
          this.fail(message.card, message.error);
        }
        break;
      case "loaded":
        if (message.error !== null) {
          this.fail(message.card, message.error);
          this.release(message.card);
        }
        break;
      case "fatal":
        this.fail(message.card, message.error);
        this.release(message.card);
        break;
      case "ready":
        break;
    }
  }

  /** Fails every draw of `card`, as when its renderer did not load. */
  private fail(card: string, failure: RendererFailure): void {
    for (const [id, pending] of this.pending) {
      if (pending.card === card) {
        this.finish(id, { kind: "failed", failure });
      }
    }
  }

  private finish(id: number, outcome: Outcome): void {
    const pending = this.pending.get(id);
    if (pending === undefined) {
      return;
    }
    this.pending.delete(id);
    if (pending.timer !== null) {
      clearTimeout(pending.timer);
    }
    this.leave();
    pending.resolve(outcome);
  }

  /** Ends a draw's turn, giving it to the next that waits. */
  private leave(): void {
    this.running -= 1;
    this.waiting.shift()?.(true);
  }

  /** Waits until fewer than PARALLEL draws run, and says whether this
   * one may run: not once the sandbox is closed. */
  private turn(): Promise<boolean> {
    if (this.disposed) {
      return Promise.resolve(false);
    }
    if (this.running < PARALLEL) {
      this.running += 1;
      return Promise.resolve(true);
    }
    return new Promise((resolve) =>
      this.waiting.push((go) => {
        if (go) {
          this.running += 1;
        }
        resolve(go);
      }),
    );
  }
}

// The eight categorical hues, in the order validated for each theme.
const SERIES = {
  light: ["#2a78d6", "#eb6834", "#1baf7a", "#eda100", "#e87ba4", "#008300", "#6250d6", "#e34948"],
  dark: ["#3987e5", "#d95926", "#199e70", "#c98500", "#d55181", "#008300", "#9085e9", "#e66767"],
};

/** The theme the page shows now. */
export function currentTheme(): "light" | "dark" {
  const chosen = document.documentElement.getAttribute("data-theme");
  if (chosen === "light" || chosen === "dark") {
    return chosen;
  }
  return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

/** The page's own colors, as its stylesheet gives them now. */
export function palette(theme: "light" | "dark"): Palette {
  const style = getComputedStyle(document.documentElement);
  const token = (name: string) => style.getPropertyValue(`--${name}`).trim();
  return {
    ink: token("ink"),
    ink2: token("ink-2"),
    ink3: token("ink-3"),
    paper: token("paper"),
    surface: token("surface"),
    line: token("line"),
    accent: token("accent"),
    changed: token("changed"),
    good: token("run"),
    bad: token("error"),
    series: SERIES[theme],
  };
}
