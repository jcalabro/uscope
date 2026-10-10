// The sandboxed frame's script: the page served at /visualizer-frame runs
// it with WORKER_SOURCE, the worker's own script, defined before it. The
// frame has an opaque origin and a policy that allows no request at all;
// it keeps one worker per card and passes messages between them and the
// page, over the one port the page hands it. It never runs a renderer.

/* global WORKER_SOURCE */

(() => {
  const url = URL.createObjectURL(new Blob([WORKER_SOURCE], { type: "text/javascript" }));
  /** Each card's worker, and the digest of the renderer it loaded. */
  const cards = new Map();
  let port = null;

  function release(card) {
    const held = cards.get(card);
    if (held !== undefined) {
      held.worker.terminate();
      cards.delete(card);
    }
  }

  function start(card, renderer) {
    const worker = new Worker(url);
    const held = { worker, digest: renderer.digest };
    worker.addEventListener("message", (event) => {
      if (cards.get(card) === held) {
        port.postMessage({ ...event.data, card });
      }
    });
    worker.addEventListener("error", (event) => {
      event.preventDefault();
      if (cards.get(card) === held) {
        port.postMessage({
          type: "error",
          card,
          id: null,
          error: {
            message: event.message || "the worker failed",
            file: null,
            line: null,
            column: null,
          },
        });
      }
    });
    worker.postMessage({ type: "load", source: renderer.source, name: renderer.name });
    cards.set(card, held);
    return held;
  }

  function receive(event) {
    const message = event.data;
    if (message?.type === "draw") {
      let held = cards.get(message.card);
      if (held !== undefined && held.digest !== message.renderer.digest) {
        release(message.card);
        held = undefined;
      }
      held ??= start(message.card, message.renderer);
      held.worker.postMessage({
        type: "draw",
        id: message.id,
        input: message.input,
        context: message.context,
      });
    } else if (message?.type === "release") {
      release(message.card);
    }
  }

  // The page's first message hands over the port; nothing else is heard.
  window.addEventListener("message", function hello(event) {
    if (
      event.source !== window.parent ||
      port !== null ||
      !(event.ports[0] instanceof MessagePort)
    ) {
      return;
    }
    window.removeEventListener("message", hello);
    port = event.ports[0];
    port.addEventListener("message", receive);
    port.start();
    port.postMessage({ type: "ready" });
  });
})();
