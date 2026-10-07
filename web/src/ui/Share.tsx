import { useLocation } from "@tanstack/react-router";
import { useEffect, useRef, useState } from "react";
import type { Role } from "../protocol";
import { useConnection, useModel } from "../store";

export function Share() {
  const role = useModel((model) => model.hello?.role);
  const open = useModel((model) => model.link === "open");
  const connection = useConnection();
  const location = useLocation();
  const [shown, setShown] = useState(false);
  const [access, setAccess] = useState<Role>("view");
  const [url, setUrl] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const field = useRef<HTMLInputElement>(null);
  const anchor = useRef<HTMLDivElement>(null);
  const to = location.pathname;
  const port = window.location.port || "80";

  useEffect(() => {
    if (!shown) {
      return;
    }
    setCopied(false);
    setError(null);
    connection
      .request("share", { role: access, to })
      .then((link) => setUrl(link.url))
      .catch((failure: Error) => setError(failure.message));
  }, [shown, access, to, connection]);

  // Escape or a click elsewhere puts it away.
  useEffect(() => {
    if (!shown) {
      return;
    }
    const key = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        setShown(false);
      }
    };
    const pointer = (event: PointerEvent) => {
      if (!anchor.current?.contains(event.target as Node)) {
        setShown(false);
      }
    };
    window.addEventListener("keydown", key);
    window.addEventListener("pointerdown", pointer);
    return () => {
      window.removeEventListener("keydown", key);
      window.removeEventListener("pointerdown", pointer);
    };
  }, [shown]);

  if (!open) {
    return null;
  }

  const copy = async () => {
    if (!url) {
      return;
    }
    try {
      await navigator.clipboard.writeText(url);
      setCopied(true);
    } catch {
      field.current?.select();
    }
  };

  return (
    <div className="anchor" ref={anchor}>
      <button
        type="button"
        className="tb ghost"
        aria-expanded={shown}
        onClick={() => setShown(!shown)}
      >
        Share
      </button>
      {shown && (
        <div className="popover" role="dialog" aria-label="Share this session">
          <div className="card-head">
            <h1>Share this session</h1>
          </div>
          <div className="card-body">
            <div className="segmented" style={{ marginLeft: 0 }}>
              <button
                type="button"
                aria-pressed={access === "view"}
                onClick={() => setAccess("view")}
              >
                Can view
              </button>
              <button
                type="button"
                aria-pressed={access === "control"}
                disabled={role !== "control"}
                onClick={() => setAccess("control")}
              >
                Can control
              </button>
            </div>
            <span className="muted">
              {access === "view"
                ? "They see every pane and move their own focus."
                : "They can also run, stop, and choose what to debug."}
            </span>
            <div className="link-field">
              <input ref={field} readOnly value={url ?? ""} aria-label="Join link" />
              <button type="button" onClick={copy} disabled={!url}>
                {copied ? "Copied" : "Copy"}
              </button>
            </div>
            {error && <span className="muted">{error}</span>}
            <span className="muted">
              Works until uscope exits. Someone on another machine needs a tunnel, such as{" "}
              <code>
                ssh -L {port}:127.0.0.1:{port} host
              </code>
              .
            </span>
          </div>
        </div>
      )}
    </div>
  );
}
